//! Dependency graphs and best-of-N fan-out, with the offline mock harness
//! and real Git repositories.
mod common;

use common::{count, create, drive, fixture, repository, settings, stage, tempdir, workflow};
use model_router::{
    config::{Candidate, Check, FanOut, Role, Stage},
    engine::{self, Action, Attempt, Cmd, Event, Run, Status},
    transitions::{self, Actor, Level},
};
use std::{fs, path::Path, process::Command, sync::atomic::AtomicBool};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn delayed(id: &str, role: Role, after: &[&str], seconds: f32) -> Stage {
    Stage {
        after: after.iter().map(|s| (*s).into()).collect(),
        extra_args: vec!["--delay".into(), seconds.to_string()],
        ..stage(id, role)
    }
}

fn overlap(a: &Attempt, b: &Attempt) -> bool {
    a.started_at < b.finished_at.unwrap() && b.started_at < a.finished_at.unwrap()
}

#[test]
fn parallel_read_only_stages_overlap_and_join() {
    let (dir, _, mut run) = fixture(vec![
        delayed("a", Role::Research, &[], 1.5),
        delayed("b", Role::Planning, &[], 1.5),
        Stage {
            after: vec!["a".into(), "b".into()],
            ..stage("join", Role::Summary)
        },
    ]);
    assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);
    let (a, b, join) = (
        &run.stages[0].attempts[0],
        &run.stages[1].attempts[0],
        &run.stages[2].attempts[0],
    );
    assert!(overlap(a, b), "read-only roots run side by side");
    assert!(join.started_at >= a.finished_at.unwrap() && join.started_at >= b.finished_at.unwrap());
    let prompt = fs::read_to_string(dir.path().join(".model-router/mock/join.prompt")).unwrap();
    assert!(prompt.contains("a evidence") && prompt.contains("b evidence"));
    // Siblings never see each other's handoffs.
    let a_prompt = fs::read_to_string(dir.path().join(".model-router/mock/a.prompt")).unwrap();
    assert!(!a_prompt.contains("b evidence"));
}

#[test]
fn writers_wait_and_run_alone_in_list_order() {
    let (_dir, _, mut run) = fixture(vec![
        delayed("w1", Role::Implementation, &[], 0.5),
        delayed("r", Role::Research, &[], 0.5),
        delayed("w2", Role::Custom, &[], 0.5),
        // A dependency makes the workflow a graph: the others are roots.
        delayed("end", Role::Summary, &["w1", "r", "w2"], 0.0),
    ]);
    let mut notes = vec![];
    let action = engine::drive(&mut run, &AtomicBool::new(false), |event| {
        if let Event::Output(text) = event {
            notes.push(text);
        }
    })
    .unwrap();
    assert_eq!(action, Action::Complete, "{:?}", run.error);
    let attempts: Vec<_> = run.stages.iter().map(|s| &s.attempts[0]).collect();
    for (i, x) in attempts.iter().enumerate() {
        for y in &attempts[i + 1..] {
            assert!(!overlap(x, y), "writers run alone");
        }
    }
    assert!(
        notes
            .iter()
            .any(|n| n.contains("Stage w2 changes the shared workspace")),
        "{notes:?}"
    );
}

#[test]
fn a_parallel_review_repair_waits_for_its_sibling_and_reruns_both() {
    let mut fail = delayed("review-a", Role::Review, &["implementation"], 0.5);
    fail.extra_args
        .extend(["--review-fail-until".into(), "1".into()]);
    let (dir, _, mut run) = fixture(vec![
        stage("implementation", Role::Implementation),
        fail,
        delayed("review-b", Role::Review, &["implementation"], 0.5),
    ]);
    assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);
    assert!(overlap(
        &run.stages[1].attempts[0],
        &run.stages[2].attempts[0]
    ));
    assert_eq!(run.repair_passes, 1);
    assert_eq!(count(dir.path(), "implementation"), 2);
    assert_eq!(count(dir.path(), "review-a"), 2);
    assert_eq!(count(dir.path(), "review-b"), 2);
    let prompt =
        fs::read_to_string(dir.path().join(".model-router/mock/implementation.prompt")).unwrap();
    assert!(prompt.contains("REPAIR REQUEST"));
}

fn fan_out(candidates: &[(&str, &str)], judge: &str) -> Stage {
    Stage {
        fan_out: Some(FanOut {
            candidates: candidates
                .iter()
                .map(|(provider, model)| Candidate {
                    harness: "mock".into(),
                    provider: (*provider).into(),
                    model: (*model).into(),
                    ..Candidate::default()
                })
                .collect(),
            judge: judge.into(),
        }),
        extra_args: vec!["--write-file".into(), "out.txt".into()],
        ..stage("implement", Role::Implementation)
    }
}

/// A run in a fresh repository; None without a human Git identity.
fn git_run(stages: Vec<Stage>) -> Option<(tempfile::TempDir, Run)> {
    let dir = tempdir();
    repository(dir.path())?;
    let workflow = workflow(stages);
    let run = create(dir.path(), &settings(&workflow), &workflow);
    Some((dir, run))
}

fn lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn auto_judge_applies_the_smallest_candidate_whose_checks_pass() {
    // Without checks the fewest changed lines win; a failing required check
    // ranks a candidate after every passing one.
    for (check, winner, size) in [(false, "c2", 2), (true, "c1", 5)] {
        let mut implement = fan_out(
            &[
                ("cand1", "lines-5"),
                ("cand2", "lines-2"),
                ("cand3", "fail"),
            ],
            "auto",
        );
        if check {
            implement.checks = vec![Check {
                command: vec![
                    "python3".into(),
                    "-c".into(),
                    "import pathlib, sys; sys.exit(len(pathlib.Path('out.txt').read_text().splitlines()) == 2)".into(),
                ],
                ..Check::default()
            }];
        }
        let Some((dir, mut run)) = git_run(vec![implement]) else {
            return;
        };
        let head = git(dir.path(), &["rev-parse", "HEAD"]);
        let branches = git(dir.path(), &["branch", "--list"]);
        assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);
        let state = run.stages[0].fan_out.as_ref().unwrap();
        let selection = state.selection.as_ref().unwrap();
        assert_eq!(
            (
                selection.candidate.as_str(),
                selection.by.as_str(),
                selection.applied
            ),
            (winner, "auto", true)
        );
        let statuses: Vec<_> = state.candidates.iter().map(|c| c.status).collect();
        assert_eq!(
            statuses,
            [Status::Complete, Status::Complete, Status::Failed]
        );
        assert!(
            state.candidates[2]
                .error
                .as_deref()
                .unwrap()
                .contains("code Some(17)")
        );
        let written = lines(&dir.path().join("out.txt"));
        assert_eq!(written.len(), size);
        assert!(written[0].starts_with(&format!("written by cand{}", &winner[1..])));
        // Candidates started from the uncommitted workspace and stay on disk.
        for candidate in &state.candidates[..2] {
            let tree = candidate.attempts[0].worktree.as_ref().unwrap();
            assert_eq!(
                fs::read_to_string(tree.join("draft.txt")).unwrap(),
                "uncommitted draft\n"
            );
            assert!(candidate.changes.as_ref().unwrap().patch.is_file());
        }
        // Nothing the user owns moved: HEAD, branches, and the index.
        assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), head);
        assert_eq!(git(dir.path(), &["branch", "--list"]), branches);
        assert_eq!(git(dir.path(), &["diff", "--cached", "--name-only"]), "");
        assert!(
            git(dir.path(), &["for-each-ref", "refs/model-router/"])
                .contains("/base/implement-001")
        );
        // The run's changes include the applied candidate.
        let report = model_router::report::changes(&run);
        let model_router::report::Changes::Tracked { files } = report else {
            panic!("{report:?}");
        };
        assert!(files.iter().any(|f| f.path == "out.txt"));
        let text = model_router::report::render(&model_router::report::build(&run), None);
        assert!(
            text.contains(&format!("judge auto: {winner} applied")),
            "{text}"
        );
        assert!(text.contains(&format!("★ {winner} mock")), "{text}");
        assert!(text.contains("· c3 mock fail failed"), "{text}");
        let records = transitions::read(&run.directory).unwrap();
        for id in ["c1", "c2", "c3"] {
            assert!(
                records
                    .iter()
                    .any(|r| r.level == Level::Candidate && r.candidate.as_deref() == Some(id))
            );
        }
        assert!(records.iter().any(|r| {
            r.level == Level::Attempt
                && r.candidate.as_deref() == Some("c3")
                && r.to == Status::Failed
        }));
    }
}

#[test]
fn a_judge_stage_chooses_candidate_two_and_dependents_wait_for_it() {
    let implement = Stage {
        after: vec!["research".into()],
        ..fan_out(
            &[("implement", "lines-1"), ("implement", "lines-3")],
            "judge",
        )
    };
    let judge = Stage {
        after: vec!["implement".into()],
        extra_args: vec!["--select".into(), "c2".into()],
        ..stage("judge", Role::Review)
    };
    let verify = Stage {
        after: vec!["implement".into()],
        extra_args: vec!["--test-status".into(), "passed".into()],
        ..stage("verify", Role::Test)
    };
    let Some((dir, mut run)) = git_run(vec![
        stage("research", Role::Research),
        implement,
        judge,
        verify,
    ]) else {
        return;
    };
    assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);
    let state = run.stages[1].fan_out.as_ref().unwrap();
    let selection = state.selection.as_ref().unwrap();
    assert_eq!(
        (selection.candidate.as_str(), selection.by.as_str()),
        ("c2", "judge")
    );
    assert_eq!(lines(&dir.path().join("out.txt")).len(), 3);
    // The judge saw both candidates; verification waited for its choice.
    let prompt = fs::read_to_string(dir.path().join(".model-router/mock/judge.prompt")).unwrap();
    assert!(
        prompt.contains("FAN-OUT CANDIDATES") && prompt.contains("- c1") && prompt.contains("- c2")
    );
    assert!(
        prompt.contains(
            &state.candidates[1]
                .changes
                .as_ref()
                .unwrap()
                .patch
                .display()
                .to_string()
        )
    );
    assert!(run.stages[3].attempts[0].started_at >= run.stages[2].attempts[0].finished_at.unwrap());
    // The candidates were told where they work.
    let candidate = &state.candidates[0].attempts[0];
    let cprompt = fs::read_to_string(candidate.directory.join("prompt.md")).unwrap();
    assert!(cprompt.contains("FAN-OUT CANDIDATE\nYou are candidate c1"));
    assert!(cprompt.contains(&candidate.worktree.as_ref().unwrap().display().to_string()));
    // The fan-out stage carries the winner's handoff onward.
    let handoff = run.stages[1].handoff.as_ref().unwrap();
    assert!(
        handoff
            .decisions
            .iter()
            .any(|d| d.contains("Chosen by judge"))
    );
    // A fork carries no candidates to judge: point at the fan-out instead.
    let error =
        model_router::history::fork(dir.path(), &settings(&run.workflow), &run.id, "judge", None)
            .unwrap_err();
    assert!(
        format!("{error:#}").contains("fork at implement instead"),
        "{error:#}"
    );
}

#[test]
fn a_fan_out_receipt_reports_the_chosen_candidates_failed_checks() {
    use model_router::report::{Basis, receipts};
    let mut implement = fan_out(&[("a", "test-model"), ("b", "test-model")], "auto");
    implement.checks = vec![Check {
        command: vec!["python3".into(), "-c".into(), "raise SystemExit(1)".into()],
        ..Check::default()
    }];
    let Some((_dir, mut run)) = git_run(vec![implement]) else {
        return;
    };
    // Auto-pick still applies a candidate when every one fails its checks.
    drive(&mut run);
    assert!(run.stages[0].fan_out.as_ref().unwrap().selection.is_some());
    // The stage's own attempt runs no checks; the receipt is the winner's.
    let receipt = receipts(&run).remove(0);
    assert_eq!(receipt.basis, Basis::RouterChecksFailed, "{receipt:?}");
    assert_eq!((receipt.required_passed, receipt.required_total), (0, 1));
    // It names the stage's attempt, and the winner's tested snapshot is what
    // the workspace now holds.
    assert_eq!(
        receipt.attempt,
        run.stages[0].attempts.last().unwrap().number
    );
    assert!(receipt.snapshot.is_some(), "{receipt:?}");
    assert_eq!(
        receipt.freshness,
        Some(model_router::report::Freshness::Current),
        "{receipt:?}"
    );
}

#[test]
fn a_candidates_snapshot_never_writes_through_a_link_in_its_attempt_directory() {
    let Some((_dir, mut run)) = git_run(vec![fan_out(
        &[("a", "test-model"), ("b", "test-model")],
        "auto",
    )]) else {
        return;
    };
    // As a candidate could leave it in the attempt directory it may write.
    let outside = tempdir();
    let victim = outside.path().join("victim");
    let attempt = run.directory.join("01-implement/candidates/c1/attempt-001");
    fs::create_dir_all(&attempt).unwrap();
    std::os::unix::fs::symlink(&victim, attempt.join("snapshot.index")).unwrap();
    drive(&mut run);
    assert!(
        run.stages[0].fan_out.as_ref().unwrap().selection.is_some(),
        "{:?}",
        run.error
    );
    assert!(!victim.exists(), "the router wrote through the link");
}

#[test]
fn a_fan_out_run_with_linked_metadata_stays_loadable() {
    // `.model-router` moved to another disk behind a link Git does not
    // ignore, so candidate checkouts carry the link and their handoffs land
    // in the run's own handoff directory.
    let dir = tempdir();
    if repository(dir.path()).is_none() {
        return;
    }
    let elsewhere = tempdir();
    std::os::unix::fs::symlink(elsewhere.path(), dir.path().join(".model-router")).unwrap();
    let workflow = workflow(vec![fan_out(
        &[("a", "test-model"), ("b", "test-model")],
        "auto",
    )]);
    let mut run = create(dir.path(), &settings(&workflow), &workflow);
    drive(&mut run);
    assert!(
        run.stages[0].fan_out.as_ref().unwrap().selection.is_some(),
        "{:?}",
        run.error
    );
    engine::load(dir.path(), &run.id).unwrap();
}

#[test]
fn a_conflicting_winner_blocks_and_leaves_the_workspace_unchanged() {
    let judge = Stage {
        // The judge edits the file the candidates created, then picks one.
        extra_args: vec![
            "--select".into(),
            "c1".into(),
            "--write-file".into(),
            "out.txt".into(),
        ],
        ..stage("judge", Role::Review)
    };
    let Some((dir, mut run)) = git_run(vec![
        fan_out(
            &[("implement", "lines-1"), ("implement", "lines-2")],
            "judge",
        ),
        judge,
    ]) else {
        return;
    };
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!((run.status, run.cursor), (Status::Blocked, 1));
    let error = run.error.clone().unwrap();
    assert!(
        error.contains("conflict") && error.contains("out.txt"),
        "{error}"
    );
    assert_eq!(
        lines(&dir.path().join("out.txt")),
        ["written by judge attempt 1 line 1"]
    );
    let state = run.stages[0].fan_out.as_ref().unwrap();
    assert!(!state.selection.as_ref().unwrap().applied);
    let tree = state.candidates[0].attempts[0].worktree.as_ref().unwrap();
    assert!(tree.join("out.txt").is_file() && error.contains(&tree.display().to_string()));
}

#[test]
fn a_crashed_candidate_fails_and_retries_alone() {
    let Some((dir, mut run)) = git_run(vec![fan_out(
        &[("implement", "fail"), ("implement", "fail")],
        "auto",
    )]) else {
        return;
    };
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!(run.status, Status::Blocked);
    assert!(
        run.error
            .as_deref()
            .unwrap()
            .contains("No fan-out candidate succeeded")
    );
    // Simulate a router that died while candidate c2 ran, after c1 failed.
    let path = run.directory.join("run.json");
    let mut saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    saved["status"] = "running".into();
    let stage = &mut saved["stages"][0];
    stage["status"] = "running".into();
    stage["attempts"][0]["status"] = "running".into();
    let c2 = &mut stage["fan_out"]["candidates"][1];
    c2["status"] = "running".into();
    c2["attempts"][0]["status"] = "running".into();
    // Its retry will succeed.
    saved["workflow"]["stages"][0]["fan_out"]["candidates"][1]["model"] = "lines-2".into();
    fs::write(&path, serde_json::to_vec_pretty(&saved).unwrap()).unwrap();
    let mut run = engine::load(dir.path(), &run.id).unwrap();
    assert_eq!(drive(&mut run), Action::Stopped);
    let c2 = &run.stages[0].fan_out.as_ref().unwrap().candidates[1];
    assert_eq!(c2.status, Status::Failed);
    assert!(c2.error.as_deref().unwrap().contains("interrupted"));
    assert_eq!(run.status, Status::Failed);
    let records = transitions::read(&run.directory).unwrap();
    assert!(records.iter().any(|r| {
        r.actor == Actor::Recovery
            && r.level == Level::Candidate
            && r.candidate.as_deref() == Some("c2")
    }));
    // Candidates run unattended, and a round has no session to take over.
    for cmd in [
        Cmd::Retry {
            interactive: true,
            model: None,
        },
        Cmd::TakeOver,
    ] {
        assert!(engine::command(dir.path(), &run.id, Some(run.revision), cmd).is_err());
    }
    // Only a stopped candidate can be retried.
    assert!(
        engine::command(
            dir.path(),
            &run.id,
            Some(run.revision),
            Cmd::RetryCandidate {
                stage: "implement".into(),
                candidate: "c9".into(),
            },
        )
        .is_err()
    );
    let mut run = engine::command(
        dir.path(),
        &run.id,
        Some(run.revision),
        Cmd::RetryCandidate {
            stage: "implement".into(),
            candidate: "c2".into(),
        },
    )
    .unwrap();
    assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);
    let state = run.stages[0].fan_out.as_ref().unwrap();
    assert_eq!(
        (
            state.candidates[0].attempts.len(),
            state.candidates[1].attempts.len()
        ),
        (1, 2),
        "c1 is not rerun"
    );
    assert_eq!(state.selection.as_ref().unwrap().candidate, "c2");
    assert_eq!(lines(&dir.path().join("out.txt")).len(), 2);
    assert_eq!(
        run.stages[0].attempts.len(),
        2,
        "each round is a stage attempt"
    );
}

#[test]
fn cleaning_a_run_removes_its_candidate_worktrees() {
    let Some((dir, mut run)) = git_run(vec![fan_out(
        &[("implement", "lines-1"), ("implement", "lines-2")],
        "auto",
    )]) else {
        return;
    };
    assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);
    let candidates = dir.path().join(".model-router/candidates").join(&run.id);
    assert!(candidates.is_dir());
    assert_eq!(git(dir.path(), &["worktree", "list"]).lines().count(), 3);
    let criteria = model_router::history::Criteria {
        ids: vec![run.id.clone()],
        ..Default::default()
    };
    let plan = model_router::history::cleanup_plan(dir.path(), &criteria).unwrap();
    assert!(
        plan.text(false).contains(".model-router/candidates/"),
        "{}",
        plan.text(false)
    );
    let done = model_router::history::apply(&plan);
    assert_eq!(done.items.len(), 1, "{}", done.text(true));
    assert!(!candidates.exists());
    assert_eq!(git(dir.path(), &["worktree", "list"]).lines().count(), 1);
    assert!(git(dir.path(), &["for-each-ref", "refs/model-router/"]).is_empty());
    // The applied change stays in the workspace.
    assert_eq!(lines(&dir.path().join("out.txt")).len(), 1);
}

#[test]
fn fan_outs_refuse_projects_outside_git() {
    let dir = tempdir();
    let workflow = workflow(vec![fan_out(&[("a", "x"), ("b", "y")], "auto")]);
    let error = Run::create(
        dir.path(),
        &settings(&workflow),
        &workflow,
        "Task".into(),
        false,
    )
    .unwrap_err();
    assert!(error.to_string().contains("Git repository"), "{error}");
}
