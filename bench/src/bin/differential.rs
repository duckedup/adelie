//! `differential [paths…]`: runs the `tests/slt` corpus (or given files/dirs) against a
//! fresh `DuckDb` through the hand-rolled slt runner. Exits 1 on any failure or zero files.
#![deny(unsafe_code)]

use std::path::{Path, PathBuf};

use adelie_bench::DuckDb;

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
        // E6: once adelie implements `Engine`, this becomes
        // `adelie_harness::slt::diff(&mut DuckDb::new()?, &mut Adelie::new()?, &records)`.
        let mut engine = DuckDb::new().unwrap_or_else(|e| panic!("opening duckdb: {e}"));
        match adelie_harness::slt::run_file(&mut engine, path) {
            Ok(report) if report.ok() => {
                println!(
                    "ok  {}  (passed {}, skipped {})",
                    path.display(),
                    report.passed,
                    report.skipped
                );
            }
            Ok(report) => {
                any_failed = true;
                println!("FAIL {}", path.display());
                for f in &report.failures {
                    println!("  line {}: {}", f.line, f.sql);
                    println!("    expected: {}", f.expected);
                    println!("    actual:   {}", f.actual);
                }
            }
            Err(e) => {
                any_failed = true;
                println!("FAIL {}  ({e})", path.display());
            }
        }
    }

    if any_failed {
        std::process::exit(1);
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
