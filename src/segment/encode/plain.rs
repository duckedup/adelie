//! PLAIN (encoding id 0, SPEC §5, D0008): fixed-width LE per row, STRING/BYTES as offsets
//! plus data, LIST as offsets plus a recursively-encoded child chunk. Applies to every type.

use crate::exec::{Bitmap, Column, ColumnValues, OwnedValues};
use crate::types::DataType;

use crate::segment::error::DecodeError;
use crate::segment::wire::{Cursor, Sink};

pub(super) fn encode(col: &Column, out: &mut Sink) {
    match col.values() {
        ColumnValues::Bool(bm) => {
            for &w in bm.words() {
                out.u64(w);
            }
        }
        ColumnValues::Int64(v) | ColumnValues::Timestamp(v) => {
            for &x in v {
                out.i64(x);
            }
        }
        ColumnValues::UInt64(v) => {
            for &x in v {
                out.u64(x);
            }
        }
        ColumnValues::Float64(v) => {
            for &x in v {
                out.u64(x.to_bits());
            }
        }
        ColumnValues::Decimal(v) => {
            for &x in v {
                out.i128(x);
            }
        }
        ColumnValues::Date(v) => {
            for &x in v {
                out.i32(x);
            }
        }
        ColumnValues::Uuid(v) | ColumnValues::Ip(v) => {
            for x in v {
                out.raw(x);
            }
        }
        ColumnValues::String { offsets, data } | ColumnValues::Bytes { offsets, data } => {
            for &o in offsets {
                out.u32(o);
            }
            out.raw(data);
        }
        ColumnValues::List { offsets, values } => {
            for &o in offsets {
                out.u32(o);
            }
            let (enc, chunk) = super::encode_column(values, None);
            out.uvarint(enc.id());
            out.bytes(&chunk);
        }
    }
}

pub(super) fn decode(
    ty: &DataType,
    rows: usize,
    cur: &mut Cursor,
) -> Result<OwnedValues, DecodeError> {
    match ty {
        DataType::Bool => {
            let n = cur.guard_len(rows.div_ceil(64) as u64, 8)?;
            let mut words = Vec::with_capacity(n);
            for _ in 0..n {
                words.push(cur.u64()?);
            }
            let bm = Bitmap::from_words(words, rows)
                .ok_or(DecodeError::Malformed("bad bool value bitmap"))?;
            Ok(OwnedValues::Bool(bm))
        }
        DataType::Int64 => Ok(OwnedValues::Int64(read_i64s(cur, rows)?)),
        DataType::Timestamp => Ok(OwnedValues::Timestamp(read_i64s(cur, rows)?)),
        DataType::UInt64 => {
            let n = cur.guard_len(rows as u64, 8)?;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(cur.u64()?);
            }
            Ok(OwnedValues::UInt64(v))
        }
        DataType::Float64 => {
            let n = cur.guard_len(rows as u64, 8)?;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(f64::from_bits(cur.u64()?));
            }
            Ok(OwnedValues::Float64(v))
        }
        DataType::Decimal(_) => {
            let n = cur.guard_len(rows as u64, 16)?;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(cur.i128()?);
            }
            Ok(OwnedValues::Decimal(v))
        }
        DataType::Date => {
            let n = cur.guard_len(rows as u64, 4)?;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(cur.i32()?);
            }
            Ok(OwnedValues::Date(v))
        }
        DataType::Uuid => Ok(OwnedValues::Uuid(read_arrays(cur, rows)?)),
        DataType::Ip => Ok(OwnedValues::Ip(read_arrays(cur, rows)?)),
        DataType::String | DataType::Bytes => {
            let (offsets, data) = read_offsets_and_data(cur, rows)?;
            Ok(if matches!(ty, DataType::String) {
                OwnedValues::String { offsets, data }
            } else {
                OwnedValues::Bytes { offsets, data }
            })
        }
        DataType::List(lt) => {
            let noffsets = cur.guard_len((rows as u64) + 1, 4)?;
            let mut offsets = Vec::with_capacity(noffsets);
            for _ in 0..noffsets {
                offsets.push(cur.u32()?);
            }
            let child_id = cur.uvarint()?;
            let child_bytes = cur.bytes()?;
            let last = offsets
                .last()
                .ok_or(DecodeError::Malformed("missing list offsets"))?;
            let child_rows = *last as usize;
            let values = super::decode_column(lt.element(), child_rows, child_id, child_bytes)?;
            Ok(OwnedValues::List { offsets, values })
        }
    }
}

fn read_i64s(cur: &mut Cursor, rows: usize) -> Result<Vec<i64>, DecodeError> {
    let n = cur.guard_len(rows as u64, 8)?;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(cur.i64()?);
    }
    Ok(v)
}

fn read_arrays(cur: &mut Cursor, rows: usize) -> Result<Vec<[u8; 16]>, DecodeError> {
    let n = cur.guard_len(rows as u64, 16)?;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(
            cur.raw(16)?
                .try_into()
                .expect("raw(16) returns exactly 16 bytes"),
        );
    }
    Ok(v)
}

fn read_offsets_and_data(
    cur: &mut Cursor,
    rows: usize,
) -> Result<(Vec<u32>, Vec<u8>), DecodeError> {
    let noffsets = cur.guard_len((rows as u64) + 1, 4)?;
    let mut offsets = Vec::with_capacity(noffsets);
    for _ in 0..noffsets {
        offsets.push(cur.u32()?);
    }
    let last = *offsets
        .last()
        .ok_or(DecodeError::Malformed("missing offsets"))?;
    let data = cur.raw(last as usize)?.to_vec();
    Ok((offsets, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Column;
    use crate::types::Value;

    fn round_trip(ty: &DataType, values: &[Value]) {
        let col = Column::from_values(ty, values).unwrap();
        let mut out = Sink::new();
        encode(&col, &mut out);
        let buf = out.into_vec();
        let mut cur = Cursor::new(&buf);
        let decoded = decode(ty, col.len(), &mut cur).unwrap();
        let rebuilt = Column::from_parts(ty.clone(), decoded, col.validity().cloned()).unwrap();
        assert_eq!(rebuilt, col, "plain round trip for {ty}");
        assert!(cur.is_empty());
    }

    #[test]
    fn every_scalar_kind_round_trips() {
        round_trip(&DataType::Bool, &[Value::Bool(true), Value::Bool(false)]);
        round_trip(&DataType::Int64, &[Value::Int64(1), Value::Int64(-2)]);
        round_trip(&DataType::UInt64, &[Value::UInt64(1), Value::UInt64(2)]);
        round_trip(
            &DataType::Float64,
            &[Value::Float64(1.5), Value::Float64(-2.0)],
        );
        round_trip(
            &DataType::String,
            &[Value::String("a".into()), Value::String("bb".into())],
        );
        round_trip(
            &DataType::Bytes,
            &[Value::Bytes(vec![1, 2]), Value::Bytes(vec![])],
        );
        round_trip(
            &DataType::Timestamp,
            &[Value::Timestamp(1), Value::Timestamp(-1)],
        );
        round_trip(&DataType::Date, &[Value::Date(1), Value::Date(-1)]);
    }

    #[test]
    fn empty_column_round_trips() {
        round_trip(&DataType::Int64, &[]);
        round_trip(&DataType::String, &[]);
        let list_ty = DataType::list(DataType::String).unwrap();
        round_trip(&list_ty, &[]);
    }

    #[test]
    fn list_of_string_with_null_elements_and_null_lists_round_trips() {
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
}
