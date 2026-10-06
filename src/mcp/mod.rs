//! The MCP surface: guardrails and an rmcp server over one store.

use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::surface;

/// What an MCP caller may do and how much it may get back.
pub struct Guardrails {
    pub allow_writes: bool,
    pub limits: surface::Limits,
    pub timeout: Duration,
    pub memory_limit: usize,
}

#[derive(Clone)]
pub struct AdelieMcp {
    #[allow(dead_code)] // read once the rmcp handler lands
    handle: Arc<surface::Handle>,
}

impl AdelieMcp {
    pub fn new(handle: Arc<surface::Handle>, _guard: Guardrails) -> Self {
        AdelieMcp { handle }
    }
}

pub fn run_stdio(_dir: &Path, _guard: Guardrails) -> Result<(), Box<dyn Error + Send + Sync>> {
    Err("not built yet".into())
}
