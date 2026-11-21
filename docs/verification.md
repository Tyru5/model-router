# Verification

Verified locally on Linux with Rust 1.100.0 (and MSRV 1.85).

| Check | Result |
| --- | --- |
| `cargo test --locked --no-fail-fast` | 541 tests passed (0 failed, 2 ignored helper tests) |
| `cargo clippy --all-targets --locked -- -D warnings` | Passed |
| `cargo fmt --check` | Passed |
| `cargo +1.85 check --all-targets --locked` (MSRV) | Passed |
| `python3 scripts/smoke_shutdown.py` | 8/8 real-PTY disconnect and signal scenarios passed |
| `presets` | 15 stage presets and 5 workflow presets; `init --preset` and `init --detect` verified |
| `themes` | 14 themes covered by unit tests (contrast, rendering in all views/dialogs) and `smoke_themes.py` |
| Guardrails & server (`tests/guardrails.rs`, `tests/serve.rs`) | Stall timeouts, `protected_paths`, pre-ship secret scanning, trust gates, `handoff_repair`, worktree `apply`, budget `on_exceed = "warn"`, billing API/subscription split, MCP line/worker bounds, `notifications/cancelled`, `notifications/progress`, and finished-run handle eviction passed |
| Previous PTY smoke runs (requiring `uv` / `pyte`) | Workflow, repository browser, task import, stage presets, theme selection, harness settings, startup animation, detach/reattach, and diff review scripts pass offline in CI (`ubuntu-latest`) |

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

All five workflow presets execute with offline harnesses in their defined order,
passing the original task, stage instructions, and prior handoffs through each
enabled stage. Review stages remain skipped by default. Required failed and
unrun checks, and unstructured verification responses, stop execution before
downstream stages. Preset configs roundtrip through TOML and initialization
refuses to overwrite existing files. Cancelled stage drafts leave no new entry;
repeated stages have distinct IDs and repeated workflows have distinct names.

The terminal smoke test exercises the `Space` menu, editing the task in its
panel, selecting stages in the strip, saving, stage toggles, Unicode paste,
80×24 and 120×36 layouts, native interactive input, the needs-you card, handoff
inspection, explicit advancement, a finished run's brief, run history, terminal
restoration, and rejection of an interactive review verdict that was printed
without a handoff file.

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
no animation on a second launch of the same version, consuming the skip key, flag/environment bypass, resizing to a small terminal,
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

`scripts/smoke_themes.py` checks all 14 themes in a real terminal, including
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
Per-adapter coverage, checked on 2026-11-07:

| Adapter | Command contract test | Parser fixtures | `doctor` on this machine | Live authenticated run |
| --- | --- | --- | --- | --- |
| Codex | Both modes, native settings | 4 schema-derived, 1 captured | 0.167.0, tested, ok | Captured 2026-11-05 |
| Claude Code | Both modes, native settings | 12 schema-derived, 1 captured | 2.1.319, tested, ok | Captured 2026-11-07 |
| Pi | Both modes | 5 schema-derived, 1 captured | 1.5.0, tested, ok | Captured 2026-11-07 |
| Oh My Pi | Both modes | 4 captured | 18.18.0, tested, ok | Captured 2026-11-07 |
| Cursor Agent | Both modes, native settings | 5 schema-derived, 1 captured | 2026.11.05-86047df, tested, ok | Captured 2026-11-07 |
| Gemini CLI | Both modes, native settings | 3 schema-derived (source and docs), 0 captured | Not installed | Unverified |
| OpenCode | Both modes | 4 schema-derived, 1 captured | 2.2.6, tested, ok | Captured 2026-11-07 |
| Aider | Both modes | 1 schema-derived transcript, 0 captured | Not installed | Unverified |
| Goose | Both modes | 1 schema-derived transcript, 0 captured | Not installed | Unverified |
| Amp | Both modes, mode flag | 4 schema-derived (docs), 1 captured | 0.0.1794081633-g6e983f, tested, ok | Captured 2026-11-07 |
| GitHub Copilot CLI | Both modes | 1 schema-derived transcript, 0 captured | Not installed | Unverified |

Command contracts: `adapter_contracts_keep_prompt_and_models_literal` builds
both modes for every adapter, `tests/harness_settings.rs` covers native
settings, and `amp_uses_mode_not_model` covers Amp's mode flag. Parser fixtures
come from installed sources, official documentation, or sanitized real output
from `scripts/capture_harness_output.py` ([details](adapters.md#parser-fixtures)). `doctor` compares the router's flags with installed help
and never checks authentication. macOS is a target platform and has not been
executed in this Linux environment.
