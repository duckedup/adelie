//! `acquire`: the one-writer-per-store lock (SPEC §6), via `std::fs::File::try_lock` (stable
//! since 1.89). No dependency, no unsafe.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use super::Error;

/// Opens (creating if needed) `dir/lock` and takes an exclusive lock on it. The returned
/// `File` must live as long as the store: dropping it releases the lock.
pub(crate) fn acquire(dir: &Path) -> Result<File, Error> {
    let path = dir.join("lock");
    let file = open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(Error::Locked { path }),
        Err(TryLockError::Error(source)) => Err(super::io_err(&path, source)),
    }
}

fn open(path: &Path) -> Result<File, Error> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|source| super::io_err(path, source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "adelie-store-lock-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn a_second_acquire_is_locked_and_a_drop_releases_it() {
        let dir = temp_dir("second");
        std::fs::create_dir_all(&dir).unwrap();
        let first = acquire(&dir).unwrap();
        assert!(matches!(acquire(&dir), Err(Error::Locked { .. })));
        drop(first);
        assert!(acquire(&dir).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
