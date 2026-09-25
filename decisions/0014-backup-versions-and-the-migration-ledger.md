# D0014: Backup, versions, and the migration ledger

**Status:** accepted · 2026-09-24
**Rule:** A retained manifest version is readable, so GC keeps what it names. A backup is hard
links plus one manifest. The versioned-migration ledger lives in the manifest until system
tables exist.

## Why

- AT VERSION (SPEC §19) needs retained versions to be readable. Before this change,
  `retain_manifests` links were "a debugging aid" and gc consulted only the current manifest
  and in-process snapshots, so version n could lose its segments while still retained.
- `Garbage.removed_at_version` makes the rule exact and cheap: retained version v names a
  segment the current version does not only if that segment was released at some r with
  v < r ≤ current. So gc keeps garbage with `removed_at_version > version - retain_manifests`,
  and no old manifest is ever decoded. `0` (older manifests) is unprotected.
- Out-of-process `Reader`s get the same guarantee, since protection comes from the writer's
  manifest and not from registration.
- Backup manifest: live tables and retired entries (so REVERT/UNDROP work on a restore) with
  every file they reference hard-linked; the ledger and id counters are kept. Garbage and
  running jobs are dropped: their files are not linked, and a job's source table is intact.
  Hard links make it instant, and a live-registered snapshot makes it consistent.
- Ledger: `Manifest.migrations` (name, XXH64 checksum, applied_at_ms), an additive trailing
  record, rather than a system table no layer can yet query. At-least-once: exec, then record.
  A crash between the two re-runs that one file, so migration files should be idempotent. A
  recorded file that changed or disappeared, or a new file numbered below the highest recorded
  one, is an error before anything runs.
- SQL (`BACKUP TO`, `SELECT … AT VERSION`), `adelie.migrations` as a table, the `adelie migrate`
  CLI and MCP approval wait for the SQL front end and surfaces (adelie-2hh.4), as D0013 did for
  ALTER.

## Consequences

- Disk: up to `retain_manifests` commits' worth of replaced segments stay on disk past
  `gc_grace`. With the default of 8 and one commit per flush, AT VERSION reaches back 8 commits,
  which is not a time window. Setting `retain_manifests = 0` restores the old reclaim timing.
- `Reader::view_at` trusts the link. If a link outlived a prune that failed, or the writer's
  `retain_manifests` was lowered, the scan can still hit `SnapshotExpired`.
- Backup targets must be on the same filesystem (hard links); a cross-device failure is the io
  error as-is.
- Manifest format stays v1: two additive fields (`Garbage.removed_at_version`, the migrations
  record).
