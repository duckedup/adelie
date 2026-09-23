//! A hand-rolled sqllogictest dialect: parse a `.slt` file into `Record`s, then run them
//! against one `Engine` or diff them across two.

mod diff;
mod parse;
mod render;
mod run;

use std::fmt;
use std::path::Path;

use crate::engine::Engine;

pub use diff::diff;
pub use parse::parse;
pub use render::render_text;
pub use run::run;

/// One column type a `query` record declares, one char per column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    Int,
    Real,
    Text,
}

/// How a query's rows are compared: in order, sorted whole, or sorted value-by-value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    #[default]
    NoSort,
    RowSort,
    ValueSort,
}

/// A `skipif`/`onlyif` line stacked above a record, matched against `Engine::name()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    SkipIf(String),
    OnlyIf(String),
}

/// What a record expects of the engine, beyond the shared `sql` and `line`.
#[derive(Debug, Clone, PartialEq)]
pub enum Directive {
    StatementOk,
    /// `None` matches any error; `Some(s)` requires a case-insensitive substring match.
    StatementError(Option<String>),
    Query {
        types: Vec<ColType>,
        sort: SortMode,
        label: Option<String>,
        expected: Vec<String>,
    },
    QueryError(Option<String>),
    /// A `N values hashing to <md5>` expected block: unsupported, always a failure.
    QueryHashUnsupported,
    Halt,
}

/// One record parsed from an slt file.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub line: usize,
    pub conditions: Vec<Condition>,
    pub sql: String,
    pub directive: Directive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug)]
pub enum FileError {
    Io(std::io::Error),
    Parse(ParseError),
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileError::Io(e) => write!(f, "{e}"),
            FileError::Parse(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FileError::Io(e) => Some(e),
            FileError::Parse(e) => Some(e),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub line: usize,
    pub sql: String,
    pub expected: String,
    pub actual: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub passed: usize,
    pub skipped: usize,
    pub failures: Vec<Failure>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Reads, parses and runs an slt file against `engine` in one step.
pub fn run_file(engine: &mut dyn Engine, path: &Path) -> Result<Report, FileError> {
    let src = std::fs::read_to_string(path).map_err(FileError::Io)?;
    let records = parse(&src).map_err(FileError::Parse)?;
    Ok(run(engine, &records))
}

/// Collapses whitespace runs to a single space and trims both ends, so a comparison does
/// not care about incidental spacing differences between a `.slt` file and a rendered row.
fn normalize_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Compares two sides of rendered row-lines under a sort mode, after normalizing each line.
fn lines_match(sort: SortMode, expected: &[String], actual: &[String]) -> bool {
    let mut exp: Vec<String> = expected.iter().map(|s| normalize_line(s)).collect();
    let mut act: Vec<String> = actual.iter().map(|s| normalize_line(s)).collect();
    match sort {
        SortMode::NoSort => exp == act,
        SortMode::RowSort => {
            exp.sort();
            act.sort();
            exp == act
        }
        SortMode::ValueSort => {
            let mut ev: Vec<&str> = exp.iter().flat_map(|l| l.split(' ')).collect();
            let mut av: Vec<&str> = act.iter().flat_map(|l| l.split(' ')).collect();
            ev.sort_unstable();
            av.sort_unstable();
            ev == av
        }
    }
}
