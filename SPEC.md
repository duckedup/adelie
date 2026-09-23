# adelie: specification

**Status:** draft · 2026-09-23 · tracked by `adelie-vnn`, `adelie-un4`

adelie is a pure-Rust columnar analytics store with SQL that explains its own data, for people
and agents alike. It runs in process as a library, behind `adelie serve` over HTTP, as an MCP
server, or in a browser on wasm. Its bytes live on local disk. Throw a table at it and it does
math fast; ask it what changed and it tells you where (§17). Its first specialised workload is
OpenTelemetry: ingest traces, logs, and metrics, render trace waterfalls, and find where a
regression is concentrated from the UI, the CLI, SQL, or an agent over MCP.

> _adelie_: the penguin. Small, fast, and at home in the cold.

---

## 1. Thesis and core commitments

**The gap.** Every Rust analytics database today sits on Arrow + DataFusion (a hundred-plus
crate graph and a general-purpose engine), and the fast engines that don't are large C++
trees (DuckDB, ClickHouse). Nothing is a small, fast, pure-Rust columnar SQL store that builds
in seconds, embeds as a normal dependency, and scales out on its own storage model.

**The idea.** Analytics engines return rows and leave the explaining to whoever reads them.
adelie also explains: which attributes account for a change (`elucidate()`), what a table
contains (`profile()`), and what a million log lines say (`patterns()`). These are deterministic
algorithms over the same aggregation machinery as `GROUP BY`, with no model inside the engine.
People and agents are equal users: every feature is SQL first and returns an ordinary table,
and the UI, CLI, and MCP are renderings of that table (§17.6).

**Our own versions, not copies.** adelie learns from ClickHouse and DuckDB but does not port
their features one to one. Each capability is designed for adelie's model: dynamic columns,
mergeable aggregate states, a single durability path, and results that serve both a person and
an agent (§16).

**The constraints are the product.** Three commitments every change is judged against.
Trading one away is a design change (a decision record and an issue first), never an
implementation detail.

1. **Speed.** Query speed, ingest speed, and build speed.
   - The lean library build (`--no-default-features`) builds clean in under 60s, and the
     default build in under 120s, both CI-enforced (D0004).
   - Performance claims are backed by published benchmarks, including where adelie loses.
2. **Testing.** Verify against the real artifact, never assume.
   - Every behaviour claim has a test that runs in CI.
   - A bug fix ships with a regression test shown to fail without the fix.
   - SQL correctness is checked differentially against an independent engine (§13).
3. **Stable.** Crash-safe, CRC-checked on-disk data, graceful resource exhaustion, and
   additive-only on-disk formats.
   - An acknowledged write survives a crash.
   - A query that exceeds its memory budget returns an error; it never takes the process down.

**Rules that follow from them:**

- Pure-Rust core. No bundled C/C++, no vendored OpenSSL, no `aws-lc`. A dependency is judged
  by build-and-ship cost: compile time, toolchain, binary size (D0004).
- No Arrow, no DataFusion, no Parquet in the core. adelie owns its format and its engine.
- `#![deny(unsafe_code)]`. Any exception is a single scoped site with its own decision record.
- The lean library build runs under Miri.
- **Compute is sync; IO is async where it pays.** Query execution is CPU-bound and runs on
  `std::thread::scope` workers over morsels (§7); it never blocks on an async runtime. IO that
  benefits from async uses it: the server (HTTP, OTLP, MCP), cluster networking and
  distributed query fan-out (§10), and the storage read path (io_uring where available). IO
  feeds batches to the compute workers through bounded queues. The lean library build keeps a
  blocking read path, so it stays runtime-free, Miri-checkable, and wasm-friendly.

---

## 2. Goals and non-goals

**Goals**

- Fast analytical SQL over columnar data: scans, filters, aggregates, joins, top-k.
- Wide, sparse, schema-on-write tables: a new column appears the first time a row carries it.
- OpenTelemetry as a first-class workload: OTLP ingest, trace waterfalls, log search.
- Analytics that explains itself: `elucidate()`, `profile()`, and `patterns()` (§17), for
  people and agents through the same SQL.
- Built-in MCP so an agent can explore and analyse data safely, locally and at scale.
- One architecture from a laptop to a cluster: scale is a quantity, not a mode.

**Non-goals** (each is a design change to revisit, not a deferral)

| Non-goal | Why |
|---|---|
| Object storage (S3, GCS) as primary storage | Local disk is the source of truth; object storage constrains the architecture |
| A write-ahead log | A write is acknowledged only once its segment is durable (§6). The proposed `ledger` engine is the one exception (§18) |
| Transactions, `UPDATE`, OLTP point workloads | Analytics store; writes are appends and predicate deletes. The proposed `ledger` engine is the one exception (§18) |
| Arrow / DataFusion / Parquet in the core | Build cost; adelie owns its format and engine. Parquet import/export may ship as a feature |
| Async compute | Operators are CPU-bound; async is for IO only (§1) |
| A cost-based optimizer | Rule-based planning over good statistics first (§8) |

---

## 3. Data model

- A **store** is a directory. It holds **databases**, which hold **tables**.
- A table is **fixed** (declared with `CREATE TABLE`, extra columns rejected) or **dynamic**
  (columns are added the first time a row carries them). OTel tables are dynamic.
- Every column is nullable. Absent and `NULL` read the same in SQL.
- A table has a **sort key** (the order rows are written within a segment) and an optional
  **partition key** (a time bucket for time-series tables). Both are declared at creation and
  fixed for the table's life.

**Types (v1)**

| Type | Notes |
|---|---|
| `BOOL` | |
| `INT64`, `UINT64` | narrower integers are an encoding detail, not a type |
| `FLOAT64` | IEEE; `NaN` sorts last, and SQL `=` never matches it |
| `DECIMAL(p, s)` | exact; precision 1–38, scale 0–p; a scaled 128-bit integer |
| `STRING` | UTF-8 |
| `BYTES` | opaque; trace and span ids |
| `TIMESTAMP` | UTC nanoseconds since the epoch; no time zone is stored |
| `DATE` | days since the epoch |
| `UUID` | 16 bytes; sorts bytewise, which matches canonical text order |
| `IP` | IPv4 or IPv6 in 16 bytes (IPv4 stored IPv4-mapped, shown dotted-quad), with CIDR containment |
| `LIST<T>` | homogeneous list of a non-list type |

There is no `ENUM` type (low-cardinality strings are dictionary-encoded automatically) and no
`MAP` or `JSON` type (dynamic columns are the map, §16.3). `STRUCT` is deferred. Representation
choices for DECIMAL, UUID, and IP: D0007.

**Ordering and equality.** The total order (sort, min/max, footer stats) is defined only
between two non-null values of the same kind, else `None`. FLOAT64: `-0.0 == 0.0`; `NaN` is
greater than `+inf` and equals itself *in the order*. DECIMAL compares exactly across scales
(`1.5` == `1.50`). STRING, BYTES, UUID, and IP compare bytewise. BOOL: false < true. LIST
compares lexicographically by element; a shorter prefix sorts first, and a NULL element sorts
after any non-null element and equals another NULL element; LIST columns report only a null
count in stats, no min/max. SQL equality is separate from the
order: NULL gives unknown, `NaN = NaN` is false, `-0.0 = 0.0` is true, and different kinds
give unknown (the binder casts before it compares).

**Fitting a column.** Writing a value into a typed column coerces losslessly and never parses
a string: `coerce(value, column_type) -> Option<Value>`.

- Null fits every type. The same kind fits as itself; DECIMAL rescales to the column's scale
  only when that is exact and within its precision. For LIST, every element must coerce.
- `INT64` → `UINT64` when non-negative. `UINT64` → `INT64` when ≤ `i64::MAX`.
- `INT64`/`UINT64` → `FLOAT64` when `|n| ≤ 2^53`; → `DECIMAL(p, s)` when `n·10^s` fits in p
  digits.
- `FLOAT64` → `INT64`/`UINT64` when finite, integral, and in range. `DECIMAL` →
  `INT64`/`UINT64` when integral and in range. `DATE` → `TIMESTAMP` (midnight UTC) when in
  range.
- Anything else is `None`, and the value is stored under `<name>::string` as its canonical
  text.

**Canonical text.** `Value::to_text` returns `None` for `NULL`. `BOOL` is `true`/`false`;
integers are decimal; `FLOAT64` is Rust `{}`, `NaN`, `inf`, or `-inf`. `DECIMAL` is exact with
exactly `scale` fractional digits (`1.50`). `STRING` is as-is; `BYTES` is `\x` followed by
lowercase hex. `TIMESTAMP` is `YYYY-MM-DDTHH:MM:SS[.fff|.ffffff|.fffffffff]Z`, using the
shortest exact fraction and nothing when there is none. `DATE` is `YYYY-MM-DD`; `UUID` is
lowercase 8-4-4-4-12; `IP` is `std::net::IpAddr` display, with IPv4-mapped shown as dotted
quad. `LIST` is `[e1, e2]`, where a `NULL` element shows as `NULL` and `STRING` elements are
single-quoted with `''` escaping.

**Dynamic columns and type conflicts.** Each segment stores only the columns its rows carry,
each with one type. Across segments a column keeps the type it was first created with; a
later value that does not fit is stored in a companion `<name>::string` column. Reading
`<name>` when a companion exists yields STRING: the primary's canonical text if the primary
is non-null, else the companion; the primary wins if both are set. With no companion,
`<name>` reads as its own type. A type conflict never rejects a row and never silently
rewrites existing data. (Open question Q2 refines this.)

---

## 4. Surfaces

The same store and semantics, reached four ways:

| Surface | What it is | Cargo feature |
|---|---|---|
| Library | `adelie::Store`, typed API plus `Store::sql` | lean core (`--no-default-features`) |
| CLI + server | `adelie` binary: `adelie sql`, `adelie serve` (HTTP, OTLP, UI) | `cli`, `serve` |
| MCP | `adelie mcp` (stdio) and `/mcp` on `adelie serve` | `mcp` |
| Browser | wasm32 with an OPFS-backed store in a dedicated worker | `wasm` |

- The default feature set ships the whole binary, so `cargo install adelie` produces it.
  `--no-default-features` is the storage-and-query core alone.
- **A feature ships whole:** core, HTTP, CLI, MCP, and docs in one PR.
- The store location is always the caller's choice: no default paths, no hidden directories.

---

## 5. On-disk format

A store directory:

```
<store>/
  manifest            current manifest (atomic rename target)
  manifest.<version>  retained prior manifests (bounded)
  lock                writer-exclusion lock
  <db>/<table>/
    <partition>/
      <segment-id>.seg   immutable segment files
      <segment-id>.idx   optional derived indexes (bloom, full-text); rebuildable
```

**Segment.** An immutable file of up to ~1M rows, split into **row groups** of ~64k rows (the
unit of pruning and of parallel work).

- Layout: header, column chunks, footer. The footer is a column directory: per column and row
  group, the encoding, byte range, row count, null count, min/max, and optional bloom-filter
  offset.
- Every column chunk and the footer carry a CRC32. A corrupt chunk is an error naming the
  segment and column, never a wrong answer.
- A reader decodes only the columns a query touches, and only the row groups its predicates
  cannot prune.

**Encodings** (pure Rust, chosen per chunk by the writer):

- Integers and timestamps: plain, delta, delta-of-delta (regular timestamps), frame-of-reference
  with bit-packing, RLE.
- Floats: plain, XOR against the previous value (slowly changing metrics).
- Strings: dictionary (low cardinality), plain with offsets, FSST (later).
- Booleans and null masks: bitmaps, RLE.
- An optional block compressor on top: `lz4_flex`. No zstd (C dependency).
- There are no per-column codec declarations: the writer picks per chunk by trying candidates
  on a sample. A table may pin an encoding for a column; skip structures are adaptive (§16.2).

**Manifest.** The one mutable object: the live segment set per table, each segment's partition,
row count, and column summary, the table schemas, and a monotonic version. It is CRC-checked
and published by write-to-temp, fsync, rename, fsync-directory. Publishing a manifest is the
commit point for every change.

**Format rule.** On-disk formats are additive only. A new encoding or manifest field must not
change how existing bytes are read. A version bump is one-way and recorded in a decision.

---

## 6. Writes and durability

**No WAL.** Rows are buffered in memory and made durable as a segment:

1. Incoming rows land in a per-table **write buffer**.
2. The buffer flushes when it reaches a row or byte limit, or when the flush interval elapses
   (default 250 ms, configurable).
3. Flush: encode and write the segment, fsync it, publish a new manifest.
4. Only then is every write in that flush **acknowledged**.

**Consequences, stated plainly:**

- An acknowledged write survives a crash. An unacknowledged one is lost, and the sender is
  expected to retry. OTLP exporters and the Collector already retry.
- Acknowledgement latency is roughly the flush interval plus an fsync. Concurrent writers share
  a flush (group commit).
- The write buffer is **queryable**: fresh rows appear in query results before they are
  durable.
- A retry after a crash can duplicate rows. OTel tables deduplicate on
  `(trace_id, span_id)` at compaction; general tables do not deduplicate (open question Q5).

**Deletes.** `DELETE FROM t WHERE <predicate>` records a tombstone predicate in the manifest.
Scans apply it; compaction materialises it and drops the tombstone. Retention (TTL) drops whole
partitions.

**Compaction.** Small segments are merged into larger ones per partition (tiered by size and
age), re-sorted by the sort key, tombstones applied, derived indexes rebuilt. Compaction
publishes one manifest swapping inputs for outputs; old files are deleted only after no reader
can still hold a manifest naming them.

**Concurrency.** One writer process per store (the `lock` file). Any number of reader processes
open a manifest snapshot and read without locks. A reader refreshes by re-reading the manifest.

---

## 7. Query execution

- **Vectorized.** Operators process column batches of ~4k rows. Kernels are plain safe Rust
  written so the compiler vectorizes them (chunked loops, no bounds checks in the inner loop).
- **Morsel-driven parallelism.** A scan splits into row-group morsels; a pool of
  `std::thread::scope` workers pulls morsels and runs the pipeline to a partial result.
- **Partial and final aggregation, always.** Every aggregate has a partial state and a merge.
  The same path serves many cores on a laptop and many nodes in a cluster (§10).
- **Operators (v1):** scan (projection, predicate pushdown, zone-map and bloom pruning), filter,
  project, hash aggregate, hash join (equi, build/probe), sort, top-k, limit, union all.
- **Memory budget.** Each query has a byte budget (default configurable, per query overridable).
  Operators account their allocations. Exceeding the budget returns a clear error. Spilling to
  disk is deferred.
- **Cancellation.** Every query takes a cancel token, checked between batches, and a timeout.

---

## 8. SQL

adelie's SQL is a real engine, bounded on purpose. Anything outside this section is a design
change.

**Front end.** A hand-rolled lexer and recursive-descent parser with an explicit nesting-depth
cap, so hostile or accidental input returns an error, never a stack overflow. No `sqlparser`
dependency.

**Dialect.** PostgreSQL-shaped syntax, with the analytics functions this workload needs.

**Statements (v1)**

| Statement | Notes |
|---|---|
| `SELECT` | `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, `LIMIT`/`OFFSET`, `DISTINCT`, `WITH` (CTEs), `UNION ALL`, subqueries in `FROM` |
| Joins | `INNER` and `LEFT` equi-joins |
| `CREATE TABLE` / `DROP TABLE` | fixed or dynamic, with sort and partition keys |
| `INSERT ... VALUES` / `INSERT ... SELECT` | durable when it returns (§6) |
| `COPY t FROM '<path>'` | CSV and NDJSON |
| `DELETE ... WHERE` | tombstone predicate (§6) |
| `EXPLAIN` | the plan, with pruning estimates |
| `CREATE ROLLUP` / `DROP ROLLUP` | pre-aggregation maintained by the store (§16.1) |
| Window functions | `OVER (PARTITION BY … ORDER BY … ROWS/RANGE …)`: `row_number`, `rank`, `lag`, `lead`, running aggregates |
| Table functions | `elucidate()`, `profile()`, `patterns()` (§17), files in place (§16.4) |

adelie's own SQL ergonomics (files in place, `LIMIT n PER`, `FILL`, `NEAREST JOIN`, path
wildcards) are in §16.4. Excluded in v1: `UPDATE`, transactions, correlated subqueries.
Deferred: scalar subqueries, non-equi joins other than `NEAREST JOIN`.

**Functions (v1).** Arithmetic, comparison, boolean, `CASE`, `CAST`, string basics
(`lower`, `upper`, `length`, `substr`, `LIKE`, `ILIKE`, `regexp_match`), time
(`date_trunc`, `time_bucket`, `now`, extraction), and aggregates: `count`, `sum`, `avg`,
`min`, `max`, `approx_count_distinct` (HyperLogLog), `quantile(x, q)` and `approx_quantile`
(DDSketch: mergeable, relative-error, right for latency), `top_k`, `arg_min`/`arg_max`,
`list_agg`, `histogram`. Any aggregate takes a standard `FILTER (WHERE …)` clause. Every
aggregate has a mergeable state, which rollups (§16.1) and `elucidate()` (§17.1) build on.

**Planner.** Rule-based: projection pruning, predicate pushdown, partition and row-group pruning
from manifest and footer stats, constant folding, partial/final aggregate split, join-side
selection by estimated row count. No cost-based optimizer.

---

## 9. OpenTelemetry

**Ingest.** OTLP over HTTP (protobuf and JSON) in v1; OTLP over gRPC behind a `grpc` feature.
The OTLP protobuf schema is fixed, so decoding is a hand-rolled, allocation-light decoder, not a
`prost` dependency.

**Tables.** Created on first ingest, all dynamic:

| Table | Sort key | Notes |
|---|---|---|
| `otel.spans` | `service.name`, `start_time` | partitioned by hour; `trace_id` bloom per row group |
| `otel.span_events` | `trace_id`, `time` | events and links, keyed by `(trace_id, span_id)` |
| `otel.logs` | `service.name`, `time` | full-text index on `body` |
| `otel.metrics` | `metric.name`, `time` | gauge, sum, histogram; exponential histogram later |

Attributes flatten to columns: resource attributes to `resource.<key>`, span and log
attributes to `attributes.<key>`. Dynamic columns (§3) make sparse attributes ordinary columns.

**Trace functions.**

- `trace(<trace_id>)`: a table function returning the trace's spans with `depth`,
  `parent_span_id`, `self_time`, and `on_critical_path`. It is what the waterfall renders.
- `service_graph(<from>, <to>)`: caller → callee edges with call counts, error counts, and
  latency quantiles.

**Log search.** A per-segment inverted index over `otel.logs.body` (tokenise, fold, BM25).
SQL reaches it through `MATCH(body, '<query>')` in `WHERE`, and `score()` in the select list.
The index is derived and rebuildable; a missing or corrupt index falls back to a scan.

**Log patterns.** Every log line gets a `pattern_id` at ingest (§17.3), so `patterns()` turns a
million lines into a short list of templates with counts.

**Run tagging.** A `run.id` resource attribute is a first-class filter, so a person or an agent
can tag a test run and query exactly its own telemetry. Comparing two runs is `compare_runs()`
(§17.4).

---

## 10. Scaling out (proposed)

Local disk stays the source of truth. adelie scales **shared-nothing**: each node owns its data
on its own disk.

- **Shards.** A table is split by (partition, hash of a shard key) into shards. Each shard has
  one leader that accepts writes and zero or more replicas.
- **Replication by segment shipping.** Segments are immutable, so a replica copies new segment
  files and adopts the leader's manifest version. No log replay, no row-level replication.
- **Distributed query.** A coordinator plans, sends each shard's fragment to a node holding it,
  and merges partial results (§7). The single-node engine is the per-shard engine, unchanged.
- **Ack policy.** A write is acknowledged after the leader's flush, or optionally after R
  replicas hold the segment.
- **Membership and shard map.** Open question Q1. Static configuration first, consensus later.

Phasing: single node → replication → sharding and distributed query. Each phase is additive
over the same segment and manifest format.

---

## 11. MCP

Built in, not bolted on: `adelie mcp` over stdio, and `/mcp` on `adelie serve`.

**Tools (v1)**

| Tool | Returns |
|---|---|
| `list_tables` | tables, row counts, time ranges |
| `describe_table` | columns, types, and how often each column is present |
| `sample` | a few rows, token-bounded |
| `sql` | a read-only query, result shaped to a budget |
| `get_trace` | a compact span tree for one trace |
| `find_errors` | error spans and logs for a service and window, grouped |
| `top_slow` | slowest operations or endpoints for a window |
| `elucidate` | which attributes account for a change in a metric (§17.1) |
| `profile` | a table's data card (§17.2): the first call an agent should make |
| `patterns` | log templates with counts for a window (§17.3) |
| `compare_runs` | `elucidate` plus new and vanished log patterns between two runs (§17.4) |
| `save_finding` | records a finding with the SQL that verifies it (§17.5) |

**Guardrails.**

- Read-only by default; writes need an explicit server flag.
- Every call has a row cap, a byte cap, a timeout, and the query memory budget (§7).
- Results are shaped to a **token budget**: summaries, histograms, and top-k instead of raw rows,
  with a note saying what was truncated.
- Errors help an agent correct itself: an unknown column names the closest real ones.
- Every MCP tool compiles to the same typed API as SQL. There is no second execution path.

---

## 12. UI

`adelie serve` embeds a small web UI: a SQL console, a trace search, and the trace waterfall.
Every chart and waterfall carries **Elucidate**: select a spike or a slow span and see the ranked
findings, each with before/after and a link to the query that verifies it. Tables open on their
`profile()`; logs have a **Patterns** tab; saved findings are listed beside the SQL console.
Assets are compiled into the binary; no separate deploy. The UI's stack is open question Q4, and
it must not break the default build budget.

---

## 13. Testing

- **Unit tests** beside the code; **one e2e test binary** that drives the real `adelie` binary.
- **SQL logic tests** in the `sqllogictest` file format, run in CI.
- **Differential testing.** A quarantined `bench` workspace crate runs the same queries against
  DuckDB and compares results. DuckDB never enters adelie's own build.
- **Crash tests.** Kill the process during flush and compaction; reopen; assert every
  acknowledged row is present and no unacknowledged state is visible.
- **Fuzzing** of the SQL parser, the OTLP decoder, and the segment decoder.
- **Miri** over the lean library build. A test Miri cannot run carries
  `#[cfg_attr(miri, ignore)]` with its reason.
- **Benchmarks.** ClickBench for general analytics, and an OTel-shaped suite (ingest rate,
  trace lookup, log search, aggregate latency). Numbers are published with the losses.

---

## 14. Module layout (target)

```
src/
  lib.rs        public API (Store, Table, sql), #![deny(unsafe_code)]
  types/        type system, values, casts
  format/       segment writer and reader, encodings, CRC framing
  manifest/     manifest codec, publish, snapshots, retention
  store/        tables, write buffer, flush, compaction, deletes, rollups, lock
  exec/         batches, kernels, operators, morsel scheduler, memory budget
  sql/          lexer, parser, binder, planner
  fts/          tokeniser, inverted index, BM25
  analysis/     elucidate, profile, patterns, findings (§17)
  otel/         OTLP decoder, table mapping, trace functions
  cli/          `adelie` binary            (feature: cli)
  server/       HTTP, OTLP endpoints, UI    (feature: serve)
  mcp/          MCP tools and guardrails    (feature: mcp)
```

---

## 15. Open questions

| # | Question |
|---|---|
| Q1 | Cluster membership and shard map: static config, Raft, or an external coordinator? |
| Q2 | Dynamic column type conflicts: is the `::string` companion column the right rule? |
| Q3 | PostgreSQL wire protocol for BI tools: v1 or later? |
| Q4 | UI stack: plain JS with no build step, or a prebuilt bundle? |
| Q5 | Deduplication of retried writes for general tables: idempotency keys, or none? |
| Q6 | Parquet import/export: which dependency, and does it fit the build budget? |
| Q7 | Rollup matching: which query shapes may the planner answer from a rollup (§16.1)? |
| Q8 | Browser mode threading: single-threaded executor, or wasm threads where available? |
| Q9 | `elucidate()` significance: which test, and how is it corrected across many candidates? |
| Q10 | Log patterns for multi-line logs (stack traces): one pattern per entry, or per first line? |
| Q11 | Adaptive skip structures: what query-log evidence builds one, and what drops it (§16.2)? |
| Q12 | ~~Table engines (§18): where exactly is the engine trait boundary, and must it be settled before the segment format (§5) ships?~~ Answered (§18): the format ships first with three hooks; the boundary is fixed with the store (E4). |
| Q13 | `latest`: is "newest" the commit sequence, a declared version column, or both? |
| Q14 | `ledger`: its own crate and product, or an engine inside adelie? |

---

## 16. Analysis features: adelie's own versions

Each of these answers a need ClickHouse or DuckDB also answers, designed for adelie's model
instead of ported.

### 16.1 Rollups

A **rollup** is a pre-aggregated view of a table, declared once and maintained by the store:

```sql
CREATE ROLLUP spans_1m ON otel.spans
  GROUP BY service.name, attributes.http.route, time_bucket('1m', start_time)
  AGGREGATE count(), count() FILTER (WHERE status = 'error'), approx_quantile(duration, 0.99);
```

- **Written in the same flush as the base rows.** A rollup's partial aggregate states are
  encoded into their own segment and published in the same manifest as the rows they summarise
  (§6). A rollup is never stale and never ahead of its table; there is no second write path.
- **Merged at compaction.** Compaction merges rollup states the way it merges rows.
- **Used without being named.** The planner answers a query from a rollup when the query's
  grouping, filters, and aggregates are a coarsening of it (Q7). Nobody writes `FROM spans_1m`.
- **Its own retention.** A rollup may outlive its base rows: raw spans for 7 days, the rollup
  for a year.
- `elucidate()` and `profile()` read rollups when they cover the question.

### 16.2 Adaptive pruning

- Encodings are chosen per chunk by the writer (§5); nobody declares codecs.
- Min/max stats are always kept. Other **skip structures** (bloom filter, value set, n-gram
  index) are built per column at compaction when the query log shows predicates on that column
  and its profile (§17.2) says the structure would prune. Unused ones are dropped (Q11).
- The store tunes its own pruning from its own query log. A table may pin a structure; `EXPLAIN`
  shows which structures pruned what.

### 16.3 Dynamic columns are the map

- There is no `MAP` or `JSON` type. Attributes are real columns (§3), addressed by path:
  `attributes.http.route`.
- **Wildcards over paths:** `SELECT attributes.http.*` selects every column under that path;
  `SELECT * EXCLUDE (resource.*)` drops a subtree. The same wildcard works in `GROUP BY` and in
  `elucidate(by => …)`.
- NDJSON ingest flattens nested objects into paths the same way.

### 16.4 SQL ergonomics

| Feature | Example |
|---|---|
| Files in place | `FROM 'logs/*.ndjson' WHERE level = 'error'`: a file glob is a table (CSV, NDJSON) |
| `FROM`-first | `FROM otel.spans SELECT count()` |
| `GROUP BY ALL` | group by every non-aggregate in the select list |
| `EXCLUDE` | `SELECT * EXCLUDE (body)` |
| Top n per group | `ORDER BY duration DESC LIMIT 3 PER service.name` |
| Gap filling | `GROUP BY time_bucket('1m', time) FILL 0` (or `FILL previous`, `FILL linear`) |
| As-of join | `NEAREST JOIN deploys d ON s.service = d.service AND s.time >= d.time` |
| Lists | `list_map(xs, x -> x * 2)`, `list_filter`, `unnest` |

### 16.5 adelie observes itself

adelie emits its own telemetry (each query as a trace with a span per operator, flushes and
compactions as spans, engine metrics) into `adelie.*` tables in the OTel shape. The waterfall
renders adelie's own query plans, and `elucidate()` works on adelie's own slowdowns.
`adelie.tables`, `adelie.segments`, and `adelie.query_log` describe the store.

---

## 17. Explaining the data, for people and agents

### 17.1 `elucidate()`

Compares two populations of rows and ranks the column values that account for the difference
in a metric.

```sql
SELECT * FROM elucidate(
  metric   => 'approx_quantile(duration, 0.99)',
  source   => 'otel.spans',
  where    => 'service.name = ''api''',
  baseline => 'start_time BETWEEN ''10:00'' AND ''11:00''',
  target   => 'start_time BETWEEN ''11:00'' AND ''12:00''',
  by       => 'attributes.*, resource.*'
);
```

`target => 'duration > 2s', baseline => 'REST'` compares a selection with everything else.

**How it runs:**

1. **Candidates.** The profile (§17.2) drops identifiers (near-unique columns), constants, and
   columns that are almost always absent. Usually tens of columns remain out of thousands.
2. **One scan.** Over the two windows, reading only candidate columns, build a mergeable
   aggregate state per (column, value) for each population: counts and sums, plus a quantile
   sketch for percentiles. Values per column are capped with a heavy-hitters sketch and the
   remainder is an `other` bucket, so memory is bounded. It is parallel and distributable
   because it is the partial/final aggregation of §7.
3. **Scoring.**
   - Additive metrics (count, sum, error count): contribution is Δ(value) / Δ(total).
   - Ratios (error rate, average): the change is split into **mix** (traffic moved toward the
     value) and **rate** (the value itself got worse). Both are reported.
   - Percentiles: which values are over-represented among target rows above the baseline's
     percentile, scored by lift and coverage.
4. **Noise.** A minimum support and a significance test drop thin findings (Q9). Values that
   always co-occur are reported as one finding.
5. **Combinations.** Only the top single findings are combined, two or three at a time, by a
   bounded beam search.
6. **Result.** An ordinary table: `finding`, `columns`, `values`, `contribution`, `mix`, `rate`,
   `before`, `after`, `support`, `verify_sql`. Every row carries the SQL that reproduces it.

**Limits, stated plainly.** It finds where a change is concentrated, not why. It needs enough
rows on both sides. A change spread evenly across everything returns no dominant finding, which
is itself an answer (often an infrastructure-wide cause).

### 17.2 `profile()`: data cards

`profile('otel.spans')` returns one row per column: type, presence, cardinality, a distribution
sketch, top values, detected meaning (duration, identifier, timestamp, enum-like, URL, IP),
candidate join keys, and drift against the previous period. It is maintained at compaction from
stats the writer already computes, so it answers instantly. It powers `elucidate()`'s candidate
selection and adaptive pruning (§16.2).

### 17.3 `patterns()`: log templates

Each log line is templated at ingest by a deterministic parse-tree algorithm (Drain-style), per
service: `user 123 timed out after 5s` becomes `user <*> timed out after <*>`. The template's
`pattern_id` is stored as a column. `patterns(source, where)` returns templates with counts,
first and last seen, and an example line; a pattern first seen in a window is flagged new (Q10).

### 17.4 Comparing runs

`compare_runs(a, b)` is `elucidate()` with `run.id = a` and `run.id = b` as the two sides, plus
the log patterns that appeared or vanished. It is a composition of §17.1 and §17.3, not a
separate engine: the dev-loop question "what did my change do?" in one call.

### 17.5 Findings

`adelie.findings` holds findings saved by people (the UI's **Save**) and agents
(`save_finding`): a title, the result row, the `verify_sql`, who saved it, and when. Re-running
a finding re-checks it against current data. It is memory that people and agents share.

### 17.6 One result, four renderings

| Surface | Renders the result as |
|---|---|
| SQL / library | a table: filter it, join it, save it as a view |
| CLI | `adelie elucidate …`, `adelie profile …`, `adelie patterns …`: a ranked table in the terminal |
| UI | **Elucidate** on any chart or span, profiles on every table, a **Patterns** tab on logs |
| MCP | the same rows, compacted to the caller's token budget, with truncation stated |

No surface gets a feature the others lack. Token budgets and read-only defaults are MCP
guardrails (§11), not a separate feature set.

---

## 18. Table engines (proposed)

A table's **engine** decides its layout and what compaction does to its rows. adelie, nidus,
and a transactional store share one core and differ only in engines. Tracked by `adelie-goi`.

**The split.** Modelled on PostgreSQL's table access methods, not MySQL's storage engines.

- **The core owns commits.** A commit is a manifest publish (§6). A commit records the
  manifest version it read, and the publish fails if a conflicting commit landed since
  (optimistic concurrency), so one commit can span several tables atomically.
- **The core owns** snapshots, the type system and batches (§3, §7), the segment format (§5),
  and the query layers (SQL, vector, full-text).
- **An engine owns** its layout, its merge policy, and how a scan resolves rows that have not
  been merged yet.
- MySQL is the counter-example: transactions live in each engine there, so the engine API
  shrinks to what every engine supports, and a transaction across engines needs two-phase
  commit.

**Engines.** Each is named for what it does to rows, never for its layout, so a layout can
change without a rename.

| Engine | Rows | Merge policy |
|---|---|---|
| `append` | every row kept | none; compaction only re-sorts and applies tombstones. The default, and adelie today |
| `latest` | newest row per key | keeps the row with the highest commit sequence (or a declared version column) per key |
| `rollup` | one row per key | folds mergeable aggregate states (§8); what `CREATE ROLLUP` (§16.1) is built on |
| `vector` | every row kept | maintains a nearest-neighbour index over an embedding column (nidus) |
| `ledger` | rows updated in place | row-oriented, with a write-ahead log; `UPDATE` and multi-statement transactions |

```sql
CREATE TABLE users (id UINT64, email STRING, seen TIMESTAMP)
  ENGINE = latest KEY (id);
```

**Rules.**

- `latest` and `rollup` resolve at read time as well as at compaction: a scan merges unmerged
  segments by key, so a query never sees two versions of one key.
- The key of `latest` and `rollup` is a prefix of the sort key, so a merge is a streaming pass
  over sorted runs.
- `ledger` is the only engine with a WAL. Its WAL is private to the engine and checkpoints into
  ordinary segments and a manifest publish, so a reader sees one snapshot model everywhere.
  Every other engine keeps §6's no-WAL path.
- An engine never changes the segment format for the others. What an engine needs on disk is
  added to §5 under the format rule (additive only).

**What the segment format owes the engines.** This answers Q12: the segment format (§5) ships
before the engine boundary is fixed, but with three hooks, because none can be added
afterwards without a format version bump.

1. **Extensible column types.** A footer column descriptor is a logical type id plus
   parameters, not a closed enum. A reader that meets an unknown type fails with an error
   naming it; it never guesses. This is how `rollup`'s aggregate states and `vector`'s
   `FLOAT32` vectors get stored.
2. **A generic index directory.** Each derived index (bloom, set, n-gram, full-text, and a
   nearest-neighbour index) is an entry of kind, columns, byte range, and CRC, in the footer
   or the `.idx` file. Readers skip kinds they do not know.
3. **Segments stay engine-agnostic.** Per-segment commit sequence, the table's engine, and
   `ledger`'s deletion vectors live in the manifest and its side files, never in a segment.
   `latest` needs no per-row version: one flush is one commit, and duplicates of a key within
   a flush are resolved at flush time.

**The engine boundary is fixed with the store** (E4), where commits, compaction, and scans are
built. An engine supplies: flush (buffered rows to segments), its merge policy (compaction
inputs to outputs), scan resolution over unmerged segments, and optional index builders. The
core keeps the manifest, commits and their conflict check, snapshots, retention, and the
planner.

**Consequences, stated plainly.**

- `ledger` reverses two non-goals in §2 (a WAL; transactions and `UPDATE`) for one engine.
  Accepting it is a design change: a decision record first.
- `vector` needs a fixed-length `FLOAT32` vector type, which §3 does not have yet.
- Optimistic multi-table commits relax §6's one-writer-per-store rule only within the writer
  process. Several writer processes remain out of scope.

Open questions Q13–Q14.
