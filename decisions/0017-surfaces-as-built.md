# D0017: Surfaces as built

**Status:** accepted · 2026-10-05
**Rule:** The CLI, MCP and HTTP surfaces use widely used crates for everything that is not the
core problem, all optional behind `cli`/`mcp`/`serve`, so the lean build stays dependency-free.
Every surface runs SQL through one path, read-only by default over a lock-free `Reader`.

## Why

- **Libraries, not hand-rolled infrastructure.** The maintainer's rule: effort goes to storage,
  query and SQL. HTTP (axum on tokio), MCP (`rmcp`, the official Rust SDK), arguments (clap),
  JSON (serde_json) and rendering (csv, comfy-table) are solved problems. The hand-rolled SQL
  front end (D0016) stays: SQL *is* the core problem.
- **Measured against D0004 before any code.** The full stack is 118 crates, no `-sys`, no C, no
  TLS. A clean local build of the stack took 9.4s; the lean build is unchanged and has an
  empty `cargo tree -e normal`. CI now times the default build too (120s).
- **Read-only by default, lock-free.** SPEC §6 promises any number of reader processes and
  SPEC §11 makes read-only the default. `sql::execute_read` runs the same `execute_query` over a
  `View` from `Reader`, and rejects a write statement with `SqlError::ReadOnly` before running
  anything. So `adelie sql` and `adelie mcp` work beside `adelie serve --allow-writes`, which
  holds the writer lock. Writes need `--write` (CLI) or `--allow-writes` (MCP, serve).
- **One PR for all three surfaces** (maintainer's call), so SPEC §4's "a feature ships whole"
  holds from the first surface on: every later feature owes CLI, HTTP and MCP together.

## Consequences

- Features: `cli` (clap, csv, comfy-table, serde, serde_json); `mcp` = `cli` + rmcp + tokio;
  `serve` = `mcp` + axum. `default` is all three. Async is IO only: every query runs in
  `spawn_blocking` on the sync morsel scheduler (SPEC §1).
- `src/surface` is the shared layer: `Handle` (Reader or Store), `value_json` (lossless: ints and
  floats as numbers, DECIMAL/TIMESTAMP/DATE/UUID/IP/BYTES as canonical text), and `shape`
  (row and byte caps that never cut a row, with a note saying what was dropped).
- MCP v1 ships `list_tables`, `describe_table`, `sample` and `sql`. The other SPEC §11 tools wait
  for E9 (analysis) and E10 (OTel) and are not advertised. Defaults: 200 rows, 64 KiB, 30s,
  256 MiB per call. Every failure is a tool error, never a dropped session.
- `adelie serve`: `GET /health`, `POST /query`, `/mcp` (streamable HTTP over the same MCP
  service). 1 MiB body limit (413). A write is acknowledged only after `Store` returns, which
  is after the durable flush (SPEC §6). A retried write can duplicate rows (Q5 is open).
- Unknown table or column errors name up to three close matches (`did you mean "name"?`).
- `View::tables()` is the catalog read: row counts from segment stats plus the buffer
  (`rows_exact` is false once a table has tombstones), per-column non-null counts, and the
  first TIMESTAMP column's range. No data is scanned.
- `just ci` stays lean; `just ci-full` checks the default build. CI's clippy and test jobs
  already run both.

## Evidence

- `src/surface`, `src/cli`, `src/mcp`, `src/server`; `src/sql/execute.rs` (`execute_read`,
  `is_read_only`); `src/storage/catalog.rs`.
- `tests/e2e/cli.rs`, `tests/e2e/mcp.rs`, `tests/e2e/serve.rs` drive the real binary.
- `.github/workflows/ci.yml` `build-budget`: both timers.
