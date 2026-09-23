//! adelie: a pure-Rust columnar analytics store with SQL.
#![deny(unsafe_code)]

pub mod exec;
mod fail;
mod io;
pub mod manifest;
pub mod segment;
pub mod store;
pub mod types;
