//! The physical plan enum operators execute (SPEC §7).

use super::expr::Expr;
use super::operator::ScanSpec;
use super::{Batch, Field};

pub use super::kernels::order::SortKey;

/// One physical query plan node. Every variant's output field list is documented on the
/// variant that changes it; the rest just pass their input's fields through.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    Scan(ScanSpec),
    Values {
        fields: Vec<Field>,
        batches: Vec<Batch>,
    },
    Filter {
        input: Box<Plan>,
        predicate: Expr,
    },
    Project {
        input: Box<Plan>,
        exprs: Vec<(String, Expr)>,
    },
    /// Output: the group columns (by the input's names), then the aggregates.
    Aggregate {
        input: Box<Plan>,
        group_by: Vec<usize>,
        aggs: Vec<AggCall>,
    },
    /// Output: `left`'s fields then `right`'s; the build side is `right`.
    Join {
        left: Box<Plan>,
        right: Box<Plan>,
        kind: JoinKind,
        on: Vec<(usize, usize)>,
    },
    Sort {
        input: Box<Plan>,
        keys: Vec<SortKey>,
    },
    TopK {
        input: Box<Plan>,
        keys: Vec<SortKey>,
        k: usize,
    },
    Limit {
        input: Box<Plan>,
        limit: Option<usize>,
        offset: usize,
    },
    /// At least one input, all with identical field types; output names come from the first.
    UnionAll(Vec<Plan>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
}

/// One `SELECT`-list aggregate call. `filter` is a BOOL input column index for
/// `FILTER (WHERE …)`, evaluated by a `Project` below the `Aggregate`.
#[derive(Debug, Clone, PartialEq)]
pub struct AggCall {
    pub func: AggFunc,
    pub args: Vec<usize>,
    pub filter: Option<usize>,
    pub name: String,
}

/// SPEC §8's aggregate set. `ArgMin`/`ArgMax` take `args: [value, key]`.
#[derive(Debug, Clone, PartialEq)]
pub enum AggFunc {
    CountStar,
    Count,
    Sum,
    Avg,
    Min,
    Max,
    ApproxCountDistinct,
    Quantile(f64),
    ApproxQuantile(f64),
    TopK(usize),
    ArgMin,
    ArgMax,
    ListAgg,
    Histogram,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_key_asc_and_desc_default_to_nulls_last() {
        let asc = SortKey::asc(0);
        assert_eq!(
            asc,
            SortKey {
                column: 0,
                descending: false,
                nulls_first: false
            }
        );
        let desc = SortKey::desc(1);
        assert_eq!(
            desc,
            SortKey {
                column: 1,
                descending: true,
                nulls_first: false
            }
        );
    }

    #[test]
    fn plan_variants_are_constructible_and_comparable() {
        let a = Plan::Limit {
            input: Box::new(Plan::UnionAll(vec![])),
            limit: Some(10),
            offset: 0,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }
}
