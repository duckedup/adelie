//! DuckDB and adelie engine adapters, deterministic data generators, and query-suite runner
//! shared by the `differential` and `bench` binaries. Its own Cargo workspace (D0006): DuckDB
//! never reaches the root lockfile or build budget; adelie is a path dependency of bench only.
#![deny(unsafe_code)]

pub mod adelie;
pub mod data;
pub mod duck;
pub mod suite;

pub use adelie::Adelie;
pub use duck::DuckDb;
