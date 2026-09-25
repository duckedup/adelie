//! Selection kernels (SPEC §7): `take`/`filter` over one column or a whole `Batch`, batch
//! concatenation and `rechunk`. Fixed-width kinds go through typed slices, no `Value`.

use crate::exec::{
    BATCH_ROWS, Batch, Bitmap, Column, ColumnBuilder, ColumnValues, ExecError, Field, OwnedValues,
};
use crate::types::DataType;

/// Lazily allocates validity only once the first invalid row appears, mirroring
/// `ColumnBuilder::push_null`'s backfill.
struct ValidityBuilder {
    bitmap: Option<Bitmap>,
    len: usize,
}

impl ValidityBuilder {
    fn new() -> Self {
        ValidityBuilder {
            bitmap: None,
            len: 0,
        }
    }

    fn push(&mut self, valid: bool) {
        if let Some(bm) = &mut self.bitmap {
            bm.push(valid);
        } else if !valid {
            let mut bm = Bitmap::new_valid(self.len);
            bm.push(false);
            self.bitmap = Some(bm);
        }
        self.len += 1;
    }

    fn finish(self) -> Option<Bitmap> {
        self.bitmap
    }
}

/// Row `indices[i]` of `col` becomes row `i` of the result. Panics if any index is out of
/// range for `col`.
pub(crate) fn take(col: &Column, indices: &[u32]) -> Column {
    build_by_index(col, indices.len(), |i| Some(indices[i] as usize))
}

/// Like `take`, but `None` produces a NULL row instead of reading `col`.
pub(crate) fn take_opt(col: &Column, indices: &[Option<u32>]) -> Column {
    build_by_index(col, indices.len(), |i| indices[i].map(|x| x as usize))
}

/// Keeps row `i` of `col` iff `keep.get(i)`. `keep.len()` must equal `col.len()`.
pub(crate) fn filter(col: &Column, keep: &Bitmap) -> Column {
    assert_eq!(
        keep.len(),
        col.len(),
        "filter: keep bitmap length {} != column length {}",
        keep.len(),
        col.len()
    );
    let indices: Vec<u32> = (0..col.len() as u32).filter(|&i| keep.get(i as usize)).collect();
    take(col, &indices)
}

pub(crate) fn take_batch(b: &Batch, indices: &[u32]) -> Batch {
    let columns: Vec<Column> = b.columns().iter().map(|c| take(c, indices)).collect();
    Batch::new(b.fields().to_vec(), columns)
        .expect("take keeps every column's type and gives them all one row count")
}

pub(crate) fn filter_batch(b: &Batch, keep: &Bitmap) -> Batch {
    let columns: Vec<Column> = b.columns().iter().map(|c| filter(c, keep)).collect();
    Batch::new(b.fields().to_vec(), columns)
        .expect("filter keeps every column's type and gives them all one row count")
}

pub(crate) fn slice_batch(b: &Batch, start: usize, len: usize) -> Batch {
    let columns: Vec<Column> = b.columns().iter().map(|c| c.slice(start, len)).collect();
    Batch::new(b.fields().to_vec(), columns)
        .expect("slice keeps every column's type and gives them all one row count")
}

pub(crate) fn concat_batches(fields: &[Field], batches: &[Batch]) -> Result<Batch, ExecError> {
    let mut columns = Vec::with_capacity(fields.len());
    for (i, field) in fields.iter().enumerate() {
        let parts: Vec<Column> = batches.iter().map(|b| b.column(i).clone()).collect();
        columns.push(Column::concat(&field.ty, &parts)?);
    }
    Ok(Batch::new(fields.to_vec(), columns)?)
}

pub(crate) fn null_column(ty: &DataType, rows: usize) -> Column {
    let mut builder = ColumnBuilder::with_capacity(ty.clone(), rows);
    for _ in 0..rows {
        builder.push_null();
    }
    builder.finish()
}

/// Splits into batches of at most `BATCH_ROWS` rows (no empty batches; empty input -> empty
/// `Vec`): concatenates everything, then slices back into `BATCH_ROWS`-sized pieces.
pub(crate) fn rechunk(fields: &[Field], batches: Vec<Batch>) -> Result<Vec<Batch>, ExecError> {
    let total: usize = batches.iter().map(Batch::rows).sum();
    if total == 0 {
        return Ok(Vec::new());
    }
    let combined = concat_batches(fields, &batches)?;
    let mut out = Vec::with_capacity(total.div_ceil(BATCH_ROWS));
    let mut start = 0;
    while start < total {
        let len = (total - start).min(BATCH_ROWS);
        out.push(slice_batch(&combined, start, len));
        start += len;
    }
    Ok(out)
}

/// Builds a column of `n` rows, row `i` taken from `col` at `index_at(i)` (`None` -> NULL).
/// Fixed-width kinds (including STRING/BYTES) go through typed buffers with no `Value`;
/// LIST falls back to `Column::get` plus `ColumnBuilder`.
fn build_by_index(col: &Column, n: usize, index_at: impl Fn(usize) -> Option<usize>) -> Column {
    if matches!(col.data_type(), DataType::List(_)) {
        return take_list_generic(col, n, index_at);
    }
    let ty = col.data_type().clone();
    let mut validity = ValidityBuilder::new();
    let values = match col.values() {
        ColumnValues::Bool(bm) => {
            let mut out = Bitmap::new_valid(0);
            for i in 0..n {
                match index_at(i) {
                    Some(idx) => {
                        let is_null = col.is_null(idx);
                        validity.push(!is_null);
                        out.push(!is_null && bm.get(idx));
                    }
                    None => {
                        validity.push(false);
                        out.push(false);
                    }
                }
            }
            OwnedValues::Bool(out)
        }
        ColumnValues::Int64(s) => {
            OwnedValues::Int64(scalar_take(s, 0i64, n, col, &index_at, &mut validity))
        }
        ColumnValues::UInt64(s) => {
            OwnedValues::UInt64(scalar_take(s, 0u64, n, col, &index_at, &mut validity))
        }
        ColumnValues::Float64(s) => {
            OwnedValues::Float64(scalar_take(s, 0.0f64, n, col, &index_at, &mut validity))
        }
        ColumnValues::Decimal(s) => {
            OwnedValues::Decimal(scalar_take(s, 0i128, n, col, &index_at, &mut validity))
        }
        ColumnValues::Timestamp(s) => {
            OwnedValues::Timestamp(scalar_take(s, 0i64, n, col, &index_at, &mut validity))
        }
        ColumnValues::Date(s) => {
            OwnedValues::Date(scalar_take(s, 0i32, n, col, &index_at, &mut validity))
        }
        ColumnValues::Uuid(s) => {
            OwnedValues::Uuid(scalar_take(s, [0u8; 16], n, col, &index_at, &mut validity))
        }
        ColumnValues::Ip(s) => {
            OwnedValues::Ip(scalar_take(s, [0u8; 16], n, col, &index_at, &mut validity))
        }
        ColumnValues::String { offsets, data } => {
            let (o, d) = take_varwidth(offsets, data, n, &index_at, &mut validity, col);
            OwnedValues::String { offsets: o, data: d }
        }
        ColumnValues::Bytes { offsets, data } => {
            let (o, d) = take_varwidth(offsets, data, n, &index_at, &mut validity, col);
            OwnedValues::Bytes { offsets: o, data: d }
        }
        ColumnValues::List { .. } => unreachable!("LIST handled above"),
    };
    Column::from_parts(ty, values, validity.finish())
        .expect("take/take_opt preserve the source column's own invariants")
}

/// Fixed-width fast path shared by every scalar `ColumnValues` kind: no `Value` involved.
fn scalar_take<T: Copy>(
    slice: &[T],
    default: T,
    n: usize,
    col: &Column,
    index_at: &impl Fn(usize) -> Option<usize>,
    validity: &mut ValidityBuilder,
) -> Vec<T> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        match index_at(i) {
            Some(idx) => {
                let is_null = col.is_null(idx);
                validity.push(!is_null);
                out.push(if is_null { default } else { slice[idx] });
            }
            None => {
                validity.push(false);
                out.push(default);
            }
        }
    }
    out
}

fn take_varwidth(
    offsets: &[u32],
    data: &[u8],
    n: usize,
    index_at: &impl Fn(usize) -> Option<usize>,
    validity: &mut ValidityBuilder,
    col: &Column,
) -> (Vec<u32>, Vec<u8>) {
    let mut out_offsets = Vec::with_capacity(n + 1);
    out_offsets.push(0u32);
    let mut out_data = Vec::new();
    for i in 0..n {
        match index_at(i) {
            Some(idx) if !col.is_null(idx) => {
                validity.push(true);
                let start = offsets[idx] as usize;
                let end = offsets[idx + 1] as usize;
                out_data.extend_from_slice(&data[start..end]);
            }
            Some(idx) => {
                let _ = col.is_null(idx); // bounds-checks idx
                validity.push(false);
            }
            None => validity.push(false),
        }
        out_offsets.push(out_data.len() as u32);
    }
    (out_offsets, out_data)
}

fn take_list_generic(col: &Column, n: usize, index_at: impl Fn(usize) -> Option<usize>) -> Column {
    let mut builder = ColumnBuilder::with_capacity(col.data_type().clone(), n);
    for i in 0..n {
        match index_at(i) {
            Some(idx) => {
                let v = col.get(idx);
                builder
                    .push(&v)
                    .expect("a value read from a column always fits that same column's type");
            }
            None => builder.push_null(),
        }
    }
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Decimal, Ip, Value};
    use std::net::IpAddr;

    fn check_take_filter_concat(ty: &DataType, values: &[Value]) {
        let col = Column::from_values(ty, values).unwrap();
        let n = col.len();

        let rev: Vec<u32> = (0..n as u32).rev().collect();
        let taken = take(&col, &rev);
        for i in 0..n {
            assert_eq!(taken.get(i), col.get(n - 1 - i), "take row {i} of {ty}");
        }

        let opt: Vec<Option<u32>> = (0..n)
            .map(|i| if i % 2 == 0 { Some(i as u32) } else { None })
            .collect();
        let taken_opt = take_opt(&col, &opt);
        for i in 0..n {
            if i % 2 == 0 {
                assert_eq!(taken_opt.get(i), col.get(i), "take_opt row {i} of {ty}");
            } else {
                assert!(taken_opt.is_null(i), "take_opt None row {i} of {ty}");
            }
        }

        let mut keep = Bitmap::new_valid(0);
        for i in 0..n {
            keep.push(i % 2 == 0);
        }
        let filtered = filter(&col, &keep);
        let mut j = 0;
        for i in 0..n {
            if i % 2 == 0 {
                assert_eq!(filtered.get(j), col.get(i), "filter row {j} of {ty}");
                j += 1;
            }
        }
        assert_eq!(filtered.len(), j);

        let field = Field {
            name: "c".to_string(),
            ty: ty.clone(),
        };
        let half = n / 2;
        let b1 = Batch::new(vec![field.clone()], vec![col.slice(0, half)]).unwrap();
        let b2 = Batch::new(vec![field.clone()], vec![col.slice(half, n - half)]).unwrap();
        let combined = concat_batches(&[field], &[b1, b2]).unwrap();
        for i in 0..n {
            assert_eq!(combined.column(0).get(i), col.get(i), "concat row {i} of {ty}");
        }
    }

    #[test]
    fn every_kind_without_validity() {
        check_take_filter_concat(
            &DataType::Bool,
            &[
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(true),
                Value::Bool(false),
            ],
        );
        check_take_filter_concat(&DataType::Int64, &(0..6).map(Value::Int64).collect::<Vec<_>>());
        check_take_filter_concat(&DataType::UInt64, &(0..6).map(Value::UInt64).collect::<Vec<_>>());
        check_take_filter_concat(
            &DataType::Float64,
            &(0..6).map(|i| Value::Float64(i as f64)).collect::<Vec<_>>(),
        );
        check_take_filter_concat(
            &DataType::decimal(5, 2).unwrap(),
            &(0..6)
                .map(|i| Value::Decimal(Decimal::new(i, 2).unwrap()))
                .collect::<Vec<_>>(),
        );
        check_take_filter_concat(
            &DataType::String,
            &["a", "bb", "ccc", "d", "ee", "f"]
                .iter()
                .map(|s| Value::String(s.to_string()))
                .collect::<Vec<_>>(),
        );
        check_take_filter_concat(
            &DataType::Bytes,
            &(0..6).map(|i| Value::Bytes(vec![i as u8])).collect::<Vec<_>>(),
        );
        check_take_filter_concat(
            &DataType::Timestamp,
            &(0..6).map(Value::Timestamp).collect::<Vec<_>>(),
        );
        check_take_filter_concat(&DataType::Date, &(0..6).map(Value::Date).collect::<Vec<_>>());
        check_take_filter_concat(
            &DataType::Uuid,
            &(0..6).map(|i| Value::Uuid([i as u8; 16])).collect::<Vec<_>>(),
        );
        let ip = Ip::from(IpAddr::from([127, 0, 0, 1]));
        check_take_filter_concat(&DataType::Ip, &(0..6).map(|_| Value::Ip(ip)).collect::<Vec<_>>());
        let list_ty = DataType::list(DataType::Int64).unwrap();
        check_take_filter_concat(
            &list_ty,
            &(0..6)
                .map(|i| Value::List(vec![Value::Int64(i)]))
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn every_kind_with_validity() {
        check_take_filter_concat(
            &DataType::Int64,
            &[
                Value::Int64(1),
                Value::Null,
                Value::Int64(3),
                Value::Null,
                Value::Int64(5),
                Value::Int64(6),
            ],
        );
        check_take_filter_concat(
            &DataType::String,
            &[
                Value::String("a".into()),
                Value::Null,
                Value::String("ccc".into()),
                Value::Null,
                Value::String("ee".into()),
                Value::String("f".into()),
            ],
        );
        let list_ty = DataType::list(DataType::Int64).unwrap();
        check_take_filter_concat(
            &list_ty,
            &[
                Value::List(vec![Value::Int64(1)]),
                Value::Null,
                Value::List(vec![]),
                Value::Null,
                Value::List(vec![Value::Int64(2)]),
                Value::List(vec![Value::Int64(3)]),
            ],
        );
    }

    #[test]
    fn rechunk_splits_over_and_concatenates_under_batch_rows() {
        let field = Field {
            name: "x".to_string(),
            ty: DataType::Int64,
        };
        let make_batch = |start: i64, n: i64| {
            let values: Vec<Value> = (start..start + n).map(Value::Int64).collect();
            Batch::new(
                vec![field.clone()],
                vec![Column::from_values(&DataType::Int64, &values).unwrap()],
            )
            .unwrap()
        };
        let batches = vec![make_batch(0, 3000), make_batch(3000, 3000), make_batch(6000, 3000)];
        let out = rechunk(&[field], batches).unwrap();
        let total: usize = out.iter().map(Batch::rows).sum();
        assert_eq!(total, 9000);
        for b in &out {
            assert!(b.rows() <= BATCH_ROWS);
            assert!(b.rows() > 0);
        }
        let mut expected = 0i64;
        for b in &out {
            for i in 0..b.rows() {
                assert_eq!(b.column(0).get(i), Value::Int64(expected));
                expected += 1;
            }
        }
    }

    #[test]
    fn rechunk_of_empty_input_is_empty() {
        let field = Field {
            name: "x".to_string(),
            ty: DataType::Int64,
        };
        let out = rechunk(&[field], Vec::new()).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    #[should_panic]
    fn take_out_of_bounds_panics() {
        let col = Column::from_values(&DataType::Int64, &[Value::Int64(1)]).unwrap();
        take(&col, &[5]);
    }
}
