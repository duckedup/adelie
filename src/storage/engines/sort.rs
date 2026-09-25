//! The ORDER BY sort `append::flush`/`merge` apply (SPEC §18): delegates to the exec sort
//! kernel (`exec::ops::sort_batches`), stable, NULL sorting after every non-null value.

use crate::exec::{Batch, ExecContext, Field, SortKey};
use crate::storage::Error;

/// Rows of `batches` (all with `fields`) in `order` order: a stable sort by each column of
/// `order` (indices into `fields`), NULL after every value. Delegates to the exec sort kernel,
/// which also chunks the output at `exec::BATCH_ROWS`, as this used to do itself.
pub(crate) fn sort_rows(
    fields: &[Field],
    batches: &[Batch],
    order: &[usize],
) -> Result<Vec<Batch>, Error> {
    let keys: Vec<SortKey> = order.iter().map(|&c| SortKey::asc(c)).collect();
    crate::exec::ops::sort_batches(fields, batches, &keys, &ExecContext::unlimited())
        .map_err(Error::from)
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

    // --- Oracle: the pre-E5 `sort_rows`, kept verbatim as the test's ground truth. ---

    use std::cmp::Ordering;

    use crate::exec::{BATCH_ROWS, Column, ColumnBuilder};
    use crate::types::{Decimal, Value, total_cmp};

    /// Rows of `batches` (all with `fields`) in `order` order: a stable sort by each column of
    /// `order` (indices into `fields`) under `types::total_cmp`, NULL after every value. Returns
    /// batches of at most `exec::BATCH_ROWS` rows; value-at-a-time (a typed kernel is the fix later).
    fn reference_sort_rows(
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
        idx.sort_by(|&a, &b| reference_compare_keys(&keys[a], &keys[b]));

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
    fn reference_compare_keys(a: &[Value], b: &[Value]) -> Ordering {
        for (x, y) in a.iter().zip(b) {
            match reference_compare_one(x, y) {
                Ordering::Equal => continue,
                other => return other,
            }
        }
        Ordering::Equal
    }

    /// NULL sorts after every non-null value and equals another NULL; `total_cmp` never returns
    /// `None` for two non-null values of one column's type (SPEC §3).
    fn reference_compare_one(a: &Value, b: &Value) -> Ordering {
        match (a.is_null(), b.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => total_cmp(a, b).expect("same-type non-null values always compare"),
        }
    }

    // --- Randomised cross-check against the oracle. ---

    use crate::storage::segment::index::test_support::SplitMix64;

    /// Structural `==`, except FLOAT64 compares bits so two NaNs (or -0.0 and 0.0) that came
    /// from the same original row count as equal.
    fn values_match(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Float64(x), Value::Float64(y)) => x.to_bits() == y.to_bits(),
            _ => a == b,
        }
    }

    fn assert_batches_match(actual: &[Batch], expected: &[Batch], fields: &[Field], case: u64) {
        assert_eq!(
            actual.iter().map(Batch::rows).collect::<Vec<_>>(),
            expected.iter().map(Batch::rows).collect::<Vec<_>>(),
            "case {case}: batch boundaries differ"
        );
        for (bi, (a, e)) in actual.iter().zip(expected).enumerate() {
            for col in 0..fields.len() {
                for row in 0..a.rows() {
                    let (av, ev) = (a.column(col).get(row), e.column(col).get(row));
                    assert!(
                        values_match(&av, &ev),
                        "case {case}, batch {bi}, col {col}, row {row}: {av:?} != {ev:?}"
                    );
                }
            }
        }
    }

    #[derive(Clone, Copy)]
    enum ColKind {
        Int64,
        Float64,
        String,
        Decimal,
        Bool,
    }

    const KINDS: [ColKind; 5] = [
        ColKind::Int64,
        ColKind::Float64,
        ColKind::String,
        ColKind::Decimal,
        ColKind::Bool,
    ];
    const FLOAT_DOMAIN: [f64; 6] = [f64::NAN, -0.0, 0.0, 1.0, -1.0, 2.0];
    const STRING_DOMAIN: [&str; 4] = ["a", "b", "c", "d"];

    impl ColKind {
        fn data_type(self) -> DataType {
            match self {
                ColKind::Int64 => DataType::Int64,
                ColKind::Float64 => DataType::Float64,
                ColKind::String => DataType::String,
                ColKind::Decimal => DataType::decimal(10, 2).unwrap(),
                ColKind::Bool => DataType::Bool,
            }
        }
    }

    /// ~20% NULL, else a small duplicate-heavy domain per kind (FLOAT64's includes NaN, -0.0, 0.0).
    fn random_value(rng: &mut SplitMix64, kind: ColKind) -> Value {
        if rng.next_u64().is_multiple_of(5) {
            return Value::Null;
        }
        match kind {
            ColKind::Int64 => Value::Int64((rng.next_u64() % 5) as i64 - 2),
            ColKind::Float64 => {
                Value::Float64(FLOAT_DOMAIN[(rng.next_u64() % FLOAT_DOMAIN.len() as u64) as usize])
            }
            ColKind::String => {
                let i = (rng.next_u64() % STRING_DOMAIN.len() as u64) as usize;
                Value::String(STRING_DOMAIN[i].to_string())
            }
            ColKind::Decimal => {
                let unscaled = (rng.next_u64() % 5) as i128 - 2;
                Value::Decimal(Decimal::new(unscaled, 2).unwrap())
            }
            ColKind::Bool => Value::Bool(rng.next_u64().is_multiple_of(2)),
        }
    }

    /// A random non-empty, randomly ordered subset of `0..num_cols`, so keys' priority varies.
    fn random_order(rng: &mut SplitMix64, num_cols: usize) -> Vec<usize> {
        let mut order: Vec<usize> = (0..num_cols)
            .filter(|_| rng.next_u64().is_multiple_of(2))
            .collect();
        if order.is_empty() {
            order.push((rng.next_u64() % num_cols as u64) as usize);
        }
        for i in (1..order.len()).rev() {
            let j = (rng.next_u64() % (i as u64 + 1)) as usize;
            order.swap(i, j);
        }
        order
    }

    /// 1–3 batches of 0–40 rows each, over `kinds` plus a unique trailing `seq` column that
    /// exposes tie-order (arrival-order) differences between `sort_rows` and the oracle.
    fn random_batches(rng: &mut SplitMix64, fields: &[Field], kinds: &[ColKind]) -> Vec<Batch> {
        let num_batches = 1 + (rng.next_u64() % 3) as usize;
        let mut seq = 0i64;
        (0..num_batches)
            .map(|_| {
                let rows = (rng.next_u64() % 41) as usize;
                let mut columns: Vec<Column> = kinds
                    .iter()
                    .map(|&k| {
                        let vals: Vec<Value> = (0..rows).map(|_| random_value(rng, k)).collect();
                        Column::from_values(&k.data_type(), &vals).unwrap()
                    })
                    .collect();
                let seq_vals: Vec<Value> = (0..rows)
                    .map(|_| {
                        let v = Value::Int64(seq);
                        seq += 1;
                        v
                    })
                    .collect();
                columns.push(Column::from_values(&DataType::Int64, &seq_vals).unwrap());
                Batch::new(fields.to_vec(), columns).unwrap()
            })
            .collect()
    }

    #[test]
    fn exec_kernel_matches_the_old_flush_order() {
        let mut rng = SplitMix64::new(0x00AD_E11E_0001);
        // 200 random cases take ~8 min under Miri; a handful still covers the path for UB.
        let cases = if cfg!(miri) { 5 } else { 200 };
        for case in 0..cases {
            let num_cols = 1 + (rng.next_u64() % 3) as usize;
            let kinds: Vec<ColKind> = (0..num_cols)
                .map(|_| KINDS[(rng.next_u64() % KINDS.len() as u64) as usize])
                .collect();
            let mut fields: Vec<Field> = kinds
                .iter()
                .enumerate()
                .map(|(i, k)| field(&format!("c{i}"), k.data_type()))
                .collect();
            fields.push(field("seq", DataType::Int64));

            let batches = random_batches(&mut rng, &fields, &kinds);
            let order = random_order(&mut rng, num_cols);

            let actual = sort_rows(&fields, &batches, &order).unwrap();
            let expected = reference_sort_rows(&fields, &batches, &order).unwrap();
            assert_batches_match(&actual, &expected, &fields, case);
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)] // slow under Miri
    fn exec_kernel_matches_chunking_across_batch_rows() {
        let mut rng = SplitMix64::new(0x00AD_E11E_0002);
        let fields = [field("a", DataType::Int64), field("seq", DataType::Int64)];
        let n = 5000usize;
        let a_vals: Vec<Value> = (0..n)
            .map(|_| random_value(&mut rng, ColKind::Int64))
            .collect();
        let seq_vals: Vec<Value> = (0..n as i64).map(Value::Int64).collect();
        let batch = Batch::new(
            fields.to_vec(),
            vec![
                Column::from_values(&DataType::Int64, &a_vals).unwrap(),
                Column::from_values(&DataType::Int64, &seq_vals).unwrap(),
            ],
        )
        .unwrap();
        let order = [0usize];

        let actual = sort_rows(&fields, std::slice::from_ref(&batch), &order).unwrap();
        let expected = reference_sort_rows(&fields, &[batch], &order).unwrap();
        assert_batches_match(&actual, &expected, &fields, 0);
    }
}
