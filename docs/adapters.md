# Adapter references

Checked during implementation on 2026-10-03. Native model names and provider
access remain the responsibility of each harness; the router has no universal
model entitlement database.

| Adapter | Reference | Integration detail |
| --- | --- | --- |
| Codex | Installed `codex exec --help` and model-cache metadata | `--model`, `--config model_reasoning_effort`, JSON events, `--output-last-message`; interactive prompt is positional. |
| Claude Code | Installed `claude --help`; [model configuration](https://code.claude.com/docs/en/model-config) | `--print --output-format json`, stdin prompt, `--model`, `--effort`; explicit permission denials are handled. |
| Pi | Installed `pi --help`; [CLI source/documentation](https://github.com/badlogic/pi-mono/tree/main/packages/coding-agent) | `--print --mode json`, `@prompt-file`, `--provider`, `--model`, `--thinking`. Provider catalogs include extension-defined models. |
| Cursor Agent | Installed `cursor-agent --help`; [CLI documentation](https://cursor.com/docs/cli/overview) | Print-mode JSON and account model listing. Uses `cursor-agent` when installed, otherwise `agent`. |
| Gemini CLI | [Headless reference](https://geminicli.com/docs/cli/headless/) and [CLI reference](https://geminicli.com/docs/cli/cli-reference/) | `--prompt`, JSON `response`, native `--model`; interactive uses `--prompt-interactive`. |
| OpenCode | Installed `opencode run --help`; [CLI reference](https://opencode.ai/docs/cli/) | `run --format json`; `run --interactive` for terminal mode. `--model provider/model` and `--variant`. OpenCode 2.x removed `run --interactive` and `--variant`; `doctor` reports both. |
| Aider | [Scripting](https://aider.chat/docs/scripting.html) and [options](https://aider.chat/docs/config/options.html) | `--message-file` unattended; interactive `--read` plus `--load` command file. Disable auto/dirty commits. |
| Goose | [CLI command reference](https://github.com/aaif-goose/goose/blob/main/documentation/docs/guides/goose-cli-commands.md) | `run --text`, `--provider`, `--model`; `--interactive` continues the conversation. |
| Amp | Installed `amp --help`; [modes](https://ampcode.com/docs/models-and-subagents) | `--execute --stream-json` reads stdin. `--mode` selects a mode, which can change its underlying model. |
| Copilot CLI | [Command reference](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-command-reference) | `--prompt` or `--interactive`, plus `--model`. Programmatic tool permissions must be explicitly configured by the user. |

The router uses native authentication and forwards explicit user arguments.
It never silently enables `--yolo`, `--force`, sandbox bypass, or permission bypass.
The per-stage harness settings dialog (`c` on Workflow) can send explicit
permission overrides. Codex sandbox and `approval_policy` config overrides,
Claude `--permission-mode`, and Cursor `--sandbox` were checked against installed
CLI help on 2026-10-04. Gemini's `--approval-mode` choices were checked against
its [CLI reference](https://geminicli.com/docs/cli/cli-reference/). Extra arguments
remain available for other native settings; conflicting permission overrides
are rejected before execution. No override is emitted for Harness default.
Unknown effort/provider combinations are rejected rather than ignored. Custom
adapters can integrate other CLI versions or tools without changing Rust code.

Discovery limitations are intentional and visible. Codex currently reads its
local cache; Claude's native SDK model discovery is not implemented. The other
non-enumerating adapters permit manual IDs and configured defaults. CLI catalog
parsers fail back to manual entry instead of making paid access-test requests.

## Compatibility checks

`model-router doctor` checks each harness without sending a model request:

```text
HARNESS   STATUS     VERSION  TESTED  CAPABILITIES                                     PATH
codex     installed  0.160.0  tested  ok                                               ~/.local/bin/codex
opencode  installed  2.0.22   tested  missing: --variant, --interactive (interactive)  ~/.opencode/bin/opencode
gemini    missing    -        -       not checked                                      gemini

Authentication is not checked: that needs a model request. Log in through each harness.
```

- **Version** is the first line of `<program> --version`.
- **Capabilities** come from the adapter code. Doctor builds sample unattended
  and interactive invocations with `adapters::build`, using a model plus every
  provider, effort, and harness setting the adapter accepts. Each `--flag` in
  them is looked up in the harness's help: `codex exec --help`,
  `opencode run --help`, or `goose run --help` when the arguments start with a
  subcommand, otherwise `<program> --help`. Only option lines count, by short
  or long alias; a flag mentioned in prose does not. A flag is `supported`,
  `missing`, or `unknown` (help unavailable). Missing means current help lacks
  it: the harness removed or hid it.
- **Tested** means the detected version has parser fixtures (below).
- Custom adapters show `not declared`: they have no flag contract, so doctor
  does not run them.
- Authentication is never checked.

`doctor --json` prints the full reports, including each flag and the help
command checked. `doctor --strict` exits non-zero when an installed harness
lacks a flag. Probes run in parallel and finish within about 10 seconds.
`model_router::doctor::inspect_all` returns the same data for the TUI.

### Parser fixtures

`tests/fixtures/harness-output/manifest.toml` lists every fixture with its
harness, output kind, harness version, scenario, provenance, source, and
sanitization notes. `captured` fixtures are sanitized real output.
`schema-derived` fixtures are built from event shapes in installed sources,
schemas, or official documentation. All current fixtures are schema-derived:
no router run output existed on disk, and none were produced by a model
request. `tests/harness_output.rs` runs `handoff::normalize`, and
`handoff::parse` for handoffs, on each fixture and checks the declared text,
errors, and permission denials. Scenarios cover success, reported errors,
retries, permission denials (Claude), and tool output or echoed prompts that
contain fake handoffs.

To record a real fixture (this sends one model request, which may be billed):

```sh
cargo build
python3 scripts/capture_harness_output.py claude --confirm-paid-request --model sonnet
cargo test --test harness_output
```

The script runs one unattended stage in a temporary project, with restrictive
settings where available: Codex read-only sandbox, Claude plan mode, Cursor
sandbox, and Gemini plan approval. The task asks for a fixed handoff block. It
sanitizes the attempt's `stdout.log` (prompt echoes, paths, user and host
names, emails, IDs, and keys) into `<harness>/captured-success.<ext>` and
appends a `captured` manifest entry. Review both before committing.
