//! Hand-rolled sketch aggregates (SPEC §8, D0015, no dependency): `approx_count_distinct`
//! (HLL), `approx_quantile` (DDSketch) and `top_k`. Parameters and encodings are frozen —
//! rollups (SPEC §16.1) persist these states across releases.

mod ddsketch;
mod hll;
mod topk;

pub(crate) use ddsketch::DdSketchAccumulator;
pub(crate) use hll::HllAccumulator;
pub(crate) use topk::TopKAccumulator;
