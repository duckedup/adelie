//! `append` (SPEC §18): every row is kept. Compaction only merges small segments, and a scan
//! reads every live segment independently.

use std::sync::Arc;

use crate::exec::Batch;
use crate::storage::manifest::{FieldId, SegmentEntry, TableEntry};

use super::sort::sort_rows;
use super::{Engine, MergePlan, ScanPlan};
use crate::storage::{Error, StoreOptions};

/// The append-only engine: rows are independent, segments are never reordered or deduplicated.
pub struct Append;

impl Engine for Append {
    fn name(&self) -> &'static str {
        "append"
    }

    fn flush(
        &self,
        table: &TableEntry,
        batches: &[Arc<Batch>],
        max_rows: usize,
    ) -> Result<Vec<Vec<Batch>>, Error> {
        if table.order_by.is_empty() {
            return Ok(split_into_segments(
                batches.iter().map(|b| (**b).clone()),
                max_rows,
            ));
        }
        let sorted = sort_rows(
            &table.fields(),
            &batches.iter().map(|b| (**b).clone()).collect::<Vec<_>>(),
            &order_positions(table),
        )?;
        Ok(split_into_segments(sorted.into_iter(), max_rows))
    }

    fn merge(
        &self,
        table: &TableEntry,
        inputs: Vec<Vec<Batch>>,
        max_rows: usize,
    ) -> Result<Vec<Vec<Batch>>, Error> {
        // Inputs arrive in seq order, so `sort_rows`'s stable tie-break keeps that order.
        let flattened: Vec<Batch> = inputs.into_iter().flatten().collect();
        if table.order_by.is_empty() {
            return Ok(split_into_segments(flattened.into_iter(), max_rows));
        }
        let sorted = sort_rows(&table.fields(), &flattened, &order_positions(table))?;
        Ok(split_into_segments(sorted.into_iter(), max_rows))
    }

    fn resolve(&self, table: &TableEntry) -> ScanPlan {
        ScanPlan {
            segments: table.segments.iter().map(|s| s.id).collect(),
            merge_key: None,
        }
    }

    fn plan_merge(&self, table: &TableEntry, opts: &StoreOptions) -> Vec<MergePlan> {
        let mut plans = Vec::new();
        for partition in distinct_partitions(&table.segments) {
            plan_partition(table, &partition, opts, &mut plans);
        }
        plans
    }
}

/// Each `table.order_by` field id, as an index into `table.schema` (and so into `table.fields()`
/// and every batch built from it).
fn order_positions(table: &TableEntry) -> Vec<usize> {
    table
        .order_by
        .iter()
        .map(|id| position_of(table, *id))
        .collect()
}

fn position_of(table: &TableEntry, id: FieldId) -> usize {
    table
        .schema
        .iter()
        .position(|f| f.id == id)
        .expect("order_by only ever names a field id present in schema")
}

/// Concatenates `batches` in order, starting a new segment each time the next batch would push
/// the current one past `max_rows`. A batch is never split, so one segment can exceed
/// `max_rows` alone.
fn split_into_segments(batches: impl Iterator<Item = Batch>, max_rows: usize) -> Vec<Vec<Batch>> {
    let mut segments = Vec::new();
    let mut current = Vec::new();
    let mut current_rows = 0usize;
    for batch in batches {
        if !current.is_empty() && current_rows + batch.rows() > max_rows {
            segments.push(std::mem::take(&mut current));
            current_rows = 0;
        }
        current_rows += batch.rows();
        current.push(batch);
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// Partition names in first-seen (i.e. seq/id) order, each listed once.
fn distinct_partitions(segments: &[SegmentEntry]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for s in segments {
        if !seen.iter().any(|p| p == &s.partition) {
            seen.push(s.partition.clone());
        }
    }
    seen
}

/// Walks one partition's live segments in seq order, closing the current run whenever a
/// segment is too big, a tombstone would straddle it, or the run hits the 32-input cap.
fn plan_partition(
    table: &TableEntry,
    partition: &str,
    opts: &StoreOptions,
    plans: &mut Vec<MergePlan>,
) {
    const MAX_INPUTS: usize = 32;
    let mut run: Vec<u64> = Vec::new();
    let mut run_min_seq = 0u64;
    for seg in table.segments.iter().filter(|s| s.partition == partition) {
        let too_big = seg.rows >= opts.compact_small_rows;
        let straddles = !run.is_empty()
            && table
                .tombstones
                .iter()
                .any(|t| run_min_seq < t.seq && t.seq <= seg.seq);
        if too_big || straddles || run.len() >= MAX_INPUTS {
            close_run(plans, partition, &mut run, opts.compact_min_inputs);
        }
        if too_big {
            continue;
        }
        if run.is_empty() {
            run_min_seq = seg.seq;
        }
        run.push(seg.id);
    }
    close_run(plans, partition, &mut run, opts.compact_min_inputs);
}

/// Emits `run` as a `MergePlan` if it meets `min_inputs`, then clears it either way.
fn close_run(plans: &mut Vec<MergePlan>, partition: &str, run: &mut Vec<u64>, min_inputs: usize) {
    if run.len() >= min_inputs {
        plans.push(MergePlan {
            partition: partition.to_string(),
            inputs: std::mem::take(run),
        });
    } else {
        run.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Field;
    use crate::storage::engines::engine_by_name;
    use crate::storage::manifest::{
        FieldId, Predicate, SchemaField, TableId, TableName, Tombstone,
    };
    use crate::types::{DataType, Value};
    use std::collections::BTreeMap;

    fn field() -> Field {
        Field {
            name: "a".to_string(),
            ty: DataType::Int64,
        }
    }

    fn batch(rows: usize) -> Batch {
        let vals: Vec<Value> = (0..rows as i64).map(Value::Int64).collect();
        Batch::new(
            vec![field()],
            vec![crate::exec::Column::from_values(&DataType::Int64, &vals).unwrap()],
        )
        .unwrap()
    }

    fn seg(id: u64, seq: u64, rows: u64) -> SegmentEntry {
        SegmentEntry {
            id,
            partition: "_".to_string(),
            seq,
            rows,
            bytes: 1,
            footer_crc: 0,
            columns: Vec::new(),
            side_files: Vec::new(),
            dir: "d/0000000000000001".to_string(),
            field_ids: vec![FieldId(1)],
            file_field_ids: Vec::new(),
        }
    }

    fn table(segments: Vec<SegmentEntry>, tombstones: Vec<Tombstone>) -> TableEntry {
        TableEntry {
            id: TableId(1),
            name: TableName::new("d", "t"),
            engine: "append".to_string(),
            schema: vec![SchemaField {
                id: FieldId(1),
                field: field(),
            }],
            next_field_id: 2,
            key: vec![],
            version: None,
            order_by: vec![],
            partition_by: None,
            ttl: None,
            options: BTreeMap::new(),
            segments,
            tombstones,
        }
    }

    #[test]
    fn engine_by_name_resolves_append_and_rejects_unknown() {
        assert_eq!(engine_by_name("append").unwrap().name(), "append");
        assert!(engine_by_name("nope").is_none());
    }

    #[test]
    fn flush_never_splits_a_batch_and_starts_a_new_segment_past_max_rows() {
        let batches: Vec<Arc<Batch>> =
            vec![Arc::new(batch(3)), Arc::new(batch(3)), Arc::new(batch(3))];
        let t = table(vec![], vec![]);
        let out = Append.flush(&t, &batches, 5).unwrap();
        let rows_per_seg: Vec<usize> = out
            .iter()
            .map(|s| s.iter().map(Batch::rows).sum())
            .collect();
        assert_eq!(rows_per_seg, vec![3, 3, 3]);
        assert_eq!(out, vec![vec![batch(3)], vec![batch(3)], vec![batch(3)]]);
    }

    #[test]
    fn merge_concatenates_inputs_in_order_and_splits_the_same_way() {
        let inputs = vec![vec![batch(4)], vec![batch(4)]];
        let t = table(vec![], vec![]);
        let out = Append.merge(&t, inputs, 5).unwrap();
        assert_eq!(out, vec![vec![batch(4)], vec![batch(4)]]);
    }

    fn int_batch(vals: &[i64]) -> Batch {
        let vs: Vec<Value> = vals.iter().map(|&n| Value::Int64(n)).collect();
        Batch::new(
            vec![field()],
            vec![crate::exec::Column::from_values(&DataType::Int64, &vs).unwrap()],
        )
        .unwrap()
    }

    /// Flattens every segment's batches' single `a` column into one `Vec<i64>`.
    fn int_rows(out: &[Vec<Batch>]) -> Vec<i64> {
        out.iter()
            .flatten()
            .flat_map(|b| {
                (0..b.rows()).map(|i| match b.column(0).get(i) {
                    Value::Int64(v) => v,
                    other => panic!("expected Int64, got {other:?}"),
                })
            })
            .collect()
    }

    #[test]
    fn flush_sorts_by_order_by_and_leaves_an_unordered_table_untouched() {
        let batches: Vec<Arc<Batch>> =
            vec![Arc::new(int_batch(&[5, 2])), Arc::new(int_batch(&[4, 1]))];

        let unordered = table(vec![], vec![]);
        let out = Append.flush(&unordered, &batches, 10).unwrap();
        assert_eq!(int_rows(&out), vec![5, 2, 4, 1]);

        let ordered = TableEntry {
            order_by: vec![FieldId(1)],
            ..table(vec![], vec![])
        };
        let out = Append.flush(&ordered, &batches, 10).unwrap();
        assert_eq!(out.len(), 1, "small enough to land in one segment");
        assert_eq!(int_rows(&out), vec![1, 2, 4, 5]);
    }

    #[test]
    fn merge_sorts_by_order_by_with_ties_kept_in_seq_order() {
        let inputs = vec![vec![int_batch(&[3, 9])], vec![int_batch(&[1, 7])]];
        let ordered = TableEntry {
            order_by: vec![FieldId(1)],
            ..table(vec![], vec![])
        };
        let out = Append.merge(&ordered, inputs, 10).unwrap();
        assert_eq!(int_rows(&out), vec![1, 3, 7, 9]);

        // Ties on the sort key: `b` tells the rows apart, and the earlier input (lower seq) must
        // come first, so reversing the inputs at compaction goes red.
        let b = Field {
            name: "b".to_string(),
            ty: DataType::Int64,
        };
        let pairs = |rows: &[(i64, i64)]| {
            let col = |f: fn(&(i64, i64)) -> i64| {
                let vals: Vec<Value> = rows.iter().map(|r| Value::Int64(f(r))).collect();
                crate::exec::Column::from_values(&DataType::Int64, &vals).unwrap()
            };
            Batch::new(vec![field(), b.clone()], vec![col(|r| r.0), col(|r| r.1)]).unwrap()
        };
        let mut two_cols = ordered.clone();
        two_cols.schema.push(SchemaField {
            id: FieldId(2),
            field: b.clone(),
        });
        let inputs = vec![
            vec![pairs(&[(1, 10), (2, 20)])],
            vec![pairs(&[(0, 5), (1, 11)])],
        ];
        let out = Append.merge(&two_cols, inputs, 10).unwrap();
        let rows: Vec<(Value, Value)> = out
            .iter()
            .flatten()
            .flat_map(|batch| {
                (0..batch.rows()).map(|i| (batch.column(0).get(i), batch.column(1).get(i)))
            })
            .collect();
        let want: Vec<(Value, Value)> = [(0, 5), (1, 10), (1, 11), (2, 20)]
            .iter()
            .map(|&(a, b)| (Value::Int64(a), Value::Int64(b)))
            .collect();
        assert_eq!(rows, want);
    }

    #[test]
    fn resolve_lists_every_live_segment_in_seq_id_order_with_no_merge_key() {
        let t = table(vec![seg(1, 1, 1), seg(2, 2, 1)], vec![]);
        let plan = Append.resolve(&t);
        assert_eq!(plan.segments, vec![1, 2]);
        assert_eq!(plan.merge_key, None);
    }

    #[test]
    fn plan_merge_groups_small_segments_up_to_min_inputs() {
        let t = table(vec![seg(1, 1, 1), seg(2, 2, 1), seg(3, 3, 1)], vec![]);
        let opts = StoreOptions {
            compact_min_inputs: 2,
            compact_small_rows: 10,
            ..StoreOptions::default()
        };
        let plans = Append.plan_merge(&t, &opts);
        assert_eq!(
            plans,
            vec![MergePlan {
                partition: "_".to_string(),
                inputs: vec![1, 2, 3]
            }]
        );
    }

    #[test]
    fn plan_merge_never_straddles_a_tombstone() {
        let t = table(
            vec![
                seg(1, 1, 1),
                seg(2, 2, 1),
                seg(3, 3, 1),
                seg(5, 5, 1),
                seg(6, 6, 1),
            ],
            vec![Tombstone {
                seq: 4,
                predicates: vec![Predicate {
                    column: "a".to_string(),
                    op: crate::storage::manifest::CmpOp::Eq,
                    value: Value::Int64(0),
                }],
            }],
        );
        let opts = StoreOptions {
            compact_min_inputs: 2,
            compact_small_rows: 10,
            ..StoreOptions::default()
        };
        let plans = Append.plan_merge(&t, &opts);
        for plan in &plans {
            let min = *plan.inputs.iter().min().unwrap();
            let max = *plan.inputs.iter().max().unwrap();
            assert!(
                !(min < 4 && 4 <= max),
                "plan {plan:?} straddles the tombstone"
            );
        }
        assert_eq!(plans.len(), 2);
    }

    #[test]
    fn plan_merge_excludes_a_large_segment_and_drops_a_short_run() {
        let t = table(vec![seg(1, 1, 1), seg(2, 2, 100)], vec![]);
        let opts = StoreOptions {
            compact_min_inputs: 2,
            compact_small_rows: 10,
            ..StoreOptions::default()
        };
        assert!(Append.plan_merge(&t, &opts).is_empty());
    }
}
