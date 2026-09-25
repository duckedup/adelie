//! `Error`: the store's error type (SPEC §6, §18). Hand-written, following the style of
//! `src/storage/segment/error.rs`: one arm per variant, every message names the table or path.

use std::fmt;
use std::path::PathBuf;

use super::{manifest, segment};

/// A store operation gone wrong.
#[derive(Debug)]
pub enum Error {
    Manifest(manifest::Error),
    Segment(segment::Error),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Locked {
        path: PathBuf,
    },
    UnknownTable(String),
    SchemaMismatch {
        table: String,
        detail: String,
    },
    SnapshotExpired {
        segment: String,
    },
    Closed,
    Flush(String),
    Usage(String),
    /// `view_at(version)`: the version's manifest link is gone (pruned past `retain_manifests`)
    /// or it is newer than `current`.
    VersionNotRetained {
        version: u64,
        current: u64,
    },
    /// `apply_migrations`: a recorded file changed since it was applied.
    MigrationChanged {
        name: String,
    },
    /// `apply_migrations`: a recorded file is no longer in the directory.
    MigrationMissing {
        name: String,
    },
    /// `apply_migrations`: a bad file name, a duplicate number, non-UTF-8 content, or a pending
    /// number at or below the highest already applied.
    MigrationInvalid {
        name: String,
        detail: String,
    },
    /// `apply_migrations`: `exec` returned an error for this file.
    MigrationFailed {
        name: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Manifest(e) => write!(f, "{e}"),
            Error::Segment(e) => write!(f, "{e}"),
            Error::Io { path, source } => write!(f, "{}: io error: {source}", path.display()),
            Error::Locked { path } => write!(f, "{}: already locked", path.display()),
            Error::UnknownTable(t) => write!(f, "unknown table {t}"),
            Error::SchemaMismatch { table, detail } => {
                write!(f, "table {table}: schema mismatch: {detail}")
            }
            Error::SnapshotExpired { segment } => {
                write!(f, "segment {segment}: no longer on disk")
            }
            Error::Closed => write!(f, "store is closed"),
            Error::Flush(msg) => write!(f, "flush failed: {msg}"),
            Error::Usage(msg) => write!(f, "{msg}"),
            Error::VersionNotRetained { version, current } if *version > *current => write!(
                f,
                "version {version} does not exist yet (current version {current})"
            ),
            Error::VersionNotRetained { version, current } => write!(
                f,
                "version {version} is not retained (current version {current})"
            ),
            Error::MigrationChanged { name } => {
                write!(f, "migration {name}: file changed since it was applied")
            }
            Error::MigrationMissing { name } => {
                write!(
                    f,
                    "migration {name}: applied but no longer in the directory"
                )
            }
            Error::MigrationInvalid { name, detail } => {
                write!(f, "migration {name}: {detail}")
            }
            Error::MigrationFailed { name, source } => write!(f, "migration {name}: {source}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<manifest::Error> for Error {
    fn from(e: manifest::Error) -> Self {
        Error::Manifest(e)
    }
}

impl From<segment::Error> for Error {
    fn from(e: segment::Error) -> Self {
        Error::Segment(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_and_segment_errors_convert_and_display() {
        let m: Error = manifest::Error::Usage("x".to_string()).into();
        assert!(matches!(m, Error::Manifest(_)));
        assert_eq!(m.to_string(), "x");
    }
}
