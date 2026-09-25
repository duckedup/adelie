//! The hash join probe side (SPEC §7): streams the probe (left) input against a shared
//! `JoinTable`, one `HashJoinProbe` instance per worker.

use std::sync::Arc;

use super::build::JoinTable;
use crate::exec::kernels::rowkey::NullKeys;
use crate::exec::kernels::{encode_row_key, take, take_opt};
use crate::exec::operator::Operator;
use crate::exec::{BATCH_ROWS, Batch, Column, ExecContext, ExecError, Field, JoinKind};

/// Streams probe (left) batches against a shared build table, emitting probe order, and
/// within one probe row, build order (SPEC §7). INNER drops unmatched/NULL/NaN-key rows;
/// LEFT keeps them with NULL build columns.
pub(crate) struct HashJoinProbe {
    table: Arc<JoinTable>,
    fields: Vec<Field>,
    probe_keys: Vec<usize>,
    kind: JoinKind,
}

impl HashJoinProbe {
    pub(crate) fn new(
        table: Arc<JoinTable>,
        probe_fields: &[Field],
        probe_keys: Vec<usize>,
        kind: JoinKind,
        _ctx: &ExecContext,
    ) -> Result<HashJoinProbe, ExecError> {
        if probe_keys.len() != table.keys.len() {
            return Err(ExecError::Plan(format!(
                "join key count mismatch: probe has {}, build has {}",
                probe_keys.len(),
                table.keys.len()
            )));
        }
        for (&pk, &bk) in probe_keys.iter().zip(&table.keys) {
            let probe_ty = &probe_fields[pk].ty;
            let build_ty = &table.batch.fields()[bk].ty;
            if probe_ty != build_ty {
                return Err(ExecError::Plan(format!(
                    "join key type mismatch: probe key is {probe_ty}, build key is {build_ty}"
                )));
            }
        }

        let mut fields = probe_fields.to_vec();
        fields.extend(table.batch.fields().iter().cloned());
        for i in 0..fields.len() {
            if fields[..i].iter().any(|f| f.name == fields[i].name) {
                return Err(ExecError::Plan(format!(
                    "duplicate join output column: {}",
                    fields[i].name
                )));
            }
        }

        Ok(HashJoinProbe {
            table,
            fields,
            probe_keys,
            kind,
        })
    }

    /// Probe fields, then build fields (`Plan::Join`'s output order: left ++ right).
    pub(crate) fn fields(&self) -> &[Field] {
        &self.fields
    }

    fn emit(
        &self,
        probe_batch: &Batch,
        probe_idx: &[u32],
        build_idx: &[Option<u32>],
        out: &mut Vec<Batch>,
    ) {
        let mut columns = Vec::with_capacity(self.fields.len());
        for col in probe_batch.columns() {
            columns.push(take(col, probe_idx));
        }
        for col in self.table.batch.columns() {
            columns.push(take_opt(col, build_idx));
        }
        let batch = Batch::new(self.fields.clone(), columns)
            .expect("probe ++ build columns share `fields`'s types and one row count");
        out.push(batch);
    }
}

impl Operator for HashJoinProbe {
    fn push(
        &mut self,
        ctx: &ExecContext,
        batch: Batch,
        out: &mut Vec<Batch>,
    ) -> Result<(), ExecError> {
        ctx.check()?;
        let key_cols: Vec<&Column> = self.probe_keys.iter().map(|&i| batch.column(i)).collect();
        let mut probe_idx: Vec<u32> = Vec::new();
        let mut build_idx: Vec<Option<u32>> = Vec::new();
        let mut buf = Vec::new();
        for row in 0..batch.rows() {
            buf.clear();
            let hit = encode_row_key(&key_cols, row, NullKeys::Skip, &mut buf)
                .then(|| self.table.map.get(&buf))
                .flatten();
            match hit {
                Some(row_ids) => {
                    for &rid in row_ids {
                        probe_idx.push(row as u32);
                        build_idx.push(Some(rid));
                    }
                }
                None if self.kind == JoinKind::Left => {
                    probe_idx.push(row as u32);
                    build_idx.push(None);
                }
                None => {}
            }
        }

        let mut start = 0;
        while start < probe_idx.len() {
            let end = (start + BATCH_ROWS).min(probe_idx.len());
            self.emit(&batch, &probe_idx[start..end], &build_idx[start..end], out);
            start = end;
            if start < probe_idx.len() {
                ctx.check()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::join::JoinBuildSink;
    use crate::exec::operator::Sink;
    use crate::types::{DataType, Value};

    fn field(name: &str, ty: DataType) -> Field {
        Field {
            name: name.to_string(),
            ty,
        }
    }

    fn batch(fields: Vec<Field>, cols: Vec<Vec<Value>>) -> Batch {
        let columns = fields
            .iter()
            .zip(cols)
            .map(|(f, vs)| Column::from_values(&f.ty, &vs).unwrap())
            .collect();
        Batch::new(fields, columns).unwrap()
    }

    /// Build-side names get an `r_` prefix, so a test may reuse the probe's fields for the build
    /// side without tripping `new`'s duplicate-output-name check.
    fn table(
        fields: Vec<Field>,
        keys: Vec<usize>,
        batches: Vec<Batch>,
        ctx: &ExecContext,
    ) -> Arc<JoinTable> {
        let renamed: Vec<Field> = fields
            .iter()
            .map(|f| field(&format!("r_{}", f.name), f.ty.clone()))
            .collect();
        let mut sink = JoinBuildSink::new(renamed.clone(), keys);
        for b in batches {
            let b = Batch::new(renamed.clone(), b.columns().to_vec()).unwrap();
            sink.push(ctx, b).unwrap();
        }
        Arc::new(sink.into_table(ctx).unwrap())
    }

    fn probe_all(probe: &mut HashJoinProbe, ctx: &ExecContext, batches: Vec<Batch>) -> Vec<Batch> {
        let mut out = Vec::new();
        for b in batches {
            probe.push(ctx, b, &mut out).unwrap();
        }
        out
    }

    /// Every row of every output batch, as a `Vec<String>` (one per output column), sorted:
    /// a multiset comparison that ignores which physical batch a row landed in.
    fn rows_sorted(batches: &[Batch]) -> Vec<Vec<String>> {
        let mut rows = Vec::new();
        for b in batches {
            for r in 0..b.rows() {
                rows.push(
                    (0..b.fields().len())
                        .map(|c| format!("{:?}", b.column(c).get(r)))
                        .collect(),
                );
            }
        }
        rows.sort();
        rows
    }

    fn row(values: &[Value]) -> Vec<String> {
        values.iter().map(|v| format!("{v:?}")).collect()
    }

    // customers (probe/left): id, name. orders (build/right): order_id, cust_id, amt.
    // Mirrors tests/slt/join.slt.
    fn customers() -> (Vec<Field>, Batch) {
        let fields = vec![
            field("id", DataType::Int64),
            field("name", DataType::String),
        ];
        let b = batch(
            fields.clone(),
            vec![
                vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)],
                vec![
                    Value::String("Alice".into()),
                    Value::String("Bob".into()),
                    Value::String("Carol".into()),
                ],
            ],
        );
        (fields, b)
    }

    fn orders() -> (Vec<Field>, Batch) {
        let fields = vec![
            field("order_id", DataType::Int64),
            field("cust_id", DataType::Int64),
            field("amt", DataType::Int64),
        ];
        let b = batch(
            fields.clone(),
            vec![
                vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)],
                vec![Value::Int64(1), Value::Int64(1), Value::Int64(2)],
                vec![Value::Int64(100), Value::Int64(50), Value::Int64(75)],
            ],
        );
        (fields, b)
    }

    #[test]
    fn inner_join_matches_slt_fixture() {
        let ctx = ExecContext::unlimited();
        let (cust_fields, cust_batch) = customers();
        let (order_fields, order_batch) = orders();
        let t = table(order_fields, vec![1], vec![order_batch], &ctx);
        let mut probe =
            HashJoinProbe::new(t, &cust_fields, vec![0], JoinKind::Inner, &ctx).unwrap();
        let out = probe_all(&mut probe, &ctx, vec![cust_batch]);

        // columns: id, name, order_id, cust_id, amt -- check (name, amt) pairs.
        let mut got: Vec<(String, String)> = out
            .iter()
            .flat_map(|b| {
                (0..b.rows()).map(move |r| {
                    (
                        format!("{:?}", b.column(1).get(r)),
                        format!("{:?}", b.column(4).get(r)),
                    )
                })
            })
            .collect();
        got.sort();
        let mut want = vec![
            (
                format!("{:?}", Value::String("Alice".into())),
                format!("{:?}", Value::Int64(100)),
            ),
            (
                format!("{:?}", Value::String("Alice".into())),
                format!("{:?}", Value::Int64(50)),
            ),
            (
                format!("{:?}", Value::String("Bob".into())),
                format!("{:?}", Value::Int64(75)),
            ),
        ];
        want.sort();
        assert_eq!(got, want);
    }

    /// Falsify: fails if LEFT behaves as INNER (Carol would be missing instead of NULL).
    #[test]
    fn left_join_keeps_unmatched_rows_with_null_build_columns() {
        let ctx = ExecContext::unlimited();
        let (cust_fields, cust_batch) = customers();
        let (order_fields, order_batch) = orders();
        let t = table(order_fields, vec![1], vec![order_batch], &ctx);
        let mut probe = HashJoinProbe::new(t, &cust_fields, vec![0], JoinKind::Left, &ctx).unwrap();
        let out = probe_all(&mut probe, &ctx, vec![cust_batch]);
        assert_eq!(out.iter().map(Batch::rows).sum::<usize>(), 4);

        let mut found_carol_null = false;
        for b in &out {
            for r in 0..b.rows() {
                if b.column(1).get(r) == Value::String("Carol".into()) {
                    assert!(b.column(4).is_null(r), "Carol's amt column must be NULL");
                    found_carol_null = true;
                }
            }
        }
        assert!(
            found_carol_null,
            "Carol must appear once with NULL build columns"
        );
    }

    /// Falsify: fails if a NULL key uses `NullKeys::Group` (NULLs would then match each other).
    #[test]
    fn null_probe_key_never_matches_left_keeps_it_inner_drops_it() {
        let ctx = ExecContext::unlimited();
        let fields = vec![field("k", DataType::Int64), field("tag", DataType::String)];
        let probe_batch = batch(
            fields.clone(),
            vec![vec![Value::Null], vec![Value::String("p".into())]],
        );
        let build_fields = vec![field("k", DataType::Int64), field("tag", DataType::String)];
        let build_batch = batch(
            build_fields.clone(),
            vec![
                vec![Value::Null, Value::Int64(1)],
                vec![Value::String("b1".into()), Value::String("b2".into())],
            ],
        );
        let t = table(build_fields, vec![0], vec![build_batch], &ctx);

        let mut inner =
            HashJoinProbe::new(Arc::clone(&t), &fields, vec![0], JoinKind::Inner, &ctx).unwrap();
        let inner_out = probe_all(&mut inner, &ctx, vec![probe_batch.clone()]);
        assert_eq!(inner_out.iter().map(Batch::rows).sum::<usize>(), 0);

        let mut left = HashJoinProbe::new(t, &fields, vec![0], JoinKind::Left, &ctx).unwrap();
        let left_out = probe_all(&mut left, &ctx, vec![probe_batch]);
        assert_eq!(left_out.iter().map(Batch::rows).sum::<usize>(), 1);
        assert!(left_out[0].column(2).is_null(0)); // build's "k" column
    }

    /// NaN never matches, even itself; −0.0 matches 0.0 (`encode_row_key` normalises it).
    #[test]
    fn nan_never_matches_negative_zero_matches_zero() {
        let ctx = ExecContext::unlimited();
        let fields = vec![field("k", DataType::Float64)];
        let build_batch = batch(fields.clone(), vec![vec![Value::Float64(0.0)]]);
        let t = table(fields.clone(), vec![0], vec![build_batch], &ctx);
        let probe_batch = batch(
            fields.clone(),
            vec![vec![Value::Float64(-0.0), Value::Float64(f64::NAN)]],
        );
        let mut probe = HashJoinProbe::new(t, &fields, vec![0], JoinKind::Inner, &ctx).unwrap();
        let out = probe_all(&mut probe, &ctx, vec![probe_batch]);
        assert_eq!(out.iter().map(Batch::rows).sum::<usize>(), 1);
    }

    /// 2 left rows x 3 right rows sharing one key give 6 output rows (the cross product).
    #[test]
    fn duplicate_keys_give_the_cross_product() {
        let ctx = ExecContext::unlimited();
        let fields = vec![field("k", DataType::Int64)];
        let probe_batch = batch(fields.clone(), vec![vec![Value::Int64(1), Value::Int64(1)]]);
        let build_batch = batch(
            fields.clone(),
            vec![vec![Value::Int64(1), Value::Int64(1), Value::Int64(1)]],
        );
        let t = table(fields.clone(), vec![0], vec![build_batch], &ctx);
        let mut probe = HashJoinProbe::new(t, &fields, vec![0], JoinKind::Inner, &ctx).unwrap();
        let out = probe_all(&mut probe, &ctx, vec![probe_batch]);
        assert_eq!(out.iter().map(Batch::rows).sum::<usize>(), 6);
    }

    #[test]
    fn multi_column_keys_match_only_when_every_column_matches() {
        let ctx = ExecContext::unlimited();
        let fields = vec![field("a", DataType::Int64), field("b", DataType::Int64)];
        let build_batch = batch(
            fields.clone(),
            vec![
                vec![Value::Int64(1), Value::Int64(1)],
                vec![Value::Int64(1), Value::Int64(2)],
            ],
        );
        let t = table(fields.clone(), vec![0, 1], vec![build_batch], &ctx);
        let probe_batch = batch(
            fields.clone(),
            vec![
                vec![Value::Int64(1), Value::Int64(2)],
                vec![Value::Int64(1), Value::Int64(1)],
            ],
        );
        let mut probe = HashJoinProbe::new(t, &fields, vec![0, 1], JoinKind::Inner, &ctx).unwrap();
        let out = probe_all(&mut probe, &ctx, vec![probe_batch]);
        assert_eq!(out.iter().map(Batch::rows).sum::<usize>(), 1);
        assert_eq!(
            rows_sorted(&out),
            vec![row(&[
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(1)
            ])]
        );
    }

    #[test]
    fn key_type_mismatch_is_plan() {
        let ctx = ExecContext::unlimited();
        let probe_fields = vec![field("k", DataType::Int64)];
        let build_fields = vec![field("k", DataType::UInt64)];
        let build_batch = batch(build_fields.clone(), vec![vec![Value::UInt64(1)]]);
        let t = table(build_fields, vec![0], vec![build_batch], &ctx);
        let err = HashJoinProbe::new(t, &probe_fields, vec![0], JoinKind::Inner, &ctx)
            .err()
            .unwrap();
        assert!(matches!(err, ExecError::Plan(_)));
    }

    #[test]
    fn duplicate_output_name_is_plan() {
        let ctx = ExecContext::unlimited();
        let probe_fields = vec![field("k", DataType::Int64)];
        let build_fields = vec![field("k", DataType::Int64)];
        let build_batch = batch(build_fields.clone(), vec![vec![Value::Int64(1)]]);
        let mut sink = JoinBuildSink::new(build_fields, vec![0]);
        sink.push(&ctx, build_batch).unwrap();
        let t = Arc::new(sink.into_table(&ctx).unwrap());
        let err = HashJoinProbe::new(t, &probe_fields, vec![0], JoinKind::Inner, &ctx)
            .err()
            .unwrap();
        assert!(matches!(err, ExecError::Plan(_)));
    }

    /// 1 probe row x 5 000 matching build rows: every emitted batch is <= BATCH_ROWS, and the
    /// total is 5 000.
    #[test]
    #[cfg_attr(miri, ignore)] // slow under Miri
    fn chunking_caps_every_batch_at_batch_rows() {
        let ctx = ExecContext::unlimited();
        let fields = vec![field("k", DataType::Int64)];
        let build_values: Vec<Value> = (0..5000).map(|_| Value::Int64(1)).collect();
        let build_batch = batch(fields.clone(), vec![build_values]);
        let t = table(fields.clone(), vec![0], vec![build_batch], &ctx);
        let probe_batch = batch(fields.clone(), vec![vec![Value::Int64(1)]]);
        let mut probe = HashJoinProbe::new(t, &fields, vec![0], JoinKind::Inner, &ctx).unwrap();
        let out = probe_all(&mut probe, &ctx, vec![probe_batch]);
        assert!(
            out.len() > 1,
            "5000 rows over BATCH_ROWS must split into more than one batch"
        );
        for b in &out {
            assert!(b.rows() <= BATCH_ROWS);
        }
        assert_eq!(out.iter().map(Batch::rows).sum::<usize>(), 5000);
    }
}
