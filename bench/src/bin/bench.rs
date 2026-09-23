//! `bench <smoke|clickbench|otel> [options]`: runs the ClickBench-subset and OTel-shaped
//! query suites over deterministic generated data (or real parquet, for clickbench) and
//! prints a markdown report. Args are parsed by hand; no clap.
#![deny(unsafe_code)]

use adelie_bench::{DuckDb, data, suite};
use adelie_harness::engine::{Engine, EngineError, Outcome};

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("smoke") => smoke(),
        Some("clickbench") => clickbench(args.collect()),
        Some("otel") => otel_cmd(args.collect()),
        other => {
            eprintln!("usage: bench <smoke|clickbench|otel> [options]");
            eprintln!("unknown subcommand: {other:?}");
            std::process::exit(2);
        }
    }
}

struct Args {
    rows: Option<usize>,
    spans: Option<usize>,
    runs: usize,
    seed: u64,
    hits_path: Option<String>,
}

fn parse_args(raw: Vec<String>, default_runs: usize, default_seed: u64) -> Args {
    let mut a = Args {
        rows: None,
        spans: None,
        runs: default_runs,
        seed: default_seed,
        hits_path: None,
    };
    let mut it = raw.into_iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--rows" => a.rows = Some(parse_value(&mut it, &flag)),
            "--spans" => a.spans = Some(parse_value(&mut it, &flag)),
            "--runs" => a.runs = parse_value(&mut it, &flag),
            "--seed" => a.seed = parse_value(&mut it, &flag),
            "--hits" => a.hits_path = Some(next_value(&mut it, &flag)),
            other => panic!("unknown flag: {other}"),
        }
    }
    a
}

fn next_value(it: &mut impl Iterator<Item = String>, flag: &str) -> String {
    it.next().unwrap_or_else(|| panic!("{flag}: missing value"))
}

fn parse_value<T: std::str::FromStr>(it: &mut impl Iterator<Item = String>, flag: &str) -> T
where
    T::Err: std::fmt::Display,
{
    let raw = next_value(it, flag);
    raw.parse().unwrap_or_else(|e| panic!("{flag} {raw}: {e}"))
}

/// hits 2_000 rows, otel 1_000 spans, one run, every query in both suites against DuckDB.
/// Exits 1 if any query errors or returns zero rows: on generated data that means the
/// generator or the query is wrong.
fn smoke() {
    let mut db = DuckDb::new().unwrap_or_else(|e| panic!("opening duckdb: {e}"));
    suite::run_setup(&mut db, "otel");

    let hits = data::hits(2_000, 1);
    data::load(&mut db, &hits).unwrap_or_else(|e| panic!("loading hits: {e}"));
    let (spans, logs) = data::otel(1_000, 1);
    data::load(&mut db, &spans).unwrap_or_else(|e| panic!("loading otel.spans: {e}"));
    data::load(&mut db, &logs).unwrap_or_else(|e| panic!("loading otel.logs: {e}"));

    let mut failed = false;
    for name in ["clickbench", "otel"] {
        for q in suite::load_queries(name) {
            match db.run(&q.sql) {
                Ok(Outcome::Rows(rows)) if rows.is_empty() => {
                    eprintln!("smoke: {name}/{} returned zero rows", q.name);
                    failed = true;
                }
                Ok(_) => println!("ok  {name}/{}", q.name),
                Err(e) => {
                    eprintln!("smoke: {name}/{}: {e}", q.name);
                    failed = true;
                }
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
}

fn clickbench(raw: Vec<String>) {
    let args = parse_args(raw, 3, 42);
    let mut db = DuckDb::new().unwrap_or_else(|e| panic!("opening duckdb: {e}"));
    let rows = match &args.hits_path {
        Some(path) => load_hits_from_parquet(&mut db, path),
        None => {
            let rows = args.rows.unwrap_or(1_000_000);
            let table = data::hits(rows, args.seed);
            data::load(&mut db, &table).unwrap_or_else(|e| panic!("loading hits: {e}"));
            rows
        }
    };
    let mut engine: &mut dyn Engine = &mut db;
    let results = suite::run_suite(std::slice::from_mut(&mut engine), "clickbench", args.runs);
    println!("# clickbench ({rows} rows, {} runs)\n", args.runs);
    println!("{}", suite::markdown(&results));
}

/// `CREATE TABLE hits AS SELECT <subset columns> FROM read_parquet(path)`: real ClickBench
/// data answers the same queries as the generated table, since the columns match by name.
fn load_hits_from_parquet(db: &mut DuckDb, path: &str) -> usize {
    let cols: Vec<String> = data::hits_columns()
        .into_iter()
        .map(|(n, _)| quote_ident(&n))
        .collect();
    let escaped_path = path.replace('\'', "''");
    let sql = format!(
        "CREATE TABLE hits AS SELECT {} FROM read_parquet('{escaped_path}')",
        cols.join(", ")
    );
    db.run(&sql)
        .unwrap_or_else(|e| panic!("loading {path}: {e}"));
    let Outcome::Rows(rows) = db
        .run("SELECT count(*) FROM hits")
        .unwrap_or_else(|e| panic!("counting hits: {e}"))
    else {
        panic!("count(*) did not return rows")
    };
    row_count(&rows)
}

fn row_count(rows: &[Vec<adelie_harness::engine::Value>]) -> usize {
    match rows.first().and_then(|r| r.first()) {
        Some(adelie_harness::engine::Value::Int(n)) => *n as usize,
        _ => panic!("count(*) did not return an integer"),
    }
}

fn otel_cmd(raw: Vec<String>) {
    let args = parse_args(raw, 3, 42);
    let spans_n = args.spans.unwrap_or(1_000_000);
    let mut db = DuckDb::new().unwrap_or_else(|e| panic!("opening duckdb: {e}"));
    suite::run_setup(&mut db, "otel");

    let (spans, logs) = data::otel(spans_n, args.seed);
    let span_secs = load_timed(&mut db, &spans);
    let log_secs = load_timed(&mut db, &logs);

    println!("# otel ingest\n");
    println!("| table | rows | seconds | rows/s |");
    println!("|---|---|---|---|");
    print_ingest_row("otel.spans", spans.rows.len(), span_secs);
    print_ingest_row("otel.logs", logs.rows.len(), log_secs);
    println!();

    let mut engine: &mut dyn Engine = &mut db;
    let results = suite::run_suite(std::slice::from_mut(&mut engine), "otel", args.runs);
    println!("# otel queries ({spans_n} spans, {} runs)\n", args.runs);
    println!("{}", suite::markdown(&results));
}

fn load_timed(db: &mut DuckDb, t: &data::Table) -> f64 {
    let elapsed: Result<std::time::Duration, EngineError> = data::load(db, t);
    elapsed
        .unwrap_or_else(|e| panic!("loading {}: {e}", t.name))
        .as_secs_f64()
}

fn print_ingest_row(name: &str, rows: usize, secs: f64) {
    let rate = if secs > 0.0 {
        rows as f64 / secs
    } else {
        f64::INFINITY
    };
    println!("| {name} | {rows} | {secs:.3} | {rate:.0} |");
}

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
