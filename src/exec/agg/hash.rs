//! Hash-aggregate `Sink` (SPEC §7): drives one `GroupsAccumulator` per `AggCall`, keyed by
//! `kernels::rowkey`. Filled by U9 (adelie-1st).

use std::collections::HashMap;
use std::mem::size_of;

use crate::exec::kernels::rowkey::NullKeys;
use crate::exec::kernels::{self, encode_row_key};
use crate::exec::operator::Sink;
use crate::exec::plan::AggCall;
use crate::exec::{Batch, Column, ExecContext, ExecError, Field, Reservation};
use crate::types::{DataType, Value};

use super::{GroupsAccumulator, accumulator};

/// Per-`HashMap`-entry overhead (bucket, `Vec<u8>` heap header) the budget estimate charges
/// once per distinct group, on top of the key's own bytes (mirrors `join::build`).
const MAP_ENTRY_OVERHEAD: usize = 48;

/// One worker's partial hash aggregate: one `GroupsAccumulator` per `AggCall`, keyed by the
/// GROUP BY columns' `encode_row_key` bytes. `merge` maps groups by that key, never by index,
/// so partials built over disjoint group subsets combine correctly.
pub(crate) struct HashAggregateSink {
    fields: Vec<Field>,
    group_by: Vec<usize>,
    aggs: Vec<AggCall>,
    accs: Vec<Box<dyn GroupsAccumulator>>,
    map: HashMap<Vec<u8>, usize>,
    /// One `Vec` per GROUP BY column; `keys[k][g]` is group `g`'s value for that column.
    keys: Vec<Vec<Value>>,
    groups: usize,
    res: Option<Reservation>,
}

impl HashAggregateSink {
    pub(crate) fn new(
        input: &[Field],
        group_by: Vec<usize>,
        aggs: Vec<AggCall>,
    ) -> Result<HashAggregateSink, ExecError> {
        for &i in &group_by {
            if i >= input.len() {
                return Err(ExecError::Plan(format!("GROUP BY column {i} out of range")));
            }
        }

        let mut accs: Vec<Box<dyn GroupsAccumulator>> = Vec::with_capacity(aggs.len());
        for call in &aggs {
            for &i in &call.args {
                if i >= input.len() {
                    return Err(ExecError::Plan(format!(
                        "aggregate argument column {i} out of range"
                    )));
                }
            }
            if let Some(f) = call.filter {
                if f >= input.len() {
                    return Err(ExecError::Plan(format!("FILTER column {f} out of range")));
                }
                if input[f].ty != DataType::Bool {
                    return Err(ExecError::Plan(format!(
                        "FILTER column {f} must be BOOL, got {}",
                        input[f].ty
                    )));
                }
            }
            let arg_types: Vec<DataType> = call.args.iter().map(|&i| input[i].ty.clone()).collect();
            accs.push(accumulator(&call.func, &arg_types)?);
        }

        let mut fields: Vec<Field> = group_by.iter().map(|&i| input[i].clone()).collect();
        for (call, acc) in aggs.iter().zip(&accs) {
            fields.push(Field {
                name: call.name.clone(),
                ty: acc.data_type(),
            });
        }
        for (i, f) in fields.iter().enumerate() {
            if fields[..i].iter().any(|g| g.name == f.name) {
                return Err(ExecError::Plan(format!(
                    "duplicate aggregate output column {:?}",
                    f.name
                )));
            }
        }

        // With no GROUP BY there is exactly one group, live even over an empty input: an
        // empty-keyed row pre-mapped to group 0, so `count(*)` and friends still finish.
        let mut map = HashMap::new();
        let mut groups = 0;
        if group_by.is_empty() {
            map.insert(Vec::new(), 0);
            groups = 1;
        }

        Ok(HashAggregateSink {
            keys: vec![Vec::new(); group_by.len()],
            fields,
            group_by,
            aggs,
            accs,
            map,
            groups,
            res: None,
        })
    }

    pub(crate) fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// Assigns row `row`'s group, inserting a new group (and its key values) on a miss.
    /// `cols[k]` is the GROUP BY column at `self.group_by[k]`, in that same order.
    fn group_of(&mut self, cols: &[&Column], row: usize, key_buf: &mut Vec<u8>) -> usize {
        key_buf.clear();
        encode_row_key(cols, row, NullKeys::Group, key_buf);
        if let Some(&g) = self.map.get(key_buf) {
            return g;
        }
        let g = self.groups;
        for (builder, col) in self.keys.iter_mut().zip(cols) {
            builder.push(col.get(row));
        }
        self.map.insert(key_buf.clone(), g);
        self.groups += 1;
        g
    }

    /// Map keys, accumulators' `byte_size`, and the key `Vec`s: everything held past one push.
    fn byte_size_estimate(&self) -> usize {
        let map_bytes: usize = self
            .map
            .keys()
            .map(|k| k.capacity() + MAP_ENTRY_OVERHEAD)
            .sum();
        let keys_bytes: usize = self
            .keys
            .iter()
            .map(|v| v.capacity() * size_of::<Value>())
            .sum();
        let accs_bytes: usize = self.accs.iter().map(|a| a.byte_size()).sum();
        map_bytes + keys_bytes + accs_bytes
    }

    fn resize_reservation(&mut self, ctx: &ExecContext) -> Result<(), ExecError> {
        let bytes = self.byte_size_estimate();
        match &mut self.res {
            Some(r) => r.resize(bytes),
            None => {
                self.res = Some(ctx.reserve(bytes)?);
                Ok(())
            }
        }
    }
}

impl Sink for HashAggregateSink {
    fn push(&mut self, ctx: &ExecContext, batch: Batch) -> Result<(), ExecError> {
        ctx.check()?;
        let rows = batch.rows();
        let group_cols: Vec<&Column> = self.group_by.iter().map(|&i| batch.column(i)).collect();
        let mut group_ids = Vec::with_capacity(rows);
        let mut key_buf = Vec::new();
        for r in 0..rows {
            group_ids.push(self.group_of(&group_cols, r, &mut key_buf));
        }

        for (call, acc) in self.aggs.iter().zip(self.accs.iter_mut()) {
            let arg_cols: Vec<&Column> = call.args.iter().map(|&i| batch.column(i)).collect();
            let filter_bitmap = call.filter.map(|i| kernels::truthy(batch.column(i)));
            acc.update(&group_ids, self.groups, &arg_cols, filter_bitmap.as_ref())?;
        }

        self.resize_reservation(ctx)
    }

    fn merge(&mut self, ctx: &ExecContext, mut other: HashAggregateSink) -> Result<(), ExecError> {
        ctx.check()?;
        // Release `other`'s bytes first: they are already counted against the shared budget, and
        // re-pushing them while its reservation lives would count them twice (JoinBuildSink too).
        drop(other.res.take());
        let other_groups = other.groups;

        // Rebuild `other`'s keys as columns so `encode_row_key` (FROZEN bytes) sees exactly
        // what `push` would have seen, rather than re-deriving the encoding by hand.
        let key_cols: Vec<Column> = (0..self.group_by.len())
            .map(|k| Column::from_values(&self.fields[k].ty, &other.keys[k]))
            .collect::<Result<_, _>>()?;
        let key_col_refs: Vec<&Column> = key_cols.iter().collect();

        let mut mapping = Vec::with_capacity(other_groups);
        let mut key_buf = Vec::new();
        for g in 0..other_groups {
            key_buf.clear();
            encode_row_key(&key_col_refs, g, NullKeys::Group, &mut key_buf);
            let target = if let Some(&t) = self.map.get(&key_buf) {
                t
            } else {
                let t = self.groups;
                for k in 0..self.group_by.len() {
                    self.keys[k].push(other.keys[k][g].clone());
                }
                self.map.insert(key_buf.clone(), t);
                self.groups += 1;
                t
            };
            mapping.push(target);
        }

        for (acc, other_acc) in self.accs.iter_mut().zip(other.accs) {
            acc.merge(&mapping, self.groups, other_acc)?;
        }

        self.resize_reservation(ctx)
    }

    fn finish(self, ctx: &ExecContext) -> Result<Vec<Batch>, ExecError> {
        ctx.check()?;
        let HashAggregateSink {
            fields,
            keys,
            accs,
            groups,
            ..
        } = self;
        let mut columns = Vec::with_capacity(fields.len());
        for (k, values) in keys.into_iter().enumerate() {
            columns.push(Column::from_values(&fields[k].ty, &values)?);
        }
        for acc in accs {
            columns.push(acc.finish(groups)?);
        }
        let batch = Batch::new(fields.clone(), columns)?;
        kernels::rechunk(&fields, vec![batch])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::ExecOptions;
    use crate::exec::plan::AggFunc;

    fn field(name: &str, ty: DataType) -> Field {
        Field {
            name: name.to_string(),
            ty,
        }
    }

    fn call(func: AggFunc, args: &[usize], filter: Option<usize>, name: &str) -> AggCall {
        AggCall {
            func,
            args: args.to_vec(),
            filter,
            name: name.to_string(),
        }
    }

    fn int_col(values: &[Option<i64>]) -> Column {
        let vs: Vec<Value> = values
            .iter()
            .map(|v| v.map_or(Value::Null, Value::Int64))
            .collect();
        Column::from_values(&DataType::Int64, &vs).unwrap()
    }

    fn batch(fields: Vec<Field>, cols: Vec<Column>) -> Batch {
        Batch::new(fields, cols).unwrap()
    }

    /// Row `i`'s value in `col`, as `i64` (panics on NULL or a non-INT64 column); used to read
    /// back a finished batch by row without caring which physical column type it is.
    fn i64_at(col: &Column, i: usize) -> Option<i64> {
        match col.get(i) {
            Value::Int64(v) => Some(v),
            Value::Null => None,
            other => panic!("expected Int64 or Null, got {other:?}"),
        }
    }

    /// Merging hands `other`'s bytes over rather than counting them twice. The budget is 1.25×
    /// what two partials with disjoint groups hold before the merge. Double counting needs about
    /// 1.5×, and a correct merge needs about 1×.
    #[test]
    fn merge_does_not_double_count_the_other_partials_reservation() {
        let fields = vec![field("k", DataType::Int64), field("v", DataType::Int64)];
        let aggs = vec![call(AggFunc::Sum, &[1], None, "s")];
        let part = |base: i64| {
            let keys: Vec<Option<i64>> = (0..200).map(|i| Some(base + i)).collect();
            batch(fields.clone(), vec![int_col(&keys), int_col(&keys)])
        };
        let run = |ctx: &ExecContext| {
            let mut a = HashAggregateSink::new(&fields, vec![0], aggs.clone()).unwrap();
            let mut c = HashAggregateSink::new(&fields, vec![0], aggs.clone()).unwrap();
            a.push(ctx, part(0)).unwrap();
            c.push(ctx, part(1_000)).unwrap();
            (a, c)
        };
        let probe = ExecContext::unlimited();
        let held = {
            let _parts = run(&probe);
            probe.memory_used()
        };
        let ctx = ExecContext::new(&ExecOptions {
            memory_limit: held + held / 4,
            ..ExecOptions::default()
        });
        let (mut a, c) = run(&ctx);
        a.merge(&ctx, c).unwrap();
        assert_eq!(a.finish(&ctx).unwrap().iter().map(Batch::rows).sum::<usize>(), 400);
    }

    /// Sorts `(group_key, count, sum)` rows so a merged and an unmerged run compare equal
    /// regardless of first-seen order.
    fn rows_sorted(
        out: &[Batch],
        key_col: usize,
        count_col: usize,
        sum_col: usize,
    ) -> Vec<(i64, i64, Option<i64>)> {
        let mut rows: Vec<(i64, i64, Option<i64>)> = out
            .iter()
            .flat_map(|b| {
                (0..b.rows()).map(move |i| {
                    (
                        i64_at(b.column(key_col), i).expect("group key is never null here"),
                        i64_at(b.column(count_col), i).expect("count is never null"),
                        i64_at(b.column(sum_col), i),
                    )
                })
            })
            .collect();
        rows.sort();
        rows
    }

    #[test]
    fn group_by_cat_count_sum_avg_min_max() {
        let ctx = ExecContext::unlimited();
        let input = vec![
            field("cat", DataType::Int64),
            field("sales", DataType::Int64),
        ];
        let aggs = vec![
            call(AggFunc::CountStar, &[], None, "n"),
            call(AggFunc::Sum, &[1], None, "sum_sales"),
            call(AggFunc::Avg, &[1], None, "avg_sales"),
            call(AggFunc::Min, &[1], None, "min_sales"),
            call(AggFunc::Max, &[1], None, "max_sales"),
        ];
        let mut sink = HashAggregateSink::new(&input, vec![0], aggs).unwrap();
        let cats = int_col(&[Some(1), Some(2), Some(1), Some(2), Some(1)]);
        let sales = int_col(&[Some(10), Some(20), Some(30), Some(40), Some(50)]);
        sink.push(&ctx, batch(input.clone(), vec![cats, sales]))
            .unwrap();
        let out = sink.finish(&ctx).unwrap();

        let mut rows: Vec<(i64, i64, i64, i64, i64, i64)> = out
            .iter()
            .flat_map(|b| {
                (0..b.rows()).map(move |i| {
                    (
                        i64_at(b.column(0), i).unwrap(),
                        i64_at(b.column(1), i).unwrap(),
                        i64_at(b.column(2), i).unwrap(),
                        match b.column(3).get(i) {
                            Value::Float64(f) => f as i64,
                            other => panic!("expected avg Float64, got {other:?}"),
                        },
                        i64_at(b.column(4), i).unwrap(),
                        i64_at(b.column(5), i).unwrap(),
                    )
                })
            })
            .collect();
        rows.sort();
        // cat 1: sales [10, 30, 50] -> n=3 sum=90 avg=30 min=10 max=50
        // cat 2: sales [20, 40]     -> n=2 sum=60 avg=30 min=20 max=40
        assert_eq!(rows, vec![(1, 3, 90, 30, 10, 50), (2, 2, 60, 30, 20, 40)]);
    }

    #[test]
    fn no_group_by_empty_input_gives_one_row_count_zero_sum_null() {
        let ctx = ExecContext::unlimited();
        let input = vec![field("x", DataType::Int64)];
        let aggs = vec![
            call(AggFunc::CountStar, &[], None, "n"),
            call(AggFunc::Sum, &[0], None, "s"),
        ];
        let sink = HashAggregateSink::new(&input, vec![], aggs).unwrap();
        let out = sink.finish(&ctx).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rows(), 1);
        assert_eq!(i64_at(out[0].column(0), 0), Some(0));
        assert_eq!(i64_at(out[0].column(1), 0), None);
    }

    #[test]
    fn no_group_by_one_row() {
        let ctx = ExecContext::unlimited();
        let input = vec![field("x", DataType::Int64)];
        let aggs = vec![call(AggFunc::CountStar, &[], None, "n")];
        let mut sink = HashAggregateSink::new(&input, vec![], aggs).unwrap();
        sink.push(&ctx, batch(input.clone(), vec![int_col(&[Some(1)])]))
            .unwrap();
        let out = sink.finish(&ctx).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rows(), 1);
        assert_eq!(i64_at(out[0].column(0), 0), Some(1));
    }

    #[test]
    fn group_by_over_empty_input_gives_zero_rows() {
        let ctx = ExecContext::unlimited();
        let input = vec![field("cat", DataType::Int64)];
        let aggs = vec![call(AggFunc::CountStar, &[], None, "n")];
        let sink = HashAggregateSink::new(&input, vec![0], aggs).unwrap();
        let out = sink.finish(&ctx).unwrap();
        let total: usize = out.iter().map(Batch::rows).sum();
        assert_eq!(total, 0);
    }

    #[test]
    fn null_group_key_forms_its_own_group_and_signed_zero_floats_merge() {
        let ctx = ExecContext::unlimited();
        let input = vec![field("k", DataType::Float64)];
        let aggs = vec![call(AggFunc::CountStar, &[], None, "n")];
        let mut sink = HashAggregateSink::new(&input, vec![0], aggs).unwrap();
        let vals: Vec<Value> = vec![
            Value::Null,
            Value::Float64(0.0),
            Value::Float64(-0.0),
            Value::Null,
        ];
        let col = Column::from_values(&DataType::Float64, &vals).unwrap();
        sink.push(&ctx, batch(input.clone(), vec![col])).unwrap();
        let out = sink.finish(&ctx).unwrap();
        // NULL is its own group (2 rows); 0.0 and -0.0 are one group (2 rows) -> 2 groups.
        let total: usize = out.iter().map(Batch::rows).sum();
        assert_eq!(total, 2);
        let mut counts: Vec<i64> = out
            .iter()
            .flat_map(|b| (0..b.rows()).map(|i| i64_at(b.column(1), i).unwrap()))
            .collect();
        counts.sort();
        assert_eq!(counts, vec![2, 2]);
    }

    #[test]
    fn count_star_filter_counts_only_flagged_rows_null_flag_not_counted() {
        let ctx = ExecContext::unlimited();
        let input = vec![field("x", DataType::Int64), field("flag", DataType::Bool)];
        let aggs = vec![call(AggFunc::CountStar, &[], Some(1), "n")];
        let mut sink = HashAggregateSink::new(&input, vec![], aggs).unwrap();
        let x = int_col(&[Some(1), Some(2), Some(3), Some(4)]);
        let flag_vals = vec![
            Value::Bool(true),
            Value::Bool(false),
            Value::Null,
            Value::Bool(true),
        ];
        let flag = Column::from_values(&DataType::Bool, &flag_vals).unwrap();
        sink.push(&ctx, batch(input.clone(), vec![x, flag]))
            .unwrap();
        let out = sink.finish(&ctx).unwrap();
        assert_eq!(i64_at(out[0].column(0), 0), Some(2));
    }

    #[test]
    fn partial_vs_single_merge_by_key_not_index() {
        let ctx = ExecContext::unlimited();
        let input = vec![
            field("cat", DataType::Int64),
            field("sales", DataType::Int64),
        ];
        let aggs_for = || {
            vec![
                call(AggFunc::CountStar, &[], None, "n"),
                call(AggFunc::Sum, &[1], None, "s"),
                call(AggFunc::Min, &[1], None, "mn"),
                call(AggFunc::Max, &[1], None, "mx"),
            ]
        };

        let cats = [3, 1, 2, 1, 3, 2, 1, 2, 3];
        let sales = [10, 20, 30, 40, 50, 60, 70, 80, 90];

        let mut whole = HashAggregateSink::new(&input, vec![0], aggs_for()).unwrap();
        whole
            .push(
                &ctx,
                batch(
                    input.clone(),
                    vec![
                        int_col(&cats.iter().map(|&v| Some(v)).collect::<Vec<_>>()),
                        int_col(&sales.iter().map(|&v| Some(v)).collect::<Vec<_>>()),
                    ],
                ),
            )
            .unwrap();
        let whole_out = whole.finish(&ctx).unwrap();

        // Three partitions by row range, deliberately holding different (overlapping) group
        // subsets, merged in order 0<-1<-2.
        let ranges = [(0..3), (3..6), (6..9)];
        let mut partials = ranges.into_iter().map(|r| {
            let mut s = HashAggregateSink::new(&input, vec![0], aggs_for()).unwrap();
            let c: Vec<Option<i64>> = cats[r.clone()].iter().map(|&v| Some(v)).collect();
            let v: Vec<Option<i64>> = sales[r].iter().map(|&v| Some(v)).collect();
            s.push(&ctx, batch(input.clone(), vec![int_col(&c), int_col(&v)]))
                .unwrap();
            s
        });
        let mut merged = partials.next().unwrap();
        for p in partials {
            merged.merge(&ctx, p).unwrap();
        }
        let merged_out = merged.finish(&ctx).unwrap();

        assert_eq!(
            rows_sorted(&whole_out, 0, 1, 2),
            rows_sorted(&merged_out, 0, 1, 2)
        );
        // min/max too, read via the same helper on columns 3/4 paired with count as a dummy
        // "sum" slot so the shared sort-and-compare helper can be reused for both pairs.
        assert_eq!(
            rows_sorted(&whole_out, 0, 1, 3),
            rows_sorted(&merged_out, 0, 1, 3)
        );
        assert_eq!(
            rows_sorted(&whole_out, 0, 1, 4),
            rows_sorted(&merged_out, 0, 1, 4)
        );
    }

    #[test]
    fn budget_exceeded_over_many_distinct_groups_with_a_tiny_limit() {
        let opts = ExecOptions {
            memory_limit: 1024,
            ..ExecOptions::default()
        };
        let ctx = ExecContext::new(&opts);
        let input = vec![field("k", DataType::Int64)];
        let aggs = vec![call(AggFunc::CountStar, &[], None, "n")];
        let mut sink = HashAggregateSink::new(&input, vec![0], aggs).unwrap();
        let values: Vec<Option<i64>> = (0..1000).map(Some).collect();
        let err = sink
            .push(&ctx, batch(input, vec![int_col(&values)]))
            .unwrap_err();
        assert!(matches!(err, ExecError::BudgetExceeded { .. }));
    }

    #[test]
    fn no_group_by_merge_maps_both_sides_group_zero_to_zero() {
        let ctx = ExecContext::unlimited();
        let input = vec![field("x", DataType::Int64)];
        let aggs = || vec![call(AggFunc::CountStar, &[], None, "n")];
        let mut a = HashAggregateSink::new(&input, vec![], aggs()).unwrap();
        a.push(
            &ctx,
            batch(input.clone(), vec![int_col(&[Some(1), Some(2)])]),
        )
        .unwrap();
        let mut b = HashAggregateSink::new(&input, vec![], aggs()).unwrap();
        b.push(&ctx, batch(input.clone(), vec![int_col(&[Some(3)])]))
            .unwrap();
        a.merge(&ctx, b).unwrap();
        let out = a.finish(&ctx).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rows(), 1);
        assert_eq!(i64_at(out[0].column(0), 0), Some(3));
    }

    #[test]
    fn duplicate_output_names_is_plan() {
        let input = vec![field("cat", DataType::Int64), field("x", DataType::Int64)];
        let aggs = vec![call(AggFunc::CountStar, &[], None, "cat")];
        assert!(matches!(
            HashAggregateSink::new(&input, vec![0], aggs),
            Err(ExecError::Plan(_))
        ));
    }

    #[test]
    fn filter_column_not_bool_is_plan() {
        let input = vec![field("x", DataType::Int64), field("flag", DataType::Int64)];
        let aggs = vec![call(AggFunc::CountStar, &[], Some(1), "n")];
        assert!(matches!(
            HashAggregateSink::new(&input, vec![], aggs),
            Err(ExecError::Plan(_))
        ));
    }

    #[test]
    fn out_of_range_group_by_index_is_plan() {
        let input = vec![field("x", DataType::Int64)];
        let aggs: Vec<AggCall> = vec![];
        assert!(matches!(
            HashAggregateSink::new(&input, vec![5], aggs),
            Err(ExecError::Plan(_))
        ));
    }
}
