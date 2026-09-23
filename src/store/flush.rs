//! The flusher thread and `run`: the one function that turns buffered batches into a durable,
//! published segment set (SPEC §6). `run`'s IO order is exactly what criterion 1 pins down.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::exec::Batch;
use crate::manifest::{Edit, Manifest, SegmentEntry, TableName};
use crate::segment::{self, WriterOptions};

use super::buffer::{FlushTicket, should_flush};
use super::engine::engine_by_name;
use super::{Error, Shared, io_err};

/// Buffered batches per table, in arrival order.
type Buffered = BTreeMap<TableName, Vec<Arc<Batch>>>;

pub(crate) fn spawn(shared: Arc<Shared>) -> JoinHandle<()> {
    std::thread::spawn(move || loop_forever(&shared))
}

/// Waits for a flush to be due, runs it, resolves its ticket, and repeats — until `closing` is
/// set with nothing pending.
fn loop_forever(shared: &Arc<Shared>) {
    loop {
        let Some((in_flight, ticket)) = wait_for_work(shared) else {
            return;
        };
        let result = run(shared, &in_flight);
        if result.is_err() {
            let mut state = shared.state.lock().unwrap();
            for t in in_flight.keys() {
                state.in_flight.remove(t);
            }
            state.in_flight_ticket = None;
        } else {
            shared.state.lock().unwrap().in_flight_ticket = None;
        }
        ticket.resolve(result.map_err(|e| e.to_string()));
    }
}

/// Blocks until a flush is due and `pending` is non-empty, then takes `pending` into
/// `in_flight` and hands back a copy plus the ticket it must resolve. `None` means: close.
fn wait_for_work(shared: &Arc<Shared>) -> Option<(Buffered, Arc<FlushTicket>)> {
    let mut state = shared.state.lock().unwrap();
    loop {
        let elapsed = state.first_pending.map(|t| t.elapsed());
        let due = should_flush(
            state.pending_rows,
            state.pending_bytes,
            elapsed,
            &shared.opts,
            state.force,
        );
        if due && !state.pending.is_empty() {
            break;
        }
        if state.closing && state.pending.is_empty() {
            return None;
        }
        let timeout = match state.first_pending {
            Some(t) => shared
                .opts
                .flush_interval
                .saturating_sub(t.elapsed())
                .max(Duration::from_millis(1)),
            None => shared.opts.flush_interval.max(Duration::from_millis(1)),
        };
        let (guard, _) = shared.wake.wait_timeout(state, timeout).unwrap();
        state = guard;
    }
    let taken = std::mem::take(&mut state.pending);
    state.pending_rows = 0;
    state.pending_bytes = 0;
    state.first_pending = None;
    state.force = false;
    state.in_flight = taken.clone();
    let ticket = std::mem::replace(&mut state.ticket, FlushTicket::new());
    state.in_flight_ticket = Some(ticket.clone());
    Some((taken, ticket))
}

/// One flush, in the exact order criterion 1 checks:
/// per table, encode a segment per engine output; write + sync each; sync each distinct
/// partition dir; commit `AddSegments` (seq = the new version); ack.
pub(crate) fn run(
    shared: &Arc<Shared>,
    in_flight: &BTreeMap<TableName, Vec<Arc<Batch>>>,
) -> Result<u64, Error> {
    let manifest = shared.state.lock().unwrap().current.manifest().clone();
    let mut written: Vec<(TableName, Vec<SegmentEntry>)> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();

    for (table, batches) in in_flight {
        if batches.is_empty() {
            continue;
        }
        let entry = manifest
            .table(table)
            .ok_or_else(|| Error::UnknownTable(table.to_string()))?;
        let engine = engine_by_name(&entry.engine).ok_or_else(|| {
            Error::Manifest(crate::manifest::Error::UnknownEngine {
                table: table.to_string(),
                engine: entry.engine.clone(),
            })
        })?;
        let outputs = engine.flush(&entry.schema, batches, shared.opts.max_rows)?;

        let partition_dir = shared.root.join(&table.db).join(&table.name).join("_");
        shared
            .io
            .create_dir_all(&partition_dir)
            .map_err(|source| io_err(&partition_dir, source))?;

        let mut segments = Vec::with_capacity(outputs.len());
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
                partition: "_".to_string(),
                seq: 0, // fixed up to the commit's new version once it is known
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
            crate::fail::point("flush.pre_segment_sync");
            shared
                .io
                .sync_file(&file, &path)
                .map_err(|source| io_err(&path, source))?;
            segments.push(seg);
        }
        if !dirs.contains(&partition_dir) {
            dirs.push(partition_dir);
        }
        written.push((table.clone(), segments));
    }

    for dir in &dirs {
        shared
            .io
            .sync_dir(dir)
            .map_err(|source| io_err(dir, source))?;
    }

    crate::fail::point("flush.pre_publish");
    let flushed_tables: Vec<TableName> = written.iter().map(|(t, _)| t.clone()).collect();
    let version = shared.commit(
        |new_version| {
            written
                .into_iter()
                .map(|(table, mut segments)| {
                    for s in &mut segments {
                        s.seq = new_version;
                    }
                    Edit::AddSegments { table, segments }
                })
                .collect()
        },
        &flushed_tables,
    )?;

    crate::fail::point("flush.pre_ack");
    shared.io.ack(version);
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Column, Field};
    use crate::io::{Io, Op};
    use crate::manifest::TableName;
    use crate::store::{Store, StoreOptions};
    use crate::types::{DataType, Value};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "adelie-store-flush-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn schema() -> Vec<Field> {
        vec![Field {
            name: "a".to_string(),
            ty: DataType::UInt64,
        }]
    }

    fn batch(n: u64) -> Batch {
        let v: Vec<Value> = (0..n).map(Value::UInt64).collect();
        Batch::new(
            schema(),
            vec![Column::from_values(&DataType::UInt64, &v).unwrap()],
        )
        .unwrap()
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn a_flush_records_the_exact_durability_order() {
        let dir = temp_dir("io-order");
        let table = TableName::new("d", "t");
        let (io, log) = Io::recording();
        let store = Store::open_with_io(&dir, StoreOptions::default(), io).unwrap();
        store.create_table(&table, "append", schema()).unwrap();
        // `create_table` publishes its own manifest version; clear that out so the first
        // flush's log can be checked in isolation.
        log.lock().unwrap().clear();
        store.write(&table, batch(1)).unwrap();

        // Segment ids start at 0 (a fresh manifest's `next_segment_id`).
        let seg_path = dir
            .join("d")
            .join("t")
            .join("_")
            .join("0000000000000000.seg");
        let root_manifest = dir.join("manifest");
        let root_tmp = dir.join("manifest.tmp");

        // The first flush also creates the db/table/partition dirs; check that up front, then
        // clear the log so the second flush's order can be checked in isolation.
        let first_log = log.lock().unwrap().clone();
        assert!(matches!(first_log[0], Op::CreateDir(_)));
        let write_pos = first_log
            .iter()
            .position(|op| matches!(op, Op::Write(p) if p == &seg_path))
            .unwrap();
        assert!(
            first_log[..write_pos]
                .iter()
                .all(|op| matches!(op, Op::CreateDir(_) | Op::SyncDir(_)))
        );
        log.lock().unwrap().clear();

        store.write(&table, batch(1)).unwrap();
        let second_log = log.lock().unwrap().clone();
        let seg_path2 = dir
            .join("d")
            .join("t")
            .join("_")
            .join("0000000000000001.seg");
        let part_dir = dir.join("d").join("t").join("_");
        let version = store.snapshot().version();
        assert_eq!(
            second_log,
            vec![
                Op::Write(seg_path2.clone()),
                Op::SyncFile(seg_path2),
                Op::SyncDir(part_dir),
                Op::Write(root_tmp.clone()),
                Op::SyncFile(root_tmp.clone()),
                Op::Rename(root_tmp, root_manifest.clone()),
                Op::SyncDir(dir.clone()),
                Op::Link(root_manifest, dir.join(format!("manifest.{version}"))),
                Op::Ack(version),
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
