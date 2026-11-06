use super::{
    actions::{self, ActionId, Scope},
    completion,
    harness_editor::HarnessEditor,
    harnesses::{HarnessChecks, draw_harnesses},
    history::{RunsView, draw_history},
    modal::{Ask, Choice, InputTarget, Modal, PickValue, draw_modal, matching_choices},
    monitor::draw_run,
    path_picker::PathPicker,
    theme::{self, Palette, Theme},
    widgets::{dim, rows_for, short_path, textarea},
    workflow_page::{StageEditor, draw_workflow},
};
use crate::{
    catalog::{self, Catalog},
    config::{self, Settings, Stage},
    engine::{self, Action, Event as RunEvent, Placement, Run, Status},
    history::Entry,
    process, storage,
};
use anyhow::{Context, Result, bail, ensure};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
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
    fn spawn(
        terminal: Option<process::InteractiveSession>,
        work: impl FnOnce(&mut dyn FnMut(RunEvent)) -> (Run, std::result::Result<Action, String>)
        + Send
        + 'static,
    ) -> Self {
        let inbox = Shared::default();
        let shared = inbox.clone();
        let thread = thread::spawn(move || {
            let (run, result) = work(&mut |event| lock(&shared).push(event));
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
    pub(super) run: Option<Run>,
    pub(super) worker: Option<Worker>,
    pub(super) cancel: Arc<AtomicBool>,
    pub(super) logs: VecDeque<String>,
    /// Counts lines ever added to `logs`, so cached wrapping knows when it is stale.
    pub(super) log_version: u64,
    pub(super) log_scroll: usize,
    /// A finished run shows results unless the output log was chosen.
    pub(super) show_output: bool,
    /// Scroll offsets of the results panels, each kept within its own panel.
    pub(super) results_scroll: [u16; 2],
    /// The furthest each results panel can scroll, as last drawn.
    pub(super) results_limit: Cell<[u16; 2]>,
    pub(super) changes: Option<completion::Loaded>,
    pub(super) changes_job: Option<(String, Receiver<completion::Loaded>)>,
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
    pub(super) output: Option<(OutputKey, std::rc::Rc<Vec<Line<'static>>>)>,
    pub(super) report: Option<(String, std::rc::Rc<crate::report::RunReport>)>,
}
impl Default for RunSlot {
    fn default() -> Self {
        Self {
            run: None,
            worker: None,
            cancel: Arc::new(AtomicBool::new(false)),
            logs: VecDeque::new(),
            log_version: 0,
            log_scroll: 0,
            show_output: false,
            results_scroll: [0; 2],
            results_limit: Cell::new([0; 2]),
            changes: None,
            changes_job: None,
            waiver_shown: String::new(),
            takeover: false,
            stopped_at: None,
            cache: RefCell::default(),
        }
    }
}
impl RunSlot {
    pub(super) fn terminal(&self) -> Option<&process::InteractiveSession> {
        self.worker.as_ref()?.terminal.as_ref()
    }
    /// Whether the run still needs the router: driving, or waiting on the user.
    pub(super) fn in_progress(&self) -> bool {
        self.worker.is_some()
            || self.run.as_ref().is_some_and(|run| {
                matches!(
                    run.status,
                    Status::Pending
                        | Status::Running
                        | Status::Interactive
                        | Status::AwaitingConfirmation
                )
            })
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
    fn poll(&mut self) -> (bool, Option<std::result::Result<Action, String>>) {
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
            self.run = Some(*run);
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
            self.run = Some(*run);
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
        let run = self.run.as_mut().ok_or("No run to take over")?;
        run.take_over()
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

/// What the status line says when a run outside the Monitor stops driving.
fn notice(run: &Run, result: &std::result::Result<Action, String>) -> String {
    let stage = run.current_name();
    let id = &run.id;
    match result {
        Ok(Action::Interactive) => format!(
            "Run {id}: {stage} is ready in its native terminal. Monitor [2], [ ] to select the run, then [i]."
        ),
        Ok(Action::Confirm) => format!("Run {id}: {stage} awaits handoff review in Monitor [2]."),
        Ok(Action::Complete) => format!("Run {id} ({}) complete.", run.workflow.name),
        Ok(Action::Stopped) => format!(
            "Run {id} stopped: {}",
            run.error.as_deref().unwrap_or("no error recorded")
        ),
        Err(error) => format!("Run {id}: {error}"),
    }
}
pub(super) struct App {
    pub(super) project: PathBuf,
    pub(super) config_path: PathBuf,
    pub(super) theme: Theme,
    pub(super) theme_path: PathBuf,
    pub(super) vim_motions: bool,
    pub(super) settings: Settings,
    pub(super) workflow: usize,
    pub(super) selected: usize,
    pub(super) task: String,
    pub(super) worktree: bool,
    pub(super) dirty: bool,
    pub(super) page: usize,
    pub(super) modal: Option<Modal>,
    pub(super) message: String,
    /// Last error shown; the status line turns red while it is current.
    pub(super) alert: String,
    /// The run the Monitor shows. Other runs of this session keep going in
    /// `parked`; switching focus swaps a parked slot in.
    pub(super) focused: RunSlot,
    pub(super) parked: Vec<RunSlot>,
    /// Position of `focused` among the session's runs, in start order.
    pub(super) focus_index: usize,
    pub(super) signal: Arc<AtomicBool>,
    /// The focused run's interactive stage gets the terminal on the next frame.
    pub(super) launch_interactive: bool,
    pub(super) history: Vec<Entry>,
    pub(super) history_selected: usize,
    pub(super) runs: RunsView,
    pub(super) harness_selected: usize,
    pub(super) harness_checks: HarnessChecks,
    pub(super) catalog: Option<Receiver<std::result::Result<Catalog, String>>>,
    pub(super) model_editor: Option<Box<HarnessEditor>>,
    /// Stage editor parked while its model picker is open.
    pub(super) stage_editor: Option<(Box<StageEditor>, Option<usize>)>,
    /// Harness whose catalog is being read.
    pub(super) catalog_harness: String,
    /// Catalogs read so far, by harness; they supply effort levels to editors.
    pub(super) catalogs: BTreeMap<String, Catalog>,
    /// A catalog being read for an editor, without a picker.
    pub(super) prefetch: Option<(String, Job<std::result::Result<Catalog, String>>)>,
    pub(super) prefetch_queue: VecDeque<String>,
    /// Git facts for the project, refreshed in the background.
    pub(super) repo: Option<storage::RepoInfo>,
    repo_job: Option<Job<Option<storage::RepoInfo>>>,
    /// When the Git facts were last read; None reads them on the next poll.
    repo_checked: Option<Instant>,
    pub(super) quit_after_stop: bool,
    /// Counts project switches; background results for an earlier project are dropped.
    pub(super) generation: u64,
    /// A run being created (worktree, snapshot) before it starts.
    pub(super) starting: Option<Job<std::result::Result<Started, String>>>,
    /// A project being opened, with the browser to return to on failure.
    project_job: Option<(ProjectJob, Box<PathPicker>)>,
    /// Saved runs being read for the Runs page.
    pub(super) runs_job: Option<Job<std::result::Result<Vec<Entry>, String>>>,
    /// The Runs page needs reading when it is next shown.
    pub(super) runs_stale: bool,
    pub(super) runs_loaded: Option<Instant>,
    /// Workflow snapshots from before removals and deletions, newest last.
    pub(super) undo: Vec<Snapshot>,
    /// Something changed that the next frame should show.
    pub(super) redraw: bool,
}

/// Work on a background thread, tagged with the project generation it serves.
pub(super) struct Job<T> {
    generation: u64,
    receiver: Receiver<T>,
}
pub(super) enum Got<T> {
    Pending,
    Ready(T),
    /// The thread ended without a result.
    Lost,
}
impl<T: Send + 'static> Job<T> {
    pub(super) fn spawn(generation: u64, work: impl FnOnce() -> T + Send + 'static) -> Self {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(work());
        });
        Self {
            generation,
            receiver,
        }
    }
    #[cfg(test)]
    pub(super) fn from_receiver(generation: u64, receiver: Receiver<T>) -> Self {
        Self {
            generation,
            receiver,
        }
    }
    fn get(&self) -> Got<T> {
        match self.receiver.try_recv() {
            Ok(value) => Got::Ready(value),
            Err(mpsc::TryRecvError::Empty) => Got::Pending,
            Err(mpsc::TryRecvError::Disconnected) => Got::Lost,
        }
    }
}

type ProjectJob = Job<std::result::Result<Opened, String>>;
/// A created run, ready for its driver.
pub(super) struct Started {
    run: Run,
    /// What was using the checkout, when the run got a worktree instead.
    holder: Option<String>,
    /// The task as submitted; the editor clears only if it still holds it.
    task: Option<String>,
    rerun_of: Option<String>,
}
/// A project read in the background.
pub(super) struct Opened {
    project: PathBuf,
    settings: Settings,
    config_path: PathBuf,
    repo: Option<storage::RepoInfo>,
}
/// The workflows before a removal or deletion, for [u].
pub(super) struct Snapshot {
    workflows: Vec<config::Workflow>,
    workflow: usize,
    selected: usize,
    what: String,
}
const UNDO_DEPTH: usize = 20;
/// How often the header re-reads Git status, and Runs re-reads active runs.
const REPO_EVERY: Duration = Duration::from_secs(5);
const RUNS_EVERY: Duration = Duration::from_secs(3);
/// A stage that stopped this recently cannot be continued by the same key.
pub(super) const SETTLE: Duration = Duration::from_millis(600);
impl App {
    pub(super) fn new(
        project: PathBuf,
        settings: Settings,
        config_path: PathBuf,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        let repo = storage::repo_info(&project);
        Self {
            project,
            settings,
            config_path,
            theme: Theme::default(),
            theme_path: config::config_dir().join("appearance.json"),
            vim_motions: false,
            workflow: 0,
            selected: 0,
            task: String::new(),
            worktree: false,
            dirty: false,
            page: 0,
            modal: None,
            message: "Define a task, choose your stages, then run. Review is optional.".into(),
            alert: String::new(),
            focused: RunSlot::default(),
            parked: vec![],
            focus_index: 0,
            signal: cancel,
            launch_interactive: false,
            history: vec![],
            history_selected: 0,
            runs: RunsView::default(),
            harness_selected: 0,
            harness_checks: HarnessChecks::new(),
            catalog: None,
            model_editor: None,
            stage_editor: None,
            catalog_harness: String::new(),
            catalogs: BTreeMap::new(),
            prefetch: None,
            prefetch_queue: VecDeque::new(),
            repo,
            repo_job: None,
            repo_checked: Some(Instant::now()),
            quit_after_stop: false,
            generation: 0,
            starting: None,
            project_job: None,
            runs_job: None,
            runs_stale: true,
            runs_loaded: None,
            undo: vec![],
            redraw: true,
        }
    }
    pub(super) fn displayed_theme(&self) -> Theme {
        if let Some(Modal::Picker {
            items,
            selected,
            filter,
            ..
        }) = &self.modal
        {
            let choices = matching_choices(items, filter);
            if let Some(choice) = choices.get((*selected).min(choices.len().saturating_sub(1))) {
                if let PickValue::Theme(theme) = choice.value {
                    return theme;
                }
            }
        }
        self.theme
    }
    pub(super) fn palette(&self) -> Palette {
        self.displayed_theme().palette()
    }
    pub(super) fn choose_theme(&mut self) {
        self.modal = Some(Modal::Picker {
            title: "Choose theme · live preview".into(),
            items: Theme::ALL
                .into_iter()
                .map(|theme| Choice {
                    label: format!(
                        "{} · {}{}",
                        theme.name(),
                        theme.appearance(),
                        if theme == self.theme {
                            " (current)"
                        } else {
                            ""
                        }
                    ),
                    value: PickValue::Theme(theme),
                })
                .collect(),
            selected: Theme::ALL
                .iter()
                .position(|theme| *theme == self.theme)
                .unwrap_or(0),
            filter: textarea(""),
            note: format!(
                "Preview as you move. [Enter] saves for every project. [Esc] restores {}.",
                self.theme.name()
            ),
        });
    }
    pub(super) fn stage(&self) -> &Stage {
        &self.settings.workflows[self.workflow].stages[self.selected]
    }
    pub(super) fn stage_mut(&mut self) -> &mut Stage {
        &mut self.settings.workflows[self.workflow].stages[self.selected]
    }
    pub(super) fn input(&mut self, title: &str, target: InputTarget, value: &str) {
        self.modal = Some(Modal::Input {
            title: title.into(),
            target,
            text: textarea(value),
        });
    }
    pub(super) fn text(&mut self, title: &str, body: String) {
        self.modal = Some(Modal::Text {
            title: title.into(),
            body: storage::clean_terminal(&body),
            scroll: 0,
            guide: false,
        });
    }
    pub(super) fn save(&mut self, global: bool) -> Result<()> {
        let path = if global {
            config::config_dir().join("config.toml")
        } else {
            self.config_path.clone()
        };
        self.settings.save(&path)?;
        self.dirty = false;
        self.message = format!("Saved {}", path.display());
        Ok(())
    }
    /// Every run of this session, in start order, with whether it is focused.
    pub(super) fn slots(&self) -> impl Iterator<Item = (&RunSlot, bool)> {
        let focused = self.focused.run.is_some().then_some(&self.focused);
        let at = self.focus_index.min(self.parked.len());
        self.parked[..at]
            .iter()
            .map(|slot| (slot, false))
            .chain(focused.map(|slot| (slot, true)))
            .chain(self.parked[at..].iter().map(|slot| (slot, false)))
    }
    pub(super) fn any_active(&self) -> bool {
        self.focused.worker.is_some() || self.parked.iter().any(|slot| slot.worker.is_some())
    }
    pub(super) fn cancel_all(&mut self) {
        for slot in std::iter::once(&self.focused).chain(&self.parked) {
            if slot.worker.is_some() {
                slot.cancel.store(true, Ordering::Relaxed);
            }
        }
    }
    /// Focuses `run` in a fresh slot. A focused run keeps going, parked.
    pub(super) fn adopt(&mut self, run: Run) {
        let mut slot = RunSlot::default();
        std::mem::swap(&mut self.focused, &mut slot);
        if slot.run.is_some() {
            self.parked
                .insert(self.focus_index.min(self.parked.len()), slot);
        }
        self.focus_index = self.parked.len();
        self.focused.run = Some(run);
    }
    /// Focuses the session's run at `index` (start order).
    pub(super) fn focus(&mut self, index: usize) {
        let has_focus = self.focused.run.is_some();
        let total = self.parked.len() + usize::from(has_focus);
        if index >= total || (has_focus && index == self.focus_index) {
            return;
        }
        let at = if has_focus && index > self.focus_index {
            index - 1
        } else {
            index
        };
        let mut slot = self.parked.remove(at);
        std::mem::swap(&mut self.focused, &mut slot);
        if slot.run.is_some() {
            // The old focus keeps its place in start order.
            let back = if self.focus_index < index {
                self.focus_index
            } else {
                self.focus_index - 1
            };
            self.parked.insert(back.min(self.parked.len()), slot);
        }
        self.focus_index = index;
    }
    /// Focuses the next or previous run of the session, wrapping.
    pub(super) fn cycle_focus(&mut self, forward: bool) {
        let total = self.parked.len() + usize::from(self.focused.run.is_some());
        if total < 2 {
            return;
        }
        self.focus(if forward {
            (self.focus_index + 1) % total
        } else {
            (self.focus_index + total - 1) % total
        });
    }
    /// Focuses the session's slot holding run `id`; false when none does.
    pub(super) fn focus_run(&mut self, id: &str) -> bool {
        let index = self
            .slots()
            .position(|(slot, _)| slot.run.as_ref().is_some_and(|run| run.id == id));
        if let Some(index) = index {
            self.focus(index);
        }
        index.is_some()
    }
    /// Removes the focused run from this session; its files stay on disk.
    pub(super) fn dismiss(&mut self) -> Result<()> {
        ensure!(
            self.focused.worker.is_none(),
            "Cancel the stage before closing its run"
        );
        self.focused = RunSlot::default();
        if self.parked.is_empty() {
            self.focus_index = 0;
        } else {
            let at = self.focus_index.min(self.parked.len() - 1);
            self.focused = self.parked.remove(at);
            self.focus_index = at;
        }
        Ok(())
    }
    /// Where a new run in the project works. A checkout another run is using
    /// gets a worktree from HEAD, so the runs cannot edit the same files.
    /// Also returns what was using it.
    pub(super) fn placement(&self, worktree: bool) -> Result<(Placement, Option<String>)> {
        if worktree {
            return Ok((Placement::Worktree, None));
        }
        let project = std::fs::canonicalize(&self.project).unwrap_or_else(|_| self.project.clone());
        let holder = self
            .slots()
            .filter(|(slot, _)| slot.in_progress())
            .filter_map(|(slot, _)| slot.run.as_ref())
            .find(|run| run.workspace == project)
            .map(|run| format!("run {}", run.id))
            .or_else(|| {
                let lock = storage::metadata_dir(&project).ok()?.join("workspace.lock");
                storage::is_locked(&lock).then(|| "another router process".to_owned())
            });
        let Some(holder) = holder else {
            return Ok((Placement::Project, None));
        };
        ensure!(
            self.repo.is_some(),
            "{holder} is using this folder. It is not a Git repository, so a second run cannot get its own worktree; wait for the first to finish"
        );
        Ok((Placement::WorktreeFromHead, Some(holder)))
    }
    pub(super) fn start_driver(&mut self) -> Result<()> {
        ensure!(self.focused.worker.is_none(), "A stage is still running");
        let mut run = self.focused.run.clone().context("No run selected")?;
        self.focused.cancel.store(false, Ordering::Relaxed);
        let cancel = self.focused.cancel.clone();
        self.focused.worker = Some(Worker::spawn(None, move |emit| {
            let action = engine::drive(&mut run, &cancel, emit).map_err(|e| format!("{e:#}"));
            (run, action)
        }));
        Ok(())
    }
    /// Start the stage once; subsequent attachments reuse this worker's PTY.
    pub(super) fn interactive_terminal(&mut self) -> Result<process::InteractiveSession> {
        if let Some(session) = self.focused.terminal() {
            return Ok(session.clone());
        }
        ensure!(self.focused.worker.is_none(), "A stage is still running");
        let mut run = self.focused.run.clone().context("No run selected")?;
        ensure!(
            run.status == Status::Interactive,
            "No interactive stage is ready"
        );
        let (cols, rows) = crossterm::terminal::size()?;
        let session = process::InteractiveSession::new(cols, rows);
        let terminal = session.clone();
        self.focused.cancel.store(false, Ordering::Relaxed);
        let cancel = self.focused.cancel.clone();
        self.focused.worker = Some(Worker::spawn(Some(session.clone()), move |emit| {
            let result = engine::run_interactive(&mut run, &cancel, &terminal, emit)
                .map(|()| {
                    if run.status == Status::AwaitingConfirmation {
                        Action::Confirm
                    } else {
                        Action::Stopped
                    }
                })
                .map_err(|error| format!("{error:#}"));
            (run, result)
        }));
        Ok(session)
    }
    pub(super) fn poll(&mut self) -> Result<bool> {
        match &mut self.modal {
            Some(Modal::Import(importer)) => self.redraw |= importer.poll(),
            Some(Modal::Repositories(picker)) => self.redraw |= picker.poll(),
            _ => {}
        }
        let (changed, stopped) = self.focused.poll();
        self.redraw |= changed;
        if let Some(result) = stopped {
            self.run_stopped();
            self.focused_stopped(result);
        }
        for index in 0..self.parked.len() {
            let (changed, stopped) = self.parked[index].poll();
            self.redraw |= changed;
            if let Some(result) = stopped {
                self.run_stopped();
                if let Some(run) = &self.parked[index].run {
                    self.message = notice(run, &result);
                    if result.is_err() || result == Ok(Action::Stopped) {
                        self.alert = self.message.clone();
                    }
                }
            }
        }
        self.poll_jobs();
        if let Some((harness, job)) = &self.prefetch {
            match job.get() {
                Got::Pending => {}
                got => {
                    let harness = harness.clone();
                    let current = job.generation == self.generation;
                    self.prefetch = None;
                    if let (Got::Ready(Ok(catalog)), true) = (got, current) {
                        self.catalogs.insert(harness.clone(), catalog);
                        self.push_catalog(&harness);
                    }
                    if let Some(next) = self.prefetch_queue.pop_front() {
                        self.request_catalog(next);
                    }
                    self.redraw = true;
                }
            }
        }
        if let Some(receiver) = &self.catalog {
            if let Ok(result) = receiver.try_recv() {
                self.catalog = None;
                self.redraw = true;
                match result {
                    Ok(catalog) => {
                        let harness = self.catalog_harness.clone();
                        self.catalogs.insert(harness.clone(), catalog.clone());
                        self.push_catalog(&harness);
                        self.show_catalog(catalog);
                    }
                    Err(error) => {
                        self.message =
                            format!("{error}. Enter a model ID manually in harness settings [c].");
                        self.alert = self.message.clone();
                        self.modal = self.model_editor.take().map(|mut editor| {
                            editor.error = self.message.clone();
                            Modal::Harness(editor)
                        });
                        if let Some((editor, insert_at)) = self.stage_editor.take() {
                            self.message = format!("{error}. Type a model ID with [i] instead.");
                            self.alert = self.message.clone();
                            self.modal = Some(Modal::Stage { editor, insert_at });
                        }
                    }
                }
            }
        }
        match self.harness_checks.poll() {
            Some(true) => {
                self.redraw = true;
                self.harness_selected = self
                    .harness_selected
                    .min(self.harness_checks.reports.len().saturating_sub(1))
            }
            Some(false) => {
                self.redraw = true;
                self.message = "Harness check failed. Press [r] to retry.".into();
                self.alert = self.message.clone();
            }
            None => {}
        }
        self.poll_completion();
        Ok(self.quit_after_stop && !self.any_active())
    }
    /// A run stopped: Git status and the saved runs list may have changed.
    fn run_stopped(&mut self) {
        self.redraw = true;
        self.repo_checked = None;
        self.runs_stale = true;
    }
    /// Collects background results and starts timed refreshes.
    fn poll_jobs(&mut self) {
        if let Some(job) = &self.starting {
            match job.get() {
                Got::Pending => {}
                Got::Ready(result) => {
                    self.starting = None;
                    self.redraw = true;
                    let started = result
                        .map_err(anyhow::Error::msg)
                        .and_then(|started| self.begin(started));
                    if let Err(error) = started {
                        self.message = format!("{error:#}");
                        self.alert = self.message.clone();
                    }
                }
                Got::Lost => {
                    self.starting = None;
                    self.message = "Starting the run stopped unexpectedly; try again.".into();
                    self.alert = self.message.clone();
                }
            }
        }
        if let Some((job, _)) = &self.project_job {
            let got = job.get();
            if !matches!(got, Got::Pending) {
                let (_, mut picker) = self.project_job.take().expect("project job");
                self.redraw = true;
                let opened = match got {
                    Got::Ready(result) => result.map_err(anyhow::Error::msg),
                    _ => Err(anyhow::anyhow!("Opening the project stopped unexpectedly")),
                };
                if let Err(error) = opened.and_then(|opened| self.open_project(opened)) {
                    // Back to the browser, which shows why.
                    picker.error = format!("{error:#}");
                    self.modal = Some(Modal::Repositories(picker));
                } else if matches!(self.modal, Some(Modal::Loading(_))) {
                    self.modal = None;
                }
            }
        }
        if let Some(job) = &self.runs_job {
            match job.get() {
                Got::Pending => {}
                got => {
                    let current = job.generation == self.generation;
                    self.runs_job = None;
                    self.redraw = true;
                    match got {
                        Got::Ready(Ok(entries)) if current => self.show_runs(entries),
                        Got::Ready(Err(error)) if current => {
                            self.message = error;
                            self.alert = self.message.clone();
                        }
                        _ => {}
                    }
                }
            }
        }
        if self.page == 2 {
            let due = self.runs_loaded.is_none_or(|at| at.elapsed() >= RUNS_EVERY);
            if due && self.slots().any(|(slot, _)| slot.in_progress()) {
                self.runs_stale = true;
            }
            if self.runs_stale && self.runs_job.is_none() {
                self.load_runs();
            }
        }
        if let Some(job) = &self.repo_job {
            match job.get() {
                Got::Pending => {}
                got => {
                    if let (Got::Ready(repo), true) = (got, job.generation == self.generation) {
                        self.redraw |= repo != self.repo;
                        self.repo = repo;
                    }
                    self.repo_job = None;
                    self.repo_checked = Some(Instant::now());
                }
            }
        } else if self
            .repo_checked
            .is_none_or(|at| at.elapsed() >= REPO_EVERY)
        {
            let project = self.project.clone();
            self.repo_job = Some(Job::spawn(self.generation, move || {
                storage::repo_info(&project)
            }));
        }
    }
    /// Reads saved runs in the background; the page shows them when they arrive.
    pub(super) fn load_runs(&mut self) {
        self.runs_stale = false;
        let project = self.project.clone();
        self.runs_job = Some(Job::spawn(self.generation, move || {
            crate::history::list(&project).map_err(|e| format!("{e:#}"))
        }));
    }
    /// Adopts a created run and starts its driver.
    fn begin(&mut self, started: Started) -> Result<()> {
        let Started {
            run,
            holder,
            task,
            rerun_of,
        } = started;
        // An interactive first stage needs the terminal right away.
        let interactive = run
            .workflow
            .stages
            .iter()
            .find(|s| s.enabled)
            .is_some_and(|s| s.mode == config::Mode::Interactive);
        let (id, workspace) = (run.id.clone(), run.workspace.clone());
        self.adopt(run);
        self.start_driver()?;
        self.runs_stale = true;
        if let Some(source) = rerun_of {
            self.message = format!(
                "Rerunning {source} as {id}{}…",
                holder.map_or_else(String::new, |h| format!(
                    " in a worktree ({h} is using the checkout)"
                ))
            );
            self.page = 1;
            return Ok(());
        }
        // The run has its own snapshot; the editor is free for the next one.
        if task.is_some_and(|task| task == self.task) {
            self.task.clear();
        }
        self.message = match holder {
            Some(holder) => format!(
                "Started {id} in worktree {}: {holder} is using the checkout, whose uncommitted changes are not included. ",
                short_path(&workspace, 60)
            ),
            None => format!("Started {id}. "),
        };
        if interactive {
            self.page = 1;
        } else {
            self.message
                .push_str("Monitor [2] follows it. Define the next task here.");
        }
        Ok(())
    }
    /// Creates a run in the background (worktree and snapshot can be slow);
    /// it starts once ready.
    pub(super) fn start_run(
        &mut self,
        create: impl FnOnce(Placement) -> Result<Run> + Send + 'static,
        worktree: bool,
        task: Option<String>,
        rerun_of: Option<String>,
    ) -> Result<()> {
        ensure!(
            self.starting.is_none(),
            "A run is still starting; wait for it to begin"
        );
        let (placement, holder) = self.placement(worktree)?;
        self.message = match &rerun_of {
            Some(id) => format!("Starting a rerun of {id}…"),
            None => "Starting run…".into(),
        };
        self.starting = Some(Job::spawn(self.generation, move || {
            create(placement)
                .map(|run| Started {
                    run,
                    holder,
                    task,
                    rerun_of,
                })
                .map_err(|e| format!("{e:#}"))
        }));
        Ok(())
    }
    fn focused_stopped(&mut self, result: std::result::Result<Action, String>) {
        // Away from the Monitor, a terminal never takes over the screen.
        if self.page != 1 {
            if let Some(run) = &self.focused.run {
                self.message = notice(run, &result);
                if result.is_err() || result == Ok(Action::Stopped) {
                    self.alert = self.message.clone();
                }
            }
            return;
        }
        match result {
            Ok(Action::Interactive) => self.launch_interactive = !self.quit_after_stop,
            Ok(Action::Confirm) => {
                self.message =
                    "Inspect the handoff: [h]. Advance: [a]. Reopen session: [t]. Restart: [i]. Save and exit: [q]."
                        .into()
            }
            Ok(Action::Complete) => {
                self.message = "Workflow complete. Artifacts and handoffs are saved.".into()
            }
            Ok(Action::Stopped) => {
                self.message = self
                    .focused
                    .run
                    .as_ref()
                    .and_then(|r| r.error.clone())
                    .unwrap_or_else(|| "Run stopped".into())
            }
            Err(error) => {
                self.alert = error.clone();
                self.message = error;
            }
        }
    }
    fn show_catalog(&mut self, catalog: Catalog) {
        let items = catalog
            .models
            .into_iter()
            .map(|model| Choice {
                label: format!(
                    "{}{}{}  ·  {}{}",
                    if model.provider.is_empty() {
                        ""
                    } else {
                        &model.provider
                    },
                    if model.provider.is_empty() { "" } else { "/" },
                    model.label,
                    model.availability,
                    if model.efforts.is_empty() {
                        String::new()
                    } else {
                        format!("  ·  {}", model.efforts.join("/"))
                    }
                ),
                value: PickValue::Model(model),
            })
            .collect();
        self.modal = Some(Modal::Picker {
            title: format!("{} selections · type to filter", self.catalog_harness),
            items,
            selected: 0,
            filter: textarea(""),
            note: catalog.note,
        });
    }
    /// Hands a cached catalog to the open or parked editors for `harness`.
    fn push_catalog(&mut self, harness: &str) {
        let Some(catalog) = self.catalogs.get(harness).cloned() else {
            return;
        };
        match &mut self.modal {
            Some(Modal::Harness(editor)) if editor.harness() == harness => {
                editor.set_catalog(catalog.clone());
            }
            Some(Modal::Stage { editor, .. }) if editor.harness() == harness => {
                editor.set_catalog(catalog.clone());
            }
            _ => {}
        }
        if let Some(editor) = &mut self.model_editor {
            if editor.harness() == harness {
                editor.set_catalog(catalog.clone());
            }
        }
        if let Some((editor, _)) = &mut self.stage_editor {
            if editor.harness() == harness {
                editor.set_catalog(catalog);
            }
        }
    }
    /// Makes `harness`'s catalog available to editors: from the cache now, or
    /// read in the background without opening a picker.
    pub(super) fn request_catalog(&mut self, harness: String) {
        if self.catalogs.contains_key(&harness) {
            self.push_catalog(&harness);
            return;
        }
        if self.prefetch_queue.contains(&harness)
            || self.prefetch.as_ref().is_some_and(|(h, _)| *h == harness)
        {
            return;
        }
        if self.prefetch.is_some() {
            self.prefetch_queue.push_back(harness);
            return;
        }
        let settings = self.settings.clone();
        let project = self.project.clone();
        let name = harness.clone();
        self.prefetch = Some((
            harness,
            Job::spawn(self.generation, move || {
                catalog::discover(&name, &settings, &project).map_err(|e| format!("{e:#}"))
            }),
        ));
    }
    pub(super) fn model_picker(&mut self) {
        self.model_picker_for(self.stage().harness.clone());
    }
    pub(super) fn model_picker_for(&mut self, harness: String) {
        self.catalog_harness = harness.clone();
        if let Some(catalog) = self.catalogs.get(&harness).cloned() {
            self.show_catalog(catalog);
            return;
        }
        let settings = self.settings.clone();
        let project = self.project.clone();
        let (sender, receiver) = mpsc::channel();
        self.catalog = Some(receiver);
        self.modal = Some(Modal::Loading(format!(
            "Reading {harness} model selections…"
        )));
        thread::spawn(move || {
            let _ = sender.send(
                catalog::discover(&harness, &settings, &project).map_err(|e| format!("{e:#}")),
            );
        });
    }
    /// Reads `path` as a project now; `begin_switch` does it in the background.
    #[cfg(test)]
    pub(super) fn switch_project(&mut self, path: PathBuf) -> Result<()> {
        let opened = read_project(path).map_err(anyhow::Error::msg)?;
        self.open_project(opened)
    }
    /// Opens `path` in the background, returning to `picker` if it fails.
    pub(super) fn begin_switch(&mut self, path: PathBuf, picker: Box<PathPicker>) {
        self.project_job = Some((
            Job::spawn(self.generation, move || read_project(path)),
            picker,
        ));
        self.modal = Some(Modal::Loading("Opening project…".into()));
    }
    /// Stops opening a project; the browser comes back.
    pub(super) fn cancel_switch(&mut self) -> bool {
        match self.project_job.take() {
            Some((_, picker)) => {
                self.modal = Some(Modal::Repositories(picker));
                true
            }
            None => false,
        }
    }
    /// Shows `opened`. Runs of this session keep going in their own projects;
    /// everything read for the previous project is dropped.
    fn open_project(&mut self, opened: Opened) -> Result<()> {
        if self.dirty {
            self.save(false)?;
        }
        let Opened {
            project,
            settings,
            config_path,
            repo,
        } = opened;
        self.generation += 1;
        self.repo = repo;
        self.repo_job = None;
        self.repo_checked = Some(Instant::now());
        self.project = project;
        self.settings = settings;
        self.config_path = config_path;
        self.workflow = 0;
        self.selected = 0;
        self.task.clear();
        self.history.clear();
        self.history_selected = 0;
        self.runs_job = None;
        self.runs_stale = true;
        self.runs_loaded = None;
        self.harness_selected = 0;
        self.harness_checks.clear();
        self.catalog = None;
        self.catalogs.clear();
        self.prefetch = None;
        self.prefetch_queue.clear();
        self.undo.clear();
        self.page = 0;
        self.message = format!("Project: {}", self.project.display());
        if let Err(error) = storage::remember_project(&self.project) {
            self.message
                .push_str(&format!(" · Could not save recent projects: {error:#}"));
        }
        Ok(())
    }
    pub(super) fn key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.kind == KeyEventKind::Release {
            return Ok(false);
        }
        if key.code == KeyCode::F(8) && key.modifiers.is_empty() {
            return self.perform_global(ActionId::ToggleVim);
        }
        if self.modal.is_some() {
            return self.modal_key(key);
        }
        if self.runs_filtering() {
            return self.runs_filter_key(key);
        }
        if let Some(id) = actions::resolve(self.scope(), key) {
            return self.perform(id);
        }
        Ok(false)
    }
    /// Run an action exactly as its key does; true means exit.
    pub(super) fn perform(&mut self, id: ActionId) -> Result<bool> {
        let entry = actions::entry(id).with_context(|| format!("{id:?} has no action entry"))?;
        // A command menu can outlive its context, e.g. when the run stops.
        if !entry.scope.covers(self.scope()) {
            return Ok(false);
        }
        match entry.scope {
            Scope::Global => return self.perform_global(id),
            Scope::Workflow => self.perform_workflow(id)?,
            Scope::Monitor | Scope::MonitorRunning | Scope::MonitorIdle => {
                self.perform_monitor(id)?
            }
            Scope::Runs => self.perform_runs(id)?,
            Scope::Harnesses => self.perform_harnesses(id)?,
        }
        Ok(false)
    }
    fn perform_global(&mut self, id: ActionId) -> Result<bool> {
        match id {
            ActionId::Interrupt => {
                if !self.any_active() {
                    return Ok(self.ask_to_save().is_none());
                }
                self.cancel_all();
                self.message = "Stopping active stages and saving their artifacts…".into();
            }
            ActionId::Quit => {
                if self.ask_to_save().is_some() {
                    return Ok(false);
                }
                return Ok(self.quit());
            }
            ActionId::Themes => self.choose_theme(),
            ActionId::ToggleVim => {
                let enabled = !self.vim_motions;
                theme::save(&self.theme_path, self.theme, enabled)?;
                self.vim_motions = enabled;
                self.runs.set_vim(enabled);
                if let Some(field) = self.modal.as_mut().and_then(Modal::active_input) {
                    field.set_vim(enabled);
                }
                self.message = if enabled {
                    "Vim motions enabled for all projects. Inputs start in Insert mode; Esc enters Normal mode. F8 disables."
                } else {
                    "Vim motions disabled for all projects. F8 enables."
                }.into();
            }
            ActionId::Help => {
                self.modal = Some(Modal::Text {
                    title: "Keyboard guide".into(),
                    body: actions::guide(),
                    scroll: 0,
                    guide: true,
                })
            }
            ActionId::Commands => self.command_menu(),
            ActionId::NextView => self.show((self.page + 1) % TABS.len())?,
            ActionId::ShowWorkflow => self.show(0)?,
            ActionId::ShowMonitor => self.show(1)?,
            ActionId::ShowRuns => self.show(2)?,
            ActionId::ShowHarnesses => self.show(3)?,
            other => bail!("{other:?} is not a global action"),
        }
        Ok(false)
    }
    /// Asks whether to save unsaved workflow edits before exiting; None when
    /// there are none.
    fn ask_to_save(&mut self) -> Option<()> {
        self.dirty.then(|| {
            self.modal = Some(Modal::Ask(Box::new(Ask::SaveAndExit)));
        })
    }
    /// Exits now, or once active runs have stopped; true means exit now.
    pub(super) fn quit(&mut self) -> bool {
        if !self.any_active() {
            return true;
        }
        self.cancel_all();
        self.quit_after_stop = true;
        self.message = "Stopping active runs and saving their artifacts before exit…".into();
        false
    }
    fn show(&mut self, page: usize) -> Result<()> {
        self.page = page;
        match page {
            // Read when shown, not while Tab passes through.
            2 => self.runs_stale = true,
            3 => self.harness_checks.start(&self.settings, &self.project),
            _ => {}
        }
        Ok(())
    }
    /// Whether something on screen animates, so frames come faster.
    pub(super) fn animating(&self) -> bool {
        matches!(self.modal, Some(Modal::Loading(_)))
            || self.starting.is_some()
            || matches!(&self.modal, Some(Modal::Import(importer)) if importer.busy())
    }
    /// Whether the focused interactive stage may take the terminal now. An
    /// open dialog keeps it waiting, and the status line says how to open it.
    pub(super) fn take_launch(&mut self) -> bool {
        if !std::mem::take(&mut self.launch_interactive) {
            return false;
        }
        if self.modal.is_some() {
            self.message = "Interactive stage ready — press [t] on the Monitor to open it.".into();
            return false;
        }
        true
    }
    /// Saves the workflows so [u] can bring them back.
    pub(super) fn remember(&mut self, what: String) {
        if self.undo.len() == UNDO_DEPTH {
            self.undo.remove(0);
        }
        self.undo.push(Snapshot {
            workflows: self.settings.workflows.clone(),
            workflow: self.workflow,
            selected: self.selected,
            what,
        });
    }
    pub(super) fn undo(&mut self) {
        let Some(snapshot) = self.undo.pop() else {
            self.message = "Nothing to undo.".into();
            return;
        };
        self.settings.workflows = snapshot.workflows;
        self.workflow = snapshot.workflow.min(self.settings.workflows.len() - 1);
        let stages = self.settings.workflows[self.workflow].stages.len();
        self.selected = snapshot.selected.min(stages.saturating_sub(1));
        self.dirty = true;
        self.message = format!("Restored {}.", snapshot.what);
    }
    fn command_menu(&mut self) {
        let items = actions::available(self.scope())
            .map(|entry| Choice {
                label: format!(
                    "{:<15}{:<14}{}",
                    theme::key_label(&entry.keys()),
                    entry.label,
                    entry.help
                ),
                value: PickValue::Action(entry.id),
            })
            .collect();
        self.modal = Some(Modal::Picker {
            title: format!("Commands · {}", TABS[self.page]),
            items,
            selected: 0,
            filter: textarea(""),
            note: "Actions available here. Type to filter by key, name, or description.".into(),
        });
    }
    /// The page's scope; Monitor depends on whether a run is active.
    fn page_scope(&self) -> Scope {
        match self.page {
            0 => Scope::Workflow,
            1 if self.focused.worker.is_some() => Scope::MonitorRunning,
            1 => Scope::MonitorIdle,
            2 => Scope::Runs,
            _ => Scope::Harnesses,
        }
    }
    /// Where actions apply. Runs snapshot their workflow, so editing and
    /// starting more runs continue while others are active.
    fn scope(&self) -> Scope {
        self.page_scope()
    }
}

const TABS: [&str; 4] = ["Workflow", "Monitor", "Runs", "Harnesses"];

/// The settings and Git facts of the project at `path`; slow on network drives.
fn read_project(path: PathBuf) -> std::result::Result<Opened, String> {
    let read = || -> Result<Opened> {
        let project = catalog::check_project(&path)?;
        let (settings, config_path) = config::load(&project, None)?;
        let repo = storage::repo_info(&project);
        Ok(Opened {
            project,
            settings,
            config_path,
            repo,
        })
    };
    read().map_err(|e| format!("{e:#}"))
}

pub(super) fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let palette = app.palette();
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.bg).fg(palette.fg)),
        area,
    );
    if area.width < 65 || area.height < 20 {
        let top = area.height.saturating_sub(3) / 2;
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    "MODEL ROUTER",
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Line::styled(
                    "Resize to at least 65 × 20.",
                    Style::default().fg(palette.fg),
                ),
                Line::styled("Press [q] to exit.", Style::default().fg(palette.muted)),
            ])
            .centered()
            .wrap(Wrap { trim: false }),
            Rect::new(area.x, area.y + top, area.width, area.height - top),
        );
        return;
    }
    let hints = actions::footer(app.page_scope());
    let keys: Vec<_> = hints
        .iter()
        .map(|(key, label)| (key.as_str(), *label))
        .collect();
    let mut footer = palette.hints(&keys);
    footer.spans.insert(0, Span::raw(" "));
    let alert = !app.message.is_empty() && app.message == app.alert;
    let status = Line::from(vec![
        Span::styled(
            if alert { "✗ " } else { "› " },
            Style::default().fg(if alert { palette.red } else { palette.accent }),
        ),
        Span::styled(
            storage::clean_terminal(&app.message),
            Style::default().fg(if alert { palette.red } else { palette.fg }),
        ),
    ]);
    let body_width = area.width.saturating_sub(2);
    let mut headers = header_lines(app, palette, area.width);
    let repo = repository_line(app, palette);
    // Repository details move to another row when the header is full.
    let last_width = headers.last().map_or(0, Line::width);
    let beside = last_width + repo.width() + 4 <= area.width as usize;
    let header_height = headers.len() as u16 + u16::from(!beside);
    let chunks = Layout::vertical([
        Constraint::Length(header_height),
        Constraint::Length(1), // Vim toggle below the header, aligned right
        Constraint::Min(10),
        Constraint::Length(rows_for(&status, body_width, 2)),
        Constraint::Length(rows_for(&footer, area.width, 2)),
    ])
    .split(area);
    let header = chunks[0];
    let row = |y: u16| Rect::new(header.x, header.y + y, header.width, 1);
    let (repo_area, used) = if beside {
        (row(headers.len() as u16 - 1), last_width + 2)
    } else {
        (row(headers.len() as u16), 0)
    };
    for (index, line) in headers.drain(..).enumerate() {
        frame.render_widget(
            Paragraph::new(line).style(Style::default().bg(palette.bg)),
            row(index as u16),
        );
    }
    draw_repository(frame, app, repo, repo_area, used, palette);
    frame.render_widget(
        Paragraph::new(vim_toggle(app.vim_motions, palette)).right_aligned(),
        chunks[1].inner(Margin::new(1, 0)),
    );
    let body = chunks[2].inner(Margin::new(1, 0));
    match app.page {
        0 => draw_workflow(frame, app, body, palette),
        1 => draw_run(frame, app, body, palette),
        2 => draw_history(frame, app, body, palette),
        _ => draw_harnesses(frame, app, body, palette),
    }
    if !app.message.is_empty() {
        frame.render_widget(
            Paragraph::new(status).wrap(Wrap { trim: false }),
            chunks[3].inner(Margin::new(1, 0)),
        );
    }
    frame.render_widget(
        Paragraph::new(footer)
            .style(Style::default().bg(palette.bg))
            .wrap(Wrap { trim: false }),
        chunks[4],
    );
    let ask = match &app.modal {
        Some(Modal::Ask(ask)) => Some(ask.text(app)),
        _ => None,
    };
    if let Some(modal) = &mut app.modal {
        if let Some(field) = modal.active_input() {
            field.set_vim(app.vim_motions);
        }
        // The theme picker previews live, so keep the page behind it legible.
        let previewing_theme = matches!(
            modal,
            Modal::Picker { items, .. }
                if items.first().is_some_and(|c| matches!(c.value, PickValue::Theme(_)))
        );
        if !previewing_theme {
            dim(frame, palette);
        }
        draw_modal(frame, modal, ask, palette);
    }
}

fn header_lines(app: &App, palette: Palette, width: u16) -> Vec<Line<'static>> {
    let mut spans = vec![
        Span::styled(" ◆ ", Style::default().fg(palette.accent)),
        Span::styled(
            "MODEL ROUTER",
            Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];
    // The Monitor tab counts runs still in progress, so they stay visible
    // while the next workflow is being set up.
    let live = app.slots().filter(|(slot, _)| slot.in_progress()).count();
    let mut tabs = Vec::new();
    for (index, name) in TABS.iter().enumerate() {
        let count = if index == 1 && live > 0 {
            format!(" ●{live}")
        } else {
            String::new()
        };
        tabs.push(Span::styled(
            format!(" {} {name}{count} ", index + 1),
            if index == app.page {
                Style::default()
                    .fg(palette.on_accent())
                    .bg(palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.muted)
            },
        ));
    }
    let tabs = Line::from(tabs);
    if spans.iter().map(Span::width).sum::<usize>() + tabs.width() <= usize::from(width) {
        spans.extend(tabs.spans);
        vec![Line::from(spans)]
    } else {
        vec![Line::from(spans), tabs]
    }
}

fn vim_toggle(enabled: bool, palette: Palette) -> Line<'static> {
    Line::from(vec![
        Span::styled("[F8] ", Style::default().fg(palette.muted)),
        Span::styled(
            if enabled { "VIM ON" } else { "Vim OFF" },
            Style::default()
                .fg(if enabled {
                    palette.accent
                } else {
                    palette.muted
                })
                .add_modifier(if enabled {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
    ])
}

/// Repository name and branch, or the project directory when not a Git repository.
fn repository_line(app: &App, palette: Palette) -> Line<'static> {
    let spans = match &app.repo {
        Some(repo) => {
            let mut spans = vec![
                Span::styled(" ⎇ ", Style::default().fg(palette.accent)),
                Span::styled(
                    repo.name.clone(),
                    Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ", Style::default()),
                Span::styled(
                    if repo.detached {
                        format!("@ {}", repo.head)
                    } else {
                        repo.head.clone()
                    },
                    Style::default().fg(palette.accent),
                ),
            ];
            if repo.changes > 0 {
                spans.push(Span::styled(
                    format!("  ● {} changed", repo.changes),
                    Style::default().fg(palette.warn),
                ));
            }
            spans
        }
        None => vec![
            Span::styled(" ▫ ", Style::default().fg(palette.muted)),
            Span::styled(
                app.project
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| app.project.display().to_string()),
                Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  not a Git repository", Style::default().fg(palette.muted)),
        ],
    };
    Line::from(spans)
}

/// Right-align the repository line, adding the path when there is room.
/// `used` is how much of the row the left-hand content already takes.
fn draw_repository(
    frame: &mut Frame<'_>,
    app: &App,
    mut line: Line<'static>,
    area: Rect,
    used: usize,
    palette: Palette,
) {
    let room = (area.width as usize).saturating_sub(used + line.width() + 4);
    if room >= 12 {
        line.spans.push(Span::styled(
            format!("  {}", short_path(&app.project, room)),
            Style::default().fg(palette.muted),
        ));
    }
    line.spans.push(Span::raw(" "));
    let width = (line.width() as u16).min(area.width.saturating_sub(used as u16));
    let x = area.x + area.width - width;
    frame.render_widget(
        Paragraph::new(line).right_aligned(),
        Rect::new(x, area.y, width, 1),
    );
}
