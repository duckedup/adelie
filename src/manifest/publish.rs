//! `Publisher`: writes a new manifest version durably (SPEC §5) — write-to-temp, fsync,
//! rename, fsync-directory, then a numbered hard link kept for `retain_manifests` versions.

use std::path::PathBuf;

use crate::fail;
use crate::io::Io;

use super::Manifest;
use super::error::Error;

pub const MANIFEST_FILE: &str = "manifest";

pub struct Publisher {
    root: PathBuf,
    io: Io,
    retain_manifests: usize,
}

impl Publisher {
    pub(crate) fn new(root: PathBuf, io: Io, retain_manifests: usize) -> Publisher {
        Publisher {
            root,
            io,
            retain_manifests,
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

        let versioned = self.versioned_path(m.version);
        self.io
            .hard_link(&dest, &versioned)
            .map_err(|source| io_err(&versioned, source))?;

        self.prune(m.version);
        Ok(())
    }

    /// Removes `manifest.<v>` links older than the last `retain_manifests`. Best-effort: a
    /// missing or already-gone link is fine, and a failure here never fails `publish`.
    fn prune(&self, latest: u64) {
        let retain = self.retain_manifests as u64;
        if retain == 0 || latest < retain {
            return;
        }
        for v in 0..=(latest - retain) {
            let _ = self.io.remove(&self.versioned_path(v));
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
    use crate::io::Op;
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
