//! `LimitSink`: keeps `offset + limit` rows of arrival order, then drops the first `offset`.

use crate::exec::kernels;
use crate::exec::operator::Sink;
use crate::exec::{Batch, ExecContext, ExecError, Field, Reservation};

pub(crate) struct LimitSink {
    fields: Vec<Field>,
    limit: Option<usize>,
    offset: usize,
    batches: Vec<Batch>,
    rows: usize,
    res: Option<Reservation>,
}

impl LimitSink {
    pub(crate) fn new(fields: Vec<Field>, limit: Option<usize>, offset: usize) -> LimitSink {
        LimitSink {
            fields,
            limit,
            offset,
            batches: Vec::new(),
            rows: 0,
            res: None,
        }
    }

    /// Total rows this sink ever holds; `None` (no limit) holds everything.
    fn bound(&self) -> Option<usize> {
        self.limit.map(|l| self.offset + l)
    }
}

impl Sink for LimitSink {
    fn push(&mut self, ctx: &ExecContext, batch: Batch) -> Result<(), ExecError> {
        ctx.check()?;
        if self.limit == Some(0) {
            return Ok(());
        }
        let batch = match self.bound() {
            Some(bound) if self.rows + batch.rows() > bound => {
                kernels::slice_batch(&batch, 0, bound.saturating_sub(self.rows))
            }
            _ => batch,
        };
        if batch.rows() == 0 {
            return Ok(());
        }
        match &mut self.res {
            Some(r) => r.grow(batch.byte_size())?,
            None => self.res = Some(ctx.reserve(batch.byte_size())?),
        }
        self.rows += batch.rows();
        self.batches.push(batch);
        Ok(())
    }

    fn merge(&mut self, ctx: &ExecContext, other: LimitSink) -> Result<(), ExecError> {
        for batch in other.batches {
            self.push(ctx, batch)?;
        }
        Ok(())
    }

    fn finish(self, _ctx: &ExecContext) -> Result<Vec<Batch>, ExecError> {
        let LimitSink {
            fields,
            offset,
            batches,
            ..
        } = self;
        let mut remaining = offset;
        let mut out = Vec::new();
        for batch in batches {
            if remaining >= batch.rows() {
                remaining -= batch.rows();
                continue;
            }
            let start = remaining;
            remaining = 0;
            out.push(kernels::slice_batch(&batch, start, batch.rows() - start));
        }
        kernels::rechunk(&fields, out)
    }

    fn done(&self) -> bool {
        self.limit == Some(0) || self.bound().is_some_and(|b| self.rows >= b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Column;
    use crate::types::{DataType, Value};

    fn field(name: &str, ty: DataType) -> Field {
        Field {
            name: name.to_string(),
            ty,
        }
    }

    fn int_batch(f: &Field, values: &[i64]) -> Batch {
        let vals: Vec<Value> = values.iter().copied().map(Value::Int64).collect();
        Batch::new(
            vec![f.clone()],
            vec![Column::from_values(&f.ty, &vals).unwrap()],
        )
        .unwrap()
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
    fn limit_and_offset_across_merged_partials() {
        let f = field("a", DataType::Int64);
        let ctx = ExecContext::unlimited();

        let mut a = LimitSink::new(vec![f.clone()], Some(3), 2);
        a.push(&ctx, int_batch(&f, &[0, 1, 2, 3])).unwrap();
        assert!(!a.done());

        let mut b = LimitSink::new(vec![f.clone()], Some(3), 2);
        b.push(&ctx, int_batch(&f, &[4, 5, 6, 7])).unwrap();

        a.merge(&ctx, b).unwrap();
        assert!(a.done());

        // Merged arrival order is 0..8; offset 2 skips [0, 1], limit 3 keeps [2, 3, 4].
        let out = a.finish(&ctx).unwrap();
        assert_eq!(int_values(&out), vec![2, 3, 4]);
    }

    #[test]
    fn limit_zero_is_done_immediately() {
        let f = field("a", DataType::Int64);
        let sink = LimitSink::new(vec![f], Some(0), 0);
        assert!(sink.done());
    }
}
