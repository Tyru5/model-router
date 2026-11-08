use crate::providers::Provider;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

/// Built-in harness names, from the spec table (`harness::SPECS`).
pub const BUILTINS: &[&str] = &crate::harness::NAMES;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Unattended,
    Interactive,
}
impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unattended => "unattended",
            Self::Interactive => "interactive",
        }
    }
}

/// OS-level filesystem sandbox around a stage's harness; see `sandbox`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum Sandbox {
    /// No router sandbox; the harness's own permissions still apply.
    #[default]
    Off,
    /// The workspace and its Git directory are writable too.
    WorkspaceWrite,
    /// Only the attempt, handoff, temporary, and harness state directories are writable.
    ReadOnly,
    /// Read-only for research, planning, review, and summary stages;
    /// workspace-write for the rest.
    ByRole,
}
impl Sandbox {
    pub const ALL: [Self; 4] = [
        Self::Off,
        Self::ReadOnly,
        Self::WorkspaceWrite,
        Self::ByRole,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::WorkspaceWrite => "workspace-write",
            Self::ReadOnly => "read-only",
            Self::ByRole => "by-role",
        }
    }
    /// The concrete mode for a stage of `role`.
    pub fn resolve(self, role: Role) -> Self {
        match (self, role) {
            (Self::ByRole, Role::Research | Role::Planning | Role::Review | Role::Summary) => {
                Self::ReadOnly
            }
            (Self::ByRole, _) => Self::WorkspaceWrite,
            (mode, _) => mode,
        }
    }
}

/// What a stage does when its sandbox cannot be set up.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxFallback {
    /// Fail the stage; never run it unsandboxed.
    #[default]
    Refuse,
    /// Run unsandboxed and record a warning.
    Warn,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Research,
    Planning,
    Implementation,
    Review,
    Test,
    /// Summarizes the run's work for the person reviewing it.
    Summary,
    /// The router commits the run's changes, optionally pushes and opens a
    /// draft PR; no harness runs. See `ship`.
    Ship,
    #[default]
    Custom,
}
impl Role {
    pub const ALL: [Self; 8] = [
        Self::Research,
        Self::Planning,
        Self::Implementation,
        Self::Review,
        Self::Test,
        Self::Summary,
        Self::Ship,
        Self::Custom,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Research => "research",
            Self::Planning => "planning",
            Self::Implementation => "implementation",
            Self::Review => "review",
            Self::Test => "test",
            Self::Summary => "summary",
            Self::Ship => "ship",
            Self::Custom => "custom",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Stage {
    pub id: String,
    pub name: String,
    pub role: Role,
    pub enabled: bool,
    pub harness: String,
    /// Harness-native model ID, alias, or (for Amp) mode. Empty inherits its default.
    pub model: String,
    pub provider: String,
    pub effort: String,
    pub mode: Mode,
    pub instructions: String,
    pub extra_args: Vec<String>,
    /// Explicit harness-native settings for this stage; empty inherits native configuration.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub harness_settings: BTreeMap<String, String>,
    pub timeout_seconds: u64,
    /// An upstream implementation stage ID; empty chooses the nearest enabled one.
    pub repair_target: String,
    /// Commands the router runs after the harness succeeds, independent of its handoff.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Check>,
    /// Test stages only: model-reported passes never satisfy the gate; the
    /// stage needs a passing required router check (or an explicit waiver).
    #[serde(skip_serializing_if = "is_false")]
    pub require_router_checks: bool,
    /// Router sandbox for this stage; unset inherits the workflow's `sandbox_default`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<Sandbox>,
    /// Extra paths the sandbox leaves writable (`~/` expands to the home directory).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sandbox_writable: Vec<String>,
    /// False cuts the sandboxed harness off the network.
    #[serde(skip_serializing_if = "is_true")]
    pub sandbox_network: bool,
    #[serde(skip_serializing_if = "is_refuse")]
    pub sandbox_fallback: SandboxFallback,
    /// Caps on this stage's spend across its attempts; see `cost`.
    #[serde(skip_serializing_if = "Budget::is_empty")]
    pub budget: Budget,
    /// Ship stage settings; None in a ship stage means the defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ship: Option<crate::ship::Options>,
    /// Stage IDs this stage runs after. Set on any stage, the workflow is a
    /// dependency graph instead of a list; see `graph`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// Best-of-N: candidates run in their own worktrees and a judge picks one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fan_out: Option<FanOut>,
}
fn is_true(value: &bool) -> bool {
    *value
}
fn is_false(value: &bool) -> bool {
    !*value
}
fn is_refuse(value: &SandboxFallback) -> bool {
    *value == SandboxFallback::Refuse
}
fn is_off(value: &Sandbox) -> bool {
    *value == Sandbox::Off
}
impl Stage {
    /// The concrete sandbox mode within `workflow`: never `ByRole`.
    pub fn effective_sandbox(&self, workflow: &Workflow) -> Sandbox {
        self.sandbox
            .unwrap_or(workflow.sandbox_default)
            .resolve(self.role)
    }
}

/// `judge` value that picks the winner without a model.
pub const AUTO_JUDGE: &str = "auto";

/// Several harness/model candidates for one stage, each in its own Git
/// worktree. The judge, `auto` or a review stage's ID, picks the one whose
/// changes the router applies to the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanOut {
    pub candidates: Vec<Candidate>,
    #[serde(default = "auto_judge")]
    pub judge: String,
}
fn auto_judge() -> String {
    AUTO_JUDGE.into()
}
impl FanOut {
    /// Each candidate's ID: its own, else `c1`, `c2`, … by position.
    pub fn ids(&self) -> Vec<String> {
        self.candidates
            .iter()
            .enumerate()
            .map(|(index, c)| c.key(index))
            .collect()
    }
    pub fn is_auto(&self) -> bool {
        self.judge == AUTO_JUDGE
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Candidate {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    pub harness: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub effort: String,
}
impl Candidate {
    /// The ID at `index`: its own, else `c<index + 1>`.
    pub fn key(&self, index: usize) -> String {
        if self.id.is_empty() {
            format!("c{}", index + 1)
        } else {
            self.id.clone()
        }
    }
    /// `stage` as this candidate runs it: its harness, model, provider, and
    /// effort. Harness-specific settings and arguments stay only when the
    /// harness is the stage's own.
    pub fn stage(&self, stage: &Stage) -> Stage {
        let mut out = stage.clone();
        if self.harness != stage.harness {
            out.harness_settings.clear();
            out.extra_args.clear();
        }
        out.harness = self.harness.clone();
        out.model = self.model.clone();
        out.provider = self.provider.clone();
        out.effort = self.effort.clone();
        out.fan_out = None;
        out
    }
}

impl Default for Stage {
    fn default() -> Self {
        Self {
            id: format!("stage-{}", &uuid::Uuid::new_v4().to_string()[..8]),
            name: "New stage".into(),
            role: Role::Custom,
            enabled: true,
            harness: "codex".into(),
            model: String::new(),
            provider: String::new(),
            effort: String::new(),
            mode: Mode::Unattended,
            instructions: String::new(),
            extra_args: vec![],
            harness_settings: BTreeMap::new(),
            timeout_seconds: 3600,
            repair_target: String::new(),
            checks: vec![],
            require_router_checks: false,
            sandbox: None,
            sandbox_writable: vec![],
            sandbox_network: true,
            sandbox_fallback: SandboxFallback::Refuse,
            budget: Budget::default(),
            ship: None,
            after: vec![],
            fan_out: None,
        }
    }
}

/// Spend caps. A run stops before its next stage once known estimated cost
/// exceeds `max_usd` or counted tokens exceed `max_tokens` (input, output,
/// and cache writes). Unknown fields warn, like the workflow's.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Budget {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
}
impl Budget {
    pub fn is_empty(&self) -> bool {
        self.max_usd.is_none() && self.max_tokens.is_none()
    }
    fn validate(&self, scope: &str) -> Result<()> {
        ensure!(
            self.max_usd.is_none_or(|usd| usd.is_finite() && usd > 0.0),
            "{scope} budget max_usd must be a positive amount"
        );
        ensure!(
            self.max_tokens != Some(0),
            "{scope} budget max_tokens must be positive"
        );
        Ok(())
    }
}

/// An argument vector run in the workspace; no shell evaluates it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Check {
    pub command: Vec<String>,
    pub required: bool,
    pub timeout_seconds: u64,
}
impl Default for Check {
    fn default() -> Self {
        Self {
            command: vec![],
            required: true,
            timeout_seconds: 600,
        }
    }
}

/// Unknown fields here are warnings (see `unknown_fields`), so a newer
/// binary's additions do not break older ones; stages and checks stay strict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub name: String,
    #[serde(default = "two")]
    pub max_repair_passes: u32,
    /// Sandbox for stages that do not set their own; see `Sandbox`.
    #[serde(default, skip_serializing_if = "is_off")]
    pub sandbox_default: Sandbox,
    /// Harness processes a graph workflow or fan-out may run at once.
    #[serde(default = "two", skip_serializing_if = "is_two")]
    pub max_parallel: u32,
    pub stages: Vec<Stage>,
    /// Caps on the whole run's spend; see `cost`.
    #[serde(default, skip_serializing_if = "Budget::is_empty")]
    pub budget: Budget,
}
fn two() -> u32 {
    2
}
fn is_two(value: &u32) -> bool {
    *value == 2
}
impl Workflow {
    /// A workflow with `stages` and default settings: two repair passes, two
    /// parallel processes, no sandbox default, and no budget.
    pub fn new(name: impl Into<String>, stages: Vec<Stage>) -> Self {
        Self {
            name: name.into(),
            max_repair_passes: two(),
            sandbox_default: Sandbox::default(),
            max_parallel: two(),
            stages,
            budget: Budget::default(),
        }
    }
    /// Appends the review summary unless a stage already has the summary role.
    /// Run once, by the v1→v2 migration: deleting the stage later opts out.
    /// The preset runs on Codex; when Codex is not installed the stage uses
    /// the first enabled stage's harness and its default model instead.
    pub fn ensure_summary(&mut self) {
        if self.stages.len() >= 64 || self.stages.iter().any(|s| s.role == Role::Summary) {
            return;
        }
        let mut stage = crate::presets::stage("summary").build();
        if crate::adapters::executable(&stage.harness).is_none() {
            if let Some(first) = self.stages.iter().find(|s| s.enabled) {
                if first.harness != stage.harness {
                    stage.harness = first.harness.clone();
                    stage.model.clear();
                    stage.effort.clear();
                    stage.harness_settings.clear();
                }
            }
        }
        let base = stage.id.clone();
        let mut suffix = 2;
        while self.stages.iter().any(|s| s.id == stage.id) {
            stage.id = format!("{base}-{suffix}");
            suffix += 1;
        }
        self.stages.push(stage);
    }
}
impl Default for Workflow {
    fn default() -> Self {
        Self { name: "Feature development".into(), max_repair_passes: 2, sandbox_default: Sandbox::Off, budget: Budget::default(), max_parallel: 2, stages: vec![
        Stage { id: "research".into(), name: "Investigate".into(), role: Role::Research, model: "gpt-6-astra".into(),
            instructions: "Investigate the task and relevant project files. Explain the root cause or requirements, cite evidence, and propose a concrete implementation plan. Do not change project code.".into(), ..Stage::default() },
        Stage { id: "implement".into(), name: "Implement".into(), role: Role::Implementation, harness: "claude".into(), model: "sonnet".into(), mode: Mode::Interactive,
            instructions: "Implement the task using the prior findings. Follow project conventions, run appropriate checks, and report what changed and any remaining issues.".into(), ..Stage::default() },
        Stage { id: "review".into(), name: "Review (optional)".into(), role: Role::Review, enabled: false, model: "gpt-6-astra".into(),
            instructions: "Review the implementation and test evidence. Do not modify code. Distinguish blocking correctness issues and failed required checks from advisory suggestions. Give an explicit pass or needs_changes verdict.".into(), ..Stage::default() },
        crate::presets::stage("summary").build(),
    ] }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomAdapter {
    pub command: String,
    pub args: Vec<String>,
    pub interactive_args: Option<Vec<String>>,
    /// Optional arguments to list model IDs (one per line) using the same executable.
    pub models_args: Option<Vec<String>>,
    pub prompt_stdin: bool,
    /// Output parser for unattended runs; plain text by default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<CustomOutput>,
    /// How the router learns the session: `{ assign = "--session-id" }` or
    /// `{ reported = true }` (read from `output` events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<CustomSession>,
    /// Interactive arguments that reopen `{session}`; enables takeover.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_args: Option<Vec<String>>,
    /// `FLAG model`, after `args`, when the stage sets a model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_flag: Option<String>,
    /// `FLAG effort`, after the model, when the stage sets an effort.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort_flag: Option<String>,
    /// How the prompt is passed after the arguments: `"stdin"` (as
    /// `prompt_stdin`), `"arg"`, or `{ flag = "-p" }`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<CustomPrompt>,
}

/// A custom adapter's output parser, named after the built-in formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CustomOutput {
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "codex-jsonl")]
    CodexJsonl,
    #[serde(rename = "claude-stream")]
    ClaudeStream,
    #[serde(rename = "pi")]
    Pi,
    #[serde(rename = "cursor-stream")]
    CursorStream,
    #[serde(rename = "gemini-json")]
    GeminiJson,
    #[serde(rename = "opencode")]
    Opencode,
    #[serde(rename = "amp-stream")]
    AmpStream,
}
impl CustomOutput {
    pub fn kind(self) -> crate::adapters::OutputKind {
        use crate::adapters::OutputKind;
        match self {
            Self::Text => OutputKind::Text,
            Self::CodexJsonl => OutputKind::Codex,
            Self::ClaudeStream => OutputKind::Claude,
            Self::Pi => OutputKind::Pi,
            Self::CursorStream => OutputKind::Cursor,
            Self::GeminiJson => OutputKind::Gemini,
            Self::Opencode => OutputKind::OpenCode,
            Self::AmpStream => OutputKind::Amp,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CustomSession {
    /// The router picks an ID and appends `FLAG id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assign: Option<String>,
    /// The harness reports its session in its output events.
    pub reported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CustomPrompt {
    Mode(PromptMode),
    Flag { flag: String },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptMode {
    Stdin,
    Arg,
}
impl CustomAdapter {
    /// Whether unattended runs send the prompt on standard input.
    pub fn stdin_prompt(&self) -> bool {
        self.prompt_stdin || self.prompt == Some(CustomPrompt::Mode(PromptMode::Stdin))
    }
    /// Arguments other than `command` that reach the executable, by field,
    /// for trust digests. Old adapters add nothing beyond `args`.
    pub fn launch_fields(&self) -> Vec<(&'static str, Vec<String>)> {
        let mut fields = vec![];
        if let Some(args) = &self.resume_args {
            fields.push(("resume", args.clone()));
        }
        for (name, flag) in [
            ("model flag", &self.model_flag),
            ("effort flag", &self.effort_flag),
        ] {
            if let Some(flag) = flag {
                fields.push((name, vec![flag.clone()]));
            }
        }
        match &self.prompt {
            Some(CustomPrompt::Flag { flag }) => fields.push(("prompt flag", vec![flag.clone()])),
            Some(CustomPrompt::Mode(PromptMode::Arg)) => {
                fields.push(("prompt", vec!["arg".into()]))
            }
            _ => {}
        }
        if let Some(flag) = self.session.as_ref().and_then(|s| s.assign.as_ref()) {
            fields.push(("session flag", vec![flag.clone()]));
        }
        fields
    }
    fn validate(&self, name: &str) -> Result<()> {
        ensure!(
            !self.command.trim().is_empty(),
            "Custom adapter {name} needs a command"
        );
        let flag = |value: &str| value.starts_with('-') && !value.contains(char::is_whitespace);
        for (field, value) in [
            ("model_flag", &self.model_flag),
            ("effort_flag", &self.effort_flag),
        ] {
            if let Some(value) = value {
                ensure!(
                    flag(value),
                    "Custom adapter {name} {field} must be a flag like --model"
                );
            }
        }
        if let Some(CustomPrompt::Flag { flag: value }) = &self.prompt {
            ensure!(
                flag(value),
                "Custom adapter {name} prompt flag must be a flag like -p"
            );
        }
        ensure!(
            !self.prompt_stdin
                || matches!(
                    self.prompt,
                    None | Some(CustomPrompt::Mode(PromptMode::Stdin))
                ),
            "Custom adapter {name} sets both prompt_stdin and another prompt"
        );
        if let Some(session) = &self.session {
            ensure!(
                session.assign.is_some() != session.reported,
                "Custom adapter {name} session needs exactly one of assign or reported"
            );
            if let Some(value) = &session.assign {
                ensure!(
                    flag(value),
                    "Custom adapter {name} session assign must be a flag like --session-id"
                );
            }
        }
        ensure!(
            self.resume_args.is_none() || self.session.is_some(),
            "Custom adapter {name} resume_args needs a session (assign or reported)"
        );
        Ok(())
    }
}

/// The config format this binary writes. Older files migrate on load.
pub const VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub workflows: Vec<Workflow>,
    pub adapters: BTreeMap<String, CustomAdapter>,
    /// OpenAI-compatible providers routed into harnesses; see `providers`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, Provider>,
    /// Price overrides by model ID, USD per million tokens; see `cost`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub pricing: BTreeMap<String, crate::cost::Price>,
    /// Load notes: unknown fields, migrations, effort levels, trust. Not saved.
    #[serde(skip)]
    pub warnings: Vec<String>,
    /// Sections an untrusted project config had ignored; restored on save.
    #[serde(skip)]
    pub ignored: Option<Box<crate::trust::Ignored>>,
    /// Explicit approval source selected by the caller. Never accepted from TOML.
    #[serde(skip)]
    pub explicit_config: Option<PathBuf>,
    /// Approval policy of the config originally loaded, before local edits.
    #[serde(skip)]
    pub loaded_approval: Option<String>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            version: VERSION,
            workflows: vec![Workflow::default()],
            adapters: BTreeMap::new(),
            providers: BTreeMap::new(),
            pricing: BTreeMap::new(),
            warnings: vec![],
            ignored: None,
            explicit_config: None,
            loaded_approval: None,
        }
    }
}

/// `(from version, note, upgrade)`.
type Migration = (u32, &'static str, fn(&mut Settings));

/// Upgrades from each version to the next, in order. A migration changes
/// only what its version introduced.
const MIGRATIONS: &[Migration] = &[(
    1,
    "added a review summary stage to workflows without one",
    migrate_v1,
)];

fn migrate_v1(settings: &mut Settings) {
    settings
        .workflows
        .iter_mut()
        .for_each(Workflow::ensure_summary);
}

/// Runs the migrations from `settings.version` to `VERSION`; returns notes.
pub fn migrate(settings: &mut Settings) -> Result<Vec<String>> {
    ensure!(
        settings.version >= 1,
        "Unsupported config version {}",
        settings.version
    );
    ensure!(
        settings.version <= VERSION,
        "Config version {} is newer than this model-router supports ({VERSION}); upgrade model-router",
        settings.version
    );
    let mut notes = vec![];
    for (from, note, migration) in MIGRATIONS {
        if settings.version == *from {
            migration(settings);
            settings.version = from + 1;
            notes.push(format!("Migrated config v{from}→v{}: {note}.", from + 1));
        }
    }
    Ok(notes)
}

/// Fields in `value` that `Settings` would ignore, as dotted paths. Stage and
/// check fields are strict and fail parsing instead.
pub fn unknown_fields(value: &toml::Value) -> Vec<String> {
    fn keys<T: Serialize>(sample: &T) -> HashSet<String> {
        toml::Value::try_from(sample)
            .ok()
            .and_then(|v| v.as_table().map(|t| t.keys().cloned().collect()))
            .unwrap_or_default()
    }
    fn extra(table: &toml::Value, known: &HashSet<String>, at: &str, out: &mut Vec<String>) {
        if let Some(table) = table.as_table() {
            for key in table.keys().filter(|k| !known.contains(*k)) {
                out.push(if at.is_empty() {
                    key.clone()
                } else {
                    format!("{at}.{key}")
                });
            }
        }
    }
    let mut sample = Settings::default();
    sample.providers.insert("p".into(), Provider::default());
    let price = crate::cost::Price {
        input: 1.0,
        output: 1.0,
        cache_read: Some(1.0),
        cache_write: Some(1.0),
    };
    sample.pricing.insert("m".into(), price);
    let budget = keys(&Budget {
        max_usd: Some(1.0),
        max_tokens: Some(1),
    });
    let adapter = CustomAdapter {
        interactive_args: Some(vec![]),
        models_args: Some(vec![]),
        output: Some(CustomOutput::Text),
        session: Some(CustomSession::default()),
        resume_args: Some(vec![]),
        model_flag: Some(String::new()),
        effort_flag: Some(String::new()),
        prompt: Some(CustomPrompt::Mode(PromptMode::Arg)),
        ..CustomAdapter::default()
    };
    let provider = Provider {
        context_limit: Some(1),
        output_limit: Some(1),
        ..Provider::default()
    };
    let mut out = vec![];
    extra(value, &keys(&sample), "", &mut out);
    let workflow = keys(&Workflow {
        sandbox_default: Sandbox::ByRole,
        // Saved only when not the default.
        max_parallel: 3,
        ..Workflow::default()
    });
    for (index, item) in value
        .get("workflows")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let at = format!("workflows[{index}]");
        extra(item, &workflow, &at, &mut out);
        if let Some(value) = item.get("budget") {
            extra(value, &budget, &format!("{at}.budget"), &mut out);
        }
        for (number, stage) in item
            .get("stages")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            if let Some(value) = stage.get("budget") {
                extra(
                    value,
                    &budget,
                    &format!("{at}.stages[{number}].budget"),
                    &mut out,
                );
            }
        }
    }
    for (section, known) in [
        ("adapters", keys(&adapter)),
        ("providers", keys(&provider)),
        ("pricing", keys(&price)),
    ] {
        for (name, item) in value
            .get(section)
            .and_then(toml::Value::as_table)
            .into_iter()
            .flatten()
        {
            extra(item, &known, &format!("{section}.{name}"), &mut out);
        }
    }
    out
}

/// Parses a config file's text: unknown fields become warnings, older
/// versions migrate, and strict stage/check fields explain their errors.
pub fn parse(text: &str) -> Result<Settings> {
    let mut value: toml::Value = toml::from_str(text)?;
    // Files from before versioning mattered omit it: they are v1.
    if let Some(table) = value.as_table_mut() {
        table.entry("version").or_insert(toml::Value::Integer(1));
    }
    let unknown = unknown_fields(&value);
    let mut settings: Settings = value.try_into().map_err(|error: toml::de::Error| {
        let message = error.to_string();
        if message.contains("unknown field") {
            anyhow::anyhow!(
                "{message}\nStage and check fields are strict so typos fail loudly; fix the field name (see `model-router init` for an example)."
            )
        } else {
            anyhow::anyhow!(message)
        }
    })?;
    settings.warnings = unknown
        .into_iter()
        .map(|field| {
            format!("Unknown config field {field} ignored (from a newer model-router or a typo).")
        })
        .collect();
    let notes = migrate(&mut settings)?;
    settings.warnings.extend(notes);
    Ok(settings)
}

/// Whether `id` can be a stage or fan-out candidate ID: nonempty letters,
/// digits, '-' and '_'. IDs name directories and worktrees.
pub fn is_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == VERSION,
            "Unsupported config version {}",
            self.version
        );
        ensure!(
            !self.workflows.is_empty(),
            "At least one workflow is required"
        );
        let mut names = HashSet::new();
        for workflow in &self.workflows {
            ensure!(
                !workflow.name.trim().is_empty() && names.insert(&workflow.name),
                "Workflow names must be nonempty and unique"
            );
            ensure!(
                !workflow.stages.is_empty() && workflow.stages.len() <= 64,
                "Workflow {} must contain 1–64 stages",
                workflow.name
            );
            workflow
                .budget
                .validate(&format!("Workflow {}", workflow.name))?;
            ensure!(
                (1..=16).contains(&workflow.max_parallel),
                "Workflow {} max_parallel must be 1–16",
                workflow.name
            );
            let mut ids = HashSet::new();
            for stage in &workflow.stages {
                ensure!(ids.insert(&stage.id), "Duplicate stage ID {}", stage.id);
            }
            crate::graph::validate(workflow)
                .with_context(|| format!("Workflow {}", workflow.name))?;
            let order = crate::graph::order(workflow);
            for (index, stage) in workflow.stages.iter().enumerate() {
                ensure!(
                    is_id(&stage.id),
                    "Stage IDs must contain only letters, digits, '-' and '_'"
                );
                ensure!(
                    !stage.name.trim().is_empty(),
                    "Stage {} needs a name",
                    stage.id
                );
                ensure!(
                    stage.timeout_seconds > 0,
                    "Stage {} needs a positive timeout",
                    stage.name
                );
                ensure!(
                    stage.instructions.len() <= 8192,
                    "Stage instructions must be at most 8192 bytes"
                );
                ensure!(
                    !stage.model.contains('\0') && !stage.provider.contains('\0'),
                    "Model and provider cannot contain NUL"
                );
                ensure!(
                    BUILTINS.contains(&stage.harness.as_str())
                        || self.adapters.contains_key(&stage.harness),
                    "Unknown harness {}",
                    stage.harness
                );
                stage.budget.validate(&format!("Stage {}", stage.name))?;
                ensure!(
                    stage.checks.len() <= 16,
                    "Stage {} can have at most 16 checks",
                    stage.name
                );
                ensure!(
                    stage.sandbox_writable.len() <= 32
                        && stage.sandbox_writable.iter().all(|p| {
                            !p.contains('\0') && (p.starts_with('/') || p.starts_with("~/"))
                        }),
                    "Stage {} sandbox_writable paths must be absolute or start with ~/ (at most 32)",
                    stage.name
                );
                for check in &stage.checks {
                    ensure!(
                        check
                            .command
                            .first()
                            .is_some_and(|program| !program.trim().is_empty()),
                        "Stage {} has a check without a command",
                        stage.name
                    );
                    ensure!(
                        !check.command.iter().any(|arg| arg.contains('\0')),
                        "Check commands cannot contain NUL"
                    );
                    ensure!(
                        check.timeout_seconds > 0,
                        "Stage {} needs a positive check timeout",
                        stage.name
                    );
                }
                // Without a required check (none configured, or ignored while
                // the project is untrusted) the stage fails closed and blocks.
                ensure!(
                    !stage.require_router_checks || stage.role == Role::Test,
                    "Only test stages can set require_router_checks ({})",
                    stage.name
                );
                if stage.enabled && !stage.repair_target.is_empty() {
                    ensure!(
                        stage.role == Role::Review,
                        "Only reviews can set a repair target"
                    );
                    ensure!(
                        workflow.stages.iter().enumerate().any(|(i, s)| {
                            order.precedes(i, index)
                                && s.id == stage.repair_target
                                && s.role == Role::Implementation
                                && s.enabled
                        }),
                        "Review {} needs an enabled upstream implementation target",
                        stage.name
                    );
                }
                if stage.role == Role::Ship {
                    ensure!(
                        stage.checks.is_empty(),
                        "Ship stage {} cannot have checks",
                        stage.name
                    );
                    stage.ship.clone().unwrap_or_default().validate()?;
                } else {
                    ensure!(
                        stage.ship.is_none(),
                        "Stage {} has ship options but is not a ship stage",
                        stage.name
                    );
                    if stage.enabled {
                        crate::adapters::validate_options(stage, &self.providers)?;
                    }
                }
                for candidate in stage.fan_out.iter().flat_map(|f| &f.candidates) {
                    ensure!(
                        BUILTINS.contains(&candidate.harness.as_str())
                            || self.adapters.contains_key(&candidate.harness),
                        "Unknown harness {} in fan-out stage {}",
                        candidate.harness,
                        stage.id
                    );
                    ensure!(
                        !candidate.model.contains('\0') && !candidate.provider.contains('\0'),
                        "Model and provider cannot contain NUL"
                    );
                    if stage.enabled {
                        crate::adapters::validate_options(
                            &candidate.stage(stage),
                            &self.providers,
                        )?;
                    }
                }
            }
        }
        for (name, adapter) in &self.adapters {
            ensure!(
                !BUILTINS.contains(&name.as_str()),
                "Custom adapter cannot replace built-in {name}"
            );
            adapter.validate(name)?;
        }
        for (name, provider) in &self.providers {
            crate::providers::validate(name, provider)?;
        }
        for (model, price) in &self.pricing {
            ensure!(
                price.is_valid(),
                "Pricing for {model} must use finite, non-negative USD per million tokens"
            );
        }
        Ok(())
    }

    pub fn workflow(&self, name: Option<&str>) -> Result<&Workflow> {
        match name {
            Some(name) => self
                .workflows
                .iter()
                .find(|w| w.name == name)
                .with_context(|| format!("Unknown workflow {name:?}")),
            None => self.workflows.first().context("No workflows"),
        }
    }

    /// Saves at the current version. Sections ignored because the project
    /// was untrusted are written back unchanged.
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut saved = crate::trust::restore(self);
        saved.version = VERSION;
        saved.validate()?;
        crate::storage::atomic_write(path, toml::to_string_pretty(&saved)?.as_bytes())
    }

    /// Effort values outside the levels the harness documents. Warnings,
    /// because a newer harness may accept more.
    fn effort_warnings(&self) -> Vec<String> {
        let mut warnings = vec![];
        for workflow in &self.workflows {
            for stage in workflow.stages.iter().filter(|s| s.enabled) {
                if let Some(levels) = crate::catalog::known_efforts(&stage.harness) {
                    if !stage.effort.is_empty() && !levels.contains(&stage.effort.as_str()) {
                        warnings.push(format!(
                            "Stage {} effort {:?} is not a documented {} level ({}).",
                            stage.id,
                            stage.effort,
                            stage.harness,
                            levels.join(", ")
                        ));
                    }
                }
            }
        }
        warnings
    }
}

/// The user's config directory. `MODEL_ROUTER_CONFIG_DIR` overrides it, for
/// tests and portable setups.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MODEL_ROUTER_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    // Unit tests never read or write the user's settings, approvals, or
    // trust store.
    #[cfg(test)]
    {
        static DIR: std::sync::Mutex<Option<tempfile::TempDir>> = std::sync::Mutex::new(None);
        // Statics are never dropped, so remove the directory when the test
        // binary exits.
        extern "C" fn remove() {
            if let Ok(mut dir) = DIR.lock() {
                dir.take();
            }
        }
        let mut dir = DIR.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if dir.is_none() {
            *dir = Some(tempfile::tempdir().expect("a test config directory"));
            // SAFETY: `remove` is a plain function that never unwinds.
            unsafe { libc::atexit(remove) };
        }
        return dir.as_ref().unwrap().path().to_path_buf();
    }
    #[allow(unreachable_code)]
    directories::ProjectDirs::from("", "", "model-router")
        .map(|p| p.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".model-router-config"))
}

/// Where loaded settings came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Default,
    /// The user's global config: always trusted.
    Global,
    /// Named with `--config`: always trusted.
    Explicit,
    /// The project's `.model-router.toml`: trusted only once approved.
    Project,
}

#[derive(Debug, Clone)]
pub struct Loaded {
    /// Settings exactly as configured, dangerous sections included.
    pub settings: Settings,
    pub save_path: PathBuf,
    pub source: Option<PathBuf>,
    pub origin: Origin,
    pub trust: crate::trust::Trust,
}
impl Loaded {
    pub fn review(&self) -> Option<&crate::trust::Review> {
        match &self.trust {
            crate::trust::Trust::Untrusted(review) => Some(review),
            crate::trust::Trust::Trusted => None,
        }
    }
    /// Settings safe to use: an untrusted project's dangerous sections are
    /// ignored (and restored when saved).
    pub fn safe(self) -> Settings {
        let mut settings = self.settings;
        if matches!(self.trust, crate::trust::Trust::Untrusted(_)) {
            crate::trust::ignore(&mut settings);
        }
        settings
    }
}

/// Loads `explicit`, else the project's config, else the global one, and
/// checks the project's trust.
pub fn load_config(project: &Path, explicit: Option<&Path>) -> Result<Loaded> {
    let project_path = project.join(".model-router.toml");
    let global = config_dir().join("config.toml");
    let (source, origin) = if let Some(path) = explicit {
        (Some(path.to_path_buf()), Origin::Explicit)
    } else if project_path.is_file() {
        (Some(project_path.clone()), Origin::Project)
    } else if global.is_file() {
        (Some(global), Origin::Global)
    } else {
        (None, Origin::Default)
    };
    let mut settings = match &source {
        Some(path) => {
            parse(&fs::read_to_string(path).with_context(|| format!("Read {}", path.display()))?)
                .with_context(|| format!("Parse {}", path.display()))?
        }
        None => Settings::default(),
    };
    settings.explicit_config = explicit.map(Path::to_path_buf);
    settings.validate()?;
    settings.loaded_approval = Some(crate::trust::settings_approval(&settings));
    let effort = settings.effort_warnings();
    settings.warnings.extend(effort);
    let trust = match origin {
        Origin::Project => crate::trust::check(project, &settings),
        _ => crate::trust::Trust::Trusted,
    };
    // Inherited templates save into this project unless a config was explicitly selected.
    let save_path = explicit.map(Path::to_path_buf).unwrap_or(project_path);
    Ok(Loaded {
        settings,
        save_path,
        source,
        origin,
        trust,
    })
}

/// Settings safe to use without asking: an untrusted project's dangerous
/// sections are ignored. See `load_config` to ask the user instead.
pub fn load(project: &Path, explicit: Option<&Path>) -> Result<(Settings, PathBuf)> {
    let loaded = load_config(project, explicit)?;
    let save_path = loaded.save_path.clone();
    Ok((loaded.safe(), save_path))
}

pub const MAX_TASK_BYTES: usize = 32 * 1024;

pub fn validate_task(task: &str) -> Result<()> {
    if task.trim().is_empty() {
        bail!("Enter a task before starting a run");
    }
    ensure!(
        task.len() <= MAX_TASK_BYTES,
        "Task must be at most 32768 bytes; reference larger files by path"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHECKS: &str = r#"
version = 2
[[workflows]]
name = "Verify"
[[workflows.stages]]
id = "verify"
role = "test"
checks = [
  { command = ["cargo", "test", "--locked"] },
  { command = ["cargo", "clippy", "--", "-D", "warnings"], required = false, timeout_seconds = 300 },
]
"#;

    #[test]
    fn checks_parse_with_defaults_and_roundtrip() {
        let settings = parse(CHECKS).unwrap();
        settings.validate().unwrap();
        let checks = &settings.workflows[0].stages[0].checks;
        assert_eq!(checks[0].command, ["cargo", "test", "--locked"]);
        assert!(checks[0].required && checks[0].timeout_seconds == 600);
        assert!(!checks[1].required && checks[1].timeout_seconds == 300);
        let saved: Settings = toml::from_str(&toml::to_string_pretty(&settings).unwrap()).unwrap();
        assert_eq!(
            saved.workflows[0].stages[0].checks[1].command,
            checks[1].command
        );
        let plain: toml::Value =
            toml::from_str(&toml::to_string_pretty(&Settings::default()).unwrap()).unwrap();
        assert!(
            plain["workflows"][0]["stages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|stage| stage.get("checks").is_none()),
            "Unused checks must not be saved"
        );
    }

    #[test]
    fn sandbox_fields_parse_validate_and_stay_out_of_default_files() {
        let text = CHECKS
            .replace(
                "name = \"Verify\"",
                "name = \"Verify\"\nsandbox_default = \"by-role\"",
            )
            .replace(
                "role = \"test\"",
                "role = \"test\"\nsandbox = \"read-only\"\nsandbox_writable = [\"~/.npm\", \"/opt/cache dir\"]\nsandbox_network = false\nsandbox_fallback = \"warn\"",
            );
        let settings = parse(&text).unwrap();
        settings.validate().unwrap();
        assert!(settings.warnings.is_empty(), "{:?}", settings.warnings);
        let workflow = &settings.workflows[0];
        let stage = &workflow.stages[0];
        assert_eq!(workflow.sandbox_default, Sandbox::ByRole);
        assert_eq!(stage.sandbox, Some(Sandbox::ReadOnly));
        assert_eq!(stage.sandbox_writable, ["~/.npm", "/opt/cache dir"]);
        assert!(!stage.sandbox_network && stage.sandbox_fallback == SandboxFallback::Warn);
        let saved = toml::to_string_pretty(&settings).unwrap();
        assert_eq!(
            parse(&saved).unwrap().workflows[0].stages[0]
                .sandbox_writable
                .len(),
            2
        );
        let plain = toml::to_string_pretty(&Settings::default()).unwrap();
        assert!(!plain.contains("sandbox_"), "{plain}");
        assert!(parse(&text.replace("\"read-only\"", "\"strict\"")).is_err());
        let mut relative = settings.clone();
        relative.workflows[0].stages[0].sandbox_writable = vec!["cache".into()];
        assert!(
            relative
                .validate()
                .unwrap_err()
                .to_string()
                .contains("absolute")
        );
    }

    #[test]
    fn invalid_checks_are_rejected() {
        let error = |edit: fn(&mut Vec<Check>)| {
            let mut settings = parse(CHECKS).unwrap();
            edit(&mut settings.workflows[0].stages[0].checks);
            settings.validate().unwrap_err().to_string()
        };
        assert!(error(|c| *c = vec![Check::default()]).contains("without a command"));
        assert!(
            error(|c| c[0].command = vec![" ".into(), "x".into()]).contains("without a command")
        );
        assert!(error(|c| c[0].command.push("a\0b".into())).contains("NUL"));
        assert!(error(|c| c[0].timeout_seconds = 0).contains("positive check timeout"));
        assert!(error(|c| *c = vec![c[0].clone(); 17]).contains("at most 16"));
        let typo = parse(&CHECKS.replace("required = false", "shell = true")).unwrap_err();
        assert!(
            format!("{typo:#}").contains("unknown field `shell`")
                && format!("{typo:#}").contains("strict"),
            "{typo:#}"
        );
    }

    #[test]
    fn unknown_fields_outside_stages_warn_instead_of_failing() {
        let text = CHECKS.replace(
            "[[workflows]]\nname = \"Verify\"",
            "future_top = 1\n[providers.p]\nbase_url = \"http://localhost:1/v1\"\nretries = 3\n[adapters.a]\ncommand = \"a\"\nsandbox = true\n[[workflows]]\nname = \"Verify\"\ncolor = \"red\"",
        );
        let settings = parse(&text).unwrap();
        settings.validate().unwrap();
        let warnings = settings.warnings.join("\n");
        for field in [
            "future_top",
            "providers.p.retries",
            "adapters.a.sandbox",
            "workflows[0].color",
        ] {
            assert!(warnings.contains(field), "{field}: {warnings}");
        }
        assert!(!warnings.contains("base_url") && !warnings.contains("checks"));
        assert!(unknown_fields(&toml::from_str(CHECKS).unwrap()).is_empty());
    }

    #[test]
    fn budgets_and_pricing_roundtrip_and_validate() {
        let text = CHECKS.replace(
            "[[workflows]]\nname = \"Verify\"",
            "[pricing.\"my-model\"]\ninput = 1.5\noutput = 6.0\ncache_read = 0.15\n[[workflows]]\nname = \"Verify\"\n[workflows.budget]\nmax_usd = 5.0\nmax_tokens = 2_000_000\nhard = true",
        )
        .replace("role = \"test\"", "role = \"test\"\nbudget = { max_usd = 1.25, max_tokens = 9 }");
        let settings = parse(&text).unwrap();
        settings.validate().unwrap();
        let workflow = &settings.workflows[0];
        assert_eq!(workflow.budget.max_usd, Some(5.0));
        assert_eq!(workflow.budget.max_tokens, Some(2_000_000));
        assert_eq!(workflow.stages[0].budget.max_usd, Some(1.25));
        assert_eq!(settings.pricing["my-model"].cache_read, Some(0.15));
        assert_eq!(settings.pricing["my-model"].cache_write, None);
        // Future budget fields warn instead of failing.
        assert!(
            settings
                .warnings
                .iter()
                .any(|w| w.contains("workflows[0].budget.hard")),
            "{:?}",
            settings.warnings
        );
        let saved = parse(&toml::to_string_pretty(&settings).unwrap()).unwrap();
        assert_eq!(saved.workflows[0].budget, workflow.budget);
        assert_eq!(
            saved.workflows[0].stages[0].budget,
            workflow.stages[0].budget
        );
        assert_eq!(saved.pricing, settings.pricing);
        // Unset budgets and pricing are not written.
        let plain = toml::to_string_pretty(&Settings::default()).unwrap();
        assert!(
            !plain.contains("budget") && !plain.contains("pricing"),
            "{plain}"
        );
        let invalid = |edit: fn(&mut Settings)| {
            let mut settings = parse(&text).unwrap();
            edit(&mut settings);
            settings.validate().unwrap_err().to_string()
        };
        assert!(invalid(|s| s.workflows[0].budget.max_usd = Some(0.0)).contains("max_usd"));
        assert!(invalid(|s| s.workflows[0].budget.max_usd = Some(f64::NAN)).contains("max_usd"));
        assert!(
            invalid(|s| s.workflows[0].stages[0].budget.max_tokens = Some(0))
                .contains("max_tokens")
        );
        assert!(
            invalid(|s| s.pricing.get_mut("my-model").unwrap().output = -1.0).contains("Pricing")
        );
    }

    #[test]
    fn versions_migrate_forward_and_newer_versions_fail() {
        let v1 = parse(&CHECKS.replace("version = 2", "version = 1")).unwrap();
        assert_eq!(v1.version, VERSION);
        assert!(v1.warnings.iter().any(|w| w.contains("v1→v2")));
        assert_eq!(v1.workflows[0].stages.last().unwrap().role, Role::Summary);
        // A file without a version is v1.
        assert_eq!(
            parse(&CHECKS.replace("version = 2\n", "")).unwrap().version,
            VERSION
        );
        // v2 files are not migrated: a deleted summary stays deleted.
        let v2 = parse(CHECKS).unwrap();
        assert_eq!(v2.workflows[0].stages.len(), 1);
        assert!(v2.warnings.is_empty());
        let newer = parse(&CHECKS.replace("version = 2", "version = 99")).unwrap_err();
        assert!(newer.to_string().contains("newer"), "{newer}");
        assert!(parse(&CHECKS.replace("version = 2", "version = 0")).is_err());
    }

    #[test]
    fn summary_migration_uses_the_first_enabled_harness_without_codex() {
        let mut workflow = Workflow {
            max_repair_passes: 0,
            ..Workflow::new(
                "w",
                vec![Stage {
                    id: "a".into(),
                    harness: "claude".into(),
                    model: "opus".into(),
                    ..Stage::default()
                }],
            )
        };
        workflow.ensure_summary();
        let summary = workflow.stages.last().unwrap();
        assert_eq!(summary.role, Role::Summary);
        if crate::adapters::executable("codex").is_none() {
            assert_eq!(summary.harness, "claude");
            assert!(summary.model.is_empty());
        } else {
            assert_eq!(summary.harness, "codex");
        }
    }

    #[test]
    fn loading_adds_an_enabled_summary_only_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".model-router.toml");
        fs::write(&path, CHECKS.replace("version = 2", "version = 1")).unwrap();
        let (settings, _) = load(dir.path(), Some(&path)).unwrap();
        let stages = &settings.workflows[0].stages;
        let last = stages.last().unwrap();
        assert_eq!(stages.len(), 2);
        assert!(last.role == Role::Summary && last.enabled && last.id == "summary");

        let mut opted_out = settings.clone();
        opted_out.workflows[0].stages[1].enabled = false;
        opted_out.save(&path).unwrap();
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .starts_with("version = 2")
        );
        let (settings, _) = load(dir.path(), Some(&path)).unwrap();
        assert_eq!(settings.workflows[0].stages.len(), 2);
        assert!(!settings.workflows[0].stages[1].enabled);
    }

    #[test]
    fn graph_and_fan_out_fields_are_optional_and_saved_only_when_set() {
        let plain = toml::to_string_pretty(&Settings::default()).unwrap();
        for field in ["after", "fan_out", "max_parallel"] {
            assert!(!plain.contains(field), "{field}: {plain}");
        }
        let text = CHECKS.replace(
            "[[workflows.stages]]\nid = \"verify\"",
            "max_parallel = 3\n[[workflows.stages]]\nid = \"build\"\nrole = \"implementation\"\nfan_out = { candidates = [{ harness = \"codex\" }, { harness = \"claude\", model = \"sonnet\", id = \"second\" }] }\n[[workflows.stages]]\nid = \"verify\"\nafter = [\"build\"]",
        );
        let settings = parse(&text).unwrap();
        settings.validate().unwrap();
        assert!(settings.warnings.is_empty(), "{:?}", settings.warnings);
        let workflow = &settings.workflows[0];
        assert_eq!(workflow.max_parallel, 3);
        let fan = workflow.stages[0].fan_out.as_ref().unwrap();
        assert!(fan.is_auto());
        assert_eq!(fan.ids(), ["c1", "second"]);
        assert_eq!(workflow.stages[1].after, ["build"]);
        let saved = parse(&toml::to_string_pretty(&settings).unwrap()).unwrap();
        assert_eq!(
            saved.workflows[0].stages[0].fan_out,
            workflow.stages[0].fan_out
        );
        let mut unknown = settings.clone();
        unknown.workflows[0].stages[0]
            .fan_out
            .as_mut()
            .unwrap()
            .candidates[0]
            .harness = "nope".into();
        assert!(
            unknown
                .validate()
                .unwrap_err()
                .to_string()
                .contains("Unknown harness nope")
        );
        let mut cycle = settings;
        cycle.workflows[0].stages[0].after = vec!["verify".into()];
        assert!(format!("{:#}", cycle.validate().unwrap_err()).contains("cycle"));
    }

    #[test]
    fn unknown_effort_levels_warn_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut settings = Settings::default();
        settings.workflows[0].stages[1].effort = "turbo".into();
        settings.workflows[0].stages[0].effort = "high".into();
        settings.save(&path).unwrap();
        let (loaded, _) = load(dir.path(), Some(&path)).unwrap();
        let warnings = loaded.warnings.join("\n");
        assert!(
            warnings.contains("implement effort \"turbo\" is not a documented claude level"),
            "{warnings}"
        );
        assert!(!warnings.contains("research"), "{warnings}");
    }
}
