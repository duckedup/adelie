//! Garbage collection and orphan cleanup (SPEC §18, D0009). A garbage segment is deleted only
//! once `gc_grace` has passed and no live in-process `Snapshot` still names it.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Weak};

use crate::manifest::{Edit, Manifest};

use super::{Error, Shared, io_err, now_ms};

/// Deletes every garbage segment old enough and unnamed by a live `Snapshot`, then commits one
/// `ForgetGarbage` for what it removed. Returns how many files were deleted.
pub(crate) fn run(shared: &Arc<Shared>) -> Result<usize, Error> {
    let manifest = shared.state.lock().unwrap().current.manifest().clone();
    let grace_ms = shared.opts.gc_grace.as_millis() as u64;
    let now = now_ms();

    let live: Vec<Arc<crate::manifest::Snapshot>> = {
        let mut live = shared.live.lock().unwrap();
        live.retain(|w| w.strong_count() > 0);
        live.iter().filter_map(Weak::upgrade).collect()
    };

    let mut removed_ids = Vec::new();
    for g in &manifest.garbage {
        if now.saturating_sub(g.removed_at_ms) < grace_ms {
            continue;
        }
        if live.iter().any(|s| s.names_segment(&g.table, g.segment.id)) {
            continue;
        }
        let path = Manifest::segment_path(&shared.root, &g.table, &g.segment);
        shared.io.remove(&path).map_err(|source| io_err(&path, source))?;
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

/// Removes any `<id>.seg` file under `root` that names neither a live nor a garbage segment
/// (SPEC §18): it was never published, or was GC'd after its grace already elapsed.
pub(crate) fn cleanup_orphans(root: &Path, manifest: &Manifest, io: &crate::io::Io) -> Result<(), Error> {
    let mut keep: HashSet<u64> = HashSet::new();
    for t in &manifest.tables {
        keep.extend(t.segments.iter().map(|s| s.id));
    }
    keep.extend(manifest.garbage.iter().map(|g| g.segment.id));
    walk(root, &keep, io)
}

fn walk(dir: &Path, keep: &HashSet<u64>, io: &crate::io::Io) -> Result<(), Error> {
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
    use crate::exec::{Column, Field};
    use crate::store::{Store, StoreOptions};
    use crate::types::{DataType, Value};
    use std::path::PathBuf;
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("adelie-store-gc-{tag}-{}-{nanos}", std::process::id()))
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
            ..StoreOptions::default()
        };
        let store = Store::open(&dir, opts).unwrap();
        let table = crate::manifest::TableName::new("d", "t");
        store.create_table(&table, "append", schema()).unwrap();
        store.write(&table, batch(1)).unwrap();
        store.write(&table, batch(2)).unwrap();

        let view = store.snapshot();
        let old_ids: Vec<u64> = view.table(&table).unwrap().segments.iter().map(|s| s.id).collect();
        let paths: Vec<PathBuf> = old_ids
            .iter()
            .map(|id| dir.join("d").join("t").join("_").join(format!("{id:016x}.seg")))
            .collect();

        store.compact(&table).unwrap().unwrap();
        assert!(paths.iter().all(|p| p.exists()), "compact must not delete while a Snapshot lives");

        drop(view);
        let removed = store.gc().unwrap();
        assert_eq!(removed, old_ids.len());
        assert!(paths.iter().all(|p| !p.exists()));
        assert!(store.snapshot().snapshot().manifest().garbage.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
