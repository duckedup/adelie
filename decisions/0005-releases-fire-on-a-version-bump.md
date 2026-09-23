# D0005: Releases fire on a version bump

**Status:** accepted · 2026-09-22
**Rule:** Bump `Cargo.toml` `version` in every PR with a user-visible or behavioural change.
`0.0.0` is the placeholder and never releases.

## Why

`release.yml` runs on every push to `main`. It builds the binary for five targets, publishes to
crates.io, tags `v<version>`, and
creates a GitHub release only when that tag does not exist yet. So a PR that does not bump the
version ships nothing, silently. Runs are serialized so a merge train cannot race the tag check.

The publish step is idempotent: a re-run after a later step failed skips "already exists" on
crates.io instead of failing.

## Evidence

- `.github/workflows/release.yml`.
- nidus D0007 and nidus #124 (concurrent release runs racing the tag).
- Uses the `CARGO_REGISTRY_TOKEN` organization secret (visible to this repo).
