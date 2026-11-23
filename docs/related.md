# Related tools

Open-source tools with a similar shape (surveyed November 2026 from their
READMEs and specs; details may have moved on). Model Router's distinguishing
mix is a native TUI plus a CLI, per-stage harness and model choice, structured
handoffs between harnesses, interactive stages in the harness's own terminal,
and router-run verification with Git-tree receipts.

## Staged pipelines across agent CLIs

| Tool | Language | Shape | Worth knowing |
| --- | --- | --- | --- |
| [Crewplane](https://github.com/crewplaneai/crewplane) | Python | Markdown-defined, resumable multi-stage workflows over Claude Code, Codex, Copilot, Gemini, Pi, OpenCode and others | Stage graph in frontmatter (`needs`, sequential/parallel); prompts are Markdown sections with `{{plan.output}}` placeholders; a deterministic mock provider is the default first run; resume skips validated stages and reports identical inputs. |
| [Vibestrate](https://github.com/guyshonshon/vibestrate) | TypeScript | Local supervisor: plan → architecture → implement → validate → review → fix → verify, with human gates; never pushes | Seats/roles/profiles indirection; two-tier policies: `advise` rules go to the reviewer, `block` rules are regexes over changed lines that fail the run regardless of the review; daily spend cap can warn, downgrade the model, or stop. |
| [Claudexor](https://github.com/razzant/claudexor) | TypeScript | Control plane over paid harnesses with best-of-N races, cross-family review and quota rotation | Continuation packet for cross-harness thread handoff; `protected_paths` globs pause the apply for a person; review counts as clean only with two provider families; PR delivery refused when the secret scanner finds anything or cannot run. |
| [Tutti](https://github.com/nutthouse/tutti) | Rust | "Terraform-style" agent ops: roles map to runtimes, TOML workflows with typed steps, worktrees, audit | Per-agent path `scope` globs; artifact globs injected into later steps; `fail_mode = "closed"`; `land` step checks PR required checks and resolved threads; `doctor --strict`; `run --dry-run --json` prints literal argv. |
| [Symphony](https://github.com/openai/symphony) | Spec + Elixir | Tracker-driven isolated Codex runs with proof of work | `WORKFLOW.md` frontmatter + prompt body; lifecycle shell hooks with defined failure semantics; stall and turn timeouts; capped exponential retry backoff; explicit run-phase state machine; hot reload keeps the last good config. |
| [ccswarm](https://github.com/nwiizo/ccswarm) | Rust | Workflow engine over Claude Code and Codex with quorum review | Per-stage `provider`/`model` in YAML with documented precedence; reviewers emit a decision token and quorum counts approvals; `flow render/check`; global `--json`; README lists what is not shipped or not enforced. |
| [Bernstein](https://github.com/sipyourdrink-ltd/bernstein) | Python | Policy-as-code governance with a deterministic scheduler and many CLI adapters | Verifies concrete signals (tests, files, lint) and re-routes failures to another model; opt-in HMAC-chained audit log and signed run receipts; replay journals locate divergence; `doctor` also checks authentication. |
| [Gas Town](https://github.com/steveyegge/gastown) | Go | Coordinator-run multi-agent town with a Git-backed tracker, convoys and a merge queue | Runtime presets for eleven harnesses; Bors-style merge queue that batches, gates and bisects; stuck-agent detection with nudge and handoff keys; `doctor --fix`; OTLP logs and metrics. |
| [Kodo](https://github.com/ikamensh/kodo) | Python | API-model director over Claude Code, Cursor, Codex, Gemini and other workers | Per-agent `max_turns`, `timeout_s`, `fallback_model`; cost split into billed API spend versus subscription-covered; warns that `$0` does not prove free when usage is missing. |
| [gh-aw](https://github.com/githubnext/gh-aw) | Go | Markdown → GitHub Actions agentic workflows | Safe outputs: the agent job is read-only; writes are buffered, validated and applied by separate scoped jobs. |

Loop runners in the same family: [Ralphy](https://github.com/michaelshimeles/ralphy)
(`boundaries.never_touch` globs, per-task branches and PRs, retry delay,
rate-limit deferral) and [wreckit](https://github.com/mikehostetler/wreckit)
(file-only state per item, user-editable prompt templates, `doctor --fix`,
exit codes 0/1/130).

## Session managers and meta-harnesses

Parallel sessions rather than staged pipelines: [claude-squad](https://github.com/smtg-ai/claude-squad),
[agent-of-empires](https://github.com/njbrake/agent-of-empires) (tmux
persistence, container sandboxes, HTTP API), [coder/xum](https://github.com/coder/xum)
(costs tab, divergence view), [kandev](https://github.com/kdlbs/kandev) (a
different agent per workflow step over ACP), [Paperclip](https://github.com/paperclipai/paperclip)
(scoped budgets with warning thresholds and hard stops; revisioned config
with rollback). [agents-cli](https://github.com/phnx-labs/agents-cli) pins
harness versions per project and checks drift in CI. Aider's
[architect mode](https://aider.chat/docs/usage/modes.html) is the same
two-model split Model Router makes per stage. vibe-kanban and Crystal were
winding down at survey time.

## Ideas worth borrowing

Ranked by value for the effort, with the source in parentheses.

1. **Protected paths** *(implemented)*. Workflow `protected_paths` globs;
   enforced on ordinary stages, fan-out candidates (candidate stops blocked),
   and candidate selection, and refused by `ship` and `apply` (Ralphy,
   Claudexor).
2. **Block and advise policies** *(implemented)*. Declarative diff rules
   (`[[workflows.policies]]`) over changed lines and path globs; `block` stops
   the stage, candidate selection, `apply`, or `ship` whatever the review said
   (failing closed when the diff cannot be read), and `advise` is injected into
   the review prompt (Vibestrate).
3. **Stall timeout and bounded retry** *(implemented)*. Stage
   `stall_timeout_seconds` stops an unattended stage that produces no output
   for a while, and `retry` (`max_attempts`, `base_delay_seconds`,
   `max_delay_seconds`) retries transient provider errors with capped
   exponential backoff on the same harness and model (Symphony).
4. **Secret scan before ship** *(implemented)*. Built-in diff scanner
   (`secret_scan = true`) plus opt-in trust-gated external `secret_scanner`
   failing closed at plan and commit time (Claudexor).
5. **Lifecycle hooks** *(implemented)*. `before_stage`/`after_stage`/`before_ship`/`on_failure`
   argument-vector commands with a timeout; before-hooks abort, after-hooks log;
   gated behind `trust` and run approval digests like custom adapters (Symphony,
   Tutti).
6. **Harness version pins** *(implemented)*. Optional `[harness_versions]` pins
   checked by `doctor` and enforced by `doctor --strict`, with detected
   `harness_version` recorded in each attempt's `command.json` (agents-cli).
7. **Billing split, budget actions, and live streaming caps** *(implemented)*.
   `cost` (`--all-projects`) splits billed API spend from subscription
   API-equivalent estimates, notes attempts with unknown cost rather than
   counting them as `$0`, and budgets support `on_exceed = "warn" | "stop"`
   including mid-stream enforcement on unattended streaming stages (Kodo,
   Vibestrate).
8. **A "not enforced" list** *(implemented)* in [safety.md](safety.md#not-enforced)
   naming guarantees that depend on a harness honoring its own flags (ccswarm).
