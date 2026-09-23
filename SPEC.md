# adelie: specification

**Status:** draft · 2026-09-22 · tracked by `adelie-vnn`

adelie is a pure-Rust columnar analytics store with SQL. It runs in process as a library,
behind `adelie serve` over HTTP, as an MCP server, or in a browser on wasm. Its bytes live
on local disk. Throw a table at it and it does math fast. Its first specialised workload is
OpenTelemetry: ingest traces, logs, and metrics, render trace waterfalls, and let an AI agent
query all of it through built-in MCP.

> _adelie_: the penguin. Small, fast, and at home in the cold.

---

## 1. Thesis and core commitments

**The gap.** Every Rust analytics database today sits on Arrow + DataFusion (a hundred-plus
crate graph and a general-purpose engine), and the fast engines that don't are large C++
trees (DuckDB, ClickHouse). Nothing is a small, fast, pure-Rust columnar SQL store that builds
in seconds, embeds as a normal dependency, and scales out on its own storage model.

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
- Built-in MCP so an agent can explore and analyse data safely, locally and at scale.
- One architecture from a laptop to a cluster: scale is a quantity, not a mode.

**Non-goals** (each is a design change to revisit, not a deferral)

| Non-goal | Why |
|---|---|
| Object storage (S3, GCS) as primary storage | Local disk is the source of truth; object storage constrains the architecture |
| A write-ahead log | A write is acknowledged only once its segment is durable (§6) |
| Transactions, `UPDATE`, OLTP point workloads | Analytics store; writes are appends and predicate deletes |
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
| `FLOAT64` | IEEE; `NaN` sorts last and never equals itself |
| `STRING` | UTF-8 |
| `BYTES` | opaque; trace and span ids |
| `TIMESTAMP` | UTC nanoseconds since the epoch; no time zone is stored |
| `DATE` | days since the epoch |
| `LIST<T>` | homogeneous list of a scalar type |

Decimal, map, and struct types are deferred.

**Dynamic columns and type conflicts.** Each segment stores only the columns its rows carry,
each with one type. Across segments a column keeps the type it was first created with; a
later value that does not fit is stored in a companion `<name>::string` column, and reading
`<name>` coalesces the two. A type conflict never rejects a row and never silently rewrites
existing data. (Open question Q2 refines this.)

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

- Integers and timestamps: plain, delta, frame-of-reference with bit-packing, RLE.
- Strings: dictionary (low cardinality), plain with offsets, FSST (later).
- Booleans and null masks: bitmaps, RLE.
- An optional block compressor on top: `lz4_flex`. No zstd (C dependency).

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

Excluded in v1: `UPDATE`, transactions, correlated subqueries. Deferred: window functions,
scalar subqueries, non-equi joins.

**Functions (v1).** Arithmetic, comparison, boolean, `CASE`, `CAST`, string basics
(`lower`, `upper`, `length`, `substr`, `LIKE`, `ILIKE`, `regexp_match`), time
(`date_trunc`, `time_bucket`, `now`, extraction), and aggregates: `count`, `sum`, `avg`,
`min`, `max`, `approx_count_distinct` (HyperLogLog), `quantile(x, q)` and `approx_quantile`
(DDSketch: mergeable, relative-error, right for latency).

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

**Run tagging.** A `run.id` resource attribute is a first-class filter, so an agent can tag a
test run and query exactly its own telemetry.

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
| `compare_windows` | what changed between two time windows (rates, latency, errors) |
| `top_slow` | slowest operations or endpoints for a window |

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
  store/        tables, write buffer, flush, compaction, deletes, lock
  exec/         batches, kernels, operators, morsel scheduler, memory budget
  sql/          lexer, parser, binder, planner
  fts/          tokeniser, inverted index, BM25
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
| Q7 | When do window functions land? |
| Q8 | Browser mode threading: single-threaded executor, or wasm threads where available? |
