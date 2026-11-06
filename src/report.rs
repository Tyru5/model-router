//! Completion summaries: what a run changed, checked, left unresolved, and
//! spent. `build` and `render` are pure; `changes` and `diff` run Git.
use crate::{
    config::Role,
    engine::{Attempt, Run, Status},
    handoff::{Severity, TestStatus, Usage},
    snapshot::{self, FileChange},
    transitions::{Level, Record},
};
use anyhow::{Result, anyhow};
use chrono::Utc;
use serde::Serialize;
use std::{fmt::Write, fs, path::Path};

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub id: String,
    pub workflow: String,
    pub status: Status,
    pub error: Option<String>,
    /// From creation to the end of the latest attempt.
    pub wall_seconds: i64,
    /// Sum of attempt durations.
    pub active_seconds: i64,
    pub attempts: usize,
    pub retries: usize,
    pub repair_passes: u32,
    pub max_repair_passes: u32,
    pub usage: UsageTotal,
    pub stages: Vec<StageReport>,
    /// Most severe first.
    pub unresolved: Vec<Item>,
    /// `files_changed` claimed by the final handoffs; unverified.
    pub reported_files: Vec<String>,
    /// The latest review summary stage's brief, Markdown; also review.md.
    pub review: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StageReport {
    pub name: String,
    pub role: Role,
    pub harness: String,
    /// The latest attempt's model; empty for the harness default.
    pub model: String,
    pub status: Status,
    pub attempts: usize,
    pub retries: usize,
    pub active_seconds: i64,
    pub usage: UsageTotal,
    /// Router checks from the latest attempt, then model-reported tests.
    pub checks: Vec<CheckLine>,
    /// Accepted not-applicable reason.
    pub waiver: Option<String>,
}

/// Sums of what attempts reported. A field stays None when no attempt
/// reported it; unreported attempts are counted, never estimated.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct UsageTotal {
    pub sum: Usage,
    pub attempts: usize,
    /// Attempts that reported any usage, and any cost.
    pub reported: usize,
    pub cost_reported: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    Router,
    ModelReported,
}
impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Router => "router",
            Self::ModelReported => "model-reported",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckLine {
    pub command: String,
    pub status: TestStatus,
    pub required: bool,
    pub source: Source,
    /// Why a router check did not pass, e.g. "exit 3".
    pub detail: Option<String>,
}

/// Unresolved item kinds, most severe first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Error,
    Blocking,
    FailedCheck,
    Unresolved,
    Waiver,
    OptionalCheck,
    Unstructured,
    Advisory,
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub kind: ItemKind,
    /// None for the run's own error.
    pub stage: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Changes {
    /// Files changed during this run, from workspace snapshots.
    Tracked {
        files: Vec<FileChange>,
    },
    NotGit,
    Unavailable {
        reason: String,
    },
}

pub fn build(run: &Run) -> RunReport {
    // Only an unfinished attempt reads the clock.
    let now = Utc::now();
    let seconds = |a: &Attempt| {
        (a.finished_at.unwrap_or(now) - a.started_at)
            .num_seconds()
            .max(0)
    };
    let mut unresolved = vec![];
    if let Some(error) = &run.error {
        unresolved.push(Item {
            kind: ItemKind::Error,
            stage: None,
            text: error.clone(),
        });
    }
    let mut reported_files: Vec<String> = vec![];
    let mut stages = vec![];
    for (stage, state) in run.workflow.stages.iter().zip(&run.stages) {
        let last = state.attempts.last();
        let mut checks: Vec<_> = last
            .map_or(&[][..], |a| &a.checks)
            .iter()
            .map(|c| CheckLine {
                command: shell_words::join(&c.command),
                status: c.status,
                required: c.required,
                source: Source::Router,
                detail: (c.status != TestStatus::Passed).then(|| c.outcome()),
            })
            .collect();
        let mut item = |kind, text: String| {
            unresolved.push(Item {
                kind,
                stage: Some(stage.name.clone()),
                text,
            })
        };
        if let Some(handoff) = &state.handoff {
            checks.extend(handoff.tests.iter().map(|t| CheckLine {
                command: t.command.clone(),
                status: t.status,
                required: t.required,
                source: Source::ModelReported,
                detail: None,
            }));
            for finding in &handoff.findings {
                match finding.severity {
                    Severity::Blocking => item(
                        ItemKind::Blocking,
                        format!("blocking finding: {}", finding.description),
                    ),
                    Severity::Advisory => item(
                        ItemKind::Advisory,
                        format!("advisory: {}", finding.description),
                    ),
                }
            }
            for text in &handoff.unresolved {
                item(ItemKind::Unresolved, format!("unresolved: {text}"));
            }
            if !handoff.structured {
                item(
                    ItemKind::Unstructured,
                    "handoff is unstructured text".into(),
                );
            }
            for file in &handoff.files_changed {
                if !reported_files.contains(file) {
                    reported_files.push(file.clone());
                }
            }
        }
        for check in checks.iter().filter(|c| c.status != TestStatus::Passed) {
            let kind = if check.required {
                ItemKind::FailedCheck
            } else {
                ItemKind::OptionalCheck
            };
            item(
                kind,
                format!(
                    "{} {} check {}: {}{}",
                    if check.required {
                        "required"
                    } else {
                        "optional"
                    },
                    check.source.label(),
                    if check.status == TestStatus::NotRun {
                        "not run"
                    } else {
                        "failed"
                    },
                    check.command,
                    check
                        .detail
                        .as_ref()
                        .map(|d| format!(" ({d})"))
                        .unwrap_or_default()
                ),
            );
        }
        if let Some(waiver) = &state.waiver {
            item(
                ItemKind::Waiver,
                format!("verification not applicable: {}", waiver.reason),
            );
        }
        stages.push(StageReport {
            name: stage.name.clone(),
            role: stage.role,
            harness: stage.harness.clone(),
            model: last.map_or_else(|| stage.model.clone(), |a| a.model.clone()),
            status: state.status,
            attempts: state.attempts.len(),
            retries: state.attempts.len().saturating_sub(1),
            active_seconds: state.attempts.iter().map(seconds).sum(),
            usage: total(&state.attempts),
            checks,
            waiver: state.waiver.as_ref().map(|w| w.reason.clone()),
        });
    }
    // Stable, so each kind keeps stage order.
    unresolved.sort_by_key(|item| item.kind);
    let attempts = || run.stages.iter().flat_map(|s| &s.attempts);
    let end = attempts()
        .map(|a| a.finished_at.unwrap_or(now))
        .max()
        .unwrap_or(run.updated_at);
    RunReport {
        id: run.id.clone(),
        workflow: run.workflow.name.clone(),
        status: run.status,
        error: run.error.clone(),
        wall_seconds: (end - run.created_at).num_seconds().max(0),
        active_seconds: stages.iter().map(|s| s.active_seconds).sum(),
        attempts: attempts().count(),
        retries: stages.iter().map(|s| s.retries).sum(),
        repair_passes: run.repair_passes,
        max_repair_passes: run.workflow.max_repair_passes,
        usage: total(attempts()),
        stages,
        unresolved,
        reported_files,
        review: run
            .workflow
            .stages
            .iter()
            .zip(&run.stages)
            .rev()
            .filter(|(stage, _)| stage.role == Role::Summary)
            .find_map(|(_, state)| state.handoff.as_ref())
            .map(|handoff| handoff.summary.trim().to_owned()),
    }
}

fn total<'a>(attempts: impl IntoIterator<Item = &'a Attempt>) -> UsageTotal {
    let mut total = UsageTotal::default();
    for attempt in attempts {
        total.attempts += 1;
        if let Some(usage) = attempt.usage {
            total.reported += 1;
            total.cost_reported += usize::from(usage.cost_usd.is_some());
            total.sum = total.sum + usage;
        }
    }
    total
}

/// Git runs in the workspace, or in the project once a worktree is removed;
/// both share the snapshot objects.
fn git_dir(run: &Run) -> &Path {
    if run.workspace.is_dir() {
        &run.workspace
    } else {
        &run.project
    }
}

/// Files changed during the run, excluding changes made before it started.
pub fn changes(run: &Run) -> Changes {
    let dir = git_dir(run);
    match run.change_range() {
        Ok((from, to)) => match snapshot::changes(dir, from, to) {
            Ok(files) => Changes::Tracked { files },
            Err(error) => Changes::Unavailable {
                reason: format!("{error:#}"),
            },
        },
        Err(_) if !snapshot::is_repository(dir) => Changes::NotGit,
        Err(reason) => Changes::Unavailable {
            reason: reason.into(),
        },
    }
}

/// The patch of changes made during the run, or the copy saved when it
/// stopped if Git can no longer produce one.
pub fn diff(run: &Run) -> Result<String> {
    let dir = git_dir(run);
    let live = match run.change_range() {
        Ok((from, to)) => snapshot::patch(dir, from, to, snapshot::PATCH_LIMIT),
        Err(_) if !snapshot::is_repository(dir) => {
            Err(anyhow!("Changes are unavailable (not a Git repository)"))
        }
        Err(reason) => Err(anyhow!("Changes are unavailable: {reason}")),
    };
    live.or_else(|error| fs::read_to_string(run.directory.join("changes.diff")).map_err(|_| error))
}

/// The run's saved status changes, oldest first.
pub fn transitions(run: &Run) -> Result<Vec<Record>> {
    crate::transitions::read(&run.directory)
}

/// The status timeline, one change per line: time, run revision, what
/// changed, from → to, who, and why.
pub fn render_transitions(run: &Run, records: &[Record]) -> String {
    let mut text = String::from("Transitions\n");
    if records.is_empty() {
        text.push_str("  none recorded\n");
        return text;
    }
    for record in records {
        let stage = record
            .stage_index
            .and_then(|i| run.workflow.stages.get(i))
            .map_or_else(String::new, |s| s.name.clone());
        let stage = match &record.candidate {
            Some(candidate) => format!("{stage}/{candidate}"),
            None => stage,
        };
        let subject = match (record.level, record.attempt) {
            (Level::Run, _) => "run".to_owned(),
            (Level::Stage | Level::Candidate, _) => stage,
            (Level::Attempt, Some(number)) => format!("{stage} #{number}"),
            (Level::Attempt, None) => format!("{stage} #?"),
        };
        let _ = writeln!(
            text,
            "  {}  r{:<3} {:<20} {} → {}  [{}] {}",
            record.at.format("%Y-%m-%d %H:%M:%S"),
            record.run_revision,
            clip(&subject, 20),
            record.from.as_str(),
            record.to.as_str(),
            record.actor.label(),
            record.reason.lines().next().unwrap_or("")
        );
    }
    text
}

fn clip(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(chars - 1).collect::<String>())
    }
}

pub fn duration(seconds: i64) -> String {
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
    }
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

pub fn tokens(n: u64) -> String {
    match n {
        n if n < 1_000 => n.to_string(),
        n if n < 10_000 => format!("{:.1}k", n as f64 / 1e3),
        n if n < 1_000_000 => format!("{}k", (n + 500) / 1_000),
        n => format!("{:.1}M", n as f64 / 1e6),
    }
}

pub fn dollars(cost: f64) -> String {
    if cost == 0.0 || cost >= 0.01 {
        format!("${cost:.2}")
    } else {
        format!("${cost:.4}")
    }
}

/// E.g. "wall 4m 12s · active 3m 50s · 3 attempts · 1 retry · repairs 0/2".
pub fn summary(report: &RunReport) -> String {
    format!(
        "wall {} · active {} · {} · {} · repairs {}/{}",
        duration(report.wall_seconds),
        duration(report.active_seconds),
        count(report.attempts, "attempt", "attempts"),
        count(report.retries, "retry", "retries"),
        report.repair_passes,
        report.max_repair_passes
    )
}

/// E.g. "$0.42 · in 12k · out 3.1k · usage reported by 2 of 3 attempts".
pub fn usage_text(total: &UsageTotal) -> String {
    if total.reported == 0 {
        return "usage not reported".into();
    }
    let sum = &total.sum;
    let mut parts = vec![sum.cost_usd.map_or("cost not reported".into(), dollars)];
    for (label, value) in [
        ("in", sum.input_tokens),
        ("out", sum.output_tokens),
        ("cache read", sum.cache_read_tokens),
        ("cache write", sum.cache_write_tokens),
    ] {
        if let Some(value) = value {
            parts.push(format!("{label} {}", tokens(value)));
        }
    }
    let of = count(total.attempts, "attempt", "attempts");
    if total.reported < total.attempts {
        parts.push(format!("usage reported by {} of {of}", total.reported));
    }
    if total.cost_reported > 0 && total.cost_reported < total.reported {
        parts.push(format!("cost reported by {} of {of}", total.cost_reported));
    }
    parts.join(" · ")
}

/// "+40 -5", or "binary" without line counts.
pub fn counts(file: &FileChange) -> String {
    match (file.added, file.removed) {
        (Some(added), Some(removed)) => format!("+{added} -{removed}"),
        _ => "binary".into(),
    }
}

/// E.g. "3 files, +42 -7".
pub fn files_summary(files: &[FileChange]) -> String {
    let (added, removed) = files.iter().fold((0, 0), |(a, r), f| {
        (a + f.added.unwrap_or(0), r + f.removed.unwrap_or(0))
    });
    format!(
        "{}, +{added} -{removed}",
        count(files.len(), "file", "files")
    )
}

pub fn check_glyph(status: TestStatus) -> &'static str {
    match status {
        TestStatus::Passed => "✓",
        TestStatus::Failed => "✗",
        TestStatus::NotRun => "–",
    }
}

pub fn item_glyph(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Error | ItemKind::FailedCheck => "✗",
        ItemKind::Blocking => "!",
        ItemKind::Unresolved | ItemKind::Waiver | ItemKind::OptionalCheck => "•",
        ItemKind::Unstructured | ItemKind::Advisory => "·",
    }
}

/// "required · router · Verify", plus why a router check did not pass.
pub fn check_detail(check: &CheckLine, stage: &str) -> String {
    let mut text = format!(
        "{} · {} · {stage}",
        if check.required {
            "required"
        } else {
            "optional"
        },
        check.source.label()
    );
    if let Some(detail) = &check.detail {
        text.push_str(&format!(" · {detail}"));
    }
    text
}

pub fn item_text(item: &Item) -> String {
    match &item.stage {
        Some(stage) => format!("{stage}: {}", item.text),
        None => item.text.clone(),
    }
}

/// E.g. "3 files, +42 -7" or "unavailable (not a Git repository)".
pub fn changes_status(changes: &Changes) -> String {
    match changes {
        Changes::Tracked { files } if files.is_empty() => "no files".into(),
        Changes::Tracked { files } => files_summary(files),
        Changes::NotGit => "unavailable (not a Git repository)".into(),
        Changes::Unavailable { reason } => format!("unavailable: {reason}"),
    }
}

pub fn changes_heading(changes: &Changes) -> String {
    format!("Changed during this run: {}", changes_status(changes))
}

const LISTED_FILES: usize = 40;

/// The compact plain-text report printed by the CLI.
pub fn render(report: &RunReport, changes: Option<&Changes>) -> String {
    let mut text = String::new();
    let _ = writeln!(
        text,
        "Status: {} · {}",
        report.status.label(),
        summary(report)
    );
    let _ = writeln!(text, "Usage: {}", usage_text(&report.usage));
    if let Some(error) = &report.error {
        let _ = writeln!(text, "Error: {error}");
    }
    if let Some(review) = &report.review {
        let _ = writeln!(text, "\nReview summary:\n{review}");
    }
    if let Some(changes) = changes {
        let _ = writeln!(text, "\n{}", changes_heading(changes));
        match changes {
            Changes::Tracked { files } => {
                for file in files.iter().take(LISTED_FILES) {
                    let _ = writeln!(
                        text,
                        "  {}  {}  {}",
                        file.status.letter(),
                        file.path,
                        counts(file)
                    );
                }
                if files.len() > LISTED_FILES {
                    let _ = writeln!(text, "  … {} more", files.len() - LISTED_FILES);
                }
            }
            _ if !report.reported_files.is_empty() => {
                let _ = writeln!(
                    text,
                    "  Model-reported (unverified): {}",
                    report.reported_files.join(", ")
                );
            }
            _ => {}
        }
    }
    let checks: Vec<_> = report
        .stages
        .iter()
        .flat_map(|s| s.checks.iter().map(move |c| (c, s.name.as_str())))
        .collect();
    if checks.is_empty() {
        let _ = writeln!(text, "\nChecks: none run or reported");
    } else {
        let _ = writeln!(text, "\nChecks:");
        for (check, stage) in checks {
            let _ = writeln!(
                text,
                "  {} {} · {}",
                check_glyph(check.status),
                check.command,
                check_detail(check, stage)
            );
        }
    }
    if report.unresolved.is_empty() {
        let _ = writeln!(text, "\nUnresolved: none");
    } else {
        let _ = writeln!(text, "\nUnresolved:");
        for item in &report.unresolved {
            let _ = writeln!(text, "  {} {}", item_glyph(item.kind), item_text(item));
        }
    }
    let _ = writeln!(text, "\nStages:");
    let width = report
        .stages
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(0);
    for stage in &report.stages {
        let _ = write!(text, "  {:<width$}  {}", stage.name, stage.status.label());
        if stage.attempts > 0 {
            let _ = write!(
                text,
                " · {} · {} · {} {} · {}",
                count(stage.attempts, "attempt", "attempts"),
                duration(stage.active_seconds),
                stage.harness,
                if stage.model.is_empty() {
                    "default"
                } else {
                    &stage.model
                },
                usage_text(&stage.usage)
            );
        }
        text.push('\n');
    }
    text
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        config::{Stage, Workflow},
        engine::StageState,
    };
    use chrono::{DateTime, Duration};
    use serde_json::{Value, json};

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + seconds, 0).unwrap()
    }
    // Fixtures deserialize from JSON, as saved runs do, so fields added
    // elsewhere with serde defaults don't break them.
    fn attempt(start: i64, end: i64, status: Status, usage: Option<Usage>) -> Attempt {
        serde_json::from_value(json!({
            "number": 1,
            "started_at": at(start),
            "finished_at": at(end),
            "model": "model-a",
            "mode": "unattended",
            "status": status,
            "exit_code": 0,
            "directory": "/attempt",
            "handoff_file": "/handoff.json",
            "error": null,
            "usage": usage,
        }))
        .unwrap()
    }
    fn handoff(fields: Value) -> crate::handoff::Handoff {
        let mut value = json!({ "summary": "Done", "verdict": "complete" });
        value
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        serde_json::from_value(value).unwrap()
    }
    fn check(code: i32, status: TestStatus, required: bool) -> crate::checks::CheckResult {
        serde_json::from_value(json!({
            "command": ["cargo", "test"],
            "required": required,
            "status": status,
            "exit_code": code,
            "started_at": at(0),
            "finished_at": at(1),
            "directory": "/checks/01",
            "error": null,
        }))
        .unwrap()
    }
    fn stage(status: Status, attempts: Vec<Attempt>) -> StageState {
        serde_json::from_value(json!({ "status": status, "attempts": [], "handoff": null }))
            .map(|mut state: StageState| {
                state.attempts = attempts;
                state
            })
            .unwrap()
    }

    /// Implement (retried once), Verify (waived, checks), Review (disabled).
    pub(crate) fn sample() -> Run {
        let names = [
            ("implement", "Implement"),
            ("verify", "Verify"),
            ("review", "Review"),
        ];
        let workflow = Workflow {
            name: "Feature".into(),
            max_repair_passes: 2,
            max_parallel: 2,
            stages: names
                .iter()
                .map(|(id, name)| Stage {
                    id: (*id).into(),
                    name: (*name).into(),
                    harness: "claude".into(),
                    enabled: *id != "review",
                    ..Stage::default()
                })
                .collect(),
        };
        let paid = Usage {
            input_tokens: Some(1_200),
            output_tokens: Some(300),
            cache_read_tokens: Some(40_000),
            cache_write_tokens: None,
            cost_usd: Some(0.25),
        };
        let tokens_only = Usage {
            input_tokens: Some(800),
            output_tokens: Some(100),
            ..Usage::default()
        };
        let mut implement = stage(
            Status::Complete,
            vec![
                attempt(10, 40, Status::Failed, Some(paid)),
                attempt(50, 90, Status::Complete, None),
            ],
        );
        implement.handoff = Some(handoff(json!({
            "findings": [
                { "severity": "advisory", "description": "Rename later" },
                { "severity": "blocking", "description": "Data loss" },
            ],
            "unresolved": ["Docs pending"],
            "files_changed": ["src/lib.rs", "src/lib.rs"],
        })));
        let mut verify_attempt = attempt(100, 130, Status::Complete, Some(tokens_only));
        verify_attempt.checks = vec![
            check(0, TestStatus::Passed, true),
            check(3, TestStatus::Failed, false),
        ];
        let mut verify = stage(Status::Complete, vec![verify_attempt]);
        verify.handoff = Some(handoff(json!({
            "structured": false,
            "tests": [{ "command": "pytest", "status": "not_run", "required": true }],
        })));
        verify.waiver = Some(crate::engine::Waiver {
            reason: "Docs only".into(),
            accepted_at: at(140),
        });
        let mut run: Run = serde_json::from_value(json!({
            "version": 1,
            "revision": 3,
            "id": "20231114-221320-abcd1234",
            "created_at": at(0),
            "updated_at": at(500),
            "project": "/project",
            "workspace": "/project",
            "directory": "/project/.model-router/runs/x",
            "worktree": false,
            "task": "Task",
            "workflow": workflow,
            "adapters": {},
            "stages": [],
            "cursor": 3,
            "repair_passes": 1,
            "repair_feedback": null,
            "status": "complete",
            "error": null,
        }))
        .unwrap();
        run.stages = vec![implement, verify, stage(Status::Skipped, vec![])];
        run
    }

    #[test]
    fn report_counts_retries_durations_and_partial_usage() {
        let report = build(&sample());
        assert_eq!((report.attempts, report.retries), (3, 1));
        assert_eq!((report.wall_seconds, report.active_seconds), (130, 100));
        let implement = &report.stages[0];
        assert_eq!((implement.attempts, implement.retries), (2, 1));
        assert_eq!(implement.active_seconds, 70);
        assert_eq!(implement.harness, "claude");
        assert_eq!(implement.model, "model-a");
        assert_eq!(report.stages[2].status, Status::Skipped);
        assert_eq!(report.stages[2].attempts, 0);

        // Two of three attempts reported tokens; only one reported cost.
        let usage = report.usage;
        assert_eq!(
            (usage.attempts, usage.reported, usage.cost_reported),
            (3, 2, 1)
        );
        assert_eq!(usage.sum.input_tokens, Some(2_000));
        assert_eq!(usage.sum.cache_read_tokens, Some(40_000));
        assert_eq!(usage.sum.cache_write_tokens, None);
        assert_eq!(usage.sum.cost_usd, Some(0.25));
        assert_eq!(
            usage_text(&usage),
            "$0.25 · in 2.0k · out 400 · cache read 40k · usage reported by 2 of 3 attempts · cost reported by 1 of 3 attempts"
        );
        assert_eq!(
            usage_text(&report.stages[1].usage),
            "cost not reported · in 800 · out 100"
        );
        assert_eq!(usage_text(&report.stages[2].usage), "usage not reported");
        assert_eq!(
            summary(&report),
            "wall 2m 10s · active 1m 40s · 3 attempts · 1 retry · repairs 1/2"
        );
    }

    #[test]
    fn report_lists_unresolved_items_and_check_sources() {
        let mut run = sample();
        run.status = Status::Blocked;
        run.error = Some("Required check failed".into());
        let report = build(&run);
        let items: Vec<_> = report
            .unresolved
            .iter()
            .map(|i| (i.kind, item_text(i)))
            .collect();
        assert_eq!(
            items,
            [
                (ItemKind::Error, "Required check failed".into()),
                (
                    ItemKind::Blocking,
                    "Implement: blocking finding: Data loss".into()
                ),
                (
                    ItemKind::FailedCheck,
                    "Verify: required model-reported check not run: pytest".into()
                ),
                (
                    ItemKind::Unresolved,
                    "Implement: unresolved: Docs pending".into()
                ),
                (
                    ItemKind::Waiver,
                    "Verify: verification not applicable: Docs only".into()
                ),
                (
                    ItemKind::OptionalCheck,
                    "Verify: optional router check failed: cargo test (exit 3)".into()
                ),
                (
                    ItemKind::Unstructured,
                    "Verify: handoff is unstructured text".into()
                ),
                (
                    ItemKind::Advisory,
                    "Implement: advisory: Rename later".into()
                ),
            ]
        );
        let checks = &report.stages[1].checks;
        assert_eq!(
            checks
                .iter()
                .map(|c| (c.source, c.status, c.required))
                .collect::<Vec<_>>(),
            [
                (Source::Router, TestStatus::Passed, true),
                (Source::Router, TestStatus::Failed, false),
                (Source::ModelReported, TestStatus::NotRun, true),
            ]
        );
        assert_eq!(report.stages[1].waiver.as_deref(), Some("Docs only"));
        assert_eq!(report.reported_files, ["src/lib.rs"]);
        assert_eq!(
            check_detail(&checks[1], "Verify"),
            "optional · router · Verify · exit 3"
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["stages"][1]["checks"][2]["source"], "model-reported");
        assert_eq!(json["unresolved"][0]["kind"], "error");
    }

    #[test]
    fn rendering_labels_unavailable_changes_and_falls_back_to_reported_files() {
        let report = build(&sample());
        let text = render(&report, Some(&Changes::NotGit));
        assert!(text.contains("Changed during this run: unavailable (not a Git repository)"));
        assert!(text.contains("Model-reported (unverified): src/lib.rs"));
        assert!(text.contains("  ✓ cargo test · required · router · Verify"));
        assert!(text.contains("  – pytest · required · model-reported · Verify"));
        assert!(text.contains("  Review     disabled\n"), "{text}");
        let unavailable = Changes::Unavailable {
            reason: "no baseline snapshot was recorded".into(),
        };
        assert!(
            render(&report, Some(&unavailable)).contains(
                "Changed during this run: unavailable: no baseline snapshot was recorded"
            )
        );
        let tracked = Changes::Tracked {
            files: vec![
                FileChange {
                    path: "src/lib.rs".into(),
                    status: snapshot::ChangeKind::Modified,
                    added: Some(40),
                    removed: Some(5),
                },
                FileChange {
                    path: "logo.png".into(),
                    status: snapshot::ChangeKind::Added,
                    added: None,
                    removed: None,
                },
            ],
        };
        let text = render(&report, Some(&tracked));
        assert!(text.contains("Changed during this run: 2 files, +40 -5"));
        assert!(text.contains("  M  src/lib.rs  +40 -5\n  A  logo.png  binary"));
        assert!(!text.contains("Model-reported"));
    }

    #[test]
    fn unfinished_attempts_count_until_now() {
        let mut run = sample();
        run.stages[0].attempts[1].finished_at = None;
        run.stages[0].attempts[1].started_at = Utc::now() - Duration::seconds(30);
        let report = build(&run);
        assert!(report.stages[0].active_seconds >= 60);
        assert!(report.wall_seconds > 130);
    }

    #[test]
    fn small_numbers_stay_exact() {
        assert_eq!(
            [
                tokens(999),
                tokens(1_240),
                tokens(45_600),
                tokens(2_340_000)
            ],
            ["999", "1.2k", "46k", "2.3M"]
        );
        assert_eq!(
            [dollars(0.0), dollars(0.004), dollars(1.5)],
            ["$0.00", "$0.0040", "$1.50"]
        );
    }
}
