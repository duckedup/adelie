# D0003: A required check's workflow lists `merge_group:`

**Status:** accepted · 2026-09-22
**Rule:** Adding a required check and adding its `merge_group:` trigger are one change, never two.

## Why

A queued PR builds on a temporary `gh-readonly-queue/**` ref and fires a `merge_group` event.
A workflow owning a required check that does not list `merge_group:` never reports there, and
the queue entry stalls until it is ejected. It looks broken when it is only waiting. nidus hit
this (nidus D0003); adelie's `ci.yml` lists the trigger from the first commit.

Never skip a required job with a job-level `if`: a skipped job is a check that never reports.
Guard individual steps instead.

## Evidence

- `.github/workflows/ci.yml`: `on: merge_group:`.
