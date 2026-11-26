//! The ship stage: the only way the router commits. The router itself, not a
//! model, commits the files the run changed on a new branch, and optionally
//! pushes it and opens a draft pull request. It runs only when the user adds
//! and enables a ship stage (or runs `model-router ship`), and by default
//! waits at the checkpoint with a preview of exactly what it will do.
//!
//! The commit is built in a private index, so the user's staged changes and
//! files they changed before the run stay out of it. Authorship comes from
//! the user's Git config only; the router adds no trailers or attribution.
//! It never force-pushes, amends, or moves another branch.
use crate::{
    config::Role,
    engine::{Attempt, Event, Run, Status, Transition, Waiver},
    handoff::{Handoff, TestStatus},
    hooks::{self, HookContext, HookEvent},
    policy::{DiffPolicy, PolicyAction, PolicyScan},
    report,
    snapshot::{self, FileChange},
    storage,
    transitions::{Actor, Target},
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fmt::Write as _,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::AtomicBool,
};

/// A ship stage's settings (`[workflows.stages.ship]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    /// New branch name; `{run_id}` and `{slug}` (from the task) expand.
    pub branch: String,
    /// Commit message template; empty generates one from the run's summary.
    /// `{subject}`, `{summary}`, `{task}`, `{run_id}`, `{slug}`, `{branch}` expand.
    pub commit_message: String,
    pub push: bool,
    pub remote: String,
    /// Open a draft pull request with `gh`; requires `push`.
    pub draft_pr: bool,
    /// The pull request's base branch; empty uses the checkout's upstream
    /// branch, else the repository's default branch from `gh`.
    pub base: String,
    /// Wait at the checkpoint with a preview before acting.
    pub confirm: bool,
    /// Commit files the run created; otherwise only changed tracked files.
    pub include_untracked: bool,
    /// Commit files changed both before and during the run (with the user's
    /// earlier edits). Otherwise such files block the stage.
    pub include_preexisting: bool,
    /// Scan added diff lines for high-confidence secrets before committing
    /// (default true). Fails closed on any match.
    pub secret_scan: bool,
    /// Optional external secret scanner argv (no shell), e.g.
    /// `["gitleaks", "protect", "--staged"]`. Requires project trust; fails
    /// closed when missing or exiting non-zero.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_scanner: Vec<String>,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            branch: "mr/{run_id}-{slug}".into(),
            commit_message: String::new(),
            push: false,
            remote: "origin".into(),
            draft_pr: false,
            base: String::new(),
            confirm: true,
            include_untracked: true,
            include_preexisting: false,
            secret_scan: true,
            secret_scanner: vec![],
        }
    }
}
impl Options {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.branch.trim().is_empty(),
            "Ship stages need a branch name"
        );
        ensure!(
            !self.remote.trim().is_empty() && !self.remote.starts_with('-'),
            "Ship remote must be a remote name"
        );
        ensure!(!self.base.starts_with('-'), "Ship base must be a branch");
        ensure!(
            !self.draft_pr || self.push,
            "A ship stage's draft_pr requires push"
        );
        ensure!(
            ![&self.branch, &self.commit_message, &self.remote, &self.base]
                .iter()
                .any(|v| v.contains('\0')),
            "Ship options cannot contain NUL"
        );
        ensure!(
            self.secret_scanner.len() <= 32
                && self.secret_scanner.iter().all(|a| !a.contains('\0'))
                && self
                    .secret_scanner
                    .first()
                    .is_none_or(|p| !p.trim().is_empty()),
            "Ship secret_scanner must be a non-empty command vector without NUL"
        );
        Ok(())
    }

    /// One line for previews, e.g. "branch mr/{run_id}-{slug} · push origin · draft PR · confirm".
    pub fn describe(&self) -> String {
        let mut parts = vec![format!("branch {}", self.branch)];
        if self.push {
            parts.push(format!("push {}", self.remote));
        }
        if self.draft_pr {
            parts.push("draft PR".into());
        }
        parts.push(
            if self.confirm {
                "confirm"
            } else {
                "no confirmation"
            }
            .into(),
        );
        parts.join(" · ")
    }
}

/// A pull request the ship will open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub title: String,
    pub base: String,
    pub body: String,
}

/// Exactly what a ship will do; shown before acting and saved as plan.json.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub run: String,
    pub workspace: PathBuf,
    /// The commit the branch starts from.
    pub head: String,
    /// The checkout's branch, or "detached HEAD".
    pub from: String,
    pub branch: String,
    /// Files committed: those the run changed.
    pub files: Vec<FileChange>,
    /// Files the run changed that are left out, with why.
    pub skipped: Vec<String>,
    /// Files changed before the run that are not committed.
    pub preexisting: Vec<String>,
    pub warnings: Vec<String>,
    pub message: String,
    /// "Name <email>" from the user's Git config.
    pub author: String,
    /// The remote to push to.
    pub push: Option<String>,
    pub pr: Option<PullRequest>,
    /// Whether built-in secret scanning runs again on the staged tree before commit.
    #[serde(default = "default_true")]
    pub secret_scan: bool,
    /// External secret scanner command verified before commit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_scanner: Vec<String>,
    /// Blocking diff policies verified against the staged tree before commit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub block_policies: Vec<DiffPolicy>,
    /// Names of the repo-local Git config the commit or push would run or
    /// follow, never values or subsections (see `repo_command_config`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub git_config: Vec<String>,
    /// Salted PBKDF2 digest of every repo-local Git config entry, to tell
    /// whether any changed after the plan (`git_config_digest`). None in
    /// plans saved before it was recorded, which no longer apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_config_digest: Option<String>,
}
fn default_true() -> bool {
    true
}

/// What a ship did; saved as ship.json. With `error`, what it did before it
/// stopped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub run: String,
    pub at: Option<DateTime<Utc>>,
    pub branch: String,
    pub commit: Option<String>,
    pub author: Option<String>,
    /// "remote/branch" once pushed.
    pub pushed: Option<String>,
    pub pr_url: Option<String>,
    pub files: Vec<String>,
    pub error: Option<String>,
}
impl Outcome {
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!("Branch: {}", self.branch)];
        if let Some(commit) = &self.commit {
            lines.push(format!("Commit: {commit}"));
        }
        if let Some(author) = &self.author {
            lines.push(format!("Author: {author}"));
        }
        if let Some(pushed) = &self.pushed {
            lines.push(format!("Pushed: {pushed}"));
        }
        if let Some(url) = &self.pr_url {
            lines.push(format!("Draft PR: {url}"));
        }
        if let Some(error) = &self.error {
            lines.push(format!("Stopped: {error}"));
        }
        lines
    }
}

impl Plan {
    /// Identifies the plan the user confirmed.
    pub fn digest(&self) -> String {
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        let hash = ring::digest::digest(&ring::digest::SHA256, &bytes);
        hash.as_ref()[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// The human checkpoint's text.
    pub fn preview(&self) -> String {
        let mut text = String::from("Ship preview: nothing has been committed yet.\n\n");
        let _ = writeln!(text, "Workspace: {}", self.workspace.display());
        let _ = writeln!(
            text,
            "Branch: {} (new, from {} at {})",
            self.branch,
            self.from,
            short(&self.head)
        );
        let _ = writeln!(text, "Author: {}", self.author);
        let _ = writeln!(text, "\nCommit {}:", report::files_summary(&self.files));
        for file in &self.files {
            let _ = writeln!(
                text,
                "  {}  {}  {}",
                file.status.letter(),
                file.path,
                report::counts(file)
            );
        }
        if !self.skipped.is_empty() {
            let _ = writeln!(text, "\nNot committed:");
            for line in &self.skipped {
                let _ = writeln!(text, "  {line}");
            }
        }
        if !self.preexisting.is_empty() {
            let _ = writeln!(
                text,
                "\nChanged before the run, left uncommitted: {}",
                self.preexisting.join(", ")
            );
        }
        for warning in &self.warnings {
            let _ = writeln!(text, "\nWarning: {warning}");
        }
        let _ = writeln!(text, "\nCommit message:\n{}", indent(&self.message));
        match &self.push {
            Some(remote) => {
                let _ = writeln!(
                    text,
                    "Push: git push --set-upstream {remote} {}",
                    self.branch
                );
            }
            None => text.push_str("Push: no\n"),
        }
        match &self.pr {
            Some(pr) => {
                let _ = writeln!(
                    text,
                    "Draft PR: gh pr create --draft --base {} --head {}\nPR title: {}\nPR body:\n{}",
                    pr.base,
                    self.branch,
                    pr.title,
                    indent(&pr.body)
                );
            }
            None => text.push_str("Draft PR: no\n"),
        }
        if !self.git_config.is_empty() {
            text.push_str(
                "\nThis repository's own Git config sets commands, proxies, or URL rewrites the commit or push uses. Their values can hold credentials and are not shown or saved; read them with `git config --show-scope --list`:\n",
            );
            for entry in &self.git_config {
                let _ = writeln!(text, "  {entry}");
            }
        }
        text
    }
}

/// What `repo_command_config` found.
struct RepoCommandConfig {
    /// The listed keys, in config order, once each: `section.var`, or
    /// `section.*.var` for a key with a subsection. Never a value or a
    /// subsection, either of which can hold a credential.
    shown: Vec<String>,
    /// `git_config_digest` of every repo-local entry.
    digest: String,
}

/// Rounds of PBKDF2-HMAC-SHA256 behind `git_config_digest`.
const GIT_CONFIG_DIGEST_ROUNDS: std::num::NonZeroU32 = std::num::NonZeroU32::new(100_000).unwrap();

/// A digest of repo-local Git config `entries` that is safe to save: the
/// entries can hold a credential (a proxy or push URL, a helper command), so
/// it is a slow PBKDF2-HMAC-SHA256 salted with the workspace path, not a
/// plain hash a guess could be checked against quickly.
fn git_config_digest(work: &Path, entries: &[u8]) -> String {
    let mut salt = b"model-router ship git config v1\0".to_vec();
    salt.extend_from_slice(work.as_os_str().as_encoded_bytes());
    let mut out = [0u8; 32];
    ring::pbkdf2::derive(
        ring::pbkdf2::PBKDF2_HMAC_SHA256,
        GIT_CONFIG_DIGEST_ROUNDS,
        &salt,
        entries,
        &mut out,
    );
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether a remote URL names a remote helper (`<transport>::<address>`,
/// such as `ext::<command>`), which `git push` runs as a program.
fn is_helper_transport(url: &str) -> bool {
    url.split_once("::").is_some_and(|(transport, _)| {
        !transport.is_empty()
            && transport
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

/// Repo-local (`local` and `worktree` scope) Git config that makes
/// `git commit-tree -S` or `git push` run a command, go elsewhere, or trust
/// another server: `gpg.program`, `gpg.<format>.program`,
/// `gpg.ssh.defaultKeyCommand`, `core.sshCommand`, `core.askPass`,
/// `core.gitProxy`, `credential.helper`, `credential.<url>.helper`,
/// `url.<base>.insteadOf`, `url.<base>.pushInsteadOf`,
/// `remote.<name>.pushUrl`, `remote.<name>.proxy`,
/// `remote.<name>.receivepack`, `remote.<name>.vcs`, a `remote.<name>.url`
/// that names a remote helper, every `http.*` and `protocol.*` key,
/// `push.recurseSubmodules`, `push.followTags`, `submodule.recurse`, and an
/// `includeIf.onbranch:<pattern>.path`, whose file Git reads only once the
/// ship has switched to its branch. Anything that can write the checkout's
/// `.git/config` can set them, so a ship names them in its plan. The router
/// stores no credentials, and a helper, proxy, or URL can hold one, so the
/// plan keeps only key names. The digest covers every repo-local entry.
fn repo_command_config(work: &Path) -> Result<RepoCommandConfig> {
    let output = snapshot::git_command()
        .args(["config", "--show-scope", "-z", "--list"])
        .current_dir(work)
        .stdin(Stdio::null())
        .output()
        .context("Start Git")?;
    ensure!(
        output.status.success(),
        "git config --list (requires Git 2.26+): {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let mut shown: Vec<String> = Vec::new();
    let mut entries: Vec<u8> = Vec::new();
    let mut parts = output.stdout.split(|&b| b == 0);
    while let (Some(scope), Some(raw)) = (parts.next(), parts.next()) {
        if scope != b"local" && scope != b"worktree" {
            continue;
        }
        for part in [scope, raw] {
            entries.extend_from_slice(&(part.len() as u64).to_le_bytes());
            entries.extend_from_slice(part);
        }
        let entry = String::from_utf8_lossy(raw);
        let (key, value) = entry.split_once('\n').unwrap_or((&entry, ""));
        let Some((section, rest)) = key.split_once('.') else {
            continue;
        };
        let (subsection, var) = match rest.rsplit_once('.') {
            Some((subsection, var)) => (Some(subsection), var.to_ascii_lowercase()),
            None => (None, rest.to_ascii_lowercase()),
        };
        let nested = subsection.is_some();
        let section = section.to_ascii_lowercase();
        let listed = match section.as_str() {
            "gpg" => matches!(var.as_str(), "program" | "defaultkeycommand"),
            "core" => !nested && matches!(var.as_str(), "sshcommand" | "askpass" | "gitproxy"),
            "credential" => var == "helper",
            "url" => nested && matches!(var.as_str(), "insteadof" | "pushinsteadof"),
            "remote" => {
                nested
                    && (matches!(var.as_str(), "pushurl" | "proxy" | "receivepack" | "vcs")
                        || (var == "url" && is_helper_transport(value)))
            }
            "http" | "protocol" => true,
            "push" => !nested && matches!(var.as_str(), "recursesubmodules" | "followtags"),
            "submodule" => !nested && var == "recurse",
            "includeif" => {
                var == "path"
                    && subsection.is_some_and(|condition| {
                        condition
                            .get(.."onbranch:".len())
                            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("onbranch:"))
                    })
            }
            _ => false,
        };
        if listed {
            // `section` and `var` are config names, never a value or a
            // subsection.
            let name = if nested {
                format!("{section}.*.{var}")
            } else {
                format!("{section}.{var}")
            };
            if !shown.contains(&name) {
                shown.push(name);
            }
        }
    }
    Ok(RepoCommandConfig {
        shown,
        digest: git_config_digest(work, &entries),
    })
}

/// Stops unless the repository's own Git config is what `plan` recorded.
fn ensure_git_config_planned(plan: &Plan, work: &Path, before: &str) -> Result<()> {
    let current = repo_command_config(work)?;
    ensure!(
        plan.git_config_digest.as_deref() == Some(current.digest.as_str()),
        "This repository's own Git config changed since the ship was planned (checked before {before}); plan again and review it"
    );
    Ok(())
}

/// Refuses a push remote defined by a legacy `remotes/<name>` or
/// `branches/<name>` file in the Git directory: `git config` does not list
/// those, so a plan cannot tell where the push goes or whether it changed.
fn ensure_configured_remote(work: &Path, remote: &str) -> Result<()> {
    let common = PathBuf::from(git(work, &["rev-parse", "--git-common-dir"])?);
    let common = if common.is_absolute() {
        common
    } else {
        work.join(common)
    };
    for legacy in ["remotes", "branches"] {
        ensure!(
            fs::symlink_metadata(common.join(legacy).join(remote)).is_err(),
            "Remote {remote} is defined by a legacy .git/{legacy}/{remote} file; define it with `git remote add` to ship to it"
        );
    }
    Ok(())
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

fn indent(text: &str) -> String {
    text.lines().map(|line| format!("    {line}\n")).collect()
}

/// Runs a program in `dir` without a shell or terminal prompts; returns its
/// trimmed stdout, or its exact stderr as the error.
fn exec(program: &str, args: &[&str], dir: &Path, envs: &[(&str, &Path)]) -> Result<String> {
    exec_with(program, args, dir, envs, None)
}

fn exec_with(
    program: &str,
    args: &[&str],
    dir: &Path,
    envs: &[(&str, &Path)],
    stdin: Option<&[u8]>,
) -> Result<String> {
    // Git also gets the router's hook and fsmonitor overrides: a stage may
    // have written the hooks directory `core.hooksPath` names.
    let mut command = if program == "git" {
        crate::snapshot::git_command()
    } else {
        Command::new(program)
    };
    // Git processes other programs start (gh runs `git status`) get them
    // through the variable `-c` itself sets, which every Git version reads,
    // after any the caller set.
    const OVERRIDES: &str = "'core.hooksPath=/dev/null' 'core.fsmonitor=false'";
    let mut parameters = std::env::var_os("GIT_CONFIG_PARAMETERS").unwrap_or_default();
    if !parameters.is_empty() {
        parameters.push(" ");
    }
    parameters.push(OVERRIDES);
    command.env("GIT_CONFIG_PARAMETERS", parameters);
    command
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_LITERAL_PATHSPECS", "1")
        // Identity comes from the user's Git config only.
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in envs {
        command.env(name, value);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("Start {program}"))?;
    let output = std::thread::scope(|scope| -> Result<std::process::Output> {
        let writer = stdin.zip(child.stdin.take()).map(|(input, mut pipe)| {
            scope.spawn(move || -> std::io::Result<()> {
                match pipe.write_all(input) {
                    Ok(()) => Ok(()),
                    Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                    Err(err) => Err(err),
                }
            })
        });
        let output = child.wait_with_output()?;
        if let Some(handle) = writer {
            if let Ok(Err(err)) = handle.join() {
                if output.status.success() {
                    return Err(err.into());
                }
            }
        }
        Ok(output)
    })?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    ensure!(
        output.status.success(),
        "{program} {} failed ({}): {}",
        args.first().copied().unwrap_or_default(),
        output
            .status
            .code()
            .map_or("signal".into(), |c| format!("exit {c}")),
        if stderr.is_empty() {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        } else {
            stderr
        }
    );
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned())
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    exec("git", args, dir, &[])
}

/// The task's first line as a branch-safe slug of at most 40 characters.
pub fn slug(task: &str) -> String {
    let line = task.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut slug = String::new();
    for c in line.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "run".into()
    } else {
        slug.into()
    }
}

/// Drops attribution lines a model may have written into a summary: the
/// router never adds trailers or AI attribution to a commit or PR.
fn scrub(text: &str) -> String {
    text.lines()
        .filter(|line| {
            let lower = line.trim().to_lowercase();
            !(lower.starts_with("co-authored-by:")
                || lower.starts_with("signed-off-by:")
                || lower.contains("generated with"))
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// The run's summary: the latest review brief, else the latest completed
/// stage's handoff summary.
fn summary(run: &Run) -> Option<String> {
    let completed = || {
        run.workflow
            .stages
            .iter()
            .zip(&run.stages)
            .rev()
            .filter(|(stage, state)| stage.role != Role::Ship && state.status == Status::Complete)
    };
    completed()
        .find(|(stage, _)| stage.role == Role::Summary)
        .or_else(|| completed().find(|(_, state)| state.handoff.is_some()))
        .and_then(|(_, state)| state.handoff.as_ref())
        .map(|h| scrub(&h.summary))
        .filter(|s| !s.is_empty())
}

/// The first line of prose: headings skipped, list markers removed, cut to
/// 72 characters at a word.
fn subject(text: &str) -> Option<String> {
    let line = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("```"))
        .map(|l| l.trim_start_matches(['-', '*', '>', ' ']).trim())
        .find(|l| !l.is_empty())?;
    let line = line.replace(['`', '*'], "");
    if line.chars().count() <= 72 {
        return Some(line.trim_end_matches('.').to_owned());
    }
    let cut: String = line.chars().take(72).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    Some(cut.trim_end_matches([',', ';', ':', '.']).to_owned())
}

fn expand(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        rest = &rest[open..];
        let Some(close) = rest[1..].find('}') else {
            out.push_str(rest);
            return out;
        };
        let key = &rest[1..1 + close];
        if let Some((_, value)) = values.iter().find(|(k, _)| *k == key) {
            out.push_str(value);
            rest = &rest[close + 2..];
        } else {
            out.push('{');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

/// The run's changes up to its last non-ship attempt: the files to commit.
fn run_range(run: &Run) -> Result<(String, String)> {
    let from = run
        .baseline
        .clone()
        .context("This run recorded no Git baseline snapshot, so its changes are unknown")?;
    let latest = run
        .workflow
        .stages
        .iter()
        .zip(&run.stages)
        .filter(|(stage, _)| stage.role != Role::Ship)
        .flat_map(|(_, state)| &state.attempts)
        .max_by_key(|a| (a.finished_at.unwrap_or(a.started_at), a.started_at));
    let to = match latest {
        Some(attempt) => attempt
            .tree_after
            .clone()
            .context("The run's latest attempt has no snapshot, so its changes are unknown")?,
        None => from.clone(),
    };
    Ok((from, to))
}

/// Works out the ship for `run` without changing anything. Errors are the
/// reasons it cannot ship, e.g. files changed before and during the run.
pub fn plan(run: &Run, options: &Options) -> Result<Plan> {
    options.validate()?;
    run.check_review_quorum()?;
    let dir = run.workspace.as_path();
    ensure!(
        dir.is_dir() && snapshot::is_repository(dir),
        "Ship needs the run's Git workspace: {}",
        dir.display()
    );
    let top = PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"])?);
    let name = git(dir, &["config", "user.name"]).unwrap_or_default();
    let email = git(dir, &["config", "user.email"]).unwrap_or_default();
    ensure!(
        !name.trim().is_empty() && !email.trim().is_empty(),
        "Set your Git identity first (git config user.name and user.email); ship commits only as you"
    );
    let head = git(dir, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .context("Ship needs a commit to branch from; this repository has none")?;
    let from = git(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .unwrap_or_else(|_| "detached HEAD".into());
    let slug = slug(&run.task);
    let branch = expand(&options.branch, &[("run_id", &run.id), ("slug", &slug)]);
    ensure!(
        !branch.starts_with('-'),
        "Ship branch {branch:?} is not a valid name"
    );
    git(dir, &["check-ref-format", "--branch", &branch])
        .with_context(|| format!("Ship branch {branch:?} is not a valid name"))?;
    ensure!(
        git(
            dir,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}")
            ]
        )
        .is_err(),
        "Branch {branch} already exists; ship never moves an existing branch. Rename it in the stage's ship options"
    );

    let (start, end) = run_range(run)?;
    let changed = snapshot::changes(dir, &start, &end)?;
    // Changes present when the run started, relative to the branch point.
    let before_paths: BTreeSet<String> = snapshot::changed_paths(dir, &head, &start)?
        .into_iter()
        .collect();
    let changed_paths: BTreeSet<String> = changed.iter().map(|f| f.path.clone()).collect();
    let overlap: Vec<_> = changed_paths.intersection(&before_paths).cloned().collect();
    ensure!(
        overlap.is_empty() || options.include_preexisting,
        "These files were changed both before and during the run, so a commit would include your earlier edits: {}. Commit or revert those edits first, or set include_preexisting = true in the ship options",
        overlap.join(", ")
    );
    let mut warnings = vec![];
    if !overlap.is_empty() {
        warnings.push(format!(
            "Changed before and during the run; committed with your earlier edits: {}",
            overlap.join(", ")
        ));
    }
    let mut files = vec![];
    let mut skipped = vec![];
    if options.include_untracked {
        files = changed;
    } else {
        let tracked: BTreeSet<String> = git(
            &top,
            &["ls-tree", "--full-tree", "-r", "-z", "--name-only", &head],
        )?
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect();
        for file in changed {
            if !tracked.contains(&file.path) {
                skipped.push(format!(
                    "{} (untracked; include_untracked = false)",
                    file.path
                ));
            } else {
                files.push(file);
            }
        }
    }
    ensure!(
        !files.is_empty(),
        "The run changed no files to commit{}",
        if skipped.is_empty() {
            ""
        } else {
            " (all skipped)"
        }
    );
    let file_paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
    if !run.workflow.protected_paths.is_empty() {
        let protected =
            crate::policy::protected_matches(&run.workflow.protected_paths, &file_paths);
        ensure!(
            protected.is_empty(),
            "Refusing to ship protected path(s): {}",
            protected
                .iter()
                .map(|(path, pat)| format!("{path} (matches {pat})"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let block_policies: Vec<DiffPolicy> = run
        .workflow
        .policies
        .iter()
        .filter(|p| p.action == PolicyAction::Block && !p.pattern.is_empty())
        .cloned()
        .collect();
    if options.secret_scan || !options.secret_scanner.is_empty() || !block_policies.is_empty() {
        // The same private index the commit is built from, in a scratch
        // directory: the scans see exactly the planned files as they would
        // be committed now, not skipped files or the user's own index.
        let scratch = tempfile::Builder::new()
            .prefix("ship-scan-")
            .tempdir_in(&run.directory)
            .context("Create a scratch directory for the ship scan")?;
        let index = std::path::absolute(scratch.path().join("index"))?;
        let tree = stage_planned(&top, &head, &files, &index)?;
        scan_staged(
            dir,
            &Staged {
                head: &head,
                tree: &tree,
                index: &index,
                files: &files,
            },
            options.secret_scan,
            &options.secret_scanner,
            &block_policies,
            ("", "refusing to ship"),
        )?;
    }
    // Edits since the run's last attempt are committed as they are now.
    if let Ok(Some(now)) = snapshot::current_tree(dir) {
        if now != end {
            let later: Vec<_> = snapshot::changed_paths(dir, &end, &now)?
                .into_iter()
                .filter(|p| files.iter().any(|f| f.path == *p))
                .collect();
            if !later.is_empty() {
                warnings.push(format!(
                    "Changed again after the run; committed as they are now: {}",
                    later.join(", ")
                ));
            }
        }
    }
    let preexisting = before_paths.difference(&changed_paths).cloned().collect();

    let summary = summary(run);
    let task_line = run
        .task
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("Model router run")
        .trim();
    let subject = summary
        .as_deref()
        .and_then(subject)
        .or_else(|| subject(task_line))
        .unwrap_or_else(|| "Model router run".into());
    let reference = format!("From model-router run {}.", run.id);
    let message = if options.commit_message.trim().is_empty() {
        match &summary {
            Some(summary) => format!("{subject}\n\n{summary}\n\n{reference}\n"),
            None => format!("{subject}\n\n{reference}\n"),
        }
    } else {
        let text = expand(
            &options.commit_message,
            &[
                ("subject", &subject),
                ("summary", summary.as_deref().unwrap_or("")),
                ("task", run.task.trim()),
                ("run_id", &run.id),
                ("slug", &slug),
                ("branch", &branch),
            ],
        );
        format!("{}\n", scrub(&text))
    };
    ensure!(
        !message.trim().is_empty(),
        "The ship commit message is empty"
    );

    let push = if options.push {
        git(dir, &["remote", "get-url", &options.remote]).with_context(|| {
            format!(
                "Ship pushes to remote {:?}, which is not configured",
                options.remote
            )
        })?;
        Some(options.remote.clone())
    } else {
        None
    };
    let pr = if options.draft_pr {
        let base = if options.base.trim().is_empty() {
            default_base(dir, &options.remote)?
        } else {
            options.base.trim().to_owned()
        };
        Some(PullRequest {
            title: subject.clone(),
            base,
            body: pr_body(run, &files, summary.as_deref()),
        })
    } else {
        None
    };
    if let Some(remote) = &push {
        ensure_configured_remote(dir, remote)?;
    }
    let git_config = repo_command_config(dir)?;
    Ok(Plan {
        run: run.id.clone(),
        workspace: dir.to_path_buf(),
        head,
        from,
        branch,
        files,
        skipped,
        preexisting,
        warnings,
        message,
        author: format!("{name} <{email}>"),
        push,
        pr,
        secret_scan: options.secret_scan,
        secret_scanner: options.secret_scanner.clone(),
        block_policies,
        git_config: git_config.shown,
        git_config_digest: Some(git_config.digest),
    })
}

/// The checkout branch's upstream on `remote`, else the repository's
/// default branch as `gh` reports it.
fn default_base(dir: &Path, remote: &str) -> Result<String> {
    if let Ok(upstream) = git(
        dir,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    ) {
        if let Some(branch) = upstream.strip_prefix(&format!("{remote}/")) {
            if !branch.is_empty() {
                return Ok(branch.to_owned());
            }
        }
    }
    let name = exec(
        "gh",
        &[
            "repo",
            "view",
            "--json",
            "defaultBranchRef",
            "--jq",
            ".defaultBranchRef.name",
        ],
        dir,
        &[],
    )
    .context("Find the pull request's base branch (set base in the ship options to skip this)")?;
    let name = name.trim();
    ensure!(
        !name.is_empty() && !name.starts_with('-'),
        "gh reported no default branch; set base in the ship options"
    );
    Ok(name.to_owned())
}

/// Markdown from the run report: summary, changed files, checks, and
/// unresolved items.
fn pr_body(run: &Run, files: &[FileChange], summary: Option<&str>) -> String {
    let report = report::build(run);
    let mut text = String::new();
    let _ = writeln!(
        text,
        "{}\n",
        summary.unwrap_or("No summary was recorded for this run.")
    );
    let _ = writeln!(text, "## Changed files\n");
    for file in files {
        let _ = writeln!(
            text,
            "- `{}` ({}, {})",
            file.path,
            file.status.letter(),
            report::counts(file)
        );
    }
    let checks: Vec<_> = report
        .stages
        .iter()
        .flat_map(|s| s.checks.iter().map(move |c| (c, s.name.as_str())))
        .collect();
    let _ = writeln!(text, "\n## Checks\n");
    if checks.is_empty() {
        let _ = writeln!(text, "None run or reported.");
    }
    for (check, stage) in checks {
        let _ = writeln!(
            text,
            "- {} `{}` ({})",
            match check.status {
                TestStatus::Passed => "passed",
                TestStatus::Failed => "failed",
                TestStatus::NotRun => "not run",
            },
            check.command,
            report::check_detail(check, stage)
        );
    }
    let _ = writeln!(text, "\n## Unresolved\n");
    if report.unresolved.is_empty() {
        let _ = writeln!(text, "None.");
    }
    for item in &report.unresolved {
        let _ = writeln!(text, "- {}", report::item_text(item));
    }
    let _ = writeln!(text, "\nFrom model-router run {}.", run.id);
    scrub(&text) + "\n"
}

/// Saves the plan's files in `dir`: plan.json, preview.md, commit-message.txt,
/// and pr-body.md.
pub fn save_plan(plan: &Plan, dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    storage::write_json(&dir.join("plan.json"), plan)?;
    storage::atomic_write(&dir.join("preview.md"), plan.preview().as_bytes())?;
    storage::atomic_write(&dir.join("commit-message.txt"), plan.message.as_bytes())?;
    if let Some(pr) = &plan.pr {
        storage::atomic_write(&dir.join("pr-body.md"), pr.body.as_bytes())?;
    }
    Ok(())
}

/// Commits, pushes, and opens the PR as planned, using the files `save_plan`
/// wrote in `dir`. Saves ship.json there. Stops at the first failure with its
/// exact stderr; never forces, amends, or touches another branch.
pub fn apply(plan: &Plan, dir: &Path, mut log: impl FnMut(String)) -> Outcome {
    let mut outcome = Outcome {
        run: plan.run.clone(),
        at: Some(Utc::now()),
        branch: plan.branch.clone(),
        files: plan.files.iter().map(|f| f.path.clone()).collect(),
        ..Outcome::default()
    };
    if let Err(error) = apply_steps(plan, dir, &mut outcome, &mut log) {
        outcome.error = Some(format!("{error:#}"));
    }
    if let Err(error) = storage::write_json(&dir.join("ship.json"), &outcome) {
        let save_err = format!("Save {}: {error:#}", dir.join("ship.json").display());
        outcome.error = Some(match outcome.error {
            Some(existing) => format!("{existing}; {save_err}"),
            None => save_err,
        });
    }
    outcome
}

/// The planned paths, NUL-terminated, for `git update-index -z --stdin`.
fn path_list(files: &[FileChange]) -> Vec<u8> {
    let mut list = Vec::new();
    for file in files {
        list.extend_from_slice(file.path.as_bytes());
        list.push(0);
    }
    list
}

/// A private Git index file, removed with its lock when dropped.
struct PrivateIndex(PathBuf);
impl Drop for PrivateIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = fs::remove_file(lock);
    }
}

fn exec_sanitized_git(
    sanitized: &snapshot::SanitizedGitDir,
    args: &[&str],
    dir: &Path,
    index: &Path,
    stdin: Option<&[u8]>,
) -> Result<String> {
    let mut command = sanitized.command(dir);
    command
        .args(args)
        .env("GIT_INDEX_FILE", index)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("Start git")?;
    let output = std::thread::scope(|scope| -> Result<std::process::Output> {
        let writer = stdin.zip(child.stdin.take()).map(|(input, mut pipe)| {
            scope.spawn(move || -> std::io::Result<()> {
                match pipe.write_all(input) {
                    Ok(()) => Ok(()),
                    Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                    Err(err) => Err(err),
                }
            })
        });
        let output = child.wait_with_output()?;
        if let Some(handle) = writer {
            if let Ok(Err(err)) = handle.join() {
                if output.status.success() {
                    return Err(err.into());
                }
            }
        }
        Ok(output)
    })?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    ensure!(
        output.status.success(),
        "git {} failed ({}): {}",
        args.first().copied().unwrap_or_default(),
        output
            .status
            .code()
            .map_or("signal".into(), |c| format!("exit {c}")),
        if stderr.is_empty() {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        } else {
            stderr
        }
    );
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned())
}

/// Refuses to stage paths whose `.gitattributes` `filter` (in the working tree
/// or the private staging `index`) names a repo-local or worktree-local
/// clean/process filter that `SanitizedGitDir` omits, so `ship` never commits
/// raw unfiltered bytes (such as an uncleaned Git LFS object).
fn refuse_suppressed_clean_filters(
    sanitized: &snapshot::SanitizedGitDir,
    work: &Path,
    files: &[FileChange],
    index: &Path,
) -> Result<()> {
    let drivers = sanitized.local_clean_filters();
    if drivers.is_empty() {
        return Ok(());
    }
    let mut stdin_paths = Vec::new();
    for file in files {
        // A deleted path restored since the run is staged as a file too.
        if file.status != snapshot::ChangeKind::Deleted
            || fs::symlink_metadata(work.join(&file.path)).is_ok()
        {
            stdin_paths.extend_from_slice(file.path.as_bytes());
            stdin_paths.push(0);
        }
    }
    if let Some((path, driver)) =
        snapshot::find_suppressed_filter_path(sanitized, work, &stdin_paths, Some(index), drivers)?
    {
        anyhow::bail!(
            "Refusing to ship {path}: repo-local Git clean filter `{driver}` is disabled for safety and shipping would commit unfiltered bytes; configure the filter globally or commit this path manually"
        );
    }
    Ok(())
}

fn stage_planned(work: &Path, head: &str, files: &[FileChange], index: &Path) -> Result<String> {
    let _ = fs::remove_file(index);
    let sanitized = snapshot::SanitizedGitDir::new(work)?;
    exec_sanitized_git(&sanitized, &["read-tree", head], work, index, None)?;
    refuse_suppressed_clean_filters(&sanitized, work, files, index)?;
    exec_sanitized_git(
        &sanitized,
        &["update-index", "--add", "--remove", "-z", "--stdin"],
        work,
        index,
        Some(&path_list(files)),
    )?;
    exec_sanitized_git(&sanitized, &["write-tree"], work, index, None)
}

/// What a ship would commit: `tree` on top of `head`, staged in `index`.
struct Staged<'a> {
    head: &'a str,
    tree: &'a str,
    index: &'a Path,
    files: &'a [FileChange],
}

/// The secret scans of a staged ship, the same at plan and at commit time.
/// The built-in scan reads the whole diff from `head` to the staged tree,
/// however large. The external scanner runs in the workspace with the
/// planned paths and the private index (see `policy::run_external_scanner`).
/// `wording` is where the secrets were found and what is refused.
fn scan_staged(
    work: &Path,
    staged: &Staged<'_>,
    builtin: bool,
    scanner: &[String],
    block_policies: &[DiffPolicy],
    (found_in, refusing): (&str, &str),
) -> Result<()> {
    if builtin || !block_policies.is_empty() {
        let mut secrets = crate::policy::SecretScan::default();
        let mut policies = PolicyScan::new(block_policies);
        snapshot::patch_lines(work, staged.head, staged.tree, |line| {
            if builtin {
                secrets.line(line);
            }
            if !policies.is_empty_rules() {
                policies.line(line);
            }
        })
        .context("Read the ship diff for policy and secret checks; refusing to ship unscanned")?;
        if builtin {
            secrets.ensure_clean(found_in, refusing)?;
        }
        policies.ensure_clean(found_in, refusing)?;
    }
    if !scanner.is_empty() {
        let file_paths: Vec<String> = staged.files.iter().map(|f| f.path.clone()).collect();
        crate::policy::run_external_scanner(work, scanner, &file_paths, staged.index)?;
    }
    Ok(())
}

fn apply_steps(
    plan: &Plan,
    dir: &Path,
    outcome: &mut Outcome,
    log: &mut impl FnMut(String),
) -> Result<()> {
    let work = plan.workspace.as_path();
    let top = PathBuf::from(git(work, &["rev-parse", "--show-toplevel"])?);
    let head = git(work, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    ensure!(
        head == plan.head,
        "HEAD moved from {} to {} since the ship was planned; retry the stage to plan again",
        short(&plan.head),
        short(&head)
    );
    // The commit and push below read the repository's own config.
    ensure_git_config_planned(plan, work, "staging")?;
    // Stage exactly the planned paths in a private index from HEAD. It
    // stays until the scans below have read it.
    let index = PrivateIndex(std::path::absolute(dir.join("ship.index"))?);
    let tree = stage_planned(&top, &head, &plan.files, &index.0)?;
    let head_tree = git(work, &["rev-parse", &format!("{head}^{{tree}}")])?;
    ensure!(
        tree != head_tree,
        "The planned files match HEAD; nothing to commit"
    );
    scan_staged(
        work,
        &Staged {
            head: &head,
            tree: &tree,
            index: &index.0,
            files: &plan.files,
        },
        plan.secret_scan,
        &plan.secret_scanner,
        &plan.block_policies,
        (" in staged changes", "refusing to commit"),
    )?;
    drop(index);
    let list = path_list(&plan.files);
    let message = std::path::absolute(dir.join("commit-message.txt"))?;
    let message = message.to_str().context("Attempt path must be UTF-8")?;
    // The scans above took time; signing runs what the config names now.
    ensure_git_config_planned(plan, work, "the commit")?;
    // `commit-tree` ignores commit.gpgSign; honor it as `git commit` would.
    let sign = git(work, &["config", "--bool", "commit.gpgsign"]).is_ok_and(|v| v == "true");
    let mut args = vec!["commit-tree", &tree, "-p", &head, "-F", message];
    if sign {
        args.push("-S");
    }
    let commit = git(work, &args)?;
    log(format!("Committed {}\n", short(&commit)));
    // Create the branch at the commit, then point the checkout at it; the
    // working tree is untouched, like `git switch -c` with the commit on top.
    git(work, &["branch", "--no-track", &plan.branch, &commit])?;
    outcome.commit = Some(commit.clone());
    outcome.author = git(work, &["log", "-1", "--format=%an <%ae>", &commit]).ok();
    git(
        work,
        &[
            "symbolic-ref",
            "-m",
            "model-router ship",
            "HEAD",
            &format!("refs/heads/{}", plan.branch),
        ],
    )?;
    // Sync ONLY the committed paths in the real index to match `commit`'s tree,
    // avoiding re-hashing live working tree files and leaving unrelated staged
    // changes untouched.
    exec_with(
        "git",
        &[
            "reset",
            "-q",
            &commit,
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ],
        &top,
        &[],
        Some(&list),
    )?;
    log(format!("Switched to new branch {}\n", plan.branch));
    if let Some(remote) = &plan.push {
        // HEAD is on the new branch now, so an `includeIf.onbranch` file
        // that matches it is part of the config the push reads.
        ensure_git_config_planned(plan, work, "the push, on the new branch")?;
        ensure_configured_remote(work, remote)?;
        let refspec = format!("refs/heads/{0}:refs/heads/{0}", plan.branch);
        git(work, &["push", "--set-upstream", remote, &refspec])?;
        outcome.pushed = Some(format!("{remote}/{}", plan.branch));
        log(format!("Pushed {remote}/{}\n", plan.branch));
    }
    if let Some(pr) = &plan.pr {
        let body = std::path::absolute(dir.join("pr-body.md"))?;
        let body = body.to_str().context("Attempt path must be UTF-8")?;
        let output = exec(
            "gh",
            &[
                "pr",
                "create",
                "--draft",
                "--title",
                &pr.title,
                "--body-file",
                body,
                "--head",
                &plan.branch,
                "--base",
                &pr.base,
            ],
            work,
            &[],
        )?;
        let url = output
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| l.starts_with("http"))
            .unwrap_or(output.trim())
            .to_owned();
        log(format!("Opened draft PR {url}\n"));
        outcome.pr_url = Some(url);
    }
    Ok(())
}

fn handoff(summary: String, files: &[FileChange], decisions: Vec<String>) -> Handoff {
    Handoff {
        decisions,
        files_changed: files.iter().map(|f| f.path.clone()).collect(),
        structured: true,
        ..Handoff::unstructured(summary)
    }
}

/// The confirmation `confirm` records on the stage for a plan.
fn approval(digest: &str) -> String {
    format!("Ship plan {digest} confirmed")
}

/// Runs configured `before_ship` hooks (workflow + optional ship stage) in the
/// run's workspace before committing. Aborts on non-zero exit, timeout, or cancel.
pub fn run_before_ship_hooks(
    run: &Run,
    stage: Option<&crate::config::Stage>,
    log_root: &Path,
    cancel: &AtomicBool,
    record_group: impl FnMut(Option<crate::process::Group>) -> Result<()>,
    output: impl FnMut(String),
) -> Result<hooks::HookRun> {
    let configured = run.workflow.hooks_for(stage, HookEvent::BeforeShip);
    let ctx = HookContext {
        run_id: run.id.clone(),
        workflow: run.workflow.name.clone(),
        stage_id: stage.map(|s| s.id.clone()),
        stage_role: stage.map(|s| s.role.label().into()),
        stage_status: None,
    };
    hooks::run(
        HookEvent::BeforeShip,
        &configured,
        &ctx,
        &run.workspace,
        log_root,
        cancel,
        None,
        record_group,
        output,
    )
}

fn run_ship_stage_hooks(
    run: &mut Run,
    index: usize,
    event: HookEvent,
    directory: &Path,
    cancel: &AtomicBool,
    emit: &mut impl FnMut(Event),
) -> Result<hooks::HookRun> {
    let stage = run.workflow.stages[index].clone();
    let configured = run.workflow.hooks_for(Some(&stage), event);
    if configured.is_empty() {
        return Ok(hooks::HookRun::default());
    }
    let ctx = HookContext {
        run_id: run.id.clone(),
        workflow: run.workflow.name.clone(),
        stage_id: Some(stage.id.clone()),
        stage_role: Some(stage.role.label().into()),
        stage_status: matches!(event, HookEvent::AfterStage | HookEvent::OnFailure)
            .then(|| run.stages[index].status.as_str().into()),
    };
    let workspace = run.workspace.clone();
    let result = hooks::run(
        event,
        &configured,
        &ctx,
        &workspace,
        directory,
        cancel,
        None,
        |group| {
            let is_some = group.is_some();
            if let Some(attempt) = run.stages[index].attempts.last_mut() {
                attempt.set_group(group);
            }
            if is_some {
                run.persist()?;
            }
            Ok(())
        },
        |text| emit(Event::Output(text)),
    )?;
    if let Some(group) = &result.survivor {
        if run.retained_groups.iter().all(|g| g.pgid != group.pgid) {
            run.retained_groups.push(group.clone());
        }
    }
    Ok(result)
}

/// Runs the current ship stage in place of a harness. Without a matching
/// confirmation (and with `confirm`), plans and waits at the checkpoint;
/// otherwise ships and advances, or blocks with the failure.
pub fn execute(run: &mut Run, cancel: &AtomicBool, emit: &mut impl FnMut(Event)) -> Result<()> {
    let index = run.cursor;
    let stage = run.workflow.stages[index].clone();
    let options = stage.ship.clone().unwrap_or_default();
    let confirmed = run.stages[index].waiver.as_ref().map(|w| w.reason.clone());
    // The preview is not part of the run's report, so the plan is stable.
    run.stages[index].handoff = None;
    let number = run.stages[index].attempts.len() + 1;
    let directory = run
        .directory
        .join(format!("{:02}-{}", index + 1, stage.id))
        .join(format!("attempt-{number:03}"));
    fs::create_dir_all(&directory)?;
    // Shipping changes Git state, not files: one snapshot bounds the attempt.
    let tree = snapshot::tree(&run.workspace, &run.directory);
    run.stages[index].attempts.push(Attempt {
        number,
        started_at: Utc::now(),
        finished_at: None,
        model: String::new(),
        mode: crate::config::Mode::Unattended,
        status: Status::Pending,
        exit_code: None,
        directory: directory.clone(),
        handoff_file: directory.join("handoff.json"),
        error: None,
        checks: vec![],
        tree_before: tree.clone(),
        tree_after: tree,
        checks_tree: None,
        ignored_protected_before: None,
        usage: None,
        session: None,
        process_group: None,
        process_identity: Default::default(),
        terminal: None,
        handoff_source: None,
        worktree: None,
        tasks: vec![],
        turns: vec![],
        launched_runs: vec![],
    });
    let reason = format!(
        "Ship attempt {number} started{}",
        if confirmed.is_some() {
            " after confirmation"
        } else {
            ""
        }
    );
    for target in [Target::Attempt(index), Target::Stage(index), Target::Run] {
        run.transition(Transition::new(
            target,
            Status::Running,
            Actor::Engine,
            &reason,
        ))?;
    }
    run.error = None;
    run.persist()?;
    emit(Event::State(Box::new(run.clone())));
    emit(Event::Output(format!(
        "── {} · attempt {number} · router ship ──\n",
        stage.name
    )));
    let before =
        run_ship_stage_hooks(run, index, HookEvent::BeforeStage, &directory, cancel, emit)?;
    if before.cancelled {
        run.stages[index].waiver = None;
        run.stop(
            Status::Cancelled,
            "Ship cancelled during before_stage hook".into(),
            Actor::User,
            true,
        )?;
        run.persist()?;
        emit(Event::State(Box::new(run.clone())));
        return Ok(());
    }
    if let Some(error) = before.failed {
        run.stages[index].waiver = None;
        run.stop(Status::Blocked, error, Actor::Engine, true)?;
        let _ = run_ship_stage_hooks(run, index, HookEvent::OnFailure, &directory, cancel, emit);
        run.persist()?;
        emit(Event::State(Box::new(run.clone())));
        return Ok(());
    }
    let planned = plan(run, &options).and_then(|plan| {
        save_plan(&plan, &directory)?;
        Ok(plan)
    });
    let plan = match planned {
        Ok(plan) => plan,
        Err(error) => {
            run.stages[index].waiver = None;
            run.stop(Status::Blocked, format!("{error:#}"), Actor::Engine, true)?;
            let _ =
                run_ship_stage_hooks(run, index, HookEvent::OnFailure, &directory, cancel, emit);
            run.persist()?;
            emit(Event::State(Box::new(run.clone())));
            return Ok(());
        }
    };
    let digest = plan.digest();
    let approved = confirmed.as_deref() == Some(approval(&digest).as_str());
    // A run an agent or a schedule created never ships unconfirmed, nor
    // does one whose commit or push would use repo-local Git commands.
    let confirm = options.confirm || !run.creation_source.is_user() || !plan.git_config.is_empty();
    if confirm && !approved {
        let mut text = plan.preview();
        if confirmed.is_some() {
            text.insert_str(
                0,
                "The plan changed since you confirmed it; confirm this one.\n\n",
            );
        }
        text.push_str(&format!(
            "\nPlan {digest}. Advance to ship; retry to plan again.\n"
        ));
        emit(Event::Output(text.clone()));
        let state = &mut run.stages[index];
        state.waiver = None;
        state.handoff = Some(handoff(text, &plan.files, vec![]));
        if let Some(attempt) = state.attempts.last_mut() {
            attempt.finished_at = Some(Utc::now());
        }
        let reason = "Ship plan awaits confirmation";
        run.transition(Transition::new(
            Target::Attempt(index),
            Status::Complete,
            Actor::Engine,
            reason,
        ))?;
        for target in [Target::Stage(index), Target::Run] {
            run.transition(Transition::new(
                target,
                Status::AwaitingConfirmation,
                Actor::Engine,
                reason,
            ))?;
        }
        run.persist()?;
        emit(Event::State(Box::new(run.clone())));
        return Ok(());
    }
    let before_ship =
        run_ship_stage_hooks(run, index, HookEvent::BeforeShip, &directory, cancel, emit)?;
    if before_ship.cancelled {
        run.stages[index].waiver = None;
        run.stop(
            Status::Cancelled,
            "Ship cancelled during before_ship hook".into(),
            Actor::User,
            true,
        )?;
        run.persist()?;
        emit(Event::State(Box::new(run.clone())));
        return Ok(());
    }
    if let Some(error) = before_ship.failed {
        run.stages[index].waiver = None;
        run.stop(Status::Blocked, error, Actor::Engine, true)?;
        let _ = run_ship_stage_hooks(run, index, HookEvent::OnFailure, &directory, cancel, emit);
        run.persist()?;
        emit(Event::State(Box::new(run.clone())));
        return Ok(());
    }
    let outcome = apply(&plan, &directory, |line| emit(Event::Output(line)));
    let lines = outcome.lines();
    let state = &mut run.stages[index];
    if let Some(attempt) = state.attempts.last_mut() {
        attempt.finished_at = Some(Utc::now());
    }
    state.handoff = Some(handoff(
        format!("Shipped.\n{}\n", lines.join("\n")),
        &plan.files,
        lines,
    ));
    match &outcome.error {
        Some(error) => {
            let mut message = format!("Ship stopped: {error}");
            if let Some(commit) = &outcome.commit {
                message.push_str(&format!(
                    "\nAlready done: commit {} on branch {}{}",
                    short(commit),
                    outcome.branch,
                    outcome
                        .pushed
                        .as_ref()
                        .map(|p| format!(", pushed to {p}"))
                        .unwrap_or_default()
                ));
            }
            run.stop(Status::Blocked, message, Actor::Engine, true)?;
            let _ =
                run_ship_stage_hooks(run, index, HookEvent::AfterStage, &directory, cancel, emit);
            let _ =
                run_ship_stage_hooks(run, index, HookEvent::OnFailure, &directory, cancel, emit);
        }
        None => {
            run.advance(Actor::Engine)?;
            let _ =
                run_ship_stage_hooks(run, index, HookEvent::AfterStage, &directory, cancel, emit);
        }
    }
    run.persist()?;
    emit(Event::State(Box::new(run.clone())));
    Ok(())
}

/// `Cmd::Confirm` at a ship checkpoint: records the confirmed plan on the
/// stage and returns it to pending, so the next drive ships exactly that
/// plan. In memory only; `engine::command` applies it under the run lock.
pub fn confirm(run: &mut Run) -> Result<()> {
    let index = run.cursor;
    ensure!(
        run.status == Status::AwaitingConfirmation
            && run
                .stages
                .get(index)
                .is_some_and(|s| s.status == Status::AwaitingConfirmation),
        "No ship plan awaits confirmation"
    );
    let attempt = run.stages[index]
        .attempts
        .last()
        .context("This ship stage has no plan")?;
    let plan: Plan = storage::read_json(&attempt.directory.join("plan.json"))?;
    let reason = approval(&plan.digest());
    run.stages[index].waiver = Some(Waiver {
        reason: reason.clone(),
        accepted_at: Utc::now(),
    });
    for target in [Target::Stage(index), Target::Run] {
        run.transition(Transition::new(
            target,
            Status::Pending,
            Actor::User,
            &reason,
        ))?;
    }
    Ok(())
}

/// The ship options of the run's last ship stage, else the defaults.
pub fn run_options(run: &Run) -> Options {
    run.workflow
        .stages
        .iter()
        .rev()
        .find(|s| s.role == Role::Ship)
        .and_then(|s| s.ship.clone())
        .unwrap_or_default()
}

fn next_completed_number(base: &Path) -> usize {
    fs::read_dir(base)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let s = name.to_str()?;
            s.strip_prefix("ship-")?.parse::<usize>().ok()
        })
        .max()
        .unwrap_or(0)
        + 1
}

/// Where `model-router ship` saves its next ship of a finished run.
pub fn completed_dir(run: &Run) -> PathBuf {
    let base = run.directory.join("ship");
    base.join(format!("ship-{:03}", next_completed_number(&base)))
}

/// `model-router ship`: plans a finished run's ship and saves the plan.
pub fn plan_completed(run: &Run, options: &Options) -> Result<(Plan, PathBuf)> {
    ensure!(
        run.status == Status::Complete,
        "Only a completed run can be shipped (this one is {})",
        run.status.label()
    );
    let plan = plan(run, options)?;
    let base = run.directory.join("ship");
    fs::create_dir_all(&base)?;
    let mut number = next_completed_number(&base);
    let dir = loop {
        let candidate = base.join(format!("ship-{number:03}"));
        match fs::create_dir(&candidate) {
            Ok(()) => break candidate,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                number += 1;
            }
            Err(err) => return Err(err.into()),
        }
    };
    save_plan(&plan, &dir)?;
    Ok((plan, dir))
}

/// Saved ships of `run`: its ship stages' attempts, then `model-router ship`.
pub fn outcomes(run: &Run) -> Vec<Outcome> {
    let mut dirs: Vec<PathBuf> = run
        .workflow
        .stages
        .iter()
        .zip(&run.stages)
        .filter(|(stage, _)| stage.role == Role::Ship)
        .flat_map(|(_, state)| state.attempts.iter().map(|a| a.directory.clone()))
        .collect();
    if let Ok(entries) = fs::read_dir(run.directory.join("ship")) {
        let mut later: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        later.sort();
        dirs.extend(later);
    }
    dirs.iter()
        .filter_map(|dir| storage::read_json(&dir.join("ship.json")).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_branch_safe_and_short() {
        assert_eq!(
            slug("Fix the bug: über $(rm -rf) now!\nmore"),
            "fix-the-bug-ber-rm-rf-now"
        );
        assert_eq!(slug("  \n\n"), "run");
        assert_eq!(slug("雪"), "run");
        assert!(slug(&"word ".repeat(30)).len() <= 40);
        assert!(!slug(&"word ".repeat(30)).ends_with('-'));
    }

    #[test]
    fn subjects_skip_headings_and_cut_at_a_word() {
        assert_eq!(
            subject("## Outcome\n\n- The task was **done**.\n## Changes").as_deref(),
            Some("The task was done")
        );
        let long = subject(&"alpha ".repeat(20)).unwrap();
        assert!(
            long.chars().count() <= 72 && long.ends_with("alpha"),
            "{long}"
        );
        assert_eq!(subject("# Only\n## Headings"), None);
    }

    #[test]
    fn scrub_drops_attribution_lines() {
        let text = "Did it\n\nCo-Authored-By: Some Model <x@example.com>\n🤖 Generated with a tool\nSigned-off-by: bot\nKept";
        assert_eq!(scrub(text), "Did it\n\nKept");
    }

    #[test]
    fn templates_expand_known_placeholders_only() {
        assert_eq!(
            expand(
                "mr/{run_id}-{slug}-{other}",
                &[("run_id", "1"), ("slug", "s")]
            ),
            "mr/1-s-{other}"
        );
        // Substituted values containing placeholders must never be re-expanded.
        assert_eq!(
            expand(
                "{subject}/{branch}",
                &[("subject", "{branch}"), ("branch", "oops")]
            ),
            "{branch}/oops"
        );
        let options = Options {
            push: true,
            draft_pr: true,
            ..Options::default()
        };
        assert_eq!(
            options.describe(),
            "branch mr/{run_id}-{slug} · push origin · draft PR · confirm"
        );
        assert!(
            Options {
                remote: "--upload-pack=x".into(),
                ..Options::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn completed_dir_picks_max_plus_one_when_earlier_entries_were_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let mut run = crate::report::tests::sample();
        run.directory = dir.path().to_path_buf();
        fs::create_dir_all(run.directory.join("ship/ship-002")).unwrap();
        assert_eq!(completed_dir(&run), run.directory.join("ship/ship-003"));
    }

    #[test]
    fn plan_works_in_subdirectory_with_include_untracked_false_without_writing_refs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        if git(root, &["config", "--get", "user.name"]).is_err() {
            return;
        }
        git(root, &["init", "-b", "main"]).unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/tracked.txt"), "one\n").unwrap();
        git(root, &["add", "."]).unwrap();
        git(
            root,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "init",
            ],
        )
        .unwrap();

        let sub = root.join("sub");
        let run_dir = root.join(".model-router/runs/r-sub");
        fs::create_dir_all(&run_dir).unwrap();
        let baseline = snapshot::current_tree(&sub).unwrap();
        fs::write(sub.join("tracked.txt"), "two\n").unwrap();
        fs::write(sub.join("untracked.txt"), "skip me\n").unwrap();
        let tree_after = snapshot::current_tree(&sub).unwrap();

        let mut run = crate::report::tests::sample();
        run.workflow.stages.truncate(1);
        run.stages.truncate(1);
        run.id = "r-sub".into();
        run.project = sub.clone();
        run.workspace = sub.clone();
        run.directory = run_dir;
        run.baseline = baseline;
        run.stages[0].attempts.last_mut().unwrap().tree_after = tree_after;
        let opts = Options {
            include_untracked: false,
            secret_scan: false,
            ..Options::default()
        };
        let planned = plan(&run, &opts).unwrap();
        assert_eq!(planned.files.len(), 1);
        assert_eq!(planned.files[0].path, "sub/tracked.txt");
        assert!(
            git(root, &["for-each-ref", "refs/model-router/"])
                .unwrap()
                .is_empty(),
            "dry-run plan must not create snapshot refs"
        );
        assert!(!run.directory.join("snapshot.index").exists());
    }

    #[test]
    fn apply_syncs_planned_paths_from_subdirectory_keeping_unrelated_staged_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-b", "main"]).unwrap();
        git(root, &["config", "user.name", "Test User"]).unwrap();
        git(root, &["config", "user.email", "test@example.com"]).unwrap();
        git(root, &["config", "commit.gpgsign", "false"]).unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("notes.txt"), "note1\n").unwrap();
        fs::write(root.join("sub/tracked.txt"), "one\n").unwrap();
        fs::write(root.join("sub/deleted.txt"), "bye\n").unwrap();
        git(root, &["add", "."]).unwrap();
        git(
            root,
            &["-c", "core.hooksPath=/dev/null", "commit", "-qm", "init"],
        )
        .unwrap();

        let sub = root.join("sub");
        let run_dir = root.join(".model-router/runs/r-apply");
        let attempt_dir = run_dir.join("02-ship/attempt-001");
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(root.join(".model-router/.gitignore"), "*\n").unwrap();
        let baseline = snapshot::current_tree(&sub).unwrap();
        fs::write(sub.join("tracked.txt"), "two\n").unwrap();
        fs::write(sub.join("added.txt"), "new\n").unwrap();
        fs::remove_file(sub.join("deleted.txt")).unwrap();
        let tree_after = snapshot::current_tree(&sub).unwrap();

        let mut run = crate::report::tests::sample();
        run.workflow.stages.truncate(1);
        run.stages.truncate(1);
        run.id = "r-apply".into();
        run.project = sub.clone();
        run.workspace = sub;
        run.directory = run_dir;
        run.baseline = baseline;
        run.stages[0].attempts.last_mut().unwrap().tree_after = tree_after;
        let opts = Options {
            confirm: false,
            secret_scan: false,
            ..Options::default()
        };
        // Repo-local config a push would run is part of the plan.
        git(root, &["config", "core.sshCommand", "ssh -i key"]).unwrap();
        let planned = plan(&run, &opts).unwrap();
        assert_eq!(
            planned
                .files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            ["sub/added.txt", "sub/deleted.txt", "sub/tracked.txt"]
        );
        assert_eq!(planned.git_config, ["core.sshcommand"]);
        assert!(
            planned.preview().contains("  core.sshcommand\n")
                && !planned.preview().contains("ssh -i key"),
            "{}",
            planned.preview()
        );

        // User has unrelated staged edits and an untracked file outside the run.
        fs::write(root.join("notes.txt"), "staged note\n").unwrap();
        git(root, &["add", "notes.txt"]).unwrap();
        fs::write(root.join("other.txt"), "dirty\n").unwrap();

        save_plan(&planned, &attempt_dir).unwrap();
        // A signing program planted after the plan never runs.
        let marker = root.join("signer-ran");
        fs::write(root.join(".git/info/exclude"), "signer-ran\nsigner\n").unwrap();
        let signer = root.join("signer");
        fs::write(
            &signer,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&signer, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        git(root, &["config", "gpg.program", signer.to_str().unwrap()]).unwrap();
        git(root, &["config", "commit.gpgsign", "true"]).unwrap();
        let outcome = apply(&planned, &attempt_dir, |_| {});
        assert!(
            outcome
                .error
                .as_deref()
                .is_some_and(|e| e.contains("changed since the ship was planned")),
            "{:?}",
            outcome.error
        );
        assert!(outcome.commit.is_none() && !marker.exists());
        git(root, &["config", "--unset", "gpg.program"]).unwrap();
        git(root, &["config", "commit.gpgsign", "false"]).unwrap();
        // Nor does a remote URL swapped after the plan receive a push.
        git(
            root,
            &[
                "config",
                "remote.origin.url",
                "https://elsewhere.invalid/x.git",
            ],
        )
        .unwrap();
        let outcome = apply(&planned, &attempt_dir, |_| {});
        assert!(
            outcome
                .error
                .as_deref()
                .is_some_and(|e| e.contains("changed since the ship was planned")),
            "{:?}",
            outcome.error
        );
        git(root, &["config", "--remove-section", "remote.origin"]).unwrap();
        let outcome = apply(&planned, &attempt_dir, |_| {});
        assert_eq!(outcome.error, None);
        assert!(outcome.commit.is_some());
        assert!(attempt_dir.join("ship.json").exists());
        assert_eq!(
            git(root, &["diff", "--name-only", "HEAD~1", "HEAD"]).unwrap(),
            "sub/added.txt\nsub/deleted.txt\nsub/tracked.txt"
        );
        assert_eq!(
            git(root, &["status", "--porcelain"]).unwrap(),
            "M  notes.txt\n?? other.txt"
        );
    }

    #[test]
    fn repo_command_config_names_push_overrides_without_values() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]).unwrap();
        let empty = repo_command_config(root).unwrap();
        assert!(empty.shown.is_empty());

        // A plain remote and a signing choice are in the digest, not the list.
        let url = "https://example.invalid/x.git";
        git(root, &["config", "remote.origin.url", url]).unwrap();
        let remote = repo_command_config(root).unwrap();
        assert!(remote.shown.is_empty());
        assert_ne!(remote.digest, empty.digest);
        git(root, &["config", "commit.gpgsign", "true"]).unwrap();
        let signing = repo_command_config(root).unwrap();
        assert!(signing.shown.is_empty());
        assert_ne!(signing.digest, remote.digest);

        for (key, value) in [
            ("remote.origin.pushUrl", "https://bob:s3cret@push.invalid/x"),
            ("remote.other.pushUrl", "https://push2.invalid/x"),
            ("http.proxy", "proxyuser:proxypass@proxy.invalid:8080"),
            ("http.https://example.invalid/.proxy", "socks5://p.invalid"),
            ("remote.origin.proxy", "dave:pr0xy@proxy2.invalid"),
            ("credential.helper", "!f() { echo password=hunter2; }; f"),
            ("credential.https://alice:tok3n@example.invalid.helper", "x"),
            (
                "url.https://carol:pw@mirror.invalid/.insteadOf",
                "https://e/",
            ),
            ("gpg.program", "/opt/sign --token=abc123"),
            ("http.sslVerify", "false"),
            ("http.extraHeader", "Authorization: Bearer abc123"),
            ("protocol.ext.allow", "always"),
            ("remote.helper.url", "ext::sh -c 'touch x'"),
            ("push.recurseSubmodules", "on-demand"),
            ("push.followTags", "true"),
            ("submodule.recurse", "true"),
            ("includeIf.onbranch:mr/**.path", "evil.inc"),
            ("includeIf.gitdir:/elsewhere/.path", "other.inc"),
        ] {
            git(root, &["config", key, value]).unwrap();
        }
        let found = repo_command_config(root).unwrap();
        assert_ne!(found.digest, signing.digest);
        assert_eq!(
            found.shown,
            [
                "remote.*.pushurl",
                "remote.*.proxy",
                "http.proxy",
                "http.sslverify",
                "http.extraheader",
                "http.*.proxy",
                "credential.helper",
                "credential.*.helper",
                "url.*.insteadof",
                "gpg.program",
                "protocol.*.allow",
                "remote.*.url",
                "push.recursesubmodules",
                "push.followtags",
                "submodule.recurse",
                "includeif.*.path",
            ]
        );

        // The digest still tells one value from another.
        git(
            root,
            &[
                "config",
                "remote.origin.pushUrl",
                "https://bob:other@push.invalid/x",
            ],
        )
        .unwrap();
        let changed = repo_command_config(root).unwrap();
        assert_eq!(changed.shown, found.shown);
        assert_ne!(changed.digest, found.digest);

        assert!(is_helper_transport("ext::sh -c x") && is_helper_transport("hg::https://h/r"));
        for plain in [
            "https://h/r",
            "git@h:r.git",
            "ssh://[::1]/r",
            "[::1]:r",
            "/a/b",
        ] {
            assert!(!is_helper_transport(plain), "{plain}");
        }
    }

    #[test]
    fn apply_rechecks_config_that_the_ship_branch_turns_on() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]).unwrap();
        git(root, &["config", "user.name", "Test"]).unwrap();
        git(root, &["config", "user.email", "test@example.com"]).unwrap();
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git(root, &["add", "a.txt"]).unwrap();
        git(root, &["commit", "-q", "-m", "init"]).unwrap();
        // An include that applies only on the ship's branch is not in the
        // config on `main`, and is once HEAD moves there.
        fs::write(
            root.join(".git/on-ship.inc"),
            "[core]\n\tsshCommand = false\n",
        )
        .unwrap();
        let on_main = repo_command_config(root).unwrap();
        git(
            root,
            &["config", "includeIf.onbranch:ship/**.path", "on-ship.inc"],
        )
        .unwrap();
        let planned = repo_command_config(root).unwrap();
        assert_eq!(planned.shown, ["includeif.*.path"]);
        assert_ne!(planned.digest, on_main.digest);
        git(root, &["symbolic-ref", "HEAD", "refs/heads/ship/x"]).unwrap();
        let on_branch = repo_command_config(root).unwrap();
        assert_eq!(on_branch.shown, ["includeif.*.path", "core.sshcommand"]);
        assert_ne!(on_branch.digest, planned.digest);

        // A legacy remote file is refused: `git config` cannot see it.
        ensure_configured_remote(root, "origin").unwrap();
        fs::create_dir_all(root.join(".git/remotes")).unwrap();
        fs::write(
            root.join(".git/remotes/origin"),
            "URL: https://h.invalid/r\n",
        )
        .unwrap();
        let error = ensure_configured_remote(root, "origin").unwrap_err();
        assert!(format!("{error:#}").contains("legacy"), "{error:#}");
    }

    #[test]
    fn stage_planned_refuses_paths_with_repo_local_clean_filter() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let run_git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .unwrap();
            assert!(status.success(), "{args:?}");
        };
        run_git(&["init", "-q", "-b", "main"]);
        run_git(&["config", "user.name", "Test"]);
        run_git(&["config", "user.email", "test@example.com"]);
        fs::write(root.join("init.txt"), "seed\n").unwrap();
        run_git(&["add", "init.txt"]);
        run_git(&["commit", "-q", "-m", "init"]);
        let head = git(root, &["rev-parse", "HEAD"]).unwrap();

        run_git(&["config", "--local", "filter.lfs.clean", "cat"]);
        run_git(&["config", "--local", "filter.lfs.required", "true"]);
        fs::write(root.join(".gitattributes"), "*.bin filter=lfs\n").unwrap();
        fs::write(root.join("asset.bin"), "raw-binary-bytes\n").unwrap();

        let files = vec![FileChange {
            path: "asset.bin".into(),
            status: snapshot::ChangeKind::Added,
            added: Some(1),
            removed: Some(0),
        }];
        let err = stage_planned(root, &head, &files, &root.join("private.index")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Refusing to ship asset.bin"), "{msg}");
        assert!(msg.contains("filter `lfs`"), "{msg}");
    }
}
