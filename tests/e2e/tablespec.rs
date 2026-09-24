//! adelie-2hh.1 (group 2): the public API against a store written before D0012 (stable ids),
//! and `TableSpec`'s error paths (SPEC §5, §18, D0012).

use std::path::Path;
use std::time::Duration;

use adelie::exec::{Batch, Column, Field};
use adelie::storage::manifest::{self, MANIFEST_FILE, TableId, TableName};
use adelie::storage::{Error, Reader, Store, StoreOptions, TableSpec};
use adelie::types::{DataType, Value};

fn opts() -> StoreOptions {
    StoreOptions {
        flush_interval: Duration::from_millis(5),
        gc_grace: Duration::ZERO,
        compact_min_inputs: 2,
        ..Default::default()
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "adelie-e2e-tablespec-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

fn fixture_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/store-0.5.0")
}

/// Recursive, std-only. Opening a store mutates it (lock, orphan cleanup, maybe a commit), so
/// every test works on a throwaway copy, never the fixture itself.
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

fn events() -> TableName {
    TableName::new("main", "events")
}

fn users() -> TableName {
    TableName::new("main", "users")
}

/// `main.events` and `main.users`: `(id UINT64, msg STRING)`.
fn schema() -> Vec<Field> {
    vec![
        Field {
            name: "id".to_string(),
            ty: DataType::UInt64,
        },
        Field {
            name: "msg".to_string(),
            ty: DataType::String,
        },
    ]
}

/// `id UINT64, ts TIMESTAMP`: for the KEY/ORDER BY tests, which need a sortable second column.
fn id_ts_schema() -> Vec<Field> {
    vec![
        Field {
            name: "id".to_string(),
            ty: DataType::UInt64,
        },
        Field {
            name: "ts".to_string(),
            ty: DataType::Timestamp,
        },
    ]
}

fn row_batch(rows: &[(u64, &str)]) -> Batch {
    let id_col: Vec<Value> = rows.iter().map(|&(id, _)| Value::UInt64(id)).collect();
    let msg_col: Vec<Value> = rows
        .iter()
        .map(|&(_, m)| Value::String(m.to_string()))
        .collect();
    Batch::new(
        schema(),
        vec![
            Column::from_values(&DataType::UInt64, &id_col).unwrap(),
            Column::from_values(&DataType::String, &msg_col).unwrap(),
        ],
    )
    .unwrap()
}

/// The reverse of `row_batch`, over every batch a scan returned.
fn flatten(batches: &[Batch]) -> Vec<(u64, String)> {
    let mut rows = Vec::new();
    for b in batches {
        let id_col = b.column_by_name("id").unwrap();
        let msg_col = b.column_by_name("msg").unwrap();
        for i in 0..b.rows() {
            let Value::UInt64(id) = id_col.get(i) else {
                unreachable!("id column is UInt64")
            };
            let Value::String(msg) = msg_col.get(i) else {
                unreachable!("msg column is STRING")
            };
            rows.push((id, msg));
        }
    }
    rows
}

fn field_ids(entry: &manifest::TableEntry) -> Vec<u64> {
    entry.schema.iter().map(|f| f.id.0).collect()
}

// ── ids on an old store ─────────────────────────────────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn an_old_store_opens_with_ids_in_name_order() {
    let dir = temp_dir("ids-in-order");
    copy_dir(&fixture_dir(), &dir);

    let store = Store::open(&dir, opts()).unwrap();
    let view = store.snapshot();

    let e = view.table(&events()).unwrap();
    assert_eq!(e.id, TableId(1));
    assert_eq!(field_ids(e), vec![1, 2]);

    let u = view.table(&users()).unwrap();
    assert_eq!(u.id, TableId(2));
    assert_eq!(field_ids(u), vec![1, 2]);

    let new_table = TableName::new("main", "brand_new");
    store
        .create_table(TableSpec::new(new_table.clone(), schema()))
        .unwrap();
    assert_eq!(store.snapshot().table(&new_table).unwrap().id, TableId(3));

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn an_old_store_reads_its_segments_where_they_are_and_writes_new_ones_under_the_table_id() {
    let dir = temp_dir("segments-in-place");
    copy_dir(&fixture_dir(), &dir);

    let store = Store::open(&dir, opts()).unwrap();
    let before = flatten(&store.snapshot().scan(&events()).unwrap());
    assert_eq!(
        before,
        vec![
            (3, "c".to_string()),
            (1, "a".to_string()),
            (2, "b".to_string()),
        ]
    );

    store.write(&events(), row_batch(&[(4, "d")])).unwrap();

    let new_seg_dir = dir.join("main/0000000000000001/_");
    let new_segs: Vec<_> = std::fs::read_dir(&new_seg_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "seg"))
        .collect();
    assert_eq!(
        new_segs.len(),
        1,
        "exactly one new segment under the table-id directory"
    );

    let old_seg_dir = dir.join("main/events/_");
    let old_segs: Vec<_> = std::fs::read_dir(&old_seg_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "seg"))
        .collect();
    assert_eq!(old_segs.len(), 2, "the two legacy segments are untouched");

    let after = flatten(&store.snapshot().scan(&events()).unwrap());
    assert_eq!(
        after,
        vec![
            (3, "c".to_string()),
            (1, "a".to_string()),
            (2, "b".to_string()),
            (4, "d".to_string()),
        ]
    );

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn ids_assigned_on_open_are_the_same_before_and_after_they_are_persisted() {
    let dir = temp_dir("ids-stable");
    copy_dir(&fixture_dir(), &dir);
    let manifest_path = dir.join(MANIFEST_FILE);

    let before = std::fs::read(&manifest_path).unwrap();

    let view = Reader::open(&dir).unwrap().snapshot().unwrap();
    assert_eq!(view.table(&events()).unwrap().id, TableId(1));
    assert_eq!(view.table(&users()).unwrap().id, TableId(2));

    let after_reader_open = std::fs::read(&manifest_path).unwrap();
    assert_eq!(
        before, after_reader_open,
        "a Reader's open must not republish the manifest"
    );

    {
        let store = Store::open(&dir, opts()).unwrap();
        store.write(&events(), row_batch(&[(9, "z")])).unwrap();
        store.close().unwrap();
    }

    let store = Store::open(&dir, opts()).unwrap();
    let view = store.snapshot();
    assert_eq!(view.table(&events()).unwrap().id, TableId(1));
    assert_eq!(view.table(&users()).unwrap().id, TableId(2));

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn compacting_an_old_table_moves_it_into_the_id_layout() {
    let dir = temp_dir("compact-migrates");
    copy_dir(&fixture_dir(), &dir);

    let store = Store::open(&dir, opts()).unwrap();
    store.write(&events(), row_batch(&[(5, "e")])).unwrap();

    assert!(store.compact(&events()).unwrap().is_some());
    store.gc().unwrap();

    let mut rows = flatten(&store.snapshot().scan(&events()).unwrap());
    rows.sort();
    let mut want = vec![
        (1, "a".to_string()),
        (2, "b".to_string()),
        (3, "c".to_string()),
        (5, "e".to_string()),
    ];
    want.sort();
    assert_eq!(rows, want);

    let new_seg_dir = dir.join("main/0000000000000001/_");
    let segs: Vec<_> = std::fs::read_dir(&new_seg_dir).unwrap().collect();
    assert_eq!(
        segs.len(),
        1,
        "the compacted segment lives under the table-id directory"
    );

    let old_seg_dir = dir.join("main/events/_");
    let old_left = std::fs::read_dir(&old_seg_dir)
        .map(|mut e| e.next().is_some())
        .unwrap_or(false);
    assert!(!old_left, "the legacy segment files are gone after gc");

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── TableSpec: a definition survives reopen ─────────────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn a_table_definition_survives_reopen() {
    let dir = temp_dir("spec-round-trip");
    let t = TableName::new("main", "full");
    let cols = vec![
        Field {
            name: "ts".to_string(),
            ty: DataType::Timestamp,
        },
        Field {
            name: "host".to_string(),
            ty: DataType::String,
        },
        Field {
            name: "n".to_string(),
            ty: DataType::Int64,
        },
    ];
    let spec = TableSpec::new(t.clone(), cols)
        .order_by(["host", "ts"])
        .partition_by("ts", Duration::from_secs(86_400))
        .ttl("ts", Duration::from_secs(30 * 86_400))
        .with("note", "x")
        .with("a", "b");

    {
        let store = Store::open(&dir, opts()).unwrap();
        store.create_table(spec.clone()).unwrap();
        store.close().unwrap();
    }

    let store = Store::open(&dir, opts()).unwrap();
    assert_eq!(store.snapshot().table(&t).unwrap().spec(), spec);

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── TableSpec errors, through the store's public API ────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn key_on_append_is_an_error_that_names_latest() {
    let dir = temp_dir("key-not-allowed");
    let store = Store::open(&dir, opts()).unwrap();
    let t = TableName::new("main", "k");

    let err = store
        .create_table(TableSpec::new(t.clone(), schema()).key(["id"]))
        .unwrap_err();
    assert!(matches!(
        err,
        Error::Manifest(manifest::Error::KeyNotAllowed { .. })
    ));
    let msg = err.to_string();
    assert!(msg.contains("latest"), "{msg}");
    assert!(msg.contains("KEY"), "{msg}");

    assert!(store.snapshot().table(&t).is_none());

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn a_key_that_is_not_a_prefix_of_order_by_is_an_error() {
    let dir = temp_dir("key-not-prefix");
    let store = Store::open(&dir, opts()).unwrap();
    let t = TableName::new("main", "k");

    let err = store
        .create_table(
            TableSpec::new(t, id_ts_schema())
                .engine("latest")
                .key(["id"])
                .order_by(["ts", "id"]),
        )
        .unwrap_err();
    // Not `EngineNotBuilt`, even though `latest` isn't built: validation runs first.
    assert!(matches!(
        err,
        Error::Manifest(manifest::Error::KeyNotSortPrefix { .. })
    ));
    let msg = err.to_string();
    assert!(msg.contains("(id)"), "{msg}");
    assert!(msg.contains("(ts, id)"), "{msg}");
    assert!(msg.contains("ORDER BY (id, ts)"), "{msg}");

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn an_unbuilt_engine_with_a_valid_spec_says_so() {
    let dir = temp_dir("engine-not-built");
    let store = Store::open(&dir, opts()).unwrap();
    let t = TableName::new("main", "k");

    let err = store
        .create_table(
            TableSpec::new(t, id_ts_schema())
                .engine("latest")
                .key(["id"]),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        Error::Manifest(manifest::Error::EngineNotBuilt { .. })
    ));
    let msg = err.to_string();
    assert!(msg.contains("latest"), "{msg}");
    assert!(msg.contains("append"), "{msg}");

    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}
