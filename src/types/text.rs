//! Canonical text (contract rule 5): `Value::to_text`, the sibling `<column>::string` reads
//! for a lossy write and DECIMAL/TIMESTAMP/DATE's on-disk-independent rendering.

use super::value::Value;

impl Value {
    /// `None` for NULL. Every other kind has one canonical spelling (contract rule 5).
    pub fn to_text(&self) -> Option<String> {
        Some(match self {
            Value::Null => return None,
            Value::Bool(b) => b.to_string(),
            Value::Int64(n) => n.to_string(),
            Value::UInt64(n) => n.to_string(),
            Value::Float64(f) => format_float(*f),
            Value::Decimal(d) => format_decimal(d.unscaled(), d.scale()),
            Value::String(s) => s.clone(),
            Value::Bytes(b) => format_bytes(b),
            Value::Timestamp(ns) => format_timestamp(*ns),
            Value::Date(days) => format_date(*days as i64),
            Value::Uuid(bytes) => format_uuid(bytes),
            Value::Ip(ip) => ip.to_ip_addr().to_string(),
            Value::List(items) => format_list(items),
        })
    }
}

fn format_float(f: f64) -> String {
    if f.is_nan() {
        "NaN".to_string()
    } else if f.is_infinite() {
        if f > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        }
    } else {
        format!("{f}")
    }
}

/// Exactly `scale` fractional digits, e.g. `Decimal(-5, 2)` -> `"-0.05"` (the sign belongs
/// to the whole value, not the zero integer part).
fn format_decimal(unscaled: i128, scale: u8) -> String {
    let sign = if unscaled < 0 { "-" } else { "" };
    let magnitude = unscaled.unsigned_abs();
    if scale == 0 {
        return format!("{sign}{magnitude}");
    }
    let factor = 10u128.pow(scale as u32);
    let int_part = magnitude / factor;
    let frac_part = magnitude % factor;
    let width = scale as usize;
    format!("{sign}{int_part}.{frac_part:0width$}")
}

fn format_bytes(bytes: &[u8]) -> String {
    let mut out = String::from("\\x");
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn format_uuid(bytes: &[u8; 16]) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn format_list(items: &[Value]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|v| match v {
            Value::Null => "NULL".to_string(),
            Value::String(s) => format!("'{}'", s.replace('\'', "''")),
            other => other.to_text().unwrap_or_else(|| "NULL".to_string()),
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

/// Howard Hinnant's civil-from-days: a day count since 1970-01-01 to (year, month, day).
/// http://howardhinnant.github.io/date_algorithms.html#civil_from_days
/// Duplicated in `harness/src/civil.rs` — adelie cannot depend on the harness.
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn format_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

const NS_PER_DAY: i64 = 86_400_000_000_000;

fn format_timestamp(ns: i64) -> String {
    let days = ns.div_euclid(NS_PER_DAY);
    let of_day = ns.rem_euclid(NS_PER_DAY);
    let (y, mo, d) = civil_from_days(days);
    let secs = of_day / 1_000_000_000;
    let frac_ns = (of_day % 1_000_000_000) as u32;
    let (h, mi, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let frac = format_fraction(frac_ns);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}{frac}Z")
}

/// The shortest exact group: milli-, micro- or nanoseconds, or nothing when there is none.
fn format_fraction(ns: u32) -> String {
    if ns == 0 {
        String::new()
    } else if ns.is_multiple_of(1_000_000) {
        format!(".{:03}", ns / 1_000_000)
    } else if ns.is_multiple_of(1_000) {
        format!(".{:06}", ns / 1_000)
    } else {
        format!(".{ns:09}")
    }
}

#[cfg(test)]
mod tests {
    use super::super::value::Decimal;
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn decimal_to_text_keeps_sign_on_zero_integer_part() {
        assert_eq!(
            Value::Decimal(Decimal::new(150, 2).unwrap())
                .to_text()
                .unwrap(),
            "1.50"
        );
        assert_eq!(
            Value::Decimal(Decimal::new(-5, 2).unwrap())
                .to_text()
                .unwrap(),
            "-0.05"
        );
    }

    #[test]
    fn timestamp_epoch_has_no_fraction() {
        assert_eq!(
            Value::Timestamp(0).to_text().unwrap(),
            "1970-01-01T00:00:00Z"
        );
    }

    #[test]
    fn timestamp_milli_fraction_is_three_digits() {
        assert_eq!(
            Value::Timestamp(1_500_000_000).to_text().unwrap(),
            "1970-01-01T00:00:01.500Z"
        );
    }

    #[test]
    fn timestamp_micro_fraction_is_six_digits() {
        assert_eq!(
            Value::Timestamp(1_000_001_000).to_text().unwrap(),
            "1970-01-01T00:00:01.000001Z"
        );
    }

    #[test]
    fn timestamp_negative_instant_renders_the_prior_day() {
        assert_eq!(
            Value::Timestamp(-1).to_text().unwrap(),
            "1969-12-31T23:59:59.999999999Z"
        );
    }

    #[test]
    fn date_renders_iso() {
        assert_eq!(Value::Date(19724).to_text().unwrap(), "2024-01-02");
    }

    #[test]
    fn uuid_renders_hyphenated_lowercase() {
        let bytes: [u8; 16] = [
            0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44,
            0x00, 0x00,
        ];
        assert_eq!(
            Value::Uuid(bytes).to_text().unwrap(),
            "550e8400-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn ip_renders_v4_dotted_and_v6_compressed() {
        let v4: IpAddr = "127.0.0.1".parse().unwrap();
        let v6: IpAddr = "::1".parse().unwrap();
        assert_eq!(Value::Ip(v4.into()).to_text().unwrap(), "127.0.0.1");
        assert_eq!(Value::Ip(v6.into()).to_text().unwrap(), "::1");
    }

    #[test]
    fn bytes_render_as_lowercase_hex() {
        assert_eq!(Value::Bytes(vec![0xab, 0x01]).to_text().unwrap(), "\\xab01");
    }

    #[test]
    fn list_quotes_strings_and_shows_null() {
        let list = Value::List(vec![Value::String("it's".to_string()), Value::Null]);
        assert_eq!(list.to_text().unwrap(), "['it''s', NULL]");
    }

    #[test]
    fn null_has_no_text() {
        assert_eq!(Value::Null.to_text(), None);
    }
}
