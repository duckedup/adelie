//! HyperLogLog for `approx_count_distinct` (SPEC §8, D0015, FROZEN). Precision p = 14
//! (m = 16 384 one-byte registers), hashed via `kernels::stable_hash`. A group under 64
//! distinct hashes stays a sparse, exact `Vec<u64>`, promoted to dense registers at 64.

use std::mem::size_of;

use crate::exec::agg::GroupsAccumulator;
use crate::exec::kernels;
use crate::exec::{Bitmap, Column};
use crate::storage::segment::wire::{Cursor, Sink};
use crate::types::{DataType, Value};

/// p = 14: `REGISTERS` = 2^14, `INDEX_BITS` of the 64-bit hash pick the register.
const INDEX_BITS: u32 = 14;
const REGISTERS: usize = 1 << INDEX_BITS;
/// A group's sparse hash set is promoted to dense registers once it reaches this size.
const SPARSE_LIMIT: usize = 64;

/// One group's registers: exact below `SPARSE_LIMIT` distinct hashes, else dense HLL.
enum HllState {
    Sparse(Vec<u64>),
    Dense(Box<[u8; REGISTERS]>),
}

impl HllState {
    /// Folds one hash in: dedups into the sparse set, or maxes the dense register.
    fn insert(&mut self, hash: u64) {
        match self {
            HllState::Dense(regs) => insert_dense(regs, hash),
            HllState::Sparse(v) => {
                if let Err(pos) = v.binary_search(&hash) {
                    v.insert(pos, hash);
                }
                if v.len() >= SPARSE_LIMIT {
                    self.promote();
                }
            }
        }
    }

    /// Converts a sparse state to dense registers built from its exact hash set.
    fn promote(&mut self) {
        if let HllState::Sparse(v) = self {
            let mut regs = Box::new([0u8; REGISTERS]);
            for &h in v.iter() {
                insert_dense(&mut regs, h);
            }
            *self = HllState::Dense(regs);
        }
    }

    /// Folds `other`'s registers in: exact set union while sparse, else register-wise max.
    fn merge(&mut self, other: &HllState) {
        match other {
            HllState::Sparse(v) => {
                for &h in v {
                    self.insert(h);
                }
            }
            HllState::Dense(rb) => {
                self.promote();
                if let HllState::Dense(ra) = self {
                    for (a, &b) in ra.iter_mut().zip(rb.iter()) {
                        *a = (*a).max(b);
                    }
                }
            }
        }
    }

    /// Exact distinct count while sparse; the standard HLL estimate once dense.
    fn estimate(&self) -> f64 {
        match self {
            HllState::Sparse(v) => v.len() as f64,
            HllState::Dense(regs) => estimate_dense(regs),
        }
    }

    fn byte_size(&self) -> usize {
        match self {
            HllState::Sparse(v) => v.capacity() * size_of::<u64>(),
            HllState::Dense(_) => REGISTERS,
        }
    }
}

/// Index = top 14 bits of the hash; rank = leading zeros of the remaining 50 bits (shifted
/// up so they occupy the top of a fresh word) + 1, capped at 51.
fn insert_dense(regs: &mut [u8; REGISTERS], hash: u64) {
    let idx = (hash >> (64 - INDEX_BITS)) as usize;
    let lz = (hash << INDEX_BITS).leading_zeros();
    let rank = (lz + 1).min(51) as u8;
    if regs[idx] < rank {
        regs[idx] = rank;
    }
}

/// `α_m·m²/Σ2^-reg`, falling back to linear counting `m·ln(m/zeros)` when the raw estimate
/// is small and some registers are still empty. No large-range correction: the hash is 64-bit.
fn estimate_dense(regs: &[u8; REGISTERS]) -> f64 {
    let m = REGISTERS as f64;
    let mut sum = 0f64;
    let mut zeros = 0u32;
    for &r in regs.iter() {
        // 2^-r built exactly (r <= 51): `powi` is not exact everywhere, and Miri perturbs it.
        sum += 1.0 / (1u64 << r) as f64;
        if r == 0 {
            zeros += 1;
        }
    }
    let alpha_m = 0.7213 / (1.0 + 1.079 / m);
    let raw = alpha_m * m * m / sum;
    if raw <= 2.5 * m && zeros > 0 {
        m * (m / zeros as f64).ln()
    } else {
        raw
    }
}

/// `approx_count_distinct(x)`: INT64 result, 0 for an empty group, NULLs skipped.
pub(crate) struct HllAccumulator {
    states: Vec<HllState>,
}

impl HllAccumulator {
    pub(crate) fn new() -> Self {
        HllAccumulator { states: Vec::new() }
    }

    fn grow(&mut self, total_groups: usize) {
        if self.states.len() < total_groups {
            self.states
                .resize_with(total_groups, || HllState::Sparse(Vec::new()));
        }
    }
}

impl GroupsAccumulator for HllAccumulator {
    fn update(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        args: &[&Column],
        filter: Option<&Bitmap>,
    ) -> Result<(), crate::exec::ExecError> {
        self.grow(total_groups);
        let col = args[0];
        for (row, &g) in groups.iter().enumerate() {
            if filter.is_some_and(|f| !f.get(row)) || col.is_null(row) {
                continue;
            }
            let hash = kernels::stable_hash(col, row);
            self.states[g].insert(hash);
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
            .downcast::<HllAccumulator>()
            .map_err(|_| crate::exec::ExecError::Plan("merge of mismatched accumulators".into()))?;
        self.grow(total_groups);
        for (g_other, state) in other.states.iter().enumerate() {
            if let Some(&target) = groups.get(g_other) {
                self.states[target].merge(state);
            }
        }
        Ok(())
    }

    fn finish(self: Box<Self>, total_groups: usize) -> Result<Column, crate::exec::ExecError> {
        let mut values = Vec::with_capacity(total_groups);
        for g in 0..total_groups {
            let est = self.states.get(g).map_or(0.0, HllState::estimate);
            values.push(Value::Int64(est.round() as i64));
        }
        Ok(Column::from_values(&DataType::Int64, &values)?)
    }

    fn data_type(&self) -> DataType {
        DataType::Int64
    }

    fn encode_state(&self, group: usize, out: &mut Vec<u8>) {
        let mut sink = Sink::new();
        sink.u8(1); // version
        match self.states.get(group) {
            Some(HllState::Dense(regs)) => {
                sink.u8(0);
                sink.raw(&regs[..]);
            }
            Some(HllState::Sparse(v)) => {
                sink.u8(1);
                sink.uvarint(v.len() as u64);
                for &h in v {
                    sink.u64(h);
                }
            }
            None => {
                sink.u8(1);
                sink.uvarint(0);
            }
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
            .map_err(|e| crate::exec::ExecError::Invalid(format!("hll state: {e:?}")))?;
        self.grow(total_groups);
        self.states[group].merge(&decoded);
        Ok(())
    }

    fn byte_size(&self) -> usize {
        self.states.capacity() * size_of::<HllState>()
            + self.states.iter().map(HllState::byte_size).sum::<usize>()
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

fn decode_state(bytes: &[u8]) -> Result<HllState, crate::storage::segment::DecodeError> {
    use crate::storage::segment::DecodeError;
    let mut cur = Cursor::new(bytes);
    if cur.u8()? != 1 {
        return Err(DecodeError::Malformed("hll: unknown state version"));
    }
    match cur.u8()? {
        0 => {
            let raw = cur.raw(REGISTERS)?;
            let mut regs = Box::new([0u8; REGISTERS]);
            regs.copy_from_slice(raw);
            Ok(HllState::Dense(regs))
        }
        1 => {
            let n_raw = cur.uvarint()?;
            let n = cur.guard_len(n_raw, 8)?;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(cur.u64()?);
            }
            Ok(HllState::Sparse(v))
        }
        _ => Err(DecodeError::Malformed("hll: unknown state tag")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::segment::index::test_support::SplitMix64;

    fn col_of(values: &[i64]) -> Column {
        let vs: Vec<Value> = values.iter().map(|&v| Value::Int64(v)).collect();
        Column::from_values(&DataType::Int64, &vs).unwrap()
    }

    fn finish_one(acc: HllAccumulator, total_groups: usize) -> i64 {
        let col = Box::new(acc).finish(total_groups).unwrap();
        match col.get(0) {
            Value::Int64(v) => v,
            other => panic!("expected INT64, got {other:?}"),
        }
    }

    #[test]
    fn zero_distinct_is_zero() {
        let acc = HllAccumulator::new();
        assert_eq!(finish_one(acc, 1), 0);
    }

    #[test]
    fn one_distinct_repeated_is_one() {
        let mut acc = HllAccumulator::new();
        let col = col_of(&[7; 1000]);
        let groups = vec![0usize; 1000];
        acc.update(&groups, 1, &[&col], None).unwrap();
        assert_eq!(finish_one(acc, 1), 1);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // slow under Miri
    fn ten_thousand_distinct_within_three_percent() {
        let mut acc = HllAccumulator::new();
        let values: Vec<i64> = (0..10_000).collect();
        let col = col_of(&values);
        let groups = vec![0usize; values.len()];
        acc.update(&groups, 1, &[&col], None).unwrap();
        let est = finish_one(acc, 1) as f64;
        let err = (est - 10_000.0).abs() / 10_000.0;
        assert!(err <= 0.03, "estimate {est} off by {err}");
    }

    #[test]
    fn merge_of_two_halves_equals_one_pass() {
        let values: Vec<i64> = (0..200).collect();
        let mut whole = HllAccumulator::new();
        let col = col_of(&values);
        let groups = vec![0usize; values.len()];
        whole.update(&groups, 1, &[&col], None).unwrap();

        let mut a = HllAccumulator::new();
        let col_a = col_of(&values[..100]);
        a.update(&vec![0usize; 100], 1, &[&col_a], None).unwrap();
        let mut b = HllAccumulator::new();
        let col_b = col_of(&values[100..]);
        b.update(&vec![0usize; 100], 1, &[&col_b], None).unwrap();
        a.merge(&[0], 1, Box::new(b)).unwrap();

        assert_eq!(finish_one(whole, 1), finish_one(a, 1));
    }

    #[test]
    fn encode_merge_round_trip_is_exact() {
        let values: Vec<i64> = (0..200).collect();
        let mut whole = HllAccumulator::new();
        let col = col_of(&values);
        let groups = vec![0usize; values.len()];
        whole.update(&groups, 1, &[&col], None).unwrap();

        let mut a = HllAccumulator::new();
        let col_a = col_of(&values[..100]);
        a.update(&vec![0usize; 100], 1, &[&col_a], None).unwrap();
        let mut b = HllAccumulator::new();
        let col_b = col_of(&values[100..]);
        b.update(&vec![0usize; 100], 1, &[&col_b], None).unwrap();
        let mut bytes = Vec::new();
        b.encode_state(0, &mut bytes);
        a.merge_encoded(0, 1, &bytes).unwrap();

        assert_eq!(finish_one(whole, 1), finish_one(a, 1));
    }

    #[test]
    fn signed_zero_counts_once() {
        let mut acc = HllAccumulator::new();
        let col = Column::from_values(
            &DataType::Float64,
            &[Value::Float64(-0.0), Value::Float64(0.0)],
        )
        .unwrap();
        acc.update(&[0, 0], 1, &[&col], None).unwrap();
        assert_eq!(finish_one(acc, 1), 1);
    }

    #[test]
    fn nan_counts_once() {
        let mut acc = HllAccumulator::new();
        let col = Column::from_values(
            &DataType::Float64,
            &[Value::Float64(f64::NAN), Value::Float64(f64::NAN)],
        )
        .unwrap();
        acc.update(&[0, 0], 1, &[&col], None).unwrap();
        assert_eq!(finish_one(acc, 1), 1);
    }

    #[test]
    fn merge_encoded_rejects_garbage() {
        let mut acc = HllAccumulator::new();
        acc.grow(1);
        assert!(acc.merge_encoded(0, 1, &[]).is_err());
        assert!(acc.merge_encoded(0, 1, &[9]).is_err());
        assert!(acc.merge_encoded(0, 1, &[1, 0, 1, 2, 3]).is_err());
    }

    #[test]
    fn dense_path_matches_sparse_seed() {
        // SplitMix64-driven stream that crosses the sparse->dense promotion boundary.
        let mut rng = SplitMix64::new(42);
        let values: Vec<i64> = (0..500).map(|_| rng.next_u64() as i64).collect();
        let mut acc = HllAccumulator::new();
        let col = col_of(&values);
        let groups = vec![0usize; values.len()];
        acc.update(&groups, 1, &[&col], None).unwrap();
        // Distinct values are very likely exactly 500; estimate should be close regardless.
        let est = finish_one(acc, 1) as f64;
        assert!((est - 500.0).abs() / 500.0 <= 0.1, "estimate {est}");
    }
}
