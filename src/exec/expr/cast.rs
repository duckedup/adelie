//! CAST validity table between `DataType`s (SPEC §7 syntax). Filled by U2 (adelie-1st).

use crate::exec::{Column, ColumnBuilder, ExecError};
use crate::types::{DataType, Decimal, DecimalType, Ip, Value, pow10};

/// Any type to itself is identity. Beyond that: the numeric kinds cross-cast, BOOL <-> INT64/
/// UINT64, any non-LIST <-> STRING, TIMESTAMP <-> DATE, UUID/IP -> BYTES. Nothing else, and
/// never LIST (SPEC's cast table).
pub(crate) fn castable(from: &DataType, to: &DataType) -> bool {
    if from == to {
        return true;
    }
    if matches!(from, DataType::List(_)) || matches!(to, DataType::List(_)) {
        return false;
    }
    match (from, to) {
        (a, b) if is_numeric(a) && is_numeric(b) => true,
        (DataType::Bool, DataType::Int64 | DataType::UInt64) => true,
        (DataType::Int64 | DataType::UInt64, DataType::Bool) => true,
        (DataType::Timestamp, DataType::Date) | (DataType::Date, DataType::Timestamp) => true,
        (DataType::Uuid, DataType::Bytes) | (DataType::Ip, DataType::Bytes) => true,
        (_, DataType::String) | (DataType::String, _) => true,
        _ => false,
    }
}

fn is_numeric(ty: &DataType) -> bool {
    matches!(
        ty,
        DataType::Int64 | DataType::UInt64 | DataType::Float64 | DataType::Decimal(_)
    )
}

const NS_PER_DAY: i64 = 86_400_000_000_000;

fn cast_err(v: &Value, to: &DataType) -> ExecError {
    ExecError::Cast {
        value: v
            .to_text()
            .expect("non-null, non-LIST value always has text"),
        to: to.clone(),
    }
}

fn bool_val(v: &Value) -> bool {
    let Value::Bool(b) = v else {
        unreachable!("checked by castable")
    };
    *b
}
fn int_val(v: &Value) -> i64 {
    let Value::Int64(n) = v else {
        unreachable!("checked by castable")
    };
    *n
}
fn uint_val(v: &Value) -> u64 {
    let Value::UInt64(n) = v else {
        unreachable!("checked by castable")
    };
    *n
}
fn ts_val(v: &Value) -> i64 {
    let Value::Timestamp(n) = v else {
        unreachable!("checked by castable")
    };
    *n
}
fn date_val(v: &Value) -> i32 {
    let Value::Date(n) = v else {
        unreachable!("checked by castable")
    };
    *n
}
fn uuid_val(v: &Value) -> [u8; 16] {
    let Value::Uuid(b) = v else {
        unreachable!("checked by castable")
    };
    *b
}
fn ip_val(v: &Value) -> Ip {
    let Value::Ip(ip) = v else {
        unreachable!("checked by castable")
    };
    *ip
}

fn timestamp_to_date(ns: i64) -> Option<i32> {
    i32::try_from(ns.div_euclid(NS_PER_DAY)).ok()
}

fn date_to_timestamp_ns(days: i32) -> Option<i64> {
    (days as i64).checked_mul(NS_PER_DAY)
}

/// Rounds `unscaled` divided by (a power-of-ten) `divisor` half away from zero.
fn round_half_away(unscaled: i128, divisor: i128) -> i128 {
    let q = unscaled / divisor;
    let r = unscaled % divisor;
    if r.unsigned_abs() * 2 >= divisor.unsigned_abs() {
        q + r.signum()
    } else {
        q
    }
}

fn int_to_decimal(n: i128, dt: DecimalType) -> Option<Value> {
    let unscaled = n.checked_mul(pow10(dt.scale()))?;
    if unscaled.unsigned_abs() >= pow10(dt.precision()) as u128 {
        return None;
    }
    Decimal::new(unscaled, dt.scale()).ok().map(Value::Decimal)
}

const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
const TWO_POW_64: f64 = 18_446_744_073_709_551_616.0;

/// `f64::round` already rounds half away from zero, matching CAST's rule for FLOAT64/DECIMAL
/// to an integer.
fn f64_to_i64_rounded(f: f64) -> Option<i64> {
    if !f.is_finite() {
        return None;
    }
    let r = f.round();
    (-TWO_POW_63..TWO_POW_63).contains(&r).then_some(r as i64)
}

fn f64_to_u64_rounded(f: f64) -> Option<u64> {
    if !f.is_finite() {
        return None;
    }
    let r = f.round();
    (0.0..TWO_POW_64).contains(&r).then_some(r as u64)
}

fn float_to_decimal(f: f64, dt: DecimalType) -> Option<Value> {
    if !f.is_finite() {
        return None;
    }
    let rounded = (f * pow10(dt.scale()) as f64).round();
    let limit = pow10(dt.precision()) as f64;
    if !(-limit..limit).contains(&rounded) {
        return None;
    }
    let unscaled = rounded as i128;
    Decimal::new(unscaled, dt.scale()).ok().map(Value::Decimal)
}

fn decimal_to_int64_rounded(d: Decimal) -> Option<i64> {
    let rounded = if d.scale() == 0 {
        d.unscaled()
    } else {
        round_half_away(d.unscaled(), pow10(d.scale()))
    };
    i64::try_from(rounded).ok()
}

fn decimal_to_uint64_rounded(d: Decimal) -> Option<u64> {
    let rounded = if d.scale() == 0 {
        d.unscaled()
    } else {
        round_half_away(d.unscaled(), pow10(d.scale()))
    };
    u64::try_from(rounded).ok()
}

fn decimal_to_decimal(d: Decimal, to: DecimalType) -> Option<Value> {
    let (from_scale, to_scale) = (d.scale(), to.scale());
    let unscaled = match to_scale.cmp(&from_scale) {
        std::cmp::Ordering::Equal => d.unscaled(),
        std::cmp::Ordering::Greater => d.unscaled().checked_mul(pow10(to_scale - from_scale))?,
        std::cmp::Ordering::Less => round_half_away(d.unscaled(), pow10(from_scale - to_scale)),
    };
    if unscaled.unsigned_abs() >= pow10(to.precision()) as u128 {
        return None;
    }
    Decimal::new(unscaled, to_scale).ok().map(Value::Decimal)
}

fn cast_numeric(v: &Value, to: &DataType) -> Result<Value, ExecError> {
    let result = match (v, to) {
        (Value::Int64(n), DataType::UInt64) => u64::try_from(*n).ok().map(Value::UInt64),
        (Value::Int64(n), DataType::Float64) => Some(Value::Float64(*n as f64)),
        (Value::Int64(n), DataType::Decimal(dt)) => int_to_decimal(*n as i128, *dt),
        (Value::UInt64(n), DataType::Int64) => i64::try_from(*n).ok().map(Value::Int64),
        (Value::UInt64(n), DataType::Float64) => Some(Value::Float64(*n as f64)),
        (Value::UInt64(n), DataType::Decimal(dt)) => int_to_decimal(*n as i128, *dt),
        (Value::Float64(f), DataType::Int64) => f64_to_i64_rounded(*f).map(Value::Int64),
        (Value::Float64(f), DataType::UInt64) => f64_to_u64_rounded(*f).map(Value::UInt64),
        (Value::Float64(f), DataType::Decimal(dt)) => float_to_decimal(*f, *dt),
        (Value::Decimal(d), DataType::Int64) => decimal_to_int64_rounded(*d).map(Value::Int64),
        (Value::Decimal(d), DataType::UInt64) => decimal_to_uint64_rounded(*d).map(Value::UInt64),
        (Value::Decimal(d), DataType::Float64) => Some(Value::Float64(
            d.unscaled() as f64 / pow10(d.scale()) as f64,
        )),
        (Value::Decimal(d), DataType::Decimal(dt)) => decimal_to_decimal(*d, *dt),
        _ => unreachable!("cast_numeric called with a non-numeric pair"),
    };
    result.ok_or_else(|| cast_err(v, to))
}

fn cast_value(v: &Value, from: &DataType, to: &DataType) -> Result<Value, ExecError> {
    if matches!(to, DataType::String) {
        return Ok(Value::String(
            v.to_text()
                .expect("non-null, non-LIST value always has text"),
        ));
    }
    if matches!(from, DataType::String) {
        let Value::String(s) = v else {
            unreachable!("from is STRING")
        };
        return Value::from_text(s, to).ok_or_else(|| ExecError::Cast {
            value: s.clone(),
            to: to.clone(),
        });
    }
    if is_numeric(from) && is_numeric(to) {
        return cast_numeric(v, to);
    }
    match (from, to) {
        (DataType::Bool, DataType::Int64) => Ok(Value::Int64(bool_val(v) as i64)),
        (DataType::Bool, DataType::UInt64) => Ok(Value::UInt64(bool_val(v) as u64)),
        (DataType::Int64, DataType::Bool) => Ok(Value::Bool(int_val(v) != 0)),
        (DataType::UInt64, DataType::Bool) => Ok(Value::Bool(uint_val(v) != 0)),
        (DataType::Timestamp, DataType::Date) => timestamp_to_date(ts_val(v))
            .map(Value::Date)
            .ok_or_else(|| cast_err(v, to)),
        (DataType::Date, DataType::Timestamp) => date_to_timestamp_ns(date_val(v))
            .map(Value::Timestamp)
            .ok_or_else(|| cast_err(v, to)),
        (DataType::Uuid, DataType::Bytes) => Ok(Value::Bytes(uuid_val(v).to_vec())),
        (DataType::Ip, DataType::Bytes) => Ok(Value::Bytes(ip_val(v).octets().to_vec())),
        _ => unreachable!("castable() only allows the pairs handled here"),
    }
}

/// Row by row via `ColumnBuilder` (SPEC's Postgres semantics: any row's failure fails the
/// whole call). A NULL input row stays NULL.
pub(crate) fn cast(col: &Column, to: &DataType) -> Result<Column, ExecError> {
    let from = col.data_type().clone();
    if &from == to {
        return Ok(col.clone());
    }
    if !castable(&from, to) {
        return Err(ExecError::Plan(format!("cannot cast {from} to {to}")));
    }
    let mut builder = ColumnBuilder::with_capacity(to.clone(), col.len());
    for i in 0..col.len() {
        if col.is_null(i) {
            builder.push_null();
            continue;
        }
        let casted = cast_value(&col.get(i), &from, to)?;
        builder
            .push(&casted)
            .expect("cast_value always produces a value that fits `to`");
    }
    Ok(builder.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(ty: &DataType, v: Value) -> Column {
        Column::from_values(ty, &[v]).unwrap()
    }

    fn dt(p: u8, s: u8) -> DataType {
        DataType::decimal(p, s).unwrap()
    }

    #[test]
    fn string_to_int64() {
        let col = one(&DataType::String, Value::String("42".into()));
        let out = cast(&col, &DataType::Int64).unwrap();
        assert_eq!(out.get(0), Value::Int64(42));
    }

    #[test]
    fn int_to_float() {
        let col = one(&DataType::Int64, Value::Int64(5));
        let out = cast(&col, &DataType::Float64).unwrap();
        assert_eq!(out.get(0), Value::Float64(5.0));
    }

    #[test]
    fn bad_string_to_int64_is_cast_error() {
        let col = one(&DataType::String, Value::String("x".into()));
        let err = cast(&col, &DataType::Int64).unwrap_err();
        assert!(matches!(err, ExecError::Cast { .. }));
    }

    #[test]
    fn float_to_int_rounds_half_away_from_zero() {
        let col = one(&DataType::Float64, Value::Float64(2.5));
        assert_eq!(
            cast(&col, &DataType::Int64).unwrap().get(0),
            Value::Int64(3)
        );
        let col = one(&DataType::Float64, Value::Float64(-2.5));
        assert_eq!(
            cast(&col, &DataType::Int64).unwrap().get(0),
            Value::Int64(-3)
        );
    }

    #[test]
    fn float_out_of_range_is_cast_error() {
        let col = one(&DataType::Float64, Value::Float64(1e20));
        let err = cast(&col, &DataType::Int64).unwrap_err();
        assert!(matches!(err, ExecError::Cast { .. }));
    }

    #[test]
    fn decimal_narrowing_rounds_half_away() {
        let col = one(&dt(5, 2), Value::Decimal(Decimal::new(12345, 2).unwrap()));
        let out = cast(&col, &dt(4, 1)).unwrap();
        assert_eq!(out.get(0), Value::Decimal(Decimal::new(1235, 1).unwrap()));
    }

    #[test]
    fn timestamp_negative_one_ns_is_date_negative_one() {
        let col = one(&DataType::Timestamp, Value::Timestamp(-1));
        let out = cast(&col, &DataType::Date).unwrap();
        assert_eq!(out.get(0), Value::Date(-1));
    }

    #[test]
    fn castable_rejects_list_to_string() {
        let list_ty = DataType::list(DataType::Int64).unwrap();
        assert!(!castable(&list_ty, &DataType::String));
    }

    #[test]
    fn null_row_stays_null() {
        let col = one(&DataType::String, Value::Null);
        let out = cast(&col, &DataType::Int64).unwrap();
        assert!(out.is_null(0));
    }
}
