//! Checks the sqllogictest corpus under `tests/slt` parses and is non-trivial. Pure file
//! reads and parsing: spawns nothing, so it runs under Miri.

use std::path::{Path, PathBuf};

use adelie_harness::slt::{Directive, parse};

fn corpus_files() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("slt");
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    files
}

/// Recurses so the corpus can grow subdirectories later without this test changing.
fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
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
        // E6: run_file(&mut adelie_engine, path) replaces this parse-only check.
    }
}

#[test]
fn a_malformed_file_is_rejected() {
    let err = parse("query IX\nSELECT 1").unwrap_err();
    assert_eq!(err.line, 1);
}
