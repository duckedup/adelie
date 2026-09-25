//! `CollectSink`: gathers every pushed batch unchanged (the plain `Sink` at a pipeline's end).

use crate::exec::kernels;
use crate::exec::operator::Sink;
use crate::exec::{Batch, ExecContext, ExecError, Field, Reservation};

pub(crate) struct CollectSink {
    fields: Vec<Field>,
    batches: Vec<Batch>,
    res: Option<Reservation>,
}

impl CollectSink {
    pub(crate) fn new(fields: Vec<Field>) -> CollectSink {
        CollectSink {
            fields,
            batches: Vec::new(),
            res: None,
        }
    }
}

impl Sink for CollectSink {
    fn push(&mut self, ctx: &ExecContext, batch: Batch) -> Result<(), ExecError> {
        ctx.check()?;
        // The reservation grows before the batch is kept, so `BudgetExceeded` leaves it unheld.
        match &mut self.res {
            Some(r) => r.grow(batch.byte_size())?,
            None => self.res = Some(ctx.reserve(batch.byte_size())?),
        }
        self.batches.push(batch);
        Ok(())
    }

    fn merge(&mut self, ctx: &ExecContext, other: CollectSink) -> Result<(), ExecError> {
        for batch in other.batches {
            self.push(ctx, batch)?;
        }
        Ok(())
    }

    fn finish(self, _ctx: &ExecContext) -> Result<Vec<Batch>, ExecError> {
        kernels::rechunk(&self.fields, self.batches)
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
        Batch::new(vec![f.clone()], vec![Column::from_values(&f.ty, &vals).unwrap()]).unwrap()
    }

    #[test]
    fn merge_appends_and_finish_rechunks() {
        let f = field("a", DataType::Int64);
        let ctx = ExecContext::unlimited();
        let mut a = CollectSink::new(vec![f.clone()]);
        a.push(&ctx, int_batch(&f, &[1, 2])).unwrap();
        let mut b = CollectSink::new(vec![f.clone()]);
        b.push(&ctx, int_batch(&f, &[3])).unwrap();
        a.merge(&ctx, b).unwrap();
        let out = a.finish(&ctx).unwrap();
        let values: Vec<i64> = out
            .iter()
            .flat_map(|batch| {
                (0..batch.rows()).map(move |i| match batch.column(0).get(i) {
                    Value::Int64(v) => v,
                    other => panic!("expected Int64, got {other:?}"),
                })
            })
            .collect();
        assert_eq!(values, vec![1, 2, 3]);
    }
}
