# Harnesses and models

A harness is the agent CLI a stage runs. The router launches it with your
existing login and configuration; it stores no credentials. Per-flag
references and fixtures: [adapters.md](adapters.md).

## Built-in harnesses

`model-router harnesses` lists them; `--spec [NAME]` prints the resolved spec
as JSON.

| Name | Program | Selection | Integration | Takeover | Follow-up |
| --- | --- | --- | --- | --- | --- |
| `codex` | `codex` | Model ID; optional provider, effort | `exec` JSON events; final response file | yes | yes |
| `claude` | `claude` | Alias or full ID; optional effort | Print-mode stream JSON | yes | yes |
| `pi` | `pi` | Provider + model ID; optional thinking | Print-mode JSON | yes | yes |
| `omp` | `omp` | Provider + model (`provider/model`); optional thinking | Pi's JSON format; approval mode | yes | no |
| `cursor` | `cursor-agent` | Cursor model slug | Print-mode stream JSON | yes | no |
| `gemini` | `gemini` | Gemini model ID | Headless JSON (at exit) | no | no |
| `opencode` | `opencode` | `provider/model`; optional variant | Run-mode JSON; TUI with `--prompt` (`run --interactive` before 2.x) | yes | yes |
| `aider` | `aider` | Native/LiteLLM model ID | Message file; auto-commits disabled | no | no |
| `goose` | `goose` | Provider + model | `run`; interactive continuation | no | no |
| `amp` | `amp` | Mode, e.g. `high` (may change the model) | Execute-mode JSON | yes | no |
| `copilot` | `copilot` | Copilot model ID | Programmatic prompt | no | no |
| custom | yours | Anything | [Custom adapters](#custom-adapters) | with `resume_args` | with `follow_up_args` |

All built-ins also run interactively in their native terminal. **Follow-up** is
the unattended session command used by `handoff_repair` (`codex` cannot follow
up on its local providers `ollama` and `lmstudio`). `gemini` prints its JSON
document only on exit (`streams = false`), so `stall_timeout_seconds` is
rejected for it.

## Choosing a model

Model IDs are not restricted to a list. In the TUI, `m` discovers models, and
the stage editor's **Model** field picks from the catalog (`i` types any ID).
`model-router models HARNESS` prints the same catalog. Discovery never sends
an inference request and is labelled by source:

- **Codex:** local model cache; may be stale.
- **Pi/omp/OpenCode:** configured-provider catalogs, not entitlements.
- **Cursor:** account-reported list. `agent` is used only when `cursor-agent`
  is missing and `agent` is Cursor's.
- **Router providers:** each configured provider's `/models`.
- **Claude/Amp:** documented aliases/modes plus saved selections.
- **Others:** native default, saved selections, manual entry.

A failed or unparsable listing says why and falls back to manual entry;
listings over 2 MiB are truncated. The router never silently replaces a model
that fails. Effort levels come from local catalogs; `validate` warns about
values a harness does not document.

## Providers

Route a stage through any OpenAI-compatible API: add `[providers.NAME]` and
set the stage's **Provider** to `NAME`. The router wires the endpoint into the
harness's own provider mechanism; the harness still runs the agent.

Built-in presets (`model-router providers`): `openrouter`, `vercel`,
`requesty`, `google`, `xai`, `mistral`, `deepseek`, `moonshot`, `zai`,
`minimax`, `groq`, `cerebras`, `together`, `fireworks`, `deepinfra`,
`huggingface`, `nebius`, `sambanova`, `perplexity`, `ollama`, `lmstudio`.

```toml
[providers.openrouter]                   # opt into the preset

[providers.litellm]
base_url = "http://localhost:4000/v1"
api_key_env = "LITELLM_KEY"              # empty for keyless local servers
context_limit = 128000                   # optional; OpenCode compaction
output_limit = 16384
```

| Harness | How it is applied |
| --- | --- |
| Codex | `--config model_providers.model-router-NAME.{name,base_url,env_key}` + `model_provider`. Needs a Responses API endpoint. |
| OpenCode | `@ai-sdk/openai-compatible` provider `model-router-NAME` in `OPENCODE_CONFIG_CONTENT` (inherited inline config kept). Limits default to 32,768 context, output ¼ of context (max 8,192). |
| Aider | `--model openai/MODEL`, `--openai-api-base`, `OPENAI_API_BASE`/`OPENAI_API_KEY`. Inherited `OPENAI_*`/`AIDER_OPENAI_*` removed. Aider still reads `.env` files. |
| Goose | `--provider openai` with `OPENAI_HOST`/`OPENAI_BASE_PATH`/`OPENAI_API_KEY`; inherited `OPENAI_*` removed. |
| Custom | `{provider}`, `{base_url}`, `{api_key_env}` placeholders (configured entries only). |
| Pi, omp | Own registries only. A provider only `[providers.NAME]` defines is rejected; define it in their `models.json`. |
| Claude Code, Cursor, Gemini, Amp, Copilot | Not supported; own accounts only. |

- Only `[providers.NAME]` entries are injected. Other names, presets
  included, pass through unchanged, so the harness's own definition wins
  (Codex profiles and `--oss`, OpenCode/Aider/Goose/Pi registries and OAuth).
- Codex and OpenCode see injected providers as `model-router-NAME`, so they
  never merge with native entries or inherit their keys.
- Keys stay in your environment: copied at launch, run refused if unset, only
  names recorded in `command.json`, previews show `NAME=<redacted>`.
- `base_url` with credentials, a query, or a fragment is rejected.
- `model-router providers NAME` lists the provider's models via `GET /models`
  (context length and prices when reported).
- Project providers with their own endpoint or key variable need
  [trust](safety.md#project-trust).

## Permission settings

`c` on a stage, or `harness_settings` in TOML. Default **Harness default**
sends no override. Values are not edits to the harness's global config.

| Harness | Key | Values |
| --- | --- | --- |
| Codex | `sandbox` | `read-only`, `workspace-write`, `danger-full-access` |
| Codex | `approval_policy` | `on-request`, `never` (no prompts; does not bypass the sandbox) |
| Claude Code | `permission_mode` | `manual`, `acceptEdits`, `plan`, `dontAsk`, `auto`, `bypassPermissions` |
| Cursor | `sandbox` | `enabled`, `disabled` |
| Gemini | `approval_mode` | `default`, `auto_edit`, `plan`, `yolo` |
| omp | `approval_mode` | `always-ask`, `write`, `yolo` |
| Aider | `confirmations` | `yes-always` (unattended runs need it; shell commands still ask) |
| Goose | `mode` | `approve`, `smart_approve`, `chat`, `auto` (as `GOOSE_MODE`) |
| Copilot | `tool_permissions` | `allow-all-tools` |
| Others | — | Use **Extra arguments** |

```toml
harness_settings = { sandbox = "workspace-write", approval_policy = "on-request" }
```

Settings that conflict with extra arguments are rejected before launch.
Loosening values (`danger-full-access`, `bypassPermissions`, `yolo`, Cursor
`disabled`, `yes-always`, Goose `auto`, `allow-all-tools`) need trust in a
project config. The router never enables a bypass by default.

Unattended stages have no input. Reported permission denials pause the run.
Some harnesses only stall or error on approval needs: cancel or let the stage
time out, then reopen it interactively. Copilot programmatic tool use needs
permissions set explicitly.

## Structured output

Codex (`--output-schema`, 0.41.0+) and Claude Code (`--json-schema`, 2.1.22+)
receive the [handoff schema](../schema/handoff.schema.json) and return a
validated handoff natively. Other harnesses write a handoff file or a
`<router-handoff>` block. Order of sources:
[workflows.md](workflows.md#handoffs).

## Takeover

`t` on the Runs page reopens a stage's session in the native terminal:

| Harness | Session source | Reopened with |
| --- | --- | --- |
| Claude Code | `--session-id` chosen by the router | `claude --resume ID` + model, effort, permissions |
| Codex | `thread.started` event | `codex resume … ID` + model, effort, permissions |
| Pi | `--session-id` chosen by the router | `pi … --session-id ID` + model, provider, thinking |
| omp | `session` event | `omp --resume ID` + model, thinking, approval mode |
| Cursor | `session_id` events | `cursor-agent … --resume ID` + model, permissions |
| OpenCode | `sessionID` events | `opencode --session ID` |
| Amp | `session_id` events | `amp threads continue --mode MODE ID` |

Leaving the harness returns to the checkpoint; the session stays saved.
Cancelling sends SIGTERM and waits five seconds before SIGKILL so harnesses can
save (omp only saves on clean exit). Gemini, Copilot, Goose, Aider, and custom
adapters without `resume_args` cannot reopen after exit; use `i`. Live detached
sessions can always be reattached, including ones left running after `mr`
exited ([tui](tui.md#interactive-sessions)).

## Doctor

`model-router doctor [--json] [--strict]` checks each harness's executable,
version, and the flags the router passes, using only `--version` and
`--help`. It reports the sandbox backend, and whether `git` and `gh` are on
the path with their versions (`git` 2.26+ serves worktrees, snapshots, diffs,
and ship; `gh` serves GitHub task imports and `ship --draft-pr`). It cannot
check logins.
`--strict` fails when an installed harness lacks a flag. The Harnesses page
(`3`) runs the same checks; the stage details then warn about missing flags.
A version counts as `tested` when a captured parser fixture covers it, or
when a live test from the Harnesses page passed in this project
([tui.md](tui.md#live-tests)).
Details: [adapters.md](adapters.md#compatibility-checks).

## Custom adapters

```toml
[adapters.my_cli]
command = "/path/to/agent"
args = ["--print", "{prompt}"]
interactive_args = ["{prompt}"]
models_args = ["models", "--plain"]       # one model ID per line
output = "claude-stream"                  # or codex-jsonl, pi, cursor-stream, gemini-json, opencode, amp-stream, text
session = { assign = "--session-id" }               # or { reported = true }
resume_args = ["--resume", "{session}"]             # enables interactive takeover
follow_up_args = ["--resume", "{session}", "--print"] # enables handoff_repair
model_flag = "--model"
effort_flag = "--effort"
prompt = { flag = "-p" }                  # or "arg", or "stdin"
```

Choose `my_cli` as a stage's harness. Arguments are passed directly; no shell,
and task/model text is never re-expanded. Order: `args`, model flag, effort
flag, extra arguments, prompt. `resume_args` enables interactive takeover only;
`follow_up_args` enables `handoff_repair` (uses the same placeholders as `args`
plus `{session}`, receives the prompt as an unattended launch does, and requires
`session`). All fields:
[configuration.md](configuration.md#adaptersname). A project's adapters need
[trust](safety.md#project-trust). Model-only tools work too, e.g.
`command = "ollama"`, `args = ["run", "{model}", "{prompt}"]`.

Adding a built-in harness: [adapters.md](adapters.md#adding-a-harness).
