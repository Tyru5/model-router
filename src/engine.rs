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
mod exec;
mod prompt;
mod recovery;
mod run;
mod schedule;
mod store;
mod types;

pub use apply::{ApplyPlan, apply_worktree, apply_worktree_plan};
pub use exec::detached_terminal;
pub use prompt::{
    FieldView, HandoffView, Inspection, Kept, PRIOR_BUDGET, PRIOR_EACH, inspect_handoffs, prompt,
};
pub use recovery::{
    authorize_replay, refuse_unproven, refuse_unproven_in_checkout, release_groups,
};
pub use store::{command, history, load};
pub use types::{
    Action, Attempt, CandidateChanges, CandidateState, Cmd, Event, FanOutState, Fork, Placement,
    Resume, Run, Selection, StageState, Status, Transition, Waiver,
};

use exec::{
    Exit, INTERRUPTED, Prepared, Resolved, candidate_index, conclude, evaluate_outcome,
    execute_stage, keep_survivor, read_head, reattach_stage, resolve_handoff, start_stage,
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
        } else if run.workflow.stages[run.cursor].delegation.is_some() {
            delegate::drive_stage(run, cancel, &mut emit)?;
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
    execute_stage(
        run,
        cancel,
        Some((session, &[&lock, &workspace_lock])),
        &mut emit,
    )?;
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
            mcp: None,
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
        let mut run = Run::create(
            dir.path(),
            &settings,
            &settings.workflows[0],
            "Task".into(),
            false,
        )
        .unwrap();
        let prepared = prepare(&mut run).unwrap();
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
}
