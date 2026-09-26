//! SQL-level e2e over a real `Store` (E6, wsr.1, 1st.1): the Q2 companion read path, INSERT
//! durability, COPY's CSV/NDJSON readers, the parser's depth cap, `now()`, DELETE and
//! schema-qualified tables. Every test can fail; the comment on each says how.

use std::path::{Path, PathBuf};

use adelie::exec::Batch;
use adelie::sql::{Options, SqlError, SqlOutput, execute, execute_with};
use adelie::storage::{Store, StoreOptions};
use adelie::types::{DataType, Decimal, Value};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "adelie-e2e-sql-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

fn open(dir: &Path) -> Store {
    Store::open(dir, StoreOptions::default()).unwrap()
}

/// One column's values, flattened row-major across every batch a `SELECT` returned.
fn col(batches: &[Batch], c: usize) -> Vec<Value> {
    batches
        .iter()
        .flat_map(|b| (0..b.rows()).map(move |r| b.column(c).get(r)))
        .collect()
}

/// Every column's values, flattened row-major across every batch.
fn rows_of(batches: &[Batch]) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for b in batches {
        for r in 0..b.rows() {
            out.push((0..b.fields().len()).map(|c| b.column(c).get(r)).collect());
        }
    }
    out
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/copy")
        .join(name)
}

/// A COPY source literal: the fixture's absolute path, with `'` doubled per SQL string rules.
fn fixture_literal(name: &str) -> String {
    fixture(name).to_str().unwrap().replace('\'', "''")
}

const COMPANION_TABLE: &str = r#"CREATE TABLE t (id INTEGER, x BIGINT, "x::string" TEXT)"#;
const COMPANION_ROWS: &str = "INSERT INTO t (id, x) VALUES (1, 1000), (2, '500')";

// ── wsr.1: the Q2 companion read path ───────────────────────────────────────

/// Fails if the type flips to STRING (companion routing must not widen the primary column) or
/// if row 2 reads back as `'500'` (that text lives only in the companion).
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn wsr1_read_type_stays_int64_with_the_companion_null() {
    let dir = temp_dir("wsr1-read-type");
    let store = open(&dir);
    execute(&store, COMPANION_TABLE).unwrap();
    execute(&store, COMPANION_ROWS).unwrap();

    let out = execute(&store, "SELECT id, x FROM t ORDER BY id").unwrap();
    let SqlOutput::Rows(rows) = out else {
        panic!("expected rows")
    };
    assert_eq!(rows.fields[1].ty, DataType::Int64);
    assert_eq!(col(&rows.batches, 1), vec![Value::Int64(1000), Value::Null]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Fails under text comparison: `'1000' < '500'` lexicographically would select row 2 instead
/// of row 1.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn wsr1_predicate_compares_the_typed_column_not_its_text() {
    let dir = temp_dir("wsr1-predicate");
    let store = open(&dir);
    execute(&store, COMPANION_TABLE).unwrap();
    execute(&store, COMPANION_ROWS).unwrap();

    let out = execute(&store, "SELECT id FROM t WHERE x >= 500").unwrap();
    let SqlOutput::Rows(rows) = out else {
        panic!("expected rows")
    };
    assert_eq!(col(&rows.batches, 0), vec![Value::Int64(1)]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Fails if `coalesce_text` returns the primary's own text instead of falling back to the
/// companion for the row that did not fit.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn coalesce_text_reads_back_the_original_text_for_every_row() {
    let dir = temp_dir("wsr1-coalesce-text");
    let store = open(&dir);
    execute(&store, COMPANION_TABLE).unwrap();
    execute(&store, COMPANION_ROWS).unwrap();

    let out = execute(&store, "SELECT coalesce_text(x) FROM t ORDER BY id").unwrap();
    let SqlOutput::Rows(rows) = out else {
        panic!("expected rows")
    };
    assert_eq!(
        col(&rows.batches, 0),
        vec![Value::String("1000".into()), Value::String("500".into())]
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Fails if `x::string` casts instead of naming the companion: a cast would give `'1000'` then
/// NULL, the opposite of the companion's own contents.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn postfix_string_on_a_bare_column_names_the_companion_not_a_cast() {
    let dir = temp_dir("wsr1-postfix-string");
    let store = open(&dir);
    execute(&store, COMPANION_TABLE).unwrap();
    execute(&store, COMPANION_ROWS).unwrap();

    let out = execute(&store, "SELECT x::string FROM t ORDER BY id").unwrap();
    let SqlOutput::Rows(rows) = out else {
        panic!("expected rows")
    };
    assert_eq!(
        col(&rows.batches, 0),
        vec![Value::Null, Value::String("500".into())]
    );

    // Under GROUP BY x the companion is ungrouped, so this is an error: a cast of x would
    // silently read '1000' and NULL instead.
    let err = execute(&store, "SELECT x::string FROM t GROUP BY x ORDER BY 1").unwrap_err();
    assert!(
        matches!(err, SqlError::Bind(ref m) if m.contains("GROUP BY")),
        "{err}"
    );
    let out = execute(
        &store,
        "SELECT x::string, count(*) FROM t GROUP BY x::string ORDER BY 1",
    )
    .unwrap();
    let SqlOutput::Rows(rows) = out else {
        panic!("expected rows")
    };
    assert_eq!(
        col(&rows.batches, 0),
        vec![Value::String("500".into()), Value::Null]
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Fails if a bare read of a column with a companion stays silent, if `id`/`coalesce_text`
/// warn when they should not, or if a table with no companion ever warns.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn only_a_bare_companion_column_read_warns() {
    let dir = temp_dir("wsr1-warnings");
    let store = open(&dir);
    execute(&store, COMPANION_TABLE).unwrap();
    execute(&store, COMPANION_ROWS).unwrap();
    execute(&store, "CREATE TABLE u (x BIGINT)").unwrap();

    let out = execute(&store, "SELECT x FROM t").unwrap();
    let SqlOutput::Rows(rows) = out else {
        panic!("expected rows")
    };
    assert_eq!(rows.warnings.len(), 1);
    assert!(
        rows.warnings[0].contains('x'),
        "warning: {:?}",
        rows.warnings[0]
    );

    let SqlOutput::Rows(rows) = execute(&store, "SELECT id FROM t").unwrap() else {
        panic!("expected rows")
    };
    assert!(rows.warnings.is_empty());

    let SqlOutput::Rows(rows) = execute(&store, "SELECT coalesce_text(x) FROM t").unwrap() else {
        panic!("expected rows")
    };
    assert!(rows.warnings.is_empty());

    let SqlOutput::Rows(rows) = execute(&store, "SELECT x FROM u").unwrap() else {
        panic!("expected rows")
    };
    assert!(rows.warnings.is_empty());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Fails if a value that does not fit is silently dropped instead of failing the statement:
/// with no declared companion, the row must never land.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn insert_without_a_companion_fails_naming_the_column_and_writes_nothing() {
    let dir = temp_dir("wsr1-insert-no-companion");
    let store = open(&dir);
    execute(&store, "CREATE TABLE u (x BIGINT)").unwrap();

    let err = execute(&store, "INSERT INTO u VALUES ('500')").unwrap_err();
    assert!(err.to_string().contains("column x"), "error: {err}");

    let SqlOutput::Rows(rows) = execute(&store, "SELECT count(*) FROM u").unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(col(&rows.batches, 0), vec![Value::Int64(0)]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Fails if INSERT returns before the write is durable: `Store::write` blocks on the flush
/// ticket, so a fresh reopen must already see it with no flush call in between.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn insert_is_durable_when_it_returns() {
    let dir = temp_dir("wsr1-durable-insert");
    {
        let store = open(&dir);
        execute(&store, "CREATE TABLE t (a INT64)").unwrap();
        execute(&store, "INSERT INTO t VALUES (1), (2), (3)").unwrap();
        store.close().unwrap();
    }
    let store = open(&dir);
    let SqlOutput::Rows(rows) = execute(&store, "SELECT a FROM t ORDER BY a").unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(
        col(&rows.batches, 0),
        vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)]
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── COPY: CSV and NDJSON ─────────────────────────────────────────────────────

/// Fails on a wrong hand-computed aggregate, if the `n/a` cell does not land in its companion,
/// or if the embedded newline in `note` is lost.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn copy_csv_loads_aggregates_correctly_and_routes_bad_cells() {
    let dir = temp_dir("wsr-copy-csv");
    let store = open(&dir);
    execute(
        &store,
        r#"CREATE TABLE sales (cat TEXT, amt BIGINT, "amt::string" TEXT, note TEXT)"#,
    )
    .unwrap();

    let sql = format!("COPY sales FROM '{}'", fixture_literal("sales.csv"));
    let out = execute(&store, &sql).unwrap();
    assert!(matches!(out, SqlOutput::Statement { rows_affected: 6 }));

    let SqlOutput::Rows(rows) = execute(
        &store,
        "SELECT cat, sum(amt), count(*) FROM sales GROUP BY cat ORDER BY cat",
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(
        rows_of(&rows.batches),
        vec![
            vec![
                Value::String("a".into()),
                Value::Int64(350),
                Value::Int64(3)
            ],
            vec![Value::String("b".into()), Value::Int64(25), Value::Int64(3)],
        ]
    );

    let SqlOutput::Rows(rows) = execute(
        &store,
        "SELECT amt, \"amt::string\" FROM sales WHERE note = 'not a number'",
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(
        rows_of(&rows.batches),
        vec![vec![Value::Null, Value::String("n/a".into())]]
    );

    let SqlOutput::Rows(rows) = execute(&store, "SELECT note FROM sales WHERE amt = 200").unwrap()
    else {
        panic!("expected rows")
    };
    assert_eq!(
        col(&rows.batches, 0),
        vec![Value::String("first line\nsecond line".into())]
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Fails if COPY partially loads instead of being atomic: a table with no companion must end
/// up with zero rows once the `n/a` cell can't be routed anywhere.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn copy_csv_without_a_companion_fails_atomically() {
    let dir = temp_dir("wsr-copy-csv-no-companion");
    let store = open(&dir);
    execute(
        &store,
        "CREATE TABLE sales (cat TEXT, amt BIGINT, note TEXT)",
    )
    .unwrap();

    let sql = format!("COPY sales FROM '{}'", fixture_literal("sales.csv"));
    let err = execute(&store, &sql).unwrap_err();
    match err {
        SqlError::Ingest(e) => assert!(e.to_string().contains("amt"), "error: {e}"),
        other => panic!("expected Ingest error, got {other:?}"),
    }

    let SqlOutput::Rows(rows) = execute(&store, "SELECT count(*) FROM sales").unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(col(&rows.batches, 0), vec![Value::Int64(0)]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Fails if a nested object does not flatten into its dotted column, or if the DECIMAL sum
/// drifts (e.g. `12.339999` instead of the exact `12.34`).
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn copy_ndjson_flattens_nested_fields_and_sums_decimal_exactly() {
    let dir = temp_dir("wsr-copy-ndjson");
    let store = open(&dir);
    execute(
        &store,
        r#"CREATE TABLE ev (ts TIMESTAMP, "service.name" TEXT, "attributes.http.route" TEXT, cost DECIMAL(10,2))"#,
    )
    .unwrap();

    let sql = format!("COPY ev FROM '{}'", fixture_literal("events.ndjson"));
    let out = execute(&store, &sql).unwrap();
    assert!(matches!(out, SqlOutput::Statement { rows_affected: 4 }));

    let SqlOutput::Rows(rows) = execute(
        &store,
        "SELECT \"attributes.http.route\", sum(cost) FROM ev GROUP BY 1 ORDER BY 1",
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(
        rows_of(&rows.batches),
        vec![
            vec![
                Value::String("/cart".into()),
                Value::Decimal(Decimal::new(500, 2).unwrap())
            ],
            vec![
                Value::String("/pay".into()),
                Value::Decimal(Decimal::new(2000, 2).unwrap())
            ],
        ]
    );

    let SqlOutput::Rows(rows) = execute(
        &store,
        "SELECT ts FROM ev WHERE \"service.name\" = 'checkout' ORDER BY ts LIMIT 1",
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows.fields[0].ty, DataType::Timestamp);
    assert!(matches!(col(&rows.batches, 0)[0], Value::Timestamp(_)));
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── parser, now(), DELETE, schemas ───────────────────────────────────────────

/// Fails if 100,000 nested parens overflow the stack instead of hitting the depth cap, or if
/// the error message does not mention nesting.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn parser_rejects_extreme_nesting_without_overflowing_the_stack() {
    let dir = temp_dir("wsr-parser-depth");
    let store = open(&dir);
    let sql = "SELECT ".to_string() + &"(".repeat(100_000) + "1" + &")".repeat(100_000);

    match execute(&store, &sql) {
        Err(SqlError::Parse(e)) => assert!(e.message.contains("nesting"), "message: {}", e.message),
        other => panic!("expected Err(SqlError::Parse(_)), got {other:?}"),
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Fails if `now()` is evaluated per row or per call instead of folded once per statement.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn now_is_one_value_per_statement() {
    let dir = temp_dir("wsr-now");
    let store = open(&dir);

    let SqlOutput::Rows(rows) = execute(&store, "SELECT now() = now()").unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(col(&rows.batches, 0), vec![Value::Bool(true)]);

    let t = 1_700_000_000_000_000_000i64;
    let opts = Options {
        now: Some(t),
        ..Options::default()
    };
    let SqlOutput::Rows(rows) = execute_with(&store, "SELECT now()", &opts).unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(col(&rows.batches, 0), vec![Value::Timestamp(t)]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Fails if DELETE with an unsupported predicate is silently a no-op instead of a `Bind` error.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn delete_matches_exactly_and_rejects_an_unsupported_predicate() {
    let dir = temp_dir("wsr-delete");
    let store = open(&dir);
    execute(&store, COMPANION_TABLE).unwrap();
    execute(&store, COMPANION_ROWS).unwrap();

    execute(&store, "DELETE FROM t WHERE id = 1").unwrap();
    let SqlOutput::Rows(rows) = execute(&store, "SELECT count(*) FROM t").unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(col(&rows.batches, 0), vec![Value::Int64(1)]);

    let err = execute(&store, "DELETE FROM t WHERE x + 1 > 2").unwrap_err();
    assert!(
        matches!(err, SqlError::Bind(_)),
        "expected Bind, got {err:?}"
    );
    let SqlOutput::Rows(rows) = execute(&store, "SELECT count(*) FROM t").unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(col(&rows.batches, 0), vec![Value::Int64(1)]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Fails if `otel.spans` does not resolve, e.g. by silently falling back to `main.spans`.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn schema_qualified_tables_resolve_and_query() {
    let dir = temp_dir("wsr-schema-qualified");
    let store = open(&dir);
    execute(&store, "CREATE SCHEMA otel").unwrap();
    execute(&store, "CREATE TABLE otel.spans (a BIGINT)").unwrap();
    execute(&store, "INSERT INTO otel.spans VALUES (1), (2), (3)").unwrap();

    let SqlOutput::Rows(rows) = execute(&store, "SELECT count(*) FROM otel.spans").unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(col(&rows.batches, 0), vec![Value::Int64(3)]);
    std::fs::remove_dir_all(&dir).unwrap();
}
