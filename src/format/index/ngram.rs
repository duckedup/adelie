//! NGRAM skip structure (SPEC §5, index kind 3): a BLOOM over every byte trigram of every
//! valid STRING value, answering substring queries rather than equality.

use crate::exec::Column;
use crate::types::{DataType, Value};

use super::super::error::DecodeError;
use super::bloom::Bloom;

/// A bloom over trigrams. `might_contain` (on `SkipIndex`) always answers true for this kind
/// — it isn't an equality index — so only `might_contain_substring` is real here.
#[derive(Debug, Clone, PartialEq)]
pub struct Ngram(Bloom);

impl Ngram {
    /// Trigrams are the raw 3 bytes of `value.as_bytes().windows(3)`, hashed directly (not
    /// through `value_bytes`): same distinct-then-insert shape as `Bloom::build`.
    pub(crate) fn build(col: &Column) -> Ngram {
        let mut keys: Vec<Vec<u8>> = Vec::new();
        for i in 0..col.len() {
            if col.is_null(i) {
                continue;
            }
            let Value::String(s) = col.get(i) else {
                unreachable!("Ngram only builds over STRING columns")
            };
            keys.extend(s.as_bytes().windows(3).map(|w| w.to_vec()));
        }
        keys.sort();
        keys.dedup();
        let mut bloom = Bloom::empty(DataType::String, keys.len());
        for k in &keys {
            bloom.insert_bytes(k);
        }
        Ngram(bloom)
    }

    /// A probe under 3 bytes can't be ruled out (there's nothing to hash), so it answers
    /// true; otherwise every trigram of `s` must hit the bloom.
    pub(crate) fn might_contain_substring(&self, s: &str) -> bool {
        let bytes = s.as_bytes();
        if bytes.len() < 3 {
            return true;
        }
        bytes.windows(3).all(|w| self.0.test_bytes(w))
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        self.0.encode()
    }

    pub(crate) fn load(blob: &[u8]) -> Result<Ngram, DecodeError> {
        Bloom::load(&DataType::String, blob).map(Ngram)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::test_support::SplitMix64;

    fn substrings_ge3(s: &str) -> Vec<String> {
        let b = s.as_bytes();
        let mut out = Vec::new();
        for start in 0..b.len() {
            for end in (start + 3)..=b.len() {
                out.push(String::from_utf8(b[start..end].to_vec()).unwrap());
            }
        }
        out
    }

    #[test]
    fn no_false_negatives_over_every_substring() {
        let n = if cfg!(miri) { 30 } else { 300 };
        let mut rng = SplitMix64::new(1);
        let values: Vec<Value> = (0..n)
            .map(|_| {
                let len = 3 + (rng.next_u64() % 10) as usize;
                let s: String = (0..len)
                    .map(|_| (b'a' + (rng.next_u64() % 4) as u8) as char)
                    .collect();
                Value::String(s)
            })
            .collect();
        let col = Column::from_values(&DataType::String, &values).unwrap();
        let ngram = Ngram::build(&col);
        for v in &values {
            let Value::String(s) = v else { unreachable!() };
            for sub in substrings_ge3(s) {
                assert!(ngram.might_contain_substring(&sub), "false negative for {sub:?}");
            }
        }
    }

    #[test]
    fn it_prunes() {
        let col =
            Column::from_values(&DataType::String, &[Value::String("checkout failed".to_string())])
                .unwrap();
        let ngram = Ngram::build(&col);
        assert!(!ngram.might_contain_substring("timeout"));
    }

    #[test]
    fn short_probe_always_matches() {
        let col =
            Column::from_values(&DataType::String, &[Value::String("abc".to_string())]).unwrap();
        let ngram = Ngram::build(&col);
        assert!(ngram.might_contain_substring(""));
        assert!(ngram.might_contain_substring("z"));
        assert!(ngram.might_contain_substring("zz"));
    }

    #[test]
    fn load_round_trips_a_built_ngram() {
        let col =
            Column::from_values(&DataType::String, &[Value::String("hello world".to_string())])
                .unwrap();
        let built = Ngram::build(&col);
        let blob = built.encode();
        let loaded = Ngram::load(&blob).unwrap();
        assert_eq!(built, loaded);
        assert!(loaded.might_contain_substring("hello"));
        assert!(!loaded.might_contain_substring("xyz123"));
    }

    #[test]
    fn truncated_blob_is_err() {
        assert_eq!(Ngram::load(&[7u8]), Err(DecodeError::Truncated));
    }

    #[test]
    fn random_blobs_never_panic() {
        let iters = if cfg!(miri) { 30 } else { 2000 };
        let mut rng = SplitMix64::new(99);
        for _ in 0..iters {
            let len = (rng.next_u64() % 64) as usize;
            let blob: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            let _ = Ngram::load(&blob);
        }
    }

    #[test]
    fn two_builds_are_byte_identical() {
        let col = Column::from_values(
            &DataType::String,
            &[Value::String("ab".to_string()), Value::String("abc".to_string())],
        )
        .unwrap();
        assert_eq!(Ngram::build(&col).encode(), Ngram::build(&col).encode());
    }
}
