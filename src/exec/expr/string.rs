//! String scalar kernels (adelie-1st.1): lower, upper, length, substr, concat. Each row is
//! independent; NULL propagates. `func_type` has already checked every arg is STRING/INT64.

use crate::exec::{Column, ColumnBuilder, ExecError};
use crate::types::{DataType, Value};

fn str_at(col: &Column, i: usize) -> String {
    let Value::String(s) = col.get(i) else {
        unreachable!("func_type checked this column is STRING")
    };
    s
}

fn int_at(col: &Column, i: usize) -> i64 {
    let Value::Int64(n) = col.get(i) else {
        unreachable!("func_type checked this column is INT64")
    };
    n
}

fn map_string(col: &Column, f: impl Fn(&str) -> String) -> Result<Column, ExecError> {
    let mut b = ColumnBuilder::with_capacity(DataType::String, col.len());
    for i in 0..col.len() {
        if col.is_null(i) {
            b.push_null();
            continue;
        }
        b.push(&Value::String(f(&str_at(col, i))))?;
    }
    Ok(b.finish())
}

pub(crate) fn lower(col: &Column) -> Result<Column, ExecError> {
    map_string(col, str::to_lowercase)
}

pub(crate) fn upper(col: &Column) -> Result<Column, ExecError> {
    map_string(col, str::to_uppercase)
}

/// Unicode scalar count, not bytes (`length('héllo')` is 5).
pub(crate) fn length(col: &Column) -> Result<Column, ExecError> {
    let mut b = ColumnBuilder::with_capacity(DataType::Int64, col.len());
    for i in 0..col.len() {
        if col.is_null(i) {
            b.push_null();
            continue;
        }
        b.push(&Value::Int64(str_at(col, i).chars().count() as i64))?;
    }
    Ok(b.finish())
}

pub(crate) fn concat(l: &Column, r: &Column) -> Result<Column, ExecError> {
    let mut b = ColumnBuilder::with_capacity(DataType::String, l.len());
    for i in 0..l.len() {
        if l.is_null(i) || r.is_null(i) {
            b.push_null();
            continue;
        }
        b.push(&Value::String(str_at(l, i) + str_at(r, i).as_str()))?;
    }
    Ok(b.finish())
}

/// PG/DuckDB 1-based-char rules: the window `[start, start+len)` (1-based, `len` omitted means
/// "to the end") intersected with the string's own `[1, count+1)`. `i128` avoids overflow.
fn substr_one(chars: &[char], start: i64, len: Option<i64>) -> Result<String, ExecError> {
    let count = chars.len() as i128;
    let start = start as i128;
    let lo = start.max(1);
    let hi = match len {
        Some(l) if l < 0 => {
            return Err(ExecError::Invalid(format!(
                "substr length must not be negative, found {l}"
            )));
        }
        Some(l) => (start + l as i128).min(count + 1),
        None => count + 1,
    };
    if hi <= lo {
        return Ok(String::new());
    }
    Ok(chars[(lo - 1) as usize..(hi - 1) as usize].iter().collect())
}

pub(crate) fn substr(
    s: &Column,
    start: &Column,
    len: Option<&Column>,
) -> Result<Column, ExecError> {
    let mut b = ColumnBuilder::with_capacity(DataType::String, s.len());
    for i in 0..s.len() {
        if s.is_null(i) || start.is_null(i) || len.is_some_and(|c| c.is_null(i)) {
            b.push_null();
            continue;
        }
        let chars: Vec<char> = str_at(s, i).chars().collect();
        let len_i = len.map(|c| int_at(c, i));
        b.push(&Value::String(substr_one(&chars, int_at(start, i), len_i)?))?;
    }
    Ok(b.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn str_col(vals: &[Option<&str>]) -> Column {
        let values: Vec<Value> = vals
            .iter()
            .map(|v| v.map_or(Value::Null, |s| Value::String(s.to_string())))
            .collect();
        Column::from_values(&DataType::String, &values).unwrap()
    }

    fn int_col(vals: &[Option<i64>]) -> Column {
        let values: Vec<Value> = vals
            .iter()
            .map(|v| v.map_or(Value::Null, Value::Int64))
            .collect();
        Column::from_values(&DataType::Int64, &values).unwrap()
    }

    fn text_at(col: &Column, i: usize) -> Option<String> {
        match col.get(i) {
            Value::Null => None,
            Value::String(s) => Some(s),
            other => panic!("expected STRING or NULL, got {other:?}"),
        }
    }

    #[test]
    fn lower_upper_and_null_propagation() {
        let col = str_col(&[Some("Charlie"), None]);
        assert_eq!(text_at(&lower(&col).unwrap(), 0), Some("charlie".into()));
        assert_eq!(text_at(&lower(&col).unwrap(), 1), None);
        assert_eq!(text_at(&upper(&col).unwrap(), 0), Some("CHARLIE".into()));
    }

    /// Falsify: fails if `length` counts UTF-8 bytes instead of chars ('é' is 2 bytes).
    #[test]
    fn length_counts_unicode_scalars_not_bytes() {
        let col = str_col(&[Some("héllo"), None]);
        let out = length(&col).unwrap();
        assert_eq!(out.get(0), Value::Int64(5));
        assert!(out.is_null(1));
    }

    #[test]
    fn concat_is_null_if_either_side_is_null() {
        let a = str_col(&[Some("foo"), None, Some("foo")]);
        let b = str_col(&[Some("bar"), Some("bar"), None]);
        let out = concat(&a, &b).unwrap();
        assert_eq!(text_at(&out, 0), Some("foobar".into()));
        assert_eq!(text_at(&out, 1), None);
        assert_eq!(text_at(&out, 2), None);
    }

    fn substr3(s: &str, start: i64, len: i64) -> String {
        let col = str_col(&[Some(s)]);
        let start_col = int_col(&[Some(start)]);
        let len_col = int_col(&[Some(len)]);
        text_at(&substr(&col, &start_col, Some(&len_col)).unwrap(), 0).unwrap()
    }

    #[test]
    fn substr_start_le_zero_counts_against_len() {
        assert_eq!(substr3("hello", 0, 2), "h");
        assert_eq!(substr3("hello", -2, 4), "h");
    }

    #[test]
    fn substr_beyond_the_end_is_empty() {
        assert_eq!(substr3("hello", 10, 5), "");
    }

    #[test]
    fn substr_len_zero_is_empty() {
        assert_eq!(substr3("hello", 1, 0), "");
    }

    #[test]
    fn substr_without_len_goes_to_the_end() {
        let col = str_col(&[Some("hello")]);
        let start_col = int_col(&[Some(2)]);
        let out = substr(&col, &start_col, None).unwrap();
        assert_eq!(text_at(&out, 0), Some("ello".into()));
    }

    #[test]
    fn substr_negative_len_is_invalid() {
        let col = str_col(&[Some("hello")]);
        let start_col = int_col(&[Some(1)]);
        let len_col = int_col(&[Some(-1)]);
        let err = substr(&col, &start_col, Some(&len_col)).unwrap_err();
        assert!(matches!(err, ExecError::Invalid(_)));
    }

    #[test]
    fn substr_null_in_any_arg_is_null() {
        let col = str_col(&[None, Some("hello"), Some("hello")]);
        let start_col = int_col(&[Some(1), None, Some(1)]);
        let len_col = int_col(&[Some(1), Some(1), None]);
        let out = substr(&col, &start_col, Some(&len_col)).unwrap();
        assert!(out.is_null(0));
        assert!(out.is_null(1));
        assert!(out.is_null(2));
    }
}
