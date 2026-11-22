# Delegation

Orchestration agents: an agent running inside a stage hands sub-tasks to
other harnesses and models through the router, launches and reads runs, and
schedules work. This page began as the build specification and describes
the implementation (`src/delegation/`, `src/engine/delegate*.rs`). The reference is T3 Code's orchestrator MCP server
(`pingdotgg/t3code`, `docs/orchestration-v2/orchestrator-mcp-server.md`);
where model-router differs, the reason is given.

## Goals

- A stage's agent can delegate a self-contained task to any allowed
  harness/model mid-run, without the workflow naming it in advance.
- The driving `mr` owns every child: process groups, sandbox, worktrees,
  budget, records, cancellation. The agent only asks.
- Completion wakes the parent; an agent whose harness can be resumed never
  polls.
- Every [invariant](../AGENTS.md#invariants) holds: argv only, no silent
  substitution, trust for anything that runs commands, no stored credentials,
  process groups end with their stage.
- Tests, smoke tests, and the demo stay offline.

## Non-goals

- Linked environments, device and browser tools, secret requests.
- A Streamable-HTTP MCP server or an async runtime.
- Interactive children. Every task runs unattended.
- Recursive delegation. Tasks get no tools; only stage parents delegate.
- Typing into a terminal to wake an interactive parent.
- Webhook signature secrets (the router stores no credentials).
- Replaying in-flight delegation after an `mr` crash. Recovery stays
  "mark interrupted, user retries".
- `t3_submit_output`/`outputSchema`: the handoff is already the structured
  result.
- Agents confirming checkpoints or ship plans. Those stay user actions.

## Terms

| Term | Meaning |
| --- | --- |
| parent | The stage attempt whose harness calls the tools. |
| task | One delegated job: a child harness invocation plus its follow-up turns. |
| turn | One harness process of a parent or task: the first launch or one `follow_up`. |
| cohort | The tasks a parent has started whose results it has not been given yet. |
| owner | The scheduler loop (`engine::schedule`) that owns the run: it alone writes `run.json`. |
| broker | The per-turn server thread in the driving `mr` that answers tool calls. |
| bridge | `mr __mcp-bridge`, the stdio MCP process the harness starts. |

## Configuration

Delegation is off unless a stage enables it.

```toml
# Named targets: fixed settings the model picks by name.
[delegates.codex-review]
harness = "codex"
model = "gpt-6.2-sol"
effort = "high"
harness_settings = { sandbox = "read-only" }
timeout_seconds = 1200

[delegates.claude-impl]
harness = "claude"
model = "claude-opus-5-5"
isolation = "worktree"        # default isolation for this target

[[workflows.stages]]
id = "implement"
harness = "claude"
[workflows.stages.delegation]
delegates = ["codex-review", "claude-impl"]   # named targets this stage may use
# Ad-hoc targets: the model may also pick any harness/model inside these.
harnesses = ["codex", "claude", "opencode"]   # [] = named targets only (default)
models = ["gpt-6.*", "claude-*"]              # globs; [] = any catalogued model
providers = []                                 # router providers ad-hoc targets may use
max_tasks = 12         # delegate_task calls per parent attempt. Hard cap 64.
max_parallel = 3       # live tasks per parent attempt; separate from workflow max_parallel
max_wake_turns = 8     # parent follow-ups that deliver results. Hard cap 32.
runs = false           # run_* tools
max_runs = 4           # runs one parent attempt may launch
schedules = false      # schedule tools
```

### Fields and loading

- `[delegates.NAME]` takes `harness`, `model`, `provider`, `effort`,
  `harness_settings`, `timeout_seconds`, `stall_timeout_seconds`,
  `isolation`, `billing`. Ad-hoc targets take the parent stage's
  `timeout_seconds` and `stall_timeout_seconds`. No task outlives the stage
  deadline. It takes no `extra_args`, `sandbox*` (including `sandbox_fallback`), `mode`,
  `checks`, or `budget`: a task's sandbox and permissions come from the
  [privilege ceiling](#privilege-ceiling), and tasks are always unattended.
- Config files do not merge: `load_config` reads the project's
  `.model-router.toml` or, without one, the global file (`config.rs:1313`).
  Named delegates must be in the same file as the workflow that names them.
- Ad-hoc targets: with `harnesses` non-empty, the model may pass
  `{harness, model?, effort?, provider?}`. The harness must be installed
  (cached `doctor` probe). With `models` set, the model is required and must
  match one of its globs; without it, a given model must be in the harness
  catalog (any model for `CatalogSource::Manual` harnesses). Providers must be
  listed in `providers` and configured. Ad-hoc targets take no
  `harness_settings`. The router never substitutes a model.
- Stage `extra_args` never cross to a task.
- `delegation` on a fan-out stage, a ship stage, or a summary stage is a
  config error.
- A delegating stage is never `graph::parallel_safe`, whatever its role, so
  no sibling stage's launch, repair, or checks run on the owner loop while it
  waits on tool calls.

### Trust and run snapshot

- `delegation` and `[delegates.*]` launch commands and spend money. In a repo
  `.model-router.toml` they need `model-router trust`: both join
  `trust::dangerous_parts` and `trust::snapshot_items_parts`. Untrusted, they
  are ignored with a warning, like other untrusted sections (`trust::ignore`).
- `Run` gains `delegates: BTreeMap<String, Delegate>`: the resolved named
  delegates of every delegating stage, snapshotted at creation. `run_items`
  hashes it with the workflow, adapters, and providers, so a saved run's
  approval covers it and a replay never reads the current config's delegates.
  `store::load` validates it like `adapters`.

## Architecture

```text
stage harness (own process group, own sandbox)
  └─ mr __mcp-bridge  (stdio MCP; reads socket path + token file from env)
        │  Unix socket, per-turn token
        ▼
broker (delegation::broker; one per parent turn)
        │  Msg::Call { request, reply }
        ▼
owner loop (engine::delegate): policy, persist, launch, wake
        ├─ parent turn 1 (unattended worker, or the interactive terminal host)
        ├─ parent wake turns 2..n (follow_up workers)
        ├─ task turns (workers, own process group and sandbox)
        └─ ticker thread             Msg::Tick every 100 ms
```

### Ownership

A delegating stage is never parallel-safe (`graph::parallel_safe`), so while
it runs it is the only thing in flight. It runs in its own owner loop,
`engine::delegate::drive_stage`, entered from the list driver
(`engine::drive`), the graph scheduler (`schedule`'s `Launch::Stage`), and
`run_interactive`. The loop is the only writer of `run.json` while it runs.

- **Messages:** `Spawned`, `Terminal` (an interactive parent's host socket),
  `Output`, `ParentDone`, `TaskDone`, `Call` (a tool request and its reply
  channel), and `Tick`.
- **Parent phases:**

  ```text
  Running(turn) --exit 0, unread results--> Awaiting
  Awaiting --cohort ready--> Running(turn+1)   (a wake turn)
  Running(turn) --exit 0, nothing unread--> Done (conclude: handoff, repair, checks)
  Running(1) --released to its terminal host--> Releasing --tasks settled--> Done
  any --failure | deadline | cancel | refused wake--> Done (stopped)
  ```

- **Blocking tool waits** (`mode: "wait"`, `task_wait`, `run_wait`) wait in
  the broker's call threads on a condition the owner signals when task state
  changes. The owner never blocks on a tool call.
- **Ticks** wake an idle owner for the stage deadline (`timeout_seconds`
  covers the whole stage) and for cancellation. Interactive parents have no
  stage deadline. The ticker stops with a flag and an unpark before the
  channel drains.
- **Stall timeout:** a parent prints nothing while a tool call waits (up to
  45 s), so a delegating stage's `stall_timeout_seconds` must be at least 60
  (config validation).
- **Final:** `conclude` (handoff, final-turn repair, checks) runs after the
  broker is closed and no task runs. `task_apply` runs on the owner.
- **Shutdown:** the owner closes the broker (its close flag ends every call's
  wait with `parent_not_active`, the accept loop is woken through a socket
  pair, the live connection is shut down, and the thread joined), cancels
  live tasks, stops the ticker, answers stray calls, and drains until every
  worker reported.
- Each task and wake process gets its own process group, journaled in
  `SpawnJournal` before `run.json` records it, and its own output paths.

### Bridge

- Hidden subcommand `mr __mcp-bridge`, dispatched in `src/main.rs` beside
  `process::host::ARG`. A stateless forwarder: newline JSON-RPC from stdin to
  the socket and back. Killing it with the stage's process group loses
  nothing.
- MCP framing, `initialize`, `tools/list`, `notifications/cancelled`, and
  progress reuse `src/serve/mcp.rs`, parameterized over a tool-set trait so
  `serve` and the broker share one implementation. Protocol `2025-06-18`.
  Server name `model-router`.
- When the socket refuses (turn over, broker closed, parent released) the
  bridge answers every call with `parent_not_active` itself.

### Socket and token

- Directory: `model-router-orch` beside `process::host::socket_dir()`: same
  private 0700 rule, fallback `/tmp/model-router-orch-UID`, and the length
  check uses the real name (`orch-<16 hex>.sock`, 5 bytes longer than the
  host's). Never one of `host::socket_dirs()`. `orch_socket_dirs()` lists
  every candidate location (`XDG_RUNTIME_DIR`, `TMPDIR`, `/tmp`) like
  `socket_dirs()` does.
- Per parent turn the broker creates `orch-<16 hex>.sock` (0600) and
  `orch-<16 hex>.token` (0600) holding 32 bytes from the OS RNG
  (`ring::rand::SystemRandom`; `ring` is already a direct dependency),
  hex-encoded. Both are deleted when
  the turn ends; a wake turn gets a new pair.
- The harness gets `MODEL_ROUTER_ORCH_SOCK` and `MODEL_ROUTER_ORCH_TOKEN_FILE`:
  paths, not secrets, so they may appear in `mcp.json`, argv, previews, and
  logs. The token itself never leaves the token file and broker memory, and
  is never written to `run.json` or a log.
- Every launch that is not a delegating turn (tasks, checks,
  other stages, ship) removes both variables from its environment.
- Handshake: the bridge's first line is the token; the broker compares in
  constant time (`serve::http::same`, made `pub(crate)`) and drops the
  connection on mismatch. One live connection per turn; a new connection with
  the right token replaces a closed one (a harness may restart its MCP
  servers); a second concurrent one is refused.
- **Sandbox, every sandboxed stage** (delegating or not, so no stage can reach
  another's socket or token):
  - bwrap: every `orch_socket_dirs()` entry joins `Plan::hidden` (tmpfs over
    it, as for `host::socket_dirs()`); a delegating turn then binds back only its own
    socket and token file (`--bind SOCK SOCK`, `--ro-bind TOKEN TOKEN`) and
    `--ro-bind` of `current_exe()`.
  - Seatbelt (last matching rule wins, `sandbox.rs:617`): every
    `orch_socket_dirs()` entry gets `(deny file-read-data file-write*
    network-outbound (subpath DIR))`. The existing hidden rule
    (`sandbox.rs:657`) denies only `file-write* network-outbound`, which is
    enough for host sockets but not for token files.
    For a delegating turn, after every deny including `(deny network*)`:
    `(allow file-read-data (literal TOKEN))`,
    `(allow file-read-metadata (literal SOCK))`, and
    `(allow network-outbound (remote unix-socket (path-literal SOCK)))`.
    The rule text is checked by the macOS connectivity test below, not by
    review alone.
  - `orch_socket_dir()` is created eagerly beside the host's
    (`sandbox.rs:324`); `SOCK` and `TOKEN` are canonicalized for Seatbelt
    (`/private/var/folders`); the bridge program bound into bwrap is the
    resolved program (`current_exe()` or the test hook).
  - Tests: `sandbox::Plan` unit tests assert the rule order and binds;
    `tests/delegation.rs` connects through a real bwrap sandbox on Linux and
    a real Seatbelt profile on macOS CI, and fails to reach a sibling turn's
    socket or token, with network on and off. bwrap runs the stage in its own
    user namespace, so it cannot read another process's `/proc/PID/environ`
    or `/root`.
- Unsandboxed stages (`sandbox = "off"`, the default): any process of the same
  user can read the token file while the turn runs. `docs/safety.md` lists
  this under "Not enforced", as it does for same-user access today.

## Tools

All tools return `structuredContent` and a short text. Failures are tool
results with `isError: true` and `structuredContent.error.code` (see
[Failures](#failures)). Mutating tools take an optional `client_request_id`
(see [Idempotency](#idempotency)).

### Delegation

| Tool | Purpose |
| --- | --- |
| `orchestrator_capabilities` | Caller's harness, model, ceiling, isolation, `supports_wake`; named delegates and ad-hoc harnesses with up to 8 models each, efforts, availability and constraints; remaining task, parallel, wake, and run budget; feature flags. `search` filters models. |
| `delegate_task` | Start a task. The child sees only `task`, its role brief, and the handoff contract. |
| `task_status` | One task's durable state. |
| `task_wait` | Block until any of `task_ids` is terminal or waiting on a question, ≤45 s, then return the state of every listed task (each terminal or `waiting` one is delivered). A timeout cancels nothing. |
| `task_cancel` | Stop a task. |
| `task_send` | Send a message to a task's session (`queue` or `restart`); at most 4 per task. |
| `task_apply` | Apply a worktree task's patch to the parent's checkout. |

`delegate_task` input:

```json
{
  "task": "self-contained brief",
  "target": {"delegate": "codex-review"},
  "title": "Review auth change",
  "role": "review",
  "isolation": "shared",
  "mode": "async",
  "timeout_ms": 30000,
  "client_request_id": "review-auth-r1"
}
```

- `target`: `{"delegate": NAME}` or `{"harness", "model"?, "effort"?,
  "provider"?}`. Omitted: the stage's first named delegate; with none,
  `invalid_request`.
- `role`: a `config::Role` a task may take: `research`, `planning`,
  `implementation`, `review`, `test` (not `summary` or `custom`). Default `implementation` for
  `worktree`, `research` for `shared`. It selects the role brief
  (`engine::prompt`) and the verdicts the task may return.
- `isolation`: `shared` (read-only) or `worktree`; default from the delegate,
  else from the role (`implementation` → `worktree`). See
  [Isolation](#isolation).
- `mode`: `async` (default for unattended parents with `supports_wake` and
  for every interactive parent) or `wait` (≤45 s; capped at the parent
  harness's tool timeout minus 15 s). A question or a timeout ends a wait;
  neither cancels the task. Unattended parents without `supports_wake` must
  use `wait` or `task_wait` (`wake_unsupported` otherwise).

Result (also from `task_status`):

```json
{
  "task_id": "t3",
  "status": "running",
  "work_state": "working",
  "target": {"delegate": "codex-review", "harness": "codex", "model": "gpt-6.2-sol", "effort": "high"},
  "executed_model": null,
  "role": "review",
  "isolation": "shared",
  "turns": 1,
  "summary": null,
  "verdict": null,
  "handoff_file": null,
  "questions": [],
  "has_follow_up": true,
  "changes": null,
  "patches": [],
  "usage": null,
  "latest_turn_status": null,
  "latest_turn_summary": null,
  "wait_timed_out": false,
  "next": "Completion resumes this session. Do not wait on it or poll it. Continue other work, and end your turn when none is left."
}
```

- `status`: `queued`, `starting`, `running`, `waiting` (blocked on a
  question), `completed`, `failed`, `blocked` (a question that cannot be
  answered, or was left unanswered at the parent's final turn), `cancelled`,
  `interrupted`.
- `work_state`: `working` (`queued`/`starting`/`running`),
  `needs_answer` (`waiting`), `result_available` (terminal).
- `summary`, `verdict`, `handoff_file`, `changes` come from the task's first
  terminal turn and stay stable; `latest_turn_*` report later `task_send`
  turns.
- `patches`: every sealed patch revision of a worktree task:
  `{revision, turn, files, added, removed, applied}`. `task_apply` takes one
  of these revisions.
- `questions`: the `unresolved` items of a `waiting` or `blocked` task's
  handoff; kept after the task settles.
- `next`: present while the task is unfinished. Unattended with
  `supports_wake`: the text above. Unattended without it: `Call task_wait with
  this task_id again while wait_timed_out is true.` Interactive: `Tell the
  user what you wait on; read the result with task_status when they prompt
  you.`

### Runs

Enabled by `delegation.runs = true`. A run is the router's top-level unit,
the counterpart of a T3 thread.

| Tool | Purpose |
| --- | --- |
| `run_launch` | Create 1–20 runs: each `{workflow?, task}`. |
| `run_list` | This project's runs, newest first; filter by status; offset-paginated. |
| `run_read` | Status, stages, handoff summaries, and stage output tail; bounded to about 30,000 characters; `cursor` is a byte offset into the selected stage's `stdout.log`, moved forward to the next UTF-8 boundary. |
| `run_wait` | Block until a run completes, stops, or waits for a decision, ≤45 s. A run no host drives returns at once with `driven: false`. |
| `run_retry` | Retry the current stage of a stopped run this parent launched (as `serve`'s `retry_stage`). |
| `run_cancel` | Cancel a run this parent launched. |

- Workflows: only workflows from the trusted config whose enabled stages are
  all unattended (as `serve`, `serve/tools.rs:385`); no `force_unattended`.
  `orchestrator_capabilities` lists the launchable ones.
- Ceiling: the resolved plan of the chosen workflow, meaning every enabled
  stage, fan-out candidate, and router check, at or below the caller's
  ceiling. Router checks run unsandboxed (`checks.rs:128`), and so does a
  ship stage's `secret_scanner` (`ship.rs:613`); both count as write
  `workspace`, native `default`, network `open`, enforcement `none`.
- Checkout: launched runs always get their own worktree from committed HEAD
  (`model-router/RUN_ID`), since the launcher holds the workspace lock
  (`store::lock_run`). Outside Git, `run_launch` is refused.
- Budget: at every admission check of the launcher, or of any run it
  launched (also after a later `resume`), the router loads the launcher and
  its launched runs read-only. Launched spend counts toward the launcher's
  run caps when `launched_by.run` matches, and toward a launcher stage's caps
  when `(launched_by.run, launched_by.stage)` matches, across that stage's
  attempts. Each
  launched run's own workflow caps apply too. If the launcher's record is
  gone (cleaned), only the run's own caps apply, with a warning.
- No recursion: inside a launched or scheduled run, `runs` and `schedules`
  are forced off. `max_runs` caps launches per parent attempt.
- Who drives them: the process-wide `RunHost` (`engine::set_run_host`). The TUI and `serve` hosts drive launched runs like other session
  runs. The CLI host creates them `pending` (`driven: false`), prints
  `model-router resume ID` for each, and lists them in its report; no
  detached `mr` is spawned. `run_retry` on a run no host drives is refused.
- Provenance: `Run.launched_by: {run, stage, attempt}` and
  `Run.creation_source: "user" | "agent" | "schedule"`, shown in `history`,
  the TUI, and `run_list`.
- Shipping: a ship stage in a run with `creation_source` other than `user`
  always waits for user confirmation, whatever `ship.confirm` says
  (`ship.rs:1147`).
- `run_retry` rechecks the run's resolved plan against the requesting
  stage's ceiling every time, not only the ceiling of the stage that
  launched it.
- Reads: `run_list` and `run_read` cover runs of the caller's project. Log
  reads open each path component beneath the run directory with
  `openat(O_NOFOLLOW)` (a new `storage::open_beneath`) and require a regular
  file, so a swapped ancestor cannot redirect the read. `run_retry` and `run_cancel` only act on runs whose `launched_by`
  is the caller's run.
- No `run_send`: confirming or rejecting a launched run's checkpoint stays a
  user action. Conversation follow-ups go to tasks (`task_send`).

### Schedules

Enabled by `delegation.schedules = true`.

| Tool | Purpose |
| --- | --- |
| `schedule_task` | Create or update (`schedule_id`): `{workflow?, task, every? \| at? \| webhook: true, enabled}`. |
| `list_scheduled_tasks` | This project's schedules, with approval and executor state. |
| `delete_scheduled_task` | Delete permanently. |

- Definitions live in `.model-router/schedules.json`. Each firing launches a
  run as `run_launch` does (own worktree, unattended workflow,
  `creation_source: "schedule"`, `runs`/`schedules` off).
- Approval: a schedule fires only with an approval receipt at
  `config_dir()/schedule-approvals/<project hash>/<schedule id>` holding the
  approved digest, outside the workspace, as run approvals
  are (`trust.rs:363`, `store.rs:86`). The receipt is keyed by a digest of
  the canonical project path, schedule id, the resolved plan of its workflow
  (stages, delegates, candidates, checks), task, trigger, `enabled`, the
  creator's ceiling, and the project's `trust::check` digest. Any pause (user,
  agent `enabled: false`, or a failed check at firing) and any edit deletes
  the schedule's receipt, so re-enabling or changing it needs approval again. No sandbox plan grants
  `config_dir()`: `mr` creates it (0700) at startup, before any launch, like
  `orch_socket_dir()`, and adds it to `Plan::protected` (bwrap `--ro-bind`;
  a Seatbelt deny after the `inside` re-allow),
  and a `sandbox_writable` entry overlapping it is a config error.
  `trust::revoke` deletes the project's receipts.
  Any edit to the schedule, its workflow, or a trusted config section changes
  the digest, so the schedule waits for approval again. Agents cannot write
  `config_dir()` from a sandbox; unsandboxed, the same-user caveat applies.
  `model-router schedules approve ID` or `a` on the TUI Runs page approves.
- Firing rechecks approval, trust, and the ceiling. A failed check pauses the
  schedule with the reason.
- Executors: `mr serve` runs a scheduler thread and drives fired runs as its
  session runs. `mr schedules tick`, for cron or systemd/launchd timers, is
  its own `RunHost`: it fires due schedules, drives each fired run until it
  completes, reaches a checkpoint, or stops, then exits; the exit code is
  non-zero if any occurrence failed. A firing claims
  `(schedule id, due time)` under `.model-router/schedules.lock` and records
  it in `schedules.json` before launching, so `serve` and `tick` never both
  fire one occurrence. A launch that fails after the claim records the
  occurrence as failed with the reason; it is not retried.
- Webhooks: `serve --http` adds `POST /schedules/<id>/trigger`, behind the
  server's bearer token like every route. No signature secrets.
- Budget: a scheduled run has only its workflow's caps.

## Lifecycle

### Delegated task

```text
delegate_task
  -> broker -> owner: resolve target, ceiling, caps, budget admission
  -> persist TaskState{status: queued, key, fingerprint}
  -> worktree if isolation = worktree
  -> task turn 1 (group journaled) -> starting -> running
  -> exit -> usage, handoff, session id, sealed patch (worktree)
  -> completed | failed | waiting (question) | ...
  -> cohort bookkeeping -> wake the parent when the cohort is ready
```

- Tasks run unattended with `engine::prompt`: the task text as the task, the
  role brief, the handoff contract, and one line: `To ask whoever delegated
  this task a question, return verdict blocked with your questions in
  unresolved.` No parent history, no tools.
- Tasks run no router checks; the parent's final checks cover what reached
  the checkout.
- A queued task that budget admission or a launch error refuses after
  `delegate_task` returned settles `failed` with `error.code`
  (`budget_exceeded`, …), and waiters and cohort readiness are signalled as
  for any terminal task.
- A task turn's sandbox pins `<task dir>/turn-NNN/` as both its attempt and
  handoff directory; its handoff file is `turn-NNN/handoff.json`. The rest of
  the task directory (`patches/`, records) is router-owned and writable by no
  task. Task directories sit in `<NN-stage>/attempt-NNN.tasks/`, outside the
  sandboxed parent's writable set (the parent can write its attempt directory
  and `handoffs/<run>/`, `sandbox.rs:234`). An unsandboxed parent can write
  anything its user can; integrity checks therefore use hashes kept in
  `run.json`, not files beside the data.

### Handoff turns of a parent

```text
turn 1   writes handoffs/<run>/<stage>-NNN.json (provisional if the cohort is open)
         -> exit 0, cohort open: store::set_aside_handoff(turns/turn-001/) -> turn-001/handoff-file.json
            record session id (Assign: known; Reported: read from stdout.log now)
         -> AwaitingCohort
wake 2   follow_up writes a fresh handoffs/<run>/<stage>-NNN.json
         -> exit 0, cohort open: set aside -> turns/turn-002/handoff-file.json ... repeat
final    exit 0, cohort empty: resolve this turn's handoff only
         -> handoff_repair allowed (this turn only), checks, conclude
```

- A provisional handoff is recorded, never accepted, and never repaired.
- A final turn that writes no handoff fails as a missing handoff does today;
  a set-aside provisional file is never resolved in its place.
- The instructions tell a parent with live tasks to end its turn with
  verdict `blocked` and summary `waiting on tasks`; the router ignores that
  verdict on provisional turns, so `blocked` does not stop the stage
  (`exec.rs:611`).

### Waking the parent

T3 queues a completion message after the parent's active turn
(`ProviderContinuationService`, `queue_after_active`). One-shot harnesses
cannot be steered mid-turn, so the router delivers at process boundaries:

- **Cohort** = the parent's undelivered tasks. It is **ready** when every one
  is terminal or `waiting`. One wake per ready cohort.
- **Wake prompt**, written to `turns/turn-NNN/prompt.md`:

  ```text
  Delegated tasks reached a state that needs you:
  - t1 (codex-review): completed, verdict pass
  - t3 (claude-impl): waiting, 2 questions
  Use task_status for each, answer questions with task_send, then continue
  or write your final handoff.
  ```

- **Delivered**: a task is delivered when a tool result returns it terminal
  or `waiting` (`task_status`, `task_wait`, wait-mode `delegate_task`), or
  when the parent itself cancels it. A wake prompt only announces; it
  delivers nothing. A timed-out wait delivers nothing. A turn that has read
  every result ends with an empty cohort and writes the final handoff.
- **Unread after a wake**: a wake turn that exits 0 with any announced task
  turn still unread stops the stage `blocked` (`delegation: results not
  read`), even if the turn started new tasks, so an announced result never
  costs a second wake.
- A wake runs `adapters::follow_up` on the owner loop with the bridge attached
  (`Part::Mcp` in the follow-up template) and a new socket and token.
- **Refused or impossible wake** — `max_wake_turns` reached, budget admission
  refuses it, no session id, or the harness/provider has no `follow_up`: the
  owner cancels live tasks and stops the stage `blocked` with the reason
  (`delegation: wake refused by budget`, …). Finished results stay recorded;
  `retry` reruns the stage.
- **Failed parent turn** (non-zero exit, stall, deadline, cancel): live tasks
  are cancelled; the stage stops as today.
- `supports_wake` is computed per `(harness, provider, mode)`: Codex with
  `ollama`/`lmstudio` has no follow-up (`adapters.rs:884`); interactive
  parents need `Session::Assign` (see [Interactive parents](#interactive-parents)).

### Wait-only parents

Unattended parents on harnesses with MCP injection but no `follow_up` (omp,
Amp, Copilot) get `supports_wake: false`. They delegate with `mode: "wait"` and call
`task_wait` while `wait_timed_out` is true; their instructions say so
instead of "don't poll". A wait-only parent that exits 0 with live tasks
cancels them and stops `blocked` (`delegation: parent exited with live
tasks`).

### Interactive parents

- Same tools; the owner loop serves them while the harness runs in its
  terminal host on a worker thread.
- No automatic wake while the harness runs: a finished task updates the TUI
  task line. Interactive parents get the `[interactive]` instruction variant
  and its `next`: tell the user what you wait on; when they prompt you, read
  results with `task_status`.
- `Ctrl+B` only switches the TUI view; delegation keeps working.
- Release (TUI quit with "leave running", CLI `d`) closes the tools for good
  and cancels live tasks, which settle before the terminal host takes the
  run's locks; the TUI already asks before leaving a stage running. On
  reattach the bridge answers `parent_not_active`, since the token cannot be
  handed back without storing it.
- Waking an interactive parent needs a session ID assigned at launch
  (Claude Code, custom adapters with `session.assign`): an interactive Codex
  or OpenCode session runs in a PTY transcript with no readable session ID.
- The harness exits 0 with unread or running tasks: with a session to
  resume, the owner waits for the cohort, then resumes the session
  unattended through `follow_up`. The wake turn's exit, handoff, and repair
  are judged as unattended (the stage's mode is switched for the conclusion
  and restored); the stage keeps its interactive mode. Without one it cancels
  live tasks and stops `blocked`, and the `[interactive]` instructions say to
  read every result before exiting.

### Questions from tasks

- A task asks by returning verdict `blocked` with at least one `unresolved`
  item. If its harness has `follow_up` and a session id, the task becomes
  `waiting` with `questions`; otherwise it settles `blocked` with its
  `questions` and `has_follow_up: false`. Verdict `blocked` with no
  `unresolved` item settles `blocked`.
- A blocking wait returns at once; an async parent gets it in the next wake.
- The parent answers with `task_send` (resumes the task's session) or asks the
  user in its own turn.
- A task still `waiting` when the parent's final turn concludes settles as
  `blocked` (`unanswered question`).

### Follow-ups (`task_send`)

| Mode | Behaviour |
| --- | --- |
| `queue` (default) | After the task's current turn, resume its session with the message. On a finished or waiting task this starts a new turn now; published results stay. |
| `restart` | Stop the running turn's group, then resume the session with the message. |

- Needs `follow_up` and a session id on the task; otherwise
  `task_not_sendable`. No fresh-session fallback.
- No `steer`: one-shot harnesses accept no mid-turn input.
- A new turn makes the task undelivered again, so it re-enters the cohort;
  a worktree task's new turn produces a new sealed patch revision.
- Each independent review round is a new `delegate_task` with the original
  brief, prior findings, and objections.

### Cancellation

- `task_cancel` sets the task's cancel flag; its group gets SIGTERM, then
  SIGKILL after 5 s (`capture_with_stall_and_abort`).
  Terminal tasks return unchanged.
- Stage end, run cancel, `Ctrl+C`, deadline, and `serve` shutdown cancel every
  live task of the run.

## Isolation

| | `shared` | `worktree` |
| --- | --- | --- |
| Checkout | Parent's, read-only | Detached worktree from a snapshot of the parent's checkout, uncommitted edits included (`fanout::base_commit`, `add_worktree`, `core.hooksPath=/dev/null`) |
| Enforcement | Its own read-only `sandbox::Plan`, whatever the parent's mode, plus the target's native read-only setting when it has one; network as the parent's `sandbox_network` | Its own workspace-write `sandbox::Plan` rooted at the worktree; refused without a backend (`isolation_unavailable`) |
| Result | Handoff | Handoff, `changes`, sealed patch |
| Reaches parent | Never | Only through `task_apply` |
| Path | — | `.model-router/delegates/RUN_ID/STAGE_ID/aNNN-TASK_ID` |

- `shared` without a sandbox backend is refused (`isolation_unavailable`).
  There is no fallback: most harnesses have no native read-only setting
  (`builtins.rs`).
- Worktree tasks: Git's common directory is read-only except `objects/` and
  the task's own `worktrees/<name>/`, so a task can commit on its detached
  HEAD but cannot change refs, hooks, or config shared with the parent
  (`Plan::confine_shared_git` drops the common directory from the writable
  paths and adds it to `Plan::protected`). `objects/` stays writable: a task
  can add or delete objects, which a parent at write level `workspace` can
  already do.
- Git command policy for every Git command delegation triggers (task
  worktree creation, snapshots, sealed patches, `task_apply`, launched-run
  worktrees through `storage`): `-c core.hooksPath=/dev/null
  -c core.fsmonitor=false`, and diffs with `--no-ext-diff --no-textconv` (as
  `fanout::write_patch` already does). Clean/smudge filters come only from
  repository config, which no sandboxed stage or task can write once build
  step 0 lands. A test configures a hook, an fsmonitor command, and a filter
  from inside a sandboxed task and checks none runs.
- Router Git on a task worktree (snapshots, `tree_after`, the sealed patch)
  never discovers the repository from the working directory, since the task
  can rewrite its worktree's `.git` file and `worktrees/<name>/` files. It
  runs with `GIT_DIR=<parent's common dir>`, `GIT_WORK_TREE=<worktree>`, a
  router-private `GIT_INDEX_FILE`, and `-c core.fsmonitor=false
  -c core.hooksPath=/dev/null`. A test has a task rewrite `.git` and checks
  that no command runs.
- Sealed patch: after each task turn's group ends, the router writes
  `<task dir>/patches/patch-NNN.diff`, the full diff from the task's base
  snapshot (so each revision stands alone), and records its SHA-256 and file list
  in `TaskState.patches` in `run.json`. `task_apply` compares against the
  owner's in-memory `TaskState`, not the file on disk (an unsandboxed parent
  can rewrite `run.json` too). `protected_paths` runs on it then.
- `task_apply {task_id, revision}`: owner thread only; `revision` is
  required. Requires the parent at write level `workspace` and a writer
  parent role (`implementation`, `test`, `custom`). Re-hashes the patch
  against `TaskState.patches`, rejects any file that fails
  `fanout::is_plain_repo_path` (made `pub(crate)`; no `.git`, no
  `.model-router`) before `git apply`
  runs, re-checks `protected_paths`, then applies with
  `fanout::check`/`fanout::apply` (three-way, restore on conflict) on a
  scratch index. A conflict returns `patch_conflict` with files; nothing
  changes. One revision applies once; a retry after success returns the
  earlier result.
- Apply journal, per revision (`SealedPatch.apply`): before touching the
  checkout, the owner computes the expected post tree by running the same
  sequence `fanout::apply` will run (plain apply first, then `--3way`;
  `fanout.rs:194`) with `--cached` on a scratch index and `git write-tree`
  (none on Git older than 2.32) and persists `{pre_tree, expected_post_tree, started_at}`; on success
  it records `applied_at`. After a crash, `interrupt_tasks` compares the
  checkout's tree: equal to `pre_tree` → not applied; equal to
  `expected_post_tree` → applied; otherwise (or with no expected tree) →
  `apply_unknown`, reported in the stage output and the report. Applying a
  later revision leaves earlier revisions' records intact.
- Worktrees and snapshot refs are removed by `clean`
  (`fanout::remove_worktrees`, `history::remove` gains `delegates/`).

## Policy

### Privilege ceiling

Native permission settings do not compare across harnesses, so each stage and
task gets four values, compared separately.

**Write** (`read_only < worktree < workspace`):

| Condition | Write |
| --- | --- |
| router sandbox `read-only`, backend present | `read_only` |
| router sandbox `workspace-write`, backend present | `workspace` |
| no enforced sandbox, native read-only choice set, no `extra_args` | `read_only` |
| no enforced sandbox, otherwise | `workspace` |

A `worktree` task's write is `worktree` (its plan is always enforced).

**Native** (`restricted < default < escalated`): `escalated` with any
`extra_args` or a `harness_settings` choice in the spec's `escalating` list;
`restricted` with the native read-only choice; else `default`.

**Network**: `restricted` when the router sandbox is enforced with
`sandbox_network = false`, else `open`.

**Enforcement** (`enforced < none`): `enforced` when a bwrap or Seatbelt plan
applies; `none` for `sandbox = "off"`, an unsandboxed fallback, and router
checks.

Rules, checked at `delegate_task`, task follow-ups, `run_launch`, schedule
firing, and config load for named delegates:

- task write ≤ parent write, task native ≤ parent native, task network
  `open` only under an `open` parent, task enforcement `none` only under a
  `none` parent, task `sandbox_writable` ⊆ parent's. A sandboxed parent
  therefore cannot launch a workflow with unsandboxed stages or router
  checks.
- `shared` tasks are `read_only`.
- A named delegate that exceeds a stage that names it on any axis is a config
  error.
- Follow-up turns keep the task's first-turn settings. For Codex, settings
  pass through `FollowUpExtra::ConfigOnly` as config keys
  (`adapters.rs:662`); a setting that cannot be expressed there makes the
  task `task_not_sendable` rather than resuming with looser permissions.

### Limits

- Tasks get no bridge, no variables, and no delegation instructions.
- `max_tasks` counts `delegate_task` calls; each task takes at most 4
  `task_send` turns. `max_parallel` counts running task turns, not `waiting`
  tasks; excess turns (new tasks and `task_send` turns) stay `queued`.
  `max_wake_turns`, `max_runs`.
- Budget admission: `cost::check_budget_action` runs before every task turn,
  wake turn, and launched run, against recorded spend (including tasks and
  launched runs). Its `has_budget` early return also considers the
  launcher's caps for a launched run (`cost.rs:687`). It cannot stop a turn
  mid-way. `Stop` returns
  `budget_exceeded` for tool calls and stops the stage for wakes (see
  [Waking the parent](#waking-the-parent)).
- Usage: a parent attempt's `usage` is the sum of its turns (as handoff
  repair does today); `Turn.usage` is per-turn detail and never summed again.
  Task usage sums `TaskState.turns[].usage`, priced under the task's own
  harness/model/provider (`providers::route`, `cost::billing_for`), and counts
  in `cost::stage_totals`, `report`, and `cost` history.

### Idempotency

- `client_request_id` is optional and scopes to `(run, stage, attempt)`. With
  a key, a retry with the same payload returns the existing task, run, or
  schedule; a different payload is `request_conflict`. Without one, the
  router generates a key and the call creates new work.
- The key and a payload fingerprint are persisted before any side effect.

### Failures

```text
delegation_disabled        parent_not_active          capability_denied
target_unknown             harness_unavailable        provider_unavailable
model_unavailable          permission_escalation_denied
isolation_unavailable      task_limit_exceeded        apply_unknown
run_limit_exceeded         wake_unsupported
budget_exceeded            worktree_unavailable       task_not_found
task_not_cancellable       task_not_sendable          patch_conflict
protected_path             run_not_found              run_not_owned
workflow_not_launchable    schedule_not_found         request_conflict
invalid_request            orchestration_error
```

## Harness support

A parent needs per-launch MCP injection that edits no user or project config
file. Async wake also needs `follow_up` and a session id. Every harness can
be a leaf task.

| Harness | Parent injection | Wake | Status |
| --- | --- | --- | --- |
| claude | `--mcp-config <turn>/mcp.json`: `{"mcpServers": {"model-router": {"type": "stdio", "command": MR, "args": ["__mcp-bridge"], "env": {"MODEL_ROUTER_ORCH_SOCK": SOCK, "MODEL_ROUTER_ORCH_TOKEN_FILE": TOKEN}}}}`; `--allowedTools=mcp__model-router__*` (one argument, so it cannot take a positional prompt) | `follow_up` | v1 |
| codex | `--config mcp_servers.model-router.command="MR"`, `.args=["__mcp-bridge"]`, `.env.MODEL_ROUTER_ORCH_SOCK="…"`, `.env.MODEL_ROUTER_ORCH_TOKEN_FILE="…"`, `.tool_timeout_sec=120`; values TOML-quoted, since paths may hold spaces | `follow_up` (not `ollama`/`lmstudio`) | v1 |
| opencode | `OPENCODE_CONFIG_CONTENT` gains `mcp.model-router = {"type": "local", "command": [MR, "__mcp-bridge"], "environment": {…}, "enabled": true}` via `EnvValue::JsonInsert` | `follow_up` | v1 |
| custom | new `mcp_config_args` with `{mcp_config}` (the Claude-format file), plus the two variables; `follow_up_args` also accept `{mcp_config}`; `mcp_config_args` joins `CustomAdapter::launch_fields`, the trust digest, and `trust::ignore` | `follow_up_args` | v1; offline tests use it |
| omp | no MCP option in `omp --help` | — | not supported |
| amp | `--mcp-config` exists, but its JSON shape could not be confirmed offline | — | not supported |
| copilot | not installed where this was built; `--additional-mcp-config` unverified | — | not supported |
| pi | needs a router-shipped `--extension` (a TypeScript bridge) | — | not supported |
| cursor | CLI reads only `.cursor/mcp.json` | — | not supported: would edit files |
| gemini | settings files only; `streams = false` | — | not supported |
| goose | `--with-extension` takes a command string | — | not supported: argv invariant |
| aider | no MCP client | — | not supported |

- `HarnessSpec` gains `mcp: Option<McpInjection>` and `Part::Mcp`. Waking
  parents add `Part::Mcp` to their `FollowUp::args` template; `ConfigOnly`
  only filters `Part::Extra` (`adapters.rs:663`). `Part::Mcp` renders
  nothing without a broker, so handoff repair and non-delegating stages are
  unchanged.
- Tests run the bridge through a `use_program` hook like the terminal host's
  (`host.rs:428`), so integration tests never exec the test binary.
- `serve::mcp::tool_result` adds `structuredContent` to error results
  (`mcp.rs:425`). `KnownKeys` learns `delegation`, `[delegates.*]`, and
  `mcp_config_args`.
- `doctor::flag_uses` gains a delegation-enabled invocation per parent so the
  new flags are probed (`every_flag_a_spec_declares_is_probed`).
- Instructions go in the prompt (`engine::prompt::prompt_for`) for every
  harness; no `--append-system-prompt`.
- With delegation off, `adapters::build` and `follow_up` output is
  byte-identical: no existing golden file changes. New golden cases cover each
  v1 parent with delegation on, launch and follow-up, with `MR`, socket, and
  token-file paths normalized.
- Native output schema stays on (`--output-schema`, `--json-schema`); both
  follow-up templates pass `Part::Schema`, so the final turn is
  schema-checked.
- Before Codex ships as a parent, `docs/adapters.md` records, from
  `codex exec --help` and config docs, whether `codex exec` runs MCP tools
  without approval under each sandbox setting, and whether Code Mode yields
  long tool calls (if it does, `mode: "wait"` is capped at 25 s for Codex).

## Records

```text
Run
  delegates: BTreeMap<String, Delegate>      # snapshot, in the approval digest
  launched_by: Option<LaunchedBy>            # {run, stage, attempt}
  creation_source: "user" | "agent" | "schedule"
Attempt
  turns: Vec<Turn>          # wake turns (2..n): dir, session, usage, exit, handoff
  tasks: Vec<TaskState>     # every task this attempt started
TaskState
  id, client_request_id, fingerprint, title, role, target: ResolvedTarget,
  isolation, ceiling, status,
  turns: Vec<TaskTurn>      # one per task turn
  handoff, questions, changes,
  patches: Vec<SealedPatch {revision, turn, path, sha256, files, added, removed,
                            apply: Option<ApplyJournal>}>,
  error, delivered: bool, cancelled_by_parent: bool, created_at
TaskTurn                    # the Attempt fields a process needs, no tasks or turns
  number, started_at, finished_at, model, status, exit_code, directory,
  handoff_file, error, usage, session, process_group, process_identity,
  tree_before, tree_after, worktree
```

All new fields are `#[serde(default, skip_serializing_if = …)]`; old runs
load unchanged.

- Paths: turn 1's set-aside handoff and wake turns in
  `<attempt>/turns/turn-NNN/`; task turns in
  `<NN-stage>/attempt-NNN.tasks/<task>/turn-NNN/`; worktrees in
  `delegates/RUN_ID/STAGE_ID/aNNN-TASK_ID`.
- `store::recorded_paths` and `load` validate every turn and task path, each
  task worktree by exact layout, and sealed patch paths; `inside()` and
  `history::{artifacts, remove}` accept `delegates/<id>`.
- `recovery::stop_orphans` walks parent turns and task turns;
  `interrupt_tasks` beside `interrupt_candidates` marks live tasks
  `interrupted` after a crash.
- `transitions.jsonl` gains `Level::Task` and `Actor::Agent`.
- `store::load` validates `TaskTurn` and `ApplyJournal` trees as Git object
  names, like attempt trees (`store.rs:101`). `history::rerun` and `fork`
  copy `delegates` from the source run.

## Surfaces

- TUI: a line under the stage strip lists each task's target and status, and
  wake turns; task output appears in the feed under
  `── Implement · task t3 (codex-review) ──`; `C` cancels the stage and its
  tasks; runs the agent launches join the session in the background.
  Schedules are approved from the CLI (`mr schedules approve ID`).
- `report` / `report --json`: `tasks` per stage, `launched_runs`.
- `serve`: `run_status` task counts; `get_handoff` includes task handoffs.
- `preview`: each parent's injection and every named delegate's command.
- `history`: task counts and agent/schedule provenance.
- `mr schedules list|approve|pause|delete|tick`.

## Instructions

Prepended to a delegating parent's prompt (`engine::prompt::prompt_for`).
Under 4,000 bytes. Exactly one of the `[wake]`, `[no wake]`, and
`[interactive]` lines is kept, by parent kind. Tasks never get this text.

```markdown
## model-router delegation

Ordinary work needs no delegation.

- Delegate work that gains from another model or from running in parallel:
  a second-opinion review, an independent investigation, an isolated
  implementation. Fix small things yourself. Tasks cannot delegate further.
- Use your harness's own subagents only for work on your own model and
  permissions. Use `delegate_task` for another harness, model, or effort, or
  for an isolated worktree.
- Call `orchestrator_capabilities` once before your first `delegate_task`, and
  again only after an error. Use only listed targets. If one you need is
  missing or fails, say so; never substitute another, and never start agent
  CLIs or git worktrees yourself.
- A task sees only your `task` text. Write a full brief: goal, files,
  constraints, and what to return. No secrets.
- `shared` tasks are read-only; keep the files they read stable. `worktree`
  tasks change their own checkout; read their `changes`, then `task_apply`
  what you accept.
- Give each call its own `client_request_id`; reuse it only to retry that call.
- [wake] Follow a task's `next`: keep working, then end your turn. The router
  resumes this session with the results. Don't poll `task_status`, sleep, or
  loop; the router resumes you to read the results with `task_status`. Until
  you have read every task's result, end your turn with verdict `blocked` and
  summary `waiting on tasks`; write your real handoff only after reading them
  all.
- [no wake] This session cannot be resumed: use `mode: "wait"` and call
  `task_wait` again while `wait_timed_out` is true, until your tasks finish.
  Read every result before you write your handoff or exit.
- [interactive] Tasks finish while you talk with the user. Tell the user what
  you are waiting on; when they prompt you, read results with `task_status`.
  Read every result before you write your handoff. {If you exit with tasks
  running, the router resumes you to read them. | Unread or running tasks
  when you exit stop the stage.}
- A task that asks questions waits for `task_send` with your answer; or ask
  the user.
- Each new review round is a new `delegate_task` with the brief, prior
  findings, and objections. `task_cancel` tasks you no longer need.
- `run_launch` and `schedule_task` only when the user asks for a new run or a
  schedule.
```

## Build order

Built in this order, each step with its tests: (0) the Git sandbox fix this
design depends on (`sandbox::git_locks`, `sandbox::refuse_writable_git_config`,
`snapshot::git_command`); (1) records, config, trust, ceiling, cost; (2–6)
owner loop, broker and bridge, tools, wake loop, Claude/Codex/OpenCode/custom
parents, worktree tasks and `task_apply`, `task_send`; (7) run tools and the
`RunHost` (`engine::set_run_host`, registered by `serve` and the TUI); (8)
schedules; (9) interactive parents; (10) report, `serve`, preview, history,
TUI task line, `scripts/smoke_delegation.py`, docs.

## Tests

All offline, against `examples/mock_harness.py`. Its `--script` mode starts
`mr __mcp-bridge` from `--mcp-config` and replays a JSON list of tool calls
per turn; model `ask` makes a task ask a question on its first turn, `slow`
keeps it running.

- `tests/delegation.rs` (`tests/common/mod.rs` temp dirs,
  `MODEL_ROUTER_CONFIG_DIR` set): a shared task wakes the parent once and its
  result is read, with `delegate_task` retry and `request_conflict`; a
  question answered with `task_send`, a retried send queued once, a changed
  send refused; a read but unanswered question settles `blocked`; a wake that
  ends without reading its results stops `blocked`; a parent with no
  `follow_up` waits in its tool call; a cancelled task needs no wake; a
  worktree task's patch reaches the checkout only through `task_apply`
  (applied, already applied, stale revision); unlisted targets and unknown
  tasks refused; launched runs get their own worktree, retry by key, and
  answer to their launcher; a schedule fires only after approval and once per
  occurrence, with retry and `request_conflict`; an interactive parent that
  exits with tasks running is resumed unattended.
- Unit tests: delegation config validation (`src/delegation/config.rs`), the
  ceiling axes and extra writable paths (`policy.rs`), schedule triggers and
  receipt ids (`schedules.rs`), and `sandbox` plans: Git locks, symlinked Git
  config refused, a worktree task's shared Git directory read-only.
- `tests/sandbox.rs`: a `workspace-write` stage cannot make the router run
  Git commands.
- `tests/adapter_golden.rs`: the delegation argv for each supported parent
  (`tests/fixtures/adapter-golden/delegation.json`).
- `scripts/smoke_delegation.py`: the Runs page task line, and cancelling the
  stage cancels its task. Skipped without bubblewrap or `sandbox-exec`.

## Decided

1. Enabling delegation on a Claude Code stage passes
   `--allowedTools=mcp__model-router__*` so it can call the tools unattended.
2. omp, Amp, Copilot, and Pi are not delegating parents (see
   [Harness support](#harness-support)); every harness can run tasks.
