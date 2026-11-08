# Changelog

All notable changes to model-router are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Release notes for a `vX.Y.Z` tag come from its section below.

## [Unreleased]

### Added

- `completions SHELL [--bin NAME]`: shell completion scripts for bash, zsh,
  fish, elvish, and PowerShell, for `model-router` or the `mr` alias.
- `doctor` reports whether `git` and `gh` are installed, their versions, and
  which features need them.
- `docs/related.md`: comparable open-source tools and ideas worth borrowing.
- Verification receipts: router checks record the Git tree they tested;
  `report` (text and JSON) shows whether the workspace still matches it or
  has changed since, and separates router passes from self-reported ones.
  Opt-in `require_router_checks` for test stages.
- `handoffs RUN_ID`: what a stage receives of upstream handoffs, which
  fields were shortened or omitted, and lost decisions or findings.
- `fork RUN_ID --stage STAGE [--model M]`: an isolated run from a stage's
  saved snapshot, reusing completed upstream handoffs.
- `stats --min-pass` and `bench --min-pass`: an advisory cost-quality
  shortlist of assignments meeting a router-check pass rate.
- Interactive stages can outlive `mr`: each harness runs in its own terminal
  host process. Quitting asks whether to leave it running (TUI `q`, CLI `d`);
  the next `mr` (or `resume ID`) reattaches with the screen intact, or
  finishes the stage if the harness exited meanwhile.

- Release builds for Linux (x86_64/aarch64, static musl) and macOS
  (x86_64/aarch64) via cargo-dist, with a shell installer, Homebrew formula,
  checksums, and an `mr` alias; `scripts/build.sh` and `just` recipes.
- Several runs at once; a busy checkout sends the next run to its own
  worktree. Session runs list (`L`), next-needing-input (`N`), notice
  history (`!`).
- Interactive sessions keep running across detach (`Ctrl+B`) and reattach;
  takeover of unattended stages in the native terminal (`t`).
- omp harness. Declarative harness spec table; custom adapters gain
  `output`, `session`, `resume_args`, `model_flag`, `effort_flag`, `prompt`;
  `harnesses [--spec]`.
- Router providers: OpenRouter plus 20 OpenAI-compatible presets;
  `providers [NAME]`.
- Review summary stage that briefs the reviewer (`review.md`).
- Stage graphs: `after` runs ready read-only stages side by side
  (`max_parallel`); `fan_out` runs best-of-N candidates in their own
  worktrees for an auto or review-stage judge; Best-of-3 implement preset,
  Judge candidates stage preset, `resume --retry-candidate` (Monitor `Y`).
- Opt-in ship stage and `ship`: the router commits the run's changes on a new
  branch after a confirmed preview, optionally pushing and opening a draft PR.
- Handoff schema v2 (`artifacts`, `confidence`, `next_steps`) with native
  structured output for Codex and Claude Code.
- Verification gate for test stages; router-run `checks`, editable in the
  stage editor; `not_applicable` waiver.
- Cost estimates by source (reported, config, provider, table), run and
  stage `budget` caps, `cost`, `resume --budget-usd/--budget-tokens`.
- Optional OS sandbox per stage (bubblewrap, `sandbox-exec`), shown in
  `doctor`, `preview`, and the stage editor.
- Notifications (terminal, desktop, webhook) via `F9`, `--notify`,
  `MODEL_ROUTER_NOTIFY`, `MODEL_ROUTER_NOTIFY_URL`.
- `serve`: MCP over stdio and a loopback HTTP API with SSE events.
- `stats` and the TUI stats dialog (`I`) with stage history hints; `bench`
  matrices.
- Monitor results view, `report` (`--json`, `--diff`, `--transitions`),
  workspace snapshots, usage extraction, status transition log.
- Diff view with file/hunk navigation, search, `$PAGER`/`$EDITOR` and
  `git difftool` hand-offs; output search and follow/pause.
- Runs history: search, status filter, rerun, compare, clean.
- `doctor` compatibility checks, shown on the Harnesses page; harness output
  fixtures and capture script.
- Harness effort levels from local catalogs; models picked from the catalog.
- Project root detection and a repository line in the header.
- Task imports of GitHub pull requests.
- Optional Vim motions; Terminal and Poimandres themes.
- Action registry, keyboard guide, and command menu (`:`).
- Config `version = 2` with migration from v1.
- `resume --allow-repair` for one more repair pass.
- Smoke-test frame marker, shared PTY harness, detach/review/shutdown smoke
  tests, macOS smoke runs and dependency audit in CI; Amp orb setup.
- Docs split into focused pages under `docs/` with an index.

### Changed

- README reduced to an entry point; detail moved to `docs/`.
- TUI split into focused modules; key hints use one format; keys and
  confirmations hardened.
- Models picker moved to `F7`; `/` or `~` opens path entry.
- Unknown top-level and workflow config fields warn instead of failing.
- Harness checks are cached; queued input is drained before each redraw.
- Removed Claude Code provider routing.
- The offline demo includes a mock review summary stage.

### Fixed

- Harness process groups end with their stage; `resume` stops groups left by
  a crash.
- Repair and attempt bookkeeping.
- Bounded output buffers; snapshot refs kept from `git gc`.
- In-flight MCP calls answered before exiting on stdin EOF.
- Shutdown when the terminal disconnects.
- macOS picker tests and process checks; smoke-test resize race and flaky
  sleeps.
- The terminal host's socket directory is created private in one step; a
  second `mr` starting at the same time no longer refuses it.
- The run and workspace locks pass between `mr` and a terminal host as file
  descriptors, so they are no longer free while a stage is left running or
  reattached (only if an attached `mr` dies, until the host takes them back).

### Security

- Project trust: a project's custom adapters, checks, extra arguments,
  loosened permissions or sandbox, unconfirmed ship stages, and custom
  provider endpoints or key variables need `model-router trust`; `serve`
  refuses untrusted projects.
- Task imports refuse HTTPS→HTTP redirects and redirects or DNS answers that
  reach loopback, link-local, or private addresses.
- Provider `base_url` with credentials, query, or fragment is rejected;
  environment values are redacted in previews and records.

## [0.1.0] - 2026-10-04

### Added

- Terminal UI for routing a task through workflow stages, each running an
  agentic CLI harness and model, unattended or in an interactive PTY.
- Repository picker with recent projects, and a startup animation.
- Task imports from files, URLs, and GitHub issues/files, with an editable
  preview.
- Editable stage and workflow presets; project workflow configuration saved
  to `.model-router.toml`.
- Theme selection and per-stage harness settings.
- CI: format, clippy, tests on Linux and macOS, MSRV check, and terminal smoke
  tests.

[Unreleased]: https://github.com/Tyru5/model-router/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Tyru5/model-router/releases/tag/v0.1.0
