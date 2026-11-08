# CLI and automation

Every subcommand uses the same engine, run records, and history as the TUI.
Generated from `model-router --help` and `model-router SUBCOMMAND --help`.

## Global options

Accepted before or after any subcommand.

| Option | Meaning |
| --- | --- |
| `-C, --project DIR` | Project directory. Default: nearest parent with `.model-router.toml`, else the Git root, else the current directory. |
| `--config FILE` | Use this config instead of project/global ([precedence](configuration.md#files-and-precedence)). |
| `--no-animation` | Open the TUI without the startup animation. |
| `-h, --help` / `-V, --version` | Help / version. |

## Run and recover

### `run`

Execute a workflow and persist its state and artifacts.

| Option | Meaning |
| --- | --- |
| `-w, --workflow NAME` | Workflow (default: the first). |
| `--task TEXT` | Task text. |
| `--task-file FILE` | Import a local Markdown/UTF-8 task (≤ 32 KiB; relative to the shell's cwd). |
| `--task-url URL` | Import a text URL or GitHub issue/PR/file URL (≤ 32 KiB). |
| `--worktree` | Isolated Git worktree from committed `HEAD`. |
| `--unattended` | Run every stage unattended this time. |
| `--with-review` | Enable existing review stages this time (adds none). |
| `--notify` | Notify when the run needs input, stops, or completes (also `MODEL_ROUTER_NOTIFY=1`). |

Use one of `--task`, `--task-file`, `--task-url`. An untrusted project config
is refused ([trust](safety.md#project-trust)). Interactive stages open their
native terminal; `Ctrl+B` detaches to a waiting view (`t` reattach, `d` leave
it running in the background and exit, `c`/`q` stop). `resume ID` reattaches
to a stage left running. Checkpoints prompt at the terminal (`a` accepts).

### `resume ID`

Resume a run. Retry and model changes are explicit.

| Option | Meaning |
| --- | --- |
| `--retry` | Retry the current stage. |
| `--interactive` | Retry it interactively. |
| `--model MODEL` | Retry with another native model ID. |
| `--advance` | Accept the checkpoint's handoff, waiver, or ship preview. |
| `--allow-repair` | One more review repair pass after the limit stopped the run. |
| `--retry-candidate STAGE/CANDIDATE` | Rerun one stopped fan-out candidate, keeping the others. |
| `--budget-usd N` / `--budget-tokens N` | Raise the run's cap (and any exceeded stage cap) to `N` and continue. |
| `--notify` | As for `run`. |

After a crash, `resume` first stops any surviving harness process group.

### `rerun ID`

New run from a saved run's task, workflow, and adapter snapshot (not the
current template). The original is unchanged.

| Option | Meaning |
| --- | --- |
| `--worktree` | Isolated worktree. Default: the saved run's choice. |
| `--unattended` | Run every stage unattended. |
| `--notify` | As for `run`. |

### `fork ID --stage STAGE`

New isolated run that continues a saved run from the checkpoint at the start
of `STAGE`. The fork gets its own worktree (from `HEAD`, then set to the
saved snapshot exactly), reuses the source's completed upstream handoffs
without running them, and runs `STAGE` and everything after it. Unlike
`resume --retry`, which keeps the files the last attempt changed, and
`rerun`, which repeats the whole workflow, a fork starts from saved files.
The source run, its workspace, and your checkout are unchanged. Nothing is
merged or shipped: inspect the fork's worktree, then `ship` it yourself. The
fork's changes, and what `ship` commits, are measured from where the source
run started, so they include the inherited upstream work (from the
checkpoint if the source's starting snapshot is gone).

| Option | Meaning |
| --- | --- |
| `--model MODEL` | Run `STAGE` with this model (not for fan-out stages). Later stages keep theirs. |
| `--unattended` | Run every stage unattended. |
| `--no-run` | Create the fork and print its ID; start it with `resume`. |
| `--notify` | As for `run`. |

The checkpoint is `STAGE`'s first attempt that started after every upstream
attempt finished, and its `tree_before` snapshot. The fork refuses, creating
nothing, when: the stage never started; the stage judges a fan-out (fork at
the fan-out stage instead); an upstream stage is not complete;
an upstream stage ran again after every attempt of `STAGE` started (a review
repair), so no snapshot matches the handoffs; a non-upstream stage changed
files before the checkpoint; the run has no snapshot (not Git) or it was
cleaned or pruned. If Git cannot reproduce the snapshot exactly (e.g.
attribute filters), the fork is saved as failed and says why. `report`
shows `Forked from:` and marks reused handoffs; `compare --source` compares
across a fork.

### `ship ID`

Commit a completed run's changes on a new branch after a preview, using the
run's ship stage options, else the defaults ([shipping](workflows.md#shipping)).

| Option | Meaning |
| --- | --- |
| `--push` | Push the branch with `--set-upstream`. |
| `--draft-pr` | Open a draft PR with `gh`; implies `--push`. |
| `--yes` | Ship without asking after the preview (required without a terminal). |

## Inspect runs

### `history`

Saved runs, newest first, including damaged records (`DAMAGED RUN_ID  error`).

| Option | Meaning |
| --- | --- |
| `--status S` | Comma-separated: `active`, `pending`, `running`, `interactive`, `awaiting_confirmation`, `complete`, `failed`, `blocked`, `cancelled`, `damaged`. |
| `--search TEXT` | Runs whose ID, workflow, task, or status contains every word. |
| `--limit N` | At most `N` runs. |
| `--json` | JSON list. |
| `--sizes` | Include run and handoff directory sizes. |

### `report ID`

Changes, checks, unresolved items, time, usage, cost, review summary. Works
for stopped runs too.

| Option | Meaning |
| --- | --- |
| `--json` | Data instead of text. |
| `--diff` | Print the patch of changes made during the run. |
| `--transitions` | Include the status timeline (`transitions.jsonl`): every run, stage, and attempt change with time, cause (engine, user, recovery), and reason. |

The text and `--json` reports include **verification receipts** (`receipts`)
for each stage whose latest attempt ran router checks (for a fan-out stage,
its chosen candidate's), and for each test stage: the basis (`router_checks`, `router_checks_failed`,
`self_reported_only`, `waived`, `none`), required router checks passed out of
those configured, model-
reported passing tests, the Git tree the checks ran against (`snapshot`), and
`freshness`: `current` (the workspace still matches), `stale` with the changed
files, or `unavailable` with a reason (not Git, no snapshot recorded, snapshot
pruned, workspace gone). Freshness compares content, so reverting an edit
makes it current again. A receipt confirms the configured checks ran against
that snapshot; it says nothing about whether they are adequate.

### `handoffs ID`

What a stage receives of upstream handoffs, from the same code that builds
its prompt: bytes sent of each handoff's budget (14,000 bytes in total, at
most 5,000 each, nearest stage first), each field that was shortened or
omitted with item counts, and warnings when decisions, unresolved items, or
findings were lost. Upstream stages that contributed nothing are listed with
why. Reads only; no model call.

| Option | Meaning |
| --- | --- |
| `--stage STAGE` | Stage ID. Default: the run's current stage. |
| `--prompt` | Also print the exact prompt the stage would receive now. |
| `--json` | JSON, including the prompt. |

### `compare ID`

Model, mode, status, exit code, duration, router checks, and handoff
differences between two attempts of a stage.

| Option | Meaning |
| --- | --- |
| `--stage STAGE` | Stage ID. Default: the stage retried most recently. |
| `--attempts A B` | Attempt numbers. Default: the stage's last two. |
| `--source` | Compare the stage's last attempt with the rerun or fork source's last attempt. |

### `clean`

Previews removing runs and their artifacts (run directory, handoffs, snapshot
refs); `--yes` removes. Runs whose `run.lock` is held are skipped.

| Option | Meaning |
| --- | --- |
| `--older-than DAYS` | Only runs last updated more than `DAYS` ago. |
| `--status S,..` | Only these statuses (as `history --status`). |
| `--keep N` | Never remove the `N` newest runs (default 0). |
| `--worktrees` | Also `git worktree remove` each clean run worktree (never forced; branches stay). |
| `--yes` | Remove instead of previewing. |

### `cost`

Estimated spend over saved runs, by harness and model ([cost.md](cost.md)).

| Option | Meaning |
| --- | --- |
| `--since AGE` | Only attempts started within `AGE`, e.g. `7d`, `12h`, `2w`. |
| `--json` | JSON. |

## Stats

`stats` ranks each (role, harness, model) seen in saved runs: samples, success
(completed with no failed, blocked, or cancelled attempt), mean retries,
repairs caused (implementation) or requested (review), review first-try pass
rate, failures, blocks, median duration, reported tokens and cost, router
check pass rate. Rows with fewer than five samples are marked `*`. Damaged
records are counted and skipped. Cached in `.model-router/stats-cache.json`.

| Option | Meaning |
| --- | --- |
| `--role ROLE` | `research`, `planning`, `implementation`, `review`, `test`, `summary`, `custom`. |
| `--json` | JSON. |
| `--all-projects` | Include every recent project. |
| `--min-pass RATE` | Add a cost-quality shortlist (below). `RATE` is 0–1. |

### Cost-quality shortlist

`stats --min-pass 0.8` (and `bench --min-pass`) lists assignments that
passed in at least that fraction of their verified samples, cheapest known
mean cost per sample first, then fastest
median duration. Each pick shows checks passed/verified, cost, median, and
sample count. A sample is verified when its stage has required router
checks, and passes when the stage completed with every one of them passing;
a stage that failed before its checks ran counts as a failure. Warnings
flag fewer than five verified samples (marked `*` and ranked after
well-sampled picks), samples without router checks, and samples with
unknown cost (unknown cost ranks after known). Assignments without required
router checks are left out as having no evidence; model-reported tests
never count. Advisory only: the
router never changes or substitutes a stage's model. `--json` adds
`shortlist`.

The TUI's `I` shows the same table. When a role has an assignment with ≥ 5
samples, a success rate 15 points higher, and no more repairs or retries, the
stage panel shows a hint like `History: claude/opus succeeded 9/10 here vs
4/8`. Advice only; nothing changes for you.

## Bench

`bench FILE` runs every task under every combination of stage assignments.
Each run gets its own Git worktree from committed `HEAD`, runs unattended
through the normal engine, and appears in history and stats.

| Option | Meaning |
| --- | --- |
| `--jobs N` | Runs at a time (default 1). |
| `--dry-run` | Show the matrix only. |
| `--allow-interactive` | Run interactive stages unattended instead of refusing. |
| `--min-pass RATE` | Add the [cost-quality shortlist](#cost-quality-shortlist) over variants (also appended to the `.md`). A cell is verified when the workflow has required router checks, and passes when the run completed with all of them passing. |

```toml
workflow = "Feature development"   # default: the first
repeat = 1                         # runs per (task, variant)
tasks = [
  { name = "greeting", text = "Add a --greeting flag with a test." },
  { name = "spec", file = "tasks/spec.md" },   # relative to the bench file
]

[[matrix]]                         # each axis multiplies the variants
stage = "implement"
assignments = [
  { harness = "claude", model = "opus" },
  { harness = "codex", model = "gpt-6-astra" },
]
```

Assignment fields: `harness`, `model`, `provider`, `effort`, `extra_args`.
Changing the harness clears provider, effort, harness settings, and extra
arguments unless given. Results: `.model-router/bench/bench-TIMESTAMP.json`
and `.md` (success, checks, repairs, duration, usage per variant and run).
`Ctrl+C` stops running stages and skips the rest. Example:
[examples/bench.toml](../examples/bench.toml).

## Configure and inspect

| Command | Purpose | Options |
| --- | --- | --- |
| `init` | Write a starter or preset workflow (reviews disabled) | `--global`, `--force`, `--preset quick-task\|full-feature\|bug-fix\|investigation\|best-of-3` |
| `presets` | List stage and workflow presets | |
| `validate` | Validate the workflow file; runs nothing | |
| `preview` | Exact argument vectors, no model launched | `-w, --workflow NAME`, `--task TEXT` (default `<your task>`) |
| `models HARNESS` | Model/mode selections, no inference request | |
| `harnesses` | Built-in and custom harnesses | `--spec [NAME]` (resolved spec JSON) |
| `providers [NAME]` | Router providers, or `NAME`'s `/models` (one listing request) | |
| `trust` | Trust this project's dangerous config sections | `--yes`, `--revoke` |
| `doctor` | Harness executables, versions, flags; sandbox backend | `--json`, `--strict` |
| `completions SHELL` | Completion script for `bash`, `zsh`, `fish`, `elvish`, or `powershell`; works outside a project | `--bin NAME` (default `model-router`; `mr` for the alias) |

```sh
model-router completions zsh > ~/.zfunc/_model-router        # then: fpath+=~/.zfunc; compinit
model-router completions zsh --bin mr > ~/.zfunc/_mr
model-router completions bash > ~/.local/share/bash-completion/completions/model-router
model-router completions fish > ~/.config/fish/completions/model-router.fish
```

## Serve

`serve` exposes run tools to other agents: MCP on stdio and/or a loopback
JSON HTTP API with SSE events. Runs are unattended only.

| Option | Meaning |
| --- | --- |
| `--mcp` | MCP over stdin/stdout. |
| `--http ADDR` | HTTP API on `ADDR`, e.g. `127.0.0.1:8737` (port 0 picks one). |
| `--allow-remote` | Let `--http` bind non-loopback. Anyone with the token can run harnesses. |
| `--allow-root PATH` | Directory whose projects clients may name (repeatable). Default: the server's project only. |

```sh
claude mcp add model-router -- model-router serve --mcp -C /path/to/project
```

Tools, Codex setup, HTTP routes: [serve.md](serve.md).

## Scripting notes

- Without a terminal, `run`/`resume` stop at interactive stages (resume from a
  terminal) and at checkpoints (accept later with `resume ID --advance`);
  `ship` needs `--yes`.
- `--json` exists on `history`, `report`, `cost`, `stats`, `doctor`.
- Artifacts for every run: `.model-router/runs/RUN_ID/`
  ([layout](workflows.md#artifacts)).
