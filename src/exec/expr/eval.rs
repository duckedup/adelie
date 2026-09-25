//! Evaluates `Expr` against a `Batch`, producing the output column. Filled by U2 (adelie-1st).

use crate::exec::kernels::{
    and, bool_column, compare, compare_scalar, not, null_column, or, truthy,
};
use crate::exec::{Batch, Bitmap, Column, ColumnBuilder, ExecError};
use crate::types::{DataType, Value};

use super::{CmpOp, Expr};
use super::{arith, cast, like};

/// Recurses after typing the whole expression, so an ill-typed plan fails as `Plan` before any
/// work is done (the recursive calls re-check their own subtree the same way).
pub fn eval(expr: &Expr, batch: &Batch) -> Result<Column, ExecError> {
    let ty = expr.data_type(batch.fields())?;
    match expr {
        Expr::Column(i) => Ok(batch.column(*i).clone()),
        Expr::Literal(v, _) => literal_column(v, &ty, batch.rows()),
        Expr::Cmp(op, l, r) => eval_cmp(*op, l, r, batch),
        Expr::And(parts) => eval_fold(parts, batch, and),
        Expr::Or(parts) => eval_fold(parts, batch, or),
        Expr::Not(e) => Ok(not(&eval(e, batch)?)),
        Expr::IsNull(e) => Ok(is_null_column(&eval(e, batch)?, false)),
        Expr::IsNotNull(e) => Ok(is_null_column(&eval(e, batch)?, true)),
        Expr::InList {
            expr,
            list,
            negated,
        } => eval_in_list(expr, list, *negated, batch),
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => eval_between(expr, low, high, *negated, batch),
        Expr::Like {
            expr,
            pattern,
            case_insensitive,
            negated,
        } => eval_like(expr, pattern, *case_insensitive, *negated, batch),
        Expr::Arith(op, l, r) => {
            let lc = eval(l, batch)?;
            let rc = eval(r, batch)?;
            arith::arith(*op, &lc, &rc, &ty)
        }
        Expr::Neg(e) => arith::neg(&eval(e, batch)?),
        Expr::Case {
            branches,
            otherwise,
        } => eval_case(branches, otherwise, &ty, batch),
        Expr::Cast(e, _) => {
            let c = eval(e, batch)?;
            cast::cast(&c, &ty)
        }
    }
}

fn literal_column(v: &Value, ty: &DataType, rows: usize) -> Result<Column, ExecError> {
    let mut builder = ColumnBuilder::with_capacity(ty.clone(), rows);
    for _ in 0..rows {
        builder.push(v)?;
    }
    Ok(builder.finish())
}

/// A scalar-literal side uses `compare_scalar` (flipping the op when the literal is on the
/// left); two columns use `compare`. `compare_scalar` already gives an all-NULL BOOL for a
/// NULL literal.
fn eval_cmp(op: CmpOp, l: &Expr, r: &Expr, batch: &Batch) -> Result<Column, ExecError> {
    match (l, r) {
        (Expr::Literal(v, _), _) => {
            let rc = eval(r, batch)?;
            compare_scalar(&rc, op.flip(), v)
        }
        (_, Expr::Literal(v, _)) => {
            let lc = eval(l, batch)?;
            compare_scalar(&lc, op, v)
        }
        _ => {
            let lc = eval(l, batch)?;
            let rc = eval(r, batch)?;
            compare(&lc, op, &rc)
        }
    }
}

fn eval_fold(
    parts: &[Expr],
    batch: &Batch,
    f: fn(&Column, &Column) -> Column,
) -> Result<Column, ExecError> {
    let mut iter = parts.iter();
    let first = iter.next().expect("data_type checked AND/OR is non-empty");
    let mut acc = eval(first, batch)?;
    for p in iter {
        let next = eval(p, batch)?;
        acc = f(&acc, &next);
    }
    Ok(acc)
}

fn is_null_column(c: &Column, want_not_null: bool) -> Column {
    let mut bits = Bitmap::new_valid(0);
    for i in 0..c.len() {
        bits.push(c.is_null(i) != want_not_null);
    }
    bool_column(bits, None)
}

/// The OR of `Eq` against each value (SQL three-valued rules fall out of `compare_scalar`'s
/// and `or`'s own NULL handling): `x IN (1, NULL)` is TRUE, else NULL, never FALSE.
fn eval_in_list(
    expr: &Expr,
    list: &[Value],
    negated: bool,
    batch: &Batch,
) -> Result<Column, ExecError> {
    let col = eval(expr, batch)?;
    let mut acc: Option<Column> = None;
    for v in list {
        let eq = compare_scalar(&col, CmpOp::Eq, v)?;
        acc = Some(match acc {
            None => eq,
            Some(prev) => or(&prev, &eq),
        });
    }
    let result = acc.unwrap_or_else(|| bool_column(Bitmap::new_null(col.len()), None));
    Ok(if negated { not(&result) } else { result })
}

fn eval_between(
    expr: &Expr,
    low: &Expr,
    high: &Expr,
    negated: bool,
    batch: &Batch,
) -> Result<Column, ExecError> {
    let ge_low = eval_cmp(CmpOp::Ge, expr, low, batch)?;
    let le_high = eval_cmp(CmpOp::Le, expr, high, batch)?;
    let result = and(&ge_low, &le_high);
    Ok(if negated { not(&result) } else { result })
}

fn eval_like(
    expr: &Expr,
    pattern: &str,
    case_insensitive: bool,
    negated: bool,
    batch: &Batch,
) -> Result<Column, ExecError> {
    let col = eval(expr, batch)?;
    let result = like::like(&col, pattern, case_insensitive)?;
    Ok(if negated { not(&result) } else { result })
}

/// Every branch condition and result is evaluated fully first (correctness over
/// short-circuiting), then each row picks its first `truthy` branch, or falls to `otherwise`.
fn eval_case(
    branches: &[(Expr, Expr)],
    otherwise: &Option<Box<Expr>>,
    ty: &DataType,
    batch: &Batch,
) -> Result<Column, ExecError> {
    let mut conds = Vec::with_capacity(branches.len());
    let mut results = Vec::with_capacity(branches.len());
    for (cond, res) in branches {
        conds.push(truthy(&eval(cond, batch)?));
        results.push(eval(res, batch)?);
    }
    let otherwise_col = match otherwise {
        Some(e) => eval(e, batch)?,
        None => null_column(ty, batch.rows()),
    };
    let mut builder = ColumnBuilder::with_capacity(ty.clone(), batch.rows());
    for row in 0..batch.rows() {
        let picked = conds
            .iter()
            .zip(&results)
            .find(|(cond, _)| cond.get(row))
            .map(|(_, res)| res.get(row));
        let v = picked.unwrap_or_else(|| otherwise_col.get(row));
        builder.push(&v)?;
    }
    Ok(builder.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{ArithOp, Field};

    fn field(name: &str, ty: DataType) -> Field {
        Field {
            name: name.to_string(),
            ty,
        }
    }

    fn batch(fields: Vec<Field>, cols: Vec<Column>) -> Batch {
        Batch::new(fields, cols).unwrap()
    }

    fn int_col(vals: &[Option<i64>]) -> Column {
        let values: Vec<Value> = vals
            .iter()
            .map(|v| v.map_or(Value::Null, Value::Int64))
            .collect();
        Column::from_values(&DataType::Int64, &values).unwrap()
    }

    fn bool_at(c: &Column, i: usize) -> Option<bool> {
        if c.is_null(i) {
            None
        } else if let Value::Bool(b) = c.get(i) {
            Some(b)
        } else {
            unreachable!()
        }
    }

    fn assert_type_matches(expr: &Expr, b: &Batch, out: &Column) {
        assert_eq!(out.data_type(), &expr.data_type(b.fields()).unwrap());
    }

    fn five_row_batch() -> Batch {
        batch(
            vec![field("a", DataType::Int64)],
            vec![int_col(&[Some(1), Some(2), None, Some(4), Some(5)])],
        )
    }

    #[test]
    fn column_and_literal() {
        let b = five_row_batch();
        let out = eval(&Expr::col(0), &b).unwrap();
        assert_type_matches(&Expr::col(0), &b, &out);
        assert_eq!(out.get(0), Value::Int64(1));

        let lit = Expr::lit(Value::Int64(9), DataType::Int64);
        let out = eval(&lit, &b).unwrap();
        assert_type_matches(&lit, &b, &out);
        for i in 0..5 {
            assert_eq!(out.get(i), Value::Int64(9));
        }
    }

    #[test]
    fn cmp_with_literal_on_either_side() {
        let b = five_row_batch();
        let right = Expr::cmp(
            CmpOp::Lt,
            Expr::col(0),
            Expr::lit(Value::Int64(2), DataType::Int64),
        );
        let out = eval(&right, &b).unwrap();
        assert_type_matches(&right, &b, &out);
        assert_eq!(bool_at(&out, 0), Some(true));
        assert_eq!(bool_at(&out, 1), Some(false));
        assert_eq!(bool_at(&out, 2), None);

        let left = Expr::cmp(
            CmpOp::Lt,
            Expr::lit(Value::Int64(2), DataType::Int64),
            Expr::col(0),
        );
        let out2 = eval(&left, &b).unwrap();
        // 2 < a  <=>  a > 2
        assert_eq!(bool_at(&out2, 3), Some(true));
        assert_eq!(bool_at(&out2, 0), Some(false));
    }

    #[test]
    fn and_or_not() {
        let b = five_row_batch();
        let gt1 = Expr::cmp(
            CmpOp::Gt,
            Expr::col(0),
            Expr::lit(Value::Int64(1), DataType::Int64),
        );
        let lt5 = Expr::cmp(
            CmpOp::Lt,
            Expr::col(0),
            Expr::lit(Value::Int64(5), DataType::Int64),
        );
        let and_e = Expr::And(vec![gt1.clone(), lt5.clone()]);
        let out = eval(&and_e, &b).unwrap();
        assert_type_matches(&and_e, &b, &out);
        assert_eq!(bool_at(&out, 1), Some(true)); // a=2
        assert_eq!(bool_at(&out, 0), Some(false)); // a=1

        let or_e = Expr::Or(vec![gt1.clone(), lt5.clone()]);
        let out = eval(&or_e, &b).unwrap();
        assert_eq!(bool_at(&out, 0), Some(true));

        let not_e = Expr::Not(Box::new(gt1));
        let out = eval(&not_e, &b).unwrap();
        assert_eq!(bool_at(&out, 0), Some(true));
    }

    #[test]
    fn is_null_and_is_not_null() {
        let b = five_row_batch();
        let isn = Expr::IsNull(Box::new(Expr::col(0)));
        let out = eval(&isn, &b).unwrap();
        assert_type_matches(&isn, &b, &out);
        assert_eq!(bool_at(&out, 2), Some(true));
        assert_eq!(bool_at(&out, 0), Some(false));
        assert_eq!(out.null_count(), 0);

        let isnn = Expr::IsNotNull(Box::new(Expr::col(0)));
        let out = eval(&isnn, &b).unwrap();
        assert_eq!(bool_at(&out, 2), Some(false));
        assert_eq!(bool_at(&out, 0), Some(true));
    }

    /// Falsify: fails if IN is built with plain (non-NULL-aware) boolean equality.
    #[test]
    fn in_list_with_null_is_true_or_null_never_false() {
        let b = batch(
            vec![field("a", DataType::Int64)],
            vec![int_col(&[Some(1), Some(2), None])],
        );
        let in_e = Expr::InList {
            expr: Box::new(Expr::col(0)),
            list: vec![Value::Int64(1), Value::Null],
            negated: false,
        };
        let out = eval(&in_e, &b).unwrap();
        assert_type_matches(&in_e, &b, &out);
        assert_eq!(bool_at(&out, 0), Some(true));
        assert_eq!(bool_at(&out, 1), None);
        assert_eq!(bool_at(&out, 2), None);

        let not_in = Expr::InList {
            expr: Box::new(Expr::col(0)),
            list: vec![Value::Int64(1), Value::Null],
            negated: true,
        };
        let out = eval(&not_in, &b).unwrap();
        assert_eq!(bool_at(&out, 0), Some(false));
        assert_eq!(bool_at(&out, 1), None);
        assert_eq!(bool_at(&out, 2), None);
    }

    #[test]
    fn between_and_negated() {
        let b = five_row_batch();
        let bt = Expr::Between {
            expr: Box::new(Expr::col(0)),
            low: Box::new(Expr::lit(Value::Int64(2), DataType::Int64)),
            high: Box::new(Expr::lit(Value::Int64(4), DataType::Int64)),
            negated: false,
        };
        let out = eval(&bt, &b).unwrap();
        assert_type_matches(&bt, &b, &out);
        assert_eq!(bool_at(&out, 0), Some(false));
        assert_eq!(bool_at(&out, 1), Some(true));
        assert_eq!(bool_at(&out, 3), Some(true));
        assert_eq!(bool_at(&out, 4), Some(false));
    }

    #[test]
    fn like_and_negated() {
        let b = batch(
            vec![field("s", DataType::String)],
            vec![
                Column::from_values(
                    &DataType::String,
                    &[Value::String("abc".into()), Value::String("xyz".into())],
                )
                .unwrap(),
            ],
        );
        let l = Expr::Like {
            expr: Box::new(Expr::col(0)),
            pattern: "a%".into(),
            case_insensitive: false,
            negated: false,
        };
        let out = eval(&l, &b).unwrap();
        assert_type_matches(&l, &b, &out);
        assert_eq!(bool_at(&out, 0), Some(true));
        assert_eq!(bool_at(&out, 1), Some(false));
    }

    #[test]
    fn arith_and_neg() {
        let b = five_row_batch();
        let add = Expr::Arith(
            ArithOp::Add,
            Box::new(Expr::col(0)),
            Box::new(Expr::lit(Value::Int64(10), DataType::Int64)),
        );
        let out = eval(&add, &b).unwrap();
        assert_type_matches(&add, &b, &out);
        assert_eq!(out.get(0), Value::Int64(11));
        assert!(out.is_null(2));

        let neg = Expr::Neg(Box::new(Expr::col(0)));
        let out = eval(&neg, &b).unwrap();
        assert_eq!(out.get(0), Value::Int64(-1));
    }

    #[test]
    fn case_picks_first_true_branch_and_falls_to_otherwise_or_null() {
        let b = five_row_batch();
        let case_e = Expr::Case {
            branches: vec![
                (
                    Expr::cmp(
                        CmpOp::Eq,
                        Expr::col(0),
                        Expr::lit(Value::Int64(1), DataType::Int64),
                    ),
                    Expr::lit(Value::Int64(100), DataType::Int64),
                ),
                (
                    Expr::cmp(
                        CmpOp::Eq,
                        Expr::col(0),
                        Expr::lit(Value::Int64(4), DataType::Int64),
                    ),
                    Expr::lit(Value::Int64(400), DataType::Int64),
                ),
            ],
            otherwise: Some(Box::new(Expr::lit(Value::Int64(-1), DataType::Int64))),
        };
        let out = eval(&case_e, &b).unwrap();
        assert_type_matches(&case_e, &b, &out);
        assert_eq!(out.get(0), Value::Int64(100));
        assert_eq!(out.get(3), Value::Int64(400));
        assert_eq!(out.get(1), Value::Int64(-1));
        assert_eq!(out.get(2), Value::Int64(-1)); // NULL cond is not truthy -> otherwise

        let no_otherwise = Expr::Case {
            branches: vec![(
                Expr::cmp(
                    CmpOp::Eq,
                    Expr::col(0),
                    Expr::lit(Value::Int64(1), DataType::Int64),
                ),
                Expr::lit(Value::Int64(100), DataType::Int64),
            )],
            otherwise: None,
        };
        let out = eval(&no_otherwise, &b).unwrap();
        assert!(out.is_null(1));
    }

    #[test]
    fn cast_expr() {
        let b = five_row_batch();
        let c = Expr::Cast(Box::new(Expr::col(0)), DataType::Float64);
        let out = eval(&c, &b).unwrap();
        assert_type_matches(&c, &b, &out);
        assert_eq!(out.get(0), Value::Float64(1.0));
    }

    #[test]
    fn ill_typed_cmp_is_plan_error() {
        let b = batch(
            vec![field("a", DataType::Int64), field("s", DataType::String)],
            vec![
                int_col(&[Some(1)]),
                Column::from_values(&DataType::String, &[Value::String("x".into())]).unwrap(),
            ],
        );
        let bad = Expr::cmp(CmpOp::Eq, Expr::col(0), Expr::col(1));
        let err = eval(&bad, &b).unwrap_err();
        assert!(matches!(err, ExecError::Plan(_)));
    }
}
