//! Comparison kernels (SPEC §7): `Eq`/`Ne` follow `types::sql_eq`, `Lt`/`Le`/`Gt`/`Ge` follow
//! `types::total_cmp`. NULL on either side gives NULL.

use std::cmp::Ordering;

use crate::exec::expr::CmpOp;
use crate::exec::{Column, ExecError};
use crate::types::{DataType, Value, sql_eq, total_cmp};

use super::boolean::BoolBuilder;

/// Compares every row of `col` to the scalar `v`. `v` NULL gives an all-NULL result; a kind
/// mismatch between `col` and `v` is `Plan`.
pub(crate) fn compare_scalar(col: &Column, op: CmpOp, v: &Value) -> Result<Column, ExecError> {
    if v.is_null() {
        let mut out = BoolBuilder::new();
        for _ in 0..col.len() {
            out.push(None);
        }
        return Ok(out.finish());
    }
    if !matches_kind(col.data_type(), v) {
        return Err(ExecError::Plan(format!(
            "cannot compare {} column to {v:?}",
            col.data_type()
        )));
    }
    let mut out = BoolBuilder::new();
    for i in 0..col.len() {
        if col.is_null(i) {
            out.push(None);
        } else {
            out.push(Some(apply_cmp(op, &col.get(i), v)));
        }
    }
    Ok(out.finish())
}

/// Row-by-row comparison of two same-length columns. A kind mismatch is `Plan`.
pub(crate) fn compare(left: &Column, op: CmpOp, right: &Column) -> Result<Column, ExecError> {
    if left.len() != right.len() {
        return Err(ExecError::Plan(format!(
            "compare length mismatch: {} vs {}",
            left.len(),
            right.len()
        )));
    }
    if !same_kind(left.data_type(), right.data_type()) {
        return Err(ExecError::Plan(format!(
            "cannot compare {} to {}",
            left.data_type(),
            right.data_type()
        )));
    }
    let mut out = BoolBuilder::new();
    for i in 0..left.len() {
        if left.is_null(i) || right.is_null(i) {
            out.push(None);
        } else {
            out.push(Some(apply_cmp(op, &left.get(i), &right.get(i))));
        }
    }
    Ok(out.finish())
}

/// `Eq`/`Ne` via `sql_eq` (NaN ≠ NaN, −0.0 = 0.0); `Lt`/`Le`/`Gt`/`Ge` via `total_cmp` (NaN
/// greatest). Both are hand-written, never derived `PartialEq`/`partial_cmp`.
fn apply_cmp(op: CmpOp, a: &Value, b: &Value) -> bool {
    match op {
        CmpOp::Eq => sql_eq(a, b).expect("same-kind values always compare"),
        CmpOp::Ne => !sql_eq(a, b).expect("same-kind values always compare"),
        CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => {
            let ord = total_cmp(a, b).expect("same-kind values always compare");
            match op {
                CmpOp::Lt => ord == Ordering::Less,
                CmpOp::Le => ord != Ordering::Greater,
                CmpOp::Gt => ord == Ordering::Greater,
                CmpOp::Ge => ord != Ordering::Less,
                CmpOp::Eq | CmpOp::Ne => unreachable!("handled above"),
            }
        }
    }
}

fn matches_kind(ty: &DataType, v: &Value) -> bool {
    match (ty, v) {
        (DataType::Bool, Value::Bool(_))
        | (DataType::Int64, Value::Int64(_))
        | (DataType::UInt64, Value::UInt64(_))
        | (DataType::Float64, Value::Float64(_))
        | (DataType::Decimal(_), Value::Decimal(_))
        | (DataType::String, Value::String(_))
        | (DataType::Bytes, Value::Bytes(_))
        | (DataType::Timestamp, Value::Timestamp(_))
        | (DataType::Date, Value::Date(_))
        | (DataType::Uuid, Value::Uuid(_))
        | (DataType::Ip, Value::Ip(_)) => true,
        (DataType::List(lt), Value::List(items)) => items
            .iter()
            .all(|item| item.is_null() || matches_kind(lt.element(), item)),
        _ => false,
    }
}

/// Any two DECIMALs match regardless of scale; LIST needs a matching element type; everything
/// else is exact `DataType` equality (mirrors `expr::Expr::data_type`'s `Cmp` rule).
fn same_kind(a: &DataType, b: &DataType) -> bool {
    match (a, b) {
        (DataType::Decimal(_), DataType::Decimal(_)) => true,
        (DataType::List(la), DataType::List(lb)) => la.element() == lb.element(),
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Decimal;

    fn int_col(vals: &[Option<i64>]) -> Column {
        let values: Vec<Value> = vals
            .iter()
            .map(|v| v.map_or(Value::Null, Value::Int64))
            .collect();
        Column::from_values(&DataType::Int64, &values).unwrap()
    }

    fn bool_at(col: &Column, i: usize) -> Option<bool> {
        if col.is_null(i) {
            None
        } else if let Value::Bool(b) = col.get(i) {
            Some(b)
        } else {
            unreachable!()
        }
    }

    #[test]
    fn int64_every_cmp_op_with_nulls() {
        let col = int_col(&[Some(1), Some(2), None]);
        let cases: [(CmpOp, i64, [Option<bool>; 3]); 6] = [
            (CmpOp::Eq, 1, [Some(true), Some(false), None]),
            (CmpOp::Ne, 1, [Some(false), Some(true), None]),
            (CmpOp::Lt, 2, [Some(true), Some(false), None]),
            (CmpOp::Le, 2, [Some(true), Some(true), None]),
            (CmpOp::Gt, 1, [Some(false), Some(true), None]),
            (CmpOp::Ge, 1, [Some(true), Some(true), None]),
        ];
        for (op, scalar, expected) in cases {
            let out = compare_scalar(&col, op, &Value::Int64(scalar)).unwrap();
            for (i, want) in expected.iter().enumerate() {
                assert_eq!(bool_at(&out, i), *want, "{op:?} row {i}");
            }
        }
    }

    /// Falsify: fails if the kernel uses derived `PartialEq`/`partial_cmp` for FLOAT64.
    #[test]
    fn float64_nan_and_signed_zero() {
        let col = Column::from_values(
            &DataType::Float64,
            &[Value::Float64(f64::NAN), Value::Float64(-0.0)],
        )
        .unwrap();
        let eq_nan = compare_scalar(&col, CmpOp::Eq, &Value::Float64(f64::NAN)).unwrap();
        assert_eq!(bool_at(&eq_nan, 0), Some(false), "NaN = NaN is false");
        let gt = compare_scalar(&col, CmpOp::Gt, &Value::Float64(1e308)).unwrap();
        assert_eq!(
            bool_at(&gt, 0),
            Some(true),
            "NaN > 1e308 is true (NaN is greatest)"
        );
        let eq_zero = compare_scalar(&col, CmpOp::Eq, &Value::Float64(0.0)).unwrap();
        assert_eq!(bool_at(&eq_zero, 1), Some(true), "-0.0 = 0.0 is true");
    }

    #[test]
    fn cross_scale_decimal_equality() {
        let col = Column::from_values(
            &DataType::decimal(5, 1).unwrap(),
            &[Value::Decimal(Decimal::new(15, 1).unwrap())],
        )
        .unwrap();
        let scalar = Value::Decimal(Decimal::new(150, 2).unwrap());
        let out = compare_scalar(&col, CmpOp::Eq, &scalar).unwrap();
        assert_eq!(bool_at(&out, 0), Some(true));
    }

    #[test]
    fn string_bytewise() {
        let col = Column::from_values(
            &DataType::String,
            &[Value::String("abc".into()), Value::String("abd".into())],
        )
        .unwrap();
        let out = compare_scalar(&col, CmpOp::Lt, &Value::String("abd".into())).unwrap();
        assert_eq!(bool_at(&out, 0), Some(true));
        assert_eq!(bool_at(&out, 1), Some(false));
    }

    #[test]
    fn null_scalar_gives_all_null() {
        let col = int_col(&[Some(1), None]);
        let out = compare_scalar(&col, CmpOp::Eq, &Value::Null).unwrap();
        assert_eq!(bool_at(&out, 0), None);
        assert_eq!(bool_at(&out, 1), None);
    }

    #[test]
    fn kind_mismatch_is_plan_error() {
        let col = int_col(&[Some(1)]);
        let err = compare_scalar(&col, CmpOp::Eq, &Value::UInt64(1)).unwrap_err();
        assert!(matches!(err, ExecError::Plan(_)));
    }

    #[test]
    fn compare_columns_kind_mismatch_is_plan() {
        let a = int_col(&[Some(1)]);
        let b = Column::from_values(&DataType::UInt64, &[Value::UInt64(1)]).unwrap();
        let err = compare(&a, CmpOp::Eq, &b).unwrap_err();
        assert!(matches!(err, ExecError::Plan(_)));
    }

    #[test]
    fn compare_columns_agree_row_by_row() {
        let a = int_col(&[Some(1), Some(2), None]);
        let b = int_col(&[Some(1), Some(3), Some(4)]);
        let out = compare(&a, CmpOp::Lt, &b).unwrap();
        assert_eq!(bool_at(&out, 0), Some(false));
        assert_eq!(bool_at(&out, 1), Some(true));
        assert_eq!(bool_at(&out, 2), None);
    }
}
