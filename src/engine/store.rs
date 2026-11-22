use super::*;

pub(super) fn set_aside_handoff(handoff_file: &Path, attempt_dir: &Path) -> Result<()> {
    let Ok(meta) = fs::symlink_metadata(handoff_file) else {
        return Ok(());
    };
    fs::create_dir_all(attempt_dir)?;
    let destination = attempt_dir.join("handoff-file.json");
    match fs::rename(handoff_file, &destination) {
        Ok(()) => Ok(()),
        Err(err) if err.raw_os_error() == Some(libc::EXDEV) => {
            if meta.file_type().is_symlink() {
                fs::remove_file(handoff_file)?;
            } else {
                let mut src = fs::File::open(handoff_file)?;
                let mut dst = storage::create_fresh(&destination)?;
                io::copy(&mut src, &mut dst)?;
                fs::remove_file(handoff_file)?;
            }
            Ok(())
        }
        Err(err) => Err(err)
            .with_context(|| format!("Move the previous handoff {} aside", handoff_file.display())),
    }
}

/// The attempt paths a run records, which `load` respells and validates.
pub(super) fn recorded_paths(run: &mut Run) -> Vec<&mut PathBuf> {
    let mut paths: Vec<&mut PathBuf> = run.resume.iter_mut().map(|r| &mut r.handoff_file).collect();
    for state in &mut run.stages {
        let mut attempts: Vec<&mut Attempt> = state.attempts.iter_mut().collect();
        let candidates = state.fan_out.iter_mut().flat_map(|f| &mut f.candidates);
        for candidate in candidates.chain(&mut state.discarded) {
            attempts.extend(&mut candidate.attempts);
            paths.extend(candidate.changes.as_mut().map(|changes| &mut changes.patch));
        }
        for attempt in attempts {
            paths.extend([&mut attempt.directory, &mut attempt.handoff_file]);
            paths.extend(&mut attempt.worktree);
            paths.extend(attempt.checks.iter_mut().map(|check| &mut check.directory));
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
            && run.cursor <= run.stages.len()
            && (run.cursor < run.stages.len()
                || !matches!(
                    run.status,
                    Status::AwaitingConfirmation | Status::Interactive
                )),
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
            state
                .attempts
                .iter()
                .chain(state.candidate_attempts().map(|(_, a)| a))
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
        // A discarded round's checkouts sit beside the current round's.
        let checkouts = base.join("candidates").join(id).join(&stage.id);
        for attempt in state.discarded.iter().flat_map(|c| &c.attempts) {
            let Some(worktree) = &attempt.worktree else {
                continue;
            };
            ensure!(
                stage.fan_out.is_some()
                    && worktree.parent() == Some(checkouts.as_path())
                    && worktree.file_name().is_some(),
                "Invalid or unsupported run record: stage {} has a misplaced discarded candidate",
                stage.id
            );
            worktrees.push(worktree.clone());
        }
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
        ensure!(
            fan_out.round <= state.attempts.len(),
            "Invalid or unsupported run record: stage {} has an invalid fan-out round",
            stage.id
        );
        for candidate in &fan_out.candidates {
            for (n, attempt) in candidate.attempts.iter().enumerate() {
                let leaf = if fan_out.round <= 1 {
                    format!("{}-{:03}", candidate.id, n + 1)
                } else {
                    format!("r{:03}-{}-{:03}", fan_out.round, candidate.id, n + 1)
                };
                let expected = base.join("candidates").join(id).join(&stage.id).join(leaf);
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
    let mut resolved_cache: std::collections::HashMap<PathBuf, Option<PathBuf>> =
        std::collections::HashMap::new();
    let resolve_cached = |cache: &mut std::collections::HashMap<PathBuf, Option<PathBuf>>,
                          path: &Path|
     -> Option<PathBuf> {
        if let Some(cached) = cache.get(path) {
            return cached.clone();
        }
        let found = path.ancestors().find_map(|known| {
            Some(
                fs::canonicalize(known)
                    .ok()?
                    .join(path.strip_prefix(known).ok()?),
            )
        });
        cache.insert(path.to_path_buf(), found.clone());
        found
    };
    let resolved_runs = resolve_cached(&mut resolved_cache, &base.join("runs").join(id));
    let resolved_handoffs = resolve_cached(&mut resolved_cache, &base.join("handoffs").join(id));
    let resolved_worktrees = resolve_cached(&mut resolved_cache, &base.join("worktrees").join(id));
    let resolved_candidates =
        resolve_cached(&mut resolved_cache, &base.join("candidates").join(id));
    let mut inside = |path: &Path| {
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
        let container = match parts[0].to_str() {
            Some("runs") if parts.len() >= 4 => resolved_runs.as_ref(),
            Some("handoffs") => resolved_handoffs.as_ref(),
            Some("worktrees" | "candidates") => {
                let laid_out = parts
                    .iter()
                    .position(|part| *part == ".model-router")
                    .map_or(worktrees.iter().any(|w| w == path), |at| {
                        parts.get(at + 1).is_some_and(|next| *next == "handoffs")
                    });
                if !laid_out {
                    return false;
                }
                if parts[0] == std::ffi::OsStr::new("worktrees") {
                    resolved_worktrees.as_ref()
                } else {
                    resolved_candidates.as_ref()
                }
            }
            _ => return false,
        };
        if !no_parent(path) {
            return false;
        }
        // The last component is judged by name: a stage may leave a link at
        // a file it can write, which is read, moved, or removed, never
        // followed to write.
        let Some(path) = path
            .parent()
            .and_then(|parent| resolve_cached(&mut resolved_cache, parent))
            .zip(path.file_name())
            .map(|(parent, name)| parent.join(name))
        else {
            return false;
        };
        container.is_some_and(|c| path.starts_with(c))
            || (matches!(parts[0].to_str(), Some("worktrees" | "candidates"))
                && resolved_handoffs
                    .as_ref()
                    .is_some_and(|c| path.starts_with(c)))
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
    let _workspace_lock = if matches!(cmd, Cmd::Confirm)
        && run.status == Status::AwaitingConfirmation
        && graph::judged_by(&run.workflow, run.cursor).is_some()
    {
        let lock = storage::lock_wait(
            &storage::metadata_dir(&run.workspace)?.join("workspace.lock"),
            LOCK_WAIT,
        )?;
        refuse_unproven(&run)?;
        Some(lock)
    } else {
        None
    };
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
            let _ =
                storage::atomic_write_fast(&run.directory.join("changes.diff"), patch.as_bytes());
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
