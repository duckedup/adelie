//! Minimal append compaction (SPEC §18): `prepare` reads and re-encodes, `commit` swaps inputs
//! for outputs under OCC. The straddle check here, not the engine, is what a plan can't skip.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::storage::manifest::{Edit, Manifest, SegmentEntry, TableEntry, TableName};
use crate::storage::segment::{self, WriterOptions};

use super::engines::{MergePlan, engine_by_name};
use super::read::read_segment;
use super::{Error, Shared, io_err};

/// The outcome of `prepare`, ready for `commit`. Kept flat (not per-plan) since a table's
/// several merge plans are disjoint and land in one commit.
pub(crate) struct Prepared {
    pub(crate) table: TableName,
    pub(crate) removes: Vec<u64>,
    pub(crate) adds: Vec<SegmentEntry>,
}

/// Rejects a plan whose inputs are not all live, not all in `plan.partition`, or whose seq
/// range straddles a tombstone (`min(input seq) < t.seq <= max(input seq)`).
pub(crate) fn validate_merge(table: &TableEntry, plan: &MergePlan) -> Result<(), Error> {
    if plan.inputs.is_empty() {
        return Err(Error::Usage("merge plan has no inputs".to_string()));
    }
    let mut min_seq = u64::MAX;
    let mut max_seq = 0u64;
    for &id in &plan.inputs {
        let seg = table
            .segments
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| Error::Usage(format!("merge input {id} is not live")))?;
        if seg.partition != plan.partition {
            return Err(Error::Usage(format!(
                "merge input {id} is not in partition {}",
                plan.partition
            )));
        }
        min_seq = min_seq.min(seg.seq);
        max_seq = max_seq.max(seg.seq);
    }
    for t in &table.tombstones {
        if min_seq < t.seq && t.seq <= max_seq {
            return Err(Error::Usage(format!(
                "merge plan for partition {} straddles the tombstone at seq {}",
                plan.partition, t.seq
            )));
        }
    }
    Ok(())
}

/// Runs every merge plan the table's engine proposes: reads the inputs, re-encodes the
/// outputs, and writes + syncs them (flush steps 1-4). `None` if the engine proposed nothing.
pub(crate) fn prepare(shared: &Arc<Shared>, table: &TableName) -> Result<Option<Prepared>, Error> {
    let manifest = shared.state.lock().unwrap().current.manifest().clone();
    let entry = manifest
        .table(table)
        .ok_or_else(|| Error::UnknownTable(table.to_string()))?;
    let engine = engine_by_name(&entry.engine).ok_or_else(|| {
        Error::Manifest(crate::storage::manifest::Error::UnknownEngine {
            table: table.to_string(),
            engine: entry.engine.clone(),
        })
    })?;
    let plans = engine.plan_merge(entry, &shared.opts);
    if plans.is_empty() {
        return Ok(None);
    }

    let mut removes = Vec::new();
    let mut adds = Vec::new();
    for plan in &plans {
        validate_merge(entry, plan)?;
        let max_seq = plan
            .inputs
            .iter()
            .map(|id| entry.segments.iter().find(|s| s.id == *id).unwrap().seq)
            .max()
            .unwrap();

        let mut inputs = Vec::with_capacity(plan.inputs.len());
        for &id in &plan.inputs {
            let seg = entry.segments.iter().find(|s| s.id == id).unwrap();
            inputs.push(read_segment(&shared.root, table, seg)?);
        }
        let outputs = engine.merge(&entry.schema, inputs, shared.opts.max_rows)?;

        let partition_dir = shared
            .root
            .join(&table.db)
            .join(&table.name)
            .join(&plan.partition);
        shared
            .io
            .create_dir_all(&partition_dir)
            .map_err(|source| io_err(&partition_dir, source))?;
        for output in outputs {
            let id = shared.next_id.fetch_add(1, Ordering::SeqCst);
            let opts = WriterOptions {
                indexes: engine.indexes(&entry.schema),
                ..Default::default()
            };
            let mut writer = segment::Writer::new(Vec::new(), entry.schema.clone(), opts)?;
            for b in &output {
                writer.push(b)?;
            }
            let (bytes, meta) = writer.finish()?;
            let seg = SegmentEntry {
                id,
                partition: plan.partition.clone(),
                seq: max_seq,
                rows: meta.rows,
                bytes: meta.bytes,
                footer_crc: meta.footer_crc,
                columns: meta.columns,
                side_files: Vec::new(),
            };
            let path = Manifest::segment_path(&shared.root, table, &seg);
            let file = shared
                .io
                .write_new(&path, &bytes)
                .map_err(|source| io_err(&path, source))?;
            shared
                .io
                .sync_file(&file, &path)
                .map_err(|source| io_err(&path, source))?;
            adds.push(seg);
        }
        shared
            .io
            .sync_dir(&partition_dir)
            .map_err(|source| io_err(&partition_dir, source))?;
        removes.extend(plan.inputs.iter().copied());
    }

    Ok(Some(Prepared {
        table: table.clone(),
        removes,
        adds,
    }))
}

/// One commit: `RemoveSegments` then `AddSegments`. A stale `Prepared` (an input already gone)
/// comes back as `Error::Manifest(Conflict)`; the outputs it wrote become orphans.
pub(crate) fn commit(shared: &Arc<Shared>, prepared: Prepared) -> Result<u64, Error> {
    shared.commit(
        |_new_version| {
            vec![
                Edit::RemoveSegments {
                    table: prepared.table.clone(),
                    ids: prepared.removes,
                },
                Edit::AddSegments {
                    table: prepared.table,
                    segments: prepared.adds,
                },
            ]
        },
        &[],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Column, Field};
    use crate::storage::{Store, StoreOptions};
    use crate::types::{DataType, Value};
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "adelie-store-compact-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn schema() -> Vec<Field> {
        vec![Field {
            name: "a".to_string(),
            ty: DataType::Int64,
        }]
    }

    fn batch(v: i64) -> crate::exec::Batch {
        crate::exec::Batch::new(
            schema(),
            vec![Column::from_values(&DataType::Int64, &[Value::Int64(v)]).unwrap()],
        )
        .unwrap()
    }

    fn open(dir: &std::path::Path, opts: StoreOptions) -> Store {
        let store = Store::open(dir, opts).unwrap();
        store
            .create_table(&TableName::new("d", "t"), "append", schema())
            .unwrap();
        store
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn validate_merge_rejects_a_plan_straddling_a_tombstone() {
        let table = TableEntry {
            name: TableName::new("d", "t"),
            engine: "append".to_string(),
            schema: schema(),
            segments: (1..=3)
                .chain(5..=6)
                .map(|id| SegmentEntry {
                    id,
                    partition: "_".to_string(),
                    seq: id,
                    rows: 1,
                    bytes: 1,
                    footer_crc: 0,
                    columns: vec![],
                    side_files: vec![],
                })
                .collect(),
            tombstones: vec![crate::storage::manifest::Tombstone {
                seq: 4,
                predicates: vec![],
            }],
        };
        let straddling = MergePlan {
            partition: "_".to_string(),
            inputs: vec![3, 5],
        };
        assert!(validate_merge(&table, &straddling).is_err());
        let one_side = MergePlan {
            partition: "_".to_string(),
            inputs: vec![1, 2, 3],
        };
        assert!(validate_merge(&table, &one_side).is_ok());
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn compact_preserves_rows_and_reduces_segment_count() {
        let dir = temp_dir("preserves-rows");
        let opts = StoreOptions {
            compact_min_inputs: 2,
            compact_small_rows: 100,
            ..StoreOptions::default()
        };
        let store = open(&dir, opts);
        let table = TableName::new("d", "t");
        for v in 0..4 {
            store.write(&table, batch(v)).unwrap();
        }
        let before = store.snapshot().scan(&table).unwrap();
        let before_segments = store.snapshot().table(&table).unwrap().segments.len();

        let version = store.compact(&table).unwrap();
        assert!(version.is_some());

        let after = store.snapshot().scan(&table).unwrap();
        let after_segments = store.snapshot().table(&table).unwrap().segments.len();
        assert!(after_segments < before_segments);
        assert_eq!(before.iter().map(|b| b.rows()).sum::<usize>(), 4);
        assert_eq!(after.iter().map(|b| b.rows()).sum::<usize>(), 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn a_concurrent_flush_does_not_conflict_but_a_repeat_commit_does() {
        let dir = temp_dir("occ");
        let opts = StoreOptions {
            compact_min_inputs: 2,
            compact_small_rows: 100,
            ..StoreOptions::default()
        };
        let store = open(&dir, opts);
        let table = TableName::new("d", "t");
        store.write(&table, batch(1)).unwrap();
        store.write(&table, batch(2)).unwrap();

        let prepared = prepare(&store.shared, &table).unwrap().unwrap();
        // A flush lands between prepare and commit: it must not conflict.
        store.write(&table, batch(3)).unwrap();
        let removes = prepared.removes.clone();
        let adds = prepared.adds.clone();
        commit(&store.shared, prepared).unwrap();

        let flushed_row = store
            .snapshot()
            .scan(&table)
            .unwrap()
            .iter()
            .map(|b| b.rows())
            .sum::<usize>();
        assert_eq!(flushed_row, 3);

        let repeat = Prepared {
            table: table.clone(),
            removes,
            adds,
        };
        assert!(matches!(
            commit(&store.shared, repeat),
            Err(Error::Manifest(
                crate::storage::manifest::Error::Conflict { .. }
            ))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
