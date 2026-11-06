use crate::providers::Provider;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

pub const BUILTINS: &[&str] = &[
    "codex", "claude", "pi", "omp", "cursor", "gemini", "opencode", "aider", "goose", "amp",
    "copilot",
];

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
    #[default]
    Custom,
}
impl Role {
    pub const ALL: [Self; 7] = [
        Self::Research,
        Self::Planning,
        Self::Implementation,
        Self::Review,
        Self::Test,
        Self::Summary,
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
}
fn is_true(value: &bool) -> bool {
    *value
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
            sandbox: None,
            sandbox_writable: vec![],
            sandbox_network: true,
            sandbox_fallback: SandboxFallback::Refuse,
        }
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
    pub stages: Vec<Stage>,
}
fn two() -> u32 {
    2
}
impl Workflow {
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
        Self { name: "Feature development".into(), max_repair_passes: 2, sandbox_default: Sandbox::Off, stages: vec![
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
    /// Load notes: unknown fields, migrations, effort levels, trust. Not saved.
    #[serde(skip)]
    pub warnings: Vec<String>,
    /// Sections an untrusted project config had ignored; restored on save.
    #[serde(skip)]
    pub ignored: Option<Box<crate::trust::Ignored>>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            version: VERSION,
            workflows: vec![Workflow::default()],
            adapters: BTreeMap::new(),
            providers: BTreeMap::new(),
            warnings: vec![],
            ignored: None,
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
    let adapter = CustomAdapter {
        interactive_args: Some(vec![]),
        models_args: Some(vec![]),
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
        ..Workflow::default()
    });
    for (index, item) in value
        .get("workflows")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        extra(item, &workflow, &format!("workflows[{index}]"), &mut out);
    }
    for (section, known) in [("adapters", keys(&adapter)), ("providers", keys(&provider))] {
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
            let mut ids = HashSet::new();
            for (index, stage) in workflow.stages.iter().enumerate() {
                ensure!(
                    !stage.id.is_empty()
                        && stage
                            .id
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                    "Stage IDs must contain only letters, digits, '-' and '_'"
                );
                ensure!(ids.insert(&stage.id), "Duplicate stage ID {}", stage.id);
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
                if stage.enabled && !stage.repair_target.is_empty() {
                    ensure!(
                        stage.role == Role::Review,
                        "Only reviews can set a repair target"
                    );
                    ensure!(
                        workflow.stages[..index]
                            .iter()
                            .any(|s| s.id == stage.repair_target
                                && s.role == Role::Implementation
                                && s.enabled),
                        "Review {} needs an enabled upstream implementation target",
                        stage.name
                    );
                }
                if stage.enabled {
                    crate::adapters::validate_options(stage, &self.providers)?;
                }
            }
        }
        for (name, adapter) in &self.adapters {
            ensure!(
                !BUILTINS.contains(&name.as_str()),
                "Custom adapter cannot replace built-in {name}"
            );
            ensure!(
                !adapter.command.trim().is_empty(),
                "Custom adapter {name} needs a command"
            );
        }
        for (name, provider) in &self.providers {
            crate::providers::validate(name, provider)?;
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
    settings.validate()?;
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
            name: "w".into(),
            max_repair_passes: 0,
            sandbox_default: Sandbox::Off,
            stages: vec![Stage {
                id: "a".into(),
                harness: "claude".into(),
                model: "opus".into(),
                ..Stage::default()
            }],
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
