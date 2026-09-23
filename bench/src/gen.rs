//! Deterministic synthetic data: a ClickBench-shaped `hits` table and an OTel-shaped
//! `spans`/`logs` pair, all reproducible from a seed. `load` inserts either into any `Engine`.

use std::time::{Duration, Instant};

use adelie_harness::engine::{Engine, EngineError, Outcome, Value};
use adelie_harness::rng::SplitMix64;

use crate::duck::civil_from_days;

/// A single table's schema and rows, engine-agnostic.
pub struct Table {
    pub name: String,
    pub columns: Vec<(String, &'static str)>,
    pub rows: Vec<Vec<Value>>,
}

/// The rare token guaranteed to appear in at least one `otel.logs` body per call to `otel`.
pub const LOG_NEEDLE: &str = "zzq_needle_7f3a";

const URL_VOCAB: &[&str] =
    &["/", "/index.html", "/product", "/category", "/search", "/about", "/contact", "/blog", "/cart", "/checkout"];
const TITLE_VOCAB: &[&str] = &[
    "Home", "Product Page", "Category", "Search Results", "About Us", "Contact", "Blog Post", "Cart", "Checkout",
    "Login",
];
const REFERER_VOCAB: &[&str] =
    &["", "https://www.google.com/", "https://www.bing.com/", "https://duckduckgo.com/", "https://example.com/"];
const PHONE_VOCAB: &[&str] = &["", "iPhone", "Galaxy S9", "Pixel 4", "Nokia 3310"];
const SEARCH_PHRASE_VOCAB: &[&str] =
    &["rust programming", "duckdb tutorial", "clickbench", "sql analytics", "columnar database"];
const HOT_COUNTER_IDS: &[i64] = &[1, 2, 3, 5, 8];

const SERVICE_VOCAB: &[&str] =
    &["frontend", "api-gateway", "auth-service", "orders-service", "payments-service", "inventory-service"];
const SPAN_NAME_VOCAB: &[&str] = &["GET /", "POST /orders", "GET /users/{id}", "db query", "cache lookup", "rpc call"];
const ROUTE_VOCAB: &[&str] = &["/orders", "/users", "/payments", "/inventory", "/health"];
const SEVERITY_VOCAB: &[&str] = &["INFO", "WARN", "ERROR", "DEBUG"];
const LOG_TEMPLATES: &[&str] =
    &["request completed id={n}", "cache miss key={n}", "retrying upstream call {n}", "queue depth is {n}"];

/// A ClickBench-shaped `hits` table: a real subset of columns and names, skewed like the
/// original data, so `--hits <parquet>` real data answers the same queries.
pub fn hits(rows: usize, seed: u64) -> Table {
    let mut rng = SplitMix64::new(seed);
    let columns = hits_columns();
    let mut out = Vec::with_capacity(rows);
    for i in 0..rows {
        out.push(hits_row(&mut rng, i as i64));
    }
    Table { name: "hits".to_string(), columns, rows: out }
}

/// The `hits` schema: real ClickBench column names and types, so `--hits <parquet>` real
/// data (same column names) answers the same queries as the generated table.
pub fn hits_columns() -> Vec<(String, &'static str)> {
    [
        ("WatchID", "BIGINT"),
        ("CounterID", "INTEGER"),
        ("EventTime", "TIMESTAMP"),
        ("EventDate", "DATE"),
        ("UserID", "BIGINT"),
        ("RegionID", "INTEGER"),
        ("OS", "SMALLINT"),
        ("UserAgent", "SMALLINT"),
        ("ResolutionWidth", "SMALLINT"),
        ("IsRefresh", "SMALLINT"),
        ("AdvEngineID", "SMALLINT"),
        ("SearchEngineID", "SMALLINT"),
        ("SearchPhrase", "TEXT"),
        ("URL", "TEXT"),
        ("Title", "TEXT"),
        ("Referer", "TEXT"),
        ("MobilePhoneModel", "TEXT"),
    ]
    .into_iter()
    .map(|(n, t)| (n.to_string(), t))
    .collect()
}

fn pick<'a, T>(rng: &mut SplitMix64, vocab: &'a [T]) -> &'a T {
    &vocab[rng.range(0, vocab.len() as u64) as usize]
}

fn hits_row(rng: &mut SplitMix64, i: i64) -> Vec<Value> {
    let counter_id =
        if rng.f64() < 0.6 { *pick(rng, HOT_COUNTER_IDS) } else { rng.range(1, 1000) as i64 };
    let day_offset = rng.range(0, 3650) as i64;
    let seconds = rng.range(0, 86_400) as i64;
    let is_refresh = if rng.f64() < 0.1 { 1 } else { 0 };
    let adv_engine_id = if rng.f64() < 0.95 { 0 } else { rng.range(1, 20) as i64 };
    let search_phrase =
        if rng.f64() < 0.8 { String::new() } else { pick(rng, SEARCH_PHRASE_VOCAB).to_string() };
    vec![
        Value::Int(rng.next_u64() as i64),
        Value::Int(counter_id),
        Value::Text(format_datetime(day_offset, seconds)),
        Value::Text(format_date(day_offset)),
        Value::Int(rng.next_u64() as i64),
        Value::Int(rng.range(1, 200) as i64),
        Value::Int(rng.range(1, 50) as i64),
        Value::Int(rng.range(1, 50) as i64),
        Value::Int(rng.range(320, 3840) as i64),
        Value::Int(is_refresh),
        Value::Int(adv_engine_id),
        Value::Int(rng.range(0, 10) as i64),
        Value::Text(search_phrase),
        Value::Text(format!("{}{i}", pick(rng, URL_VOCAB))),
        Value::Text(pick(rng, TITLE_VOCAB).to_string()),
        Value::Text(pick(rng, REFERER_VOCAB).to_string()),
        Value::Text(pick(rng, PHONE_VOCAB).to_string()),
    ]
}

/// An OTel-shaped `spans`/`logs` pair (SPEC §9 column names), grouped into traces of 3-12
/// spans each in a parent chain/tree, with lognormal-ish durations and a rare log needle.
pub fn otel(spans: usize, seed: u64) -> (Table, Table) {
    let mut rng = SplitMix64::new(seed);
    let mut span_rows = Vec::new();
    let mut log_rows = Vec::new();
    let mut trace_n: u64 = 0;
    let mut needle_used = false;
    while span_rows.len() < spans {
        trace_n += 1;
        let trace_id = format!("{trace_n:032x}");
        let n_spans = rng.range(3, 13) as usize;
        let mut parent_ids: Vec<String> = Vec::new();
        for _ in 0..n_spans {
            if span_rows.len() >= spans {
                break;
            }
            let span_id = format!("{:016x}", span_rows.len() as u64 + 1);
            let parent = if parent_ids.is_empty() {
                Value::Null
            } else {
                Value::Text(pick(&mut rng, &parent_ids).clone())
            };
            let service = pick(&mut rng, SERVICE_VOCAB);
            let day_offset = rng.range(0, 30) as i64;
            let seconds = rng.range(0, 86_400) as i64;
            let mean_ns = 5_000_000.0;
            let duration_ns = (-((1.0 - rng.f64()).ln()) * mean_ns) as i64 + 100_000;
            let status_code = if rng.f64() < 0.05 { 1 } else { 0 };
            span_rows.push(vec![
                Value::Text(trace_id.clone()),
                Value::Text(span_id.clone()),
                parent,
                Value::Text(service.to_string()),
                Value::Text(pick(&mut rng, SPAN_NAME_VOCAB).to_string()),
                Value::Text(format_datetime(day_offset, seconds)),
                Value::Int(duration_ns),
                Value::Int(status_code),
                Value::Text(pick(&mut rng, ROUTE_VOCAB).to_string()),
                Value::Text(format!("run-{}", rng.range(1, 6))),
            ]);
            parent_ids.push(span_id);

            let (log, has_needle) = log_body(&mut rng);
            needle_used |= has_needle;
            log_rows.push(vec![
                Value::Text(trace_id.clone()),
                Value::Text(parent_ids.last().unwrap().clone()),
                Value::Text(service.to_string()),
                Value::Text(format_datetime(day_offset, seconds)),
                Value::Text(pick(&mut rng, SEVERITY_VOCAB).to_string()),
                Value::Text(log),
            ]);
        }
    }
    if !needle_used {
        if let Some(last) = log_rows.last_mut() {
            last[5] = Value::Text(format!("{} {LOG_NEEDLE}", LOG_TEMPLATES[0].replace("{n}", "0")));
        }
    }
    (
        Table { name: "otel.spans".to_string(), columns: otel_spans_columns(), rows: span_rows },
        Table { name: "otel.logs".to_string(), columns: otel_logs_columns(), rows: log_rows },
    )
}

fn log_body(rng: &mut SplitMix64) -> (String, bool) {
    let n = rng.range(0, 1_000_000);
    let template = pick(rng, LOG_TEMPLATES).replace("{n}", &n.to_string());
    if rng.f64() < 0.01 {
        (format!("{template} {LOG_NEEDLE}"), true)
    } else {
        (template, false)
    }
}

fn otel_spans_columns() -> Vec<(String, &'static str)> {
    [
        ("trace_id", "TEXT"),
        ("span_id", "TEXT"),
        ("parent_span_id", "TEXT"),
        ("service.name", "TEXT"),
        ("name", "TEXT"),
        ("start_time", "TIMESTAMP"),
        ("duration_ns", "BIGINT"),
        ("status_code", "INTEGER"),
        ("attributes.http.route", "TEXT"),
        ("resource.run.id", "TEXT"),
    ]
    .into_iter()
    .map(|(n, t)| (n.to_string(), t))
    .collect()
}

fn otel_logs_columns() -> Vec<(String, &'static str)> {
    [
        ("trace_id", "TEXT"),
        ("span_id", "TEXT"),
        ("service.name", "TEXT"),
        ("time", "TIMESTAMP"),
        ("severity_text", "TEXT"),
        ("body", "TEXT"),
    ]
    .into_iter()
    .map(|(n, t)| (n.to_string(), t))
    .collect()
}

fn format_date(day_offset: i64) -> String {
    let (y, m, d) = civil_from_days(day_offset);
    format!("{y:04}-{m:02}-{d:02}")
}

fn format_datetime(day_offset: i64, seconds_of_day: i64) -> String {
    let (y, mo, d) = civil_from_days(day_offset);
    let (h, mi, s) = (seconds_of_day / 3600, (seconds_of_day % 3600) / 60, seconds_of_day % 60);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// A table's own name may be schema-qualified (`otel.spans`); each dotted part is its own
/// identifier. A column name's dot, by contrast, is literal (`"service.name"`, one identifier).
fn quote_table_name(s: &str) -> String {
    s.split('.').map(quote_ident).collect::<Vec<_>>().join(".")
}

fn escape_text(s: &str) -> String {
    s.replace('\'', "''")
}

fn render_literal(v: &Value, sql_type: &str) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Text(s) if sql_type.eq_ignore_ascii_case("TIMESTAMP") => {
            format!("TIMESTAMP '{}'", escape_text(s))
        }
        Value::Text(s) => format!("'{}'", escape_text(s)),
    }
}

/// Loads `t` into `engine`: `CREATE TABLE`, then multi-row `INSERT` in chunks of 1000 rows.
/// Engine-neutral; adelie takes the same path at E6. Returns only the insert time.
pub fn load(engine: &mut dyn Engine, t: &Table) -> Result<Duration, EngineError> {
    let col_defs: Vec<String> = t.columns.iter().map(|(n, ty)| format!("{} {ty}", quote_ident(n))).collect();
    let create = format!("CREATE TABLE {} ({})", quote_table_name(&t.name), col_defs.join(", "));
    run_statement(engine, &create)?;

    let col_names: Vec<String> = t.columns.iter().map(|(n, _)| quote_ident(n)).collect();
    let start = Instant::now();
    for chunk in t.rows.chunks(1000) {
        let rows: Vec<String> = chunk
            .iter()
            .map(|row| {
                let cells: Vec<String> =
                    row.iter().zip(&t.columns).map(|(v, (_, ty))| render_literal(v, ty)).collect();
                format!("({})", cells.join(", "))
            })
            .collect();
        let sql = format!(
            "INSERT INTO {} ({}) VALUES {}",
            quote_table_name(&t.name),
            col_names.join(", "),
            rows.join(", ")
        );
        run_statement(engine, &sql)?;
    }
    Ok(start.elapsed())
}

fn run_statement(engine: &mut dyn Engine, sql: &str) -> Result<(), EngineError> {
    match engine.run(sql)? {
        Outcome::Statement | Outcome::Rows(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_tables() {
        let a = hits(50, 7);
        let b = hits(50, 7);
        assert_eq!(a.rows, b.rows);
    }

    #[test]
    fn hits_has_the_declared_width() {
        let t = hits(100, 1);
        assert_eq!(t.rows.len(), 100);
        assert!(t.rows.iter().all(|r| r.len() == t.columns.len()));
    }

    #[test]
    fn otel_spans_per_trace_in_range() {
        let (spans, _) = otel(500, 3);
        let trace_col = spans.columns.iter().position(|(n, _)| n == "trace_id").unwrap();
        let mut counts = std::collections::HashMap::new();
        for row in &spans.rows {
            let Value::Text(id) = &row[trace_col] else { panic!("trace_id is text") };
            *counts.entry(id.clone()).or_insert(0u32) += 1;
        }
        // The last trace may be cut short by the `spans` budget; check every other one.
        let mut sorted: Vec<_> = counts.into_iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, count) in &sorted[..sorted.len() - 1] {
            assert!((3..=12).contains(count));
        }
    }

    #[test]
    fn every_log_needle_appears_at_least_once() {
        let (_, logs) = otel(1000, 9);
        let body_col = logs.columns.iter().position(|(n, _)| n == "body").unwrap();
        assert!(logs.rows.iter().any(|r| matches!(&r[body_col], Value::Text(s) if s.contains(LOG_NEEDLE))));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // links bundled DuckDB C++
    fn load_round_trip_counts_match() {
        use crate::duck::DuckDb;
        let mut db = DuckDb::new().unwrap();
        let t = hits(50, 4);
        load(&mut db, &t).unwrap();
        let Outcome::Rows(rows) = db.run("SELECT count(*) FROM hits").unwrap() else {
            panic!("expected rows")
        };
        assert_eq!(rows, vec![vec![Value::Int(50)]]);
    }
}
