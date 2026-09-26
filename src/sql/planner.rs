//! Bound logical plan → `exec::Plan` (SPEC §8, D0016 "Planner"): rule-based,
//! no cost model. Runs, in order: constant folding, projection pruning, predicate pushdown,
//! then the per-node lowering that also picks TopK vs Sort and an INNER join's build side.

use std::collections::HashSet;

use crate::exec;
use crate::types::{DataType, Value};

use super::binder::{
    BExpr, LAgg, LogicalPlan, collect_prefixes, expr_columns, fold_constant, lower, prefix_of,
};
use super::error::SqlError;

/// A table's estimated row count (the sum of its live segments' `rows`, D0016 planner rule
/// 7), supplied by `execute.rs` from the same `View` the binder read the schema from.
pub(crate) type RowEstimate<'a> = dyn Fn(&str, &str) -> u64 + 'a;

/// Lowers one bound query to a physical plan, in the fixed rule order D0016
/// numbers: constant folding (5), projection pruning (1), predicate pushdown (3), then the
/// per-node lowering (unique names already hold from the binder, rule 2; rule 4 is storage's;
/// rule 6 is the executor's; rules 7–9 are applied while lowering).
pub(crate) fn plan(
    logical: LogicalPlan,
    row_estimate: &RowEstimate,
) -> Result<exec::Plan, SqlError> {
    let logical = fold_tree(logical);
    let mut used = HashSet::new();
    collect_used_names(&logical, &mut used);
    let logical = prune_scans(logical, &used);
    let logical = merge_filters(push_down(logical));
    lower_plan(&logical, row_estimate)
}

// ── rule 5: constant folding ─────────────────────────────────────────────────

fn fold_tree(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Scan { .. } | LogicalPlan::Values { .. } => plan,
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(fold_tree(*input)),
            predicate: fold_expr(predicate),
        },
        LogicalPlan::Project {
            input,
            exprs,
            fields,
        } => LogicalPlan::Project {
            input: Box::new(fold_tree(*input)),
            exprs: exprs.into_iter().map(|(n, e)| (n, fold_expr(e))).collect(),
            fields,
        },
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggs,
            fields,
        } => LogicalPlan::Aggregate {
            input: Box::new(fold_tree(*input)),
            group_by: group_by.into_iter().map(fold_expr).collect(),
            aggs: aggs
                .into_iter()
                .map(|a| LAgg {
                    func: a.func,
                    args: a.args.into_iter().map(fold_expr).collect(),
                    filter: a.filter.map(fold_expr),
                    name: a.name,
                })
                .collect(),
            fields,
        },
        LogicalPlan::Join {
            left,
            right,
            kind,
            on,
            fields,
        } => LogicalPlan::Join {
            left: Box::new(fold_tree(*left)),
            right: Box::new(fold_tree(*right)),
            kind,
            on,
            fields,
        },
        LogicalPlan::Sort { input, keys } => LogicalPlan::Sort {
            input: Box::new(fold_tree(*input)),
            keys,
        },
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => LogicalPlan::Limit {
            input: Box::new(fold_tree(*input)),
            limit,
            offset,
        },
        LogicalPlan::UnionAll { branches, fields } => LogicalPlan::UnionAll {
            branches: branches.into_iter().map(fold_tree).collect(),
            fields,
        },
    }
}

fn fold_expr(e: BExpr) -> BExpr {
    let folded = match e {
        BExpr::Column(_) | BExpr::Literal(..) => return e,
        BExpr::Cmp(op, l, r) => BExpr::Cmp(op, Box::new(fold_expr(*l)), Box::new(fold_expr(*r))),
        BExpr::And(parts) => BExpr::And(parts.into_iter().map(fold_expr).collect()),
        BExpr::Or(parts) => BExpr::Or(parts.into_iter().map(fold_expr).collect()),
        BExpr::Not(e) => BExpr::Not(Box::new(fold_expr(*e))),
        BExpr::IsNull(e) => BExpr::IsNull(Box::new(fold_expr(*e))),
        BExpr::IsNotNull(e) => BExpr::IsNotNull(Box::new(fold_expr(*e))),
        BExpr::InList {
            expr,
            list,
            negated,
        } => BExpr::InList {
            expr: Box::new(fold_expr(*expr)),
            list,
            negated,
        },
        BExpr::Between {
            expr,
            low,
            high,
            negated,
        } => BExpr::Between {
            expr: Box::new(fold_expr(*expr)),
            low: Box::new(fold_expr(*low)),
            high: Box::new(fold_expr(*high)),
            negated,
        },
        BExpr::Like {
            expr,
            pattern,
            case_insensitive,
            negated,
        } => BExpr::Like {
            expr: Box::new(fold_expr(*expr)),
            pattern,
            case_insensitive,
            negated,
        },
        BExpr::Arith(op, l, r) => {
            BExpr::Arith(op, Box::new(fold_expr(*l)), Box::new(fold_expr(*r)))
        }
        BExpr::Neg(e) => BExpr::Neg(Box::new(fold_expr(*e))),
        BExpr::Case {
            branches,
            otherwise,
        } => BExpr::Case {
            branches: branches
                .into_iter()
                .map(|(c, r)| (fold_expr(c), fold_expr(r)))
                .collect(),
            otherwise: otherwise.map(|o| Box::new(fold_expr(*o))),
        },
        BExpr::Cast(e, t) => BExpr::Cast(Box::new(fold_expr(*e)), t),
        BExpr::Func { func, args } => BExpr::Func {
            func,
            args: args.into_iter().map(fold_expr).collect(),
        },
    };
    match fold_constant(&folded) {
        Some(v) => match lower(&folded, &[]).ok().and_then(|e| e.data_type(&[]).ok()) {
            Some(ty) => BExpr::Literal(v, ty),
            None => folded,
        },
        None => folded,
    }
}

// ── rule 1: projection pruning ───────────────────────────────────────────────

fn collect_used_names(plan: &LogicalPlan, out: &mut HashSet<String>) {
    match plan {
        LogicalPlan::Scan { .. } | LogicalPlan::Values { .. } => {}
        LogicalPlan::Filter { input, predicate } => {
            expr_columns(predicate, out);
            collect_used_names(input, out);
        }
        LogicalPlan::Project { input, exprs, .. } => {
            for (_, e) in exprs {
                expr_columns(e, out);
            }
            collect_used_names(input, out);
        }
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggs,
            ..
        } => {
            for g in group_by {
                expr_columns(g, out);
            }
            for a in aggs {
                for arg in &a.args {
                    expr_columns(arg, out);
                }
                if let Some(f) = &a.filter {
                    expr_columns(f, out);
                }
            }
            collect_used_names(input, out);
        }
        LogicalPlan::Join {
            left, right, on, ..
        } => {
            for (l, r) in on {
                out.insert(l.clone());
                out.insert(r.clone());
            }
            collect_used_names(left, out);
            collect_used_names(right, out);
        }
        LogicalPlan::Sort { input, keys } => {
            for (n, _, _) in keys {
                out.insert(n.clone());
            }
            collect_used_names(input, out);
        }
        LogicalPlan::Limit { input, .. } => collect_used_names(input, out),
        LogicalPlan::UnionAll { branches, .. } => {
            branches.iter().for_each(|b| collect_used_names(b, out))
        }
    }
}

/// The narrowest fixed-width field, so `count(*)` over an otherwise-unused table still scans
/// a non-empty column list (D0016 planner rule 1: a zero-column batch has `rows() == 0`).
fn narrowest_field(fields: &[exec::Field]) -> exec::Field {
    fn width(ty: &DataType) -> Option<u32> {
        match ty {
            DataType::Bool => Some(1),
            DataType::Date => Some(4),
            DataType::Int64 | DataType::UInt64 | DataType::Float64 | DataType::Timestamp => Some(8),
            DataType::Decimal(_) | DataType::Uuid | DataType::Ip => Some(16),
            DataType::String | DataType::Bytes | DataType::List(_) => None,
        }
    }
    fields
        .iter()
        .filter_map(|f| width(&f.ty).map(|w| (w, f)))
        .min_by_key(|(w, _)| *w)
        .map(|(_, f)| f.clone())
        .unwrap_or_else(|| fields[0].clone())
}

fn prune_scans(plan: LogicalPlan, used: &HashSet<String>) -> LogicalPlan {
    match plan {
        LogicalPlan::Scan { db, table, fields } => {
            let mut kept: Vec<exec::Field> = fields
                .iter()
                .filter(|f| used.contains(&f.name))
                .cloned()
                .collect();
            if kept.is_empty() {
                kept = vec![narrowest_field(&fields)];
            }
            LogicalPlan::Scan {
                db,
                table,
                fields: kept,
            }
        }
        LogicalPlan::Values { .. } => plan,
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(prune_scans(*input, used)),
            predicate,
        },
        LogicalPlan::Project {
            input,
            exprs,
            fields,
        } => LogicalPlan::Project {
            input: Box::new(prune_scans(*input, used)),
            exprs,
            fields,
        },
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggs,
            fields,
        } => LogicalPlan::Aggregate {
            input: Box::new(prune_scans(*input, used)),
            group_by,
            aggs,
            fields,
        },
        // A join's fields are its sides' fields, which pruning may have just narrowed.
        LogicalPlan::Join {
            left,
            right,
            kind,
            on,
            ..
        } => {
            let left = prune_scans(*left, used);
            let right = prune_scans(*right, used);
            let fields = left
                .fields()
                .iter()
                .chain(right.fields())
                .cloned()
                .collect();
            LogicalPlan::Join {
                left: Box::new(left),
                right: Box::new(right),
                kind,
                on,
                fields,
            }
        }
        LogicalPlan::Sort { input, keys } => LogicalPlan::Sort {
            input: Box::new(prune_scans(*input, used)),
            keys,
        },
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => LogicalPlan::Limit {
            input: Box::new(prune_scans(*input, used)),
            limit,
            offset,
        },
        LogicalPlan::UnionAll { branches, fields } => LogicalPlan::UnionAll {
            branches: branches.into_iter().map(|b| prune_scans(b, used)).collect(),
            fields,
        },
    }
}

// ── rule 3: predicate pushdown ───────────────────────────────────────────────

fn bexpr_conjuncts(e: BExpr) -> Vec<BExpr> {
    match e {
        BExpr::And(parts) => parts.into_iter().flat_map(bexpr_conjuncts).collect(),
        other => vec![other],
    }
}

fn and_all(mut exprs: Vec<BExpr>) -> BExpr {
    if exprs.len() == 1 {
        exprs.remove(0)
    } else {
        BExpr::And(exprs)
    }
}

fn subtree_prefixes(plan: &LogicalPlan) -> HashSet<String> {
    plan.fields().iter().map(|f| prefix_of(&f.name)).collect()
}

/// Walks every `Filter` node, splits it into conjuncts, and pushes each one as far down
/// through `Filter`/`Join` as the relations it touches allow (D0016 planner rule 3). A
/// conjunct that reaches a bare `Scan` is left wrapping it; `merge_filters` then folds any
/// stack of such wraps into one `Filter` so the final lowering sees a single predicate.
fn push_down(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Filter { input, predicate } => {
            let input = push_down(*input);
            bexpr_conjuncts(predicate)
                .into_iter()
                .fold(input, push_into)
        }
        LogicalPlan::Project {
            input,
            exprs,
            fields,
        } => LogicalPlan::Project {
            input: Box::new(push_down(*input)),
            exprs,
            fields,
        },
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggs,
            fields,
        } => LogicalPlan::Aggregate {
            input: Box::new(push_down(*input)),
            group_by,
            aggs,
            fields,
        },
        LogicalPlan::Join {
            left,
            right,
            kind,
            on,
            fields,
        } => LogicalPlan::Join {
            left: Box::new(push_down(*left)),
            right: Box::new(push_down(*right)),
            kind,
            on,
            fields,
        },
        LogicalPlan::Sort { input, keys } => LogicalPlan::Sort {
            input: Box::new(push_down(*input)),
            keys,
        },
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => LogicalPlan::Limit {
            input: Box::new(push_down(*input)),
            limit,
            offset,
        },
        LogicalPlan::UnionAll { branches, fields } => LogicalPlan::UnionAll {
            branches: branches.into_iter().map(push_down).collect(),
            fields,
        },
        other @ (LogicalPlan::Scan { .. } | LogicalPlan::Values { .. }) => other,
    }
}

fn push_into(plan: LogicalPlan, conjunct: BExpr) -> LogicalPlan {
    match plan {
        LogicalPlan::Join {
            left,
            right,
            kind,
            on,
            fields,
        } => {
            let mut used = HashSet::new();
            collect_prefixes(&conjunct, &mut used);
            let rp = subtree_prefixes(&right);
            if used.iter().all(|p| rp.contains(p)) {
                let right = Box::new(push_into(*right, conjunct));
                return LogicalPlan::Join {
                    left,
                    right,
                    kind,
                    on,
                    fields,
                };
            }
            let lp = subtree_prefixes(&left);
            if kind == exec::JoinKind::Inner && used.iter().all(|p| lp.contains(p)) {
                let left = Box::new(push_into(*left, conjunct));
                return LogicalPlan::Join {
                    left,
                    right,
                    kind,
                    on,
                    fields,
                };
            }
            LogicalPlan::Filter {
                input: Box::new(LogicalPlan::Join {
                    left,
                    right,
                    kind,
                    on,
                    fields,
                }),
                predicate: conjunct,
            }
        }
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(push_into(*input, conjunct)),
            predicate,
        },
        other => LogicalPlan::Filter {
            input: Box::new(other),
            predicate: conjunct,
        },
    }
}

/// Collapses a stack of `Filter`s (left by `push_into` re-wrapping the same node) into one,
/// so the final lowering only ever sees a single predicate directly over a `Scan`.
fn merge_filters(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Filter { input, predicate } => {
            let input = merge_filters(*input);
            if let LogicalPlan::Filter {
                input: inner,
                predicate: inner_pred,
            } = input
            {
                merge_filters(LogicalPlan::Filter {
                    input: inner,
                    predicate: and_all(vec![predicate, inner_pred]),
                })
            } else {
                LogicalPlan::Filter {
                    input: Box::new(input),
                    predicate,
                }
            }
        }
        LogicalPlan::Project {
            input,
            exprs,
            fields,
        } => LogicalPlan::Project {
            input: Box::new(merge_filters(*input)),
            exprs,
            fields,
        },
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggs,
            fields,
        } => LogicalPlan::Aggregate {
            input: Box::new(merge_filters(*input)),
            group_by,
            aggs,
            fields,
        },
        LogicalPlan::Join {
            left,
            right,
            kind,
            on,
            fields,
        } => LogicalPlan::Join {
            left: Box::new(merge_filters(*left)),
            right: Box::new(merge_filters(*right)),
            kind,
            on,
            fields,
        },
        LogicalPlan::Sort { input, keys } => LogicalPlan::Sort {
            input: Box::new(merge_filters(*input)),
            keys,
        },
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => LogicalPlan::Limit {
            input: Box::new(merge_filters(*input)),
            limit,
            offset,
        },
        LogicalPlan::UnionAll { branches, fields } => LogicalPlan::UnionAll {
            branches: branches.into_iter().map(merge_filters).collect(),
            fields,
        },
        other @ (LogicalPlan::Scan { .. } | LogicalPlan::Values { .. }) => other,
    }
}

// ── lowering ──────────────────────────────────────────────────────────────────

/// Recovers a base table's real column name from its internal one (`"r0.id"` → `"id"`):
/// `ScanSpec.columns` names real storage columns, never the binder's disambiguating prefix.
/// Splits on the *first* `.` only, so a real column name containing dots (e.g.
/// `"resource.run.id"`, seen in the OTel fixtures) round-trips correctly.
fn real_name(internal: &str) -> String {
    internal
        .split_once('.')
        .map(|(_, n)| n.to_string())
        .unwrap_or_else(|| internal.to_string())
}

fn column_positions(exprs: &[BExpr], fields: &[exec::Field]) -> Result<Vec<usize>, SqlError> {
    exprs
        .iter()
        .map(|e| match e {
            BExpr::Column(name) => fields
                .iter()
                .position(|f| &f.name == name)
                .ok_or_else(|| SqlError::Plan(format!("internal error: unresolved column {name}"))),
            _ => Err(SqlError::Plan(
                "internal error: expected a plain column at plan time".into(),
            )),
        })
        .collect()
}

fn lower_sort_keys(
    keys: &[(String, bool, bool)],
    fields: &[exec::Field],
) -> Result<Vec<exec::SortKey>, SqlError> {
    keys.iter()
        .map(|(name, desc, nulls_first)| {
            let column = fields.iter().position(|f| &f.name == name).ok_or_else(|| {
                SqlError::Plan(format!("internal error: unresolved sort key {name}"))
            })?;
            Ok(exec::SortKey {
                column,
                descending: *desc,
                nulls_first: *nulls_first,
            })
        })
        .collect()
}

fn rows_to_batch(fields: &[exec::Field], rows: &[Vec<Value>]) -> Result<exec::Batch, SqlError> {
    let mut builders: Vec<exec::ColumnBuilder> = fields
        .iter()
        .map(|f| exec::ColumnBuilder::with_capacity(f.ty.clone(), rows.len()))
        .collect();
    for row in rows {
        for (b, v) in builders.iter_mut().zip(row) {
            b.push(v).map_err(|e| SqlError::Plan(e.to_string()))?;
        }
    }
    let cols = builders
        .into_iter()
        .map(exec::ColumnBuilder::finish)
        .collect();
    exec::Batch::new(fields.to_vec(), cols).map_err(|e| SqlError::Plan(e.to_string()))
}

/// Rule 7: for an INNER join, an approximate row-count estimate (a table's own count via
/// `est`, halved per `Filter` layer above it, summed across a join's two sides) decides which
/// side is smaller; that side is put on the right (the build side, `exec::Plan::Join`'s
/// convention).
fn estimate_rows(plan: &LogicalPlan, est: &RowEstimate) -> u64 {
    match plan {
        LogicalPlan::Scan { db, table, .. } => est(db, table),
        LogicalPlan::Filter { input, .. } => (estimate_rows(input, est) / 2).max(1),
        LogicalPlan::Project { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Aggregate { input, .. } => estimate_rows(input, est),
        LogicalPlan::Limit { input, limit, .. } => {
            limit.map_or_else(|| estimate_rows(input, est), |n| n as u64)
        }
        LogicalPlan::Join { left, right, .. } => {
            estimate_rows(left, est).saturating_add(estimate_rows(right, est))
        }
        LogicalPlan::UnionAll { branches, .. } => {
            branches.iter().map(|b| estimate_rows(b, est)).sum()
        }
        LogicalPlan::Values { rows, .. } => rows.len() as u64,
    }
}

fn maybe_swap(
    left: LogicalPlan,
    right: LogicalPlan,
    on: Vec<(String, String)>,
    kind: exec::JoinKind,
    est: &RowEstimate,
) -> (LogicalPlan, LogicalPlan, Vec<(String, String)>, bool) {
    if kind != exec::JoinKind::Inner || estimate_rows(&right, est) <= estimate_rows(&left, est) {
        return (left, right, on, false);
    }
    let swapped_on = on.into_iter().map(|(l, r)| (r, l)).collect();
    (right, left, swapped_on, true)
}

fn lower_plan(plan: &LogicalPlan, est: &RowEstimate) -> Result<exec::Plan, SqlError> {
    Ok(match plan {
        LogicalPlan::Scan { db, table, fields } => {
            let columns = fields.iter().map(|f| real_name(&f.name)).collect();
            exec::Plan::Scan(exec::ScanSpec {
                db: db.clone(),
                table: table.clone(),
                columns,
                predicate: None,
            })
        }
        LogicalPlan::Values { fields, rows } => exec::Plan::Values {
            fields: fields.clone(),
            batches: vec![rows_to_batch(fields, rows)?],
        },
        LogicalPlan::Filter { input, predicate } => {
            if let LogicalPlan::Scan { .. } = input.as_ref() {
                let lowered = lower(predicate, input.fields())?;
                let mut inner = lower_plan(input, est)?;
                if let exec::Plan::Scan(spec) = &mut inner {
                    spec.predicate = Some(lowered.clone());
                }
                exec::Plan::Filter {
                    input: Box::new(inner),
                    predicate: lowered,
                }
            } else {
                let inner = lower_plan(input, est)?;
                let lowered = lower(predicate, input.fields())?;
                exec::Plan::Filter {
                    input: Box::new(inner),
                    predicate: lowered,
                }
            }
        }
        LogicalPlan::Project { input, exprs, .. } => {
            let inner = lower_plan(input, est)?;
            let input_fields = input.fields();
            let lowered_exprs = exprs
                .iter()
                .map(|(n, e)| Ok((n.clone(), lower(e, input_fields)?)))
                .collect::<Result<_, SqlError>>()?;
            exec::Plan::Project {
                input: Box::new(inner),
                exprs: lowered_exprs,
            }
        }
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggs,
            ..
        } => {
            let inner = lower_plan(input, est)?;
            let input_fields = input.fields();
            let group_by = column_positions(group_by, input_fields)?;
            let mut agg_calls = Vec::with_capacity(aggs.len());
            for a in aggs {
                let args = column_positions(&a.args, input_fields)?;
                let filter = match &a.filter {
                    Some(f) => Some(column_positions(std::slice::from_ref(f), input_fields)?[0]),
                    None => None,
                };
                agg_calls.push(exec::AggCall {
                    func: a.func.clone(),
                    args,
                    filter,
                    name: a.name.clone(),
                });
            }
            exec::Plan::Aggregate {
                input: Box::new(inner),
                group_by,
                aggs: agg_calls,
            }
        }
        LogicalPlan::Join {
            left,
            right,
            kind,
            on,
            fields,
        } => {
            let (new_left, new_right, new_on, swapped) =
                maybe_swap((**left).clone(), (**right).clone(), on.clone(), *kind, est);
            let left_fields = new_left.fields().to_vec();
            let right_fields = new_right.fields().to_vec();
            let lowered_left = lower_plan(&new_left, est)?;
            let lowered_right = lower_plan(&new_right, est)?;
            let on_idx: Vec<(usize, usize)> = new_on
                .iter()
                .map(|(l, r)| {
                    let li = left_fields
                        .iter()
                        .position(|f| &f.name == l)
                        .ok_or_else(|| {
                            SqlError::Plan(format!("internal error: join key {l} not found"))
                        })?;
                    let ri = right_fields
                        .iter()
                        .position(|f| &f.name == r)
                        .ok_or_else(|| {
                            SqlError::Plan(format!("internal error: join key {r} not found"))
                        })?;
                    Ok((li, ri))
                })
                .collect::<Result<_, SqlError>>()?;
            let joined = exec::Plan::Join {
                left: Box::new(lowered_left),
                right: Box::new(lowered_right),
                kind: *kind,
                on: on_idx,
            };
            if swapped {
                let mut combined = left_fields;
                combined.extend(right_fields);
                let exprs =
                    fields
                        .iter()
                        .map(|f| {
                            let pos = combined.iter().position(|c| c.name == f.name).ok_or_else(
                                || {
                                    SqlError::Plan(format!(
                                        "internal error: join column {} missing after swap",
                                        f.name
                                    ))
                                },
                            )?;
                            Ok((f.name.clone(), exec::Expr::Column(pos)))
                        })
                        .collect::<Result<_, SqlError>>()?;
                exec::Plan::Project {
                    input: Box::new(joined),
                    exprs,
                }
            } else {
                joined
            }
        }
        LogicalPlan::Sort { input, keys } => {
            let inner = lower_plan(input, est)?;
            let sort_keys = lower_sort_keys(keys, input.fields())?;
            exec::Plan::Sort {
                input: Box::new(inner),
                keys: sort_keys,
            }
        }
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => {
            if let LogicalPlan::Sort {
                input: sort_input,
                keys,
            } = input.as_ref()
                && let Some(n) = limit
            {
                let inner = lower_plan(sort_input, est)?;
                let sort_keys = lower_sort_keys(keys, sort_input.fields())?;
                let k = (*n).saturating_add(*offset).max(0) as usize;
                let topk = exec::Plan::TopK {
                    input: Box::new(inner),
                    keys: sort_keys,
                    k,
                };
                exec::Plan::Limit {
                    input: Box::new(topk),
                    limit: Some((*n).max(0) as usize),
                    offset: (*offset).max(0) as usize,
                }
            } else {
                let inner = lower_plan(input, est)?;
                exec::Plan::Limit {
                    input: Box::new(inner),
                    limit: limit.map(|n| n.max(0) as usize),
                    offset: (*offset).max(0) as usize,
                }
            }
        }
        LogicalPlan::UnionAll { branches, .. } => exec::Plan::UnionAll(
            branches
                .iter()
                .map(|b| lower_plan(b, est))
                .collect::<Result<_, SqlError>>()?,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn est(_db: &str, _table: &str) -> u64 {
        100
    }

    fn field(name: &str, ty: DataType) -> exec::Field {
        exec::Field {
            name: name.to_string(),
            ty,
        }
    }

    #[test]
    fn no_from_becomes_values_with_one_dummy_row() {
        let plan = LogicalPlan::Project {
            input: Box::new(LogicalPlan::Values {
                fields: vec![field("_dummy", DataType::Bool)],
                rows: vec![vec![Value::Bool(true)]],
            }),
            exprs: vec![(
                "c".to_string(),
                BExpr::Literal(Value::Int64(1), DataType::Int64),
            )],
            fields: vec![field("c", DataType::Int64)],
        };
        let out = plan_test(plan);
        assert!(matches!(out, exec::Plan::Project { .. }));
    }

    fn plan_test(logical: LogicalPlan) -> exec::Plan {
        plan(logical, &est).unwrap()
    }

    #[test]
    fn constant_folding_reduces_one_plus_two_to_a_literal() {
        let expr = BExpr::Arith(
            exec::ArithOp::Add,
            Box::new(BExpr::Literal(Value::Int64(1), DataType::Int64)),
            Box::new(BExpr::Literal(Value::Int64(2), DataType::Int64)),
        );
        let folded = fold_expr(expr);
        assert!(matches!(folded, BExpr::Literal(Value::Int64(3), _)));
    }

    #[test]
    fn count_star_scans_a_non_empty_column_list() {
        let scan = LogicalPlan::Scan {
            db: "main".to_string(),
            table: "t".to_string(),
            fields: vec![
                field("r0.a", DataType::Int64),
                field("r0.b", DataType::String),
            ],
        };
        let agg = LogicalPlan::Aggregate {
            input: Box::new(scan),
            group_by: vec![],
            aggs: vec![LAgg {
                func: exec::AggFunc::CountStar,
                args: vec![],
                filter: None,
                name: "n".to_string(),
            }],
            fields: vec![field("n", DataType::Int64)],
        };
        let out = plan_test(agg);
        let exec::Plan::Aggregate { input, .. } = out else {
            panic!("expected Aggregate")
        };
        let exec::Plan::Scan(spec) = *input else {
            panic!("expected Scan")
        };
        assert!(!spec.columns.is_empty());
    }

    #[test]
    fn pushdown_sets_scan_predicate_and_keeps_the_filter() {
        let scan = LogicalPlan::Scan {
            db: "main".to_string(),
            table: "t".to_string(),
            fields: vec![field("r0.a", DataType::Int64)],
        };
        let predicate = BExpr::Cmp(
            exec::CmpOp::Gt,
            Box::new(BExpr::Column("r0.a".to_string())),
            Box::new(BExpr::Literal(Value::Int64(1), DataType::Int64)),
        );
        let filter = LogicalPlan::Filter {
            input: Box::new(scan),
            predicate,
        };
        let out = plan_test(filter);
        let exec::Plan::Filter { input, .. } = out else {
            panic!("expected Filter")
        };
        let exec::Plan::Scan(spec) = *input else {
            panic!("expected Scan")
        };
        assert!(spec.predicate.is_some());
    }

    #[test]
    fn order_by_limit_becomes_topk_then_limit() {
        let scan = LogicalPlan::Scan {
            db: "main".to_string(),
            table: "t".to_string(),
            fields: vec![field("r0.a", DataType::Int64)],
        };
        let sort = LogicalPlan::Sort {
            input: Box::new(scan),
            keys: vec![("r0.a".to_string(), false, false)],
        };
        let limit = LogicalPlan::Limit {
            input: Box::new(sort),
            limit: Some(3),
            offset: 0,
        };
        let out = plan_test(limit);
        let exec::Plan::Limit { input, .. } = out else {
            panic!("expected Limit")
        };
        assert!(matches!(*input, exec::Plan::TopK { .. }));
    }

    fn join_of(left_db: &str, right_db: &str, kind: exec::JoinKind) -> LogicalPlan {
        let scan = |db: &str, name: &str| LogicalPlan::Scan {
            db: db.to_string(),
            table: "t".to_string(),
            fields: vec![field(name, DataType::Int64)],
        };
        LogicalPlan::Join {
            left: Box::new(scan(left_db, "r0.k")),
            right: Box::new(scan(right_db, "r1.k")),
            kind,
            on: vec![("r0.k".to_string(), "r1.k".to_string())],
            fields: vec![
                field("r0.k", DataType::Int64),
                field("r1.k", DataType::Int64),
            ],
        }
    }

    fn rows_by_db(db: &str, _table: &str) -> u64 {
        if db == "big" { 1000 } else { 10 }
    }

    #[test]
    fn inner_join_moves_the_smaller_side_to_the_build_side() {
        // Written small-left: the swap puts `big` on the probe (left) side, then a Project
        // restores the written column order.
        let out = plan(join_of("small", "big", exec::JoinKind::Inner), &rows_by_db).unwrap();
        let exec::Plan::Project { input, exprs } = out else {
            panic!("expected a restoring Project, got {out:?}")
        };
        let exec::Plan::Join { left, on, .. } = *input else {
            panic!("expected a Join under the Project")
        };
        assert!(matches!(*left, exec::Plan::Scan(ref s) if s.db == "big"));
        assert_eq!(on, vec![(0, 0)]);
        let names: Vec<_> = exprs.iter().map(|(n, e)| (n.as_str(), e.clone())).collect();
        assert_eq!(
            names,
            vec![
                ("r0.k", exec::Expr::Column(1)),
                ("r1.k", exec::Expr::Column(0))
            ]
        );
    }

    #[test]
    fn inner_join_keeps_sides_when_the_right_is_already_smaller_and_left_join_never_swaps() {
        let kept = plan(join_of("big", "small", exec::JoinKind::Inner), &rows_by_db).unwrap();
        assert!(matches!(kept, exec::Plan::Join { .. }));
        let left = plan(join_of("small", "big", exec::JoinKind::Left), &rows_by_db).unwrap();
        assert!(matches!(left, exec::Plan::Join { .. }));
    }
}
