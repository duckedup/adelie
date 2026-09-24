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
    NoSuchJob {
        job: u64,
    },
    /// A second migration was started while one was already running on the table.
    JobRunning {
        table: String,
        job: u64,
    },
    NothingToRevert {
        table: String,
    },
    /// REVERT refused: restoring would duplicate or lose rows (SPEC §19).
    RevertStale {
        table: String,
        detail: String,
    },
    NotDropped {
        table: String,
    },
    /// `DROP COLUMN` on a column engine, KEY or VERSION depends on (SPEC §19: those clauses
    /// never change for a table's life).
    FixedColumn {
        table: String,
        column: String,
        clause: String,
    },
    /// `DROP COLUMN` on a column a tombstone predicate still names.
    ColumnInTombstone {
        table: String,
        column: String,
        seq: u64,
    },
    /// A rollup (SPEC §16.1, not built) reads this column.
    ColumnUsedByRollup {
        table: String,
        column: String,
        rollup: String,
    },
    /// `PARTITION BY` or a type change: planned by SPEC §19 but not implemented (D0013).
    MigrationNotBuilt {
        table: String,
        kind: String,
    },
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
            Error::NoSuchJob { job } => write!(f, "no job with id {job}"),
            Error::JobRunning { table, job } => write!(
                f,
                "table {table}: migration job {job} is running; finish it with run_job or \
                 cancel it"
            ),
            Error::NothingToRevert { table } => write!(f, "table {table}: nothing to revert"),
            Error::RevertStale { table, detail } => {
                write!(f, "table {table}: cannot revert: {detail}")
            }
            Error::NotDropped { table } => write!(f, "table {table} is not dropped"),
            Error::FixedColumn {
                table,
                column,
                clause,
            } => write!(
                f,
                "table {table}: column {column} is in {clause}; engine, KEY and VERSION are \
                 fixed for a table's life — copy to a new table instead (SPEC §19)"
            ),
            Error::ColumnInTombstone {
                table,
                column,
                seq,
            } => write!(
                f,
                "table {table}: column {column} is named by the tombstone at seq {seq}"
            ),
            Error::ColumnUsedByRollup {
                table,
                column,
                rollup,
            } => write!(
                f,
                "table {table}: column {column} is used by rollup {rollup}"
            ),
            Error::MigrationNotBuilt { table, kind } => {
                write!(f, "table {table}: {kind} is not built yet")
            }
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

    #[test]
    fn no_such_job_names_the_id() {
        let msg = Error::NoSuchJob { job: 7 }.to_string();
        assert!(msg.contains('7'));
    }

    #[test]
    fn job_running_names_table_and_job_and_how_to_resolve_it() {
        let msg = Error::JobRunning {
            table: "d.t".to_string(),
            job: 3,
        }
        .to_string();
        assert!(msg.contains("d.t"));
        assert!(msg.contains('3'));
        assert!(msg.contains("run_job"));
        assert!(msg.contains("cancel"));
    }

    #[test]
    fn nothing_to_revert_names_the_table() {
        let msg = Error::NothingToRevert {
            table: "d.t".to_string(),
        }
        .to_string();
        assert!(msg.contains("d.t"));
    }

    #[test]
    fn revert_stale_carries_the_detail() {
        let msg = Error::RevertStale {
            table: "d.t".to_string(),
            detail: "segment 9 was compacted or removed since the swap".to_string(),
        }
        .to_string();
        assert!(msg.contains("d.t"));
        assert!(msg.contains("segment 9"));
    }

    #[test]
    fn not_dropped_names_the_table() {
        let msg = Error::NotDropped {
            table: "d.t".to_string(),
        }
        .to_string();
        assert!(msg.contains("d.t"));
    }

    #[test]
    fn fixed_column_names_the_clause_and_suggests_a_new_table() {
        let msg = Error::FixedColumn {
            table: "d.t".to_string(),
            column: "id".to_string(),
            clause: "KEY".to_string(),
        }
        .to_string();
        assert!(msg.contains("id"));
        assert!(msg.contains("KEY"));
        assert!(msg.contains("copy to a new table"));
    }

    #[test]
    fn column_in_tombstone_names_the_seq() {
        let msg = Error::ColumnInTombstone {
            table: "d.t".to_string(),
            column: "a".to_string(),
            seq: 4,
        }
        .to_string();
        assert!(msg.contains('a'));
        assert!(msg.contains('4'));
    }

    #[test]
    fn column_used_by_rollup_names_it() {
        let msg = Error::ColumnUsedByRollup {
            table: "d.t".to_string(),
            column: "a".to_string(),
            rollup: "r1".to_string(),
        }
        .to_string();
        assert!(msg.contains('a'));
        assert!(msg.contains("r1"));
    }

    #[test]
    fn migration_not_built_names_the_kind() {
        let msg = Error::MigrationNotBuilt {
            table: "d.t".to_string(),
            kind: "PARTITION BY".to_string(),
        }
        .to_string();
        assert!(msg.contains("PARTITION BY"));
    }
}
