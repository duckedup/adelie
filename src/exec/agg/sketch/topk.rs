//! Space-Saving summary for `top_k` (SPEC §8, D0015, FROZEN). Capacity c = max(64, 8·k),
//! keyed by `encode_row_key` bytes. Exact whenever the group's distinct count ≤ c; above
//! that, the summary is a bounded approximation with a per-entry over-count bound.

use std::collections::HashMap;
use std::mem::size_of;

use crate::exec::agg::GroupsAccumulator;
use crate::exec::kernels::{self, rowkey::NullKeys};
use crate::exec::{Bitmap, Column, ColumnBuilder};
use crate::storage::segment::value::{decode_value, encode_value};
use crate::storage::segment::wire::{Cursor, Sink};
use crate::types::{DataType, Value, total_cmp};

struct Entry {
    value: Value,
    count: u64,
    error: u64,
}

#[derive(Default)]
struct Summary {
    entries: HashMap<Vec<u8>, Entry>,
}

impl Summary {
    /// Existing key: count += 1. New key under capacity: (1, 0). New key at capacity:
    /// replaces the minimum-count entry with (min.count + 1, min.count).
    fn insert(&mut self, key: Vec<u8>, value: impl FnOnce() -> Value, capacity: usize) {
        if let Some(e) = self.entries.get_mut(&key) {
            e.count += 1;
            return;
        }
        if self.entries.len() < capacity {
            self.entries.insert(
                key,
                Entry {
                    value: value(),
                    count: 1,
                    error: 0,
                },
            );
            return;
        }
        let min_key = self
            .entries
            .iter()
            .min_by_key(|(_, e)| e.count)
            .map(|(k, _)| k.clone())
            .expect("capacity is at least 1");
        let min_count = self.entries.remove(&min_key).expect("just found").count;
        self.entries.insert(
            key,
            Entry {
                value: value(),
                count: min_count + 1,
                error: min_count,
            },
        );
    }

    /// Sums counts/errors of shared keys, inserts the rest, then trims to `capacity`.
    fn merge(&mut self, other: Summary, capacity: usize) {
        for (key, oe) in other.entries {
            match self.entries.get_mut(&key) {
                Some(e) => {
                    e.count += oe.count;
                    e.error += oe.error;
                }
                None => {
                    self.entries.insert(key, oe);
                }
            }
        }
        self.trim(capacity);
    }

    /// Keeps the `capacity` largest by count; ties at the cut break by ascending key bytes.
    fn trim(&mut self, capacity: usize) {
        if self.entries.len() <= capacity {
            return;
        }
        let mut items: Vec<(Vec<u8>, Entry)> = self.entries.drain().collect();
        items.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(&b.0)));
        items.truncate(capacity);
        self.entries = items.into_iter().collect();
    }

    fn byte_size(&self) -> usize {
        self.entries
            .iter()
            .map(|(k, e)| k.capacity() + size_of::<Entry>() + value_heap_size(&e.value))
            .sum()
    }
}

fn value_heap_size(v: &Value) -> usize {
    match v {
        Value::String(s) => s.capacity(),
        Value::Bytes(b) => b.capacity(),
        Value::List(items) => {
            items.capacity() * size_of::<Value>() + items.iter().map(value_heap_size).sum::<usize>()
        }
        _ => 0,
    }
}

/// A single value's `encode_row_key` bytes, for turning a decoded state entry back into the
/// map key that `update` would have produced for the same value.
fn key_bytes(ty: &DataType, value: &Value) -> Vec<u8> {
    let col = Column::from_values(ty, std::slice::from_ref(value)).expect("value fits its type");
    let mut out = Vec::new();
    let _ = kernels::encode_row_key(&[&col], 0, NullKeys::Group, &mut out);
    out
}

/// `top_k(x, k)`: LIST<T> of up to k values most frequent first (ties ascending `total_cmp`),
/// NULL for an empty group, NULLs skipped. LIST arguments and `k == 0` are rejected by `new`.
pub(crate) struct TopKAccumulator {
    ty: DataType,
    k: usize,
    capacity: usize,
    groups: Vec<Summary>,
}

impl TopKAccumulator {
    pub(crate) fn new(ty: &DataType, k: usize) -> Result<Self, crate::exec::ExecError> {
        if matches!(ty, DataType::List(_)) {
            return Err(crate::exec::ExecError::Plan(format!(
                "top_k: LIST argument {ty} is not supported"
            )));
        }
        if k == 0 {
            return Err(crate::exec::ExecError::Invalid(
                "top_k: k must be at least 1".into(),
            ));
        }
        Ok(TopKAccumulator {
            ty: ty.clone(),
            k,
            capacity: (8 * k).max(64),
            groups: Vec::new(),
        })
    }

    fn grow(&mut self, total_groups: usize) {
        if self.groups.len() < total_groups {
            self.groups.resize_with(total_groups, Summary::default);
        }
    }
}

impl GroupsAccumulator for TopKAccumulator {
    fn update(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        args: &[&Column],
        filter: Option<&Bitmap>,
    ) -> Result<(), crate::exec::ExecError> {
        self.grow(total_groups);
        let col = args[0];
        let capacity = self.capacity;
        for (row, &g) in groups.iter().enumerate() {
            if filter.is_some_and(|f| !f.get(row)) || col.is_null(row) {
                continue;
            }
            let mut key = Vec::new();
            let _ = kernels::encode_row_key(&[col], row, NullKeys::Group, &mut key);
            self.groups[g].insert(key, || col.get(row), capacity);
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
            .downcast::<TopKAccumulator>()
            .map_err(|_| crate::exec::ExecError::Plan("merge of mismatched accumulators".into()))?;
        self.grow(total_groups);
        let capacity = self.capacity;
        for (g_other, summary) in other.groups.into_iter().enumerate() {
            if let Some(&target) = groups.get(g_other) {
                self.groups[target].merge(summary, capacity);
            }
        }
        Ok(())
    }

    fn finish(self: Box<Self>, total_groups: usize) -> Result<Column, crate::exec::ExecError> {
        let list_ty = DataType::list(self.ty.clone()).expect("checked non-list at construction");
        let mut builder = ColumnBuilder::new(list_ty);
        for g in 0..total_groups {
            match self.groups.get(g) {
                None => builder.push_null(),
                Some(summary) if summary.entries.is_empty() => builder.push_null(),
                Some(summary) => {
                    let mut items: Vec<&Entry> = summary.entries.values().collect();
                    items.sort_by(|a, b| {
                        b.count.cmp(&a.count).then_with(|| {
                            total_cmp(&a.value, &b.value).unwrap_or(std::cmp::Ordering::Equal)
                        })
                    });
                    let top: Vec<Value> = items
                        .into_iter()
                        .take(self.k)
                        .map(|e| e.value.clone())
                        .collect();
                    builder.push(&Value::List(top))?;
                }
            }
        }
        Ok(builder.finish())
    }

    fn data_type(&self) -> DataType {
        DataType::list(self.ty.clone()).expect("checked non-list at construction")
    }

    fn encode_state(&self, group: usize, out: &mut Vec<u8>) {
        let mut sink = Sink::new();
        sink.u8(1); // version
        sink.uvarint(self.k as u64);
        let empty = Summary::default();
        let summary = self.groups.get(group).unwrap_or(&empty);
        let mut items: Vec<&Entry> = summary.entries.values().collect();
        items.sort_by_key(|e| std::cmp::Reverse(e.count));
        sink.uvarint(items.len() as u64);
        for e in items {
            encode_value(&e.value, &self.ty, &mut sink);
            sink.uvarint(e.count);
            sink.uvarint(e.error);
        }
        out.extend(sink.into_vec());
    }

    fn merge_encoded(
        &mut self,
        group: usize,
        total_groups: usize,
        bytes: &[u8],
    ) -> Result<(), crate::exec::ExecError> {
        let mut cur = Cursor::new(bytes);
        let invalid = |e: crate::storage::segment::DecodeError| {
            crate::exec::ExecError::Invalid(format!("top_k state: {e:?}"))
        };
        if cur.u8().map_err(invalid)? != 1 {
            return Err(crate::exec::ExecError::Invalid(
                "top_k state: unknown version".into(),
            ));
        }
        let k = cur.uvarint().map_err(invalid)?;
        if k != self.k as u64 {
            return Err(crate::exec::ExecError::Invalid(format!(
                "top_k state: mismatched k (state {k}, accumulator {})",
                self.k
            )));
        }
        let n_raw = cur.uvarint().map_err(invalid)?;
        let n = cur.guard_len(n_raw, 2).map_err(invalid)?;
        self.grow(total_groups);
        for _ in 0..n {
            let value = decode_value(&mut cur, &self.ty).map_err(invalid)?;
            let count = cur.uvarint().map_err(invalid)?;
            let error = cur.uvarint().map_err(invalid)?;
            let key = key_bytes(&self.ty, &value);
            let entries = &mut self.groups[group].entries;
            match entries.get_mut(&key) {
                Some(e) => {
                    e.count += count;
                    e.error += error;
                }
                None => {
                    entries.insert(
                        key,
                        Entry {
                            value,
                            count,
                            error,
                        },
                    );
                }
            }
        }
        let capacity = self.capacity;
        self.groups[group].trim(capacity);
        Ok(())
    }

    fn byte_size(&self) -> usize {
        self.groups.capacity() * size_of::<Summary>()
            + self.groups.iter().map(Summary::byte_size).sum::<usize>()
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::segment::index::test_support::SplitMix64;

    fn col_of(values: &[&str]) -> Column {
        let vs: Vec<Value> = values
            .iter()
            .map(|&v| Value::String(v.to_string()))
            .collect();
        Column::from_values(&DataType::String, &vs).unwrap()
    }

    fn top_values(col: &Column) -> Vec<String> {
        match col.get(0) {
            Value::List(items) => items
                .into_iter()
                .map(|v| match v {
                    Value::String(s) => s,
                    other => panic!("expected STRING, got {other:?}"),
                })
                .collect(),
            other => panic!("expected LIST, got {other:?}"),
        }
    }

    #[test]
    fn tie_breaks_ascending_by_value() {
        let values = ["a", "a", "a", "a", "a", "b", "b", "b", "c", "c", "c", "d"];
        let col = col_of(&values);
        let mut acc = TopKAccumulator::new(&DataType::String, 2).unwrap();
        acc.update(&vec![0; values.len()], 1, &[&col], None)
            .unwrap();
        let result = Box::new(acc).finish(1).unwrap();
        assert_eq!(top_values(&result), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // slow under Miri
    fn skewed_stream_finds_true_top_three() {
        let mut rng = SplitMix64::new(11);
        let mut values: Vec<i64> = Vec::with_capacity(5000);
        // Top 3 keys hold 50% of 5000 rows; the rest spread over ~997 distinct tail keys.
        for _ in 0..2500 {
            values.push((rng.next_u64() % 3) as i64);
        }
        for _ in 0..2500 {
            values.push(3 + (rng.next_u64() % 997) as i64);
        }
        let vs: Vec<Value> = values.iter().map(|&v| Value::Int64(v)).collect();
        let col = Column::from_values(&DataType::Int64, &vs).unwrap();
        let mut acc = TopKAccumulator::new(&DataType::Int64, 3).unwrap();
        acc.update(&vec![0; values.len()], 1, &[&col], None)
            .unwrap();
        let result = Box::new(acc).finish(1).unwrap();
        let top = match result.get(0) {
            Value::List(items) => items
                .into_iter()
                .map(|v| match v {
                    Value::Int64(x) => x,
                    other => panic!("expected INT64, got {other:?}"),
                })
                .collect::<Vec<_>>(),
            other => panic!("expected LIST, got {other:?}"),
        };
        let mut expect = vec![0i64, 1, 2];
        let mut got = top.clone();
        expect.sort_unstable();
        got.sort_unstable();
        assert_eq!(got, expect, "top: {top:?}");
    }

    #[test]
    fn merge_of_disjoint_halves_under_capacity_is_exact() {
        let col_a = col_of(&["a", "a", "b"]);
        let col_b = col_of(&["c", "c", "c"]);
        let mut a = TopKAccumulator::new(&DataType::String, 2).unwrap();
        a.update(&[0; 3], 1, &[&col_a], None).unwrap();
        let mut b = TopKAccumulator::new(&DataType::String, 2).unwrap();
        b.update(&[0; 3], 1, &[&col_b], None).unwrap();
        a.merge(&[0], 1, Box::new(b)).unwrap();
        let result = Box::new(a).finish(1).unwrap();
        assert_eq!(top_values(&result), vec!["c".to_string(), "a".to_string()]);
    }

    #[test]
    fn new_rejects_k_zero() {
        assert!(TopKAccumulator::new(&DataType::Int64, 0).is_err());
    }

    #[test]
    fn new_rejects_list_type() {
        let list_ty = DataType::list(DataType::Int64).unwrap();
        assert!(TopKAccumulator::new(&list_ty, 1).is_err());
    }

    #[test]
    fn encoded_round_trip_is_exact() {
        let col = col_of(&["a", "a", "b", "c", "c", "c"]);
        let mut a = TopKAccumulator::new(&DataType::String, 2).unwrap();
        a.update(&[0; 6], 1, &[&col], None).unwrap();
        let mut bytes = Vec::new();
        a.encode_state(0, &mut bytes);
        let mut b = TopKAccumulator::new(&DataType::String, 2).unwrap();
        b.merge_encoded(0, 1, &bytes).unwrap();
        let result = Box::new(b).finish(1).unwrap();
        assert_eq!(top_values(&result), vec!["c".to_string(), "a".to_string()]);
    }

    #[test]
    fn merge_encoded_rejects_garbage() {
        let mut acc = TopKAccumulator::new(&DataType::Int64, 2).unwrap();
        acc.grow(1);
        assert!(acc.merge_encoded(0, 1, &[]).is_err());
        assert!(acc.merge_encoded(0, 1, &[9]).is_err());
        // Right version, but a k that doesn't match this accumulator's.
        let mut sink = Sink::new();
        sink.u8(1);
        sink.uvarint(99);
        sink.uvarint(0);
        assert!(acc.merge_encoded(0, 1, &sink.into_vec()).is_err());
    }
}
