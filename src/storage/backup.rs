//! `backup_to`: a consistent hard-linked copy of the store (SPEC §19, D0014). An interrupted
//! backup has no `manifest` file there and is incomplete — delete it and retry. Hard links need
//! the same filesystem; a cross-device failure surfaces as the plain io error.

use std::collections::HashSet;
use std::path::Path;

use crate::storage::manifest::{Manifest, Publisher, SegmentEntry};

use super::{Error, Store, io_err};

/// Flushes, then hard-links every unique live segment (by id, across tables and retired
/// entries) into `dir` and publishes a manifest there with `garbage` and `jobs` cleared.
/// Returns the backed-up version.
pub(crate) fn backup_to(store: &Store, dir: &Path) -> Result<u64, Error> {
    store.flush()?;
    // Live-registered for as long as `view` is held, so gc leaves its segments alone while we
    // link them.
    let view = store.snapshot();
    let manifest = view.snapshot().manifest();

    if std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some()) {
        return Err(Error::Usage(format!(
            "backup target {} is not empty",
            dir.display()
        )));
    }
    store.shared.io.create_dir_all(dir).map_err(|e| io_err(dir, e))?;

    // v1 rejects any side file on decode (manifest/mod.rs:87-88): a segment's own file is the
    // only file it owns.
    let mut seen = HashSet::new();
    let mut segments: Vec<&SegmentEntry> = Vec::new();
    for t in &manifest.tables {
        segments.extend(t.segments.iter().filter(|s| seen.insert(s.id)));
    }
    for r in &manifest.retired {
        segments.extend(r.entry.segments.iter().filter(|s| seen.insert(s.id)));
    }

    let mut synced_dirs: Vec<std::path::PathBuf> = Vec::new();
    for seg in segments {
        let src = Manifest::segment_path(&store.shared.root, seg);
        let dst = Manifest::segment_path(dir, seg);
        let parent = dst.parent().expect("a segment path always has a parent");
        store
            .shared
            .io
            .create_dir_all(parent)
            .map_err(|e| io_err(parent, e))?;
        crate::storage::fail::point("backup.pre_link");
        store
            .shared
            .io
            .hard_link(&src, &dst)
            .map_err(|e| io_err(&dst, e))?;
        if !synced_dirs.iter().any(|d| d == parent) {
            synced_dirs.push(parent.to_path_buf());
        }
    }
    for d in &synced_dirs {
        store.shared.io.sync_dir(d).map_err(|e| io_err(d, e))?;
    }
    store.shared.io.sync_dir(dir).map_err(|e| io_err(dir, e))?;

    let mut backup_manifest = manifest.clone();
    backup_manifest.garbage.clear();
    backup_manifest.jobs.clear();
    crate::storage::fail::point("backup.pre_manifest");
    Publisher::new(
        dir.to_path_buf(),
        store.shared.io.clone(),
        store.shared.opts.retain_manifests,
    )
    .publish(&backup_manifest)?;
    Ok(backup_manifest.version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Batch, Column, Field};
    use crate::storage::manifest::TableName;
    use crate::storage::{StoreOptions, TableSpec};
    use crate::types::{DataType, Value};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("adelie-backup-{tag}-{}-{nanos}", std::process::id()))
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn backup_to_a_non_empty_directory_is_refused_before_any_link() {
        let dir = temp_dir("src");
        let target = temp_dir("dst");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("junk"), b"x").unwrap();

        let store = Store::open(&dir, StoreOptions::default()).unwrap();
        let table = TableName::new("d", "t");
        let fields = vec![Field {
            name: "a".to_string(),
            ty: DataType::Int64,
        }];
        store
            .create_table(TableSpec::new(table.clone(), fields.clone()))
            .unwrap();
        store
            .write(
                &table,
                Batch::new(
                    fields,
                    vec![Column::from_values(&DataType::Int64, &[Value::Int64(1)]).unwrap()],
                )
                .unwrap(),
            )
            .unwrap();

        let err = store.backup_to(&target).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not empty"), "{msg}");
        assert_eq!(
            std::fs::read_dir(&target).unwrap().count(),
            1,
            "nothing must be linked into a rejected target"
        );

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&target).unwrap();
    }
}
