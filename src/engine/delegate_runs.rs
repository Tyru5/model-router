//! Run tools (`delegation.runs`): a delegating stage's agent launches,
//! reads, waits on, retries, and cancels top-level runs. Launched runs get
//! their own worktree, stay within the launcher's ceiling, count toward its
//! budget, and may not launch runs or schedules themselves.
use super::*;
use crate::delegation::{
    config::Delegation,
    policy::{self, Ceiling},
    records::LaunchedRun,
};
use crate::serve::mcp::ToolError;
use serde_json::{Value, json};

/// The launcher's side of a run tool call.
pub(super) struct Launcher<'a> {
    pub index: usize,
    pub stage: &'a Stage,
    pub ceiling: &'a Ceiling,
    pub delegation: &'a Delegation,
}

fn error(code: &'static str, message: impl Into<String>) -> ToolError {
    ToolError::new(code, message)
}

fn internal(error: anyhow::Error) -> ToolError {
    ToolError::new("orchestration_error", format!("{error:#}"))
}

fn arg_str<'v>(arguments: &'v Value, name: &str) -> Result<&'v str, ToolError> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| error("invalid_request", format!("{name} is required")))
}

pub(super) fn run_tool(
    run: &mut Run,
    launcher: &Launcher<'_>,
    tool: &str,
    arguments: &Value,
) -> std::result::Result<Value, ToolError> {
    let allowed =
        launcher.delegation.runs && run.launched_by.is_none() && run.creation_source.is_user();
    if !allowed && tool.starts_with("run_") {
        return Err(error(
            "capability_denied",
            "Run tools are not enabled for this stage (delegation.runs), or this run was itself launched",
        ));
    }
    if tool.contains("schedule") {
        let allowed = launcher.delegation.schedules
            && run.launched_by.is_none()
            && run.creation_source.is_user();
        if !allowed {
            return Err(error(
                "capability_denied",
                "Schedule tools are not enabled for this stage (delegation.schedules), or this run was itself launched or scheduled",
            ));
        }
    }
    match tool {
        "schedule_task" => schedule_task(run, launcher, arguments),
        "list_scheduled_tasks" => list_schedules(run),
        "delete_scheduled_task" => {
            let id = arg_str(arguments, "schedule_id")?;
            crate::delegation::schedules::delete(&run.project, id)
                .map_err(|e| error("schedule_not_found", format!("{e:#}")))?;
            Ok(json!({"deleted": id}))
        }
        "run_launch" => launch(run, launcher, arguments),
        "run_list" => list(run, arguments),
        "run_read" => read(run, arguments),
        "run_status" => status(run, arguments),
        "run_retry" => retry(run, launcher, arguments),
        "run_cancel" => cancel(run, launcher, arguments),
        other => Err(error(
            "capability_denied",
            format!("{other} is not enabled for this stage"),
        )),
    }
}

fn launch(
    run: &mut Run,
    launcher: &Launcher<'_>,
    arguments: &Value,
) -> std::result::Result<Value, ToolError> {
    let requests = arguments
        .get("runs")
        .and_then(Value::as_array)
        .filter(|r| (1..=20).contains(&r.len()))
        .ok_or_else(|| error("invalid_request", "runs must list 1 to 20 runs"))?
        .clone();
    let key = arguments
        .get("client_request_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let fingerprint = crate::trust::digest(&[serde_json::to_string(&requests).unwrap_or_default()]);
    let attempt = run.stages[launcher.index]
        .attempts
        .last()
        .ok_or_else(|| error("parent_not_active", "No attempt"))?;
    if let Some(key) = &key {
        let earlier: Vec<&LaunchedRun> = attempt
            .launched_runs
            .iter()
            .filter(|l| &l.client_request_id == key)
            .collect();
        if !earlier.is_empty() {
            if earlier.iter().any(|l| l.fingerprint != fingerprint) {
                return Err(error(
                    "request_conflict",
                    format!("client_request_id {key} was used for a different request"),
                ));
            }
            let runs: Vec<Value> = earlier
                .iter()
                .map(|l| launched_value(&run.project, &l.id))
                .collect();
            return Ok(json!({ "runs": runs }));
        }
    }
    let used = attempt.launched_runs.len();
    let limit = launcher.delegation.max_runs as usize;
    if used + requests.len() > limit {
        return Err(error(
            "run_limit_exceeded",
            format!("This stage may launch at most {limit} runs ({used} launched)"),
        ));
    }
    if let crate::cost::BudgetCheck::Stop(reason) = crate::cost::check_budget_action(run) {
        return Err(error("budget_exceeded", reason));
    }
    let loaded = crate::config::load_config(&run.project, None).map_err(internal)?;
    if !matches!(loaded.trust, crate::trust::Trust::Trusted) {
        return Err(error(
            "workflow_not_launchable",
            "The project config is untrusted; run `model-router trust` first",
        ));
    }
    let backend = crate::sandbox::detect().is_ok();
    // Check every request before creating any run.
    let mut plans = vec![];
    for request in &requests {
        let task = arg_str(request, "task")?.to_owned();
        crate::config::validate_task(&task)
            .map_err(|e| error("invalid_request", format!("{e:#}")))?;
        let mut workflow = loaded
            .settings
            .workflow(request.get("workflow").and_then(Value::as_str))
            .map_err(|e| error("workflow_not_launchable", format!("{e:#}")))?
            .clone();
        let interactive: Vec<&str> = workflow
            .stages
            .iter()
            .filter(|s| s.enabled && s.mode == Mode::Interactive)
            .map(|s| s.id.as_str())
            .collect();
        if !interactive.is_empty() {
            return Err(error(
                "workflow_not_launchable",
                format!(
                    "Workflow {} has interactive stages ({}); agents launch unattended workflows only",
                    workflow.name,
                    interactive.join(", ")
                ),
            ));
        }
        for (what, ceiling) in policy::workflow_ceilings(&workflow, backend) {
            if let Some(why) = policy::exceeds(&ceiling, launcher.ceiling) {
                return Err(error(
                    "permission_escalation_denied",
                    format!(
                        "Workflow {}: {what} exceeds this stage: {why}",
                        workflow.name
                    ),
                ));
            }
        }
        // No recursion: a launched run launches no runs or schedules.
        for stage in &mut workflow.stages {
            if let Some(delegation) = stage.delegation.as_mut() {
                delegation.runs = false;
                delegation.schedules = false;
            }
        }
        plans.push((workflow, task));
    }
    let attempt_number = attempt.number;
    let mut launched = vec![];
    for (offset, (workflow, task)) in plans.into_iter().enumerate() {
        let mut created = Run::create_at(
            &run.project,
            &loaded.settings,
            &workflow,
            task,
            Placement::WorktreeFromHead,
        )
        .map_err(|e| error("worktree_unavailable", format!("{e:#}")))?;
        created.launched_by = Some(LaunchedBy {
            run: run.id.clone(),
            stage: launcher.stage.id.clone(),
            attempt: attempt_number,
        });
        created.creation_source = CreationSource::Agent;
        created.persist().map_err(internal)?;
        let id = created.id.clone();
        if let Some(attempt) = run.stages[launcher.index].attempts.last_mut() {
            attempt.launched_runs.push(LaunchedRun {
                id: id.clone(),
                client_request_id: key
                    .clone()
                    .unwrap_or_else(|| format!("generated-{id}-{offset}")),
                fingerprint: fingerprint.clone(),
            });
        }
        run.persist().map_err(internal)?;
        if let Some(host) = run_host() {
            host.drive(&run.project, &id).map_err(internal)?;
        }
        launched.push(launched_value(&run.project, &id));
    }
    Ok(json!({ "runs": launched }))
}

fn launched_value(project: &Path, id: &str) -> Value {
    let driven = run_host().is_some();
    json!({
        "run_id": id,
        "driven": driven,
        "resume": (!driven).then(|| format!("model-router -C {} resume {id}", shell_words::quote(&project.display().to_string()))),
    })
}

fn list(run: &Run, arguments: &Value) -> std::result::Result<Value, ToolError> {
    let status = arguments.get("status").and_then(Value::as_str);
    let offset = arguments.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let limit = arguments
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .clamp(1, 100) as usize;
    let entries = crate::history::list(&run.project).map_err(internal)?;
    let runs: Vec<Value> = entries
        .iter()
        .filter(|e| status.is_none_or(|s| e.status_label() == s))
        .skip(offset)
        .take(limit)
        .filter_map(|e| load(&run.project, e.id()).ok())
        .map(|r| {
            json!({
                "run_id": r.id,
                "status": r.status.label(),
                "workflow": r.workflow.name,
                "task": storage::truncate(&r.task, 200),
                "created_at": r.created_at,
                "creation_source": r.creation_source.label(),
                "launched_by": r.launched_by,
            })
        })
        .collect();
    Ok(json!({ "runs": runs, "offset": offset, "has_more": entries.len() > offset + runs.len() }))
}

/// Budget for one `run_read`, in characters.
const READ_LIMIT: usize = 30_000;

fn read(run: &Run, arguments: &Value) -> std::result::Result<Value, ToolError> {
    let id = arg_str(arguments, "run_id")?;
    let target = load(&run.project, id).map_err(|e| error("run_not_found", format!("{e:#}")))?;
    let stages: Vec<Value> = target
        .workflow
        .stages
        .iter()
        .zip(&target.stages)
        .map(|(stage, state)| {
            json!({
                "stage": stage.id,
                "name": stage.name,
                "status": state.status.label(),
                "summary": state.handoff.as_ref().map(|h| storage::truncate(&h.summary, 2000)),
            })
        })
        .collect();
    let index = match arguments.get("stage").and_then(Value::as_str) {
        Some(stage) => target
            .workflow
            .stages
            .iter()
            .position(|s| s.id == stage)
            .ok_or_else(|| error("invalid_request", format!("Run {id} has no stage {stage}")))?,
        None => target.cursor.min(target.stages.len().saturating_sub(1)),
    };
    let cursor = arguments.get("cursor").and_then(Value::as_u64).unwrap_or(0);
    let (output, next) = match target.stages[index].attempts.last() {
        Some(attempt) => read_log(&target.directory, &attempt.directory, cursor)?,
        None => (String::new(), None),
    };
    Ok(json!({
        "run_id": target.id,
        "status": target.status.label(),
        "error": target.error,
        "stages": stages,
        "output_stage": target.workflow.stages[index].id,
        "output": output,
        "next_cursor": next,
    }))
}

/// Up to `READ_LIMIT` characters of an attempt's `stdout.log` from byte
/// `cursor` (moved to the next UTF-8 boundary), opened without following
/// links beneath the run's directory.
fn read_log(
    run_dir: &Path,
    attempt_dir: &Path,
    cursor: u64,
) -> std::result::Result<(String, Option<u64>), ToolError> {
    use std::io::{Read, Seek, SeekFrom};
    let relative = attempt_dir
        .strip_prefix(run_dir)
        .map_err(|_| error("orchestration_error", "The attempt is outside its run"))?
        .join("stdout.log");
    let mut file = match storage::open_beneath(run_dir, &relative) {
        Ok(file) => file,
        Err(_) => return Ok((String::new(), None)),
    };
    let length = file.metadata().map(|m| m.len()).unwrap_or(0);
    file.seek(SeekFrom::Start(cursor.min(length)))
        .map_err(|e| internal(e.into()))?;
    let mut bytes = Vec::new();
    file.take((READ_LIMIT * 4) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| internal(e.into()))?;
    // Skip continuation bytes a cursor inside a character points at.
    let start = bytes.iter().take_while(|b| (**b & 0xC0) == 0x80).count();
    let text = String::from_utf8_lossy(&bytes[start..]);
    let text: String = text.chars().take(READ_LIMIT).collect();
    let consumed = start as u64 + text.len() as u64;
    let next = (cursor + consumed < length).then_some(cursor + consumed);
    Ok((storage::clean_terminal(&text), next))
}

fn status(run: &Run, arguments: &Value) -> std::result::Result<Value, ToolError> {
    let id = arg_str(arguments, "run_id")?;
    let target = load(&run.project, id).map_err(|e| error("run_not_found", format!("{e:#}")))?;
    Ok(json!({
        "run_id": target.id,
        "status": target.status.label(),
        "error": target.error,
        "driven": run_host().is_some() || target.status != Status::Pending,
    }))
}

/// A run this stage launched, or why it is not one.
fn owned(run: &Run, launcher: &Launcher<'_>, id: &str) -> std::result::Result<Run, ToolError> {
    let target = load(&run.project, id).map_err(|e| error("run_not_found", format!("{e:#}")))?;
    let mine = target
        .launched_by
        .as_ref()
        .is_some_and(|l| l.run == run.id && l.stage == launcher.stage.id);
    if !mine {
        return Err(error(
            "run_not_owned",
            format!("Run {id} was not launched by this stage"),
        ));
    }
    Ok(target)
}

fn retry(
    run: &Run,
    launcher: &Launcher<'_>,
    arguments: &Value,
) -> std::result::Result<Value, ToolError> {
    let id = arg_str(arguments, "run_id")?;
    let target = owned(run, launcher, id)?;
    let backend = crate::sandbox::detect().is_ok();
    for (what, ceiling) in policy::workflow_ceilings(&target.workflow, backend) {
        if let Some(why) = policy::exceeds(&ceiling, launcher.ceiling) {
            return Err(error(
                "permission_escalation_denied",
                format!("{what} exceeds this stage: {why}"),
            ));
        }
    }
    let Some(host) = run_host() else {
        return Err(error(
            "capability_denied",
            format!("Nothing drives runs here; the user resumes run {id}"),
        ));
    };
    host.retry(&run.project, id).map_err(internal)?;
    status(run, &json!({"run_id": id}))
}

fn cancel(
    run: &Run,
    launcher: &Launcher<'_>,
    arguments: &Value,
) -> std::result::Result<Value, ToolError> {
    let id = arg_str(arguments, "run_id")?;
    let target = owned(run, launcher, id)?;
    let cancelled = run_host().is_some_and(|host| host.cancel(id));
    if !cancelled && target.status == Status::Pending {
        cancel_pending(&run.project, id, "Cancelled by the agent that launched it")
            .map_err(internal)?;
    }
    status(run, &json!({"run_id": id}))
}

fn schedule_view(project: &Path, schedule: &crate::delegation::schedules::Schedule) -> Value {
    use crate::delegation::schedules;
    let approved = schedules::trusted_settings(project)
        .is_ok_and(|settings| schedules::approved(project, schedule, &settings));
    json!({
        "schedule_id": schedule.id,
        "workflow": schedule.workflow,
        "task": storage::truncate(&schedule.task, 500),
        "trigger": schedule.trigger.describe(),
        "enabled": schedule.enabled,
        "approved": approved,
        "approve": (!approved && schedule.enabled).then(|| format!(
            "The user approves it with: model-router -C {} schedules approve {}",
            shell_words::quote(&project.display().to_string()),
            schedule.id
        )),
        "next_due": schedules::next_due(schedule, Utc::now()),
        "paused_reason": schedule.paused_reason,
        "last": schedule.occurrences.last(),
        "executors": "mr serve runs a scheduler; mr schedules tick fires due schedules for cron or systemd timers",
    })
}

fn list_schedules(run: &Run) -> std::result::Result<Value, ToolError> {
    let store = crate::delegation::schedules::load(&run.project).map_err(internal)?;
    let views: Vec<Value> = store
        .schedules
        .iter()
        .map(|s| schedule_view(&run.project, s))
        .collect();
    Ok(json!({ "schedules": views }))
}

fn schedule_task(
    run: &Run,
    launcher: &Launcher<'_>,
    arguments: &Value,
) -> std::result::Result<Value, ToolError> {
    use crate::delegation::schedules::{self, Schedule};
    let text = |name: &str| {
        arguments
            .get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
    };
    let trigger_given = text("every").is_some()
        || text("at").is_some()
        || arguments.get("webhook").and_then(Value::as_bool) == Some(true);
    let trigger = trigger_given
        .then(|| {
            schedules::parse_trigger(
                text("every"),
                text("at"),
                arguments.get("webhook").and_then(Value::as_bool) == Some(true),
            )
        })
        .transpose()
        .map_err(|e| error("invalid_request", format!("{e:#}")))?;
    let settings = schedules::trusted_settings(&run.project)
        .map_err(|e| error("workflow_not_launchable", format!("{e:#}")))?;
    let workflow = text("workflow").map(str::to_owned);
    if let Some(task) = text("task") {
        crate::config::validate_task(task)
            .map_err(|e| error("invalid_request", format!("{e:#}")))?;
    }
    let key = text("client_request_id").map(str::to_owned);
    let created_by = format!("{}/{}", run.id, launcher.stage.id);
    let ceiling = launcher.ceiling.clone();
    let id = text("schedule_id").map(str::to_owned);
    let schedule = schedules::update(&run.project, |store| {
        let now = Utc::now();
        // An update: the schedule this request names, or the one its key made.
        let existing = store.schedules.iter_mut().find(|s| {
            id.as_deref() == Some(s.id.as_str())
                || (id.is_none() && key.as_deref().is_some_and(|k| k == s.client_request_id))
        });
        let schedule = match existing {
            Some(schedule) => {
                if let Some(workflow) = &workflow {
                    schedule.workflow = Some(workflow.clone());
                }
                if let Some(task) = text("task") {
                    schedule.task = task.to_owned();
                }
                if let Some(trigger) = &trigger {
                    schedule.trigger = trigger.clone();
                }
                if let Some(enabled) = arguments.get("enabled").and_then(Value::as_bool) {
                    schedule.enabled = enabled;
                }
                schedule.ceiling = ceiling.clone();
                schedule.revision += 1;
                schedule.updated_at = now;
                schedule.approved_at = None;
                schedule.clone()
            }
            None => {
                if id.is_some() {
                    anyhow::bail!("No schedule {}", id.as_deref().unwrap_or_default());
                }
                let task = text("task").context("task is required for a new schedule")?;
                let trigger = trigger
                    .clone()
                    .context("Give one of every, at, or webhook")?;
                let number = store
                    .schedules
                    .iter()
                    .filter_map(|s| s.id.strip_prefix('s')?.parse::<usize>().ok())
                    .max()
                    .unwrap_or(0)
                    + 1;
                let schedule = Schedule {
                    id: format!("s{number}"),
                    workflow: workflow.clone(),
                    task: task.to_owned(),
                    trigger,
                    enabled: arguments
                        .get("enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    created_by: created_by.clone(),
                    ceiling: ceiling.clone(),
                    client_request_id: key.clone().unwrap_or_default(),
                    revision: 1,
                    created_at: now,
                    updated_at: now,
                    approved_at: None,
                    paused_reason: None,
                    occurrences: vec![],
                };
                store.schedules.push(schedule.clone());
                schedule
            }
        };
        schedules::check_launchable(&settings, schedule.workflow.as_deref(), &schedule.ceiling)?;
        Ok(schedule)
    })
    .map_err(|e| error("invalid_request", format!("{e:#}")))?;
    // Any edit needs approval again.
    schedules::revoke(&run.project, &schedule.id);
    Ok(schedule_view(&run.project, &schedule))
}
