# Format all code
fmt:
    cargo fmt --all

# Verify formatting is clean (CI guard)
fmt-check:
    cargo fmt --all -- --check

# Lint with clippy, deny all warnings (lean library build)
lint:
    cargo clippy --all-targets --no-default-features -- -D warnings

# Run all tests (lean library build)
test:
    cargo test --no-default-features

# Debug build
build:
    cargo build

# Optimized build
release:
    cargo build --release

# Undefined-behaviour check (nightly)
miri:
    MIRIFLAGS="-Zmiri-disable-isolation" cargo +nightly miri test --no-default-features

# Dependency tree
deps:
    cargo tree -p adelie

# The local gate: what CI's fmt, clippy and test jobs run on the lean build
ci: fmt-check lint test

# Recover the beads database in a fresh clone and wire the remote (never `bd init`, D0002)
bd-setup:
    ./scripts/bd-setup.sh

# `git push` does NOT carry issue state; the database rides refs/dolt/data.
# Publish and collect issue state (bd dolt pull, then push)
bd-sync:
    bd dolt pull
    bd dolt push

# Fetch one section of a repo doc: just spec toc | just spec 8 | just spec find wal
spec *ARGS:
    @.claude/skills/adelie/bin/spec {{ARGS}}

# Deterministic repo-law check over the branch (the /adelie skill runs this before shipping)
laws *ARGS:
    @.claude/skills/adelie/bin/adelie-check laws {{ARGS}}
