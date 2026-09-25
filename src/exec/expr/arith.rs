//! Arithmetic kernels for `Expr::Arith`/`Expr::Neg` (SPEC §7 syntax). Filled by U2 (adelie-1st).

use crate::exec::{Bitmap, Column, ColumnValues, ExecError, OwnedValues};
use crate::types::{DataType, DecimalType, pow10};

use super::ArithOp;

/// Lazily allocates validity only once the first invalid row appears (mirrors
/// `ColumnBuilder::push_null`'s and the selection kernels' own backfill).
struct ValidityBuilder {
    bitmap: Option<Bitmap>,
    len: usize,
}

impl ValidityBuilder {
    fn new() -> Self {
        ValidityBuilder {
            bitmap: None,
            len: 0,
        }
    }

    fn push(&mut self, valid: bool) {
        if let Some(bm) = &mut self.bitmap {
            bm.push(valid);
        } else if !valid {
            let mut bm = Bitmap::new_valid(self.len);
            bm.push(false);
            self.bitmap = Some(bm);
        }
        self.len += 1;
    }

    fn finish(self) -> Option<Bitmap> {
        self.bitmap
    }
}

fn op_symbol(op: ArithOp) -> &'static str {
    match op {
        ArithOp::Add => "+",
        ArithOp::Sub => "-",
        ArithOp::Mul => "*",
        ArithOp::Div => "/",
        ArithOp::Mod => "%",
    }
}

fn int64_slice(c: &Column) -> &[i64] {
    match c.values() {
        ColumnValues::Int64(s) => s,
        _ => unreachable!("caller checked the column is INT64"),
    }
}

fn uint64_slice(c: &Column) -> &[u64] {
    match c.values() {
        ColumnValues::UInt64(s) => s,
        _ => unreachable!("caller checked the column is UINT64"),
    }
}

fn float64_slice(c: &Column) -> &[f64] {
    match c.values() {
        ColumnValues::Float64(s) => s,
        _ => unreachable!("caller checked the column is FLOAT64"),
    }
}

fn decimal_slice(c: &Column) -> &[i128] {
    match c.values() {
        ColumnValues::Decimal(s) => s,
        _ => unreachable!("caller checked the column is DECIMAL"),
    }
}

/// `INT64 op INT64`: `checked_*`, `Div`/`Mod` truncate toward zero (Rust's `/`/`%`) and are
/// NULL by zero, never an error.
fn apply_int64(op: ArithOp, a: i64, b: i64) -> Result<Option<i64>, ExecError> {
    let overflow = || {
        ExecError::Overflow(format!(
            "{} {} {}",
            DataType::Int64,
            op_symbol(op),
            DataType::Int64
        ))
    };
    let result = match op {
        ArithOp::Add => a.checked_add(b),
        ArithOp::Sub => a.checked_sub(b),
        ArithOp::Mul => a.checked_mul(b),
        ArithOp::Div => {
            if b == 0 {
                return Ok(None);
            }
            a.checked_div(b)
        }
        ArithOp::Mod => {
            if b == 0 {
                return Ok(None);
            }
            a.checked_rem(b)
        }
    };
    result.map(Some).ok_or_else(overflow)
}

fn apply_uint64(op: ArithOp, a: u64, b: u64) -> Result<Option<u64>, ExecError> {
    let overflow = || {
        ExecError::Overflow(format!(
            "{} {} {}",
            DataType::UInt64,
            op_symbol(op),
            DataType::UInt64
        ))
    };
    let result = match op {
        ArithOp::Add => a.checked_add(b),
        ArithOp::Sub => a.checked_sub(b),
        ArithOp::Mul => a.checked_mul(b),
        ArithOp::Div => {
            if b == 0 {
                return Ok(None);
            }
            a.checked_div(b)
        }
        ArithOp::Mod => {
            if b == 0 {
                return Ok(None);
            }
            a.checked_rem(b)
        }
    };
    result.map(Some).ok_or_else(overflow)
}

fn int64_arith(op: ArithOp, l: &Column, r: &Column) -> Result<Column, ExecError> {
    let (ls, rs) = (int64_slice(l), int64_slice(r));
    let mut values = Vec::with_capacity(ls.len());
    let mut validity = ValidityBuilder::new();
    for i in 0..ls.len() {
        if l.is_null(i) || r.is_null(i) {
            values.push(0);
            validity.push(false);
            continue;
        }
        match apply_int64(op, ls[i], rs[i])? {
            Some(v) => {
                values.push(v);
                validity.push(true);
            }
            None => {
                values.push(0);
                validity.push(false);
            }
        }
    }
    Ok(Column::from_parts(
        DataType::Int64,
        OwnedValues::Int64(values),
        validity.finish(),
    )
    .expect("int64 arith always builds a valid column"))
}

fn uint64_arith(op: ArithOp, l: &Column, r: &Column) -> Result<Column, ExecError> {
    let (ls, rs) = (uint64_slice(l), uint64_slice(r));
    let mut values = Vec::with_capacity(ls.len());
    let mut validity = ValidityBuilder::new();
    for i in 0..ls.len() {
        if l.is_null(i) || r.is_null(i) {
            values.push(0);
            validity.push(false);
            continue;
        }
        match apply_uint64(op, ls[i], rs[i])? {
            Some(v) => {
                values.push(v);
                validity.push(true);
            }
            None => {
                values.push(0);
                validity.push(false);
            }
        }
    }
    Ok(Column::from_parts(
        DataType::UInt64,
        OwnedValues::UInt64(values),
        validity.finish(),
    )
    .expect("uint64 arith always builds a valid column"))
}

/// FLOAT64 follows IEEE: no overflow, no error, `Div`/`Mod` by zero give inf/NaN, not NULL.
fn float64_arith(op: ArithOp, l: &Column, r: &Column) -> Column {
    let (ls, rs) = (float64_slice(l), float64_slice(r));
    let mut values = Vec::with_capacity(ls.len());
    let mut validity = ValidityBuilder::new();
    for i in 0..ls.len() {
        if l.is_null(i) || r.is_null(i) {
            values.push(0.0);
            validity.push(false);
            continue;
        }
        let v = match op {
            ArithOp::Add => ls[i] + rs[i],
            ArithOp::Sub => ls[i] - rs[i],
            ArithOp::Mul => ls[i] * rs[i],
            ArithOp::Div => ls[i] / rs[i],
            ArithOp::Mod => ls[i] % rs[i],
        };
        values.push(v);
        validity.push(true);
    }
    Column::from_parts(
        DataType::Float64,
        OwnedValues::Float64(values),
        validity.finish(),
    )
    .expect("float64 arith always builds a valid column")
}

/// Rescales `unscaled` (at `from` digits) up to `to` digits (`to >= from`, the result scale is
/// always `max(l, r)`). `None` on i128 overflow.
fn rescale_up(unscaled: i128, from: u8, to: u8) -> Option<i128> {
    if to == from {
        Some(unscaled)
    } else {
        unscaled.checked_mul(pow10(to - from))
    }
}

fn check_decimal_range(unscaled: i128, precision: u8) -> bool {
    unscaled.unsigned_abs() < pow10(precision) as u128
}

/// `Add`/`Sub`/`Mod`: rescale both operands to `out`'s scale, then the integer op at that
/// scale. `Mul`: the raw product, no rescale (the result scale is the sum of the scales).
/// `Div`: both sides to `f64` (`unscaled / 10^scale`), divided — a FLOAT64 result.
fn decimal_arith(
    op: ArithOp,
    l: &Column,
    a: DecimalType,
    r: &Column,
    b: DecimalType,
    out: &DataType,
) -> Result<Column, ExecError> {
    let (ls, rs) = (decimal_slice(l), decimal_slice(r));
    let n = ls.len();
    if op == ArithOp::Div {
        let mut values = Vec::with_capacity(n);
        let mut validity = ValidityBuilder::new();
        let (fa, fb) = (pow10(a.scale()) as f64, pow10(b.scale()) as f64);
        for i in 0..n {
            if l.is_null(i) || r.is_null(i) || rs[i] == 0 {
                values.push(0.0);
                validity.push(false);
                continue;
            }
            values.push((ls[i] as f64 / fa) / (rs[i] as f64 / fb));
            validity.push(true);
        }
        return Ok(Column::from_parts(
            DataType::Float64,
            OwnedValues::Float64(values),
            validity.finish(),
        )
        .expect("decimal div always builds a valid FLOAT64 column"));
    }
    let DataType::Decimal(out_dt) = out else {
        unreachable!("DECIMAL Add/Sub/Mul/Mod always types to DECIMAL")
    };
    let overflow = || {
        ExecError::Overflow(format!(
            "{} {} {}",
            DataType::Decimal(a),
            op_symbol(op),
            DataType::Decimal(b)
        ))
    };
    let mut values = Vec::with_capacity(n);
    let mut validity = ValidityBuilder::new();
    for i in 0..n {
        if l.is_null(i) || r.is_null(i) {
            values.push(0);
            validity.push(false);
            continue;
        }
        let result = if op == ArithOp::Mul {
            ls[i].checked_mul(rs[i]).ok_or_else(overflow)?
        } else {
            let scale = out_dt.scale();
            let lv = rescale_up(ls[i], a.scale(), scale).ok_or_else(overflow)?;
            let rv = rescale_up(rs[i], b.scale(), scale).ok_or_else(overflow)?;
            if op == ArithOp::Mod && rv == 0 {
                values.push(0);
                validity.push(false);
                continue;
            }
            match op {
                ArithOp::Add => lv.checked_add(rv),
                ArithOp::Sub => lv.checked_sub(rv),
                ArithOp::Mod => lv.checked_rem(rv),
                ArithOp::Mul | ArithOp::Div => unreachable!("handled above"),
            }
            .ok_or_else(overflow)?
        };
        if !check_decimal_range(result, out_dt.precision()) {
            return Err(overflow());
        }
        values.push(result);
        validity.push(true);
    }
    Ok(
        Column::from_parts(out.clone(), OwnedValues::Decimal(values), validity.finish())
            .expect("decimal arith always builds a valid column"),
    )
}

pub(crate) fn arith(
    op: ArithOp,
    l: &Column,
    r: &Column,
    out: &DataType,
) -> Result<Column, ExecError> {
    match (l.data_type(), r.data_type()) {
        (DataType::Int64, DataType::Int64) => int64_arith(op, l, r),
        (DataType::UInt64, DataType::UInt64) => uint64_arith(op, l, r),
        (DataType::Float64, DataType::Float64) => Ok(float64_arith(op, l, r)),
        (DataType::Decimal(a), DataType::Decimal(b)) => decimal_arith(op, l, *a, r, *b, out),
        (lt, rt) => Err(ExecError::Plan(format!(
            "cannot apply arithmetic to {lt} and {rt}"
        ))),
    }
}

pub(crate) fn neg(c: &Column) -> Result<Column, ExecError> {
    match c.data_type() {
        DataType::Int64 => neg_int64(c),
        DataType::Float64 => Ok(neg_float64(c)),
        DataType::Decimal(_) => Ok(neg_decimal(c)),
        other => Err(ExecError::Plan(format!("NEG does not apply to {other}"))),
    }
}

fn neg_int64(c: &Column) -> Result<Column, ExecError> {
    let s = int64_slice(c);
    let mut values = Vec::with_capacity(s.len());
    let mut validity = ValidityBuilder::new();
    for (i, &x) in s.iter().enumerate() {
        if c.is_null(i) {
            values.push(0);
            validity.push(false);
            continue;
        }
        let v = x
            .checked_neg()
            .ok_or_else(|| ExecError::Overflow(format!("- {}", DataType::Int64)))?;
        values.push(v);
        validity.push(true);
    }
    Ok(Column::from_parts(
        DataType::Int64,
        OwnedValues::Int64(values),
        validity.finish(),
    )
    .expect("neg always builds a valid column"))
}

fn neg_float64(c: &Column) -> Column {
    let s = float64_slice(c);
    let mut values = Vec::with_capacity(s.len());
    let mut validity = ValidityBuilder::new();
    for (i, &x) in s.iter().enumerate() {
        values.push(if c.is_null(i) { 0.0 } else { -x });
        validity.push(!c.is_null(i));
    }
    Column::from_parts(
        DataType::Float64,
        OwnedValues::Float64(values),
        validity.finish(),
    )
    .expect("neg always builds a valid column")
}

fn neg_decimal(c: &Column) -> Column {
    let ty = c.data_type().clone();
    let s = decimal_slice(c);
    let mut values = Vec::with_capacity(s.len());
    let mut validity = ValidityBuilder::new();
    for (i, &x) in s.iter().enumerate() {
        values.push(if c.is_null(i) { 0 } else { -x });
        validity.push(!c.is_null(i));
    }
    Column::from_parts(ty, OwnedValues::Decimal(values), validity.finish())
        .expect("neg always builds a valid column")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Decimal, Value};

    fn int_col(vals: &[Option<i64>]) -> Column {
        let values: Vec<Value> = vals
            .iter()
            .map(|v| v.map_or(Value::Null, Value::Int64))
            .collect();
        Column::from_values(&DataType::Int64, &values).unwrap()
    }

    fn dec_col(ty: &DataType, vals: &[i128]) -> Column {
        let scale = match ty {
            DataType::Decimal(dt) => dt.scale(),
            _ => unreachable!(),
        };
        let values: Vec<Value> = vals
            .iter()
            .map(|&u| Value::Decimal(Decimal::new(u, scale).unwrap()))
            .collect();
        Column::from_values(ty, &values).unwrap()
    }

    fn int_at(c: &Column, i: usize) -> Option<i64> {
        if c.is_null(i) {
            None
        } else if let Value::Int64(n) = c.get(i) {
            Some(n)
        } else {
            unreachable!()
        }
    }

    #[test]
    fn int64_add_overflow_is_overflow_error() {
        let l = int_col(&[Some(i64::MAX)]);
        let r = int_col(&[Some(1)]);
        let err = arith(ArithOp::Add, &l, &r, &DataType::Int64).unwrap_err();
        assert!(matches!(err, ExecError::Overflow(_)));
    }

    #[test]
    fn int64_div_and_mod_by_zero_are_null() {
        let l = int_col(&[Some(7)]);
        let r = int_col(&[Some(0)]);
        let div = arith(ArithOp::Div, &l, &r, &DataType::Int64).unwrap();
        assert_eq!(int_at(&div, 0), None);
        let m = arith(ArithOp::Mod, &l, &r, &DataType::Int64).unwrap();
        assert_eq!(int_at(&m, 0), None);
    }

    #[test]
    fn int64_div_truncates_toward_zero_mod_has_dividend_sign() {
        let l = int_col(&[Some(-7)]);
        let r = int_col(&[Some(2)]);
        let div = arith(ArithOp::Div, &l, &r, &DataType::Int64).unwrap();
        assert_eq!(int_at(&div, 0), Some(-3));
        let m = arith(ArithOp::Mod, &l, &r, &DataType::Int64).unwrap();
        assert_eq!(int_at(&m, 0), Some(-1));
    }

    #[test]
    fn decimal_add_rescales_to_max_scale() {
        let l = dec_col(&DataType::decimal(5, 1).unwrap(), &[15]); // 1.5
        let r = dec_col(&DataType::decimal(5, 2).unwrap(), &[25]); // 0.25
        let out_ty = DataType::decimal(6, 2).unwrap();
        let out = arith(ArithOp::Add, &l, &r, &out_ty).unwrap();
        let Value::Decimal(d) = out.get(0) else {
            unreachable!()
        };
        assert_eq!((d.unscaled(), d.scale()), (175, 2)); // 1.75
    }

    #[test]
    fn decimal_mul_scale_is_sum_of_scales() {
        let l = dec_col(&DataType::decimal(5, 1).unwrap(), &[15]); // 1.5
        let r = dec_col(&DataType::decimal(5, 2).unwrap(), &[25]); // 0.25
        let out_ty = DataType::decimal(8, 3).unwrap();
        let out = arith(ArithOp::Mul, &l, &r, &out_ty).unwrap();
        let Value::Decimal(d) = out.get(0) else {
            unreachable!()
        };
        assert_eq!((d.unscaled(), d.scale()), (375, 3)); // 0.375
    }

    #[test]
    fn float64_div_by_zero_is_ieee_infinity() {
        let l = Column::from_values(&DataType::Float64, &[Value::Float64(1.0)]).unwrap();
        let r = Column::from_values(&DataType::Float64, &[Value::Float64(0.0)]).unwrap();
        let out = arith(ArithOp::Div, &l, &r, &DataType::Float64).unwrap();
        let Value::Float64(f) = out.get(0) else {
            unreachable!()
        };
        assert!(f.is_infinite() && f > 0.0);
    }

    #[test]
    fn neg_int64_min_overflows() {
        let c = int_col(&[Some(i64::MIN)]);
        let err = neg(&c).unwrap_err();
        assert!(matches!(err, ExecError::Overflow(_)));
    }

    #[test]
    fn neg_null_stays_null() {
        let c = int_col(&[None]);
        let out = neg(&c).unwrap();
        assert!(out.is_null(0));
    }
}
