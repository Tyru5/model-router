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
use std::collections::BTreeMap;

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
    // A retry finishes what its first call left: runs it created are
    // returned as they are, and only the missing ones are created.
    let mut done: BTreeMap<usize, String> = BTreeMap::new();
    if let Some(key) = &key {
        let earlier: Vec<&LaunchedRun> = attempt
            .launched_runs
            .iter()
            .filter(|l| &l.client_request_id == key)
            .collect();
        if earlier.iter().any(|l| l.fingerprint != fingerprint) {
            return Err(error(
                "request_conflict",
                format!("client_request_id {key} was used for a different request"),
            ));
        }
        done.extend(earlier.iter().map(|l| (l.offset, l.id.clone())));
        if done.len() >= requests.len() {
            let runs: Vec<Value> = done
                .values()
                .map(|id| launched_value(&run.project, id))
                .collect();
            return Ok(json!({ "runs": runs }));
        }
    }
    let used = attempt.launched_runs.len();
    let missing = requests.len() - done.len();
    let limit = launcher.delegation.max_runs as usize;
    if used + missing > limit {
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
        if let Some(id) = done.get(&offset) {
            launched.push(launched_value(&run.project, id));
            continue;
        }
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
                offset,
            });
        }
        run.persist().map_err(internal)?;
        let mut value = launched_value(&run.project, &id);
        // The run exists either way; a host that cannot drive it leaves it
        // pending for `model-router resume`, and says so for this run.
        if let Some(host) = run_host() {
            if let Err(e) = host.drive(&run.project, &id) {
                value["driven"] = json!(false);
                value["resume"] = json!(resume_hint(&run.project, &id));
                value["error"] = json!(format!("{e:#}"));
            }
        }
        launched.push(value);
    }
    Ok(json!({ "runs": launched }))
}

fn launched_value(project: &Path, id: &str) -> Value {
    let driven = run_host().is_some();
    json!({
        "run_id": id,
        "driven": driven,
        "resume": (!driven).then(|| resume_hint(project, id)),
    })
}

fn resume_hint(project: &Path, id: &str) -> String {
    format!(
        "model-router -C {} resume {id}",
        shell_words::quote(&project.display().to_string())
    )
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
    let entries: Vec<_> = entries
        .iter()
        .filter(|e| status.is_none_or(|s| e.status_label() == s))
        .collect();
    let runs: Vec<Value> = entries
        .iter()
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
    if target.stages.is_empty() {
        return Err(error("invalid_request", format!("Run {id} has no stages")));
    }
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
    let base = cursor.min(length);
    // The up to 3 bytes before the cursor tell a cursor inside a character
    // from stray continuation bytes, which show as U+FFFD.
    let back = base.min(3);
    let mut before = vec![0; back as usize];
    file.seek(SeekFrom::Start(base - back))
        .and_then(|_| file.read_exact(&mut before))
        .map_err(|e| internal(e.into()))?;
    let mut bytes = Vec::new();
    file.take((READ_LIMIT * 4) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| internal(e.into()))?;
    // Skip the rest of a character the cursor points inside.
    let tail = bytes.iter().take_while(|b| (**b & 0xC0) == 0x80).count();
    let window = [before.as_slice(), &bytes[..tail]].concat();
    let (mut offset, mut start) = (0, 0);
    for chunk in window.utf8_chunks() {
        for c in chunk.valid().chars() {
            let end = offset + c.len_utf8();
            if offset < before.len() && end > before.len() {
                start = end - before.len();
            }
            offset = end;
        }
        offset += chunk.invalid().len();
    }
    // Count the bytes each character came from, not its decoded length: an
    // invalid byte decodes to a 3-byte U+FFFD.
    let mut text = String::new();
    let mut consumed = start;
    let mut chars = 0;
    'chunks: for chunk in bytes[start..].utf8_chunks() {
        for c in chunk.valid().chars() {
            if chars == READ_LIMIT {
                break 'chunks;
            }
            text.push(c);
            consumed += c.len_utf8();
            chars += 1;
        }
        let invalid = chunk.invalid();
        if invalid.is_empty() {
            continue;
        }
        // A character the read cut off (possible once a cursor inside a
        // character spent bytes) is left for the next page.
        let at_end = consumed + invalid.len() == bytes.len();
        if chars == READ_LIMIT || (at_end && base + (bytes.len() as u64) < length) {
            break;
        }
        text.push(char::REPLACEMENT_CHARACTER);
        consumed += invalid.len();
        chars += 1;
    }
    let consumed = consumed as u64;
    let next = (base + consumed < length).then_some(base + consumed);
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
    let settings = schedules::trusted_settings(project).ok();
    let approved = settings
        .as_ref()
        .is_some_and(|settings| schedules::approved(project, schedule, settings));
    let digest = settings
        .as_ref()
        .and_then(|settings| schedules::digest(project, schedule, settings).ok());
    json!({
        "schedule_id": schedule.id,
        "workflow": schedule.workflow,
        "task": storage::truncate(&schedule.task, 500),
        "trigger": schedule.trigger.describe(),
        "enabled": schedule.enabled,
        "approved": approved,
        "approve": (!approved && schedule.enabled).then(|| {
            // Pin the definition reported here: a later edit makes this
            // command fail instead of approving what the user never saw.
            let pin = digest
                .as_deref()
                .map(|d| format!(" --digest {d}"))
                .unwrap_or_default();
            format!(
                "The user approves it with: model-router -C {} schedules approve {}{pin}",
                shell_words::quote(&project.display().to_string()),
                schedule.id
            )
        }),
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
        || text("cron").is_some()
        || arguments.get("webhook").and_then(Value::as_bool) == Some(true);
    let trigger = trigger_given
        .then(|| {
            schedules::parse_trigger(
                text("every"),
                text("at"),
                text("cron"),
                arguments.get("webhook").and_then(Value::as_bool) == Some(true),
                text("tz"),
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
    // Keys scope to (run, stage, attempt), like every tool's.
    let attempt = run.stages[launcher.index].attempts.len();
    let requester = format!("{created_by}/{attempt}");
    let fingerprint = crate::trust::digest(&[serde_json::to_string(&json!({
        "schedule_id": id,
        "workflow": workflow,
        "task": text("task"),
        "every": text("every"),
        "at": text("at"),
        "cron": text("cron"),
        "tz": text("tz"),
        "webhook": arguments.get("webhook"),
        "enabled": arguments.get("enabled"),
    }))
    .unwrap_or_default()]);
    // A key is this stage's: a retry returns what the first call made.
    if let Some(key) = &key {
        let store = schedules::load(&run.project)
            .map_err(|e| error("orchestration_error", format!("{e:#}")))?;
        if let Some(earlier) = store
            .schedules
            .iter()
            .find(|s| &s.client_request_id == key && s.requested_by == requester)
        {
            if earlier.request_fingerprint != fingerprint {
                return Err(error(
                    "request_conflict",
                    format!("client_request_id {key} was used for a different request"),
                ));
            }
            return Ok(schedule_view(&run.project, earlier));
        }
    }
    let schedule = schedules::update_after(
        &run.project,
        |store| {
            let now = Utc::now();
            let existing = store
                .schedules
                .iter_mut()
                .find(|s| id.as_deref() == Some(s.id.as_str()));
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
                    } else if let Some(tz) = text("tz") {
                        // A tz alone re-times the stored at/cron trigger.
                        schedule.trigger = schedule.trigger.with_tz(Some(tz))?;
                    }
                    if let Some(enabled) = arguments.get("enabled").and_then(Value::as_bool) {
                        schedule.enabled = enabled;
                    }
                    schedule.ceiling = ceiling.clone();
                    schedule.revision += 1;
                    schedule.updated_at = now;
                    schedule.approved_at = None;
                    if let Some(key) = &key {
                        schedule.client_request_id = key.clone();
                        schedule.requested_by = requester.clone();
                        schedule.request_fingerprint = fingerprint.clone();
                    }
                    schedule.clone()
                }
                None => {
                    if id.is_some() {
                        anyhow::bail!("No schedule {}", id.as_deref().unwrap_or_default());
                    }
                    let task = text("task").context("task is required for a new schedule")?;
                    let trigger = trigger
                        .clone()
                        .context("Give one of every, at, cron, or webhook")?;
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
                        requested_by: key.as_ref().map(|_| requester.clone()).unwrap_or_default(),
                        request_fingerprint: key
                            .as_ref()
                            .map(|_| fingerprint.clone())
                            .unwrap_or_default(),
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
            schedules::check_launchable(
                &settings,
                schedule.workflow.as_deref(),
                &schedule.ceiling,
            )?;
            Ok(schedule)
        },
        // Any edit needs approval again; revoke only after save succeeds while
        // schedules.lock is still held.
        |schedule| schedules::revoke(&run.project, &schedule.id),
    )
    .map_err(|e| error("invalid_request", format!("{e:#}")))?;
    Ok(schedule_view(&run.project, &schedule))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_log_pages_by_bytes_read() {
        let dir = tempfile::tempdir().unwrap();
        let attempt = dir.path().join("stages/01/attempt-1");
        std::fs::create_dir_all(&attempt).unwrap();
        // An invalid byte, then more characters than one page holds, ending
        // in a two-byte character on the second page.
        let mut log = vec![0xFF];
        log.extend(std::iter::repeat_n(b'x', READ_LIMIT + 5));
        log.extend("é".as_bytes());
        std::fs::write(attempt.join("stdout.log"), &log).unwrap();

        let (mut text, mut next) = read_log(dir.path(), &attempt, 0).unwrap();
        let mut pages = 1;
        while let Some(cursor) = next {
            let (more, after) = read_log(dir.path(), &attempt, cursor).unwrap();
            text.push_str(&more);
            next = after;
            pages += 1;
        }
        assert_eq!(pages, 2);
        assert_eq!(
            text,
            format!("\u{FFFD}{}é", "x".repeat(READ_LIMIT + 5)),
            "every byte is read once"
        );

        // A stray continuation byte that starts a page shows as U+FFFD.
        let mut log = "x".repeat(READ_LIMIT).into_bytes();
        log.push(0x80);
        log.push(b'A');
        std::fs::write(attempt.join("stdout.log"), &log).unwrap();
        let (mut text, next) = read_log(dir.path(), &attempt, 0).unwrap();
        let (more, after) = read_log(dir.path(), &attempt, next.unwrap()).unwrap();
        text.push_str(&more);
        assert_eq!(after, None);
        assert_eq!(text, format!("{}\u{FFFD}A", "x".repeat(READ_LIMIT)));

        // From a cursor inside a 4-byte character, the first read ends one
        // byte into a character, which the second page reads whole.
        std::fs::write(attempt.join("stdout.log"), "𝄞".repeat(READ_LIMIT + 1)).unwrap();
        let (mut text, mut next) = read_log(dir.path(), &attempt, 1).unwrap();
        while let Some(cursor) = next {
            let (more, after) = read_log(dir.path(), &attempt, cursor).unwrap();
            text.push_str(&more);
            next = after;
        }
        assert_eq!(text, "𝄞".repeat(READ_LIMIT));
    }
}
