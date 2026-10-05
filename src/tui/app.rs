use super::{
    harness_editor::HarnessEditor,
    harnesses::draw_harnesses,
    history::draw_history,
    modal::{Choice, HELP, InputTarget, Modal, PickValue, draw_modal, matching_choices},
    monitor::draw_run,
    theme::{Palette, Theme},
    widgets::{dim, rows_for, short_path, textarea},
    workflow_page::draw_workflow,
};
use crate::{
    catalog::{self, Catalog},
    config::{self, Settings, Stage},
    engine::{self, Action, Event as RunEvent, Run},
    storage,
};
use anyhow::{Context, Result, ensure};
use crossterm::event::{KeyCode as Key, KeyEvent, KeyEventKind, KeyModifiers as Mods};
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
    pub(super) history: Vec<Run>,
    pub(super) history_selected: usize,
    pub(super) harness_selected: usize,
    pub(super) catalog: Option<Receiver<std::result::Result<Catalog, String>>>,
    pub(super) model_editor: Option<Box<HarnessEditor>>,
    pub(super) quit_after_stop: bool,
}
impl App {
    pub(super) fn new(
        project: PathBuf,
        settings: Settings,
        config_path: PathBuf,
        cancel: Arc<AtomicBool>,
    ) -> Self {
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
            history: vec![],
            history_selected: 0,
            harness_selected: 0,
            catalog: None,
            model_editor: None,
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
                            title: format!("{} selections · type to filter", self.stage().harness),
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
                    }
                }
            }
        }
        Ok(self.quit_after_stop && self.worker.is_none())
    }
    pub(super) fn model_picker(&mut self) {
        let harness = self.stage().harness.clone();
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
            self.modal_key(key)?;
            return Ok(false);
        }
        if key.code == Key::Char('c') && key.modifiers.contains(Mods::CONTROL) {
            if self.worker.is_some() {
                self.cancel.store(true, Ordering::Relaxed);
                self.message = "Stopping the stage and saving its artifacts…".into();
                return Ok(false);
            }
            return Ok(true);
        }
        if key.code == Key::Char('q') {
            if self.dirty {
                self.save(false)?;
            }
            if self.worker.is_some() {
                self.cancel.store(true, Ordering::Relaxed);
                self.quit_after_stop = true;
                self.message = "Stopping and saving before exit…".into();
                return Ok(false);
            }
            return Ok(true);
        }
        match key.code {
            Key::Char('T') => {
                self.choose_theme();
                return Ok(false);
            }
            Key::Char('?') => {
                self.modal = Some(Modal::Text {
                    title: "Keyboard guide".into(),
                    body: HELP.into(),
                    scroll: 0,
                    guide: true,
                });
                return Ok(false);
            }
            Key::Tab => {
                self.page = (self.page + 1) % 4;
                if self.page == 2 {
                    self.history = engine::history(&self.project)?;
                }
                return Ok(false);
            }
            Key::Char('1'..='4') => {
                self.page = match key.code {
                    Key::Char(c) => (c as usize) - ('1' as usize),
                    _ => 0,
                };
                if self.page == 2 {
                    self.history = engine::history(&self.project)?;
                }
                return Ok(false);
            }
            _ => {}
        }
        if self.worker.is_some() && self.page == 0 {
            self.message =
                "Workflow is running. Use the monitor to cancel; edit after it stops.".into();
            return Ok(false);
        }
        match self.page {
            0 => self.workflow_key(key)?,
            1 => self.run_key(key)?,
            2 => self.history_key(key)?,
            3 => self.harnesses_key(key),
            _ => {}
        }
        Ok(false)
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
    let keys: &[(&str, &str)] = match app.page {
        0 => &[
            ("r", "run"),
            ("t", "task"),
            ("f", "import"),
            ("p", "repos"),
            ("c", "harness"),
            ("m", "models"),
            ("Space", "toggle"),
            ("s", "save"),
            ("T", "themes"),
            ("?", "help"),
            ("q", "quit"),
        ],
        1 if app.worker.is_some() => &[
            ("c", "cancel stage"),
            ("h", "handoff"),
            ("PgUp/PgDn", "scroll"),
            ("Tab", "switch view"),
            ("T", "themes"),
            ("q", "stop & exit"),
        ],
        1 => &[
            ("a", "advance"),
            ("c", "continue"),
            ("y", "retry"),
            ("i", "interactive"),
            ("m", "model"),
            ("h", "handoff"),
            ("e", "summary"),
            ("n", "workflow"),
            ("T", "themes"),
            ("?", "help"),
        ],
        2 => &[
            ("↑↓", "select"),
            ("Enter", "open run"),
            ("r", "refresh"),
            ("Tab", "switch view"),
            ("T", "themes"),
            ("q", "quit"),
        ],
        _ => &[
            ("↑↓", "select"),
            ("Enter", "details"),
            ("Tab", "switch view"),
            ("T", "themes"),
            ("?", "help"),
            ("q", "quit"),
        ],
    };
    let mut footer = palette.hints(keys);
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
    let left = Line::from(spans);
    let room = (area.width as usize).saturating_sub(left.width() + 4);
    frame.render_widget(
        Paragraph::new(left).style(Style::default().bg(palette.bg)),
        area,
    );
    if room >= 12 {
        frame.render_widget(
            Paragraph::new(format!("{} ", short_path(&app.project, room)))
                .right_aligned()
                .style(Style::default().fg(palette.muted)),
            area,
        );
    }
}
