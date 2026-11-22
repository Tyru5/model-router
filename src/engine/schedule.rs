use super::*;

/// Auto judge: a complete candidate whose required checks passed, then the
/// fewest changed lines, then the first listed.
pub(super) fn auto_pick(state: &FanOutState) -> Option<usize> {
    state
        .candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| c.status == Status::Complete)
        .min_by_key(|(index, c)| {
            (
                !c.checks_pass(),
                c.changes.as_ref().map_or(u64::MAX, CandidateChanges::lines),
                *index,
            )
        })
        .map(|(index, _)| index)
}

/// What the judge, or a candidate, needs to know beyond the stage prompt.
/// A harness process the scheduler runs: a stage attempt or a candidate's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Job {
    Stage(usize),
    Candidate(usize, usize),
    RetryWait(usize),
}
pub(super) enum Message {
    Spawned(Job, process::Group),
    /// Formatted lines, as the worker's formatter produced them.
    Output(Job, String),
    /// A stage supervisor's result, live usage, optional mid-stream budget stop, and end time.
    StageDone(
        usize,
        Result<process::Outcome>,
        Option<handoff::Usage>,
        Option<String>,
        DateTime<Utc>,
    ),
    /// A transient retry backoff delay finished (or was interrupted).
    RetryReady(usize, bool),
    /// A candidate worker's post-processed result and when the harness ended.
    CandidateDone(usize, usize, Box<Result<run::CandidatePost>>, DateTime<Utc>),
}
/// What the scheduler starts next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Launch {
    Stage(usize),
    FanOut(usize),
    Candidate(usize, usize),
    Interactive(usize),
    Ship(usize),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Next {
    Interactive(usize),
    Ship(usize),
}

struct StageJob {
    index: usize,
    invocation: Invocation,
    sandbox: Option<crate::sandbox::Plan>,
    directory: PathBuf,
    cwd: PathBuf,
    live_budget: Option<crate::cost::LiveBudgetMonitor>,
    timeout: Duration,
}

struct CandidateJob {
    index: usize,
    c: usize,
    prepared: Prepared,
    cwd: PathBuf,
    worktree: PathBuf,
    candidate: Stage,
    base: String,
    run_id: String,
    protected_paths: Vec<String>,
    policies: Vec<crate::policy::DiffPolicy>,
}

enum WorkerJob {
    Stage(Box<StageJob>),
    Candidate(Box<CandidateJob>),
}

/// The next thing to start, in list order, and a writer stage waiting to
/// run alone. Stages start only when every stage they run after is done;
/// parallel-safe ones run together up to `max_parallel` processes, others
/// alone, and interactive or ship ones alone. `only` limits it to that
/// fan-out stage's candidates.
pub(super) fn next_launch(run: &Run, only: Option<usize>) -> (Option<Launch>, Option<usize>) {
    let stages = &run.workflow.stages;
    let max = run.workflow.max_parallel.max(1) as usize;
    let mut flight = 0;
    let mut writer = false;
    for (index, (stage, state)) in stages.iter().zip(&run.stages).enumerate() {
        if let Some(fan) = &state.fan_out {
            flight += fan
                .candidates
                .iter()
                .filter(|c| c.status == Status::Running)
                .count();
        }
        if state.status == Status::Running && stage.fan_out.is_none() {
            flight += 1;
            writer |= !graph::parallel_safe(&run.workflow, index);
        }
    }
    let pending = |index: usize| {
        run.stages[index].fan_out.as_ref().and_then(|f| {
            f.candidates
                .iter()
                .position(|c| c.status == Status::Pending)
        })
    };
    if let Some(index) = only {
        return (
            pending(index)
                .filter(|_| flight < max)
                .map(|c| Launch::Candidate(index, c)),
            None,
        );
    }
    if writer {
        return (None, None);
    }
    let order = run.order();
    for (index, stage) in stages.iter().enumerate() {
        let status = run.stages[index].status;
        if !stage.enabled {
            continue;
        }
        if stage.fan_out.is_some() && status == Status::Running {
            if let Some(c) = pending(index).filter(|_| flight < max) {
                return (Some(Launch::Candidate(index, c)), None);
            }
            continue;
        }
        let ready = status == Status::Pending
            && (0..stages.len()).all(|before| {
                !order.precedes(before, index)
                    || matches!(
                        run.stages[before].status,
                        Status::Complete | Status::Skipped
                    )
            });
        if !ready {
            continue;
        }
        if stage.role == Role::Ship {
            return if flight == 0 {
                (Some(Launch::Ship(index)), None)
            } else {
                (None, Some(index))
            };
        }
        if stage.mode == Mode::Interactive {
            return ((flight == 0).then_some(Launch::Interactive(index)), None);
        }
        if graph::parallel_safe(&run.workflow, index) {
            if flight >= max {
                return (None, None);
            }
            return (
                Some(if stage.fan_out.is_some() {
                    Launch::FanOut(index)
                } else {
                    Launch::Stage(index)
                }),
                None,
            );
        }
        return if flight == 0 {
            (Some(Launch::Stage(index)), None)
        } else {
            (None, Some(index))
        };
    }
    (None, None)
}

/// Process groups scheduler workers started and the run has not recorded
/// yet. Only the scheduler thread saves the run, and it can be busy running
/// checks for a long time, so each worker writes its group here the moment
/// it exists; a crash meanwhile still leaves it for `stop_orphans`. One
/// group per line, as JSON with its identity; a bare number in journals
/// written before groups were identified.
pub(super) struct SpawnJournal {
    pub(super) path: PathBuf,
    pub(super) lock: std::sync::Mutex<()>,
}
impl SpawnJournal {
    pub(super) fn new(run: &Run) -> Self {
        Self {
            path: run.directory.join("spawned-groups"),
            lock: std::sync::Mutex::new(()),
        }
    }
    pub(super) fn read(&self) -> Vec<process::Group> {
        fs::read_to_string(&self.path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line.trim()).ok())
            .collect()
    }
    pub(super) fn add(&self, group: &process::Group) -> Result<()> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let line = format!("{}\n", serde_json::to_string(group)?);
        io::Write::write_all(&mut file, line.as_bytes())?;
        file.sync_data()?;
        Ok(())
    }
    /// Forgets `pgid` once the run has saved it or the group has ended.
    pub(super) fn recorded(&self, pgid: u32) {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let rest: Vec<String> = self
            .read()
            .into_iter()
            .filter(|g| g.pgid != pgid)
            .filter_map(|g| serde_json::to_string(&g).ok())
            .collect();
        if rest.is_empty() {
            let _ = fs::remove_file(&self.path);
        } else {
            let _ = storage::atomic_write(&self.path, format!("{}\n", rest.join("\n")).as_bytes());
        }
    }
    /// Forgets every group, once they are stopped and the run is saved.
    pub(super) fn clear(&self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Runs stages and candidates side by side until none can start and none
/// runs, then settles the run. Each process has its own process group and
/// logs; workers only supervise processes and report over a channel, so
/// only this thread changes or saves the run. Returns an interactive stage
/// that should run next, and how many processes or rounds started.
pub(super) fn schedule(
    run: &mut Run,
    cancel: &AtomicBool,
    emit: &mut impl FnMut(Event),
    only: Option<usize>,
) -> Result<(Option<Next>, usize)> {
    let mut next = None;
    let mut started = 0;
    let mut noted = vec![];
    let halted = |run: &Run| {
        cancel.load(Ordering::Relaxed)
            || run
                .hold
                .as_ref()
                .is_some_and(|h| h.repair.is_some() || !h.stops.is_empty())
            || run
                .stages
                .iter()
                .any(|s| s.status == Status::AwaitingConfirmation)
    };
    let abort = AtomicBool::new(false);
    let abort = &abort;
    let journal = SpawnJournal::new(run);
    let journal = &journal;
    let result = thread::scope(|scope| -> Result<()> {
        let (tx, rx) = mpsc::channel::<Message>();
        let result = (|| -> Result<()> {
            let tx = &tx;
            let mut flight: Vec<(Job, Option<Prepared>)> = vec![];
            let mut deferred_settle: Vec<usize> = vec![];
            let mut labels = std::collections::HashMap::new();
            let mut speaking = None;
            let mut stage_retries: std::collections::HashMap<usize, (u32, std::time::Instant)> =
                std::collections::HashMap::new();
            let mut retry_reason: std::collections::HashMap<usize, String> =
                std::collections::HashMap::new();
            loop {
                while !halted(run) && deferred_settle.is_empty() {
                    let (launch, waiting) = next_launch(run, only);
                    if let Some(index) = waiting.filter(|i| !noted.contains(i)) {
                        noted.push(index);
                        emit(Event::Output(format!(
                            "Stage {} changes the shared workspace: it waits for the running stages, then runs alone\n",
                            run.workflow.stages[index].name
                        )));
                    }
                    let Some(launch) = launch else {
                        break;
                    };
                    if let Launch::Stage(index)
                    | Launch::FanOut(index)
                    | Launch::Interactive(index)
                    | Launch::Ship(index) = launch
                    {
                        if let Some(reason) = run.should_skip_when_changed(index) {
                            let name = run.workflow.stages[index].name.clone();
                            run.stages[index].skipped_reason = Some(reason.clone());
                            run.set(
                                Target::Stage(index),
                                Status::Skipped,
                                Actor::Engine,
                                &reason,
                            )?;
                            run.persist()?;
                            emit(Event::Output(format!("── {name} · {reason} ──\n")));
                            emit(Event::State(Box::new(run.clone())));
                            started += 1;
                            continue;
                        }
                        // As in `drive`, a cap stops or warns before another stage
                        // starts; stages already running finish.
                        match crate::cost::check_budget_action(run) {
                            crate::cost::BudgetCheck::Ok => {}
                            crate::cost::BudgetCheck::Warn(warning) => {
                                emit(Event::Output(format!("{warning}\n")));
                            }
                            crate::cost::BudgetCheck::Stop(reason) => {
                                run.cursor = index;
                                run.stop(Status::Blocked, reason, Actor::Engine, false)?;
                                run.persist()?;
                                emit(Event::State(Box::new(run.clone())));
                                break;
                            }
                        }
                    }
                    started += 1;
                    let (job, flight_prepared, worker_job) = match launch {
                        Launch::Interactive(index) => {
                            next = Some(Next::Interactive(index));
                            break;
                        }
                        Launch::Ship(index) => {
                            next = Some(Next::Ship(index));
                            break;
                        }
                        Launch::FanOut(index) => {
                            run.cursor = index;
                            let attempts = run.stages[index].attempts.len();
                            if let Err(error) = run.start_fan_out(index) {
                                let recorded = run.stages[index].attempts.len() > attempts;
                                run.stop(
                                    Status::Failed,
                                    format!("{error:#}"),
                                    Actor::Engine,
                                    recorded,
                                )?;
                                run.persist()?;
                            }
                            emit(Event::State(Box::new(run.clone())));
                            continue;
                        }
                        // A delegating stage runs alone in its own owner loop.
                        Launch::Stage(index) if run.workflow.stages[index].delegation.is_some() => {
                            run.cursor = index;
                            super::delegate::drive_stage(run, cancel, None, emit)?;
                            emit(Event::State(Box::new(run.clone())));
                            continue;
                        }
                        Launch::Stage(index) => {
                            run.cursor = index;
                            let entry = stage_retries
                                .entry(index)
                                .or_insert_with(|| (0, std::time::Instant::now()));
                            let first_attempt = entry.0 == 0;
                            let stage_started = entry.1;
                            let stage_timeout =
                                Duration::from_secs(run.workflow.stages[index].timeout_seconds);
                            let remaining_timeout = stage_timeout
                                .saturating_sub(stage_started.elapsed())
                                .max(Duration::from_secs(1));
                            match start_stage_inner(run, cancel, first_attempt, emit)? {
                                Some(prepared) => {
                                    let live_budget =
                                        crate::cost::LiveBudgetMonitor::for_stage(run, index);
                                    let worker = WorkerJob::Stage(Box::new(StageJob {
                                        index,
                                        invocation: prepared.invocation.clone(),
                                        sandbox: prepared.sandbox.clone(),
                                        directory: prepared.directory.clone(),
                                        cwd: run.workspace.clone(),
                                        live_budget,
                                        timeout: remaining_timeout,
                                    }));
                                    (Job::Stage(index), Some(prepared), worker)
                                }
                                None => continue,
                            }
                        }
                        Launch::Candidate(index, c) => match run.launch_candidate(index, c) {
                            Ok((prepared, cwd)) => {
                                emit(Event::State(Box::new(run.clone())));
                                let stage = &run.workflow.stages[index];
                                let candidate = stage
                                    .fan_out
                                    .as_ref()
                                    .and_then(|f| f.candidates.get(c))
                                    .context("No such candidate")?
                                    .stage(stage);
                                let fan_state =
                                    run.stages[index].fan_out.as_ref().context("No fan-out")?;
                                let base = fan_state.base.clone();
                                let worktree = fan_state.candidates[c]
                                    .attempts
                                    .last()
                                    .and_then(|a| a.worktree.clone())
                                    .context("No worktree")?;
                                let worker = WorkerJob::Candidate(Box::new(CandidateJob {
                                    index,
                                    c,
                                    prepared,
                                    cwd,
                                    worktree,
                                    candidate,
                                    base,
                                    run_id: run.id.clone(),
                                    protected_paths: run.workflow.protected_paths.clone(),
                                    policies: run.workflow.policies.clone(),
                                }));
                                (Job::Candidate(index, c), None, worker)
                            }
                            Err(error) => {
                                run.stop_candidate(
                                    index,
                                    c,
                                    Status::Failed,
                                    format!("{error:#}"),
                                    Actor::Engine,
                                )?;
                                if run.fan_out_done(index) {
                                    if graph::judge_of(&run.workflow, index).is_none()
                                        && !flight.is_empty()
                                    {
                                        if !deferred_settle.contains(&index) {
                                            deferred_settle.push(index);
                                        }
                                    } else {
                                        run.settle_fan_out(index, false)?;
                                    }
                                }
                                run.persist()?;
                                emit(Event::State(Box::new(run.clone())));
                                continue;
                            }
                        },
                    };
                    let (index, kind, label) = match &worker_job {
                        WorkerJob::Stage(stage) => (
                            stage.index,
                            stage.invocation.kind,
                            run.workflow.stages[stage.index].name.clone(),
                        ),
                        WorkerJob::Candidate(cand) => {
                            let state = run.candidate(cand.index, cand.c);
                            let label = format!(
                                "{} · candidate {}",
                                run.workflow.stages[cand.index].name, state.id
                            );
                            emit(Event::Output(format!(
                                "── {label} · attempt {} · {}{}{} ──\n",
                                state.attempts.len(),
                                state.harness,
                                if state.model.is_empty() { "" } else { " " },
                                state.model
                            )));
                            (cand.index, cand.prepared.invocation.kind, label)
                        }
                    };
                    speaking = Some(job);
                    labels.insert(job, label);
                    let timeout = match &worker_job {
                        WorkerJob::Stage(stage) => stage.timeout,
                        WorkerJob::Candidate(_) => {
                            Duration::from_secs(run.workflow.stages[index].timeout_seconds)
                        }
                    };
                    // As in `execute_stage`: never for output that
                    // arrives only at exit.
                    let stall_timeout = run.workflow.stages[index]
                        .stall_timeout_seconds
                        .filter(|_| kind != adapters::OutputKind::Gemini)
                        .map(Duration::from_secs);
                    let worker_tx = tx.clone();
                    scope.spawn(move || {
                        let (invocation, cwd, directory, sandbox, live_budget) = match &worker_job {
                            WorkerJob::Stage(stage) => (
                                &stage.invocation,
                                &stage.cwd,
                                &stage.directory,
                                stage.sandbox.as_ref(),
                                stage.live_budget.as_ref(),
                            ),
                            WorkerJob::Candidate(cand) => (
                                &cand.prepared.invocation,
                                &cand.cwd,
                                &cand.prepared.directory,
                                cand.prepared.sandbox.as_ref(),
                                None,
                            ),
                        };
                        let spawned = std::cell::Cell::new(None);
                        // Formatting here keeps it off the scheduler, which
                        // also runs checks, and sends lines, not raw output.
                        let mut formatter = events::Formatter::new(invocation.kind);
                        let mut usage_tracker = handoff::UsageTracker::new(invocation.kind);
                        let budget_abort = AtomicBool::new(false);
                        let mut budget_exceeded: Option<String> = None;
                        let outcome_result =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                process::capture_with_controls(
                                    invocation,
                                    cwd,
                                    directory,
                                    timeout,
                                    stall_timeout,
                                    cancel,
                                    &[abort, &budget_abort],
                                    sandbox,
                                    |group| {
                                        // Durable before this worker reports it;
                                        // forgotten below if no survivor stayed.
                                        spawned.set(Some(group.pgid));
                                        journal.add(&group)?;
                                        let _ = worker_tx.send(Message::Spawned(job, group));
                                        Ok(())
                                    },
                                    |err, text| {
                                        let lines = if err {
                                            Some(format!(
                                                "[stderr] {}",
                                                storage::clean_terminal(text)
                                            ))
                                        } else {
                                            if let Some(monitor) = live_budget {
                                                if budget_exceeded.is_none() {
                                                    if let Some(live_usage) =
                                                        usage_tracker.push(text)
                                                    {
                                                        if let Some(reason) =
                                                            monitor.check(live_usage)
                                                        {
                                                            budget_exceeded = Some(reason.clone());
                                                            budget_abort
                                                                .store(true, Ordering::Relaxed);
                                                            let _ =
                                                                worker_tx.send(Message::Output(
                                                                    job,
                                                                    format!("\n{reason}\n"),
                                                                ));
                                                        }
                                                    }
                                                }
                                            }
                                            formatter.push(text)
                                        };
                                        if let Some(lines) = lines {
                                            let _ = worker_tx.send(Message::Output(job, lines));
                                        }
                                    },
                                )
                            }))
                            .unwrap_or_else(|_| {
                                Err(anyhow::anyhow!("The harness supervisor panicked"))
                            });
                        let ended = Utc::now();
                        if let Some(rest) = formatter.finish() {
                            let _ = worker_tx.send(Message::Output(job, rest));
                        }
                        // If the group ended cleanly with the supervisor, forget
                        // it now; if a Survivor remains, keep the journal entry
                        // until the scheduler persists `retained_groups`.
                        let survived = outcome_result
                            .as_ref()
                            .err()
                            .is_some_and(|e| e.downcast_ref::<process::Survivor>().is_some());
                        if !survived {
                            if let Some(pgid) = spawned.get() {
                                journal.recorded(pgid);
                            }
                        }
                        let live_usage = usage_tracker.current();
                        match worker_job {
                            WorkerJob::Stage(stage) => {
                                let _ = worker_tx.send(Message::StageDone(
                                    stage.index,
                                    outcome_result,
                                    live_usage,
                                    budget_exceeded,
                                    ended,
                                ));
                            }
                            WorkerJob::Candidate(cand) => {
                                let post =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        run::post_process_candidate(
                                            &cand.candidate,
                                            &cand.prepared,
                                            &cand.cwd,
                                            &cand.worktree,
                                            &cand.base,
                                            &cand.run_id,
                                            &cand.protected_paths,
                                            &cand.policies,
                                            outcome_result,
                                            journal,
                                            cancel,
                                            abort,
                                            &mut |group| {
                                                let _ =
                                                    worker_tx.send(Message::Spawned(job, group));
                                            },
                                            &mut |text| {
                                                let _ = worker_tx.send(Message::Output(job, text));
                                            },
                                        )
                                    }))
                                    .unwrap_or_else(|_| {
                                        Err(anyhow::anyhow!("Candidate post-processing panicked"))
                                    });
                                let _ = worker_tx.send(Message::CandidateDone(
                                    cand.index,
                                    cand.c,
                                    Box::new(post),
                                    ended,
                                ));
                            }
                        }
                    });
                    flight.push((job, flight_prepared));
                }
                if flight.is_empty() {
                    if !deferred_settle.is_empty() {
                        let cancelled = cancel.load(Ordering::Relaxed);
                        for index in std::mem::take(&mut deferred_settle) {
                            if run.stages[index].status == Status::Running
                                && run.fan_out_done(index)
                            {
                                run.settle_fan_out(index, cancelled)?;
                                run.persist()?;
                                emit(Event::State(Box::new(run.clone())));
                            }
                        }
                        if !halted(run) && next.is_none() {
                            continue;
                        }
                    }
                    break;
                }
                let message = rx.recv().context("Harness workers stopped")?;
                match message {
                    Message::Spawned(job, group) => {
                        // Recorded so a later drive can stop the group if mr dies meanwhile.
                        let attempt = match job {
                            Job::Stage(i) => run.stages[i].attempts.last_mut(),
                            Job::Candidate(i, c) => run.candidate_mut(i, c).attempts.last_mut(),
                            Job::RetryWait(_) => None,
                        };
                        let pgid = group.pgid;
                        if let Some(attempt) = attempt {
                            attempt.set_group(Some(group));
                        }
                        if run.persist().is_ok() {
                            journal.recorded(pgid);
                        }
                    }
                    Message::Output(job, lines) => {
                        let Some(label) = labels.get(&job) else {
                            continue;
                        };
                        if speaking != Some(job) {
                            speaking = Some(job);
                            emit(Event::Output(format!("── {label} ──\n")));
                        }
                        emit(Event::Output(lines));
                    }
                    Message::RetryReady(index, interrupted) => {
                        let job = Job::RetryWait(index);
                        if let Some(position) = flight.iter().position(|(j, _)| *j == job) {
                            flight.swap_remove(position);
                        }
                        let reason = retry_reason
                            .remove(&index)
                            .unwrap_or_else(|| "Transient error retry".into());
                        if interrupted
                            || cancel.load(Ordering::Relaxed)
                            || abort.load(Ordering::Relaxed)
                        {
                            run.cursor = index;
                            let _ = run.set(
                                Target::Stage(index),
                                Status::Pending,
                                Actor::User,
                                "Cancelled during retry backoff",
                            );
                            run.stop(
                                Status::Cancelled,
                                "Stage cancelled during transient retry backoff. Filesystem changes are retained; retry explicitly".into(),
                                Actor::User,
                                false,
                            )?;
                            run.persist()?;
                            emit(Event::State(Box::new(run.clone())));
                        } else {
                            run.set(
                                Target::Stage(index),
                                Status::Pending,
                                Actor::Engine,
                                &reason,
                            )?;
                            run.persist()?;
                            emit(Event::State(Box::new(run.clone())));
                        }
                    }
                    Message::StageDone(index, done_result, live_usage, budget_exceeded, ended) => {
                        let job = Job::Stage(index);
                        let Some(position) = flight.iter().position(|(j, _)| *j == job) else {
                            continue;
                        };
                        let (_, prepared) = flight.swap_remove(position);
                        let prepared = prepared.context("Missing stage flight state")?;
                        if labels.remove(&job).is_some() {
                            speaking = None;
                        }
                        let survivor_pgid = done_result
                            .as_ref()
                            .err()
                            .and_then(|e| e.downcast_ref::<process::Survivor>())
                            .map(|s| s.group.pgid);
                        let outcome_ok_not_aborted = done_result
                            .as_ref()
                            .is_ok_and(|o| !o.cancelled && !o.timed_out && !o.stalled);
                        let had_budget_exceed = budget_exceeded.is_some();
                        // When the harness ended, not when this got to it.
                        if let (Some(attempt), true) =
                            (run.stages[index].attempts.last_mut(), done_result.is_ok())
                        {
                            attempt.finished_at = Some(ended);
                        }
                        run.cursor = index;
                        conclude_inner(
                            run,
                            &prepared,
                            done_result,
                            live_usage,
                            budget_exceeded,
                            false,
                            cancel,
                            emit,
                        )?;
                        if let Some(pgid) = survivor_pgid {
                            journal.recorded(pgid);
                        }
                        let retry = run.workflow.stages[index].effective_retry(&run.workflow);
                        let (retries_done, stage_started) = stage_retries
                            .get(&index)
                            .copied()
                            .unwrap_or((0, std::time::Instant::now()));
                        let stage_timeout =
                            Duration::from_secs(run.workflow.stages[index].timeout_seconds);
                        let mut scheduled_retry = false;
                        if should_transient_retry(
                            run,
                            index,
                            &prepared,
                            outcome_ok_not_aborted,
                            retries_done,
                            retry,
                            had_budget_exceed,
                            cancel,
                        ) {
                            let budget_ok = match crate::cost::check_budget_action(run) {
                                crate::cost::BudgetCheck::Ok => true,
                                crate::cost::BudgetCheck::Warn(warning) => {
                                    emit(Event::Output(format!("{warning}\n")));
                                    true
                                }
                                crate::cost::BudgetCheck::Stop(reason) => {
                                    if let Some(hold) = &mut run.hold {
                                        hold.stops.retain(|(i, _)| *i != index);
                                    }
                                    run.set(
                                        Target::Stage(index),
                                        Status::Pending,
                                        Actor::Engine,
                                        "Budget check before retry",
                                    )?;
                                    run.stop(Status::Blocked, reason, Actor::Engine, false)?;
                                    false
                                }
                            };
                            if budget_ok {
                                let next_retry = retries_done + 1;
                                let delay = retry.delay_for(next_retry);
                                let remaining =
                                    stage_timeout.saturating_sub(stage_started.elapsed());
                                if !remaining.is_zero() && (delay.is_zero() || delay < remaining) {
                                    if let Some(hold) = &mut run.hold {
                                        hold.stops.retain(|(i, _)| *i != index);
                                    }
                                    let stage = &run.workflow.stages[index];
                                    let err_summary =
                                        run.error.as_deref().unwrap_or("transient error");
                                    emit(Event::Output(format!(
                                        "Transient failure ({}); retrying {next_retry}/{} in {}s with {}{}{}\n",
                                        storage::truncate(
                                            err_summary.lines().next().unwrap_or(""),
                                            160
                                        ),
                                        retry.max_attempts,
                                        delay.as_secs(),
                                        stage.harness,
                                        if stage.model.is_empty() { "" } else { " " },
                                        stage.model
                                    )));
                                    stage_retries.insert(index, (next_retry, stage_started));
                                    retry_reason.insert(
                                        index,
                                        format!(
                                            "Transient error retry {next_retry}/{} after {}s backoff",
                                            retry.max_attempts,
                                            delay.as_secs()
                                        ),
                                    );
                                    let sleeper_tx = tx.clone();
                                    scope.spawn(move || {
                                        let interrupted =
                                            sleep_interruptible(delay, cancel, Some(abort));
                                        let _ = sleeper_tx
                                            .send(Message::RetryReady(index, interrupted));
                                    });
                                    flight.push((Job::RetryWait(index), None));
                                    scheduled_retry = true;
                                } else {
                                    emit(Event::Output(format!(
                                        "Transient retry {next_retry}/{} skipped: stage timeout budget exhausted\n",
                                        retry.max_attempts
                                    )));
                                }
                            }
                        }
                        if !scheduled_retry {
                            stage_retries.remove(&index);
                            run_post_stage_hooks(
                                run,
                                index,
                                &prepared.directory,
                                prepared.reattached,
                                cancel,
                                emit,
                            );
                            run.persist()?;
                            emit(Event::State(Box::new(run.clone())));
                        }
                    }
                    Message::CandidateDone(index, c, post_result, ended) => {
                        let job = Job::Candidate(index, c);
                        let Some(position) = flight.iter().position(|(j, _)| *j == job) else {
                            continue;
                        };
                        flight.swap_remove(position);
                        if labels.remove(&job).is_some() {
                            speaking = None;
                        }
                        run.cursor = index;
                        let survivor_pgid =
                            run.apply_candidate_post((index, c), *post_result, ended)?;
                        if run.fan_out_done(index) {
                            if graph::judge_of(&run.workflow, index).is_none() && !flight.is_empty()
                            {
                                if !deferred_settle.contains(&index) {
                                    deferred_settle.push(index);
                                }
                            } else {
                                run.settle_fan_out(index, cancel.load(Ordering::Relaxed))?;
                            }
                        }
                        run.persist()?;
                        if let Some(pgid) = survivor_pgid {
                            journal.recorded(pgid);
                        }
                        emit(Event::State(Box::new(run.clone())));
                    }
                }
            }
            Ok(())
        })();
        if result.is_err() {
            // Running harnesses end promptly via the local abort flag while
            // workers drain and report any survivors before the scope ends.
            abort.store(true, Ordering::Relaxed);
            drop(tx);
            let mut survivors_added = false;
            while let Ok(message) = rx.recv() {
                match message {
                    Message::StageDone(_, done_result, _, _, _) => {
                        let before = run.retained_groups.len();
                        keep_survivor(run, &done_result);
                        if run.retained_groups.len() > before {
                            survivors_added = true;
                        }
                    }
                    Message::CandidateDone(_, _, post, _) => {
                        if let Ok(post) = *post {
                            if let Some(group) = post.survivor {
                                if run.retained_groups.iter().all(|g| g.pgid != group.pgid) {
                                    run.retained_groups.push(group);
                                    survivors_added = true;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            if survivors_added {
                let _ = run.persist();
            }
        }
        result
    });
    result?;
    let cancel_requested = cancel.load(Ordering::Relaxed);
    let before_cursor = run.cursor;
    let before_error = run.error.clone();
    // Rounds the run stopped before every candidate started.
    for index in 0..run.stages.len() {
        if only.is_some_and(|only| only != index)
            || run.stages[index].status != Status::Running
            || run.stages[index].fan_out.is_none()
        {
            continue;
        }
        let count = run.stages[index]
            .fan_out
            .as_ref()
            .map_or(0, |f| f.candidates.len());
        let mut had_pending = false;
        for c in 0..count {
            if run.candidate(index, c).status == Status::Pending {
                had_pending = true;
                run.stop_candidate(
                    index,
                    c,
                    Status::Cancelled,
                    "Not started: the run stopped first".into(),
                    Actor::Engine,
                )?;
            }
        }
        run.settle_fan_out(index, cancel_requested || had_pending)?;
    }
    if only.is_none() {
        run.settle()?;
    }
    if !run.journal.is_empty() || run.cursor != before_cursor || run.error != before_error {
        run.persist()?;
    }
    emit(Event::State(Box::new(run.clone())));
    Ok((next, started))
}

/// Runs the current fan-out stage of a list workflow: its candidates side
/// by side, then its judgement.
pub(super) fn execute_fan_out(
    run: &mut Run,
    cancel: &AtomicBool,
    emit: &mut impl FnMut(Event),
) -> Result<()> {
    let index = run.cursor;
    let attempts = run.stages[index].attempts.len();
    if let Err(error) = run.start_fan_out(index) {
        let recorded = run.stages[index].attempts.len() > attempts;
        run.stop(
            Status::Failed,
            format!("{error:#}"),
            Actor::Engine,
            recorded,
        )?;
        run.persist()?;
        emit(Event::State(Box::new(run.clone())));
        return Ok(());
    }
    emit(Event::State(Box::new(run.clone())));
    schedule(run, cancel, emit, Some(index))?;
    Ok(())
}

/// `drive` for a dependency graph: ready stages side by side where safe.
pub(super) fn drive_graph(
    run: &mut Run,
    cancel: &AtomicBool,
    emit: &mut impl FnMut(Event),
) -> Result<Action> {
    let mut checkout_checked = false;
    loop {
        match run.status {
            Status::AwaitingConfirmation => return Ok(Action::Confirm),
            Status::Interactive => return Ok(Action::Interactive),
            Status::Failed | Status::Blocked | Status::Cancelled => {
                save_changes(run);
                if !run.journal.is_empty() {
                    run.persist()?;
                }
                return Ok(Action::Stopped);
            }
            Status::Running => {
                run.recover()?;
                run.persist()?;
                return Ok(Action::Stopped);
            }
            _ => {}
        }
        if run
            .stages
            .iter()
            .all(|s| matches!(s.status, Status::Complete | Status::Skipped))
        {
            run.cursor = run.stages.len();
            if let Err(error) = run.check_review_quorum() {
                run.stop(Status::Blocked, format!("{error:#}"), Actor::Engine, false)?;
                run.persist()?;
                save_changes(run);
                emit(Event::State(Box::new(run.clone())));
                return Ok(Action::Stopped);
            }
            run.set(
                Target::Run,
                Status::Complete,
                Actor::Engine,
                "All enabled stages complete",
            )?;
            run.error = None;
            run.persist()?;
            save_changes(run);
            emit(Event::State(Box::new(run.clone())));
            return Ok(Action::Complete);
        }
        if cancel.load(Ordering::Relaxed) {
            run.cursor = run.focus();
            run.stop(
                Status::Cancelled,
                "Run cancelled before the next stage".into(),
                Actor::User,
                false,
            )?;
            run.persist()?;
            save_changes(run);
            return Ok(Action::Stopped);
        }
        if !checkout_checked {
            refuse_unproven(run)?;
            checkout_checked = true;
        } else {
            refuse_own_unproven(run)?;
        }
        run.hold = Some(Hold::default());
        let scheduled = schedule(run, cancel, emit, None);
        run.hold = None;
        let (next, started) = scheduled?;
        if run.status == Status::Pending {
            match next {
                Some(Next::Ship(index)) => {
                    run.cursor = index;
                    crate::ship::execute(run, cancel, emit)?;
                    continue;
                }
                Some(Next::Interactive(index)) => {
                    run.cursor = index;
                    let reason = format!(
                        "Stage {} waits for its terminal",
                        run.workflow.stages[index].name
                    );
                    run.set(Target::Run, Status::Interactive, Actor::Engine, reason)?;
                    run.persist()?;
                    emit(Event::State(Box::new(run.clone())));
                    return Ok(Action::Interactive);
                }
                None => {
                    ensure!(
                        started > 0,
                        "No stage can start; inspect the run's stage states"
                    );
                }
            }
        }
    }
}
