# List recipes.
default:
    @just --list

# Format Rust and Python sources.
fmt:
    cargo fmt
    uvx ruff format scripts examples
