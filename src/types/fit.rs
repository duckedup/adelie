//! Fit / coercion on write (contract rule 4): lossless only, and it never parses a string.

use super::datatype::{DataType, DecimalType};
use super::value::{Decimal, Value, pow10};

pub const COMPANION_SUFFIX: &str = "::string";

/// `<column>::string`, where the companion STRING column for a lossy write lives.
pub fn companion_name(column: &str) -> String {
    format!("{column}{COMPANION_SUFFIX}")
}

/// `None` unless the value fits `ty` exactly and losslessly (contract rule 4). NULL always
/// fits; a LIST fits only when every element does.
pub fn coerce(value: &Value, ty: &DataType) -> Option<Value> {
    if matches!(value, Value::Null) {
        return Some(Value::Null);
    }
    match (value, ty) {
        (Value::Bool(_), DataType::Bool)
        | (Value::Int64(_), DataType::Int64)
        | (Value::UInt64(_), DataType::UInt64)
        | (Value::Float64(_), DataType::Float64)
        | (Value::String(_), DataType::String)
        | (Value::Bytes(_), DataType::Bytes)
        | (Value::Timestamp(_), DataType::Timestamp)
        | (Value::Date(_), DataType::Date)
        | (Value::Uuid(_), DataType::Uuid)
        | (Value::Ip(_), DataType::Ip) => Some(value.clone()),

        (Value::Decimal(d), DataType::Decimal(dt)) => rescale(*d, *dt).map(Value::Decimal),
        (Value::List(items), DataType::List(lt)) => items
            .iter()
            .map(|item| coerce(item, lt.element()))
            .collect::<Option<Vec<_>>>()
            .map(Value::List),

        (Value::Int64(n), DataType::UInt64) => {
            if *n >= 0 {
                Some(Value::UInt64(*n as u64))
            } else {
                None
            }
        }
        (Value::UInt64(n), DataType::Int64) => {
            if *n <= i64::MAX as u64 {
                Some(Value::Int64(*n as i64))
            } else {
                None
            }
        }
        (Value::Int64(n), DataType::Float64) => {
            if n.unsigned_abs() <= (1u64 << 53) {
                Some(Value::Float64(*n as f64))
            } else {
                None
            }
        }
        (Value::UInt64(n), DataType::Float64) => {
            if *n <= (1u64 << 53) {
                Some(Value::Float64(*n as f64))
            } else {
                None
            }
        }
        (Value::Int64(n), DataType::Decimal(dt)) => int_to_decimal(*n as i128, *dt),
        (Value::UInt64(n), DataType::Decimal(dt)) => int_to_decimal(*n as i128, *dt),

        (Value::Float64(f), DataType::Int64) => float_to_int(*f),
        (Value::Float64(f), DataType::UInt64) => float_to_uint(*f),
        (Value::Decimal(d), DataType::Int64) => decimal_to_int(*d),
        (Value::Decimal(d), DataType::UInt64) => decimal_to_uint(*d),

        (Value::Date(days), DataType::Timestamp) => date_to_timestamp(*days),

        _ => None,
    }
}

pub fn fits(value: &Value, ty: &DataType) -> bool {
    coerce(value, ty).is_some()
}

/// Rescale to the column's scale: exact only, and within its precision. Going up multiplies;
/// going down is allowed only when the dropped digits are all zero.
fn rescale(d: Decimal, dt: DecimalType) -> Option<Decimal> {
    let target = dt.scale();
    let unscaled = if target >= d.scale() {
        d.unscaled().checked_mul(pow10(target - d.scale()))?
    } else {
        let drop = pow10(d.scale() - target);
        if d.unscaled().rem_euclid(drop) != 0 {
            return None;
        }
        d.unscaled().div_euclid(drop)
    };
    if unscaled.unsigned_abs() >= pow10(dt.precision()) as u128 {
        return None;
    }
    Decimal::new(unscaled, target).ok()
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

fn float_to_int(f: f64) -> Option<Value> {
    if !f.is_finite() || f.fract() != 0.0 {
        return None;
    }
    // `i64::MAX as f64` rounds up to 2^63, so the upper bound must be exclusive.
    if !(-TWO_POW_63..TWO_POW_63).contains(&f) {
        return None;
    }
    Some(Value::Int64(f as i64))
}

fn float_to_uint(f: f64) -> Option<Value> {
    if !f.is_finite() || f.fract() != 0.0 {
        return None;
    }
    if !(0.0..TWO_POW_64).contains(&f) {
        return None;
    }
    Some(Value::UInt64(f as u64))
}

fn decimal_to_int(d: Decimal) -> Option<Value> {
    let p = pow10(d.scale());
    if d.unscaled().rem_euclid(p) != 0 {
        return None;
    }
    let n = d.unscaled().div_euclid(p);
    if !(i64::MIN as i128..=i64::MAX as i128).contains(&n) {
        return None;
    }
    Some(Value::Int64(n as i64))
}

fn decimal_to_uint(d: Decimal) -> Option<Value> {
    let p = pow10(d.scale());
    if d.unscaled().rem_euclid(p) != 0 {
        return None;
    }
    let n = d.unscaled().div_euclid(p);
    if !(0..=u64::MAX as i128).contains(&n) {
        return None;
    }
    Some(Value::UInt64(n as u64))
}

fn date_to_timestamp(days: i32) -> Option<Value> {
    let ns = (days as i128) * 86_400_000_000_000i128;
    if !(i64::MIN as i128..=i64::MAX as i128).contains(&ns) {
        return None;
    }
    Some(Value::Timestamp(ns as i64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(unscaled: i128, scale: u8) -> Value {
        Value::Decimal(Decimal::new(unscaled, scale).unwrap())
    }

    fn dt(precision: u8, scale: u8) -> DataType {
        DataType::decimal(precision, scale).unwrap()
    }

    #[test]
    fn null_fits_every_type() {
        assert_eq!(coerce(&Value::Null, &DataType::Int64), Some(Value::Null));
        assert_eq!(coerce(&Value::Null, &dt(5, 1)), Some(Value::Null));
    }

    #[test]
    fn int64_to_uint64_rejects_negative_accepts_zero() {
        assert_eq!(coerce(&Value::Int64(-1), &DataType::UInt64), None);
        assert_eq!(
            coerce(&Value::Int64(0), &DataType::UInt64),
            Some(Value::UInt64(0))
        );
    }

    #[test]
    fn uint64_to_int64_boundary_is_i64_max() {
        let over = i64::MAX as u64 + 1;
        assert_eq!(coerce(&Value::UInt64(over), &DataType::Int64), None);
        assert_eq!(
            coerce(&Value::UInt64(i64::MAX as u64), &DataType::Int64),
            Some(Value::Int64(i64::MAX))
        );
    }

    #[test]
    fn int_to_float_boundary_is_two_pow_53() {
        let n: i64 = 1 << 53;
        assert!(coerce(&Value::Int64(n), &DataType::Float64).is_some());
        assert_eq!(coerce(&Value::Int64(n + 1), &DataType::Float64), None);
    }

    #[test]
    fn float_to_int_rejects_the_rounded_up_top_of_range() {
        // i64::MAX as f64 == 2^63 and u64::MAX as f64 == 2^64: neither fits.
        assert_eq!(coerce(&Value::Float64(TWO_POW_63), &DataType::Int64), None);
        assert_eq!(
            coerce(&Value::Float64(-TWO_POW_63), &DataType::Int64),
            Some(Value::Int64(i64::MIN))
        );
        assert_eq!(coerce(&Value::Float64(TWO_POW_64), &DataType::UInt64), None);
        let below = TWO_POW_64 - 2048.0; // the largest f64 under 2^64
        assert_eq!(
            coerce(&Value::Float64(below), &DataType::UInt64),
            Some(Value::UInt64(18_446_744_073_709_549_568))
        );
    }

    #[test]
    fn float_to_int_requires_integral_finite_value() {
        assert_eq!(
            coerce(&Value::Float64(3.0), &DataType::Int64),
            Some(Value::Int64(3))
        );
        assert_eq!(coerce(&Value::Float64(3.5), &DataType::Int64), None);
        assert_eq!(coerce(&Value::Float64(f64::NAN), &DataType::Int64), None);
        assert_eq!(
            coerce(&Value::Float64(f64::INFINITY), &DataType::Int64),
            None
        );
    }

    #[test]
    fn int_to_decimal_checks_precision_after_scaling() {
        assert_eq!(coerce(&Value::Int64(12345), &dt(5, 1)), None);
        assert_eq!(
            coerce(&Value::Int64(12345), &dt(6, 1)),
            Some(dec(123450, 1))
        );
    }

    #[test]
    fn decimal_rescale_drops_zero_digits_only() {
        assert_eq!(coerce(&dec(150, 2), &dt(3, 1)), Some(dec(15, 1)));
        assert_eq!(coerce(&dec(155, 2), &dt(3, 1)), None);
    }

    #[test]
    fn date_to_timestamp_is_midnight_utc() {
        assert_eq!(
            coerce(&Value::Date(1), &DataType::Timestamp),
            Some(Value::Timestamp(86_400 * 1_000_000_000))
        );
    }

    #[test]
    fn strings_never_parse() {
        assert_eq!(
            coerce(&Value::String("1".to_string()), &DataType::Int64),
            None
        );
    }

    #[test]
    fn list_fits_only_when_every_element_does() {
        let list_ty = DataType::list(DataType::UInt64).unwrap();
        assert_eq!(
            coerce(&Value::List(vec![Value::Int64(1)]), &list_ty),
            Some(Value::List(vec![Value::UInt64(1)]))
        );
        assert_eq!(coerce(&Value::List(vec![Value::Int64(-1)]), &list_ty), None);
    }

    #[test]
    fn companion_name_appends_suffix() {
        assert_eq!(companion_name("status"), "status::string");
    }
}
