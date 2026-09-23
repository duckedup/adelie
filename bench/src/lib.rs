//! DuckDB engine adapter, deterministic data generators, and query-suite runner shared by
//! the `differential` and `bench` binaries. Its own Cargo workspace (D0006): DuckDB never
//! reaches the root lockfile or build budget.
#![deny(unsafe_code)]

pub mod data;
pub mod duck;
pub mod suite;

pub use duck::DuckDb;
