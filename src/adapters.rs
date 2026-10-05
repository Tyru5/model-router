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
    /// Removes an inherited variable.
    Unset,
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
            EnvValue::Unset => format!("-u {name}"),
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
    /// Resolves `env` for launch (None removes the variable); fails when a
    /// referenced variable is unset.
    pub fn environment(&self) -> Result<Vec<(String, Option<String>)>> {
        self.env
            .iter()
            .map(|(name, value)| {
                Ok((
                    name.clone(),
                    match value {
                        EnvValue::Unset => return Ok((name.clone(), None)),
                        EnvValue::Literal(v) => v.clone(),
                        EnvValue::Var(var) => env::var(var)
                            .ok()
                            .filter(|v| !v.is_empty())
                            .with_context(|| format!("Set {var} to use this provider"))?,
                        EnvValue::JsonInsert { path, value } => {
                            json_insert(env::var(name).ok().as_deref(), path, value)
                                .with_context(|| format!("Add provider config to {name}"))?
                        }
                    }
                    .into(),
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
const UNROUTABLE: &[&str] = &["cursor", "gemini", "amp", "copilot"];

pub fn validate_options(stage: &Stage, configured: &BTreeMap<String, Provider>) -> Result<()> {
    if let Some(provider) = providers::route(&stage.harness, &stage.provider, configured) {
        ensure!(
            !UNROUTABLE.contains(&stage.harness.as_str()),
            "{} cannot use router provider {}; it only uses its own account",
            stage.harness,
            stage.provider
        );
        ensure!(
            stage.harness != "claude" || provider.anthropic_base_url.is_some(),
            "Claude Code needs an Anthropic-compatible endpoint; set anthropic_base_url for provider {}",
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
            if let Some(provider) = &routed {
                ensure!(
                    !extra_args
                        .iter()
                        .any(|a| a == "--settings" || a.starts_with("--settings=")),
                    "A routed Claude stage sets --settings itself; remove it from Extra arguments"
                );
                claude_settings_conflicts(paths.workspace)?;
                append(&["--settings", &claude_route_settings(provider)?]);
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

/// Inherited Claude Code variables removed for a routed launch: headers that
/// could carry another gateway's credentials, a socket relay that bypasses the
/// base URL, an inherited API-key descriptor, and every backend selector in
/// Claude Code 2.1.289 (each would bypass `ANTHROPIC_BASE_URL`).
const CLAUDE_ROUTE_CONFLICTS: &[&str] = &[
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_UNIX_SOCKET",
    "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_GATEWAY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_VERTEX",
];
/// Key file Claude reads on remote hosts when no other API key is set.
const CLAUDE_REMOTE_API_KEY: &str = "/home/claude/.claude/remote/.api_key";
/// Claude settings that would replace the routed credential. `--settings`
/// cannot override them without putting the secret in arguments.
const CLAUDE_CREDENTIAL_ENV: &[&str] = &["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"];

/// `--settings` JSON for a routed Claude launch. Command-line settings outrank
/// user and project settings, so their `env` cannot move the endpoint, restore
/// headers, or select another backend. Contains no secrets.
fn claude_route_settings(provider: &Provider) -> Result<String> {
    let url = provider
        .anthropic_base_url
        .as_deref()
        .context("Provider has no anthropic_base_url")?;
    let mut env = serde_json::Map::new();
    env.insert("ANTHROPIC_BASE_URL".into(), url.into());
    for name in CLAUDE_ROUTE_CONFLICTS {
        env.insert((*name).into(), "".into());
    }
    Ok(serde_json::json!({ "env": env }).to_string())
}

/// Fails when Claude configuration could undo routing; see
/// [`claude_conflicts_in`]. Unit tests skip user and system configuration.
fn claude_settings_conflicts(workspace: &Path) -> Result<()> {
    let user = (!cfg!(test))
        .then(
            || match env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from) {
                Some(dir) => Some((dir.clone(), dir.join(".claude.json"))),
                None => directories::BaseDirs::new().map(|d| {
                    (
                        d.home_dir().join(".claude"),
                        d.home_dir().join(".claude.json"),
                    )
                }),
            },
        )
        .flatten()
        // Claude prefers a legacy `.config.json` in its config dir when present.
        .map(|(dir, global)| (dir.join(".config.json"), dir, global))
        .map(|(legacy, dir, global)| (dir, vec![legacy, global]));
    claude_conflicts_in(workspace, user, !cfg!(test))
}

/// Checks, failing closed:
/// - project settings in `workspace`, its ancestors, and the main checkout of
///   a Git worktree, plus user settings: no credential (`env` key or
///   `apiKeyHelper`);
/// - the user's global config (legacy `.config.json` and `.claude.json`) and
///   the remote-host key file: no saved key, which Claude sends alongside
///   the routed token;
/// - managed settings, which outrank `--settings`: no routed variable or
///   `policyHelper`, and no macOS MDM policy, whose content is not inspected.
fn claude_conflicts_in(
    workspace: &Path,
    user: Option<(PathBuf, Vec<PathBuf>)>,
    system: bool,
) -> Result<()> {
    let conflict = |what: String| {
        anyhow::anyhow!(
            "{what}, which would override or leak alongside the router provider. Remove it or leave this stage's provider blank."
        )
    };
    let main_checkout = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(workspace)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|dir| Path::new(dir.trim()).parent().map(Path::to_path_buf));
    let mut files: Vec<(PathBuf, bool)> = workspace
        .ancestors()
        .chain(main_checkout.as_deref())
        .flat_map(|dir| {
            ["settings.json", "settings.local.json"]
                .map(|name| (dir.join(".claude").join(name), false))
        })
        .collect();
    if let Some((dir, globals)) = &user {
        files.push((dir.join("settings.json"), false));
        for global in globals {
            let Ok(text) = fs::read_to_string(global) else {
                continue;
            };
            let config: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            if config
                .get("primaryApiKey")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|key| !key.is_empty())
            {
                return Err(conflict(format!(
                    "Claude config {} has a saved API key (primaryApiKey)",
                    global.display()
                )));
            }
        }
    }
    if system {
        let remote = Path::new(CLAUDE_REMOTE_API_KEY);
        if fs::metadata(remote).is_ok_and(|m| m.len() > 0) {
            return Err(conflict(format!(
                "Claude remote API key {} exists",
                remote.display()
            )));
        }
    }
    if system {
        let mdm = Path::new("/Library/Managed Preferences/com.anthropic.claudecode.plist");
        if mdm.exists() {
            return Err(conflict(format!(
                "macOS MDM policy {} applies",
                mdm.display()
            )));
        }
        for dir in [
            "/etc/claude-code",
            "/Library/Application Support/ClaudeCode",
        ] {
            let dir = Path::new(dir);
            files.push((dir.join("managed-settings.json"), true));
            if let Ok(entries) = fs::read_dir(dir.join("managed-settings.d")) {
                files.extend(entries.flatten().map(|e| (e.path(), true)));
            }
        }
    }
    for (path, managed) in files {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let settings: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("Claude settings {} are not JSON", path.display()))?;
        let env = settings.get("env").and_then(serde_json::Value::as_object);
        let set = |name: &str| env.is_some_and(|env| env.contains_key(name));
        let has = |key: &str| settings.get(key).is_some();
        let found = CLAUDE_CREDENTIAL_ENV
            .iter()
            .copied()
            .find(|name| set(name))
            .or_else(|| has("apiKeyHelper").then_some("apiKeyHelper"))
            .or_else(|| {
                managed
                    .then(|| {
                        std::iter::once("ANTHROPIC_BASE_URL")
                            .chain(CLAUDE_ROUTE_CONFLICTS.iter().copied())
                            .find(|name| set(name))
                            .or_else(|| has("policyHelper").then_some("policyHelper"))
                    })
                    .flatten()
            });
        if let Some(name) = found {
            return Err(conflict(format!(
                "Claude settings {} set {name}",
                path.display()
            )));
        }
    }
    Ok(())
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
        // OpenRouter's Claude Code integration: bearer token, empty API key.
        "claude" => {
            let url = provider
                .anthropic_base_url
                .as_deref()
                .context("Provider has no anthropic_base_url")?;
            env.extend([
                ("ANTHROPIC_BASE_URL".into(), literal(url)),
                ("ANTHROPIC_AUTH_TOKEN".into(), key()),
                ("ANTHROPIC_API_KEY".into(), literal("")),
            ]);
            // Inherited headers would carry another gateway's credentials to
            // this endpoint; backend selectors would bypass it entirely.
            env.extend(
                CLAUDE_ROUTE_CONFLICTS
                    .iter()
                    .map(|n| ((*n).into(), EnvValue::Unset)),
            );
        }
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

        let claude = routed("claude", "openrouter", Mode::Interactive).unwrap();
        assert_eq!(
            env(&claude, "ANTHROPIC_BASE_URL"),
            Some(&EnvValue::Literal("https://openrouter.ai/api".into()))
        );
        assert_eq!(env(&claude, "ANTHROPIC_AUTH_TOKEN"), Some(&key));
        assert_eq!(
            env(&claude, "ANTHROPIC_API_KEY"),
            Some(&EnvValue::Literal(String::new()))
        );
        assert!(
            claude
                .display()
                .contains("ANTHROPIC_AUTH_TOKEN=$OPENROUTER_API_KEY")
        );
        for name in [
            "ANTHROPIC_CUSTOM_HEADERS",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ] {
            assert_eq!(env(&claude, name), Some(&EnvValue::Unset), "{name}");
        }
        let unattended = routed("claude", "openrouter", Mode::Unattended).unwrap();
        assert_eq!(unattended.env, claude.env);
        assert!(
            claude
                .environment()
                .is_ok_and(|e| e.contains(&("CLAUDE_CODE_USE_BEDROCK".into(), None)))
                || std::env::var_os("OPENROUTER_API_KEY").is_none()
        );
        assert!(routed("claude", "groq", Mode::Unattended).is_err());
        assert!(routed("claude", "openai", Mode::Unattended).is_err());
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
                anthropic_base_url: None,
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
                anthropic_base_url: None,
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
            anthropic_base_url: None,
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

    #[test]
    fn routed_claude_pins_settings_and_rejects_credential_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("repo");
        fs::create_dir_all(workspace.join(".claude")).unwrap();
        let paths = ContextPaths {
            workspace: &workspace,
            attempt_dir: dir.path(),
            prompt_file: dir.path(),
            handoff_file: dir.path(),
        };
        let stage = |extra: &[&str]| Stage {
            harness: "claude".into(),
            provider: "openrouter".into(),
            model: "m".into(),
            extra_args: extra.iter().map(|a| (*a).into()).collect(),
            ..Stage::default()
        };
        let none = BTreeMap::new();
        let build_with = |stage: &Stage| build(stage, &BTreeMap::new(), &none, "t", &paths);

        // Project env that only moves the endpoint is outranked by --settings.
        fs::write(
            workspace.join(".claude/settings.json"),
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://old.gateway","CLAUDE_CODE_USE_MANTLE":"1"}}"#,
        )
        .unwrap();
        let i = build_with(&stage(&[])).unwrap();
        let at = i.args.iter().position(|a| a == "--settings").unwrap();
        let settings: serde_json::Value = serde_json::from_str(&i.args[at + 1]).unwrap();
        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"],
            "https://openrouter.ai/api"
        );
        for name in CLAUDE_ROUTE_CONFLICTS {
            assert_eq!(settings["env"][name], "", "{name}");
            assert_eq!(env(&i, name), Some(&EnvValue::Unset), "{name}");
        }
        assert!(
            !["ANTHROPIC_AUTH_TOKEN", "OPENROUTER_API_KEY"]
                .iter()
                .any(|k| i.args[at + 1].contains(k))
        );
        assert!(build_with(&stage(&["--settings", "{}"])).is_err());

        // Credentials in any layer, here a parent's local settings, are rejected.
        for (file, body) in [
            (
                "settings.local.json",
                r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"old"}}"#,
            ),
            ("settings.json", r#"{"env":{"ANTHROPIC_API_KEY":"old"}}"#),
            ("settings.json", r#"{"apiKeyHelper":"print-key"}"#),
        ] {
            fs::create_dir_all(dir.path().join(".claude")).unwrap();
            fs::write(dir.path().join(".claude").join(file), body).unwrap();
            let error = build_with(&stage(&[])).unwrap_err().to_string();
            assert!(
                error.contains("would override or leak alongside the router provider"),
                "{error}"
            );
            fs::remove_dir_all(dir.path().join(".claude")).unwrap();
        }
        // Unrouted Claude stages ignore those settings.
        fs::write(
            workspace.join(".claude/settings.json"),
            r#"{"apiKeyHelper":"x"}"#,
        )
        .unwrap();
        let native = Stage {
            provider: String::new(),
            ..stage(&[])
        };
        assert!(build_with(&native).is_ok());
    }

    #[test]
    fn claude_conflicts_cover_saved_keys_and_sibling_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str], cwd: &Path| {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(cwd)
                    .output()
                    .unwrap()
                    .status
                    .success()
            )
        };
        let main = dir.path().join("main");
        fs::create_dir_all(&main).unwrap();
        git(&["init", "-q"], &main);
        git(
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "i",
            ],
            &main,
        );
        let sibling = dir.path().join("sibling");
        git(&["worktree", "add", "-q", sibling.to_str().unwrap()], &main);
        claude_conflicts_in(&sibling, None, false).unwrap();
        fs::create_dir_all(main.join(".claude")).unwrap();
        fs::write(
            main.join(".claude/settings.local.json"),
            r#"{"env":{"ANTHROPIC_API_KEY":"x"}}"#,
        )
        .unwrap();
        assert!(claude_conflicts_in(&sibling, None, false).is_err());

        let user = dir.path().join("user");
        fs::create_dir_all(&user).unwrap();
        let global = dir.path().join(".claude.json");
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let legacy = user.join(".config.json");
        let check = || {
            claude_conflicts_in(
                &elsewhere,
                Some((user.clone(), vec![legacy.clone(), global.clone()])),
                false,
            )
        };
        check().unwrap();
        fs::write(&global, r#"{"primaryApiKey":""}"#).unwrap();
        check().unwrap();
        fs::write(&global, r#"{"primaryApiKey":"sk-saved"}"#).unwrap();
        assert!(check().unwrap_err().to_string().contains("primaryApiKey"));
        fs::remove_file(&global).unwrap();
        fs::write(&legacy, r#"{"primaryApiKey":"sk-legacy"}"#).unwrap();
        assert!(check().unwrap_err().to_string().contains(".config.json"));
        fs::remove_file(&legacy).unwrap();
        fs::write(user.join("settings.json"), r#"{"apiKeyHelper":"x"}"#).unwrap();
        assert!(check().is_err());
        assert!(CLAUDE_ROUTE_CONFLICTS.contains(&"ANTHROPIC_UNIX_SOCKET"));
        assert!(CLAUDE_ROUTE_CONFLICTS.contains(&"CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR"));
    }
}
