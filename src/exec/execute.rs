//! Plan execution entry point (SPEC §7): compiles a `Plan` into pipelines of operators and
//! sinks (`stream`), then drains the result into batches (`materialize`).

use std::sync::Arc;

use super::agg::HashAggregateSink;
use super::context::{ExecContext, ExecError, ExecOptions};
use super::join::{HashJoinProbe, JoinBuildSink};
use super::operator::{BatchSource, MorselSource, Operator, ScanStats, Sink, TableSource};
use super::ops::{CollectSink, Filter, LimitSink, Project, SortSink, TopKSink, UnionSource};
use super::pipeline;
use super::plan::{Plan, SortKey};
use super::{Batch, Field};

#[derive(Debug)]
pub struct QueryResult {
    pub fields: Vec<Field>,
    pub batches: Vec<Batch>,
    pub stats: QueryStats,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryStats {
    pub scan: ScanStats,
    pub peak_memory: usize,
}

/// Builds one worker's fresh `Operator` for a stage of the pipeline; called once per worker
/// inside a `pipeline::run` factory, so it must not share state across calls.
type OpFactory = Arc<dyn Fn(&ExecContext) -> Result<Box<dyn Operator>, ExecError> + Send + Sync>;

/// A `MorselSource` plus the streaming operators still to run over it, and the fields the
/// last operator (or the source itself, with none) emits.
struct Stream<'a> {
    source: Box<dyn MorselSource + 'a>,
    ops: Vec<OpFactory>,
    fields: Vec<Field>,
}

pub fn execute(
    source: &dyn TableSource,
    plan: &Plan,
    opts: &ExecOptions,
) -> Result<QueryResult, ExecError> {
    let ctx = ExecContext::new(opts);
    let threads = opts.threads.max(1);
    let mut stats = ScanStats::default();
    let s = stream(plan, source, &ctx, threads, &mut stats)?;
    let fields = s.fields.clone();
    let batches = materialize(s, &ctx, threads, &mut stats)?;
    Ok(QueryResult {
        fields,
        batches,
        stats: QueryStats {
            scan: stats,
            peak_memory: ctx.peak_memory(),
        },
    })
}

/// Compiles one `Plan` node (and, recursively, its inputs) into a `Stream`. A node that ends
/// a pipeline (aggregate, sort, top-k, limit, the join build side) runs its input to a `Sink`
/// via `run_stream` and restarts as a fresh, op-less `Stream` over the result.
fn stream<'a>(
    p: &Plan,
    src: &'a dyn TableSource,
    ctx: &ExecContext,
    threads: usize,
    stats: &mut ScanStats,
) -> Result<Stream<'a>, ExecError> {
    match p {
        Plan::Scan(spec) => {
            let source = src.open_scan(spec, ctx)?;
            let fields = source.fields().to_vec();
            let mut ops: Vec<OpFactory> = Vec::new();
            if let Some(pred) = &spec.predicate {
                pred.data_type(&fields)?;
                let pred = pred.clone();
                ops.push(Arc::new(
                    move |_ctx: &ExecContext| -> Result<Box<dyn Operator>, ExecError> {
                        Ok(Box::new(Filter::new(pred.clone())) as Box<dyn Operator>)
                    },
                ));
            }
            Ok(Stream {
                source,
                ops,
                fields,
            })
        }

        Plan::Values { fields, batches } => Ok(Stream {
            source: Box::new(BatchSource::new(fields.clone(), batches.clone())),
            ops: Vec::new(),
            fields: fields.clone(),
        }),

        Plan::Filter { input, predicate } => {
            let mut s = stream(input, src, ctx, threads, stats)?;
            predicate.data_type(&s.fields)?;
            let predicate = predicate.clone();
            s.ops.push(Arc::new(
                move |_ctx: &ExecContext| -> Result<Box<dyn Operator>, ExecError> {
                    Ok(Box::new(Filter::new(predicate.clone())) as Box<dyn Operator>)
                },
            ));
            Ok(s)
        }

        Plan::Project { input, exprs } => {
            let mut s = stream(input, src, ctx, threads, stats)?;
            let input_fields = s.fields.clone();
            let new_fields = Project::new(exprs.clone(), &input_fields)?
                .fields()
                .to_vec();
            let exprs = exprs.clone();
            s.ops.push(Arc::new(
                move |_ctx: &ExecContext| -> Result<Box<dyn Operator>, ExecError> {
                    Ok(Box::new(Project::new(exprs.clone(), &input_fields)?) as Box<dyn Operator>)
                },
            ));
            s.fields = new_fields;
            Ok(s)
        }

        Plan::Aggregate {
            input,
            group_by,
            aggs,
        } => {
            let input_stream = stream(input, src, ctx, threads, stats)?;
            let input_fields = input_stream.fields.clone();
            let group_by = group_by.clone();
            let aggs = aggs.clone();
            let out_fields = HashAggregateSink::new(&input_fields, group_by.clone(), aggs.clone())?
                .fields()
                .to_vec();
            let sink: HashAggregateSink =
                run_stream(input_stream, ctx, threads, stats, move || {
                    HashAggregateSink::new(&input_fields, group_by.clone(), aggs.clone())
                })?;
            let batches = sink.finish(ctx)?;
            Ok(Stream {
                source: Box::new(BatchSource::new(out_fields.clone(), batches)),
                ops: Vec::new(),
                fields: out_fields,
            })
        }

        Plan::Join {
            left,
            right,
            kind,
            on,
        } => {
            let right_stream = stream(right, src, ctx, threads, stats)?;
            let right_fields = right_stream.fields.clone();
            let left_stream = stream(left, src, ctx, threads, stats)?;
            let left_fields = left_stream.fields.clone();
            for &(l, r) in on {
                if l >= left_fields.len() {
                    return Err(ExecError::Plan(format!(
                        "join left column {l} out of range for {} inputs",
                        left_fields.len()
                    )));
                }
                if r >= right_fields.len() {
                    return Err(ExecError::Plan(format!(
                        "join right column {r} out of range for {} inputs",
                        right_fields.len()
                    )));
                }
            }
            let right_keys: Vec<usize> = on.iter().map(|&(_, r)| r).collect();
            let left_keys: Vec<usize> = on.iter().map(|&(l, _)| l).collect();

            let build_fields = right_fields.clone();
            let build_keys = right_keys.clone();
            let build_sink: JoinBuildSink =
                run_stream(right_stream, ctx, threads, stats, move || {
                    Ok(JoinBuildSink::new(build_fields.clone(), build_keys.clone()))
                })?;
            let table = Arc::new(build_sink.into_table(ctx)?);

            // Built once outside the factory to type-check `on` and compute the joined fields.
            let probe_fields = HashJoinProbe::new(
                Arc::clone(&table),
                &left_fields,
                left_keys.clone(),
                *kind,
                ctx,
            )?
            .fields()
            .to_vec();

            let mut s = left_stream;
            let kind = *kind;
            s.ops.push(Arc::new(
                move |ctx: &ExecContext| -> Result<Box<dyn Operator>, ExecError> {
                    let probe = HashJoinProbe::new(
                        Arc::clone(&table),
                        &left_fields,
                        left_keys.clone(),
                        kind,
                        ctx,
                    )?;
                    Ok(Box::new(probe) as Box<dyn Operator>)
                },
            ));
            s.fields = probe_fields;
            Ok(s)
        }

        Plan::Sort { input, keys } => {
            let input_stream = stream(input, src, ctx, threads, stats)?;
            let fields = input_stream.fields.clone();
            check_sort_keys(keys, &fields)?;
            let out_fields = fields.clone();
            let keys = keys.clone();
            let sink: SortSink = run_stream(input_stream, ctx, threads, stats, move || {
                Ok(SortSink::new(fields.clone(), keys.clone()))
            })?;
            let batches = sink.finish(ctx)?;
            Ok(Stream {
                source: Box::new(BatchSource::new(out_fields.clone(), batches)),
                ops: Vec::new(),
                fields: out_fields,
            })
        }

        Plan::TopK { input, keys, k } => {
            let input_stream = stream(input, src, ctx, threads, stats)?;
            let fields = input_stream.fields.clone();
            check_sort_keys(keys, &fields)?;
            let out_fields = fields.clone();
            let keys = keys.clone();
            let k = *k;
            let sink: TopKSink = run_stream(input_stream, ctx, threads, stats, move || {
                Ok(TopKSink::new(fields.clone(), keys.clone(), k))
            })?;
            let batches = sink.finish(ctx)?;
            Ok(Stream {
                source: Box::new(BatchSource::new(out_fields.clone(), batches)),
                ops: Vec::new(),
                fields: out_fields,
            })
        }

        Plan::Limit {
            input,
            limit,
            offset,
        } => {
            let input_stream = stream(input, src, ctx, threads, stats)?;
            let fields = input_stream.fields.clone();
            let out_fields = fields.clone();
            let limit = *limit;
            let offset = *offset;
            let sink: LimitSink = run_stream(input_stream, ctx, threads, stats, move || {
                Ok(LimitSink::new(fields.clone(), limit, offset))
            })?;
            let batches = sink.finish(ctx)?;
            Ok(Stream {
                source: Box::new(BatchSource::new(out_fields.clone(), batches)),
                ops: Vec::new(),
                fields: out_fields,
            })
        }

        Plan::UnionAll(inputs) => {
            let mut sources: Vec<Box<dyn MorselSource + 'a>> = Vec::with_capacity(inputs.len());
            for input in inputs {
                let s = stream(input, src, ctx, threads, stats)?;
                if s.ops.is_empty() {
                    sources.push(s.source);
                } else {
                    let fields = s.fields.clone();
                    let batches = materialize(s, ctx, threads, stats)?;
                    sources.push(Box::new(BatchSource::new(fields, batches)));
                }
            }
            let union = UnionSource::new(sources)?;
            let fields = union.fields().to_vec();
            Ok(Stream {
                source: Box::new(union),
                ops: Vec::new(),
                fields,
            })
        }
    }
}

/// Runs `s`'s source through its own ops and a fresh `make_sink()` per worker, merging the
/// partials (SPEC §7's per-query scheduler). Adds `s`'s source stats to `stats` either way.
fn run_stream<S, F>(
    s: Stream<'_>,
    ctx: &ExecContext,
    threads: usize,
    stats: &mut ScanStats,
    make_sink: F,
) -> Result<S, ExecError>
where
    S: Sink,
    F: Fn() -> Result<S, ExecError> + Sync,
{
    let Stream { source, ops, .. } = s;
    let make = || -> Result<(Vec<Box<dyn Operator>>, S), ExecError> {
        let built = ops.iter().map(|f| f(ctx)).collect::<Result<Vec<_>, _>>()?;
        Ok((built, make_sink()?))
    };
    let result = pipeline::run(source.as_ref(), ctx, threads, make);
    stats.add(&source.stats());
    result
}

/// The top-level drain: runs `s` into a `CollectSink` and returns its rechunked batches.
fn materialize(
    s: Stream<'_>,
    ctx: &ExecContext,
    threads: usize,
    stats: &mut ScanStats,
) -> Result<Vec<Batch>, ExecError> {
    let fields = s.fields.clone();
    let sink: CollectSink = run_stream(s, ctx, threads, stats, move || {
        Ok(CollectSink::new(fields.clone()))
    })?;
    sink.finish(ctx)
}

/// `SortSink`/`TopKSink` take `SortKey`s without validating them, so `stream` checks the
/// column range itself (the "column index out of range is `Plan`" rule, SPEC §7).
fn check_sort_keys(keys: &[SortKey], fields: &[Field]) -> Result<(), ExecError> {
    for key in keys {
        if key.column >= fields.len() {
            return Err(ExecError::Plan(format!(
                "ORDER BY column {} out of range for {} inputs",
                key.column,
                fields.len()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::exec::operator::ScanSpec;
    use crate::exec::plan::{AggCall, AggFunc, JoinKind};
    use crate::exec::{CancelToken, CmpOp, Column, Expr};
    use crate::types::{DataType, Value};

    fn field(name: &str, ty: DataType) -> Field {
        Field {
            name: name.to_string(),
            ty,
        }
    }

    fn int_batch(f: &Field, values: &[i64]) -> Batch {
        let vals: Vec<Value> = values.iter().copied().map(Value::Int64).collect();
        Batch::new(
            vec![f.clone()],
            vec![Column::from_values(&f.ty, &vals).unwrap()],
        )
        .unwrap()
    }

    fn int_values(batches: &[Batch], col: usize) -> Vec<i64> {
        batches
            .iter()
            .flat_map(|b| {
                (0..b.rows()).map(move |i| match b.column(col).get(i) {
                    Value::Int64(v) => v,
                    other => panic!("expected Int64, got {other:?}"),
                })
            })
            .collect()
    }

    /// Maps `(db, table)` to fields plus batches. `open_scan` projects by column name and
    /// ignores the predicate, as the scan contract (pruning only) allows.
    struct FakeSource {
        tables: HashMap<(String, String), (Vec<Field>, Vec<Batch>)>,
    }

    impl FakeSource {
        fn new() -> Self {
            FakeSource {
                tables: HashMap::new(),
            }
        }

        fn with_table(
            mut self,
            db: &str,
            table: &str,
            fields: Vec<Field>,
            batches: Vec<Batch>,
        ) -> Self {
            self.tables
                .insert((db.to_string(), table.to_string()), (fields, batches));
            self
        }
    }

    impl TableSource for FakeSource {
        fn open_scan<'a>(
            &'a self,
            spec: &ScanSpec,
            _ctx: &ExecContext,
        ) -> Result<Box<dyn MorselSource + 'a>, ExecError> {
            let (fields, batches) = self
                .tables
                .get(&(spec.db.clone(), spec.table.clone()))
                .ok_or_else(|| {
                    ExecError::Plan(format!("unknown table {}.{}", spec.db, spec.table))
                })?;
            let mut idxs = Vec::with_capacity(spec.columns.len());
            let mut out_fields = Vec::with_capacity(spec.columns.len());
            for name in &spec.columns {
                let pos = fields
                    .iter()
                    .position(|f| &f.name == name)
                    .ok_or_else(|| ExecError::Plan(format!("unknown column {name}")))?;
                idxs.push(pos);
                out_fields.push(fields[pos].clone());
            }
            let projected: Vec<Batch> = batches
                .iter()
                .map(|b| {
                    let cols = idxs.iter().map(|&i| b.column(i).clone()).collect();
                    Batch::new(out_fields.clone(), cols).unwrap()
                })
                .collect();
            Ok(Box::new(BatchSource::new(out_fields, projected)))
        }
    }

    fn scan(db: &str, table: &str, columns: &[&str], predicate: Option<Expr>) -> Plan {
        Plan::Scan(ScanSpec {
            db: db.to_string(),
            table: table.to_string(),
            columns: columns.iter().map(|s| s.to_string()).collect(),
            predicate,
        })
    }

    fn count_star(name: &str) -> AggCall {
        AggCall {
            func: AggFunc::CountStar,
            args: Vec::new(),
            filter: None,
            name: name.to_string(),
        }
    }

    #[test]
    fn scan_variant_reads_the_source() {
        let f = field("a", DataType::Int64);
        let src = FakeSource::new().with_table(
            "d",
            "t",
            vec![f.clone()],
            vec![int_batch(&f, &[1, 2, 3])],
        );
        let plan = scan("d", "t", &["a"], None);
        let out = execute(&src, &plan, &ExecOptions::default()).unwrap();
        assert_eq!(out.fields, vec![f]);
        assert_eq!(int_values(&out.batches, 0), vec![1, 2, 3]);
    }

    #[test]
    fn values_variant_returns_its_own_rows() {
        let f = field("a", DataType::Int64);
        let src = FakeSource::new();
        let plan = Plan::Values {
            fields: vec![f.clone()],
            batches: vec![int_batch(&f, &[7, 8])],
        };
        let out = execute(&src, &plan, &ExecOptions::default()).unwrap();
        assert_eq!(int_values(&out.batches, 0), vec![7, 8]);
    }

    #[test]
    fn filter_variant_keeps_matching_rows() {
        let f = field("a", DataType::Int64);
        let src = FakeSource::new();
        let input = Plan::Values {
            fields: vec![f.clone()],
            batches: vec![int_batch(&f, &[1, 2, 3])],
        };
        let predicate = Expr::cmp(
            CmpOp::Gt,
            Expr::col(0),
            Expr::lit(Value::Int64(1), DataType::Int64),
        );
        let plan = Plan::Filter {
            input: Box::new(input),
            predicate,
        };
        let out = execute(&src, &plan, &ExecOptions::default()).unwrap();
        assert_eq!(int_values(&out.batches, 0), vec![2, 3]);
    }

    #[test]
    fn project_variant_evaluates_exprs() {
        let f = field("a", DataType::Int64);
        let src = FakeSource::new();
        let input = Plan::Values {
            fields: vec![f.clone()],
            batches: vec![int_batch(&f, &[1, 2])],
        };
        let plan = Plan::Project {
            input: Box::new(input),
            exprs: vec![("b".to_string(), Expr::col(0))],
        };
        let out = execute(&src, &plan, &ExecOptions::default()).unwrap();
        assert_eq!(out.fields, vec![field("b", DataType::Int64)]);
        assert_eq!(int_values(&out.batches, 0), vec![1, 2]);
    }

    #[test]
    fn aggregate_variant_groups_and_counts() {
        let f = field("a", DataType::Int64);
        let src = FakeSource::new();
        let input = Plan::Values {
            fields: vec![f.clone()],
            batches: vec![int_batch(&f, &[1, 1, 2])],
        };
        let plan = Plan::Aggregate {
            input: Box::new(input),
            group_by: vec![0],
            aggs: vec![count_star("n")],
        };
        let out = execute(&src, &plan, &ExecOptions::default()).unwrap();
        let mut rows: Vec<(i64, i64)> = (0..out.batches[0].rows())
            .map(|i| {
                let a = int_at(&out.batches[0], 0, i);
                let n = int_at(&out.batches[0], 1, i);
                (a, n)
            })
            .collect();
        rows.sort();
        assert_eq!(rows, vec![(1, 2), (2, 1)]);
    }

    #[test]
    fn sort_variant_orders_rows() {
        let f = field("a", DataType::Int64);
        let src = FakeSource::new();
        let input = Plan::Values {
            fields: vec![f.clone()],
            batches: vec![int_batch(&f, &[3, 1, 2])],
        };
        let plan = Plan::Sort {
            input: Box::new(input),
            keys: vec![SortKey::asc(0)],
        };
        let out = execute(&src, &plan, &ExecOptions::default()).unwrap();
        assert_eq!(int_values(&out.batches, 0), vec![1, 2, 3]);
    }

    #[test]
    fn topk_variant_keeps_the_smallest_k() {
        let f = field("a", DataType::Int64);
        let src = FakeSource::new();
        let input = Plan::Values {
            fields: vec![f.clone()],
            batches: vec![int_batch(&f, &[5, 3, 8, 1])],
        };
        let plan = Plan::TopK {
            input: Box::new(input),
            keys: vec![SortKey::asc(0)],
            k: 2,
        };
        let out = execute(&src, &plan, &ExecOptions::default()).unwrap();
        assert_eq!(int_values(&out.batches, 0), vec![1, 3]);
    }

    #[test]
    fn limit_variant_applies_offset_and_limit() {
        let f = field("a", DataType::Int64);
        let src = FakeSource::new();
        let input = Plan::Values {
            fields: vec![f.clone()],
            batches: vec![int_batch(&f, &[0, 1, 2, 3, 4])],
        };
        let plan = Plan::Limit {
            input: Box::new(input),
            limit: Some(2),
            offset: 1,
        };
        let out = execute(&src, &plan, &ExecOptions::default()).unwrap();
        assert_eq!(int_values(&out.batches, 0), vec![1, 2]);
    }

    #[test]
    fn union_all_variant_of_a_scan_and_values() {
        let f = field("a", DataType::Int64);
        let src =
            FakeSource::new().with_table("d", "t", vec![f.clone()], vec![int_batch(&f, &[1])]);
        let scan_plan = scan("d", "t", &["a"], None);
        let values_plan = Plan::Values {
            fields: vec![f.clone()],
            batches: vec![int_batch(&f, &[2, 3])],
        };
        let plan = Plan::UnionAll(vec![scan_plan, values_plan]);
        let out = execute(&src, &plan, &ExecOptions::default()).unwrap();
        let mut got = int_values(&out.batches, 0);
        got.sort_unstable();
        assert_eq!(got, vec![1, 2, 3]);
    }

    fn int_at(b: &Batch, col: usize, row: usize) -> i64 {
        match b.column(col).get(row) {
            Value::Int64(v) => v,
            other => panic!("expected Int64, got {other:?}"),
        }
    }

    #[test]
    fn join_left_over_two_scans() {
        let lk = field("lk", DataType::Int64);
        let lv = field("lv", DataType::Int64);
        let rk = field("rk", DataType::Int64);
        let rv = field("rv", DataType::Int64);
        let left_batch = Batch::new(
            vec![lk.clone(), lv.clone()],
            vec![
                Column::from_values(&DataType::Int64, &[Value::Int64(1), Value::Int64(2)]).unwrap(),
                Column::from_values(&DataType::Int64, &[Value::Int64(10), Value::Int64(20)])
                    .unwrap(),
            ],
        )
        .unwrap();
        let right_batch = Batch::new(
            vec![rk.clone(), rv.clone()],
            vec![
                Column::from_values(&DataType::Int64, &[Value::Int64(1)]).unwrap(),
                Column::from_values(&DataType::Int64, &[Value::Int64(100)]).unwrap(),
            ],
        )
        .unwrap();
        let src = FakeSource::new()
            .with_table("d", "l", vec![lk.clone(), lv.clone()], vec![left_batch])
            .with_table("d", "r", vec![rk.clone(), rv.clone()], vec![right_batch]);
        let plan = Plan::Join {
            left: Box::new(scan("d", "l", &["lk", "lv"], None)),
            right: Box::new(scan("d", "r", &["rk", "rv"], None)),
            kind: JoinKind::Left,
            on: vec![(0, 0)],
        };
        let out = execute(&src, &plan, &ExecOptions::default()).unwrap();
        assert_eq!(out.fields.len(), 4);
        let mut rows: Vec<(i64, i64, Value)> = (0..out.batches[0].rows())
            .map(|i| {
                let k = int_at(&out.batches[0], 0, i);
                let lv = int_at(&out.batches[0], 1, i);
                let rv = out.batches[0].column(3).get(i);
                (k, lv, rv)
            })
            .collect();
        rows.sort_by_key(|(k, ..)| *k);
        assert_eq!(rows, vec![(1, 10, Value::Int64(100)), (2, 20, Value::Null)]);
    }

    #[test]
    fn three_deep_plan_scan_filter_aggregate_sort_limit() {
        let f = field("a", DataType::Int64);
        let src = FakeSource::new().with_table(
            "d",
            "t",
            vec![f.clone()],
            vec![int_batch(&f, &[1, 1, 2, 2, 2, 3])],
        );
        let predicate = Expr::cmp(
            CmpOp::Ne,
            Expr::col(0),
            Expr::lit(Value::Int64(3), DataType::Int64),
        );
        let plan = Plan::Limit {
            input: Box::new(Plan::Sort {
                input: Box::new(Plan::Aggregate {
                    input: Box::new(Plan::Filter {
                        input: Box::new(scan("d", "t", &["a"], None)),
                        predicate,
                    }),
                    group_by: vec![0],
                    aggs: vec![count_star("n")],
                }),
                keys: vec![SortKey::desc(1)],
            }),
            limit: Some(1),
            offset: 0,
        };
        let out = execute(&src, &plan, &ExecOptions::default()).unwrap();
        assert_eq!(out.batches.iter().map(Batch::rows).sum::<usize>(), 1);
        assert_eq!(
            (int_at(&out.batches[0], 0, 0), int_at(&out.batches[0], 1, 0)),
            (2, 3)
        );
    }

    #[test]
    fn scan_predicate_is_always_filtered_even_if_the_source_ignores_it() {
        // Falsify: this fails if `execute` trusts the scan to filter.
        let f = field("a", DataType::Int64);
        let src = FakeSource::new().with_table(
            "d",
            "t",
            vec![f.clone()],
            vec![int_batch(&f, &[1, 2, 3])],
        );
        let predicate = Expr::cmp(
            CmpOp::Gt,
            Expr::col(0),
            Expr::lit(Value::Int64(1), DataType::Int64),
        );
        let plan = scan("d", "t", &["a"], Some(predicate));
        let out = execute(&src, &plan, &ExecOptions::default()).unwrap();
        assert_eq!(int_values(&out.batches, 0), vec![2, 3]);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // spawns threads
    fn threads_one_equals_threads_four_for_an_aggregate() {
        let f = field("a", DataType::Int64);
        let values: Vec<i64> = (0..100).map(|i| i % 5).collect();
        let batches: Vec<Batch> = values.chunks(4).map(|c| int_batch(&f, c)).collect();
        let src = FakeSource::new().with_table("d", "t", vec![f.clone()], batches);
        let plan = Plan::Aggregate {
            input: Box::new(scan("d", "t", &["a"], None)),
            group_by: vec![0],
            aggs: vec![count_star("n")],
        };
        let mut results = Vec::new();
        for threads in [1, 4] {
            let opts = ExecOptions {
                threads,
                ..ExecOptions::default()
            };
            let out = execute(&src, &plan, &opts).unwrap();
            let mut rows: Vec<(i64, i64)> = (0..out.batches[0].rows())
                .map(|i| (int_at(&out.batches[0], 0, i), int_at(&out.batches[0], 1, i)))
                .collect();
            rows.sort();
            results.push(rows);
        }
        assert_eq!(results[0], results[1]);
    }

    #[test]
    fn memory_limit_below_a_sort_gives_budget_exceeded() {
        let f = field("a", DataType::Int64);
        let src = FakeSource::new();
        let input = Plan::Values {
            fields: vec![f.clone()],
            batches: vec![int_batch(&f, &[1, 2, 3])],
        };
        let plan = Plan::Sort {
            input: Box::new(input),
            keys: vec![SortKey::asc(0)],
        };
        let opts = ExecOptions {
            memory_limit: 1,
            ..ExecOptions::default()
        };
        let err = execute(&src, &plan, &opts).unwrap_err();
        assert!(matches!(err, ExecError::BudgetExceeded { .. }));
    }

    #[test]
    fn cancel_before_the_run_gives_cancelled() {
        let f = field("a", DataType::Int64);
        let src = FakeSource::new();
        let input = Plan::Values {
            fields: vec![f.clone()],
            batches: vec![int_batch(&f, &[1, 2, 3])],
        };
        let plan = Plan::Limit {
            input: Box::new(input),
            limit: None,
            offset: 0,
        };
        let cancel = CancelToken::new();
        cancel.cancel();
        let opts = ExecOptions {
            cancel,
            ..ExecOptions::default()
        };
        let err = execute(&src, &plan, &opts).unwrap_err();
        assert!(matches!(err, ExecError::Cancelled));
    }
}
