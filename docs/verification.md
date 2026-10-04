# Verification

Verified locally on Linux with Rust 1.93.0. Installed executable reports
`model-router 0.1.0`.

| Check | Result |
| --- | --- |
| `cargo test --locked` | 58 tests passed |
| `cargo clippy --all-targets --locked -- -D warnings` | Passed |
| `cargo fmt --check` | Passed |
| `cargo build --release --locked` | Passed; installation rebuilt the final changed code |
| `cargo install --path . --locked` | Installed successfully in the user's Cargo bin directory |
| Installed `model-router --version` | `model-router 0.1.0` |
| Installed import commands | `run --help` exposes `--task-file` and `--task-url`; installed executable matches the release build |
| Installed preset commands | `presets` lists 12 stages and four workflows; `init --preset` accepts the workflow keys |
| Installed `model-router preview --task 'Example task'` | Astra research, interactive Claude implementation, review disabled |
| Real terminal smoke tests | Workflow, repository browser, task import, stage presets, theme selection, and startup animation scripts passed using offline fixtures and actual PTYs |
| Theme selection | All 12 palettes preview and save; Ctrl+N/P, cancellation, cross-project persistence, restart, themed startup, and invalid preferences verified |
| Stage harness settings | Stage isolation, native permission arguments, conflict validation, model discovery returning to the draft, cancellation, and persistence passed; workflow, preset, and theme PTY regressions also passed |
| Private GitHub task import | Imported `Tyru5/model-router/blob/main/docs/spec.md` into an editable TUI preview using the existing `gh` login; no workflow started |
| Native model discovery | Codex cache, Pi catalog, Cursor account list, and OpenCode catalog returned selections |

The tests cover original-task preservation, cross-stage handoffs, disabled reviews,
repair limits, invalid verdicts, rerunning affected reviews, explicit model
replacement, durable resume, timeouts, process-tree cancellation, SIGTERM cleanup,
workspace locks, stale run revisions, literal shell metacharacters, Unicode paths,
stream backpressure, configuration roundtrips, and Git worktree isolation.
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

All execution tests use the offline Python fixture; none send paid inference
requests. Built-in adapter command construction is tested and checked against
CLI help or official docs, but live authenticated end-to-end execution against
all ten services is unverified. macOS is a target platform and has not been
executed in this Linux environment.
