use crate::{
    adapters,
    catalog::{self, Catalog, Model},
    config::{self, Mode, Role, Settings, Stage},
    engine::{self, Action, Event as RunEvent, Run, Status},
    presets, storage,
};
use anyhow::{Context, Result, ensure};
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode as Key, KeyEvent,
        KeyEventKind, KeyModifiers as Mods,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table,
        TableState, Tabs, Wrap,
    },
};
use std::{
    collections::VecDeque,
    io::{self, Stdout},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tui_textarea::TextArea;

mod path_picker;
mod startup;
mod task_importer;
use path_picker::{PathPicker, PickerAction};
use task_importer::{ImportAction, TaskImporter};

const BG: Color = Color::Rgb(13, 20, 33);
const PANEL: Color = Color::Rgb(19, 29, 45);
const FG: Color = Color::Rgb(222, 231, 242);
const MUTED: Color = Color::Rgb(141, 160, 180);
const ACCENT: Color = Color::Rgb(79, 209, 197);
const LINE: Color = Color::Rgb(46, 65, 86);
const WARN: Color = Color::Rgb(246, 193, 91);
const RED: Color = Color::Rgb(250, 126, 126);

struct Screen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    active: bool,
}
impl Screen {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode()?;
        if let Err(e) = execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste) {
            let _ = terminal::disable_raw_mode();
            return Err(e.into());
        }
        let mut screen = Self {
            terminal: Terminal::new(CrosstermBackend::new(io::stdout()))?,
            active: true,
        };
        screen.terminal.clear()?;
        Ok(screen)
    }
    fn resume(&mut self) -> Result<()> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        self.active = true;
        self.terminal.clear()?;
        Ok(())
    }
    fn restore(&mut self) {
        if self.active {
            let _ = terminal::disable_raw_mode();
            let _ = execute!(
                io::stdout(),
                DisableBracketedPaste,
                event::DisableMouseCapture,
                LeaveAlternateScreen,
                crossterm::style::ResetColor,
                crossterm::cursor::Show
            );
            self.active = false;
        }
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        self.restore();
    }
}

#[derive(Clone, Copy)]
enum InputTarget {
    Task,
    NewWorkflow,
    RenameWorkflow,
    RetryModel,
    RepairLimit,
    Handoff,
}
#[derive(Clone)]
enum PickValue {
    Workflow(usize),
    NewWorkflow,
    WorkflowPreset(usize),
    StagePreset(usize),
    Harness(String),
    Model(Model),
}
impl PickValue {
    fn detail(&self) -> Option<String> {
        match self {
            Self::StagePreset(index) => {
                let preset = &presets::STAGES[*index];
                let stage = preset.build();
                Some(format!(
                    "{}\nDefault: {} / {} · {}",
                    preset.purpose,
                    stage.harness,
                    if stage.model.is_empty() {
                        "native default"
                    } else {
                        &stage.model
                    },
                    stage.mode.label()
                ))
            }
            Self::WorkflowPreset(index) => {
                let preset = &presets::WORKFLOWS[*index];
                Some(format!("{}\n{}", preset.purpose, preset.stages.join(" → ")))
            }
            _ => None,
        }
    }
}
struct Choice {
    label: String,
    value: PickValue,
}
enum Modal {
    Input {
        title: String,
        target: InputTarget,
        text: TextArea<'static>,
    },
    Stage {
        editor: Box<StageEditor>,
        insert_at: Option<usize>,
    },
    Repositories(Box<PathPicker>),
    Import(Box<TaskImporter>),
    Picker {
        title: String,
        items: Vec<Choice>,
        selected: usize,
        filter: TextArea<'static>,
        note: String,
    },
    Text {
        title: String,
        body: String,
        scroll: u16,
    },
    Loading(String),
}

const FIELDS: &[&str] = &[
    "Name",
    "Role",
    "Harness",
    "Model / alias / mode",
    "Provider (blank = native config)",
    "Reasoning effort",
    "Execution",
    "Timeout (seconds)",
    "Review repair target (stage ID; blank = nearest)",
    "Extra arguments (shell quoting; no shell execution)",
    "Stage instructions",
];
struct StageEditor {
    stage: Stage,
    fields: Vec<TextArea<'static>>,
    selected: usize,
    harnesses: Vec<String>,
}
fn textarea(value: &str) -> TextArea<'static> {
    let mut text = TextArea::from(value.split('\n').map(str::to_owned).collect::<Vec<_>>());
    text.set_style(Style::default().fg(FG).bg(PANEL));
    text.set_cursor_line_style(Style::default());
    text.set_cursor_style(Style::default().fg(BG).bg(ACCENT));
    text.set_selection_style(Style::default().fg(BG).bg(ACCENT));
    text
}
fn content(text: &TextArea<'_>) -> String {
    text.lines().join("\n")
}
impl StageEditor {
    fn new(stage: Stage, harnesses: Vec<String>) -> Self {
        let args = stage
            .extra_args
            .iter()
            .map(|a| shell_words::quote(a).into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let fields = [
            &stage.name,
            stage.role.label(),
            &stage.harness,
            &stage.model,
            &stage.provider,
            &stage.effort,
            stage.mode.label(),
            &stage.timeout_seconds.to_string(),
            &stage.repair_target,
            &args,
            &stage.instructions,
        ]
        .into_iter()
        .map(textarea)
        .collect();
        Self {
            stage,
            fields,
            selected: 0,
            harnesses,
        }
    }
    fn finish(&self) -> Result<Stage> {
        let mut stage = self.stage.clone();
        stage.name = content(&self.fields[0]).trim().into();
        stage.role = Role::ALL
            .into_iter()
            .find(|r| r.label() == content(&self.fields[1]))
            .context("Unknown role")?;
        stage.harness = content(&self.fields[2]);
        stage.model = content(&self.fields[3]).trim().into();
        stage.provider = content(&self.fields[4]).trim().into();
        stage.effort = content(&self.fields[5]).trim().into();
        stage.mode = if content(&self.fields[6]) == "interactive" {
            Mode::Interactive
        } else {
            Mode::Unattended
        };
        stage.timeout_seconds = content(&self.fields[7])
            .trim()
            .parse()
            .context("Timeout must be a positive number of seconds")?;
        stage.repair_target = content(&self.fields[8]).trim().into();
        stage.extra_args = shell_words::split(&content(&self.fields[9]))
            .context("Extra arguments have unmatched quotes")?;
        stage.instructions = content(&self.fields[10]);
        Ok(stage)
    }
    fn key(&mut self, key: KeyEvent) {
        if key.code == Key::Tab {
            self.selected = (self.selected + 1) % self.fields.len();
        } else if key.code == Key::BackTab {
            self.selected = (self.selected + self.fields.len() - 1) % self.fields.len();
        } else if [1, 2, 6].contains(&self.selected) {
            if matches!(
                key.code,
                Key::Left | Key::Right | Key::Char(' ') | Key::Enter
            ) {
                let values: Vec<String> = match self.selected {
                    1 => Role::ALL.iter().map(|r| r.label().into()).collect(),
                    2 => self.harnesses.clone(),
                    _ => vec!["unattended".into(), "interactive".into()],
                };
                let current = values
                    .iter()
                    .position(|v| *v == content(&self.fields[self.selected]))
                    .unwrap_or(0);
                let next = if key.code == Key::Left {
                    (current + values.len() - 1) % values.len()
                } else {
                    (current + 1) % values.len()
                };
                self.fields[self.selected] = textarea(&values[next]);
                if self.selected == 2 {
                    for index in 3..=5 {
                        self.fields[index] = textarea("");
                    }
                }
            }
        } else if key.code == Key::Enter && self.selected != 10 {
            self.selected = (self.selected + 1) % self.fields.len();
        } else {
            self.fields[self.selected].input(key);
        }
    }
}

enum WorkerMessage {
    Event(RunEvent),
    Done(Box<Run>, std::result::Result<Action, String>),
}
struct Worker {
    receiver: Receiver<WorkerMessage>,
    thread: Option<JoinHandle<()>>,
}
struct App {
    project: PathBuf,
    config_path: PathBuf,
    settings: Settings,
    workflow: usize,
    selected: usize,
    task: String,
    worktree: bool,
    dirty: bool,
    page: usize,
    modal: Option<Modal>,
    message: String,
    run: Option<Run>,
    worker: Option<Worker>,
    cancel: Arc<AtomicBool>,
    signal: Arc<AtomicBool>,
    launch_interactive: bool,
    logs: VecDeque<String>,
    log_scroll: usize,
    history: Vec<Run>,
    history_selected: usize,
    harness_selected: usize,
    catalog: Option<Receiver<std::result::Result<Catalog, String>>>,
    quit_after_stop: bool,
}
impl App {
    fn new(
        project: PathBuf,
        settings: Settings,
        config_path: PathBuf,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        Self {
            project,
            settings,
            config_path,
            workflow: 0,
            selected: 0,
            task: String::new(),
            worktree: false,
            dirty: false,
            page: 0,
            modal: None,
            message: "Define a task, choose your stages, then run. Review is optional.".into(),
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
            quit_after_stop: false,
        }
    }
    fn stage(&self) -> &Stage {
        &self.settings.workflows[self.workflow].stages[self.selected]
    }
    fn stage_mut(&mut self) -> &mut Stage {
        &mut self.settings.workflows[self.workflow].stages[self.selected]
    }
    fn input(&mut self, title: &str, target: InputTarget, value: &str) {
        self.modal = Some(Modal::Input {
            title: title.into(),
            target,
            text: textarea(value),
        });
    }
    fn text(&mut self, title: &str, body: String) {
        self.modal = Some(Modal::Text {
            title: title.into(),
            body,
            scroll: 0,
        });
    }
    fn save(&mut self, global: bool) -> Result<()> {
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
    fn start_driver(&mut self) -> Result<()> {
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
    fn poll(&mut self) -> Result<bool> {
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
                                "Inspect the handoff: h. Advance: a. Reopen: i. Save and exit: q."
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
                        Err(error) => self.message = error,
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
                        self.modal = None;
                        self.message = format!("{error}. Use e to enter a model ID manually.");
                    }
                }
            }
        }
        Ok(self.quit_after_stop && self.worker.is_none())
    }
    fn model_picker(&mut self) {
        let harness = self.stage().harness.clone();
        let settings = self.settings.clone();
        let project = self.project.clone();
        let (sender, receiver) = mpsc::channel();
        self.catalog = Some(receiver);
        self.modal = Some(Modal::Loading(format!(
            "Reading {harness} model selections…  Esc cancels"
        )));
        thread::spawn(move || {
            let _ = sender.send(
                catalog::discover(&harness, &settings, &project).map_err(|e| format!("{e:#}")),
            );
        });
    }
    fn pick(&mut self, choice: PickValue) -> Result<()> {
        match choice {
            PickValue::Workflow(index) => {
                self.workflow = index;
                self.selected = 0;
            }
            PickValue::NewWorkflow => self.input(
                "Name the new workflow (copies current stages)",
                InputTarget::NewWorkflow,
                "",
            ),
            PickValue::WorkflowPreset(index) => {
                let mut workflow = presets::WORKFLOWS[index].build();
                let base = workflow.name.clone();
                let mut suffix = 2;
                while self
                    .settings
                    .workflows
                    .iter()
                    .any(|w| w.name == workflow.name)
                {
                    workflow.name = format!("{base} ({suffix})");
                    suffix += 1;
                }
                let mut candidate = self.settings.clone();
                candidate.workflows.push(workflow);
                candidate.validate()?;
                self.settings = candidate;
                self.workflow = self.settings.workflows.len() - 1;
                self.selected = 0;
                self.dirty = true;
                self.message =
                    "Workflow preset added. e edits each stage; reviews start disabled.".into();
            }
            PickValue::StagePreset(index) => {
                ensure!(
                    self.settings.workflows[self.workflow].stages.len() < 64,
                    "A workflow can contain at most 64 stages"
                );
                let mut stage = presets::STAGES[index].build();
                stage.id = format!("{}-{}", stage.id, Stage::default().id);
                self.modal = Some(Modal::Stage {
                    editor: Box::new(StageEditor::new(
                        stage,
                        catalog::known_harnesses(&self.settings),
                    )),
                    insert_at: Some(self.selected + 1),
                });
            }
            PickValue::Harness(harness) => {
                let stage = self.stage_mut();
                stage.harness = harness;
                stage.model.clear();
                stage.provider.clear();
                stage.effort.clear();
                self.dirty = true;
            }
            PickValue::Model(model) => {
                let stage = self.stage_mut();
                stage.model = model.id;
                stage.provider = model.provider;
                self.dirty = true;
            }
        }
        Ok(())
    }
    fn switch_project(&mut self, path: PathBuf) -> Result<()> {
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
    fn submit_input(&mut self, target: InputTarget, value: String) -> Result<()> {
        match target {
            InputTarget::Task => {
                config::validate_task(&value)?;
                self.task = value;
            }
            InputTarget::NewWorkflow | InputTarget::RenameWorkflow => {
                let value = value.trim();
                ensure!(!value.is_empty(), "Workflow name is required");
                ensure!(
                    !self.settings.workflows.iter().any(|w| w.name == value),
                    "That workflow name already exists"
                );
                if matches!(target, InputTarget::NewWorkflow) {
                    let mut copy = self.settings.workflows[self.workflow].clone();
                    copy.name = value.into();
                    self.settings.workflows.push(copy);
                    self.workflow = self.settings.workflows.len() - 1;
                    self.selected = 0;
                } else {
                    self.settings.workflows[self.workflow].name = value.into();
                }
                self.dirty = true;
            }
            InputTarget::RetryModel => {
                self.run
                    .as_mut()
                    .context("No run")?
                    .retry(false, Some(value.trim().into()))?;
                self.start_driver()?;
            }
            InputTarget::RepairLimit => {
                self.settings.workflows[self.workflow].max_repair_passes = value
                    .trim()
                    .parse()
                    .context("Enter a nonnegative integer")?;
                self.dirty = true;
            }
            InputTarget::Handoff => {
                engine::edit_handoff(self.run.as_mut().context("No run")?, value)?
            }
        }
        Ok(())
    }
    fn modal_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(mut modal) = self.modal.take() else {
            return Ok(());
        };
        if key.code == Key::Esc && !matches!(modal, Modal::Repositories(_) | Modal::Import(_)) {
            self.catalog = None;
            return Ok(());
        }
        let mut keep = true;
        let result: Result<()> = (|| {
            match &mut modal {
                Modal::Input { target, text, .. } => {
                    if key.modifiers.contains(Mods::CONTROL) && key.code == Key::Char('s') {
                        self.submit_input(*target, content(text))?;
                        keep = false;
                    } else {
                        text.input(key);
                    }
                }
                Modal::Stage { editor, insert_at } => {
                    if key.modifiers.contains(Mods::CONTROL) && key.code == Key::Char('s') {
                        let stage = editor.finish()?;
                        let mut candidate = self.settings.clone();
                        if let Some(index) = insert_at {
                            candidate.workflows[self.workflow]
                                .stages
                                .insert(*index, stage);
                        } else {
                            candidate.workflows[self.workflow].stages[self.selected] = stage;
                        }
                        candidate.validate()?;
                        self.settings = candidate;
                        if let Some(index) = insert_at {
                            self.selected = *index;
                            self.message =
                                "Stage added. J/K reorders; Space enables or disables it.".into();
                        }
                        self.dirty = true;
                        keep = false;
                    } else {
                        editor.key(key);
                    }
                }
                Modal::Repositories(picker) => {
                    let action = picker.key(key).and_then(|action| {
                        if let PickerAction::Select(path) = action {
                            self.switch_project(path)?;
                            Ok(PickerAction::Close)
                        } else {
                            Ok(action)
                        }
                    });
                    match action {
                        Ok(PickerAction::Close) => keep = false,
                        Ok(_) => {}
                        Err(error) => picker.error = format!("{error:#}"),
                    }
                }
                Modal::Import(importer) => match importer.key(key) {
                    ImportAction::Stay => {}
                    ImportAction::Close => keep = false,
                    ImportAction::Apply(task) => {
                        self.task = task;
                        self.message = "Task imported. t edits it; r starts the workflow.".into();
                        keep = false;
                    }
                },
                Modal::Text { scroll, .. } => match key.code {
                    Key::Up | Key::Char('k') => *scroll = scroll.saturating_sub(1),
                    Key::Down | Key::Char('j') => *scroll = scroll.saturating_add(1),
                    Key::PageDown => *scroll = scroll.saturating_add(15),
                    Key::PageUp => *scroll = scroll.saturating_sub(15),
                    Key::Char('q') => keep = false,
                    _ => {}
                },
                Modal::Picker {
                    items,
                    selected,
                    filter,
                    ..
                } => {
                    let query = content(filter).to_lowercase();
                    let matches: Vec<usize> = items
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| c.label.to_lowercase().contains(&query))
                        .map(|(i, _)| i)
                        .collect();
                    match key.code {
                        Key::Up => *selected = selected.saturating_sub(1),
                        Key::Down => {
                            *selected = (*selected + 1).min(matches.len().saturating_sub(1))
                        }
                        Key::Enter if !matches.is_empty() => {
                            let index = matches[(*selected).min(matches.len() - 1)];
                            self.pick(items[index].value.clone())?;
                            keep = false;
                        }
                        _ => {
                            filter.input(key);
                            *selected = 0;
                        }
                    }
                }
                Modal::Loading(_) => {}
            }
            Ok(())
        })();
        if keep {
            self.modal = Some(modal);
        }
        result
    }
    fn key(&mut self, key: KeyEvent) -> Result<bool> {
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
            Key::Char('?') => {
                self.text("Keyboard guide", HELP.into());
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
            2 => {
                match key.code {
                    Key::Down | Key::Char('j') => {
                        self.history_selected =
                            (self.history_selected + 1).min(self.history.len().saturating_sub(1))
                    }
                    Key::Up | Key::Char('k') => {
                        self.history_selected = self.history_selected.saturating_sub(1)
                    }
                    Key::Char('r') => self.history = engine::history(&self.project)?,
                    Key::Enter if self.worker.is_none() => {
                        if let Some(run) = self.history.get(self.history_selected).cloned() {
                            self.run = Some(run);
                            self.page = 1;
                            self.logs.clear();
                            self.message = "Saved run loaded. c continues; y retries; i reopens interactively.".into();
                        }
                    }
                    _ => {}
                }
            }
            3 => {
                let harnesses = catalog::known_harnesses(&self.settings);
                match key.code {
                    Key::Down | Key::Char('j') => {
                        self.harness_selected =
                            (self.harness_selected + 1).min(harnesses.len().saturating_sub(1))
                    }
                    Key::Up | Key::Char('k') => {
                        self.harness_selected = self.harness_selected.saturating_sub(1)
                    }
                    Key::Enter => {
                        let harness = &harnesses[self.harness_selected];
                        self.text(harness, format!("Executable: {}\n\n{}\n\nSelect this harness with h on the workflow page, then press m to load model selections. Use e to enter any native model ID, alias, or mode.\n\nPermissions and authentication come from the harness's configuration.\n\nCustom adapters are defined in TOML. See README.md for the template contract.", adapters::program(harness, &self.settings.adapters), if harness == "amp" { "Amp uses mode keys. A mode determines its model and tools." } else { "A model must be supported by the selected harness/provider." }));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(false)
    }
    fn workflow_key(&mut self, key: KeyEvent) -> Result<()> {
        let count = self.settings.workflows[self.workflow].stages.len();
        match key.code {
            Key::Down | Key::Char('j') => self.selected = (self.selected + 1).min(count - 1),
            Key::Up | Key::Char('k') => self.selected = self.selected.saturating_sub(1),
            Key::Char(' ') => {
                let stage = self.stage_mut();
                stage.enabled = !stage.enabled;
                self.dirty = true;
            }
            Key::Char('v') => {
                let stage = self.stage_mut();
                stage.mode = if stage.mode == Mode::Interactive {
                    Mode::Unattended
                } else {
                    Mode::Interactive
                };
                self.dirty = true;
            }
            Key::Char('t') => self.input(
                "Task · Ctrl+S applies · Enter adds a line",
                InputTarget::Task,
                &self.task.clone(),
            ),
            Key::Char('p') => {
                self.modal = Some(Modal::Repositories(Box::new(PathPicker::new(
                    &self.project,
                    storage::recent_projects(),
                )?)));
            }
            Key::Char('f') => {
                self.modal = Some(Modal::Import(Box::new(TaskImporter::new(
                    &self.project,
                    &self.task,
                )?)));
            }
            Key::Char('w') => {
                let mut items: Vec<_> = self
                    .settings
                    .workflows
                    .iter()
                    .enumerate()
                    .map(|(i, w)| Choice {
                        label: w.name.clone(),
                        value: PickValue::Workflow(i),
                    })
                    .collect();
                items.push(Choice {
                    label: "+ New workflow from current".into(),
                    value: PickValue::NewWorkflow,
                });
                items.extend(
                    presets::WORKFLOWS
                        .iter()
                        .enumerate()
                        .map(|(index, preset)| Choice {
                            label: format!("+ Preset: {}", preset.name),
                            value: PickValue::WorkflowPreset(index),
                        }),
                );
                self.modal = Some(Modal::Picker {
                    title: "Choose workflow".into(),
                    items,
                    selected: self.workflow,
                    filter: textarea(""),
                    note: "Presets add an editable workflow. Reviews start disabled; existing workflows are kept."
                        .into(),
                });
            }
            Key::Char('R') => self.input(
                "Rename workflow",
                InputTarget::RenameWorkflow,
                &self.settings.workflows[self.workflow].name.clone(),
            ),
            Key::Char('l') => self.input(
                "Maximum automatic repair passes (0 disables repairs)",
                InputTarget::RepairLimit,
                &self.settings.workflows[self.workflow]
                    .max_repair_passes
                    .to_string(),
            ),
            Key::Char('h') => {
                let items = catalog::known_harnesses(&self.settings)
                    .into_iter()
                    .map(|name| Choice {
                        label: format!(
                            "{} · {}",
                            name,
                            if adapters::executable(&adapters::program(
                                &name,
                                &self.settings.adapters
                            ))
                            .is_some()
                            {
                                "installed"
                            } else {
                                "not installed"
                            }
                        ),
                        value: PickValue::Harness(name),
                    })
                    .collect();
                self.modal = Some(Modal::Picker {
                    title: "Harness for selected stage".into(),
                    items,
                    selected: 0,
                    filter: textarea(""),
                    note: "Changing harness clears the previous model/provider/effort selection."
                        .into(),
                });
            }
            Key::Char('e') | Key::Enter => {
                self.modal = Some(Modal::Stage {
                    editor: Box::new(StageEditor::new(
                        self.stage().clone(),
                        catalog::known_harnesses(&self.settings),
                    )),
                    insert_at: None,
                })
            }
            Key::Char('m') => self.model_picker(),
            Key::Char('a') => {
                ensure!(count < 64, "A workflow can contain at most 64 stages");
                self.modal = Some(Modal::Picker {
                    title: "Add stage from preset".into(),
                    items: presets::STAGES
                        .iter()
                        .enumerate()
                        .map(|(index, preset)| Choice {
                            label: format!("{} · {}", preset.name, preset.role.label()),
                            value: PickValue::StagePreset(index),
                        })
                        .collect(),
                    selected: 0,
                    filter: textarea(""),
                    note:
                        "Choose, edit, then Ctrl+S to insert after the selected stage. Esc cancels."
                            .into(),
                });
            }
            Key::Char('d') => {
                ensure!(count < 64, "A workflow can contain at most 64 stages");
                let mut copy = self.stage().clone();
                copy.id = Stage::default().id;
                copy.name.push_str(" copy");
                self.settings.workflows[self.workflow]
                    .stages
                    .insert(self.selected + 1, copy);
                self.selected += 1;
                self.dirty = true;
            }
            Key::Char('x') => {
                ensure!(count > 1, "Keep at least one stage");
                self.settings.workflows[self.workflow]
                    .stages
                    .remove(self.selected);
                self.selected = self.selected.min(count - 2);
                self.dirty = true;
            }
            Key::Char('X') => {
                ensure!(
                    self.settings.workflows.len() > 1,
                    "Keep at least one workflow"
                );
                self.settings.workflows.remove(self.workflow);
                self.workflow = 0;
                self.selected = 0;
                self.dirty = true;
            }
            Key::Char('K') if self.selected > 0 => {
                self.settings.workflows[self.workflow]
                    .stages
                    .swap(self.selected, self.selected - 1);
                self.selected -= 1;
                self.dirty = true;
            }
            Key::Char('J') if self.selected + 1 < count => {
                self.settings.workflows[self.workflow]
                    .stages
                    .swap(self.selected, self.selected + 1);
                self.selected += 1;
                self.dirty = true;
            }
            Key::Char('g') => self.worktree = !self.worktree,
            Key::Char('s') => self.save(false)?,
            Key::Char('S') => self.save(true)?,
            Key::Char('P') => self.text(
                "Launch preview · no model is started",
                engine::preview(
                    &self.settings,
                    &self.settings.workflows[self.workflow],
                    &self.project,
                    &self.task,
                )?,
            ),
            Key::Char('r') => {
                self.run = Some(Run::create(
                    &self.project,
                    &self.settings,
                    &self.settings.workflows[self.workflow],
                    self.task.clone(),
                    self.worktree,
                )?);
                self.logs.clear();
                self.log_scroll = 0;
                self.message = "Starting workflow…".into();
                self.start_driver()?;
            }
            _ => {}
        }
        Ok(())
    }
    fn run_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.code == Key::Char('c') && self.worker.is_some() {
            self.cancel.store(true, Ordering::Relaxed);
            self.message = "Stopping stage…".into();
            return Ok(());
        }
        if key.code == Key::PageUp {
            self.log_scroll = self.log_scroll.saturating_add(10).min(self.logs.len());
            return Ok(());
        }
        if key.code == Key::PageDown {
            self.log_scroll = self.log_scroll.saturating_sub(10);
            return Ok(());
        }
        if key.code == Key::Char('h') {
            let run = self.run.as_ref().context("No run selected")?;
            let index = run.cursor.min(run.stages.len().saturating_sub(1));
            let handoff = run.stages[index]
                .handoff
                .as_ref()
                .or_else(|| run.stages.iter().rev().find_map(|s| s.handoff.as_ref()));
            let body = handoff
                .map(serde_json::to_string_pretty)
                .transpose()?
                .unwrap_or_else(|| {
                    format!(
                        "No completed handoff yet.\n\n{}\n\nArtifacts: {}",
                        run.error.as_deref().unwrap_or(""),
                        run.directory.display()
                    )
                });
            self.text("Handoff · arrows scroll · Esc closes", body);
            return Ok(());
        }
        if self.worker.is_some() {
            return Ok(());
        }
        match key.code {
            Key::Char('c') => self.start_driver()?,
            Key::Char('a') => {
                self.run.as_mut().context("No run")?.confirm()?;
                self.start_driver()?;
            }
            Key::Char('y') => {
                self.run.as_mut().context("No run")?.retry(false, None)?;
                self.start_driver()?;
            }
            Key::Char('i') => {
                self.run.as_mut().context("No run")?.retry(true, None)?;
                self.start_driver()?;
            }
            Key::Char('m') => {
                let run = self.run.as_ref().context("No run")?;
                let stage = run
                    .workflow
                    .stages
                    .get(run.cursor)
                    .context("Run is complete")?;
                self.input(
                    "Replace model / mode and retry this stage",
                    InputTarget::RetryModel,
                    &stage.model.clone(),
                );
            }
            Key::Char('e') => {
                let run = self.run.as_ref().context("No run")?;
                ensure!(
                    run.status == Status::AwaitingConfirmation,
                    "Only interactive handoffs can be edited"
                );
                let summary = run.stages[run.cursor]
                    .handoff
                    .as_ref()
                    .map(|h| h.summary.clone())
                    .unwrap_or_default();
                self.input(
                    "Edit interactive handoff summary",
                    InputTarget::Handoff,
                    &summary,
                );
            }
            Key::Char('n') => self.page = 0,
            _ => {}
        }
        Ok(())
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

fn panel(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(LINE))
        .style(Style::default().bg(PANEL).fg(FG))
}
fn selected_style() -> Style {
    Style::default()
        .bg(Color::Rgb(29, 61, 70))
        .fg(ACCENT)
        .add_modifier(Modifier::BOLD)
}
fn status_color(status: Status) -> Color {
    match status {
        Status::Complete => ACCENT,
        Status::Failed | Status::Blocked | Status::Cancelled => RED,
        Status::Running | Status::Interactive | Status::AwaitingConfirmation => WARN,
        _ => MUTED,
    }
}
fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(4));
    let height = height.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG).fg(FG)), area);
    if area.width < 65 || area.height < 20 {
        frame.render_widget(
            Paragraph::new("Model Router\nResize to at least 65 × 20. Press q to exit.")
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(ACCENT)),
            area,
        );
        return;
    }
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Min(10),
        Constraint::Length(2),
        Constraint::Length(2),
    ])
    .margin(1)
    .split(area);
    let title = Line::from(vec![
        Span::styled(
            " MODEL ROUTER ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" / ", Style::default().fg(LINE)),
        Span::styled(
            app.project.display().to_string(),
            Style::default().fg(MUTED),
        ),
    ]);
    frame.render_widget(Paragraph::new(title), chunks[0]);
    frame.render_widget(
        Tabs::new(["1 Workflow", "2 Monitor", "3 Runs", "4 Harnesses"])
            .select(app.page)
            .style(Style::default().fg(MUTED))
            .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
            .divider("  "),
        chunks[1],
    );
    match app.page {
        0 => draw_workflow(frame, app, chunks[2]),
        1 => draw_run(frame, app, chunks[2]),
        2 => draw_history(frame, app, chunks[2]),
        _ => draw_harnesses(frame, app, chunks[2]),
    }
    frame.render_widget(
        Paragraph::new(storage::clean_terminal(&app.message))
            .style(Style::default().fg(WARN))
            .wrap(Wrap { trim: false }),
        chunks[3],
    );
    let help = match app.page {
        0 => {
            "p repos  t task  f import  e edit  m models  Space enable  r run  s save  ? help  q quit"
        }
        1 if app.worker.is_some() => {
            "c cancel stage  h handoff  PgUp/PgDn logs  Tab switch view  q stop & exit"
        }
        1 => {
            "a advance  c continue  y retry  i interactive  m model  h handoff  e summary  n workflow"
        }
        2 => "↑↓ select  Enter inspect saved run  r refresh  Tab switch view  q quit",
        _ => "↑↓ select  Enter details  Tab switch view  ? help  q quit",
    };
    frame.render_widget(
        Paragraph::new(help)
            .style(Style::default().fg(MUTED))
            .wrap(Wrap { trim: false }),
        chunks[4],
    );
    if let Some(modal) = &mut app.modal {
        draw_modal(frame, modal);
    }
}

fn draw_workflow(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(5),
        Constraint::Min(5),
    ])
    .split(area);
    let workflow = &app.settings.workflows[app.workflow];
    let enabled = workflow.stages.iter().filter(|s| s.enabled).count();
    let heading = format!(
        " {}{}  ·  {enabled}/{} stages  ·  {}  ·  repairs ≤ {}",
        workflow.name,
        if app.dirty { " *" } else { "" },
        workflow.stages.len(),
        if app.worktree {
            "isolated worktree"
        } else {
            "current directory"
        },
        workflow.max_repair_passes
    );
    frame.render_widget(
        Paragraph::new(heading)
            .block(panel(
                " Workflow · w choose · R rename · g worktree · l repair limit ",
            ))
            .style(Style::default().fg(ACCENT)),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(if app.task.is_empty() {
            "Press t to write a task, or f to import a task/spec."
        } else {
            &app.task
        })
        .wrap(Wrap { trim: false })
        .block(panel(" Task · t edit · f import file / URL "))
        .style(Style::default().fg(if app.task.is_empty() { MUTED } else { FG })),
        chunks[1],
    );
    let cols = if area.width >= 100 {
        Layout::horizontal([Constraint::Percentage(68), Constraint::Percentage(32)])
            .split(chunks[2])
    } else {
        Layout::horizontal([Constraint::Percentage(100), Constraint::Length(0)]).split(chunks[2])
    };
    let compact = area.height < 20;
    let rows = workflow.stages.iter().enumerate().map(|(index, stage)| {
        Row::new(vec![
            Cell::from(if stage.enabled { "●" } else { "○" })
                .style(Style::default().fg(if stage.enabled { ACCENT } else { MUTED })),
            Cell::from(format!("{}. {}", index + 1, stage.name)),
            Cell::from(stage.harness.clone()),
            Cell::from(if stage.model.is_empty() {
                "(default)".into()
            } else {
                stage.model.clone()
            }),
            Cell::from(if stage.mode == Mode::Interactive {
                "interactive"
            } else {
                "auto"
            }),
        ])
        .style(Style::default().fg(if stage.enabled { FG } else { MUTED }))
        .height(if compact { 1 } else { 2 })
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Percentage(29),
            Constraint::Length(10),
            Constraint::Percentage(34),
            Constraint::Length(11),
        ],
    )
    .header(
        Row::new(["", "STAGE", "HARNESS", "MODEL / MODE", "EXECUTION"])
            .style(Style::default().fg(MUTED))
            .bottom_margin(if compact { 0 } else { 1 }),
    )
    .block(panel(" Stages · a add · e edit · J/K reorder "))
    .row_highlight_style(selected_style());
    frame.render_stateful_widget(
        table,
        cols[0],
        &mut TableState::default().with_selected(Some(app.selected)),
    );
    if cols[1].width > 0 {
        let stage = app.stage();
        let body = format!(
            "{}\n{} · {}\n\n{}\n\nProvider: {}\nEffort: {}\nTimeout: {} min\n\n{}\n\nID: {}",
            stage.name,
            stage.role.label(),
            if stage.enabled { "enabled" } else { "disabled" },
            stage.instructions,
            if stage.provider.is_empty() {
                "harness default"
            } else {
                &stage.provider
            },
            if stage.effort.is_empty() {
                "harness default"
            } else {
                &stage.effort
            },
            stage.timeout_seconds / 60,
            if stage.role == Role::Review {
                "Review is optional. Space toggles this stage. Blocking findings request repairs."
            } else {
                "Permissions: native settings plus explicit stage arguments."
            },
            stage.id
        );
        frame.render_widget(
            Paragraph::new(body)
                .wrap(Wrap { trim: false })
                .block(panel(" Selected stage ")),
            cols[1],
        );
    }
}

fn draw_run(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(run) = &app.run else {
        frame.render_widget(
            Paragraph::new(
                "No run selected. Start one from Workflow, or open a saved run from Runs.",
            )
            .wrap(Wrap { trim: false })
            .block(panel(" Run monitor ")),
            area,
        );
        return;
    };
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length((run.stages.len() as u16 + 3).min(9)),
        Constraint::Min(3),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(format!(
            "{}  ·  {}  ·  repairs {}/{}",
            run.id,
            run.status.label(),
            run.repair_passes,
            run.workflow.max_repair_passes
        ))
        .style(Style::default().fg(status_color(run.status)))
        .block(panel(format!(" {} ", run.workflow.name))),
        chunks[0],
    );
    let rows = run.stages.iter().enumerate().map(|(i, state)| {
        let elapsed = state
            .attempts
            .last()
            .map(|a| {
                (a.finished_at.unwrap_or_else(chrono::Utc::now) - a.started_at)
                    .num_seconds()
                    .max(0)
            })
            .unwrap_or(0);
        Row::new([
            run.workflow.stages[i].name.clone(),
            state.status.label().into(),
            format!("{} attempt(s)", state.attempts.len()),
            format!("{elapsed}s"),
        ])
        .style(Style::default().fg(status_color(state.status)))
    });
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Percentage(40),
                Constraint::Percentage(30),
                Constraint::Length(15),
                Constraint::Length(8),
            ],
        )
        .block(panel(" Progress "))
        .row_highlight_style(selected_style()),
        chunks[1],
        &mut TableState::default().with_selected(Some(run.cursor.min(run.stages.len() - 1))),
    );
    let cols = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(chunks[2]);
    let visible = cols[0].height.saturating_sub(2) as usize;
    let end = app.logs.len().saturating_sub(app.log_scroll);
    let start = end.saturating_sub(visible);
    let logs = if app.logs.is_empty() {
        "Harness output appears here. Some harnesses emit their final response only when the stage finishes.".into()
    } else {
        app.logs
            .iter()
            .skip(start)
            .take(end - start)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    };
    frame.render_widget(
        Paragraph::new(logs)
            .block(panel(" Live output · full logs saved on disk "))
            .style(Style::default().fg(MUTED)),
        cols[0],
    );
    let summary = if let Some(error) = &run.error {
        error.clone()
    } else {
        run.stages
            .get(run.cursor.min(run.stages.len() - 1))
            .and_then(|s| s.handoff.as_ref())
            .map(|h| {
                format!(
                    "{}\n\n{}",
                    if h.structured {
                        "Structured handoff"
                    } else {
                        "Text handoff · inspect before advancing"
                    },
                    h.summary
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "{}\n\nWorkspace: {}\n\nArtifacts: {}",
                    run.current_name(),
                    run.workspace.display(),
                    run.directory.display()
                )
            })
    };
    frame.render_widget(
        Paragraph::new(summary)
            .wrap(Wrap { trim: false })
            .block(panel(" Handoff / status · h inspect ")),
        cols[1],
    );
}

fn draw_history(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if app.history.is_empty() {
        frame.render_widget(Paragraph::new("No saved runs for this project yet. Start a workflow to create a durable run history.").wrap(Wrap { trim: false }).block(panel(" Run history ")), area);
        return;
    }
    let rows = app.history.iter().map(|run| {
        Row::new([
            run.created_at.format("%m-%d %H:%M").to_string(),
            run.status.label().into(),
            run.workflow.name.clone(),
            run.task.replace('\n', " "),
        ])
        .height(2)
    });
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(13),
                Constraint::Length(19),
                Constraint::Percentage(25),
                Constraint::Percentage(45),
            ],
        )
        .header(
            Row::new(["STARTED", "STATUS", "WORKFLOW", "TASK"]).style(Style::default().fg(MUTED)),
        )
        .block(panel(" Saved runs · Enter opens the monitor "))
        .row_highlight_style(selected_style()),
        area,
        &mut TableState::default()
            .with_selected(Some(app.history_selected.min(app.history.len() - 1))),
    );
}
fn draw_harnesses(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let rows = catalog::known_harnesses(&app.settings)
        .into_iter()
        .map(|name| {
            let program = adapters::program(&name, &app.settings.adapters);
            let path = adapters::executable(&program);
            Row::new([
                name,
                if path.is_some() {
                    "installed".into()
                } else {
                    "not installed".into()
                },
                path.map(|p| p.display().to_string()).unwrap_or(program),
            ])
            .height(2)
        });
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(14),
                Constraint::Length(16),
                Constraint::Min(20),
            ],
        )
        .header(
            Row::new(["HARNESS", "EXECUTABLE", "PATH / COMMAND"]).style(Style::default().fg(MUTED)),
        )
        .block(panel(
            " Harnesses · uses existing logins and provider settings ",
        ))
        .row_highlight_style(selected_style()),
        area,
        &mut TableState::default().with_selected(Some(app.harness_selected)),
    );
}

fn draw_modal(frame: &mut Frame<'_>, modal: &mut Modal) {
    let rect = popup(
        frame.area(),
        if matches!(modal, Modal::Stage { .. }) {
            100
        } else {
            95
        },
        if matches!(modal, Modal::Stage { .. }) {
            35
        } else {
            26
        },
    );
    frame.render_widget(Clear, rect);
    match modal {
        Modal::Repositories(picker) => picker.draw(frame, rect),
        Modal::Import(importer) => importer.draw(frame, rect),
        Modal::Input { title, text, .. } => {
            let block = panel(format!(" {title} ")).border_style(Style::default().fg(ACCENT));
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).split(inner);
            frame.render_widget(&*text, chunks[0]);
            frame.render_widget(
                Paragraph::new("Ctrl+S apply  ·  Esc cancel  ·  standard text selection and paste")
                    .style(Style::default().fg(MUTED)),
                chunks[1],
            );
        }
        Modal::Stage { editor, .. } => {
            let block = panel(" Edit stage · Tab next · ←/→ cycle role, harness, execution ")
                .border_style(Style::default().fg(ACCENT));
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let chunks = Layout::vertical([Constraint::Min(8), Constraint::Length(2)]).split(inner);
            let halves =
                Layout::horizontal([Constraint::Percentage(43), Constraint::Percentage(57)])
                    .split(chunks[0]);
            let rows = FIELDS.iter().enumerate().map(|(i, label)| {
                ListItem::new(vec![
                    Line::from(*label),
                    Line::styled(
                        storage::truncate(&content(&editor.fields[i]).replace('\n', " "), 36),
                        Style::default().fg(MUTED),
                    ),
                ])
            });
            frame.render_stateful_widget(
                List::new(rows)
                    .highlight_style(selected_style())
                    .block(Block::default().padding(ratatui::widgets::Padding::right(1))),
                halves[0],
                &mut ListState::default().with_selected(Some(editor.selected)),
            );
            let field = &mut editor.fields[editor.selected];
            field.set_block(
                panel(format!(" {} ", FIELDS[editor.selected]))
                    .border_style(Style::default().fg(ACCENT)),
            );
            frame.render_widget(&*field, halves[1]);
            frame.render_widget(Paragraph::new("Ctrl+S apply stage  ·  Esc cancel  ·  Model IDs are open-ended; blank inherits default").style(Style::default().fg(MUTED)).wrap(Wrap { trim: false }), chunks[1]);
        }
        Modal::Picker {
            title,
            items,
            selected,
            filter,
            note,
        } => {
            let block = panel(format!(" {title} ")).border_style(Style::default().fg(ACCENT));
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let query = content(filter).to_lowercase();
            let choices: Vec<_> = items
                .iter()
                .filter(|c| c.label.to_lowercase().contains(&query))
                .collect();
            *selected = (*selected).min(choices.len().saturating_sub(1));
            let detail = choices.get(*selected).and_then(|c| c.value.detail());
            let chunks = Layout::vertical([
                Constraint::Length(3),
                Constraint::Min(3),
                Constraint::Length(if detail.is_some() { 7 } else { 3 }),
                Constraint::Length(2),
            ])
            .split(inner);
            filter.set_block(panel(" Filter "));
            frame.render_widget(&*filter, chunks[0]);
            frame.render_stateful_widget(
                List::new(choices.iter().map(|c| ListItem::new(c.label.clone())))
                    .highlight_style(selected_style()),
                chunks[1],
                &mut ListState::default().with_selected(Some(*selected)),
            );
            frame.render_widget(
                Paragraph::new(format!(
                    "{}{note}",
                    detail.map(|text| format!("{text}\n\n")).unwrap_or_default()
                ))
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(MUTED)),
                chunks[2],
            );
            frame.render_widget(
                Paragraph::new("↑↓ select · Enter choose · Esc cancel")
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(MUTED)),
                chunks[3],
            );
        }
        Modal::Text {
            title,
            body,
            scroll,
        } => frame.render_widget(
            Paragraph::new(storage::clean_terminal(body))
                .wrap(Wrap { trim: false })
                .scroll((*scroll, 0))
                .block(
                    panel(format!(" {title} · Esc close "))
                        .border_style(Style::default().fg(ACCENT)),
                ),
            rect,
        ),
        Modal::Loading(message) => frame.render_widget(
            Paragraph::new(message.as_str())
                .wrap(Wrap { trim: false })
                .block(panel(" Model discovery · no inference ")),
            rect,
        ),
    }
}

const HELP: &str = "WORKFLOW\nt  Edit task (Ctrl+S applies; Enter adds a line)\nf  Import task/spec: browse files; Ctrl+U opens URL import\n   Preview and edit; Tab append/replace; Ctrl+S apply; Esc back\np  Browse repositories (type to filter; Tab for recent)\n   Enter selects a repo or opens a folder; Right browses inside a repo\n   Left goes to parent; Ctrl+L enters a path; Esc cancels\nw  Select/clone workflow or add a preset    R  Rename    X  Delete workflow\ne / Enter  Edit selected stage\nh  Pick harness    m  Discover models\nSpace  Enable/disable stage, including optional review\nv  Toggle unattended/interactive\na  Add stage preset (edit, then Ctrl+S inserts; Esc cancels)\nd  Duplicate    x  Remove\nJ / K  Move stage down/up\ng  Toggle isolated Git worktree\nl  Set automatic repair limit\nP  Preview commands (no model calls)\ns  Save project    S  Save global templates\nr  Start workflow\n\nMONITOR\nc  Cancel active stage; continue when idle\na  Accept interactive handoff and advance\ny  Explicitly retry current stage\ni  Retry current stage interactively\nm  Replace current model and retry\nh  Inspect handoff/error    PgUp / PgDn  Scroll output\nn  Return to workflow editor\n\nINTERACTIVE HARNESS\nNative terminal keys and paste are forwarded.\nExit the harness to inspect its handoff.\nCtrl+] stops a stuck stage.\n\nEVERYWHERE\n1–4 / Tab  Switch views    ?  Guide\nq  Save workflow edits and exit (stops active work first)\nCtrl+C  Cancel work or exit\n\nSaved runs snapshot their workflow. Template edits affect new runs.\nRetries retain filesystem changes. Disabled reviews never gate a run.\nArtifacts: <project>/.model-router/runs/\nProvider credentials stay with each harness. Catalogs do not prove universal access.";

pub fn run(
    project: PathBuf,
    settings: Settings,
    config_path: PathBuf,
    cancel: Arc<AtomicBool>,
    animate: bool,
) -> Result<()> {
    let old_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
        old_hook(info);
    }));
    let mut screen = Screen::new()?;
    if animate && !startup::play(&mut screen.terminal, &cancel)? {
        return Ok(());
    }
    let mut app = App::new(project, settings, config_path, cancel);
    let _ = storage::remember_project(&app.project);
    loop {
        if app.signal.load(Ordering::Relaxed) {
            app.cancel.store(true, Ordering::Relaxed);
            app.quit_after_stop = true;
        }
        if app.poll()? {
            break;
        }
        if app.launch_interactive {
            app.launch_interactive = false;
            screen.restore();
            let result = engine::run_interactive(
                app.run.as_mut().context("No interactive run")?,
                &app.signal,
            );
            screen.resume()?;
            app.message = match result {
                Ok(()) => "Stage returned. h inspects the handoff; a advances; i reopens.".into(),
                Err(e) => format!("{e:#}"),
            };
            if let Some(run) = &app.run {
                if let Some(error) = &run.error {
                    app.message = error.clone();
                }
            }
        }
        screen.terminal.draw(|frame| draw(frame, &mut app))?;
        if event::poll(Duration::from_millis(70))? {
            match event::read()? {
                Event::Key(key) => match app.key(key) {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(error) => app.message = format!("{error:#}"),
                },
                Event::Paste(text) => match &mut app.modal {
                    Some(Modal::Input { text: field, .. }) => {
                        field.insert_str(text);
                    }
                    Some(Modal::Stage { editor, .. }) if ![1, 2, 6].contains(&editor.selected) => {
                        editor.fields[editor.selected].insert_str(text);
                    }
                    Some(Modal::Picker { filter, .. }) => {
                        filter.insert_str(text);
                    }
                    Some(Modal::Repositories(picker)) => picker.paste(&text),
                    Some(Modal::Import(importer)) => importer.paste(&text),
                    _ => {}
                },
                _ => {}
            }
        }
        if app.cancel.load(Ordering::Relaxed) && app.worker.is_none() {
            app.cancel.store(false, Ordering::Relaxed);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: Key) -> KeyEvent {
        KeyEvent::new(code, Mods::NONE)
    }

    #[test]
    fn stage_presets_are_inserted_only_after_save_and_have_unique_ids() {
        let project = tempfile::tempdir().unwrap();
        let mut app = App::new(
            project.path().into(),
            Settings::default(),
            project.path().join("config.toml"),
            Arc::new(AtomicBool::new(false)),
        );
        let original = toml::to_string(&app.settings).unwrap();
        let preset = presets::STAGES
            .iter()
            .position(|p| p.key == "plan")
            .unwrap();
        app.pick(PickValue::StagePreset(preset)).unwrap();
        assert_eq!(toml::to_string(&app.settings).unwrap(), original);
        app.modal_key(key(Key::Esc)).unwrap();
        assert!(!app.dirty);
        assert_eq!(toml::to_string(&app.settings).unwrap(), original);
        for _ in 0..2 {
            app.pick(PickValue::StagePreset(preset)).unwrap();
            app.modal_key(KeyEvent::new(Key::Char('s'), Mods::CONTROL))
                .unwrap();
            assert_eq!(app.stage().role, Role::Planning);
        }
        let stages = &app.settings.workflows[0].stages;
        assert_eq!(stages.len(), 5);
        assert_eq!(stages[0].id, "research");
        assert_eq!(stages[3].id, "implement");
        assert_ne!(stages[1].id, stages[2].id);
        app.settings.validate().unwrap();
        // Adding or duplicating at the limit must not leave an invalid workflow.
        app.settings.workflows[0].stages = (0..64).map(|_| Stage::default()).collect();
        assert!(app.workflow_key(key(Key::Char('a'))).is_err());
        assert!(app.workflow_key(key(Key::Char('d'))).is_err());
        assert_eq!(app.settings.workflows[0].stages.len(), 64);
    }

    #[test]
    fn workflow_presets_preserve_existing_workflows_and_task() {
        let project = tempfile::tempdir().unwrap();
        let mut app = App::new(
            project.path().into(),
            Settings::default(),
            project.path().join("config.toml"),
            Arc::new(AtomicBool::new(false)),
        );
        let original = toml::to_string(&app.settings.workflows[0]).unwrap();
        app.task = "Keep the imported spec".into();
        app.pick(PickValue::WorkflowPreset(2)).unwrap();
        app.pick(PickValue::WorkflowPreset(2)).unwrap();
        assert_eq!(app.settings.workflows.len(), 3);
        assert_eq!(app.settings.workflows[1].name, "Bug fix");
        assert_eq!(app.settings.workflows[2].name, "Bug fix (2)");
        assert_eq!(app.task, "Keep the imported spec");
        assert_eq!(
            toml::to_string(&app.settings.workflows[0]).unwrap(),
            original
        );
        app.settings.validate().unwrap();
    }

    #[test]
    fn renders_starter_at_common_sizes() {
        let project = tempfile::tempdir().unwrap();
        for (width, height) in [(120, 36), (80, 24), (65, 20), (40, 10)] {
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let mut app = App::new(
                project.path().to_path_buf(),
                Settings::default(),
                PathBuf::from("/tmp/config"),
                Arc::new(AtomicBool::new(false)),
            );
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            if width >= 65 {
                let text = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect::<String>();
                assert!(text.contains("MODEL ROUTER"));
            }
            app.modal = Some(Modal::Repositories(Box::new(
                PathPicker::new(&app.project, vec![]).unwrap(),
            )));
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            if width >= 65 {
                let text = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect::<String>();
                assert!(text.contains("Choose repository"));
                assert!(text.contains("Ctrl+L"));
            }
        }
    }
    #[test]
    fn editor_preserves_multiline_instructions_and_literal_args() {
        let stage = Stage {
            instructions: "line one\nline two".into(),
            extra_args: vec!["--config".into(), "value=a b $(literal)".into()],
            ..Stage::default()
        };
        let result = StageEditor::new(stage.clone(), vec!["codex".into()])
            .finish()
            .unwrap();
        assert_eq!(result.extra_args, stage.extra_args);
        assert_eq!(result.instructions, stage.instructions);
    }
}
