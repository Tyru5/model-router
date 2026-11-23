//! Configuration of delegation: a stage's `delegation` section and the
//! top-level `[delegates.NAME]` targets. See `docs/delegation.md`.
use crate::config::{BUILTINS, Billing, CustomAdapter, Mode, Role, Stage, Workflow};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_TASKS_CAP: u32 = 64;
pub const MAX_WAKE_TURNS_CAP: u32 = 32;
pub const MAX_PARALLEL_CAP: u32 = 16;
pub const MAX_RUNS_CAP: u32 = 20;
/// `task_send` turns one task may take after its first.
pub const MAX_TASK_SENDS: usize = 4;

/// Where a task works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Isolation {
    /// The parent's checkout, read-only.
    Shared,
    /// A detached worktree from a snapshot of the parent's checkout.
    Worktree,
}
impl Isolation {
    pub fn label(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Worktree => "worktree",
        }
    }
}

/// A stage's delegation policy. Present means enabled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Delegation {
    /// Named targets (`[delegates.NAME]`) this stage may use.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub delegates: Vec<String>,
    /// Harnesses the model may pick ad hoc; empty allows named targets only.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub harnesses: Vec<String>,
    /// Model globs ad-hoc targets must match; empty allows catalogued models.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Router providers ad-hoc targets may use.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<String>,
    pub max_tasks: u32,
    pub max_parallel: u32,
    pub max_wake_turns: u32,
    #[serde(skip_serializing_if = "crate::delegation::config::is_false")]
    pub runs: bool,
    pub max_runs: u32,
    #[serde(skip_serializing_if = "crate::delegation::config::is_false")]
    pub schedules: bool,
}
impl Default for Delegation {
    fn default() -> Self {
        Self {
            delegates: vec![],
            harnesses: vec![],
            models: vec![],
            providers: vec![],
            max_tasks: 12,
            max_parallel: 3,
            max_wake_turns: 8,
            runs: false,
            max_runs: 4,
            schedules: false,
        }
    }
}

pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}

/// A named delegate target: fixed settings the model picks by name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Delegate {
    pub harness: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub effort: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub harness_settings: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stall_timeout_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isolation: Option<Isolation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub billing: Option<Billing>,
}
impl Delegate {
    /// One line for trust reviews and previews, with every value.
    pub fn describe(&self) -> String {
        let mut text = format!("harness {}", self.harness);
        for (label, value) in [
            ("model", &self.model),
            ("provider", &self.provider),
            ("effort", &self.effort),
        ] {
            if !value.is_empty() {
                text.push_str(&format!(" {label} {value}"));
            }
        }
        if !self.harness_settings.is_empty() {
            text.push_str(&format!(" settings {:?}", self.harness_settings));
        }
        if let Some(isolation) = self.isolation {
            text.push_str(&format!(" isolation {}", isolation.label()));
        }
        text
    }
}

/// The roles a task may take.
pub const TASK_ROLES: [Role; 5] = [
    Role::Research,
    Role::Planning,
    Role::Implementation,
    Role::Review,
    Role::Test,
];

impl Delegation {
    /// One line for trust reviews, with every value that grants something.
    pub fn describe(&self) -> String {
        format!(
            "delegates {:?}; harnesses {:?}; models {:?}; providers {:?}; max_tasks {}; max_parallel {}; max_wake_turns {}; runs {}; max_runs {}; schedules {}",
            self.delegates,
            self.harnesses,
            self.models,
            self.providers,
            self.max_tasks,
            self.max_parallel,
            self.max_wake_turns,
            self.runs,
            self.max_runs,
            self.schedules
        )
    }

    /// Whether any target is allowed at all.
    pub fn has_targets(&self) -> bool {
        !self.delegates.is_empty() || !self.harnesses.is_empty()
    }
}

/// Whether `model` matches glob `pattern` (`*` matches any run of
/// characters, `?` one character).
pub fn glob_match(pattern: &str, model: &str) -> bool {
    fn inner(p: &[char], m: &[char]) -> bool {
        match (p.first(), m.first()) {
            (None, None) => true,
            (Some('*'), _) => inner(&p[1..], m) || (!m.is_empty() && inner(p, &m[1..])),
            (Some('?'), Some(_)) => inner(&p[1..], &m[1..]),
            (Some(a), Some(b)) if a == b => inner(&p[1..], &m[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let m: Vec<char> = model.chars().collect();
    inner(&p, &m)
}

fn known_harness(harness: &str, adapters: &BTreeMap<String, CustomAdapter>) -> bool {
    BUILTINS.contains(&harness) || adapters.contains_key(harness)
}

/// The stage a named delegate runs as: its harness, model, and settings,
/// unattended, with the task's role and the parent's timeouts unless the
/// delegate sets its own.
pub fn delegate_stage(id: &str, delegate: &Delegate, parent: &Stage, role: Role) -> Stage {
    Stage {
        id: id.into(),
        name: id.into(),
        role,
        enabled: true,
        harness: delegate.harness.clone(),
        model: delegate.model.clone(),
        provider: delegate.provider.clone(),
        effort: delegate.effort.clone(),
        mode: Mode::Unattended,
        harness_settings: delegate.harness_settings.clone(),
        timeout_seconds: delegate.timeout_seconds.unwrap_or(parent.timeout_seconds),
        stall_timeout_seconds: delegate
            .stall_timeout_seconds
            .or(parent.stall_timeout_seconds),
        billing: delegate.billing,
        sandbox_network: parent.sandbox_network,
        ..Stage::default()
    }
}

/// Validates every stage's delegation section and the named delegates.
pub fn validate(
    workflows: &[Workflow],
    delegates: &BTreeMap<String, Delegate>,
    adapters: &BTreeMap<String, CustomAdapter>,
    providers: &BTreeMap<String, crate::providers::Provider>,
) -> Result<()> {
    for (name, delegate) in delegates {
        ensure!(
            crate::config::is_id(name),
            "Delegate names must contain only letters, digits, '-' and '_' ({name})"
        );
        ensure!(
            known_harness(&delegate.harness, adapters),
            "Delegate {name} uses unknown harness {}",
            delegate.harness
        );
        ensure!(
            !delegate.model.contains('\0') && !delegate.provider.contains('\0'),
            "Delegate {name}: model and provider cannot contain NUL"
        );
        ensure!(
            delegate.timeout_seconds != Some(0) && delegate.stall_timeout_seconds != Some(0),
            "Delegate {name}: timeouts must be positive"
        );
        if delegate.stall_timeout_seconds.is_some() {
            ensure!(
                crate::adapters::streams(&delegate.harness, adapters),
                "Delegate {name} sets stall_timeout_seconds, but {} prints its output only when it exits",
                delegate.harness
            );
        }
        let stage = delegate_stage(name, delegate, &Stage::default(), Role::Research);
        crate::harness_settings::validate(&stage)
            .map_err(|error| anyhow::anyhow!("Delegate {name}: {error:#}"))?;
        crate::adapters::validate_options(&stage, providers)
            .map_err(|error| anyhow::anyhow!("Delegate {name}: {error:#}"))?;
    }
    for workflow in workflows {
        for stage in &workflow.stages {
            let Some(delegation) = &stage.delegation else {
                continue;
            };
            let at = format!("Stage {} in workflow {}", stage.id, workflow.name);
            ensure!(
                stage.fan_out.is_none(),
                "{at} cannot delegate: it is a fan-out stage"
            );
            ensure!(
                !matches!(stage.role, Role::Ship | Role::Summary),
                "{at} cannot delegate: {} stages do not",
                stage.role.label()
            );
            ensure!(
                delegation.has_targets(),
                "{at} enables delegation without targets: list delegates or harnesses"
            );
            ensure!(
                crate::adapters::can_delegate(&stage.harness, adapters),
                "{at} cannot delegate: {} cannot receive the router's MCP server (Claude Code, Codex, OpenCode, or a custom adapter with mcp_config_args can)",
                stage.harness
            );
            // A parent prints nothing while a tool call waits (up to 45 s).
            ensure!(
                stage.stall_timeout_seconds.is_none_or(|s| s >= 60),
                "{at}: a delegating stage's stall_timeout_seconds must be at least 60, since tool calls wait up to 45 seconds without output"
            );
            for name in &delegation.delegates {
                ensure!(
                    delegates.contains_key(name),
                    "{at} names unknown delegate {name}"
                );
            }
            for harness in &delegation.harnesses {
                ensure!(
                    known_harness(harness, adapters),
                    "{at} allows unknown harness {harness}"
                );
            }
            for provider in &delegation.providers {
                ensure!(
                    providers.contains_key(provider)
                        || crate::providers::resolve(provider, providers).is_some(),
                    "{at} allows unknown provider {provider}"
                );
            }
            ensure!(
                delegation
                    .models
                    .iter()
                    .all(|m| !m.is_empty() && !m.contains('\0')),
                "{at}: model globs must be non-empty"
            );
            ensure!(
                (1..=MAX_TASKS_CAP).contains(&delegation.max_tasks),
                "{at}: max_tasks must be 1–{MAX_TASKS_CAP}"
            );
            ensure!(
                (1..=MAX_PARALLEL_CAP).contains(&delegation.max_parallel),
                "{at}: max_parallel must be 1–{MAX_PARALLEL_CAP}"
            );
            ensure!(
                (1..=MAX_WAKE_TURNS_CAP).contains(&delegation.max_wake_turns),
                "{at}: max_wake_turns must be 1–{MAX_WAKE_TURNS_CAP}"
            );
            ensure!(
                delegation.max_runs <= MAX_RUNS_CAP,
                "{at}: max_runs must be 0–{MAX_RUNS_CAP}"
            );
            for name in &delegation.delegates {
                let delegate = &delegates[name];
                crate::delegation::policy::check_delegate_ceiling(workflow, stage, name, delegate)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;

    fn settings(toml: &str) -> Result<Settings> {
        let settings = crate::config::parse(toml)?;
        settings.validate()?;
        Ok(settings)
    }

    const BASE: &str = r#"
version = 2
[delegates.codex-review]
harness = "codex"
model = "gpt-6.2-sol"
harness_settings = { sandbox = "read-only" }

[[workflows]]
name = "W"
[[workflows.stages]]
id = "implement"
name = "Implement"
role = "implementation"
harness = "claude"
"#;

    #[test]
    fn a_stage_delegates_to_named_and_ad_hoc_targets() {
        let text = format!(
            "{BASE}[workflows.stages.delegation]\ndelegates = [\"codex-review\"]\nharnesses = [\"opencode\"]\nmodels = [\"openrouter/*\"]\nmax_tasks = 4\n"
        );
        let settings = settings(&text).unwrap();
        let delegation = settings.workflows[0].stages[0].delegation.clone().unwrap();
        assert_eq!(delegation.delegates, ["codex-review"]);
        assert_eq!(delegation.max_tasks, 4);
        assert_eq!(delegation.max_parallel, 3, "defaults fill the rest");
        assert_eq!(settings.delegates["codex-review"].model, "gpt-6.2-sol");
        // Saved and parsed again, nothing changes.
        let saved = toml::to_string(&settings).unwrap();
        let again = crate::config::parse(&saved).unwrap();
        assert_eq!(again.workflows[0].stages[0].delegation, Some(delegation));
    }

    #[test]
    fn invalid_delegation_fails_with_a_reason() {
        let cases = [
            ("[workflows.stages.delegation]\n", "without targets"),
            (
                "[workflows.stages.delegation]\ndelegates = [\"nope\"]\n",
                "unknown delegate nope",
            ),
            (
                "[workflows.stages.delegation]\nharnesses = [\"nope\"]\n",
                "unknown harness nope",
            ),
            (
                "[workflows.stages.delegation]\nharnesses = [\"codex\"]\nmax_tasks = 0\n",
                "max_tasks",
            ),
            (
                "[workflows.stages.delegation]\nharnesses = [\"codex\"]\nmax_wake_turns = 99\n",
                "max_wake_turns",
            ),
            (
                "[workflows.stages.delegation]\nharnesses = [\"codex\"]\nbogus = 1\n",
                "unknown field",
            ),
            (
                "retry = { max_attempts = 1, base_delay_seconds = 1, max_delay_seconds = 5 }\n[workflows.stages.delegation]\nharnesses = [\"codex\"]\n",
                "does not retry automatically",
            ),
        ];
        for (section, expected) in cases {
            let error = settings(&format!("{BASE}{section}")).unwrap_err();
            assert!(
                format!("{error:#}").contains(expected),
                "{expected}: {error:#}"
            );
        }
    }

    #[test]
    fn fan_out_and_summary_stages_cannot_delegate() {
        let mut settings = crate::config::parse(BASE).unwrap();
        let stage = &mut settings.workflows[0].stages[0];
        stage.delegation = Some(Delegation {
            harnesses: vec!["codex".into()],
            ..Delegation::default()
        });
        stage.role = Role::Summary;
        let error = settings.validate().unwrap_err();
        assert!(
            format!("{error:#}").contains("summary stages do not"),
            "{error:#}"
        );
    }

    #[test]
    fn a_delegate_cannot_exceed_the_stage_that_names_it() {
        let mut settings = crate::config::parse(BASE).unwrap();
        settings
            .delegates
            .get_mut("codex-review")
            .unwrap()
            .harness_settings = [("sandbox".to_owned(), "danger-full-access".to_owned())].into();
        settings.workflows[0].stages[0].delegation = Some(Delegation {
            delegates: vec!["codex-review".into()],
            ..Delegation::default()
        });
        let error = settings.validate().unwrap_err();
        assert!(
            format!("{error:#}").contains("exceed the stage's"),
            "{error:#}"
        );
    }

    #[test]
    fn globs_match_whole_model_ids() {
        assert!(glob_match("gpt-6.*", "gpt-6.2-sol"));
        assert!(glob_match("claude-*", "claude-opus-5-5"));
        assert!(!glob_match("claude-*", "xclaude-opus"));
        assert!(glob_match("m?", "m1"));
        assert!(!glob_match("m?", "m12"));
        assert!(glob_match("*", ""));
    }
}
