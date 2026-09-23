//! `Batch`: a set of same-length, named `Column`s — what operators pass to one another
//! (SPEC §7). `BATCH_ROWS` is the target size a producer aims for, not a hard cap.

use crate::types::DataType;

use super::column::Column;

/// A named, typed slot in a `Batch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub ty: DataType,
}

/// One column batch: `columns[i]` has type `fields[i].ty` and `rows()` rows, for every `i`.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    fields: Vec<Field>,
    columns: Vec<Column>,
    rows: usize,
}

impl Batch {
    /// Validates, in order: column count, each column's type against its field, equal row
    /// counts, then unique field names. Zero columns gives `rows() == 0`.
    pub fn new(fields: Vec<Field>, columns: Vec<Column>) -> Result<Batch, BatchError> {
        if fields.len() != columns.len() {
            return Err(BatchError::ColumnCount {
                fields: fields.len(),
                columns: columns.len(),
            });
        }
        for (f, c) in fields.iter().zip(&columns) {
            if c.data_type() != &f.ty {
                return Err(BatchError::TypeMismatch {
                    field: f.name.clone(),
                    expected: f.ty.clone(),
                    found: c.data_type().clone(),
                });
            }
        }
        let rows = columns.first().map_or(0, Column::len);
        for (f, c) in fields.iter().zip(&columns) {
            if c.len() != rows {
                return Err(BatchError::RowCount {
                    field: f.name.clone(),
                    expected: rows,
                    found: c.len(),
                });
            }
        }
        for (i, field) in fields.iter().enumerate() {
            if fields[..i].iter().any(|f| f.name == field.name) {
                return Err(BatchError::DuplicateField(field.name.clone()));
            }
        }
        Ok(Batch {
            fields,
            columns,
            rows,
        })
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub fn column(&self, i: usize) -> &Column {
        &self.columns[i]
    }

    pub fn column_by_name(&self, name: &str) -> Option<&Column> {
        let i = self.fields.iter().position(|f| f.name == name)?;
        Some(&self.columns[i])
    }

    /// The sum of every column's `byte_size()`.
    pub fn byte_size(&self) -> usize {
        self.columns.iter().map(Column::byte_size).sum()
    }
}

/// A rejected `Batch::new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchError {
    ColumnCount {
        fields: usize,
        columns: usize,
    },
    TypeMismatch {
        field: String,
        expected: DataType,
        found: DataType,
    },
    RowCount {
        field: String,
        expected: usize,
        found: usize,
    },
    DuplicateField(String),
}

impl std::fmt::Display for BatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BatchError::ColumnCount { fields, columns } => {
                write!(f, "{fields} fields but {columns} columns")
            }
            BatchError::TypeMismatch {
                field,
                expected,
                found,
            } => {
                write!(f, "field {field}: expected {expected}, found {found}")
            }
            BatchError::RowCount {
                field,
                expected,
                found,
            } => {
                write!(f, "field {field}: expected {expected} rows, found {found}")
            }
            BatchError::DuplicateField(name) => write!(f, "duplicate field name {name}"),
        }
    }
}

impl std::error::Error for BatchError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;

    fn int_field(name: &str) -> Field {
        Field {
            name: name.to_string(),
            ty: DataType::Int64,
        }
    }

    fn int_col(values: &[i64]) -> Column {
        let values: Vec<Value> = values.iter().copied().map(Value::Int64).collect();
        Column::from_values(&DataType::Int64, &values).unwrap()
    }

    #[test]
    fn happy_path_reports_rows_and_looks_up_by_name() {
        let batch = Batch::new(
            vec![int_field("a"), int_field("b")],
            vec![int_col(&[1, 2]), int_col(&[3, 4])],
        )
        .unwrap();
        assert_eq!(batch.rows(), 2);
        assert_eq!(batch.column_by_name("b"), Some(batch.column(1)));
        assert_eq!(batch.column_by_name("missing"), None);
    }

    #[test]
    fn zero_columns_gives_zero_rows() {
        let batch = Batch::new(vec![], vec![]).unwrap();
        assert_eq!(batch.rows(), 0);
    }

    #[test]
    fn column_count_mismatch_is_rejected() {
        let err = Batch::new(vec![int_field("a")], vec![]).unwrap_err();
        assert_eq!(
            err,
            BatchError::ColumnCount {
                fields: 1,
                columns: 0
            }
        );
    }

    #[test]
    fn type_mismatch_is_rejected() {
        let field = Field {
            name: "a".to_string(),
            ty: DataType::UInt64,
        };
        let err = Batch::new(vec![field], vec![int_col(&[1])]).unwrap_err();
        assert_eq!(
            err,
            BatchError::TypeMismatch {
                field: "a".to_string(),
                expected: DataType::UInt64,
                found: DataType::Int64,
            }
        );
    }

    #[test]
    fn row_count_mismatch_is_rejected() {
        let err = Batch::new(
            vec![int_field("a"), int_field("b")],
            vec![int_col(&[1, 2]), int_col(&[1])],
        )
        .unwrap_err();
        assert_eq!(
            err,
            BatchError::RowCount {
                field: "b".to_string(),
                expected: 2,
                found: 1
            }
        );
    }

    #[test]
    fn duplicate_field_name_is_rejected() {
        let err = Batch::new(
            vec![int_field("a"), int_field("a")],
            vec![int_col(&[1]), int_col(&[1])],
        )
        .unwrap_err();
        assert_eq!(err, BatchError::DuplicateField("a".to_string()));
    }

    #[test]
    fn byte_size_is_the_sum_of_column_byte_sizes() {
        let batch = Batch::new(
            vec![int_field("a"), int_field("b")],
            vec![int_col(&[1, 2]), int_col(&[3, 4])],
        )
        .unwrap();
        let expected: usize = batch.columns().iter().map(Column::byte_size).sum();
        assert_eq!(batch.byte_size(), expected);
    }
}
