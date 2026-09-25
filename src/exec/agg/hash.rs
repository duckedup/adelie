//! Hash-aggregate `Sink` (SPEC §7): drives one `GroupsAccumulator` per `AggCall`, keyed by
//! `kernels::rowkey`. Filled by U9 (adelie-1st).

pub(crate) struct HashAggregateSink;
