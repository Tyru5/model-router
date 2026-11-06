use crate::config::{CustomAdapter, Mode, Stage};
use crate::harness_settings;
use crate::providers::{self, Provider};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::Duration,
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
    /// Removes every inherited variable whose name starts with this entry's
    /// name, except those the invocation sets: `OPENAI_` keeps an OpenAI
    /// organization, project, or headers from reaching a routed endpoint.
    UnsetPrefix,
}

#[derive(Debug, Clone)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    pub stdin: Option<String>,
    pub kind: OutputKind,
    pub final_file: PathBuf,
    pub env: Vec<(String, EnvValue)>,
    /// Files the harness reads, written just before launch (never by `build`,
    /// so previews and doctor have no side effects).
    pub files: Vec<(PathBuf, String)>,
}
impl Invocation {
    /// The command line with environment values redacted: values can hold
    /// endpoints or inline config, and previews are shared.
    pub fn display(&self) -> String {
        let env = self.env.iter().map(|(name, value)| match value {
            EnvValue::Literal(_) => format!("{name}=<redacted>"),
            EnvValue::Var(var) => format!("{name}=${var}"),
            EnvValue::JsonInsert { path, .. } => {
                format!("{name}=${name}+{}=<redacted>", path.join("."))
            }
            EnvValue::UnsetPrefix => format!("-u {name}*"),
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
    /// Several JSON inserts into one variable apply in order.
    pub fn environment(&self) -> Result<Vec<(String, String)>> {
        let mut resolved: Vec<(String, String)> = vec![];
        for (name, value) in &self.env {
            let text = match value {
                EnvValue::Literal(v) => v.clone(),
                EnvValue::Var(var) => env::var(var)
                    .ok()
                    .filter(|v| !v.is_empty())
                    .with_context(|| format!("Set {var} to use this provider"))?,
                EnvValue::JsonInsert { path, value } => {
                    let base = match resolved.iter().find(|(n, _)| n == name) {
                        Some((_, text)) => Some(text.clone()),
                        None => env::var(name).ok(),
                    };
                    json_insert(base.as_deref(), path, value)
                        .with_context(|| format!("Add router config to {name}"))?
                }
                EnvValue::UnsetPrefix => continue,
            };
            match resolved.iter_mut().find(|(n, _)| n == name) {
                Some(entry) => entry.1 = text,
                None => resolved.push((name.clone(), text)),
            }
        }
        Ok(resolved)
    }
    /// Inherited variables to remove at launch (see `EnvValue::UnsetPrefix`).
    pub fn removed(&self) -> Vec<OsString> {
        self.removed_from(env::vars_os().map(|(name, _)| name))
    }
    fn removed_from(&self, inherited: impl Iterator<Item = OsString>) -> Vec<OsString> {
        let prefixes: Vec<&str> = self
            .env
            .iter()
            .filter(|(_, v)| *v == EnvValue::UnsetPrefix)
            .map(|(name, _)| name.as_str())
            .collect();
        if prefixes.is_empty() {
            return vec![];
        }
        inherited
            .filter(|name| {
                let text = name.to_string_lossy();
                prefixes.iter().any(|p| text.starts_with(p))
                    && !self
                        .env
                        .iter()
                        .any(|(set, v)| *v != EnvValue::UnsetPrefix && *set == text)
            })
            .collect()
    }
    /// Writes `files`; the process launcher calls this right before spawning.
    pub fn write_files(&self) -> Result<()> {
        for (path, text) in &self.files {
            fs::write(path, text).with_context(|| format!("Write {}", path.display()))?;
        }
        Ok(())
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
        "cursor" => cursor_program().into(),
        other => other.into(),
    }
}

/// `cursor-agent`, or Cursor's newer `agent` name when only that is
/// installed and it is Cursor's: `agent` is a common name for other tools.
fn cursor_program() -> &'static str {
    if executable("cursor-agent").is_some() {
        return "cursor-agent";
    }
    match executable("agent") {
        Some(path) if is_cursor_agent(&path) => "agent",
        _ => "cursor-agent",
    }
}

/// Cursor installs `agent` as a link into its `cursor-agent` directory;
/// otherwise its help names the Cursor Agent. Cached per path.
fn is_cursor_agent(path: &Path) -> bool {
    static SEEN: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    if let Some(known) = seen.lock().ok().and_then(|s| s.get(path).copied()) {
        return known;
    }
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let cursor = resolved.to_string_lossy().contains("cursor-agent")
        || probe(&path.to_string_lossy(), "--help")
            .is_some_and(|help| help.contains("Cursor Agent"));
    if let Ok(mut seen) = seen.lock() {
        seen.insert(path.to_path_buf(), cursor);
    }
    cursor
}

/// Output of `<program> <flag>`, or None when it fails or takes over 5 s.
fn probe(program: &str, flag: &str) -> Option<String> {
    let temp = tempfile::tempdir().ok()?;
    let invocation = Invocation {
        program: program.into(),
        args: vec![flag.into()],
        stdin: None,
        kind: OutputKind::Text,
        final_file: temp.path().join("unused"),
        env: vec![],
        files: vec![],
    };
    let outcome = crate::process::capture(
        &invocation,
        temp.path(),
        temp.path(),
        Duration::from_secs(5),
        &std::sync::atomic::AtomicBool::new(false),
        |_, _| {},
    )
    .ok()?;
    if !outcome.success() || outcome.timed_out {
        return None;
    }
    let text = crate::process::read_tail(&temp.path().join("stdout.log"), 64 * 1024).ok()?;
    Some(crate::storage::clean_terminal(&text))
}

fn versions() -> &'static Mutex<HashMap<String, Option<String>>> {
    static VERSIONS: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    VERSIONS.get_or_init(Default::default)
}

/// Records a detected `--version` line (doctor does), so builds reuse it.
pub fn remember_version(program: &str, line: Option<&str>) {
    if let Ok(mut versions) = versions().lock() {
        versions.insert(program.into(), line.map(str::to_owned));
    }
}

/// The harness's version: `MODEL_ROUTER_HARNESS_VERSIONS` (`opencode=1.4.0,…`)
/// overrides detection; otherwise the first line of `<program> --version`,
/// detected once per process.
pub fn harness_version(harness: &str, program: &str) -> Option<String> {
    if let Ok(overrides) = env::var("MODEL_ROUTER_HARNESS_VERSIONS") {
        if let Some(version) = overrides.split(',').find_map(|pair| {
            let (name, version) = pair.split_once('=')?;
            (name.trim() == harness).then(|| version.trim().to_owned())
        }) {
            return Some(version);
        }
    }
    if let Some(known) = versions().lock().ok().and_then(|v| v.get(program).cloned()) {
        return known;
    }
    let line = executable(program)
        .and_then(|_| probe(program, "--version"))
        .and_then(|text| {
            text.lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(str::to_owned)
        });
    remember_version(program, line.as_deref());
    line
}

/// The major version in a `--version` line, e.g. 2 in `opencode v2.0.22`.
pub fn major_version(line: &str) -> Option<u64> {
    line.split(|c: char| c.is_whitespace() || matches!(c, '/' | '(' | ')' | ','))
        .map(|token| token.strip_prefix('v').unwrap_or(token))
        .find(|token| token.starts_with(|c: char| c.is_ascii_digit()))
        .and_then(|token| token.split('.').next()?.parse().ok())
}

/// Whether the harness is older than `major`. Unknown versions count as
/// current, so the newest flags are the default.
fn older_than(harness: &str, program: &str, major: u64) -> bool {
    harness_version(harness, program)
        .as_deref()
        .and_then(major_version)
        .is_some_and(|found| found < major)
}

pub fn validate_options(stage: &Stage, configured: &BTreeMap<String, Provider>) -> Result<()> {
    if !stage.provider.is_empty() && !harness_settings::supports_provider(&stage.harness) {
        if configured.contains_key(&stage.provider) {
            bail!(
                "{} cannot use router provider {}; it only uses its own account",
                stage.harness,
                stage.provider
            );
        }
        bail!(
            "{} uses its existing provider/auth configuration; leave the provider field empty",
            stage.harness
        );
    }
    // Pi and omp resolve providers in their own registry; a provider only the
    // router defines would fail inside the harness.
    if matches!(stage.harness.as_str(), "pi" | "omp")
        && configured.contains_key(&stage.provider)
        && !providers::builtins().contains_key(&stage.provider)
    {
        bail!(
            "{harness} cannot use provider {provider}: it is defined only in model-router's [providers.{provider}], and {harness} uses its own provider registry. Define it in {harness}'s models.json, or choose a provider listed by `model-router models {harness}`",
            harness = stage.harness,
            provider = stage.provider
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

/// Whether `harness` can reopen a stage's session in its interactive CLI.
pub fn resumable(harness: &str, custom: &BTreeMap<String, CustomAdapter>) -> bool {
    !custom.contains_key(harness)
        && matches!(
            harness,
            "claude" | "codex" | "pi" | "omp" | "cursor" | "opencode" | "amp"
        )
}

/// Gives a Claude Code or Pi launch a known session ID, which it returns, so
/// the stage can be taken over or reopened later. The others report their own
/// session in their JSON events.
pub fn assign_session(invocation: &mut Invocation, harness: &str) -> Option<String> {
    let chosen = |flag: &str| {
        invocation
            .args
            .iter()
            .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
    };
    // Flags that already pick, continue, or disable a session.
    let session_flags: &[&str] = match harness {
        "claude" => &["--session-id", "--resume", "-r", "--continue", "-c"],
        // omp shares Pi's output but has no --session-id; it reports its own.
        "pi" => &[
            "--session-id",
            "--session",
            "--resume",
            "-r",
            "--continue",
            "-c",
            "--fork",
            "--no-session",
        ],
        _ => return None,
    };
    if session_flags.iter().any(|flag| chosen(flag)) {
        return None;
    }
    let id = uuid::Uuid::new_v4().to_string();
    let flag = ["--session-id".to_owned(), id.clone()];
    if harness == "claude" {
        // Interactive Claude Code takes its prompt last.
        invocation.args.splice(0..0, flag);
    } else {
        invocation.args.extend(flag);
    }
    Some(id)
}

/// The native interactive command that resumes `session` with the stage's
/// model and permission settings, where the harness accepts them on resume.
pub fn resume(
    stage: &Stage,
    custom: &BTreeMap<String, CustomAdapter>,
    configured: &BTreeMap<String, Provider>,
    session: &str,
    paths: &ContextPaths<'_>,
) -> Result<Invocation> {
    ensure!(
        resumable(&stage.harness, custom),
        "{} sessions cannot be resumed",
        stage.harness
    );
    let stage = Stage {
        mode: Mode::Interactive,
        // The OpenCode TUI keeps the session's model and variant.
        effort: if stage.harness == "opencode" {
            String::new()
        } else {
            stage.effort.clone()
        },
        ..stage.clone()
    };
    let mut invocation = build(&stage, custom, configured, "", paths)?;
    let args = &mut invocation.args;
    // Interactive builds end with the prompt (Pi, omp: its @file); the session replaces it.
    args.pop();
    match stage.harness.as_str() {
        "claude" => {
            args.splice(0..0, ["--resume".to_owned(), session.to_owned()]);
        }
        "codex" => {
            args.insert(0, "resume".into());
            args.push(session.into());
        }
        "pi" => args.extend(["--session-id".to_owned(), session.to_owned()]),
        "omp" => {
            args.splice(0..0, ["--resume".to_owned(), session.to_owned()]);
        }
        "cursor" => args.extend(["--resume".to_owned(), session.to_owned()]),
        "amp" => {
            args.splice(0..0, ["threads".to_owned(), "continue".to_owned()]);
            args.push(session.into());
        }
        // The OpenCode TUI keeps the session's model; `run` options do not apply.
        "opencode" => *args = vec!["--session".into(), session.into()],
        other => bail!("{other} sessions cannot be resumed"),
    }
    Ok(invocation)
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
        files: vec![],
    };
    if let Some(provider) = &routed {
        invocation.env = route_env(stage, provider)?;
    }
    for (name, value) in harness_settings::environment(stage) {
        invocation.env.push((name, EnvValue::Literal(value)));
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
                // Stream events so the monitor shows progress; print mode
                // requires --verbose for stream-json.
                append(&["--print", "--output-format", "stream-json", "--verbose"]);
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
        // oh-my-pi prints Pi's JSON events. Its provider is part of the model
        // selector; it reads the prompt file and needs no stdin.
        "omp" => {
            invocation.kind = OutputKind::Pi;
            if !interactive {
                append(&["--print", "--mode", "json"]);
            }
            if !qualified.is_empty() {
                append(&["--model", &qualified]);
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
                append(&["--print", "--output-format", "stream-json"]);
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
            let legacy = older_than("opencode", &invocation.program, 2);
            let (args, env) = opencode_args(
                legacy,
                interactive,
                &qualified,
                &stage.effort,
                &extra_args,
                prompt,
            )?;
            a.extend(args);
            invocation.env.extend(env);
        }
        "aider" => {
            if let Some(provider) = &routed {
                // LiteLLM's openai/ prefix targets OPENAI_API_BASE. The flag
                // also outranks an OPENAI_API_BASE in a project .env file.
                append(&["--model", &format!("openai/{}", stage.model)]);
                append(&["--openai-api-base", &provider.base_url]);
            } else if !qualified.is_empty() {
                append(&["--model", &qualified]);
            }
            append(&["--no-auto-commits", "--no-dirty-commits"]);
            if interactive {
                let commands = paths.attempt_dir.join("aider.commands");
                invocation.files.push((
                    commands.clone(),
                    "/code Complete the stage described in the attached model-router prompt file.\n"
                        .into(),
                ));
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
            // An untrusted adapter keeps its name but runs BLOCKED_COMMAND,
            // which no PATH provides: previews show it and launches fail.
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

type Env = Vec<(String, EnvValue)>;

/// OpenCode arguments and inline config. OpenCode 2.x selects a variant with
/// `--model provider/model#variant`, and its interactive TUI is the root
/// command with `--prompt`, which takes the model from config; 1.x used
/// `run --interactive` and `--variant`.
fn opencode_args(
    legacy: bool,
    interactive: bool,
    model: &str,
    effort: &str,
    extra_args: &[String],
    prompt: &str,
) -> Result<(Vec<String>, Env)> {
    let mut args: Vec<String> = vec![];
    let mut env = vec![];
    let owned = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    if legacy {
        args.extend(owned(if interactive {
            &["run", "--interactive"]
        } else {
            &["run", "--format", "json"]
        }));
        if !model.is_empty() {
            args.extend(owned(&["--model", model]));
        }
        if !effort.is_empty() {
            args.extend(owned(&["--variant", effort]));
        }
        args.extend(extra_args.iter().cloned());
        args.push(prompt.into());
        return Ok((args, env));
    }
    ensure!(
        effort.is_empty() || !model.is_empty(),
        "OpenCode selects a variant (effort) as part of the model; choose a model"
    );
    if interactive {
        ensure!(
            effort.is_empty(),
            "Interactive OpenCode has no variant option; leave effort empty or run unattended"
        );
        if !model.is_empty() {
            env.push((
                "OPENCODE_CONFIG_CONTENT".to_owned(),
                EnvValue::JsonInsert {
                    path: vec!["model".into()],
                    value: serde_json::to_string(model)?,
                },
            ));
        }
        args.extend(extra_args.iter().cloned());
        args.extend(owned(&["--prompt", prompt]));
    } else {
        args.extend(owned(&["run", "--format", "json"]));
        if !model.is_empty() {
            let selector = if effort.is_empty() {
                model.to_owned()
            } else {
                format!("{model}#{effort}")
            };
            args.extend(["--model".to_owned(), selector]);
        }
        args.extend(extra_args.iter().cloned());
        args.push(prompt.into());
    }
    Ok((args, env))
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
        // LiteLLM and Aider read OPENAI_* (organization, project, API type
        // and version, headers) and AIDER_OPENAI_*; inherited ones are meant
        // for OpenAI, so none reach the routed endpoint.
        "aider" => env.extend([
            ("OPENAI_".into(), EnvValue::UnsetPrefix),
            ("AIDER_OPENAI_".into(), EnvValue::UnsetPrefix),
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
            // Goose's OpenAI provider also reads OPENAI_ORGANIZATION,
            // OPENAI_PROJECT, and OPENAI_CUSTOM_HEADERS.
            env.extend([
                ("OPENAI_".into(), EnvValue::UnsetPrefix),
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
    fn takeover_assigns_sessions_and_resumes_in_the_native_cli() {
        let none = BTreeMap::new();
        let providers = BTreeMap::new();
        let dir = tempfile::tempdir().unwrap();
        let paths = ContextPaths {
            workspace: dir.path(),
            attempt_dir: dir.path(),
            prompt_file: &dir.path().join("prompt.md"),
            handoff_file: dir.path(),
        };
        let stage = |harness: &str, mode| Stage {
            harness: harness.into(),
            model: "m".into(),
            effort: if harness_settings::supports_effort(harness) {
                "high".into()
            } else {
                String::new()
            },
            mode,
            ..Stage::default()
        };
        let launch =
            |harness, mode| build(&stage(harness, mode), &none, &providers, "task", &paths);

        // Claude Code and Pi accept a session ID chosen by the router.
        let mut claude = launch("claude", Mode::Unattended).unwrap();
        assert_eq!(
            claude.args,
            [
                "--print",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "m",
                "--effort",
                "high"
            ]
        );
        let id = assign_session(&mut claude, "claude").unwrap();
        assert_eq!(claude.args[..2], ["--session-id".to_owned(), id]);
        let mut interactive = launch("claude", Mode::Interactive).unwrap();
        let id = assign_session(&mut interactive, "claude").unwrap();
        assert_eq!(interactive.args[..2], ["--session-id".to_owned(), id]);
        assert_eq!(interactive.args.last().map(String::as_str), Some("task"));
        let mut pi = launch("pi", Mode::Unattended).unwrap();
        let id = assign_session(&mut pi, "pi").unwrap();
        assert_eq!(
            pi.args[pi.args.len() - 2..],
            ["--session-id".to_owned(), id]
        );
        // The others report their own; explicit session flags are kept.
        for harness in ["codex", "omp", "cursor", "opencode", "amp"] {
            assert_eq!(
                assign_session(&mut launch(harness, Mode::Unattended).unwrap(), harness),
                None
            );
        }
        let mut chosen = launch("claude", Mode::Unattended).unwrap();
        chosen.args.push("--continue".into());
        assert_eq!(assign_session(&mut chosen, "claude"), None);

        let resumed = |harness| {
            resume(
                &stage(harness, Mode::Unattended),
                &none,
                &providers,
                "s-1",
                &paths,
            )
            .unwrap()
        };
        let claude = resumed("claude");
        assert_eq!(
            claude.args,
            ["--resume", "s-1", "--model", "m", "--effort", "high"]
        );
        assert!(claude.stdin.is_none());
        let codex = resumed("codex");
        assert_eq!(codex.args.first().map(String::as_str), Some("resume"));
        assert_eq!(codex.args.last().map(String::as_str), Some("s-1"));
        assert!(codex.args.contains(&"--model".into()) && !codex.args.contains(&"exec".into()));
        assert_eq!(
            resumed("pi").args,
            ["--model", "m", "--thinking", "high", "--session-id", "s-1"]
        );
        assert_eq!(resumed("cursor").args, ["--model", "m", "--resume", "s-1"]);
        assert_eq!(
            resumed("amp").args,
            ["threads", "continue", "--mode", "m", "s-1"]
        );
        assert_eq!(resumed("opencode").args, ["--session", "s-1"]);

        assert!(!resumable("gemini", &none));
        assert!(
            resume(
                &stage("gemini", Mode::Unattended),
                &none,
                &providers,
                "x",
                &paths
            )
            .is_err()
        );
        let custom = BTreeMap::from([("claude".to_owned(), CustomAdapter::default())]);
        assert!(!resumable("claude", &custom));
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

    fn paths_in(dir: &Path) -> ContextPaths<'_> {
        ContextPaths {
            workspace: dir,
            attempt_dir: dir,
            prompt_file: dir,
            handoff_file: dir,
        }
    }

    #[test]
    fn routed_aider_and_goose_drop_inherited_openai_settings() {
        let configured = BTreeMap::from([(
            "proxy".to_string(),
            Provider {
                base_url: "http://localhost:4000/v1".into(),
                api_key_env: "PROXY_KEY".into(),
                ..Provider::default()
            },
        )]);
        let inherited = || {
            [
                "OPENAI_ORGANIZATION",
                "OPENAI_PROJECT",
                "OPENAI_CUSTOM_HEADERS",
                "OPENAI_API_KEY",
                "AIDER_OPENAI_API_KEY",
                "PATH",
            ]
            .into_iter()
            .map(OsString::from)
        };
        for harness in ["aider", "goose"] {
            let i = routed_with(harness, "proxy", Mode::Unattended, &configured).unwrap();
            let removed: Vec<_> = i
                .removed_from(inherited())
                .into_iter()
                .map(|n| n.into_string().unwrap())
                .collect();
            assert!(removed.contains(&"OPENAI_ORGANIZATION".into()), "{harness}");
            assert!(
                removed.contains(&"OPENAI_CUSTOM_HEADERS".into()),
                "{harness}"
            );
            // The key the router sets is replaced, not removed; PATH stays.
            assert!(!removed.contains(&"OPENAI_API_KEY".into()), "{harness}");
            assert!(!removed.contains(&"PATH".into()), "{harness}");
            assert_eq!(
                removed.contains(&"AIDER_OPENAI_API_KEY".into()),
                harness == "aider"
            );
            assert!(
                !i.environment()
                    .unwrap_or_default()
                    .iter()
                    .any(|(n, _)| n.ends_with('_'))
            );
        }
        // The CLI flag outranks an OPENAI_API_BASE in a project .env.
        let aider = routed_with("aider", "proxy", Mode::Unattended, &configured).unwrap();
        assert!(
            aider
                .args
                .windows(2)
                .any(|a| a == ["--openai-api-base", "http://localhost:4000/v1"])
        );
        // Unrouted invocations remove nothing.
        let plain = routed("aider", "", Mode::Unattended).unwrap();
        assert!(plain.removed_from(inherited()).is_empty());
    }

    #[test]
    fn display_redacts_environment_values() {
        let i = Invocation {
            program: "goose".into(),
            args: vec!["run".into()],
            stdin: None,
            kind: OutputKind::Text,
            final_file: PathBuf::from("unused"),
            env: vec![
                ("OPENAI_".into(), EnvValue::UnsetPrefix),
                (
                    "OPENAI_HOST".into(),
                    EnvValue::Literal("https://h.example".into()),
                ),
                ("OPENAI_API_KEY".into(), EnvValue::Var("PROXY_KEY".into())),
                (
                    "OPENCODE_CONFIG_CONTENT".into(),
                    EnvValue::JsonInsert {
                        path: vec!["model".into()],
                        value: "\"secret-ish\"".into(),
                    },
                ),
            ],
            files: vec![],
        };
        let shown = i.display();
        assert_eq!(
            shown,
            "-u OPENAI_* OPENAI_HOST=<redacted> OPENAI_API_KEY=$PROXY_KEY OPENCODE_CONFIG_CONTENT=$OPENCODE_CONFIG_CONTENT+model=<redacted> goose run"
        );
    }

    #[test]
    fn json_inserts_into_one_variable_accumulate() {
        let i = Invocation {
            program: "opencode".into(),
            args: vec![],
            stdin: None,
            kind: OutputKind::OpenCode,
            final_file: PathBuf::from("unused"),
            env: vec![
                (
                    "MODEL_ROUTER_TEST_UNSET_CONFIG".into(),
                    EnvValue::JsonInsert {
                        path: vec!["provider".into(), "p".into()],
                        value: "{}".into(),
                    },
                ),
                (
                    "MODEL_ROUTER_TEST_UNSET_CONFIG".into(),
                    EnvValue::JsonInsert {
                        path: vec!["model".into()],
                        value: "\"p/m\"".into(),
                    },
                ),
            ],
            files: vec![],
        };
        let env = i.environment().unwrap();
        assert_eq!(env.len(), 1);
        let merged: serde_json::Value = serde_json::from_str(&env[0].1).unwrap();
        assert_eq!(merged["model"], "p/m");
        assert!(merged["provider"]["p"].is_object());
    }

    #[test]
    fn opencode_uses_2x_flags_unless_an_older_version_is_detected() {
        let none: Vec<String> = vec![];
        let (args, env) = opencode_args(false, false, "p/m", "high", &none, "task").unwrap();
        assert_eq!(
            args,
            ["run", "--format", "json", "--model", "p/m#high", "task"]
        );
        assert!(env.is_empty());
        let (args, env) = opencode_args(false, true, "p/m", "", &none, "task").unwrap();
        assert_eq!(args, ["--prompt", "task"]);
        assert_eq!(
            env,
            [(
                "OPENCODE_CONFIG_CONTENT".to_owned(),
                EnvValue::JsonInsert {
                    path: vec!["model".into()],
                    value: "\"p/m\"".into()
                }
            )]
        );
        assert!(opencode_args(false, true, "p/m", "high", &none, "t").is_err());
        assert!(opencode_args(false, false, "", "high", &none, "t").is_err());
        let (args, _) = opencode_args(true, true, "p/m", "high", &none, "task").unwrap();
        assert_eq!(
            args,
            [
                "run",
                "--interactive",
                "--model",
                "p/m",
                "--variant",
                "high",
                "task"
            ]
        );
        assert_eq!(major_version("opencode v2.0.22"), Some(2));
        assert_eq!(major_version("1.4.3"), Some(1));
        assert_eq!(major_version("omp/18.6.3"), Some(18));
        assert_eq!(major_version("unknown"), None);
    }

    #[test]
    fn pi_rejects_providers_only_the_router_defines() {
        let configured = BTreeMap::from([
            (
                "litellm".to_string(),
                Provider {
                    base_url: "http://localhost:4000/v1".into(),
                    ..Provider::default()
                },
            ),
            ("openrouter".to_string(), Provider::default()),
        ]);
        for harness in ["pi", "omp"] {
            let error = routed_with(harness, "litellm", Mode::Unattended, &configured)
                .unwrap_err()
                .to_string();
            assert!(error.contains("own provider registry"), "{error}");
            // Built-in names and Pi's own providers pass through.
            routed_with(harness, "openrouter", Mode::Unattended, &configured).unwrap();
            routed_with(harness, "anthropic", Mode::Unattended, &configured).unwrap();
        }
    }

    #[test]
    fn permission_settings_reach_aider_goose_and_copilot() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let with = |harness: &str, key: &str, value: &str| {
            let stage = Stage {
                harness: harness.into(),
                model: "m".into(),
                harness_settings: BTreeMap::from([(key.into(), value.into())]),
                ..Stage::default()
            };
            build(&stage, &BTreeMap::new(), &BTreeMap::new(), "t", &paths).unwrap()
        };
        assert!(
            with("aider", "confirmations", "yes-always")
                .args
                .contains(&"--yes-always".into())
        );
        let goose = with("goose", "mode", "smart_approve");
        assert_eq!(
            env(&goose, "GOOSE_MODE"),
            Some(&EnvValue::Literal("smart_approve".into()))
        );
        assert!(!goose.args.iter().any(|a| a.contains("GOOSE")));
        assert!(
            with("copilot", "tool_permissions", "allow-all-tools")
                .args
                .contains(&"--allow-all-tools".into())
        );
        let claude = with("claude", "permission_mode", "manual");
        assert!(
            claude
                .args
                .windows(2)
                .any(|a| a == ["--permission-mode", "manual"])
        );
        // Nothing is emitted for the harness default.
        assert!(
            !with("aider", "confirmations", "")
                .args
                .contains(&"--yes-always".into())
        );
    }

    #[test]
    fn build_is_pure_and_aider_commands_are_written_at_launch() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let stage = Stage {
            harness: "aider".into(),
            model: "m".into(),
            mode: Mode::Interactive,
            ..Stage::default()
        };
        let i = build(&stage, &BTreeMap::new(), &BTreeMap::new(), "t", &paths).unwrap();
        let commands = dir.path().join("aider.commands");
        assert!(!commands.exists(), "build must not write files");
        assert_eq!(i.files.len(), 1);
        i.write_files().unwrap();
        assert!(fs::read_to_string(&commands).unwrap().starts_with("/code "));
    }

    #[test]
    fn untrusted_custom_adapters_cannot_launch() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let custom = BTreeMap::from([(
            "mine".to_string(),
            CustomAdapter {
                command: crate::trust::BLOCKED_COMMAND.into(),
                ..CustomAdapter::default()
            },
        )]);
        let stage = Stage {
            harness: "mine".into(),
            ..Stage::default()
        };
        let i = build(&stage, &custom, &BTreeMap::new(), "t", &paths).unwrap();
        assert_eq!(i.program, crate::trust::BLOCKED_COMMAND);
        assert!(executable(&i.program).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cursor_accepts_agent_only_when_it_is_cursor() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = |name: &str, body: &str| {
            let path = dir.path().join(name);
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let other = script("other-agent", "#!/bin/sh\necho 'Usage: agent [deploy]'\n");
        let cursor = script(
            "cursor-help",
            "#!/bin/sh\necho 'Usage: agent [options]'\necho 'Start the Cursor Agent'\n",
        );
        assert!(!is_cursor_agent(&other));
        assert!(is_cursor_agent(&cursor));
        let linked = dir.path().join("cursor-agent/versions/1/cursor-agent");
        fs::create_dir_all(linked.parent().unwrap()).unwrap();
        fs::write(&linked, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&linked, fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.path().join("agent");
        std::os::unix::fs::symlink(&linked, &link).unwrap();
        assert!(is_cursor_agent(&link), "Cursor's own link needs no probe");
    }
}
