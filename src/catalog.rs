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
    /// Context length and prices from a router provider's `/models`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<crate::providers::ModelInfo>,
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
const OMP_THINKING: [&str; 8] = [
    "off", "minimal", "low", "medium", "high", "xhigh", "max", "auto",
];

/// Effort levels a harness documents, without running it; None when they
/// depend on the model (OpenCode variants) or the harness has no effort.
pub fn known_efforts(harness: &str) -> Option<&'static [&'static str]> {
    match harness {
        "claude" => Some(&CLAUDE_EFFORTS),
        "codex" => Some(&CODEX_EFFORTS),
        "pi" => Some(&PI_THINKING),
        "omp" => Some(&OMP_THINKING),
        _ => None,
    }
}

/// Catalog output kept from the end of a listing.
const LISTING_LIMIT: usize = 2 * 1024 * 1024;

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

/// A listing's output; `truncated` when only its last 2 MiB were kept.
struct Listing {
    text: String,
    truncated: bool,
}

/// The last `limit` bytes of `path`, starting at a whole line.
fn read_listing(path: &Path, limit: usize) -> Result<Listing> {
    let size = fs::metadata(path)?.len();
    let mut text = process::read_tail(path, limit)?;
    let truncated = size > limit as u64;
    if truncated {
        // The tail starts mid-line; that line cannot be parsed.
        text = text
            .split_once('\n')
            .map(|(_, rest)| rest.to_owned())
            .unwrap_or_default();
    }
    Ok(Listing { text, truncated })
}

fn query(
    harness: &str,
    args: Vec<String>,
    custom: &BTreeMap<String, CustomAdapter>,
    project: &Path,
) -> Result<String> {
    query_listing(harness, args, custom, project).map(|listing| listing.text)
}

fn query_listing(
    harness: &str,
    args: Vec<String>,
    custom: &BTreeMap<String, CustomAdapter>,
    project: &Path,
) -> Result<Listing> {
    if custom
        .get(harness)
        .is_some_and(|a| a.command == crate::trust::BLOCKED_COMMAND)
    {
        anyhow::bail!("{harness} comes from an untrusted project config; run `model-router trust`");
    }
    let temp = tempfile::tempdir()?;
    let inv = Invocation {
        program: adapters::program(harness, custom),
        args,
        stdin: None,
        kind: OutputKind::Text,
        final_file: temp.path().join("unused"),
        env: vec![],
        files: vec![],
    };
    let shown = format!("{} {}", inv.program, inv.args.join(" "));
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
        "`{shown}` failed or timed out: {}",
        storage::clean_terminal(
            &process::read_tail(&temp.path().join("stderr.log"), 1500).unwrap_or_default()
        )
        .trim()
    );
    let listing = read_listing(&temp.path().join("stdout.log"), LISTING_LIMIT)?;
    Ok(Listing {
        text: storage::clean_terminal(&listing.text),
        truncated: listing.truncated,
    })
}

/// A short label for a failed listing, shown in the catalog note.
fn failure(error: &anyhow::Error) -> String {
    let text = storage::clean_terminal(&format!("{error:#}"));
    storage::truncate(text.lines().next().unwrap_or_default(), 300)
}

/// Router providers a harness can use, with their `/models` listings.
/// Listings run in parallel; a failure becomes a note, never an error.
fn provider_models(harness: &str, settings: &Settings) -> (Vec<Model>, Vec<String>) {
    if !crate::harness_settings::supports_provider(harness) || matches!(harness, "pi" | "omp") {
        return (vec![], vec![]);
    }
    let routed: Vec<(String, crate::providers::Provider)> = settings
        .providers
        .keys()
        .filter_map(|name| {
            crate::providers::route(harness, name, &settings.providers)
                .map(|p| (name.clone(), p.into_owned()))
        })
        .collect();
    let results: Vec<_> = std::thread::scope(|scope| {
        let jobs: Vec<_> = routed
            .iter()
            .map(|(name, provider)| {
                scope.spawn(move || {
                    (
                        name,
                        crate::providers::models_within(provider, Duration::from_secs(8)),
                    )
                })
            })
            .collect();
        jobs.into_iter().filter_map(|job| job.join().ok()).collect()
    });
    let mut models = vec![];
    let mut notes = vec![];
    for (name, result) in results {
        match result {
            Ok(list) => {
                for info in list {
                    let mut model = entry(
                        &info.id,
                        &format!("{name} /models"),
                        "provider listing; entitlement not checked",
                    );
                    model.provider = name.clone();
                    model.info = Some(info);
                    models.push(model);
                }
            }
            Err(error) => notes.push(format!(
                "Could not list provider {name}'s models ({}).",
                failure(&error)
            )),
        }
    }
    (models, notes)
}

/// Parses a successful listing with `parse`; a failed run or parse becomes
/// a visible note (`Err`) and the catalog falls back to manual entry.
fn listed(
    listing: Result<Listing>,
    command: &str,
    truncated: &mut bool,
    parse: impl FnOnce(&str) -> Result<String>,
) -> std::result::Result<String, String> {
    let fallback = |error: &anyhow::Error| {
        format!(
            "Could not read models from `{command}` ({}); enter a model ID manually.",
            failure(error)
        )
    };
    let listing = listing.map_err(|e| fallback(&e))?;
    *truncated = listing.truncated;
    parse(&listing.text).map_err(|e| fallback(&e))
}

pub fn discover(harness: &str, settings: &Settings, project: &Path) -> Result<Catalog> {
    let mut models = vec![entry("", "harness setting", "inherits configured default")];
    let mut efforts: Vec<String> = vec![];
    let mut effort_source = String::new();
    let mut truncated = false;

    let mut note: String = match harness {
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
        "pi" => listed(
            query_listing(harness, vec!["--list-models".into()], &settings.adapters, project),
            "pi --list-models",
            &mut truncated,
            |output| {
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
            Ok("Pi lists models from configured providers and extensions, with whether each supports thinking. Exact provider/model selections avoid fuzzy routing.".into())
        },
        )
        .inspect(|_| {
            efforts = strings(&PI_THINKING);
            effort_source = "Pi thinking levels".into();
        })
        .unwrap_or_else(|note| note),
        "omp" => {
            efforts = strings(&OMP_THINKING);
            effort_source = "omp thinking levels".into();
            listed(
                query_listing(
                    harness,
                    vec!["models".into(), "--json".into()],
                    &settings.adapters,
                    project,
                ),
                "omp models --json",
                &mut truncated,
                |output| {
                    models.extend(parse_omp_models(output)?);
                    Ok("omp lists chat models from its configured providers, with each model's thinking levels. The router passes provider/model as omp's --model selector.".into())
                },
            )
            .unwrap_or_else(|note| note)
        }
        "opencode" => listed(
            query_listing(harness, vec!["models".into()], &settings.adapters, project),
            "opencode models",
            &mut truncated,
            |output| {
            for line in output.lines().map(str::trim) {
                if !line.contains(char::is_whitespace) && line.contains('/') {
                    models.push(entry(
                        line,
                        "OpenCode provider catalog",
                        "catalog; entitlement not checked",
                    ));
                }
            }
            Ok("Provider catalog, not an account entitlement check. Model IDs include the provider prefix.".into())
        },
        )
        .unwrap_or_else(|note| note),
        "cursor" => listed(
            query_listing(harness, vec!["--list-models".into()], &settings.adapters, project),
            "--list-models",
            &mut truncated,
            |output| {
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
            Ok("Cursor-native account selections. If your CLI uses a different list format, enter its exact slug manually.".into())
        },
        )
        .unwrap_or_else(|note| note),
        name if settings.adapters.contains_key(name) => {
            let custom = &settings.adapters[name];
            const NOTE: &str =
                "Custom adapters can provide models_args returning one native model ID per line.";
            match &custom.models_args {
                _ if custom.command == crate::trust::BLOCKED_COMMAND => {
                    "This adapter comes from an untrusted project config, so its model list is not run; run `model-router trust` to use it.".into()
                }
                Some(args) => listed(
                    query_listing(name, args.clone(), &settings.adapters, project),
                    "models_args",
                    &mut truncated,
                    |output| {
                        for line in output.lines().map(str::trim).filter(|l| !l.is_empty()) {
                            models.push(entry(line, "custom catalog command", "adapter-reported"));
                        }
                        Ok(NOTE.into())
                    },
                )
                .unwrap_or_else(|note| note),
                None => NOTE.into(),
            }
        }
        "gemini" | "aider" | "goose" | "copilot" => {
            "Use your harness's configured default, a saved selection, or enter a native model ID. This adapter does not probe account entitlements.".into()
        }
        _ => anyhow::bail!("Unknown harness {harness}"),
    };
    if truncated {
        note.push_str(" Listing truncated: only its last 2 MiB were read.");
    }
    let (listed, notes) = provider_models(harness, settings);
    if !listed.is_empty() {
        note.push_str(" Router provider listings come from each provider's /models.");
    }
    models.extend(listed);
    for extra in notes {
        note.push(' ');
        note.push_str(&extra);
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

/// Chat models from `omp models --json`, with each model's thinking levels.
fn parse_omp_models(output: &str) -> Result<Vec<Model>> {
    let listed: serde_json::Value =
        serde_json::from_str(output).context("omp models --json was not JSON")?;
    Ok(listed["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|model| {
            model["kind"] == "chat" && model["id"].as_str().is_some_and(|id| !id.is_empty())
        })
        .map(|model| {
            let field = |key: &str| model[key].as_str().unwrap_or_default();
            let mut m = entry(
                field("id"),
                "omp model catalog",
                "configured provider; entitlement not checked",
            );
            m.provider = field("provider").into();
            m.efforts = model["thinking"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|level| level.as_str().map(str::to_owned))
                .collect();
            m
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn omp_catalog_keeps_chat_models_with_provider_and_thinking() {
        let output = r#"{"models":[
            {"provider":"cursor","kind":"chat","id":"claude-haiku-5-5","selector":"cursor/claude-haiku-5-5","thinking":["low","high"]},
            {"provider":"openai","kind":"embedding","id":"text-embed","thinking":[]},
            {"provider":"zai","kind":"chat","id":"glm","thinking":null}
        ]}"#;
        let models = parse_omp_models(output).unwrap();
        let ids: Vec<_> = models
            .iter()
            .map(|m| (m.provider.as_str(), m.id.as_str(), m.efforts.clone()))
            .collect();
        assert_eq!(
            ids,
            [
                (
                    "cursor",
                    "claude-haiku-5-5",
                    vec!["low".to_owned(), "high".to_owned()]
                ),
                ("zai", "glm", vec![])
            ]
        );
        assert!(parse_omp_models("not json").is_err());
    }

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

    #[test]
    fn truncated_listings_drop_the_partial_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout.log");
        fs::write(&path, "first-model\nsecond-model\nthird-model\n").unwrap();
        let whole = read_listing(&path, 1024).unwrap();
        assert!(!whole.truncated && whole.text.starts_with("first-model"));
        // The last 20 bytes start inside "second-model".
        let tail = read_listing(&path, 20).unwrap();
        assert!(tail.truncated);
        assert_eq!(tail.text, "third-model\n");
    }

    #[cfg(unix)]
    #[test]
    fn failed_listings_fall_back_to_manual_entry_with_a_visible_note() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        settings.workflows[0].stages[0].harness = "mine".into();
        settings.workflows[0].stages[0].model = "saved-model".into();
        settings.adapters.insert(
            "mine".into(),
            CustomAdapter {
                command: "false".into(),
                models_args: Some(vec![]),
                ..CustomAdapter::default()
            },
        );
        let catalog = discover("mine", &settings, dir.path()).unwrap();
        assert!(
            catalog.note.contains("Could not read models"),
            "{}",
            catalog.note
        );
        assert!(catalog.models.iter().any(|m| m.id == "saved-model"));
        assert!(catalog.models.iter().any(|m| m.id.is_empty()));
        // Untrusted adapters are never run.
        crate::trust::ignore(&mut settings);
        let blocked = discover("mine", &settings, dir.path()).unwrap();
        assert!(blocked.note.contains("untrusted"), "{}", blocked.note);
    }

    #[test]
    fn router_provider_listings_join_routable_harness_catalogs() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut bytes = [0; 4096];
            let count = socket.read(&mut bytes).unwrap();
            assert!(String::from_utf8_lossy(&bytes[..count]).starts_with("GET /v1/models"));
            let body = r#"{"data":[{"id":"vendor/big","context_length":131072,"pricing":{"prompt":"0.000001","completion":"0.000002"}}]}"#;
            let _ = write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        });
        let mut settings = Settings::default();
        settings.providers.insert(
            "local".into(),
            crate::providers::Provider {
                base_url: base,
                ..crate::providers::Provider::default()
            },
        );
        let (models, notes) = provider_models("aider", &settings);
        server.join().unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        let model = &models[0];
        assert_eq!(
            (model.provider.as_str(), model.id.as_str()),
            ("local", "vendor/big")
        );
        assert_eq!(model.info.as_ref().unwrap().context_length, Some(131_072));
        // Own-account harnesses and Pi never list router providers.
        for harness in ["claude", "pi"] {
            assert!(provider_models(harness, &settings).0.is_empty());
        }
    }
}
