smoke_python := "uv run --with-requirements scripts/requirements-smoke.txt python"

# List recipes.
default:
    @just --list

# Format Rust and Python sources.
fmt:
    cargo fmt
    uvx ruff format scripts examples

# Check formatting and clippy (mirrors CI).
lint:
    cargo fmt --check
    cargo clippy --all-targets --locked -- -D warnings

# Run Rust tests.
test:
    cargo test --locked

# Build, then run every terminal smoke test, or one: `just smoke detach`.
smoke *names: (_build)
    #!/usr/bin/env bash
    set -euo pipefail
    names="{{ names }}"
    [[ -n "$names" ]] || names="tui repos imports stages themes harness_settings startup tour detach review delegation shutdown"
    for name in $names; do
        echo "==> smoke_$name"
        {{ smoke_python }} "scripts/smoke_$name.py"
    done

# Plan a release and build this host's archive, without publishing (needs `dist`).
release-dry-run:
    dist plan
    dist build --artifacts=host

_build:
    cargo build --locked
