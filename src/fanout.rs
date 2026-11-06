//! Git plumbing for best-of-N stages. Candidates start from the workspace's
//! current state (HEAD plus uncommitted, non-ignored changes): a snapshot
//! tree committed on a private ref, `refs/model-router/runs/<run>/base/…`,
//! checked out as a detached worktree under `.model-router/candidates/`.
//! User branches, HEAD, and the user's index never change. The chosen
//! candidate's changes reach the workspace as a patch; a patch that does not
//! apply leaves the workspace as it was.
use crate::{snapshot, storage};
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

fn git(dir: &Path, index: Option<&Path>, args: &[&str]) -> Result<String> {
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
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned())
}

/// Where a run's candidate worktrees live: `<project>/.model-router/candidates/<run>`.
pub fn candidates_dir(project: &Path, run: &str) -> Result<PathBuf> {
    Ok(storage::metadata_dir(project)?.join("candidates").join(run))
}

/// The workspace's path inside its repository, e.g. `app/` for a project in
/// a subdirectory; empty at the top.
pub fn prefix(workspace: &Path) -> String {
    git(workspace, None, &["rev-parse", "--show-prefix"]).unwrap_or_default()
}

/// Commits snapshot `tree` (parent HEAD, if any) under `reference`, so a
/// worktree can check it out. Returns the commit.
pub fn base_commit(workspace: &Path, tree: &str, reference: &str) -> Result<String> {
    let mut command = Command::new("git");
    command
        .args(["commit-tree", tree, "-m", "model-router fan-out base"])
        .current_dir(workspace)
        .stdin(Stdio::null())
        // A private object: never the user's identity or signing setup.
        .env("GIT_AUTHOR_NAME", "model-router")
        .env("GIT_AUTHOR_EMAIL", "model-router@localhost")
        .env("GIT_COMMITTER_NAME", "model-router")
        .env("GIT_COMMITTER_EMAIL", "model-router@localhost");
    if let Ok(head) = git(
        workspace,
        None,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
    ) {
        command.args(["-p", &head]);
    }
    let output = command.output().context("Start Git")?;
    ensure!(
        output.status.success(),
        "git commit-tree: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    git(workspace, None, &["update-ref", reference, &commit])?;
    Ok(commit)
}

/// A detached worktree at `commit`. Repository hooks do not run.
pub fn add_worktree(workspace: &Path, path: &Path, commit: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    git(
        workspace,
        None,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "worktree",
            "add",
            "--detach",
            path.to_str().context("Worktree path must be UTF-8")?,
            commit,
        ],
    )?;
    Ok(())
}

/// Removes every candidate worktree under `dir`, with force: they are the
/// router's own and their changes were offered as patches. Then `dir`.
pub fn remove_worktrees(project: &Path, dir: &Path) -> Result<()> {
    if fs::symlink_metadata(dir).is_err() {
        return Ok(());
    }
    let mut trees = vec![];
    collect(dir, &mut trees);
    for tree in trees {
        if let Some(path) = tree.to_str() {
            let _ = git(project, None, &["worktree", "remove", "--force", path]);
        }
    }
    fs::remove_dir_all(dir).with_context(|| format!("Remove {}", dir.display()))?;
    let _ = git(project, None, &["worktree", "prune"]);
    Ok(())
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    if dir.join(".git").is_file() {
        out.push(dir.to_path_buf());
        return;
    }
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            collect(&entry.path(), out);
        }
    }
}

/// Writes the full binary patch from tree `from` to tree `to`.
pub fn write_patch(dir: &Path, from: &str, to: &str, path: &Path) -> Result<()> {
    let file = fs::File::create(path).with_context(|| format!("Create {}", path.display()))?;
    let status = Command::new("git")
        .args([
            "--no-pager",
            "diff",
            "--binary",
            "--full-index",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            from,
            to,
        ])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(Stdio::null())
        .status()
        .context("Start Git")?;
    ensure!(status.success(), "git diff {from} {to} failed");
    Ok(())
}

/// How a chosen candidate's patch reached the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// It applied as is.
    Clean,
    /// It needed a three-way merge with workspace changes, which succeeded.
    Merged,
    /// It conflicted with these files; the workspace was left unchanged.
    Conflict(Vec<String>),
}

/// Applies `patch` (made from `base`) to the workspace, whose current
/// snapshot is `current` in private index `index`. Tries a plain apply,
/// then a three-way merge in a copy of that index; on conflict, restores
/// the patch's files from `current`. `scratch` holds temporary indexes.
pub fn apply(
    workspace: &Path,
    index: &Path,
    current: &str,
    files: &[String],
    patch: &Path,
    scratch: &Path,
) -> Result<Applied> {
    let top = PathBuf::from(git(workspace, None, &["rev-parse", "--show-toplevel"])?);
    let patch = std::path::absolute(patch)?;
    let patch = patch.to_str().context("Patch path must be UTF-8")?;
    if fs::metadata(patch)?.len() == 0 {
        return Ok(Applied::Clean);
    }
    // Plain apply is atomic and leaves every index alone.
    if git(&top, None, &["apply", "--check", "--binary", patch]).is_ok() {
        git(&top, None, &["apply", "--binary", patch])?;
        return Ok(Applied::Clean);
    }
    fs::create_dir_all(scratch)?;
    let merge = std::path::absolute(scratch.join("apply.index"))?;
    fs::copy(index, &merge).context("Copy the snapshot index")?;
    if git(&top, Some(&merge), &["apply", "--3way", "--binary", patch]).is_ok() {
        let _ = fs::remove_file(&merge);
        return Ok(Applied::Merged);
    }
    let _ = fs::remove_file(&merge);
    restore(&top, current, files, scratch)?;
    Ok(Applied::Conflict(files.to_vec()))
}

/// Puts `files` back as they are in tree `tree`: present ones checked out,
/// absent ones removed.
fn restore(top: &Path, tree: &str, files: &[String], scratch: &Path) -> Result<()> {
    let index = std::path::absolute(scratch.join("restore.index"))?;
    let _ = fs::remove_file(&index);
    git(top, Some(&index), &["read-tree", tree])?;
    let listed = git(top, Some(&index), &["ls-files", "-z"])?;
    let present: std::collections::HashSet<&str> = listed.split('\0').collect();
    let (keep, gone): (Vec<&String>, Vec<&String>) =
        files.iter().partition(|f| present.contains(f.as_str()));
    if !keep.is_empty() {
        let mut args = vec!["checkout-index", "-f", "--"];
        args.extend(keep.iter().map(|f| f.as_str()));
        git(top, Some(&index), &args)?;
    }
    for file in gone {
        let path = top.join(file);
        if fs::symlink_metadata(&path).is_ok() {
            fs::remove_file(&path).with_context(|| format!("Remove {}", path.display()))?;
        }
    }
    let _ = fs::remove_file(&index);
    Ok(())
}

/// Changed files and line counts from `from` to `to`.
pub fn stat(dir: &Path, from: &str, to: &str) -> Result<(Vec<String>, u64, u64)> {
    let files = snapshot::changes(dir, from, to)?;
    let added = files.iter().filter_map(|f| f.added).sum();
    let removed = files.iter().filter_map(|f| f.removed).sum();
    Ok((files.into_iter().map(|f| f.path).collect(), added, removed))
}
