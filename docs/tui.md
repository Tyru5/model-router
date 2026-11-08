# TUI

`model-router` (or `mr`) with no subcommand opens the TUI. Opening it starts no
model. In the TUI, `?` shows the keyboard guide and `:` a searchable command
menu of the current view's actions. Both are built from the same action table
as this page (`src/tui/actions.rs`).

Keys need their exact modifiers: `Ctrl+R` is not `r`. Shift is implied by
capital letters (`T` = Shift+T).

## Startup

A 1.7-second animation plays once. Any key skips it (the key is consumed);
`Ctrl+C` exits. Skip it with `--no-animation`, `MODEL_ROUTER_NO_ANIMATION=1`,
or `TERM=dumb`. CLI subcommands never play it.

The header line shows the project: repository name, branch (or detached
commit), changed-file count, and path; non-Git folders are labelled.

Project detection (TUI and CLI): `-C DIR` as given; otherwise the nearest
parent with `.model-router.toml`, else the Git root (worktrees and submodules
included), else the current directory. The walk stops at the Git root.

## Everywhere

| Key | Action |
| --- | --- |
| `Tab` | Next view |
| `1` `2` `3` `4` | Workflow, Monitor, Runs (reloads), Harnesses |
| `T` | Theme picker ([themes.md](themes.md)) |
| `I` | Routing stats dialog |
| `F8` | Toggle Vim motions |
| `F9` | Notifications… |
| `L` | This session's runs |
| `N` | Jump to the next run needing input |
| `!` | Notice history, newest first |
| `?` | Keyboard guide |
| `:` | Command menu |
| `q` | Quit; asks to save unsaved workflow edits (`y` save, `n` discard, `Esc` stay); stops active runs first |
| `Ctrl+C` | Cancel work, or exit |

In every selection menu, `Ctrl+N`/`Ctrl+P` = `↓`/`↑`. In editors, `Ctrl+S`
applies and `Esc` cancels (asking first if there are changes).

## Workflow (1)

| Key | Action |
| --- | --- |
| `r` | Start the workflow (clears the task for the next run) |
| `t` | Edit task (`Enter` adds a line, `Ctrl+S` applies) |
| `f` | Import task/spec ([below](#task-import)) |
| `p` | Repository browser ([below](#repository-browser)) |
| `w` | Select/clone a workflow or add a workflow preset |
| `R` / `X` | Rename / delete workflow (`y` confirms) |
| `↑` `↓` / `k` `j` | Select stage |
| `e` / `Enter` | Stage editor |
| `c` | Harness settings for the stage ([below](#harness-settings)) |
| `h` | Pick harness |
| `m` | Discover models |
| `Space` | Enable/disable stage |
| `v` | Toggle unattended/interactive |
| `a` | Add stage preset (edit, `Ctrl+S` inserts after the selection) |
| `d` / `x` | Duplicate / remove stage |
| `u` | Undo last stage removal or workflow deletion (until you switch projects) |
| `J` / `K` | Move stage down / up |
| `g` | Toggle isolated Git worktree for the next run |
| `l` | Automatic repair limit |
| `P` | Preview commands (no model calls) |
| `s` / `S` | Save project / global templates |

The stage panel shows routing, sandbox, missing harness flags (after
Harnesses has checked), and a dim history hint when stats favour another
assignment.

### Stage editor

`e` opens every stage field: Name, Role, Harness, Model, Provider, Reasoning
effort, Execution, Timeout, Review repair target, Extra arguments, Run after,
Fan-out candidates, Router checks, Router sandbox, Stage instructions, Ship
options. See [configuration.md](configuration.md#workflowsstages) for values.

- `↑`/`↓`, `Tab`/`Shift+Tab`, `Ctrl+N`/`Ctrl+P` move between fields; in
  multiline fields `↑`/`↓` move by line and leave from the first/last line.
- Focusing an input puts the cursor at the end.
- **Model** is a selection: `Enter` lists the harness catalog, `Backspace`
  returns to the default, `i` types an unlisted ID. A catalog entry with a
  provider fills Provider too.
- Multiline fields (checks, instructions, candidates, ship options): `Enter`
  inserts a newline.

### Harness settings

`c` edits model, provider, effort, execution, timeout, native permissions, and
extra arguments for the selected stage. `Tab`/`Shift+Tab`/`Ctrl+N/P` move;
`←`/`→`, `Space`, or `Enter` cycle choices; `F7` browses models and returns.
`Ctrl+S` applies, `Esc` cancels. Changing the harness clears its settings,
model, provider, and effort; extra arguments are kept for review. Permission
values: [harnesses.md](harnesses.md#permission-settings).

### Repository browser

`p`. Starts beside the current project; Git repositories first. Type to filter.

| Key | Action |
| --- | --- |
| `Enter` | Choose repo, or open folder |
| `→` / `←` | Browse inside / parent folder |
| `Tab` | Folders ↔ recent projects |
| `/`, `~`, `Ctrl+L` | Type or paste a path (absolute, relative, `~/`) |
| `Esc` | Cancel path entry, or close |

**Use this folder** selects the displayed directory, Git or not. It lists one
level at a time and never clones. Switching saves the current workflow, loads
the new project's config, and resets task and run view; active runs continue
and stay in the Monitor.

### Task import

`f` browses Markdown/UTF-8 text files from the project (same navigation as
above). `Ctrl+U` imports a URL instead:

- Direct HTTP(S) Markdown or text.
- GitHub issues and pull requests (title, body, link; no comments).
- GitHub file links (`blob`, `raw`, `raw.githubusercontent.com`), any ref.
- The same on GitHub Enterprise hosts `gh` is logged in to (`GH_HOST`).

GitHub links use `gh` and its login. Other URLs are fetched without
credentials, with [network limits](safety.md#task-imports).

The preview is editable. `Tab` switches **Append to task** / **Replace task**
(each keeps its edits; default Append if a task exists). `Ctrl+S` applies,
`Esc` goes back or cancels a download; the existing task is never lost. The
task must fit in 32 KiB of UTF-8; nothing is truncated. A BOM is removed and
line endings normalized. Runs save their own task copy.

## Monitor (2)

The tab shows active runs (`2 Monitor ●2`). The header shows cost and
`◆ 2 need input [N]` while runs wait.

| Key | When | Action |
| --- | --- | --- |
| `[` / `]` | any | Previous / next run of this session |
| `t` | any | Attach to the live interactive terminal, or reopen the saved session ([takeover](#interactive-sessions)); `y` confirms |
| `h` | any | Inspect handoff/error: findings by severity, tests by status; `↑↓` + `e`/`Enter` open a changed file in `$EDITOR` |
| `PgUp` / `PgDn` | any | Scroll output |
| `Home` / `End` | any | Oldest output (pauses) / newest (follows) |
| `f` | any | Pause/resume following |
| `/` | any | [Search output](#output-search) |
| `O` | any | Open the attempt's raw `stdout.log` in `$PAGER` |
| `C` | running | Cancel active stage (`y` confirms) |
| `a` | idle | Accept handoff and advance; accepts a `not_applicable` waiver; confirms a ship preview |
| `c` | idle | Continue the run |
| `y` | idle | Retry current stage |
| `Y` | idle | Retry one failed fan-out candidate (pick from a list) |
| `i` | idle | Retry interactively |
| `m` | idle | Replace model and retry |
| `e` | idle | Edit an interactive handoff summary |
| `d` | idle | [Diff view](#diff-view) |
| `o` | idle | Results ↔ output log |
| `x` | idle | Close the run here (files stay; Runs reopens it) |
| `n` | idle | Back to Workflow |

Output lines: assistant text `●`, tool calls `⚙`/`$` with a one-line result
`↳`, edits `✎`, reasoning `∴`, errors `✗`. Hook, delta, and bookkeeping events
are hidden; `stdout.log` keeps everything.

### Results

When a run completes or stops: review summary brief, files changed during the
run with line counts, checks (router or model, required or optional),
unresolved findings, per-stage active time, attempts, retries, usage, and
cost with its source. Fan-out candidates are listed under their stage, ★ on the
winner.

### Output search

`/`, type, `Enter`. Lowercase queries ignore case. Matches are highlighted;
`n`/`N` step while the search is shown; `Esc` clears. `PgUp`/`Home` pause
following (`▲ paused — End to follow`); `End`, `f`, or scrolling to the bottom
resume.

### Diff view

`d` on a finished run. Files left, hunks right. Up to 5,000 lines per file;
the pager shows the rest.

| Key | Action |
| --- | --- |
| `]` / `[`, `Tab` / `Shift+Tab` (`n` / `p` without a search) | Next / previous file |
| `}` / `{` | Next / previous hunk |
| `↑↓` `jk`, `PgUp` `PgDn` `Space`, `Home` `g`, `End` `G` | Scroll |
| `/`, then `n` / `N` | Search; next / previous match; `Esc` clears |
| `w` | Toggle wrap |
| `o` | Coloured patch in `$PAGER` (default `less -R`) |
| `e` / `Enter` | Open the file at the line in view in `$VISUAL`/`$EDITOR` (`+N` for vi/Vim/Neovim/nano/Emacs, `file:N` Helix, `-g file:N` VS Code) |
| `D` | `git difftool --dir-diff` over the run's changes (needs `diff.tool`) |
| `q` / `Esc` | Close |

`$PAGER` and `$EDITOR` are split like shell words and run without a shell.

### Interactive sessions

Native harness terminals get keys and paste forwarded.

| Key | Action |
| --- | --- |
| `Ctrl+B` | Detach to the Monitor; the process keeps running and recording |
| `t` (Monitor) | Reattach, restoring screen and size |
| `Ctrl+]` | Stop a stuck stage |
| `/exit` or the harness's quit | Quit the harness (not detach); the router then inspects the handoff |

A new interactive stage takes the screen only while its run is shown and no
dialog is open; otherwise `t` or `i` opens it. A workflow whose first enabled
stage is interactive opens the Monitor immediately.

**Leaving a stage running.** Each interactive harness runs in its own small
terminal host process, so it can outlive `mr`. Quitting (`q`) while one runs
asks first: `y`/`Enter` leaves it running in the background, `n` stops it,
`Esc` stays. The next `mr` in that project reconnects to it, opens the Monitor,
and `t` attaches with the screen as the harness left it. While no `mr` is
attached, the host holds the checkout's locks, so another run goes to its own
worktree. If the harness exits meanwhile, the next `mr` finishes the stage from
how it exited. One `mr` attaches at a time. `Ctrl+C`, `SIGTERM` and a closed
terminal still stop running stages.

**Takeover.** `t` on an unattended stage stops it (after `y`) and reopens the
same session in the native terminal, history included. Works for Claude Code,
Codex, Pi, omp, Cursor, OpenCode, Amp, and custom adapters with
`resume_args`; also on failed, blocked, or cancelled stages. Other harnesses:
use `i`. Fan-out stages cannot be taken over. Details:
[harnesses.md](harnesses.md#takeover).

### Several runs

`L` lists this session's runs (status, stage, elapsed, needs-input marker):
`Enter` focuses, `n` jumps to the next needing input, `C` cancels a run's
stage (`y` confirms). The status line reports runs that stop, complete, or
wait while you are elsewhere; `!` lists them.

## Runs (3)

| Key | Action |
| --- | --- |
| `↑↓` / `kj` | Select |
| `Enter` | Open in the Monitor with its latest output (damaged: error and path) |
| `/` | Filter by ID, workflow, task, status (`Enter` keeps, `Esc` clears) |
| `f` | Cycle status: all, active, complete, failed/blocked, cancelled, damaged |
| `n` | Rerun from the saved snapshot (confirm) |
| `c` | Compare two attempts of a stage |
| `D` | Preview removing the run (`y` confirms) |
| `r` | Reload runs and sizes |

The list loads in the background and refreshes while runs are active.

## Harnesses (4)

Runs `doctor` checks in the background: version, tested status,
capabilities. `↑↓`/`kj` select, `Enter` lists each flag and note, `r`
rechecks. See [harnesses.md](harnesses.md#doctor).

## Stats dialog

`I` shows the `model-router stats` table for this project. See
[automation.md](automation.md#stats).

## Notifications

`F9` (or `:` → **Notifications…**) picks backends, saved in `appearance.json`:

- **Terminal** (on): bell, OSC 9, OSC 777 to the controlling terminal (tmux
  passthrough), never into a harness PTY.
- **Desktop** (off): `notify-send` (Linux), `osascript` (macOS).
- **Webhook** (off): ntfy-style POST (body = message, `Title` header) to a URL,
  or `MODEL_ROUTER_NOTIFY_URL`, which turns it on by itself.

Sent when a run needs input (interactive stage, handoff review, verification
waiver), stops blocked or failed, or completes, even while another session
has the screen. The same event is sent at most once per three minutes. The
dialog has a test button. CLI runs: `--notify` or `MODEL_ROUTER_NOTIFY=1`.

## Themes

`T`. Type to filter, `↑↓`/`Ctrl+N/P` preview live, `Enter` saves for all
projects, `Esc` restores. Fourteen themes, including **Terminal**, which uses
your terminal's own colours. See [themes.md](themes.md).

## Vim motions

Off by default. `F8` (also while editing) or `:` → **Vim motions** toggles;
saved in `appearance.json`. The toggle shows `[F8] Vim OFF` / `[F8] VIM ON`.

Inputs open in Insert mode with the cursor at the end. `Esc` → Normal mode;
`Esc` again cancels (it first clears an unfinished `g`). The mode shows at the
bottom of the dialog.

| Normal-mode key | Motion |
| --- | --- |
| `h` `j` `k` `l` | Left, next line, previous line, right |
| `w` `b` `e` | Next word, previous word, end of word |
| `0` `^` `$` | Line start, first nonblank, line end |
| `gg` `G` | First / last line |
| `i` `a` | Insert before / append after cursor |
| `I` `A` | Insert at first nonblank / append at line end |

`↑↓` and `Tab` keep form navigation; `Ctrl+S` still applies. Paste inserts
literally and returns to Insert mode. `j`/`k` move by original lines, not
wrapped rows. No counts, operators (`dw`), visual mode, registers, or `.`.
