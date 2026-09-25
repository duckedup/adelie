//! The hash join build side (SPEC §7): buffers the build (right) input, then builds one
//! `JoinTable` keyed by the join columns' encoded bytes.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use crate::exec::kernels::rowkey::NullKeys;
use crate::exec::kernels::{concat_batches, encode_row_key, rechunk};
use crate::exec::operator::Sink;
use crate::exec::{Batch, Column, ExecContext, ExecError, Field, Reservation};

/// Per-`HashMap`-entry overhead (bucket, `Vec<u8>` heap header) the budget estimate charges
/// once per distinct key, on top of the key's own bytes and 4 bytes per row id it holds.
const MAP_ENTRY_OVERHEAD: usize = 48;

/// One worker's build-side partial: buffers batches (reserved as they arrive), `merge`s
/// another worker's partial in, then `into_table` builds the shared `JoinTable`.
pub(crate) struct JoinBuildSink {
    fields: Vec<Field>,
    keys: Vec<usize>,
    batches: Vec<Batch>,
    res: Option<Reservation>,
}

impl JoinBuildSink {
    pub(crate) fn new(fields: Vec<Field>, keys: Vec<usize>) -> JoinBuildSink {
        JoinBuildSink {
            fields,
            keys,
            batches: Vec::new(),
            res: None,
        }
    }

    /// Concatenates the held batches, hashes every row's key (`NullKeys::Skip`: a NULL/NaN key
    /// can never match, so that row is left out of the map), and reserves the table's bytes.
    pub(crate) fn into_table(self, ctx: &ExecContext) -> Result<JoinTable, ExecError> {
        let JoinBuildSink {
            fields,
            keys,
            batches,
            res,
        } = self;
        drop(res); // the table below takes its own reservation; don't hold both at once
        let batch = concat_batches(&fields, &batches)?;
        let mut res = ctx.reserve(batch.byte_size())?;

        let key_cols: Vec<&Column> = keys.iter().map(|&i| batch.column(i)).collect();
        let mut map: HashMap<Vec<u8>, Vec<u32>> = HashMap::new();
        let mut key_bytes = 0usize;
        let mut row_ids = 0usize;
        let mut buf = Vec::new();
        for row in 0..batch.rows() {
            buf.clear();
            if !encode_row_key(&key_cols, row, NullKeys::Skip, &mut buf) {
                continue;
            }
            row_ids += 1;
            match map.entry(buf.clone()) {
                Entry::Occupied(mut e) => e.get_mut().push(row as u32),
                Entry::Vacant(e) => {
                    key_bytes += buf.len();
                    e.insert(vec![row as u32]);
                }
            }
        }
        let map_bytes = key_bytes + map.len() * MAP_ENTRY_OVERHEAD + row_ids * 4;
        res.grow(map_bytes)?;

        Ok(JoinTable {
            batch,
            map,
            keys,
            _res: res,
        })
    }
}

impl Sink for JoinBuildSink {
    fn push(&mut self, ctx: &ExecContext, batch: Batch) -> Result<(), ExecError> {
        ctx.check()?;
        let bytes = batch.byte_size();
        match &mut self.res {
            Some(r) => r.grow(bytes)?,
            None => self.res = Some(ctx.reserve(bytes)?),
        }
        self.batches.push(batch);
        Ok(())
    }

    fn merge(&mut self, ctx: &ExecContext, mut other: JoinBuildSink) -> Result<(), ExecError> {
        // Release `other`'s reservation before growing ours: the bytes are already counted
        // against the shared budget, and this only moves which `Reservation` tracks them.
        let bytes = other.res.take().map_or(0, |r| r.bytes());
        match &mut self.res {
            Some(r) => r.grow(bytes)?,
            None => self.res = Some(ctx.reserve(bytes)?),
        }
        self.batches.append(&mut other.batches);
        Ok(())
    }

    fn finish(self, _ctx: &ExecContext) -> Result<Vec<Batch>, ExecError> {
        rechunk(&self.fields, self.batches)
    }
}

/// The built hash table: one `Batch` (the concatenated build side) plus a map from a join
/// key's encoded bytes to every build row id sharing it, in row order.
pub(crate) struct JoinTable {
    pub(super) batch: Batch,
    pub(super) map: HashMap<Vec<u8>, Vec<u32>>,
    pub(super) keys: Vec<usize>,
    _res: Reservation,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::ExecOptions;
    use crate::types::{DataType, Value};

    fn field(name: &str, ty: DataType) -> Field {
        Field {
            name: name.to_string(),
            ty,
        }
    }

    fn int_batch(fields: Vec<Field>, cols: Vec<Vec<i64>>) -> Batch {
        let columns = cols
            .into_iter()
            .map(|vs| {
                let values: Vec<Value> = vs.into_iter().map(Value::Int64).collect();
                Column::from_values(&DataType::Int64, &values).unwrap()
            })
            .collect();
        Batch::new(fields, columns).unwrap()
    }

    fn table_keys_sorted(table: &JoinTable) -> Vec<(Vec<u8>, Vec<u32>)> {
        let mut rows: Vec<(Vec<u8>, Vec<u32>)> = table
            .map
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        rows.sort();
        rows
    }

    #[test]
    fn merge_of_two_partials_matches_one_build_over_all_rows() {
        let fields = vec![field("k", DataType::Int64), field("v", DataType::Int64)];
        let ctx = ExecContext::unlimited();

        let mut whole = JoinBuildSink::new(fields.clone(), vec![0]);
        whole
            .push(&ctx, int_batch(fields.clone(), vec![vec![1, 2, 1], vec![10, 20, 30]]))
            .unwrap();
        let whole_table = whole.into_table(&ctx).unwrap();

        let mut a = JoinBuildSink::new(fields.clone(), vec![0]);
        a.push(&ctx, int_batch(fields.clone(), vec![vec![1], vec![10]]))
            .unwrap();
        let mut b = JoinBuildSink::new(fields.clone(), vec![0]);
        b.push(&ctx, int_batch(fields.clone(), vec![vec![2, 1], vec![20, 30]]))
            .unwrap();
        a.merge(&ctx, b).unwrap();
        let merged_table = a.into_table(&ctx).unwrap();

        assert_eq!(table_keys_sorted(&whole_table), table_keys_sorted(&merged_table));
        assert_eq!(whole_table.batch.rows(), merged_table.batch.rows());
    }

    #[test]
    fn build_over_a_one_byte_limit_is_budget_exceeded() {
        let opts = ExecOptions {
            memory_limit: 1,
            ..ExecOptions::default()
        };
        let ctx = ExecContext::new(&opts);
        let fields = vec![field("k", DataType::Int64)];
        let mut sink = JoinBuildSink::new(fields.clone(), vec![0]);
        let err = sink
            .push(&ctx, int_batch(fields, vec![vec![1, 2, 3]]))
            .unwrap_err();
        assert!(matches!(err, ExecError::BudgetExceeded { .. }));
    }

    #[test]
    fn skipped_null_key_rows_are_absent_from_the_map() {
        let fields = vec![field("k", DataType::Int64)];
        let ctx = ExecContext::unlimited();
        let mut sink = JoinBuildSink::new(fields.clone(), vec![0]);
        let col = Column::from_values(&DataType::Int64, &[Value::Int64(1), Value::Null]).unwrap();
        sink.push(&ctx, Batch::new(fields, vec![col]).unwrap()).unwrap();
        let table = sink.into_table(&ctx).unwrap();
        let total_row_ids: usize = table.map.values().map(Vec::len).sum();
        assert_eq!(total_row_ids, 1);
    }

    #[test]
    fn finish_returns_the_held_batches_rechunked() {
        let fields = vec![field("k", DataType::Int64)];
        let ctx = ExecContext::unlimited();
        let mut sink = JoinBuildSink::new(fields.clone(), vec![0]);
        sink.push(&ctx, int_batch(fields.clone(), vec![vec![1, 2]])).unwrap();
        sink.push(&ctx, int_batch(fields.clone(), vec![vec![3]])).unwrap();
        let out = sink.finish(&ctx).unwrap();
        let total: usize = out.iter().map(Batch::rows).sum();
        assert_eq!(total, 3);
    }
}
