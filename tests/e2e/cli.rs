//! `adelie sql` e2e: the real binary over a real store directory. Every test can fail; the
//! comment on each says how.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use adelie::sql::{SqlOutput, execute};
use adelie::storage::{Store, StoreOptions};

use super::common::{bin, temp_dir};

fn fixture_literal(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/copy")
        .join(name)
        .to_str()
        .unwrap()
        .replace('\'', "''")
}

fn adelie(args: &[&str]) -> Output {
    Command::new(bin()).args(args).output().unwrap()
}

fn sql_args<'a>(dir: &'a Path, rest: &[&'a str]) -> Output {
    let mut args = vec!["sql", dir.to_str().unwrap()];
    args.extend_from_slice(rest);
    adelie(&args)
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

/// `count(*)` of `t`, read through the library (the writer, so it sees everything).
fn count(store: &Store) -> i64 {
    let SqlOutput::Rows(rows) = execute(store, "SELECT count(*) FROM t").unwrap() else {
        panic!("expected rows")
    };
    match rows.batches[0].column(0).get(0) {
        adelie::types::Value::Int64(n) => n,
        other => panic!("count was {other:?}"),
    }
}

/// A store with `t (id, name)` holding three rows, flushed and closed.
fn people(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    let store = Store::open(&dir, StoreOptions::default()).unwrap();
    execute(&store, "CREATE TABLE t (id BIGINT, name TEXT)").unwrap();
    execute(&store, "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')").unwrap();
    store.close().unwrap();
    dir
}

/// Fails if the CLI's rendering or its read path drifts from the library's: the CSV it prints
/// must equal the library's rows rendered with `to_text`.
#[test]
#[cfg_attr(miri, ignore)] // spawns the adelie binary
fn csv_output_matches_the_library() {
    let dir = temp_dir("cli-parity");
    let store = Store::open(&dir, StoreOptions::default()).unwrap();
    execute(
        &store,
        r#"CREATE TABLE sales (cat TEXT, amt BIGINT, "amt::string" TEXT, note TEXT)"#,
    )
    .unwrap();
    let copy = format!("COPY sales FROM '{}'", fixture_literal("sales.csv"));
    execute(&store, &copy).unwrap();
    let query = "SELECT cat, sum(amt), count(*) FROM sales GROUP BY cat ORDER BY cat";
    let SqlOutput::Rows(rows) = execute(&store, query).unwrap() else {
        panic!("expected rows")
    };
    let mut want = Vec::new();
    for b in &rows.batches {
        for r in 0..b.rows() {
            let cells: Vec<String> = (0..3)
                .map(|c| b.column(c).get(r).to_text().unwrap_or_default())
                .collect();
            want.push(cells.join(","));
        }
    }
    store.close().unwrap();

    let out = sql_args(&dir, &[query, "--format", "csv"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1 + want.len(), "{text}");
    assert!(lines[0].starts_with("cat,"), "{text}");
    assert_eq!(&lines[1..], want.as_slice());
    assert_eq!(want, ["a,350,3", "b,25,3"]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Fails if a write goes through without `--write`, the error omits the hint, or `--write`
/// does not actually write.
#[test]
#[cfg_attr(miri, ignore)] // spawns the adelie binary
fn writes_need_the_write_flag() {
    let dir = people("cli-readonly");
    let insert = "INSERT INTO t VALUES (4, 'd')";

    let out = sql_args(&dir, &[insert]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("--write"), "{}", stderr(&out));
    assert!(stderr(&out).starts_with("error: "), "{}", stderr(&out));
    let store = Store::open(&dir, StoreOptions::default()).unwrap();
    assert_eq!(count(&store), 3);
    store.close().unwrap();

    let out = sql_args(&dir, &[insert, "--write"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "1 rows affected");
    let store = Store::open(&dir, StoreOptions::default()).unwrap();
    assert_eq!(count(&store), 4);
    store.close().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Fails (with a Locked error) if reads take the writer lock: a live writer holds the
/// directory and flushed rows must still be readable by the CLI.
#[test]
#[cfg_attr(miri, ignore)] // spawns the adelie binary
fn reads_work_while_a_writer_holds_the_lock() {
    let dir = temp_dir("cli-lock");
    let store = Store::open(&dir, StoreOptions::default()).unwrap();
    execute(&store, "CREATE TABLE t (id BIGINT, name TEXT)").unwrap();
    execute(&store, "INSERT INTO t VALUES (1, 'a'), (2, 'b')").unwrap();
    store.flush().unwrap();

    let out = sql_args(&dir, &["SELECT count(*) FROM t", "--format", "csv"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).lines().last(), Some("2"));
    store.close().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Fails if the binder's did-you-mean suggestion does not reach the user's terminal.
#[test]
#[cfg_attr(miri, ignore)] // spawns the adelie binary
fn misspelled_column_suggests_the_right_one() {
    let dir = people("cli-suggest");
    let out = sql_args(&dir, &["SELECT naem FROM t"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("did you mean \"name\""),
        "{}",
        stderr(&out)
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Fails if stdin or `--file` produce different output than the argument form.
#[test]
#[cfg_attr(miri, ignore)] // spawns the adelie binary
fn query_from_stdin_and_file_matches_the_argument() {
    let dir = people("cli-input");
    let query = "SELECT id, name FROM t ORDER BY id";
    let from_arg = sql_args(&dir, &[query, "--format", "json"]);
    assert!(from_arg.status.success(), "{}", stderr(&from_arg));
    assert!(
        stdout(&from_arg).contains("\"name\":\"b\""),
        "{}",
        stdout(&from_arg)
    );

    let mut child = Command::new(bin())
        .args(["sql", dir.to_str().unwrap(), "--format", "json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(query.as_bytes())
        .unwrap();
    let from_stdin = child.wait_with_output().unwrap();
    assert!(from_stdin.status.success(), "{}", stderr(&from_stdin));
    assert_eq!(stdout(&from_stdin), stdout(&from_arg));

    let file = temp_dir("cli-input-query").with_extension("sql");
    std::fs::write(&file, query).unwrap();
    let from_file = sql_args(
        &dir,
        &["--file", file.to_str().unwrap(), "--format", "json"],
    );
    assert!(from_file.status.success(), "{}", stderr(&from_file));
    assert_eq!(stdout(&from_file), stdout(&from_arg));
    std::fs::remove_file(&file).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}
