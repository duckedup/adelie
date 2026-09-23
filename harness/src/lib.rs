//! Test harness for adelie: sqllogictest runner, engine adapter, crash harness.
//! Never published; a dev-dependency of the root crate and a path dependency of `bench/`.
#![deny(unsafe_code)]

pub mod civil;
pub mod crash;
pub mod engine;
pub mod fake;
pub mod rng;
pub mod slt;

pub use engine::{Engine, EngineError, Outcome, Value};
