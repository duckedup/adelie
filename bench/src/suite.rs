//! Loads a query suite from `bench/queries/<suite>/*.sql` and runs it against one or more
//! engines, cross-checking result rows and reporting per-query medians.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use adelie_harness::engine::{Engine, EngineError, Outcome, Value};

/// One query file: its filename stem, its `-- <description>` first line, and its SQL body.
pub struct Query {
    pub name: String,
    pub description: String,
    pub sql: String,
}

/// One query's outcome: a median duration per engine, and any cross-engine mismatch found.
pub struct QueryResult {
    pub name: String,
    pub description: String,
    pub timings: Vec<(String, Duration)>,
    pub mismatch: Option<String>,
}

fn queries_dir(suite: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("queries").join(suite)
}

/// Reads `<suite>/setup.sql`, if present, as raw text for the caller to split and run.
pub fn setup_sql(suite: &str) -> Option<String> {
    std::fs::read_to_string(queries_dir(suite).join("setup.sql")).ok()
}

/// Loads every `*.sql` file in `<suite>` except `setup.sql`, in filename order.
pub fn load_queries(suite: &str) -> Vec<Query> {
    let dir = queries_dir(suite);
    let entries = std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "sql") && p.file_name().and_then(|n| n.to_str()) != Some("setup.sql")
        })
        .collect();
    paths.sort();
    paths.iter().map(|p| load_query(p)).collect()
}

fn load_query(path: &Path) -> Query {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let mut lines = text.lines();
    let description = lines.next().unwrap_or_default().strip_prefix("--").unwrap_or_default().trim().to_string();
    let sql = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
    Query { name, description, sql }
}

/// Runs `<suite>/setup.sql` on one engine, split on `;`. A no-op when the suite has none.
pub fn run_setup(engine: &mut dyn Engine, suite: &str) {
    let Some(setup) = setup_sql(suite) else { return };
    for stmt in setup.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        engine.run(stmt).unwrap_or_else(|e| panic!("{suite} setup `{stmt}`: {e}"));
    }
}

/// Runs `setup.sql` (if any) on every engine, then every query in `suite`: one untimed
/// warm-up per engine, then `runs` timed runs whose median is kept, with a cross-engine
/// row check when there are 2+ engines.
pub fn run_suite(engines: &mut [&mut dyn Engine], suite: &str, runs: usize) -> Vec<QueryResult> {
    for engine in engines.iter_mut() {
        run_setup(*engine, suite);
    }
    load_queries(suite).iter().map(|q| run_query(engines, q, runs)).collect()
}

fn run_query(engines: &mut [&mut dyn Engine], q: &Query, runs: usize) -> QueryResult {
    let mut timings = Vec::new();
    let mut outcomes = Vec::new();
    for engine in engines.iter_mut() {
        let _ = engine.run(&q.sql); // untimed warm-up
        let mut samples = Vec::with_capacity(runs);
        let mut last = None;
        for _ in 0..runs.max(1) {
            let start = Instant::now();
            let result = engine.run(&q.sql);
            if result.is_ok() {
                samples.push(start.elapsed());
            }
            last = Some(result);
        }
        let last = last.expect("runs.max(1) guarantees at least one iteration");
        if let Some(median) = median(&mut samples) {
            timings.push((engine.name().to_string(), median));
        }
        outcomes.push((engine.name().to_string(), last));
    }
    let mismatch = cross_check(&outcomes);
    QueryResult { name: q.name.clone(), description: q.description.clone(), timings, mismatch }
}

fn median(samples: &mut [Duration]) -> Option<Duration> {
    if samples.is_empty() {
        return None;
    }
    samples.sort();
    Some(samples[samples.len() / 2])
}

/// Compares each engine's last outcome for a query, rendering rows as the harness would
/// under all-`T` (text) column types after sorting, so row order never causes a mismatch.
fn cross_check(outcomes: &[(String, Result<Outcome, EngineError>)]) -> Option<String> {
    if outcomes.len() < 2 {
        return None;
    }
    let mut rendered = Vec::with_capacity(outcomes.len());
    for (name, outcome) in outcomes {
        match outcome {
            Err(e) => return Some(format!("{name}: error: {e}")),
            Ok(Outcome::Statement) => rendered.push((name, vec!["<statement>".to_string()])),
            Ok(Outcome::Rows(rows)) => {
                let mut lines: Vec<String> = rows.iter().map(|r| render_row(r)).collect();
                lines.sort();
                rendered.push((name, lines));
            }
        }
    }
    let (first_name, first_lines) = &rendered[0];
    rendered[1..]
        .iter()
        .find(|(_, lines)| lines != first_lines)
        .map(|(name, lines)| format!("{first_name} ({} rows) vs {name} ({} rows)", first_lines.len(), lines.len()))
}

fn render_row(row: &[Value]) -> String {
    row.iter().map(render_value).collect::<Vec<_>>().join(" ")
}

fn render_value(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) if f.is_nan() => "NaN".to_string(),
        Value::Float(f) if f.is_infinite() => if *f > 0.0 { "inf" } else { "-inf" }.to_string(),
        Value::Float(f) => format!("{f}"),
        Value::Text(s) if s.is_empty() => "(empty)".to_string(),
        Value::Text(s) => s.clone(),
    }
}

/// A table with query, description, one median-ms column per engine, and `loss` (slowest
/// engine's median ÷ fastest's, per query) — losses are printed, never hidden (SPEC §13).
pub fn markdown(results: &[QueryResult]) -> String {
    let mut names: Vec<String> = Vec::new();
    for r in results {
        for (n, _) in &r.timings {
            if !names.contains(n) {
                names.push(n.clone());
            }
        }
    }
    let mut out = String::from("| query | description |");
    for n in &names {
        out.push_str(&format!(" {n} (ms) |"));
    }
    out.push_str(" loss | notes |\n|---|---|");
    out.push_str(&"---|".repeat(names.len()));
    out.push_str("---|---|\n");
    for r in results {
        out.push_str(&format!("| {} | {} |", r.name, r.description));
        let (mut fastest, mut slowest) = (f64::INFINITY, 0.0_f64);
        for n in &names {
            match r.timings.iter().find(|(tn, _)| tn == n) {
                Some((_, d)) => {
                    let ms = d.as_secs_f64() * 1000.0;
                    fastest = fastest.min(ms);
                    slowest = slowest.max(ms);
                    out.push_str(&format!(" {ms:.2} |"));
                }
                None => out.push_str(" - |"),
            }
        }
        let loss = if fastest.is_finite() && fastest > 0.0 { slowest / fastest } else { f64::NAN };
        out.push_str(&format!(" {loss:.2} | {} |\n", r.mismatch.as_deref().unwrap_or("")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_query_splits_description_from_sql() {
        let dir = std::env::temp_dir().join(format!("adelie-bench-suite-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("00-count.sql");
        std::fs::write(&path, "-- ClickBench Q0: row count\nSELECT count(*) FROM hits\n").unwrap();
        let q = load_query(&path);
        assert_eq!(q.description, "ClickBench Q0: row count");
        assert_eq!(q.sql, "SELECT count(*) FROM hits");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn median_of_odd_count_is_the_middle() {
        let mut samples = vec![Duration::from_millis(3), Duration::from_millis(1), Duration::from_millis(2)];
        assert_eq!(median(&mut samples), Some(Duration::from_millis(2)));
    }

    #[test]
    fn cross_check_flags_a_differing_row() {
        let a = Ok(Outcome::Rows(vec![vec![Value::Int(1)]]));
        let b = Ok(Outcome::Rows(vec![vec![Value::Int(2)]]));
        let outcomes = vec![("a".to_string(), a), ("b".to_string(), b)];
        assert!(cross_check(&outcomes).is_some());
    }

    #[test]
    fn cross_check_ignores_row_order() {
        let a = Ok(Outcome::Rows(vec![vec![Value::Int(1)], vec![Value::Int(2)]]));
        let b = Ok(Outcome::Rows(vec![vec![Value::Int(2)], vec![Value::Int(1)]]));
        let outcomes = vec![("a".to_string(), a), ("b".to_string(), b)];
        assert!(cross_check(&outcomes).is_none());
    }
}
