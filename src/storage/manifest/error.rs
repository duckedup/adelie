//! `Error`: the manifest's error type (SPEC §5, §18, D0009). Hand-written, following the style
//! of `segment::error`: one arm per variant, and every message names the path or table.

use std::fmt;
use std::path::PathBuf;

/// A manifest operation gone wrong. `UnknownEngine` is raised by `store` on open; it is
/// defined here because the manifest is where the engine name lives.
#[derive(Debug)]
pub enum Error {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Corrupt {
        path: String,
        detail: String,
    },
    UnsupportedVersion {
        path: String,
        version: u16,
    },
    UnknownEngine {
        table: String,
        engine: String,
    },
    UnknownSideFile {
        path: String,
        kind: u64,
    },
    Conflict {
        table: String,
        detail: String,
    },
    Usage(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { path, source } => write!(f, "{}: io error: {source}", path.display()),
            Error::Corrupt { path, detail } => write!(f, "{path}: corrupt: {detail}"),
            Error::UnsupportedVersion { path, version } => {
                write!(f, "{path}: unsupported manifest version {version}")
            }
            Error::UnknownEngine { table, engine } => {
                write!(f, "table {table}: unknown engine {engine}")
            }
            Error::UnknownSideFile { path, kind } => {
                write!(f, "{path}: unknown side-file kind {kind}")
            }
            Error::Conflict { table, detail } => write!(f, "table {table}: conflict: {detail}"),
            Error::Usage(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {}
