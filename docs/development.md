# Development

Requires Rust 1.85+. Python 3 and [`uv`](https://docs.astral.sh/uv/) for the
demo and terminal smoke tests. [`just`](https://just.systems) wraps the
common commands.

## Offline demo

Uses Python 3 and a mock harness; sends no model requests. From the repo root:

```sh
cargo run -- --config examples/demo.toml
cargo run -- --config examples/demo.toml run --task 'Exercise the handoff'
cargo run -- --config examples/demo.toml run --task 'Exercise repairs' --with-review
```

## Checks

```sh
just lint            # cargo fmt --check; clippy -D warnings
just test            # cargo test --locked
just smoke           # build, then every terminal smoke test
just smoke detach    # one smoke test
just release-dry-run # dist plan + host build (needs cargo-dist)
```

Smoke tests (`scripts/smoke_*.py`) drive the real binary in PTYs with `pyte`
and offline fixtures; screen captures go to `.model-router/qa/`. They wait on
frames via `MODEL_ROUTER_FRAME_MARKER`/`MODEL_ROUTER_FRAME_FILE` rather than
sleeping.

| Script | Covers |
| --- | --- |
| `smoke_tui` | PTYs, text editing, Unicode paste, resize, disabled review, interactive completion, history, terminal cleanup |
| `smoke_repos` | Repository browser: navigation, filtering, recent projects, config loading, autosave, invalid paths/config |
| `smoke_imports` | File browsing, text and GitHub URLs, editable previews, append/replace, cancellation, errors (local HTTP server, mock `gh`) |
| `smoke_stages` | Stage presets, editing, ordering, optional reviews, persistence, interactive verification handoffs |
| `smoke_themes` | All 14 themes, cancellation, restart, repo switching, invalid preferences |
| `smoke_harness_settings` | Per-stage isolation, permission arguments, conflicts, model browsing, persistence |
| `smoke_startup` | Animation, skip/bypass, resize, interrupt cleanup; records `.model-router/qa/startup.gif` |
| `smoke_detach` | Detach and reattach of interactive sessions |
| `smoke_review` | Diff view: file list, hunk/file jumps, search, `$PAGER`/`$VISUAL` hand-offs |
| `smoke_shutdown` | Shutdown and terminal loss |

`scripts/capture_harness_output.py` captures real harness output into
`tests/fixtures/harness-output/` ([parser fixtures](adapters.md#parser-fixtures)).

## CI

`.github/workflows/ci.yml` on every PR and push to `main`: formatting, clippy,
`cargo test` on Linux and macOS, Rust 1.85 MSRV check, smoke tests on Linux
(macOS smoke runs are non-blocking), and a dependency audit. Failed smoke runs upload screen captures.

Adapter flags were checked against installed CLI help or official docs;
automated tests use mocks. Live authenticated runs against every harness have
**not** been performed. Project verification log:
[verification.md](verification.md).

## Releases

cargo-dist builds Linux (x86_64/aarch64, static musl) and macOS
(x86_64/arm64) archives, a shell installer, and a Homebrew formula when a
`vX.Y.Z` tag is pushed. Release notes come from that version's section of
[CHANGELOG.md](../CHANGELOG.md).

## Amp orbs

`.agents/setup` prepares fresh orbs: stable Rust (rustfmt, clippy), Rust 1.85,
Python 3.12, `uv`, `just`, Ruff; caches smoke-test dependencies; fetches
`Cargo.lock` dependencies; warms debug/test and MSRV builds. Re-runs reuse
installed tools. `.agents/resume` only checks tools are present. Neither
authenticates harnesses or sends model requests.

## Design references

- [spec.md](spec.md): build specification and acceptance checks.
- [design-interview.md](design-interview.md): confirmed product decisions.
- [adapters.md](adapters.md): per-harness references, adding a harness, doctor, fixtures.
