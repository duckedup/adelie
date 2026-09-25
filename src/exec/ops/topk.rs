//! `TopKSink`: an ORDER BY ... LIMIT k sink that never holds more than `k` rows.

use crate::exec::kernels;
use crate::exec::operator::Sink;
use crate::exec::{Batch, Column, ExecContext, ExecError, Field, Reservation, SortKey};

pub(crate) struct TopKSink {
    fields: Vec<Field>,
    keys: Vec<SortKey>,
    k: usize,
    held: Vec<Batch>,
    res: Option<Reservation>,
}

impl TopKSink {
    pub(crate) fn new(fields: Vec<Field>, keys: Vec<SortKey>, k: usize) -> TopKSink {
        TopKSink {
            fields,
            keys,
            k,
            held: Vec::new(),
            res: None,
        }
    }

    /// Concats `held` with `incoming`, sorts, and keeps the first `k`. Computes the new held
    /// batch before touching `self`, so a `BudgetExceeded` from the resize leaves it untouched.
    fn fold_in(&mut self, ctx: &ExecContext, incoming: Vec<Batch>) -> Result<(), ExecError> {
        if self.k == 0 {
            return Ok(());
        }
        let mut all = self.held.clone();
        all.extend(incoming);
        if all.is_empty() {
            return Ok(());
        }
        let concat = kernels::concat_batches(&self.fields, &all)?;
        let cols: Vec<&Column> = concat.columns().iter().collect();
        let order = kernels::sort_indices(&cols, &self.keys);
        let take_n = order.len().min(self.k);
        let held = kernels::take_batch(&concat, &order[..take_n]);
        match &mut self.res {
            Some(r) => r.resize(held.byte_size())?,
            None => self.res = Some(ctx.reserve(held.byte_size())?),
        }
        self.held = vec![held];
        Ok(())
    }
}

impl Sink for TopKSink {
    fn push(&mut self, ctx: &ExecContext, batch: Batch) -> Result<(), ExecError> {
        ctx.check()?;
        // Held rows come first, so ties against the new batch keep arrival order.
        self.fold_in(ctx, vec![batch])
    }

    fn merge(&mut self, ctx: &ExecContext, other: TopKSink) -> Result<(), ExecError> {
        ctx.check()?;
        self.fold_in(ctx, other.held)
    }

    fn finish(self, _ctx: &ExecContext) -> Result<Vec<Batch>, ExecError> {
        Ok(self.held)
    }

    fn done(&self) -> bool {
        self.k == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DataType, Value};

    fn field(name: &str, ty: DataType) -> Field {
        Field {
            name: name.to_string(),
            ty,
        }
    }

    fn int_batch(f: &Field, values: &[i64]) -> Batch {
        let vals: Vec<Value> = values.iter().copied().map(Value::Int64).collect();
        Batch::new(vec![f.clone()], vec![Column::from_values(&f.ty, &vals).unwrap()]).unwrap()
    }

    fn int_values(batches: &[Batch]) -> Vec<i64> {
        batches
            .iter()
            .flat_map(|b| {
                (0..b.rows()).map(move |i| match b.column(0).get(i) {
                    Value::Int64(v) => v,
                    other => panic!("expected Int64, got {other:?}"),
                })
            })
            .collect()
    }

    #[test]
    fn k_two_over_three_merged_partials_equals_sort_then_take_two() {
        let f = field("a", DataType::Int64);
        let ctx = ExecContext::unlimited();
        let keys = vec![SortKey::asc(0)];

        let mut a = TopKSink::new(vec![f.clone()], keys.clone(), 2);
        a.push(&ctx, int_batch(&f, &[5, 3])).unwrap();
        let mut b = TopKSink::new(vec![f.clone()], keys.clone(), 2);
        b.push(&ctx, int_batch(&f, &[8, 1])).unwrap();
        let mut c = TopKSink::new(vec![f.clone()], keys, 2);
        c.push(&ctx, int_batch(&f, &[9, 2])).unwrap();

        a.merge(&ctx, b).unwrap();
        a.merge(&ctx, c).unwrap();

        assert_eq!(int_values(&a.finish(&ctx).unwrap()), vec![1, 2]);
    }

    #[test]
    fn ties_across_partials_are_a_deterministic_multiset() {
        let f = field("a", DataType::Int64);
        let ctx = ExecContext::unlimited();
        let keys = vec![SortKey::asc(0)];

        let mut a = TopKSink::new(vec![f.clone()], keys.clone(), 2);
        a.push(&ctx, int_batch(&f, &[1, 1])).unwrap();
        let mut b = TopKSink::new(vec![f.clone()], keys, 2);
        b.push(&ctx, int_batch(&f, &[1, 5])).unwrap();
        a.merge(&ctx, b).unwrap();

        let mut got = int_values(&a.finish(&ctx).unwrap());
        got.sort_unstable();
        assert_eq!(got, vec![1, 1]);
    }

    #[test]
    fn k_zero_is_empty_and_done_immediately() {
        let f = field("a", DataType::Int64);
        let mut sink = TopKSink::new(vec![f.clone()], vec![SortKey::asc(0)], 0);
        assert!(sink.done());
        let ctx = ExecContext::unlimited();
        sink.push(&ctx, int_batch(&f, &[1, 2])).unwrap();
        assert!(sink.finish(&ctx).unwrap().is_empty());
    }
}
