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
use chrono::{
    DateTime, Datelike, Duration, FixedOffset, Local, MappedLocalTime, NaiveDate, NaiveDateTime,
    NaiveTime, TimeZone, Timelike, Utc,
};
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
    /// Daily at this time (`HH:MM`) in `tz` (default local).
    At {
        time: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
    /// Standard 5-field cron expression (`M H DOM MON DOW`) in `tz` (default local).
    Cron {
        expr: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
    /// On each `POST /schedules/<id>/trigger` to `serve --http`.
    Webhook,
}
impl Trigger {
    pub fn describe(&self) -> String {
        match self {
            Self::Every { text, .. } => format!("every {text}"),
            Self::At { time, tz } => {
                format!("daily at {time} ({})", tz.as_deref().unwrap_or("local"))
            }
            Self::Cron { expr, tz } => {
                format!("cron {expr} ({})", tz.as_deref().unwrap_or("local"))
            }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TzSpec {
    Local,
    Fixed(FixedOffset),
}

/// Parses a schedule timezone: `None` or `"local"` (case-insensitive) for
/// local time, `"UTC"` / `"Z"` for UTC, or a fixed offset like `"+02:00"` /
/// `"-07:00"`. Returns the canonical stored label (`None` for local) and the
/// resolved offset spec.
fn parse_tz(tz: Option<&str>) -> Result<(Option<String>, TzSpec)> {
    let Some(raw) = tz.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok((None, TzSpec::Local));
    };
    if raw.eq_ignore_ascii_case("local") {
        return Ok((None, TzSpec::Local));
    }
    if raw.eq_ignore_ascii_case("utc") || raw.eq_ignore_ascii_case("z") {
        let offset = FixedOffset::east_opt(0).expect("UTC offset");
        return Ok((Some("UTC".into()), TzSpec::Fixed(offset)));
    }
    let bytes = raw.as_bytes();
    if bytes.len() == 6 && (bytes[0] == b'+' || bytes[0] == b'-') && bytes[3] == b':' {
        let hours: i32 = raw[1..3]
            .parse()
            .with_context(|| format!("Invalid timezone offset {raw:?}"))?;
        let mins: i32 = raw[4..6]
            .parse()
            .with_context(|| format!("Invalid timezone offset {raw:?}"))?;
        if (0..=23).contains(&hours) && (0..=59).contains(&mins) {
            let secs = hours * 3600 + mins * 60;
            let offset = if bytes[0] == b'+' {
                FixedOffset::east_opt(secs)
            } else {
                FixedOffset::west_opt(secs)
            };
            if let Some(offset) = offset {
                let sign = raw.as_bytes()[0] as char;
                return Ok((
                    Some(format!("{sign}{hours:02}:{mins:02}")),
                    TzSpec::Fixed(offset),
                ));
            }
        }
    }
    bail!("tz must be local, UTC, or a fixed offset like +02:00 or -07:00, not {raw:?}");
}

/// Standard 5-field cron expression (`M H DOM MON DOW`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct CronExpr {
    minutes: [bool; 60],
    hours: [bool; 24],
    doms: [bool; 32],
    months: [bool; 13],
    dows: [bool; 7],
    dom_wild: bool,
    dow_wild: bool,
}

fn parse_cron_field(
    field: &str,
    name: &str,
    min: usize,
    max: usize,
    sunday_is_seven: bool,
    out: &mut [bool],
) -> Result<()> {
    ensure!(!field.is_empty(), "cron {name} field is empty");
    for item in field.split(',') {
        ensure!(
            !item.is_empty(),
            "cron {name} field has an empty list entry: {field:?}"
        );
        let (range_part, step_part) = match item.split_once('/') {
            Some((left, right)) => {
                ensure!(
                    !right.contains('/'),
                    "cron {name} item {item:?} has multiple '/' steps"
                );
                let step: usize = right
                    .parse()
                    .with_context(|| format!("cron {name} step must be a number in {item:?}"))?;
                ensure!(step >= 1, "cron {name} step must be at least 1 in {item:?}");
                (left, Some(step))
            }
            None => (item, None),
        };
        let step = step_part.unwrap_or(1);
        let (start, end) = if range_part == "*" {
            (min, max)
        } else if let Some((a, b)) = range_part.split_once('-') {
            ensure!(
                !b.contains('-'),
                "cron {name} range {range_part:?} has multiple '-'"
            );
            let start: usize = a
                .parse()
                .with_context(|| format!("cron {name} value must be a number in {item:?}"))?;
            let end: usize = b
                .parse()
                .with_context(|| format!("cron {name} value must be a number in {item:?}"))?;
            ensure!(
                start >= min && end <= max && start <= end,
                "cron {name} range {start}-{end} must be within {min}..={max}"
            );
            (start, end)
        } else {
            let value: usize = range_part
                .parse()
                .with_context(|| format!("cron {name} value must be a number in {item:?}"))?;
            ensure!(
                value >= min && value <= max,
                "cron {name} value {value} must be within {min}..={max}"
            );
            if step_part.is_some() {
                (value, max)
            } else {
                (value, value)
            }
        };
        let mut value = start;
        while value <= end {
            let slot = if sunday_is_seven && value == 7 {
                0
            } else {
                value
            };
            out[slot] = true;
            value = match value.checked_add(step) {
                Some(next) => next,
                None => break,
            };
        }
    }
    Ok(())
}

fn max_days_in_month(month: usize) -> usize {
    match month {
        2 => 29,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn parse_cron(expr: &str) -> Result<(String, CronExpr)> {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    ensure!(
        parts.len() == 5,
        "cron expression must have 5 fields (M H DOM MON DOW), got {}: {expr:?}",
        parts.len()
    );
    let mut cron = CronExpr {
        minutes: [false; 60],
        hours: [false; 24],
        doms: [false; 32],
        months: [false; 13],
        dows: [false; 7],
        dom_wild: parts[2] == "*",
        dow_wild: parts[4] == "*",
    };
    parse_cron_field(parts[0], "minute", 0, 59, false, &mut cron.minutes)?;
    parse_cron_field(parts[1], "hour", 0, 23, false, &mut cron.hours)?;
    parse_cron_field(parts[2], "day-of-month", 1, 31, false, &mut cron.doms)?;
    parse_cron_field(parts[3], "month", 1, 12, false, &mut cron.months)?;
    parse_cron_field(parts[4], "day-of-week", 0, 7, true, &mut cron.dows)?;
    if !cron.dom_wild {
        let feasible = (1..=12)
            .filter(|&m| cron.months[m])
            .any(|m| (1..=max_days_in_month(m)).any(|d| cron.doms[d]));
        ensure!(
            feasible,
            "cron expression {expr:?} matches no valid calendar date"
        );
    }
    Ok((parts.join(" "), cron))
}

fn resolve_datetime_after(
    naive: NaiveDateTime,
    tz: TzSpec,
    after: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    match tz {
        TzSpec::Fixed(offset) => offset
            .from_local_datetime(&naive)
            .single()
            .map(|dt| dt.with_timezone(&Utc))
            .filter(|dt| *dt > after),
        TzSpec::Local => match Local.from_local_datetime(&naive) {
            MappedLocalTime::Single(dt) => {
                let utc = dt.with_timezone(&Utc);
                (utc > after).then_some(utc)
            }
            MappedLocalTime::Ambiguous(early, late) => {
                let early = early.with_timezone(&Utc);
                if early > after {
                    Some(early)
                } else {
                    let late = late.with_timezone(&Utc);
                    (late > after).then_some(late)
                }
            }
            MappedLocalTime::None => None,
        },
    }
}

fn resolve_date_time(date: NaiveDate, time: NaiveTime, tz: TzSpec) -> Option<DateTime<Utc>> {
    let naive = date.and_time(time);
    match tz {
        TzSpec::Fixed(offset) => offset
            .from_local_datetime(&naive)
            .single()
            .map(|dt| dt.with_timezone(&Utc)),
        TzSpec::Local => match Local.from_local_datetime(&naive) {
            MappedLocalTime::Single(dt) => Some(dt.with_timezone(&Utc)),
            MappedLocalTime::Ambiguous(early, _) => Some(early.with_timezone(&Utc)),
            MappedLocalTime::None => {
                // Spring-forward DST gap: advance to the first valid minute on that date.
                let mut cur = naive;
                for _ in 0..180 {
                    cur += Duration::minutes(1);
                    if let MappedLocalTime::Single(dt) = Local.from_local_datetime(&cur) {
                        return Some(dt.with_timezone(&Utc));
                    }
                }
                None
            }
        },
    }
}

impl CronExpr {
    fn day_matches(&self, date: NaiveDate) -> bool {
        let dom = date.day() as usize;
        let dow = date.weekday().num_days_from_sunday() as usize;
        match (self.dom_wild, self.dow_wild) {
            (true, true) => true,
            (false, true) => self.doms[dom],
            (true, false) => self.dows[dow],
            (false, false) => self.doms[dom] || self.dows[dow],
        }
    }

    /// Earliest firing time strictly after `after` in `tz`.
    fn next_after(&self, after: DateTime<Utc>, tz: TzSpec) -> Option<DateTime<Utc>> {
        let after_naive = match tz {
            TzSpec::Local => after.with_timezone(&Local).naive_local(),
            TzSpec::Fixed(offset) => after.with_timezone(&offset).naive_local(),
        };
        let mut cur =
            after_naive
                .date()
                .and_hms_opt(after_naive.hour(), after_naive.minute(), 0)?;
        if matches!(tz, TzSpec::Fixed(_)) {
            cur += Duration::minutes(1);
        }
        let max_year = cur.year() + 8;
        while cur.year() <= max_year {
            let month = cur.month() as usize;
            if !self.months[month] {
                let (next_year, next_month) = if month == 12 {
                    (cur.year() + 1, 1)
                } else {
                    (cur.year(), (month + 1) as u32)
                };
                cur = NaiveDate::from_ymd_opt(next_year, next_month, 1)?.and_hms_opt(0, 0, 0)?;
                continue;
            }
            if !self.day_matches(cur.date()) {
                cur = cur.date().succ_opt()?.and_hms_opt(0, 0, 0)?;
                continue;
            }
            let hour = cur.hour() as usize;
            if !self.hours[hour] {
                let hour_start = cur.date().and_hms_opt(cur.hour(), 0, 0)?;
                cur = hour_start + Duration::hours(1);
                continue;
            }
            let minute = cur.minute() as usize;
            if !self.minutes[minute] {
                cur += Duration::minutes(1);
                continue;
            }
            if let Some(utc) = resolve_datetime_after(cur, tz, after) {
                return Some(utc);
            }
            cur += Duration::minutes(1);
        }
        None
    }
}

/// Parses `every` (`30m`, `6h`, `1d`, `2w`), `at` (`HH:MM`), `cron` (5-field
/// `M H DOM MON DOW`), or a webhook, with an optional `tz` for `at`/`cron`.
pub fn parse_trigger(
    every: Option<&str>,
    at: Option<&str>,
    cron: Option<&str>,
    webhook: bool,
    tz: Option<&str>,
) -> Result<Trigger> {
    let tz_nonempty = tz.map(str::trim).filter(|s| !s.is_empty());
    match (every, at, cron, webhook) {
        (Some(every), None, None, false) => {
            ensure!(
                tz_nonempty.is_none(),
                "tz applies only to at or cron triggers"
            );
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
        (None, Some(at), None, false) => {
            NaiveTime::parse_from_str(at.trim(), "%H:%M")
                .with_context(|| format!("at must be HH:MM, not {at:?}"))?;
            let (tz_label, _) = parse_tz(tz_nonempty)?;
            Ok(Trigger::At {
                time: at.trim().to_owned(),
                tz: tz_label,
            })
        }
        (None, None, Some(cron), false) => {
            let (normalized, _) = parse_cron(cron)?;
            let (tz_label, _) = parse_tz(tz_nonempty)?;
            Ok(Trigger::Cron {
                expr: normalized,
                tz: tz_label,
            })
        }
        (None, None, None, true) => {
            ensure!(
                tz_nonempty.is_none(),
                "tz applies only to at or cron triggers"
            );
            Ok(Trigger::Webhook)
        }
        _ => bail!("Give exactly one of every, at, cron, or webhook"),
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

/// Options for creating a schedule from the CLI (`mr schedules add`).
#[derive(Debug, Clone)]
pub struct AddOptions {
    pub id: Option<String>,
    pub workflow: Option<String>,
    pub task: String,
    pub trigger: Trigger,
    pub approve: bool,
}

/// Creates a user-authored schedule in `.model-router/schedules.json`.
/// When `options.approve` is true, requires a trusted project config and writes
/// the approval receipt atomically so the schedule can fire right away.
pub fn add(project: &Path, options: AddOptions) -> Result<(Schedule, bool)> {
    crate::config::validate_task(&options.task)?;
    if let Some(id) = &options.id {
        ensure!(
            safe_id(id),
            "Invalid schedule id {id:?}: use 1–64 ASCII letters, digits, '-' or '_'"
        );
    }
    let settings = if options.approve {
        trusted_settings(project)?
    } else {
        let loaded = crate::config::load_config(project, None)?;
        match loaded.trust {
            crate::trust::Trust::Trusted => loaded.settings,
            crate::trust::Trust::Untrusted(_) => loaded.safe(),
        }
    };
    let ceiling = policy::USER_CEILING;
    check_launchable(&settings, options.workflow.as_deref(), &ceiling)?;
    let schedule = update(project, |store| {
        let id = match &options.id {
            Some(id) => {
                ensure!(
                    !store.schedules.iter().any(|s| s.id == *id),
                    "Schedule {id} already exists"
                );
                id.clone()
            }
            None => {
                let number = store
                    .schedules
                    .iter()
                    .filter_map(|s| s.id.strip_prefix('s')?.parse::<usize>().ok())
                    .max()
                    .unwrap_or(0)
                    + 1;
                format!("s{number}")
            }
        };
        let now = Utc::now();
        let mut schedule = Schedule {
            id: id.clone(),
            workflow: options.workflow.clone(),
            task: options.task.clone(),
            trigger: options.trigger.clone(),
            enabled: true,
            created_by: "user".into(),
            ceiling: ceiling.clone(),
            client_request_id: String::new(),
            requested_by: String::new(),
            request_fingerprint: String::new(),
            revision: 1,
            created_at: now,
            updated_at: now,
            approved_at: None,
            paused_reason: None,
            occurrences: vec![],
        };
        if options.approve {
            let digest = digest(project, &schedule, &settings)?;
            let path =
                receipt(project, &id).with_context(|| format!("Invalid schedule id {id}"))?;
            fs::create_dir_all(approvals_dir(project))?;
            crate::storage::atomic_write(&path, digest.as_bytes())?;
            schedule.approved_at = Some(now);
        }
        store.schedules.push(schedule.clone());
        Ok(schedule)
    })?;
    Ok((schedule, options.approve))
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
        Trigger::At { time, tz } => {
            let time = NaiveTime::parse_from_str(time, "%H:%M").ok()?;
            let (_, tz_spec) = parse_tz(tz.as_deref()).ok()?;
            let today_date = match tz_spec {
                TzSpec::Local => now.with_timezone(&Local).date_naive(),
                TzSpec::Fixed(offset) => now.with_timezone(&offset).date_naive(),
            };
            let today = resolve_date_time(today_date, time, tz_spec)?;
            let after = last.unwrap_or(schedule.approved_at.unwrap_or(schedule.created_at));
            if today > after {
                Some(today)
            } else {
                let tomorrow = today_date.succ_opt()?;
                resolve_date_time(tomorrow, time, tz_spec)
            }
        }
        Trigger::Cron { expr, tz } => {
            let (_, cron) = parse_cron(expr).ok()?;
            let (_, tz_spec) = parse_tz(tz.as_deref()).ok()?;
            let after = last.or(schedule.approved_at).unwrap_or(schedule.created_at);
            cron.next_after(after, tz_spec)
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

    fn utc(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn triggers_parse_and_refuse_ambiguity() {
        assert!(matches!(
            parse_trigger(Some("6h"), None, None, false, None).unwrap(),
            Trigger::Every { seconds: 21600, .. }
        ));
        assert!(
            parse_trigger(Some("1m"), None, None, false, None).is_err(),
            "more often than every 5 minutes"
        );
        assert_eq!(
            parse_trigger(None, Some("09:30"), None, false, None).unwrap(),
            Trigger::At {
                time: "09:30".into(),
                tz: None,
            }
        );
        assert_eq!(
            parse_trigger(None, Some("09:30"), None, false, Some("utc")).unwrap(),
            Trigger::At {
                time: "09:30".into(),
                tz: Some("UTC".into()),
            }
        );
        assert_eq!(
            parse_trigger(None, Some("09:30"), None, false, Some("+02:00")).unwrap(),
            Trigger::At {
                time: "09:30".into(),
                tz: Some("+02:00".into()),
            }
        );
        assert_eq!(
            parse_trigger(None, Some("09:30"), None, false, Some("local")).unwrap(),
            Trigger::At {
                time: "09:30".into(),
                tz: None,
            }
        );
        assert_eq!(
            parse_trigger(None, None, Some("0 9 * * 1-5"), false, Some("-07:00")).unwrap(),
            Trigger::Cron {
                expr: "0 9 * * 1-5".into(),
                tz: Some("-07:00".into()),
            }
        );
        assert!(parse_trigger(None, Some("9.30"), None, false, None).is_err());
        assert!(parse_trigger(Some("6h"), Some("09:30"), None, false, None).is_err());
        assert!(parse_trigger(None, Some("09:30"), Some("0 9 * * *"), false, None).is_err());
        assert!(parse_trigger(Some("6h"), None, None, false, Some("UTC")).is_err());
        assert!(parse_trigger(None, None, None, true, Some("UTC")).is_err());
        assert!(parse_trigger(None, Some("09:30"), None, false, Some("Mars/Olympus")).is_err());
        assert!(parse_trigger(None, Some("09:30"), None, false, Some("+25:00")).is_err());
        assert!(parse_trigger(None, None, None, false, None).is_err());
        assert_eq!(
            parse_trigger(None, None, None, true, None).unwrap(),
            Trigger::Webhook
        );
    }

    #[test]
    fn every_counts_from_the_last_due_time() {
        let mut s = schedule(parse_trigger(Some("6h"), None, None, false, None).unwrap());
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
    fn at_trigger_respects_utc_and_fixed_offsets() {
        // 09:00 +02:00 is 07:00 UTC.
        let mut s =
            schedule(parse_trigger(None, Some("09:00"), None, false, Some("+02:00")).unwrap());
        let now = utc("2026-01-01T06:00:00Z");
        assert_eq!(next_due(&s, now), Some(utc("2026-01-01T07:00:00Z")));
        s.occurrences.push(Occurrence {
            due: utc("2026-01-01T07:00:00Z"),
            fired_at: utc("2026-01-01T07:00:05Z"),
            run: None,
            error: None,
        });
        assert_eq!(next_due(&s, now), Some(utc("2026-01-02T07:00:00Z")));

        // Legacy Trigger::At JSON without tz deserializes cleanly as local.
        let legacy: Trigger = serde_json::from_str(r#"{"at":{"time":"08:15"}}"#).unwrap();
        assert_eq!(
            legacy,
            Trigger::At {
                time: "08:15".into(),
                tz: None
            }
        );
        assert_eq!(legacy.describe(), "daily at 08:15 (local)");
    }

    #[test]
    fn cron_expressions_validate_and_compute_next_due() {
        // Every 15 minutes in UTC.
        let mut s =
            schedule(parse_trigger(None, None, Some("*/15 * * * *"), false, Some("UTC")).unwrap());
        assert_eq!(s.trigger.describe(), "cron */15 * * * * (UTC)");
        assert_eq!(
            next_due(&s, utc("2026-01-01T00:00:00Z")),
            Some(utc("2026-01-01T00:15:00Z"))
        );

        // Weekdays at 09:30 in -07:00 (= 16:30 UTC). 2026-01-02 is Friday; next weekday is Mon 2026-01-05.
        s.trigger = parse_trigger(None, None, Some("30 9 * * 1-5"), false, Some("-07:00")).unwrap();
        s.approved_at = Some(utc("2026-01-02T17:00:00Z"));
        assert_eq!(
            next_due(&s, utc("2026-01-02T17:00:00Z")),
            Some(utc("2026-01-05T16:30:00Z"))
        );

        // Comma lists and Sunday as 7. 2026-01-04 is Sunday.
        s.trigger = parse_trigger(None, None, Some("0,30 8,20 * * 7"), false, Some("UTC")).unwrap();
        s.approved_at = Some(utc("2026-01-03T12:00:00Z"));
        assert_eq!(
            next_due(&s, utc("2026-01-03T12:00:00Z")),
            Some(utc("2026-01-04T08:00:00Z"))
        );

        // Leap day: 2026-01-01 -> 2028-02-29 00:00 UTC.
        s.trigger = parse_trigger(None, None, Some("0 0 29 2 *"), false, Some("UTC")).unwrap();
        s.approved_at = Some(utc("2026-01-01T00:00:00Z"));
        assert_eq!(
            next_due(&s, utc("2026-01-01T00:00:00Z")),
            Some(utc("2028-02-29T00:00:00Z"))
        );

        // Vixie cron: when both DOM and DOW are restricted, either matches.
        // 2026-01-01 is Thursday (4); 15th or Sunday (0) matches Sun Jan 4 first.
        s.trigger = parse_trigger(None, None, Some("0 0 15 * 0"), false, Some("UTC")).unwrap();
        s.approved_at = Some(utc("2026-01-01T00:00:00Z"));
        assert_eq!(
            next_due(&s, utc("2026-01-01T00:00:00Z")),
            Some(utc("2026-01-04T00:00:00Z"))
        );

        // Invalid cron expressions.
        for bad in [
            "",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * 32 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "10-5 * * * *",
            "1,,2 * * * *",
            "0 0 31 2 *",
        ] {
            assert!(
                parse_trigger(None, None, Some(bad), false, Some("UTC")).is_err(),
                "expected invalid cron: {bad:?}"
            );
        }
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
