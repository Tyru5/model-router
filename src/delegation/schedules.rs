//! Schedules (`delegation.schedules`): recurring runs an agent asks for and
//! the user approves. Definitions live in `.model-router/schedules.json`;
//! approvals live outside the workspace, in
//! `config_dir()/schedule-approvals/<project>/<id>`, bound to a digest of
//! everything the schedule would run, so an agent cannot approve its own
//! and any edit or pause needs approval again.
use super::policy::{self, Ceiling};
use crate::config::{Mode, Settings, Workflow};
use crate::engine::{CreationSource, Placement, Run};
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Duration, Local, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// When a schedule fires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// Every this many seconds after the last firing (or approval).
    Every { seconds: i64, text: String },
    /// Daily at this local time, `HH:MM`.
    At { time: String },
    /// On each `POST /schedules/<id>/trigger` to `serve --http`.
    Webhook,
}
impl Trigger {
    pub fn describe(&self) -> String {
        match self {
            Self::Every { text, .. } => format!("every {text}"),
            Self::At { time } => format!("daily at {time} (local)"),
            Self::Webhook => "on webhook".into(),
        }
    }
}

/// One firing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occurrence {
    pub due: DateTime<Utc>,
    pub fired_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    pub task: String,
    pub trigger: Trigger,
    pub enabled: bool,
    /// The stage whose agent created it, and that stage's ceiling: firing
    /// rechecks the workflow against it.
    pub created_by: String,
    pub ceiling: Ceiling,
    /// The latest `schedule_task` call with a `client_request_id`: its key,
    /// the stage that sent it, and a digest of its payload. A retry of that
    /// call from the same stage changes nothing.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_request_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub requested_by: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub request_fingerprint: String,
    /// Bumped on every edit and enable or pause.
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_reason: Option<String>,
    /// The latest firings, newest last (at most 20).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub occurrences: Vec<Occurrence>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    pub schedules: Vec<Schedule>,
}

fn store_path(project: &Path) -> Result<PathBuf> {
    Ok(crate::storage::metadata_dir(project)?.join("schedules.json"))
}

fn lock_path(project: &Path) -> Result<PathBuf> {
    Ok(crate::storage::metadata_dir(project)?.join("schedules.lock"))
}

pub fn load(project: &Path) -> Result<Store> {
    let path = store_path(project)?;
    if !path.exists() {
        return Ok(Store::default());
    }
    crate::storage::read_json(&path)
}

fn save(project: &Path, store: &Store) -> Result<()> {
    let path = store_path(project)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::storage::write_json(&path, store)
}

/// Applies `change` to the store under the schedules lock.
pub fn update<T>(project: &Path, change: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
    let lock = lock_path(project)?;
    if let Some(parent) = lock.parent() {
        fs::create_dir_all(parent)?;
    }
    let _guard = crate::storage::lock_wait(&lock, std::time::Duration::from_secs(10))?;
    let mut store = load(project)?;
    let value = change(&mut store)?;
    save(project, &store)?;
    Ok(value)
}

/// Parses `every` (`30m`, `6h`, `1d`, `2w`), `at` (`HH:MM`), or a webhook.
pub fn parse_trigger(every: Option<&str>, at: Option<&str>, webhook: bool) -> Result<Trigger> {
    match (every, at, webhook) {
        (Some(every), None, false) => {
            let duration = crate::cost::parse_since(every)?;
            ensure!(
                duration >= Duration::minutes(5),
                "A schedule fires at most every 5 minutes"
            );
            Ok(Trigger::Every {
                seconds: duration.num_seconds(),
                text: every.trim().to_owned(),
            })
        }
        (None, Some(at), false) => {
            NaiveTime::parse_from_str(at.trim(), "%H:%M")
                .with_context(|| format!("at must be HH:MM, not {at:?}"))?;
            Ok(Trigger::At {
                time: at.trim().to_owned(),
            })
        }
        (None, None, true) => Ok(Trigger::Webhook),
        _ => bail!("Give exactly one of every, at, or webhook"),
    }
}

fn approvals_dir(project: &Path) -> PathBuf {
    let key = fs::canonicalize(project)
        .unwrap_or_else(|_| project.to_path_buf())
        .display()
        .to_string();
    crate::config::config_dir()
        .join("schedule-approvals")
        .join(crate::trust::digest(&[key]))
}

/// Whether `id` is a schedule id the router could have made: a receipt
/// path is joined from it, so anything else (a path, `..`) is refused.
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn receipt(project: &Path, id: &str) -> Option<PathBuf> {
    safe_id(id).then(|| approvals_dir(project).join(id))
}

/// Forgets a schedule's approval (on any pause or edit).
pub fn revoke(project: &Path, id: &str) {
    if let Some(path) = receipt(project, id) {
        let _ = fs::remove_file(path);
    }
}

/// Forgets every schedule approval of `project` (`trust --revoke`).
pub fn revoke_all(project: &Path) {
    let _ = fs::remove_dir_all(approvals_dir(project));
}

/// Everything the schedule would run, in one digest: its definition, the
/// resolved workflow and the delegates it names, the creator's ceiling,
/// and the project's trusted config digest.
pub fn digest(project: &Path, schedule: &Schedule, settings: &Settings) -> Result<String> {
    let workflow = settings.workflow(schedule.workflow.as_deref())?;
    let delegates: Vec<(&String, &super::Delegate)> = workflow
        .stages
        .iter()
        .filter_map(|s| s.delegation.as_ref())
        .flat_map(|d| &d.delegates)
        .filter_map(|name| settings.delegates.get_key_value(name))
        .collect();
    let key = fs::canonicalize(project)
        .unwrap_or_else(|_| project.to_path_buf())
        .display()
        .to_string();
    Ok(crate::trust::digest(&[
        key,
        schedule.id.clone(),
        schedule.revision.to_string(),
        serde_json::to_string(workflow)?,
        serde_json::to_string(&delegates)?,
        schedule.task.clone(),
        serde_json::to_string(&schedule.trigger)?,
        schedule.enabled.to_string(),
        serde_json::to_string(&schedule.ceiling)?,
        crate::trust::digest(&crate::trust::dangerous(settings)),
    ]))
}

/// Whether the schedule's current definition is approved.
pub fn approved(project: &Path, schedule: &Schedule, settings: &Settings) -> bool {
    let Ok(expected) = digest(project, schedule, settings) else {
        return false;
    };
    receipt(project, &schedule.id)
        .is_some_and(|path| fs::read_to_string(path).is_ok_and(|text| text.trim() == expected))
}

/// The project's config, refused unless trusted.
pub fn trusted_settings(project: &Path) -> Result<Settings> {
    let loaded = crate::config::load_config(project, None)?;
    ensure!(
        matches!(loaded.trust, crate::trust::Trust::Trusted),
        "The project config is untrusted; run `model-router trust` first"
    );
    Ok(loaded.settings)
}

/// Approves schedule `id` as it stands now (`mr schedules approve`).
pub fn approve(project: &Path, id: &str) -> Result<Schedule> {
    let settings = trusted_settings(project)?;
    update(project, |store| {
        let schedule = store
            .schedules
            .iter_mut()
            .find(|s| s.id == id)
            .with_context(|| format!("No schedule {id}"))?;
        ensure!(schedule.enabled, "Schedule {id} is paused; enable it first");
        check_launchable(&settings, schedule.workflow.as_deref(), &schedule.ceiling)?;
        let digest = digest(project, schedule, &settings)?;
        let path = receipt(project, id).with_context(|| format!("Invalid schedule id {id}"))?;
        fs::create_dir_all(approvals_dir(project))?;
        crate::storage::atomic_write(&path, digest.as_bytes())?;
        schedule.approved_at = Some(Utc::now());
        schedule.paused_reason = None;
        Ok(schedule.clone())
    })
}

/// Pauses (or enables) schedule `id`; either way it needs approval again.
pub fn set_enabled(project: &Path, id: &str, enabled: bool) -> Result<Schedule> {
    update(project, |store| {
        let schedule = store
            .schedules
            .iter_mut()
            .find(|s| s.id == id)
            .with_context(|| format!("No schedule {id}"))?;
        revoke(project, id);
        schedule.enabled = enabled;
        schedule.revision += 1;
        schedule.updated_at = Utc::now();
        schedule.approved_at = None;
        Ok(schedule.clone())
    })
}

pub fn delete(project: &Path, id: &str) -> Result<()> {
    update(project, |store| {
        let before = store.schedules.len();
        store.schedules.retain(|s| s.id != id);
        ensure!(store.schedules.len() < before, "No schedule {id}");
        revoke(project, id);
        Ok(())
    })
}

/// Refuses a workflow a schedule may not run: unknown, interactive, or
/// beyond the creator's ceiling.
pub fn check_launchable(
    settings: &Settings,
    workflow: Option<&str>,
    ceiling: &Ceiling,
) -> Result<Workflow> {
    let workflow = settings.workflow(workflow)?.clone();
    let interactive: Vec<&str> = workflow
        .stages
        .iter()
        .filter(|s| s.enabled && s.mode == Mode::Interactive)
        .map(|s| s.id.as_str())
        .collect();
    ensure!(
        interactive.is_empty(),
        "Workflow {} has interactive stages ({}); schedules run unattended workflows only",
        workflow.name,
        interactive.join(", ")
    );
    let backend = crate::sandbox::detect().is_ok();
    for (what, needed) in policy::workflow_ceilings(&workflow, backend) {
        if let Some(why) = policy::exceeds(&needed, ceiling) {
            bail!(
                "Workflow {}: {what} exceeds the creating stage: {why}",
                workflow.name
            );
        }
    }
    Ok(workflow)
}

/// When the schedule is next due, if it is time-triggered.
pub fn next_due(schedule: &Schedule, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let last = schedule.occurrences.last().map(|o| o.due);
    match &schedule.trigger {
        Trigger::Every { seconds, .. } => {
            let from = last.or(schedule.approved_at).unwrap_or(schedule.created_at);
            Some(from + Duration::seconds(*seconds))
        }
        Trigger::At { time } => {
            let time = NaiveTime::parse_from_str(time, "%H:%M").ok()?;
            let local_now = now.with_timezone(&Local);
            let today = local_now
                .date_naive()
                .and_time(time)
                .and_local_timezone(Local)
                .single()?
                .with_timezone(&Utc);
            let after = last.unwrap_or(schedule.approved_at.unwrap_or(schedule.created_at));
            if today > after {
                Some(today)
            } else {
                Some(today + Duration::days(1))
            }
        }
        Trigger::Webhook => None,
    }
}

/// A run a firing created, to drive.
pub struct Fired {
    pub schedule: String,
    pub run: Option<Run>,
    pub error: Option<String>,
}

/// Claims and creates the runs of every approved schedule due at `now` (or,
/// with `only`, that webhook schedule): each occurrence is recorded under
/// the schedules lock before its run is created, so `serve` and `tick`
/// never both fire it. A launch that fails after its claim is recorded as
/// failed and not retried. A schedule whose approval or checks no longer
/// hold is paused with the reason.
pub fn fire_due(project: &Path, now: DateTime<Utc>, only: Option<&str>) -> Result<Vec<Fired>> {
    if !load(project)?.schedules.iter().any(|s| s.enabled) {
        return Ok(vec![]);
    }
    let settings = trusted_settings(project)?;
    let claimed: Vec<(Schedule, DateTime<Utc>)> = update(project, |store| {
        let mut claimed = vec![];
        for schedule in &mut store.schedules {
            if !schedule.enabled {
                continue;
            }
            let due = match (only, &schedule.trigger) {
                (Some(id), Trigger::Webhook) if id == schedule.id => now,
                (Some(_), _) | (None, Trigger::Webhook) => continue,
                (None, _) => match next_due(schedule, now) {
                    Some(due) if due <= now => due,
                    _ => continue,
                },
            };
            if !approved(project, schedule, &settings) {
                continue;
            }
            if let Err(error) =
                check_launchable(&settings, schedule.workflow.as_deref(), &schedule.ceiling)
            {
                schedule.enabled = false;
                schedule.revision += 1;
                schedule.paused_reason = Some(format!("{error:#}"));
                revoke(project, &schedule.id);
                continue;
            }
            schedule.occurrences.push(Occurrence {
                due,
                fired_at: now,
                run: None,
                error: None,
            });
            if schedule.occurrences.len() > 20 {
                schedule.occurrences.remove(0);
            }
            claimed.push((schedule.clone(), due));
        }
        Ok(claimed)
    })?;
    let mut fired = vec![];
    for (schedule, due) in claimed {
        let created = check_launchable(&settings, schedule.workflow.as_deref(), &schedule.ceiling)
            .and_then(|mut workflow| {
                for stage in &mut workflow.stages {
                    if let Some(delegation) = stage.delegation.as_mut() {
                        delegation.runs = false;
                        delegation.schedules = false;
                    }
                }
                let mut run = Run::create_at(
                    project,
                    &settings,
                    &workflow,
                    schedule.task.clone(),
                    Placement::WorktreeFromHead,
                )?;
                run.creation_source = CreationSource::Schedule;
                run.persist()?;
                Ok(run)
            });
        let (run, error) = match created {
            Ok(run) => (Some(run), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        let (run_id, recorded_error) = (run.as_ref().map(|r| r.id.clone()), error.clone());
        update(project, |store| {
            if let Some(occurrence) = store
                .schedules
                .iter_mut()
                .find(|s| s.id == schedule.id)
                .and_then(|s| s.occurrences.iter_mut().rev().find(|o| o.due == due))
            {
                occurrence.run = run_id;
                occurrence.error = recorded_error;
            }
            Ok(())
        })?;
        fired.push(Fired {
            schedule: schedule.id.clone(),
            run,
            error,
        });
    }
    Ok(fired)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule(trigger: Trigger) -> Schedule {
        let at = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        Schedule {
            id: "s1".into(),
            workflow: None,
            task: "t".into(),
            trigger,
            enabled: true,
            created_by: "run/stage".into(),
            ceiling: policy::UNSANDBOXED_COMMAND,
            client_request_id: String::new(),
            requested_by: String::new(),
            request_fingerprint: String::new(),
            revision: 1,
            created_at: at,
            updated_at: at,
            approved_at: Some(at),
            paused_reason: None,
            occurrences: vec![],
        }
    }

    #[test]
    fn triggers_parse_and_refuse_ambiguity() {
        assert!(matches!(
            parse_trigger(Some("6h"), None, false).unwrap(),
            Trigger::Every { seconds: 21600, .. }
        ));
        assert!(
            parse_trigger(Some("1m"), None, false).is_err(),
            "more often than every 5 minutes"
        );
        assert!(matches!(
            parse_trigger(None, Some("09:30"), false).unwrap(),
            Trigger::At { .. }
        ));
        assert!(parse_trigger(None, Some("9.30"), false).is_err());
        assert!(parse_trigger(Some("6h"), Some("09:30"), false).is_err());
        assert!(parse_trigger(None, None, false).is_err());
        assert_eq!(parse_trigger(None, None, true).unwrap(), Trigger::Webhook);
    }

    #[test]
    fn every_counts_from_the_last_due_time() {
        let mut s = schedule(parse_trigger(Some("6h"), None, false).unwrap());
        let start = s.approved_at.unwrap();
        assert_eq!(next_due(&s, start), Some(start + Duration::hours(6)));
        s.occurrences.push(Occurrence {
            due: start + Duration::hours(6),
            fired_at: start + Duration::hours(7),
            run: None,
            error: None,
        });
        assert_eq!(next_due(&s, start), Some(start + Duration::hours(12)));
        assert_eq!(next_due(&schedule(Trigger::Webhook), start), None);
    }

    #[test]
    fn an_unknown_or_path_like_id_touches_no_file() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(project.join(".model-router")).unwrap();
        let victim = temp.path().join("victim");
        fs::write(&victim, "keep").unwrap();
        let id = victim.display().to_string();
        assert!(delete(&project, &id).is_err());
        assert!(set_enabled(&project, &id, false).is_err());
        assert!(
            victim.is_file(),
            "deleting an unknown schedule removed a file"
        );
        for bad in ["", "..", "a/b", "../x", &id] {
            assert!(receipt(&project, bad).is_none(), "{bad}");
        }
        assert!(receipt(&project, "s12").is_some());
    }
}
