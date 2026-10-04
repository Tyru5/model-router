use crate::{
    adapters::{self, Invocation, OutputKind},
    config::{CustomAdapter, Settings},
    process, storage,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::Path, sync::atomic::AtomicBool, time::Duration};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub source: String,
    pub availability: String,
}
#[derive(Debug, Clone)]
pub struct Catalog {
    pub models: Vec<Model>,
    pub note: String,
}
fn entry(id: &str, source: &str, availability: &str) -> Model {
    Model {
        id: id.into(),
        provider: String::new(),
        label: if id.is_empty() {
            "Harness default".into()
        } else {
            id.into()
        },
        source: source.into(),
        availability: availability.into(),
    }
}

fn query(
    harness: &str,
    args: Vec<String>,
    custom: &BTreeMap<String, CustomAdapter>,
    project: &Path,
) -> Result<String> {
    let temp = tempfile::tempdir()?;
    let inv = Invocation {
        program: adapters::program(harness, custom),
        args,
        stdin: None,
        kind: OutputKind::Text,
        final_file: temp.path().join("unused"),
    };
    let result = process::capture(
        &inv,
        project,
        temp.path(),
        Duration::from_secs(15),
        &AtomicBool::new(false),
        |_, _| {},
    )?;
    ensure!(
        result.success(),
        "Catalog query failed or timed out: {}",
        storage::clean_terminal(
            &process::read_tail(&temp.path().join("stderr.log"), 1500).unwrap_or_default()
        )
    );
    Ok(storage::clean_terminal(&process::read_tail(
        &temp.path().join("stdout.log"),
        2 * 1024 * 1024,
    )?))
}

pub fn discover(harness: &str, settings: &Settings, project: &Path) -> Result<Catalog> {
    let mut models = vec![entry("", "harness setting", "inherits configured default")];
    let note: String;
    match harness {
        "codex" => {
            let dir = std::env::var_os("CODEX_HOME")
                .map(std::path::PathBuf::from)
                .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().join(".codex")));
            if let Some(dir) = dir {
                if let Ok(text) = fs::read_to_string(dir.join("models_cache.json")) {
                    if let Ok(value) = serde_json::from_str::<Value>(&text) {
                        if let Some(list) = value.get("models").and_then(Value::as_array) {
                            for value in list {
                                if matches!(
                                    value.get("visibility").and_then(Value::as_str),
                                    Some("hide" | "hidden")
                                ) {
                                    continue;
                                }
                                if let Some(id) = value.get("slug").and_then(Value::as_str) {
                                    let mut model = entry(
                                        id,
                                        "Codex local cache",
                                        "cached; entitlement not checked",
                                    );
                                    model.label = value
                                        .get("display_name")
                                        .and_then(Value::as_str)
                                        .unwrap_or(id)
                                        .into();
                                    models.push(model);
                                }
                            }
                        }
                    }
                }
            }
            note = "Reads Codex's local model cache. Open Codex to refresh its account catalog; custom IDs remain editable.".into();
        }
        "claude" => {
            for id in ["sonnet", "opus", "haiku"] {
                models.push(entry(
                    id,
                    "documented alias",
                    "unverified; depends on account/provider",
                ));
            }
            note = "Claude aliases follow your native account/provider settings. Enter a full model ID for an explicit selection.".into();
        }
        "amp" => {
            for id in ["low", "medium", "high", "ultra"] {
                models.push(entry(id, "Amp mode", "mode; underlying model may change"));
            }
            note = "Amp selects modes, not arbitrary model IDs. Plugin mode keys may be entered manually.".into();
        }
        "pi" => {
            let output = query(
                harness,
                vec!["--list-models".into()],
                &settings.adapters,
                project,
            )?;
            for line in output.lines() {
                let cols: Vec<&str> = line.split_whitespace().collect();
                if cols.len() >= 4
                    && cols[0] != "provider"
                    && cols[2].chars().any(|c| c.is_ascii_digit())
                {
                    let mut m = entry(
                        cols[1],
                        "Pi provider catalog",
                        "configured provider; entitlement not checked",
                    );
                    m.provider = cols[0].into();
                    models.push(m);
                }
            }
            note = "Pi lists models from configured providers and extensions. Exact provider/model selections avoid fuzzy routing.".into();
        }
        "opencode" => {
            let output = query(harness, vec!["models".into()], &settings.adapters, project)?;
            for line in output.lines().map(str::trim) {
                if !line.contains(char::is_whitespace) && line.contains('/') {
                    models.push(entry(
                        line,
                        "OpenCode provider catalog",
                        "catalog; entitlement not checked",
                    ));
                }
            }
            note = "Provider catalog, not an account entitlement check. Model IDs include the provider prefix.".into();
        }
        "cursor" => {
            let output = query(
                harness,
                vec!["--list-models".into()],
                &settings.adapters,
                project,
            )?;
            for line in output.lines() {
                let line = line.trim().trim_start_matches(['*', '•', '-']).trim();
                // Cursor lists `slug - Display name`; keep native parameterized slugs.
                if let Some((id, label)) = line.split_once(" - ") {
                    if !id.is_empty() && !id.contains(' ') {
                        let mut model = entry(id, "Cursor account listing", "account-reported");
                        model.label = label.into();
                        models.push(model);
                    }
                }
            }
            note = "Cursor-native account selections. If your CLI uses a different list format, enter its exact slug manually.".into();
        }
        name if settings.adapters.contains_key(name) => {
            let custom = &settings.adapters[name];
            if let Some(args) = &custom.models_args {
                for line in query(name, args.clone(), &settings.adapters, project)?
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                {
                    models.push(entry(line, "custom catalog command", "adapter-reported"));
                }
            }
            note =
                "Custom adapters can provide models_args returning one native model ID per line."
                    .into();
        }
        "gemini" | "aider" | "goose" | "copilot" => {
            note = "Use your harness's configured default, a saved selection, or enter a native model ID. This adapter does not probe account entitlements.".into();
        }
        _ => anyhow::bail!("Unknown harness {harness}"),
    }
    for stage in settings
        .workflows
        .iter()
        .flat_map(|w| &w.stages)
        .filter(|s| s.harness == harness && !s.model.is_empty())
    {
        if !models
            .iter()
            .any(|m| m.id == stage.model && m.provider == stage.provider)
        {
            let mut model = entry(&stage.model, "saved workflow", "unverified selection");
            model.provider = stage.provider.clone();
            models.push(model);
        }
    }
    ensure!(models.len() <= 20000, "Catalog contains too many entries");
    models.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));
    models.dedup_by(|a, b| a.id == b.id && a.provider == b.provider);
    Ok(Catalog { models, note })
}

pub fn known_harnesses(settings: &Settings) -> Vec<String> {
    crate::config::BUILTINS
        .iter()
        .map(|s| (*s).into())
        .chain(settings.adapters.keys().cloned())
        .collect()
}

pub fn check_project(project: &Path) -> Result<std::path::PathBuf> {
    let path = fs::canonicalize(project)
        .with_context(|| format!("Project not found: {}", project.display()))?;
    ensure!(path.is_dir(), "Project must be a directory");
    Ok(path)
}
