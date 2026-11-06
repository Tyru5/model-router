//! Routing insight from saved runs: per (role, harness, model) outcomes, and
//! conservative suggestions. Nothing here changes a workflow; the router never
//! classifies tasks or reroutes stages on its own.
//!
//! Each run is reduced to one `Sample` per stage that ran. Samples are cached
//! in `.model-router/stats-cache.json` by run revision, so a project's runs
//! are only reloaded after they change.
use crate::{
    config::Role,
    engine::{self, Run, Status},
    handoff::{TestStatus, Usage},
    history::{self, Entry},
    report, storage,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Fewer samples than this are anecdotal: tables flag them and suggestions
/// ignore them.
pub const MIN_SAMPLES: usize = 5;
/// How much higher a suggestion's clean-success rate must be.
pub const MIN_GAIN: f64 = 0.15;
const CACHE_VERSION: u32 = 1;

/// One stage of one saved run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub run: String,
    pub stage: String,
    pub role: Role,
    pub harness: String,
    /// The model of the stage's last attempt; empty is the harness default.
    pub model: String,
    pub attempts: u32,
    /// Completed with no failed, blocked, or cancelled attempt.
    pub clean: bool,
    /// Attempts started after an attempt that did not complete.
    pub retries: u32,
    pub failed: u32,
    pub blocked: u32,
    /// Implementation: times a downstream review sent work back to it.
    pub repairs_caused: u32,
    /// Review: times it requested a repair.
    pub repair_requests: u32,
    /// Review: its first attempt passed. Implementation: reviews downstream
    /// passed without a repair. None when no review judged it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_pass: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub checks_passed: u32,
    pub checks_total: u32,
}

/// The samples of a run. Stages still running, and stages that never ran,
/// give none.
pub fn samples(run: &Run) -> Vec<Sample> {
    let stages = &run.workflow.stages;
    // Every attempt in start order: a completed review followed by an
    // earlier stage's attempt sent work back for repair.
    let mut timeline: Vec<(usize, &engine::Attempt)> = run
        .stages
        .iter()
        .enumerate()
        .flat_map(|(index, state)| state.attempts.iter().map(move |a| (index, a)))
        .collect();
    timeline.sort_by_key(|(_, a)| a.started_at);
    let mut caused = vec![0u32; stages.len()];
    let mut requested = vec![0u32; stages.len()];
    // (stage, attempt number) of review attempts that requested a repair.
    let mut repairing = vec![];
    for pair in timeline.windows(2) {
        let ((review, attempt), (next, _)) = (pair[0], pair[1]);
        if stages[review].role == Role::Review
            && attempt.status == Status::Complete
            && next < review
        {
            requested[review] += 1;
            caused[next] += 1;
            repairing.push((review, attempt.number));
        }
    }
    // A review the repair limit stopped still requested one.
    if run.status == Status::Blocked && run.cursor < stages.len() {
        let review = run.cursor;
        let requests = run.stages[review]
            .handoff
            .as_ref()
            .is_some_and(crate::handoff::Handoff::requires_repair);
        if stages[review].role == Role::Review && requests {
            requested[review] += 1;
            let target = stages[..review].iter().rposition(|s| {
                s.enabled
                    && s.role == Role::Implementation
                    && (stages[review].repair_target.is_empty()
                        || s.id == stages[review].repair_target)
            });
            if let Some(target) = target {
                caused[target] += 1;
            }
            if let Some(attempt) = run.stages[review].attempts.last() {
                repairing.push((review, attempt.number));
            }
        }
    }
    let reviewed = |index: usize| {
        (index + 1..stages.len()).any(|i| {
            stages[i].role == Role::Review
                && (requested[i] > 0 || run.stages[i].status == Status::Complete)
        })
    };
    let mut out = vec![];
    for (index, (stage, state)) in stages.iter().zip(&run.stages).enumerate() {
        let Some(last) = state.attempts.last() else {
            continue;
        };
        // A fan-out round is several assignments at once, not one.
        if state.fan_out.is_some() {
            continue;
        }
        if matches!(state.status, Status::Running | Status::Interactive)
            || state.attempts.iter().any(|a| a.status == Status::Running)
        {
            continue;
        }
        let count = |status| state.attempts.iter().filter(|a| a.status == status).count() as u32;
        let (failed, blocked, cancelled) = (
            count(Status::Failed),
            count(Status::Blocked),
            count(Status::Cancelled),
        );
        let retries = state
            .attempts
            .windows(2)
            .filter(|pair| pair[0].status != Status::Complete)
            .count() as u32;
        let completed = state.status == Status::Complete || last.status == Status::Complete;
        let mut seconds = None;
        let mut usage: Option<Usage> = None;
        let (mut checks_passed, mut checks_total) = (0, 0);
        for attempt in &state.attempts {
            if let Some(finished) = attempt.finished_at {
                let spent = (finished - attempt.started_at).num_seconds().max(0);
                seconds = Some(seconds.unwrap_or(0) + spent);
            }
            if let Some(part) = attempt.usage {
                usage = Some(usage.map_or(part, |sum| sum + part));
            }
            for check in &attempt.checks {
                checks_total += 1;
                if check.status == TestStatus::Passed {
                    checks_passed += 1;
                }
            }
        }
        let first_pass = match stage.role {
            Role::Review => state.attempts.first().map(|first| {
                first.status == Status::Complete && !repairing.contains(&(index, first.number))
            }),
            Role::Implementation if reviewed(index) => Some(caused[index] == 0),
            _ => None,
        };
        out.push(Sample {
            run: run.id.clone(),
            stage: stage.id.clone(),
            role: stage.role,
            harness: stage.harness.clone(),
            model: if last.model.is_empty() {
                stage.model.clone()
            } else {
                last.model.clone()
            },
            attempts: state.attempts.len() as u32,
            clean: completed && failed + blocked + cancelled == 0,
            retries,
            failed,
            blocked,
            repairs_caused: caused[index],
            repair_requests: requested[index],
            first_pass,
            seconds,
            usage,
            checks_passed,
            checks_total,
        });
    }
    out
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    version: u32,
    runs: BTreeMap<String, CachedRun>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedRun {
    revision: u64,
    samples: Vec<Sample>,
}

/// What collecting a project's runs found.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Collected {
    pub samples: Vec<Sample>,
    pub runs: usize,
    /// Records that could not be read; they contribute nothing.
    pub damaged: usize,
    /// Runs loaded from run.json rather than the cache.
    #[serde(skip)]
    pub loaded: usize,
}

pub fn cache_path(project: &Path) -> PathBuf {
    project.join(".model-router/stats-cache.json")
}

/// Samples of every saved run in `project`, through the cache. Damaged and
/// unsupported records are counted and skipped. The cache is rewritten only
/// when it changed; failing to write it is not an error.
pub fn collect(project: &Path) -> Collected {
    let mut found = Collected::default();
    let Ok(entries) = history::list(project) else {
        return found;
    };
    let path = cache_path(project);
    let mut cache = storage::read_json::<Cache>(&path)
        .ok()
        .filter(|c| c.version == CACHE_VERSION)
        .unwrap_or_default();
    let mut fresh = BTreeMap::new();
    for entry in &entries {
        let summary = match entry {
            Entry::Summary(summary) => summary,
            Entry::Damaged { .. } => {
                found.damaged += 1;
                continue;
            }
        };
        let cached = cache
            .runs
            .remove(&summary.id)
            .filter(|c| c.revision == summary.revision && summary.revision > 0);
        let record = match cached {
            Some(record) => record,
            None => match engine::load(project, &summary.id) {
                Ok(run) => {
                    found.loaded += 1;
                    CachedRun {
                        revision: run.revision,
                        samples: samples(&run),
                    }
                }
                Err(_) => {
                    found.damaged += 1;
                    continue;
                }
            },
        };
        found.runs += 1;
        found.samples.extend(record.samples.iter().cloned());
        fresh.insert(summary.id.clone(), record);
    }
    let changed = found.loaded > 0 || !cache.runs.is_empty();
    if changed && project.join(".model-router/runs").is_dir() {
        let _ = storage::write_json(
            &path,
            &Cache {
                version: CACHE_VERSION,
                runs: fresh,
            },
        );
    }
    found
}

/// Aggregated outcomes of one (role, harness, model).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Metrics {
    pub role: Role,
    pub harness: String,
    pub model: String,
    pub samples: usize,
    pub clean: usize,
    pub retries: u32,
    pub failed: u32,
    pub blocked: u32,
    pub repairs_caused: u32,
    pub repair_requests: u32,
    pub first_pass: usize,
    pub judged: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median_seconds: Option<i64>,
    pub usage: Usage,
    pub checks_passed: u32,
    pub checks_total: u32,
}
impl Metrics {
    pub fn success_rate(&self) -> f64 {
        ratio(self.clean, self.samples)
    }
    pub fn mean_retries(&self) -> f64 {
        ratio(self.retries as usize, self.samples)
    }
    /// Repairs this assignment caused (implementation) or requested
    /// (review), per sample.
    pub fn repair_rate(&self) -> f64 {
        ratio(
            (self.repairs_caused + self.repair_requests) as usize,
            self.samples,
        )
    }
    pub fn first_pass_rate(&self) -> Option<f64> {
        (self.judged > 0).then(|| ratio(self.first_pass, self.judged))
    }
    pub fn check_pass_rate(&self) -> Option<f64> {
        (self.checks_total > 0)
            .then(|| ratio(self.checks_passed as usize, self.checks_total as usize))
    }
    /// "claude/opus", or "claude/default".
    pub fn assignment(&self) -> String {
        assignment(&self.harness, &self.model)
    }
}
fn ratio(a: usize, b: usize) -> f64 {
    if b == 0 { 0.0 } else { a as f64 / b as f64 }
}
fn assignment(harness: &str, model: &str) -> String {
    format!(
        "{harness}/{}",
        if model.is_empty() { "default" } else { model }
    )
}

/// Metrics across one or more projects.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub projects: Vec<PathBuf>,
    pub runs: usize,
    pub damaged: usize,
    /// Ranked: by role, then clean-success rate, then sample count.
    pub metrics: Vec<Metrics>,
}

impl Stats {
    /// Stats of each project's saved runs.
    pub fn collect(projects: &[PathBuf]) -> Self {
        let mut stats = Self::default();
        let mut all = vec![];
        for project in projects {
            let found = collect(project);
            stats.runs += found.runs;
            stats.damaged += found.damaged;
            all.extend(found.samples);
            stats.projects.push(project.clone());
        }
        stats.metrics = aggregate(&all);
        stats
    }

    pub fn find(&self, role: Role, harness: &str, model: &str) -> Option<&Metrics> {
        self.metrics
            .iter()
            .find(|m| m.role == role && m.harness == harness && m.model == model)
    }

    /// A ranked table per role, or only `role`'s.
    pub fn table(&self, role: Option<Role>) -> String {
        let mut text = format!(
            "{} run(s) in {} project(s){}\n",
            self.runs,
            self.projects.len(),
            if self.damaged > 0 {
                format!("; {} damaged record(s) skipped", self.damaged)
            } else {
                String::new()
            }
        );
        let mut small = false;
        let mut any = false;
        for current in Role::ALL {
            if role.is_some_and(|r| r != current) {
                continue;
            }
            let rows: Vec<_> = self.metrics.iter().filter(|m| m.role == current).collect();
            if rows.is_empty() {
                continue;
            }
            any = true;
            let width = rows
                .iter()
                .map(|m| m.assignment().chars().count() + 2)
                .max()
                .unwrap_or(0)
                .max(10);
            text.push_str(&format!(
                "\n{}\n  {:<width$} {:>5} {:>9} {:>7} {:>7} {:>6} {:>5} {:>5} {:>8} {:>8} {:>8} {:>6}\n",
                current.label().to_uppercase(),
                "ASSIGNMENT",
                "N",
                "SUCCESS",
                "RETRY",
                "REPAIR",
                "1ST",
                "FAIL",
                "BLOCK",
                "MEDIAN",
                "TOKENS",
                "COST",
                "CHECKS",
            ));
            for m in rows {
                let flag = if m.samples < MIN_SAMPLES {
                    small = true;
                    " *"
                } else {
                    ""
                };
                let tokens = match (m.usage.input_tokens, m.usage.output_tokens) {
                    (None, None) => "-".into(),
                    (a, b) => report::tokens(a.unwrap_or(0) + b.unwrap_or(0)),
                };
                text.push_str(&format!(
                    "  {:<width$} {:>5} {:>9} {:>7.2} {:>7} {:>6} {:>5} {:>5} {:>8} {:>8} {:>8} {:>6}\n",
                    format!("{}{flag}", m.assignment()),
                    m.samples,
                    format!("{}/{} {}", m.clean, m.samples, percent(m.success_rate())),
                    m.mean_retries(),
                    m.repairs_caused + m.repair_requests,
                    m.first_pass_rate().map_or("-".into(), percent),
                    m.failed,
                    m.blocked,
                    m.median_seconds.map_or("-".into(), report::duration),
                    tokens,
                    m.usage.cost_usd.map_or("-".into(), report::dollars),
                    m.check_pass_rate().map_or("-".into(), percent),
                ));
            }
        }
        if !any {
            text.push_str("\nNo finished stages recorded yet.\n");
        }
        if small {
            text.push_str(&format!(
                "\n* fewer than {MIN_SAMPLES} samples: anecdotal, not evidence.\n"
            ));
        }
        text.push_str(
            "\nSUCCESS: completed with no failed, blocked, or cancelled attempt. REPAIR: repairs caused (implementation) or requested (review). 1ST: review passed on its first try. Suggestions only; the router never changes a stage's assignment.\n",
        );
        text
    }
}

fn percent(rate: f64) -> String {
    format!("{:.0}%", rate * 100.0)
}

/// Samples grouped and ranked: by role, then clean-success rate, then count.
pub fn aggregate(samples: &[Sample]) -> Vec<Metrics> {
    let mut groups: BTreeMap<(usize, &str, &str), (Metrics, Vec<i64>)> = BTreeMap::new();
    for s in samples {
        let role_index = Role::ALL.iter().position(|r| *r == s.role).unwrap_or(0);
        let (m, durations) = groups
            .entry((role_index, &s.harness, &s.model))
            .or_insert_with(|| {
                (
                    Metrics {
                        role: s.role,
                        harness: s.harness.clone(),
                        model: s.model.clone(),
                        ..Metrics::default()
                    },
                    vec![],
                )
            });
        m.samples += 1;
        m.clean += usize::from(s.clean);
        m.retries += s.retries;
        m.failed += s.failed;
        m.blocked += s.blocked;
        m.repairs_caused += s.repairs_caused;
        m.repair_requests += s.repair_requests;
        if let Some(pass) = s.first_pass {
            m.judged += 1;
            m.first_pass += usize::from(pass);
        }
        if let Some(seconds) = s.seconds {
            durations.push(seconds);
        }
        if let Some(usage) = s.usage {
            m.usage = m.usage + usage;
        }
        m.checks_passed += s.checks_passed;
        m.checks_total += s.checks_total;
    }
    let mut metrics: Vec<_> = groups
        .into_iter()
        .map(|((role, _, _), (mut m, mut durations))| {
            durations.sort_unstable();
            m.median_seconds = median(&durations);
            (role, m)
        })
        .collect();
    metrics.sort_by(|(ra, a), (rb, b)| {
        ra.cmp(rb)
            .then(b.success_rate().total_cmp(&a.success_rate()))
            .then(b.samples.cmp(&a.samples))
            .then(a.assignment().cmp(&b.assignment()))
    });
    metrics.into_iter().map(|(_, m)| m).collect()
}

/// The median of sorted values; the mean of the middle two for an even count.
pub fn median(sorted: &[i64]) -> Option<i64> {
    match sorted.len() {
        0 => None,
        n if n % 2 == 1 => Some(sorted[n / 2]),
        n => Some((sorted[n / 2 - 1] + sorted[n / 2]) / 2),
    }
}

/// A better-performing assignment for a role, as history shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Suggestion {
    pub harness: String,
    pub model: String,
    pub clean: usize,
    pub samples: usize,
    pub current_clean: usize,
    pub current_samples: usize,
}
impl Suggestion {
    /// "History: claude/opus succeeded 9/10 here vs 4/8".
    pub fn text(&self) -> String {
        format!(
            "History: {} succeeded {}/{} here vs {}/{}",
            assignment(&self.harness, &self.model),
            self.clean,
            self.samples,
            self.current_clean,
            self.current_samples
        )
    }
}

/// A clearly better assignment for `role` than `harness`/`model`, or None.
/// Conservative: both need `MIN_SAMPLES` samples, the suggestion a clean
/// success rate `MIN_GAIN` higher, and no more repairs or retries per sample.
pub fn suggest(stats: &Stats, role: Role, harness: &str, model: &str) -> Option<Suggestion> {
    let current = stats.find(role, harness, model)?;
    if current.samples < MIN_SAMPLES {
        return None;
    }
    stats
        .metrics
        .iter()
        .filter(|m| m.role == role && !(m.harness == harness && m.model == model))
        .filter(|m| {
            m.samples >= MIN_SAMPLES
                && m.success_rate() >= current.success_rate() + MIN_GAIN
                && m.repair_rate() <= current.repair_rate()
                && m.mean_retries() <= current.mean_retries()
        })
        // Ranked best first already.
        .map(|m| Suggestion {
            harness: m.harness.clone(),
            model: m.model.clone(),
            clean: m.clean,
            samples: m.samples,
            current_clean: current.clean,
            current_samples: current.samples,
        })
        .next()
}

/// The project, and with `all` every recent project, without duplicates.
pub fn projects(project: &Path, all: bool) -> Vec<PathBuf> {
    let mut list = vec![project.to_path_buf()];
    if all {
        for other in storage::recent_projects() {
            let same = std::fs::canonicalize(&other).ok() == std::fs::canonicalize(project).ok();
            if !same && !list.contains(&other) {
                list.push(other);
            }
        }
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(harness: &str, model: &str, clean: bool, repairs: u32) -> Sample {
        Sample {
            run: "r".into(),
            stage: "implement".into(),
            role: Role::Implementation,
            harness: harness.into(),
            model: model.into(),
            attempts: 1,
            clean,
            retries: 0,
            failed: u32::from(!clean),
            blocked: 0,
            repairs_caused: repairs,
            repair_requests: 0,
            first_pass: Some(repairs == 0),
            seconds: Some(10),
            usage: None,
            checks_passed: 0,
            checks_total: 0,
        }
    }
    fn stats(samples: &[Sample]) -> Stats {
        Stats {
            metrics: aggregate(samples),
            ..Stats::default()
        }
    }
    fn many(harness: &str, model: &str, clean: usize, total: usize, repairs: u32) -> Vec<Sample> {
        (0..total)
            .map(|i| sample(harness, model, i < clean, repairs))
            .collect()
    }

    #[test]
    fn suggests_only_clearly_better_well_sampled_assignments() {
        let mut all = many("claude", "opus", 9, 10, 0);
        all.extend(many("codex", "gpt", 4, 8, 0));
        let s = stats(&all);
        let found = suggest(&s, Role::Implementation, "codex", "gpt").unwrap();
        assert_eq!(
            found.text(),
            "History: claude/opus succeeded 9/10 here vs 4/8"
        );
        // Never suggests the worse one, or anything for the best.
        assert_eq!(suggest(&s, Role::Implementation, "claude", "opus"), None);
        // Other roles have no history.
        assert_eq!(suggest(&s, Role::Review, "codex", "gpt"), None);
    }

    #[test]
    fn small_samples_close_rates_or_more_repairs_suggest_nothing() {
        // Candidate under MIN_SAMPLES.
        let mut all = many("claude", "opus", 4, 4, 0);
        all.extend(many("codex", "gpt", 1, 8, 0));
        assert_eq!(
            suggest(&stats(&all), Role::Implementation, "codex", "gpt"),
            None
        );
        // Current under MIN_SAMPLES.
        let mut all = many("claude", "opus", 9, 10, 0);
        all.extend(many("codex", "gpt", 0, 3, 0));
        assert_eq!(
            suggest(&stats(&all), Role::Implementation, "codex", "gpt"),
            None
        );
        // Gain under MIN_GAIN: 8/10 vs 7/10.
        let mut all = many("claude", "opus", 8, 10, 0);
        all.extend(many("codex", "gpt", 7, 10, 0));
        assert_eq!(
            suggest(&stats(&all), Role::Implementation, "codex", "gpt"),
            None
        );
        // Better success, but more repairs.
        let mut all = many("claude", "opus", 10, 10, 1);
        all.extend(many("codex", "gpt", 5, 10, 0));
        assert_eq!(
            suggest(&stats(&all), Role::Implementation, "codex", "gpt"),
            None
        );
    }

    #[test]
    fn aggregate_ranks_and_takes_median_duration() {
        let mut all = many("a", "x", 1, 2, 0);
        all[0].seconds = Some(5);
        all[1].seconds = Some(15);
        all.extend(many("b", "", 3, 3, 0));
        let metrics = aggregate(&all);
        assert_eq!(metrics[0].assignment(), "b/default");
        assert_eq!(metrics[1].median_seconds, Some(10));
        assert_eq!(metrics[1].first_pass_rate(), Some(1.0));
        let table = stats(&all).table(None);
        assert!(table.contains("IMPLEMENTATION"), "{table}");
        assert!(table.contains("a/x *"), "{table}");
        assert!(table.contains("fewer than 5 samples"), "{table}");
    }
}
