# Model Router

<p align="center">
  <img src="docs/assets/model-router-icon.png" alt="Model Router icon" width="160">
</p>

A native Rust TUI for running a task through your preferred agentic harnesses
and models. Choose the sequence, the model for each stage, and whether each
stage runs unattended or opens its native interactive terminal.

The starter workflow is:

```text
Codex / gpt-6-astra       Claude Code / sonnet      Optional review        Codex / gpt-6-astra
Investigate, unattended → Implement, interactive → Disabled initially → Review summary, unattended
```

Every stage is editable, movable, repeatable, and optional. Review is never
required. The last stage writes a brief for you: what changed, how it was
verified, and what to read closely (see [Review summary](#review-summary)). The Astra ID is taken from the local account catalog used during this
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

`scripts/build.sh` builds, installs, and symlinks `mr` -> `model-router`:

```sh
scripts/build.sh
mr -C /path/to/project
```

The line under the title shows where `mr` is working: the repository name,
branch (or detached commit), and the count of changed files, followed by the
path; non-Git folders are labelled as such. It refreshes when you switch
projects with `p`.

Without `-C`, the router detects the project from the current directory: the
nearest parent with a `.model-router.toml`, otherwise the Git root (including
linked worktrees and submodules), otherwise the current directory itself. So
`mr` from any subdirectory of a configured repository loads that repository's
workflow, runs, and metadata. The walk stops at the Git root. CLI subcommands
use the same detection; `-C` always takes the given directory literally.

The TUI opens with a 1.7-second animation: a signal branches through model nodes
and converges into a route. Press any key to skip, or `Ctrl+C` to exit. The skip
key is consumed so it cannot trigger an action in the editor. The animation
adapts to terminal resizing and only plays once when the TUI starts.

Use `model-router --no-animation` to open the editor immediately. Set
`MODEL_ROUTER_NO_ANIMATION=1` in your shell to always bypass the intro. CLI
subcommands bypass it automatically; `NO_COLOR` suppresses its colors.

Opening the router does not start a model. Press `p` to choose a repository,
`t` to describe the task or `f` to import a task/spec,
`c` to configure the selected stage's harness, `e` to edit the stage,
`m` to choose a model, and `r` to start. `Space` enables
or disables a stage. `T` chooses a theme. `1`–`4` or `Tab` switch views.
`?` opens the full keyboard guide. `:` opens a command menu: type to search
the current view's actions, then press `Enter` to run one. Use `Ctrl+S` to apply
text/form edits; `Esc` cancels them, asking first if an editor has changes.
`q` and `Ctrl+C` exit; with unsaved workflow edits they ask first (`y` saves,
`n` discards, `Esc` stays). Page keys need their exact modifiers: `Ctrl+R` is
not `r`.

In selection menus, including repository and file browsers, `Ctrl+N` moves to
the next option and `Ctrl+P` to the previous option, just like `↓` and `↑`.

## What works

- Fourteen colour themes, including one that follows your terminal's colours, with live preview and a saved preference across projects.
- Named workflow templates and per-run snapshots.
- Thirteen stage presets and four ready-to-edit workflow presets.
- A closing review summary stage that briefs you on the finished work.
- Explicit harness/model/provider/effort selection and extra CLI arguments.
- In-app harness settings for each stage, including native permission controls.
- Optional research, planning, implementation, review, testing, and custom stages.
- Native interactive terminals, including detach/reattach, paste, resize, and a saved transcript.
- Several runs at once; a busy checkout sends the next run to its own worktree.
- Readable live output; take over a running stage's session in its native terminal.
- Unattended execution with live output, cancellation, and timeouts.
- Structured handoffs plus the original task carried between stages.
- Optional review-driven repairs, with two automatic repair passes by default.
- Saved runs, explicit retry, model replacement, and resume.
- Interactive repository browser, recent projects, and optional Git worktrees.
- Editable task/spec imports from local files, text URLs, and GitHub issues/files.
- CLI execution and previews using the same engine as the TUI.

## Choose a theme

Press `T` (Shift+T) from any main view. Type to filter, then use `↑`/`↓` or
`Ctrl+N`/`Ctrl+P` to preview a theme across the app. `Enter` applies and saves it;
`Esc` restores your current theme.

Choose from **Model Router** (the original default), **Terminal**, **Catppuccin Mocha / Latte**,
**Dracula**, **Nord**, **Tokyo Night**, **Gruvbox Dark / Light**, **One Dark**,
**Monokai**, **Solarized Dark / Light**, and **Poimandres**. Editors, file browsers,
status colours, and the startup animation all follow the selected theme.
**Terminal** draws with your terminal's own background, text, and ANSI colours,
so it matches your terminal's colour scheme and updates when you change it.

The preference is saved automatically in your user config directory
(`~/.config/model-router/appearance.json` on Linux, respecting `XDG_CONFIG_HOME`).
It applies to every project without changing workflow files. Native harness
terminals keep their own theme settings. See [theme details and palette sources](docs/themes.md).

## Optional Vim motions

Press **F8** (including while editing), or open `:` and choose **Vim motions**, to
enable or disable Vim motions for text inputs. They are **off by default**.
The preference is saved alongside the theme in `appearance.json` and applies
across projects and restarts.

The toggle sits below the header options, aligned to the right. It shows
**[F8] Vim OFF** when disabled and **[F8] VIM ON** in the accent color when enabled.

Inputs open in **Insert mode**, with the cursor at the end of their content.
`Esc` switches to **Normal mode**; another `Esc` cancels or goes back.
The current mode appears at the bottom of the dialog (beside the Runs filter
when filtering runs).

| Normal-mode key | Motion |
| --- | --- |
| `h`, `j`, `k`, `l` | Left, next line, previous line, right |
| `w`, `b`, `e` | Next word, previous word, end of word |
| `0`, `^`, `$` | Start of line, first nonblank character, end of line |
| `gg`, `G` | First line, last line |
| `i`, `a` | Insert before, append after the cursor |
| `I`, `A` | Insert at first nonblank character, append at end of line |

`Up`/`Down` and `Tab` retain their form/list navigation behavior; `Ctrl+S`
still applies edits. Refocusing a field returns to Insert mode at the end.
Pasting inserts literal text and returns to Insert mode. `j`/`k` move between
original lines, not soft-wrapped screen rows. This is basic motion support:
counts, operators such as `dw`, visual mode, registers, and repeat `.` are not
implemented. `Esc` clears an unfinished `g` before it can close a dialog.

## Select a repository

Press `p` on the Workflow page. The browser starts beside your current project,
with Git repositories listed before other folders. Type to filter names, then
use `↑`/`↓` and `Enter` to choose a repo or open a folder.

| Key | Action |
| --- | --- |
| `→` | Browse inside the highlighted folder or repo |
| `←` | Go to the parent folder |
| `Tab` | Switch between folders and recent projects |
| `/`, `~`, or `Ctrl+L` | Enter or paste a directory path; `Enter` opens it |
| `Esc` | Cancel path entry, or close the browser |

Paths can be absolute, relative to the displayed folder, or start with `~/`.
Choose **Use this folder** to select the displayed directory, including a
project without Git. Linked worktrees and submodules are recognized as repos.
The browser lists local folders one level at a time; it does not scan your
entire disk or clone remote repositories.

Switching saves edits to the current workflow, loads the selected project's
configuration, and resets the task and run view. Saved runs remain on disk.
Recent projects persist across launches. Active runs keep going in their own
projects; the Monitor still shows them.

## Import a task/spec

Press `f` on the Workflow page to browse local Markdown or UTF-8 text files.
The browser starts in the selected project. Type to filter, use `↑`/`↓` to
select, and press `Enter` to open a folder or preview a file. `←` opens the
parent folder; typing `/` or `~`, or `Ctrl+L`, accepts an absolute path, a path relative to the displayed
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
| omp (oh-my-pi) | Provider and model ID, passed as its `provider/model` selector; optional thinking | Print-mode JSON events (Pi's format); native interactive CLI; approval mode |
| Cursor Agent | Cursor model slug | Print-mode JSON; native interactive CLI |
| Gemini CLI | Gemini model ID | Headless JSON; interactive prompt |
| OpenCode | `provider/model`; optional variant | Run-mode JSON; interactive run |
| Aider | Native/LiteLLM model ID | Message file; interactive command file; automatic commits disabled |
| Goose | Provider and model | `run`; optional interactive continuation |
| Amp | Mode key, such as `high` | Execute-mode JSON; native interactive CLI |
| GitHub Copilot CLI | Copilot model ID | Programmatic prompt; native interactive CLI |
| Custom | Whatever your tool accepts | Executable and argument templates; optional catalog command |

Model IDs are not restricted to a hardcoded list. Amp's mode determines more
than its underlying model.

### Providers (OpenRouter and others)

Route a stage's harness through any OpenAI-compatible API by adding a
`[providers.NAME]` entry to the workflow TOML and setting the stage's
**Provider** to `NAME`. The router wires the endpoint into the harness's own
provider mechanism; the harness still runs the agent. Built-in presets
(`model-router providers`):

`openrouter`, `vercel`, `requesty`, `google`, `xai`, `mistral`, `deepseek`,
`moonshot`, `zai`, `minimax`, `groq`, `cerebras`, `together`, `fireworks`,
`deepinfra`, `huggingface`, `nebius`, `sambanova`, `perplexity`, `ollama`,
`lmstudio`.

An entry for a preset name without `base_url` uses that preset, optionally
overriding its other fields. Any other provider (Azure OpenAI, LiteLLM, vLLM, a
corporate gateway) needs `base_url`:

```toml
[providers.litellm]
base_url = "http://localhost:4000/v1"    # OpenAI-compatible root
api_key_env = "LITELLM_KEY"              # empty for keyless local servers
context_limit = 128000                   # optional; OpenCode compaction
output_limit = 16384                     # optional

[providers.openrouter]                   # use the built-in preset
```

| Harness | How the provider is applied |
| --- | --- |
| Codex | `--config model_providers.model-router-NAME.{name,base_url,env_key}` and `model_provider`. Needs a Responses API endpoint. |
| OpenCode | Adds an `@ai-sdk/openai-compatible` provider `model-router-NAME` to `OPENCODE_CONFIG_CONTENT`, keeping inherited inline config and its order. Model limits default to 32,768 context and, when unset, an output limit of a quarter of the context (at most 8,192), so compaction works; set `context_limit`/`output_limit` for the real values. Output must be below context. |
| Aider | `--model openai/MODEL` with `OPENAI_API_BASE`/`OPENAI_API_KEY`. |
| Goose | `--provider openai` with `OPENAI_HOST`/`OPENAI_BASE_PATH`/`OPENAI_API_KEY`. |
| Custom | `{provider}`, `{base_url}`, and `{api_key_env}` placeholders (configured entries only). |
| Claude Code, Cursor, Gemini, Amp, Copilot | Not supported; they use only their own accounts. |

Which providers are injected:

- **`[providers.NAME]` entries** route into every harness in the table above,
  custom adapters included. Pi and omp always use their own registries.
- **Names without an entry**, presets included, pass to the harness unchanged,
  so its own definition wins: Codex config layers and profiles (including
  `--oss` for `ollama`/`lmstudio`), and the OpenCode, Aider, Goose, and Pi
  registries with their auth (including OAuth). A custom adapter sees only
  `{provider}`; `{base_url}` and `{api_key_env}` stay empty and no key is
  required.

Codex and OpenCode see injected providers as `model-router-NAME`, so they never
merge with a native or reserved entry (`openai`, `ollama`, …) and inherit no
native endpoint, key, header, or stored login.

Keys stay in your environment. The router copies the named variable at launch,
fails before a run starts if it is unset, and records only variable names in
`command.json` and previews. `model-router providers NAME` lists the provider's
models via `GET /models` (no inference request).

Model discovery is deliberately labeled:

- **Codex:** local cache, which may be stale. Refresh it through Codex.
- **Pi/omp/OpenCode:** configured-provider catalogs, not an entitlement guarantee.
- **Cursor:** account-reported CLI list.
- **Claude/Amp:** documented aliases/modes plus your saved selections.
- **Other adapters:** native default, saved selections, and manual entry.

In the stage editor (`e`), `Up`/`Down`, `Tab`/`Shift+Tab`, or `Ctrl+N/P`
move between fields. Focusing an input places its cursor at the end of the
existing content, ready to append.

The **Model** field is a selection, not a text box:
`Enter` lists the stage's harness catalog, `Backspace` returns to the harness
default, and `i` types an ID the catalog does not list. Picking a catalog entry
with a provider fills the Provider field too. Built-in discovery does not send
inference requests.
The router does not silently replace a model that fails or is unavailable.

`model-router doctor` checks each harness's executable, version, and the flags
the router passes, using only `--version` and `--help`. It cannot check logins.
See [compatibility checks](docs/adapters.md#compatibility-checks). The
**Harnesses** page (**4**) runs the same checks in the background and shows each
harness's version, tested status, and capabilities; **Enter** lists each flag
and note. After it runs, the Workflow stage panel warns when the selected
stage's harness lacks a flag.

## Stage and workflow presets

Press `a` on the Workflow page to choose a stage. Type to filter the catalog;
the selected preset shows its purpose and default routing. `Enter` opens its
editable name, harness, model, mode, and instructions. `Ctrl+S` inserts the stage
after the selected stage; `Esc` cancels without changing the workflow.

| Stage | Purpose |
| --- | --- |
| Clarify requirements | Scope, acceptance criteria, assumptions, and open questions |
| Research | Project investigation and evidence gathering |
| Diagnose bug | Reproduction, root cause, and a regression check |
| Design solution | Architecture, interfaces, behavior, and tradeoffs |
| Plan implementation | Ordered changes and validation steps |
| Implement | Code changes and appropriate tests |
| Verify tests | Acceptance checks with actual results; no code fixes |
| Code review | Correctness, regressions, and optional repair requests |
| Security review | Relevant trust boundaries, permissions, and data handling |
| Update documentation | Usage, examples, and operational notes |
| Review summary | A brief for you: outcome, changes, verification, review focus, open questions |
| Summarize handoff | Outcome, evidence, limitations, and next actions |
| Custom stage | Blank instructions and your own routing |

Analysis, verification, and review presets start with unattended Codex/Astra.
Implementation and documentation use interactive Claude/Sonnet. Custom stages
inherit Codex's native model default. These are editable starting points using
the same model choices as the starter workflow; choose IDs available to your
account. Presets do not change your existing stages or saved runs.

Press `w` and choose **+ Preset:** to add a workflow:

| Workflow | Enabled sequence |
| --- | --- |
| Quick task | Implement → verify → review summary |
| Full feature | Clarify → research → design → plan → implement → verify → document → review summary |
| Bug fix | Diagnose → implement → verify → review summary |
| Investigation | Research → review summary |

The first three include a disabled code review; Full feature also includes a
disabled security review. Enable either with `Space` when wanted. Adding a
review through `a` explicitly opts that stage in. All stages can be disabled,
reordered, duplicated, removed, or reassigned to another harness/model.
Repeated workflow presets get distinct names and leave existing workflows intact.
The original short starter workflow remains the default.

```sh
model-router presets                 # list stage and workflow presets
model-router init --preset bug-fix    # create a project config from a preset
```

Initialization refuses to overwrite an existing config unless you pass
`--force`. Use `w` in the TUI to add presets to an existing configuration.

Verification stages require a structured handoff. Blocking findings, failed
required checks, or required checks marked `not_run` stop the run for inspection
and explicit retry, even with reviews disabled. Automatic repair loops remain
specific to enabled review stages. A review that still requests changes after
the repair limit blocks the run; `resume RUN_ID --allow-repair` allows one more
pass. A repair request reaches only the repaired stages and the review that
asked; it is dropped once that review passes.

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

`s` saves project templates; `S` saves global templates. `w` chooses, clones,
or adds a workflow preset; `R` renames and `X` deletes the current workflow.
`a` adds a stage preset, `d` duplicates it, `J`/`K` move it, and `x` removes it.
`X` asks before deleting; `u` undoes stage removals and workflow deletions
(until you switch projects). In the stage editor, `↑`/`↓` move within
multiline fields and leave them from the first or last line.
`h` picks the stage's harness and `v` toggles execution mode. `g` toggles an
isolated Git worktree for the next run. `l` sets the repair limit.
All fields, including instructions and permission arguments, are editable with `e`.

Each stage has a stable `id`, human-readable `name`, semantic `role`, `enabled`
flag, `harness`, `model`, `provider`, `effort`, `mode`, `instructions`,
`extra_args`, `harness_settings`, and `timeout_seconds`. A review may also set `repair_target` to
an upstream implementation stage ID. Blank chooses the nearest enabled one.

## Running and recovering

```sh
model-router run --task 'Investigate and fix the cache invalidation bug'
model-router run --task-file task.md --unattended
model-router run --task 'Implement the feature' --with-review
model-router run --task 'Implement the feature' --worktree
model-router history
model-router history --status failed,blocked --search parser --limit 10 --sizes
model-router rerun RUN_ID --unattended
model-router compare RUN_ID --stage research --attempts 1 2
model-router clean --older-than 30 --keep 5
model-router resume RUN_ID
model-router resume RUN_ID --retry
model-router resume RUN_ID --model NEW_NATIVE_MODEL_ID
model-router resume RUN_ID --interactive
model-router resume RUN_ID --advance
model-router resume RUN_ID --allow-repair
model-router report RUN_ID
model-router report RUN_ID --json
model-router report RUN_ID --diff
```

`--unattended` overrides execution mode for this run. `--with-review` enables
existing review stages. It does not add one to a workflow that has none.

### Several runs at once

`r` starts the run and clears the task, leaving Workflow ready for the next
one. Runs snapshot their workflow, so editing, switching workflows or projects,
and starting more runs never touch active ones. The Monitor tab counts runs in
progress (`2 Monitor ●2`). When there is more than one, the Monitor lists them
under its title; `[` and `]` switch between them, and `x` closes a stopped run
here (its files stay; Runs reopens it). The status line reports runs that stop,
complete, or wait for you while you are elsewhere.

A run that starts while another run of this session (or another router
process) is using the checkout gets its own Git worktree from committed HEAD,
on branch `model-router/RUN_ID`. Uncommitted changes in the checkout, including
the first run's edits, are not part of it. Outside Git, the second run is
refused instead. `g` still chooses a worktree explicitly.

A new interactive stage takes over the screen only while its run is shown in
the Monitor and no dialog is open. Otherwise it waits, and `t` or `i` opens it
when you are ready. A workflow
whose first enabled stage is interactive opens the Monitor right away.

### Detach and reattach an interactive session

Press **Ctrl+B** inside a native harness to return to the Monitor without
stopping it. The same process and PTY keep running and recording output.
Press **t** in that run's Monitor to reattach, restoring its current screen
and terminal size. Detaching does not create an attempt, finish the stage,
or advance the workflow. This works with custom adapters too; it does not
require the harness to support saved-session resume.

The harness still waits when it needs input or approval. `/exit` (or the
harness's own quit command) **quits the harness**, rather than detaching.
After it exits, the router performs the usual checks and handoff inspection.
`Ctrl+]` stops the attached stage; `C` stops it from the Monitor (`y` confirms).

CLI runs use the same **Ctrl+B** shortcut. Their detached waiting view accepts
`t` to reattach and `c` or `q` to stop, and notices completion without a keypress.
Detached sessions live only for the current `mr` process: quitting the router
or losing its terminal still cancels its active runs. In the TUI, `q` quits
after stopping all active runs; `Ctrl+C` stops them without quitting. In the
Monitor, `C` cancels the running stage after a confirmation, and `c` continues
an idle run (not in the moment a stage stops, so a late `c` cannot start the
next stage).

### Take over an unattended stage

The Monitor's Output pane shows each stage's events as readable lines:
assistant text (`●`), tool calls and shell commands (`⚙`, `$`) with a one-line
result (`↳`), file edits (`✎`), reasoning (`∴`), and errors (`✗`). Hook, delta,
and bookkeeping events are hidden. `stdout.log` keeps the raw events.

To work in the session yourself, press `t` in the Monitor and confirm with `y`:
the router stops the unattended process and reopens the same session in the harness's native
terminal, with its history, including the stage prompt and handoff path:

| Harness | Session | Reopened with |
| --- | --- | --- |
| Claude Code | `--session-id` chosen by the router | `claude --resume ID` + model, effort, permissions |
| Codex | `thread.started` event | `codex resume … ID` + model, effort, permissions |
| Pi | `--session-id` chosen by the router | `pi … --session-id ID` + model, provider, thinking |
| omp | `session` event | `omp --resume ID` + model, thinking, approval mode |
| Cursor Agent | `session_id` stream events | `cursor-agent … --resume ID` + model, permissions |
| OpenCode | `sessionID` events | `opencode --session ID` (the session keeps its model) |
| Amp | `session_id` stream events | `amp threads continue --mode MODE ID` |

Leaving the harness normally (`/exit`, `Ctrl+C`, or its own quit key) ends that
process and returns to the usual interactive checkpoint; the run is not
cancelled and the session stays saved. `t` there reopens the same session
again. `t` also works on a stage that failed, was blocked, or was cancelled,
and on interactive Claude Code and Pi stages. `Ctrl+]` still stops a stage
outright. Cancelling sends SIGTERM and allows five seconds before SIGKILL, so a harness
can save its session first (omp only saves on a clean exit). Gemini, Copilot,
Goose, Aider, and custom adapters cannot reopen a saved session after exit;
use `i` to retry the stage interactively. Live detached sessions can always
be reattached with `t`, regardless of harness.

### Review summary

Every workflow ends with an enabled **Review summary** stage (role `summary`).
It runs unattended (Codex, read-only sandbox) after everything else, reads the
task, every earlier handoff, and the actual Git changes, and writes a Markdown
brief for the person reviewing the work:

- **Outcome**: done, partly done, or not.
- **Changes**: each changed file or area, what changed and why.
- **Verification**: checks actually run and their results, and what was not verified.
- **Review focus**: the places to read closely, with file references.
- **Open questions**: decisions for you, limitations, follow-ups.
- **Try it**: commands to exercise the change.

The brief leads the Monitor's results when the run finishes, appears in
`model-router report` and the CLI's completion output, and is saved as
`.model-router/runs/RUN_ID/review.md`. Loading a workflow with no `summary`
stage, such as one saved before the stage existed, appends it enabled. To opt
out, disable it with `Space`; a deleted stage comes back on the next load.

When an interactive harness exits, inspect its handoff and advance, reopen, or
stop. `Ctrl+]` stops a stuck interactive harness. In the monitor, `h` inspects
the handoff, `a` advances, `i` reopens interactively, `y` retries, and `m` replaces
the model before retrying. `e` edits a non-review interactive handoff summary.
The CLI's `--advance` is the equivalent explicit handoff acceptance.

When a run completes or stops, the monitor shows its results: files changed
during this run with line counts, checks performed (router or model-reported,
required or optional), unresolved findings, and each stage's active time,
attempts, retries, and usage. `d` opens the full diff and `o` switches to the
output log. The CLI prints the same summary after `Completed`. `report` prints
it for any saved run, including stopped ones; `--json` gives the data and
`--diff` the patch. Changes come from Git snapshots taken by the router
(baseline at run start, then before and after each attempt), so edits made
before the run are excluded. Outside Git they are labelled unavailable and the
model-reported `files_changed` list is shown instead. Token counts and cost
appear only when a harness reports them; nothing is estimated, and totals say
how many attempts reported.

`history` lists runs newest first from their summaries, including damaged
records (`DAMAGED RUN_ID  error`). Filter with `--status` (`active`, a status
name, or `damaged`) and `--search` words matched against ID, workflow, task, and
status; `--json` prints the list and `--sizes` adds artifact sizes. `rerun`
starts a new run from a saved run's task, workflow, and adapter snapshot (not
the current template) and leaves the original unchanged. `compare` shows
model, mode, status, exit code, duration, router checks, and handoff differences
between two attempts of a stage, defaulting to the last two of the stage
retried most recently; `--source` compares a rerun with its source run.

On the Runs page (`3`), `/` filters as you type (`Enter` keeps, `Esc` clears),
`f` cycles the status filter, `n` reruns after a confirmation, `c` compares
attempts, and `D` previews removing the selected run. The list, sizes, and
new runs load in the background and refresh while runs are active. `Enter`
opens a run with the tail of its latest output; on a damaged row it shows the
error and path.

Interactive review and verification stages must write their handoff files;
terminal echoes are not accepted as verdicts. Other interactive stages without
a structured handoff stop at the human checkpoint with a missing-handoff notice.
You can supply a summary or reopen the stage.

Unattended stages have no interactive input. The router inherits native
permissions unless you explicitly override them in stage settings or extra
arguments; it never enables permission bypass by default. Reported denials pause the run. Some harnesses
surface approval needs only as an error or a stalled process; cancel or let
the stage time out, then reopen it interactively. Copilot programmatic tool
execution needs permissions configured explicitly in its native settings/flags.

### Configure a stage's harness

Select a stage in **Workflow** and press **c**. Edit its model, provider and effort
(where supported), execution mode, timeout, native permissions, and extra
arguments. **F7** browses models and returns to the settings draft.

- **Tab**, **Shift+Tab**, or **Ctrl+N/P** move between fields.
- **Left/Right**, **Space**, or **Enter** cycle choice fields. Type in text fields.
- **Ctrl+S** applies the draft; **Esc** cancels it. **s** on Workflow saves the
  project configuration; **q** also saves on exit.
- **P** on Workflow previews the resulting commands without running a model.

| Harness | Native permission controls |
| --- | --- |
| Codex | Sandbox (`read-only`, `workspace-write`, `danger-full-access`) and approval policy (`on-request`, `never`) |
| Claude Code | Permission mode (`manual`, `acceptEdits`, `plan`, `dontAsk`, `auto`, `bypassPermissions`) |
| Cursor | Sandbox (`enabled`, `disabled`) |
| Gemini | Approval mode (`default`, `auto_edit`, `plan`, `yolo`) |
| Other/custom harnesses | Native permission arguments through **Extra arguments** |

Permission controls default to **Harness default**, which sends no override. Native
mode availability depends on the installed CLI version and account. Codex
`never` disables approval prompts; it does not bypass the chosen sandbox.
Settings that also appear in extra arguments must be resolved before applying.
Choosing **Harness default** leaves control with native configuration and your
explicit extra arguments.

These settings belong to the selected stage. They are saved in its
`harness_settings` table and carried into new run snapshots:

```toml
[[workflows.stages]]
id = "implement"
harness = "codex"
# ...
harness_settings = { sandbox = "workspace-write", approval_policy = "on-request" }
```

Keys are `sandbox` and `approval_policy` (Codex), `permission_mode` (Claude),
`sandbox` (Cursor), and `approval_mode` (Gemini). Omitted keys inherit native
configuration. They do not edit
the harness's global configuration or credentials. Changing harness clears
its structured settings along with model/provider/effort; explicit extra
arguments are kept for you to review.

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
`description`. Test status is `passed`, `failed`, or `not_run`. A test stage
that cannot apply any check may add `"not_applicable": "reason"`.

Models can write the given handoff file or, when unattended, emit the object
between `<router-handoff>` and `</router-handoff>`. Plain text is accepted for
unattended stages other than review/verification and labeled unstructured.
Tool output and echoed prompts are not extracted as assistant verdicts.

### Verification

A `test` stage advances only with evidence: at least one required check that
passed. The model can report it in `tests`, or the router can run it. An empty
`tests` list, or one with only optional checks, stops the run.

Any stage can list checks for the router to run after its harness succeeds:

```toml
[[workflows.stages]]
id = "verify"
role = "test"
# ...
checks = [
  { command = ["cargo", "test", "--locked"] },
  { command = ["cargo", "clippy", "--", "-D", "warnings"], required = false, timeout_seconds = 300 },
]
```

Each `command` is an argument vector run in the run's workspace; no shell
evaluates it. `required` defaults to `true` and `timeout_seconds` to 600. The
prompt lists these commands. Results are recorded independently of the model's
handoff, in `run.json` and the attempt's `checks.json`, with logs under
`checks/NN/`. A required check that fails, times out, or cannot start blocks the
stage, whatever its role. Optional failures are only recorded.

In the TUI, **e** on a stage edits **Router checks**: one command per line,
shell-quoted (`npm run 'lint fix'`) but never run by a shell. A leading `?`
(`?cargo clippy` or `? cargo clippy`) marks a check optional. Timeouts are set
in TOML: a line whose command and required flag are unchanged keeps its
`timeout_seconds`; new lines get 600.

When no check can apply, a test stage may report `not_applicable` with a reason.
The run then waits at the human checkpoint. Accept it with `a` in the monitor,
`resume RUN_ID --advance`, or `a` at the CLI prompt. At an interactive
checkpoint, that one confirmation is the acceptance. It is saved on the stage;
a retry or repair discards it, so the rerun needs a new acceptance.

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
    summary.json                # what history lists; rewritten with run.json
    snapshot.index              # private Git index for change snapshots
    changes.diff                # patch of changes made during the run
    01-research/attempt-001/
      prompt.md
      command.json              # executable and literal argument vector
      stdout.log
      stderr.log
      handoff.json
      checks/NN/                # router check stdout.log and stderr.log
      checks.json               # router check results
  handoffs/RUN_ID/               # model-writable handoff files
  worktrees/RUN_ID/              # only when requested
```

For worktree runs, run records remain in the original project's history;
model-writable handoffs are inside the worktree. Writes to run state are atomic,
and execution uses run/workspace locks. Native harness session storage is
independent.

A stage's harness runs in its own process group, and the whole group ends with
the stage, after a normal exit too: SIGTERM, five seconds, then SIGKILL. Dev
servers or watchers a harness started do not outlive the workspace lock. On
Linux an unattended harness also gets SIGTERM if mr dies; an interactive one
gets SIGHUP from its closed terminal. Each attempt records its process group
while it runs, so after a crash the next `resume` stops any group still alive
before doing anything else (process group IDs are not reused while a member
lives).

Snapshot trees are kept by refs under `refs/model-router/runs/RUN_ID/`, so
`git gc` cannot prune them; `clean` deletes a run's refs with the run.

`model-router clean [--older-than DAYS] [--status S,..] [--keep N] [--worktrees]`
previews which runs it would remove, with sizes and skipped runs; add `--yes`
to remove them. It deletes each run directory and its handoffs. With
`--worktrees` it runs `git worktree remove` only on clean worktrees, never
forces, and keeps branches. Runs whose `run.lock` is held are skipped. It
also deletes the run's snapshot refs (`refs/model-router/runs/RUN_ID/`).

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
With a configured `[providers.NAME]` entry (see
[Providers](#providers-openrouter-and-others)), `{base_url}` and
`{api_key_env}` hold its endpoint and key variable name, and the key variable
is passed through to the process. Built-in provider names are not injected:
`{provider}` holds the name, and `{base_url}`/`{api_key_env}` expand empty.
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
uv run --with pyte python scripts/smoke_stages.py
uv run --with pyte python scripts/smoke_themes.py
uv run --with pyte python scripts/smoke_harness_settings.py
uv run --with pyte --with pillow python scripts/smoke_startup.py
```

GitHub Actions (`.github/workflows/ci.yml`) runs these checks on every pull
request and push to `main`: formatting, clippy, `cargo test` on Linux and
macOS, a Rust 1.85 MSRV check, and every smoke script against the Linux debug
build. Failed smoke runs upload their screen
captures as artifacts.

The terminal smoke test exercises actual PTYs, text editing, Unicode paste,
resizing, disabled review, interactive completion, history, and terminal cleanup.
The repository test covers navigation, filtering, recent projects, configuration
loading, autosave, and recovery from invalid paths/configuration. Screen captures
are text files in `.model-router/qa/`.
The import test covers file browsing, text and GitHub URLs, editable previews,
append/replace, cancellation, errors, and saved task contents using a local HTTP
server and an offline GitHub CLI fixture.
The stage test covers preset selection, editing, cancellation, ordering,
optional reviews, workflow persistence, and interactive verification handoffs.
The theme test previews and saves all 14 themes, including cancellation,
restart, repository switching, and invalid preferences.
The harness settings test covers per-stage isolation, native permission
arguments, conflict validation, model browsing from the draft, cancellation,
and persistence.
The startup test verifies animation, automatic completion, skip/bypass controls,
resize, and interrupt cleanup. It also records a GIF preview from the real
terminal at `.model-router/qa/startup.gif`.

Built-in adapter flags were checked against locally installed CLI help or
official documentation. Automated execution tests use mocks. Live authenticated
end-to-end runs against all ten services have **not** been performed. CLI
versions, native permissions, and account access can affect compatibility.
Linux is tested locally and in CI. On macOS, CI runs the Rust test suite; the
terminal smoke tests and live harness runs are Linux-only so far.

### Amp orbs

`.agents/setup` prepares fresh orbs with stable Rust (rustfmt and clippy),
Rust 1.85 for the MSRV check, Python 3.12, `uv`, `just`, and Ruff. It caches
the Python smoke-test dependencies (`pyte` and Pillow), fetches `Cargo.lock`
dependencies, and warms debug/test and MSRV build caches. Repeated setup runs
reuse installed tools and Cargo's incremental build state. Python 3.12 is
selected for `uv` commands in this checkout's non-interactive login shells.

Amp snapshots the prepared environment for later orbs. `.agents/resume`
only checks that tools are available; it installs nothing and starts no
services. Neither script authenticates harnesses or sends model requests.
The offline checks above need no API keys; live workflows still require
each harness's own credentials.

See [design decisions](docs/design-interview.md), [build specification](docs/spec.md),
and [adapter references](docs/adapters.md).
