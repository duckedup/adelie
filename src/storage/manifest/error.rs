//! `Error`: the manifest's error type (SPEC §5, §18, D0009, D0012). Hand-written, following the
//! style of `segment::error`: one arm per variant, and every message names the path or table.

use std::fmt;
use std::path::PathBuf;

/// Engines actually built (mirrors `storage::engines::engine_by_name`): only `EngineNotBuilt`'s
/// `Display` reads this, so it stays a plain list rather than a dependency on `engines`.
const BUILT_ENGINES: &[&str] = &["append"];

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
    /// The engine named in a valid `TableSpec` is not built yet (SPEC §18): reachable only once
    /// `validate` has already passed, so the KEY rules for an unbuilt engine still teach.
    EngineNotBuilt {
        table: String,
        engine: String,
    },
    /// KEY on an engine that keeps every row instead of merging by key.
    KeyNotAllowed {
        table: String,
        engine: String,
        key: Vec<String>,
    },
    /// A keyed engine (SPEC §18) with no KEY clause.
    KeyRequired {
        table: String,
        engine: String,
    },
    /// KEY is not a prefix of the resolved ORDER BY.
    KeyNotSortPrefix {
        table: String,
        key: Vec<String>,
        order_by: Vec<String>,
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
                let choices: Vec<&str> = super::spec::ENGINES.iter().map(|e| e.name).collect();
                write!(
                    f,
                    "table {table}: unknown engine {engine} (engines: {})",
                    choices.join(", ")
                )
            }
            Error::EngineNotBuilt { table, engine } => write!(
                f,
                "table {table}: ENGINE = {engine} is not available yet (built: {})",
                BUILT_ENGINES.join(", ")
            ),
            Error::KeyNotAllowed { table, engine, key } => write!(
                f,
                "table {table}: KEY is not allowed with ENGINE = {engine}, which keeps every \
                 row; for the newest row per key use ENGINE = latest KEY ({})",
                key.join(", ")
            ),
            Error::KeyRequired { table, engine } => write!(
                f,
                "table {table}: ENGINE = {engine} needs a KEY, e.g. ENGINE = {engine} KEY (id)"
            ),
            Error::KeyNotSortPrefix {
                table,
                key,
                order_by,
            } => {
                let mut suggestion = key.clone();
                for c in order_by {
                    if !suggestion.contains(c) {
                        suggestion.push(c.clone());
                    }
                }
                write!(
                    f,
                    "table {table}: KEY ({}) must be a prefix of ORDER BY ({}); try ORDER BY ({})",
                    key.join(", "),
                    order_by.join(", "),
                    suggestion.join(", ")
                )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_engine_lists_the_choices() {
        let msg = Error::UnknownEngine {
            table: "d.t".to_string(),
            engine: "nope".to_string(),
        }
        .to_string();
        assert!(msg.contains("nope"));
        assert!(msg.contains("append, latest, rollup, vector, ledger"));
    }

    #[test]
    fn engine_not_built_names_the_engine_and_what_is_built() {
        let msg = Error::EngineNotBuilt {
            table: "d.t".to_string(),
            engine: "latest".to_string(),
        }
        .to_string();
        assert!(msg.contains("latest"));
        assert!(msg.contains("built: append"));
    }

    #[test]
    fn key_not_allowed_names_latest_and_echoes_the_key() {
        let msg = Error::KeyNotAllowed {
            table: "d.t".to_string(),
            engine: "append".to_string(),
            key: vec!["id".to_string()],
        }
        .to_string();
        assert!(msg.contains("latest"));
        assert!(msg.contains("KEY (id)"));
    }

    #[test]
    fn key_not_sort_prefix_names_both_lists_and_suggests_the_fix() {
        let msg = Error::KeyNotSortPrefix {
            table: "d.t".to_string(),
            key: vec!["id".to_string()],
            order_by: vec!["ts".to_string(), "id".to_string()],
        }
        .to_string();
        assert!(msg.contains("KEY (id)"));
        assert!(msg.contains("ORDER BY (ts, id)"));
        assert!(msg.contains("try ORDER BY (id, ts)"));
    }
}
