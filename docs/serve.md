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

Tool failures (bad arguments, unknown run, interactive workflow, …) are tool
results with `isError: true` and a text explanation. Unknown tools and
methods are JSON-RPC errors.

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
  capped at 2 MiB and concurrent `tools/call` workers at 32, of which at most
  24 may be blocking `wait_run` calls (leaving slots for non-blocking calls
  such as `cancel_run` and `run_status`). Finished run handles kept in memory
  are capped at 128 (disk records are unaffected).

## HTTP API

`--http ADDR` binds loopback only unless `--allow-remote` is given. Every
request needs `Authorization: Bearer TOKEN`. The token comes from
`MODEL_ROUTER_SERVE_TOKEN`, or is generated at startup and printed to stderr.
Port 0 picks a free port; the bound address is printed to stderr. HTTP allows at
most 16 pending unauthenticated connections and 32 authenticated connections (of
which at most 24 may be event streams or `wait_run` calls), with a 64 KiB header
cap, 1 MiB body cap, and 10-second request read deadline.

| Route | Body / query | Response |
| --- | --- | --- |
| `GET /tools` | | `{"tools": [...]}` with input schemas |
| `POST /tools/<name>` | JSON arguments object (empty body = `{}`) | `200 {"result": ...}` or `400 {"error": "..."}`; unknown tool `404` |
| `GET /runs/<id>/events` | `?project=DIR` (URL-encoded, optional) | Server-Sent Events |

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
