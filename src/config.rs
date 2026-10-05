use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

pub const BUILTINS: &[&str] = &[
    "codex", "claude", "pi", "cursor", "gemini", "opencode", "aider", "goose", "amp", "copilot",
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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Research,
    Planning,
    Implementation,
    Review,
    Test,
    #[default]
    Custom,
}
impl Role {
    pub const ALL: [Self; 6] = [
        Self::Research,
        Self::Planning,
        Self::Implementation,
        Self::Review,
        Self::Test,
        Self::Custom,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Research => "research",
            Self::Planning => "planning",
            Self::Implementation => "implementation",
            Self::Review => "review",
            Self::Test => "test",
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    pub name: String,
    #[serde(default = "two")]
    pub max_repair_passes: u32,
    pub stages: Vec<Stage>,
}
fn two() -> u32 {
    2
}
impl Default for Workflow {
    fn default() -> Self {
        Self { name: "Feature development".into(), max_repair_passes: 2, stages: vec![
        Stage { id: "research".into(), name: "Investigate".into(), role: Role::Research, model: "gpt-6-astra".into(),
            instructions: "Investigate the task and relevant project files. Explain the root cause or requirements, cite evidence, and propose a concrete implementation plan. Do not change project code.".into(), ..Stage::default() },
        Stage { id: "implement".into(), name: "Implement".into(), role: Role::Implementation, harness: "claude".into(), model: "sonnet".into(), mode: Mode::Interactive,
            instructions: "Implement the task using the prior findings. Follow project conventions, run appropriate checks, and report what changed and any remaining issues.".into(), ..Stage::default() },
        Stage { id: "review".into(), name: "Review (optional)".into(), role: Role::Review, enabled: false, model: "gpt-6-astra".into(),
            instructions: "Review the implementation and test evidence. Do not modify code. Distinguish blocking correctness issues and failed required checks from advisory suggestions. Give an explicit pass or needs_changes verdict.".into(), ..Stage::default() },
    ] }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CustomAdapter {
    pub command: String,
    pub args: Vec<String>,
    pub interactive_args: Option<Vec<String>>,
    /// Optional arguments to list model IDs (one per line) using the same executable.
    pub models_args: Option<Vec<String>>,
    pub prompt_stdin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub version: u32,
    pub workflows: Vec<Workflow>,
    pub adapters: BTreeMap<String, CustomAdapter>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            workflows: vec![Workflow::default()],
            adapters: BTreeMap::new(),
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
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
                    crate::adapters::validate_options(stage)?;
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

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        crate::storage::atomic_write(path, toml::to_string_pretty(self)?.as_bytes())
    }
}

pub fn config_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "model-router")
        .map(|p| p.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".model-router-config"))
}

pub fn load(project: &Path, explicit: Option<&Path>) -> Result<(Settings, PathBuf)> {
    let project_path = project.join(".model-router.toml");
    let global = config_dir().join("config.toml");
    let source = explicit
        .map(Path::to_path_buf)
        .or_else(|| project_path.is_file().then(|| project_path.clone()))
        .or_else(|| global.is_file().then_some(global));
    let settings = match &source {
        Some(path) => toml::from_str::<Settings>(
            &fs::read_to_string(path).with_context(|| format!("Read {}", path.display()))?,
        )
        .with_context(|| format!("Parse {}", path.display()))?,
        None => Settings::default(),
    };
    settings.validate()?;
    // Inherited templates save into this project unless a config was explicitly selected.
    let save_path = explicit.map(Path::to_path_buf).unwrap_or(project_path);
    Ok((settings, save_path))
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
version = 1
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
        let settings: Settings = toml::from_str(CHECKS).unwrap();
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
    fn invalid_checks_are_rejected() {
        let error = |edit: fn(&mut Vec<Check>)| {
            let mut settings: Settings = toml::from_str(CHECKS).unwrap();
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
        assert!(
            toml::from_str::<Settings>(&CHECKS.replace("required = false", "shell = true"))
                .is_err()
        );
    }
}
