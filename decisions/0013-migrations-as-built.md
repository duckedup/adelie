# D0013: Migrations as built

**Status:** accepted · 2026-09-24
**Rule:** A migration builds a new table id and swaps it in by name; a table's segment entries
are views of shared files; the library API comes first.

## Why

- A migration's target gets a new table id; the swap moves the name. D0012: "with table ids in
  paths, swap, REVERT and UNDROP are each one manifest commit." Rewritten files live under the
  new id's directory, so a cancelled job's files are all in one place.
- `SegmentEntry` is the owning table's view (stats aligned to its schema) plus `file_field_ids`
  for the physical layout, because stats are encoded positionally against the table schema, and
  a reused file belongs to two schemas.
- `ALTER TABLE`, `EXPLAIN ALTER`, `CANCEL JOB`, `adelie.jobs`, `UNDROP` and `TRUNCATE` as SQL
  wait for the SQL front end (E6); SPEC §4's "ships whole" applies once a surface exists, as it
  did for adelie-2hh.1.

## Consequences

- Jobs are caller-driven (`Store::run_job`, one bounded step per call, progress committed each
  step); nothing runs on open; `jobs()` lists what to resume.
- The swap backs off while the flusher holds batches for the table, and projects buffered
  batches onto the new schema in the same critical section as the publish. After the swap a
  writer still sending the old shape gets `SchemaMismatch`.
- Compaction skips a table with a running job.
- REVERT restores the retired definition over its original segment entries plus every segment
  and tombstone added since the swap; it is refused (`RevertStale`) once a segment the swap
  produced is gone (compacted or truncated), since restoring would duplicate or lose rows. A
  tombstone since the swap on a column the old definition lacks also refuses.
- `DROP COLUMN` is refused while a tombstone names the column (tombstones name columns).
- Not built, refused by name (`MigrationNotBuilt`): `PARTITION BY` (flush writes every segment
  to `_` today) and changing a column's type (needs coerce-on-read).
- The rollup guardrail is checked against an always-empty dependency list until rollups exist
  (adelie-zit.2); it cannot fire yet.
- Grace: `StoreOptions::retain_definitions`, default 24 hours, separate from `gc_grace`. A
  `DROP TABLE` waits for the table's buffered writes to flush, so every acked write lands
  before it; a table written continuously refuses the drop as busy.
- While a job runs on a table, compaction of it and a `DELETE` naming a column the job drops
  are refused (`JobRunning`), checked in the commit itself so no race slips past.
