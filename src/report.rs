//! Completion summaries: what a run changed, checked, left unresolved, and
//! spent. `build_with` and `render` are pure; `build` also reads pricing
//! files and saved ship results (never the network); `changes`, `diff`, and
//! `receipts` run Git.
use crate::{
    config::{Budget, Role},
    cost::{self, CostTotal, Pricing},
    engine::{Attempt, Run, Status},
    handoff::{HandoffSource, Severity, TestStatus, Usage},
    snapshot::{self, FileChange},
    transitions::{Level, Record},
};
use anyhow::{Result, anyhow};
use chrono::Utc;
use serde::Serialize;
use std::{collections::HashSet, fmt::Write, fs, path::Path};

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
    /// The run's spend caps.
    pub budget: Budget,
    /// When the built-in price table was last checked.
    pub prices_as_of: String,
    pub stages: Vec<StageReport>,
    /// Most severe first.
    pub unresolved: Vec<Item>,
    /// `files_changed` claimed by the final handoffs; unverified.
    pub reported_files: Vec<String>,
    /// The latest review summary stage's brief, Markdown; also review.md.
    pub review: Option<String>,
    /// Ships of this run: by ship stages, then `model-router ship`. Read
    /// from each ship's ship.json.
    pub shipped: Vec<crate::ship::Outcome>,
    /// The saved run and stage checkpoint this run was forked from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork_of: Option<crate::engine::Fork>,
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
    pub budget: Budget,
    /// Router checks from the latest attempt, then model-reported tests.
    pub checks: Vec<CheckLine>,
    /// Accepted not-applicable reason.
    pub waiver: Option<String>,
    /// Where the latest attempt's handoff came from; None when unrecorded.
    pub handoff_source: Option<HandoffSource>,
    /// A best-of-N stage's candidates and choice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fan_out: Option<FanOutReport>,
    /// A fork reused this stage's handoff from its source run without running it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub inherited: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FanOutReport {
    /// `auto`, or the judge stage's name.
    pub judge: String,
    pub candidates: Vec<CandidateReport>,
    /// The chosen candidate, why, and whether its patch reached the workspace.
    pub selected: Option<String>,
    pub reason: Option<String>,
    pub applied: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CandidateReport {
    pub id: String,
    pub harness: String,
    pub model: String,
    pub status: Status,
    pub attempts: usize,
    pub usage: UsageTotal,
    /// Changed files and lines from the round's base.
    pub files: Option<usize>,
    pub added: Option<u64>,
    pub removed: Option<u64>,
    /// Whether every required router check passed in its worktree.
    pub checks_pass: bool,
    pub error: Option<String>,
    pub worktree: Option<std::path::PathBuf>,
    pub patch: Option<std::path::PathBuf>,
}

/// Sums of what attempts reported. A field stays None when no attempt
/// reported it; unreported attempts are counted, never estimated. `cost`
/// adds estimates, each labelled with its source.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct UsageTotal {
    pub sum: Usage,
    pub attempts: usize,
    /// Attempts that reported any usage, and any cost.
    pub reported: usize,
    pub cost_reported: usize,
    pub cost: CostTotal,
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

/// What backs a stage's verification, from its latest attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// Every required router check ran and passed.
    RouterChecks,
    /// A required router check failed, timed out, or did not run.
    RouterChecksFailed,
    /// No required router check; the model reported a passing required test.
    SelfReportedOnly,
    /// No required router check; the user accepted a not-applicable reason.
    Waived,
    None,
}

/// Whether the workspace still holds the code a stage's router checks tested.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Freshness {
    /// The workspace matches the tested snapshot.
    Current,
    /// Files changed since the checks ran; the evidence no longer covers them.
    Stale { changed: Vec<String> },
    /// The snapshot or the workspace cannot be compared.
    Unavailable { reason: String },
}

/// A stage's verification receipt: which router checks ran against which
/// Git snapshot, and whether the code has changed since. It confirms the
/// checks ran and passed against that snapshot, not that they are adequate.
#[derive(Debug, Clone, Serialize)]
pub struct Receipt {
    pub stage: String,
    pub role: Role,
    /// The latest attempt's number.
    pub attempt: usize,
    pub basis: Basis,
    pub required_passed: usize,
    pub required_total: usize,
    /// Required tests the model reported passing; the router ran none of them.
    pub model_reported_passed: usize,
    /// The tree the router checks tested; None without one.
    pub snapshot: Option<String>,
    /// None when no router check ran.
    pub freshness: Option<Freshness>,
}

/// Receipts for stages whose latest attempt ran router checks, and for test
/// stages. Compares each tested snapshot with the workspace now, so it runs
/// Git (writing only objects).
pub fn receipts(run: &Run) -> Vec<Receipt> {
    let dir = git_dir(run);
    let mut is_repo: Option<bool> = None;
    let mut current: Option<std::result::Result<String, String>> = None;
    let mut now = || {
        current
            .get_or_insert_with(|| {
                if !run.workspace.is_dir() {
                    return Err(format!(
                        "the workspace {} no longer exists",
                        run.workspace.display()
                    ));
                }
                match snapshot::current_tree(&run.workspace) {
                    Ok(Some(tree)) => Ok(tree),
                    Ok(None) => Err("not a Git repository".into()),
                    Err(error) => Err(format!("{error:#}")),
                }
            })
            .clone()
    };
    let mut out = vec![];
    for (stage, state) in run.workflow.stages.iter().zip(&run.stages) {
        // A fan-out stage's checks ran on its chosen candidate.
        let Some(last) = state.checked() else {
            continue;
        };
        if last.checks.is_empty() && stage.role != Role::Test {
            continue;
        }
        // Count what the stage configures, not what ran: a cancel can stop
        // the checks part way through.
        let configured = stage.checks.iter().filter(|c| c.required).count();
        let required_passed = last
            .checks
            .iter()
            .filter(|c| c.required && c.status == TestStatus::Passed)
            .count();
        let model_reported_passed = state.handoff.as_ref().map_or(0, |h| {
            h.tests
                .iter()
                .filter(|t| t.required && t.status == TestStatus::Passed)
                .count()
        });
        let basis = if configured > 0 && required_passed == configured {
            Basis::RouterChecks
        } else if configured > 0 {
            Basis::RouterChecksFailed
        } else if state.waiver.is_some() {
            Basis::Waived
        } else if model_reported_passed > 0 {
            Basis::SelfReportedOnly
        } else {
            Basis::None
        };
        let freshness = (!last.checks.is_empty()).then(|| match &last.checks_tree {
            None if !*is_repo.get_or_insert_with(|| snapshot::is_repository(dir)) => {
                Freshness::Unavailable {
                    reason: "not a Git repository".into(),
                }
            }
            None => Freshness::Unavailable {
                reason: "no snapshot was recorded for these checks (Git failed, or the run predates receipts)".into(),
            },
            Some(tree) if !snapshot::exists(dir, tree) => Freshness::Unavailable {
                reason: format!(
                    "snapshot {} is no longer in the repository (run cleaned or pruned)",
                    short(tree)
                ),
            },
            Some(tree) => match now() {
                Err(reason) => Freshness::Unavailable { reason },
                Ok(now) if now == *tree => Freshness::Current,
                Ok(now) => match snapshot::changed_paths(dir, tree, &now) {
                    Ok(changed) => Freshness::Stale { changed },
                    Err(error) => Freshness::Unavailable {
                        reason: format!("{error:#}"),
                    },
                },
            },
        });
        out.push(Receipt {
            stage: stage.name.clone(),
            role: stage.role,
            // The stage's attempt, even when a candidate's checks stand for it.
            attempt: state.attempts.last().map_or(last.number, |a| a.number),
            basis,
            required_passed,
            required_total: configured,
            model_reported_passed,
            snapshot: last.checks_tree.clone(),
            freshness,
        });
    }
    out
}

fn short(tree: &str) -> &str {
    tree.get(..10).unwrap_or(tree)
}

/// One line per receipt, under a heading; empty without receipts.
pub fn render_receipts(receipts: &[Receipt]) -> String {
    if receipts.is_empty() {
        return String::new();
    }
    let mut text = String::from("Verification receipts:\n");
    for receipt in receipts {
        let fresh = matches!(receipt.freshness, None | Some(Freshness::Current));
        let glyph = match receipt.basis {
            Basis::RouterChecks if fresh => "✓",
            Basis::RouterChecks => "!",
            Basis::RouterChecksFailed => "✗",
            Basis::SelfReportedOnly | Basis::Waived | Basis::None => "•",
        };
        let basis = match receipt.basis {
            Basis::RouterChecks | Basis::RouterChecksFailed => format!(
                "router checks {}/{} required passed",
                receipt.required_passed, receipt.required_total
            ),
            Basis::SelfReportedOnly => format!(
                "self-reported only: {} passing required test(s) claimed by the model, none run by the router",
                receipt.model_reported_passed
            ),
            Basis::Waived => {
                "no required router check; verification waived as not applicable".into()
            }
            Basis::None => "no passing required check".into(),
        };
        let _ = write!(
            text,
            "  {glyph} {} (attempt {}): {basis}",
            receipt.stage, receipt.attempt
        );
        if let Some(tree) = &receipt.snapshot {
            let _ = write!(text, " at snapshot {}", short(tree));
        }
        match &receipt.freshness {
            None => {}
            Some(Freshness::Current) => text.push_str(" · current: the workspace still matches"),
            Some(Freshness::Stale { changed }) => {
                let _ = write!(
                    text,
                    " · stale: {} changed since the checks ran ({}{})",
                    count(changed.len(), "file", "files"),
                    changed
                        .iter()
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", "),
                    if changed.len() > 5 { ", …" } else { "" }
                );
            }
            Some(Freshness::Unavailable { reason }) => {
                let _ = write!(text, " · freshness unknown: {reason}");
            }
        }
        text.push('\n');
    }
    text.push_str(
        "  A receipt shows the router ran these checks against that snapshot, not that they are adequate.\n",
    );
    text
}

/// Sums tokens and estimated costs across every attempt (including fan-out
/// candidates) without building a full `RunReport` or reading `ship.json`.
pub fn usage_total(run: &Run) -> UsageTotal {
    usage_total_with(run, &Pricing::load(&run.project))
}

pub fn usage_total_with(run: &Run, pricing: &Pricing) -> UsageTotal {
    let mut sum = UsageTotal::default();
    for (index, state) in run.stages.iter().enumerate() {
        let (provider, billing) = cost::stage_estimate_params(run, index);
        let candidates = state.candidate_attempts().map(|(_, a)| a);
        for attempt in state.attempts.iter().chain(candidates) {
            let estimate = cost::estimate_with_billing(
                attempt.usage.as_ref(),
                &attempt.model,
                provider,
                billing,
                pricing,
            );
            sum.attempts += 1;
            sum.cost.add(&estimate);
            if let Some(usage) = attempt.usage {
                sum.reported += 1;
                sum.cost_reported += usize::from(usage.cost_usd.is_some());
                sum.sum = sum.sum + usage;
            }
        }
        for (task, turn) in state.task_turns() {
            let estimate = cost::task_turn_estimate(run, task, turn, pricing);
            sum.attempts += 1;
            sum.cost.add(&estimate);
            if let Some(usage) = turn.usage {
                sum.reported += 1;
                sum.cost_reported += usize::from(usage.cost_usd.is_some());
                sum.sum = sum.sum + usage;
            }
        }
    }
    sum
}

/// `build_with` the project's pricing.
pub fn build(run: &Run) -> RunReport {
    build_with(run, &Pricing::load(&run.project))
}

pub fn build_with(run: &Run, pricing: &Pricing) -> RunReport {
    let mut estimates: Vec<Vec<cost::Estimate>> = Vec::with_capacity(run.stages.len());
    let mut candidate_estimates: Vec<Vec<cost::Estimate>> = Vec::with_capacity(run.stages.len());
    // Delegated tasks' turns, with their usage, priced under each task's target.
    let task_estimates: Vec<Vec<(Option<crate::handoff::Usage>, cost::Estimate)>> = run
        .stages
        .iter()
        .map(|state| {
            state
                .task_turns()
                .map(|(task, turn)| {
                    (
                        turn.usage,
                        cost::task_turn_estimate(run, task, turn, pricing),
                    )
                })
                .collect()
        })
        .collect();
    for (index, state) in run.stages.iter().enumerate() {
        let (provider, billing) = cost::stage_estimate_params(run, index);
        let est = |a: &Attempt| {
            cost::estimate_with_billing(a.usage.as_ref(), &a.model, provider, billing, pricing)
        };
        estimates.push(state.attempts.iter().map(est).collect());
        // The current round's candidates first: `FanOutReport` slices them.
        candidate_estimates.push(state.candidate_attempts().map(|(_, a)| est(a)).collect());
    }
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
    let mut seen_files: HashSet<String> = HashSet::new();
    let mut stages = vec![];
    for (stage_index, (((stage, state), estimates), candidate_estimates)) in run
        .workflow
        .stages
        .iter()
        .zip(&run.stages)
        .zip(&estimates)
        .zip(&candidate_estimates)
        .enumerate()
    {
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
                if seen_files.insert(file.clone()) {
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
        // A ship stage's acceptance is its plan's confirmation.
        let waiver = state.waiver.as_ref().filter(|_| stage.role != Role::Ship);
        if let Some(waiver) = waiver {
            item(
                ItemKind::Waiver,
                format!("verification not applicable: {}", waiver.reason),
            );
        }
        let fan_out = state.fan_out.as_ref().map(|fan| {
            let judge = stage.fan_out.as_ref().map_or_else(
                || crate::config::AUTO_JUDGE.to_owned(),
                |f| {
                    run.workflow
                        .stages
                        .iter()
                        .find(|s| s.id == f.judge && s.enabled)
                        .map_or_else(|| crate::config::AUTO_JUDGE.to_owned(), |s| s.name.clone())
                },
            );
            if let Some(selection) = fan.selection.as_ref().filter(|s| !s.applied) {
                item(
                    ItemKind::Error,
                    format!("chosen candidate {} was not applied", selection.candidate),
                );
            }
            FanOutReport {
                judge,
                candidates: fan
                    .candidates
                    .iter()
                    .scan(0, |offset, c| {
                        let start = *offset;
                        *offset += c.attempts.len();
                        Some((c, &candidate_estimates[start..*offset]))
                    })
                    .map(|(c, estimates)| CandidateReport {
                        id: c.id.clone(),
                        harness: c.harness.clone(),
                        model: c.model.clone(),
                        status: c.status,
                        attempts: c.attempts.len(),
                        usage: total(c.attempts.iter().zip(estimates)),
                        files: c.changes.as_ref().map(|x| x.files.len()),
                        added: c.changes.as_ref().map(|x| x.added),
                        removed: c.changes.as_ref().map(|x| x.removed),
                        checks_pass: c.checks_pass(),
                        error: c.error.clone(),
                        worktree: c.attempts.last().and_then(|a| a.worktree.clone()),
                        patch: c.changes.as_ref().map(|x| x.patch.clone()),
                    })
                    .collect(),
                selected: fan.selection.as_ref().map(|s| s.candidate.clone()),
                reason: fan.selection.as_ref().map(|s| s.reason.clone()),
                applied: fan.selection.as_ref().is_some_and(|s| s.applied),
            }
        });
        let candidate_attempts = state.candidate_attempts().map(|(_, a)| a);
        stages.push(StageReport {
            name: stage.name.clone(),
            role: stage.role,
            harness: stage.harness.clone(),
            model: last.map_or_else(|| stage.model.clone(), |a| a.model.clone()),
            status: state.status,
            attempts: state.attempts.len(),
            retries: state.attempts.len().saturating_sub(1),
            active_seconds: state.attempts.iter().map(seconds).sum(),
            // Spend counts every candidate's and every delegated task's.
            usage: usage_sum(
                state
                    .attempts
                    .iter()
                    .zip(estimates)
                    .chain(candidate_attempts.zip(candidate_estimates))
                    .map(|(a, e)| (a.usage, e))
                    .chain(task_estimates[stage_index].iter().map(|(u, e)| (*u, e))),
            ),
            budget: stage.budget,
            checks,
            waiver: waiver.map(|w| w.reason.clone()),
            handoff_source: last.and_then(|a| a.handoff_source),
            fan_out,
            inherited: state.inherited.is_some(),
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
        usage: usage_sum(
            run.stages
                .iter()
                .zip(&estimates)
                .flat_map(|(s, e)| s.attempts.iter().zip(e))
                .chain(
                    run.stages
                        .iter()
                        .zip(&candidate_estimates)
                        .flat_map(|(s, e)| s.candidate_attempts().map(|(_, a)| a).zip(e)),
                )
                .map(|(a, e)| (a.usage, e))
                .chain(task_estimates.iter().flatten().map(|(u, e)| (*u, e))),
        ),
        budget: run.workflow.budget,
        prices_as_of: cost::table_as_of().into(),
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
        shipped: crate::ship::outcomes(run),
        fork_of: run.fork_of.clone(),
    }
}

fn total<'a>(attempts: impl IntoIterator<Item = (&'a Attempt, &'a cost::Estimate)>) -> UsageTotal {
    usage_sum(
        attempts
            .into_iter()
            .map(|(attempt, estimate)| (attempt.usage, estimate)),
    )
}

/// Sums processes' usage and estimates: stage attempts, candidates, tasks.
fn usage_sum<'a>(
    processes: impl IntoIterator<Item = (Option<crate::handoff::Usage>, &'a cost::Estimate)>,
) -> UsageTotal {
    let mut total = UsageTotal::default();
    for (usage, estimate) in processes {
        total.attempts += 1;
        total.cost.add(estimate);
        if let Some(usage) = usage {
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

/// E.g. "~$0.31 (reported $0.25 + table $0.06) · budget $5.00 / 2.0M tokens".
pub fn cost_text(cost: &CostTotal, budget: &Budget) -> String {
    let mut text = cost.text();
    let caps: Vec<String> = budget
        .max_usd
        .map(dollars)
        .into_iter()
        .chain(budget.max_tokens.map(|n| format!("{} tokens", tokens(n))))
        .collect();
    if !caps.is_empty() {
        text.push_str(&format!(" · budget {}", caps.join(" / ")));
    }
    text
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
    let _ = write!(
        text,
        "Cost: {}",
        cost_text(&report.usage.cost, &report.budget)
    );
    if report.usage.cost.table.attempts > 0 {
        let _ = write!(
            text,
            " · table prices are estimates as of {}",
            report.prices_as_of
        );
    }
    text.push('\n');
    if let Some(fork) = &report.fork_of {
        let _ = writeln!(
            text,
            "Forked from: {} at stage {} (attempt {}, snapshot {}){}",
            fork.run,
            fork.stage,
            fork.attempt,
            short(&fork.snapshot),
            fork.model
                .as_deref()
                .map(|m| format!(" with model {m}"))
                .unwrap_or_default()
        );
    }
    if let Some(error) = &report.error {
        let _ = writeln!(text, "Error: {error}");
    }
    if let Some(review) = &report.review {
        let _ = writeln!(text, "\nReview summary:\n{review}");
    }
    for ship in &report.shipped {
        let _ = writeln!(text, "\nShipped:");
        for line in ship.lines() {
            let _ = writeln!(text, "  {line}");
        }
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
        if stage.inherited {
            text.push_str(" · handoff reused from the fork's source run");
        }
        if stage.attempts > 0 {
            let _ = write!(
                text,
                " · {} · {} · {} {} · {} · cost {}",
                count(stage.attempts, "attempt", "attempts"),
                duration(stage.active_seconds),
                stage.harness,
                if stage.model.is_empty() {
                    "default"
                } else {
                    &stage.model
                },
                usage_text(&stage.usage),
                cost_text(&stage.usage.cost, &stage.budget)
            );
            if let Some(source) = stage.handoff_source {
                let _ = write!(text, " · handoff: {}", source.label());
            }
        }
        text.push('\n');
        if let Some(fan) = &stage.fan_out {
            for line in fan_out_text(fan) {
                let _ = writeln!(text, "    {line}");
            }
        }
    }
    text
}

/// A best-of-N stage's judge, choice, and candidates, one line each.
pub fn fan_out_text(fan: &FanOutReport) -> Vec<String> {
    let mut lines = vec![match &fan.selected {
        Some(id) => format!(
            "judge {}: {id} {}{}",
            fan.judge,
            if fan.applied {
                "applied"
            } else {
                "not applied"
            },
            fan.reason
                .as_deref()
                .filter(|r| !r.is_empty())
                .map(|r| format!(" ({})", r.lines().next().unwrap_or("")))
                .unwrap_or_default()
        ),
        None => format!("judge {}: no choice yet", fan.judge),
    }];
    for candidate in &fan.candidates {
        let mut line = format!(
            "{} {} {} {} {}",
            if fan.selected.as_ref() == Some(&candidate.id) {
                "★"
            } else {
                "·"
            },
            candidate.id,
            candidate.harness,
            if candidate.model.is_empty() {
                "default"
            } else {
                &candidate.model
            },
            candidate.status.label()
        );
        if let (Some(files), Some(added), Some(removed)) =
            (candidate.files, candidate.added, candidate.removed)
        {
            let _ = write!(
                line,
                " · {} +{added} −{removed}",
                count(files, "file", "files")
            );
            if !candidate.checks_pass {
                line.push_str(" · checks failed");
            }
        }
        if let Some(error) = &candidate.error {
            let _ = write!(line, " · {}", clip(error.lines().next().unwrap_or(""), 80));
        }
        if let Some(tree) = &candidate.worktree {
            let _ = write!(line, " · {}", tree.display());
        }
        lines.push(line);
    }
    lines
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
        let workflow = Workflow::new(
            "Feature",
            names
                .iter()
                .map(|(id, name)| Stage {
                    id: (*id).into(),
                    name: (*name).into(),
                    harness: "claude".into(),
                    enabled: *id != "review",
                    ..Stage::default()
                })
                .collect(),
        );
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
    fn report_labels_cost_sources_per_stage_and_in_total() {
        let mut run = sample();
        run.workflow.budget.max_usd = Some(5.0);
        // model-a is not in the table: only the reported cost is known.
        let report = build_with(&run, &Pricing::default());
        assert_eq!(
            report.usage.cost.text(),
            "$0.25 (reported) · cost unknown for 2 of 3 attempts"
        );
        let price = cost::Price {
            input: 100.0,
            output: 1000.0,
            cache_read: None,
            cache_write: None,
        };
        let report = build_with(&run, &Pricing::default().with_override("model-a", price));
        // Verify: 800 in at $100/M + 100 out at $1000/M = $0.18.
        assert_eq!(report.stages[1].usage.cost.text(), "~$0.18 (config)");
        assert_eq!(
            cost_text(&report.usage.cost, &report.budget),
            "~$0.43 (reported $0.25 + config $0.18) · cost unknown for 1 of 3 attempts · budget $5.00"
        );
        let text = render(&report, None);
        assert!(
            text.contains("\nCost: ~$0.43 (reported $0.25 + config $0.18)"),
            "{text}"
        );
        assert!(text.contains("cost ~$0.18 (config)"), "{text}");
        assert!(!text.contains("table prices"), "{text}");
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["usage"]["cost"]["config"]["attempts"], 1);
        assert_eq!(json["budget"]["max_usd"], 5.0);
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
