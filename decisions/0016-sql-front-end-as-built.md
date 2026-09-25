# D0016: SQL front end as built

**Status:** accepted · 2026-09-25
**Rule:** SQL is a hand-rolled lexer/parser, a binder that addresses columns by a
unique internal name rather than a position (never `exec::Expr::Column` until the final
lowering), a rule-based planner with no cost model, and a statement executor over `Store`. The
library API ships first (SPEC §4, D0013/D0015 precedent).

## Why

- SPEC §8 wants a dialect close to PG/DuckDB, without a query optimizer: the nine planner
  rules below are applied unconditionally, in a fixed order, never chosen by a cost estimate.
- A binder built on named columns (`"r0.id"`-style, one counter per relation instance) makes
  projection pruning and predicate pushdown local rewrites — dropping an unused scan column,
  or moving a filter, never needs to renumber anything else in the tree, because nothing else
  addresses that scan by position until the very last step.
- The AST keeps `x::string` apart from `CAST(x AS STRING)` (`Cast::via_cast`). The companion
  rule routes only the postfix spelling to the companion, and `CAST` always casts.
- Recursion is bounded twice: `MAX_DEPTH` caps nesting, and parse, bind and plan run on a
  thread with a fixed 16 MiB stack (`parser::on_sql_stack`). About 13 debug frames per nesting
  level would otherwise overflow a 2 MiB caller thread well under the cap.

## Consequences

- **Dialect:** PG-shaped. `MAX_DEPTH` is 128. A statement that binds more than 10 000 table
  scans is `Bind`: a CTE is inlined per reference, so a `WITH` chain that references each level
  twice grows the plan exponentially.
- **`::string` (root decision):** a `::` postfix (not `CAST(...)`, not `::text`/`::varchar`) on
  a bare column reference resolves to that relation's `<col>::string` companion, or
  `NULL::STRING` if it declares none. `coalesce_text(x)` resolves the same way. Neither warns;
  every other reference to a base field with a companion does, once per `(table, field)`.
- **Routing (C4):** COPY and INSERT route a value that does not fit its column to the declared
  companion as text, or fail the statement naming the column; the rule `RowBuilder` implements.
  A string literal stays a string (`'500'` into INT64 goes to the companion), so a DATE or
  TIMESTAMP is written as a typed literal (`DATE '2024-01-01'`).
- **Literal typing:** an integer literal is INT64 if it fits, else UINT64, else `DECIMAL(p,0)`
  sized to its digits; a literal with a decimal point is `DECIMAL(p,s)` from its digits (`0.5` is
  `DECIMAL(1,1)`, as in DuckDB: leading zeros are not digits); one with an exponent is FLOAT64.
  **Adaptation:** a number or `NULL` literal meeting a typed operand is retyped via
  `Value::from_text`/the other side's type first; **promotion** (a cast on whichever side is still
  narrower) follows this table: INT64/UINT64 vs FLOAT64 → FLOAT64; INT64/UINT64 vs DECIMAL(p,s) →
  DECIMAL(min(38,19+s),s); DECIMAL vs FLOAT64 → FLOAT64; DATE vs TIMESTAMP → TIMESTAMP. An untyped
  `NULL` takes the other operand's type (BOOL inside AND/OR/NOT, STRING standalone).
- **NULL typing and promotion are hint-driven, not AST-order-driven:** `bind_expr` takes an
  optional type hint; a comparison/arithmetic pair binds whichever side is *not* an adaptable
  literal first, then binds the literal side with that type as its hint. This means `1.5 > x`
  and `x < 1.5` adapt identically, regardless of which side is written first.
- **Default db** is `main`; `CREATE SCHEMA [IF NOT EXISTS]` succeeds and changes nothing.
- **`now()`/`current_timestamp`** fold to one `Literal(Timestamp)` per statement, read at bind
  start from `Options.now` or the clock; `exec` itself stays pure (no `now()` kernel, SPEC §13).
- **The regex subset** is a Thompson-NFA engine (compiled programs are capped at 100 000
  instructions, so nested counted repeats are an error, not an allocation); `regexp_match`/`regexp_matches` both
  return BOOL (unanchored, linear time).
- **`time_bucket`'s origin** defaults to `946_857_600_000_000_000` ns (2000-01-03, DuckDB's
  default) when no third argument is given; only fixed-width intervals are accepted anywhere
  (`INTERVAL 'n unit'`, ns through weeks) — months and years are `Bind: fixed-width intervals
  only`, checked once in `parse_interval`, shared by `TTL`, `PARTITION BY` and `time_bucket`.
- **Frozen:** the `CountDistinct` v1 state encoding. SQL calls it only through
  `exec::AggFunc::CountDistinct`, never touching its bytes.
- **COPY** is one atomic `store.write_many`: every batch the reader produces lands in one
  flush and one commit. A column list is accepted only when it is `HEADER`-based and names
  exactly the table's data columns in their declared order (the readers cannot remap
  positionally); anything else is `Bind: "COPY column list needs HEADER"`.
- **The DELETE predicate subset:** an AND of `column <op> literal` (literal typed by the
  column), or no `WHERE` at all. An empty predicate list is itself a valid tombstone — storage
  treats "AND of zero conjuncts" as matching every row (`scan.rs`'s `tombstone_keep_mask`) — so
  `DELETE FROM t` needs no special case; `rows_affected` is a `count(*)` with the same
  predicate (or none) run on the statement's snapshot, so a
  concurrent commit that lands before the tombstone is deleted but not counted.
- **The join restrictions:** `ON` must be an AND of conjuncts with at least one
  `left_col = right_col`; other single-side conjuncts push down (`Filter` under that side);
  cross-side non-equalities become a `Filter` above an INNER join, and are rejected outright
  for LEFT (`Bind: "LEFT JOIN ON supports equalities and right-side filters in v1"`). Comma
  joins (`FROM a, b`) are `Bind: "use JOIN … ON"`.
- **Output-name dedupe** is case-sensitive, appending `_1`, `_2`, … in select-list order,
  because `Batch::new` rejects duplicate field names.
- **Planner rules 4 and 6** are met by storage and exec: `ScanSpec.predicate`
  is how storage's own manifest/footer pruning sees a predicate at all (`storage/prune.rs`),
  and the executor already splits an `Aggregate` into per-worker partials merged back
  (`exec/agg/hash.rs`); the planner emits exactly one `Plan::Aggregate` node either way.
- **`exec::AggCall.args`/`.filter` and `Plan::Aggregate.group_by` are positions, not
  expressions:** a composite `GROUP BY` key, a non-column aggregate argument (`sum(amt * 2)`),
  and a `FILTER (WHERE …)` are each materialized into a named column by a `Project` the binder
  inserts directly below the `Aggregate`, before the planner ever sees them.
- **Join side selection (rule 7)** uses each side's estimated row count — a table's live
  segments' summed `rows` from the manifest, roughly halved per `Filter` layer above it, summed
  across a nested join's own two sides — to decide which side is smaller; that side becomes the
  build side (`Plan::Join`'s right). A restoring `Project` puts columns back in written order
  when the sides swap.
- **SPEC §4 ("ships whole"):** the SQL library API ships first; the CLI/HTTP/MCP surfaces are
  E8 (adelie-v5u), the D0013/D0015 precedent.
- **The SQL-level choices the DuckDB differential may revisit:** `count(DISTINCT)` exactness
  (frozen v1 state), INT64 `/` truncation (D0015), and output naming
  (alias → column/function name → `?column?`, deduped case-sensitively).
- **Aggregate result types** come from the accumulator itself (`exec::agg_output_type`), so the
  binder's field types cannot drift from what the executor produces.
