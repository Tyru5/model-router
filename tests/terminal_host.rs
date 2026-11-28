//! An interactive harness's terminal host, run as the real executable.
mod common;

use model_router::{
    adapters::{Invocation, OutputKind},
    process::{self, InteractiveSession},
};
use std::{
    fs,
    os::unix::net::UnixStream,
    sync::atomic::AtomicBool,
    thread,
    time::{Duration, Instant},
};

fn running(pid: i32) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&output.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

/// Ends the group however the test ends.
struct EndGroup(Option<u32>);
impl Drop for EndGroup {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            process::end_group(pgid, Duration::from_millis(100), || {});
        }
    }
}

#[test]
fn a_host_that_dies_before_the_first_attach_reports_a_surviving_group() {
    let dir = common::tempdir();
    let attempt = dir.path().join("attempt");
    fs::create_dir(&attempt).unwrap();
    let member_file = dir.path().join("member.pid");
    // The harness starts a member that ignores SIGTERM and SIGHUP and carries
    // no marker, then kills its host, as a crash would.
    let inv = Invocation {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            r#"sh -c 'trap "" TERM HUP; echo $$ > "$1"; exec env -u MODEL_ROUTER_GROUP sleep 30' member "$0" &
sleep 1; kill -KILL $PPID; exec sleep 30"#
                .into(),
            member_file.display().to_string(),
        ],
        stdin: None,
        kind: OutputKind::Text,
        final_file: attempt.join("unused"),
        env: vec![],
        files: vec![],
        assign: None,
    };
    // This test binary cannot act as the terminal host; the router can.
    process::host::use_program(env!("CARGO_BIN_EXE_model-router").into());
    let session = InteractiveSession::new(80, 24);
    let mut end = EndGroup(None);
    let error = process::interactive(
        &inv,
        dir.path(),
        &attempt,
        &AtomicBool::new(false),
        &session,
        None,
        &[],
        |group, socket| {
            end.0 = Some(group.pgid);
            // The router attaches only once the host is gone.
            let deadline = Instant::now() + Duration::from_secs(10);
            while UnixStream::connect(socket).is_ok() {
                assert!(Instant::now() < deadline, "the host never died");
                thread::sleep(Duration::from_millis(20));
            }
        },
    )
    .unwrap_err();
    let pgid = end.0.unwrap();
    let survivor = error
        .downcast_ref::<process::Survivor>()
        .unwrap_or_else(|| panic!("no survivor reported: {error:#}"));
    assert_eq!(survivor.group.pgid, pgid);
    assert!(
        format!("{error:#}").contains(&format!("Process group {pgid} may still be running")),
        "{error:#}"
    );
    let member: i32 = fs::read_to_string(&member_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(running(member), "the member was killed");
}

#[test]
fn finishing_an_interactive_stage_from_its_written_handoff_concludes_cleanly() {
    use model_router::{
        config::{Check, CustomAdapter, Mode, Role, Sandbox, Settings, Stage, Workflow},
        engine::{self, Event, Run, Status},
        handoff::TestStatus,
    };
    use std::{
        collections::BTreeMap,
        path::PathBuf,
        sync::{Arc, mpsc},
    };

    let dir = common::tempdir();
    let project = dir.path().join("project");
    fs::create_dir(&project).unwrap();
    let harness_script = project.join("stay_open.py");
    let pid_file = project.join("harness.pid");
    fs::write(
        &harness_script,
        r#"import json, os, pathlib, sys, time
pathlib.Path(sys.argv[2]).write_text(str(os.getpid()))
handoff = {
    "schema_version": 2,
    "summary": "Wrote handoff and stayed at interactive prompt",
    "verdict": "complete",
    "files_changed": [],
    "tests": [],
    "findings": [],
    "decisions": [],
    "unresolved": [],
}
pathlib.Path(sys.argv[1]).write_text(json.dumps(handoff))
while True:
    time.sleep(0.2)
"#,
    )
    .unwrap();

    let mut stage = Stage {
        id: "implement".into(),
        name: "Implement".into(),
        role: Role::Implementation,
        harness: "mock".into(),
        mode: Mode::Interactive,
        checks: vec![Check {
            command: vec![
                "python3".into(),
                "-c".into(),
                "print('router check ok')".into(),
            ],
            required: true,
            timeout_seconds: 30,
        }],
        ..Stage::default()
    };
    stage.sandbox = Some(Sandbox::Off);
    let settings = Settings {
        workflows: vec![Workflow::new("Interactive finish", vec![stage])],
        adapters: BTreeMap::from([(
            "mock".into(),
            CustomAdapter {
                command: "python3".into(),
                args: vec![
                    harness_script.display().to_string(),
                    "{handoff_file}".into(),
                    pid_file.display().to_string(),
                ],
                interactive_args: Some(vec![
                    harness_script.display().to_string(),
                    "{handoff_file}".into(),
                    pid_file.display().to_string(),
                ]),
                ..CustomAdapter::default()
            },
        )]),
        ..Settings::default()
    };
    process::host::use_program(PathBuf::from(env!("CARGO_BIN_EXE_model-router")));
    let mut run = Run::create(
        &project,
        &settings,
        &settings.workflows[0],
        "Finish interactive stage from handoff".into(),
        false,
    )
    .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    assert_eq!(
        engine::drive(&mut run, &cancel, |_| {}).unwrap(),
        engine::Action::Interactive
    );

    let session = InteractiveSession::new(80, 24);
    let (tx, rx) = mpsc::channel();
    let worker_session = session.clone();
    let worker_cancel = cancel.clone();
    let worker = thread::spawn(move || {
        let res = engine::run_interactive(&mut run, &worker_cancel, &worker_session, |event| {
            if let Event::InteractiveHandoff(handoff) = event {
                let _ = tx.send(handoff.summary);
            }
        });
        (run, res)
    });

    // Wait for the watcher to detect the written handoff while the harness stays open.
    let detected = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("watcher should emit Event::InteractiveHandoff");
    assert_eq!(detected, "Wrote handoff and stayed at interactive prompt");

    let pid: i32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(running(pid), "harness should still be open before finish");

    // Finish cleanly without sending /exit or setting cancel.
    session.finish_with_handoff();
    let (run, res) = worker.join().unwrap();
    res.unwrap();
    assert_eq!(run.status, Status::AwaitingConfirmation);
    assert_eq!(
        run.stages[0].handoff.as_ref().unwrap().summary,
        "Wrote handoff and stayed at interactive prompt"
    );
    assert_eq!(run.stages[0].attempts[0].checks.len(), 1);
    assert_eq!(
        run.stages[0].attempts[0].checks[0].status,
        TestStatus::Passed
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    while running(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!running(pid), "harness process was not stopped and reaped");
}
