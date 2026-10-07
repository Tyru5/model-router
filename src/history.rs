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
    entries.sort_by(|a, b| {
        b.created_at()
            .cmp(&a.created_at())
            .then_with(|| b.id().cmp(a.id()))
    });
    Ok(entries)
}

fn read_summary(project: &Path, id: &str, dir: &Path) -> Result<RunSummary> {
    if let Ok(summary) = storage::read_json::<RunSummary>(&dir.join("summary.json")) {
        if summary.id == id {
            return Ok(summary);
        }
    }
    Ok(RunSummary::from(&engine::load(project, id)?))
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
    let worktree = match worktree {
        Some(worktree) => worktree,
        None => engine::load(project, id)?.worktree,
    };
    let placement = if worktree {
        Placement::Worktree
    } else {
        Placement::Project
    };
    rerun_at(project, settings, id, placement)
}

/// Reruns saved run `id` as a new run working at `placement`.
pub fn rerun_at(
    project: &Path,
    settings: &Settings,
    id: &str,
    placement: Placement,
) -> Result<Run> {
    let source = engine::load(project, id)?;
    let settings = Settings {
        adapters: source.adapters.clone(),
        providers: source.providers.clone(),
        ..settings.clone()
    };
    let mut run = Run::create_at(
        project,
        &settings,
        &source.workflow,
        source.task.clone(),
        placement,
    )?;
    run.rerun_of = Some(source.id);
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
        rerun.rerun_of.as_deref() == Some(source.id.as_str()),
        "Run {} is not a rerun of {}",
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
    for item in x.iter().filter(|item| !y.contains(item)) {
        out.push_str(&format!("  {:<14}- {item}\n", ""));
    }
    for item in y.iter().filter(|item| !x.contains(item)) {
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

/// A run's directory, then its handoff directories in the project or its worktree.
fn artifacts(project: &Path, entry: &Entry) -> Vec<PathBuf> {
    let id = entry.id();
    let root = project.join(".model-router");
    let handoffs = [
        root.join("handoffs").join(id),
        root.join("worktrees")
            .join(id)
            .join(".model-router/handoffs")
            .join(id),
    ];
    std::iter::once(entry.dir(project))
        .chain(
            handoffs
                .into_iter()
                .filter(|path| fs::symlink_metadata(path).is_ok()),
        )
        .collect()
}

/// Bytes in a run's directory and handoff files. Worktrees are not counted.
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
            plan.skipped.push((id.into(), "Open in the monitor".into()));
            continue;
        }
        // A missing lock file is free; checking must not create one.
        let lock = entry.dir(project).join("run.lock");
        if lock.exists() && storage::lock(&lock).is_err() {
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

/// Ok when `git worktree remove` takes `tree` without --force.
fn removable(tree: &Path) -> Result<()> {
    ensure!(tree.join(".git").is_file(), "not a linked Git worktree");
    ensure!(
        storage::git(tree, &["status", "--porcelain"])?.is_empty(),
        "uncommitted changes"
    );
    Ok(())
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
    let _lock = storage::lock(&dir.join("run.lock")).map_err(|_| anyhow!(IN_USE))?;
    let mut done = Item {
        paths: item
            .paths
            .iter()
            .map(|(path, _)| (path.clone(), size(path)))
            .collect(),
        worktree: item
            .worktree
            .as_ref()
            .map(|(tree, _)| (tree.clone(), size(tree))),
        ..item.clone()
    };
    if let Some((tree, _)) = &item.worktree {
        // Never --force: Git refuses worktrees with changes. Branches stay.
        let removed = tree
            .to_str()
            .context("Worktree path must be UTF-8")
            .and_then(|path| storage::git(project, &["worktree", "remove", path]));
        if let Err(error) = removed {
            done.worktree = None;
            done.note = Some(format!("Worktree kept: {error:#}"));
        }
    }
    for (path, _) in item.paths.iter().rev() {
        remove_path(path).with_context(|| format!("Remove {}", path.display()))?;
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
