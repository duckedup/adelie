# D0001: The tracker is beads, and its JSONL export stays untracked

**Status:** accepted · 2026-09-22
**Rule:** Never let `.beads/issues.jsonl` become tracked. The shared state is the Dolt ref.

## Why

Issue state lives in a Dolt database under `.beads/`, local and gitignored, shared by pushing
to `refs/dolt/data` on `origin`. A tracked JSONL export is rewritten from each branch's local
database, so any branch can silently revert another branch's closes. nidus shipped that bug
(nidus #83); adelie starts with the export ignored and `export.git-add: false`.

## Evidence

- `.beads/.gitignore`: `issues.jsonl`, `events.jsonl`, `interactions.jsonl`.
- `.beads/config.yaml`: `export.git-add: false`.
- nidus D0001.
