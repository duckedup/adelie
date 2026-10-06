//! adelie: a pure-Rust columnar analytics store with SQL.
#![deny(unsafe_code)]

#[cfg(feature = "cli")]
pub mod cli;
pub mod exec;
#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "serve")]
pub mod server;
pub mod sql;
pub mod storage;
#[cfg(any(feature = "cli", feature = "mcp", feature = "serve"))]
pub mod surface;
pub mod types;
