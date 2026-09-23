# D0002: A fresh clone runs `just bd-setup`, never `bd init` or `bd bootstrap`

**Status:** accepted · 2026-09-22
**Rule:** `just bd-setup` in a fresh clone. Never `bd bootstrap`, never `bd init`. A worktree needs neither.

## Why

`bd bootstrap` rejects the tracked `git+ssh://` remote as "not a Dolt remote" and leaves a
fresh clone with an empty tracker and no error. An empty database reads as divergent history,
and the recovery offered is `bd dolt push --force`, which would overwrite everyone's issues.
`bd init` mints a new identity and can do the same. `scripts/bd-setup.sh` extracts
`refs/dolt/data`, clones it with `dolt`, and wires the remote instead.

`bd init` ran exactly once, to create this repo's database on 2026-09-22.

## Evidence

- `scripts/bd-setup.sh`, ported from nidus.
- nidus D0002 and nidus-1oq.
