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
    process::Stdio,
};

fn git_raw(dir: &Path, args: &[&str]) -> Result<String> {
    let mut command = crate::snapshot::git_command();
    command.args(args).current_dir(dir).stdin(Stdio::null());
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

/// Runs Git with a caller-held sanitized Git directory, so several steps
/// share one fixed config.
fn git_in(
    sanitized: &snapshot::SanitizedGitDir,
    dir: &Path,
    index: Option<&Path>,
    args: &[&str],
) -> Result<String> {
    let mut command = sanitized.command(dir);
    command.args(args).stdin(Stdio::null());
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
    git_raw(workspace, &["rev-parse", "--show-prefix"]).unwrap_or_default()
}

/// Commits snapshot `tree` (parent HEAD, if any) under `reference`, so a
/// worktree can check it out. Returns the commit.
pub fn base_commit(workspace: &Path, tree: &str, reference: &str) -> Result<String> {
    let mut command = crate::snapshot::git_command();
    command
        .args(["commit-tree", tree, "-m", "model-router fan-out base"])
        .current_dir(workspace)
        .stdin(Stdio::null())
        // A private object: never the user's identity or signing setup.
        .env("GIT_AUTHOR_NAME", "model-router")
        .env("GIT_AUTHOR_EMAIL", "model-router@localhost")
        .env("GIT_COMMITTER_NAME", "model-router")
        .env("GIT_COMMITTER_EMAIL", "model-router@localhost");
    if let Ok(head) = git_raw(workspace, &["rev-parse", "--verify", "--quiet", "HEAD"]) {
        command.args(["-p", &head]);
    }
    let output = command.output().context("Start Git")?;
    ensure!(
        output.status.success(),
        "git commit-tree: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    git_raw(workspace, &["update-ref", reference, &commit])?;
    Ok(commit)
}

/// A detached worktree at `commit`. Repository hooks and local smudge filters
/// do not run; worktrees requiring a suppressed local smudge filter are refused.
pub fn add_worktree(workspace: &Path, path: &Path, commit: &str) -> Result<()> {
    let resolved = git_raw(
        workspace,
        &["rev-parse", "--verify", &format!("{commit}^{{commit}}")],
    )?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let path_str = path.to_str().context("Worktree path must be UTF-8")?;
    git_raw(
        workspace,
        &[
            "worktree",
            "add",
            "--no-checkout",
            "--detach",
            path_str,
            &resolved,
        ],
    )?;
    if let Err(err) = snapshot::populate_worktree(path, &resolved) {
        storage::discard_worktree(workspace, path);
        return Err(err);
    }
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
    // Each tree's own record, removed below if Git left it: never
    // `git worktree prune`, which also drops the records of other worktrees
    // whose directories are missing for now.
    let mut records = vec![];
    for tree in trees {
        records.extend(storage::worktree_record(&tree).ok());
        if let Some(path) = tree.to_str() {
            let _ = git_raw(project, &["worktree", "remove", "--force", path]);
        }
    }
    fs::remove_dir_all(dir).with_context(|| format!("Remove {}", dir.display()))?;
    for record in records {
        let _ = fs::remove_dir_all(record);
    }
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
    // The candidate could write its attempt directory.
    let file = storage::create_fresh(path)?;
    let sanitized = crate::snapshot::SanitizedGitDir::new(dir)?;
    let output = sanitized
        .command(dir)
        .args([
            "--no-pager",
            "diff",
            "--binary",
            "--full-index",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            // Whatever diff.noprefix or diff.*Prefix say, `git apply` expects these.
            "--src-prefix=a/",
            "--dst-prefix=b/",
            from,
            to,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::piped())
        .spawn()
        .context("Start Git")?
        .wait_with_output()
        .context("Wait for Git")?;
    if !output.status.success() {
        let _ = fs::remove_file(path);
        anyhow::bail!(
            "git diff {from} {to} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// An apply the router refused before it wrote any file.
#[derive(Debug)]
pub struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

fn refused(err: anyhow::Error) -> anyhow::Error {
    Refused(format!("{err:#}")).into()
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
/// An error that is a [`Refused`] left the workspace untouched.
pub fn apply(
    workspace: &Path,
    index: &Path,
    current: &str,
    files: &[String],
    patch: &Path,
    scratch: &Path,
) -> Result<Applied> {
    let top = PathBuf::from(git_raw(workspace, &["rev-parse", "--show-toplevel"])?);
    let patch = std::path::absolute(patch)?;
    let patch = patch.to_str().context("Patch path must be UTF-8")?;
    if fs::metadata(patch)?.len() == 0 {
        return Ok(Applied::Clean);
    }
    // One fixed config for the filter check, the applies, and the restore.
    let sanitized = snapshot::SanitizedGitDir::new(&top).map_err(refused)?;
    let paths = patch_paths(&sanitized, &top, files, patch).map_err(refused)?;
    refuse_unfiltered_apply(&sanitized, &top, index, &paths, patch, scratch).map_err(refused)?;
    // Plain apply checks the whole patch before writing any file.
    if git_in(&sanitized, &top, None, &["apply", "--binary", patch]).is_ok() {
        return Ok(Applied::Clean);
    }
    refuse_local_merge_driver(&sanitized, &top, index, &paths).map_err(refused)?;
    fs::create_dir_all(scratch)?;
    let check_index = std::path::absolute(scratch.join("apply-check.index"))?;
    fs::copy(index, &check_index).context("Copy the snapshot index")?;
    let precheck = git_in(
        &sanitized,
        &top,
        Some(&check_index),
        &["apply", "--3way", "--cached", "--binary", patch],
    );
    let _ = fs::remove_file(&check_index);
    if let Err(err) = precheck {
        let msg = err.to_string();
        let unsupported = msg.contains("cannot be used")
            || msg.contains("unknown option")
            || msg.contains("usage:");
        if !unsupported {
            return Ok(Applied::Conflict(files.to_vec()));
        }
        // This Git cannot merge in an index only, so the filter check above
        // never saw the merged result's attribute rules.
        if !sanitized.local_smudge_filters().is_empty() {
            return Err(refused(anyhow::anyhow!(
                "Refusing a three-way apply: this Git lacks `git apply --3way --cached` (Git 2.32 or later), so the merged result cannot be checked for repo-local smudge filters, which are disabled for safety. Apply the patch by hand"
            )));
        }
    }
    let merge = std::path::absolute(scratch.join("apply.index"))?;
    fs::copy(index, &merge).context("Copy the snapshot index")?;
    if git_in(
        &sanitized,
        &top,
        Some(&merge),
        &["apply", "--3way", "--binary", patch],
    )
    .is_ok()
    {
        let _ = fs::remove_file(&merge);
        return Ok(Applied::Merged);
    }
    let _ = fs::remove_file(&merge);
    restore(&sanitized, &top, current, files, scratch)?;
    Ok(Applied::Conflict(files.to_vec()))
}

/// A three-way apply merges with Git's built-in drivers, since the sanitized
/// config leaves out repo-local `merge.<name>.driver` commands and
/// `merge.default`: refused when the repository's config would merge a
/// patched file with one, whose result or conflict could differ. The local
/// driver never runs.
fn refuse_local_merge_driver(
    sanitized: &snapshot::SanitizedGitDir,
    top: &Path,
    index: &Path,
    paths: &[u8],
) -> Result<()> {
    if let Some((path, driver)) =
        snapshot::find_local_merge_driver_path(sanitized, top, paths, Some(index))?
    {
        anyhow::bail!(
            "Refusing a three-way apply: {path} uses repo-local Git merge driver `{driver}`, which is disabled for safety, and the built-in merge could give a different result. Configure the driver globally or apply the patch by hand"
        );
    }
    Ok(())
}

/// The patch's paths, NUL-delimited: `files`, and the patch's own as raw
/// bytes. Empty when no repo-local driver needs an attribute check.
fn patch_paths(
    sanitized: &snapshot::SanitizedGitDir,
    top: &Path,
    files: &[String],
    patch: &str,
) -> Result<Vec<u8>> {
    if sanitized.local_smudge_filters().is_empty()
        && sanitized.local_merge_drivers().is_empty()
        && sanitized.local_merge_default().is_none()
    {
        return Ok(Vec::new());
    }
    let mut stdin_paths = Vec::new();
    for file in files {
        stdin_paths.extend_from_slice(file.as_bytes());
        stdin_paths.push(0);
    }
    // The patch's own paths as raw bytes too: `files` is lossy for names
    // that are not UTF-8. Records are `<added>\t<deleted>\t<path>`.
    let numstat = sanitized
        .command(top)
        .args(["apply", "--numstat", "-z", patch])
        .stdin(Stdio::null())
        .output()
        .context("Start Git")?;
    ensure!(
        numstat.status.success(),
        "git apply --numstat: {}",
        String::from_utf8_lossy(&numstat.stderr).trim()
    );
    for record in numstat.stdout.split(|&b| b == 0) {
        if let Some(path) = record.splitn(3, |&b| b == b'\t').nth(2) {
            if !path.is_empty() {
                stdin_paths.extend_from_slice(path);
                stdin_paths.push(0);
            }
        }
    }
    Ok(stdin_paths)
}

/// An apply, and the restore after a failed three-way one, write files from
/// Git's stored bytes without the smudge filters the router disables:
/// refused when a patched file (`stdin_paths`, from `patch_paths`) uses one.
fn refuse_unfiltered_apply(
    sanitized: &snapshot::SanitizedGitDir,
    top: &Path,
    index: &Path,
    stdin_paths: &[u8],
    patch: &str,
    scratch: &Path,
) -> Result<()> {
    let drivers = sanitized.local_smudge_filters();
    if drivers.is_empty() {
        return Ok(());
    }
    let mut found =
        snapshot::find_suppressed_filter_path(sanitized, top, stdin_paths, Some(index), drivers)?;
    if found.is_none() {
        // The patch may add the `.gitattributes` rule itself: check again
        // with the rules of the proposed result, in a copy of the index.
        fs::create_dir_all(scratch)?;
        let proposed = std::path::absolute(scratch.join("filter-check.index"))?;
        let mut applied = false;
        for mode in [
            &["apply", "--cached", "--binary"][..],
            &["apply", "--3way", "--cached", "--binary"],
        ] {
            fs::copy(index, &proposed).context("Copy the snapshot index")?;
            let args: Vec<&str> = mode.iter().copied().chain([patch]).collect();
            if git_in(sanitized, top, Some(&proposed), &args).is_ok() {
                applied = true;
                break;
            }
        }
        let checked = if applied {
            snapshot::find_suppressed_filter_path_cached(
                sanitized,
                top,
                stdin_paths,
                &proposed,
                drivers,
            )
        } else {
            Ok(None)
        };
        let _ = fs::remove_file(&proposed);
        found = checked?;
    }
    if let Some((path, driver)) = found {
        anyhow::bail!(
            "Refusing to apply the patch: {path} uses repo-local Git smudge filter `{driver}`, which is disabled for safety, and applying or restoring it would write unfiltered contents. Configure the filter globally or apply the patch by hand"
        );
    }
    Ok(())
}

/// What `apply` would do, without touching the workspace: a plain apply
/// check, then the three-way merge in a copy of the snapshot index only
/// (`git apply --3way --cached`, Git 2.32 or later; an older Git reports a
/// conflict where a merge might have worked).
pub fn check(
    workspace: &Path,
    index: &Path,
    files: &[String],
    patch: &Path,
    scratch: &Path,
) -> Result<Applied> {
    let top = PathBuf::from(git_raw(workspace, &["rev-parse", "--show-toplevel"])?);
    let patch = std::path::absolute(patch)?;
    let patch = patch.to_str().context("Patch path must be UTF-8")?;
    if fs::metadata(patch)?.len() == 0 {
        return Ok(Applied::Clean);
    }
    let sanitized = snapshot::SanitizedGitDir::new(&top)?;
    let paths = patch_paths(&sanitized, &top, files, patch)?;
    refuse_unfiltered_apply(&sanitized, &top, index, &paths, patch, scratch)?;
    if git_in(
        &sanitized,
        &top,
        None,
        &["apply", "--check", "--binary", patch],
    )
    .is_ok()
    {
        return Ok(Applied::Clean);
    }
    refuse_local_merge_driver(&sanitized, &top, index, &paths)?;
    fs::create_dir_all(scratch)?;
    let merge = std::path::absolute(scratch.join("check.index"))?;
    fs::copy(index, &merge).context("Copy the snapshot index")?;
    let merged = git_in(
        &sanitized,
        &top,
        Some(&merge),
        &["apply", "--3way", "--cached", "--binary", patch],
    )
    .is_ok();
    let _ = fs::remove_file(&merge);
    Ok(if merged {
        Applied::Merged
    } else {
        Applied::Conflict(files.to_vec())
    })
}

pub(crate) fn is_plain_repo_path(file: &str) -> bool {
    !file.is_empty()
        && Path::new(file).components().all(|c| match c {
            std::path::Component::Normal(part) => {
                let part = part.to_string_lossy();
                !part.eq_ignore_ascii_case(".git") && !part.eq_ignore_ascii_case(".model-router")
            }
            _ => false,
        })
}

/// Puts `files` back as they are in tree `tree`: present ones checked out,
/// absent ones removed.
fn restore(
    sanitized: &snapshot::SanitizedGitDir,
    top: &Path,
    tree: &str,
    files: &[String],
    scratch: &Path,
) -> Result<()> {
    let index = std::path::absolute(scratch.join("restore.index"))?;
    let _ = fs::remove_file(&index);
    let plain_files: Vec<&str> = files
        .iter()
        .map(String::as_str)
        .filter(|file| is_plain_repo_path(file))
        .collect();
    if plain_files.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(scratch)?;
    let read_out = sanitized
        .command(top)
        .args(["read-tree", tree])
        .env("GIT_INDEX_FILE", &index)
        .stdin(Stdio::null())
        .output()
        .context("Start Git")?;
    ensure!(
        read_out.status.success(),
        "git read-tree {tree}: {}",
        String::from_utf8_lossy(&read_out.stderr).trim()
    );
    let mut present = std::collections::HashSet::new();
    for chunk in plain_files.chunks(512) {
        let mut command = sanitized.command(top);
        command
            .arg("--literal-pathspecs")
            .args(["ls-files", "-z", "--"])
            .args(chunk)
            .env("GIT_INDEX_FILE", &index)
            .stdin(Stdio::null());
        let output = command.output().context("Start Git")?;
        ensure!(
            output.status.success(),
            "git ls-files: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        for name in output.stdout.split(|&b| b == 0) {
            if !name.is_empty() {
                present.insert(String::from_utf8_lossy(name).into_owned());
            }
        }
    }
    let (keep, gone): (Vec<&str>, Vec<&str>) = plain_files
        .into_iter()
        .partition(|file| present.contains(*file));
    if !keep.is_empty() {
        let mut stdin_bytes = Vec::new();
        for file in &keep {
            stdin_bytes.extend_from_slice(file.as_bytes());
            stdin_bytes.push(0);
        }
        let mut child = sanitized
            .command(top)
            .args(["checkout-index", "-f", "-z", "--stdin"])
            .env("GIT_INDEX_FILE", &index)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("Start Git")?;
        let mut pipe = child.stdin.take();
        let output = std::thread::scope(|scope| -> Result<std::process::Output> {
            let writer = scope.spawn(move || -> std::io::Result<()> {
                if let Some(mut pipe) = pipe.take() {
                    std::io::Write::write_all(&mut pipe, &stdin_bytes)?;
                }
                Ok(())
            });
            let output = child.wait_with_output().context("Wait for Git")?;
            let _ = writer.join();
            Ok(output)
        })?;
        ensure!(
            output.status.success(),
            "git checkout-index: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let root = fs::canonicalize(top)?;
    for file in gone {
        let path = top.join(file);
        // The names come from the saved run. A tree's file is a plain path
        // inside the repository that crosses no link (Git stores links as
        // files), and never Git's or the router's own, which no tree diff
        // names; anything else is left alone.
        let name = Path::new(file);
        let dir = name.parent().unwrap_or(Path::new(""));
        let inside = fs::canonicalize(top.join(dir)).is_ok_and(|parent| parent == root.join(dir));
        if inside && fs::symlink_metadata(&path).is_ok() {
            fs::remove_file(&path).with_context(|| format!("Remove {}", path.display()))?;
            let mut dir_path = path.parent();
            while let Some(dir) = dir_path {
                if dir == top || !dir.starts_with(top) {
                    break;
                }
                if fs::remove_dir(dir).is_err() {
                    break;
                }
                dir_path = dir.parent();
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, index: Option<&Path>, args: &[&str]) -> Result<String> {
        let mut command = crate::snapshot::git_command();
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

    #[test]
    fn a_patch_never_writes_through_a_link_the_candidate_left() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, None, &["init", "-q"]).unwrap();
        fs::write(repo.join("a"), "one\n").unwrap();
        let index = dir.path().join("index");
        let tree = |index: &Path| {
            git(&repo, Some(index), &["add", "-A"]).unwrap();
            git(&repo, Some(index), &["write-tree"]).unwrap()
        };
        let from = tree(&index);
        fs::write(repo.join("a"), "two\n").unwrap();
        let to = tree(&index);
        let victim = dir.path().join("victim");
        fs::write(&victim, "keep").unwrap();
        let patch = dir.path().join("changes.diff");
        std::os::unix::fs::symlink(&victim, &patch).unwrap();
        write_patch(&repo, &from, &to, &patch).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        assert!(fs::read_to_string(&patch).unwrap().contains("+two"));
    }

    #[test]
    fn a_conflict_removes_only_files_inside_the_repository() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, None, &["init", "-q"]).unwrap();
        let index = dir.path().join("index");
        let tree = || {
            git(&repo, Some(&index), &["add", "-A"]).unwrap();
            git(&repo, Some(&index), &["write-tree"]).unwrap()
        };
        fs::write(repo.join("a"), "one\n").unwrap();
        let from = tree();
        fs::write(repo.join("a"), "two\n").unwrap();
        let to = tree();
        let patch = dir.path().join("changes.diff");
        write_patch(&repo, &from, &to, &patch).unwrap();
        // The workspace moved on, so the patch conflicts; a stage left a link
        // to a directory elsewhere, which the saved file names go through.
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("victim"), "keep").unwrap();
        fs::write(repo.join("a"), "three\n").unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("link")).unwrap();
        let current = tree();
        let scratch = dir.path().join("scratch");
        for files in [
            vec!["a".to_owned(), "link/victim".to_owned()],
            vec![outside.join("victim").display().to_string()],
        ] {
            let _ = apply(&repo, &index, &current, &files, &patch, &scratch);
            assert_eq!(fs::read_to_string(outside.join("victim")).unwrap(), "keep");
        }
        // Nor Git's or the router's own files inside it.
        fs::create_dir(repo.join(".model-router")).unwrap();
        fs::write(repo.join(".model-router/run.json"), "{}").unwrap();
        // Also not through a link a stage left inside the repository.
        std::os::unix::fs::symlink(".git", repo.join("g")).unwrap();
        std::os::unix::fs::symlink(".model-router", repo.join("m")).unwrap();
        let files = [
            ".git/HEAD".to_owned(),
            ".model-router/run.json".to_owned(),
            "g/config".to_owned(),
            "m/run.json".to_owned(),
            ".MODEL-ROUTER/run.json".to_owned(),
        ];
        let _ = apply(&repo, &index, &current, &files, &patch, &scratch);
        assert!(repo.join(".git/HEAD").is_file());
        assert!(repo.join(".git/config").is_file());
        assert!(repo.join(".model-router/run.json").is_file());
        let files = ["a".to_owned(), "link/victim".to_owned()];
        assert!(matches!(
            apply(&repo, &index, &current, &files, &patch, &scratch).unwrap(),
            Applied::Conflict(_)
        ));
        assert_eq!(fs::read_to_string(repo.join("a")).unwrap(), "three\n");
    }

    #[test]
    fn write_patch_and_apply_ignore_diff_noprefix() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, None, &["init", "-q"]).unwrap();
        git(&repo, None, &["config", "diff.noprefix", "true"]).unwrap();
        fs::write(repo.join("a.txt"), "one\n").unwrap();
        let index = dir.path().join("index");
        let tree = || {
            git(&repo, Some(&index), &["add", "-A"]).unwrap();
            git(&repo, Some(&index), &["write-tree"]).unwrap()
        };
        let from = tree();
        fs::write(repo.join("a.txt"), "two\n").unwrap();
        let to = tree();
        let patch = dir.path().join("changes.diff");
        write_patch(&repo, &from, &to, &patch).unwrap();
        // Put the working tree back to `from`, then apply the generated patch.
        fs::write(repo.join("a.txt"), "one\n").unwrap();
        let current = tree();
        let scratch = dir.path().join("scratch");
        assert_eq!(
            apply(
                &repo,
                &index,
                &current,
                &["a.txt".to_owned()],
                &patch,
                &scratch
            )
            .unwrap(),
            Applied::Clean
        );
        assert_eq!(fs::read_to_string(repo.join("a.txt")).unwrap(), "two\n");
    }

    #[test]
    fn restore_reverts_tracked_prunes_new_dirs_and_skips_unsafe_paths() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let outside = dir.path().join("outside");
        fs::create_dir(&repo).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("victim"), "keep").unwrap();
        git(&repo, None, &["init", "-q"]).unwrap();
        fs::write(repo.join("tracked.txt"), "original\n").unwrap();
        let index = dir.path().join("index");
        git(&repo, Some(&index), &["add", "-A"]).unwrap();
        let baseline = git(&repo, Some(&index), &["write-tree"]).unwrap();

        // Simulate partial mutations left by a failed apply.
        fs::write(repo.join("tracked.txt"), "mutated\n").unwrap();
        fs::create_dir_all(repo.join("newdir/nested")).unwrap();
        fs::write(repo.join("newdir/nested/extra.txt"), "temp\n").unwrap();
        fs::create_dir_all(repo.join(".model-router")).unwrap();
        fs::write(repo.join(".model-router/run.json"), "{}").unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("link")).unwrap();

        let files = vec![
            "tracked.txt".to_owned(),
            "newdir/nested/extra.txt".to_owned(),
            "link/victim".to_owned(),
            outside.join("victim").display().to_string(),
            ".git/HEAD".to_owned(),
            ".model-router/run.json".to_owned(),
        ];
        let scratch = dir.path().join("scratch");
        restore(
            &snapshot::SanitizedGitDir::new(&repo).unwrap(),
            &repo,
            &baseline,
            &files,
            &scratch,
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(repo.join("tracked.txt")).unwrap(),
            "original\n"
        );
        assert!(!repo.join("newdir/nested/extra.txt").exists());
        assert!(
            !repo.join("newdir").exists(),
            "empty parent directories must be pruned"
        );
        assert_eq!(fs::read_to_string(outside.join("victim")).unwrap(), "keep");
        assert!(repo.join(".git/HEAD").is_file());
        assert!(repo.join(".model-router/run.json").is_file());
    }

    #[test]
    fn a_three_way_apply_refuses_a_repo_local_merge_driver_without_running_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, None, &["init", "-q"]).unwrap();
        let marker = dir.path().join("driver-ran");
        let driver = format!("touch '{}'; exit 1", marker.display());
        git(&repo, None, &["config", "merge.custom.driver", &driver]).unwrap();
        fs::write(repo.join(".gitattributes"), "a merge=custom\n").unwrap();
        let index = dir.path().join("index");
        let tree = || {
            git(&repo, Some(&index), &["add", "-A"]).unwrap();
            git(&repo, Some(&index), &["write-tree"]).unwrap()
        };
        fs::write(repo.join("a"), "one\ntwo\nthree\n").unwrap();
        let from = tree();
        fs::write(repo.join("a"), "ONE\ntwo\nthree\n").unwrap();
        let to = tree();
        let patch = dir.path().join("changes.diff");
        write_patch(&repo, &from, &to, &patch).unwrap();
        // The workspace changed another line: only a three-way merge applies.
        fs::write(repo.join("a"), "one\ntwo\nTHREE\n").unwrap();
        let current = tree();
        let scratch = dir.path().join("scratch");
        let files = ["a".to_owned()];

        let err = apply(&repo, &index, &current, &files, &patch, &scratch).unwrap_err();
        assert!(err.downcast_ref::<Refused>().is_some(), "{err:#}");
        assert!(
            format!("{err:#}").contains("merge driver `custom`"),
            "{err:#}"
        );
        let err = check(&repo, &index, &files, &patch, &scratch).unwrap_err();
        assert!(
            format!("{err:#}").contains("merge driver `custom`"),
            "{err:#}"
        );
        assert_eq!(
            fs::read_to_string(repo.join("a")).unwrap(),
            "one\ntwo\nTHREE\n"
        );
        assert!(!marker.exists(), "the repo-local merge driver ran");

        // A repo-local merge.default covers files with no merge attribute.
        fs::write(repo.join(".gitattributes"), "").unwrap();
        let current = tree();
        git(&repo, None, &["config", "merge.default", "custom"]).unwrap();
        let err = apply(&repo, &index, &current, &files, &patch, &scratch).unwrap_err();
        assert!(err.downcast_ref::<Refused>().is_some(), "{err:#}");
        assert!(!marker.exists(), "the repo-local merge driver ran");

        // Without them the built-in merge applies.
        git(&repo, None, &["config", "--unset", "merge.default"]).unwrap();
        assert_eq!(
            check(&repo, &index, &files, &patch, &scratch).unwrap(),
            Applied::Merged
        );
        assert_eq!(
            apply(&repo, &index, &current, &files, &patch, &scratch).unwrap(),
            Applied::Merged
        );
        assert_eq!(
            fs::read_to_string(repo.join("a")).unwrap(),
            "ONE\ntwo\nTHREE\n"
        );
        assert!(!marker.exists(), "the repo-local merge driver ran");
    }
}
