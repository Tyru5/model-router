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
| `smoke_tour` | First-launch tour from Harnesses to a completed demo run, `F1`, no reopen once finished, the one-time `F1` line for earlier users. Other smoke tests write `tour.json` so the tour stays closed (`Terminal(tour=True)` opts in) |
| `smoke_detach` | Detach and reattach of interactive sessions, including leaving one running after `mr` exits and reattaching from a new `mr` |
| `smoke_review` | Diff view: file list, hunk/file jumps, search, `$PAGER`/`$VISUAL` hand-offs |
| `smoke_delegation` | A delegating stage's task line on the Runs page; cancelling the stage cancels its task (skipped without bubblewrap or sandbox-exec) |
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

Release assets on GitHub are public, and `model-router-installer.sh` on each
release installs from them. `.github/workflows/r2.yml` also mirrors every
release so the installer has a short URL that does not depend on GitHub. It
runs after each successful tag release (or by hand with a tag) and copies the
`.tar.xz` archives and their `.sha256` files to a Cloudflare R2 bucket served
on a public domain:

```text
install.sh                                    scripts/install.sh with the bucket URL filled in
latest.txt                                    newest stable version, e.g. 0.2.0
vX.Y.Z/model-router-<target>.tar.xz(.sha256)  one folder per release
```

Prerelease tags (`v1.0.0-rc.1`) get a folder but never move `install.sh` or
`latest.txt`. `scripts/install.sh` picks the target from `uname`, downloads
over HTTPS with curl or wget, checks the SHA-256, and installs `model-router`
and an `mr` link into `$CARGO_HOME/bin` (default `~/.cargo/bin`). It accepts
`--version X.Y.Z` and `--to DIR`, or `MODEL_ROUTER_VERSION`,
`MODEL_ROUTER_INSTALL_DIR`, and `MODEL_ROUTER_DOWNLOAD_URL`. It leaves an
existing `mr` that is not a link to `model-router` in place.

The bucket is `model-router-downloads` in the Cloudflare account, served at
`https://model-router.tiru5.me` (custom domain on the `tiru5.me` zone). The
workflow defaults to that bucket, account, and URL; `R2_BUCKET` and
`R2_ACCOUNT_ID` secrets and an `R2_PUBLIC_URL` variable override them.

One-time setup:

1. Cloudflare dashboard → R2 → Manage API tokens → create a token with Object
   Read & Write on `model-router-downloads` only.
2. Add its S3 credentials as repository secrets `R2_ACCESS_KEY_ID` and
   `R2_SECRET_ACCESS_KEY`. The workflow skips with a notice until both are set.
3. For a release that already exists, run the **R2 downloads** workflow by
   hand with its tag.

## Website

`site/` is the landing page at `https://model-router.dev`: one static
`index.html` and `icon.png`, no build step. The Vercel project `model-router`
(team Tirus) is linked to this repository with root directory `site`, so a push
to `main` that changes `site/` deploys it to production and other branches get
preview deployments. The domain is registered through Vercel and uses Vercel
DNS; `www.model-router.dev` redirects (308) to the apex. The installer stays on
the R2 domain, `model-router.tiru5.me`.

## Amp orbs

`.agents/setup` prepares fresh orbs: stable Rust (rustfmt, clippy), Rust 1.85,
Python 3.12, `uv`, `just`, Ruff, bubblewrap (so sandbox tests run); caches
smoke-test dependencies; fetches `Cargo.lock` dependencies; warms debug/test
and MSRV builds. Re-runs reuse installed tools. `.agents/resume` only checks tools are present. Neither
authenticates harnesses or sends model requests.

## Design references

- [spec.md](spec.md): build specification and acceptance checks.
- [design-interview.md](design-interview.md): confirmed product decisions.
- [adapters.md](adapters.md): per-harness references, adding a harness, doctor, fixtures.
