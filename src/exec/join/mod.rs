//! Hash join (SPEC §7: INNER/LEFT equi-join). The build (right) side runs through the
//! scheduler into a `JoinBuildSink`; workers' partials `merge`, then `into_table` builds one
//! hash table that every `HashJoinProbe` (one per worker, probe/left side) shares.

#![allow(dead_code)] // wired in from adelie-1st U7 (pipeline) and U11 (executor)

mod build;
mod probe;

pub(crate) use build::JoinBuildSink;
pub(crate) use probe::HashJoinProbe;
