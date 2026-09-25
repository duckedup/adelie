//! adelie's SQL front end and engine (SPEC §8): a hand-rolled lexer and recursive-descent
//! parser producing an `ast::Statement`, a binder that gives it types, a rule-based planner
//! that lowers it to `exec::Plan`, and a statement executor over `Store`. `ingest` holds
//! COPY's CSV/NDJSON readers and the row builder INSERT shares with them (D0016).

pub mod ast;
pub mod ingest;
mod lexer;
mod parser;

mod binder;
mod error;
mod execute;
mod planner;
mod result;

pub use parser::{MAX_DEPTH, ParseError, parse};

pub use error::SqlError;
pub use execute::{Options, execute, execute_with};
pub use result::{Rows, SqlOutput};
