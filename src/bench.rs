//! Benchmarks: run each task of a bench file under each combination of stage
//! assignments, every run unattended in its own Git worktree, and summarize
//! the outcomes. `model-router bench <bench.toml>`.
use crate::{
    config::{Mode, Settings, Workflow},
    engine::{self, Action, Placement, Run, Status},
    handoff::{TestStatus, Usage},
    report, storage, task_import,
};
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

/// A bench file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bench {
    /// Base workflow; default: the first configured.
    #[serde(default)]
    pub workflow: Option<String>,
    pub tasks: Vec<BenchTask>,
    /// Stages to vary. Variants are every combination of their assignments.
    #[serde(default)]
    pub matrix: Vec<Axis>,
    /// Runs of each (task, variant). Default 1.
    #[serde(default = "one")]
    pub repeat: usize,
}
fn one() -> usize {
    1
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchTask {
    /// Default: "task-N".
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub text: Option<String>,
    /// Markdown or text task file, relative to the bench file.
    #[serde(default)]
    pub file: Option<PathBuf>,
}

/// One stage and the assignments to try in it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Axis {
    pub stage: String,
    pub assignments: Vec<Assignment>,
}

/// A stage's harness and model. Fields left out keep the workflow's values;
/// changing the harness clears its provider, effort, settings, and extra
/// arguments unless they are given here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_args: Option<Vec<String>>,
}

/// One combination of assignments, by stage ID.
#[derive(Debug, Clone, Serialize)]
pub struct Variant {
    pub label: String,
    pub assignments: Vec<(String, Assignment)>,
}

/// One (task, variant, repetition) run.
#[derive(Debug, Clone, Serialize)]
pub struct Cell {
    pub task: String,
    pub variant: usize,
    pub repetition: usize,
}

pub struct Options {
    pub jobs: usize,
    /// Run interactive stages unattended instead of refusing them.
    pub allow_interactive: bool,
}

/// A loaded bench: tasks read, variants expanded, workflows checked.
pub struct Plan {
    pub source: PathBuf,
    pub workflow: Workflow,
    pub tasks: Vec<(String, String)>,
    pub variants: Vec<Variant>,
    pub cells: Vec<Cell>,
}

pub fn load(path: &Path) -> Result<Bench> {
    let text = fs::read_to_string(path).with_context(|| format!("Read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("Parse {}", path.display()))
}

impl Plan {
    pub fn new(
        source: &Path,
        bench: &Bench,
        settings: &Settings,
        options: &Options,
    ) -> Result<Self> {
        ensure!(!bench.tasks.is_empty(), "The bench lists no tasks");
        ensure!((1..=100).contains(&bench.repeat), "repeat must be 1–100");
        let mut workflow = settings.workflow(bench.workflow.as_deref())?.clone();
        let interactive: Vec<_> = workflow
            .stages
            .iter()
            .filter(|s| s.enabled && s.mode == Mode::Interactive)
            .map(|s| s.id.clone())
            .collect();
        ensure!(
            interactive.is_empty() || options.allow_interactive,
            "Benchmarks run unattended, but workflow {} has interactive stages: {}. Make them unattended, or pass --allow-interactive to run them unattended",
            workflow.name,
            interactive.join(", ")
        );
        for stage in &mut workflow.stages {
            stage.mode = Mode::Unattended;
        }
        let base = source.parent().unwrap_or(Path::new("."));
        let mut tasks = vec![];
        for (index, task) in bench.tasks.iter().enumerate() {
            let name = if task.name.trim().is_empty() {
                format!("task-{}", index + 1)
            } else {
                task.name.trim().to_string()
            };
            ensure!(
                tasks.iter().all(|(n, _)| *n != name),
                "Two tasks are named {name}"
            );
            let text = match (&task.text, &task.file) {
                (Some(text), None) => text.clone(),
                (None, Some(file)) => {
                    task_import::read(&base.join(file)).with_context(|| format!("Task {name}"))?
                }
                _ => bail!("Task {name} needs exactly one of text or file"),
            };
            crate::config::validate_task(&text).with_context(|| format!("Task {name}"))?;
            tasks.push((name, text));
        }
        let mut variants = vec![Variant {
            label: String::new(),
            assignments: vec![],
        }];
        for axis in &bench.matrix {
            let stage = workflow
                .stages
                .iter()
                .find(|s| s.id == axis.stage)
                .with_context(|| {
                    format!("Workflow {} has no stage {}", workflow.name, axis.stage)
                })?;
            ensure!(
                stage.enabled,
                "Stage {} is disabled in workflow {}",
                axis.stage,
                workflow.name
            );
            ensure!(
                !axis.assignments.is_empty(),
                "Stage {} lists no assignments",
                axis.stage
            );
            ensure!(
                bench
                    .matrix
                    .iter()
                    .filter(|a| a.stage == axis.stage)
                    .count()
                    == 1,
                "Stage {} appears twice in the matrix",
                axis.stage
            );
            let mut next = vec![];
            for variant in &variants {
                for assignment in &axis.assignments {
                    let mut assignments = variant.assignments.clone();
                    assignments.push((axis.stage.clone(), assignment.clone()));
                    next.push(Variant {
                        label: String::new(),
                        assignments,
                    });
                }
            }
            variants = next;
        }
        ensure!(
            variants.len() * tasks.len() * bench.repeat <= 1000,
            "The bench expands to more than 1000 runs"
        );
        for variant in &mut variants {
            let applied = apply(&workflow, variant)?;
            Settings {
                workflows: vec![applied.clone()],
                ..settings.clone()
            }
            .validate()
            .with_context(|| format!("Variant {}", label(&applied, variant)))?;
            variant.label = label(&applied, variant);
        }
        let cells = tasks
            .iter()
            .flat_map(|(task, _)| {
                (0..variants.len()).flat_map(move |variant| {
                    (0..bench.repeat).map(move |repetition| Cell {
                        task: task.clone(),
                        variant,
                        repetition,
                    })
                })
            })
            .collect();
        Ok(Self {
            source: source.to_path_buf(),
            workflow,
            tasks,
            variants,
            cells,
        })
    }

    pub fn workflow_for(&self, variant: usize) -> Result<Workflow> {
        apply(&self.workflow, &self.variants[variant])
    }

    /// The matrix without running anything.
    pub fn preview(&self) -> String {
        let mut text = format!(
            "Bench {}\nWorkflow: {} (all stages unattended)\n{} task(s) × {} variant(s) = {} run(s), each in its own Git worktree\n\nVARIANTS\n",
            self.source.display(),
            self.workflow.name,
            self.tasks.len(),
            self.variants.len(),
            self.cells.len()
        );
        for (index, variant) in self.variants.iter().enumerate() {
            text.push_str(&format!("  {}. {}\n", index + 1, variant.label));
        }
        text.push_str("\nTASKS\n");
        for (name, task) in &self.tasks {
            text.push_str(&format!("  {name}: {}\n", crate::history::excerpt(task)));
        }
        text
    }
}

/// The base workflow with a variant's assignments applied.
fn apply(base: &Workflow, variant: &Variant) -> Result<Workflow> {
    let mut workflow = base.clone();
    for (id, assignment) in &variant.assignments {
        let stage = workflow
            .stages
            .iter_mut()
            .find(|s| s.id == *id)
            .with_context(|| format!("No stage {id}"))?;
        if let Some(harness) = &assignment.harness {
            if *harness != stage.harness {
                stage.harness = harness.clone();
                stage.provider.clear();
                stage.effort.clear();
                stage.harness_settings.clear();
                stage.extra_args.clear();
            }
        }
        if let Some(model) = &assignment.model {
            stage.model = model.clone();
        }
        if let Some(provider) = &assignment.provider {
            stage.provider = provider.clone();
        }
        if let Some(effort) = &assignment.effort {
            stage.effort = effort.clone();
        }
        if let Some(args) = &assignment.extra_args {
            stage.extra_args = args.clone();
        }
    }
    Ok(workflow)
}

/// "implement=claude/opus, review=codex/gpt-6-astra", or the base workflow.
fn label(workflow: &Workflow, variant: &Variant) -> String {
    if variant.assignments.is_empty() {
        return "base workflow".into();
    }
    variant
        .assignments
        .iter()
        .filter_map(|(id, _)| workflow.stages.iter().find(|s| s.id == *id))
        .map(|s| {
            format!(
                "{}={}/{}",
                s.id,
                s.harness,
                if s.model.is_empty() {
                    "default"
                } else {
                    &s.model
                }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The outcome of one cell.
#[derive(Debug, Clone, Serialize)]
pub struct CellResult {
    pub task: String,
    pub variant: String,
    pub repetition: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    /// "complete", "failed", "blocked", …, "error" (never started), or
    /// "skipped" (cancelled before starting).
    pub status: String,
    pub success: bool,
    pub checks_passed: usize,
    pub checks_total: usize,
    pub repairs: u32,
    pub retries: usize,
    pub seconds: i64,
    pub usage: Usage,
    /// When the workflow configures required router checks: whether the run
    /// completed with every one passing. None when it configures none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    /// Estimated cost of the run; None when any attempt's cost is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CellResult {
    fn new(cell: &Cell, variant: &str, status: &str, error: Option<String>) -> Self {
        Self {
            task: cell.task.clone(),
            variant: variant.into(),
            repetition: cell.repetition,
            run: None,
            workspace: None,
            status: status.into(),
            success: false,
            checks_passed: 0,
            checks_total: 0,
            repairs: 0,
            retries: 0,
            seconds: 0,
            usage: Usage::default(),
            verified: None,
            cost_usd: None,
            error,
        }
    }
    fn from_run(cell: &Cell, variant: &str, run: &Run) -> Self {
        let summary = report::build(run);
        // Router checks and model-reported tests of each stage's final attempt.
        let (mut passed, mut total) = (0, 0);
        let mut all_required_passed = true;
        for (stage, state) in run.workflow.stages.iter().zip(&run.stages) {
            let configured = stage.checks.iter().filter(|c| c.required).count();
            // Every configured required check must have run and passed; a
            // fan-out stage's checks are its chosen candidate's.
            let attempt = state.checked();
            let required: Vec<_> = attempt
                .iter()
                .flat_map(|a| &a.checks)
                .filter(|c| c.required)
                .collect();
            all_required_passed &= !stage.enabled
                || configured == 0
                || (required.len() == configured
                    && required.iter().all(|c| c.status == TestStatus::Passed));
            if let Some(attempt) = attempt {
                total += attempt.checks.len();
                passed += attempt
                    .checks
                    .iter()
                    .filter(|c| c.status == TestStatus::Passed)
                    .count();
            }
        }
        // Evidence when the workflow configures a required router check; a
        // pass needs the run to complete with every required check passing,
        // so a stage that failed before its checks counts against it.
        let configured = run
            .workflow
            .stages
            .iter()
            .any(|s| s.enabled && s.checks.iter().any(|c| c.required));
        let verified = configured.then_some(run.status == Status::Complete && all_required_passed);
        Self {
            run: Some(run.id.clone()),
            workspace: Some(run.workspace.clone()),
            status: run.status.label().replace(' ', "_"),
            success: run.status == Status::Complete,
            checks_passed: passed,
            checks_total: total,
            repairs: run.repair_passes,
            retries: summary.retries,
            seconds: summary.wall_seconds,
            usage: summary.usage.sum,
            verified,
            cost_usd: (summary.usage.cost.unknown == 0).then_some(summary.usage.cost.usd),
            error: run.error.as_deref().map(crate::history::excerpt),
            ..Self::new(cell, variant, "", None)
        }
    }
}

/// A finished bench, as saved.
#[derive(Debug, Clone, Serialize)]
pub struct Results {
    pub bench: PathBuf,
    pub workflow: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub jobs: usize,
    pub variants: Vec<Variant>,
    pub results: Vec<CellResult>,
}

/// Runs every cell, `options.jobs` at a time, each in a new worktree of
/// `project`. Cancellation stops running stages and skips cells not yet
/// started. `progress` receives a line as each cell starts and ends.
pub fn run(
    project: &Path,
    settings: &Settings,
    plan: &Plan,
    options: &Options,
    cancel: &AtomicBool,
    progress: &(dyn Fn(String) + Sync),
) -> Result<Results> {
    ensure!(
        storage::repo_info(project).is_some(),
        "Benchmarks run each cell in its own Git worktree; {} is not a Git repository",
        project.display()
    );
    let started_at = Utc::now();
    let next = AtomicUsize::new(0);
    // Worktree creation takes Git locks in the project; one at a time.
    let creating = Mutex::new(());
    let results: Mutex<Vec<Option<CellResult>>> = Mutex::new(vec![None; plan.cells.len()]);
    let jobs = options.jobs.clamp(1, 32).min(plan.cells.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(cell) = plan.cells.get(index) else {
                        break;
                    };
                    let variant = &plan.variants[cell.variant].label;
                    let result = if cancel.load(Ordering::Relaxed) {
                        CellResult::new(cell, variant, "skipped", Some("Bench cancelled".into()))
                    } else {
                        progress(format!(
                            "[{}/{}] start {} · {}",
                            index + 1,
                            plan.cells.len(),
                            cell.task,
                            variant
                        ));
                        let result = run_cell(project, settings, plan, cell, cancel, &creating);
                        progress(format!(
                            "[{}/{}] {} {} · {}{}",
                            index + 1,
                            plan.cells.len(),
                            result.status,
                            cell.task,
                            variant,
                            result
                                .run
                                .as_deref()
                                .map_or_else(String::new, |id| format!(" (run {id})"))
                        ));
                        result
                    };
                    results.lock().unwrap_or_else(|e| e.into_inner())[index] = Some(result);
                }
            });
        }
    });
    let results = results
        .into_inner()
        .unwrap_or_else(|e| e.into_inner())
        .into_iter()
        .zip(&plan.cells)
        .map(|(result, cell)| {
            result.unwrap_or_else(|| {
                CellResult::new(cell, &plan.variants[cell.variant].label, "skipped", None)
            })
        })
        .collect();
    Ok(Results {
        bench: plan.source.clone(),
        workflow: plan.workflow.name.clone(),
        started_at,
        finished_at: Utc::now(),
        jobs,
        variants: plan.variants.clone(),
        results,
    })
}

fn run_cell(
    project: &Path,
    settings: &Settings,
    plan: &Plan,
    cell: &Cell,
    cancel: &AtomicBool,
    creating: &Mutex<()>,
) -> CellResult {
    let variant = &plan.variants[cell.variant].label;
    let failed =
        |error: anyhow::Error| CellResult::new(cell, variant, "error", Some(format!("{error:#}")));
    let workflow = match plan.workflow_for(cell.variant) {
        Ok(workflow) => workflow,
        Err(error) => return failed(error),
    };
    let task = plan
        .tasks
        .iter()
        .find(|(name, _)| *name == cell.task)
        .map(|(_, text)| text.clone())
        .unwrap_or_default();
    let created = {
        let _guard = creating.lock().unwrap_or_else(|e| e.into_inner());
        Run::create_at(project, settings, &workflow, task, Placement::Worktree)
    };
    let mut run = match created {
        Ok(run) => run,
        Err(error) => return failed(error),
    };
    if let Err(error) = drive(&mut run, cancel) {
        let mut result = CellResult::from_run(cell, variant, &run);
        result.error = Some(format!("{error:#}"));
        if result.status != "complete" {
            result.status = "error".into();
        }
        return result;
    }
    CellResult::from_run(cell, variant, &run)
}

/// Drives a run until it stops; a checkpoint awaiting confirmation stops it.
fn drive(run: &mut Run, cancel: &AtomicBool) -> Result<()> {
    match engine::drive(run, cancel, |_| {})? {
        Action::Complete | Action::Stopped => Ok(()),
        Action::Confirm => bail!(
            "Stage {} awaits confirmation; inspect it with: model-router resume {}",
            run.current_name(),
            run.id
        ),
        Action::Interactive => bail!("Stage {} is interactive", run.current_name()),
    }
}

/// Per-variant totals, then every cell, as Markdown.
pub fn markdown(results: &Results) -> String {
    let mut text = format!(
        "# Bench {}\n\nWorkflow: {} · {} run(s) · jobs {} · {} → {}\n\n| Variant | Success | Checks | Repairs | Median duration | Tokens | Cost |\n|---|---|---|---|---|---|---|\n",
        results.bench.display(),
        results.workflow,
        results.results.len(),
        results.jobs,
        results.started_at.format("%Y-%m-%d %H:%M:%S"),
        results.finished_at.format("%H:%M:%S")
    );
    for variant in &results.variants {
        let cells: Vec<_> = results
            .results
            .iter()
            .filter(|r| r.variant == variant.label)
            .collect();
        let success = cells.iter().filter(|r| r.success).count();
        let passed: usize = cells.iter().map(|r| r.checks_passed).sum();
        let total: usize = cells.iter().map(|r| r.checks_total).sum();
        let repairs: u32 = cells.iter().map(|r| r.repairs).sum();
        let mut durations: Vec<_> = cells
            .iter()
            .filter(|r| r.run.is_some())
            .map(|r| r.seconds)
            .collect();
        durations.sort_unstable();
        let usage = cells.iter().fold(Usage::default(), |sum, r| sum + r.usage);
        text.push_str(&format!(
            "| {} | {success}/{} | {} | {repairs} | {} | {} | {} |\n",
            variant.label,
            cells.len(),
            if total == 0 {
                "-".into()
            } else {
                format!("{passed}/{total}")
            },
            crate::stats::median(&durations).map_or("-".into(), report::duration),
            tokens(&usage),
            usage.cost_usd.map_or("-".into(), report::dollars),
        ));
    }
    text.push_str("\n| Task | Variant | Status | Checks | Repairs | Retries | Duration | Tokens | Run |\n|---|---|---|---|---|---|---|---|---|\n");
    for r in &results.results {
        text.push_str(&format!(
            "| {}{} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            r.task,
            if r.repetition > 0 {
                format!(" #{}", r.repetition + 1)
            } else {
                String::new()
            },
            r.variant,
            r.status,
            if r.checks_total == 0 {
                "-".into()
            } else {
                format!("{}/{}", r.checks_passed, r.checks_total)
            },
            r.repairs,
            r.retries,
            if r.run.is_some() {
                report::duration(r.seconds)
            } else {
                "-".into()
            },
            tokens(&r.usage),
            r.run.as_deref().unwrap_or("-"),
        ));
    }
    let errors: Vec<_> = results
        .results
        .iter()
        .filter_map(|r| Some((r, r.error.as_deref()?)))
        .collect();
    if !errors.is_empty() {
        text.push_str("\nErrors:\n");
        for (r, error) in errors {
            text.push_str(&format!(
                "- {} · {}: {}\n",
                r.task,
                r.variant,
                error.replace('\n', " ")
            ));
        }
    }
    text
}

/// The advisory cost-quality shortlist over variants: those whose cells'
/// required router checks all passed at least `threshold` of the time.
/// Cells that never started count as samples without evidence.
pub fn shortlist(results: &Results, threshold: f64) -> crate::stats::Shortlist {
    let rows: Vec<_> = results
        .variants
        .iter()
        .map(|variant| {
            let cells: Vec<_> = results
                .results
                .iter()
                .filter(|r| r.variant == variant.label)
                .collect();
            let mut durations: Vec<_> = cells
                .iter()
                .filter(|r| r.run.is_some())
                .map(|r| r.seconds)
                .collect();
            durations.sort_unstable();
            crate::stats::Tradeoff {
                assignment: variant.label.clone(),
                role: None,
                samples: cells.len(),
                verified: cells.iter().filter(|r| r.verified.is_some()).count(),
                passed: cells.iter().filter(|r| r.verified == Some(true)).count(),
                cost_known: cells.iter().filter(|r| r.cost_usd.is_some()).count(),
                cost_usd: cells.iter().filter_map(|r| r.cost_usd).sum(),
                median_seconds: crate::stats::median(&durations),
            }
        })
        .collect();
    crate::stats::shortlist(&rows, threshold)
}

fn tokens(usage: &Usage) -> String {
    match (usage.input_tokens, usage.output_tokens) {
        (None, None) => "-".into(),
        (a, b) => report::tokens(a.unwrap_or(0) + b.unwrap_or(0)),
    }
}

/// Saves `bench-<timestamp>.json` and `.md` in `<project>/.model-router/bench/`.
/// Returns the two paths.
pub fn save(project: &Path, results: &Results) -> Result<(PathBuf, PathBuf)> {
    let dir = storage::metadata_dir(project)?.join("bench");
    fs::create_dir_all(&dir)?;
    let stamp = results.started_at.format("%Y%m%d-%H%M%S");
    let mut base = format!("bench-{stamp}");
    let mut suffix = 2;
    while dir.join(format!("{base}.json")).exists() {
        base = format!("bench-{stamp}-{suffix}");
        suffix += 1;
    }
    let json = dir.join(format!("{base}.json"));
    let md = dir.join(format!("{base}.md"));
    storage::write_json(&json, results)?;
    storage::atomic_write(&md, markdown(results).as_bytes())?;
    Ok((json, md))
}
