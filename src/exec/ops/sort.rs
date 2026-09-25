//! `SortSink` and `sort_batches`: the ORDER BY sink and its stable, deterministic kernel.
//! `sort_batches` doubles as the flush-time ORDER BY kernel (`storage::engines::sort`).

use crate::exec::kernels;
use crate::exec::operator::Sink;
use crate::exec::{Batch, Column, ExecContext, ExecError, Field, Reservation, SortKey};

pub(crate) struct SortSink {
    fields: Vec<Field>,
    keys: Vec<SortKey>,
    batches: Vec<Batch>,
    res: Option<Reservation>,
}

impl SortSink {
    pub(crate) fn new(fields: Vec<Field>, keys: Vec<SortKey>) -> SortSink {
        SortSink {
            fields,
            keys,
            batches: Vec::new(),
            res: None,
        }
    }
}

impl Sink for SortSink {
    fn push(&mut self, ctx: &ExecContext, batch: Batch) -> Result<(), ExecError> {
        ctx.check()?;
        match &mut self.res {
            Some(r) => r.grow(batch.byte_size())?,
            None => self.res = Some(ctx.reserve(batch.byte_size())?),
        }
        self.batches.push(batch);
        Ok(())
    }

    fn merge(&mut self, ctx: &ExecContext, other: SortSink) -> Result<(), ExecError> {
        for batch in other.batches {
            self.push(ctx, batch)?;
        }
        Ok(())
    }

    fn finish(self, ctx: &ExecContext) -> Result<Vec<Batch>, ExecError> {
        sort_batches(&self.fields, &self.batches, &self.keys, ctx)
    }
}

/// Stable sort of every row across `batches` over `keys`: ties keep input order, batch order
/// then row order within a batch. Also E3's flush-time ORDER BY kernel.
pub(crate) fn sort_batches(
    fields: &[Field],
    batches: &[Batch],
    keys: &[SortKey],
    ctx: &ExecContext,
) -> Result<Vec<Batch>, ExecError> {
    if batches.is_empty() {
        return Ok(Vec::new());
    }
    let concat = kernels::concat_batches(fields, batches)?;
    let _reservation = ctx.reserve(concat.byte_size())?;
    let cols: Vec<&Column> = concat.columns().iter().collect();
    let order = kernels::sort_indices(&cols, keys);
    let sorted = kernels::take_batch(&concat, &order);
    kernels::rechunk(fields, vec![sorted])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{CancelToken, ExecOptions};
    use crate::types::{DataType, Value};

    fn field(name: &str, ty: DataType) -> Field {
        Field {
            name: name.to_string(),
            ty,
        }
    }

    fn two_col_batch(fields: &[Field], a: &[Value], b: &[Value]) -> Batch {
        Batch::new(
            fields.to_vec(),
            vec![
                Column::from_values(&fields[0].ty, a).unwrap(),
                Column::from_values(&fields[1].ty, b).unwrap(),
            ],
        )
        .unwrap()
    }

    fn one_col_values(out: &[Batch], col: usize) -> Vec<Value> {
        out.iter()
            .flat_map(|b| (0..b.rows()).map(move |i| b.column(col).get(i)))
            .collect()
    }

    #[test]
    fn multi_key_asc_desc_matches_order_limit_slt() {
        // tests/slt/order_limit.slt: ORDER BY a ASC, b DESC over (a, b) rows, shuffled here.
        let fields = [field("a", DataType::Int64), field("b", DataType::Int64)];
        let batch = two_col_batch(
            &fields,
            &[
                Value::Int64(2),
                Value::Int64(3),
                Value::Int64(1),
                Value::Int64(2),
                Value::Int64(1),
            ],
            &[
                Value::Int64(1),
                Value::Int64(7),
                Value::Int64(5),
                Value::Int64(9),
                Value::Int64(3),
            ],
        );
        let ctx = ExecContext::unlimited();
        let out = sort_batches(
            &fields,
            &[batch],
            &[SortKey::asc(0), SortKey::desc(1)],
            &ctx,
        )
        .unwrap();
        assert_eq!(
            one_col_values(&out, 0),
            vec![
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(2),
                Value::Int64(2),
                Value::Int64(3),
            ]
        );
        assert_eq!(
            one_col_values(&out, 1),
            vec![
                Value::Int64(5),
                Value::Int64(3),
                Value::Int64(9),
                Value::Int64(1),
                Value::Int64(7),
            ]
        );
    }

    #[test]
    fn nulls_first_and_nulls_last() {
        let fields = [field("a", DataType::Int64)];
        let batch = Batch::new(
            fields.to_vec(),
            vec![
                Column::from_values(
                    &DataType::Int64,
                    &[Value::Int64(1), Value::Null, Value::Int64(2)],
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let ctx = ExecContext::unlimited();

        let first = SortKey {
            column: 0,
            descending: false,
            nulls_first: true,
        };
        let out = sort_batches(&fields, std::slice::from_ref(&batch), &[first], &ctx).unwrap();
        assert_eq!(
            one_col_values(&out, 0),
            vec![Value::Null, Value::Int64(1), Value::Int64(2)]
        );

        let last = SortKey {
            column: 0,
            descending: false,
            nulls_first: false,
        };
        let out = sort_batches(&fields, &[batch], &[last], &ctx).unwrap();
        assert_eq!(
            one_col_values(&out, 0),
            vec![Value::Int64(1), Value::Int64(2), Value::Null]
        );
    }

    #[test]
    fn stable_across_batches_on_equal_keys() {
        // Falsify: this fails if `sort_indices` is not a stable sort.
        let fields = [field("key", DataType::Int64), field("tag", DataType::Int64)];
        let b1 = two_col_batch(
            &fields,
            &[Value::Int64(0), Value::Int64(0)],
            &[Value::Int64(0), Value::Int64(1)],
        );
        let b2 = two_col_batch(
            &fields,
            &[Value::Int64(0), Value::Int64(0)],
            &[Value::Int64(2), Value::Int64(3)],
        );
        let b3 = two_col_batch(
            &fields,
            &[Value::Int64(0), Value::Int64(0)],
            &[Value::Int64(4), Value::Int64(5)],
        );
        let ctx = ExecContext::unlimited();
        let out = sort_batches(&fields, &[b1, b2, b3], &[SortKey::asc(0)], &ctx).unwrap();
        assert_eq!(
            one_col_values(&out, 1),
            (0..6i64).map(Value::Int64).collect::<Vec<Value>>()
        );
    }

    #[test]
    fn push_reports_budget_exceeded_under_a_tiny_limit() {
        // Falsify: this fails if the sink never reserves.
        let fields = [field("a", DataType::Int64)];
        let opts = ExecOptions {
            memory_limit: 1,
            threads: 1,
            timeout: None,
            cancel: CancelToken::new(),
        };
        let ctx = ExecContext::new(&opts);
        let mut sink = SortSink::new(fields.to_vec(), vec![SortKey::asc(0)]);
        let batch = Batch::new(
            fields.to_vec(),
            vec![
                Column::from_values(&DataType::Int64, &[Value::Int64(1), Value::Int64(2)]).unwrap(),
            ],
        )
        .unwrap();
        let err = sink.push(&ctx, batch).unwrap_err();
        assert!(matches!(err, ExecError::BudgetExceeded { .. }));
    }
}
