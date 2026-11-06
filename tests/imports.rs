use model_router::{
    config::{CustomAdapter, Mode, Role, Settings, Stage, Workflow},
    engine,
};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    process::{Command, Output},
};

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings {
        workflows: vec![Workflow {
            name: "Import test".into(),
            max_repair_passes: 0,
            budget: Default::default(),
            stages: vec![
                Stage {
                    id: "implementation".into(),
                    name: "Implement".into(),
                    role: Role::Implementation,
                    mode: Mode::Unattended,
                    harness: "mock".into(),
                    provider: "implementation".into(),
                    model: "offline-test".into(),
                    ..Stage::default()
                },
                Stage {
                    // Otherwise loading appends an enabled Codex summary.
                    id: "summary".into(),
                    role: Role::Summary,
                    enabled: false,
                    ..Stage::default()
                },
            ],
        }],
        adapters: BTreeMap::from([(
            "mock".into(),
            CustomAdapter {
                command: "python3".into(),
                args: vec![
                    format!("{}/examples/mock_harness.py", env!("CARGO_MANIFEST_DIR")),
                    "--role".into(),
                    "{provider}".into(),
                    "--prompt-file".into(),
                    "{prompt_file}".into(),
                    "--handoff-file".into(),
                    "{handoff_file}".into(),
                ],
                ..CustomAdapter::default()
            },
        )]),
        ..Settings::default()
    };
    settings
        .save(&dir.path().join(".model-router.toml"))
        .unwrap();
    // The project config defines a custom adapter, so it needs trust.
    let trusted = Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("-C")
        .arg(dir.path())
        .args(["trust", "--yes"])
        .env("MODEL_ROUTER_CONFIG_DIR", dir.path().join("config-home"))
        .output()
        .unwrap();
    success(&trusted);
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("gh"), include_str!("../examples/mock_github.py")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(bin.join("gh"), fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

fn command(project: &Path) -> Command {
    let paths = std::iter::once(project.join("bin"))
        .chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ))
        .collect::<Vec<_>>();
    let mut command = Command::new(env!("CARGO_BIN_EXE_model-router"));
    command
        .current_dir(project)
        .arg("run")
        .env("MODEL_ROUTER_CONFIG_DIR", project.join("config-home"))
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("ROUTER_TEST_GH_TRACE", project.join("gh-requests.jsonl"));
    command
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cli_file_import_snapshots_the_document_and_passes_it_to_the_stage() {
    let dir = project();
    let path = dir.path().join("Task spec 雪.md");
    let text = "# Imported task 雪\n\n- Literal $(touch NEVER)\n";
    fs::write(&path, text).unwrap();
    success(
        &command(dir.path())
            .args(["--task-file", "Task spec 雪.md"])
            .output()
            .unwrap(),
    );
    fs::remove_file(&path).unwrap();
    let runs = engine::history(dir.path()).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].task, text);
    assert_eq!(runs[0].status, engine::Status::Complete);
    assert!(
        fs::read_to_string(dir.path().join(".model-router/mock/implementation.prompt"))
            .unwrap()
            .contains(text)
    );
    assert!(!dir.path().join("NEVER").exists());
}

#[test]
fn cli_github_imports_issue_file_and_slash_branch_urls_using_read_only_requests() {
    for (url, expected) in [
        (
            "https://github.com/acme/project/issues/7",
            "# GitHub issue spec",
        ),
        (
            "https://github.com/acme/project/blob/main/docs/spec.md",
            "# GitHub file spec",
        ),
        (
            "https://github.com/acme/project/blob/feature/new/docs/spec%20one.md",
            "# GitHub file spec",
        ),
        (
            "https://raw.githubusercontent.com/acme/project/refs/tags/v1.0/spec.md",
            "# GitHub file spec",
        ),
    ] {
        let dir = project();
        success(
            &command(dir.path())
                .args(["--task-url", url])
                .output()
                .unwrap(),
        );
        let runs = engine::history(dir.path()).unwrap();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].task.starts_with(expected));
        let trace = fs::read_to_string(dir.path().join("gh-requests.jsonl")).unwrap();
        if url.contains("feature/new") {
            assert!(trace.contains(
                "repos/acme/project/contents/docs/spec%20one.md?ref=refs%2Fheads%2Ffeature%2Fnew"
            ));
        }
        if url.contains("issues/7") {
            assert!(runs[0].task.contains(url));
        }
        for line in trace.lines() {
            let args: Vec<String> = serde_json::from_str(line).unwrap();
            assert_eq!(
                &args[..5],
                ["api", "--hostname", "github.com", "--method", "GET"]
            );
        }
    }
}

#[test]
fn import_errors_and_conflicting_flags_do_not_create_runs() {
    let dir = project();
    fs::write(dir.path().join("binary.md"), [0xff, 0]).unwrap();
    let output = command(dir.path())
        .args(["--task-file", "binary.md"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("UTF-8"));
    let output = command(dir.path())
        .args(["--task-url", "https://github.com/acme/project/issues/7"])
        .env("ROUTER_TEST_GH_FAILURE", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("GitHub import failed"));
    let output = command(dir.path())
        .args(["--task", "Original", "--task-file", "binary.md"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let output = command(dir.path())
        .args([
            "--task-file",
            "binary.md",
            "--task-url",
            "https://example.com/spec.md",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(engine::history(dir.path()).unwrap().is_empty());
}
