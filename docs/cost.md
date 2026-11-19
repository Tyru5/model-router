# Cost and budgets

Costs are **estimates**, labelled by source. Verify with your provider.

## Sources

Each attempt's cost comes from the first source that applies:

| Source | Meaning |
| --- | --- |
| `reported` | The harness reported its own cost (Claude Code, Amp, Pi, OpenCode). |
| `config` | Tokens × your prices: `<config dir>/pricing.toml`, then `[pricing]` in the global, then project config (later wins). |
| `provider` | Tokens × a router provider's `/models` prices, cached daily in `.model-router/pricing-cache.json`. |
| `table` | Tokens × the built-in table ([`src/cost/prices.toml`](../src/cost/prices.toml), with an `as_of` date; harness aliases like `sonnet` map to the model they select). |
| `unknown` | No tokens or no price. |

Tokens appear only when a harness reports them; totals say how many attempts
reported. Interactive Claude Code and Codex stages read tokens from the
harness's session logs (`~/.claude/projects`, `~/.codex/sessions`), best
effort.

## Your prices

USD per 1M tokens:

```toml
# .model-router.toml or global config.toml
[pricing."my-model"]
input = 1.5
output = 6.0
cache_read = 0.15     # optional; default 0.1 × input
cache_write = 1.9     # optional; default 1.25 × input
```

In `<config dir>/pricing.toml` use `[models."my-model"]` with the same fields.

## Where it shows

- Run header and a finished run's brief: amounts with sources.
- `model-router report RUN_ID` (`--json` for data).
- `model-router cost [--since 7d] [--json]`: history summed by harness and
  model. `--since` accepts e.g. `12h`, `7d`, `2w`.
- `model-router stats`: reported tokens and cost per assignment.

## Budgets

```toml
[[workflows]]
name = "Feature development"
budget = { max_usd = 5.0, max_tokens = 2_000_000 }

[[workflows.stages]]
id = "implement"
budget = { max_usd = 2.0 }
```

- After each attempt, a run whose known cost or counted tokens exceed a run or
  stage cap stops, **blocked**, before its next stage. In a stage graph,
  stages already running side by side finish first.
- A running stage is never killed (live caps on streamed usage are future
  work).
- `max_tokens` counts input, output, and cache writes; cache reads are
  excluded. Unknown costs count only toward `max_tokens`.
- Continue: `model-router resume RUN_ID --budget-usd N` or `--budget-tokens N`
  sets the run's cap and any exceeded stage cap to `N`. In the TUI, `B` on a
  run a USD cap stopped asks for `N` and confirms first.
