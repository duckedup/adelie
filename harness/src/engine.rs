//! The `Engine` trait every harness (slt, differential, crash) runs SQL through.

use std::fmt;

/// A single cell value, engine-agnostic.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL NULL.
    Null,
    Bool(bool),
    Int(i64),
    /// An integer too wide for `i64` unsigned, e.g. DuckDB's `UBIGINT`.
    UInt(u64),
    Float(f64),
    /// An exact fixed-point value; `value` × 10^-`scale`.
    Decimal {
        value: i128,
        scale: u8,
    },
    Text(String),
    /// Opaque bytes, e.g. a `BLOB` column.
    Bytes(Vec<u8>),
    /// Nanoseconds since the epoch, UTC.
    Timestamp(i64),
    /// Days since the epoch.
    Date(i32),
    /// A 16-byte UUID.
    Uuid([u8; 16]),
    /// An IPv4 or IPv6 address.
    Ip(std::net::IpAddr),
    /// A nested list of values, possibly of mixed kinds.
    List(Vec<Value>),
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
