//! Handoff contract v2: saved v1 runs, the prompt's contract size, and
//! native output-schema flags gated by harness version.
use model_router::{
    config::{Mode, Role, Settings, Stage, Workflow},
    engine::{self, Run},
    handoff, report,
};
use std::{fs, path::Path, process::Command};

const RUNS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/runs");

#[test]
fn version_1_runs_load_report_and_save_unchanged() {
    let path = Path::new(RUNS).join("v1/run.json");
    let text = fs::read_to_string(&path).unwrap();
    let run: Run = serde_json::from_str(&text).unwrap();
    let handoff = run.stages[0].handoff.as_ref().unwrap();
    assert_eq!(handoff.schema_version, None);
    assert!(handoff.structured && handoff.artifacts.is_empty());
    assert_eq!(run.stages[0].attempts[0].handoff_source, None);
    // Fields v2 added are omitted when unset, so the record is unchanged.
    let saved: serde_json::Value = serde_json::to_value(&run).unwrap();
    let original: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(saved, original);
    let rendered = report::render(&report::build(&run), None);
    let line = rendered
        .lines()
        .find(|l| l.starts_with("  Investigate"))
        .unwrap();
    assert!(line.contains("complete · 1 attempt"), "{line}");
    assert!(!rendered.contains("handoff:"));
}

/// The contract section before v2, for measuring the new one.
fn legacy_contract(handoff_path: &Path) -> String {
    format!(
        "\nHANDOFF CONTRACT\nFinish with the JSON below between the exact markers. Replace example values with evidence. Also write the JSON object (without markers) to {} if your tools permit it. This file is router bookkeeping; research/review may write this file while leaving project code untouched. In interactive mode, complete this handoff before exiting.\n{}",
        handoff_path.display(),
        r#"<router-handoff>
{"summary":"What was established or changed","verdict":"complete","findings":[],"decisions":[],"files_changed":[],"tests":[{"command":"check actually run","status":"passed","required":true}],"unresolved":[]}
</router-handoff>
Use complete for normal successful stages. Reviews MUST use pass or needs_changes.
Test stages MUST return a structured handoff reporting each check actually run in tests.
Only if no check can apply, a test stage may set "not_applicable":"reason"; the user must accept it.
Use blocked if you cannot complete the task (including missing permission).
Findings use {"severity":"blocking" or "advisory","description":"specific evidence"}.
Tests use status passed, failed, or not_run. Never claim a test ran if it did not.
"#
    )
}

#[test]
fn contract_is_at_least_a_fifth_shorter_for_every_starter_role() {
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings::default();
    let mut workflow = settings.workflows[0].clone();
    // Cover every role, not only the starter's.
    for role in [Role::Planning, Role::Test, Role::Custom] {
        workflow.stages.push(Stage {
            id: role.label().into(),
            role,
            ..Stage::default()
        });
    }
    let mut run =
        Run::create(dir.path(), &settings, &workflow, "Add a flag".into(), false).unwrap();
    let handoff_path = Path::new("/project/.model-router/handoffs/run/research-001.json");
    let schema_path =
        Path::new("/project/.model-router/runs/run/01-research/attempt-001/handoff.schema.json");
    let legacy = legacy_contract(handoff_path).len();
    for index in 0..workflow.stages.len() {
        run.cursor = index;
        let prompt = engine::prompt(&run, handoff_path, schema_path);
        let (_, contract) = prompt.split_once("\nHANDOFF CONTRACT").unwrap();
        let contract = contract.len() + "\nHANDOFF CONTRACT".len();
        let role = workflow.stages[index].role;
        eprintln!("{role:?}: contract {contract} bytes, legacy {legacy}");
        assert!(
            contract * 5 <= legacy * 4,
            "{role:?}: {contract} vs {legacy}"
        );
        assert!(prompt.contains(&schema_path.display().to_string()));
        assert!(prompt.contains(&handoff_path.display().to_string()));
        assert!(prompt.contains(&handoff::example(role)));
        let required = prompt.lines().find(|l| l.starts_with("Required:")).unwrap();
        match role {
            Role::Review => assert!(required.contains("pass or needs_changes")),
            Role::Test => assert!(required.contains("not_applicable")),
            _ => assert!(required.contains("blocked")),
        }
    }
}

/// `model-router preview` for a Codex and a Claude Code unattended stage,
/// with harness versions overridden and no harness on PATH.
fn preview(versions: Option<&str>) -> String {
    let dir = tempfile::tempdir().unwrap();
    let stage = |harness: &str| Stage {
        id: harness.into(),
        name: harness.into(),
        harness: harness.into(),
        mode: Mode::Unattended,
        ..Stage::default()
    };
    let settings = Settings {
        workflows: vec![Workflow {
            name: "Native".into(),
            max_repair_passes: 0,
            budget: Default::default(),
            sandbox_default: Default::default(),
            stages: vec![
                stage("codex"),
                stage("claude"),
                Stage {
                    id: "summary".into(),
                    role: Role::Summary,
                    enabled: false,
                    ..stage("codex")
                },
            ],
        }],
        ..Settings::default()
    };
    settings
        .save(&dir.path().join(".model-router.toml"))
        .unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_model-router"));
    command
        .arg("-C")
        .arg(dir.path())
        .arg("preview")
        .env("PATH", &bin)
        .env("MODEL_ROUTER_CONFIG_DIR", dir.path().join("config-home"))
        .env_remove("MODEL_ROUTER_HARNESS_VERSIONS");
    if let Some(versions) = versions {
        command.env("MODEL_ROUTER_HARNESS_VERSIONS", versions);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn native_schema_flags_follow_harness_versions() {
    let line = |text: &str, harness: &str| {
        text.lines()
            .skip_while(|l| !l.contains(&format!(". {harness} |")))
            .nth(1)
            .unwrap()
            .to_owned()
    };
    // Unknown versions count as current.
    let current = preview(None);
    assert!(
        line(&current, "codex").contains("--output-schema"),
        "{current}"
    );
    assert!(line(&current, "codex").contains(adapters_file()));
    assert!(
        line(&current, "claude").contains("--json-schema"),
        "{current}"
    );
    let supported = preview(Some("codex=codex-cli 0.41.0,claude=2.1.22 (Claude Code)"));
    assert!(line(&supported, "codex").contains("--output-schema"));
    assert!(line(&supported, "claude").contains("--json-schema"));
    let older = preview(Some("codex=codex-cli 0.40.9,claude=2.1.21 (Claude Code)"));
    assert!(
        !line(&older, "codex").contains("--output-schema"),
        "{older}"
    );
    assert!(!line(&older, "claude").contains("--json-schema"), "{older}");
    // The marker contract still applies without them.
    assert!(line(&older, "codex").contains("--output-last-message"));
}

fn adapters_file() -> &'static str {
    model_router::adapters::OUTPUT_SCHEMA_FILE
}
