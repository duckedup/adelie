//! `differential [paths…]`: runs the `tests/slt` corpus (or given files/dirs) against a fresh
//! `DuckDb` and a fresh `Adelie` through the hand-rolled slt runner, then diffs the two engines
//! against each other. Exits 1 on any of the three failing, or on zero files.
#![deny(unsafe_code)]

use std::path::{Path, PathBuf};

use adelie_bench::{Adelie, DuckDb};
use adelie_harness::slt::{self, FileError, Report};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let roots: Vec<PathBuf> = if args.is_empty() {
        vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/slt")]
    } else {
        args.into_iter().map(PathBuf::from).collect()
    };

    let mut files = Vec::new();
    for root in &roots {
        collect_slt_files(root, &mut files);
    }
    files.sort();

    if files.is_empty() {
        eprintln!("differential: no .slt files found under {roots:?}");
        std::process::exit(1);
    }

    let mut any_failed = false;
    for path in &files {
        let mut duck = DuckDb::new().unwrap_or_else(|e| panic!("opening duckdb: {e}"));
        if !report_file_run("duckdb", path, slt::run_file(&mut duck, path)) {
            any_failed = true;
        }

        let mut adelie = Adelie::new().unwrap_or_else(|e| panic!("opening adelie: {e}"));
        if !report_file_run("adelie", path, slt::run_file(&mut adelie, path)) {
            any_failed = true;
        }

        if !diff_file(path) {
            any_failed = true;
        }
    }

    if any_failed {
        std::process::exit(1);
    }
}

/// Parses and diffs one file's records across a fresh `DuckDb`/`Adelie` pair (contract C6).
/// Prints one line and returns whether it passed; a parse failure counts as a failure too.
fn diff_file(path: &Path) -> bool {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            println!("FAIL diff  {}  ({e})", path.display());
            return false;
        }
    };
    let records = match slt::parse(&src) {
        Ok(r) => r,
        Err(e) => {
            println!("FAIL diff  {}  ({e})", path.display());
            return false;
        }
    };
    let mut duck = DuckDb::new().unwrap_or_else(|e| panic!("opening duckdb: {e}"));
    let mut adelie = Adelie::new().unwrap_or_else(|e| panic!("opening adelie: {e}"));
    let report = slt::diff(&mut duck, &mut adelie, &records);
    print_report("diff", path, &report);
    report.ok()
}

/// Prints one line for a `run_file` result, engine-prefixed, and returns whether it passed.
fn report_file_run(engine: &str, path: &Path, result: Result<Report, FileError>) -> bool {
    match result {
        Ok(report) => {
            print_report(engine, path, &report);
            report.ok()
        }
        Err(e) => {
            println!("FAIL {engine}  {}  ({e})", path.display());
            false
        }
    }
}

fn print_report(engine: &str, path: &Path, report: &Report) {
    if report.ok() {
        println!(
            "ok  {engine}  {}  (passed {}, skipped {})",
            path.display(),
            report.passed,
            report.skipped
        );
        return;
    }
    println!("FAIL {engine}  {}", path.display());
    for f in &report.failures {
        println!("  line {}: {}", f.line, f.sql);
        println!("    expected: {}", f.expected);
        println!("    actual:   {}", f.actual);
    }
}

/// Recurses into directories collecting `*.slt` files; a plain file path is taken as-is.
fn collect_slt_files(root: &Path, out: &mut Vec<PathBuf>) {
    if root.is_file() {
        if root.extension().is_some_and(|e| e == "slt") {
            out.push(root.to_path_buf());
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_slt_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "slt") {
            out.push(path);
        }
    }
}
