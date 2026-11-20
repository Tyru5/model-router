mod common;

use common::{count, drive, fixture, stage};
use model_router::{
    config::{Check, Role},
    engine::{self, Action, Cmd, Status},
    storage,
    transitions::{self, Actor, Level},
};
use std::{fs, path::Path};

/// Status changes of `level`, as (stage index, from, to).
fn moves(dir: &Path, level: Level) -> Vec<(Option<usize>, Status, Status)> {
    transitions::read(dir)
        .unwrap()
        .into_iter()
        .filter(|r| r.level == level)
        .map(|r| (r.stage_index, r.from, r.to))
        .collect()
}

#[test]
fn transitions_record_every_status_change_across_a_repair_loop() {
    let mut review = stage("review", Role::Review);
    review.extra_args = vec!["--review-fail-until".into(), "1".into()];
    let (_dir, _, mut run) = fixture(vec![stage("implementation", Role::Implementation), review]);
    assert_eq!(drive(&mut run), Action::Complete);
    let records = transitions::read(&run.directory).unwrap();
    assert!(records.iter().all(|r| r.actor == Actor::Engine));
    // Each record names the revision that saved it, in order.
    assert!(
        records
            .windows(2)
            .all(|w| w[0].run_revision <= w[1].run_revision)
    );
    let saved = engine::load(&run.project, &run.id).unwrap();
    assert!(records.iter().all(|r| r.run_revision <= saved.revision));
    use Status::*;
    assert_eq!(
        moves(&run.directory, Level::Run)
            .into_iter()
            .map(|(_, from, to)| (from, to))
            .collect::<Vec<_>>(),
        [
            (Pending, Running),
            (Running, Pending),
            (Pending, Running),
            // The review requested a repair.
            (Running, Pending),
            (Pending, Running),
            (Running, Pending),
            (Pending, Running),
            (Running, Pending),
            (Pending, Complete),
        ]
    );
    assert_eq!(
        moves(&run.directory, Level::Stage),
        [
            (Some(0), Pending, Running),
            (Some(0), Running, Complete),
            (Some(1), Pending, Running),
            // The repair resets the implementation and the review.
            (Some(0), Complete, Pending),
            (Some(1), Running, Pending),
            (Some(0), Pending, Running),
            (Some(0), Running, Complete),
            (Some(1), Pending, Running),
            (Some(1), Running, Complete),
        ]
    );
    let repair = records
        .iter()
        .find(|r| {
            r.level == Level::Attempt
                && r.stage_index == Some(1)
                && r.attempt == Some(1)
                && r.to == Complete
        })
        .unwrap();
    assert!(repair.reason.contains("requested a repair"), "{repair:?}");
    let report = model_router::report::render_transitions(&run, &records);
    assert!(report.contains("review #2"), "{report}");
}

/// A failed harness reports its exit code as a number, then its error.
#[test]
fn failed_harness_reports_its_exit_code() {
    let mut implement = stage("implementation", Role::Implementation);
    implement.model = "fail".into();
    let (_dir, _, mut run) = fixture(vec![implement]);
    assert_eq!(drive(&mut run), Action::Stopped);
    let error = run.error.clone().unwrap_or_default();
    assert!(
        error.starts_with("Harness exited with code 17. Simulated authentication"),
        "{error}"
    );
}

#[test]
fn command_holds_the_run_lock_and_refuses_stale_revisions() {
    let mut implement = stage("implementation", Role::Implementation);
    implement.model = "fail".into();
    let (dir, _, mut run) = fixture(vec![implement]);
    assert_eq!(drive(&mut run), Action::Stopped);
    let retry = || Cmd::Retry {
        interactive: false,
        model: Some("replacement".into()),
    };
    {
        let _held = storage::lock(&run.directory.join("run.lock")).unwrap();
        let error = engine::command(dir.path(), &run.id, Some(run.revision), retry()).unwrap_err();
        assert!(
            format!("{error:#}").contains("Another router process"),
            "{error:#}"
        );
    }
    let error = engine::command(dir.path(), &run.id, Some(run.revision - 1), retry()).unwrap_err();
    assert!(error.to_string().contains("changed in another process"));
    // Neither refusal changed the saved run.
    assert_eq!(
        engine::load(dir.path(), &run.id).unwrap().revision,
        run.revision
    );

    let before = run.revision;
    run.command(retry()).unwrap();
    assert_eq!(run.revision, before + 1);
    assert_eq!(run.status, Status::Pending);
    let saved = engine::load(dir.path(), &run.id).unwrap();
    assert_eq!(
        (
            saved.revision,
            saved.status,
            saved.workflow.stages[0].model.as_str()
        ),
        (run.revision, Status::Pending, "replacement")
    );
    let last = transitions::read(&run.directory).unwrap().pop().unwrap();
    assert_eq!(
        (
            last.level,
            last.from,
            last.to,
            last.actor,
            last.run_revision
        ),
        (
            Level::Run,
            Status::Failed,
            Status::Pending,
            Actor::User,
            run.revision
        )
    );
    assert!(last.reason.contains("replacement"));
    // The returned run drives without a revision conflict.
    assert_eq!(drive(&mut run), Action::Complete);
    assert!(engine::command(dir.path(), &run.id, None, Cmd::Confirm).is_err());
}

/// Installs a run.json saved before transitions were recorded into a fresh project.
fn old_run(name: &str) -> (tempfile::TempDir, String, serde_json::Value) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let text = fs::read_to_string(format!(
        "{}/tests/fixtures/runs/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
    .replace("{{PROJECT}}", root.to_str().unwrap());
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let id = value["id"].as_str().unwrap().to_owned();
    let run_dir = storage::run_dir(&root, &id).unwrap();
    fs::create_dir_all(&run_dir).unwrap();
    fs::write(run_dir.join("run.json"), &text).unwrap();
    (dir, id, value)
}

#[test]
fn runs_saved_before_transitions_load_unchanged_and_accept_commands() {
    let (dir, id, original) = old_run("blocked-repair-limit");
    let run = engine::load(dir.path(), &id).unwrap();
    assert_eq!(serde_json::to_value(&run).unwrap(), original);
    assert_eq!(
        (run.status, run.cursor, run.repair_passes),
        (Status::Blocked, 2, 2)
    );
    assert_eq!(run.stages[3].status, Status::Skipped);
    assert!(model_router::report::transitions(&run).unwrap().is_empty());
    let run = engine::command(dir.path(), &id, Some(run.revision), Cmd::AllowRepair).unwrap();
    assert_eq!(
        (run.status, run.cursor, run.extra_repair_passes),
        (Status::Pending, 1, 1)
    );
    assert_eq!(
        run.stages[2].attempts.last().unwrap().status,
        Status::Complete
    );
    let records = transitions::read(&run.directory).unwrap();
    assert!(records.iter().all(|r| r.actor == Actor::User));
    assert!(
        records
            .iter()
            .any(|r| r.level == Level::Attempt && r.from == Status::Blocked)
    );

    // An interactive attempt left awaiting confirmation by older versions.
    let (dir, id, original) = old_run("awaiting-placeholder");
    let run = engine::load(dir.path(), &id).unwrap();
    assert_eq!(serde_json::to_value(&run).unwrap(), original);
    assert_eq!(
        run.stages[0].attempts[0].status,
        Status::AwaitingConfirmation
    );
    let run = engine::command(dir.path(), &id, None, Cmd::Confirm).unwrap();
    assert_eq!((run.status, run.cursor), (Status::Pending, 1));
    assert_eq!(run.stages[0].attempts[0].status, Status::Complete);
    assert_eq!(run.stages[0].status, Status::Complete);
}

fn implementation_count_at_least(n: u32) -> Check {
    Check {
        command: vec![
            "python3".into(),
            "-c".into(),
            format!(
                "import pathlib, sys; sys.exit(0 if int(pathlib.Path('.model-router/mock/implementation.count').read_text()) >= {n} else 1)"
            ),
        ],
        ..Check::default()
    }
}

#[test]
fn a_failed_required_check_in_a_review_requests_a_repair() {
    let mut review = stage("review", Role::Review);
    review.checks = vec![implementation_count_at_least(2)];
    let (dir, _, mut run) = fixture(vec![stage("implementation", Role::Implementation), review]);
    assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);
    assert_eq!(run.repair_passes, 1);
    assert_eq!(count(dir.path(), "implementation"), 2);
    let prompt =
        fs::read_to_string(dir.path().join(".model-router/mock/implementation.prompt")).unwrap();
    assert!(
        prompt.contains("Router checks: Required check failed"),
        "{prompt}"
    );

    // Without repairs left it blocks, and --allow-repair grants another pass.
    let mut review = stage("review", Role::Review);
    review.checks = vec![implementation_count_at_least(2)];
    let (dir, _, mut run) = fixture(vec![stage("implementation", Role::Implementation), review]);
    run.workflow.max_repair_passes = 0;
    run.persist().unwrap();
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!((run.status, run.cursor), (Status::Blocked, 1));
    let error = run.error.clone().unwrap();
    assert!(
        error.contains("--allow-repair") && error.contains("Required check failed"),
        "{error}"
    );
    run.command(Cmd::AllowRepair).unwrap();
    assert_eq!((run.status, run.cursor), (Status::Pending, 0));
    assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);
    assert_eq!(count(dir.path(), "implementation"), 2);

    // A review with no upstream implementation still blocks on the check.
    let mut review = stage("review", Role::Review);
    review.checks = vec![implementation_count_at_least(1)];
    let (_dir, _, mut run) = fixture(vec![review]);
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!(run.status, Status::Blocked);
    assert!(
        run.error
            .as_deref()
            .unwrap()
            .contains("no enabled upstream")
    );
}
