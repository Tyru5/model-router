mod common;

use common::{fixture, stage};
use model_router::{
    bench::{self, Options, Plan},
    config::{Mode, Role, Settings},
    engine,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, atomic::AtomicBool},
};

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(status.status.success(), "{status:?}");
}

/// A committed Git project whose workflow implements with the mock harness.
fn project() -> (tempfile::TempDir, Settings) {
    let (dir, settings, _) = fixture(vec![
        stage("implement", Role::Implementation),
        stage("review", Role::Review),
    ]);
    // The fixture's own run is not part of any bench.
    fs::remove_dir_all(dir.path().join(".model-router/runs")).unwrap();
    git(dir.path(), &["init", "-q"]);
    fs::write(dir.path().join("README"), "bench project\n").unwrap();
    git(dir.path(), &["add", "README"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    (dir, settings)
}

fn bench_file(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("bench.toml");
    fs::write(&path, body).unwrap();
    path
}

const MATRIX: &str = r#"
tasks = [
  { name = "inline", text = "Add a greeting" },
  { file = "task.md" },
]
[[matrix]]
stage = "implement"
assignments = [{ model = "test-model" }, { model = "fail" }]
"#;

fn options(jobs: usize) -> Options {
    Options {
        jobs,
        allow_interactive: false,
    }
}

#[test]
fn plan_expands_tasks_by_variants_and_previews() {
    let (_project, settings) = project();
    let files = tempfile::tempdir().unwrap();
    fs::write(files.path().join("task.md"), "# Spec\nDo the thing").unwrap();
    let path = bench_file(files.path(), MATRIX);
    let plan = Plan::new(&path, &bench::load(&path).unwrap(), &settings, &options(1)).unwrap();
    assert_eq!(plan.cells.len(), 4);
    assert_eq!(
        plan.tasks[1],
        ("task-2".into(), "# Spec\nDo the thing".into())
    );
    let labels: Vec<_> = plan.variants.iter().map(|v| v.label.as_str()).collect();
    assert_eq!(labels, ["implement=mock/test-model", "implement=mock/fail"]);
    // Assignments keep the stage's provider when the harness stays the same.
    let workflow = plan.workflow_for(1).unwrap();
    assert_eq!(workflow.stages[0].provider, "implement");
    assert_eq!(workflow.stages[0].model, "fail");
    let preview = plan.preview();
    assert!(
        preview.contains("2 task(s) × 2 variant(s) = 4 run(s)"),
        "{preview}"
    );
    assert!(preview.contains("task-2: # Spec"), "{preview}");

    for (body, error) in [
        (
            "tasks = [{ text = \"x\" }]\n[[matrix]]\nstage = \"nope\"\nassignments = [{ model = \"a\" }]",
            "has no stage nope",
        ),
        (
            "tasks = [{ text = \"x\" }]\n[[matrix]]\nstage = \"review\"\nassignments = [{ model = \"a\" }]",
            "Stage review is disabled",
        ),
        (
            "tasks = [{ text = \"x\", file = \"task.md\" }]",
            "exactly one of text or file",
        ),
        ("tasks = []", "lists no tasks"),
    ] {
        let path = bench_file(files.path(), body);
        let bench = bench::load(&path).unwrap();
        let mut settings = settings.clone();
        settings.workflows[0].stages[1].enabled = false;
        let message = Plan::new(&path, &bench, &settings, &options(1))
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(message.contains(error), "{body}: {message}");
    }
}

#[test]
fn interactive_stages_are_refused_unless_allowed() {
    let (_project, mut settings) = project();
    settings.workflows[0].stages[0].mode = Mode::Interactive;
    let files = tempfile::tempdir().unwrap();
    let path = bench_file(files.path(), "tasks = [{ text = \"x\" }]");
    let bench = bench::load(&path).unwrap();
    let error = Plan::new(&path, &bench, &settings, &options(1))
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("interactive stages: implement"), "{error}");
    assert!(error.contains("--allow-interactive"), "{error}");
    let allowed = Options {
        jobs: 1,
        allow_interactive: true,
    };
    let plan = Plan::new(&path, &bench, &settings, &allowed).unwrap();
    assert!(
        plan.workflow
            .stages
            .iter()
            .all(|s| s.mode == Mode::Unattended)
    );
}

#[test]
fn runs_every_cell_in_its_own_worktree_and_saves_a_summary() {
    let (project, settings) = project();
    let files = tempfile::tempdir().unwrap();
    fs::write(files.path().join("task.md"), "Do the thing").unwrap();
    let path = bench_file(files.path(), MATRIX);
    let plan = Plan::new(&path, &bench::load(&path).unwrap(), &settings, &options(2)).unwrap();
    let lines = Mutex::new(vec![]);
    let results = bench::run(
        project.path(),
        &settings,
        &plan,
        &options(2),
        &AtomicBool::new(false),
        &|line| lines.lock().unwrap().push(line),
    )
    .unwrap();
    assert_eq!(results.jobs, 2);
    assert_eq!(results.results.len(), 4);
    assert_eq!(lines.lock().unwrap().len(), 8);
    let mut workspaces = vec![];
    for result in &results.results {
        let expect_success = result.variant.ends_with("test-model");
        assert_eq!(result.success, expect_success, "{result:?}");
        assert_eq!(
            result.status,
            if expect_success { "complete" } else { "failed" }
        );
        let run = engine::load(project.path(), result.run.as_deref().unwrap()).unwrap();
        assert!(run.worktree);
        assert!(
            run.workflow
                .stages
                .iter()
                .all(|s| s.mode == Mode::Unattended)
        );
        assert_ne!(run.workspace, project.path());
        workspaces.push(run.workspace);
    }
    workspaces.sort();
    workspaces.dedup();
    assert_eq!(workspaces.len(), 4);

    let (json, markdown) = bench::save(project.path(), &results).unwrap();
    assert!(
        json.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("bench-")
    );
    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&json).unwrap()).unwrap();
    assert_eq!(saved["results"].as_array().unwrap().len(), 4);
    let table = fs::read_to_string(markdown).unwrap();
    assert!(
        table.contains("| implement=mock/test-model | 2/2 |"),
        "{table}"
    );
    assert!(table.contains("| implement=mock/fail | 0/2 |"), "{table}");
    assert!(table.contains("Simulated authentication"), "{table}");
    // A second save in the same second never overwrites the first.
    let (again, _) = bench::save(project.path(), &results).unwrap();
    assert_ne!(again, json);
}

#[test]
fn cancelled_bench_skips_cells_and_a_non_git_project_is_refused() {
    let (project, settings) = project();
    let files = tempfile::tempdir().unwrap();
    fs::write(files.path().join("task.md"), "Do the thing").unwrap();
    let path = bench_file(files.path(), MATRIX);
    let plan = Plan::new(&path, &bench::load(&path).unwrap(), &settings, &options(1)).unwrap();
    let results = bench::run(
        project.path(),
        &settings,
        &plan,
        &options(1),
        &AtomicBool::new(true),
        &|_| {},
    )
    .unwrap();
    assert!(results.results.iter().all(|r| r.status == "skipped"));
    assert!(!project.path().join(".model-router/runs").exists());
    let plain = tempfile::tempdir().unwrap();
    let error = bench::run(
        plain.path(),
        &settings,
        &plan,
        &options(1),
        &AtomicBool::new(false),
        &|_| {},
    )
    .err()
    .unwrap()
    .to_string();
    assert!(error.contains("not a Git repository"), "{error}");
}

fn cli(dir: &Path, settings: &Settings, args: &[&str]) -> (bool, String) {
    let config = dir.join(".model-router/config.toml");
    settings.save(&config).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("-C")
        .arg(dir)
        .arg("--config")
        .arg(&config)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

#[test]
fn bench_command_dry_runs_and_runs() {
    let (project, settings) = project();
    let files = tempfile::tempdir().unwrap();
    let path = bench_file(
        files.path(),
        "tasks = [{ name = \"one\", text = \"Add a greeting\" }]\n[[matrix]]\nstage = \"implement\"\nassignments = [{ model = \"test-model\" }]\n",
    );
    let file = path.to_str().unwrap();
    let (ok, text) = cli(project.path(), &settings, &["bench", file, "--dry-run"]);
    assert!(ok, "{text}");
    assert!(
        text.contains("1 task(s) × 1 variant(s) = 1 run(s)"),
        "{text}"
    );
    assert!(text.contains("Dry run: nothing started."), "{text}");
    assert!(!project.path().join(".model-router/runs").exists());

    let (ok, text) = cli(project.path(), &settings, &["bench", file, "--jobs", "1"]);
    assert!(ok, "{text}");
    assert!(
        text.contains("| implement=mock/test-model | 1/1 |"),
        "{text}"
    );
    assert!(text.contains("bench-"), "{text}");
    assert!(project.path().join(".model-router/bench").is_dir());
}
