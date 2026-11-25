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

mod apply;
mod delegate;
mod delegate_apply;
mod delegate_runs;
mod exec;
mod host;
mod prompt;
mod recovery;
mod run;
mod schedule;
mod store;
mod types;

pub use apply::{ApplyPlan, apply_worktree, apply_worktree_plan};
pub use delegate::cancel_delegated_task;
pub use exec::detached_terminal;
pub use host::{RunHost, cancel_pending, run_host, set_run_host};
pub use prompt::{
    ContextFileView, FieldView, HandoffView, Inspection, Kept, MAX_CONTEXT_BYTES,
    MAX_CONTEXT_FILES, PRIOR_BUDGET, PRIOR_EACH, inspect_handoffs, prompt,
};
pub use recovery::{
    authorize_replay, refuse_unproven, refuse_unproven_in_checkout, release_groups,
};
pub use store::{command, history, load};
pub use types::{
    Action, Attempt, CandidateChanges, CandidateState, Cmd, CreationSource, Event, FanOutState,
    Fork, LaunchedBy, Placement, Resume, Run, Selection, StageState, Status, Transition, Waiver,
};

use exec::{
    Exit, INTERRUPTED, Prepared, Resolved, candidate_index, conclude, conclude_inner,
    evaluate_outcome, execute_stage, is_transient_error, keep_survivor, read_head, reattach_stage,
    resolve_handoff, run_post_stage_hooks, run_stage_hooks, should_transient_retry,
    sleep_interruptible, start_stage, start_stage_inner,
};
#[cfg(test)]
use exec::{finish, prepare};
#[cfg(test)]
use prompt::handoff_excerpt;
use prompt::{PromptTarget, prompt_for};
use recovery::{refuse_own_unproven, stop_orphans, stop_recorded};
use schedule::{SpawnJournal, auto_pick, drive_graph, execute_fan_out};
use store::{LOCK_WAIT, lock_run, save_changes, set_aside_handoff};
use types::{Decision, Hold, legal};

/// Drive until completion, failure, or a terminal/confirmation is required.
pub fn drive(run: &mut Run, cancel: &AtomicBool, mut emit: impl FnMut(Event)) -> Result<Action> {
    // An interactive harness outlived the mr that started it: reattach
    // (`run_interactive`). Its terminal host holds the locks meanwhile.
    if detached_terminal(run).is_some() {
        emit(Event::State(Box::new(run.clone())));
        return Ok(Action::Interactive);
    }
    let (_lock, _workspace_lock) = lock_run(run)?;
    run.check_revision()?;
    stop_orphans(run, &mut emit)?;
    crate::trust::authorize_run(run)?;
    if run.graph() {
        return drive_graph(run, cancel, &mut emit);
    }
    let mut checkout_checked = false;
    loop {
        match run.status {
            Status::AwaitingConfirmation => return Ok(Action::Confirm),
            Status::Interactive => return Ok(Action::Interactive),
            Status::Failed | Status::Blocked | Status::Cancelled => {
                save_changes(run);
                if !run.journal.is_empty() {
                    run.persist()?;
                }
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
            // Nothing gates after the last stage but this: spend that crossed a
            // stop cap blocks the run instead of completing it.
            let blocked = run
                .check_review_quorum()
                .err()
                .map(|error| format!("{error:#}"))
                .or_else(|| crate::cost::check_budget(run));
            if let Some(reason) = blocked {
                run.stop(Status::Blocked, reason, Actor::Engine, false)?;
                run.persist()?;
                save_changes(run);
                emit(Event::State(Box::new(run.clone())));
                return Ok(Action::Stopped);
            }
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
        if let Some(reason) = run.should_skip_when_changed(run.cursor) {
            let idx = run.cursor;
            let name = run.workflow.stages[idx].name.clone();
            run.stages[idx].skipped_reason = Some(reason.clone());
            run.set(Target::Stage(idx), Status::Skipped, Actor::Engine, &reason)?;
            run.cursor += 1;
            run.persist()?;
            emit(Event::Output(format!("── {name} · {reason} ──\n")));
            emit(Event::State(Box::new(run.clone())));
            continue;
        }
        // Spend is recorded per attempt; a cap stops or warns before more.
        match crate::cost::check_budget_action(run) {
            crate::cost::BudgetCheck::Ok => {}
            crate::cost::BudgetCheck::Warn(warning) => {
                emit(Event::Output(format!("{warning}\n")));
            }
            crate::cost::BudgetCheck::Stop(reason) => {
                run.stop(Status::Blocked, reason, Actor::Engine, false)?;
                emit(Event::State(Box::new(run.clone())));
                continue;
            }
        }
        if !checkout_checked {
            refuse_unproven(run)?;
            checkout_checked = true;
        } else {
            refuse_own_unproven(run)?;
        }
        if run.workflow.stages[run.cursor].role == Role::Ship {
            crate::ship::execute(run, cancel, &mut emit)?;
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
        } else if run.workflow.stages[run.cursor].delegation.is_some() {
            delegate::drive_stage(run, cancel, None, &mut emit)?;
        } else {
            execute_stage(run, cancel, None, &mut emit)?;
        }
    }
}

/// Own the stage on a worker; its foreground terminal can attach
/// independently. Reattaches when the stage's harness outlived an earlier mr.
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
            // A released connection an error left open: close it; the host
            // takes the locks by path once they are free.
            self.0.hand_over(&[]);
        }
    }
    let _session = FinishSession(session);
    if let Some(socket) = detached_terminal(run) {
        return reattach_stage(run, &socket, cancel, session, &mut emit);
    }
    ensure!(
        run.status == Status::Interactive && run.cursor < run.stages.len(),
        "No interactive stage is ready"
    );
    let (lock, workspace_lock) = lock_run(run)?;
    run.check_revision()?;
    stop_orphans(run, &mut emit)?;
    refuse_unproven(run)?;
    crate::trust::authorize_run(run)?;
    if run.workflow.stages[run.cursor].delegation.is_some() {
        delegate::drive_stage(
            run,
            cancel,
            Some((session, &[&lock, &workspace_lock])),
            &mut emit,
        )?;
    } else {
        execute_stage(
            run,
            cancel,
            Some((session, &[&lock, &workspace_lock])),
            &mut emit,
        )?;
    }
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
        "{}\nProject: {}\nTask: {}\n",
        workflow.name,
        project.display(),
        task
    );
    if workflow.handoff_budget_bytes.is_some() || workflow.handoff_each_bytes.is_some() {
        text.push_str(&format!(
            "Handoff budget: {} bytes total, {} each\n",
            workflow.effective_handoff_budget_bytes(),
            workflow.effective_handoff_each_bytes()
        ));
    }
    if let Some(quorum) = workflow.review_quorum.filter(|&n| n > 1) {
        text.push_str(&format!("Review quorum: {quorum} distinct reviews\n"));
    }
    if workflow.retry.max_attempts > 0 {
        text.push_str(&format!(
            "Default retry: up to {} transient attempt(s) (base {}s, max {}s)\n",
            workflow.retry.max_attempts,
            workflow.retry.base_delay_seconds,
            workflow.retry.max_delay_seconds
        ));
    }
    for (event, hook) in workflow.hooks.all_entries() {
        text.push_str(&format!(
            "Workflow hook ({}): {} ({}s)\n",
            event.label(),
            shell_words::join(&hook.command),
            hook.timeout_seconds
        ));
    }
    for policy in &workflow.policies {
        let paths = if policy.paths.is_empty() {
            "*".to_owned()
        } else {
            policy.paths.join(", ")
        };
        text.push_str(&format!(
            "Policy [{}] ({}): paths={paths}{}\n",
            policy.name,
            policy.action.label(),
            if policy.pattern.is_empty() {
                String::new()
            } else {
                format!(", pattern={:?}", policy.pattern)
            }
        ));
    }
    text.push('\n');
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
            if !stage.when_changed.is_empty() {
                text.push_str(&format!(
                    "   when_changed: {}\n",
                    stage.when_changed.join(", ")
                ));
            }
            for (event, hook) in stage.hooks.all_entries() {
                text.push_str(&format!(
                    "   hook ({}): {} ({}s)\n",
                    event.label(),
                    shell_words::join(&hook.command),
                    hook.timeout_seconds
                ));
            }
            continue;
        }
        // A delegating stage's launch carries the bridge; its socket and
        // token file are made per turn.
        let mcp = stage.delegation.as_ref().map(|_| adapters::McpLaunch {
            program: PathBuf::from("<model-router>"),
            sock: PathBuf::from("<orch>/orch-<id>.sock"),
            token_file: PathBuf::from("<orch>/orch-<id>.token"),
        });
        let paths = ContextPaths {
            workspace: project,
            attempt_dir: temp.path(),
            prompt_file: Path::new("<attempt>/prompt.md"),
            handoff_file: Path::new("<workspace>/.model-router/handoffs/<run>/<stage>.json"),
            mcp: mcp.as_ref(),
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
        if let Some(context) = prompt::preview_context_files(project, workflow, stage) {
            text.push_str(&format!("   {context}\n"));
        }
        if let Some(delegation) = &stage.delegation {
            text.push_str(&format!(
                "   delegation: tools through `model-router __mcp-bridge`; {} task(s) at most, {} at a time, {} wake turn(s)\n",
                delegation.max_tasks, delegation.max_parallel, delegation.max_wake_turns
            ));
            for name in &delegation.delegates {
                let Some(delegate) = settings.delegates.get(name) else {
                    continue;
                };
                let task = crate::delegation::config::delegate_stage(
                    name,
                    delegate,
                    stage,
                    Role::Research,
                );
                let paths = ContextPaths {
                    workspace: project,
                    attempt_dir: temp.path(),
                    prompt_file: Path::new("<task turn>/prompt.md"),
                    handoff_file: Path::new("<task turn>/handoff.json"),
                    mcp: None,
                };
                match adapters::build(
                    &task,
                    &settings.adapters,
                    &settings.providers,
                    "<task brief>",
                    &paths,
                ) {
                    Ok(command) => text.push_str(&format!(
                        "   delegate {name}: {}{}\n",
                        command.display(),
                        if command.stdin.is_some() {
                            " < <task turn>/prompt.md"
                        } else {
                            ""
                        }
                    )),
                    Err(error) => text.push_str(&format!("   delegate {name}: {error:#}\n")),
                }
            }
            if !delegation.harnesses.is_empty() {
                text.push_str(&format!(
                    "   ad-hoc targets: harnesses {:?}{}\n",
                    delegation.harnesses,
                    if delegation.models.is_empty() {
                        String::new()
                    } else {
                        format!(", models {:?}", delegation.models)
                    }
                ));
            }
        }
        if let Some(stall) = stage.stall_timeout_seconds {
            // Only unattended output is watched; an interactive stage is
            // the user's to watch.
            text.push_str(&format!(
                "   stall timeout: {stall}s{}\n",
                if stage.mode == Mode::Interactive {
                    " (not applied while the stage is interactive)"
                } else {
                    ""
                }
            ));
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
        if !stage.when_changed.is_empty() {
            text.push_str(&format!(
                "   when_changed: {}\n",
                stage.when_changed.join(", ")
            ));
        }
        if let Some(retry) = &stage.retry {
            text.push_str(&format!(
                "   retry: up to {} transient attempt(s) (base {}s, max {}s)\n",
                retry.max_attempts, retry.base_delay_seconds, retry.max_delay_seconds
            ));
        }
        for (event, hook) in stage.hooks.all_entries() {
            text.push_str(&format!(
                "   hook ({}): {} ({}s)\n",
                event.label(),
                shell_words::join(&hook.command),
                hook.timeout_seconds
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
        // A recorded session that could pass as an option is refused before
        // the handoff moves or the stage changes.
        run.stages[0].attempts[0].session = Some("--yolo".into());
        assert!(run.take_over().is_err());
        assert!(previous.handoff_file.is_file());
        assert_eq!(run.workflow.stages[0].mode, Mode::Interactive);
        assert_eq!(run.status, Status::AwaitingConfirmation);
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
        let (excerpt, _) = handoff_excerpt(&handoff, 5000, full);
        let (json, note) = excerpt.rsplit_once('\n').unwrap();
        assert_eq!(note, "[truncated; full artifact at /runs/x/handoff.json]");
        assert!(json.len() <= 5000, "{}", json.len());
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["verdict"], "complete");
        assert!(value["summary"].as_str().unwrap().ends_with('…'));
        // A small handoff is shown whole, without a note.
        let small: Handoff =
            serde_json::from_str(r#"{"summary":"Done","verdict":"complete"}"#).unwrap();
        let (whole, _) = handoff_excerpt(&small, 5000, full);
        assert!(serde_json::from_str::<serde_json::Value>(&whole).is_ok());
    }

    #[test]
    fn recovery_stops_groups_a_worker_journaled_but_the_run_never_saved() {
        use std::os::unix::process::CommandExt;
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            workflows: vec![Workflow::new(
                "W",
                vec![Stage {
                    id: "implement".into(),
                    harness: "mock".into(),
                    ..Stage::default()
                }],
            )],
            adapters: BTreeMap::from([(
                "mock".into(),
                CustomAdapter {
                    command: "python3".into(),
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
        // A worker started this group, then mr died before saving the run.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let journal = SpawnJournal::new(&run);
        let group = process::Group::launched(child.id(), None);
        journal.add(&group).unwrap();
        // A recovery that cannot save the run keeps the journal for the next.
        let summary = run.directory.join("summary.json");
        fs::remove_file(&summary).unwrap();
        fs::create_dir(&summary).unwrap();
        fs::write(summary.join("blocker"), "").unwrap();
        assert!(stop_orphans(&mut run, &mut |_| {}).is_err());
        assert!(child.try_wait().unwrap().is_some(), "orphan still running");
        assert_eq!(journal.read(), vec![group]);
        fs::remove_dir_all(&summary).unwrap();
        stop_orphans(&mut run, &mut |_| {}).unwrap();
        assert!(!run.directory.join("spawned-groups").exists());
        // A group the run saved leaves the journal; a bare ID from an
        // earlier version reads as a group without identity.
        fs::write(run.directory.join("spawned-groups"), "6\n7\n").unwrap();
        let eight = process::Group {
            pgid: 8,
            identity: process::Identity {
                started: Some("1.000000".into()),
                marker: Some("m".into()),
            },
        };
        journal.add(&eight).unwrap();
        journal.recorded(7);
        let six = process::Group {
            pgid: 6,
            identity: process::Identity::default(),
        };
        assert_eq!(journal.read(), vec![six, eight]);
    }

    #[test]
    fn a_harness_group_a_failed_launch_left_running_is_retained() {
        use std::os::unix::process::CommandExt;
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            workflows: vec![Workflow::new(
                "W",
                vec![Stage {
                    id: "implement".into(),
                    harness: "mock".into(),
                    ..Stage::default()
                }],
            )],
            adapters: BTreeMap::from([(
                "mock".into(),
                CustomAdapter {
                    command: "python3".into(),
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
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let group = process::Group {
            pgid: child.id(),
            identity: process::Identity::default(),
        };
        // As `process::interactive` reports a failed first attach.
        let result: Result<process::Outcome> = Err(anyhow::anyhow!(
            "Connect to the harness terminal"
        )
        .context(process::Survivor {
            group: group.clone(),
            why: "its leader has exited",
        }));
        keep_survivor(&mut run, &result);
        keep_survivor(&mut run, &result);
        assert_eq!(run.retained_groups, vec![group]);
        assert!(refuse_unproven(&run).is_err());
        // Any other failure leaves nothing behind to keep.
        let mut other = run.clone();
        other.retained_groups.clear();
        keep_survivor(&mut other, &Err(anyhow::anyhow!("The harness exited")));
        assert!(other.retained_groups.is_empty());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn a_worker_forgets_its_ended_group_even_when_the_run_could_not_save_it() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            workflows: vec![Workflow::new(
                "W",
                vec![
                    Stage {
                        id: "implement".into(),
                        harness: "mock".into(),
                        ..Stage::default()
                    },
                    // Makes the run a graph, which the scheduler drives.
                    Stage {
                        id: "review".into(),
                        harness: "mock".into(),
                        after: vec!["implement".into()],
                        ..Stage::default()
                    },
                ],
            )],
            adapters: BTreeMap::from([(
                "mock".into(),
                CustomAdapter {
                    command: "python3".into(),
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
        assert!(run.graph());
        authorize_replay(&mut run, None, |_| {}).unwrap();
        // Every save fails from the moment the harness is about to start.
        let summary = run.directory.join("summary.json");
        let cancel = AtomicBool::new(false);
        let result = drive(&mut run, &cancel, |event| {
            if let Event::Output(text) = event {
                if text.contains("attempt 1") && summary.is_file() {
                    fs::remove_file(&summary).unwrap();
                    fs::create_dir(&summary).unwrap();
                    fs::write(summary.join("blocker"), "").unwrap();
                }
            }
        });
        assert!(result.is_err(), "saving the finished attempt must fail");
        assert!(summary.is_dir());
        // The group ended with its worker, so a later recovery must not
        // signal its ID, which another process may have reused.
        assert_eq!(
            fs::read_to_string(run.directory.join("spawned-groups")).unwrap_or_default(),
            ""
        );
    }

    #[test]
    fn a_second_shortening_pass_counts_what_the_list_really_lost() {
        // Too big for the first pass (40 items), small enough for the second (20).
        let handoff: Handoff = serde_json::from_value(serde_json::json!({
            "summary": "s".repeat(900),
            "verdict": "complete",
            "decisions": (0..45).map(|i| format!("{i:02} {}", "d".repeat(300))).collect::<Vec<_>>(),
        }))
        .unwrap();
        let (excerpt, sent) = handoff_excerpt(&handoff, 8000, Path::new("/h.json"));
        let decisions = sent["decisions"].as_array().unwrap();
        assert_eq!(decisions.len(), 21, "{excerpt}");
        assert_eq!(decisions[20], "… 25 more");
        let view = HandoffView::compare("implement".into(), &handoff, &sent, 8000, excerpt.len());
        assert!(
            view.warnings
                .iter()
                .any(|w| w == "decisions lost: 25 of 45 dropped"),
            "{:?}",
            view.warnings
        );
    }

    const NOT_APPLICABLE: &str = r#"{"summary":"Docs only","verdict":"complete","tests":[],"not_applicable":"No code changed"}"#;

    /// Finishes an interactive attempt as if its harness wrote `handoff` and exited 0.
    #[test]
    fn a_harness_that_exited_while_no_mr_was_attached_finishes_its_stage() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            workflows: vec![Workflow::new(
                "Interactive",
                vec![Stage {
                    id: "implement".into(),
                    harness: "mock".into(),
                    mode: Mode::Interactive,
                    ..Stage::default()
                }],
            )],
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
        // As left by an mr that released the stage and exited; its terminal
        // host then recorded the harness's exit and went away.
        let prepared = prepare(&mut run).unwrap();
        fs::write(prepared.directory.join("stdout.log"), "transcript").unwrap();
        fs::write(
            &prepared.handoff_file,
            r#"{"summary":"Done while away","verdict":"complete"}"#,
        )
        .unwrap();
        fs::write(
            prepared.directory.join("terminal-exit.json"),
            r#"{"code":0,"cancelled":false}"#,
        )
        .unwrap();
        run.stages[0].attempts[0].terminal = Some(process::host::socket_path().unwrap());
        run.persist().unwrap();
        let mut run = load(dir.path(), &run.id).unwrap();
        assert!(detached_terminal(&run).is_some());
        let cancel = AtomicBool::new(false);
        assert_eq!(
            drive(&mut run, &cancel, |_| {}).unwrap(),
            Action::Interactive
        );
        authorize_replay(&mut run, None, |_| {}).unwrap();
        let session = process::InteractiveSession::new(80, 24);
        run_interactive(&mut run, &cancel, &session, |_| {}).unwrap();
        assert_eq!(run.status, Status::AwaitingConfirmation);
        assert_eq!(run.stages[0].attempts.len(), 1);
        assert_eq!(run.stages[0].attempts[0].exit_code, Some(0));
        assert_eq!(
            run.stages[0].handoff.as_ref().unwrap().summary,
            "Done while away"
        );
        assert!(detached_terminal(&run).is_none());
    }

    #[test]
    fn reattaching_refuses_saved_settings_changed_since_approval() {
        // The host recorded the harness's exit, or died without doing so.
        for exited in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let settings = Settings {
                workflows: vec![Workflow::new(
                    "Interactive",
                    vec![Stage {
                        id: "implement".into(),
                        harness: "mock".into(),
                        mode: Mode::Interactive,
                        ..Stage::default()
                    }],
                )],
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
            fs::write(
                &prepared.handoff_file,
                r#"{"summary":"Done while away","verdict":"complete"}"#,
            )
            .unwrap();
            if exited {
                fs::write(
                    prepared.directory.join("terminal-exit.json"),
                    r#"{"code":0,"cancelled":false}"#,
                )
                .unwrap();
            }
            let socket = process::host::socket_path().unwrap();
            if !exited {
                // Its socket outlived it: nothing listens there now.
                drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
            }
            run.stages[0].attempts[0].terminal = Some(socket);
            // run.json gained a check command nobody approved.
            let marker = dir.path().join("ran");
            run.workflow.stages[0].checks = vec![crate::config::Check {
                command: vec!["touch".into(), marker.display().to_string()],
                required: true,
                ..Default::default()
            }];
            run.persist().unwrap();
            let mut run = load(dir.path(), &run.id).unwrap();
            assert!(detached_terminal(&run).is_some());
            // Reattaching still finishes the harness, which ran under the
            // earlier approval, but never runs the unapproved check.
            authorize_replay(&mut run, None, |_| {}).unwrap();
            let cancel = AtomicBool::new(false);
            let session = process::InteractiveSession::new(80, 24);
            run_interactive(&mut run, &cancel, &session, |_| {}).unwrap();
            assert!(!marker.exists(), "an unapproved check ran");
            assert_eq!(run.status, Status::Failed);
            assert_eq!(run.stages[0].attempts[0].status, Status::Failed);
            let error = run.error.clone().unwrap_or_default();
            if exited {
                assert!(
                    error.contains("checks did not run") && error.contains("untrusted"),
                    "{error}"
                );
                // The attempt's ending is still recorded, as for any stage.
                let attempt = &run.stages[0].attempts[0];
                assert_eq!(attempt.exit_code, Some(0), "{attempt:?}");
                assert!(attempt.finished_at.is_some() && attempt.process_group.is_none());
                assert_eq!(attempt.handoff_source, Some(HandoffSource::File));
            } else {
                // A harness that never finished had nothing to check: the
                // failure is the lost terminal, not the approval.
                assert!(
                    error.contains("terminal from an earlier mr is gone")
                        && !error.contains("untrusted"),
                    "{error}"
                );
            }
            assert!(detached_terminal(&run).is_none());
        }
    }

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
            stalled: false,
            released: false,
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

    #[test]
    fn settle_keeps_sibling_failure_ahead_of_repair_and_clears_running() {
        let s = |id: &str, role: Role, after: &[&str]| Stage {
            id: id.into(),
            name: id.into(),
            role,
            harness: "mock".into(),
            after: after.iter().map(|a| (*a).into()).collect(),
            ..Stage::default()
        };
        let settings = Settings {
            workflows: vec![Workflow::new(
                "Graph",
                vec![
                    s("impl", Role::Implementation, &[]),
                    s("rev", Role::Review, &["impl"]),
                    s("other", Role::Research, &["impl"]),
                    s("extra", Role::Planning, &["impl"]),
                ],
            )],
            adapters: BTreeMap::from([(
                "mock".into(),
                CustomAdapter {
                    command: "python3".into(),
                    ..CustomAdapter::default()
                },
            )]),
            ..Settings::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let mut run = Run::create(
            dir.path(),
            &settings,
            &settings.workflows[0],
            "Task".into(),
            false,
        )
        .unwrap();
        run.stages[0].status = Status::Complete;
        run.stages[1].status = Status::Complete;
        run.stages[2].status = Status::Failed;
        run.stages[3].status = Status::Running;
        run.hold = Some(Hold {
            repair: Some((1, 0, "feedback".into(), "repair".into())),
            stops: vec![(2, "sibling failed".into())],
        });
        run.settle().unwrap();
        assert_eq!(run.status, Status::Failed);
        assert_eq!(run.cursor, 2);
        assert_eq!(run.repair_passes, 0);
        assert_eq!(run.stages[2].status, Status::Failed);
        assert!(run.stages.iter().all(|st| st.status != Status::Running));
    }

    #[test]
    fn retry_on_crashed_graph_marks_other_running_stages_failed() {
        let s = |id: &str, role: Role, after: &[&str]| Stage {
            id: id.into(),
            name: id.into(),
            role,
            harness: "mock".into(),
            after: after.iter().map(|a| (*a).into()).collect(),
            ..Stage::default()
        };
        let settings = Settings {
            workflows: vec![Workflow::new(
                "Graph",
                vec![
                    s("a", Role::Research, &[]),
                    s("b", Role::Planning, &[]),
                    s("join", Role::Summary, &["a", "b"]),
                ],
            )],
            adapters: BTreeMap::from([(
                "mock".into(),
                CustomAdapter {
                    command: "python3".into(),
                    ..CustomAdapter::default()
                },
            )]),
            ..Settings::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let mut run = Run::create(
            dir.path(),
            &settings,
            &settings.workflows[0],
            "Task".into(),
            false,
        )
        .unwrap();
        run.status = Status::Running;
        run.cursor = 0;
        run.stages[0].status = Status::Failed;
        run.stages[1].status = Status::Running;
        run.stages[1].attempts.push(Attempt {
            number: 1,
            started_at: Utc::now(),
            finished_at: None,
            model: "m".into(),
            mode: Mode::Unattended,
            status: Status::Running,
            exit_code: None,
            directory: run.directory.join("02-b/attempt-001"),
            handoff_file: run.directory.join("02-b/attempt-001/handoff.json"),
            error: None,
            checks: vec![],
            tree_before: None,
            tree_after: None,
            checks_tree: None,
            usage: None,
            session: None,
            process_group: None,
            process_identity: Default::default(),
            terminal: None,
            handoff_source: None,
            worktree: None,
            tasks: vec![],
            turns: vec![],
            launched_runs: vec![],
        });
        run.retry(false, None).unwrap();
        assert_eq!(run.status, Status::Pending);
        assert_eq!(run.stages[0].status, Status::Pending);
        assert_eq!(run.stages[1].status, Status::Failed);
        assert_eq!(run.stages[1].attempts[0].status, Status::Failed);
    }

    #[test]
    fn load_rejects_out_of_bounds_cursor_for_active_checkpoint() {
        for status in [Status::AwaitingConfirmation, Status::Interactive] {
            let dir = tempfile::tempdir().unwrap();
            let settings = Settings {
                workflows: vec![Workflow::new(
                    "W",
                    vec![Stage {
                        id: "implement".into(),
                        harness: "mock".into(),
                        ..Stage::default()
                    }],
                )],
                adapters: BTreeMap::from([(
                    "mock".into(),
                    CustomAdapter {
                        command: "python3".into(),
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
            run.status = status;
            run.cursor = run.stages.len();
            run.persist().unwrap();
            assert!(load(dir.path(), &run.id).is_err());
        }
    }

    #[test]
    fn persist_and_release_groups_preserve_state_on_write_failure() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            workflows: vec![Workflow::new(
                "W",
                vec![Stage {
                    id: "implement".into(),
                    harness: "mock".into(),
                    ..Stage::default()
                }],
            )],
            adapters: BTreeMap::from([(
                "mock".into(),
                CustomAdapter {
                    command: "python3".into(),
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
        let group = process::Group {
            pgid: 999999,
            identity: process::Identity::default(),
        };
        run.retained_groups = vec![group.clone()];
        run.set(Target::Stage(0), Status::Running, Actor::Engine, "test")
            .unwrap();
        let rev_before = run.revision;
        let journal_len = run.journal.len();

        let summary = run.directory.join("summary.json");
        fs::remove_file(&summary).unwrap();
        fs::create_dir(&summary).unwrap();
        fs::write(summary.join("blocker"), "").unwrap();

        assert!(run.persist().is_err());
        assert_eq!(run.revision, rev_before);
        assert_eq!(run.journal.len(), journal_len);

        assert!(release_groups(&mut run).is_err());
        assert_eq!(run.retained_groups, vec![group]);
    }

    #[test]
    fn protected_paths_fail_closed_when_snapshot_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            workflows: vec![Workflow {
                protected_paths: vec![".github/**".into()],
                ..Workflow::new(
                    "W",
                    vec![Stage {
                        id: "implement".into(),
                        harness: "mock".into(),
                        ..Stage::default()
                    }],
                )
            }],
            adapters: BTreeMap::from([(
                "mock".into(),
                CustomAdapter {
                    command: "python3".into(),
                    ..CustomAdapter::default()
                },
            )]),
            ..Settings::default()
        };
        // Outside Git, prepare refuses before launching a stage when
        // protected_paths require a pre-stage snapshot.
        let mut non_git_run = Run::create(
            dir.path(),
            &settings,
            &settings.workflows[0],
            "Task".into(),
            false,
        )
        .unwrap();
        let err = prepare(&mut non_git_run)
            .err()
            .expect("prepare should refuse outside Git when protected_paths is set");
        assert!(format!("{err:#}").contains("protected_paths"), "{err:#}");
        assert!(non_git_run.stages[0].attempts.is_empty());

        // Inside a Git repo where snapshot capture fails (e.g. required local
        // clean filter disabled), prepare refuses before launch and surfaces
        // the underlying Git error.
        let repo = tempfile::tempdir().unwrap();
        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .status()
                .unwrap();
            assert!(status.success(), "{args:?}");
        };
        run_git(&["init", "-q"]);
        fs::write(repo.path().join(".gitattributes"), "*.req filter=req\n").unwrap();
        fs::write(repo.path().join("data.req"), "payload\n").unwrap();
        run_git(&["config", "--local", "filter.req.clean", "cat"]);
        run_git(&["config", "--local", "filter.req.required", "true"]);
        let mut req_run = Run::create(
            repo.path(),
            &settings,
            &settings.workflows[0],
            "Task".into(),
            false,
        )
        .unwrap();
        let req_err = prepare(&mut req_run)
            .err()
            .expect("prepare should refuse when pre-stage Git snapshot fails");
        let req_msg = format!("{req_err:#}");
        assert!(req_msg.contains("protected_paths"), "{req_msg}");
        assert!(req_msg.contains("Git clean filter `req`"), "{req_msg}");
        assert!(req_run.stages[0].attempts.is_empty());

        // Defense in depth: finish also blocks if an existing attempt somehow
        // lacks tree_before.
        run_git(&["config", "--local", "--unset", "filter.req.required"]);
        run_git(&["config", "--local", "--unset", "filter.req.clean"]);
        let mut run = Run::create(
            repo.path(),
            &settings,
            &settings.workflows[0],
            "Task".into(),
            false,
        )
        .unwrap();
        let prepared = prepare(&mut run).unwrap();
        run.stages[0].attempts[0].tree_before = None;
        fs::write(prepared.directory.join("stdout.log"), "ok").unwrap();
        fs::write(
            &prepared.handoff_file,
            r#"{"summary":"Done","verdict":"complete"}"#,
        )
        .unwrap();
        let outcome = process::Outcome {
            code: Some(0),
            cancelled: false,
            timed_out: false,
            stalled: false,
            released: false,
        };
        finish(
            &mut run,
            &prepared,
            Ok(outcome),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(run.status, Status::Blocked);
        assert!(
            run.error.as_deref().unwrap().contains("protected_paths"),
            "{:?}",
            run.error
        );
    }

    #[test]
    fn prompt_and_inspect_handoffs_emit_identical_exhausted_budget_pointer() {
        // Enough prior stages that the budget runs out whatever the length
        // of the temp paths in each excerpt (macOS's are much longer).
        const STAGES: usize = 12;
        let dir = tempfile::tempdir().unwrap();
        let make_stage = |id: &str| Stage {
            id: id.into(),
            name: id.into(),
            role: Role::Research,
            harness: "mock".into(),
            ..Stage::default()
        };
        let workflow = Workflow::new(
            "Budget",
            (1..=STAGES).map(|i| make_stage(&format!("s{i}"))).collect(),
        );
        let settings = Settings {
            workflows: vec![workflow.clone()],
            adapters: BTreeMap::from([(
                "mock".into(),
                CustomAdapter {
                    command: "python3".into(),
                    ..CustomAdapter::default()
                },
            )]),
            ..Settings::default()
        };
        let mut run = Run::create(dir.path(), &settings, &workflow, "Task".into(), false).unwrap();
        let long = "word ".repeat(250);
        let decisions: Vec<String> = (0..32)
            .map(|j| format!("decision {j:02}: {}", "x".repeat(120)))
            .collect();
        for i in 0..STAGES - 1 {
            run.stages[i].status = Status::Complete;
            let mut handoff = Handoff::unstructured(long.clone());
            handoff.decisions = decisions.clone();
            run.stages[i].handoff = Some(handoff);
            run.stages[i].inherited = Some(run.directory.join(format!("{:02}-s{}", i + 1, i + 1)));
        }
        run.cursor = STAGES - 1;
        let handoff = run
            .workspace
            .join(".model-router/handoffs")
            .join(&run.id)
            .join(format!("s{STAGES}-001.json"));
        let schema = run
            .directory
            .join(format!("{STAGES:02}-s{STAGES}/attempt-001"))
            .join(handoff::SCHEMA_FILE);
        let direct = prompt(&run, &handoff, &schema);
        let inspection = inspect_handoffs(&run, None).unwrap();
        assert_eq!(inspection.prompt, direct);
        assert!(
            direct.contains("[omitted: prior handoff budget exhausted; full artifact at "),
            "{direct}"
        );
        // Nearest stage first, so s1 (last in the list) has no budget left.
        assert_eq!(inspection.handoffs.len(), STAGES - 1);
        let oldest = inspection.handoffs.last().unwrap();
        assert!(!oldest.sent);
        assert_eq!(
            oldest.reason.as_deref(),
            Some("prior handoff budget exhausted")
        );
    }

    #[test]
    fn tunable_handoff_budgets_control_prompt_and_inspection() {
        let dir = tempfile::tempdir().unwrap();
        let stage = |id: &str| Stage {
            id: id.into(),
            name: id.into(),
            role: Role::Research,
            harness: "mock".into(),
            ..Stage::default()
        };
        let mut workflow = Workflow::new("Tunable", vec![stage("s1"), stage("s2")]);
        workflow.handoff_budget_bytes = Some(2_048);
        workflow.handoff_each_bytes = Some(1_500);
        let settings = Settings {
            workflows: vec![workflow.clone()],
            adapters: BTreeMap::from([(
                "mock".into(),
                CustomAdapter {
                    command: "python3".into(),
                    ..CustomAdapter::default()
                },
            )]),
            ..Settings::default()
        };
        let mut run = Run::create(dir.path(), &settings, &workflow, "Task".into(), false).unwrap();
        let mut large = Handoff::unstructured("alpha ".repeat(1_200));
        large.decisions = (0..10)
            .map(|i| format!("decision {i}: {}", "d".repeat(60)))
            .collect();
        run.stages[0].status = Status::Complete;
        run.stages[0].handoff = Some(large);
        run.stages[0].inherited = Some(run.directory.join("01-s1"));
        run.cursor = 1;

        let small = inspect_handoffs(&run, None).unwrap();
        assert_eq!((small.budget_bytes, small.each_bytes), (2_048, 1_500));
        assert!(small.handoffs[0].sent);
        assert_eq!(small.handoffs[0].budget, 1_500);
        assert!(!small.handoffs[0].fields.is_empty());
        assert!(
            small
                .text()
                .contains("Budget: 2048 bytes of prior handoffs, at most 1500 each"),
            "{}",
            small.text()
        );

        // Raising the budget keeps the same large handoff whole.
        run.workflow.handoff_budget_bytes = Some(30_000);
        run.workflow.handoff_each_bytes = Some(15_000);
        let large_view = inspect_handoffs(&run, None).unwrap();
        assert_eq!(
            (large_view.budget_bytes, large_view.each_bytes),
            (30_000, 15_000)
        );
        assert_eq!(large_view.handoffs[0].budget, 15_000);
        assert!(
            large_view.handoffs[0].fields.is_empty(),
            "{:?}",
            large_view.handoffs[0]
        );
    }

    #[test]
    fn context_files_inject_globs_dedup_bounds_and_block_symlinks_binary_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "TOP_SECRET_OUTSIDE").unwrap();
        fs::create_dir_all(dir.path().join("docs/sub")).unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::create_dir_all(dir.path().join(".model-router")).unwrap();
        fs::write(dir.path().join("AGENTS.md"), "Follow project rules.\n").unwrap();
        fs::write(dir.path().join("docs/a.md"), "Doc A\n").unwrap();
        fs::write(dir.path().join("docs/b.md"), "Doc B\n").unwrap();
        fs::write(dir.path().join("docs/sub/c.md"), "Doc C\n").unwrap();
        fs::write(dir.path().join("binary.bin"), [0xff, 0xfe, 0x00, b'x']).unwrap();
        fs::write(dir.path().join(".git/config"), "GIT_METADATA_SECRET").unwrap();
        fs::write(dir.path().join(".model-router/secret.txt"), "MR_SECRET").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                outside.path().join("secret.txt"),
                dir.path().join("link_file.md"),
            )
            .unwrap();
            std::os::unix::fs::symlink(outside.path(), dir.path().join("docs/link_dir")).unwrap();
        }

        let mut stage = Stage {
            id: "research".into(),
            name: "Research".into(),
            role: Role::Research,
            harness: "mock".into(),
            context_files: vec![
                "AGENTS.md".into(),
                "docs/**/*.md".into(),
                "binary.bin".into(),
                "link_file.md".into(),
                "missing.md".into(),
            ],
            ..Stage::default()
        };
        let mut workflow = Workflow::new("Context", vec![stage.clone()]);
        workflow.context_files = vec!["AGENTS.md".into(), "docs/*.md".into()];
        let settings = Settings {
            workflows: vec![workflow.clone()],
            adapters: BTreeMap::from([(
                "mock".into(),
                CustomAdapter {
                    command: "python3".into(),
                    ..CustomAdapter::default()
                },
            )]),
            ..Settings::default()
        };

        let preview_text = preview(&settings, &workflow, dir.path(), "Task").unwrap();
        assert!(preview_text.contains("context files:"), "{preview_text}");
        assert!(preview_text.contains("AGENTS.md"), "{preview_text}");
        assert!(preview_text.contains("docs/sub/c.md"), "{preview_text}");
        assert!(
            preview_text.contains("binary.bin (non-UTF-8 or binary file)"),
            "{preview_text}"
        );
        assert!(
            preview_text.contains("missing.md (not found)"),
            "{preview_text}"
        );
        #[cfg(unix)]
        assert!(
            preview_text.contains("link_file.md (symlink not allowed)"),
            "{preview_text}"
        );

        let run = Run::create(dir.path(), &settings, &workflow, "Task".into(), false).unwrap();
        let inspection = inspect_handoffs(&run, None).unwrap();
        // Deduplicated: AGENTS.md appears once, docs/a.md, docs/b.md, docs/sub/c.md included.
        assert_eq!(
            inspection.prompt.matches("--- AGENTS.md ---").count(),
            1,
            "{}",
            inspection.prompt
        );
        for expected in ["Follow project rules.", "Doc A", "Doc B", "Doc C"] {
            assert!(
                inspection.prompt.contains(expected),
                "missing {expected} in {}",
                inspection.prompt
            );
        }
        for forbidden in ["TOP_SECRET_OUTSIDE", "GIT_METADATA_SECRET", "MR_SECRET"] {
            assert!(
                !inspection.prompt.contains(forbidden),
                "leaked {forbidden} in {}",
                inspection.prompt
            );
        }
        assert!(
            inspection.text().contains("Context files:"),
            "{}",
            inspection.text()
        );

        // Even a broad `**/*` glob never reads `.git`, `.model-router`, or symlinks.
        stage.context_files = vec!["**/*".into()];
        let broad_wf = Workflow::new("Broad", vec![stage]);
        let broad_run =
            Run::create(dir.path(), &settings, &broad_wf, "Task".into(), false).unwrap();
        let broad = inspect_handoffs(&broad_run, None).unwrap();
        for forbidden in ["TOP_SECRET_OUTSIDE", "GIT_METADATA_SECRET", "MR_SECRET"] {
            assert!(
                !broad.prompt.contains(forbidden),
                "broad glob leaked {forbidden}"
            );
        }

        // File count cap (max 32 files).
        fs::create_dir_all(dir.path().join("many")).unwrap();
        for i in 0..40 {
            fs::write(
                dir.path().join(format!("many/f{i:02}.txt")),
                format!("v{i}\n"),
            )
            .unwrap();
        }
        let many_wf = Workflow {
            context_files: vec!["many/*.txt".into()],
            ..Workflow::new(
                "Many",
                vec![Stage {
                    id: "r".into(),
                    name: "R".into(),
                    harness: "mock".into(),
                    ..Stage::default()
                }],
            )
        };
        let many_run = Run::create(dir.path(), &settings, &many_wf, "Task".into(), false).unwrap();
        let many = inspect_handoffs(&many_run, None).unwrap();
        let included_count = many.context_files.iter().filter(|f| f.included).count();
        assert_eq!(included_count, MAX_CONTEXT_FILES);
        assert!(
            many.context_files
                .iter()
                .any(|f| !f.included && f.path == "(8 more)")
        );

        // Total byte cap (max 32 KiB) with multi-byte UTF-8 boundary truncation.
        fs::create_dir_all(dir.path().join("big")).unwrap();
        fs::write(dir.path().join("big/01.txt"), "a".repeat(30_000)).unwrap();
        // Multi-byte 'é' (2 bytes) crosses the remaining 2768-byte boundary cleanly.
        fs::write(dir.path().join("big/02.txt"), "é".repeat(3_000)).unwrap();
        fs::write(dir.path().join("big/03.txt"), "never included").unwrap();
        let big_wf = Workflow {
            context_files: vec!["big/*.txt".into()],
            ..Workflow::new(
                "Big",
                vec![Stage {
                    id: "r".into(),
                    name: "R".into(),
                    harness: "mock".into(),
                    ..Stage::default()
                }],
            )
        };
        let big_run = Run::create(dir.path(), &settings, &big_wf, "Task".into(), false).unwrap();
        let big = inspect_handoffs(&big_run, None).unwrap();
        let total_bytes: usize = big
            .context_files
            .iter()
            .filter(|f| f.included)
            .map(|f| f.bytes)
            .sum();
        assert!(total_bytes <= MAX_CONTEXT_BYTES, "{total_bytes}");
        assert!(
            big.context_files
                .iter()
                .any(|f| f.path == "big/02.txt" && f.truncated)
        );
        assert!(
            big.prompt
                .contains("[truncated; full file in workspace at big/02.txt]")
        );
        assert!(!big.prompt.contains("never included"));
        assert!(big.context_files.iter().any(|f| {
            !f.included
                && f.reason
                    .as_deref()
                    .is_some_and(|r| r.contains("byte limit"))
        }));
    }

    #[test]
    fn transient_error_classifier_matches_provider_limits_and_rejects_auth_or_line_numbers() {
        use super::exec::is_transient_error;
        assert!(is_transient_error("HTTP 429 Too Many Requests", ""));
        assert!(is_transient_error("API Error: 529 overloaded_error", ""));
        assert!(is_transient_error(
            "Harness exited with code 1.",
            "503 Service Unavailable"
        ));
        assert!(is_transient_error("connection reset by peer", ""));
        assert!(is_transient_error("rate_limit_error: try again later", ""));
        assert!(!is_transient_error(
            "Invalid API key · Please run /login",
            "429"
        ));
        assert!(!is_transient_error(
            "authentication_error: 401 unauthorized",
            ""
        ));
        assert!(!is_transient_error("syntax error at line 429, col 503", ""));
        assert!(!is_transient_error(
            "Process exited with code 1",
            "file not found"
        ));
    }
}
