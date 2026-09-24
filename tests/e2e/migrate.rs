//! adelie-2hh.2 (unit D): public-API e2e + crash suite for migrations (SPEC §19, D0012):
//! ADD/DROP/RENAME COLUMN, ORDER BY, REVERT, DROP/UNDROP TABLE, TRUNCATE, cross-table GC.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use adelie::exec::{Batch, Column, Field};
use adelie::storage::manifest::{self, CmpOp, FieldId, Manifest, Predicate, TableName};
use adelie::storage::{Alter, Error, JobStatus, Store, StoreOptions, TableSpec};
use adelie::types::{DataType, Value};
use adelie_harness::crash::{CrashTarget, Kill, Plan, Row, run};

fn opts() -> StoreOptions {
    StoreOptions {
        flush_interval: Duration::from_millis(5),
        gc_grace: Duration::ZERO,
        compact_min_inputs: 2,
        retain_definitions: Duration::from_secs(3600),
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

/// `schema()` plus a trailing `extra STRING`, as `AddColumn` always appends it.
fn extra_fields() -> Vec<Field> {
    let mut f = schema();
    f.push(Field {
        name: "extra".to_string(),
        ty: DataType::String,
    });
    f
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
        "adelie-e2e-migrate-{tag}-{}-{nanos}",
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

/// A `(batch, idx, extra)` batch against the post-`AddColumn(extra STRING)` schema; `None`
/// writes a NULL `extra`.
fn extra_batch(rows: &[(u64, u32, Option<&str>)]) -> Batch {
    let batch_col: Vec<Value> = rows.iter().map(|&(b, _, _)| Value::UInt64(b)).collect();
    let idx_col: Vec<Value> = rows
        .iter()
        .map(|&(_, i, _)| Value::UInt64(u64::from(i)))
        .collect();
    let extra_col: Vec<Value> = rows
        .iter()
        .map(|&(_, _, e)| e.map_or(Value::Null, |s| Value::String(s.to_string())))
        .collect();
    Batch::new(
        extra_fields(),
        vec![
            Column::from_values(&DataType::UInt64, &batch_col).unwrap(),
            Column::from_values(&DataType::UInt64, &idx_col).unwrap(),
            Column::from_values(&DataType::String, &extra_col).unwrap(),
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
/// separate segments. Returns their ids, ascending.
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

/// Every `*.seg` file under `dir`, recursively.
fn count_seg_files(dir: &Path) -> usize {
    fn walk(dir: &Path, count: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, count);
            } else if path.extension().is_some_and(|e| e == "seg") {
                *count += 1;
            }
        }
    }
    let mut count = 0;
    walk(dir, &mut count);
    count
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/store-0.5.0")
}

/// Recursive, std-only. Opening a store mutates it, so every test works on a throwaway copy.
fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}

// ── 1: ADD reuses every segment ─────────────────────────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn add_column_reuses_every_segment() {
    let dir = temp_dir("add-reuses");
    let store = open_table(&dir);
    let before_ids = write_three_batches(&store);

    let add = Alter::AddColumn(Field {
        name: "extra".to_string(),
        ty: DataType::String,
    });
    let plan = store
        .explain_alter(&table(), std::slice::from_ref(&add))
        .unwrap();
    assert_eq!(plan.rewrites, 0);
    assert_eq!(plan.segments, 3);

    store.migrate(&table(), vec![add]).unwrap();

    let mut after_ids: Vec<u64> = store
        .table(&table())
        .unwrap()
        .segments
        .iter()
        .map(|s| s.id)
        .collect();
    after_ids.sort_unstable();
    assert_eq!(after_ids, before_ids, "no segment must be rewritten");

    let scanned = store.snapshot().scan(&table()).unwrap();
    let mut old_rows = 0;
    for b in &scanned {
        let extra = b.column_by_name("extra").unwrap();
        for i in 0..b.rows() {
            assert!(extra.is_null(i), "an old row's extra must read NULL");
            old_rows += 1;
        }
    }
    assert_eq!(old_rows, 12);

    store
        .write(&table(), extra_batch(&[(9, 0, Some("x"))]))
        .unwrap();
    let scanned = store.snapshot().scan(&table()).unwrap();
    let found = scanned.iter().any(|b| {
        let extra = b.column_by_name("extra").unwrap();
        (0..b.rows()).any(|i| !extra.is_null(i) && extra.get(i) == Value::String("x".to_string()))
    });
    assert!(found, "a write with extra set must read back");

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 2: DROP then re-ADD the same name is a new id ───────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn drop_then_readd_same_name_reads_null() {
    let dir = temp_dir("drop-readd");
    let store = open_table(&dir);
    store
        .migrate(
            &table(),
            vec![Alter::AddColumn(Field {
                name: "extra".to_string(),
                ty: DataType::String,
            })],
        )
        .unwrap();
    store
        .write(&table(), extra_batch(&[(0, 0, Some("v"))]))
        .unwrap();

    store
        .migrate(&table(), vec![Alter::DropColumn("extra".to_string())])
        .unwrap();
    store
        .migrate(
            &table(),
            vec![Alter::AddColumn(Field {
                name: "extra".to_string(),
                ty: DataType::String,
            })],
        )
        .unwrap();

    let scanned = store.snapshot().scan(&table()).unwrap();
    for b in &scanned {
        let extra = b.column_by_name("extra").unwrap();
        for i in 0..b.rows() {
            assert!(extra.is_null(i), "a re-added column must not match by name");
        }
    }

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 3: RENAME keeps segments and data ───────────────────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn rename_keeps_segments_and_data() {
    let dir = temp_dir("rename");
    let store = open_table(&dir);
    let before_ids = write_three_batches(&store);

    let rename = Alter::RenameColumn {
        from: "idx".to_string(),
        to: "i".to_string(),
    };
    let plan = store
        .explain_alter(&table(), std::slice::from_ref(&rename))
        .unwrap();
    assert_eq!(plan.rewrites, 0);

    store.migrate(&table(), vec![rename]).unwrap();

    let mut after_ids: Vec<u64> = store
        .table(&table())
        .unwrap()
        .segments
        .iter()
        .map(|s| s.id)
        .collect();
    after_ids.sort_unstable();
    assert_eq!(after_ids, before_ids);

    let scanned = store.snapshot().scan(&table()).unwrap();
    let mut got: Vec<(u64, u64)> = Vec::new();
    for b in &scanned {
        let batch_col = b.column_by_name("batch").unwrap();
        let i_col = b.column_by_name("i").unwrap();
        for i in 0..b.rows() {
            let Value::UInt64(bn) = batch_col.get(i) else {
                unreachable!()
            };
            let Value::UInt64(iv) = i_col.get(i) else {
                unreachable!()
            };
            got.push((bn, iv));
        }
    }
    got.sort_unstable();
    let mut want: Vec<(u64, u64)> = (0..3u64)
        .flat_map(|b| (0..4u64).map(move |i| (b, i)))
        .collect();
    want.sort_unstable();
    assert_eq!(got, want);

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 4: ORDER BY rewrites every segment ──────────────────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn order_by_rewrites_every_segment() {
    let dir = temp_dir("order-by-rewrite");
    let store = open_table(&dir);
    for b in 0..3u64 {
        // Descending idx: only a real rewrite could leave each segment sorted.
        let rows: Vec<Row> = (0..4u32).rev().map(|i| (b, i)).collect();
        store.write(&table(), row_batch(&rows)).unwrap();
    }
    let mut before_ids: Vec<u64> = store
        .table(&table())
        .unwrap()
        .segments
        .iter()
        .map(|s| s.id)
        .collect();
    before_ids.sort_unstable();

    let order_by = Alter::OrderBy(vec!["idx".to_string(), "batch".to_string()]);
    let plan = store
        .explain_alter(&table(), std::slice::from_ref(&order_by))
        .unwrap();
    assert_eq!(plan.rewrites, 3);

    store.migrate(&table(), vec![order_by]).unwrap();

    let after_ids: Vec<u64> = store
        .table(&table())
        .unwrap()
        .segments
        .iter()
        .map(|s| s.id)
        .collect();
    assert!(
        after_ids.iter().all(|id| !before_ids.contains(id)),
        "no old id must remain"
    );

    let scanned = store.snapshot().scan(&table()).unwrap();
    for b in &scanned {
        let idx_col = b.column_by_name("idx").unwrap();
        let mut vals = Vec::new();
        for i in 0..b.rows() {
            let Value::UInt64(v) = idx_col.get(i) else {
                unreachable!()
            };
            vals.push(v);
        }
        let mut sorted = vals.clone();
        sorted.sort_unstable();
        assert_eq!(vals, sorted, "each row group must be sorted by idx");
    }

    let mut got = flatten(&scanned);
    got.sort_unstable();
    let mut want: Vec<Row> = (0..3u64)
        .flat_map(|b| (0..4u32).map(move |i| (b, i)))
        .collect();
    want.sort_unstable();
    assert_eq!(got, want, "row multiset unchanged");

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 5: writes during a job are caught up ────────────────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn writes_during_a_job_are_caught_up() {
    let dir = temp_dir("catch-up");
    let store = open_table(&dir);
    write_three_batches(&store);

    let job = store
        .alter(
            &table(),
            vec![Alter::OrderBy(vec!["idx".to_string(), "batch".to_string()])],
        )
        .unwrap();
    assert!(matches!(
        store.run_job(job).unwrap(),
        JobStatus::Running { .. }
    ));

    for b in 3..5u64 {
        let rows: Vec<Row> = (0..4u32).map(|i| (b, i)).collect();
        store.write(&table(), row_batch(&rows)).unwrap();
    }

    loop {
        if let JobStatus::Swapped { .. } = store.run_job(job).unwrap() {
            break;
        }
    }

    let mut got = flatten(&store.snapshot().scan(&table()).unwrap());
    got.sort_unstable();
    let mut want: Vec<Row> = (0..5u64)
        .flat_map(|b| (0..4u32).map(move |i| (b, i)))
        .collect();
    want.sort_unstable();
    assert_eq!(got, want);

    let mut seen = HashSet::new();
    for r in &got {
        assert!(seen.insert(r), "row {r:?} must not be duplicated");
    }

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 6: a job resumes after reopen without redoing steps ─────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn a_job_resumes_after_reopen_without_redoing_steps() {
    let dir = temp_dir("resume");
    let store = open_table(&dir);
    write_three_batches(&store);

    let job = store
        .alter(
            &table(),
            vec![Alter::OrderBy(vec!["idx".to_string(), "batch".to_string()])],
        )
        .unwrap();
    assert!(matches!(
        store.run_job(job).unwrap(),
        JobStatus::Running { .. }
    ));

    let handled_before = store
        .jobs()
        .iter()
        .find(|j| j.id == job)
        .unwrap()
        .handled
        .len();
    let target_ids_before: Vec<u64> = store
        .jobs()
        .iter()
        .find(|j| j.id == job)
        .unwrap()
        .target
        .segments
        .iter()
        .map(|s| s.id)
        .collect();
    store.close().unwrap();

    let store = Store::open(&dir, opts()).unwrap();
    let jobs = store.jobs();
    assert_eq!(jobs.len(), 1, "progress must be in the manifest");
    assert_eq!(jobs[0].id, job);
    assert_eq!(jobs[0].handled.len(), handled_before);

    loop {
        if let JobStatus::Swapped { .. } = store.run_job(job).unwrap() {
            break;
        }
    }
    let final_ids: Vec<u64> = store
        .table(&table())
        .unwrap()
        .segments
        .iter()
        .map(|s| s.id)
        .collect();
    for id in &target_ids_before {
        assert!(
            final_ids.contains(id),
            "step {id} must survive to the final table, not be redone"
        );
    }

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 7: readers keep their snapshot across the swap ──────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn readers_keep_their_snapshot_across_the_swap() {
    let dir = temp_dir("reader-snapshot");
    let store = open_table(&dir);
    write_three_batches(&store);
    let view = store.snapshot();

    store
        .migrate(
            &table(),
            vec![Alter::AddColumn(Field {
                name: "extra".to_string(),
                ty: DataType::String,
            })],
        )
        .unwrap();

    let scanned = view.scan(&table()).unwrap();
    for b in &scanned {
        assert!(
            b.column_by_name("extra").is_none(),
            "an old view must still see the two-column schema"
        );
    }

    drop(view);
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 8: cancel leaves the table as it was ────────────────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn cancel_leaves_the_table_as_it_was() {
    let dir = temp_dir("cancel");
    let store = open_table(&dir);
    let before_ids = write_three_batches(&store);
    let before_schema = store.table(&table()).unwrap().schema.clone();
    let pre_job_count = count_seg_files(&dir);
    assert_eq!(pre_job_count, 3);

    let job = store
        .alter(
            &table(),
            vec![Alter::OrderBy(vec!["idx".to_string(), "batch".to_string()])],
        )
        .unwrap();
    store.run_job(job).unwrap();
    store.cancel_job(job).unwrap();

    assert!(store.jobs().is_empty());
    let after = store.table(&table()).unwrap();
    assert_eq!(after.schema, before_schema, "definition must be unchanged");
    let mut after_ids: Vec<u64> = after.segments.iter().map(|s| s.id).collect();
    after_ids.sort_unstable();
    assert_eq!(after_ids, before_ids, "segment ids must be unchanged");

    store.gc().unwrap();
    assert_eq!(
        count_seg_files(&dir),
        pre_job_count,
        "the rewritten file must not leak, and source files must not be deleted"
    );

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 9: REVERT after ADD is free and keeps new writes ────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn revert_after_add_column_is_free_and_keeps_new_writes() {
    let dir = temp_dir("revert-add");
    let store = open_table(&dir);
    write_three_batches(&store);

    store
        .migrate(
            &table(),
            vec![Alter::AddColumn(Field {
                name: "extra".to_string(),
                ty: DataType::String,
            })],
        )
        .unwrap();
    store
        .write(&table(), extra_batch(&[(9, 0, Some("z"))]))
        .unwrap();

    let count_before = count_seg_files(&dir);
    store.revert_table(&table()).unwrap();

    assert_eq!(store.table(&table()).unwrap().fields(), schema());

    let mut got = flatten(&store.snapshot().scan(&table()).unwrap());
    got.sort_unstable();
    let mut want: Vec<Row> = (0..3u64)
        .flat_map(|b| (0..4u32).map(move |i| (b, i)))
        .collect();
    want.push((9, 0));
    want.sort_unstable();
    assert_eq!(
        got, want,
        "every row, including the post-swap batch, survives"
    );

    assert_eq!(
        count_seg_files(&dir),
        count_before,
        "REVERT must not rewrite or drop files"
    );

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 10: REVERT after DROP brings the values back ────────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn revert_after_drop_column_brings_the_values_back() {
    let dir = temp_dir("revert-drop");
    let store = open_table(&dir);
    store
        .migrate(
            &table(),
            vec![Alter::AddColumn(Field {
                name: "extra".to_string(),
                ty: DataType::String,
            })],
        )
        .unwrap();
    store
        .write(
            &table(),
            extra_batch(&[(0, 0, Some("v0")), (1, 0, Some("v1"))]),
        )
        .unwrap();

    store
        .migrate(&table(), vec![Alter::DropColumn("extra".to_string())])
        .unwrap();
    store.revert_table(&table()).unwrap();

    let scanned = store.snapshot().scan(&table()).unwrap();
    let mut got: Vec<(u64, u64, String)> = Vec::new();
    for b in &scanned {
        let batch_col = b.column_by_name("batch").unwrap();
        let idx_col = b.column_by_name("idx").unwrap();
        let extra_col = b.column_by_name("extra").unwrap();
        for i in 0..b.rows() {
            let Value::UInt64(bn) = batch_col.get(i) else {
                unreachable!()
            };
            let Value::UInt64(iv) = idx_col.get(i) else {
                unreachable!()
            };
            let Value::String(e) = extra_col.get(i) else {
                unreachable!("REVERT must bring the original values back, not NULL")
            };
            got.push((bn, iv, e));
        }
    }
    got.sort();
    let want = vec![(0, 0, "v0".to_string()), (1, 0, "v1".to_string())];
    assert_eq!(got, want);

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 11: REVERT after compaction is refused ──────────────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn revert_after_compaction_is_refused() {
    let dir = temp_dir("revert-compact");
    let store = open_table(&dir);
    write_three_batches(&store);
    store
        .migrate(
            &table(),
            vec![Alter::AddColumn(Field {
                name: "extra".to_string(),
                ty: DataType::String,
            })],
        )
        .unwrap();

    for b in 3..5u64 {
        let rows: Vec<(u64, u32, Option<&str>)> = (0..4u32).map(|i| (b, i, None)).collect();
        store.write(&table(), extra_batch(&rows)).unwrap();
    }
    assert!(store.compact(&table()).unwrap().is_some());

    let err = store.revert_table(&table()).unwrap_err();
    assert!(
        matches!(err, Error::Manifest(manifest::Error::RevertStale { .. })),
        "{err}"
    );

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 12: a shared file outlives compaction until the retired entry expires ───

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn a_shared_file_outlives_compaction_until_the_retired_entry_expires() {
    let dir = temp_dir("shared-file-gc");
    let store = open_table(&dir);
    write_three_batches(&store);
    store
        .migrate(
            &table(),
            vec![Alter::AddColumn(Field {
                name: "extra".to_string(),
                ty: DataType::String,
            })],
        )
        .unwrap();

    let live_before = store.table(&table()).unwrap();
    let seg_paths: Vec<PathBuf> = live_before
        .segments
        .iter()
        .map(|s| Manifest::segment_path(&dir, s))
        .collect();
    assert!(seg_paths.iter().all(|p| p.exists()));

    assert!(store.compact(&table()).unwrap().is_some());
    store.gc().unwrap();
    assert!(
        seg_paths.iter().all(|p| p.exists()),
        "the retired Swapped entry still names them, so compaction's gc must not delete them"
    );

    drop(store);
    let store = Store::open(
        &dir,
        StoreOptions {
            retain_definitions: Duration::ZERO,
            ..opts()
        },
    )
    .unwrap();
    store.gc().unwrap();
    assert!(
        seg_paths.iter().all(|p| !p.exists()),
        "once the retired entry expires, GC must finally free them"
    );

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 13: DROP and UNDROP ──────────────────────────────────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn drop_and_undrop() {
    let dir = temp_dir("drop-undrop");
    let store = open_table(&dir);
    write_three_batches(&store);
    let orig_id = store.table(&table()).unwrap().id;

    store.drop_table(&table()).unwrap();
    assert!(matches!(
        store.write(&table(), row_batch(&[(0, 0)])),
        Err(Error::UnknownTable(_))
    ));

    store.undrop_table(&table()).unwrap();
    let mut got = flatten(&store.snapshot().scan(&table()).unwrap());
    got.sort_unstable();
    let mut want: Vec<Row> = (0..3u64)
        .flat_map(|b| (0..4u32).map(move |i| (b, i)))
        .collect();
    want.sort_unstable();
    assert_eq!(got, want);
    assert_eq!(store.table(&table()).unwrap().id, orig_id);

    assert!(matches!(
        store.undrop_table(&table()),
        Err(Error::Manifest(manifest::Error::NotDropped { .. }))
    ));

    store.drop_table(&table()).unwrap();
    store
        .create_table(TableSpec::new(table(), schema()))
        .unwrap();
    assert!(matches!(
        store.undrop_table(&table()),
        Err(Error::Manifest(manifest::Error::Conflict { .. }))
    ));

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 14: a dropped table's files survive reopen, then go after retention ────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn dropped_table_files_survive_reopen_and_go_after_retention() {
    let dir = temp_dir("drop-survive-reopen");
    let store = open_table(&dir);
    write_three_batches(&store);
    let seg_paths: Vec<PathBuf> = store
        .table(&table())
        .unwrap()
        .segments
        .iter()
        .map(|s| Manifest::segment_path(&dir, s))
        .collect();
    store.drop_table(&table()).unwrap();
    store.close().unwrap();
    assert!(seg_paths.iter().all(|p| p.exists()));

    // Reopen runs `cleanup_orphans`, which must keep a retired table's files.
    let store = Store::open(&dir, opts()).unwrap();
    assert!(seg_paths.iter().all(|p| p.exists()));
    store.undrop_table(&table()).unwrap();
    let mut got = flatten(&store.snapshot().scan(&table()).unwrap());
    got.sort_unstable();
    let mut want: Vec<Row> = (0..3u64)
        .flat_map(|b| (0..4u32).map(move |i| (b, i)))
        .collect();
    want.sort_unstable();
    assert_eq!(got, want);

    store.drop_table(&table()).unwrap();
    store.close().unwrap();

    let store = Store::open(
        &dir,
        StoreOptions {
            retain_definitions: Duration::ZERO,
            ..opts()
        },
    )
    .unwrap();
    store.gc().unwrap();
    assert!(seg_paths.iter().all(|p| !p.exists()));
    assert!(matches!(
        store.undrop_table(&table()),
        Err(Error::Manifest(manifest::Error::NotDropped { .. }))
    ));

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 15: TRUNCATE empties but keeps the table ────────────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn truncate_empties_but_keeps_the_table() {
    let dir = temp_dir("truncate");
    let store = open_table(&dir);
    write_three_batches(&store);
    let before = store.table(&table()).unwrap();
    let field_ids: Vec<FieldId> = before.schema.iter().map(|f| f.id).collect();

    store.truncate_table(&table()).unwrap();
    let empty = store.snapshot().scan(&table()).unwrap();
    assert!(empty.iter().all(|b| b.rows() == 0));

    let after = store.table(&table()).unwrap();
    assert_eq!(after.id, before.id);
    let after_field_ids: Vec<FieldId> = after.schema.iter().map(|f| f.id).collect();
    assert_eq!(after_field_ids, field_ids);

    store.write(&table(), row_batch(&[(9, 0)])).unwrap();
    assert_eq!(
        flatten(&store.snapshot().scan(&table()).unwrap()),
        vec![(9, 0)]
    );

    store.gc().unwrap();
    let old_paths: Vec<PathBuf> = before
        .segments
        .iter()
        .map(|s| Manifest::segment_path(&dir, s))
        .collect();
    assert!(old_paths.iter().all(|p| !p.exists()));

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 16: guardrails ───────────────────────────────────────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn guardrails() {
    let dir = temp_dir("guardrails");
    let store = open_table(&dir);
    write_three_batches(&store);

    store
        .delete(
            &table(),
            vec![Predicate {
                column: "idx".to_string(),
                op: CmpOp::Eq,
                value: Value::UInt64(0),
            }],
        )
        .unwrap();
    assert!(matches!(
        store.explain_alter(&table(), &[Alter::DropColumn("idx".to_string())]),
        Err(Error::Manifest(manifest::Error::ColumnInTombstone { .. }))
    ));

    assert!(matches!(
        store.explain_alter(
            &table(),
            &[Alter::PartitionBy(Some((
                "batch".to_string(),
                Duration::from_secs(1)
            )))],
        ),
        Err(Error::Manifest(manifest::Error::MigrationNotBuilt { .. }))
    ));
    assert!(matches!(
        store.explain_alter(
            &table(),
            &[Alter::SetType {
                column: "batch".to_string(),
                ty: DataType::Int64,
            }],
        ),
        Err(Error::Manifest(manifest::Error::MigrationNotBuilt { .. }))
    ));

    store
        .alter(
            &table(),
            vec![Alter::AddColumn(Field {
                name: "extra".to_string(),
                ty: DataType::String,
            })],
        )
        .unwrap();
    assert!(matches!(
        store.alter(
            &table(),
            vec![Alter::AddColumn(Field {
                name: "extra2".to_string(),
                ty: DataType::String,
            })],
        ),
        Err(Error::Manifest(manifest::Error::JobRunning { .. }))
    ));
    assert!(matches!(
        store.truncate_table(&table()),
        Err(Error::Manifest(manifest::Error::JobRunning { .. }))
    ));
    assert!(matches!(
        store.drop_table(&table()),
        Err(Error::Manifest(manifest::Error::JobRunning { .. }))
    ));

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── 17: a store from before jobs opens and migrates ─────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn a_store_from_before_jobs_opens_and_migrates() {
    let dir = temp_dir("old-fixture");
    copy_dir(&fixture_dir(), &dir);

    let store = Store::open(&dir, opts()).unwrap();
    assert!(store.jobs().is_empty());

    let events = TableName::new("main", "events");
    store
        .migrate(
            &events,
            vec![Alter::AddColumn(Field {
                name: "extra".to_string(),
                ty: DataType::String,
            })],
        )
        .unwrap();

    let scanned = store.snapshot().scan(&events).unwrap();
    assert!(!scanned.is_empty());
    for b in &scanned {
        let extra = b.column_by_name("extra").unwrap();
        for i in 0..b.rows() {
            assert!(extra.is_null(i));
        }
    }

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── Crash suite: MigrateTarget ───────────────────────────────────────────────

/// `main.t`, plus a churn of ADD/REVERT COLUMN and ORDER BY/REVERT between batches. `open`
/// resumes every leftover job to `Swapped`, which is the resume path under crash test.
struct MigrateTarget;

impl CrashTarget for MigrateTarget {
    type Store = Store;

    fn open(dir: &Path) -> io::Result<Store> {
        let store = Store::open(dir, opts()).map_err(to_io)?;
        if store.table(&table()).is_none() {
            store
                .create_table(TableSpec::new(table(), schema()))
                .map_err(to_io)?;
        }
        let job_ids: Vec<u64> = store.jobs().iter().map(|j| j.id).collect();
        for id in job_ids {
            loop {
                if let JobStatus::Swapped { .. } = store.run_job(id).map_err(to_io)? {
                    break;
                }
            }
        }
        Ok(store)
    }

    fn write(store: &mut Store, batch: &[Row]) -> io::Result<()> {
        let entry = store
            .table(&table())
            .ok_or_else(|| io::Error::other("main.t is missing"))?;
        let fields = entry.fields();
        let has_extra = fields.iter().any(|f| f.name == "extra");

        let batch_col: Vec<Value> = batch.iter().map(|&(b, _)| Value::UInt64(b)).collect();
        let idx_col: Vec<Value> = batch
            .iter()
            .map(|&(_, i)| Value::UInt64(u64::from(i)))
            .collect();
        let mut cols = vec![
            Column::from_values(&DataType::UInt64, &batch_col).unwrap(),
            Column::from_values(&DataType::UInt64, &idx_col).unwrap(),
        ];
        if has_extra {
            let nulls = vec![Value::Null; batch.len()];
            cols.push(Column::from_values(&DataType::String, &nulls).unwrap());
        }
        let b = Batch::new(fields, cols).map_err(|e| io::Error::other(e.to_string()))?;
        store.write(&table(), b).map_err(to_io)?;
        Ok(())
    }

    fn read_all(store: &Store) -> io::Result<Vec<Row>> {
        let view = store.snapshot();
        Ok(flatten(&view.scan(&table()).map_err(to_io)?))
    }

    fn between(store: &mut Store, batch: u64) -> io::Result<()> {
        let entry = store
            .table(&table())
            .ok_or_else(|| io::Error::other("main.t is missing"))?;
        let has_extra = entry.fields().iter().any(|f| f.name == "extra");
        let order_by_empty = entry.order_by.is_empty();

        match batch % 6 {
            1 if !has_extra => {
                store
                    .migrate(
                        &table(),
                        vec![Alter::AddColumn(Field {
                            name: "extra".to_string(),
                            ty: DataType::String,
                        })],
                    )
                    .map_err(to_io)?;
            }
            2 if has_extra => match store.revert_table(&table()) {
                Ok(_) => {}
                Err(Error::Manifest(manifest::Error::NothingToRevert { .. }))
                | Err(Error::Manifest(manifest::Error::RevertStale { .. })) => {
                    store
                        .migrate(&table(), vec![Alter::DropColumn("extra".to_string())])
                        .map_err(to_io)?;
                }
                Err(e) => return Err(to_io(e)),
            },
            4 if order_by_empty => {
                store
                    .migrate(
                        &table(),
                        vec![Alter::OrderBy(vec!["idx".to_string(), "batch".to_string()])],
                    )
                    .map_err(to_io)?;
            }
            5 if !order_by_empty => match store.revert_table(&table()) {
                Ok(_) => {}
                Err(Error::Manifest(manifest::Error::NothingToRevert { .. }))
                | Err(Error::Manifest(manifest::Error::RevertStale { .. })) => {
                    store
                        .migrate(&table(), vec![Alter::OrderBy(Vec::new())])
                        .map_err(to_io)?;
                }
                Err(e) => return Err(to_io(e)),
            },
            _ => {}
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
    let summary = run::<T>(test_path, &plan).expect("the store must survive this kill");
    assert!(summary.killed > 0, "{name} was never hit in 8 runs");
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn crash_migrate_pre_segment_sync() {
    assert_failpoint::<MigrateTarget>(
        concat!(module_path!(), "::crash_migrate_pre_segment_sync"),
        "migrate.pre_segment_sync",
        20,
    );
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn crash_migrate_pre_advance() {
    assert_failpoint::<MigrateTarget>(
        concat!(module_path!(), "::crash_migrate_pre_advance"),
        "migrate.pre_advance",
        21,
    );
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn crash_migrate_pre_swap() {
    assert_failpoint::<MigrateTarget>(
        concat!(module_path!(), "::crash_migrate_pre_swap"),
        "migrate.pre_swap",
        22,
    );
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn crash_revert_pre_publish() {
    assert_failpoint::<MigrateTarget>(
        concat!(module_path!(), "::crash_revert_pre_publish"),
        "revert.pre_publish",
        23,
    );
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn crash_migrate_random() {
    let plan = Plan {
        runs: 8,
        batches: 12,
        rows_per_batch: 16,
        seed: 24,
        kill: Kill::Random { max_delay_ms: 40 },
    };
    run::<MigrateTarget>(concat!(module_path!(), "::crash_migrate_random"), &plan)
        .expect("the store must survive a random kill mid-migration");
}
