# D0006: DuckDB lives in its own bench workspace

**Status:** accepted · 2026-09-23
**Rule:** DuckDB (and any bundled-C test oracle) lives in `bench/`, a separate Cargo workspace
excluded from the root. It is never a member and never in the root `Cargo.lock`.

## Why

SPEC §13 says "a quarantined bench workspace crate". A *member* would put duckdb in the root
lockfile, one `--workspace` away from every build. The forbidden-dep law and build-budget
would then guard only by convention. A separate workspace makes the quarantine structural: CI's
root jobs cannot build it by accident, and only the `bench` job pays the C++ build. The cost:
bench has its own lockfile, and path-depends on `harness/`.

## Evidence

- `bench/Cargo.toml` (`[workspace]`)
- root `Cargo.toml` `exclude = ["bench"]`
- ci.yml `bench` job
- the forbidden-dep law reads the root Cargo.toml only
