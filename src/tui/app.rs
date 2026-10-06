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
    engine::{self, Action, Event as RunEvent, Run},
    history::Entry,
    storage,
};
use anyhow::{Context, Result, bail, ensure};
use crossterm::event::{KeyEvent, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
};
use std::{
    collections::VecDeque,
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
}
#[cfg(test)]
impl Worker {
    /// A worker without a thread, so tests can put the app in its running state.
    pub(super) fn stub() -> Self {
        Self {
            receiver: mpsc::sync_channel(1).1,
            thread: None,
        }
    }
}
pub(super) struct App {
    pub(super) project: PathBuf,
    pub(super) config_path: PathBuf,
    pub(super) theme: Theme,
    pub(super) theme_path: PathBuf,
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
    pub(super) run: Option<Run>,
    pub(super) worker: Option<Worker>,
    pub(super) cancel: Arc<AtomicBool>,
    pub(super) signal: Arc<AtomicBool>,
    pub(super) launch_interactive: bool,
    pub(super) logs: VecDeque<String>,
    pub(super) log_scroll: usize,
    /// A finished run shows results unless the output log was chosen.
    pub(super) show_output: bool,
    pub(super) results_scroll: u16,
    pub(super) changes: Option<completion::Loaded>,
    pub(super) changes_job: Option<(String, Receiver<completion::Loaded>)>,
    /// The run checkpoint whose waiver the status line last announced.
    pub(super) waiver_shown: String,
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
            workflow: 0,
            selected: 0,
            task: String::new(),
            worktree: false,
            dirty: false,
            page: 0,
            modal: None,
            message: "Define a task, choose your stages, then run. Review is optional.".into(),
            alert: String::new(),
            run: None,
            worker: None,
            signal: cancel,
            cancel: Arc::new(AtomicBool::new(false)),
            launch_interactive: false,
            logs: VecDeque::new(),
            log_scroll: 0,
            show_output: false,
            results_scroll: 0,
            changes: None,
            changes_job: None,
            waiver_shown: String::new(),
            history: vec![],
            history_selected: 0,
            runs: RunsView::default(),
            harness_selected: 0,
            harness_checks: HarnessChecks::new(),
            catalog: None,
            model_editor: None,
            stage_editor: None,
            catalog_harness: String::new(),
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
    pub(super) fn start_driver(&mut self) -> Result<()> {
        ensure!(self.worker.is_none(), "A stage is still running");
        let mut run = self.run.clone().context("No run selected")?;
        self.cancel.store(false, Ordering::Relaxed);
        let cancel = self.cancel.clone();
        let (sender, receiver) = mpsc::sync_channel(128);
        let handle = thread::spawn(move || {
            let action = engine::drive(&mut run, &cancel, |event| {
                let _ = sender.try_send(WorkerMessage::Event(event));
            })
            .map_err(|e| format!("{e:#}"));
            let _ = sender.send(WorkerMessage::Done(Box::new(run), action));
        });
        self.worker = Some(Worker {
            receiver,
            thread: Some(handle),
        });
        self.page = 1;
        Ok(())
    }
    pub(super) fn poll(&mut self) -> Result<bool> {
        if let Some(Modal::Import(importer)) = &mut self.modal {
            importer.poll();
        }
        let messages: Vec<_> = self
            .worker
            .as_ref()
            .map(|w| w.receiver.try_iter().collect())
            .unwrap_or_default();
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
                    match result {
                        Ok(Action::Interactive) => self.launch_interactive = !self.quit_after_stop,
                        Ok(Action::Confirm) => {
                            self.message =
                                "Inspect the handoff: [h]. Advance: [a]. Reopen: [i]. Save and exit: [q]."
                                    .into()
                        }
                        Ok(Action::Complete) => {
                            self.message =
                                "Workflow complete. Artifacts and handoffs are saved.".into()
                        }
                        Ok(Action::Stopped) => {
                            self.message = self
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
            }
        }
        if let Some(receiver) = &self.catalog {
            if let Ok(result) = receiver.try_recv() {
                self.catalog = None;
                match result {
                    Ok(catalog) => {
                        let items = catalog
                            .models
                            .into_iter()
                            .map(|model| Choice {
                                label: format!(
                                    "{}{}{}  ·  {}",
                                    if model.provider.is_empty() {
                                        ""
                                    } else {
                                        &model.provider
                                    },
                                    if model.provider.is_empty() { "" } else { "/" },
                                    model.label,
                                    model.availability
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
        Ok(self.quit_after_stop && self.worker.is_none())
    }
    pub(super) fn model_picker(&mut self) {
        self.model_picker_for(self.stage().harness.clone());
    }
    pub(super) fn model_picker_for(&mut self, harness: String) {
        self.catalog_harness = harness.clone();
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
    pub(super) fn switch_project(&mut self, path: PathBuf) -> Result<()> {
        ensure!(
            self.worker.is_none(),
            "Stop the active run before changing projects"
        );
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
        self.run = None;
        self.task.clear();
        self.history.clear();
        self.history_selected = 0;
        self.harness_selected = 0;
        self.harness_checks.clear();
        self.logs.clear();
        self.log_scroll = 0;
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
        if self.modal.is_some() {
            return self.modal_key(key);
        }
        if self.runs_filtering() {
            return self.runs_filter_key(key);
        }
        if let Some(id) = actions::resolve(self.scope(), key) {
            return self.perform(id);
        }
        if self.worker.is_some() && self.page == 0 {
            self.message =
                "Workflow is running. Use the monitor to cancel; edit after it stops.".into();
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
                if self.worker.is_none() {
                    return Ok(true);
                }
                self.cancel.store(true, Ordering::Relaxed);
                self.message = "Stopping the stage and saving its artifacts…".into();
            }
            ActionId::Quit => {
                if self.dirty {
                    self.save(false)?;
                }
                if self.worker.is_none() {
                    return Ok(true);
                }
                self.cancel.store(true, Ordering::Relaxed);
                self.quit_after_stop = true;
                self.message = "Stopping and saving before exit…".into();
            }
            ActionId::Themes => self.choose_theme(),
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
            1 if self.worker.is_some() => Scope::MonitorRunning,
            1 => Scope::MonitorIdle,
            2 => Scope::Runs,
            _ => Scope::Harnesses,
        }
    }
    /// Where actions apply: workflow edits wait until the run stops.
    fn scope(&self) -> Scope {
        if self.page == 0 && self.worker.is_some() {
            Scope::Global
        } else {
            self.page_scope()
        }
    }
}
impl Drop for App {
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
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(10),
        Constraint::Length(rows_for(&status, body_width, 2)),
        Constraint::Length(rows_for(&footer, area.width, 2)),
    ])
    .split(area);
    draw_header(frame, app, chunks[0], palette);
    draw_repository(frame, app, chunks[1], palette);
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

fn draw_header(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) {
    let mut spans = vec![
        Span::styled(" ◆ ", Style::default().fg(palette.accent)),
        Span::styled(
            "MODEL ROUTER",
            Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];
    for (index, name) in TABS.iter().enumerate() {
        spans.push(Span::styled(
            format!(" {} {name} ", index + 1),
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
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(palette.bg)),
        area,
    );
}

/// The project line: repository name and branch first, then the path.
fn draw_repository(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) {
    let mut spans = match &app.repo {
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
    let used = Line::from(spans.clone()).width();
    let room = (area.width as usize).saturating_sub(used + 4);
    if room >= 12 {
        spans.push(Span::styled(
            format!("  {}", short_path(&app.project, room)),
            Style::default().fg(palette.muted),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(palette.bg)),
        area,
    );
}
