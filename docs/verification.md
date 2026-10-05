# Verification

Verified locally on Linux with Rust 1.93.0. Installed executable reports
`model-router 0.1.0`.

| Check | Result |
| --- | --- |
| `cargo test --locked` | 141 tests passed |
| `cargo clippy --all-targets --locked -- -D warnings` | Passed |
| `cargo fmt --check` | Passed |
| `cargo +1.85 check --all-targets --locked` (MSRV) | Passed |
| `cargo +1.99 clippy --all-targets --locked -- -D warnings` | Passed |
| `cargo build --release --locked` | Passed; installation rebuilt the final changed code |
| `cargo install --path . --locked` | Installed successfully in the user's Cargo bin directory |
| Installed `model-router --version` | `model-router 0.1.0` |
| Installed import commands | `run --help` exposes `--task-file` and `--task-url`; installed executable matches the release build |
| Installed preset commands | `presets` lists 12 stages and four workflows; `init --preset` accepts the workflow keys |
| Installed `model-router preview --task 'Example task'` | Astra research, interactive Claude implementation, review disabled |
| Real terminal smoke tests | Workflow, repository browser, task import, stage presets, theme selection, and startup animation scripts passed using offline fixtures and actual PTYs |
| Terminal loss and shutdown | Eight real-PTY scenarios passed: idle/startup disconnect, unattended/interactive cancellation, terminals without SIGHUP, and normal SIGTERM/SIGHUP cleanup |
| Theme selection | All 12 palettes preview and save; Ctrl+N/P, cancellation, cross-project persistence, restart, themed startup, and invalid preferences verified |
| Stage harness settings | Stage isolation, native permission arguments, conflict validation, model discovery returning to the draft, cancellation, and persistence passed; workflow, preset, and theme PTY regressions also passed |
| Private GitHub task import | Imported `Tyru5/model-router/blob/main/docs/spec.md` into an editable TUI preview using the existing `gh` login; no workflow started |
| Native model discovery | Codex cache, Pi catalog, Cursor account list, and OpenCode catalog returned selections |

The tests cover original-task preservation, cross-stage handoffs, disabled reviews,
repair limits, invalid verdicts, rerunning affected reviews, explicit model
replacement, durable resume, timeouts, process-tree cancellation, SIGTERM cleanup,
workspace locks, stale run revisions, literal shell metacharacters, Unicode paths,
stream backpressure, configuration roundtrips, and Git worktree isolation.
Verification tests block test stages lacking a passing required check, require
one explicit acceptance for not-applicable reasons, and cover router check logs,
timeouts, cancellation, and optional and required failures.
Report tests cover Git snapshots (edits, untracked files, deletions, ignored
files, an untouched index), harness usage parsing, the completion view at 80×24
and 120×36, and the `report` CLI after an offline run writes a file.
Repository tests also cover folder navigation, `.git` files, symlink loops,
non-Git projects, empty searches, and invalid paths. A deterministic regression
test verifies locks release when their owner drops them, even if another process
inherited the file descriptor before executing its command. This fixes an
intermittent lock failure exposed by the full test suite.

Import tests cover local files, BOM/line-ending normalization, UTF-8 and binary
validation, the 32 KiB limit, nonblocking rejection of named pipes, text URL
errors, GitHub issues/files/tags, branch names containing slashes, read-only
GitHub requests, conflicting CLI flags, and source deletion after import.
Saved run tasks and stage prompts retain the imported contents.

History tests cover summary listing without run records, damaged records,
filters, reruns from snapshots, attempt comparison, symlink-safe sizes, cleanup
selection with held locks, the CLI commands, and the Runs page dialogs.

All four workflow presets execute with offline harnesses in their defined order,
passing the original task, stage instructions, and prior handoffs through each
enabled stage. Review stages remain skipped by default. Required failed and
unrun checks, and unstructured verification responses, stop execution before
downstream stages. Preset configs roundtrip through TOML and initialization
refuses to overwrite existing files. Cancelled stage drafts leave no new entry;
repeated stages have distinct IDs and repeated workflows have distinct names.

The terminal smoke test exercises editing/saving, stage toggles, Unicode paste,
80×24 and 120×36 layouts, native interactive input, handoff inspection, explicit
advancement, completion, run history, terminal restoration, and rejection of an
interactive review verdict that was printed without a handoff file.

`scripts/smoke_repos.py` verifies repository browsing, type-to-filter, parent
navigation, Unicode paths, recent projects surviving restart, workflow autosave,
per-repository configuration, invalid config/path recovery, and non-Git project
selection in a real terminal. Picking a repository never launches a model.

`scripts/smoke_imports.py` verifies local file navigation, text URL and GitHub
imports, editable append/replace previews, cancellation with late results,
recoverable errors, unchanged source files, and imported task contents across
research/implementation stages. It exercises 80×24 and 120×36 layouts and
checks terminal restoration. GitHub fixtures and a local HTTP server keep the
script independent of network access and account credentials.

`scripts/smoke_stages.py` verifies searchable preset lists and routing details,
stage editing/cancellation, insertion order, reordering, enable/mode toggles,
duplication, custom stages, workflow presets, optional reviews, task preservation,
and persistence across restart at 80×24 and 120×36. It also verifies that an
interactive test stage cannot pass by printing results without a handoff file.

`scripts/smoke_startup.py` checks visible animation frames, automatic completion,
consuming the skip key, flag/environment bypass, resizing to a small terminal,
paste, Ctrl+C/SIGTERM cleanup, and unaffected CLI output. It records the real
terminal animation as `startup.gif`; rendering is also tested at 120×36, 80×24,
40×10, and 1×1. Both color and `NO_COLOR` terminal runs passed.

`python3 scripts/smoke_shutdown.py` closes real PTYs while the router is idle,
animating, or running offline unattended/interactive harnesses. It requires
prompt exit without a panic, checks that harness children stop and cancellation
is saved, and verifies SIGTERM/SIGHUP still restore an open terminal. Cases
without a controlling terminal ensure shutdown does not depend on SIGHUP.
An optional binary path runs the same checks against a release or installed build.

This reproduces the former full-core CPU loop on terminal EOF. Crossterm's
`use-dev-tty` backend bounds its reads and polling after EOF; the interactive
forwarder also cancels on hangup, invalid input descriptors, or EOF. Terminal cleanup and
error reporting tolerate disconnected stdout/stderr.

`scripts/smoke_themes.py` checks all 12 themes in a real terminal, including
Ctrl+N/P previews, cancellation, saved preferences, repository switching,
restart, startup colours, and recovery from invalid preferences. Rust tests
cover failed saves, unchanged task/workflow contents, text contrast, and every
theme in all main views and dialogs. Theme preferences live separately from
workflow files. The stage smoke check waits for the resized layout before
typing into its workflow picker.

`scripts/smoke_harness_settings.py` opens stage settings from the main view,
edits Codex sandbox/approvals and Claude permissions, browses an offline model
catalog, validates timeout and conflicting argument errors, cancels drafts,
and reloads saved settings. It checks Ctrl+N/P navigation and layouts at
120×36, 80×24, and 65×20. Rust tests cover native arguments in both execution
modes, old configuration compatibility, unsupported settings, stage isolation,
and returning to the draft after model discovery failure. Permission controls
inherit native settings until explicitly changed. No test starts a paid model.

TUI tests cover the router checks editor (required, optional, and quoted lines,
kept timeouts, quoting errors, the detail panel) and, with a stubbed doctor
probe, the Harnesses page: background checking, version/tested/capability
columns at 80 and 140 columns, per-flag details, and the Workflow missing-flag
warning. No test runs a harness.

Local screen captures:

- `../.model-router/qa/workflow-120x36.txt`
- `../.model-router/qa/workflow-80x24.txt`
- `../.model-router/qa/interactive-checkpoint.txt`
- `../.model-router/qa/handoff.txt`
- `../.model-router/qa/completed.txt`
- `../.model-router/qa/repos-120x36.txt`
- `../.model-router/qa/repos-80x24.txt`
- `../.model-router/qa/repos-invalid-config.txt`
- `../.model-router/qa/import-preview-80x24.txt`
- `../.model-router/qa/import-github-120x36.txt`
- `../.model-router/qa/import-private-github-120x36.txt`
- `../.model-router/qa/stage-presets-120x36.txt`
- `../.model-router/qa/stage-presets-80x24.txt`
- `../.model-router/qa/workflow-presets-80x24.txt`
- `../.model-router/qa/full-feature-120x36.txt`
- `../.model-router/qa/startup.gif`
- `../.model-router/qa/startup.png`
- `../.model-router/qa/harness-settings-codex-120x36.png`
- `../.model-router/qa/harness-settings-claude-80x24.png`
- `../.model-router/qa/harness-settings-codex-65x20.png`

All execution tests use offline fixtures; none send paid inference requests.
Per-adapter coverage, checked on 2026-10-04:

| Adapter | Command contract test | Parser fixtures | `doctor` on this machine | Live authenticated run |
| --- | --- | --- | --- | --- |
| Codex | Both modes, native settings | 4 schema-derived, 0 captured | 0.160.0, tested, ok | Unverified |
| Claude Code | Both modes, native settings | 5 schema-derived, 0 captured | 2.1.289, tested, ok | Unverified |
| Pi | Both modes | 5 schema-derived, 0 captured | 1.0.2, tested, ok | Unverified |
| Cursor Agent | Both modes, native settings | 3 schema-derived, 0 captured | 2026.10.01-e373342, tested, ok | Unverified |
| Gemini CLI | Both modes, native settings | 3 schema-derived (source and docs), 0 captured | Not installed | Unverified |
| OpenCode | Both modes | 3 schema-derived, 0 captured | 2.0.22, tested; missing `--variant`, `--interactive` (interactive) | Unverified |
| Aider | Both modes | 1 schema-derived transcript, 0 captured | Not installed | Unverified |
| Goose | Both modes | 1 schema-derived transcript, 0 captured | Not installed | Unverified |
| Amp | Both modes, mode flag | 3 schema-derived (docs), 0 captured | 0.0.1791134020-gd49eb2, tested, ok | Unverified |
| GitHub Copilot CLI | Both modes | 1 schema-derived transcript, 0 captured | Not installed | Unverified |

Command contracts: `adapter_contracts_keep_prompt_and_models_literal` builds
both modes for every adapter, `tests/harness_settings.rs` covers native
settings, and `amp_uses_mode_not_model` covers Amp's mode flag. Parser fixtures
come from installed sources or official documentation ([details](adapters.md#parser-fixtures));
none are captured yet. `doctor` compares the router's flags with installed help
and never checks authentication. macOS is a target platform and has not been
executed in this Linux environment.
