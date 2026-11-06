mod common;

use common::{count, drive, fixture, stage};
use model_router::{
    adapters::{self, ContextPaths, OutputKind},
    config::{Mode, Role, Settings, Stage},
    engine::{self, Action, Status},
    handoff, process, storage,
};
use std::{
    collections::BTreeMap,
    fs,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[test]
fn no_review_run_passes_context_and_skips_disabled_review() {
    let mut implement = stage("implementation", Role::Implementation);
    implement.extra_args = vec!["--require-context".into(), "research evidence".into()];
    let mut review = stage("review", Role::Review);
    review.enabled = false;
    review.model = "fail".into();
    let (dir, _, mut run) = fixture(vec![stage("research", Role::Research), implement, review]);
    assert_eq!(drive(&mut run), Action::Complete);
    assert_eq!(count(dir.path(), "review"), 0);
    assert_eq!(run.stages[2].status, Status::Skipped);
    let prompt =
        fs::read_to_string(dir.path().join(".model-router/mock/implementation.prompt")).unwrap();
    assert!(prompt.contains("preserve 雪 and $(touch NEVER) literally"));
    assert!(!dir.path().join("NEVER").exists());
    assert_eq!(
        engine::load(dir.path(), &run.id).unwrap().status,
        Status::Complete
    );
}

#[test]
fn all_workflow_presets_execute_in_order_with_optional_reviews_disabled() {
    for preset in model_router::presets::WORKFLOWS {
        let mut stages = preset.build().stages;
        let mut previous = None;
        for stage in &mut stages {
            stage.harness = "mock".into();
            // Native settings belong to the preset's harness, not the mock.
            stage.harness_settings.clear();
            stage.provider = stage.id.clone();
            stage.model = "test-model".into();
            stage.mode = Mode::Unattended;
            if stage.role == Role::Test {
                stage.extra_args = vec!["--test-status".into(), "passed".into()];
            }
            if stage.enabled {
                if let Some(previous) = previous {
                    stage
                        .extra_args
                        .extend(["--require-context".into(), format!("{previous} evidence")]);
                }
                previous = Some(stage.id.clone());
            }
        }
        let (dir, _, mut run) = fixture(stages);
        assert_eq!(
            drive(&mut run),
            Action::Complete,
            "{}: {:?}",
            preset.key,
            run.error
        );
        for (stage, state) in run.workflow.stages.iter().zip(&run.stages) {
            assert_eq!(
                state.status,
                if stage.enabled {
                    Status::Complete
                } else {
                    Status::Skipped
                }
            );
            assert_eq!(count(dir.path(), &stage.id), u32::from(stage.enabled));
            if stage.enabled {
                let prompt = fs::read_to_string(
                    dir.path()
                        .join(format!(".model-router/mock/{}.prompt", stage.id)),
                )
                .unwrap();
                assert!(prompt.contains(&run.task));
                assert!(prompt.contains(&stage.instructions));
            }
        }
        // A review summary stage leaves its brief beside the run for the reviewer.
        let summary = run
            .workflow
            .stages
            .iter()
            .zip(&run.stages)
            .find(|(stage, _)| stage.role == Role::Summary);
        let review = fs::read_to_string(run.directory.join("review.md"));
        match summary {
            Some((_, state)) => assert_eq!(
                review.unwrap().trim(),
                state.handoff.as_ref().unwrap().summary.trim(),
                "{}",
                preset.key
            ),
            None => assert!(review.is_err(), "{}", preset.key),
        }
        assert_eq!(
            model_router::report::build(&run).review.is_some(),
            summary.is_some()
        );
    }
}

#[test]
fn verification_blocks_required_failures_missing_checks_and_unstructured_output() {
    for status in ["failed", "not_run", "unstructured"] {
        let mut verify = stage("verify", Role::Test);
        if status == "unstructured" {
            verify.model = "malformed".into();
        } else {
            verify.extra_args = vec!["--test-status".into(), status.into()];
        }
        let (dir, _, mut run) = fixture(vec![
            stage("implementation", Role::Implementation),
            verify,
            stage("handoff", Role::Custom),
        ]);
        assert_eq!(drive(&mut run), Action::Stopped, "{status}");
        assert_eq!(run.status, Status::Blocked);
        assert_eq!(run.cursor, 1);
        assert_eq!(count(dir.path(), "handoff"), 0);
        assert_eq!(
            run.repair_passes, 0,
            "Verification must not silently enable review repairs"
        );
        assert!(run.error.as_ref().unwrap().contains("Test stage"));
    }
}

#[test]
fn failed_stage_resumes_without_rerunning_completed_research() {
    let mut implement = stage("implementation", Role::Implementation);
    implement.model = "fail".into();
    let (dir, _, mut run) = fixture(vec![stage("research", Role::Research), implement]);
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!(run.status, Status::Failed);
    let first = run.stages[1].attempts[0].directory.clone();
    assert!(
        fs::read_to_string(first.join("stderr.log"))
            .unwrap()
            .contains("Simulated")
    );
    let mut resumed = engine::load(dir.path(), &run.id).unwrap();
    resumed.retry(false, Some("replacement".into())).unwrap();
    assert_eq!(drive(&mut resumed), Action::Complete);
    assert_eq!(count(dir.path(), "research"), 1);
    assert_eq!(resumed.stages[1].attempts.len(), 2);
    assert_eq!(resumed.stages[1].attempts[0].model, "fail");
    assert_eq!(resumed.stages[1].attempts[1].model, "replacement");
}

#[test]
fn review_repairs_invalidate_and_rerun_earlier_passing_reviews() {
    let mut review_b = stage("review-b", Role::Review);
    review_b.extra_args = vec!["--review-fail-until".into(), "1".into()];
    let (dir, _, mut run) = fixture(vec![
        stage("research", Role::Research),
        stage("implementation", Role::Implementation),
        stage("review-a", Role::Review),
        review_b,
    ]);
    assert_eq!(drive(&mut run), Action::Complete);
    assert_eq!(run.repair_passes, 1);
    assert_eq!(count(dir.path(), "research"), 1);
    assert_eq!(count(dir.path(), "implementation"), 2);
    assert_eq!(count(dir.path(), "review-a"), 2);
    assert_eq!(count(dir.path(), "review-b"), 2);
    assert!(
        fs::read_to_string(dir.path().join(".model-router/mock/implementation.prompt"))
            .unwrap()
            .contains("Mock required fix")
    );
}

#[test]
fn repair_limit_and_malformed_verdict_never_pass() {
    let mut review = stage("review", Role::Review);
    review.extra_args = vec!["--review-fail-until".into(), "100".into()];
    let (dir, _, mut run) = fixture(vec![stage("implementation", Role::Implementation), review]);
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!(run.status, Status::Blocked);
    assert_eq!(run.repair_passes, 2);
    assert_eq!(count(dir.path(), "implementation"), 3);
    let (hold, _, mut invalid2) = fixture(vec![Stage {
        model: "malformed".into(),
        ..stage("review", Role::Review)
    }]);
    assert_eq!(drive(&mut invalid2), Action::Stopped);
    assert_eq!(invalid2.status, Status::Blocked);
    assert!(invalid2.error.unwrap().contains("valid structured verdict"));
    drop(hold);
}

#[test]
fn timeout_and_cancellation_preserve_attempts_and_stop_children() {
    let mut slow = stage("slow", Role::Custom);
    slow.extra_args = vec!["--delay".into(), "30".into()];
    slow.timeout_seconds = 1;
    let (_dir, _, mut run) = fixture(vec![slow]);
    let started = Instant::now();
    assert_eq!(drive(&mut run), Action::Stopped);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(run.error.as_ref().unwrap().contains("timed out"));
    assert!(run.stages[0].attempts[0].finished_at.is_some());
    let mut child_stage = stage("parent", Role::Custom);
    child_stage.model = "spawn-child".into();
    let (dir, _, mut run) = fixture(vec![child_stage]);
    let cancel = Arc::new(AtomicBool::new(false));
    let signal = cancel.clone();
    let pid_path = dir.path().join(".model-router/mock/child.pid");
    let check_path = pid_path.clone();
    let signal_thread = thread::spawn(move || {
        let start = Instant::now();
        while !check_path.exists() && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(20));
        }
        signal.store(true, Ordering::Relaxed);
    });
    assert_eq!(
        engine::drive(&mut run, &cancel, |_| {}).unwrap(),
        Action::Stopped
    );
    signal_thread.join().unwrap();
    assert_eq!(run.status, Status::Cancelled);
    let pid: i32 = fs::read_to_string(pid_path).unwrap().parse().unwrap();
    thread::sleep(Duration::from_millis(100));
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        assert!(
            stat.is_empty() || stat.split_whitespace().nth(2) == Some("Z"),
            "descendant still running: {stat}"
        );
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&output.stdout);
        let stat = stat.trim();
        assert!(
            stat.is_empty() || stat.starts_with('Z'),
            "descendant still running: {stat}"
        );
    }
}

#[test]
fn config_roundtrip_preserves_ids_arguments_modes_and_disabled_stages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".model-router.toml");
    let mut settings = Settings::default();
    settings.workflows[0].stages[0].extra_args =
        vec!["--config".into(), "value with spaces; $(literal)".into()];
    settings.save(&path).unwrap();
    let (loaded, _) = model_router::config::load(dir.path(), Some(&path)).unwrap();
    assert_eq!(
        loaded.workflows[0].stages[0].extra_args,
        settings.workflows[0].stages[0].extra_args
    );
    assert_eq!(loaded.workflows[0].stages[1].mode, Mode::Interactive);
    assert!(!loaded.workflows[0].stages[2].enabled);
    assert_eq!(loaded.workflows[0].stages[0].id, "research");
}

#[test]
fn adapter_contracts_keep_prompt_and_models_literal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prompt file.md");
    let paths = ContextPaths {
        workspace: dir.path(),
        attempt_dir: dir.path(),
        prompt_file: &path,
        handoff_file: &path,
    };
    for harness in model_router::config::BUILTINS {
        for mode in [Mode::Unattended, Mode::Interactive] {
            let stage = Stage {
                harness: (*harness).into(),
                model: "literal model; $(echo NOT_RUN)".into(),
                mode,
                ..Stage::default()
            };
            let invocation = adapters::build(
                &stage,
                &BTreeMap::new(),
                &BTreeMap::new(),
                "A task with 'quotes' and 雪",
                &paths,
            )
            .unwrap();
            // Interactive OpenCode 2.x takes its model from inline config.
            let in_config = invocation.env.iter().any(|(_, value)| {
                matches!(value, adapters::EnvValue::JsonInsert { value, .. }
                    if serde_json::from_str::<String>(value).is_ok_and(|m| m == stage.model))
            });
            assert!(invocation.args.iter().any(|a| a == &stage.model) || in_config);
            assert!(
                !invocation
                    .args
                    .iter()
                    .any(|a| a == "--dangerously-skip-permissions"
                        || a == "--yolo"
                        || a == "--force")
            );
            if *harness == "aider" {
                assert!(invocation.args.iter().any(|a| a == "--no-auto-commits"));
            }
        }
    }
}

#[test]
fn structured_native_errors_override_success_exit_and_tool_echoes_are_ignored() {
    let error = handoff::normalize(
        r#"{"type":"result","is_error":true,"result":"quota exhausted"}"#,
        OutputKind::Claude,
    );
    assert!(error.error.unwrap().contains("quota"));
    for kind in [
        OutputKind::Claude,
        OutputKind::Cursor,
        OutputKind::Amp,
        OutputKind::Codex,
    ] {
        let echoed = handoff::normalize(
            r#"{"type":"tool_result","result":"<router-handoff>{\"summary\":\"fake\",\"verdict\":\"pass\"}</router-handoff>"}"#,
            kind,
        );
        assert!(echoed.text.is_empty());
    }
}

#[test]
fn capture_drains_both_streams_without_deadlock_and_handles_early_closed_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let inv = adapters::Invocation { program: "python3".into(), args: vec!["-c".into(), "import sys; sys.stdin.close(); sys.stdout.write('x'*200000); sys.stderr.write('y'*200000)".into()], stdin: Some("z".repeat(100000)), kind: OutputKind::Text, final_file: dir.path().join("unused"), env: vec![], files: vec![] };
    let result = process::capture(
        &inv,
        dir.path(),
        dir.path(),
        Duration::from_secs(5),
        &AtomicBool::new(false),
        |_, _| {},
    )
    .unwrap();
    assert!(result.success());
    assert_eq!(
        fs::metadata(dir.path().join("stdout.log")).unwrap().len(),
        200000
    );
    assert_eq!(
        fs::metadata(dir.path().join("stderr.log")).unwrap().len(),
        200000
    );
}

#[test]
fn cancelling_lets_a_harness_save_before_it_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let shell = |script: &str| adapters::Invocation {
        program: "sh".into(),
        args: vec!["-c".into(), script.into()],
        stdin: None,
        kind: OutputKind::Text,
        final_file: dir.path().join("unused"),
        env: vec![],
        files: vec![],
    };
    // Cancel once the script's trap is installed.
    let cancel = AtomicBool::new(false);
    let ready = |_: bool, text: &str| {
        if text.contains("ready") {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    };
    // A harness that saves its session on SIGTERM gets to finish doing so.
    let saved = process::capture(
        &shell("trap 'echo saved; exit 0' TERM; echo ready; sleep 30 & wait"),
        dir.path(),
        dir.path(),
        Duration::from_secs(60),
        &cancel,
        ready,
    )
    .unwrap();
    assert!(saved.cancelled);
    assert!(
        fs::read_to_string(dir.path().join("stdout.log"))
            .unwrap()
            .contains("saved")
    );
    // One that ignores SIGTERM is killed after the grace period.
    let started = std::time::Instant::now();
    cancel.store(false, std::sync::atomic::Ordering::Relaxed);
    let stubborn = process::capture(
        &shell("trap '' TERM; echo ready; sleep 30"),
        dir.path(),
        dir.path(),
        Duration::from_secs(60),
        &cancel,
        ready,
    )
    .unwrap();
    assert!(stubborn.cancelled && !stubborn.success());
    assert!(started.elapsed() < Duration::from_secs(15));
}

#[test]
fn workspace_lock_and_revision_prevent_concurrent_or_stale_execution() {
    let (dir, _, mut run) = fixture(vec![stage("research", Role::Research)]);
    let lock = storage::lock(&dir.path().join(".model-router/workspace.lock")).unwrap();
    assert!(engine::drive(&mut run, &AtomicBool::new(false), |_| {}).is_err());
    drop(lock);
    let mut stale = run.clone();
    assert_eq!(drive(&mut run), Action::Complete);
    assert!(engine::drive(&mut stale, &AtomicBool::new(false), |_| {}).is_err());
    assert_eq!(count(dir.path(), "research"), 1);
}

#[test]
fn worktree_is_isolated_and_original_checkout_is_not_changed() {
    use std::process::Command;
    let dir = tempfile::tempdir().unwrap();
    let run_git = |args: &[&str]| {
        let result = Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8_lossy(&result.stdout).trim().to_owned()
    };
    // Use the host's human identity, never invent test commit authorship.
    let name = Command::new("git")
        .args(["config", "--get", "user.name"])
        .output()
        .unwrap();
    let email = Command::new("git")
        .args(["config", "--get", "user.email"])
        .output()
        .unwrap();
    if !name.status.success() || !email.status.success() {
        eprintln!("Skipping worktree fixture: configure the human git identity first");
        return;
    }
    let name = String::from_utf8(name.stdout).unwrap().trim().to_owned();
    let email = String::from_utf8(email.stdout).unwrap().trim().to_owned();
    run_git(&["init", "-q"]);
    run_git(&["config", "user.name", &name]);
    run_git(&["config", "user.email", &email]);
    fs::write(dir.path().join("tracked.txt"), "original").unwrap();
    run_git(&["add", "tracked.txt"]);
    run_git(&[
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-qm",
        "Test fixture",
    ]);
    assert_eq!(
        run_git(&["log", "-1", "--format=%an <%ae>"]),
        format!("{name} <{email}>")
    );
    let before = run_git(&["rev-parse", "HEAD"]);
    let path = storage::create_worktree(dir.path(), "fixture-run", true).unwrap();
    fs::write(path.join("tracked.txt"), "changed in worktree").unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
        "original"
    );
    assert_eq!(run_git(&["rev-parse", "HEAD"]), before);
    assert!(run_git(&["status", "--porcelain"]).is_empty());
    assert!(path.exists());
}

#[test]
fn terminating_cli_stops_its_stage_and_persists_cancellation() {
    use std::process::{Command, Stdio};
    let mut slow = stage("slow", Role::Custom);
    slow.extra_args = vec!["--delay".into(), "30".into()];
    let (dir, settings, initial) = fixture(vec![slow]);
    let config = dir.path().join("config.toml");
    settings.save(&config).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("-C")
        .arg(dir.path())
        .arg("--config")
        .arg(config)
        .args(["run", "--task", "Stop this run on SIGTERM"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !dir.path().join(".model-router/mock/slow.count").exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("CLI did not handle SIGTERM");
        }
        thread::sleep(Duration::from_millis(20));
    }
    let stopped = engine::history(dir.path())
        .unwrap()
        .into_iter()
        .find(|r| r.id != initial.id)
        .unwrap();
    assert_eq!(stopped.status, Status::Cancelled);
    assert!(stopped.stages[0].attempts[0].finished_at.is_some());
}

#[test]
fn capture_applies_environment_variables() {
    let dir = tempfile::tempdir().unwrap();
    let inv = adapters::Invocation {
        program: "python3".into(),
        args: vec![
            "-c".into(),
            "import os; print(os.environ['ROUTED'], open('aider.commands').read().strip(), 'HOME' in os.environ)".into(),
        ],
        stdin: None,
        kind: OutputKind::Text,
        final_file: dir.path().join("unused"),
        env: vec![
            ("ROUTED".into(), adapters::EnvValue::Literal("yes".into())),
            // Removes inherited HOME* variables, but never one the invocation sets.
            ("HOME".into(), adapters::EnvValue::UnsetPrefix),
            ("ROUTED".into(), adapters::EnvValue::UnsetPrefix),
        ],
        // Written at launch, not when the invocation is built.
        files: vec![(dir.path().join("aider.commands"), "/code go\n".into())],
    };
    let result = process::capture(
        &inv,
        dir.path(),
        dir.path(),
        Duration::from_secs(5),
        &AtomicBool::new(false),
        |_, _| {},
    )
    .unwrap();
    assert!(result.success());
    assert_eq!(
        fs::read_to_string(dir.path().join("stdout.log")).unwrap(),
        "yes /code go False\n"
    );
}

#[cfg(target_os = "linux")]
fn running(pid: i32) -> bool {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    !stat.is_empty() && stat.split_whitespace().nth(2) != Some("Z")
}
#[cfg(all(unix, not(target_os = "linux")))]
fn running(pid: i32) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&output.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

#[test]
fn background_children_end_with_a_harness_that_exits_normally() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("server.pid");
    let inv = adapters::Invocation {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            "sleep 30 & echo $! > server.pid; echo done".into(),
        ],
        stdin: None,
        kind: OutputKind::Text,
        final_file: dir.path().join("unused"),
        env: vec![],
        files: vec![],
    };
    let mut group = None;
    let started = Instant::now();
    let result = process::capture_with(
        &inv,
        dir.path(),
        dir.path(),
        Duration::from_secs(10),
        &AtomicBool::new(false),
        None,
        |pgid| group = Some(pgid),
        |_, _| {},
    )
    .unwrap();
    assert!(result.success());
    assert!(started.elapsed() < Duration::from_secs(5));
    let pid: i32 = fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    thread::sleep(Duration::from_millis(100));
    assert!(!running(pid), "background child outlived its harness");
    assert!(!process::group_alive(group.unwrap()));
}

#[test]
fn drive_stops_a_harness_group_left_by_a_dead_router() {
    let (dir, _, mut run) = fixture(vec![stage("implementation", Role::Implementation)]);
    assert_eq!(drive(&mut run), Action::Complete);
    // A crash mid-stage leaves the attempt running with its group recorded.
    let mut leader = {
        use std::os::unix::process::CommandExt;
        std::process::Command::new("sh")
            .args(["-c", "sleep 30 & echo $! > orphan.pid"])
            .current_dir(dir.path())
            .process_group(0)
            .spawn()
            .unwrap()
    };
    let pgid = leader.id();
    leader.wait().unwrap();
    let pid: i32 = fs::read_to_string(dir.path().join("orphan.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(process::group_alive(pgid));
    run.cursor = 0;
    run.status = Status::Running;
    run.stages[0].status = Status::Running;
    let attempt = run.stages[0].attempts.last_mut().unwrap();
    attempt.status = Status::Running;
    attempt.process_group = Some(pgid);
    run.persist().unwrap();
    assert_eq!(drive(&mut run), Action::Stopped);
    assert!(run.error.as_deref().unwrap().contains("interrupted"));
    assert!(run.stages[0].attempts[0].process_group.is_none());
    thread::sleep(Duration::from_millis(100));
    assert!(!running(pid), "orphaned harness child still running");
}

#[test]
fn repair_request_reaches_only_the_repair_and_its_review() {
    let mut review = stage("review", Role::Review);
    review.extra_args = vec!["--review-fail-until".into(), "1".into()];
    let (dir, _, mut run) = fixture(vec![
        stage("implementation", Role::Implementation),
        review,
        stage("docs", Role::Custom),
    ]);
    assert_eq!(drive(&mut run), Action::Complete);
    let prompt = |role: &str| {
        fs::read_to_string(dir.path().join(format!(".model-router/mock/{role}.prompt"))).unwrap()
    };
    assert!(prompt("implementation").contains("REPAIR REQUEST"));
    assert!(prompt("review").contains("REPAIR REQUEST"));
    assert!(
        !prompt("docs").contains("REPAIR REQUEST"),
        "a stage after the passing review saw a stale repair request"
    );
    assert!(run.repair_feedback.is_none() && run.repair_review.is_none());
}

#[test]
fn errors_outside_an_attempt_leave_finished_attempts_alone() {
    // A launch that fails before its attempt exists.
    let mut implement = stage("implementation", Role::Implementation);
    implement.model = "fail".into();
    let (_dir, _, mut run) = fixture(vec![implement]);
    assert_eq!(drive(&mut run), Action::Stopped);
    let first = run.stages[0].attempts[0].clone();
    run.retry(false, Some("test-model".into())).unwrap();
    let taken = first.handoff_file.with_file_name("implementation-002.json");
    fs::write(&taken, "{}").unwrap();
    assert_eq!(drive(&mut run), Action::Stopped);
    assert!(run.error.as_deref().unwrap().contains("already exists"));
    assert_eq!(run.stages[0].attempts.len(), 1);
    let kept = &run.stages[0].attempts[0];
    assert_eq!((kept.status, &kept.error), (first.status, &first.error));

    // A cancel after a repair moved the cursor back to a finished stage.
    let mut review = stage("review", Role::Review);
    review.extra_args = vec!["--review-fail-until".into(), "1".into()];
    let (_dir, _, mut run) = fixture(vec![stage("implementation", Role::Implementation), review]);
    let cancel = AtomicBool::new(false);
    let action = engine::drive(&mut run, &cancel, |event| {
        if let engine::Event::State(state) = event {
            if state.repair_passes == 1 {
                cancel.store(true, Ordering::Relaxed);
            }
        }
    })
    .unwrap();
    assert_eq!(
        (action, run.status, run.cursor),
        (Action::Stopped, Status::Cancelled, 0)
    );
    assert_eq!(run.stages[0].attempts.len(), 1);
    assert_eq!(run.stages[0].attempts[0].status, Status::Complete);
    assert!(run.stages[0].attempts[0].error.is_none());
}

#[test]
fn repair_limit_can_be_extended_explicitly() {
    let mut review = stage("review", Role::Review);
    review.extra_args = vec!["--review-fail-until".into(), "3".into()];
    let (dir, _, mut run) = fixture(vec![stage("implementation", Role::Implementation), review]);
    assert!(run.allow_repair().is_err(), "nothing is blocked yet");
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!((run.status, run.repair_passes), (Status::Blocked, 2));
    let error = run.error.clone().unwrap();
    assert!(
        error.contains(&format!("resume {} --allow-repair", run.id)),
        "{error}"
    );
    let mut resumed = engine::load(dir.path(), &run.id).unwrap();
    resumed.allow_repair().unwrap();
    assert_eq!((resumed.cursor, resumed.status), (0, Status::Pending));
    assert_eq!(
        resumed.stages[1].attempts.last().unwrap().status,
        Status::Complete
    );
    assert_eq!(drive(&mut resumed), Action::Complete);
    assert_eq!(resumed.repair_passes, 3);
    assert_eq!(count(dir.path(), "implementation"), 4);
    assert!(resumed.allow_repair().is_err());
}
