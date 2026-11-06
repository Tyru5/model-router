//! Per-stage native settings shared by the editor and command builder.
use crate::config::Stage;
use anyhow::{Context, Result, ensure};

pub struct Setting {
    pub key: &'static str,
    pub label: &'static str,
    pub choices: &'static [&'static str],
    pub help: &'static str,
    flag: &'static str,
    config_key: Option<&'static str>,
    conflicts: &'static [&'static str],
    /// Choices that loosen the harness's own approvals or sandbox. A project
    /// config needs trust to set them (see `trust`).
    escalating: &'static [&'static str],
    emit: Emit,
}

/// How a chosen value reaches the harness.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Emit {
    /// `flag value`, or `--config key="value"` for Codex config keys.
    Value,
    /// The flag alone; the setting has one non-default choice.
    Switch,
    /// An environment variable named by `flag`.
    Env,
}

const CODEX: &[Setting] = &[
    Setting {
        key: "sandbox",
        label: "Sandbox",
        choices: &["read-only", "workspace-write", "danger-full-access"],
        help: "read-only: inspect files. workspace-write: allow changes in the workspace. danger-full-access: run without sandbox restrictions.",
        flag: "--sandbox",
        config_key: Some("sandbox_mode"),
        conflicts: &[
            "-s",
            "--full-auto",
            "--approve-for-me",
            "--dangerously-bypass-approvals-and-sandbox",
        ],
        escalating: &["danger-full-access"],
        emit: Emit::Value,
    },
    Setting {
        key: "approval_policy",
        label: "Approval policy",
        choices: &["on-request", "never"],
        help: "on-request: the harness can request approval. never: no approval prompts; commands blocked by the sandbox stay blocked. Unattended runs cannot answer prompts.",
        flag: "--config",
        config_key: Some("approval_policy"),
        conflicts: &[
            "-a",
            "--ask-for-approval",
            "--full-auto",
            "--approve-for-me",
            "--dangerously-bypass-approvals-and-sandbox",
        ],
        escalating: &[],
        emit: Emit::Value,
    },
];
// Choices checked against `claude --help` 2.1.318 on 2026-11-05: acceptEdits,
// auto, bypassPermissions, manual, dontAsk, plan. Older releases called
// `manual` `default`.
const CLAUDE: &[Setting] = &[Setting {
    key: "permission_mode",
    label: "Permission mode",
    choices: &[
        "manual",
        "acceptEdits",
        "plan",
        "dontAsk",
        "auto",
        "bypassPermissions",
    ],
    help: "manual: ask for permissions. acceptEdits: approve file edits. plan: planning only. dontAsk: deny unapproved tools. auto: native automatic review. bypassPermissions: skip checks. Modes depend on your Claude version and account.",
    flag: "--permission-mode",
    config_key: None,
    conflicts: &["--dangerously-skip-permissions"],
    escalating: &["bypassPermissions"],
    emit: Emit::Value,
}];
const CURSOR: &[Setting] = &[Setting {
    key: "sandbox",
    label: "Sandbox",
    choices: &["enabled", "disabled"],
    help: "Explicitly enable or disable Cursor's sandbox for this stage. Additional tool permissions can be supplied in Extra arguments.",
    flag: "--sandbox",
    config_key: None,
    conflicts: &[],
    escalating: &["disabled"],
    emit: Emit::Value,
}];
const GEMINI: &[Setting] = &[Setting {
    key: "approval_mode",
    label: "Approval mode",
    choices: &["default", "auto_edit", "plan", "yolo"],
    help: "default: prompt for tool approval. auto_edit: approve edits. plan: planning mode. yolo: approve all actions. Unattended runs cannot answer prompts.",
    flag: "--approval-mode",
    config_key: None,
    conflicts: &["--yolo", "-y"],
    escalating: &["yolo"],
    emit: Emit::Value,
}];

const OMP: &[Setting] = &[Setting {
    key: "approval_mode",
    label: "Approval mode",
    choices: &["always-ask", "write", "yolo"],
    help: "always-ask: prompt before tools run. write: approve edits automatically. yolo: approve all tool calls. Unattended runs cannot answer prompts.",
    flag: "--approval-mode",
    config_key: None,
    conflicts: &["--auto-approve"],
    escalating: &["yolo"],
    emit: Emit::Value,
}];

/// Unattended Aider cannot answer its confirmation prompts without this.
const AIDER: &[Setting] = &[Setting {
    key: "confirmations",
    label: "Confirmations",
    choices: &["yes-always"],
    help: "yes-always: answer yes to Aider's confirmation prompts (adding files, creating files). Aider still asks before running shell commands. Unattended runs cannot answer prompts.",
    flag: "--yes-always",
    config_key: None,
    conflicts: &["--yes"],
    escalating: &["yes-always"],
    emit: Emit::Switch,
}];

/// Goose reads its permission mode from GOOSE_MODE; `run` has no flag.
const GOOSE: &[Setting] = &[Setting {
    key: "mode",
    label: "Permission mode",
    choices: &["approve", "smart_approve", "chat", "auto"],
    help: "approve: ask before every tool. smart_approve: ask only for risky tools. chat: no tools. auto: run tools without asking. Set as GOOSE_MODE. Unattended runs cannot answer prompts.",
    flag: "GOOSE_MODE",
    config_key: None,
    conflicts: &[],
    escalating: &["auto"],
    emit: Emit::Env,
}];

const COPILOT: &[Setting] = &[Setting {
    key: "tool_permissions",
    label: "Tool permissions",
    choices: &["allow-all-tools"],
    help: "allow-all-tools: let Copilot run every tool without asking, as programmatic mode requires for tool use. Use Extra arguments for --allow-tool/--deny-tool rules.",
    flag: "--allow-all-tools",
    config_key: None,
    conflicts: &["--allow-all", "--yolo"],
    escalating: &["allow-all-tools"],
    emit: Emit::Switch,
}];

pub fn settings(harness: &str) -> &'static [Setting] {
    match harness {
        "codex" => CODEX,
        "omp" => OMP,
        "claude" => CLAUDE,
        "cursor" => CURSOR,
        "gemini" => GEMINI,
        "aider" => AIDER,
        "goose" => GOOSE,
        "copilot" => COPILOT,
        _ => &[],
    }
}

/// Harnesses that only use their own account: no provider field and no
/// router-defined provider.
pub const OWN_ACCOUNT_ONLY: &[&str] = &["claude", "cursor", "gemini", "amp", "copilot"];

pub fn supports_provider(harness: &str) -> bool {
    !OWN_ACCOUNT_ONLY.contains(&harness)
}

/// `(key, value)` settings of `stage` that loosen the harness's own checks.
pub fn escalations(stage: &Stage) -> Vec<(&str, &str)> {
    settings(&stage.harness)
        .iter()
        .filter_map(|setting| {
            let value = stage.harness_settings.get(setting.key)?;
            setting
                .escalating
                .contains(&value.as_str())
                .then_some((setting.key, value.as_str()))
        })
        .collect()
}

pub fn supports_effort(harness: &str) -> bool {
    !matches!(
        harness,
        "cursor" | "gemini" | "aider" | "goose" | "amp" | "copilot"
    )
}

fn matches_flag(argument: &str, flag: &str) -> bool {
    argument == flag
        || argument.strip_prefix(flag).is_some_and(|suffix| {
            suffix.starts_with('=') || (flag.len() == 2 && !suffix.is_empty())
        })
}

impl Setting {
    fn conflicts_with(&self, arguments: &[String]) -> bool {
        arguments.iter().enumerate().any(|(index, argument)| {
            // --config is repeatable; only the matching key conflicts.
            (self.flag != "--config" && self.emit != Emit::Env && matches_flag(argument, self.flag))
                || self
                    .conflicts
                    .iter()
                    .any(|flag| matches_flag(argument, flag))
                || self.config_key.is_some_and(|key| {
                    let value = if matches!(argument.as_str(), "--config" | "-c") {
                        arguments.get(index + 1).map(String::as_str)
                    } else {
                        argument.strip_prefix("--config=").or_else(|| {
                            argument
                                .strip_prefix("-c")
                                .map(|v| v.trim_start_matches('='))
                        })
                    };
                    value
                        .and_then(|v| v.split_once('='))
                        .is_some_and(|(name, _)| name.trim().trim_matches(['\'', '"']) == key)
                })
        })
    }
}

pub fn validate(stage: &Stage) -> Result<()> {
    for (key, value) in &stage.harness_settings {
        let setting = settings(&stage.harness)
            .iter()
            .find(|s| s.key == key)
            .with_context(|| format!("{} has no stage setting {key}", stage.harness))?;
        if value.is_empty() {
            continue;
        }
        ensure!(
            setting.choices.contains(&value.as_str()),
            "Invalid {} for {}: {value}",
            setting.label,
            stage.harness
        );
        ensure!(
            !setting.conflicts_with(&stage.extra_args),
            "{} is also set in Extra arguments. Remove the conflicting argument or choose Harness default.",
            setting.label
        );
    }
    Ok(())
}

pub fn arguments(stage: &Stage) -> Result<Vec<String>> {
    validate(stage)?;
    let mut args = Vec::new();
    for setting in settings(&stage.harness) {
        if let Some(value) = stage
            .harness_settings
            .get(setting.key)
            .filter(|v| !v.is_empty())
        {
            match setting.emit {
                Emit::Env => {}
                Emit::Switch => args.push(setting.flag.into()),
                Emit::Value => {
                    args.push(setting.flag.into());
                    args.push(if setting.flag == "--config" {
                        format!("{}={}", setting.key, serde_json::to_string(value)?)
                    } else {
                        value.clone()
                    });
                }
            }
        }
    }
    Ok(args)
}

/// Environment variables the stage's settings set, e.g. Goose's GOOSE_MODE.
pub fn environment(stage: &Stage) -> Vec<(String, String)> {
    settings(&stage.harness)
        .iter()
        .filter(|setting| setting.emit == Emit::Env)
        .filter_map(|setting| {
            let value = stage.harness_settings.get(setting.key)?;
            (!value.is_empty()).then(|| (setting.flag.to_owned(), value.clone()))
        })
        .collect()
}

pub fn summary(stage: &Stage) -> String {
    let mut labels: Vec<_> = settings(&stage.harness)
        .iter()
        .filter_map(|setting| {
            stage
                .harness_settings
                .get(setting.key)
                .filter(|v| !v.is_empty())
                .map(|value| format!("{}: {value}", setting.label))
        })
        .collect();
    if labels.is_empty() {
        labels.push("Permissions: harness default".into());
    }
    if !stage.extra_args.is_empty() {
        labels.push("Extra arguments: configured".into());
    }
    labels.join("\n")
}
