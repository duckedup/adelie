//! `Publisher`: writes a new manifest version durably (SPEC §5) — write-to-temp, fsync,
//! rename, fsync-directory, then a numbered hard link kept for `retain_manifests` versions.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::storage::fail;
use crate::storage::io::Io;

use super::Manifest;
use super::error::Error;

pub const MANIFEST_FILE: &str = "manifest";

pub struct Publisher {
    root: PathBuf,
    io: Io,
    retain_manifests: usize,
    swept: AtomicBool,
}

impl Publisher {
    pub(crate) fn new(root: PathBuf, io: Io, retain_manifests: usize) -> Publisher {
        Publisher {
            root,
            io,
            retain_manifests,
            swept: AtomicBool::new(false),
        }
    }

    fn manifest_path(&self) -> PathBuf {
        self.root.join(MANIFEST_FILE)
    }

    fn tmp_path(&self) -> PathBuf {
        self.root.join(format!("{MANIFEST_FILE}.tmp"))
    }

    fn versioned_path(&self, version: u64) -> PathBuf {
        self.root.join(format!("{MANIFEST_FILE}.{version}"))
    }

    /// write manifest.tmp -> sync_file -> [fail manifest.pre_rename] -> rename to manifest
    /// -> [fail manifest.pre_dir_sync] -> sync_dir(root) -> hard_link manifest -> manifest.<v>
    /// -> prune links older than the last `retain_manifests` (not durability-critical).
    pub fn publish(&self, m: &Manifest) -> Result<(), Error> {
        let tmp = self.tmp_path();
        let bytes = m.encode();
        let file = self
            .io
            .write_new(&tmp, &bytes)
            .map_err(|source| io_err(&tmp, source))?;
        self.io
            .sync_file(&file, &tmp)
            .map_err(|source| io_err(&tmp, source))?;

        fail::point("manifest.pre_rename");
        let dest = self.manifest_path();
        self.io
            .rename(&tmp, &dest)
            .map_err(|source| io_err(&dest, source))?;

        fail::point("manifest.pre_dir_sync");
        self.io
            .sync_dir(&self.root)
            .map_err(|source| io_err(&self.root, source))?;

        // Committed above; the link and prune are a debugging aid. Failing here would tell the
        // caller a durable commit did not happen, and invite a retry that applies it twice.
        let _ = self.io.hard_link(&dest, &self.versioned_path(m.version));
        self.prune(m.version);
        Ok(())
    }

    /// Removes `manifest.<v>` links older than the last `retain_manifests`. Best-effort. The
    /// first publish sweeps the directory once (links left by a crash or a smaller `retain`);
    /// after that each publish removes only the one link that just fell out of the window.
    fn prune(&self, latest: u64) {
        let retain = self.retain_manifests as u64;
        if retain == 0 || latest < retain {
            return;
        }
        let cutoff = latest - retain;
        if self.swept.swap(true, Ordering::Relaxed) {
            let _ = self.io.remove(&self.versioned_path(cutoff));
            return;
        }
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let version = name
                .to_str()
                .and_then(|n| n.strip_prefix(&format!("{MANIFEST_FILE}.")))
                .and_then(|v| v.parse::<u64>().ok());
            if version.is_some_and(|v| v <= cutoff) {
                let _ = self.io.remove(&entry.path());
            }
        }
    }

    /// Reads `root/manifest`; a missing file loads as `Manifest::empty()`.
    pub fn load(&self) -> Result<Manifest, Error> {
        let path = self.manifest_path();
        match self.io.read(&path) {
            Ok(bytes) => Manifest::decode(&path.display().to_string(), &bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Manifest::empty()),
            Err(source) => Err(io_err(&path, source)),
        }
    }

    /// Reads retained `root/manifest.<version>`; `None` once it has been pruned (or was never
    /// linked). A link whose decoded version differs is `Corrupt`.
    pub fn load_version(&self, version: u64) -> Result<Option<Manifest>, Error> {
        let path = self.versioned_path(version);
        match self.io.read(&path) {
            Ok(bytes) => {
                let m = Manifest::decode(&path.display().to_string(), &bytes)?;
                if m.version != version {
                    return Err(Error::Corrupt {
                        path: path.display().to_string(),
                        detail: format!("names version {version} but decodes to {}", m.version),
                    });
                }
                Ok(Some(m))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(io_err(&path, source)),
        }
    }
}

fn io_err(path: &std::path::Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::io::Op;
    use std::path::Path;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "adelie-manifest-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn publish_records_the_exact_durability_sequence() {
        let root = temp_dir("publish-order");
        std::fs::create_dir_all(&root).unwrap();
        let (io, log) = Io::recording();
        let publisher = Publisher::new(root.clone(), io, 8);

        let mut m = Manifest::empty();
        m.version = 1;
        publisher.publish(&m).unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            vec![
                Op::Write(root.join("manifest.tmp")),
                Op::SyncFile(root.join("manifest.tmp")),
                Op::Rename(root.join("manifest.tmp"), root.join("manifest")),
                Op::SyncDir(root.clone()),
                Op::Link(root.join("manifest"), root.join("manifest.1")),
            ]
        );

        let loaded = publisher.load().unwrap();
        assert_eq!(loaded, m);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn load_of_a_missing_manifest_is_empty() {
        let root = temp_dir("publish-missing");
        std::fs::create_dir_all(&root).unwrap();
        let publisher = Publisher::new(root.clone(), Io::real(), 8);

        assert_eq!(publisher.load().unwrap(), Manifest::empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn retention_keeps_only_the_last_n_versioned_links() {
        let root = temp_dir("publish-retain");
        std::fs::create_dir_all(&root).unwrap();
        let publisher = Publisher::new(root.clone(), Io::real(), 3);

        for v in 1..=10u64 {
            let mut m = Manifest::empty();
            m.version = v;
            publisher.publish(&m).unwrap();
        }

        let mut present: Vec<u64> = (1..=10)
            .filter(|v| Path::new(&root.join(format!("manifest.{v}"))).exists())
            .collect();
        present.sort_unstable();
        assert_eq!(present, vec![8, 9, 10]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn the_first_publish_sweeps_links_a_previous_run_left_behind() {
        let root = temp_dir("publish-sweep");
        std::fs::create_dir_all(&root).unwrap();
        for v in 1..=5u64 {
            std::fs::write(root.join(format!("manifest.{v}")), b"old").unwrap();
        }
        let publisher = Publisher::new(root.clone(), Io::real(), 2);
        let mut m = Manifest::empty();
        m.version = 6;
        publisher.publish(&m).unwrap();

        let present: Vec<u64> = (1..=6)
            .filter(|v| root.join(format!("manifest.{v}")).exists())
            .collect();
        assert_eq!(present, vec![5, 6]);
        assert!(root.join(MANIFEST_FILE).exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn load_version_reads_a_retained_link_and_none_for_a_pruned_one() {
        let root = temp_dir("publish-load-version");
        std::fs::create_dir_all(&root).unwrap();
        let publisher = Publisher::new(root.clone(), Io::real(), 3);

        let mut versions = Vec::new();
        for v in 1..=10u64 {
            let mut m = Manifest::empty();
            m.version = v;
            publisher.publish(&m).unwrap();
            versions.push(m);
        }

        // Retained: the last 3 (8, 9, 10), per `retention_keeps_only_the_last_n_versioned_links`.
        assert_eq!(publisher.load_version(10).unwrap(), Some(versions[9].clone()));
        assert_eq!(publisher.load_version(8).unwrap(), Some(versions[7].clone()));
        // Pruned: falls outside the retained window.
        assert_eq!(publisher.load_version(1).unwrap(), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn a_kill_before_rename_leaves_the_previous_manifest_intact() {
        let root = temp_dir("publish-pre-rename");
        std::fs::create_dir_all(&root).unwrap();
        let publisher = Publisher::new(root.clone(), Io::real(), 8);

        let mut v1 = Manifest::empty();
        v1.version = 1;
        publisher.publish(&v1).unwrap();

        // Simulate a crash before rename: write only the tmp file, as `publish` would have
        // before its `manifest.pre_rename` failpoint, then check the live manifest is untouched.
        let mut v2 = Manifest::empty();
        v2.version = 2;
        std::fs::write(root.join("manifest.tmp"), v2.encode()).unwrap();

        assert_eq!(publisher.load().unwrap(), v1);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
