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

- custom adapters (`[adapters.*]`, including spec fields that reach the executable);
- stage `checks` and `extra_args`;
- permission settings that loosen a harness (`danger-full-access`,
  `bypassPermissions`, `yolo`, Cursor sandbox `disabled`, Aider `yes-always`,
  Goose `auto`, Copilot `allow-all-tools`);
- router sandbox loosening: a stage `sandbox` weaker than the workflow
  default would give it, `sandbox_writable` paths, `sandbox_fallback = "warn"`;
- ship stages with `confirm = false` (while untrusted, ship stages ask first);
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
actually execute before retry, rerun, or interactive recovery. New runs keep
an approval digest outside the workspace in `<config dir>/run-approvals/`,
including runs created from global or explicit configs. Changing saved commands
or effective sandbox, network, or native permission settings invalidates that
approval. `trust --revoke` also removes the project's saved-run
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
| `workspace-write` | The above plus the workspace and, for linked worktrees, their Git directories. |
| `by-role` | `read-only` for research, planning, review, summary; `workspace-write` otherwise. |

- Linux: [bubblewrap](https://github.com/containers/bubblewrap) (`bwrap`).
  macOS: `sandbox-exec` with a generated profile.
- No working backend: the stage fails before launch unless
  `sandbox_fallback = "warn"`.
- Interactive stages keep their terminal.
- `doctor` reports the backend; `preview` and the stage panel show each
  stage's mode; `command.json` records backend and writable paths.
- Limits: harness state directories stay writable, so a harness can change its
  own settings. A project config replaces your global one, so set the sandbox
  there too.

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
addresses, unless the URL you typed names such a host.

## Process cleanup

- Each harness and router-run check runs in its own process group. When it
  ends, normally or not, the group gets SIGTERM, then SIGKILL after five
  seconds. Dev servers and watchers do not outlive the stage.
- On Linux, an unattended harness gets SIGTERM if `mr` dies. An interactive
  one lives in its terminal host, so it survives a crashed `mr`; the next `mr`
  reattaches to it. If the host itself dies, its closed PTY hangs the harness
  up, and the next `mr` stops whatever is left.
- Each attempt records its active harness or check process group; after a
  crash, the next `resume` stops any surviving group first. Discarding a fan-out
  round for a full retry retains its groups until recovery stops them.
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
16) apart from authenticated ones (at most 32, including event streams and
requests still sending their body): when
the pending queue is full, the oldest pending connection is cut off once it has
waited half a second (newer arrivals wait in the listen backlog until then), so
idle sockets cannot lock out a client that sends its headers promptly, and a
burst of clients is not cut off before its headers are read. A connection
leaves the pending queue as soon as its headers authenticate, so a body still
on the way is never cut off. Request headers and bodies share a 10-second
absolute read deadline; authorization is checked before waiting for a request
body.
