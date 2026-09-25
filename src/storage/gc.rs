//! Garbage collection and orphan cleanup (SPEC §18, D0009). A garbage segment is deleted only
//! once `gc_grace` has passed, no live in-process `Snapshot` still names it, and it fell out of
//! the `retain_manifests` window (SPEC §19 AT VERSION, D0014).

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Weak};

use crate::storage::manifest::{Edit, Manifest};

use super::{Error, Shared, io_err, now_ms};

/// First expires any retired table entry past `retain_definitions`, releasing its segments,
/// then deletes every garbage segment old enough, unnamed by a live `Snapshot`, and outside the
/// `retain_manifests` window a retained `view_at` might still read, committing one
/// `ForgetGarbage` for what it removed. Returns how many files were deleted.
pub(crate) fn run(shared: &Arc<Shared>) -> Result<usize, Error> {
    let now = now_ms();
    // Expired once its age reaches `retain_definitions` (`ExpireRetired` is strict, hence the
    // +1): zero retention then expires even an entry retired this same millisecond.
    let before_ms = now.saturating_sub(shared.opts.retain_definitions.as_millis() as u64) + 1;
    let any_expired = shared
        .state
        .lock()
        .unwrap()
        .current
        .manifest()
        .retired
        .iter()
        .any(|r| r.retired_at_ms < before_ms);
    if any_expired {
        shared.commit(|_v| vec![Edit::ExpireRetired { before_ms }], &[])?;
    }

    let manifest = shared.state.lock().unwrap().current.manifest().clone();
    let grace_ms = shared.opts.gc_grace.as_millis() as u64;
    let retain = shared.opts.retain_manifests as u64;

    let live: Vec<Arc<crate::storage::manifest::Snapshot>> = {
        let mut live = shared.live.lock().unwrap();
        live.retain(|w| w.strong_count() > 0);
        live.iter().filter_map(Weak::upgrade).collect()
    };

    let mut removed_ids = Vec::new();
    for g in &manifest.garbage {
        if now.saturating_sub(g.removed_at_ms) < grace_ms {
            continue;
        }
        // A retained manifest version is readable (SPEC §19 AT VERSION, D0014): anything it
        // names that the current version doesn't was released after it, so keep garbage
        // released inside the retain window. 0 = released before this was recorded, which
        // retention does not protect. Saturating: the field is decoded from disk.
        if retain > 0
            && g.removed_at_version > 0
            && g.removed_at_version.saturating_add(retain) > manifest.version + 1
        {
            continue;
        }
        // The reference count is `release`'s job; this is defence in depth (D0012).
        if live.iter().any(|s| s.references_segment(g.segment.id))
            || manifest.references_segment(g.segment.id)
        {
            continue;
        }
        let path = Manifest::segment_path(&shared.root, &g.segment);
        shared
            .io
            .remove(&path)
            .map_err(|source| io_err(&path, source))?;
        removed_ids.push(g.segment.id);
    }

    if !removed_ids.is_empty() {
        shared.commit(
            |_v| {
                vec![Edit::ForgetGarbage {
                    ids: removed_ids.clone(),
                }]
            },
            &[],
        )?;
    }
    Ok(removed_ids.len())
}

/// Removes any `<id>.seg` file under `root` that names neither a live, garbage, retired nor
/// job-target segment (SPEC §18, D0012): it was never published, or was GC'd after its grace
/// already elapsed. A retired entry's and a running job's files must survive this (D0012).
pub(crate) fn cleanup_orphans(
    root: &Path,
    manifest: &Manifest,
    io: &crate::storage::io::Io,
) -> Result<(), Error> {
    let mut keep: HashSet<u64> = HashSet::new();
    for t in &manifest.tables {
        keep.extend(t.segments.iter().map(|s| s.id));
    }
    keep.extend(manifest.garbage.iter().map(|g| g.segment.id));
    for r in &manifest.retired {
        keep.extend(r.entry.segments.iter().map(|s| s.id));
    }
    for j in &manifest.jobs {
        keep.extend(j.target.segments.iter().map(|s| s.id));
    }
    walk(root, &keep, io)
}

fn walk(dir: &Path, keep: &HashSet<u64>, io: &crate::storage::io::Io) -> Result<(), Error> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(io_err(dir, source)),
    };
    for entry in entries {
        let entry = entry.map_err(|source| io_err(dir, source))?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| io_err(&path, source))?;
        if file_type.is_dir() {
            walk(&path, keep, io)?;
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(hex) = name.strip_suffix(".seg") else {
            continue;
        };
        let Ok(id) = u64::from_str_radix(hex, 16) else {
            continue;
        };
        if !keep.contains(&id) {
            io.remove(&path).map_err(|source| io_err(&path, source))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::cleanup_orphans;
    use crate::exec::{Column, Field};
    use crate::storage::manifest::Edit;
    use crate::storage::{Store, StoreOptions, TableSpec};
    use crate::types::{DataType, Value};
    use std::path::PathBuf;
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "adelie-store-gc-{tag}-{}-{nanos}",
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

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn a_compacted_file_survives_a_live_snapshot_and_is_gone_after_it_drops() {
        let dir = temp_dir("safe-deletion");
        let opts = StoreOptions {
            compact_min_inputs: 2,
            compact_small_rows: 100,
            gc_grace: Duration::ZERO,
            retain_manifests: 0,
            ..StoreOptions::default()
        };
        let store = Store::open(&dir, opts).unwrap();
        let table = crate::storage::manifest::TableName::new("d", "t");
        store
            .create_table(TableSpec::new(table.clone(), schema()))
            .unwrap();
        store.write(&table, batch(1)).unwrap();
        store.write(&table, batch(2)).unwrap();

        let view = store.snapshot();
        let old_ids: Vec<u64> = view
            .table(&table)
            .unwrap()
            .segments
            .iter()
            .map(|s| s.id)
            .collect();
        let table_dir = view.table(&table).unwrap().dir();
        let seg_dir = crate::storage::manifest::Manifest::table_dir(&dir, &table_dir).join("_");
        let paths: Vec<PathBuf> = old_ids
            .iter()
            .map(|id| seg_dir.join(format!("{id:016x}.seg")))
            .collect();

        store.compact(&table).unwrap().unwrap();
        assert!(
            paths.iter().all(|p| p.exists()),
            "compact must not delete while a Snapshot lives"
        );

        drop(view);
        let removed = store.gc().unwrap();
        assert_eq!(removed, old_ids.len());
        assert!(paths.iter().all(|p| !p.exists()));
        assert!(store.snapshot().snapshot().manifest().garbage.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// SPEC §19 AT VERSION, D0014: a retained manifest version can still read a segment
    /// released after it, so gc must not delete one until it falls out of `retain_manifests`.
    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn garbage_survives_gc_until_it_falls_out_of_the_retain_manifests_window() {
        let dir = temp_dir("retain-window");
        let opts = StoreOptions {
            compact_min_inputs: 2,
            compact_small_rows: 100,
            gc_grace: Duration::ZERO,
            retain_manifests: 3,
            ..StoreOptions::default()
        };
        let store = Store::open(&dir, opts).unwrap();
        let table = crate::storage::manifest::TableName::new("d", "t");
        store
            .create_table(TableSpec::new(table.clone(), schema()))
            .unwrap();
        store.write(&table, batch(1)).unwrap();
        store.write(&table, batch(2)).unwrap();

        // `table()`, not `snapshot()`: a clone, not a live `Snapshot`, so only retention (not
        // the live-reference check) is under test.
        let before = store.table(&table).unwrap();
        let old_ids: Vec<u64> = before.segments.iter().map(|s| s.id).collect();
        let seg_dir = crate::storage::manifest::Manifest::table_dir(&dir, &before.dir()).join("_");
        let paths: Vec<PathBuf> = old_ids
            .iter()
            .map(|id| seg_dir.join(format!("{id:016x}.seg")))
            .collect();

        // `compact` releases the old segments and runs its own gc; still inside the window.
        store.compact(&table).unwrap().unwrap();
        assert!(
            paths.iter().all(|p| p.exists()),
            "a version still inside the retain window must be able to read the old segments"
        );
        assert_eq!(store.gc().unwrap(), 0);
        assert!(paths.iter().all(|p| p.exists()));

        // Two more commits, on a second table, push the release version out of the window.
        let table2 = crate::storage::manifest::TableName::new("d", "t2");
        store
            .create_table(TableSpec::new(table2.clone(), schema()))
            .unwrap();
        store.write(&table2, batch(3)).unwrap();

        let removed = store.gc().unwrap();
        assert_eq!(removed, old_ids.len());
        assert!(paths.iter().all(|p| !p.exists()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// D0012: a segment a live table and a retired entry both name must survive GC by that
    /// cross-table reference alone, not just by the table `gc.rs:30` used to check.
    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn a_segment_a_retired_entry_shares_survives_a_live_view_and_is_gone_once_it_drops() {
        let dir = temp_dir("cross-table-refcount");
        let opts = StoreOptions {
            compact_min_inputs: 1,
            compact_small_rows: 100,
            gc_grace: Duration::ZERO,
            // Long enough that `compact`'s own gc cannot expire the retired entry mid-test;
            // the test expires it explicitly below, at the point it means to.
            retain_definitions: Duration::from_secs(3600),
            retain_manifests: 0,
            ..StoreOptions::default()
        };
        let store = Store::open(&dir, opts).unwrap();
        let table = crate::storage::manifest::TableName::new("d", "t");
        store
            .create_table(TableSpec::new(table.clone(), schema()))
            .unwrap();
        store.write(&table, batch(1)).unwrap();

        // A rename reuses every segment: the swap shares it between the new live table and the
        // now-retired source.
        store
            .migrate(
                &table,
                vec![crate::storage::manifest::Alter::RenameColumn {
                    from: "a".to_string(),
                    to: "aa".to_string(),
                }],
            )
            .unwrap();
        let retired_seg = store
            .snapshot()
            .snapshot()
            .manifest()
            .retired
            .last()
            .unwrap()
            .entry
            .segments[0]
            .clone();
        let shared_id = retired_seg.id;
        // The physical file never moves for a reused segment (only a rewrite does), so it is
        // still found at the *retired* (source) entry's own recorded directory, not the live
        // (target) table's.
        let seg_path = crate::storage::manifest::Manifest::segment_path(&dir, &retired_seg);

        // Compacting the live table drops its own reference; the retired entry still names it,
        // so `release` must not garbage it yet.
        store.compact(&table).unwrap();
        assert!(seg_path.exists());

        // A view taken now sees the id only through the retired entry, since the live table
        // already dropped it: exactly the reference the old table-scoped check missed.
        let view = store.snapshot();
        assert!(view.snapshot().manifest().references_segment(shared_id));
        assert_eq!(
            store.gc().unwrap(),
            0,
            "the retired entry must still protect it"
        );
        assert!(seg_path.exists());

        // Expire the retired entry: the segment is now garbage, named only by `view`.
        store
            .shared
            .commit(
                |_v| {
                    vec![Edit::ExpireRetired {
                        before_ms: u64::MAX,
                    }]
                },
                &[],
            )
            .unwrap();
        assert!(
            store
                .snapshot()
                .snapshot()
                .manifest()
                .garbage
                .iter()
                .any(|g| g.segment.id == shared_id)
        );

        let removed = store.gc().unwrap();
        assert_eq!(removed, 0, "the live view must still protect it");
        assert!(seg_path.exists());

        drop(view);
        let removed = store.gc().unwrap();
        assert_eq!(removed, 1);
        assert!(!seg_path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn stub_table(
        id: u64,
        name: &str,
        segments: Vec<crate::storage::manifest::SegmentEntry>,
    ) -> crate::storage::manifest::TableEntry {
        use crate::storage::manifest::{TableEntry, TableId, TableName};
        TableEntry {
            id: TableId(id),
            name: TableName::new("d", name),
            engine: "append".to_string(),
            schema: Vec::new(),
            next_field_id: 1,
            key: Vec::new(),
            version: None,
            order_by: Vec::new(),
            partition_by: None,
            ttl: None,
            options: std::collections::BTreeMap::new(),
            segments,
            tombstones: Vec::new(),
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn cleanup_orphans_keeps_a_retired_entrys_file_and_a_job_targets_file() {
        let root = temp_dir("cleanup-orphans");
        let dir = "d/0000000000000001".to_string();
        let part_dir = crate::storage::manifest::Manifest::table_dir(&root, &dir).join("_");
        std::fs::create_dir_all(&part_dir).unwrap();
        let retired_path = part_dir.join("0000000000000001.seg");
        let job_path = part_dir.join("0000000000000002.seg");
        let orphan_path = part_dir.join("0000000000000003.seg");
        for p in [&retired_path, &job_path, &orphan_path] {
            std::fs::write(p, b"x").unwrap();
        }

        let mk_seg = |id: u64| crate::storage::manifest::SegmentEntry {
            id,
            partition: "_".to_string(),
            seq: 1,
            rows: 1,
            bytes: 1,
            footer_crc: 0,
            columns: Vec::new(),
            side_files: Vec::new(),
            dir: dir.clone(),
            field_ids: Vec::new(),
            file_field_ids: Vec::new(),
        };

        let mut manifest = crate::storage::manifest::Manifest::empty();
        manifest.retired.push(crate::storage::manifest::Retired {
            entry: stub_table(1, "t", vec![mk_seg(1)]),
            reason: crate::storage::manifest::RetireReason::Swapped,
            retired_at_ms: 0,
            version: 1,
            successor_segments: vec![],
        });
        manifest.jobs.push(crate::storage::manifest::Job {
            id: 1,
            source: crate::storage::manifest::TableId(2),
            snapshot: 0,
            target: stub_table(3, "t2", vec![mk_seg(2)]),
            handled: vec![],
            reused: 0,
            rewritten: 0,
        });

        cleanup_orphans(&root, &manifest, &crate::storage::io::Io::real()).unwrap();
        assert!(retired_path.exists());
        assert!(job_path.exists());
        assert!(!orphan_path.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
