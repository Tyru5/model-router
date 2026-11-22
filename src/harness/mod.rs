//! Declarative harness specs. Every built-in harness is one entry of
//! [`SPECS`] (see `builtins.rs`): how it is found, how the prompt, model,
//! effort, provider, permissions, and schema reach it, how its output is
//! parsed, and how a session is assigned and resumed. The adapter builder,
//! settings editor, catalog, doctor, and provider routing all read this
//! table, so adding a harness is one entry plus parser fixtures.
//!
//! The few genuinely custom mechanisms are named strategies implemented in
//! `adapters` (the escape hatch): [`ProviderPass::CodexConfig`],
//! [`Routing::CodexConfig`], and [`Routing::OpencodeJson`].
use crate::adapters::OutputKind;
use crate::config::Billing;
use serde::{Serialize, Serializer};

mod builtins;
pub use builtins::SPECS;

/// One value per execution mode.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Modes<T> {
    pub unattended: T,
    pub interactive: T,
}
impl<T: Copy> Modes<T> {
    pub const fn both(value: T) -> Self {
        Self {
            unattended: value,
            interactive: value,
        }
    }
    pub fn get(&self, interactive: bool) -> T {
        if interactive {
            self.interactive
        } else {
            self.unattended
        }
    }
}

/// One step of an argument template, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Part {
    /// Literal arguments; `{final_file}` expands to the attempt's final-message file.
    Args(&'static [&'static str]),
    /// The native structured-output flag, when [`HarnessSpec::structured`] applies.
    Schema,
    /// The model, per [`HarnessSpec::model`].
    Model,
    /// The provider, per [`HarnessSpec::provider`].
    Provider,
    /// The effort, per the template's [`EffortPass`].
    Effort,
    /// Permission-setting arguments, then the stage's extra arguments.
    Extra,
    /// The prompt, per the template's [`Prompt`].
    Prompt,
    /// Sets the qualified model at `path` in the JSON object held by `var`.
    ModelEnv {
        var: &'static str,
        path: &'static str,
    },
    /// The delegation bridge's MCP server, per [`HarnessSpec::mcp`], when
    /// the stage delegates; nothing otherwise.
    Mcp,
}

/// How a delegating stage's harness receives the router's MCP server for
/// one launch, without editing user or project config files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpInjection {
    /// `--mcp-config FILE` (Claude's JSON format) and the tools allowed.
    ClaudeConfigFile,
    /// `--config mcp_servers.model-router.*` keys.
    CodexConfig,
    /// An `mcp.model-router` entry in `OPENCODE_CONFIG_CONTENT`.
    OpencodeConfig,
}
impl McpInjection {
    pub fn flags(self) -> &'static [&'static str] {
        match self {
            Self::ClaudeConfigFile => &["--mcp-config", "--allowedTools"],
            Self::CodexConfig => &["--config"],
            Self::OpencodeConfig => &[],
        }
    }
}

/// How the prompt reaches the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Prompt {
    /// Standard input only.
    Stdin,
    /// An argument (e.g. `-`) that tells the harness to read standard input.
    StdinArg(&'static str),
    /// The prompt text as an argument.
    Arg,
    /// `FLAG prompt`.
    Flag(&'static str),
    /// `@PROMPT_FILE`.
    AtFile,
    /// `FLAG PROMPT_FILE`.
    MessageFile(&'static str),
    /// `READ PROMPT_FILE LOAD COMMANDS_FILE`; `commands` is written to
    /// `file` in the attempt directory at launch.
    ReadAndLoad {
        read: &'static str,
        load: &'static str,
        file: &'static str,
        commands: &'static str,
    },
}

/// How a model selection reaches the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPass {
    /// `FLAG model`.
    Flag(&'static str),
    /// `FLAG provider/model`; a routed provider uses its router ID where the
    /// routing defines one.
    Qualified(&'static str),
    /// `FLAG mode`: the harness selects modes, not model IDs (Amp).
    Mode(&'static str),
}
impl ModelPass {
    pub fn flag(self) -> &'static str {
        match self {
            Self::Flag(flag) | Self::Qualified(flag) | Self::Mode(flag) => flag,
        }
    }
}

/// How an effort level reaches the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffortPass {
    /// No portable effort option.
    None,
    /// `FLAG effort`.
    Flag(&'static str),
    /// `FLAG key="effort"` (Codex `--config`).
    ConfigKv {
        flag: &'static str,
        key: &'static str,
    },
    /// Appended to the model selector after this character (OpenCode `#variant`).
    ModelSuffix(char),
}

/// How a provider name that the router does not route reaches the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderPass {
    /// The harness has no provider choice.
    None,
    /// `FLAG provider`.
    Flag(&'static str),
    /// Part of the qualified model ([`ModelPass::Qualified`]).
    InModel,
    /// Codex config overrides and `--oss` for local providers (custom).
    CodexConfig,
}

/// How a router-defined provider (`[providers.NAME]`) is injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Routing {
    /// The harness uses only its own account: no provider field.
    OwnAccount,
    /// Provider names pass to the harness's own registry; router-only
    /// providers are rejected (no runtime injection).
    Registry,
    /// Codex `--config model_providers.ID.*` (custom).
    CodexConfig,
    /// An OpenCode provider entry merged into `OPENCODE_CONFIG_CONTENT` (custom).
    OpencodeJson,
    /// OpenAI-compatible environment variables.
    OpenAiEnv(OpenAiEnv),
}
impl Routing {
    /// Whether the router injects its providers into this harness.
    pub fn injects(self) -> bool {
        matches!(
            self,
            Self::CodexConfig | Self::OpencodeJson | Self::OpenAiEnv(_)
        )
    }
    /// Whether a routed provider gets an isolated `model-router-NAME` ID in
    /// the qualified model.
    pub fn router_id_in_model(self) -> bool {
        matches!(self, Self::OpencodeJson)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OpenAiEnv {
    /// Inherited variables with these prefixes are removed, so settings meant
    /// for OpenAI never reach the routed endpoint.
    pub strip_prefixes: &'static [&'static str],
    pub base: BaseEnv,
    /// Receives the key (or `none` for keyless providers).
    pub key_var: &'static str,
    /// Routed models are passed as `PREFIX model` (LiteLLM's `openai/`).
    pub model_prefix: Option<&'static str>,
    /// Routed base URL also passed as `FLAG url` after the model.
    pub base_url_flag: Option<&'static str>,
    /// Provider flag value when routed (Goose's `openai`).
    pub provider_value: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseEnv {
    /// The whole base URL in one variable.
    Url(&'static str),
    /// Origin in `host`, path plus `suffix` in `path`.
    HostPath {
        host: &'static str,
        path: &'static str,
        suffix: &'static str,
    },
}

/// How the router learns the harness session for takeover and resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Session {
    None,
    /// The harness reports its own session in its JSON events.
    Reported,
    /// The router picks an ID and passes `FLAG id`, unless the arguments
    /// already contain one of `conflicts` (they pick, continue, or disable a
    /// session).
    Assign {
        flag: &'static str,
        prepend: bool,
        conflicts: &'static [&'static str],
    },
}

/// The interactive command that reopens a session: an interactive build
/// without its prompt, wrapped. `{session}` expands to the session ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Resume {
    pub prepend: &'static [&'static str],
    pub append: &'static [&'static str],
    /// Replaces all arguments instead (the TUI keeps the session's options).
    pub replace: Option<&'static [&'static str]>,
    /// False drops the stage effort (the session keeps its own).
    pub keep_effort: bool,
}

/// The unattended command that sends one more prompt to an existing session
/// (`workflow.handoff_repair`). It lists only what the harness accepts when
/// it resumes: a resume subcommand can take fewer flags than a new session.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct FollowUp {
    /// The whole template; `{session}` and `{final_file}` expand in `Args`.
    pub args: &'static [Part],
    pub prompt: Prompt,
    pub extra: FollowUpExtra,
}

/// What `Part::Extra` passes in a follow-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowUpExtra {
    /// Stage settings and extra arguments, as for a new session.
    Same,
    /// Only what can be written as `FLAG key=value`: stage settings that
    /// have a config key, and the stage's own `--config`/`-c` extra
    /// arguments. Everything else is left out, because the resume
    /// subcommand rejects it (`codex exec resume` has no `--sandbox`).
    ConfigOnly(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Structured {
    pub flag: &'static str,
    /// First version whose unattended mode takes the flag.
    pub since: [u64; 3],
    pub delivery: SchemaDelivery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaDelivery {
    /// The compact schema as the flag's value.
    Inline,
    /// The strict schema in this attempt file, written at launch.
    File(&'static str),
}

/// Where the catalog reads model selections (functions in `catalog`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSource {
    /// Codex's `models_cache.json`.
    CodexCache,
    /// `claude --help`, falling back to these documented aliases.
    ClaudeHelp { aliases: &'static [&'static str] },
    /// A fixed list of modes.
    Modes(&'static [&'static str]),
    /// `pi --list-models`.
    PiList,
    /// `omp models --json`.
    OmpJson,
    /// `opencode models`.
    OpencodeModels,
    /// `cursor-agent --list-models`.
    CursorList,
    /// Manual entry only.
    Manual,
}

/// An alternative program name that other tools also use: accepted only
/// when its resolved path contains `path_contains` or its `--help` contains
/// `help_contains`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Identity {
    pub program: &'static str,
    pub path_contains: &'static str,
    pub help_contains: &'static str,
}

/// Arguments for harness versions older than `below_major` (by `--version`;
/// unknown versions count as current).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct VersionOverride {
    pub below_major: u64,
    pub args: Modes<&'static [Part]>,
    pub prompt: Modes<Prompt>,
    pub effort: EffortPass,
}

/// A per-stage native setting (permissions, sandbox).
#[derive(Debug, Serialize)]
pub struct Setting {
    pub key: &'static str,
    pub label: &'static str,
    pub choices: &'static [&'static str],
    pub help: &'static str,
    pub(crate) flag: &'static str,
    pub(crate) config_key: Option<&'static str>,
    pub(crate) conflicts: &'static [&'static str],
    /// Choices that loosen the harness's own approvals or sandbox. A project
    /// config needs trust to set them (see `trust`).
    pub(crate) escalating: &'static [&'static str],
    pub(crate) emit: Emit,
}

/// How a chosen setting value reaches the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Emit {
    /// `flag value`, or `--config key="value"` for Codex config keys.
    Value,
    /// The flag alone; the setting has one non-default choice.
    Switch,
    /// An environment variable named by `flag`.
    Env,
}

#[derive(Debug, Serialize)]
pub struct HarnessSpec {
    pub name: &'static str,
    pub display: &'static str,
    /// Executable names in order of preference; the first is the fallback.
    pub programs: &'static [&'static str],
    pub identity: Option<Identity>,
    #[serde(serialize_with = "output_name")]
    pub output: OutputKind,
    pub args: Modes<&'static [Part]>,
    pub prompt: Modes<Prompt>,
    pub model: ModelPass,
    pub effort: EffortPass,
    /// Documented effort levels; empty when they depend on the model.
    pub efforts: &'static [&'static str],
    /// What the harness calls an effort level, for messages.
    pub effort_name: &'static str,
    /// Editor help for the effort field; empty uses a generic text.
    pub effort_help: &'static str,
    pub provider: ProviderPass,
    pub routing: Routing,
    /// Shown after the harness in the provider note, e.g. API requirements.
    pub routing_note: &'static str,
    pub session: Session,
    pub resume: Option<Resume>,
    /// The unattended follow-up on a session; None when the harness has no
    /// such command.
    pub follow_up: Option<FollowUp>,
    /// False when unattended output arrives as one document at exit, so a
    /// quiet stage cannot be told from a stalled one
    /// (`stall_timeout_seconds` is rejected).
    pub streams: bool,
    /// How the harness's own account is usually paid for, when the stage is
    /// not routed to a router provider and sets no `billing`
    /// (`cost::stage_billing`).
    pub billing: Billing,
    pub structured: Option<Structured>,
    /// How a delegating stage hands it the router's MCP server; None when
    /// it cannot delegate.
    pub mcp: Option<McpInjection>,
    pub settings: &'static [Setting],
    /// The `(setting key, choice)` that keeps the harness from changing
    /// files; None when it has no such setting.
    pub read_only: Option<(&'static str, &'static str)>,
    pub catalog: CatalogSource,
    pub version_override: Option<VersionOverride>,
    /// Home-relative state paths the router sandbox leaves writable, beyond
    /// `.NAME`, `.config/NAME`, `.local/share/NAME`, and the caches.
    pub state_dirs: &'static [&'static str],
    pub notes: &'static str,
}

fn output_name<S: Serializer>(kind: &OutputKind, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(kind.name())
}

impl HarnessSpec {
    /// Whether a stage can set a provider.
    pub fn supports_provider(&self) -> bool {
        self.routing != Routing::OwnAccount
    }
    pub fn supports_effort(&self) -> bool {
        self.effort != EffortPass::None
    }
    /// Template, prompt delivery, and effort pass for a mode, honouring a
    /// version override when `legacy`.
    pub fn template(
        &self,
        interactive: bool,
        legacy: bool,
    ) -> (&'static [Part], Prompt, EffortPass) {
        match (&self.version_override, legacy) {
            (Some(o), true) => (o.args.get(interactive), o.prompt.get(interactive), o.effort),
            _ => (
                self.args.get(interactive),
                self.prompt.get(interactive),
                self.effort,
            ),
        }
    }
    /// Flags the spec passes with the current (non-override) templates:
    /// what doctor must find in the harness's help. Settings that are
    /// environment variables are not flags.
    pub fn declared_flags(&self) -> Vec<&'static str> {
        let mut flags = vec![self.model.flag()];
        match self.effort {
            EffortPass::Flag(flag) | EffortPass::ConfigKv { flag, .. } => flags.push(flag),
            EffortPass::None | EffortPass::ModelSuffix(_) => {}
        }
        if let ProviderPass::Flag(flag) = self.provider {
            flags.push(flag);
        }
        if let Routing::OpenAiEnv(OpenAiEnv {
            base_url_flag: Some(flag),
            ..
        }) = self.routing
        {
            flags.push(flag);
        }
        if let Some(structured) = self.structured {
            flags.push(structured.flag);
        }
        if let Some(mcp) = self.mcp {
            flags.extend(mcp.flags());
        }
        if let Session::Assign { flag, .. } = self.session {
            flags.push(flag);
        }
        if let Some(resume) = self.resume {
            let items = resume.prepend.iter().chain(resume.append);
            flags.extend(items.chain(resume.replace.unwrap_or(&[])).copied());
        }
        for interactive in [false, true] {
            match self.prompt.get(interactive) {
                Prompt::Flag(flag) | Prompt::MessageFile(flag) => flags.push(flag),
                Prompt::ReadAndLoad { read, load, .. } => flags.extend([read, load]),
                Prompt::Stdin | Prompt::StdinArg(_) | Prompt::Arg | Prompt::AtFile => {}
            }
            for part in self.args.get(interactive) {
                if let Part::Args(items) = part {
                    flags.extend(items.iter().copied());
                }
            }
        }
        flags.extend(
            self.settings
                .iter()
                .filter(|s| s.emit != Emit::Env)
                .map(|s| s.flag),
        );
        flags.retain(|f| f.starts_with('-') && f.len() > 1);
        flags.sort_unstable();
        flags.dedup();
        flags
    }
    /// Whether the mode's current template takes an effort level.
    pub fn accepts_effort(&self, interactive: bool) -> bool {
        let (parts, _, effort) = self.template(interactive, false);
        match effort {
            EffortPass::None => false,
            EffortPass::ModelSuffix(_) => parts.contains(&Part::Model),
            _ => parts.contains(&Part::Effort),
        }
    }
}

/// The built-in harness named `name`.
pub fn get(name: &str) -> Option<&'static HarnessSpec> {
    SPECS.iter().find(|spec| spec.name == name)
}

/// Built-in harness names, in table order.
pub const NAMES: [&str; SPECS.len()] = names();

const fn names() -> [&'static str; SPECS.len()] {
    let mut out = [""; SPECS.len()];
    let mut index = 0;
    while index < SPECS.len() {
        out[index] = SPECS[index].name;
        index += 1;
    }
    out
}

/// Provider routing summary for `model-router providers`, from the table.
pub fn provider_note() -> String {
    let routed: Vec<String> = SPECS
        .iter()
        .filter(|s| s.routing.injects())
        .map(|s| {
            if s.routing_note.is_empty() {
                s.name.to_owned()
            } else {
                format!("{} ({})", s.name, s.routing_note)
            }
        })
        .collect();
    let display = |routing: fn(&Routing) -> bool| -> Vec<&str> {
        SPECS
            .iter()
            .filter(|s| routing(&s.routing))
            .map(|s| s.display)
            .collect()
    };
    let registry = display(|r| *r == Routing::Registry);
    let own = display(|r| *r == Routing::OwnAccount);
    format!(
        "Built-in names are presets: add an empty [providers.NAME] to route one into {}, or custom adapters; without it the name passes to the harness unchanged, so its own provider definition wins. Add other providers with [providers.NAME] base_url/api_key_env. {} always use their own registries. {} use only their own accounts.",
        routed.join(", "),
        and_list(&registry),
        and_list(&own),
    )
}

/// `a`, `a and b`, `a, b, and c`.
fn and_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).into(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_consistent() {
        let mut seen = std::collections::HashSet::new();
        for spec in SPECS {
            assert!(seen.insert(spec.name), "duplicate {}", spec.name);
            assert!(!spec.programs.is_empty(), "{}", spec.name);
            for interactive in [false, true] {
                let parts = spec.args.get(interactive);
                assert!(
                    parts.contains(&Part::Prompt),
                    "{} needs a prompt",
                    spec.name
                );
                assert!(
                    parts.contains(&Part::Extra),
                    "{} needs extra args",
                    spec.name
                );
            }
            // A provider choice needs a way to pass it.
            assert_eq!(
                spec.supports_provider(),
                spec.provider != ProviderPass::None,
                "{}",
                spec.name
            );
            assert_eq!(
                spec.provider == ProviderPass::InModel,
                matches!(spec.model, ModelPass::Qualified(_)),
                "{}",
                spec.name
            );
            if let Some(structured) = spec.structured {
                assert!(
                    spec.args.unattended.contains(&Part::Schema),
                    "{}",
                    spec.name
                );
                assert!(structured.flag.starts_with("--"));
            }
            if spec.resume.is_some() {
                assert_ne!(spec.session, Session::None, "{}", spec.name);
            }
            if let Some(follow_up) = spec.follow_up {
                assert!(spec.resume.is_some(), "{}", spec.name);
                assert!(follow_up.args.contains(&Part::Prompt), "{}", spec.name);
                assert!(follow_up.args.contains(&Part::Extra), "{}", spec.name);
                let names_session = follow_up
                    .args
                    .iter()
                    .any(|part| matches!(part, Part::Args(items) if items.contains(&"{session}")));
                assert!(names_session, "{} follow-up needs {{session}}", spec.name);
            }
            for setting in spec.settings {
                assert!(!setting.choices.is_empty(), "{}", setting.key);
            }
            if let Some((key, choice)) = spec.read_only {
                let setting = spec.settings.iter().find(|s| s.key == key);
                assert!(
                    setting.is_some_and(|s| s.choices.contains(&choice)),
                    "{} read_only names no setting choice",
                    spec.name
                );
            }
        }
        assert_eq!(NAMES.len(), SPECS.len());
        assert_eq!(get("codex").map(|s| s.display), Some("Codex"));
        assert!(get("nope").is_none());
    }

    #[test]
    fn provider_note_lists_routing_from_the_table() {
        let note = provider_note();
        assert!(
            note.contains(
                "codex (needs Responses API support), opencode, aider, goose, or custom adapters"
            ),
            "{note}"
        );
        assert!(
            note.contains("Pi and oh-my-pi always use their own registries"),
            "{note}"
        );
        assert!(
            note.contains(
                "Claude Code, Cursor, Gemini, Amp, and Copilot use only their own accounts"
            ),
            "{note}"
        );
    }
}
