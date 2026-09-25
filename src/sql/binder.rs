//! AST → bound logical plan (SPEC §8, D0016): name resolution, typing, the companion rules,
//! function/aggregate resolution, literal typing and `now()` folding. `BExpr`/`LogicalPlan`
//! mirror `exec::Expr`/`exec::Plan` but address columns by a unique internal name rather than
//! a position, so `planner.rs` can prune and push down without reindexing anything (root
//! D0016 planner rule 2: "unique internal names").

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::exec;
use crate::storage::View;
use crate::storage::manifest::{self, TableName};
use crate::types::{DataType, Value, coerce, companion_name};

use super::ast;
use super::error::SqlError;

/// What the binder needs from storage: a table's full field list, or `None` if it does not
/// exist. `View` implements this directly; tests use a small stub.
pub(crate) trait Catalog {
    fn fields_of(&self, name: &TableName) -> Option<Vec<exec::Field>>;
}

impl Catalog for View {
    fn fields_of(&self, name: &TableName) -> Option<Vec<exec::Field>> {
        self.table(name).map(|e| e.fields())
    }
}

// ── bound expressions ───────────────────────────────────────────────────────

/// `exec::Expr`, but `Column` names a unique internal field (e.g. `"r0.id"`) instead of a
/// position. Only resolved to a position by `lower`, once a node's final field list is fixed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BExpr {
    Column(String),
    Literal(Value, DataType),
    Cmp(exec::CmpOp, Box<BExpr>, Box<BExpr>),
    And(Vec<BExpr>),
    Or(Vec<BExpr>),
    Not(Box<BExpr>),
    IsNull(Box<BExpr>),
    IsNotNull(Box<BExpr>),
    InList {
        expr: Box<BExpr>,
        list: Vec<Value>,
        negated: bool,
    },
    Between {
        expr: Box<BExpr>,
        low: Box<BExpr>,
        high: Box<BExpr>,
        negated: bool,
    },
    Like {
        expr: Box<BExpr>,
        pattern: String,
        case_insensitive: bool,
        negated: bool,
    },
    Arith(exec::ArithOp, Box<BExpr>, Box<BExpr>),
    Neg(Box<BExpr>),
    Case {
        branches: Vec<(BExpr, BExpr)>,
        otherwise: Option<Box<BExpr>>,
    },
    Cast(Box<BExpr>, DataType),
    Func {
        func: exec::ScalarFunc,
        args: Vec<BExpr>,
    },
}

/// Resolves every `Column(name)` to its position in `fields`. Only ever fails on a binder
/// bug (a name `fields` does not carry), so it reports as `Plan`, never `Bind`.
pub(crate) fn lower(e: &BExpr, fields: &[exec::Field]) -> Result<exec::Expr, SqlError> {
    Ok(match e {
        BExpr::Column(name) => {
            let idx = fields.iter().position(|f| &f.name == name).ok_or_else(|| {
                SqlError::Plan(format!("internal error: unresolved column {name}"))
            })?;
            exec::Expr::Column(idx)
        }
        BExpr::Literal(v, t) => exec::Expr::Literal(v.clone(), t.clone()),
        BExpr::Cmp(op, l, r) => exec::Expr::Cmp(
            *op,
            Box::new(lower(l, fields)?),
            Box::new(lower(r, fields)?),
        ),
        BExpr::And(parts) => exec::Expr::And(lower_all(parts, fields)?),
        BExpr::Or(parts) => exec::Expr::Or(lower_all(parts, fields)?),
        BExpr::Not(e) => exec::Expr::Not(Box::new(lower(e, fields)?)),
        BExpr::IsNull(e) => exec::Expr::IsNull(Box::new(lower(e, fields)?)),
        BExpr::IsNotNull(e) => exec::Expr::IsNotNull(Box::new(lower(e, fields)?)),
        BExpr::InList {
            expr,
            list,
            negated,
        } => exec::Expr::InList {
            expr: Box::new(lower(expr, fields)?),
            list: list.clone(),
            negated: *negated,
        },
        BExpr::Between {
            expr,
            low,
            high,
            negated,
        } => exec::Expr::Between {
            expr: Box::new(lower(expr, fields)?),
            low: Box::new(lower(low, fields)?),
            high: Box::new(lower(high, fields)?),
            negated: *negated,
        },
        BExpr::Like {
            expr,
            pattern,
            case_insensitive,
            negated,
        } => exec::Expr::Like {
            expr: Box::new(lower(expr, fields)?),
            pattern: pattern.clone(),
            case_insensitive: *case_insensitive,
            negated: *negated,
        },
        BExpr::Arith(op, l, r) => exec::Expr::Arith(
            *op,
            Box::new(lower(l, fields)?),
            Box::new(lower(r, fields)?),
        ),
        BExpr::Neg(e) => exec::Expr::Neg(Box::new(lower(e, fields)?)),
        BExpr::Case {
            branches,
            otherwise,
        } => exec::Expr::Case {
            branches: branches
                .iter()
                .map(|(c, r)| Ok((lower(c, fields)?, lower(r, fields)?)))
                .collect::<Result<_, SqlError>>()?,
            otherwise: otherwise
                .as_deref()
                .map(|o| lower(o, fields))
                .transpose()?
                .map(Box::new),
        },
        BExpr::Cast(e, t) => exec::Expr::Cast(Box::new(lower(e, fields)?), t.clone()),
        BExpr::Func { func, args } => exec::Expr::Func {
            func: func.clone(),
            args: lower_all(args, fields)?,
        },
    })
}

fn lower_all(parts: &[BExpr], fields: &[exec::Field]) -> Result<Vec<exec::Expr>, SqlError> {
    parts.iter().map(|p| lower(p, fields)).collect()
}

fn contains_column(e: &BExpr) -> bool {
    match e {
        BExpr::Column(_) => true,
        BExpr::Literal(..) => false,
        BExpr::Cmp(_, l, r) | BExpr::Arith(_, l, r) => contains_column(l) || contains_column(r),
        BExpr::And(parts) | BExpr::Or(parts) => parts.iter().any(contains_column),
        BExpr::Not(e) | BExpr::IsNull(e) | BExpr::IsNotNull(e) | BExpr::Neg(e) => {
            contains_column(e)
        }
        BExpr::InList { expr, .. } => contains_column(expr),
        BExpr::Between {
            expr, low, high, ..
        } => contains_column(expr) || contains_column(low) || contains_column(high),
        BExpr::Like { expr, .. } => contains_column(expr),
        BExpr::Case {
            branches,
            otherwise,
        } => {
            branches
                .iter()
                .any(|(c, r)| contains_column(c) || contains_column(r))
                || otherwise.as_deref().is_some_and(contains_column)
        }
        BExpr::Cast(e, _) => contains_column(e),
        BExpr::Func { args, .. } => args.iter().any(contains_column),
    }
}

/// Evaluates a literal-only `BExpr` on a 1-row dummy batch (D0016 planner rule 5). A
/// column reference or an eval error leaves it unfolded: `None`.
pub(crate) fn fold_constant(e: &BExpr) -> Option<Value> {
    if contains_column(e) {
        return None;
    }
    let dummy = exec::Field {
        name: "_dummy".to_string(),
        ty: DataType::Bool,
    };
    let fields = vec![dummy];
    let lowered = lower(e, &fields).ok()?;
    let col = exec::Column::from_values(&DataType::Bool, &[Value::Bool(true)]).ok()?;
    let batch = exec::Batch::new(fields, vec![col]).ok()?;
    lowered.data_type(batch.fields()).ok()?;
    let out = exec::expr::eval(&lowered, &batch).ok()?;
    Some(out.get(0))
}

// ── bound logical plan ──────────────────────────────────────────────────────

/// One `SELECT`-list aggregate call, bound (D0016).
#[derive(Debug, Clone)]
pub(crate) struct LAgg {
    pub func: exec::AggFunc,
    pub args: Vec<BExpr>,
    pub filter: Option<BExpr>,
    pub name: String,
}

/// The binder's output: a relational tree over `BExpr`, lowered to `exec::Plan` by
/// `planner.rs`. Every variant that changes its input's field list stores `fields` directly
/// (computed once, at construction, rather than re-derived on every `fields()` call).
#[derive(Debug, Clone)]
pub(crate) enum LogicalPlan {
    Scan {
        db: String,
        table: String,
        fields: Vec<exec::Field>,
    },
    Values {
        fields: Vec<exec::Field>,
        rows: Vec<Vec<Value>>,
    },
    Filter {
        input: Box<LogicalPlan>,
        predicate: BExpr,
    },
    Project {
        input: Box<LogicalPlan>,
        exprs: Vec<(String, BExpr)>,
        fields: Vec<exec::Field>,
    },
    Aggregate {
        input: Box<LogicalPlan>,
        group_by: Vec<BExpr>,
        aggs: Vec<LAgg>,
        fields: Vec<exec::Field>,
    },
    Join {
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        kind: exec::JoinKind,
        on: Vec<(String, String)>,
        fields: Vec<exec::Field>,
    },
    Sort {
        input: Box<LogicalPlan>,
        keys: Vec<(String, bool, bool)>,
    },
    Limit {
        input: Box<LogicalPlan>,
        limit: Option<i64>,
        offset: i64,
    },
    UnionAll {
        branches: Vec<LogicalPlan>,
        fields: Vec<exec::Field>,
    },
}

impl LogicalPlan {
    pub(crate) fn fields(&self) -> &[exec::Field] {
        match self {
            LogicalPlan::Scan { fields, .. }
            | LogicalPlan::Values { fields, .. }
            | LogicalPlan::Project { fields, .. }
            | LogicalPlan::Aggregate { fields, .. }
            | LogicalPlan::Join { fields, .. }
            | LogicalPlan::UnionAll { fields, .. } => fields,
            LogicalPlan::Filter { input, .. } => input.fields(),
            LogicalPlan::Sort { input, .. } => input.fields(),
            LogicalPlan::Limit { input, .. } => input.fields(),
        }
    }
}

// ── scope: user-facing names in play while binding ──────────────────────────

#[derive(Debug, Clone)]
struct ScopeCol {
    qualifier: String,
    name: String,
    internal: String,
    ty: DataType,
    /// The base table and field this column reads, for the wsr.1 warning; `None` once a
    /// column has passed through any subquery, join or aggregate.
    origin: Option<(TableName, String)>,
    /// True only for a base field that is not itself a companion and has one.
    has_companion: bool,
}

#[derive(Debug, Clone, Default)]
struct Scope {
    cols: Vec<ScopeCol>,
}

impl Scope {
    fn fields(&self) -> Vec<exec::Field> {
        self.cols
            .iter()
            .map(|c| exec::Field {
                name: c.internal.clone(),
                ty: c.ty.clone(),
            })
            .collect()
    }

    fn from_fields(fields: &[exec::Field]) -> Scope {
        Scope {
            cols: fields
                .iter()
                .map(|f| ScopeCol {
                    qualifier: String::new(),
                    name: f.name.clone(),
                    internal: f.name.clone(),
                    ty: f.ty.clone(),
                    origin: None,
                    has_companion: false,
                })
                .collect(),
        }
    }

    fn prefixes(&self) -> HashSet<String> {
        self.cols.iter().map(|c| prefix_of(&c.internal)).collect()
    }
}

pub(crate) fn prefix_of(internal: &str) -> String {
    internal
        .split_once('.')
        .map(|(p, _)| p.to_string())
        .unwrap_or_else(|| internal.to_string())
}

/// A select item, resolved to a name and a bound expression, before `dedupe_output_names`.
type NamedExpr = (String, BExpr);

/// What binding one `SELECT` (or set of `UNION ALL` branches) leaves behind: the plan whose
/// own fields `output_items` are bound against, and those items themselves. `bind_query`
/// applies `ORDER BY`/`LIMIT`/`OFFSET` on top, materialising `output_items` (plus any hidden
/// `ORDER BY`-only column) into a real `Project`.
struct SelectResult {
    scope_plan: LogicalPlan,
    scope: Scope,
    output_items: Vec<NamedExpr>,
}

// ── the binder itself ────────────────────────────────────────────────────────

const AGGREGATE_NAMES: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "approx_count_distinct",
    "quantile",
    "quantile_disc",
    "approx_quantile",
    "top_k",
    "arg_min",
    "arg_max",
    "min_by",
    "max_by",
    "list",
    "list_agg",
    "array_agg",
    "histogram",
];

fn is_aggregate_name(name: &str) -> bool {
    AGGREGATE_NAMES.contains(&name.to_ascii_lowercase().as_str())
}

/// Aggregate-extraction state for one `SELECT`: the `GROUP BY` items (as written, after
/// ordinal/alias substitution) and the aggregate calls collected from the select list,
/// `HAVING` and (for those already selected) `ORDER BY`.
struct AggContext {
    aggregate_mode: bool,
    group_keys: Vec<ast::Expr>,
    group_names: Vec<String>,
    group_types: Vec<DataType>,
    collected: RefCell<Vec<(ast::Expr, LAgg)>>,
}

/// The most base-table scans one statement may bind (D0016).
const MAX_RELATIONS: usize = 10_000;

pub(crate) struct Binder<'a> {
    catalog: &'a dyn Catalog,
    default_db: String,
    now: i64,
    next_rel: Cell<usize>,
    warnings: RefCell<Vec<String>>,
    warned: RefCell<HashSet<(String, String)>>,
}

impl<'a> Binder<'a> {
    pub(crate) fn new(
        catalog: &'a dyn Catalog,
        default_db: String,
        now: Option<i64>,
    ) -> Binder<'a> {
        let now = now.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0)
        });
        Binder {
            catalog,
            default_db,
            now,
            next_rel: Cell::new(0),
            warnings: RefCell::new(Vec::new()),
            warned: RefCell::new(HashSet::new()),
        }
    }

    fn next_rel(&self) -> usize {
        let r = self.next_rel.get();
        self.next_rel.set(r + 1);
        r
    }

    // ── types ────────────────────────────────────────────────────────────

    /// `CREATE TABLE`'s type-name table (D0016).
    pub(crate) fn map_type_name(&self, t: &ast::TypeName) -> Result<DataType, SqlError> {
        if let Some(inner) = &t.list_of {
            let elem = self.map_type_name(inner)?;
            return DataType::list(elem).map_err(|e| SqlError::Bind(e.to_string()));
        }
        let base = match t.name.to_ascii_uppercase().as_str() {
            "BOOLEAN" | "BOOL" => DataType::Bool,
            "TINYINT" | "SMALLINT" | "INT" | "INTEGER" | "INT4" | "BIGINT" | "INT8" | "INT64" => {
                DataType::Int64
            }
            "UBIGINT" | "UINT64" => DataType::UInt64,
            "REAL" | "FLOAT" | "FLOAT4" | "DOUBLE" | "FLOAT8" | "FLOAT64" => DataType::Float64,
            "DECIMAL" | "NUMERIC" => {
                let (p, s) = match t.params.as_slice() {
                    [] => (18u8, 3u8),
                    [p] => (*p as u8, 0),
                    [p, s, ..] => (*p as u8, *s as u8),
                };
                return DataType::decimal(p, s).map_err(|e| SqlError::Bind(e.to_string()));
            }
            "TEXT" | "VARCHAR" | "CHAR" | "STRING" => DataType::String,
            "BLOB" | "BYTEA" | "BYTES" => DataType::Bytes,
            "TIMESTAMP" => DataType::Timestamp,
            "DATE" => DataType::Date,
            "UUID" => DataType::Uuid,
            "INET" | "IP" => DataType::Ip,
            other => return Err(SqlError::Bind(format!("unknown type {other}"))),
        };
        let mut ty = base;
        for _ in 0..t.array_dims {
            ty = DataType::list(ty).map_err(|e| SqlError::Bind(e.to_string()))?;
        }
        Ok(ty)
    }

    fn literal_value(&self, lit: &ast::Literal, target: &DataType) -> Result<Value, SqlError> {
        match lit {
            ast::Literal::Null => Ok(Value::Null),
            ast::Literal::Number(text) => Value::from_text(text, target)
                .ok_or_else(|| SqlError::Bind(format!("value {text} does not fit {target}"))),
            ast::Literal::String(s) => Ok(Value::String(s.clone())),
            ast::Literal::Bool(b) => Ok(Value::Bool(*b)),
            ast::Literal::Date(s) => Value::from_text(s, &DataType::Date)
                .ok_or_else(|| SqlError::Bind(format!("invalid date '{s}'"))),
            ast::Literal::Timestamp(s) => Value::from_text(s, &DataType::Timestamp)
                .ok_or_else(|| SqlError::Bind(format!("invalid timestamp '{s}'"))),
            ast::Literal::Interval(_) => Err(SqlError::Bind("INTERVAL is not valid here".into())),
        }
    }

    /// `INSERT ... VALUES`: binds `e` with no input relation, constant-folds it, and requires
    /// a literal (D0016). A string that does not fit `target` is returned as-is, so
    /// `RowBuilder::set_value` routes it to the declared companion (wsr.1) or fails naming it.
    pub(crate) fn bind_insert_value(
        &self,
        e: &ast::Expr,
        target: &DataType,
    ) -> Result<Value, SqlError> {
        let empty = Scope::default();
        let (be, _ty) = self.bind_expr(e, &empty, Some(target), None)?;
        let v = fold_constant(&be)
            .ok_or_else(|| SqlError::Bind("INSERT value must be constant".into()))?;
        Ok(coerce(&v, target).unwrap_or(v))
    }

    /// `DELETE ... WHERE`: an AND of `column op literal` (D0016). No `WHERE` at all
    /// is an empty predicate list, which storage's tombstone match treats as "matches every
    /// row" (an AND of zero conjuncts), so it needs no special case here.
    pub(crate) fn bind_delete_predicates(
        &self,
        fields: &[exec::Field],
        selection: &Option<ast::Expr>,
    ) -> Result<Vec<manifest::Predicate>, SqlError> {
        let Some(sel) = selection else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for c in ast_conjuncts(sel) {
            let bad = || {
                SqlError::Bind(
                    "DELETE supports AND of column comparisons with literals in v1".into(),
                )
            };
            let ast::Expr::BinaryOp { op, left, right } = c else {
                return Err(bad());
            };
            let Some(cmp) = manifest_cmp(*op) else {
                return Err(bad());
            };
            let (col_name, lit, cmp) = match (left.as_ref(), right.as_ref()) {
                (ast::Expr::Column(p), ast::Expr::Literal(l)) if p.len() == 1 => {
                    (p[0].clone(), l, cmp)
                }
                (ast::Expr::Literal(l), ast::Expr::Column(p)) if p.len() == 1 => {
                    (p[0].clone(), l, flip_manifest_cmp(cmp))
                }
                _ => return Err(bad()),
            };
            let field = fields
                .iter()
                .find(|f| f.name == col_name)
                .ok_or_else(|| SqlError::Bind(format!("column \"{col_name}\" does not exist")))?;
            let value = self.literal_value(lit, &field.ty)?;
            if value.is_null() {
                return Err(bad());
            }
            out.push(manifest::Predicate {
                column: col_name,
                op: cmp,
                value,
            });
        }
        Ok(out)
    }

    // ── query entry point ────────────────────────────────────────────────

    /// Binds one `Query` (contract C5's per-statement snapshot: the caller takes a fresh
    /// `View` and builds a fresh `Binder` per statement). Returns the final plan, whose
    /// fields are exactly the user-facing output columns, plus the wsr.1 warnings gathered.
    pub(crate) fn bind_query(
        &self,
        q: &ast::Query,
    ) -> Result<(LogicalPlan, Vec<String>), SqlError> {
        let plan = self.bind_query_with(q, &[])?;
        Ok((plan, self.warnings.borrow().clone()))
    }

    fn bind_query_with(
        &self,
        q: &ast::Query,
        outer_ctes: &[(String, ast::Query)],
    ) -> Result<LogicalPlan, SqlError> {
        let mut ctes: Vec<(String, ast::Query)> = outer_ctes.to_vec();
        for cte in &q.ctes {
            ctes.push((cte.name.clone(), (*cte.query).clone()));
        }
        let sr = self.bind_set_expr(&q.body, &ctes)?;
        self.finish_query(sr, &q.order_by, &q.limit, q.offset)
    }

    fn bind_set_expr(
        &self,
        se: &ast::SetExpr,
        ctes: &[(String, ast::Query)],
    ) -> Result<SelectResult, SqlError> {
        if se.selects.len() == 1 {
            return self.bind_select_body(&se.selects[0], ctes);
        }
        let mut branches = Vec::with_capacity(se.selects.len());
        for sb in &se.selects {
            let sr = self.bind_select_body(sb, ctes)?;
            branches.push(self.finalize_branch(sr)?);
        }
        let arity = branches[0].fields().len();
        for (i, p) in branches.iter().enumerate() {
            if p.fields().len() != arity {
                return Err(SqlError::Bind(format!(
                    "UNION ALL branch {} has {} columns, branch 1 has {arity}",
                    i + 1,
                    p.fields().len()
                )));
            }
        }
        let mut col_types: Vec<DataType> =
            branches[0].fields().iter().map(|f| f.ty.clone()).collect();
        for p in &branches[1..] {
            for (i, f) in p.fields().iter().enumerate() {
                col_types[i] = unify_types(&col_types[i], &f.ty);
            }
        }
        let first_names: Vec<String> = branches[0]
            .fields()
            .iter()
            .map(|f| f.name.clone())
            .collect();
        let mut casted = Vec::with_capacity(branches.len());
        for p in branches {
            let src_fields = p.fields().to_vec();
            let exprs: Vec<(String, BExpr)> = src_fields
                .iter()
                .zip(&col_types)
                .zip(&first_names)
                .map(|((f, t), n)| {
                    let e = cast_towards(BExpr::Column(f.name.clone()), &f.ty, t);
                    (n.clone(), e)
                })
                .collect();
            let out_fields: Vec<exec::Field> = first_names
                .iter()
                .zip(&col_types)
                .map(|(n, t)| exec::Field {
                    name: n.clone(),
                    ty: t.clone(),
                })
                .collect();
            casted.push(LogicalPlan::Project {
                input: Box::new(p),
                exprs,
                fields: out_fields,
            });
        }
        let fields = casted[0].fields().to_vec();
        let union_plan = LogicalPlan::UnionAll {
            branches: casted,
            fields: fields.clone(),
        };
        let scope = Scope::from_fields(&fields);
        let output_items = scope
            .cols
            .iter()
            .map(|c| (c.name.clone(), BExpr::Column(c.internal.clone())))
            .collect();
        Ok(SelectResult {
            scope_plan: union_plan,
            scope,
            output_items,
        })
    }

    fn bind_select_body(
        &self,
        body: &ast::SelectBody,
        ctes: &[(String, ast::Query)],
    ) -> Result<SelectResult, SqlError> {
        match body {
            ast::SelectBody::Select(s) => self.bind_select(s, ctes),
            ast::SelectBody::Query(q) => {
                let plan = self.bind_query_with(q, ctes)?;
                let scope = Scope::from_fields(plan.fields());
                let output_items = scope
                    .cols
                    .iter()
                    .map(|c| (c.name.clone(), BExpr::Column(c.internal.clone())))
                    .collect();
                Ok(SelectResult {
                    scope_plan: plan,
                    scope,
                    output_items,
                })
            }
        }
    }

    fn finalize_branch(&self, sr: SelectResult) -> Result<LogicalPlan, SqlError> {
        let input_fields = sr.scope_plan.fields().to_vec();
        let mut fields = Vec::with_capacity(sr.output_items.len());
        for (n, e) in &sr.output_items {
            fields.push(exec::Field {
                name: n.clone(),
                ty: self.typeof_bexpr(e, &input_fields)?,
            });
        }
        Ok(LogicalPlan::Project {
            input: Box::new(sr.scope_plan),
            exprs: sr.output_items,
            fields,
        })
    }

    /// `ORDER BY`/`LIMIT`/`OFFSET` (D0016): an `ORDER BY` item not already in
    /// `output_items` is carried as a hidden column and dropped by the final `Project`.
    fn finish_query(
        &self,
        sr: SelectResult,
        order_by: &[ast::OrderItem],
        limit: &Option<ast::Limit>,
        offset: Option<i64>,
    ) -> Result<LogicalPlan, SqlError> {
        let SelectResult {
            scope_plan,
            scope,
            output_items,
        } = sr;
        let n_output = output_items.len();
        let mut combined = output_items.clone();
        let mut keys: Vec<(String, bool, bool)> = Vec::new();
        for (idx, item) in order_by.iter().enumerate() {
            let bexpr = self.resolve_order_item(item, &scope, &output_items)?;
            let name = match combined.iter().position(|(_, e)| e == &bexpr) {
                Some(pos) => combined[pos].0.clone(),
                None => {
                    let hidden = format!("_order{idx}");
                    combined.push((hidden.clone(), bexpr));
                    hidden
                }
            };
            let nulls_first = match item.nulls {
                Some(ast::NullsOrder::First) => true,
                Some(ast::NullsOrder::Last) => false,
                None => item.desc,
            };
            keys.push((name, item.desc, nulls_first));
        }
        let has_hidden = combined.len() > n_output;
        let scope_fields = scope.fields();
        let mut combined_fields = Vec::with_capacity(combined.len());
        for (n, e) in &combined {
            combined_fields.push(exec::Field {
                name: n.clone(),
                ty: self.typeof_bexpr(e, &scope_fields)?,
            });
        }
        let mut plan = LogicalPlan::Project {
            input: Box::new(scope_plan),
            exprs: combined,
            fields: combined_fields,
        };
        if !keys.is_empty() {
            plan = LogicalPlan::Sort {
                input: Box::new(plan),
                keys,
            };
        }
        let limit_n = match limit {
            Some(ast::Limit::Count(n)) => Some(*n),
            Some(ast::Limit::All) | None => None,
        };
        let off = offset.unwrap_or(0);
        if limit_n.is_some() || off != 0 {
            plan = LogicalPlan::Limit {
                input: Box::new(plan),
                limit: limit_n,
                offset: off,
            };
        }
        if has_hidden {
            let final_fields: Vec<exec::Field> = output_items
                .iter()
                .map(|(n, _)| {
                    plan.fields()
                        .iter()
                        .find(|f| &f.name == n)
                        .cloned()
                        .expect("kept in combined")
                })
                .collect();
            let final_exprs: Vec<(String, BExpr)> = output_items
                .iter()
                .map(|(n, _)| (n.clone(), BExpr::Column(n.clone())))
                .collect();
            plan = LogicalPlan::Project {
                input: Box::new(plan),
                exprs: final_exprs,
                fields: final_fields,
            };
        }
        Ok(plan)
    }

    fn resolve_order_item(
        &self,
        item: &ast::OrderItem,
        scope: &Scope,
        output_items: &[NamedExpr],
    ) -> Result<BExpr, SqlError> {
        if let ast::Expr::Literal(ast::Literal::Number(n)) = &item.expr
            && let Ok(idx) = n.parse::<usize>()
        {
            return match idx.checked_sub(1).and_then(|i| output_items.get(i)) {
                Some((_, e)) => Ok(e.clone()),
                None => Err(SqlError::Bind(format!(
                    "ORDER BY position {idx} is out of range"
                ))),
            };
        }
        if let ast::Expr::Column(parts) = &item.expr
            && parts.len() == 1
            && let Some((_, e)) = output_items.iter().find(|(n, _)| n == &parts[0])
        {
            return Ok(e.clone());
        }
        Ok(self.bind_expr(&item.expr, scope, None, None)?.0)
    }

    // ── SELECT ───────────────────────────────────────────────────────────

    fn bind_select(
        &self,
        sel: &ast::Select,
        ctes: &[(String, ast::Query)],
    ) -> Result<SelectResult, SqlError> {
        if sel.from.len() > 1 {
            return Err(SqlError::Bind("use JOIN … ON to combine tables".into()));
        }
        let (from_plan, from_scope) = match sel.from.first() {
            Some(item) => self.bind_from_item(item, ctes)?,
            None => {
                let field = exec::Field {
                    name: "_dummy".to_string(),
                    ty: DataType::Bool,
                };
                let plan = LogicalPlan::Values {
                    fields: vec![field],
                    rows: vec![vec![Value::Bool(true)]],
                };
                let scope = Scope {
                    cols: vec![ScopeCol {
                        qualifier: String::new(),
                        name: "_dummy".to_string(),
                        internal: "_dummy".to_string(),
                        ty: DataType::Bool,
                        origin: None,
                        has_companion: false,
                    }],
                };
                (plan, scope)
            }
        };
        let filtered_plan = match &sel.selection {
            Some(w) => {
                let (be, _t) = self.bind_expr(w, &from_scope, Some(&DataType::Bool), None)?;
                LogicalPlan::Filter {
                    input: Box::new(from_plan),
                    predicate: be,
                }
            }
            None => from_plan,
        };

        let group_by_ast = self.resolve_group_by_items(&sel.group_by, &sel.projection)?;
        let agg_in_select = sel
            .projection
            .iter()
            .any(|it| matches!(it, ast::SelectItem::Expr { expr, .. } if contains_agg(expr)));
        let agg_in_having = sel.having.as_ref().is_some_and(contains_agg);
        let aggregate_mode = !group_by_ast.is_empty() || agg_in_select || agg_in_having;

        if aggregate_mode {
            let from_fields = from_scope.fields();
            let mut pre_agg: Vec<(String, BExpr)> = Vec::new();
            let mut group_names = Vec::with_capacity(group_by_ast.len());
            let mut group_types = Vec::with_capacity(group_by_ast.len());
            for (i, g) in group_by_ast.iter().enumerate() {
                let (be, ty) = self.bind_expr(g, &from_scope, None, None)?;
                let name = format!("_g{i}");
                pre_agg.push((name.clone(), be));
                group_names.push(name);
                group_types.push(ty);
            }
            let actx = AggContext {
                aggregate_mode: true,
                group_keys: group_by_ast,
                group_names: group_names.clone(),
                group_types: group_types.clone(),
                collected: RefCell::new(Vec::new()),
            };
            let mut output_items = Vec::with_capacity(sel.projection.len());
            for item in &sel.projection {
                match item {
                    ast::SelectItem::Wildcard | ast::SelectItem::QualifiedWildcard(_) => {
                        return Err(SqlError::Bind(
                            "SELECT * is not allowed with GROUP BY or aggregates".into(),
                        ));
                    }
                    ast::SelectItem::Expr { expr, alias } => {
                        let (be, _ty) = self.bind_expr(expr, &from_scope, None, Some(&actx))?;
                        output_items.push((output_name(expr, alias.as_deref()), be));
                    }
                }
            }
            let output_items = dedupe_output_names(output_items);
            let having = match &sel.having {
                Some(h) => Some(
                    self.bind_expr(h, &from_scope, Some(&DataType::Bool), Some(&actx))?
                        .0,
                ),
                None => None,
            };
            let collected = actx.collected.into_inner();
            let mut aggs = Vec::with_capacity(collected.len());
            let mut agg_fields: Vec<exec::Field> = group_names
                .iter()
                .zip(&group_types)
                .map(|(n, t)| exec::Field {
                    name: n.clone(),
                    ty: t.clone(),
                })
                .collect();
            // `exec::AggCall.args`/`.filter` and `Plan::Aggregate.group_by` are column
            // *positions*: any non-trivial arg, filter or GROUP BY expression is materialized
            // into a named column by `pre_agg` first (D0016).
            for (_, a) in collected {
                let arg_types = self.arg_types_of(&a.args, &from_fields)?;
                let mut arg_cols = Vec::with_capacity(a.args.len());
                for (j, arg_expr) in a.args.into_iter().enumerate() {
                    let name = format!("{}_arg{j}", a.name);
                    pre_agg.push((name.clone(), arg_expr));
                    arg_cols.push(BExpr::Column(name));
                }
                let filter_col = a.filter.map(|f| {
                    let name = format!("{}_filter", a.name);
                    pre_agg.push((name.clone(), f));
                    BExpr::Column(name)
                });
                agg_fields.push(exec::Field {
                    name: a.name.clone(),
                    ty: agg_result_type(&a.func, &arg_types)?,
                });
                aggs.push(LAgg {
                    func: a.func,
                    args: arg_cols,
                    filter: filter_col,
                    name: a.name,
                });
            }
            // A zero-column batch has zero rows, so `count(*)` alone still needs one column.
            if pre_agg.is_empty() {
                pre_agg.push((
                    "#rows".to_string(),
                    BExpr::Literal(Value::Bool(true), DataType::Bool),
                ));
            }
            let mut pre_fields = Vec::with_capacity(pre_agg.len());
            for (n, e) in &pre_agg {
                pre_fields.push(exec::Field {
                    name: n.clone(),
                    ty: self.typeof_bexpr(e, &from_fields)?,
                });
            }
            let pre_agg_plan = LogicalPlan::Project {
                input: Box::new(filtered_plan),
                exprs: pre_agg,
                fields: pre_fields,
            };
            let group_by: Vec<BExpr> = group_names
                .iter()
                .map(|n| BExpr::Column(n.clone()))
                .collect();
            let agg_plan = LogicalPlan::Aggregate {
                input: Box::new(pre_agg_plan),
                group_by,
                aggs,
                fields: agg_fields.clone(),
            };
            let scope_plan = match having {
                Some(hf) => LogicalPlan::Filter {
                    input: Box::new(agg_plan),
                    predicate: hf,
                },
                None => agg_plan,
            };
            let scope = Scope::from_fields(&agg_fields);
            Ok(SelectResult {
                scope_plan,
                scope,
                output_items,
            })
        } else {
            let mut output_items = Vec::with_capacity(sel.projection.len());
            for item in &sel.projection {
                match item {
                    ast::SelectItem::Wildcard => {
                        for col in &from_scope.cols {
                            self.maybe_warn(col);
                            output_items
                                .push((col.name.clone(), BExpr::Column(col.internal.clone())));
                        }
                    }
                    ast::SelectItem::QualifiedWildcard(q) => {
                        for col in from_scope.cols.iter().filter(|c| &c.qualifier == q) {
                            self.maybe_warn(col);
                            output_items
                                .push((col.name.clone(), BExpr::Column(col.internal.clone())));
                        }
                    }
                    ast::SelectItem::Expr { expr, alias } => {
                        let (be, _ty) = self.bind_expr(expr, &from_scope, None, None)?;
                        output_items.push((output_name(expr, alias.as_deref()), be));
                    }
                }
            }
            let output_items = dedupe_output_names(output_items);
            if sel.distinct {
                let from_fields = from_scope.fields();
                let mut proj_fields = Vec::with_capacity(output_items.len());
                for (n, e) in &output_items {
                    proj_fields.push(exec::Field {
                        name: n.clone(),
                        ty: self.typeof_bexpr(e, &from_fields)?,
                    });
                }
                let project = LogicalPlan::Project {
                    input: Box::new(filtered_plan),
                    exprs: output_items,
                    fields: proj_fields.clone(),
                };
                let group_by: Vec<BExpr> = proj_fields
                    .iter()
                    .map(|f| BExpr::Column(f.name.clone()))
                    .collect();
                let agg = LogicalPlan::Aggregate {
                    input: Box::new(project),
                    group_by,
                    aggs: vec![],
                    fields: proj_fields,
                };
                let scope = Scope::from_fields(agg.fields());
                let output_items = scope
                    .cols
                    .iter()
                    .map(|c| (c.name.clone(), BExpr::Column(c.internal.clone())))
                    .collect();
                Ok(SelectResult {
                    scope_plan: agg,
                    scope,
                    output_items,
                })
            } else {
                Ok(SelectResult {
                    scope_plan: filtered_plan,
                    scope: from_scope,
                    output_items,
                })
            }
        }
    }

    fn maybe_warn(&self, col: &ScopeCol) {
        if !col.has_companion {
            return;
        }
        let Some((table, field)) = &col.origin else {
            return;
        };
        let key = (table.to_string(), field.clone());
        if self.warned.borrow_mut().insert(key) {
            self.warnings.borrow_mut().push(format!(
                "column {table}.{field} has a companion \"{field}::string\": values that did not fit read as NULL here; read them with coalesce_text({field}) or {field}::string"
            ));
        }
    }

    fn resolve_group_by_items(
        &self,
        group_by: &[ast::Expr],
        projection: &[ast::SelectItem],
    ) -> Result<Vec<ast::Expr>, SqlError> {
        let mut out = Vec::with_capacity(group_by.len());
        for g in group_by {
            if let ast::Expr::Literal(ast::Literal::Number(n)) = g
                && let Ok(idx) = n.parse::<usize>()
            {
                if idx == 0 || idx > projection.len() {
                    return Err(SqlError::Bind(format!(
                        "GROUP BY position {idx} is out of range"
                    )));
                }
                match &projection[idx - 1] {
                    ast::SelectItem::Expr { expr, .. } => {
                        out.push(expr.clone());
                        continue;
                    }
                    _ => {
                        return Err(SqlError::Bind(
                            "GROUP BY position cannot name a wildcard".into(),
                        ));
                    }
                }
            }
            if let ast::Expr::Column(parts) = g
                && parts.len() == 1
            {
                let alias_hit = projection.iter().find_map(|it| match it {
                    ast::SelectItem::Expr {
                        expr,
                        alias: Some(a),
                    } if a == &parts[0] => Some(expr.clone()),
                    _ => None,
                });
                if let Some(expr) = alias_hit {
                    out.push(expr);
                    continue;
                }
            }
            out.push(g.clone());
        }
        Ok(out)
    }

    fn arg_types_of(
        &self,
        args: &[BExpr],
        fields: &[exec::Field],
    ) -> Result<Vec<DataType>, SqlError> {
        args.iter().map(|a| self.typeof_bexpr(a, fields)).collect()
    }

    fn typeof_bexpr(&self, e: &BExpr, fields: &[exec::Field]) -> Result<DataType, SqlError> {
        Ok(lower(e, fields)?.data_type(fields)?)
    }

    /// `scope`'s own fields, plus (in aggregate mode) the synthetic group/aggregate columns a
    /// bound sub-expression may already reference (e.g. `sum(x) + 1`): everything a `BExpr`
    /// built by `bind_expr` under this `agg` context could possibly name.
    fn type_env(&self, scope: &Scope, agg: Option<&AggContext>) -> Vec<exec::Field> {
        let mut fields = scope.fields();
        if let Some(actx) = agg {
            for (n, t) in actx.group_names.iter().zip(&actx.group_types) {
                fields.push(exec::Field {
                    name: n.clone(),
                    ty: t.clone(),
                });
            }
            for (_, lagg) in actx.collected.borrow().iter() {
                let arg_types = self
                    .arg_types_of(&lagg.args, &scope.fields())
                    .unwrap_or_default();
                // Typed when it was collected; one that failed then never reaches here.
                if let Ok(ty) = agg_result_type(&lagg.func, &arg_types) {
                    fields.push(exec::Field {
                        name: lagg.name.clone(),
                        ty,
                    });
                }
            }
        }
        fields
    }

    // ── FROM / JOIN ──────────────────────────────────────────────────────

    fn bind_from_item(
        &self,
        item: &ast::FromItem,
        ctes: &[(String, ast::Query)],
    ) -> Result<(LogicalPlan, Scope), SqlError> {
        let (mut plan, mut scope) = self.bind_table_ref(&item.table, ctes)?;
        for j in &item.joins {
            let (rplan, rscope) = self.bind_table_ref(&j.table, ctes)?;
            let mut combined_cols = scope.cols.clone();
            combined_cols.extend(rscope.cols.clone());
            let combined_scope = Scope {
                cols: combined_cols,
            };
            let lp = scope.prefixes();
            let rp = rscope.prefixes();

            let mut on_pairs: Vec<(String, String)> = Vec::new();
            let mut left_extra: Vec<BExpr> = Vec::new();
            let mut right_extra: Vec<BExpr> = Vec::new();
            let mut residual: Vec<BExpr> = Vec::new();
            for c in ast_conjuncts(&j.on) {
                let (be, _ty) = self.bind_expr(c, &combined_scope, Some(&DataType::Bool), None)?;
                let mut used = HashSet::new();
                collect_prefixes(&be, &mut used);
                let touches_left = used.iter().any(|p| lp.contains(p));
                let touches_right = used.iter().any(|p| rp.contains(p));
                match (touches_left, touches_right) {
                    (true, false) => left_extra.push(be),
                    (false, true) => right_extra.push(be),
                    _ => {
                        if let Some((a, b)) = as_equi_pair(&be) {
                            let a_prefix = prefix_of(&a);
                            let (lname, rname) = if lp.contains(&a_prefix) {
                                (a, b)
                            } else {
                                (b, a)
                            };
                            on_pairs.push((lname, rname));
                        } else {
                            residual.push(be);
                        }
                    }
                }
            }
            if on_pairs.is_empty() {
                return Err(SqlError::Bind(
                    "JOIN ON needs at least one equality between the two sides".into(),
                ));
            }
            let kind = match j.kind {
                ast::JoinKind::Inner => exec::JoinKind::Inner,
                ast::JoinKind::Left => exec::JoinKind::Left,
            };
            if kind == exec::JoinKind::Left && (!left_extra.is_empty() || !residual.is_empty()) {
                return Err(SqlError::Bind(
                    "LEFT JOIN ON supports equalities and right-side filters in v1".into(),
                ));
            }
            let new_left = if left_extra.is_empty() {
                plan
            } else {
                LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate: and_all(left_extra),
                }
            };
            let new_right = if right_extra.is_empty() {
                rplan
            } else {
                LogicalPlan::Filter {
                    input: Box::new(rplan),
                    predicate: and_all(right_extra),
                }
            };
            let mut joined_fields = new_left.fields().to_vec();
            joined_fields.extend(new_right.fields().to_vec());
            let join_plan = LogicalPlan::Join {
                left: Box::new(new_left),
                right: Box::new(new_right),
                kind,
                on: on_pairs,
                fields: joined_fields,
            };
            plan = if residual.is_empty() {
                join_plan
            } else {
                LogicalPlan::Filter {
                    input: Box::new(join_plan),
                    predicate: and_all(residual),
                }
            };
            scope = combined_scope;
        }
        Ok((plan, scope))
    }

    fn bind_table_ref(
        &self,
        tr: &ast::TableRef,
        ctes: &[(String, ast::Query)],
    ) -> Result<(LogicalPlan, Scope), SqlError> {
        match tr {
            ast::TableRef::Table { name, alias } => {
                if name.db.is_none()
                    && let Some((_, q)) = ctes.iter().rev().find(|(n, _)| n == &name.name)
                {
                    let q = q.clone();
                    let plan = self.bind_query_with(&q, ctes)?;
                    let alias_name = alias.clone().unwrap_or_else(|| name.name.clone());
                    return Ok(self.wrap_relation(plan, &alias_name));
                }
                let tn = TableName::new(
                    name.db.clone().unwrap_or_else(|| self.default_db.clone()),
                    name.name.clone(),
                );
                let alias_name = alias.clone().unwrap_or_else(|| name.name.clone());
                self.bind_base_table(&tn, &alias_name)
            }
            ast::TableRef::Subquery { query, alias } => {
                let plan = self.bind_query_with(query, ctes)?;
                Ok(self.wrap_relation(plan, alias))
            }
        }
    }

    fn bind_base_table(
        &self,
        name: &TableName,
        alias: &str,
    ) -> Result<(LogicalPlan, Scope), SqlError> {
        let fields = self
            .catalog
            .fields_of(name)
            .ok_or_else(|| SqlError::Bind(format!("table {name} does not exist")))?;
        let rel = self.next_rel();
        // A CTE is inlined per reference, so `WITH` chains can grow the plan exponentially.
        if rel >= MAX_RELATIONS {
            return Err(SqlError::Bind(format!(
                "query expands to more than {MAX_RELATIONS} table scans"
            )));
        }
        let is_companion: Vec<bool> = fields
            .iter()
            .map(|f| {
                f.name
                    .strip_suffix(crate::types::COMPANION_SUFFIX)
                    .is_some_and(|base| fields.iter().any(|g| g.name == base))
            })
            .collect();
        let scan_fields: Vec<exec::Field> = fields
            .iter()
            .map(|f| exec::Field {
                name: format!("r{rel}.{}", f.name),
                ty: f.ty.clone(),
            })
            .collect();
        let mut cols = Vec::with_capacity(fields.len());
        for (i, f) in fields.iter().enumerate() {
            let has_companion =
                !is_companion[i] && fields.iter().any(|g| g.name == companion_name(&f.name));
            cols.push(ScopeCol {
                qualifier: alias.to_string(),
                name: f.name.clone(),
                internal: scan_fields[i].name.clone(),
                ty: f.ty.clone(),
                origin: Some((name.clone(), f.name.clone())),
                has_companion,
            });
        }
        Ok((
            LogicalPlan::Scan {
                db: name.db.clone(),
                table: name.name.clone(),
                fields: scan_fields,
            },
            Scope { cols },
        ))
    }

    /// Wraps an already-bound plan (a subquery or CTE result) as one relation under `alias`,
    /// with fresh, unique internal names, so a self-join of a CTE can never collide.
    fn wrap_relation(&self, plan: LogicalPlan, alias: &str) -> (LogicalPlan, Scope) {
        let rel = self.next_rel();
        let src_fields = plan.fields().to_vec();
        let exprs: Vec<(String, BExpr)> = src_fields
            .iter()
            .map(|f| (format!("r{rel}.{}", f.name), BExpr::Column(f.name.clone())))
            .collect();
        let out_fields: Vec<exec::Field> = exprs
            .iter()
            .zip(&src_fields)
            .map(|((iname, _), f)| exec::Field {
                name: iname.clone(),
                ty: f.ty.clone(),
            })
            .collect();
        let cols = src_fields
            .iter()
            .zip(&exprs)
            .map(|(f, (iname, _))| ScopeCol {
                qualifier: alias.to_string(),
                name: f.name.clone(),
                internal: iname.clone(),
                ty: f.ty.clone(),
                origin: None,
                has_companion: false,
            })
            .collect();
        (
            LogicalPlan::Project {
                input: Box::new(plan),
                exprs,
                fields: out_fields,
            },
            Scope { cols },
        )
    }

    // ── scalar expression binding ────────────────────────────────────────

    fn resolve_scope_col<'s>(
        &self,
        scope: &'s Scope,
        parts: &[String],
    ) -> Result<&'s ScopeCol, SqlError> {
        let (qualifier, name): (Option<&str>, &str) = match parts.len() {
            0 => return Err(SqlError::Bind("empty column reference".into())),
            1 => (None, parts[0].as_str()),
            _ => (
                Some(parts[parts.len() - 2].as_str()),
                parts[parts.len() - 1].as_str(),
            ),
        };
        let matches: Vec<&ScopeCol> = scope
            .cols
            .iter()
            .filter(|c| c.name == name && qualifier.is_none_or(|q| c.qualifier == q))
            .collect();
        match matches.len() {
            0 => Err(SqlError::Bind(format!("column \"{name}\" does not exist"))),
            1 => Ok(matches[0]),
            _ => Err(SqlError::Bind(format!("column \"{name}\" is ambiguous"))),
        }
    }

    fn resolve_column_and_warn(
        &self,
        scope: &Scope,
        parts: &[String],
    ) -> Result<(BExpr, DataType), SqlError> {
        let col = self.resolve_scope_col(scope, parts)?;
        self.maybe_warn(col);
        Ok((BExpr::Column(col.internal.clone()), col.ty.clone()))
    }

    /// The companion column for a bare column reference, or `NULL::STRING` if it has none —
    /// shared by `coalesce_text(x)` and the `x::string` postfix cast. Neither warns.
    fn companion_or_null(&self, scope: &Scope, col: &ScopeCol) -> BExpr {
        let target = companion_name(&col.name);
        let prefix = prefix_of(&col.internal);
        match scope
            .cols
            .iter()
            .find(|c| c.name == target && prefix_of(&c.internal) == prefix)
        {
            Some(comp) => BExpr::Column(comp.internal.clone()),
            None => BExpr::Literal(Value::Null, DataType::String),
        }
    }

    fn bare_column<'s>(&self, e: &ast::Expr, scope: &'s Scope) -> Option<&'s ScopeCol> {
        match e {
            ast::Expr::Column(parts) => self.resolve_scope_col(scope, parts).ok(),
            _ => None,
        }
    }

    /// The workhorse: binds one AST expression against `scope`, with an optional type `hint`
    /// (drives NULL typing and number-literal adaptation, D0016) and an optional aggregate
    /// context (drives `GROUP BY`/aggregate extraction; `None` means a plain scalar context).
    fn bind_expr(
        &self,
        e: &ast::Expr,
        scope: &Scope,
        hint: Option<&DataType>,
        agg: Option<&AggContext>,
    ) -> Result<(BExpr, DataType), SqlError> {
        if let Some(actx) = agg
            && actx.aggregate_mode
            && let Some(pos) = actx.group_keys.iter().position(|g| g == e)
        {
            return Ok((
                BExpr::Column(actx.group_names[pos].clone()),
                actx.group_types[pos].clone(),
            ));
        }
        if let ast::Expr::Func {
            name,
            distinct,
            args,
            filter,
        } = e
            && let Some(actx) = agg
            && is_aggregate_name(name)
        {
            return self.bind_aggregate_call(e, name, *distinct, args, filter, scope, actx);
        }
        match e {
            ast::Expr::Literal(lit) => self.bind_literal(lit, hint),
            ast::Expr::Wildcard => Err(SqlError::Bind(
                "* is only valid as count(*)'s argument".into(),
            )),
            ast::Expr::Column(parts) => {
                if parts.len() == 1
                    && parts[0] == "current_timestamp"
                    && self.resolve_scope_col(scope, parts).is_err()
                {
                    return Ok((
                        BExpr::Literal(Value::Timestamp(self.now), DataType::Timestamp),
                        DataType::Timestamp,
                    ));
                }
                if agg.is_some_and(|a| a.aggregate_mode) {
                    return Err(SqlError::Bind(format!(
                        "column \"{}\" must appear in GROUP BY or in an aggregate",
                        parts.join(".")
                    )));
                }
                self.resolve_column_and_warn(scope, parts)
            }
            ast::Expr::BinaryOp { op, left, right } => {
                self.bind_binary_op(*op, left, right, scope, agg)
            }
            ast::Expr::UnaryOp { op, expr } => {
                let (be, ty) = self.bind_expr(expr, scope, hint, agg)?;
                match op {
                    ast::UnaryOp::Plus => Ok((be, ty)),
                    ast::UnaryOp::Neg => Ok((BExpr::Neg(Box::new(be)), ty)),
                }
            }
            ast::Expr::Not(inner) => {
                let (be, _) = self.bind_expr(inner, scope, Some(&DataType::Bool), agg)?;
                Ok((BExpr::Not(Box::new(be)), DataType::Bool))
            }
            ast::Expr::IsNull { expr, negated } => {
                let (be, _) = self.bind_expr(expr, scope, None, agg)?;
                let out = if *negated {
                    BExpr::IsNotNull(Box::new(be))
                } else {
                    BExpr::IsNull(Box::new(be))
                };
                Ok((out, DataType::Bool))
            }
            ast::Expr::IsBool {
                expr,
                value,
                negated,
            } => {
                let (be, _) = self.bind_expr(expr, scope, Some(&DataType::Bool), agg)?;
                let cond = BExpr::Cmp(
                    exec::CmpOp::Eq,
                    Box::new(be),
                    Box::new(BExpr::Literal(Value::Bool(*value), DataType::Bool)),
                );
                let case = BExpr::Case {
                    branches: vec![(cond, BExpr::Literal(Value::Bool(true), DataType::Bool))],
                    otherwise: Some(Box::new(BExpr::Literal(Value::Bool(false), DataType::Bool))),
                };
                let out = if *negated {
                    BExpr::Not(Box::new(case))
                } else {
                    case
                };
                Ok((out, DataType::Bool))
            }
            ast::Expr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let (eb, et) = self.bind_expr(expr, scope, None, agg)?;
                let (lob, lot) = self.bind_expr(low, scope, Some(&et), agg)?;
                let (hib, hit) = self.bind_expr(high, scope, Some(&et), agg)?;
                let lob = cast_towards(lob, &lot, &et);
                let hib = cast_towards(hib, &hit, &et);
                let out = BExpr::Between {
                    expr: Box::new(eb),
                    low: Box::new(lob),
                    high: Box::new(hib),
                    negated: *negated,
                };
                Ok((out, DataType::Bool))
            }
            ast::Expr::InList {
                expr,
                list,
                negated,
            } => {
                let (eb, et) = self.bind_expr(expr, scope, None, agg)?;
                let mut values = Vec::with_capacity(list.len());
                for item in list {
                    let (ib, _it) = self.bind_expr(item, scope, Some(&et), agg)?;
                    let v = fold_constant(&ib)
                        .ok_or_else(|| SqlError::Bind("IN list values must be constant".into()))?;
                    values.push(v);
                }
                let out = BExpr::InList {
                    expr: Box::new(eb),
                    list: values,
                    negated: *negated,
                };
                Ok((out, DataType::Bool))
            }
            ast::Expr::Like {
                expr,
                pattern,
                case_insensitive,
                negated,
            } => {
                let (eb, _) = self.bind_expr(expr, scope, Some(&DataType::String), agg)?;
                let ast::Expr::Literal(ast::Literal::String(p)) = pattern.as_ref() else {
                    return Err(SqlError::Bind("LIKE pattern must be a literal".into()));
                };
                let out = BExpr::Like {
                    expr: Box::new(eb),
                    pattern: p.clone(),
                    case_insensitive: *case_insensitive,
                    negated: *negated,
                };
                Ok((out, DataType::Bool))
            }
            ast::Expr::Func {
                name,
                distinct,
                args,
                filter,
            } => self.bind_scalar_func(name, *distinct, args, filter, scope, agg),
            ast::Expr::Cast {
                expr,
                type_name,
                via_cast,
            } => {
                if !via_cast
                    && type_name.name.eq_ignore_ascii_case("string")
                    && type_name.list_of.is_none()
                    && let Some(col) = self.bare_column(expr, scope)
                {
                    // A grouped `x::string` matched its GROUP BY key above; reaching here in
                    // aggregate mode means the companion is ungrouped, never a cast of `x`.
                    if agg.is_some_and(|a| a.aggregate_mode) {
                        return Err(SqlError::Bind(format!(
                            "{}::string must appear in GROUP BY or in an aggregate",
                            col.name
                        )));
                    }
                    let col = col.clone();
                    return Ok((self.companion_or_null(scope, &col), DataType::String));
                }
                let (be, _t) = self.bind_expr(expr, scope, None, agg)?;
                let target = self.map_type_name(type_name)?;
                Ok((BExpr::Cast(Box::new(be), target.clone()), target))
            }
            ast::Expr::Extract { field, expr } => {
                let part = date_part(field)
                    .ok_or_else(|| SqlError::Bind(format!("unknown EXTRACT field {field}")))?;
                let (be, _t) = self.bind_expr(expr, scope, None, agg)?;
                Ok((
                    BExpr::Func {
                        func: exec::ScalarFunc::Extract(part),
                        args: vec![be],
                    },
                    DataType::Int64,
                ))
            }
            ast::Expr::Case {
                operand,
                branches,
                otherwise,
            } => self.bind_case(
                operand.as_deref(),
                branches,
                otherwise.as_deref(),
                scope,
                agg,
            ),
            ast::Expr::Subquery(_) => Err(SqlError::Bind(
                "scalar subqueries are deferred in v1 (SPEC §8)".into(),
            )),
        }
    }

    fn bind_literal(
        &self,
        lit: &ast::Literal,
        hint: Option<&DataType>,
    ) -> Result<(BExpr, DataType), SqlError> {
        match lit {
            ast::Literal::Null => {
                let ty = hint.cloned().unwrap_or(DataType::String);
                Ok((BExpr::Literal(Value::Null, ty.clone()), ty))
            }
            ast::Literal::Number(text) => {
                if let Some(t) = hint
                    && let Some(v) = Value::from_text(text, t)
                {
                    return Ok((BExpr::Literal(v, t.clone()), t.clone()));
                }
                let (v, ty) = number_literal(text)?;
                Ok((BExpr::Literal(v, ty.clone()), ty))
            }
            ast::Literal::String(s) => Ok((
                BExpr::Literal(Value::String(s.clone()), DataType::String),
                DataType::String,
            )),
            ast::Literal::Bool(b) => Ok((
                BExpr::Literal(Value::Bool(*b), DataType::Bool),
                DataType::Bool,
            )),
            ast::Literal::Date(s) => {
                let v = Value::from_text(s, &DataType::Date)
                    .ok_or_else(|| SqlError::Bind(format!("invalid date '{s}'")))?;
                Ok((BExpr::Literal(v, DataType::Date), DataType::Date))
            }
            ast::Literal::Timestamp(s) => {
                let v = Value::from_text(s, &DataType::Timestamp)
                    .ok_or_else(|| SqlError::Bind(format!("invalid timestamp '{s}'")))?;
                Ok((BExpr::Literal(v, DataType::Timestamp), DataType::Timestamp))
            }
            ast::Literal::Interval(_) => Err(SqlError::Bind(
                "INTERVAL is only valid in time_bucket, PARTITION BY or TTL".into(),
            )),
        }
    }

    fn bind_binary_op(
        &self,
        op: ast::BinaryOp,
        left: &ast::Expr,
        right: &ast::Expr,
        scope: &Scope,
        agg: Option<&AggContext>,
    ) -> Result<(BExpr, DataType), SqlError> {
        match op {
            ast::BinaryOp::Or => {
                let (lb, _) = self.bind_expr(left, scope, Some(&DataType::Bool), agg)?;
                let (rb, _) = self.bind_expr(right, scope, Some(&DataType::Bool), agg)?;
                Ok((BExpr::Or(vec![lb, rb]), DataType::Bool))
            }
            ast::BinaryOp::And => {
                let (lb, _) = self.bind_expr(left, scope, Some(&DataType::Bool), agg)?;
                let (rb, _) = self.bind_expr(right, scope, Some(&DataType::Bool), agg)?;
                Ok((BExpr::And(vec![lb, rb]), DataType::Bool))
            }
            ast::BinaryOp::Concat => {
                let (lb, _) = self.bind_expr(left, scope, Some(&DataType::String), agg)?;
                let (rb, _) = self.bind_expr(right, scope, Some(&DataType::String), agg)?;
                Ok((
                    BExpr::Func {
                        func: exec::ScalarFunc::Concat,
                        args: vec![lb, rb],
                    },
                    DataType::String,
                ))
            }
            ast::BinaryOp::Eq
            | ast::BinaryOp::NotEq
            | ast::BinaryOp::Lt
            | ast::BinaryOp::LtEq
            | ast::BinaryOp::Gt
            | ast::BinaryOp::GtEq => {
                let (lb, _, rb, _) = self.bind_pair(left, right, scope, agg)?;
                let cmp = match op {
                    ast::BinaryOp::Eq => exec::CmpOp::Eq,
                    ast::BinaryOp::NotEq => exec::CmpOp::Ne,
                    ast::BinaryOp::Lt => exec::CmpOp::Lt,
                    ast::BinaryOp::LtEq => exec::CmpOp::Le,
                    ast::BinaryOp::Gt => exec::CmpOp::Gt,
                    ast::BinaryOp::GtEq => exec::CmpOp::Ge,
                    _ => unreachable!(),
                };
                Ok((BExpr::Cmp(cmp, Box::new(lb), Box::new(rb)), DataType::Bool))
            }
            ast::BinaryOp::Add
            | ast::BinaryOp::Sub
            | ast::BinaryOp::Mul
            | ast::BinaryOp::Div
            | ast::BinaryOp::Mod => {
                let (lb, _lt, rb, _rt) = self.bind_pair(left, right, scope, agg)?;
                let arith = match op {
                    ast::BinaryOp::Add => exec::ArithOp::Add,
                    ast::BinaryOp::Sub => exec::ArithOp::Sub,
                    ast::BinaryOp::Mul => exec::ArithOp::Mul,
                    ast::BinaryOp::Div => exec::ArithOp::Div,
                    ast::BinaryOp::Mod => exec::ArithOp::Mod,
                    _ => unreachable!(),
                };
                let node = BExpr::Arith(arith, Box::new(lb), Box::new(rb));
                let ty = self.typeof_bexpr(&node, &self.type_env(scope, agg))?;
                Ok((node, ty))
            }
        }
    }

    /// Binds a pair of operands, adapting a lone number/NULL literal to the other side's type
    /// (D0016), then promoting whichever side is still narrower (D0016).
    fn bind_pair(
        &self,
        left: &ast::Expr,
        right: &ast::Expr,
        scope: &Scope,
        agg: Option<&AggContext>,
    ) -> Result<(BExpr, DataType, BExpr, DataType), SqlError> {
        let left_adaptable = is_adaptable_literal(left);
        let right_adaptable = is_adaptable_literal(right);
        let (lb, rb) = if left_adaptable && !right_adaptable {
            let (rb, rt) = self.bind_expr(right, scope, None, agg)?;
            let (lb, _) = self.bind_expr(left, scope, Some(&rt), agg)?;
            (lb, rb)
        } else {
            let (lb, lt) = self.bind_expr(left, scope, None, agg)?;
            let (rb, _) = self.bind_expr(right, scope, Some(&lt), agg)?;
            (lb, rb)
        };
        let fields = self.type_env(scope, agg);
        let lt = self.typeof_bexpr(&lb, &fields)?;
        let rt = self.typeof_bexpr(&rb, &fields)?;
        if lt == rt {
            return Ok((lb, lt, rb, rt));
        }
        let target = unify_types(&lt, &rt);
        let lb2 = cast_towards(lb, &lt, &target);
        let rb2 = cast_towards(rb, &rt, &target);
        Ok((lb2, target.clone(), rb2, target))
    }

    fn bind_case(
        &self,
        operand: Option<&ast::Expr>,
        branches: &[(ast::Expr, ast::Expr)],
        otherwise: Option<&ast::Expr>,
        scope: &Scope,
        agg: Option<&AggContext>,
    ) -> Result<(BExpr, DataType), SqlError> {
        let mut conds = Vec::with_capacity(branches.len());
        let mut results = Vec::with_capacity(branches.len());
        let mut result_types = Vec::with_capacity(branches.len());
        let operand_bound = operand
            .map(|o| self.bind_expr(o, scope, None, agg))
            .transpose()?;
        for (c, r) in branches {
            let cond = match &operand_bound {
                Some((ob, ot)) => {
                    let (cb, _) = self.bind_expr(c, scope, Some(ot), agg)?;
                    BExpr::Cmp(exec::CmpOp::Eq, Box::new(ob.clone()), Box::new(cb))
                }
                None => self.bind_expr(c, scope, Some(&DataType::Bool), agg)?.0,
            };
            conds.push(cond);
            let (rb, rt) = self.bind_expr(r, scope, None, agg)?;
            results.push(rb);
            result_types.push(rt);
        }
        let mut target = result_types[0].clone();
        for t in &result_types[1..] {
            target = unify_types(&target, t);
        }
        let otherwise_bound = match otherwise {
            Some(o) => {
                let (ob, ot) = self.bind_expr(o, scope, Some(&target), agg)?;
                target = unify_types(&target, &ot);
                Some(cast_towards(ob, &ot, &target))
            }
            None => None,
        };
        let branches: Vec<(BExpr, BExpr)> = conds
            .into_iter()
            .zip(results.into_iter().zip(result_types))
            .map(|(c, (r, rt))| (c, cast_towards(r, &rt, &target)))
            .collect();
        Ok((
            BExpr::Case {
                branches,
                otherwise: otherwise_bound.map(Box::new),
            },
            target,
        ))
    }

    fn bind_scalar_func(
        &self,
        name: &str,
        distinct: bool,
        args: &[ast::Expr],
        filter: &Option<Box<ast::Expr>>,
        scope: &Scope,
        agg: Option<&AggContext>,
    ) -> Result<(BExpr, DataType), SqlError> {
        let lname = name.to_ascii_lowercase();
        if is_aggregate_name(&lname) {
            return Err(SqlError::Bind(format!(
                "aggregate function {lname} is not allowed here"
            )));
        }
        if distinct {
            return Err(SqlError::Bind(format!(
                "DISTINCT is only valid on an aggregate, not {lname}"
            )));
        }
        if filter.is_some() {
            return Err(SqlError::Bind(format!(
                "FILTER is only valid on an aggregate, not {lname}"
            )));
        }
        if lname == "now" {
            if !args.is_empty() {
                return Err(SqlError::Bind("now() takes no arguments".into()));
            }
            return Ok((
                BExpr::Literal(Value::Timestamp(self.now), DataType::Timestamp),
                DataType::Timestamp,
            ));
        }
        if lname == "coalesce_text" {
            if args.len() != 1 {
                return Err(SqlError::Bind("coalesce_text takes one argument".into()));
            }
            if agg.is_some_and(|a| a.aggregate_mode) {
                return Err(SqlError::Bind(
                    "coalesce_text's argument must appear in GROUP BY or in an aggregate".into(),
                ));
            }
            let Some(col) = self.bare_column(&args[0], scope) else {
                return Err(SqlError::Bind(
                    "coalesce_text's argument must be a bare column reference".into(),
                ));
            };
            let col = col.clone();
            let companion = self.companion_or_null(scope, &col);
            let func = exec::ScalarFunc::CoalesceText;
            return Ok((
                BExpr::Func {
                    func,
                    args: vec![BExpr::Column(col.internal.clone()), companion],
                },
                DataType::String,
            ));
        }
        match lname.as_str() {
            "lower" | "upper" | "length" | "char_length" => {
                if args.len() != 1 {
                    return Err(SqlError::Bind(format!("{lname} takes one argument")));
                }
                let (be, _) = self.bind_expr(&args[0], scope, Some(&DataType::String), agg)?;
                let (func, ty) = match lname.as_str() {
                    "lower" => (exec::ScalarFunc::Lower, DataType::String),
                    "upper" => (exec::ScalarFunc::Upper, DataType::String),
                    _ => (exec::ScalarFunc::Length, DataType::Int64),
                };
                Ok((
                    BExpr::Func {
                        func,
                        args: vec![be],
                    },
                    ty,
                ))
            }
            "substr" | "substring" => {
                if args.len() != 2 && args.len() != 3 {
                    return Err(SqlError::Bind("substr takes 2 or 3 arguments".into()));
                }
                let (s, _) = self.bind_expr(&args[0], scope, Some(&DataType::String), agg)?;
                let (a, _) = self.bind_expr(&args[1], scope, Some(&DataType::Int64), agg)?;
                let mut fargs = vec![s, a];
                if let Some(b) = args.get(2) {
                    let (b, _) = self.bind_expr(b, scope, Some(&DataType::Int64), agg)?;
                    fargs.push(b);
                }
                Ok((
                    BExpr::Func {
                        func: exec::ScalarFunc::Substr,
                        args: fargs,
                    },
                    DataType::String,
                ))
            }
            "regexp_match" | "regexp_matches" => {
                if args.len() != 2 {
                    return Err(SqlError::Bind(
                        "regexp_match takes (string, pattern)".into(),
                    ));
                }
                let (s, _) = self.bind_expr(&args[0], scope, Some(&DataType::String), agg)?;
                let ast::Expr::Literal(ast::Literal::String(p)) = &args[1] else {
                    return Err(SqlError::Bind(
                        "regexp_match's pattern must be a string literal".into(),
                    ));
                };
                let re =
                    exec::expr::Regex::compile(p).map_err(|e| SqlError::Plan(e.to_string()))?;
                Ok((
                    BExpr::Func {
                        func: exec::ScalarFunc::RegexpMatch(re),
                        args: vec![s],
                    },
                    DataType::Bool,
                ))
            }
            "date_trunc" => {
                if args.len() != 2 {
                    return Err(SqlError::Bind("date_trunc takes (unit, timestamp)".into()));
                }
                let ast::Expr::Literal(ast::Literal::String(unit)) = &args[0] else {
                    return Err(SqlError::Bind(
                        "date_trunc's unit must be a string literal".into(),
                    ));
                };
                let unit = trunc_unit(unit)
                    .ok_or_else(|| SqlError::Bind(format!("unknown date_trunc unit {unit}")))?;
                let (mut ts, ty) = self.bind_expr(&args[1], scope, None, agg)?;
                if ty == DataType::Date {
                    ts = BExpr::Cast(Box::new(ts), DataType::Timestamp);
                }
                Ok((
                    BExpr::Func {
                        func: exec::ScalarFunc::DateTrunc(unit),
                        args: vec![ts],
                    },
                    DataType::Timestamp,
                ))
            }
            "time_bucket" => {
                if args.len() != 2 && args.len() != 3 {
                    return Err(SqlError::Bind(
                        "time_bucket takes (interval, timestamp[, origin])".into(),
                    ));
                }
                let ast::Expr::Literal(ast::Literal::Interval(text)) = &args[0] else {
                    return Err(SqlError::Bind(
                        "time_bucket's width must be an INTERVAL literal".into(),
                    ));
                };
                let width_ns = duration_ns(parse_interval(text)?);
                let origin_ns = match args.get(2) {
                    Some(ast::Expr::Literal(ast::Literal::Timestamp(s))) => {
                        match Value::from_text(s, &DataType::Timestamp) {
                            Some(Value::Timestamp(t)) => t,
                            _ => return Err(SqlError::Bind(format!("invalid timestamp '{s}'"))),
                        }
                    }
                    Some(_) => {
                        return Err(SqlError::Bind(
                            "time_bucket's origin must be a TIMESTAMP literal".into(),
                        ));
                    }
                    None => 946_857_600_000_000_000,
                };
                let (mut ts, ty) = self.bind_expr(&args[1], scope, None, agg)?;
                if ty == DataType::Date {
                    ts = BExpr::Cast(Box::new(ts), DataType::Timestamp);
                }
                let func = exec::ScalarFunc::TimeBucket {
                    width_ns,
                    origin_ns,
                };
                Ok((
                    BExpr::Func {
                        func,
                        args: vec![ts],
                    },
                    DataType::Timestamp,
                ))
            }
            "date_part" => {
                if args.len() != 2 {
                    return Err(SqlError::Bind("date_part takes (part, expr)".into()));
                }
                let ast::Expr::Literal(ast::Literal::String(part)) = &args[0] else {
                    return Err(SqlError::Bind(
                        "date_part's part must be a string literal".into(),
                    ));
                };
                let part = date_part(part)
                    .ok_or_else(|| SqlError::Bind(format!("unknown date_part field {part}")))?;
                let (e, _) = self.bind_expr(&args[1], scope, None, agg)?;
                Ok((
                    BExpr::Func {
                        func: exec::ScalarFunc::Extract(part),
                        args: vec![e],
                    },
                    DataType::Int64,
                ))
            }
            other => Err(SqlError::Bind(format!("unknown function {other}"))),
        }
    }

    #[allow(clippy::too_many_arguments)] // one per piece of the parsed call, plus its scope
    fn bind_aggregate_call(
        &self,
        whole: &ast::Expr,
        name: &str,
        distinct: bool,
        args: &[ast::Expr],
        filter: &Option<Box<ast::Expr>>,
        scope: &Scope,
        actx: &AggContext,
    ) -> Result<(BExpr, DataType), SqlError> {
        if let Some((_, existing)) = actx.collected.borrow().iter().find(|(k, _)| k == whole) {
            let fields = scope.fields();
            let ty = agg_result_type(&existing.func, &self.arg_types_of(&existing.args, &fields)?)?;
            return Ok((BExpr::Column(existing.name.clone()), ty));
        }
        let lname = name.to_ascii_lowercase();
        let (func, bound_args): (exec::AggFunc, Vec<BExpr>) = match lname.as_str() {
            "count" if matches!(args, [ast::Expr::Wildcard]) => (exec::AggFunc::CountStar, vec![]),
            "count" => {
                if args.len() != 1 {
                    return Err(SqlError::Bind("count takes one argument".into()));
                }
                let (a, _) = self.bind_expr(&args[0], scope, None, None)?;
                (
                    if distinct {
                        exec::AggFunc::CountDistinct
                    } else {
                        exec::AggFunc::Count
                    },
                    vec![a],
                )
            }
            _ if distinct => {
                return Err(SqlError::Bind(format!(
                    "DISTINCT is not supported on {lname}"
                )));
            }
            "sum"
            | "avg"
            | "min"
            | "max"
            | "approx_count_distinct"
            | "list"
            | "list_agg"
            | "array_agg"
            | "histogram" => {
                if args.len() != 1 {
                    return Err(SqlError::Bind(format!("{lname} takes one argument")));
                }
                let (a, _) = self.bind_expr(&args[0], scope, None, None)?;
                let f = match lname.as_str() {
                    "sum" => exec::AggFunc::Sum,
                    "avg" => exec::AggFunc::Avg,
                    "min" => exec::AggFunc::Min,
                    "max" => exec::AggFunc::Max,
                    "approx_count_distinct" => exec::AggFunc::ApproxCountDistinct,
                    "histogram" => exec::AggFunc::Histogram,
                    _ => exec::AggFunc::ListAgg,
                };
                (f, vec![a])
            }
            "quantile" | "quantile_disc" | "approx_quantile" => {
                if args.len() != 2 {
                    return Err(SqlError::Bind(format!("{lname} takes (expr, quantile)")));
                }
                let (a, _) = self.bind_expr(&args[0], scope, None, None)?;
                let q = self.literal_f64_in_unit(&args[1])?;
                let f = if lname == "approx_quantile" {
                    exec::AggFunc::ApproxQuantile(q)
                } else {
                    exec::AggFunc::Quantile(q)
                };
                (f, vec![a])
            }
            "top_k" => {
                if args.len() != 2 {
                    return Err(SqlError::Bind("top_k takes (expr, k)".into()));
                }
                let (a, _) = self.bind_expr(&args[0], scope, None, None)?;
                let k = self.literal_usize(&args[1])?;
                (exec::AggFunc::TopK(k), vec![a])
            }
            "arg_min" | "min_by" | "arg_max" | "max_by" => {
                if args.len() != 2 {
                    return Err(SqlError::Bind(format!("{lname} takes (value, key)")));
                }
                let (v, _) = self.bind_expr(&args[0], scope, None, None)?;
                let (k, _) = self.bind_expr(&args[1], scope, None, None)?;
                let f = if lname == "arg_min" || lname == "min_by" {
                    exec::AggFunc::ArgMin
                } else {
                    exec::AggFunc::ArgMax
                };
                (f, vec![v, k])
            }
            other => {
                return Err(SqlError::Bind(format!(
                    "unknown aggregate function {other}"
                )));
            }
        };
        let bound_filter = match filter {
            Some(f) => Some(self.bind_expr(f, scope, Some(&DataType::Bool), None)?.0),
            None => None,
        };
        let fields = scope.fields();
        let arg_types = self.arg_types_of(&bound_args, &fields)?;
        let ty = agg_result_type(&func, &arg_types)?;
        let out_name = format!("_agg{}", actx.collected.borrow().len());
        let lagg = LAgg {
            func,
            args: bound_args,
            filter: bound_filter,
            name: out_name.clone(),
        };
        actx.collected.borrow_mut().push((whole.clone(), lagg));
        Ok((BExpr::Column(out_name), ty))
    }

    fn literal_f64_in_unit(&self, e: &ast::Expr) -> Result<f64, SqlError> {
        let (be, _) = self.bind_expr(e, &Scope::default(), Some(&DataType::Float64), None)?;
        let v = fold_constant(&be)
            .ok_or_else(|| SqlError::Bind("expected a literal quantile".into()))?;
        let q = match v {
            Value::Float64(f) => f,
            Value::Int64(i) => i as f64,
            other => {
                return Err(SqlError::Bind(format!(
                    "expected a numeric quantile, found {other:?}"
                )));
            }
        };
        if !(0.0..=1.0).contains(&q) {
            return Err(SqlError::Bind(format!(
                "quantile {q} is out of range [0, 1]"
            )));
        }
        Ok(q)
    }

    fn literal_usize(&self, e: &ast::Expr) -> Result<usize, SqlError> {
        let (be, _) = self.bind_expr(e, &Scope::default(), Some(&DataType::Int64), None)?;
        match fold_constant(&be) {
            Some(Value::Int64(n)) if n >= 0 => Ok(n as usize),
            _ => Err(SqlError::Bind(
                "expected a non-negative literal integer".into(),
            )),
        }
    }
}

// ── free helpers ─────────────────────────────────────────────────────────────

fn is_adaptable_literal(e: &ast::Expr) -> bool {
    matches!(
        e,
        ast::Expr::Literal(ast::Literal::Number(_)) | ast::Expr::Literal(ast::Literal::Null)
    )
}

/// The wider of two types under D0016's promotion table; unrelated types return `a`
/// unchanged; `exec::Expr::data_type` rejects the mismatch at plan time.
fn unify_types(a: &DataType, b: &DataType) -> DataType {
    use DataType::*;
    if a == b {
        return a.clone();
    }
    match (a, b) {
        (Int64 | UInt64, Float64) | (Float64, Int64 | UInt64) => Float64,
        (Int64 | UInt64, Decimal(dt)) | (Decimal(dt), Int64 | UInt64) => {
            let scale = dt.scale();
            let precision = (19u16 + scale as u16).min(38) as u8;
            DataType::decimal(precision.max(scale), scale).unwrap_or(Decimal(*dt))
        }
        (Decimal(_), Float64) | (Float64, Decimal(_)) => Float64,
        (Date, Timestamp) | (Timestamp, Date) => Timestamp,
        _ => a.clone(),
    }
}

/// Casts `e` (currently typed `from`) towards `to`, only when they differ; otherwise `e`
/// unchanged. Never fails: an unsupported cast surfaces at plan time as `ExecError::Plan`.
fn cast_towards(e: BExpr, from: &DataType, to: &DataType) -> BExpr {
    if from == to {
        e
    } else {
        BExpr::Cast(Box::new(e), to.clone())
    }
}

/// D0016's literal-typing table: an integer that fits `i64`/`u64`, else a `DECIMAL` sized to
/// the text's digits, or `FLOAT64` when it has an exponent.
fn number_literal(text: &str) -> Result<(Value, DataType), SqlError> {
    let bad = || SqlError::Bind(format!("invalid number literal '{text}'"));
    if text.contains(['e', 'E']) {
        let f: f64 = text.parse().map_err(|_| bad())?;
        return Ok((Value::Float64(f), DataType::Float64));
    }
    if let Some(dot) = text.find('.') {
        let int_part = &text[..dot];
        let frac_part = &text[dot + 1..];
        let sign_stripped = int_part.strip_prefix('-').unwrap_or(int_part);
        let int_digits = if sign_stripped.is_empty() || sign_stripped.bytes().all(|b| b == b'0') {
            0
        } else {
            sign_stripped.len()
        };
        let scale = (frac_part.len() as u8).min(38);
        let precision = ((int_digits + frac_part.len()).max(1) as u16).min(38) as u8;
        let scale = scale.min(precision);
        let ty = DataType::decimal(precision, scale).map_err(|_| bad())?;
        let v = Value::from_text(text, &ty).ok_or_else(bad)?;
        return Ok((v, ty));
    }
    if let Ok(i) = text.parse::<i64>() {
        return Ok((Value::Int64(i), DataType::Int64));
    }
    if let Ok(u) = text.parse::<u64>() {
        return Ok((Value::UInt64(u), DataType::UInt64));
    }
    let digits = (text.trim_start_matches('-').len() as u16).min(38) as u8;
    let ty = DataType::decimal(digits.max(1), 0).map_err(|_| bad())?;
    let v = Value::from_text(text, &ty).ok_or_else(bad)?;
    Ok((v, ty))
}

/// The fixed-width interval units (D0016); months/years are rejected by name.
pub(crate) fn parse_interval(text: &str) -> Result<Duration, SqlError> {
    let text = text.trim();
    let idx = text
        .find(|c: char| c.is_ascii_alphabetic())
        .ok_or_else(|| SqlError::Bind(format!("invalid interval '{text}'")))?;
    let (num_str, unit) = text.split_at(idx);
    let n: i64 = num_str
        .trim()
        .parse()
        .map_err(|_| SqlError::Bind(format!("invalid interval '{text}'")))?;
    if n < 0 {
        return Err(SqlError::Bind(format!(
            "invalid interval '{text}': must not be negative"
        )));
    }
    let n = n as u128;
    let ns: u128 = match unit.trim().to_ascii_lowercase().as_str() {
        "ns" => n,
        "us" => n * 1_000,
        "ms" => n * 1_000_000,
        "s" | "second" | "seconds" => n * 1_000_000_000,
        "minute" | "minutes" => n * 60_000_000_000,
        "hour" | "hours" => n * 3_600_000_000_000,
        "day" | "days" => n * 86_400_000_000_000,
        "week" | "weeks" => n * 604_800_000_000_000,
        "month" | "months" | "year" | "years" => {
            return Err(SqlError::Bind("fixed-width intervals only".into()));
        }
        other => return Err(SqlError::Bind(format!("unknown interval unit '{other}'"))),
    };
    Ok(Duration::new(
        (ns / 1_000_000_000) as u64,
        (ns % 1_000_000_000) as u32,
    ))
}

fn duration_ns(d: Duration) -> i64 {
    d.as_nanos().min(i64::MAX as u128) as i64
}

fn trunc_unit(s: &str) -> Option<exec::expr::TruncUnit> {
    use exec::expr::TruncUnit::*;
    Some(match s.to_ascii_lowercase().as_str() {
        "microsecond" => Microsecond,
        "millisecond" => Millisecond,
        "second" => Second,
        "minute" => Minute,
        "hour" => Hour,
        "day" => Day,
        "week" => Week,
        "month" => Month,
        "quarter" => Quarter,
        "year" => Year,
        _ => return None,
    })
}

fn date_part(s: &str) -> Option<exec::expr::DatePart> {
    use exec::expr::DatePart::*;
    Some(match s.to_ascii_lowercase().as_str() {
        "year" => Year,
        "quarter" => Quarter,
        "month" => Month,
        "week" => Week,
        "day" => Day,
        "dow" => DayOfWeek,
        "doy" => DayOfYear,
        "hour" => Hour,
        "minute" => Minute,
        "second" => Second,
        "millisecond" => Millisecond,
        "microsecond" => Microsecond,
        "epoch" => Epoch,
        _ => return None,
    })
}

fn output_name(expr: &ast::Expr, alias: Option<&str>) -> String {
    if let Some(a) = alias {
        return a.to_string();
    }
    match expr {
        ast::Expr::Column(parts) => parts
            .last()
            .cloned()
            .unwrap_or_else(|| "?column?".to_string()),
        ast::Expr::Func { name, .. } => name.to_ascii_lowercase(),
        _ => "?column?".to_string(),
    }
}

/// Case-sensitive dedupe by appending `_1`, `_2`, … (`Batch::new` rejects
/// duplicate field names).
fn dedupe_output_names(items: Vec<NamedExpr>) -> Vec<NamedExpr> {
    // A generated `<name>_<n>` may itself be another item's literal name, so reserve those first.
    let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
    let literal: std::collections::HashSet<String> = items.iter().map(|(n, _)| n.clone()).collect();
    items
        .into_iter()
        .map(|(name, e)| {
            let mut out_name = name.clone();
            let mut n = 1;
            while taken.contains(&out_name) || (out_name != name && literal.contains(&out_name)) {
                out_name = format!("{name}_{n}");
                n += 1;
            }
            taken.insert(out_name.clone());
            (out_name, e)
        })
        .collect()
}

fn contains_agg(e: &ast::Expr) -> bool {
    match e {
        ast::Expr::Func {
            name, args, filter, ..
        } => {
            is_aggregate_name(name)
                || args.iter().any(contains_agg)
                || filter.as_deref().is_some_and(contains_agg)
        }
        ast::Expr::BinaryOp { left, right, .. } => contains_agg(left) || contains_agg(right),
        ast::Expr::UnaryOp { expr, .. }
        | ast::Expr::Not(expr)
        | ast::Expr::IsNull { expr, .. }
        | ast::Expr::IsBool { expr, .. } => contains_agg(expr),
        ast::Expr::Between {
            expr, low, high, ..
        } => contains_agg(expr) || contains_agg(low) || contains_agg(high),
        ast::Expr::InList { expr, list, .. } => contains_agg(expr) || list.iter().any(contains_agg),
        ast::Expr::Like { expr, pattern, .. } => contains_agg(expr) || contains_agg(pattern),
        ast::Expr::Cast { expr, .. } | ast::Expr::Extract { expr, .. } => contains_agg(expr),
        ast::Expr::Case {
            operand,
            branches,
            otherwise,
        } => {
            operand.as_deref().is_some_and(contains_agg)
                || branches
                    .iter()
                    .any(|(c, r)| contains_agg(c) || contains_agg(r))
                || otherwise.as_deref().is_some_and(contains_agg)
        }
        _ => false,
    }
}

fn agg_result_type(func: &exec::AggFunc, arg_types: &[DataType]) -> Result<DataType, SqlError> {
    exec::agg_output_type(func, arg_types).map_err(|e| SqlError::Bind(format!("{func:?}: {e}")))
}

fn ast_conjuncts(e: &ast::Expr) -> Vec<&ast::Expr> {
    let mut out = Vec::new();
    fn walk<'a>(e: &'a ast::Expr, out: &mut Vec<&'a ast::Expr>) {
        if let ast::Expr::BinaryOp {
            op: ast::BinaryOp::And,
            left,
            right,
        } = e
        {
            walk(left, out);
            walk(right, out);
        } else {
            out.push(e);
        }
    }
    walk(e, &mut out);
    out
}

fn and_all(mut exprs: Vec<BExpr>) -> BExpr {
    if exprs.len() == 1 {
        exprs.remove(0)
    } else {
        BExpr::And(exprs)
    }
}

pub(crate) fn collect_prefixes(e: &BExpr, out: &mut HashSet<String>) {
    match e {
        BExpr::Column(name) => {
            out.insert(prefix_of(name));
        }
        BExpr::Literal(..) => {}
        BExpr::Cmp(_, l, r) | BExpr::Arith(_, l, r) => {
            collect_prefixes(l, out);
            collect_prefixes(r, out);
        }
        BExpr::And(parts) | BExpr::Or(parts) => parts.iter().for_each(|p| collect_prefixes(p, out)),
        BExpr::Not(e) | BExpr::IsNull(e) | BExpr::IsNotNull(e) | BExpr::Neg(e) => {
            collect_prefixes(e, out)
        }
        BExpr::InList { expr, .. } => collect_prefixes(expr, out),
        BExpr::Between {
            expr, low, high, ..
        } => {
            collect_prefixes(expr, out);
            collect_prefixes(low, out);
            collect_prefixes(high, out);
        }
        BExpr::Like { expr, .. } => collect_prefixes(expr, out),
        BExpr::Case {
            branches,
            otherwise,
        } => {
            branches.iter().for_each(|(c, r)| {
                collect_prefixes(c, out);
                collect_prefixes(r, out);
            });
            if let Some(o) = otherwise {
                collect_prefixes(o, out);
            }
        }
        BExpr::Cast(e, _) => collect_prefixes(e, out),
        BExpr::Func { args, .. } => args.iter().for_each(|a| collect_prefixes(a, out)),
    }
}

/// Every internal column name a `BExpr` reads, for the planner's projection-pruning pass.
pub(crate) fn expr_columns(e: &BExpr, out: &mut HashSet<String>) {
    match e {
        BExpr::Column(name) => {
            out.insert(name.clone());
        }
        BExpr::Literal(..) => {}
        BExpr::Cmp(_, l, r) | BExpr::Arith(_, l, r) => {
            expr_columns(l, out);
            expr_columns(r, out);
        }
        BExpr::And(parts) | BExpr::Or(parts) => parts.iter().for_each(|p| expr_columns(p, out)),
        BExpr::Not(e) | BExpr::IsNull(e) | BExpr::IsNotNull(e) | BExpr::Neg(e) => {
            expr_columns(e, out)
        }
        BExpr::InList { expr, .. } => expr_columns(expr, out),
        BExpr::Between {
            expr, low, high, ..
        } => {
            expr_columns(expr, out);
            expr_columns(low, out);
            expr_columns(high, out);
        }
        BExpr::Like { expr, .. } => expr_columns(expr, out),
        BExpr::Case {
            branches,
            otherwise,
        } => {
            branches.iter().for_each(|(c, r)| {
                expr_columns(c, out);
                expr_columns(r, out);
            });
            if let Some(o) = otherwise {
                expr_columns(o, out);
            }
        }
        BExpr::Cast(e, _) => expr_columns(e, out),
        BExpr::Func { args, .. } => args.iter().for_each(|a| expr_columns(a, out)),
    }
}

fn as_equi_pair(e: &BExpr) -> Option<(String, String)> {
    if let BExpr::Cmp(exec::CmpOp::Eq, l, r) = e
        && let (BExpr::Column(a), BExpr::Column(b)) = (l.as_ref(), r.as_ref())
    {
        return Some((a.clone(), b.clone()));
    }
    None
}

fn manifest_cmp(op: ast::BinaryOp) -> Option<manifest::CmpOp> {
    use manifest::CmpOp as M;
    Some(match op {
        ast::BinaryOp::Eq => M::Eq,
        ast::BinaryOp::NotEq => M::Ne,
        ast::BinaryOp::Lt => M::Lt,
        ast::BinaryOp::LtEq => M::Le,
        ast::BinaryOp::Gt => M::Gt,
        ast::BinaryOp::GtEq => M::Ge,
        _ => return None,
    })
}

fn flip_manifest_cmp(op: manifest::CmpOp) -> manifest::CmpOp {
    use manifest::CmpOp::*;
    match op {
        Lt => Gt,
        Gt => Lt,
        Le => Ge,
        Ge => Le,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeCatalog(Vec<(TableName, Vec<exec::Field>)>);

    impl Catalog for FakeCatalog {
        fn fields_of(&self, name: &TableName) -> Option<Vec<exec::Field>> {
            self.0
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, f)| f.clone())
        }
    }

    fn f(name: &str, ty: DataType) -> exec::Field {
        exec::Field {
            name: name.to_string(),
            ty,
        }
    }

    fn catalog_with_companion() -> FakeCatalog {
        FakeCatalog(vec![(
            TableName::new("main", "t"),
            vec![
                f("x", DataType::Int64),
                f("x::string", DataType::String),
                f("y", DataType::Int64),
            ],
        )])
    }

    fn binder(cat: &FakeCatalog) -> Binder<'_> {
        Binder::new(cat, "main".to_string(), Some(0))
    }

    fn parse_one(sql: &str) -> ast::Query {
        let stmts = super::super::parser::parse(sql).unwrap();
        match stmts.into_iter().next().unwrap() {
            ast::Statement::Query(q) => q,
            other => panic!("expected a query, got {other:?}"),
        }
    }

    #[test]
    fn companion_cast_resolves_without_warning_when_present() {
        let cat = catalog_with_companion();
        let b = binder(&cat);
        let q = parse_one("SELECT x::string FROM t");
        let (_plan, warnings) = b.bind_query(&q).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn companion_cast_is_null_when_absent() {
        let cat = catalog_with_companion();
        let b = binder(&cat);
        let q = parse_one("SELECT y::string FROM t");
        let (plan, warnings) = b.bind_query(&q).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(plan.fields()[0].ty, DataType::String);
    }

    #[test]
    fn reading_the_base_column_warns_once_per_column() {
        let cat = catalog_with_companion();
        let b = binder(&cat);
        let q = parse_one("SELECT x, x FROM t WHERE x > 0");
        let (_plan, warnings) = b.bind_query(&q).unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("main.t.x"));
    }

    #[test]
    fn coalesce_text_does_not_warn() {
        let cat = catalog_with_companion();
        let b = binder(&cat);
        let q = parse_one("SELECT coalesce_text(x) FROM t");
        let (_plan, warnings) = b.bind_query(&q).unwrap();
        assert!(warnings.is_empty());
    }

    #[test]
    fn ambiguous_unqualified_column_is_bind() {
        let cat = FakeCatalog(vec![
            (TableName::new("main", "a"), vec![f("id", DataType::Int64)]),
            (TableName::new("main", "b"), vec![f("id", DataType::Int64)]),
        ]);
        let b = binder(&cat);
        let q = parse_one("SELECT id FROM a JOIN b ON a.id = b.id");
        let err = b.bind_query(&q).unwrap_err();
        assert!(matches!(err, SqlError::Bind(msg) if msg.contains("ambiguous")));
    }

    #[test]
    fn group_by_error_names_the_column() {
        let cat = catalog_with_companion();
        let b = binder(&cat);
        let q = parse_one("SELECT x, sum(y) FROM t GROUP BY y");
        let err = b.bind_query(&q).unwrap_err();
        assert!(
            matches!(err, SqlError::Bind(ref msg) if msg.contains("GROUP BY")),
            "{err}"
        );
    }

    #[test]
    fn output_name_dedupe_appends_suffix() {
        let cat = catalog_with_companion();
        let b = binder(&cat);
        let q = parse_one("SELECT x, x AS x FROM t");
        let (plan, _) = b.bind_query(&q).unwrap();
        let names: Vec<&str> = plan.fields().iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["x", "x_1"]);
    }

    #[test]
    fn literal_adaptation_types_a_float_literal_against_a_float_column() {
        let cat = FakeCatalog(vec![(
            TableName::new("main", "t"),
            vec![f("x", DataType::Float64)],
        )]);
        let b = binder(&cat);
        let q = parse_one("SELECT * FROM t WHERE x > 1.5");
        b.bind_query(&q).unwrap();
    }

    #[test]
    fn now_is_folded_once_at_bind_time() {
        let cat = catalog_with_companion();
        let b = binder(&cat);
        let q = parse_one("SELECT now() = now()");
        let (plan, _) = b.bind_query(&q).unwrap();
        assert_eq!(plan.fields().len(), 1);
    }
}
