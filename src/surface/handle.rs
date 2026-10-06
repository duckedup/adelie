//! `Handle`: the one way every surface opens a directory and runs SQL against it.

use std::path::Path;

use crate::sql::{self, SqlError, SqlOutput};
use crate::storage::{self, Reader, Store, StoreOptions, View};

/// A read-only `Reader` (lock-free) or a writing `Store` (holds the writer lock).
pub enum Handle {
    Read(Reader),
    Write(Store),
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Handle>();
};

impl Handle {
    /// `write` takes the writer lock; otherwise the directory opens lock-free for reading.
    pub fn open(dir: &Path, write: bool) -> Result<Handle, storage::Error> {
        if write {
            Ok(Handle::Write(Store::open(dir, StoreOptions::default())?))
        } else {
            Ok(Handle::Read(Reader::open(dir)?))
        }
    }

    pub fn view(&self) -> Result<View, storage::Error> {
        match self {
            Handle::Read(reader) => reader.snapshot(),
            Handle::Write(store) => Ok(store.snapshot()),
        }
    }

    /// A read handle rejects any write statement with `SqlError::ReadOnly`.
    pub fn run(&self, sql: &str, opts: &sql::Options) -> Result<SqlOutput, SqlError> {
        match self {
            Handle::Read(reader) => sql::execute_read(&reader.snapshot()?, sql, opts),
            Handle::Write(store) => sql::execute_with(store, sql, opts),
        }
    }
}
