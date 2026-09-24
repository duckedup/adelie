//! The ORDER BY sort `append::flush`/`merge` apply (SPEC §18), reusable by `latest`/`rollup`
//! later: a stable sort over the ORDER BY columns under `types::total_cmp`, NULL sorting
//! after every non-null value.

use std::cmp::Ordering;

use crate::exec::{BATCH_ROWS, Batch, Column, ColumnBuilder, Field};
use crate::storage::Error;
use crate::types::{Value, total_cmp};

/// Rows of `batches` (all with `fields`) in `order` order: a stable sort by each column of
/// `order` (indices into `fields`) under `types::total_cmp`, NULL after every value. Returns
/// batches of at most `exec::BATCH_ROWS` rows; value-at-a-time (a typed kernel is the fix later).
pub(crate) fn sort_rows(
    fields: &[Field],
    batches: &[Batch],
    order: &[usize],
) -> Result<Vec<Batch>, Error> {
    let mut positions: Vec<(usize, usize)> = Vec::new();
    for (bi, batch) in batches.iter().enumerate() {
        positions.extend((0..batch.rows()).map(|ri| (bi, ri)));
    }

    // Precomputed once, so the comparator below never calls `Column::get` again.
    let keys: Vec<Vec<Value>> = positions
        .iter()
        .map(|&(bi, ri)| {
            order
                .iter()
                .map(|&col| batches[bi].column(col).get(ri))
                .collect()
        })
        .collect();

    let mut idx: Vec<usize> = (0..positions.len()).collect();
    idx.sort_by(|&a, &b| compare_keys(&keys[a], &keys[b]));

    let mut out = Vec::new();
    for chunk in idx.chunks(BATCH_ROWS) {
        let mut builders: Vec<ColumnBuilder> = fields
            .iter()
            .map(|f| ColumnBuilder::with_capacity(f.ty.clone(), chunk.len()))
            .collect();
        for &i in chunk {
            let (bi, ri) = positions[i];
            let row = &batches[bi];
            for (col, builder) in builders.iter_mut().enumerate() {
                let v = row.column(col).get(ri);
                builder
                    .push(&v)
                    .expect("a value read from a column always fits that same column's type");
            }
        }
        let columns: Vec<Column> = builders.into_iter().map(ColumnBuilder::finish).collect();
        out.push(
            Batch::new(fields.to_vec(), columns)
                .expect("gathered columns share fields' types and row count by construction"),
        );
    }
    Ok(out)
}

/// Lexicographic over each key column; the first non-equal column decides.
fn compare_keys(a: &[Value], b: &[Value]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        match compare_one(x, y) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

/// NULL sorts after every non-null value and equals another NULL; `total_cmp` never returns
/// `None` for two non-null values of one column's type (SPEC §3).
fn compare_one(a: &Value, b: &Value) -> Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => total_cmp(a, b).expect("same-type non-null values always compare"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DataType;

    fn field(name: &str, ty: DataType) -> Field {
        Field {
            name: name.to_string(),
            ty,
        }
    }

    fn two_col_batch(fields: &[Field], a: &[Value], b: &[Value]) -> Batch {
        Batch::new(
            fields.to_vec(),
            vec![
                Column::from_values(&fields[0].ty, a).unwrap(),
                Column::from_values(&fields[1].ty, b).unwrap(),
            ],
        )
        .unwrap()
    }

    fn one_col_values(out: &[Batch], col: usize) -> Vec<Value> {
        out.iter()
            .flat_map(|b| (0..b.rows()).map(move |i| b.column(col).get(i)))
            .collect()
    }

    #[test]
    fn sorts_across_batches_null_last_and_stable_on_ties() {
        let fields = [field("a", DataType::Int64), field("b", DataType::Int64)];
        let batch1 = two_col_batch(
            &fields,
            &[Value::Int64(3), Value::Null, Value::Int64(1)],
            &[Value::Int64(10), Value::Int64(11), Value::Int64(12)],
        );
        let batch2 = two_col_batch(
            &fields,
            &[Value::Int64(2), Value::Int64(1)],
            &[Value::Int64(20), Value::Int64(21)],
        );
        let out = sort_rows(&fields, &[batch1, batch2], &[0]).unwrap();
        assert_eq!(
            one_col_values(&out, 0),
            vec![
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(2),
                Value::Int64(3),
                Value::Null,
            ]
        );
        // The two `a == 1` rows keep arrival order (batch1's row before batch2's): a stable sort.
        assert_eq!(
            one_col_values(&out, 1),
            vec![
                Value::Int64(12),
                Value::Int64(21),
                Value::Int64(20),
                Value::Int64(10),
                Value::Int64(11),
            ]
        );
    }

    #[test]
    fn two_column_order_by_breaks_ties_on_the_second_column() {
        let fields = [field("a", DataType::Int64), field("b", DataType::Int64)];
        let batch = two_col_batch(
            &fields,
            &[Value::Int64(1), Value::Int64(1), Value::Int64(2)],
            &[Value::Int64(5), Value::Int64(3), Value::Int64(9)],
        );
        let out = sort_rows(&fields, &[batch], &[0, 1]).unwrap();
        assert_eq!(
            one_col_values(&out, 0),
            vec![Value::Int64(1), Value::Int64(1), Value::Int64(2)]
        );
        assert_eq!(
            one_col_values(&out, 1),
            vec![Value::Int64(3), Value::Int64(5), Value::Int64(9)]
        );
    }

    #[test]
    fn float64_sorts_nan_greatest_and_folds_signed_zero() {
        let fields = [field("a", DataType::Float64)];
        let vals: Vec<Value> = [f64::NAN, 1.0, -0.0, 0.0]
            .into_iter()
            .map(Value::Float64)
            .collect();
        let batch = Batch::new(
            fields.to_vec(),
            vec![Column::from_values(&DataType::Float64, &vals).unwrap()],
        )
        .unwrap();
        let out = sort_rows(&fields, &[batch], &[0]).unwrap();
        let sorted = one_col_values(&out, 0);
        let Value::Float64(last) = sorted[3] else {
            panic!("expected Float64")
        };
        assert!(last.is_nan(), "NaN must sort last");
        assert_eq!(sorted[0], Value::Float64(0.0));
        assert_eq!(sorted[1], Value::Float64(0.0));
        assert_eq!(sorted[2], Value::Float64(1.0));
    }

    #[test]
    fn output_is_chunked_at_batch_rows() {
        let fields = [field("a", DataType::Int64)];
        let n = 5000i64;
        let vals: Vec<Value> = (0..n).rev().map(Value::Int64).collect();
        let batch = Batch::new(
            fields.to_vec(),
            vec![Column::from_values(&DataType::Int64, &vals).unwrap()],
        )
        .unwrap();
        let out = sort_rows(&fields, &[batch], &[0]).unwrap();
        let rows_per: Vec<usize> = out.iter().map(Batch::rows).collect();
        assert_eq!(rows_per, vec![4096, 904]);
        let expected: Vec<Value> = (0..n).map(Value::Int64).collect();
        assert_eq!(one_col_values(&out, 0), expected);
    }
}
