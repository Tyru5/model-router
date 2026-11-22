# Configuration

Workflows live in TOML. This page lists every field, its default, and where
files live. For what stages, graphs, and fan-out do, see
[workflows.md](workflows.md).

## Files and precedence

| File | Purpose |
| --- | --- |
| `--config FILE` | Explicit config. Always trusted. Wins over everything below. |
| `<project>/.model-router.toml` | Project config. Replaces global workflows, while global `[providers.*]`, `[adapters.*]`, `[delegates.*]`, and `[pricing.*]` layer underneath (project keys win on collision). Global entries stay trusted; project entries need [trust](safety.md#project-trust). Saving a project config omits unmodified global entries. |
| `<config dir>/config.toml` | Global templates. Always trusted. |
| (none) | Built-in starter workflow. |

`<config dir>` is the platform config directory: `~/.config/model-router` on
Linux (respects `XDG_CONFIG_HOME`), `~/Library/Application Support/model-router`
on macOS. `MODEL_ROUTER_CONFIG_DIR` overrides it.

```sh
model-router init                      # starter .model-router.toml in the project
model-router init --detect             # detect installed harnesses; blank models inherit defaults
model-router init --preset bug-fix     # quick-task | full-feature | bug-fix | investigation | best-of-3
model-router init --global             # global templates; prints the path
model-router validate                  # parse, migrate, warn; runs nothing
```

`init` refuses to overwrite an existing file without `--force`. `init --detect`
checks `PATH` for installed harnesses, leaves stage models and effort blank to
inherit each harness's default, and replaces any stage harness that is not
installed while preserving read-only intent (mapped to the new harness's native
read-only setting—Codex `sandbox = "read-only"`, Claude
`permission_mode = "plan"`, Gemini `approval_mode = "plan"`—or else the
stage's router `sandbox = "read-only"`). When more than one harness is
installed, review stages move to a different harness than the implementing
stage. In the TUI, `s` saves project templates and `S` global templates.

Other files in `<config dir>`:

| File | Contents |
| --- | --- |
| `appearance.json` | Preferences for every project: `theme`, `vim_motions`, `notifications` (`terminal`, `desktop`, `webhook`, `webhook_url`). Written by the TUI. |
| `trust.json` | Per-project SHA-256 digest of trusted config sections. Written by `model-router trust` and the TUI prompt. |
| `pricing.toml` | Your token prices, as `[models."ID"]` tables. See [cost.md](cost.md). |
| `recent-projects.json` | Recent projects for the repository browser. |
| `startup.json` | `version` that last played the startup animation. Delete it to see the animation again. |

Project state lives under `<project>/.model-router/` (see
[workflows.md](workflows.md#artifacts)).

## Versioning and migration

Files carry `version = 2`. A file without `version` is v1. Loading migrates
older files in memory; saving writes v2.

| From | Migration |
| --- | --- |
| v1 → v2 | Appends an enabled **Review summary** stage to workflows without a `summary`-role stage. Uses Codex; if Codex is not installed, the first enabled stage's harness with its default model. Runs once: delete or disable the stage to opt out. |

A file with a newer `version` than this binary fails with a message to
upgrade. Unknown top-level and workflow fields are ignored with a warning
(shown by `validate` and the TUI status line), so newer files still load.
Stage, check, fan-out, and candidate fields are strict: typos fail.
`validate` also warns about effort values a harness does not document.

## Top level

```toml
version = 2
[[workflows]]        # one or more; the first is the default
[adapters.NAME]      # custom harnesses
[providers.NAME]     # OpenAI-compatible endpoints
[pricing."MODEL"]    # token prices
[delegates.NAME]     # targets a delegating stage's agent may hand tasks to
[harness_versions]   # optional harness version pins checked by `doctor`
```

### `[harness_versions]`

Optional map of harness name to version pin, checked by `model-router doctor`
(and enforced by `doctor --strict`). Does not require project trust.

```toml
[harness_versions]
codex = ">=0.160.0"
claude = "2.1"
opencode = "2.2.5"
```

Accepted pin forms: exact 3-part versions (`"0.167.0"`), 1–2 part segment
prefixes (`"0.167"`, `"2"`), comparisons (`>=`, `<=`, `>`, `<`, `=`, `==`), and
comma-separated ranges (`">=0.160.0, <0.200.0"`).

## `[[workflows]]`

| Field | Default | Meaning |
| --- | --- | --- |
| `name` | required | Display and `--workflow` name. |
| `max_repair_passes` | `2` | Automatic review repair passes; `0` disables repair. TUI: `l`. |
| `max_parallel` | `2` | Harness processes a graph workflow or fan-out runs at once. |
| `sandbox_default` | `"off"` | Router sandbox for stages without `sandbox`: `off`, `read-only`, `workspace-write`, `by-role`. |
| `budget` | none | `{ max_usd, max_tokens, on_exceed }` for the whole run. |
| `protected_paths` | `[]` | Repository-relative globs (`*`, `**`, `?`, up to 64). Enforced for ordinary stages, fan-out candidates (candidate stops blocked), and candidate selection; `ship` and `apply` refuse matching files. |
| `handoff_repair` | `false` | When `true`, an unattended stage that exits 0 without a valid handoff sends one follow-up on the same session asking only for handoff JSON (an extra, possibly billed request). Supported on `codex` (not local providers `ollama`/`lmstudio`), `claude`, `pi`, `opencode`, and custom adapters with `follow_up_args`. |
| `handoff_budget_bytes` | `14000` | Total byte budget for prior stage handoffs in downstream prompts (`1024`–`200000`). |
| `handoff_each_bytes` | `5000` | Per-stage byte budget for each prior handoff (`1024`–`200000`, at most `handoff_budget_bytes`). |
| `context_files` | `[]` | Repo-relative paths or globs (`*`, `**`, `?`, up to 32) whose UTF-8 contents are injected into every stage prompt (max 32 files and 32 KiB total, symlink-safe, never reading `.git` or `.model-router`). Shown in `preview` and `handoffs --prompt`. |
| `retry` | `{ max_attempts = 0, base_delay_seconds = 5, max_delay_seconds = 60 }` | Default transient retry policy for unattended stages (`max_attempts` `0`–`5`). Below. |
| `hooks` | `{}` | Workflow lifecycle hooks (`before_stage`, `after_stage`, `before_ship`, `on_failure`). Needs trust in a project config. Below. |
| `policies` | `[]` | Declarative diff policies (`[[workflows.policies]]` with `action = "block" | "advise"`). Below. |
| `review_quorum` | unset | Minimum distinct enabled non-judge review stages that must pass (`1`–`64`). When `> 1`, blocks completion and `ship` until that many reviews pass. |
| `stages` | required | `[[workflows.stages]]` tables, in list order (at most 64). |

## `[[workflows.stages]]`

| Field | Default | Meaning |
| --- | --- | --- |
| `id` | `stage-XXXXXXXX` | Stable ID used by `after`, `repair_target`, `--stage`, `resume --retry-candidate`. |
| `name` | `"New stage"` | Display name. |
| `role` | `"custom"` | `research`, `planning`, `implementation`, `review`, `test`, `summary`, `ship`, `custom`. See [roles](workflows.md#roles). |
| `enabled` | `true` | Disabled stages are skipped. |
| `harness` | `"codex"` | Built-in name or a `[adapters.NAME]` key. |
| `model` | `""` | Native model ID, alias, or (Amp) mode. Empty = harness default. |
| `provider` | `""` | Harness provider or a `[providers.NAME]` key. Empty = native config. |
| `effort` | `""` | Reasoning effort / thinking level / OpenCode variant, where supported. |
| `mode` | `"unattended"` | `unattended` or `interactive`. |
| `instructions` | `""` | Stage prompt. |
| `extra_args` | `[]` | Extra harness arguments. Needs trust in a project config. |
| `harness_settings` | `{}` | Native permission overrides; see [harnesses.md](harnesses.md#permission-settings). |
| `timeout_seconds` | `3600` | Attempt timeout. |
| `stall_timeout_seconds` | unset | Optional unattended inactivity limit in seconds: stops the stage if neither stdout nor stderr produces bytes for this long. Rejected for harnesses that print only at exit (`gemini` and custom adapters with `output = "gemini-json"`, including fan-out candidates); ignored for interactive stages (`preview` notes this). |
| `billing` | unset | `"api"` or `"subscription"`. Precedence: stage `billing`, else `"api"` when routed through a router provider, else the harness default (`codex`, `claude`, `cursor`, `gemini`, `copilot` → `"subscription"`; `pi`, `omp`, `opencode`, `aider`, `goose`, `amp`, and custom adapters → `"api"`). The router does not detect a harness's login method; set `billing` explicitly when it differs from the default (for example, `billing = "subscription"` for `pi` or `omp` on a Claude or ChatGPT plan). See [cost.md](cost.md#billing). |
| `repair_target` | `""` | Review only: implementation stage ID to repair. Empty = nearest enabled upstream. |
| `checks` | `[]` | Router-run checks, below. Needs trust in a project config. |
| `require_router_checks` | `false` | Test stages only: only a passing required router check (or an accepted waiver) satisfies the gate; model-reported passes do not. |
| `sandbox` | unset | Overrides `sandbox_default` (same values). |
| `sandbox_writable` | `[]` | Extra writable paths (absolute or `~/`). |
| `sandbox_network` | `true` | `false` cuts the sandboxed harness off the network. |
| `sandbox_fallback` | `"refuse"` | `refuse` fails the stage if no sandbox backend works; `warn` runs unsandboxed. |
| `budget` | none | `{ max_usd, max_tokens, on_exceed }` across this stage's attempts. |
| `ship` | defaults | Ship options; `ship` role only. Below. |
| `after` | `[]` | Stage IDs this stage waits for. Any `after` makes the workflow a graph. |
| `fan_out` | none | Best-of-N candidates and judge. Below. |
| `delegation` | none | The stage's agent may delegate tasks, launch runs, and schedule work. Below. Needs trust in a project config. |
| `context_files` | `[]` | Extra repo-relative paths or globs injected into this stage's prompt after the workflow's `context_files` (same 32-file / 32 KiB total limit and workspace-only safety checks). |
| `retry` | unset | Overrides workflow `retry` for this stage (`{ max_attempts, base_delay_seconds, max_delay_seconds }`). |
| `hooks` | `{}` | Stage lifecycle hooks (`before_stage`, `after_stage`, `before_ship`, `on_failure`), run after workflow hooks. Needs trust in a project config. |
| `when_changed` | `[]` | Repo-relative globs (`*`, `**`, `?`, up to 32). In a Git repository, skips the stage (`skipped`) when no files changed so far in the run match any pattern; outside Git, runs normally. |

### `checks`

```toml
checks = [
  { command = ["cargo", "test", "--locked"] },
  { command = ["cargo", "clippy", "--", "-D", "warnings"], required = false, timeout_seconds = 300 },
]
```

| Field | Default | Meaning |
| --- | --- | --- |
| `command` | required | Argument vector run in the workspace. No shell. |
| `required` | `true` | A failing required check blocks the stage (or requests a repair in a review). |
| `timeout_seconds` | `600` | Per check. |

### `fan_out`

```toml
fan_out = { judge = "judge", candidates = [
  { harness = "claude", model = "sonnet" },
  { id = "astra", harness = "codex", model = "gpt-6-astra", effort = "high" },
] }
```

| Field | Default | Meaning |
| --- | --- | --- |
| `candidates` | required | 2–8 of `{ id?, harness, model?, provider?, effort? }`. IDs (letters, digits, `-`, `_`) default to `c1`, `c2`, … A candidate on another harness drops the stage's `harness_settings` and `extra_args`. |
| `judge` | `"auto"` | `auto` or the ID of a review stage that runs after this one. |

### `delegation`

```toml
[workflows.stages.delegation]
delegates = ["codex-review"]     # named targets ([delegates.NAME])
harnesses = ["opencode"]         # ad-hoc targets the model may pick
models = ["openrouter/*"]        # globs ad-hoc models must match
max_tasks = 12
```

| Field | Default | Meaning |
| --- | --- | --- |
| `delegates` | `[]` | Named targets this stage may use. |
| `harnesses` | `[]` | Harnesses the model may pick ad hoc. Empty allows named targets only. |
| `models` | `[]` | Globs (`*`, `?`) ad-hoc models must match. Empty: the harness's catalog, or any model for a harness with manual entry. |
| `providers` | `[]` | Router providers ad-hoc targets may use. |
| `max_tasks` | `12` | `delegate_task` calls per attempt (1–64). |
| `max_parallel` | `3` | Task processes at a time (1–16). |
| `max_wake_turns` | `8` | Follow-up turns that hand results to the parent (1–32). |
| `runs` | `false` | Run tools (`run_launch`, …). |
| `max_runs` | `4` | Runs one attempt may launch (0–20). |
| `schedules` | `false` | Schedule tools (`schedule_task`, …). |

The stage's harness must accept the router's MCP server: Claude Code, Codex,
OpenCode, or a custom adapter with `mcp_config_args`. A delegating stage's
`stall_timeout_seconds` must be at least 60, since a tool call waits up to 45 s
without output. Not on fan-out, ship, or summary stages. See
[delegation.md](delegation.md).

### `budget`

```toml
budget = { max_usd = 2.0, max_tokens = 2_000_000, on_exceed = "stop" }
```

`max_usd` and `max_tokens` are optional and must be positive. `on_exceed` is
`"stop"` (default: block before the next stage, or abort an unattended streaming
stage mid-stream as soon as streamed usage exceeds the cap) or `"warn"` (log a
warning and continue). `max_tokens` counts input, output, and cache writes (not
cache reads). See [cost.md](cost.md#budgets).

### `retry`

```toml
retry = { max_attempts = 2, base_delay_seconds = 5, max_delay_seconds = 60 }
```

Retries unattended stage failures matching transient provider or network errors
(HTTP 429/502/503/504/529, rate limits, overloaded, connection reset/timeout)
on the same harness and model with capped exponential backoff (`max_attempts`
`0`–`5`, `base_delay_seconds` `1`–`600`, `max_delay_seconds` from
`base_delay_seconds` to `3600`).
Permanent auth, permission, and invalid-request errors are not retried.

### `hooks`

```toml
[workflows.hooks]
before_stage = [{ command = ["cargo", "check", "--locked"], timeout_seconds = 120 }]
after_stage  = [{ command = ["scripts/post-stage.sh"] }]
before_ship  = [{ command = ["cargo", "test", "--locked"], timeout_seconds = 300 }]
on_failure   = [{ command = ["scripts/notify-fail.sh"] }]
```

Each hook entry has `command` (non-empty argv, no shell) and `timeout_seconds`
(`1`–`3600`, default `120`), run in its own process group with
`MODEL_ROUTER_HOOK`, `MODEL_ROUTER_RUN_ID`, `MODEL_ROUTER_WORKFLOW`,
`MODEL_ROUTER_STAGE_ID`, `MODEL_ROUTER_STAGE_ROLE`, and
`MODEL_ROUTER_STAGE_STATUS`. `before_stage` and `before_ship` abort on non-zero
exit or timeout; `after_stage` and `on_failure` log results under
`<attempt>/hooks/<event>/NN/` without masking the stage outcome. Requires
project trust.

### `policies`

```toml
[[workflows.policies]]
name = "no-dbg"
action = "block"
pattern = "dbg!("
paths = ["src/**/*.rs"]
message = "Remove dbg! macro calls before shipping"

[[workflows.policies]]
name = "migration-review"
action = "advise"
paths = ["migrations/**"]
message = "Verify rollback safety and lock duration"
```

Non-backtracking line matcher (`literal`, `i:<case-insensitive>`, `word:<token>`,
`any:<a|b>`, `glob:<pattern>`). `block` scans added diff lines of matching
`paths` at stage end, fan-out candidate post-processing and selection, `apply`,
and `ship`, failing closed if the diff cannot be read. `advise` injects
`message` into review stage prompts (`POLICY ADVISORIES`) when changed files
(and optional `pattern`) match.

### `ship`

`[workflows.stages.ship]` (or inline `ship = { ... }`). All optional.

| Field | Default | Meaning |
| --- | --- | --- |
| `branch` | `"mr/{run_id}-{slug}"` | New branch. `{run_id}`, `{slug}` (task's first line) expand. |
| `commit_message` | `""` | Empty = summary's first line + summary. Template placeholders: `{subject}`, `{summary}`, `{task}`, `{run_id}`, `{slug}`, `{branch}`. |
| `push` | `false` | `git push --set-upstream REMOTE BRANCH`. Never forced. |
| `remote` | `"origin"` | Push remote. |
| `draft_pr` | `false` | `gh pr create --draft`. Requires `push`. |
| `base` | `""` | PR base. Empty = checkout's upstream branch, else `gh`'s default branch. |
| `confirm` | `true` | `false` skips the preview. Needs trust in a project config. |
| `include_untracked` | `true` | Commit files the run created. |
| `include_preexisting` | `false` | Commit files you also changed before the run; otherwise they block the ship. |
| `secret_scan` | `true` | Built-in scan of the full diff of the planned files only (no size cutoff; files Git treats as binary are read as text) for high-confidence secrets at plan time and right before commit; lists at most 20 hits then `and N more` (never prints secret values). Setting `false` needs trust in a project config. |
| `secret_scanner` | `[]` | External scanner argv (no shell), e.g. `["gitleaks", "protect", "--staged"]`. Runs at plan time and right before commit with `cwd` = workspace, `{files}` and `MODEL_ROUTER_SHIP_FILES` = planned paths, and `GIT_INDEX_FILE` = a private index containing `HEAD` plus the planned files as they are now, so `--staged` scanners see exactly what will be committed without reading or writing your index. Planning writes blob objects to the object database. Needs trust; fails closed if missing or non-zero. |

## `[adapters.NAME]`

Custom harnesses. A project's adapters need trust.

| Field | Default | Meaning |
| --- | --- | --- |
| `command` | required | Executable. |
| `args` | `[]` | Unattended arguments. |
| `interactive_args` | none | Interactive arguments. Omit if the tool has no interactive mode. |
| `models_args` | none | Arguments that print one model ID per line. |
| `prompt_stdin` | `false` | Send the unattended prompt on stdin. |
| `output` | `"text"` | Parser: `text`, `codex-jsonl`, `claude-stream`, `pi`, `cursor-stream`, `gemini-json`, `opencode`, `amp-stream`. |
| `session` | none | `{ assign = "--session-id" }` (router picks an ID) or `{ reported = true }` (read from output events). |
| `resume_args` | none | Interactive arguments that reopen `{session}`. Enables interactive takeover only (not `handoff_repair`); needs `session`. |
| `follow_up_args` | none | Unattended arguments that follow up on `{session}`. Enables `handoff_repair`; accepts the same placeholders as `args` plus `{session}`, receives the prompt as an unattended launch does, needs `session` (`assign` or `reported`), and enters the project trust digest only when set. |
| `model_flag` | none | Appends `FLAG model` after `args` when the stage sets a model. |
| `effort_flag` | none | Appends `FLAG effort`. |
| `prompt` | none | `"stdin"`, `"arg"`, or `{ flag = "-p" }`. |
| `mcp_config_args` | none | Arguments that hand a delegating stage's harness the router's MCP server; `{mcp_config}` is a Claude-format config file. Appended unless the template uses `{mcp_config}` itself. Without it the adapter cannot delegate. Enters the trust digest. |

Argument order: `args`, model flag, effort flag, extra arguments, prompt.
Placeholders: `{prompt}`, `{prompt_file}`, `{handoff_file}`, `{model}`,
`{provider}`, `{effort}`, `{workspace}`, `{run_dir}`, `{session}`, plus
`{base_url}` and `{api_key_env}` for a configured `[providers.NAME]`. No shell
evaluates them. See [harnesses.md](harnesses.md#custom-adapters).

## `[delegates.NAME]`

A named target for stages' `delegation` (global `[delegates.*]` layer
underneath project entries; project keys win on collision). Needs trust in a
project config.

```toml
[delegates.codex-review]
harness = "codex"
model = "gpt-6.2-sol"
effort = "high"
harness_settings = { sandbox = "read-only" }
isolation = "shared"
```

| Field | Default | Meaning |
| --- | --- | --- |
| `harness` | required | Built-in name or an `[adapters.NAME]` key. |
| `model`, `provider`, `effort` | `""` | As for a stage. |
| `harness_settings` | `{}` | Native settings; may not exceed a stage that names it. |
| `timeout_seconds`, `stall_timeout_seconds` | the stage's | Per task turn. |
| `isolation` | by role | `shared` (read-only in the parent's checkout) or `worktree`. |
| `billing` | unset | As for a stage. |

No `extra_args`, `sandbox*`, `mode`, `checks`, or `budget`: tasks run
unattended, in their own OS sandbox.

## `[providers.NAME]`

| Field | Default | Meaning |
| --- | --- | --- |
| `base_url` | preset's | OpenAI-compatible API root. Required unless `NAME` is a built-in preset. No credentials, query, or fragment. |
| `api_key_env` | preset's | Environment variable holding the key. Empty for keyless local servers. |
| `context_limit` | `32768` | Context tokens (OpenCode compaction). |
| `output_limit` | context/4, max `8192` | Output tokens. Must be below context. |

An empty `[providers.openrouter]` opts into the preset. A project provider
with its own `base_url`, or a key variable other than the preset's, needs
trust. See [harnesses.md](harnesses.md#providers).

## `[pricing."MODEL"]`

USD per 1M tokens. `input` and `output` required; `cache_read` defaults to
0.1 × input, `cache_write` to 1.25 × input. See [cost.md](cost.md).

## Full example

```toml
version = 2

[providers.openrouter]

[pricing."my-model"]
input = 1.5
output = 6.0

[[workflows]]
name = "Feature development"
max_repair_passes = 2
max_parallel = 2
sandbox_default = "by-role"
budget = { max_usd = 5.0 }

[[workflows.stages]]
id = "research"
name = "Investigate"
role = "research"
harness = "codex"
model = "gpt-6-astra"
instructions = "Investigate the task. Do not change code."

[[workflows.stages]]
id = "implement"
name = "Implement"
role = "implementation"
harness = "claude"
model = "sonnet"
mode = "interactive"
harness_settings = { permission_mode = "acceptEdits" }

[[workflows.stages]]
id = "verify"
role = "test"
harness = "codex"
checks = [{ command = ["cargo", "test", "--locked"] }]

[[workflows.stages]]
id = "summary"
name = "Review summary"
role = "summary"
harness = "codex"
```

## Environment variables

| Variable | Effect |
| --- | --- |
| `MODEL_ROUTER_CONFIG_DIR` | Overrides `<config dir>`. |
| `MODEL_ROUTER_NO_ANIMATION=1` | Skip the TUI startup animation (as `--no-animation`). |
| `MODEL_ROUTER_NOTIFY=1` | CLI `run`/`resume`/`rerun` notify (as `--notify`). |
| `MODEL_ROUTER_NOTIFY_URL` | Webhook URL; turns the webhook backend on. |
| `MODEL_ROUTER_SERVE_TOKEN` | Fixed bearer token for `serve --http`. |
| `MODEL_ROUTER_METRICS_FILE` | Path for append-only JSONL structured operational events (`run_start`, `run_end`, `attempt_start`, `attempt_end`; same as `--metrics-file`). Contains no task text, prompts, or secrets. |
| `MODEL_ROUTER_HOOK` | Set by the router on lifecycle hooks (`before_stage`, `after_stage`, `before_ship`, `on_failure`), alongside `MODEL_ROUTER_RUN_ID`, `MODEL_ROUTER_WORKFLOW`, `MODEL_ROUTER_STAGE_ID`, `MODEL_ROUTER_STAGE_ROLE`, and `MODEL_ROUTER_STAGE_STATUS`. |
| `MODEL_ROUTER_HARNESS_VERSIONS` | Override detected versions, e.g. `opencode=1.4.0`. |
| `MODEL_ROUTER_FRAME_MARKER=1`, `MODEL_ROUTER_FRAME_FILE` | Test hook: smoke tests wait on frames written to this file. |
| `MODEL_ROUTER_GROUP` | Set by the router on every harness and check it launches: a random marker that crash recovery uses to recognise the process group. Not read from your environment. |
| `MODEL_ROUTER_ORCH_SOCK`, `MODEL_ROUTER_ORCH_TOKEN_FILE` | Set by the router on a delegating turn's harness: its broker socket and token file, which `model-router __mcp-bridge` reads. Removed from every other launch. |
| `MODEL_ROUTER_BRIDGE_PROGRAM` | Test hook: the program delegating harnesses start as the bridge (default: this `model-router`). |
| `MODEL_ROUTER_SHIP_FILES` | Set by the router on `ship.secret_scanner`: newline-separated repository-relative paths of the planned ship files. Not read from your environment. |
| `NO_COLOR` | Disables TUI colours (crossterm). Harnesses are always launched with `NO_COLOR=1`. |
| `TERM=dumb` | Skips the startup animation. |
| `PAGER`, `VISUAL`, `EDITOR` | Log/diff pager (default `less -R`) and editor. Split like shell words; no shell. |
| `GH_HOST`, `GH_CONFIG_DIR` | GitHub Enterprise host / `gh` config for task imports. |
| Provider key variables | Named by `api_key_env`; copied at launch, never stored. |
