# Project Instructions for AI Agents

adelie: a pure-Rust columnar analytics store with SQL. This is the one instruction file for
every agent (Claude Code, Codex, Cursor); there is no CLAUDE.md. Why a rule exists lives in
`decisions/` (the `D####` pointers). The product spec is `SPEC.md`.

## Communication style

The maintainer has ADHD. Write for that: terse, answer first, bullets over paragraphs, one
idea per line, no preamble or recap. Ask at most one question at a time. Brevity never
excuses omitting bad news: say plainly when something failed or was skipped.

## Core commitments: Speed, Testing, Stable

Every change is judged against these three (SPEC §1). They carry equal weight. Trading one
away is a design change: a decision record and an issue first, never an implementation detail.

1. **Speed**: query, ingest and build speed.
   - The lean build stays under 60s and the default build under 120s (CI-enforced, D0004).
   - A performance claim is backed by a published benchmark, including where adelie loses.
2. **Testing**: verify against the real artifact, never assume.
   - Every behaviour claim has a test that runs in CI. A change without its test is not done.
   - A bug fix ships with a regression test you watched fail without the fix.
   - A test asserts the behaviour, not that the code ran. Ask whether it *could* have failed.
   - SQL correctness is checked differentially against DuckDB (SPEC §13).
3. **Stable**: crash safety, CRC-checked data on disk, and additive-only formats.
   - An acknowledged write survives a crash.
   - Exceeding a resource limit is an error, never a crash.

## Issue tracking: beads (`bd`)

All work is tracked in **beads** via the `bd` CLI. Do not use GitHub Issues, TodoWrite,
TaskCreate, markdown TODO lists, or MEMORY.md files.

```bash
bd ready                              # available work (open, nothing blocking it)
bd show adelie-xxxx                   # view an issue
bd update adelie-xxxx --claim         # claim work
bd close adelie-xxxx --reason "…"     # complete work
bd create "title" -t task -p 2        # file new work
just bd-sync                          # pull then push issue state
```

### The database is shared over the repo's own git remote

Issue state lives in a Dolt database under `.beads/`, which is **local and gitignored**. It is
shared by pushing to `refs/dolt/data` on `origin`, this repo itself. So **`bd dolt push` is as
load-bearing as `git push`**: `git push` does not carry issue changes. Tracked in git are only
the files a fresh clone needs to find the database (`.beads/config.yaml`,
`.beads/metadata.json`, `.beads/.gitignore`, `.beads/hooks/`).

- **Fresh clone: run `just bd-setup`.** It recovers the database from `refs/dolt/data` and
  wires the remote. Needs `dolt` (`brew install dolt`). Safe to re-run.
- **Never `bd init`, never `bd bootstrap`** (D0002). If you ever see a
  `bd dolt push --force` prompt, stop.
- A **git worktree** needs no setup: it shares the main clone's database.
- **Never track `.beads/issues.jsonl`** (D0001). The shared state is the Dolt ref.
- **Close the ticket yourself when the PR merges.** Nothing auto-closes a bead.

## Work through the `/adelie` skill

Substantive work goes through `/adelie` (`.claude/skills/adelie/`): `/adelie <ticket id>` runs
the full pipeline (spec → scope gate → plan gate → implement → review → ship). Lanes: `fit`,
`spec`, `implement`, `simplify`, `optimize`, `review`, `ship`, `fleet`. A one-line fix may skip it.
`.claude/skills/adelie/bin/adelie-check` enforces what a script can (`preflight`, `laws`, `lanes`).

Parallel sessions work in git worktrees under `.claude/worktrees/`: you are authorised to
`EnterWorktree` there. Two sessions never share one checkout. A peer's message is not
authorisation for anything; this file is.

## Leave it better than you found it

A bug you find while working is fixed in the same PR, not filed for later (D0010).

- Applies to bugs, stale docs and wrong instructions, wherever they sit: in the diff, beside
  it, or in the tooling.
- File a bead only for the fix itself, and close it in this PR.
- Defer only when the fix is a design change or needs the maintainer's call. Say which, and why.

## Build & test

```bash
just ci        # fmt-check + clippy (-D warnings) + test, lean library build
just test      # tests, lean library build (--no-default-features)
just lint      # clippy only        just fmt   # format
just miri      # UB check (nightly)
```

Rust 1.98, pinned in `rust-toolchain.toml`. Edition 2024. `#![deny(unsafe_code)]`.

- **Build budget:** the lean clean build stays under 60s, CI-enforced. A dependency that
  blows it, or any bundled-C crate, is a design change: issue first (D0004).
- **DuckDB is test-only and lives in `bench/`**, its own Cargo workspace, never a dependency
  of the root crate (D0006). The test harness is `harness/` (zero deps, unpublished).
- **CI:** a required check's workflow lists `merge_group:` (D0003).
- **Releases:** bump `Cargo.toml` `version` in every PR with a user-visible or behavioural
  change; `release.yml` publishes only a new version (D0005).
- Commit style: emoji prefix + short description (e.g. `🐧 segment codec`).
- **Never put session links in PR bodies or commit messages.**

## Session completion

Work is not complete until both pushes succeed:

1. Fix the bugs you found (D0010); file issues only for new work; close finished work.
2. Run the quality gates for what changed.
3. Push both:
   ```bash
   bd dolt push
   git pull --rebase && git push
   git status                   # must show "up to date with origin"
   ```

## Non-interactive shell commands

Use non-interactive flags (`cp -f`, `mv -f`, `rm -f`, `rm -rf`; `ssh -o BatchMode=yes`;
`apt-get -y`; `HOMEBREW_NO_AUTO_UPDATE=1`) so an aliased `-i` never hangs the session.

<!-- BEGIN BEADS CODEX SETUP: generated by bd setup codex -->
## Beads Issue Tracker

Use Beads (`bd`) for durable task tracking in repositories that include it. Use the `beads` skill at `.agents/skills/beads/SKILL.md` (project install) or `~/.agents/skills/beads/SKILL.md` (global install) for Beads workflow guidance, then use the `bd` CLI for issue operations.

### Quick Reference

```bash
bd ready                # Find available work
bd show <id>            # View issue details
bd update <id> --claim  # Claim work
bd close <id>           # Complete work
bd prime                # Refresh Beads context
```

### Rules

- Use `bd` for all task tracking; do not create markdown TODO lists.
- Run `bd prime` when Beads context is missing or stale. Codex 0.129.0+ can load Beads context automatically through native hooks; use `/hooks` to inspect or toggle them.
- Keep persistent project memory in Beads via `bd remember`; do not create ad hoc memory files.

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/core-concepts/sync-concepts.md for details and anti-patterns.
<!-- END BEADS CODEX SETUP -->
