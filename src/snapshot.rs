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
    io::{BufRead, Read},
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

/// The repository's top-level directory for `dir`.
pub(crate) fn top_level(dir: &Path) -> Result<PathBuf> {
    let out = git(dir, None, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(String::from_utf8_lossy(&out).trim()))
}

/// `git` for the router's own commands in a workspace or checkout. A stage
/// may have written the repository's config or hooks before the router
/// runs Git there, outside the stage's sandbox: hooks and an fsmonitor
/// command never run. (Sandboxed stages cannot write Git config or hooks;
/// see `sandbox::git_locks`.)
pub(crate) fn git_command() -> Command {
    let mut command = Command::new("git");
    command.args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
    ]);
    command
}

/// `git_command` in `dir`, additionally disabling any repository-local or
/// worktree-local `filter.*`, `diff.*`, or `merge.*.driver` executable config
/// keys so commands like `git status`, `git add -A`, `git worktree add`, and
/// `git apply --3way` never execute repo-defined commands. Fails closed when
/// config scope cannot be queried or a key cannot be safely overridden.
pub(crate) fn git_command_in(dir: &Path) -> Result<Command> {
    let mut command = git_command();
    let overrides = repo_driver_overrides(dir)?;
    if !overrides.is_empty() {
        let needs_env = overrides.iter().any(|(key, _)| key.contains(&b'='));
        ensure!(
            !needs_env || git_supports_config_env(),
            "Refusing Git command in {}: repository defines a driver key containing `=`, which requires Git 2.31+ to disable safely",
            dir.display()
        );
        // Pass via `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_<i>` / `GIT_CONFIG_VALUE_<i>`
        // with raw key bytes so non-UTF-8 subsection names and names containing
        // `=` are preserved exactly; also pass `-c` for keys without `=` so
        // Git 2.26-2.30 disables them too.
        let base_count = std::env::var("GIT_CONFIG_COUNT")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        for (offset, (key, value)) in overrides.iter().enumerate() {
            let idx = base_count + offset;
            let key_os = bytes_to_os(key)?;
            command
                .env(format!("GIT_CONFIG_KEY_{idx}"), &key_os)
                .env(format!("GIT_CONFIG_VALUE_{idx}"), value);
            if !key.contains(&b'=') {
                let mut arg = key_os;
                arg.push("=");
                arg.push(value);
                command.arg("-c").arg(arg);
            }
        }
        command.env(
            "GIT_CONFIG_COUNT",
            (base_count + overrides.len()).to_string(),
        );
    }
    command.current_dir(dir);
    Ok(command)
}

/// Raw driver names of repo-local/worktree `filter.<driver>.(clean|process|required)`
/// keys that `git_command_in` disables in `dir`.
pub(crate) fn repo_local_clean_filters(dir: &Path) -> Result<Vec<Vec<u8>>> {
    let mut drivers = Vec::new();
    for (key, _) in repo_driver_overrides(dir)? {
        let Some((section, rest)) = split_first(&key, b'.') else {
            continue;
        };
        let Some((subsection, var)) = split_last(rest, b'.') else {
            continue;
        };
        if section.eq_ignore_ascii_case(b"filter")
            && (var.eq_ignore_ascii_case(b"clean")
                || var.eq_ignore_ascii_case(b"process")
                || var.eq_ignore_ascii_case(b"required"))
            && !subsection.is_empty()
            && !drivers.iter().any(|d| d == subsection)
        {
            drivers.push(subsection.to_vec());
        }
    }
    Ok(drivers)
}

fn split_first(bytes: &[u8], sep: u8) -> Option<(&[u8], &[u8])> {
    let pos = bytes.iter().position(|&b| b == sep)?;
    Some((&bytes[..pos], &bytes[pos + 1..]))
}

fn split_last(bytes: &[u8], sep: u8) -> Option<(&[u8], &[u8])> {
    let pos = bytes.iter().rposition(|&b| b == sep)?;
    Some((&bytes[..pos], &bytes[pos + 1..]))
}

#[cfg(unix)]
fn bytes_to_os(bytes: &[u8]) -> Result<OsString> {
    use std::os::unix::ffi::OsStrExt;
    ensure!(!bytes.contains(&0), "Git config key contains NUL byte");
    Ok(std::ffi::OsStr::from_bytes(bytes).to_os_string())
}

#[cfg(not(unix))]
fn bytes_to_os(bytes: &[u8]) -> Result<OsString> {
    let text = std::str::from_utf8(bytes)
        .context("Git config key is not valid UTF-8 and cannot be safely overridden")?;
    ensure!(!text.contains('\0'), "Git config key contains NUL byte");
    Ok(OsString::from(text))
}

fn git_supports_config_env() -> bool {
    static SUPPORTS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTS.get_or_init(|| {
        let Ok(output) = git_command()
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        else {
            return false;
        };
        if !output.status.success() {
            return false;
        }
        parse_git_version(&String::from_utf8_lossy(&output.stdout))
            .is_some_and(|(major, minor)| (major, minor) >= (2, 31))
    })
}

fn parse_git_version(text: &str) -> Option<(u32, u32)> {
    let token = text
        .split_whitespace()
        .find(|part| part.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let mut nums = token.split('.');
    let major = nums.next()?.parse().ok()?;
    let minor = nums.next()?.parse().ok()?;
    Some((major, minor))
}

fn driver_override_value(key: &[u8]) -> Option<&'static str> {
    if key.eq_ignore_ascii_case(b"diff.external") {
        return Some("");
    }
    let (section, rest) = split_first(key, b'.')?;
    let (subsection, var) = split_last(rest, b'.')?;
    if subsection.is_empty() {
        return None;
    }
    if section.eq_ignore_ascii_case(b"filter") {
        if var.eq_ignore_ascii_case(b"clean")
            || var.eq_ignore_ascii_case(b"smudge")
            || var.eq_ignore_ascii_case(b"process")
        {
            return Some("");
        }
        if var.eq_ignore_ascii_case(b"required") {
            return Some("false");
        }
    } else if section.eq_ignore_ascii_case(b"diff") {
        if var.eq_ignore_ascii_case(b"textconv") || var.eq_ignore_ascii_case(b"command") {
            return Some("");
        }
    } else if section.eq_ignore_ascii_case(b"merge") && var.eq_ignore_ascii_case(b"driver") {
        return Some("");
    }
    None
}

fn repo_driver_overrides(dir: &Path) -> Result<Vec<(Vec<u8>, &'static str)>> {
    // `--list -z` avoids locale-dependent regexes (which skip non-UTF-8 keys)
    // and preserves exact key bytes, including newlines and non-UTF-8 bytes.
    let output = git_command()
        .args(["config", "--show-scope", "--name-only", "-z", "--list"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .context("Start Git")?;
    ensure!(
        output.status.success(),
        "Could not inspect Git config scopes in {} (requires Git 2.26+): {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let mut overrides = Vec::new();
    let mut parts = output.stdout.split(|&b| b == 0);
    while let (Some(scope), Some(key)) = (parts.next(), parts.next()) {
        if scope != b"local" && scope != b"worktree" {
            continue;
        }
        let Some(value) = driver_override_value(key) else {
            continue;
        };
        let entry = (key.to_vec(), value);
        if !overrides.contains(&entry) {
            overrides.push(entry);
        }
    }
    Ok(overrides)
}

fn git(dir: &Path, index: Option<&Path>, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = crate::snapshot::git_command_in(dir)?;
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

/// The working tree as a Git tree ID now, through caller-provided `index` and
/// without creating a ref. Ok(None) outside Git.
pub fn capture_index(workspace: &Path, index: &Path) -> Result<Option<String>> {
    capture(workspace, &std::path::absolute(index)?)
}

/// The working tree as a Git tree ID now, through a throwaway index and
/// without a ref: for comparing against saved snapshots. Writes only Git
/// objects. Ok(None) outside Git.
pub fn current_tree(workspace: &Path) -> Result<Option<String>> {
    let scratch = tempfile::tempdir().context("Create a scratch index directory")?;
    capture(workspace, &scratch.path().join("index"))
}

/// Whether `tree` names a tree object in `dir`'s repository.
pub fn exists(dir: &Path, tree: &str) -> bool {
    git(dir, None, &["cat-file", "-e", &format!("{tree}^{{tree}}")]).is_ok()
}

/// Makes the files of a fresh checkout `workspace` match snapshot `tree`
/// exactly, as uncommitted changes against its HEAD: the index, HEAD, and
/// refs stay as they are. Fails, saying why, when the result differs from
/// `tree` (e.g. attribute filters rewrote files).
pub fn materialize(workspace: &Path, tree: &str) -> Result<()> {
    let head = git(
        workspace,
        None,
        &["rev-parse", "--verify", "--quiet", "HEAD^{tree}"],
    )
    .map(|out| String::from_utf8_lossy(&out).trim().to_owned())
    .context("The checkout has no HEAD commit")?;
    if head != tree {
        let scratch = tempfile::tempdir().context("Create a scratch directory")?;
        let patch = scratch.path().join("snapshot.patch");
        let file = fs::File::create(&patch)?;
        let status = crate::snapshot::git_command()
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
                &head,
                tree,
            ])
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(file)
            .stderr(Stdio::null())
            .status()
            .context("Start Git")?;
        ensure!(status.success(), "git diff {head} {tree} failed");
        let patch = patch.to_str().context("Scratch path must be UTF-8")?;
        git(
            workspace,
            None,
            &["apply", "--binary", "--whitespace=nowarn", patch],
        )?;
    }
    let now = current_tree(workspace)?.context("The checkout is not a Git repository")?;
    ensure!(
        now == tree,
        "The checkout does not reproduce snapshot {tree} exactly (got {now}); Git attributes or filters may rewrite files"
    );
    Ok(())
}

/// Keeps existing tree `tree` under run `run`'s refs, so cleaning another
/// run cannot prune it. False when `tree` is not in the repository.
pub fn keep(dir: &Path, run: &str, tree: &str) -> bool {
    exists(dir, tree)
        && git(
            dir,
            None,
            &["update-ref", &format!("{}/{tree}", namespace(run)), tree],
        )
        .is_ok()
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
    let child = crate::snapshot::git_command()
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
    if !index.exists() {
        if !is_repository(workspace) {
            return Ok(None);
        }
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

/// File paths that differ between two snapshot trees (one `git diff` call).
pub fn changed_paths(dir: &Path, from: &str, to: &str) -> Result<Vec<String>> {
    let out = git(
        dir,
        None,
        &["diff", "--no-renames", "-z", "--name-only", from, to],
    )?;
    Ok(out
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect())
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
    const STDERR_LIMIT: u64 = 64 * 1024;
    let mut child = crate::snapshot::git_command()
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
    let mut stdout = child.stdout.take().context("No Git output")?;
    let mut stderr = child.stderr.take().context("No Git output")?;
    let mut bytes = Vec::new();
    let (read, error) = std::thread::scope(|scope| {
        let errors = scope.spawn(move || {
            let mut text = Vec::new();
            let _ = stderr.by_ref().take(STDERR_LIMIT).read_to_end(&mut text);
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            String::from_utf8_lossy(&text).into_owned()
        });
        let read = stdout.by_ref().take(max as u64 + 1).read_to_end(&mut bytes);
        if read.is_err() || bytes.len() > max {
            let _ = child.kill();
        }
        let error = errors.join().unwrap_or_default();
        (read, error)
    });
    let status = child.wait()?;
    read.context("Read git diff")?;
    let truncated = bytes.len() > max;
    ensure!(truncated || status.success(), "git diff: {}", error.trim());
    if truncated {
        bytes.truncate(max);
        // Trim an incomplete multi-byte UTF-8 sequence split at `max`, without
        // disturbing valid or invalid bytes earlier in the diff.
        let tail_start = bytes.len().saturating_sub(4);
        if let Some(rel) = bytes[tail_start..]
            .iter()
            .rposition(|&b| (b & 0b1100_0000) != 0b1000_0000)
        {
            let pos = tail_start + rel;
            if let Err(err) = std::str::from_utf8(&bytes[pos..]) {
                if err.error_len().is_none() {
                    bytes.truncate(pos);
                }
            }
        }
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        text.push_str(&format!(
            "\n[Patch truncated after {max} bytes. Full patch: git diff {from} {to}]\n"
        ));
    }
    Ok(text)
}

fn retain_overlap(line: &mut Vec<u8>, overlap: usize) {
    let mut start = line.len().saturating_sub(overlap).max(1);
    while start < line.len() && (line[start] & 0b1100_0000) == 0b1000_0000 {
        start += 1;
    }
    let tail_len = line.len() - start;
    line.copy_within(start.., 1);
    line[0] = b'+';
    line.truncate(1 + tail_len);
}

fn emit_added_tail(
    line: &mut Vec<u8>,
    mut rest: &[u8],
    limit: usize,
    overlap: usize,
    each: &mut impl FnMut(&str),
) {
    while !rest.is_empty() {
        let allowed = limit.saturating_sub(line.len());
        let copy_len = rest.len().min(allowed);
        line.extend_from_slice(&rest[..copy_len]);
        rest = &rest[copy_len..];
        if line.len() == limit && !rest.is_empty() {
            each(&String::from_utf8_lossy(line));
            retain_overlap(line, overlap);
        }
    }
    each(&String::from_utf8_lossy(line));
}

/// The whole `git diff` between two trees, one line at a time and never
/// cut, for scans that must see all of it. `--text` shows files Git would
/// call binary, so their added lines are read too. Very long lines are streamed
/// in bounded 1 MiB chunks with a 2 KiB overlap on added lines.
pub fn patch_lines(dir: &Path, from: &str, to: &str, mut each: impl FnMut(&str)) -> Result<()> {
    const LINE_CHUNK_LIMIT: usize = 1024 * 1024;
    // More than any text a workflow policy pattern (at most 512 bytes) can
    // match, even case-insensitively where lowercasing changes byte lengths,
    // so a match across a chunk boundary is whole in one chunk.
    const OVERLAP: usize = 2048;
    const STDERR_LIMIT: u64 = 64 * 1024;
    let mut child = crate::snapshot::git_command()
        .args([
            "--no-pager",
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--text",
            from,
            to,
        ])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Start Git")?;
    let stdout = child.stdout.take().context("No Git output")?;
    // Drained concurrently, capped at 64 KiB + drained to sink so Git cannot block.
    let mut stderr = child.stderr.take().context("No Git output")?;
    let (read, error) = std::thread::scope(|scope| {
        let errors = scope.spawn(move || {
            let mut text = Vec::new();
            let _ = stderr.by_ref().take(STDERR_LIMIT).read_to_end(&mut text);
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            String::from_utf8_lossy(&text).into_owned()
        });
        let mut reader = std::io::BufReader::new(stdout);
        let mut line: Vec<u8> = Vec::new();
        let mut in_overflow = false;
        let mut overflow_added = false;
        let mut has_new_bytes = false;
        let read = loop {
            let buf = match std::io::BufRead::fill_buf(&mut reader) {
                Ok(buf) => buf,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => break Err(err),
            };
            if buf.is_empty() {
                if !line.is_empty() && (!in_overflow || (overflow_added && has_new_bytes)) {
                    each(&String::from_utf8_lossy(&line));
                }
                break Ok(());
            }
            if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let take = &buf[..pos];
                if !in_overflow {
                    let allowed = LINE_CHUNK_LIMIT.saturating_sub(line.len());
                    if take.len() <= allowed {
                        line.extend_from_slice(take);
                        each(&String::from_utf8_lossy(&line));
                    } else {
                        let (head, rest) = take.split_at(allowed);
                        line.extend_from_slice(head);
                        let is_added = line.first() == Some(&b'+');
                        each(&String::from_utf8_lossy(&line));
                        if is_added {
                            emit_added_tail(&mut line, rest, LINE_CHUNK_LIMIT, OVERLAP, &mut each);
                        }
                    }
                } else if overflow_added && (!take.is_empty() || has_new_bytes) {
                    emit_added_tail(&mut line, take, LINE_CHUNK_LIMIT, OVERLAP, &mut each);
                }
                line.clear();
                in_overflow = false;
                overflow_added = false;
                has_new_bytes = false;
                reader.consume(pos + 1);
                continue;
            }
            if in_overflow && !overflow_added {
                let n = buf.len();
                reader.consume(n);
                continue;
            }
            let allowed = LINE_CHUNK_LIMIT.saturating_sub(line.len());
            let copy_len = buf.len().min(allowed);
            line.extend_from_slice(&buf[..copy_len]);
            if copy_len > 0 {
                has_new_bytes = true;
            }
            reader.consume(copy_len);
            if line.len() == LINE_CHUNK_LIMIT {
                if !in_overflow {
                    overflow_added = line.first() == Some(&b'+');
                    in_overflow = true;
                    each(&String::from_utf8_lossy(&line));
                } else if overflow_added {
                    each(&String::from_utf8_lossy(&line));
                }
                if overflow_added {
                    retain_overlap(&mut line, OVERLAP);
                    has_new_bytes = false;
                } else {
                    line.clear();
                }
            }
        };
        if read.is_err() {
            let _ = child.kill();
        }
        let error = errors.join().unwrap_or_default();
        (read, error)
    });
    let status = child.wait()?;
    read.context("Read git diff")?;
    ensure!(status.success(), "git diff: {}", error.trim());
    Ok(())
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

    #[test]
    fn patch_trims_incomplete_utf8_and_changed_paths_tracks_edits() {
        let Some(dir) = repository() else { return };
        let workspace = dir.path();
        let run_dir = run_dir(workspace);
        let from = tree(workspace, &run_dir).unwrap();
        // Multi-byte UTF-8 line (4-byte emoji repeatedly) + deletion + addition.
        fs::write(
            workspace.join("tracked.txt"),
            format!("{}\n", "🦀".repeat(40)),
        )
        .unwrap();
        fs::write(workspace.join("extra.txt"), "hi\n").unwrap();
        fs::remove_file(workspace.join("doomed.txt")).unwrap();
        let to = tree(workspace, &run_dir).unwrap();
        let mut paths = changed_paths(workspace, &from, &to).unwrap();
        paths.sort();
        assert_eq!(paths, ["doomed.txt", "extra.txt", "tracked.txt"]);

        let header_len = patch(workspace, &from, &to, 10_000)
            .unwrap()
            .find('🦀')
            .unwrap();
        // Cut right inside a 4-byte character (+1, +2, +3 bytes).
        for offset in 1..=3 {
            let cut = patch(workspace, &from, &to, header_len + offset).unwrap();
            let prefix = cut.split("\n[Patch truncated").next().unwrap();
            assert!(
                !prefix.ends_with('\u{FFFD}'),
                "split UTF-8 char at +{offset}: {prefix:?}"
            );
        }
    }

    #[test]
    fn patch_lines_catches_secrets_across_chunk_boundary_and_ignores_mnemonic_prefix() {
        let Some(dir) = repository() else { return };
        let workspace = dir.path();
        git(workspace, None, &["config", "diff.mnemonicPrefix", "true"]).unwrap();
        let run_dir = run_dir(workspace);
        let from = tree(workspace, &run_dir).unwrap();

        let prefix_len = 1024 * 1024 - 12;
        let secret = format!("ghp_{}", "a1b2c3d4".repeat(4));
        let mut big = "x".repeat(prefix_len - 1);
        big.push(' ');
        big.push_str(&secret);
        big.push('\n');
        fs::write(workspace.join("big.txt"), big.as_bytes()).unwrap();
        let to = tree(workspace, &run_dir).unwrap();

        let mut scan = crate::policy::SecretScan::default();
        let mut seen_plus_header = false;
        patch_lines(workspace, &from, &to, |line| {
            if line == "+++ b/big.txt" {
                seen_plus_header = true;
            }
            scan.line(line);
        })
        .unwrap();
        assert!(seen_plus_header, "expected canonical b/ header");
        assert_eq!(scan.hits().len(), 1);
        assert_eq!(scan.hits()[0].file, "big.txt");
    }

    #[test]
    fn patch_lines_keeps_the_longest_policy_pattern_whole_across_chunks() {
        let Some(dir) = repository() else { return };
        let workspace = dir.path();
        let run_dir = run_dir(workspace);
        let from = tree(workspace, &run_dir).unwrap();
        // A 510-byte literal (the most a policy pattern holds after `i:`)
        // that straddles the first 1 MiB chunk boundary.
        let banned = format!("BANNED{}", "z".repeat(504));
        let mut line = "x".repeat(1024 * 1024 - 400);
        line.push_str(&banned);
        line.push_str(" tail\n");
        fs::write(workspace.join("big.txt"), line.as_bytes()).unwrap();
        let to = tree(workspace, &run_dir).unwrap();
        let policies = vec![crate::policy::DiffPolicy {
            name: "no-banned".into(),
            action: crate::policy::PolicyAction::Block,
            pattern: format!("i:{}", banned.to_lowercase()),
            paths: vec![],
            message: "No banned text".into(),
        }];
        let mut scan = crate::policy::PolicyScan::new(&policies);
        patch_lines(workspace, &from, &to, |line| scan.line(line)).unwrap();
        assert_eq!(scan.hits().len(), 1, "{:?}", scan.hits());
    }

    #[test]
    fn snapshot_and_repo_info_do_not_run_repo_local_filters() {
        let Some(dir) = repository() else { return };
        let workspace = dir.path();
        let marker = workspace.join("pwned-by-filter");
        let run_git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(workspace)
                .status()
                .unwrap();
            assert!(status.success(), "{args:?}");
        };
        run_git(&[
            "config",
            "--local",
            "filter.evil.clean",
            &format!("sh -c 'touch {:?}; cat'", marker.display().to_string()),
        ]);
        run_git(&["config", "--local", "filter.evil.required", "true"]);
        // Subsection containing `=` must also be neutralized (not split by `-c`).
        run_git(&[
            "config",
            "--local",
            "filter.a=b.clean",
            &format!("sh -c 'touch {:?}; cat'", marker.display().to_string()),
        ]);
        run_git(&[
            "config",
            "--local",
            "merge.pwn.driver",
            &format!("sh -c 'touch {:?}'", marker.display().to_string()),
        ]);
        fs::write(
            workspace.join(".gitattributes"),
            "*.txt filter=evil\n*.eq filter=a=b\n*.mrg merge=pwn\n",
        )
        .unwrap();
        fs::write(workspace.join("filtered.txt"), "hello\n").unwrap();
        fs::write(workspace.join("subsection.eq"), "hello\n").unwrap();

        let info = crate::storage::repo_info(workspace).expect("repo info");
        assert!(info.changes >= 3);
        assert!(
            !marker.exists(),
            "repo_info executed repo-local filter command"
        );

        let run_dir = run_dir(workspace);
        assert!(tree(workspace, &run_dir).is_some());
        assert!(
            !marker.exists(),
            "snapshot::tree executed repo-local filter command"
        );

        // Verify merge.pwn.driver is cleared by git_command_in.
        let effective = git_command_in(workspace)
            .unwrap()
            .args(["config", "--get", "merge.pwn.driver"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&effective.stdout).trim().is_empty(),
            "merge.pwn.driver was not cleared: {:?}",
            String::from_utf8_lossy(&effective.stdout)
        );

        // Non-UTF-8 filter subsection names must also be preserved and neutralized.
        #[cfg(unix)]
        {
            let cfg_path = workspace.join(".git/config");
            let mut cfg = fs::read(&cfg_path).unwrap();
            cfg.extend_from_slice(b"\n[filter \"bad\xffname\"]\n\tclean = sh -c 'touch ");
            cfg.extend_from_slice(marker.to_string_lossy().as_bytes());
            cfg.extend_from_slice(b"; cat'\n\trequired = true\n");
            fs::write(&cfg_path, cfg).unwrap();
            let mut attrs = fs::read(workspace.join(".gitattributes")).unwrap();
            attrs.extend_from_slice(b"*.raw filter=bad\xffname\n");
            fs::write(workspace.join(".gitattributes"), attrs).unwrap();
            fs::write(workspace.join("bytes.raw"), "payload\n").unwrap();

            assert!(tree(workspace, &run_dir).is_some());
            assert!(
                !marker.exists(),
                "snapshot::tree executed non-UTF-8 repo-local filter"
            );
        }
    }
}
