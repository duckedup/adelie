//! Inverse of `Value::to_text` (`text.rs`): canonical text, plus a few permissive
//! extensions (whitespace, `+`, bool synonyms, integer text, DATE-as-TIMESTAMP), back to a
//! `Value` of a given `DataType`. Never used for LIST.

use std::net::IpAddr;

use super::datatype::{DataType, DecimalType};
use super::value::{Decimal, Ip, Value, pow10};

const NS_PER_DAY: i64 = 86_400_000_000_000;

impl Value {
    /// `None` on anything outside the canonical spelling plus the stated extensions. STRING
    /// is never trimmed (whitespace is significant data there).
    pub fn from_text(text: &str, ty: &DataType) -> Option<Value> {
        if matches!(ty, DataType::String) {
            return Some(Value::String(text.to_string()));
        }
        let text = text.trim_matches(|c: char| c.is_ascii_whitespace());
        match ty {
            DataType::Bool => parse_bool(text),
            DataType::Int64 => strip_plus(text).parse::<i64>().ok().map(Value::Int64),
            DataType::UInt64 => strip_plus(text).parse::<u64>().ok().map(Value::UInt64),
            DataType::Float64 => strip_plus(text).parse::<f64>().ok().map(Value::Float64),
            DataType::Decimal(dt) => parse_decimal(text, *dt),
            DataType::Bytes => parse_bytes(text),
            DataType::Timestamp => parse_timestamp(text).map(Value::Timestamp),
            DataType::Date => parse_date(text).map(Value::Date),
            DataType::Uuid => parse_uuid(text),
            DataType::Ip => text.parse::<IpAddr>().ok().map(|a| Value::Ip(Ip::from(a))),
            DataType::String | DataType::List(_) => None,
        }
    }
}

fn strip_plus(s: &str) -> &str {
    s.strip_prefix('+').unwrap_or(s)
}

fn parse_bool(text: &str) -> Option<Value> {
    match text.to_ascii_lowercase().as_str() {
        "true" | "t" | "1" => Some(Value::Bool(true)),
        "false" | "f" | "0" => Some(Value::Bool(false)),
        _ => None,
    }
}

/// Sign, digits, optional `.` and fractional digits, scaled exactly to `dt.scale()`. Extra
/// fractional digits (would lose precision) or a value past `dt`'s precision is `None`.
fn parse_decimal(text: &str, dt: DecimalType) -> Option<Value> {
    let text = strip_plus(text);
    let (neg, rest) = match text.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, text),
    };
    let (int_part, frac_part) = match rest.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (rest, None),
    };
    if int_part.is_empty() && frac_part.is_none_or(str::is_empty) {
        return None;
    }
    if !int_part.is_empty() && !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let scale = dt.scale();
    let int_digits: i128 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().ok()?
    };
    let unscaled = match frac_part {
        None => int_digits.checked_mul(pow10(scale))?,
        Some(f) => {
            if f.is_empty() || f.len() as u8 > scale || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let frac_digits: i128 = f.parse().ok()?;
            let pad = scale - f.len() as u8;
            int_digits
                .checked_mul(pow10(scale))?
                .checked_add(frac_digits.checked_mul(pow10(pad))?)?
        }
    };
    let unscaled = if neg { -unscaled } else { unscaled };
    if unscaled.unsigned_abs() >= pow10(dt.precision()) as u128 {
        return None;
    }
    Decimal::new(unscaled, scale).ok().map(Value::Decimal)
}

fn parse_bytes(text: &str) -> Option<Value> {
    let hex = text.strip_prefix("\\x")?;
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let bytes = hex.as_bytes();
    let mut out = Vec::with_capacity(hex.len() / 2);
    for chunk in bytes.chunks(2) {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(Value::Bytes(out))
}

fn parse_uuid(text: &str) -> Option<Value> {
    let bytes = text.as_bytes();
    if bytes.len() != 36 {
        return None;
    }
    for &i in &[8usize, 13, 18, 23] {
        if bytes[i] != b'-' {
            return None;
        }
    }
    let hex: String = text
        .char_indices()
        .filter(|(i, _)| !matches!(i, 8 | 13 | 18 | 23))
        .map(|(_, c)| c)
        .collect();
    let hex = hex.as_bytes();
    let mut out = [0u8; 16];
    for i in 0..16 {
        let hi = (hex[2 * i] as char).to_digit(16)?;
        let lo = (hex[2 * i + 1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(Value::Uuid(out))
}

/// `days_from_civil` (Howard Hinnant), the inverse of `text::civil_from_days`. Splits off the
/// trailing `-MM-DD` so a negative (variable-width) year still parses.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m as i64 - 3 } else { m as i64 + 9 };
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn parse_date(text: &str) -> Option<i32> {
    if text.len() < 6 {
        return None;
    }
    let (year_part, rest) = text.split_at(text.len() - 6);
    let rb = rest.as_bytes();
    if rb[0] != b'-' || rb[3] != b'-' {
        return None;
    }
    let year: i64 = year_part.parse().ok()?;
    let month: u32 = rest[1..3].parse().ok()?;
    let day: u32 = rest[4..6].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    i32::try_from(days_from_civil(year, month, day)).ok()
}

fn parse_timestamp(text: &str) -> Option<i64> {
    if let Some(days) = parse_date(text) {
        return (days as i64).checked_mul(NS_PER_DAY);
    }
    let (date_part, time_part) = text.split_once('T')?;
    let days = parse_date(date_part)?;
    let time_part = time_part.strip_suffix('Z')?;
    let (hms, frac) = match time_part.split_once('.') {
        Some((h, f)) => (h, Some(f)),
        None => (time_part, None),
    };
    if hms.len() != 8 || hms.as_bytes()[2] != b':' || hms.as_bytes()[5] != b':' {
        return None;
    }
    let h: i64 = hms[0..2].parse().ok()?;
    let mi: i64 = hms[3..5].parse().ok()?;
    let s: i64 = hms[6..8].parse().ok()?;
    if h >= 24 || mi >= 60 || s >= 60 {
        return None;
    }
    let frac_ns: i64 = match frac {
        None => 0,
        Some(f) => {
            if f.is_empty() || f.len() > 9 || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let val: i64 = f.parse().ok()?;
            val * 10i64.pow(9 - f.len() as u32)
        }
    };
    let day_ns = (days as i64).checked_mul(NS_PER_DAY)?;
    let tod_ns = h * 3_600_000_000_000 + mi * 60_000_000_000 + s * 1_000_000_000 + frac_ns;
    day_ns.checked_add(tod_ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(precision: u8, scale: u8) -> DataType {
        DataType::decimal(precision, scale).unwrap()
    }

    fn round_trip(v: Value, ty: &DataType) {
        let text = v.to_text().expect("non-null value has text");
        assert_eq!(Value::from_text(&text, ty), Some(v), "round trip of {text:?}");
    }

    #[test]
    fn round_trip_every_scalar_kind() {
        round_trip(Value::Bool(true), &DataType::Bool);
        round_trip(Value::Bool(false), &DataType::Bool);
        round_trip(Value::Int64(-42), &DataType::Int64);
        round_trip(Value::UInt64(42), &DataType::UInt64);
        round_trip(Value::Float64(1.5), &DataType::Float64);
        round_trip(Value::Float64(f64::INFINITY), &DataType::Float64);
        round_trip(Value::Decimal(Decimal::new(-5, 2).unwrap()), &dt(5, 2));
        round_trip(Value::Decimal(Decimal::new(150, 2).unwrap()), &dt(5, 2));
        round_trip(Value::String("hello world".into()), &DataType::String);
        round_trip(Value::Bytes(vec![0xab, 0x01]), &DataType::Bytes);
        round_trip(Value::Bytes(vec![]), &DataType::Bytes);
        round_trip(Value::Timestamp(0), &DataType::Timestamp);
        round_trip(Value::Timestamp(-1), &DataType::Timestamp);
        round_trip(Value::Timestamp(1_500_000_000), &DataType::Timestamp);
        round_trip(Value::Date(19724), &DataType::Date);
        round_trip(Value::Date(-1), &DataType::Date);
        round_trip(Value::Uuid([0x11; 16]), &DataType::Uuid);
        let v4: IpAddr = "127.0.0.1".parse().unwrap();
        let v6: IpAddr = "::1".parse().unwrap();
        round_trip(Value::Ip(v4.into()), &DataType::Ip);
        round_trip(Value::Ip(v6.into()), &DataType::Ip);
    }

    /// NaN never equals itself, so the round trip is checked with `is_nan` instead of `==`.
    #[test]
    fn round_trip_nan() {
        let text = Value::Float64(f64::NAN).to_text().unwrap();
        match Value::from_text(&text, &DataType::Float64) {
            Some(Value::Float64(f)) => assert!(f.is_nan()),
            other => panic!("expected NaN, got {other:?}"),
        }
    }

    #[test]
    fn whitespace_and_plus_are_permissive_extensions() {
        assert_eq!(Value::from_text(" 12 ", &DataType::Int64), Some(Value::Int64(12)));
        assert_eq!(Value::from_text("+12", &DataType::Int64), Some(Value::Int64(12)));
        assert_eq!(Value::from_text("12x", &DataType::Int64), None);
    }

    #[test]
    fn bool_synonyms_case_insensitive() {
        for s in ["true", "TRUE", "t", "T", "1"] {
            assert_eq!(Value::from_text(s, &DataType::Bool), Some(Value::Bool(true)));
        }
        for s in ["false", "FALSE", "f", "F", "0"] {
            assert_eq!(Value::from_text(s, &DataType::Bool), Some(Value::Bool(false)));
        }
    }

    #[test]
    fn integer_text_for_float_and_decimal() {
        assert_eq!(Value::from_text("5", &DataType::Float64), Some(Value::Float64(5.0)));
        assert_eq!(
            Value::from_text("5", &dt(5, 2)),
            Some(Value::Decimal(Decimal::new(500, 2).unwrap()))
        );
    }

    #[test]
    fn decimal_rejects_extra_fractional_digits() {
        assert_eq!(Value::from_text("1.234", &dt(5, 2)), None);
    }

    #[test]
    fn date_text_for_timestamp_is_midnight() {
        assert_eq!(
            Value::from_text("2024-01-02", &DataType::Timestamp),
            Some(Value::Timestamp(19724 * NS_PER_DAY))
        );
    }

    #[test]
    fn negative_year_date_parses() {
        assert!(Value::from_text("-0001-06-15", &DataType::Date).is_some());
    }

    #[test]
    fn garbage_is_none() {
        assert_eq!(Value::from_text("not a number", &DataType::Int64), None);
        assert_eq!(Value::from_text("2024-13-01", &DataType::Date), None);
        assert_eq!(Value::from_text("not-a-uuid", &DataType::Uuid), None);
    }
}
