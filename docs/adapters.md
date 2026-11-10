# Adapter references

Checked during implementation on 2026-10-03. Native model names and provider
access remain the responsibility of each harness; the router has no universal
model entitlement database.

| Adapter | Reference | Integration detail |
| --- | --- | --- |
| Codex | Installed `codex exec --help` and model-cache metadata | `--model`, `--config model_reasoning_effort`, JSON events, `--output-last-message`, `--output-schema` (handoff schema, 0.41.0+); interactive prompt is positional. Takeover runs `codex resume THREAD_ID`. |
| Claude Code | Installed `claude --help`; [model configuration](https://code.claude.com/docs/en/model-config) | `--print --output-format stream-json --verbose`, `--json-schema` (handoff schema, 2.1.22+), stdin prompt, `--model`, `--effort`, `--session-id` (unattended and interactive) for takeover with `--resume`; explicit permission denials are handled. |
| Pi | Installed `pi --help`; [CLI source/documentation](https://github.com/badlogic/pi-mono/tree/main/packages/coding-agent) | `--print --mode json`, `@prompt-file`, `--provider`, `--model`, `--thinking`. Provider catalogs include extension-defined models. A provider defined only in the router's `[providers]` is rejected before launch. Takeover: router-chosen `--session-id`. |
| omp (oh-my-pi) | Installed `omp --help` and `omp models --json` | `--print --mode json` (Pi's event format), `@prompt-file`, `--model provider/model`, `--thinking`, `--approval-mode`. Catalog from `omp models --json`, with per-model thinking levels. Takeover: `omp --resume ID` from its `session` event. |
| Cursor Agent | Installed `cursor-agent --help` (2026.10.22); [CLI documentation](https://cursor.com/docs/cli/overview) | Print-mode stream JSON and account model listing. Uses `cursor-agent`; `agent` only when it links into Cursor's `cursor-agent` directory or its `--help` names the Cursor Agent, since other tools use that name. Unattended uses `--output-format stream-json`, whose retries emit `{type: retry}` events; takeover `--resume ID`. |
| Gemini CLI | [Headless reference](https://geminicli.com/docs/cli/headless/) and [CLI reference](https://geminicli.com/docs/cli/cli-reference/) | `--prompt`, JSON `response`, native `--model`; interactive uses `--prompt-interactive`. |
| OpenCode | Installed `opencode run --help` and `opencode --help` (2.2.5, 2026-11-05); [CLI reference](https://opencode.ai/docs/cli/) | 2.x: `run --format json --model provider/model#variant`; interactive is the root TUI with `--prompt`, its model set through `OPENCODE_CONFIG_CONTENT` (`model`), and no variant option, so interactive effort is rejected. Before 2.x (detected `--version` major below 2): `run --interactive`, `--model`, `--variant`. `MODEL_ROUTER_HARNESS_VERSIONS=opencode=1.4.0` overrides detection. Takeover: `opencode --session ID`. |
| Aider | [Scripting](https://aider.chat/docs/scripting.html) and [options](https://aider.chat/docs/config/options.html) | `--message-file` unattended; interactive `--read` plus `--load` command file, written at launch (never by previews). Disable auto/dirty commits. `--yes-always` is the Confirmations setting. Aider has no flag to skip its `.env` files (home, Git root, working directory); routed providers pass `--openai-api-base` so a `.env` cannot redirect the endpoint. |
| Goose | [CLI command reference](https://github.com/aaif-goose/goose/blob/main/documentation/docs/guides/goose-cli-commands.md) | `run --text`, `--provider`, `--model`; `--interactive` continues the conversation. Permission mode via `GOOSE_MODE` (`run` has no flag). |
| Amp | Installed `amp --help`; [modes](https://ampcode.com/docs/models-and-subagents) | `--execute --stream-json` reads stdin. `--mode` selects a mode, which can change its underlying model. Takeover: `amp threads continue ID`. |
| Copilot CLI | [Command reference](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-command-reference) | `--prompt` or `--interactive`, plus `--model`. The Tool permissions setting passes `--allow-all-tools`; finer `--allow-tool`/`--deny-tool` rules go in Extra arguments. |

The router uses native authentication and forwards explicit user arguments.
It never silently enables `--yolo`, `--force`, sandbox bypass, or permission bypass.
The per-stage harness settings dialog (`c` on Workflow) can send explicit
permission overrides. Codex sandbox and `approval_policy` config overrides,
Claude `--permission-mode`, and Cursor `--sandbox` were checked against installed
CLI help on 2026-10-04; Claude's choices again on 2026-11-05 (2.1.318:
`acceptEdits`, `auto`, `bypassPermissions`, `manual`, `dontAsk`, `plan`; older
releases called `manual` `default`). Aider `--yes-always`, Goose `GOOSE_MODE`,
and Copilot `--allow-all-tools` follow their documentation (not installed here). Gemini's `--approval-mode` choices were checked against
its [CLI reference](https://geminicli.com/docs/cli/cli-reference/). Extra arguments
remain available for other native settings; conflicting permission overrides
are rejected before execution. No override is emitted for Harness default.
Unknown effort/provider combinations are rejected rather than ignored; an
effort value outside a harness's documented levels (Claude, Codex, Pi, omp) is
a `validate` warning, since newer versions may add levels. Custom
adapters can integrate other CLI versions or tools without changing Rust code.

Discovery limitations are intentional and visible. Codex currently reads its
local cache; Claude's native SDK model discovery is not implemented. The other
non-enumerating adapters permit manual IDs and configured defaults. CLI catalog
parsers fail back to manual entry instead of making paid access-test requests:
a listing that fails or does not parse keeps the default and saved selections
and names the failure in the catalog note. Only a listing's last 2 MiB is read;
the partial first line is dropped and the note says it was truncated. Harnesses
that can use a router provider (Codex, OpenCode, Aider, Goose, custom) also
list each configured provider's `/models`, keeping context length and prices
(`providers::ModelInfo`).

## Adding a harness

Every built-in harness is one `HarnessSpec` entry in `src/harness/builtins.rs`.
The builder (`adapters::build`/`resume`), settings editor, catalog, doctor,
provider routing, and sandbox state paths all read it. An entry declares:

| Field | Meaning |
| --- | --- |
| `programs`, `identity` | Executable candidates; a shared name (Cursor's `agent`) is accepted only when its path or `--help` identifies it. |
| `args` | Per-mode templates of `Part`s, in order: `Args(&[..])` literals (`{final_file}` expands), `Schema`, `Model`, `Provider`, `Effort`, `Extra` (permission settings, then extra arguments), `Prompt`, `ModelEnv`. |
| `prompt` | Per mode: `Stdin`, `StdinArg("-")`, `Arg`, `Flag("-p")`, `AtFile`, `MessageFile(flag)`, `ReadAndLoad` (Aider's command file). |
| `model`, `effort`, `efforts` | `Flag`/`Qualified` (provider/model)/`Mode`; effort `Flag`, `ConfigKv`, `ModelSuffix('#')`, or `None`, plus documented levels. |
| `provider`, `routing` | How a native provider name is passed, and how a `[providers.NAME]` entry is injected: `OwnAccount`, `Registry`, `OpenAiEnv { .. }`, or the custom `CodexConfig`/`OpencodeJson` strategies. |
| `output` | The parser (`OutputKind`). |
| `session`, `resume` | `Assign { flag, .. }`, `Reported`, or `None`; resume prepends/appends/replaces with `{session}`. |
| `structured` | Native schema flag, first supporting version, inline or file. |
| `settings` | Permission/sandbox settings for the stage editor. |
| `catalog`, `version_override`, `state_dirs`, `notes` | Model listing source, older-version arguments, sandbox-writable state paths, and notes. |

So adding a harness is:

1. One table entry. `model-router harnesses --spec NAME` shows the result.
2. Parser fixtures under `tests/fixtures/harness-output/` (see below).
3. Its cases in `tests/adapter_golden.rs` (`HARNESSES`, `SETTINGS`), then
   `MODEL_ROUTER_UPDATE_GOLDEN=1 cargo test --test adapter_golden` to record
   `tests/fixtures/adapter-golden/NAME.json`. Review the JSON: it is the exact
   argv, environment, stdin, files, and parser for every mode and option.

Doctor needs no list: it probes the flags of sample invocations built from the
spec, and a unit test checks every flag the spec declares is probed. Only a new
mechanism (not a new harness) needs Rust code: a new `Part`, `Prompt`, or
`Routing` variant. Tools that fit the custom adapter fields need no code at all.

## Native structured output

The handoff's primary channel, where the harness has one; the prompt's
`<router-handoff>` contract is the fallback everywhere.

| Harness | Flag | Router behaviour |
| --- | --- | --- |
| Codex | `codex exec --output-schema FILE` (since 0.41.0, [openai/codex#4079](https://github.com/openai/codex/pull/4079); checked with 0.167.0 on 2026-11-05) | Writes `handoff::strict_schema()` to `output-schema.json` in the attempt directory at launch. Codex structured output is strict: every property is required, so optional scalars are nullable and lists may be empty. The final message (`--output-last-message`) is the handoff. |
| Claude Code | `--print --json-schema JSON` (inline JSON only; a path is rejected; print-mode fix in 2.1.22; checked with 2.1.318) | Passes the published schema without annotations. The stream's `result` event carries `structured_output`, read as the handoff; `result` holds the same JSON as text. |
| Gemini CLI, Cursor, OpenCode, Pi, omp, Amp | None in installed help (2026-11-05) or the Gemini [headless](https://geminicli.com/docs/cli/headless/) and [CLI](https://geminicli.com/docs/cli/cli-reference/) references | Marker fallback. |
| Aider, Goose, Copilot, custom | Not documented | Marker fallback. |

The flag is added only to unattended launches whose detected version
(`--version`, or `MODEL_ROUTER_HARNESS_VERSIONS=codex=0.40.0,claude=2.1.21`)
is at least the version above; unknown versions count as current. A user's own
`--output-schema` or `--json-schema` in Extra arguments is kept and the
router's is skipped. `doctor` checks the flag in help like any other. When a
native response is missing or invalid, the router falls back to the handoff
file and then the response text; the attempt records which channel it used
(`handoff_source`: `native`, `file`, `marker`, `fence`, `json`, or
`unstructured`).

## Provider routing references

Router providers (`src/providers.rs`) follow the shape other tools use for
user-defined OpenAI-compatible endpoints: a name, a base URL, and the
environment variable holding the key. Only `[providers.NAME]` entries,
including empty opt-ins to a preset, are emitted, for every supported harness
except Pi. Other names pass to the harness unchanged, because the router cannot
reliably resolve its effective native configuration (Codex system, user, and
profile layers; native registries). Claude Code is not routed: it reads
credentials from sources the router cannot isolate. Codex and OpenCode receive injected providers under an
isolated `model-router-NAME` id; OpenCode also gets model limits so
compaction works. Neither reads inherited `OPENAI_*` variables for that id
(Codex applies `OPENAI_ORGANIZATION`/`OPENAI_PROJECT` headers only to its
built-in `openai` provider). Aider (LiteLLM) and Goose do read them, so routed
launches remove inherited `OPENAI_*` (and Aider's `AIDER_OPENAI_*`) variables
other than the ones the router sets. Pi and omp have no runtime injection; a
provider only the router defines is rejected for them.

A `base_url` may not contain credentials, a query, or a fragment: it reaches
Codex `--config` arguments, saved `command.json`, and previews. Previews redact
every environment value. A project config's provider with its own endpoint,
or a key variable other than the preset's, needs project trust.

| Tool | Mechanism the router mirrors or emits |
| --- | --- |
| Codex | [`model_providers.<id>`](https://github.com/openai/codex/blob/main/docs/config.md#model_providers) with `base_url` and `env_key`, set via `--config`. |
| OpenCode | [Custom providers](https://opencode.ai/docs/providers/#custom-provider) with `@ai-sdk/openai-compatible`, `options.baseURL`, `{env:VAR}`; passed inline via `OPENCODE_CONFIG_CONTENT`. |
| Crush | [`providers`](https://github.com/charmbracelet/crush#custom-providers) with `type: openai`, `base_url`, `api_key: $VAR`. Same schema shape as `[providers.NAME]`. |
| Goose | [OpenAI-compatible provider](https://github.com/aaif-goose/goose/blob/main/documentation/docs/getting-started/providers.md) via `OPENAI_HOST` and `OPENAI_BASE_PATH`. |
| Aider | [OpenAI-compatible APIs](https://aider.chat/docs/llms/openai-compat.html): `OPENAI_API_BASE`, `openai/` model prefix. |

Built-in presets' URLs and key variable names come from each provider's
OpenAI-compatibility documentation (endpoints probed without credentials on 2026-10-04; no inference requests). Bedrock and
Vertex use cloud credentials rather than API keys; use their native harness
settings or an OpenAI-compatible gateway such as LiteLLM.

## Compatibility checks

`model-router doctor` checks each harness without sending a model request:

```text
HARNESS   STATUS     VERSION  TESTED          CAPABILITIES  PATH
codex     installed  0.160.0  schema-derived  ok            ~/.local/bin/codex
omp       installed  18.6.3   tested          ok            ~/.local/bin/omp
gemini    missing    -        -               not checked   gemini

Sandbox: bwrap available at /usr/bin/bwrap (bubblewrap 0.9.0)
Tools: git 2.43.0 at /usr/bin/git; gh missing (needed for GitHub task imports, ship --draft-pr)

Authentication is not checked: that needs a model request. Log in through each harness.
```

- **Version** is the first line of `<program> --version`.
- **Capabilities** come from the adapter code. Doctor builds sample unattended
  and interactive invocations with `adapters::build`, using a model plus every
  provider, effort, and harness setting the adapter accepts. Each `--flag` in
  them is looked up in the harness's help: `codex exec --help`,
  `opencode run --help`, or `goose run --help` when the arguments start with a
  subcommand, otherwise `<program> --help`. Only option lines count, by short
  or long alias; a flag mentioned in prose does not. A flag is `supported`,
  `missing`, or `unknown` (help unavailable). Missing means current help lacks
  it: the harness removed or hid it.
- **Tested** means the detected version has captured parser fixtures (below),
  or a live test of that version passed in this project
  ([tui.md](tui.md#live-tests)).
  A version whose fixtures are all schema-derived shows `schema-derived`
  instead; others show `untested`. JSON reports carry `version_tested` (any
  fixture) and `version_captured` (a captured fixture, or a live pass in this
  project). Doctor's detected version is reused by
  adapters that change flags by version (OpenCode).
- Custom adapters show `not declared`: they have no flag contract, so doctor
  does not run them.
- Authentication is never checked.

`doctor --json` prints the full reports, including each flag and the help
command checked. `doctor --strict` exits non-zero when an installed harness
lacks a flag. Probes run in parallel and finish within about 10 seconds.
The TUI's Harnesses page runs `model_router::doctor::inspect_all` in the
background when opened or on `r`, shows the same columns, and lists each flag
on Enter.

### Parser fixtures

`tests/fixtures/harness-output/manifest.toml` lists every fixture with its
harness, output kind, output format, harness version, scenario, provenance,
source, and sanitization notes. The format must match what the adapter passes
(`--output-format`, `--format`, `--json`, `--stream-json`, `--mode`), or a
`format_note` explains the difference (Claude's older one-shot `json` fixtures,
Cursor `json`); each required scenario needs a fixture in the adapter's
format. `captured` fixtures are sanitized real output.
`schema-derived` fixtures are built from event shapes in installed sources,
schemas, or official documentation. Captured fixtures: the omp set,
`codex/structured-output.jsonl` (strict handoff schema, 2026-11-05), and one
`captured-success` each for Claude, Pi, omp, Cursor, OpenCode, and Amp at
their 2026-11-07 versions. Doctor only says `tested` for an exact version
match, so re-capture after a harness update. `tests/harness_output.rs` runs `handoff::normalize`, and
`handoff::parse` for handoffs, on each fixture and checks the declared text,
errors, and permission denials; `native = true` fixtures were launched with the
output schema and must yield a handoff from it. Scenarios cover success, reported errors,
retries (Codex, Pi, Claude `api_retry`, Cursor `retry`, OpenCode, and an
assumed Amp shape), permission denials (Claude), and tool output or echoed
prompts that contain fake handoffs. An error event followed by a successful
final event (Claude/Cursor/Amp `result` without `is_error`, Gemini `response`,
OpenCode `step_finish` with reason `stop`) is a retry, not a failure. OpenCode's
handoff is the text of its last message only, not every step.

To record a real fixture (this sends one model request, which may be billed):

```sh
cargo build
python3 scripts/capture_harness_output.py claude --confirm-paid-request --model sonnet
cargo test --test harness_output
```

The script runs one unattended stage in a temporary project, with restrictive
settings where available: Codex read-only sandbox, Claude plan mode, Cursor
sandbox, and Gemini plan approval. The task asks for a fixed handoff block. It
sanitizes the attempt's `stdout.log` (prompt echoes, paths, user and host
names, emails, IDs, and keys) into `<harness>/captured-success.<ext>` and
appends a `captured` manifest entry with the adapter's format. Review both before committing.
In the TUI, `t` on Harnesses runs the same script for the marked harnesses,
or the selected one ([tui.md](tui.md#live-tests)).
