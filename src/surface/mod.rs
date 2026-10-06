//! Shared plumbing for the CLI, MCP and HTTP surfaces: one handle, JSON values, shaped output.

use std::path::Path;

use crate::sql::{self, SqlError, SqlOutput};
use crate::storage::{self, Reader, Store, View};
use crate::types::Value;

/// A read-only `Reader` (lock-free) or a writing `Store` (holds the writer lock).
pub enum Handle {
    Read(Reader),
    Write(Store),
}

impl Handle {
    pub fn open(_dir: &Path, _write: bool) -> Result<Handle, storage::Error> {
        Err(storage::Error::Usage("not built yet".to_string()))
    }

    pub fn view(&self) -> Result<View, storage::Error> {
        Err(storage::Error::Usage("not built yet".to_string()))
    }

    pub fn run(&self, _sql: &str, _opts: &sql::Options) -> Result<SqlOutput, SqlError> {
        Err(SqlError::Plan("not built yet".to_string()))
    }
}

pub fn value_json(_v: &Value) -> serde_json::Value {
    serde_json::Value::Null
}

/// Caps on what one result may carry back to a caller.
pub struct Limits {
    pub max_rows: usize,
    pub max_bytes: usize,
}

pub struct Shaped {
    pub json: serde_json::Value,
    pub truncated: Option<String>,
}

pub fn shape(_out: &SqlOutput, _limits: &Limits) -> Shaped {
    Shaped {
        json: serde_json::Value::Null,
        truncated: Some("not built yet".to_string()),
    }
}
