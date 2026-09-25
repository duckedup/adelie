//! `stable_hash`: XXH64 over `rowkey::encode_row_key`'s bytes (SPEC §7; D0015). Frozen
//! forever — HLL states persist it.

use crate::exec::Column;
use crate::storage::segment::hash::xxh64;

use super::rowkey::{NullKeys, encode_row_key};

pub(crate) fn stable_hash(col: &Column, row: usize) -> u64 {
    let mut bytes = Vec::new();
    encode_row_key(&[col], row, NullKeys::Group, &mut bytes);
    xxh64(&bytes, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DataType, Value};

    /// The frozen-hash guard: computed once from `encode_row_key`'s pinned layout and
    /// `xxh64`'s known-answer tests. A change here means the encoding silently changed.
    #[test]
    fn stable_hash_of_a_fixed_input_is_pinned() {
        let col = Column::from_values(&DataType::Int64, &[Value::Int64(42)]).unwrap();
        assert_eq!(stable_hash(&col, 0), 0xee53_6f90_35b7_1091);
    }
}
