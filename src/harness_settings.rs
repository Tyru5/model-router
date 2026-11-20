//! Per-stage native settings shared by the editor and command builder.
use crate::config::Stage;
use anyhow::{Context, Result, ensure};

use crate::harness::Emit;
pub use crate::harness::Setting;

/// The harness's native settings, from its spec; none for custom adapters.
pub fn settings(harness: &str) -> &'static [Setting] {
    crate::harness::get(harness).map_or(&[], |spec| spec.settings)
}

/// Whether a stage can set a provider: not for harnesses that only use their
/// own account. Custom adapters can.
pub fn supports_provider(harness: &str) -> bool {
    crate::harness::get(harness).is_none_or(|spec| spec.supports_provider())
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

/// Whether the harness has a portable effort option. Custom adapters do.
pub fn supports_effort(harness: &str) -> bool {
    crate::harness::get(harness).is_none_or(|spec| spec.supports_effort())
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

/// The stage's settings that have a config key, each as `flag key="value"`:
/// for commands that take config overrides but not the settings' own flags.
pub fn config_arguments(stage: &Stage, flag: &str) -> Result<Vec<String>> {
    validate(stage)?;
    let mut args = Vec::new();
    for setting in settings(&stage.harness) {
        let value = stage
            .harness_settings
            .get(setting.key)
            .filter(|v| !v.is_empty());
        if let (Some(value), Some(key)) = (value, setting.config_key) {
            args.push(flag.into());
            args.push(format!("{key}={}", serde_json::to_string(value)?));
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
