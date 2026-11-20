use super::*;

pub(super) fn set_aside_handoff(handoff_file: &Path, attempt_dir: &Path) -> Result<()> {
    if handoff_file.exists() {
        fs::create_dir_all(attempt_dir)?;
        fs::rename(handoff_file, attempt_dir.join("handoff-file.json")).with_context(|| {
            format!("Move the previous handoff {} aside", handoff_file.display())
        })?;
    }
    Ok(())
}

/// The attempt paths a run records, which `load` respells and validates.
pub(super) fn recorded_paths(run: &mut Run) -> Vec<&mut PathBuf> {
    let mut paths: Vec<&mut PathBuf> = run.resume.iter_mut().map(|r| &mut r.handoff_file).collect();
    for state in &mut run.stages {
        let mut attempts: Vec<&mut Attempt> = state.attempts.iter_mut().collect();
        for candidate in state.fan_out.iter_mut().flat_map(|f| &mut f.candidates) {
            attempts.extend(&mut candidate.attempts);
            paths.extend(candidate.changes.as_mut().map(|changes| &mut changes.patch));
        }
        for attempt in attempts {
            paths.extend([&mut attempt.directory, &mut attempt.handoff_file]);
            paths.extend(&mut attempt.worktree);
        }
    }
    paths
}

pub fn load(project: &Path, id: &str) -> Result<Run> {
    let mut run: Run = storage::read_json(&storage::run_dir(project, id)?.join("run.json"))?;
    let project = fs::canonicalize(project)?;
    let no_parent = |path: &Path| {
        !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    };
    ensure!(
        run.version == 1
            && run.id == id
            && no_parent(&run.project)
            && fs::canonicalize(&run.project)? == project
            && run.stages.len() == run.workflow.stages.len()
            && run.cursor <= run.stages.len(),
        "Invalid or unsupported run record"
    );
    // The record may spell the project through a link: the project moved
    // behind one since, or a link (perhaps inside the project, where a stage
    // can repoint it) was put there. From now on every path follows the
    // project as it resolves today, never the link.
    let spelled = std::mem::replace(&mut run.project, project.clone());
    let respell = |path: &mut PathBuf| {
        if let Ok(rest) = path.strip_prefix(&spelled) {
            // `join("")` would add a trailing separator to the display.
            *path = if rest.as_os_str().is_empty() {
                project.clone()
            } else {
                project.join(rest)
            };
        }
    };
    respell(&mut run.workspace);
    respell(&mut run.directory);
    recorded_paths(&mut run).into_iter().for_each(respell);
    // run.json is writable from inside a workspace-write sandbox, and the
    // replay approval does not cover paths: where the run executes and
    // keeps its artifacts must follow from the project and ID alone.
    let base = project.join(".model-router");
    let workspace = if run.worktree {
        base.join("worktrees").join(id)
    } else {
        project.clone()
    };
    ensure!(
        run.workspace == workspace && run.directory == base.join("runs").join(id),
        "Invalid or unsupported run record: its workspace or directory is not where the project keeps run {id}"
    );
    // Snapshot names reach Git's command line: hex object names only, never
    // an option such as `--output=<file>`.
    let object = |name: &String| {
        matches!(name.len(), 40 | 64) && name.bytes().all(|b| b.is_ascii_hexdigit())
    };
    let attempts = || {
        run.stages.iter().flat_map(|state| {
            state.attempts.iter().chain(
                state
                    .fan_out
                    .iter()
                    .flat_map(|f| &f.candidates)
                    .flat_map(|c| &c.attempts),
            )
        })
    };
    ensure!(
        run.baseline.iter().all(object)
            && attempts().all(|a| {
                [&a.tree_before, &a.tree_after, &a.checks_tree]
                    .into_iter()
                    .flatten()
                    .all(object)
            })
            && run
                .stages
                .iter()
                .filter_map(|state| state.fan_out.as_ref())
                .all(|f| object(&f.base) && object(&f.base_commit)),
        "Invalid or unsupported run record: a recorded snapshot is not a Git object name"
    );
    // Stage and candidate IDs name directories and worktrees, and candidate
    // IDs are not part of the approval digest.
    ensure!(
        run.workflow.stages.iter().all(|stage| {
            crate::config::is_id(&stage.id)
                && stage
                    .fan_out
                    .as_ref()
                    .is_none_or(|f| f.ids().iter().all(|id| crate::config::is_id(id)))
        }),
        "Invalid or unsupported run record: a stage or candidate ID is not a valid ID"
    );
    // Candidate IDs and attempt numbers name candidate directories and
    // worktrees: they must be the stage's own, and each candidate attempt's
    // worktree exactly where `launch_candidate` puts it.
    let mut worktrees = vec![];
    for (state, stage) in run.stages.iter().zip(&run.workflow.stages) {
        ensure!(
            state.attempts.iter().all(|a| a.worktree.is_none()),
            "Invalid or unsupported run record: a stage attempt names a worktree"
        );
        let Some(fan_out) = &state.fan_out else {
            continue;
        };
        ensure!(
            stage
                .fan_out
                .as_ref()
                .is_some_and(|f| f.ids().iter().eq(fan_out.candidates.iter().map(|c| &c.id))),
            "Invalid or unsupported run record: stage {} has candidates its workflow does not",
            stage.id
        );
        for candidate in &fan_out.candidates {
            for (n, attempt) in candidate.attempts.iter().enumerate() {
                let expected = base
                    .join("candidates")
                    .join(id)
                    .join(&stage.id)
                    .join(format!("{}-{:03}", candidate.id, n + 1));
                ensure!(
                    attempt.number == n + 1
                        && attempt.worktree.as_ref().is_none_or(|w| *w == expected),
                    "Invalid or unsupported run record: candidate {} has a misplaced attempt",
                    candidate.id
                );
                worktrees.push(expected);
            }
        }
    }
    // Attempt paths are read, moved aside, and removed later; all of them
    // live inside this run's own directories under the project's metadata
    // directory (`runs/<id>/…`, `handoffs/<id>/…`, …), never another run's.
    // Links may relocate the metadata directory or those directories, but
    // none inside one may lead out.
    let resolved = |path: &Path| {
        path.ancestors().find_map(|known| {
            Some(
                fs::canonicalize(known)
                    .ok()?
                    .join(path.strip_prefix(known).ok()?),
            )
        })
    };
    let inside = |path: &Path| {
        let Ok(rest) = path.strip_prefix(&base) else {
            return false;
        };
        let parts: Vec<&std::ffi::OsStr> = rest.components().map(|c| c.as_os_str()).collect();
        // Attempt directories sit at `runs/<id>/<stage>/<attempt>`, below the
        // run's own records and locks; in a worktree or candidate checkout,
        // only its metadata directory's handoffs belong to the run, and a
        // candidate's checkout itself.
        if parts.len() <= 2 || parts[1] != std::ffi::OsStr::new(id) {
            return false;
        }
        let laid_out = match parts[0].to_str() {
            Some("runs") => parts.len() >= 4,
            Some("handoffs") => true,
            Some("worktrees" | "candidates") => parts
                .iter()
                .position(|part| *part == ".model-router")
                .map_or(worktrees.iter().any(|w| w == path), |at| {
                    parts.get(at + 1).is_some_and(|next| *next == "handoffs")
                }),
            _ => false,
        };
        if !no_parent(path) || !laid_out {
            return false;
        }
        let mut containers = vec![base.join(rest.components().take(2).collect::<PathBuf>())];
        // A checkout's metadata directory may be a link to the project's
        // (one Git does not ignore is checked out with it), so its handoffs
        // land in this run's own handoff directory.
        if matches!(parts[0].to_str(), Some("worktrees" | "candidates")) {
            containers.push(base.join("handoffs").join(id));
        }
        // The last component is judged by name: a stage may leave a link at
        // a file it can write, which is read, moved, or removed, never
        // followed to write.
        let Some(path) = path
            .parent()
            .and_then(resolved)
            .zip(path.file_name())
            .map(|(parent, name)| parent.join(name))
        else {
            return false;
        };
        containers
            .iter()
            .filter_map(|container| resolved(container))
            .any(|container| path.starts_with(container))
    };
    ensure!(
        recorded_paths(&mut run)
            .into_iter()
            .all(|path| inside(path)),
        "Invalid or unsupported run record: a recorded path is outside {}",
        base.display()
    );
    Ok(run)
}

/// Applies a recovery or checkpoint command to the saved run and saves the
/// result, holding the run lock throughout. With `revision`, refuses a run
/// that another process changed since the caller loaded it. Returns the run
/// as saved; callers replace their copy with it.
pub fn command(project: &Path, id: &str, revision: Option<u64>, cmd: Cmd) -> Result<Run> {
    let directory = storage::run_dir(project, id)?;
    let _lock = storage::lock_wait(&directory.join("run.lock"), LOCK_WAIT)?;
    let mut run = load(project, id)?;
    if let Some(revision) = revision {
        ensure!(
            run.revision == revision,
            "This run changed in another process; reload it before continuing"
        );
    }
    match cmd {
        Cmd::Retry { interactive, model } => run.retry(interactive, model)?,
        Cmd::TakeOver => run.take_over()?,
        Cmd::Confirm => run.confirm()?,
        Cmd::AllowRepair => run.allow_repair()?,
        Cmd::EditSummary(summary) => run.edit_summary(summary)?,
        Cmd::RetryCandidate { stage, candidate } => run.retry_candidate(&stage, &candidate)?,
        Cmd::RaiseBudget { usd, tokens } => crate::cost::raise_budget(&mut run, usd, tokens)?,
    }
    run.persist()?;
    Ok(run)
}

/// Every readable run, newest first. Listing should use `history::list`,
/// which reads summaries and reports damaged records.
pub fn history(project: &Path) -> Result<Vec<Run>> {
    Ok(crate::history::list(project)?
        .iter()
        .filter_map(|entry| load(project, entry.id()).ok())
        .collect())
}

/// Saves the patch of this run's changes to changes.diff; best effort, like
/// the snapshots it compares.
pub(super) fn save_changes(run: &Run) {
    if let Ok((from, to)) = run.change_range() {
        if let Ok(patch) = snapshot::patch(&run.workspace, from, to, snapshot::PATCH_LIMIT) {
            let _ = storage::atomic_write(&run.directory.join("changes.diff"), patch.as_bytes());
        }
    }
}

/// How long `drive` waits for the run and workspace locks. Short probes
/// (history cleanup, the TUI's busy-checkout check) hold them briefly.
pub(super) const LOCK_WAIT: Duration = Duration::from_secs(2);

pub(super) fn lock_run(run: &Run) -> Result<(storage::FileLock, storage::FileLock)> {
    let run_lock = storage::lock_wait(&run.directory.join("run.lock"), LOCK_WAIT)?;
    let workspace_lock = storage::lock_wait(
        &storage::metadata_dir(&run.workspace)?.join("workspace.lock"),
        LOCK_WAIT,
    )?;
    Ok((run_lock, workspace_lock))
}
