//! Skip structures (SPEC §16.2): built per column and row group on request (Q11 is open).

mod bloom;
mod ngram;
mod set;

pub use bloom::Bloom;
pub use ngram::Ngram;
pub use set::ValueSet;

use crate::exec::Column;
use crate::types::{DataType, Value};

use super::error::DecodeError;

const KIND_BLOOM: u64 = 1;
const KIND_VALUE_SET: u64 = 2;
const KIND_NGRAM: u64 = 3;

/// Which skip structure an index entry holds (SPEC §5). Ids are pinned, never derived from
/// enum order, so a reader can skip a future kind it doesn't know instead of erroring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IndexKind {
    Bloom,
    ValueSet,
    Ngram,
}

impl IndexKind {
    pub fn id(self) -> u64 {
        match self {
            IndexKind::Bloom => KIND_BLOOM,
            IndexKind::ValueSet => KIND_VALUE_SET,
            IndexKind::Ngram => KIND_NGRAM,
        }
    }

    /// Any id besides the three known ones names a future kind: the reader drops it from
    /// `indexes()` rather than erroring (SPEC §5).
    pub fn from_id(id: u64) -> Option<IndexKind> {
        match id {
            KIND_BLOOM => Some(IndexKind::Bloom),
            KIND_VALUE_SET => Some(IndexKind::ValueSet),
            KIND_NGRAM => Some(IndexKind::Ngram),
            _ => None,
        }
    }

    /// BLOOM/VALUE_SET cover every type but LIST; NGRAM only ever indexes STRING.
    pub fn applies_to(self, ty: &DataType) -> bool {
        match self {
            IndexKind::Bloom | IndexKind::ValueSet => !matches!(ty, DataType::List(_)),
            IndexKind::Ngram => matches!(ty, DataType::String),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            IndexKind::Bloom => "BLOOM",
            IndexKind::ValueSet => "VALUE_SET",
            IndexKind::Ngram => "NGRAM",
        }
    }
}

/// A loaded skip structure, ready to probe (SPEC §5, §16.2).
#[derive(Debug, Clone, PartialEq)]
pub enum SkipIndex {
    Bloom(Bloom),
    ValueSet(ValueSet),
    Ngram(Ngram),
}

impl SkipIndex {
    pub fn kind(&self) -> IndexKind {
        match self {
            SkipIndex::Bloom(_) => IndexKind::Bloom,
            SkipIndex::ValueSet(_) => IndexKind::ValueSet,
            SkipIndex::Ngram(_) => IndexKind::Ngram,
        }
    }

    /// False only when no row can equal `v`. NGRAM answers substrings, not equality, so it
    /// always answers true here.
    pub fn might_contain(&self, v: &Value) -> bool {
        match self {
            SkipIndex::Bloom(b) => b.might_contain(v),
            SkipIndex::ValueSet(s) => s.might_contain(v),
            SkipIndex::Ngram(_) => true,
        }
    }

    /// False only when no row can contain `s`. BLOOM/VALUE_SET answer equality, not
    /// substrings, so they always answer true here.
    pub fn might_contain_substring(&self, s: &str) -> bool {
        match self {
            SkipIndex::Bloom(_) | SkipIndex::ValueSet(_) => true,
            SkipIndex::Ngram(n) => n.might_contain_substring(s),
        }
    }
}

/// A cheap tag match ahead of the exact `value_bytes` encode (which panics on a mismatch):
/// mirrors `value::encode_value`'s arms, shared by BLOOM and VALUE_SET probes.
fn value_matches(v: &Value, ty: &DataType) -> bool {
    matches!(
        (v, ty),
        (Value::Bool(_), DataType::Bool)
            | (Value::Int64(_), DataType::Int64)
            | (Value::UInt64(_), DataType::UInt64)
            | (Value::Float64(_), DataType::Float64)
            | (Value::Decimal(_), DataType::Decimal(_))
            | (Value::String(_), DataType::String)
            | (Value::Bytes(_), DataType::Bytes)
            | (Value::Timestamp(_), DataType::Timestamp)
            | (Value::Date(_), DataType::Date)
            | (Value::Uuid(_), DataType::Uuid)
            | (Value::Ip(_), DataType::Ip)
    )
}

/// Blob for `col` under `kind`, or `None` when not built: `kind` doesn't apply to the
/// column's type, or (VALUE_SET) it has more than `VALUE_SET_MAX` distinct values.
pub(crate) fn build(kind: IndexKind, col: &Column) -> Option<Vec<u8>> {
    if !kind.applies_to(col.data_type()) {
        return None;
    }
    match kind {
        IndexKind::Bloom => Some(Bloom::build(col).encode()),
        IndexKind::ValueSet => ValueSet::build(col).map(|s| s.encode()),
        IndexKind::Ngram => Some(Ngram::build(col).encode()),
    }
}

pub(crate) fn load(kind: IndexKind, ty: &DataType, blob: &[u8]) -> Result<SkipIndex, DecodeError> {
    match kind {
        IndexKind::Bloom => Bloom::load(ty, blob).map(SkipIndex::Bloom),
        IndexKind::ValueSet => ValueSet::load(ty, blob).map(SkipIndex::ValueSet),
        IndexKind::Ngram => Ngram::load(blob).map(SkipIndex::Ngram),
    }
}

/// A dependency-free, deterministic RNG (SplitMix64) shared by this module's inline tests.
#[cfg(test)]
pub(crate) mod test_support {
    pub struct SplitMix64(u64);

    impl SplitMix64 {
        pub fn new(seed: u64) -> Self {
            SplitMix64(seed)
        }

        pub fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
            z ^ (z >> 31)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_pinned_to_their_numbers() {
        assert_eq!(IndexKind::Bloom.id(), 1);
        assert_eq!(IndexKind::ValueSet.id(), 2);
        assert_eq!(IndexKind::Ngram.id(), 3);
        assert_eq!(IndexKind::from_id(1), Some(IndexKind::Bloom));
        assert_eq!(IndexKind::from_id(2), Some(IndexKind::ValueSet));
        assert_eq!(IndexKind::from_id(3), Some(IndexKind::Ngram));
        assert_eq!(IndexKind::from_id(999), None);
    }

    #[test]
    fn applies_to_matches_the_type_table() {
        let list_ty = DataType::list(DataType::Int64).unwrap();
        assert!(IndexKind::Bloom.applies_to(&DataType::Int64));
        assert!(!IndexKind::Bloom.applies_to(&list_ty));
        assert!(IndexKind::ValueSet.applies_to(&DataType::String));
        assert!(!IndexKind::ValueSet.applies_to(&list_ty));
        assert!(IndexKind::Ngram.applies_to(&DataType::String));
        assert!(!IndexKind::Ngram.applies_to(&DataType::Int64));
    }

    #[test]
    fn build_value_set_over_257_distinct_is_none() {
        let values: Vec<Value> = (0..257i64).map(Value::Int64).collect();
        let col = Column::from_values(&DataType::Int64, &values).unwrap();
        assert_eq!(build(IndexKind::ValueSet, &col), None);
    }

    #[test]
    fn build_ngram_over_int64_column_is_none() {
        let col = Column::from_values(&DataType::Int64, &[Value::Int64(1)]).unwrap();
        assert_eq!(build(IndexKind::Ngram, &col), None);
    }

    #[test]
    fn build_bloom_over_list_column_is_none() {
        let list_ty = DataType::list(DataType::Int64).unwrap();
        let col = Column::from_values(&list_ty, &[]).unwrap();
        assert_eq!(build(IndexKind::Bloom, &col), None);
    }

    #[test]
    fn two_builds_over_the_same_column_are_byte_identical() {
        let values: Vec<Value> = vec![
            Value::String("alpha".to_string()),
            Value::String("beta".to_string()),
        ];
        let col = Column::from_values(&DataType::String, &values).unwrap();
        for kind in [IndexKind::Bloom, IndexKind::ValueSet, IndexKind::Ngram] {
            assert_eq!(build(kind, &col), build(kind, &col));
        }
    }

    #[test]
    fn load_round_trips_every_kind() {
        let values: Vec<Value> = vec![
            Value::String("hello".to_string()),
            Value::String("world".to_string()),
        ];
        let col = Column::from_values(&DataType::String, &values).unwrap();
        for kind in [IndexKind::Bloom, IndexKind::ValueSet, IndexKind::Ngram] {
            let blob = build(kind, &col).expect("all three kinds apply to STRING");
            let idx = load(kind, &DataType::String, &blob).unwrap();
            assert_eq!(idx.kind(), kind);
            assert!(idx.might_contain(&Value::String("hello".to_string())));
        }
    }
}
