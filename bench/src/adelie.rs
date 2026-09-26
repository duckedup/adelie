//! `Adelie`: the `Engine` adapter over a real `adelie::storage::Store` + `sql::execute`.
//! Mirror of tests/e2e/adelie.rs (see there).

use std::path::PathBuf;

use adelie::sql::{self, SqlOutput};
use adelie::storage::{Store, StoreOptions};
use adelie::types;
use adelie_harness::engine::{Engine, EngineError, Outcome, Value};

pub struct Adelie {
    store: Option<Store>,
    dir: PathBuf,
}

impl Adelie {
    pub fn new() -> Result<Self, EngineError> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "adelie-bench-adelie-{}-{nanos}",
            std::process::id()
        ));
        // `Store::open` creates `dir` itself (storage/mod.rs), so no explicit mkdir here.
        let store = Store::open(&dir, StoreOptions::default()).map_err(to_engine_error)?;
        Ok(Adelie {
            store: Some(store),
            dir,
        })
    }
}

impl Engine for Adelie {
    fn name(&self) -> &str {
        "adelie"
    }

    fn run(&mut self, sql: &str) -> Result<Outcome, EngineError> {
        let store = self.store.as_ref().expect("store dropped before run");
        match sql::execute(store, sql) {
            Ok(SqlOutput::Statement { .. }) => Ok(Outcome::Statement),
            Ok(SqlOutput::Rows(rows)) => {
                let mut out = Vec::new();
                for batch in &rows.batches {
                    for r in 0..batch.rows() {
                        let mut cells = Vec::with_capacity(batch.fields().len());
                        for c in 0..batch.fields().len() {
                            cells.push(to_value(batch.column(c).get(r)));
                        }
                        out.push(cells);
                    }
                }
                Ok(Outcome::Rows(out))
            }
            Err(e) => Err(EngineError(e.to_string())),
        }
    }
}

impl Drop for Adelie {
    fn drop(&mut self) {
        // Drop the store first, so its own close (flush + unlock) runs before the directory
        // that backs it is removed.
        self.store.take();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn to_engine_error(e: adelie::storage::Error) -> EngineError {
    EngineError(e.to_string())
}

fn to_value(v: types::Value) -> Value {
    match v {
        types::Value::Null => Value::Null,
        types::Value::Bool(b) => Value::Bool(b),
        types::Value::Int64(n) => Value::Int(n),
        types::Value::UInt64(n) => Value::UInt(n),
        types::Value::Float64(f) => Value::Float(f),
        types::Value::Decimal(d) => Value::Decimal {
            value: d.unscaled(),
            scale: d.scale(),
        },
        types::Value::String(s) => Value::Text(s),
        types::Value::Bytes(b) => Value::Bytes(b),
        types::Value::Timestamp(ns) => Value::Timestamp(ns),
        types::Value::Date(d) => Value::Date(d),
        types::Value::Uuid(bytes) => Value::Uuid(bytes),
        types::Value::Ip(ip) => Value::Ip(ip.to_ip_addr()),
        types::Value::List(xs) => Value::List(xs.into_iter().map(to_value).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn create_insert_select_maps_int_text_and_null() {
        let mut db = Adelie::new().unwrap();
        assert_eq!(
            db.run("CREATE TABLE t (a INT64, b STRING)").unwrap(),
            Outcome::Statement
        );
        assert_eq!(
            db.run("INSERT INTO t VALUES (1, 'x'), (2, NULL)").unwrap(),
            Outcome::Statement
        );
        let outcome = db.run("SELECT a, b FROM t ORDER BY a").unwrap();
        assert_eq!(
            outcome,
            Outcome::Rows(vec![
                vec![Value::Int(1), Value::Text("x".to_string())],
                vec![Value::Int(2), Value::Null],
            ])
        );
    }
}
