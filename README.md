# Model Router

A native Rust TUI for running a task through your preferred agentic harnesses
and models. Choose the sequence, the model for each stage, and whether each
stage runs unattended or opens its native interactive terminal.

The starter workflow is:

```text
Codex / gpt-6-astra          Claude Code / sonnet          Optional review
Investigate, unattended  →  Implement, interactive  →     Disabled initially
```

Every stage is editable, movable, repeatable, and optional. Review is never
required. The Astra ID is taken from the local account catalog used during this
project's design; select another ID or leave it blank if your account differs.

## Start

Requires Rust 1.85+ to build. Install and authenticate the harnesses you want to
use separately. The router reuses their credentials and provider configuration.

```sh
cargo run --release
```

Or install a local executable:

```sh
cargo install --path . --locked
model-router -C /path/to/project
```

The TUI opens with a 1.7-second animation: a signal branches through model nodes
and converges into a route. Press any key to skip, or `Ctrl+C` to exit. The skip
key is consumed so it cannot trigger an action in the editor. The animation
adapts to terminal resizing and only plays once when the TUI starts.

Use `model-router --no-animation` to open the editor immediately. Set
`MODEL_ROUTER_NO_ANIMATION=1` in your shell to always bypass the intro. CLI
subcommands bypass it automatically; `NO_COLOR` suppresses its colors.

Opening the router does not start a model. Press `p` to choose a repository,
`t` to describe the task or `f` to import a task/spec,
`e` to edit a stage, `m` to choose a model, and `r` to start. `Space` enables
or disables a stage. `?` opens the full keyboard guide. Use `Ctrl+S` to apply
text/form edits; `Esc` cancels them. `q` saves workflow edits and exits.

## What works

- Named workflow templates and per-run snapshots.
- Explicit harness/model/provider/effort selection and extra CLI arguments.
- Optional research, planning, implementation, review, testing, and custom stages.
- Native interactive terminals, including paste, resize, and a saved transcript.
- Unattended execution with live output, cancellation, and timeouts.
- Structured handoffs plus the original task carried between stages.
- Optional review-driven repairs, with two automatic repair passes by default.
- Saved runs, explicit retry, model replacement, and resume.
- Interactive repository browser, recent projects, and optional Git worktrees.
- Editable task/spec imports from local files, text URLs, and GitHub issues/files.
- CLI execution and previews using the same engine as the TUI.

## Select a repository

Press `p` on the Workflow page. The browser starts beside your current project,
with Git repositories listed before other folders. Type to filter names, then
use `↑`/`↓` and `Enter` to choose a repo or open a folder.

| Key | Action |
| --- | --- |
| `→` | Browse inside the highlighted folder or repo |
| `←` | Go to the parent folder |
| `Tab` | Switch between folders and recent projects |
| `Ctrl+L` | Enter or paste a directory path; `Enter` opens it |
| `Esc` | Cancel path entry, or close the browser |

Paths can be absolute, relative to the displayed folder, or start with `~/`.
Choose **Use this folder** to select the displayed directory, including a
project without Git. Linked worktrees and submodules are recognized as repos.
The browser lists local folders one level at a time; it does not scan your
entire disk or clone remote repositories.

Switching saves edits to the current workflow, loads the selected project's
configuration, and resets the task and run view. Saved runs remain on disk.
Recent projects persist across launches. Stop an active run before switching.

## Import a task/spec

Press `f` on the Workflow page to browse local Markdown or UTF-8 text files.
The browser starts in the selected project. Type to filter, use `↑`/`↓` to
select, and press `Enter` to open a folder or preview a file. `←` opens the
parent folder; `Ctrl+L` accepts an absolute path, a path relative to the displayed
folder, or a `~/` path.

Press `Ctrl+U` in the browser to paste a URL instead:

- Direct HTTP(S) links to Markdown or plain text.
- GitHub issue URLs, importing the title, body, and source link.
- GitHub file links (`blob`, `raw`, or `raw.githubusercontent.com`), including
  branches with slashes, tags, and commit references.

GitHub links use the GitHub CLI (`gh`) and its existing login, including private
repositories. Run `gh auth login` if needed. Imports only read the selected
file or issue; issue comments are not included. Other text URLs are fetched
without GitHub credentials. Export HTML pages, PDFs, or Word documents to
Markdown/plain text first.

The preview is editable. `Tab` switches between **Append to task** and
**Replace task**, preserving edits in each draft. An existing task defaults to
Append; an empty task defaults to Replace. Append inserts a Markdown separator
between the existing task and imported text. `Ctrl+S` applies the selected
draft. `Esc` goes back or cancels a pending download; cancellation and failed
imports preserve the existing task. Press `r` separately to start the workflow.

The complete task must fit within **32 KiB of UTF-8 text**. Oversized files or
combined drafts show an error; text is never silently truncated. Imports remove
a UTF-8 byte-order mark and normalize line endings. The source remains
unchanged. Starting a run saves a copy of the task for every stage and resume,
so later source edits or deletion cannot change that run.

CLI imports start the configured workflow directly:

```sh
model-router run --task-file docs/spec.md
model-router run --task-url 'https://github.com/owner/repo/issues/123'
model-router run --task-url 'https://github.com/owner/repo/blob/main/docs/spec.md'
```

Choose one of `--task`, `--task-file`, or `--task-url`. Relative CLI file paths
resolve from the shell's working directory. Use the TUI to preview and edit
before starting a run.

## Harness and model support

| Harness | Selection | Integration |
| --- | --- | --- |
| Codex | Native model ID, optional provider and reasoning effort | `exec` JSON events; native interactive CLI; final response file |
| Claude Code | Alias or full model ID; optional effort | Print-mode JSON; native interactive CLI |
| Pi | Provider and exact model ID; optional thinking | Print-mode JSON events; native interactive CLI |
| Cursor Agent | Cursor model slug | Print-mode JSON; native interactive CLI |
| Gemini CLI | Gemini model ID | Headless JSON; interactive prompt |
| OpenCode | `provider/model`; optional variant | Run-mode JSON; interactive run |
| Aider | Native/LiteLLM model ID | Message file; interactive command file; automatic commits disabled |
| Goose | Provider and model | `run`; optional interactive continuation |
| Amp | Mode key, such as `high` | Execute-mode JSON; native interactive CLI |
| GitHub Copilot CLI | Copilot model ID | Programmatic prompt; native interactive CLI |
| Custom | Whatever your tool accepts | Executable and argument templates; optional catalog command |

Model IDs are not restricted to a hardcoded list. OpenAI, Anthropic, Google,
OpenRouter, xAI, DeepSeek, Mistral, local models, and other providers can be routed
through a harness that supports and is configured for them. Selecting a model
does not add provider support to a harness. Amp's mode determines more than its
underlying model. Direct provider API clients are outside this version's scope.

Model discovery is deliberately labeled:

- **Codex:** local cache, which may be stale. Refresh it through Codex.
- **Pi/OpenCode:** configured-provider catalogs, not an entitlement guarantee.
- **Cursor:** account-reported CLI list.
- **Claude/Amp:** documented aliases/modes plus your saved selections.
- **Other adapters:** native default, saved selections, and manual entry.

Custom IDs always remain editable. Built-in discovery does not send inference requests.
The router does not silently replace a model that fails or is unavailable.

## Workflow files

```sh
model-router init             # .model-router.toml in the selected project
model-router init --global    # reusable global templates; prints exact path
model-router validate
model-router preview
```

Configuration precedence is explicit `--config`, project `.model-router.toml`,
global config, then the starter workflow. The project file replaces the global
template set. Global configuration uses your platform's config directory
(`~/.config/model-router/config.toml` on Linux).

`s` saves project templates; `S` saves global templates. `w` chooses or clones
a workflow. `a` adds a stage, `d` duplicates it, `J`/`K` move it, and `x` removes
it. `v` toggles execution mode. `l` sets the repair limit. All fields, including
instructions and permission arguments, are editable with `e`.

Each stage has a stable `id`, human-readable `name`, semantic `role`, `enabled`
flag, `harness`, `model`, `provider`, `effort`, `mode`, `instructions`,
`extra_args`, and `timeout_seconds`. A review may also set `repair_target` to
an upstream implementation stage ID. Blank chooses the nearest enabled one.

## Running and recovering

```sh
model-router run --task 'Investigate and fix the cache invalidation bug'
model-router run --task-file task.md --unattended
model-router run --task 'Implement the feature' --with-review
model-router run --task 'Implement the feature' --worktree
model-router history
model-router resume RUN_ID
model-router resume RUN_ID --retry
model-router resume RUN_ID --model NEW_NATIVE_MODEL_ID
model-router resume RUN_ID --interactive
model-router resume RUN_ID --advance
```

`--unattended` overrides execution mode for this run. `--with-review` enables
existing review stages. It does not add one to a workflow that has none.

When an interactive harness exits, inspect its handoff and advance, reopen, or
stop. `Ctrl+]` stops a stuck interactive harness. In the monitor, `h` inspects
the handoff, `a` advances, `i` reopens interactively, `y` retries, and `m` replaces
the model before retrying. `e` edits a non-review interactive handoff summary.
The CLI's `--advance` is the equivalent explicit handoff acceptance.

An interactive review must write its handoff file; terminal echoes are not
accepted as review verdicts. A non-review interactive stage without a structured
handoff stops at the human checkpoint with an explicit missing-handoff notice.
You can supply a summary or reopen the stage.

Unattended stages have no interactive input. The router inherits native
permissions and accepts explicit per-stage argument overrides; it never adds
blanket permission-bypass flags. Reported denials pause the run. Some harnesses
surface approval needs only as an error or a stalled process; cancel or let
the stage time out, then reopen it interactively. Copilot programmatic tool
execution needs permissions configured explicitly in its native settings/flags.

Failures, cancellation, malformed review verdicts, and timeouts stop progress.
Retries preserve existing filesystem changes and get separate attempt logs.
After interruption, explicitly retry the current stage. Completed prior stages
are not replayed. Editing a template does not rewrite an existing run snapshot.

## Optional reviews and handoffs

Successful unattended stages advance automatically. Enabled reviews need a
structured `pass` or `needs_changes` verdict. Blocking findings and failed
required tests request repairs; advisory suggestions alone do not.

A repair returns to the review's implementation target and reruns downstream
stages, including earlier reviews that passed against the old code. Two repair
passes are allowed by default; a limit of zero disables automatic repair. If no
implementation target exists, or the limit is reached, the run stops for you.

Each stage receives the original task, its instructions, bounded prior handoffs,
and artifact paths. Full logs stay on disk. Prompts request this handoff format:

```json
{
  "summary": "What changed or was established",
  "verdict": "complete",
  "findings": [],
  "decisions": [],
  "files_changed": [],
  "tests": [{"command": "cargo test", "status": "passed", "required": true}],
  "unresolved": []
}
```

Normal stages use `complete`; reviews use `pass` or `needs_changes`. Any stage
can report `blocked`. Findings contain `severity` (`blocking` or `advisory`) and
`description`. Test status is `passed`, `failed`, or `not_run`.

Models can write the given handoff file or, when unattended, emit the object
between `<router-handoff>` and `</router-handoff>`. Plain text is accepted for
unattended non-review stages and labeled unstructured. Tool output and echoed
prompts are not extracted as assistant verdicts.

## Workspace and artifacts

All stages normally use your selected directory. Worktree mode requires a clean
Git checkout and starts from committed `HEAD` on a new `model-router/RUN_ID`
branch. The worktree is retained at `.model-router/worktrees/RUN_ID`. The router
does not merge, commit, or delete worktrees automatically.

```text
.model-router/
  .gitignore                    # ignores generated contents
  workspace.lock                # prevents simultaneous active runs here
  runs/RUN_ID/
    run.json                    # task, workflow snapshot, state, attempts
    01-research/attempt-001/
      prompt.md
      command.json              # executable and literal argument vector
      stdout.log
      stderr.log
      handoff.json
  handoffs/RUN_ID/               # model-writable handoff files
  worktrees/RUN_ID/              # only when requested
```

For worktree runs, run records remain in the original project's history;
model-writable handoffs are inside the worktree. Writes to run state are atomic,
and execution uses run/workspace locks. Remove old artifacts manually when you
no longer need their history. Native harness session storage is independent.

Read-only stage instructions are intent; effective tool access comes from the
harness. Credentials stay in native configuration/environment variables. Extra
arguments and task text are part of saved run artifacts.

## Custom adapters and model-only tools

Add an adapter to your workflow TOML, then choose its name as the harness:

```toml
[adapters.my_cli]
command = "/path/to/agent"
args = ["--print", "--model", "{model}", "{prompt}"]
interactive_args = ["--model", "{model}", "{prompt}"]
models_args = ["models", "--plain"]
prompt_stdin = false
```

Supported placeholders: `{prompt}`, `{prompt_file}`, `{handoff_file}`, `{model}`,
`{provider}`, `{effort}`, `{workspace}`, and `{run_dir}` (the attempt directory).
Arguments are passed directly; no shell evaluates them. Placeholder substitution
does not recursively interpret content supplied by a task or model.

Set `prompt_stdin = true` for tools that read an unattended prompt from stdin.
Omit `interactive_args` when the tool has no interactive interface. A catalog
command returns one native model ID per line. Model-only tools can use the same
adapter mechanism, for example `ollama run {model} {prompt}`, with provider
configuration kept in that tool.

## Offline demo and checks

The demo uses Python 3 and a local fixture. It sends no model requests.
Run it from this repository's root:

```sh
cargo run -- --config examples/demo.toml
cargo run -- --config examples/demo.toml run --task 'Exercise the handoff'
cargo run -- --config examples/demo.toml run --task 'Exercise repairs' --with-review
cargo test
cargo clippy --all-targets -- -D warnings
cargo build
uv run --with pyte python scripts/smoke_tui.py
uv run --with pyte python scripts/smoke_repos.py
uv run --with pyte python scripts/smoke_imports.py
uv run --with pyte --with pillow python scripts/smoke_startup.py
```

The terminal smoke test exercises actual PTYs, text editing, Unicode paste,
resizing, disabled review, interactive completion, history, and terminal cleanup.
The repository test covers navigation, filtering, recent projects, configuration
loading, autosave, and recovery from invalid paths/configuration. Screen captures
are text files in `.model-router/qa/`.
The import test covers file browsing, text and GitHub URLs, editable previews,
append/replace, cancellation, errors, and saved task contents using a local HTTP
server and an offline GitHub CLI fixture.
The startup test verifies animation, automatic completion, skip/bypass controls,
resize, and interrupt cleanup. It also records a GIF preview from the real
terminal at `.model-router/qa/startup.gif`.

Built-in adapter flags were checked against locally installed CLI help or
official documentation. Automated execution tests use mocks. Live authenticated
end-to-end runs against all ten services have **not** been performed. CLI
versions, native permissions, and account access can affect compatibility.
Linux is tested locally; the implementation targets Linux and macOS, but macOS
has not been exercised in this environment.

See [design decisions](docs/design-interview.md), [build specification](docs/spec.md),
and [adapter references](docs/adapters.md).
