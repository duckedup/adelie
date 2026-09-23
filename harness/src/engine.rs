//! The `Engine` trait every harness (slt, differential, crash) runs SQL through.

use std::fmt;

/// A single cell value, engine-agnostic.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

/// The result of running one SQL statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// DDL/DML with no result set.
    Statement,
    /// A result set, row-major. Every row has the same width.
    Rows(Vec<Vec<Value>>),
}

/// An engine failure, carrying only the message text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError(pub String);

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for EngineError {}

/// Anything that can run SQL and report what happened: DuckDB, adelie, or a fake.
pub trait Engine {
    /// Stable lowercase name matched by slt `skipif`/`onlyif`: "duckdb", "adelie", "fake".
    fn name(&self) -> &str;
    fn run(&mut self, sql: &str) -> Result<Outcome, EngineError>;
}
