//! `RowBuilder`: the one place INSERT and COPY route a value onto a table's full field list
//! (D0016). `read_csv` and `read_ndjson` are COPY's hand-rolled readers, both built on it.

mod csv;
mod json;

use crate::exec::{Batch, BatchError, ColumnBuilder, Field};
use crate::types::{COMPANION_SUFFIX, DataType, Value, companion_name};

pub use csv::read_csv;
pub use json::read_ndjson;

/// Builds one full-width `Batch` at a time from a table's field list, applying the D0016
/// routing rule: a value that does not fit its column goes to the declared `<col>::string`
/// companion as text, or the row fails naming the column.
pub struct RowBuilder {
    fields: Vec<Field>,
    builders: Vec<ColumnBuilder>,
    data_columns: Vec<usize>,
    /// `companion_of[i]` is the field index of `fields[i]`'s companion, if it declares one.
    companion_of: Vec<Option<usize>>,
    set_this_row: Vec<bool>,
    /// Rows completed since the last `finish`, i.e. sitting in the builders right now.
    pending_rows: usize,
    /// Rows completed over this `RowBuilder`'s whole lifetime, never reset by `finish`, so
    /// error rows stay 1-based across batches.
    total_rows: u64,
}

impl RowBuilder {
    pub fn new(fields: &[Field]) -> RowBuilder {
        let is_companion: Vec<bool> = fields
            .iter()
            .map(|f| {
                f.name
                    .strip_suffix(COMPANION_SUFFIX)
                    .is_some_and(|base| fields.iter().any(|g| g.name == base))
            })
            .collect();
        let companion_of = fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                if is_companion[i] {
                    return None;
                }
                let target = companion_name(&f.name);
                fields.iter().position(|g| g.name == target)
            })
            .collect();
        let data_columns = (0..fields.len()).filter(|&i| !is_companion[i]).collect();
        let builders = fields
            .iter()
            .map(|f| ColumnBuilder::with_capacity(f.ty.clone(), 0))
            .collect();
        RowBuilder {
            fields: fields.to_vec(),
            builders,
            data_columns,
            companion_of,
            set_this_row: vec![false; fields.len()],
            pending_rows: 0,
            total_rows: 0,
        }
    }

    /// Indices of the non-companion fields, in field order.
    pub fn data_columns(&self) -> &[usize] {
        &self.data_columns
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }

    /// The 1-based row currently being built, for error messages.
    fn current_row(&self) -> u64 {
        self.total_rows + 1
    }

    fn does_not_fit(&self, col: usize, v: &Value) -> IngestError {
        IngestError::DoesNotFit {
            column: self.fields[col].name.clone(),
            value: v.to_text().unwrap_or_else(|| format!("{v:?}")),
            row: self.current_row(),
        }
    }

    fn already_set(&self, col: usize) -> IngestError {
        IngestError::Malformed {
            row: self.current_row(),
            message: format!("column {} set twice in one row", self.fields[col].name),
        }
    }

    /// `types::coerce`, else the declared companion (as `v.to_text()`), else `DoesNotFit`.
    /// A companion column itself only ever coerces STRING and NULL, so this also implements
    /// "setting a companion directly" without a separate branch.
    pub fn set_value(&mut self, col: usize, v: Value) -> Result<(), IngestError> {
        if self.set_this_row[col] {
            return Err(self.already_set(col));
        }
        if self.builders[col].push(&v).is_ok() {
            self.set_this_row[col] = true;
            return Ok(());
        }
        let Some(comp) = self.companion_of[col] else {
            return Err(self.does_not_fit(col, &v));
        };
        let Some(text) = v.to_text() else {
            return Err(self.does_not_fit(col, &v));
        };
        if self.set_this_row[comp] {
            return Err(self.already_set(comp));
        }
        self.builders[col].push_null();
        self.builders[comp]
            .push(&Value::String(text))
            .expect("STRING always fits a STRING companion column");
        self.set_this_row[col] = true;
        self.set_this_row[comp] = true;
        Ok(())
    }

    /// `Value::from_text`, else the declared companion (verbatim), else `DoesNotFit`. A
    /// STRING column (primary or companion) always takes `text` as-is; `""` is NULL for
    /// every other type, matching DuckDB's CSV reader.
    pub fn set_text(&mut self, col: usize, text: &str) -> Result<(), IngestError> {
        if self.set_this_row[col] {
            return Err(self.already_set(col));
        }
        let ty = self.fields[col].ty.clone();
        if matches!(ty, DataType::String) {
            self.builders[col]
                .push(&Value::String(text.to_string()))
                .expect("STRING always fits a STRING column");
            self.set_this_row[col] = true;
            return Ok(());
        }
        if text.is_empty() {
            self.builders[col].push_null();
            self.set_this_row[col] = true;
            return Ok(());
        }
        if let Some(v) = Value::from_text(text, &ty) {
            self.builders[col]
                .push(&v)
                .expect("from_text already produced a value of this column's type");
            self.set_this_row[col] = true;
            return Ok(());
        }
        let Some(comp) = self.companion_of[col] else {
            return Err(IngestError::DoesNotFit {
                column: self.fields[col].name.clone(),
                value: text.to_string(),
                row: self.current_row(),
            });
        };
        if self.set_this_row[comp] {
            return Err(self.already_set(comp));
        }
        self.builders[col].push_null();
        self.builders[comp]
            .push(&Value::String(text.to_string()))
            .expect("STRING always fits a STRING companion column");
        self.set_this_row[col] = true;
        self.set_this_row[comp] = true;
        Ok(())
    }

    /// NULL for every column not set this row. Both halves of a routed pair are always set
    /// together (see `set_value`/`set_text`), so column lengths never diverge.
    pub fn end_row(&mut self) -> Result<(), IngestError> {
        for (i, set) in self.set_this_row.iter_mut().enumerate() {
            if !*set {
                self.builders[i].push_null();
            }
            *set = false;
        }
        self.pending_rows += 1;
        self.total_rows += 1;
        Ok(())
    }

    /// Rows built since the last `finish`.
    pub fn rows(&self) -> usize {
        self.pending_rows
    }

    /// Drains the builders into a `Batch` and resets them; `total_rows` keeps counting.
    pub fn finish(&mut self) -> Result<Batch, IngestError> {
        let columns = self
            .builders
            .iter_mut()
            .zip(&self.fields)
            .map(|(b, f)| std::mem::replace(b, ColumnBuilder::new(f.ty.clone())).finish())
            .collect();
        let batch = Batch::new(self.fields.clone(), columns)
            .map_err(|e: BatchError| IngestError::Batch(e.to_string()))?;
        self.pending_rows = 0;
        Ok(batch)
    }
}

/// A rejected row or batch (C4). `row` is always 1-based and keeps counting across batches.
#[derive(Debug, Clone, PartialEq)]
pub enum IngestError {
    DoesNotFit {
        column: String,
        value: String,
        row: u64,
    },
    UnknownColumn {
        column: String,
        row: u64,
    },
    Malformed {
        row: u64,
        message: String,
    },
    Batch(String),
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestError::DoesNotFit { column, value, row } => {
                write!(f, "row {row}: value {value:?} does not fit column {column}")
            }
            IngestError::UnknownColumn { column, row } => {
                write!(f, "row {row}: unknown column {column}")
            }
            IngestError::Malformed { row, message } => write!(f, "row {row}: {message}"),
            IngestError::Batch(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for IngestError {}

/// COPY CSV options (SPEC §8). Defaults match `CsvOptions::default()`: a header row, comma
/// delimited.
pub struct CsvOptions {
    pub header: bool,
    pub delimiter: u8,
}

impl Default for CsvOptions {
    fn default() -> Self {
        CsvOptions {
            header: true,
            delimiter: b',',
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, ty: DataType) -> Field {
        Field {
            name: name.to_string(),
            ty,
        }
    }

    fn fields_with_companion() -> Vec<Field> {
        vec![
            field("x", DataType::Int64),
            field("x::string", DataType::String),
        ]
    }

    fn values(batch: &Batch, col: usize) -> Vec<Value> {
        (0..batch.rows())
            .map(|i| batch.column(col).get(i))
            .collect()
    }

    #[test]
    fn routing_onto_an_already_set_companion_is_malformed_not_a_second_push() {
        let fields = fields_with_companion();
        let mut b = RowBuilder::new(&fields);
        b.set_value(1, Value::String("explicit".to_string()))
            .unwrap();
        assert!(matches!(
            b.set_text(0, "abc"),
            Err(IngestError::Malformed { .. })
        ));
        assert!(matches!(
            b.set_value(0, Value::String("abc".to_string())),
            Err(IngestError::Malformed { .. })
        ));
    }

    #[test]
    fn set_value_routes_a_typed_value_that_does_not_coerce_to_the_companion() {
        let fields = fields_with_companion();
        let mut b = RowBuilder::new(&fields);
        b.set_value(0, Value::String("500".to_string())).unwrap();
        b.end_row().unwrap();
        let batch = b.finish().unwrap();
        assert_eq!(values(&batch, 0), vec![Value::Null]);
        assert_eq!(values(&batch, 1), vec![Value::String("500".to_string())]);
    }

    #[test]
    fn set_text_parses_into_the_primary_leaving_the_companion_null() {
        let fields = fields_with_companion();
        let mut b = RowBuilder::new(&fields);
        b.set_text(0, "500").unwrap();
        b.end_row().unwrap();
        let batch = b.finish().unwrap();
        assert_eq!(values(&batch, 0), vec![Value::Int64(500)]);
        assert_eq!(values(&batch, 1), vec![Value::Null]);
    }

    #[test]
    fn set_text_with_no_companion_is_does_not_fit_naming_the_column_and_row() {
        let fields = vec![field("x", DataType::Int64)];
        let mut b = RowBuilder::new(&fields);
        let err = b.set_text(0, "abc").unwrap_err();
        assert_eq!(
            err,
            IngestError::DoesNotFit {
                column: "x".to_string(),
                value: "abc".to_string(),
                row: 1,
            }
        );
    }

    #[test]
    fn an_unset_column_is_null() {
        let fields = vec![field("a", DataType::Int64), field("b", DataType::String)];
        let mut b = RowBuilder::new(&fields);
        b.set_value(0, Value::Int64(1)).unwrap();
        b.end_row().unwrap();
        let batch = b.finish().unwrap();
        assert_eq!(values(&batch, 1), vec![Value::Null]);
    }

    #[test]
    fn setting_the_same_column_twice_is_malformed() {
        let fields = vec![field("a", DataType::Int64)];
        let mut b = RowBuilder::new(&fields);
        b.set_value(0, Value::Int64(1)).unwrap();
        let err = b.set_value(0, Value::Int64(2)).unwrap_err();
        assert!(matches!(err, IngestError::Malformed { row: 1, .. }));
    }

    #[test]
    fn finish_resets_the_builder_and_a_second_batch_continues_the_row_numbers() {
        let fields = vec![field("a", DataType::Int64)];
        let mut b = RowBuilder::new(&fields);
        for _ in 0..5 {
            b.set_value(0, Value::Int64(1)).unwrap();
            b.end_row().unwrap();
        }
        let first = b.finish().unwrap();
        assert_eq!(first.rows(), 5);
        assert_eq!(b.rows(), 0);

        b.set_value(0, Value::Int64(2)).unwrap();
        b.end_row().unwrap();
        let second = b.finish().unwrap();
        assert_eq!(second.rows(), 1);

        // Row 7 fails; the counter kept running through both finished batches instead of
        // resetting.
        let err = b.set_text(0, "not a number").unwrap_err();
        assert_eq!(
            err,
            IngestError::DoesNotFit {
                column: "a".to_string(),
                value: "not a number".to_string(),
                row: 7,
            }
        );
    }

    #[test]
    fn data_columns_excludes_the_companion() {
        let fields = fields_with_companion();
        let b = RowBuilder::new(&fields);
        assert_eq!(b.data_columns(), &[0]);
    }

    #[test]
    fn setting_a_companion_directly_takes_string_or_null_only() {
        let fields = fields_with_companion();
        let mut b = RowBuilder::new(&fields);
        let comp = b.index_of("x::string").unwrap();
        let err = b.set_value(comp, Value::Int64(5)).unwrap_err();
        assert!(matches!(err, IngestError::DoesNotFit { .. }));

        let mut b = RowBuilder::new(&fields);
        let comp = b.index_of("x::string").unwrap();
        b.set_value(comp, Value::String("hi".to_string())).unwrap();
        b.end_row().unwrap();
        let batch = b.finish().unwrap();
        assert_eq!(values(&batch, 1), vec![Value::String("hi".to_string())]);
    }
}
