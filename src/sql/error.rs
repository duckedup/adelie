//! `SqlError` (contract C5): every way `sql::execute`/`execute_with` can fail. Hand-written,
//! no `anyhow`/`thiserror` (D0004); `Display` plus `std::error::Error` throughout.

use std::fmt;

use super::ingest::IngestError;
use super::parser::ParseError;
use crate::exec::ExecError;
use crate::storage;

/// Everything a statement can fail with (contract C5). `Bind` and `Plan` are both plain
/// messages: `Bind` for name/shape errors the binder catches itself, `Plan` for anything an
/// `exec::ExecError` (including one surfaced through `storage::Error::Exec`) reports.
#[derive(Debug)]
pub enum SqlError {
    Parse(ParseError),
    Bind(String),
    Plan(String),
    Ingest(IngestError),
    Io(String),
    Storage(storage::Error),
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlError::Parse(e) => write!(f, "{e}"),
            SqlError::Bind(msg) => write!(f, "{msg}"),
            SqlError::Plan(msg) => write!(f, "{msg}"),
            SqlError::Ingest(e) => write!(f, "{e}"),
            SqlError::Io(msg) => write!(f, "{msg}"),
            SqlError::Storage(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SqlError {}

impl From<ParseError> for SqlError {
    fn from(e: ParseError) -> Self {
        SqlError::Parse(e)
    }
}

impl From<IngestError> for SqlError {
    fn from(e: IngestError) -> Self {
        SqlError::Ingest(e)
    }
}

/// `storage::Error::Exec` is the executor's own failure surfacing through storage (a bad
/// plan, budget or cancellation), never storage's fault, so it becomes `Plan` here too.
impl From<storage::Error> for SqlError {
    fn from(e: storage::Error) -> Self {
        match e {
            storage::Error::Exec(ee) => ee.into(),
            other => SqlError::Storage(other),
        }
    }
}

/// Any executor failure the binder/planner hits directly (constant folding, a bad plan node)
/// becomes `Plan`: the contract names only that variant for typing failures.
impl From<ExecError> for SqlError {
    fn from(e: ExecError) -> Self {
        match e {
            ExecError::Plan(msg) => SqlError::Plan(msg),
            other => SqlError::Plan(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_forwards_to_the_inner_message() {
        assert_eq!(SqlError::Bind("bad".to_string()).to_string(), "bad");
        assert_eq!(SqlError::Plan("nope".to_string()).to_string(), "nope");
    }

    #[test]
    fn exec_error_becomes_plan() {
        let e: SqlError = ExecError::Plan("oops".to_string()).into();
        assert!(matches!(e, SqlError::Plan(_)));
    }

    #[test]
    fn storage_exec_error_unwraps_to_plan() {
        let inner = storage::Error::Exec(ExecError::Plan("bad plan".to_string()));
        let e: SqlError = inner.into();
        assert!(matches!(e, SqlError::Plan(msg) if msg == "bad plan"));
    }

    #[test]
    fn other_storage_error_stays_storage() {
        let inner = storage::Error::UnknownTable("main.t".to_string());
        let e: SqlError = inner.into();
        assert!(matches!(e, SqlError::Storage(_)));
    }
}
