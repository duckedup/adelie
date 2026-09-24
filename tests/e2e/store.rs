//! adelie-1i8 (E4): public-API tests for the store (SPEC §6, §13, D0009): durability through
//! kills, OCC, the lock, the queryable buffer.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use adelie::exec::{Batch, Column, Field};
use adelie::storage::manifest::TableName;
use adelie::storage::{Error, Reader, Store, StoreOptions};
use adelie::types::{DataType, Value};
use adelie_harness::crash::{CrashTarget, Kill, Plan, Row, run};

fn opts() -> StoreOptions {
    StoreOptions {
        flush_interval: Duration::from_millis(5),
        gc_grace: Duration::ZERO,
        compact_min_inputs: 2,
        ..Default::default()
    }
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

fn table() -> TableName {
    TableName::new("main", "rows")
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "adelie-e2e-store-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

fn to_io(e: Error) -> io::Error {
    io::Error::other(e.to_string())
}

/// Builds one `Batch { batch: UInt64, idx: UInt64 }` from a crash-harness row slice.
fn row_batch(rows: &[Row]) -> Batch {
    let batch_col: Vec<Value> = rows.iter().map(|&(b, _)| Value::UInt64(b)).collect();
    let idx_col: Vec<Value> = rows
        .iter()
        .map(|&(_, i)| Value::UInt64(u64::from(i)))
        .collect();
    Batch::new(
        schema(),
        vec![
            Column::from_values(&DataType::UInt64, &batch_col).unwrap(),
            Column::from_values(&DataType::UInt64, &idx_col).unwrap(),
        ],
    )
    .unwrap()
}

/// The reverse of `row_batch`, over every batch a scan returned.
fn flatten(batches: &[Batch]) -> Vec<Row> {
    let mut rows = Vec::new();
    for b in batches {
        let batch_col = b.column_by_name("batch").unwrap();
        let idx_col = b.column_by_name("idx").unwrap();
        for i in 0..b.rows() {
            let Value::UInt64(batch_no) = batch_col.get(i) else {
                unreachable!("batch column is UInt64")
            };
            let Value::UInt64(idx) = idx_col.get(i) else {
                unreachable!("idx column is UInt64")
            };
            rows.push((batch_no, idx as u32));
        }
    }
    rows
}

/// Opens `dir`, creating every table in `tables` only if it is not already there — so a parent
/// reopening after a kill mid-`create_table` does not trip over the "exists" conflict.
fn open_with_tables(dir: &Path, tables: &[TableName]) -> io::Result<Store> {
    let store = Store::open(dir, opts()).map_err(to_io)?;
    let existing = store.snapshot();
    for t in tables {
        if existing.table(t).is_none() {
            store.create_table(t, "append", schema()).map_err(to_io)?;
        }
    }
    Ok(store)
}

// ── StoreTarget: the crash-harness adapter over `main.rows` ────────────────

struct StoreTarget;

impl CrashTarget for StoreTarget {
    type Store = Store;

    fn open(dir: &Path) -> io::Result<Store> {
        open_with_tables(dir, &[table()])
    }

    fn write(store: &mut Store, batch: &[Row]) -> io::Result<()> {
        store.write(&table(), row_batch(batch)).map_err(to_io)?;
        Ok(())
    }

    fn read_all(store: &Store) -> io::Result<Vec<Row>> {
        let view = store.snapshot();
        Ok(flatten(&view.scan(&table()).map_err(to_io)?))
    }
}

// ── CompactTarget: StoreTarget plus a compaction every third batch ─────────

struct CompactTarget;

impl CrashTarget for CompactTarget {
    type Store = Store;

    fn open(dir: &Path) -> io::Result<Store> {
        StoreTarget::open(dir)
    }

    fn write(store: &mut Store, batch: &[Row]) -> io::Result<()> {
        StoreTarget::write(store, batch)
    }

    fn read_all(store: &Store) -> io::Result<Vec<Row>> {
        StoreTarget::read_all(store)
    }

    fn between(store: &mut Store, batch: u64) -> io::Result<()> {
        if batch % 3 == 2 {
            store.compact(&table()).map_err(to_io)?;
        }
        Ok(())
    }
}

// ── PairTarget: two tables written atomically, for the multi-table check ───

struct PairTarget;

fn table_a() -> TableName {
    TableName::new("main", "a")
}

fn table_b() -> TableName {
    TableName::new("main", "b")
}

impl CrashTarget for PairTarget {
    type Store = Store;

    fn open(dir: &Path) -> io::Result<Store> {
        open_with_tables(dir, &[table_a(), table_b()])
    }

    fn write(store: &mut Store, batch: &[Row]) -> io::Result<()> {
        let b = row_batch(batch);
        store
            .write_many(vec![(table_a(), b.clone()), (table_b(), b)])
            .map_err(to_io)?;
        Ok(())
    }

    /// `a`'s rows, but only if `b` holds the identical set: a torn multi-table commit must
    /// show up as a mismatch here, which the harness reports as `Violation::Reopen`.
    fn read_all(store: &Store) -> io::Result<Vec<Row>> {
        let view = store.snapshot();
        let a = flatten(&view.scan(&table_a()).map_err(to_io)?);
        let b = flatten(&view.scan(&table_b()).map_err(to_io)?);
        if a == b {
            Ok(a)
        } else {
            Err(io::Error::other("main.a and main.b diverged"))
        }
    }
}

// ── Criteria 1/2: every kill policy survives ────────────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn crash_at_ack() {
    let plan = Plan {
        runs: 8,
        batches: 12,
        rows_per_batch: 16,
        seed: 1,
        kill: Kill::AtAck,
    };
    run::<StoreTarget>(concat!(module_path!(), "::crash_at_ack"), &plan)
        .expect("the store must survive a kill at any ack");
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn crash_random() {
    let plan = Plan {
        runs: 8,
        batches: 12,
        rows_per_batch: 16,
        seed: 2,
        kill: Kill::Random { max_delay_ms: 40 },
    };
    run::<StoreTarget>(concat!(module_path!(), "::crash_random"), &plan)
        .expect("the store must survive a random kill");
}

/// Runs `plan` under `Kill::Failpoint(name)` and asserts it actually fired: without this, a
/// misspelled or never-hit failpoint name would pass vacuously.
fn assert_failpoint<T: CrashTarget>(test_path: &str, name: &'static str, seed: u64) {
    let plan = Plan {
        runs: 8,
        batches: 12,
        rows_per_batch: 16,
        seed,
        kill: Kill::Failpoint(name),
    };
    let summary = run::<T>(test_path, &plan).expect("the store must survive this kill");
    assert!(summary.killed > 0, "{name} was never hit in 8 runs");
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn crash_flush_pre_segment_sync() {
    assert_failpoint::<StoreTarget>(
        concat!(module_path!(), "::crash_flush_pre_segment_sync"),
        "flush.pre_segment_sync",
        3,
    );
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn crash_flush_pre_publish() {
    assert_failpoint::<StoreTarget>(
        concat!(module_path!(), "::crash_flush_pre_publish"),
        "flush.pre_publish",
        4,
    );
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn crash_manifest_pre_rename() {
    assert_failpoint::<StoreTarget>(
        concat!(module_path!(), "::crash_manifest_pre_rename"),
        "manifest.pre_rename",
        5,
    );
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn crash_manifest_pre_dir_sync() {
    assert_failpoint::<StoreTarget>(
        concat!(module_path!(), "::crash_manifest_pre_dir_sync"),
        "manifest.pre_dir_sync",
        6,
    );
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn crash_flush_pre_ack() {
    assert_failpoint::<StoreTarget>(
        concat!(module_path!(), "::crash_flush_pre_ack"),
        "flush.pre_ack",
        7,
    );
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn crash_compact_pre_publish() {
    assert_failpoint::<CompactTarget>(
        concat!(module_path!(), "::crash_compact_pre_publish"),
        "compact.pre_publish",
        8,
    );
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn crash_compact_pre_gc() {
    assert_failpoint::<CompactTarget>(
        concat!(module_path!(), "::crash_compact_pre_gc"),
        "compact.pre_gc",
        9,
    );
}

// ── Criterion 4: a multi-table commit is all or nothing ────────────────────

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn multi_table_is_atomic() {
    let plan = Plan {
        runs: 8,
        batches: 12,
        rows_per_batch: 16,
        seed: 10,
        kill: Kill::Failpoint("manifest.pre_rename"),
    };
    let summary = run::<PairTarget>(concat!(module_path!(), "::multi_table_is_atomic"), &plan)
        .expect("a multi-table commit must be atomic across this kill");
    assert!(
        summary.killed > 0,
        "manifest.pre_rename was never hit in 8 runs"
    );
}

// ── Criterion 5: one writer per store; a reader is always welcome ──────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn second_writer_is_locked() {
    let dir = temp_dir("second-writer");
    let store = Store::open(&dir, StoreOptions::default()).unwrap();
    store.create_table(&table(), "append", schema()).unwrap();

    assert!(matches!(
        Store::open(&dir, StoreOptions::default()),
        Err(Error::Locked { .. })
    ));

    let reader = Reader::open(&dir).unwrap();
    reader.snapshot().unwrap();

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── Criterion 6: the buffer is queryable before it is durable ──────────────

#[test]
#[cfg_attr(miri, ignore)] // spawns a thread, touches the real filesystem
fn buffer_is_queryable_but_not_durable() {
    let dir = temp_dir("buffered");
    let store = Store::open(
        &dir,
        StoreOptions {
            flush_interval: Duration::from_secs(3600),
            ..StoreOptions::default()
        },
    )
    .unwrap();
    store.create_table(&table(), "append", schema()).unwrap();
    let store = Arc::new(store);

    let rows: Vec<Row> = (0..16u32).map(|i| (0u64, i)).collect();
    let writer = {
        let store = store.clone();
        let rows = rows.clone();
        std::thread::spawn(move || store.write(&table(), row_batch(&rows)).unwrap())
    };

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let seen = store
            .snapshot()
            .scan(&table())
            .unwrap()
            .iter()
            .map(Batch::rows)
            .sum::<usize>();
        if seen == 16 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "buffered rows never became visible"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let durable_rows: usize = Reader::open(&dir)
        .unwrap()
        .snapshot()
        .unwrap()
        .scan(&table())
        .unwrap()
        .iter()
        .map(Batch::rows)
        .sum();
    assert_eq!(durable_rows, 0, "an unflushed write must not be durable");

    let flushed_version = store.flush().unwrap();
    let write_version = writer.join().unwrap();
    assert!(flushed_version >= write_version);

    let reader_view = Reader::open(&dir).unwrap().snapshot().unwrap();
    assert!(reader_view.version() >= write_version);
    let rows_after: usize = reader_view
        .scan(&table())
        .unwrap()
        .iter()
        .map(Batch::rows)
        .sum();
    assert_eq!(rows_after, 16);

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── Criterion 6 (durability side): a clean reopen sees every write, in order ─

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn durable_round_trip() {
    let dir = temp_dir("round-trip");
    {
        let store = Store::open(&dir, opts()).unwrap();
        store.create_table(&table(), "append", schema()).unwrap();
        for b in 0..5u64 {
            let rows: Vec<Row> = (0..16u32).map(|i| (b, i)).collect();
            store.write(&table(), row_batch(&rows)).unwrap();
        }
        store.close().unwrap();
    }

    let store = Store::open(&dir, opts()).unwrap();
    let got = flatten(&store.snapshot().scan(&table()).unwrap());
    let want: Vec<Row> = (0..5u64)
        .flat_map(|b| (0..16u32).map(move |i| (b, i)))
        .collect();
    assert_eq!(got, want);

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── Criteria 3/8 (e2e half): compaction never breaks a live reader ─────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn compaction_is_safe_for_live_readers() {
    let dir = temp_dir("compaction-safe");
    let store = Store::open(
        &dir,
        StoreOptions {
            gc_grace: Duration::ZERO,
            compact_min_inputs: 2,
            ..StoreOptions::default()
        },
    )
    .unwrap();
    store.create_table(&table(), "append", schema()).unwrap();

    for b in 0..4u64 {
        let rows: Vec<Row> = (0..4u32).map(|i| (b, i)).collect();
        store.write(&table(), row_batch(&rows)).unwrap();
    }
    let old = store.snapshot();

    assert!(store.compact(&table()).unwrap().is_some());
    let rows: Vec<Row> = (0..4u32).map(|i| (4u64, i)).collect();
    store.write(&table(), row_batch(&rows)).unwrap();
    store.gc().unwrap();

    let mut old_rows = flatten(&old.scan(&table()).unwrap());
    old_rows.sort();
    let mut want_old: Vec<Row> = (0..4u64)
        .flat_map(|b| (0..4u32).map(move |i| (b, i)))
        .collect();
    want_old.sort();
    assert_eq!(
        old_rows, want_old,
        "a live Snapshot must still see its rows after compaction"
    );

    let mut all_rows = flatten(&store.snapshot().scan(&table()).unwrap());
    all_rows.sort();
    let mut want_all: Vec<Row> = (0..5u64)
        .flat_map(|b| (0..4u32).map(move |i| (b, i)))
        .collect();
    want_all.sort();
    assert_eq!(
        all_rows, want_all,
        "every batch's rows must appear exactly once"
    );

    drop(old);
    assert!(
        store.gc().unwrap() >= 1,
        "dropping the live snapshot must free the compacted files"
    );

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}
