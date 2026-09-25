//! `ListAgg`, `Quantile` and `Histogram`: the collecting accumulators (U3, SPEC §7, §8).
//! Each keeps every row it sees (or a value→count map), so `finish` does the real work.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;

use crate::exec::kernels::rowkey::{NullKeys, encode_row_key};
use crate::exec::{Bitmap, Column, ColumnBuilder, ExecError};
use crate::storage::segment::value::{decode_opt, decode_value, encode_opt, encode_value};
use crate::storage::segment::wire::{Cursor, Sink};
use crate::types::{DataType, Value, total_cmp};

use super::GroupsAccumulator;
use super::basic::{check_version, downcast, ensure_len, invalid};

const STATE_VERSION: u8 = 1;

/// `LIST_AGG`: every row the filter keeps, NULL elements included, in arrival order within
/// a partial. `merge` appends `other`'s elements after `self`'s. A group with no rows is NULL.
pub(crate) struct ListAggAccumulator {
    elem_ty: DataType,
    states: Vec<Vec<Option<Value>>>,
}

impl ListAggAccumulator {
    pub(crate) fn new(ty: &DataType) -> Result<Self, ExecError> {
        if matches!(ty, DataType::List(_)) {
            return Err(ExecError::Plan("list_agg does not support LIST".into()));
        }
        Ok(ListAggAccumulator {
            elem_ty: ty.clone(),
            states: Vec::new(),
        })
    }
}

impl GroupsAccumulator for ListAggAccumulator {
    fn update(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        args: &[&Column],
        filter: Option<&Bitmap>,
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.states, total_groups, Vec::new());
        let col = args[0];
        for (r, &g) in groups.iter().enumerate() {
            if filter.is_some_and(|f| !f.get(r)) {
                continue;
            }
            let v = col.get(r);
            self.states[g].push(if v.is_null() { None } else { Some(v) });
        }
        Ok(())
    }

    fn merge(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        other: Box<dyn GroupsAccumulator>,
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.states, total_groups, Vec::new());
        let mut other = downcast::<Self>(other)?;
        for (g, &target) in groups.iter().enumerate() {
            if let Some(elems) = other.states.get_mut(g) {
                self.states[target].append(elems);
            }
        }
        Ok(())
    }

    fn finish(self: Box<Self>, total_groups: usize) -> Result<Column, ExecError> {
        let mut this = *self;
        ensure_len(&mut this.states, total_groups, Vec::new());
        let list_ty = DataType::list(this.elem_ty.clone()).expect("elem type is never LIST");
        let mut builder = ColumnBuilder::new(list_ty);
        for g in 0..total_groups {
            if this.states[g].is_empty() {
                builder.push(&Value::Null)?;
            } else {
                let elems: Vec<Value> = this.states[g]
                    .iter()
                    .map(|e| e.clone().unwrap_or(Value::Null))
                    .collect();
                builder.push(&Value::List(elems))?;
            }
        }
        Ok(builder.finish())
    }

    fn data_type(&self) -> DataType {
        DataType::list(self.elem_ty.clone()).expect("elem type is never LIST")
    }

    fn encode_state(&self, group: usize, out: &mut Vec<u8>) {
        let mut s = Sink::new();
        s.u8(STATE_VERSION);
        let empty = Vec::new();
        let elems = self.states.get(group).unwrap_or(&empty);
        s.uvarint(elems.len() as u64);
        for e in elems {
            encode_opt(e.as_ref(), &self.elem_ty, &mut s);
        }
        out.extend_from_slice(&s.into_vec());
    }

    fn merge_encoded(
        &mut self,
        group: usize,
        total_groups: usize,
        bytes: &[u8],
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.states, total_groups, Vec::new());
        let mut cur = Cursor::new(bytes);
        check_version(&mut cur, "list_agg")?;
        let n = cur.uvarint().map_err(|e| invalid("list_agg", e))?;
        let n = cur.guard_len(n, 1).map_err(|e| invalid("list_agg", e))?;
        for _ in 0..n {
            let v = decode_opt(&mut cur, &self.elem_ty).map_err(|e| invalid("list_agg", e))?;
            self.states[group].push(v);
        }
        Ok(())
    }

    fn byte_size(&self) -> usize {
        self.states
            .iter()
            .map(|v| v.capacity() * size_of::<Option<Value>>())
            .sum()
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

/// `QUANTILE(q)`: every non-null value, `total_cmp`-sorted, then the discrete lower
/// nearest-rank element at `floor(q * (n - 1))`. A group with no rows is NULL.
pub(crate) struct QuantileAccumulator {
    ty: DataType,
    q: f64,
    states: Vec<Vec<Value>>,
}

impl QuantileAccumulator {
    pub(crate) fn new(ty: &DataType, q: f64) -> Result<Self, ExecError> {
        if q.is_nan() || !(0.0..=1.0).contains(&q) {
            return Err(ExecError::Invalid(format!(
                "quantile q {q} out of range [0,1]"
            )));
        }
        let supported = matches!(
            ty,
            DataType::Int64
                | DataType::UInt64
                | DataType::Float64
                | DataType::Decimal(_)
                | DataType::Timestamp
                | DataType::Date
        );
        if !supported {
            return Err(ExecError::Plan(format!("quantile does not support {ty}")));
        }
        Ok(QuantileAccumulator {
            ty: ty.clone(),
            q,
            states: Vec::new(),
        })
    }
}

impl GroupsAccumulator for QuantileAccumulator {
    fn update(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        args: &[&Column],
        filter: Option<&Bitmap>,
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.states, total_groups, Vec::new());
        let col = args[0];
        for (r, &g) in groups.iter().enumerate() {
            if filter.is_some_and(|f| !f.get(r)) || col.is_null(r) {
                continue;
            }
            self.states[g].push(col.get(r));
        }
        Ok(())
    }

    fn merge(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        other: Box<dyn GroupsAccumulator>,
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.states, total_groups, Vec::new());
        let mut other = downcast::<Self>(other)?;
        for (g, &target) in groups.iter().enumerate() {
            if let Some(vals) = other.states.get_mut(g) {
                self.states[target].append(vals);
            }
        }
        Ok(())
    }

    fn finish(self: Box<Self>, total_groups: usize) -> Result<Column, ExecError> {
        let mut this = *self;
        ensure_len(&mut this.states, total_groups, Vec::new());
        let mut values = Vec::with_capacity(total_groups);
        for g in 0..total_groups {
            let vals = &mut this.states[g];
            if vals.is_empty() {
                values.push(Value::Null);
                continue;
            }
            vals.sort_by(|a, b| total_cmp(a, b).expect("same-typed values always compare"));
            let idx = ((this.q * (vals.len() - 1) as f64).floor() as usize).min(vals.len() - 1);
            values.push(vals[idx].clone());
        }
        Ok(Column::from_values(&this.ty, &values)?)
    }

    fn data_type(&self) -> DataType {
        self.ty.clone()
    }

    fn encode_state(&self, group: usize, out: &mut Vec<u8>) {
        let mut s = Sink::new();
        s.u8(STATE_VERSION);
        let empty = Vec::new();
        let vals = self.states.get(group).unwrap_or(&empty);
        s.uvarint(vals.len() as u64);
        for v in vals {
            encode_value(v, &self.ty, &mut s);
        }
        out.extend_from_slice(&s.into_vec());
    }

    fn merge_encoded(
        &mut self,
        group: usize,
        total_groups: usize,
        bytes: &[u8],
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.states, total_groups, Vec::new());
        let mut cur = Cursor::new(bytes);
        check_version(&mut cur, "quantile")?;
        let n = cur.uvarint().map_err(|e| invalid("quantile", e))?;
        let n = cur.guard_len(n, 1).map_err(|e| invalid("quantile", e))?;
        for _ in 0..n {
            let v = decode_value(&mut cur, &self.ty).map_err(|e| invalid("quantile", e))?;
            self.states[group].push(v);
        }
        Ok(())
    }

    fn byte_size(&self) -> usize {
        self.states
            .iter()
            .map(|v| v.capacity() * size_of::<Value>())
            .sum()
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

/// `COUNT(DISTINCT x)`: an exact set of `encode_row_key`-encoded keys per group (any
/// non-LIST type). NULLs are ignored. A group nothing reaches is 0, not NULL.
pub(crate) struct CountDistinctAccumulator {
    states: Vec<BTreeSet<Vec<u8>>>,
}

impl CountDistinctAccumulator {
    pub(crate) fn new(ty: &DataType) -> Result<Self, ExecError> {
        if matches!(ty, DataType::List(_)) {
            return Err(ExecError::Plan(
                "count(distinct) does not support LIST".into(),
            ));
        }
        Ok(CountDistinctAccumulator { states: Vec::new() })
    }
}

impl GroupsAccumulator for CountDistinctAccumulator {
    fn update(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        args: &[&Column],
        filter: Option<&Bitmap>,
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.states, total_groups, BTreeSet::new());
        let col = args[0];
        for (r, &g) in groups.iter().enumerate() {
            if filter.is_some_and(|f| !f.get(r)) || col.is_null(r) {
                continue;
            }
            let mut key = Vec::new();
            let _ = encode_row_key(&[col], r, NullKeys::Group, &mut key);
            self.states[g].insert(key);
        }
        Ok(())
    }

    fn merge(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        other: Box<dyn GroupsAccumulator>,
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.states, total_groups, BTreeSet::new());
        let other = downcast::<Self>(other)?;
        for (g, &target) in groups.iter().enumerate() {
            if let Some(set) = other.states.get(g) {
                self.states[target].extend(set.iter().cloned());
            }
        }
        Ok(())
    }

    fn finish(self: Box<Self>, total_groups: usize) -> Result<Column, ExecError> {
        let mut this = *self;
        ensure_len(&mut this.states, total_groups, BTreeSet::new());
        let values: Vec<Value> = (0..total_groups)
            .map(|g| Value::Int64(this.states[g].len() as i64))
            .collect();
        Ok(Column::from_values(&DataType::Int64, &values)?)
    }

    fn data_type(&self) -> DataType {
        DataType::Int64
    }

    fn encode_state(&self, group: usize, out: &mut Vec<u8>) {
        let mut s = Sink::new();
        s.u8(STATE_VERSION);
        let empty = BTreeSet::new();
        let set = self.states.get(group).unwrap_or(&empty);
        s.uvarint(set.len() as u64);
        for key in set {
            s.bytes(key);
        }
        out.extend_from_slice(&s.into_vec());
    }

    fn merge_encoded(
        &mut self,
        group: usize,
        total_groups: usize,
        bytes: &[u8],
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.states, total_groups, BTreeSet::new());
        let mut cur = Cursor::new(bytes);
        check_version(&mut cur, "count_distinct")?;
        let n = cur.uvarint().map_err(|e| invalid("count_distinct", e))?;
        let n = cur
            .guard_len(n, 1)
            .map_err(|e| invalid("count_distinct", e))?;
        for _ in 0..n {
            let key = cur.bytes().map_err(|e| invalid("count_distinct", e))?;
            self.states[group].insert(key.to_vec());
        }
        Ok(())
    }

    fn byte_size(&self) -> usize {
        self.states
            .iter()
            .map(|set| {
                set.iter()
                    .map(|k| k.capacity() + size_of::<Vec<u8>>())
                    .sum::<usize>()
            })
            .sum()
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

/// `HISTOGRAM`: an exact value→count map (any non-LIST type), rendered as DuckDB-style MAP
/// text `{v1=c1, v2=c2}`, ascending by `total_cmp`; NULL keys are counted and sort last.
pub(crate) struct HistogramAccumulator {
    ty: DataType,
    states: Vec<BTreeMap<Vec<u8>, (Value, u64)>>,
}

impl HistogramAccumulator {
    pub(crate) fn new(ty: &DataType) -> Result<Self, ExecError> {
        if matches!(ty, DataType::List(_)) {
            return Err(ExecError::Plan("histogram does not support LIST".into()));
        }
        Ok(HistogramAccumulator {
            ty: ty.clone(),
            states: Vec::new(),
        })
    }

    /// The map key for a single value: wraps it in a one-row column so `encode_row_key`
    /// (which canonicalises NaN and -0.0/0.0) can key both live rows and decoded states.
    fn key_of(&self, v: &Value) -> Vec<u8> {
        let col = Column::from_values(&self.ty, std::slice::from_ref(v))
            .expect("value already matches the histogram's own type");
        let mut key = Vec::new();
        let _ = encode_row_key(&[&col], 0, NullKeys::Group, &mut key);
        key
    }
}

impl GroupsAccumulator for HistogramAccumulator {
    fn update(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        args: &[&Column],
        filter: Option<&Bitmap>,
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.states, total_groups, BTreeMap::new());
        let col = args[0];
        for (r, &g) in groups.iter().enumerate() {
            if filter.is_some_and(|f| !f.get(r)) {
                continue;
            }
            let v = col.get(r);
            let mut key = Vec::new();
            let _ = encode_row_key(&[col], r, NullKeys::Group, &mut key);
            self.states[g].entry(key).or_insert_with(|| (v, 0)).1 += 1;
        }
        Ok(())
    }

    fn merge(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        other: Box<dyn GroupsAccumulator>,
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.states, total_groups, BTreeMap::new());
        let other = downcast::<Self>(other)?;
        for (g, &target) in groups.iter().enumerate() {
            if let Some(map) = other.states.get(g) {
                for (k, (v, c)) in map {
                    self.states[target]
                        .entry(k.clone())
                        .or_insert_with(|| (v.clone(), 0))
                        .1 += c;
                }
            }
        }
        Ok(())
    }

    fn finish(self: Box<Self>, total_groups: usize) -> Result<Column, ExecError> {
        let mut this = *self;
        ensure_len(&mut this.states, total_groups, BTreeMap::new());
        let mut values = Vec::with_capacity(total_groups);
        for g in 0..total_groups {
            let map = &this.states[g];
            if map.is_empty() {
                values.push(Value::Null);
                continue;
            }
            let mut entries: Vec<(Value, u64)> = map.values().cloned().collect();
            entries.sort_by(|a, b| match (a.0.is_null(), b.0.is_null()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => total_cmp(&a.0, &b.0).expect("same-typed values always compare"),
            });
            let rendered: Vec<String> = entries
                .iter()
                .map(|(v, c)| format!("{}={c}", v.to_text().unwrap_or_else(|| "NULL".to_string())))
                .collect();
            values.push(Value::String(format!("{{{}}}", rendered.join(", "))));
        }
        Ok(Column::from_values(&DataType::String, &values)?)
    }

    fn data_type(&self) -> DataType {
        DataType::String
    }

    fn encode_state(&self, group: usize, out: &mut Vec<u8>) {
        let mut s = Sink::new();
        s.u8(STATE_VERSION);
        let empty = BTreeMap::new();
        let map = self.states.get(group).unwrap_or(&empty);
        s.uvarint(map.len() as u64);
        for (v, c) in map.values() {
            let opt = if v.is_null() { None } else { Some(v) };
            encode_opt(opt, &self.ty, &mut s);
            s.uvarint(*c);
        }
        out.extend_from_slice(&s.into_vec());
    }

    fn merge_encoded(
        &mut self,
        group: usize,
        total_groups: usize,
        bytes: &[u8],
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.states, total_groups, BTreeMap::new());
        let mut cur = Cursor::new(bytes);
        check_version(&mut cur, "histogram")?;
        let n = cur.uvarint().map_err(|e| invalid("histogram", e))?;
        let n = cur.guard_len(n, 1).map_err(|e| invalid("histogram", e))?;
        for _ in 0..n {
            let v = decode_opt(&mut cur, &self.ty)
                .map_err(|e| invalid("histogram", e))?
                .unwrap_or(Value::Null);
            let c = cur.uvarint().map_err(|e| invalid("histogram", e))?;
            let key = self.key_of(&v);
            self.states[group].entry(key).or_insert_with(|| (v, 0)).1 += c;
        }
        Ok(())
    }

    fn byte_size(&self) -> usize {
        self.states
            .iter()
            .map(|m| {
                m.keys()
                    .map(|k| k.capacity() + size_of::<Value>() + 8)
                    .sum::<usize>()
            })
            .sum()
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::super::basic::{assert_merge_encoded_rejects_garbage, check_split};
    use super::*;

    fn col(ty: &DataType, values: &[Value]) -> Column {
        Column::from_values(ty, values).unwrap()
    }

    #[test]
    fn list_agg_1_null_2_keeps_nulls_and_order() {
        let c = col(
            &DataType::Int64,
            &[Value::Int64(1), Value::Null, Value::Int64(2)],
        );
        let groups = [0, 0, 0];
        let mut acc = ListAggAccumulator::new(&DataType::Int64).unwrap();
        acc.update(&groups, 1, &[&c], None).unwrap();
        let got = Box::new(acc).finish(1).unwrap().get(0);
        assert_eq!(
            got,
            Value::List(vec![Value::Int64(1), Value::Null, Value::Int64(2)])
        );
    }

    #[test]
    fn list_agg_rejects_list() {
        let list_ty = DataType::list(DataType::Int64).unwrap();
        assert!(matches!(
            ListAggAccumulator::new(&list_ty),
            Err(ExecError::Plan(_))
        ));
    }

    #[test]
    fn list_agg_check_split() {
        let c = col(
            &DataType::Int64,
            &[
                Value::Int64(1),
                Value::Null,
                Value::Int64(3),
                Value::Int64(4),
                Value::Int64(5),
                Value::Int64(6),
            ],
        );
        let groups = [0, 1, 0, 2, 1, 0];
        check_split(
            || Box::new(ListAggAccumulator::new(&DataType::Int64).unwrap()),
            &[c],
            None,
            &groups,
            4,
        );
    }

    #[test]
    fn quantile_median_of_four_values() {
        let c = col(
            &DataType::Int64,
            &[
                Value::Int64(1),
                Value::Int64(2),
                Value::Int64(3),
                Value::Int64(4),
            ],
        );
        let groups = [0, 0, 0, 0];
        let mut acc = QuantileAccumulator::new(&DataType::Int64, 0.5).unwrap();
        acc.update(&groups, 1, &[&c], None).unwrap();
        assert_eq!(Box::new(acc).finish(1).unwrap().get(0), Value::Int64(2));
    }

    #[test]
    fn quantile_zero_is_min_one_is_max() {
        let c = col(
            &DataType::Int64,
            &[
                Value::Int64(1),
                Value::Int64(2),
                Value::Int64(3),
                Value::Int64(4),
            ],
        );
        let groups = [0, 0, 0, 0];
        let mut lo = QuantileAccumulator::new(&DataType::Int64, 0.0).unwrap();
        lo.update(&groups, 1, &[&c], None).unwrap();
        assert_eq!(Box::new(lo).finish(1).unwrap().get(0), Value::Int64(1));

        let mut hi = QuantileAccumulator::new(&DataType::Int64, 1.0).unwrap();
        hi.update(&groups, 1, &[&c], None).unwrap();
        assert_eq!(Box::new(hi).finish(1).unwrap().get(0), Value::Int64(4));
    }

    #[test]
    fn quantile_rejects_q_outside_unit_range() {
        assert!(matches!(
            QuantileAccumulator::new(&DataType::Int64, 1.5),
            Err(ExecError::Invalid(_))
        ));
        assert!(matches!(
            QuantileAccumulator::new(&DataType::Int64, f64::NAN),
            Err(ExecError::Invalid(_))
        ));
    }

    #[test]
    fn quantile_check_split() {
        let c = col(
            &DataType::Int64,
            &[
                Value::Int64(3),
                Value::Int64(1),
                Value::Int64(9),
                Value::Int64(4),
                Value::Int64(2),
                Value::Int64(7),
            ],
        );
        let groups = [0, 1, 0, 2, 1, 0];
        check_split(
            || Box::new(QuantileAccumulator::new(&DataType::Int64, 0.5).unwrap()),
            &[c],
            None,
            &groups,
            4,
        );
    }

    #[test]
    fn histogram_of_b_a_b_null() {
        let c = col(
            &DataType::String,
            &[
                Value::String("b".into()),
                Value::String("a".into()),
                Value::String("b".into()),
                Value::Null,
            ],
        );
        let groups = [0, 0, 0, 0];
        let mut acc = HistogramAccumulator::new(&DataType::String).unwrap();
        acc.update(&groups, 1, &[&c], None).unwrap();
        assert_eq!(
            Box::new(acc).finish(1).unwrap().get(0),
            Value::String("{a=1, b=2, NULL=1}".into())
        );
    }

    #[test]
    fn histogram_rejects_list() {
        let list_ty = DataType::list(DataType::Int64).unwrap();
        assert!(matches!(
            HistogramAccumulator::new(&list_ty),
            Err(ExecError::Plan(_))
        ));
    }

    #[test]
    fn histogram_check_split() {
        let c = col(
            &DataType::Int64,
            &[
                Value::Int64(1),
                Value::Int64(2),
                Value::Int64(1),
                Value::Null,
                Value::Int64(2),
                Value::Int64(1),
            ],
        );
        let groups = [0, 1, 0, 2, 1, 0];
        check_split(
            || Box::new(HistogramAccumulator::new(&DataType::Int64).unwrap()),
            &[c],
            None,
            &groups,
            4,
        );
    }

    #[test]
    fn list_agg_quantile_histogram_merge_encoded_rejects_garbage() {
        assert_merge_encoded_rejects_garbage(|| {
            Box::new(ListAggAccumulator::new(&DataType::Int64).unwrap())
        });
        assert_merge_encoded_rejects_garbage(|| {
            Box::new(QuantileAccumulator::new(&DataType::Int64, 0.5).unwrap())
        });
        assert_merge_encoded_rejects_garbage(|| {
            Box::new(HistogramAccumulator::new(&DataType::Int64).unwrap())
        });
    }

    #[test]
    fn count_distinct_1_1_2_null_3_ignores_null_and_dedupes() {
        let c = col(
            &DataType::Int64,
            &[
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(2),
                Value::Null,
                Value::Int64(3),
            ],
        );
        let groups = [0, 0, 0, 0, 0];
        let mut acc = CountDistinctAccumulator::new(&DataType::Int64).unwrap();
        acc.update(&groups, 1, &[&c], None).unwrap();
        assert_eq!(Box::new(acc).finish(1).unwrap().get(0), Value::Int64(3));
    }

    #[test]
    fn count_distinct_empty_group_is_zero_not_null() {
        let c = col(&DataType::Int64, &[Value::Int64(1), Value::Int64(2)]);
        let groups = [0, 0];
        let mut acc = CountDistinctAccumulator::new(&DataType::Int64).unwrap();
        acc.update(&groups, 2, &[&c], None).unwrap();
        let got = Box::new(acc).finish(2).unwrap();
        assert_eq!(got.get(0), Value::Int64(2));
        assert_eq!(got.get(1), Value::Int64(0));
    }

    #[test]
    fn count_distinct_merge_unions_overlapping_keys() {
        let c1 = col(&DataType::Int64, &[Value::Int64(1), Value::Int64(2)]);
        let c2 = col(&DataType::Int64, &[Value::Int64(2), Value::Int64(3)]);
        let mut a = CountDistinctAccumulator::new(&DataType::Int64).unwrap();
        a.update(&[0, 0], 1, &[&c1], None).unwrap();
        let mut b = CountDistinctAccumulator::new(&DataType::Int64).unwrap();
        b.update(&[0, 0], 1, &[&c2], None).unwrap();
        a.merge(&[0], 1, Box::new(b)).unwrap();
        // A count-sum merge would give 4; the union gives 3.
        assert_eq!(Box::new(a).finish(1).unwrap().get(0), Value::Int64(3));
    }

    #[test]
    fn count_distinct_filter_excludes_rows() {
        let c = col(
            &DataType::Int64,
            &[Value::Int64(1), Value::Int64(2), Value::Int64(3)],
        );
        let mut filter = Bitmap::new_valid(3);
        filter.set(1, false);
        let groups = [0, 0, 0];
        let mut acc = CountDistinctAccumulator::new(&DataType::Int64).unwrap();
        acc.update(&groups, 1, &[&c], Some(&filter)).unwrap();
        assert_eq!(Box::new(acc).finish(1).unwrap().get(0), Value::Int64(2));
    }

    #[test]
    fn count_distinct_rejects_list() {
        let list_ty = DataType::list(DataType::Int64).unwrap();
        assert!(matches!(
            CountDistinctAccumulator::new(&list_ty),
            Err(ExecError::Plan(_))
        ));
    }

    #[test]
    fn count_distinct_check_split() {
        let c = col(
            &DataType::Int64,
            &[
                Value::Int64(1),
                Value::Int64(2),
                Value::Int64(1),
                Value::Null,
                Value::Int64(2),
                Value::Int64(1),
            ],
        );
        let groups = [0, 1, 0, 2, 1, 0];
        check_split(
            || Box::new(CountDistinctAccumulator::new(&DataType::Int64).unwrap()),
            &[c],
            None,
            &groups,
            4,
        );
    }

    #[test]
    fn count_distinct_merge_encoded_rejects_garbage() {
        assert_merge_encoded_rejects_garbage(|| {
            Box::new(CountDistinctAccumulator::new(&DataType::Int64).unwrap())
        });
    }

    #[test]
    fn count_distinct_normalises_float_negative_zero_and_distinguishes_trailing_space() {
        let floats = col(
            &DataType::Float64,
            &[Value::Float64(-0.0), Value::Float64(0.0)],
        );
        let mut fa = CountDistinctAccumulator::new(&DataType::Float64).unwrap();
        fa.update(&[0, 0], 1, &[&floats], None).unwrap();
        assert_eq!(Box::new(fa).finish(1).unwrap().get(0), Value::Int64(1));

        let strings = col(
            &DataType::String,
            &[Value::String("a".into()), Value::String("a ".into())],
        );
        let mut sa = CountDistinctAccumulator::new(&DataType::String).unwrap();
        sa.update(&[0, 0], 1, &[&strings], None).unwrap();
        assert_eq!(Box::new(sa).finish(1).unwrap().get(0), Value::Int64(2));
    }
}
