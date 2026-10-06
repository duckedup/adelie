//! Result renderers for `adelie sql`: table, CSV, JSON and NDJSON. NULL is `NULL` in a table,
//! empty in CSV and `null` in JSON.

use comfy_table::{Table, presets::UTF8_FULL};

use crate::sql::Rows;
use crate::surface::value_json;
use crate::types::Value;

fn each_row(rows: &Rows, mut f: impl FnMut(Vec<Value>)) {
    for batch in &rows.batches {
        for r in 0..batch.rows() {
            f((0..batch.fields().len())
                .map(|c| batch.column(c).get(r))
                .collect());
        }
    }
}

pub fn table(rows: &Rows) -> String {
    let mut t = Table::new();
    t.load_style(UTF8_FULL);
    t.set_header(rows.fields.iter().map(|f| f.name.as_str()));
    each_row(rows, |row| {
        t.add_row(
            row.iter()
                .map(|v| v.to_text().unwrap_or_else(|| "NULL".to_string())),
        );
    });
    t.to_string()
}

pub fn csv(rows: &Rows) -> Result<String, csv::Error> {
    let mut w = csv::Writer::from_writer(Vec::new());
    w.write_record(rows.fields.iter().map(|f| f.name.as_str()))?;
    let mut failed = None;
    each_row(rows, |row| {
        if failed.is_none() {
            let cells = row.iter().map(|v| v.to_text().unwrap_or_default());
            failed = w.write_record(cells).err();
        }
    });
    if let Some(e) = failed {
        return Err(e);
    }
    let bytes = w.into_inner().map_err(|e| e.into_error())?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// One JSON object per row, keys in column order (a `serde_json::Map` would sort them).
fn object(rows: &Rows, row: &[Value]) -> String {
    let mut out = String::from("{");
    for (i, (field, v)) in rows.fields.iter().zip(row).enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&serde_json::Value::String(field.name.clone()).to_string());
        out.push(':');
        out.push_str(&value_json(v).to_string());
    }
    out.push('}');
    out
}

/// One array of objects, one object per line.
pub fn json(rows: &Rows) -> String {
    let mut lines = Vec::new();
    each_row(rows, |row| lines.push(object(rows, &row)));
    if lines.is_empty() {
        return "[]\n".to_string();
    }
    format!("[\n{}\n]\n", lines.join(",\n"))
}

pub fn ndjson(rows: &Rows) -> String {
    let mut out = String::new();
    each_row(rows, |row| {
        out.push_str(&object(rows, &row));
        out.push('\n');
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Batch, Column, Field, QueryStats};
    use crate::types::DataType;

    fn sample() -> Rows {
        let fields = vec![
            Field {
                name: "name".to_string(),
                ty: DataType::String,
            },
            Field {
                name: "n".to_string(),
                ty: DataType::Int64,
            },
        ];
        let names = [Value::String("a,\"b\"".into()), Value::Null];
        let counts = [Value::Int64(1), Value::Null];
        let cols = vec![
            Column::from_values(&DataType::String, &names).unwrap(),
            Column::from_values(&DataType::Int64, &counts).unwrap(),
        ];
        Rows {
            batches: vec![Batch::new(fields.clone(), cols).unwrap()],
            fields,
            stats: QueryStats::default(),
            warnings: Vec::new(),
        }
    }

    /// Fails if a comma or quote is not escaped or NULL is not an empty cell.
    #[test]
    fn csv_quotes_and_renders_null_empty() {
        assert_eq!(csv(&sample()).unwrap(), "name,n\n\"a,\"\"b\"\"\",1\n,\n");
    }

    /// Fails if NULL is not `null`, a string is not escaped or key order is not column order.
    #[test]
    fn ndjson_one_object_per_line() {
        assert_eq!(
            ndjson(&sample()),
            "{\"name\":\"a,\\\"b\\\"\",\"n\":1}\n{\"name\":null,\"n\":null}\n"
        );
    }

    #[test]
    fn json_is_one_array_and_table_marks_null() {
        let parsed: serde_json::Value = serde_json::from_str(&json(&sample())).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 2);
        assert!(table(&sample()).contains("NULL"));
    }
}
