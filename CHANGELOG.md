# Changelog

All notable changes to model-router are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Release notes for a `vX.Y.Z` tag come from its section below.

## [Unreleased]

### Added

- TUI Harnesses live tests: `x`/`a` mark harnesses, `t` tests the marked ones
  (or the selected one) in parallel with `scripts/capture_harness_output.py`
  (one real request each, confirmed first) and shows one result dialog;
  failures stay marked for retry. Offered in a model-router checkout.
- `doctor` counts passing live tests saved in the project
  (`.model-router/harness-tests/`), so TESTED updates after one passes.
- `resume ID --release-groups`: forgets process groups recovery left running
  because it could not prove they were the run's harnesses, without
  signalling them, and prints each released ID.
- TUI recovery keys for a stopped run: `R` allows one more repair pass after
  the repair limit, `B` raises a USD budget cap after asking for the amount.
  Each asks first and shows only for its stop.
- TUI action menu: `Space`, then a group letter (Run, Stage, Workflow, Go,
  View) and an action letter, lists every action of the current view.
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

- TUI: the startup animation plays only on the first launch of each
  installed version, recorded in `<config dir>/startup.json`.
- TUI: Monitor and Runs are one Runs page (`2`): a sidebar of this
  session's runs and saved runs beside the selected one; Harnesses is `3`.
  Its list keys moved where they collided: filter `F`, status `s`,
  compare `=`; `n` reruns; `[`/`]` gave way to `↑↓`.
- TUI: a run shows a stage strip and one feed of every stage's output, in
  place of the Progress, Output, and Handoff panels. A card under the feed
  says when the run waits on you and which keys answer it. A finished run
  shows its brief in one scrolling column with a Next bar of likely keys.
- TUI: the Workflow page sets up a new run: the task panel on top, edited in
  place with `t`, then a strip of the stages (`←→` selects) with the selected
  stage's settings below it at every width.
- TUI: enabling or disabling a stage moved from `Space` to `o`. The footer is
  one row, ending with `Space menu` and `? keys`.
- README reduced to an entry point; detail moved to `docs/`.
- TUI split into focused modules; key hints use one format; keys and
  confirmations hardened.
- Models picker moved to `F7`; `/` or `~` opens path entry.
- Unknown top-level and workflow config fields warn instead of failing.
- Harness checks are cached; queued input is drained before each redraw.
- Removed Claude Code provider routing.
- The offline demo includes a mock review summary stage.

### Fixed

- An interactive stage no longer fails, leaving its harness running
  unrecorded in its terminal host, when a screen redraw passes 1 MiB (a
  large truecolour screen): the host splits redraws into frames mr accepts,
  and an unreadable host stream now ends the harness's process group.
- Pastes into an interactive stage are no longer cut off past 64 KiB: the
  terminal host leaves input it cannot queue with mr until the harness reads
  it, and writes to the harness as fast as its terminal takes it.
- Recovery no longer signals a recorded process group ID that now belongs to
  another process: it stops a group only if its leader's start time or the
  `MODEL_ROUTER_GROUP` marker in a member's environment matches the record,
  and checks again before SIGTERM and before SIGKILL, so a group that ends
  during the grace period and whose ID is reused is not killed. A group it
  cannot prove is left running and stays recorded in the run, as does one
  that survives SIGTERM once nothing proves it is the run's, including when
  an interactive harness's terminal host dies before mr first attaches. While it is
  alive the run launches no harness (`resume`, retry, advance, `fork`), and
  no new run or `rerun` starts in the same checkout; the error names the
  group, why it is unproven, and the `kill` command. Runs saved by
  earlier versions record neither, so their surviving groups are left
  running and recorded the same way.
- An interactive harness's start time is read by its terminal host before
  the host can reap it, and reported on launch and on reattach, so a
  short-lived harness's PID is not mistaken for a later process's.
- `resume` prints what recovery stopped or left running instead of
  discarding it; the TUI shows it in the run's output, `serve` in its log.
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
- `fork` refuses a stage that judges a fan-out, which could never finish.
- Receipts and `bench --min-pass` use a fan-out stage's chosen candidate's
  checks, and count configured required checks rather than those that ran.
- `handoffs` counts list items lost to repeated shortening correctly.
- Process groups that parallel workers start are journaled at once, so a
  crash while checks run still lets `resume` stop them.
- A panic in a TUI run's driver fails that run with the panic message
  instead of leaving it running forever, and a panic off the TUI thread no
  longer restores the terminal under the still-drawing TUI. The run is
  reloaded from disk first, so retry and continue do not fail with "This run
  changed in another process".

### Security

- Project trust: a project's custom adapters, checks, extra arguments,
  loosened permissions or sandbox, unconfirmed ship stages, and custom
  provider endpoints or key variables need `model-router trust`; `serve`
  refuses untrusted projects.
- Task imports refuse HTTPS→HTTP redirects and redirects or DNS answers that
  reach loopback, link-local, or private addresses.
- Provider `base_url` with credentials, query, or fragment is rejected;
  environment values are redacted in previews and records.
- Saved runs must keep the workspace, run directory, and candidate worktree
  paths that follow from their project and ID, attempt and candidate patch
  paths inside the run's own directories, with no link there leading out, valid
  stage and candidate IDs, only the fan-out candidate IDs their workflow
  gives, and Git object names as snapshots; a running attempt's terminal is reattached to only when it is
  a socket in the terminal host's socket directory, and a config can no
  longer reapprove a run whose approval digest stopped matching or cannot be
  read. A run recorded through a link to its project follows the project
  as it resolves now, not the link.
- Reattaching to an interactive stage whose harness outlived its `mr` runs
  the stage's checks only while the saved run is still approved; otherwise
  the stage fails once the harness ends, which it can still be stopped or
  watched to do.
- A session ID that could pass as an option never reaches a harness.
- `workspace-write` sandboxes keep the workspace's `.model-router` directory
  read-only apart from the attempt and handoff directories, and refuse to
  start when it is a symbolic link the stage could replace. A sandboxed
  stage can write inside its attempt and handoff directories but not remove
  or replace them, and a stage refuses to start when either is a link.
- Sandboxed stages can no longer reach terminal-host sockets in the usual
  socket directories, so they cannot attach to and type into another run's
  interactive harness, which runs outside the sandbox, or take its locks.
  An `mr` run with another `TMPDIR` or a custom `XDG_RUNTIME_DIR` puts its
  sockets elsewhere; see the safety docs.
- `serve --http` keeps connections whose headers have not authenticated in
  their own queue and cuts off the oldest once it has waited half a second,
  so idle sockets cannot lock out authenticated clients and a burst of
  clients is not cut off before its headers are read.
- Harness-reported errors are stripped of terminal escapes; recorded process
  group IDs outside the valid range are never signalled.
- Check logs, fan-out candidate patches, and candidate snapshot indexes
  never write through a link a stage left in its attempt directory (a
  process the stage detached and left running can still race them), and
  undoing a conflicting winner removes only plain files inside the
  repository, never through a link and never Git's or the router's own.

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
