//! Saved-run history: a summary index that listing reads instead of full
//! records, damaged-record reporting, filters, reruns, attempt comparison,
//! artifact sizes, and cleanup.
use crate::{
    config::Settings,
    engine::{self, Attempt, Placement, Run, Status},
    handoff::Handoff,
    storage,
};
use anyhow::{Context, Result, anyhow, ensure};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
    str::FromStr,
};

/// What listing needs from a run. `Run::persist` saves it beside run.json.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub status: Status,
    pub workflow: String,
    /// See `excerpt`.
    pub task: String,
    pub enabled_stages: usize,
    pub complete_stages: usize,
    pub attempts: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub worktree: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerun_of: Option<String>,
    /// run.json's revision when this summary was saved.
    #[serde(default)]
    pub revision: u64,
    /// run.json's size when this summary was saved, so listing can notice a
    /// run.json that changed or broke without parsing it. None in summaries
    /// saved before this was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_bytes: Option<u64>,
    /// Groups recovery left running for the run; see
    /// `engine::refuse_unproven`, which reads them here for every run in
    /// the checkout. Empty in summaries saved before this was recorded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retained_groups: Vec<crate::process::Group>,
    /// Tasks the run's stages delegated, across attempts.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tasks: usize,
    /// Who created the run: the user, an agent (`run_launch`), or a schedule.
    #[serde(
        default,
        skip_serializing_if = "crate::engine::CreationSource::is_user"
    )]
    pub creation_source: crate::engine::CreationSource,
}
fn is_zero(value: &usize) -> bool {
    *value == 0
}
impl From<&Run> for RunSummary {
    fn from(run: &Run) -> Self {
        Self {
            id: run.id.clone(),
            created_at: run.created_at,
            updated_at: run.updated_at,
            status: run.status,
            workflow: run.workflow.name.clone(),
            task: excerpt(&run.task),
            enabled_stages: run.workflow.stages.iter().filter(|s| s.enabled).count(),
            complete_stages: run
                .stages
                .iter()
                .filter(|s| s.status == Status::Complete)
                .count(),
            attempts: run.stages.iter().map(|s| s.attempts.len()).sum(),
            error: run.error.as_deref().map(excerpt),
            worktree: run.worktree,
            rerun_of: run.rerun_of.clone(),
            revision: run.revision,
            run_bytes: None,
            retained_groups: run.retained_groups().to_vec(),
            tasks: run
                .stages
                .iter()
                .flat_map(|s| &s.attempts)
                .map(|a| a.tasks.len())
                .sum(),
            creation_source: run.creation_source,
        }
    }
}

/// The first nonblank line, without terminal controls, cut to 200 characters.
pub fn excerpt(text: &str) -> String {
    let text = storage::clean_terminal(text);
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if line.chars().count() <= 200 {
        line.into()
    } else {
        line.chars().take(199).chain(['…']).collect()
    }
}

/// A run directory as listed.
#[derive(Debug, Clone)]
pub enum Entry {
    Summary(RunSummary),
    /// Neither summary.json nor run.json is readable and valid.
    Damaged {
        id: String,
        path: PathBuf,
        error: String,
    },
}
impl Entry {
    pub fn id(&self) -> &str {
        match self {
            Self::Summary(summary) => &summary.id,
            Self::Damaged { id, .. } => id,
        }
    }
    /// When the run started. A damaged record's comes from its ID, if it has one.
    pub fn created_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Summary(summary) => Some(summary.created_at),
            Self::Damaged { id, .. } => {
                NaiveDateTime::parse_from_str(id.get(..15)?, "%Y%m%d-%H%M%S")
                    .ok()
                    .map(|time| time.and_utc())
            }
        }
    }
    fn updated_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Summary(summary) => Some(summary.updated_at),
            Self::Damaged { .. } => self.created_at(),
        }
    }
    /// The run's status label, or "damaged".
    pub fn status_label(&self) -> &'static str {
        match self {
            Self::Summary(summary) => summary.status.label(),
            Self::Damaged { .. } => "damaged",
        }
    }
    pub fn dir(&self, project: &Path) -> PathBuf {
        match self {
            Self::Summary(summary) => runs_dir(project).join(&summary.id),
            Self::Damaged { path, .. } => path.clone(),
        }
    }
}

fn runs_dir(project: &Path) -> PathBuf {
    project.join(".model-router/runs")
}

/// Saved runs, newest first. Reads each summary.json, or run.json for runs
/// saved before summaries existed. Writes nothing.
pub fn list(project: &Path) -> Result<Vec<Entry>> {
    let directory = runs_dir(project);
    if !directory.exists() {
        return Ok(vec![]);
    }
    let mut entries = Vec::new();
    for item in fs::read_dir(&directory).with_context(|| format!("Read {}", directory.display()))? {
        let item = item?;
        if !item.file_type()?.is_dir() {
            continue;
        }
        let id = item.file_name().to_string_lossy().into_owned();
        entries.push(match read_summary(project, &id, &item.path()) {
            Ok(summary) => Entry::Summary(summary),
            Err(error) => Entry::Damaged {
                id,
                path: item.path(),
                error: format!("{error:#}"),
            },
        });
    }
    let mut keyed: Vec<(Option<DateTime<Utc>>, Entry)> = entries
        .into_iter()
        .map(|entry| (entry.created_at(), entry))
        .collect();
    keyed.sort_by(|(ta, a), (tb, b)| tb.cmp(ta).then_with(|| b.id().cmp(a.id())));
    let entries = keyed.into_iter().map(|(_, entry)| entry).collect();
    Ok(entries)
}

fn is_clean_excerpt(text: &str) -> bool {
    text.is_empty()
        || (text == text.trim()
            && !text.bytes().any(|b| b < 0x20 || b == 0x7f || b == 0xc2)
            && (text.len() <= 200 || text.chars().count() <= 200))
}

/// The summary when run.json still has the size it recorded; otherwise the
/// full record decides, so a broken run.json lists as damaged and one saved
/// without its summary (a crash between the two writes) lists current.
fn read_summary(project: &Path, id: &str, dir: &Path) -> Result<RunSummary> {
    if let Ok(mut summary) = storage::read_json::<RunSummary>(&dir.join("summary.json")) {
        let run_bytes = fs::metadata(dir.join("run.json")).map(|meta| meta.len());
        let consistent = match (summary.run_bytes, run_bytes) {
            (Some(expected), Ok(actual)) => expected == actual,
            (None, Ok(actual)) => actual > 0,
            (_, Err(_)) => false,
        };
        if summary.id == id && consistent {
            // Clean once here, not wherever it is shown: the TUI draws every
            // saved task each frame.
            if !is_clean_excerpt(&summary.task) {
                summary.task = excerpt(&summary.task);
            }
            return Ok(summary);
        }
    }
    let run = engine::load(project, id)?;
    Ok(RunSummary::from(&run))
}

/// Selects entries by run status, by every active status, or damaged records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    Is(Status),
    /// Pending, running, interactive, or awaiting confirmation.
    Active,
    Damaged,
}
const FILTERS: [(&str, StatusFilter); 10] = [
    ("active", StatusFilter::Active),
    ("pending", StatusFilter::Is(Status::Pending)),
    ("running", StatusFilter::Is(Status::Running)),
    ("interactive", StatusFilter::Is(Status::Interactive)),
    (
        "awaiting_confirmation",
        StatusFilter::Is(Status::AwaitingConfirmation),
    ),
    ("complete", StatusFilter::Is(Status::Complete)),
    ("failed", StatusFilter::Is(Status::Failed)),
    ("blocked", StatusFilter::Is(Status::Blocked)),
    ("cancelled", StatusFilter::Is(Status::Cancelled)),
    ("damaged", StatusFilter::Damaged),
];
impl StatusFilter {
    pub fn matches(self, entry: &Entry) -> bool {
        match (self, entry) {
            (Self::Damaged, Entry::Damaged { .. }) => true,
            (Self::Active, Entry::Summary(summary)) => matches!(
                summary.status,
                Status::Pending
                    | Status::Running
                    | Status::Interactive
                    | Status::AwaitingConfirmation
            ),
            (Self::Is(status), Entry::Summary(summary)) => summary.status == status,
            _ => false,
        }
    }
}
impl FromStr for StatusFilter {
    type Err = anyhow::Error;
    fn from_str(name: &str) -> Result<Self> {
        let key = name.trim().to_ascii_lowercase().replace('-', "_");
        FILTERS
            .iter()
            .find(|(filter, _)| *filter == key)
            .map(|(_, filter)| *filter)
            .with_context(|| {
                format!(
                    "Unknown status {name:?}. Use {}",
                    FILTERS.map(|(filter, _)| filter).join(", ")
                )
            })
    }
}

/// Entries with any of `statuses` (any status when empty) whose ID, workflow,
/// task, or status contains every word of `search`, ignoring case.
pub fn filter<'a>(entries: &'a [Entry], statuses: &[StatusFilter], search: &str) -> Vec<&'a Entry> {
    let words: Vec<String> = search.split_whitespace().map(str::to_lowercase).collect();
    entries
        .iter()
        .filter(|entry| statuses.is_empty() || statuses.iter().any(|s| s.matches(entry)))
        .filter(|entry| {
            if words.is_empty() {
                return true;
            }
            let text = match entry {
                Entry::Summary(s) => {
                    format!("{} {} {} {}", s.id, s.workflow, s.task, s.status.label())
                }
                Entry::Damaged { id, .. } => format!("{id} damaged"),
            }
            .to_lowercase();
            words.iter().all(|word| text.contains(word))
        })
        .collect()
}

/// Starts a new run from a saved run's task, workflow snapshot, and adapter
/// and provider snapshots, leaving the saved run unchanged. `worktree` defaults to its mode.
pub fn rerun(project: &Path, settings: &Settings, id: &str, worktree: Option<bool>) -> Result<Run> {
    let source = engine::load(project, id)?;
    let use_worktree = worktree.unwrap_or(source.worktree);
    let placement = if use_worktree {
        Placement::Worktree
    } else {
        Placement::Project
    };
    rerun_from(project, settings, source, placement)
}

/// Reruns saved run `id` as a new run working at `placement`.
pub fn rerun_at(
    project: &Path,
    settings: &Settings,
    id: &str,
    placement: Placement,
) -> Result<Run> {
    let source = engine::load(project, id)?;
    rerun_from(project, settings, source, placement)
}

fn rerun_from(
    project: &Path,
    settings: &Settings,
    source: Run,
    placement: Placement,
) -> Result<Run> {
    // `Run::create_rerun_from` refuses a rerun in the checkout while a harness
    // a run left there may still be running (`engine::refuse_unproven`).
    crate::trust::authorize_run_config(&source, settings.explicit_config.as_deref())?;
    let settings = Settings {
        adapters: source.adapters.clone(),
        providers: source.providers.clone(),
        delegates: source.delegates.clone(),
        ..settings.clone()
    };
    let source_id = source.id.clone();
    Run::create_rerun_from(
        project,
        &settings,
        &source.workflow,
        source.task.clone(),
        placement,
        &source,
        source_id,
    )
}

/// Starts a new run in its own Git worktree from a saved run's checkpoint at
/// the start of stage `stage_id`: the worktree's files are exactly the
/// snapshot taken when that stage first started after its upstream stages
/// finished, the upstream handoffs are reused without running them again,
/// and the run continues at that stage, optionally with `model`. The saved
/// run, its workspace, and the user's checkout are unchanged; nothing is
/// merged or shipped.
pub fn fork(
    project: &Path,
    settings: &Settings,
    id: &str,
    stage_id: &str,
    model: Option<String>,
) -> Result<Run> {
    let source = engine::load(project, id)?;
    // Not while a harness the source left running may still be at work.
    engine::refuse_unproven(&source)?;
    // A fork replays the source's executable settings, as a rerun does.
    crate::trust::authorize_run_config(&source, settings.explicit_config.as_deref())?;
    let index = source
        .workflow
        .stages
        .iter()
        .position(|s| s.id == stage_id)
        .with_context(|| format!("Run {id} has no stage {stage_id}"))?;
    let stage = &source.workflow.stages[index];
    ensure!(stage.enabled, "Stage {stage_id} is disabled in run {id}");
    ensure!(
        stage.role != crate::config::Role::Ship,
        "Ship stages run in the router; ship the fork instead (model-router ship)"
    );
    // A judge needs the fan-out's candidates, and its checkpoint predates
    // the winner being applied; a fork carries neither.
    if let Some(fan_out) = crate::graph::judged_by(&source.workflow, index) {
        let fan_out = &source.workflow.stages[fan_out].id;
        return Err(anyhow!(
            "Stage {stage_id} judges fan-out stage {fan_out}; fork at {fan_out} instead"
        ));
    }
    if model.is_some() {
        ensure!(
            stage.fan_out.is_none(),
            "Fan-out stage {stage_id} has candidate models; fork without --model"
        );
    }
    let order = source.order();
    let upstream: Vec<usize> = (0..source.stages.len())
        .filter(|&i| order.precedes(i, index))
        .collect();
    for &i in &upstream {
        let state = &source.stages[i];
        let name = &source.workflow.stages[i].name;
        ensure!(
            state.status == Status::Skipped
                || (state.status == Status::Complete && state.handoff.is_some()),
            "Upstream stage {name} is {} in run {id}; a fork reuses only completed upstream handoffs",
            state.status.label()
        );
    }
    // The checkpoint: the stage's first attempt after every upstream attempt
    // ended. Later retries started from files earlier attempts had changed.
    let upstream_end = upstream
        .iter()
        .flat_map(|&i| &source.stages[i].attempts)
        .filter_map(|a| a.finished_at)
        .max();
    let attempts = &source.stages[index].attempts;
    ensure!(
        !attempts.is_empty(),
        "Stage {stage_id} never started in run {id}, so no checkpoint was saved for it"
    );
    let boundary = attempts
        .iter()
        .find(|a| upstream_end.is_none_or(|end| a.started_at >= end))
        .with_context(|| {
            format!(
                "An upstream stage ran again after every attempt of {stage_id} started (e.g. a review repair), so no saved snapshot of {stage_id} matches the upstream handoffs"
            )
        })?;
    let tree = boundary.tree_before.clone().with_context(|| {
        format!(
            "Attempt {} of {stage_id} has no workspace snapshot (not a Git repository, or Git failed)",
            boundary.number
        )
    })?;
    ensure!(
        crate::snapshot::exists(project, &tree),
        "Snapshot {tree} of {stage_id} is no longer in the repository (the run was cleaned or its refs removed)"
    );
    // Other stages that changed files before the checkpoint would rerun on
    // top of their own changes.
    for (i, state) in source.stages.iter().enumerate() {
        if i == index || upstream.contains(&i) {
            continue;
        }
        let changed = state.attempts.iter().any(|a| {
            a.started_at <= boundary.started_at
                && (a.tree_after.is_none() || a.tree_after != a.tree_before)
        });
        ensure!(
            !changed,
            "Stage {} is not upstream of {stage_id} but changed files before its checkpoint; fork from a stage that runs after it",
            source.workflow.stages[i].name
        );
    }
    let mut workflow = source.workflow.clone();
    if let Some(model) = &model {
        workflow.stages[index].model = model.clone();
    }
    let settings = Settings {
        adapters: source.adapters.clone(),
        providers: source.providers.clone(),
        delegates: source.delegates.clone(),
        ..settings.clone()
    };
    let mut run =
        Run::create_for_fork(project, &settings, &workflow, source.task.clone(), &source)?;
    run.fork_of = Some(engine::Fork {
        run: source.id.clone(),
        stage: stage_id.into(),
        attempt: boundary.number,
        snapshot: tree.clone(),
        model,
    });
    if let Err(error) = crate::snapshot::materialize(&run.workspace, &tree) {
        let message = format!(
            "Could not reproduce the checkpoint in the fork's worktree {}: {error:#}",
            run.workspace.display()
        );
        run.transition(engine::Transition::new(
            crate::transitions::Target::Run,
            Status::Failed,
            crate::transitions::Actor::Engine,
            message.clone(),
        ))?;
        run.error = Some(message.clone());
        run.persist()?;
        anyhow::bail!("{message}. The fork {} was saved as failed", run.id);
    }
    // Changes, and what `ship` commits, are measured from where the source
    // run started, so the inherited upstream work is part of the fork's;
    // the checkpoint when that snapshot is gone.
    run.baseline = source
        .baseline
        .clone()
        .filter(|tree| crate::snapshot::keep(&run.workspace, &run.id, tree))
        .or_else(|| crate::snapshot::tree(&run.workspace, &run.directory));
    for &i in &upstream {
        let from = &source.stages[i];
        if from.status != Status::Complete {
            continue;
        }
        let state = &mut run.stages[i];
        state.status = Status::Complete;
        state.handoff = from.handoff.clone();
        state.waiver = from.waiver.clone();
        // A fork of a fork points at the original attempt.
        state.inherited = from
            .attempts
            .last()
            .map(|a| a.directory.clone())
            .or_else(|| from.inherited.clone());
    }
    run.cursor = run
        .stages
        .iter()
        .position(|s| !matches!(s.status, Status::Complete | Status::Skipped))
        .unwrap_or(run.stages.len());
    run.persist()?;
    Ok(run)
}

/// One attempt in a comparison, with the handoff it saved.
#[derive(Debug, Clone)]
pub struct Side {
    pub run: String,
    /// "attempt 2", or "source attempt 1" across a rerun.
    pub label: String,
    pub attempt: Attempt,
    /// None when the attempt saved no readable handoff.json.
    pub handoff: Option<Handoff>,
}
#[derive(Debug, Clone)]
pub struct Comparison {
    pub stage: String,
    pub a: Side,
    pub b: Side,
}

/// Compares attempt numbers `a` and `b` of the stage at index `stage`.
/// Reads each attempt's handoff.json; writes nothing.
pub fn compare(run: &Run, stage: usize, a: usize, b: usize) -> Result<Comparison> {
    let name = &run
        .workflow
        .stages
        .get(stage)
        .with_context(|| format!("Run {} has no stage {}", run.id, stage + 1))?
        .name;
    Ok(Comparison {
        stage: name.clone(),
        a: side(run, stage, a, format!("attempt {a}"))?,
        b: side(run, stage, b, format!("attempt {b}"))?,
    })
}

/// Compares a stage's last attempt in a rerun's source with its last in the rerun.
pub fn compare_rerun(source: &Run, rerun: &Run, stage_id: &str) -> Result<Comparison> {
    ensure!(
        source_of(rerun) == Some(source.id.as_str()),
        "Run {} is not a rerun or fork of {}",
        rerun.id,
        source.id
    );
    let last = |run: &Run, label: &str| {
        let index = run
            .workflow
            .stages
            .iter()
            .position(|s| s.id == stage_id)
            .with_context(|| format!("Run {} has no stage {stage_id}", run.id))?;
        let number = run.stages[index]
            .attempts
            .last()
            .with_context(|| format!("Stage {stage_id} has no attempts in run {}", run.id))?
            .number;
        Ok::<_, anyhow::Error>((
            index,
            side(run, index, number, format!("{label} attempt {number}"))?,
        ))
    };
    let (index, b) = last(rerun, "rerun")?;
    Ok(Comparison {
        stage: rerun.workflow.stages[index].name.clone(),
        a: last(source, "source")?.1,
        b,
    })
}

/// The run a rerun or fork came from.
pub fn source_of(run: &Run) -> Option<&str> {
    run.rerun_of
        .as_deref()
        .or(run.fork_of.as_ref().map(|f| f.run.as_str()))
}

/// The stage with two or more attempts that ran most recently.
pub fn retried_stage(run: &Run) -> Option<usize> {
    run.stages
        .iter()
        .enumerate()
        .filter(|(_, state)| state.attempts.len() >= 2)
        .max_by_key(|(_, state)| state.attempts.last().map(|a| a.started_at))
        .map(|(index, _)| index)
}

fn side(run: &Run, stage: usize, number: usize, label: String) -> Result<Side> {
    let state = &run.stages[stage];
    let attempt = state
        .attempts
        .iter()
        .find(|a| a.number == number)
        .with_context(|| {
            format!(
                "Stage {} has no attempt {number}",
                run.workflow.stages[stage].name
            )
        })?;
    // The record also holds the latest attempt's handoff.
    let latest = state.attempts.last().is_some_and(|a| a.number == number);
    let handoff = storage::read_json(&attempt.directory.join("handoff.json"))
        .ok()
        .or_else(|| state.handoff.clone().filter(|_| latest));
    Ok(Side {
        run: run.id.clone(),
        label,
        attempt: attempt.clone(),
        handoff,
    })
}

/// A labeled field of an attempt or handoff.
type Row<T, V> = (&'static str, fn(&T) -> V);

impl Comparison {
    /// Field by field; `*` marks a difference.
    pub fn text(&self) -> String {
        let (a, b) = (&self.a, &self.b);
        let mut out = format!("{} · {} vs {}\n", self.stage, a.label, b.label);
        if a.run == b.run {
            out.push_str(&format!("Run {}\n", a.run));
        } else {
            out.push_str(&format!("{}: {}\n{}: {}\n", a.label, a.run, b.label, b.run));
        }
        out.push_str("* marks a difference.\n\n");
        let labels = (a.label.as_str(), b.label.as_str());
        let rows: [Row<Attempt, String>; 6] = [
            ("model", |t| {
                if t.model.is_empty() {
                    "default".into()
                } else {
                    t.model.clone()
                }
            }),
            ("mode", |t| t.mode.label().into()),
            ("status", |t| t.status.label().into()),
            ("exit code", |t| {
                t.exit_code.map_or("none".into(), |code| code.to_string())
            }),
            ("duration", |t| {
                t.finished_at.map_or("unfinished".into(), |end| {
                    format!("{}s", (end - t.started_at).num_seconds().max(0))
                })
            }),
            ("error", |t| t.error.as_deref().map_or("none".into(), flat)),
        ];
        for (label, f) in rows {
            field(&mut out, labels, label, &f(&a.attempt), &f(&b.attempt));
        }
        let checks = |t: &Attempt| -> Option<Vec<String>> {
            Some(
                t.checks
                    .iter()
                    .map(|c| {
                        format!(
                            "{} — {}{}",
                            shell_words::join(&c.command),
                            c.outcome(),
                            optional(c.required)
                        )
                    })
                    .collect(),
            )
        };
        items(&mut out, "checks", checks(&a.attempt), checks(&b.attempt));
        let (p, q) = (a.handoff.as_ref(), b.handoff.as_ref());
        let saved = |h: Option<&Handoff>| match h {
            None => "missing",
            Some(h) if h.structured => "structured",
            Some(_) => "text (unstructured)",
        };
        field(&mut out, labels, "handoff", saved(p), saved(q));
        let fields: [Row<Handoff, String>; 2] = [
            ("verdict", |h| serialized(h.verdict)),
            ("summary", |h| flat(&h.summary)),
        ];
        for (label, f) in fields {
            let text = |h: Option<&Handoff>| h.map_or("–".into(), f);
            field(&mut out, labels, label, &text(p), &text(q));
        }
        let lists: [Row<Handoff, Vec<String>>; 4] = [
            ("findings", |h| {
                h.findings
                    .iter()
                    .map(|f| format!("{}: {}", serialized(f.severity), flat(&f.description)))
                    .collect()
            }),
            ("tests", |h| {
                h.tests
                    .iter()
                    .map(|t| {
                        format!(
                            "{} — {}{}",
                            flat(&t.command),
                            serialized(t.status),
                            optional(t.required)
                        )
                    })
                    .collect()
            }),
            ("files changed", |h| h.files_changed.clone()),
            ("unresolved", |h| {
                h.unresolved.iter().map(|u| flat(u)).collect()
            }),
        ];
        for (label, f) in lists {
            items(&mut out, label, p.map(f), q.map(f));
        }
        out
    }
}

fn field(out: &mut String, (a, b): (&str, &str), name: &str, x: &str, y: &str) {
    if x == y {
        out.push_str(&format!("  {name:<14}{x}\n"));
    } else if x.chars().count() + y.chars().count() <= 56 {
        out.push_str(&format!("* {name:<14}{x} → {y}\n"));
    } else {
        out.push_str(&format!("* {name:<14}{a}: {x}\n  {:<14}{b}: {y}\n", ""));
    }
}

/// Counts, then the items only one side has. None: the handoff is missing.
fn items(out: &mut String, name: &str, x: Option<Vec<String>>, y: Option<Vec<String>>) {
    let sorted = |items: Option<Vec<String>>| {
        items.map(|mut items| {
            items.sort();
            items
        })
    };
    let (x, y) = (sorted(x), sorted(y));
    let count = |items: &Option<Vec<String>>| {
        items
            .as_ref()
            .map_or("missing".into(), |items| items.len().to_string())
    };
    if x == y {
        let same = match &x {
            Some(items) if items.is_empty() => "none".into(),
            Some(items) => format!("{} same", items.len()),
            None => "missing".into(),
        };
        out.push_str(&format!("  {name:<14}{same}\n"));
        return;
    }
    out.push_str(&format!("* {name:<14}{} → {}\n", count(&x), count(&y)));
    let (x, y) = (x.unwrap_or_default(), y.unwrap_or_default());
    let (mut i, mut j) = (0, 0);
    let mut only_y = Vec::new();
    while i < x.len() && j < y.len() {
        match x[i].cmp(&y[j]) {
            std::cmp::Ordering::Less => {
                out.push_str(&format!("  {:<14}- {}\n", "", x[i]));
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                only_y.push(&y[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                let common = &x[i];
                while i < x.len() && &x[i] == common {
                    i += 1;
                }
                while j < y.len() && &y[j] == common {
                    j += 1;
                }
            }
        }
    }
    while i < x.len() {
        out.push_str(&format!("  {:<14}- {}\n", "", x[i]));
        i += 1;
    }
    while j < y.len() {
        only_y.push(&y[j]);
        j += 1;
    }
    for item in only_y {
        out.push_str(&format!("  {:<14}+ {item}\n", ""));
    }
}

fn flat(text: &str) -> String {
    storage::clean_terminal(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn optional(required: bool) -> &'static str {
    if required { "" } else { " (optional)" }
}
/// A value's serialized name, e.g. "needs_changes".
fn serialized(value: impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Bytes in files under `path`, without following symlinks.
pub fn size(path: &Path) -> u64 {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::read_dir(path)
            .map(|items| items.flatten().map(|item| size(&item.path())).sum())
            .unwrap_or(0),
        Ok(meta) => meta.len(),
        Err(_) => 0,
    }
}

/// A run's directory, then its handoff directories in the project or its
/// worktree, then its fan-out candidates' worktrees.
fn artifacts(project: &Path, entry: &Entry) -> Vec<PathBuf> {
    let id = entry.id();
    let root = project.join(".model-router");
    let handoffs = [
        root.join("handoffs").join(id),
        root.join("worktrees")
            .join(id)
            .join(".model-router/handoffs")
            .join(id),
        root.join("candidates").join(id),
        root.join("delegates").join(id),
    ];
    std::iter::once(entry.dir(project))
        .chain(
            handoffs
                .into_iter()
                .filter(|path| fs::symlink_metadata(path).is_ok()),
        )
        .collect()
}

/// Bytes in a run's directory, handoff files, and candidate worktrees. The
/// run's own worktree is not counted.
pub fn run_size(project: &Path, entry: &Entry) -> u64 {
    artifacts(project, entry)
        .iter()
        .map(|path| size(path))
        .sum()
}

/// E.g. "512 B", "4.2 KiB", "13 MiB".
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// Which runs a cleanup selects. Empty and zero fields select everything.
#[derive(Debug, Clone, Default)]
pub struct Criteria {
    /// Only runs last updated more than this many days ago.
    pub older_than_days: Option<u64>,
    pub statuses: Vec<StatusFilter>,
    /// Never select the newest N runs.
    pub keep: usize,
    /// Also remove each selected run's worktree, when it is clean.
    pub worktrees: bool,
    /// Only these run IDs.
    pub ids: Vec<String>,
    /// Runs to leave, e.g. the one open in the TUI.
    pub protect: Vec<String>,
}
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub project: PathBuf,
    pub items: Vec<Item>,
    /// Selected runs that stay, as (ID, reason).
    pub skipped: Vec<(String, String)>,
}
#[derive(Debug, Clone)]
pub struct Item {
    pub id: String,
    /// A run status label, or "damaged".
    pub status: &'static str,
    /// The run directory, then its handoff directories, with their sizes.
    pub paths: Vec<(PathBuf, u64)>,
    /// A clean worktree removed with the run, and its size.
    pub worktree: Option<(PathBuf, u64)>,
    /// Why the run's worktree stays.
    pub note: Option<String>,
}
impl Item {
    pub fn bytes(&self) -> u64 {
        let inside = |path: &Path| {
            self.worktree
                .as_ref()
                .is_some_and(|(tree, _)| path.starts_with(tree))
        };
        let paths: u64 = self
            .paths
            .iter()
            .filter(|(path, _)| !inside(path))
            .map(|(_, bytes)| bytes)
            .sum();
        paths + self.worktree.as_ref().map_or(0, |(_, bytes)| *bytes)
    }
}

const IN_USE: &str = "In use: its run.lock is held";

/// Selects runs and sizes what removing them frees. Removes nothing.
pub fn cleanup_plan(project: &Path, criteria: &Criteria) -> Result<Plan> {
    let now = Utc::now();
    let mut plan = Plan {
        project: project.to_path_buf(),
        ..Plan::default()
    };
    for (index, entry) in list(project)?.iter().enumerate() {
        let id = entry.id();
        let old = |days: u64| {
            let limit = i64::try_from(days.saturating_mul(86_400)).unwrap_or(i64::MAX);
            entry
                .updated_at()
                .is_some_and(|time| (now - time).num_seconds() > limit)
        };
        if index < criteria.keep
            || (!criteria.ids.is_empty() && !criteria.ids.iter().any(|i| i == id))
            || (!criteria.statuses.is_empty()
                && !criteria.statuses.iter().any(|s| s.matches(entry)))
            || criteria.older_than_days.is_some_and(|days| !old(days))
        {
            continue;
        }
        if criteria.protect.iter().any(|p| p == id) {
            plan.skipped
                .push((id.into(), "Open in this session".into()));
            continue;
        }
        // A missing lock file is free; checking must not create one.
        let lock = entry.dir(project).join("run.lock");
        if storage::is_locked(&lock) {
            plan.skipped.push((id.into(), IN_USE.into()));
            continue;
        }
        let mut item = Item {
            id: id.into(),
            status: entry.status_label(),
            paths: artifacts(project, entry)
                .into_iter()
                .map(|path| {
                    let bytes = size(&path);
                    (path, bytes)
                })
                .collect(),
            worktree: None,
            note: None,
        };
        let tree = project.join(".model-router/worktrees").join(id);
        if criteria.worktrees && fs::symlink_metadata(&tree).is_ok() {
            match removable(&tree) {
                Ok(()) => item.worktree = Some((tree.clone(), size(&tree))),
                Err(error) => item.note = Some(format!("Worktree kept: {error:#}")),
            }
        }
        plan.items.push(item);
    }
    Ok(plan)
}

/// Ok when `git worktree remove` would take `tree` without --force: no
/// changes, no untracked files, and no initialized submodules.
fn removable(tree: &Path) -> Result<()> {
    ensure!(tree.join(".git").is_file(), "not a linked Git worktree");
    ensure!(
        !storage::has_populated_submodules(tree)?,
        "contains submodules"
    );
    ensure!(
        storage::git_readonly(
            tree,
            &["status", "--porcelain", "--ignore-submodules=dirty"]
        )?
        .is_empty(),
        "uncommitted changes"
    );
    // Removal is forced, so a status the router cannot trust keeps the tree.
    crate::snapshot::refuse_unverifiable_clean(tree)
}

/// Removes a plan's runs: a planned worktree first, then handoff and run
/// directories. Returns what was removed; locked or failed runs are skipped.
pub fn apply(plan: &Plan) -> Plan {
    let mut done = Plan {
        project: plan.project.clone(),
        items: vec![],
        skipped: plan.skipped.clone(),
    };
    for item in &plan.items {
        match remove(&plan.project, item) {
            Ok(item) => done.items.push(item),
            Err(error) => done.skipped.push((item.id.clone(), format!("{error:#}"))),
        }
    }
    done
}

fn remove(project: &Path, item: &Item) -> Result<Item> {
    let dir = &item.paths.first().context("No run directory")?.0;
    ensure!(dir.is_dir(), "Already removed");
    let _lock = storage::lock(&dir.join("run.lock")).map_err(|err| {
        if err
            .root_cause()
            .downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::WouldBlock)
        {
            anyhow!(IN_USE)
        } else {
            err
        }
    })?;
    let mut done = item.clone();
    if let Some((tree, _)) = &item.worktree {
        // Git's own check before removal runs with the repository's config,
        // filters included, so the router checks with its sanitized config
        // and only then removes with --force. Branches stay.
        let removed = removable(tree).and_then(|()| {
            let path = tree.to_str().context("Worktree path must be UTF-8")?;
            storage::git(project, &["worktree", "remove", "--force", path]).map(|_| ())
        });
        if let Err(error) = removed {
            done.worktree = None;
            done.note = Some(format!("Worktree kept: {error:#}"));
        }
    }
    let mut first_err: Option<anyhow::Error> = None;
    let mut record = |res: Result<()>| {
        if let Err(err) = res {
            first_err.get_or_insert(err);
        }
    };
    // Candidate worktrees are the router's own, kept for inspection only.
    record(crate::fanout::remove_worktrees(
        project,
        &project.join(".model-router/candidates").join(&item.id),
    ));
    // Delegated tasks' worktrees too.
    record(crate::fanout::remove_worktrees(
        project,
        &project.join(".model-router/delegates").join(&item.id),
    ));
    let mut run_dir_removed = false;
    for (idx, (path, _)) in item.paths.iter().enumerate().rev() {
        let res = remove_path(path).with_context(|| format!("Remove {}", path.display()));
        if res.is_ok() && idx == 0 {
            run_dir_removed = true;
        }
        record(res);
    }
    if run_dir_removed {
        // Its snapshots become unreachable, so `git gc` may prune them.
        crate::snapshot::forget(project, &item.id);
    }
    if let Some(err) = first_err {
        return Err(err);
    }
    Ok(done)
}

fn remove_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

impl Plan {
    pub fn bytes(&self) -> u64 {
        self.items.iter().map(Item::bytes).sum()
    }
    /// Runs with sizes and paths relative to the project, then skipped runs.
    /// `removed` words an applied plan.
    pub fn text(&self, removed: bool) -> String {
        let relative = |path: &Path| {
            path.strip_prefix(&self.project)
                .unwrap_or(path)
                .display()
                .to_string()
        };
        let mut out = String::new();
        if !self.items.is_empty() {
            out.push_str(&format!(
                "{} {} run{}, {}:\n",
                if removed { "Removed" } else { "Would remove" },
                self.items.len(),
                if self.items.len() == 1 { "" } else { "s" },
                human(self.bytes())
            ));
        }
        for item in &self.items {
            out.push_str(&format!(
                "  {}  {}  {}\n",
                item.id,
                item.status,
                human(item.bytes())
            ));
            for (path, bytes) in &item.paths {
                out.push_str(&format!("    {}  {}\n", relative(path), human(*bytes)));
            }
            if let Some((tree, bytes)) = &item.worktree {
                out.push_str(&format!(
                    "    worktree {}  {}\n",
                    relative(tree),
                    human(*bytes)
                ));
            }
            if let Some(note) = &item.note {
                out.push_str(&format!("    {note}\n"));
            }
        }
        if !self.skipped.is_empty() {
            out.push_str("Skipped:\n");
            for (id, reason) in &self.skipped {
                out.push_str(&format!("  {id}  {reason}\n"));
            }
        }
        if out.is_empty() {
            out.push_str("Nothing to clean.\n");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Role, Stage, Workflow};

    #[test]
    fn items_two_pointer_diff_preserves_order_and_duplicate_semantics() {
        let mut out = String::new();
        items(
            &mut out,
            "files",
            Some(vec!["b".into(), "a".into(), "a".into(), "d".into()]),
            Some(vec!["c".into(), "a".into(), "e".into()]),
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "* files         4 → 3");
        assert_eq!(
            &lines[1..],
            [
                "                - b",
                "                - d",
                "                + c",
                "                + e",
            ]
        );
    }

    #[test]
    fn remove_surfaces_non_lock_errors_without_masking_as_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join(".model-router/runs/r-err");
        fs::create_dir_all(run_dir.join("run.lock")).unwrap();
        let item = Item {
            id: "r-err".into(),
            status: "failed",
            paths: vec![(run_dir, 0)],
            worktree: None,
            note: None,
        };
        let err = remove(dir.path(), &item).unwrap_err().to_string();
        assert!(err != IN_USE && !err.contains("In use"), "{err}");
    }

    #[test]
    fn fork_rejects_overlapping_or_crashed_non_upstream_stages() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        fs::write(dir.path().join("f.txt"), "one\n").unwrap();
        let workflow = Workflow::new(
            "Graph",
            vec![
                Stage {
                    id: "a".into(),
                    name: "A".into(),
                    role: Role::Research,
                    harness: "mock".into(),
                    ..Stage::default()
                },
                Stage {
                    id: "b".into(),
                    name: "B".into(),
                    role: Role::Implementation,
                    harness: "mock".into(),
                    ..Stage::default()
                },
                Stage {
                    id: "c".into(),
                    name: "C".into(),
                    role: Role::Test,
                    harness: "mock".into(),
                    after: vec!["a".into()],
                    ..Stage::default()
                },
            ],
        );
        let mut settings = Settings {
            workflows: vec![workflow.clone()],
            ..Settings::default()
        };
        settings.adapters.insert(
            "mock".into(),
            crate::config::CustomAdapter {
                command: "python3".into(),
                ..Default::default()
            },
        );
        let mut run = Run::create(dir.path(), &settings, &workflow, "Task".into(), false).unwrap();
        let t0 = Utc::now();
        let tree_a = run.baseline.clone().unwrap();
        run.stages[0].status = Status::Complete;
        run.stages[0].handoff = Some(Handoff::unstructured("done".to_string()));
        run.stages[0].attempts.push(Attempt {
            number: 1,
            started_at: t0,
            finished_at: Some(t0 + chrono::Duration::seconds(1)),
            model: "m".into(),
            mode: crate::config::Mode::Unattended,
            status: Status::Complete,
            exit_code: Some(0),
            directory: run.directory.join("01-a/attempt-001"),
            handoff_file: run.directory.join("01-a/attempt-001/handoff.json"),
            error: None,
            checks: vec![],
            tree_before: Some(tree_a.clone()),
            tree_after: Some(tree_a.clone()),
            checks_tree: None,
            usage: None,
            session: None,
            process_group: None,
            process_identity: Default::default(),
            terminal: None,
            handoff_source: None,
            worktree: None,
            tasks: vec![],
            turns: vec![],
            launched_runs: vec![],
        });
        run.stages[2].status = Status::Failed;
        run.stages[2].attempts.push(Attempt {
            number: 1,
            started_at: t0 + chrono::Duration::seconds(2),
            finished_at: Some(t0 + chrono::Duration::seconds(3)),
            model: "m".into(),
            mode: crate::config::Mode::Unattended,
            status: Status::Failed,
            exit_code: Some(1),
            directory: run.directory.join("03-c/attempt-001"),
            handoff_file: run.directory.join("03-c/attempt-001/handoff.json"),
            error: None,
            checks: vec![],
            tree_before: Some(tree_a.clone()),
            tree_after: Some(tree_a.clone()),
            checks_tree: None,
            usage: None,
            session: None,
            process_group: None,
            process_identity: Default::default(),
            terminal: None,
            handoff_source: None,
            worktree: None,
            tasks: vec![],
            turns: vec![],
            launched_runs: vec![],
        });

        // Stage b started before checkpoint c and crashed (tree_after: None).
        run.stages[1].status = Status::Failed;
        run.stages[1].attempts.push(Attempt {
            number: 1,
            started_at: t0,
            finished_at: None,
            model: "m".into(),
            mode: crate::config::Mode::Unattended,
            status: Status::Failed,
            exit_code: None,
            directory: run.directory.join("02-b/attempt-001"),
            handoff_file: run.directory.join("02-b/attempt-001/handoff.json"),
            error: None,
            checks: vec![],
            tree_before: Some(tree_a.clone()),
            tree_after: None,
            checks_tree: None,
            usage: None,
            session: None,
            process_group: None,
            process_identity: Default::default(),
            terminal: None,
            handoff_source: None,
            worktree: None,
            tasks: vec![],
            turns: vec![],
            launched_runs: vec![],
        });
        run.persist().unwrap();
        let err = fork(dir.path(), &settings, &run.id, "c", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not upstream of c"), "{err}");

        // Stage b started before checkpoint c and finished after, modifying files.
        let mut loaded = engine::load(dir.path(), &run.id).unwrap();
        loaded.stages[1].attempts[0].finished_at = Some(t0 + chrono::Duration::seconds(5));
        loaded.stages[1].attempts[0].tree_after = Some("0".repeat(40));
        loaded.persist().unwrap();
        let err = fork(dir.path(), &settings, &run.id, "c", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not upstream of c"), "{err}");
    }
}
