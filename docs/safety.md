# Safety model

- The router runs harnesses with **your** logins; it stores no credentials.
- It never commits, pushes, or opens PRs, except an enabled
  [ship stage](workflows.md#shipping) after you confirm its preview.
- It never enables a permission bypass by default.
- A cloned repository's config cannot run commands or redirect keys until you
  [trust](#project-trust) it.
- An OS [sandbox](#sandboxing) around harnesses is available, off by default.

## Project trust

A project's `.model-router.toml` could run commands or send keys elsewhere.
These sections take effect only after you trust the file:

- custom adapters (`[adapters.*]`, including spec fields that reach the
  executable; `follow_up_args` enters the trust digest only when set);
- stage `checks`, `extra_args`, and workflow or stage lifecycle `hooks`
  (`before_stage`, `after_stage`, `before_ship`, `on_failure`);
- permission settings that loosen a harness (`danger-full-access`,
  `bypassPermissions`, `yolo`, Cursor sandbox `disabled`, Aider `yes-always`,
  Goose `auto`, Copilot `allow-all-tools`);
- router sandbox loosening: a stage `sandbox` weaker than the workflow
  default would give it, `sandbox_writable` paths, `sandbox_fallback = "warn"`;
- ship stages with `confirm = false` (while untrusted, ship stages ask first),
  `secret_scan = false` (forced on while untrusted), or a `secret_scanner`
  command;
- providers with their own `base_url`, or a key variable other than the preset's.

```sh
model-router trust          # list the sections and confirm
model-router trust --yes    # trust without asking
model-router trust --revoke
```

Trust is a SHA-256 digest of those sections per project in
`<config dir>/trust.json`; any change asks again.

| Entry point | Untrusted config |
| --- | --- |
| `run` | Refused; lists what it would run. |
| TUI | Asks once: trust, continue with them ignored (saving keeps them in the file), or quit. |
| `serve` | Refused for that project until `model-router -C DIR trust`. |
| Other commands | Ignore the sections and say so. |

Your global config and a `--config` file are always trusted.

Saved runs are checked against the commands and permission policies they will
actually execute before retry, rerun, or interactive recovery; a harness that
outlived its `mr` can still be reattached, stopped, or left to end, but its
stage's checks run only while the run is approved. New runs keep
an approval digest outside the workspace in `<config dir>/run-approvals/`,
including runs created from global or explicit configs. Changing saved commands
or effective sandbox, network, or native permission settings invalidates that
approval, and a config cannot reapprove a run whose digest no longer matches;
to replay such a run anyway, review its run.json, run `trust --revoke`, and
trust the matching config again. An update to model-router that changes how
settings are assessed (for example, which harness settings escalate) can
also stop older approvals from matching; they recover the same way.
A saved run must also keep the workspace, run directory, and candidate
worktree paths that follow from its project and ID, attempt and candidate
patch paths inside that run's own directories (with no link inside them
leading out),
valid stage and
fan-out candidate IDs (letters, digits, `-` and `_`), only the candidate IDs
its workflow gives each stage, and Git object names as its snapshots. A running attempt's recorded terminal is reattached to only when it
is a socket in the terminal host's private socket directory; otherwise the
attempt counts as interrupted. A recorded or reported harness session ID
that could pass as an option is never given to a harness. A run recorded
through a link to its project is respelled to the project as it resolves now,
so the link is never followed again. `trust --revoke` also removes the project's saved-run
approvals. Older runs, or runs without an approval digest, can replay only when
their executable settings and policies are still present in the current trusted
config. To reapprove one, restore and trust the matching config, or explicitly
select a matching user-owned file with `--config`. That file is read afresh;
cached TUI settings cannot undo a revocation. A currently loaded global config
remains trusted without a project approval.

## Harness permissions

Each harness enforces its own permissions. The router inherits them unless a
stage sets `harness_settings` or extra arguments; see
[harnesses.md](harnesses.md#permission-settings). Stage instructions such as
"do not modify code" are intent, not enforcement.

## Sandboxing

An optional OS-level filesystem sandbox around any harness.

```toml
[[workflows]]
sandbox_default = "by-role"     # off | read-only | workspace-write | by-role

[[workflows.stages]]
id = "review"
sandbox = "read-only"           # unset inherits sandbox_default
sandbox_writable = ["~/.npm"]   # extra writable paths
sandbox_network = false         # default true: harnesses need their APIs
sandbox_fallback = "warn"       # default "refuse"
```

| Mode | Writable |
| --- | --- |
| `read-only` | Attempt directory, run's handoff directory, a private `/tmp`, harness state (`~/.<harness>`, `~/.config/<harness>`, `~/.local/share/<harness>`, `~/.cache`, `~/.local/state`, `~/.claude.json` for Claude), `sandbox_writable`. Everything is readable. |
| `workspace-write` | The above plus the workspace and, for linked worktrees, their Git directories, except the workspace's `.model-router` directory (run records, other runs), which stays read-only apart from the attempt and handoff directories; a stage refuses to start when that directory is a symbolic link, which it could replace. |
| `by-role` | `read-only` for research, planning, review, summary; `workspace-write` otherwise. |

In every mode a stage can write inside its attempt and handoff directories but
cannot remove or replace them, and a stage refuses to start when either is a
symbolic link: later stages are granted the same directories. The router's own
files there, such as check logs and candidate patches, replace whatever the
stage left at their names rather than writing through a link. This assumes
nothing from the stage still runs: a process it detached into its own session
outlives the harness (the sandbox has no process namespace), keeps the stage's
write access, and could race those writes.

In every mode a sandboxed stage cannot change the workspace's Git config, its
`config.worktree`, or its hooks directory (created when missing), nor, in a
linked worktree, its `.git` file, `commondir`, or `gitdir`. Under
`workspace-write` the `.git` directory stays writable but cannot be renamed
or replaced, and a stage refuses to start while its Git config,
`config.worktree`, or hooks entry is a symbolic link it could replace. The
router runs Git in the workspace outside the sandbox after
each stage, and Git runs commands its config and hooks name (fsmonitor,
filters, diff drivers). A stage refuses to start when a Git config file the
router reads, or the target of an `include.path` or `includeIf.*.path`,
lies in a path the stage may write. The router's own Git commands also pass
`-c core.fsmonitor=false -c core.hooksPath=/dev/null`. Operations that inspect
working-tree attributes, stage files, diff, apply patches, populate worktrees,
or launch `git difftool` run against a fixed, private sanitized `GIT_DIR`
config (requiring Git 2.26+) that excludes repo-local filters, diff/merge
drivers, and difftools so concurrent `.git/config` changes cannot inject
commands; it keeps repo-local Git LFS endpoint URLs and remote selection
(`lfs.url`, `remote.*.url`, `remote.*.lfsurl`, `remote.lfsdefault`,
`remote.lfspushdefault`, `branch.*.remote`). Router `git status` calls pass
`--ignore-submodules=dirty`, so Git never runs inside a submodule with that
submodule's own config; worktree mode checks each populated submodule with
its own sanitized Git directory instead. New
worktrees are checked for disabled smudge filters from the new worktree's own
config, and a fan-out patch apply is refused when a patched file uses one. That `GIT_DIR` lives in a `tmp` directory inside the router's
socket directory, which stages cannot write. Partial clones
(`extensions.partialClone` or `remote.*.promisor`) and a repository-local
`lfs.storage` are refused; use a full clone and set `lfs.storage` globally.
Worktree creation and shipping fail closed when affected files rely
on suppressed repo-local filters, and workflows with `protected_paths` or
blocking `policies` fail before launch when the pre-stage snapshot cannot be
captured.

A stage cannot reach a terminal host (see below), whose harness runs outside
the sandbox, through the usual socket directories: `$XDG_RUNTIME_DIR/model-router`,
`/run/user/<uid>/model-router`, `/tmp/model-router-<uid>`, and the temp
directory's `model-router-<uid>`. bubblewrap hides them; Seatbelt denies
connecting to or writing in them, and renaming the directories above them.
An `mr` that ran with another `TMPDIR` or a custom `XDG_RUNTIME_DIR` put its
sockets elsewhere, which stay reachable, as does a socket directory first
created after the stage started.

- Linux: [bubblewrap](https://github.com/containers/bubblewrap) (`bwrap`).
  macOS: `sandbox-exec` with a generated profile.
- No working backend: the stage fails before launch unless
  `sandbox_fallback = "warn"`.
- Interactive stages keep their terminal.
- `doctor` reports the backend; `preview` and the stage details show each
  stage's mode; `command.json` records backend and writable paths.
- Limits: harness state directories stay writable, so a harness can change its
  own settings. A project config replaces your global one, so set the sandbox
  there too.

## Delegation

A delegating stage's agent reaches the router only through its own turn's
broker ([delegation.md](delegation.md)).

- Each parent turn gets a new Unix socket and a 32-byte token file in a
  private 0700 directory (`model-router-orch` beside the terminal host's).
  The harness's `model-router __mcp-bridge` presents the token; one live
  connection per turn; both files are removed when the turn ends. The
  environment, argv, `mcp.json`, and previews hold only their paths.
- Every sandboxed stage has every orchestration directory hidden (bubblewrap
  tmpfs; Seatbelt denies reads, writes, and connects). A delegating turn gets
  back only its own socket and token file, after every deny. Other launches
  drop inherited `MODEL_ROUTER_ORCH_*` variables.
- Tasks always run in their own OS sandbox (`shared`: read-only; `worktree`:
  writable inside its worktree, Git's shared refs, hooks, and config
  read-only; `objects/` and its own `worktrees/<name>/` stay writable) and
  are refused when no backend works. A task, launched run, or schedule never
  exceeds its creator on write access, native permissions (`extra_args` or an
  escalating setting count as escalated), network, sandbox enforcement, or
  `sandbox_writable` paths outside the creator's.
- A worktree task's patch is sealed by the router after the task ends,
  hashed in `run.json`, re-checked against `protected_paths` and for `.git`
  or `.model-router` paths, and applied only by `task_apply`, with a journal
  that crash recovery settles.
- `delegation` and `[delegates.*]` need project trust and enter the run's
  approval digest; untrusted, they are ignored.
- Runs an agent launches get their own worktree, may not exceed the
  launcher's ceiling (router checks and a secret scanner count as unsandboxed
  commands), launch no runs or schedules, and count toward the launcher's
  budget. A ship stage in a run an agent or schedule created always asks for
  confirmation.
- Schedules fire only with an approval receipt in
  `<config dir>/schedule-approvals`, bound to everything the schedule would
  run; any edit, pause, or enable forgets it, and `trust --revoke` forgets
  them all. Sandboxed stages cannot write `<config dir>`.

## Secrets and redaction

- Provider keys are read from the variable named by `api_key_env` at launch;
  a run fails before starting if it is unset.
- `command.json` records variable names only; previews show `NAME=<redacted>`
  for every environment value.
- `base_url` with credentials, a query, or a fragment is rejected.
- Codex and OpenCode get injected providers under `model-router-NAME`, so no
  native key or login leaks into them; Aider and Goose launches drop
  inherited `OPENAI_*` variables.
- Task text, extra arguments, prompts, and logs are saved in run artifacts
  under `.model-router/` (git-ignored). Treat them as sensitive as your task.
- Unattended progress output strips terminal controls, including plain text
  and malformed events. Raw `stdout.log` is preserved; interactive PTY output
  remains terminal passthrough. Treat raw logs as untrusted terminal input.

## Task imports

GitHub URLs go through `gh` and its login; imports read only the named file,
issue, or PR. Other URLs are fetched without credentials. HTTPS may not
redirect to HTTP, and neither redirects nor DNS answers may reach loopback,
link-local (`169.254.0.0/16`, `fe80::/10`), or private (RFC 1918, `fc00::/7`)
addresses, unless the URL you typed has such an IP address or `localhost`
(or a `*.localhost` name) as its host. A public name that resolves to a
private address is refused.

## Process cleanup

- Each harness and router-run check runs in its own process group. When it
  ends, normally or not, the group gets SIGTERM, then SIGKILL after five
  seconds. Dev servers and watchers do not outlive the stage.
- On Linux, an unattended harness gets SIGTERM if `mr` dies. An interactive
  one lives in its terminal host, so it survives a crashed `mr`; the next `mr`
  reattaches to it. If the host itself dies, its closed PTY hangs the harness
  up, and the next `mr` stops whatever is left.
- Each attempt records its active harness or check process group (parallel
  workers first journal it in the run directory, so a crash while the
  scheduler is busy still leaves it); after a crash, the next `resume` stops
  any surviving group first. Discarding a fan-out
  round for a full retry retains its groups until recovery stops them.
- A process group ID can name an unrelated group after a reboot or a long
  gap, so each record also keeps the leader's start time (Linux: boot ID and
  `/proc/<pid>/stat` start ticks; macOS: `proc_pidinfo` start time) and a
  random marker every process in the group inherits as `MODEL_ROUTER_GROUP`.
  Recovery stops a live group only if its leader has the recorded start time
  or, once the leader has exited, a process in it carries the marker
  (`/proc/<pid>/environ`; on macOS `KERN_PROCARGS2`, which hides the
  environment of system binaries such as `/bin/sleep`). The check runs again
  just before SIGTERM and before SIGKILL, against the record, so a group
  that ends during the grace period and whose ID passes to another group is
  not killed. A narrow race remains between each check and its signal.
  An interactive harness's start time is read by its terminal host, which
  is its parent, before the host can reap it.
- A group that fails the check, and any group recorded by an earlier
  version, is left running and stays recorded in `run.json`
  (`retained_groups`): it may still be the run's harness editing the
  workspace. So does a group whose leader exits on SIGTERM leaving a member
  that proves nothing, as SIGKILL is skipped then. While one is alive the
  run launches nothing (`resume`, retry, advance, TUI continue, `fork`), and
  no new run or rerun starts in the same checkout (runs in their own
  worktree may); the error names each group, why it could not be
  identified, and `kill -TERM -- -PGID`. Once the group has exited the
  record is dropped. `resume ID --release-groups` drops the records without
  signalling, for a group you have checked belongs to something else.
- Quitting the TUI (`q`) stops active runs. A running interactive stage stops
  too unless you choose to leave it running: its harness lives in a terminal
  host process (own session, private socket under `$XDG_RUNTIME_DIR` or the
  temp directory, mode 0700) that holds the run and workspace locks until an
  `mr` reattaches or the harness exits. The locks pass between `mr` and the
  host as open file descriptors over the socket, so no other run can take the
  checkout in between. If an attached `mr` dies, though, the host takes the
  locks back by path, and another run can take the checkout at that moment.
  Recovery skips its process group.
- Runs are serialized per checkout by `workspace.lock`; run state writes are
  atomic.

## Server

`serve --http` binds loopback only unless `--allow-remote`, needs a bearer
token, and only accepts projects under `--allow-root` paths. Server runs are
unattended only and cannot confirm ship stages. See [serve.md](serve.md#safety).
HTTP keeps connections that have not yet sent authenticated headers (at most
16) apart from authenticated ones (at most 32, of which at most 24 may be event
streams or `wait_run` calls so quick calls such as `cancel_run` and `run_status`
stay runnable): when
the pending queue is full, the oldest pending connection is cut off once it has
waited half a second (newer arrivals wait in the listen backlog until then), so
idle sockets cannot lock out a client that sends its headers promptly, and a
burst of clients is not cut off before its headers are read. A connection
leaves the pending queue as soon as its headers authenticate, so a body still
on the way is never cut off. Request headers are capped at 64 KiB (enforced
while reading each line) and bodies at 1 MiB, sharing a 10-second absolute read
deadline; authorization is checked before waiting for a request body.

MCP over stdio (`serve --mcp`) caps request lines at 2 MiB and concurrent
`tools/call` workers at 32, of which at most 24 may be blocking `wait_run`
calls.

## Protected paths and secret scanning

- Workflow `protected_paths` lists repository-relative globs (`*`, `**`, `?`).
  It is enforced for ordinary stages (the stage stops **blocked**), for fan-out
  candidates (the candidate stops **blocked** with its worktree retained), and
  again at candidate selection; `ship` and `apply` also refuse matching files.
- `ship` runs a built-in diff secret scanner (`secret_scan = true` by default)
  at plan time and again right before `git commit-tree`. It reads the full diff
  of the planned files only (no size cutoff; files Git treats as binary are
  read as text), checks added lines for private key blocks and
  AWS/GitHub/Anthropic/OpenAI/Slack/Stripe/Google tokens, lists at most 20 hits
  followed by `and N more`, and never prints secret values.
- Set `secret_scanner` (argument vector, no shell, e.g.
  `["gitleaks", "protect", "--staged"]`) to also run an external scanner in
  both phases. It runs with `cwd` = workspace, `{files}` and
  `MODEL_ROUTER_SHIP_FILES` = planned paths, and `GIT_INDEX_FILE` = a private
  index containing `HEAD` plus the planned files as they are now, so `--staged`
  scanners see exactly what will be committed, in its own process group with
  `GIT_CONFIG_PARAMETERS` disabling hooks and fsmonitor. Your own index is
  never read or written; planning writes blob objects to Git's object database.
  Requires project trust and fails closed if missing or non-zero.

## Not enforced

Some guarantees depend on the harness or OS rather than the router itself:

| Area | What is not enforced by the router |
| --- | --- |
| Stage instructions | Prompt lines such as "do not modify code" are intent only; use `sandbox = "read-only"` (or `sandbox_default = "by-role"`) for OS enforcement. |
| Native harness permissions | Passing `--sandbox`, `--permission-mode`, etc. relies on the harness honoring its own flags. |
| Harness state directories | Under router sandboxing, harness config/cache directories stay writable so the CLI can run; a sandboxed harness could modify its own config. |
| Detached processes | The router sandbox has no PID namespace. A process a harness detaches into a new session outlives the stage's process group and keeps its sandbox permissions. |
| Custom `TMPDIR` / `XDG_RUNTIME_DIR` | Terminal-host socket hiding covers standard runtime/temp socket paths; a concurrent `mr` launched with a custom `TMPDIR` or `XDG_RUNTIME_DIR` puts its sockets elsewhere. |
| Built-in secret scan | Pattern matching catches high-confidence token formats only; pair with an external `secret_scanner` (e.g. `gitleaks`) when broader coverage is required. |
| Unsandboxed delegating stages | With `sandbox = "off"`, any process of your user can read a running turn's token file and reach its broker, as it could anything else you run. |
| Non-streaming or unpriced spend caps | Unattended streaming harnesses (`codex-jsonl`, `claude-stream`, `pi`, `opencode`, `amp-stream`, `cursor-stream`) abort mid-stream on `on_exceed = "stop"` as soon as streamed usage exceeds a USD or token cap; interactive stages and harnesses that report usage only at exit check after the attempt completes. |
