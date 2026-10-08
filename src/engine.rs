use crate::providers::{self, Provider};
use crate::{
    adapters::{self, ContextPaths, Invocation},
    checks::{self, CheckResult},
    config::{CustomAdapter, Mode, Role, Settings, Workflow},
    events,
    handoff::{self, Handoff, TestStatus, Verdict},
    process, snapshot, storage,
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
    pub repair_feedback: Option<String>,
    pub status: Status,
    pub error: Option<String>,
    /// The next interactive attempt resumes this harness session instead of
    /// starting a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<Resume>,
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

/// Holds applied to a stage's handoff before review repairs and advancing.
enum Gate {
    Block(String),
    Waiver,
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
            repair_feedback: None,
            status: Status::Pending,
            error: None,
            resume: None,
        };
        run.persist()?;
        Ok(run)
    }

    /// Saves run.json, then the summary that history listing reads instead.
    pub fn persist(&mut self) -> Result<()> {
        self.revision += 1;
        self.updated_at = Utc::now();
        storage::write_json(&self.directory.join("run.json"), self)?;
        storage::write_json(
            &self.directory.join("summary.json"),
            &crate::history::RunSummary::from(&*self),
        )
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
    pub fn retry(&mut self, interactive: bool, model: Option<String>) -> Result<()> {
        ensure!(
            self.cursor < self.stages.len(),
            "This run is already complete"
        );
        if let Some(attempt) = self.stages[self.cursor].attempts.last_mut() {
            if attempt.status == Status::Running {
                attempt.status = Status::Failed;
                attempt.finished_at = Some(Utc::now());
                attempt.error = Some(
                    "Previous execution was interrupted; retry was explicitly requested".into(),
                );
            }
        }
        if interactive {
            self.workflow.stages[self.cursor].mode = Mode::Interactive;
        }
        if let Some(model) = model {
            self.workflow.stages[self.cursor].model = model;
        }
        self.reset(self.cursor);
        self.status = Status::Pending;
        self.error = None;
        self.resume = None;
        Ok(())
    }

    /// Reopens the current stage's last session in the harness's native
    /// interactive CLI: an unattended attempt, or an interactive one awaiting
    /// review. The stage must have stopped; filesystem changes are kept, as
    /// with retry.
    pub fn take_over(&mut self) -> Result<()> {
        ensure!(
            self.cursor < self.stages.len(),
            "This run is already complete"
        );
        ensure!(
            matches!(
                self.status,
                Status::Failed | Status::Blocked | Status::Cancelled | Status::AwaitingConfirmation
            ),
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
        self.resume = Some(Resume {
            session,
            handoff_file: attempt.handoff_file.clone(),
        });
        self.workflow.stages[self.cursor].mode = Mode::Interactive;
        self.reset(self.cursor);
        self.status = Status::Interactive;
        self.error = None;
        Ok(())
    }

    /// Return stages from `from` onward to pending, discarding handoffs and acceptances.
    fn reset(&mut self, from: usize) {
        for i in from..self.stages.len() {
            let state = &mut self.stages[i];
            state.status = if self.workflow.stages[i].enabled {
                Status::Pending
            } else {
                Status::Skipped
            };
            state.handoff = None;
            state.waiver = None;
        }
    }

    /// Confirming a checkpoint also accepts its pending waiver; it never needs a second step.
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
        self.stages[self.cursor].status = Status::Complete;
        if let Some(attempt) = self.stages[self.cursor].attempts.last_mut() {
            attempt.status = Status::Complete;
        }
        self.apply_handoff()?;
        Ok(())
    }

    /// The not-applicable reason when this checkpoint awaits acceptance of a test
    /// stage without verification evidence.
    pub fn pending_waiver(&self) -> Option<&str> {
        if self.status != Status::AwaitingConfirmation || !matches!(self.gate(), Some(Gate::Waiver))
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

    fn gate(&self) -> Option<Gate> {
        let stage = self.workflow.stages.get(self.cursor)?;
        let state = &self.stages[self.cursor];
        let handoff = state.handoff.as_ref()?;
        if handoff.verdict == Verdict::Blocked {
            return Some(Gate::Block(format!(
                "Stage reports a block: {}",
                handoff.summary
            )));
        }
        let checks = state.attempts.last().map_or(&[][..], |a| &a.checks);
        let mut failed = checks
            .iter()
            .filter(|c| c.required && c.status != TestStatus::Passed);
        if let Some(check) = failed.next() {
            let more = failed.count();
            return Some(Gate::Block(format!(
                "Required check failed: {} ({}){}. Logs: {}",
                shell_words::join(&check.command),
                check.outcome(),
                if more > 0 {
                    format!("; {more} more failed")
                } else {
                    String::new()
                },
                check.directory.display()
            )));
        }
        if stage.role != Role::Test {
            return None;
        }
        if handoff.requires_repair()
            || handoff
                .tests
                .iter()
                .any(|test| test.required && test.status == TestStatus::NotRun)
        {
            return Some(Gate::Block("Test stage reports incomplete verification; inspect its findings and required checks before retrying".into()));
        }
        if handoff.has_passing_required_check()
            || checks
                .iter()
                .any(|c| c.required && c.status == TestStatus::Passed)
            || state.waiver.is_some()
        {
            return None;
        }
        Some(match handoff.not_applicable_reason() {
            Some(_) => Gate::Waiver,
            None => Gate::Block("Test stage reported no passing required check. Retry it with evidence, or have it report not_applicable with a reason, which requires your explicit acceptance".into()),
        })
    }

    fn apply_handoff(&mut self) -> Result<()> {
        ensure!(
            self.stages[self.cursor].handoff.is_some(),
            "Missing handoff"
        );
        match self.gate() {
            Some(Gate::Block(message)) => {
                self.fail(Status::Blocked, message);
                return Ok(());
            }
            Some(Gate::Waiver) => {
                self.stages[self.cursor].status = Status::AwaitingConfirmation;
                self.status = Status::AwaitingConfirmation;
                return Ok(());
            }
            None => {}
        }
        let stage = &self.workflow.stages[self.cursor];
        let handoff = self.stages[self.cursor]
            .handoff
            .as_ref()
            .context("Missing handoff")?;
        if stage.role == Role::Review && handoff.requires_repair() {
            let feedback = format!(
                "Review {} requested changes:\n{}",
                stage.name,
                serde_json::to_string_pretty(handoff)?
            );
            if self.repair_passes >= self.workflow.max_repair_passes {
                self.fail(Status::Blocked, format!("Review still requests changes after {} repair passes. Inspect findings and continue explicitly", self.repair_passes));
                return Ok(());
            }
            let target = self.workflow.stages[..self.cursor].iter().rposition(|s| {
                s.enabled
                    && s.role == Role::Implementation
                    && (stage.repair_target.is_empty() || s.id == stage.repair_target)
            });
            let Some(target) = target else {
                self.fail(
                    Status::Blocked,
                    "Review requested changes, but no enabled upstream implementation stage exists"
                        .into(),
                );
                return Ok(());
            };
            self.repair_feedback = Some(feedback);
            self.repair_passes += 1;
            // Invalidate all downstream handoffs, including previously passing reviews.
            self.reset(target);
            self.cursor = target;
            self.status = Status::Pending;
            return Ok(());
        }
        if handoff.verdict == Verdict::NeedsChanges {
            self.fail(
                Status::Blocked,
                "Stage reports incomplete work; inspect its handoff and retry".into(),
            );
            return Ok(());
        }
        self.stages[self.cursor].status = Status::Complete;
        self.cursor += 1;
        self.status = Status::Pending;
        Ok(())
    }

    fn fail(&mut self, status: Status, message: String) {
        self.status = status;
        self.error = Some(message.clone());
        if let Some(stage) = self.stages.get_mut(self.cursor) {
            stage.status = status;
            if let Some(attempt) = stage.attempts.last_mut() {
                attempt.status = status;
                attempt.error = Some(message);
                attempt.finished_at = Some(Utc::now());
            }
        }
    }
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

/// Every readable run, newest first. Listing should use `history::list`,
/// which reads summaries and reports damaged records.
pub fn history(project: &Path) -> Result<Vec<Run>> {
    Ok(crate::history::list(project)?
        .iter()
        .filter_map(|entry| load(project, entry.id()).ok())
        .collect())
}

pub fn prompt(run: &Run, handoff_path: &Path) -> String {
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
                let json = serde_json::to_string_pretty(handoff).unwrap_or_default();
                let excerpt = storage::truncate(&json, remaining.min(5000));
                remaining = remaining.saturating_sub(excerpt.len());
                text.push_str(&format!(
                    "\nPRIOR STAGE: {}\n{}\nFull artifacts: {}\n",
                    run.workflow.stages[index].name,
                    excerpt,
                    state
                        .attempts
                        .last()
                        .map(|a| a.directory.display().to_string())
                        .unwrap_or_default()
                ));
            }
        }
    }
    if let Some(feedback) = &run.repair_feedback {
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
    text.push_str(&format!("\nHANDOFF CONTRACT\nFinish with the JSON below between the exact markers. Replace example values with evidence. Also write the JSON object (without markers) to {} if your tools permit it. This file is router bookkeeping; research/review may write this file while leaving project code untouched. In interactive mode, complete this handoff before exiting.\n{}", handoff_path.display(), handoff::FORMAT));
    text
}

struct Prepared {
    invocation: Invocation,
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
        // The resumed session already knows its handoff path.
        Some(resume) => resume.handoff_file.clone(),
        None => handoff_dir.join(format!("{}-{number:03}.json", stage.id)),
    };
    ensure!(
        resume.is_some() || !handoff_file.exists(),
        "Handoff path already exists: {}",
        handoff_file.display()
    );
    let prompt_file = directory.join("prompt.md");
    let text = match &resume {
        Some(resume) => format!(
            "Interactive takeover of harness session {}. The session's own history holds the stage prompt; write its handoff to {}.\n",
            resume.session,
            handoff_file.display()
        ),
        None => prompt(run, &handoff_file),
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
    storage::write_json(
        &directory.join("command.json"),
        &serde_json::json!({ "program": invocation.program, "args": invocation.args, "stdin_from": invocation.stdin.as_ref().map(|_| "prompt.md"), "env": invocation.env.iter().map(|(name, _)| name).collect::<Vec<_>>(), "workspace": run.workspace, "model": stage.model, "provider": stage.provider, "effort": stage.effort, "mode": stage.mode }),
    )?;
    run.stages[index].attempts.push(Attempt {
        number,
        started_at: Utc::now(),
        finished_at: None,
        model: stage.model.clone(),
        mode: stage.mode,
        status: Status::Running,
        exit_code: None,
        directory: directory.clone(),
        handoff_file: handoff_file.clone(),
        error: None,
        checks: vec![],
        tree_before: snapshot::tree(&run.workspace, &run.directory),
        tree_after: None,
        usage: None,
        session,
    });
    run.resume = None;
    run.stages[index].status = Status::Running;
    run.status = Status::Running;
    run.error = None;
    run.persist()?;
    Ok(Prepared {
        invocation,
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

fn finish(
    run: &mut Run,
    prepared: &Prepared,
    result: Result<process::Outcome>,
    cancel: &AtomicBool,
    emit: &mut impl FnMut(Event),
) -> Result<()> {
    let index = run.cursor;
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            run.fail(Status::Failed, format!("{error:#}"));
            return Ok(());
        }
    };
    if let Some(attempt) = run.stages[index].attempts.last_mut() {
        attempt.exit_code = outcome.code;
        attempt.finished_at = Some(Utc::now());
        // Spend counts whether or not the stage succeeded. Usage events can be
        // spread through the log, so read far more than the final response needs.
        if attempt.mode == Mode::Unattended {
            attempt.usage = process::read_tail(&prepared.directory.join("stdout.log"), 64 << 20)
                .ok()
                .and_then(|raw| handoff::usage(&raw, prepared.invocation.kind));
        }
    }
    if outcome.cancelled {
        run.fail(
            Status::Cancelled,
            "Stage cancelled. Filesystem changes are retained; retry explicitly".into(),
        );
        return Ok(());
    }
    if outcome.timed_out {
        run.fail(
            Status::Failed,
            "Stage timed out. Inspect logs or reopen interactively; existing changes are retained"
                .into(),
        );
        return Ok(());
    }
    if !outcome.success() {
        // A JSON error event names the cause; stderr often holds only warnings.
        let reported = (run.workflow.stages[index].mode == Mode::Unattended)
            .then(|| {
                process::read_tail(&prepared.directory.join("stdout.log"), 4 * 1024 * 1024).ok()
            })
            .flatten()
            .and_then(|raw| handoff::normalize(&raw, prepared.invocation.kind).error);
        let detail = match reported {
            Some(error) => error,
            None => storage::clean_terminal(
                &process::read_tail(&prepared.directory.join("stderr.log"), 3000)
                    .unwrap_or_default(),
            ),
        };
        run.fail(
            Status::Failed,
            format!("Harness exited with code {:?}. {detail}", outcome.code),
        );
        return Ok(());
    }
    let (mode, role) = (
        run.workflow.stages[index].mode,
        run.workflow.stages[index].role,
    );
    let raw = process::read_tail(&prepared.directory.join("stdout.log"), 4 * 1024 * 1024)?;
    let normalized = if mode == Mode::Interactive {
        handoff::Normalized {
            text: storage::clean_terminal(&raw),
            ..handoff::Normalized::default()
        }
    } else {
        handoff::normalize(&raw, prepared.invocation.kind)
    };
    if normalized.permission_blocked {
        run.fail(Status::Blocked, "Harness reported denied tool permissions. Reopen interactively or set explicit permission arguments and retry".into());
        return Ok(());
    }
    // A handoff file may replace a missing assistant text event, but not a real
    // protocol error (e.g. a tool/API failure after the file was written).
    if let Some(error) = &normalized.error {
        if !error.starts_with("No final assistant")
            || (!prepared.handoff_file.is_file() && !prepared.invocation.final_file.is_file())
        {
            run.fail(Status::Failed, error.clone());
            return Ok(());
        }
    }
    // Interactive terminal transcripts can echo the original prompt (including
    // the example JSON). Never infer a structured verdict from an echoed prompt.
    if mode == Mode::Interactive && !prepared.handoff_file.is_file() {
        if matches!(role, Role::Review | Role::Test) {
            run.fail(Status::Blocked, format!("Interactive {} did not write its handoff file. Reopen it to produce a structured verdict and evidence", role.label()));
            return Ok(());
        }
        let handoff = Handoff {
            summary: format!(
                "Interactive stage exited without a structured handoff. Inspect the terminal transcript at {}. Reopen to produce a handoff, or supply a summary before advancing.",
                prepared.directory.join("stdout.log").display()
            ),
            verdict: Verdict::Complete,
            findings: vec![],
            decisions: vec![],
            files_changed: vec![],
            tests: vec![],
            unresolved: vec!["No model-authored handoff was produced".into()],
            not_applicable: None,
            structured: false,
        };
        storage::write_json(&prepared.directory.join("handoff.json"), &handoff)?;
        run.stages[index].handoff = Some(handoff);
        if !run_checks(run, prepared, cancel, emit)? {
            return Ok(());
        }
        run.stages[index].status = Status::AwaitingConfirmation;
        run.status = Status::AwaitingConfirmation;
        if let Some(attempt) = run.stages[index].attempts.last_mut() {
            attempt.status = Status::AwaitingConfirmation;
        }
        return Ok(());
    }
    let source = if prepared.handoff_file.is_file() {
        process::read_tail(&prepared.handoff_file, 1024 * 1024)?
    } else if prepared.invocation.final_file.is_file() {
        process::read_tail(&prepared.invocation.final_file, 1024 * 1024)?
    } else {
        normalized.text
    };
    let handoff = match handoff::parse(&source, role) {
        Ok(handoff) => handoff,
        Err(error) => {
            run.fail(Status::Blocked, format!("{error:#}"));
            return Ok(());
        }
    };
    storage::write_json(&prepared.directory.join("handoff.json"), &handoff)?;
    if role == Role::Summary {
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
    run.stages[index].status = Status::Complete;
    if let Some(attempt) = run.stages[index].attempts.last_mut() {
        attempt.status = Status::Complete;
    }
    if mode == Mode::Interactive {
        run.status = Status::AwaitingConfirmation;
        run.stages[index].status = Status::AwaitingConfirmation;
    } else {
        run.apply_handoff()?;
    }
    Ok(())
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
        run.fail(
            Status::Cancelled,
            "Stage cancelled during checks. Filesystem changes are retained; retry explicitly"
                .into(),
        );
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
    let prepared = match prepare(run) {
        Ok(p) => p,
        Err(e) => {
            run.fail(Status::Failed, format!("{e:#}"));
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
    let result = if let Some(session) = interactive {
        process::interactive(
            &prepared.invocation,
            &run.workspace,
            &prepared.directory,
            cancel,
            session,
        )
    } else {
        // Events become readable lines; stdout.log keeps them raw.
        let mut formatter = events::Formatter::new(prepared.invocation.kind);
        let result = process::capture(
            &prepared.invocation,
            &run.workspace,
            &prepared.directory,
            Duration::from_secs(run.workflow.stages[run.cursor].timeout_seconds),
            cancel,
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
        run.fail(
            Status::Failed,
            format!("Could not complete stage bookkeeping: {error:#}"),
        );
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

/// Drive until completion, failure, or a terminal/confirmation is required.
pub fn drive(run: &mut Run, cancel: &AtomicBool, mut emit: impl FnMut(Event)) -> Result<Action> {
    let _lock = storage::lock(&run.directory.join("run.lock"))?;
    let _workspace_lock =
        storage::lock(&storage::metadata_dir(&run.workspace)?.join("workspace.lock"))?;
    run.check_revision()?;
    loop {
        match run.status {
            Status::AwaitingConfirmation => return Ok(Action::Confirm),
            Status::Failed | Status::Blocked | Status::Cancelled => {
                save_changes(run);
                run.persist()?;
                return Ok(Action::Stopped);
            }
            Status::Running => {
                run.fail(
                    Status::Failed,
                    "A previous execution was interrupted. Inspect artifacts and retry explicitly"
                        .into(),
                );
                run.persist()?;
                return Ok(Action::Stopped);
            }
            _ => {}
        }
        while run.cursor < run.workflow.stages.len() && !run.workflow.stages[run.cursor].enabled {
            run.stages[run.cursor].status = Status::Skipped;
            run.cursor += 1;
        }
        if run.cursor == run.stages.len() {
            run.status = Status::Complete;
            run.error = None;
            run.persist()?;
            save_changes(run);
            emit(Event::State(Box::new(run.clone())));
            return Ok(Action::Complete);
        }
        if cancel.load(Ordering::Relaxed) {
            run.fail(
                Status::Cancelled,
                "Run cancelled before the next stage".into(),
            );
            run.persist()?;
            save_changes(run);
            return Ok(Action::Stopped);
        }
        if run.workflow.stages[run.cursor].mode == Mode::Interactive {
            run.status = Status::Interactive;
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
    let _lock = storage::lock(&run.directory.join("run.lock"))?;
    let _workspace_lock =
        storage::lock(&storage::metadata_dir(&run.workspace)?.join("workspace.lock"))?;
    run.check_revision()?;
    execute_stage(run, cancel, Some(session), &mut emit)?;
    if matches!(
        run.status,
        Status::Failed | Status::Blocked | Status::Cancelled
    ) {
        save_changes(run);
    }
    Ok(())
}

pub fn edit_handoff(run: &mut Run, summary: String) -> Result<()> {
    ensure!(
        run.status == Status::AwaitingConfirmation
            && run.workflow.stages[run.cursor].role != Role::Review,
        "Only an interactive non-review handoff summary can be edited"
    );
    ensure!(
        !summary.trim().is_empty() && summary.len() <= 12000,
        "Enter a summary of 1–12000 bytes"
    );
    let _lock = storage::lock(&run.directory.join("run.lock"))?;
    run.check_revision()?;
    let state = &mut run.stages[run.cursor];
    let handoff = state.handoff.as_mut().context("No handoff")?;
    handoff.summary = summary;
    handoff
        .decisions
        .push("Handoff summary supplied or edited by the user".into());
    if let Some(attempt) = state.attempts.last() {
        storage::write_json(&attempt.directory.join("handoff.json"), handoff)?;
    }
    run.persist()
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
