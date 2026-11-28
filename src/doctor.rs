//! Offline adapter compatibility checks. Only `--version` and `--help` run, so
//! no model request is sent and authentication is never checked.
use crate::{
    adapters::{self, ContextPaths, Invocation, OutputKind},
    catalog,
    config::{Mode, Settings, Stage},
    process, providers, storage,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{OnceLock, atomic::AtomicBool},
    thread,
    time::Duration,
};

pub const AUTH_NOTE: &str =
    "Authentication is not checked: that needs a model request. Log in through each harness.";
const MANIFEST: &str = include_str!("../tests/fixtures/harness-output/manifest.toml");
/// Where the TUI's live harness tests save output under a project. The test
/// script writes a run's `manifest.toml` only after the run passes.
pub const LIVE_TESTS: &str = ".model-router/harness-tests";
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// Probes run in parallel, so this also bounds `inspect_all`.
const HELP_TIMEOUT: Duration = Duration::from_secs(10);
const SAMPLE_PROMPT: &str = "Sample prompt.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Supported,
    Missing,
    /// Help output was unavailable.
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct Capability {
    /// A flag the router passes, as it appears in the argument vector.
    pub flag: String,
    pub modes: Vec<Mode>,
    /// The help command checked, e.g. `codex exec --help`.
    pub help: String,
    pub support: Support,
}

/// Offline auth-readiness signal for an installed harness. Checks only
/// whether well-known env vars are non-empty or config/auth files exist —
/// never reads secret contents or makes network requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthStatus {
    Ready,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthHint {
    pub status: AuthStatus,
    /// Names of detected env vars or `~/...` config paths (never secret values).
    pub signals: Vec<String>,
    /// Short guidance when no local auth signal was detected.
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKeyStatus {
    Set,
    Missing,
    NotRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderCheck {
    pub name: String,
    pub api_key_env: String,
    pub status: ProviderKeyStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub harness: String,
    pub builtin: bool,
    pub program: String,
    pub path: Option<PathBuf>,
    /// First meaningful line of `<program> --version`.
    pub version: Option<String>,
    /// Harness versions covered by parser fixtures.
    pub tested_versions: Vec<String>,
    /// Whether `version` has parser fixtures; None when no version was detected.
    pub version_tested: Option<bool>,
    /// Whether a fixture for `version` is captured output, not schema-derived.
    pub version_captured: bool,
    /// Configured pin from `[harness_versions]`, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_pin: Option<String>,
    /// Whether the detected version satisfies `version_pin`; None without a pin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_pin_ok: Option<bool>,
    /// Offline auth-readiness hint for installed built-in harnesses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_hint: Option<AuthHint>,
    /// Derived from the adapter's own invocations. None for custom adapters,
    /// which declare no flag contract.
    pub capabilities: Option<Vec<Capability>>,
    /// `ok`, `missing: ...`, `unknown: ...`, `not checked`, or `not declared`.
    pub summary: String,
    pub notes: Vec<String>,
}
impl Report {
    pub fn installed(&self) -> bool {
        self.path.is_some()
    }
    pub fn missing_flags(&self) -> bool {
        self.capabilities
            .iter()
            .flatten()
            .any(|c| c.support == Support::Missing)
    }
    pub fn version_pin_mismatch(&self) -> bool {
        self.version_pin_ok == Some(false)
    }
    pub fn version_label(&self) -> String {
        match &self.version {
            _ if !self.builtin => "not declared".into(),
            Some(line) => version_tokens(line).next().unwrap_or(line).into(),
            None if self.installed() => "unknown".into(),
            None => "-".into(),
        }
    }
    /// `tested` only for captured fixtures; fixtures built from schemas and
    /// documentation say `schema-derived`.
    pub fn tested_label(&self) -> &'static str {
        match self.version_tested {
            Some(true) if self.version_captured => "tested",
            Some(true) => "schema-derived",
            Some(false) => "untested",
            None => "-",
        }
    }
}

/// Checks every built-in and custom harness in parallel threads.
pub fn inspect_all(settings: &Settings, project: &Path) -> Vec<Report> {
    let names = catalog::known_harnesses(settings);
    let live = live_fixtures(project);
    thread::scope(|scope| {
        let live = &live;
        let checks: Vec<_> = names
            .iter()
            .map(|name| scope.spawn(move || inspect_with(name, settings, project, live)))
            .collect();
        checks
            .into_iter()
            .map(|check| check.join().expect("Harness check panicked"))
            .collect()
    })
}

pub fn inspect(harness: &str, settings: &Settings, project: &Path) -> Report {
    let live = live_fixtures(project);
    inspect_with(harness, settings, project, &live)
}

fn inspect_with(harness: &str, settings: &Settings, project: &Path, live: &[Fixture]) -> Report {
    let program = adapters::program(harness, &settings.adapters);
    let mut report = Report {
        harness: harness.into(),
        builtin: crate::harness::get(harness).is_some(),
        path: adapters::executable_at(&program, project),
        program,
        version: None,
        tested_versions: vec![],
        version_tested: None,
        version_captured: false,
        version_pin: settings.harness_versions.get(harness).cloned(),
        version_pin_ok: None,
        auth_hint: None,
        capabilities: None,
        summary: "not declared".into(),
        notes: vec![AUTH_NOTE.into()],
    };
    if report.builtin {
        check_with(&mut report, project, live);
        if report.installed() {
            report.auth_hint = Some(auth_hint_for(harness));
        }
    } else if settings.adapters.contains_key(harness) {
        report
            .notes
            .push("Custom adapter: no version or flag contract, so its command is not run.".into());
    } else {
        report.notes.push(format!("Unknown harness {harness}"));
    }
    evaluate_version_pin(&mut report);
    report
}

fn check_with(report: &mut Report, project: &Path, live: &[Fixture]) {
    let fixtures: Vec<_> = manifest()
        .iter()
        .chain(live)
        .filter(|f| f.harness == report.harness)
        .cloned()
        .collect();
    record_fixtures(report, &fixtures);
    if report.installed() {
        let version = probe(
            &report.program,
            &["--version".into()],
            project,
            VERSION_TIMEOUT,
        );
        match version {
            Ok((true, text)) => report.version = first_line(&text),
            Ok((false, _)) => report
                .notes
                .push(format!("{} --version failed.", report.program)),
            Err(error) => report
                .notes
                .push(format!("{} --version: {error:#}", report.program)),
        }
        adapters::remember_version(&report.program, report.version.as_deref());
    } else {
        report
            .notes
            .push(format!("{} not found on PATH.", report.program));
    }
    let uses = flag_uses(&report.harness).unwrap_or_else(|error| {
        report
            .notes
            .push(format!("Sample invocation failed: {error:#}"));
        vec![]
    });
    let help_args: BTreeSet<&Vec<String>> = uses.iter().map(|u| &u.help).collect();
    let mut helps = BTreeMap::new();
    if report.installed() && !help_args.is_empty() {
        let program = report.program.as_str();
        let outputs: Vec<_> = thread::scope(|scope| {
            let outputs: Vec<_> = help_args
                .iter()
                .map(|args| {
                    let output = scope.spawn(move || probe(program, args, project, HELP_TIMEOUT));
                    (*args, output)
                })
                .collect();
            outputs
                .into_iter()
                .map(|(args, output)| (args, joined(output)))
                .collect()
        });
        for (args, output) in outputs {
            let command = format!("{program} {}", args.join(" "));
            let flags = match output.map(|(_, text)| help_flags(&text)) {
                Ok(flags) if !flags.is_empty() => Some(flags),
                Ok(_) => {
                    report.notes.push(format!("{command} listed no flags."));
                    None
                }
                Err(error) => {
                    report.notes.push(format!("{command}: {error:#}"));
                    None
                }
            };
            helps.insert(args.clone(), flags);
        }
    }
    match_version(report, &fixtures);
    let capabilities: Vec<_> = uses
        .into_iter()
        .map(|u| {
            let support = match helps.get(&u.help) {
                Some(Some(flags)) if flags.contains(&u.flag) => Support::Supported,
                Some(Some(_)) => Support::Missing,
                _ => Support::Unknown,
            };
            Capability {
                help: format!("{} {}", report.program, u.help.join(" ")),
                flag: u.flag,
                modes: u.modes,
                support,
            }
        })
        .collect();
    report.summary = summary(&capabilities, report.installed());
    if capabilities.iter().any(|c| c.support == Support::Missing) {
        report.notes.push(
            "A missing flag is absent from current help: the harness removed or hid it.".into(),
        );
    }
    report.capabilities = Some(capabilities);
}

fn summary(capabilities: &[Capability], installed: bool) -> String {
    if !installed {
        return "not checked".into();
    }
    let unknown = capabilities
        .iter()
        .filter(|c| c.support == Support::Unknown)
        .count();
    let mut missing: Vec<(&str, Vec<Mode>)> = vec![];
    for capability in capabilities
        .iter()
        .filter(|c| c.support == Support::Missing)
    {
        match missing
            .iter_mut()
            .find(|(flag, _)| *flag == capability.flag)
        {
            Some((_, modes)) => modes.extend(&capability.modes),
            None => missing.push((&capability.flag, capability.modes.clone())),
        }
    }
    if missing.is_empty() {
        return match unknown {
            0 if capabilities.is_empty() => "unknown: no sample invocation".into(),
            0 => "ok".into(),
            _ => "unknown: help unavailable".into(),
        };
    }
    let flags: Vec<String> = missing
        .into_iter()
        .map(|(flag, modes)| {
            if [Mode::Unattended, Mode::Interactive]
                .iter()
                .all(|mode| modes.contains(mode))
            {
                flag.into()
            } else {
                format!("{flag} ({})", modes[0].label())
            }
        })
        .collect();
    let mut summary = format!("missing: {}", flags.join(", "));
    if unknown > 0 {
        summary.push_str(&format!("; {unknown} unknown"));
    }
    summary
}

/// A flag the adapter passes and the help command that should list it.
struct FlagUse {
    help: Vec<String>,
    flag: String,
    modes: Vec<Mode>,
}

/// A router-defined provider for samples of harnesses that route them, so
/// routed-only flags are probed too. Never contacted.
const SAMPLE_ROUTED: &str = "model-router-sample";

/// Builds sample invocations from the harness spec with the adapter itself,
/// so probed flags follow the spec and cannot drift from what runs.
fn flag_uses(harness: &str) -> Result<Vec<FlagUse>> {
    let temp = tempfile::tempdir()?;
    let prompt_file = temp.path().join("prompt.md");
    // A delegating launch, so the delegation flags are probed too.
    let mcp = adapters::McpLaunch {
        program: temp.path().join("mr"),
        sock: temp.path().join("orch.sock"),
        token_file: temp.path().join("orch.token"),
    };
    let paths = ContextPaths {
        workspace: temp.path(),
        attempt_dir: temp.path(),
        prompt_file: &prompt_file,
        handoff_file: &prompt_file,
        mcp: Some(&mcp),
    };
    let mut uses: Vec<FlagUse> = vec![];
    let routes = crate::harness::get(harness).is_some_and(|spec| spec.routing.injects());
    let configured = BTreeMap::from([(
        SAMPLE_ROUTED.to_owned(),
        crate::providers::Provider {
            base_url: "http://127.0.0.1:9/v1".into(),
            ..Default::default()
        },
    )]);
    // A hosted and a local provider reach both native provider code paths;
    // a router provider reaches the routed one.
    let providers: &[&str] = if routes {
        &["openai", "ollama", SAMPLE_ROUTED]
    } else {
        &["openai", "ollama"]
    };
    for mode in [Mode::Unattended, Mode::Interactive] {
        for provider in providers {
            let stage = sample(harness, mode, provider);
            let none = BTreeMap::new();
            let mut invocation =
                adapters::build(&stage, &none, &configured, SAMPLE_PROMPT, &paths)?;
            adapters::assign_session(&mut invocation, harness);
            let mut invocations = vec![invocation];
            // Takeover reopens an unattended session in the interactive CLI.
            if mode == Mode::Interactive && adapters::resumable(harness, &none) {
                invocations.push(adapters::resume(
                    &stage,
                    &none,
                    &configured,
                    "sample-session",
                    &paths,
                )?);
            }
            for invocation in invocations {
                let help: Vec<String> = subcommand(&invocation.args)
                    .into_iter()
                    .chain(["--help"])
                    .map(String::from)
                    .collect();
                for flag in invocation.args.iter().filter_map(|arg| flag_name(arg)) {
                    match uses.iter_mut().find(|u| u.help == help && u.flag == flag) {
                        Some(found) if !found.modes.contains(&mode) => found.modes.push(mode),
                        Some(_) => {}
                        None => uses.push(FlagUse {
                            help: help.clone(),
                            flag: flag.into(),
                            modes: vec![mode],
                        }),
                    }
                }
            }
        }
    }
    Ok(uses)
}

/// A stage that sets everything the spec says the mode accepts.
fn sample(harness: &str, mode: Mode, provider: &str) -> Stage {
    let spec = crate::harness::get(harness);
    let supported = |yes: bool, value: &str| if yes { value.into() } else { String::new() };
    // Interactive OpenCode 2.x has no variant option.
    let effort = spec.is_some_and(|s| s.accepts_effort(mode == Mode::Interactive));
    Stage {
        harness: harness.into(),
        mode,
        model: "sample-model".into(),
        provider: supported(spec.is_some_and(|s| s.supports_provider()), provider),
        effort: supported(effort, "high"),
        harness_settings: spec
            .map_or(&[][..], |s| s.settings)
            .iter()
            .map(|s| (s.key.into(), s.choices[0].into()))
            .collect(),
        ..Stage::default()
    }
}

/// A leading bare word, as in `codex exec` or `opencode run`.
fn subcommand(args: &[String]) -> Option<&str> {
    args.first().map(String::as_str).filter(|a| {
        a.starts_with(|c: char| c.is_ascii_lowercase())
            && a.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
    })
}

fn flag_name(arg: &str) -> Option<&str> {
    (arg.starts_with('-') && arg.len() > 1).then(|| arg.split('=').next().unwrap_or(arg))
}

/// Flags defined in help output: option tokens on lines that start with `-`,
/// before the description column. Short and long aliases both count, so prose
/// that merely mentions a removed flag does not.
fn help_flags(text: &str) -> BTreeSet<String> {
    let mut flags = BTreeSet::new();
    for line in text.lines().map(str::trim_start) {
        if !line.starts_with('-') {
            continue;
        }
        let head = line.split("  ").next().unwrap_or(line);
        for token in head.split(|c: char| c.is_whitespace() || matches!(c, ',' | '|' | '/')) {
            if let Some(name) = token.strip_prefix("--[no-]") {
                flags.insert(format!("--{name}"));
                flags.insert(format!("--no-{name}"));
                continue;
            }
            let name: String = token
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                .collect();
            let bare = name.trim_start_matches('-');
            if !bare.is_empty() && (1..=2).contains(&(name.len() - bare.len())) {
                flags.insert(name);
            }
        }
    }
    flags
}

/// Runs `<program> <args>` without a shell. Returns success and combined output.
fn probe(program: &str, args: &[String], cwd: &Path, timeout: Duration) -> Result<(bool, String)> {
    let temp = tempfile::tempdir()?;
    let invocation = Invocation {
        program: program.into(),
        args: args.to_vec(),
        stdin: None,
        kind: OutputKind::Text,
        final_file: temp.path().join("unused"),
        env: vec![],
        files: vec![],
        assign: None,
    };
    let outcome = process::capture(
        &invocation,
        cwd,
        temp.path(),
        timeout,
        &AtomicBool::new(false),
        |_, _| {},
    )?;
    let mut text = String::new();
    for log in ["stdout.log", "stderr.log"] {
        text.push_str(&process::read_tail(&temp.path().join(log), 512 * 1024)?);
        text.push('\n');
    }
    if outcome.timed_out {
        return Err(anyhow!("timed out after {}s", timeout.as_secs()));
    }
    Ok((outcome.success(), storage::clean_terminal(&text)))
}

fn joined<T>(handle: thread::ScopedJoinHandle<'_, Result<T>>) -> Result<T> {
    handle
        .join()
        .unwrap_or_else(|_| Err(anyhow!("check panicked")))
}

fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(120).collect())
}

/// Version-like tokens: `opencode v2.0.22` yields `2.0.22`.
fn version_tokens(line: &str) -> impl Iterator<Item = &str> {
    // `omp/18.6.3` separates its name with a slash.
    line.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | ';' | '/'))
        .map(|token| token.strip_prefix('v').unwrap_or(token))
        .filter(|token| token.starts_with(|c: char| c.is_ascii_digit()))
}

#[derive(Deserialize)]
struct Manifest {
    fixture: Vec<Fixture>,
}
#[derive(Clone, Deserialize)]
struct Fixture {
    harness: String,
    version: String,
    provenance: String,
    /// From a live test in the project, not the built-in manifest.
    #[serde(skip)]
    live: bool,
}

fn manifest() -> &'static [Fixture] {
    static FIXTURES: OnceLock<Vec<Fixture>> = OnceLock::new();
    FIXTURES
        .get_or_init(|| {
            toml::from_str::<Manifest>(MANIFEST)
                .map(|m| m.fixture)
                .unwrap_or_default()
        })
        .as_slice()
}

/// Lists the versions `fixtures` cover and counts the built-in ones.
fn record_fixtures(report: &mut Report, fixtures: &[Fixture]) {
    for fixture in fixtures {
        if !report.tested_versions.contains(&fixture.version) {
            report.tested_versions.push(fixture.version.clone());
        }
    }
    let built_in: Vec<_> = fixtures.iter().filter(|f| !f.live).collect();
    let captured = built_in
        .iter()
        .filter(|f| f.provenance == "captured")
        .count();
    report.notes.push(format!(
        "Parser fixtures: {captured} captured, {} schema-derived.",
        built_in.len() - captured
    ));
}

/// Whether `fixtures` cover the detected version, and with captured output;
/// a passing live test counts as captured.
fn match_version(report: &mut Report, fixtures: &[Fixture]) {
    let matches =
        |fixture: &Fixture, line: &str| version_tokens(line).any(|token| token == fixture.version);
    report.version_tested = report
        .version
        .as_deref()
        .map(|line| fixtures.iter().any(|f| matches(f, line)));
    report.version_captured = report.version.as_deref().is_some_and(|line| {
        fixtures
            .iter()
            .any(|f| f.provenance == "captured" && matches(f, line))
    });
    let live = report
        .version
        .as_deref()
        .is_some_and(|line| fixtures.iter().any(|f| f.live && matches(f, line)));
    if live {
        report.notes.push(format!(
            "A live test of this version passed in this project ({LIVE_TESTS})."
        ));
    }
    if report.version_tested == Some(true) && !report.version_captured {
        report
            .notes
            .push("Fixtures for this version are schema-derived, not captured output.".into());
    }
    if report.version_tested == Some(false) {
        report.notes.push(format!(
            "Detected version is not a parser-tested version ({}).",
            report.tested_versions.join(", ")
        ));
    }
}

/// Entries of the passing live tests saved under `project`; unreadable or
/// malformed manifests are skipped.
fn live_fixtures(project: &Path) -> Vec<Fixture> {
    let Ok(runs) = std::fs::read_dir(project.join(LIVE_TESTS)) else {
        return vec![];
    };
    runs.flatten()
        .filter_map(|run| std::fs::read_to_string(run.path().join("manifest.toml")).ok())
        .filter_map(|text| toml::from_str::<Manifest>(&text).ok())
        .flat_map(|manifest| manifest.fixture)
        .map(|fixture| Fixture {
            live: true,
            ..fixture
        })
        .collect()
}

/// Whether a router sandbox backend works here; see `sandbox`.
pub fn sandbox_line() -> String {
    crate::sandbox::doctor_line()
}

/// External tools the router itself runs, and which features need them.
pub const TOOLS: [(&str, &str); 2] = [
    ("git", "worktrees, snapshots, diffs, ship"),
    ("gh", "GitHub task imports, ship --draft-pr"),
];

/// One tool the router runs: where it is and its version, or what cannot
/// work without it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tool {
    pub program: &'static str,
    pub needed_for: &'static str,
    pub path: Option<PathBuf>,
    pub version: Option<String>,
}

/// `git` and `gh` on this host, found as harness executables are.
pub fn inspect_tools(project: &Path) -> Vec<Tool> {
    thread::scope(|scope| {
        let checks: Vec<_> = TOOLS
            .iter()
            .map(|&(program, needed_for)| {
                scope.spawn(move || {
                    let path = adapters::executable_at(program, project);
                    let version = path.as_ref().and_then(|_| {
                        let (ok, text) =
                            probe(program, &["--version".into()], project, VERSION_TIMEOUT).ok()?;
                        let line = first_line(&text)?;
                        let version = version_tokens(&line).next().map(str::to_owned);
                        ok.then(|| version.unwrap_or(line))
                    });
                    Tool {
                        program,
                        needed_for,
                        path,
                        version,
                    }
                })
            })
            .collect();
        checks
            .into_iter()
            .map(|check| check.join().expect("Tool check panicked"))
            .collect()
    })
}

/// One line for `doctor`: `Tools: git 2.43.0 at /usr/bin/git; gh missing (...)`.
pub fn tools_line(tools: &[Tool]) -> String {
    let parts: Vec<String> = tools
        .iter()
        .map(|tool| match &tool.path {
            Some(path) => format!(
                "{}{} at {}",
                tool.program,
                tool.version
                    .as_deref()
                    .map(|v| format!(" {v}"))
                    .unwrap_or_default(),
                path.display()
            ),
            None => format!("{} missing (needed for {})", tool.program, tool.needed_for),
        })
        .collect();
    format!("Tools: {}", parts.join("; "))
}

/// The `doctor` table: one row per harness.
pub fn table(reports: &[Report]) -> String {
    let header = [
        "HARNESS",
        "STATUS",
        "VERSION",
        "TESTED",
        "CAPABILITIES",
        "PATH",
    ];
    let rows: Vec<[String; 6]> = std::iter::once(header.map(String::from))
        .chain(reports.iter().map(|r| {
            [
                r.harness.clone(),
                (if r.installed() {
                    "installed"
                } else {
                    "missing"
                })
                .into(),
                r.version_label(),
                r.tested_label().into(),
                r.summary.clone(),
                r.path
                    .as_ref()
                    .map_or_else(|| r.program.clone(), |p| p.display().to_string()),
            ]
        }))
        .collect();
    let mut widths = [0; 6];
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut output = String::new();
    for row in &rows {
        let line: Vec<String> = row
            .iter()
            .zip(widths)
            .map(|(cell, width)| format!("{cell:width$}"))
            .collect();
        output.push_str(line.join("  ").trim_end());
        output.push('\n');
    }
    output
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PinOp {
    /// Bare pin: exact for 3+ segments (`0.167.0`), segment-prefix for 1–2 (`0.167`, `2`).
    Bare,
    Eq,
    Ge,
    Gt,
    Le,
    Lt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PinClause {
    op: PinOp,
    parts: Vec<u64>,
}

fn parse_version_parts(raw: &str) -> Option<Vec<u64>> {
    let token = raw.trim().strip_prefix('v').unwrap_or(raw.trim());
    if token.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for segment in token.split('.') {
        if segment.is_empty() || !segment.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        parts.push(segment.parse().ok()?);
    }
    (!parts.is_empty()).then_some(parts)
}

fn detected_version_parts(line: &str) -> Option<Vec<u64>> {
    let token = version_tokens(line).next()?;
    let mut parts = Vec::new();
    for segment in token.split('.') {
        let digits: String = segment.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            break;
        }
        parts.push(digits.parse().ok()?);
    }
    (!parts.is_empty()).then_some(parts)
}

fn parse_pin_clauses(pin: &str) -> Result<Vec<PinClause>> {
    let trimmed = pin.trim();
    ensure!(!trimmed.is_empty(), "Version pin cannot be empty");
    let mut clauses = Vec::new();
    for part in trimmed.split(',') {
        let part = part.trim();
        ensure!(!part.is_empty(), "Empty clause in version pin {pin:?}");
        let (op, rest) = if let Some(r) = part.strip_prefix(">=") {
            (PinOp::Ge, r)
        } else if let Some(r) = part.strip_prefix("<=") {
            (PinOp::Le, r)
        } else if let Some(r) = part.strip_prefix("==") {
            (PinOp::Eq, r)
        } else if let Some(r) = part.strip_prefix('>') {
            (PinOp::Gt, r)
        } else if let Some(r) = part.strip_prefix('<') {
            (PinOp::Lt, r)
        } else if let Some(r) = part.strip_prefix('=') {
            (PinOp::Eq, r)
        } else {
            (PinOp::Bare, part)
        };
        let parts = parse_version_parts(rest).with_context(|| {
            format!("Expected numeric version (e.g. 0.167.0 or >=0.160.0), got {pin:?}")
        })?;
        clauses.push(PinClause { op, parts });
    }
    Ok(clauses)
}

/// Validates a `[harness_versions]` pin spec (e.g. `"0.167.0"`, `"0.167"`,
/// `">=0.160.0"`, `">=0.160.0, <0.200.0"`).
pub fn validate_version_pin(pin: &str) -> Result<()> {
    parse_pin_clauses(pin)?;
    Ok(())
}

fn cmp_version(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    let len = a.len().max(b.len());
    for i in 0..len {
        let av = a.get(i).copied().unwrap_or(0);
        let bv = b.get(i).copied().unwrap_or(0);
        match av.cmp(&bv) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

/// Whether a `--version` line matches a `[harness_versions]` pin.
pub fn version_pin_matches(pin: &str, detected_line: &str) -> bool {
    let Ok(clauses) = parse_pin_clauses(pin) else {
        return false;
    };
    let Some(found) = detected_version_parts(detected_line) else {
        return false;
    };
    clauses.iter().all(|clause| match clause.op {
        PinOp::Bare => {
            if clause.parts.len() < 3 {
                found.len() >= clause.parts.len() && found[..clause.parts.len()] == clause.parts[..]
            } else {
                cmp_version(&found, &clause.parts) == std::cmp::Ordering::Equal
            }
        }
        PinOp::Eq => cmp_version(&found, &clause.parts) == std::cmp::Ordering::Equal,
        PinOp::Ge => cmp_version(&found, &clause.parts) != std::cmp::Ordering::Less,
        PinOp::Gt => cmp_version(&found, &clause.parts) == std::cmp::Ordering::Greater,
        PinOp::Le => cmp_version(&found, &clause.parts) != std::cmp::Ordering::Greater,
        PinOp::Lt => cmp_version(&found, &clause.parts) == std::cmp::Ordering::Less,
    })
}

fn evaluate_version_pin(report: &mut Report) {
    let Some(pin) = report.version_pin.clone() else {
        return;
    };
    if !report.installed() {
        report.version_pin_ok = Some(false);
        report.notes.push(format!(
            "Pinned to {pin}, but {} is not installed.",
            report.program
        ));
        return;
    }
    let Some(version) = report.version.as_deref() else {
        report.version_pin_ok = Some(false);
        report.notes.push(format!(
            "Pinned to {pin}, but {} version could not be detected.",
            report.program
        ));
        return;
    };
    let ok = version_pin_matches(&pin, version);
    report.version_pin_ok = Some(ok);
    if ok {
        report
            .notes
            .push(format!("Matches configured version pin {pin}."));
    } else {
        report.notes.push(format!(
            "Version {} does not match configured pin {pin}.",
            report.version_label()
        ));
    }
}

/// Checks configured `[providers.*]` API key environment variables (existence
/// and non-emptiness only; never reads or records secret values).
pub fn inspect_providers(settings: &Settings) -> Vec<ProviderCheck> {
    inspect_providers_with(settings, |var| {
        std::env::var_os(var).is_some_and(|v| !v.is_empty())
    })
}

pub fn inspect_providers_with(
    settings: &Settings,
    env_set: impl Fn(&str) -> bool,
) -> Vec<ProviderCheck> {
    settings
        .providers
        .keys()
        .map(|name| {
            let resolved = providers::resolve(name, &settings.providers);
            let api_key_env = resolved
                .map(|p| p.api_key_env.clone())
                .unwrap_or_else(|| settings.providers[name].api_key_env.clone());
            let status = if api_key_env.is_empty() {
                ProviderKeyStatus::NotRequired
            } else if env_set(&api_key_env) {
                ProviderKeyStatus::Set
            } else {
                ProviderKeyStatus::Missing
            };
            ProviderCheck {
                name: name.clone(),
                api_key_env,
                status,
            }
        })
        .collect()
}

/// One line for `doctor` summarizing configured `[providers.*]` key presence.
pub fn providers_line(checks: &[ProviderCheck]) -> Option<String> {
    if checks.is_empty() {
        return None;
    }
    let parts: Vec<String> = checks
        .iter()
        .map(|c| match c.status {
            ProviderKeyStatus::Set => format!("{} ({} set)", c.name, c.api_key_env),
            ProviderKeyStatus::Missing => format!("{} ({} missing)", c.name, c.api_key_env),
            ProviderKeyStatus::NotRequired => format!("{} (no key required)", c.name),
        })
        .collect();
    Some(format!("Providers: {}", parts.join("; ")))
}

/// One line for `doctor` summarizing `[harness_versions]` pins.
pub fn pins_line(settings: &Settings, reports: &[Report]) -> Option<String> {
    if settings.harness_versions.is_empty() {
        return None;
    }
    let parts: Vec<String> = settings
        .harness_versions
        .iter()
        .map(
            |(harness, pin)| match reports.iter().find(|r| r.harness == *harness) {
                Some(r) if r.version_pin_ok == Some(true) => {
                    format!("{harness} {} matches {pin}", r.version_label())
                }
                Some(r) if !r.installed() => format!("{harness} missing (pinned {pin})"),
                Some(r) => format!("{harness} {} drift (pinned {pin})", r.version_label()),
                None => format!("{harness} unknown harness (pinned {pin})"),
            },
        )
        .collect();
    Some(format!("Version pins: {}", parts.join("; ")))
}

struct AuthSpec {
    env_vars: &'static [&'static str],
    /// Paths relative to the user's home directory, checked with `exists()` only.
    home_files: &'static [&'static str],
    guidance: &'static str,
}

fn auth_spec(harness: &str) -> Option<AuthSpec> {
    Some(match harness {
        "codex" => AuthSpec {
            env_vars: &["OPENAI_API_KEY", "CODEX_API_KEY"],
            home_files: &[".codex/auth.json", ".codex/config.toml"],
            guidance: "set OPENAI_API_KEY or run `codex login`",
        },
        "claude" => AuthSpec {
            env_vars: &[
                "ANTHROPIC_API_KEY",
                "CLAUDE_CODE_OAUTH_TOKEN",
                "ANTHROPIC_AUTH_TOKEN",
            ],
            home_files: &[
                ".claude/.credentials.json",
                ".claude.json",
                ".config/claude/credentials.json",
            ],
            guidance: "set ANTHROPIC_API_KEY or run `claude` to sign in",
        },
        "pi" => AuthSpec {
            env_vars: &[
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "GEMINI_API_KEY",
                "OPENROUTER_API_KEY",
            ],
            home_files: &[
                ".pi/agent/auth.json",
                ".pi/agent/models.json",
                ".config/pi/auth.json",
            ],
            guidance: "set a provider API key env var or configure ~/.pi/agent/auth.json",
        },
        "omp" => AuthSpec {
            env_vars: &[
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "GEMINI_API_KEY",
                "OPENROUTER_API_KEY",
            ],
            home_files: &[
                ".omp/agent/auth.json",
                ".omp/agent/models.json",
                ".pi/agent/auth.json",
            ],
            guidance: "set a provider API key env var or configure ~/.omp/agent/auth.json",
        },
        "cursor" => AuthSpec {
            env_vars: &["CURSOR_API_KEY"],
            home_files: &[
                ".cursor/cli-config.json",
                ".config/cursor/auth.json",
                ".cursor/auth.json",
            ],
            guidance: "set CURSOR_API_KEY or run `cursor-agent login`",
        },
        "antigravity" => AuthSpec {
            env_vars: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            home_files: &[
                ".gemini/antigravity-cli/settings.json",
                ".gemini/oauth_creds.json",
                ".gemini/settings.json",
            ],
            guidance: "run `agy` to sign in or set GEMINI_API_KEY",
        },
        "opencode" => AuthSpec {
            env_vars: &[
                "OPENAI_API_KEY",
                "ANTHROPIC_API_KEY",
                "OPENROUTER_API_KEY",
                "GEMINI_API_KEY",
            ],
            home_files: &[
                ".local/share/opencode/auth.json",
                ".config/opencode/opencode.json",
            ],
            guidance: "set a provider API key env var or run `opencode auth login`",
        },
        "aider" => AuthSpec {
            env_vars: &[
                "OPENAI_API_KEY",
                "ANTHROPIC_API_KEY",
                "OPENROUTER_API_KEY",
                "GEMINI_API_KEY",
                "DEEPSEEK_API_KEY",
            ],
            home_files: &[".aider.conf.yml", ".aider.model.settings.yml"],
            guidance: "set OPENAI_API_KEY/ANTHROPIC_API_KEY or configure ~/.aider.conf.yml",
        },
        "goose" => AuthSpec {
            env_vars: &[
                "OPENAI_API_KEY",
                "ANTHROPIC_API_KEY",
                "GOOGLE_API_KEY",
                "OPENROUTER_API_KEY",
            ],
            home_files: &[".config/goose/config.yaml", ".config/goose/secrets.yaml"],
            guidance: "set a provider API key env var or run `goose configure`",
        },
        "amp" => AuthSpec {
            env_vars: &["AMP_API_KEY"],
            home_files: &[
                ".local/share/amp/secrets.json",
                ".config/amp/settings.json",
                ".amp/secrets.json",
            ],
            guidance: "set AMP_API_KEY or run `amp login`",
        },
        "copilot" => AuthSpec {
            env_vars: &["GH_TOKEN", "GITHUB_TOKEN", "COPILOT_GITHUB_TOKEN"],
            home_files: &[
                ".copilot/config.json",
                ".config/github-copilot/hosts.json",
                ".config/github-copilot/apps.json",
            ],
            guidance: "set GH_TOKEN/GITHUB_TOKEN or run `copilot` to sign in",
        },
        _ => return None,
    })
}

/// Offline auth-readiness hint for a built-in harness: checks only presence of
/// well-known env vars or config files (never reads secrets or contacts a host).
pub fn auth_hint_for(harness: &str) -> AuthHint {
    let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    auth_hint_with(
        harness,
        home.as_deref(),
        |var| std::env::var_os(var).is_some_and(|v| !v.is_empty()),
        |var| std::env::var_os(var).map(PathBuf::from),
    )
}

pub fn auth_hint_with(
    harness: &str,
    home: Option<&Path>,
    env_set: impl Fn(&str) -> bool,
    env_path: impl Fn(&str) -> Option<PathBuf>,
) -> AuthHint {
    let Some(spec) = auth_spec(harness) else {
        return AuthHint {
            status: AuthStatus::Missing,
            signals: vec![],
            detail: "no built-in auth hints for this harness".into(),
        };
    };
    let mut signals = Vec::new();
    for &var in spec.env_vars {
        if env_set(var) {
            signals.push(var.to_owned());
        }
    }
    if harness == "codex" {
        if let Some(codex_home) = env_path("CODEX_HOME") {
            if codex_home.join("auth.json").exists() {
                signals.push("$CODEX_HOME/auth.json".into());
            } else if codex_home.join("config.toml").exists() {
                signals.push("$CODEX_HOME/config.toml".into());
            }
        }
    }
    if let Some(home) = home {
        for &rel in spec.home_files {
            if home.join(rel).exists() {
                signals.push(format!("~/{rel}"));
            }
        }
    }
    if signals.is_empty() {
        AuthHint {
            status: AuthStatus::Missing,
            signals,
            detail: spec.guidance.into(),
        }
    } else {
        AuthHint {
            status: AuthStatus::Ready,
            signals,
            detail: "local auth signal present (validity not verified offline)".into(),
        }
    }
}

/// One line for `doctor` summarizing offline auth-readiness hints for installed harnesses.
pub fn auth_hints_line(reports: &[Report]) -> Option<String> {
    let parts: Vec<String> = reports
        .iter()
        .filter_map(|r| {
            let hint = r.auth_hint.as_ref()?;
            Some(match hint.status {
                AuthStatus::Ready => {
                    format!("{} ready ({})", r.harness, hint.signals.join(", "))
                }
                AuthStatus::Missing => format!("{} none detected ({})", r.harness, hint.detail),
            })
        })
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(format!("Auth hints (offline): {}", parts.join("; ")))
    }
}

/// All `--strict` failure reasons: missing flags, version pin drift, or missing
/// required `[providers.*]` API keys.
pub fn strict_failures(
    settings: &Settings,
    reports: &[Report],
    providers: &[ProviderCheck],
) -> Vec<String> {
    let mut failures = Vec::new();
    if reports.iter().any(Report::missing_flags) {
        failures.push("An installed harness lacks a flag the router passes".into());
    }
    for (harness, pin) in &settings.harness_versions {
        match reports.iter().find(|r| r.harness == *harness) {
            Some(r) if r.version_pin_ok == Some(true) => {}
            Some(r) if !r.installed() => failures.push(format!(
                "Harness {harness} is pinned to {pin} but is not installed"
            )),
            Some(r) => failures.push(format!(
                "Harness {harness} version {} does not match pin {pin}",
                r.version_label()
            )),
            None => failures.push(format!(
                "Harness {harness} is pinned to {pin} but is unknown/unconfigured"
            )),
        }
    }
    for check in providers {
        if check.status == ProviderKeyStatus::Missing {
            failures.push(format!(
                "Configured provider {} requires {} which is unset or empty",
                check.name, check.api_key_env
            ));
        }
    }
    failures
}

/// Ensures `strict_failures` is empty, returning an error listing all failures.
pub fn check_strict(
    settings: &Settings,
    reports: &[Report],
    providers: &[ProviderCheck],
) -> Result<()> {
    let failures = strict_failures(settings, reports, providers);
    if !failures.is_empty() {
        bail!("{}", failures.join("; "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(text: &str) -> Vec<String> {
        help_flags(text).into_iter().collect()
    }

    #[test]
    fn help_flags_read_common_help_layouts() {
        // clap, commander, pi, OpenCode 2, argparse, and negatable forms.
        let help = "  -m, --model <MODEL>\n          Model the agent should use\n  \
            -o, --output-last-message <FILE>\n  \
            -p, --print                  Print responses (default: false)\n  \
            --output-format <format>     Output format: text | json\n  \
            --print, -p                    Non-interactive mode\n  \
            --model, -m string      Model to use in the format provider/model#variant\n  \
            --auto-commits, --no-auto-commits\n  \
            --[no-]color  Colors\n  \
            -x, --execute [message]\n  \
            --                             End option parsing\n";
        assert_eq!(
            flags(help),
            [
                "--auto-commits",
                "--color",
                "--execute",
                "--model",
                "--no-auto-commits",
                "--no-color",
                "--output-format",
                "--output-last-message",
                "--print",
                "-m",
                "-o",
                "-p",
                "-x"
            ]
        );
    }

    #[test]
    fn prose_and_bullets_do_not_define_flags() {
        let help = "  --tmux    Use --tmux=classic for tmux\n\
            Use --variant to pick effort\n\
            - on-request: The model decides\n\
            ------------\n";
        assert_eq!(flags(help), ["--tmux"]);
    }

    #[test]
    fn sample_invocations_follow_adapter_subcommands_and_modes() {
        let find = |harness: &str, flag: &str| {
            flag_uses(harness)
                .unwrap()
                .into_iter()
                .filter(|u| u.flag == flag)
                .map(|u| (u.help.join(" "), u.modes))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            find("codex", "--skip-git-repo-check"),
            [("exec --help".to_owned(), vec![Mode::Unattended])]
        );
        assert_eq!(
            find("codex", "--model"),
            [
                ("exec --help".to_owned(), vec![Mode::Unattended]),
                ("--help".to_owned(), vec![Mode::Interactive]),
                ("resume --help".to_owned(), vec![Mode::Interactive])
            ]
        );
        assert_eq!(
            find("claude", "--session-id"),
            [(
                "--help".to_owned(),
                vec![Mode::Unattended, Mode::Interactive]
            )]
        );
        assert_eq!(
            find("claude", "--resume"),
            [("--help".to_owned(), vec![Mode::Interactive])]
        );
        assert_eq!(
            find("codex", "--oss"),
            [
                ("exec --help".to_owned(), vec![Mode::Unattended]),
                ("--help".to_owned(), vec![Mode::Interactive]),
                ("resume --help".to_owned(), vec![Mode::Interactive])
            ],
            "the local-provider path is sampled"
        );
        assert_eq!(
            find("opencode", "--model"),
            [("run --help".to_owned(), vec![Mode::Unattended])]
        );
        assert_eq!(
            find("opencode", "--prompt"),
            [("--help".to_owned(), vec![Mode::Interactive])]
        );
        assert_eq!(
            find("aider", "--yes-always"),
            [(
                "--help".to_owned(),
                vec![Mode::Unattended, Mode::Interactive]
            )]
        );
        assert_eq!(
            find("goose", "--provider"),
            [(
                "run --help".to_owned(),
                vec![Mode::Unattended, Mode::Interactive]
            )]
        );
        assert_eq!(
            find("claude", "--permission-mode"),
            [(
                "--help".to_owned(),
                vec![Mode::Unattended, Mode::Interactive]
            )]
        );
        assert_eq!(
            find("amp", "--stream-json"),
            [("--help".to_owned(), vec![Mode::Unattended])]
        );
        for harness in crate::config::BUILTINS {
            let uses = flag_uses(harness).unwrap();
            assert!(
                uses.iter()
                    .any(|u| u.flag == "--model" || u.flag == "--mode")
            );
            assert!(
                uses.iter()
                    .all(|u| u.flag.starts_with('-') && u.help.ends_with(&["--help".to_owned()]))
            );
        }
    }

    #[test]
    fn every_flag_a_spec_declares_is_probed() {
        for spec in crate::harness::SPECS {
            let uses = flag_uses(spec.name).unwrap();
            for flag in spec.declared_flags() {
                assert!(
                    uses.iter().any(|u| u.flag == flag),
                    "{}: {flag} is declared but never probed",
                    spec.name
                );
            }
        }
    }

    fn capability(flag: &str, modes: &[Mode], support: Support) -> Capability {
        Capability {
            flag: flag.into(),
            modes: modes.to_vec(),
            help: "x --help".into(),
            support,
        }
    }

    #[test]
    fn summaries_name_missing_flags_and_their_modes() {
        let both = [Mode::Unattended, Mode::Interactive];
        let caps = [
            capability("--format", &both, Support::Supported),
            capability("--variant", &both, Support::Missing),
            capability("--interactive", &[Mode::Interactive], Support::Missing),
        ];
        assert_eq!(
            summary(&caps, true),
            "missing: --variant, --interactive (interactive)"
        );
        assert_eq!(summary(&caps[..1], true), "ok");
        assert_eq!(summary(&caps, false), "not checked");
        let split = [
            capability("--model", &[Mode::Unattended], Support::Missing),
            capability("--model", &[Mode::Interactive], Support::Missing),
            capability("--json", &[Mode::Unattended], Support::Unknown),
        ];
        assert_eq!(summary(&split, true), "missing: --model; 1 unknown");
        assert_eq!(summary(&split[2..], true), "unknown: help unavailable");
    }

    #[test]
    fn tools_line_shows_versions_and_what_a_missing_tool_blocks() {
        let tools = vec![
            Tool {
                program: "git",
                needed_for: "worktrees",
                path: Some(PathBuf::from("/usr/bin/git")),
                version: Some("2.43.0".into()),
            },
            Tool {
                program: "gh",
                needed_for: "GitHub task imports, ship --draft-pr",
                path: None,
                version: None,
            },
        ];
        assert_eq!(
            tools_line(&tools),
            "Tools: git 2.43.0 at /usr/bin/git; gh missing (needed for GitHub task imports, ship --draft-pr)"
        );
        let found = inspect_tools(Path::new("."));
        assert_eq!(
            found.iter().map(|t| t.program).collect::<Vec<_>>(),
            vec!["git", "gh"]
        );
        // Tests run inside a Git checkout, so git is present with a version.
        let git = &found[0];
        assert!(git.path.is_some(), "{git:?}");
        assert!(
            git.version
                .as_deref()
                .is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit())),
            "{git:?}"
        );
    }

    #[test]
    fn versions_match_tested_tokens_not_prefixes() {
        assert_eq!(version_tokens("omp/18.6.3").collect::<Vec<_>>(), ["18.6.3"]);
        let tokens: Vec<_> = version_tokens("opencode v2.0.22").collect();
        assert_eq!(tokens, ["2.0.22"]);
        let amp: Vec<_> =
            version_tokens("0.0.1791134020-gd49eb2 (released 2026-10-04T17:13:40.000Z, 8h ago)")
                .collect();
        assert_eq!(amp[0], "0.0.1791134020-gd49eb2");
        assert!(!version_tokens("2.1.2890 (Claude Code)").any(|t| t == "2.1.289"));
        assert_eq!(
            first_line("\n\n  codex-cli 0.160.0 \nmore").as_deref(),
            Some("codex-cli 0.160.0")
        );
    }

    #[test]
    fn live_tests_in_the_project_count_as_captured_fixtures() {
        let project = tempfile::tempdir().unwrap();
        assert!(live_fixtures(project.path()).is_empty());
        let runs = project.path().join(LIVE_TESTS);
        for (run, text) in [
            (
                "claude-1",
                "[[fixture]]\nfile = \"claude/captured-success.jsonl\"\nharness = \"claude\"\nversion = \"9.9.9\"\nprovenance = \"captured\"\n",
            ),
            ("codex-1", "not toml ["),
        ] {
            std::fs::create_dir_all(runs.join(run)).unwrap();
            std::fs::write(runs.join(run).join("manifest.toml"), text).unwrap();
        }
        // A failed run leaves no manifest.
        std::fs::create_dir_all(runs.join("pi-1")).unwrap();
        let live = live_fixtures(project.path());
        assert_eq!(live.len(), 1);
        assert!(live[0].live && live[0].harness == "claude" && live[0].version == "9.9.9");
        assert!(manifest().iter().all(|f| !f.live));

        // A live pass of the detected version makes it `tested`; the parser
        // fixture count leaves it out.
        let built_in = Fixture {
            harness: "claude".into(),
            version: "1.0.0".into(),
            provenance: "schema-derived".into(),
            live: false,
        };
        let coverage = |version: &str, fixtures: &[Fixture]| {
            let mut report = Report {
                harness: "claude".into(),
                builtin: true,
                program: "claude".into(),
                path: None,
                version: Some(format!("{version} (Claude Code)")),
                tested_versions: vec![],
                version_tested: None,
                version_captured: false,
                version_pin: None,
                version_pin_ok: None,
                auth_hint: None,
                capabilities: None,
                summary: "ok".into(),
                notes: vec![],
            };
            record_fixtures(&mut report, fixtures);
            match_version(&mut report, fixtures);
            report
        };
        let both = [built_in, live.into_iter().next().unwrap()];
        let report = coverage("9.9.9", &both);
        assert_eq!(report.tested_label(), "tested");
        assert!(report.version_captured);
        assert_eq!(report.tested_versions, ["1.0.0", "9.9.9"]);
        assert!(
            report
                .notes
                .contains(&"Parser fixtures: 0 captured, 1 schema-derived.".into()),
            "{:?}",
            report.notes
        );
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.starts_with("A live test of this version passed"))
        );
        // Another version stays untested; the built-in one is schema-derived.
        assert_eq!(coverage("9.9.10", &both).tested_label(), "untested");
        let report = coverage("1.0.0", &both);
        assert_eq!(report.tested_label(), "schema-derived");
        assert!(!report.notes.iter().any(|n| n.starts_with("A live test")));
    }

    #[test]
    fn manifest_names_builtins_and_custom_adapters_are_not_run() {
        let fixtures = manifest();
        assert!(!fixtures.is_empty(), "manifest must parse");
        for harness in crate::config::BUILTINS {
            assert!(fixtures.iter().any(|f| f.harness == *harness), "{harness}");
        }
        let mut settings = Settings::default();
        settings.adapters.insert(
            "mine".into(),
            crate::config::CustomAdapter {
                command: "definitely-not-installed-router-tool".into(),
                ..Default::default()
            },
        );
        let report = inspect("mine", &settings, Path::new("."));
        assert_eq!(report.summary, "not declared");
        assert_eq!(report.version_label(), "not declared");
        assert!(report.capabilities.is_none() && report.version.is_none());
        assert!(report.notes[0].contains("Authentication is not checked"));
    }

    #[test]
    fn version_pins_validate_and_match_exact_prefix_and_ranges() {
        assert!(validate_version_pin("0.167.0").is_ok());
        assert!(validate_version_pin("0.167").is_ok());
        assert!(validate_version_pin(">=0.160.0").is_ok());
        assert!(validate_version_pin(">=0.160.0, <0.200.0").is_ok());
        assert!(validate_version_pin("").is_err());
        assert!(validate_version_pin("latest").is_err());
        assert!(validate_version_pin("!=0.160.0").is_err());
        assert!(validate_version_pin("0..1").is_err());

        // Exact 3-segment pin
        assert!(version_pin_matches("0.167.0", "codex-cli 0.167.0"));
        assert!(!version_pin_matches("0.167.0", "codex-cli 0.167.1"));
        // Prefix 1–2 segment pin respects segment boundaries
        assert!(version_pin_matches("0.167", "codex-cli 0.167.5"));
        assert!(!version_pin_matches("0.16", "codex-cli 0.167.0"));
        assert!(version_pin_matches("2", "opencode v2.0.22"));
        assert!(!version_pin_matches("2", "opencode v1.9.0"));
        // Semver comparisons & ranges
        assert!(version_pin_matches(">=0.160.0", "codex-cli 0.167.0"));
        assert!(version_pin_matches(">=0.160.0", "codex-cli 0.160.0"));
        assert!(!version_pin_matches(">=0.160.0", "codex-cli 0.159.9"));
        assert!(version_pin_matches(
            ">=0.160.0, <0.170.0",
            "codex-cli 0.167.0"
        ));
        assert!(!version_pin_matches(
            ">=0.160.0, <0.170.0",
            "codex-cli 0.170.0"
        ));
    }

    #[test]
    fn provider_checks_and_strict_failures_never_leak_secrets() {
        let mut settings = Settings::default();
        // Built-in preset opt-in inherits OPENROUTER_API_KEY; ollama needs no key.
        settings
            .providers
            .insert("openrouter".into(), providers::Provider::default());
        settings
            .providers
            .insert("ollama".into(), providers::Provider::default());
        settings.providers.insert(
            "gateway".into(),
            providers::Provider {
                base_url: "https://gw.example/v1".into(),
                api_key_env: "GW_SECRET_TOKEN".into(),
                ..Default::default()
            },
        );
        let checks = inspect_providers_with(&settings, |var| var == "OPENROUTER_API_KEY");
        assert_eq!(checks.len(), 3);
        let find = |name: &str| checks.iter().find(|c| c.name == name).unwrap();
        assert_eq!(find("openrouter").status, ProviderKeyStatus::Set);
        assert_eq!(find("ollama").status, ProviderKeyStatus::NotRequired);
        assert_eq!(find("gateway").status, ProviderKeyStatus::Missing);
        let line = providers_line(&checks).unwrap();
        assert!(
            line.contains("openrouter (OPENROUTER_API_KEY set)"),
            "{line}"
        );
        assert!(line.contains("gateway (GW_SECRET_TOKEN missing)"), "{line}");
        assert!(line.contains("ollama (no key required)"), "{line}");

        let failures = strict_failures(&settings, &[], &checks);
        assert!(
            failures
                .iter()
                .any(|f| f.contains("gateway") && f.contains("GW_SECRET_TOKEN")),
            "{failures:?}"
        );
    }

    #[test]
    fn offline_auth_hints_detect_env_or_files_without_reading_contents() {
        let home = tempfile::tempdir().unwrap();
        let none = auth_hint_with("codex", Some(home.path()), |_| false, |_| None);
        assert_eq!(none.status, AuthStatus::Missing);
        assert!(none.signals.is_empty());

        let via_env = auth_hint_with(
            "codex",
            Some(home.path()),
            |v| v == "OPENAI_API_KEY",
            |_| None,
        );
        assert_eq!(via_env.status, AuthStatus::Ready);
        assert_eq!(via_env.signals, ["OPENAI_API_KEY"]);

        std::fs::create_dir_all(home.path().join(".codex")).unwrap();
        std::fs::write(
            home.path().join(".codex/auth.json"),
            "SECRET_VALUE_DO_NOT_LEAK",
        )
        .unwrap();
        let via_file = auth_hint_with("codex", Some(home.path()), |_| false, |_| None);
        assert_eq!(via_file.status, AuthStatus::Ready);
        assert_eq!(via_file.signals, ["~/.codex/auth.json"]);
        assert!(!via_file.detail.contains("SECRET_VALUE"));
    }
}
