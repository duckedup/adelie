//! The routes and their limits: `/health`, `/query` (guarded) and the `/mcp` mount.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use crate::exec::ExecOptions;
use crate::mcp::{AdelieMcp, Guardrails};
use crate::sql::{self, SqlError};
use crate::surface::{self, Handle};

/// Largest request body: 413 beyond it.
const BODY_LIMIT: usize = 1 << 20;

struct App {
    handle: Arc<Handle>,
    limits: surface::Limits,
    timeout: Duration,
    memory_limit: usize,
    threads: usize,
    /// Shared with the MCP tools: one permit per morsel thread, held until the query finishes.
    permits: Arc<Semaphore>,
}

/// `loopback`: the server is bound to a loopback address, so every route refuses a `Host`
/// that is not loopback (a DNS-rebinding page cannot reach it). Bound elsewhere, the operator
/// chose to expose it and no `Host` is refused.
pub fn router(handle: Arc<Handle>, guard: Guardrails, loopback: bool) -> Router {
    let threads = ExecOptions::default().threads.max(1);
    let limits = surface::Limits {
        max_rows: guard.limits.max_rows,
        max_bytes: guard.limits.max_bytes,
    };
    let (timeout, memory_limit) = (guard.timeout, guard.memory_limit);
    let mcp = AdelieMcp::new(handle.clone(), guard);
    let app = Arc::new(App {
        handle,
        limits,
        timeout,
        memory_limit,
        threads,
        permits: mcp.permits(),
    });
    let router = Router::new()
        .route("/health", get(health))
        .route("/query", post(query))
        .with_state(app)
        .merge(mcp_routes(mcp))
        .layer(DefaultBodyLimit::max(BODY_LIMIT));
    if loopback {
        router.layer(middleware::from_fn(loopback_host))
    } else {
        router
    }
}

/// 403 unless the `Host` header names a loopback host (any port).
async fn loopback_host(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if is_loopback_host(host) {
        next.run(req).await
    } else {
        error(
            StatusCode::FORBIDDEN,
            format!("host {host:?} is not allowed"),
        )
    }
}

fn is_loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(h, _)| h),
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

#[derive(Deserialize)]
struct QueryRequest {
    sql: String,
}

fn error(status: StatusCode, msg: impl ToString) -> Response {
    (status, Json(json!({ "error": msg.to_string() }))).into_response()
}

async fn query(
    State(app): State<Arc<App>>,
    body: Result<Json<QueryRequest>, JsonRejection>,
) -> Response {
    let req = match body {
        Ok(Json(req)) => req,
        Err(e) => return error(e.status(), e.body_text()),
    };
    let Ok(permit) = Arc::clone(&app.permits).acquire_owned().await else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "server is shutting down");
    };
    let job = Arc::clone(&app);
    // The write is acknowledged only after `run` returns, and `Store` flushes before that. The
    // permit moves into the task, so a client that hangs up does not free it early.
    let ran = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let opts = sql::Options {
            exec: ExecOptions {
                memory_limit: job.memory_limit,
                threads: job.threads,
                timeout: Some(job.timeout),
                ..ExecOptions::default()
            },
            ..sql::Options::default()
        };
        job.handle
            .run(&req.sql, &opts)
            .map(|out| surface::shape(&out, &job.limits))
    })
    .await;
    match ran {
        Ok(Ok(shaped)) => {
            let mut body = shaped.json;
            if let (Some(note), Some(obj)) = (shaped.truncated, body.as_object_mut()) {
                obj.insert("truncated".to_string(), Value::String(note));
            }
            Json(body).into_response()
        }
        Ok(Err(e)) => {
            let status = match &e {
                SqlError::ReadOnly(_) => StatusCode::FORBIDDEN,
                SqlError::Io(_) | SqlError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
                _ => StatusCode::BAD_REQUEST,
            };
            error(status, e)
        }
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("query task failed: {e}"),
        ),
    }
}

/// `/mcp`: rmcp's streamable HTTP service over `AdelieMcp`. rmcp checks the `Host` header itself
/// (loopback only by default).
fn mcp_routes(mcp: AdelieMcp) -> Router {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };

    let service = Arc::new(StreamableHttpService::new(
        move || Ok(mcp.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    ));
    let serve = move |req: Request| {
        let service = Arc::clone(&service);
        async move { service.handle(req).await.map(Body::new) }
    };
    Router::new().route("/mcp", any(serve))
}
