# Verification

Verified locally on Linux with Rust 1.93.0. Installed executable reports
`model-router 0.1.0`.

| Check | Result |
| --- | --- |
| `cargo test --locked` | 28 tests passed |
| `cargo clippy --all-targets -- -D warnings` | Passed |
| `cargo fmt --check` | Passed |
| `cargo build --release --locked` | Passed; installation rebuilt the final changed code |
| `cargo install --path . --locked` | Installed successfully in the user's Cargo bin directory |
| Installed `model-router --version` | `model-router 0.1.0` |
| Installed `model-router preview --task 'Example task'` | Astra research, interactive Claude implementation, review disabled |
| Real terminal smoke tests | Workflow, repository browser, and startup animation scripts passed using offline fixtures and actual PTYs |
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

The terminal smoke test exercises editing/saving, stage toggles, Unicode paste,
80×24 and 120×36 layouts, native interactive input, handoff inspection, explicit
advancement, completion, run history, terminal restoration, and rejection of an
interactive review verdict that was printed without a handoff file.

`scripts/smoke_repos.py` verifies repository browsing, type-to-filter, parent
navigation, Unicode paths, recent projects surviving restart, workflow autosave,
per-repository configuration, invalid config/path recovery, and non-Git project
selection in a real terminal. Picking a repository never launches a model.

`scripts/smoke_startup.py` checks visible animation frames, automatic completion,
consuming the skip key, flag/environment bypass, resizing to a small terminal,
paste, Ctrl+C/SIGTERM cleanup, and unaffected CLI output. It records the real
terminal animation as `startup.gif`; rendering is also tested at 120×36, 80×24,
40×10, and 1×1. Both color and `NO_COLOR` terminal runs passed.

Local screen captures:

- `../.model-router/qa/workflow-120x36.txt`
- `../.model-router/qa/workflow-80x24.txt`
- `../.model-router/qa/interactive-checkpoint.txt`
- `../.model-router/qa/handoff.txt`
- `../.model-router/qa/completed.txt`
- `../.model-router/qa/repos-120x36.txt`
- `../.model-router/qa/repos-80x24.txt`
- `../.model-router/qa/repos-invalid-config.txt`
- `../.model-router/qa/startup.gif`
- `../.model-router/qa/startup.png`

All execution tests use the offline Python fixture; none send paid inference
requests. Built-in adapter command construction is tested and checked against
CLI help or official docs, but live authenticated end-to-end execution against
all ten services is unverified. macOS is a target platform and has not been
executed in this Linux environment.
