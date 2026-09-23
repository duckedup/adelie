//! The table-engine boundary (SPEC §18, Q12): flush, merge policy, scan resolution and
//! optional index builders. The core (commits, snapshots, GC, the straddle check) is fixed;
//! only these hooks vary per engine. `append` is the only engine E4 ships.

use std::sync::Arc;

use crate::exec::{Batch, Field};
use crate::manifest::{SegmentEntry, TableEntry};
use crate::segment::IndexKind;

use super::{Error, StoreOptions};

/// A merge plan: compact these input segment ids of `partition` together. The core validates
/// every plan (`compact::validate_merge`) before running it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePlan {
    pub partition: String,
    pub inputs: Vec<u64>,
}

/// Which segments a scan should read, and the key rows are merged on (`None`: rows are
/// independent, as `append` produces).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanPlan {
    pub segments: Vec<u64>,
    pub merge_key: Option<Vec<String>>,
}

/// A table engine (SPEC §18). Implementations are stateless: everything they need comes in
/// through the arguments.
pub trait Engine: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// Buffered batches, in arrival order, grouped into one `Vec<Batch>` per output segment.
    fn flush(
        &self,
        schema: &[Field],
        batches: &[Arc<Batch>],
        max_rows: usize,
    ) -> Result<Vec<Vec<Batch>>, Error>;

    /// Which live segments to compact together. The core validates the result.
    fn plan_merge(&self, table: &TableEntry, opts: &StoreOptions) -> Vec<MergePlan>;

    /// Input rows, one `Vec<Batch>` per input in seq order, merged into output segments.
    fn merge(
        &self,
        schema: &[Field],
        inputs: Vec<Vec<Batch>>,
        max_rows: usize,
    ) -> Result<Vec<Vec<Batch>>, Error>;

    /// Scan resolution over the table's unmerged segments.
    fn resolve(&self, table: &TableEntry) -> ScanPlan;

    /// Skip indexes to build for every segment this engine writes. None by default.
    fn indexes(&self, _schema: &[Field]) -> Vec<(String, IndexKind)> {
        Vec::new()
    }
}

/// The append-only engine: rows are independent, segments are never reordered or deduplicated.
pub struct Append;

impl Engine for Append {
    fn name(&self) -> &'static str {
        "append"
    }

    fn flush(
        &self,
        _schema: &[Field],
        batches: &[Arc<Batch>],
        max_rows: usize,
    ) -> Result<Vec<Vec<Batch>>, Error> {
        Ok(split_into_segments(
            batches.iter().map(|b| (**b).clone()),
            max_rows,
        ))
    }

    fn merge(
        &self,
        _schema: &[Field],
        inputs: Vec<Vec<Batch>>,
        max_rows: usize,
    ) -> Result<Vec<Vec<Batch>>, Error> {
        Ok(split_into_segments(inputs.into_iter().flatten(), max_rows))
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
fn plan_partition(table: &TableEntry, partition: &str, opts: &StoreOptions, plans: &mut Vec<MergePlan>) {
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

/// `"append"` maps to `&Append`; every other name is unknown to the core.
pub fn engine_by_name(name: &str) -> Option<&'static dyn Engine> {
    static APPEND: Append = Append;
    match name {
        "append" => Some(&APPEND),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Predicate, TableName, Tombstone};
    use crate::types::{DataType, Value};

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
        }
    }

    fn table(segments: Vec<SegmentEntry>, tombstones: Vec<Tombstone>) -> TableEntry {
        TableEntry {
            name: TableName::new("d", "t"),
            engine: "append".to_string(),
            schema: vec![field()],
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
        let batches: Vec<Arc<Batch>> = vec![Arc::new(batch(3)), Arc::new(batch(3)), Arc::new(batch(3))];
        let out = Append.flush(&[field()], &batches, 5).unwrap();
        let rows_per_seg: Vec<usize> = out.iter().map(|s| s.iter().map(Batch::rows).sum()).collect();
        assert_eq!(rows_per_seg, vec![3, 3, 3]);
        assert_eq!(out, vec![vec![batch(3)], vec![batch(3)], vec![batch(3)]]);
    }

    #[test]
    fn merge_concatenates_inputs_in_order_and_splits_the_same_way() {
        let inputs = vec![vec![batch(4)], vec![batch(4)]];
        let out = Append.merge(&[field()], inputs, 5).unwrap();
        assert_eq!(out, vec![vec![batch(4)], vec![batch(4)]]);
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
            vec![seg(1, 1, 1), seg(2, 2, 1), seg(3, 3, 1), seg(5, 5, 1), seg(6, 6, 1)],
            vec![Tombstone {
                seq: 4,
                predicates: vec![Predicate {
                    column: "a".to_string(),
                    op: crate::manifest::CmpOp::Eq,
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
            assert!(!(min < 4 && 4 <= max), "plan {plan:?} straddles the tombstone");
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
