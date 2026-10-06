//! The four v1 tools. Every failure is a tool error the caller reads, never a dropped call.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::schemars::JsonSchema;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Value as Json, json};
use std::sync::Arc;
use tokio::sync::Semaphore;

use super::{AdelieMcp, guard};
use crate::storage::TableSummary;
use crate::surface::{self, Shaped};

const INSTRUCTIONS: &str = "adelie columnar analytics store. Call list_tables first, then \
describe_table for a table's columns, then sql to query. sample previews a table's rows. \
Results are capped; a `truncated` note says when rows were cut.";

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct DescribeArgs {
    /// Table name, optionally `db.name`.
    table: String,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct SampleArgs {
    /// Table name, optionally `db.name`.
    table: String,
    /// Rows to return; defaults to 10 and is capped by the server's row limit.
    n: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct SqlArgs {
    /// One or more SQL statements. Writes need the server started with --allow-writes.
    query: String,
}

fn ok(json: &Json) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(json.to_string())])
}

fn fail(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg)])
}

/// Runs `f` off the async threads holding one of `permits` until `f` returns (not until the
/// caller stops waiting); a panic in it becomes an error string.
async fn blocking<T: Send + 'static>(
    permits: &Arc<Semaphore>,
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let permit = Arc::clone(permits)
        .acquire_owned()
        .await
        .map_err(|_| "server is shutting down".to_string())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f()
    })
    .await
    .map_err(|e| format!("tool failed: {e}"))?
}

/// `cut` is a note from before the query ran (a sample clamped to the row cap); a note from
/// shaping the result wins over it.
fn shaped_result(shaped: Shaped, cut: Option<String>) -> CallToolResult {
    let mut json = shaped.json;
    if let (Some(note), Some(obj)) = (shaped.truncated.or(cut), json.as_object_mut()) {
        obj.insert("truncated".to_string(), Json::String(note));
    }
    ok(&json)
}

/// `wanted` is a bare name or `db.name`; unknown names get the closest real ones.
fn find_table<'a>(tables: &'a [TableSummary], wanted: &str) -> Result<&'a TableSummary, String> {
    let hit = tables
        .iter()
        .find(|t| t.name.to_string() == wanted)
        .or_else(|| tables.iter().find(|t| t.name.name == wanted));
    if let Some(t) = hit {
        return Ok(t);
    }
    let hint = crate::sql::hint(wanted, tables.iter().map(|t| t.name.name.clone()));
    Err(if hint.is_empty() {
        format!("unknown table \"{wanted}\"; call list_tables to see what exists")
    } else {
        format!("unknown table \"{wanted}\"{hint}")
    })
}

impl AdelieMcp {
    /// Runs `query` under the guardrails; `gate` applies the read-only check first.
    async fn run_query(&self, query: String, gate: bool, cut: Option<String>) -> CallToolResult {
        let (handle, guard) = (self.handle.clone(), self.guard.clone());
        let shaped = blocking(&self.permits, move || {
            if gate {
                guard::check_sql(&guard, &query)?;
            }
            let out = handle
                .run(&query, &guard::options(&guard))
                .map_err(|e| e.to_string())?;
            Ok(surface::shape(&out, &guard.limits))
        })
        .await;
        match shaped {
            Ok(shaped) => shaped_result(shaped, cut),
            Err(msg) => fail(msg),
        }
    }

    async fn tables(&self) -> Result<Vec<TableSummary>, String> {
        let handle = self.handle.clone();
        blocking(&self.permits, move || {
            Ok(handle.view().map_err(|e| e.to_string())?.tables())
        })
        .await
    }
}

#[tool_router]
impl AdelieMcp {
    #[tool(description = "List every table with its engine, row count and time range.")]
    async fn list_tables(&self) -> CallToolResult {
        let tables = match self.tables().await {
            Ok(t) => t,
            Err(msg) => return fail(msg),
        };
        let rows: Vec<Json> = tables
            .iter()
            .map(|t| {
                let time_range = t.time_range.as_ref().map(|r| {
                    json!({
                        "column": r.column,
                        "min": surface::value_json(&r.min),
                        "max": surface::value_json(&r.max),
                    })
                });
                json!({
                    "name": t.name.name,
                    "db": t.name.db,
                    "engine": t.engine,
                    "rows": t.rows,
                    "rows_exact": t.rows_exact,
                    "time_range": time_range,
                })
            })
            .collect();
        ok(&Json::Array(rows))
    }

    #[tool(
        description = "Describe a table: each column's name, type and fraction of rows with a value."
    )]
    async fn describe_table(&self, Parameters(args): Parameters<DescribeArgs>) -> CallToolResult {
        let tables = match self.tables().await {
            Ok(t) => t,
            Err(msg) => return fail(msg),
        };
        let table = match find_table(&tables, &args.table) {
            Ok(t) => t,
            Err(msg) => return fail(msg),
        };
        let columns: Vec<Json> = table
            .columns
            .iter()
            .map(|c| {
                let present = if table.rows == 0 {
                    0.0
                } else {
                    (c.non_null as f64 / table.rows as f64).min(1.0)
                };
                json!({ "name": c.name, "type": c.ty.to_string(), "present_fraction": present })
            })
            .collect();
        ok(&json!({ "columns": columns }))
    }

    #[tool(description = "Preview up to n rows of a table (default 10).")]
    async fn sample(&self, Parameters(args): Parameters<SampleArgs>) -> CallToolResult {
        let tables = match self.tables().await {
            Ok(t) => t,
            Err(msg) => return fail(msg),
        };
        let table = match find_table(&tables, &args.table) {
            Ok(t) => t,
            Err(msg) => return fail(msg),
        };
        let cap = self.guard.limits.max_rows;
        let n = guard::sample_rows(args.n, cap);
        let cut = args
            .n
            .filter(|&asked| asked > n)
            .map(|asked| format!("asked for {asked} rows; returned at most {n} (row cap {cap})"));
        let query = format!(
            "SELECT * FROM {}.{} LIMIT {n}",
            guard::quote_ident(&table.name.db),
            guard::quote_ident(&table.name.name)
        );
        self.run_query(query, false, cut).await
    }

    #[tool(
        description = "Run SQL. Reads are always allowed; writes only when the server allows them."
    )]
    async fn sql(&self, Parameters(args): Parameters<SqlArgs>) -> CallToolResult {
        self.run_query(args.query, true, None).await
    }
}

#[tool_handler]
impl ServerHandler for AdelieMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("adelie", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
