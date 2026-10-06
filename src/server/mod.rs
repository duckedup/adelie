//! The HTTP surface: `POST /query`, `GET /health` and `/mcp`.

mod routes;

use std::error::Error;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use crate::mcp;
use crate::surface::Handle;

pub struct ServeConfig {
    pub dir: PathBuf,
    pub listen: SocketAddr,
    pub guard: mcp::Guardrails,
}

pub fn run(cfg: ServeConfig) -> Result<(), Box<dyn Error + Send + Sync>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve(cfg))
}

async fn serve(cfg: ServeConfig) -> Result<(), Box<dyn Error + Send + Sync>> {
    let handle = Arc::new(Handle::open(&cfg.dir, cfg.guard.allow_writes)?);
    let listener = tokio::net::TcpListener::bind(cfg.listen).await?;
    let loopback = cfg.listen.ip().is_loopback();
    let app = routes::router(handle.clone(), cfg.guard, loopback);
    let mut out = std::io::stdout().lock();
    writeln!(out, "listening on http://{}", listener.local_addr()?)?;
    out.flush()?;
    drop(out);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    // Close a writer so its flush errors are not lost; a straggling request just drops it.
    if let Ok(Handle::Write(store)) = Arc::try_unwrap(handle) {
        store.close()?;
    }
    Ok(())
}
