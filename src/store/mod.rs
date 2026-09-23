//! The store (SPEC §5, §6, §18, D0009).

mod buffer;
mod compact;
mod engine;
mod error;
mod flush;
mod gc;
mod lock;
mod read;

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::exec::{Batch, Field};
use crate::io::Io;
use crate::manifest::{self, Commit, Edit, Predicate, Publisher, Snapshot, TableEntry, TableName};

use buffer::{FlushTicket, should_flush};

pub use engine::{Append, Engine, MergePlan, ScanPlan, engine_by_name};
pub use error::Error;

/// `Store::open` defaults and the compaction/GC policy (SPEC §6, §18).
#[derive(Debug, Clone)]
pub struct StoreOptions {
    pub max_rows: usize,
    pub max_bytes: usize,
    pub flush_interval: Duration,
    pub retain_manifests: usize,
    pub gc_grace: Duration,
    pub compact_min_inputs: usize,
    pub compact_small_rows: u64,
}

impl Default for StoreOptions {
    fn default() -> Self {
        StoreOptions {
            max_rows: 1_000_000,
            max_bytes: 64 * 1024 * 1024,
            flush_interval: Duration::from_millis(250),
            retain_manifests: 8,
            gc_grace: Duration::from_secs(5 * 60),
            compact_min_inputs: 4,
            compact_small_rows: 262_144,
        }
    }
}

/// State shared between the `Store` handle, its flusher thread, and every `View` it hands out.
struct Shared {
    root: PathBuf,
    opts: StoreOptions,
    io: Io,
    publisher: Publisher,
    state: Mutex<State>,
    /// The flusher waits here for new work or a timeout.
    wake: Condvar,
    /// Serialises apply + publish + the `current`/`in_flight` swap across flush/delete/compact/gc.
    commit: Mutex<()>,
    /// In-process readers, consulted by `gc` before deleting a compacted-away file.
    live: Mutex<Vec<Weak<Snapshot>>>,
    next_id: AtomicU64,
}

/// The write buffer plus the manifest snapshot writers and readers see.
struct State {
    current: Arc<Snapshot>,
    pending: BTreeMap<TableName, Vec<Arc<Batch>>>,
    pending_rows: usize,
    pending_bytes: usize,
    first_pending: Option<Instant>,
    /// The flush the current `pending` rows will land in.
    ticket: Arc<FlushTicket>,
    /// Taken from `pending` by the flusher; not yet reflected in `current`.
    in_flight: BTreeMap<TableName, Vec<Arc<Batch>>>,
    in_flight_ticket: Option<Arc<FlushTicket>>,
    force: bool,
    closing: bool,
}

impl Shared {
    /// One commit, start to finish: lock `commit`, read `current`, let `edits_for` build edits
    /// against the version they will land at, apply, publish, then swap `current` in and clear
    /// `clear_in_flight`'s tables — all under `state`'s lock, so a `View` never sees a gap.
    fn commit(
        &self,
        edits_for: impl FnOnce(u64) -> Vec<Edit>,
        clear_in_flight: &[TableName],
    ) -> Result<u64, Error> {
        let _guard = self.commit.lock().unwrap();
        let (base, manifest) = {
            let state = self.state.lock().unwrap();
            (state.current.version(), state.current.manifest().clone())
        };
        let edits = edits_for(base + 1);
        let next = Commit { base, edits }.apply(&manifest, now_ms())?;
        self.publisher.publish(&next)?;
        let version = next.version;
        let mut state = self.state.lock().unwrap();
        state.current = Arc::new(Snapshot::new(self.root.clone(), next));
        for t in clear_in_flight {
            state.in_flight.remove(t);
        }
        Ok(version)
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn io_err(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// The store: the single writer process for `dir` (SPEC §6). `Send + Sync`.
pub struct Store {
    shared: Arc<Shared>,
    flusher: Option<JoinHandle<()>>,
    _lock: File,
}

impl Store {
    /// Creates `dir` if missing, takes `dir/lock`, loads the manifest, checks every table's
    /// engine is known, cleans up orphaned segment files, and starts the flusher thread.
    pub fn open(dir: impl AsRef<Path>, opts: StoreOptions) -> Result<Store, Error> {
        Self::open_internal(dir.as_ref(), opts, Io::real())
    }

    /// Same as `open`, with a caller-supplied `Io` (tests: `Io::recording()`).
    #[cfg(test)]
    pub(crate) fn open_with_io(
        dir: impl AsRef<Path>,
        opts: StoreOptions,
        io: Io,
    ) -> Result<Store, Error> {
        Self::open_internal(dir.as_ref(), opts, io)
    }

    fn open_internal(dir: &Path, opts: StoreOptions, io: Io) -> Result<Store, Error> {
        std::fs::create_dir_all(dir).map_err(|source| io_err(dir, source))?;
        let root = dir.to_path_buf();
        let lock_file = lock::acquire(&root)?;

        let publisher = Publisher::new(root.clone(), io.clone(), opts.retain_manifests);
        let loaded = publisher.load()?;
        for table in &loaded.tables {
            if engine_by_name(&table.engine).is_none() {
                return Err(Error::Manifest(manifest::Error::UnknownEngine {
                    table: table.name.to_string(),
                    engine: table.engine.clone(),
                }));
            }
        }
        gc::cleanup_orphans(&root, &loaded, &io)?;

        let next_id = AtomicU64::new(loaded.next_segment_id);
        let state = State {
            current: Arc::new(Snapshot::new(root.clone(), loaded)),
            pending: BTreeMap::new(),
            pending_rows: 0,
            pending_bytes: 0,
            first_pending: None,
            ticket: FlushTicket::new(),
            in_flight: BTreeMap::new(),
            in_flight_ticket: None,
            force: false,
            closing: false,
        };
        let shared = Arc::new(Shared {
            root,
            opts,
            io,
            publisher,
            state: Mutex::new(state),
            wake: Condvar::new(),
            commit: Mutex::new(()),
            live: Mutex::new(Vec::new()),
            next_id,
        });
        let flusher = flush::spawn(shared.clone());
        Ok(Store {
            shared,
            flusher: Some(flusher),
            _lock: lock_file,
        })
    }

    pub fn create_table(
        &self,
        name: &TableName,
        engine: &str,
        schema: Vec<Field>,
    ) -> Result<(), Error> {
        if engine_by_name(engine).is_none() {
            return Err(Error::Manifest(manifest::Error::UnknownEngine {
                table: name.to_string(),
                engine: engine.to_string(),
            }));
        }
        let name = name.clone();
        let engine = engine.to_string();
        self.shared.commit(
            move |_v| {
                vec![Edit::CreateTable {
                    name,
                    engine,
                    schema,
                }]
            },
            &[],
        )?;
        Ok(())
    }

    /// Blocks until the batch is acked (durably flushed and published); returns that version.
    pub fn write(&self, table: &TableName, batch: Batch) -> Result<u64, Error> {
        self.write_many(vec![(table.clone(), batch)])
    }

    /// All the writes land in one flush and one commit: atomic together.
    pub fn write_many(&self, writes: Vec<(TableName, Batch)>) -> Result<u64, Error> {
        let ticket = {
            let mut state = self.shared.state.lock().unwrap();
            if state.closing {
                return Err(Error::Closed);
            }
            for (table, batch) in &writes {
                let entry = state
                    .current
                    .manifest()
                    .table(table)
                    .ok_or_else(|| Error::UnknownTable(table.to_string()))?;
                if batch.fields() != entry.schema.as_slice() {
                    return Err(Error::SchemaMismatch {
                        table: table.to_string(),
                        detail: describe_mismatch(batch.fields(), &entry.schema),
                    });
                }
            }
            if state.first_pending.is_none() {
                state.first_pending = Some(Instant::now());
            }
            for (table, batch) in writes {
                state.pending_rows += batch.rows();
                state.pending_bytes += batch.byte_size();
                state
                    .pending
                    .entry(table)
                    .or_default()
                    .push(Arc::new(batch));
            }
            let should = should_flush(
                state.pending_rows,
                state.pending_bytes,
                state.first_pending.map(|t| t.elapsed()),
                &self.shared.opts,
                state.force,
            );
            let ticket = state.ticket.clone();
            drop(state);
            if should {
                self.shared.wake.notify_one();
            }
            ticket
        };
        ticket.wait().map_err(Error::Flush)
    }

    /// Forces a flush and waits for it. `Ok(current version)` with no commit if there was
    /// nothing pending and no flush in flight.
    pub fn flush(&self) -> Result<u64, Error> {
        let (pending_ticket, in_flight_ticket) = {
            let mut state = self.shared.state.lock().unwrap();
            let pending_ticket = (!state.pending.is_empty()).then(|| state.ticket.clone());
            let in_flight_ticket = state.in_flight_ticket.clone();
            if pending_ticket.is_none() && in_flight_ticket.is_none() {
                return Ok(state.current.version());
            }
            // Only set when there is something to force: a stale `force` would flush the next
            // write alone instead of letting it group-commit.
            if pending_ticket.is_some() {
                state.force = true;
            }
            (pending_ticket, in_flight_ticket)
        };
        self.shared.wake.notify_one();
        if let Some(t) = &in_flight_ticket {
            t.wait().map_err(Error::Flush)?;
        }
        match pending_ticket {
            Some(t) => t.wait().map_err(Error::Flush),
            None => in_flight_ticket.unwrap().wait().map_err(Error::Flush),
        }
    }

    /// Flushes pending rows first, then commits the tombstone: rows written before the delete
    /// therefore always sit at a lower seq than it (SPEC §6 delete ordering).
    pub fn delete(&self, table: &TableName, predicates: Vec<Predicate>) -> Result<u64, Error> {
        self.flush()?;
        let table = table.clone();
        self.shared.commit(
            move |_v| vec![Edit::AddTombstone { table, predicates }],
            &[],
        )
    }

    /// Merges every plan the table's engine proposes into one commit. `None` if it proposed
    /// none.
    pub fn compact(&self, table: &TableName) -> Result<Option<u64>, Error> {
        match compact::prepare(&self.shared, table)? {
            Some(prepared) => {
                crate::fail::point("compact.pre_publish");
                let version = compact::commit(&self.shared, prepared)?;
                crate::fail::point("compact.pre_gc");
                self.gc()?;
                Ok(Some(version))
            }
            None => Ok(None),
        }
    }

    pub fn gc(&self) -> Result<usize, Error> {
        gc::run(&self.shared)
    }

    pub fn snapshot(&self) -> View {
        let state = self.shared.state.lock().unwrap();
        let current = state.current.clone();
        let mut buffered: BTreeMap<TableName, Vec<Arc<Batch>>> = BTreeMap::new();
        for (t, v) in state.in_flight.iter().chain(state.pending.iter()) {
            buffered
                .entry(t.clone())
                .or_default()
                .extend(v.iter().cloned());
        }
        // Registered before `state` is released, so a gc that reads `current` after us also sees
        // us in `live`. Lock order is state, then live; gc never holds live while taking state.
        {
            let mut live = self.shared.live.lock().unwrap();
            live.retain(|w| w.strong_count() > 0);
            live.push(Arc::downgrade(&current));
        }
        drop(state);
        View {
            snapshot: current,
            buffered,
        }
    }

    pub fn close(mut self) -> Result<(), Error> {
        self.close_internal()
    }

    fn close_internal(&mut self) -> Result<(), Error> {
        if self.flusher.is_none() {
            return Ok(());
        }
        let flush_result = self.flush();
        self.shared.state.lock().unwrap().closing = true;
        self.shared.wake.notify_all();
        if let Some(handle) = self.flusher.take() {
            let _ = handle.join();
        }
        flush_result.map(|_| ())
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.close_internal();
    }
}

/// The first field where `batch` and `schema` differ, or a length mismatch.
fn describe_mismatch(batch_fields: &[Field], schema: &[Field]) -> String {
    if batch_fields.len() != schema.len() {
        return format!(
            "expected {} fields, got {}",
            schema.len(),
            batch_fields.len()
        );
    }
    for (i, (b, s)) in batch_fields.iter().zip(schema).enumerate() {
        if b != s {
            return format!("field {i}: expected {s:?}, got {b:?}");
        }
    }
    "fields do not match".to_string()
}

/// A read-only, lock-free handle: re-reads the manifest on every `snapshot()`.
pub struct Reader {
    root: PathBuf,
    publisher: Publisher,
}

impl Reader {
    pub fn open(dir: impl AsRef<Path>) -> Result<Reader, Error> {
        let root = dir.as_ref().to_path_buf();
        let publisher = Publisher::new(root.clone(), Io::real(), 0);
        Ok(Reader { root, publisher })
    }

    /// A `Reader`'s view never registers as a live snapshot: D0009's reader grace covers it
    /// instead, so this never blocks `gc` from ever deleting anything.
    pub fn snapshot(&self) -> Result<View, Error> {
        let manifest = self.publisher.load()?;
        Ok(View {
            snapshot: Arc::new(Snapshot::new(self.root.clone(), manifest)),
            buffered: BTreeMap::new(),
        })
    }
}

/// A manifest snapshot plus, for the writer's own views, the rows still in the buffer.
#[derive(Clone)]
pub struct View {
    snapshot: Arc<Snapshot>,
    buffered: BTreeMap<TableName, Vec<Arc<Batch>>>,
}

impl View {
    pub fn snapshot(&self) -> &Arc<Snapshot> {
        &self.snapshot
    }

    pub fn version(&self) -> u64 {
        self.snapshot.version()
    }

    pub fn table(&self, name: &TableName) -> Option<&TableEntry> {
        self.snapshot.manifest().table(name)
    }

    /// Every row of the table: each live segment's row groups, then buffered batches in
    /// arrival order. Tombstones are not applied (E5).
    pub fn scan(&self, name: &TableName) -> Result<Vec<Batch>, Error> {
        let buffered = self.buffered.get(name).map(Vec::as_slice).unwrap_or(&[]);
        read::scan(&self.snapshot, name, buffered)
    }

    pub fn buffered(&self, name: &TableName) -> &[Arc<Batch>] {
        self.buffered.get(name).map(Vec::as_slice).unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Column;
    use crate::types::{DataType, Value};
    use std::sync::Barrier;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("adelie-store-{}-{tag}-{nanos}", std::process::id()))
    }

    fn schema() -> Vec<Field> {
        vec![
            Field {
                name: "batch".to_string(),
                ty: DataType::UInt64,
            },
            Field {
                name: "idx".to_string(),
                ty: DataType::UInt64,
            },
        ]
    }

    fn batch(batch_no: u64, rows: u64) -> Batch {
        let b: Vec<Value> = (0..rows).map(|_| Value::UInt64(batch_no)).collect();
        let idx: Vec<Value> = (0..rows).map(Value::UInt64).collect();
        Batch::new(
            schema(),
            vec![
                Column::from_values(&DataType::UInt64, &b).unwrap(),
                Column::from_values(&DataType::UInt64, &idx).unwrap(),
            ],
        )
        .unwrap()
    }

    fn table() -> TableName {
        TableName::new("d", "t")
    }

    fn open(dir: &Path, opts: StoreOptions) -> Store {
        let store = Store::open(dir, opts).unwrap();
        store.create_table(&table(), "append", schema()).unwrap();
        store
    }

    #[test]
    #[cfg_attr(miri, ignore)] // spawns threads
    fn group_commit_gives_every_writer_the_same_version() {
        let dir = temp_dir("group-commit");
        let opts = StoreOptions {
            flush_interval: Duration::from_millis(300),
            max_rows: 1_000_000,
            ..StoreOptions::default()
        };
        let store = Arc::new(open(&dir, opts));
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for i in 0..8u64 {
            let store = store.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                store.write(&table(), batch(i, 1)).unwrap()
            }));
        }
        let versions: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(versions.windows(2).all(|w| w[0] == w[1]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn wait_until_buffered(store: &Store) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while store.snapshot().buffered(&table()).is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "write never reached the buffer"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem, spawns a thread
    fn an_idle_flush_does_not_force_the_next_write_out_alone() {
        let dir = temp_dir("idle-flush");
        let opts = StoreOptions {
            flush_interval: Duration::from_secs(3600),
            ..StoreOptions::default()
        };
        let store = Arc::new(open(&dir, opts));
        let before = store.flush().unwrap(); // nothing pending: a no-op
        let writer = {
            let store = store.clone();
            std::thread::spawn(move || store.write(&table(), batch(0, 1)).unwrap())
        };
        wait_until_buffered(&store);
        std::thread::sleep(Duration::from_millis(200));
        // A stale `force` from the idle flush would have published this write already.
        assert_eq!(store.snapshot().version(), before);
        assert!(!writer.is_finished());
        store.flush().unwrap();
        assert_eq!(writer.join().unwrap(), before + 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem, spawns a thread
    fn delete_flushes_pending_rows_first_so_the_tombstone_covers_them() {
        let dir = temp_dir("delete-ordering");
        let opts = StoreOptions {
            flush_interval: Duration::from_secs(3600),
            ..StoreOptions::default()
        };
        let store = Arc::new(open(&dir, opts));

        let writer = {
            let store = store.clone();
            std::thread::spawn(move || store.write(&table(), batch(0, 1)).unwrap())
        };
        wait_until_buffered(&store);

        let del_version = store
            .delete(
                &table(),
                vec![Predicate {
                    column: "batch".to_string(),
                    op: crate::manifest::CmpOp::Eq,
                    value: Value::UInt64(0),
                }],
            )
            .unwrap();
        let write_version = writer.join().unwrap();
        assert_eq!(del_version, write_version + 1);

        let view = store.snapshot();
        let seg = view
            .table(&table())
            .unwrap()
            .segments
            .iter()
            .find(|s| s.seq == write_version)
            .unwrap();
        assert_eq!(view.table(&table()).unwrap().tombstones_for(seg).len(), 1);

        // Blocking would wait out the 1h interval, so write on a thread and force the flush.
        let later = {
            let store = store.clone();
            std::thread::spawn(move || store.write(&table(), batch(1, 1)).unwrap())
        };
        wait_until_buffered(&store);
        store.flush().unwrap();
        let later_version = later.join().unwrap();
        let view2 = store.snapshot();
        let later_seg = view2
            .table(&table())
            .unwrap()
            .segments
            .iter()
            .find(|s| s.seq == later_version)
            .unwrap();
        assert!(
            view2
                .table(&table())
                .unwrap()
                .tombstones_for(later_seg)
                .is_empty()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn opening_a_manifest_with_an_unknown_engine_names_it() {
        let dir = temp_dir("unknown-engine-open");
        std::fs::create_dir_all(&dir).unwrap();
        let m = crate::manifest::Commit {
            base: 0,
            edits: vec![Edit::CreateTable {
                name: table(),
                engine: "nope".to_string(),
                schema: schema(),
            }],
        }
        .apply(&crate::manifest::Manifest::empty(), 0)
        .unwrap();
        Publisher::new(dir.clone(), Io::real(), 8)
            .publish(&m)
            .unwrap();

        let err = Store::open(&dir, StoreOptions::default())
            .err()
            .expect("open must fail");
        assert!(
            matches!(&err, Error::Manifest(manifest::Error::UnknownEngine { engine, .. }) if engine == "nope"),
            "{err}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn create_table_with_an_unknown_engine_names_it() {
        let dir = temp_dir("unknown-engine");
        let store = Store::open(&dir, StoreOptions::default()).unwrap();
        let err = store.create_table(&table(), "nope", schema()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nope"), "{msg}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn a_planted_orphan_is_removed_on_reopen() {
        let dir = temp_dir("orphans");
        {
            let store = open(&dir, StoreOptions::default());
            store.write(&table(), batch(0, 1)).unwrap();
        }
        let orphan = dir
            .join("d")
            .join("t")
            .join("_")
            .join("ffffffffffffff00.seg");
        std::fs::write(&orphan, b"junk").unwrap();

        let store = Store::open(&dir, StoreOptions::default()).unwrap();
        assert!(!orphan.exists());
        let view = store.snapshot();
        assert_eq!(view.scan(&table()).unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn a_second_writer_is_locked_but_a_reader_still_opens() {
        let dir = temp_dir("locked");
        let store = open(&dir, StoreOptions::default());
        assert!(matches!(
            Store::open(&dir, StoreOptions::default()),
            Err(Error::Locked { .. })
        ));
        assert!(Reader::open(&dir).is_ok());
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn buffered_rows_are_in_the_writers_view_but_not_a_separate_readers() {
        let dir = temp_dir("buffered");
        let opts = StoreOptions {
            flush_interval: Duration::from_secs(3600),
            ..StoreOptions::default()
        };
        let store = open(&dir, opts);
        store
            .shared
            .state
            .lock()
            .unwrap()
            .pending
            .entry(table())
            .or_default()
            .push(Arc::new(batch(0, 3)));

        assert_eq!(store.snapshot().buffered(&table()).len(), 1);
        let reader_view = Reader::open(&dir).unwrap().snapshot().unwrap();
        assert!(reader_view.buffered(&table()).is_empty());

        // Drop flushes the row it just made pending: a clean close never loses an ack.
        drop(store);
        let reopened = Store::open(&dir, StoreOptions::default()).unwrap();
        let batches = reopened.snapshot().scan(&table()).unwrap();
        assert_eq!(batches.iter().map(Batch::rows).sum::<usize>(), 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn write_with_the_wrong_schema_is_a_schema_mismatch() {
        let dir = temp_dir("schema-mismatch");
        let store = open(&dir, StoreOptions::default());
        let bad_field = Field {
            name: "batch".to_string(),
            ty: DataType::Int64,
        };
        let bad = Batch::new(
            vec![bad_field, schema()[1].clone()],
            vec![
                Column::from_values(&DataType::Int64, &[Value::Int64(1)]).unwrap(),
                Column::from_values(&DataType::UInt64, &[Value::UInt64(1)]).unwrap(),
            ],
        )
        .unwrap();
        let err = store.write(&table(), bad).unwrap_err();
        assert!(matches!(err, Error::SchemaMismatch { .. }));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
