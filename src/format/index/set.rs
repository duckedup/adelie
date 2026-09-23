//! VALUE_SET skip structure (SPEC §5, index kind 2): the exact set of a column's distinct
//! non-null values, kept only when there are few enough to be worth storing.

use std::cmp::Ordering;

use crate::exec::Column;
use crate::types::{DataType, Value, total_cmp};

use super::super::VALUE_SET_MAX;
use super::super::error::DecodeError;
use super::super::value::{decode_value, encode_value};
use super::super::wire::{Cursor, Sink};
use super::value_matches;

/// A sorted, deduplicated set of a column's distinct non-null values (SPEC §16.2).
#[derive(Debug, Clone, PartialEq)]
pub struct ValueSet {
    ty: DataType,
    values: Vec<Value>,
}

impl ValueSet {
    /// `None` when there are more than `VALUE_SET_MAX` distinct non-null values: not worth
    /// storing. Dedupe is by `total_cmp == Equal`, so `1.0`/`1.0` collapse and both NaN
    /// payloads become one value.
    pub(crate) fn build(col: &Column) -> Option<ValueSet> {
        let ty = col.data_type().clone();
        let mut values: Vec<Value> = (0..col.len())
            .filter(|&i| !col.is_null(i))
            .map(|i| col.get(i))
            .collect();
        values.sort_by(|a, b| total_cmp(a, b).expect("same-type values always compare"));
        values.dedup_by(|a, b| total_cmp(a, b) == Some(Ordering::Equal));
        if values.len() > VALUE_SET_MAX {
            return None;
        }
        Some(ValueSet { ty, values })
    }

    /// A binary search by `total_cmp`: a `DECIMAL` probe at a different scale still finds a
    /// stored value at the same numeric value, since `total_cmp` compares across scales.
    pub(crate) fn might_contain(&self, v: &Value) -> bool {
        if v.is_null() || !value_matches(v, &self.ty) {
            return true;
        }
        self.values
            .binary_search_by(|probe| total_cmp(probe, v).expect("same-type values always compare"))
            .is_ok()
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut s = Sink::new();
        s.uvarint(self.values.len() as u64);
        for v in &self.values {
            encode_value(v, &self.ty, &mut s);
        }
        s.into_vec()
    }

    /// The values must be strictly increasing under `total_cmp`, or the blob is `Malformed`
    /// (a builder never writes a tie or an out-of-order pair).
    pub(crate) fn load(ty: &DataType, blob: &[u8]) -> Result<ValueSet, DecodeError> {
        let mut c = Cursor::new(blob);
        let n = c.uvarint()?;
        let n = c.guard_len(n, 1)?;
        let mut values = Vec::with_capacity(n);
        for _ in 0..n {
            values.push(decode_value(&mut c, ty)?);
        }
        if !c.is_empty() {
            return Err(DecodeError::Malformed("value set blob has trailing bytes"));
        }
        for pair in values.windows(2) {
            if total_cmp(&pair[0], &pair[1]) != Some(Ordering::Less) {
                return Err(DecodeError::Malformed("value set is not strictly increasing"));
            }
        }
        Ok(ValueSet { ty: ty.clone(), values })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::test_support::SplitMix64;
    use crate::types::Decimal;

    const N: usize = if cfg!(miri) { 100 } else { 5000 };

    fn no_false_negatives(ty: DataType, values: Vec<Value>) {
        let col = Column::from_values(&ty, &values).unwrap();
        let set = ValueSet::build(&col).expect("distinct count within VALUE_SET_MAX");
        for v in &values {
            assert!(set.might_contain(v), "false negative for {v:?}");
        }
    }

    #[test]
    fn no_false_negatives_int64() {
        let mut rng = SplitMix64::new(1);
        let values: Vec<Value> = (0..N)
            .map(|_| Value::Int64((rng.next_u64() % 200) as i64))
            .collect();
        no_false_negatives(DataType::Int64, values);
    }

    #[test]
    fn no_false_negatives_string() {
        let mut rng = SplitMix64::new(2);
        let values: Vec<Value> = (0..N)
            .map(|_| Value::String(format!("s{}", rng.next_u64() % 200)))
            .collect();
        no_false_negatives(DataType::String, values);
    }

    #[test]
    fn no_false_negatives_uuid() {
        let mut rng = SplitMix64::new(3);
        let values: Vec<Value> = (0..N)
            .map(|_| {
                let b = (rng.next_u64() % 200) as u8;
                Value::Uuid([b; 16])
            })
            .collect();
        no_false_negatives(DataType::Uuid, values);
    }

    #[test]
    fn no_false_negatives_float_with_nan() {
        let mut rng = SplitMix64::new(4);
        let values: Vec<Value> = (0..N)
            .map(|i| {
                if i % 50 == 0 {
                    Value::Float64(f64::NAN)
                } else {
                    Value::Float64((rng.next_u64() % 200) as f64)
                }
            })
            .collect();
        no_false_negatives(DataType::Float64, values);
    }

    #[test]
    fn no_false_negatives_decimal() {
        let mut rng = SplitMix64::new(5);
        let ty = DataType::decimal(10, 2).unwrap();
        let values: Vec<Value> = (0..N)
            .map(|_| Value::Decimal(Decimal::new((rng.next_u64() % 200) as i128, 2).unwrap()))
            .collect();
        no_false_negatives(ty, values);
    }

    #[test]
    fn it_prunes() {
        let col = Column::from_values(
            &DataType::Int64,
            &[Value::Int64(1), Value::Int64(2), Value::Int64(3)],
        )
        .unwrap();
        let set = ValueSet::build(&col).unwrap();
        assert!(!set.might_contain(&Value::Int64(4)));
    }

    #[test]
    fn decimal_probe_at_a_different_scale_still_matches() {
        let col = Column::from_values(
            &DataType::decimal(10, 1).unwrap(),
            &[Value::Decimal(Decimal::new(15, 1).unwrap())],
        )
        .unwrap();
        let set = ValueSet::build(&col).unwrap();
        let probe = Value::Decimal(Decimal::new(150, 2).unwrap());
        assert!(set.might_contain(&probe));
    }

    #[test]
    fn wrong_type_probe_returns_true() {
        let col = Column::from_values(&DataType::Int64, &[Value::Int64(1)]).unwrap();
        let set = ValueSet::build(&col).unwrap();
        assert!(set.might_contain(&Value::String("x".to_string())));
    }

    #[test]
    fn null_probe_returns_true() {
        let col = Column::from_values(&DataType::Int64, &[Value::Int64(1)]).unwrap();
        let set = ValueSet::build(&col).unwrap();
        assert!(set.might_contain(&Value::Null));
    }

    #[test]
    fn over_max_distinct_returns_none() {
        let values: Vec<Value> = (0..(VALUE_SET_MAX as i64 + 1)).map(Value::Int64).collect();
        let col = Column::from_values(&DataType::Int64, &values).unwrap();
        assert_eq!(ValueSet::build(&col), None);
    }

    #[test]
    fn load_round_trips_a_built_set() {
        let values: Vec<Value> = vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)];
        let col = Column::from_values(&DataType::Int64, &values).unwrap();
        let built = ValueSet::build(&col).unwrap();
        let blob = built.encode();
        let loaded = ValueSet::load(&DataType::Int64, &blob).unwrap();
        assert_eq!(built, loaded);
        for v in &values {
            assert!(loaded.might_contain(v));
        }
        assert!(!loaded.might_contain(&Value::Int64(4)));
    }

    #[test]
    fn truncated_blob_is_err() {
        let blob = [3u8]; // claims 3 values, none follow
        assert_eq!(
            ValueSet::load(&DataType::Int64, &blob),
            Err(DecodeError::Truncated)
        );
    }

    #[test]
    fn unsorted_blob_is_malformed() {
        let mut s = Sink::new();
        s.uvarint(2);
        encode_value(&Value::Int64(2), &DataType::Int64, &mut s);
        encode_value(&Value::Int64(1), &DataType::Int64, &mut s);
        let blob = s.into_vec();
        assert!(matches!(
            ValueSet::load(&DataType::Int64, &blob),
            Err(DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn duplicate_valued_blob_is_malformed() {
        let mut s = Sink::new();
        s.uvarint(2);
        encode_value(&Value::Int64(1), &DataType::Int64, &mut s);
        encode_value(&Value::Int64(1), &DataType::Int64, &mut s);
        let blob = s.into_vec();
        assert!(matches!(
            ValueSet::load(&DataType::Int64, &blob),
            Err(DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn random_blobs_never_panic() {
        let iters = if cfg!(miri) { 30 } else { 2000 };
        let mut rng = SplitMix64::new(99);
        for _ in 0..iters {
            let len = (rng.next_u64() % 64) as usize;
            let blob: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            let _ = ValueSet::load(&DataType::Int64, &blob);
        }
    }

    #[test]
    fn two_builds_are_byte_identical() {
        let values: Vec<Value> = vec![Value::Int64(3), Value::Int64(1), Value::Int64(2)];
        let col = Column::from_values(&DataType::Int64, &values).unwrap();
        assert_eq!(
            ValueSet::build(&col).unwrap().encode(),
            ValueSet::build(&col).unwrap().encode()
        );
    }
}
