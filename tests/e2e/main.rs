//! The repo's one integration test binary (the test-placement law): every e2e suite is a
//! module here.

mod adelie;
#[cfg(feature = "cli")]
mod cli;
#[cfg(feature = "cli")]
mod common;
mod exec;
mod lifecycle;
#[cfg(feature = "mcp")]
mod mcp;
mod migrate;
mod segment;
#[cfg(feature = "serve")]
mod serve;
mod slt;
mod sql;
mod store;
mod tablespec;
