use crate::providers::Provider;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::OnceLock,
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
    /// Unattended inactivity timeout in seconds: stop when the harness produces
    /// no stdout or stderr output for this long. None disables stall detection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stall_timeout_seconds: Option<u64>,
    /// How this stage is billed: API (metered) or subscription-covered.
    /// Unset infers API when routed through a router provider, else subscription
    /// for native-account harnesses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub billing: Option<Billing>,
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
    /// The stage's agent may delegate tasks; see `delegation`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delegation: Option<crate::delegation::Delegation>,
    /// Extra repository-relative paths or globs whose contents are injected
    /// into this stage's prompt, after the workflow's `context_files`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context_files: Vec<String>,
    /// Transient retry settings overriding the workflow's `retry` when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<Retry>,
    /// Lifecycle hooks scoped to this stage; require project trust.
    #[serde(default, skip_serializing_if = "Hooks::is_empty")]
    pub hooks: Hooks,
    /// Repo-relative globs: in a Git repo, skip this stage if no files changed
    /// so far in the run match any pattern. Outside Git, runs normally.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub when_changed: Vec<String>,
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
    /// Transient retry settings for this stage: its own override, else the workflow's.
    pub fn effective_retry(&self, workflow: &Workflow) -> Retry {
        self.retry.unwrap_or(workflow.retry)
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
            stall_timeout_seconds: None,
            billing: None,
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
            delegation: None,
            context_files: vec![],
            retry: None,
            hooks: Hooks::default(),
            when_changed: vec![],
        }
    }
}

/// Automatic transient retry policy with capped exponential backoff.
/// Only unattended stage failures matching transient provider errors retry;
/// the same harness and model are always reused (no silent model substitution).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retry {
    /// Maximum automatic retry attempts after the initial failure (0..=5, default 0).
    pub max_attempts: u32,
    /// Initial backoff delay in seconds before the first retry (default 5).
    pub base_delay_seconds: u64,
    /// Maximum capped backoff delay in seconds (default 60).
    pub max_delay_seconds: u64,
}
impl Default for Retry {
    fn default() -> Self {
        Self {
            max_attempts: 0,
            base_delay_seconds: 5,
            max_delay_seconds: 60,
        }
    }
}
impl Retry {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
    /// Backoff delay for 1-based `retry_number` (1, 2, …):
    /// `min(base_delay_seconds * 2^(retry_number - 1), max_delay_seconds)`.
    pub fn delay_for(&self, retry_number: u32) -> std::time::Duration {
        let shift = retry_number.saturating_sub(1).min(16);
        let factor = 1u64 << shift;
        let secs = self
            .base_delay_seconds
            .saturating_mul(factor)
            .min(self.max_delay_seconds);
        std::time::Duration::from_secs(secs)
    }
    fn validate(&self, scope: &str) -> Result<()> {
        ensure!(
            self.max_attempts <= 5,
            "{scope} retry max_attempts must be 0–5"
        );
        ensure!(
            (1..=600).contains(&self.base_delay_seconds),
            "{scope} retry base_delay_seconds must be 1–600"
        );
        ensure!(
            (self.base_delay_seconds..=3600).contains(&self.max_delay_seconds),
            "{scope} retry max_delay_seconds must be at least base_delay_seconds and at most 3600"
        );
        Ok(())
    }
}

/// A lifecycle hook command executed as an argument vector; no shell evaluates it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Hook {
    pub command: Vec<String>,
    pub timeout_seconds: u64,
}
impl Default for Hook {
    fn default() -> Self {
        Self {
            command: vec![],
            timeout_seconds: 120,
        }
    }
}

/// Lifecycle hooks on a workflow or stage. All hooks require project trust.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Hooks {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before_stage: Vec<Hook>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after_stage: Vec<Hook>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before_ship: Vec<Hook>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_failure: Vec<Hook>,
}
impl Hooks {
    pub fn is_empty(&self) -> bool {
        self.before_stage.is_empty()
            && self.after_stage.is_empty()
            && self.before_ship.is_empty()
            && self.on_failure.is_empty()
    }
    pub fn for_event(&self, event: crate::hooks::HookEvent) -> &[Hook] {
        match event {
            crate::hooks::HookEvent::BeforeStage => &self.before_stage,
            crate::hooks::HookEvent::AfterStage => &self.after_stage,
            crate::hooks::HookEvent::BeforeShip => &self.before_ship,
            crate::hooks::HookEvent::OnFailure => &self.on_failure,
        }
    }
    pub fn all_entries(&self) -> impl Iterator<Item = (crate::hooks::HookEvent, &Hook)> {
        use crate::hooks::HookEvent::*;
        self.before_stage
            .iter()
            .map(|h| (BeforeStage, h))
            .chain(self.after_stage.iter().map(|h| (AfterStage, h)))
            .chain(self.before_ship.iter().map(|h| (BeforeShip, h)))
            .chain(self.on_failure.iter().map(|h| (OnFailure, h)))
    }
    fn validate(&self, scope: &str) -> Result<()> {
        for (event, list) in [
            ("before_stage", &self.before_stage),
            ("after_stage", &self.after_stage),
            ("before_ship", &self.before_ship),
            ("on_failure", &self.on_failure),
        ] {
            ensure!(
                list.len() <= 16,
                "{scope} {event} hooks can have at most 16 commands"
            );
            for hook in list {
                ensure!(
                    hook.command
                        .first()
                        .is_some_and(|program| !program.trim().is_empty()),
                    "{scope} has a {event} hook without a command"
                );
                ensure!(
                    !hook.command.iter().any(|arg| arg.contains('\0')),
                    "{scope} {event} hook commands cannot contain NUL"
                );
                ensure!(
                    (1..=3600).contains(&hook.timeout_seconds),
                    "{scope} {event} hook timeout_seconds must be 1–3600"
                );
            }
        }
        Ok(())
    }
}

/// Whether a stage's spend is billed per token (API) or covered by a flat
/// harness subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Billing {
    Api,
    Subscription,
}
impl Billing {
    pub fn label(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Subscription => "subscription",
        }
    }
}

/// What happens when a budget cap is exceeded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BudgetAction {
    /// Block the run before launching the next stage (default).
    #[default]
    Stop,
    /// Emit a warning in the run output and continue.
    Warn,
}
impl BudgetAction {
    pub fn is_stop(&self) -> bool {
        *self == Self::Stop
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Warn => "warn",
        }
    }
}

/// Spend caps. A run stops (or warns when `on_exceed = "warn"`) before its
/// next stage once known estimated cost exceeds `max_usd` or counted tokens
/// exceed `max_tokens` (input, output, and cache writes). Unknown fields warn,
/// like the workflow's.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Budget {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "BudgetAction::is_stop")]
    pub on_exceed: BudgetAction,
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

/// Default total bytes of prior stage handoffs carried in a downstream prompt.
pub const DEFAULT_HANDOFF_BUDGET_BYTES: usize = 14_000;
/// Default maximum bytes of any single prior stage handoff in a downstream prompt.
pub const DEFAULT_HANDOFF_EACH_BYTES: usize = 5_000;
/// Minimum allowed value for `handoff_budget_bytes` and `handoff_each_bytes`.
pub const MIN_HANDOFF_BUDGET_BYTES: usize = 1_024;
/// Maximum allowed value for `handoff_budget_bytes` and `handoff_each_bytes`.
pub const MAX_HANDOFF_BUDGET_BYTES: usize = 200_000;
/// Maximum number of context files (and patterns per list) injected into a prompt.
pub const MAX_CONTEXT_FILES: usize = 32;
/// Maximum total bytes of context file contents injected into a prompt (32 KiB).
pub const MAX_CONTEXT_BYTES: usize = 32 * 1024;

/// Whether `pattern` is a valid repo-relative path or glob for `context_files`:
/// non-empty, no NUL or backslashes, not absolute or `~/`, no trailing `/`,
/// and no `.`, `..`, `.git`, or `.model-router` segments.
pub fn is_valid_context_pattern(pattern: &str) -> bool {
    let trimmed = pattern.trim();
    if trimmed.is_empty()
        || pattern.contains('\0')
        || pattern.contains('\\')
        || trimmed.starts_with('/')
        || trimmed.starts_with("~/")
        || trimmed.ends_with('/')
    {
        return false;
    }
    let segments: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    !segments.is_empty()
        && !segments
            .iter()
            .any(|&seg| matches!(seg, "." | ".." | ".git" | ".model-router"))
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
    /// Repository-relative glob patterns that no stage may modify and ship
    /// refuses to commit (e.g. `.github/workflows/*`, `**/Cargo.lock`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protected_paths: Vec<String>,
    /// When true, an unattended stage on a resumable harness that exits 0 with
    /// missing or malformed handoff JSON gets one lightweight session-resume
    /// follow-up asking only for valid handoff JSON before failing.
    #[serde(default, skip_serializing_if = "is_false")]
    pub handoff_repair: bool,
    /// Total bytes of prior stage handoffs carried in a downstream prompt.
    /// Unset uses `DEFAULT_HANDOFF_BUDGET_BYTES` (14000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_budget_bytes: Option<usize>,
    /// Maximum bytes of any single prior stage handoff in a downstream prompt.
    /// Unset uses `DEFAULT_HANDOFF_EACH_BYTES` (5000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_each_bytes: Option<usize>,
    /// Repository-relative paths or globs whose contents are injected into
    /// every stage's prompt (bounded, workspace-only, UTF-8 text).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_files: Vec<String>,
    /// Default transient retry policy for unattended stages in this workflow.
    #[serde(default, skip_serializing_if = "Retry::is_empty")]
    pub retry: Retry,
    /// Workflow-level lifecycle hooks; require project trust.
    #[serde(default, skip_serializing_if = "Hooks::is_empty")]
    pub hooks: Hooks,
    /// Declarative diff policies (`block` and `advise`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policies: Vec<crate::policy::DiffPolicy>,
    /// Minimum distinct enabled review stages that must pass before the
    /// workflow considers review satisfied (default None / 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_quorum: Option<usize>,
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
            protected_paths: vec![],
            handoff_repair: false,
            handoff_budget_bytes: None,
            handoff_each_bytes: None,
            context_files: vec![],
            retry: Retry::default(),
            hooks: Hooks::default(),
            policies: vec![],
            review_quorum: None,
        }
    }
    /// Effective total byte budget for prior stage handoffs in downstream prompts.
    pub fn effective_handoff_budget_bytes(&self) -> usize {
        self.handoff_budget_bytes
            .unwrap_or(DEFAULT_HANDOFF_BUDGET_BYTES)
            .clamp(MIN_HANDOFF_BUDGET_BYTES, MAX_HANDOFF_BUDGET_BYTES)
    }
    /// Effective per-stage byte budget for prior handoffs in downstream prompts.
    pub fn effective_handoff_each_bytes(&self) -> usize {
        let total = self.effective_handoff_budget_bytes();
        self.handoff_each_bytes
            .unwrap_or(DEFAULT_HANDOFF_EACH_BYTES)
            .clamp(MIN_HANDOFF_BUDGET_BYTES, MAX_HANDOFF_BUDGET_BYTES)
            .min(total)
    }
    /// Workflow hooks followed by stage hooks (when `stage` is Some) for `event`.
    pub fn hooks_for(&self, stage: Option<&Stage>, event: crate::hooks::HookEvent) -> Vec<Hook> {
        let mut combined = self.hooks.for_event(event).to_vec();
        if let Some(stage) = stage {
            combined.extend(stage.hooks.for_event(event).iter().cloned());
        }
        combined
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
                    if let Some((key, value)) =
                        crate::harness::get(&stage.harness).and_then(|spec| spec.read_only)
                    {
                        stage
                            .harness_settings
                            .insert(key.to_owned(), value.to_owned());
                    } else if self.sandbox_default.resolve(Role::Summary) != Sandbox::ReadOnly {
                        stage.sandbox = Some(Sandbox::ReadOnly);
                    }
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
        Self { name: "Feature development".into(), max_repair_passes: 2, sandbox_default: Sandbox::Off, budget: Budget::default(), max_parallel: 2, protected_paths: vec![], handoff_repair: false, handoff_budget_bytes: None, handoff_each_bytes: None, context_files: vec![], retry: Retry::default(), hooks: Hooks::default(), policies: vec![], review_quorum: None, stages: vec![
        Stage { id: "research".into(), name: "Investigate".into(), role: Role::Research,
            instructions: "Investigate the task and relevant project files. Explain the root cause or requirements, cite evidence, and propose a concrete implementation plan. Do not change project code.".into(), ..Stage::default() },
        Stage { id: "implement".into(), name: "Implement".into(), role: Role::Implementation, harness: "claude".into(), model: "sonnet".into(), mode: Mode::Interactive,
            instructions: "Implement the task using the prior findings. Follow project conventions, run appropriate checks, and report what changed and any remaining issues.".into(), ..Stage::default() },
        Stage { id: "review".into(), name: "Review (optional)".into(), role: Role::Review, enabled: false,
            instructions: "Review the implementation and test evidence. Do not modify code. Distinguish blocking correctness issues and failed required checks from advisory suggestions. Give an explicit pass or needs_changes verdict.".into(), ..Stage::default() },
        crate::presets::stage("summary").build(),
    ] }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Unattended arguments that send one more prompt to `{session}`, in
    /// place of `args`; enables `workflow.handoff_repair`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follow_up_args: Option<Vec<String>>,
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
    /// Arguments that hand a delegating stage's harness the router's MCP
    /// server; `{mcp_config}` is a Claude-format MCP config file. Without
    /// them the adapter cannot delegate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_config_args: Option<Vec<String>>,
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
        if let Some(args) = &self.follow_up_args {
            fields.push(("follow-up", args.clone()));
        }
        if let Some(args) = &self.mcp_config_args {
            fields.push(("mcp config", args.clone()));
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
        ensure!(
            self.follow_up_args.is_none() || self.session.is_some(),
            "Custom adapter {name} follow_up_args needs a session (assign or reported)"
        );
        Ok(())
    }
}

/// The config format this binary writes. Older files migrate on load.
pub const VERSION: u32 = 2;

/// Entries layered from the user's global `config.toml` underneath a project
/// `.model-router.toml`. Kept only for keys not defined by the project; those
/// entries stay trusted and are omitted when saving the project config unless
/// edited locally. Never accepted from TOML.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GlobalLayer {
    pub adapters: BTreeMap<String, CustomAdapter>,
    pub providers: BTreeMap<String, Provider>,
    pub delegates: BTreeMap<String, crate::delegation::Delegate>,
    pub pricing: BTreeMap<String, crate::cost::Price>,
}
impl GlobalLayer {
    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
            && self.providers.is_empty()
            && self.delegates.is_empty()
            && self.pricing.is_empty()
    }
    pub fn has_adapter(&self, name: &str, adapter: &CustomAdapter) -> bool {
        self.adapters.get(name) == Some(adapter)
    }
    pub fn has_provider(&self, name: &str, provider: &Provider) -> bool {
        self.providers.get(name) == Some(provider)
    }
    pub fn has_delegate(&self, name: &str, delegate: &crate::delegation::Delegate) -> bool {
        self.delegates.get(name) == Some(delegate)
    }
}

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
    /// Named delegate targets for stages' `delegation`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub delegates: BTreeMap<String, crate::delegation::Delegate>,
    /// Optional harness version pins checked by `doctor` (e.g. `codex = "0.167.0"` or `">=0.160.0"`).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub harness_versions: BTreeMap<String, String>,
    /// Load notes: unknown fields, migrations, effort levels, trust. Not saved.
    #[serde(skip)]
    pub warnings: Vec<String>,
    /// Sections an untrusted project config had ignored; restored on save.
    #[serde(skip)]
    pub ignored: Option<Box<crate::trust::Ignored>>,
    /// Global entries layered underneath a project config. Never accepted from TOML.
    #[serde(skip)]
    pub global_layer: Option<Box<GlobalLayer>>,
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
            delegates: BTreeMap::new(),
            harness_versions: BTreeMap::new(),
            warnings: vec![],
            ignored: None,
            global_layer: None,
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

struct KnownKeys {
    settings: HashSet<String>,
    workflow: HashSet<String>,
    budget: HashSet<String>,
    adapters: HashSet<String>,
    providers: HashSet<String>,
    pricing: HashSet<String>,
}

fn known_keys() -> &'static KnownKeys {
    fn keys<T: Serialize>(sample: &T) -> HashSet<String> {
        toml::Value::try_from(sample)
            .ok()
            .and_then(|v| v.as_table().map(|t| t.keys().cloned().collect()))
            .unwrap_or_default()
    }
    static KNOWN: OnceLock<KnownKeys> = OnceLock::new();
    KNOWN.get_or_init(|| {
        let mut sample = Settings::default();
        sample.providers.insert("p".into(), Provider::default());
        let price = crate::cost::Price {
            input: 1.0,
            output: 1.0,
            cache_read: Some(1.0),
            cache_write: Some(1.0),
        };
        sample.pricing.insert("m".into(), price);
        sample.delegates.insert(
            "d".into(),
            crate::delegation::Delegate {
                harness: "codex".into(),
                ..crate::delegation::Delegate::default()
            },
        );
        sample
            .harness_versions
            .insert("codex".into(), "0.167.0".into());
        let budget = keys(&Budget {
            max_usd: Some(1.0),
            max_tokens: Some(1),
            on_exceed: BudgetAction::Warn,
        });
        let adapter = CustomAdapter {
            interactive_args: Some(vec![]),
            models_args: Some(vec![]),
            output: Some(CustomOutput::Text),
            session: Some(CustomSession::default()),
            resume_args: Some(vec![]),
            follow_up_args: Some(vec![]),
            mcp_config_args: Some(vec![]),
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
        let workflow = keys(&Workflow {
            sandbox_default: Sandbox::ByRole,
            // Saved only when not the default or non-empty.
            max_parallel: 3,
            budget: Budget {
                max_usd: Some(1.0),
                ..Budget::default()
            },
            protected_paths: vec!["x".into()],
            handoff_repair: true,
            handoff_budget_bytes: Some(DEFAULT_HANDOFF_BUDGET_BYTES),
            handoff_each_bytes: Some(DEFAULT_HANDOFF_EACH_BYTES),
            context_files: vec!["x".into()],
            retry: Retry {
                max_attempts: 1,
                ..Retry::default()
            },
            hooks: Hooks {
                before_stage: vec![Hook {
                    command: vec!["true".into()],
                    ..Hook::default()
                }],
                ..Hooks::default()
            },
            policies: vec![crate::policy::DiffPolicy {
                name: "p".into(),
                action: crate::policy::PolicyAction::Block,
                pattern: "x".into(),
                paths: vec!["*".into()],
                message: "m".into(),
            }],
            review_quorum: Some(2),
            ..Workflow::default()
        });
        KnownKeys {
            settings: keys(&sample),
            workflow,
            budget,
            adapters: keys(&adapter),
            providers: keys(&provider),
            pricing: keys(&price),
        }
    })
}

/// Fields in `value` that `Settings` would ignore, as dotted paths. Stage and
/// check fields are strict and fail parsing instead.
pub fn unknown_fields(value: &toml::Value) -> Vec<String> {
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
    let known = known_keys();
    let mut out = vec![];
    extra(value, &known.settings, "", &mut out);
    for (index, item) in value
        .get("workflows")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let at = format!("workflows[{index}]");
        extra(item, &known.workflow, &at, &mut out);
        if let Some(value) = item.get("budget") {
            extra(value, &known.budget, &format!("{at}.budget"), &mut out);
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
                    &known.budget,
                    &format!("{at}.stages[{number}].budget"),
                    &mut out,
                );
            }
        }
    }
    for (section, section_keys) in [
        ("adapters", &known.adapters),
        ("providers", &known.providers),
        ("pricing", &known.pricing),
    ] {
        for (name, item) in value
            .get(section)
            .and_then(toml::Value::as_table)
            .into_iter()
            .flatten()
        {
            extra(item, section_keys, &format!("{section}.{name}"), &mut out);
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
            workflow
                .retry
                .validate(&format!("Workflow {}", workflow.name))?;
            workflow
                .hooks
                .validate(&format!("Workflow {}", workflow.name))?;
            ensure!(
                workflow.protected_paths.len() <= 64
                    && workflow
                        .protected_paths
                        .iter()
                        .all(|p| !p.trim().is_empty() && !p.contains('\0')),
                "Workflow {} protected_paths must contain at most 64 non-empty patterns without NUL",
                workflow.name
            );
            ensure!(
                workflow.policies.len() <= 64,
                "Workflow {} can have at most 64 policies",
                workflow.name
            );
            for policy in &workflow.policies {
                ensure!(
                    !policy.name.trim().is_empty() && !policy.name.contains('\0'),
                    "Workflow {} policy needs a non-empty name without NUL",
                    workflow.name
                );
                ensure!(
                    !policy.message.trim().is_empty() && !policy.message.contains('\0'),
                    "Workflow {} policy {} needs a non-empty message without NUL",
                    workflow.name,
                    policy.name
                );
                ensure!(
                    !policy.pattern.contains('\0') && policy.pattern.len() <= 512,
                    "Workflow {} policy {} pattern must be at most 512 bytes without NUL",
                    workflow.name,
                    policy.name
                );
                ensure!(
                    policy.paths.len() <= 64
                        && policy
                            .paths
                            .iter()
                            .all(|p| !p.trim().is_empty() && !p.contains('\0')),
                    "Workflow {} policy {} paths must contain at most 64 non-empty patterns without NUL",
                    workflow.name,
                    policy.name
                );
                match policy.action {
                    crate::policy::PolicyAction::Block => {
                        ensure!(
                            !policy.pattern.is_empty(),
                            "Workflow {} block policy {} requires a non-empty pattern",
                            workflow.name,
                            policy.name
                        );
                    }
                    crate::policy::PolicyAction::Advise => {
                        ensure!(
                            !policy.pattern.is_empty() || !policy.paths.is_empty(),
                            "Workflow {} advise policy {} requires a pattern or at least one path glob",
                            workflow.name,
                            policy.name
                        );
                    }
                }
            }
            ensure!(
                workflow.review_quorum.is_none_or(|n| (1..=64).contains(&n)),
                "Workflow {} review_quorum must be 1–64",
                workflow.name
            );
            ensure!(
                (1..=16).contains(&workflow.max_parallel),
                "Workflow {} max_parallel must be 1–16",
                workflow.name
            );
            if let Some(total) = workflow.handoff_budget_bytes {
                ensure!(
                    (MIN_HANDOFF_BUDGET_BYTES..=MAX_HANDOFF_BUDGET_BYTES).contains(&total),
                    "Workflow {} handoff_budget_bytes must be {}–{}",
                    workflow.name,
                    MIN_HANDOFF_BUDGET_BYTES,
                    MAX_HANDOFF_BUDGET_BYTES
                );
            }
            if let Some(each) = workflow.handoff_each_bytes {
                ensure!(
                    (MIN_HANDOFF_BUDGET_BYTES..=MAX_HANDOFF_BUDGET_BYTES).contains(&each),
                    "Workflow {} handoff_each_bytes must be {}–{}",
                    workflow.name,
                    MIN_HANDOFF_BUDGET_BYTES,
                    MAX_HANDOFF_BUDGET_BYTES
                );
                let total = workflow.effective_handoff_budget_bytes();
                ensure!(
                    each <= total,
                    "Workflow {} handoff_each_bytes ({each}) cannot exceed handoff_budget_bytes ({total})",
                    workflow.name
                );
            }
            ensure!(
                workflow.context_files.len() <= MAX_CONTEXT_FILES
                    && workflow
                        .context_files
                        .iter()
                        .all(|p| is_valid_context_pattern(p)),
                "Workflow {} context_files must contain at most {} repo-relative paths or globs outside .git and .model-router",
                workflow.name,
                MAX_CONTEXT_FILES
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
                    stage.stall_timeout_seconds != Some(0),
                    "Stage {} stall_timeout_seconds must be positive",
                    stage.name
                );
                if stage.stall_timeout_seconds.is_some() {
                    // A fan-out stage runs its candidates' harnesses.
                    let harnesses: Vec<&str> = match &stage.fan_out {
                        Some(fan_out) => fan_out
                            .candidates
                            .iter()
                            .map(|c| c.harness.as_str())
                            .collect(),
                        None => vec![stage.harness.as_str()],
                    };
                    for harness in harnesses {
                        ensure!(
                            crate::adapters::streams(harness, &self.adapters),
                            "Stage {} sets stall_timeout_seconds, but {harness} prints its output only when it exits, so a working stage would be stopped as stalled. Remove stall_timeout_seconds and rely on timeout_seconds",
                            stage.name
                        );
                    }
                }
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
                if let Some(retry) = &stage.retry {
                    retry.validate(&format!("Stage {}", stage.name))?;
                    // Rerunning a delegating parent would rerun or abandon the
                    // tasks it started and the changes they applied.
                    ensure!(
                        retry.max_attempts == 0 || stage.delegation.is_none(),
                        "Stage {} delegates, and a delegating stage does not retry automatically. Remove its retry and retry the stage explicitly",
                        stage.name
                    );
                }
                stage.hooks.validate(&format!("Stage {}", stage.name))?;
                ensure!(
                    stage.when_changed.len() <= 32
                        && stage
                            .when_changed
                            .iter()
                            .all(|p| !p.trim().is_empty() && !p.contains('\0')),
                    "Stage {} when_changed must contain at most 32 non-empty patterns without NUL",
                    stage.name
                );
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
                ensure!(
                    stage.context_files.len() <= MAX_CONTEXT_FILES
                        && stage
                            .context_files
                            .iter()
                            .all(|p| is_valid_context_pattern(p)),
                    "Stage {} context_files must contain at most {} repo-relative paths or globs outside .git and .model-router",
                    stage.name,
                    MAX_CONTEXT_FILES
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
        crate::delegation::config::validate(
            &self.workflows,
            &self.delegates,
            &self.adapters,
            &self.providers,
        )?;
        for (model, price) in &self.pricing {
            ensure!(
                price.is_valid(),
                "Pricing for {model} must use finite, non-negative USD per million tokens"
            );
        }
        for (harness, pin) in &self.harness_versions {
            ensure!(
                is_id(harness),
                "harness_versions key {harness:?} must be a harness name (letters, digits, '-' and '_')"
            );
            crate::doctor::validate_version_pin(pin)
                .with_context(|| format!("Invalid version pin for harness {harness}"))?;
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
    /// was untrusted are written back unchanged. Unmodified entries layered
    /// from the user's global `config.toml` are omitted so saving a project
    /// config never copies global definitions into the repository. Saving to
    /// the global `config.toml` itself keeps them, since they came from there.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.save_as(path, !is_global_config(path))
    }

    /// Writes `path`, omitting unmodified global entries when `project` is set.
    fn save_as(&self, path: &Path, project: bool) -> Result<()> {
        let restored = crate::trust::restore_cow(self);
        let needs_strip = project
            && restored
                .global_layer
                .as_ref()
                .is_some_and(|layer| !layer.is_empty());
        if restored.version == VERSION && !needs_strip {
            restored.validate()?;
            return crate::storage::atomic_write(
                path,
                toml::to_string_pretty(&*restored)?.as_bytes(),
            );
        }
        let mut saved = restored.into_owned();
        saved.version = VERSION;
        saved.validate()?;
        if let Some(layer) = saved.global_layer.take().filter(|_| needs_strip) {
            saved
                .adapters
                .retain(|k, v| layer.adapters.get(k) != Some(v));
            saved
                .providers
                .retain(|k, v| layer.providers.get(k) != Some(v));
            saved
                .delegates
                .retain(|k, v| layer.delegates.get(k) != Some(v));
            saved.pricing.retain(|k, v| layer.pricing.get(k) != Some(v));
        }
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

/// Whether `path` is the user's global `config.toml`.
fn is_global_config(path: &Path) -> bool {
    let global = config_dir().join("config.toml");
    path == global
        || matches!(
            (fs::canonicalize(path), fs::canonicalize(&global)),
            (Ok(a), Ok(b)) if a == b
        )
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

const MAX_CONFIG_BYTES: u64 = 2 * 1024 * 1024;

fn read_config_file(path: &Path, required: bool) -> Result<Option<String>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("Read {}", path.display())),
    };
    let meta = file
        .metadata()
        .with_context(|| format!("Read {}", path.display()))?;
    if !meta.is_file() {
        if !required && meta.is_dir() {
            return Ok(None);
        }
        bail!("Config {} is not a regular file", path.display());
    }
    let mut buf = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut buf)
        .with_context(|| format!("Read {}", path.display()))?;
    ensure!(
        buf.len() as u64 <= MAX_CONFIG_BYTES,
        "Config {} exceeds {} byte limit",
        path.display(),
        MAX_CONFIG_BYTES
    );
    let text = String::from_utf8(buf).with_context(|| format!("Read {}", path.display()))?;
    Ok(Some(text))
}

fn layer_global(settings: &mut Settings, global: Settings) {
    use std::collections::btree_map::Entry;
    let mut layer = GlobalLayer::default();
    for (name, adapter) in global.adapters {
        if let Entry::Vacant(slot) = settings.adapters.entry(name) {
            layer.adapters.insert(slot.key().clone(), adapter.clone());
            slot.insert(adapter);
        }
    }
    for (name, provider) in global.providers {
        if let Entry::Vacant(slot) = settings.providers.entry(name) {
            layer.providers.insert(slot.key().clone(), provider.clone());
            slot.insert(provider);
        }
    }
    for (name, delegate) in global.delegates {
        if let Entry::Vacant(slot) = settings.delegates.entry(name) {
            layer.delegates.insert(slot.key().clone(), delegate.clone());
            slot.insert(delegate);
        }
    }
    for (model, price) in global.pricing {
        if let Entry::Vacant(slot) = settings.pricing.entry(model) {
            layer.pricing.insert(slot.key().clone(), price);
            slot.insert(price);
        }
    }
    if !layer.is_empty() {
        settings.global_layer = Some(Box::new(layer));
    }
}

/// Loads `explicit`, else the project's config (with global `[providers]`,
/// `[adapters]`, `[delegates]`, and `[pricing]` layered underneath), else the
/// global config, and checks the project's trust.
pub fn load_config(project: &Path, explicit: Option<&Path>) -> Result<Loaded> {
    let project_path = project.join(".model-router.toml");
    let global = config_dir().join("config.toml");
    let (source, origin, text) = if let Some(path) = explicit {
        let text =
            read_config_file(path, true)?.with_context(|| format!("Read {}", path.display()))?;
        (Some(path.to_path_buf()), Origin::Explicit, Some(text))
    } else if let Some(text) = read_config_file(&project_path, false)? {
        (Some(project_path.clone()), Origin::Project, Some(text))
    } else if let Some(text) = read_config_file(&global, false)? {
        (Some(global.clone()), Origin::Global, Some(text))
    } else {
        (None, Origin::Default, None)
    };
    let mut settings = match (&source, text) {
        (Some(path), Some(text)) => {
            parse(&text).with_context(|| format!("Parse {}", path.display()))?
        }
        _ => Settings::default(),
    };
    if origin == Origin::Project {
        if let Some(global_text) = read_config_file(&global, false)? {
            let global_settings =
                parse(&global_text).with_context(|| format!("Parse {}", global.display()))?;
            layer_global(&mut settings, global_settings);
        }
    }
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
        // Future budget fields warn instead of failing, and valid [workflows.budget] does not.
        assert_eq!(settings.warnings.len(), 1, "{:?}", settings.warnings);
        assert!(
            settings.warnings[0].contains("workflows[0].budget.hard"),
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

    #[test]
    fn oversized_or_non_regular_config_fails_without_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join(".model-router.toml");
        fs::write(&big, "x".repeat(MAX_CONFIG_BYTES as usize + 1)).unwrap();
        let err = load_config(dir.path(), None).unwrap_err();
        assert!(format!("{err:#}").contains("limit"), "{err:#}");
        fs::remove_file(&big).unwrap();

        #[cfg(unix)]
        {
            let fifo = std::ffi::CString::new(big.to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
            let err = load_config(dir.path(), None).unwrap_err();
            assert!(format!("{err:#}").contains("not a regular file"), "{err:#}");
        }
    }

    #[test]
    fn starter_workflow_and_presets_use_native_default_model_instead_of_gpt6_astra() {
        let starter = Workflow::default();
        for stage in &starter.stages {
            assert_ne!(stage.model, "gpt-6-astra", "stage {}", stage.id);
        }
        assert!(starter.stages[0].model.is_empty());
        assert!(starter.stages[2].model.is_empty());
        assert!(starter.stages[3].model.is_empty());
        for preset in crate::presets::STAGES {
            let built = preset.build();
            assert_ne!(built.model, "gpt-6-astra", "stage preset {}", preset.key);
        }
        for preset in crate::presets::WORKFLOWS {
            let built = preset.build();
            for stage in &built.stages {
                assert_ne!(
                    stage.model, "gpt-6-astra",
                    "workflow preset {} stage {}",
                    preset.key, stage.id
                );
                if let Some(fan) = &stage.fan_out {
                    for cand in &fan.candidates {
                        assert_ne!(
                            cand.model, "gpt-6-astra",
                            "workflow preset {} candidate {}",
                            preset.key, cand.harness
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn handoff_budget_and_context_files_parse_validate_roundtrip_and_stay_out_of_default() {
        let plain = toml::to_string_pretty(&Settings::default()).unwrap();
        for field in [
            "handoff_budget_bytes",
            "handoff_each_bytes",
            "context_files",
        ] {
            assert!(!plain.contains(field), "{field}: {plain}");
        }
        let text = CHECKS
            .replace(
                "name = \"Verify\"",
                "name = \"Verify\"\nhandoff_budget_bytes = 24000\nhandoff_each_bytes = 8000\ncontext_files = [\"AGENTS.md\", \"docs/*.md\"]",
            )
            .replace(
                "role = \"test\"",
                "role = \"test\"\ncontext_files = [\"src/**/*.rs\"]",
            );
        let settings = parse(&text).unwrap();
        settings.validate().unwrap();
        assert!(settings.warnings.is_empty(), "{:?}", settings.warnings);
        let wf = &settings.workflows[0];
        assert_eq!(wf.handoff_budget_bytes, Some(24_000));
        assert_eq!(wf.handoff_each_bytes, Some(8_000));
        assert_eq!(wf.effective_handoff_budget_bytes(), 24_000);
        assert_eq!(wf.effective_handoff_each_bytes(), 8_000);
        assert_eq!(wf.context_files, ["AGENTS.md", "docs/*.md"]);
        assert_eq!(wf.stages[0].context_files, ["src/**/*.rs"]);
        let roundtrip = parse(&toml::to_string_pretty(&settings).unwrap()).unwrap();
        assert_eq!(roundtrip.workflows[0].handoff_budget_bytes, Some(24_000));
        assert_eq!(roundtrip.workflows[0].handoff_each_bytes, Some(8_000));
        assert_eq!(
            roundtrip.workflows[0].context_files,
            ["AGENTS.md", "docs/*.md"]
        );
        assert_eq!(
            roundtrip.workflows[0].stages[0].context_files,
            ["src/**/*.rs"]
        );

        // Lowering only handoff_budget_bytes clamps effective per-stage budget cleanly.
        let mut small_total = parse(CHECKS).unwrap();
        small_total.workflows[0].handoff_budget_bytes = Some(2_048);
        small_total.validate().unwrap();
        assert_eq!(
            small_total.workflows[0].effective_handoff_each_bytes(),
            2_048
        );

        let invalid = |edit: fn(&mut Settings)| {
            let mut s = parse(CHECKS).unwrap();
            edit(&mut s);
            s.validate().unwrap_err().to_string()
        };
        assert!(
            invalid(|s| s.workflows[0].handoff_budget_bytes = Some(512))
                .contains("handoff_budget_bytes")
        );
        assert!(
            invalid(|s| s.workflows[0].handoff_budget_bytes = Some(250_000))
                .contains("handoff_budget_bytes")
        );
        assert!(
            invalid(|s| s.workflows[0].handoff_each_bytes = Some(512))
                .contains("handoff_each_bytes")
        );
        assert!(
            invalid(|s| s.workflows[0].handoff_each_bytes = Some(250_000))
                .contains("handoff_each_bytes")
        );
        assert!(
            invalid(|s| s.workflows[0].handoff_each_bytes = Some(16_000)).contains("cannot exceed")
        );
        assert!(
            invalid(|s| {
                s.workflows[0].handoff_budget_bytes = Some(4_000);
                s.workflows[0].handoff_each_bytes = Some(5_000);
            })
            .contains("cannot exceed")
        );

        for bad in [
            "",
            "   ",
            "/etc/passwd",
            "~/secret.txt",
            "../outside.md",
            "docs/../secret.md",
            "./AGENTS.md",
            ".git/config",
            ".model-router/runs/1/run.json",
            "nested/.git/HEAD",
            "docs/",
            "a\\b.md",
            "a\0b.md",
        ] {
            let mut s = parse(CHECKS).unwrap();
            s.workflows[0].context_files = vec![bad.into()];
            assert!(s.validate().is_err(), "workflow accepted {bad:?}");
            let mut s = parse(CHECKS).unwrap();
            s.workflows[0].stages[0].context_files = vec![bad.into()];
            assert!(s.validate().is_err(), "stage accepted {bad:?}");
        }
        assert!(
            invalid(|s| s.workflows[0].context_files = vec!["a.md".into(); MAX_CONTEXT_FILES + 1])
                .contains("context_files")
        );
        assert!(
            invalid(|s| {
                s.workflows[0].stages[0].context_files = vec!["a.md".into(); MAX_CONTEXT_FILES + 1];
            })
            .contains("context_files")
        );
    }

    #[test]
    fn global_layering_prefers_project_keys_and_save_omits_unmodified_global_entries() {
        let dir = tempfile::tempdir().unwrap();
        let project_path = dir.path().join(".model-router.toml");
        let global = parse(
            r#"
version = 2

[adapters.global_only]
command = "global-bin"

[adapters.shared]
command = "global-shared-bin"

[providers.global_gw]
base_url = "https://global.example/v1"
api_key_env = "GLOBAL_KEY"

[providers.shared_gw]
base_url = "https://global-shared.example/v1"

[delegates.global_del]
harness = "codex"
model = "global-m"

[delegates.shared_del]
harness = "codex"
model = "global-shared-m"

[pricing."global-model"]
input = 1.0
output = 2.0

[pricing."shared-model"]
input = 1.0
output = 2.0
"#,
        )
        .unwrap();

        let mut project = parse(
            r#"
version = 2

[adapters.shared]
command = "project-shared-bin"

[providers.shared_gw]
base_url = "https://project-shared.example/v1"

[delegates.shared_del]
harness = "claude"
model = "project-shared-m"

[pricing."shared-model"]
input = 5.0
output = 10.0

[[workflows]]
name = "Layered"
[[workflows.stages]]
id = "s1"
name = "Stage 1"
harness = "global_only"
provider = "global_gw"
model = "global-model"
"#,
        )
        .unwrap();

        layer_global(&mut project, global);
        project.validate().unwrap();

        // Project keys win on collision; non-colliding global keys are layered underneath.
        assert_eq!(project.adapters["global_only"].command, "global-bin");
        assert_eq!(project.adapters["shared"].command, "project-shared-bin");
        assert_eq!(
            project.providers["global_gw"].base_url,
            "https://global.example/v1"
        );
        assert_eq!(
            project.providers["shared_gw"].base_url,
            "https://project-shared.example/v1"
        );
        assert_eq!(project.delegates["global_del"].model, "global-m");
        assert_eq!(project.delegates["shared_del"].model, "project-shared-m");
        assert_eq!(project.pricing["global-model"].input, 1.0);
        assert_eq!(project.pricing["shared-model"].input, 5.0);

        // Only global-only keys live in global_layer; shadowed keys belong to project.
        let layer = project.global_layer.as_ref().unwrap();
        assert!(layer.adapters.contains_key("global_only"));
        assert!(!layer.adapters.contains_key("shared"));
        assert!(layer.providers.contains_key("global_gw"));
        assert!(!layer.providers.contains_key("shared_gw"));
        assert!(layer.delegates.contains_key("global_del"));
        assert!(!layer.delegates.contains_key("shared_del"));
        assert!(layer.pricing.contains_key("global-model"));
        assert!(!layer.pricing.contains_key("shared-model"));

        // Even after ignoring untrusted project sections, saving restores project
        // sections and omits unmodified global entries without failing validation.
        crate::trust::ignore(&mut project);
        assert_eq!(project.adapters["global_only"].command, "global-bin");
        assert_eq!(
            project.adapters["shared"].command,
            crate::trust::BLOCKED_COMMAND
        );
        project.save(&project_path).unwrap();

        let saved_raw = fs::read_to_string(&project_path).unwrap();
        let saved = parse(&saved_raw).unwrap();
        assert!(!saved.adapters.contains_key("global_only"), "{saved_raw}");
        assert!(!saved.providers.contains_key("global_gw"), "{saved_raw}");
        assert!(!saved.delegates.contains_key("global_del"), "{saved_raw}");
        assert!(!saved.pricing.contains_key("global-model"), "{saved_raw}");
        assert_eq!(saved.adapters["shared"].command, "project-shared-bin");
        assert_eq!(
            saved.providers["shared_gw"].base_url,
            "https://project-shared.example/v1"
        );
        assert_eq!(saved.delegates["shared_del"].model, "project-shared-m");
        assert_eq!(saved.pricing["shared-model"].input, 5.0);

        // Saving as global templates keeps the global entries in config.toml.
        let global_path = dir.path().join("config.toml");
        project.save_as(&global_path, false).unwrap();
        let saved_raw = fs::read_to_string(&global_path).unwrap();
        let saved = parse(&saved_raw).unwrap();
        assert_eq!(saved.adapters["global_only"].command, "global-bin");
        assert!(saved.providers.contains_key("global_gw"), "{saved_raw}");
        assert!(saved.delegates.contains_key("global_del"), "{saved_raw}");
        assert!(saved.pricing.contains_key("global-model"), "{saved_raw}");
        assert_eq!(saved.adapters["shared"].command, "project-shared-bin");
    }

    #[test]
    fn save_treats_only_the_global_config_toml_as_global() {
        assert!(is_global_config(&config_dir().join("config.toml")));
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_global_config(&dir.path().join(".model-router.toml")));
        assert!(!is_global_config(&dir.path().join("config.toml")));
    }

    #[test]
    fn retry_hooks_policies_when_changed_and_quorum_roundtrip_and_validate() {
        let toml = r#"
version = 2
[[workflows]]
name = "Resilient"
review_quorum = 2
retry = { max_attempts = 3, base_delay_seconds = 2, max_delay_seconds = 10 }
[workflows.hooks]
before_stage = [{ command = ["echo", "wf-before"], timeout_seconds = 30 }]
before_ship = [{ command = ["echo", "wf-ship"] }]
[[workflows.policies]]
name = "no-dbg"
action = "block"
pattern = "dbg!("
paths = ["src/**/*.rs"]
message = "Remove dbg! macro calls"
[[workflows.policies]]
name = "review-migrations"
action = "advise"
paths = ["migrations/**"]
message = "Check migration safety"
[[workflows.stages]]
id = "impl"
name = "Implement"
role = "implementation"
when_changed = ["src/**"]
retry = { max_attempts = 1, base_delay_seconds = 1, max_delay_seconds = 5 }
[workflows.stages.hooks]
after_stage = [{ command = ["echo", "stage-after"] }]
on_failure = [{ command = ["echo", "stage-fail"] }]
"#;
        let settings = parse(toml).unwrap();
        settings.validate().unwrap();
        assert!(settings.warnings.is_empty(), "{:?}", settings.warnings);
        let wf = &settings.workflows[0];
        assert_eq!(wf.review_quorum, Some(2));
        assert_eq!(wf.retry.max_attempts, 3);
        assert_eq!(wf.retry.delay_for(1).as_secs(), 2);
        assert_eq!(wf.retry.delay_for(2).as_secs(), 4);
        assert_eq!(wf.retry.delay_for(3).as_secs(), 8);
        assert_eq!(wf.retry.delay_for(4).as_secs(), 10);
        assert_eq!(wf.stages[0].effective_retry(wf).max_attempts, 1);
        assert_eq!(wf.stages[0].when_changed, vec!["src/**"]);
        assert_eq!(wf.policies.len(), 2);

        let saved = parse(&toml::to_string_pretty(&settings).unwrap()).unwrap();
        assert_eq!(saved.workflows[0].retry, wf.retry);
        assert_eq!(saved.workflows[0].hooks, wf.hooks);
        assert_eq!(saved.workflows[0].policies, wf.policies);
        assert_eq!(saved.workflows[0].review_quorum, wf.review_quorum);
        assert_eq!(
            saved.workflows[0].stages[0].when_changed,
            wf.stages[0].when_changed
        );

        let plain = toml::to_string_pretty(&Settings::default()).unwrap();
        for omitted in [
            "retry",
            "hooks",
            "policies",
            "review_quorum",
            "when_changed",
        ] {
            assert!(!plain.contains(omitted), "{omitted} in {plain}");
        }

        let mut bad = settings.clone();
        bad.workflows[0].retry.max_attempts = 6;
        assert!(bad.validate().unwrap_err().to_string().contains("0–5"));

        // Documented limits: base 1–600, base <= max <= 3600, hooks 1–3600 s,
        // at most 32 when_changed patterns.
        let error = |edit: &dyn Fn(&mut Settings)| {
            let mut bad = settings.clone();
            edit(&mut bad);
            bad.validate().map_err(|e| e.to_string()).err()
        };
        let retry = |base, max| {
            move |s: &mut Settings| {
                s.workflows[0].retry.base_delay_seconds = base;
                s.workflows[0].retry.max_delay_seconds = max;
            }
        };
        assert_eq!(error(&retry(1, 1)), None);
        assert_eq!(error(&retry(600, 3600)), None);
        for (base, max) in [(0, 1), (601, 3600), (10, 5), (5, 3601)] {
            let message = error(&retry(base, max)).unwrap_or_default();
            assert!(message.contains("delay_seconds"), "{base}/{max}: {message}");
        }
        let stage_retry = |s: &mut Settings| {
            s.workflows[0].stages[0]
                .retry
                .as_mut()
                .unwrap()
                .base_delay_seconds = 0
        };
        assert!(error(&stage_retry).unwrap().contains("base_delay_seconds"));
        let hook = |secs| {
            move |s: &mut Settings| s.workflows[0].hooks.before_stage[0].timeout_seconds = secs
        };
        assert_eq!(error(&hook(3600)), None);
        assert!(error(&hook(3601)).unwrap().contains("1–3600"));
        assert!(error(&hook(0)).unwrap().contains("1–3600"));
        let patterns = |n: usize| {
            move |s: &mut Settings| s.workflows[0].stages[0].when_changed = vec!["src/**".into(); n]
        };
        assert_eq!(error(&patterns(32)), None);
        assert!(error(&patterns(33)).unwrap().contains("at most 32"));

        let mut bad_quorum = settings.clone();
        bad_quorum.workflows[0].review_quorum = Some(0);
        assert!(
            bad_quorum
                .validate()
                .unwrap_err()
                .to_string()
                .contains("review_quorum")
        );

        let mut bad_policy = settings.clone();
        bad_policy.workflows[0].policies[0].pattern.clear();
        assert!(
            bad_policy
                .validate()
                .unwrap_err()
                .to_string()
                .contains("non-empty pattern")
        );
    }
}
