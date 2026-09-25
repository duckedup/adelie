//! The per-group accumulator trait every SPEC §8 aggregate implements, and the factory U9
//! wires up. `basic`/`collect`/`sketch` supply the concrete accumulators (U3/U4); `hash.rs`
//! (U9) is the `GroupsAccumulator`-driven hash-aggregate `Sink`.

#![allow(dead_code)] // until adelie-1st U9 wires accumulator() and HashAggregateSink

mod basic;
mod collect;
mod hash;
mod sketch;

pub(crate) use hash::HashAggregateSink;

use crate::exec::{Bitmap, Column, ExecError};
use crate::types::DataType;

use super::plan::AggFunc;

/// One aggregate function's running state, grouped: row `r` folds into group `groups[r]`.
pub(crate) trait GroupsAccumulator: Send + 'static {
    /// `args` are the call's argument columns; `filter` keeps row `r` iff its bit is set
    /// (`FILTER (WHERE …)`). Grows to `total_groups` groups.
    fn update(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        args: &[&Column],
        filter: Option<&Bitmap>,
    ) -> Result<(), ExecError>;

    /// Folds `other` (same function, same arg types) in: `other`'s group `g` goes to
    /// `groups[g]`.
    fn merge(
        &mut self,
        groups: &[usize],
        total_groups: usize,
        other: Box<dyn GroupsAccumulator>,
    ) -> Result<(), ExecError>;

    /// One value per group `0..total_groups`; a group nothing reached gets the empty value.
    fn finish(self: Box<Self>, total_groups: usize) -> Result<Column, ExecError>;

    fn data_type(&self) -> DataType;

    /// Group `g`'s partial state: first byte is the format version (1). The rollup contract
    /// (D0015).
    fn encode_state(&self, group: usize, out: &mut Vec<u8>);

    /// Merges an encoded state into group `g`. Malformed bytes are `Invalid`, never a panic.
    fn merge_encoded(
        &mut self,
        group: usize,
        total_groups: usize,
        bytes: &[u8],
    ) -> Result<(), ExecError>;

    fn byte_size(&self) -> usize;

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any>;
}

/// U9's factory: one constructor per `AggFunc`/arg-type combination (the root blueprint's
/// §Contract table names each one).
pub(crate) fn accumulator(
    func: &AggFunc,
    arg_types: &[DataType],
) -> Result<Box<dyn GroupsAccumulator>, ExecError> {
    unimplemented!("adelie-1st U9")
}
