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
#[serde(default)]
pub struct Provider {
    /// OpenAI-compatible API root, e.g. `https://openrouter.ai/api/v1`.
    pub base_url: String,
    /// Environment variable holding the API key. Empty for keyless local servers.
    pub api_key_env: String,
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

/// `(name, base_url, api_key_env)`. On 2026-10-04 every hosted URL answered
/// unauthenticated probes (200/401 on `/models`); no inference request was
/// made. Perplexity has no `/models` listing. `openai` is omitted: Codex
/// reaches it natively.
const BUILTINS: &[(&str, &str, &str)] = &[
    (
        "openrouter",
        "https://openrouter.ai/api/v1",
        "OPENROUTER_API_KEY",
    ),
    (
        "vercel",
        "https://ai-gateway.vercel.sh/v1",
        "AI_GATEWAY_API_KEY",
    ),
    (
        "requesty",
        "https://router.requesty.ai/v1",
        "REQUESTY_API_KEY",
    ),
    (
        "google",
        "https://generativelanguage.googleapis.com/v1beta/openai",
        "GEMINI_API_KEY",
    ),
    ("xai", "https://api.x.ai/v1", "XAI_API_KEY"),
    ("mistral", "https://api.mistral.ai/v1", "MISTRAL_API_KEY"),
    (
        "deepseek",
        "https://api.deepseek.com/v1",
        "DEEPSEEK_API_KEY",
    ),
    ("moonshot", "https://api.moonshot.ai/v1", "MOONSHOT_API_KEY"),
    ("zai", "https://api.z.ai/api/paas/v4", "ZAI_API_KEY"),
    ("minimax", "https://api.minimax.io/v1", "MINIMAX_API_KEY"),
    ("groq", "https://api.groq.com/openai/v1", "GROQ_API_KEY"),
    ("cerebras", "https://api.cerebras.ai/v1", "CEREBRAS_API_KEY"),
    (
        "together",
        "https://api.together.xyz/v1",
        "TOGETHER_API_KEY",
    ),
    (
        "fireworks",
        "https://api.fireworks.ai/inference/v1",
        "FIREWORKS_API_KEY",
    ),
    (
        "deepinfra",
        "https://api.deepinfra.com/v1/openai",
        "DEEPINFRA_API_KEY",
    ),
    (
        "huggingface",
        "https://router.huggingface.co/v1",
        "HF_TOKEN",
    ),
    (
        "nebius",
        "https://api.studio.nebius.com/v1",
        "NEBIUS_API_KEY",
    ),
    (
        "sambanova",
        "https://api.sambanova.ai/v1",
        "SAMBANOVA_API_KEY",
    ),
    (
        "perplexity",
        "https://api.perplexity.ai",
        "PERPLEXITY_API_KEY",
    ),
    ("ollama", "http://localhost:11434/v1", ""),
    ("lmstudio", "http://localhost:1234/v1", ""),
];

/// Providers available without configuration. A `[providers.NAME]` entry with
/// the same name replaces the built-in definition.
pub fn builtins() -> BTreeMap<String, Provider> {
    BUILTINS
        .iter()
        .map(|(name, base_url, key)| {
            (
                (*name).into(),
                Provider {
                    base_url: (*base_url).into(),
                    api_key_env: (*key).into(),
                    ..Provider::default()
                },
            )
        })
        .collect()
}

/// How router providers reach each harness, derived from the harness table.
pub fn note() -> String {
    crate::harness::provider_note()
}

/// The provider the router injects for `harness`, or None to pass the name to
/// the harness unchanged. Only `[providers.NAME]` entries route: harnesses may
/// define the same name natively (Codex config layers and profiles,
/// OpenCode/Aider/Goose registries), so built-in presets need an explicit
/// opt-in. Pi and omp have no runtime injection and always use their own
/// registries.
pub fn route<'a>(
    harness: &str,
    name: &str,
    configured: &'a BTreeMap<String, Provider>,
) -> Option<Cow<'a, Provider>> {
    let registry = crate::harness::get(harness)
        .is_some_and(|spec| spec.routing == crate::harness::Routing::Registry);
    if registry || !configured.contains_key(name) {
        return None;
    }
    resolve(name, configured)
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
    if !provider.base_url.is_empty() {
        let url = &provider.base_url;
        let parsed = reqwest::Url::parse(url)
            .with_context(|| format!("Provider {name} has an invalid URL: {url:?}"))?;
        ensure!(
            matches!(parsed.scheme(), "http" | "https") && parsed.host().is_some(),
            "Provider {name} base_url must be http:// or https://"
        );
        // base_url reaches Codex arguments, saved commands, and previews, so
        // it must carry no secret: keys belong in api_key_env.
        ensure!(
            parsed.username().is_empty() && parsed.password().is_none(),
            "Provider {name} base_url cannot embed credentials; name the key's variable in api_key_env"
        );
        ensure!(
            parsed.query().is_none() && parsed.fragment().is_none(),
            "Provider {name} base_url cannot have a query or fragment; name the key's variable in api_key_env"
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

/// One entry of a provider's `/models` listing. Context length and prices are
/// kept when the provider reports them (OpenRouter and similar gateways).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    /// USD per prompt token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_price: Option<f64>,
    /// USD per completion token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_price: Option<f64>,
}

fn model_info(entry: &Value) -> Option<ModelInfo> {
    // Prices arrive as decimal strings ("0.000003") or numbers.
    let number = |value: Option<&Value>| match value? {
        Value::String(text) => text.parse::<f64>().ok(),
        value => value.as_f64(),
    };
    Some(ModelInfo {
        id: entry.get("id")?.as_str()?.to_owned(),
        context_length: entry
            .get("context_length")
            .or_else(|| entry.get("context_window"))
            .and_then(Value::as_u64),
        prompt_price: number(entry.pointer("/pricing/prompt")),
        completion_price: number(entry.pointer("/pricing/completion")),
    })
}

/// Parses an OpenAI-compatible `/models` body, sorted by ID.
pub fn parse_models(body: &str) -> Result<Vec<ModelInfo>> {
    let body: Value = serde_json::from_str(body).context("Provider model list is not JSON")?;
    let mut models: Vec<ModelInfo> = body
        .get("data")
        .and_then(Value::as_array)
        .context("Provider model list has no data array")?
        .iter()
        .filter_map(model_info)
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

/// Lists models from the provider's OpenAI-compatible `GET /models`.
/// Sends the key when set; OpenRouter's listing is public. No model request.
pub fn models(provider: &Provider) -> Result<Vec<ModelInfo>> {
    models_within(provider, Duration::from_secs(15))
}

pub fn models_within(provider: &Provider, timeout: Duration) -> Result<Vec<ModelInfo>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
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
    parse_models(&response.text()?)
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
            Provider {
                base_url: "https://user:secret@gateway.example/v1".into(),
                ..good.clone()
            },
            Provider {
                base_url: "https://token@gateway.example/v1".into(),
                ..good.clone()
            },
            Provider {
                base_url: "https://gateway.example/v1?key=secret".into(),
                ..good.clone()
            },
            Provider {
                base_url: "https://gateway.example/v1#secret".into(),
                ..good.clone()
            },
        ] {
            assert!(validate("x", &bad).is_err());
        }
        assert!(validate("a/b", &good).is_err());
    }
    #[test]
    fn model_listings_keep_context_and_prices() {
        let body = r#"{"data":[
            {"id":"z/model","context_length":200000,"pricing":{"prompt":"0.000003","completion":"0.000015"}},
            {"id":"a/plain"},
            {"id":"m/numeric","context_window":8192,"pricing":{"prompt":0.5,"completion":1}},
            {"name":"no id"}
        ]}"#;
        let models = parse_models(body).unwrap();
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["a/plain", "m/numeric", "z/model"]
        );
        assert_eq!(
            models[0],
            ModelInfo {
                id: "a/plain".into(),
                ..ModelInfo::default()
            }
        );
        assert_eq!(models[1].context_length, Some(8192));
        assert_eq!(models[1].completion_price, Some(1.0));
        assert_eq!(models[2].context_length, Some(200_000));
        assert_eq!(models[2].prompt_price, Some(0.000003));
        assert!(parse_models("[]").is_err() && parse_models("nope").is_err());
    }
    #[test]
    fn builtins_route_only_when_opted_in() {
        for (name, provider) in builtins() {
            validate(&name, &provider).unwrap();
        }
        let none = BTreeMap::new();
        for harness in [
            "codex", "claude", "opencode", "aider", "goose", "pi", "custom",
        ] {
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
