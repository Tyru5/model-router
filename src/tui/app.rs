use super::{
    actions::{self, ActionId, Scope},
    completion,
    harness_editor::HarnessEditor,
    harnesses::{HarnessChecks, draw_harnesses},
    history::{RunsView, draw_history},
    modal::{Choice, InputTarget, Modal, PickValue, draw_modal, matching_choices},
    monitor::draw_run,
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
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
};

enum WorkerMessage {
    Event(RunEvent),
    Done(Box<Run>, std::result::Result<Action, String>),
}
pub(super) struct Worker {
    receiver: Receiver<WorkerMessage>,
    thread: Option<JoinHandle<()>>,
    terminal: Option<process::InteractiveSession>,
}
#[cfg(test)]
impl Worker {
    /// A worker without a thread, so tests can put the app in its running state.
    pub(super) fn stub() -> Self {
        Self {
            receiver: mpsc::sync_channel(1).1,
            thread: None,
            terminal: None,
        }
    }
}

/// A run started or opened in this session, with its Monitor view state.
pub(super) struct RunSlot {
    pub(super) run: Option<Run>,
    pub(super) worker: Option<Worker>,
    pub(super) cancel: Arc<AtomicBool>,
    pub(super) logs: VecDeque<String>,
    pub(super) log_scroll: usize,
    /// A finished run shows results unless the output log was chosen.
    pub(super) show_output: bool,
    pub(super) results_scroll: u16,
    pub(super) changes: Option<completion::Loaded>,
    pub(super) changes_job: Option<(String, Receiver<completion::Loaded>)>,
    /// The run checkpoint whose waiver the status line last announced.
    pub(super) waiver_shown: String,
    /// Reopen the stage's session interactively once its worker stops.
    pub(super) takeover: bool,
}
impl Default for RunSlot {
    fn default() -> Self {
        Self {
            run: None,
            worker: None,
            cancel: Arc::new(AtomicBool::new(false)),
            logs: VecDeque::new(),
            log_scroll: 0,
            show_output: false,
            results_scroll: 0,
            changes: None,
            changes_job: None,
            waiver_shown: String::new(),
            takeover: false,
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
    /// Applies worker messages; returns the driver's result once it stops.
    fn poll(&mut self) -> Option<std::result::Result<Action, String>> {
        let messages: Vec<_> = self
            .worker
            .as_ref()
            .map(|w| w.receiver.try_iter().collect())
            .unwrap_or_default();
        let mut stopped = None;
        for message in messages {
            match message {
                WorkerMessage::Event(RunEvent::State(run)) => self.run = Some(*run),
                WorkerMessage::Event(RunEvent::Output(text)) => {
                    for line in text.lines() {
                        self.logs.push_back(storage::truncate(line, 1500));
                    }
                    while self.logs.len() > 400 {
                        self.logs.pop_front();
                    }
                }
                WorkerMessage::Done(run, result) => {
                    self.run = Some(*run);
                    if let Some(mut worker) = self.worker.take() {
                        if let Some(handle) = worker.thread.take() {
                            let _ = handle.join();
                        }
                    }
                    stopped = Some(self.settle(result));
                }
            }
        }
        if self.worker.is_none() {
            self.cancel.store(false, Ordering::Relaxed);
        }
        stopped
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
            drop(worker.receiver);
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
    prefetch: Option<(String, Receiver<std::result::Result<Catalog, String>>)>,
    prefetch_queue: VecDeque<String>,
    /// Git facts for the project, refreshed on start and project switch.
    pub(super) repo: Option<storage::RepoInfo>,
    pub(super) quit_after_stop: bool,
}
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
            quit_after_stop: false,
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
            body,
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
                (lock.is_file() && storage::lock(&lock).is_err())
                    .then(|| "another router process".to_owned())
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
        let (sender, receiver) = mpsc::sync_channel(128);
        let handle = thread::spawn(move || {
            let action = engine::drive(&mut run, &cancel, |event| {
                let _ = sender.try_send(WorkerMessage::Event(event));
            })
            .map_err(|e| format!("{e:#}"));
            let _ = sender.send(WorkerMessage::Done(Box::new(run), action));
        });
        self.focused.worker = Some(Worker {
            receiver,
            thread: Some(handle),
            terminal: None,
        });
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
        let (sender, receiver) = mpsc::sync_channel(128);
        let handle = thread::spawn(move || {
            let result = engine::run_interactive(&mut run, &cancel, &terminal, |event| {
                let _ = sender.try_send(WorkerMessage::Event(event));
            })
            .map(|()| {
                if run.status == Status::AwaitingConfirmation {
                    Action::Confirm
                } else {
                    Action::Stopped
                }
            })
            .map_err(|error| format!("{error:#}"));
            let _ = sender.send(WorkerMessage::Done(Box::new(run), result));
        });
        self.focused.worker = Some(Worker {
            receiver,
            thread: Some(handle),
            terminal: Some(session.clone()),
        });
        Ok(session)
    }
    pub(super) fn poll(&mut self) -> Result<bool> {
        if let Some(Modal::Import(importer)) = &mut self.modal {
            importer.poll();
        }
        if let Some(result) = self.focused.poll() {
            self.focused_stopped(result);
        }
        for index in 0..self.parked.len() {
            if let Some(result) = self.parked[index].poll() {
                if let Some(run) = &self.parked[index].run {
                    self.message = notice(run, &result);
                    if result.is_err() || result == Ok(Action::Stopped) {
                        self.alert = self.message.clone();
                    }
                }
            }
        }
        if let Some((harness, receiver)) = &self.prefetch {
            if let Ok(result) = receiver.try_recv() {
                let harness = harness.clone();
                self.prefetch = None;
                if let Ok(catalog) = result {
                    self.catalogs.insert(harness.clone(), catalog);
                    self.push_catalog(&harness);
                }
                if let Some(next) = self.prefetch_queue.pop_front() {
                    self.request_catalog(next);
                }
            }
        }
        if let Some(receiver) = &self.catalog {
            if let Ok(result) = receiver.try_recv() {
                self.catalog = None;
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
                self.harness_selected = self
                    .harness_selected
                    .min(self.harness_checks.reports.len().saturating_sub(1))
            }
            Some(false) => {
                self.message = "Harness check failed. Press [r] to retry.".into();
                self.alert = self.message.clone();
            }
            None => {}
        }
        self.poll_completion();
        Ok(self.quit_after_stop && !self.any_active())
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
        let (sender, receiver) = mpsc::channel();
        self.prefetch = Some((harness.clone(), receiver));
        thread::spawn(move || {
            let _ = sender.send(
                catalog::discover(&harness, &settings, &project).map_err(|e| format!("{e:#}")),
            );
        });
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
    /// Runs of this session keep going in their own projects.
    pub(super) fn switch_project(&mut self, path: PathBuf) -> Result<()> {
        let project = catalog::check_project(&path)?;
        if self.dirty {
            self.save(false)?;
        }
        let (settings, config_path) = config::load(&project, None)?;
        self.repo = storage::repo_info(&project);
        self.project = project;
        self.settings = settings;
        self.config_path = config_path;
        self.workflow = 0;
        self.selected = 0;
        self.task.clear();
        self.history.clear();
        self.history_selected = 0;
        self.harness_selected = 0;
        self.harness_checks.clear();
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
                    return Ok(true);
                }
                self.cancel_all();
                self.message = "Stopping active stages and saving their artifacts…".into();
            }
            ActionId::Quit => {
                if self.dirty {
                    self.save(false)?;
                }
                if !self.any_active() {
                    return Ok(true);
                }
                self.cancel_all();
                self.quit_after_stop = true;
                self.message = "Stopping active runs and saving before exit…".into();
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
    fn show(&mut self, page: usize) -> Result<()> {
        self.page = page;
        match page {
            2 => self.refresh_runs()?,
            3 => self.harness_checks.start(&self.settings, &self.project),
            _ => {}
        }
        Ok(())
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
        draw_modal(frame, modal, palette);
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
                    .fg(palette.bg)
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
