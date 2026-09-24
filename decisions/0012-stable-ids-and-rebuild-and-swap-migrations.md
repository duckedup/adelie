# D0012: Stable table and field ids; every migration is rebuild and swap

**Status:** accepted · 2026-09-23
**Rule:** Tables and columns have stable ids that are never reused, and names are labels. A
table's engine, `KEY`, and `VERSION` are fixed for its life. Every other shape change is a
migration: rebuild from a snapshot, reusing still-valid segments, then swap in one commit.

## Why

- **One model.** Users learn one thing: a migration is a job with `EXPLAIN`, progress, an atomic
  swap, and `REVERT`. One code path gets crash-tested.
- **Reuse keeps it cheap.** `ADD COLUMN` and `DROP COLUMN` rewrite nothing, so the common case
  stays instant without a second mechanism.
- **Ids make renames safe.** With field ids, a rename is instant, and a column dropped and
  re-added under the same name never resurfaces the old data (Iceberg's lesson). With table ids
  in paths, rename, swap, `REVERT`, and `UNDROP` are each one manifest commit.
- **The engine and `KEY` decide which rows survive.** Changing them loses data
  (`append` → `latest`) or cannot restore it (`latest` → `append`), so no migration may. The user
  copies to a new table instead.
- The maintainer asked for the long-term design now, not a v1 to revise later.

## Consequences

- SPEC §5's layout becomes `<db>/<table-id>/`, and each manifest segment entry records its own
  directory and its columns' field ids. Both additions follow the format rule and are additive
  to D0009's manifest. A manifest written before them gets ids assigned when it is opened.
- A reused segment may be referenced by two tables (a swapped table and the one kept for
  `REVERT`), so GC counts references across tables.
- A migration temporarily needs disk for the segments it rewrites.
