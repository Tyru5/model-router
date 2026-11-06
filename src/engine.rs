use crate::providers::{self, Provider};
use crate::{
    adapters::{self, ContextPaths, Invocation},
    checks::{self, CheckResult},
    config::{CustomAdapter, Mode, Role, Settings, Stage, Workflow},
    events, fanout, graph,
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
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
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
        // A fan-out candidate runs, ends, and runs again only when retried.
        Level::Candidate => matches!(
            (from, to),
            (Pending, Running | Failed | Cancelled)
                | (Running, Complete | Failed | Blocked | Cancelled)
                | (Failed | Blocked | Cancelled, Pending)
        ),
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
    /// Rerun one stopped candidate of a stopped fan-out stage, keeping the others.
    RetryCandidate { stage: String, candidate: String },
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
    /// A fan-out candidate attempt's own Git worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageState {
    pub status: Status,
    pub attempts: Vec<Attempt>,
    pub handoff: Option<Handoff>,
    /// User acceptance of a test stage that reported no applicable check,
    /// or of a ship stage's plan (see `ship::confirm`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiver: Option<Waiver>,
    /// A fan-out stage's candidates and choice; None for other stages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_out: Option<FanOutState>,
}

/// One round of a best-of-N stage: where candidates started, each
/// candidate, and which one reached the workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanOutState {
    /// The workspace snapshot every candidate started from.
    pub base: String,
    /// The private commit of `base` their worktrees check out.
    pub base_commit: String,
    pub candidates: Vec<CandidateState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<Selection>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateState {
    pub id: String,
    pub harness: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub effort: String,
    pub status: Status,
    pub attempts: Vec<Attempt>,
    pub handoff: Option<Handoff>,
    /// What the latest attempt changed, from the base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<CandidateChanges>,
    /// Why the latest attempt stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateChanges {
    pub files: Vec<String>,
    pub added: u64,
    pub removed: u64,
    /// The full patch from the base; what the router applies if chosen.
    pub patch: PathBuf,
}
impl CandidateChanges {
    pub fn lines(&self) -> u64 {
        self.added + self.removed
    }
}
impl CandidateState {
    /// Whether every required router check of its latest attempt passed.
    pub fn checks_pass(&self) -> bool {
        self.attempts.last().is_none_or(|a| {
            a.checks
                .iter()
                .all(|c| !c.required || c.status == TestStatus::Passed)
        })
    }
}
/// The judge's choice and whether its patch reached the workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Selection {
    pub candidate: String,
    /// `auto`, or the judge stage's ID.
    pub by: String,
    pub reason: String,
    pub applied: bool,
    pub at: DateTime<Utc>,
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
    /// While stages run side by side, their stops and repair requests wait
    /// here and the run's own status settles once they all end.
    #[serde(skip)]
    hold: Option<Hold>,
    /// The fan-out candidate whose prompt `prompt` renders.
    #[serde(skip)]
    view: Option<usize>,
}

/// Run-level outcomes deferred while several stages run; see `Run::settle`.
#[derive(Debug, Clone, Default)]
struct Hold {
    /// The first review repair requested: (review, target, feedback, reason).
    repair: Option<(usize, usize, String, String)>,
    /// Messages of stages that stopped, by stage.
    stops: Vec<(usize, String)>,
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
    /// A judge chose candidate `candidate` of fan-out stage `fan`.
    Select {
        fan: usize,
        candidate: usize,
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
        let candidates: Vec<Stage> = workflow
            .stages
            .iter()
            .filter(|s| s.enabled)
            .flat_map(|s| {
                s.fan_out
                    .iter()
                    .flat_map(|f| &f.candidates)
                    .map(|c| c.stage(s))
            })
            .collect();
        ensure!(
            candidates.is_empty() || snapshot::is_repository(project),
            "Fan-out stages need a Git repository: candidates run in Git worktrees"
        );
        for stage in workflow
            .stages
            .iter()
            // Ship stages run in the router, without a harness.
            .filter(|s| s.enabled && s.fan_out.is_none() && s.role != Role::Ship)
            .chain(&candidates)
        {
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
                    fan_out: None,
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
            hold: None,
            view: None,
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
            Target::Stage(i)
            | Target::Attempt(i)
            | Target::Candidate(i, _)
            | Target::CandidateAttempt(i, _) => {
                let stage = self
                    .workflow
                    .stages
                    .get(i)
                    .with_context(|| format!("No stage {}", i + 1))?;
                let level = match target {
                    Target::Stage(_) => Level::Stage,
                    Target::Candidate(..) => Level::Candidate,
                    _ => Level::Attempt,
                };
                (level, Some(i), stage.enabled)
            }
        };
        let (slot, attempt, candidate) = match target {
            Target::Run => (&mut self.status, None, None),
            Target::Stage(i) => (&mut self.stages[i].status, None, None),
            Target::Attempt(i) => {
                let attempt = self.stages[i]
                    .attempts
                    .last_mut()
                    .with_context(|| format!("Stage {} has no attempt", i + 1))?;
                (&mut attempt.status, Some(attempt.number), None)
            }
            Target::Candidate(i, c) | Target::CandidateAttempt(i, c) => {
                let candidate = self.stages[i]
                    .fan_out
                    .as_mut()
                    .and_then(|f| f.candidates.get_mut(c))
                    .with_context(|| format!("Stage {} has no candidate {}", i + 1, c + 1))?;
                let id = Some(candidate.id.clone());
                if matches!(target, Target::Candidate(..)) {
                    (&mut candidate.status, None, id)
                } else {
                    let attempt = candidate
                        .attempts
                        .last_mut()
                        .with_context(|| format!("Candidate {} has no attempt", candidate.id))?;
                    (&mut attempt.status, Some(attempt.number), id)
                }
            }
        };
        let from = *slot;
        if from == to {
            return Ok(());
        }
        ensure!(
            legal(level, from, to, enabled),
            "Illegal {} transition{}{}: {} → {} ({reason})",
            level.label(),
            stage_index.map_or_else(String::new, |i| format!(" in stage {}", i + 1)),
            candidate
                .as_ref()
                .map_or_else(String::new, |c| format!(" candidate {c}")),
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
            candidate,
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
        ensure!(
            !(interactive && self.workflow.stages[index].fan_out.is_some()),
            "Fan-out candidates run unattended; retry without interactive, or retry one candidate"
        );
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
            stage.fan_out.is_none(),
            "Fan-out stages have no session to take over; open a candidate's worktree, or retry one candidate"
        );
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

    /// Return stage `from` and every stage after it (in a graph, those that
    /// depend on it) to pending, discarding handoffs, acceptances, and fan-out
    /// rounds.
    fn reset(&mut self, from: usize, actor: Actor, reason: &str) -> Result<()> {
        let order = self.order();
        for i in from..self.stages.len() {
            if i != from && self.graph() && !order.precedes(from, i) {
                continue;
            }
            let to = if self.workflow.stages[i].enabled {
                Status::Pending
            } else {
                Status::Skipped
            };
            self.set(Target::Stage(i), to, actor, reason)?;
            let state = &mut self.stages[i];
            state.handoff = None;
            state.waiver = None;
            state.fan_out = None;
        }
        // A graph's dependents may come earlier in the list.
        if self.graph() {
            for i in (0..from).filter(|&i| order.precedes(from, i)) {
                let to = if self.workflow.stages[i].enabled {
                    Status::Pending
                } else {
                    Status::Skipped
                };
                self.set(Target::Stage(i), to, actor, reason)?;
                let state = &mut self.stages[i];
                state.handoff = None;
                state.waiver = None;
                state.fan_out = None;
            }
        }
        Ok(())
    }

    /// Whether the workflow is a dependency graph; see `graph`.
    pub fn graph(&self) -> bool {
        graph::is_graph(&self.workflow)
    }
    /// Which stages run before which.
    pub fn order(&self) -> graph::Ancestry {
        graph::order(&self.workflow)
    }
    /// The first stage not yet complete or disabled; the stage commands act on.
    fn focus(&self) -> usize {
        self.stages
            .iter()
            .position(|s| !matches!(s.status, Status::Complete | Status::Skipped))
            .unwrap_or(self.stages.len())
    }
    /// Sets the run's status, unless stages run side by side: then stops are
    /// collected and `settle` decides once they all end.
    fn set_run(&mut self, to: Status, actor: Actor, reason: impl Into<String>) -> Result<()> {
        let reason = reason.into();
        if let Some(hold) = &mut self.hold {
            if to.stopped() {
                hold.stops.push((self.cursor, reason));
            }
            return Ok(());
        }
        self.set(Target::Run, to, actor, reason)
    }

    /// Rerun one stopped candidate of the stopped fan-out stage `stage`; the
    /// other candidates keep their results. In memory only; `command`
    /// applies it under the lock.
    pub fn retry_candidate(&mut self, stage: &str, candidate: &str) -> Result<()> {
        let index = self
            .workflow
            .stages
            .iter()
            .position(|s| s.id == stage)
            .with_context(|| format!("No stage {stage}"))?;
        ensure!(
            self.status.stopped() && self.stages[index].status.stopped(),
            "Only a stopped fan-out stage of a stopped run can retry a candidate"
        );
        ensure!(
            self.graph() || self.cursor == index,
            "The run stopped at another stage"
        );
        let c = self.stages[index]
            .fan_out
            .as_ref()
            .and_then(|f| f.candidates.iter().position(|c| c.id == candidate))
            .with_context(|| format!("Stage {stage} has no candidate {candidate}"))?;
        let state = &self.stages[index]
            .fan_out
            .as_ref()
            .context("No fan-out")?
            .candidates[c];
        ensure!(
            state.status.stopped(),
            "Candidate {candidate} is {}; only a stopped candidate can be retried",
            state.status.label()
        );
        let reason = format!("Retry candidate {candidate}");
        self.set(
            Target::Candidate(index, c),
            Status::Pending,
            Actor::User,
            &reason,
        )?;
        self.set(Target::Stage(index), Status::Pending, Actor::User, &reason)?;
        self.stages[index].handoff = None;
        self.cursor = index;
        self.error = None;
        self.resume = None;
        self.set(Target::Run, Status::Pending, Actor::User, reason)
    }

    /// Confirming a checkpoint also accepts its pending waiver; it never needs
    /// a second step. In memory only; `command` applies it under the lock.
    pub fn confirm(&mut self) -> Result<()> {
        ensure!(
            self.status == Status::AwaitingConfirmation,
            "No handoff awaits confirmation"
        );
        if self.workflow.stages[self.cursor].role == Role::Ship {
            return crate::ship::confirm(self);
        }
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
        // The last to finish: stages side by side can end out of order.
        let latest = self
            .stages
            .iter()
            .flat_map(|s| &s.attempts)
            .max_by_key(|a| (a.finished_at.unwrap_or(a.started_at), a.started_at));
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
        // A judge chooses; it never requests repairs.
        if let Some(fan) = graph::judged_by(&self.workflow, self.cursor) {
            return Ok(self.judgement(fan, handoff));
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
            let order = self.order();
            let target = (0..self.stages.len()).rev().find(|&i| {
                let s = &self.workflow.stages[i];
                order.precedes(i, self.cursor)
                    && s.enabled
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
    pub(crate) fn advance(&mut self, actor: Actor) -> Result<()> {
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
                self.set_run(Status::AwaitingConfirmation, actor, reason)
            }
            Decision::Select { fan, candidate } => {
                let by = self.workflow.stages[index].id.clone();
                let summary = self.stages[index]
                    .handoff
                    .as_ref()
                    .map(|h| h.summary.clone())
                    .unwrap_or_default();
                match self.select(fan, candidate, &by, &summary)? {
                    Ok(()) => self.complete(index, actor),
                    Err(message) => self.stop(Status::Blocked, message, actor, true),
                }
            }
            Decision::Repair { target, feedback } => {
                let reason = format!(
                    "Review {name} requested a repair at {}",
                    self.workflow.stages[target].name
                );
                self.set(Target::Attempt(index), Status::Complete, actor, &reason)?;
                // Stages still running see it once they end; see `settle`.
                if let Some(hold) = &mut self.hold {
                    hold.repair.get_or_insert((index, target, feedback, reason));
                    return Ok(());
                }
                self.repair_feedback = Some(feedback);
                self.repair_review = Some(index);
                self.repair_passes += 1;
                // Invalidate all downstream handoffs, including previously passing reviews.
                self.reset(target, actor, &reason)?;
                self.cursor = target;
                self.set(Target::Run, Status::Pending, actor, reason)
            }
            Decision::Advance => self.complete(index, actor),
        }
    }

    /// Completes stage `index` and moves on.
    fn complete(&mut self, index: usize, actor: Actor) -> Result<()> {
        let reason = format!("Stage {} complete", self.workflow.stages[index].name);
        self.set(Target::Attempt(index), Status::Complete, actor, &reason)?;
        self.set(Target::Stage(index), Status::Complete, actor, &reason)?;
        // The review that asked for the repair passed: later stages, and
        // later retries, must not see its old request.
        if self.repair_review == Some(index) {
            self.repair_feedback = None;
            self.repair_review = None;
        }
        self.cursor = if self.graph() {
            self.focus()
        } else {
            index + 1
        };
        self.set_run(Status::Pending, actor, reason)
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
    pub(crate) fn stop(
        &mut self,
        status: Status,
        message: String,
        actor: Actor,
        attempt: bool,
    ) -> Result<()> {
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
        self.set_run(status, actor, message.clone())?;
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
        Cmd::RetryCandidate { stage, candidate } => run.retry_candidate(&stage, &candidate)?,
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
    // Stages this one runs after; in a list, every earlier stage.
    let order = run.order();
    for (index, state) in run
        .stages
        .iter()
        .enumerate()
        .rev()
        .filter(|(index, _)| order.precedes(*index, run.cursor))
    {
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
    let feedback = run.repair_feedback.as_ref().filter(|_| {
        run.repair_review
            .is_none_or(|review| run.cursor == review || order.precedes(run.cursor, review))
    });
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
    text.push_str(&fan_out_context(run));
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
        worktree: None,
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
    attempt: Option<&mut Attempt>,
    mode: Mode,
    (harness, workspace): (&str, &Path),
    prepared: &Prepared,
    result: Result<process::Outcome>,
) -> Result<Exit> {
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            // The supervisor ended the group before returning its error.
            if let Some(attempt) = attempt {
                attempt.process_group = None;
            }
            return Ok(Exit::Stopped(Status::Failed, format!("{error:#}")));
        }
    };
    let stdout = prepared.directory.join("stdout.log");
    if let Some(attempt) = attempt {
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
                harness,
                attempt.session.as_deref(),
                workspace,
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
    stage: &Stage,
    prepared: &Prepared,
    normalized: handoff::Normalized,
) -> Result<Resolved> {
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
    let mode = run.workflow.stages[index].mode;
    let harness = run.workflow.stages[index].harness.clone();
    let workspace = run.workspace.clone();
    let attempt = run.stages[index].attempts.last_mut();
    let normalized = match classify_exit(attempt, mode, (&harness, &workspace), prepared, result)? {
        Exit::Succeeded(normalized) => normalized,
        Exit::Stopped(status, message) => return run.stop(status, message, Actor::Engine, true),
    };
    let (source, handoff) =
        match resolve_handoff(&run.workflow.stages[index], prepared, normalized)? {
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

/// Prepares the current stage's attempt and announces it; None when that
/// failed and stopped the stage.
fn start_stage(run: &mut Run, emit: &mut impl FnMut(Event)) -> Result<Option<Prepared>> {
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
            return Ok(None);
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
    Ok(Some(prepared))
}

/// Finishes the current stage's attempt after its harness ended, records
/// the workspace snapshot, and saves.
fn conclude(
    run: &mut Run,
    prepared: &Prepared,
    result: Result<process::Outcome>,
    cancel: &AtomicBool,
    emit: &mut impl FnMut(Event),
) -> Result<()> {
    let index = run.cursor;
    if let Err(error) = finish(run, prepared, result, cancel, emit) {
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

fn execute_stage(
    run: &mut Run,
    cancel: &AtomicBool,
    interactive: Option<&process::InteractiveSession>,
    emit: &mut impl FnMut(Event),
) -> Result<()> {
    let index = run.cursor;
    let Some(prepared) = start_stage(run, emit)? else {
        return Ok(());
    };
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
    conclude(run, &prepared, result, cancel, emit)
}

const INTERRUPTED: &str =
    "A previous execution was interrupted. Inspect artifacts and retry explicitly";

/// Best-of-N stages: rounds, candidates, judging, and applying the winner.
impl Run {
    /// Marks every running fan-out candidate failed: its process died with
    /// an earlier router. Never retried silently.
    fn interrupt_candidates(&mut self, message: &str) -> Result<()> {
        for index in 0..self.stages.len() {
            let count = self.stages[index]
                .fan_out
                .as_ref()
                .map_or(0, |f| f.candidates.len());
            for c in 0..count {
                if self.candidate(index, c).status == Status::Running {
                    self.stop_candidate(index, c, Status::Failed, message.into(), Actor::Recovery)?;
                }
            }
        }
        Ok(())
    }

    fn candidate(&self, index: usize, c: usize) -> &CandidateState {
        &self.stages[index]
            .fan_out
            .as_ref()
            .expect("A fan-out stage has candidates")
            .candidates[c]
    }
    fn candidate_mut(&mut self, index: usize, c: usize) -> &mut CandidateState {
        &mut self.stages[index]
            .fan_out
            .as_mut()
            .expect("A fan-out stage has candidates")
            .candidates[c]
    }

    /// Ends candidate `c` with `status`, and its running attempt.
    fn stop_candidate(
        &mut self,
        index: usize,
        c: usize,
        status: Status,
        message: String,
        actor: Actor,
    ) -> Result<()> {
        if self
            .candidate(index, c)
            .attempts
            .last()
            .is_some_and(|a| a.status == Status::Running)
        {
            self.set(Target::CandidateAttempt(index, c), status, actor, &message)?;
            let attempt = self
                .candidate_mut(index, c)
                .attempts
                .last_mut()
                .context("No attempt")?;
            attempt.error = Some(message.clone());
            attempt.finished_at.get_or_insert_with(Utc::now);
            attempt.process_group = None;
        }
        self.set(Target::Candidate(index, c), status, actor, &message)?;
        self.candidate_mut(index, c).error = Some(message);
        Ok(())
    }

    /// Starts a round of fan-out stage `index`: a new attempt for the stage,
    /// and, unless an earlier round left candidates to retry, a base commit
    /// of the workspace and pending candidates.
    fn start_fan_out(&mut self, index: usize) -> Result<()> {
        let stage = self.workflow.stages[index].clone();
        let fan = stage.fan_out.clone().context("Not a fan-out stage")?;
        let number = self.stages[index].attempts.len() + 1;
        let directory = self
            .directory
            .join(format!("{:02}-{}", index + 1, stage.id))
            .join(format!("attempt-{number:03}"));
        fs::create_dir_all(&directory)?;
        if self.stages[index].fan_out.is_none() {
            let base = snapshot::tree(&self.workspace, &self.directory)
                .context("Fan-out needs a Git snapshot of the workspace")?;
            let reference = snapshot::base_ref(&self.id, &format!("{}-{number:03}", stage.id));
            let base_commit = fanout::base_commit(&self.workspace, &base, &reference)?;
            let candidates = fan
                .candidates
                .iter()
                .zip(fan.ids())
                .map(|(c, id)| CandidateState {
                    id,
                    harness: c.harness.clone(),
                    model: c.model.clone(),
                    provider: c.provider.clone(),
                    effort: c.effort.clone(),
                    status: Status::Pending,
                    attempts: vec![],
                    handoff: None,
                    changes: None,
                    error: None,
                })
                .collect();
            self.stages[index].fan_out = Some(FanOutState {
                base,
                base_commit,
                candidates,
                selection: None,
            });
        }
        let state = self.stages[index].fan_out.as_ref().context("No fan-out")?;
        let pending = state
            .candidates
            .iter()
            .filter(|c| c.status == Status::Pending)
            .count();
        let base = state.base.clone();
        self.stages[index].attempts.push(Attempt {
            number,
            started_at: Utc::now(),
            finished_at: None,
            model: format!("{} candidates", fan.candidates.len()),
            mode: Mode::Unattended,
            status: Status::Pending,
            exit_code: None,
            directory: directory.clone(),
            handoff_file: directory.join("handoff.json"),
            error: None,
            checks: vec![],
            tree_before: Some(base),
            tree_after: None,
            usage: None,
            session: None,
            process_group: None,
            handoff_source: None,
            worktree: None,
        });
        let reason = format!("Fan-out round {number} started: {pending} candidate(s)");
        self.resume = None;
        self.error = None;
        self.set(
            Target::Attempt(index),
            Status::Running,
            Actor::Engine,
            &reason,
        )?;
        self.set(
            Target::Stage(index),
            Status::Running,
            Actor::Engine,
            &reason,
        )?;
        self.set(Target::Run, Status::Running, Actor::Engine, reason)?;
        self.persist()
    }

    /// Prepares candidate `c` of fan-out stage `index` in a new worktree from
    /// the round's base, with the stage's prompt as that candidate sees it.
    fn launch_candidate(&mut self, index: usize, c: usize) -> Result<(Prepared, PathBuf)> {
        let stage = self.workflow.stages[index].clone();
        let config = stage
            .fan_out
            .as_ref()
            .and_then(|f| f.candidates.get(c))
            .context("No such candidate")?
            .clone();
        let candidate = config.stage(&stage);
        let state = self.stages[index].fan_out.as_ref().context("No fan-out")?;
        let (base, base_commit) = (state.base.clone(), state.base_commit.clone());
        let id = state.candidates[c].id.clone();
        let number = state.candidates[c].attempts.len() + 1;
        let directory = self
            .directory
            .join(format!("{:02}-{}", index + 1, stage.id))
            .join("candidates")
            .join(&id)
            .join(format!("attempt-{number:03}"));
        fs::create_dir_all(&directory)?;
        let worktree = fanout::candidates_dir(&self.project, &self.id)?
            .join(&stage.id)
            .join(format!("{id}-{number:03}"));
        fanout::add_worktree(&self.workspace, &worktree, &base_commit)?;
        // A project inside a larger repository works in the same subdirectory.
        let cwd = worktree.join(fanout::prefix(&self.workspace));
        let handoff_file = storage::metadata_dir(&cwd)?
            .join("handoffs")
            .join(&self.id)
            .join(format!("{}-{id}-{number:03}.json", stage.id));
        fs::create_dir_all(handoff_file.parent().context("No handoff directory")?)?;
        let mut view = self.clone();
        view.workspace = cwd.clone();
        view.workflow.stages[index] = candidate.clone();
        view.cursor = index;
        view.view = Some(c);
        // The prompt names this copy of the handoff schema.
        let schema_file = directory.join(handoff::SCHEMA_FILE);
        storage::atomic_write(&schema_file, handoff::SCHEMA.as_bytes())?;
        let text = prompt(&view, &handoff_file, &schema_file);
        let prompt_file = directory.join("prompt.md");
        storage::atomic_write(&prompt_file, text.as_bytes())?;
        let paths = ContextPaths {
            workspace: &cwd,
            attempt_dir: &directory,
            prompt_file: &prompt_file,
            handoff_file: &handoff_file,
        };
        let mut invocation =
            adapters::build(&candidate, &self.adapters, &self.providers, &text, &paths)?;
        let session = adapters::assign_session(&mut invocation, &candidate.harness);
        let sandbox = crate::sandbox::plan(
            &self.workflow,
            &candidate,
            &crate::sandbox::Paths {
                workspace: &cwd,
                attempt_dir: &directory,
                handoff_dir: handoff_file.parent().context("No handoff directory")?,
                program: &invocation.program,
            },
        )?;
        storage::write_json(
            &directory.join("command.json"),
            &serde_json::json!({ "program": invocation.program, "args": invocation.args, "stdin_from": invocation.stdin.as_ref().map(|_| "prompt.md"), "env": invocation.env.iter().map(|(name, _)| name).collect::<Vec<_>>(), "workspace": cwd, "model": candidate.model, "provider": candidate.provider, "effort": candidate.effort, "mode": candidate.mode, "candidate": id, "sandbox": sandbox }),
        )?;
        let state = self.candidate_mut(index, c);
        state.harness = config.harness.clone();
        state.model = config.model.clone();
        state.provider = config.provider.clone();
        state.effort = config.effort.clone();
        state.error = None;
        state.handoff = None;
        state.changes = None;
        state.attempts.push(Attempt {
            number,
            started_at: Utc::now(),
            finished_at: None,
            model: candidate.model.clone(),
            mode: Mode::Unattended,
            status: Status::Pending,
            exit_code: None,
            directory: directory.clone(),
            handoff_file: handoff_file.clone(),
            error: None,
            checks: vec![],
            tree_before: Some(base),
            tree_after: None,
            usage: None,
            session,
            process_group: None,
            handoff_source: None,
            worktree: Some(worktree),
        });
        let reason = format!(
            "Candidate {id} attempt {number} started: {} {}",
            candidate.harness, candidate.model
        );
        self.set(
            Target::CandidateAttempt(index, c),
            Status::Running,
            Actor::Engine,
            &reason,
        )?;
        self.set(
            Target::Candidate(index, c),
            Status::Running,
            Actor::Engine,
            reason,
        )?;
        self.persist()?;
        Ok((
            Prepared {
                invocation,
                sandbox,
                directory,
                handoff_file,
            },
            cwd,
        ))
    }

    /// Records how candidate `c`'s harness ended: its handoff, its router
    /// checks (run in its worktree), and its changes from the base.
    fn finish_candidate(
        &mut self,
        (index, c): (usize, usize),
        prepared: &Prepared,
        cwd: &Path,
        result: Result<process::Outcome>,
        cancel: &AtomicBool,
        emit: &mut impl FnMut(Event),
    ) -> Result<()> {
        let candidate = {
            let stage = &self.workflow.stages[index];
            let config = &stage.fan_out.as_ref().context("No fan-out")?.candidates[c];
            config.stage(stage)
        };
        let attempt = self.candidate_mut(index, c).attempts.last_mut();
        let normalized = match classify_exit(
            attempt,
            Mode::Unattended,
            (&candidate.harness, cwd),
            prepared,
            result,
        )? {
            Exit::Succeeded(normalized) => normalized,
            Exit::Stopped(status, message) => {
                return self.stop_candidate(index, c, status, message, Actor::Engine);
            }
        };
        let (source, handoff) = match resolve_handoff(&candidate, prepared, normalized)? {
            Resolved::Handoff(source, handoff) => (source, *handoff),
            Resolved::Stopped(status, message) => {
                return self.stop_candidate(index, c, status, message, Actor::Engine);
            }
        };
        storage::write_json(&prepared.directory.join("handoff.json"), &handoff)?;
        let verdict = handoff.verdict;
        let summary = handoff.summary.clone();
        let state = self.candidate_mut(index, c);
        state.handoff = Some(handoff);
        let attempt = state.attempts.last_mut().context("No attempt")?;
        attempt.handoff_source = Some(source);
        let worktree = attempt.worktree.clone().context("No worktree")?;
        if matches!(verdict, Verdict::Blocked | Verdict::NeedsChanges) {
            return self.stop_candidate(
                index,
                c,
                Status::Blocked,
                format!(
                    "Candidate reports incomplete work: {}",
                    storage::truncate(&summary, 500)
                ),
                Actor::Engine,
            );
        }
        if !candidate.checks.is_empty() {
            let (results, cancelled) = checks::run(
                &candidate.checks,
                cwd,
                &prepared.directory,
                cancel,
                |text| emit(Event::Output(text)),
            )?;
            if let Some(attempt) = self.candidate_mut(index, c).attempts.last_mut() {
                attempt.checks = results;
            }
            if cancelled {
                return self.stop_candidate(
                    index,
                    c,
                    Status::Cancelled,
                    "Candidate cancelled during checks; its worktree is retained".into(),
                    Actor::Engine,
                );
            }
        }
        let base = self.stages[index]
            .fan_out
            .as_ref()
            .context("No fan-out")?
            .base
            .clone();
        let tree = snapshot::tree_with(
            &worktree,
            &prepared.directory.join("snapshot.index"),
            &self.id,
        )
        .context("Snapshot the candidate's worktree")?;
        let patch = prepared.directory.join("changes.diff");
        fanout::write_patch(&worktree, &base, &tree, &patch)?;
        let (files, added, removed) = fanout::stat(&worktree, &base, &tree)?;
        let state = self.candidate_mut(index, c);
        state.changes = Some(CandidateChanges {
            files,
            added,
            removed,
            patch,
        });
        if let Some(attempt) = state.attempts.last_mut() {
            attempt.tree_after = Some(tree);
        }
        let reason = format!("Candidate {} complete", state.id);
        self.set(
            Target::CandidateAttempt(index, c),
            Status::Complete,
            Actor::Engine,
            &reason,
        )?;
        self.set(
            Target::Candidate(index, c),
            Status::Complete,
            Actor::Engine,
            reason,
        )
    }

    /// Whether no candidate of stage `index` waits or runs.
    fn fan_out_done(&self, index: usize) -> bool {
        self.stages[index].fan_out.as_ref().is_some_and(|f| {
            f.candidates
                .iter()
                .all(|c| !matches!(c.status, Status::Pending | Status::Running))
        })
    }

    /// Ends the round of fan-out stage `index` once its candidates have:
    /// stops when none succeeded or the run was cancelled, waits for the
    /// judge stage, or picks and applies a winner automatically.
    fn settle_fan_out(&mut self, index: usize, cancelled: bool) -> Result<()> {
        self.cursor = index;
        let stage = self.workflow.stages[index].clone();
        let state = self.stages[index].fan_out.as_ref().context("No fan-out")?;
        let complete = state
            .candidates
            .iter()
            .filter(|c| c.status == Status::Complete)
            .count();
        let total = state.candidates.len();
        let failures: Vec<String> = state
            .candidates
            .iter()
            .filter(|c| c.status != Status::Complete)
            .map(|c| {
                format!(
                    "{} {}{}",
                    c.id,
                    c.status.label(),
                    c.error
                        .as_deref()
                        .map(|e| format!(" ({})", e.lines().next().unwrap_or("")))
                        .unwrap_or_default()
                )
            })
            .collect();
        let auto = state.selection.is_none() && graph::judge_of(&self.workflow, index).is_none();
        let finish = |run: &mut Run| {
            let tree = snapshot::tree(&run.workspace, &run.directory);
            if let Some(attempt) = run.stages[index].attempts.last_mut() {
                attempt.finished_at.get_or_insert_with(Utc::now);
                attempt.tree_after = tree;
            }
        };
        if cancelled
            || state
                .candidates
                .iter()
                .any(|c| c.status == Status::Cancelled)
                && complete == 0
        {
            let message = format!(
                "Fan-out stopped before every candidate finished ({}). Worktrees are retained; retry a candidate (resume --retry-candidate {}/<id>) or the stage",
                failures.join("; "),
                stage.id
            );
            self.stop(Status::Cancelled, message, Actor::Engine, true)?;
            finish(self);
            return Ok(());
        }
        if complete == 0 {
            let message = format!(
                "No fan-out candidate succeeded: {}. Retry a candidate (resume --retry-candidate {}/<id>) or the stage",
                failures.join("; "),
                stage.id
            );
            self.stop(Status::Blocked, message, Actor::Engine, true)?;
            finish(self);
            return Ok(());
        }
        if !auto {
            let judge = graph::judge_of(&self.workflow, index)
                .map_or_else(String::new, |j| self.workflow.stages[j].name.clone());
            let handoff: Handoff = serde_json::from_value(serde_json::json!({
                "summary": format!("{complete} of {total} fan-out candidates finished; the judge stage {judge:?} chooses one, then the router applies its changes to the workspace."),
                "verdict": "complete",
                "unresolved": failures,
            }))?;
            if let Some(attempt) = self.stages[index].attempts.last() {
                storage::write_json(&attempt.directory.join("handoff.json"), &handoff)?;
            }
            self.stages[index].handoff = Some(handoff);
            finish(self);
            return self.advance(Actor::Engine);
        }
        let state = self.stages[index].fan_out.as_ref().context("No fan-out")?;
        let winner = auto_pick(state).context("No complete candidate")?;
        let chosen = &state.candidates[winner];
        let reason = format!(
            "auto: {} required checks, {} changed lines, among {complete} complete candidates",
            if chosen.checks_pass() {
                "passing"
            } else {
                "failing"
            },
            chosen.changes.as_ref().map_or(0, CandidateChanges::lines)
        );
        let outcome = self.select(index, winner, crate::config::AUTO_JUDGE, &reason)?;
        finish(self);
        match outcome {
            Ok(()) => self.advance(Actor::Engine),
            Err(message) => self.stop(Status::Blocked, message, Actor::Engine, true),
        }
    }

    /// Why a judge's handoff chooses, or fails to choose, a candidate of
    /// fan-out stage `fan`.
    fn judgement(&self, fan: usize, handoff: &Handoff) -> Decision {
        let Some(state) = self.stages[fan].fan_out.as_ref() else {
            return Decision::Block(format!(
                "Fan-out stage {} has no candidates to judge",
                self.workflow.stages[fan].name
            ));
        };
        let ids = state
            .candidates
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let Some(selected) = handoff
            .selected
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Decision::Block(format!(
                "The judge chose no candidate: its handoff needs \"selected\" set to one of {ids}. Retry it"
            ));
        };
        match state.candidates.iter().position(|c| c.id == selected) {
            Some(c) if state.candidates[c].status == Status::Complete => {
                Decision::Select { fan, candidate: c }
            }
            Some(c) => Decision::Block(format!(
                "The judge chose candidate {selected}, which is {}. Retry the judge",
                state.candidates[c].status.label()
            )),
            None => Decision::Block(format!(
                "The judge chose unknown candidate {selected}; candidates: {ids}. Retry it"
            )),
        }
    }

    /// Applies candidate `c` of fan-out stage `fan` to the workspace and
    /// records the choice; the inner error explains a conflict, which leaves
    /// the workspace unchanged.
    fn select(
        &mut self,
        fan: usize,
        c: usize,
        by: &str,
        reason: &str,
    ) -> Result<std::result::Result<(), String>> {
        let state = self.stages[fan].fan_out.as_ref().context("No fan-out")?;
        if let Some(done) = state.selection.as_ref().filter(|s| s.applied) {
            return Ok(Err(format!(
                "Candidate {} was already applied; retry the fan-out stage to choose again",
                done.candidate
            )));
        }
        let candidate = &state.candidates[c];
        let id = candidate.id.clone();
        let changes = candidate
            .changes
            .clone()
            .context("The candidate's changes were not recorded")?;
        let worktree = candidate
            .attempts
            .last()
            .and_then(|a| a.worktree.clone())
            .unwrap_or_default();
        let ids = state
            .candidates
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let mut handoff = candidate
            .handoff
            .clone()
            .context("The candidate has no handoff")?;
        let current = snapshot::tree(&self.workspace, &self.directory)
            .context("Snapshot the workspace before applying the candidate")?;
        let scratch = self.stages[fan]
            .attempts
            .last()
            .map_or_else(|| self.directory.clone(), |a| a.directory.clone());
        let applied = fanout::apply(
            &self.workspace,
            &self.directory.join("snapshot.index"),
            &current,
            &changes.files,
            &changes.patch,
            &scratch,
        )?;
        let outcome = match &applied {
            fanout::Applied::Clean | fanout::Applied::Merged => Ok(()),
            fanout::Applied::Conflict(files) => Err(format!(
                "Candidate {id}'s changes conflict with workspace changes made since the candidates started ({}). The workspace is unchanged. The candidate's worktree stays at {} and its patch at {}: apply it yourself (git apply --3way <patch>), or undo the conflicting changes and retry this stage",
                files.join(", "),
                worktree.display(),
                changes.patch.display()
            )),
        };
        if outcome.is_ok() {
            handoff.decisions.push(format!(
                "Chosen by {by} from fan-out candidates {ids}{}",
                if applied == fanout::Applied::Merged {
                    "; merged with workspace changes"
                } else {
                    ""
                }
            ));
            if let Some(attempt) = self.stages[fan].attempts.last() {
                storage::write_json(&attempt.directory.join("handoff.json"), &handoff)?;
            }
            self.stages[fan].handoff = Some(handoff);
        }
        if let Some(state) = self.stages[fan].fan_out.as_mut() {
            state.selection = Some(Selection {
                candidate: id,
                by: by.into(),
                reason: storage::truncate(reason, 2000),
                applied: outcome.is_ok(),
                at: Utc::now(),
            });
        }
        Ok(outcome)
    }

    /// After stages ran side by side: applies a held repair request, then
    /// stops the run at the first stopped stage, waits at a checkpoint, or
    /// returns it to pending.
    fn settle(&mut self) -> Result<()> {
        let hold = self.hold.take().unwrap_or_default();
        if let Some((review, target, feedback, reason)) = hold.repair {
            self.repair_feedback = Some(feedback);
            self.repair_review = Some(review);
            self.repair_passes += 1;
            // Invalidate all downstream handoffs, including previously passing reviews.
            self.reset(target, Actor::Engine, &reason)?;
            self.cursor = target;
        }
        if let Some(index) = (0..self.stages.len()).find(|&i| self.stages[i].status.stopped()) {
            let status = self.stages[index].status;
            let message = hold
                .stops
                .iter()
                .find(|(i, _)| *i == index)
                .map(|(_, m)| m.clone())
                .or_else(|| {
                    self.stages[index]
                        .attempts
                        .last()
                        .and_then(|a| a.error.clone())
                })
                .unwrap_or_else(|| {
                    format!(
                        "Stage {} {}",
                        self.workflow.stages[index].name,
                        status.label()
                    )
                });
            self.cursor = index;
            self.error = Some(message.clone());
            return self.set(Target::Run, status, Actor::Engine, message);
        }
        if let Some(index) = self
            .stages
            .iter()
            .position(|s| s.status == Status::AwaitingConfirmation)
        {
            self.cursor = index;
            return self.set(
                Target::Run,
                Status::AwaitingConfirmation,
                Actor::Engine,
                "Stage awaits acceptance",
            );
        }
        self.cursor = self.focus();
        self.error = None;
        self.set(
            Target::Run,
            Status::Pending,
            Actor::Engine,
            "Side-by-side stages ended",
        )
    }

    /// Recovers a graph run whose router died: every running candidate,
    /// attempt, and stage failed.
    fn recover(&mut self) -> Result<()> {
        self.interrupt_candidates(INTERRUPTED)?;
        let running: Vec<usize> = (0..self.stages.len())
            .filter(|&i| self.stages[i].status == Status::Running)
            .collect();
        let stopped = if running.is_empty() {
            vec![self.focus()]
        } else {
            running
        };
        let count = self.stages.len();
        for index in stopped.iter().copied().filter(|&i| i < count) {
            if self.stages[index]
                .attempts
                .last()
                .is_some_and(|a| a.status == Status::Running)
            {
                self.set(
                    Target::Attempt(index),
                    Status::Failed,
                    Actor::Recovery,
                    INTERRUPTED,
                )?;
                let attempt = self.stages[index]
                    .attempts
                    .last_mut()
                    .context("No attempt")?;
                attempt.error = Some(INTERRUPTED.into());
                attempt.finished_at.get_or_insert_with(Utc::now);
            }
            self.set(
                Target::Stage(index),
                Status::Failed,
                Actor::Recovery,
                INTERRUPTED,
            )?;
        }
        self.cursor = stopped[0].min(self.stages.len());
        self.set(Target::Run, Status::Failed, Actor::Recovery, INTERRUPTED)?;
        self.error = Some(INTERRUPTED.into());
        Ok(())
    }
}

/// Auto judge: a complete candidate whose required checks passed, then the
/// fewest changed lines, then the first listed.
fn auto_pick(state: &FanOutState) -> Option<usize> {
    state
        .candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| c.status == Status::Complete)
        .min_by_key(|(index, c)| {
            (
                !c.checks_pass(),
                c.changes.as_ref().map_or(u64::MAX, CandidateChanges::lines),
                *index,
            )
        })
        .map(|(index, _)| index)
}

/// What the judge, or a candidate, needs to know beyond the stage prompt.
fn fan_out_context(run: &Run) -> String {
    let index = run.cursor;
    if let Some(c) = run.view {
        let Some(state) = run.stages.get(index).and_then(|s| s.fan_out.as_ref()) else {
            return String::new();
        };
        return format!(
            "\nFAN-OUT CANDIDATE\nYou are candidate {}, one of {} candidates for this stage, each working in its own Git worktree started from the same workspace state. The working directory above is your worktree: change files only there, and do not commit. A judge compares the candidates, and the router applies only the chosen candidate's changes to the main workspace.\n",
            state.candidates[c].id,
            state.candidates.len()
        );
    }
    let Some(fan) = graph::judged_by(&run.workflow, index) else {
        return String::new();
    };
    let Some(state) = run.stages[fan].fan_out.as_ref() else {
        return String::new();
    };
    let mut text = format!(
        "\nFAN-OUT CANDIDATES\nStage {:?} ran {} candidates, each in its own Git worktree from the same starting point. Compare them against the original task and choose one; the router then applies only the chosen candidate's changes to the working directory. Inspect worktrees and patches read-only; do not modify them or the working directory.\n",
        run.workflow.stages[fan].name,
        state.candidates.len()
    );
    for candidate in &state.candidates {
        text.push_str(&format!(
            "\n- {} ({}{}{}): {}",
            candidate.id,
            candidate.harness,
            if candidate.model.is_empty() { "" } else { " " },
            candidate.model,
            candidate.status.label()
        ));
        if let Some(error) = &candidate.error {
            text.push_str(&format!(
                " — {}",
                storage::truncate(error.lines().next().unwrap_or(""), 300)
            ));
        }
        if let Some(attempt) = candidate.attempts.last() {
            if let Some(worktree) = &attempt.worktree {
                text.push_str(&format!("\n  worktree: {}", worktree.display()));
            }
            let required: Vec<_> = attempt.checks.iter().filter(|c| c.required).collect();
            if !required.is_empty() {
                text.push_str(&format!(
                    "\n  router checks: {}/{} required passed",
                    required
                        .iter()
                        .filter(|c| c.status == TestStatus::Passed)
                        .count(),
                    required.len()
                ));
            }
        }
        if let Some(changes) = &candidate.changes {
            text.push_str(&format!(
                "\n  changes: {} files, +{} −{}; patch: {}",
                changes.files.len(),
                changes.added,
                changes.removed,
                changes.patch.display()
            ));
        }
        if let Some(handoff) = &candidate.handoff {
            text.push_str(&format!(
                "\n  handoff: {}",
                storage::truncate(handoff.summary.trim(), 800).replace('\n', "\n    ")
            ));
        }
    }
    text.push_str("\n\nSet \"selected\" in your handoff to the chosen candidate's ID, for example \"selected\":\"c1\", with verdict pass, and explain the choice in summary. Choose only a complete candidate. Use verdict blocked if none is acceptable.\n");
    text
}

/// A harness process the scheduler runs: a stage attempt or a candidate's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Job {
    Stage(usize),
    Candidate(usize, usize),
}
enum Message {
    Spawned(Job, u32),
    Output(Job, bool, String),
    Done(Job, Result<process::Outcome>),
}
/// What the scheduler starts next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Launch {
    Stage(usize),
    FanOut(usize),
    Candidate(usize, usize),
    Interactive(usize),
}

/// The next thing to start, in list order, and a writer stage waiting to
/// run alone. Stages start only when every stage they run after is done;
/// parallel-safe ones run together up to `max_parallel` processes, others
/// alone, and interactive ones alone with the run handed to their terminal.
/// `only` limits it to that fan-out stage's candidates.
fn next_launch(run: &Run, only: Option<usize>) -> (Option<Launch>, Option<usize>) {
    let stages = &run.workflow.stages;
    let max = run.workflow.max_parallel.max(1) as usize;
    let mut flight = 0;
    let mut writer = false;
    for (stage, state) in stages.iter().zip(&run.stages) {
        if let Some(fan) = &state.fan_out {
            flight += fan
                .candidates
                .iter()
                .filter(|c| c.status == Status::Running)
                .count();
        }
        if state.status == Status::Running && stage.fan_out.is_none() {
            flight += 1;
            writer |= !graph::parallel_safe(stage);
        }
    }
    let pending = |index: usize| {
        run.stages[index].fan_out.as_ref().and_then(|f| {
            f.candidates
                .iter()
                .position(|c| c.status == Status::Pending)
        })
    };
    if let Some(index) = only {
        return (
            pending(index)
                .filter(|_| flight < max)
                .map(|c| Launch::Candidate(index, c)),
            None,
        );
    }
    if writer {
        return (None, None);
    }
    let order = run.order();
    for (index, stage) in stages.iter().enumerate() {
        let status = run.stages[index].status;
        if !stage.enabled {
            continue;
        }
        if stage.fan_out.is_some() && status == Status::Running {
            if let Some(c) = pending(index).filter(|_| flight < max) {
                return (Some(Launch::Candidate(index, c)), None);
            }
            continue;
        }
        let ready = status == Status::Pending
            && (0..stages.len()).all(|before| {
                !order.precedes(before, index)
                    || matches!(
                        run.stages[before].status,
                        Status::Complete | Status::Skipped
                    )
            });
        if !ready {
            continue;
        }
        if stage.mode == Mode::Interactive {
            return ((flight == 0).then_some(Launch::Interactive(index)), None);
        }
        if graph::parallel_safe(stage) {
            if flight >= max {
                return (None, None);
            }
            return (
                Some(if stage.fan_out.is_some() {
                    Launch::FanOut(index)
                } else {
                    Launch::Stage(index)
                }),
                None,
            );
        }
        return if flight == 0 {
            (Some(Launch::Stage(index)), None)
        } else {
            (None, Some(index))
        };
    }
    (None, None)
}

/// Runs stages and candidates side by side until none can start and none
/// runs, then settles the run. Each process has its own process group and
/// logs; workers only supervise processes and report over a channel, so
/// only this thread changes or saves the run. Returns an interactive stage
/// that should run next, and how many processes or rounds started.
fn schedule(
    run: &mut Run,
    cancel: &AtomicBool,
    emit: &mut impl FnMut(Event),
    only: Option<usize>,
) -> Result<(Option<usize>, usize)> {
    let mut interactive = None;
    let mut started = 0;
    let mut noted = vec![];
    let halted = |run: &Run| {
        cancel.load(Ordering::Relaxed)
            || run
                .hold
                .as_ref()
                .is_some_and(|h| h.repair.is_some() || !h.stops.is_empty())
            || run
                .stages
                .iter()
                .any(|s| s.status == Status::AwaitingConfirmation)
    };
    let result = thread::scope(|scope| -> Result<()> {
        let result = (|| -> Result<()> {
            let (tx, rx) = mpsc::channel::<Message>();
            let mut flight: Vec<(Job, Prepared, PathBuf)> = vec![];
            let mut formatters = std::collections::HashMap::new();
            let mut speaking = None;
            loop {
                while !halted(run) {
                    let (launch, waiting) = next_launch(run, only);
                    if let Some(index) = waiting.filter(|i| !noted.contains(i)) {
                        noted.push(index);
                        emit(Event::Output(format!(
                            "Stage {} changes the shared workspace: it waits for the running stages, then runs alone\n",
                            run.workflow.stages[index].name
                        )));
                    }
                    let Some(launch) = launch else {
                        break;
                    };
                    started += 1;
                    let (job, prepared, cwd) = match launch {
                        Launch::Interactive(index) => {
                            interactive = Some(index);
                            break;
                        }
                        Launch::FanOut(index) => {
                            run.cursor = index;
                            let attempts = run.stages[index].attempts.len();
                            if let Err(error) = run.start_fan_out(index) {
                                let recorded = run.stages[index].attempts.len() > attempts;
                                run.stop(
                                    Status::Failed,
                                    format!("{error:#}"),
                                    Actor::Engine,
                                    recorded,
                                )?;
                                run.persist()?;
                            }
                            emit(Event::State(Box::new(run.clone())));
                            continue;
                        }
                        Launch::Stage(index) => {
                            run.cursor = index;
                            match start_stage(run, emit)? {
                                Some(prepared) => {
                                    (Job::Stage(index), prepared, run.workspace.clone())
                                }
                                None => continue,
                            }
                        }
                        Launch::Candidate(index, c) => match run.launch_candidate(index, c) {
                            Ok((prepared, cwd)) => {
                                emit(Event::State(Box::new(run.clone())));
                                (Job::Candidate(index, c), prepared, cwd)
                            }
                            Err(error) => {
                                run.stop_candidate(
                                    index,
                                    c,
                                    Status::Failed,
                                    format!("{error:#}"),
                                    Actor::Engine,
                                )?;
                                if run.fan_out_done(index) {
                                    run.settle_fan_out(index, false)?;
                                }
                                run.persist()?;
                                emit(Event::State(Box::new(run.clone())));
                                continue;
                            }
                        },
                    };
                    let (index, label) = match job {
                        Job::Stage(index) => (index, run.workflow.stages[index].name.clone()),
                        Job::Candidate(index, c) => (
                            index,
                            format!(
                                "{} · candidate {}",
                                run.workflow.stages[index].name,
                                run.candidate(index, c).id
                            ),
                        ),
                    };
                    if let Job::Candidate(..) = job {
                        let state = match job {
                            Job::Candidate(i, c) => run.candidate(i, c),
                            Job::Stage(_) => unreachable!(),
                        };
                        emit(Event::Output(format!(
                            "── {label} · attempt {} · {}{}{} ──\n",
                            state.attempts.len(),
                            state.harness,
                            if state.model.is_empty() { "" } else { " " },
                            state.model
                        )));
                    }
                    speaking = Some(job);
                    formatters.insert(
                        job,
                        (label, events::Formatter::new(prepared.invocation.kind)),
                    );
                    let timeout = Duration::from_secs(run.workflow.stages[index].timeout_seconds);
                    let invocation = prepared.invocation.clone();
                    let sandbox = prepared.sandbox.clone();
                    let directory = prepared.directory.clone();
                    let worker_cwd = cwd.clone();
                    let tx = tx.clone();
                    scope.spawn(move || {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            process::capture_with(
                                &invocation,
                                &worker_cwd,
                                &directory,
                                timeout,
                                cancel,
                                sandbox.as_ref(),
                                |pgid| {
                                    let _ = tx.send(Message::Spawned(job, pgid));
                                },
                                |err, text| {
                                    let _ = tx.send(Message::Output(job, err, text.to_owned()));
                                },
                            )
                        }))
                        .unwrap_or_else(|_| {
                            Err(anyhow::anyhow!("The harness supervisor panicked"))
                        });
                        let _ = tx.send(Message::Done(job, result));
                    });
                    flight.push((job, prepared, cwd));
                }
                if flight.is_empty() {
                    break;
                }
                let message = rx.recv().context("Harness workers stopped")?;
                match message {
                    Message::Spawned(job, pgid) => {
                        // Recorded so a later drive can stop the group if mr dies meanwhile.
                        let attempt = match job {
                            Job::Stage(i) => run.stages[i].attempts.last_mut(),
                            Job::Candidate(i, c) => run.candidate_mut(i, c).attempts.last_mut(),
                        };
                        if let Some(attempt) = attempt {
                            attempt.process_group = Some(pgid);
                        }
                        let _ = run.persist();
                    }
                    Message::Output(job, err, text) => {
                        let Some((label, formatter)) = formatters.get_mut(&job) else {
                            continue;
                        };
                        let lines = if err {
                            Some(format!("[stderr] {}", storage::clean_terminal(&text)))
                        } else {
                            formatter.push(&text)
                        };
                        if let Some(lines) = lines {
                            if speaking != Some(job) {
                                speaking = Some(job);
                                emit(Event::Output(format!("── {label} ──\n")));
                            }
                            emit(Event::Output(lines));
                        }
                    }
                    Message::Done(job, result) => {
                        let Some(position) = flight.iter().position(|(j, ..)| *j == job) else {
                            continue;
                        };
                        let (_, prepared, cwd) = flight.swap_remove(position);
                        if let Some((label, mut formatter)) = formatters.remove(&job) {
                            if let Some(rest) = formatter.finish() {
                                if speaking != Some(job) {
                                    emit(Event::Output(format!("── {label} ──\n")));
                                }
                                emit(Event::Output(rest));
                            }
                            speaking = None;
                        }
                        match job {
                            Job::Stage(index) => {
                                run.cursor = index;
                                conclude(run, &prepared, result, cancel, emit)?;
                            }
                            Job::Candidate(index, c) => {
                                run.cursor = index;
                                if let Err(error) = run.finish_candidate(
                                    (index, c),
                                    &prepared,
                                    &cwd,
                                    result,
                                    cancel,
                                    emit,
                                ) {
                                    run.stop_candidate(
                                        index,
                                        c,
                                        Status::Failed,
                                        format!(
                                            "Could not complete candidate bookkeeping: {error:#}"
                                        ),
                                        Actor::Engine,
                                    )?;
                                }
                                if run.fan_out_done(index) {
                                    run.settle_fan_out(index, cancel.load(Ordering::Relaxed))?;
                                }
                                run.persist()?;
                                emit(Event::State(Box::new(run.clone())));
                            }
                        }
                    }
                }
            }
            Ok(())
        })();
        if result.is_err() {
            // Running harnesses end promptly; the scope waits for them.
            cancel.store(true, Ordering::Relaxed);
        }
        result
    });
    result?;
    // Rounds the run stopped before every candidate started.
    for index in 0..run.stages.len() {
        if only.is_some_and(|only| only != index)
            || run.stages[index].status != Status::Running
            || run.stages[index].fan_out.is_none()
        {
            continue;
        }
        let count = run.stages[index]
            .fan_out
            .as_ref()
            .map_or(0, |f| f.candidates.len());
        for c in 0..count {
            if run.candidate(index, c).status == Status::Pending {
                run.stop_candidate(
                    index,
                    c,
                    Status::Cancelled,
                    "Not started: the run stopped first".into(),
                    Actor::Engine,
                )?;
            }
        }
        run.settle_fan_out(index, true)?;
    }
    if only.is_none() {
        run.settle()?;
    }
    run.persist()?;
    emit(Event::State(Box::new(run.clone())));
    Ok((interactive, started))
}

/// Runs the current fan-out stage of a list workflow: its candidates side
/// by side, then its judgement.
fn execute_fan_out(run: &mut Run, cancel: &AtomicBool, emit: &mut impl FnMut(Event)) -> Result<()> {
    let index = run.cursor;
    let attempts = run.stages[index].attempts.len();
    if let Err(error) = run.start_fan_out(index) {
        let recorded = run.stages[index].attempts.len() > attempts;
        run.stop(
            Status::Failed,
            format!("{error:#}"),
            Actor::Engine,
            recorded,
        )?;
        run.persist()?;
        emit(Event::State(Box::new(run.clone())));
        return Ok(());
    }
    emit(Event::State(Box::new(run.clone())));
    schedule(run, cancel, emit, Some(index))?;
    Ok(())
}

/// `drive` for a dependency graph: ready stages side by side where safe.
fn drive_graph(run: &mut Run, cancel: &AtomicBool, emit: &mut impl FnMut(Event)) -> Result<Action> {
    loop {
        match run.status {
            Status::AwaitingConfirmation => return Ok(Action::Confirm),
            Status::Interactive => return Ok(Action::Interactive),
            Status::Failed | Status::Blocked | Status::Cancelled => {
                save_changes(run);
                run.persist()?;
                return Ok(Action::Stopped);
            }
            Status::Running => {
                run.recover()?;
                run.persist()?;
                return Ok(Action::Stopped);
            }
            _ => {}
        }
        if run
            .stages
            .iter()
            .all(|s| matches!(s.status, Status::Complete | Status::Skipped))
        {
            run.cursor = run.stages.len();
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
            run.cursor = run.focus();
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
        run.hold = Some(Hold::default());
        let scheduled = schedule(run, cancel, emit, None);
        run.hold = None;
        let (interactive, started) = scheduled?;
        if run.status == Status::Pending {
            if let Some(index) = interactive {
                run.cursor = index;
                let reason = format!(
                    "Stage {} waits for its terminal",
                    run.workflow.stages[index].name
                );
                run.set(Target::Run, Status::Interactive, Actor::Engine, reason)?;
                run.persist()?;
                emit(Event::State(Box::new(run.clone())));
                return Ok(Action::Interactive);
            }
            ensure!(
                started > 0,
                "No stage can start; inspect the run's stage states"
            );
        }
    }
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
        let candidates = state
            .fan_out
            .iter_mut()
            .flat_map(|f| &mut f.candidates)
            .flat_map(|c| &mut c.attempts);
        for attempt in state.attempts.iter_mut().chain(candidates) {
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
    if run.graph() {
        return drive_graph(run, cancel, &mut emit);
    }
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
                run.interrupt_candidates(INTERRUPTED)?;
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
        if run.workflow.stages[run.cursor].role == Role::Ship {
            crate::ship::execute(run, &mut emit)?;
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
        if run.workflow.stages[run.cursor].fan_out.is_some() {
            execute_fan_out(run, cancel, &mut emit)?;
        } else {
            execute_stage(run, cancel, None, &mut emit)?;
        }
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
        if stage.role == Role::Ship {
            let options = stage.ship.clone().unwrap_or_default();
            text.push_str(&format!(
                "{}. {} | ship | router, no harness\n   {}\n",
                index + 1,
                stage.name,
                options.describe()
            ));
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
                max_repair_passes: 0,
                ..Workflow::new(
                    "Interactive",
                    vec![
                        Stage {
                            harness: "mock".into(),
                            mode: Mode::Interactive,
                            ..stage
                        },
                        next,
                    ],
                )
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
