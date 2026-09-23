//! Logical type ids (SPEC §5, D0008): the `DataType <-> type_desc` codec. Ids are explicit
//! and never derived from enum order, so a renumbering is a deliberate, reviewed edit.

use crate::types::DataType;

use super::error::DecodeError;
use super::wire::{Cursor, Sink};

pub(crate) const TYPE_BOOL: u64 = 1;
pub(crate) const TYPE_INT64: u64 = 2;
pub(crate) const TYPE_UINT64: u64 = 3;
pub(crate) const TYPE_FLOAT64: u64 = 4;
pub(crate) const TYPE_DECIMAL: u64 = 5;
pub(crate) const TYPE_STRING: u64 = 6;
pub(crate) const TYPE_BYTES: u64 = 7;
pub(crate) const TYPE_TIMESTAMP: u64 = 8;
pub(crate) const TYPE_DATE: u64 = 9;
pub(crate) const TYPE_UUID: u64 = 10;
pub(crate) const TYPE_IP: u64 = 11;
pub(crate) const TYPE_LIST: u64 = 12;

/// `type_desc := uvarint type_id, bytes params`, for an arbitrary id/params pair — used by
/// `encode_type` and by tests that need an id `encode_type` would never produce.
pub(crate) fn encode_raw_type(id: u64, params: &[u8], out: &mut Sink) {
    out.uvarint(id);
    out.bytes(params);
}

/// Exhaustive over `DataType`'s variants, so a new one fails to compile until it has an id.
pub(crate) fn encode_type(ty: &DataType, out: &mut Sink) {
    match ty {
        DataType::Bool => encode_raw_type(TYPE_BOOL, &[], out),
        DataType::Int64 => encode_raw_type(TYPE_INT64, &[], out),
        DataType::UInt64 => encode_raw_type(TYPE_UINT64, &[], out),
        DataType::Float64 => encode_raw_type(TYPE_FLOAT64, &[], out),
        DataType::Decimal(dt) => {
            encode_raw_type(TYPE_DECIMAL, &[dt.precision(), dt.scale()], out)
        }
        DataType::String => encode_raw_type(TYPE_STRING, &[], out),
        DataType::Bytes => encode_raw_type(TYPE_BYTES, &[], out),
        DataType::Timestamp => encode_raw_type(TYPE_TIMESTAMP, &[], out),
        DataType::Date => encode_raw_type(TYPE_DATE, &[], out),
        DataType::Uuid => encode_raw_type(TYPE_UUID, &[], out),
        DataType::Ip => encode_raw_type(TYPE_IP, &[], out),
        DataType::List(lt) => {
            let mut params = Sink::new();
            encode_type(lt.element(), &mut params);
            encode_raw_type(TYPE_LIST, &params.into_vec(), out);
        }
    }
}

/// DECIMAL params go through `DataType::decimal`; a nested LIST is rejected through
/// `DataType::list`. Both map their `TypeError` to `Malformed`, not a distinct variant.
pub(crate) fn decode_type(cur: &mut Cursor) -> Result<DataType, DecodeError> {
    let id = cur.uvarint()?;
    let params = cur.bytes()?;
    let mut p = Cursor::new(params);
    match id {
        TYPE_BOOL => Ok(DataType::Bool),
        TYPE_INT64 => Ok(DataType::Int64),
        TYPE_UINT64 => Ok(DataType::UInt64),
        TYPE_FLOAT64 => Ok(DataType::Float64),
        TYPE_DECIMAL => {
            let precision = p.u8()?;
            let scale = p.u8()?;
            DataType::decimal(precision, scale)
                .map_err(|_| DecodeError::Malformed("bad decimal params"))
        }
        TYPE_STRING => Ok(DataType::String),
        TYPE_BYTES => Ok(DataType::Bytes),
        TYPE_TIMESTAMP => Ok(DataType::Timestamp),
        TYPE_DATE => Ok(DataType::Date),
        TYPE_UUID => Ok(DataType::Uuid),
        TYPE_IP => Ok(DataType::Ip),
        TYPE_LIST => {
            let elem = decode_type(&mut p)?;
            DataType::list(elem).map_err(|_| DecodeError::Malformed("nested list"))
        }
        other => Err(DecodeError::UnknownTypeId(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(ty: DataType) {
        let mut s = Sink::new();
        encode_type(&ty, &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert_eq!(decode_type(&mut c).unwrap(), ty);
    }

    #[test]
    fn every_type_round_trips() {
        round_trip(DataType::Bool);
        round_trip(DataType::Int64);
        round_trip(DataType::UInt64);
        round_trip(DataType::Float64);
        round_trip(DataType::decimal(38, 0).unwrap());
        round_trip(DataType::decimal(1, 1).unwrap());
        round_trip(DataType::String);
        round_trip(DataType::Bytes);
        round_trip(DataType::Timestamp);
        round_trip(DataType::Date);
        round_trip(DataType::Uuid);
        round_trip(DataType::Ip);
        round_trip(DataType::list(DataType::Uuid).unwrap());
    }

    #[test]
    fn unknown_id_is_reported_with_its_number() {
        let mut s = Sink::new();
        encode_raw_type(999, &[], &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert_eq!(decode_type(&mut c), Err(DecodeError::UnknownTypeId(999)));
    }

    #[test]
    fn out_of_range_decimal_params_are_malformed() {
        let mut s = Sink::new();
        encode_raw_type(TYPE_DECIMAL, &[39, 0], &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert!(matches!(decode_type(&mut c), Err(DecodeError::Malformed(_))));
    }

    #[test]
    fn ids_are_pinned_to_their_numbers() {
        assert_eq!(TYPE_BOOL, 1);
        assert_eq!(TYPE_INT64, 2);
        assert_eq!(TYPE_UINT64, 3);
        assert_eq!(TYPE_FLOAT64, 4);
        assert_eq!(TYPE_DECIMAL, 5);
        assert_eq!(TYPE_STRING, 6);
        assert_eq!(TYPE_BYTES, 7);
        assert_eq!(TYPE_TIMESTAMP, 8);
        assert_eq!(TYPE_DATE, 9);
        assert_eq!(TYPE_UUID, 10);
        assert_eq!(TYPE_IP, 11);
        assert_eq!(TYPE_LIST, 12);
    }
}
