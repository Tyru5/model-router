//! Tool definitions and implementations shared by MCP and HTTP.

use super::{Server, runs::Info};
use crate::{
    config::{Mode, validate_task},
    engine::{self, Run, Status},
    history::{self, Entry, StatusFilter, excerpt},
    report,
};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

/// Longest a single `wait_run` call blocks.
const MAX_WAIT_SECONDS: u64 = 3600;
const DEFAULT_WAIT_SECONDS: u64 = 60;
const DEFAULT_RUN_LIMIT: usize = 20;
const MAX_RUN_LIMIT: usize = 500;

fn project_prop() -> Value {
    json!({
        "type": "string",
        "description": "Project directory. Default: the server's project. Must lie under an allowed root."
    })
}
fn run_id_prop() -> Value {
    json!({"type": "string", "description": "Run ID, e.g. 20260101-120000-abcd1234."})
}

fn schema(properties: Value, required: &[&str]) -> Value {
    let mut properties = properties;
    properties["project"] = project_prop();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

/// Every tool with its description and JSON input schema.
pub fn list() -> Vec<Value> {
    let tool = |name: &str, description: &str, input: Value| json!({"name": name, "description": description, "inputSchema": input});
    let overrides = json!({
        "type": "array",
        "description": "Per-run stage changes; the saved workflow is not modified.",
        "items": {
            "type": "object",
            "properties": {
                "stage": {"type": "string", "description": "Stage ID."},
                "harness": {"type": "string"},
                "model": {"type": "string", "description": "Harness-native model ID; empty for the harness default."},
                "enabled": {"type": "boolean"}
            },
            "required": ["stage"],
            "additionalProperties": false
        }
    });
    let force = json!({
        "type": "boolean",
        "description": "Run interactive stages unattended for this run. The server never opens a terminal."
    });
    vec![
        tool(
            "list_workflows",
            "List the project's workflows and their stages (harness, model, mode, enabled). The first workflow is the default.",
            schema(json!({}), &[]),
        ),
        tool(
            "preview_run",
            "Show the exact harness commands a workflow would run for a task, without running anything.",
            schema(
                json!({
                    "workflow": {"type": "string", "description": "Workflow name. Default: the first."},
                    "task": {"type": "string"}
                }),
                &["task"],
            ),
        ),
        tool(
            "start_run",
            "Start a workflow run for a task in the background and return its run ID. Runs are unattended: workflows with enabled interactive stages are refused unless force_unattended is true. A checkout another run is using gets its own Git worktree.",
            schema(
                json!({
                    "workflow": {"type": "string", "description": "Workflow name. Default: the first."},
                    "task": {"type": "string", "description": "The task, up to 32 KiB."},
                    "worktree": {"type": "boolean", "description": "Work in a new Git worktree from HEAD; refuses uncommitted changes."},
                    "overrides": overrides,
                    "force_unattended": force
                }),
                &["task"],
            ),
        ),
        tool(
            "run_status",
            "A run's status, current stage, per-stage status, latest handoff summary and verdict, elapsed time, and usage.",
            schema(json!({"run_id": run_id_prop()}), &["run_id"]),
        ),
        tool(
            "wait_run",
            "Block until the run completes, stops, or waits for a decision (confirm_stage / retry_stage), or until timeout_seconds pass. Returns run_status plus timed_out.",
            schema(
                json!({
                    "run_id": run_id_prop(),
                    "timeout_seconds": {"type": "integer", "minimum": 0, "maximum": MAX_WAIT_SECONDS, "description": "Default 60."}
                }),
                &["run_id"],
            ),
        ),
        tool(
            "cancel_run",
            "Cancel a run this server is driving. The harness is stopped; the run is saved as cancelled.",
            schema(json!({"run_id": run_id_prop()}), &["run_id"]),
        ),
        tool(
            "list_runs",
            "List saved runs, newest first.",
            schema(
                json!({
                    "status": {"type": "string", "description": "Comma-separated: active, pending, running, interactive, awaiting_confirmation, complete, failed, blocked, cancelled, damaged."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_RUN_LIMIT, "description": "Default 20."}
                }),
                &[],
            ),
        ),
        tool(
            "get_handoff",
            "A stage's handoff (summary, verdict, findings, tests, files changed) and its attempts.",
            schema(
                json!({
                    "run_id": run_id_prop(),
                    "stage": {"type": "string", "description": "Stage ID, name, or 1-based number."}
                }),
                &["run_id", "stage"],
            ),
        ),
        tool(
            "get_report",
            "A run's report: status, time, usage, checks, unresolved items, review summary, and changed files.",
            schema(json!({"run_id": run_id_prop()}), &["run_id"]),
        ),
        tool(
            "confirm_stage",
            "Accept the handoff of a run waiting at a checkpoint (status awaiting_confirmation) and continue the run.",
            schema(json!({"run_id": run_id_prop()}), &["run_id"]),
        ),
        tool(
            "retry_stage",
            "Retry the current stage of a stopped or waiting run, optionally with another model, and continue the run. Filesystem changes from earlier attempts are kept.",
            schema(
                json!({
                    "run_id": run_id_prop(),
                    "model": {"type": "string", "description": "Model for the retry. Default: the stage's model."},
                    "force_unattended": force
                }),
                &["run_id"],
            ),
        ),
    ]
}

const TOOL_NAMES: &[&str] = &[
    "list_workflows",
    "preview_run",
    "start_run",
    "run_status",
    "wait_run",
    "cancel_run",
    "list_runs",
    "get_handoff",
    "get_report",
    "confirm_stage",
    "retry_stage",
];

pub fn exists(name: &str) -> bool {
    TOOL_NAMES.contains(&name)
}

/// Whether a call to the tool can hold its thread for a long time
/// (`wait_run`, up to `MAX_WAIT_SECONDS`); the others answer at once.
pub fn blocks(name: &str) -> bool {
    name == "wait_run"
}

fn args<T: DeserializeOwned>(value: &Value) -> Result<T> {
    let value = if value.is_null() {
        json!({})
    } else {
        value.clone()
    };
    serde_json::from_value(value).context("Invalid arguments")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectArgs {
    project: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewArgs {
    project: Option<String>,
    workflow: Option<String>,
    task: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Override {
    stage: String,
    harness: Option<String>,
    model: Option<String>,
    enabled: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartArgs {
    project: Option<String>,
    workflow: Option<String>,
    task: String,
    #[serde(default)]
    worktree: bool,
    #[serde(default)]
    overrides: Vec<Override>,
    #[serde(default)]
    force_unattended: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArgs {
    project: Option<String>,
    run_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    project: Option<String>,
    run_id: String,
    timeout_seconds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListRunsArgs {
    project: Option<String>,
    status: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HandoffArgs {
    project: Option<String>,
    run_id: String,
    stage: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryArgs {
    project: Option<String>,
    run_id: String,
    model: Option<String>,
    #[serde(default)]
    force_unattended: bool,
}

/// Runs a tool with optional cancellation and progress reporting (for MCP/HTTP).
/// Errors become `isError` results (MCP) or 400s (HTTP).
pub fn call_with(
    server: &Server,
    name: &str,
    arguments: &Value,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    on_progress: &mut dyn FnMut(&Value),
) -> Result<Value> {
    match name {
        "list_workflows" => list_workflows(server, args(arguments)?),
        "preview_run" => preview_run(server, args(arguments)?),
        "start_run" => start_run(server, args(arguments)?),
        "run_status" => {
            let a: RunArgs = args(arguments)?;
            let (run, info) = load(server, a.project.as_deref(), &a.run_id)?;
            Ok(status_value(&run, &info))
        }
        "wait_run" => wait_run(server, args(arguments)?, cancel, on_progress),
        "cancel_run" => cancel_run(server, args(arguments)?),
        "list_runs" => list_runs(server, args(arguments)?),
        "get_handoff" => get_handoff(server, args(arguments)?),
        "get_report" => get_report(server, args(arguments)?),
        "confirm_stage" => confirm_stage(server, args(arguments)?),
        "retry_stage" => retry_stage(server, args(arguments)?),
        _ => bail!("Unknown tool {name:?}"),
    }
}

/// A saved run and what this server knows about it. A run this server
/// drives must be addressed in its own project.
pub(super) fn load(server: &Server, project: Option<&str>, id: &str) -> Result<(Run, Info)> {
    let project = server.project(project)?;
    let run = load_in_project(server, &project, id)?;
    Ok((run, server.runs.info(id)))
}

fn load_in_project(server: &Server, project: &Path, id: &str) -> Result<Run> {
    if let Some(owner) = server.runs.project_of(id) {
        ensure!(
            owner == project,
            "Run {id} belongs to project {}",
            owner.display()
        );
    }
    engine::load(project, id)
        .with_context(|| format!("No readable run {id} in {}", project.display()))
}

fn list_workflows(server: &Server, a: ProjectArgs) -> Result<Value> {
    let (project, settings) = server.project_and_settings(a.project.as_deref())?;
    let workflows: Vec<Value> = settings
        .workflows
        .iter()
        .enumerate()
        .map(|(index, workflow)| {
            json!({
                "name": workflow.name,
                "default": index == 0,
                "max_repair_passes": workflow.max_repair_passes,
                "stages": workflow.stages.iter().map(|stage| json!({
                    "id": stage.id,
                    "name": stage.name,
                    "role": stage.role,
                    "harness": stage.harness,
                    "model": stage.model,
                    "provider": stage.provider,
                    "effort": stage.effort,
                    "mode": stage.mode,
                    "enabled": stage.enabled,
                    "checks": stage.checks.len(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({"project": project, "workflows": workflows}))
}

fn preview_run(server: &Server, a: PreviewArgs) -> Result<Value> {
    let (project, settings) = server.project_and_settings(a.project.as_deref())?;
    let workflow = settings.workflow(a.workflow.as_deref())?;
    let preview = engine::preview(&settings, workflow, &project, &a.task)?;
    Ok(json!({"project": project, "workflow": workflow.name, "preview": preview}))
}

fn start_run(server: &Server, a: StartArgs) -> Result<Value> {
    let (project, settings) = server.project_and_settings(a.project.as_deref())?;
    let mut workflow = settings.workflow(a.workflow.as_deref())?.clone();
    for change in &a.overrides {
        let stage = workflow
            .stages
            .iter_mut()
            .find(|s| s.id == change.stage)
            .with_context(|| {
                format!(
                    "Workflow {} has no stage {:?}",
                    workflow_name(&settings, a.workflow.as_deref()),
                    change.stage
                )
            })?;
        if let Some(harness) = &change.harness {
            stage.harness = harness.clone();
        }
        if let Some(model) = &change.model {
            stage.model = model.clone();
        }
        if let Some(enabled) = change.enabled {
            stage.enabled = enabled;
        }
    }
    let interactive: Vec<&str> = workflow
        .stages
        .iter()
        .filter(|s| s.enabled && s.mode == Mode::Interactive)
        .map(|s| s.id.as_str())
        .collect();
    if !interactive.is_empty() {
        ensure!(
            a.force_unattended,
            "Workflow {} has enabled interactive stages ({}). The server runs unattended only; pass force_unattended: true to run them unattended, or disable them with overrides",
            workflow.name,
            interactive.join(", ")
        );
        for stage in &mut workflow.stages {
            stage.mode = Mode::Unattended;
        }
    }
    validate_task(&a.task)?;
    let (run, holder) = server.runs.start(&project, a.worktree, |placement| {
        Run::create_at(&project, &settings, &workflow, a.task, placement)
    })?;
    super::log(format!(
        "started run {} in {}",
        run.id,
        run.workspace.display()
    ));
    let mut value = status_value(&run, &server.runs.info(&run.id));
    value["artifacts"] = json!(run.directory);
    value["busy_checkout"] = json!(holder);
    Ok(value)
}

fn workflow_name(settings: &crate::config::Settings, name: Option<&str>) -> String {
    settings
        .workflow(name)
        .map_or_else(|_| String::new(), |w| w.name.clone())
}

/// Statuses where a run needs nothing more from the engine to change.
fn settled(status: Status) -> bool {
    !matches!(status, Status::Pending | Status::Running)
}

fn wait_run(
    server: &Server,
    a: WaitArgs,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    on_progress: &mut dyn FnMut(&Value),
) -> Result<Value> {
    use std::sync::atomic::Ordering;
    let timeout = a
        .timeout_seconds
        .unwrap_or(DEFAULT_WAIT_SECONDS)
        .min(MAX_WAIT_SECONDS);
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout);
    let (mut run, mut info) = load(server, a.project.as_deref(), &a.run_id)?;
    loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            break;
        }
        if info.active {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            server.runs.wait_with(&a.run_id, left, cancel, |snapshot| {
                on_progress(snapshot);
            });
        } else if settled(run.status) {
            break;
        } else {
            // Driven by another process: poll its saved state.
            std::thread::sleep(
                Duration::from_millis(250)
                    .min(deadline.saturating_duration_since(std::time::Instant::now())),
            );
        }
        (run, info) = load(server, a.project.as_deref(), &a.run_id)?;
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed))
            || (!info.active && settled(run.status))
            || std::time::Instant::now() >= deadline
        {
            break;
        }
    }
    let timed_out = info.active || !settled(run.status);
    let mut value = status_value(&run, &info);
    value["timed_out"] = json!(timed_out);
    Ok(value)
}

fn cancel_run(server: &Server, a: RunArgs) -> Result<Value> {
    let (run, _) = load(server, a.project.as_deref(), &a.run_id)?;
    ensure!(
        server.runs.cancel(&run.id),
        "Run {} is not running in this server (status {})",
        run.id,
        run.status.label()
    );
    let mut value = status_value(&run, &server.runs.info(&run.id));
    value["cancelling"] = json!(true);
    Ok(value)
}

fn list_runs(server: &Server, a: ListRunsArgs) -> Result<Value> {
    let project = server.project(a.project.as_deref())?;
    let statuses = a
        .status
        .as_deref()
        .unwrap_or("")
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(str::parse)
        .collect::<Result<Vec<StatusFilter>>>()?;
    let entries = history::list(&project)?;
    let mut shown = history::filter(&entries, &statuses, "");
    let limit = a.limit.unwrap_or(DEFAULT_RUN_LIMIT).clamp(1, MAX_RUN_LIMIT);
    let total = shown.len();
    shown.truncate(limit);
    let runs = shown
        .iter()
        .map(|entry| {
            Ok(match entry {
                Entry::Summary(summary) => {
                    let mut value = serde_json::to_value(summary)?;
                    value["active_here"] = json!(server.runs.is_active(&summary.id));
                    value
                }
                Entry::Damaged { id, path, error } => {
                    json!({"id": id, "status": "damaged", "path": path, "error": error})
                }
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({"project": project, "total": total, "runs": runs}))
}

fn stage_index(run: &Run, stage: &str) -> Result<usize> {
    let stages = &run.workflow.stages;
    stages
        .iter()
        .position(|s| s.id == stage)
        .or_else(|| stages.iter().position(|s| s.name == stage))
        .or_else(|| {
            stage
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=stages.len()).contains(n))
                .map(|n| n - 1)
        })
        .with_context(|| {
            format!(
                "Run {} has no stage {stage:?}; stages: {}",
                run.id,
                stages
                    .iter()
                    .map(|s| s.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

fn get_handoff(server: &Server, a: HandoffArgs) -> Result<Value> {
    let (run, _) = load(server, a.project.as_deref(), &a.run_id)?;
    let index = stage_index(&run, &a.stage)?;
    let stage = &run.workflow.stages[index];
    let state = &run.stages[index];
    Ok(json!({
        "run_id": run.id,
        "stage": {
            "index": index,
            "id": stage.id,
            "name": stage.name,
            "role": stage.role,
            "status": state.status,
        },
        "handoff": state.handoff,
        "waiver": state.waiver,
        "attempts": state.attempts,
        // Delegated tasks' handoffs, from the latest attempt.
        "tasks": state.attempts.last().map(|a| a.tasks.iter().map(|t| json!({
            "task_id": t.id,
            "target": t.target,
            "status": t.status.label(),
            "handoff": t.handoff,
            "latest_handoff": t.latest_handoff,
            "questions": t.questions,
        })).collect::<Vec<_>>()).unwrap_or_default(),
    }))
}

/// A stage's delegated tasks by status, for run_status; null without any.
fn task_counts(state: &engine::StageState) -> Value {
    let Some(attempt) = state.attempts.last().filter(|a| !a.tasks.is_empty()) else {
        return Value::Null;
    };
    let mut counts = serde_json::Map::new();
    for task in &attempt.tasks {
        let entry = counts.entry(task.status.label()).or_insert(json!(0));
        *entry = json!(entry.as_u64().unwrap_or(0) + 1);
    }
    counts.insert("wake_turns".into(), json!(attempt.turns.len()));
    Value::Object(counts)
}

fn get_report(server: &Server, a: RunArgs) -> Result<Value> {
    let (run, info) = load(server, a.project.as_deref(), &a.run_id)?;
    let built = report::build(&run);
    let summary = report::summary(&built);
    let mut value = serde_json::to_value(&built)?;
    value["changes"] = serde_json::to_value(report::changes(&run))?;
    value["receipts"] = serde_json::to_value(report::receipts(&run))?;
    value["summary"] = json!(summary);
    value["artifacts"] = json!(run.directory);
    value["workspace"] = json!(run.workspace);
    value["active"] = json!(info.active);
    Ok(value)
}

/// Reloads after the engine's last save and atomically reserves and resumes a
/// run this server is not already driving.
/// `serve` as the run host for runs agents launch and schedules fire.
pub struct ServeHost(pub std::sync::Arc<Server>);
impl engine::RunHost for ServeHost {
    fn drive(&self, project: &Path, id: &str) -> Result<()> {
        let run = engine::load(project, id)?;
        self.0.runs.drive(project, run)
    }
    fn retry(&self, project: &Path, id: &str) -> Result<()> {
        let project = project.display().to_string();
        resume_idle_run(&self.0, Some(&project), id, |run| {
            ensure!(
                run.status != Status::Complete && run.cursor < run.stages.len(),
                "Run {} is complete",
                run.id
            );
            run.retry(false, None)
        })
        .map(|_| ())
    }
    fn cancel(&self, id: &str) -> bool {
        self.0.runs.cancel(id)
    }
}

fn resume_idle_run(
    server: &Server,
    project: Option<&str>,
    id: &str,
    prepare: impl FnOnce(&mut Run) -> Result<()>,
) -> Result<Run> {
    let (project, settings) = server.project_and_settings(project)?;
    server.runs.resume_run(&project, id, || {
        let mut run = load_in_project(server, &project, id)?;
        // What recovery stopped or left running goes to the server's log.
        let run_id = run.id.clone();
        engine::authorize_replay(&mut run, settings.explicit_config.as_deref(), |event| {
            if let engine::Event::Output(text) = event {
                super::log(format!("run {run_id}: {}", text.trim_end()));
            }
        })?;
        prepare(&mut run)?;
        Ok(run)
    })
}

fn confirm_stage(server: &Server, a: RunArgs) -> Result<Value> {
    let run = resume_idle_run(server, a.project.as_deref(), &a.run_id, |run| {
        // The ship checkpoint is a person's decision to commit and publish.
        ensure!(
            run.workflow
                .stages
                .get(run.cursor)
                .is_none_or(|s| s.role != crate::config::Role::Ship),
            "Run {} waits for a person to confirm its ship plan: model-router resume {} --advance, or [a] in the TUI",
            run.id,
            run.id
        );
        run.confirm()
    })?;
    Ok(status_value(&run, &server.runs.info(&run.id)))
}

fn retry_stage(server: &Server, a: RetryArgs) -> Result<Value> {
    let force_unattended = a.force_unattended;
    let model = a.model.filter(|m| !m.trim().is_empty());
    let run = resume_idle_run(server, a.project.as_deref(), &a.run_id, move |run| {
        ensure!(
            run.status != Status::Complete && run.cursor < run.stages.len(),
            "Run {} is complete",
            run.id
        );
        if run.workflow.stages[run.cursor].mode == Mode::Interactive {
            ensure!(
                force_unattended,
                "Stage {} is interactive. The server runs unattended only; pass force_unattended: true to retry it unattended",
                run.current_name()
            );
            run.workflow.stages[run.cursor].mode = Mode::Unattended;
        }
        run.retry(false, model)
    })?;
    Ok(status_value(&run, &server.runs.info(&run.id)))
}

/// What a client needs to follow a run, in `run_status`, `wait_run`, and
/// streamed state events.
pub fn status_value(run: &Run, info: &Info) -> Value {
    let pricing = crate::cost::Pricing::load(&run.project);
    status_value_with(run, info, &pricing)
}

/// Like `status_value`, reusing an already-loaded `Pricing` table.
pub fn status_value_with(run: &Run, info: &Info, pricing: &crate::cost::Pricing) -> Value {
    let usage = report::usage_total_with(run, pricing);
    let end = if info.active || !settled(run.status) {
        Utc::now()
    } else {
        run.updated_at
    };
    let stages: Vec<Value> = run
        .workflow
        .stages
        .iter()
        .zip(&run.stages)
        .enumerate()
        .map(|(index, (stage, state))| {
            json!({
                "index": index,
                "id": stage.id,
                "name": stage.name,
                "role": stage.role,
                "harness": stage.harness,
                "model": stage.model,
                "mode": stage.mode,
                "enabled": stage.enabled,
                "status": state.status,
                "attempts": state.attempts.len(),
                "handoff": state.handoff.as_ref().map(|h| json!({
                    "summary": excerpt(&h.summary),
                    "verdict": h.verdict,
                })),
                "tasks": task_counts(state),
            })
        })
        .collect();
    let latest = run
        .workflow
        .stages
        .iter()
        .zip(&run.stages)
        .filter_map(|(stage, state)| {
            let handoff = state.handoff.as_ref()?;
            let at = state.attempts.last().map(|a| a.started_at);
            Some((at, stage, handoff))
        })
        .max_by_key(|(at, _, _)| *at)
        .map(|(_, stage, handoff)| {
            json!({
                "stage": stage.id,
                "summary": handoff.summary,
                "verdict": handoff.verdict,
            })
        });
    let waiting = match run.status {
        Status::AwaitingConfirmation if !info.active => Some(match run.pending_waiver() {
            Some(_) => {
                "confirm_stage accepts the not-applicable verification; retry_stage reruns the stage"
            }
            None => "confirm_stage accepts the handoff; retry_stage reruns the stage",
        }),
        Status::Interactive if !info.active => Some(
            "the stage is interactive; retry_stage with force_unattended runs it unattended, or resume it in a terminal",
        ),
        Status::Failed | Status::Blocked | Status::Cancelled if !info.active => {
            Some("retry_stage reruns the current stage")
        }
        _ => None,
    };
    json!({
        "run_id": run.id,
        "project": run.project,
        "workspace": run.workspace,
        "worktree": run.worktree,
        "workflow": run.workflow.name,
        "task": excerpt(&run.task),
        "status": run.status,
        "active": info.active,
        "cursor": run.cursor,
        "current_stage": run.current_name(),
        "error": run.error,
        "server_error": info.error,
        "last_drive": info.outcome,
        "pending_waiver": run.pending_waiver(),
        "next": waiting,
        "created_at": run.created_at,
        "updated_at": run.updated_at,
        "elapsed_seconds": (end - run.created_at).num_seconds().max(0),
        "repair_passes": run.repair_passes,
        "stages": stages,
        "latest_handoff": latest,
        "usage": usage,
        "usage_text": report::usage_text(&usage),
    })
}
