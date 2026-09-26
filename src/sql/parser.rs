//! A hand-rolled recursive-descent parser over the lexer's tokens (SPEC §8), producing
//! `ast::Statement`s. The binder (group 2) is the only place that decides what a statement
//! means; this module is exhaustive over the grammar's syntax and nothing more.
//!
//! `expr`'s precedence, loosest to tightest:
//! `OR < AND < NOT < IS [NOT] NULL/TRUE/FALSE < comparison < [NOT] BETWEEN/IN/LIKE/ILIKE
//! < || < + - < * / % < unary - + < '::' (postfix, repeatable) < primary`.
//!
//! **Depth cap (the Stable commitment).** One `Cell<usize>` counter, guarded by a small RAII
//! type so every return path (including `?`) decrements it. A guard sits at `expr`'s entry
//! (so every nested expression — a parenthesised primary, a subquery, a function argument, a
//! CASE branch, a BETWEEN bound, an IN element — counts once), at the unary and `NOT` chains
//! (which recurse into themselves without a fresh `expr` entry), and at `query` and `select`.
//! Past `MAX_DEPTH` the parser returns a `ParseError` instead of recursing further, so hostile
//! input (100,000 nested parens or unary minuses) errors instead of overflowing the stack.

use std::cell::Cell;
use std::fmt;

use super::ast::*;
use super::lexer::{self, Token};

/// Nested expressions, subqueries and parentheses combined may not exceed this.
pub const MAX_DEPTH: usize = 128;

/// A parse failure. `Display` renders `"line L, column C: message"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
    pub line: usize,
    pub column: usize,
}

impl ParseError {
    pub(crate) fn new(source: &str, offset: usize, message: impl Into<String>) -> ParseError {
        let (line, column) = line_col(source, offset);
        ParseError {
            message: message.into(),
            offset,
            line,
            column,
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "line {}, column {}: {}",
            self.line, self.column, self.message
        )
    }
}

impl std::error::Error for ParseError {}

/// The 1-based line and column of a byte offset into `source`.
pub(crate) fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    let mut line = 1usize;
    let mut last_newline = None;
    for (i, b) in source.as_bytes()[..offset].iter().enumerate() {
        if *b == b'\n' {
            line += 1;
            last_newline = Some(i);
        }
    }
    let column = match last_newline {
        Some(nl) => offset - nl,
        None => offset + 1,
    };
    (line, column)
}

/// Parses `sql` into every statement it contains (`statements := statement (';' statement)*
/// ';'?`). Hostile input (unterminated tokens, nesting past `MAX_DEPTH`, unknown syntax)
/// returns `ParseError` rather than panicking or recursing without bound.
pub fn parse(sql: &str) -> Result<Vec<Statement>, ParseError> {
    on_sql_stack(|| parse_here(sql))
}

/// The stack every recursive SQL phase (parse, bind, plan) runs on. `MAX_DEPTH` levels cost
/// ~13 frames each and debug frames are large, so the caller's thread (2 MiB for a test) is
/// not enough; a fixed stack makes the cap a real bound wherever the call comes from.
const SQL_STACK_BYTES: usize = 16 << 20;

/// Runs `f` on a scoped thread with `SQL_STACK_BYTES` of stack; inline if none can be spawned.
pub(crate) fn on_sql_stack<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    let slot = std::sync::Mutex::new(Some(f));
    let take = || slot.lock().unwrap_or_else(|p| p.into_inner()).take();
    std::thread::scope(|s| {
        let spawned = std::thread::Builder::new()
            .name("adelie-sql".to_string())
            .stack_size(SQL_STACK_BYTES)
            .spawn_scoped(s, || take().map(|f| f()));
        let ran = match spawned {
            Ok(handle) => handle
                .join()
                .unwrap_or_else(|p| std::panic::resume_unwind(p)),
            Err(_) => None,
        };
        ran.unwrap_or_else(|| take().expect("closure not yet run")())
    })
}

/// `parse` on the current thread, for callers already on the SQL stack.
pub(crate) fn parse_here(sql: &str) -> Result<Vec<Statement>, ParseError> {
    let tokens = lexer::tokenize(sql)?;
    let parser = Parser {
        source: sql,
        tokens,
        pos: Cell::new(0),
        depth: Cell::new(0),
    };
    parser.parse_statements()
}

/// Keywords that must not be swallowed as an implicit (`AS`-less) table alias: the ones that
/// can legally follow a `table_ref` in `from_item`/`join`.
const TABLE_ALIAS_STOP: &[&str] = &[
    "where", "group", "having", "order", "limit", "offset", "union", "join", "inner", "left",
    "right", "full", "cross", "natural", "using", "on",
];

/// Keywords that can follow a select item, so none of them is an implicit alias: `FROM`, plus
/// every clause a FROM-less select may go straight on to (`SELECT 1 UNION ALL SELECT 2`).
const SELECT_ALIAS_STOP: &[&str] = &[
    "from", "where", "group", "having", "order", "limit", "offset", "union", "into",
];

/// `pos` and `depth` are `Cell`s (not `&mut self`) so a `DepthGuard` borrowing `&self` can
/// stay alive across the recursive-descent calls that follow it in the same expression.
struct Parser<'a> {
    source: &'a str,
    tokens: Vec<lexer::SpannedToken>,
    pos: Cell<usize>,
    depth: Cell<usize>,
}

/// Holds the parser's depth counter incremented for its lifetime; every return path
/// (including an early `?`) decrements it via `Drop`, so the counter never leaks.
struct DepthGuard<'a> {
    depth: &'a Cell<usize>,
}

impl<'a> DepthGuard<'a> {
    fn enter(
        depth: &'a Cell<usize>,
        source: &str,
        offset: usize,
    ) -> Result<DepthGuard<'a>, ParseError> {
        let next = depth.get() + 1;
        if next > MAX_DEPTH {
            return Err(ParseError::new(
                source,
                offset,
                format!("nesting deeper than {MAX_DEPTH}"),
            ));
        }
        depth.set(next);
        Ok(DepthGuard { depth })
    }
}

impl Drop for DepthGuard<'_> {
    fn drop(&mut self) {
        self.depth.set(self.depth.get() - 1);
    }
}

impl<'a> Parser<'a> {
    fn enter_depth(&self) -> Result<DepthGuard<'_>, ParseError> {
        DepthGuard::enter(&self.depth, self.source, self.peek_offset())
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.pos.get()].token
    }

    fn nth(&self, n: usize) -> &Token {
        let i = (self.pos.get() + n).min(self.tokens.len() - 1);
        &self.tokens[i].token
    }

    fn peek_offset(&self) -> usize {
        self.tokens[self.pos.get()].offset
    }

    fn is_eof(&self) -> bool {
        matches!(self.peek(), Token::Eof)
    }

    fn advance(&self) -> Token {
        let i = self.pos.get();
        let tok = self.tokens[i].token.clone();
        if i + 1 < self.tokens.len() {
            self.pos.set(i + 1);
        }
        tok
    }

    fn eat(&self, tok: &Token) -> bool {
        if self.peek() == tok {
            self.advance();
            true
        } else {
            false
        }
    }

    fn peek_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Token::Ident(s) if s == kw)
    }

    fn nth_kw(&self, n: usize, kw: &str) -> bool {
        matches!(self.nth(n), Token::Ident(s) if s == kw)
    }

    fn eat_kw(&self, kw: &str) -> bool {
        if self.peek_kw(kw) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&self, tok: Token) -> Result<(), ParseError> {
        if self.eat(&tok) {
            Ok(())
        } else {
            Err(self.err(format!("expected {:?}, found {:?}", tok, self.peek())))
        }
    }

    fn expect_kw(&self, kw: &str) -> Result<(), ParseError> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.err(format!(
                "expected '{}', found {:?}",
                kw.to_uppercase(),
                self.peek()
            )))
        }
    }

    fn err(&self, msg: impl Into<String>) -> ParseError {
        ParseError::new(self.source, self.peek_offset(), msg)
    }

    fn err_at(&self, offset: usize, msg: impl Into<String>) -> ParseError {
        ParseError::new(self.source, offset, msg)
    }

    fn ident(&self) -> Result<String, ParseError> {
        let offset = self.peek_offset();
        match self.advance() {
            Token::Ident(s) => Ok(s),
            Token::QuotedIdent(s) => Ok(s),
            other => Err(self.err_at(offset, format!("expected an identifier, found {other:?}"))),
        }
    }

    fn string_lit(&self) -> Result<String, ParseError> {
        let offset = self.peek_offset();
        match self.advance() {
            Token::Str(s) => Ok(s),
            other => Err(self.err_at(
                offset,
                format!("expected a string literal, found {other:?}"),
            )),
        }
    }

    fn expect_int(&self) -> Result<i64, ParseError> {
        let offset = self.peek_offset();
        match self.advance() {
            Token::Number(text) if !text.contains(['.', 'e', 'E']) => text
                .parse::<i64>()
                .map_err(|_| self.err_at(offset, format!("integer out of range: '{text}'"))),
            other => Err(self.err_at(offset, format!("expected an integer, found {other:?}"))),
        }
    }

    fn object_name(&self) -> Result<ObjectName, ParseError> {
        let first = self.ident()?;
        if self.eat(&Token::Dot) {
            let second = self.ident()?;
            Ok(ObjectName {
                db: Some(first),
                name: second,
            })
        } else {
            Ok(ObjectName {
                db: None,
                name: first,
            })
        }
    }

    fn ident_list(&self) -> Result<Vec<String>, ParseError> {
        let mut out = vec![self.ident()?];
        while self.eat(&Token::Comma) {
            out.push(self.ident()?);
        }
        Ok(out)
    }

    fn eat_if_not_exists(&self) -> Result<bool, ParseError> {
        if self.eat_kw("if") {
            self.expect_kw("not")?;
            self.expect_kw("exists")?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// `type_name := ident ['(' int (',' int)* ')'] ('[' ']')*`, plus `LIST<T>`.
    fn type_name(&self) -> Result<TypeName, ParseError> {
        let name = self.ident()?;
        if name.eq_ignore_ascii_case("list") && matches!(self.peek(), Token::Lt) {
            self.advance();
            let inner = self.type_name()?;
            self.expect(Token::Gt)?;
            return Ok(TypeName {
                name,
                params: vec![],
                array_dims: 0,
                list_of: Some(Box::new(inner)),
            });
        }
        let mut params = vec![];
        if self.eat(&Token::LParen) {
            params.push(self.expect_int()?);
            while self.eat(&Token::Comma) {
                params.push(self.expect_int()?);
            }
            self.expect(Token::RParen)?;
        }
        let mut array_dims = 0usize;
        while self.eat(&Token::LBracket) {
            self.expect(Token::RBracket)?;
            array_dims += 1;
        }
        Ok(TypeName {
            name,
            params,
            array_dims,
            list_of: None,
        })
    }

    fn literal(&self) -> Result<Literal, ParseError> {
        if self.eat_kw("true") {
            return Ok(Literal::Bool(true));
        }
        if self.eat_kw("false") {
            return Ok(Literal::Bool(false));
        }
        if self.eat_kw("null") {
            return Ok(Literal::Null);
        }
        if self.eat_kw("date") {
            return Ok(Literal::Date(self.string_lit()?));
        }
        if self.eat_kw("timestamp") {
            return Ok(Literal::Timestamp(self.string_lit()?));
        }
        if self.eat_kw("interval") {
            return Ok(Literal::Interval(self.string_lit()?));
        }
        let offset = self.peek_offset();
        match self.advance() {
            Token::Number(text) => Ok(Literal::Number(text)),
            Token::Str(text) => Ok(Literal::String(text)),
            other => Err(self.err_at(offset, format!("expected a literal, found {other:?}"))),
        }
    }

    // ── statements ───────────────────────────────────────────────────────

    fn parse_statements(&self) -> Result<Vec<Statement>, ParseError> {
        if self.is_eof() {
            return Ok(Vec::new());
        }
        let mut out = vec![self.statement()?];
        while self.eat(&Token::Semicolon) {
            if self.is_eof() {
                break;
            }
            out.push(self.statement()?);
        }
        if !self.is_eof() {
            return Err(self.err(format!(
                "expected ';' or end of input, found {:?}",
                self.peek()
            )));
        }
        Ok(out)
    }

    fn statement(&self) -> Result<Statement, ParseError> {
        if self.peek_kw("create") {
            if self.nth_kw(1, "table") {
                return self.create_table().map(Statement::CreateTable);
            }
            if self.nth_kw(1, "schema") {
                return self.create_schema().map(Statement::CreateSchema);
            }
            return Err(self.err_at(
                self.tokens[self.pos.get() + 1].offset,
                "expected TABLE or SCHEMA after CREATE",
            ));
        }
        if self.peek_kw("drop") {
            return self.drop_table().map(Statement::DropTable);
        }
        if self.peek_kw("undrop") {
            return self.undrop_table().map(Statement::UndropTable);
        }
        if self.peek_kw("truncate") {
            return self.truncate().map(Statement::Truncate);
        }
        if self.peek_kw("insert") {
            return self.insert().map(Statement::Insert);
        }
        if self.peek_kw("copy") {
            return self.copy_stmt().map(Statement::Copy);
        }
        if self.peek_kw("delete") {
            return self.delete().map(Statement::Delete);
        }
        if self.peek_kw("with") || self.peek_kw("select") || matches!(self.peek(), Token::LParen) {
            return self.query().map(Statement::Query);
        }
        Err(self.err(format!("expected a statement, found {:?}", self.peek())))
    }

    // ── query / set_expr / select ────────────────────────────────────────

    fn query(&self) -> Result<Query, ParseError> {
        let _g = self.enter_depth()?;
        let mut ctes = vec![];
        if self.eat_kw("with") {
            loop {
                let name = self.ident()?;
                self.expect_kw("as")?;
                self.expect(Token::LParen)?;
                let q = self.query()?;
                self.expect(Token::RParen)?;
                ctes.push(Cte {
                    name,
                    query: Box::new(q),
                });
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
        }
        let body = self.set_expr()?;
        let mut order_by = vec![];
        if self.eat_kw("order") {
            self.expect_kw("by")?;
            order_by.push(self.order_item()?);
            while self.eat(&Token::Comma) {
                order_by.push(self.order_item()?);
            }
        }
        let limit = if self.eat_kw("limit") {
            if self.eat_kw("all") {
                Some(Limit::All)
            } else {
                Some(Limit::Count(self.expect_int()?))
            }
        } else {
            None
        };
        let offset = if self.eat_kw("offset") {
            Some(self.expect_int()?)
        } else {
            None
        };
        Ok(Query {
            ctes,
            body,
            order_by,
            limit,
            offset,
        })
    }

    fn set_expr(&self) -> Result<SetExpr, ParseError> {
        let mut selects = vec![self.select_body()?];
        while self.peek_kw("union") {
            let offset = self.peek_offset();
            self.advance();
            if !self.eat_kw("all") {
                return Err(self.err_at(offset, "UNION requires ALL in v1"));
            }
            selects.push(self.select_body()?);
        }
        Ok(SetExpr { selects })
    }

    fn select_body(&self) -> Result<SelectBody, ParseError> {
        let _g = self.enter_depth()?;
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            let q = self.query()?;
            self.expect(Token::RParen)?;
            return Ok(SelectBody::Query(Box::new(q)));
        }
        self.expect_kw("select")?;
        let distinct = self.eat_kw("distinct");
        let mut projection = vec![self.select_item()?];
        while self.eat(&Token::Comma) {
            projection.push(self.select_item()?);
        }
        let mut from = vec![];
        if self.eat_kw("from") {
            from.push(self.table_expr()?);
            while self.eat(&Token::Comma) {
                from.push(self.table_expr()?);
            }
        }
        let selection = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        let mut group_by = vec![];
        if self.eat_kw("group") {
            self.expect_kw("by")?;
            group_by.push(self.expr()?);
            while self.eat(&Token::Comma) {
                group_by.push(self.expr()?);
            }
        }
        let having = if self.eat_kw("having") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(SelectBody::Select(Box::new(Select {
            distinct,
            projection,
            from,
            selection,
            group_by,
            having,
        })))
    }

    fn select_item(&self) -> Result<SelectItem, ParseError> {
        if matches!(self.peek(), Token::Star) {
            self.advance();
            return Ok(SelectItem::Wildcard);
        }
        if matches!(self.peek(), Token::Ident(_) | Token::QuotedIdent(_))
            && matches!(self.nth(1), Token::Dot)
            && matches!(self.nth(2), Token::Star)
        {
            let name = self.ident()?;
            self.advance(); // '.'
            self.advance(); // '*'
            return Ok(SelectItem::QualifiedWildcard(name));
        }
        let expr = self.expr()?;
        let alias = self.optional_alias(SELECT_ALIAS_STOP)?;
        Ok(SelectItem::Expr { expr, alias })
    }

    fn optional_alias(&self, stop: &[&str]) -> Result<Option<String>, ParseError> {
        if self.eat_kw("as") {
            return Ok(Some(self.ident()?));
        }
        match self.peek() {
            Token::Ident(s) if !stop.contains(&s.as_str()) => {
                let name = s.clone();
                self.advance();
                Ok(Some(name))
            }
            Token::QuotedIdent(s) => {
                let name = s.clone();
                self.advance();
                Ok(Some(name))
            }
            _ => Ok(None),
        }
    }

    fn table_expr(&self) -> Result<FromItem, ParseError> {
        let table = self.table_ref()?;
        let mut joins = vec![];
        loop {
            let offset = self.peek_offset();
            if self.eat_kw("inner") {
                self.expect_kw("join")?;
                let table = self.table_ref()?;
                let on = self.join_on()?;
                joins.push(Join {
                    kind: JoinKind::Inner,
                    table,
                    on,
                });
            } else if self.eat_kw("join") {
                let table = self.table_ref()?;
                let on = self.join_on()?;
                joins.push(Join {
                    kind: JoinKind::Inner,
                    table,
                    on,
                });
            } else if self.eat_kw("left") {
                self.eat_kw("outer");
                self.expect_kw("join")?;
                let table = self.table_ref()?;
                let on = self.join_on()?;
                joins.push(Join {
                    kind: JoinKind::Left,
                    table,
                    on,
                });
            } else if self.peek_kw("cross")
                || self.peek_kw("right")
                || self.peek_kw("full")
                || self.peek_kw("natural")
            {
                let name = match self.peek() {
                    Token::Ident(s) => s.to_uppercase(),
                    _ => unreachable!(),
                };
                return Err(self.err_at(offset, format!("{name} JOIN is not in v1")));
            } else {
                break;
            }
        }
        Ok(FromItem { table, joins })
    }

    fn join_on(&self) -> Result<Expr, ParseError> {
        if self.peek_kw("using") {
            return Err(self.err("USING is not in v1"));
        }
        self.expect_kw("on")?;
        self.expr()
    }

    fn table_ref(&self) -> Result<TableRef, ParseError> {
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            let q = self.query()?;
            self.expect(Token::RParen)?;
            self.eat_kw("as");
            let alias = self.ident()?;
            return Ok(TableRef::Subquery {
                query: Box::new(q),
                alias,
            });
        }
        let name = self.object_name()?;
        let alias = self.optional_alias(TABLE_ALIAS_STOP)?;
        Ok(TableRef::Table { name, alias })
    }

    fn order_item(&self) -> Result<OrderItem, ParseError> {
        let expr = self.expr()?;
        let desc = if self.eat_kw("desc") {
            true
        } else {
            self.eat_kw("asc");
            false
        };
        let nulls = if self.eat_kw("nulls") {
            if self.eat_kw("first") {
                Some(NullsOrder::First)
            } else if self.eat_kw("last") {
                Some(NullsOrder::Last)
            } else {
                return Err(self.err("expected FIRST or LAST after NULLS"));
            }
        } else {
            None
        };
        Ok(OrderItem { expr, desc, nulls })
    }

    // ── DDL / DML ────────────────────────────────────────────────────────

    fn create_table(&self) -> Result<CreateTable, ParseError> {
        self.expect_kw("create")?;
        self.expect_kw("table")?;
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.object_name()?;
        self.expect(Token::LParen)?;
        let mut columns = vec![self.column_def()?];
        while self.eat(&Token::Comma) {
            columns.push(self.column_def()?);
        }
        self.expect(Token::RParen)?;
        let clauses = self.table_clauses()?;
        Ok(CreateTable {
            if_not_exists,
            name,
            columns,
            clauses,
        })
    }

    fn column_def(&self) -> Result<ColumnDef, ParseError> {
        let name = self.ident()?;
        let type_name = self.type_name()?;
        let nullable = if self.eat_kw("not") {
            self.expect_kw("null")?;
            Some(false)
        } else if self.eat_kw("null") {
            Some(true)
        } else {
            None
        };
        Ok(ColumnDef {
            name,
            type_name,
            nullable,
        })
    }

    fn table_clauses(&self) -> Result<Vec<TableClause>, ParseError> {
        let mut clauses = vec![];
        loop {
            if self.eat_kw("engine") {
                self.expect(Token::Eq)?;
                clauses.push(TableClause::Engine(self.ident()?));
            } else if self.eat_kw("key") {
                self.expect(Token::LParen)?;
                let list = self.ident_list()?;
                self.expect(Token::RParen)?;
                clauses.push(TableClause::Key(list));
            } else if self.eat_kw("version") {
                self.expect(Token::LParen)?;
                let id = self.ident()?;
                self.expect(Token::RParen)?;
                clauses.push(TableClause::Version(id));
            } else if self.eat_kw("order") {
                self.expect_kw("by")?;
                let list = if self.eat(&Token::LParen) {
                    let list = self.ident_list()?;
                    self.expect(Token::RParen)?;
                    list
                } else {
                    self.ident_list()?
                };
                clauses.push(TableClause::OrderBy(list));
            } else if self.eat_kw("partition") {
                self.expect_kw("by")?;
                let column = self.ident()?;
                self.expect(Token::LParen)?;
                let bucket = self.string_lit()?;
                self.expect(Token::RParen)?;
                clauses.push(TableClause::PartitionBy { column, bucket });
            } else if self.eat_kw("ttl") {
                let column = self.ident()?;
                self.expect(Token::Plus)?;
                self.expect_kw("interval")?;
                let interval = self.string_lit()?;
                clauses.push(TableClause::Ttl { column, interval });
            } else if self.eat_kw("with") {
                self.expect(Token::LParen)?;
                let mut pairs = vec![];
                loop {
                    let key = self.ident()?;
                    self.expect(Token::Eq)?;
                    let value = self.literal()?;
                    pairs.push((key, value));
                    if !self.eat(&Token::Comma) {
                        break;
                    }
                }
                self.expect(Token::RParen)?;
                clauses.push(TableClause::With(pairs));
            } else {
                break;
            }
        }
        Ok(clauses)
    }

    fn create_schema(&self) -> Result<CreateSchema, ParseError> {
        self.expect_kw("create")?;
        self.expect_kw("schema")?;
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.ident()?;
        Ok(CreateSchema {
            if_not_exists,
            name,
        })
    }

    fn drop_table(&self) -> Result<DropTable, ParseError> {
        self.expect_kw("drop")?;
        self.expect_kw("table")?;
        let if_exists = if self.eat_kw("if") {
            self.expect_kw("exists")?;
            true
        } else {
            false
        };
        let name = self.object_name()?;
        Ok(DropTable { if_exists, name })
    }

    fn undrop_table(&self) -> Result<ObjectName, ParseError> {
        self.expect_kw("undrop")?;
        self.expect_kw("table")?;
        self.object_name()
    }

    fn truncate(&self) -> Result<ObjectName, ParseError> {
        self.expect_kw("truncate")?;
        self.eat_kw("table");
        self.object_name()
    }

    fn insert(&self) -> Result<Insert, ParseError> {
        self.expect_kw("insert")?;
        self.expect_kw("into")?;
        let table = self.object_name()?;
        let columns = self.optional_ident_list_in_parens()?;
        let source = if self.eat_kw("values") {
            let mut rows = vec![self.row()?];
            while self.eat(&Token::Comma) {
                rows.push(self.row()?);
            }
            InsertSource::Values(rows)
        } else {
            InsertSource::Query(Box::new(self.query()?))
        };
        Ok(Insert {
            table,
            columns,
            source,
        })
    }

    fn optional_ident_list_in_parens(&self) -> Result<Option<Vec<String>>, ParseError> {
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            let list = self.ident_list()?;
            self.expect(Token::RParen)?;
            Ok(Some(list))
        } else {
            Ok(None)
        }
    }

    fn row(&self) -> Result<Vec<Expr>, ParseError> {
        self.expect(Token::LParen)?;
        let mut items = vec![self.expr()?];
        while self.eat(&Token::Comma) {
            items.push(self.expr()?);
        }
        self.expect(Token::RParen)?;
        Ok(items)
    }

    fn copy_stmt(&self) -> Result<CopyStatement, ParseError> {
        self.expect_kw("copy")?;
        let table = self.object_name()?;
        let columns = self.optional_ident_list_in_parens()?;
        self.expect_kw("from")?;
        let source = self.string_lit()?;
        let mut options = vec![];
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            options.push(self.copy_option()?);
            while self.eat(&Token::Comma) {
                options.push(self.copy_option()?);
            }
            self.expect(Token::RParen)?;
        } else {
            self.eat_kw("with");
            while let Some(opt) = self.try_copy_option()? {
                options.push(opt);
            }
        }
        Ok(CopyStatement {
            table,
            columns,
            source,
            options,
        })
    }

    fn copy_option(&self) -> Result<CopyOption, ParseError> {
        match self.try_copy_option()? {
            Some(opt) => Ok(opt),
            None => Err(self.err("expected a COPY option (FORMAT, HEADER or DELIMITER)")),
        }
    }

    fn try_copy_option(&self) -> Result<Option<CopyOption>, ParseError> {
        if self.eat_kw("format") {
            return Ok(Some(CopyOption::Format(self.ident()?)));
        }
        if self.eat_kw("header") {
            let value = if self.eat_kw("true") {
                Some(true)
            } else if self.eat_kw("false") {
                Some(false)
            } else {
                None
            };
            return Ok(Some(CopyOption::Header(value)));
        }
        if self.eat_kw("delimiter") {
            return Ok(Some(CopyOption::Delimiter(self.string_lit()?)));
        }
        Ok(None)
    }

    fn delete(&self) -> Result<Delete, ParseError> {
        self.expect_kw("delete")?;
        self.expect_kw("from")?;
        let table = self.object_name()?;
        let selection = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Delete { table, selection })
    }

    // ── expr, precedence-climbed ─────────────────────────────────────────

    fn expr(&self) -> Result<Expr, ParseError> {
        let _g = self.enter_depth()?;
        self.parse_or()
    }

    fn parse_or(&self) -> Result<Expr, ParseError> {
        let mut left = self.parse_and()?;
        while self.eat_kw("or") {
            let right = self.parse_and()?;
            left = Expr::BinaryOp {
                op: BinaryOp::Or,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_and(&self) -> Result<Expr, ParseError> {
        let mut left = self.parse_not()?;
        while self.eat_kw("and") {
            let right = self.parse_not()?;
            left = Expr::BinaryOp {
                op: BinaryOp::And,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// Recurses into itself so `NOT NOT NOT x` parses and counts against the depth cap; the
    /// base case falls through to `parse_is`, so `NOT`'s operand is a full comparison (`NOT a
    /// = b` is `NOT (a = b)`, looser than `=`).
    fn parse_not(&self) -> Result<Expr, ParseError> {
        if self.eat_kw("not") {
            let _g = self.enter_depth()?;
            let inner = self.parse_not()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.parse_is()
    }

    fn parse_is(&self) -> Result<Expr, ParseError> {
        let expr = self.parse_comparison()?;
        if self.eat_kw("is") {
            let negated = self.eat_kw("not");
            if self.eat_kw("null") {
                return Ok(Expr::IsNull {
                    expr: Box::new(expr),
                    negated,
                });
            }
            if self.eat_kw("true") {
                return Ok(Expr::IsBool {
                    expr: Box::new(expr),
                    value: true,
                    negated,
                });
            }
            if self.eat_kw("false") {
                return Ok(Expr::IsBool {
                    expr: Box::new(expr),
                    value: false,
                    negated,
                });
            }
            return Err(self.err("expected NULL, TRUE or FALSE after IS"));
        }
        Ok(expr)
    }

    fn parse_comparison(&self) -> Result<Expr, ParseError> {
        let left = self.parse_between_in_like()?;
        let op = match self.peek() {
            Token::Eq => BinaryOp::Eq,
            Token::NotEq => BinaryOp::NotEq,
            Token::Lt => BinaryOp::Lt,
            Token::LtEq => BinaryOp::LtEq,
            Token::Gt => BinaryOp::Gt,
            Token::GtEq => BinaryOp::GtEq,
            _ => return Ok(left),
        };
        self.advance();
        let right = self.parse_between_in_like()?;
        Ok(Expr::BinaryOp {
            op,
            left: Box::new(left),
            right: Box::new(right),
        })
    }

    fn parse_between_in_like(&self) -> Result<Expr, ParseError> {
        let expr = self.parse_concat()?;
        let negated = self.peek_kw("not")
            && (self.nth_kw(1, "between")
                || self.nth_kw(1, "in")
                || self.nth_kw(1, "like")
                || self.nth_kw(1, "ilike"));
        if negated {
            self.advance();
        }
        if self.eat_kw("between") {
            let low = self.parse_concat()?;
            self.expect_kw("and")?;
            let high = self.parse_concat()?;
            return Ok(Expr::Between {
                expr: Box::new(expr),
                low: Box::new(low),
                high: Box::new(high),
                negated,
            });
        }
        if self.eat_kw("in") {
            self.expect(Token::LParen)?;
            let mut list = vec![self.expr()?];
            while self.eat(&Token::Comma) {
                list.push(self.expr()?);
            }
            self.expect(Token::RParen)?;
            return Ok(Expr::InList {
                expr: Box::new(expr),
                list,
                negated,
            });
        }
        if self.eat_kw("like") {
            let pattern = self.parse_concat()?;
            return Ok(Expr::Like {
                expr: Box::new(expr),
                pattern: Box::new(pattern),
                case_insensitive: false,
                negated,
            });
        }
        if self.eat_kw("ilike") {
            let pattern = self.parse_concat()?;
            return Ok(Expr::Like {
                expr: Box::new(expr),
                pattern: Box::new(pattern),
                case_insensitive: true,
                negated,
            });
        }
        if negated {
            return Err(self.err("expected BETWEEN, IN, LIKE or ILIKE after NOT"));
        }
        Ok(expr)
    }

    fn parse_concat(&self) -> Result<Expr, ParseError> {
        let mut left = self.parse_additive()?;
        while matches!(self.peek(), Token::Concat) {
            self.advance();
            let right = self.parse_additive()?;
            left = Expr::BinaryOp {
                op: BinaryOp::Concat,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_additive(&self) -> Result<Expr, ParseError> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Token::Plus => BinaryOp::Add,
                Token::Minus => BinaryOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_multiplicative()?;
            left = Expr::BinaryOp {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_multiplicative(&self) -> Result<Expr, ParseError> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Token::Star => BinaryOp::Mul,
                Token::Slash => BinaryOp::Div,
                Token::Percent => BinaryOp::Mod,
                _ => break,
            };
            self.advance();
            let right = self.parse_unary()?;
            left = Expr::BinaryOp {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// Recurses into itself (like `parse_not`), so `- - - - 1` parses and counts against the
    /// depth cap. The cast postfix binds tighter (PG rule): `-x::int` is `-(x::int)`.
    fn parse_unary(&self) -> Result<Expr, ParseError> {
        let op = match self.peek() {
            Token::Minus => UnaryOp::Neg,
            Token::Plus => UnaryOp::Plus,
            _ => return self.parse_cast_postfix(),
        };
        self.advance();
        let _g = self.enter_depth()?;
        let inner = self.parse_unary()?;
        Ok(Expr::UnaryOp {
            op,
            expr: Box::new(inner),
        })
    }

    fn parse_cast_postfix(&self) -> Result<Expr, ParseError> {
        let mut expr = self.primary()?;
        while matches!(self.peek(), Token::DoubleColon) {
            self.advance();
            let type_name = self.type_name()?;
            expr = Expr::Cast {
                expr: Box::new(expr),
                type_name,
                via_cast: false,
            };
        }
        Ok(expr)
    }

    fn primary(&self) -> Result<Expr, ParseError> {
        let offset = self.peek_offset();
        match self.peek().clone() {
            Token::Number(text) => {
                self.advance();
                Ok(Expr::Literal(Literal::Number(text)))
            }
            Token::Str(text) => {
                self.advance();
                Ok(Expr::Literal(Literal::String(text)))
            }
            Token::Star => {
                self.advance();
                Ok(Expr::Wildcard)
            }
            Token::LParen => {
                // No guard here: `query()` and `expr()` each guard on their own entry, and a
                // guard here too would count this one nesting layer twice (breaking "depth
                // 100 parses fine" well before 128 real layers).
                self.advance();
                if self.peek_kw("select") || self.peek_kw("with") {
                    let q = self.query()?;
                    self.expect(Token::RParen)?;
                    Ok(Expr::Subquery(Box::new(q)))
                } else {
                    let e = self.expr()?;
                    self.expect(Token::RParen)?;
                    Ok(e)
                }
            }
            Token::Ident(word) => match word.as_str() {
                "true" => {
                    self.advance();
                    Ok(Expr::Literal(Literal::Bool(true)))
                }
                "false" => {
                    self.advance();
                    Ok(Expr::Literal(Literal::Bool(false)))
                }
                "null" => {
                    self.advance();
                    Ok(Expr::Literal(Literal::Null))
                }
                "date" if matches!(self.nth(1), Token::Str(_)) => {
                    self.advance();
                    Ok(Expr::Literal(Literal::Date(self.string_lit()?)))
                }
                "timestamp" if matches!(self.nth(1), Token::Str(_)) => {
                    self.advance();
                    Ok(Expr::Literal(Literal::Timestamp(self.string_lit()?)))
                }
                "interval" if matches!(self.nth(1), Token::Str(_)) => {
                    self.advance();
                    Ok(Expr::Literal(Literal::Interval(self.string_lit()?)))
                }
                "cast" => self.parse_cast(),
                "extract" => self.parse_extract(),
                "case" => self.parse_case(),
                _ if matches!(self.nth(1), Token::LParen) => self.parse_func_call(),
                _ => self.parse_column_ref(),
            },
            Token::QuotedIdent(_) => self.parse_column_ref(),
            other => Err(self.err_at(offset, format!("expected an expression, found {other:?}"))),
        }
    }

    fn parse_cast(&self) -> Result<Expr, ParseError> {
        self.advance(); // 'cast'
        self.expect(Token::LParen)?;
        let expr = self.expr()?;
        self.expect_kw("as")?;
        let type_name = self.type_name()?;
        self.expect(Token::RParen)?;
        Ok(Expr::Cast {
            expr: Box::new(expr),
            type_name,
            via_cast: true,
        })
    }

    fn parse_extract(&self) -> Result<Expr, ParseError> {
        self.advance(); // 'extract'
        self.expect(Token::LParen)?;
        let field = self.ident()?;
        self.expect_kw("from")?;
        let expr = self.expr()?;
        self.expect(Token::RParen)?;
        Ok(Expr::Extract {
            field,
            expr: Box::new(expr),
        })
    }

    fn parse_case(&self) -> Result<Expr, ParseError> {
        self.advance(); // 'case'
        let operand = if self.peek_kw("when") {
            None
        } else {
            Some(Box::new(self.expr()?))
        };
        self.expect_kw("when")?;
        let mut branches = vec![];
        loop {
            let cond = self.expr()?;
            self.expect_kw("then")?;
            let result = self.expr()?;
            branches.push((cond, result));
            if !self.eat_kw("when") {
                break;
            }
        }
        let otherwise = if self.eat_kw("else") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        self.expect_kw("end")?;
        Ok(Expr::Case {
            operand,
            branches,
            otherwise,
        })
    }

    fn parse_func_call(&self) -> Result<Expr, ParseError> {
        let name = self.ident()?;
        self.expect(Token::LParen)?;
        let distinct = self.eat_kw("distinct");
        let mut args = vec![];
        if matches!(self.peek(), Token::Star) {
            self.advance();
            args.push(Expr::Wildcard);
        } else if !matches!(self.peek(), Token::RParen) {
            args.push(self.expr()?);
            while self.eat(&Token::Comma) {
                args.push(self.expr()?);
            }
        }
        self.expect(Token::RParen)?;
        let filter = if self.eat_kw("filter") {
            self.expect(Token::LParen)?;
            self.expect_kw("where")?;
            let f = self.expr()?;
            self.expect(Token::RParen)?;
            Some(Box::new(f))
        } else {
            None
        };
        Ok(Expr::Func {
            name,
            distinct,
            args,
            filter,
        })
    }

    fn parse_column_ref(&self) -> Result<Expr, ParseError> {
        let mut parts = vec![self.ident()?];
        while matches!(self.peek(), Token::Dot) && !matches!(self.nth(1), Token::Star) {
            self.advance();
            parts.push(self.ident()?);
        }
        Ok(Expr::Column(parts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(sql: &str) -> Vec<Statement> {
        parse(sql).unwrap_or_else(|e| panic!("expected {sql:?} to parse, got {e}"))
    }

    fn one(sql: &str) -> Statement {
        let mut stmts = ok(sql);
        assert_eq!(stmts.len(), 1, "{sql:?}");
        stmts.remove(0)
    }

    fn select(sql: &str) -> Select {
        match one(sql) {
            Statement::Query(q) => match q.body.selects.into_iter().next().unwrap() {
                SelectBody::Select(s) => *s,
                other => panic!("expected a plain SELECT, got {other:?}"),
            },
            other => panic!("expected a query, got {other:?}"),
        }
    }

    fn expr(sql: &str) -> Expr {
        let s = select(&format!("SELECT {sql}"));
        match s.projection.into_iter().next().unwrap() {
            SelectItem::Expr { expr, .. } => expr,
            other => panic!("expected an expr item, got {other:?}"),
        }
    }

    // ── every statement from tests/slt/*.slt (grep -h '^[A-Z]' tests/slt/*.slt) ────────────

    #[test]
    fn slt_filter() {
        for s in [
            "CREATE TABLE t (id INTEGER, name TEXT, score INTEGER)",
            "INSERT INTO t VALUES (1, 'Alice', 50), (2, 'bob', NULL), (3, 'CHARLIE', 75), (4, NULL, 20), (5, 'dave', 90)",
            "SELECT id FROM t WHERE score > 40 AND score < 80",
            "SELECT id FROM t WHERE score < 30 OR score > 80",
            "SELECT id FROM t WHERE NOT (score > 60)",
            "SELECT id FROM t WHERE id IN (1, 3, 5)",
            "SELECT id FROM t WHERE score BETWEEN 50 AND 80",
            "SELECT id FROM t WHERE name LIKE 'char%'",
            "SELECT id FROM t WHERE name ILIKE 'char%'",
            "SELECT id FROM t WHERE score IS NULL",
            "SELECT id FROM t WHERE name IS NOT NULL",
        ] {
            ok(s);
        }
    }

    #[test]
    fn slt_ddl_dml() {
        for s in [
            "CREATE TABLE src (a INTEGER)",
            "INSERT INTO src VALUES (1), (2), (3)",
            "CREATE TABLE dst (a INTEGER)",
            "INSERT INTO dst SELECT a FROM src WHERE a > 1",
            "SELECT a FROM dst",
            "DELETE FROM dst WHERE a = 2",
            "DROP TABLE dst",
        ] {
            ok(s);
        }
    }

    #[test]
    fn slt_select() {
        for s in [
            "CREATE TABLE t (a INTEGER, b INTEGER)",
            "INSERT INTO t VALUES (1, 10), (2, 20), (2, 30)",
            "SELECT 1 + 2",
            "SELECT 10 - 3",
            "SELECT 4 * 5",
            "SELECT 7 % 3",
            "SELECT 1 < 2",
            "SELECT 2 = 3",
            "SELECT CASE WHEN 1 > 2 THEN 'big' ELSE 'small' END",
            "SELECT CAST('42' AS INTEGER)",
            "SELECT CAST(5 AS DOUBLE)",
            "SELECT a AS renamed FROM t WHERE a = 1",
            "SELECT DISTINCT a FROM t",
        ] {
            ok(s);
        }
    }

    #[test]
    fn slt_aggregate() {
        for s in [
            "CREATE TABLE sales (cat TEXT, amt INTEGER)",
            "INSERT INTO sales VALUES ('a', 10), ('a', 20), ('b', 5), ('b', 15), ('b', 25)",
            "SELECT cat, count(*), sum(amt), avg(amt), min(amt), max(amt) FROM sales GROUP BY cat",
            "SELECT cat, sum(amt) FROM sales GROUP BY cat HAVING sum(amt) > 40",
            "SELECT amt % 2, count(*) FROM sales GROUP BY amt % 2",
            "CREATE TABLE empty_t (v INTEGER)",
            "SELECT count(*) FROM empty_t",
            "SELECT count(v) FROM empty_t",
            "SELECT sum(v) FROM empty_t",
            "SELECT avg(v) FROM empty_t",
            "SELECT min(v) FROM empty_t",
            "SELECT max(v) FROM empty_t",
        ] {
            ok(s);
        }
    }

    #[test]
    fn slt_cte_union() {
        for s in [
            "CREATE TABLE nums (v INTEGER)",
            "INSERT INTO nums VALUES (1), (2), (3), (4), (5)",
            "WITH evens AS (SELECT v FROM nums WHERE v % 2 = 0) SELECT v FROM evens ORDER BY v",
            "SELECT v FROM nums WHERE v <= 2 UNION ALL SELECT v FROM nums WHERE v >= 4",
            "SELECT s.v, s.v * 2 FROM (SELECT v FROM nums WHERE v > 3) AS s ORDER BY s.v",
        ] {
            ok(s);
        }
    }

    #[test]
    fn slt_order_limit() {
        for s in [
            "CREATE TABLE ol (a INTEGER, b INTEGER)",
            "INSERT INTO ol VALUES (1, 5), (1, 3), (2, 9), (2, 1), (3, 7)",
            "SELECT a, b FROM ol ORDER BY a ASC, b DESC",
            "SELECT a, b FROM ol ORDER BY a ASC, b DESC LIMIT 3",
            "SELECT a, b FROM ol ORDER BY a ASC, b DESC LIMIT 2 OFFSET 2",
        ] {
            ok(s);
        }
    }

    #[test]
    fn slt_join() {
        for s in [
            "CREATE TABLE customers (id INTEGER, name TEXT)",
            "INSERT INTO customers VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')",
            "CREATE TABLE orders (id INTEGER, cust_id INTEGER, amt INTEGER)",
            "INSERT INTO orders VALUES (1, 1, 100), (2, 1, 50), (3, 2, 75)",
            "SELECT c.name, o.amt FROM customers c INNER JOIN orders o ON c.id = o.cust_id",
            "SELECT c.name, o.amt FROM customers c LEFT JOIN orders o ON c.id = o.cust_id",
            "SELECT c.name, count(*), sum(o.amt) FROM customers c INNER JOIN orders o ON c.id = o.cust_id GROUP BY c.name",
        ] {
            ok(s);
        }
    }

    #[test]
    fn slt_null() {
        for s in [
            "CREATE TABLE n (a INTEGER)",
            "INSERT INTO n VALUES (1), (NULL), (2), (NULL), (3)",
            "SELECT NULL = NULL",
            "SELECT NULL AND false",
            "SELECT NULL AND true",
            "SELECT NULL OR true",
            "SELECT count(a), count(*) FROM n",
            "SELECT a FROM n ORDER BY a NULLS FIRST",
            "SELECT a FROM n ORDER BY a NULLS LAST",
        ] {
            ok(s);
        }
    }

    #[test]
    fn slt_strings() {
        for s in [
            "CREATE TABLE words (w TEXT)",
            "INSERT INTO words VALUES ('Hello'), ('World')",
            "SELECT lower(w) FROM words",
            "SELECT upper('HeLLo')",
            "SELECT length('hello')",
            "SELECT substr('hello world', 1, 5)",
            "SELECT substr('hello world', 7)",
            "SELECT 'foo' || 'bar'",
            "SELECT 'a' || 'b' || 'c'",
        ] {
            ok(s);
        }
    }

    // ── every ClickBench query (bench/queries/clickbench/*.sql) ────────────────────────────

    #[test]
    fn clickbench_queries() {
        for s in [
            r#"SELECT count(*) FROM hits"#,
            r#"SELECT count(*) FROM hits WHERE "AdvEngineID" <> 0"#,
            r#"SELECT sum("AdvEngineID"), count(*), avg("ResolutionWidth") FROM hits"#,
            r#"SELECT avg("UserID") FROM hits"#,
            r#"SELECT count(DISTINCT "UserID") FROM hits"#,
            r#"SELECT count(DISTINCT "SearchPhrase") FROM hits"#,
            r#"SELECT min("EventDate"), max("EventDate") FROM hits"#,
            r#"SELECT "AdvEngineID", count(*) AS c FROM hits WHERE "AdvEngineID" <> 0 GROUP BY "AdvEngineID" ORDER BY c DESC"#,
            r#"SELECT "RegionID", count(DISTINCT "UserID") AS u FROM hits GROUP BY "RegionID" ORDER BY u DESC LIMIT 10"#,
            r#"SELECT "RegionID", sum("AdvEngineID"), count(*) AS c, avg("ResolutionWidth"), count(DISTINCT "UserID")
                FROM hits GROUP BY "RegionID" ORDER BY c DESC LIMIT 10"#,
            r#"SELECT "MobilePhoneModel", count(DISTINCT "UserID") AS u FROM hits
                WHERE "MobilePhoneModel" <> '' GROUP BY "MobilePhoneModel" ORDER BY u DESC LIMIT 10"#,
            r#"SELECT "MobilePhoneModel", "SearchPhrase", count(*) AS c FROM hits
                WHERE "MobilePhoneModel" <> '' AND "SearchPhrase" <> ''
                GROUP BY "MobilePhoneModel", "SearchPhrase" ORDER BY c DESC LIMIT 10"#,
            r#"SELECT "SearchPhrase", count(*) AS c FROM hits WHERE "SearchPhrase" <> '' GROUP BY "SearchPhrase" ORDER BY c DESC LIMIT 10"#,
            r#"SELECT count(*) FROM hits WHERE "SearchPhrase" LIKE '%rust%'"#,
            r#"SELECT count(*) FROM hits WHERE "EventDate" >= DATE '1970-01-01' AND "EventDate" < DATE '2030-01-01'"#,
        ] {
            ok(s);
        }
    }

    // ── every OTel query (bench/queries/otel/*.sql), including setup.sql ───────────────────

    #[test]
    fn otel_queries() {
        for s in [
            "CREATE SCHEMA IF NOT EXISTS otel",
            "SELECT * FROM otel.spans WHERE trace_id = '00000000000000000000000000000001'",
            r#"SELECT count(*) FROM otel.logs WHERE body LIKE '%zzq_needle_7f3a%'"#,
            r#"SELECT "service.name", quantile(duration_ns, 0.95) AS p95_ns FROM otel.spans GROUP BY "service.name" ORDER BY p95_ns DESC"#,
            r#"SELECT "attributes.http.route", avg(status_code) AS error_rate, count(*) AS total
                FROM otel.spans GROUP BY "attributes.http.route" ORDER BY error_rate DESC"#,
            r#"SELECT trace_id, sum(duration_ns) AS total_ns FROM otel.spans GROUP BY trace_id ORDER BY total_ns DESC LIMIT 10"#,
            r#"SELECT "resource.run.id", count(*) AS c FROM otel.spans GROUP BY "resource.run.id" ORDER BY c DESC"#,
        ] {
            ok(s);
        }
    }

    // ── precedence ───────────────────────────────────────────────────────

    #[test]
    fn precedence_add_before_mul() {
        // 1 + (2 * 3)
        match expr("1 + 2 * 3") {
            Expr::BinaryOp {
                op: BinaryOp::Add,
                right,
                ..
            } => {
                assert!(matches!(
                    *right,
                    Expr::BinaryOp {
                        op: BinaryOp::Mul,
                        ..
                    }
                ));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn precedence_and_before_or() {
        // a OR (b AND c)
        match expr("a OR b AND c") {
            Expr::BinaryOp {
                op: BinaryOp::Or,
                right,
                ..
            } => {
                assert!(matches!(
                    *right,
                    Expr::BinaryOp {
                        op: BinaryOp::And,
                        ..
                    }
                ));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn not_binds_looser_than_eq() {
        // NOT (a = b)
        match expr("NOT a = b") {
            Expr::Not(inner) => assert!(matches!(
                *inner,
                Expr::BinaryOp {
                    op: BinaryOp::Eq,
                    ..
                }
            )),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cast_postfix_on_column() {
        match expr("x::int") {
            Expr::Cast {
                expr, type_name, ..
            } => {
                assert!(matches!(*expr, Expr::Column(parts) if parts == vec!["x".to_string()]));
                assert_eq!(type_name.name, "int");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cast_binds_tighter_than_unary_minus() {
        // -(x::int)
        match expr("-x::int") {
            Expr::UnaryOp {
                op: UnaryOp::Neg,
                expr,
            } => {
                assert!(matches!(*expr, Expr::Cast { .. }));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn x_cast_to_string_keeps_type_text() {
        match expr("x::string") {
            Expr::Cast { type_name, .. } => assert_eq!(type_name.name, "string"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn via_cast_distinguishes_postfix_from_the_cast_keyword() {
        match expr("x::string") {
            Expr::Cast { via_cast, .. } => assert!(!via_cast),
            other => panic!("{other:?}"),
        }
        match expr("CAST(x AS STRING)") {
            Expr::Cast { via_cast, .. } => assert!(via_cast),
            other => panic!("{other:?}"),
        }
    }

    // ── functions ────────────────────────────────────────────────────────

    #[test]
    fn count_star() {
        match expr("count(*)") {
            Expr::Func { name, args, .. } => {
                assert_eq!(name, "count");
                assert_eq!(args, vec![Expr::Wildcard]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn count_distinct() {
        match expr("count(DISTINCT x)") {
            Expr::Func {
                name,
                distinct,
                args,
                ..
            } => {
                assert_eq!(name, "count");
                assert!(distinct);
                assert_eq!(args.len(), 1);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sum_with_filter() {
        match expr("sum(x) FILTER (WHERE y > 0)") {
            Expr::Func { name, filter, .. } => {
                assert_eq!(name, "sum");
                assert!(filter.is_some());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn extract_year() {
        match expr("extract(year FROM ts)") {
            Expr::Extract { field, .. } => assert_eq!(field, "year"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn substr_three_args() {
        match expr("substr(s, 1, 2)") {
            Expr::Func { name, args, .. } => {
                assert_eq!(name, "substr");
                assert_eq!(args.len(), 3);
            }
            other => panic!("{other:?}"),
        }
    }

    // ── CREATE TABLE with every clause ───────────────────────────────────

    #[test]
    fn create_table_every_clause() {
        let stmt = one(
            "CREATE TABLE IF NOT EXISTS t (a INT64 NOT NULL, b STRING NULL)
             ENGINE = mergetree
             KEY (a, b)
             VERSION (a)
             ORDER BY (a, b)
             PARTITION BY a ('1 day')
             TTL a + INTERVAL '30 days'
             WITH (x = 1, y = 'z')",
        );
        let ct = match stmt {
            Statement::CreateTable(ct) => ct,
            other => panic!("{other:?}"),
        };
        assert!(ct.if_not_exists);
        assert_eq!(ct.columns.len(), 2);
        assert_eq!(ct.columns[0].nullable, Some(false));
        assert_eq!(ct.columns[1].nullable, Some(true));
        assert_eq!(ct.clauses.len(), 7);
        assert!(matches!(&ct.clauses[0], TableClause::Engine(e) if e == "mergetree"));
        assert!(matches!(&ct.clauses[1], TableClause::Key(k) if k.len() == 2));
        assert!(matches!(&ct.clauses[2], TableClause::Version(v) if v == "a"));
        assert!(matches!(&ct.clauses[3], TableClause::OrderBy(o) if o.len() == 2));
        assert!(
            matches!(&ct.clauses[4], TableClause::PartitionBy { column, bucket }
            if column == "a" && bucket == "1 day")
        );
        assert!(
            matches!(&ct.clauses[5], TableClause::Ttl { column, interval }
            if column == "a" && interval == "30 days")
        );
        assert!(matches!(&ct.clauses[6], TableClause::With(pairs) if pairs.len() == 2));
    }

    #[test]
    fn order_by_without_parens_also_parses() {
        let stmt = one("CREATE TABLE t (a INT64) ORDER BY a, a");
        assert!(matches!(stmt, Statement::CreateTable(_)));
    }

    #[test]
    fn list_of_type_parses() {
        let stmt = one("CREATE TABLE t (a LIST<INT64>)");
        match stmt {
            Statement::CreateTable(ct) => {
                assert!(ct.columns[0].type_name.list_of.is_some());
            }
            other => panic!("{other:?}"),
        }
    }

    // ── COPY, both option syntaxes ───────────────────────────────────────

    #[test]
    fn copy_parenthesised_options() {
        let stmt = one("COPY t FROM '/tmp/x.csv' (FORMAT csv, HEADER true, DELIMITER ',')");
        match stmt {
            Statement::Copy(c) => assert_eq!(c.options.len(), 3),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn copy_bare_with_options() {
        let stmt = one("COPY t FROM '/tmp/x.ndjson' WITH FORMAT ndjson HEADER");
        match stmt {
            Statement::Copy(c) => {
                assert_eq!(c.options.len(), 2);
                assert!(matches!(&c.options[1], CopyOption::Header(None)));
            }
            other => panic!("{other:?}"),
        }
    }

    // ── INSERT, both forms ───────────────────────────────────────────────

    #[test]
    fn insert_with_column_list() {
        let stmt = one("INSERT INTO t (a, b) VALUES (1, 2)");
        match stmt {
            Statement::Insert(i) => {
                assert_eq!(i.columns, Some(vec!["a".to_string(), "b".to_string()]))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn insert_from_query() {
        let stmt = one("INSERT INTO t SELECT a FROM src");
        match stmt {
            Statement::Insert(i) => assert!(matches!(i.source, InsertSource::Query(_))),
            other => panic!("{other:?}"),
        }
    }

    // ── rejected constructs name what they rejected ─────────────────────

    #[test]
    fn full_join_is_rejected_by_name() {
        let err = parse("SELECT * FROM a FULL JOIN b ON a.x = b.x").unwrap_err();
        assert!(err.message.contains("FULL JOIN"), "{}", err.message);
        assert!(err.message.contains("not in v1"), "{}", err.message);
    }

    #[test]
    fn union_without_all_is_rejected() {
        let err = parse("SELECT 1 UNION SELECT 2").unwrap_err();
        assert!(
            err.message.contains("UNION requires ALL in v1"),
            "{}",
            err.message
        );
    }

    // ── depth cap ────────────────────────────────────────────────────────

    #[test]
    fn deep_but_legal_nesting_parses() {
        let sql = format!("SELECT {}1{}", "(".repeat(100), ")".repeat(100));
        parse(&sql).unwrap();
    }

    #[test]
    fn max_depth_plus_one_fails_small_case() {
        // Cheap enough to run under Miri: exactly MAX_DEPTH + 1 parens.
        let sql = format!(
            "SELECT {}1{}",
            "(".repeat(MAX_DEPTH + 1),
            ")".repeat(MAX_DEPTH + 1)
        );
        let err = parse(&sql).unwrap_err();
        assert!(err.message.contains("nesting"), "{}", err.message);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // 100k-token input, no UB surface beyond the small depth tests
    fn hundred_thousand_parens_errors_not_overflows() {
        let sql = format!("{}1{}", "(".repeat(100_000), ")".repeat(100_000));
        let err = parse(&sql).unwrap_err();
        assert!(err.message.contains("nesting"), "{}", err.message);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // 100k-token input, no UB surface beyond the small depth tests
    fn ten_thousand_scalar_subqueries_errors_not_overflows() {
        let sql = format!(
            "SELECT {}1{}",
            "(SELECT ".repeat(10_000),
            ")".repeat(10_000)
        );
        let err = parse(&sql).unwrap_err();
        assert!(err.message.contains("nesting"), "{}", err.message);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // 100k-token input, no UB surface beyond the small depth tests
    fn hundred_thousand_unary_minuses_errors_not_overflows() {
        let sql = format!("SELECT {}1", "- ".repeat(100_000));
        let err = parse(&sql).unwrap_err();
        assert!(err.message.contains("nesting"), "{}", err.message);
    }

    // ── line/column ──────────────────────────────────────────────────────

    #[test]
    fn parse_error_reports_line_and_column() {
        let err = parse("SELECT 1\nFROM t WHERE ,").unwrap_err();
        assert_eq!(err.line, 2);
        assert!(err.column > 0);
    }
}
