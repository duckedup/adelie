//! Stable multi-key sort (SPEC §7): non-null values order by `total_cmp`, NULL placement
//! follows `nulls_first` alone (`storage/engines/sort.rs`'s `compare_one`, generalised).

use std::cmp::Ordering;

use crate::exec::Column;
use crate::types::total_cmp;

/// One ORDER BY key: `column` indexes the columns being sorted over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortKey {
    pub column: usize,
    pub descending: bool,
    pub nulls_first: bool,
}

impl SortKey {
    pub fn asc(column: usize) -> SortKey {
        SortKey {
            column,
            descending: false,
            nulls_first: false,
        }
    }

    pub fn desc(column: usize) -> SortKey {
        SortKey {
            column,
            descending: true,
            nulls_first: false,
        }
    }
}

/// Stable argsort of rows `0..cols[0].len()`.
pub(crate) fn sort_indices(cols: &[&Column], keys: &[SortKey]) -> Vec<u32> {
    let n = cols[0].len();
    let mut idx: Vec<u32> = (0..n as u32).collect();
    idx.sort_by(|&a, &b| compare_rows(cols, a as usize, cols, b as usize, keys));
    idx
}

/// Row `ar` of `a` vs row `br` of `b`, over the same `keys` (each `key.column` indexes both
/// `a` and `b`). NULL placement follows `nulls_first` alone, independent of `descending`.
pub(crate) fn compare_rows(
    a: &[&Column],
    ar: usize,
    b: &[&Column],
    br: usize,
    keys: &[SortKey],
) -> Ordering {
    for key in keys {
        let ca = a[key.column];
        let cb = b[key.column];
        let (an, bn) = (ca.is_null(ar), cb.is_null(br));
        let ord = match (an, bn) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if key.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if key.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, false) => {
                let cmp = total_cmp(&ca.get(ar), &cb.get(br))
                    .expect("same-type non-null values always compare");
                if key.descending { cmp.reverse() } else { cmp }
            }
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DataType, Value};

    fn col(vals: &[Option<i64>]) -> Column {
        let values: Vec<Value> = vals
            .iter()
            .map(|v| v.map_or(Value::Null, Value::Int64))
            .collect();
        Column::from_values(&DataType::Int64, &values).unwrap()
    }

    #[test]
    fn desc_puts_null_last() {
        let c = col(&[Some(1), None, Some(2)]);
        let cols = [&c];
        assert_eq!(sort_indices(&cols, &[SortKey::desc(0)]), vec![2, 0, 1]);
    }

    #[test]
    fn asc_puts_null_last_too() {
        let c = col(&[Some(1), None, Some(2)]);
        let cols = [&c];
        assert_eq!(sort_indices(&cols, &[SortKey::asc(0)]), vec![0, 2, 1]);
    }

    #[test]
    fn nulls_first_both_directions() {
        let c = col(&[Some(1), None, Some(2)]);
        let cols = [&c];
        let asc_nf = sort_indices(
            &cols,
            &[SortKey {
                column: 0,
                descending: false,
                nulls_first: true,
            }],
        );
        assert_eq!(asc_nf, vec![1, 0, 2]);
        let desc_nf = sort_indices(
            &cols,
            &[SortKey {
                column: 0,
                descending: true,
                nulls_first: true,
            }],
        );
        assert_eq!(desc_nf, vec![1, 2, 0]);
    }

    #[test]
    fn multi_key_breaks_ties_with_second_column() {
        let a = col(&[Some(1), Some(1), Some(0)]);
        let b = col(&[Some(2), Some(1), Some(5)]);
        let cols = [&a, &b];
        let idx = sort_indices(&cols, &[SortKey::asc(0), SortKey::asc(1)]);
        assert_eq!(idx, vec![2, 1, 0]);
    }

    #[test]
    fn stable_on_equal_keys() {
        let a = col(&[Some(1), Some(1), Some(1)]);
        let cols = [&a];
        assert_eq!(sort_indices(&cols, &[SortKey::asc(0)]), vec![0, 1, 2]);
    }
}
