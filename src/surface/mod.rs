//! Shared plumbing for the CLI, MCP and HTTP surfaces: one handle, JSON values, shaped output.

mod handle;
mod json;

pub use handle::Handle;
pub use json::{Limits, Shaped, shape, value_json};
