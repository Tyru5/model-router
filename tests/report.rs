mod common;

use common::{drive, fixture, stage};
use model_router::{
    config::Role,
    engine::{Action, Run},
    report::{self, Changes},
    snapshot::ChangeKind,
};
use std::{
    fs,
    path::Path,
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
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Commits one file with the host's human identity; None when it is unset,
/// as tests never invent commit authorship.
fn repository(dir: &Path) -> Option<()> {
    let identity = |key| {
        let output = Command::new("git")
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
    assert_eq!(
        git(dir, &["log", "-1", "--format=%an <%ae>"]),
        format!("{name} <{email}>")
    );
    Some(())
}

fn cli(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("-C")
        .arg(dir)
        .arg("--config")
        .arg(dir.join("config.toml"))
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn report_lists_files_a_stage_wrote_in_a_git_workspace() {
    let mut implement = stage("implement", Role::Implementation);
    implement.extra_args = vec!["--write-file".into(), "src/new.txt".into()];
    let (dir, settings, _) = fixture(vec![implement]);
    if repository(dir.path()).is_none() {
        return;
    }
    // A change made before the run starts is not attributed to it.
    fs::write(dir.path().join("tracked.txt"), "edited by the user\n").unwrap();
    settings.save(&dir.path().join("config.toml")).unwrap();
    let mut run = Run::create(
        dir.path(),
        &settings,
        &settings.workflows[0],
        "Write a file".into(),
        false,
    )
    .unwrap();
    assert!(run.baseline.is_some());
    assert_eq!(drive(&mut run), Action::Complete);
    let attempt = &run.stages[0].attempts[0];
    assert!(attempt.tree_before.is_some());
    assert_ne!(attempt.tree_before, attempt.tree_after);
    assert_eq!(attempt.usage, None);

    let Changes::Tracked { files } = report::changes(&run) else {
        panic!("Expected tracked changes");
    };
    let files: Vec<_> = files
        .iter()
        .map(|f| (f.path.as_str(), f.status, f.added, f.removed))
        .collect();
    assert_eq!(
        files,
        [("src/new.txt", ChangeKind::Added, Some(1), Some(0))]
    );
    let saved = fs::read_to_string(run.directory.join("changes.diff")).unwrap();
    assert!(saved.contains("+written by implement attempt 1"), "{saved}");
    assert!(!saved.contains("edited by the user"));
    // The user's index and status are untouched.
    assert_eq!(git(dir.path(), &["diff", "--cached", "--name-only"]), "");
    assert_eq!(
        git(dir.path(), &["status", "--porcelain"]),
        "M tracked.txt\n?? config.toml\n?? src/"
    );

    let text = stdout(&cli(dir.path(), &["report", &run.id]));
    for expected in [
        "Status: complete · wall ",
        "1 attempt · 0 retries · repairs 0/2",
        "Usage: usage not reported",
        "Changed during this run: 1 file, +1 -0",
        "  A  src/new.txt  +1 -0",
        "Checks: none run or reported",
        "Unresolved: none",
    ] {
        assert!(
            text.contains(expected),
            "Report lacks {expected:?}:\n{text}"
        );
    }
    let json: serde_json::Value =
        serde_json::from_str(&stdout(&cli(dir.path(), &["report", &run.id, "--json"]))).unwrap();
    assert_eq!(json["status"], "complete");
    assert_eq!(json["changes"]["state"], "tracked");
    assert_eq!(json["changes"]["files"][0]["path"], "src/new.txt");
    assert_eq!(json["stages"][0]["attempts"], 1);
    let diff = stdout(&cli(dir.path(), &["report", &run.id, "--diff"]));
    assert!(diff.contains("+written by implement attempt 1"), "{diff}");
}

#[test]
fn cli_run_delivers_its_webhook_before_exiting() {
    use std::{io::Read, io::Write, net::TcpListener, sync::mpsc};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/topic", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            // Recorded before the reply the router waits for.
            let _ = tx.send(String::from_utf8_lossy(&request).into_owned());
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        }
    });
    // A stage that fails at once: the CLI exits right after notifying.
    let mut implement = stage("implement", Role::Implementation);
    implement.extra_args = vec!["--no-such-flag".into()];
    let (dir, settings, _) = fixture(vec![implement]);
    settings.save(&dir.path().join("config.toml")).unwrap();
    let config = common::tempdir();
    let output = Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("-C")
        .arg(dir.path())
        .arg("--config")
        .arg(dir.path().join("config.toml"))
        .args(["run", "--notify", "--task", "Fail and notify"])
        .env("MODEL_ROUTER_CONFIG_DIR", config.path())
        .env("MODEL_ROUTER_NOTIFY_URL", &url)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let request = rx
        .try_recv()
        .expect("the router exited before its webhook arrived");
    assert!(request.starts_with("POST /topic "), "{request}");
}

#[test]
fn cli_run_prints_the_report_and_labels_missing_git() {
    let (dir, settings, _) = fixture(vec![stage("implement", Role::Implementation)]);
    settings.save(&dir.path().join("config.toml")).unwrap();
    let text = stdout(&cli(dir.path(), &["run", "--task", "Summarize this run"]));
    for expected in [
        "Completed ",
        "Status: complete · wall ",
        "Changed during this run: unavailable (not a Git repository)",
        "Unresolved: none",
        "Stages:\n  implement  complete · 1 attempt · ",
    ] {
        assert!(
            text.contains(expected),
            "Output lacks {expected:?}:\n{text}"
        );
    }
    let id = text
        .lines()
        .find_map(|line| line.strip_prefix("Completed "))
        .unwrap()
        .to_owned();
    let diff = cli(dir.path(), &["report", &id, "--diff"]);
    assert!(!diff.status.success());
    assert!(
        String::from_utf8_lossy(&diff.stderr).contains("not a Git repository"),
        "{}",
        String::from_utf8_lossy(&diff.stderr)
    );
    assert!(!cli(dir.path(), &["report", "no-such-run"]).status.success());
}
