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
| OpenCode | Installed `opencode run --help`; [CLI reference](https://opencode.ai/docs/cli/) | `run --format json`; `run --interactive` for terminal mode. `--model provider/model` and `--variant`. |
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
