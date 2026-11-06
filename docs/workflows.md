# Workflows

A workflow is a list (or graph) of stages. Each stage runs one harness with
one model, unattended or in its native interactive terminal, and writes a
structured handoff for the next. Field reference:
[configuration.md](configuration.md).

## Default workflow

**Feature development**:

| Stage | Role | Harness / model | Mode | Enabled |
| --- | --- | --- | --- | --- |
| Investigate | research | Codex / `gpt-6-astra` | unattended | yes |
| Implement | implementation | Claude Code / `sonnet` | interactive | yes |
| Review (optional) | review | Codex / `gpt-6-astra` | unattended | no |
| Review summary | summary | Codex | unattended | yes |

The Astra ID came from the account used during design; pick another if yours
differs. Every stage is editable, movable, repeatable, and optional.

## Roles

| Role | Behaviour |
| --- | --- |
| `research`, `planning` | Read-only intent. Run in parallel in graphs. `by-role` sandbox: read-only. |
| `implementation` | Writes the workspace. Repair target for reviews. Can fan out. |
| `review` | Must return `pass` or `needs_changes`. Blocking findings or failed required checks request a repair. Can judge a fan-out. |
| `test` | Advances only with evidence: at least one passed required check. See [Verification](#verification). |
| `summary` | Writes the closing brief (`review.md`). See [Review summary](#review-summary). |
| `ship` | No model. The router commits the run's changes. See [Shipping](#shipping). |
| `custom` | Anything else. Can fan out. |

Stage instructions saying "read-only" are intent only; real access comes from
the harness permissions and the optional [router sandbox](safety.md#sandboxing).

## Presets

`model-router presets` lists them. TUI: `a` adds a stage preset, `w` → **+ Preset:** adds a workflow.

| Stage preset | Key | Role |
| --- | --- | --- |
| Clarify requirements | `clarify` | planning |
| Research | `research` | research |
| Diagnose bug | `diagnose` | research |
| Design solution | `design` | planning |
| Plan implementation | `plan` | planning |
| Implement | `implement` | implementation |
| Verify tests | `verify` | test |
| Code review | `review` | review |
| Judge candidates | `judge` | review |
| Security review | `security` | review |
| Update documentation | `document` | custom |
| Review summary | `summary` | summary |
| Summarize handoff | `handoff` | custom |
| Ship (commit + draft PR) | `ship` | ship |
| Custom stage | `custom` | custom |

Analysis, verification, and review presets start as unattended Codex/Astra;
implementation and documentation as interactive Claude/Sonnet; custom stages
use Codex's default model.

| Workflow preset | Key | Enabled sequence |
| --- | --- | --- |
| Quick task | `quick-task` | Implement → verify → review summary |
| Full feature | `full-feature` | Clarify → research → design → plan → implement → verify → document → review summary |
| Bug fix | `bug-fix` | Diagnose → implement → verify → review summary |
| Investigation | `investigation` | Research → review summary |
| Best-of-3 implement | `best-of-3` | Research → implement (Claude, Codex, OpenCode side by side) → judge → verify → review summary |

Reviews (and Full feature's security review) and ship stages start disabled;
fan-out judges start enabled. `model-router init --preset KEY` writes one.

## Running

Order without `after`: list order. Successful unattended stages advance
automatically. Interactive stages stop at a checkpoint when the harness exits:
inspect the handoff, then advance (`a`), reopen (`i`), retry (`y`), or replace
the model (`m`). CLI: `resume RUN_ID --advance`, `--interactive`, `--retry`,
`--model ID`.

Failures, cancellation, malformed review verdicts, and timeouts stop the run.
Retries keep filesystem changes and get their own attempt logs. Completed
stages are never replayed. Runs snapshot their workflow: editing templates
never changes an existing run.

### Several runs and worktrees

Start as many runs as you like; each snapshots its workflow. A run that starts
while another run (this process or another) uses the checkout gets its own Git
worktree from committed `HEAD` on branch `model-router/RUN_ID`, at
`.model-router/worktrees/RUN_ID`. Uncommitted changes are not in it. Outside
Git the second run is refused. `g` (TUI) or `--worktree` (CLI) asks for a
worktree explicitly; that requires a clean checkout. The router never merges,
commits, or deletes worktrees, except an enabled ship stage commits.

## Graphs: `after` and parallel stages

Give any stage `after = ["id", ...]` and the workflow becomes a dependency
graph. A stage runs once every stage it names is complete; a stage without
`after` starts immediately; disabled stages are skipped through.

- Ready research, planning, review, and summary stages, and fan-out stages,
  run side by side, up to `max_parallel` harness processes (default 2).
- Stages that write the shared workspace, and interactive stages, wait and run
  alone in list order.
- `validate` rejects unknown IDs and cycles.

TUI: the stage editor's **Run after** field (comma-separated IDs).

## Fan-out: best-of-N

```toml
[[workflows]]
name = "Best of two"
max_parallel = 3

[[workflows.stages]]
id = "implement"
role = "implementation"
fan_out = { judge = "judge", candidates = [
  { harness = "claude", model = "sonnet" },
  { harness = "codex", model = "gpt-6-astra", effort = "high" },
] }

[[workflows.stages]]
id = "judge"
role = "review"
after = ["implement"]

[[workflows.stages]]
id = "verify"
role = "test"
after = ["implement"]   # also waits for the judge
```

- A `fan_out` stage must be unattended, with role `implementation` or `custom`,
  and have 2–8 candidates.
- Each candidate runs in its own detached Git worktree under
  `.model-router/candidates/RUN_ID/`, started from the workspace as it is,
  uncommitted changes included. IDs: `c1`, `c2`, … unless set.
- **Judge** `auto` (default, and what a disabled judge stage means): required
  router checks passing, then fewest changed lines. Or another review stage
  that runs after the fan-out (in a graph, its `after` lists the fan-out; in a
  list, it is the next enabled stage), judges only that fan-out, and names its
  choice in its handoff as `"selected": "c2"`.
- The router applies the winning patch to the workspace. If it conflicts with
  changes made since, the workspace is left as it was and the run blocks with
  the patch and worktree paths.
- Losing candidates stay on disk until `clean` removes the run.
- One stopped candidate can be rerun alone:
  `resume RUN_ID --retry-candidate implement/c2`, or `Y` on the Monitor.
- Fan-out stages cannot be retried interactively or taken over.

TUI: **Fan-out candidates** field, one per line
`[id:] harness [model] [provider] [effort]`, plus a `judge = auto|ID` line.
The Monitor lists candidates under their stage; ★ marks the choice.

## Handoffs

Each stage gets the original task, its instructions, bounded prior handoffs,
and artifact paths. It returns a handoff (schema version 2,
[`schema/handoff.schema.json`](../schema/handoff.schema.json); a copy is
written to each attempt directory):

```json
{
  "schema_version": 2,
  "summary": "What changed or was established",
  "verdict": "complete",
  "findings": [],
  "decisions": [],
  "files_changed": [],
  "tests": [{"command": "cargo test", "status": "passed", "required": true}],
  "unresolved": []
}
```

- `verdict`: `complete`; reviews `pass` or `needs_changes`; any stage `blocked`.
- `findings`: `{severity: blocking|advisory, description}`.
- `tests[].status`: `passed`, `failed`, `not_run`.
- Optional: `not_applicable` (test stage), `artifacts`
  (`[{kind: patch|file|url|note, path_or_url, description}]`), `confidence`
  (`low|medium|high`), `next_steps`, `selected` (fan-out judge).
- A handoff without `schema_version` is v1; old runs stay valid.

The router takes the first valid handoff from: the harness's native structured
output (Codex `--output-schema`, Claude Code `--json-schema`, when the version
supports them), the handoff file, the last `<router-handoff>` block, fenced
JSON, or a bare JSON response. A repeated contract example is never an answer.
Plain text is accepted, labelled unstructured, except for review and test
stages. Interactive review/test stages must write the handoff file. Reports
show each handoff's source.

## Reviews and repairs

Reviews are optional and start disabled. An enabled review needs `pass` or
`needs_changes`. Blocking findings, or a failed required router check, request
a repair; advisory findings do not.

- A repair reruns the review's `repair_target` (default: nearest enabled
  upstream implementation stage) and every downstream stage, including earlier
  reviews.
- `max_repair_passes` (default 2; TUI `l`) bounds automatic passes; `0`
  disables them. Past the limit, or with no target, the run blocks.
  `resume RUN_ID --allow-repair` allows one more pass.
- The repair request reaches only the repaired stages and the requesting
  review; it is dropped once that review passes.
- `run --with-review` enables existing review stages for one run; it does not
  add one.

## Verification

A `test` stage advances only with evidence: at least one **required** check
that **passed**, reported by the model in `tests` or run by the router. Empty
`tests`, only optional checks, a failed required check, or a required check
marked `not_run` stop the run, even with reviews disabled.

### Router checks

Any stage can list `checks` (argument vectors, no shell) that the router runs
after the harness succeeds, independent of the handoff:

```toml
checks = [
  { command = ["cargo", "test", "--locked"] },
  { command = ["cargo", "clippy", "--", "-D", "warnings"], required = false, timeout_seconds = 300 },
]
```

- `required` defaults to `true`, `timeout_seconds` to 600.
- Results go to `run.json` and the attempt's `checks.json`; logs under `checks/NN/`.
- A required check that fails, times out, or cannot start blocks the stage; in
  an enabled review it requests a repair while passes remain. Optional
  failures are recorded only.
- TUI **Router checks** field: one command per line, shell-quoted but never
  run by a shell; a leading `?` marks it optional. A line whose command and
  required flag are unchanged keeps its `timeout_seconds`; new lines get 600.

### Not applicable

When no check can apply, a test stage may report
`"not_applicable": "reason"`. The run waits; accept with `a` (Monitor),
`resume RUN_ID --advance`, or `a` at the CLI prompt. The waiver is saved on
the stage; a retry or repair discards it.

## Review summary

Every workflow ends with an enabled **Review summary** stage (role `summary`,
unattended Codex with its `read-only` sandbox). It reads the task, every
handoff, and the actual Git changes, and writes a brief:

- **Outcome**: done, partly done, or not.
- **Changes**: each file or area, what and why.
- **Verification**: checks run and results; what was not verified.
- **Review focus**: places to read closely, with file references.
- **Open questions**: decisions, limitations, follow-ups.
- **Try it**: commands to exercise the change.

It leads the Monitor's results, `report`, and the CLI completion output, and
is saved as `.model-router/runs/RUN_ID/review.md`. Disable or delete it to
opt out.

## Shipping

The router never commits unless an enabled **Ship** stage (role `ship`) runs.
No model runs: the router commits exactly the files changed during the run
(from its snapshots) on a new branch from `HEAD`, optionally pushes, and
optionally opens a draft PR with `gh`.

1. The stage stops at a checkpoint with a preview: branch, files, commit
   message, push, PR title/body.
2. `a` (or `resume RUN_ID --advance`) ships that exact plan. If anything
   changed since, it previews again. `serve` clients cannot confirm it.

- Files you changed before the run, and your staged changes, stay uncommitted.
  A file changed both before and during the run blocks the ship unless
  `include_preexisting = true`.
- The commit uses your `git config` identity (refuses without one) and adds
  no trailers or attribution.
- Any Git or `gh` failure blocks the stage with its exact error and what was
  already done.
- Branch, commit, push, and PR URL appear in the handoff and `report`.
- After the fact: `model-router ship RUN_ID [--push] [--draft-pr] [--yes]`.

Options: [configuration.md](configuration.md#ship). TUI: **Ship options**
field (TOML).

## Budgets

Workflow and stage `budget` caps stop a run before its next stage. See
[cost.md](cost.md#budgets).

## Artifacts

```text
.model-router/
  .gitignore                    # ignores generated contents
  workspace.lock                # one active run per checkout
  pricing-cache.json            # provider prices, daily
  stats-cache.json              # stats by run revision
  runs/RUN_ID/
    run.json                    # task, workflow snapshot, state, attempts
    summary.json                # what history lists
    transitions.jsonl           # every status change
    snapshot.index              # private Git index for snapshots
    changes.diff                # patch of changes made during the run
    review.md                   # review summary brief
    01-research/attempt-001/
      prompt.md
      handoff.schema.json
      command.json              # executable and argument vector; env values redacted
      stdout.log
      stderr.log
      handoff.json
      checks/NN/                # router check logs
      checks.json
  handoffs/RUN_ID/              # model-writable handoff files
  worktrees/RUN_ID/             # worktree runs
  candidates/RUN_ID/            # fan-out candidate worktrees
  bench/                        # bench results
```

Changes come from Git snapshots (baseline at run start, before and after each
attempt), so edits made before the run are excluded. Snapshot trees are kept by
refs under `refs/model-router/runs/RUN_ID/` so `git gc` keeps them; `clean`
deletes them with the run. Outside Git, changes are labelled unavailable and
the model-reported `files_changed` is shown. For worktree runs, run records
stay in the original project; handoffs are inside the worktree. Run state
writes are atomic and guarded by run/workspace locks.
