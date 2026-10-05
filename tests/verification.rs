mod common;

use common::{count, drive, fixture, stage};
use model_router::{
    config::{Check, Role, Settings, Stage},
    engine::{self, Action, Event, Status},
    handoff::TestStatus,
};
use std::{
    fs,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

fn verify(args: &[&str]) -> Stage {
    Stage {
        extra_args: args.iter().map(|a| (*a).into()).collect(),
        ..stage("verify", Role::Test)
    }
}
fn python(code: &str, required: bool) -> Check {
    Check {
        command: vec!["python3".into(), "-c".into(), code.into()],
        required,
        ..Check::default()
    }
}

#[test]
fn empty_test_evidence_blocks_before_downstream_stages() {
    let (dir, _, mut run) = fixture(vec![
        stage("implementation", Role::Implementation),
        verify(&[]),
        stage("handoff", Role::Custom),
    ]);
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!((run.status, run.cursor), (Status::Blocked, 1));
    assert!(run.stages[1].handoff.as_ref().unwrap().tests.is_empty());
    let error = run.error.as_deref().unwrap();
    assert!(
        error.contains("Test stage") && error.contains("no passing required check"),
        "{error}"
    );
    assert_eq!(count(dir.path(), "handoff"), 0);
}

#[test]
fn optional_passing_checks_are_not_evidence() {
    let mut verify = verify(&["--test-status", "passed", "--test-optional"]);
    verify.checks = vec![python("pass", false)];
    let (dir, _, mut run) = fixture(vec![verify, stage("handoff", Role::Custom)]);
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!(run.status, Status::Blocked);
    assert!(!run.stages[0].handoff.as_ref().unwrap().tests[0].required);
    assert_eq!(
        run.stages[0].attempts[0].checks[0].status,
        TestStatus::Passed
    );
    assert!(run.error.as_deref().unwrap().contains("Test stage"));
    assert_eq!(count(dir.path(), "handoff"), 0);
}

#[test]
fn not_applicable_waits_for_acceptance_and_records_it() {
    let (dir, _, mut run) = fixture(vec![
        verify(&["--not-applicable", "Documentation-only change"]),
        stage("handoff", Role::Custom),
    ]);
    assert_eq!(drive(&mut run), Action::Confirm);
    assert_eq!(run.status, Status::AwaitingConfirmation);
    assert_eq!(run.pending_waiver(), Some("Documentation-only change"));
    assert!(run.stages[0].waiver.is_none());
    assert_eq!(
        drive(&mut run),
        Action::Confirm,
        "Driving is not acceptance"
    );
    assert_eq!(count(dir.path(), "handoff"), 0);
    run.confirm().unwrap();
    assert_eq!(drive(&mut run), Action::Complete);
    assert_eq!(count(dir.path(), "verify"), 1);
    assert_eq!(count(dir.path(), "handoff"), 1);
    let saved = engine::load(dir.path(), &run.id).unwrap();
    let waiver = saved.stages[0].waiver.as_ref().unwrap();
    assert_eq!(waiver.reason, "Documentation-only change");
    assert!(waiver.accepted_at >= saved.stages[0].attempts[0].started_at);
    assert_eq!(saved.stages[0].status, Status::Complete);
    assert_eq!(saved.pending_waiver(), None);
}

#[test]
fn repairs_and_retries_require_fresh_acceptance() {
    let mut review = stage("review", Role::Review);
    review.extra_args = vec!["--review-fail-until".into(), "1".into()];
    let (dir, _, mut run) = fixture(vec![
        stage("implementation", Role::Implementation),
        verify(&["--not-applicable", "No runnable checks"]),
        review,
    ]);
    assert_eq!(drive(&mut run), Action::Confirm);
    run.confirm().unwrap();
    assert!(run.stages[1].waiver.is_some());
    // The review's repair reruns verification, which must pause again.
    assert_eq!(drive(&mut run), Action::Confirm);
    assert_eq!((run.cursor, run.repair_passes), (1, 1));
    assert!(run.stages[1].waiver.is_none());
    assert_eq!(run.pending_waiver(), Some("No runnable checks"));
    run.retry(false, None).unwrap();
    assert_eq!(run.pending_waiver(), None);
    assert_eq!(drive(&mut run), Action::Confirm);
    assert!(run.stages[1].waiver.is_none());
    run.confirm().unwrap();
    assert_eq!(drive(&mut run), Action::Complete);
    assert_eq!(
        [
            count(dir.path(), "implementation"),
            count(dir.path(), "verify"),
            count(dir.path(), "review")
        ],
        [2, 3, 2]
    );
    let saved = engine::load(dir.path(), &run.id).unwrap();
    assert_eq!(saved.stages[1].attempts.len(), 3);
    assert!(saved.stages[1].waiver.is_some());
}

#[test]
fn passing_required_router_check_is_evidence() {
    let mut verify = verify(&[]);
    verify.checks = vec![python("print('router check ran')", true)];
    let (dir, _, mut run) = fixture(vec![verify, stage("handoff", Role::Custom)]);
    let mut output = String::new();
    let action = engine::drive(&mut run, &AtomicBool::new(false), |event| {
        if let Event::Output(text) = event {
            output.push_str(&text);
        }
    })
    .unwrap();
    assert_eq!(action, Action::Complete, "{:?}", run.error);
    assert!(run.stages[0].handoff.as_ref().unwrap().tests.is_empty());
    let attempt = &run.stages[0].attempts[0];
    let check = &attempt.checks[0];
    assert_eq!(
        (check.status, check.exit_code, check.required),
        (TestStatus::Passed, Some(0), true)
    );
    assert_eq!(check.directory, attempt.directory.join("checks/01"));
    assert_eq!(
        fs::read_to_string(check.directory.join("stdout.log")).unwrap(),
        "router check ran\n"
    );
    assert!(output.contains("[check 1/1] python3 -c") && output.contains("router check ran"));
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(attempt.directory.join("checks.json")).unwrap()).unwrap();
    assert_eq!(saved[0]["status"], "passed");
    let prompt = fs::read_to_string(dir.path().join(".model-router/mock/verify.prompt")).unwrap();
    assert!(prompt.contains("ROUTER CHECKS") && prompt.contains("python3 -c"));
    assert!(prompt.contains("(required)"));
    assert_eq!(count(dir.path(), "handoff"), 1);
}

#[test]
fn failing_required_router_check_blocks_with_logs() {
    let mut verify = verify(&["--test-status", "passed"]);
    verify.checks = vec![python(
        "import sys; print('router saw a failure'); sys.exit(3)",
        true,
    )];
    let (dir, _, mut run) = fixture(vec![verify, stage("handoff", Role::Custom)]);
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!(run.status, Status::Blocked);
    let check = &run.stages[0].attempts[0].checks[0];
    assert_eq!(
        (check.status, check.exit_code),
        (TestStatus::Failed, Some(3))
    );
    let error = run.error.as_deref().unwrap();
    assert!(
        error.contains("Required check failed: python3 -c")
            && error.contains("(exit 3)")
            && error.contains(&check.directory.display().to_string()),
        "{error}"
    );
    assert!(
        fs::read_to_string(check.directory.join("stdout.log"))
            .unwrap()
            .contains("router saw a failure")
    );
    assert!(check.directory.join("stderr.log").is_file());
    assert_eq!(count(dir.path(), "handoff"), 0);
}

#[test]
fn failing_optional_checks_are_recorded_without_blocking() {
    let mut implement = stage("implementation", Role::Implementation);
    implement.checks = vec![
        python("import sys; sys.exit(1)", false),
        Check {
            command: vec!["model-router-missing-check".into()],
            required: false,
            ..Check::default()
        },
    ];
    let (dir, _, mut run) = fixture(vec![implement, stage("handoff", Role::Custom)]);
    assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);
    let checks = &run.stages[0].attempts[0].checks;
    assert_eq!(
        (checks[0].status, checks[0].exit_code),
        (TestStatus::Failed, Some(1))
    );
    assert_eq!(checks[1].status, TestStatus::Failed);
    assert!(
        checks[1]
            .error
            .as_deref()
            .unwrap()
            .contains("Launch model-router-missing-check")
    );
    assert_eq!(count(dir.path(), "handoff"), 1);
    let saved = engine::load(dir.path(), &run.id).unwrap();
    assert_eq!(saved.stages[0].attempts[0].checks.len(), 2);
}

#[test]
fn timed_out_check_fails_and_blocks() {
    let mut implement = stage("implementation", Role::Implementation);
    implement.checks = vec![Check {
        timeout_seconds: 1,
        ..python("import time; time.sleep(30)", true)
    }];
    let (_dir, _, mut run) = fixture(vec![implement]);
    let started = Instant::now();
    assert_eq!(drive(&mut run), Action::Stopped);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(run.status, Status::Blocked);
    let check = &run.stages[0].attempts[0].checks[0];
    assert_eq!(check.status, TestStatus::Failed);
    assert_eq!(check.error.as_deref(), Some("Timed out after 1s"));
    assert!(run.error.as_deref().unwrap().contains("Timed out after 1s"));
}

#[test]
fn non_test_stage_failing_required_check_blocks() {
    let mut research = stage("research", Role::Research);
    research.checks = vec![python("pass", true), python("raise SystemExit(2)", true)];
    let (dir, _, mut run) = fixture(vec![
        research,
        stage("implementation", Role::Implementation),
    ]);
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!((run.status, run.cursor), (Status::Blocked, 0));
    assert!(run.error.as_deref().unwrap().contains("(exit 2)"));
    assert!(run.error.as_deref().unwrap().contains("checks/02"));
    assert_eq!(count(dir.path(), "implementation"), 0);
}

#[test]
fn cancelling_a_check_cancels_the_run() {
    let mut implement = stage("implementation", Role::Implementation);
    implement.checks = vec![
        python(
            "import pathlib, time; pathlib.Path('check-started').touch(); time.sleep(30)",
            true,
        ),
        python("pass", true),
    ];
    let (dir, _, mut run) = fixture(vec![implement]);
    let cancel = Arc::new(AtomicBool::new(false));
    let signal = cancel.clone();
    let marker = dir.path().join("check-started");
    let watcher = thread::spawn(move || {
        let start = Instant::now();
        while !marker.exists() && start.elapsed() < Duration::from_secs(10) {
            thread::sleep(Duration::from_millis(20));
        }
        signal.store(true, Ordering::Relaxed);
    });
    let started = Instant::now();
    assert_eq!(
        engine::drive(&mut run, &cancel, |_| {}).unwrap(),
        Action::Stopped
    );
    watcher.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(15));
    assert_eq!(run.status, Status::Cancelled);
    let checks = &run.stages[0].attempts[0].checks;
    assert_eq!(
        checks.len(),
        1,
        "Checks after a cancellation must not start"
    );
    assert_eq!(checks[0].status, TestStatus::NotRun);
    assert!(
        run.error
            .as_deref()
            .unwrap()
            .contains("cancelled during checks")
    );
}

#[test]
fn preview_lists_router_checks() {
    let mut verify = verify(&[]);
    verify.checks = vec![Check {
        timeout_seconds: 30,
        ..python("pass", false)
    }];
    let (dir, settings, _) = fixture(vec![verify]);
    let text = engine::preview(&settings, &settings.workflows[0], dir.path(), "Task").unwrap();
    assert!(
        text.contains("   check: python3 -c pass (optional, 30s)"),
        "{text}"
    );
}

const OLD_CONFIG: &str = r#"version = 1

[adapters.mock]
command = "python3"
args = ["mock_harness.py", "--prompt-file", "{prompt_file}", "--handoff-file", "{handoff_file}"]

[[workflows]]
name = "Old"

[[workflows.stages]]
id = "verify"
name = "Verify"
role = "test"
harness = "mock"
extra_args = ["--test-status", "passed"]
"#;

// Written by the router before checks, waivers, and not_applicable existed.
const OLD_RUN: &str = r#"{
  "version": 1, "revision": 6, "id": "20261005-014038-e79bc622",
  "created_at": "2026-10-05T01:40:38.501629249Z", "updated_at": "2026-10-05T01:40:38.584932326Z",
  "project": "{dir}", "workspace": "{dir}", "directory": "{dir}/.model-router/runs/20261005-014038-e79bc622",
  "worktree": false, "task": "Old run",
  "workflow": {"name": "Old", "max_repair_passes": 2, "stages": [
    {"id": "implement", "name": "Implement", "role": "implementation", "enabled": true, "harness": "mock", "model": "m",
     "provider": "implement", "effort": "", "mode": "unattended", "instructions": "", "extra_args": [],
     "timeout_seconds": 3600, "repair_target": ""},
    {"id": "verify", "name": "Verify", "role": "test", "enabled": true, "harness": "mock", "model": "m",
     "provider": "verify", "effort": "", "mode": "unattended", "instructions": "", "extra_args": ["--test-status", "passed"],
     "timeout_seconds": 3600, "repair_target": ""}]},
  "adapters": {"mock": {"command": "python3", "args": ["mock_harness.py"], "interactive_args": null, "models_args": null, "prompt_stdin": false}},
  "stages": [
    {"status": "complete", "attempts": [{"number": 1, "started_at": "2026-10-05T01:40:38.510441991Z",
      "finished_at": "2026-10-05T01:40:38.540308916Z", "model": "m", "mode": "unattended", "status": "complete", "exit_code": 0,
      "directory": "{dir}/.model-router/runs/20261005-014038-e79bc622/01-implement/attempt-001",
      "handoff_file": "{dir}/.model-router/handoffs/20261005-014038-e79bc622/implement-001.json", "error": null}],
     "handoff": {"summary": "implement evidence", "verdict": "complete", "findings": [], "decisions": [], "files_changed": [],
      "tests": [], "unresolved": [], "structured": true}},
    {"status": "complete", "attempts": [{"number": 1, "started_at": "2026-10-05T01:40:38.550363900Z",
      "finished_at": "2026-10-05T01:40:38.580342379Z", "model": "m", "mode": "unattended", "status": "complete", "exit_code": 0,
      "directory": "{dir}/.model-router/runs/20261005-014038-e79bc622/02-verify/attempt-001",
      "handoff_file": "{dir}/.model-router/handoffs/20261005-014038-e79bc622/verify-001.json", "error": null}],
     "handoff": {"summary": "verify evidence", "verdict": "complete", "findings": [], "decisions": [], "files_changed": [],
      "tests": [{"command": "offline acceptance check", "status": "passed", "required": true}], "unresolved": [], "structured": true}}],
  "cursor": 2, "repair_passes": 0, "repair_feedback": null, "status": "complete", "error": null
}"#;

#[test]
fn records_and_configs_without_new_fields_still_load() {
    let dir = tempfile::tempdir().unwrap();
    let id = "20261005-014038-e79bc622";
    let run_dir = dir.path().join(".model-router/runs").join(id);
    fs::create_dir_all(&run_dir).unwrap();
    fs::write(
        run_dir.join("run.json"),
        OLD_RUN.replace("{dir}", &dir.path().display().to_string()),
    )
    .unwrap();
    let run = engine::load(dir.path(), id).unwrap();
    assert_eq!(run.status, Status::Complete);
    assert!(run.stages.iter().all(|s| s.waiver.is_none()));
    assert!(
        run.stages
            .iter()
            .flat_map(|s| &s.attempts)
            .all(|a| a.checks.is_empty())
    );
    assert!(
        run.stages[1]
            .handoff
            .as_ref()
            .unwrap()
            .not_applicable
            .is_none()
    );
    assert!(run.workflow.stages.iter().all(|s| s.checks.is_empty()));
    // Unused fields are not written back.
    let json = serde_json::to_value(&run).unwrap();
    for (stage, state) in json["workflow"]["stages"]
        .as_array()
        .unwrap()
        .iter()
        .zip(json["stages"].as_array().unwrap())
    {
        assert!(stage.get("checks").is_none() && state.get("waiver").is_none());
        assert!(state["handoff"].get("not_applicable").is_none());
        assert!(state["attempts"][0].get("checks").is_none());
    }
    let settings: Settings = toml::from_str(OLD_CONFIG).unwrap();
    settings.validate().unwrap();
    assert!(settings.workflows[0].stages[0].checks.is_empty());
    let saved: toml::Value = toml::from_str(&toml::to_string_pretty(&settings).unwrap()).unwrap();
    assert!(saved["workflows"][0]["stages"][0].get("checks").is_none());
}

#[test]
fn cli_requires_explicit_advance_for_not_applicable_verification() {
    let (dir, settings, _) = fixture(vec![
        verify(&["--not-applicable", "Docs only"]),
        stage("handoff", Role::Custom),
    ]);
    let config = dir.path().join("config.toml");
    settings.save(&config).unwrap();
    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_model-router"))
            .arg("-C")
            .arg(dir.path())
            .arg("--config")
            .arg(&config)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let output = cli(&["run", "--task", "Update the docs"]);
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Run: "))
        .unwrap()
        .to_owned();
    assert!(stdout.contains("Not applicable: Docs only"), "{stdout}");
    assert!(
        stderr.contains("awaits acceptance of a not-applicable verification")
            && stderr.contains(&format!("resume {id} --advance")),
        "{stderr}"
    );
    assert_eq!(count(dir.path(), "handoff"), 0);
    let output = cli(&["resume", &id, "--advance"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = engine::load(dir.path(), &id).unwrap();
    assert_eq!(run.status, Status::Complete);
    assert_eq!(run.stages[0].waiver.as_ref().unwrap().reason, "Docs only");
    assert_eq!(count(dir.path(), "handoff"), 1);
}
