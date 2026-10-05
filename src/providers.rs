//! Router-defined model providers: OpenAI-compatible endpoints that the router
//! wires into each harness's native provider mechanism (config overrides or
//! environment variables), like Codex `model_providers`, OpenCode `provider`,
//! and Crush `providers`. API keys are read from the named environment variable
//! at launch and never written to run artifacts.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{borrow::Cow, collections::BTreeMap, time::Duration};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Provider {
    /// OpenAI-compatible API root, e.g. `https://openrouter.ai/api/v1`.
    pub base_url: String,
    /// Environment variable holding the API key. Empty for keyless local servers.
    pub api_key_env: String,
    /// Anthropic Messages-compatible root. Required for Claude Code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anthropic_base_url: Option<String>,
    /// Model context window in tokens, for harnesses that need it to compact
    /// (OpenCode). Unset uses a conservative default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<u64>,
    /// Maximum output tokens per response, as for `context_limit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_limit: Option<u64>,
}

/// Conservative model limits when unset: an underestimate compacts early
/// instead of overflowing.
const DEFAULT_CONTEXT: u64 = 32_768;
const DEFAULT_OUTPUT: u64 = 8_192;

impl Provider {
    /// Effective `(context, output)` token limits. An omitted output limit is
    /// at most a quarter of the context, so usable context stays positive.
    pub fn limits(&self) -> (u64, u64) {
        let context = self.context_limit.unwrap_or(DEFAULT_CONTEXT);
        let output = self
            .output_limit
            .unwrap_or_else(|| (context / 4).clamp(1, DEFAULT_OUTPUT));
        (context, output)
    }
}

/// `(name, base_url, api_key_env, anthropic_base_url)`. On 2026-10-04 every
/// hosted URL answered unauthenticated probes (200/401 on `/models`, 401 on
/// Anthropic `/v1/messages`); no inference request was made. Perplexity has no
/// `/models` listing.
/// `openai` and `anthropic` are omitted: Codex and Claude Code reach them natively.
const BUILTINS: &[(&str, &str, &str, Option<&str>)] = &[
    (
        "openrouter",
        "https://openrouter.ai/api/v1",
        "OPENROUTER_API_KEY",
        Some("https://openrouter.ai/api"),
    ),
    (
        "vercel",
        "https://ai-gateway.vercel.sh/v1",
        "AI_GATEWAY_API_KEY",
        Some("https://ai-gateway.vercel.sh"),
    ),
    (
        "requesty",
        "https://router.requesty.ai/v1",
        "REQUESTY_API_KEY",
        None,
    ),
    (
        "google",
        "https://generativelanguage.googleapis.com/v1beta/openai",
        "GEMINI_API_KEY",
        None,
    ),
    ("xai", "https://api.x.ai/v1", "XAI_API_KEY", None),
    (
        "mistral",
        "https://api.mistral.ai/v1",
        "MISTRAL_API_KEY",
        None,
    ),
    (
        "deepseek",
        "https://api.deepseek.com/v1",
        "DEEPSEEK_API_KEY",
        Some("https://api.deepseek.com/anthropic"),
    ),
    (
        "moonshot",
        "https://api.moonshot.ai/v1",
        "MOONSHOT_API_KEY",
        Some("https://api.moonshot.ai/anthropic"),
    ),
    (
        "zai",
        "https://api.z.ai/api/paas/v4",
        "ZAI_API_KEY",
        Some("https://api.z.ai/api/anthropic"),
    ),
    (
        "minimax",
        "https://api.minimax.io/v1",
        "MINIMAX_API_KEY",
        Some("https://api.minimax.io/anthropic"),
    ),
    (
        "groq",
        "https://api.groq.com/openai/v1",
        "GROQ_API_KEY",
        None,
    ),
    (
        "cerebras",
        "https://api.cerebras.ai/v1",
        "CEREBRAS_API_KEY",
        None,
    ),
    (
        "together",
        "https://api.together.xyz/v1",
        "TOGETHER_API_KEY",
        None,
    ),
    (
        "fireworks",
        "https://api.fireworks.ai/inference/v1",
        "FIREWORKS_API_KEY",
        None,
    ),
    (
        "deepinfra",
        "https://api.deepinfra.com/v1/openai",
        "DEEPINFRA_API_KEY",
        None,
    ),
    (
        "huggingface",
        "https://router.huggingface.co/v1",
        "HF_TOKEN",
        None,
    ),
    (
        "nebius",
        "https://api.studio.nebius.com/v1",
        "NEBIUS_API_KEY",
        None,
    ),
    (
        "sambanova",
        "https://api.sambanova.ai/v1",
        "SAMBANOVA_API_KEY",
        None,
    ),
    (
        "perplexity",
        "https://api.perplexity.ai",
        "PERPLEXITY_API_KEY",
        None,
    ),
    (
        "ollama",
        "http://localhost:11434/v1",
        "",
        Some("http://localhost:11434"),
    ),
    ("lmstudio", "http://localhost:1234/v1", "", None),
];

/// Providers available without configuration. A `[providers.NAME]` entry with
/// the same name replaces the built-in definition.
pub fn builtins() -> BTreeMap<String, Provider> {
    BUILTINS
        .iter()
        .map(|(name, base_url, key, anthropic)| {
            (
                (*name).into(),
                Provider {
                    base_url: (*base_url).into(),
                    api_key_env: (*key).into(),
                    anthropic_base_url: anthropic.map(Into::into),
                    ..Provider::default()
                },
            )
        })
        .collect()
}

pub const NOTE: &str = "Set a stage's provider to one of these names. Add others with [providers.NAME] base_url/api_key_env in the workflow TOML. Built-in names route automatically only into Claude Code (needs an Anthropic-compatible URL). Other harnesses receive them unchanged, so their own provider definitions win; add an empty [providers.NAME] to route a built-in into codex (needs Responses API support), opencode, aider, goose, or custom adapters. Pi always uses its own registry. Cursor, Gemini, Amp, and Copilot use only their own accounts.";

/// The provider the router injects for `harness`, or None to pass the name to
/// the harness unchanged. `[providers.NAME]` entries route everywhere but Pi,
/// which has no runtime injection. Built-ins route only into Claude Code, which
/// has no provider registry; every other harness may define the same name
/// natively (Codex config layers and profiles, OpenCode/Aider/Goose
/// registries), so it gets the name unchanged unless the workflow opts in
/// with `[providers.NAME]`.
pub fn route<'a>(
    harness: &str,
    name: &str,
    configured: &'a BTreeMap<String, Provider>,
) -> Option<Cow<'a, Provider>> {
    match harness {
        "pi" => None,
        _ if configured.contains_key(name) => resolve(name, configured),
        "claude" => resolve(name, configured),
        _ => None,
    }
}

/// A configured entry, or else the built-in preset. A configured entry without
/// `base_url` opts into the built-in preset of that name; its other fields
/// override the preset's.
pub fn resolve<'a>(
    name: &str,
    configured: &'a BTreeMap<String, Provider>,
) -> Option<Cow<'a, Provider>> {
    if name.is_empty() {
        return None;
    }
    let preset = || builtins().remove(name);
    match configured.get(name) {
        Some(entry) if entry.base_url.is_empty() => {
            let preset = preset()?;
            Some(Cow::Owned(Provider {
                base_url: preset.base_url,
                api_key_env: if entry.api_key_env.is_empty() {
                    preset.api_key_env
                } else {
                    entry.api_key_env.clone()
                },
                anthropic_base_url: entry
                    .anthropic_base_url
                    .clone()
                    .or(preset.anthropic_base_url),
                ..entry.clone()
            }))
        }
        Some(entry) => Some(Cow::Borrowed(entry)),
        None => preset().map(Cow::Owned),
    }
}

/// Built-in and configured providers, configured definitions winning.
pub fn all(configured: &BTreeMap<String, Provider>) -> BTreeMap<String, Provider> {
    let mut providers = builtins();
    for name in configured.keys() {
        if let Some(provider) = resolve(name, configured) {
            providers.insert(name.clone(), provider.into_owned());
        }
    }
    providers
}

pub fn validate(name: &str, provider: &Provider) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')),
        "Provider names must contain only letters, digits, '-', '_' and '.'"
    );
    let (context, output) = provider.limits();
    ensure!(
        output > 0 && output < context,
        "Provider {name} needs 0 < output_limit < context_limit (effective {output} and {context})"
    );
    ensure!(
        !provider.base_url.is_empty() || builtins().contains_key(name),
        "Provider {name} needs base_url (only built-in names can omit it)"
    );
    let base = Some(&provider.base_url).filter(|url| !url.is_empty());
    for url in base.into_iter().chain(&provider.anthropic_base_url) {
        let parsed = reqwest::Url::parse(url)
            .with_context(|| format!("Provider {name} has an invalid URL: {url:?}"))?;
        ensure!(
            matches!(parsed.scheme(), "http" | "https") && parsed.host().is_some(),
            "Provider {name} URLs must be http:// or https://"
        );
    }
    ensure!(
        provider.api_key_env.is_empty()
            || !provider
                .api_key_env
                .starts_with(|c: char| c.is_ascii_digit())
                && provider
                    .api_key_env
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "Provider {name} api_key_env must name an environment variable"
    );
    Ok(())
}

/// Lists model IDs from the provider's OpenAI-compatible `GET /models`.
/// Sends the key when set; OpenRouter's listing is public. No model request.
pub fn models(provider: &Provider) -> Result<Vec<String>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let mut request = client.get(format!(
        "{}/models",
        provider.base_url.trim_end_matches('/')
    ));
    if let Some(key) = (!provider.api_key_env.is_empty())
        .then(|| std::env::var(&provider.api_key_env).ok())
        .flatten()
    {
        request = request.bearer_auth(key);
    }
    let response = request.send()?.error_for_status()?;
    let body: Value =
        serde_json::from_str(&response.text()?).context("Provider model list is not JSON")?;
    let mut ids: Vec<String> = body
        .get("data")
        .and_then(Value::as_array)
        .context("Provider model list has no data array")?
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(String::from))
        .collect();
    ids.sort();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configured_providers_replace_builtins() {
        assert_eq!(
            resolve("openrouter", &BTreeMap::new()).unwrap().api_key_env,
            "OPENROUTER_API_KEY"
        );
        let configured = BTreeMap::from([(
            "openrouter".into(),
            Provider {
                base_url: "http://localhost:4000/v1".into(),
                api_key_env: "PROXY_KEY".into(),
                ..Provider::default()
            },
        )]);
        assert_eq!(
            resolve("openrouter", &configured).unwrap().api_key_env,
            "PROXY_KEY"
        );
        assert!(resolve("openai", &configured).is_none());
        assert!(resolve("", &configured).is_none());
    }
    #[test]
    fn validation_rejects_bad_urls_and_env_names() {
        let good = builtins().remove("openrouter").unwrap();
        validate("openrouter", &good).unwrap();
        for bad in [
            Provider {
                base_url: "file:///etc".into(),
                ..good.clone()
            },
            Provider {
                api_key_env: "KEY; rm".into(),
                ..good.clone()
            },
        ] {
            assert!(validate("x", &bad).is_err());
        }
        assert!(validate("a/b", &good).is_err());
    }
    #[test]
    fn builtins_route_only_into_claude_unless_opted_in() {
        for (name, provider) in builtins() {
            validate(&name, &provider).unwrap();
        }
        let none = BTreeMap::new();
        assert!(route("claude", "deepseek", &none).is_some());
        for harness in ["codex", "opencode", "aider", "goose", "pi", "custom"] {
            assert!(route(harness, "openrouter", &none).is_none(), "{harness}");
        }
        // An empty entry opts into the preset; Pi still uses its registry.
        let configured = BTreeMap::from([("openrouter".to_string(), Provider::default())]);
        validate("openrouter", &configured["openrouter"]).unwrap();
        for harness in ["codex", "opencode", "aider", "goose", "custom"] {
            assert_eq!(
                route(harness, "openrouter", &configured).unwrap().as_ref(),
                &builtins()["openrouter"],
                "{harness}"
            );
        }
        assert!(route("pi", "openrouter", &configured).is_none());
        assert!(validate("corp", &Provider::default()).is_err());
    }
    #[test]
    fn preset_entries_keep_their_overrides() {
        let configured = BTreeMap::from([(
            "openrouter".to_string(),
            Provider {
                api_key_env: "TEAM_OPENROUTER_KEY".into(),
                context_limit: Some(200_000),
                ..Provider::default()
            },
        )]);
        let provider = resolve("openrouter", &configured).unwrap();
        assert_eq!(provider.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(provider.api_key_env, "TEAM_OPENROUTER_KEY");
        assert_eq!(provider.context_limit, Some(200_000));
        assert!(provider.anthropic_base_url.is_some());
        assert_eq!(all(&configured)["openrouter"], *provider);
    }
    #[test]
    fn limits_keep_usable_context_positive() {
        let with = |context, output| Provider {
            context_limit: context,
            output_limit: output,
            ..builtins()["openrouter"].clone()
        };
        assert_eq!(with(None, None).limits(), (32_768, 8_192));
        assert_eq!(with(Some(4096), None).limits(), (4096, 1024));
        assert_eq!(with(Some(200_000), None).limits(), (200_000, 8_192));
        validate("x", &with(Some(4096), None)).unwrap();
        for (context, output) in [
            (Some(0), None),
            (Some(4096), Some(4096)),
            (Some(4096), Some(0)),
            (None, Some(40_000)),
        ] {
            assert!(
                validate("x", &with(context, output)).is_err(),
                "{context:?} {output:?}"
            );
        }
    }
}
