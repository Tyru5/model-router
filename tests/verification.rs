mod common;

use common::{count, drive, fixture, stage};
use model_router::{
    config::{Check, CustomAdapter, Execution, Mode, Role, Stage},
    engine::{self, Action, Cmd, Event, Status},
    handoff::{HandoffSource, TestStatus, Verdict},
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
    // v1 configs migrate on load.
    let settings = model_router::config::parse(OLD_CONFIG).unwrap();
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

#[test]
fn require_router_checks_ignores_model_reported_passes() {
    // The model claims a passing required test; the router ran nothing.
    let mut strict = verify(&["--test-status", "passed"]);
    strict.require_router_checks = true;
    let (dir, _, mut run) = fixture(vec![strict, stage("handoff", Role::Custom)]);
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!(run.status, Status::Blocked);
    let error = run.error.as_deref().unwrap();
    assert!(
        error.contains("require_router_checks") && error.contains("model-reported passes"),
        "{error}"
    );
    assert_eq!(count(dir.path(), "handoff"), 0);

    // An actual required router pass satisfies it.
    let mut strict = verify(&["--test-status", "passed"]);
    strict.require_router_checks = true;
    strict.checks = vec![python("pass", true)];
    let (dir, _, mut run) = fixture(vec![strict, stage("handoff", Role::Custom)]);
    assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);
    assert_eq!(count(dir.path(), "handoff"), 1);

    // An explicit not-applicable waiver still applies.
    let mut strict = verify(&["--not-applicable", "Docs only"]);
    strict.require_router_checks = true;
    let (_dir, _, mut run) = fixture(vec![strict]);
    assert_eq!(drive(&mut run), Action::Confirm);
    assert_eq!(run.pending_waiver(), Some("Docs only"));
}

#[test]
fn require_router_checks_is_for_test_stages_only() {
    let mut implement = stage("implementation", Role::Implementation);
    implement.require_router_checks = true;
    let workflow = common::workflow(vec![implement]);
    let error = common::settings(&workflow).validate().unwrap_err();
    assert!(error.to_string().contains("Only test stages"), "{error}");
}

fn receipt(run: &engine::Run, stage: &str) -> model_router::report::Receipt {
    model_router::report::receipts(run)
        .into_iter()
        .find(|r| r.stage == stage)
        .unwrap()
}

#[test]
fn receipts_bind_router_checks_to_the_tested_snapshot() {
    use model_router::report::{Basis, Freshness};
    let dir = common::tempdir();
    if common::repository(dir.path()).is_none() {
        return;
    }
    let mut implement = stage("implementation", Role::Implementation);
    implement.extra_args = vec!["--write-file".into(), "made.txt".into()];
    let mut checked = verify(&[]);
    checked.checks = vec![python("pass", true)];
    let claimed = Stage {
        id: "claimed".into(),
        name: "claimed".into(),
        extra_args: vec!["--test-status".into(), "passed".into()],
        ..stage("claimed", Role::Test)
    };
    let workflow = common::workflow(vec![implement, checked, claimed]);
    let settings = common::settings(&workflow);
    let mut run = common::create(dir.path(), &settings, &workflow);
    assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);

    let tested = run.stages[1].attempts[0].checks_tree.clone().unwrap();
    let verified = receipt(&run, "verify");
    assert_eq!(verified.basis, Basis::RouterChecks);
    assert_eq!((verified.required_passed, verified.required_total), (1, 1));
    assert_eq!(verified.snapshot.as_deref(), Some(tested.as_str()));
    assert_eq!(verified.freshness, Some(Freshness::Current));
    // A model claim with no router check is labelled, not trusted.
    let claimed = receipt(&run, "claimed");
    assert_eq!(claimed.basis, Basis::SelfReportedOnly);
    assert_eq!(claimed.model_reported_passed, 1);
    assert_eq!(claimed.freshness, None);
    // The run's own implementation stage has no checks: no receipt.
    assert!(
        model_router::report::receipts(&run)
            .iter()
            .all(|r| r.stage != "implementation")
    );

    // Editing code after the checks makes the receipt stale, naming files.
    let made = dir.path().join("made.txt");
    let original = fs::read_to_string(&made).unwrap();
    fs::write(&made, "edited later\n").unwrap();
    match receipt(&run, "verify").freshness {
        Some(Freshness::Stale { changed }) => assert_eq!(changed, ["made.txt"]),
        other => panic!("expected stale, got {other:?}"),
    }
    let text = model_router::report::render_receipts(&model_router::report::receipts(&run));
    assert!(
        text.contains("stale: 1 file changed since the checks ran (made.txt)"),
        "{text}"
    );
    assert!(text.contains("self-reported only"), "{text}");
    assert!(text.contains("not that they are adequate"), "{text}");
    // Freshness compares content, not time: reverting makes it current again.
    fs::write(&made, original).unwrap();
    assert_eq!(receipt(&run, "verify").freshness, Some(Freshness::Current));

    // Once the run's snapshot refs are gone and Git prunes, it is unavailable.
    model_router::snapshot::forget(dir.path(), &run.id);
    let gc = Command::new("git")
        .args(["gc", "-q", "--prune=now"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(gc.success());
    match receipt(&run, "verify").freshness {
        Some(Freshness::Unavailable { reason }) => {
            assert!(reason.contains("no longer in the repository"), "{reason}")
        }
        other => panic!("expected unavailable, got {other:?}"),
    }
}

#[test]
fn receipts_outside_git_say_freshness_is_unavailable() {
    use model_router::report::{Basis, Freshness};
    let mut checked = verify(&[]);
    checked.checks = vec![python("pass", true)];
    let (_dir, _, mut run) = fixture(vec![checked]);
    assert_eq!(drive(&mut run), Action::Complete);
    let receipt = receipt(&run, "verify");
    assert_eq!(receipt.basis, Basis::RouterChecks);
    assert_eq!(receipt.snapshot, None);
    assert_eq!(
        receipt.freshness,
        Some(Freshness::Unavailable {
            reason: "not a Git repository".into()
        })
    );
    // A required check with no result (a cancel stopped the checks after
    // the first) is not a pass of the ones that happened to run.
    run.workflow.stages[0].checks.push(python("pass", true));
    let partial = model_router::report::receipts(&run).remove(0);
    assert_eq!(partial.basis, Basis::RouterChecksFailed);
    assert_eq!((partial.required_passed, partial.required_total), (1, 2));
}

/// A test stage whose checks the router runs in place of a harness.
fn checks_only(checks: Vec<Check>) -> Stage {
    Stage {
        execution: Execution::ChecksOnly,
        checks,
        ..stage("verify", Role::Test)
    }
}

#[test]
fn checks_only_stage_runs_its_checks_and_never_launches_a_harness() {
    let mut verify = checks_only(vec![
        python("print('router check ran')", true),
        python("import sys; sys.exit(4)", false),
    ]);
    // Its harness is not installed: nothing looks for it or starts it.
    verify.harness = "absent".into();
    let workflow = common::workflow(vec![
        stage("implementation", Role::Implementation),
        verify,
        stage("handoff", Role::Custom),
    ]);
    let mut settings = common::settings(&workflow);
    settings.adapters.insert(
        "absent".into(),
        CustomAdapter {
            command: "/nonexistent/model-router-test-harness".into(),
            ..CustomAdapter::default()
        },
    );
    let dir = common::tempdir();
    let mut run = common::create(dir.path(), &settings, &workflow);
    let mut output = String::new();
    let action = engine::drive(&mut run, &AtomicBool::new(false), |event| {
        if let Event::Output(text) = event {
            output.push_str(&text);
        }
    })
    .unwrap();
    assert_eq!(action, Action::Complete, "{:?}", run.error);
    assert_eq!(
        ["implementation", "verify", "handoff"].map(|role| count(dir.path(), role)),
        [1, 0, 1]
    );
    assert!(
        output.contains("verify · attempt 1 · router checks, no harness")
            && output.contains("router check ran"),
        "{output}"
    );

    let state = &run.stages[1];
    assert_eq!((state.status, state.attempts.len()), (Status::Complete, 1));
    let attempt = &state.attempts[0];
    assert_eq!(
        attempt
            .checks
            .iter()
            .map(|c| (c.required, c.status, c.exit_code))
            .collect::<Vec<_>>(),
        [
            (true, TestStatus::Passed, Some(0)),
            (false, TestStatus::Failed, Some(4))
        ]
    );
    assert_eq!(attempt.handoff_source, Some(HandoffSource::Router));
    assert!(attempt.model.is_empty() && attempt.usage.is_none() && attempt.session.is_none());
    // No prompt was built and nothing was sent anywhere.
    assert!(!attempt.directory.join("prompt.md").exists());
    assert!(!attempt.directory.join("stdout.log").exists());
    let command: serde_json::Value =
        serde_json::from_slice(&fs::read(attempt.directory.join("command.json")).unwrap()).unwrap();
    assert_eq!(command["execution"], "checks-only");
    assert!(command.get("program").is_none());

    // The handoff is the router's: results stay on the attempt, and `tests`
    // (what a model reports) stays empty.
    let handoff = state.handoff.as_ref().unwrap();
    assert_eq!(handoff.verdict, Verdict::Complete);
    assert!(handoff.tests.is_empty() && handoff.structured);
    assert!(
        handoff
            .summary
            .starts_with("Router checks: 1/1 required passed, 0/1 optional passed. No harness ran"),
        "{}",
        handoff.summary
    );
    // Failures are in the summary once, with their logs; the report reads
    // the results themselves, so the handoff repeats none as findings.
    assert!(handoff.findings.is_empty());
    assert!(
        handoff.summary.contains("(optional): exit 4. Logs: ")
            && handoff.summary.contains("(required): passed"),
        "{}",
        handoff.summary
    );
    assert!(attempt.directory.join("handoff.json").is_file());
    let prompt = fs::read_to_string(dir.path().join(".model-router/mock/handoff.prompt")).unwrap();
    assert!(
        prompt.contains("PRIOR STAGE: verify") && prompt.contains("1/1 required passed"),
        "{prompt}"
    );

    let report = model_router::report::build(&run);
    assert_eq!(report.stages[1].execution, Execution::ChecksOnly);
    assert_eq!(report.stages[1].handoff_source, Some(HandoffSource::Router));
    let text = model_router::report::render(&report, None);
    assert!(
        text.contains("router checks, no harness · handoff: router"),
        "{text}"
    );
    // Its attempt ran no model: it adds no usage, and is not counted as an
    // attempt whose cost is unknown.
    assert_eq!(report.stages[1].attempts, 1);
    assert_eq!(report.stages[1].usage.attempts, 0);
    assert_eq!(report.usage.attempts, 2);
    let pricing = model_router::cost::Pricing::load(dir.path());
    let totals = model_router::cost::stage_totals(&run, &pricing);
    assert_eq!(
        totals.iter().map(|t| t.attempts).collect::<Vec<_>>(),
        [1, 0, 1]
    );
    let receipt = receipt(&run, "verify");
    assert_eq!(receipt.basis, model_router::report::Basis::RouterChecks);
    assert_eq!(receipt.model_reported_passed, 0);
}

#[test]
fn checks_only_failing_required_check_blocks_until_a_retry_passes() {
    let (dir, _, mut run) = fixture(vec![
        checks_only(vec![python(
            "import os, sys; sys.exit(0 if os.path.exists('fixed') else 3)",
            true,
        )]),
        stage("handoff", Role::Custom),
    ]);
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!((run.status, run.cursor), (Status::Blocked, 0));
    let error = run.error.clone().unwrap();
    assert!(
        error.contains("Required check failed: python3 -c") && error.contains("(exit 3)"),
        "{error}"
    );
    let handoff = run.stages[0].handoff.as_ref().unwrap();
    assert_eq!(handoff.verdict, Verdict::NeedsChanges);
    assert!(handoff.summary.contains("0/1 required passed"));
    assert_eq!(count(dir.path(), "handoff"), 0);

    // No harness exists to reopen or to give another model.
    for cmd in [
        Cmd::Retry {
            interactive: true,
            model: None,
        },
        Cmd::Retry {
            interactive: false,
            model: Some("other".into()),
        },
        Cmd::TakeOver,
    ] {
        let error = engine::command(dir.path(), &run.id, Some(run.revision), cmd).unwrap_err();
        assert!(error.to_string().contains("Checks-only"), "{error:#}");
    }

    fs::write(dir.path().join("fixed"), "").unwrap();
    run = engine::command(
        dir.path(),
        &run.id,
        Some(run.revision),
        Cmd::Retry {
            interactive: false,
            model: None,
        },
    )
    .unwrap();
    assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);
    assert_eq!(run.stages[0].attempts.len(), 2);
    assert_eq!(
        run.stages[0].handoff.as_ref().unwrap().verdict,
        Verdict::Complete
    );
    assert_eq!(
        (count(dir.path(), "verify"), count(dir.path(), "handoff")),
        (0, 1)
    );
}

#[test]
fn cancelling_a_checks_only_stage_cancels_the_run() {
    let (dir, _, mut run) = fixture(vec![
        checks_only(vec![
            python(
                "import pathlib, time; pathlib.Path('check-started').touch(); time.sleep(30)",
                true,
            ),
            python("pass", true),
        ]),
        stage("handoff", Role::Custom),
    ]);
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
    let attempt = &run.stages[0].attempts[0];
    assert_eq!(attempt.status, Status::Cancelled);
    assert_eq!(attempt.checks.len(), 1, "The second check must not start");
    assert!(attempt.process_group.is_none());
    // A cancelled attempt wrote no handoff, and nothing downstream ran.
    assert!(run.stages[0].handoff.is_none());
    assert!(
        run.error
            .as_deref()
            .unwrap()
            .contains("cancelled during checks")
    );
    assert_eq!(count(dir.path(), "handoff"), 0);
}

#[test]
fn checks_only_is_for_unattended_test_stages_with_a_required_check() {
    let rejected = |edit: fn(&mut Stage)| {
        let mut stage = checks_only(vec![python("pass", true)]);
        edit(&mut stage);
        let workflow = common::workflow(vec![stage]);
        format!("{:#}", common::settings(&workflow).validate().unwrap_err())
    };
    assert!(rejected(|s| s.role = Role::Implementation).contains("Only test stages"));
    assert!(rejected(|s| s.mode = Mode::Interactive).contains("cannot be interactive"));
    assert!(rejected(|s| s.checks[0].required = false).contains("at least one required check"));
    assert!(rejected(|s| s.checks.clear()).contains("at least one required check"));
    // A disabled draft may still lack its checks.
    let mut draft = checks_only(vec![]);
    draft.enabled = false;
    let workflow = common::workflow(vec![stage("implementation", Role::Implementation), draft]);
    common::settings(&workflow).validate().unwrap();

    // The field is omitted for ordinary stages, so older configs round-trip.
    let plain = toml::to_string(&stage("verify", Role::Test)).unwrap();
    assert!(!plain.contains("execution"), "{plain}");
    let saved = toml::to_string(&checks_only(vec![python("pass", true)])).unwrap();
    assert!(saved.contains("execution = \"checks-only\""), "{saved}");
    let parsed: Stage = toml::from_str(&saved).unwrap();
    assert!(parsed.checks_only());
}

#[test]
fn preview_shows_a_checks_only_stage_without_a_harness_command() {
    let (dir, settings, _) = fixture(vec![
        stage("implementation", Role::Implementation),
        checks_only(vec![python("pass", true)]),
    ]);
    let text = engine::preview(&settings, &settings.workflows[0], dir.path(), "Task").unwrap();
    assert!(
        text.contains("2. verify | test | router checks, no harness\n   check: python3 -c pass (required, 600s)"),
        "{text}"
    );
    assert_eq!(text.matches("mock_harness.py").count(), 1, "{text}");
}

const CHECKS_ONLY_PROJECT: &str = r#"version = 2

[[workflows]]
name = "Checks only"

[[workflows.stages]]
id = "verify"
name = "Verify"
role = "test"
execution = "checks-only"
checks = [{ command = ["python3", "-c", "open('checked', 'w').close()"] }]

[[workflows.stages]]
id = "summary"
name = "Summary"
role = "summary"
enabled = false
"#;

#[test]
fn untrusted_checks_only_stage_refuses_to_run_instead_of_launching_its_harness() {
    use model_router::{config, engine::Run, trust};
    common::isolate_config_dir();
    let dir = common::tempdir();
    fs::write(dir.path().join(".model-router.toml"), CHECKS_ONLY_PROJECT).unwrap();
    let create = |settings: &config::Settings| {
        Run::create(
            dir.path(),
            settings,
            &settings.workflows[0],
            "Verify".into(),
            false,
        )
    };

    // Untrusted: the checks are ignored, the config still loads, and the
    // stage has nothing to run. Its default harness is never the fallback.
    let (untrusted, _) = config::load(dir.path(), None).unwrap();
    let stage = &untrusted.workflows[0].stages[0];
    assert!(stage.checks_only() && stage.checks.is_empty());
    let text = engine::preview(&untrusted, &untrusted.workflows[0], dir.path(), "Task").unwrap();
    assert!(text.contains("no required check"), "{text}");
    let error = format!("{:#}", create(&untrusted).unwrap_err());
    assert!(
        error.contains("has no required check to run") && error.contains("model-router trust"),
        "{error}"
    );
    assert!(!dir.path().join("checked").exists());

    let loaded = config::load_config(dir.path(), None).unwrap();
    trust::approve(loaded.review().unwrap()).unwrap();
    let (trusted, _) = config::load(dir.path(), None).unwrap();
    let mut run = create(&trusted).unwrap();
    assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);
    assert!(dir.path().join("checked").exists());

    // A saved run edited to launch the harness instead no longer matches
    // what was approved.
    let path = run.directory.join("run.json");
    let saved = fs::read_to_string(&path).unwrap();
    assert!(saved.contains("\"execution\": \"checks-only\""), "{saved}");
    fs::write(
        &path,
        saved.replace(
            "\"execution\": \"checks-only\"",
            "\"execution\": \"harness\"",
        ),
    )
    .unwrap();
    let edited = engine::load(dir.path(), &run.id).unwrap();
    assert!(trust::authorize_run(&edited).is_err());
}

#[test]
fn checks_only_receipt_binds_to_the_tested_snapshot() {
    use model_router::report::{Basis, Freshness};
    let dir = common::tempdir();
    if common::repository(dir.path()).is_none() {
        return;
    }
    let workflow = common::workflow(vec![checks_only(vec![python("pass", true)])]);
    let settings = common::settings(&workflow);
    let mut run = common::create(dir.path(), &settings, &workflow);
    assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);
    let attempt = &run.stages[0].attempts[0];
    let tested = attempt.checks_tree.clone().unwrap();
    assert_eq!(attempt.tree_before.as_deref(), Some(tested.as_str()));
    assert_eq!(attempt.tree_after.as_deref(), Some(tested.as_str()));
    let verified = receipt(&run, "verify");
    assert_eq!(verified.basis, Basis::RouterChecks);
    assert_eq!((verified.required_passed, verified.required_total), (1, 1));
    assert_eq!(verified.snapshot.as_deref(), Some(tested.as_str()));
    assert_eq!(verified.freshness, Some(Freshness::Current));
    fs::write(dir.path().join("tracked.txt"), "edited later\n").unwrap();
    match receipt(&run, "verify").freshness {
        Some(Freshness::Stale { changed }) => assert_eq!(changed, ["tracked.txt"]),
        other => panic!("expected stale, got {other:?}"),
    }
}

#[test]
fn checks_only_changes_answer_to_protected_paths() {
    let dir = common::tempdir();
    if common::repository(dir.path()).is_none() {
        return;
    }
    let mut workflow = common::workflow(vec![
        checks_only(vec![python("open('locked.txt', 'w').write('x')", true)]),
        stage("handoff", Role::Custom),
    ]);
    workflow.protected_paths = vec!["locked.txt".into()];
    let settings = common::settings(&workflow);
    let mut run = common::create(dir.path(), &settings, &workflow);
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!(run.status, Status::Blocked);
    let error = run.error.as_deref().unwrap();
    assert!(
        error.contains("protected path") && error.contains("locked.txt"),
        "{error}"
    );
    // The check passed and its results are kept; the stage still stops.
    assert_eq!(
        run.stages[0].attempts[0].checks[0].status,
        TestStatus::Passed
    );
    assert_eq!(count(dir.path(), "handoff"), 0);
}

#[test]
fn checks_only_stage_runs_alone_in_a_dependency_graph() {
    let mut verify = checks_only(vec![python("open('verified', 'w').close()", true)]);
    verify.after = vec!["implementation".into()];
    let mut review = stage("review", Role::Review);
    review.after = vec!["verify".into()];
    let mut research = stage("research", Role::Research);
    research.after = vec!["verify".into()];
    let (dir, _, mut run) = fixture(vec![
        stage("implementation", Role::Implementation),
        verify,
        review,
        research,
    ]);
    assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);
    assert!(run.stages.iter().all(|s| s.status == Status::Complete));
    assert!(dir.path().join("verified").exists());
    assert_eq!(
        ["implementation", "verify", "review", "research"].map(|role| count(dir.path(), role)),
        [1, 0, 1, 1]
    );
    // Both dependents started only after the checks finished.
    let checked = run.stages[1].attempts[0].finished_at.unwrap();
    for dependent in [2, 3] {
        assert!(run.stages[dependent].attempts[0].started_at >= checked);
    }
}
