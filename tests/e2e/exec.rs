//! adelie-1st (E5): public-API tests for query execution (SPEC §7) against a real store.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use adelie::exec::{
    AggCall, AggFunc, ArithOp, Batch, CancelToken, CmpOp, Column, ExecError, ExecOptions, Expr,
    Field, JoinKind, Plan, QueryResult, ScanSpec, SortKey,
};
use adelie::storage::manifest::{CmpOp as MCmpOp, Manifest, Predicate, TableName};
use adelie::storage::{Error, Store, StoreOptions, TableSpec};
use adelie::types::{DataType, Value, total_cmp};

// ── harness (mirrors tests/e2e/store.rs) ────────────────────────────────────

fn opts() -> StoreOptions {
    StoreOptions {
        flush_interval: Duration::from_millis(5),
        gc_grace: Duration::ZERO,
        ..StoreOptions::default()
    }
}

fn schema() -> Vec<Field> {
    vec![
        Field {
            name: "ts".to_string(),
            ty: DataType::Timestamp,
        },
        Field {
            name: "svc".to_string(),
            ty: DataType::String,
        },
        Field {
            name: "status".to_string(),
            ty: DataType::Int64,
        },
        Field {
            name: "dur".to_string(),
            ty: DataType::Float64,
        },
        Field {
            name: "id".to_string(),
            ty: DataType::UInt64,
        },
    ]
}

const COLS: [&str; 5] = ["ts", "svc", "status", "dur", "id"];
const TS_IDX: usize = 0;
const SVC_IDX: usize = 1;
const STATUS_IDX: usize = 2;
const DUR_IDX: usize = 3;
const ID_IDX: usize = 4;

fn table() -> TableName {
    TableName::new("main", "events")
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "adelie-e2e-exec-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

fn open_events(dir: &Path, o: StoreOptions) -> Store {
    let store = Store::open(dir, o).unwrap();
    store
        .create_table(TableSpec::new(table(), schema()))
        .unwrap();
    store
}

// ── deterministic event generator ───────────────────────────────────────────

const SVC_CYCLE: [&str; 3] = ["api", "web", "db"];
/// A 3x3 Latin square over `SVC_CYCLE`'s period: every svc sees every status once per 9 rows.
const STATUS_CYCLE: [i64; 9] = [200, 404, 500, 404, 500, 200, 500, 200, 404];
const TS_BASE: i64 = 1_700_000_000_000_000_000;

fn svc_of(i: u64) -> &'static str {
    SVC_CYCLE[(i % 3) as usize]
}

fn status_of(i: u64) -> i64 {
    STATUS_CYCLE[(i % 9) as usize]
}

/// A distinct small integer per row (not per group): `sum`/`avg` stay exact regardless of how
/// morsels are partitioned across threads, and `arg_max` never hits a tie.
fn dur_of(i: u64) -> f64 {
    i as f64
}

fn ts_of(i: u64) -> i64 {
    TS_BASE + i as i64 * 1_000_000
}

/// One `main.events` batch for `ids`: every column a pure function of the row's `id`.
fn gen_events(ids: Range<u64>) -> Batch {
    let ts: Vec<Value> = ids.clone().map(|i| Value::Timestamp(ts_of(i))).collect();
    let svc: Vec<Value> = ids
        .clone()
        .map(|i| Value::String(svc_of(i).to_string()))
        .collect();
    let status: Vec<Value> = ids.clone().map(|i| Value::Int64(status_of(i))).collect();
    let dur: Vec<Value> = ids.clone().map(|i| Value::Float64(dur_of(i))).collect();
    let id: Vec<Value> = ids.map(Value::UInt64).collect();
    events_columns(ts, svc, status, dur, id)
}

/// Explicit `(id, svc, status, dur)` rows for tests that need specific values; `ts` still
/// derives from `id`, like `gen_events`.
fn events_batch(rows: &[(u64, &str, i64, f64)]) -> Batch {
    let ts: Vec<Value> = rows
        .iter()
        .map(|&(id, ..)| Value::Timestamp(ts_of(id)))
        .collect();
    let svc: Vec<Value> = rows
        .iter()
        .map(|&(_, s, ..)| Value::String(s.to_string()))
        .collect();
    let status: Vec<Value> = rows.iter().map(|&(_, _, st, _)| Value::Int64(st)).collect();
    let dur: Vec<Value> = rows.iter().map(|&(_, _, _, d)| Value::Float64(d)).collect();
    let id: Vec<Value> = rows.iter().map(|&(id, ..)| Value::UInt64(id)).collect();
    events_columns(ts, svc, status, dur, id)
}

fn events_columns(
    ts: Vec<Value>,
    svc: Vec<Value>,
    status: Vec<Value>,
    dur: Vec<Value>,
    id: Vec<Value>,
) -> Batch {
    Batch::new(
        schema(),
        vec![
            Column::from_values(&DataType::Timestamp, &ts).unwrap(),
            Column::from_values(&DataType::String, &svc).unwrap(),
            Column::from_values(&DataType::Int64, &status).unwrap(),
            Column::from_values(&DataType::Float64, &dur).unwrap(),
            Column::from_values(&DataType::UInt64, &id).unwrap(),
        ],
    )
    .unwrap()
}

fn svcs_table() -> TableName {
    TableName::new("main", "svcs")
}

fn svcs_schema() -> Vec<Field> {
    vec![
        Field {
            name: "svc".to_string(),
            ty: DataType::String,
        },
        Field {
            name: "team".to_string(),
            ty: DataType::String,
        },
    ]
}

fn svcs_batch(rows: &[(&str, &str)]) -> Batch {
    let svc: Vec<Value> = rows
        .iter()
        .map(|&(s, _)| Value::String(s.to_string()))
        .collect();
    let team: Vec<Value> = rows
        .iter()
        .map(|&(_, t)| Value::String(t.to_string()))
        .collect();
    Batch::new(
        svcs_schema(),
        vec![
            Column::from_values(&DataType::String, &svc).unwrap(),
            Column::from_values(&DataType::String, &team).unwrap(),
        ],
    )
    .unwrap()
}

// ── local helpers (blueprint's `rows`, `sorted`, `plan_scan`) ───────────────

fn rows(result: &QueryResult) -> Vec<Vec<Value>> {
    batch_rows(&result.batches)
}

/// Flattens a scan's or query's batches row-major, in batch order.
fn batch_rows(batches: &[Batch]) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for b in batches {
        for r in 0..b.rows() {
            out.push((0..b.columns().len()).map(|c| b.column(c).get(r)).collect());
        }
    }
    out
}

/// Orders by each row's `Debug` text, for comparisons that don't care about row order.
fn sorted(mut v: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    v.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    v
}

fn plan_scan(cols: &[&str], pred: Option<Expr>) -> Plan {
    Plan::Scan(ScanSpec {
        db: "main".to_string(),
        table: "events".to_string(),
        columns: cols.iter().map(|s| s.to_string()).collect(),
        predicate: pred,
    })
}

fn all_cols_scan(pred: Option<Expr>) -> Plan {
    plan_scan(&COLS, pred)
}

fn count_star_plan(input: Plan) -> Plan {
    Plan::Aggregate {
        input: Box::new(input),
        group_by: vec![],
        aggs: vec![AggCall {
            func: AggFunc::CountStar,
            args: vec![],
            filter: None,
            name: "cnt".to_string(),
        }],
    }
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int64(n) => *n,
        other => panic!("expected Int64, got {other:?}"),
    }
}

fn as_u64(v: &Value) -> u64 {
    match v {
        Value::UInt64(n) => *n,
        other => panic!("expected UInt64, got {other:?}"),
    }
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Float64(n) => *n,
        other => panic!("expected Float64, got {other:?}"),
    }
}

fn as_str(v: &Value) -> &str {
    match v {
        Value::String(s) => s,
        other => panic!("expected String, got {other:?}"),
    }
}

fn status_of_row(r: &[Value]) -> i64 {
    as_i64(&r[STATUS_IDX])
}

fn id_of_row(r: &[Value]) -> u64 {
    as_u64(&r[ID_IDX])
}

fn dur_of_row(r: &[Value]) -> f64 {
    as_f64(&r[DUR_IDX])
}

fn svc_of_row(r: &[Value]) -> &str {
    as_str(&r[SVC_IDX])
}

// ── tests ────────────────────────────────────────────────────────────────

/// *Catches:* tombstones ignored, and `seq` ignored (a later write matching a spent
/// predicate must survive).
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn deleted_rows_vanish_and_later_writes_survive() {
    let dir = temp_dir("delete");
    let store = open_events(&dir, opts());
    store
        .write(
            &table(),
            events_batch(&[(0, "api", 500, 1.0), (1, "api", 200, 2.0)]),
        )
        .unwrap();
    store.flush().unwrap();
    store
        .delete(
            &table(),
            vec![Predicate {
                column: "status".to_string(),
                op: MCmpOp::Eq,
                value: Value::Int64(500),
            }],
        )
        .unwrap();
    store
        .write(&table(), events_batch(&[(2, "api", 500, 3.0)]))
        .unwrap();
    store.flush().unwrap();

    let view = store.snapshot();
    let scanned = batch_rows(&view.scan(&table()).unwrap());
    assert_eq!(scanned.len(), 2);
    assert_eq!(
        scanned.iter().filter(|r| status_of_row(r) == 500).count(),
        1
    );
    assert_eq!(
        scanned.iter().filter(|r| status_of_row(r) == 200).count(),
        1
    );

    let queried = rows(
        &view
            .query(&all_cols_scan(None), &ExecOptions::default())
            .unwrap(),
    );
    assert_eq!(queried.len(), 2);
    assert_eq!(
        queried.iter().filter(|r| status_of_row(r) == 500).count(),
        1
    );
    assert_eq!(
        queried.iter().filter(|r| status_of_row(r) == 200).count(),
        1
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

/// *Catches:* no pruning (`segments_pruned == 0`) and over-pruning (rows differ from an
/// unpruned, explicitly filtered scan).
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn pruning_skips_segments_without_changing_results() {
    let dir = temp_dir("prune");
    let store = open_events(&dir, opts());
    for seg in 0..4u64 {
        let start = seg * 10;
        store
            .write(&table(), gen_events(start..start + 10))
            .unwrap();
        store.flush().unwrap();
    }
    let view = store.snapshot();

    let pred = Expr::cmp(
        CmpOp::Ge,
        Expr::col(ID_IDX),
        Expr::lit(Value::UInt64(30), DataType::UInt64),
    );
    let pruned = view
        .query(&all_cols_scan(Some(pred.clone())), &ExecOptions::default())
        .unwrap();
    assert_eq!(pruned.stats.scan.segments_pruned, 3);

    let filtered_plan = Plan::Filter {
        input: Box::new(all_cols_scan(None)),
        predicate: pred,
    };
    let filtered = view.query(&filtered_plan, &ExecOptions::default()).unwrap();
    assert_eq!(sorted(rows(&pruned)), sorted(rows(&filtered)));

    std::fs::remove_dir_all(&dir).unwrap();
}

/// *Catches:* trusting the scan to filter (a predicate that prunes nothing must still leave
/// only matching rows).
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn scan_predicate_is_always_applied() {
    let dir = temp_dir("scan-filter");
    let store = open_events(&dir, opts());
    for seg in 0..4u64 {
        let start = seg * 10;
        store
            .write(&table(), gen_events(start..start + 10))
            .unwrap();
        store.flush().unwrap();
    }
    let view = store.snapshot();

    let pred = Expr::cmp(
        CmpOp::Eq,
        Expr::col(STATUS_IDX),
        Expr::lit(Value::Int64(404), DataType::Int64),
    );
    let result = view
        .query(&all_cols_scan(Some(pred)), &ExecOptions::default())
        .unwrap();
    // 404 sits inside every segment's [200, 500] status range: a correct scan prunes nothing.
    assert_eq!(result.stats.scan.segments_pruned, 0);

    let got = rows(&result);
    let want = (0..40u64).filter(|&i| status_of(i) == 404).count();
    assert_eq!(got.len(), want);
    assert!(got.iter().all(|r| status_of_row(r) == 404));

    std::fs::remove_dir_all(&dir).unwrap();
}

/// *Catches:* a merge that depends on partition order, in any aggregate — exact for
/// `approx_*` too, since the same inputs merged under a different partitioning must sketch
/// identically.
#[test]
#[cfg_attr(miri, ignore)] // spawns threads, touches the real filesystem
fn aggregate_matches_across_thread_counts() {
    let dir = temp_dir("agg-threads");
    let store = open_events(&dir, opts());
    for seg in 0..4u64 {
        let start = seg * 25;
        store
            .write(&table(), gen_events(start..start + 25))
            .unwrap();
        store.flush().unwrap();
    }
    let view = store.snapshot();

    let aggs = vec![
        AggCall {
            func: AggFunc::CountStar,
            args: vec![],
            filter: None,
            name: "cnt".to_string(),
        },
        AggCall {
            func: AggFunc::Sum,
            args: vec![STATUS_IDX],
            filter: None,
            name: "sum_status".to_string(),
        },
        AggCall {
            func: AggFunc::Avg,
            args: vec![DUR_IDX],
            filter: None,
            name: "avg_dur".to_string(),
        },
        AggCall {
            func: AggFunc::Min,
            args: vec![TS_IDX],
            filter: None,
            name: "min_ts".to_string(),
        },
        AggCall {
            func: AggFunc::Max,
            args: vec![TS_IDX],
            filter: None,
            name: "max_ts".to_string(),
        },
        AggCall {
            func: AggFunc::ApproxCountDistinct,
            args: vec![ID_IDX],
            filter: None,
            name: "acd_id".to_string(),
        },
        AggCall {
            func: AggFunc::Quantile(0.5),
            args: vec![DUR_IDX],
            filter: None,
            name: "q50_dur".to_string(),
        },
        AggCall {
            func: AggFunc::ApproxQuantile(0.99),
            args: vec![DUR_IDX],
            filter: None,
            name: "aq99_dur".to_string(),
        },
        AggCall {
            func: AggFunc::TopK(3),
            args: vec![STATUS_IDX],
            filter: None,
            name: "topk_status".to_string(),
        },
        AggCall {
            func: AggFunc::ArgMax,
            args: vec![ID_IDX, DUR_IDX],
            filter: None,
            name: "argmax_id".to_string(),
        },
        AggCall {
            func: AggFunc::ListAgg,
            args: vec![STATUS_IDX],
            filter: None,
            name: "list_status".to_string(),
        },
        AggCall {
            func: AggFunc::Histogram,
            args: vec![STATUS_IDX],
            filter: None,
            name: "hist_status".to_string(),
        },
    ];
    let list_col = 1 + aggs.iter().position(|c| c.name == "list_status").unwrap();

    let plan = Plan::Sort {
        input: Box::new(Plan::Aggregate {
            input: Box::new(all_cols_scan(None)),
            group_by: vec![SVC_IDX],
            aggs,
        }),
        keys: vec![SortKey::asc(0)],
    };

    let one = view
        .query(
            &plan,
            &ExecOptions {
                threads: 1,
                ..ExecOptions::default()
            },
        )
        .unwrap();
    let four = view
        .query(
            &plan,
            &ExecOptions {
                threads: 4,
                ..ExecOptions::default()
            },
        )
        .unwrap();

    // `list_agg`'s arrival order depends on the morsel scheduler's racy merge order: sort each
    // group's list before comparing, so this checks its *contents*, not order that got lucky.
    let normalize = |r: &QueryResult| -> Vec<Vec<Value>> {
        rows(r)
            .into_iter()
            .map(|mut row| {
                if let Value::List(items) = &mut row[list_col] {
                    items.sort_by(|a, b| total_cmp(a, b).expect("status is never NULL or NaN"));
                }
                row
            })
            .collect()
    };
    assert_eq!(normalize(&one), normalize(&four));

    std::fs::remove_dir_all(&dir).unwrap();
}

/// *Catches:* an aggregate emitting no row on empty input.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn count_star_over_empty_table_is_one_row_of_zero() {
    let dir = temp_dir("empty");
    let store = open_events(&dir, opts());
    let view = store.snapshot();

    let plan = Plan::Aggregate {
        input: Box::new(all_cols_scan(None)),
        group_by: vec![],
        aggs: vec![
            AggCall {
                func: AggFunc::CountStar,
                args: vec![],
                filter: None,
                name: "cnt".to_string(),
            },
            AggCall {
                func: AggFunc::Sum,
                args: vec![STATUS_IDX],
                filter: None,
                name: "sum_status".to_string(),
            },
        ],
    };
    let result = view.query(&plan, &ExecOptions::default()).unwrap();
    assert_eq!(rows(&result), vec![vec![Value::Int64(0), Value::Null]]);

    std::fs::remove_dir_all(&dir).unwrap();
}

/// *Catches:* offset/limit off by one, and top-k ordering errors.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn sort_topk_limit() {
    let dir = temp_dir("sort-topk");
    let store = open_events(&dir, opts());
    store.write(&table(), gen_events(0..20)).unwrap();
    store.flush().unwrap();
    let view = store.snapshot();

    let mut want_sorted = batch_rows(&view.scan(&table()).unwrap());
    want_sorted.sort_by(|a, b| {
        status_of_row(a)
            .cmp(&status_of_row(b))
            .then_with(|| id_of_row(b).cmp(&id_of_row(a)))
    });
    let want_slice = want_sorted[2..7].to_vec();

    let sort_limit_plan = Plan::Limit {
        input: Box::new(Plan::Sort {
            input: Box::new(all_cols_scan(None)),
            keys: vec![SortKey::asc(STATUS_IDX), SortKey::desc(ID_IDX)],
        }),
        limit: Some(5),
        offset: 2,
    };
    let got = rows(
        &view
            .query(&sort_limit_plan, &ExecOptions::default())
            .unwrap(),
    );
    assert_eq!(got, want_slice);

    let mut want_by_dur = batch_rows(&view.scan(&table()).unwrap());
    want_by_dur.sort_by(|a, b| dur_of_row(b).partial_cmp(&dur_of_row(a)).unwrap());
    let want_top10 = want_by_dur[..10].to_vec();

    let topk_plan = Plan::TopK {
        input: Box::new(all_cols_scan(None)),
        keys: vec![SortKey::desc(DUR_IDX)],
        k: 10,
    };
    let got_topk = rows(&view.query(&topk_plan, &ExecOptions::default()).unwrap());
    assert_eq!(got_topk, want_top10);

    std::fs::remove_dir_all(&dir).unwrap();
}

/// *Catches:* LEFT behaving as INNER, and a lost build partial.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn hash_join_inner_and_left() {
    let dir = temp_dir("join");
    let store = open_events(&dir, opts());
    store
        .create_table(TableSpec::new(svcs_table(), svcs_schema()))
        .unwrap();
    store.write(&table(), gen_events(0..9)).unwrap();
    // "db" (one of the three svcs in `gen_events`) is missing from `svcs` on purpose.
    store
        .write(
            &svcs_table(),
            svcs_batch(&[("api", "core"), ("web", "core")]),
        )
        .unwrap();
    store.flush().unwrap();
    let view = store.snapshot();

    let events_rows = batch_rows(&view.scan(&table()).unwrap());
    let svcs_rows = batch_rows(&view.scan(&svcs_table()).unwrap());
    let inner_want = events_rows
        .iter()
        .filter(|e| svcs_rows.iter().any(|s| as_str(&s[0]) == svc_of_row(e)))
        .count();
    let left_want = events_rows.len();

    let left_scan = plan_scan(&COLS, None);
    let right_scan = Plan::Scan(ScanSpec {
        db: "main".to_string(),
        table: "svcs".to_string(),
        columns: vec!["svc".to_string(), "team".to_string()],
        predicate: None,
    });

    let inner_plan = Plan::Join {
        left: Box::new(left_scan.clone()),
        right: Box::new(right_scan.clone()),
        kind: JoinKind::Inner,
        on: vec![(SVC_IDX, 0)],
    };
    let inner_got = rows(&view.query(&inner_plan, &ExecOptions::default()).unwrap());
    assert_eq!(inner_got.len(), inner_want);
    assert!(inner_got.iter().all(|r| svc_of_row(r) != "db"));

    let left_plan = Plan::Join {
        left: Box::new(left_scan),
        right: Box::new(right_scan),
        kind: JoinKind::Left,
        on: vec![(SVC_IDX, 0)],
    };
    let left_got = rows(&view.query(&left_plan, &ExecOptions::default()).unwrap());
    assert_eq!(left_got.len(), left_want);
    // Output fields are `left ++ right`; `team` is the right side's second column.
    let team_idx = COLS.len() + 1;
    for r in &left_got {
        if svc_of_row(r) == "db" {
            assert_eq!(r[team_idx], Value::Null);
        }
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

/// *Catches:* a `UnionAll` that drops or duplicates one side's rows.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn union_all_concatenates() {
    let dir = temp_dir("union");
    let store = open_events(&dir, opts());
    store.write(&table(), gen_events(0..30)).unwrap();
    store.flush().unwrap();
    let view = store.snapshot();

    let pred_200 = Expr::cmp(
        CmpOp::Eq,
        Expr::col(STATUS_IDX),
        Expr::lit(Value::Int64(200), DataType::Int64),
    );
    let pred_500 = Expr::cmp(
        CmpOp::Eq,
        Expr::col(STATUS_IDX),
        Expr::lit(Value::Int64(500), DataType::Int64),
    );

    let q200 = view
        .query(
            &all_cols_scan(Some(pred_200.clone())),
            &ExecOptions::default(),
        )
        .unwrap();
    let q500 = view
        .query(
            &all_cols_scan(Some(pred_500.clone())),
            &ExecOptions::default(),
        )
        .unwrap();
    let want = rows(&q200).len() + rows(&q500).len();

    let union_plan = Plan::UnionAll(vec![
        all_cols_scan(Some(pred_200)),
        all_cols_scan(Some(pred_500)),
    ]);
    let got = view.query(&union_plan, &ExecOptions::default()).unwrap();
    assert_eq!(rows(&got).len(), want);

    std::fs::remove_dir_all(&dir).unwrap();
}

/// SPEC §6: the write buffer is queryable. *Catches:* `query` skipping buffered rows.
#[test]
#[cfg_attr(miri, ignore)] // spawns a thread, touches the real filesystem
fn buffered_rows_are_queryable() {
    let dir = temp_dir("buffered");
    let store = Store::open(
        &dir,
        StoreOptions {
            flush_interval: Duration::from_secs(3600),
            ..StoreOptions::default()
        },
    )
    .unwrap();
    store
        .create_table(TableSpec::new(table(), schema()))
        .unwrap();
    let store = Arc::new(store);

    // `write` blocks until its batch is flushed, so it must run off the main thread here.
    let writer = {
        let store = store.clone();
        std::thread::spawn(move || store.write(&table(), gen_events(0..5)).unwrap())
    };

    let deadline = Instant::now() + Duration::from_secs(10);
    let seen = loop {
        let view = store.snapshot();
        let got = rows(
            &view
                .query(&all_cols_scan(None), &ExecOptions::default())
                .unwrap(),
        );
        if got.len() == 5 {
            break got;
        }
        assert!(
            Instant::now() < deadline,
            "buffered rows never became visible"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(seen.len(), 5);

    store.flush().unwrap();
    writer.join().unwrap();
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// *Catches:* `view_at` reading the current rows instead of the pinned version's.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn at_version_reads_the_old_rows() {
    let dir = temp_dir("at-version");
    let store = open_events(&dir, opts());
    store.write(&table(), gen_events(0..5)).unwrap();
    store.flush().unwrap();
    let v1 = store.snapshot().version();
    store.write(&table(), gen_events(5..12)).unwrap();
    store.flush().unwrap();

    let plan = count_star_plan(all_cols_scan(None));
    let old_view = store.view_at(v1).unwrap();
    let old_rows = rows(&old_view.query(&plan, &ExecOptions::default()).unwrap());
    let old_count = as_i64(&old_rows[0][0]);
    assert_eq!(old_count, 5);

    let current = store.snapshot();
    let current_rows = rows(&current.query(&plan, &ExecOptions::default()).unwrap());
    let current_count = as_i64(&current_rows[0][0]);
    assert_eq!(current_count, 12);
    assert!(current_count > old_count);

    std::fs::remove_dir_all(&dir).unwrap();
}

/// *Catches:* no memory accounting (a sort over more data than the budget must fail cleanly).
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn budget_exceeded_is_a_clean_error() {
    let dir = temp_dir("budget");
    let store = open_events(&dir, opts());
    store.write(&table(), gen_events(0..2000)).unwrap();
    store.flush().unwrap();
    let view = store.snapshot();

    let plan = Plan::Sort {
        input: Box::new(all_cols_scan(None)),
        keys: vec![SortKey::asc(ID_IDX)],
    };

    let tiny = ExecOptions {
        memory_limit: 4096,
        ..ExecOptions::default()
    };
    let err = view.query(&plan, &tiny).unwrap_err();
    assert!(
        matches!(err, Error::Exec(ExecError::BudgetExceeded { .. })),
        "{err}"
    );

    let ok = view.query(&plan, &ExecOptions::default());
    assert!(ok.is_ok(), "{:?}", ok.err().map(|e| e.to_string()));

    std::fs::remove_dir_all(&dir).unwrap();
}

/// *Catches:* nothing checking cancellation or the deadline between morsels.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn cancel_and_timeout() {
    let dir = temp_dir("cancel-timeout");
    let store = open_events(&dir, opts());
    store.write(&table(), gen_events(0..20)).unwrap();
    store.flush().unwrap();
    let view = store.snapshot();
    let plan = count_star_plan(all_cols_scan(None));

    let cancel = CancelToken::new();
    cancel.cancel();
    let cancelled = ExecOptions {
        cancel,
        ..ExecOptions::default()
    };
    let err = view.query(&plan, &cancelled).unwrap_err();
    assert!(matches!(err, Error::Exec(ExecError::Cancelled)), "{err}");

    let zero_timeout = ExecOptions {
        timeout: Some(Duration::ZERO),
        ..ExecOptions::default()
    };
    let err = view.query(&plan, &zero_timeout).unwrap_err();
    assert!(
        matches!(err, Error::Exec(ExecError::Timeout { .. })),
        "{err}"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

/// *Catches:* a `Project`/`Filter`/`Case`/`Cast` chain producing wrong values.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn expressions_end_to_end() {
    let dir = temp_dir("expressions");
    let store = open_events(&dir, opts());
    store.write(&table(), gen_events(0..12)).unwrap();
    store.flush().unwrap();
    let view = store.snapshot();

    let project = Plan::Project {
        input: Box::new(all_cols_scan(None)),
        exprs: vec![
            ("svc".to_string(), Expr::col(SVC_IDX)),
            (
                "status_plus_1".to_string(),
                Expr::Arith(
                    ArithOp::Add,
                    Box::new(Expr::col(STATUS_IDX)),
                    Box::new(Expr::lit(Value::Int64(1), DataType::Int64)),
                ),
            ),
            (
                "bucket".to_string(),
                Expr::Case {
                    branches: vec![(
                        Expr::cmp(
                            CmpOp::Gt,
                            Expr::col(DUR_IDX),
                            Expr::lit(Value::Float64(1.0), DataType::Float64),
                        ),
                        Expr::lit(Value::String("slow".to_string()), DataType::String),
                    )],
                    otherwise: Some(Box::new(Expr::lit(
                        Value::String("ok".to_string()),
                        DataType::String,
                    ))),
                },
            ),
            (
                "status_str".to_string(),
                Expr::Cast(Box::new(Expr::col(STATUS_IDX)), DataType::String),
            ),
        ],
    };
    let plan = Plan::Filter {
        input: Box::new(project),
        predicate: Expr::Like {
            expr: Box::new(Expr::col(0)),
            pattern: "api%".to_string(),
            case_insensitive: false,
            negated: false,
        },
    };
    let got = rows(&view.query(&plan, &ExecOptions::default()).unwrap());

    let source_rows = batch_rows(&view.scan(&table()).unwrap());
    let mut want = Vec::new();
    for r in &source_rows {
        if !svc_of_row(r).starts_with("api") {
            continue;
        }
        let status = status_of_row(r);
        let bucket = if dur_of_row(r) > 1.0 { "slow" } else { "ok" };
        want.push(vec![
            Value::String(svc_of_row(r).to_string()),
            Value::Int64(status + 1),
            Value::String(bucket.to_string()),
            Value::String(status.to_string()),
        ]);
    }
    assert_eq!(sorted(got), sorted(want));

    std::fs::remove_dir_all(&dir).unwrap();
}

/// *Catches:* a missing segment file surfacing as a generic `Error::Exec` instead of the
/// downcast `SnapshotExpired` a reader can act on.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn snapshot_expired_surfaces_as_its_own_error() {
    let dir = temp_dir("snapshot-expired");
    let store = open_events(&dir, opts());
    store.write(&table(), gen_events(0..5)).unwrap();
    store.flush().unwrap();

    let entry = store.table(&table()).unwrap();
    let seg_path = Manifest::segment_path(&dir, &entry.segments[0]);
    std::fs::remove_file(&seg_path).unwrap();

    let view = store.snapshot();
    let err = view
        .query(&all_cols_scan(None), &ExecOptions::default())
        .unwrap_err();
    assert!(matches!(err, Error::SnapshotExpired { .. }), "{err}");

    std::fs::remove_dir_all(&dir).unwrap();
}
