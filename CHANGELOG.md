# Changelog

All notable changes to model-router are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Release notes for a `vX.Y.Z` tag come from its section below.

## [Unreleased]

### Security

- TUI `git` commands now pass `-c core.fsmonitor=false -c core.hooksPath=/dev/null`, and the diff view's `D` (`git difftool`) refuses repository-local `diff.tool`, `diff.guitool`, `diff.external`, `difftool.*`, `merge.tool`, `merge.guitool`, `mergetool.*`, and `filter.*` config entries and runs against a fixed, sanitized temporary `GIT_DIR` (requiring Git 2.26+).
- Router Git operations that touch working-tree files, attributes, patches, or worktree checkouts (`snapshot`, `storage`, `ship`, `fanout`, `delegate_apply`) now run against a fixed, private sanitized `GIT_DIR` config (requiring Git 2.26+) so repository-local and worktree-local `filter.*`, `diff.*`, and `merge.*.driver` commands cannot execute or be swapped in concurrently; worktree creation and `ship` fail closed when affected files rely on suppressed repo-local filters, and stages with `protected_paths` or blocking `policies` stop before launch if the pre-stage snapshot fails. The sanitized `GIT_DIR` and the staged Harnesses `[t]` capture script live in a private `tmp` directory inside the socket directory, which sandboxed stages cannot write. Partial clones (`extensions.partialClone`, `remote.*.promisor`) and repository-local `lfs.storage` are refused. `ship` also checks clean filters on deleted paths restored since the run. The sanitized config keeps repo-local Git LFS endpoint URLs and remote selection (`lfs.url`, `remote.*.url`, `remote.*.lfsurl`, `remote.lfsdefault`, `remote.lfspushdefault`, `branch.*.remote`), and new worktrees are checked for disabled smudge filters with the new worktree's own config, not the source worktree's `config.worktree`. Router `git status` calls pass `--ignore-submodules=dirty` so a submodule's own config cannot run filters (worktree mode checks each populated submodule with its own sanitized config), snapshots are refused when a new or changed file uses a disabled repo-local clean filter, and a fan-out patch apply is refused when a patched file uses a disabled repo-local smudge filter. The apply preview refuses the same files. An empty repo-local `filter.<driver>.clean`, `smudge`, or `process` still turns a global filter off in the sanitized config. The Harnesses `[t]` result shows the embedded script it ran, not a command for the checkout's file. Removing a saved run's worktree checks it with the sanitized config, then removes it with `--force`, so Git's own recheck cannot run repo-local filters. A worktree whose tracked files use a disabled repo-local clean filter is kept, since its status cannot be trusted. The fan-out apply check also reads `.gitattributes` rules the patch adds. Worktree mode refuses sparse checkouts and uses one sanitized config and commit for the filter check and the checkout. A repo-local filter that a later empty value turns off no longer blocks snapshots, shipping, or checkout.
- Task URL imports no longer relax `PublicResolver` or redirect SSRF checks based on pre-request DNS lookups, and redirect checks now also block `*.localhost` and trailing-dot `localhost.`.
- TUI Harnesses `[t]` now verifies that `scripts/capture_harness_output.py` is a non-symlink file matching the build's script byte-for-byte before enabling or executing live tests, launches Python with `-I` (isolated mode) to prevent stdlib shadowing in `scripts/`, and names the script in the confirmation prompt.
- External `secret_scanner` commands now run in their own process group (reaped on exit or timeout with start-time verification) with `GIT_CONFIG_PARAMETERS` disabling Git hooks and fsmonitor.
- HTTP and delegation bearer-token checks now use constant-time `ring::hmac::verify` over per-process keyed digests, and HTTP bearer tokens are generated with `ring::rand::SystemRandom`.

### Fixed

- `serve`'s background schedule ticker no longer panics on hosts with monotonic uptime under 60 seconds.
- `schedule_task` now revokes prior schedule approvals while holding `schedules.lock`, preventing a race with concurrent `schedules approve`.
- Scheduled runs no longer advertise `run_*` and `schedule_*` delegation tools that `run_tool` always rejects.
- HTTP header line reading now decodes UTF-8 after reading a full line instead of per buffer chunk, avoiding spurious errors when multi-byte UTF-8 spans an 8 KiB read boundary.
- `delegation::broker::Broker::start` resolves the bridge executable before binding and removes partial socket/token files if startup fails.
- Updated `README.md` and `site/index.html` starter workflow examples to show Codex inheriting its native default model instead of `gpt-6-astra`.
- `just lint` now checks Python scripts and examples with `ruff check`.

## [0.3.0] - 2026-11-24

### Added

- TUI tour: on the first launch, a panel above the status line walks from
  the Harnesses page through stages, the task, and starting a run to its
  completion, naming the next key for each step and run state. `F1` shows or
  hides it; `<config dir>/tour.json` records it finished or hidden so it
  does not reopen. Earlier users (recent projects, no `tour.json`) get a
  one-time status line about `F1` instead. See `docs/tui.md#tour`.

- Delegation: a stage with `[workflows.stages.delegation]` gives its agent
  the router's MCP tools (`delegate_task`, `task_status`, `task_wait`,
  `task_send`, `task_cancel`, `task_apply`, `run_*`, `schedule_*`) through
  `mr __mcp-bridge` and a per-turn Unix socket and token. Tasks run on named
  `[delegates.*]` or allowlisted harness/model/provider targets, in their own
  OS sandbox (`shared` read-only or `worktree`), never above the parent's
  privilege ceiling. Finished tasks wake the parent with a follow-up turn.
  Worktree tasks return sealed patches the parent applies with `task_apply`.
  Parents: Claude Code, Codex, OpenCode, and custom adapters with
  `mcp_config_args`; interactive stages too. Delegates need
  `model-router trust`. See `docs/delegation.md`.
- Agent-launched runs (`run_launch`) are driven by the TUI or `serve`; the
  CLI leaves them pending for `model-router resume`.
- Schedules: `schedule_task` writes `.model-router/schedules.json`; a
  schedule fires only after `mr schedules approve ID`. `mr schedules
  list|approve|pause|enable|delete|tick`; `serve` fires due schedules and
  accepts `POST /schedules/<id>/trigger`. `approve --digest D` (and the TUI
  confirm dialog) refuse a schedule changed after it was shown; `list --json`
  includes `digest`.
- Reports, previews, history, and the Runs page show tasks, wake turns, and
  their cost.
- Global and project config layering: loading `<project>/.model-router.toml`
  layers `<config dir>/config.toml`'s `[providers.*]`, `[adapters.*]`,
  `[delegates.*]`, and `[pricing.*]` underneath project entries (project keys
  win on collision). Global entries stay trusted while project-defined and
  shadowing entries require `model-router trust`; saving a project config omits
  unmodified global entries, and saving the global `config.toml` keeps them.
- Built-in pricing table entries for `gpt-6`, `gpt-6-astra`, and `gpt-6.2-sol`
  in `src/cost/prices.toml`.
- Prompt `context_files` (`Vec<String>` on workflows and stages, up to 32
  repo-relative paths or globs and 32 KiB total, symlink-safe, UTF-8 text only,
  never reading `.git` or `.model-router`) and tunable handoff budgets
  (`handoff_budget_bytes` default `14000`, `handoff_each_bytes` default `5000`,
  validated in `1024..=200000`), shown in `preview` and `handoffs`.
- Transient stage retries (`[workflows.retry]` and `[workflows.stages.retry]`
  with `max_attempts` `0..=5`, `base_delay_seconds` `1..=600`,
  `max_delay_seconds` from the base to `3600`) that
  retry unattended transient provider or network errors (HTTP
  429/502/503/504/529, rate limits, overloaded, connection reset/timeout) on
  the same harness and model with interruptible capped exponential backoff.
  Fan-out candidates retry in a fresh worktree; a stage `retry` on a
  delegating stage is a config error. Policy checks cover edits left by failed
  attempts, and a retried run writes one `run_end` metric.
- Trust-gated lifecycle hooks (`[workflows.hooks]` and
  `[workflows.stages.hooks]` with `before_stage`, `after_stage`, `before_ship`,
  `on_failure`), executed as argument vectors in their own process group with
  `MODEL_ROUTER_HOOK` and stage context environment variables, and included in
  project trust and run approval digests. `timeout_seconds` is `1..=3600`.
  Stage hooks run once around a fan-out round (candidates start from the
  files `before_stage` prepared), and edits from `after_stage`
  and `on_failure` hooks are captured in the stage's changes.
- Declarative diff policies (`[[workflows.policies]]` with
  `action = "block" | "advise"`, non-backtracking `pattern`, `paths`,
  `message`) that block prohibited added lines at stage end, fan-out candidate
  post-processing and selection, `apply`, and `ship` (failing closed), or
  inject path/diff advisories into review prompts. Whole added lines are
  scanned; candidates are scanned as text diffs (binary-looking files read as
  text, non-ASCII and quoted paths decoded).
- Conditional stage execution (`when_changed` repo-relative globs on stages,
  up to 32, skipping unmatched stages in Git repositories; a fan-out judge with
  candidates waiting never skips) and multi-review quorum gating
  (`review_quorum` on workflows).
- Live mid-stream budget enforcement for unattended streaming stages with
  `on_exceed = "stop"`, terminating the harness process group as soon as
  streamed usage exceeds stage or run USD/token caps. Caps count every live
  worker (parallel stages, fan-out candidates, delegating parents and their
  tasks), an exceeded run cap stops workers in every stage, a stopped worker
  keeps the usage it streamed, and a run whose recorded spend is over a stop
  cap when its last stage finishes ends Blocked.
- `model-router schedules add` CLI (`--workflow`, `--task`/`--task-file`,
  `--every`/`--at`/`--cron`/`--webhook`, `--tz`, trust-gated `--approve`), plus
  5-field cron expressions (`cron`; day-of-month OR day-of-week when both are
  set) and timezone support (`tz`: `"local"`, `"UTC"`, or `"±HH:MM"`) across
  the CLI and `schedule_task` MCP tool. A `tz`-only `schedule_task` edit
  updates an `at` or `cron` trigger and is refused for `every` and webhooks.
- `model-router doctor` checks for `[harness_versions]` pins (`--strict` fails
  on drift), `[providers.*]` API key variable presence, and offline harness
  auth-readiness hints without reading secret contents or making network
  requests; `command.json` records detected `harness_version`.
- `model-router cost --all-projects` to aggregate cost history across recent
  projects, and `--metrics-file <PATH>` / `MODEL_ROUTER_METRICS_FILE` for
  lock-serialized append-only JSONL operational event export (`run_start`,
  `run_end`, `attempt_start`, `attempt_end`) with no task text, prompts, or
  secrets.
- `model-router serve`: add `fork_run`, `apply_run` (default
  `check_only: true`), `get_handoffs`, `get_cost`, `get_stats`, and
  `list_schedules` tools over both MCP and HTTP; add unauthenticated
  `GET /health` liveness endpoint on `serve --http`; advertise MCP `resources`
  on `serve --mcp` and serve `model-router://runs/<run_id>/review` and
  `.../diff` via `resources/list` and bounded, symlink-safe `resources/read`.
- TUI Workflow settings editor (`W` / `Space w W`), expanded Stage editor (`e`:
  `billing`, stage `budget`, `require_router_checks`, `sandbox_writable`,
  `sandbox_network`, `sandbox_fallback`, `delegation`), and Runs page parity:
  fork from a stage checkpoint (`K`), preview and confirm ad-hoc ship (`S`),
  inspect handoff budgets and truncation warnings (`H`), view status
  transitions timeline (`l`), raise token budget caps (`B`), inspect delegated
  tasks and their handoffs/sealed patches or cancel a single task (`g`), and
  manage/approve project schedules (`u`).

### Changed

- The repository is public under the MIT license. Install with the
  `model-router-installer.sh` attached to each GitHub release, or
  `cargo install --git https://github.com/Tyru5/model-router --locked`.
  The landing page links the source, releases, changelog, and issues.
- `model-router doctor --json` prints an object,
  `{"harnesses": [...], "providers": [{name, api_key_env, status}]}`, instead
  of a bare array of harness reports.
- Built-in starter workflow and stage/workflow presets now leave stage `model`
  blank (`""`) where `gpt-6-astra` was previously hardcoded, so stages inherit
  each harness's native default model out of the box.

### Removed

- The R2 download mirror: `scripts/install.sh`, the **R2 downloads**
  workflow, and `https://model-router.tiru5.me/install.sh`. GitHub release
  assets no longer need a token, so the release installer replaces them.

### Security

- Sandboxed stages can no longer make the router run commands outside the
  sandbox through Git. A `workspace-write` stage could write `.git/config`
  (an fsmonitor or filter command), add a hook, or swap in its own `.git`,
  and the router's unsandboxed `git add -A` after the stage would run it.
  Git config, `config.worktree`, and hooks are now read-only in every
  sandbox mode, the `.git` directory (or a linked worktree's `.git` file and
  `commondir`) cannot be replaced, a stage refuses to start when a Git config
  file or include target it could write would be read, a `workspace-write`
  stage refuses to start while its Git config or hooks entry is a symbolic
  link it could replace, and the router's own Git commands disable fsmonitor
  and hooks.
- Ship's Git commands pass `-c core.fsmonitor=false -c
  core.hooksPath=/dev/null` like the router's other Git commands, and `gh`
  gets the same through `GIT_CONFIG_PARAMETERS`, so the Git commands it runs
  do too. A stage could write the hooks directory a repo's
  `core.hooksPath` names (husky's `.husky/_`, say), and ship then ran those
  hooks outside the sandbox while planning. Your own hooks, `pre-push`
  included, no longer run on ship.

### Fixed

- Handoff repair saves the run before its follow-up starts, so `run.json` no
  longer names the finished harness's process group while the follow-up
  runs (a crash then could leave a stale group recorded).
- Budgets now count fan-out candidate spend. `cost::stage_totals` summed only
  a stage's own attempts, so run and stage caps never saw candidate spend, and
  `model-router cost` left candidates out of its history. A repair rewind past
  a fan-out stage also dropped the round's candidates from `run.json`, and
  their spend with them; they are now kept as the stage's `discarded`
  candidates and count in budgets, `report`, and `cost`.
- Follow-up turns that resume a harness session (handoff repair, delegation
  wake turns, delegated task follow-ups) count only their own spend, in the
  live budget monitor and in recorded usage. Codex reports the session's
  token totals and Claude Code 2.1.277+ (by detected version) its session
  cost, which the router added to what the session had already spent.
- `cost --since`, the MCP `get_cost` tool, and schedule `--every` refuse
  non-ASCII and out-of-range durations (`7日`, `99999999999999999w`,
  `100000000d`) instead of panicking. `--every` takes at most 10 years, and a
  hand-edited interval past the date range has no due time.
- Ctrl+C ends commands that do not watch for cancellation (`trust`, `apply`,
  `ship`, `doctor`, and others) instead of being ignored. Once `ship` is
  confirmed, Ctrl+C ends only its Git or `gh` command, and it reports what
  was already done; a confirmed `apply` finishes and ignores Ctrl+C. Ctrl+C at a `run` or `resume` prompt saves
  and stops at once instead of cancelling the next stage.
- `every` and `cron` schedules fire once after downtime, at the latest
  missed occurrence, instead of once per missed occurrence. A cron schedule
  finds it by searching back from now in widening windows.
- `serve`: a `confirm_stage` or `retry_stage` that fails to load a run no
  longer leaves the run tied to the caller's project, which made later calls
  for it fail with "belongs to project". A webhook trigger whose run fails to
  start answers `500` with that error instead of a 409.
- A stage `sandbox = "by-role"` looser than the workflow's `sandbox_default`
  needs `model-router trust`, like any other sandbox downgrade.
- Delegation run tools: `run_list` with `status` reports `has_more` for the
  filtered list, `run_read` pages a log with invalid UTF-8 without skipping
  bytes, and a run record with no stages is an error instead of a panic.
- TUI: a delegated task's handoff and patch open files in the task's
  worktree (or its run's checkout), not the focused run's. While a folder
  loads, the folder picker ignores moves and Enter, and `←` goes up from the
  folder being loaded. Output search no longer caches matches from stale
  lines, which could misplace highlights or panic.
- `schedules tick` counts a failed occurrence once.

## [0.2.1] - 2026-11-21

### Added

- `scripts/install.sh`: installs `model-router` and `mr` on Linux or macOS from
  the release download bucket, with SHA-256 verification, `--version`, and
  `--to`. The **R2 downloads** workflow copies each release's archives to a
  Cloudflare R2 bucket and uploads the installer there, since GitHub release
  assets of the private repository need a token.

### Security

- `serve --http`: enforce the 64 KiB header cap while reading lines, cap long-lived event streams and `wait_run` requests at 24 of the 32 authenticated connection slots so quick calls (`cancel_run`, `run_status`) stay runnable, and verify project trust once per call.
- Cap config and benchmark reads at 2 MiB and provider model / Codex catalog reads at 16 MiB; harden project trust approvals, OS sandbox path resolution, protected-path validation, pre-ship secret scanning, and terminal escape cleaning on handoff and progress feeds.

### Changed

- Replace `fs2` with direct `libc::flock` in `src/storage.rs`, replace `ctrlc` (and its dedicated signal thread and duplicate `nix` version) with async-signal-safe `libc::sigaction` handlers (`SIGINT`, `SIGTERM`, `SIGHUP`) in `src/main.rs`, and trim `chrono` default features to `clock`, `serde`, and `std`.
- Pool HTTP clients across providers, task imports, and webhooks; use persistent notification/reaper worker threads; parallelize `doctor` harness checks and provider pricing refreshes; index pricing tables by model/family with case-insensitive lookup.
- Optimize engine, snapshot, ship, and storage paths: skip fsync on regenerable scratch artifacts, stream patch lines and git status/changed paths with bounded stderr capture, reduce prompt/history allocations, and harden graph/fan-out scheduling and state recovery.
- Improve TUI responsiveness and per-frame efficiency: conditional animation/elapsed timers, adaptive idle event polling (`250 ms`), backgrounded saved-run sizing and worktree apply planning, viewport-windowed pickers and diff/file lists, coalesced wrapped textarea spans, and cached output search, diff stats, and completion briefs.

### Fixed

- Detect TUI PTY hangup without a controlling terminal via zero-timeout `poll` (`POLLHUP`/`POLLERR`/`POLLNVAL`), stopping active runs and restoring terminal modes across normal exit, error paths, and panics.
- Fix permanent `StatsCache` loading locks when switching projects, stale one-frame output search status, multi-line `Ask::ApplyWorktree` dialog truncation, and overflow-prone UI/duration calculations.
- Harden process-group capture, interactive terminal-host teardown, and crash-recovery termination under signal and timeout races.
- Detect TUI terminal hangup on macOS, where `poll` reports `POLLHUP` only when `POLLIN` is requested.
- On macOS, retry reading a process group member's run marker when `sysctl(KERN_PROCARGS2)` fails briefly with `EIO` or `EINVAL`, so recovery and teardown no longer skip a leaderless group that is the run's own.

## [0.2.0] - 2026-11-20

### Added

- `init --detect`: detects installed harnesses on `PATH`, adapts starter or
  preset stages to them, leaves stage models and effort blank to inherit each
  harness's default, keeps read-only intent when replacing an unavailable
  harness (mapped to the new harness's native read-only setting, else router
  `sandbox = "read-only"`), and moves review stages off the implementing
  harness when another is installed.
- Stage `stall_timeout_seconds` (editable in Harness settings and shown in
  `preview`): stops an unattended stage when neither stdout nor stderr produces
  output for that long; rejected for harnesses that print only at exit
  (`gemini` and custom adapters with `output = "gemini-json"`, including
  fan-out candidates), and ignored for interactive stages (`preview` notes
  this).
- Workflow `protected_paths` globs (`*`, `**`, `?`): enforced for ordinary
  stages (stage stops **blocked**), fan-out candidates (candidate stops
  **blocked** with its worktree retained), and candidate selection; `ship` and
  `apply` refuse matching files.
- Pre-ship secret scanning: `ship.secret_scan` (default `true`) scans the full
  diff of the planned files only (no size cutoff; files Git treats as binary
  are read as text) at plan time and right before commit, listing at most 20
  hits then `and N more` without printing secret values; optional
  `ship.secret_scanner` (argv, no shell, trust-gated, fails closed) runs in
  both phases with `cwd` = workspace, `{files}`, `MODEL_ROUTER_SHIP_FILES`, and
  `GIT_INDEX_FILE` pointing at a private index of `HEAD` plus the planned files
  (leaving the user's index untouched; planning writes blob objects to the
  object database).
- Workflow `handoff_repair = true`: when an unattended stage exits 0 without a
  valid handoff, sends one extra (possibly billed) follow-up on the same
  session asking only for handoff JSON (`min(timeout_seconds, 120)` timeout;
  process group recorded for crash recovery; skips and failures logged in stage
  output). Works on `codex` (via `codex exec resume`, passing sandbox as
  `--config sandbox_mode="…"`, dropping stage extra args other than
  `-c`/`--config` pairs, and excluding local providers `ollama`/`lmstudio`),
  `claude`, `pi`, `opencode`, and custom adapters with `follow_up_args`
  (`resume_args` enables interactive takeover only).
- `apply RUN_ID [--check] [--yes]` and TUI Runs `A`: applies a finished
  worktree run's changes back into the project checkout without committing,
  using `git apply` with a 3-way merge fallback in a private index and atomic
  restore on conflict; `--check` reports clean apply, 3-way merge, or conflict
  without touching the checkout or its index (merge prediction requires
  Git 2.32+).
- Billing classification and budget actions: per-stage
  `billing = "api" | "subscription"` (precedence: stage `billing`, router
  provider → `api`, then harness default—`codex`, `claude`, `cursor`, `gemini`,
  `copilot` default to `subscription`; `pi`, `omp`, `opencode`, `aider`,
  `goose`, `amp`, and custom adapters default to `api`), `cost` splits totals
  into billed API spend and subscription API-equivalent estimates and notes
  attempts with unknown cost (never counted as `$0`), and budgets accept
  `on_exceed = "stop" | "warn"`.
- MCP (`serve --mcp`): handles `notifications/cancelled` to abort in-flight
  `wait_run` calls without a response, and emits `notifications/progress`
  during `wait_run` when `_meta.progressToken` is set.
- `docs/safety.md#not-enforced`: table of guarantees that depend on harnesses
  or the OS rather than the router.
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
- omp harness. Declarative harness spec table (including `follow_up`,
  `streams`, `billing`, and `read_only`); custom adapters gain `output`,
  `session`, `resume_args`, `follow_up_args`, `model_flag`, `effort_flag`,
  `prompt`; `harnesses [--spec]`.
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

- Modularized `src/engine.rs`, `src/process.rs`, `src/handoff.rs`, and
  `src/main.rs` into focused submodules (`src/engine/`, `src/process/`,
  `src/handoff/`, `src/cli/`), and updated the repository's own
  `.model-router.toml` to v2 with `sandbox_default = "by-role"`, `summary`,
  and router checks.
- `serve`: finished run handles in memory are capped at 128 (older inactive
  handles are evicted while disk records stay intact), and `tools::exists`
  checks a static list instead of building schemas per call.
- Interactive stages echo keystrokes in about 1 ms instead of 40–80 ms: the
  terminal host sends output as soon as it reads it, and mr wakes its
  supervisor and attachment when input or output is queued instead of
  polling every 40 ms.
- TUI: the Runs sidebar draws in constant time however many runs are saved
  (0.4 ms a frame at 10,000 runs, was 38 ms): it builds only the rows that
  can be on screen, skips search matching without a search, and saved tasks
  are cleaned once when listed rather than on every frame.
- Graph and fan-out stages: each harness's output is formatted on its own
  worker thread, so the scheduler, which also runs checks, gets formatted
  lines rather than raw output.
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

- Failed-stage messages show the exit code as a number (`exited with code 3`,
  not `Some(3)`) or say the harness was stopped by a signal.
- An interactive stage no longer fails, leaving its harness running
  unrecorded in its terminal host, when a screen redraw passes 1 MiB (a
  large truecolour screen): the host splits redraws into frames mr accepts,
  and an unreadable host stream now ends the harness's process group.
- Pastes into an interactive stage are no longer cut off past 64 KiB: the
  terminal host leaves input it cannot queue with mr until the harness reads
  it, and writes to the harness as fast as its terminal takes it.
- `serve --mcp` answers a line that is not UTF-8 with a parse error instead
  of exiting, and an unreadable stdin now cancels active runs as EOF does,
  rather than leaving them recorded as running.
- Budget caps now stop graph workflows (stages with `after`) before the next
  stage starts; before, they launched every stage regardless of spend.
- A graph stage or fan-out candidate whose harness ends while the scheduler
  runs another's checks records when it ended, not when the checks did.
- CLI `--notify` webhooks reach their endpoint: `run`, `resume`, and `rerun`
  wait up to 5 s for a webhook still being sent before exiting, instead of
  dropping it.
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

- `serve --mcp` caps JSON-RPC request lines at 2 MiB and concurrent
  `tools/call` workers at 32 (at most 24 blocking `wait_run` calls).
- Project trust: a project's custom adapters (including `follow_up_args` when
  set), checks, extra arguments, loosened permissions or sandbox, unconfirmed
  ship stages, `ship.secret_scanner` commands, `ship.secret_scan = false`
  (forced on while untrusted), and custom provider endpoints or key variables
  need `model-router trust`; `serve` refuses untrusted projects.
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

[Unreleased]: https://github.com/Tyru5/model-router/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/Tyru5/model-router/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/Tyru5/model-router/releases/tag/v0.2.1
[0.2.0]: https://github.com/Tyru5/model-router/releases/tag/v0.2.0
[0.1.0]: https://github.com/Tyru5/model-router/releases/tag/v0.1.0
