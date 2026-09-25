//! Streaming row filter (SPEC §7): keeps rows where `predicate` evaluates true.

use crate::exec::expr::{Expr, eval};
use crate::exec::kernels;
use crate::exec::operator::Operator;
use crate::exec::{Batch, ExecContext, ExecError};

pub(crate) struct Filter {
    predicate: Expr,
}

impl Filter {
    pub(crate) fn new(predicate: Expr) -> Filter {
        Filter { predicate }
    }
}

impl Operator for Filter {
    fn push(
        &mut self,
        _ctx: &ExecContext,
        batch: Batch,
        out: &mut Vec<Batch>,
    ) -> Result<(), ExecError> {
        let mask = eval(&self.predicate, &batch)?;
        let keep = kernels::truthy(&mask);
        let kept = keep.count_valid();
        if kept == batch.rows() {
            // Every row passed: forward the batch itself rather than copy it.
            out.push(batch);
        } else if kept > 0 {
            out.push(kernels::filter_batch(&batch, &keep));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{CmpOp, Column, Field};
    use crate::types::{DataType, Value};

    fn field(name: &str, ty: DataType) -> Field {
        Field {
            name: name.to_string(),
            ty,
        }
    }

    fn one_col_batch(f: &Field, values: &[Value]) -> Batch {
        Batch::new(
            vec![f.clone()],
            vec![Column::from_values(&f.ty, values).unwrap()],
        )
        .unwrap()
    }

    fn gt(col: usize, v: i64) -> Expr {
        Expr::cmp(CmpOp::Gt, Expr::col(col), Expr::lit(Value::Int64(v), DataType::Int64))
    }

    #[test]
    fn keeps_rows_greater_than_one_and_drops_null() {
        let f = field("a", DataType::Int64);
        let batch = one_col_batch(
            &f,
            &[
                Value::Int64(1),
                Value::Int64(2),
                Value::Null,
                Value::Int64(3),
            ],
        );
        let mut filter = Filter::new(gt(0, 1));
        let ctx = ExecContext::unlimited();
        let mut out = Vec::new();
        filter.push(&ctx, batch, &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rows(), 2);
        assert_eq!(out[0].column(0).get(0), Value::Int64(2));
        assert_eq!(out[0].column(0).get(1), Value::Int64(3));
    }

    #[test]
    fn all_false_emits_no_batch() {
        let f = field("a", DataType::Int64);
        let batch = one_col_batch(&f, &[Value::Int64(1), Value::Int64(2)]);
        let mut filter = Filter::new(gt(0, 100));
        let ctx = ExecContext::unlimited();
        let mut out = Vec::new();
        filter.push(&ctx, batch, &mut out).unwrap();
        assert!(out.is_empty());
    }
}
