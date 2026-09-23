//! `Io`: every store and manifest filesystem operation goes through here (SPEC §6). Its
//! recording mode logs each op after it succeeds, so a test can assert the exact durability
//! order a crash must not be able to reorder.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// One filesystem operation `Io` performed. Recorded in call order, only after it succeeded.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Op {
    Write(PathBuf),
    SyncFile(PathBuf),
    SyncDir(PathBuf),
    Rename(PathBuf, PathBuf),
    Remove(PathBuf),
    Link(PathBuf, PathBuf),
    CreateDir(PathBuf),
    Ack(u64),
}

/// A thin wrapper over `std::fs`. `real()` performs the calls with no bookkeeping; `recording()`
/// also appends every successful op to a shared log a test can inspect.
#[derive(Clone, Default)]
pub(crate) struct Io {
    log: Option<Arc<Mutex<Vec<Op>>>>,
}

impl Io {
    /// No recording.
    pub(crate) fn real() -> Io {
        Io { log: None }
    }

    /// A recording `Io`, plus the shared log it appends to.
    #[cfg(test)]
    pub(crate) fn recording() -> (Io, Arc<Mutex<Vec<Op>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        (
            Io {
                log: Some(log.clone()),
            },
            log,
        )
    }

    fn push(&self, op: Op) {
        if let Some(log) = &self.log {
            log.lock().unwrap().push(op);
        }
    }

    /// Creates (truncating) `path`, writes `bytes`, and logs `Write`. The returned handle is
    /// for a follow-up `sync_file`.
    pub(crate) fn write_new(&self, path: &Path, bytes: &[u8]) -> io::Result<File> {
        let mut f = File::create(path)?;
        f.write_all(bytes)?;
        self.push(Op::Write(path.to_path_buf()));
        Ok(f)
    }

    pub(crate) fn sync_file(&self, f: &File, path: &Path) -> io::Result<()> {
        f.sync_all()?;
        self.push(Op::SyncFile(path.to_path_buf()));
        Ok(())
    }

    pub(crate) fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        File::open(dir)?.sync_all()?;
        self.push(Op::SyncDir(dir.to_path_buf()));
        Ok(())
    }

    pub(crate) fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)?;
        self.push(Op::Rename(from.to_path_buf(), to.to_path_buf()));
        Ok(())
    }

    /// `NotFound` is not an error: the caller wanted `path` gone, and it already is.
    pub(crate) fn remove(&self, path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        self.push(Op::Remove(path.to_path_buf()));
        Ok(())
    }

    /// `AlreadyExists` is not an error: the link the caller wanted is already there.
    pub(crate) fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        match std::fs::hard_link(from, to) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        self.push(Op::Link(from.to_path_buf(), to.to_path_buf()));
        Ok(())
    }

    /// Creates every missing ancestor of `dir`, top-down. Each new directory's creation is
    /// followed by `sync_dir` on its parent, which is what makes the new entry durable.
    pub(crate) fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        let mut missing = Vec::new();
        let mut cur = dir;
        while !cur.is_dir() {
            missing.push(cur);
            match cur.parent() {
                Some(parent) => cur = parent,
                None => break,
            }
        }
        for d in missing.into_iter().rev() {
            std::fs::create_dir(d)?;
            self.push(Op::CreateDir(d.to_path_buf()));
            if let Some(parent) = d.parent() {
                self.sync_dir(parent)?;
            }
        }
        Ok(())
    }

    /// Not logged: a read plays no part in the durability sequence a test asserts.
    pub(crate) fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    /// Logs `Ack(version)` only — the point in the sequence where pending writers unblock.
    pub(crate) fn ack(&self, version: u64) {
        self.push(Op::Ack(version));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("adelie-io-{tag}-{}-{nanos}", std::process::id()))
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn create_dir_all_logs_each_new_dir_and_its_parent_sync() {
        let root = temp_dir("create-dir-all");
        std::fs::create_dir_all(&root).unwrap();
        let (io, log) = Io::recording();

        io.create_dir_all(&root.join("a").join("b")).unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            vec![
                Op::CreateDir(root.join("a")),
                Op::SyncDir(root.clone()),
                Op::CreateDir(root.join("a").join("b")),
                Op::SyncDir(root.join("a")),
            ]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn create_dir_all_on_an_existing_dir_is_a_no_op() {
        let root = temp_dir("create-dir-all-existing");
        std::fs::create_dir_all(&root).unwrap();
        let (io, log) = Io::recording();

        io.create_dir_all(&root).unwrap();

        assert!(log.lock().unwrap().is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn remove_of_a_missing_file_is_ok_and_still_logged() {
        let root = temp_dir("remove-missing");
        std::fs::create_dir_all(&root).unwrap();
        let (io, log) = Io::recording();

        io.remove(&root.join("nope")).unwrap();

        assert_eq!(*log.lock().unwrap(), vec![Op::Remove(root.join("nope"))]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn hard_link_onto_an_existing_link_is_ok() {
        let root = temp_dir("hard-link-existing");
        std::fs::create_dir_all(&root).unwrap();
        let (io, _log) = Io::recording();
        let src = root.join("a");
        let dst = root.join("b");
        io.write_new(&src, b"x").unwrap();

        io.hard_link(&src, &dst).unwrap();
        io.hard_link(&src, &dst).unwrap();

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn real_io_does_not_record() {
        let root = temp_dir("real");
        std::fs::create_dir_all(&root).unwrap();
        let io = Io::real();
        io.write_new(&root.join("f"), b"hi").unwrap();
        io.ack(1);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
