//! The `adelie` engine adapter (contract C6): opens a fresh `Store` per instance and runs SQL
//! through `adelie::sql::execute`, mapping the result onto `adelie_harness::engine`.
//!
//! Mirror of `bench/src/adelie.rs`: harness/ has no dependencies and bench/ is its own
//! workspace (D0006), so the adapter lives in both places.

use std::path::PathBuf;

use adelie::sql::{self, SqlOutput};
use adelie::storage::{Store, StoreOptions};
use adelie::types::Value as AValue;
use adelie_harness::engine::{Engine, EngineError, Outcome, Value};

pub struct Adelie {
    store: Option<Store>,
    dir: PathBuf,
}

/// Mirrors `tests/e2e/store.rs:44-53`'s helper, tagged `slt` for this suite's temp dirs.
fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("adelie-e2e-{tag}-{}-{nanos}", std::process::id()))
}

impl Adelie {
    pub fn new() -> Result<Adelie, EngineError> {
        let dir = temp_dir("slt");
        let store =
            Store::open(&dir, StoreOptions::default()).map_err(|e| EngineError(e.to_string()))?;
        Ok(Adelie {
            store: Some(store),
            dir,
        })
    }
}

impl Drop for Adelie {
    fn drop(&mut self) {
        let _ = self.store.take().map(Store::close);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn map_value(v: AValue) -> Value {
    match v {
        AValue::Null => Value::Null,
        AValue::Bool(b) => Value::Bool(b),
        AValue::Int64(n) => Value::Int(n),
        AValue::UInt64(n) => Value::UInt(n),
        AValue::Float64(f) => Value::Float(f),
        AValue::Decimal(d) => Value::Decimal {
            value: d.unscaled(),
            scale: d.scale(),
        },
        AValue::String(s) => Value::Text(s),
        AValue::Bytes(b) => Value::Bytes(b),
        AValue::Timestamp(ns) => Value::Timestamp(ns),
        AValue::Date(d) => Value::Date(d),
        AValue::Uuid(bytes) => Value::Uuid(bytes),
        AValue::Ip(ip) => Value::Ip(ip.to_ip_addr()),
        AValue::List(items) => Value::List(items.into_iter().map(map_value).collect()),
    }
}

impl Engine for Adelie {
    fn name(&self) -> &str {
        "adelie"
    }

    fn run(&mut self, sql_text: &str) -> Result<Outcome, EngineError> {
        let store = self
            .store
            .as_ref()
            .expect("Adelie's store is only taken by Drop");
        match sql::execute(store, sql_text) {
            Ok(SqlOutput::Statement { .. }) => Ok(Outcome::Statement),
            Ok(SqlOutput::Rows(rows)) => {
                let mut out = Vec::new();
                for batch in &rows.batches {
                    for r in 0..batch.rows() {
                        let row: Vec<Value> = (0..batch.fields().len())
                            .map(|c| map_value(batch.column(c).get(r)))
                            .collect();
                        out.push(row);
                    }
                }
                Ok(Outcome::Rows(out))
            }
            Err(e) => Err(EngineError(e.to_string())),
        }
    }
}
