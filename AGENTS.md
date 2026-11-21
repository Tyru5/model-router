# model-router

A Rust TUI and CLI (`model-router`, alias `mr`) that runs one task through a
sequence of agent harnesses (Codex, Claude Code, Pi, omp, Cursor, Gemini,
OpenCode, Aider, Goose, Amp, Copilot, custom), one model per stage, with
structured handoffs between stages. Linux and macOS only.

## Commands

```sh
just fmt             # cargo fmt + ruff format scripts examples
just lint            # cargo fmt --check; clippy --all-targets -D warnings
just test            # cargo test --locked
just smoke           # build, then every PTY smoke test
just smoke detach    # one smoke test (scripts/smoke_detach.py)
cargo +1.85 check --all-targets --locked   # MSRV, as CI runs it
cargo run -- --config examples/demo.toml   # offline demo with a mock harness
```

CI sets `RUSTFLAGS=-D warnings`, so any warning fails the build. Always pass
`--locked`; `Cargo.lock` is tracked. The crate is edition 2024 with
`rust-version = "1.85"`. Language or std features stabilized after 1.85 (let
chains, for example) pass on stable and fail the MSRV job.

Smoke tests need Python 3 and `uv`. Before finishing a TUI change, run the
smoke scripts that cover it (table in `docs/development.md`).

## No model requests

Tests, smoke tests, and the demo run offline against `examples/mock_harness.py`
and fixtures. Keep them offline.

- `scripts/capture_harness_output.py --confirm-paid-request` and the TUI
  Harnesses page live test (`t`) each send a real, possibly billed request.
  Do not run either unless the user asks.
- Never send requests to harnesses or providers to check a change. Use the
  mock adapter (`tests/common/mod.rs` sets it up) or `model-router preview`,
  which prints exact commands without launching anything.

## Code map

- `src/engine.rs`, `src/engine/`: runs, stages, attempts, scheduling, prompts,
  repair loop, recovery, apply, and resume. Most behaviour changes land here.
- `src/harness/builtins.rs`: one `HarnessSpec` per built-in harness.
  `src/adapters.rs` builds argv/env/stdin from a spec.
- `src/handoff.rs`, `src/handoff/`: output normalization, usage extraction, and
  handoff parsing. `schema/handoff.schema.json` is the published schema.
- `src/process.rs`, `src/process/`: process groups, capture, PTYs, interactive
  sessions, and the terminal host (`host.rs`) that lets interactive stages
  outlive `mr`.
- `src/config.rs`, `src/trust.rs`, `src/sandbox.rs`, `src/policy.rs`,
  `src/ship.rs`: config loading, project trust, OS sandbox, protected paths and
  secret scanning, and the only code path that commits.
- `src/serve/`: MCP and HTTP server. `src/tui/`: the TUI (unit tests in
  `src/tui/tests.rs`).
- `src/main.rs`, `src/cli/`: clap CLI and subcommand handlers.

## Invariants

These are product decisions (`docs/design-interview.md`, `docs/safety.md`).
Don't weaken them without the user's say-so.

- No shell evaluates any command. Build argument vectors.
- Only an enabled ship stage commits, on a new branch, after a confirmed
  preview.
- Harness permissions stay native unless a stage overrides them.
- Anything in a repo's `.model-router.toml` that runs commands, loosens
  permissions or sandboxing, or sets custom endpoints requires
  `model-router trust`. New config fields with that power must be gated the
  same way.
- The router stores no credentials. It reads provider keys from env vars at
  launch and redacts them in saved records and previews.
- No silent model substitution. Errors stop the stage and offer retry,
  replacement, or resume.
- A stage's harness process group ends with the stage.

## Tests

- Integration tests live in `tests/`, with shared fixtures in
  `tests/common/mod.rs`. Its `tempdir()` puts a space and non-ASCII
  characters in the path on purpose. Keep using it.
- Unit tests must not touch the user's config directory. Under `cfg(test)`,
  `config::config_dir()` returns a scratch directory unless
  `MODEL_ROUTER_CONFIG_DIR` is set. That fallback does not cover integration
  tests or the binary, so an integration test that reads settings, approvals,
  or trust must set `MODEL_ROUTER_CONFIG_DIR` to a temp dir (as
  `tests/trust.rs` and `scripts/smoke_common.py` do).
- Smoke scripts wait for frames through `MODEL_ROUTER_FRAME_MARKER` and
  `MODEL_ROUTER_FRAME_FILE`. Don't add sleeps. Screen captures go to
  `.model-router/qa/`. `smoke_shutdown.py` runs under plain `python3` and
  must not import `pyte`.
- Gate `/proc` checks to Linux. Smoke tests also run on macOS.

## Adding or changing a harness

Follow `docs/adapters.md#adding-a-harness`. Briefly: add a `HarnessSpec`
entry, add parser fixtures under `tests/fixtures/harness-output/` and list
them in `manifest.toml`, add cases to `tests/adapter_golden.rs`, then run
`MODEL_ROUTER_UPDATE_GOLDEN=1 cargo test --test adapter_golden`. Read the
regenerated `tests/fixtures/adapter-golden/*.json` diff. It is the exact
argv, env, stdin, and files per mode, so an unexpected change there is a bug.
Any argument change to an existing harness also changes its golden file.

## Docs and changelog

A user-facing change updates, in the same commit:

- `CHANGELOG.md` under `## [Unreleased]` (Keep a Changelog sections). Release
  notes come from it.
- The matching `docs/*.md` page (`docs/index.md` lists which covers what).
  New keys go in `docs/tui.md`. A new subcommand or flag goes in
  `docs/automation.md`. A new config field goes in `docs/configuration.md`.
- `README.md` only if it changes the quickstart, keys table, or features list.

Docs are terse and factual: say what the code does, with flags, file names,
and versions.

## Commits

One-line imperative subject in sentence case that describes the behaviour,
no type prefix: "Keep unit tests out of the user's config directory",
"Batch live harness tests and count passes as tested". Put the why in the
body. Commit only when asked.
