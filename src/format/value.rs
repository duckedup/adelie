//! The value codec (SPEC §5, D0008): how footer stats, value-set entries and bloom hash
//! input encode one `Value`. LIST has none of its own — its stats are always absent.

use std::net::{IpAddr, Ipv6Addr};

use crate::types::{DataType, Decimal, Ip, Value};

use super::error::DecodeError;
use super::wire::{Cursor, Sink};

/// 10^`exp`; `exp` here is always a `DECIMAL` precision, so it fits `i128` (max 10^38).
fn pow10(exp: u8) -> i128 {
    10i128.pow(exp as u32)
}

/// `v` is non-null and matches `ty`; anything else is a writer bug, so this panics.
pub(crate) fn encode_value(v: &Value, ty: &DataType, out: &mut Sink) {
    match (v, ty) {
        (Value::Bool(b), DataType::Bool) => out.u8(*b as u8),
        (Value::Int64(x), DataType::Int64) => out.i64(*x),
        (Value::UInt64(x), DataType::UInt64) => out.u64(*x),
        (Value::Float64(x), DataType::Float64) => out.u64(x.to_bits()),
        (Value::Decimal(d), DataType::Decimal(_)) => out.i128(d.unscaled()),
        (Value::String(s), DataType::String) => out.str(s),
        (Value::Bytes(b), DataType::Bytes) => out.bytes(b),
        (Value::Timestamp(x), DataType::Timestamp) => out.i64(*x),
        (Value::Date(x), DataType::Date) => out.i32(*x),
        (Value::Uuid(u), DataType::Uuid) => out.raw(u),
        (Value::Ip(ip), DataType::Ip) => out.raw(&ip.octets()),
        _ => panic!("encode_value: value {v:?} does not match type {ty}"),
    }
}

pub(crate) fn decode_value(cur: &mut Cursor, ty: &DataType) -> Result<Value, DecodeError> {
    match ty {
        DataType::Bool => match cur.u8()? {
            0 => Ok(Value::Bool(false)),
            1 => Ok(Value::Bool(true)),
            _ => Err(DecodeError::Malformed("bool byte not 0 or 1")),
        },
        DataType::Int64 => Ok(Value::Int64(cur.i64()?)),
        DataType::UInt64 => Ok(Value::UInt64(cur.u64()?)),
        DataType::Float64 => Ok(Value::Float64(f64::from_bits(cur.u64()?))),
        DataType::Decimal(dt) => {
            let unscaled = cur.i128()?;
            if unscaled.unsigned_abs() >= pow10(dt.precision()) as u128 {
                return Err(DecodeError::Malformed("decimal magnitude exceeds precision"));
            }
            let d = Decimal::new(unscaled, dt.scale())
                .map_err(|_| DecodeError::Malformed("bad decimal value"))?;
            Ok(Value::Decimal(d))
        }
        DataType::String => Ok(Value::String(cur.str()?.to_string())),
        DataType::Bytes => Ok(Value::Bytes(cur.bytes()?.to_vec())),
        DataType::Timestamp => Ok(Value::Timestamp(cur.i64()?)),
        DataType::Date => Ok(Value::Date(cur.i32()?)),
        DataType::Uuid => Ok(Value::Uuid(cur.raw(16)?.try_into().unwrap())),
        DataType::Ip => {
            let octets: [u8; 16] = cur.raw(16)?.try_into().unwrap();
            Ok(Value::Ip(Ip::from(IpAddr::V6(Ipv6Addr::from(octets)))))
        }
        DataType::List(_) => Err(DecodeError::Malformed("list has no value codec")),
    }
}

pub(crate) fn encode_opt(v: Option<&Value>, ty: &DataType, out: &mut Sink) {
    match v {
        Some(v) => {
            out.u8(1);
            encode_value(v, ty, out);
        }
        None => out.u8(0),
    }
}

pub(crate) fn decode_opt(cur: &mut Cursor, ty: &DataType) -> Result<Option<Value>, DecodeError> {
    match cur.u8()? {
        0 => Ok(None),
        1 => Ok(Some(decode_value(cur, ty)?)),
        _ => Err(DecodeError::Malformed("opt_value tag not 0 or 1")),
    }
}

/// `encode_value` into a fresh `Vec`: the hash input for bloom/n-gram skip structures.
pub(crate) fn value_bytes(v: &Value, ty: &DataType) -> Vec<u8> {
    let mut s = Sink::new();
    encode_value(v, ty, &mut s);
    s.into_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::total_cmp;
    use std::cmp::Ordering::Equal;

    fn round_trip(v: Value, ty: DataType) {
        let mut s = Sink::new();
        encode_value(&v, &ty, &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        let out = decode_value(&mut c, &ty).unwrap();
        assert_eq!(total_cmp(&v, &out), Some(Equal));
        if let Value::Float64(x) = v {
            let Value::Float64(y) = out else { panic!("expected float") };
            assert_eq!(x.to_bits(), y.to_bits());
        }
    }

    #[test]
    fn every_type_round_trips() {
        round_trip(Value::Bool(true), DataType::Bool);
        round_trip(Value::Int64(-7), DataType::Int64);
        round_trip(Value::UInt64(7), DataType::UInt64);
        round_trip(Value::Float64(1.5), DataType::Float64);
        round_trip(Value::Float64(f64::NAN), DataType::Float64);
        round_trip(Value::Float64(-0.0), DataType::Float64);
        round_trip(
            Value::Decimal(Decimal::new(0, 0).unwrap()),
            DataType::decimal(38, 0).unwrap(),
        );
        round_trip(
            Value::Decimal(Decimal::new(9, 38).unwrap()),
            DataType::decimal(38, 38).unwrap(),
        );
        round_trip(Value::String("hi".to_string()), DataType::String);
        round_trip(Value::Bytes(vec![1, 2, 3]), DataType::Bytes);
        round_trip(Value::Timestamp(123), DataType::Timestamp);
        round_trip(Value::Date(-1), DataType::Date);
        round_trip(Value::Uuid([9; 16]), DataType::Uuid);
        round_trip(Value::Ip(Ip::from("127.0.0.1".parse::<IpAddr>().unwrap())), DataType::Ip);
    }

    #[test]
    fn opt_value_round_trips_none_and_some() {
        let mut s = Sink::new();
        encode_opt(None, &DataType::Int64, &mut s);
        encode_opt(Some(&Value::Int64(5)), &DataType::Int64, &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert_eq!(decode_opt(&mut c, &DataType::Int64).unwrap(), None);
        assert_eq!(
            decode_opt(&mut c, &DataType::Int64).unwrap(),
            Some(Value::Int64(5))
        );
    }

    #[test]
    fn bad_bool_byte_is_rejected() {
        let buf = [2u8];
        let mut c = Cursor::new(&buf);
        assert!(matches!(
            decode_value(&mut c, &DataType::Bool),
            Err(DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn list_has_no_value_codec() {
        let buf = [0u8];
        let mut c = Cursor::new(&buf);
        let ty = DataType::list(DataType::Int64).unwrap();
        assert_eq!(
            decode_value(&mut c, &ty),
            Err(DecodeError::Malformed("list has no value codec"))
        );
    }
}
