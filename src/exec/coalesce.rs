//! `coalesce_companion`: the kernel behind `Expr::Func`'s `CoalesceText`, contract rule 6's
//! read side. It is not the default read of `<name>`, which keeps its own type rather than
//! widening to STRING (SPEC §3, Q2); only an explicit `coalesce_text()` call reaches here.

use crate::types::{DataType, Value};

use super::column::{Column, ColumnBuilder, ColumnError};

/// `companion` must be STRING and the same length as `primary`, or this returns an error.
pub fn coalesce_companion(primary: &Column, companion: &Column) -> Result<Column, ColumnError> {
    if companion.data_type() != &DataType::String {
        return Err(ColumnError::NotString(companion.data_type().clone()));
    }
    if primary.len() != companion.len() {
        return Err(ColumnError::LengthMismatch {
            left: primary.len(),
            right: companion.len(),
        });
    }

    let mut builder = ColumnBuilder::with_capacity(DataType::String, primary.len());
    for i in 0..primary.len() {
        if primary.is_null(i) {
            builder.push(&companion.get(i))?;
        } else {
            let text = primary
                .get(i)
                .to_text()
                .expect("a non-null value always has text");
            builder.push(&Value::String(text))?;
        }
    }
    Ok(builder.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(ty: &DataType, values: &[Value]) -> Column {
        Column::from_values(ty, values).unwrap()
    }

    fn strings(col: &Column) -> Vec<Value> {
        (0..col.len()).map(|i| col.get(i)).collect()
    }

    #[test]
    fn companion_fills_only_where_primary_is_null() {
        let primary = col(
            &DataType::Int64,
            &[Value::Int64(1), Value::Null, Value::Null],
        );
        let companion = col(
            &DataType::String,
            &[Value::Null, Value::String("x".to_string()), Value::Null],
        );
        let out = coalesce_companion(&primary, &companion).unwrap();
        assert_eq!(
            strings(&out),
            vec![
                Value::String("1".to_string()),
                Value::String("x".to_string()),
                Value::Null
            ]
        );
    }

    #[test]
    fn primary_wins_when_both_are_set() {
        let primary = col(&DataType::Int64, &[Value::Int64(1)]);
        let companion = col(&DataType::String, &[Value::String("x".to_string())]);
        let out = coalesce_companion(&primary, &companion).unwrap();
        assert_eq!(strings(&out), vec![Value::String("1".to_string())]);
    }

    #[test]
    fn non_string_companion_is_rejected() {
        let primary = col(&DataType::Int64, &[Value::Int64(1)]);
        let companion = col(&DataType::Int64, &[Value::Int64(2)]);
        let err = coalesce_companion(&primary, &companion).unwrap_err();
        assert_eq!(err, ColumnError::NotString(DataType::Int64));
    }

    #[test]
    fn length_mismatch_is_rejected() {
        let primary = col(&DataType::Int64, &[Value::Int64(1), Value::Int64(2)]);
        let companion = col(&DataType::String, &[Value::Null, Value::Null, Value::Null]);
        let err = coalesce_companion(&primary, &companion).unwrap_err();
        assert_eq!(err, ColumnError::LengthMismatch { left: 2, right: 3 });
    }

    #[test]
    fn float_primary_nan_renders_as_nan_text() {
        let primary = col(&DataType::Float64, &[Value::Float64(f64::NAN)]);
        let companion = col(&DataType::String, &[Value::Null]);
        let out = coalesce_companion(&primary, &companion).unwrap();
        assert_eq!(strings(&out), vec![Value::String("NaN".to_string())]);
    }
}
