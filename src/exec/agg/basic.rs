//! `Count`, `Sum`, `Avg`, `MinMax` and `Arg`: the plain per-group accumulators (U3, SPEC §7,
//! §8). Every state is a flat `Vec` indexed by group id, resized lazily to `total_groups`.

use std::cmp::Ordering;
use std::mem::size_of;

use crate::exec::{Bitmap, Column, ExecError};
use crate::storage::segment::value::{decode_opt, decode_value, encode_opt, encode_value};
use crate::storage::segment::wire::{Cursor, Sink};
use crate::types::{DataType, Decimal, Value, total_cmp};

use super::GroupsAccumulator;

const STATE_VERSION: u8 = 1;

/// Downcasts a boxed accumulator to `T`; every `merge` mirrors U1's pattern.
pub(super) fn downcast<T: 'static>(other: Box<dyn GroupsAccumulator>) -> Result<Box<T>, ExecError> {
    other
        .into_any()
        .downcast::<T>()
        .map_err(|_| ExecError::Plan("merge of mismatched accumulators".into()))
}

/// Grows `v` to `total_groups`, filling new slots with `init` (U1's growth pattern).
pub(super) fn ensure_len<T: Clone>(v: &mut Vec<T>, total_groups: usize, init: T) {
    if v.len() < total_groups {
        v.resize(total_groups, init);
    }
}

/// A malformed or truncated encoded state never panics (D0008); it is always `Invalid`.
pub(super) fn invalid<E: std::fmt::Debug>(what: &str, err: E) -> ExecError {
    ExecError::Invalid(format!("{what} state: {err:?}"))
}

/// Reads and checks the leading format-version byte every encoded state starts with.
pub(super) fn check_version(cur: &mut Cursor, what: &str) -> Result<(), ExecError> {
    let version = cur.u8().map_err(|e| invalid(what, e))?;
    if version != STATE_VERSION {
        return Err(ExecError::Invalid(format!(
            "{what} state: unknown version {version}"
        )));
    }
    Ok(())
}

/// `COUNT` / `COUNT(*)`: rows per group. Body: uvarint count. `Count` skips NULL args;
/// `CountStar` counts every row. Both honour `filter`.
pub(crate) struct CountAccumulator {
    count_star: bool,
    counts: Vec<u64>,
}

impl CountAccumulator {
    pub(crate) fn new(count_star: bool) -> Self {
        CountAccumulator {
            count_star,
            counts: Vec::new(),
        }
    }
}

impl GroupsAccumulator for CountAccumulator {
    fn update(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        args: &[&Column],
        filter: Option<&Bitmap>,
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.counts, total_groups, 0);
        for (r, &g) in groups.iter().enumerate() {
            if filter.is_some_and(|f| !f.get(r)) {
                continue;
            }
            if self.count_star || !args[0].is_null(r) {
                self.counts[g] += 1;
            }
        }
        Ok(())
    }

    fn merge(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        other: Box<dyn GroupsAccumulator>,
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.counts, total_groups, 0);
        let other = downcast::<Self>(other)?;
        for (g, &target) in groups.iter().enumerate() {
            self.counts[target] += other.counts.get(g).copied().unwrap_or(0);
        }
        Ok(())
    }

    fn finish(self: Box<Self>, total_groups: usize) -> Result<Column, ExecError> {
        let mut this = *self;
        ensure_len(&mut this.counts, total_groups, 0);
        let values: Vec<Value> = this.counts[..total_groups]
            .iter()
            .map(|&c| Value::Int64(c as i64))
            .collect();
        Ok(Column::from_values(&DataType::Int64, &values)?)
    }

    fn data_type(&self) -> DataType {
        DataType::Int64
    }

    fn encode_state(&self, group: usize, out: &mut Vec<u8>) {
        let mut s = Sink::new();
        s.u8(STATE_VERSION);
        s.uvarint(self.counts.get(group).copied().unwrap_or(0));
        out.extend_from_slice(&s.into_vec());
    }

    fn merge_encoded(
        &mut self,
        group: usize,
        total_groups: usize,
        bytes: &[u8],
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.counts, total_groups, 0);
        let mut cur = Cursor::new(bytes);
        check_version(&mut cur, "count")?;
        let count = cur.uvarint().map_err(|e| invalid("count", e))?;
        self.counts[group] += count;
        Ok(())
    }

    fn byte_size(&self) -> usize {
        self.counts.capacity() * size_of::<u64>()
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

/// `SUM`'s single-kind running total, shared by `Sum` and `Avg` (both use the same wire
/// shape: `i128` for integer/DECIMAL/UINT64 sums, `f64` bits for FLOAT64).
#[derive(Clone, Copy)]
enum SumKind {
    Int64,
    UInt64,
    Float64,
    Decimal(u8),
}

impl SumKind {
    fn of(ty: &DataType) -> Option<Self> {
        match ty {
            DataType::Int64 => Some(SumKind::Int64),
            DataType::UInt64 => Some(SumKind::UInt64),
            DataType::Float64 => Some(SumKind::Float64),
            DataType::Decimal(dt) => Some(SumKind::Decimal(dt.scale())),
            _ => None,
        }
    }
}

/// `SUM`: INT64→INT64, UINT64→UINT64, FLOAT64→FLOAT64, DECIMAL(p,s)→DECIMAL(38,s). Checked
/// add; overflow is `Overflow("sum(<TYPE>)")`. A group with no rows finishes NULL.
pub(crate) struct SumAccumulator {
    kind: SumKind,
    has: Vec<bool>,
    int_sum: Vec<i128>,
    float_sum: Vec<f64>,
}

impl SumAccumulator {
    pub(crate) fn new(ty: &DataType) -> Result<Self, ExecError> {
        let kind =
            SumKind::of(ty).ok_or_else(|| ExecError::Plan(format!("sum does not support {ty}")))?;
        Ok(SumAccumulator {
            kind,
            has: Vec::new(),
            int_sum: Vec::new(),
            float_sum: Vec::new(),
        })
    }

    fn grow(&mut self, total_groups: usize) {
        ensure_len(&mut self.has, total_groups, false);
        match self.kind {
            SumKind::Float64 => ensure_len(&mut self.float_sum, total_groups, 0.0),
            _ => ensure_len(&mut self.int_sum, total_groups, 0),
        }
    }

    /// Checked add into group `g`'s running `i128` total, range-checked against the output
    /// kind so a genuine INT64/UINT64 overflow surfaces even mid-accumulation.
    fn add_int(&mut self, g: usize, v: i128, op: &str) -> Result<(), ExecError> {
        let sum = self.int_sum[g]
            .checked_add(v)
            .ok_or_else(|| ExecError::Overflow(op.to_string()))?;
        let in_range = match self.kind {
            SumKind::Int64 => (i64::MIN as i128..=i64::MAX as i128).contains(&sum),
            SumKind::UInt64 => (0..=u64::MAX as i128).contains(&sum),
            _ => true,
        };
        if !in_range {
            return Err(ExecError::Overflow(op.to_string()));
        }
        self.int_sum[g] = sum;
        self.has[g] = true;
        Ok(())
    }

    fn op_name(&self) -> &'static str {
        match self.kind {
            SumKind::Int64 => "sum(INT64)",
            SumKind::UInt64 => "sum(UINT64)",
            SumKind::Float64 => "sum(FLOAT64)",
            SumKind::Decimal(_) => "sum(DECIMAL)",
        }
    }
}

impl GroupsAccumulator for SumAccumulator {
    fn update(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        args: &[&Column],
        filter: Option<&Bitmap>,
    ) -> Result<(), ExecError> {
        self.grow(total_groups);
        let col = args[0];
        let op = self.op_name();
        for (r, &g) in groups.iter().enumerate() {
            if filter.is_some_and(|f| !f.get(r)) || col.is_null(r) {
                continue;
            }
            match (self.kind, col.get(r)) {
                (SumKind::Float64, Value::Float64(v)) => {
                    self.float_sum[g] += v;
                    self.has[g] = true;
                }
                (SumKind::Int64, Value::Int64(v)) => self.add_int(g, v as i128, op)?,
                (SumKind::UInt64, Value::UInt64(v)) => self.add_int(g, v as i128, op)?,
                (SumKind::Decimal(_), Value::Decimal(d)) => self.add_int(g, d.unscaled(), op)?,
                _ => unreachable!("sum arg type matches the accumulator's kind"),
            }
        }
        Ok(())
    }

    fn merge(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        other: Box<dyn GroupsAccumulator>,
    ) -> Result<(), ExecError> {
        self.grow(total_groups);
        let other = downcast::<Self>(other)?;
        let op = self.op_name();
        for (g, &target) in groups.iter().enumerate() {
            if !other.has.get(g).copied().unwrap_or(false) {
                continue;
            }
            match self.kind {
                SumKind::Float64 => {
                    self.float_sum[target] += other.float_sum.get(g).copied().unwrap_or(0.0);
                    self.has[target] = true;
                }
                _ => self.add_int(target, other.int_sum.get(g).copied().unwrap_or(0), op)?,
            }
        }
        Ok(())
    }

    fn finish(self: Box<Self>, total_groups: usize) -> Result<Column, ExecError> {
        let mut this = *self;
        this.grow(total_groups);
        let ty = this.data_type();
        let mut values = Vec::with_capacity(total_groups);
        for g in 0..total_groups {
            if !this.has[g] {
                values.push(Value::Null);
                continue;
            }
            values.push(match this.kind {
                SumKind::Int64 => Value::Int64(this.int_sum[g] as i64),
                SumKind::UInt64 => Value::UInt64(this.int_sum[g] as u64),
                SumKind::Float64 => Value::Float64(this.float_sum[g]),
                SumKind::Decimal(scale) => Value::Decimal(
                    Decimal::new(this.int_sum[g], scale)
                        .map_err(|_| ExecError::Overflow("sum(DECIMAL)".into()))?,
                ),
            });
        }
        Ok(Column::from_values(&ty, &values)?)
    }

    fn data_type(&self) -> DataType {
        match self.kind {
            SumKind::Int64 => DataType::Int64,
            SumKind::UInt64 => DataType::UInt64,
            SumKind::Float64 => DataType::Float64,
            SumKind::Decimal(scale) => {
                DataType::decimal(38, scale).expect("scale <= 38 fits DECIMAL(38,_)")
            }
        }
    }

    fn encode_state(&self, group: usize, out: &mut Vec<u8>) {
        let mut s = Sink::new();
        s.u8(STATE_VERSION);
        let has = self.has.get(group).copied().unwrap_or(false);
        s.u8(has as u8);
        if has {
            match self.kind {
                SumKind::Float64 => s.u64(self.float_sum[group].to_bits()),
                _ => s.i128(self.int_sum[group]),
            }
        }
        out.extend_from_slice(&s.into_vec());
    }

    fn merge_encoded(
        &mut self,
        group: usize,
        total_groups: usize,
        bytes: &[u8],
    ) -> Result<(), ExecError> {
        self.grow(total_groups);
        let mut cur = Cursor::new(bytes);
        check_version(&mut cur, "sum")?;
        let has = cur.u8().map_err(|e| invalid("sum", e))? != 0;
        if !has {
            return Ok(());
        }
        let op = self.op_name();
        match self.kind {
            SumKind::Float64 => {
                let bits = cur.u64().map_err(|e| invalid("sum", e))?;
                self.float_sum[group] += f64::from_bits(bits);
                self.has[group] = true;
            }
            _ => {
                let v = cur.i128().map_err(|e| invalid("sum", e))?;
                self.add_int(group, v, op)?;
            }
        }
        Ok(())
    }

    fn byte_size(&self) -> usize {
        self.has.capacity()
            + self.int_sum.capacity() * size_of::<i128>()
            + self.float_sum.capacity() * size_of::<f64>()
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

#[derive(Clone, Copy)]
enum AvgKind {
    IntLike,
    Decimal(u8),
    Float64,
}

/// `AVG`: INT64, UINT64, FLOAT64, DECIMAL → FLOAT64. State is (sum, count); integer and
/// DECIMAL sums accumulate in `i128` (no overflow before 2^64 rows). Finish is sum/count.
pub(crate) struct AvgAccumulator {
    kind: AvgKind,
    int_sum: Vec<i128>,
    float_sum: Vec<f64>,
    counts: Vec<u64>,
}

impl AvgAccumulator {
    pub(crate) fn new(ty: &DataType) -> Result<Self, ExecError> {
        let kind = match ty {
            DataType::Int64 | DataType::UInt64 => AvgKind::IntLike,
            DataType::Decimal(dt) => AvgKind::Decimal(dt.scale()),
            DataType::Float64 => AvgKind::Float64,
            _ => return Err(ExecError::Plan(format!("avg does not support {ty}"))),
        };
        Ok(AvgAccumulator {
            kind,
            int_sum: Vec::new(),
            float_sum: Vec::new(),
            counts: Vec::new(),
        })
    }

    fn grow(&mut self, total_groups: usize) {
        ensure_len(&mut self.counts, total_groups, 0);
        match self.kind {
            AvgKind::Float64 => ensure_len(&mut self.float_sum, total_groups, 0.0),
            _ => ensure_len(&mut self.int_sum, total_groups, 0),
        }
    }
}

impl GroupsAccumulator for AvgAccumulator {
    fn update(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        args: &[&Column],
        filter: Option<&Bitmap>,
    ) -> Result<(), ExecError> {
        self.grow(total_groups);
        let col = args[0];
        for (r, &g) in groups.iter().enumerate() {
            if filter.is_some_and(|f| !f.get(r)) || col.is_null(r) {
                continue;
            }
            match (self.kind, col.get(r)) {
                (AvgKind::Float64, Value::Float64(v)) => self.float_sum[g] += v,
                (AvgKind::Decimal(_), Value::Decimal(d)) => self.int_sum[g] += d.unscaled(),
                (AvgKind::IntLike, Value::Int64(v)) => self.int_sum[g] += v as i128,
                (AvgKind::IntLike, Value::UInt64(v)) => self.int_sum[g] += v as i128,
                _ => unreachable!("avg arg type matches the accumulator's kind"),
            }
            self.counts[g] += 1;
        }
        Ok(())
    }

    fn merge(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        other: Box<dyn GroupsAccumulator>,
    ) -> Result<(), ExecError> {
        self.grow(total_groups);
        let other = downcast::<Self>(other)?;
        for (g, &target) in groups.iter().enumerate() {
            let count = other.counts.get(g).copied().unwrap_or(0);
            if count == 0 {
                continue;
            }
            match self.kind {
                AvgKind::Float64 => {
                    self.float_sum[target] += other.float_sum.get(g).copied().unwrap_or(0.0)
                }
                _ => self.int_sum[target] += other.int_sum.get(g).copied().unwrap_or(0),
            }
            self.counts[target] += count;
        }
        Ok(())
    }

    fn finish(self: Box<Self>, total_groups: usize) -> Result<Column, ExecError> {
        let mut this = *self;
        this.grow(total_groups);
        let values: Vec<Value> = (0..total_groups)
            .map(|g| {
                let count = this.counts[g];
                if count == 0 {
                    return Value::Null;
                }
                let avg = match this.kind {
                    AvgKind::Float64 => this.float_sum[g] / count as f64,
                    AvgKind::Decimal(scale) => {
                        (this.int_sum[g] as f64) / crate::types::pow10(scale) as f64 / count as f64
                    }
                    AvgKind::IntLike => this.int_sum[g] as f64 / count as f64,
                };
                Value::Float64(avg)
            })
            .collect();
        Ok(Column::from_values(&DataType::Float64, &values)?)
    }

    fn data_type(&self) -> DataType {
        DataType::Float64
    }

    /// The sum as `Sum` encodes it, plus a trailing uvarint count.
    fn encode_state(&self, group: usize, out: &mut Vec<u8>) {
        let mut s = Sink::new();
        s.u8(STATE_VERSION);
        let count = self.counts.get(group).copied().unwrap_or(0);
        let has = count > 0;
        s.u8(has as u8);
        if has {
            match self.kind {
                AvgKind::Float64 => s.u64(self.float_sum[group].to_bits()),
                _ => s.i128(self.int_sum[group]),
            }
        }
        s.uvarint(count);
        out.extend_from_slice(&s.into_vec());
    }

    fn merge_encoded(
        &mut self,
        group: usize,
        total_groups: usize,
        bytes: &[u8],
    ) -> Result<(), ExecError> {
        self.grow(total_groups);
        let mut cur = Cursor::new(bytes);
        check_version(&mut cur, "avg")?;
        let has = cur.u8().map_err(|e| invalid("avg", e))? != 0;
        if has {
            match self.kind {
                AvgKind::Float64 => {
                    let bits = cur.u64().map_err(|e| invalid("avg", e))?;
                    self.float_sum[group] += f64::from_bits(bits);
                }
                _ => {
                    let v = cur.i128().map_err(|e| invalid("avg", e))?;
                    self.int_sum[group] += v;
                }
            }
        }
        let count = cur.uvarint().map_err(|e| invalid("avg", e))?;
        self.counts[group] += count;
        Ok(())
    }

    fn byte_size(&self) -> usize {
        self.int_sum.capacity() * size_of::<i128>()
            + self.float_sum.capacity() * size_of::<f64>()
            + self.counts.capacity() * size_of::<u64>()
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

/// `MIN` / `MAX`: any non-LIST type, `total_cmp` order (NaN greatest), NULLs skipped. State
/// is `Option<Value>` per group; a group with no rows finishes NULL.
pub(crate) struct MinMaxAccumulator {
    ty: DataType,
    is_max: bool,
    states: Vec<Option<Value>>,
}

impl MinMaxAccumulator {
    pub(crate) fn new(ty: &DataType, is_max: bool) -> Result<Self, ExecError> {
        if matches!(ty, DataType::List(_)) {
            let name = if is_max { "max" } else { "min" };
            return Err(ExecError::Plan(format!("{name} does not support LIST")));
        }
        Ok(MinMaxAccumulator {
            ty: ty.clone(),
            is_max,
            states: Vec::new(),
        })
    }

    /// Replaces `incumbent` with `challenger` iff `challenger` strictly wins the direction;
    /// a tie or a worse value keeps the incumbent (first-seen wins ties).
    fn consider(is_max: bool, incumbent: &mut Option<Value>, challenger: Value) {
        let replace = match incumbent {
            None => true,
            Some(cur) => {
                let ord = total_cmp(&challenger, cur).expect("same-typed values always compare");
                if is_max {
                    ord == Ordering::Greater
                } else {
                    ord == Ordering::Less
                }
            }
        };
        if replace {
            *incumbent = Some(challenger);
        }
    }
}

impl GroupsAccumulator for MinMaxAccumulator {
    fn update(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        args: &[&Column],
        filter: Option<&Bitmap>,
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.states, total_groups, None);
        let col = args[0];
        for (r, &g) in groups.iter().enumerate() {
            if filter.is_some_and(|f| !f.get(r)) || col.is_null(r) {
                continue;
            }
            Self::consider(self.is_max, &mut self.states[g], col.get(r));
        }
        Ok(())
    }

    fn merge(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        other: Box<dyn GroupsAccumulator>,
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.states, total_groups, None);
        let other = downcast::<Self>(other)?;
        for (g, &target) in groups.iter().enumerate() {
            if let Some(Some(v)) = other.states.get(g) {
                Self::consider(self.is_max, &mut self.states[target], v.clone());
            }
        }
        Ok(())
    }

    fn finish(self: Box<Self>, total_groups: usize) -> Result<Column, ExecError> {
        let mut this = *self;
        ensure_len(&mut this.states, total_groups, None);
        let values: Vec<Value> = this.states[..total_groups]
            .iter()
            .map(|v| v.clone().unwrap_or(Value::Null))
            .collect();
        Ok(Column::from_values(&this.ty, &values)?)
    }

    fn data_type(&self) -> DataType {
        self.ty.clone()
    }

    fn encode_state(&self, group: usize, out: &mut Vec<u8>) {
        let mut s = Sink::new();
        s.u8(STATE_VERSION);
        let v = self.states.get(group).and_then(|v| v.as_ref());
        encode_opt(v, &self.ty, &mut s);
        out.extend_from_slice(&s.into_vec());
    }

    fn merge_encoded(
        &mut self,
        group: usize,
        total_groups: usize,
        bytes: &[u8],
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.states, total_groups, None);
        let mut cur = Cursor::new(bytes);
        check_version(&mut cur, "minmax")?;
        if let Some(v) = decode_opt(&mut cur, &self.ty).map_err(|e| invalid("minmax", e))? {
            Self::consider(self.is_max, &mut self.states[group], v);
        }
        Ok(())
    }

    fn byte_size(&self) -> usize {
        self.states.capacity() * size_of::<Option<Value>>()
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

/// `ARG_MIN` / `ARG_MAX`: the `value` at the row with the winning `key` (any non-LIST key;
/// `value` is also non-LIST, so it stays within the state codec's value types).
pub(crate) struct ArgAccumulator {
    value_ty: DataType,
    key_ty: DataType,
    is_max: bool,
    states: Vec<Option<(Value, Option<Value>)>>,
}

impl ArgAccumulator {
    pub(crate) fn new(value: &DataType, key: &DataType, is_max: bool) -> Result<Self, ExecError> {
        let name = if is_max { "arg_max" } else { "arg_min" };
        if matches!(key, DataType::List(_)) {
            return Err(ExecError::Plan(format!("{name} key does not support LIST")));
        }
        if matches!(value, DataType::List(_)) {
            return Err(ExecError::Plan(format!(
                "{name} value does not support LIST"
            )));
        }
        Ok(ArgAccumulator {
            value_ty: value.clone(),
            key_ty: key.clone(),
            is_max,
            states: Vec::new(),
        })
    }

    /// A row with a NULL value but a winning key still wins (result becomes NULL); a tie
    /// keeps the incumbent, so the first row to reach a key wins that key's ties.
    fn consider(
        is_max: bool,
        incumbent: &mut Option<(Value, Option<Value>)>,
        key: Value,
        value: Option<Value>,
    ) {
        let replace = match incumbent {
            None => true,
            Some((cur_key, _)) => {
                let ord = total_cmp(&key, cur_key).expect("same-typed keys always compare");
                if is_max {
                    ord == Ordering::Greater
                } else {
                    ord == Ordering::Less
                }
            }
        };
        if replace {
            *incumbent = Some((key, value));
        }
    }
}

impl GroupsAccumulator for ArgAccumulator {
    fn update(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        args: &[&Column],
        filter: Option<&Bitmap>,
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.states, total_groups, None);
        let (value_col, key_col) = (args[0], args[1]);
        for (r, &g) in groups.iter().enumerate() {
            if filter.is_some_and(|f| !f.get(r)) || key_col.is_null(r) {
                continue;
            }
            let key = key_col.get(r);
            let value = value_col.get(r);
            let value = if value.is_null() { None } else { Some(value) };
            Self::consider(self.is_max, &mut self.states[g], key, value);
        }
        Ok(())
    }

    fn merge(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        other: Box<dyn GroupsAccumulator>,
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.states, total_groups, None);
        let other = downcast::<Self>(other)?;
        for (g, &target) in groups.iter().enumerate() {
            if let Some(Some((key, value))) = other.states.get(g) {
                Self::consider(
                    self.is_max,
                    &mut self.states[target],
                    key.clone(),
                    value.clone(),
                );
            }
        }
        Ok(())
    }

    fn finish(self: Box<Self>, total_groups: usize) -> Result<Column, ExecError> {
        let mut this = *self;
        ensure_len(&mut this.states, total_groups, None);
        let values: Vec<Value> = this.states[..total_groups]
            .iter()
            .map(|s| match s {
                None => Value::Null,
                Some((_, value)) => value.clone().unwrap_or(Value::Null),
            })
            .collect();
        Ok(Column::from_values(&this.value_ty, &values)?)
    }

    fn data_type(&self) -> DataType {
        self.value_ty.clone()
    }

    fn encode_state(&self, group: usize, out: &mut Vec<u8>) {
        let mut s = Sink::new();
        s.u8(STATE_VERSION);
        match self.states.get(group).and_then(|v| v.as_ref()) {
            None => s.u8(0),
            Some((key, value)) => {
                s.u8(1);
                encode_value(key, &self.key_ty, &mut s);
                encode_opt(value.as_ref(), &self.value_ty, &mut s);
            }
        }
        out.extend_from_slice(&s.into_vec());
    }

    fn merge_encoded(
        &mut self,
        group: usize,
        total_groups: usize,
        bytes: &[u8],
    ) -> Result<(), ExecError> {
        ensure_len(&mut self.states, total_groups, None);
        let mut cur = Cursor::new(bytes);
        check_version(&mut cur, "arg")?;
        let has = cur.u8().map_err(|e| invalid("arg", e))? != 0;
        if !has {
            return Ok(());
        }
        let key = decode_value(&mut cur, &self.key_ty).map_err(|e| invalid("arg", e))?;
        let value = decode_opt(&mut cur, &self.value_ty).map_err(|e| invalid("arg", e))?;
        Self::consider(self.is_max, &mut self.states[group], key, value);
        Ok(())
    }

    fn byte_size(&self) -> usize {
        self.states.capacity() * size_of::<Option<(Value, Option<Value>)>>()
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

#[cfg(test)]
pub(super) fn slice_bitmap(bm: &Bitmap, start: usize, len: usize) -> Bitmap {
    let mut out = Bitmap::new_valid(0);
    for i in start..start + len {
        out.push(bm.get(i));
    }
    out
}

/// Row-by-row equality via `total_cmp`, so NaN-vs-NaN and NULL-vs-NULL both count as equal
/// (unlike `Column`'s derived, structural `PartialEq`).
#[cfg(test)]
pub(super) fn assert_columns_equal(a: &Column, b: &Column) {
    assert_eq!(a.len(), b.len(), "row count differs");
    for i in 0..a.len() {
        let (va, vb) = (a.get(i), b.get(i));
        match (va.is_null(), vb.is_null()) {
            (true, true) => continue,
            (false, false) => assert_eq!(
                total_cmp(&va, &vb),
                Some(Ordering::Equal),
                "row {i}: {va:?} != {vb:?}"
            ),
            _ => panic!("row {i}: {va:?} != {vb:?}"),
        }
    }
}

/// (a) one `update` over every row, (b) the same rows split in half and `merge`d, and (c)
/// split then round-tripped through `encode_state`/`merge_encoded`, all give an equal
/// `finish`. `groups` should include a group id no row reaches.
#[cfg(test)]
pub(super) fn check_split<F>(
    make: F,
    args: &[Column],
    filter: Option<&Bitmap>,
    groups: &[usize],
    total_groups: usize,
) where
    F: Fn() -> Box<dyn GroupsAccumulator>,
{
    let refs: Vec<&Column> = args.iter().collect();
    let mut whole = make();
    whole.update(groups, total_groups, &refs, filter).unwrap();
    let want = whole.finish(total_groups).unwrap();

    let mid = groups.len() / 2;
    let rest = groups.len() - mid;
    let (g1, g2) = groups.split_at(mid);
    let args1: Vec<Column> = args.iter().map(|c| c.slice(0, mid)).collect();
    let args2: Vec<Column> = args.iter().map(|c| c.slice(mid, rest)).collect();
    let f1 = filter.map(|f| slice_bitmap(f, 0, mid));
    let f2 = filter.map(|f| slice_bitmap(f, mid, rest));
    let refs1: Vec<&Column> = args1.iter().collect();
    let refs2: Vec<&Column> = args2.iter().collect();

    let mut a: Box<dyn GroupsAccumulator> = make();
    a.update(g1, total_groups, &refs1, f1.as_ref()).unwrap();
    let mut b: Box<dyn GroupsAccumulator> = make();
    b.update(g2, total_groups, &refs2, f2.as_ref()).unwrap();
    let identity: Vec<usize> = (0..total_groups).collect();
    a.merge(&identity, total_groups, b).unwrap();
    assert_columns_equal(&want, &a.finish(total_groups).unwrap());

    let mut a: Box<dyn GroupsAccumulator> = make();
    a.update(g1, total_groups, &refs1, f1.as_ref()).unwrap();
    let mut b: Box<dyn GroupsAccumulator> = make();
    b.update(g2, total_groups, &refs2, f2.as_ref()).unwrap();
    for g in 0..total_groups {
        let mut bytes = Vec::new();
        b.encode_state(g, &mut bytes);
        a.merge_encoded(g, total_groups, &bytes).unwrap();
    }
    assert_columns_equal(&want, &a.finish(total_groups).unwrap());
}

/// `[]`, `[9]` (bad version) and a truncated body must all be `Invalid`, never a panic.
#[cfg(test)]
pub(super) fn assert_merge_encoded_rejects_garbage<F>(make: F)
where
    F: Fn() -> Box<dyn GroupsAccumulator>,
{
    for bytes in [&[][..], &[9][..], &[STATE_VERSION][..]] {
        let mut acc = make();
        assert!(
            matches!(acc.merge_encoded(0, 1, bytes), Err(ExecError::Invalid(_))),
            "bytes {bytes:?} should be Invalid"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(ty: &DataType, values: &[Value]) -> Column {
        Column::from_values(ty, values).unwrap()
    }

    fn dec(unscaled: i128, scale: u8) -> Value {
        Value::Decimal(Decimal::new(unscaled, scale).unwrap())
    }

    #[test]
    fn count_check_split() {
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
            || Box::new(CountAccumulator::new(false)),
            std::slice::from_ref(&c),
            None,
            &groups,
            4,
        );
        check_split(
            || Box::new(CountAccumulator::new(true)),
            &[c],
            None,
            &groups,
            4,
        );
    }

    #[test]
    fn count_vs_count_star_skips_null_and_honours_filter() {
        let c = col(
            &DataType::Int64,
            &[Value::Int64(1), Value::Null, Value::Int64(3)],
        );
        let groups = [0, 0, 0];
        let mut count = CountAccumulator::new(false);
        count.update(&groups, 1, &[&c], None).unwrap();
        assert_eq!(Box::new(count).finish(1).unwrap().get(0), Value::Int64(2));

        let mut star = CountAccumulator::new(true);
        star.update(&groups, 1, &[&c], None).unwrap();
        assert_eq!(Box::new(star).finish(1).unwrap().get(0), Value::Int64(3));

        let mut filter = Bitmap::new_valid(3);
        filter.set(1, false);
        let mut star_filtered = CountAccumulator::new(true);
        star_filtered
            .update(&groups, 1, &[&c], Some(&filter))
            .unwrap();
        assert_eq!(
            Box::new(star_filtered).finish(1).unwrap().get(0),
            Value::Int64(2)
        );
    }

    #[test]
    fn sum_over_empty_is_null_count_over_empty_is_zero() {
        let c = col(&DataType::Int64, &[]);
        let groups: [usize; 0] = [];
        let mut sum = SumAccumulator::new(&DataType::Int64).unwrap();
        sum.update(&groups, 1, &[&c], None).unwrap();
        assert_eq!(Box::new(sum).finish(1).unwrap().get(0), Value::Null);

        let mut count = CountAccumulator::new(false);
        count.update(&groups, 1, &[&c], None).unwrap();
        assert_eq!(Box::new(count).finish(1).unwrap().get(0), Value::Int64(0));
    }

    #[test]
    fn sum_int64_overflow_is_overflow() {
        let c = col(&DataType::Int64, &[Value::Int64(i64::MAX), Value::Int64(1)]);
        let groups = [0, 0];
        let mut sum = SumAccumulator::new(&DataType::Int64).unwrap();
        let err = sum.update(&groups, 1, &[&c], None).unwrap_err();
        assert!(matches!(err, ExecError::Overflow(op) if op == "sum(INT64)"));
    }

    #[test]
    fn sum_check_split() {
        let c = col(
            &DataType::Int64,
            &[
                Value::Int64(1),
                Value::Int64(2),
                Value::Int64(3),
                Value::Int64(4),
                Value::Int64(5),
                Value::Int64(6),
            ],
        );
        let groups = [0, 1, 0, 2, 1, 0];
        check_split(
            || Box::new(SumAccumulator::new(&DataType::Int64).unwrap()),
            &[c],
            None,
            &groups,
            4,
        );
    }

    #[test]
    fn avg_int64_10_and_20_is_15() {
        let c = col(&DataType::Int64, &[Value::Int64(10), Value::Int64(20)]);
        let groups = [0, 0];
        let mut avg = AvgAccumulator::new(&DataType::Int64).unwrap();
        avg.update(&groups, 1, &[&c], None).unwrap();
        assert_eq!(
            Box::new(avg).finish(1).unwrap().get(0),
            Value::Float64(15.0)
        );
    }

    #[test]
    fn avg_decimal_1_00_and_2_00_is_1_5() {
        let ty = DataType::decimal(5, 2).unwrap();
        let c = col(&ty, &[dec(100, 2), dec(200, 2)]);
        let groups = [0, 0];
        let mut avg = AvgAccumulator::new(&ty).unwrap();
        avg.update(&groups, 1, &[&c], None).unwrap();
        assert_eq!(Box::new(avg).finish(1).unwrap().get(0), Value::Float64(1.5));
    }

    #[test]
    fn avg_check_split() {
        let c = col(
            &DataType::Int64,
            &[
                Value::Int64(1),
                Value::Int64(2),
                Value::Int64(3),
                Value::Int64(4),
                Value::Int64(5),
                Value::Int64(6),
            ],
        );
        let groups = [0, 1, 0, 2, 1, 0];
        check_split(
            || Box::new(AvgAccumulator::new(&DataType::Int64).unwrap()),
            &[c],
            None,
            &groups,
            4,
        );
    }

    #[test]
    fn min_max_float64_nan_is_greatest_and_zero_signs_are_equal() {
        let c = col(
            &DataType::Float64,
            &[
                Value::Float64(1.0),
                Value::Float64(f64::NAN),
                Value::Float64(-0.0),
            ],
        );
        let groups = [0, 0, 0];

        let mut min = MinMaxAccumulator::new(&DataType::Float64, false).unwrap();
        min.update(&groups, 1, &[&c], None).unwrap();
        let got = Box::new(min).finish(1).unwrap().get(0);
        assert_eq!(
            total_cmp(&got, &Value::Float64(-0.0)),
            Some(Ordering::Equal)
        );

        let mut max = MinMaxAccumulator::new(&DataType::Float64, true).unwrap();
        max.update(&groups, 1, &[&c], None).unwrap();
        assert!(matches!(Box::new(max).finish(1).unwrap().get(0), Value::Float64(f) if f.is_nan()));
    }

    #[test]
    fn min_max_rejects_list() {
        let list_ty = DataType::list(DataType::Int64).unwrap();
        assert!(matches!(
            MinMaxAccumulator::new(&list_ty, true),
            Err(ExecError::Plan(_))
        ));
    }

    #[test]
    fn min_max_check_split() {
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
            || Box::new(MinMaxAccumulator::new(&DataType::Int64, false).unwrap()),
            std::slice::from_ref(&c),
            None,
            &groups,
            4,
        );
        check_split(
            || Box::new(MinMaxAccumulator::new(&DataType::Int64, true).unwrap()),
            &[c],
            None,
            &groups,
            4,
        );
    }

    #[test]
    fn arg_max_skips_null_score_and_keeps_first_on_tie() {
        let names = col(
            &DataType::String,
            &[
                Value::String("a".into()),
                Value::String("b".into()),
                Value::String("c".into()),
                Value::String("d".into()),
            ],
        );
        let scores = col(
            &DataType::Int64,
            &[
                Value::Int64(1),
                Value::Null,
                Value::Int64(5),
                Value::Int64(5),
            ],
        );
        let groups = [0, 0, 0, 0];
        let mut acc = ArgAccumulator::new(&DataType::String, &DataType::Int64, true).unwrap();
        acc.update(&groups, 1, &[&names, &scores], None).unwrap();
        assert_eq!(
            Box::new(acc).finish(1).unwrap().get(0),
            Value::String("c".into())
        );
    }

    #[test]
    fn arg_null_value_with_winning_key_still_wins() {
        let values = col(&DataType::Int64, &[Value::Int64(1), Value::Null]);
        let keys = col(&DataType::Int64, &[Value::Int64(1), Value::Int64(5)]);
        let groups = [0, 0];
        let mut acc = ArgAccumulator::new(&DataType::Int64, &DataType::Int64, true).unwrap();
        acc.update(&groups, 1, &[&values, &keys], None).unwrap();
        assert_eq!(Box::new(acc).finish(1).unwrap().get(0), Value::Null);
    }

    #[test]
    fn arg_check_split() {
        let values = col(
            &DataType::Int64,
            &[
                Value::Int64(10),
                Value::Int64(20),
                Value::Int64(30),
                Value::Int64(40),
                Value::Int64(50),
                Value::Int64(60),
            ],
        );
        let keys = col(
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
            || Box::new(ArgAccumulator::new(&DataType::Int64, &DataType::Int64, true).unwrap()),
            &[values, keys],
            None,
            &groups,
            4,
        );
    }

    #[test]
    fn count_sum_avg_min_max_arg_merge_encoded_rejects_garbage() {
        assert_merge_encoded_rejects_garbage(|| Box::new(CountAccumulator::new(false)));
        assert_merge_encoded_rejects_garbage(|| {
            Box::new(SumAccumulator::new(&DataType::Int64).unwrap())
        });
        assert_merge_encoded_rejects_garbage(|| {
            Box::new(AvgAccumulator::new(&DataType::Int64).unwrap())
        });
        assert_merge_encoded_rejects_garbage(|| {
            Box::new(MinMaxAccumulator::new(&DataType::Int64, true).unwrap())
        });
        assert_merge_encoded_rejects_garbage(|| {
            Box::new(ArgAccumulator::new(&DataType::Int64, &DataType::Int64, true).unwrap())
        });
    }
}
