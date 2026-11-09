use super::{
    actions::{self, ActionId, Scope},
    completion,
    editor::Editor,
    harnesses::{HarnessesPage, draw_harnesses},
    history::{RunsPage, draw_runs},
    modal::{Ask, Choice, InputTarget, Loading, Modal, PickValue, draw_modal},
    models::Catalogs,
    run_slot::{RunSlot, Worker},
    session::{Session, draw_session_runs, notice_text},
    tasks::{Msg, Tasks},
    theme::{self, Palette, Theme},
    ui::Ui,
    widgets::{dim, rows_for, short_path, textarea},
    workflow_page::{draw_workflow, editing_task},
};
use crate::{
    catalog,
    config::{self, Settings},
    engine::{self, Action, Placement, Run, Status},
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
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub(super) struct App {
    pub(super) project: PathBuf,
    /// The Workflow page's workflow, task, and undo stack.
    pub(super) editor: Editor,
    /// This session's runs; the Runs page shows the focused one.
    pub(super) session: Session,
    pub(super) runs: RunsPage,
    pub(super) harnesses: HarnessesPage,
    /// Preferences, status line, dialog stack, and redraw flag.
    pub(super) ui: Ui,
    /// Ctrl+C from the terminal, shared with the signal handler.
    pub(super) signal: Arc<AtomicBool>,
    /// Saved-run stats behind the stage hint and the Stats dialog.
    pub(super) route_stats: super::stats_page::StatsCache,
    /// Harness model catalogs, for pickers and editors.
    pub(super) catalogs: Catalogs,
    /// Git facts for the header.
    pub(super) repo: Repo,
    /// A run is being created (worktree, snapshot) before it starts.
    pub(super) starting: bool,
    /// Background work and the channel its results arrive on.
    pub(super) tasks: Tasks,
}

/// Git facts for the project, refreshed in the background.
pub(super) struct Repo {
    pub(super) info: Option<storage::RepoInfo>,
    /// When the facts were last read; None reads them on the next poll.
    pub(super) checked: Option<Instant>,
    pub(super) reading: bool,
}
impl Repo {
    fn new(info: Option<storage::RepoInfo>) -> Self {
        Self {
            info,
            checked: Some(Instant::now()),
            reading: false,
        }
    }
}

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
#[cfg(test)]
impl App {
    /// The focused run's slot, which the test has set up.
    pub(super) fn slot(&self) -> &RunSlot {
        self.session.focused().expect("a focused run")
    }
    pub(super) fn slot_mut(&mut self) -> &mut RunSlot {
        self.session.focused_mut().expect("a focused run")
    }
}
/// How often the header re-reads Git status, and Runs re-reads active runs.
const REPO_EVERY: Duration = Duration::from_secs(5);
const RUNS_EVERY: Duration = Duration::from_secs(3);
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
            editor: Editor::new(settings, config_path),
            session: Session::default(),
            runs: RunsPage::default(),
            harnesses: HarnessesPage::default(),
            ui: Ui::default(),
            signal: cancel,
            route_stats: Default::default(),
            catalogs: Catalogs::default(),
            repo: Repo::new(repo),
            starting: false,
            tasks: Tasks::default(),
        }
    }
    pub(super) fn choose_theme(&mut self) {
        self.ui.open(Modal::Picker {
            title: "Choose theme · live preview".into(),
            items: Theme::ALL
                .into_iter()
                .map(|theme| Choice {
                    label: format!(
                        "{} · {}{}",
                        theme.name(),
                        theme.appearance(),
                        if theme == self.ui.theme {
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
                .position(|theme| *theme == self.ui.theme)
                .unwrap_or(0),
            filter: textarea(""),
            note: format!(
                "Preview as you move. [Enter] saves for every project. [Esc] restores {}.",
                self.ui.theme.name()
            ),
        });
    }
    pub(super) fn input(&mut self, title: &str, target: InputTarget, value: &str) {
        self.ui.open(Modal::input(title, target, value));
    }
    pub(super) fn text(&mut self, title: &str, body: String) {
        self.ui.open(Modal::Text {
            title: title.into(),
            body: storage::clean_terminal(&body),
            scroll: 0,
            guide: false,
        });
    }
    pub(super) fn save(&mut self, global: bool) -> Result<()> {
        let path = self.editor.save(global)?;
        self.ui.message = format!("Saved {}", path.display());
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
            .session
            .runs
            .iter()
            .filter(|slot| slot.in_progress())
            .map(|slot| &slot.run)
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
            self.repo.info.is_some(),
            "{holder} is using this folder. It is not a Git repository, so a second run cannot get its own worktree; wait for the first to finish"
        );
        Ok((Placement::WorktreeFromHead, Some(holder)))
    }
    pub(super) fn start_driver(&mut self) -> Result<()> {
        let slot = self.session.focused_mut().context("No run selected")?;
        ensure!(slot.worker.is_none(), "A stage is still running");
        let mut run = slot.run.clone();
        let explicit = self.editor.settings.explicit_config.clone();
        slot.cancel.store(false, Ordering::Relaxed);
        let cancel = slot.cancel.clone();
        slot.worker = Some(Worker::spawn(
            None,
            |_| true,
            move |emit| {
                let action = engine::authorize_replay(&mut run, explicit.as_deref(), &mut *emit)
                    .and_then(|()| engine::drive(&mut run, &cancel, emit))
                    .map_err(|e| format!("{e:#}"));
                (run, action)
            },
        ));
        Ok(())
    }
    /// Start the stage once; subsequent attachments reuse this worker's PTY.
    pub(super) fn interactive_terminal(&mut self) -> Result<process::InteractiveSession> {
        let slot = self.session.focused_mut().context("No run selected")?;
        if let Some(session) = slot.terminal() {
            return Ok(session.clone());
        }
        ensure!(slot.worker.is_none(), "A stage is still running");
        let mut run = slot.run.clone();
        ensure!(
            run.status == Status::Interactive || engine::detached_terminal(&run).is_some(),
            "No interactive stage is ready"
        );
        let explicit = self.editor.settings.explicit_config.clone();
        let (cols, rows) = crossterm::terminal::size().unwrap_or((100, 30));
        let session = process::InteractiveSession::new(cols, rows);
        let terminal = session.clone();
        slot.cancel.store(false, Ordering::Relaxed);
        let cancel = slot.cancel.clone();
        // The user ends an interactive stage themselves, so only failures
        // notify; a harness left running in the background has not ended.
        let failures = |result: &std::result::Result<Action, String>| {
            !matches!(result, Ok(Action::Confirm | Action::Interactive))
        };
        slot.worker = Some(Worker::spawn(
            Some(session.clone()),
            failures,
            move |emit| {
                let result = engine::authorize_replay(&mut run, explicit.as_deref(), &mut *emit)
                    .and_then(|()| engine::run_interactive(&mut run, &cancel, &terminal, emit))
                    .map(|()| {
                        if terminal.released() {
                            Action::Interactive
                        } else if run.status == Status::AwaitingConfirmation {
                            Action::Confirm
                        } else {
                            Action::Stopped
                        }
                    })
                    .map_err(|error| format!("{error:#}"));
                (run, result)
            },
        ));
        Ok(session)
    }
    /// Reconnects the focused run to its interactive harness when that
    /// outlived an earlier mr, without taking the screen; [t] then attaches.
    pub(super) fn reattach_detached(&mut self) {
        let detached = self.session.focused().is_some_and(|slot| {
            slot.worker.is_none() && engine::detached_terminal(&slot.run).is_some()
        });
        if !detached {
            return;
        }
        self.ui.message = match self.interactive_terminal() {
            Ok(_) => "This interactive stage kept running after an earlier mr exited. [t] attaches; [C] stops it.".into(),
            Err(error) => format!("{error:#}"),
        };
    }
    /// Adopts this project's runs whose interactive harness kept running
    /// after the mr that started it exited, and reconnects to them.
    pub(super) fn adopt_detached(&mut self) {
        let Ok(entries) = crate::history::list(&self.project) else {
            return;
        };
        let mut found = 0;
        for entry in entries {
            let crate::history::Entry::Summary(summary) = entry else {
                continue;
            };
            if summary.status != Status::Running || self.session.focus_run(&summary.id) {
                continue;
            }
            let Ok(run) = engine::load(&self.project, &summary.id) else {
                continue;
            };
            if engine::detached_terminal(&run).is_none() {
                continue;
            }
            self.session.adopt(run);
            self.reattach_detached();
            found += 1;
        }
        if found > 1 {
            self.ui.message = format!(
                "{found} interactive stages kept running after an earlier mr exited. [L] lists them; [t] attaches."
            );
        }
        if found > 0 {
            self.show_run();
        }
    }
    /// Applies background results and run progress, and starts timed
    /// refreshes; true means exit now.
    pub(super) fn poll(&mut self) -> Result<bool> {
        while let Some((generation, msg)) = self.tasks.try_recv() {
            self.update(generation, msg);
        }
        // The focused run first, then the others in start order.
        let focus = self.session.focus;
        let others = (0..self.session.runs.len()).filter(|index| *index != focus);
        for index in std::iter::once(focus).chain(others) {
            let Some(slot) = self.session.runs.get_mut(index) else {
                continue;
            };
            let (changed, stopped) = slot.poll();
            self.ui.redraw |= changed;
            let Some(result) = stopped else { continue };
            self.run_stopped();
            if index == focus {
                self.focused_stopped(result);
            } else {
                let run = &self.session.runs[index].run;
                let alert = result.is_err() || result == Ok(Action::Stopped);
                self.push_notice(notice_text(run, &result), alert);
            }
        }
        self.refresh();
        self.poll_completion();
        Ok(self.session.quit_after_stop && !self.session.any_active())
    }
    /// A run stopped: Git status and the saved runs list may have changed.
    fn run_stopped(&mut self) {
        self.ui.redraw = true;
        self.repo.checked = None;
        self.runs.stale = true;
    }
    /// Starts reads that are due: saved runs on the Runs page, Git status,
    /// and stats for the Workflow page's hint.
    fn refresh(&mut self) {
        if self.ui.page == Page::Runs {
            let due = self
                .runs
                .loaded_at
                .is_none_or(|at| at.elapsed() >= RUNS_EVERY);
            if due && self.session.runs.iter().any(RunSlot::in_progress) {
                self.runs.stale = true;
            }
            if self.runs.stale && !self.runs.loading {
                self.load_runs();
            }
        }
        let due = self
            .repo
            .checked
            .is_none_or(|at| at.elapsed() >= REPO_EVERY);
        if due && !self.repo.reading {
            self.repo.reading = true;
            let project = self.project.clone();
            self.tasks.spawn(
                "Reading Git status stopped unexpectedly",
                move || Ok(storage::repo_info(&project)),
                Msg::Repo,
            );
        }
        if self.ui.page == Page::Workflow {
            self.refresh_stats();
        }
    }
    /// Reads saved runs in the background; the page shows them when they arrive.
    pub(super) fn load_runs(&mut self) {
        self.runs.stale = false;
        self.runs.loading = true;
        let project = self.project.clone();
        self.tasks.spawn(
            "Reading saved runs stopped unexpectedly; [r] retries.",
            move || crate::history::list(&project).map_err(|e| format!("{e:#}")),
            Msg::Runs,
        );
    }
    /// Adopts a created run and starts its driver.
    pub(super) fn begin(&mut self, started: Started) -> Result<()> {
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
        self.session.adopt(run);
        self.start_driver()?;
        self.runs.stale = true;
        if let Some(source) = rerun_of {
            self.ui.message = format!(
                "Rerunning {source} as {id}{}…",
                holder.map_or_else(String::new, |h| format!(
                    " in a worktree ({h} is using the checkout)"
                ))
            );
            self.show_run();
            return Ok(());
        }
        // The run has its own snapshot; the editor is free for the next one.
        if task.is_some_and(|task| task == self.editor.task) {
            self.editor.task.clear();
        }
        self.ui.message = match holder {
            Some(holder) => format!(
                "Started {id} in worktree {}: {holder} is using the checkout, whose uncommitted changes are not included. ",
                short_path(&workspace, 60)
            ),
            None => format!("Started {id}. "),
        };
        if interactive {
            self.show_run();
        } else {
            // Runs opens on the new run, not a saved one selected earlier.
            self.runs.on_saved = false;
            self.ui
                .message
                .push_str("Runs [2] follows it. Define the next task here.");
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
            !self.starting,
            "A run is still starting; wait for it to begin"
        );
        let (placement, holder) = self.placement(worktree)?;
        self.ui.message = match &rerun_of {
            Some(id) => format!("Starting a rerun of {id}…"),
            None => "Starting run…".into(),
        };
        self.starting = true;
        self.tasks.spawn(
            "Starting the run stopped unexpectedly; try again.",
            move || {
                create(placement)
                    .map(|run| {
                        Box::new(Started {
                            run,
                            holder,
                            task,
                            rerun_of,
                        })
                    })
                    .map_err(|e| format!("{e:#}"))
            },
            Msg::Started,
        );
        Ok(())
    }
    fn focused_stopped(&mut self, result: std::result::Result<Action, String>) {
        // Every stop is kept in the notice history; on the run, the
        // status line then says what to do next.
        if let Some(run) = self.session.run() {
            let alert = result.is_err() || result == Ok(Action::Stopped);
            self.push_notice(notice_text(run, &result), alert);
        }
        // Away from the run, a terminal never takes over the screen.
        if !self.monitoring() {
            return;
        }
        match result {
            Ok(Action::Interactive) => self.session.launch_interactive = !self.session.quit_after_stop,
            Ok(Action::Confirm) => {
                self.ui.message =
                    "Inspect the handoff: [h]. Advance: [a]. Reopen session: [t]. Restart: [i]. Save and exit: [q]."
                        .into()
            }
            Ok(Action::Complete) => {
                self.ui.message = "Workflow complete. Artifacts and handoffs are saved.".into()
            }
            Ok(Action::Stopped) => {
                self.ui.message = self
                    .session
                    .run()
                    .and_then(|r| r.error.clone())
                    .unwrap_or_else(|| "Run stopped".into())
            }
            Err(error) => {
                self.ui.alert = error.clone();
                self.ui.message = error;
            }
        }
    }
    /// Reads `path` as a project now; `begin_switch` does it in the background.
    #[cfg(test)]
    pub(super) fn switch_project(&mut self, path: PathBuf) -> Result<()> {
        let opened = read_project(path).map_err(anyhow::Error::msg)?;
        self.open_project(opened)
    }
    /// Opens `path` in the background, above the repository browser.
    pub(super) fn begin_switch(&mut self, path: PathBuf) {
        let loading = Loading::new("Opening project…");
        let ticket = loading.ticket;
        self.tasks.spawn(
            "Opening the project stopped unexpectedly",
            move || read_project(path),
            move |result| Msg::Opened { ticket, result },
        );
        self.ui.open(Modal::Loading(loading));
    }
    /// Shows the project read in the background and closes the browser, or
    /// returns to the browser, which shows why it could not open.
    pub(super) fn project_opened(&mut self, opened: Result<Opened>) {
        match opened.and_then(|opened| self.open_project(opened)) {
            Ok(()) => {
                if matches!(self.ui.dialog(), Some(Modal::Repositories(_))) {
                    self.ui.dialogs.pop();
                }
            }
            Err(error) => match self.ui.dialog_mut() {
                Some(Modal::Repositories(picker)) => picker.error = format!("{error:#}"),
                _ => self.ui.error(format!("{error:#}")),
            },
        }
    }
    /// Shows `opened`. Runs of this session keep going in their own projects;
    /// everything read for the previous project is dropped.
    fn open_project(&mut self, opened: Opened) -> Result<()> {
        if self.editor.dirty {
            self.save(false)?;
        }
        let Opened {
            project,
            settings,
            config_path,
            repo,
        } = opened;
        // What is still being read for the old project is dropped on arrival.
        self.tasks.next_generation();
        self.repo = Repo::new(repo);
        self.project = project;
        self.editor.settings = settings;
        self.editor.config_path = config_path;
        self.editor.workflow = 0;
        self.editor.selected = 0;
        self.editor.task.clear();
        self.runs.entries.clear();
        self.runs.selected = 0;
        self.runs.loading = false;
        self.runs.stale = true;
        self.runs.loaded_at = None;
        self.harnesses.selected = 0;
        self.harnesses.checks.clear();
        self.catalogs = Catalogs::default();
        self.editor.undo.clear();
        self.ui.page = Page::Workflow;
        self.ui.message = format!("Project: {}", self.project.display());
        if let Some(note) = super::trust_prompt::notice(&self.editor.settings) {
            self.ui.message.push_str(&format!(" · {note}"));
        }
        if let Err(error) = storage::remember_project(&self.project) {
            self.ui
                .message
                .push_str(&format!(" · Could not save recent projects: {error:#}"));
        }
        Ok(())
    }
    pub(super) fn key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.kind == KeyEventKind::Release {
            return Ok(false);
        }
        self.sync_runs();
        if key.code == KeyCode::F(8) && key.modifiers.is_empty() {
            return self.perform_global(ActionId::ToggleVim);
        }
        if !self.ui.dialogs.is_empty() {
            return self.modal_key(key);
        }
        if self.runs_filtering() {
            return self.runs_filter_key(key);
        }
        // A search belongs to the feed; the brief's keys, such as [n], are its own.
        if self.monitoring() && !self.results_shown() && self.output_search_key(key) {
            return Ok(false);
        }
        if let Some(id) = actions::resolve(self.scope(), key) {
            return self.perform(id);
        }
        Ok(false)
    }
    /// Run an action exactly as its key does; true means exit.
    pub(super) fn perform(&mut self, id: ActionId) -> Result<bool> {
        self.sync_runs();
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
        // Opening and closing runs moves them in and out of Saved.
        self.sync_runs();
        Ok(false)
    }
    fn perform_global(&mut self, id: ActionId) -> Result<bool> {
        match id {
            ActionId::Interrupt => {
                if !self.session.any_active() {
                    return Ok(self.ask_to_save().is_none());
                }
                self.session.cancel_all();
                self.ui.message = "Stopping active stages and saving their artifacts…".into();
            }
            ActionId::Quit => {
                if self.ask_to_save().is_some() {
                    return Ok(false);
                }
                return Ok(self.quit());
            }
            ActionId::Themes => self.choose_theme(),
            ActionId::ToggleVim => {
                let enabled = !self.ui.vim_motions;
                theme::save(
                    &self.ui.theme_path,
                    &theme::Preferences {
                        vim_motions: enabled,
                        ..self.ui.preferences()
                    },
                )?;
                self.ui.vim_motions = enabled;
                self.runs.set_vim(enabled);
                if let Some(field) = self.ui.dialog_mut().and_then(Modal::active_input) {
                    field.set_vim(enabled);
                }
                self.ui.message = if enabled {
                    "Vim motions enabled for all projects. Inputs start in Insert mode; Esc enters Normal mode. F8 disables."
                } else {
                    "Vim motions disabled for all projects. F8 enables."
                }.into();
            }
            ActionId::Help => self.ui.open(Modal::Text {
                title: "Keyboard guide".into(),
                body: actions::guide(),
                scroll: 0,
                guide: true,
            }),
            ActionId::Commands => self.command_menu(),
            ActionId::Menu => self.ui.open(Modal::Menu {
                scope: self.scope(),
                group: None,
            }),
            ActionId::SessionRuns => self.session_runs(),
            ActionId::NextNeedsInput => self.jump_to_input(),
            ActionId::Notices => self.show_notices(),
            ActionId::Notifications => self.notifications(),
            ActionId::Stats => self.show_stats(),
            ActionId::NextView => self.show(self.ui.page.next()),
            ActionId::ShowWorkflow => self.show(Page::Workflow),
            ActionId::ShowRuns => self.show(Page::Runs),
            ActionId::ShowHarnesses => self.show(Page::Harnesses),
            other => bail!("{other:?} is not a global action"),
        }
        Ok(false)
    }
    /// Asks whether to save unsaved workflow edits before exiting; None when
    /// there are none.
    fn ask_to_save(&mut self) -> Option<()> {
        self.editor.dirty.then(|| {
            self.ui.open(Modal::Ask(Box::new(Ask::SaveAndExit)));
        })
    }
    /// Exits now, or once active runs have stopped; true means exit now.
    /// Asks first whether running interactive harnesses keep running.
    pub(super) fn quit(&mut self) -> bool {
        if self.session.live_terminals() > 0 {
            self.ui.open(Modal::Ask(Box::new(Ask::LeaveRunning)));
            return false;
        }
        self.stop_and_quit()
    }
    /// Exits now, or once active runs have stopped; true means exit now.
    pub(super) fn stop_and_quit(&mut self) -> bool {
        if !self.session.any_active() {
            return true;
        }
        self.session.cancel_all();
        self.session.quit_after_stop = true;
        self.ui.message = "Stopping active runs and saving their artifacts before exit…".into();
        false
    }
    fn show(&mut self, page: Page) {
        self.ui.page = page;
        match page {
            // Read when shown, not while Tab passes through.
            Page::Runs => self.runs.stale = true,
            Page::Harnesses => {
                self.harnesses
                    .checks
                    .start(&self.editor.settings, &self.project, &self.tasks)
            }
            Page::Workflow => {}
        }
    }
    /// Whether something on screen animates, so frames come faster.
    pub(super) fn animating(&self) -> bool {
        matches!(self.ui.dialog(), Some(Modal::Loading(_)))
            || self.starting
            || matches!(self.ui.dialog(), Some(Modal::Import(importer)) if importer.busy())
    }
    /// Whether the focused interactive stage may take the terminal now. An
    /// open dialog keeps it waiting, and the status line says how to open it.
    pub(super) fn take_launch(&mut self) -> bool {
        if !std::mem::take(&mut self.session.launch_interactive) {
            return false;
        }
        if !self.ui.dialogs.is_empty() {
            self.ui.message = "Interactive stage ready — press [t] on Runs to open it.".into();
            return false;
        }
        true
    }
    fn command_menu(&mut self) {
        let items = actions::in_menu(self.scope())
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
        self.ui.open(Modal::Picker {
            title: format!("Commands · {}", self.ui.page.name()),
            items,
            selected: 0,
            filter: textarea(""),
            note: "Actions available here. Type to filter by key, name, or description.".into(),
        });
    }
    /// The page's scope; on Runs it depends on what the sidebar selects.
    fn page_scope(&self) -> Scope {
        match self.ui.page {
            Page::Workflow => Scope::Workflow,
            Page::Runs if self.on_saved() => Scope::Runs,
            Page::Runs if self.session.running() => Scope::MonitorRunning,
            Page::Runs => Scope::MonitorIdle,
            Page::Harnesses => Scope::Harnesses,
        }
    }
    /// Where actions apply. Runs snapshot their workflow, so editing and
    /// starting more runs continue while others are active.
    fn scope(&self) -> Scope {
        self.page_scope()
    }
}

/// The views, in tab order; [1]–[3] and Tab switch between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Page {
    Workflow,
    /// This session's runs and the saved ones, beside the selected run.
    Runs,
    Harnesses,
}
impl Page {
    pub(super) const ALL: [Page; 3] = [Self::Workflow, Self::Runs, Self::Harnesses];
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Workflow => "Workflow",
            Self::Runs => "Runs",
            Self::Harnesses => "Harnesses",
        }
    }
    /// The next tab, wrapping.
    fn next(self) -> Self {
        let index = Self::ALL.iter().position(|page| *page == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }
}

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
    app.sync_runs();
    let palette = app.ui.palette();
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
    // One footer row: the view's hints left, as many as fit before the menu
    // and guide keys on the right.
    // While the task is edited in place, keys type text: the footer names
    // the editor's keys instead of the page's.
    // So do a feed search and the Runs filter while they are typed.
    let searching = app.monitoring()
        && !app.results_shown()
        && app
            .session
            .focused()
            .and_then(|slot| slot.log_search.as_ref())
            .is_some_and(|search| search.typing);
    let typing = editing_task(app) || searching || app.runs_filtering();
    let right = if typing {
        Line::default()
    } else {
        palette.hints(&[("Space", "menu"), ("?", "keys")])
    };
    let room = usize::from(area.width).saturating_sub(right.width() + 4);
    let field = |keys: &[(&str, &'static str)]| -> Vec<(String, &'static str)> {
        keys.iter()
            .map(|(key, label)| ((*key).to_owned(), *label))
            .collect()
    };
    let hints = if editing_task(app) {
        field(&[("Ctrl+S", "done"), ("Esc", "cancel"), ("Enter", "new line")])
    } else if app.runs_filtering() {
        field(&[("Enter", "keep"), ("Esc", "clear"), ("↑↓", "runs")])
    } else if searching {
        field(&[("Enter", "find"), ("Esc", "cancel")])
    } else {
        // Leave out keys the engine refuses for the selected run: only a
        // handoff awaits acceptance; a finished, interrupted, or waiting run
        // does not continue (an interrupted one must be retried, a waiting
        // one accepted); a complete one has nothing to retry.
        let mut skip = vec![];
        if let Some(run) = app
            .session
            .run()
            .filter(|_| app.page_scope() == Scope::MonitorIdle)
        {
            if run.status != Status::AwaitingConfirmation {
                skip.push(ActionId::Advance);
            }
            if completion::finished(run)
                || matches!(run.status, Status::Running | Status::AwaitingConfirmation)
            {
                skip.push(ActionId::Continue);
            }
            if run.status == Status::Complete {
                skip.push(ActionId::Retry);
            }
        }
        actions::footer(app.page_scope(), &skip)
    };
    let mut keys: Vec<(&str, &str)> = vec![];
    for (key, label) in &hints {
        keys.push((key.as_str(), *label));
        if palette.hints(&keys).width() > room {
            keys.pop();
            break;
        }
    }
    let mut footer = palette.hints(&keys);
    footer.spans.insert(0, Span::raw(" "));
    let alert = !app.ui.message.is_empty() && app.ui.message == app.ui.alert;
    let status = Line::from(vec![
        Span::styled(
            if alert { "✗ " } else { "› " },
            Style::default().fg(if alert { palette.red } else { palette.accent }),
        ),
        Span::styled(
            storage::clean_terminal(&app.ui.message),
            Style::default().fg(if alert { palette.red } else { palette.fg }),
        ),
        Span::styled(
            match app.more_notices() {
                0 => String::new(),
                more => format!("  (+{more} more · [!] notices)"),
            },
            Style::default().fg(palette.muted),
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
        Constraint::Length(1),
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
        Paragraph::new(vim_toggle(app.ui.vim_motions, palette)).right_aligned(),
        chunks[1].inner(Margin::new(1, 0)),
    );
    let body = chunks[2].inner(Margin::new(1, 0));
    // The task editor opens in the Workflow page's task panel, not as a dialog.
    let mut task_area = None;
    match app.ui.page {
        Page::Workflow => task_area = Some(draw_workflow(frame, app, body, palette)),
        Page::Runs => draw_runs(frame, app, body, palette),
        Page::Harnesses => draw_harnesses(frame, app, body, palette),
    }
    let inline = task_area.filter(|_| editing_task(app));
    if !app.ui.message.is_empty() {
        frame.render_widget(
            Paragraph::new(status).wrap(Wrap { trim: false }),
            chunks[3].inner(Margin::new(1, 0)),
        );
    }
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().bg(palette.bg)),
        chunks[4],
    );
    frame.render_widget(
        Paragraph::new(right).right_aligned(),
        chunks[4].inner(Margin::new(1, 0)),
    );
    // Only the top dialog shows; those below come back as it closes.
    let ask = match app.ui.dialog() {
        Some(Modal::Ask(ask)) => Some(ask.text(app)),
        _ => None,
    };
    let vim = app.ui.vim_motions;
    if let Some(modal) = app.ui.dialog_mut() {
        if let Some(field) = modal.active_input() {
            field.set_vim(vim);
        }
        if let (Some(area), Modal::Input { text, .. }) = (inline, &mut *modal) {
            palette.style_textarea(text);
            super::widgets::wrapped_textarea(frame, text, area, Block::default());
            if let Some(hint) = text.mode_hint() {
                frame.render_widget(
                    Paragraph::new(format!(" {hint} "))
                        .style(Style::default().fg(palette.accent).bg(palette.panel))
                        .centered(),
                    Rect::new(area.x, area.bottom(), area.width, 1),
                );
            }
            return;
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
        if let Modal::SessionRuns { selected } = modal {
            // This dialog reads the session's runs, so it draws from the app.
            let selected = *selected;
            draw_session_runs(frame, app, selected, palette);
        } else {
            draw_modal(frame, modal, ask, palette);
        }
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
    // The Runs tab counts runs still in progress, so they stay visible
    // while the next workflow is being set up.
    let live = app
        .session
        .runs
        .iter()
        .filter(|slot| slot.in_progress())
        .count();
    let mut tabs = Vec::new();
    for (index, page) in Page::ALL.into_iter().enumerate() {
        let name = page.name();
        let count = if page == Page::Runs && live > 0 {
            format!(" ●{live}")
        } else {
            String::new()
        };
        tabs.push(Span::styled(
            format!(" {} {name}{count} ", index + 1),
            if page == app.ui.page {
                Style::default()
                    .fg(palette.on_accent())
                    .bg(palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.muted)
            },
        ));
    }
    let waiting = app.session.needing_input();
    if waiting > 0 {
        tabs.push(Span::styled(
            format!("  ◆ {waiting} need input [N] "),
            Style::default()
                .fg(palette.warn)
                .add_modifier(Modifier::BOLD),
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
    let spans = match &app.repo.info {
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
