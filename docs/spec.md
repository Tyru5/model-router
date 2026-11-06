# Model Router

## Product contract

An installable Rust terminal application for personal use on Linux and macOS.
The user chooses a project, writes or imports a task, selects a workflow, assigns
a harness and native model/alias/mode to each stage, and starts the workflow. Every stage
can be interactive or unattended. The starter workflow is Codex/Astra research,
then interactive Claude Code implementation. Review is disabled by default.

Workflows can add, remove, duplicate, reorder, and disable stages. Model IDs are
open-ended; a catalog is assistance rather than a whitelist. Harness constraints
remain visible, including Amp's mode selection and provider-specific identifiers.

Provide editable stage presets for requirements, research, diagnosis, design,
planning, implementation, verification, code/security review, documentation,
handoff, and a blank custom stage. Provide Quick task, Full feature, Bug fix,
and Investigation workflow presets. Adding a preset never overwrites an existing
workflow or changes a saved run; reviews start disabled in workflow presets.

## Execution and handoff

- Execute argument vectors directly, without constructing shell commands.
- Unattended stages capture stdout/stderr and advance on successful completion.
- Interactive stages receive a real terminal. On exit, return to the router to
  inspect the handoff and advance, reopen, or stop.
- Preserve the original task, stage instructions, structured findings, decisions,
  changed files, tests, and unresolved issues. Retain complete attempt logs on disk.
- Use a small structured handoff in subsequent prompts, with artifact paths for
  details. Never treat a missing review verdict as a passing review.
- Verification (`test`) stages require structured handoffs and evidence: at
  least one passing required check, reported by the model or run by the router.
  Required failed or unrun checks, blocking findings, and missing evidence stop
  the run for inspection and retry. A stage that reports `not_applicable` with a
  reason waits at the human checkpoint and continues only after explicit
  acceptance, which is saved on the stage until a retry or repair resets it.
  Interactive verification must write its handoff file. Automatic repairs are
  reserved for enabled reviews.
- Any stage may declare router checks: argument vectors run in the workspace
  after the harness succeeds, each with a timeout. Results and logs are saved
  with the attempt, independent of the handoff. A required check that fails,
  times out, or cannot start blocks the stage; optional failures are recorded.
- Read-only stage intent is expressed in instructions. Effective execution
  permissions are supplied by the harness and the user's explicit overrides;
  the router is not a universal sandbox.
- Surface reported permission denials and errors. Unattended processes have no
  interactive stdin. A harness that does not report approval requirements clearly
  can be cancelled or timed out and reopened interactively.

## Optional reviews

Any number of review stages may be enabled. A structured blocking finding or a
failed required test requests a repair at the selected upstream implementation
stage (nearest implementation by default). Invalidate and rerun subsequent stages
so stale reviews cannot approve changed code. Allow two repair passes per run by
default. Suggestions alone do not trigger repair. Disabled reviews never gate a
run, and a run with no review ends after its last enabled stage. The repair
request reaches only the repaired stages up to the review that made it, and is
cleared once that review passes. Reaching the limit blocks the run; the user
may explicitly allow one more pass at a time (`resume --allow-repair`).

## Persistence and recovery

Save reusable workflows as TOML and snapshot them into each run. Save every
attempt, handoff, status transition, and explicit model override. Use atomic
writes and locks to protect run state and active workspaces. A crash, failure,
timeout, or cancellation never silently retries a possibly mutating stage.
A harness's whole process group ends with its stage (SIGTERM, a grace period,
then SIGKILL), including after a normal exit, and on Linux when mr dies. Each
attempt records its process group while running; the next drive stops a group
left behind by a crashed router before continuing. Git snapshot trees stay
reachable through `refs/model-router/runs/<run>/` until the run is cleaned.

The user can retry, switch model, reopen interactively, or resume a saved run.
Resuming preserves prior completed stages; it does not roll back filesystem
changes. Default timeout: 60 minutes per unattended stage.

Use the selected working directory by default. Optional Git worktrees start from
committed HEAD, remain on disk, and are never merged or committed automatically.
Store generated artifacts in a self-ignored `.model-router` directory. Retain
history until the user removes it. Aider's automatic commits are disabled.

Each save also writes a run summary that history lists instead of full records;
older runs fall back to their record. Listing never writes and reports
unreadable or invalid records as damaged rather than hiding them. Full records
load on demand. A rerun creates a new run from a saved run's task, workflow, and
adapter snapshot and records its source; the source stays unchanged. Attempt
comparison reads saved attempts and per-attempt handoffs, marking missing ones.
Cleanup previews selected runs and sizes before removing anything, skips runs
whose lock is held or that are open, and removes worktrees only when Git can
without force; branches stay.

## Adapters and model discovery

Built-in targets: Codex, Claude Code, Pi, Cursor Agent, Gemini CLI, OpenCode,
Aider, Goose, Amp, and GitHub Copilot CLI. Custom adapters declare executable,
unattended and interactive arguments, and optionally a model-list command.

Reuse native authentication and provider configurations. No credential store,
no automatic installation of harnesses, and no model inference during discovery.
Label account-reported, cached, provider-catalog, and unverified selections
distinctly. Do not claim every model can run in every harness. Direct provider
API clients, automatic task classification, and embedded IDE control are excluded.

## Interface

- A brief routing animation before the TUI, with any-key skip, `Ctrl+C` exit,
  and flag/environment bypass. No model calls or simulated loading operations.
- Workflow editor with task, project, stage order, enable toggle, execution mode,
  model selector, provider, reasoning effort, instructions, and extra arguments.
- Workflow picker with built-in presets and local/global template saving.
- Searchable stage catalog with purpose and default routing. Selecting a preset
  opens an editor; saving inserts after the current stage. Cancelling the picker
  or editor leaves the workflow unchanged. Preserve freely editable custom stages.
- Task/spec import from local Markdown/UTF-8 files, direct HTTP(S) text URLs,
  and GitHub issue/file links. Reuse `gh` authentication for GitHub access.
  Browse files or enter a path/URL, then edit an append/replace preview.
  Default to append when a task exists. Apply explicitly; importing never starts
  execution. Cancellation and errors preserve the current task. Limit the final
  task to 32 KiB, normalize BOM/line endings, and snapshot its contents into each
  run. Leave source files and issues unchanged. CLI `--task-file`/`--task-url`
  use the same importer before starting execution.
- Repository line under the header: name, branch or detached commit, changed
  file count, path; non-Git folders say so. Refreshed on project switch.
- Stage editor model field is a catalog selection (Enter), with Backspace for
  the harness default and `i` for an unlisted ID; a picked provider fills the
  Provider field. The draft survives cancel and catalog errors.
- Project detection when `-C` is omitted: the nearest ancestor of the current
  directory with `.model-router.toml`, else the Git root, else the current
  directory; never above a Git root. `-C` is literal. The TUI says so when the
  project differs from the current directory.
- Repository browser with type-to-filter, folder navigation, direct path entry,
  and persistent recent projects. Recognizes Git directories and `.git` files
  for linked worktrees/submodules; non-Git folders can also be selected.
  Saves workflow edits and loads the selected project's configuration.
- Run monitor with stage state, elapsed time, check results, logs, cancellation,
  handoff view, and explicit recovery actions. Finished runs show a results view:
  files changed during the run (from private Git snapshots), checks by source,
  unresolved findings, per-stage time, retries, and harness-reported usage, with
  unavailable values labelled; the full diff opens in a dialog. `report` prints
  the same summary from the CLI.
- Run history with live text and status filters, damaged-record rows,
  background-computed sizes, confirmed reruns, attempt comparison, and cleanup
  previews; harness availability/catalog view.
- CLI equivalents for initialization, validation, previews, execution, discovery,
  history (filters, JSON, sizes), rerun, compare, clean, and resume; all share
  the same execution engine.

## Acceptance checks

1. Starting the application requires no API key and does not launch a model.
2. Editing and reloading a workflow preserves models, modes, order, and toggles.
3. A mock research/implementation run passes the original task and findings
   between stages and succeeds without any review stage.
4. Disabled reviews never execute; enabled reviews with missing or malformed
   verdicts cannot pass silently.
5. Blocking review findings rerun the implementation and affected reviews, with
   the configured repair limit enforced.
6. Failure, permission denial, cancellation, and timeout stop the run and retain
   artifacts. Explicit resume does not rerun completed stages unnecessarily.
7. Interactive execution uses a terminal and returns to a handoff checkpoint.
8. Unicode prompts, paths with spaces, and shell metacharacters remain literal.
9. Worktree execution isolates tracked changes and leaves the original checkout
   and resulting worktree intact.
10. Test harness integrations without paid inference; distinguish mock-tested
    command contracts from live authenticated end-to-end verification.
11. Import a local or remote spec, edit and append/replace it, and pass its saved
    contents through all stages even after the source is changed or removed.
    Invalid documents and cancelled downloads must leave the existing task intact.
12. Add, edit, reorder, and persist stage/workflow presets through the real TUI.
    All built-in workflows execute in order with mocked harnesses, keep reviews
    optional, and stop when verification reports required failures or missing checks.
