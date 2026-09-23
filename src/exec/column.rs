//! `Column`: one typed, validity-tracked buffer of `BATCH_ROWS`-ish values, plus the
//! `ColumnBuilder` that appends `Value`s to one through `types::coerce`.

use std::cmp::Ordering;
use std::mem::size_of;
use std::net::{IpAddr, Ipv6Addr};

use crate::types::{DataType, Decimal, Ip, Value, coerce, total_cmp};

use super::bitmap::Bitmap;
use super::stats::ColumnStats;

/// A typed column of `len()` rows. `validity` is `None` when every row is valid; a set bit
/// means valid (contract rule 7). `null_count` is cached, not recomputed from `validity`.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    ty: DataType,
    data: ColumnData,
    validity: Option<Bitmap>,
    null_count: usize,
}

/// One physical layout per `DataType` kind. Offsets are `u32` and `offsets.len() ==
/// rows + 1`; a null row repeats the previous offset rather than storing an empty range.
#[derive(Debug, Clone, PartialEq)]
enum ColumnData {
    Bool(Bitmap),
    Int64(Vec<i64>),
    UInt64(Vec<u64>),
    Float64(Vec<f64>),
    /// Unscaled `i128` at the column's own `DecimalType::scale()`.
    Decimal(Vec<i128>),
    Timestamp(Vec<i64>),
    Date(Vec<i32>),
    Uuid(Vec<[u8; 16]>),
    Ip(Vec<[u8; 16]>),
    String {
        offsets: Vec<u32>,
        data: Vec<u8>,
    },
    Bytes {
        offsets: Vec<u32>,
        data: Vec<u8>,
    },
    List {
        offsets: Vec<u32>,
        values: Box<Column>,
    },
}

impl Column {
    /// Builds a column from `values` via `ColumnBuilder`, one push per value.
    pub fn from_values(ty: &DataType, values: &[Value]) -> Result<Column, ColumnError> {
        let mut builder = ColumnBuilder::with_capacity(ty.clone(), values.len());
        for v in values {
            builder.push(v)?;
        }
        Ok(builder.finish())
    }

    pub fn data_type(&self) -> &DataType {
        &self.ty
    }

    pub fn len(&self) -> usize {
        match &self.data {
            ColumnData::Bool(bm) => bm.len(),
            ColumnData::Int64(v) => v.len(),
            ColumnData::UInt64(v) => v.len(),
            ColumnData::Float64(v) => v.len(),
            ColumnData::Decimal(v) => v.len(),
            ColumnData::Timestamp(v) => v.len(),
            ColumnData::Date(v) => v.len(),
            ColumnData::Uuid(v) => v.len(),
            ColumnData::Ip(v) => v.len(),
            ColumnData::String { offsets, .. } | ColumnData::Bytes { offsets, .. } => {
                offsets.len() - 1
            }
            ColumnData::List { offsets, .. } => offsets.len() - 1,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn null_count(&self) -> usize {
        self.null_count
    }

    pub fn is_null(&self, i: usize) -> bool {
        assert!(
            i < self.len(),
            "column index {i} out of bounds for len {}",
            self.len()
        );
        self.validity.as_ref().is_some_and(|b| !b.get(i))
    }

    /// `Value::Null` for a null row. Panics if `i` is out of bounds.
    pub fn get(&self, i: usize) -> Value {
        if self.is_null(i) {
            return Value::Null;
        }
        match &self.data {
            ColumnData::Bool(bm) => Value::Bool(bm.get(i)),
            ColumnData::Int64(v) => Value::Int64(v[i]),
            ColumnData::UInt64(v) => Value::UInt64(v[i]),
            ColumnData::Float64(v) => Value::Float64(v[i]),
            ColumnData::Decimal(v) => Value::Decimal(
                Decimal::new(v[i], decimal_scale(&self.ty)).expect("column decimal in range"),
            ),
            ColumnData::Timestamp(v) => Value::Timestamp(v[i]),
            ColumnData::Date(v) => Value::Date(v[i]),
            ColumnData::Uuid(v) => Value::Uuid(v[i]),
            ColumnData::Ip(v) => Value::Ip(ip_from_octets(v[i])),
            ColumnData::String { offsets, data } => {
                let bytes = &data[offsets[i] as usize..offsets[i + 1] as usize];
                let s = std::str::from_utf8(bytes).expect("column string data is valid utf-8");
                Value::String(s.to_string())
            }
            ColumnData::Bytes { offsets, data } => {
                Value::Bytes(data[offsets[i] as usize..offsets[i + 1] as usize].to_vec())
            }
            ColumnData::List { offsets, values } => {
                let start = offsets[i] as usize;
                let end = offsets[i + 1] as usize;
                Value::List((start..end).map(|j| values.get(j)).collect())
            }
        }
    }

    /// Capacity of every buffer, including validity and (for LIST) the nested child column.
    pub fn byte_size(&self) -> usize {
        let mut n = self.validity.as_ref().map_or(0, Bitmap::byte_size);
        n += match &self.data {
            ColumnData::Bool(bm) => bm.byte_size(),
            ColumnData::Int64(v) => v.capacity() * size_of::<i64>(),
            ColumnData::UInt64(v) => v.capacity() * size_of::<u64>(),
            ColumnData::Float64(v) => v.capacity() * size_of::<f64>(),
            ColumnData::Decimal(v) => v.capacity() * size_of::<i128>(),
            ColumnData::Timestamp(v) => v.capacity() * size_of::<i64>(),
            ColumnData::Date(v) => v.capacity() * size_of::<i32>(),
            ColumnData::Uuid(v) => v.capacity() * size_of::<[u8; 16]>(),
            ColumnData::Ip(v) => v.capacity() * size_of::<[u8; 16]>(),
            ColumnData::String { offsets, data } => {
                offsets.capacity() * size_of::<u32>() + data.capacity()
            }
            ColumnData::Bytes { offsets, data } => {
                offsets.capacity() * size_of::<u32>() + data.capacity()
            }
            ColumnData::List { offsets, values } => {
                offsets.capacity() * size_of::<u32>() + values.byte_size()
            }
        };
        n
    }

    /// Per-column `{rows, null_count, min, max}` for E3's segment footer (SPEC §5).
    pub fn stats(&self) -> ColumnStats {
        let (min, max) = if matches!(self.ty, DataType::List(_)) {
            (None, None)
        } else {
            self.scan_min_max()
        };
        ColumnStats {
            rows: self.len(),
            null_count: self.null_count,
            min,
            max,
        }
    }

    /// The chunked fast path SPEC §7 asks for: fold the whole slice with no per-row null
    /// test when `validity` is `None`, else skip each all-zero validity word outright.
    fn scan_min_max(&self) -> (Option<Value>, Option<Value>) {
        let len = self.len();
        let value_at = |i: usize| self.get_non_null(i);
        let mut min: Option<Value> = None;
        let mut max: Option<Value> = None;
        let mut consider = |v: Value| {
            if min
                .as_ref()
                .is_none_or(|m| total_cmp(&v, m) == Some(Ordering::Less))
            {
                min = Some(v.clone());
            }
            if max
                .as_ref()
                .is_none_or(|m| total_cmp(&v, m) == Some(Ordering::Greater))
            {
                max = Some(v);
            }
        };
        match &self.validity {
            None => {
                for i in 0..len {
                    consider(value_at(i));
                }
            }
            Some(bm) => {
                for (word_idx, &word) in bm.words().iter().enumerate() {
                    if word == 0 {
                        continue;
                    }
                    let base = word_idx * 64;
                    for bit in 0..64 {
                        let i = base + bit;
                        if i >= len {
                            break;
                        }
                        if (word >> bit) & 1 == 1 {
                            consider(value_at(i));
                        }
                    }
                }
            }
        }
        (min, max)
    }

    /// `get(i)` for a row already known non-null (the stats scan never visits a null row).
    fn get_non_null(&self, i: usize) -> Value {
        match &self.data {
            ColumnData::Bool(bm) => Value::Bool(bm.get(i)),
            ColumnData::Int64(v) => Value::Int64(v[i]),
            ColumnData::UInt64(v) => Value::UInt64(v[i]),
            ColumnData::Float64(v) => Value::Float64(v[i]),
            ColumnData::Decimal(v) => Value::Decimal(
                Decimal::new(v[i], decimal_scale(&self.ty)).expect("column decimal in range"),
            ),
            ColumnData::Timestamp(v) => Value::Timestamp(v[i]),
            ColumnData::Date(v) => Value::Date(v[i]),
            ColumnData::Uuid(v) => Value::Uuid(v[i]),
            ColumnData::Ip(v) => Value::Ip(ip_from_octets(v[i])),
            ColumnData::String { .. } | ColumnData::Bytes { .. } => self.get(i),
            ColumnData::List { .. } => unreachable!("stats() skips LIST before scanning"),
        }
    }
}

fn decimal_scale(ty: &DataType) -> u8 {
    match ty {
        DataType::Decimal(dt) => dt.scale(),
        _ => unreachable!("decimal_scale called on a non-DECIMAL column"),
    }
}

fn ip_from_octets(bytes: [u8; 16]) -> Ip {
    Ip::from(IpAddr::V6(Ipv6Addr::from(bytes)))
}

/// Appends `Value`s to a `Column` under construction, one physical layout per `DataType`
/// kind, mirroring `ColumnData`.
pub struct ColumnBuilder {
    ty: DataType,
    data: BuilderData,
    validity: Option<Bitmap>,
    null_count: usize,
    len: usize,
}

enum BuilderData {
    Bool(Bitmap),
    Int64(Vec<i64>),
    UInt64(Vec<u64>),
    Float64(Vec<f64>),
    Decimal(Vec<i128>),
    Timestamp(Vec<i64>),
    Date(Vec<i32>),
    Uuid(Vec<[u8; 16]>),
    Ip(Vec<[u8; 16]>),
    String {
        offsets: Vec<u32>,
        data: Vec<u8>,
    },
    Bytes {
        offsets: Vec<u32>,
        data: Vec<u8>,
    },
    List {
        offsets: Vec<u32>,
        values: Box<ColumnBuilder>,
    },
}

impl ColumnBuilder {
    pub fn new(ty: DataType) -> Self {
        Self::with_capacity(ty, 0)
    }

    pub fn with_capacity(ty: DataType, rows: usize) -> Self {
        let data = match &ty {
            DataType::Bool => BuilderData::Bool(Bitmap::new_valid(0)),
            DataType::Int64 => BuilderData::Int64(Vec::with_capacity(rows)),
            DataType::UInt64 => BuilderData::UInt64(Vec::with_capacity(rows)),
            DataType::Float64 => BuilderData::Float64(Vec::with_capacity(rows)),
            DataType::Decimal(_) => BuilderData::Decimal(Vec::with_capacity(rows)),
            DataType::Timestamp => BuilderData::Timestamp(Vec::with_capacity(rows)),
            DataType::Date => BuilderData::Date(Vec::with_capacity(rows)),
            DataType::Uuid => BuilderData::Uuid(Vec::with_capacity(rows)),
            DataType::Ip => BuilderData::Ip(Vec::with_capacity(rows)),
            DataType::String => BuilderData::String {
                offsets: vec![0],
                data: Vec::new(),
            },
            DataType::Bytes => BuilderData::Bytes {
                offsets: vec![0],
                data: Vec::new(),
            },
            DataType::List(lt) => BuilderData::List {
                offsets: vec![0],
                values: Box::new(ColumnBuilder::new(lt.element().clone())),
            },
        };
        ColumnBuilder {
            ty,
            data,
            validity: None,
            null_count: 0,
            len: 0,
        }
    }

    /// Coerces `v` to the builder's type via `types::coerce`; `DoesNotFit` leaves the
    /// builder unchanged, which is the signal a writer routes to `companion_name(col)`.
    pub fn push(&mut self, v: &Value) -> Result<(), ColumnError> {
        if v.is_null() {
            self.push_null();
            return Ok(());
        }
        let coerced = coerce(v, &self.ty).ok_or_else(|| ColumnError::DoesNotFit {
            value: v.clone(),
            ty: self.ty.clone(),
        })?;
        self.push_coerced(&coerced)
    }

    /// A null row: a zero/default in fixed-width buffers, the previous offset repeated in
    /// variable-width ones. Backfills `validity` to all-valid on the first null (lazy).
    pub fn push_null(&mut self) {
        self.push_default();
        if self.validity.is_none() {
            self.validity = Some(Bitmap::new_valid(self.len));
        }
        self.validity.as_mut().expect("just set").push(false);
        self.len += 1;
        self.null_count += 1;
    }

    pub fn finish(self) -> Column {
        let data = match self.data {
            BuilderData::Bool(bm) => ColumnData::Bool(bm),
            BuilderData::Int64(v) => ColumnData::Int64(v),
            BuilderData::UInt64(v) => ColumnData::UInt64(v),
            BuilderData::Float64(v) => ColumnData::Float64(v),
            BuilderData::Decimal(v) => ColumnData::Decimal(v),
            BuilderData::Timestamp(v) => ColumnData::Timestamp(v),
            BuilderData::Date(v) => ColumnData::Date(v),
            BuilderData::Uuid(v) => ColumnData::Uuid(v),
            BuilderData::Ip(v) => ColumnData::Ip(v),
            BuilderData::String { offsets, data } => ColumnData::String { offsets, data },
            BuilderData::Bytes { offsets, data } => ColumnData::Bytes { offsets, data },
            BuilderData::List { offsets, values } => ColumnData::List {
                offsets,
                values: Box::new(values.finish()),
            },
        };
        Column {
            ty: self.ty,
            data,
            validity: self.validity,
            null_count: self.null_count,
        }
    }

    /// A zero/default physical value for a null row; `push_coerced`'s non-null counterpart.
    fn push_default(&mut self) {
        match &mut self.data {
            BuilderData::Bool(bm) => bm.push(false),
            BuilderData::Int64(v) => v.push(0),
            BuilderData::UInt64(v) => v.push(0),
            BuilderData::Float64(v) => v.push(0.0),
            BuilderData::Decimal(v) => v.push(0),
            BuilderData::Timestamp(v) => v.push(0),
            BuilderData::Date(v) => v.push(0),
            BuilderData::Uuid(v) => v.push([0; 16]),
            BuilderData::Ip(v) => v.push([0; 16]),
            BuilderData::String { offsets, .. }
            | BuilderData::Bytes { offsets, .. }
            | BuilderData::List { offsets, .. } => {
                offsets.push(
                    *offsets
                        .last()
                        .expect("offsets always starts with one entry"),
                );
            }
        }
    }

    /// Appends a value already coerced to `self.ty`; a LIST recurses into the child
    /// builder, so its own coerced elements never run through `coerce` a second time.
    fn push_coerced(&mut self, v: &Value) -> Result<(), ColumnError> {
        match (&mut self.data, v) {
            (BuilderData::Bool(bm), Value::Bool(b)) => bm.push(*b),
            (BuilderData::Int64(buf), Value::Int64(n)) => buf.push(*n),
            (BuilderData::UInt64(buf), Value::UInt64(n)) => buf.push(*n),
            (BuilderData::Float64(buf), Value::Float64(f)) => buf.push(*f),
            (BuilderData::Decimal(buf), Value::Decimal(d)) => buf.push(d.unscaled()),
            (BuilderData::Timestamp(buf), Value::Timestamp(ns)) => buf.push(*ns),
            (BuilderData::Date(buf), Value::Date(d)) => buf.push(*d),
            (BuilderData::Uuid(buf), Value::Uuid(bytes)) => buf.push(*bytes),
            (BuilderData::Ip(buf), Value::Ip(ip)) => buf.push(ip.octets()),
            (BuilderData::String { offsets, data }, Value::String(s)) => {
                offsets.push(checked_offset(data.len() + s.len())?);
                data.extend_from_slice(s.as_bytes());
            }
            (BuilderData::Bytes { offsets, data }, Value::Bytes(b)) => {
                offsets.push(checked_offset(data.len() + b.len())?);
                data.extend_from_slice(b);
            }
            (BuilderData::List { offsets, values }, Value::List(items)) => {
                // Checked before any element is pushed, so an over-long list leaves the
                // builder untouched. A child STRING overflowing mid-list does not.
                checked_offset(values.len + items.len())?;
                for item in items {
                    if item.is_null() {
                        values.push_null();
                    } else {
                        values.push_coerced(item)?;
                    }
                }
                push_offset(offsets, values.len)?
            }
            _ => unreachable!("push_coerced called with a value not coerced to self.ty"),
        }
        if let Some(validity) = &mut self.validity {
            validity.push(true);
        }
        self.len += 1;
        Ok(())
    }
}

/// `total` as a `u32` offset, or `TooLarge` (never truncated) past `u32::MAX` bytes/elements.
fn checked_offset(total: usize) -> Result<u32, ColumnError> {
    u32::try_from(total).map_err(|_| ColumnError::TooLarge)
}

fn push_offset(offsets: &mut Vec<u32>, total: usize) -> Result<(), ColumnError> {
    offsets.push(checked_offset(total)?);
    Ok(())
}

/// A rejected `Column`/`ColumnBuilder` operation.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnError {
    DoesNotFit {
        value: Value,
        ty: DataType,
    },
    /// A STRING/BYTES buffer or a LIST child column grew past `u32::MAX`.
    TooLarge,
    LengthMismatch {
        left: usize,
        right: usize,
    },
    NotString(DataType),
}

impl std::fmt::Display for ColumnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ColumnError::DoesNotFit { value, ty } => {
                write!(f, "value {value:?} does not losslessly fit {ty}")
            }
            ColumnError::TooLarge => write!(f, "column buffer exceeds u32::MAX"),
            ColumnError::LengthMismatch { left, right } => {
                write!(f, "column length mismatch: {left} vs {right}")
            }
            ColumnError::NotString(ty) => write!(f, "companion column must be STRING, found {ty}"),
        }
    }
}

impl std::error::Error for ColumnError {}

#[cfg(test)]
mod tests {
    use super::*;
    use adelie_harness::rng::SplitMix64;

    fn round_trip(ty: &DataType, values: &[Value]) {
        let col = Column::from_values(ty, values).unwrap();
        assert_eq!(col.len(), values.len());
        for (i, v) in values.iter().enumerate() {
            assert_eq!(&col.get(i), v, "row {i} of {ty}");
        }
    }

    #[test]
    fn round_trip_every_scalar_kind() {
        round_trip(
            &DataType::Bool,
            &[Value::Bool(true), Value::Null, Value::Bool(false)],
        );
        round_trip(
            &DataType::Int64,
            &[Value::Int64(1), Value::Null, Value::Int64(-2)],
        );
        round_trip(
            &DataType::UInt64,
            &[Value::UInt64(1), Value::Null, Value::UInt64(2)],
        );
        round_trip(
            &DataType::Float64,
            &[Value::Float64(1.5), Value::Null, Value::Float64(-2.0)],
        );
        round_trip(
            &DataType::decimal(10, 2).unwrap(),
            &[
                Value::Decimal(Decimal::new(150, 2).unwrap()),
                Value::Null,
                Value::Decimal(Decimal::new(-5, 2).unwrap()),
            ],
        );
        round_trip(
            &DataType::String,
            &[
                Value::String("a".into()),
                Value::Null,
                Value::String("bb".into()),
            ],
        );
        round_trip(
            &DataType::Bytes,
            &[Value::Bytes(vec![1, 2]), Value::Null, Value::Bytes(vec![])],
        );
        round_trip(
            &DataType::Timestamp,
            &[Value::Timestamp(1), Value::Null, Value::Timestamp(-1)],
        );
        round_trip(
            &DataType::Date,
            &[Value::Date(1), Value::Null, Value::Date(-1)],
        );
        round_trip(
            &DataType::Uuid,
            &[Value::Uuid([1; 16]), Value::Null, Value::Uuid([2; 16])],
        );
        let ip = Ip::from(std::net::IpAddr::from([127, 0, 0, 1]));
        round_trip(&DataType::Ip, &[Value::Ip(ip), Value::Null, Value::Ip(ip)]);
    }

    #[test]
    fn round_trip_list_of_string() {
        let ty = DataType::list(DataType::String).unwrap();
        round_trip(
            &ty,
            &[
                Value::List(vec![Value::String("a".into()), Value::Null]),
                Value::Null,
                Value::List(vec![]),
            ],
        );
    }

    #[test]
    fn random_round_trip_int64_string_and_list() {
        let mut rng = SplitMix64::new(7);
        let rows = super::super::BATCH_ROWS.min(600);

        let mut int_vals = Vec::with_capacity(rows);
        for _ in 0..rows {
            int_vals.push(if rng.f64() < 0.2 {
                Value::Null
            } else {
                Value::Int64(rng.range(0, 1_000_000) as i64 - 500_000)
            });
        }
        round_trip(&DataType::Int64, &int_vals);

        let mut str_vals = Vec::with_capacity(rows);
        for _ in 0..rows {
            str_vals.push(if rng.f64() < 0.2 {
                Value::Null
            } else {
                let len = rng.range(0, 8) as usize;
                Value::String(
                    (0..len)
                        .map(|_| (b'a' + (rng.range(0, 26) as u8)) as char)
                        .collect(),
                )
            });
        }
        round_trip(&DataType::String, &str_vals);

        let list_ty = DataType::list(DataType::Int64).unwrap();
        let mut list_vals = Vec::with_capacity(rows);
        for _ in 0..rows {
            list_vals.push(if rng.f64() < 0.2 {
                Value::Null
            } else {
                let len = rng.range(0, 4) as usize;
                Value::List(
                    (0..len)
                        .map(|_| Value::Int64(rng.range(0, 100) as i64))
                        .collect(),
                )
            });
        }
        round_trip(&list_ty, &list_vals);
    }

    #[test]
    fn null_count_all_valid_has_no_validity_bitmap() {
        let col =
            Column::from_values(&DataType::Int64, &[Value::Int64(1), Value::Int64(2)]).unwrap();
        assert_eq!(col.null_count(), 0);
        assert!(col.validity.is_none());
        let ColumnData::Int64(buf) = &col.data else {
            unreachable!()
        };
        assert_eq!(col.byte_size(), buf.capacity() * size_of::<i64>());
    }

    #[test]
    fn null_count_all_null() {
        let col = Column::from_values(&DataType::Int64, &[Value::Null, Value::Null]).unwrap();
        assert_eq!(col.null_count(), 2);
    }

    #[test]
    fn null_count_mixed() {
        let col = Column::from_values(
            &DataType::Int64,
            &[Value::Int64(1), Value::Null, Value::Int64(2)],
        )
        .unwrap();
        assert_eq!(col.null_count(), 1);
    }

    #[test]
    fn empty_column_has_zero_len_and_empty_stats() {
        let col = Column::from_values(&DataType::Int64, &[]).unwrap();
        assert_eq!(col.len(), 0);
        let stats = col.stats();
        assert_eq!(stats.rows, 0);
        assert_eq!(stats.null_count, 0);
        assert_eq!(stats.min, None);
        assert_eq!(stats.max, None);
    }

    #[test]
    fn push_of_unfit_value_leaves_column_unchanged() {
        let mut b = ColumnBuilder::new(DataType::UInt64);
        let err = b.push(&Value::Int64(-1)).unwrap_err();
        assert_eq!(
            err,
            ColumnError::DoesNotFit {
                value: Value::Int64(-1),
                ty: DataType::UInt64
            }
        );
        assert_eq!(b.len, 0);
    }

    #[test]
    fn push_coerces_int_into_float() {
        let mut b = ColumnBuilder::new(DataType::Float64);
        b.push(&Value::Int64(3)).unwrap();
        let col = b.finish();
        assert_eq!(col.get(0), Value::Float64(3.0));
    }

    #[test]
    fn push_rescales_decimal_to_column_scale() {
        let ty = DataType::decimal(5, 2).unwrap();
        let mut b = ColumnBuilder::new(ty);
        b.push(&Value::Decimal(Decimal::new(15, 1).unwrap()))
            .unwrap();
        let col = b.finish();
        assert_eq!(col.get(0), Value::Decimal(Decimal::new(150, 2).unwrap()));
    }

    fn int_stats(values: &[Value]) -> ColumnStats {
        Column::from_values(&DataType::Int64, values)
            .unwrap()
            .stats()
    }

    #[test]
    fn stats_int64() {
        let s = int_stats(&[
            Value::Int64(3),
            Value::Null,
            Value::Int64(-7),
            Value::Int64(12),
        ]);
        assert_eq!(s.rows, 4);
        assert_eq!(s.null_count, 1);
        assert_eq!(s.min, Some(Value::Int64(-7)));
        assert_eq!(s.max, Some(Value::Int64(12)));
    }

    #[test]
    fn stats_float64_nan_is_max() {
        let col = Column::from_values(
            &DataType::Float64,
            &[
                Value::Float64(1.0),
                Value::Float64(f64::NAN),
                Value::Float64(-2.0),
            ],
        )
        .unwrap();
        let s = col.stats();
        assert_eq!(s.min, Some(Value::Float64(-2.0)));
        match s.max {
            Some(Value::Float64(f)) => assert!(f.is_nan()),
            other => panic!("expected NaN max, got {other:?}"),
        }
    }

    #[test]
    fn stats_string_lexicographic() {
        let col = Column::from_values(
            &DataType::String,
            &[
                Value::String("b".into()),
                Value::String("a".into()),
                Value::String("ab".into()),
            ],
        )
        .unwrap();
        let s = col.stats();
        assert_eq!(s.min, Some(Value::String("a".into())));
        assert_eq!(s.max, Some(Value::String("b".into())));
    }

    #[test]
    fn stats_all_null_has_no_min_or_max() {
        let s = int_stats(&[Value::Null, Value::Null]);
        assert_eq!(s.min, None);
        assert_eq!(s.max, None);
    }

    #[test]
    fn stats_null_at_word_boundary_row_63() {
        let mut values: Vec<Value> = (0..70).map(Value::Int64).collect();
        values[63] = Value::Null;
        let s = int_stats(&values);
        assert_eq!(s.null_count, 1);
        assert_eq!(s.max, Some(Value::Int64(69)));
    }

    #[test]
    fn stats_null_at_word_boundary_row_64() {
        let mut values: Vec<Value> = (0..70).map(Value::Int64).collect();
        values[64] = Value::Null;
        let s = int_stats(&values);
        assert_eq!(s.null_count, 1);
        assert_eq!(s.max, Some(Value::Int64(69)));
    }

    #[test]
    fn stats_without_nulls_at_word_boundary() {
        let values: Vec<Value> = (0..70).map(Value::Int64).collect();
        let s = int_stats(&values);
        assert_eq!(s.null_count, 0);
        assert_eq!(s.min, Some(Value::Int64(0)));
        assert_eq!(s.max, Some(Value::Int64(69)));
    }

    #[test]
    fn list_stats_report_null_count_only() {
        let ty = DataType::list(DataType::Int64).unwrap();
        let col =
            Column::from_values(&ty, &[Value::List(vec![Value::Int64(1)]), Value::Null]).unwrap();
        let s = col.stats();
        assert_eq!(s.null_count, 1);
        assert_eq!(s.min, None);
        assert_eq!(s.max, None);
    }

    #[test]
    fn byte_size_grows_with_more_rows() {
        let small = Column::from_values(&DataType::Int64, &[Value::Int64(1)]).unwrap();
        let big: Vec<Value> = (0..1000).map(Value::Int64).collect();
        let big = Column::from_values(&DataType::Int64, &big).unwrap();
        assert!(big.byte_size() > small.byte_size());
    }
}
