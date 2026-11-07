# Configuration

Workflows live in TOML. This page lists every field, its default, and where
files live. For what stages, graphs, and fan-out do, see
[workflows.md](workflows.md).

## Files and precedence

| File | Purpose |
| --- | --- |
| `--config FILE` | Explicit config. Always trusted. Wins over everything below. |
| `<project>/.model-router.toml` | Project config. Replaces the global template set. Some sections need [trust](safety.md#project-trust). |
| `<config dir>/config.toml` | Global templates. Always trusted. |
| (none) | Built-in starter workflow. |

`<config dir>` is the platform config directory: `~/.config/model-router` on
Linux (respects `XDG_CONFIG_HOME`), `~/Library/Application Support/model-router`
on macOS. `MODEL_ROUTER_CONFIG_DIR` overrides it.

```sh
model-router init                      # starter .model-router.toml in the project
model-router init --preset bug-fix     # quick-task | full-feature | bug-fix | investigation | best-of-3
model-router init --global             # global templates; prints the path
model-router validate                  # parse, migrate, warn; runs nothing
```

`init` refuses to overwrite an existing file without `--force`. In the TUI,
`s` saves project templates and `S` global templates.

Other files in `<config dir>`:

| File | Contents |
| --- | --- |
| `appearance.json` | Preferences for every project: `theme`, `vim_motions`, `notifications` (`terminal`, `desktop`, `webhook`, `webhook_url`). Written by the TUI. |
| `trust.json` | Per-project SHA-256 digest of trusted config sections. Written by `model-router trust` and the TUI prompt. |
| `pricing.toml` | Your token prices, as `[models."ID"]` tables. See [cost.md](cost.md). |
| `recent-projects.json` | Recent projects for the repository browser. |

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
```

## `[[workflows]]`

| Field | Default | Meaning |
| --- | --- | --- |
| `name` | required | Display and `--workflow` name. |
| `max_repair_passes` | `2` | Automatic review repair passes; `0` disables repair. TUI: `l`. |
| `max_parallel` | `2` | Harness processes a graph workflow or fan-out runs at once. |
| `sandbox_default` | `"off"` | Router sandbox for stages without `sandbox`: `off`, `read-only`, `workspace-write`, `by-role`. |
| `budget` | none | `{ max_usd, max_tokens }` for the whole run. |
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
| `repair_target` | `""` | Review only: implementation stage ID to repair. Empty = nearest enabled upstream. |
| `checks` | `[]` | Router-run checks, below. Needs trust in a project config. |
| `require_router_checks` | `false` | Test stages only: only a passing required router check (or an accepted waiver) satisfies the gate; model-reported passes do not. |
| `sandbox` | unset | Overrides `sandbox_default` (same values). |
| `sandbox_writable` | `[]` | Extra writable paths (absolute or `~/`). |
| `sandbox_network` | `true` | `false` cuts the sandboxed harness off the network. |
| `sandbox_fallback` | `"refuse"` | `refuse` fails the stage if no sandbox backend works; `warn` runs unsandboxed. |
| `budget` | none | `{ max_usd, max_tokens }` across this stage's attempts. |
| `ship` | defaults | Ship options; `ship` role only. Below. |
| `after` | `[]` | Stage IDs this stage waits for. Any `after` makes the workflow a graph. |
| `fan_out` | none | Best-of-N candidates and judge. Below. |

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

### `budget`

```toml
budget = { max_usd = 2.0, max_tokens = 2_000_000 }
```

Both optional, both must be positive. `max_tokens` counts input, output, and
cache writes (not cache reads). See [cost.md](cost.md#budgets).

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
| `resume_args` | none | Interactive arguments that reopen `{session}`. Enables takeover; needs `session`. |
| `model_flag` | none | Appends `FLAG model` after `args` when the stage sets a model. |
| `effort_flag` | none | Appends `FLAG effort`. |
| `prompt` | none | `"stdin"`, `"arg"`, or `{ flag = "-p" }`. |

Argument order: `args`, model flag, effort flag, extra arguments, prompt.
Placeholders: `{prompt}`, `{prompt_file}`, `{handoff_file}`, `{model}`,
`{provider}`, `{effort}`, `{workspace}`, `{run_dir}`, `{session}`, plus
`{base_url}` and `{api_key_env}` for a configured `[providers.NAME]`. No shell
evaluates them. See [harnesses.md](harnesses.md#custom-adapters).

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
| `MODEL_ROUTER_HARNESS_VERSIONS` | Override detected versions, e.g. `opencode=1.4.0`. |
| `MODEL_ROUTER_FRAME_MARKER=1`, `MODEL_ROUTER_FRAME_FILE` | Test hook: smoke tests wait on frames written to this file. |
| `NO_COLOR` | Disables TUI colours (crossterm). Harnesses are always launched with `NO_COLOR=1`. |
| `TERM=dumb` | Skips the startup animation. |
| `PAGER`, `VISUAL`, `EDITOR` | Log/diff pager (default `less -R`) and editor. Split like shell words; no shell. |
| `GH_HOST`, `GH_CONFIG_DIR` | GitHub Enterprise host / `gh` config for task imports. |
| Provider key variables | Named by `api_key_env`; copied at launch, never stored. |
