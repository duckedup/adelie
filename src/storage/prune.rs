//! Zone-map and skip-index pruning (SPEC §7): pure, no IO, so Miri runs it directly. Only
//! ever proves a conjunct false; an unknown bound or an unsupported form proves nothing.

use std::cmp::Ordering;

use crate::exec::{CmpOp, Expr};
use crate::types::{Value, total_cmp};

/// One column's known range, as manifest segment stats or a footer zone map give it. `None`
/// fields mean "no stats for this column": they never prove anything.
pub(crate) struct ColumnRange<'a> {
    pub rows: u64,
    pub null_count: u64,
    pub min: Option<&'a Value>,
    pub max: Option<&'a Value>,
}

/// True unless `pred` is provably false for every row. `stats(c)` answers for scan output
/// column `c`; `None` means no stats, which never proves anything.
pub(crate) fn may_match<'s>(pred: &Expr, stats: &dyn Fn(usize) -> Option<ColumnRange<'s>>) -> bool {
    !pred.conjuncts().iter().any(|c| conjunct_is_false(c, stats))
}

/// True iff `probe(col, v)` answers `might column col contain v`; `None` means no index.
pub(crate) fn index_may_match(pred: &Expr, probe: &dyn Fn(usize, &Value) -> Option<bool>) -> bool {
    !pred
        .conjuncts()
        .iter()
        .any(|c| index_conjunct_is_false(c, probe))
}

fn conjunct_is_false<'s>(e: &Expr, stats: &dyn Fn(usize) -> Option<ColumnRange<'s>>) -> bool {
    match e {
        Expr::Cmp(op, l, r) => cmp_is_false(*op, l, r, stats),
        Expr::Between {
            expr,
            low,
            high,
            negated: false,
        } => between_is_false(expr, low, high, stats),
        Expr::InList {
            expr,
            list,
            negated: false,
        } => in_list_is_false(expr, list, stats),
        Expr::IsNull(e) => {
            column_of(e).is_some_and(|c| stats(c).is_some_and(|r| r.null_count == 0))
        }
        Expr::IsNotNull(e) => {
            column_of(e).is_some_and(|c| stats(c).is_some_and(|r| r.null_count == r.rows))
        }
        Expr::Or(parts) => parts.iter().all(|p| !may_match(p, stats)),
        _ => false,
    }
}

fn index_conjunct_is_false(e: &Expr, probe: &dyn Fn(usize, &Value) -> Option<bool>) -> bool {
    match e {
        Expr::Cmp(CmpOp::Eq, l, r) => index_eq_is_false(l, r, probe),
        Expr::InList {
            expr,
            list,
            negated: false,
        } => column_of(expr).is_some_and(|c| {
            list.iter()
                .filter(|v| !v.is_null())
                .all(|v| probe(c, v) == Some(false))
        }),
        Expr::Or(parts) => parts.iter().all(|p| !index_may_match(p, probe)),
        _ => false,
    }
}

fn column_of(e: &Expr) -> Option<usize> {
    match e {
        Expr::Column(c) => Some(*c),
        _ => None,
    }
}

fn index_eq_is_false(l: &Expr, r: &Expr, probe: &dyn Fn(usize, &Value) -> Option<bool>) -> bool {
    let (col, v) = match (l, r) {
        (Expr::Column(c), Expr::Literal(v, _)) => (*c, v),
        (Expr::Literal(v, _), Expr::Column(c)) => (*c, v),
        _ => return false,
    };
    probe(col, v) == Some(false)
}

/// Normalises `l op r` to `Column(c) op' v` (flipping a scalar-left comparison), then applies
/// the pinned per-op rule. `v` NULL is always false: NULL never compares true.
fn cmp_is_false<'s>(
    op: CmpOp,
    l: &Expr,
    r: &Expr,
    stats: &dyn Fn(usize) -> Option<ColumnRange<'s>>,
) -> bool {
    let (col, v, op) = match (l, r) {
        (Expr::Column(c), Expr::Literal(v, _)) => (*c, v, op),
        (Expr::Literal(v, _), Expr::Column(c)) => (*c, v, op.flip()),
        _ => return false,
    };
    if v.is_null() {
        return true;
    }
    let Some(range) = stats(col) else {
        return false;
    };
    if range.null_count == range.rows {
        return true;
    }
    if matches!(op, CmpOp::Eq | CmpOp::Ne) && matches!(v, Value::Float64(f) if f.is_nan()) {
        return false;
    }
    match op {
        CmpOp::Eq => {
            matches!(
                range.min.and_then(|m| total_cmp(v, m)),
                Some(Ordering::Less)
            ) || matches!(
                range.max.and_then(|m| total_cmp(v, m)),
                Some(Ordering::Greater)
            )
        }
        CmpOp::Ne => match (range.min, range.max) {
            (Some(min), Some(max)) => {
                total_cmp(min, v) == Some(Ordering::Equal)
                    && total_cmp(max, v) == Some(Ordering::Equal)
            }
            _ => false,
        },
        CmpOp::Lt => matches!(
            range.min.and_then(|m| total_cmp(m, v)),
            Some(Ordering::Greater) | Some(Ordering::Equal)
        ),
        CmpOp::Le => matches!(
            range.min.and_then(|m| total_cmp(m, v)),
            Some(Ordering::Greater)
        ),
        CmpOp::Gt => matches!(
            range.max.and_then(|m| total_cmp(m, v)),
            Some(Ordering::Less) | Some(Ordering::Equal)
        ),
        CmpOp::Ge => matches!(
            range.max.and_then(|m| total_cmp(m, v)),
            Some(Ordering::Less)
        ),
    }
}

/// `Between{negated: false}` with literal bounds: false when `high < min` or `low > max`.
fn between_is_false<'s>(
    expr: &Expr,
    low: &Expr,
    high: &Expr,
    stats: &dyn Fn(usize) -> Option<ColumnRange<'s>>,
) -> bool {
    let Some(col) = column_of(expr) else {
        return false;
    };
    let (Expr::Literal(low, _), Expr::Literal(high, _)) = (low, high) else {
        return false;
    };
    if low.is_null() || high.is_null() {
        return false;
    }
    let Some(range) = stats(col) else {
        return false;
    };
    let high_below_min = range
        .min
        .is_some_and(|m| total_cmp(high, m) == Some(Ordering::Less));
    let low_above_max = range
        .max
        .is_some_and(|m| total_cmp(low, m) == Some(Ordering::Greater));
    high_below_min || low_above_max
}

/// `InList{negated: false}`: false iff every non-null element is outside `[min, max]`. A NULL
/// element never proves anything either way.
fn in_list_is_false<'s>(
    expr: &Expr,
    list: &[Value],
    stats: &dyn Fn(usize) -> Option<ColumnRange<'s>>,
) -> bool {
    let Some(col) = column_of(expr) else {
        return false;
    };
    let Some(range) = stats(col) else {
        return false;
    };
    for v in list.iter().filter(|v| !v.is_null()) {
        let outside = match (range.min, range.max) {
            (Some(min), Some(max)) => {
                total_cmp(v, min) == Some(Ordering::Less)
                    || total_cmp(v, max) == Some(Ordering::Greater)
            }
            _ => return false,
        };
        if !outside {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DataType;

    fn stats_of<'a>(
        rows: u64,
        null_count: u64,
        min: Option<&'a Value>,
        max: Option<&'a Value>,
    ) -> impl Fn(usize) -> Option<ColumnRange<'a>> {
        move |c| {
            (c == 0).then_some(ColumnRange {
                rows,
                null_count,
                min,
                max,
            })
        }
    }

    fn lit(v: i64) -> Expr {
        Expr::lit(Value::Int64(v), DataType::Int64)
    }

    fn cmp(op: CmpOp, v: i64) -> Expr {
        Expr::cmp(op, Expr::col(0), lit(v))
    }

    #[test]
    fn all_rows_null_proves_any_comparison_false() {
        let (min, max) = (Value::Int64(1), Value::Int64(5));
        let stats = stats_of(3, 3, Some(&min), Some(&max));
        assert!(!may_match(&cmp(CmpOp::Ge, 1), &stats));
    }

    #[test]
    fn some_non_null_rows_do_not_prove_it_false() {
        let (min, max) = (Value::Int64(1), Value::Int64(5));
        let stats = stats_of(3, 2, Some(&min), Some(&max));
        assert!(may_match(&cmp(CmpOp::Ge, 1), &stats));
    }

    #[test]
    fn comparison_with_null_literal_is_always_false() {
        let stats = stats_of(3, 0, None, None);
        let pred = Expr::cmp(
            CmpOp::Eq,
            Expr::col(0),
            Expr::Literal(Value::Null, DataType::Int64),
        );
        assert!(!may_match(&pred, &stats));
    }

    #[test]
    fn a_non_null_literal_is_not_pruned_by_the_null_rule() {
        let stats = stats_of(3, 0, None, None);
        assert!(may_match(&cmp(CmpOp::Eq, 1), &stats));
    }

    #[test]
    fn eq_false_when_literal_below_min() {
        let (min, max) = (Value::Int64(5), Value::Int64(10));
        let stats = stats_of(3, 0, Some(&min), Some(&max));
        assert!(!may_match(&cmp(CmpOp::Eq, 4), &stats));
    }

    #[test]
    fn eq_true_at_min_boundary() {
        let (min, max) = (Value::Int64(5), Value::Int64(10));
        let stats = stats_of(3, 0, Some(&min), Some(&max));
        assert!(may_match(&cmp(CmpOp::Eq, 5), &stats));
    }

    #[test]
    fn ne_false_when_min_equals_max_equals_literal() {
        let (min, max) = (Value::Int64(5), Value::Int64(5));
        let stats = stats_of(3, 0, Some(&min), Some(&max));
        assert!(!may_match(&cmp(CmpOp::Ne, 5), &stats));
    }

    #[test]
    fn ne_true_when_min_and_max_differ() {
        let (min, max) = (Value::Int64(5), Value::Int64(6));
        let stats = stats_of(3, 0, Some(&min), Some(&max));
        assert!(may_match(&cmp(CmpOp::Ne, 5), &stats));
    }

    #[test]
    fn lt_false_when_min_at_least_literal() {
        let min = Value::Int64(5);
        let stats = stats_of(3, 0, Some(&min), None);
        assert!(!may_match(&cmp(CmpOp::Lt, 5), &stats));
    }

    #[test]
    fn lt_true_when_min_below_literal() {
        let min = Value::Int64(4);
        let stats = stats_of(3, 0, Some(&min), None);
        assert!(may_match(&cmp(CmpOp::Lt, 5), &stats));
    }

    #[test]
    fn le_false_when_min_above_literal() {
        let min = Value::Int64(6);
        let stats = stats_of(3, 0, Some(&min), None);
        assert!(!may_match(&cmp(CmpOp::Le, 5), &stats));
    }

    #[test]
    fn le_true_at_min_boundary() {
        let min = Value::Int64(5);
        let stats = stats_of(3, 0, Some(&min), None);
        assert!(may_match(&cmp(CmpOp::Le, 5), &stats));
    }

    #[test]
    fn gt_false_at_max_boundary() {
        let max = Value::Int64(5);
        let stats = stats_of(3, 0, None, Some(&max));
        assert!(!may_match(&cmp(CmpOp::Gt, 5), &stats));
    }

    #[test]
    fn gt_true_when_max_above_literal() {
        let max = Value::Int64(6);
        let stats = stats_of(3, 0, None, Some(&max));
        assert!(may_match(&cmp(CmpOp::Gt, 5), &stats));
    }

    #[test]
    fn ge_false_when_max_below_literal() {
        let max = Value::Int64(4);
        let stats = stats_of(3, 0, None, Some(&max));
        assert!(!may_match(&cmp(CmpOp::Ge, 5), &stats));
    }

    #[test]
    fn ge_true_at_max_boundary() {
        let max = Value::Int64(5);
        let stats = stats_of(3, 0, None, Some(&max));
        assert!(may_match(&cmp(CmpOp::Ge, 5), &stats));
    }

    #[test]
    fn unknown_max_never_prunes_gt() {
        let min = Value::Int64(0);
        let stats = stats_of(3, 0, Some(&min), None);
        assert!(may_match(&cmp(CmpOp::Gt, 1_000_000), &stats));
    }

    #[test]
    fn literal_on_the_left_is_flipped_before_the_rule_applies() {
        // `5 < c` <=> `c > 5`: false (via the Gt rule) when max <= 5.
        let max = Value::Int64(5);
        let stats = stats_of(3, 0, None, Some(&max));
        let pred = Expr::cmp(CmpOp::Lt, lit(5), Expr::col(0));
        assert!(!may_match(&pred, &stats));
        let max_above = Value::Int64(6);
        let stats_above = stats_of(3, 0, None, Some(&max_above));
        assert!(may_match(&pred, &stats_above));
    }

    #[test]
    fn nan_literal_never_prunes_eq_or_ne() {
        let (min, max) = (Value::Int64(0), Value::Int64(100));
        let stats = stats_of(3, 0, Some(&min), Some(&max));
        let pred = Expr::cmp(
            CmpOp::Eq,
            Expr::col(0),
            Expr::lit(Value::Float64(f64::NAN), DataType::Float64),
        );
        assert!(may_match(&pred, &stats));
    }

    #[test]
    fn between_false_when_range_lies_left_of_the_stats() {
        let (min, max) = (Value::Int64(5), Value::Int64(10));
        let stats = stats_of(3, 0, Some(&min), Some(&max));
        let pred = Expr::Between {
            expr: Box::new(Expr::col(0)),
            low: Box::new(lit(3)),
            high: Box::new(lit(4)),
            negated: false,
        };
        assert!(!may_match(&pred, &stats));
    }

    #[test]
    fn between_true_when_it_touches_the_stats() {
        let (min, max) = (Value::Int64(5), Value::Int64(10));
        let stats = stats_of(3, 0, Some(&min), Some(&max));
        let pred = Expr::Between {
            expr: Box::new(Expr::col(0)),
            low: Box::new(lit(3)),
            high: Box::new(lit(5)),
            negated: false,
        };
        assert!(may_match(&pred, &stats));
    }

    #[test]
    fn in_list_false_when_every_element_is_outside_the_range() {
        let (min, max) = (Value::Int64(5), Value::Int64(10));
        let stats = stats_of(3, 0, Some(&min), Some(&max));
        let pred = Expr::InList {
            expr: Box::new(Expr::col(0)),
            list: vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)],
            negated: false,
        };
        assert!(!may_match(&pred, &stats));
    }

    #[test]
    fn in_list_true_when_one_element_falls_inside() {
        let (min, max) = (Value::Int64(5), Value::Int64(10));
        let stats = stats_of(3, 0, Some(&min), Some(&max));
        let pred = Expr::InList {
            expr: Box::new(Expr::col(0)),
            list: vec![Value::Int64(1), Value::Int64(5)],
            negated: false,
        };
        assert!(may_match(&pred, &stats));
    }

    #[test]
    fn in_list_a_null_element_proves_nothing_either_way() {
        let (min, max) = (Value::Int64(5), Value::Int64(10));
        let stats = stats_of(3, 0, Some(&min), Some(&max));
        let all_outside = Expr::InList {
            expr: Box::new(Expr::col(0)),
            list: vec![Value::Null, Value::Int64(1)],
            negated: false,
        };
        assert!(!may_match(&all_outside, &stats));
        let one_inside = Expr::InList {
            expr: Box::new(Expr::col(0)),
            list: vec![Value::Null, Value::Int64(6)],
            negated: false,
        };
        assert!(may_match(&one_inside, &stats));
    }

    #[test]
    fn is_null_false_when_nothing_is_null() {
        let stats = stats_of(3, 0, None, None);
        assert!(!may_match(&Expr::IsNull(Box::new(Expr::col(0))), &stats));
    }

    #[test]
    fn is_null_true_when_something_might_be_null() {
        let stats = stats_of(3, 1, None, None);
        assert!(may_match(&Expr::IsNull(Box::new(Expr::col(0))), &stats));
    }

    #[test]
    fn is_not_null_false_when_everything_is_null() {
        let stats = stats_of(3, 3, None, None);
        assert!(!may_match(&Expr::IsNotNull(Box::new(Expr::col(0))), &stats));
    }

    #[test]
    fn is_not_null_true_when_something_is_not_null() {
        let stats = stats_of(3, 2, None, None);
        assert!(may_match(&Expr::IsNotNull(Box::new(Expr::col(0))), &stats));
    }

    #[test]
    fn or_of_two_false_branches_is_false() {
        let min = Value::Int64(5);
        let stats = stats_of(3, 0, Some(&min), None);
        let pred = Expr::Or(vec![cmp(CmpOp::Lt, 5), cmp(CmpOp::Lt, 4)]);
        assert!(!may_match(&pred, &stats));
    }

    #[test]
    fn or_of_a_false_and_a_true_branch_is_true() {
        let min = Value::Int64(4);
        let stats = stats_of(3, 0, Some(&min), None);
        let pred = Expr::Or(vec![cmp(CmpOp::Lt, 4), cmp(CmpOp::Lt, 5)]);
        assert!(may_match(&pred, &stats));
    }

    #[test]
    fn index_probe_some_false_prunes_eq() {
        let probe = |_c: usize, _v: &Value| Some(false);
        assert!(!index_may_match(&cmp(CmpOp::Eq, 1), &probe));
    }

    #[test]
    fn index_probe_none_does_not_prune() {
        let probe = |_c: usize, _v: &Value| None;
        assert!(index_may_match(&cmp(CmpOp::Eq, 1), &probe));
    }

    #[test]
    fn index_probe_prunes_in_list_only_when_every_non_null_element_is_absent() {
        let probe = |_c: usize, v: &Value| Some(*v == Value::Int64(2));
        let pred = Expr::InList {
            expr: Box::new(Expr::col(0)),
            list: vec![Value::Int64(1), Value::Int64(3)],
            negated: false,
        };
        assert!(!index_may_match(&pred, &probe));
        let pred_with_present = Expr::InList {
            expr: Box::new(Expr::col(0)),
            list: vec![Value::Int64(1), Value::Int64(2)],
            negated: false,
        };
        assert!(index_may_match(&pred_with_present, &probe));
    }
}
