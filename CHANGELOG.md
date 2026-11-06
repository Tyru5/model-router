# Changelog

All notable changes to model-router are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Release notes for a `vX.Y.Z` tag come from its section below.

## [Unreleased]

### Added

- Release builds for Linux (x86_64/aarch64, static musl) and macOS
  (x86_64/aarch64) via cargo-dist, with a shell installer, Homebrew formula,
  checksums, and an `mr` alias.
- Run several workflows at once and take over running interactive sessions.
- Interactive sessions keep running across detach (Ctrl+B) and reattach.
- Review summary stage that briefs the reviewer; loaded workflows get one
  unless a stage already has the summary role (disable it to opt out).
- omp harness.
- Stage graphs: `after` runs read-only stages side by side (`max_parallel`),
  and `fan_out` runs best-of-N candidates in their own worktrees for an auto
  or review-stage judge, applying the winner; Best-of-3 implement preset;
  `resume --retry-candidate`.
- Router providers: OpenRouter plus presets; research model-router profile.
- Harness effort levels discovered from local catalogs and editable per stage.
- Stage models picked from the harness catalog; project root detection and a
  repository line in the header.
- Optional Vim motions in TUI text inputs.
- Terminal theme that follows the terminal's colour scheme; Poimandres theme.
- Monitor completion view, `report` command, workspace snapshots, and usage
  extraction.
- Runs history index: search, filter, status cycle, rerun, compare, clean.
- `doctor` compatibility checks, shown on the Harnesses page; harness output
  capture script and fixtures.
- Router checks editable in the stage editor; verification evidence required
  for test stages.
- Action registry and command menu (`:`).
- `scripts/build.sh` release build/install script; `mr` alias symlink.
- Toolchain and dependency cache preparation for Amp orbs.
- Smoke-test frame marker (`MODEL_ROUTER_FRAME_MARKER`) and a shared PTY
  harness; detach/reattach smoke test and macOS smoke runs in CI; dependency
  audit job.

### Changed

- TUI split into focused modules; key hints use one format.
- Models picker moved to F7; `/` or `~` opens path entry.
- Harness checks are cached; queued input is drained before each redraw.
- Removed Claude Code provider routing.
- The offline demo config includes a mock review summary stage, so it stays
  offline.

### Fixed

- Shutdown when the terminal disconnects.
- macOS picker tests and process checks; smoke-test resize race.
- Flaky smoke tests: fixed sleeps replaced with waits on frames, screen
  content, and files; cursor-position replies survive split reads.
- Smoke tests for task previews and effort choices.
- Run path comparison in the cleanup history test.

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
