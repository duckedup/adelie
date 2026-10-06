//! The HTTP surface: `POST /query`, `GET /health` and `/mcp`.

use std::error::Error;
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::mcp;

pub struct ServeConfig {
    pub dir: PathBuf,
    pub listen: SocketAddr,
    pub guard: mcp::Guardrails,
}

pub fn run(_cfg: ServeConfig) -> Result<(), Box<dyn Error + Send + Sync>> {
    Err("not built yet".into())
}
