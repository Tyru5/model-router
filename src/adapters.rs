use crate::config::{CustomAdapter, CustomPrompt, Mode, PromptMode, Stage};
use crate::harness::{
    self, BaseEnv, EffortPass, HarnessSpec, ModelPass, Part, Prompt, ProviderPass, Routing,
    SchemaDelivery, Session,
};
use crate::providers::{self, Provider};
use crate::{handoff, harness_settings, storage};
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
    Antigravity,
    OpenCode,
    Amp,
}
impl OutputKind {
    /// The parser name custom adapters use for `output`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Codex => "codex-jsonl",
            Self::Claude => "claude-stream",
            Self::Pi => "pi",
            Self::Cursor => "cursor-stream",
            Self::Antigravity => "antigravity-stream",
            Self::OpenCode => "opencode",
            Self::Amp => "amp-stream",
        }
    }
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
    /// A session ID `assign_session` may add (`Session::Assign`).
    pub assign: Option<AssignSession>,
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
        let mut prefixes: Vec<&str> = self
            .env
            .iter()
            .filter(|(_, v)| *v == EnvValue::UnsetPrefix)
            .map(|(name, _)| name.as_str())
            .collect();
        // Only a delegating turn reaches a broker: every other launch drops
        // a socket and token an outer `mr` may have passed down.
        prefixes.extend([
            crate::delegation::broker::SOCK_VAR,
            crate::delegation::broker::TOKEN_VAR,
        ]);
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
    /// They go in the attempt directory, where an earlier candidate round
    /// may have left a link at the same name.
    pub fn write_files(&self) -> Result<()> {
        for (path, text) in &self.files {
            std::io::Write::write_all(&mut storage::create_fresh(path)?, text.as_bytes())
                .with_context(|| format!("Write {}", path.display()))?;
        }
        Ok(())
    }
}

pub fn executable(program: &str) -> Option<PathBuf> {
    fn usable(path: &Path) -> bool {
        let Ok(meta) = fs::metadata(path) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o111 != 0
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
    match harness::get(harness) {
        Some(spec) => resolve_program(spec).into(),
        None => harness.into(),
    }
}

/// The first installed program candidate, skipping a shared name (Cursor's
/// `agent`) that is not the harness's; the first candidate otherwise.
fn resolve_program(spec: &HarnessSpec) -> &'static str {
    for &candidate in spec.programs {
        let Some(path) = executable(candidate) else {
            continue;
        };
        match spec.identity {
            Some(identity) if identity.program == candidate => {
                if is_identified(&path, identity.path_contains, identity.help_contains) {
                    return candidate;
                }
            }
            _ => return candidate,
        }
    }
    spec.programs[0]
}

/// Whether `path` resolves into a directory named `path_contains` (Cursor
/// installs `agent` as a link into its `cursor-agent` directory), or else
/// its help contains `help_contains`. Cached per path.
fn is_identified(path: &Path, path_contains: &str, help_contains: &str) -> bool {
    static SEEN: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    if let Some(known) = seen.lock().ok().and_then(|s| s.get(path).copied()) {
        return known;
    }
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let found = resolved.to_string_lossy().contains(path_contains)
        || probe(&path.to_string_lossy(), "--help")
            .is_some_and(|help| help.contains(help_contains));
    if let Ok(mut seen) = seen.lock() {
        seen.insert(path.to_path_buf(), found);
    }
    found
}

#[cfg(test)]
fn is_cursor_agent(path: &Path) -> bool {
    let identity = harness::get("cursor")
        .and_then(|spec| spec.identity)
        .expect("cursor identity");
    is_identified(path, identity.path_contains, identity.help_contains)
}

/// The output parser for a harness's unattended runs.
pub fn output_kind(harness: &str, custom: &BTreeMap<String, CustomAdapter>) -> OutputKind {
    match custom.get(harness) {
        Some(adapter) => adapter.output.map_or(OutputKind::Text, |o| o.kind()),
        None => harness::get(harness).map_or(OutputKind::Text, |spec| spec.output),
    }
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
        assign: None,
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

/// Returns the detected or overridden version for `(harness, program)` to
/// record in attempt `command.json`. Built-in harnesses use the cached
/// `harness_version` lookup (at most one `--version` probe per process);
/// custom adapters are never probed and only read overrides or cached entries.
pub fn detected_version(harness: &str, program: &str) -> Option<String> {
    if harness::get(harness).is_some() {
        return harness_version(harness, program);
    }
    if let Ok(overrides) = env::var("MODEL_ROUTER_HARNESS_VERSIONS") {
        if let Some(version) = overrides.split(',').find_map(|pair| {
            let (name, version) = pair.split_once('=')?;
            (name.trim() == harness).then(|| version.trim().to_owned())
        }) {
            return Some(version);
        }
    }
    versions()
        .lock()
        .ok()
        .and_then(|v| v.get(program).cloned())
        .flatten()
}

/// The major version in a `--version` line, e.g. 2 in `opencode v2.0.22`.
pub fn major_version(line: &str) -> Option<u64> {
    line.split(|c: char| c.is_whitespace() || matches!(c, '/' | '(' | ')' | ','))
        .map(|token| token.strip_prefix('v').unwrap_or(token))
        .find(|token| token.starts_with(|c: char| c.is_ascii_digit()))
        .and_then(|token| token.split('.').next()?.parse().ok())
}

/// The `[major, minor, patch]` version in a `--version` line, e.g.
/// `[0, 41, 0]` in `codex-cli 0.41.0`; missing parts are 0.
pub fn version_numbers(line: &str) -> Option<[u64; 3]> {
    let token = line
        .split(|c: char| c.is_whitespace() || matches!(c, '/' | '(' | ')' | ','))
        .map(|token| token.strip_prefix('v').unwrap_or(token))
        .find(|token| token.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut numbers = [0; 3];
    for (slot, part) in numbers.iter_mut().zip(token.split('.')) {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        *slot = digits.parse().ok()?;
    }
    Some(numbers)
}

/// Whether `version` (a `--version` line) takes the harness's native output
/// schema flag (`HarnessSpec::structured`). Unknown versions count as current.
pub fn output_schema_supported(harness: &str, version: Option<&str>) -> bool {
    let Some(structured) = harness::get(harness).and_then(|spec| spec.structured) else {
        return false;
    };
    version
        .and_then(version_numbers)
        .is_none_or(|found| found >= structured.since)
}

/// The Codex attempt file holding the strict handoff schema.
pub const OUTPUT_SCHEMA_FILE: &str = "output-schema.json";

/// Whether the invocation asks its harness for schema-constrained output.
pub fn uses_output_schema(invocation: &Invocation) -> bool {
    harness::SPECS
        .iter()
        .filter_map(|spec| spec.structured.map(|s| s.flag))
        .any(|flag| invocation.args.iter().any(|a| a == flag))
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
    let spec = harness::get(&stage.harness);
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
    // Registry harnesses (Pi, omp) resolve providers themselves; a provider
    // only the router defines would fail inside the harness.
    if spec.is_some_and(|s| s.routing == Routing::Registry)
        && configured.contains_key(&stage.provider)
        && providers::builtin(&stage.provider).is_none()
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
    match custom.get(harness) {
        Some(adapter) => adapter.resume_args.is_some() && adapter.session.is_some(),
        None => harness::get(harness).is_some_and(|spec| spec.resume.is_some()),
    }
}

/// A session ID the router may choose for a launch (`Session::Assign`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignSession {
    pub flag: String,
    pub prepend: bool,
    /// Flags that already pick, continue, or disable a session.
    pub conflicts: Vec<String>,
}

/// Gives a launch whose harness accepts one (Claude Code, Pi, custom
/// adapters with `session.assign`) a known session ID, which it returns, so
/// the stage can be taken over or reopened later. The others report their
/// own session in their JSON events. The plan comes from `build`.
pub fn assign_session(invocation: &mut Invocation, _harness: &str) -> Option<String> {
    let plan = invocation.assign.take()?;
    let chosen = |flag: &str| {
        invocation
            .args
            .iter()
            .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
    };
    if plan.conflicts.iter().any(|flag| chosen(flag)) {
        return None;
    }
    let id = uuid::Uuid::new_v4().to_string();
    let flag = [plan.flag, id.clone()];
    if plan.prepend {
        invocation.args.splice(0..0, flag);
    } else {
        invocation.args.extend(flag);
    }
    Some(id)
}

/// Whether `id` can be a harness session ID. Session IDs come from run.json
/// and harness logs and become their own argument: never an option.
pub fn is_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
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
        is_session_id(session),
        "{session:?} is not a harness session ID"
    );
    ensure!(
        resumable(&stage.harness, custom),
        "{} sessions cannot be resumed",
        stage.harness
    );
    if custom.contains_key(&stage.harness) {
        let stage = Stage {
            mode: Mode::Interactive,
            ..stage.clone()
        };
        return build_inner(
            &stage,
            custom,
            configured,
            "",
            paths,
            Launch::Resume(session),
        );
    }
    let plan = harness::get(&stage.harness)
        .context("Unknown adapter")?
        .resume
        .context("Not resumable")?;
    let stage = Stage {
        mode: Mode::Interactive,
        effort: if plan.keep_effort {
            stage.effort.clone()
        } else {
            String::new()
        },
        ..stage.clone()
    };
    build_inner(
        &stage,
        custom,
        configured,
        "",
        paths,
        Launch::Resume(session),
    )
}

/// Whether the harness's unattended output arrives while it works. False
/// when a harness prints one document at exit: a quiet stage then looks
/// the same as a stalled one, so `stall_timeout_seconds` cannot apply.
pub fn streams(harness: &str, custom: &BTreeMap<String, CustomAdapter>) -> bool {
    match custom.get(harness) {
        Some(_) => true,
        None => harness::get(harness).is_none_or(|spec| spec.streams),
    }
}

/// Whether the harness has an unattended follow-up command (see `follow_up`).
pub fn has_follow_up(harness: &str, custom: &BTreeMap<String, CustomAdapter>) -> bool {
    match custom.get(harness) {
        Some(adapter) => adapter.follow_up_args.is_some(),
        None => harness::get(harness).is_some_and(|s| s.follow_up.is_some()),
    }
}

/// The unattended command that sends `prompt` to the existing `session`,
/// built from the harness's `FollowUp` plan or a custom adapter's
/// `follow_up_args`: `workflow.handoff_repair` uses it to ask for valid
/// handoff JSON after a stage finished without one. An error says why there
/// is no such command.
pub fn follow_up(
    stage: &Stage,
    custom: &BTreeMap<String, CustomAdapter>,
    configured: &BTreeMap<String, Provider>,
    session: &str,
    prompt: &str,
    paths: &ContextPaths<'_>,
) -> Result<Invocation> {
    ensure!(
        is_session_id(session),
        "{session:?} is not a harness session ID"
    );
    ensure!(
        has_follow_up(&stage.harness, custom),
        "{} has no unattended follow-up command for a session",
        stage.harness
    );
    let stage = Stage {
        mode: Mode::Unattended,
        ..stage.clone()
    };
    let mut invocation = build_inner(
        &stage,
        custom,
        configured,
        prompt,
        paths,
        Launch::FollowUp(session),
    )?;
    // The session exists; nothing is assigned.
    invocation.assign = None;
    Ok(invocation)
}

/// Whether `harness` can receive the router's MCP server for one launch, so
/// a stage on it can delegate.
pub fn can_delegate(harness: &str, custom: &BTreeMap<String, CustomAdapter>) -> bool {
    match custom.get(harness) {
        Some(adapter) => adapter.mcp_config_args.is_some(),
        None => harness::get(harness).is_some_and(|spec| spec.mcp.is_some()),
    }
}

/// What a launch does with a session.
#[derive(Clone, Copy)]
enum Launch<'a> {
    /// Starts one.
    New,
    /// A custom adapter's interactive `resume_args`.
    Resume(&'a str),
    /// The unattended follow-up on one.
    FollowUp(&'a str),
}

/// The `--config KEY=VALUE` / `-c KEY=VALUE` pairs among `args`, each as
/// `flag KEY=VALUE`.
fn config_pairs(args: &[String], flag: &str) -> Vec<String> {
    let mut out = vec![];
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let value = if arg == "--config" || arg == "-c" {
            iter.next().map(String::as_str)
        } else {
            arg.strip_prefix("--config=").or_else(|| {
                arg.strip_prefix("-c")
                    .filter(|rest| !rest.starts_with('-') && rest.contains('='))
                    .map(|rest| rest.trim_start_matches('='))
            })
        };
        if let Some(value) = value {
            out.extend([flag.to_owned(), value.to_owned()]);
        }
    }
    out
}

pub struct ContextPaths<'a> {
    pub workspace: &'a Path,
    pub attempt_dir: &'a Path,
    pub prompt_file: &'a Path,
    pub handoff_file: &'a Path,
    /// A delegating turn's bridge; None for every other launch.
    pub mcp: Option<&'a McpLaunch>,
}

/// What a delegating turn's harness needs to start the router's MCP bridge:
/// paths only, never the token itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpLaunch {
    pub program: PathBuf,
    pub sock: PathBuf,
    pub token_file: PathBuf,
}
impl McpLaunch {
    fn env(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut env = serde_json::Map::new();
        env.insert(
            crate::delegation::broker::SOCK_VAR.into(),
            self.sock.display().to_string().into(),
        );
        env.insert(
            crate::delegation::broker::TOKEN_VAR.into(),
            self.token_file.display().to_string().into(),
        );
        env
    }
    /// Claude Code's `--mcp-config` format, also used by custom adapters.
    pub fn claude_config(&self) -> String {
        serde_json::json!({
            "mcpServers": {
                "model-router": {
                    "type": "stdio",
                    "command": self.program.display().to_string(),
                    "args": [crate::delegation::broker::BRIDGE_ARG],
                    "env": self.env(),
                }
            }
        })
        .to_string()
    }
    /// Codex `--config` pairs; values are TOML (JSON strings are valid TOML
    /// basic strings), since paths may hold spaces.
    fn codex_args(&self) -> Result<Vec<String>> {
        let key = "mcp_servers.model-router";
        let quote = |text: &str| serde_json::to_string(text);
        Ok(vec![
            "--config".into(),
            format!(
                "{key}.command={}",
                quote(&self.program.display().to_string())?
            ),
            "--config".into(),
            format!(
                "{key}.args=[{}]",
                quote(crate::delegation::broker::BRIDGE_ARG)?
            ),
            "--config".into(),
            format!(
                "{key}.env={{{}={}, {}={}}}",
                crate::delegation::broker::SOCK_VAR,
                quote(&self.sock.display().to_string())?,
                crate::delegation::broker::TOKEN_VAR,
                quote(&self.token_file.display().to_string())?
            ),
            "--config".into(),
            format!("{key}.tool_timeout_sec=120"),
        ])
    }
    /// OpenCode's `mcp.model-router` entry.
    fn opencode_entry(&self) -> String {
        serde_json::json!({
            "type": "local",
            "command": [self.program.display().to_string(), crate::delegation::broker::BRIDGE_ARG],
            "environment": self.env(),
            "enabled": true,
        })
        .to_string()
    }
    /// The variables the bridge reads, set on the harness too.
    fn set_env(&self, invocation: &mut Invocation) {
        for (name, value) in self.env() {
            if let serde_json::Value::String(value) = value {
                invocation.env.push((name, EnvValue::Literal(value)));
            }
        }
    }
}

pub fn build(
    stage: &Stage,
    custom: &BTreeMap<String, CustomAdapter>,
    configured: &BTreeMap<String, Provider>,
    prompt: &str,
    paths: &ContextPaths<'_>,
) -> Result<Invocation> {
    build_inner(stage, custom, configured, prompt, paths, Launch::New)
}

/// `build`, a custom adapter's resume command, or a follow-up.
fn build_inner(
    stage: &Stage,
    custom: &BTreeMap<String, CustomAdapter>,
    configured: &BTreeMap<String, Provider>,
    prompt: &str,
    paths: &ContextPaths<'_>,
    launch: Launch<'_>,
) -> Result<Invocation> {
    validate_options(stage, configured)?;
    let routed = providers::route(&stage.harness, &stage.provider, configured);
    let spec = harness::get(&stage.harness);
    let follow_up = match (launch, spec.and_then(|s| s.follow_up)) {
        (Launch::FollowUp(session), Some(plan)) if !custom.contains_key(&stage.harness) => {
            Some((plan, session))
        }
        _ => None,
    };
    let extra_args = match follow_up.map(|(plan, _)| plan.extra) {
        Some(harness::FollowUpExtra::ConfigOnly(flag)) => {
            let mut args = harness_settings::config_arguments(stage, flag)?;
            args.extend(config_pairs(&stage.extra_args, flag));
            args
        }
        Some(harness::FollowUpExtra::Same) | None => {
            let mut args = harness_settings::arguments(stage)?;
            args.extend(stage.extra_args.clone());
            args
        }
    };
    let mut invocation = Invocation {
        program: program(&stage.harness, custom),
        args: vec![],
        stdin: None,
        kind: OutputKind::Text,
        final_file: paths.attempt_dir.join("final.txt"),
        env: vec![],
        files: vec![],
        assign: None,
    };
    if let Some(provider) = &routed {
        invocation.env = route_env(stage, provider)?;
    }
    for (name, value) in harness_settings::environment(stage) {
        invocation.env.push((name, EnvValue::Literal(value)));
    }
    let routed = routed.as_deref();
    if let Some(adapter) = custom.get(&stage.harness) {
        build_custom(
            &mut invocation,
            &stage.harness,
            adapter,
            stage,
            routed,
            &extra_args,
            prompt,
            paths,
            launch,
        )?;
        return Ok(invocation);
    }
    let spec = spec.with_context(|| format!("Unknown adapter {}", stage.harness))?;
    let legacy = spec
        .version_override
        .is_some_and(|o| older_than(spec.name, &invocation.program, o.below_major));
    build_spec(
        spec,
        &mut invocation,
        stage,
        routed,
        &extra_args,
        prompt,
        paths,
        legacy,
        launch,
    )?;
    if let Some(mcp) = paths.mcp {
        if spec.mcp == Some(harness::McpInjection::OpencodeConfig) {
            invocation.env.push((
                "OPENCODE_CONFIG_CONTENT".into(),
                EnvValue::JsonInsert {
                    path: vec!["mcp".into(), "model-router".into()],
                    value: mcp.opencode_entry(),
                },
            ));
        }
        if spec.mcp.is_some() {
            mcp.set_env(&mut invocation);
        }
    }
    Ok(invocation)
}

/// Builds a built-in harness's invocation from its spec; `legacy` selects
/// its version override. With `Launch::FollowUp`, its template and prompt
/// replace the mode's, and `{session}` expands to the session.
#[allow(clippy::too_many_arguments)]
fn build_spec(
    spec: &HarnessSpec,
    invocation: &mut Invocation,
    stage: &Stage,
    routed: Option<&Provider>,
    extra_args: &[String],
    prompt: &str,
    paths: &ContextPaths<'_>,
    legacy: bool,
    launch: Launch<'_>,
) -> Result<()> {
    let interactive = stage.mode == Mode::Interactive;
    invocation.kind = spec.output;
    let follow_up = match (launch, spec.follow_up) {
        (Launch::FollowUp(session), Some(plan)) => Some((plan, session)),
        _ => None,
    };
    let (parts, prompt_pass, effort_pass) = match follow_up {
        Some((plan, _)) => (plan.args, plan.prompt, spec.template(false, legacy).2),
        None => spec.template(interactive, legacy),
    };
    let session = follow_up.map_or("", |(_, session)| session);
    let model = &stage.model;
    let effort = &stage.effort;
    if let EffortPass::ModelSuffix(_) = effort_pass {
        ensure!(
            effort.is_empty() || !model.is_empty(),
            "{} selects a {} (effort) as part of the model; choose a model",
            spec.display,
            spec.effort_name
        );
    }
    let takes_effort = match effort_pass {
        EffortPass::None => false,
        EffortPass::ModelSuffix(_) => parts.contains(&Part::Model),
        _ => parts.contains(&Part::Effort),
    };
    ensure!(
        effort.is_empty() || takes_effort,
        "{} {} has no {} option; leave effort empty or run {}",
        if interactive {
            "Interactive"
        } else {
            "Unattended"
        },
        spec.display,
        spec.effort_name,
        if interactive {
            "unattended"
        } else {
            "interactively"
        }
    );
    let qualified = if stage.provider.is_empty() {
        model.clone()
    } else if routed.is_some() && spec.routing.router_id_in_model() {
        format!("{}/{}", router_provider_id(&stage.provider), model)
    } else {
        format!("{}/{}", stage.provider, model)
    };
    let selector = |value: &str| match effort_pass {
        EffortPass::ModelSuffix(c) if !effort.is_empty() => format!("{value}{c}{effort}"),
        _ => value.to_owned(),
    };
    let openai = match spec.routing {
        Routing::OpenAiEnv(env) => Some(env),
        _ => None,
    };
    // Native structured output, unless the user passed their own schema or
    // the detected harness predates the flag.
    let schema = spec.structured.filter(|s| {
        !interactive
            && !extra_args
                .iter()
                .any(|a| a == s.flag || a.starts_with(&format!("{}=", s.flag)))
            && output_schema_supported(
                spec.name,
                harness_version(spec.name, &invocation.program).as_deref(),
            )
    });
    if let Session::Assign {
        flag,
        prepend,
        conflicts,
    } = spec.session
    {
        invocation.assign = Some(AssignSession {
            flag: flag.into(),
            prepend,
            conflicts: conflicts.iter().map(|c| (*c).to_owned()).collect(),
        });
    }
    let utf8 = |path: &Path, what: &str| -> Result<String> {
        Ok(path
            .to_str()
            .with_context(|| format!("{what} path must be UTF-8"))?
            .to_owned())
    };
    let mut args: Vec<String> = vec![];
    for part in parts {
        match *part {
            Part::Args(items) => {
                for item in items {
                    args.push(if item.contains("{final_file}") {
                        let file = utf8(&invocation.final_file, "Output")?;
                        expand(item, &[("{final_file}", &file)])
                    } else if follow_up.is_some() && item.contains("{session}") {
                        expand(item, &[("{session}", session)])
                    } else {
                        (*item).to_owned()
                    });
                }
            }
            Part::Schema => match schema.map(|s| (s.flag, s.delivery)) {
                Some((flag, SchemaDelivery::File(name))) => {
                    let path = paths.attempt_dir.join(name);
                    args.extend([flag.to_owned(), utf8(&path, "Schema")?]);
                    invocation.files.push((path, handoff::strict_schema()));
                }
                Some((flag, SchemaDelivery::Inline)) => {
                    args.extend([flag.to_owned(), handoff::compact_schema()]);
                }
                None => {}
            },
            Part::Model => match spec.model {
                ModelPass::Flag(flag) | ModelPass::Mode(flag) => {
                    if !model.is_empty() {
                        args.extend([flag.to_owned(), selector(model)]);
                    }
                }
                ModelPass::Qualified(flag) => {
                    let prefix = openai.and_then(|o| o.model_prefix);
                    match (routed, prefix) {
                        (Some(provider), Some(prefix)) => {
                            args.extend([flag.to_owned(), format!("{prefix}{model}")]);
                            if let Some(base) = openai.and_then(|o| o.base_url_flag) {
                                args.extend([base.to_owned(), provider.base_url.clone()]);
                            }
                        }
                        _ if !qualified.is_empty() => {
                            args.extend([flag.to_owned(), selector(&qualified)]);
                        }
                        _ => {}
                    }
                }
            },
            Part::Provider => match spec.provider {
                ProviderPass::None | ProviderPass::InModel => {}
                ProviderPass::Flag(flag) => match (routed, openai.and_then(|o| o.provider_value)) {
                    (Some(_), Some(value)) => args.extend([flag.to_owned(), value.to_owned()]),
                    _ if !stage.provider.is_empty() => {
                        args.extend([flag.to_owned(), stage.provider.clone()]);
                    }
                    _ => {}
                },
                ProviderPass::CodexConfig => {
                    // `codex exec resume` has no --oss/--local-provider.
                    ensure!(
                        follow_up.is_none()
                            || routed.is_some()
                            || !matches!(stage.provider.as_str(), "ollama" | "lmstudio"),
                        "{} cannot follow up on a session using local provider {}",
                        spec.display,
                        stage.provider
                    );
                    codex_provider_args(&mut args, stage, routed)?;
                }
            },
            Part::Effort => {
                if !effort.is_empty() {
                    match effort_pass {
                        EffortPass::None | EffortPass::ModelSuffix(_) => {}
                        EffortPass::Flag(flag) => args.extend([flag.to_owned(), effort.clone()]),
                        EffortPass::ConfigKv { flag, key } => args.extend([
                            flag.to_owned(),
                            format!("{key}={}", serde_json::to_string(effort)?),
                        ]),
                    }
                }
            }
            Part::Extra => args.extend(extra_args.iter().cloned()),
            Part::Prompt if matches!(launch, Launch::Resume(_)) => {}
            Part::Prompt => match prompt_pass {
                Prompt::Stdin => invocation.stdin = Some(prompt.into()),
                Prompt::StdinArg(arg) => {
                    args.push(arg.into());
                    invocation.stdin = Some(prompt.into());
                }
                Prompt::Arg => args.push(prompt.into()),
                Prompt::Flag(flag) => args.extend([flag.to_owned(), prompt.to_owned()]),
                Prompt::AtFile => args.push(format!("@{}", paths.prompt_file.display())),
                Prompt::MessageFile(flag) => {
                    args.extend([flag.to_owned(), utf8(paths.prompt_file, "Prompt")?]);
                }
                Prompt::ReadAndLoad {
                    read,
                    load,
                    file,
                    commands,
                } => {
                    let path = paths.attempt_dir.join(file);
                    invocation.files.push((path.clone(), commands.into()));
                    args.extend([
                        read.to_owned(),
                        utf8(paths.prompt_file, "Prompt")?,
                        load.to_owned(),
                        utf8(&path, "Command")?,
                    ]);
                }
            },
            Part::Mcp => match (paths.mcp, spec.mcp) {
                (Some(mcp), Some(harness::McpInjection::ClaudeConfigFile)) => {
                    let file = paths.attempt_dir.join("mcp.json");
                    invocation.files.push((file.clone(), mcp.claude_config()));
                    args.extend([
                        "--mcp-config".to_owned(),
                        utf8(&file, "MCP config")?,
                        // One argument, so it cannot take a positional prompt.
                        "--allowedTools=mcp__model-router__*".to_owned(),
                    ]);
                }
                (Some(mcp), Some(harness::McpInjection::CodexConfig)) => {
                    args.extend(mcp.codex_args()?);
                }
                _ => {}
            },
            Part::ModelEnv { var, path } => {
                if !qualified.is_empty() {
                    invocation.env.push((
                        var.to_owned(),
                        EnvValue::JsonInsert {
                            path: vec![path.into()],
                            value: serde_json::to_string(&qualified)?,
                        },
                    ));
                }
            }
        }
    }
    if let (Launch::Resume(session), Some(plan)) = (launch, spec.resume) {
        invocation.assign = None;
        let fill = |items: &[&str]| -> Vec<String> {
            items
                .iter()
                .map(|item| expand(item, &[("{session}", session)]))
                .collect()
        };
        match plan.replace {
            Some(items) => args = fill(items),
            None => {
                args.splice(0..0, fill(plan.prepend));
                args.extend(fill(plan.append));
            }
        }
    }
    invocation.args = args;
    Ok(())
}

/// Codex provider arguments (escape hatch): a routed provider becomes an
/// isolated `model_providers` entry, local providers use `--oss`, and other
/// names select Codex's own provider.
fn codex_provider_args(
    args: &mut Vec<String>,
    stage: &Stage,
    routed: Option<&Provider>,
) -> Result<()> {
    let mut append = |items: &[&str]| args.extend(items.iter().map(|s| (*s).to_owned()));
    if let Some(provider) = routed {
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
    Ok(())
}

/// A custom adapter's template arguments, then `model_flag`, `effort_flag`,
/// extra arguments, and the prompt. A resume uses `resume_args` and passes
/// no prompt; a follow-up uses `follow_up_args` and passes the prompt as an
/// unattended launch does.
#[allow(clippy::too_many_arguments)]
fn build_custom(
    invocation: &mut Invocation,
    name: &str,
    adapter: &CustomAdapter,
    stage: &Stage,
    routed: Option<&Provider>,
    extra_args: &[String],
    prompt: &str,
    paths: &ContextPaths<'_>,
    launch: Launch<'_>,
) -> Result<()> {
    let interactive = stage.mode == Mode::Interactive;
    invocation.kind = adapter.output.map_or(OutputKind::Text, |o| o.kind());
    // An untrusted adapter keeps its name but runs BLOCKED_COMMAND, which no
    // PATH provides: previews show it and launches fail.
    let template = match launch {
        Launch::Resume(_) => adapter
            .resume_args
            .as_ref()
            .with_context(|| format!("{name} does not define resume_args"))?,
        Launch::FollowUp(_) => adapter
            .follow_up_args
            .as_ref()
            .with_context(|| format!("{name} does not define follow_up_args"))?,
        Launch::New if interactive => adapter
            .interactive_args
            .as_ref()
            .with_context(|| format!("{name} does not define interactive_args"))?,
        Launch::New => &adapter.args,
    };
    let session = match launch {
        Launch::Resume(session) | Launch::FollowUp(session) => session,
        Launch::New => "",
    };
    let prompt_file = paths.prompt_file.display().to_string();
    let handoff_file = paths.handoff_file.display().to_string();
    let workspace = paths.workspace.display().to_string();
    let run_dir = paths.attempt_dir.display().to_string();
    let base_url = routed.map_or("", |p| p.base_url.as_str());
    let api_key_env = routed.map_or("", |p| p.api_key_env.as_str());
    let values = [
        ("{prompt}", prompt),
        ("{model}", stage.model.as_str()),
        ("{provider}", stage.provider.as_str()),
        ("{effort}", stage.effort.as_str()),
        ("{prompt_file}", prompt_file.as_str()),
        ("{handoff_file}", handoff_file.as_str()),
        ("{workspace}", workspace.as_str()),
        ("{run_dir}", run_dir.as_str()),
        ("{base_url}", base_url),
        ("{api_key_env}", api_key_env),
        ("{session}", session),
    ];
    // A delegating turn's MCP config, for `mcp_config_args` and `{mcp_config}`.
    let mcp_config = match (paths.mcp, &adapter.mcp_config_args) {
        (Some(mcp), Some(_)) => {
            let file = paths.attempt_dir.join("mcp.json");
            invocation.files.push((file.clone(), mcp.claude_config()));
            mcp.set_env(invocation);
            file.display().to_string()
        }
        _ => String::new(),
    };
    let values: Vec<(&str, &str)> = values
        .into_iter()
        .chain([("{mcp_config}", mcp_config.as_str())])
        .collect();
    let a = &mut invocation.args;
    a.extend(template.iter().map(|arg| expand(arg, &values)));
    // Unless the template places the config itself.
    if let (false, Some(args)) = (mcp_config.is_empty(), &adapter.mcp_config_args) {
        if !template.iter().any(|t| t.contains("{mcp_config}")) {
            a.extend(args.iter().map(|arg| expand(arg, &values)));
        }
    }
    for (flag, value) in [
        (&adapter.model_flag, &stage.model),
        (&adapter.effort_flag, &stage.effort),
    ] {
        if let (Some(flag), false) = (flag, value.is_empty()) {
            a.extend([flag.clone(), value.clone()]);
        }
    }
    a.extend(extra_args.iter().cloned());
    if matches!(launch, Launch::Resume(_)) {
        return Ok(());
    }
    match &adapter.prompt {
        Some(CustomPrompt::Mode(PromptMode::Arg)) => a.push(prompt.into()),
        Some(CustomPrompt::Flag { flag }) => a.extend([flag.clone(), prompt.into()]),
        _ => {
            if adapter.stdin_prompt() && !interactive {
                invocation.stdin = Some(prompt.into());
            }
        }
    }
    if let Some(flag) = adapter.session.as_ref().and_then(|s| s.assign.as_ref()) {
        invocation.assign = Some(AssignSession {
            flag: flag.clone(),
            prepend: false,
            conflicts: vec![flag.clone()],
        });
    }
    Ok(())
}

type Env = Vec<(String, EnvValue)>;

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

/// Environment that points a harness at a router-defined provider, per its
/// `Routing`. The key variable is always listed so a missing key fails
/// before launch.
fn route_env(stage: &Stage, provider: &Provider) -> Result<Env> {
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
    let routing = harness::get(&stage.harness).map(|spec| spec.routing);
    match routing {
        Some(Routing::OpencodeJson) => env.push(opencode_provider_env(stage, provider)),
        Some(Routing::OpenAiEnv(openai)) => {
            for prefix in openai.strip_prefixes {
                env.push(((*prefix).into(), EnvValue::UnsetPrefix));
            }
            match openai.base {
                BaseEnv::Url(var) => env.push((var.into(), literal(&provider.base_url))),
                BaseEnv::HostPath { host, path, suffix } => {
                    let url = reqwest::Url::parse(&provider.base_url)?;
                    let origin = url.origin().ascii_serialization();
                    let trimmed = url.path().trim_matches('/');
                    let full = if trimmed.is_empty() {
                        suffix.to_owned()
                    } else {
                        format!("{trimmed}/{suffix}")
                    };
                    env.push((host.into(), literal(&origin)));
                    env.push((path.into(), literal(&full)));
                }
            }
            env.push((openai.key_var.into(), key()));
        }
        _ => {}
    }
    Ok(env)
}

/// OpenCode provider entry (escape hatch): an isolated ID merged into the
/// inherited `OPENCODE_CONFIG_CONTENT`.
fn opencode_provider_env(stage: &Stage, provider: &Provider) -> (String, EnvValue) {
    let mut options = serde_json::json!({ "baseURL": &provider.base_url });
    if !provider.api_key_env.is_empty() {
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
    (
        "OPENCODE_CONFIG_CONTENT".into(),
        EnvValue::JsonInsert {
            path: vec!["provider".into(), router_provider_id(&stage.provider)],
            value: entry.to_string(),
        },
    )
}

/// Expand only tokens present in the template, never tokens introduced by user input.
fn expand(template: &str, values: &[(&str, &str)]) -> String {
    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(brace) = rest.find('{') {
        output.push_str(&rest[..brace]);
        rest = &rest[brace..];
        if let Some((key, value)) = values.iter().find(|(key, _)| rest.starts_with(*key)) {
            output.push_str(value);
            rest = &rest[key.len()..];
        } else {
            output.push('{');
            rest = &rest[1..];
        }
    }
    output.push_str(rest);
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
                    ("{prompt}", "literal {model} $(touch x)"),
                    ("{model}", "chosen")
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
            mcp: None,
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
            mcp: None,
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
    fn launch_files_never_write_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, "keep").unwrap();
        let file = dir.path().join("output-schema.json");
        std::os::unix::fs::symlink(&victim, &file).unwrap();
        let invocation = Invocation {
            program: "true".into(),
            args: vec![],
            stdin: None,
            kind: OutputKind::Text,
            final_file: dir.path().join("final"),
            env: vec![],
            files: vec![(file.clone(), "{}".into())],
            assign: None,
        };
        invocation.write_files().unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(fs::read_to_string(&file).unwrap(), "{}");
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
            mcp: None,
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
                "--json-schema",
                &handoff::compact_schema(),
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
            resumed("antigravity").args,
            ["--resume", "s-1", "--model", "m", "--effort", "high"]
        );
        assert_eq!(
            resumed("amp").args,
            ["threads", "continue", "--mode", "m", "s-1"]
        );
        assert_eq!(resumed("opencode").args, ["--session", "s-1"]);
        // A recorded session is its own argument, so never an option.
        assert!(
            resume(
                &stage("claude", Mode::Unattended),
                &none,
                &providers,
                "--dangerously-skip-permissions",
                &paths
            )
            .is_err()
        );

        assert!(!resumable("copilot", &none));
        assert!(
            resume(
                &stage("copilot", Mode::Unattended),
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
            mcp: None,
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
            mcp: None,
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
                ..CustomAdapter::default()
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
            mcp: None,
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
            mcp: None,
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
            mcp: None,
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
            assign: None,
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
            assign: None,
        };
        let env = i.environment().unwrap();
        assert_eq!(env.len(), 1);
        let merged: serde_json::Value = serde_json::from_str(&env[0].1).unwrap();
        assert_eq!(merged["model"], "p/m");
        assert!(merged["provider"]["p"].is_object());
    }

    #[test]
    fn opencode_uses_2x_flags_unless_an_older_version_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let spec = harness::get("opencode").unwrap();
        let run = |interactive: bool, model: &str, effort: &str, legacy: bool| {
            let stage = Stage {
                harness: "opencode".into(),
                model: model.into(),
                effort: effort.into(),
                mode: if interactive {
                    Mode::Interactive
                } else {
                    Mode::Unattended
                },
                ..Stage::default()
            };
            let mut invocation = Invocation {
                program: "opencode".into(),
                args: vec![],
                stdin: None,
                kind: OutputKind::Text,
                final_file: PathBuf::from("unused"),
                env: vec![],
                files: vec![],
                assign: None,
            };
            build_spec(
                spec,
                &mut invocation,
                &stage,
                None,
                &[],
                "task",
                &paths,
                legacy,
                Launch::New,
            )
            .map(|()| invocation)
        };
        let i = run(false, "p/m", "high", false).unwrap();
        assert_eq!(
            i.args,
            ["run", "--format", "json", "--model", "p/m#high", "task"]
        );
        assert!(i.env.is_empty());
        let i = run(true, "p/m", "", false).unwrap();
        assert_eq!(i.args, ["--prompt", "task"]);
        assert_eq!(
            i.env,
            [(
                "OPENCODE_CONFIG_CONTENT".to_owned(),
                EnvValue::JsonInsert {
                    path: vec!["model".into()],
                    value: "\"p/m\"".into()
                }
            )]
        );
        assert!(run(true, "p/m", "high", false).is_err());
        assert!(run(false, "", "high", false).is_err());
        let i = run(true, "p/m", "high", true).unwrap();
        assert_eq!(
            i.args,
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
