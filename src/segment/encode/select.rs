//! Encoding selection (SPEC §5, D0008): sample small chunks whole, larger ones at four
//! offsets; the candidate that encodes smallest wins, lower id breaking a tie.

use crate::exec::Column;

use super::Encoding;

pub(super) fn choose(col: &Column) -> Encoding {
    let sample = build_sample(col);
    let mut best: Option<(Encoding, usize)> = None;
    for enc in candidates(col) {
        let size = super::encode_with(&sample, enc).len();
        if best.is_none_or(|(_, best_size)| size < best_size) {
            best = Some((enc, size));
        }
    }
    best.map(|(enc, _)| enc).unwrap_or(Encoding::Plain)
}

/// Every id-0..=6 encoding that applies to `col`'s type, in id order (PLAIN first).
fn candidates(col: &Column) -> Vec<Encoding> {
    (0..=6u64)
        .filter_map(Encoding::from_id)
        .filter(|e| e.applies_to(col.data_type()))
        .collect()
}

/// The whole column when it fits in one sample; otherwise four 1024-row windows at 0, r/4,
/// r/2 and 3r/4, clamped to the rows that exist.
fn build_sample(col: &Column) -> Column {
    let rows = col.len();
    if rows <= super::SAMPLE_ROWS {
        return col.slice(0, rows);
    }
    let starts = [0, rows / 4, rows / 2, 3 * rows / 4];
    let pieces: Vec<Column> = starts
        .iter()
        .filter_map(|&s| {
            let len = 1024usize.min(rows.saturating_sub(s));
            (len > 0).then(|| col.slice(s, len))
        })
        .collect();
    Column::concat(col.data_type(), &pieces).expect("slices of one column always concat cleanly")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DataType, Value};
    use adelie_harness::rng::SplitMix64;

    #[test]
    fn constant_delta_int64_avoids_plain() {
        let values: Vec<Value> = (0..10_000i64).map(Value::Int64).collect();
        let col = Column::from_values(&DataType::Int64, &values).unwrap();
        let chosen = choose(&col);
        assert!(
            matches!(chosen, Encoding::Delta | Encoding::DeltaOfDelta),
            "expected Delta or DeltaOfDelta, got {chosen:?}"
        );
    }

    #[test]
    fn repeated_string_chooses_dict() {
        let values: Vec<Value> = (0..10_000)
            .map(|_| Value::String("same-value".into()))
            .collect();
        let col = Column::from_values(&DataType::String, &values).unwrap();
        assert_eq!(choose(&col), Encoding::Dict);
    }

    #[test]
    fn random_u64_avoids_dict_and_rle() {
        let mut rng = SplitMix64::new(99);
        let values: Vec<Value> = (0..10_000).map(|_| Value::UInt64(rng.next_u64())).collect();
        let col = Column::from_values(&DataType::UInt64, &values).unwrap();
        let chosen = choose(&col);
        assert!(
            matches!(chosen, Encoding::Plain | Encoding::For),
            "expected Plain or For, got {chosen:?}"
        );
    }
}
