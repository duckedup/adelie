# Decisions

One record per rule in `AGENTS.md`: the rule, why it exists, and the evidence.

| Record | Rule |
|---|---|
| [D0001](0001-tracker-is-beads-and-the-export-stays-untracked.md) | The tracker is beads; its JSONL export stays untracked |
| [D0002](0002-bd-setup-not-bd-init.md) | A fresh clone runs `just bd-setup`, never `bd init` or `bd bootstrap` |
| [D0003](0003-merge-queue-needs-the-merge-group-trigger.md) | A required check's workflow lists `merge_group:` |
| [D0004](0004-the-build-budget-is-enforced.md) | The lean clean build stays under 60s, CI-enforced |
