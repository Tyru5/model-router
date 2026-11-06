//! Offline workflow fixtures shared by integration tests.
#![allow(dead_code)]
use model_router::{
    config::{CustomAdapter, Role, Settings, Stage, Workflow},
    engine::{self, Action, Run},
};
use std::{collections::BTreeMap, fs, path::Path, sync::atomic::AtomicBool};

pub fn fixture(stages: Vec<Stage>) -> (tempfile::TempDir, Settings, Run) {
    let dir = tempdir();
    let workflow = workflow(stages);
    let settings = settings(&workflow);
    let run = create(dir.path(), &settings, &workflow);
    (dir, settings, run)
}
pub fn tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("router test 雪 ")
        .tempdir()
        .unwrap()
}
pub fn workflow(stages: Vec<Stage>) -> Workflow {
    Workflow {
        name: "Test".into(),
        max_repair_passes: 2,
        max_parallel: 2,
        stages,
    }
}
/// Settings with the offline mock adapter and `workflow` saved.
pub fn settings(workflow: &Workflow) -> Settings {
    let adapter = CustomAdapter {
        command: "python3".into(),
        args: vec![
            format!("{}/examples/mock_harness.py", env!("CARGO_MANIFEST_DIR")),
            "--role".into(),
            "{provider}".into(),
            "--model".into(),
            "{model}".into(),
            "--prompt-file".into(),
            "{prompt_file}".into(),
            "--handoff-file".into(),
            "{handoff_file}".into(),
        ],
        ..CustomAdapter::default()
    };
    // Saved configs gain an enabled Codex summary on load; opt out to stay offline.
    let mut saved = workflow.clone();
    if !saved.stages.iter().any(|s| s.role == Role::Summary) {
        saved.stages.push(Stage {
            enabled: false,
            ..stage("summary", Role::Summary)
        });
    }
    Settings {
        workflows: vec![saved],
        adapters: BTreeMap::from([("mock".into(), adapter)]),
        ..Settings::default()
    }
}
pub fn create(dir: &Path, settings: &Settings, workflow: &Workflow) -> Run {
    Run::create(
        dir,
        settings,
        workflow,
        "Original task: preserve 雪 and $(touch NEVER) literally".into(),
        false,
    )
    .unwrap()
}
pub fn stage(id: &str, role: Role) -> Stage {
    Stage {
        id: id.into(),
        name: id.into(),
        role,
        harness: "mock".into(),
        provider: id.into(),
        model: "test-model".into(),
        ..Stage::default()
    }
}
pub fn drive(run: &mut Run) -> Action {
    engine::drive(run, &AtomicBool::new(false), |_| {}).unwrap()
}
pub fn count(path: &Path, role: &str) -> u32 {
    fs::read_to_string(path.join(format!(".model-router/mock/{role}.count")))
        .unwrap_or_else(|_| "0".into())
        .parse()
        .unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
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

/// A repository with one commit by the host's human identity, plus an
/// uncommitted draft; None when that identity is unset, as tests never
/// invent commit authorship.
pub fn repository(dir: &Path) -> Option<()> {
    let identity = |key| {
        let output = std::process::Command::new("git")
            .args(["config", "--get", key])
            .output()
            .unwrap();
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let (Some(name), Some(email)) = (identity("user.name"), identity("user.email")) else {
        eprintln!("Skipping Git fixture: configure the human git identity first");
        return None;
    };
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.name", &name]);
    git(dir, &["config", "user.email", &email]);
    fs::write(dir.join("tracked.txt"), "original\n").unwrap();
    git(dir, &["add", "tracked.txt"]);
    git(
        dir,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "Test fixture",
        ],
    );
    fs::write(dir.join("draft.txt"), "uncommitted draft\n").unwrap();
    Some(())
}
