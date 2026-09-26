//! The SQL abstract syntax tree the parser (`parser.rs`) builds (SPEC §8). Every node
//! documents the grammar production it came from; the binder is the only place
//! that decides what a node *means* — this module stays a plain, exhaustive shape.

/// `object_name := ident ['.' ident]`. `db` is `None` for an unqualified name; the binder
/// resolves that to the default database (root contract C5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectName {
    pub db: Option<String>,
    pub name: String,
}

/// `statements := statement (';' statement)* ';'?`. `parse` returns the statement list.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Query(Query),
    CreateTable(CreateTable),
    CreateSchema(CreateSchema),
    DropTable(DropTable),
    UndropTable(ObjectName),
    Truncate(ObjectName),
    Insert(Insert),
    Copy(CopyStatement),
    Delete(Delete),
}

/// `query := [WITH cte (',' cte)*] set_expr [ORDER BY ...] [LIMIT ...] [OFFSET int]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub ctes: Vec<Cte>,
    pub body: SetExpr,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<Limit>,
    pub offset: Option<i64>,
}

/// `cte := ident AS '(' query ')'`.
#[derive(Debug, Clone, PartialEq)]
pub struct Cte {
    pub name: String,
    pub query: Box<Query>,
}

/// `LIMIT (int | ALL)`.
#[derive(Debug, Clone, PartialEq)]
pub enum Limit {
    All,
    Count(i64),
}

/// `set_expr := select (UNION ALL select)*`. One element is a plain `select`; more than one
/// is a `UNION ALL` chain, left to right. Plain `UNION` is a `ParseError` at parse time.
#[derive(Debug, Clone, PartialEq)]
pub struct SetExpr {
    pub selects: Vec<SelectBody>,
}

/// `select := SELECT ... | '(' query ')'`, the two alternatives of one `select`.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectBody {
    Select(Box<Select>),
    Query(Box<Query>),
}

/// The `SELECT ...` alternative of `select`.
#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub distinct: bool,
    pub projection: Vec<SelectItem>,
    pub from: Vec<FromItem>,
    pub selection: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
}

/// `select_item := '*' | ident '.' '*' | expr [[AS] ident]`.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Wildcard,
    QualifiedWildcard(String),
    Expr { expr: Expr, alias: Option<String> },
}

/// `from_item := table_ref (join)*`.
#[derive(Debug, Clone, PartialEq)]
pub struct FromItem {
    pub table: TableRef,
    pub joins: Vec<Join>,
}

/// `table_ref := object_name [[AS] ident] | '(' query ')' [AS] ident`. A derived table's
/// alias is mandatory (the grammar puts `ident` outside the brackets there).
#[derive(Debug, Clone, PartialEq)]
pub enum TableRef {
    Table {
        name: ObjectName,
        alias: Option<String>,
    },
    Subquery {
        query: Box<Query>,
        alias: String,
    },
}

/// `join := [INNER] JOIN table_ref ON expr | LEFT [OUTER] JOIN table_ref ON expr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub kind: JoinKind,
    pub table: TableRef,
    pub on: Expr,
}

/// `order_item := expr [ASC | DESC] [NULLS (FIRST | LAST)]`.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    pub expr: Expr,
    pub desc: bool,
    pub nulls: Option<NullsOrder>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullsOrder {
    First,
    Last,
}

/// `create_table := CREATE TABLE [IF NOT EXISTS] object_name '(' column_def (',' column_def)*
/// ')' table_clause*`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    pub if_not_exists: bool,
    pub name: ObjectName,
    pub columns: Vec<ColumnDef>,
    pub clauses: Vec<TableClause>,
}

/// `column_def := ident type_name [NOT NULL | NULL]`. Nullability is parsed and recorded;
/// the binder may ignore it (root contract note).
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub type_name: TypeName,
    /// `Some(false)` for `NOT NULL`, `Some(true)` for `NULL`, `None` when unspecified.
    pub nullable: Option<bool>,
}

/// `type_name := ident ['(' int (',' int)* ')'] ('[' ']')*`, plus the `LIST<T>` special
/// case. `name` keeps the source text as written; the binder maps it to a `DataType`.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeName {
    pub name: String,
    pub params: Vec<i64>,
    pub array_dims: usize,
    /// `LIST<T>`'s element type, when `name` was written that way.
    pub list_of: Option<Box<TypeName>>,
}

/// `table_clause := ENGINE '=' ident | KEY '(' ident_list ')' | VERSION '(' ident ')'
/// | ORDER BY '(' ident_list ')' | ORDER BY ident_list | PARTITION BY ident '(' string ')'
/// | TTL ident '+' interval_lit | WITH '(' ident '=' literal (',' ident '=' literal)* ')'`.
#[derive(Debug, Clone, PartialEq)]
pub enum TableClause {
    Engine(String),
    Key(Vec<String>),
    Version(String),
    OrderBy(Vec<String>),
    /// The bucket string is a duration like `'1 day'`; the binder parses it.
    PartitionBy {
        column: String,
        bucket: String,
    },
    /// The interval text from `INTERVAL '...'`.
    Ttl {
        column: String,
        interval: String,
    },
    With(Vec<(String, Literal)>),
}

/// `create_schema := CREATE SCHEMA [IF NOT EXISTS] ident`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateSchema {
    pub if_not_exists: bool,
    pub name: String,
}

/// `drop_table := DROP TABLE [IF EXISTS] object_name`.
#[derive(Debug, Clone, PartialEq)]
pub struct DropTable {
    pub if_exists: bool,
    pub name: ObjectName,
}

/// `insert := INSERT INTO object_name ['(' ident_list ')'] (VALUES row (',' row)* | query)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    pub table: ObjectName,
    pub columns: Option<Vec<String>>,
    pub source: InsertSource,
}

/// `row := '(' expr (',' expr)* ')'`.
#[derive(Debug, Clone, PartialEq)]
pub enum InsertSource {
    Values(Vec<Vec<Expr>>),
    Query(Box<Query>),
}

/// `copy := COPY object_name ['(' ident_list ')'] FROM string
/// ['(' copy_opt (',' copy_opt)* ')' | [WITH] copy_opt*]`. Named `CopyStatement`, not `Copy`,
/// so it never shadows `std::marker::Copy` in a module that globs this one in.
#[derive(Debug, Clone, PartialEq)]
pub struct CopyStatement {
    pub table: ObjectName,
    pub columns: Option<Vec<String>>,
    pub source: String,
    pub options: Vec<CopyOption>,
}

/// `copy_opt := FORMAT ident | HEADER [bool_lit] | DELIMITER string`.
#[derive(Debug, Clone, PartialEq)]
pub enum CopyOption {
    Format(String),
    Header(Option<bool>),
    Delimiter(String),
}

/// `delete := DELETE FROM object_name [WHERE expr]`. The binder restricts `expr` to an AND
/// of `column op literal`; the parser accepts the general grammar.
#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    pub table: ObjectName,
    pub selection: Option<Expr>,
}

/// `literal := number | string | TRUE | FALSE | NULL | (DATE | TIMESTAMP | INTERVAL) string`.
/// `Number` keeps the source text (root D0016): the binder types it by context.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Number(String),
    String(String),
    Bool(bool),
    Null,
    Date(String),
    Timestamp(String),
    Interval(String),
}

/// Binary operators over the `||`, `+ - * /  %`, and comparison precedence levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    And,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Concat,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

/// Unary `-` and `+`, at the precedence level between `* / %` and `::`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Plus,
}

/// `expr`, precedence-climbed from `OR` (loosest) to `primary` (tightest); see
/// `parser.rs`'s module doc for the full chain. The binder gives every node its type.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Literal),
    /// A dotted name (`col`, `t.col` or `db.t.col`), from `object_name ['.' ident]`.
    Column(Vec<String>),
    /// Bare `*`, only valid as `count(*)`'s sole argument.
    Wildcard,
    BinaryOp {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    UnaryOp {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    /// `NOT expr`, at its own precedence level (looser than comparison, tighter than AND).
    Not(Box<Expr>),
    /// `expr IS [NOT] NULL`.
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    /// `expr IS [NOT] (TRUE | FALSE)`.
    IsBool {
        expr: Box<Expr>,
        value: bool,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    /// `[NOT] IN (list)`. The list holds general expressions; the binder requires them to be
    /// constants (root contract C3: the executor's `InList` holds `Vec<Value>`).
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    /// `[NOT] (LIKE | ILIKE) pattern`.
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        case_insensitive: bool,
        negated: bool,
    },
    /// `func_name '(' [DISTINCT] [args] ')' [FILTER '(' WHERE expr ')']`.
    Func {
        name: String,
        distinct: bool,
        args: Vec<Expr>,
        filter: Option<Box<Expr>>,
    },
    /// `CAST '(' expr AS type_name ')'` and the postfix `expr '::' type_name`; both produce
    /// this node (root decision: `x::string`'s meaning is the binder's call, not the
    /// parser's). `via_cast` distinguishes them: only the `::` postfix on a bare column
    /// triggers the companion-routing rule, never `CAST(...)` (root decision table).
    Cast {
        expr: Box<Expr>,
        type_name: TypeName,
        via_cast: bool,
    },
    /// `EXTRACT '(' ident FROM expr ')'`. `field` keeps the ident's (lowercased) text.
    Extract {
        field: String,
        expr: Box<Expr>,
    },
    /// Simple (`operand` set) and searched (`operand` `None`) `CASE`.
    Case {
        operand: Option<Box<Expr>>,
        branches: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
    /// A parenthesised scalar subquery, `'(' query ')'`. Parses; the binder rejects it
    /// ("deferred in v1", root contract "Out of scope").
    Subquery(Box<Query>),
}
