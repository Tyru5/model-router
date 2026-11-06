//! Workspace snapshots as Git tree objects. A run-private index
//! (`<run dir>/snapshot.index`) records the working tree, so the user's index,
//! HEAD, refs, and files never change. Untracked files are included and ignored
//! ones are not, which keeps the self-ignored `.model-router/` out.
//!
//! Trees are not reachable from any commit, so each is kept by a ref,
//! `refs/model-router/runs/<run id>/<tree>`, or `git gc` would prune it and
//! old runs would lose their diffs. Cleaning a run deletes its refs.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Upper bound for patch text held in memory or saved as changes.diff.
pub const PATCH_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    /// E.g. a file replaced by a symlink.
    TypeChanged,
}
impl ChangeKind {
    /// The `git diff --name-status` letter.
    pub fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Deleted => 'D',
            Self::TypeChanged => 'T',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub status: ChangeKind,
    /// Line counts; None for binary files.
    pub added: Option<u64>,
    pub removed: Option<u64>,
}

fn git(dir: &Path, index: Option<&Path>, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command.args(args).current_dir(dir).stdin(Stdio::null());
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = command.output().context("Start Git")?;
    ensure!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

pub fn is_repository(dir: &Path) -> bool {
    git(dir, None, &["rev-parse", "--is-inside-work-tree"])
        .is_ok_and(|out| out.starts_with(b"true"))
}

/// The working tree as a Git tree ID. None outside Git or when Git fails; a
/// snapshot never fails a run.
pub fn tree(workspace: &Path, run_dir: &Path) -> Option<String> {
    let run = run_dir.file_name().and_then(|name| name.to_str())?;
    tree_with(workspace, &run_dir.join("snapshot.index"), run)
}

/// `tree` with its own private index, e.g. for a fan-out candidate's
/// worktree; kept under run `run`'s refs.
pub fn tree_with(workspace: &Path, index: &Path, run: &str) -> Option<String> {
    let index = std::path::absolute(index).ok()?;
    let tree = capture(workspace, &index).ok().flatten()?;
    // Best effort: an unkept tree still serves until the next gc.
    let _ = git(
        workspace,
        None,
        &["update-ref", &format!("{}/{tree}", namespace(run)), &tree],
    );
    Some(tree)
}

/// The ref keeping fan-out base commit `name` of run `run`.
pub fn base_ref(run: &str, name: &str) -> String {
    format!("{}/base/{name}", namespace(run))
}

fn namespace(run: &str) -> String {
    format!("refs/model-router/runs/{run}")
}

/// Deletes the refs keeping run `run`'s snapshots. Best effort; nothing to do
/// outside Git.
pub fn forget(project: &Path, run: &str) {
    let prefix = format!("{}/", namespace(run));
    let Ok(refs) = git(
        project,
        None,
        &["for-each-ref", "--format=%(refname)", &prefix],
    ) else {
        return;
    };
    let commands: String = String::from_utf8_lossy(&refs)
        .lines()
        .filter(|name| name.starts_with(&prefix))
        .map(|name| format!("delete {name}\n"))
        .collect();
    if commands.is_empty() {
        return;
    }
    let child = Command::new("git")
        .args(["update-ref", "--stdin"])
        .current_dir(project)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        if let Some(mut input) = child.stdin.take() {
            let _ = std::io::Write::write_all(&mut input, commands.as_bytes());
        }
        let _ = child.wait();
    }
}

fn capture(workspace: &Path, index: &Path) -> Result<Option<String>> {
    if !is_repository(workspace) {
        return Ok(None);
    }
    if !index.exists() {
        seed(workspace, index)?;
    }
    // Snapshots are taken under the run lock, so a leftover lock is from an
    // interrupted add and would fail every later snapshot.
    let mut lock = OsString::from(index);
    lock.push(".lock");
    let _ = fs::remove_file(PathBuf::from(lock));
    git(workspace, Some(index), &["add", "-A"])?;
    let tree = git(workspace, Some(index), &["write-tree"])?;
    Ok(Some(String::from_utf8_lossy(&tree).trim().to_owned()))
}

/// Seeds the private index from the real one, whose stat data keeps the first
/// add fast; else from HEAD; else, for an unborn HEAD, starts empty.
fn seed(workspace: &Path, index: &Path) -> Result<()> {
    let real = git(workspace, None, &["rev-parse", "--git-path", "index"])?;
    let real = workspace.join(String::from_utf8_lossy(&real).trim_end());
    if fs::copy(&real, index).is_ok() {
        return Ok(());
    }
    let _ = fs::remove_file(index);
    if git(
        workspace,
        None,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
    )
    .is_ok()
    {
        git(workspace, Some(index), &["read-tree", "HEAD"])?;
    }
    Ok(())
}

/// Files that differ between two snapshot trees.
pub fn changes(dir: &Path, from: &str, to: &str) -> Result<Vec<FileChange>> {
    let names = git(
        dir,
        None,
        &["diff", "--no-renames", "-z", "--name-status", from, to],
    )?;
    let numstat = git(
        dir,
        None,
        &["diff", "--no-renames", "-z", "--numstat", from, to],
    )?;
    // Each record is "added\tremoved\tpath"; binary files count "-".
    let counts: HashMap<&[u8], (Option<u64>, Option<u64>)> = numstat
        .split(|&b| b == 0)
        .filter_map(|record| {
            let mut fields = record.splitn(3, |&b| b == b'\t');
            let (added, removed, path) = (fields.next()?, fields.next()?, fields.next()?);
            Some((path, (number(added), number(removed))))
        })
        .collect();
    let mut fields = names.split(|&b| b == 0);
    let mut files = vec![];
    while let (Some(code), Some(path)) = (fields.next(), fields.next()) {
        let status = match code.first() {
            Some(b'A') => ChangeKind::Added,
            Some(b'D') => ChangeKind::Deleted,
            Some(b'T') => ChangeKind::TypeChanged,
            Some(_) => ChangeKind::Modified,
            None => break,
        };
        let (added, removed) = counts.get(path).copied().unwrap_or_default();
        files.push(FileChange {
            path: String::from_utf8_lossy(path).into_owned(),
            status,
            added,
            removed,
        });
    }
    Ok(files)
}

fn number(field: &[u8]) -> Option<u64> {
    std::str::from_utf8(field).ok()?.parse().ok()
}

/// `git diff` between two trees, cut after `max` bytes with a note.
pub fn patch(dir: &Path, from: &str, to: &str, max: usize) -> Result<String> {
    let mut child = Command::new("git")
        .args([
            "--no-pager",
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            from,
            to,
        ])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Start Git")?;
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .context("No Git output")?
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > max;
    if truncated {
        let _ = child.kill();
    }
    let mut error = String::new();
    child
        .stderr
        .take()
        .context("No Git output")?
        .read_to_string(&mut error)?;
    let status = child.wait()?;
    ensure!(truncated || status.success(), "git diff: {}", error.trim());
    bytes.truncate(max);
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        text.push_str(&format!(
            "\n[Patch truncated after {max} bytes. Full patch: git diff {from} {to}]\n"
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary repository with one commit, or None when the host has no
    /// Git identity: tests never invent commit authorship.
    fn repository() -> Option<tempfile::TempDir> {
        let identity = |key| {
            let output = Command::new("git")
                .args(["config", "--get", key])
                .output()
                .ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        };
        let (Some(name), Some(email)) = (identity("user.name"), identity("user.email")) else {
            eprintln!("Skipping Git fixture: configure the human git identity first");
            return None;
        };
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            git(dir.path(), None, args).unwrap();
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", &name]);
        run(&["config", "user.email", &email]);
        fs::write(dir.path().join("tracked.txt"), "one\ntwo\n").unwrap();
        fs::write(dir.path().join("doomed.txt"), "bye\n").unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.log\n").unwrap();
        run(&["add", "."]);
        run(&[
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "Test fixture",
        ]);
        Some(dir)
    }

    /// A run directory inside the self-ignored metadata, as the router lays it out.
    fn run_dir(workspace: &Path) -> PathBuf {
        let dir = workspace.join(".model-router/runs/fixture");
        fs::create_dir_all(&dir).unwrap();
        fs::write(workspace.join(".model-router/.gitignore"), "*\n").unwrap();
        dir
    }

    /// Sorted `git status` lines, without the optional index refresh that
    /// would let status itself rewrite the index.
    fn status(dir: &Path) -> Vec<String> {
        let output = Command::new("git")
            .args(["status", "--porcelain", "--untracked-files=all"])
            .env("GIT_OPTIONAL_LOCKS", "0")
            .current_dir(dir)
            .output()
            .unwrap();
        let mut lines: Vec<_> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect();
        lines.sort();
        lines
    }

    #[test]
    fn snapshots_capture_the_working_tree_without_touching_the_index() {
        let Some(dir) = repository() else { return };
        let workspace = dir.path();
        let run_dir = run_dir(workspace);
        let index = fs::read(workspace.join(".git/index")).unwrap();
        let head = git(workspace, None, &["rev-parse", "HEAD", "HEAD^{tree}"]).unwrap();
        let base = tree(workspace, &run_dir).unwrap();
        assert!(String::from_utf8_lossy(&head).ends_with(&format!("{base}\n")));

        fs::write(workspace.join("tracked.txt"), "one\n2\nthree\n").unwrap();
        fs::write(workspace.join("new.txt"), "fresh\n").unwrap();
        fs::write(workspace.join("image.bin"), [0u8, 159, 146, 150]).unwrap();
        fs::write(workspace.join("ignored.log"), "noise\n").unwrap();
        fs::remove_file(workspace.join("doomed.txt")).unwrap();
        let after = tree(workspace, &run_dir).unwrap();
        let files = changes(workspace, &base, &after).unwrap();
        let summary: Vec<_> = files
            .iter()
            .map(|f| (f.path.as_str(), f.status, f.added, f.removed))
            .collect();
        assert_eq!(
            summary,
            [
                ("doomed.txt", ChangeKind::Deleted, Some(0), Some(1)),
                ("image.bin", ChangeKind::Added, None, None),
                ("new.txt", ChangeKind::Added, Some(1), Some(0)),
                ("tracked.txt", ChangeKind::Modified, Some(2), Some(1)),
            ]
        );
        let text = patch(workspace, &base, &after, PATCH_LIMIT).unwrap();
        assert!(text.contains("+fresh") && text.contains("-two") && !text.contains("noise"));
        let cut = patch(workspace, &base, &after, 40).unwrap();
        assert!(cut.contains("[Patch truncated after 40 bytes"), "{cut}");
        assert!(changes(workspace, &after, &after).unwrap().is_empty());

        // The real index, HEAD, and status are exactly as the user left them.
        assert_eq!(fs::read(workspace.join(".git/index")).unwrap(), index);
        assert_eq!(
            git(workspace, None, &["rev-parse", "HEAD", "HEAD^{tree}"]).unwrap(),
            head
        );
        assert_eq!(
            status(workspace),
            [
                " D doomed.txt",
                " M tracked.txt",
                "?? image.bin",
                "?? new.txt"
            ]
        );
        assert!(run_dir.join("snapshot.index").is_file());
    }

    #[test]
    fn unborn_head_starts_from_an_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), None, &["init", "-q"]).unwrap();
        let run_dir = run_dir(dir.path());
        fs::write(dir.path().join("first.txt"), "hello\n").unwrap();
        let first = tree(dir.path(), &run_dir).unwrap();
        fs::write(dir.path().join("second.txt"), "world\n").unwrap();
        let second = tree(dir.path(), &run_dir).unwrap();
        let files = changes(dir.path(), &first, &second).unwrap();
        assert_eq!(
            files
                .iter()
                .map(|f| (f.path.as_str(), f.status))
                .collect::<Vec<_>>(),
            [("second.txt", ChangeKind::Added)]
        );
        assert!(!dir.path().join(".git/index").exists());
        assert_eq!(status(dir.path()), ["?? first.txt", "?? second.txt"]);
    }

    #[test]
    fn refs_keep_snapshots_through_gc_until_the_run_is_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), None, &["init", "-q"]).unwrap();
        let run_dir = run_dir(dir.path());
        fs::write(dir.path().join("kept.txt"), "kept\n").unwrap();
        let kept = tree(dir.path(), &run_dir).unwrap();
        let exists = |object: &str| git(dir.path(), None, &["cat-file", "-e", object]).is_ok();
        git(dir.path(), None, &["gc", "-q", "--prune=now"]).unwrap();
        assert!(exists(&kept), "gc pruned a kept snapshot");
        let refs = git(dir.path(), None, &["for-each-ref", "refs/model-router/"]).unwrap();
        assert!(String::from_utf8_lossy(&refs).contains("refs/model-router/runs/fixture/"));
        forget(dir.path(), "fixture");
        let refs = git(dir.path(), None, &["for-each-ref", "refs/model-router/"]).unwrap();
        assert!(refs.is_empty());
        git(dir.path(), None, &["gc", "-q", "--prune=now"]).unwrap();
        assert!(!exists(&kept));
    }

    #[test]
    fn non_git_workspaces_have_no_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file.txt"), "plain").unwrap();
        assert!(!is_repository(dir.path()));
        assert_eq!(tree(dir.path(), dir.path()), None);
        assert!(!dir.path().join("snapshot.index").exists());
    }
}
