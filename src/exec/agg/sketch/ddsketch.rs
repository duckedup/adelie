//! DDSketch for `approx_quantile` (SPEC §8, D0015, FROZEN). Relative accuracy α = 0.01, so
//! γ = (1+α)/(1−α). Bucket key for x > 0 is `ceil(ln(x)/ln(γ))`; negative x key off `−x` in
//! a separate store; x == 0.0 (± sign) goes to `zero_count`. Stores are sparse `BTreeMap`s.

use std::collections::BTreeMap;
use std::mem::size_of;

use crate::exec::agg::GroupsAccumulator;
use crate::exec::{Bitmap, Column};
use crate::storage::segment::wire::{Cursor, Sink};
use crate::types::{DataType, Decimal, Value};

const ALPHA: f64 = 0.01;

fn gamma() -> f64 {
    (1.0 + ALPHA) / (1.0 - ALPHA)
}

fn key_for(x: f64, ln_gamma: f64) -> i32 {
    (x.ln() / ln_gamma).ceil() as i32
}

fn value_of(key: i32, gamma: f64) -> f64 {
    2.0 * gamma.powi(key) / (gamma + 1.0)
}

/// `unscaled / 10^scale`, the only place DECIMAL loses precision on the way into the sketch.
fn decimal_to_f64(d: Decimal) -> f64 {
    d.unscaled() as f64 / 10f64.powi(d.scale() as i32)
}

/// NULL and NaN both fold to "skip this row"; the type match mirrors `new`'s accepted set.
fn row_as_f64(col: &Column, row: usize) -> Option<f64> {
    match col.get(row) {
        Value::Null => None,
        Value::Int64(x) => Some(x as f64),
        Value::UInt64(x) => Some(x as f64),
        Value::Float64(x) if !x.is_nan() => Some(x),
        Value::Float64(_) => None,
        Value::Decimal(d) => Some(decimal_to_f64(d)),
        _ => None,
    }
}

#[derive(Default)]
struct GroupState {
    zero_count: u64,
    neg: BTreeMap<i32, u64>,
    pos: BTreeMap<i32, u64>,
}

impl GroupState {
    fn insert(&mut self, x: f64, ln_gamma: f64) {
        if x == 0.0 {
            self.zero_count += 1;
        } else if x > 0.0 {
            *self.pos.entry(key_for(x, ln_gamma)).or_insert(0) += 1;
        } else {
            *self.neg.entry(key_for(-x, ln_gamma)).or_insert(0) += 1;
        }
    }

    fn merge_from(&mut self, other: &GroupState) {
        self.zero_count += other.zero_count;
        for (&k, &c) in &other.neg {
            *self.neg.entry(k).or_insert(0) += c;
        }
        for (&k, &c) in &other.pos {
            *self.pos.entry(k).or_insert(0) += c;
        }
    }

    /// Ranks by `q·(n−1)`; walks negative (high to low), zero, then positive (low to high),
    /// returning the first bucket whose cumulative count exceeds the rank.
    fn quantile(&self, q: f64, gamma: f64) -> Option<f64> {
        let n = self.neg.values().sum::<u64>() + self.zero_count + self.pos.values().sum::<u64>();
        if n == 0 {
            return None;
        }
        let rank = q * (n - 1) as f64;
        let mut cum = 0u64;
        for (&k, &c) in self.neg.iter().rev() {
            cum += c;
            if cum as f64 > rank {
                return Some(-value_of(k, gamma));
            }
        }
        cum += self.zero_count;
        if cum as f64 > rank {
            return Some(0.0);
        }
        for (&k, &c) in self.pos.iter() {
            cum += c;
            if cum as f64 > rank {
                return Some(value_of(k, gamma));
            }
        }
        None
    }

    fn byte_size(&self) -> usize {
        let entry = size_of::<(i32, u64)>();
        (self.neg.len() + self.pos.len()) * entry
    }
}

/// `approx_quantile(x, q)`: FLOAT64 result, NULL for an empty group, NaN inputs skipped.
pub(crate) struct DdSketchAccumulator {
    q: f64,
    groups: Vec<GroupState>,
}

impl DdSketchAccumulator {
    pub(crate) fn new(ty: &DataType, q: f64) -> Result<Self, crate::exec::ExecError> {
        if q.is_nan() || !(0.0..=1.0).contains(&q) {
            return Err(crate::exec::ExecError::Invalid(format!(
                "approx_quantile: q {q} is not in [0,1]"
            )));
        }
        match ty {
            DataType::Int64 | DataType::UInt64 | DataType::Float64 | DataType::Decimal(_) => {}
            other => {
                return Err(crate::exec::ExecError::Plan(format!(
                    "approx_quantile: unsupported argument type {other}"
                )));
            }
        }
        Ok(DdSketchAccumulator {
            q,
            groups: Vec::new(),
        })
    }

    fn grow(&mut self, total_groups: usize) {
        if self.groups.len() < total_groups {
            self.groups.resize_with(total_groups, GroupState::default);
        }
    }
}

impl GroupsAccumulator for DdSketchAccumulator {
    fn update(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        args: &[&Column],
        filter: Option<&Bitmap>,
    ) -> Result<(), crate::exec::ExecError> {
        self.grow(total_groups);
        let col = args[0];
        let ln_gamma = gamma().ln();
        for (row, &g) in groups.iter().enumerate() {
            if filter.is_some_and(|f| !f.get(row)) {
                continue;
            }
            let Some(x) = row_as_f64(col, row) else {
                continue;
            };
            self.groups[g].insert(x, ln_gamma);
        }
        Ok(())
    }

    fn merge(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        other: Box<dyn GroupsAccumulator>,
    ) -> Result<(), crate::exec::ExecError> {
        let other = other
            .into_any()
            .downcast::<DdSketchAccumulator>()
            .map_err(|_| crate::exec::ExecError::Plan("merge of mismatched accumulators".into()))?;
        self.grow(total_groups);
        for (g_other, state) in other.groups.iter().enumerate() {
            if let Some(&target) = groups.get(g_other) {
                self.groups[target].merge_from(state);
            }
        }
        Ok(())
    }

    fn finish(self: Box<Self>, total_groups: usize) -> Result<Column, crate::exec::ExecError> {
        let gamma = gamma();
        let mut values = Vec::with_capacity(total_groups);
        for g in 0..total_groups {
            let v = self.groups.get(g).and_then(|st| st.quantile(self.q, gamma));
            values.push(v.map_or(Value::Null, Value::Float64));
        }
        Ok(Column::from_values(&DataType::Float64, &values)?)
    }

    fn data_type(&self) -> DataType {
        DataType::Float64
    }

    fn encode_state(&self, group: usize, out: &mut Vec<u8>) {
        let mut sink = Sink::new();
        sink.u8(1); // version
        let empty = GroupState::default();
        let st = self.groups.get(group).unwrap_or(&empty);
        sink.uvarint(st.zero_count);
        sink.uvarint(st.neg.len() as u64);
        for (&k, &c) in &st.neg {
            sink.svarint(k as i64);
            sink.uvarint(c);
        }
        sink.uvarint(st.pos.len() as u64);
        for (&k, &c) in &st.pos {
            sink.svarint(k as i64);
            sink.uvarint(c);
        }
        out.extend(sink.into_vec());
    }

    fn merge_encoded(
        &mut self,
        group: usize,
        total_groups: usize,
        bytes: &[u8],
    ) -> Result<(), crate::exec::ExecError> {
        let decoded = decode_state(bytes)
            .map_err(|e| crate::exec::ExecError::Invalid(format!("ddsketch state: {e:?}")))?;
        self.grow(total_groups);
        self.groups[group].merge_from(&decoded);
        Ok(())
    }

    fn byte_size(&self) -> usize {
        self.groups.capacity() * size_of::<GroupState>()
            + self.groups.iter().map(GroupState::byte_size).sum::<usize>()
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

fn decode_state(bytes: &[u8]) -> Result<GroupState, crate::storage::segment::DecodeError> {
    let mut cur = Cursor::new(bytes);
    if cur.u8()? != 1 {
        return Err(crate::storage::segment::DecodeError::Malformed(
            "ddsketch: unknown state version",
        ));
    }
    let zero_count = cur.uvarint()?;
    let n_neg_raw = cur.uvarint()?;
    let n_neg = cur.guard_len(n_neg_raw, 2)?;
    let mut neg = BTreeMap::new();
    for _ in 0..n_neg {
        neg.insert(cur.svarint()? as i32, cur.uvarint()?);
    }
    let n_pos_raw = cur.uvarint()?;
    let n_pos = cur.guard_len(n_pos_raw, 2)?;
    let mut pos = BTreeMap::new();
    for _ in 0..n_pos {
        pos.insert(cur.svarint()? as i32, cur.uvarint()?);
    }
    Ok(GroupState {
        zero_count,
        neg,
        pos,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::segment::index::test_support::SplitMix64;

    fn col_of(values: &[f64]) -> Column {
        let vs: Vec<Value> = values.iter().map(|&v| Value::Float64(v)).collect();
        Column::from_values(&DataType::Float64, &vs).unwrap()
    }

    fn quantile_of(acc: DdSketchAccumulator, total_groups: usize) -> Option<f64> {
        let col = Box::new(acc).finish(total_groups).unwrap();
        match col.get(0) {
            Value::Null => None,
            Value::Float64(v) => Some(v),
            other => panic!("expected FLOAT64, got {other:?}"),
        }
    }

    #[test]
    fn median_over_1_to_1000_within_one_percent() {
        let values: Vec<f64> = (1..=1000).map(|v| v as f64).collect();
        let mut acc = DdSketchAccumulator::new(&DataType::Float64, 0.5).unwrap();
        let col = col_of(&values);
        acc.update(&vec![0; values.len()], 1, &[&col], None)
            .unwrap();
        let got = quantile_of(acc, 1).unwrap();
        assert!((got - 500.0).abs() / 500.0 <= 0.01, "got {got}");
    }

    #[test]
    #[cfg_attr(miri, ignore)] // slow under Miri
    fn q99_within_one_percent_of_exact() {
        let mut rng = SplitMix64::new(7);
        // Lognormal-ish: exponentiate a small uniform-ish spread so the tail is heavy.
        let mut values: Vec<f64> = (0..5000)
            .map(|_| {
                let u = (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
                (u * 10.0).exp()
            })
            .collect();
        let mut acc = DdSketchAccumulator::new(&DataType::Float64, 0.99).unwrap();
        let col = col_of(&values);
        acc.update(&vec![0; values.len()], 1, &[&col], None)
            .unwrap();
        let got = quantile_of(acc, 1).unwrap();

        values.sort_by(|a, b| a.total_cmp(b));
        let rank = (0.99 * (values.len() - 1) as f64).floor() as usize;
        let exact = values[rank];
        assert!(
            (got - exact).abs() / exact <= 0.01,
            "got {got}, exact {exact}"
        );
    }

    #[test]
    fn negatives_and_zero_median_is_exactly_zero() {
        let mut acc = DdSketchAccumulator::new(&DataType::Float64, 0.5).unwrap();
        let col = col_of(&[-5.0, 0.0, 5.0]);
        acc.update(&[0, 0, 0], 1, &[&col], None).unwrap();
        assert_eq!(quantile_of(acc, 1), Some(0.0));
    }

    #[test]
    fn merging_two_sketches_matches_one_pass_bucket_counts() {
        let values: Vec<f64> = (1..=200).map(|v| v as f64).collect();
        let mut whole = DdSketchAccumulator::new(&DataType::Float64, 0.9).unwrap();
        let col = col_of(&values);
        whole
            .update(&vec![0; values.len()], 1, &[&col], None)
            .unwrap();

        let mut a = DdSketchAccumulator::new(&DataType::Float64, 0.9).unwrap();
        let col_a = col_of(&values[..100]);
        a.update(&vec![0; 100], 1, &[&col_a], None).unwrap();
        let mut b = DdSketchAccumulator::new(&DataType::Float64, 0.9).unwrap();
        let col_b = col_of(&values[100..]);
        b.update(&vec![0; 100], 1, &[&col_b], None).unwrap();
        a.merge(&[0], 1, Box::new(b)).unwrap();

        assert_eq!(whole.groups[0].neg, a.groups[0].neg);
        assert_eq!(whole.groups[0].pos, a.groups[0].pos);
        assert_eq!(whole.groups[0].zero_count, a.groups[0].zero_count);
    }

    #[test]
    fn encoded_round_trip_is_exact() {
        let values: Vec<f64> = (1..=50).map(|v| v as f64).collect();
        let mut a = DdSketchAccumulator::new(&DataType::Float64, 0.5).unwrap();
        let col = col_of(&values);
        a.update(&vec![0; values.len()], 1, &[&col], None).unwrap();

        let mut bytes = Vec::new();
        a.encode_state(0, &mut bytes);
        let mut b = DdSketchAccumulator::new(&DataType::Float64, 0.5).unwrap();
        b.merge_encoded(0, 1, &bytes).unwrap();

        assert_eq!(a.groups[0].neg, b.groups[0].neg);
        assert_eq!(a.groups[0].pos, b.groups[0].pos);
        assert_eq!(a.groups[0].zero_count, b.groups[0].zero_count);
    }

    #[test]
    fn new_rejects_bad_q() {
        assert!(DdSketchAccumulator::new(&DataType::Float64, -0.1).is_err());
        assert!(DdSketchAccumulator::new(&DataType::Float64, 1.1).is_err());
        assert!(DdSketchAccumulator::new(&DataType::Float64, f64::NAN).is_err());
    }

    #[test]
    fn merge_encoded_rejects_garbage() {
        let mut acc = DdSketchAccumulator::new(&DataType::Float64, 0.5).unwrap();
        acc.grow(1);
        assert!(acc.merge_encoded(0, 1, &[]).is_err());
        assert!(acc.merge_encoded(0, 1, &[9]).is_err());
        assert!(
            acc.merge_encoded(0, 1, &[1, 255, 255, 255, 255, 255])
                .is_err()
        );
    }
}
