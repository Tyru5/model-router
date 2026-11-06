//! The ship stage against a temporary repository, a local bare remote, and a
//! fake `gh` that records its arguments. No model, network, or real remote.
mod common;

use common::{drive, stage};
use model_router::{
    config::{CustomAdapter, Role, Settings, Stage, Workflow},
    engine::{Action, Cmd, Run, Status},
    report, ship,
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
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

/// The host's human identity; tests never invent commit authorship.
fn identity() -> Option<(String, String)> {
    let get = |key| {
        let output = Command::new("git")
            .args(["config", "--get", key])
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    match (get("user.name"), get("user.email")) {
        (Some(name), Some(email)) => Some((name, email)),
        _ => {
            eprintln!("Skipping ship test: configure the human git identity first");
            None
        }
    }
}

struct Fixture {
    repo: tempfile::TempDir,
    /// Outside the repository: config, fake executables, remote, gh log.
    home: tempfile::TempDir,
    settings: Settings,
    author: String,
}
impl Fixture {
    fn dir(&self) -> &Path {
        self.repo.path()
    }
    fn remote(&self) -> PathBuf {
        self.home.path().join("remote.git")
    }
    fn gh_log(&self) -> PathBuf {
        self.home.path().join("gh.jsonl")
    }
    fn config(&self) -> PathBuf {
        self.home.path().join("config.toml")
    }
    fn create(&self) -> Run {
        Run::create(
            self.dir(),
            &self.settings,
            &self.settings.workflows[0],
            "Add the new greeting file\n\nDetails follow.".into(),
            false,
        )
        .unwrap()
    }
    fn cli(&self, args: &[&str]) -> Output {
        let bin = self.home.path().join("bin");
        let path = std::iter::once(bin)
            .chain(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ))
            .collect::<Vec<_>>();
        Command::new(env!("CARGO_BIN_EXE_model-router"))
            .arg("-C")
            .arg(self.dir())
            .arg("--config")
            .arg(self.config())
            .args(args)
            .env("PATH", std::env::join_paths(path).unwrap())
            .env("FAKE_GH_LOG", self.gh_log())
            .env("MODEL_ROUTER_CONFIG_DIR", self.home.path().join("home"))
            .env("MODEL_ROUTER_NO_ANIMATION", "1")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }
    fn gh_calls(&self) -> Vec<Vec<String>> {
        fs::read_to_string(self.gh_log())
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

const FAKE_GH: &str = r#"#!/usr/bin/env python3
import json, os, shutil, sys
args = sys.argv[1:]
log = os.environ["FAKE_GH_LOG"]
with open(log, "a") as f:
    f.write(json.dumps(args) + "\n")
if os.environ.get("FAKE_GH_FAIL"):
    print("gh: simulated failure", file=sys.stderr)
    sys.exit(1)
if args[:2] == ["repo", "view"]:
    print("trunk")
elif args[:2] == ["pr", "create"]:
    shutil.copy(args[args.index("--body-file") + 1], log + ".body")
    print("https://github.com/example/repo/pull/7")
"#;

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn writer(id: &str, role: Role, file: &str) -> Stage {
    let mut stage = stage(id, role);
    stage.extra_args = vec!["--write-file".into(), file.into()];
    stage
}

fn ship_stage(options: ship::Options) -> Stage {
    Stage {
        id: "ship".into(),
        name: "Ship".into(),
        role: Role::Ship,
        ship: Some(options),
        ..Stage::default()
    }
}

/// A repository on `main` tracking a bare `origin`, with `stages` and a
/// ship stage. `before` edits the checkout before the run starts.
fn fixture(stages: Vec<Stage>, options: Option<ship::Options>) -> Option<Fixture> {
    let (name, email) = identity()?;
    let repo = tempfile::Builder::new()
        .prefix("ship test 雪 ")
        .tempdir()
        .unwrap();
    let home = tempfile::tempdir().unwrap();
    let dir = repo.path();
    git(dir, &["init", "-q"]);
    git(dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    for (key, value) in [
        ("user.name", name.as_str()),
        ("user.email", email.as_str()),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ] {
        git(dir, &["config", key, value]);
    }
    fs::write(dir.join("tracked.txt"), "original\n").unwrap();
    fs::write(dir.join("other.txt"), "original\n").unwrap();
    git(dir, &["add", "tracked.txt", "other.txt"]);
    git(dir, &["commit", "-qm", "Test fixture"]);
    let remote = home.path().join("remote.git");
    git(
        home.path(),
        &["init", "-q", "--bare", remote.to_str().unwrap()],
    );
    git(dir, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(dir, &["push", "-q", "-u", "origin", "main"]);

    let bin = home.path().join("bin");
    fs::create_dir(&bin).unwrap();
    for harness in [
        "codex",
        "claude",
        "gemini",
        "pi",
        "omp",
        "opencode",
        "aider",
        "goose",
        "amp",
        "copilot",
        "cursor-agent",
        "agent",
    ] {
        executable(&bin.join(harness), "#!/bin/sh\nexit 97\n");
    }
    executable(&bin.join("gh"), FAKE_GH);

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
    let mut stages = stages;
    if let Some(options) = options {
        stages.push(ship_stage(options));
    }
    stages.push(Stage {
        enabled: false,
        ..stage("summary", Role::Summary)
    });
    let settings = Settings {
        workflows: vec![Workflow {
            max_repair_passes: 0,
            ..Workflow::new("Ship test", stages)
        }],
        adapters: BTreeMap::from([("mock".into(), adapter)]),
        ..Settings::default()
    };
    let fixture = Fixture {
        repo,
        home,
        settings,
        author: format!("{name} <{email}>"),
    };
    fixture.settings.save(&fixture.config()).unwrap();
    Some(fixture)
}

fn two_writers() -> Vec<Stage> {
    vec![
        writer("implement", Role::Implementation, "src/new.txt"),
        writer("docs", Role::Custom, "other.txt"),
    ]
}

fn committed_files(dir: &Path, commit: &str) -> Vec<String> {
    let text = git(
        dir,
        &["show", "--name-only", "--format=", "--no-renames", commit],
    );
    let mut files: Vec<_> = text.lines().map(str::to_owned).collect();
    files.sort();
    files
}

#[test]
fn ship_previews_waits_for_confirmation_then_commits_only_run_files() {
    let Some(fx) = fixture(two_writers(), Some(ship::Options::default())) else {
        return;
    };
    let dir = fx.dir();
    // Changed by the user before the run: never committed.
    fs::write(dir.join("tracked.txt"), "edited by the user\n").unwrap();
    fs::write(dir.join("notes.txt"), "scratch\n").unwrap();
    git(dir, &["add", "notes.txt"]);
    let head = git(dir, &["rev-parse", "HEAD"]);
    let mut run = fx.create();

    assert_eq!(drive(&mut run), Action::Confirm);
    assert_eq!(run.status, Status::AwaitingConfirmation);
    let preview = run.stages[2].handoff.as_ref().unwrap().summary.clone();
    let branch = format!("mr/{}-add-the-new-greeting-file", run.id);
    for expected in [
        "Ship preview: nothing has been committed yet.",
        &format!("Branch: {branch} (new, from main at "),
        &format!("Author: {}", fx.author),
        "M  other.txt",
        "A  src/new.txt",
        "Changed before the run, left uncommitted: notes.txt, tracked.txt",
        "Push: no",
        "Draft PR: no",
        &format!("From model-router run {}.", run.id),
    ] {
        assert!(preview.contains(expected), "{expected}\n{preview}");
    }
    let attempt = run.stages[2].attempts.last().unwrap().directory.clone();
    assert!(attempt.join("preview.md").is_file());
    assert!(attempt.join("plan.json").is_file());
    // Nothing happened yet.
    assert_eq!(git(dir, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(dir, &["branch", "--list", "mr/*"]), "");

    run.command(Cmd::Confirm).unwrap();
    assert_eq!(drive(&mut run), Action::Complete);
    let commit = git(dir, &["rev-parse", "HEAD"]);
    assert_ne!(commit, head);
    assert_eq!(git(dir, &["symbolic-ref", "--short", "HEAD"]), branch);
    assert_eq!(git(dir, &["rev-parse", "HEAD^"]), head);
    assert_eq!(committed_files(dir, &commit), ["other.txt", "src/new.txt"]);
    assert_eq!(
        git(dir, &["log", "-1", "--format=%an <%ae>|%cn <%ce>"]),
        format!("{0}|{0}", fx.author)
    );
    let message = git(dir, &["log", "-1", "--format=%B"]);
    assert!(
        message.starts_with("docs evidence from model test-model, attempt 1"),
        "{message}"
    );
    assert!(
        !message.to_lowercase().contains("co-authored-by"),
        "{message}"
    );
    assert!(
        !message.to_lowercase().contains("generated with"),
        "{message}"
    );
    let trailers = Command::new("git")
        .args(["interpret-trailers", "--parse"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(message.as_bytes())?;
            child.wait_with_output()
        })
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&trailers.stdout).trim(), "");
    // The user's earlier edits and staging stay as they were.
    assert_eq!(
        git(dir, &["status", "--porcelain"]),
        "A  notes.txt\n M tracked.txt"
    );
    assert_eq!(
        git(dir, &["branch", "--list", "main", "--format=%(objectname)"]),
        head
    );

    let shipped = &report::build(&run).shipped;
    assert_eq!(shipped.len(), 1);
    assert_eq!(shipped[0].commit.as_deref(), Some(commit.as_str()));
    assert_eq!(shipped[0].pushed, None);
    let text = report::render(&report::build(&run), None);
    assert!(text.contains(&format!("Commit: {commit}")), "{text}");
    assert!(!text.contains("not applicable"), "{text}");
    let handoff = run.stages[2].handoff.as_ref().unwrap();
    assert!(handoff.decisions.contains(&format!("Branch: {branch}")));
}

#[test]
fn files_changed_before_and_during_the_run_block_unless_included() {
    let Some(fx) = fixture(two_writers(), Some(ship::Options::default())) else {
        return;
    };
    let dir = fx.dir();
    fs::write(dir.join("other.txt"), "edited by the user\n").unwrap();
    let head = git(dir, &["rev-parse", "HEAD"]);
    let mut run = fx.create();
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!(run.status, Status::Blocked);
    let error = run.error.clone().unwrap();
    assert!(
        error.contains("changed both before and during the run")
            && error.contains("other.txt")
            && error.contains("include_preexisting"),
        "{error}"
    );
    assert_eq!(git(dir, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(dir, &["branch", "--list", "mr/*"]), "");

    // Opting in commits the file with the user's edits, and says so.
    let options = ship::Options {
        include_preexisting: true,
        confirm: false,
        ..ship::Options::default()
    };
    run.workflow.stages[2].ship = Some(options);
    run.persist().unwrap();
    run.command(Cmd::Retry {
        interactive: false,
        model: None,
    })
    .unwrap();
    assert_eq!(drive(&mut run), Action::Complete);
    assert_eq!(committed_files(dir, "HEAD"), ["other.txt", "src/new.txt"]);
    let plan: ship::Plan = serde_json::from_slice(
        &fs::read(
            run.stages[2]
                .attempts
                .last()
                .unwrap()
                .directory
                .join("plan.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("before and during") && w.contains("other.txt")),
        "{:?}",
        plan.warnings
    );
}

#[test]
fn a_changed_plan_needs_confirming_again() {
    let Some(fx) = fixture(two_writers(), Some(ship::Options::default())) else {
        return;
    };
    let mut run = fx.create();
    assert_eq!(drive(&mut run), Action::Confirm);
    run.command(Cmd::Confirm).unwrap();
    // HEAD's branch gains a commit meanwhile: the plan's base changes.
    let dir = fx.dir();
    git(dir, &["commit", "-q", "--allow-empty", "-m", "Meanwhile"]);
    assert_eq!(drive(&mut run), Action::Confirm);
    let preview = &run.stages[2].handoff.as_ref().unwrap().summary;
    assert!(
        preview.starts_with("The plan changed since you confirmed it"),
        "{preview}"
    );
    assert_eq!(git(dir, &["branch", "--list", "mr/*"]), "");
    run.command(Cmd::Confirm).unwrap();
    assert_eq!(drive(&mut run), Action::Complete);
    assert_eq!(
        git(dir, &["log", "-1", "--format=%s", "HEAD^"]),
        "Meanwhile"
    );
}

#[test]
fn push_failure_blocks_with_gits_stderr_and_keeps_the_commit() {
    let options = ship::Options {
        push: true,
        remote: "broken".into(),
        confirm: false,
        ..ship::Options::default()
    };
    let Some(fx) = fixture(two_writers(), Some(options)) else {
        return;
    };
    let dir = fx.dir();
    let missing = fx.home.path().join("missing.git");
    git(dir, &["remote", "add", "broken", missing.to_str().unwrap()]);
    let mut run = fx.create();
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!(run.status, Status::Blocked);
    let error = run.error.clone().unwrap();
    assert!(
        error.starts_with("Ship stopped: git push failed"),
        "{error}"
    );
    assert!(
        error.contains("does not appear to be a git repository") || error.contains("fatal"),
        "{error}"
    );
    let commit = git(dir, &["rev-parse", "HEAD"]);
    assert!(
        error.contains(&format!("Already done: commit {}", &commit[..12])),
        "{error}"
    );
    let outcome = &report::build(&run).shipped[0];
    assert_eq!(outcome.commit.as_deref(), Some(commit.as_str()));
    assert!(outcome.pushed.is_none() && outcome.error.is_some());
    // Retrying never moves the branch it made.
    run.command(Cmd::Retry {
        interactive: false,
        model: None,
    })
    .unwrap();
    assert_eq!(drive(&mut run), Action::Stopped);
    assert!(run.error.as_deref().unwrap().contains("already exists"));
    assert_eq!(git(dir, &["rev-parse", "HEAD"]), commit);
}

#[test]
fn options_validate_and_ship_stages_need_no_harness() {
    let mut settings = Settings::default();
    settings.workflows[0].stages.push(ship_stage(ship::Options {
        draft_pr: true,
        ..ship::Options::default()
    }));
    let error = settings.validate().unwrap_err().to_string();
    assert!(error.contains("draft_pr requires push"), "{error}");
    settings.workflows[0].stages.last_mut().unwrap().ship = Some(ship::Options::default());
    settings.validate().unwrap();
    settings.workflows[0].stages[0].ship = Some(ship::Options::default());
    assert!(
        settings.validate().is_err(),
        "ship options on a non-ship stage"
    );

    let text = r#"
version = 2
[[workflows]]
name = "W"
[[workflows.stages]]
id = "ship"
name = "Ship"
role = "ship"
[workflows.stages.ship]
push = true
draft_pr = true
base = "develop"
"#;
    let parsed = model_router::config::parse(text).unwrap();
    parsed.validate().unwrap();
    let options = parsed.workflows[0].stages[0].ship.clone().unwrap();
    assert!(options.push && options.draft_pr && options.confirm && options.include_untracked);
    assert_eq!(
        (
            options.branch.as_str(),
            options.remote.as_str(),
            options.base.as_str()
        ),
        ("mr/{run_id}-{slug}", "origin", "develop")
    );
    let typo = model_router::config::parse(&text.replace("base =", "bse =")).unwrap_err();
    assert!(format!("{typo:#}").contains("unknown field"), "{typo:#}");
}

#[test]
fn cli_run_waits_then_resume_advance_pushes_and_opens_a_draft_pr() {
    let options = ship::Options {
        push: true,
        draft_pr: true,
        ..ship::Options::default()
    };
    let Some(fx) = fixture(two_writers(), Some(options)) else {
        return;
    };
    let dir = fx.dir();
    let output = fx.cli(&["run", "--task", "Add the new greeting file"]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!output.status.success(), "{stdout}");
    assert!(stderr.contains("ship plan awaits confirmation"), "{stderr}");
    assert!(stdout.contains("Ship preview"), "{stdout}");
    assert!(
        stdout.contains("Draft PR: gh pr create --draft --base main --head mr/"),
        "{stdout}"
    );
    let id = stdout
        .lines()
        .find_map(|l| l.strip_prefix("Run: "))
        .unwrap()
        .to_owned();
    // Previewing never asks gh: the checkout's upstream names the base.
    assert!(fx.gh_calls().is_empty(), "{:?}", fx.gh_calls());
    let branch = format!("mr/{id}-add-the-new-greeting-file");
    assert!(
        Command::new("git")
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}")
            ])
            .current_dir(fx.remote())
            .output()
            .unwrap()
            .stdout
            .is_empty()
    );

    let output = fx.cli(&["resume", &id, "--advance"]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let commit = git(dir, &["rev-parse", "HEAD"]);
    assert_eq!(
        git(
            &fx.remote(),
            &["rev-parse", &format!("refs/heads/{branch}")]
        ),
        commit
    );
    assert_eq!(
        git(dir, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        format!("origin/{branch}")
    );
    let calls = fx.gh_calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let call = &calls[0];
    let body_file = call[call.iter().position(|a| a == "--body-file").unwrap() + 1].clone();
    assert_eq!(
        call,
        &[
            "pr",
            "create",
            "--draft",
            "--title",
            "docs evidence from model test-model, attempt 1",
            "--body-file",
            &body_file,
            "--head",
            &branch,
            "--base",
            "main",
        ]
    );
    assert!(body_file.ends_with("pr-body.md"), "{body_file}");
    let body = fs::read_to_string(fx.gh_log().with_extension("jsonl.body")).unwrap();
    for expected in [
        "## Changed files",
        "- `src/new.txt` (A, +1 -0)",
        "## Checks",
        "## Unresolved",
        &format!("From model-router run {id}."),
    ] {
        assert!(body.contains(expected), "{expected}\n{body}");
    }
    for expected in [
        "Shipped:",
        &format!("Pushed: origin/{branch}"),
        "Draft PR: https://github.com/example/repo/pull/7",
    ] {
        assert!(stdout.contains(expected), "{expected}\n{stdout}");
    }
    let report = fx.cli(&["report", &id, "--json"]);
    let value: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(
        value["shipped"][0]["pr_url"],
        "https://github.com/example/repo/pull/7"
    );
}

#[test]
fn cli_ship_ships_a_completed_run_after_the_fact() {
    let Some(fx) = fixture(two_writers(), None) else {
        return;
    };
    let dir = fx.dir();
    // No upstream: the PR base comes from gh's default branch.
    git(dir, &["branch", "--unset-upstream"]);
    let mut run = fx.create();
    assert_eq!(drive(&mut run), Action::Complete);
    let head = git(dir, &["rev-parse", "HEAD"]);

    let output = fx.cli(&["ship", &run.id, "--draft-pr"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains("Add --yes"),
        "{stderr}"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Ship preview"));
    assert_eq!(git(dir, &["rev-parse", "HEAD"]), head, "nothing shipped");
    assert_eq!(fx.gh_calls()[0][..2], ["repo", "view"]);

    let output = fx.cli(&["ship", &run.id, "--draft-pr", "--yes"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let branch = format!("mr/{}-add-the-new-greeting-file", run.id);
    let commit = git(dir, &["rev-parse", "HEAD"]);
    assert_eq!(committed_files(dir, &commit), ["other.txt", "src/new.txt"]);
    assert_eq!(
        git(
            &fx.remote(),
            &["rev-parse", &format!("refs/heads/{branch}")]
        ),
        commit
    );
    let create = fx.gh_calls().pop().unwrap();
    assert_eq!(create[..3], ["pr", "create", "--draft"]);
    assert_eq!(create.last().map(String::as_str), Some("trunk"));
    let report = fx.cli(&["report", &run.id]);
    let text = String::from_utf8_lossy(&report.stdout);
    assert!(text.contains(&format!("Commit: {commit}")), "{text}");
    assert!(run.directory.join("ship/ship-002/ship.json").is_file());
}

#[test]
fn gh_failure_blocks_with_its_stderr() {
    let Some(fx) = fixture(two_writers(), None) else {
        return;
    };
    let mut run = fx.create();
    assert_eq!(drive(&mut run), Action::Complete);
    let mut command = Command::new(env!("CARGO_BIN_EXE_model-router"));
    let bin = fx.home.path().join("bin");
    let path = std::iter::once(bin)
        .chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ))
        .collect::<Vec<_>>();
    let output = command
        .arg("-C")
        .arg(fx.dir())
        .arg("--config")
        .arg(fx.config())
        .args(["ship", &run.id, "--draft-pr", "--yes"])
        .env("PATH", std::env::join_paths(path).unwrap())
        .env("FAKE_GH_LOG", fx.gh_log())
        .env("FAKE_GH_FAIL", "1")
        .env("MODEL_ROUTER_CONFIG_DIR", fx.home.path().join("home"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    // The upstream names the base, so the only gh call is the failing create.
    assert!(
        stderr.contains("Ship stopped: gh pr failed (exit 1): gh: simulated failure"),
        "{stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Pushed: origin/mr/"), "{stdout}");
}

#[test]
fn ship_refuses_without_the_users_git_identity() {
    let Some(fx) = fixture(two_writers(), None) else {
        return;
    };
    let mut run = fx.create();
    assert_eq!(drive(&mut run), Action::Complete);
    let dir = fx.dir();
    git(dir, &["config", "--unset", "user.name"]);
    git(dir, &["config", "--unset", "user.email"]);
    let empty = fx.home.path().join("empty-gitconfig");
    fs::write(&empty, "").unwrap();
    let head = git(dir, &["rev-parse", "HEAD"]);
    let output = Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("-C")
        .arg(dir)
        .arg("--config")
        .arg(fx.config())
        .args(["ship", &run.id, "--yes"])
        .env("GIT_CONFIG_GLOBAL", &empty)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Some Assistant")
        .env("GIT_AUTHOR_EMAIL", "assistant@example.invalid")
        .env("MODEL_ROUTER_CONFIG_DIR", fx.home.path().join("home"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains("Set your Git identity first"),
        "{stderr}"
    );
    assert_eq!(git(dir, &["rev-parse", "HEAD"]), head);
}

#[test]
fn preview_names_the_ship_stage_without_a_harness_command() {
    let Some(fx) = fixture(two_writers(), Some(ship::Options::default())) else {
        return;
    };
    let output = fx.cli(&["preview"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(
            "3. Ship | ship | router, no harness\n   branch mr/{run_id}-{slug} · confirm"
        ),
        "{stdout}"
    );
}
