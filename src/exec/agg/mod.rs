//! The per-group accumulator trait every SPEC §8 aggregate implements, and the factory U9
//! wires up. `basic`/`collect`/`sketch` supply the concrete accumulators (U3/U4); `hash.rs`
//! (U9) is the `GroupsAccumulator`-driven hash-aggregate `Sink`.

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
    /// (D0015). No caller until rollups (adelie-zit.2); every accumulator's tests exercise it.
    #[allow(dead_code)]
    fn encode_state(&self, group: usize, out: &mut Vec<u8>);

    /// Merges an encoded state into group `g`. Malformed bytes are `Invalid`, never a panic.
    #[allow(dead_code)] // rollups (adelie-zit.2) are the first caller
    fn merge_encoded(
        &mut self,
        group: usize,
        total_groups: usize,
        bytes: &[u8],
    ) -> Result<(), ExecError>;

    fn byte_size(&self) -> usize;

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any>;
}

/// One `AggFunc`'s expected argument count: `CountStar` takes none, `ArgMin`/`ArgMax` take a
/// value and a key, everything else takes one argument.
fn arity(func: &AggFunc) -> usize {
    match func {
        AggFunc::CountStar => 0,
        AggFunc::ArgMin | AggFunc::ArgMax => 2,
        _ => 1,
    }
}

/// The output type of `func` over `arg_types`, read off the accumulator that computes it, so a
/// planner's belief can never drift from the executor's result.
pub fn output_type(func: &AggFunc, arg_types: &[DataType]) -> Result<DataType, ExecError> {
    Ok(accumulator(func, arg_types)?.data_type())
}

/// U9's factory: one constructor per `AggFunc`/arg-type combination (the root blueprint's
/// §Contract table names each one). Wrong arity, or a type a constructor rejects, is `Plan`.
pub(crate) fn accumulator(
    func: &AggFunc,
    arg_types: &[DataType],
) -> Result<Box<dyn GroupsAccumulator>, ExecError> {
    let want = arity(func);
    if arg_types.len() != want {
        return Err(ExecError::Plan(format!(
            "{func:?} takes {want} argument(s), got {}",
            arg_types.len()
        )));
    }
    let acc: Box<dyn GroupsAccumulator> = match func {
        AggFunc::CountStar => Box::new(basic::CountAccumulator::new(true)),
        AggFunc::Count => Box::new(basic::CountAccumulator::new(false)),
        AggFunc::CountDistinct => Box::new(collect::CountDistinctAccumulator::new(&arg_types[0])?),
        AggFunc::Sum => Box::new(basic::SumAccumulator::new(&arg_types[0])?),
        AggFunc::Avg => Box::new(basic::AvgAccumulator::new(&arg_types[0])?),
        AggFunc::Min => Box::new(basic::MinMaxAccumulator::new(&arg_types[0], false)?),
        AggFunc::Max => Box::new(basic::MinMaxAccumulator::new(&arg_types[0], true)?),
        AggFunc::ArgMin => Box::new(basic::ArgAccumulator::new(
            &arg_types[0],
            &arg_types[1],
            false,
        )?),
        AggFunc::ArgMax => Box::new(basic::ArgAccumulator::new(
            &arg_types[0],
            &arg_types[1],
            true,
        )?),
        AggFunc::ListAgg => Box::new(collect::ListAggAccumulator::new(&arg_types[0])?),
        AggFunc::Quantile(q) => Box::new(collect::QuantileAccumulator::new(&arg_types[0], *q)?),
        AggFunc::Histogram => Box::new(collect::HistogramAccumulator::new(&arg_types[0])?),
        AggFunc::ApproxCountDistinct => Box::new(sketch::HllAccumulator::new()),
        AggFunc::ApproxQuantile(q) => {
            Box::new(sketch::DdSketchAccumulator::new(&arg_types[0], *q)?)
        }
        AggFunc::TopK(k) => Box::new(sketch::TopKAccumulator::new(&arg_types[0], *k)?),
    };
    Ok(acc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_funcs() -> Vec<(AggFunc, Vec<DataType>)> {
        vec![
            (AggFunc::CountStar, vec![]),
            (AggFunc::Count, vec![DataType::Int64]),
            (AggFunc::CountDistinct, vec![DataType::Int64]),
            (AggFunc::Sum, vec![DataType::Int64]),
            (AggFunc::Avg, vec![DataType::Int64]),
            (AggFunc::Min, vec![DataType::Int64]),
            (AggFunc::Max, vec![DataType::Int64]),
            (AggFunc::ApproxCountDistinct, vec![DataType::Int64]),
            (AggFunc::Quantile(0.5), vec![DataType::Int64]),
            (AggFunc::ApproxQuantile(0.5), vec![DataType::Int64]),
            (AggFunc::TopK(3), vec![DataType::Int64]),
            (AggFunc::ArgMin, vec![DataType::Int64, DataType::Int64]),
            (AggFunc::ArgMax, vec![DataType::Int64, DataType::Int64]),
            (AggFunc::ListAgg, vec![DataType::Int64]),
            (AggFunc::Histogram, vec![DataType::Int64]),
        ]
    }

    #[test]
    fn every_agg_func_constructs_for_a_valid_type() {
        for (func, arg_types) in all_funcs() {
            accumulator(&func, &arg_types)
                .unwrap_or_else(|e| panic!("{func:?} over {arg_types:?} should build: {e}"));
        }
    }

    #[test]
    fn sum_of_string_is_plan() {
        let err = accumulator(&AggFunc::Sum, &[DataType::String])
            .err()
            .unwrap();
        assert!(matches!(err, ExecError::Plan(_)));
    }

    #[test]
    fn wrong_arity_is_plan() {
        assert!(matches!(
            accumulator(&AggFunc::CountStar, &[DataType::Int64]),
            Err(ExecError::Plan(_))
        ));
        assert!(matches!(
            accumulator(&AggFunc::Sum, &[]),
            Err(ExecError::Plan(_))
        ));
        assert!(matches!(
            accumulator(&AggFunc::Sum, &[DataType::Int64, DataType::Int64]),
            Err(ExecError::Plan(_))
        ));
        assert!(matches!(
            accumulator(&AggFunc::ArgMin, &[DataType::Int64]),
            Err(ExecError::Plan(_))
        ));
    }
}
