//! Executor kernels every operator shares (SPEC §7): selection, comparison, boolean logic,
//! row keys, stable hashing and sort. Real code (U1), wired into operators from U2 onward.

#![allow(dead_code)] // until U2/U5/U6/U7/U9 call these from eval/ops/join/pipeline/agg

pub(crate) mod boolean;
pub(crate) mod compare;
pub(crate) mod hash;
pub(crate) mod order;
pub(crate) mod rowkey;
pub(crate) mod select;

pub(crate) use boolean::{and, bool_column, not, or, truthy};
pub(crate) use compare::{compare, compare_scalar};
pub(crate) use hash::stable_hash;
pub(crate) use order::sort_indices;
pub(crate) use rowkey::encode_row_key;
pub(crate) use select::{
    concat_batches, filter_batch, null_column, rechunk, slice_batch, take, take_batch, take_opt,
};
