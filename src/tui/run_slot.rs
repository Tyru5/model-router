//! A run of this session: its driver thread, output log, and view state.
use super::{completion, output_search, theme::Theme};
use crate::{
    engine::{self, Action, Event as RunEvent, Run, Status},
    notify, process, storage,
};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    panic::{self, AssertUnwindSafe},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// What a worker thread hands the UI. The worker never blocks on it: the
/// latest state replaces older ones, and output beyond `PENDING_LINES`
/// drops its oldest lines, counted so the log can mark the gap.
#[derive(Default)]
pub(super) struct Inbox {
    state: Option<Box<Run>>,
    output: VecDeque<String>,
    skipped: usize,
    /// The final run and result. No run means the worker panicked; the UI
    /// then reloads the saved run (see `RunSlot::crashed`).
    done: Option<(Option<Box<Run>>, Outcome)>,
}
/// How a worker's run ended.
type Outcome = std::result::Result<Action, String>;
/// Output lines a worker keeps while the UI is not reading, e.g. while
/// another run's interactive session has the terminal.
const PENDING_LINES: usize = 2000;
/// A stage that stopped this recently cannot be continued by the same key.
pub(super) const SETTLE: Duration = Duration::from_millis(600);
/// Output lines kept per run.
pub(super) const LOG_LINES: usize = 400;
impl Inbox {
    fn push(&mut self, event: RunEvent) {
        match event {
            RunEvent::State(run) => self.state = Some(run),
            RunEvent::Output(text) => {
                for line in text.lines() {
                    if self.output.len() >= PENDING_LINES {
                        self.output.pop_front();
                        self.skipped += 1;
                    }
                    self.output.push_back(storage::truncate(line, 1500));
                }
            }
        }
    }
}
type Shared = Arc<Mutex<Inbox>>;
fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Inbox> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
type Notify = fn(&Outcome) -> bool;
pub(super) struct Worker {
    inbox: Shared,
    thread: Option<JoinHandle<()>>,
    terminal: Option<process::InteractiveSession>,
    notify: Notify,
}
impl Worker {
    /// Runs `work` on a thread that reports through the returned worker.
    /// When the work ends, the user is notified of results `notify` accepts.
    /// Notifying from the worker thread reaches the user even while another
    /// run's interactive session has the screen.
    /// A panic in `work` fails the run with the panic message.
    pub(super) fn spawn(
        terminal: Option<process::InteractiveSession>,
        notify: Notify,
        work: impl FnOnce(&mut dyn FnMut(RunEvent)) -> (Run, std::result::Result<Action, String>)
        + Send
        + 'static,
    ) -> Self {
        let inbox = Shared::default();
        let shared = inbox.clone();
        let session = terminal.clone();
        let thread = thread::spawn(move || {
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                work(&mut |event| lock(&shared).push(event))
            }));
            if let Some(session) = session {
                session.finish();
            }
            let done = match outcome {
                Ok((run, result)) => {
                    if notify(&result) {
                        notify::notify_outcome(&run, &result);
                    }
                    (Some(Box::new(run)), result)
                }
                // The UI marks the run failed and notifies; it has the run.
                Err(payload) => (None, Err(panicked(payload.as_ref()))),
            };
            lock(&shared).done = Some(done);
        });
        Self {
            inbox,
            thread: Some(thread),
            terminal,
            notify,
        }
    }
}
#[cfg(test)]
impl Worker {
    /// A worker without a thread, so tests can put the app in its running state.
    pub(super) fn stub() -> Self {
        Self {
            inbox: Shared::default(),
            thread: None,
            terminal: None,
            notify: |_| false,
        }
    }
    /// What a worker thread would report.
    pub(super) fn send(&self, event: RunEvent) {
        lock(&self.inbox).push(event);
    }
    pub(super) fn finish(&self, run: Run, result: std::result::Result<Action, String>) {
        lock(&self.inbox).done = Some((Some(Box::new(run)), result));
    }
}
/// The message for a worker that panicked, with the panic's own text.
fn panicked(payload: &(dyn std::any::Any + Send)) -> String {
    let detail = payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "no message".into());
    format!("The run's driver panicked: {detail}. Inspect artifacts and retry explicitly")
}

/// A run started or opened in this session, with its view state.
pub(super) struct RunSlot {
    pub(super) run: Run,
    pub(super) worker: Option<Worker>,
    pub(super) cancel: Arc<AtomicBool>,
    pub(super) logs: VecDeque<String>,
    /// Counts lines ever added to `logs`, so cached wrapping knows when it is stale.
    pub(super) log_version: u64,
    /// Following new output, or paused at a line.
    pub(super) log_view: output_search::LogView,
    /// Rows of the output panel, as last drawn.
    pub(super) log_visible: Cell<usize>,
    pub(super) log_search: Option<output_search::LogSearch>,
    /// A finished run shows results unless the output log was chosen.
    pub(super) show_output: bool,
    /// Scroll offset of the finished run's brief.
    pub(super) results_scroll: u16,
    /// The furthest the brief can scroll, as last drawn.
    pub(super) results_limit: Cell<u16>,
    pub(super) changes: Option<completion::Loaded>,
    /// The key of the changes being computed.
    pub(super) changes_pending: Option<completion::CompletionKey>,
    /// The run checkpoint whose waiver the status line last announced.
    pub(super) waiver_shown: String,
    /// Reopen the stage's session interactively once its worker stops.
    pub(super) takeover: bool,
    /// When the worker last stopped; a key pressed just after cannot continue.
    pub(super) stopped_at: Option<Instant>,
    /// Drawing caches, so frames do not rebuild what has not changed.
    pub(super) cache: RefCell<DrawCache>,
}
/// Width, log version, and theme the output was wrapped for.
pub(super) type OutputKey = (usize, u64, Theme);
/// Width, log version, and query the search matches were built for.
pub(super) type SearchKey = (usize, u64, String);
pub(super) type SearchMatches = std::rc::Rc<Vec<(usize, std::ops::Range<usize>)>>;
/// Wrapped output, search matches, and results brief from the last frame, with what they were built from.
#[derive(Default)]
pub(super) struct DrawCache {
    pub(super) output: Option<(OutputKey, std::rc::Rc<output_search::Wrapped>)>,
    pub(super) search: Option<(SearchKey, SearchMatches)>,
    pub(super) report: Option<(completion::ReportKey, std::rc::Rc<crate::report::RunReport>)>,
    pub(super) brief: Option<(completion::BriefKey, std::rc::Rc<completion::CachedBrief>)>,
}
impl RunSlot {
    pub(super) fn new(run: Run) -> Self {
        Self {
            run,
            worker: None,
            cancel: Arc::new(AtomicBool::new(false)),
            logs: VecDeque::new(),
            log_version: 0,
            log_view: Default::default(),
            log_visible: Cell::new(0),
            log_search: None,
            show_output: false,
            results_scroll: 0,
            results_limit: Cell::new(0),
            changes: None,
            changes_pending: None,
            waiver_shown: String::new(),
            takeover: false,
            stopped_at: None,
            cache: RefCell::default(),
        }
    }
    pub(super) fn terminal(&self) -> Option<&process::InteractiveSession> {
        self.worker.as_ref()?.terminal.as_ref()
    }
    /// Whether the run still needs the router: driving, or waiting on the user.
    pub(super) fn in_progress(&self) -> bool {
        self.worker.is_some()
            || matches!(
                self.run.status,
                Status::Pending
                    | Status::Running
                    | Status::Interactive
                    | Status::AwaitingConfirmation
            )
    }
    /// `engine::authorize_replay` for this idle run, before a command changes
    /// it; what recovery stopped or left running goes to the output log.
    pub(super) fn authorize_replay(
        &mut self,
        explicit: Option<&std::path::Path>,
    ) -> anyhow::Result<()> {
        let mut notes = String::new();
        let result = engine::authorize_replay(&mut self.run, explicit, |event| {
            if let RunEvent::Output(text) = event {
                notes.push_str(&text);
            }
        });
        for line in notes.lines() {
            self.push_log(storage::truncate(line, 1500));
        }
        result
    }
    pub(super) fn push_log(&mut self, line: String) {
        self.logs.push_back(line);
        self.log_version += 1;
        while self.logs.len() > LOG_LINES {
            self.logs.pop_front();
        }
    }
    /// Applies what the worker reported. Returns whether anything changed,
    /// and the driver's result once it stops.
    pub(super) fn poll(&mut self) -> (bool, Option<std::result::Result<Action, String>>) {
        let Some(worker) = &self.worker else {
            self.cancel.store(false, Ordering::Relaxed);
            return (false, None);
        };
        let (state, mut output, mut skipped, done) = {
            let mut inbox = lock(&worker.inbox);
            (
                inbox.state.take(),
                std::mem::take(&mut inbox.output),
                std::mem::take(&mut inbox.skipped),
                inbox.done.take(),
            )
        };
        let changed = state.is_some() || !output.is_empty() || skipped > 0 || done.is_some();
        if let Some(run) = state {
            self.run = *run;
        }
        // Lines the log could not keep anyway count toward the gap, so its
        // marker stays in view above what follows.
        if skipped > 0 && output.len() >= LOG_LINES {
            let extra = output.len() + 1 - LOG_LINES;
            output.drain(..extra);
            skipped += extra;
        }
        if skipped > 0 {
            self.push_log(format!(
                "… {skipped} lines skipped while the view was busy; stdout.log has them"
            ));
        }
        for line in output {
            self.push_log(line);
        }
        let mut stopped = None;
        if let Some((run, result)) = done {
            match run {
                Some(run) => self.run = *run,
                None => self.crashed(&result),
            }
            if let Some(mut worker) = self.worker.take() {
                if let Some(handle) = worker.thread.take() {
                    let _ = handle.join();
                }
            }
            self.stopped_at = Some(Instant::now());
            self.cancel.store(false, Ordering::Relaxed);
            stopped = Some(self.settle(result));
        }
        (changed, stopped)
    }
    /// Fails the run whose worker panicked, in memory only: the record on
    /// disk still says running, which the next drive recovers as interrupted.
    /// The worker may have saved the run after the last state it reported
    /// (a launch records its process group without one), so the saved run,
    /// which its unwinding has unlocked by now, is the latest; the last
    /// reported state only stands in when it cannot be read.
    fn crashed(&mut self, result: &std::result::Result<Action, String>) {
        let Err(message) = result else {
            return;
        };
        if let Ok(saved) = engine::load(&self.run.project, &self.run.id) {
            self.run = saved;
        }
        let running = self
            .run
            .stages
            .get(self.run.cursor)
            .and_then(|stage| stage.attempts.last())
            .is_some_and(|attempt| attempt.status == Status::Running);
        let failed = self.run.stop(
            Status::Failed,
            message.clone(),
            crate::transitions::Actor::Engine,
            running,
        );
        if failed.is_err() {
            self.run.status = Status::Failed;
            self.run.error = Some(message.clone());
        }
        let notify = self
            .worker
            .as_ref()
            .is_some_and(|worker| (worker.notify)(result));
        if notify {
            notify::notify_outcome(&self.run, result);
        }
    }
    /// A takeover turns the cancelled stage into one waiting for the terminal.
    fn settle(
        &mut self,
        result: std::result::Result<Action, String>,
    ) -> std::result::Result<Action, String> {
        if !std::mem::take(&mut self.takeover) || result != Ok(Action::Stopped) {
            return result;
        }
        self.run
            .command(engine::Cmd::TakeOver)
            .map(|()| Action::Interactive)
            .map_err(|e| format!("{e:#}"))
    }
}
impl Drop for RunSlot {
    fn drop(&mut self) {
        if !self.terminal().is_some_and(|t| t.released()) {
            self.cancel.store(true, Ordering::Relaxed);
        }
        if let Some(mut worker) = self.worker.take() {
            if let Some(thread) = worker.thread.take() {
                let _ = thread.join();
            }
        }
    }
}
