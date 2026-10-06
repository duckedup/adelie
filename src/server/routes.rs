//! The routes and their limits: `/health`, `/query` (guarded) and the `/mcp` mount.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::StatusCode;
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
    /// One permit per morsel thread: excess `/query` requests queue here.
    permits: Semaphore,
}

pub fn router(handle: Arc<Handle>, guard: Guardrails) -> Router {
    let threads = ExecOptions::default().threads.max(1);
    let app = Arc::new(App {
        handle: handle.clone(),
        limits: surface::Limits {
            max_rows: guard.limits.max_rows,
            max_bytes: guard.limits.max_bytes,
        },
        timeout: guard.timeout,
        memory_limit: guard.memory_limit,
        threads,
        permits: Semaphore::new(threads),
    });
    Router::new()
        .route("/health", get(health))
        .route("/query", post(query))
        .with_state(app)
        .merge(mcp_routes(handle, guard))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
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

async fn query(State(app): State<Arc<App>>, Json(req): Json<QueryRequest>) -> Response {
    let Ok(permit) = app.permits.acquire().await else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "server is shutting down");
    };
    let job = Arc::clone(&app);
    // The write is acknowledged only after `run` returns, and `Store` flushes before that.
    let ran = tokio::task::spawn_blocking(move || {
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
    drop(permit);
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

/// `/mcp`: rmcp's streamable HTTP service over `AdelieMcp`. Unverified until `AdelieMcp`
/// implements `ServerHandler` (src/mcp) and the two halves are merged.
fn mcp_routes(handle: Arc<Handle>, guard: Guardrails) -> Router {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };

    let mcp = AdelieMcp::new(handle, guard);
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
