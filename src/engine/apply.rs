//! Apply a finished worktree run's changes back into the project checkout
//! without committing, pushing, or touching Git's index or branches.
use super::*;

/// What `apply_worktree` will apply to the project checkout.
#[derive(Debug, Clone)]
pub struct ApplyPlan {
    pub run_id: String,
    pub project: PathBuf,
    pub workspace: PathBuf,
    pub from: String,
    pub to: String,
    pub files: Vec<snapshot::FileChange>,
}

impl ApplyPlan {
    pub fn preview(&self) -> String {
        let mut text = format!(
            "Apply preview for run {} (worktree → checkout; nothing committed):\n",
            self.run_id
        );
        text.push_str(&format!("  From worktree: {}\n", self.workspace.display()));
        text.push_str(&format!("  Into checkout: {}\n", self.project.display()));
        text.push_str(&format!("  Files ({}):\n", self.files.len()));
        for file in &self.files {
            let counts = match (file.added, file.removed) {
                (Some(a), Some(r)) => format!(" (+{a}/-{r})"),
                _ => " (binary)".into(),
            };
            text.push_str(&format!(
                "    {} {}{counts}\n",
                file.status.letter(),
                file.path
            ));
        }
        text
    }
}

/// Computes the patch plan for bringing a finished worktree run's changes into
/// the main project checkout.
pub fn apply_worktree_plan(run: &Run) -> Result<ApplyPlan> {
    let project = fs::canonicalize(&run.project).unwrap_or_else(|_| run.project.clone());
    let workspace = fs::canonicalize(&run.workspace).unwrap_or_else(|_| run.workspace.clone());
    ensure!(
        workspace != project,
        "Run {} already worked directly in the project checkout; nothing to apply from a worktree",
        run.id
    );
    ensure!(
        !matches!(
            run.status,
            Status::Running | Status::Interactive | Status::Pending
        ),
        "Run {} is still active ({}); finish or stop it before applying its worktree changes",
        run.id,
        run.status.label()
    );
    ensure!(
        workspace.is_dir() && snapshot::is_repository(&workspace),
        "Run {}'s worktree is no longer present at {}",
        run.id,
        workspace.display()
    );
    ensure!(
        project.is_dir() && snapshot::is_repository(&project),
        "Project checkout at {} is not a Git repository",
        project.display()
    );
    let (from, to) = match run.change_range() {
        Ok((from, to)) => (from.to_owned(), to.to_owned()),
        Err(_) => {
            let from = run
                .stages
                .iter()
                .flat_map(|s| &s.attempts)
                .find_map(|a| a.tree_before.clone())
                .context("Run has no baseline Git snapshot to diff from")?;
            let to = snapshot::current_tree(&workspace)?
                .context("Could not snapshot the run's worktree")?;
            (from, to)
        }
    };
    let files = snapshot::changes(&workspace, &from, &to)?;
    ensure!(
        !files.is_empty(),
        "Run {} has no file changes in its worktree to apply",
        run.id
    );
    if !run.workflow.protected_paths.is_empty() {
        let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
        let hits = crate::policy::protected_matches(&run.workflow.protected_paths, &paths);
        ensure!(
            hits.is_empty(),
            "Refusing to apply protected path(s): {}",
            hits.iter()
                .map(|(p, pat)| format!("{p} (matches {pat})"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(ApplyPlan {
        run_id: run.id.clone(),
        project,
        workspace,
        from,
        to,
        files,
    })
}

/// Applies a worktree run's patch to the main project checkout using plain
/// `git apply`, falling back to a 3-way merge in a private index, and
/// restoring the checkout atomically on conflict. Never commits. With
/// `check_only`, reports what that would do (clean, merged, or conflict)
/// and leaves the checkout untouched.
pub fn apply_worktree(run: &Run, check_only: bool) -> Result<(ApplyPlan, fanout::Applied)> {
    let project = fs::canonicalize(&run.project).unwrap_or_else(|_| run.project.clone());
    let _run_lock = storage::lock_wait(&run.directory.join("run.lock"), LOCK_WAIT)?;
    let _checkout_lock = storage::lock_wait(
        &storage::metadata_dir(&project)?.join("workspace.lock"),
        LOCK_WAIT,
    )?;
    run.check_revision()?;
    let plan = apply_worktree_plan(run)?;
    let scratch = tempfile::tempdir().context("Create apply scratch directory")?;
    let patch_path = scratch.path().join("worktree.patch");
    fanout::write_patch(&plan.workspace, &plan.from, &plan.to, &patch_path)?;
    let index = scratch.path().join("checkout.index");
    let current = snapshot::capture_index(&plan.project, &index)?
        .context("Snapshot the project checkout before applying")?;
    let file_paths: Vec<String> = plan.files.iter().map(|f| f.path.clone()).collect();
    if check_only {
        // The same two steps as the real apply, with the merge tried in a
        // scratch index: the checkout and its index stay as they are.
        let outcome = fanout::check(
            &plan.project,
            &index,
            &file_paths,
            &patch_path,
            scratch.path(),
        )?;
        return Ok((plan, outcome));
    }
    let applied = fanout::apply(
        &plan.project,
        &index,
        &current,
        &file_paths,
        &patch_path,
        scratch.path(),
    )?;
    Ok((plan, applied))
}
