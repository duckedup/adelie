//! Chunk encodings (SPEC §5, D0008): one stable id per encoding, chosen per chunk.
mod bitpack;
mod delta;
mod dict;
mod frame_of_ref;
mod nulls;
mod plain;
mod rle;
mod select;
mod xor;

use crate::exec::{Bitmap, Column, ColumnValues, OwnedValues};
use crate::types::DataType;

pub(crate) use bitpack::{BitReader, BitWriter, read_for, write_for};

use super::MAX_DECODE_ROWS;
use super::error::DecodeError;
use super::wire::{Cursor, Sink};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Encoding {
    Plain,
    Dict,
    Rle,
    For,
    Delta,
    DeltaOfDelta,
    Xor,
}

impl Encoding {
    pub fn id(self) -> u64 {
        match self {
            Encoding::Plain => 0,
            Encoding::Dict => 1,
            Encoding::Rle => 2,
            Encoding::For => 3,
            Encoding::Delta => 4,
            Encoding::DeltaOfDelta => 5,
            Encoding::Xor => 6,
        }
    }

    pub fn from_id(id: u64) -> Option<Encoding> {
        match id {
            0 => Some(Encoding::Plain),
            1 => Some(Encoding::Dict),
            2 => Some(Encoding::Rle),
            3 => Some(Encoding::For),
            4 => Some(Encoding::Delta),
            5 => Some(Encoding::DeltaOfDelta),
            6 => Some(Encoding::Xor),
            _ => None,
        }
    }

    /// The format table's "types" column; PLAIN applies to everything (LIST included), so a
    /// non-PLAIN encoding never matches LIST because none of its arms mention it.
    pub fn applies_to(self, ty: &DataType) -> bool {
        match self {
            Encoding::Plain => true,
            Encoding::Dict => matches!(ty, DataType::String | DataType::Bytes),
            Encoding::Rle => matches!(
                ty,
                DataType::Bool | DataType::Int64 | DataType::UInt64 | DataType::Timestamp | DataType::Date
            ),
            Encoding::For | Encoding::Delta | Encoding::DeltaOfDelta => matches!(
                ty,
                DataType::Int64 | DataType::UInt64 | DataType::Timestamp | DataType::Date
            ),
            Encoding::Xor => matches!(ty, DataType::Float64),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Encoding::Plain => "plain",
            Encoding::Dict => "dict",
            Encoding::Rle => "rle",
            Encoding::For => "for",
            Encoding::Delta => "delta",
            Encoding::DeltaOfDelta => "delta_of_delta",
            Encoding::Xor => "xor",
        }
    }
}

/// Selection (`select.rs`), or `pin` when given. Panics if `pin` does not apply to `col`'s
/// type: the writer checks first, so reaching this means a writer bug.
pub(crate) fn encode_column(col: &Column, pin: Option<Encoding>) -> (Encoding, Vec<u8>) {
    let enc = match pin {
        Some(e) => {
            assert!(
                e.applies_to(col.data_type()),
                "pinned encoding {e:?} does not apply to {}",
                col.data_type()
            );
            e
        }
        None => select::choose(col),
    };
    (enc, encode_with(col, enc))
}

/// Validity section, then `enc`'s values. `enc` must apply to `col`'s type (caller's job).
pub(crate) fn encode_with(col: &Column, enc: Encoding) -> Vec<u8> {
    let mut out = Sink::new();
    nulls::encode_validity(col.validity(), col.len(), &mut out);
    encode_values(col, enc, &mut out);
    out.into_vec()
}

/// Inverse of `encode_with`. Unknown or inapplicable `enc_id` is `UnknownEncoding`; trailing
/// bytes after the values section is `Malformed`.
pub(crate) fn decode_column(
    ty: &DataType,
    rows: usize,
    enc_id: u64,
    bytes: &[u8],
) -> Result<Column, DecodeError> {
    if rows > MAX_DECODE_ROWS {
        return Err(DecodeError::Malformed("row count exceeds MAX_DECODE_ROWS"));
    }
    let enc = Encoding::from_id(enc_id)
        .filter(|e| e.applies_to(ty))
        .ok_or(DecodeError::UnknownEncoding(enc_id))?;
    let mut cur = Cursor::new(bytes);
    let validity = nulls::decode_validity(&mut cur, rows)?;
    let values = decode_values(ty, rows, enc, validity.as_ref(), &mut cur)?;
    if !cur.is_empty() {
        return Err(DecodeError::Malformed("trailing bytes after chunk values"));
    }
    Column::from_parts(ty.clone(), values, validity).map_err(|_| DecodeError::Malformed("column parts"))
}

fn encode_values(col: &Column, enc: Encoding, out: &mut Sink) {
    match enc {
        Encoding::Plain => plain::encode(col, out),
        Encoding::Dict => encode_dict(col, out),
        Encoding::Rle if matches!(col.data_type(), DataType::Bool) => encode_bool_rle(col, out),
        Encoding::Rle | Encoding::For | Encoding::Delta | Encoding::DeltaOfDelta => {
            encode_int_like(col, enc, out)
        }
        Encoding::Xor => encode_xor(col, out),
    }
}

fn encode_dict(col: &Column, out: &mut Sink) {
    match col.values() {
        ColumnValues::String { offsets, data } | ColumnValues::Bytes { offsets, data } => {
            dict::encode(offsets, data, col.validity(), out)
        }
        _ => unreachable!("dict encoding does not apply to {}", col.data_type()),
    }
}

fn encode_bool_rle(col: &Column, out: &mut Sink) {
    let ColumnValues::Bool(bm) = col.values() else {
        unreachable!("rle-bool dispatch requires a BOOL column")
    };
    rle::encode_bool(bm, out);
}

fn encode_xor(col: &Column, out: &mut Sink) {
    let ColumnValues::Float64(v) = col.values() else {
        unreachable!("xor dispatch requires a FLOAT64 column")
    };
    xor::encode(v, out);
}

/// INT64/TIMESTAMP borrow directly; DATE widens `i32` to `i64` (SPEC §5) into an owned copy.
fn as_i64(cv: ColumnValues<'_>) -> Option<std::borrow::Cow<'_, [i64]>> {
    match cv {
        ColumnValues::Int64(v) | ColumnValues::Timestamp(v) => Some(std::borrow::Cow::Borrowed(v)),
        ColumnValues::Date(v) => Some(std::borrow::Cow::Owned(v.iter().map(|&x| x as i64).collect())),
        _ => None,
    }
}

fn encode_int_like(col: &Column, enc: Encoding, out: &mut Sink) {
    if let Some(v) = as_i64(col.values()) {
        match enc {
            Encoding::Rle => rle::encode_i64(&v, out),
            Encoding::For => frame_of_ref::encode_i64(&v, out),
            Encoding::Delta => delta::encode_delta_i64(&v, out),
            Encoding::DeltaOfDelta => delta::encode_dod_i64(&v, out),
            _ => unreachable!(),
        }
        return;
    }
    let ColumnValues::UInt64(v) = col.values() else {
        unreachable!("int-like encoding does not apply to {}", col.data_type())
    };
    match enc {
        Encoding::Rle => rle::encode_u64(v, out),
        Encoding::For => frame_of_ref::encode_u64(v, out),
        Encoding::Delta => delta::encode_delta_u64(v, out),
        Encoding::DeltaOfDelta => delta::encode_dod_u64(v, out),
        _ => unreachable!(),
    }
}

fn decode_values(
    ty: &DataType,
    rows: usize,
    enc: Encoding,
    validity: Option<&Bitmap>,
    cur: &mut Cursor,
) -> Result<OwnedValues, DecodeError> {
    match enc {
        Encoding::Plain => plain::decode(ty, rows, cur),
        Encoding::Dict => decode_dict(ty, rows, validity, cur),
        Encoding::Rle if matches!(ty, DataType::Bool) => {
            Ok(OwnedValues::Bool(rle::decode_bool(cur, rows)?))
        }
        Encoding::Rle | Encoding::For | Encoding::Delta | Encoding::DeltaOfDelta => {
            decode_int_like(ty, rows, enc, cur)
        }
        Encoding::Xor => Ok(OwnedValues::Float64(xor::decode(cur, rows)?)),
    }
}

fn decode_dict(
    ty: &DataType,
    rows: usize,
    validity: Option<&Bitmap>,
    cur: &mut Cursor,
) -> Result<OwnedValues, DecodeError> {
    let (offsets, data) = dict::decode(cur, rows, validity)?;
    match ty {
        DataType::String => Ok(OwnedValues::String { offsets, data }),
        DataType::Bytes => Ok(OwnedValues::Bytes { offsets, data }),
        _ => Err(DecodeError::Malformed("dict encoding used on a non-string/bytes type")),
    }
}

fn decode_int_like(ty: &DataType, rows: usize, enc: Encoding, cur: &mut Cursor) -> Result<OwnedValues, DecodeError> {
    match ty {
        DataType::Int64 => Ok(OwnedValues::Int64(decode_i64_by(enc, cur, rows)?)),
        DataType::Timestamp => Ok(OwnedValues::Timestamp(decode_i64_by(enc, cur, rows)?)),
        DataType::UInt64 => Ok(OwnedValues::UInt64(decode_u64_by(enc, cur, rows)?)),
        DataType::Date => {
            let v = decode_i64_by(enc, cur, rows)?;
            let mut narrowed = Vec::with_capacity(v.len());
            for x in v {
                narrowed.push(i32::try_from(x).map_err(|_| DecodeError::Malformed("date value out of i32 range"))?);
            }
            Ok(OwnedValues::Date(narrowed))
        }
        _ => Err(DecodeError::Malformed("encoding does not apply to this type")),
    }
}

fn decode_i64_by(enc: Encoding, cur: &mut Cursor, rows: usize) -> Result<Vec<i64>, DecodeError> {
    match enc {
        Encoding::Rle => rle::decode_i64(cur, rows),
        Encoding::For => frame_of_ref::decode_i64(cur, rows),
        Encoding::Delta => delta::decode_delta_i64(cur, rows),
        Encoding::DeltaOfDelta => delta::decode_dod_i64(cur, rows),
        _ => unreachable!(),
    }
}

fn decode_u64_by(enc: Encoding, cur: &mut Cursor, rows: usize) -> Result<Vec<u64>, DecodeError> {
    match enc {
        Encoding::Rle => rle::decode_u64(cur, rows),
        Encoding::For => frame_of_ref::decode_u64(cur, rows),
        Encoding::Delta => delta::decode_delta_u64(cur, rows),
        Encoding::DeltaOfDelta => delta::decode_dod_u64(cur, rows),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;
    use adelie_harness::rng::SplitMix64;

    fn all_encodings() -> [Encoding; 7] {
        [
            Encoding::Plain,
            Encoding::Dict,
            Encoding::Rle,
            Encoding::For,
            Encoding::Delta,
            Encoding::DeltaOfDelta,
            Encoding::Xor,
        ]
    }

    fn round_trip(ty: &DataType, values: &[Value]) {
        let col = Column::from_values(ty, values).unwrap();
        for enc in all_encodings() {
            if !enc.applies_to(ty) {
                continue;
            }
            let bytes = encode_with(&col, enc);
            let decoded = decode_column(ty, col.len(), enc.id(), &bytes).unwrap();
            assert_eq!(decoded, col, "{} round trip via {}", ty, enc.name());
        }
    }

    #[test]
    fn every_applicable_encoding_round_trips_with_nulls() {
        round_trip(
            &DataType::Bool,
            &[Value::Bool(true), Value::Null, Value::Bool(false)],
        );
        round_trip(
            &DataType::Int64,
            &[Value::Int64(1), Value::Null, Value::Int64(-2), Value::Int64(3)],
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
            &DataType::String,
            &[Value::String("a".into()), Value::Null, Value::String("bb".into())],
        );
        round_trip(
            &DataType::Bytes,
            &[Value::Bytes(vec![1, 2]), Value::Null, Value::Bytes(vec![])],
        );
        round_trip(
            &DataType::Timestamp,
            &[Value::Timestamp(1), Value::Null, Value::Timestamp(-1)],
        );
        round_trip(&DataType::Date, &[Value::Date(1), Value::Null, Value::Date(-1)]);
        round_trip(
            &DataType::Uuid,
            &[Value::Uuid([1; 16]), Value::Null, Value::Uuid([2; 16])],
        );
        let ip = crate::types::Ip::from(std::net::IpAddr::from([127, 0, 0, 1]));
        round_trip(&DataType::Ip, &[Value::Ip(ip), Value::Null, Value::Ip(ip)]);
    }

    #[test]
    fn every_applicable_encoding_round_trips_with_no_validity_and_zero_rows() {
        round_trip(&DataType::Int64, &[Value::Int64(1), Value::Int64(2)]);
        round_trip(&DataType::Int64, &[]);
        round_trip(&DataType::UInt64, &[]);
        round_trip(&DataType::Bool, &[]);
        round_trip(&DataType::String, &[]);
        round_trip(&DataType::Float64, &[]);
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

    #[test]
    fn same_column_encodes_identically_every_time() {
        let values: Vec<Value> = (0..500i64).map(Value::Int64).collect();
        let col = Column::from_values(&DataType::Int64, &values).unwrap();
        let (enc1, bytes1) = encode_column(&col, None);
        let (enc2, bytes2) = encode_column(&col, None);
        assert_eq!(enc1, enc2);
        assert_eq!(bytes1, bytes2);
    }

    fn hostile_types() -> Vec<DataType> {
        vec![
            DataType::Bool,
            DataType::Int64,
            DataType::UInt64,
            DataType::Float64,
            DataType::decimal(10, 2).unwrap(),
            DataType::String,
            DataType::Bytes,
            DataType::Timestamp,
            DataType::Date,
            DataType::Uuid,
            DataType::Ip,
            DataType::list(DataType::Int64).unwrap(),
        ]
    }

    #[test]
    fn hostile_bytes_never_panic_and_valid_rows_are_gettable() {
        let mut rng = SplitMix64::new(0xC0FF_EE);
        let iterations = if cfg!(miri) { 50 } else { 2_000 };
        for _ in 0..iterations {
            let rows = rng.range(0, 8) as usize;
            let len = rng.range(0, 40) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            for ty in hostile_types() {
                for id in 0..=7u64 {
                    if let Ok(col) = decode_column(&ty, rows, id, &bytes) {
                        for i in 0..col.len() {
                            let _ = col.get(i);
                        }
                    }
                }
            }
        }
    }
}
