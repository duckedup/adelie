//! Group and join row keys (SPEC §7). FROZEN encoding: `kernels::hash::stable_hash` (and the
//! HLL states it feeds) depend on these exact bytes, forever.

use crate::exec::Column;
use crate::types::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NullKeys {
    /// GROUP BY: NULL is a value (NULLs group together), and so is every NaN.
    Group,
    /// A join key: a NULL or NaN never matches (`sql_eq`), so encoding stops and reports it.
    Skip,
}

/// Appends row `row`'s key over `cols` to `out`; `Skip` returns `false` (`out` unspecified)
/// on a NULL/NaN key. FROZEN: tag `0`=NULL else `1`+little-endian value (FLOAT64 normalises
/// −0.0→0.0/NaN→`f64::NAN` first); STRING/BYTES get a `u32` length prefix, LIST a count.
pub(crate) fn encode_row_key(
    cols: &[&Column],
    row: usize,
    nulls: NullKeys,
    out: &mut Vec<u8>,
) -> bool {
    for col in cols {
        if !encode_value(&col.get(row), nulls, out) {
            return false;
        }
    }
    true
}

fn encode_value(v: &Value, nulls: NullKeys, out: &mut Vec<u8>) -> bool {
    match v {
        Value::Null => {
            if nulls == NullKeys::Skip {
                return false;
            }
            out.push(0);
            true
        }
        Value::Bool(b) => {
            out.push(1);
            out.push(*b as u8);
            true
        }
        Value::Int64(n) => {
            out.push(1);
            out.extend_from_slice(&n.to_le_bytes());
            true
        }
        Value::UInt64(n) => {
            out.push(1);
            out.extend_from_slice(&n.to_le_bytes());
            true
        }
        Value::Float64(f) => {
            let normalized = if *f == 0.0 {
                0.0
            } else if f.is_nan() {
                f64::NAN
            } else {
                *f
            };
            if normalized.is_nan() && nulls == NullKeys::Skip {
                return false;
            }
            out.push(1);
            out.extend_from_slice(&normalized.to_bits().to_le_bytes());
            true
        }
        Value::Decimal(d) => {
            // Every value in one physical column shares one scale, so the unscaled i128 alone
            // (no scale byte) already makes equal values byte-equal within that column.
            out.push(1);
            out.extend_from_slice(&d.unscaled().to_le_bytes());
            true
        }
        Value::String(s) => {
            out.push(1);
            encode_bytes(s.as_bytes(), out);
            true
        }
        Value::Bytes(b) => {
            out.push(1);
            encode_bytes(b, out);
            true
        }
        Value::Timestamp(n) => {
            out.push(1);
            out.extend_from_slice(&n.to_le_bytes());
            true
        }
        Value::Date(n) => {
            out.push(1);
            out.extend_from_slice(&n.to_le_bytes());
            true
        }
        Value::Uuid(bytes) => {
            out.push(1);
            out.extend_from_slice(bytes);
            true
        }
        Value::Ip(ip) => {
            out.push(1);
            out.extend_from_slice(&ip.octets());
            true
        }
        Value::List(items) => {
            out.push(1);
            out.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for item in items {
                if !encode_value(item, nulls, out) {
                    return false;
                }
            }
            true
        }
    }
}

fn encode_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DataType;

    fn key(cols: &[&Column], row: usize, nulls: NullKeys) -> (bool, Vec<u8>) {
        let mut out = Vec::new();
        let ok = encode_row_key(cols, row, nulls, &mut out);
        (ok, out)
    }

    #[test]
    fn equal_int_keys_give_equal_bytes() {
        let values = [Value::Int64(5), Value::Int64(5)];
        let col = Column::from_values(&DataType::Int64, &values).unwrap();
        let (_, a) = key(&[&col], 0, NullKeys::Group);
        let (_, b) = key(&[&col], 1, NullKeys::Group);
        assert_eq!(a, b);
    }

    #[test]
    fn signed_zero_and_nan_collapse_under_group() {
        let col = Column::from_values(
            &DataType::Float64,
            &[
                Value::Float64(0.0),
                Value::Float64(-0.0),
                Value::Float64(f64::NAN),
                Value::Float64(f64::NAN),
            ],
        )
        .unwrap();
        let (_, z1) = key(&[&col], 0, NullKeys::Group);
        let (_, z2) = key(&[&col], 1, NullKeys::Group);
        assert_eq!(z1, z2);
        let (ok1, n1) = key(&[&col], 2, NullKeys::Group);
        let (ok2, n2) = key(&[&col], 3, NullKeys::Group);
        assert!(ok1 && ok2);
        assert_eq!(n1, n2);
        assert_ne!(z1, n1);
    }

    #[test]
    fn nulls_are_equal_under_group_and_false_under_skip() {
        let col = Column::from_values(&DataType::Int64, &[Value::Null, Value::Null]).unwrap();
        let (ok_a, a) = key(&[&col], 0, NullKeys::Group);
        let (ok_b, b) = key(&[&col], 1, NullKeys::Group);
        assert!(ok_a && ok_b);
        assert_eq!(a, b);
        let (ok, _) = key(&[&col], 0, NullKeys::Skip);
        assert!(!ok);
    }

    /// Falsify: this fails if the STRING length prefix is dropped (both encode to `"abc"`).
    #[test]
    fn string_length_prefix_prevents_boundary_collision() {
        let a = Column::from_values(&DataType::String, &[Value::String("a".into())]).unwrap();
        let bc = Column::from_values(&DataType::String, &[Value::String("bc".into())]).unwrap();
        let ab = Column::from_values(&DataType::String, &[Value::String("ab".into())]).unwrap();
        let c = Column::from_values(&DataType::String, &[Value::String("c".into())]).unwrap();
        let (_, k1) = key(&[&a, &bc], 0, NullKeys::Group);
        let (_, k2) = key(&[&ab, &c], 0, NullKeys::Group);
        assert_ne!(k1, k2);
    }
}
