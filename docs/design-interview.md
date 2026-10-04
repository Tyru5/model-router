# Model Router — design interview

Status: all 16 product decisions confirmed by the user. Implementation authorized
by the original build request and acceptance of the final recommendations.

## Purpose

A personal native TUI to configure and run a sequence of task stages across
agentic harnesses and models. Example: Codex with Astra investigates a task,
Claude Code implements it, and an optional model reviews the result.

## Confirmed decisions

| Question | Decision |
| --- | --- |
| Initial | Run a sequence of stages, not just a one-off launcher. |
| Q1: interaction | Choose unattended or interactive execution per stage. |
| Q2: routing | Reusable workflows with explicit harness/model assignments and per-run overrides. Automatic task classification is not part of the initial design. |
| Q3: review authority | When review is enabled, blocking findings can send work back to implementation automatically, with a bounded repair loop. |
| Q4: integrations | CLI orchestration for Codex, Claude Code, Pi, Cursor Agent, Gemini CLI, OpenCode, Aider, Goose, Amp, and GitHub Copilot CLI; custom adapters for other tools. Embedded IDE control is outside the initial scope. |
| Q5: authentication | Reuse each harness's existing subscription login or API/provider configuration. No router credential vault. |
| Q6: audience | Personal tool first, portable configuration, Linux and macOS support. |
| Q7: handoff | Carry original task and constraints plus structured findings, decisions, changed files, test results, and unresolved issues. Retain full logs separately. |
| Q8: workspace | All stages share the selected working directory. Optional Git worktree per run. Commits and merges remain under user control. |
| Q9: optional review | Review is optional, selectable per workflow/run. Multiple review stages are allowed. Blocking correctness findings and failed required tests trigger repairs; suggestions are advisory. Rerun affected reviews after repairs. A run without review completes after its last enabled stage. |
| Q10: recovery and limits | Stop on harness/authentication errors; allow retry, explicit model replacement, or resume. No silent substitution. Default to two repair passes and a configurable 60-minute timeout per unattended stage. |
| Q11: interactive completion | Exiting a harness returns to the router to inspect the handoff and advance, reopen, or stop. Successful unattended stages advance automatically. |
| Q12: entry point | Start in the current project by default; also offer a project picker, reusable workflow templates, and saved runs that can be resumed. |
| Q13: model selection | Show discovered models with source/availability labels; allow custom identifiers and preserve native model, alias, and mode semantics. |
| Q14: flexibility | Add, remove, reorder, repeat, and disable stages; instructions per stage. Starter: unattended Codex/Astra research, interactive Claude implementation, disabled review. |
| Q15: permissions | Inherit harness permissions with explicit per-stage argument overrides. Surface permission blocks and offer an interactive retry. |
| Q16: model access | Access models through compatible harnesses and custom adapters for model-only CLIs. Direct API clients are outside initial scope. |

## Decision frontier

Empty. The user's final acceptance of all remaining recommendations confirms the
shared understanding. Routine implementation choices are recorded in the build
specification rather than reopening the interview.

## Discovery facts

The project directory was initially empty and was not a Git repository. Codex,
Claude Code, Pi, Cursor Agent, OpenCode, and Amp executables are installed on the
current Linux workstation. Presence on PATH is not proof of authentication or
successful execution. Discovery has not started any model inference.

Model selection needs to preserve each harness's native semantics. A catalog
entry is not necessarily proof that an account can run that model.

| Harness | Discovery evidence | Selection semantics |
| --- | --- | --- |
| Codex | App-server `model/list`; local model cache can be a fallback. The local cache includes `gpt-6-astra`. | Native model ID plus supported reasoning effort. Cache entries can be stale or hidden. |
| Claude Code | Installed CLI accepts aliases and full model IDs; no standalone list command in its help. SDK initialization can expose supported models but needs a tested adapter. | Alias or provider-specific model ID. Aliases can change. |
| Pi | Installed `--list-models` command and local implementation. | Exact provider/model selection; configured providers and extensions may add entries. |
| Cursor Agent | Installed `models` and `--list-models` commands. | Cursor-native selection; account-specific catalog. |
| OpenCode | Documented `models [provider]` command. | Provider/model ID; configured-provider catalog is not an entitlement check. |
| Amp | Installed CLI uses `--mode`; provider access checks may invoke inference. | Mode, not an arbitrary model ID. Modes also select prompts and tools. |

Sources and local evidence:

- Installed `codex exec --help`, `claude --help`, `pi --help`,
  `cursor-agent --help`, and `amp --help`.
- Local Codex model-cache metadata (credentials were not read).
- [Claude Code model configuration](https://code.claude.com/docs/en/model-config).
- [OpenCode CLI model catalog](https://opencode.ai/docs/cli/#models).
- [Amp modes](https://ampcode.com/docs/models-and-subagents).
- [Amp model routing](https://ampcode.com/docs/customize/model-routing).
- [Gemini CLI headless reference](https://geminicli.com/docs/cli/headless/).
- [Goose CLI commands](https://github.com/aaif-goose/goose/blob/main/documentation/docs/guides/goose-cli-commands.md).
- [GitHub Copilot CLI reference](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-command-reference).

The remaining adapter contracts and invocation flags still need verification
before implementation can claim support.
