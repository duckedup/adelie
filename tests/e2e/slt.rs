//! The sqllogictest corpus under `tests/slt`, checked two ways: `corpus_parses` is a pure,
//! Miri-clean parse of every file, and `corpus_passes_against_adelie` (E6, wsr.1, 1st.1) runs
//! each one against a fresh `Adelie`, failing on any mismatch.

use std::path::{Path, PathBuf};

use adelie_harness::slt::{Directive, parse, run_file};

use crate::adelie::Adelie;

fn corpus_files() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/slt");
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    files
}

/// Recurses so the corpus can grow subdirectories later without this test changing.
fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
    // A missing directory is a wrong path, not an empty corpus: say which.
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, files);
        } else if path.extension().is_some_and(|e| e == "slt") {
            files.push(path);
        }
    }
}

#[test]
fn corpus_parses() {
    let files = corpus_files();
    assert!(
        files.len() >= 8,
        "expected at least 8 .slt files, found {}",
        files.len()
    );
    for path in &files {
        let src =
            std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let records = parse(&src).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let has_query = records
            .iter()
            .any(|r| matches!(r.directive, Directive::Query { .. }));
        assert!(has_query, "{}: no query record", path.display());
    }
}

/// One collected mismatch, tagged with the file it came from, for `corpus_passes_against_adelie`
/// to report all at once instead of stopping at the first one.
struct Miss {
    path: PathBuf,
    line: usize,
    sql: String,
    expected: String,
    actual: String,
}

/// Runs every corpus file against a fresh `Adelie` (E6: no exclusion list), collecting every
/// failure across every file before panicking once with all of them listed.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn corpus_passes_against_adelie() {
    let files = corpus_files();
    assert!(
        files.len() >= 10,
        "expected at least 10 .slt files (9 plus functions.slt), found {}",
        files.len()
    );
    let mut misses = Vec::new();
    for path in &files {
        let mut engine = Adelie::new().expect("Adelie::new");
        let report =
            run_file(&mut engine, path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        // Catches a file where every record was skipped (e.g. a bad `onlyif`/`skipif`).
        assert!(report.passed > 0, "{}: no record passed", path.display());
        for f in report.failures {
            misses.push(Miss {
                path: path.clone(),
                line: f.line,
                sql: f.sql,
                expected: f.expected,
                actual: f.actual,
            });
        }
    }
    if misses.is_empty() {
        return;
    }
    let mut msg = format!("{} slt failure(s) against adelie:\n", misses.len());
    for m in &misses {
        msg.push_str(&format!(
            "{}:{}: {}\n  expected: {}\n  actual:   {}\n",
            m.path.display(),
            m.line,
            m.sql,
            m.expected,
            m.actual
        ));
    }
    panic!("{msg}");
}

#[test]
fn a_malformed_file_is_rejected() {
    let err = parse("query IX\nSELECT 1").unwrap_err();
    assert_eq!(err.line, 1);
}
