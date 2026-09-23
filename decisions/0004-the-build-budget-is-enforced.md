# D0004: The build budget is enforced in CI

**Status:** accepted · 2026-09-22
**Rule:** A clean, uncached, offline build of the lean library build (`--no-default-features`)
finishes in under 60s. A dependency that blows it, or any bundled-C / native-linking crate, is
a design change: file an issue first.

## Why

Speed is a core commitment, and build speed is part of it: an unenforced budget drifts one
reasonable-looking dependency at a time. The `build-budget` job fetches deps first so network
time is not counted, uses no cache, and fails past 60s. The bound is order-of-magnitude on
purpose so a shared runner never flakes it while a bundled C/C++ tree still cannot pass.

Judge a dependency by build-and-ship cost (compile time, toolchain, binary size), not by
whether it is pure Rust.

## Evidence

- `.github/workflows/ci.yml`: the `build-budget` job.
- `Cargo.lock` is committed so a new `*-sys` crate shows up as a reviewable diff.
