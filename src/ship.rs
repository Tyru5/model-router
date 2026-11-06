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
        text
    }
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
    let mut command = Command::new(program);
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
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input)?;
    }
    let output = child.wait_with_output()?;
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
    values
        .iter()
        .fold(template.to_owned(), |text, (key, value)| {
            text.replace(&format!("{{{key}}}"), value)
        })
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
        .max_by_key(|a| a.started_at);
    let to = match latest {
        Some(attempt) => attempt
            .tree_after
            .clone()
            .context("The run's latest attempt has no snapshot, so its changes are unknown")?,
        None => from.clone(),
    };
    Ok((from, to))
}

fn paths(files: &[FileChange]) -> BTreeSet<String> {
    files.iter().map(|f| f.path.clone()).collect()
}

/// Works out the ship for `run` without changing anything. Errors are the
/// reasons it cannot ship, e.g. files changed before and during the run.
pub fn plan(run: &Run, options: &Options) -> Result<Plan> {
    options.validate()?;
    let dir = run.workspace.as_path();
    ensure!(
        dir.is_dir() && snapshot::is_repository(dir),
        "Ship needs the run's Git workspace: {}",
        dir.display()
    );
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
    let before = snapshot::changes(dir, &head, &start)?;
    let before_paths = paths(&before);
    let changed_paths = paths(&changed);
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
    let tracked: BTreeSet<String> = git(dir, &["ls-tree", "-r", "-z", "--name-only", &head])?
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect();
    let mut files = vec![];
    let mut skipped = vec![];
    for file in changed {
        if !options.include_untracked && !tracked.contains(&file.path) {
            skipped.push(format!(
                "{} (untracked; include_untracked = false)",
                file.path
            ));
        } else {
            files.push(file);
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
    // Edits since the run's last attempt are committed as they are now.
    if let Some(now) = snapshot::tree(dir, &run.directory) {
        if now != end {
            let later: Vec<_> = snapshot::changes(dir, &end, &now)?
                .into_iter()
                .map(|f| f.path)
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
    let _ = storage::write_json(&dir.join("ship.json"), &outcome);
    outcome
}

fn apply_steps(
    plan: &Plan,
    dir: &Path,
    outcome: &mut Outcome,
    log: &mut impl FnMut(String),
) -> Result<()> {
    let work = plan.workspace.as_path();
    let head = git(work, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    ensure!(
        head == plan.head,
        "HEAD moved from {} to {} since the ship was planned; retry the stage to plan again",
        short(&plan.head),
        short(&head)
    );
    // Stage exactly the planned paths in a private index from HEAD.
    let index = std::path::absolute(dir.join("ship.index"))?;
    let _ = fs::remove_file(&index);
    let private = [("GIT_INDEX_FILE", index.as_path())];
    exec("git", &["read-tree", &head], work, &private)?;
    let mut list = Vec::new();
    for file in &plan.files {
        list.extend_from_slice(file.path.as_bytes());
        list.push(0);
    }
    exec_with(
        "git",
        &["update-index", "--add", "--remove", "-z", "--stdin"],
        work,
        &private,
        Some(&list),
    )?;
    let tree = exec("git", &["write-tree"], work, &private)?;
    let _ = fs::remove_file(&index);
    ensure!(
        tree != git(work, &["rev-parse", &format!("{head}^{{tree}}")])?,
        "The planned files match HEAD; nothing to commit"
    );
    let message = std::path::absolute(dir.join("commit-message.txt"))?;
    let message = message.to_str().context("Attempt path must be UTF-8")?;
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
    // The real index now matches the commit for the committed paths, so they
    // show as clean; other staged or unstaged changes stay as they were.
    exec_with(
        "git",
        &["update-index", "--add", "--remove", "-z", "--stdin"],
        work,
        &[],
        Some(&list),
    )?;
    log(format!("Switched to new branch {}\n", plan.branch));
    if let Some(remote) = &plan.push {
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

/// Runs the current ship stage in place of a harness. Without a matching
/// confirmation (and with `confirm`), plans and waits at the checkpoint;
/// otherwise ships and advances, or blocks with the failure.
pub fn execute(run: &mut Run, emit: &mut impl FnMut(Event)) -> Result<()> {
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
        usage: None,
        session: None,
        process_group: None,
        handoff_source: None,
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
    let planned = plan(run, &options).and_then(|plan| {
        save_plan(&plan, &directory)?;
        Ok(plan)
    });
    let plan = match planned {
        Ok(plan) => plan,
        Err(error) => {
            run.stages[index].waiver = None;
            run.stop(Status::Blocked, format!("{error:#}"), Actor::Engine, true)?;
            run.persist()?;
            emit(Event::State(Box::new(run.clone())));
            return Ok(());
        }
    };
    let digest = plan.digest();
    let approved = confirmed.as_deref() == Some(approval(&digest).as_str());
    if options.confirm && !approved {
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
        }
        None => run.advance(Actor::Engine)?,
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
            && run.stages[index].status == Status::AwaitingConfirmation,
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

/// Where `model-router ship` saves its next ship of a finished run.
pub fn completed_dir(run: &Run) -> PathBuf {
    let base = run.directory.join("ship");
    let number = fs::read_dir(&base).map_or(0, |entries| entries.count()) + 1;
    base.join(format!("ship-{number:03}"))
}

/// `model-router ship`: plans a finished run's ship and saves the plan.
pub fn plan_completed(run: &Run, options: &Options) -> Result<(Plan, PathBuf)> {
    ensure!(
        run.status == Status::Complete,
        "Only a completed run can be shipped (this one is {})",
        run.status.label()
    );
    let plan = plan(run, options)?;
    let dir = completed_dir(run);
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
}
