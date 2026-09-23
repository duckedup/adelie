# D0010: Fix bugs in the PR that finds them

**Status:** accepted · 2026-09-23
**Rule:** A bug found while working is fixed in the same PR, not filed for later. Only a design
change or a maintainer's call may be deferred, and the deferral says which and why.

## Why

- A filed bug is a bug that ships. The session that found it has the context; a later one has to
  rebuild it from a ticket.
- "Out of scope" was being used for anything not in the ticket, including one-line fixes to
  wrong instructions that every later session would trip over.
- The maintainer asked for it directly: "Don't defer bugs" and "leave it better than you found
  it".

## Evidence

- adelie-1i8 (E4): two bugs found mid-epic were first filed as follow-ups: SPEC §5/§6/§18 out
  of date with E4 (adelie-70j), and the implement worker prompt citing a nonexistent
  `.claude/rules/` and "Errors are anyhow" (adelie-c8f). The maintainer redirected both into the
  same PR.
