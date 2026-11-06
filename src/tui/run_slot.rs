//! A run of this session: its driver thread, output log, and Monitor view state.
use super::{completion, output_search, theme::Theme};
use crate::{
    engine::{self, Action, Event as RunEvent, Run, Status},
    notify, process, storage,
};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
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
    done: Option<(Box<Run>, std::result::Result<Action, String>)>,
}
/// Output lines a worker keeps while the UI is not reading, e.g. while
/// another run's interactive session has the terminal.
const PENDING_LINES: usize = 2000;
/// A stage that stopped this recently cannot be continued by the same key.
pub(super) const SETTLE: Duration = Duration::from_millis(600);
/// Lines the Monitor keeps per run.
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
pub(super) struct Worker {
    inbox: Shared,
    thread: Option<JoinHandle<()>>,
    terminal: Option<process::InteractiveSession>,
}
impl Worker {
    /// Runs `work` on a thread that reports through the returned worker.
    /// When the work ends, the user is notified of results `notify` accepts.
    /// Notifying from the worker thread reaches the user even while another
    /// run's interactive session has the screen.
    pub(super) fn spawn(
        terminal: Option<process::InteractiveSession>,
        notify: fn(&std::result::Result<Action, String>) -> bool,
        work: impl FnOnce(&mut dyn FnMut(RunEvent)) -> (Run, std::result::Result<Action, String>)
        + Send
        + 'static,
    ) -> Self {
        let inbox = Shared::default();
        let shared = inbox.clone();
        let thread = thread::spawn(move || {
            let (run, result) = work(&mut |event| lock(&shared).push(event));
            if notify(&result) {
                notify::notify_outcome(&run, &result);
            }
            lock(&shared).done = Some((Box::new(run), result));
        });
        Self {
            inbox,
            thread: Some(thread),
            terminal,
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
        }
    }
    /// What a worker thread would report.
    pub(super) fn send(&self, event: RunEvent) {
        lock(&self.inbox).push(event);
    }
    pub(super) fn finish(&self, run: Run, result: std::result::Result<Action, String>) {
        lock(&self.inbox).done = Some((Box::new(run), result));
    }
}

/// A run started or opened in this session, with its Monitor view state.
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
    /// Scroll offsets of the results panels, each kept within its own panel.
    pub(super) results_scroll: [u16; 2],
    /// The furthest each results panel can scroll, as last drawn.
    pub(super) results_limit: Cell<[u16; 2]>,
    pub(super) changes: Option<completion::Loaded>,
    /// The key of the changes being computed.
    pub(super) changes_pending: Option<String>,
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
/// Wrapped output and the results report from the last frame, with what they were built from.
#[derive(Default)]
pub(super) struct DrawCache {
    pub(super) output: Option<(OutputKey, std::rc::Rc<output_search::Wrapped>)>,
    pub(super) report: Option<(String, std::rc::Rc<crate::report::RunReport>)>,
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
            results_scroll: [0; 2],
            results_limit: Cell::new([0; 2]),
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
            self.run = *run;
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
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(mut worker) = self.worker.take() {
            if let Some(thread) = worker.thread.take() {
                let _ = thread.join();
            }
        }
    }
}
