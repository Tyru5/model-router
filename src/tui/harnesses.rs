use super::tasks::{Msg, Tasks};
use super::{
    actions::ActionId,
    app::App,
    modal::{Ask, Modal},
    theme::Palette,
    widgets::short_path,
};
use crate::{
    catalog,
    config::Settings,
    doctor::{self, Report, Support},
};
use anyhow::{Result, bail};
use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Cell, HighlightSpacing, Row, Table, TableState},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub(super) type Probe = fn(&Settings, &Path) -> Vec<Report>;

/// The script, in a checkout of this repository, that sends one real request
/// through a harness and records its sanitized output.
pub(super) const TEST_SCRIPT: &str = "scripts/capture_harness_output.py";
/// Exact contents of `TEST_SCRIPT` built into this binary. `can_test_harnesses`
/// verifies the checkout's copy matches byte-for-byte (without following
/// symlinks), so an untrusted repo cannot plant an arbitrary Python script.
const TEST_SCRIPT_BYTES: &[u8] = include_bytes!("../../scripts/capture_harness_output.py");

/// One live harness test: the script's arguments.
#[derive(Clone, Debug)]
pub(super) struct HarnessTest {
    pub(super) harness: String,
    pub(super) project: PathBuf,
    /// The running router, so the test exercises this build.
    pub(super) router: PathBuf,
    /// Where the script saves the sanitized output and its manifest entry.
    pub(super) output: PathBuf,
}
/// How a finished test went.
#[derive(Clone, Debug)]
pub(super) struct TestOutcome {
    pub(super) passed: bool,
    /// The result dialog's body: command, exit status, output.
    pub(super) report: String,
}
/// Runs a test; tests swap in a stub so no request is sent.
pub(super) type TestRunner = fn(&HarnessTest) -> Result<TestOutcome, String>;

/// A harness's live test this session, shown in the TESTED column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LiveTest {
    Running,
    Passed,
    Failed,
}

/// Live tests started from the Harnesses page.
pub(super) struct HarnessTests {
    /// Harnesses marked for the next batch.
    pub(super) marked: BTreeSet<String>,
    /// The latest test of each harness this session.
    pub(super) states: BTreeMap<String, LiveTest>,
    /// The running batch in start order, with each result once it arrives.
    /// One batch runs at a time; its tests run in parallel.
    pub(super) batch: Vec<(String, Option<Result<TestOutcome, String>>)>,
    /// Identifies the running batch, so a result from an earlier one (e.g.
    /// of another project) is dropped.
    pub(super) batch_id: u64,
    pub(super) runner: TestRunner,
}
impl Default for HarnessTests {
    fn default() -> Self {
        Self {
            marked: BTreeSet::new(),
            states: BTreeMap::new(),
            batch: vec![],
            batch_id: 0,
            runner: run_test,
        }
    }
}
impl HarnessTests {
    /// Harnesses of the batch still running.
    pub(super) fn running(&self) -> Vec<&str> {
        self.batch
            .iter()
            .filter(|(_, result)| result.is_none())
            .map(|(harness, _)| harness.as_str())
            .collect()
    }
    /// Forgets marks, states, and the batch, e.g. on a project switch; a
    /// running test's result is dropped when it arrives.
    pub(super) fn reset(&mut self) {
        *self = Self {
            runner: self.runner,
            ..Self::default()
        };
    }
}

/// Doctor results for the Harnesses page. Probes scan PATH and run `--version`
/// and `--help`, which is slow on some systems (e.g. WSL), so they run on a
/// thread when the page opens.
pub(super) struct HarnessChecks {
    pub(super) reports: Vec<Report>,
    /// A check is running.
    pub(super) checking: bool,
    /// Check again when the running one finishes: it may have read the
    /// project before a live test saved its manifest.
    pub(super) again: bool,
    /// Tests swap in a stub so no harness runs.
    pub(super) probe: Probe,
}
#[derive(Clone, PartialEq, Eq)]
struct ScriptStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

struct ScriptCache {
    project: PathBuf,
    stamp: Option<ScriptStamp>,
    verified: bool,
}

/// The Harnesses page: doctor results and the selected row.
#[derive(Default)]
pub(super) struct HarnessesPage {
    pub(super) selected: usize,
    pub(super) checks: HarnessChecks,
    pub(super) tests: HarnessTests,
    script_cache: std::cell::RefCell<Option<ScriptCache>>,
}

impl Default for HarnessChecks {
    fn default() -> Self {
        Self::new()
    }
}
impl HarnessChecks {
    pub(super) fn new() -> Self {
        Self {
            reports: vec![],
            checking: false,
            again: false,
            probe: doctor::inspect_all,
        }
    }
    /// Starts a check unless one is running. Old results stay until new ones arrive.
    pub(super) fn start(&mut self, settings: &Settings, project: &Path, tasks: &Tasks) {
        if self.checking {
            return;
        }
        self.checking = true;
        let (settings, project, probe) = (settings.clone(), project.to_path_buf(), self.probe);
        tasks.spawn(
            "Harness check failed",
            move || Ok(probe(&settings, &project)),
            Msg::Checks,
        );
    }
    /// Starts a check, or queues one after the running check.
    pub(super) fn refresh(&mut self, settings: &Settings, project: &Path, tasks: &Tasks) {
        if self.checking {
            self.again = true;
        } else {
            self.start(settings, project, tasks);
        }
    }
    pub(super) fn clear(&mut self) {
        self.reports.clear();
        self.checking = false;
        self.again = false;
    }
    pub(super) fn report(&self, harness: &str) -> Option<&Report> {
        self.reports.iter().find(|r| r.harness == harness)
    }
    /// The Workflow warning for a stage's harness; cached results only.
    pub(super) fn warning(&self, harness: &str) -> Option<String> {
        let report = self.report(harness).filter(|r| r.missing_flags())?;
        Some(format!(
            "⚠ {} {} lacks {}; see Harnesses [3]",
            report.harness,
            report.version_label(),
            missing(report).join(", ")
        ))
    }
}

/// Distinct missing flags in adapter order.
fn missing(report: &Report) -> Vec<&str> {
    let mut flags: Vec<&str> = vec![];
    for capability in report.capabilities.iter().flatten() {
        if capability.support == Support::Missing && !flags.contains(&capability.flag.as_str()) {
            flags.push(&capability.flag);
        }
    }
    flags
}

impl HarnessesPage {
    /// Harness names in display order: the last results, or the configured
    /// harnesses until the first check finishes.
    pub(super) fn names(&self, settings: &Settings) -> Vec<String> {
        if self.checks.reports.is_empty() {
            catalog::known_harnesses(settings)
        } else {
            self.checks
                .reports
                .iter()
                .map(|r| r.harness.clone())
                .collect()
        }
    }
    fn selected_name(&self, settings: &Settings) -> Option<String> {
        self.names(settings).get(self.selected).cloned()
    }
}

/// Whether `project/scripts/capture_harness_output.py` is a non-symlink regular
/// file matching this binary's built-in script byte-for-byte.
fn verify_test_script(project: &Path) -> bool {
    use std::io::Read;
    let Ok(file) = crate::storage::open_beneath(project, Path::new(TEST_SCRIPT)) else {
        return false;
    };
    let mut bytes = Vec::with_capacity(TEST_SCRIPT_BYTES.len());
    file.take((TEST_SCRIPT_BYTES.len() as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .is_ok()
        && bytes == TEST_SCRIPT_BYTES
}

impl HarnessTest {
    pub(super) fn script(&self) -> PathBuf {
        self.project.join(TEST_SCRIPT)
    }
    pub(super) fn argv(&self) -> Vec<OsString> {
        let python = if cfg!(windows) { "python" } else { "python3" };
        vec![
            python.into(),
            // Isolated mode: do not add `<project>/scripts` to `sys.path[0]`
            // and ignore `PYTHONPATH`, preventing stdlib module shadowing.
            "-I".into(),
            self.script().into(),
            self.harness.as_str().into(),
            "--confirm-paid-request".into(),
            "--router".into(),
            self.router.as_os_str().into(),
            "--output-dir".into(),
            self.output.as_os_str().into(),
        ]
    }
}

fn script_stamp(project: &Path) -> Option<ScriptStamp> {
    let meta = std::fs::symlink_metadata(project.join(TEST_SCRIPT)).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Some(ScriptStamp {
        len: meta.len(),
        modified: meta.modified().ok(),
        #[cfg(unix)]
        dev: meta.dev(),
        #[cfg(unix)]
        ino: meta.ino(),
    })
}

/// Verifies the checkout's script, then writes the embedded `TEST_SCRIPT_BYTES`
/// to a private temporary file outside the checkout so Python executes only
/// the verified bytes and never reopens a path inside the repository.
fn run_test(test: &HarnessTest) -> Result<TestOutcome, String> {
    if !verify_test_script(&test.project) {
        return Err(format!(
            "Refusing to run {TEST_SCRIPT}: file missing or does not match this build."
        ));
    }
    let mut temp = tempfile::Builder::new()
        .prefix("model-router-capture-")
        .suffix(".py")
        .tempfile()
        .map_err(|error| format!("Could not stage capture script: {error}"))?;
    std::io::Write::write_all(&mut temp, TEST_SCRIPT_BYTES)
        .map_err(|error| format!("Could not write capture script: {error}"))?;
    temp.as_file()
        .sync_all()
        .map_err(|error| format!("Could not flush capture script: {error}"))?;
    let mut argv = test.argv();
    // Replace the checkout script path (index 2, after `python3` and `-I`) with
    // the private temp file so a concurrent edit in the checkout cannot swap it.
    if let Some(slot) = argv.get_mut(2) {
        *slot = temp.path().as_os_str().to_owned();
    }
    run_test_with_argv(test, &argv)
}

#[cfg(test)]
fn run_test_unchecked(test: &HarnessTest) -> Result<TestOutcome, String> {
    let argv = test.argv();
    run_test_with_argv(test, &argv)
}

fn run_test_with_argv(test: &HarnessTest, argv: &[OsString]) -> Result<TestOutcome, String> {
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(&test.project)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("Could not start {}: {error}", argv[0].to_string_lossy()))?;
    let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !text.is_empty() && !stdout.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&stdout);
    let passed = output.status.success();
    Ok(TestOutcome {
        passed,
        report: test_report(test, passed, &output.status.to_string(), &text),
    })
}

/// The result dialog's body.
pub(super) fn test_report(test: &HarnessTest, passed: bool, status: &str, output: &str) -> String {
    let command: Vec<_> = test
        .argv()
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let output = crate::storage::clean_terminal(output.trim());
    let mut text = format!(
        "{}: {status}\n\n$ {}\n\n{}\n",
        if passed { "✓ Passed" } else { "✗ Failed" },
        shell_words::join(&command),
        if output.is_empty() {
            "(no output)"
        } else {
            &output
        }
    );
    if passed {
        text.push_str(&format!(
            "\nThe router ran {harness}. Review the sanitized output in {dir}/{harness}/. To keep it as a parser fixture, copy that file into tests/fixtures/harness-output/{harness}/ (rename it if the name is taken, and update the entry's `file`), append the [[fixture]] entry from {dir}/manifest.toml to tests/fixtures/harness-output/manifest.toml, then run `cargo test --test harness_output`.\n",
            harness = test.harness,
            dir = test.output.display()
        ));
    }
    text
}

impl App {
    pub(super) fn perform_harnesses(&mut self, id: ActionId) -> Result<()> {
        match id {
            ActionId::NextHarness => {
                self.harnesses.selected = (self.harnesses.selected + 1).min(
                    self.harnesses
                        .names(&self.editor.settings)
                        .len()
                        .saturating_sub(1),
                )
            }
            ActionId::PreviousHarness => {
                self.harnesses.selected = self.harnesses.selected.saturating_sub(1)
            }
            ActionId::HarnessDetails => {
                let Some(harness) = self.harnesses.selected_name(&self.editor.settings) else {
                    return Ok(());
                };
                let body = details(&harness, self.harnesses.checks.report(&harness));
                self.text(&harness, body);
            }
            ActionId::RefreshHarnesses => {
                self.harnesses
                    .checks
                    .start(&self.editor.settings, &self.project, &self.tasks)
            }
            ActionId::TestHarness | ActionId::MarkHarness | ActionId::MarkAllHarnesses
                if !self.can_test_harnesses() =>
            {
                self.ui.message = format!(
                    "Testing needs {TEST_SCRIPT}; open a model-router checkout as the project."
                );
            }
            // Marks need check results: only then is "installed" known.
            ActionId::MarkHarness | ActionId::MarkAllHarnesses
                if self.harnesses.checks.reports.is_empty() =>
            {
                self.ui.message = if self.harnesses.checks.checking {
                    "Wait for the harness check: marks need to know what is installed."
                } else {
                    "No check results to mark from; [r] rechecks."
                }
                .into();
            }
            ActionId::MarkHarness => {
                let Some(harness) = self.harnesses.selected_name(&self.editor.settings) else {
                    return Ok(());
                };
                let marked = &mut self.harnesses.tests.marked;
                if marked.remove(&harness) {
                    self.ui.message = format!("Unmarked {harness}; {} marked.", marked.len());
                } else if let Some(why) = self.untestable(&harness) {
                    self.ui.message = why;
                } else {
                    let marked = &mut self.harnesses.tests.marked;
                    marked.insert(harness.clone());
                    self.ui.message = format!(
                        "Marked {harness}; {} marked. [t] tests the marked harnesses.",
                        marked.len()
                    );
                }
            }
            ActionId::MarkAllHarnesses => {
                let testable: Vec<_> = self
                    .harnesses
                    .names(&self.editor.settings)
                    .into_iter()
                    .filter(|name| self.untestable(name).is_none())
                    .collect();
                let marked = &mut self.harnesses.tests.marked;
                marked.retain(|name| testable.contains(name));
                if testable.is_empty() {
                    self.ui.message = "No installed built-in harnesses to mark.".into();
                } else if testable.iter().all(|name| marked.contains(name)) {
                    marked.clear();
                    self.ui.message = "Cleared all marks.".into();
                } else {
                    marked.extend(testable);
                    self.ui.message = format!(
                        "Marked {} installed harnesses. [t] tests them.",
                        marked.len()
                    );
                }
            }
            ActionId::TestHarness => {
                let running = self.harnesses.tests.running();
                if !running.is_empty() {
                    self.ui.message = format!(
                        "Still testing {}; one batch runs at a time.",
                        running.join(", ")
                    );
                    return Ok(());
                }
                // The marked harnesses in display order, else the selected
                // one. A check since marking may have found one missing.
                let names = self.harnesses.names(&self.editor.settings);
                let (mut harnesses, dropped): (Vec<_>, Vec<_>) = names
                    .iter()
                    .filter(|name| self.harnesses.tests.marked.contains(*name))
                    .cloned()
                    .partition(|name| self.untestable(name).is_none());
                let marked = &mut self.harnesses.tests.marked;
                marked.retain(|name| harnesses.contains(name));
                if !dropped.is_empty() {
                    self.ui.message = format!(
                        "Unmarked {}: not installed or not built in.",
                        dropped.join(", ")
                    );
                    if harnesses.is_empty() {
                        return Ok(());
                    }
                }
                if harnesses.is_empty() {
                    let Some(harness) = self.harnesses.selected_name(&self.editor.settings) else {
                        return Ok(());
                    };
                    if let Some(why) = self.untestable(&harness) {
                        self.ui.message = why;
                        return Ok(());
                    }
                    harnesses.push(harness);
                }
                self.ui
                    .open(Modal::Ask(Box::new(Ask::TestHarness { harnesses })));
            }
            other => bail!("{other:?} is not a Harnesses action"),
        }
        Ok(())
    }
}

impl App {
    /// Whether this project has a verified copy of the built-in test script
    /// (regular file, no symlinks, matching this binary's script byte-for-byte).
    /// Cached by `(project, metadata)` so UI redraws do not reread the file.
    pub(super) fn can_test_harnesses(&self) -> bool {
        let stamp = script_stamp(&self.project);
        if let Some(cache) = self.harnesses.script_cache.borrow().as_ref() {
            if cache.project == self.project && cache.stamp == stamp {
                return cache.verified;
            }
        }
        let verified = stamp.is_some() && verify_test_script(&self.project);
        *self.harnesses.script_cache.borrow_mut() = Some(ScriptCache {
            project: self.project.clone(),
            stamp,
            verified,
        });
        verified
    }
    /// Where a test of `harness` started now saves its output.
    pub(super) fn test_output(&self, harness: &str) -> PathBuf {
        self.project.join(doctor::LIVE_TESTS).join(format!(
            "{harness}-{}",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        ))
    }
    /// Why `harness` cannot be tested: the script covers built-in
    /// harnesses, and a checked one must be installed.
    fn untestable(&self, harness: &str) -> Option<String> {
        if crate::harness::get(harness).is_none() {
            return Some(format!(
                "{harness} is a custom adapter; the test script covers built-in harnesses."
            ));
        }
        match self.harnesses.checks.report(harness) {
            Some(report) if !report.installed() => Some(format!("{harness} is not installed.")),
            _ => None,
        }
    }
    /// Starts the confirmed batch: each harness's test runs in parallel in
    /// the background.
    pub(super) fn start_harness_tests(&mut self, harnesses: Vec<String>) -> Result<()> {
        if !self.harnesses.tests.batch.is_empty() {
            return Ok(());
        }
        if !verify_test_script(&self.project) {
            self.ui.message = format!(
                "Refusing to run {TEST_SCRIPT}: file missing or does not match this build."
            );
            return Ok(());
        }
        // A check may have landed while the confirmation was open.
        let (harnesses, dropped): (Vec<_>, Vec<_>) = harnesses
            .into_iter()
            .partition(|name| self.untestable(name).is_none());
        let skipped = if dropped.is_empty() {
            String::new()
        } else {
            let marked = &mut self.harnesses.tests.marked;
            marked.retain(|name| !dropped.contains(name));
            format!(
                "Skipped {}: not installed or not built in.",
                dropped.join(", ")
            )
        };
        if harnesses.is_empty() {
            self.ui.message = skipped;
            return Ok(());
        }
        let router = running_router()?;
        let runner = self.harnesses.tests.runner;
        let batch = super::tasks::ticket();
        self.harnesses.tests.batch_id = batch;
        for harness in &harnesses {
            let test = HarnessTest {
                output: self.test_output(harness),
                harness: harness.clone(),
                project: self.project.clone(),
                router: router.clone(),
            };
            let tests = &mut self.harnesses.tests;
            tests.states.insert(harness.clone(), LiveTest::Running);
            tests.batch.push((harness.clone(), None));
            let harness = harness.clone();
            self.tasks.spawn(
                "Harness test stopped unexpectedly",
                move || runner(&test),
                move |result| Msg::HarnessTest {
                    batch,
                    harness,
                    result,
                },
            );
        }
        self.ui.message = match harnesses.len() {
            1 => format!(
                "Testing {}: one real request; the result opens when it finishes.",
                harnesses[0]
            ),
            n => format!(
                "Testing {n} harnesses: one real request each; results open when all finish."
            ),
        };
        if !skipped.is_empty() {
            self.ui.message = format!("{skipped} {}", self.ui.message);
        }
        Ok(())
    }
    /// One test of the batch finished; the last one opens the results.
    pub(super) fn harness_tested(
        &mut self,
        batch: u64,
        harness: String,
        result: Result<TestOutcome, String>,
    ) {
        let tests = &mut self.harnesses.tests;
        if batch != tests.batch_id {
            return;
        }
        let Some(slot) = tests
            .batch
            .iter_mut()
            .find(|(name, result)| *name == harness && result.is_none())
        else {
            return;
        };
        let passed = matches!(&result, Ok(outcome) if outcome.passed);
        slot.1 = Some(result);
        tests.states.insert(
            harness.clone(),
            if passed {
                LiveTest::Passed
            } else {
                LiveTest::Failed
            },
        );
        let left = tests.running().len();
        if left > 0 {
            self.ui.message = format!(
                "{harness} {}; {left} still testing.",
                if passed { "passed" } else { "failed" }
            );
            return;
        }
        let batch = std::mem::take(&mut tests.batch);
        let mut passes = 0;
        for (name, result) in &batch {
            // Failures are marked even if their marks were cleared meanwhile.
            if matches!(result, Some(Ok(outcome)) if outcome.passed) {
                passes += 1;
                tests.marked.remove(name);
            } else {
                tests.marked.insert(name.clone());
            }
        }
        let fails = batch.len() - passes;
        self.ui.message = match (batch.len(), fails) {
            (1, 0) => format!("Test of {harness} passed."),
            (1, _) => format!("Test of {harness} failed."),
            (n, 0) => format!("All {n} harness tests passed."),
            (n, fails) => format!("{fails} of {n} harness tests failed."),
        };
        let (title, body) = match batch.as_slice() {
            [(name, Some(result))] => (format!("Test · {name}"), body_of(result)),
            _ => ("Harness tests".to_owned(), summary(&batch, passes)),
        };
        self.text(&title, body);
        // Passing tests saved manifests that doctor reads: refresh TESTED.
        if passes > 0 {
            self.harnesses
                .checks
                .refresh(&self.editor.settings, &self.project, &self.tasks);
        }
    }
}

fn body_of(result: &Result<TestOutcome, String>) -> String {
    match result {
        Ok(outcome) => outcome.report.clone(),
        Err(error) => format!("✗ Could not run the test: {error}\n"),
    }
}

/// The batch dialog: a line per harness, then each report.
fn summary(batch: &[(String, Option<Result<TestOutcome, String>>)], passes: usize) -> String {
    let fails = batch.len() - passes;
    let mut text = format!("{passes} passed, {fails} failed.");
    if fails > 0 {
        text.push_str(" Failed harnesses stay marked; [t] retries them.");
    }
    text.push_str("\n\n");
    let width = batch.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    for (name, result) in batch {
        let line = match result {
            Some(Ok(outcome)) => outcome.report.lines().next().unwrap_or_default().to_owned(),
            Some(Err(error)) => format!("✗ Could not run: {error}"),
            None => "still running".into(),
        };
        text.push_str(&format!("{name:<width$}  {line}\n"));
    }
    for (name, result) in batch {
        if let Some(result) = result {
            text.push_str(&format!("\n── {name} ──\n{}", body_of(result)));
        }
    }
    text
}

/// The running executable. On Linux a rebuild while `mr` runs leaves the
/// path with a ` (deleted)` suffix; the rebuilt file is at the plain path.
fn running_router() -> Result<PathBuf> {
    let path = std::env::current_exe()?;
    Ok(path
        .to_str()
        .and_then(|p| p.strip_suffix(" (deleted)"))
        .map_or(path.clone(), PathBuf::from))
}

fn details(harness: &str, report: Option<&Report>) -> String {
    let mut text = String::new();
    match report {
        None => {
            text.push_str("Still checking this harness. Details appear when the check finishes.\n")
        }
        Some(r) => {
            text.push_str(&format!(
                "Executable: {}\nVersion: {} ({})\nTested versions: {}\nCapabilities: {}\n",
                r.path.as_ref().map_or_else(
                    || format!("{} (not found)", r.program),
                    |p| p.display().to_string()
                ),
                r.version.as_deref().unwrap_or("-"),
                r.tested_label(),
                if r.tested_versions.is_empty() {
                    "-".into()
                } else {
                    r.tested_versions.join(", ")
                },
                r.summary
            ));
            for c in r.capabilities.iter().flatten() {
                let modes: Vec<_> = c.modes.iter().map(|m| m.label()).collect();
                text.push_str(&format!(
                    "  {} {} ({}) · {}\n",
                    match c.support {
                        Support::Supported => "✓ supported",
                        Support::Missing => "✗ missing  ",
                        Support::Unknown => "? unknown  ",
                    },
                    c.flag,
                    modes.join(", "),
                    c.help
                ));
            }
            let notes: Vec<_> = r.notes.iter().filter(|n| *n != doctor::AUTH_NOTE).collect();
            if !notes.is_empty() {
                text.push_str("\nNotes:\n");
                for note in notes {
                    text.push_str(&format!("- {note}\n"));
                }
            }
        }
    }
    text.push_str(&format!(
        "\n{}\n\nSelect this harness with [h] on the workflow page, then press [m] to load model selections. Use [c] on Workflow to configure the selected stage, including permissions and extra arguments.\n\nPermissions inherit the harness configuration unless overridden for a stage.\n\n{}\n\nCustom adapters are defined in TOML. See README.md for the template contract.",
        doctor::AUTH_NOTE,
        match crate::harness::get(harness) {
            Some(spec) if matches!(spec.model, crate::harness::ModelPass::Mode(_)) => format!(
                "{} uses mode keys. A mode determines its model and tools.",
                spec.display
            ),
            _ => "A model must be supported by the selected harness/provider.".into(),
        }
    ));
    if let Some(spec) = crate::harness::get(harness) {
        text.push_str(&format!(
            "\n{}: {} `model-router harnesses --spec {harness}` prints its full spec.\n",
            spec.display, spec.notes
        ));
    }
    text
}

/// Truncates to `width` columns with an ellipsis.
fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.into();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

pub(super) fn draw_harnesses(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    palette: Palette,
    marks: bool,
) {
    let checks = &app.harnesses.checks;
    let tests = &app.harnesses.tests;
    let wide = area.width >= 110;
    let names = app.harnesses.names(&app.editor.settings);
    let name_width = names.iter().map(|n| n.chars().count()).max().unwrap_or(7);
    let name_width = name_width.clamp(7, 14);
    let version_width = if area.width >= 140 { 22 } else { 10 };
    // Borders, padding, highlight symbol, and column gaps.
    let fixed = 4
        + 1
        + name_width
        + 11
        + version_width
        + 8
        + if wide { 5 } else { 4 }
        + if marks { 4 } else { 0 };
    let rest = (area.width as usize).saturating_sub(fixed);
    let (caps_width, path_width) = if wide {
        let caps = rest.min(48);
        (caps, rest.saturating_sub(caps + 1))
    } else {
        (rest, 0)
    };
    let styled = |text: String, color: Color| Cell::from(text).style(Style::default().fg(color));
    let mark = |name: &str| {
        let marked = tests.marked.contains(name);
        styled(
            if marked { "[x]" } else { "[ ]" }.into(),
            if marked {
                palette.accent
            } else {
                palette.muted
            },
        )
    };
    let rows = names.iter().map(|name| {
        let Some(r) = checks.report(name) else {
            let mut cells = vec![];
            if marks {
                cells.push(mark(name));
            }
            return Row::new(
                cells.into_iter().chain([
                    Cell::from(clip(name, name_width)),
                    styled(
                        if checks.checking {
                            "checking…"
                        } else {
                            "not checked"
                        }
                        .into(),
                        palette.muted,
                    ),
                    Cell::from(""),
                    match tests.states.get(name) {
                        Some(LiveTest::Running) => styled("testing…".into(), palette.accent),
                        Some(LiveTest::Failed) => styled("failed".into(), palette.red),
                        Some(LiveTest::Passed) => styled("passed".into(), palette.success),
                        None => Cell::from(""),
                    },
                ]),
            );
        };
        let installed = r.installed();
        let version = r.version_label();
        // This session's live test shows until a fresh check says `tested`.
        let (tested, tested_color) = match tests.states.get(name) {
            Some(LiveTest::Running) => ("testing…", palette.accent),
            Some(LiveTest::Failed) => ("failed", palette.red),
            Some(LiveTest::Passed) if r.tested_label() != "tested" => ("passed", palette.success),
            _ => (
                r.tested_label(),
                match r.version_tested {
                    Some(true) => palette.accent,
                    Some(false) => palette.warn,
                    None => palette.muted,
                },
            ),
        };
        let mut cells = vec![];
        if marks {
            cells.push(mark(name));
        }
        cells.extend([
            Cell::from(clip(name, name_width)),
            if installed {
                styled("● installed".into(), palette.accent)
            } else {
                styled("○ not found".into(), palette.muted)
            },
            styled(
                clip(&version, version_width),
                if r.version.is_some() {
                    palette.fg
                } else {
                    palette.muted
                },
            ),
            styled(tested.into(), tested_color),
            styled(
                clip(&r.summary, caps_width),
                if r.missing_flags() {
                    palette.warn
                } else if r.summary == "ok" {
                    palette.accent
                } else {
                    palette.muted
                },
            ),
        ]);
        if wide {
            cells.push(styled(
                r.path.as_ref().map_or_else(
                    || clip(&r.program, path_width),
                    |p| short_path(p, path_width),
                ),
                palette.muted,
            ));
        }
        Row::new(cells).style(Style::default().fg(if installed {
            palette.fg
        } else {
            palette.muted
        }))
    });
    let running = tests.running();
    let note = match running.as_slice() {
        [] if checks.checking => " checking… ".to_owned(),
        [] if !tests.marked.is_empty() => {
            format!(" {} marked · [t] tests them ", tests.marked.len())
        }
        [] => " uses existing logins and provider settings ".to_owned(),
        [harness] => format!(" testing {harness}… "),
        many => format!(" testing {} harnesses… ", many.len()),
    };
    let block = palette
        .panel(" Harnesses ")
        .title(Line::styled(note, Style::default().fg(palette.muted)).right_aligned());
    let mut widths = vec![];
    let mut header = vec![];
    if marks {
        widths.push(Constraint::Length(3));
        header.push("");
    }
    widths.extend([
        Constraint::Length(name_width as u16),
        Constraint::Length(11),
        Constraint::Length(version_width as u16),
        Constraint::Length(8),
        Constraint::Length(caps_width as u16),
    ]);
    header.extend(["HARNESS", "STATUS", "VERSION", "TESTED", "CAPABILITIES"]);
    if wide {
        widths.push(Constraint::Fill(1));
        header.push("PATH");
    }
    frame.render_stateful_widget(
        Table::new(rows, widths)
            .header(
                Row::new(header)
                    .style(palette.header_style())
                    .bottom_margin(1),
            )
            .block(block)
            .highlight_symbol("▌")
            .highlight_spacing(HighlightSpacing::Always)
            .row_highlight_style(palette.selected_style()),
        area,
        &mut TableState::default().with_selected(Some(app.harnesses.selected)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Mode,
        doctor::{AUTH_NOTE, Capability},
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::{
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    fn capability(flag: &str, modes: &[Mode], support: Support) -> Capability {
        Capability {
            flag: flag.into(),
            modes: modes.to_vec(),
            help: "opencode run --help".into(),
            support,
        }
    }

    fn report(harness: &str, path: Option<&str>) -> Report {
        Report {
            harness: harness.into(),
            builtin: true,
            program: harness.into(),
            path: path.map(PathBuf::from),
            version: None,
            tested_versions: vec![],
            version_tested: None,
            version_captured: false,
            version_pin: None,
            version_pin_ok: None,
            auth_hint: None,
            capabilities: None,
            summary: "not checked".into(),
            notes: vec![AUTH_NOTE.into()],
        }
    }

    fn stub(_: &Settings, _: &Path) -> Vec<Report> {
        let both = [Mode::Unattended, Mode::Interactive];
        let mut codex = report("codex", Some("/usr/bin/codex"));
        codex.version = Some("codex-cli 0.160.0".into());
        codex.tested_versions = vec!["0.160.0".into()];
        codex.version_tested = Some(true);
        codex.capabilities = Some(vec![capability("--model", &both, Support::Supported)]);
        codex.summary = "ok".into();
        let mut opencode = report("opencode", Some("/usr/bin/opencode"));
        opencode.version = Some("opencode v2.0.22".into());
        opencode.tested_versions = vec!["1.4.3".into()];
        opencode.version_tested = Some(false);
        opencode.capabilities = Some(vec![
            capability("--model", &both, Support::Supported),
            capability("--variant", &both, Support::Missing),
            capability("--interactive", &[Mode::Interactive], Support::Missing),
        ]);
        opencode.summary = "missing: --variant, --interactive (interactive)".into();
        opencode
            .notes
            .push("Detected version is not a parser-tested version.".into());
        vec![codex, opencode, report("gemini", None)]
    }

    fn app() -> App {
        let mut app = App::new(
            std::env::temp_dir(),
            Settings::default(),
            PathBuf::from("/tmp/config"),
            Arc::new(AtomicBool::new(false)),
        );
        app.harnesses.checks.probe = stub;
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        app.key(KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
    }

    fn wait(app: &mut App) {
        let start = Instant::now();
        while app.harnesses.checks.checking {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "check never finished"
            );
            app.poll().unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Rendered rows, and the foreground color where `needle` starts.
    fn render(app: &mut App, width: u16, needle: &str) -> (String, Option<Color>) {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 24)).unwrap();
        terminal
            .draw(|frame| super::super::app::draw(frame, app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        let mut color = None;
        for y in 0..buffer.area.height {
            let cells: Vec<_> = (0..buffer.area.width).map(|x| &buffer[(x, y)]).collect();
            let row: String = cells.iter().map(|c| c.symbol()).collect();
            if let Some(index) = row.find(needle) {
                let column = row[..index].chars().count();
                color = color.or(Some(cells[column].fg));
            }
            text.push_str(&row);
            text.push('\n');
        }
        (text, color)
    }

    #[test]
    fn page_checks_in_background_and_shows_doctor_columns() {
        let mut app = app();
        let palette = app.ui.palette();
        press(&mut app, KeyCode::Char('3'));
        assert!(app.harnesses.checks.checking);
        let (text, _) = render(&mut app, 80, "checking…");
        assert!(
            text.contains("codex") && text.contains("checking…"),
            "{text}"
        );
        wait(&mut app);
        assert_eq!(app.harnesses.checks.reports.len(), 3);

        let (text, color) = render(&mut app, 80, "missing: --variant");
        assert_eq!(color, Some(palette.warn), "{text}");
        assert!(
            text.contains("2.0.22") && text.contains("untested"),
            "{text}"
        );
        assert!(
            text.contains("○ not found") && !text.contains("/usr/bin"),
            "{text}"
        );
        assert_eq!(render(&mut app, 80, "untested").1, Some(palette.warn));
        assert_eq!(render(&mut app, 80, "ok ").1, Some(palette.accent));
        assert_eq!(render(&mut app, 80, "○ not found").1, Some(palette.muted));
        let (text, _) = render(&mut app, 140, "PATH");
        assert!(
            text.contains("missing: --variant, --interactive (interactive)")
                && text.contains("/usr/bin/opencode"),
            "{text}"
        );

        // Refresh keeps old rows until fresh results arrive.
        app.harnesses.checks.reports.truncate(1);
        press(&mut app, KeyCode::Char('r'));
        assert!(app.harnesses.checks.checking);
        assert_eq!(app.harnesses.checks.reports.len(), 1);
        wait(&mut app);
        assert_eq!(app.harnesses.checks.reports.len(), 3);
    }

    #[test]
    fn details_list_capabilities_notes_and_auth() {
        let mut app = app();
        press(&mut app, KeyCode::Char('3'));
        press(&mut app, KeyCode::Enter);
        let Some(super::super::modal::Modal::Text { body, .. }) = app.ui.dialog() else {
            panic!("details did not open");
        };
        assert!(body.contains("Still checking"), "{body}");
        app.ui.dialogs.clear();
        wait(&mut app);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        let Some(super::super::modal::Modal::Text { title, body, .. }) = app.ui.dialog() else {
            panic!("details did not open");
        };
        assert_eq!(title, "opencode");
        for expected in [
            "Executable: /usr/bin/opencode",
            "Version: opencode v2.0.22 (untested)",
            "Tested versions: 1.4.3",
            "✗ missing   --variant (unattended, interactive) · opencode run --help",
            "✗ missing   --interactive (interactive)",
            "✓ supported --model",
            "- Detected version is not a parser-tested version.",
            "[h] on the workflow page",
        ] {
            assert!(body.contains(expected), "{expected}\n{body}");
        }
        assert_eq!(body.matches(AUTH_NOTE).count(), 1);
    }

    /// opencode fails; others pass.
    fn stub_test(test: &HarnessTest) -> Result<TestOutcome, String> {
        let passed = test.harness != "opencode";
        Ok(TestOutcome {
            passed,
            report: test_report(
                test,
                passed,
                if passed {
                    "exit status: 0"
                } else {
                    "exit status: 1"
                },
                "Saved fixture.",
            ),
        })
    }

    fn finish_batch(app: &mut App) {
        let start = Instant::now();
        while !app.harnesses.tests.batch.is_empty() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "tests never finished"
            );
            app.poll().unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The app on Harnesses, checked, in a model-router checkout.
    fn checkout() -> (tempfile::TempDir, App) {
        let project = tempfile::tempdir().unwrap();
        let script = project.path().join(TEST_SCRIPT);
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::write(&script, TEST_SCRIPT_BYTES).unwrap();
        let mut app = App::new(
            project.path().into(),
            Settings::default(),
            project.path().join("config.toml"),
            Arc::new(AtomicBool::new(false)),
        );
        app.ui.theme_path = project.path().join("appearance.json");
        app.harnesses.checks.probe = stub;
        app.harnesses.tests.runner = stub_test;
        press(&mut app, KeyCode::Char('3'));
        wait(&mut app);
        (project, app)
    }

    #[test]
    fn marked_harnesses_test_as_one_batch_and_failures_stay_marked() {
        static PROBES: AtomicUsize = AtomicUsize::new(0);
        let (_project, mut app) = checkout();
        app.harnesses.checks.probe = |settings, project| {
            PROBES.fetch_add(1, Ordering::SeqCst);
            stub(settings, project)
        };
        // The test keys fit the footer at 80 columns.
        let (text, _) = render(&mut app, 80, "[x] mark");
        assert!(
            text.contains("[t] test") && text.contains("[x] mark") && text.contains("[ ] codex"),
            "{text}"
        );

        // gemini is not installed, so it cannot be marked.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(app.ui.message, "gemini is not installed.");
        assert!(app.harnesses.tests.marked.is_empty());
        // [a] marks every installed harness, then clears them.
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(
            app.harnesses.tests.marked.iter().collect::<Vec<_>>(),
            ["codex", "opencode"]
        );
        press(&mut app, KeyCode::Char('a'));
        assert!(app.harnesses.tests.marked.is_empty());
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Char('x'));
        let (text, _) = render(&mut app, 100, "marked");
        assert!(
            text.contains("[x] codex") && text.contains("2 marked · [t] tests them"),
            "{text}"
        );

        // [t] tests the marks, not the selection, in display order.
        press(&mut app, KeyCode::Char('t'));
        let Some(Modal::Ask(ask)) = app.ui.dialog() else {
            panic!("no confirmation");
        };
        let Ask::TestHarness { harnesses } = &**ask else {
            panic!("wrong question");
        };
        assert_eq!(harnesses, &["codex", "opencode"]);
        let (text, _) = render(&mut app, 100, "2 requests");
        assert!(text.contains("2 requests"), "{text}");
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(app.harnesses.tests.running(), ["codex", "opencode"]);
        let (text, _) = render(&mut app, 100, "testing 2");
        assert!(
            text.contains("testing 2 harnesses…") && text.contains("testing…"),
            "{text}"
        );
        finish_batch(&mut app);
        assert_eq!(app.ui.message, "1 of 2 harness tests failed.");
        // Passing tests saved manifests: TESTED refreshes from doctor.
        wait(&mut app);
        assert_eq!(PROBES.load(Ordering::SeqCst), 1);
        let Some(Modal::Text { title, body, .. }) = app.ui.dialog() else {
            panic!("results did not open");
        };
        assert_eq!(title, "Harness tests");
        for expected in [
            "1 passed, 1 failed. Failed harnesses stay marked; [t] retries them.",
            "codex     ✓ Passed: exit status: 0",
            "opencode  ✗ Failed: exit status: 1",
            "── codex ──",
            "── opencode ──",
        ] {
            assert!(body.contains(expected), "{expected}\n{body}");
        }
        assert_eq!(
            app.harnesses.tests.marked.iter().collect::<Vec<_>>(),
            ["opencode"]
        );
        app.ui.dialogs.clear();
        wait(&mut app);
        let (text, color) = render(&mut app, 100, "failed");
        assert_eq!(color, Some(app.ui.palette().red), "{text}");
        // The stub check still calls codex's version schema-derived, so the
        // live pass shows.
        assert!(text.contains("passed"), "{text}");
    }

    #[test]
    fn failures_stay_marked_even_if_marks_were_cleared_during_the_batch() {
        let (_project, mut app) = checkout();
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Char('t'));
        press(&mut app, KeyCode::Char('y'));
        press(&mut app, KeyCode::Char('a'));
        assert!(app.harnesses.tests.marked.is_empty());
        finish_batch(&mut app);
        assert_eq!(
            app.harnesses.tests.marked.iter().collect::<Vec<_>>(),
            ["opencode"]
        );
    }

    #[test]
    fn untrusted_modified_capture_script_cannot_run() {
        let (project, mut app) = checkout();
        assert!(app.can_test_harnesses());
        std::fs::write(project.path().join(TEST_SCRIPT), "print('pwned')\n").unwrap();
        assert!(!app.can_test_harnesses());
        app.start_harness_tests(vec!["codex".into()]).unwrap();
        assert!(app.harnesses.tests.batch.is_empty());
        assert!(
            app.ui.message.contains("does not match this build"),
            "{}",
            app.ui.message
        );
    }

    #[test]
    fn a_check_running_when_tests_pass_is_followed_by_another() {
        static PROBES: AtomicUsize = AtomicUsize::new(0);
        let (_project, mut app) = checkout();
        app.harnesses.checks.probe = |settings, project| {
            PROBES.fetch_add(1, Ordering::SeqCst);
            stub(settings, project)
        };
        press(&mut app, KeyCode::Char('t'));
        press(&mut app, KeyCode::Char('y'));
        // A check that read the project before codex's manifest was saved.
        app.harnesses.checks.checking = true;
        finish_batch(&mut app);
        assert!(app.harnesses.checks.again);
        assert_eq!(PROBES.load(Ordering::SeqCst), 0);
        let reports = stub(&app.editor.settings, &app.project);
        app.update(app.tasks.generation(), Msg::Checks(Ok(reports)));
        assert!(!app.harnesses.checks.again);
        wait(&mut app);
        assert_eq!(PROBES.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn marks_are_revalidated_and_stale_results_dropped() {
        let (_project, mut app) = checkout();
        // Marked before the check found gemini missing.
        app.harnesses.tests.marked = ["codex", "gemini"].map(String::from).into();
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(
            app.ui.message,
            "Unmarked gemini: not installed or not built in."
        );
        assert!(matches!(
            app.ui.dialog(),
            Some(Modal::Ask(ask)) if matches!(&**ask, Ask::TestHarness { harnesses } if harnesses == &["codex"])
        ));
        // A check that lands while the question is open is applied on [y].
        app.harnesses.checks.reports[0].path = None;
        press(&mut app, KeyCode::Char('y'));
        assert!(app.harnesses.tests.batch.is_empty());
        assert!(app.harnesses.tests.marked.is_empty());
        assert_eq!(
            app.ui.message,
            "Skipped codex: not installed or not built in."
        );
        app.harnesses.checks.reports = stub(&app.editor.settings, &app.project);
        // When others remain, the skip stays in the message beside them.
        app.harnesses.tests.marked = ["codex", "opencode"].map(String::from).into();
        press(&mut app, KeyCode::Char('t'));
        app.harnesses.checks.reports[0].path = None;
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(app.harnesses.tests.running(), ["opencode"]);
        assert!(
            app.ui
                .message
                .starts_with("Skipped codex: not installed or not built in. Testing opencode"),
            "{}",
            app.ui.message
        );
        finish_batch(&mut app);
        app.ui.dialogs.clear();
        app.harnesses.checks.reports = stub(&app.editor.settings, &app.project);

        // [a] drops marks a check found stale.
        app.harnesses.tests.marked = ["gemini"].map(String::from).into();
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(
            app.harnesses.tests.marked.iter().collect::<Vec<_>>(),
            ["codex", "opencode"]
        );

        // Marks wait for check results; without any, [r] is the way on.
        app.harnesses.checks.reports.clear();
        app.harnesses.tests.marked.clear();
        for key in ['x', 'a'] {
            press(&mut app, KeyCode::Char(key));
            assert!(app.harnesses.tests.marked.is_empty());
            assert_eq!(
                app.ui.message,
                "No check results to mark from; [r] rechecks."
            );
        }
        app.harnesses.checks.checking = true;
        press(&mut app, KeyCode::Char('x'));
        assert!(app.ui.message.starts_with("Wait for the harness check"));
        app.harnesses.checks.checking = false;

        // A result from an earlier batch, e.g. of another project, is dropped.
        app.harnesses.tests.batch = vec![("codex".into(), None)];
        app.harnesses.tests.batch_id = 2;
        let outcome = TestOutcome {
            passed: true,
            report: String::new(),
        };
        app.update(
            app.tasks.generation(),
            Msg::HarnessTest {
                batch: 1,
                harness: "codex".into(),
                result: Ok(outcome),
            },
        );
        assert_eq!(app.harnesses.tests.running(), ["codex"]);
    }

    #[test]
    fn test_key_confirms_then_runs_the_script_in_the_background() {
        let project = tempfile::tempdir().unwrap();
        let mut app = App::new(
            project.path().into(),
            Settings::default(),
            project.path().join("config.toml"),
            Arc::new(AtomicBool::new(false)),
        );
        app.ui.theme_path = project.path().join("appearance.json");
        app.harnesses.checks.probe = stub;
        app.harnesses.tests.runner = stub_test;
        press(&mut app, KeyCode::Char('3'));
        wait(&mut app);

        // Outside a model-router checkout the key is not offered and explains why.
        assert!(!render(&mut app, 100, "[t] test").0.contains("[t] test"));
        press(&mut app, KeyCode::Char('t'));
        assert!(app.ui.dialog().is_none());
        assert!(app.ui.message.contains(TEST_SCRIPT), "{}", app.ui.message);

        let script = project.path().join(TEST_SCRIPT);
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::write(&script, TEST_SCRIPT_BYTES).unwrap();
        let (text, _) = render(&mut app, 100, "test");
        assert!(text.contains("[t] test"), "{text}");
        // [n] sends nothing.
        press(&mut app, KeyCode::Char('t'));
        assert!(matches!(
            app.ui.dialog(),
            Some(Modal::Ask(ask)) if matches!(&**ask, Ask::TestHarness { harnesses } if harnesses == &["codex"])
        ));
        press(&mut app, KeyCode::Char('n'));
        assert!(app.ui.dialog().is_none() && app.harnesses.tests.batch.is_empty());

        press(&mut app, KeyCode::Char('t'));
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(app.harnesses.tests.running(), ["codex"]);
        assert!(
            render(&mut app, 100, "testing codex")
                .0
                .contains("testing codex…")
        );
        // A second test waits for the first.
        app.harnesses.tests.runner = |_| Err("second test ran".into());
        press(&mut app, KeyCode::Char('t'));
        assert!(app.ui.dialog().is_none());
        assert!(
            app.ui.message.starts_with("Still testing codex"),
            "{}",
            app.ui.message
        );
        finish_batch(&mut app);
        assert_eq!(app.ui.message, "Test of codex passed.");
        let Some(Modal::Text { title, body, .. }) = app.ui.dialog() else {
            panic!("result did not open");
        };
        assert_eq!(title, "Test · codex");
        for expected in [
            "✓ Passed: exit status: 0",
            "capture_harness_output.py codex --confirm-paid-request --router",
            "--output-dir",
            ".model-router/harness-tests/codex-",
            "Saved fixture.",
            "append the [[fixture]] entry from",
            "cargo test --test harness_output",
        ] {
            assert!(body.contains(expected), "{expected}\n{body}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_test_reports_the_script_exit_and_both_streams() {
        let project = tempfile::tempdir().unwrap();
        let script = project.path().join(TEST_SCRIPT);
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::write(
            &script,
            "import sys\nprint('to stdout')\nsys.stderr.write('to stderr')\nsys.exit(1)\n",
        )
        .unwrap();
        let test = HarnessTest {
            harness: "codex".into(),
            project: project.path().into(),
            router: PathBuf::from("/bin/true"),
            output: project.path().join("out"),
        };
        // `run_test` itself refuses scripts that do not match the build.
        assert!(run_test(&test).is_err());
        let outcome = run_test_unchecked(&test).unwrap();
        assert!(!outcome.passed);
        let body = outcome.report;
        assert!(body.starts_with("✗ Failed: exit status: 1"), "{body}");
        assert!(body.contains("to stderr\nto stdout"), "{body}");
        assert!(!body.contains("cargo test"), "{body}");
    }

    #[cfg(unix)]
    #[test]
    fn python_isolated_mode_blocks_stdlib_shadowing_in_scripts_dir() {
        let project = tempfile::tempdir().unwrap();
        let script = project.path().join(TEST_SCRIPT);
        let scripts_dir = script.parent().unwrap();
        std::fs::create_dir_all(scripts_dir).unwrap();
        std::fs::write(&script, "import json\nprint(json.dumps({'ok': True}))\n").unwrap();
        // A malicious stdlib shadow beside the script must not be imported under -I.
        std::fs::write(
            scripts_dir.join("json.py"),
            "raise RuntimeError('shadowed stdlib imported')\n",
        )
        .unwrap();
        let test = HarnessTest {
            harness: "codex".into(),
            project: project.path().into(),
            router: PathBuf::from("/bin/true"),
            output: project.path().join("out"),
        };
        let outcome = run_test_unchecked(&test).unwrap();
        assert!(outcome.passed, "{}", outcome.report);
        assert!(
            outcome.report.contains("{\"ok\": true}"),
            "{}",
            outcome.report
        );
    }

    #[test]
    fn failed_test_report_shows_the_output_without_fixture_advice() {
        let test = HarnessTest {
            harness: "claude".into(),
            project: PathBuf::from("/repo"),
            router: PathBuf::from("/repo/target/debug/model-router"),
            output: PathBuf::from("/repo/.model-router/harness-tests/claude-1"),
        };
        let body = test_report(
            &test,
            false,
            "exit status: 1",
            "\x1b[31mRun failed\x1b[0m\n",
        );
        assert!(body.starts_with("✗ Failed: exit status: 1"), "{body}");
        assert!(
            body.contains("\nRun failed\n") && !body.contains('\x1b'),
            "{body}"
        );
        assert!(!body.contains("cargo test"), "{body}");
        let argv: Vec<_> = test
            .argv()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv[1..5],
            [
                "-I",
                "/repo/scripts/capture_harness_output.py",
                "claude",
                "--confirm-paid-request"
            ]
        );
    }

    #[test]
    fn workflow_warns_only_from_cached_results() {
        let mut app = app();
        app.editor.stage_mut().harness = "opencode".into();
        let warning = "⚠ opencode 2.0.22 lacks --variant, --interactive; see Harnesses [3]";
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(260, 40)).unwrap();
        let mut draw = |app: &mut App| {
            terminal
                .draw(|frame| super::super::app::draw(frame, app))
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };
        assert!(!draw(&mut app).contains("lacks"));
        assert!(
            !app.harnesses.checks.checking,
            "Workflow must not run doctor"
        );
        app.harnesses.checks.reports = stub(&app.editor.settings, &app.project);
        assert!(draw(&mut app).contains(warning));
        app.editor.stage_mut().harness = "codex".into();
        assert!(!draw(&mut app).contains("lacks"));
    }
}
