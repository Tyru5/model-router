use crate::{
    adapters::{self, Invocation, OutputKind},
    config::{CustomAdapter, Settings},
    process, storage,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::Path, sync::atomic::AtomicBool, time::Duration};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub source: String,
    pub availability: String,
    /// Reasoning levels the harness reports for this model; empty when it
    /// reports none, in which case the harness-wide levels apply.
    #[serde(default)]
    pub efforts: Vec<String>,
    #[serde(default)]
    pub default_effort: String,
}
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub models: Vec<Model>,
    pub note: String,
    /// Reasoning levels the harness accepts for any model; empty when unknown.
    pub efforts: Vec<String>,
    /// Where the levels were read from, e.g. `claude --help`.
    pub effort_source: String,
}
/// Reasoning levels that apply to one selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Efforts {
    pub levels: Vec<String>,
    pub default: String,
    pub source: String,
}
impl Catalog {
    /// Levels for a selection: the model's own when it lists them, else the
    /// harness-wide set. None when the harness reports nothing.
    pub fn efforts_for(&self, id: &str, provider: &str) -> Option<Efforts> {
        let model = self
            .models
            .iter()
            .find(|m| m.id == id && m.provider == provider && !m.efforts.is_empty());
        match model {
            Some(model) => Some(Efforts {
                levels: model.efforts.clone(),
                default: model.default_effort.clone(),
                source: model.source.clone(),
            }),
            None if self.efforts.is_empty() => None,
            None => Some(Efforts {
                levels: self.efforts.clone(),
                default: String::new(),
                source: self.effort_source.clone(),
            }),
        }
    }
}
fn entry(id: &str, source: &str, availability: &str) -> Model {
    Model {
        id: id.into(),
        label: if id.is_empty() {
            "Harness default".into()
        } else {
            id.into()
        },
        source: source.into(),
        availability: availability.into(),
        ..Model::default()
    }
}

const CLAUDE_ALIASES: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];
const CLAUDE_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const CODEX_EFFORTS: [&str; 5] = ["minimal", "low", "medium", "high", "xhigh"];
const PI_THINKING: [&str; 6] = ["off", "minimal", "low", "medium", "high", "xhigh"];

/// The text of one `--flag` entry in a clap/commander style help page.
fn help_option(help: &str, flag: &str) -> Option<String> {
    let mut lines = help.lines();
    let first = lines.find(|l| {
        let l = l.trim_start();
        l.starts_with(flag)
            && l[flag.len()..]
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace() || c == '=' || c == ',')
    })?;
    let mut text = first.trim().to_owned();
    for line in lines {
        if line.trim_start().starts_with('-') || line.trim().is_empty() {
            break;
        }
        text.push(' ');
        text.push_str(line.trim());
    }
    Some(text)
}

/// Aliases and effort levels documented by `claude --help`.
fn parse_claude_help(help: &str) -> (Vec<String>, Vec<String>) {
    let aliases = help_option(help, "--model")
        .map(|text| {
            text.split('\'')
                .skip(1)
                .step_by(2)
                .filter(|a| {
                    !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                })
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let efforts = help_option(help, "--effort")
        .and_then(|text| {
            let start = text.find('(')? + 1;
            let end = text[start..].find(')')? + start;
            Some(
                text[start..end]
                    .split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty() && !s.contains(' '))
                    .collect(),
            )
        })
        .unwrap_or_default();
    (aliases, efforts)
}

fn strings(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|s| (*s).to_owned()).collect()
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
        env: vec![],
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
    let mut efforts: Vec<String> = vec![];
    let mut effort_source = String::new();

    let note: String = match harness {
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
                                    model.efforts = value
                                        .get("supported_reasoning_levels")
                                        .and_then(Value::as_array)
                                        .map(|levels| {
                                            levels
                                                .iter()
                                                .filter_map(|l| {
                                                    l.get("effort").and_then(Value::as_str)
                                                })
                                                .map(str::to_owned)
                                                .collect()
                                        })
                                        .unwrap_or_default();
                                    model.default_effort = value
                                        .get("default_reasoning_level")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .into();
                                    for level in &model.efforts {
                                        if !efforts.contains(level) {
                                            efforts.push(level.clone());
                                        }
                                    }
                                    models.push(model);
                                }
                            }
                        }
                    }
                }
            }
            if efforts.is_empty() {
                efforts = strings(&CODEX_EFFORTS);
                effort_source = "documented Codex levels".into();
            } else {
                effort_source = "Codex local cache".into();
            }
            "Reads Codex's local model cache, including each model's reasoning levels. Open Codex to refresh its account catalog; custom IDs remain editable.".into()
        }
        "claude" => {
            let help = query(harness, vec!["--help".into()], &settings.adapters, project);
            let (aliases, levels) = help
                .as_deref()
                .map(parse_claude_help)
                .unwrap_or_default();
            let documented = !aliases.is_empty();
            for id in &aliases {
                models.push(entry(
                    id,
                    "claude --help alias",
                    "unverified; depends on account/provider",
                ));
            }
            for id in CLAUDE_ALIASES {
                if !aliases.iter().any(|a| a == id) {
                    models.push(entry(
                        id,
                        "documented alias",
                        "unverified; depends on account/provider",
                    ));
                }
            }
            if levels.is_empty() {
                efforts = strings(&CLAUDE_EFFORTS);
                effort_source = "documented Claude levels".into();
            } else {
                efforts = levels;
                effort_source = "claude --help".into();
            }
            match help {
                Ok(_) if documented => "Aliases and effort levels read from `claude --help`; they follow your native account/provider settings. Enter a full model ID for an explicit selection.".into(),
                Ok(_) => "`claude --help` did not list aliases; showing documented ones. Enter a full model ID for an explicit selection.".into(),
                Err(error) => format!(
                    "Could not run `claude --help` ({}); showing documented aliases and levels.",
                    storage::clean_terminal(&error.to_string())
                        .lines()
                        .next()
                        .unwrap_or_default()
                ),
            }
        }
        "amp" => {
            for id in ["low", "medium", "high", "ultra"] {
                models.push(entry(id, "Amp mode", "mode; underlying model may change"));
            }
            "Amp selects modes, not arbitrary model IDs. Plugin mode keys may be entered manually.".into()
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
                    // The `thinking` column says whether --thinking levels apply.
                    m.efforts = match cols.get(4).copied() {
                        Some("yes") => strings(&PI_THINKING),
                        Some("no") => vec!["off".into()],
                        _ => vec![],
                    };
                    models.push(m);
                }
            }
            efforts = strings(&PI_THINKING);
            effort_source = "Pi thinking levels".into();
            "Pi lists models from configured providers and extensions, with whether each supports thinking. Exact provider/model selections avoid fuzzy routing.".into()
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
            "Provider catalog, not an account entitlement check. Model IDs include the provider prefix.".into()
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
            "Cursor-native account selections. If your CLI uses a different list format, enter its exact slug manually.".into()
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
            "Custom adapters can provide models_args returning one native model ID per line."
                    .into()
        }
        "gemini" | "aider" | "goose" | "copilot" => {
            "Use your harness's configured default, a saved selection, or enter a native model ID. This adapter does not probe account entitlements.".into()
        }
        _ => anyhow::bail!("Unknown harness {harness}"),
    };
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
    Ok(Catalog {
        models,
        note,
        efforts,
        effort_source,
    })
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

/// Where `-C` was omitted: the project the current directory belongs to.
///
/// Walks from `start` upward and returns the nearest ancestor that holds a
/// `.model-router.toml`, or failing that the nearest Git root (`.git`
/// directory, or `.git` file for linked worktrees and submodules). The walk
/// never crosses a Git root, so a configuration outside the repository is not
/// picked up. Outside any repository and configuration, `start` itself is the
/// project.
pub fn detect_project(start: &Path) -> Result<std::path::PathBuf> {
    let start = check_project(start)?;
    for dir in start.ancestors() {
        if dir.join(".model-router.toml").is_file() {
            return Ok(dir.to_path_buf());
        }
        let git = dir.join(".git");
        if git.is_dir() || git.is_file() {
            return Ok(dir.to_path_buf());
        }
    }
    Ok(start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn claude_help_yields_aliases_and_effort_levels() {
        let help = "\
  --effort <level>                      Effort level for the current session
                                        (low, medium, high, xhigh, max)
  --environment <environment_id>        Create a new cloud session
  --model <model>                       Model for the current session. Provide
                                        an alias for the latest model (e.g.
                                        'fable', 'opus', or 'sonnet') or a
                                        model's full name.
  -n, --name <name>                     Set a display name
";
        let (aliases, efforts) = parse_claude_help(help);
        assert_eq!(aliases, ["fable", "opus", "sonnet"]);
        assert_eq!(efforts, ["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(parse_claude_help("usage: claude"), (vec![], vec![]));
    }

    #[test]
    fn efforts_prefer_the_model_then_the_harness() {
        let mut gpt = entry("gpt", "cache", "");
        gpt.efforts = vec!["low".into(), "max".into()];
        gpt.default_effort = "low".into();
        let catalog = Catalog {
            models: vec![entry("", "", ""), gpt, entry("other", "cache", "")],
            note: String::new(),
            efforts: vec!["low".into(), "high".into()],
            effort_source: "harness".into(),
        };
        let model = catalog.efforts_for("gpt", "").unwrap();
        assert_eq!(model.levels, ["low", "max"]);
        assert_eq!(model.default, "low");
        assert_eq!(model.source, "cache");
        let other = catalog.efforts_for("other", "").unwrap();
        assert_eq!(other.levels, ["low", "high"]);
        assert_eq!(other.source, "harness");
        assert_eq!(
            catalog.efforts_for("typed", "").unwrap().levels,
            ["low", "high"]
        );
        assert!(Catalog::default().efforts_for("gpt", "").is_none());
    }

    #[test]
    fn detect_project_prefers_nearest_config_then_git_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let repo = root.join("repo");
        let deep = repo.join("src").join("nested");
        fs::create_dir_all(&deep).unwrap();

        // Plain directories: the start itself.
        assert_eq!(detect_project(&deep).unwrap(), deep);

        // Git root without configuration.
        fs::create_dir(repo.join(".git")).unwrap();
        assert_eq!(detect_project(&deep).unwrap(), repo);
        assert_eq!(detect_project(&repo).unwrap(), repo);

        // Configuration at the Git root.
        fs::write(repo.join(".model-router.toml"), "version = 1\n").unwrap();
        assert_eq!(detect_project(&deep).unwrap(), repo);

        // A nearer configuration wins (monorepo sub-project).
        fs::write(repo.join("src/.model-router.toml"), "version = 1\n").unwrap();
        assert_eq!(detect_project(&deep).unwrap(), repo.join("src"));

        // A configuration outside the repository is never used.
        fs::write(root.join(".model-router.toml"), "version = 1\n").unwrap();
        let other = root.join("other");
        fs::create_dir_all(other.join("a")).unwrap();
        fs::write(other.join(".git"), "gitdir: /elsewhere\n").unwrap();
        assert_eq!(detect_project(&other.join("a")).unwrap(), other);
        // Outside any repository the nearest configuration still applies.
        let bare = root.join("bare");
        fs::create_dir(&bare).unwrap();
        assert_eq!(detect_project(&bare).unwrap(), root);
    }
}
