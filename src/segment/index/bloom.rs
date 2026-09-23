//! BLOOM skip structure (SPEC §5, index kind 1): a fixed-`k`, power-of-two bit array probed
//! by two xxh64 hashes. `Ngram` reuses the same bit array over raw trigram bytes.

use crate::exec::Column;
use crate::types::{DataType, Value};

use super::super::error::DecodeError;
use super::super::hash::xxh64;
use super::super::value::value_bytes;
use super::super::wire::{Cursor, Sink};
use super::value_matches;

const K: u8 = 7;
const MIN_LOG2_BITS: u32 = 9; // 512 bits
const MAX_LOG2_BITS: u32 = 20;

/// A `k`-probe bloom filter over one column's `value_bytes`, or (via `Ngram`) over raw
/// trigram bytes. `ty` is unused by the trigram path, which never calls `might_contain`.
#[derive(Debug, Clone, PartialEq)]
pub struct Bloom {
    k: u8,
    log2_bits: u8,
    words: Vec<u64>,
    ty: DataType,
}

impl Bloom {
    fn bits(&self) -> u64 {
        1u64 << self.log2_bits
    }

    /// The `k` bit positions probe `i` sets or tests: `(h1 + i*h2) mod bits`, `h2` forced odd
    /// so every probe stays reachable regardless of `bits`' power of two.
    fn probe(bytes: &[u8], k: u8, bits: u64) -> impl Iterator<Item = u64> {
        let h1 = xxh64(bytes, 0);
        let h2 = xxh64(bytes, 1) | 1;
        (0..k as u64).map(move |i| h1.wrapping_add(i.wrapping_mul(h2)) & (bits - 1))
    }

    pub(crate) fn insert_bytes(&mut self, bytes: &[u8]) {
        let bits = self.bits();
        for b in Self::probe(bytes, self.k, bits) {
            self.words[(b / 64) as usize] |= 1 << (b % 64);
        }
    }

    pub(crate) fn test_bytes(&self, bytes: &[u8]) -> bool {
        let bits = self.bits();
        Self::probe(bytes, self.k, bits)
            .all(|b| self.words[(b / 64) as usize] & (1 << (b % 64)) != 0)
    }

    /// `next_pow2(max(512, 10 * distinct))`, capped at `2^20` (SPEC §16.2).
    fn size_log2(distinct: usize) -> u8 {
        let want = 10u64
            .saturating_mul(distinct as u64)
            .max(1u64 << MIN_LOG2_BITS);
        let mut log2 = MIN_LOG2_BITS;
        while (1u64 << log2) < want && log2 < MAX_LOG2_BITS {
            log2 += 1;
        }
        log2 as u8
    }

    /// An empty, all-zero bloom sized for `distinct` keys, ready for `insert_bytes`. The
    /// shared path both `Bloom::build` (typed values) and `Ngram::build` (raw trigrams) use.
    pub(crate) fn empty(ty: DataType, distinct: usize) -> Bloom {
        let log2_bits = Self::size_log2(distinct);
        Bloom {
            k: K,
            log2_bits,
            words: vec![0u64; (1usize << log2_bits) / 64],
            ty,
        }
    }

    /// Sized from the distinct valid-value count, counted via a sorted `value_bytes` list
    /// (never a `HashSet`), so the result never depends on hash iteration order.
    pub(crate) fn build(col: &Column) -> Bloom {
        let ty = col.data_type().clone();
        let mut keys: Vec<Vec<u8>> = (0..col.len())
            .filter(|&i| !col.is_null(i))
            .map(|i| value_bytes(&col.get(i), &ty))
            .collect();
        keys.sort();
        keys.dedup();
        let mut bloom = Bloom::empty(ty, keys.len());
        for k in &keys {
            bloom.insert_bytes(k);
        }
        bloom
    }

    /// `Value::Null` and a value of the wrong type both return true: neither can be ruled
    /// out (null is never indexed; a mismatched type was never encoded into this bloom).
    pub(crate) fn might_contain(&self, v: &Value) -> bool {
        if v.is_null() || !value_matches(v, &self.ty) {
            return true;
        }
        self.test_bytes(&value_bytes(v, &self.ty))
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut s = Sink::new();
        s.u8(self.k);
        s.u8(self.log2_bits);
        for w in &self.words {
            s.u64(*w);
        }
        s.into_vec()
    }

    /// `k` must be in 1..=16, `log2_bits` in 9..=20, and the word count must match exactly
    /// (no bytes left over once the words are read).
    pub(crate) fn load(ty: &DataType, blob: &[u8]) -> Result<Bloom, DecodeError> {
        let mut c = Cursor::new(blob);
        let k = c.u8()?;
        if !(1..=16).contains(&k) {
            return Err(DecodeError::Malformed("bloom k out of range"));
        }
        let log2_bits = c.u8()?;
        if !(9..=20).contains(&log2_bits) {
            return Err(DecodeError::Malformed("bloom log2_bits out of range"));
        }
        let nwords = c.guard_len((1u64 << log2_bits) / 64, 8)?;
        let mut words = Vec::with_capacity(nwords);
        for _ in 0..nwords {
            words.push(c.u64()?);
        }
        if !c.is_empty() {
            return Err(DecodeError::Malformed("bloom blob has trailing bytes"));
        }
        Ok(Bloom {
            k,
            log2_bits,
            words,
            ty: ty.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::SplitMix64;
    use super::*;
    use crate::types::Decimal;

    const N: usize = if cfg!(miri) { 200 } else { 5000 };

    fn random_int64(rng: &mut SplitMix64) -> Value {
        Value::Int64(rng.next_u64() as i64)
    }

    fn random_string(rng: &mut SplitMix64) -> Value {
        let len = 1 + (rng.next_u64() % 12) as usize;
        let s: String = (0..len)
            .map(|_| (b'a' + (rng.next_u64() % 26) as u8) as char)
            .collect();
        Value::String(s)
    }

    fn random_uuid(rng: &mut SplitMix64) -> Value {
        let mut bytes = [0u8; 16];
        for chunk in bytes.chunks_mut(8) {
            chunk.copy_from_slice(&rng.next_u64().to_le_bytes());
        }
        Value::Uuid(bytes)
    }

    fn random_float(rng: &mut SplitMix64, i: usize) -> Value {
        if i % 97 == 0 {
            return Value::Float64(f64::NAN);
        }
        Value::Float64((rng.next_u64() as i64) as f64 / 1000.0)
    }

    fn random_decimal(rng: &mut SplitMix64) -> Value {
        let unscaled = (rng.next_u64() % 1_000_000_000) as i128;
        Value::Decimal(Decimal::new(unscaled, 6).unwrap())
    }

    fn no_false_negatives(ty: DataType, values: Vec<Value>) {
        let col = Column::from_values(&ty, &values).unwrap();
        let bloom = Bloom::build(&col);
        for v in &values {
            assert!(bloom.might_contain(v), "false negative for {v:?}");
        }
    }

    #[test]
    fn no_false_negatives_int64() {
        let mut rng = SplitMix64::new(1);
        no_false_negatives(
            DataType::Int64,
            (0..N).map(|_| random_int64(&mut rng)).collect(),
        );
    }

    #[test]
    fn no_false_negatives_string() {
        let mut rng = SplitMix64::new(2);
        no_false_negatives(
            DataType::String,
            (0..N).map(|_| random_string(&mut rng)).collect(),
        );
    }

    #[test]
    fn no_false_negatives_uuid() {
        let mut rng = SplitMix64::new(3);
        no_false_negatives(
            DataType::Uuid,
            (0..N).map(|_| random_uuid(&mut rng)).collect(),
        );
    }

    #[test]
    fn no_false_negatives_float_with_nan() {
        let mut rng = SplitMix64::new(4);
        no_false_negatives(
            DataType::Float64,
            (0..N).map(|i| random_float(&mut rng, i)).collect(),
        );
    }

    #[test]
    fn no_false_negatives_decimal() {
        let mut rng = SplitMix64::new(5);
        let ty = DataType::decimal(15, 6).unwrap();
        no_false_negatives(ty, (0..N).map(|_| random_decimal(&mut rng)).collect());
    }

    #[test]
    fn it_prunes_int64() {
        let inserted: Vec<Value> = (0..1000i64).map(Value::Int64).collect();
        let col = Column::from_values(&DataType::Int64, &inserted).unwrap();
        let bloom = Bloom::build(&col);
        let false_count = (1_000_000i64..1_001_000)
            .filter(|&i| !bloom.might_contain(&Value::Int64(i)))
            .count();
        assert!(false_count >= 950, "only {false_count}/1000 pruned");
    }

    #[test]
    fn wrong_type_probe_returns_true() {
        let col = Column::from_values(&DataType::Int64, &[Value::Int64(1)]).unwrap();
        let bloom = Bloom::build(&col);
        assert!(bloom.might_contain(&Value::String("x".to_string())));
    }

    #[test]
    fn null_probe_returns_true() {
        let col = Column::from_values(&DataType::Int64, &[Value::Int64(1)]).unwrap();
        let bloom = Bloom::build(&col);
        assert!(bloom.might_contain(&Value::Null));
    }

    #[test]
    fn empty_column_builds_a_minimum_size_bloom() {
        let col = Column::from_values(&DataType::Int64, &[]).unwrap();
        let bloom = Bloom::build(&col);
        assert_eq!(bloom.log2_bits, MIN_LOG2_BITS as u8);
        assert!(bloom.words.iter().all(|w| *w == 0));
        assert!(!bloom.might_contain(&Value::Int64(0)));
    }

    #[test]
    fn load_round_trips_a_built_bloom() {
        let values: Vec<Value> = (0..50i64).map(Value::Int64).collect();
        let col = Column::from_values(&DataType::Int64, &values).unwrap();
        let built = Bloom::build(&col);
        let blob = built.encode();
        let loaded = Bloom::load(&DataType::Int64, &blob).unwrap();
        assert_eq!(built, loaded);
        for v in &values {
            assert!(loaded.might_contain(v));
        }
    }

    #[test]
    fn truncated_blob_is_err() {
        let blob = [K, MIN_LOG2_BITS as u8];
        assert_eq!(
            Bloom::load(&DataType::Int64, &blob),
            Err(DecodeError::Truncated)
        );
    }

    #[test]
    fn bad_k_is_malformed() {
        let mut blob = vec![0u8, MIN_LOG2_BITS as u8];
        blob.extend(std::iter::repeat_n(
            0u8,
            8 * ((1usize << MIN_LOG2_BITS) / 64),
        ));
        assert!(matches!(
            Bloom::load(&DataType::Int64, &blob),
            Err(DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn bad_log2_bits_is_malformed() {
        let blob = [K, 8u8];
        assert!(matches!(
            Bloom::load(&DataType::Int64, &blob),
            Err(DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn trailing_bytes_are_malformed() {
        let mut blob = vec![K, MIN_LOG2_BITS as u8];
        blob.extend(std::iter::repeat_n(
            0u8,
            8 * ((1usize << MIN_LOG2_BITS) / 64) + 1,
        ));
        assert!(matches!(
            Bloom::load(&DataType::Int64, &blob),
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
            let _ = Bloom::load(&DataType::Int64, &blob);
        }
    }

    #[test]
    fn two_builds_are_byte_identical() {
        let values: Vec<Value> = vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
            Value::String("a".to_string()),
        ];
        let col = Column::from_values(&DataType::String, &values).unwrap();
        assert_eq!(Bloom::build(&col).encode(), Bloom::build(&col).encode());
    }
}
