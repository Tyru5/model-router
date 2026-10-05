use crate::config::{CustomAdapter, Mode, Stage};
use crate::harness_settings;
use crate::providers::{self, Provider};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputKind {
    Text,
    Codex,
    Claude,
    Pi,
    Cursor,
    Gemini,
    OpenCode,
    Amp,
}

/// An environment value for the harness. `Var` copies another variable at
/// launch so secrets never appear in an Invocation or run artifacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvValue {
    Literal(String),
    Var(String),
    /// Sets `path` to the JSON `value` inside the inherited JSON object of the
    /// same variable, replacing only that entry. Other inline config
    /// (permissions, providers) is kept in its original order.
    JsonInsert {
        path: Vec<String>,
        value: String,
    },
}

#[derive(Debug, Clone)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    pub stdin: Option<String>,
    pub kind: OutputKind,
    pub final_file: PathBuf,
    pub env: Vec<(String, EnvValue)>,
}
impl Invocation {
    pub fn display(&self) -> String {
        let env = self.env.iter().map(|(name, value)| match value {
            EnvValue::Literal(v) => format!("{name}={}", shell_words::quote(v)),
            EnvValue::Var(var) => format!("{name}=${var}"),
            EnvValue::JsonInsert { path, value } => {
                format!(
                    "{name}=${name}+{}={}",
                    path.join("."),
                    shell_words::quote(value)
                )
            }
        });
        env.chain(
            std::iter::once(self.program.as_str())
                .chain(self.args.iter().map(String::as_str))
                .map(|arg| shell_words::quote(arg).into_owned()),
        )
        .collect::<Vec<_>>()
        .join(" ")
    }
    /// Resolves `env` for launch; fails when a referenced variable is unset.
    pub fn environment(&self) -> Result<Vec<(String, String)>> {
        self.env
            .iter()
            .map(|(name, value)| {
                Ok((
                    name.clone(),
                    match value {
                        EnvValue::Literal(v) => v.clone(),
                        EnvValue::Var(var) => env::var(var)
                            .ok()
                            .filter(|v| !v.is_empty())
                            .with_context(|| format!("Set {var} to use this provider"))?,
                        EnvValue::JsonInsert { path, value } => {
                            json_insert(env::var(name).ok().as_deref(), path, value)
                                .with_context(|| format!("Add provider config to {name}"))?
                        }
                    },
                ))
            })
            .collect()
    }
}

pub fn executable(program: &str) -> Option<PathBuf> {
    fn usable(path: &Path) -> bool {
        if !path.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            path.metadata()
                .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            true
        }
    }
    let path = Path::new(program);
    if path.components().count() > 1 {
        return usable(path).then(|| path.to_path_buf());
    }
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join(program))
        .find(|p| usable(p))
}

pub fn executable_at(program: &str, project: &Path) -> Option<PathBuf> {
    let path = Path::new(program);
    if path.is_relative() && path.components().count() > 1 {
        return executable(project.join(path).to_str()?);
    }
    executable(program)
}

pub fn program(harness: &str, custom: &BTreeMap<String, CustomAdapter>) -> String {
    if let Some(adapter) = custom.get(harness) {
        return adapter.command.clone();
    }
    match harness {
        "cursor" if executable("cursor-agent").is_some() => "cursor-agent".into(),
        "cursor" => "agent".into(),
        other => other.into(),
    }
}

/// Harnesses that cannot be pointed at a router-defined provider.
const UNROUTABLE: &[&str] = &["claude", "cursor", "gemini", "amp", "copilot"];

pub fn validate_options(stage: &Stage, configured: &BTreeMap<String, Provider>) -> Result<()> {
    if providers::route(&stage.harness, &stage.provider, configured).is_some() {
        ensure!(
            !UNROUTABLE.contains(&stage.harness.as_str()),
            "{} cannot use router provider {}; it only uses its own account",
            stage.harness,
            stage.provider
        );
    } else if !stage.provider.is_empty() && !harness_settings::supports_provider(&stage.harness) {
        bail!(
            "{} uses its existing provider/auth configuration; leave the provider field empty",
            stage.harness
        );
    }
    if !stage.provider.is_empty() {
        ensure!(
            !stage.model.is_empty(),
            "A provider override requires a model"
        );
    }
    if !stage.effort.is_empty() && !harness_settings::supports_effort(&stage.harness) {
        bail!(
            "{} has no portable effort flag; use its native model/mode or explicit arguments instead",
            stage.harness
        );
    }
    harness_settings::validate(stage)
}

pub struct ContextPaths<'a> {
    pub workspace: &'a Path,
    pub attempt_dir: &'a Path,
    pub prompt_file: &'a Path,
    pub handoff_file: &'a Path,
}

pub fn build(
    stage: &Stage,
    custom: &BTreeMap<String, CustomAdapter>,
    configured: &BTreeMap<String, Provider>,
    prompt: &str,
    paths: &ContextPaths<'_>,
) -> Result<Invocation> {
    validate_options(stage, configured)?;
    let routed = providers::route(&stage.harness, &stage.provider, configured);
    let mut extra_args = harness_settings::arguments(stage)?;
    extra_args.extend(stage.extra_args.clone());
    let interactive = stage.mode == Mode::Interactive;
    let mut invocation = Invocation {
        program: program(&stage.harness, custom),
        args: vec![],
        stdin: None,
        kind: OutputKind::Text,
        final_file: paths.attempt_dir.join("final.txt"),
        env: vec![],
    };
    if let Some(provider) = &routed {
        invocation.env = route_env(stage, provider)?;
    }
    let a = &mut invocation.args;
    let mut append = |items: &[&str]| {
        a.extend(items.iter().map(|s| (*s).to_owned()));
    };
    let qualified = if stage.provider.is_empty() {
        stage.model.clone()
    } else if routed.is_some() && stage.harness == "opencode" {
        format!("{}/{}", router_provider_id(&stage.provider), stage.model)
    } else {
        format!("{}/{}", stage.provider, stage.model)
    };
    match stage.harness.as_str() {
        "codex" => {
            invocation.kind = OutputKind::Codex;
            if !interactive {
                append(&[
                    "exec",
                    "--json",
                    "--color",
                    "never",
                    "--skip-git-repo-check",
                    "--output-last-message",
                    invocation
                        .final_file
                        .to_str()
                        .context("Output path must be UTF-8")?,
                ]);
            }
            if !stage.model.is_empty() {
                append(&["--model", &stage.model]);
            }
            if let Some(provider) = &routed {
                // Isolated from native/reserved IDs (openai, ollama, …); dotted
                // names stay one TOML key.
                let id = router_provider_id(&stage.provider);
                let key = format!("model_providers.{id}");
                for (field, value) in [
                    ("name", &stage.provider),
                    ("base_url", &provider.base_url),
                    ("env_key", &provider.api_key_env),
                ] {
                    if value.is_empty() {
                        continue;
                    }
                    append(&[
                        "--config",
                        &format!("{key}.{field}={}", serde_json::to_string(value)?),
                    ]);
                }
                append(&[
                    "--config",
                    &format!("model_provider={}", serde_json::to_string(&id)?),
                ]);
            } else if !stage.provider.is_empty() {
                if matches!(stage.provider.as_str(), "ollama" | "lmstudio") {
                    append(&["--oss", "--local-provider", &stage.provider]);
                } else {
                    append(&[
                        "--config",
                        &format!("model_provider={}", serde_json::to_string(&stage.provider)?),
                    ]);
                }
            }
            if !stage.effort.is_empty() {
                append(&[
                    "--config",
                    &format!(
                        "model_reasoning_effort={}",
                        serde_json::to_string(&stage.effort)?
                    ),
                ]);
            }
            a.extend(extra_args.clone());
            if interactive {
                a.push(prompt.into());
            } else {
                a.push("-".into());
                invocation.stdin = Some(prompt.into());
            }
        }
        "claude" => {
            invocation.kind = OutputKind::Claude;
            if !interactive {
                append(&["--print", "--output-format", "json"]);
            }
            if !stage.model.is_empty() {
                append(&["--model", &stage.model]);
            }
            if !stage.effort.is_empty() {
                append(&["--effort", &stage.effort]);
            }
            a.extend(extra_args.clone());
            if interactive {
                a.push(prompt.into());
            } else {
                invocation.stdin = Some(prompt.into());
            }
        }
        "pi" => {
            invocation.kind = OutputKind::Pi;
            if !interactive {
                append(&["--print", "--mode", "json"]);
            }
            if !stage.model.is_empty() {
                append(&["--model", &stage.model]);
            }
            if !stage.provider.is_empty() {
                append(&["--provider", &stage.provider]);
            }
            if !stage.effort.is_empty() {
                append(&["--thinking", &stage.effort]);
            }
            a.extend(extra_args.clone());
            a.push(format!("@{}", paths.prompt_file.display()));
        }
        "cursor" => {
            invocation.kind = OutputKind::Cursor;
            if !interactive {
                append(&["--print", "--output-format", "json"]);
            }
            if !stage.model.is_empty() {
                append(&["--model", &stage.model]);
            }
            a.extend(extra_args.clone());
            a.push(prompt.into());
        }
        "gemini" => {
            invocation.kind = OutputKind::Gemini;
            if !stage.model.is_empty() {
                append(&["--model", &stage.model]);
            }
            if interactive {
                append(&["--prompt-interactive", prompt]);
            } else {
                append(&["--prompt", prompt, "--output-format", "json"]);
            }
            a.extend(extra_args.clone());
        }
        "opencode" => {
            invocation.kind = OutputKind::OpenCode;
            if interactive {
                append(&["run", "--interactive"]);
            } else {
                append(&["run", "--format", "json"]);
            }
            if !qualified.is_empty() {
                append(&["--model", &qualified]);
            }
            if !stage.effort.is_empty() {
                append(&["--variant", &stage.effort]);
            }
            a.extend(extra_args.clone());
            a.push(prompt.into());
        }
        "aider" => {
            if routed.is_some() {
                // LiteLLM's openai/ prefix targets OPENAI_API_BASE.
                append(&["--model", &format!("openai/{}", stage.model)]);
            } else if !qualified.is_empty() {
                append(&["--model", &qualified]);
            }
            append(&["--no-auto-commits", "--no-dirty-commits"]);
            if interactive {
                let commands = paths.attempt_dir.join("aider.commands");
                fs::write(
                    &commands,
                    "/code Complete the stage described in the attached model-router prompt file.\n",
                )?;
                append(&[
                    "--read",
                    paths
                        .prompt_file
                        .to_str()
                        .context("Prompt path must be UTF-8")?,
                    "--load",
                    commands.to_str().context("Command path must be UTF-8")?,
                ]);
            } else {
                append(&[
                    "--message-file",
                    paths
                        .prompt_file
                        .to_str()
                        .context("Prompt path must be UTF-8")?,
                ]);
            }
            a.extend(extra_args.clone());
        }
        "goose" => {
            append(&["run", "--text", prompt]);
            if interactive {
                append(&["--interactive"]);
            }
            if !stage.model.is_empty() {
                append(&["--model", &stage.model]);
            }
            if routed.is_some() {
                append(&["--provider", "openai"]);
            } else if !stage.provider.is_empty() {
                append(&["--provider", &stage.provider]);
            }
            a.extend(extra_args.clone());
        }
        "amp" => {
            invocation.kind = OutputKind::Amp;
            if !stage.model.is_empty() {
                append(&["--mode", &stage.model]);
            }
            if !interactive {
                append(&["--execute", "--stream-json"]);
            }
            a.extend(extra_args.clone());
            if interactive {
                a.push(prompt.into());
            } else {
                invocation.stdin = Some(prompt.into());
            }
        }
        "copilot" => {
            if !stage.model.is_empty() {
                append(&["--model", &stage.model]);
            }
            append(&[
                if interactive {
                    "--interactive"
                } else {
                    "--prompt"
                },
                prompt,
            ]);
            a.extend(extra_args.clone());
        }
        name => {
            let adapter = custom
                .get(name)
                .with_context(|| format!("Unknown adapter {name}"))?;
            let args = if interactive {
                adapter
                    .interactive_args
                    .as_ref()
                    .with_context(|| format!("{name} does not define interactive_args"))?
            } else {
                &adapter.args
            };
            let values = [
                ("{prompt}", prompt.to_owned()),
                ("{model}", stage.model.clone()),
                ("{provider}", stage.provider.clone()),
                ("{effort}", stage.effort.clone()),
                ("{prompt_file}", paths.prompt_file.display().to_string()),
                ("{handoff_file}", paths.handoff_file.display().to_string()),
                ("{workspace}", paths.workspace.display().to_string()),
                ("{run_dir}", paths.attempt_dir.display().to_string()),
                (
                    "{base_url}",
                    routed
                        .as_ref()
                        .map(|p| p.base_url.clone())
                        .unwrap_or_default(),
                ),
                (
                    "{api_key_env}",
                    routed
                        .as_ref()
                        .map(|p| p.api_key_env.clone())
                        .unwrap_or_default(),
                ),
            ];
            a.extend(args.iter().map(|arg| expand(arg, &values)));
            a.extend(extra_args.clone());
            if adapter.prompt_stdin && !interactive {
                invocation.stdin = Some(prompt.into());
            }
        }
    }
    Ok(invocation)
}

/// Sets `path` in `base` (an inherited JSON object, if any) to `value`,
/// replacing whatever was there. Key order elsewhere is preserved.
fn json_insert(base: Option<&str>, path: &[String], value: &str) -> Result<String> {
    let mut root = match base.filter(|b| !b.trim().is_empty()) {
        Some(text) => serde_json::from_str(text).context("Inherited value is not JSON")?,
        None => serde_json::json!({}),
    };
    let (last, parents) = path.split_last().context("Empty JSON path")?;
    let mut node = &mut root;
    for key in parents {
        let object = node
            .as_object_mut()
            .with_context(|| format!("Inherited {key} parent is not a JSON object"))?;
        node = object
            .entry(key.clone())
            .or_insert_with(|| serde_json::json!({}));
    }
    node.as_object_mut()
        .context("Inherited value is not a JSON object")?
        .insert(last.clone(), serde_json::from_str(value)?);
    Ok(root.to_string())
}

/// Harness-side ID for a router provider: `model-router-NAME` with
/// characters other than letters, digits, '-' and '_' replaced by '-'. A
/// router-owned ID never merges with a native or reserved provider entry, so
/// no native endpoint, credential, or header leaks in.
fn router_provider_id(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("model-router-{safe}")
}

/// Environment that points a harness at a router-defined provider. The key
/// variable is always listed so a missing key fails before launch.
fn route_env(stage: &Stage, provider: &Provider) -> Result<Vec<(String, EnvValue)>> {
    let keyless = provider.api_key_env.is_empty();
    // Local servers ignore the key, but clients refuse to send an empty one.
    let key = || {
        if keyless {
            EnvValue::Literal("none".into())
        } else {
            EnvValue::Var(provider.api_key_env.clone())
        }
    };
    let literal = |v: &str| EnvValue::Literal(v.into());
    let mut env = vec![];
    if !keyless {
        env.push((provider.api_key_env.clone(), key()));
    }
    match stage.harness.as_str() {
        "opencode" => {
            let mut options = serde_json::json!({ "baseURL": &provider.base_url });
            if !keyless {
                options["apiKey"] = format!("{{env:{}}}", provider.api_key_env).into();
            }
            // A fresh ID shares no inherited options, headers, or stored auth.
            let (context, output) = provider.limits();
            let entry = serde_json::json!({
                "npm": "@ai-sdk/openai-compatible",
                "name": &stage.provider,
                "options": options,
                // The isolated ID has no catalog entry; without limits OpenCode
                // treats the context as 0 and never compacts.
                "models": { &stage.model: { "limit": {
                    "context": context,
                    "output": output,
                }}},
            });
            env.push((
                "OPENCODE_CONFIG_CONTENT".into(),
                EnvValue::JsonInsert {
                    path: vec!["provider".into(), router_provider_id(&stage.provider)],
                    value: entry.to_string(),
                },
            ));
        }
        "aider" => env.extend([
            ("OPENAI_API_BASE".into(), literal(&provider.base_url)),
            ("OPENAI_API_KEY".into(), key()),
        ]),
        // Goose's OpenAI provider splits the URL into host and path.
        "goose" => {
            let url = reqwest::Url::parse(&provider.base_url)?;
            let host = url.origin().ascii_serialization();
            let path = url.path().trim_matches('/');
            let path = if path.is_empty() {
                "chat/completions".into()
            } else {
                format!("{path}/chat/completions")
            };
            env.extend([
                ("OPENAI_HOST".into(), literal(&host)),
                ("OPENAI_BASE_PATH".into(), literal(&path)),
                ("OPENAI_API_KEY".into(), key()),
            ]);
        }
        _ => {}
    }
    Ok(env)
}

/// Expand only tokens present in the template, never tokens introduced by user input.
fn expand(template: &str, values: &[(&str, String)]) -> String {
    let mut output = String::new();
    let mut rest = template;
    while !rest.is_empty() {
        if let Some((key, value)) = values.iter().find(|(key, _)| rest.starts_with(*key)) {
            output.push_str(value);
            rest = &rest[key.len()..];
        } else {
            let c = rest.chars().next().unwrap();
            output.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn templates_do_not_recursively_interpolate_prompt() {
        assert_eq!(
            expand(
                "x {prompt} {model}",
                &[
                    ("{prompt}", "literal {model} $(touch x)".into()),
                    ("{model}", "chosen".into())
                ]
            ),
            "x literal {model} $(touch x) chosen"
        );
    }
    #[test]
    fn amp_uses_mode_not_model() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ContextPaths {
            workspace: dir.path(),
            attempt_dir: dir.path(),
            prompt_file: dir.path(),
            handoff_file: dir.path(),
        };
        let i = build(
            &Stage {
                harness: "amp".into(),
                model: "high".into(),
                ..Stage::default()
            },
            &BTreeMap::new(),
            &BTreeMap::new(),
            "task",
            &paths,
        )
        .unwrap();
        assert!(i.args.windows(2).any(|a| a == ["--mode", "high"]));
        assert!(!i.args.iter().any(|a| a == "--model"));
    }

    fn routed(harness: &str, provider: &str, mode: Mode) -> Result<Invocation> {
        routed_with(harness, provider, mode, &BTreeMap::new())
    }
    fn routed_with(
        harness: &str,
        provider: &str,
        mode: Mode,
        configured: &BTreeMap<String, Provider>,
    ) -> Result<Invocation> {
        let dir = tempfile::tempdir().unwrap();
        let paths = ContextPaths {
            workspace: dir.path(),
            attempt_dir: dir.path(),
            prompt_file: dir.path(),
            handoff_file: dir.path(),
        };
        build(
            &Stage {
                harness: harness.into(),
                provider: provider.into(),
                model: "vendor/model".into(),
                mode,
                ..Stage::default()
            },
            &BTreeMap::new(),
            configured,
            "task",
            &paths,
        )
    }
    fn env<'a>(i: &'a Invocation, name: &str) -> Option<&'a EnvValue> {
        i.env.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    #[test]
    fn router_providers_use_native_mechanisms_without_secrets() {
        let key = EnvValue::Var("OPENROUTER_API_KEY".into());
        // Codex passes built-in names through to its own config unless the
        // workflow opts in with an empty [providers.NAME].
        let native = routed("codex", "openrouter", Mode::Unattended).unwrap();
        assert!(native.env.is_empty());
        assert!(
            native
                .args
                .iter()
                .any(|a| a == "model_provider=\"openrouter\"")
        );
        let opt_in = BTreeMap::from([("openrouter".to_string(), Provider::default())]);
        let codex = routed_with("codex", "openrouter", Mode::Unattended, &opt_in).unwrap();
        for arg in [
            "model_providers.model-router-openrouter.base_url=\"https://openrouter.ai/api/v1\"",
            "model_providers.model-router-openrouter.env_key=\"OPENROUTER_API_KEY\"",
            "model_provider=\"model-router-openrouter\"",
        ] {
            assert!(
                codex.args.iter().any(|a| a == arg),
                "{arg}: {:?}",
                codex.args
            );
        }
        assert_eq!(env(&codex, "OPENROUTER_API_KEY"), Some(&key));

        assert!(routed("claude", "openrouter", Mode::Unattended).is_err());
        assert!(routed("cursor", "openrouter", Mode::Unattended).is_err());

        // Native registries keep built-in names; codex keeps --oss for Ollama.
        let opencode = routed("opencode", "openrouter", Mode::Unattended).unwrap();
        assert!(opencode.env.is_empty());
        assert!(opencode.args.iter().any(|a| a == "openrouter/vendor/model"));
        let ollama = routed("codex", "ollama", Mode::Unattended).unwrap();
        assert!(ollama.args.iter().any(|a| a == "--oss") && ollama.env.is_empty());
    }

    #[test]
    fn configured_providers_route_into_registry_harnesses() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ContextPaths {
            workspace: dir.path(),
            attempt_dir: dir.path(),
            prompt_file: dir.path(),
            handoff_file: dir.path(),
        };
        let configured = BTreeMap::from([(
            "proxy".to_string(),
            Provider {
                base_url: "http://localhost:4000/v1".into(),
                api_key_env: "PROXY_KEY".into(),
                ..Provider::default()
            },
        )]);
        let stage = |harness: &str| Stage {
            harness: harness.into(),
            provider: "proxy".into(),
            model: "m".into(),
            ..Stage::default()
        };
        let build = |harness| build(&stage(harness), &BTreeMap::new(), &configured, "t", &paths);
        let goose = build("goose").unwrap();
        assert_eq!(
            env(&goose, "OPENAI_HOST"),
            Some(&EnvValue::Literal("http://localhost:4000".into()))
        );
        assert_eq!(
            env(&goose, "OPENAI_BASE_PATH"),
            Some(&EnvValue::Literal("v1/chat/completions".into()))
        );
        assert!(goose.args.windows(2).any(|a| a == ["--provider", "openai"]));
        let aider = build("aider").unwrap();
        assert!(aider.args.windows(2).any(|a| a == ["--model", "openai/m"]));
        assert_eq!(
            env(&aider, "OPENAI_API_KEY"),
            Some(&EnvValue::Var("PROXY_KEY".into()))
        );
        let opencode = build("opencode").unwrap();
        let Some(EnvValue::JsonInsert { path, value }) = env(&opencode, "OPENCODE_CONFIG_CONTENT")
        else {
            panic!()
        };
        assert_eq!(path, &["provider", "model-router-proxy"]);
        let entry: serde_json::Value = serde_json::from_str(value).unwrap();
        assert_eq!(entry["options"]["apiKey"], "{env:PROXY_KEY}");
        assert_eq!(entry["models"]["m"]["limit"]["context"], 32_768);
        assert_eq!(entry["models"]["m"]["limit"]["output"], 8_192);
        assert!(opencode.args.iter().any(|a| a == "model-router-proxy/m"));

        let missing = Invocation {
            env: vec![(
                "K".into(),
                EnvValue::Var("MODEL_ROUTER_TEST_UNSET_KEY".into()),
            )],
            ..goose
        };
        assert!(missing.environment().is_err());
    }

    #[test]
    fn custom_adapters_route_only_configured_providers() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ContextPaths {
            workspace: dir.path(),
            attempt_dir: dir.path(),
            prompt_file: dir.path(),
            handoff_file: dir.path(),
        };
        let custom = BTreeMap::from([(
            "mine".to_string(),
            CustomAdapter {
                command: "mine".into(),
                args: vec![
                    "--provider={provider}".into(),
                    "--base={base_url}".into(),
                    "--key={api_key_env}".into(),
                ],
                interactive_args: None,
                models_args: None,
                prompt_stdin: false,
            },
        )]);
        let configured = BTreeMap::from([(
            "proxy".to_string(),
            Provider {
                base_url: "http://localhost:4000/v1".into(),
                api_key_env: "PROXY_KEY".into(),
                ..Provider::default()
            },
        )]);
        let build = |provider: &str| {
            let stage = Stage {
                harness: "mine".into(),
                provider: provider.into(),
                model: "m".into(),
                ..Stage::default()
            };
            build(&stage, &custom, &configured, "t", &paths).unwrap()
        };

        // Built-in names pass through as text: no env, no key requirement.
        let groq = build("groq");
        assert!(groq.env.is_empty(), "{:?}", groq.env);
        assert_eq!(groq.args, ["--provider=groq", "--base=", "--key="]);

        let proxy = build("proxy");
        assert_eq!(
            proxy.env,
            [("PROXY_KEY".to_string(), EnvValue::Var("PROXY_KEY".into()))]
        );
        assert_eq!(
            proxy.args,
            [
                "--provider=proxy",
                "--base=http://localhost:4000/v1",
                "--key=PROXY_KEY"
            ]
        );
    }

    #[test]
    fn codex_uses_isolated_ids_for_reserved_dotted_and_keyless_providers() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ContextPaths {
            workspace: dir.path(),
            attempt_dir: dir.path(),
            prompt_file: dir.path(),
            handoff_file: dir.path(),
        };
        let gateway = Provider {
            base_url: "http://gateway.local/v1".into(),
            api_key_env: String::new(),
            ..Provider::default()
        };
        for (name, id) in [
            ("openai", "model-router-openai"),
            ("ollama", "model-router-ollama"),
            ("corp.gateway", "model-router-corp-gateway"),
        ] {
            let configured = BTreeMap::from([(name.to_string(), gateway.clone())]);
            let stage = Stage {
                harness: "codex".into(),
                provider: name.into(),
                model: "m".into(),
                ..Stage::default()
            };
            let i = build(&stage, &BTreeMap::new(), &configured, "t", &paths).unwrap();
            let provider_args: Vec<_> = i
                .args
                .iter()
                .filter(|a| a.starts_with("model_provider"))
                .collect();
            assert!(
                provider_args
                    .iter()
                    .all(|a| a.starts_with(&format!("model_providers.{id}."))
                        || **a == format!("model_provider=\"{id}\"")),
                "{name}: {provider_args:?}"
            );
            assert!(
                !i.args
                    .iter()
                    .any(|a| a == "--oss" || a.contains(".env_key=")),
                "{name}"
            );
        }
    }

    #[test]
    fn opencode_provider_entry_keeps_inherited_config_order_and_shares_no_auth() {
        // Last-match permission rules: order is semantics.
        let inherited = r#"{"permission":{"bash":{"*":"ask","git *":"allow","* --force*":"deny"}},"provider":{"model-router-proxy":{"options":{"apiKey":"OLD","headers":{"Authorization":"x"}}},"other":{"npm":"x"}}}"#;
        let path = ["provider".to_string(), "model-router-proxy".to_string()];
        let keyless = r#"{"npm":"@ai-sdk/openai-compatible","options":{"baseURL":"http://h/v1"}}"#;
        let text = json_insert(Some(inherited), &path, keyless).unwrap();
        assert!(
            text.starts_with(
                r#"{"permission":{"bash":{"*":"ask","git *":"allow","* --force*":"deny"}}"#
            ),
            "{text}"
        );
        let merged: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(merged["provider"]["other"]["npm"], "x");
        let options = &merged["provider"]["model-router-proxy"]["options"];
        assert_eq!(options["baseURL"], "http://h/v1");
        assert!(options.get("apiKey").is_none() && options.get("headers").is_none());
        assert!(json_insert(None, &path, "{}").is_ok());
        assert!(json_insert(Some("not json"), &path, "{}").is_err());
        assert!(json_insert(Some("[1]"), &path, "{}").is_err());
        assert!(json_insert(Some(r#"{"provider":3}"#), &path, "{}").is_err());

        // Keyless routes emit no apiKey of their own either.
        let dir = tempfile::tempdir().unwrap();
        let paths = ContextPaths {
            workspace: dir.path(),
            attempt_dir: dir.path(),
            prompt_file: dir.path(),
            handoff_file: dir.path(),
        };
        let configured = BTreeMap::from([(
            "local".to_string(),
            Provider {
                base_url: "http://localhost:8000/v1".into(),
                ..Provider::default()
            },
        )]);
        let stage = Stage {
            harness: "opencode".into(),
            provider: "local".into(),
            model: "m".into(),
            ..Stage::default()
        };
        let i = build(&stage, &BTreeMap::new(), &configured, "t", &paths).unwrap();
        let Some(EnvValue::JsonInsert { value, .. }) = env(&i, "OPENCODE_CONFIG_CONTENT") else {
            panic!()
        };
        assert!(!value.contains("apiKey"), "{value}");
    }
}
