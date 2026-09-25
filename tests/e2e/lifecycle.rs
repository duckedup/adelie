//! adelie-2hh.3: public-API e2e + crash suite for BACKUP TO, AT VERSION and the
//! versioned-migration ledger (SPEC §19, D0014).

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::convert::Infallible;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use adelie::exec::{Batch, Column, Field};
use adelie::storage::manifest::{Manifest, TableName};
use adelie::storage::{
    Alter, Error, MigrateMode, PendingMigration, Reader, Store, StoreOptions, TableSpec,
};
use adelie::types::{DataType, Value};
use adelie_harness::crash::{CrashTarget, Kill, Plan, Row, run};

/// Small `retain_manifests` (unlike `migrate.rs`/`store.rs`, which zero it to keep their own
/// gc assertions instant), so the AT VERSION window below is actually exercised.
fn opts() -> StoreOptions {
    StoreOptions {
        flush_interval: Duration::from_millis(5),
        gc_grace: Duration::ZERO,
        compact_min_inputs: 2,
        retain_definitions: Duration::from_secs(3600),
        retain_manifests: 4,
        job_step_segments: 1,
        job_swap_gap: 0,
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
    TableName::new("main", "t")
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "adelie-e2e-lifecycle-{tag}-{}-{nanos}",
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

fn open_table(dir: &Path) -> Store {
    let store = Store::open(dir, opts()).unwrap();
    store
        .create_table(TableSpec::new(table(), schema()))
        .unwrap();
    store
}

/// Writes 3 batches of 4 rows each, each `write` blocking for its own flush, so they land in 3
/// separate segments and 3 separate commits.
fn write_three_batches(store: &Store) -> Vec<u64> {
    for b in 0..3u64 {
        let rows: Vec<Row> = (0..4u32).map(|i| (b, i)).collect();
        store.write(&table(), row_batch(&rows)).unwrap();
    }
    let mut ids: Vec<u64> = store
        .table(&table())
        .unwrap()
        .segments
        .iter()
        .map(|s| s.id)
        .collect();
    ids.sort_unstable();
    ids
}

// ── 1: a retained version reads after compaction and gc ────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn a_retained_version_reads_after_compaction_and_gc() {
    let dir = temp_dir("at-version-retained");
    let store = open_table(&dir);
    write_three_batches(&store);
    let v = store.snapshot().version();
    let want = flatten(&store.snapshot().scan(&table()).unwrap());

    let live = store.table(&table()).unwrap();
    let seg_paths: Vec<PathBuf> = live
        .segments
        .iter()
        .map(|s| Manifest::segment_path(&dir, s))
        .collect();

    store.write(&table(), row_batch(&[(9, 0)])).unwrap();
    assert!(store.compact(&table()).unwrap().is_some());
    store.gc().unwrap();

    assert!(
        seg_paths.iter().all(|p| p.exists()),
        "a version still inside the retain window must keep its segments; fails on main \
         because gc deletes them unconditionally"
    );

    let got = {
        let view = store.view_at(v).unwrap();
        flatten(&view.scan(&table()).unwrap())
    };
    assert_eq!(got, want);

    // Reader::view_at never registers live, so a read through it, after every Store view above
    // has been dropped, proves the protection comes from the retain window, not `live`.
    let reader_got = {
        let reader = Reader::open(&dir).unwrap();
        let view = reader.view_at(v).unwrap();
        flatten(&view.scan(&table()).unwrap())
    };
    assert_eq!(reader_got, want);

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 2: out of the window, view_at errors and the files go ──────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn out_of_the_retain_window_view_at_errors_and_the_files_go() {
    let dir = temp_dir("at-version-expired");
    let store = open_table(&dir);
    write_three_batches(&store);
    let v = store.snapshot().version();

    let live = store.table(&table()).unwrap();
    let seg_paths: Vec<PathBuf> = live
        .segments
        .iter()
        .map(|s| Manifest::segment_path(&dir, s))
        .collect();

    store.write(&table(), row_batch(&[(9, 0)])).unwrap();
    assert!(store.compact(&table()).unwrap().is_some());
    store.gc().unwrap();

    // Push well past the window: one commit per write, plenty more than `retain_manifests`.
    let retain = opts().retain_manifests as u64;
    for i in 0..(retain + 6) {
        store.write(&table(), row_batch(&[(10 + i, 0)])).unwrap();
    }

    assert!(matches!(
        store.view_at(v),
        Err(Error::VersionNotRetained { version, .. }) if version == v
    ));
    let reader = Reader::open(&dir).unwrap();
    assert!(matches!(
        reader.view_at(v),
        Err(Error::VersionNotRetained { version, .. }) if version == v
    ));

    store.gc().unwrap();
    assert!(
        seg_paths.iter().all(|p| !p.exists()),
        "once out of the window, gc must finally free the compacted-away segments; fails if \
         retention protected them forever"
    );

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 3: the future errors, the present matches snapshot() ────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn view_at_future_errors_and_view_at_current_matches_snapshot() {
    let dir = temp_dir("at-version-bounds");
    let store = open_table(&dir);
    write_three_batches(&store);
    let current = store.snapshot().version();

    assert!(matches!(
        store.view_at(current + 1),
        Err(Error::VersionNotRetained { version, current: c })
            if version == current + 1 && c == current
    ));

    let want = flatten(&store.snapshot().scan(&table()).unwrap());
    let got = flatten(&store.view_at(current).unwrap().scan(&table()).unwrap());
    assert_eq!(got, want);

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 4: BACKUP round trip ─────────────────────────────────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn backup_round_trip_reads_back_and_accepts_writes() {
    let dir = temp_dir("backup-round-trip");
    let bdir = temp_dir("backup-round-trip-dst");
    let store = open_table(&dir);
    write_three_batches(&store);
    let want = flatten(&store.snapshot().scan(&table()).unwrap());

    let version = store.backup_to(&bdir).unwrap();

    let reader_view = Reader::open(&bdir).unwrap().snapshot().unwrap();
    assert_eq!(flatten(&reader_view.scan(&table()).unwrap()), want);
    assert_eq!(reader_view.version(), version);

    store.close().unwrap();

    let backup_store = Store::open(&bdir, opts()).unwrap();
    assert_eq!(
        flatten(&backup_store.snapshot().scan(&table()).unwrap()),
        want
    );

    backup_store.write(&table(), row_batch(&[(9, 0)])).unwrap();
    let mut got = flatten(&backup_store.snapshot().scan(&table()).unwrap());
    got.sort_unstable();
    let mut expected = want;
    expected.push((9, 0));
    expected.sort_unstable();
    assert_eq!(got, expected);

    drop(backup_store);
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&bdir).unwrap();
}

// ── 5: a backup is independent once the source deletes ──────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn backup_is_independent_once_the_source_deletes() {
    let dir = temp_dir("backup-independent-src");
    let bdir = temp_dir("backup-independent-dst");
    let store = Store::open(
        &dir,
        StoreOptions {
            retain_manifests: 0,
            ..opts()
        },
    )
    .unwrap();
    store
        .create_table(TableSpec::new(table(), schema()))
        .unwrap();
    write_three_batches(&store);
    let want = flatten(&store.snapshot().scan(&table()).unwrap());

    let live = store.table(&table()).unwrap();
    let seg_paths: Vec<PathBuf> = live
        .segments
        .iter()
        .map(|s| Manifest::segment_path(&dir, s))
        .collect();

    store.backup_to(&bdir).unwrap();

    store.write(&table(), row_batch(&[(9, 0)])).unwrap();
    assert!(store.compact(&table()).unwrap().is_some());
    store.gc().unwrap();
    assert!(
        seg_paths.iter().all(|p| !p.exists()),
        "the source, with retain_manifests 0, must really delete its old segments"
    );

    let backup_view = Reader::open(&bdir).unwrap().snapshot().unwrap();
    assert_eq!(flatten(&backup_view.scan(&table()).unwrap()), want);

    let backup_seg_paths: Vec<PathBuf> = backup_view
        .table(&table())
        .unwrap()
        .segments
        .iter()
        .map(|s| Manifest::segment_path(&bdir, s))
        .collect();
    assert!(
        backup_seg_paths.iter().all(|p| p.exists()),
        "this fails if backup copied references instead of linking"
    );

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&bdir).unwrap();
}

// ── 6: retired entries travel; no garbage, no jobs ──────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn backup_carries_retired_tables_with_no_garbage_or_jobs() {
    let dir = temp_dir("backup-retired");
    let bdir = temp_dir("backup-retired-dst");
    let store = open_table(&dir);
    write_three_batches(&store);

    let second = TableName::new("main", "t2");
    store
        .create_table(TableSpec::new(second.clone(), schema()))
        .unwrap();
    store.write(&second, row_batch(&[(0, 0), (0, 1)])).unwrap();
    let want_second = flatten(&store.snapshot().scan(&second).unwrap());
    store.drop_table(&second).unwrap();

    // Compaction's replaced segments stay in the source's garbage list (inside the retain
    // window), so the assertion below has something to clear.
    assert!(store.compact(&table()).unwrap().is_some());
    assert!(
        !store.snapshot().snapshot().manifest().garbage.is_empty(),
        "the source must hold garbage, or the no-garbage assertion proves nothing"
    );

    // Left running (never stepped): backup_to must clear jobs unconditionally.
    store
        .alter(
            &table(),
            vec![Alter::AddColumn(Field {
                name: "extra".to_string(),
                ty: DataType::String,
            })],
        )
        .unwrap();

    store.backup_to(&bdir).unwrap();

    let backup_store = Store::open(&bdir, opts()).unwrap();
    assert!(
        backup_store.jobs().is_empty(),
        "a job must not travel into the backup"
    );
    assert!(
        backup_store
            .snapshot()
            .snapshot()
            .manifest()
            .garbage
            .is_empty(),
        "the backup's manifest must carry no garbage"
    );

    backup_store.undrop_table(&second).unwrap();
    assert_eq!(
        flatten(&backup_store.snapshot().scan(&second).unwrap()),
        want_second
    );

    drop(backup_store);
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&bdir).unwrap();
}

// ── 7: a non-empty target is refused before any link ────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn backup_to_a_non_empty_target_is_refused_before_any_link() {
    let dir = temp_dir("backup-refused-src");
    let bdir = temp_dir("backup-refused-dst");
    let store = open_table(&dir);
    write_three_batches(&store);

    std::fs::create_dir_all(&bdir).unwrap();
    std::fs::write(bdir.join("junk"), b"x").unwrap();

    let err = store.backup_to(&bdir).unwrap_err();
    assert!(matches!(err, Error::Usage(_)), "{err}");
    assert_eq!(
        std::fs::read_dir(&bdir).unwrap().count(),
        1,
        "nothing must be linked into a rejected target"
    );

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&bdir).unwrap();
}

// ── 8: migrations apply in number order; a rerun is idempotent ──────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn migrations_apply_in_number_order_and_a_rerun_is_idempotent() {
    let dir = temp_dir("ledger-order");
    let mig_dir = temp_dir("ledger-order-migrations");
    std::fs::create_dir_all(&mig_dir).unwrap();
    std::fs::write(mig_dir.join("1_a.sql"), b"-- a").unwrap();
    std::fs::write(mig_dir.join("2_b.sql"), b"-- b").unwrap();
    std::fs::write(mig_dir.join("10_c.sql"), b"-- c").unwrap();
    std::fs::write(mig_dir.join("README.md"), b"ignore me").unwrap();

    let store = Store::open(&dir, opts()).unwrap();
    let order: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let applied = store
        .apply_migrations(&mig_dir, MigrateMode::Apply, |p: &PendingMigration| {
            order.borrow_mut().push(p.name.clone());
            Ok::<(), Infallible>(())
        })
        .unwrap();
    assert_eq!(
        order
            .borrow()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["1_a.sql", "2_b.sql", "10_c.sql"]
    );
    assert_eq!(
        applied.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        vec!["1_a.sql", "2_b.sql", "10_c.sql"]
    );
    let recorded: Vec<String> = store.migrations().iter().map(|m| m.name.clone()).collect();
    assert_eq!(
        recorded.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["1_a.sql", "2_b.sql", "10_c.sql"]
    );

    order.borrow_mut().clear();
    let applied2 = store
        .apply_migrations(&mig_dir, MigrateMode::Apply, |p: &PendingMigration| {
            order.borrow_mut().push(p.name.clone());
            Ok::<(), Infallible>(())
        })
        .unwrap();
    assert!(applied2.is_empty(), "a rerun must apply nothing new");
    assert!(
        order.borrow().is_empty(),
        "a rerun must call exec zero times"
    );

    store.close().unwrap();
    let store = Store::open(&dir, opts()).unwrap();
    let recorded_after_reopen: Vec<String> =
        store.migrations().iter().map(|m| m.name.clone()).collect();
    assert_eq!(
        recorded_after_reopen
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["1_a.sql", "2_b.sql", "10_c.sql"]
    );

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&mig_dir).unwrap();
}

// ── 9: a dry run executes nothing and is deterministic ──────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn dry_run_executes_nothing_and_is_deterministic() {
    let dir = temp_dir("ledger-dry-run");
    let mig_dir = temp_dir("ledger-dry-run-migrations");
    std::fs::create_dir_all(&mig_dir).unwrap();
    let files: [(&str, &str); 3] = [
        ("1_a.sql", "-- a"),
        ("2_b.sql", "-- b"),
        ("10_c.sql", "-- c"),
    ];
    for (name, sql) in files {
        std::fs::write(mig_dir.join(name), sql).unwrap();
    }

    let store = Store::open(&dir, opts()).unwrap();
    let calls = Cell::new(0u32);
    let pending = store
        .apply_migrations(&mig_dir, MigrateMode::DryRun, |_p: &PendingMigration| {
            calls.set(calls.get() + 1);
            Ok::<(), Infallible>(())
        })
        .unwrap();
    assert_eq!(calls.get(), 0, "DryRun must execute nothing");
    assert_eq!(
        pending.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        vec!["1_a.sql", "2_b.sql", "10_c.sql"]
    );
    for (i, p) in pending.iter().enumerate() {
        assert_eq!(p.sql, files[i].1, "{}'s sql must match the file", p.name);
    }
    assert!(store.migrations().is_empty(), "DryRun must record nothing");

    // A second DryRun must derive the identical checksums from the same bytes.
    let pending2 = store
        .apply_migrations(&mig_dir, MigrateMode::DryRun, |_p: &PendingMigration| {
            Ok::<(), Infallible>(())
        })
        .unwrap();
    assert_eq!(
        pending, pending2,
        "checksum must be a pure function of the file's bytes"
    );

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&mig_dir).unwrap();
}

// ── 10: changed / missing / below — every case, zero exec calls ─────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn changed_missing_and_below_are_rejected_with_no_exec_calls() {
    let dir = temp_dir("ledger-checks");
    let mig_dir = temp_dir("ledger-checks-migrations");
    std::fs::create_dir_all(&mig_dir).unwrap();
    std::fs::write(mig_dir.join("1_a.sql"), b"-- a").unwrap();
    std::fs::write(mig_dir.join("2_b.sql"), b"-- b").unwrap();
    std::fs::write(mig_dir.join("10_c.sql"), b"-- c").unwrap();

    let store = Store::open(&dir, opts()).unwrap();
    let calls = Cell::new(0u32);
    store
        .apply_migrations(&mig_dir, MigrateMode::Apply, |_p: &PendingMigration| {
            calls.set(calls.get() + 1);
            Ok::<(), Infallible>(())
        })
        .unwrap();
    assert_eq!(calls.get(), 3);

    // Changed: even with a new, higher-numbered file present.
    calls.set(0);
    std::fs::write(mig_dir.join("2_b.sql"), b"-- b changed").unwrap();
    std::fs::write(mig_dir.join("11_d.sql"), b"-- d").unwrap();
    let err = store
        .apply_migrations(&mig_dir, MigrateMode::Apply, |_p: &PendingMigration| {
            calls.set(calls.get() + 1);
            Ok::<(), Infallible>(())
        })
        .unwrap_err();
    assert!(
        matches!(err, Error::MigrationChanged { ref name } if name == "2_b.sql"),
        "{err}"
    );
    assert_eq!(calls.get(), 0);

    // Missing.
    std::fs::write(mig_dir.join("2_b.sql"), b"-- b").unwrap();
    std::fs::remove_file(mig_dir.join("1_a.sql")).unwrap();
    calls.set(0);
    let err = store
        .apply_migrations(&mig_dir, MigrateMode::Apply, |_p: &PendingMigration| {
            calls.set(calls.get() + 1);
            Ok::<(), Infallible>(())
        })
        .unwrap_err();
    assert!(
        matches!(err, Error::MigrationMissing { ref name } if name == "1_a.sql"),
        "{err}"
    );
    assert_eq!(calls.get(), 0);

    // Below: a new file numbered at or below the highest applied.
    std::fs::write(mig_dir.join("1_a.sql"), b"-- a").unwrap();
    std::fs::write(mig_dir.join("5_x.sql"), b"-- x").unwrap();
    calls.set(0);
    let err = store
        .apply_migrations(&mig_dir, MigrateMode::Apply, |_p: &PendingMigration| {
            calls.set(calls.get() + 1);
            Ok::<(), Infallible>(())
        })
        .unwrap_err();
    assert!(
        matches!(err, Error::MigrationInvalid { ref name, .. } if name == "5_x.sql"),
        "{err}"
    );
    assert_eq!(calls.get(), 0);

    // A bad file name is invalid too.
    std::fs::remove_file(mig_dir.join("5_x.sql")).unwrap();
    std::fs::write(mig_dir.join("x_bad.sql"), b"-- bad").unwrap();
    calls.set(0);
    let err = store
        .apply_migrations(&mig_dir, MigrateMode::Apply, |_p: &PendingMigration| {
            calls.set(calls.get() + 1);
            Ok::<(), Infallible>(())
        })
        .unwrap_err();
    assert!(
        matches!(err, Error::MigrationInvalid { ref name, .. } if name == "x_bad.sql"),
        "{err}"
    );
    assert_eq!(calls.get(), 0);

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&mig_dir).unwrap();
}

// ── 11: a failing exec stops and records nothing for that file ──────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn a_failing_exec_stops_and_records_nothing_for_that_file() {
    let dir = temp_dir("ledger-fail");
    let mig_dir = temp_dir("ledger-fail-migrations");
    std::fs::create_dir_all(&mig_dir).unwrap();
    std::fs::write(mig_dir.join("1_a.sql"), b"-- a").unwrap();
    std::fs::write(mig_dir.join("2_b.sql"), b"-- b").unwrap();
    std::fs::write(mig_dir.join("10_c.sql"), b"-- c").unwrap();

    let store = Store::open(&dir, opts()).unwrap();
    let err = store
        .apply_migrations(&mig_dir, MigrateMode::Apply, |p: &PendingMigration| {
            if p.name == "2_b.sql" {
                Err("boom".to_string())
            } else {
                Ok(())
            }
        })
        .unwrap_err();
    assert!(
        matches!(err, Error::MigrationFailed { ref name, .. } if name == "2_b.sql"),
        "{err}"
    );

    let recorded: Vec<String> = store.migrations().iter().map(|m| m.name.clone()).collect();
    assert_eq!(
        recorded.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["1_a.sql"]
    );

    let order: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let applied = store
        .apply_migrations(&mig_dir, MigrateMode::Apply, |p: &PendingMigration| {
            order.borrow_mut().push(p.name.clone());
            Ok::<(), Infallible>(())
        })
        .unwrap();
    assert_eq!(
        order
            .borrow()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["2_b.sql", "10_c.sql"]
    );
    assert_eq!(
        applied.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        vec!["2_b.sql", "10_c.sql"]
    );

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&mig_dir).unwrap();
}

// ── 12: at-least-once across a panic (in-process) ────────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn at_least_once_across_a_panic_reruns_only_the_incomplete_file() {
    let dir = temp_dir("ledger-panic");
    let mig_dir = temp_dir("ledger-panic-migrations");
    std::fs::create_dir_all(&mig_dir).unwrap();
    std::fs::write(mig_dir.join("1_a.sql"), b"-- a").unwrap();
    std::fs::write(mig_dir.join("2_b.sql"), b"-- b").unwrap();
    std::fs::write(mig_dir.join("10_c.sql"), b"-- c").unwrap();

    let store = Store::open(&dir, opts()).unwrap();
    let order: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.apply_migrations(&mig_dir, MigrateMode::Apply, |p: &PendingMigration| {
            order.borrow_mut().push(p.name.clone());
            if p.name == "2_b.sql" {
                panic!("simulated crash after the side effect");
            }
            Ok::<(), Infallible>(())
        })
    }));
    assert!(
        result.is_err(),
        "the panic must propagate through catch_unwind"
    );
    assert_eq!(
        order
            .borrow()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["1_a.sql", "2_b.sql"]
    );

    let recorded: Vec<String> = store.migrations().iter().map(|m| m.name.clone()).collect();
    assert_eq!(
        recorded.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["1_a.sql"],
        "2_b.sql's side effect ran but its record never committed"
    );

    order.borrow_mut().clear();
    let applied = store
        .apply_migrations(&mig_dir, MigrateMode::Apply, |p: &PendingMigration| {
            order.borrow_mut().push(p.name.clone());
            Ok::<(), Infallible>(())
        })
        .unwrap();
    assert_eq!(
        order
            .borrow()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["2_b.sql", "10_c.sql"],
        "2_b.sql ran twice overall, 1_a.sql once"
    );
    assert_eq!(
        applied.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        vec!["2_b.sql", "10_c.sql"]
    );

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&mig_dir).unwrap();
}

// ── Crash suite: LifecycleTarget ─────────────────────────────────────────────

/// A sibling of the store dir, never inside it: `cleanup_orphans` walks the store root
/// recursively, so a migrations dir living there would itself be swept for orphan files.
fn sibling_migrations_dir(store_dir: &Path) -> PathBuf {
    let name = store_dir
        .file_name()
        .expect("the store dir has a file name")
        .to_string_lossy()
        .into_owned();
    store_dir.with_file_name(format!("{name}-migrations"))
}

/// The number before the first `_` in a migration file's stem: enough to check order from
/// outside the crate, without reaching into `ledger.rs`'s own (private) parser.
fn leading_number(name: &str) -> Option<u64> {
    let stem = name.strip_suffix(".sql")?;
    let us = stem.find('_')?;
    stem[..us].parse().ok()
}

/// `main.t`, plus a sibling dir the harness's `between` uses for backups and migration files.
/// `between` needs the store's own directory, which `CrashTarget::between` isn't handed, so the
/// target carries it alongside the `Store` instead of impl'ing `CrashTarget` for `Store` itself.
struct LifecycleStore {
    store: Store,
    dir: PathBuf,
}

struct LifecycleTarget;

impl CrashTarget for LifecycleTarget {
    type Store = LifecycleStore;

    /// Creates `main.t` if missing, then resumes: applies any migration file left over from a
    /// kill mid-`between` (at-least-once), and refuses a corrupt ledger outright.
    fn open(dir: &Path) -> io::Result<LifecycleStore> {
        let store = Store::open(dir, opts()).map_err(to_io)?;
        if store.table(&table()).is_none() {
            store
                .create_table(TableSpec::new(table(), schema()))
                .map_err(to_io)?;
        }

        let mig_dir = sibling_migrations_dir(dir);
        if mig_dir.exists() {
            store
                .apply_migrations(&mig_dir, MigrateMode::Apply, |_p: &PendingMigration| {
                    Ok::<(), Infallible>(())
                })
                .map_err(to_io)?;
        }

        let mut seen = HashSet::new();
        let mut last: Option<u64> = None;
        for r in store.migrations() {
            if !seen.insert(r.name.clone()) {
                return Err(io::Error::other(format!(
                    "duplicate migration record {}",
                    r.name
                )));
            }
            let n = leading_number(&r.name).ok_or_else(|| {
                io::Error::other(format!("unparseable migration name {}", r.name))
            })?;
            if last.is_some_and(|prev| n <= prev) {
                return Err(io::Error::other(format!(
                    "migrations out of order at {}",
                    r.name
                )));
            }
            last = Some(n);
        }

        // Every backup a kill left behind with a `manifest` must read back whole: the
        // manifest is published last, so one that names a missing segment is a real bug. One
        // without a manifest is an interrupted backup, incomplete by design.
        let name = dir
            .file_name()
            .expect("the store dir has a file name")
            .to_string_lossy()
            .into_owned();
        let prefix = format!("{name}-backup-");
        if let Some(parent) = dir.parent() {
            for entry in std::fs::read_dir(parent)?.flatten() {
                let b = entry.path();
                let is_backup = entry.file_name().to_string_lossy().starts_with(&prefix);
                if !is_backup || !b.join("manifest").exists() {
                    continue;
                }
                let view = Reader::open(&b).and_then(|r| r.snapshot()).map_err(to_io)?;
                view.scan(&table()).map_err(|e| {
                    io::Error::other(format!("backup {} does not read back: {e}", b.display()))
                })?;
            }
        }

        Ok(LifecycleStore {
            store,
            dir: dir.to_path_buf(),
        })
    }

    fn write(target: &mut LifecycleStore, batch: &[Row]) -> io::Result<()> {
        target
            .store
            .write(&table(), row_batch(batch))
            .map_err(to_io)?;
        Ok(())
    }

    fn read_all(target: &LifecycleStore) -> io::Result<Vec<Row>> {
        let view = target.store.snapshot();
        Ok(flatten(&view.scan(&table()).map_err(to_io)?))
    }

    /// Even batches: `backup_to` a fresh sibling dir. Odd batches: write and apply one more
    /// migration file. Both live outside the store dir (see `sibling_migrations_dir`).
    fn between(target: &mut LifecycleStore, batch: u64) -> io::Result<()> {
        if batch.is_multiple_of(2) {
            let name = target
                .dir
                .file_name()
                .expect("the store dir has a file name")
                .to_string_lossy()
                .into_owned();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let backup_dir = target
                .dir
                .with_file_name(format!("{name}-backup-{batch}-{nanos}"));
            target.store.backup_to(&backup_dir).map_err(to_io)?;
        } else {
            let mig_dir = sibling_migrations_dir(&target.dir);
            std::fs::create_dir_all(&mig_dir)
                .map_err(|e| io::Error::other(format!("mkdir {}: {e}", mig_dir.display())))?;
            let file = mig_dir.join(format!("{batch:04}_m.sql"));
            std::fs::write(&file, b"-- noop")
                .map_err(|e| io::Error::other(format!("write {}: {e}", file.display())))?;
            target
                .store
                .apply_migrations(&mig_dir, MigrateMode::Apply, |_p: &PendingMigration| {
                    Ok::<(), Infallible>(())
                })
                .map_err(to_io)?;
        }
        Ok(())
    }
}

/// Runs `plan` under `Kill::Failpoint(name)` and asserts it actually fired.
fn assert_failpoint<T: CrashTarget>(test_path: &str, name: &'static str, seed: u64) {
    let plan = Plan {
        runs: 8,
        batches: 12,
        rows_per_batch: 16,
        seed,
        kill: Kill::Failpoint(name),
    };
    let summary = run::<T>(test_path, &plan).expect("the source store must survive this kill");
    assert!(summary.killed > 0, "{name} was never hit in 8 runs");
    cleanup_lifecycle_siblings(test_path, plan.runs);
}

/// The sibling dirs `between` creates outside the crash dir aren't touched by the harness's own
/// per-run cleanup: remove them here, best-effort, by the same naming scheme it uses.
fn cleanup_lifecycle_siblings(test_path: &str, runs: u32) {
    let sanitised: String = test_path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let pid = std::process::id();
    let tmp = std::env::temp_dir();
    for run in 0..runs {
        let base = format!("adelie-crash-{pid}-{sanitised}-{run}");
        let Ok(entries) = std::fs::read_dir(&tmp) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&base) && name != base {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn crash_backup_pre_link() {
    assert_failpoint::<LifecycleTarget>(
        concat!(module_path!(), "::crash_backup_pre_link"),
        "backup.pre_link",
        40,
    );
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn crash_backup_pre_manifest() {
    assert_failpoint::<LifecycleTarget>(
        concat!(module_path!(), "::crash_backup_pre_manifest"),
        "backup.pre_manifest",
        41,
    );
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn crash_ledger_pre_record() {
    assert_failpoint::<LifecycleTarget>(
        concat!(module_path!(), "::crash_ledger_pre_record"),
        "ledger.pre_record",
        42,
    );
}
