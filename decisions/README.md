# Decisions

One record per rule in `AGENTS.md`: the rule, why it exists, and the evidence.

| Record | Rule |
|---|---|
| [D0001](0001-tracker-is-beads-and-the-export-stays-untracked.md) | The tracker is beads; its JSONL export stays untracked |
| [D0002](0002-bd-setup-not-bd-init.md) | A fresh clone runs `just bd-setup`, never `bd init` or `bd bootstrap` |
| [D0003](0003-merge-queue-needs-the-merge-group-trigger.md) | A required check's workflow lists `merge_group:` |
| [D0004](0004-the-build-budget-is-enforced.md) | The lean clean build stays under 60s, CI-enforced |
| [D0005](0005-releases-fire-on-a-version-bump.md) | Bump the version to release; 0.0.0 never releases |
| [D0006](0006-duckdb-lives-in-its-own-bench-workspace.md) | DuckDB lives in `bench/`, a separate Cargo workspace excluded from the root |
