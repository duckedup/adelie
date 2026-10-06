//! JSON for query results: one value mapping and the capped, shaped result envelope.

use serde_json::{Value as Json, json};

use crate::sql::SqlOutput;
use crate::types::Value;

/// Caps on what one result may carry back to a caller.
pub struct Limits {
    pub max_rows: usize,
    pub max_bytes: usize,
}

/// A result as JSON, plus a human note when it was cut to fit `Limits`.
pub struct Shaped {
    pub json: Json,
    pub truncated: Option<String>,
}

/// Numbers stay numbers, NULL is null, lists are arrays; every other kind is its canonical
/// text, which is lossless (DECIMAL, TIMESTAMP, DATE, UUID, IP, BYTES).
pub fn value_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int64(n) => Json::from(*n),
        Value::UInt64(n) => Json::from(*n),
        Value::Float64(f) if f.is_finite() => Json::from(*f),
        Value::List(items) => Json::Array(items.iter().map(value_json).collect()),
        other => other.to_text().map_or(Json::Null, Json::String),
    }
}

/// Stops at `max_rows` rows or before the serialized rows would pass `max_bytes`, never
/// mid-row. The first row is always kept so a wide row cannot return nothing.
pub fn shape(out: &SqlOutput, limits: &Limits) -> Shaped {
    let rows = match out {
        SqlOutput::Statement { rows_affected } => {
            return Shaped {
                json: json!({ "rows_affected": rows_affected }),
                truncated: None,
            };
        }
        SqlOutput::Rows(rows) => rows,
    };
    let columns: Vec<Json> = rows
        .fields
        .iter()
        .map(|f| json!({ "name": f.name, "type": f.ty.to_string() }))
        .collect();
    let total: usize = rows.batches.iter().map(|b| b.rows()).sum();
    let mut kept: Vec<Json> = Vec::new();
    let mut bytes = 0usize;
    let mut cut_by_bytes = false;
    'batches: for batch in &rows.batches {
        for r in 0..batch.rows() {
            if kept.len() >= limits.max_rows {
                break 'batches;
            }
            let row = Json::Array(
                (0..batch.fields().len())
                    .map(|c| value_json(&batch.column(c).get(r)))
                    .collect(),
            );
            let size = row.to_string().len() + 1;
            if !kept.is_empty() && bytes + size > limits.max_bytes {
                cut_by_bytes = true;
                break 'batches;
            }
            bytes += size;
            kept.push(row);
        }
    }
    let truncated = (kept.len() < total).then(|| {
        let cap = if cut_by_bytes {
            format!("byte cap {}", limits.max_bytes)
        } else {
            format!("row cap {}", limits.max_rows)
        };
        format!("showed {} of {total} rows ({cap})", kept.len())
    });
    Shaped {
        json: json!({ "columns": columns, "rows": kept, "warnings": rows.warnings }),
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;
    use crate::exec::{Batch, Column, Field, QueryStats};
    use crate::sql::Rows;
    use crate::types::{DataType, Decimal, Ip};

    #[test]
    fn value_json_maps_every_kind() {
        let ip = Value::Ip(Ip::from("10.0.0.1".parse::<IpAddr>().unwrap()));
        let ts = Value::Timestamp(1_500_000_000);
        let ts_text = ts.to_text().unwrap();
        let cases = [
            (Value::Null, Json::Null),
            (Value::Bool(true), json!(true)),
            (Value::Int64(i64::MIN), json!(i64::MIN)),
            (Value::UInt64(u64::MAX), json!(u64::MAX)),
            (Value::Float64(1.5), json!(1.5)),
            (Value::Float64(f64::NAN), json!("NaN")),
            (Value::Decimal(Decimal::new(-5, 2).unwrap()), json!("-0.05")),
            (
                Value::Uuid([0; 16]),
                json!("00000000-0000-0000-0000-000000000000"),
            ),
            (ip, json!("10.0.0.1")),
            (ts, json!(ts_text)),
            (Value::Date(19724), json!("2024-01-02")),
            (
                Value::List(vec![Value::Int64(1), Value::Null]),
                json!([1, null]),
            ),
        ];
        for (value, want) in cases {
            assert_eq!(value_json(&value), want, "{value:?}");
        }
    }

    fn ints(n: i64) -> SqlOutput {
        let values: Vec<Value> = (0..n).map(Value::Int64).collect();
        let col = Column::from_values(&DataType::Int64, &values).unwrap();
        let field = Field {
            name: "x".to_string(),
            ty: DataType::Int64,
        };
        SqlOutput::Rows(Rows {
            fields: vec![field.clone()],
            batches: vec![Batch::new(vec![field], vec![col]).unwrap()],
            stats: QueryStats::default(),
            warnings: Vec::new(),
        })
    }

    /// Fails if the row cap keeps the wrong count or the note is missing.
    #[test]
    fn shape_row_cap_truncates_with_a_note() {
        let limits = Limits {
            max_rows: 3,
            max_bytes: 1 << 20,
        };
        let shaped = shape(&ints(10), &limits);
        assert_eq!(shaped.json["rows"], json!([[0], [1], [2]]));
        assert_eq!(
            shaped.truncated.as_deref(),
            Some("showed 3 of 10 rows (row cap 3)")
        );
        assert_eq!(shaped.json["columns"][0]["type"], json!("INT64"));
    }

    /// Fails if the byte cap lets a row past the budget or cuts one in half. Each row is
    /// `[n]` (3 bytes) plus a separator, so a 9 byte budget fits two rows and not three.
    #[test]
    fn shape_byte_cap_stops_on_a_row_boundary() {
        let limits = Limits {
            max_rows: 100,
            max_bytes: 9,
        };
        let shaped = shape(&ints(10), &limits);
        assert_eq!(shaped.json["rows"], json!([[0], [1]]));
        assert_eq!(
            shaped.truncated.as_deref(),
            Some("showed 2 of 10 rows (byte cap 9)")
        );
    }

    #[test]
    fn shape_under_the_caps_is_not_truncated() {
        let limits = Limits {
            max_rows: 10,
            max_bytes: 1 << 20,
        };
        let shaped = shape(&ints(10), &limits);
        assert!(shaped.truncated.is_none());
        assert_eq!(shaped.json["rows"].as_array().unwrap().len(), 10);
        let st = shape(&SqlOutput::Statement { rows_affected: 4 }, &limits);
        assert_eq!(st.json, json!({ "rows_affected": 4 }));
    }
}
