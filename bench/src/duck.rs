//! `DuckDb`: the `Engine` adapter over an in-memory `duckdb::Connection`.

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

/// As `int_or_text`, for widths that are unsigned all the way up to u128 (UBigInt, UHugeInt).
fn uint_or_text(n: u128) -> Value {
    match i64::try_from(n) {
        Ok(v) => Value::Int(v),
        Err(_) => Value::Text(n.to_string()),
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
        ValueRef::UBigInt(n) => uint_or_text(n as u128),
        ValueRef::UHugeInt(n) => uint_or_text(n),
        ValueRef::Float(f) => Value::Float(f as f64),
        ValueRef::Double(f) => Value::Float(f),
        ValueRef::Decimal(d) => Value::Float(d.value() as f64 / 10f64.powi(d.scale() as i32)),
        ValueRef::Text(bytes) => Value::Text(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Date32(days) => Value::Text(format_date(days as i64)),
        ValueRef::Timestamp(unit, v) => Value::Text(format_timestamp(unit, v)),
        other => {
            return Err(EngineError(format!(
                "duckdb: unsupported result type {other:?}"
            )));
        }
    })
}

/// Howard Hinnant's civil-from-days: a day count since 1970-01-01 to (year, month, day).
/// http://howardhinnant.github.io/date_algorithms.html#civil_from_days
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn format_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn format_timestamp(unit: TimeUnit, v: i64) -> String {
    let micros = match unit {
        TimeUnit::Second => v * 1_000_000,
        TimeUnit::Millisecond => v * 1_000,
        TimeUnit::Microsecond => v,
        TimeUnit::Nanosecond => v.div_euclid(1_000),
    };
    let days = micros.div_euclid(86_400_000_000);
    let of_day = micros.rem_euclid(86_400_000_000);
    let (y, mo, d) = civil_from_days(days);
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
                    TIMESTAMP '2024-01-02 03:04:05.5'";
        let Outcome::Rows(rows) = db.run(sql).unwrap() else {
            panic!("expected rows")
        };
        assert_eq!(
            rows,
            vec![vec![
                Value::Int(1),
                Value::Int(1),
                Value::Float(1.5),
                Value::Float(1.5),
                Value::Text("x".to_string()),
                Value::Null,
                Value::Bool(true),
                Value::Text("2024-01-02".to_string()),
                Value::Text("2024-01-02 03:04:05".to_string()),
                Value::Text("2024-01-02 03:04:05.500000".to_string()),
            ]]
        );
    }

    #[test]
    #[cfg_attr(miri, ignore)] // links bundled DuckDB C++
    fn unsupported_result_type_is_a_loud_error() {
        let mut db = DuckDb::new().unwrap();
        let err = db.run("SELECT [1, 2]").unwrap_err();
        assert!(err.0.starts_with("duckdb: unsupported result type"));
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
