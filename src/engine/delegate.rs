//! A delegating stage's owner loop (`docs/delegation.md`): the stage's
//! harness turns, the tasks it delegates, and the wake turns that hand it
//! their results. A delegating stage is never parallel-safe, so while this
//! loop runs it is the only thing in flight and the only writer of the run.
use super::exec::{Prepared, left_running, lock_paths};
use super::*;
use crate::delegation::{
    Isolation,
    broker::Broker,
    config::{Delegation, MAX_TASK_SENDS, TASK_ROLES, delegate_stage, glob_match},
    instructions::{self, ParentKind},
    policy::{self, Ceiling},
    records::{ResolvedTarget, TaskState, TaskStatus, TaskTurn, Turn},
    tools::{Call, Changed, DelegationTools, Reply, Request},
};
use crate::serve::mcp::ToolError;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Instant,
};

/// How a parent learns of finished tasks: woken through `follow_up`,
/// waiting in tool calls, or (interactive) told by the user.
pub(crate) fn parent_kind(run: &Run, stage: &Stage) -> ParentKind {
    let local = harness_settings_local(stage, &run.providers);
    let follow = adapters::has_follow_up(&stage.harness, &run.adapters) && !local;
    match stage.mode {
        Mode::Interactive => {
            let assigns = match run.adapters.get(&stage.harness) {
                Some(adapter) => adapter.session.as_ref().is_some_and(|s| s.assign.is_some()),
                None => crate::harness::get(&stage.harness).is_some_and(|spec| {
                    matches!(spec.session, crate::harness::Session::Assign { .. })
                }),
            };
            ParentKind::Interactive {
                resumable: follow && assigns,
            }
        }
        Mode::Unattended if follow => ParentKind::Wake,
        Mode::Unattended => ParentKind::NoWake,
    }
}

/// Codex cannot follow up on a session with a local provider.
fn harness_settings_local(stage: &Stage, configured: &BTreeMap<String, Provider>) -> bool {
    stage.harness == "codex"
        && matches!(stage.provider.as_str(), "ollama" | "lmstudio")
        && providers::route(&stage.harness, &stage.provider, configured).is_none()
}

/// What a worker reports.
enum Msg {
    Spawned(Who, process::Group),
    /// An interactive parent's terminal host socket.
    Terminal(PathBuf),
    Output(Who, String),
    ParentDone(Result<process::Outcome>, DateTime<Utc>),
    TaskDone(String, Result<process::Outcome>, DateTime<Utc>),
    Call(Call),
    Tick,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Who {
    Parent,
    Task(String),
}

/// Where the parent is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Its turn `n` runs.
    Running(usize),
    /// Its turn ended with tasks unread; it waits for them.
    Awaiting,
    /// The interactive parent was left running in its terminal host: live
    /// tasks are being cancelled before the host takes the run's locks.
    Releasing,
    /// The stage concluded or stopped.
    Done,
}

/// A task process the owner launched and has not heard back from.
struct TaskJob {
    abort: Arc<AtomicBool>,
    prepared: Prepared,
    stage: Stage,
}

struct Owner<'a> {
    index: usize,
    stage: Stage,
    delegation: Delegation,
    kind: ParentKind,
    ceiling: Ceiling,
    changed: Arc<Changed>,
    tx: mpsc::Sender<Msg>,
    /// The current turn's broker and its close flag.
    broker: Option<Broker>,
    /// The current parent turn: its launch, and what to conclude it with.
    turn: Option<Prepared>,
    phase: Phase,
    parent_abort: Arc<AtomicBool>,
    running: HashMap<String, TaskJob>,
    queue: VecDeque<String>,
    /// Usage of the parent's turns before the current one.
    earlier_usage: Option<handoff::Usage>,
    deadline: Option<Instant>,
    labels: HashMap<Who, String>,
    speaking: Option<Who>,
    journal: &'a SpawnJournal,
    cancel: &'a AtomicBool,
    /// Catalog model IDs per harness, read once per stage.
    catalogs: std::cell::RefCell<HashMap<String, Vec<String>>>,
    /// An interactive parent's terminal session and the locks it may hand
    /// over to its terminal host.
    interactive: Option<(&'a process::InteractiveSession, &'a [&'a storage::FileLock])>,
    /// The parent turn released to its terminal host; nothing may write the
    /// run after the hand-over.
    released: Option<Result<process::Outcome>>,
    handed_over: bool,
}

/// Runs the current stage, which delegates, to its end.
pub(super) fn drive_stage(
    run: &mut Run,
    cancel: &AtomicBool,
    interactive: Option<(&process::InteractiveSession, &[&storage::FileLock])>,
    emit: &mut impl FnMut(Event),
) -> Result<()> {
    let index = run.cursor;
    let stage = run.workflow.stages[index].clone();
    let delegation = stage
        .delegation
        .clone()
        .context("The stage does not delegate")?;
    let kind = parent_kind(run, &stage);
    let ceiling = policy::stage_ceiling(&run.workflow, &stage, crate::sandbox::detect().is_ok());
    let journal = SpawnJournal::new(run);
    let changed = Arc::new(Changed::default());
    let (tx, rx) = mpsc::channel::<Msg>();
    let mut owner = Owner {
        index,
        deadline: (stage.mode == Mode::Unattended)
            .then(|| Instant::now() + Duration::from_secs(stage.timeout_seconds)),
        stage,
        delegation,
        kind,
        ceiling,
        changed,
        tx,
        broker: None,
        turn: None,
        phase: Phase::Running(1),
        parent_abort: Arc::new(AtomicBool::new(false)),
        running: HashMap::new(),
        queue: VecDeque::new(),
        earlier_usage: None,
        labels: HashMap::new(),
        speaking: None,
        journal: &journal,
        cancel,
        catalogs: Default::default(),
        interactive,
        released: None,
        handed_over: false,
    };
    // Turn 1: the broker exists before the launch that names it.
    owner.open_broker(run)?;
    run.mcp = owner.broker.as_ref().map(mcp_launch);
    let started = start_stage(run, emit);
    run.mcp = None;
    let Some(prepared) = started? else {
        owner.close_broker();
        return Ok(());
    };
    let stop_ticker = Arc::new(AtomicBool::new(false));
    thread::scope(|scope| -> Result<()> {
        // Ticks wake an idle owner for the deadline and for cancellation.
        let ticker = {
            let (tx, stop) = (owner.tx.clone(), stop_ticker.clone());
            scope.spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    thread::park_timeout(Duration::from_millis(100));
                    if tx.send(Msg::Tick).is_err() {
                        break;
                    }
                }
            })
        };
        owner.spawn_parent(scope, run, prepared);
        let looped = owner.run_loop(scope, run, &rx, emit);
        // Whatever happened, nothing outlives the stage.
        owner.close_broker();
        for job in owner.running.values() {
            job.abort.store(true, Ordering::SeqCst);
        }
        owner.parent_abort.store(true, Ordering::SeqCst);
        stop_ticker.store(true, Ordering::SeqCst);
        ticker.thread().unpark();
        // Drain until every worker reported, answering stray calls.
        drop(std::mem::replace(&mut owner.tx, mpsc::channel().0));
        while !owner.running.is_empty() || owner.phase_running() {
            match rx.recv() {
                Ok(Msg::TaskDone(id, outcome, ended)) => {
                    let _ = owner.task_done(run, &id, outcome, ended);
                }
                Ok(Msg::ParentDone(outcome, _)) => {
                    keep_survivor(run, &outcome);
                    owner.phase = Phase::Done;
                }
                Ok(Msg::Call(call)) => {
                    let _ = call.reply.send(Err(not_active()));
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        // After a hand-over the terminal host owns the run's locks.
        if !owner.handed_over {
            let _ = run.persist();
        }
        looped
    })
}

fn not_active() -> ToolError {
    ToolError::new(
        "parent_not_active",
        "This stage turn is over or its tools were closed",
    )
}

fn mcp_launch(broker: &Broker) -> adapters::McpLaunch {
    adapters::McpLaunch {
        program: broker.endpoint.program.clone(),
        sock: broker.endpoint.sock.clone(),
        token_file: broker.endpoint.token_file.clone(),
    }
}

impl<'a> Owner<'a> {
    /// The harness's catalogued model IDs (its listing may run a command,
    /// so once per stage).
    fn models(&self, run: &Run, harness: &str) -> Vec<String> {
        if let Some(models) = self.catalogs.borrow().get(harness) {
            return models.clone();
        }
        let settings = Settings {
            adapters: run.adapters.clone(),
            providers: run.providers.clone(),
            ..Settings::default()
        };
        let models: Vec<String> = crate::catalog::discover(harness, &settings, &run.project)
            .map(|catalog| {
                catalog
                    .models
                    .into_iter()
                    .map(|m| m.id)
                    .filter(|id| !id.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        self.catalogs
            .borrow_mut()
            .insert(harness.to_owned(), models.clone());
        models
    }

    fn phase_running(&self) -> bool {
        matches!(self.phase, Phase::Running(_))
    }

    fn open_broker(&mut self, run: &Run) -> Result<()> {
        self.close_broker();
        let close = Arc::new(AtomicBool::new(false));
        let tx = Mutex::new(self.tx.clone());
        let tools = DelegationTools::new(
            move |call| {
                tx.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .send(Msg::Call(call))
                    .is_ok()
            },
            close.clone(),
            self.changed.clone(),
            self.kind,
            self.delegation.runs && run.launched_by.is_none(),
            self.delegation.schedules && run.launched_by.is_none(),
        );
        self.broker = Some(Broker::start(Arc::new(tools), close)?);
        Ok(())
    }

    fn close_broker(&mut self) {
        if let Some(mut broker) = self.broker.take() {
            broker.close();
        }
    }

    fn attempt<'r>(&self, run: &'r Run) -> &'r Attempt {
        run.stages[self.index]
            .attempts
            .last()
            .expect("a delegating stage has an attempt")
    }
    fn attempt_mut<'r>(&self, run: &'r mut Run) -> &'r mut Attempt {
        run.stages[self.index]
            .attempts
            .last_mut()
            .expect("a delegating stage has an attempt")
    }
    fn task<'r>(&self, run: &'r Run, id: &str) -> Option<&'r TaskState> {
        self.attempt(run).tasks.iter().find(|t| t.id == id)
    }
    fn task_mut<'r>(&self, run: &'r mut Run, id: &str) -> Option<&'r mut TaskState> {
        self.attempt_mut(run).tasks.iter_mut().find(|t| t.id == id)
    }

    fn remaining(&self) -> Duration {
        self.deadline
            .map_or(Duration::from_secs(u64::MAX / 4), |d| {
                d.saturating_duration_since(Instant::now())
            })
    }

    fn emit_lines(&mut self, who: Who, lines: String, emit: &mut impl FnMut(Event)) {
        let Some(label) = self.labels.get(&who).cloned() else {
            emit(Event::Output(lines));
            return;
        };
        if self.speaking.as_ref() != Some(&who) {
            self.speaking = Some(who);
            emit(Event::Output(format!("── {label} ──\n")));
        }
        emit(Event::Output(lines));
    }

    /// Starts a process on a worker thread; its group, output, and end come
    /// back as messages.
    #[allow(clippy::too_many_arguments)]
    fn spawn_process<'s, 'e>(
        &self,
        scope: &'s thread::Scope<'s, 'e>,
        who: Who,
        invocation: Invocation,
        cwd: PathBuf,
        directory: PathBuf,
        sandbox: Option<crate::sandbox::Plan>,
        timeout: Duration,
        stall: Option<Duration>,
        abort: Arc<AtomicBool>,
    ) where
        'a: 's,
    {
        let tx = self.tx.clone();
        let journal = self.journal;
        let cancel = self.cancel;
        scope.spawn(move || {
            let mut formatter = events::Formatter::new(invocation.kind);
            let spawned = std::cell::Cell::new(None);
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                process::capture_with_stall_and_abort(
                    &invocation,
                    &cwd,
                    &directory,
                    timeout,
                    stall,
                    cancel,
                    Some(&abort),
                    sandbox.as_ref(),
                    |group| {
                        spawned.set(Some(group.pgid));
                        journal.add(&group)?;
                        let _ = tx.send(Msg::Spawned(who.clone(), group));
                        Ok(())
                    },
                    |err, text| {
                        let lines = if err {
                            Some(format!("[stderr] {}", storage::clean_terminal(text)))
                        } else {
                            formatter.push(text)
                        };
                        if let Some(lines) = lines {
                            let _ = tx.send(Msg::Output(who.clone(), lines));
                        }
                    },
                )
            }))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("The harness supervisor panicked")));
            let ended = Utc::now();
            if let Some(rest) = formatter.finish() {
                let _ = tx.send(Msg::Output(who.clone(), rest));
            }
            let survived = outcome
                .as_ref()
                .err()
                .is_some_and(|e| e.downcast_ref::<process::Survivor>().is_some());
            if !survived {
                if let Some(pgid) = spawned.get() {
                    journal.recorded(pgid);
                }
            }
            let _ = tx.send(match who {
                Who::Parent => Msg::ParentDone(outcome, ended),
                Who::Task(id) => Msg::TaskDone(id, outcome, ended),
            });
        });
    }

    fn spawn_parent<'s, 'e>(
        &mut self,
        scope: &'s thread::Scope<'s, 'e>,
        run: &Run,
        prepared: Prepared,
    ) where
        'a: 's,
    {
        let stall = self
            .stage
            .stall_timeout_seconds
            .filter(|_| adapters::streams(&self.stage.harness, &run.adapters))
            .map(Duration::from_secs);
        self.labels.insert(Who::Parent, self.stage.name.clone());
        if let (Some((session, _)), Phase::Running(1)) = (self.interactive, self.phase) {
            // An interactive first turn runs in its terminal host.
            let (tx, journal, cancel) = (self.tx.clone(), self.journal, self.cancel);
            let locks = lock_paths(run).unwrap_or_default();
            let (invocation, cwd, directory, sandbox) = (
                prepared.invocation.clone(),
                run.workspace.clone(),
                prepared.directory.clone(),
                prepared.sandbox.clone(),
            );
            scope.spawn(move || {
                let outcome = process::interactive(
                    &invocation,
                    &cwd,
                    &directory,
                    cancel,
                    session,
                    sandbox.as_ref(),
                    &locks,
                    |group, socket| {
                        let _ = journal.add(&group);
                        let _ = tx.send(Msg::Terminal(socket.to_path_buf()));
                        let _ = tx.send(Msg::Spawned(Who::Parent, group));
                    },
                );
                let _ = tx.send(Msg::ParentDone(outcome, Utc::now()));
            });
            self.turn = Some(prepared);
            return;
        }
        self.spawn_process(
            scope,
            Who::Parent,
            prepared.invocation.clone(),
            run.workspace.clone(),
            prepared.directory.clone(),
            prepared.sandbox.clone(),
            self.remaining(),
            stall,
            self.parent_abort.clone(),
        );
        self.turn = Some(prepared);
    }

    fn run_loop<'s, 'e>(
        &mut self,
        scope: &'s thread::Scope<'s, 'e>,
        run: &mut Run,
        rx: &mpsc::Receiver<Msg>,
        emit: &mut impl FnMut(Event),
    ) -> Result<()>
    where
        'a: 's,
    {
        while self.phase != Phase::Done {
            let message = rx.recv().context("Delegation workers stopped")?;
            match message {
                Msg::Spawned(who, group) => {
                    let pgid = group.pgid;
                    match &who {
                        Who::Parent => self.attempt_mut(run).set_group(Some(group)),
                        Who::Task(id) => {
                            if let Some(turn) =
                                self.task_mut(run, id).and_then(|t| t.turns.last_mut())
                            {
                                turn.set_group(Some(group));
                                turn.status = TaskStatus::Running;
                            }
                            if let Some(task) = self.task_mut(run, id) {
                                task.status = TaskStatus::Running;
                            }
                            self.changed.bump();
                        }
                    }
                    if run.persist().is_ok() {
                        self.journal.recorded(pgid);
                    }
                }
                Msg::Terminal(socket) => {
                    self.attempt_mut(run).terminal = Some(socket);
                    let _ = run.persist();
                }
                Msg::Output(who, lines) => self.emit_lines(who, lines, emit),
                Msg::Call(call) => {
                    let quiet = matches!(
                        call.request,
                        Request::Status { deliver: false, .. } | Request::Capabilities { .. }
                    );
                    let reply = self.handle(scope, run, call.request);
                    let _ = call.reply.send(reply);
                    if !quiet {
                        emit(Event::State(Box::new(run.clone())));
                    }
                }
                Msg::TaskDone(id, outcome, ended) => {
                    self.task_done(run, &id, outcome, ended)?;
                    if self.phase == Phase::Releasing {
                        self.finish_release(run, emit)?;
                        continue;
                    }
                    self.launch_queued(scope, run)?;
                    emit(Event::State(Box::new(run.clone())));
                    if self.phase == Phase::Awaiting {
                        self.maybe_wake(scope, run, emit)?;
                    }
                }
                Msg::ParentDone(outcome, ended) => {
                    self.parent_done(scope, run, outcome, ended, emit)?;
                }
                Msg::Tick => self.tick(run, emit)?,
            }
        }
        Ok(())
    }

    fn tick(&mut self, run: &mut Run, emit: &mut impl FnMut(Event)) -> Result<()> {
        let cancelled = self.cancel.load(Ordering::Relaxed);
        let late = self.deadline.is_some_and(|d| Instant::now() >= d);
        if !cancelled && !late {
            return Ok(());
        }
        match self.phase {
            // The parent's own supervisor sees both and ends it.
            Phase::Running(_) => {
                if late {
                    self.parent_abort.store(true, Ordering::SeqCst);
                }
            }
            Phase::Awaiting => {
                let (status, message) = if cancelled {
                    (
                        Status::Cancelled,
                        "Stage cancelled while its delegated tasks ran. Filesystem changes are retained; retry explicitly".to_owned(),
                    )
                } else {
                    (
                        Status::Failed,
                        "Stage timed out while its delegated tasks ran; finished results are kept"
                            .to_owned(),
                    )
                };
                self.stop_stage(run, status, message, emit)?;
            }
            Phase::Releasing | Phase::Done => {}
        }
        Ok(())
    }

    /// Hands a released interactive parent to its terminal host once every
    /// task it started has stopped: the host then holds the run's locks, so
    /// this loop never writes the run again.
    fn finish_release(&mut self, run: &mut Run, emit: &mut impl FnMut(Event)) -> Result<()> {
        if !self.running.is_empty() {
            return Ok(());
        }
        let Some(outcome) = self.released.take() else {
            return Ok(());
        };
        let (session, held) = self.interactive.context("Not interactive")?;
        emit(Event::Output(
            "Delegation closed: the stage's tools stop with the release, and its tasks were cancelled\n"
                .into(),
        ));
        left_running(run, &outcome, session, held, emit)?;
        self.handed_over = true;
        self.phase = Phase::Done;
        Ok(())
    }

    /// Stops the stage while no parent process runs: live tasks are
    /// cancelled, the attempt and stage stop.
    fn stop_stage(
        &mut self,
        run: &mut Run,
        status: Status,
        message: String,
        emit: &mut impl FnMut(Event),
    ) -> Result<()> {
        self.close_broker();
        self.cancel_live(run);
        let usage = self.earlier_usage;
        let attempt = self.attempt_mut(run);
        attempt.usage = usage;
        attempt.finished_at.get_or_insert_with(Utc::now);
        run.cursor = self.index;
        run.stop(status, message, Actor::Engine, true)?;
        if let Some(attempt) = run.stages[self.index].attempts.last_mut() {
            if attempt.tree_after.is_none() {
                attempt.tree_after = snapshot::tree(&run.workspace, &run.directory);
            }
        }
        run.persist()?;
        emit(Event::State(Box::new(run.clone())));
        self.phase = Phase::Done;
        Ok(())
    }

    /// Cancels every task still queued or running.
    fn cancel_live(&mut self, run: &mut Run) {
        for job in self.running.values() {
            job.abort.store(true, Ordering::SeqCst);
        }
        for id in std::mem::take(&mut self.queue) {
            if let Some(task) = self.task_mut(run, &id) {
                task.status = TaskStatus::Cancelled;
                task.error = Some("Cancelled: the stage ended".into());
            }
        }
        self.changed.bump();
    }

    fn parent_done<'s, 'e>(
        &mut self,
        scope: &'s thread::Scope<'s, 'e>,
        run: &mut Run,
        outcome: Result<process::Outcome>,
        ended: DateTime<Utc>,
        emit: &mut impl FnMut(Event),
    ) -> Result<()>
    where
        'a: 's,
    {
        let Phase::Running(turn_number) = self.phase else {
            return Ok(());
        };
        let prepared = self.turn.take().context("No parent turn")?;
        // Calls in flight answer parent_not_active from here on.
        self.close_broker();
        if matches!(&outcome, Ok(o) if o.released) {
            // Left running in its terminal host: its tools close for good and
            // its tasks are cancelled before the host takes the locks.
            self.cancel_live(run);
            self.released = Some(outcome);
            self.phase = Phase::Releasing;
            run.persist()?;
            return self.finish_release(run, emit);
        }
        let succeeded = outcome
            .as_ref()
            .is_ok_and(|o| o.success() && !o.cancelled && !o.timed_out && !o.stalled);
        if turn_number > 1 {
            if let Some(turn) = self.attempt_mut(run).turns.last_mut() {
                turn.finished_at = Some(ended);
                turn.exit_code = outcome.as_ref().ok().and_then(|o| o.code);
            }
        }
        let undelivered: Vec<String> = self
            .attempt(run)
            .tasks
            .iter()
            .filter(|t| t.undelivered())
            .map(|t| t.id.clone())
            .collect();
        if !succeeded || undelivered.is_empty() {
            // The final turn, or a failure: conclude it as any stage attempt.
            if !succeeded {
                self.cancel_live(run);
            }
            return self.conclude_turn(run, &prepared, outcome, emit);
        }
        // Results a wake announced must be read in that wake.
        let unread: Vec<String> = self
            .attempt(run)
            .tasks
            .iter()
            .filter(|t| t.announced && t.undelivered())
            .map(|t| t.id.clone())
            .collect();
        let turn_usage = handoff::usage_file(
            &prepared.directory.join("stdout.log"),
            prepared.invocation.kind,
        );
        self.earlier_usage = add_usage(self.earlier_usage, turn_usage);
        if turn_number > 1 {
            if let Some(turn) = self.attempt_mut(run).turns.last_mut() {
                turn.usage = turn_usage;
            }
        }
        self.attempt_mut(run).set_group(None);
        if !unread.is_empty() {
            return self.stop_stage(
                run,
                Status::Blocked,
                format!(
                    "delegation: results not read (tasks {}). The wake that announced them ended without reading them",
                    unread.join(", ")
                ),
                emit,
            );
        }
        // A provisional handoff: set aside, never accepted.
        let turn_dir = self
            .attempt(run)
            .directory
            .join("turns")
            .join(format!("turn-{turn_number:03}"));
        fs::create_dir_all(&turn_dir)?;
        set_aside_handoff(&prepared.handoff_file, &turn_dir)?;
        // The session to resume: assigned at launch, or reported in output.
        if self.attempt(run).session.is_none() {
            let session = read_head(&prepared.directory.join("stdout.log"), 64 * 1024)
                .ok()
                .and_then(|head| handoff::session_id(&head));
            self.attempt_mut(run).session = session;
        }
        match self.kind {
            ParentKind::Wake | ParentKind::Interactive { resumable: true } => {}
            _ => {
                return self.stop_stage(
                    run,
                    Status::Blocked,
                    "delegation: the parent exited with unread or running tasks, and its session cannot be resumed".into(),
                    emit,
                );
            }
        }
        self.phase = Phase::Awaiting;
        run.persist()?;
        emit(Event::Output(format!(
            "Delegation: {} waits for tasks {}\n",
            self.stage.name,
            undelivered.join(", ")
        )));
        self.maybe_wake(scope, run, emit)
    }

    /// Concludes the parent's last turn through the stage's usual path:
    /// handoff, final-turn repair, checks.
    fn conclude_turn(
        &mut self,
        run: &mut Run,
        prepared: &Prepared,
        outcome: Result<process::Outcome>,
        emit: &mut impl FnMut(Event),
    ) -> Result<()> {
        run.cursor = self.index;
        // A wake turn of an interactive stage is an unattended follow-up:
        // its exit, handoff, and repair are judged as unattended.
        let woke = self.stage.mode == Mode::Interactive && !self.attempt(run).turns.is_empty();
        if woke {
            run.workflow.stages[self.index].mode = Mode::Unattended;
            self.attempt_mut(run).mode = Mode::Unattended;
        }
        let concluded = conclude(run, prepared, outcome, self.cancel, emit);
        if woke {
            run.workflow.stages[self.index].mode = Mode::Interactive;
            if let Some(attempt) = run.stages[self.index].attempts.last_mut() {
                attempt.mode = Mode::Interactive;
            }
            run.persist()?;
        }
        concluded?;
        // `classify_exit` counted only this turn; earlier turns count too.
        if self.earlier_usage.is_some() {
            if let Some(attempt) = run.stages[self.index].attempts.last_mut() {
                attempt.usage = add_usage(self.earlier_usage, attempt.usage);
            }
            run.persist()?;
        }
        self.phase = Phase::Done;
        Ok(())
    }

    /// Wakes the parent when every unread task is finished or waiting on a
    /// question, if the stage still may.
    fn maybe_wake<'s, 'e>(
        &mut self,
        scope: &'s thread::Scope<'s, 'e>,
        run: &mut Run,
        emit: &mut impl FnMut(Event),
    ) -> Result<()>
    where
        'a: 's,
    {
        let cohort: Vec<&TaskState> = self
            .attempt(run)
            .tasks
            .iter()
            .filter(|t| t.undelivered())
            .collect();
        if cohort
            .iter()
            .any(|t| t.status.is_live() || !t.queued_messages.is_empty())
        {
            return Ok(());
        }
        let announced: Vec<String> = cohort.iter().map(|t| t.id.clone()).collect();
        let lines: Vec<String> = cohort
            .iter()
            .map(|t| {
                let target = t
                    .target
                    .delegate
                    .clone()
                    .unwrap_or_else(|| t.target.harness.clone());
                let detail = match t.status {
                    TaskStatus::Waiting => format!("waiting, {} question(s)", t.questions.len()),
                    status => match t.latest_handoff.as_ref().or(t.handoff.as_ref()) {
                        Some(h) => {
                            format!("{}, verdict {}", status.label(), verdict_label(h.verdict))
                        }
                        None => status.label().to_owned(),
                    },
                };
                format!("- {} ({target}): {detail}", t.id)
            })
            .collect();
        if announced.is_empty() {
            return Ok(());
        }
        let wakes = self.attempt(run).turns.len();
        if wakes >= self.delegation.max_wake_turns as usize {
            return self.stop_stage(
                run,
                Status::Blocked,
                format!(
                    "delegation: wake refused, max_wake_turns ({}) reached; finished results are kept",
                    self.delegation.max_wake_turns
                ),
                emit,
            );
        }
        if let crate::cost::BudgetCheck::Stop(reason) = crate::cost::check_budget_action(run) {
            return self.stop_stage(
                run,
                Status::Blocked,
                format!("delegation: wake refused by budget. {reason}"),
                emit,
            );
        }
        let Some(session) = self.attempt(run).session.clone() else {
            return self.stop_stage(
                run,
                Status::Blocked,
                format!(
                    "delegation: no session ID recorded for {}, so the parent cannot be resumed",
                    self.stage.harness
                ),
                emit,
            );
        };
        let number = wakes + 2;
        let attempt_dir = self.attempt(run).directory.clone();
        let turn_dir = attempt_dir.join("turns").join(format!("turn-{number:03}"));
        fs::create_dir_all(&turn_dir)?;
        let prompt = format!(
            "Delegated tasks reached a state that needs you:\n{}\nUse task_status for each, answer questions with task_send, then continue or write your final handoff.\n",
            lines.join("\n")
        );
        let prompt_file = turn_dir.join("prompt.md");
        storage::atomic_write_fast(&prompt_file, prompt.as_bytes())?;
        self.open_broker(run)?;
        let mcp = self.broker.as_ref().map(mcp_launch);
        let handoff_file = self.attempt(run).handoff_file.clone();
        let paths = ContextPaths {
            workspace: &run.workspace,
            attempt_dir: &turn_dir,
            prompt_file: &prompt_file,
            handoff_file: &handoff_file,
            mcp: mcp.as_ref(),
        };
        let invocation = match adapters::follow_up(
            &self.stage,
            &run.adapters,
            &run.providers,
            &session,
            &prompt,
            &paths,
        ) {
            Ok(invocation) => invocation,
            Err(error) => {
                return self.stop_stage(
                    run,
                    Status::Blocked,
                    format!("delegation: the parent cannot be resumed: {error:#}"),
                    emit,
                );
            }
        };
        let handoff_dir = handoff_file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let wake_stage = Stage {
            mode: Mode::Unattended,
            ..self.stage.clone()
        };
        let mut sandbox = crate::sandbox::plan(
            &run.workflow,
            &wake_stage,
            &crate::sandbox::Paths {
                workspace: &run.workspace,
                attempt_dir: &attempt_dir,
                handoff_dir: &handoff_dir,
                program: &invocation.program,
            },
        )?;
        if let (Some(plan), Some(mcp)) = (sandbox.as_mut(), &mcp) {
            plan.allow_orch(&mcp.sock, &mcp.token_file, &mcp.program);
        }
        for task in self.attempt_mut(run).tasks.iter_mut() {
            if announced.contains(&task.id) {
                task.announced = true;
            }
        }
        self.attempt_mut(run).turns.push(Turn {
            number,
            started_at: Utc::now(),
            finished_at: None,
            directory: turn_dir.clone(),
            exit_code: None,
            usage: None,
            announced: announced.clone(),
        });
        run.persist()?;
        emit(Event::Output(format!(
            "── {} · wake turn {number}: tasks {} ──\n",
            self.stage.name,
            announced.join(", ")
        )));
        self.phase = Phase::Running(number);
        let prepared = Prepared {
            invocation,
            sandbox,
            directory: turn_dir,
            handoff_file,
            reattached: false,
        };
        self.spawn_parent(scope, run, prepared);
        Ok(())
    }

    // ── Tool requests ────────────────────────────────────────────────────

    fn handle<'s, 'e>(
        &mut self,
        scope: &'s thread::Scope<'s, 'e>,
        run: &mut Run,
        request: Request,
    ) -> Reply
    where
        'a: 's,
    {
        if !self.phase_running() {
            return Err(not_active());
        }
        match request {
            Request::Capabilities { search } => Ok(self.capabilities(run, search.as_deref())),
            Request::Delegate { arguments } => self.delegate(scope, run, &arguments),
            Request::Status { ids, deliver } => self.status(run, &ids, deliver),
            Request::Cancel { id, reason } => self.cancel_task(run, &id, reason),
            Request::Send { arguments } => self.send(scope, run, &arguments),
            Request::Apply { arguments } => self.apply(run, &arguments),
            Request::Other { tool, arguments } => super::delegate_runs::run_tool(
                run,
                &super::delegate_runs::Launcher {
                    index: self.index,
                    stage: &self.stage,
                    ceiling: &self.ceiling,
                    delegation: &self.delegation,
                },
                &tool,
                &arguments,
            ),
        }
    }

    fn capabilities(&self, run: &Run, search: Option<&str>) -> Value {
        let attempt = self.attempt(run);
        let backend = crate::sandbox::detect().is_ok();
        let available = |harness: &str| -> (bool, Vec<String>) {
            let mut constraints = vec![];
            let program = adapters::program(harness, &run.adapters);
            if adapters::executable(&program).is_none() {
                constraints.push(format!("{program} is not installed"));
            }
            if !backend {
                constraints.push(
                    "no OS sandbox backend: tasks cannot run (install bubblewrap on Linux)".into(),
                );
            }
            (constraints.is_empty(), constraints)
        };
        let delegates: Vec<Value> = self
            .delegation
            .delegates
            .iter()
            .filter_map(|name| run.delegates.get(name).map(|d| (name, d)))
            .map(|(name, d)| {
                let (ok, constraints) = available(&d.harness);
                json!({
                    "delegate": name,
                    "harness": d.harness,
                    "model": d.model,
                    "effort": d.effort,
                    "isolation": d.isolation.map(Isolation::label),
                    "available": ok,
                    "constraints": constraints,
                })
            })
            .collect();
        let harnesses: Vec<Value> = self
            .delegation
            .harnesses
            .iter()
            .map(|harness| {
                let (ok, constraints) = available(harness);
                let mut models: Vec<String> = self
                    .models(run, harness)
                    .into_iter()
                    .filter(|m| {
                        self.delegation.models.is_empty()
                            || self.delegation.models.iter().any(|g| glob_match(g, m))
                    })
                    .filter(|m| search.is_none_or(|s| m.contains(s)))
                    .collect();
                let omitted = models.len().saturating_sub(8);
                models.truncate(8);
                json!({
                    "harness": harness,
                    "models": models,
                    "omitted_models": omitted,
                    "model_globs": self.delegation.models,
                    "efforts": crate::harness::get(harness).map_or(&[][..], |s| s.efforts),
                    "available": ok,
                    "constraints": constraints,
                })
            })
            .collect();
        let used = attempt.tasks.len();
        let running = attempt.tasks.iter().filter(|t| t.status.is_live()).count();
        json!({
            "caller": {
                "stage": self.stage.id,
                "harness": self.stage.harness,
                "model": self.stage.model,
                "ceiling": self.ceiling,
                "supports_wake": matches!(self.kind, ParentKind::Wake | ParentKind::Interactive { resumable: true }),
            },
            "delegates": delegates,
            "harnesses": harnesses,
            "limits": {
                "max_tasks": self.delegation.max_tasks,
                "tasks_used": used,
                "max_parallel": self.delegation.max_parallel,
                "tasks_live": running,
                "max_wake_turns": self.delegation.max_wake_turns,
                "wake_turns_used": attempt.turns.len(),
                "max_task_sends": MAX_TASK_SENDS,
            },
            "features": {
                "isolation": ["shared", "worktree"],
                "runs": self.delegation.runs && run.launched_by.is_none(),
                "schedules": self.delegation.schedules && run.launched_by.is_none(),
            },
        })
    }

    /// The target a request names, resolved and checked against the policy.
    fn resolve_target(
        &self,
        run: &Run,
        target: Option<&Value>,
        role: Role,
    ) -> std::result::Result<(ResolvedTarget, Stage), ToolError> {
        let named = |name: &str| -> std::result::Result<(ResolvedTarget, Stage), ToolError> {
            if !self.delegation.delegates.iter().any(|d| d == name) {
                return Err(ToolError::new(
                    "target_unknown",
                    format!("Delegate {name} is not listed for this stage"),
                ));
            }
            let delegate = run.delegates.get(name).ok_or_else(|| {
                ToolError::new(
                    "target_unknown",
                    format!("Delegate {name} is not in this run"),
                )
            })?;
            Ok((
                ResolvedTarget {
                    delegate: Some(name.to_owned()),
                    harness: delegate.harness.clone(),
                    model: delegate.model.clone(),
                    provider: delegate.provider.clone(),
                    effort: delegate.effort.clone(),
                },
                delegate_stage(name, delegate, &self.stage, role),
            ))
        };
        let Some(target) = target.filter(|t| !t.is_null()) else {
            return match self.delegation.delegates.first() {
                Some(name) => named(name),
                None => Err(ToolError::new(
                    "invalid_request",
                    "target is required: this stage has no named delegate",
                )),
            };
        };
        let field = |name: &str| {
            target
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        if let Some(name) = target.get("delegate").and_then(Value::as_str) {
            return named(name);
        }
        let harness = field("harness");
        if harness.is_empty() {
            return Err(ToolError::new(
                "invalid_request",
                "target needs delegate or harness",
            ));
        }
        if !self.delegation.harnesses.contains(&harness) {
            return Err(ToolError::new(
                "target_unknown",
                format!("Harness {harness} is not allowed for this stage"),
            ));
        }
        let (model, provider, effort) = (field("model"), field("provider"), field("effort"));
        if !model.is_empty() {
            let globbed = self.delegation.models.iter().any(|g| glob_match(g, &model));
            let catalogued = self.models(run, &harness).contains(&model);
            let manual = crate::harness::get(&harness)
                .is_none_or(|s| s.catalog == crate::harness::CatalogSource::Manual);
            let allowed = if self.delegation.models.is_empty() {
                catalogued || manual
            } else {
                globbed
            };
            if !allowed {
                return Err(ToolError::new(
                    "model_unavailable",
                    format!("Model {model} is not allowed or not in {harness}'s catalog"),
                ));
            }
        }
        if !provider.is_empty() && !self.delegation.providers.contains(&provider) {
            return Err(ToolError::new(
                "provider_unavailable",
                format!("Provider {provider} is not allowed for this stage"),
            ));
        }
        let stage = Stage {
            id: "task".into(),
            name: "task".into(),
            role,
            harness: harness.clone(),
            model: model.clone(),
            provider: provider.clone(),
            effort: effort.clone(),
            mode: Mode::Unattended,
            timeout_seconds: self.stage.timeout_seconds,
            stall_timeout_seconds: self.stage.stall_timeout_seconds,
            sandbox_network: self.stage.sandbox_network,
            ..Stage::default()
        };
        adapters::validate_options(&stage, &run.providers)
            .map_err(|e| ToolError::new("invalid_request", format!("{e:#}")))?;
        Ok((
            ResolvedTarget {
                delegate: None,
                harness,
                model,
                provider,
                effort,
            },
            stage,
        ))
    }

    fn delegate<'s, 'e>(
        &mut self,
        scope: &'s thread::Scope<'s, 'e>,
        run: &mut Run,
        arguments: &Value,
    ) -> Reply
    where
        'a: 's,
    {
        let task_text = arguments
            .get("task")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| ToolError::new("invalid_request", "task is required"))?
            .to_owned();
        if task_text.len() > 64 * 1024 {
            return Err(ToolError::new(
                "invalid_request",
                "task is longer than 64 KiB",
            ));
        }
        let key = arguments
            .get("client_request_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let fingerprint = crate::trust::digest(&[serde_json::to_string(&json!({
            "task": task_text,
            "target": arguments.get("target"),
            "role": arguments.get("role"),
            "isolation": arguments.get("isolation"),
            "title": arguments.get("title"),
        }))
        .unwrap_or_default()]);
        if let Some(key) = &key {
            if let Some(existing) = self
                .attempt(run)
                .tasks
                .iter()
                .find(|t| &t.client_request_id == key)
            {
                if existing.fingerprint != fingerprint {
                    return Err(ToolError::new(
                        "request_conflict",
                        format!("client_request_id {key} was used for a different request"),
                    ));
                }
                return Ok(self.view(run, existing));
            }
        }
        let attempt = self.attempt(run);
        if attempt.tasks.len() >= self.delegation.max_tasks as usize {
            return Err(ToolError::new(
                "task_limit_exceeded",
                format!(
                    "This stage may start at most {} tasks",
                    self.delegation.max_tasks
                ),
            ));
        }
        let role = match arguments.get("role").and_then(Value::as_str) {
            None => None,
            Some(text) => Some(
                TASK_ROLES
                    .into_iter()
                    .find(|r| r.label() == text)
                    .ok_or_else(|| {
                        ToolError::new("invalid_request", format!("Unknown role {text}"))
                    })?,
            ),
        };
        let isolation_arg = match arguments.get("isolation").and_then(Value::as_str) {
            None => None,
            Some("shared") => Some(Isolation::Shared),
            Some("worktree") => Some(Isolation::Worktree),
            Some(other) => {
                return Err(ToolError::new(
                    "invalid_request",
                    format!("Unknown isolation {other}"),
                ));
            }
        };
        let probe_role = role.unwrap_or(Role::Research);
        let (target, _) = self.resolve_target(run, arguments.get("target"), probe_role)?;
        let delegate_isolation = target
            .delegate
            .as_ref()
            .and_then(|n| run.delegates.get(n))
            .and_then(|d| d.isolation);
        let isolation = isolation_arg.or(delegate_isolation).unwrap_or(match role {
            Some(Role::Implementation) => Isolation::Worktree,
            _ => Isolation::Shared,
        });
        let role = role.unwrap_or(match isolation {
            Isolation::Worktree => Role::Implementation,
            Isolation::Shared => Role::Research,
        });
        let (target, mut task_stage) = self.resolve_target(run, arguments.get("target"), role)?;
        if isolation == Isolation::Shared {
            force_native_read_only(&mut task_stage);
        }
        if crate::sandbox::detect().is_err() {
            return Err(ToolError::new(
                "isolation_unavailable",
                "Tasks run in their own OS sandbox, and no backend works here (install bubblewrap on Linux)",
            ));
        }
        if isolation == Isolation::Worktree && run.baseline.is_none() && !is_git(&run.workspace) {
            return Err(ToolError::new(
                "worktree_unavailable",
                "Worktree tasks need a Git checkout",
            ));
        }
        let ceiling = policy::task_ceiling(&task_stage, isolation, &self.ceiling);
        if let Some(why) = policy::exceeds(&ceiling, &self.ceiling) {
            return Err(ToolError::new(
                "permission_escalation_denied",
                format!("The task cannot run: {why}"),
            ));
        }
        let program = adapters::program(&target.harness, &run.adapters);
        if adapters::executable(&program).is_none() {
            return Err(ToolError::new(
                "harness_unavailable",
                format!("{program} is not installed"),
            ));
        }
        if let crate::cost::BudgetCheck::Stop(reason) = crate::cost::check_budget_action(run) {
            return Err(ToolError::new("budget_exceeded", reason));
        }
        let id = format!("t{}", self.attempt(run).tasks.len() + 1);
        let task = TaskState {
            id: id.clone(),
            client_request_id: key.unwrap_or_else(|| format!("generated-{id}")),
            fingerprint,
            title: arguments
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect(),
            role,
            target,
            isolation,
            ceiling,
            status: TaskStatus::Queued,
            task: task_text,
            turns: vec![],
            handoff: None,
            latest_handoff: None,
            questions: vec![],
            worktree: None,
            base: None,
            patches: vec![],
            error: None,
            delivered: false,
            announced: false,
            cancelled_by_parent: false,
            queued_messages: vec![],
            created_at: Utc::now(),
        };
        self.attempt_mut(run).tasks.push(task);
        self.queue.push_back(id.clone());
        run.persist()
            .map_err(|e| ToolError::new("orchestration_error", format!("{e:#}")))?;
        self.launch_queued(scope, run)
            .map_err(|e| ToolError::new("orchestration_error", format!("{e:#}")))?;
        self.changed.bump();
        let task = self.task(run, &id).expect("just added");
        Ok(self.view(run, task))
    }

    /// Starts queued tasks and queued follow-ups while slots are free.
    fn launch_queued<'s, 'e>(
        &mut self,
        scope: &'s thread::Scope<'s, 'e>,
        run: &mut Run,
    ) -> Result<()>
    where
        'a: 's,
    {
        if self.phase == Phase::Done {
            return Ok(());
        }
        // Follow-ups first: their tasks already hold the parent's attention.
        let sends: Vec<String> = self
            .attempt(run)
            .tasks
            .iter()
            .filter(|t| !t.queued_messages.is_empty() && !self.running.contains_key(&t.id))
            .map(|t| t.id.clone())
            .collect();
        for id in sends {
            if self.running.len() >= self.delegation.max_parallel as usize {
                return Ok(());
            }
            let message = match self.task_mut(run, &id) {
                Some(task) => task.queued_messages.remove(0),
                None => continue,
            };
            self.launch_turn(scope, run, &id, Some(message))?;
        }
        while self.running.len() < self.delegation.max_parallel as usize {
            let Some(id) = self.queue.pop_front() else {
                break;
            };
            if let crate::cost::BudgetCheck::Stop(reason) = crate::cost::check_budget_action(run) {
                if let Some(task) = self.task_mut(run, &id) {
                    task.status = TaskStatus::Failed;
                    task.error = Some(format!("budget_exceeded: {reason}"));
                }
                self.changed.bump();
                continue;
            }
            self.launch_turn(scope, run, &id, None)?;
        }
        run.persist()?;
        Ok(())
    }

    /// Launches a task's first turn, or (with `message`) a follow-up turn on
    /// its session.
    fn launch_turn<'s, 'e>(
        &mut self,
        scope: &'s thread::Scope<'s, 'e>,
        run: &mut Run,
        id: &str,
        message: Option<String>,
    ) -> Result<()>
    where
        'a: 's,
    {
        let task = self.task(run, id).context("No such task")?.clone();
        let (_, mut stage) = self
            .resolve_target(run, Some(&target_value(&task.target)), task.role)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if task.isolation == Isolation::Shared {
            force_native_read_only(&mut stage);
        }
        let number = task.turns.len() + 1;
        let attempt_dir = self.attempt(run).directory.clone();
        let tasks_dir = attempt_dir.with_file_name(format!(
            "{}.tasks",
            attempt_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("attempt")
        ));
        let task_dir = tasks_dir.join(id);
        let turn_dir = task_dir.join(format!("turn-{number:03}"));
        fs::create_dir_all(&turn_dir)?;
        let handoff_file = turn_dir.join("handoff.json");
        let schema_file = turn_dir.join(handoff::SCHEMA_FILE);
        storage::atomic_write_fast(&schema_file, handoff::SCHEMA.as_bytes())?;
        let cwd = match task.isolation {
            Isolation::Shared => run.workspace.clone(),
            Isolation::Worktree => self.task_worktree(run, id)?,
        };
        let prompt = match &message {
            None => task_prompt(&task, &self.stage, &cwd, &handoff_file, &schema_file),
            Some(message) => format!(
                "{message}\n\nWhen you are done, write your handoff for this turn.\n{}",
                handoff::contract(task.role, &handoff_file, &schema_file)
            ),
        };
        let prompt_file = turn_dir.join("prompt.md");
        storage::atomic_write_fast(&prompt_file, prompt.as_bytes())?;
        let paths = ContextPaths {
            workspace: &cwd,
            attempt_dir: &turn_dir,
            prompt_file: &prompt_file,
            handoff_file: &handoff_file,
            mcp: None,
        };
        let session = task.turns.iter().rev().find_map(|t| t.session.clone());
        let (invocation, assigned) = match (&message, &session) {
            (Some(_), Some(session)) => (
                adapters::follow_up(
                    &stage,
                    &run.adapters,
                    &run.providers,
                    session,
                    &prompt,
                    &paths,
                )?,
                Some(session.clone()),
            ),
            (Some(_), None) => bail!("Task {id} has no session to follow up on"),
            (None, _) => {
                let mut invocation =
                    adapters::build(&stage, &run.adapters, &run.providers, &prompt, &paths)?;
                let assigned = adapters::assign_session(&mut invocation, &stage.harness);
                (invocation, assigned)
            }
        };
        let sandbox_stage = Stage {
            sandbox: Some(match task.isolation {
                Isolation::Shared => crate::config::Sandbox::ReadOnly,
                Isolation::Worktree => crate::config::Sandbox::WorkspaceWrite,
            }),
            sandbox_fallback: crate::config::SandboxFallback::Refuse,
            sandbox_writable: vec![],
            ..stage.clone()
        };
        let sandbox = crate::sandbox::plan(
            &run.workflow,
            &sandbox_stage,
            &crate::sandbox::Paths {
                workspace: &cwd,
                attempt_dir: &turn_dir,
                handoff_dir: &turn_dir,
                program: &invocation.program,
            },
        )?;
        storage::write_json_fast(
            &turn_dir.join("command.json"),
            &json!({
                "program": invocation.program,
                "args": invocation.args,
                "env": invocation.env.iter().map(|(n, _)| n).collect::<Vec<_>>(),
                "workspace": cwd,
                "sandbox": sandbox,
            }),
        )?;
        let tree_before = (task.isolation == Isolation::Worktree)
            .then(|| snapshot::tree(&cwd, &run.directory))
            .flatten();
        let turn = TaskTurn {
            number,
            started_at: Utc::now(),
            finished_at: None,
            model: stage.model.clone(),
            status: TaskStatus::Starting,
            exit_code: None,
            directory: turn_dir.clone(),
            handoff_file: handoff_file.clone(),
            error: None,
            usage: None,
            session: assigned,
            process_group: None,
            process_identity: Default::default(),
            tree_before,
            tree_after: None,
            message: message.clone(),
        };
        if let Some(task) = self.task_mut(run, id) {
            task.turns.push(turn);
            task.status = TaskStatus::Starting;
            task.delivered = false;
            task.announced = false;
        }
        run.persist()?;
        let label = format!(
            "{} · task {id} ({})",
            self.stage.name,
            task.target
                .delegate
                .clone()
                .unwrap_or_else(|| task.target.harness.clone())
        );
        self.labels.insert(Who::Task(id.to_owned()), label);
        let abort = Arc::new(AtomicBool::new(false));
        let timeout = Duration::from_secs(stage.timeout_seconds).min(self.remaining());
        let stall = stage
            .stall_timeout_seconds
            .filter(|_| adapters::streams(&stage.harness, &run.adapters))
            .map(Duration::from_secs);
        self.spawn_process(
            scope,
            Who::Task(id.to_owned()),
            invocation.clone(),
            cwd,
            turn_dir.clone(),
            sandbox.clone(),
            timeout,
            stall,
            abort.clone(),
        );
        self.running.insert(
            id.to_owned(),
            TaskJob {
                abort,
                prepared: Prepared {
                    invocation,
                    sandbox,
                    directory: turn_dir,
                    handoff_file,
                    reattached: false,
                },
                stage,
            },
        );
        self.changed.bump();
        Ok(())
    }

    /// A worktree task's checkout: created on its first turn from a snapshot
    /// of the parent's checkout.
    fn task_worktree(&mut self, run: &mut Run, id: &str) -> Result<PathBuf> {
        if let Some(worktree) = self.task(run, id).and_then(|t| t.worktree.clone()) {
            return Ok(worktree);
        }
        let base = snapshot::tree(&run.workspace, &run.directory)
            .context("A worktree task needs a Git snapshot of the workspace")?;
        let attempt_number = self.attempt(run).number;
        let reference = snapshot::base_ref(
            &run.id,
            &format!("{}-a{attempt_number:03}-{id}", self.stage.id),
        );
        let commit = fanout::base_commit(&run.workspace, &base, &reference)?;
        let worktree = storage::metadata_dir(&run.project)?
            .join("delegates")
            .join(&run.id)
            .join(&self.stage.id)
            .join(format!("a{attempt_number:03}-{id}"));
        if let Some(parent) = worktree.parent() {
            fs::create_dir_all(parent)?;
        }
        fanout::add_worktree(&run.workspace, &worktree, &commit)?;
        if let Some(task) = self.task_mut(run, id) {
            task.worktree = Some(worktree.clone());
            task.base = Some(base);
        }
        run.persist()?;
        Ok(worktree)
    }

    /// Records a task turn's end: usage, session, handoff, and what the
    /// task asks of its parent.
    fn task_done(
        &mut self,
        run: &mut Run,
        id: &str,
        outcome: Result<process::Outcome>,
        ended: DateTime<Utc>,
    ) -> Result<()> {
        let Some(job) = self.running.remove(id) else {
            return Ok(());
        };
        if self.speaking.as_ref() == Some(&Who::Task(id.to_owned())) {
            self.speaking = None;
        }
        keep_survivor(run, &outcome);
        let stdout = job.prepared.directory.join("stdout.log");
        let usage = handoff::usage_file(&stdout, job.prepared.invocation.kind);
        let session_reported = read_head(&stdout, 64 * 1024)
            .ok()
            .and_then(|head| handoff::session_id(&head));
        let can_answer = adapters::has_follow_up(&job.stage.harness, &run.adapters);
        let cancelled_by_parent = self.task(run, id).is_some_and(|t| t.cancelled_by_parent);
        // Status and result of this turn.
        let (status, error, handoff_result, questions) = match &outcome {
            Err(error) => (TaskStatus::Failed, Some(format!("{error:#}")), None, vec![]),
            Ok(o) if o.cancelled => (
                TaskStatus::Cancelled,
                Some(if cancelled_by_parent {
                    "Cancelled by the parent".into()
                } else {
                    "Cancelled".into()
                }),
                None,
                vec![],
            ),
            Ok(o) => match evaluate_outcome(Mode::Unattended, &job.prepared, o)? {
                Exit::Stopped(_, message) => (TaskStatus::Failed, Some(message), None, vec![]),
                Exit::Succeeded(normalized) => {
                    match resolve_handoff(&job.stage, &job.prepared, normalized)? {
                        Resolved::Stopped(_, message) => {
                            (TaskStatus::Failed, Some(message), None, vec![])
                        }
                        Resolved::Handoff(_, handoff) => {
                            let asks = handoff.verdict == Verdict::Blocked
                                && !handoff.unresolved.is_empty();
                            let questions = if asks {
                                handoff.unresolved.clone()
                            } else {
                                vec![]
                            };
                            let session = self
                                .task(run, id)
                                .and_then(|t| t.turns.last())
                                .and_then(|t| t.session.clone())
                                .or(session_reported.clone());
                            let status = if asks && can_answer && session.is_some() {
                                TaskStatus::Waiting
                            } else if handoff.verdict == Verdict::Blocked {
                                TaskStatus::Blocked
                            } else {
                                TaskStatus::Completed
                            };
                            (status, None, Some(*handoff), questions)
                        }
                    }
                }
            },
        };
        let tree_after = job
            .prepared
            .sandbox
            .as_ref()
            .and_then(|_| self.task(run, id))
            .filter(|t| t.isolation == Isolation::Worktree)
            .and_then(|t| t.worktree.clone())
            .and_then(|w| snapshot::tree(&w, &run.directory));
        let task = self.task_mut(run, id).context("No such task")?;
        if let Some(turn) = task.turns.last_mut() {
            turn.finished_at = Some(ended);
            turn.exit_code = outcome.as_ref().ok().and_then(|o| o.code);
            turn.status = status;
            turn.error = error.clone();
            turn.usage = usage;
            turn.set_group(None);
            if turn.session.is_none() {
                turn.session = session_reported;
            }
            turn.tree_after = tree_after;
        }
        task.status = status;
        task.error = error;
        task.questions = questions;
        if let Some(handoff) = handoff_result {
            if task.handoff.is_none() {
                task.handoff = Some(handoff);
            } else {
                task.latest_handoff = Some(handoff);
            }
        }
        if task.cancelled_by_parent {
            task.status = TaskStatus::Cancelled;
            task.delivered = true;
        } else {
            task.delivered = false;
        }
        if task.isolation == Isolation::Worktree && !task.cancelled_by_parent {
            self.seal_patch(run, id)?;
        }
        run.persist()?;
        self.changed.bump();
        Ok(())
    }

    /// A worktree task's sealed patch for its latest turn: the full diff from
    /// its base, in router-owned storage, hashed in `run.json`.
    fn seal_patch(&mut self, run: &mut Run, id: &str) -> Result<()> {
        let Some(task) = self.task(run, id).cloned() else {
            return Ok(());
        };
        let (Some(base), Some(turn)) = (task.base.clone(), task.turns.last()) else {
            return Ok(());
        };
        let Some(after) = turn.tree_after.clone() else {
            return Ok(());
        };
        let tasks_dir = turn.directory.parent().context("No task directory")?;
        let patches = tasks_dir.join("patches");
        fs::create_dir_all(&patches)?;
        let revision = task.patches.len() + 1;
        let path = patches.join(format!("patch-{revision:03}.diff"));
        let worktree = task.worktree.clone().context("No worktree")?;
        fanout::write_patch(&worktree, &base, &after, &path)?;
        let bytes = fs::read(&path)?;
        let sha256 = sha256_hex(&bytes);
        let changes = snapshot::changes(&worktree, &base, &after).unwrap_or_default();
        let files: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
        let added = changes.iter().filter_map(|c| c.added).sum();
        let removed = changes.iter().filter_map(|c| c.removed).sum();
        if let Some(blocked) =
            crate::policy::protected_detail(&run.workflow.protected_paths, &files)
        {
            let task = self.task_mut(run, id).context("No such task")?;
            task.status = TaskStatus::Blocked;
            task.error = Some(format!("protected_path: the task changed {blocked}"));
        }
        let task = self.task_mut(run, id).context("No such task")?;
        task.patches.push(crate::delegation::records::SealedPatch {
            revision,
            turn: turn.number,
            path,
            sha256,
            files,
            added,
            removed,
            apply: None,
        });
        Ok(())
    }

    fn status(&mut self, run: &mut Run, ids: &[String], deliver: bool) -> Reply {
        let mut views = vec![];
        let mut changed = false;
        for id in ids {
            let task = self
                .task(run, id)
                .ok_or_else(|| ToolError::new("task_not_found", format!("No task {id}")))?
                .clone();
            if deliver
                && !task.status.is_live()
                && task.queued_messages.is_empty()
                && !task.delivered
            {
                if let Some(task) = self.task_mut(run, id) {
                    task.delivered = true;
                    changed = true;
                }
            }
            let task = self.task(run, id).expect("exists");
            views.push(self.view(run, task));
        }
        if changed {
            run.persist()
                .map_err(|e| ToolError::new("orchestration_error", format!("{e:#}")))?;
        }
        Ok(Value::Array(views))
    }

    fn cancel_task(&mut self, run: &mut Run, id: &str, reason: Option<String>) -> Reply {
        let task = self
            .task(run, id)
            .ok_or_else(|| ToolError::new("task_not_found", format!("No task {id}")))?
            .clone();
        if task.status.is_terminal() {
            return Ok(self.view(run, &task));
        }
        if let Some(job) = self.running.get(id) {
            job.abort.store(true, Ordering::SeqCst);
        }
        self.queue.retain(|q| q != id);
        let task = self.task_mut(run, id).expect("exists");
        task.cancelled_by_parent = true;
        task.delivered = true;
        task.queued_messages.clear();
        if let Some(reason) = reason {
            task.error = Some(format!("Cancelled by the parent: {reason}"));
        }
        if !task.status.is_live() || task.status == TaskStatus::Queued {
            task.status = TaskStatus::Cancelled;
        }
        run.persist()
            .map_err(|e| ToolError::new("orchestration_error", format!("{e:#}")))?;
        self.changed.bump();
        let task = self.task(run, id).expect("exists");
        Ok(self.view(run, task))
    }

    fn send<'s, 'e>(
        &mut self,
        scope: &'s thread::Scope<'s, 'e>,
        run: &mut Run,
        arguments: &Value,
    ) -> Reply
    where
        'a: 's,
    {
        let id = arguments
            .get("task_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::new("invalid_request", "task_id is required"))?
            .to_owned();
        let message = arguments
            .get("message")
            .and_then(Value::as_str)
            .filter(|m| !m.trim().is_empty())
            .ok_or_else(|| ToolError::new("invalid_request", "message is required"))?
            .to_owned();
        let restart = arguments.get("mode").and_then(Value::as_str) == Some("restart");
        let task = self
            .task(run, &id)
            .ok_or_else(|| ToolError::new("task_not_found", format!("No task {id}")))?
            .clone();
        let has_session = task.turns.iter().any(|t| t.session.is_some());
        if !adapters::has_follow_up(&task.target.harness, &run.adapters) || !has_session {
            return Err(ToolError::new(
                "task_not_sendable",
                format!(
                    "{} cannot resume this task's session, so it takes no follow-up",
                    task.target.harness
                ),
            ));
        }
        if task.cancelled_by_parent || task.status == TaskStatus::Cancelled {
            return Err(ToolError::new(
                "task_not_sendable",
                "The task was cancelled",
            ));
        }
        if task.turns.len() > MAX_TASK_SENDS {
            return Err(ToolError::new(
                "task_limit_exceeded",
                format!("A task takes at most {MAX_TASK_SENDS} follow-ups"),
            ));
        }
        if let Some(t) = self.task_mut(run, &id) {
            t.queued_messages.push(message);
            t.delivered = false;
            t.status = if t.status.is_live() {
                t.status
            } else {
                TaskStatus::Queued
            };
        }
        if restart {
            if let Some(job) = self.running.get(&id) {
                job.abort.store(true, Ordering::SeqCst);
            }
        }
        run.persist()
            .map_err(|e| ToolError::new("orchestration_error", format!("{e:#}")))?;
        self.launch_queued(scope, run)
            .map_err(|e| ToolError::new("orchestration_error", format!("{e:#}")))?;
        self.changed.bump();
        let task = self.task(run, &id).expect("exists");
        Ok(self.view(run, task))
    }

    fn apply(&mut self, run: &mut Run, arguments: &Value) -> Reply {
        let id = arguments
            .get("task_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::new("invalid_request", "task_id is required"))?
            .to_owned();
        let revision = arguments
            .get("revision")
            .and_then(Value::as_u64)
            .ok_or_else(|| ToolError::new("invalid_request", "revision is required"))?
            as usize;
        super::delegate_apply::apply_patch(
            run,
            self.index,
            &self.stage,
            &self.ceiling,
            &id,
            revision,
        )
        .map(|outcome| {
            let task = self.task(run, &id).expect("checked");
            let mut view = self.view(run, task);
            view["applied_revision"] = json!(revision);
            view["apply"] = outcome;
            view
        })
    }

    /// A task as the tools return it.
    fn view(&self, run: &Run, task: &TaskState) -> Value {
        let handoff = task.handoff.as_ref();
        let latest = task.latest_handoff.as_ref();
        let turn = task.turns.last();
        let has_follow_up = adapters::has_follow_up(&task.target.harness, &run.adapters);
        let unfinished = task.status.is_live() || !task.queued_messages.is_empty();
        let usage = task
            .turns
            .iter()
            .filter_map(|t| t.usage)
            .reduce(|a, b| a + b);
        json!({
            "task_id": task.id,
            "title": task.title,
            "status": task.status.label(),
            "work_state": if unfinished { "working" } else { task.status.work_state() },
            "target": task.target,
            "executed_model": turn.map(|t| t.model.clone()),
            "role": task.role.label(),
            "isolation": task.isolation.label(),
            "turns": task.turns.len(),
            "summary": handoff.map(|h| h.summary.clone()),
            "verdict": handoff.map(|h| verdict_label(h.verdict)),
            "handoff_file": task.turns.iter().find(|t| t.handoff_file.is_file()).map(|t| t.handoff_file.clone()),
            "findings": handoff.map(|h| h.findings.len()),
            "questions": task.questions,
            "has_follow_up": has_follow_up,
            "worktree": task.worktree,
            "patches": task.patches.iter().map(|p| json!({
                "revision": p.revision,
                "turn": p.turn,
                "files": p.files,
                "added": p.added,
                "removed": p.removed,
                "applied": p.apply.as_ref().is_some_and(|a| a.applied_at.is_some()),
            })).collect::<Vec<_>>(),
            "usage": usage,
            "error": task.error,
            "latest_turn_status": (task.turns.len() > 1).then(|| turn.map(|t| t.status.label())).flatten(),
            "latest_turn_summary": latest.map(|h| h.summary.clone()),
            "wait_timed_out": false,
            "next": unfinished.then(|| instructions::next_hint(self.kind)),
        })
    }
}

fn add_usage(a: Option<handoff::Usage>, b: Option<handoff::Usage>) -> Option<handoff::Usage> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        (a, b) => a.or(b),
    }
}

fn verdict_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Complete => "complete",
        Verdict::Pass => "pass",
        Verdict::NeedsChanges => "needs_changes",
        Verdict::Blocked => "blocked",
    }
}

fn target_value(target: &ResolvedTarget) -> Value {
    match &target.delegate {
        Some(name) => json!({"delegate": name}),
        None => json!({
            "harness": target.harness,
            "model": target.model,
            "provider": target.provider,
            "effort": target.effort,
        }),
    }
}

/// A shared task also sets its harness's native read-only choice, when it
/// has one, beside its read-only OS sandbox.
fn force_native_read_only(stage: &mut Stage) {
    if let Some((key, choice)) = crate::harness::get(&stage.harness).and_then(|s| s.read_only) {
        stage.harness_settings.insert(key.into(), choice.into());
    }
}

fn is_git(workspace: &Path) -> bool {
    snapshot::is_repository(workspace)
}

/// SHA-256 of `bytes`, hex-encoded.
pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A task's prompt: its brief, its role's contract, and how to ask.
fn task_prompt(
    task: &TaskState,
    parent: &Stage,
    cwd: &Path,
    handoff_file: &Path,
    schema_file: &Path,
) -> String {
    let checkout = match task.isolation {
        Isolation::Shared => "This checkout is read-only for you: report, do not edit.",
        Isolation::Worktree => {
            "This is your own Git worktree: make your changes here. Do not commit, merge, or push."
        }
    };
    format!(
        "You are a delegated task (role: {}) of stage {:?} in a model-router run.\n\nTASK\n{}\n\nWORKING DIRECTORY\n{}\n{checkout}\n\n{}\nFollow the project's own instructions. Do not change router configuration or run state.\n{}",
        task.role.label(),
        parent.name,
        task.task,
        cwd.display(),
        instructions::TASK_QUESTION_LINE,
        handoff::contract(task.role, handoff_file, schema_file)
    )
}

use std::sync::Mutex;
