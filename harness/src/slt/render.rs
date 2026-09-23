//! Renders a `Value` as text under a declared column type, per the sqllogictest dialect's
//! rendering table.

use super::ColType;
use crate::civil;
use crate::engine::Value;

pub(super) fn render_row(row: &[Value], types: &[ColType]) -> String {
    row.iter()
        .zip(types)
        .map(|(v, t)| render_cell(v, *t))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Renders `v` as it would appear under an all-text (`T`) column; used where no `.slt`
/// column-type declaration applies, e.g. the bench differential's cross-check.
pub fn render_text(v: &Value) -> String {
    render_cell(v, ColType::Text)
}

/// `f` under `ColType::Int`: truncated toward zero.
fn format_float_as_int(f: f64) -> String {
    (f.trunc() as i64).to_string()
}

/// `f` under `ColType::Real`: 3 fractional digits, with `-0.000` shown as `0.000`.
fn format_float_as_real(f: f64) -> String {
    let s = format!("{f:.3}");
    if s == "-0.000" {
        "0.000".to_string()
    } else {
        s
    }
}

/// Exact decimal text: `scale` fractional digits, then trailing fractional zeros and a
/// dangling `.` stripped (`1.50` -> `1.5`, `2.00` -> `2`, `-0.05` stays `-0.05`).
fn format_decimal_text(value: i128, scale: u8) -> String {
    let scale = scale as usize;
    let digits = value.unsigned_abs().to_string();
    let mut s = if scale == 0 {
        digits
    } else if digits.len() <= scale {
        format!("0.{}{digits}", "0".repeat(scale - digits.len()))
    } else {
        let point = digits.len() - scale;
        format!("{}.{}", &digits[..point], &digits[point..])
    };
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if value < 0 && s != "0" {
        format!("-{s}")
    } else {
        s
    }
}

fn format_bytes(b: &[u8]) -> String {
    let mut s = String::from("\\x");
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

fn format_uuid(bytes: &[u8; 16]) -> String {
    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].concat(),
        hex[4..6].concat(),
        hex[6..8].concat(),
        hex[8..10].concat(),
        hex[10..16].concat(),
    )
}

fn render_cell(value: &Value, ty: ColType) -> String {
    if let Value::Float(f) = value {
        if f.is_nan() {
            return "NaN".to_string();
        }
        if f.is_infinite() {
            return if *f > 0.0 {
                "inf".to_string()
            } else {
                "-inf".to_string()
            };
        }
    }
    match value {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => match ty {
            ColType::Int | ColType::Text => n.to_string(),
            ColType::Real => format!("{:.3}", *n as f64),
        },
        Value::UInt(n) => match (ty, i64::try_from(*n)) {
            (ColType::Real, Ok(small)) => format!("{:.3}", small as f64),
            // Above i64::MAX this used to arrive as Text, whose digits pass through.
            _ => n.to_string(),
        },
        Value::Float(f) => match ty {
            ColType::Int => format_float_as_int(*f),
            ColType::Real => format_float_as_real(*f),
            ColType::Text => format!("{f}"),
        },
        Value::Decimal { value, scale } => {
            let f = *value as f64 / 10f64.powi(*scale as i32);
            match ty {
                ColType::Int => format_float_as_int(f),
                ColType::Real => format_float_as_real(f),
                ColType::Text => format_decimal_text(*value, *scale),
            }
        }
        Value::Text(s) => match ty {
            ColType::Int | ColType::Real => s.clone(),
            ColType::Text => {
                if s.is_empty() {
                    "(empty)".to_string()
                } else {
                    s.clone()
                }
            }
        },
        Value::Bytes(b) => format_bytes(b),
        Value::Timestamp(ns) => civil::format_timestamp_ns(*ns),
        Value::Date(d) => civil::format_date(*d as i64),
        Value::Uuid(bytes) => format_uuid(bytes),
        Value::Ip(addr) => addr.to_string(),
        Value::List(xs) => {
            let items: Vec<String> = xs.iter().map(|v| render_cell(v, ColType::Text)).collect();
            format!("[{}]", items.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_is_always_null() {
        for ty in [ColType::Int, ColType::Real, ColType::Text] {
            assert_eq!(render_cell(&Value::Null, ty), "NULL");
        }
    }

    #[test]
    fn bool_is_true_false_everywhere() {
        for ty in [ColType::Int, ColType::Real, ColType::Text] {
            assert_eq!(render_cell(&Value::Bool(true), ty), "true");
            assert_eq!(render_cell(&Value::Bool(false), ty), "false");
        }
    }

    #[test]
    fn int_renders_per_column() {
        assert_eq!(render_cell(&Value::Int(7), ColType::Int), "7");
        assert_eq!(render_cell(&Value::Int(7), ColType::Real), "7.000");
        assert_eq!(render_cell(&Value::Int(7), ColType::Text), "7");
    }

    #[test]
    fn float_renders_per_column() {
        assert_eq!(render_cell(&Value::Float(2.5), ColType::Int), "2");
        assert_eq!(render_cell(&Value::Float(2.5), ColType::Real), "2.500");
        assert_eq!(render_cell(&Value::Float(2.5), ColType::Text), "2.5");
    }

    #[test]
    fn negative_zero_real_is_positive() {
        assert_eq!(
            render_cell(&Value::Float(-0.0000001), ColType::Real),
            "0.000"
        );
    }

    #[test]
    fn non_finite_float_overrides_every_column() {
        for ty in [ColType::Int, ColType::Real, ColType::Text] {
            assert_eq!(render_cell(&Value::Float(f64::NAN), ty), "NaN");
            assert_eq!(render_cell(&Value::Float(f64::INFINITY), ty), "inf");
            assert_eq!(render_cell(&Value::Float(f64::NEG_INFINITY), ty), "-inf");
        }
    }

    #[test]
    fn text_renders_per_column() {
        assert_eq!(
            render_cell(&Value::Text("hi".to_string()), ColType::Int),
            "hi"
        );
        assert_eq!(
            render_cell(&Value::Text("hi".to_string()), ColType::Real),
            "hi"
        );
        assert_eq!(
            render_cell(&Value::Text("hi".to_string()), ColType::Text),
            "hi"
        );
    }

    #[test]
    fn empty_text_in_text_column() {
        assert_eq!(
            render_cell(&Value::Text(String::new()), ColType::Text),
            "(empty)"
        );
        assert_eq!(render_cell(&Value::Text(String::new()), ColType::Int), "");
    }

    #[test]
    fn row_joins_with_single_spaces() {
        let row = vec![Value::Int(1), Value::Text("a".to_string())];
        assert_eq!(render_row(&row, &[ColType::Int, ColType::Text]), "1 a");
    }

    /// The core invariant: a new variant renders, under every `ColType`, exactly what the
    /// `Value::Text`/`Value::Float` duck.rs produced for the same DuckDB value before this
    /// change. A mismatch here is a corpus-moving regression.
    #[test]
    fn new_variants_match_their_pre_change_rendering() {
        for ty in [ColType::Int, ColType::Real, ColType::Text] {
            assert_eq!(
                render_cell(&Value::Date(19724), ty),
                render_cell(&Value::Text("2024-01-02".to_string()), ty)
            );
            assert_eq!(
                render_cell(&Value::Timestamp(1_704_164_645_000_000_000), ty),
                render_cell(&Value::Text("2024-01-02 03:04:05".to_string()), ty)
            );
            assert_eq!(
                render_cell(&Value::Timestamp(1_704_164_645_500_000_000), ty),
                render_cell(&Value::Text("2024-01-02 03:04:05.500000".to_string()), ty)
            );
            assert_eq!(
                render_cell(&Value::UInt(7), ty),
                render_cell(&Value::Int(7), ty)
            );
            assert_eq!(
                render_cell(&Value::UInt(u64::MAX), ty),
                render_cell(&Value::Text(u64::MAX.to_string()), ty)
            );
            assert_eq!(
                render_cell(
                    &Value::Decimal {
                        value: 15,
                        scale: 1
                    },
                    ty
                ),
                render_cell(&Value::Float(1.5), ty)
            );
        }
        assert_eq!(
            render_cell(
                &Value::Decimal {
                    value: 150,
                    scale: 2
                },
                ColType::Text
            ),
            "1.5"
        );
    }

    #[test]
    fn timestamp_sub_microsecond_renders_nine_digits() {
        assert_eq!(
            render_cell(&Value::Timestamp(1_704_164_645_500_000_001), ColType::Text),
            "2024-01-02 03:04:05.500000001"
        );
    }

    #[test]
    fn timestamp_negative_one_ns_is_the_last_ns_of_1969() {
        assert_eq!(
            render_cell(&Value::Timestamp(-1), ColType::Text),
            "1969-12-31 23:59:59.999999999"
        );
    }

    #[test]
    fn decimal_text_strips_trailing_zeros_and_keeps_negative_fractions() {
        assert_eq!(
            render_cell(
                &Value::Decimal {
                    value: -5,
                    scale: 2
                },
                ColType::Text
            ),
            "-0.05"
        );
        assert_eq!(
            render_cell(&Value::Decimal { value: 0, scale: 2 }, ColType::Text),
            "0"
        );
    }

    #[test]
    fn bytes_render_as_lowercase_hex() {
        assert_eq!(
            render_cell(&Value::Bytes(vec![0xab, 1]), ColType::Text),
            "\\xab01"
        );
    }

    #[test]
    fn ip_addresses_render_via_display() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        assert_eq!(
            render_cell(
                &Value::Ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))),
                ColType::Text
            ),
            "127.0.0.1"
        );
        assert_eq!(
            render_cell(&Value::Ip(IpAddr::V6(Ipv6Addr::LOCALHOST)), ColType::Text),
            "::1"
        );
    }

    #[test]
    fn list_renders_elements_under_text_with_null_as_null() {
        let list = Value::List(vec![
            Value::Int(1),
            Value::Null,
            Value::Text("a".to_string()),
        ]);
        assert_eq!(render_cell(&list, ColType::Text), "[1, NULL, a]");
    }
}
