//! Offline workflow fixtures shared by integration tests.
#![allow(dead_code)]
use model_router::{
    config::{CustomAdapter, Role, Settings, Stage, Workflow},
    engine::{self, Action, Run},
};
use std::{collections::BTreeMap, fs, path::Path, sync::atomic::AtomicBool};

pub fn fixture(stages: Vec<Stage>) -> (tempfile::TempDir, Settings, Run) {
    let dir = tempfile::Builder::new()
        .prefix("router test 雪 ")
        .tempdir()
        .unwrap();
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
    let workflow = Workflow {
        name: "Test".into(),
        max_repair_passes: 2,
        stages,
    };
    // Saved configs gain an enabled Codex summary on load; opt out to stay offline.
    let mut saved = workflow.clone();
    if !saved.stages.iter().any(|s| s.role == Role::Summary) {
        saved.stages.push(Stage {
            enabled: false,
            ..stage("summary", Role::Summary)
        });
    }
    let settings = Settings {
        workflows: vec![saved],
        adapters: BTreeMap::from([("mock".into(), adapter)]),
        ..Settings::default()
    };
    let run = Run::create(
        dir.path(),
        &settings,
        &workflow,
        "Original task: preserve 雪 and $(touch NEVER) literally".into(),
        false,
    )
    .unwrap();
    (dir, settings, run)
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
