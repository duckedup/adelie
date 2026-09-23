//! `DuckDb`: the `Engine` adapter over an in-memory `duckdb::Connection`.

use adelie_harness::civil;
use adelie_harness::engine::{Engine, EngineError, Outcome, Value};
use duckdb::Connection;
use duckdb::types::{TimeUnit, ValueRef};

/// Keywords whose statement returns a result set; anything else goes through `execute_batch`.
const QUERY_KEYWORDS: &[&str] = &[
    "SELECT",
    "WITH",
    "VALUES",
    "FROM",
    "EXPLAIN",
    "SHOW",
    "DESCRIBE",
    "SUMMARIZE",
    "PRAGMA",
];

pub struct DuckDb {
    conn: Connection,
}

impl DuckDb {
    pub fn new() -> Result<Self, EngineError> {
        Connection::open_in_memory()
            .map(|conn| DuckDb { conn })
            .map_err(to_engine_error)
    }

    fn run_query(&mut self, sql: &str) -> Result<Outcome, EngineError> {
        let mut stmt = self.conn.prepare(sql).map_err(to_engine_error)?;
        let mut rows = stmt.query([]).map_err(to_engine_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(to_engine_error)? {
            // VERIFY: column count read via Row::AsRef<Statement>, since Statement::column_count
            // is documented to need an executed query, which `query()` above already did.
            let width = row.as_ref().column_count();
            let mut cells = Vec::with_capacity(width);
            for i in 0..width {
                cells.push(to_value(row.get_ref(i).map_err(to_engine_error)?)?);
            }
            out.push(cells);
        }
        Ok(Outcome::Rows(out))
    }
}

impl Engine for DuckDb {
    fn name(&self) -> &str {
        "duckdb"
    }

    fn run(&mut self, sql: &str) -> Result<Outcome, EngineError> {
        if is_query(sql) {
            self.run_query(sql)
        } else {
            self.conn
                .execute_batch(sql)
                .map(|_| Outcome::Statement)
                .map_err(to_engine_error)
        }
    }
}

fn to_engine_error(e: duckdb::Error) -> EngineError {
    EngineError(e.to_string())
}

/// Strips leading whitespace and `--` comment lines, then checks the first keyword
/// case-insensitively against `QUERY_KEYWORDS`.
fn is_query(sql: &str) -> bool {
    let mut rest = sql;
    loop {
        rest = rest.trim_start();
        match rest.strip_prefix("--") {
            Some(after) => rest = after.split_once('\n').map_or("", |(_, tail)| tail),
            None => break,
        }
    }
    let word: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    let word = word.to_ascii_uppercase();
    QUERY_KEYWORDS.contains(&word.as_str())
}

/// Widens a signed integer to `Value::Int`; anything wider than i64 becomes its decimal
/// text instead of silently truncating.
fn int_or_text(n: i128) -> Value {
    match i64::try_from(n) {
        Ok(v) => Value::Int(v),
        Err(_) => Value::Text(n.to_string()),
    }
}

/// Widens an unsigned integer to `Value::UInt`; anything wider than u64 (UHugeInt only)
/// becomes its decimal text instead of silently truncating.
fn uint_or_text(n: u128) -> Value {
    match u64::try_from(n) {
        Ok(v) => Value::UInt(v),
        Err(_) => Value::Text(n.to_string()),
    }
}

/// Converts a DuckDB `TIMESTAMP` to nanoseconds via a checked multiply, per its unit. On
/// overflow (a value far outside any real calendar range) it falls back to text so a wide
/// value never panics.
fn timestamp_value(unit: TimeUnit, v: i64) -> Value {
    let ns = match unit {
        TimeUnit::Second => v.checked_mul(1_000_000_000),
        TimeUnit::Millisecond => v.checked_mul(1_000_000),
        TimeUnit::Microsecond => v.checked_mul(1_000),
        TimeUnit::Nanosecond => Some(v),
    };
    match ns {
        Some(ns) => Value::Timestamp(ns),
        None => Value::Text(format_timestamp_micros(unit, v)),
    }
}

/// Fallback when the nanosecond form overflows `i64`: renders via microseconds instead, so
/// an extreme timestamp still prints rather than panicking.
fn format_timestamp_micros(unit: TimeUnit, v: i64) -> String {
    let micros = match unit {
        TimeUnit::Second => v * 1_000_000,
        TimeUnit::Millisecond => v * 1_000,
        TimeUnit::Microsecond => v,
        TimeUnit::Nanosecond => v.div_euclid(1_000),
    };
    let days = micros.div_euclid(86_400_000_000);
    let of_day = micros.rem_euclid(86_400_000_000);
    let (y, mo, d) = civil::civil_from_days(days);
    let secs = of_day / 1_000_000;
    let frac = of_day % 1_000_000;
    let (h, mi, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let base = format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}");
    if frac == 0 {
        base
    } else {
        format!("{base}.{frac:06}")
    }
}

fn to_value(v: ValueRef<'_>) -> Result<Value, EngineError> {
    Ok(match v {
        ValueRef::Null => Value::Null,
        ValueRef::Boolean(b) => Value::Bool(b),
        ValueRef::TinyInt(n) => Value::Int(n as i64),
        ValueRef::SmallInt(n) => Value::Int(n as i64),
        ValueRef::Int(n) => Value::Int(n as i64),
        ValueRef::BigInt(n) => Value::Int(n),
        ValueRef::HugeInt(n) => int_or_text(n),
        ValueRef::UTinyInt(n) => Value::Int(n as i64),
        ValueRef::USmallInt(n) => Value::Int(n as i64),
        ValueRef::UInt(n) => Value::Int(n as i64),
        ValueRef::UBigInt(n) => Value::UInt(n),
        ValueRef::UHugeInt(n) => uint_or_text(n),
        ValueRef::Float(f) => Value::Float(f as f64),
        ValueRef::Double(f) => Value::Float(f),
        ValueRef::Decimal(d) => Value::Decimal {
            value: d.value(),
            scale: d.scale(),
        },
        ValueRef::Text(bytes) => Value::Text(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(b) => Value::Bytes(b.to_vec()),
        ValueRef::Date32(days) => Value::Date(days),
        ValueRef::Timestamp(unit, v) => timestamp_value(unit, v),
        ValueRef::List(..) => owned_to_value(v.to_owned())?,
        other => {
            return Err(EngineError(format!(
                "duckdb: unsupported result type {other:?}"
            )));
        }
    })
}

/// Converts the owned `duckdb::types::Value` a LIST's elements (and its `ValueRef::to_owned`)
/// carry; the same mapping as `to_value` for scalar kinds, recursing into nested lists.
/// Everything else (STRUCT, MAP, …) is a loud error, same shape as `to_value`'s fallback.
fn owned_to_value(v: duckdb::types::Value) -> Result<Value, EngineError> {
    use duckdb::types::Value as OwnedValue;
    Ok(match v {
        OwnedValue::Null => Value::Null,
        OwnedValue::Boolean(b) => Value::Bool(b),
        OwnedValue::TinyInt(n) => Value::Int(n as i64),
        OwnedValue::SmallInt(n) => Value::Int(n as i64),
        OwnedValue::Int(n) => Value::Int(n as i64),
        OwnedValue::BigInt(n) => Value::Int(n),
        OwnedValue::HugeInt(n) => int_or_text(n),
        OwnedValue::UTinyInt(n) => Value::Int(n as i64),
        OwnedValue::USmallInt(n) => Value::Int(n as i64),
        OwnedValue::UInt(n) => Value::Int(n as i64),
        OwnedValue::UBigInt(n) => Value::UInt(n),
        OwnedValue::UHugeInt(n) => uint_or_text(n),
        OwnedValue::Float(f) => Value::Float(f as f64),
        OwnedValue::Double(f) => Value::Float(f),
        OwnedValue::Decimal(d) => Value::Decimal {
            value: d.value(),
            scale: d.scale(),
        },
        OwnedValue::Text(s) => Value::Text(s),
        OwnedValue::Blob(b) => Value::Bytes(b),
        OwnedValue::Date32(days) => Value::Date(days),
        OwnedValue::Timestamp(unit, v) => timestamp_value(unit, v),
        OwnedValue::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(owned_to_value(item)?);
            }
            Value::List(out)
        }
        other => {
            return Err(EngineError(format!(
                "duckdb: unsupported list element type {other:?}"
            )));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use adelie_harness::engine::{Engine, Outcome, Value};

    #[test]
    #[cfg_attr(miri, ignore)] // links bundled DuckDB C++
    fn value_mapping_covers_the_declared_types() {
        let mut db = DuckDb::new().unwrap();
        let sql = "SELECT 1::TINYINT, 1::HUGEINT, 1.5::DOUBLE, 1.5::DECIMAL(4,1), 'x', NULL, \
                    true, DATE '2024-01-02', TIMESTAMP '2024-01-02 03:04:05', \
                    TIMESTAMP '2024-01-02 03:04:05.5', 18446744073709551615::UBIGINT, \
                    '\\xAB'::BLOB, [1, NULL]";
        let Outcome::Rows(rows) = db.run(sql).unwrap() else {
            panic!("expected rows")
        };
        assert_eq!(
            rows,
            vec![vec![
                Value::Int(1),
                Value::Int(1),
                Value::Float(1.5),
                Value::Decimal {
                    value: 15,
                    scale: 1
                },
                Value::Text("x".to_string()),
                Value::Null,
                Value::Bool(true),
                Value::Date(19724),
                Value::Timestamp(1_704_164_645_000_000_000),
                Value::Timestamp(1_704_164_645_500_000_000),
                Value::UInt(u64::MAX),
                Value::Bytes(vec![0xab]),
                Value::List(vec![Value::Int(1), Value::Null]),
            ]]
        );
    }

    #[test]
    #[cfg_attr(miri, ignore)] // links bundled DuckDB C++
    fn unsupported_result_type_is_a_loud_error() {
        let mut db = DuckDb::new().unwrap();
        let err = db.run("SELECT {'a': 1}").unwrap_err();
        assert!(err.0.starts_with("duckdb: unsupported result type"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // links bundled DuckDB C++
    fn render_text_matches_the_pre_e2_corpus_strings() {
        let mut db = DuckDb::new().unwrap();
        let Outcome::Rows(rows) = db
            .run("SELECT DATE '2024-01-02', 1.50, 7::UBIGINT")
            .unwrap()
        else {
            panic!("expected rows")
        };
        let rendered: Vec<String> = rows[0]
            .iter()
            .map(adelie_harness::slt::render_text)
            .collect();
        assert_eq!(rendered, vec!["2024-01-02", "1.5", "7"]);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // links bundled DuckDB C++
    fn create_table_is_a_statement() {
        let mut db = DuckDb::new().unwrap();
        assert_eq!(
            db.run("CREATE TABLE t (a INT)").unwrap(),
            Outcome::Statement
        );
    }

    #[test]
    #[cfg_attr(miri, ignore)] // links bundled DuckDB C++
    fn leading_comment_is_still_a_query() {
        let mut db = DuckDb::new().unwrap();
        let outcome = db.run("-- comment\nSELECT 1").unwrap();
        assert_eq!(outcome, Outcome::Rows(vec![vec![Value::Int(1)]]));
    }
}
