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
    },
];
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
}];
const CURSOR: &[Setting] = &[Setting {
    key: "sandbox",
    label: "Sandbox",
    choices: &["enabled", "disabled"],
    help: "Explicitly enable or disable Cursor's sandbox for this stage. Additional tool permissions can be supplied in Extra arguments.",
    flag: "--sandbox",
    config_key: None,
    conflicts: &[],
}];
const GEMINI: &[Setting] = &[Setting {
    key: "approval_mode",
    label: "Approval mode",
    choices: &["default", "auto_edit", "plan", "yolo"],
    help: "default: prompt for tool approval. auto_edit: approve edits. plan: planning mode. yolo: approve all actions. Unattended runs cannot answer prompts.",
    flag: "--approval-mode",
    config_key: None,
    conflicts: &["--yolo", "-y"],
}];

const OMP: &[Setting] = &[Setting {
    key: "approval_mode",
    label: "Approval mode",
    choices: &["always-ask", "write", "yolo"],
    help: "always-ask: prompt before tools run. write: approve edits automatically. yolo: approve all tool calls. Unattended runs cannot answer prompts.",
    flag: "--approval-mode",
    config_key: None,
    conflicts: &["--auto-approve"],
}];

pub fn settings(harness: &str) -> &'static [Setting] {
    match harness {
        "codex" => CODEX,
        "omp" => OMP,
        "claude" => CLAUDE,
        "cursor" => CURSOR,
        "gemini" => GEMINI,
        _ => &[],
    }
}

pub fn supports_provider(harness: &str) -> bool {
    !matches!(harness, "claude" | "cursor" | "gemini" | "amp" | "copilot")
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
            (self.flag != "--config" && matches_flag(argument, self.flag))
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
            args.push(setting.flag.into());
            args.push(if setting.flag == "--config" {
                format!("{}={}", setting.key, serde_json::to_string(value)?)
            } else {
                value.clone()
            });
        }
    }
    Ok(args)
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
