use crate::{
    adapters::{self, ContextPaths, Invocation},
    config::{CustomAdapter, Mode, Role, Settings, Workflow},
    handoff::{self, Handoff, Verdict},
    process, storage,
};
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
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
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageState {
    pub status: Status,
    pub attempts: Vec<Attempt>,
    pub handoff: Option<Handoff>,
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
    pub directory: PathBuf,
    pub worktree: bool,
    pub task: String,
    pub workflow: Workflow,
    pub adapters: BTreeMap<String, CustomAdapter>,
    pub stages: Vec<StageState>,
    pub cursor: usize,
    pub repair_passes: u32,
    pub repair_feedback: Option<String>,
    pub status: Status,
    pub error: Option<String>,
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

impl Run {
    pub fn create(
        project: &Path,
        settings: &Settings,
        workflow: &Workflow,
        task: String,
        worktree: bool,
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
        let workspace = if worktree {
            storage::create_worktree(&project, &id)?
        } else {
            project.clone()
        };
        let directory = storage::metadata_dir(&project)?.join("runs").join(&id);
        fs::create_dir_all(&directory)?;
        let now = Utc::now();
        let mut run = Self {
            version: 1,
            revision: 0,
            id,
            created_at: now,
            updated_at: now,
            project,
            workspace,
            directory,
            worktree,
            task,
            workflow: workflow.clone(),
            adapters: settings.adapters.clone(),
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
                })
                .collect(),
            cursor: 0,
            repair_passes: 0,
            repair_feedback: None,
            status: Status::Pending,
            error: None,
        };
        run.persist()?;
        Ok(run)
    }

    pub fn persist(&mut self) -> Result<()> {
        self.revision += 1;
        self.updated_at = Utc::now();
        storage::write_json(&self.directory.join("run.json"), self)
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
        for i in self.cursor..self.stages.len() {
            self.stages[i].status = if self.workflow.stages[i].enabled {
                Status::Pending
            } else {
                Status::Skipped
            };
            self.stages[i].handoff = None;
        }
        self.status = Status::Pending;
        self.error = None;
        Ok(())
    }

    pub fn confirm(&mut self) -> Result<()> {
        ensure!(
            self.status == Status::AwaitingConfirmation,
            "No interactive handoff awaits confirmation"
        );
        self.stages[self.cursor].status = Status::Complete;
        if let Some(attempt) = self.stages[self.cursor].attempts.last_mut() {
            attempt.status = Status::Complete;
        }
        self.apply_handoff()?;
        Ok(())
    }

    fn apply_handoff(&mut self) -> Result<()> {
        let stage = &self.workflow.stages[self.cursor];
        let handoff = self.stages[self.cursor]
            .handoff
            .as_ref()
            .context("Missing handoff")?;
        if handoff.verdict == Verdict::Blocked {
            self.fail(
                Status::Blocked,
                format!("Stage reports a block: {}", handoff.summary),
            );
            return Ok(());
        }
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
            for i in target..self.stages.len() {
                self.stages[i].status = if self.workflow.stages[i].enabled {
                    Status::Pending
                } else {
                    Status::Skipped
                };
                self.stages[i].handoff = None;
            }
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
            && run.stages.len() == run.workflow.stages.len()
            && run.cursor <= run.stages.len(),
        "Invalid or unsupported run record"
    );
    Ok(run)
}

pub fn history(project: &Path) -> Result<Vec<Run>> {
    let directory = project.join(".model-router/runs");
    if !directory.exists() {
        return Ok(vec![]);
    }
    let mut runs = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Ok(run) = storage::read_json::<Run>(&entry.path().join("run.json")) {
                runs.push(run);
            }
        }
    }
    runs.sort_by_key(|run| std::cmp::Reverse(run.created_at));
    Ok(runs)
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
    let handoff_file = handoff_dir.join(format!("{}-{number:03}.json", stage.id));
    ensure!(
        !handoff_file.exists(),
        "Handoff path already exists: {}",
        handoff_file.display()
    );
    let prompt_file = directory.join("prompt.md");
    let text = prompt(run, &handoff_file);
    storage::atomic_write(&prompt_file, text.as_bytes())?;
    let invocation = adapters::build(
        stage,
        &run.adapters,
        &text,
        &ContextPaths {
            workspace: &run.workspace,
            attempt_dir: &directory,
            prompt_file: &prompt_file,
            handoff_file: &handoff_file,
        },
    )?;
    storage::write_json(
        &directory.join("command.json"),
        &serde_json::json!({ "program": invocation.program, "args": invocation.args, "stdin_from": invocation.stdin.as_ref().map(|_| "prompt.md"), "workspace": run.workspace, "model": stage.model, "provider": stage.provider, "effort": stage.effort, "mode": stage.mode }),
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
    });
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

fn finish(run: &mut Run, prepared: &Prepared, result: Result<process::Outcome>) -> Result<()> {
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
        let stderr =
            process::read_tail(&prepared.directory.join("stderr.log"), 3000).unwrap_or_default();
        run.fail(
            Status::Failed,
            format!(
                "Harness exited with code {:?}. {}",
                outcome.code,
                storage::clean_terminal(&stderr)
            ),
        );
        return Ok(());
    }
    let stage = &run.workflow.stages[index];
    let raw = process::read_tail(&prepared.directory.join("stdout.log"), 4 * 1024 * 1024)?;
    let normalized = if stage.mode == Mode::Interactive {
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
    if stage.mode == Mode::Interactive && !prepared.handoff_file.is_file() {
        if stage.role == Role::Review {
            run.fail(Status::Blocked, "Interactive review did not write its handoff file. Reopen it to produce an explicit verdict".into());
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
            structured: false,
        };
        storage::write_json(&prepared.directory.join("handoff.json"), &handoff)?;
        run.stages[index].handoff = Some(handoff);
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
    let handoff = match handoff::parse(&source, stage.role) {
        Ok(handoff) => handoff,
        Err(error) => {
            run.fail(Status::Blocked, format!("{error:#}"));
            return Ok(());
        }
    };
    storage::write_json(&prepared.directory.join("handoff.json"), &handoff)?;
    run.stages[index].handoff = Some(handoff);
    run.stages[index].status = Status::Complete;
    if let Some(attempt) = run.stages[index].attempts.last_mut() {
        attempt.status = Status::Complete;
    }
    if stage.mode == Mode::Interactive {
        run.status = Status::AwaitingConfirmation;
        run.stages[index].status = Status::AwaitingConfirmation;
    } else {
        run.apply_handoff()?;
    }
    Ok(())
}

fn execute_stage(
    run: &mut Run,
    cancel: &AtomicBool,
    interactive: bool,
    emit: &mut impl FnMut(Event),
) -> Result<()> {
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
    let result = if interactive {
        process::interactive(
            &prepared.invocation,
            &run.workspace,
            &prepared.directory,
            cancel,
        )
    } else {
        process::capture(
            &prepared.invocation,
            &run.workspace,
            &prepared.directory,
            Duration::from_secs(run.workflow.stages[run.cursor].timeout_seconds),
            cancel,
            |err, text| {
                emit(Event::Output(format!(
                    "{}{}",
                    if err { "[stderr] " } else { "" },
                    storage::clean_terminal(text)
                )))
            },
        )
    };
    if let Err(error) = finish(run, &prepared, result) {
        run.fail(
            Status::Failed,
            format!("Could not complete stage bookkeeping: {error:#}"),
        );
    }
    run.persist()?;
    emit(Event::State(Box::new(run.clone())));
    Ok(())
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
            emit(Event::State(Box::new(run.clone())));
            return Ok(Action::Complete);
        }
        if cancel.load(Ordering::Relaxed) {
            run.fail(
                Status::Cancelled,
                "Run cancelled before the next stage".into(),
            );
            run.persist()?;
            return Ok(Action::Stopped);
        }
        if run.workflow.stages[run.cursor].mode == Mode::Interactive {
            run.status = Status::Interactive;
            run.persist()?;
            emit(Event::State(Box::new(run.clone())));
            return Ok(Action::Interactive);
        }
        execute_stage(run, cancel, false, &mut emit)?;
    }
}

pub fn run_interactive(run: &mut Run, cancel: &AtomicBool) -> Result<()> {
    ensure!(
        run.status == Status::Interactive,
        "No interactive stage is ready"
    );
    let _lock = storage::lock(&run.directory.join("run.lock"))?;
    let _workspace_lock =
        storage::lock(&storage::metadata_dir(&run.workspace)?.join("workspace.lock"))?;
    run.check_revision()?;
    execute_stage(run, cancel, true, &mut |_| {})
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
    }
    Ok(text)
}

pub fn ensure_runnable(run: &Run) -> Result<()> {
    if let Some(error) = &run.error {
        bail!("{error}\nArtifacts: {}", run.directory.display());
    }
    Ok(())
}
