use super::*;

pub(super) struct Prepared {
    pub(super) invocation: Invocation,
    pub(super) sandbox: Option<crate::sandbox::Plan>,
    pub(super) directory: PathBuf,
    pub(super) handoff_file: PathBuf,
    /// The harness ran under an earlier mr's approval (reattached), so the
    /// stage's checks need the run to be approved now.
    pub(super) reattached: bool,
}

pub(super) fn prepare(run: &mut Run) -> Result<Prepared> {
    let index = run.cursor;
    let stage = &run.workflow.stages[index];
    let number = run.stages[index].attempts.len() + 1;
    let directory = run
        .directory
        .join(format!("{:02}-{}", index + 1, stage.id))
        .join(format!("attempt-{number:03}"));
    fs::create_dir_all(&directory)?;
    let handoff_dir = storage::metadata_dir(&run.workspace)?
        .join("handoffs")
        .join(&run.id);
    fs::create_dir_all(&handoff_dir)?;
    let resume = run.resume.clone();
    let handoff_file = match &resume {
        // The resumed session only knows the path its original prompt named;
        // `take_over` moved that attempt's file aside, so whatever is there
        // after this attempt was written by it.
        Some(resume) => resume.handoff_file.clone(),
        None => handoff_dir.join(format!("{}-{number:03}.json", stage.id)),
    };
    ensure!(
        fs::symlink_metadata(&handoff_file).is_err(),
        "Handoff path already exists: {}",
        handoff_file.display()
    );
    let prompt_file = directory.join("prompt.md");
    // The prompt names this copy of the handoff schema.
    let schema_file = directory.join(handoff::SCHEMA_FILE);
    storage::atomic_write_fast(&schema_file, handoff::SCHEMA.as_bytes())?;
    let text = match &resume {
        Some(resume) => format!(
            "Interactive takeover of harness session {}. The session's own history holds the stage prompt. Write a new handoff for this attempt to {}; the previous attempt's file there was moved aside and is not read.\n",
            resume.session,
            handoff_file.display()
        ),
        None => prompt(run, &handoff_file, &schema_file),
    };
    storage::atomic_write_fast(&prompt_file, text.as_bytes())?;
    let mcp = run.mcp.clone();
    let paths = ContextPaths {
        workspace: &run.workspace,
        attempt_dir: &directory,
        prompt_file: &prompt_file,
        handoff_file: &handoff_file,
        mcp: mcp.as_ref(),
    };
    let (invocation, session) = match &resume {
        Some(resume) => (
            adapters::resume(
                stage,
                &run.adapters,
                &run.providers,
                &resume.session,
                &paths,
            )?,
            Some(resume.session.clone()),
        ),
        None => {
            let mut invocation =
                adapters::build(stage, &run.adapters, &run.providers, &text, &paths)?;
            let session = adapters::assign_session(&mut invocation, &stage.harness);
            (invocation, session)
        }
    };
    let mut sandbox = crate::sandbox::plan(
        &run.workflow,
        stage,
        &crate::sandbox::Paths {
            workspace: &run.workspace,
            attempt_dir: &directory,
            handoff_dir: &handoff_dir,
            program: &invocation.program,
        },
    )?;
    if let (Some(plan), Some(mcp)) = (sandbox.as_mut(), &mcp) {
        plan.allow_orch(&mcp.sock, &mcp.token_file, &mcp.program);
    }
    storage::write_json_fast(
        &directory.join("command.json"),
        &serde_json::json!({ "program": invocation.program, "args": invocation.args, "stdin_from": invocation.stdin.as_ref().map(|_| "prompt.md"), "env": invocation.env.iter().map(|(name, _)| name).collect::<Vec<_>>(), "workspace": run.workspace, "model": stage.model, "provider": stage.provider, "effort": stage.effort, "mode": stage.mode, "sandbox": sandbox }),
    )?;
    run.stages[index].attempts.push(Attempt {
        number,
        started_at: Utc::now(),
        finished_at: None,
        model: stage.model.clone(),
        mode: stage.mode,
        status: Status::Pending,
        exit_code: None,
        directory: directory.clone(),
        handoff_file: handoff_file.clone(),
        error: None,
        checks: vec![],
        tree_before: snapshot::tree(&run.workspace, &run.directory),
        tree_after: None,
        checks_tree: None,
        usage: None,
        session,
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
        "Attempt {number} started: {} {} ({})",
        stage.harness,
        stage.model,
        stage.mode.label()
    );
    run.resume = None;
    run.error = None;
    run.set(
        Target::Attempt(index),
        Status::Running,
        Actor::Engine,
        &reason,
    )?;
    run.set(
        Target::Stage(index),
        Status::Running,
        Actor::Engine,
        &reason,
    )?;
    run.set(Target::Run, Status::Running, Actor::Engine, reason)?;
    run.persist()?;
    Ok(Prepared {
        invocation,
        sandbox,
        directory,
        handoff_file,
        reattached: false,
    })
}

/// The first `max` bytes of a log, where harnesses report their session.
pub(super) fn read_head(path: &Path, max: u64) -> Result<String> {
    let mut bytes = Vec::new();
    io::Read::read_to_end(&mut io::Read::take(fs::File::open(path)?, max), &mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// How a harness process ended, before its handoff is read.
pub(super) enum Exit {
    /// The harness succeeded; its output, normalized.
    Succeeded(handoff::Normalized),
    /// The stage stops with this status and message.
    Stopped(Status, String),
}

/// Records the process's end on the attempt and classifies it: cancelled,
/// timed out, failed, denied permissions, a protocol error, or success.
pub(super) fn classify_exit(
    attempt: Option<&mut Attempt>,
    mode: Mode,
    (harness, workspace): (&str, &Path),
    prepared: &Prepared,
    result: Result<process::Outcome>,
) -> Result<Exit> {
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            // The supervisor ended the group before returning its error.
            if let Some(attempt) = attempt {
                attempt.set_group(None);
            }
            return Ok(Exit::Stopped(Status::Failed, format!("{error:#}")));
        }
    };
    let stdout = prepared.directory.join("stdout.log");
    if let Some(attempt) = attempt {
        attempt.exit_code = outcome.code;
        // A scheduler may have recorded when the harness ended already.
        attempt.finished_at.get_or_insert_with(Utc::now);
        // Spend counts whether or not the stage succeeded. Usage events can be
        // spread through the log, so read far more than the final response needs.
        attempt.set_group(None);
        if attempt.mode == Mode::Unattended {
            attempt.usage = handoff::usage_file(&stdout, prepared.invocation.kind);
        } else {
            // Best effort, from the harness's own session log.
            attempt.usage = handoff::session_usage(
                harness,
                attempt.session.as_deref(),
                workspace,
                attempt.started_at,
                Utc::now(),
            );
        }
    }
    evaluate_outcome(mode, prepared, &outcome)
}

fn format_failure_detail(
    dir: &Path,
    kind: adapters::OutputKind,
    code: Option<i32>,
    unattended: bool,
    prefix: &str,
) -> String {
    // A JSON error event names the cause; stderr often holds only warnings.
    let reported = unattended
        .then(|| process::read_tail(&dir.join("stdout.log"), 4 * 1024 * 1024).ok())
        .flatten()
        .and_then(|raw| handoff::normalize(&raw, kind).error);
    let detail = match reported {
        Some(error) => error.to_string(),
        None => storage::clean_terminal(
            &process::read_tail(&dir.join("stderr.log"), 3000).unwrap_or_default(),
        ),
    };
    format!("{prefix} {}. {}", ended(code), detail.trim())
        .trim_end()
        .to_owned()
}

pub(super) fn evaluate_outcome(
    mode: Mode,
    prepared: &Prepared,
    outcome: &process::Outcome,
) -> Result<Exit> {
    if outcome.cancelled {
        return Ok(Exit::Stopped(
            Status::Cancelled,
            "Stage cancelled. Filesystem changes are retained; retry explicitly".into(),
        ));
    }
    if outcome.stalled {
        return Ok(Exit::Stopped(
            Status::Failed,
            "Stage stalled with no output. Inspect logs or reopen interactively; existing changes are retained"
                .into(),
        ));
    }
    if outcome.timed_out {
        return Ok(Exit::Stopped(
            Status::Failed,
            "Stage timed out. Inspect logs or reopen interactively; existing changes are retained"
                .into(),
        ));
    }
    if !outcome.success() {
        return Ok(Exit::Stopped(
            Status::Failed,
            format_failure_detail(
                &prepared.directory,
                prepared.invocation.kind,
                outcome.code,
                mode == Mode::Unattended,
                "Harness",
            ),
        ));
    }
    let raw = process::read_tail(&prepared.directory.join("stdout.log"), 4 * 1024 * 1024)?;
    let normalized = if mode == Mode::Interactive {
        handoff::Normalized {
            text: storage::clean_terminal(&raw),
            ..handoff::Normalized::default()
        }
    } else {
        handoff::normalize(&raw, prepared.invocation.kind)
    };
    if normalized.permission_blocked {
        return Ok(Exit::Stopped(Status::Blocked, "Harness reported denied tool permissions. Reopen interactively or set explicit permission arguments and retry".into()));
    }
    // A handoff file may replace a missing assistant text event, but not a real
    // protocol error (e.g. a tool/API failure after the file was written).
    match &normalized.error {
        Some(ProtocolError::NoFinalResponse)
            if prepared.handoff_file.is_file() || prepared.invocation.final_file.is_file() => {}
        Some(error) => return Ok(Exit::Stopped(Status::Failed, error.to_string())),
        None => {}
    }
    Ok(Exit::Succeeded(normalized))
}

/// A successful attempt's handoff, or why the stage stops without one.
pub(super) enum Resolved {
    Handoff(HandoffSource, Box<Handoff>),
    Stopped(Status, String),
}

/// Reads the handoff from the first source present: the handoff file, the
/// harness's final-message file, then the output stream. An interactive
/// stage that wrote no file gets a placeholder, except reviews and tests,
/// which must produce a structured verdict.
pub(super) fn resolve_handoff(
    stage: &Stage,
    prepared: &Prepared,
    normalized: handoff::Normalized,
) -> Result<Resolved> {
    let (mode, role) = (stage.mode, stage.role);
    // Interactive terminal transcripts can echo the original prompt (including
    // the example JSON). Never infer a structured verdict from an echoed prompt.
    if mode == Mode::Interactive && !prepared.handoff_file.is_file() {
        if matches!(role, Role::Review | Role::Test) {
            return Ok(Resolved::Stopped(
                Status::Blocked,
                format!(
                    "Interactive {} did not write its handoff file. Reopen it to produce a structured verdict and evidence",
                    role.label()
                ),
            ));
        }
        let handoff = Handoff {
            unresolved: vec!["No model-authored handoff was produced".into()],
            ..Handoff::unstructured(format!(
                "Interactive stage exited without a structured handoff. Inspect the terminal transcript at {}. Reopen to produce a handoff, or supply a summary before advancing.",
                prepared.directory.join("stdout.log").display()
            ))
        };
        return Ok(Resolved::Handoff(
            HandoffSource::Placeholder,
            Box::new(handoff),
        ));
    }
    let read = |path: &Path| -> Result<Option<String>> {
        if path.is_file() {
            Ok(Some(process::read_tail(path, 1024 * 1024)?))
        } else {
            Ok(None)
        }
    };
    let file = read(&prepared.handoff_file)?;
    let text = match read(&prepared.invocation.final_file)? {
        Some(text) => text,
        None => normalized.text.clone(),
    };
    let native = adapters::uses_output_schema(&prepared.invocation)
        .then(|| handoff::native_output(&normalized, prepared.invocation.kind, &text))
        .flatten();
    // An interactive transcript echoes the prompt; only its file counts.
    let sources = handoff::Sources {
        native: native.as_deref(),
        file: file.as_deref(),
        text: (mode == Mode::Unattended).then_some(text.as_str()),
    };
    Ok(match handoff::resolve(sources, role) {
        Ok((handoff, source)) => Resolved::Handoff(source, Box::new(handoff)),
        Err(error) => Resolved::Stopped(Status::Blocked, format!("{error:#}")),
    })
}

/// Keeps in `retained_groups` a harness group that the failure in `result`
/// left running (`process::Survivor`), so nothing launches beside it.
pub(super) fn keep_survivor(run: &mut Run, result: &Result<process::Outcome>) {
    let Err(error) = result else {
        return;
    };
    if let Some(survivor) = error.downcast_ref::<process::Survivor>() {
        let group = &survivor.group;
        if run.retained_groups.iter().all(|g| g.pgid != group.pgid) {
            run.retained_groups.push(group.clone());
        }
    }
}

enum RepairOutcome {
    Repaired(HandoffSource, Box<Handoff>),
    Cancelled,
    Unchanged,
}

/// When `workflow.handoff_repair` is enabled and an unattended stage exited 0
/// with missing or malformed handoff JSON, sends one follow-up on the same
/// session asking only for valid handoff JSON (`adapters::follow_up`). Every
/// reason it could not, or did not help, is written to the stage output.
fn try_handoff_repair(
    run: &mut Run,
    index: usize,
    prepared: &Prepared,
    failure: &str,
    cancel: &AtomicBool,
    emit: &mut impl FnMut(Event),
) -> Result<RepairOutcome> {
    let stage = run.workflow.stages[index].clone();
    if !run.workflow.handoff_repair
        || stage.mode != Mode::Unattended
        || cancel.load(Ordering::Relaxed)
    {
        return Ok(RepairOutcome::Unchanged);
    }
    let mut skip = |reason: String| {
        emit(Event::Output(format!("Handoff repair skipped: {reason}\n")));
    };
    if !adapters::has_follow_up(&stage.harness, &run.adapters) {
        skip(format!(
            "{} has no unattended follow-up command for a session",
            stage.harness
        ));
        return Ok(RepairOutcome::Unchanged);
    }
    let session = run.stages[index]
        .attempts
        .last()
        .and_then(|a| a.session.clone())
        .or_else(|| {
            read_head(&prepared.directory.join("stdout.log"), 64 * 1024)
                .ok()
                .and_then(|head| handoff::session_id(&head))
        });
    let Some(session) = session else {
        skip(format!("no session ID recorded for {}", stage.harness));
        return Ok(RepairOutcome::Unchanged);
    };
    let repair_dir = prepared.directory.join("repair");
    fs::create_dir_all(&repair_dir)?;
    let repair_prompt_file = repair_dir.join("prompt.md");
    let schema_path = prepared.directory.join(handoff::SCHEMA_FILE);
    let repair_prompt = format!(
        "Your previous response finished the stage work, but its handoff JSON was missing or invalid ({failure}).\n\
Do not edit any project files or run destructive commands. Emit ONLY a valid handoff JSON matching the required contract for the work you just completed.\n{}",
        handoff::contract(stage.role, &prepared.handoff_file, &schema_path)
    );
    storage::atomic_write_fast(&repair_prompt_file, repair_prompt.as_bytes())?;
    let paths = ContextPaths {
        workspace: &run.workspace,
        attempt_dir: &repair_dir,
        prompt_file: &repair_prompt_file,
        handoff_file: &prepared.handoff_file,
        mcp: None,
    };
    let invocation = match adapters::follow_up(
        &stage,
        &run.adapters,
        &run.providers,
        &session,
        &repair_prompt,
        &paths,
    ) {
        Ok(invocation) => invocation,
        Err(error) => {
            skip(format!("{error:#}"));
            return Ok(RepairOutcome::Unchanged);
        }
    };
    emit(Event::Output(format!(
        "── {} · handoff repair nudge (session {session}) ──\n",
        stage.name
    )));
    let timeout = Duration::from_secs(stage.timeout_seconds.min(120));
    let stall = stage
        .stall_timeout_seconds
        .filter(|_| adapters::streams(&stage.harness, &run.adapters))
        .map(|s| Duration::from_secs(s.min(120)));
    let workspace = run.workspace.clone();
    // The first harness has ended; save that before the follow-up starts, so
    // the record never names its finished group while the follow-up runs.
    if let Some(attempt) = run.stages[index].attempts.last_mut() {
        attempt.set_group(None);
    }
    run.persist()?;
    // Recorded like the stage's own harness, so a later drive can stop the
    // follow-up's group if mr dies meanwhile.
    // The repair gets no delegation tools: a delegating turn's broker is
    // closed by now, and its socket and token are gone.
    let sandbox = prepared.sandbox.clone().map(|mut plan| {
        plan.orch = None;
        plan
    });
    let outcome = process::capture_with_stall(
        &invocation,
        &workspace,
        &repair_dir,
        timeout,
        stall,
        cancel,
        sandbox.as_ref(),
        |group| {
            if let Some(attempt) = run.stages[index].attempts.last_mut() {
                attempt.set_group(Some(group));
            }
            run.persist()
        },
        |err, text| {
            if err {
                emit(Event::Output(format!(
                    "[stderr] {}",
                    storage::clean_terminal(text)
                )));
            }
        },
    );
    // A group the supervisor could not end stays in `retained_groups`.
    keep_survivor(run, &outcome);
    if let Some(attempt) = run.stages[index].attempts.last_mut() {
        attempt.set_group(None);
    }
    run.persist()?;
    let mut failed = |reason: String| {
        emit(Event::Output(format!("Handoff repair failed: {reason}\n")));
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            failed(format!("{error:#}"));
            return Ok(RepairOutcome::Unchanged);
        }
    };
    let repair_stdout = repair_dir.join("stdout.log");
    if let Some(extra_usage) = handoff::usage_file(&repair_stdout, invocation.kind) {
        if let Some(attempt) = run.stages[index].attempts.last_mut() {
            attempt.usage = Some(match attempt.usage {
                Some(existing) => existing + extra_usage,
                None => extra_usage,
            });
        }
    }
    if outcome.cancelled {
        failed("cancelled".into());
        return Ok(RepairOutcome::Cancelled);
    }
    if outcome.stalled || outcome.timed_out {
        failed(format!(
            "the follow-up {}",
            if outcome.stalled {
                "stalled with no output"
            } else {
                "timed out"
            }
        ));
        return Ok(RepairOutcome::Unchanged);
    }
    if !outcome.success() {
        failed(format_failure_detail(
            &repair_dir,
            invocation.kind,
            outcome.code,
            true,
            "the follow-up",
        ));
        return Ok(RepairOutcome::Unchanged);
    }
    let raw = process::read_tail(&repair_stdout, 2 * 1024 * 1024).unwrap_or_default();
    let normalized = handoff::normalize(&raw, invocation.kind);
    let repair_prepared = Prepared {
        invocation,
        sandbox: prepared.sandbox.clone(),
        directory: repair_dir,
        handoff_file: prepared.handoff_file.clone(),
        reattached: false,
    };
    match resolve_handoff(&stage, &repair_prepared, normalized)? {
        Resolved::Handoff(source, handoff) => Ok(RepairOutcome::Repaired(source, handoff)),
        Resolved::Stopped(_, message) => {
            failed(format!(
                "the follow-up produced no valid handoff: {message}"
            ));
            Ok(RepairOutcome::Unchanged)
        }
    }
}

/// Completes the attempt's bookkeeping after its harness ended, returning any
/// fresh workspace snapshot already captured when no checks invalidated it.
pub(super) fn finish(
    run: &mut Run,
    prepared: &Prepared,
    result: Result<process::Outcome>,
    cancel: &AtomicBool,
    emit: &mut impl FnMut(Event),
) -> Result<Option<String>> {
    let index = run.cursor;
    let mode = run.workflow.stages[index].mode;
    let harness = run.workflow.stages[index].harness.clone();
    let workspace = run.workspace.clone();
    // Before `classify_exit` forgets the attempt's group and the caller
    // saves the run and lets go of the workspace.
    keep_survivor(run, &result);
    let attempt = run.stages[index].attempts.last_mut();
    let normalized = match classify_exit(attempt, mode, (&harness, &workspace), prepared, result)? {
        Exit::Succeeded(normalized) => normalized,
        Exit::Stopped(status, message) => {
            run.stop(status, message, Actor::Engine, true)?;
            return Ok(None);
        }
    };
    let (source, handoff) = match resolve_handoff(
        &run.workflow.stages[index],
        prepared,
        normalized,
    )? {
        Resolved::Handoff(source, handoff) => (source, *handoff),
        Resolved::Stopped(status, message) => {
            match try_handoff_repair(run, index, prepared, &message, cancel, emit)? {
                RepairOutcome::Repaired(source, handoff) => (source, *handoff),
                RepairOutcome::Cancelled => {
                    run.stop(
                        Status::Cancelled,
                        "Stage cancelled during handoff repair. Filesystem changes are retained; retry explicitly"
                            .into(),
                        Actor::Engine,
                        true,
                    )?;
                    return Ok(None);
                }
                RepairOutcome::Unchanged => {
                    run.stop(status, message, Actor::Engine, true)?;
                    return Ok(None);
                }
            }
        }
    };
    if let Some(attempt) = run.stages[index].attempts.last_mut() {
        attempt.handoff_source = Some(source);
    }
    storage::write_json(&prepared.directory.join("handoff.json"), &handoff)?;
    if run.workflow.stages[index].role == Role::Summary && source != HandoffSource::Placeholder {
        // The brief is for a person: keep it readable outside the router.
        storage::atomic_write(
            &run.directory.join("review.md"),
            format!("{}\n", handoff.summary.trim_end()).as_bytes(),
        )?;
    }
    // A reported block stops the stage regardless of check results.
    let blocked = handoff.verdict == Verdict::Blocked;
    run.stages[index].handoff = Some(handoff);
    let mut fresh_tree: Option<String> = None;
    if !run.workflow.protected_paths.is_empty() {
        let workspace = run.workspace.clone();
        let Some(before) = run.stages[index]
            .attempts
            .last()
            .and_then(|a| a.tree_before.clone())
        else {
            run.stop(
                Status::Blocked,
                "Could not verify workflow protected_paths: no pre-stage Git snapshot was recorded"
                    .into(),
                Actor::Engine,
                true,
            )?;
            return Ok(None);
        };
        let Some(after) = snapshot::tree(&workspace, &run.directory) else {
            run.stop(
                Status::Blocked,
                "Could not snapshot workspace to verify workflow protected_paths; inspect workspace and retry"
                    .into(),
                Actor::Engine,
                true,
            )?;
            return Ok(None);
        };
        fresh_tree = Some(after.clone());
        if after != before {
            let changed = match snapshot::changed_paths(&workspace, &before, &after) {
                Ok(paths) => paths,
                Err(error) => {
                    run.stop(
                        Status::Blocked,
                        format!(
                            "Could not diff workspace to verify workflow protected_paths: {error:#}"
                        ),
                        Actor::Engine,
                        true,
                    )?;
                    return Ok(fresh_tree);
                }
            };
            if let Some(detail) =
                crate::policy::protected_detail(&run.workflow.protected_paths, &changed)
            {
                run.stop(
                    Status::Blocked,
                    format!(
                        "Stage modified protected path(s): {detail}. Revert them or update workflow protected_paths before retrying"
                    ),
                    Actor::Engine,
                    true,
                )?;
                return Ok(fresh_tree);
            }
        }
    }
    if !blocked {
        let existing = (!run.workflow.stages[index].checks.is_empty())
            .then(|| fresh_tree.take())
            .flatten();
        if !run_checks(run, prepared, existing, cancel, emit)? {
            return Ok(None);
        }
    }
    if run.workflow.stages[index].mode == Mode::Interactive {
        run.checkpoint()?;
    } else {
        run.advance(Actor::Engine)?;
    }
    Ok(fresh_tree)
}

/// Runs the current stage's router checks. False when cancellation stopped the run.
pub(super) fn run_checks(
    run: &mut Run,
    prepared: &Prepared,
    existing_tree: Option<String>,
    cancel: &AtomicBool,
    emit: &mut impl FnMut(Event),
) -> Result<bool> {
    let index = run.cursor;
    let configured = run.workflow.stages[index].checks.clone();
    if configured.is_empty() {
        return Ok(true);
    }
    if prepared.reattached {
        if let Err(error) = crate::trust::authorize_run(run) {
            run.stop(
                Status::Failed,
                format!("The harness ended, but the stage's checks did not run: {error:#}"),
                Actor::Engine,
                true,
            )?;
            return Ok(false);
        }
    }
    let workspace = run.workspace.clone();
    // The receipt: exactly what the checks are about to test.
    let tested = existing_tree.or_else(|| snapshot::tree(&workspace, &run.directory));
    if let Some(attempt) = run.stages[index].attempts.last_mut() {
        attempt.checks_tree = tested;
    }
    let (results, cancelled) = checks::run(
        &configured,
        &workspace,
        &prepared.directory,
        cancel,
        |group| {
            let is_some = group.is_some();
            run.stages[index]
                .attempts
                .last_mut()
                .context("No attempt")?
                .set_group(group);
            if is_some {
                run.persist()?;
            }
            Ok(())
        },
        |text| emit(Event::Output(text)),
    )?;
    if let Some(attempt) = run.stages[index].attempts.last_mut() {
        attempt.checks = results;
    }
    if cancelled {
        run.stop(
            Status::Cancelled,
            "Stage cancelled during checks. Filesystem changes are retained; retry explicitly"
                .into(),
            Actor::Engine,
            true,
        )?;
    }
    Ok(!cancelled)
}

/// Prepares the current stage's attempt and announces it; None when that
/// failed and stopped the stage.
pub(super) fn start_stage(run: &mut Run, emit: &mut impl FnMut(Event)) -> Result<Option<Prepared>> {
    let index = run.cursor;
    let attempts = run.stages[index].attempts.len();
    let prepared = match prepare(run) {
        Ok(p) => p,
        Err(e) => {
            // Errors before the attempt was recorded belong to the stage,
            // never to an earlier, finished attempt.
            let recorded = run.stages[index].attempts.len() > attempts;
            run.stop(Status::Failed, format!("{e:#}"), Actor::Engine, recorded)?;
            run.persist()?;
            emit(Event::State(Box::new(run.clone())));
            return Ok(None);
        }
    };
    emit(Event::State(Box::new(run.clone())));
    let stage = &run.workflow.stages[index];
    emit(Event::Output(format!(
        "── {} · attempt {} · {}{}{} ──\n",
        stage.name,
        run.stages[index].attempts.len(),
        stage.harness,
        if stage.model.is_empty() { "" } else { " " },
        stage.model
    )));
    Ok(Some(prepared))
}

/// Finishes the current stage's attempt after its harness ended, records
/// the workspace snapshot, and saves.
pub(super) fn conclude(
    run: &mut Run,
    prepared: &Prepared,
    result: Result<process::Outcome>,
    cancel: &AtomicBool,
    emit: &mut impl FnMut(Event),
) -> Result<()> {
    let index = run.cursor;
    let fresh_tree = match finish(run, prepared, result, cancel, emit) {
        Ok(tree) => tree,
        Err(error) => {
            run.stop(
                Status::Failed,
                format!("Could not complete stage bookkeeping: {error:#}"),
                Actor::Engine,
                true,
            )?;
            None
        }
    };
    // Finishing may move the cursor, so record the stage that ran (unless
    // `Decision::Select` already recorded a post-apply snapshot).
    if run.stages[index]
        .attempts
        .last()
        .is_some_and(|a| a.tree_after.is_none())
    {
        let tree = fresh_tree.or_else(|| snapshot::tree(&run.workspace, &run.directory));
        if let Some(attempt) = run.stages[index].attempts.last_mut() {
            attempt.tree_after = tree;
        }
    }
    run.persist()?;
    emit(Event::State(Box::new(run.clone())));
    Ok(())
}

/// An interactive stage gets its session and the run's held locks, which go
/// to its terminal host if the stage is left running.
pub(super) fn execute_stage(
    run: &mut Run,
    cancel: &AtomicBool,
    interactive: Option<(&process::InteractiveSession, &[&storage::FileLock])>,
    emit: &mut impl FnMut(Event),
) -> Result<()> {
    let index = run.cursor;
    let Some(prepared) = start_stage(run, emit)? else {
        return Ok(());
    };
    let workspace = run.workspace.clone();
    // Recorded so a later drive can stop the group if mr dies meanwhile.
    let result = if let Some((session, held)) = interactive {
        let locks = lock_paths(run)?;
        let result = process::interactive(
            &prepared.invocation,
            &workspace,
            &prepared.directory,
            cancel,
            session,
            prepared.sandbox.as_ref(),
            &locks,
            |group, socket| {
                if let Some(attempt) = run.stages[index].attempts.last_mut() {
                    attempt.terminal = Some(socket.to_path_buf());
                    attempt.set_group(Some(group.clone()));
                }
                if run.persist().is_err() {
                    let _ = SpawnJournal::new(run).add(&group);
                }
            },
        );
        if left_running(run, &result, session, held, emit)? {
            return Ok(());
        }
        result
    } else {
        // Events become readable lines; stdout.log keeps them raw.
        let mut formatter = events::Formatter::new(prepared.invocation.kind);
        let timeout = Duration::from_secs(run.workflow.stages[index].timeout_seconds);
        // Config validation rejects a stall timeout on a harness that
        // prints only at exit; a run whose harness was replaced since never
        // applies one.
        let stall_timeout = run.workflow.stages[index]
            .stall_timeout_seconds
            .filter(|_| adapters::streams(&run.workflow.stages[index].harness, &run.adapters))
            .map(Duration::from_secs);
        let result = process::capture_with_stall(
            &prepared.invocation,
            &workspace,
            &prepared.directory,
            timeout,
            stall_timeout,
            cancel,
            prepared.sandbox.as_ref(),
            |group| {
                if let Some(attempt) = run.stages[index].attempts.last_mut() {
                    attempt.set_group(Some(group));
                }
                run.persist()
            },
            |err, text| {
                if err {
                    emit(Event::Output(format!(
                        "[stderr] {}",
                        storage::clean_terminal(text)
                    )));
                } else if let Some(lines) = formatter.push(text) {
                    emit(Event::Output(lines));
                }
            },
        );
        if let Some(rest) = formatter.finish() {
            emit(Event::Output(rest));
        }
        result
    };
    conclude(run, &prepared, result, cancel, emit)
}

/// True when the interactive harness was released to its terminal host and
/// keeps running: the attempt stays running for a later mr to reattach.
/// The run is saved before its locks go to the terminal host with the
/// connection: the host may hand them to another mr at once, and this one
/// must not write the run after that.
pub(super) fn left_running(
    run: &mut Run,
    result: &Result<process::Outcome>,
    session: &process::InteractiveSession,
    held: &[&storage::FileLock],
    emit: &mut impl FnMut(Event),
) -> Result<bool> {
    if !matches!(result, Ok(outcome) if outcome.released) {
        return Ok(false);
    }
    let saved = run.persist();
    session.hand_over(held);
    saved?;
    emit(Event::Output(format!(
        "{} keeps running in the background; mr reattaches to it\n",
        run.current_name()
    )));
    emit(Event::State(Box::new(run.clone())));
    Ok(true)
}

/// The terminal host socket of the current stage's interactive harness when
/// it outlived the mr that started it, to reattach to: the attempt is still
/// running and its host is listening or recorded how the harness ended.
pub fn detached_terminal(run: &Run) -> Option<PathBuf> {
    if run.status != Status::Running {
        return None;
    }
    let stage = run.workflow.stages.get(run.cursor)?;
    if stage.mode != Mode::Interactive || stage.fan_out.is_some() {
        return None;
    }
    let attempt = run.stages.get(run.cursor)?.attempts.last()?;
    // run.json names the socket, which is sent keystrokes, or removed once
    // its host is gone: anything but a terminal host's socket (or one from a
    // socket directory this mr does not use) leaves the attempt interrupted.
    let socket = attempt
        .terminal
        .as_ref()
        .filter(|socket| process::host::is_socket_path(socket))?;
    (attempt.status == Status::Running
        && (socket.exists() || process::recorded_exit(&attempt.directory).is_some()))
    .then(|| socket.clone())
}

/// The locks `drive` takes, in order; a terminal host holds them while no
/// mr is attached.
pub(super) fn lock_paths(run: &Run) -> Result<Vec<PathBuf>> {
    Ok(vec![
        run.directory.join("run.lock"),
        storage::metadata_dir(&run.workspace)?.join("workspace.lock"),
    ])
}

/// Rebuilds what finishing the running attempt needs, without launching or
/// recording anything; for an attempt whose harness outlived its mr.
pub(super) fn reprepare(run: &Run) -> Result<Prepared> {
    let index = run.cursor;
    let stage = &run.workflow.stages[index];
    let attempt = run.stages[index].attempts.last().context("No attempt")?;
    let directory = attempt.directory.clone();
    let prompt_file = directory.join("prompt.md");
    let text = fs::read_to_string(&prompt_file)
        .with_context(|| format!("Read {}", prompt_file.display()))?;
    let paths = ContextPaths {
        workspace: &run.workspace,
        attempt_dir: &directory,
        prompt_file: &prompt_file,
        handoff_file: &attempt.handoff_file,
        mcp: None,
    };
    let invocation = adapters::build(stage, &run.adapters, &run.providers, &text, &paths)?;
    Ok(Prepared {
        invocation,
        sandbox: None,
        directory,
        handoff_file: attempt.handoff_file.clone(),
        reattached: true,
    })
}

/// Reconnects to the current stage's harness in its terminal host, or reads
/// how it ended while no mr was attached, and finishes the stage.
pub(super) fn reattach_stage(
    run: &mut Run,
    socket: &Path,
    cancel: &AtomicBool,
    session: &process::InteractiveSession,
    emit: &mut impl FnMut(Event),
) -> Result<()> {
    let index = run.cursor;
    let directory = run.stages[index]
        .attempts
        .last()
        .context("No attempt")?
        .directory
        .clone();
    // Attach first: the host hands over the locks it held, so they are never
    // free. One that held none (it ended, or its mr vanished moments ago)
    // has let go of them by now.
    let expected = lock_paths(run)?;
    let mut terminal = process::Terminal::connect(socket);
    let handed = match &mut terminal {
        Ok(terminal) => terminal.take_locks(),
        Err(_) => Vec::new(),
    };
    // run.json names the socket: a host that hands over some other run's
    // locks gets them back, and its harness is not this stage's.
    let ours = handed.len() == expected.len()
        && handed
            .iter()
            .zip(&expected)
            .all(|(lock, path)| lock.is(path));
    if !handed.is_empty() && !ours {
        if let Ok(terminal) = &terminal {
            terminal.hand_over(&[], &handed.iter().collect::<Vec<_>>());
        }
        bail!(
            "The recorded terminal {} belongs to another run; this run's record is invalid",
            socket.display()
        );
    }
    let locks: [storage::FileLock; 2] = match handed.try_into() {
        Ok(locks) => locks,
        Err(_) => {
            let (run_lock, workspace_lock) = lock_run(run)?;
            [run_lock, workspace_lock]
        }
    };
    // Until the harness is reconnected, a failure gives the locks back to
    // its host instead of freeing them while it still runs.
    if let Err(error) = run.check_revision().and_then(|()| stop_orphans(run, emit)) {
        if let Ok(terminal) = &terminal {
            terminal.hand_over(&[], &locks.each_ref());
        }
        return Err(error);
    }
    let result = match terminal {
        Ok(mut terminal) => {
            let mut updated = false;
            if let Some(attempt) = run.stages[index].attempts.last_mut() {
                let pid = terminal.pid();
                // Only the host, the harness's parent, can read its start
                // time before reaping it; one that does not report it
                // leaves it unknown, never read here once the PID may name
                // another process.
                if attempt.process_group != Some(pid) {
                    attempt.set_group(Some(process::Group {
                        pgid: pid,
                        identity: process::Identity {
                            started: terminal.started(),
                            marker: None,
                        },
                    }));
                    updated = true;
                } else if attempt.process_identity.started.is_none() {
                    attempt.process_identity.started = terminal.started();
                    updated = attempt.process_identity.started.is_some();
                }
                terminal.set_marker(attempt.process_identity.marker.clone());
            }
            if updated {
                if let Err(error) = run.persist() {
                    terminal.hand_over(&[], &locks.each_ref());
                    return Err(error);
                }
            }
            emit(Event::Output(format!(
                "Reattached to {} in its terminal\n",
                run.current_name()
            )));
            process::reconnect(terminal, &directory, cancel, session)
        }
        Err(error) => {
            session.finish();
            match process::recorded_exit(&directory) {
                Some(outcome) => Ok(outcome),
                None => {
                    // Its host died without recording an exit: stop whatever
                    // it left, and forget the dead socket.
                    if let Some(group) = run.stages[index].attempts.last().and_then(|a| a.group()) {
                        if stop_recorded(&group, "its terminal host", emit)
                            && run.retained_groups.iter().all(|g| g.pgid != group.pgid)
                        {
                            run.retained_groups.push(group);
                        }
                    }
                    // Only ever a socket: run.json names this path.
                    if fs::symlink_metadata(socket)
                        .is_ok_and(|m| std::os::unix::fs::FileTypeExt::is_socket(&m.file_type()))
                    {
                        let _ = fs::remove_file(socket);
                    }
                    Err(error.context("The harness terminal from an earlier mr is gone"))
                }
            }
        }
    };
    if left_running(run, &result, session, &locks.each_ref(), emit)? {
        return Ok(());
    }
    // The harness ran under an earlier approval, so watching, stopping, or
    // letting it end needs none; its checks run only with one now (see
    // `run_checks`).
    let prepared = match reprepare(run) {
        Ok(prepared) => prepared,
        Err(error) => {
            keep_survivor(run, &result);
            if let Some(attempt) = run.stages[index].attempts.last_mut() {
                attempt.set_group(None);
            }
            run.stop(
                Status::Failed,
                format!("Could not finish the reattached stage: {error:#}"),
                Actor::Engine,
                true,
            )?;
            run.persist()?;
            emit(Event::State(Box::new(run.clone())));
            return Ok(());
        }
    };
    conclude(run, &prepared, result, cancel, emit)?;
    if matches!(
        run.status,
        Status::Failed | Status::Blocked | Status::Cancelled
    ) {
        save_changes(run);
    }
    Ok(())
}

/// A candidate attempt's private Git index for snapshots: beside its attempt
/// directory, not in it, since the candidate could have left a link there
/// that the router's Git would write through.
pub(super) fn candidate_index(prepared: &Prepared) -> PathBuf {
    prepared.directory.with_extension("index")
}

/// How a harness process ended, for messages: its exit code, or a signal
/// when it has none.
fn ended(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("exited with code {code}"),
        None => "was stopped by a signal".to_owned(),
    }
}

pub(super) const INTERRUPTED: &str =
    "A previous execution was interrupted. Inspect artifacts and retry explicitly";
