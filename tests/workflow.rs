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
            for candidate in stage.fan_out.iter_mut().flat_map(|f| &mut f.candidates) {
                candidate.harness = "mock".into();
                candidate.provider = stage.id.clone();
                candidate.model = "test-model".into();
            }
            if stage.id == "judge" {
                stage.extra_args = vec!["--select".into(), "c1".into()];
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
        // Fan-out candidates work in Git worktrees.
        let fans = stages.iter().any(|s| s.fan_out.is_some());
        let (dir, mut run) = if fans {
            let dir = common::tempdir();
            if common::repository(dir.path()).is_none() {
                continue;
            }
            let workflow = common::workflow(stages);
            let run = common::create(dir.path(), &common::settings(&workflow), &workflow);
            (dir, run)
        } else {
            let (dir, _, run) = fixture(stages);
            (dir, run)
        };
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
            if let Some(fan) = &state.fan_out {
                // Each candidate ran once, in its own worktree, with the stage prompt.
                assert_eq!(count(dir.path(), &stage.id), 0);
                for candidate in &fan.candidates {
                    let prompt =
                        fs::read_to_string(candidate.attempts[0].directory.join("prompt.md"))
                            .unwrap();
                    assert!(prompt.contains(&run.task) && prompt.contains(&stage.instructions));
                }
                assert_eq!(fan.selection.as_ref().unwrap().candidate, "c1");
                continue;
            }
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
        mcp: None,
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
    let inv = adapters::Invocation { program: "python3".into(), args: vec!["-c".into(), "import sys; sys.stdin.close(); sys.stdout.write('x'*200000); sys.stderr.write('y'*200000)".into()], stdin: Some("z".repeat(100000)), kind: OutputKind::Text, final_file: dir.path().join("unused"), env: vec![], files: vec![], assign: None };
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
        assign: None,
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
        assign: None,
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
        assign: None,
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
        |pgid| {
            group = Some(pgid);
            Ok(())
        },
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
    assert!(!process::group_alive(group.unwrap().pgid));
}

/// Not a test: what `orphan` leaves running. This test binary, unlike system
/// binaries, shows its environment to recovery's marker check on macOS.
#[test]
#[ignore]
fn linger() {
    if std::env::var_os("MODEL_ROUTER_TEST_LINGER").is_some() {
        thread::sleep(Duration::from_secs(30));
    }
}

/// A crashed run whose recorded harness group is still alive: its leader
/// exited and left a background child. Returns the child's pid.
fn orphan(dir: &std::path::Path, run: &mut engine::Run, identity: process::Identity) -> i32 {
    assert_eq!(drive(run), Action::Complete);
    // A crash mid-stage leaves the attempt running with its group recorded.
    let mut leader = {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg(r#""$0" --exact linger --ignored >/dev/null 2>&1 & echo $! > orphan.pid"#)
            .arg(std::env::current_exe().unwrap())
            .env("MODEL_ROUTER_TEST_LINGER", "1")
            .current_dir(dir)
            .process_group(0);
        if let Some(marker) = &identity.marker {
            command.env(process::group::MARKER_ENV, marker);
        }
        command.spawn().unwrap()
    };
    let pgid = leader.id();
    leader.wait().unwrap();
    let pid: i32 = fs::read_to_string(dir.join("orphan.pid"))
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
    attempt.set_group(Some(process::Group { pgid, identity }));
    run.persist().unwrap();
    pid
}

fn drive_logged(run: &mut engine::Run) -> (Action, String) {
    let mut log = String::new();
    let action = engine::drive(run, &AtomicBool::new(false), |event| {
        if let engine::Event::Output(text) = event {
            log.push_str(&text);
        }
    })
    .unwrap();
    (action, log)
}

#[test]
fn drive_stops_a_harness_group_left_by_a_dead_router() {
    let (dir, _, mut run) = fixture(vec![stage("implementation", Role::Implementation)]);
    let identity = process::Identity {
        started: Some("the exited leader".into()),
        marker: Some(process::group::new_marker()),
    };
    let pid = orphan(dir.path(), &mut run, identity);
    let (action, log) = drive_logged(&mut run);
    assert_eq!(action, Action::Stopped);
    assert!(log.contains("Stopping process group"), "{log}");
    assert!(run.error.as_deref().unwrap().contains("interrupted"));
    assert!(run.stages[0].attempts[0].process_group.is_none());
    assert!(run.stages[0].attempts[0].process_identity.is_empty());
    let deadline = Instant::now() + Duration::from_secs(5);
    while running(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!running(pid), "orphaned harness child still running");
}

#[test]
fn drive_leaves_alone_a_group_it_cannot_prove_is_the_recorded_one() {
    // Saved by an earlier version (ID only), or recorded with a marker no
    // process in the group carries: the ID may name an unrelated group.
    for identity in [
        process::Identity::default(),
        process::Identity {
            started: Some("the exited leader".into()),
            marker: Some(process::group::new_marker()),
        },
    ] {
        let (dir, _, mut run) = fixture(vec![stage("implementation", Role::Implementation)]);
        let unmarked = process::Identity {
            marker: None,
            ..identity.clone()
        };
        let pid = orphan(dir.path(), &mut run, unmarked);
        let attempt = run.stages[0].attempts.last_mut().unwrap();
        let pgid = attempt.process_group.unwrap();
        attempt.process_identity = identity.clone();
        run.persist().unwrap();
        // A run.json without the identity (an earlier version's) still loads.
        let mut run = engine::load(dir.path(), &run.id).unwrap();
        assert_eq!(run.stages[0].attempts[0].process_identity, identity);
        let (action, log) = drive_logged(&mut run);
        assert_eq!(action, Action::Stopped);
        assert!(
            log.contains(&format!("Left process group {pgid} running")),
            "{log}"
        );
        assert!(running(pid), "an unverified group was signalled");
        process::end_group(pgid, Duration::from_millis(100), || {});
    }
}

/// A crashed run whose harness group is alive but carries no marker, so
/// recovery cannot prove it is the run's. Returns its pgid and a member.
fn unproven_orphan(dir: &std::path::Path, run: &mut engine::Run) -> (u32, i32) {
    let identity = process::Identity {
        started: Some("the exited leader".into()),
        marker: Some(process::group::new_marker()),
    };
    let unmarked = process::Identity {
        marker: None,
        ..identity.clone()
    };
    let pid = orphan(dir, run, unmarked);
    let attempt = run.stages[0].attempts.last_mut().unwrap();
    let pgid = attempt.process_group.unwrap();
    attempt.process_identity = identity;
    run.persist().unwrap();
    (pgid, pid)
}

/// Ends the group however the test ends.
struct EndGroup(u32);
impl Drop for EndGroup {
    fn drop(&mut self) {
        process::end_group(self.0, Duration::from_millis(100), || {});
    }
}

/// A run the CLI can replay: its saved config, trusted.
fn trusted_fixture() -> (tempfile::TempDir, engine::Run) {
    let (dir, settings, run) = fixture(vec![stage("implementation", Role::Implementation)]);
    settings
        .save(&dir.path().join(".model-router.toml"))
        .unwrap();
    let (ok, stdout, stderr) = router(dir.path(), &["trust", "--yes"]);
    assert!(ok, "{stdout}{stderr}");
    (dir, run)
}

fn router(dir: &std::path::Path, args: &[&str]) -> (bool, String, String) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("MODEL_ROUTER_CONFIG_DIR", dir.join("config-home"))
        .output()
        .unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn an_unproven_group_blocks_retry_until_released() {
    let (dir, mut run) = trusted_fixture();
    let (pgid, pid) = unproven_orphan(dir.path(), &mut run);
    let _end = EndGroup(pgid);
    let id = run.id.clone();
    // Recovery leaves the group running, says so, and launches nothing.
    for _ in 0..2 {
        let (ok, stdout, stderr) = router(dir.path(), &["resume", &id, "--retry"]);
        assert!(!ok, "{stdout}");
        assert!(
            stderr.contains(&format!("process group {pgid}: "))
                && stderr.contains(&format!("kill -TERM -- -{pgid}"))
                && stderr.contains(&format!("model-router resume {id} --release-groups")),
            "{stderr}"
        );
        assert!(running(pid), "an unproven group was signalled");
        let saved = engine::load(dir.path(), &id).unwrap();
        assert_eq!(saved.stages[0].attempts.len(), 1, "a harness launched");
        assert_eq!(
            saved
                .retained_groups()
                .iter()
                .map(|g| g.pgid)
                .collect::<Vec<_>>(),
            [pgid]
        );
    }
    // Released after the user checked it: forgotten, not signalled.
    let (ok, stdout, stderr) = router(dir.path(), &["resume", &id, "--release-groups", "--retry"]);
    assert!(ok, "{stdout}{stderr}");
    assert!(
        stdout.contains(&format!(
            "Released process group {pgid} without signalling it"
        )),
        "{stdout}"
    );
    assert!(running(pid), "a released group was signalled");
    let saved = engine::load(dir.path(), &id).unwrap();
    assert_eq!(saved.status, Status::Complete);
    assert_eq!(saved.stages[0].attempts.len(), 2);
    assert!(saved.retained_groups().is_empty());
}

#[test]
fn recovery_reports_a_group_it_leaves_running() {
    let (dir, mut run) = trusted_fixture();
    let (pgid, _) = unproven_orphan(dir.path(), &mut run);
    let _end = EndGroup(pgid);
    let (ok, stdout, _) = router(dir.path(), &["resume", &run.id, "--retry"]);
    assert!(!ok);
    assert!(
        stdout.contains(&format!("Left process group {pgid} running")),
        "{stdout}"
    );
}

#[test]
fn a_retained_group_that_has_ended_is_dropped_silently() {
    let (dir, mut run) = trusted_fixture();
    let (pgid, _) = unproven_orphan(dir.path(), &mut run);
    let id = run.id.clone();
    let (ok, ..) = router(dir.path(), &["resume", &id, "--retry"]);
    assert!(!ok);
    // The user stops it, as the refusal says.
    process::end_group(pgid, Duration::from_millis(100), || {});
    let deadline = Instant::now() + Duration::from_secs(5);
    while process::group_alive(pgid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!process::group_alive(pgid));
    let (ok, stdout, stderr) = router(dir.path(), &["resume", &id, "--retry"]);
    assert!(ok, "{stdout}{stderr}");
    assert!(!stdout.contains("process group"), "{stdout}");
    let saved = engine::load(dir.path(), &id).unwrap();
    assert_eq!(saved.status, Status::Complete);
    assert!(saved.retained_groups().is_empty());
}

#[test]
fn recovery_keeps_a_group_whose_unmarked_member_outlives_sigterm() {
    let (dir, _, mut run) = fixture(vec![stage("implementation", Role::Implementation)]);
    assert_eq!(drive(&mut run), Action::Complete);
    // The recorded leader exits on SIGTERM; a member ignores it and carries
    // no marker, so nothing proves the group is the run's once the leader
    // is gone, and SIGKILL is skipped.
    let marker = process::group::new_marker();
    let file = dir.path().join("member.pid");
    let mut leader = {
        use std::os::unix::process::CommandExt;
        std::process::Command::new("sh")
            .arg("-c")
            .arg(
                r#"trap 'exit 0' TERM
sh -c 'trap "" TERM; echo $$ > "$1"; exec env -u MODEL_ROUTER_GROUP sleep 30' member "$0" &
wait"#,
            )
            .arg(&file)
            .env(process::group::MARKER_ENV, &marker)
            .process_group(0)
            .spawn()
            .unwrap()
    };
    let pgid = leader.id();
    let group = process::Group::launched(pgid, Some(marker));
    let _end = EndGroup(pgid);
    let deadline = Instant::now() + Duration::from_secs(5);
    let pid: i32 = loop {
        if let Some(pid) = fs::read_to_string(&file)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "member never started");
        thread::sleep(Duration::from_millis(10));
    };
    // Reaped as soon as it exits, as init reaps an orphaned leader.
    let reaper = thread::spawn(move || leader.wait());
    run.cursor = 0;
    run.status = Status::Running;
    run.stages[0].status = Status::Running;
    let attempt = run.stages[0].attempts.last_mut().unwrap();
    attempt.status = Status::Running;
    attempt.set_group(Some(group));
    run.persist().unwrap();

    let mut log = String::new();
    let error = engine::authorize_replay(&mut run, None, |event| {
        if let engine::Event::Output(text) = event {
            log.push_str(&text);
        }
    })
    .unwrap_err();
    reaper.join().unwrap().unwrap();
    assert!(
        log.contains(&format!("Stopping process group {pgid}")),
        "{log}"
    );
    assert!(
        log.contains(&format!(
            "Process group {pgid} is still running after SIGTERM"
        )),
        "{log}"
    );
    assert!(
        format!("{error:#}").contains(&format!("process group {pgid}: ")),
        "{error:#}"
    );
    assert!(running(pid), "the member was killed");
    let saved = engine::load(dir.path(), &run.id).unwrap();
    assert_eq!(
        saved
            .retained_groups()
            .iter()
            .map(|g| g.pgid)
            .collect::<Vec<_>>(),
        [pgid]
    );
    assert!(saved.stages[0].attempts[0].process_group.is_none());
    // Once it has ended, recovery forgets it and the run can go on.
    process::end_group(pgid, Duration::from_millis(100), || {});
    let deadline = Instant::now() + Duration::from_secs(5);
    while process::group_alive(pgid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let mut run = engine::load(dir.path(), &run.id).unwrap();
    let (action, log) = drive_logged(&mut run);
    assert_eq!(action, Action::Stopped);
    assert!(!log.contains("process group"), "{log}");
    assert!(run.retained_groups().is_empty());
}

#[test]
fn a_new_run_or_rerun_in_the_checkout_waits_for_another_runs_unproven_group() {
    let (dir, mut run) = trusted_fixture();
    let (pgid, _) = unproven_orphan(dir.path(), &mut run);
    let _end = EndGroup(pgid);
    let id = run.id.clone();
    let (ok, ..) = router(dir.path(), &["resume", &id, "--retry"]);
    assert!(!ok);
    let runs = || {
        fs::read_dir(dir.path().join(".model-router/runs"))
            .unwrap()
            .count()
    };
    let before = runs();
    for args in [
        vec!["rerun", id.as_str()],
        vec!["run", "--task", "Another task"],
    ] {
        let (ok, stdout, stderr) = router(dir.path(), &args);
        assert!(!ok, "{args:?}: {stdout}");
        assert!(
            stderr.contains(&format!("process group {pgid}: "))
                && stderr.contains(&format!("model-router resume {id} --release-groups")),
            "{args:?}: {stderr}"
        );
    }
    assert_eq!(runs(), before, "a run was created");
}

#[cfg(target_os = "linux")]
#[test]
fn retry_after_router_crash_stops_check_descendants_before_rerunning() {
    check_crash_recovery(false, false);
}

#[cfg(target_os = "linux")]
#[test]
fn full_fan_out_retry_after_router_crash_stops_candidate_check_descendants() {
    check_crash_recovery(true, false);
}

#[cfg(target_os = "linux")]
#[test]
fn revoked_replay_still_stops_crashed_check_descendants_without_relaunching() {
    check_crash_recovery(false, true);
}

#[cfg(target_os = "linux")]
fn check_crash_recovery(fan_out: bool, revoked: bool) {
    use std::process::{Child, Command, Stdio};
    let dir = tempfile::tempdir().unwrap();
    if fan_out && common::repository(dir.path()).is_none() {
        return;
    }
    let marker = dir.path().join("check-child.json");
    let mut implementation = stage("implementation", Role::Implementation);
    if fan_out {
        implementation.fan_out = Some(model_router::config::FanOut {
            candidates: ["test-model", "fail"]
                .into_iter()
                .map(|model| model_router::config::Candidate {
                    harness: "mock".into(),
                    provider: "implementation".into(),
                    model: model.into(),
                    ..Default::default()
                })
                .collect(),
            judge: "auto".into(),
        });
    }
    implementation.checks.push(model_router::config::Check {
        command: vec!["python3".into(), "-c".into(), r#"
import json, os, pathlib, subprocess, sys, time
marker = pathlib.Path(sys.argv[1])
if marker.exists():
    pid = json.loads(marker.read_text())['pid']
    stat = pathlib.Path('/proc/%s/stat' % pid)
    assert not stat.exists() or stat.read_text().split()[2] == 'Z', 'old check child is still running'
else:
    child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
    marker.write_text(json.dumps({'pid': child.pid, 'pgid': os.getpgrp()}))
    time.sleep(60)
"#.into(), marker.display().to_string()],
        ..model_router::config::Check::default()
    });
    let workflow = common::workflow(vec![implementation]);
    common::settings(&workflow)
        .save(&dir.path().join(".model-router.toml"))
        .unwrap();
    let router = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_model-router"));
        command
            .arg("-C")
            .arg(dir.path())
            .env("MODEL_ROUTER_CONFIG_DIR", dir.path().join("config-home"));
        command
    };
    assert!(
        router()
            .args(["trust", "--yes"])
            .output()
            .unwrap()
            .status
            .success()
    );
    struct Cleanup {
        router: Child,
        group: Option<u32>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.router.kill();
            let _ = self.router.wait();
            if let Some(group) = self.group {
                process::end_group(group, Duration::from_millis(100), || {});
            }
        }
    }
    let mut cleanup = Cleanup {
        router: router()
            .args(["run", "--task", "Check crash recovery"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        group: None,
    };
    let started = Instant::now();
    let child: serde_json::Value = loop {
        if let Ok(bytes) = fs::read(&marker) {
            if let Ok(child) = serde_json::from_slice(&bytes) {
                break child;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "check did not start"
        );
        thread::sleep(Duration::from_millis(20));
    };
    let pgid = child["pgid"].as_u64().unwrap() as u32;
    let pid = child["pid"].as_i64().unwrap() as i32;
    cleanup.group = Some(pgid);
    let directory = fs::read_dir(dir.path().join(".model-router/runs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let id = directory.file_name().unwrap().to_str().unwrap();
    // The check wrote its marker before the router saved its group; wait
    // for the save rather than racing it.
    let recorded = |saved: &engine::Run| {
        let attempt = if fan_out {
            &saved.stages[0].fan_out.as_ref().unwrap().candidates[0].attempts[0]
        } else {
            &saved.stages[0].attempts[0]
        };
        attempt.process_group
    };
    let mut group = None;
    while started.elapsed() < Duration::from_secs(10) {
        group = engine::load(dir.path(), id)
            .ok()
            .and_then(|saved| recorded(&saved));
        if group == Some(pgid) {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(group, Some(pgid));
    let saved = engine::load(dir.path(), id).unwrap();
    cleanup.router.kill().unwrap();
    cleanup.router.wait().unwrap();
    assert!(
        running(pid),
        "probe descendant must survive the router crash"
    );
    if fan_out {
        // Full retries currently reuse candidate worktree names. Remove only
        // these disposable worktrees to isolate crash recovery from that bug;
        // the surviving process group must still be stopped by the router.
        for candidate in &saved.stages[0].fan_out.as_ref().unwrap().candidates {
            for attempt in &candidate.attempts {
                if let Some(worktree) = &attempt.worktree {
                    let removed = Command::new("git")
                        .current_dir(dir.path())
                        .args([
                            "-c",
                            "core.hooksPath=/dev/null",
                            "worktree",
                            "remove",
                            "--force",
                        ])
                        .arg(worktree)
                        .output()
                        .unwrap();
                    assert!(
                        removed.status.success(),
                        "{}",
                        String::from_utf8_lossy(&removed.stderr)
                    );
                }
            }
        }
        assert!(running(pid));
    }
    if revoked {
        assert!(
            router()
                .args(["trust", "--revoke"])
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    let retry = router().args(["resume", id, "--retry"]).output().unwrap();
    assert!(!running(pid), "old check child survived recovery");
    if revoked {
        assert!(!retry.status.success());
        assert!(
            String::from_utf8_lossy(&retry.stderr)
                .contains("Saved run executable settings are untrusted")
        );
        let saved = engine::load(dir.path(), id).unwrap();
        assert_eq!(
            saved.stages[0].attempts.len(),
            1,
            "revocation must prevent a new launch"
        );
        assert_eq!(saved.stages[0].attempts[0].process_group, None);
        return;
    }
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    let saved = engine::load(dir.path(), id).unwrap();
    assert_eq!(saved.status, Status::Complete);
    assert!(
        saved.stages[0]
            .attempts
            .iter()
            .all(|a| a.process_group.is_none())
    );
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

#[test]
fn handoff_inspector_matches_the_real_prompt_and_reports_losses() {
    use model_router::engine::Kept;
    let (_dir, _, mut run) = common::fixture(vec![
        common::stage("research", Role::Research),
        common::stage("verify", Role::Test),
    ]);
    // Verify reports no evidence and blocks after its first attempt.
    assert_eq!(
        common::drive(&mut run),
        model_router::engine::Action::Stopped
    );
    let long = "decision text ".repeat(100);
    let handoff = run.stages[0].handoff.as_mut().unwrap();
    handoff.decisions = vec![long.clone(); 60];
    handoff.unresolved = (0..80)
        .map(|i| format!("open question {i} {long}"))
        .collect();
    handoff.files_changed = (0..300).map(|i| format!("src/f{i}.rs")).collect();
    run.persist().unwrap();
    run.retry(false, None).unwrap();

    let inspection = engine::inspect_handoffs(&run, None).unwrap();
    assert_eq!(inspection.stage, "verify");
    let view = &inspection.handoffs[0];
    assert!(
        view.sent && view.bytes <= engine::PRIOR_EACH + 200,
        "{}",
        view.bytes
    );
    let field = |name: &str| view.fields.iter().find(|f| f.field == name).unwrap();
    // Whole fields are not listed: the short summary arrived intact.
    assert!(view.fields.iter().all(|f| f.field != "summary"));
    for name in ["decisions", "unresolved"] {
        let f = field(name);
        assert_ne!(f.kept, Kept::Whole, "{name}");
        if let Some((total, shown)) = f.items {
            assert!(shown < total, "{name}: {shown} of {total}");
        }
    }
    assert!(
        view.warnings
            .iter()
            .any(|w| w.starts_with("decisions lost"))
    );
    assert!(
        view.warnings
            .iter()
            .any(|w| w.starts_with("unresolved items lost"))
    );
    let text = inspection.text();
    assert!(text.contains("warning: decisions lost"), "{text}");

    // The next attempt's prompt is exactly what the inspector showed.
    common::drive(&mut run);
    let attempt = run.stages[1].attempts.last().unwrap();
    assert_eq!(attempt.number, 2);
    let sent = fs::read_to_string(attempt.directory.join("prompt.md")).unwrap();
    assert_eq!(sent, inspection.prompt);
}

#[test]
fn context_files_and_tunable_handoff_budgets_flow_through_cli_preview_run_and_handoffs() {
    let dir = common::tempdir();
    let outside = common::tempdir();
    let config_home = common::tempdir();
    let mock = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/mock_harness.py");

    fs::write(
        outside.path().join("secret.txt"),
        "OUTSIDE_SECRET_DO_NOT_READ",
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("docs")).unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join("AGENTS.md"), "WORKFLOW_CONTEXT_RULE\n").unwrap();
    fs::write(dir.path().join("docs/guide.md"), "DOCS_GUIDE_CONTENT\n").unwrap();
    fs::write(dir.path().join("stage-note.txt"), "STAGE_ONLY_NOTE\n").unwrap();
    fs::write(dir.path().join(".git/config"), "GIT_CONFIG_SECRET\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        outside.path().join("secret.txt"),
        dir.path().join("docs/linked.md"),
    )
    .unwrap();

    let cfg = format!(
        r#"version = 2

[adapters.mock]
command = "python3"
args = ["{mock}", "--role", "{{provider}}", "--prompt-file", "{{prompt_file}}", "--handoff-file", "{{handoff_file}}"]

[[workflows]]
name = "Context workflow"
max_repair_passes = 0
handoff_budget_bytes = 24000
handoff_each_bytes = 8000
context_files = ["AGENTS.md", "docs/*.md"]

[[workflows.stages]]
id = "research"
name = "Research"
role = "research"
harness = "mock"
provider = "research"
model = "offline-test"

[[workflows.stages]]
id = "implement"
name = "Implement"
role = "implementation"
harness = "mock"
provider = "implementation"
model = "offline-test"
context_files = ["stage-note.txt"]
"#
    );
    let config_path = dir.path().join("workflow.toml");
    fs::write(&config_path, cfg).unwrap();

    let run_cli = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_model-router"))
            .arg("-C")
            .arg(dir.path())
            .arg("--config")
            .arg(&config_path)
            .args(args)
            .env("MODEL_ROUTER_CONFIG_DIR", config_home.path())
            .output()
            .unwrap()
    };

    // 1. Validate produces no unknown-field warnings.
    let val = run_cli(&["validate"]);
    assert!(
        val.status.success(),
        "{}{}",
        String::from_utf8_lossy(&val.stdout),
        String::from_utf8_lossy(&val.stderr)
    );
    let val_out = String::from_utf8_lossy(&val.stdout);
    assert!(!val_out.contains("Warning:"), "{val_out}");

    // 2. Preview shows tuned handoff budget and resolved context files per stage.
    let prev = run_cli(&["preview", "--task", "Check context"]);
    assert!(prev.status.success());
    let prev_out = String::from_utf8_lossy(&prev.stdout);
    assert!(
        prev_out.contains("Handoff budget: 24000 bytes total, 8000 each"),
        "{prev_out}"
    );
    assert!(prev_out.contains("AGENTS.md"), "{prev_out}");
    assert!(prev_out.contains("docs/guide.md"), "{prev_out}");
    assert!(prev_out.contains("stage-note.txt"), "{prev_out}");
    assert!(
        !prev_out.contains("OUTSIDE_SECRET_DO_NOT_READ"),
        "{prev_out}"
    );

    // 3. Running injects context files into actual stage prompts.
    let run_out = run_cli(&["run", "--task", "Check context"]);
    assert!(
        run_out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&run_out.stdout),
        String::from_utf8_lossy(&run_out.stderr)
    );
    let run_dir = fs::read_dir(dir.path().join(".model-router/runs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let run_id = run_dir.file_name().unwrap().to_str().unwrap().to_owned();
    let research_prompt =
        fs::read_to_string(run_dir.join("01-research/attempt-001/prompt.md")).unwrap();
    let implement_prompt =
        fs::read_to_string(run_dir.join("02-implement/attempt-001/prompt.md")).unwrap();
    assert!(research_prompt.contains("WORKFLOW_CONTEXT_RULE"));
    assert!(research_prompt.contains("DOCS_GUIDE_CONTENT"));
    assert!(!research_prompt.contains("STAGE_ONLY_NOTE"));
    assert!(implement_prompt.contains("WORKFLOW_CONTEXT_RULE"));
    assert!(implement_prompt.contains("DOCS_GUIDE_CONTENT"));
    assert!(implement_prompt.contains("STAGE_ONLY_NOTE"));
    for secret in ["OUTSIDE_SECRET_DO_NOT_READ", "GIT_CONFIG_SECRET"] {
        assert!(!research_prompt.contains(secret));
        assert!(!implement_prompt.contains(secret));
    }

    // 4. `handoffs --stage implement --prompt` and `--json` show context files and tuned budget.
    let ho = run_cli(&["handoffs", &run_id, "--stage", "implement", "--prompt"]);
    assert!(ho.status.success());
    let ho_out = String::from_utf8_lossy(&ho.stdout);
    assert!(ho_out.contains("Context files:"), "{ho_out}");
    assert!(ho_out.contains("stage-note.txt"), "{ho_out}");
    assert!(
        ho_out.contains("Budget: 24000 bytes of prior handoffs, at most 8000 each"),
        "{ho_out}"
    );
    assert!(ho_out.contains("PROJECT CONTEXT FILES"), "{ho_out}");
    assert!(ho_out.contains("STAGE_ONLY_NOTE"), "{ho_out}");

    let ho_json = run_cli(&["handoffs", &run_id, "--stage", "implement", "--json"]);
    assert!(ho_json.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&ho_json.stdout).unwrap();
    assert_eq!(parsed["budget_bytes"], 24000);
    assert_eq!(parsed["each_bytes"], 8000);
    let files = parsed["context_files"].as_array().unwrap();
    assert!(
        files
            .iter()
            .any(|f| f["path"] == "stage-note.txt" && f["included"] == true)
    );
}
