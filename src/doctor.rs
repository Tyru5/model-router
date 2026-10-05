//! Offline adapter compatibility checks. Only `--version` and `--help` run, so
//! no model request is sent and authentication is never checked.
use crate::{
    adapters::{self, ContextPaths, Invocation, OutputKind},
    catalog,
    config::{BUILTINS, Mode, Settings, Stage},
    harness_settings, process, storage,
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    thread,
    time::Duration,
};

pub const AUTH_NOTE: &str =
    "Authentication is not checked: that needs a model request. Log in through each harness.";
const MANIFEST: &str = include_str!("../tests/fixtures/harness-output/manifest.toml");
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
    /// Whether `version` is a tested version; None when no version was detected.
    pub version_tested: Option<bool>,
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
    pub fn version_label(&self) -> String {
        match &self.version {
            _ if !self.builtin => "not declared".into(),
            Some(line) => version_tokens(line).next().unwrap_or(line).into(),
            None if self.installed() => "unknown".into(),
            None => "-".into(),
        }
    }
    pub fn tested_label(&self) -> &'static str {
        match self.version_tested {
            Some(true) => "tested",
            Some(false) => "untested",
            None => "-",
        }
    }
}

/// Checks every built-in and custom harness in parallel threads.
pub fn inspect_all(settings: &Settings, project: &Path) -> Vec<Report> {
    let names = catalog::known_harnesses(settings);
    thread::scope(|scope| {
        let checks: Vec<_> = names
            .iter()
            .map(|name| scope.spawn(move || inspect(name, settings, project)))
            .collect();
        checks
            .into_iter()
            .map(|check| check.join().expect("Harness check panicked"))
            .collect()
    })
}

pub fn inspect(harness: &str, settings: &Settings, project: &Path) -> Report {
    let program = adapters::program(harness, &settings.adapters);
    let mut report = Report {
        harness: harness.into(),
        builtin: BUILTINS.contains(&harness),
        path: adapters::executable_at(&program, project),
        program,
        version: None,
        tested_versions: vec![],
        version_tested: None,
        capabilities: None,
        summary: "not declared".into(),
        notes: vec![AUTH_NOTE.into()],
    };
    if report.builtin {
        check(&mut report, project);
    } else if settings.adapters.contains_key(harness) {
        report
            .notes
            .push("Custom adapter: no version or flag contract, so its command is not run.".into());
    } else {
        report.notes.push(format!("Unknown harness {harness}"));
    }
    report
}

fn check(report: &mut Report, project: &Path) {
    let fixtures: Vec<_> = manifest()
        .into_iter()
        .filter(|f| f.harness == report.harness)
        .collect();
    for fixture in &fixtures {
        if !report.tested_versions.contains(&fixture.version) {
            report.tested_versions.push(fixture.version.clone());
        }
    }
    let captured = fixtures
        .iter()
        .filter(|f| f.provenance == "captured")
        .count();
    report.notes.push(format!(
        "Parser fixtures: {captured} captured, {} schema-derived.",
        fixtures.len() - captured
    ));
    let uses = flag_uses(&report.harness).unwrap_or_else(|error| {
        report
            .notes
            .push(format!("Sample invocation failed: {error:#}"));
        vec![]
    });
    let help_args: BTreeSet<&Vec<String>> = uses.iter().map(|u| &u.help).collect();
    let mut helps = BTreeMap::new();
    if report.installed() {
        let program = report.program.clone();
        let program = program.as_str();
        let (version, outputs) = thread::scope(|scope| {
            let version =
                scope.spawn(|| probe(program, &["--version".into()], project, VERSION_TIMEOUT));
            let outputs: Vec<_> = help_args
                .iter()
                .map(|args| {
                    let output = scope.spawn(move || probe(program, args, project, HELP_TIMEOUT));
                    (*args, output)
                })
                .collect();
            (
                joined(version),
                outputs
                    .into_iter()
                    .map(|(args, output)| (args, joined(output)))
                    .collect::<Vec<_>>(),
            )
        });
        match version {
            Ok((true, text)) => report.version = first_line(&text),
            Ok((false, _)) => report.notes.push(format!("{program} --version failed.")),
            Err(error) => report.notes.push(format!("{program} --version: {error:#}")),
        }
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
    } else {
        report
            .notes
            .push(format!("{} not found on PATH.", report.program));
    }
    report.version_tested = report.version.as_deref().map(|line| {
        version_tokens(line).any(|token| report.tested_versions.iter().any(|v| v == token))
    });
    if report.version_tested == Some(false) {
        report.notes.push(format!(
            "Detected version is not a parser-tested version ({}).",
            report.tested_versions.join(", ")
        ));
    }
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

/// Builds sample invocations with the adapter itself, so checks follow its code.
fn flag_uses(harness: &str) -> Result<Vec<FlagUse>> {
    let temp = tempfile::tempdir()?;
    let prompt_file = temp.path().join("prompt.md");
    let paths = ContextPaths {
        workspace: temp.path(),
        attempt_dir: temp.path(),
        prompt_file: &prompt_file,
        handoff_file: &prompt_file,
    };
    let mut uses: Vec<FlagUse> = vec![];
    for mode in [Mode::Unattended, Mode::Interactive] {
        // A hosted and a local provider reach both provider code paths.
        for provider in ["openai", "ollama"] {
            let invocation = adapters::build(
                &sample(harness, mode, provider),
                &BTreeMap::new(),
                &BTreeMap::new(),
                SAMPLE_PROMPT,
                &paths,
            )?;
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
    Ok(uses)
}

fn sample(harness: &str, mode: Mode, provider: &str) -> Stage {
    let supported = |yes: bool, value: &str| if yes { value.into() } else { String::new() };
    Stage {
        harness: harness.into(),
        mode,
        model: "sample-model".into(),
        provider: supported(harness_settings::supports_provider(harness), provider),
        effort: supported(harness_settings::supports_effort(harness), "high"),
        harness_settings: harness_settings::settings(harness)
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
    line.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | ';'))
        .map(|token| token.strip_prefix('v').unwrap_or(token))
        .filter(|token| token.starts_with(|c: char| c.is_ascii_digit()))
}

#[derive(Deserialize)]
struct Manifest {
    fixture: Vec<Fixture>,
}
#[derive(Deserialize)]
struct Fixture {
    harness: String,
    version: String,
    provenance: String,
}

fn manifest() -> Vec<Fixture> {
    toml::from_str::<Manifest>(MANIFEST)
        .map(|m| m.fixture)
        .unwrap_or_default()
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
                ("--help".to_owned(), vec![Mode::Interactive])
            ]
        );
        assert_eq!(
            find("codex", "--oss"),
            [
                ("exec --help".to_owned(), vec![Mode::Unattended]),
                ("--help".to_owned(), vec![Mode::Interactive])
            ],
            "the local-provider path is sampled"
        );
        assert_eq!(
            find("opencode", "--model"),
            [(
                "run --help".to_owned(),
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
        for harness in BUILTINS {
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
    fn versions_match_tested_tokens_not_prefixes() {
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
    fn manifest_names_builtins_and_custom_adapters_are_not_run() {
        let fixtures = manifest();
        assert!(!fixtures.is_empty(), "manifest must parse");
        for harness in BUILTINS {
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
}
