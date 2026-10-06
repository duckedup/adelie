//! The MCP surface: guardrails and an rmcp server over one store.

mod guard;
mod tools;

use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt;
use tokio::sync::Semaphore;

use crate::surface;

/// What an MCP caller may do and how much it may get back.
pub struct Guardrails {
    pub allow_writes: bool,
    pub limits: surface::Limits,
    pub timeout: Duration,
    pub memory_limit: usize,
}

/// The four tools over one handle; clones share it.
#[derive(Clone)]
pub struct AdelieMcp {
    handle: Arc<surface::Handle>,
    guard: Arc<Guardrails>,
    /// One permit per morsel thread, held by each running query until it finishes: excess
    /// calls queue instead of oversubscribing the scheduler. `adelie serve` shares it with
    /// `/query`.
    permits: Arc<Semaphore>,
}

impl AdelieMcp {
    pub fn new(handle: Arc<surface::Handle>, guard: Guardrails) -> Self {
        let threads = crate::exec::ExecOptions::default().threads.max(1);
        AdelieMcp {
            handle,
            guard: Arc::new(guard),
            permits: Arc::new(Semaphore::new(threads)),
        }
    }

    /// The query permits, so another route can share the same bound.
    pub fn permits(&self) -> Arc<Semaphore> {
        Arc::clone(&self.permits)
    }
}

/// Serves MCP on stdin/stdout until the client closes it. Only protocol goes to stdout.
pub fn run_stdio(dir: &Path, guard: Guardrails) -> Result<(), Box<dyn Error + Send + Sync>> {
    let handle = surface::Handle::open(dir, guard.allow_writes).map_err(|e| e.to_string())?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let running = AdelieMcp::new(Arc::new(handle), guard)
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|e| e.to_string())?;
        running.waiting().await?;
        Ok(())
    })
}
