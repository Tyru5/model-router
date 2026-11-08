//! The router sandbox, end to end. Needs a working bubblewrap; skipped
//! (with the reason printed) elsewhere.
use model_router::{
    adapters::{Invocation, OutputKind},
    config::{CustomAdapter, Role, Sandbox, Settings, Stage, Workflow},
    engine::Status,
    engine::{self, Action, Run},
    process::{self, InteractiveSession},
    sandbox,
};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

/// Tries to write into the workspace, `/tmp`, and the handoff file; reports
/// which succeeded in the handoff summary.
const PROBE: &str = r#"
import json, pathlib, sys
def attempt(path):
    try:
        pathlib.Path(path).write_text("x")
        return "wrote"
    except OSError:
        return "blocked"
summary = "workspace=" + attempt("escaped.txt") + " tmp=" + attempt("/tmp/probe.txt")
handoff = {"summary": summary, "verdict": "complete", "findings": [], "decisions": [],
           "files_changed": [], "tests": [], "unresolved": []}
pathlib.Path(sys.argv[1]).write_text(json.dumps(handoff))
print("<router-handoff>" + json.dumps(handoff) + "</router-handoff>", flush=True)
"#;

fn available() -> bool {
    match sandbox::detect() {
        Ok(_) => true,
        Err(reason) => {
            eprintln!("skipping sandbox integration test: {reason}");
            false
        }
    }
}

fn run_probe(mode: Sandbox) -> (tempfile::TempDir, Run) {
    let dir = tempfile::Builder::new()
        .prefix("sandbox test 雪 ")
        .tempdir()
        .unwrap();
    // Inside the workspace: a private /tmp hides other temporary files.
    fs::write(dir.path().join("probe.py"), PROBE).unwrap();
    let adapter = CustomAdapter {
        command: "python3".into(),
        args: vec![
            dir.path().join("probe.py").display().to_string(),
            "{handoff_file}".into(),
        ],
        ..CustomAdapter::default()
    };
    let workflow = Workflow {
        max_repair_passes: 0,
        ..Workflow::new(
            "Sandbox",
            vec![Stage {
                id: "research".into(),
                name: "Research".into(),
                role: Role::Research,
                harness: "probe".into(),
                sandbox: Some(mode),
                ..Stage::default()
            }],
        )
    };
    let settings = Settings {
        workflows: vec![workflow.clone()],
        adapters: BTreeMap::from([("probe".into(), adapter)]),
        ..Settings::default()
    };
    let mut run = Run::create(dir.path(), &settings, &workflow, "probe".into(), false).unwrap();
    let action = engine::drive(&mut run, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(action, Action::Complete, "{:?}", run.error);
    (dir, run)
}

#[test]
fn read_only_blocks_workspace_writes_but_not_the_handoff() {
    if !available() {
        return;
    }
    let (dir, run) = run_probe(Sandbox::ReadOnly);
    let stage = &run.stages[0];
    assert_eq!(stage.status, Status::Complete, "{:?}", run.error);
    let summary = &stage.handoff.as_ref().unwrap().summary;
    assert_eq!(summary, "workspace=blocked tmp=wrote");
    assert!(!dir.path().join("escaped.txt").exists());
    assert!(stage.attempts[0].handoff_file.exists());
    let command: serde_json::Value = serde_json::from_slice(
        &fs::read(stage.attempts[0].directory.join("command.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(command["sandbox"]["mode"], "read-only");
    let (backend, _) = sandbox::detect().unwrap();
    assert_eq!(
        command["sandbox"]["backend"],
        serde_json::to_value(backend).unwrap()
    );
    assert!(
        command["sandbox"]["writable"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| Path::new(p.as_str().unwrap()) == stage.attempts[0].directory),
        "{command}"
    );
    // Under bubblewrap the sandbox's /tmp was private; sandbox-exec shares it.
    if backend == sandbox::Backend::Bubblewrap {
        assert!(
            !Path::new("/tmp/probe.txt").exists()
                || fs::read_to_string("/tmp/probe.txt").unwrap() != "x"
        );
    }
}

#[test]
fn workspace_write_allows_workspace_writes() {
    if !available() {
        return;
    }
    let (dir, run) = run_probe(Sandbox::WorkspaceWrite);
    let summary = &run.stages[0].handoff.as_ref().unwrap().summary;
    assert_eq!(summary, "workspace=wrote tmp=wrote");
    assert!(dir.path().join("escaped.txt").exists());
}

#[test]
fn interactive_harnesses_keep_their_terminal_inside_the_sandbox() {
    if !available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let attempt = dir.path().join("attempt");
    fs::create_dir(&attempt).unwrap();
    let workflow = Workflow {
        max_repair_passes: 0,
        sandbox_default: Sandbox::ReadOnly,
        ..Workflow::new("w", vec![])
    };
    let plan = sandbox::plan(
        &workflow,
        &Stage::default(),
        &sandbox::Paths {
            workspace: dir.path(),
            attempt_dir: &attempt,
            handoff_dir: &attempt,
            program: "sh",
        },
    )
    .unwrap()
    .unwrap();
    let inv = Invocation {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            "test -t 0 && test -t 1 && stty size > attempt/size; touch blocked 2>/dev/null; echo done > attempt/reply".into(),
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
    let session = InteractiveSession::new(100, 30);
    let cancel = AtomicBool::new(false);
    let outcome = thread::scope(|scope| {
        scope.spawn(|| {
            let start = Instant::now();
            while !attempt.join("reply").exists() && start.elapsed() < Duration::from_secs(10) {
                thread::sleep(Duration::from_millis(20));
            }
            if !attempt.join("reply").exists() {
                cancel.store(true, Ordering::Relaxed);
            }
        });
        process::interactive(
            &inv,
            dir.path(),
            &attempt,
            &cancel,
            &session,
            Some(&plan),
            &[],
            |_, _| {},
        )
        .unwrap()
    });
    assert!(outcome.success(), "{outcome:?}");
    assert_eq!(
        fs::read_to_string(attempt.join("size")).unwrap().trim(),
        "30 100"
    );
    assert_eq!(
        fs::read_to_string(attempt.join("reply")).unwrap().trim(),
        "done"
    );
    assert!(!dir.path().join("blocked").exists());
}
