use crate::providers::{self, Provider};
use crate::{
    adapters::{self, ContextPaths, Invocation},
    checks::{self, CheckResult},
    config::{CustomAdapter, Mode, Role, Settings, Workflow},
    events,
    handoff::{self, Handoff, HandoffSource, ProtocolError, TestStatus, Verdict},
    process, snapshot, storage,
    transitions::{self, Actor, Level, Target},
};
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Running,
    Interactive,
    AwaitingConfirmation,
    Complete,
    Failed,
    Blocked,
    Cancelled,
    Skipped,
}
impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Interactive => "interactive",
            Self::AwaitingConfirmation => "inspect handoff",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
            Self::Skipped => "disabled",
        }
    }
    /// The saved name, as in run.json.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Interactive => "interactive",
            Self::AwaitingConfirmation => "awaiting_confirmation",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }
    fn stopped(self) -> bool {
        matches!(self, Self::Failed | Self::Blocked | Self::Cancelled)
    }
}

/// Whether a run, stage, or attempt may move from `from` to `to`. One status
/// type serves all three, so each level admits only its own values: a run is
/// never skipped, a stage never waits for a terminal, and an attempt only runs
/// and ends. `enabled` is the stage's switch; disabled stages are skipped.
fn legal(level: Level, from: Status, to: Status, enabled: bool) -> bool {
    use Status::*;
    match level {
        Level::Run => match from {
            Pending => matches!(
                to,
                Running | Interactive | Complete | Failed | Blocked | Cancelled
            ),
            Running => matches!(
                to,
                Pending | AwaitingConfirmation | Failed | Blocked | Cancelled
            ),
            Interactive => matches!(to, Running | Pending | Failed | Cancelled),
            AwaitingConfirmation => matches!(to, Pending | Interactive | Failed | Blocked),
            Failed | Blocked | Cancelled => matches!(to, Pending | Interactive),
            Complete | Skipped => false,
        },
        Level::Stage => match (from, to) {
            (_, Interactive) => false,
            (_, Skipped) => !enabled,
            (_, Pending) => enabled,
            (Pending, Running | Failed | Blocked | Cancelled) => true,
            (Running, Complete | AwaitingConfirmation | Failed | Blocked | Cancelled) => true,
            (AwaitingConfirmation, Complete | Failed | Blocked) => true,
            _ => false,
        },
        Level::Attempt => match (from, to) {
            (Pending, Running) => true,
            (Running, Complete | Failed | Blocked | Cancelled) => true,
            // A checkpoint or bookkeeping stops a finished attempt.
            (Complete, Failed | Blocked) => true,
            // `allow_repair`: the review completed; only the limit held it.
            (Blocked, Complete) => true,
            // Saved by versions that left interactive attempts waiting.
            (AwaitingConfirmation, Complete | Failed | Blocked) => true,
            _ => false,
        },
    }
}

/// One status change of the run, a stage, or a stage's latest attempt.
#[derive(Debug, Clone)]
pub struct Transition {
    pub target: Target,
    pub to: Status,
    pub actor: Actor,
    pub reason: String,
}
impl Transition {
    pub fn new(target: Target, to: Status, actor: Actor, reason: impl Into<String>) -> Self {
        Self {
            target,
            to,
            actor,
            reason: reason.into(),
        }
    }
}

/// A recovery or checkpoint command, applied by `command` under the run lock.
#[derive(Debug, Clone)]
pub enum Cmd {
    /// Rerun the current stage, optionally interactively or with another model.
    Retry {
        interactive: bool,
        model: Option<String>,
    },
    /// Reopen the stopped stage's session interactively.
    TakeOver,
    /// Accept the handoff at the checkpoint.
    Confirm,
    /// One more review repair pass beyond the limit.
    AllowRepair,
    /// Replace the summary of the handoff awaiting confirmation.
    EditSummary(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attempt {
    pub number: usize,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub model: String,
    pub mode: Mode,
    pub status: Status,
    pub exit_code: Option<i32>,
    pub directory: PathBuf,
    pub handoff_file: PathBuf,
    pub error: Option<String>,
    /// Router-executed check results; empty when the stage has no checks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<CheckResult>,
    /// Workspace Git trees before the harness starts and after its checks;
    /// None outside Git or when Git failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_after: Option<String>,
    /// Tokens and cost the harness reported; None when it reported none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<handoff::Usage>,
    /// Harness session the router assigned at launch, so the stage can be
    /// taken over interactively. Codex reports its own in stdout.log instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The unattended or interactive harness's process group while it runs.
    /// Cleared when the harness is reaped; still set after mr itself died,
    /// so the next `drive` can stop the orphaned group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_group: Option<u32>,
    /// Where the accepted handoff came from; None before it was read and in
    /// runs saved before this was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_source: Option<HandoffSource>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageState {
    pub status: Status,
    pub attempts: Vec<Attempt>,
    pub handoff: Option<Handoff>,
    /// User acceptance of a test stage that reported no applicable check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiver: Option<Waiver>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Waiver {
    pub reason: String,
    pub accepted_at: DateTime<Utc>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub version: u32,
    pub revision: u64,
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub project: PathBuf,
    pub workspace: PathBuf,
    /// Workspace Git tree when the run was created; None outside Git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<String>,
    pub directory: PathBuf,
    pub worktree: bool,
    pub task: String,
    /// The saved run this one reran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerun_of: Option<String>,
    pub workflow: Workflow,
    pub adapters: BTreeMap<String, CustomAdapter>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, Provider>,
    pub stages: Vec<StageState>,
    pub cursor: usize,
    pub repair_passes: u32,
    /// Repair passes the user allowed beyond the workflow's limit.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub extra_repair_passes: u32,
    pub repair_feedback: Option<String>,
    /// Index of the review whose findings `repair_feedback` holds. Stages up
    /// to and including it see the feedback; it is cleared once that review
    /// passes. None in runs saved before this was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_review: Option<usize>,
    pub status: Status,
    pub error: Option<String>,
    /// The next interactive attempt resumes this harness session instead of
    /// starting a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<Resume>,
    /// Status changes not yet saved; `persist` appends them to transitions.jsonl.
    #[serde(skip)]
    journal: Vec<transitions::Record>,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// An unattended attempt's session, reopened in the native interactive CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resume {
    pub session: String,
    /// The handoff path the session's original prompt named.
    pub handoff_file: PathBuf,
}

/// Where a new run works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Project,
    /// A new Git worktree from committed HEAD; refuses uncommitted changes.
    Worktree,
    /// A worktree from HEAD although the project has uncommitted changes,
    /// e.g. another run's edits in progress. Those changes are not included.
    WorktreeFromHead,
}

#[derive(Debug, Clone)]
pub enum Event {
    State(Box<Run>),
    Output(String),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Complete,
    Interactive,
    Confirm,
    Stopped,
}

/// What a finished stage's handoff and checks lead to.
enum Decision {
    Block(String),
    /// A test stage without evidence waits for the user to accept its reason.
    AwaitWaiver,
    Repair {
        target: usize,
        feedback: String,
    },
    Advance,
}

impl Run {
    pub fn create(
        project: &Path,
        settings: &Settings,
        workflow: &Workflow,
        task: String,
        worktree: bool,
    ) -> Result<Self> {
        let placement = if worktree {
            Placement::Worktree
        } else {
            Placement::Project
        };
        Self::create_at(project, settings, workflow, task, placement)
    }

    pub fn create_at(
        project: &Path,
        settings: &Settings,
        workflow: &Workflow,
        task: String,
        placement: Placement,
    ) -> Result<Self> {
        crate::config::validate_task(&task)?;
        let snapshot = Settings {
            workflows: vec![workflow.clone()],
            ..settings.clone()
        };
        snapshot.validate()?;
        ensure!(
            workflow.stages.iter().any(|s| s.enabled),
            "Enable at least one stage"
        );
        // Catch every missing selected harness before any stage mutates the project.
        for stage in workflow.stages.iter().filter(|s| s.enabled) {
            let program = adapters::program(&stage.harness, &settings.adapters);
            ensure!(
                adapters::executable_at(&program, project).is_some(),
                "{} is not installed or executable: {}",
                stage.harness,
                program
            );
            if let Some(provider) =
                providers::route(&stage.harness, &stage.provider, &settings.providers)
            {
                ensure!(
                    provider.api_key_env.is_empty()
                        || std::env::var_os(&provider.api_key_env).is_some_and(|v| !v.is_empty()),
                    "Set {} to use provider {} in stage {}",
                    provider.api_key_env,
                    stage.provider,
                    stage.name
                );
            }
            if stage.mode == Mode::Interactive {
                if let Some(custom) = settings.adapters.get(&stage.harness) {
                    ensure!(
                        custom.interactive_args.is_some(),
                        "{} has no interactive_args",
                        stage.harness
                    );
                }
            }
        }
        let project = fs::canonicalize(project).context("Project directory does not exist")?;
        ensure!(project.is_dir(), "Project must be a directory");
        let id = format!(
            "{}-{}",
            Utc::now().format("%Y%m%d-%H%M%S"),
            &uuid::Uuid::new_v4().to_string()[..8]
        );
        let worktree = placement != Placement::Project;
        let workspace = if worktree {
            storage::create_worktree(&project, &id, placement == Placement::Worktree)?
        } else {
            project.clone()
        };
        let directory = storage::metadata_dir(&project)?.join("runs").join(&id);
        fs::create_dir_all(&directory)?;
        let baseline = snapshot::tree(&workspace, &directory);
        let now = Utc::now();
        let mut run = Self {
            version: 1,
            revision: 0,
            id,
            created_at: now,
            updated_at: now,
            project,
            workspace,
            baseline,
            directory,
            worktree,
            task,
            rerun_of: None,
            workflow: workflow.clone(),
            adapters: settings.adapters.clone(),
            providers: settings.providers.clone(),
            stages: workflow
                .stages
                .iter()
                .map(|s| StageState {
                    status: if s.enabled {
                        Status::Pending
                    } else {
                        Status::Skipped
                    },
                    attempts: vec![],
                    handoff: None,
                    waiver: None,
                })
                .collect(),
            cursor: 0,
            repair_passes: 0,
            extra_repair_passes: 0,
            repair_feedback: None,
            repair_review: None,
            status: Status::Pending,
            error: None,
            resume: None,
            journal: vec![],
        };
        run.persist()?;
        Ok(run)
    }

    /// Saves run.json, then the summary that history listing reads instead.
    /// The summary records run.json's revision and size, so listing can tell
    /// when the two disagree.
    pub fn persist(&mut self) -> Result<()> {
        self.revision += 1;
        self.updated_at = Utc::now();
        let record = serde_json::to_vec_pretty(&*self)?;
        storage::atomic_write(&self.directory.join("run.json"), &record)?;
        let summary = crate::history::RunSummary {
            run_bytes: Some(record.len() as u64),
            ..crate::history::RunSummary::from(&*self)
        };
        storage::write_json(&self.directory.join("summary.json"), &summary)?;
        let mut journal = std::mem::take(&mut self.journal);
        for record in &mut journal {
            record.run_revision = self.revision;
        }
        transitions::append(&self.directory, &journal)
    }

    /// The only place run, stage, and attempt statuses change. Refuses moves
    /// `legal` does not allow; records the others for transitions.jsonl.
    /// Moving to the current status is a no-op and is not recorded.
    pub fn transition(&mut self, change: Transition) -> Result<()> {
        let Transition {
            target,
            to,
            actor,
            reason,
        } = change;
        let (level, stage_index, enabled) = match target {
            Target::Run => (Level::Run, None, true),
            Target::Stage(i) | Target::Attempt(i) => {
                let stage = self
                    .workflow
                    .stages
                    .get(i)
                    .with_context(|| format!("No stage {}", i + 1))?;
                let level = if matches!(target, Target::Stage(_)) {
                    Level::Stage
                } else {
                    Level::Attempt
                };
                (level, Some(i), stage.enabled)
            }
        };
        let (slot, attempt) = match target {
            Target::Run => (&mut self.status, None),
            Target::Stage(i) => (&mut self.stages[i].status, None),
            Target::Attempt(i) => {
                let attempt = self.stages[i]
                    .attempts
                    .last_mut()
                    .with_context(|| format!("Stage {} has no attempt", i + 1))?;
                (&mut attempt.status, Some(attempt.number))
            }
        };
        let from = *slot;
        if from == to {
            return Ok(());
        }
        ensure!(
            legal(level, from, to, enabled),
            "Illegal {} transition{}: {} → {} ({reason})",
            level.label(),
            stage_index.map_or_else(String::new, |i| format!(" in stage {}", i + 1)),
            from.as_str(),
            to.as_str()
        );
        *slot = to;
        self.journal.push(transitions::Record {
            at: Utc::now(),
            run_revision: self.revision,
            level,
            stage_index,
            attempt,
            from,
            to,
            reason,
            actor,
        });
        Ok(())
    }

    fn set(
        &mut self,
        target: Target,
        to: Status,
        actor: Actor,
        reason: impl Into<String>,
    ) -> Result<()> {
        self.transition(Transition::new(target, to, actor, reason))
    }

    /// Applies `cmd` here and saves it, under the run lock; see `command`.
    /// On error this run is left as it was.
    pub fn command(&mut self, cmd: Cmd) -> Result<()> {
        *self = command(&self.project, &self.id, Some(self.revision), cmd)?;
        Ok(())
    }
    pub fn current_name(&self) -> &str {
        self.workflow
            .stages
            .get(self.cursor)
            .map_or("Finished", |s| s.name.as_str())
    }

    fn check_revision(&self) -> Result<()> {
        let saved: Self = storage::read_json(&self.directory.join("run.json"))?;
        ensure!(
            saved.revision == self.revision,
            "This run changed in another process; reload it before continuing"
        );
        Ok(())
    }

    /// Explicit retry may preserve filesystem effects of the previous attempt.
    /// Changes this run in memory only; `command` applies it under the lock.
    pub fn retry(&mut self, interactive: bool, model: Option<String>) -> Result<()> {
        ensure!(
            self.cursor < self.stages.len(),
            "This run is already complete"
        );
        let index = self.cursor;
        if self.stages[index]
            .attempts
            .last()
            .is_some_and(|a| a.status == Status::Running)
        {
            let message = "Previous execution was interrupted; retry was explicitly requested";
            self.set(
                Target::Attempt(index),
                Status::Failed,
                Actor::Recovery,
                message,
            )?;
            let attempt = self.stages[index]
                .attempts
                .last_mut()
                .context("No attempt")?;
            attempt.finished_at = Some(Utc::now());
            attempt.error = Some(message.into());
        }
        if interactive {
            self.workflow.stages[index].mode = Mode::Interactive;
        }
        let mut reason = String::from("Retry");
        if let Some(model) = model {
            reason = format!("Retry with model {model}");
            self.workflow.stages[index].model = model;
        }
        if interactive {
            reason.push_str(" interactively");
        }
        self.reset(index, Actor::User, &reason)?;
        self.error = None;
        self.resume = None;
        self.set(Target::Run, Status::Pending, Actor::User, reason)
    }

    /// Reopens the current stage's last session in the harness's native
    /// interactive CLI: an unattended attempt, or an interactive one awaiting
    /// review. The stage must have stopped; filesystem changes are kept, as
    /// with retry. In memory only; `command` applies it under the lock.
    pub fn take_over(&mut self) -> Result<()> {
        ensure!(
            self.cursor < self.stages.len(),
            "This run is already complete"
        );
        ensure!(
            self.status.stopped() || self.status == Status::AwaitingConfirmation,
            "Stop the stage before taking it over"
        );
        let stage = &self.workflow.stages[self.cursor];
        ensure!(
            adapters::resumable(&stage.harness, &self.adapters),
            "{} sessions cannot be resumed; retry interactively with [i] instead",
            stage.harness
        );
        let attempt = self.stages[self.cursor]
            .attempts
            .last()
            .context("This stage has not run yet")?;
        let session = attempt
            .session
            .clone()
            .or_else(|| {
                let head = read_head(&attempt.directory.join("stdout.log"), 256 * 1024).ok()?;
                handoff::session_id(&head)
            })
            .context("The harness did not report a session for this attempt; retry interactively with [i] instead")?;
        set_aside_handoff(&attempt.handoff_file, &attempt.directory)?;
        let reason = format!("Take over session {session}");
        self.resume = Some(Resume {
            session,
            handoff_file: attempt.handoff_file.clone(),
        });
        self.workflow.stages[self.cursor].mode = Mode::Interactive;
        self.reset(self.cursor, Actor::User, &reason)?;
        self.error = None;
        self.set(Target::Run, Status::Interactive, Actor::User, reason)
    }

    /// Allows one more review repair pass after the workflow's limit stopped
    /// the run, then sends the review's findings to the implementation stage
    /// as an automatic repair would. `model-router resume <id> --allow-repair`.
    /// In memory only; `command` applies it under the lock.
    pub fn allow_repair(&mut self) -> Result<()> {
        ensure!(
            self.status == Status::Blocked
                && self.cursor < self.stages.len()
                && self.review_failures().is_some()
                && self.repair_passes >= self.repair_limit(),
            "Only a run stopped by its repair limit can get another repair pass"
        );
        self.extra_repair_passes += 1;
        self.error = None;
        if let Some(attempt) = self.stages[self.cursor].attempts.last_mut() {
            attempt.error = None;
        }
        self.advance(Actor::User)
    }

    fn repair_limit(&self) -> u32 {
        self.workflow
            .max_repair_passes
            .saturating_add(self.extra_repair_passes)
    }

    /// Return stages from `from` onward to pending, discarding handoffs and acceptances.
    fn reset(&mut self, from: usize, actor: Actor, reason: &str) -> Result<()> {
        for i in from..self.stages.len() {
            let to = if self.workflow.stages[i].enabled {
                Status::Pending
            } else {
                Status::Skipped
            };
            self.set(Target::Stage(i), to, actor, reason)?;
            let state = &mut self.stages[i];
            state.handoff = None;
            state.waiver = None;
        }
        Ok(())
    }

    /// Confirming a checkpoint also accepts its pending waiver; it never needs
    /// a second step. In memory only; `command` applies it under the lock.
    pub fn confirm(&mut self) -> Result<()> {
        ensure!(
            self.status == Status::AwaitingConfirmation,
            "No handoff awaits confirmation"
        );
        if let Some(reason) = self.pending_waiver().map(str::to_owned) {
            self.stages[self.cursor].waiver = Some(Waiver {
                reason,
                accepted_at: Utc::now(),
            });
        }
        self.advance(Actor::User)
    }

    /// Replaces the summary of the non-review handoff awaiting confirmation.
    fn edit_summary(&mut self, summary: String) -> Result<()> {
        ensure!(
            self.status == Status::AwaitingConfirmation
                && self.workflow.stages[self.cursor].role != Role::Review,
            "Only an interactive non-review handoff summary can be edited"
        );
        ensure!(
            !summary.trim().is_empty() && summary.len() <= 12000,
            "Enter a summary of 1–12000 bytes"
        );
        let state = &mut self.stages[self.cursor];
        let handoff = state.handoff.as_mut().context("No handoff")?;
        handoff.summary = summary;
        handoff
            .decisions
            .push("Handoff summary supplied or edited by the user".into());
        if let Some(attempt) = state.attempts.last() {
            storage::write_json(&attempt.directory.join("handoff.json"), handoff)?;
        }
        Ok(())
    }

    /// The not-applicable reason when this checkpoint awaits acceptance of a test
    /// stage without verification evidence.
    pub fn pending_waiver(&self) -> Option<&str> {
        if self.status != Status::AwaitingConfirmation
            || !matches!(self.decide(), Ok(Decision::AwaitWaiver))
        {
            return None;
        }
        self.stages[self.cursor]
            .handoff
            .as_ref()?
            .not_applicable_reason()
    }

    /// Trees bounding the changes made during this run: the baseline and the
    /// latest attempt's final snapshot, so earlier user changes are excluded.
    pub fn change_range(&self) -> std::result::Result<(&str, &str), &'static str> {
        let from = self
            .baseline
            .as_deref()
            .ok_or("no baseline snapshot was recorded")?;
        let latest = self
            .stages
            .iter()
            .flat_map(|s| &s.attempts)
            .max_by_key(|a| a.started_at);
        let to = match latest {
            Some(attempt) => attempt
                .tree_after
                .as_deref()
                .ok_or("the latest attempt has no snapshot")?,
            None => from,
        };
        Ok((from, to))
    }

    /// The current stage's failed required router checks, described.
    fn failed_checks(&self) -> Option<String> {
        let checks = self.stages[self.cursor]
            .attempts
            .last()
            .map_or(&[][..], |a| &a.checks);
        let mut failed = checks
            .iter()
            .filter(|c| c.required && c.status != TestStatus::Passed);
        let check = failed.next()?;
        let more = failed.count();
        Some(format!(
            "Required check failed: {} ({}){}. Logs: {}",
            shell_words::join(&check.command),
            check.outcome(),
            if more > 0 {
                format!("; {more} more failed")
            } else {
                String::new()
            },
            check.directory.display()
        ))
    }

    /// Why the current review stage requests a repair: its handoff, its failed
    /// required router checks, or both. None for other stages, a review that
    /// passed, or one that reports a block.
    fn review_failures(&self) -> Option<Option<String>> {
        let stage = self.workflow.stages.get(self.cursor)?;
        let handoff = self.stages[self.cursor].handoff.as_ref()?;
        if stage.role != Role::Review || !stage.enabled || handoff.verdict == Verdict::Blocked {
            return None;
        }
        let checks = self.failed_checks();
        (handoff.requires_repair() || checks.is_some()).then_some(checks)
    }

    /// What the current stage's handoff and checks lead to.
    fn decide(&self) -> Result<Decision> {
        let stage = self
            .workflow
            .stages
            .get(self.cursor)
            .context("This run is already complete")?;
        let state = &self.stages[self.cursor];
        let handoff = state.handoff.as_ref().context("Missing handoff")?;
        if handoff.verdict == Verdict::Blocked {
            return Ok(Decision::Block(format!(
                "Stage reports a block: {}",
                handoff.summary
            )));
        }
        // A failed required check in an enabled review requests a repair, as
        // its blocking findings do; anywhere else it blocks the stage.
        if let Some(checks) = self.review_failures() {
            let mut feedback = format!(
                "Review {} requested changes:\n{}",
                stage.name,
                serde_json::to_string_pretty(handoff)?
            );
            if let Some(checks) = &checks {
                feedback.push_str(&format!("\nRouter checks: {checks}"));
            }
            let note = checks.map_or_else(String::new, |c| format!(" {c}"));
            if self.repair_passes >= self.repair_limit() {
                return Ok(Decision::Block(format!(
                    "Review still requests changes after {} repair passes.{note} Inspect its findings, then allow another pass with: model-router -C {} resume {} --allow-repair",
                    self.repair_passes,
                    self.project.display(),
                    self.id
                )));
            }
            let target = self.workflow.stages[..self.cursor].iter().rposition(|s| {
                s.enabled
                    && s.role == Role::Implementation
                    && (stage.repair_target.is_empty() || s.id == stage.repair_target)
            });
            return Ok(match target {
                Some(target) => Decision::Repair { target, feedback },
                None => Decision::Block(format!(
                    "Review requested changes, but no enabled upstream implementation stage exists.{note}"
                )),
            });
        }
        if let Some(checks) = self.failed_checks() {
            return Ok(Decision::Block(checks));
        }
        if stage.role == Role::Test {
            let checks = state.attempts.last().map_or(&[][..], |a| &a.checks);
            if handoff.requires_repair()
                || handoff
                    .tests
                    .iter()
                    .any(|test| test.required && test.status == TestStatus::NotRun)
            {
                return Ok(Decision::Block("Test stage reports incomplete verification; inspect its findings and required checks before retrying".into()));
            }
            if handoff.has_passing_required_check()
                || checks
                    .iter()
                    .any(|c| c.required && c.status == TestStatus::Passed)
                || state.waiver.is_some()
            {
                return Ok(Decision::Advance);
            }
            return Ok(match handoff.not_applicable_reason() {
                Some(_) => Decision::AwaitWaiver,
                None => Decision::Block("Test stage reported no passing required check. Retry it with evidence, or have it report not_applicable with a reason, which requires your explicit acceptance".into()),
            });
        }
        if handoff.verdict == Verdict::NeedsChanges {
            return Ok(Decision::Block(
                "Stage reports incomplete work; inspect its handoff and retry".into(),
            ));
        }
        Ok(Decision::Advance)
    }

    /// Acts on the current stage's finished handoff: blocks, waits for the
    /// user's acceptance, sends a review's request to a repair, or moves on.
    fn advance(&mut self, actor: Actor) -> Result<()> {
        let index = self.cursor;
        let name = self.workflow.stages[index].name.clone();
        match self.decide()? {
            Decision::Block(message) => self.stop(Status::Blocked, message, actor, true),
            Decision::AwaitWaiver => {
                let reason = "Test stage reports not applicable; awaiting acceptance";
                self.set(Target::Attempt(index), Status::Complete, actor, reason)?;
                self.set(
                    Target::Stage(index),
                    Status::AwaitingConfirmation,
                    actor,
                    reason,
                )?;
                self.set(Target::Run, Status::AwaitingConfirmation, actor, reason)
            }
            Decision::Repair { target, feedback } => {
                let reason = format!(
                    "Review {name} requested a repair at {}",
                    self.workflow.stages[target].name
                );
                self.set(Target::Attempt(index), Status::Complete, actor, &reason)?;
                self.repair_feedback = Some(feedback);
                self.repair_review = Some(index);
                self.repair_passes += 1;
                // Invalidate all downstream handoffs, including previously passing reviews.
                self.reset(target, actor, &reason)?;
                self.cursor = target;
                self.set(Target::Run, Status::Pending, actor, reason)
            }
            Decision::Advance => {
                let reason = format!("Stage {name} complete");
                self.set(Target::Attempt(index), Status::Complete, actor, &reason)?;
                self.set(Target::Stage(index), Status::Complete, actor, &reason)?;
                // The review that asked for the repair passed: later stages, and
                // later retries, must not see its old request.
                if self.repair_review == Some(index) {
                    self.repair_feedback = None;
                    self.repair_review = None;
                }
                self.cursor += 1;
                self.set(Target::Run, Status::Pending, actor, reason)
            }
        }
    }

    /// An interactive stage's handoff waits for the user at the checkpoint.
    fn checkpoint(&mut self) -> Result<()> {
        let index = self.cursor;
        let reason = "Interactive handoff awaits inspection";
        self.set(
            Target::Attempt(index),
            Status::Complete,
            Actor::Engine,
            reason,
        )?;
        self.set(
            Target::Stage(index),
            Status::AwaitingConfirmation,
            Actor::Engine,
            reason,
        )?;
        self.set(
            Target::Run,
            Status::AwaitingConfirmation,
            Actor::Engine,
            reason,
        )
    }

    /// Stops the run at the current stage. With `attempt`, also records the
    /// error on the stage's latest attempt, which this execution created or
    /// just finished; without, the error belongs to no attempt of this
    /// execution, e.g. a launch that failed before its attempt was recorded,
    /// or a cancel between stages.
    fn stop(&mut self, status: Status, message: String, actor: Actor, attempt: bool) -> Result<()> {
        let index = self.cursor;
        let has_attempt = attempt
            && self
                .stages
                .get(index)
                .is_some_and(|s| !s.attempts.is_empty());
        if has_attempt {
            self.set(Target::Attempt(index), status, actor, &message)?;
            let attempt = self.stages[index]
                .attempts
                .last_mut()
                .context("No attempt")?;
            attempt.error = Some(message.clone());
            attempt.finished_at.get_or_insert_with(Utc::now);
        }
        if index < self.stages.len() {
            self.set(Target::Stage(index), status, actor, &message)?;
        }
        self.set(Target::Run, status, actor, &message)?;
        self.error = Some(message);
        Ok(())
    }
}

/// The resumed session only knows the handoff path its original prompt
/// named, so a takeover writes there again. Move the earlier attempt's file
/// into that attempt's directory first, so the takeover can never accept a
/// stale handoff the resumed session did not write.
fn set_aside_handoff(handoff_file: &Path, attempt_dir: &Path) -> Result<()> {
    if handoff_file.exists() {
        fs::create_dir_all(attempt_dir)?;
        fs::rename(handoff_file, attempt_dir.join("handoff-file.json")).with_context(|| {
            format!("Move the previous handoff {} aside", handoff_file.display())
        })?;
    }
    Ok(())
}

pub fn load(project: &Path, id: &str) -> Result<Run> {
    let run: Run = storage::read_json(&storage::run_dir(project, id)?.join("run.json"))?;
    ensure!(
        run.version == 1
            && run.id == id
            && run.stages.len() == run.workflow.stages.len()
            && run.cursor <= run.stages.len(),
        "Invalid or unsupported run record"
    );
    Ok(run)
}

/// Applies a recovery or checkpoint command to the saved run and saves the
/// result, holding the run lock throughout. With `revision`, refuses a run
/// that another process changed since the caller loaded it. Returns the run
/// as saved; callers replace their copy with it.
pub fn command(project: &Path, id: &str, revision: Option<u64>, cmd: Cmd) -> Result<Run> {
    let directory = storage::run_dir(project, id)?;
    let _lock = storage::lock_wait(&directory.join("run.lock"), LOCK_WAIT)?;
    let mut run = load(project, id)?;
    if let Some(revision) = revision {
        ensure!(
            run.revision == revision,
            "This run changed in another process; reload it before continuing"
        );
    }
    match cmd {
        Cmd::Retry { interactive, model } => run.retry(interactive, model)?,
        Cmd::TakeOver => run.take_over()?,
        Cmd::Confirm => run.confirm()?,
        Cmd::AllowRepair => run.allow_repair()?,
        Cmd::EditSummary(summary) => run.edit_summary(summary)?,
    }
    run.persist()?;
    Ok(run)
}

/// Every readable run, newest first. Listing should use `history::list`,
/// which reads summaries and reports damaged records.
pub fn history(project: &Path) -> Result<Vec<Run>> {
    Ok(crate::history::list(project)?
        .iter()
        .filter_map(|entry| load(project, entry.id()).ok())
        .collect())
}

pub fn prompt(run: &Run, handoff_path: &Path, schema_path: &Path) -> String {
    let stage = &run.workflow.stages[run.cursor];
    let mut text = format!(
        "You are completing stage {:?} (role: {}) in a multi-stage workflow.\n\nORIGINAL USER TASK\n{}\n\nSTAGE INSTRUCTIONS\n{}\n\nWORKING DIRECTORY\n{}\n\nFollow the project's own instructions. Do not commit, merge, or push unless the original task explicitly requests it. Do not change router configuration, run state, or other stages' artifacts.\n",
        stage.name,
        stage.role.label(),
        run.task,
        stage.instructions,
        run.workspace.display()
    );
    let mut remaining = 14000;
    for (index, state) in run.stages[..run.cursor].iter().enumerate().rev() {
        if state.status == Status::Complete {
            if let Some(handoff) = &state.handoff {
                let directory = state
                    .attempts
                    .last()
                    .map(|a| a.directory.clone())
                    .unwrap_or_default();
                let excerpt = handoff_excerpt(
                    handoff,
                    remaining.min(5000),
                    &directory.join("handoff.json"),
                );
                remaining = remaining.saturating_sub(excerpt.len());
                text.push_str(&format!(
                    "\nPRIOR STAGE: {}\n{}\nFull artifacts: {}\n",
                    run.workflow.stages[index].name,
                    excerpt,
                    directory.display()
                ));
            }
        }
    }
    // Only the repaired stages and the review that asked see its request.
    let feedback = run
        .repair_feedback
        .as_ref()
        .filter(|_| run.repair_review.is_none_or(|review| run.cursor <= review));
    if let Some(feedback) = feedback {
        text.push_str(&format!(
            "\nREPAIR REQUEST\n{}\n",
            storage::truncate(feedback, 4000)
        ));
    }
    if !stage.checks.is_empty() {
        text.push_str("\nROUTER CHECKS\nAfter you finish, the router runs these commands in the working directory and records their results independently of your handoff:\n");
        for check in &stage.checks {
            text.push_str(&format!(
                "- {} ({})\n",
                shell_words::join(&check.command),
                if check.required {
                    "required"
                } else {
                    "optional"
                }
            ));
        }
    }
    text.push_str(&handoff::contract(stage.role, handoff_path, schema_path));
    text
}

/// A handoff as pretty JSON of at most about `max` bytes that stays valid
/// JSON: long strings are shortened and long lists cut, then whole fields
/// dropped, least important first, rather than cutting the text mid-object.
fn handoff_excerpt(handoff: &Handoff, max: usize, full: &Path) -> String {
    let Ok(mut value) = serde_json::to_value(handoff) else {
        return String::new();
    };
    let render =
        |value: &serde_json::Value| serde_json::to_string_pretty(value).unwrap_or_default();
    let mut json = render(&value);
    if json.len() <= max {
        return json;
    }
    for (chars, items) in [(1000, 40), (400, 20), (160, 10), (60, 5)] {
        shorten(&mut value, chars, items);
        json = render(&value);
        if json.len() <= max {
            break;
        }
    }
    for field in [
        "next_steps",
        "artifacts",
        "files_changed",
        "decisions",
        "tests",
        "unresolved",
        "findings",
    ] {
        if json.len() <= max {
            break;
        }
        if let Some(object) = value.as_object_mut().filter(|o| o.contains_key(field)) {
            object.insert(field.into(), serde_json::Value::String("omitted".into()));
            json = render(&value);
        }
    }
    format!("{json}\n[truncated; full artifact at {}]", full.display())
}

/// Cuts every string to `chars` characters and every list to `items` items,
/// noting how many were left out.
fn shorten(value: &mut serde_json::Value, chars: usize, items: usize) {
    use serde_json::Value;
    match value {
        Value::String(text) if text.chars().count() > chars => {
            *text = format!("{}…", text.chars().take(chars).collect::<String>());
        }
        Value::Array(list) => {
            if list.len() > items {
                let more = list.len() - items;
                list.truncate(items);
                list.push(Value::String(format!("… {more} more")));
            }
            list.iter_mut().for_each(|item| shorten(item, chars, items));
        }
        Value::Object(map) => map
            .values_mut()
            .for_each(|item| shorten(item, chars, items)),
        _ => {}
    }
}

struct Prepared {
    invocation: Invocation,
    sandbox: Option<crate::sandbox::Plan>,
    directory: PathBuf,
    handoff_file: PathBuf,
}

fn prepare(run: &mut Run) -> Result<Prepared> {
    let index = run.cursor;
    let stage = &run.workflow.stages[index];
    let number = run.stages[index].attempts.len() + 1;
    let directory = run
        .directory
        .join(format!("{:02}-{}", index + 1, stage.id))
        .join(format!("attempt-{number:03}"));
    fs::create_dir_all(&directory)?;
    let handoff_dir = storage::metadata_dir(&run.workspace)?
        .join("handoffs")
        .join(&run.id);
    fs::create_dir_all(&handoff_dir)?;
    let resume = run.resume.clone();
    let handoff_file = match &resume {
        // The resumed session only knows the path its original prompt named;
        // `take_over` moved that attempt's file aside, so whatever is there
        // after this attempt was written by it.
        Some(resume) => resume.handoff_file.clone(),
        None => handoff_dir.join(format!("{}-{number:03}.json", stage.id)),
    };
    ensure!(
        !handoff_file.exists(),
        "Handoff path already exists: {}",
        handoff_file.display()
    );
    let prompt_file = directory.join("prompt.md");
    // The prompt names this copy of the handoff schema.
    let schema_file = directory.join(handoff::SCHEMA_FILE);
    storage::atomic_write(&schema_file, handoff::SCHEMA.as_bytes())?;
    let text = match &resume {
        Some(resume) => format!(
            "Interactive takeover of harness session {}. The session's own history holds the stage prompt. Write a new handoff for this attempt to {}; the previous attempt's file there was moved aside and is not read.\n",
            resume.session,
            handoff_file.display()
        ),
        None => prompt(run, &handoff_file, &schema_file),
    };
    storage::atomic_write(&prompt_file, text.as_bytes())?;
    let paths = ContextPaths {
        workspace: &run.workspace,
        attempt_dir: &directory,
        prompt_file: &prompt_file,
        handoff_file: &handoff_file,
    };
    let (invocation, session) = match &resume {
        Some(resume) => (
            adapters::resume(
                stage,
                &run.adapters,
                &run.providers,
                &resume.session,
                &paths,
            )?,
            Some(resume.session.clone()),
        ),
        None => {
            let mut invocation =
                adapters::build(stage, &run.adapters, &run.providers, &text, &paths)?;
            let session = adapters::assign_session(&mut invocation, &stage.harness);
            (invocation, session)
        }
    };
    let sandbox = crate::sandbox::plan(
        &run.workflow,
        stage,
        &crate::sandbox::Paths {
            workspace: &run.workspace,
            attempt_dir: &directory,
            handoff_dir: &handoff_dir,
            program: &invocation.program,
        },
    )?;
    storage::write_json(
        &directory.join("command.json"),
        &serde_json::json!({ "program": invocation.program, "args": invocation.args, "stdin_from": invocation.stdin.as_ref().map(|_| "prompt.md"), "env": invocation.env.iter().map(|(name, _)| name).collect::<Vec<_>>(), "workspace": run.workspace, "model": stage.model, "provider": stage.provider, "effort": stage.effort, "mode": stage.mode, "sandbox": sandbox }),
    )?;
    run.stages[index].attempts.push(Attempt {
        number,
        started_at: Utc::now(),
        finished_at: None,
        model: stage.model.clone(),
        mode: stage.mode,
        status: Status::Pending,
        exit_code: None,
        directory: directory.clone(),
        handoff_file: handoff_file.clone(),
        error: None,
        checks: vec![],
        tree_before: snapshot::tree(&run.workspace, &run.directory),
        tree_after: None,
        usage: None,
        session,
        process_group: None,
        handoff_source: None,
    });
    let reason = format!(
        "Attempt {number} started: {} {} ({})",
        stage.harness,
        stage.model,
        stage.mode.label()
    );
    run.resume = None;
    run.error = None;
    run.set(
        Target::Attempt(index),
        Status::Running,
        Actor::Engine,
        &reason,
    )?;
    run.set(
        Target::Stage(index),
        Status::Running,
        Actor::Engine,
        &reason,
    )?;
    run.set(Target::Run, Status::Running, Actor::Engine, reason)?;
    run.persist()?;
    Ok(Prepared {
        invocation,
        sandbox,
        directory,
        handoff_file,
    })
}

/// The first `max` bytes of a log, where harnesses report their session.
fn read_head(path: &Path, max: u64) -> Result<String> {
    let mut bytes = Vec::new();
    io::Read::read_to_end(&mut io::Read::take(fs::File::open(path)?, max), &mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// How a harness process ended, before its handoff is read.
enum Exit {
    /// The harness succeeded; its output, normalized.
    Succeeded(handoff::Normalized),
    /// The stage stops with this status and message.
    Stopped(Status, String),
}

/// Records the process's end on the attempt and classifies it: cancelled,
/// timed out, failed, denied permissions, a protocol error, or success.
fn classify_exit(
    run: &mut Run,
    prepared: &Prepared,
    result: Result<process::Outcome>,
) -> Result<Exit> {
    let index = run.cursor;
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            // The supervisor ended the group before returning its error.
            if let Some(attempt) = run.stages[index].attempts.last_mut() {
                attempt.process_group = None;
            }
            return Ok(Exit::Stopped(Status::Failed, format!("{error:#}")));
        }
    };
    let stdout = prepared.directory.join("stdout.log");
    if let Some(attempt) = run.stages[index].attempts.last_mut() {
        attempt.exit_code = outcome.code;
        attempt.finished_at = Some(Utc::now());
        // Spend counts whether or not the stage succeeded. Usage events can be
        // spread through the log, so read far more than the final response needs.
        attempt.process_group = None;
        if attempt.mode == Mode::Unattended {
            attempt.usage = handoff::usage_file(&stdout, prepared.invocation.kind);
        } else {
            // Best effort, from the harness's own session log.
            attempt.usage = handoff::session_usage(
                &run.workflow.stages[index].harness,
                attempt.session.as_deref(),
                &run.workspace,
                attempt.started_at,
                Utc::now(),
            );
        }
    }
    if outcome.cancelled {
        return Ok(Exit::Stopped(
            Status::Cancelled,
            "Stage cancelled. Filesystem changes are retained; retry explicitly".into(),
        ));
    }
    if outcome.timed_out {
        return Ok(Exit::Stopped(
            Status::Failed,
            "Stage timed out. Inspect logs or reopen interactively; existing changes are retained"
                .into(),
        ));
    }
    let mode = run.workflow.stages[index].mode;
    if !outcome.success() {
        // A JSON error event names the cause; stderr often holds only warnings.
        let reported = (mode == Mode::Unattended)
            .then(|| process::read_tail(&stdout, 4 * 1024 * 1024).ok())
            .flatten()
            .and_then(|raw| handoff::normalize(&raw, prepared.invocation.kind).error);
        let detail = match reported {
            Some(error) => error.to_string(),
            None => storage::clean_terminal(
                &process::read_tail(&prepared.directory.join("stderr.log"), 3000)
                    .unwrap_or_default(),
            ),
        };
        return Ok(Exit::Stopped(
            Status::Failed,
            format!("Harness exited with code {:?}. {detail}", outcome.code),
        ));
    }
    let raw = process::read_tail(&stdout, 4 * 1024 * 1024)?;
    let normalized = if mode == Mode::Interactive {
        handoff::Normalized {
            text: storage::clean_terminal(&raw),
            ..handoff::Normalized::default()
        }
    } else {
        handoff::normalize(&raw, prepared.invocation.kind)
    };
    if normalized.permission_blocked {
        return Ok(Exit::Stopped(Status::Blocked, "Harness reported denied tool permissions. Reopen interactively or set explicit permission arguments and retry".into()));
    }
    // A handoff file may replace a missing assistant text event, but not a real
    // protocol error (e.g. a tool/API failure after the file was written).
    match &normalized.error {
        Some(ProtocolError::NoFinalResponse)
            if prepared.handoff_file.is_file() || prepared.invocation.final_file.is_file() => {}
        Some(error) => return Ok(Exit::Stopped(Status::Failed, error.to_string())),
        None => {}
    }
    Ok(Exit::Succeeded(normalized))
}

/// A successful attempt's handoff, or why the stage stops without one.
enum Resolved {
    Handoff(HandoffSource, Box<Handoff>),
    Stopped(Status, String),
}

/// Reads the handoff from the first source present: the handoff file, the
/// harness's final-message file, then the output stream. An interactive
/// stage that wrote no file gets a placeholder, except reviews and tests,
/// which must produce a structured verdict.
fn resolve_handoff(
    run: &Run,
    prepared: &Prepared,
    normalized: handoff::Normalized,
) -> Result<Resolved> {
    let stage = &run.workflow.stages[run.cursor];
    let (mode, role) = (stage.mode, stage.role);
    // Interactive terminal transcripts can echo the original prompt (including
    // the example JSON). Never infer a structured verdict from an echoed prompt.
    if mode == Mode::Interactive && !prepared.handoff_file.is_file() {
        if matches!(role, Role::Review | Role::Test) {
            return Ok(Resolved::Stopped(
                Status::Blocked,
                format!(
                    "Interactive {} did not write its handoff file. Reopen it to produce a structured verdict and evidence",
                    role.label()
                ),
            ));
        }
        let handoff = Handoff {
            unresolved: vec!["No model-authored handoff was produced".into()],
            ..Handoff::unstructured(format!(
                "Interactive stage exited without a structured handoff. Inspect the terminal transcript at {}. Reopen to produce a handoff, or supply a summary before advancing.",
                prepared.directory.join("stdout.log").display()
            ))
        };
        return Ok(Resolved::Handoff(
            HandoffSource::Placeholder,
            Box::new(handoff),
        ));
    }
    let read = |path: &Path| -> Result<Option<String>> {
        if path.is_file() {
            Ok(Some(process::read_tail(path, 1024 * 1024)?))
        } else {
            Ok(None)
        }
    };
    let file = read(&prepared.handoff_file)?;
    let text = match read(&prepared.invocation.final_file)? {
        Some(text) => text,
        None => normalized.text.clone(),
    };
    let native = adapters::uses_output_schema(&prepared.invocation)
        .then(|| handoff::native_output(&normalized, prepared.invocation.kind, &text))
        .flatten();
    // An interactive transcript echoes the prompt; only its file counts.
    let sources = handoff::Sources {
        native: native.as_deref(),
        file: file.as_deref(),
        text: (mode == Mode::Unattended).then_some(text.as_str()),
    };
    Ok(match handoff::resolve(sources, role) {
        Ok((handoff, source)) => Resolved::Handoff(source, Box::new(handoff)),
        Err(error) => Resolved::Stopped(Status::Blocked, format!("{error:#}")),
    })
}

/// Completes the attempt's bookkeeping after its harness ended.
fn finish(
    run: &mut Run,
    prepared: &Prepared,
    result: Result<process::Outcome>,
    cancel: &AtomicBool,
    emit: &mut impl FnMut(Event),
) -> Result<()> {
    let index = run.cursor;
    let normalized = match classify_exit(run, prepared, result)? {
        Exit::Succeeded(normalized) => normalized,
        Exit::Stopped(status, message) => return run.stop(status, message, Actor::Engine, true),
    };
    let (source, handoff) = match resolve_handoff(run, prepared, normalized)? {
        Resolved::Handoff(source, handoff) => (source, *handoff),
        Resolved::Stopped(status, message) => {
            return run.stop(status, message, Actor::Engine, true);
        }
    };
    if let Some(attempt) = run.stages[index].attempts.last_mut() {
        attempt.handoff_source = Some(source);
    }
    storage::write_json(&prepared.directory.join("handoff.json"), &handoff)?;
    if run.workflow.stages[index].role == Role::Summary && source != HandoffSource::Placeholder {
        // The brief is for a person: keep it readable outside the router.
        storage::atomic_write(
            &run.directory.join("review.md"),
            format!("{}\n", handoff.summary.trim_end()).as_bytes(),
        )?;
    }
    // A reported block stops the stage regardless of check results.
    let blocked = handoff.verdict == Verdict::Blocked;
    run.stages[index].handoff = Some(handoff);
    if !blocked && !run_checks(run, prepared, cancel, emit)? {
        return Ok(());
    }
    if run.workflow.stages[index].mode == Mode::Interactive {
        run.checkpoint()
    } else {
        run.advance(Actor::Engine)
    }
}

/// Runs the current stage's router checks. False when cancellation stopped the run.
fn run_checks(
    run: &mut Run,
    prepared: &Prepared,
    cancel: &AtomicBool,
    emit: &mut impl FnMut(Event),
) -> Result<bool> {
    let index = run.cursor;
    let configured = run.workflow.stages[index].checks.clone();
    if configured.is_empty() {
        return Ok(true);
    }
    let (results, cancelled) = checks::run(
        &configured,
        &run.workspace,
        &prepared.directory,
        cancel,
        |text| emit(Event::Output(text)),
    )?;
    if let Some(attempt) = run.stages[index].attempts.last_mut() {
        attempt.checks = results;
    }
    if cancelled {
        run.stop(
            Status::Cancelled,
            "Stage cancelled during checks. Filesystem changes are retained; retry explicitly"
                .into(),
            Actor::Engine,
            true,
        )?;
    }
    Ok(!cancelled)
}

fn execute_stage(
    run: &mut Run,
    cancel: &AtomicBool,
    interactive: Option<&process::InteractiveSession>,
    emit: &mut impl FnMut(Event),
) -> Result<()> {
    let index = run.cursor;
    let attempts = run.stages[index].attempts.len();
    let prepared = match prepare(run) {
        Ok(p) => p,
        Err(e) => {
            // Errors before the attempt was recorded belong to the stage,
            // never to an earlier, finished attempt.
            let recorded = run.stages[index].attempts.len() > attempts;
            run.stop(Status::Failed, format!("{e:#}"), Actor::Engine, recorded)?;
            run.persist()?;
            emit(Event::State(Box::new(run.clone())));
            return Ok(());
        }
    };
    emit(Event::State(Box::new(run.clone())));
    let stage = &run.workflow.stages[index];
    emit(Event::Output(format!(
        "── {} · attempt {} · {}{}{} ──\n",
        stage.name,
        run.stages[index].attempts.len(),
        stage.harness,
        if stage.model.is_empty() { "" } else { " " },
        stage.model
    )));
    let workspace = run.workspace.clone();
    // Recorded so a later drive can stop the group if mr dies meanwhile.
    let record = |run: &mut Run, pgid: u32| {
        if let Some(attempt) = run.stages[index].attempts.last_mut() {
            attempt.process_group = Some(pgid);
        }
        let _ = run.persist();
    };
    let result = if let Some(session) = interactive {
        process::interactive(
            &prepared.invocation,
            &workspace,
            &prepared.directory,
            cancel,
            session,
            prepared.sandbox.as_ref(),
            |pgid| record(run, pgid),
        )
    } else {
        // Events become readable lines; stdout.log keeps them raw.
        let mut formatter = events::Formatter::new(prepared.invocation.kind);
        let timeout = Duration::from_secs(run.workflow.stages[index].timeout_seconds);
        let result = process::capture_with(
            &prepared.invocation,
            &workspace,
            &prepared.directory,
            timeout,
            cancel,
            prepared.sandbox.as_ref(),
            |pgid| record(run, pgid),
            |err, text| {
                if err {
                    emit(Event::Output(format!(
                        "[stderr] {}",
                        storage::clean_terminal(text)
                    )));
                } else if let Some(lines) = formatter.push(text) {
                    emit(Event::Output(lines));
                }
            },
        );
        if let Some(rest) = formatter.finish() {
            emit(Event::Output(rest));
        }
        result
    };
    if let Err(error) = finish(run, &prepared, result, cancel, emit) {
        run.stop(
            Status::Failed,
            format!("Could not complete stage bookkeeping: {error:#}"),
            Actor::Engine,
            true,
        )?;
    }
    // Finishing may move the cursor, so record the stage that ran.
    let tree = snapshot::tree(&run.workspace, &run.directory);
    if let Some(attempt) = run.stages[index].attempts.last_mut() {
        attempt.tree_after = tree;
    }
    run.persist()?;
    emit(Event::State(Box::new(run.clone())));
    Ok(())
}

/// Saves the patch of this run's changes to changes.diff; best effort, like
/// the snapshots it compares.
fn save_changes(run: &Run) {
    if let Ok((from, to)) = run.change_range() {
        if let Ok(patch) = snapshot::patch(&run.workspace, from, to, snapshot::PATCH_LIMIT) {
            let _ = storage::atomic_write(&run.directory.join("changes.diff"), patch.as_bytes());
        }
    }
}

/// How long `drive` waits for the run and workspace locks. Short probes
/// (history cleanup, the TUI's busy-checkout check) hold them briefly.
const LOCK_WAIT: Duration = Duration::from_secs(2);

fn lock_run(run: &Run) -> Result<(storage::FileLock, storage::FileLock)> {
    let run_lock = storage::lock_wait(&run.directory.join("run.lock"), LOCK_WAIT)?;
    let workspace_lock = storage::lock_wait(
        &storage::metadata_dir(&run.workspace)?.join("workspace.lock"),
        LOCK_WAIT,
    )?;
    Ok((run_lock, workspace_lock))
}

/// Stops harness process groups left by an mr process that died while they
/// ran (no PR_SET_PDEATHSIG outside Linux, and background children survive
/// it everywhere), so a retry never runs two agents on one workspace. Holding
/// both locks means no live router owns them. Process group IDs are not
/// reused while any member lives; a stale ID whose group is gone is ignored.
fn stop_orphans(run: &mut Run, emit: &mut impl FnMut(Event)) -> Result<()> {
    let mut changed = false;
    for state in &mut run.stages {
        for attempt in &mut state.attempts {
            let Some(pgid) = attempt.process_group.take() else {
                continue;
            };
            changed = true;
            if process::group_alive(pgid) {
                emit(Event::Output(format!(
                    "Stopping process group {pgid} left running by an interrupted execution\n"
                )));
                process::end_group(pgid, process::GRACE, || {});
            }
        }
    }
    if changed {
        run.persist()?;
    }
    Ok(())
}

/// Drive until completion, failure, or a terminal/confirmation is required.
pub fn drive(run: &mut Run, cancel: &AtomicBool, mut emit: impl FnMut(Event)) -> Result<Action> {
    let (_lock, _workspace_lock) = lock_run(run)?;
    run.check_revision()?;
    stop_orphans(run, &mut emit)?;
    loop {
        match run.status {
            Status::AwaitingConfirmation => return Ok(Action::Confirm),
            Status::Failed | Status::Blocked | Status::Cancelled => {
                save_changes(run);
                run.persist()?;
                return Ok(Action::Stopped);
            }
            Status::Running => {
                // Only an attempt still marked running belongs to that execution.
                let running = run
                    .stages
                    .get(run.cursor)
                    .and_then(|s| s.attempts.last())
                    .is_some_and(|a| a.status == Status::Running);
                run.stop(
                    Status::Failed,
                    "A previous execution was interrupted. Inspect artifacts and retry explicitly"
                        .into(),
                    Actor::Recovery,
                    running,
                )?;
                run.persist()?;
                return Ok(Action::Stopped);
            }
            _ => {}
        }
        while run.cursor < run.workflow.stages.len() && !run.workflow.stages[run.cursor].enabled {
            run.set(
                Target::Stage(run.cursor),
                Status::Skipped,
                Actor::Engine,
                "Stage disabled",
            )?;
            run.cursor += 1;
        }
        if run.cursor == run.stages.len() {
            run.set(
                Target::Run,
                Status::Complete,
                Actor::Engine,
                "All enabled stages complete",
            )?;
            run.error = None;
            run.persist()?;
            save_changes(run);
            emit(Event::State(Box::new(run.clone())));
            return Ok(Action::Complete);
        }
        if cancel.load(Ordering::Relaxed) {
            run.stop(
                Status::Cancelled,
                "Run cancelled before the next stage".into(),
                Actor::User,
                false,
            )?;
            run.persist()?;
            save_changes(run);
            return Ok(Action::Stopped);
        }
        // Spend is recorded per attempt; a cap stops the run before more.
        if let Some(reason) = crate::cost::check_budget(run) {
            run.stop(Status::Blocked, reason, Actor::Engine, false)?;
            emit(Event::State(Box::new(run.clone())));
            continue;
        }
        if run.workflow.stages[run.cursor].mode == Mode::Interactive {
            let reason = format!(
                "Stage {} waits for its terminal",
                run.workflow.stages[run.cursor].name
            );
            run.set(Target::Run, Status::Interactive, Actor::Engine, reason)?;
            run.persist()?;
            emit(Event::State(Box::new(run.clone())));
            return Ok(Action::Interactive);
        }
        execute_stage(run, cancel, None, &mut emit)?;
    }
}

/// Own the stage on a worker; its foreground terminal can attach independently.
pub fn run_interactive(
    run: &mut Run,
    cancel: &AtomicBool,
    session: &process::InteractiveSession,
    mut emit: impl FnMut(Event),
) -> Result<()> {
    struct FinishSession<'a>(&'a process::InteractiveSession);
    impl Drop for FinishSession<'_> {
        fn drop(&mut self) {
            self.0.finish();
        }
    }
    let _session = FinishSession(session);
    ensure!(
        run.status == Status::Interactive,
        "No interactive stage is ready"
    );
    let (_lock, _workspace_lock) = lock_run(run)?;
    run.check_revision()?;
    stop_orphans(run, &mut emit)?;
    execute_stage(run, cancel, Some(session), &mut emit)?;
    if matches!(
        run.status,
        Status::Failed | Status::Blocked | Status::Cancelled
    ) {
        save_changes(run);
    }
    Ok(())
}

/// Replaces the summary of the handoff awaiting confirmation and saves it;
/// `command` with `Cmd::EditSummary`.
pub fn edit_handoff(run: &mut Run, summary: String) -> Result<()> {
    run.command(Cmd::EditSummary(summary))
}

pub fn preview(
    settings: &Settings,
    workflow: &Workflow,
    project: &Path,
    task: &str,
) -> Result<String> {
    settings.validate()?;
    let temp = tempfile::tempdir()?;
    let mut text = format!(
        "{}\nProject: {}\nTask: {}\n\n",
        workflow.name,
        project.display(),
        task
    );
    for (index, stage) in workflow.stages.iter().enumerate() {
        if !stage.enabled {
            text.push_str(&format!("{}. {} [disabled]\n", index + 1, stage.name));
            continue;
        }
        let paths = ContextPaths {
            workspace: project,
            attempt_dir: temp.path(),
            prompt_file: Path::new("<attempt>/prompt.md"),
            handoff_file: Path::new("<workspace>/.model-router/handoffs/<run>/<stage>.json"),
        };
        let command = adapters::build(
            stage,
            &settings.adapters,
            &settings.providers,
            "<task + stage instructions + prior handoffs>",
            &paths,
        )?;
        text.push_str(&format!(
            "{}. {} | {} | {}\n   {}{}\n",
            index + 1,
            stage.name,
            stage.role.label(),
            stage.mode.label(),
            command.display(),
            if command.stdin.is_some() {
                " < <attempt>/prompt.md"
            } else {
                ""
            }
        ));
        if let Some(sandbox) = crate::sandbox::preview(workflow, stage) {
            text.push_str(&format!("   {sandbox}\n"));
        }
        for check in &stage.checks {
            text.push_str(&format!(
                "   check: {} ({}, {}s)\n",
                shell_words::join(&check.command),
                if check.required {
                    "required"
                } else {
                    "optional"
                },
                check.timeout_seconds
            ));
        }
    }
    Ok(text)
}

pub fn ensure_runnable(run: &Run) -> Result<()> {
    if let Some(error) = &run.error {
        bail!("{error}\nArtifacts: {}", run.directory.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Check, Stage};

    #[test]
    fn take_over_reopens_the_stopped_attempts_session() {
        let mut run = crate::report::tests::sample();
        run.cursor = 0;
        assert!(
            run.take_over().is_err(),
            "a stage must stop before it is taken over"
        );
        run.status = Status::Cancelled;
        let attempt = run.stages[0].attempts.last_mut().unwrap();
        attempt.session = Some("s-1".into());
        let handoff_file = attempt.handoff_file.clone();
        run.take_over().unwrap();
        assert_eq!(run.status, Status::Interactive);
        assert_eq!(run.workflow.stages[0].mode, Mode::Interactive);
        let resume = run.resume.clone().unwrap();
        assert_eq!(
            (resume.session.as_str(), resume.handoff_file),
            ("s-1", handoff_file)
        );
        run.retry(false, None).unwrap();
        assert!(run.resume.is_none(), "retry starts a new session");

        // Codex reports its thread in the attempt's log.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("stdout.log"),
            "{\"type\":\"thread.started\",\"thread_id\":\"t-1\"}\n",
        )
        .unwrap();
        run.workflow.stages[0].harness = "codex".into();
        run.status = Status::Failed;
        let attempt = run.stages[0].attempts.last_mut().unwrap();
        attempt.session = None;
        attempt.directory = dir.path().into();
        run.take_over().unwrap();
        assert_eq!(run.resume.as_ref().unwrap().session, "t-1");

        run.status = Status::Failed;
        run.workflow.stages[0].harness = "gemini".into();
        assert!(run.take_over().is_err());
    }

    #[test]
    fn take_over_never_accepts_the_previous_attempts_handoff() {
        let implement = Stage {
            id: "implement".into(),
            role: Role::Implementation,
            ..Stage::default()
        };
        let (_dir, mut run) =
            interactive(implement, Some(r#"{"summary":"Old","verdict":"complete"}"#));
        assert_eq!(run.status, Status::AwaitingConfirmation);
        run.workflow.stages[0].harness = "claude".into();
        let previous = run.stages[0].attempts[0].clone();
        run.stages[0].attempts[0].session = Some("s-1".into());
        run.take_over().unwrap();
        assert!(
            !previous.handoff_file.exists(),
            "stale handoff left in place"
        );
        assert!(previous.directory.join("handoff-file.json").is_file());
        let prepared = prepare(&mut run).unwrap();
        let prompt = fs::read_to_string(prepared.directory.join("prompt.md")).unwrap();
        assert!(prompt.contains(&prepared.handoff_file.display().to_string()));
        assert!(!prepared.handoff_file.exists());
    }

    #[test]
    fn illegal_transitions_are_refused_and_not_recorded() {
        let mut run = crate::report::tests::sample();
        let refused = |run: &mut Run, target, to| {
            let before = (run.status, run.stages[0].status, run.journal.len());
            let error = run
                .transition(Transition::new(target, to, Actor::Engine, "test"))
                .unwrap_err();
            assert!(error.to_string().starts_with("Illegal"), "{error}");
            assert_eq!(
                before,
                (run.status, run.stages[0].status, run.journal.len())
            );
        };
        // Values that belong to another level.
        refused(&mut run, Target::Run, Status::Skipped);
        refused(&mut run, Target::Stage(0), Status::Interactive);
        refused(&mut run, Target::Attempt(0), Status::AwaitingConfirmation);
        refused(&mut run, Target::Attempt(0), Status::Pending);
        // A complete run is final; an enabled stage is never skipped.
        refused(&mut run, Target::Run, Status::Pending);
        refused(&mut run, Target::Stage(0), Status::Skipped);
        refused(&mut run, Target::Stage(2), Status::Pending);
        // A finished attempt never runs again.
        refused(&mut run, Target::Attempt(0), Status::Running);
        assert!(
            run.transition(Transition::new(
                Target::Stage(9),
                Status::Pending,
                Actor::Engine,
                "test"
            ))
            .is_err()
        );
        // Moving to the current status records nothing.
        run.set(Target::Run, Status::Complete, Actor::Engine, "same")
            .unwrap();
        assert!(run.journal.is_empty());
        run.status = Status::Failed;
        run.set(Target::Stage(0), Status::Pending, Actor::User, "reset")
            .unwrap();
        let record = run.journal.last().unwrap();
        assert_eq!(
            (record.level, record.stage_index, record.from, record.to),
            (Level::Stage, Some(0), Status::Complete, Status::Pending)
        );
    }

    #[test]
    fn prior_handoffs_are_cut_to_valid_json() {
        let long = "word ".repeat(2000);
        let handoff: Handoff = serde_json::from_value(serde_json::json!({
            "summary": long,
            "verdict": "complete",
            "decisions": vec![long.clone(); 30],
            "files_changed": (0..500).map(|i| format!("src/file{i}.rs")).collect::<Vec<_>>(),
        }))
        .unwrap();
        let full = Path::new("/runs/x/handoff.json");
        let excerpt = handoff_excerpt(&handoff, 5000, full);
        let (json, note) = excerpt.rsplit_once('\n').unwrap();
        assert_eq!(note, "[truncated; full artifact at /runs/x/handoff.json]");
        assert!(json.len() <= 5000, "{}", json.len());
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["verdict"], "complete");
        assert!(value["summary"].as_str().unwrap().ends_with('…'));
        // A small handoff is shown whole, without a note.
        let small: Handoff =
            serde_json::from_str(r#"{"summary":"Done","verdict":"complete"}"#).unwrap();
        let whole = handoff_excerpt(&small, 5000, full);
        assert!(serde_json::from_str::<serde_json::Value>(&whole).is_ok());
    }

    const NOT_APPLICABLE: &str = r#"{"summary":"Docs only","verdict":"complete","tests":[],"not_applicable":"No code changed"}"#;

    /// Finishes an interactive attempt as if its harness wrote `handoff` and exited 0.
    fn interactive(stage: Stage, handoff: Option<&str>) -> (tempfile::TempDir, Run) {
        let dir = tempfile::tempdir().unwrap();
        let next = Stage {
            id: "next".into(),
            harness: "mock".into(),
            ..Stage::default()
        };
        let settings = Settings {
            workflows: vec![Workflow {
                name: "Interactive".into(),
                max_repair_passes: 0,
                sandbox_default: Default::default(),
                budget: Default::default(),
                stages: vec![
                    Stage {
                        harness: "mock".into(),
                        mode: Mode::Interactive,
                        ..stage
                    },
                    next,
                ],
            }],
            adapters: BTreeMap::from([(
                "mock".into(),
                CustomAdapter {
                    command: "python3".into(),
                    interactive_args: Some(vec![]),
                    ..CustomAdapter::default()
                },
            )]),
            ..Settings::default()
        };
        let mut run = Run::create(
            dir.path(),
            &settings,
            &settings.workflows[0],
            "Task".into(),
            false,
        )
        .unwrap();
        let prepared = prepare(&mut run).unwrap();
        fs::write(prepared.directory.join("stdout.log"), "transcript").unwrap();
        if let Some(handoff) = handoff {
            fs::write(&prepared.handoff_file, handoff).unwrap();
        }
        let outcome = process::Outcome {
            code: Some(0),
            cancelled: false,
            timed_out: false,
        };
        finish(
            &mut run,
            &prepared,
            Ok(outcome),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        (dir, run)
    }
    fn python(code: &str) -> Check {
        Check {
            command: vec!["python3".into(), "-c".into(), code.into()],
            ..Check::default()
        }
    }

    #[test]
    fn interactive_not_applicable_is_accepted_by_one_confirmation() {
        let verify = Stage {
            id: "verify".into(),
            role: Role::Test,
            ..Stage::default()
        };
        let (_dir, mut run) = interactive(verify, Some(NOT_APPLICABLE));
        assert_eq!(run.status, Status::AwaitingConfirmation);
        assert_eq!(run.pending_waiver(), Some("No code changed"));
        run.confirm().unwrap();
        assert_eq!((run.status, run.cursor), (Status::Pending, 1));
        assert_eq!(run.stages[0].status, Status::Complete);
        assert_eq!(
            run.stages[0].waiver.as_ref().unwrap().reason,
            "No code changed"
        );
        assert!(run.confirm().is_err());
        // Retry resets from the cursor; point it back at the accepted stage.
        run.cursor = 0;
        run.retry(false, None).unwrap();
        assert!(run.stages[0].waiver.is_none());
    }

    #[test]
    fn interactive_test_with_router_evidence_needs_no_waiver() {
        let verify = Stage {
            id: "verify".into(),
            role: Role::Test,
            checks: vec![python("pass")],
            ..Stage::default()
        };
        let (_dir, mut run) = interactive(verify, Some(NOT_APPLICABLE));
        assert_eq!(run.status, Status::AwaitingConfirmation);
        assert_eq!(
            run.stages[0].attempts[0].checks[0].status,
            TestStatus::Passed
        );
        assert_eq!(run.pending_waiver(), None);
        run.confirm().unwrap();
        assert_eq!(run.cursor, 1);
        assert!(run.stages[0].waiver.is_none());
    }

    #[test]
    fn interactive_checkpoint_shows_checks_before_blocking() {
        let implement = Stage {
            id: "implement".into(),
            role: Role::Implementation,
            checks: vec![python("raise SystemExit(4)")],
            ..Stage::default()
        };
        // No handoff file: the placeholder checkpoint still runs checks.
        let (_dir, mut run) = interactive(implement, None);
        assert_eq!(run.status, Status::AwaitingConfirmation);
        assert!(!run.stages[0].handoff.as_ref().unwrap().structured);
        assert_eq!(run.stages[0].attempts[0].checks[0].exit_code, Some(4));
        assert_eq!(run.pending_waiver(), None);
        run.confirm().unwrap();
        assert_eq!((run.status, run.cursor), (Status::Blocked, 0));
        assert!(run.error.as_deref().unwrap().contains("(exit 4)"));
    }
}
