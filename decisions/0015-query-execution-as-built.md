# D0015: Query execution as built

**Status:** accepted · 2026-09-24
**Rule:** Query execution is a pure library engine in `src/exec` driven by a physical `Plan`,
with storage behind `TableSource`. Aggregate partial states and their encodings are frozen,
because rollups persist them. The library API comes first.

## Why

- SPEC §4 "ships whole" applies once a surface exists. There is no SQL until E6 (D0013's
  precedent, verbatim reasoning).
- `src/exec` stays pure and Miri-clean (SPEC §6, §13). Storage owns IO and implements
  `TableSource` for `View`.
- The scan prunes but never filters. The executor always filters after a predicated scan, so a
  pruning bug can only cost speed, never correctness.
- Hand-rolled sketches: no dependency (D0004). This was the maintainer's scope call.

## Consequences

- **Frozen encodings** (copy the layouts from the code's doc comments; changing any of them is a
  new version byte, never an edit):
  - `kernels::encode_row_key`: per value, tag `0` = NULL else `1` + little-endian value (FLOAT64
    normalises −0.0→0.0 and every NaN to `f64::NAN` first); STRING/BYTES get a `u32` length
    prefix, LIST a `u32` count then its elements;
  - `stable_hash` = `xxh64(seed 0)` over `encode_row_key(&[col], row, Group)`;
  - HLL p = 14 (16 384 one-byte registers), sparse exact `Vec<u64>` below 64 distinct hashes,
    promoted to dense registers at 64;
  - DDSketch α = 0.01, γ = (1+α)/(1−α), bucket key `ceil(ln(x)/ln(γ))` for x > 0, mirrored for
    x < 0 off `−x` in a separate store, `x == 0.0` goes to `zero_count`; sparse `BTreeMap` stores;
  - Space-Saving capacity `max(64, 8·k)`, keyed by `encode_row_key` bytes; new key under capacity
    is `(count 1, error 0)`, at capacity replaces the minimum-count entry with
    `(min.count + 1, min.count)`;
  - every basic accumulator's v1 state body starts with a version byte (`STATE_VERSION = 1`,
    checked by `check_version`), then its own fields (e.g. `Count`: uvarint count; `Sum`: a
    `has`-byte then `i128`/`f64` bits; `Avg`: `has`-byte, sum, uvarint count; `ListAgg`/
    `Quantile`: uvarint length then each encoded value/optional value; `Histogram`: its
    value→count map).
- **Semantics chosen that E6's DuckDB differential may revisit:**
  - INT64 `/` truncates;
  - `/` and `%` by zero give NULL;
  - `sum(INT64)` overflow is an error (not HUGEINT);
  - `avg` is FLOAT64;
  - `quantile` is discrete, lower nearest-rank `floor(q·(n−1))`;
  - `list_agg` is LIST including NULLs;
  - `histogram` emits STRING in DuckDB MAP text form until a MAP/STRUCT type exists. Plan-gate
    answer: STRING in DuckDB MAP text form (`{v1=c1, v2=c2}`); the state is an exact value→count
    map; the output type changes when MAP lands.
- **Morsel granularity:** plan-gate answer: one morsel per segment (plus one per buffered
  batch), because `segment::Reader::open` needs the whole file; row groups are pruned inside the
  morsel. Follow-up: adelie-1st.2 (footer-only open, row-group morsels).
- **Row order without ORDER BY** is unspecified once `threads > 1`. `View::scan` is sequential
  and keeps manifest order. Flag it for adelie-zit.1 (`latest`), which must not rely on arrival
  order across morsels.
- `Engine::resolve` is now on the read path. The scan honours `ScanPlan.segments`; `merge_key`
  is adelie-zit.1's.
- Tombstones apply at scan time via `tombstones_for`. Buffered rows need none, because `delete`
  flushes first. Compaction and migrations still read unfiltered through `read_segment_as`.
- **Budget:**
  - `ExecOptions` default 1 GiB, threads = `available_parallelism`, no timeout;
  - sinks, the join table and in-flight segment bytes reserve, and streaming operators do not;
  - there is no spill (SPEC §7 defers it);
  - no store-level default yet: E6's session settings will carry one.
- **Cancel and timeout:** `ExecContext::check` returns `Cancelled` first, then `Timeout` once
  `Instant::now() >= deadline`; called before each morsel and between batches. A zero timeout
  fails at the first check (deliberate; tests rely on it).
- Flush and compaction ORDER BY now run on `exec::ops::sort_batches`, proven identical to the
  old `sort_rows` by an in-test oracle (`reference_sort_rows`, a verbatim copy of the pre-E5
  implementation) checked with randomised cross-checks.

## Notes for the merge thread

At the time this record was written, this worktree held only groups 1–3 (U1–U10) plus this
bookkeeping merge; U11 (`execute.rs` body, `storage/query.rs`) and U12 (e2e tests) had not yet
landed here. Everything above was verified against the code as built in groups 1–3 and the
pinned contract in the root blueprint for the not-yet-merged group 4 units; nothing here depends
on U11/U12 changing before merge.
