# Headless server: `model-router serve`

`serve` lets other agents and clients drive model-router without the TUI.
It speaks the Model Context Protocol over stdio (`--mcp`) and/or a small JSON
HTTP API on loopback (`--http`). Both expose the same tools and use the same
engine, run records, and history as the TUI and CLI.

```sh
model-router serve --mcp -C /path/to/project
model-router serve --http 127.0.0.1:8737 -C /path/to/project
model-router serve --mcp --http 127.0.0.1:0 --allow-root ~/src
```

Runs started by the server are **unattended only**. A workflow with an
enabled interactive stage is refused unless the call passes
`force_unattended: true`, which runs those stages unattended for that run.
The saved workflow never changes.

## Register it with an agent

Claude Code:

```sh
claude mcp add model-router -- model-router serve --mcp -C /path/to/project
```

Codex (`~/.codex/config.toml`):

```toml
[mcp_servers.model-router]
command = "model-router"
args = ["serve", "--mcp", "-C", "/path/to/project"]
# wait_run blocks up to its timeout_seconds; keep this above the waits you ask for.
tool_timeout_sec = 600
```

Clients time tool calls out (Codex defaults to 60 s). Pass a `timeout_seconds`
to `wait_run` below that limit and call it again while `timed_out` is true.

`serve --client-config CLIENT` prints the registration for the exact command
it was given and exits without editing any file. The program is the path
`model-router` was started by (looked up on `PATH` when bare) when that is
the running file, so a package manager's `bin/` symlink stays valid across
upgrades; otherwise the resolved executable. `-C`, `--config`, and
`--allow-root` are written canonical; the metrics file (`--metrics-file` or
`MODEL_ROUTER_METRICS_FILE`) is written as an absolute `--metrics-file`.
Paths that are not UTF-8 are refused.

| `CLIENT` | Output |
| --- | --- |
| `claude` | `claude mcp add model-router -- …` |
| `codex` | `[mcp_servers.model-router]` TOML with `tool_timeout_sec = 600` |
| `cursor`, `json` | `{"mcpServers": {"model-router": {"command", "args"}}}` |
| `gemini` | `mcpServers` JSON with `timeout: 600000` (ms) |
| `opencode` | `opencode.json` with `mcp.model-router` (`type: "local"`) |
| `vscode` | `.vscode/mcp.json` with `servers.model-router` (`type: "stdio"`) |
| `amp` | `amp.mcpServers` for Amp's `settings.json` |
| `copilot` | `mcpServers` JSON for `~/.copilot/mcp-config.json`, with `tools: ["*"]` (required) and `timeout: 600000` |
| `pi` | `mcpServers` JSON for `~/.pi/agent/mcp.json` or `.pi/mcp.json` |
| `omp` | `mcpServers` JSON for `~/.omp/agent/mcp.json` or `.omp/mcp.json` |
| `goose` | `extensions.model-router` YAML for `~/.config/goose/config.yaml` |

```sh
model-router -C /path/to/project serve --client-config codex >> ~/.codex/config.toml
```

Clients that only speak remote MCP can use `POST /mcp` on `serve --http`
(see [HTTP API](#http-api)).

## Client compatibility

Checked on Linux on 2026-11-28 against a mock-harness project, with each
client's own MCP client and a config from `--client-config`. No model was
called: each check lists tools or opens a session without a prompt.

| Client | Version | Check | Result |
| --- | --- | --- | --- |
| Claude Code | 2.1.332 | `claude mcp list` | Connected; negotiates `2025-11-25` |
| Codex | 0.172.0 | `codex app-server` → `mcpServerStatus/list` | 17 tools; negotiates `2025-06-18` |
| Gemini CLI | 0.71.1 | `gemini mcp list` | Connected once the folder is trusted |
| OpenCode | 1.19.6 | `opencode mcp list` | Connected |
| Cursor Agent | 2026.11.22 | `cursor-agent mcp list-tools model-router` | Tools listed |
| Amp | 0.0.1795838433 | `amp tools list` | 17 tools |
| Pi | 1.7.0 | `pi mcp list`, stdio and `POST /mcp` | 17 tools on both; wrong token reported |
| oh-my-pi | 19.2.1 | omp's `discoverAndLoadMCPTools` | 17 tools |
| goose | 1.61.1 | `goose acp` → `session/new` | Session opens with the extension |
| GitHub Copilot CLI | 1.0.117 | `copilot mcp get` | Config accepted; `copilot --acp` needs `copilot login` before it starts MCP servers, so not connected |
| Aider | — | — | No MCP client; use the HTTP JSON API or the CLI |
| MCP Python SDK | latest | stdio and `POST /mcp`, full run | Tools, prompts, completion, resources, run to `complete` |
| MCP Inspector (TypeScript SDK) | latest | stdio and `POST /mcp` | Lists and calls work |

Client notes:

- Gemini CLI disables every MCP server in a folder it does not trust. Trust
  the project in Gemini first (or add it to `~/.gemini/trustedFolders.json`).
- Codex passes stdio servers a fixed set of environment variables (`HOME`,
  `PATH`, and a few others), so variables such as `MODEL_ROUTER_CONFIG_DIR`
  set in your shell do not reach the server. The trust store under `HOME` is
  used.
- Codex, Claude Code, Amp, and goose probe `server/discover` first; the
  method-not-found answer makes them fall back to `initialize`.

## Protocol

- Revision `2025-11-25`. `initialize` echoes `2025-06-18`, `2025-03-26`, or
  `2024-11-05` when the client asks for one; any other request (including a
  newer revision) gets `2025-11-25`. `server/discover` (revision
  `2026-07-28`) is answered with method-not-found, so clients fall back to
  `initialize`.
- JSON-RPC batches are refused with `-32600` at every revision, including
  `2025-03-26`, which allowed them. A message with neither a `method` nor a
  `result`/`error` gets `-32600` with its `id` (or `null`).
- Capabilities: `tools`, `resources`, `prompts`, and `completions`.
- Each tool has a `title` and `annotations`: `readOnlyHint` (reads only),
  `destructiveHint` (`cancel_run`, `apply_run`), `idempotentHint`, and
  `openWorldHint` (`start_run`, `confirm_stage`, `retry_stage`, `fork_run`:
  they launch harnesses that reach model providers). Delegation tools carry
  the same annotations.

## Tools

Every tool takes an optional `project` (default: the server's project).
Relative paths resolve against the server's project.

| Tool | Arguments | Returns |
| --- | --- | --- |
| `list_workflows` | | Workflows (first is the default) and stages: id, role, harness, model, mode, enabled |
| `preview_run` | `workflow?`, `task` | The exact harness commands, as `model-router preview`; runs nothing |
| `start_run` | `workflow?`, `task`, `worktree?`, `overrides?: [{stage, harness?, model?, enabled?}]`, `force_unattended?` | Run status with `run_id`; the run continues on a background thread |
| `run_status` | `run_id` | Status, cursor, current stage, per-stage status and handoff verdict, latest handoff, elapsed seconds, usage, `next` hint |
| `wait_run` | `run_id`, `timeout_seconds?` (default 60, max 3600) | Run status plus `timed_out`, once the run completes, stops, or waits for a decision; emits `notifications/progress` when `_meta.progressToken` is set, and aborts without a response on `notifications/cancelled` |
| `cancel_run` | `run_id` | Stops the harness; the run is saved as cancelled |
| `list_runs` | `status?` (comma-separated, as `history --status`), `limit?` | History summaries, newest first |
| `get_handoff` | `run_id`, `stage` (ID, name, or 1-based number) | The stage's full handoff, waiver, and attempts |
| `get_report` | `run_id` | `report --json`: time, usage, checks, unresolved items, review summary, and `changes` (files changed during the run) |
| `confirm_stage` | `run_id` | Accepts a checkpoint (`awaiting_confirmation`) and continues the run |
| `retry_stage` | `run_id`, `model?`, `force_unattended?` | Retries the current stage of a stopped or waiting run and continues it |
| `fork_run` | `run_id`, `stage`, `model?`, `force_unattended?`, `no_run?` | Forks a saved run in a new Git worktree from a stage checkpoint, reusing completed upstream handoffs; drives the fork unless `no_run: true` |
| `apply_run` | `run_id`, `check_only?` (default `true`) | Checks (`check_only: true`) or applies (`check_only: false`) a finished worktree run's uncommitted changes back into the project checkout; enforces `protected_paths` |
| `get_handoffs` | `run_id`, `stage?`, `include_prompt?` (default `false`) | Handoff budget, truncation, and field-loss report (`handoffs --json`), plus the rendered prompt when `include_prompt: true` |
| `get_cost` | `since?` (`7d`, `12h`, `30m`, `2w`) | Project cost and token usage summary (`cost --json`) |
| `get_stats` | `role?`, `min_pass?` (`0`–`1`) | Per-(role, harness, model) routing stats (`stats --json`), with an optional cost-quality shortlist |
| `list_schedules` | | Project schedules with approval status, `next_due`, and recent occurrences (`schedules list --json`) |

Tool failures (bad arguments, unknown run, interactive workflow, …) are tool
results with `isError: true` and a text explanation. Unknown tools and
methods are JSON-RPC errors.

## MCP resources

`initialize` advertises `capabilities.resources`, exposing saved run
artifacts as read-only MCP resources:

- `resources/list` lists the 20 most recent runs' review briefs
  (`review.md`), diffs (`changes.diff`), and reports under the server's
  project as `model-router://runs/<run_id>/review` (`text/markdown`),
  `model-router://runs/<run_id>/diff` (`text/x-diff`), and
  `model-router://runs/<run_id>/report` (`application/json`, the
  `get_report` result, computed on read).
- `resources/templates/list` returns the same three URIs with a
  `{run_id}` placeholder.
- `resources/read` reads any of them (optionally with `?project=DIR`,
  URL-encoded). Files are bounded to 2 MiB and opened without following
  symlinks beneath the project directory. `resources/list` and
  `resources/read` enforce `--allow-root` and project trust.

## MCP prompts

`prompts/list` and `prompts/get` offer instructions a client can hand its
model. Prompts and completion use the server's own project.

| Prompt | Arguments | Messages |
| --- | --- | --- |
| `run_task` | `task`, `workflow?` | Steps to run the task with `start_run`, `wait_run`, `get_handoff`, and `get_report`, asking the user before `confirm_stage` or `retry_stage`. An unknown workflow is invalid params. |
| `review_run` | `run_id` | A review instruction, the run's report, and its `review.md` when present, as embedded resources. |
| `unblock_run` | `run_id` | The run's status and the handoff of the stage at its cursor (by stage ID), as text, with an instruction to choose `confirm_stage`, `retry_stage`, or reporting the blocker. |

`completion/complete` completes `workflow` (configured workflow names) and
`run_id` (the 50 most recent runs) by prefix, for prompt arguments and the
`run_id` of resource templates; at most 100 values.

A typical client loop: `list_workflows` → `start_run` → `wait_run` (repeat
while `timed_out`) → `get_report`. When `status` is `awaiting_confirmation`,
read the handoff with `get_handoff`, then `confirm_stage` or `retry_stage`.
When it is `failed`, `blocked`, or `cancelled`, `retry_stage` (optionally with
another `model`) reruns the current stage; earlier changes are kept.

## Delegation hosts

`serve` drives runs that a delegating stage's agent launches (`run_launch`)
and that schedules fire, like runs its own clients start. It fires the
server project's approved, due schedules every 30 s, and with `--http` adds
`POST /schedules/<id>/trigger` (bearer token, like every route) for webhook
schedules: `200 {"result": {"runs": [...]}}`, or `409` when the schedule is
not an approved, enabled webhook schedule. `run_status` reports each
delegating stage's task counts, and `get_handoff` includes its tasks'
handoffs. See [delegation.md](delegation.md).

## Concurrency

Each run is driven on its own thread, so several runs proceed at once. As in
the TUI, a run that starts while another run (in this server or another
router process) is using the checkout gets its own Git worktree from committed
HEAD on branch `model-router/RUN_ID`; outside Git it is refused. `worktree:
true` asks for a worktree explicitly. `cancel_run` uses the engine's cancel
flag; the harness's process group is stopped.

When stdin closes (MCP) or the server gets SIGINT/SIGTERM, it cancels every
run it is driving and waits up to 15 s for their harnesses to stop. Runs
saved as cancelled can be retried later from the server, CLI, or TUI.

## Safety

- `project` must lie under an allowed root. The default is the server's own
  project; add roots with `--allow-root PATH` (repeatable).
- Each project's configuration is read on every call, so harness commands
  come from that project's `.model-router.toml` (or the global config).
  `--config` applies to the server's own project only.
- Every project must pass [project trust](safety.md#project-trust): a call
  naming a project whose `.model-router.toml` has untrusted sections fails
  until you run `model-router -C DIR trust`.
- MCP: stdout carries JSON-RPC only; diagnostics go to stderr. Request lines are
  capped at 2 MiB. `tools/call`, `resources/list`, `resources/read`,
  `prompts/get`, and `completion/complete` run on worker threads, at most 32
  at once, of which at most 24 may be blocking `wait_run` calls (leaving
  slots for non-blocking calls such as `cancel_run` and `run_status`). A
  worker that fails unexpectedly answers with an error (a tool error for
  `tools/call`, `-32603` otherwise) instead of leaving the request
  unanswered. Finished run handles kept in memory are capped at 128 (disk
  records are unaffected).

## HTTP API

`--http ADDR` binds loopback only unless `--allow-remote` is given. Every
route except `GET /health` needs `Authorization: Bearer TOKEN`. The token comes
from `MODEL_ROUTER_SERVE_TOKEN`, or is generated at startup and printed to
stderr. Port 0 picks a free port; the bound address is printed to stderr. HTTP
allows at most 16 pending unauthenticated connections and 32 authenticated
connections (of which at most 24 may be event streams or `wait_run` calls),
with a 64 KiB header cap, 1 MiB body cap, and 10-second request read deadline.

| Route | Body / query | Response |
| --- | --- | --- |
| `GET /health` | | Unauthenticated liveness probe: `200 {"status": "ok", "version": "..."}` (no project or run data) |
| `GET /tools` | | `{"tools": [...]}` with input schemas |
| `POST /tools/<name>` | JSON arguments object (empty body = `{}`) | `200 {"result": ...}` or `400 {"error": "..."}`; unknown tool `404` |
| `GET /runs/<id>/events` | `?project=DIR` (URL-encoded, optional) | Server-Sent Events |
| `POST /mcp` | One JSON-RPC message | MCP Streamable HTTP: `200` with the JSON-RPC response, or `202` with no body for a notification or response. `400` for invalid JSON (`-32700`), a batch or a message with no method (`-32600`), or an `MCP-Protocol-Version` header naming an unsupported revision (except on `initialize` and `server/discover`). `503` with a JSON-RPC error (`-32000`) when a `wait_run` finds every long-lived slot taken. `403` when an `Origin` header is not `http(s)://` + `localhost`, `127.0.0.1`, or `[::1]` + an optional port. A blocking `wait_run` ends early, answering with `timed_out: true`, on a `notifications/cancelled` POST naming its request ID (IDs are not scoped per client: there are no sessions), when its client closes or half-closes the connection, or at shutdown. Bytes sent after the request body are discarded. `GET /mcp` is `405`: there is no server-to-client stream, so progress notifications are stdio-only. |

```sh
curl -s -H "Authorization: Bearer $TOKEN" -d '{"task":"Fix the flaky test"}' \
  http://127.0.0.1:8737/tools/start_run
curl -N -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8737/runs/$RUN_ID/events
```

The event stream sends `event: state` (the `run_status` object) first and on
every engine state change, `event: output` (`{"text": ...}`) for harness and
check output, and `event: end` (final status) when the run stops, completes, or
waits for a decision. Up to 500 recent events are replayed to a subscriber
that connects mid-run. A run the server is not driving gets a single `end`.
Comments (`: keep-alive`) are sent every 15 s. Connections are closed after
each response; there is no keep-alive or chunked request support.
