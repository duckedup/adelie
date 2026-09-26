//! `Options`, `execute`/`execute_with`, and per-statement dispatch onto `Store` (contract C5).
//! Each statement runs against its own fresh `View` (SPEC's per-statement snapshot rule).

use std::collections::BTreeSet;

use crate::exec;
use crate::storage::manifest::{self, TableName};
use crate::storage::{Store, View};
use crate::types::{DataType, Value};

use super::ast;
use super::binder::Binder;
use super::error::SqlError;
use super::ingest;
use super::planner;
use super::result::{Rows, SqlOutput};

/// Per-call knobs (contract C5). `default_db` is `"main"`; an unqualified table name resolves
/// there.
pub struct Options {
    /// `now()`/`current_timestamp`'s value, folded once at bind time; `None` reads the clock.
    pub now: Option<i64>,
    pub exec: exec::ExecOptions,
    pub default_db: String,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            now: None,
            exec: exec::ExecOptions::default(),
            default_db: "main".to_string(),
        }
    }
}

pub fn execute(store: &Store, sql: &str) -> Result<SqlOutput, SqlError> {
    execute_with(store, sql, &Options::default())
}

/// Runs every `;`-separated statement in `sql`, in order, and returns the last one's output
/// (contract C5).
pub fn execute_with(store: &Store, sql: &str, opts: &Options) -> Result<SqlOutput, SqlError> {
    super::parser::on_sql_stack(|| execute_here(store, sql, opts))
}

fn execute_here(store: &Store, sql: &str, opts: &Options) -> Result<SqlOutput, SqlError> {
    let statements = super::parser::parse_here(sql)?;
    if statements.is_empty() {
        return Err(SqlError::Bind("no statement".to_string()));
    }
    let mut last = None;
    for stmt in &statements {
        last = Some(execute_statement(store, stmt, opts)?);
    }
    Ok(last.expect("checked non-empty above"))
}

fn resolve_name(name: &ast::ObjectName, default_db: &str) -> TableName {
    TableName::new(
        name.db.clone().unwrap_or_else(|| default_db.to_string()),
        name.name.clone(),
    )
}

fn row_estimator(view: &View) -> impl Fn(&str, &str) -> u64 + '_ {
    move |db: &str, table: &str| {
        view.table(&TableName::new(db, table))
            .map(|e| e.segments.iter().map(|s| s.rows).sum())
            .unwrap_or(0)
    }
}

fn execute_statement(
    store: &Store,
    stmt: &ast::Statement,
    opts: &Options,
) -> Result<SqlOutput, SqlError> {
    let view = store.snapshot();
    let binder = Binder::new(&view, opts.default_db.clone(), opts.now);
    match stmt {
        ast::Statement::Query(q) => execute_query(&view, &binder, q, opts),
        ast::Statement::CreateTable(ct) => execute_create_table(store, &binder, ct, opts),
        ast::Statement::CreateSchema(_) => Ok(SqlOutput::Statement { rows_affected: 0 }),
        ast::Statement::DropTable(dt) => execute_drop_table(store, dt, opts),
        ast::Statement::UndropTable(name) => {
            store.undrop_table(&resolve_name(name, &opts.default_db))?;
            Ok(SqlOutput::Statement { rows_affected: 0 })
        }
        ast::Statement::Truncate(name) => {
            store.truncate_table(&resolve_name(name, &opts.default_db))?;
            Ok(SqlOutput::Statement { rows_affected: 0 })
        }
        ast::Statement::Insert(ins) => execute_insert(store, &view, &binder, ins, opts),
        ast::Statement::Copy(cp) => execute_copy(store, &view, cp, opts),
        ast::Statement::Delete(del) => execute_delete(store, &view, &binder, del, opts),
    }
}

fn execute_query(
    view: &View,
    binder: &Binder,
    q: &ast::Query,
    opts: &Options,
) -> Result<SqlOutput, SqlError> {
    let (logical, warnings) = binder.bind_query(q)?;
    let physical = planner::plan(logical, &row_estimator(view))?;
    let result = view.query(&physical, &opts.exec)?;
    Ok(SqlOutput::Rows(Rows {
        fields: result.fields,
        batches: result.batches,
        stats: result.stats,
        warnings,
    }))
}

fn literal_text(lit: &ast::Literal) -> String {
    match lit {
        ast::Literal::Number(s) | ast::Literal::String(s) => s.clone(),
        ast::Literal::Bool(b) => b.to_string(),
        ast::Literal::Null => "null".to_string(),
        ast::Literal::Date(s) | ast::Literal::Timestamp(s) | ast::Literal::Interval(s) => s.clone(),
    }
}

fn execute_create_table(
    store: &Store,
    binder: &Binder,
    ct: &ast::CreateTable,
    opts: &Options,
) -> Result<SqlOutput, SqlError> {
    let tn = resolve_name(&ct.name, &opts.default_db);
    if ct.if_not_exists && store.table(&tn).is_some() {
        return Ok(SqlOutput::Statement { rows_affected: 0 });
    }
    let columns: Vec<exec::Field> = ct
        .columns
        .iter()
        .map(|c| {
            Ok(exec::Field {
                name: c.name.clone(),
                ty: binder.map_type_name(&c.type_name)?,
            })
        })
        .collect::<Result<_, SqlError>>()?;
    let mut spec = crate::storage::TableSpec::new(tn, columns);
    for clause in &ct.clauses {
        spec = match clause {
            ast::TableClause::Engine(e) => spec.engine(e),
            ast::TableClause::Key(cols) => spec.key(cols.clone()),
            ast::TableClause::Version(v) => spec.version(v.clone()),
            ast::TableClause::OrderBy(cols) => spec.order_by(cols.clone()),
            ast::TableClause::PartitionBy { column, bucket } => {
                let d = super::binder::parse_interval(bucket)?;
                spec.partition_by(column.clone(), d)
            }
            ast::TableClause::Ttl { column, interval } => {
                let d = super::binder::parse_interval(interval)?;
                spec.ttl(column.clone(), d)
            }
            ast::TableClause::With(pairs) => {
                let mut s = spec;
                for (k, v) in pairs {
                    s = s.with(k.clone(), literal_text(v));
                }
                s
            }
        };
    }
    store.create_table(spec)?;
    Ok(SqlOutput::Statement { rows_affected: 0 })
}

fn execute_drop_table(
    store: &Store,
    dt: &ast::DropTable,
    opts: &Options,
) -> Result<SqlOutput, SqlError> {
    let tn = resolve_name(&dt.name, &opts.default_db);
    // The live store, not the statement's snapshot: another writer may have dropped it since.
    if dt.if_exists && store.table(&tn).is_none() {
        return Ok(SqlOutput::Statement { rows_affected: 0 });
    }
    match store.drop_table(&tn) {
        Err(_) if dt.if_exists && store.table(&tn).is_none() => {
            Ok(SqlOutput::Statement { rows_affected: 0 })
        }
        other => other
            .map(|_| SqlOutput::Statement { rows_affected: 0 })
            .map_err(Into::into),
    }
}

fn execute_insert(
    store: &Store,
    view: &View,
    binder: &Binder,
    ins: &ast::Insert,
    opts: &Options,
) -> Result<SqlOutput, SqlError> {
    let tn = resolve_name(&ins.table, &opts.default_db);
    let entry = view
        .table(&tn)
        .ok_or_else(|| SqlError::Bind(format!("table {tn} does not exist")))?;
    let fields = entry.fields();
    let mut builder = ingest::RowBuilder::new(&fields);
    let target_cols: Vec<usize> = match &ins.columns {
        Some(names) => names
            .iter()
            .map(|n| {
                builder
                    .index_of(n)
                    .ok_or_else(|| SqlError::Bind(format!("unknown column {n}")))
            })
            .collect::<Result<_, SqlError>>()?,
        None => builder.data_columns().to_vec(),
    };
    match &ins.source {
        ast::InsertSource::Values(rows) => {
            for row in rows {
                if row.len() != target_cols.len() {
                    return Err(SqlError::Bind(format!(
                        "INSERT has {} values for {} columns",
                        row.len(),
                        target_cols.len()
                    )));
                }
                for (&col, expr) in target_cols.iter().zip(row) {
                    let target_ty = fields[col].ty.clone();
                    let v = binder.bind_insert_value(expr, &target_ty)?;
                    builder.set_value(col, v)?;
                }
                builder.end_row()?;
            }
        }
        ast::InsertSource::Query(q) => {
            let (logical, _warnings) = binder.bind_query(q)?;
            let physical = planner::plan(logical, &row_estimator(view))?;
            let result = view.query(&physical, &opts.exec)?;
            for batch in &result.batches {
                for r in 0..batch.rows() {
                    for (i, &col) in target_cols.iter().enumerate() {
                        builder.set_value(col, batch.column(i).get(r))?;
                    }
                    builder.end_row()?;
                }
            }
        }
    }
    let batch = builder.finish()?;
    let rows_affected = batch.rows() as u64;
    store.write(&tn, batch)?;
    Ok(SqlOutput::Statement { rows_affected })
}

fn execute_copy(
    store: &Store,
    view: &View,
    cp: &ast::CopyStatement,
    opts: &Options,
) -> Result<SqlOutput, SqlError> {
    let tn = resolve_name(&cp.table, &opts.default_db);
    let entry = view
        .table(&tn)
        .ok_or_else(|| SqlError::Bind(format!("table {tn} does not exist")))?;
    let fields = entry.fields();

    let format = cp.options.iter().find_map(|o| match o {
        ast::CopyOption::Format(f) => Some(f.to_ascii_lowercase()),
        _ => None,
    });
    let format = format.unwrap_or_else(|| {
        let lower = cp.source.to_ascii_lowercase();
        if lower.ends_with(".ndjson") || lower.ends_with(".jsonl") || lower.ends_with(".json") {
            "ndjson".to_string()
        } else {
            "csv".to_string()
        }
    });
    let header = cp
        .options
        .iter()
        .find_map(|o| match o {
            ast::CopyOption::Header(h) => Some(h.unwrap_or(true)),
            _ => None,
        })
        .unwrap_or(true);
    let delimiter = cp
        .options
        .iter()
        .find_map(|o| match o {
            ast::CopyOption::Delimiter(d) => d.as_bytes().first().copied(),
            _ => None,
        })
        .unwrap_or(b',');

    let rb = ingest::RowBuilder::new(&fields);
    if let Some(cols) = &cp.columns {
        let want: Option<Vec<usize>> = cols.iter().map(|c| rb.index_of(c)).collect();
        let want = want.ok_or_else(|| SqlError::Bind("COPY names an unknown column".into()))?;
        if !header || want.as_slice() != rb.data_columns() {
            return Err(SqlError::Bind("COPY column list needs HEADER".into()));
        }
    }

    let file =
        std::fs::File::open(&cp.source).map_err(|e| SqlError::Io(format!("{}: {e}", cp.source)))?;
    let mut reader = std::io::BufReader::new(file);
    let mut batches: Vec<exec::Batch> = Vec::new();
    let mut sink = |b: exec::Batch| -> Result<(), ingest::IngestError> {
        batches.push(b);
        Ok(())
    };
    let rows = if format == "ndjson" {
        ingest::read_ndjson(&mut reader, &fields, 65_536, &mut sink)
    } else {
        let csv_opts = ingest::CsvOptions { header, delimiter };
        ingest::read_csv(&mut reader, &csv_opts, &fields, 65_536, &mut sink)
    }?;

    // One atomic write: one flush, one commit, durable when it returns (D0016).
    let writes: Vec<(TableName, exec::Batch)> =
        batches.into_iter().map(|b| (tn.clone(), b)).collect();
    if !writes.is_empty() {
        store.write_many(writes)?;
    }
    Ok(SqlOutput::Statement {
        rows_affected: rows,
    })
}

fn map_cmp(op: manifest::CmpOp) -> exec::CmpOp {
    use exec::CmpOp as E;
    use manifest::CmpOp as M;
    match op {
        M::Eq => E::Eq,
        M::Ne => E::Ne,
        M::Lt => E::Lt,
        M::Le => E::Le,
        M::Gt => E::Gt,
        M::Ge => E::Ge,
    }
}

/// `count(*)` over `tn` with the same predicates DELETE is about to apply, on the pre-delete
/// snapshot (D0016: how `rows_affected` is computed).
fn count_matching_plan(
    tn: &TableName,
    fields: &[exec::Field],
    preds: &[manifest::Predicate],
) -> exec::Plan {
    let field_ty = |name: &str| {
        fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.ty.clone())
            .unwrap_or(DataType::String)
    };
    let names: BTreeSet<&str> = preds.iter().map(|p| p.column.as_str()).collect();
    let columns: Vec<String> = if names.is_empty() {
        vec![fields.first().map(|f| f.name.clone()).unwrap_or_default()]
    } else {
        names.into_iter().map(str::to_string).collect()
    };
    let predicate = if preds.is_empty() {
        None
    } else {
        let conjuncts: Vec<exec::Expr> = preds
            .iter()
            .map(|p| {
                let pos = columns
                    .iter()
                    .position(|c| c == &p.column)
                    .expect("column was added above");
                exec::Expr::Cmp(
                    map_cmp(p.op),
                    Box::new(exec::Expr::Column(pos)),
                    Box::new(exec::Expr::Literal(p.value.clone(), field_ty(&p.column))),
                )
            })
            .collect();
        Some(if conjuncts.len() == 1 {
            conjuncts.into_iter().next().unwrap()
        } else {
            exec::Expr::And(conjuncts)
        })
    };
    let scan = exec::Plan::Scan(exec::ScanSpec {
        db: tn.db.clone(),
        table: tn.name.clone(),
        columns,
        predicate: predicate.clone(),
    });
    let input = match predicate {
        Some(p) => exec::Plan::Filter {
            input: Box::new(scan),
            predicate: p,
        },
        None => scan,
    };
    exec::Plan::Aggregate {
        input: Box::new(input),
        group_by: vec![],
        aggs: vec![exec::AggCall {
            func: exec::AggFunc::CountStar,
            args: vec![],
            filter: None,
            name: "n".to_string(),
        }],
    }
}

fn execute_delete(
    store: &Store,
    view: &View,
    binder: &Binder,
    del: &ast::Delete,
    opts: &Options,
) -> Result<SqlOutput, SqlError> {
    let tn = resolve_name(&del.table, &opts.default_db);
    let entry = view
        .table(&tn)
        .ok_or_else(|| SqlError::Bind(format!("table {tn} does not exist")))?;
    let fields = entry.fields();
    let preds = binder.bind_delete_predicates(&fields, &del.selection)?;
    let count_plan = count_matching_plan(&tn, &fields, &preds);
    let result = view.query(&count_plan, &opts.exec)?;
    let rows_affected = match result.batches.first().map(|b| b.column(0).get(0)) {
        Some(Value::Int64(n)) => n.max(0) as u64,
        _ => 0,
    };
    store.delete(&tn, preds)?;
    Ok(SqlOutput::Statement { rows_affected })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::storage::StoreOptions;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "adelie-sql-execute-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn open(dir: &Path) -> Store {
        Store::open(dir, StoreOptions::default()).unwrap()
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn create_insert_select_round_trip() {
        let dir = temp_dir("basic");
        let store = open(&dir);
        execute(&store, "CREATE TABLE t (a INT64, b STRING)").unwrap();
        let out = execute(&store, "INSERT INTO t VALUES (1, 'x'), (2, 'y')").unwrap();
        assert!(matches!(out, SqlOutput::Statement { rows_affected: 2 }));

        let out = execute(&store, "SELECT a, b FROM t WHERE a > 1").unwrap();
        let SqlOutput::Rows(rows) = out else {
            panic!("expected rows")
        };
        let total: usize = rows.batches.iter().map(exec::Batch::rows).sum();
        assert_eq!(total, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn insert_is_durable_across_a_reopen() {
        let dir = temp_dir("durable");
        {
            let store = open(&dir);
            execute(&store, "CREATE TABLE t (a INT64)").unwrap();
            execute(&store, "INSERT INTO t VALUES (1), (2), (3)").unwrap();
            store.close().unwrap();
        }
        {
            let store = open(&dir);
            let out = execute(&store, "SELECT count(*) AS n FROM t").unwrap();
            let SqlOutput::Rows(rows) = out else {
                panic!("expected rows")
            };
            let n = rows.batches[0].column(0).get(0);
            assert_eq!(n, Value::Int64(3));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn delete_removes_matching_rows_and_reports_the_count() {
        let dir = temp_dir("delete");
        let store = open(&dir);
        execute(&store, "CREATE TABLE t (a INT64)").unwrap();
        execute(&store, "INSERT INTO t VALUES (1), (2), (3)").unwrap();
        let out = execute(&store, "DELETE FROM t WHERE a = 2").unwrap();
        assert!(matches!(out, SqlOutput::Statement { rows_affected: 1 }));
        let out = execute(&store, "SELECT a FROM t").unwrap();
        let SqlOutput::Rows(rows) = out else {
            panic!("expected rows")
        };
        let total: usize = rows.batches.iter().map(exec::Batch::rows).sum();
        assert_eq!(total, 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn empty_input_is_a_bind_error() {
        let dir = temp_dir("empty");
        let store = open(&dir);
        let err = execute(&store, "   ").unwrap_err();
        assert!(matches!(err, SqlError::Bind(_)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn create_schema_is_a_no_op() {
        let dir = temp_dir("schema");
        let store = open(&dir);
        let out = execute(&store, "CREATE SCHEMA IF NOT EXISTS otel").unwrap();
        assert!(matches!(out, SqlOutput::Statement { rows_affected: 0 }));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
