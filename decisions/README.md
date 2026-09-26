# Decisions

One record per rule in `AGENTS.md`, or per design change to `SPEC.md`'s commitments: the
rule, why it exists, and the evidence.

| Record | Rule |
|---|---|
| [D0001](0001-tracker-is-beads-and-the-export-stays-untracked.md) | The tracker is beads; its JSONL export stays untracked |
| [D0002](0002-bd-setup-not-bd-init.md) | A fresh clone runs `just bd-setup`, never `bd init` or `bd bootstrap` |
| [D0003](0003-merge-queue-needs-the-merge-group-trigger.md) | A required check's workflow lists `merge_group:` |
| [D0004](0004-the-build-budget-is-enforced.md) | The lean clean build stays under 60s, CI-enforced |
| [D0005](0005-releases-fire-on-a-version-bump.md) | Bump the version to release; 0.0.0 never releases |
| [D0006](0006-duckdb-lives-in-its-own-bench-workspace.md) | DuckDB lives in `bench/`, a separate Cargo workspace excluded from the root |
| [D0007](0007-the-type-set-adds-decimal-uuid-ip.md) | The v1 type set adds `DECIMAL(p, s)`, `UUID`, and `IP` |
| [D0008](0008-segment-format-v1.md) | Segment format v1: CRC32C framing, fixed type/encoding/index ids, additive records |
| [D0009](0009-manifest-format-v1.md) | Manifest v1: CRC32C framing, additive records, per-table OCC, temp-fsync-rename publish |
| [D0010](0010-fix-bugs-in-the-pr-that-finds-them.md) | Fix a bug in the PR that finds it; defer only a design change or a maintainer's call |
| [D0011](0011-ledger-and-transactions-across-engines.md) | `ledger` is an engine in adelie; transactions span every engine |
| [D0012](0012-stable-ids-and-rebuild-and-swap-migrations.md) | Stable table/field ids; engine, `KEY`, `VERSION` fixed; every migration is rebuild and swap |
| [D0013](0013-migrations-as-built.md) | A migration builds a new table id and swaps it in by name; segment entries are views of shared files; the library API comes first |
| [D0014](0014-backup-versions-and-the-migration-ledger.md) | A retained manifest version is readable, so GC keeps what it names; a backup is hard links plus one manifest; the migration ledger lives in the manifest |
| [D0015](0015-query-execution-as-built.md) | Query execution is a pure library engine in `src/exec` driven by a physical `Plan`, with storage behind `TableSource`; aggregate partial states and encodings are frozen |
| [D0016](0016-sql-front-end-as-built.md) | SQL is a hand-rolled lexer/parser, a binder addressing columns by name until the final lowering, a rule-based planner with no cost model, and a statement executor over `Store` |
