use super::*;

/// Stops harness process groups left by an mr process that died while they
/// ran (no PR_SET_PDEATHSIG outside Linux, and background children survive
/// it everywhere), so a retry never runs two agents on one workspace. Holding
/// both locks means no live router owns them. Process group IDs are not
/// reused while any member lives; a stale ID whose group is gone is ignored.
/// A live group is stopped only if it is provably the recorded one (see
/// `process::group::verify`): after a reboot or a long gap its ID may name
/// one of the user's own groups, which is left running with a message. It
/// may as well still be the run's harness, so the run keeps it in
/// `retained_groups`, and `refuse_unproven` stops any launch while it is
/// alive. Retained groups that have ended, or can now be proven and are
/// stopped, are forgotten here without a word.
pub(super) fn stop_orphans(run: &mut Run, emit: &mut impl FnMut(Event)) -> Result<()> {
    let retained = std::mem::take(&mut run.retained_groups);
    let mut kept = Vec::new();
    for group in &retained {
        // Reported when it was retained; `refuse_unproven` names it again.
        let alive = match process::group::verify(group) {
            process::Verdict::Unverified(_) => true,
            process::Verdict::Ours => stop_recorded(group, "an interrupted execution", emit),
            process::Verdict::Gone | process::Verdict::Reused => false,
        };
        if alive {
            kept.push(group.clone());
        }
    }
    let mut groups = std::mem::take(&mut run.orphan_groups);
    // A harness in its own terminal host is not orphaned: mr reattaches.
    let detached = detached_terminal(run).map(|_| run.cursor);
    for (index, state) in run.stages.iter_mut().enumerate() {
        let kept = state
            .attempts
            .len()
            .checked_sub(1)
            .filter(|_| detached == Some(index));
        let candidates = state
            .fan_out
            .iter_mut()
            .flat_map(|f| &mut f.candidates)
            .flat_map(|c| &mut c.attempts);
        for (number, attempt) in state.attempts.iter_mut().enumerate() {
            if Some(number) == kept {
                continue;
            }
            if let Some(group) = attempt.take_group() {
                groups.push(group);
            }
        }
        for attempt in candidates {
            if let Some(group) = attempt.take_group() {
                groups.push(group);
            }
        }
        // Delegated tasks' turns run in their own groups.
        for turn in state
            .attempts
            .iter_mut()
            .flat_map(|a| &mut a.tasks)
            .flat_map(|t| &mut t.turns)
        {
            if let Some(group) = turn.take_group() {
                groups.push(group);
            }
        }
    }
    // Groups a scheduler worker started but the run never recorded.
    let journal = SpawnJournal::new(run);
    groups.extend(journal.read());
    for group in &groups {
        let alive = stop_recorded(group, "an interrupted execution", emit);
        if alive && kept.iter().all(|g| g.pgid != group.pgid) {
            kept.push(group.clone());
        }
    }
    let changed = !groups.is_empty() || kept != retained;
    run.retained_groups = kept;
    if changed {
        run.persist()?;
    }
    // Only now: an mr that dies while stopping them leaves the journal for
    // the next one, as run.json keeps the groups it recorded until saved.
    journal.clear();
    Ok(())
}

/// Refuses to launch anything for `run` while a group recovery left running
/// (`stop_orphans`) is alive: it may be one of the run's harnesses, still
/// editing the workspace. A run in the project's own checkout also waits
/// for groups other runs left there. Names each group, why it could not be
/// proven, and how to stop it or, if it is something else, release it.
pub fn refuse_unproven(run: &Run) -> Result<()> {
    let mut alive = unproven(&run.id, &run.retained_groups);
    if !run.worktree {
        alive.extend(unproven_in_checkout(&run.project, Some(&run.id)));
    }
    refuse(&format!("run {}", run.id), &alive)
}

/// Refuses to start a run in `project`'s own checkout while a group
/// recovery left running for a run there is alive; see `refuse_unproven`.
pub fn refuse_unproven_in_checkout(project: &Path) -> Result<()> {
    refuse(
        "a new run in this checkout",
        &unproven_in_checkout(project, None),
    )
}

/// Refuses to launch anything for `run` while `run` itself holds an unproven
/// surviving group in `retained_groups`.
pub(super) fn refuse_own_unproven(run: &Run) -> Result<()> {
    refuse(
        &format!("run {}", run.id),
        &unproven(&run.id, &run.retained_groups),
    )
}

/// Lines naming each of `groups`, retained by run `id`, still alive.
pub(super) fn unproven(id: &str, groups: &[process::Group]) -> Vec<String> {
    groups
        .iter()
        .filter_map(|group| {
            let pgid = group.pgid;
            let why = match process::group::verify(group) {
                process::Verdict::Unverified(why) => why,
                process::Verdict::Ours => "it is still the run's and outlived SIGKILL",
                process::Verdict::Gone | process::Verdict::Reused => return None,
            };
            Some(format!(
                "  process group {pgid}: {why}. Left by run {id}. Stop it with \
                 `kill -TERM -- -{pgid}`, or if it belongs to something else, release it with \
                 `model-router resume {id} --release-groups`"
            ))
        })
        .collect()
}

#[derive(Deserialize)]
struct RetainedMeta {
    id: String,
    worktree: bool,
    #[serde(default)]
    run_bytes: Option<u64>,
    #[serde(default)]
    retained_groups: Vec<process::Group>,
}

/// `unproven` for every run in `project`'s own checkout but `except`. Runs
/// with their own worktree work elsewhere.
pub(super) fn unproven_in_checkout(project: &Path, except: Option<&str>) -> Vec<String> {
    let dir = project.join(".model-router/runs");
    let Ok(items) = fs::read_dir(&dir) else {
        return vec![];
    };
    let mut lines = Vec::new();
    for item in items.flatten() {
        if !item.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let id = item.file_name().to_string_lossy().into_owned();
        if except == Some(id.as_str()) {
            continue;
        }
        let run_dir = item.path();
        if let Ok(meta) = storage::read_json::<RetainedMeta>(&run_dir.join("summary.json")) {
            let run_bytes = fs::metadata(run_dir.join("run.json")).map(|m| m.len());
            let consistent = match (meta.run_bytes, run_bytes) {
                (Some(expected), Ok(actual)) => expected == actual,
                (None, Ok(actual)) => actual > 0,
                (_, Err(_)) => false,
            };
            if meta.id == id && consistent {
                if !meta.worktree && !meta.retained_groups.is_empty() {
                    lines.extend(unproven(&meta.id, &meta.retained_groups));
                }
                continue;
            }
        }
        if let Ok(run) = load(project, &id) {
            if !run.worktree && !run.retained_groups.is_empty() {
                lines.extend(unproven(&run.id, &run.retained_groups));
            }
        }
    }
    lines
}

pub(super) fn refuse(what: &str, alive: &[String]) -> Result<()> {
    if alive.is_empty() {
        return Ok(());
    }
    bail!(
        "Not launching a harness for {what}: a harness an earlier mr started may still be \
         running in its workspace, and recovery could not prove these groups are it:\n{}",
        alive.join("\n")
    )
}

/// Forgets the groups recovery left running for `run` (see `stop_orphans`),
/// without signalling them, once the user has checked they are not its
/// harnesses. Returns them.
pub fn release_groups(run: &mut Run) -> Result<Vec<process::Group>> {
    let _lock = storage::lock_wait(&run.directory.join("run.lock"), LOCK_WAIT)?;
    run.check_revision()?;
    let released = std::mem::take(&mut run.retained_groups);
    if !released.is_empty() {
        if let Err(err) = run.persist() {
            run.retained_groups = released;
            return Err(err);
        }
    }
    Ok(released)
}

/// Stops `group`, recorded by `left_by`, if it is provably still that group,
/// and says what it did. Returns whether it is alive afterwards and may
/// still be that group: one it cannot prove is left running, and so is one
/// whose leader exited on SIGTERM leaving a member that proves nothing (no
/// SIGKILL then) or that outlived SIGKILL.
pub(super) fn stop_recorded(
    group: &process::Group,
    left_by: &str,
    emit: &mut impl FnMut(Event),
) -> bool {
    let pgid = group.pgid;
    match process::group::verify(group) {
        process::Verdict::Gone => false,
        process::Verdict::Ours => {
            emit(Event::Output(format!(
                "Stopping process group {pgid} left running by {left_by}\n"
            )));
            match process::group::end_recorded(group, process::GRACE) {
                process::Verdict::Gone | process::Verdict::Reused => false,
                process::Verdict::Unverified(why) => {
                    emit(Event::Output(format!(
                        "Process group {pgid} is still running after SIGTERM, but {why}, so it \
                         was not killed. If it is the one {left_by} left, stop it with \
                         `kill -KILL -- -{pgid}`\n"
                    )));
                    true
                }
                process::Verdict::Ours => {
                    emit(Event::Output(format!(
                        "Process group {pgid} is still running after SIGKILL\n"
                    )));
                    true
                }
            }
        }
        process::Verdict::Reused => {
            emit(Event::Output(format!(
                "Not stopping process group {pgid}: another process now has that ID\n"
            )));
            false
        }
        process::Verdict::Unverified(why) => {
            emit(Event::Output(format!(
                "Left process group {pgid} running: {why}, so it may not be the one {left_by} \
                 left. If it is, stop it with `kill -TERM -- -{pgid}`\n"
            )));
            true
        }
    }
}

/// Clean up an interrupted execution before checking replay approval. Recovery
/// must stop surviving work even when revocation refuses another launch.
/// Refuses, before any command changes the run, while a harness group it
/// could not stop may still be running (`refuse_unproven`).
pub fn authorize_replay(
    run: &mut Run,
    explicit: Option<&Path>,
    mut emit: impl FnMut(Event),
) -> Result<()> {
    // Its terminal host holds the locks. Reattaching needs no approval to
    // show, stop, or end the harness, and checks it before running the
    // stage's checks; this only lets a selected config approve a run that
    // has no receipt yet.
    if detached_terminal(run).is_some() {
        let _ = crate::trust::authorize_run_config(run, explicit);
        return Ok(());
    }
    let (_lock, _workspace_lock) = lock_run(run)?;
    run.check_revision()?;
    stop_orphans(run, &mut emit)?;
    refuse_unproven(run)?;
    crate::trust::authorize_run_config(run, explicit)
}
