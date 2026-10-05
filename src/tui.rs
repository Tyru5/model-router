use crate::{
    adapters,
    catalog::{self, Catalog, Model},
    config::{self, Mode, Role, Settings, Stage},
    engine::{self, Action, Event as RunEvent, Run, Status},
    harness_settings, presets, storage,
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
    layout::{Constraint, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Cell, Clear, HighlightSpacing, List, ListItem, ListState, Padding,
        Paragraph, Row, Table, TableState, Wrap,
    },
};
use std::{
    collections::VecDeque,
    io::{self, Stdout, Write},
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

mod harness_editor;
mod path_picker;
mod startup;
mod task_importer;
mod theme;
use harness_editor::HarnessEditor;
use path_picker::{PathPicker, PickerAction};
use task_importer::{ImportAction, TaskImporter};
use theme::{Palette, Theme};

struct ScreenOutput {
    stdout: Stdout,
    closed: Arc<AtomicBool>,
}

impl Write for ScreenOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.closed.load(Ordering::Relaxed) {
            Ok(bytes.len())
        } else {
            self.stdout.write(bytes)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            Ok(())
        } else {
            self.stdout.flush()
        }
    }
}

struct Screen {
    terminal: Terminal<CrosstermBackend<ScreenOutput>>,
    output_closed: Arc<AtomicBool>,
    active: bool,
}
impl Screen {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode()?;
        if let Err(e) = execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste) {
            let _ = terminal::disable_raw_mode();
            return Err(e.into());
        }
        let output_closed = Arc::new(AtomicBool::new(false));
        let mut screen = Self {
            terminal: Terminal::new(CrosstermBackend::new(ScreenOutput {
                stdout: io::stdout(),
                closed: output_closed.clone(),
            }))?,
            output_closed,
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
        // Ratatui 0.29 retries cursor restoration in Drop and eprintln!s on failure.
        // After our best-effort restore, discard those duplicate writes so a lost
        // terminal (including stderr) cannot turn normal shutdown into a panic.
        self.output_closed.store(true, Ordering::Relaxed);
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
    Theme(Theme),
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
fn matching_choices<'a>(items: &'a [Choice], filter: &TextArea<'_>) -> Vec<&'a Choice> {
    let query = content(filter).to_lowercase();
    items
        .iter()
        .filter(|choice| choice.label.to_lowercase().contains(&query))
        .collect()
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
    Harness(Box<HarnessEditor>),
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
        guide: bool,
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
    TextArea::from(value.split('\n').map(str::to_owned).collect::<Vec<_>>())
}
fn content(text: &TextArea<'_>) -> String {
    text.lines().join("\n")
}
fn menu_key(key: KeyEvent) -> Key {
    match key.code {
        Key::Char('n') if key.modifiers.contains(Mods::CONTROL) => Key::Down,
        Key::Char('p') if key.modifiers.contains(Mods::CONTROL) => Key::Up,
        code => code,
    }
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
        if stage.harness != self.stage.harness {
            stage.harness_settings.clear();
        }
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
        if key.code == Key::Tab
            || (key.code == Key::Char('n') && key.modifiers.contains(Mods::CONTROL))
        {
            self.selected = (self.selected + 1) % self.fields.len();
        } else if key.code == Key::BackTab
            || (key.code == Key::Char('p') && key.modifiers.contains(Mods::CONTROL))
        {
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
    theme: Theme,
    theme_path: PathBuf,
    settings: Settings,
    workflow: usize,
    selected: usize,
    task: String,
    worktree: bool,
    dirty: bool,
    page: usize,
    modal: Option<Modal>,
    message: String,
    /// Last error shown; the status line turns red while it is current.
    alert: String,
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
    model_editor: Option<Box<HarnessEditor>>,
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
    fn displayed_theme(&self) -> Theme {
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
    fn palette(&self) -> Palette {
        self.displayed_theme().palette()
    }
    fn choose_theme(&mut self) {
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
            guide: false,
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
    fn model_picker(&mut self) {
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
                if stage.harness != harness {
                    stage.harness_settings.clear();
                }
                stage.harness = harness;
                stage.model.clear();
                stage.provider.clear();
                stage.effort.clear();
                self.dirty = true;
            }
            PickValue::Model(model) => {
                if let Some(mut editor) = self.model_editor.take() {
                    editor.set_model(model);
                    self.modal = Some(Modal::Harness(editor));
                } else {
                    let stage = self.stage_mut();
                    stage.model = model.id;
                    stage.provider = model.provider;
                    self.dirty = true;
                }
            }
            PickValue::Theme(theme) => {
                theme::save(&self.theme_path, theme)?;
                self.theme = theme;
                self.message = format!("Theme saved: {}. T changes it anytime.", theme.name());
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
            self.modal = self.model_editor.take().map(Modal::Harness);
            return Ok(());
        }
        if key.code == Key::F(7) && matches!(modal, Modal::Harness(_)) {
            if let Modal::Harness(editor) = modal {
                self.model_editor = Some(editor);
            }
            self.model_picker();
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
                                "Stage added. [J]/[K] reorders; [Space] enables or disables it."
                                    .into();
                        }
                        self.dirty = true;
                        keep = false;
                    } else {
                        editor.key(key);
                    }
                }
                Modal::Harness(editor) => {
                    if key.modifiers.contains(Mods::CONTROL) && key.code == Key::Char('s') {
                        let stage = editor.finish()?;
                        let mut candidate = self.settings.clone();
                        candidate.workflows[self.workflow].stages[self.selected] = stage;
                        candidate.validate()?;
                        self.settings = candidate;
                        self.dirty = true;
                        self.message = "Harness settings applied to this stage. s saves; P previews the command.".into();
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
                    let matches = matching_choices(items, filter);
                    match menu_key(key) {
                        Key::Up => *selected = selected.saturating_sub(1),
                        Key::Down => {
                            *selected = (*selected + 1).min(matches.len().saturating_sub(1))
                        }
                        Key::Enter if !matches.is_empty() => {
                            let choice = matches[(*selected).min(matches.len() - 1)];
                            self.pick(choice.value.clone())?;
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
        if let Err(error) = &result {
            if let Modal::Harness(editor) = &mut modal {
                editor.error = format!("{error:#}");
            }
            if let Modal::Picker { note, .. } = &mut modal {
                *note = storage::clean_terminal(&format!(
                    "Could not apply selection: {error:#}\nEsc cancels."
                ));
            }
        }
        if keep {
            self.modal = Some(modal);
        }
        if matches!(self.modal, Some(Modal::Harness(_))) {
            // Validation is shown in the draft, so it cannot leave a stale
            // error behind the dialog after the user corrects the field.
            Ok(())
        } else {
            result
        }
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
                        self.text(harness, format!("Executable: {}\n\n{}\n\nSelect this harness with [h] on the workflow page, then press [m] to load model selections. Use [c] on Workflow to configure the selected stage, including permissions and extra arguments.\n\nPermissions inherit the harness configuration unless overridden for a stage. Authentication stays with the harness.\n\nCustom adapters are defined in TOML. See README.md for the template contract.", adapters::program(harness, &self.settings.adapters), if harness == "amp" { "Amp uses mode keys. A mode determines its model and tools." } else { "A model must be supported by the selected harness/provider." }));
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
                "Task · [Ctrl+S] applies · [Enter] adds a line",
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
                    note: "Changing harness clears model/provider/effort and native settings; extra arguments are kept."
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
            Key::Char('c') => {
                self.modal = Some(Modal::Harness(Box::new(HarnessEditor::new(
                    self.stage().clone(),
                ))))
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
                        "Choose, edit, then [Ctrl+S] to insert after the selected stage. [Esc] cancels."
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
            self.text("Handoff · [↑↓] scroll · [Esc] closes", body);
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

/// Lines needed to show `line` wrapped at `width`, capped at `max`.
fn rows_for(line: &Line<'_>, width: u16, max: u16) -> u16 {
    (line.width() as u16).div_ceil(width.max(1)).clamp(1, max)
}
fn elapsed(seconds: i64) -> String {
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
    }
}
/// Home-relative path, trimmed from the left to fit `width` columns.
fn short_path(path: &std::path::Path, width: usize) -> String {
    let mut text = path.display().to_string();
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        if let Ok(rest) = path.strip_prefix(&home) {
            text = format!("~/{}", rest.display());
        }
    }
    let text = storage::clean_terminal(&text);
    let count = text.chars().count();
    if count <= width {
        return text;
    }
    let keep = width.saturating_sub(1);
    format!("…{}", text.chars().skip(count - keep).collect::<String>())
}
fn kv(palette: Palette, label: &str, value: &str, set: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<11}"), Style::default().fg(palette.muted)),
        Span::styled(
            value.to_string(),
            Style::default().fg(if set { palette.fg } else { palette.muted }),
        ),
    ])
}
fn or_default<'a>(value: &'a str, fallback: &'a str) -> (&'a str, bool) {
    if value.is_empty() {
        (fallback, false)
    } else {
        (value, true)
    }
}
/// Centered headline plus muted guidance inside a titled panel.
fn empty_state(
    frame: &mut Frame<'_>,
    palette: Palette,
    area: Rect,
    block: Block<'static>,
    head: &str,
    body: &str,
) {
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let text = Text::from(vec![
        Line::styled(
            head.to_string(),
            Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::styled(body.to_string(), Style::default().fg(palette.muted)),
    ]);
    let top = inner.height.saturating_sub(4) / 2;
    let rect = Rect::new(inner.x, inner.y + top, inner.width, inner.height - top);
    frame.render_widget(
        Paragraph::new(text).centered().wrap(Wrap { trim: true }),
        rect,
    );
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
/// Fade everything already drawn so a modal reads as the foreground.
fn dim(frame: &mut Frame<'_>, palette: Palette) {
    for cell in &mut frame.buffer_mut().content {
        cell.set_style(
            Style::default()
                .fg(palette.line)
                .bg(palette.bg)
                .remove_modifier(Modifier::all()),
        );
    }
}

const TABS: [&str; 4] = ["Workflow", "Monitor", "Runs", "Harnesses"];

fn draw(frame: &mut Frame<'_>, app: &mut App) {
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

fn draw_workflow(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(5),
        Constraint::Min(5),
    ])
    .split(area);
    let workflow = &app.settings.workflows[app.workflow];
    let enabled = workflow.stages.iter().filter(|s| s.enabled).count();
    let mut heading = vec![Span::styled(
        workflow.name.clone(),
        Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
    )];
    if app.dirty {
        heading.push(Span::styled(
            "  ● unsaved",
            Style::default().fg(palette.warn),
        ));
    }
    heading.push(Span::styled(
        format!(
            "   {enabled}/{} stages  ·  {}  ·  repairs ≤ {}",
            workflow.stages.len(),
            if app.worktree {
                "isolated worktree"
            } else {
                "current directory"
            },
            workflow.max_repair_passes
        ),
        Style::default().fg(palette.muted),
    ));
    let heading = Line::from(heading);
    let actions = palette.hints(&[
        ("w", "switch"),
        ("R", "rename"),
        ("g", "worktree"),
        ("l", "repairs"),
    ]);
    let actions_width = actions.width() as u16;
    if heading.width() as u16 + actions_width + 2 <= area.width {
        frame.render_widget(Paragraph::new(actions).right_aligned(), chunks[0]);
    }
    frame.render_widget(Paragraph::new(heading), chunks[0]);
    let task = if app.task.is_empty() {
        Paragraph::new(Line::styled(
            "No task yet. Press [t] to write one, or [f] to import a file, URL, or GitHub issue.",
            Style::default()
                .fg(palette.muted)
                .add_modifier(Modifier::ITALIC),
        ))
    } else {
        Paragraph::new(storage::clean_terminal(&app.task)).style(Style::default().fg(palette.fg))
    };
    frame.render_widget(
        task.wrap(Wrap { trim: false })
            .block(palette.panel_with("Task", &[("t", "edit"), ("f", "import")])),
        chunks[2],
    );
    let cols = if area.width >= 100 {
        Layout::horizontal([Constraint::Percentage(66), Constraint::Percentage(34)])
            .split(chunks[3])
    } else {
        Layout::horizontal([Constraint::Percentage(100), Constraint::Length(0)]).split(chunks[3])
    };
    let rows = workflow.stages.iter().enumerate().map(|(index, stage)| {
        let on = stage.enabled;
        let interactive = stage.mode == Mode::Interactive;
        Row::new(vec![
            Cell::from(if on { "●" } else { "○" }).style(Style::default().fg(if on {
                palette.accent
            } else {
                palette.muted
            })),
            Cell::from(format!("{}", index + 1)).style(Style::default().fg(palette.muted)),
            Cell::from(stage.name.clone()),
            Cell::from(stage.harness.clone()),
            if stage.model.is_empty() {
                Cell::from("default").style(Style::default().fg(palette.muted))
            } else {
                Cell::from(stage.model.clone())
            },
            Cell::from(if interactive { "interactive" } else { "auto" }).style(
                Style::default().fg(match (on, interactive) {
                    (false, _) => palette.muted,
                    (true, true) => palette.blue,
                    (true, false) => palette.fg,
                }),
            ),
        ])
        .style(Style::default().fg(if on { palette.fg } else { palette.muted }))
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Fill(4),
            Constraint::Length(9),
            Constraint::Fill(3),
            Constraint::Length(11),
        ],
    )
    .column_spacing(2)
    .header(
        Row::new(["", "#", "STAGE", "HARNESS", "MODEL", "EXECUTION"])
            .style(palette.header_style())
            .bottom_margin(1),
    )
    .block(palette.panel_with(
        "Stages",
        &[
            ("c", "harness"),
            ("e", "edit"),
            ("a", "add"),
            ("J/K", "move"),
        ],
    ))
    .highlight_symbol("▌")
    .highlight_spacing(HighlightSpacing::Always)
    .row_highlight_style(palette.selected_style());
    frame.render_stateful_widget(
        table,
        cols[0],
        &mut TableState::default().with_selected(Some(app.selected)),
    );
    if cols[1].width > 0 {
        draw_stage_detail(frame, app.stage(), cols[1], palette);
    }
}

fn draw_stage_detail(frame: &mut Frame<'_>, stage: &Stage, area: Rect, palette: Palette) {
    let model = or_default(&stage.model, "harness default");
    let provider = or_default(&stage.provider, "harness default");
    let effort = or_default(&stage.effort, "harness default");
    let mut lines = vec![
        Line::styled(
            stage.name.clone(),
            Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
        ),
        Line::from(vec![
            Span::styled(
                format!("{}  ·  ", stage.role.label()),
                Style::default().fg(palette.muted),
            ),
            if stage.enabled {
                Span::styled("● enabled", Style::default().fg(palette.accent))
            } else {
                Span::styled("○ disabled", Style::default().fg(palette.muted))
            },
        ]),
        Line::raw(""),
        kv(palette, "Harness", &stage.harness, true),
        kv(palette, "Model", model.0, model.1),
        kv(palette, "Execution", stage.mode.label(), true),
        kv(palette, "Provider", provider.0, provider.1),
        kv(palette, "Effort", effort.0, effort.1),
        kv(
            palette,
            "Timeout",
            &format!("{} min", stage.timeout_seconds / 60),
            true,
        ),
        kv(palette, "ID", &stage.id, true),
        Line::raw(""),
        Line::styled("Instructions", palette.header_style()),
    ];
    lines.extend(
        storage::clean_terminal(&stage.instructions)
            .lines()
            .map(|line| Line::styled(line.to_string(), Style::default().fg(palette.fg))),
    );
    lines.push(Line::raw(""));
    lines.push(Line::styled("Harness settings", palette.header_style()));
    lines.extend(
        harness_settings::summary(stage)
            .lines()
            .map(|line| Line::styled(line.to_string(), Style::default().fg(palette.fg))),
    );
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        if stage.role == Role::Review {
            "Optional. [Space] toggles it; blocking findings request repairs."
        } else {
            "c configures this stage's harness."
        },
        Style::default()
            .fg(palette.muted)
            .add_modifier(Modifier::ITALIC),
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(palette.panel(" Selected stage ")),
        area,
    );
}

fn draw_run(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) {
    let Some(run) = &app.run else {
        empty_state(
            frame,
            palette,
            area,
            palette.panel(" Run monitor "),
            "No run selected",
            "Start one from Workflow [r], or open a saved run from Runs [3].",
        );
        return;
    };
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length((run.stages.len() as u16 + 3).min(10)),
        Constraint::Min(3),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                run.workflow.name.clone(),
                Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            palette.status_span(run.status),
            Span::styled(
                format!(
                    "   {}  ·  repairs {}/{}",
                    run.id, run.repair_passes, run.workflow.max_repair_passes
                ),
                Style::default().fg(palette.muted),
            ),
        ])),
        chunks[0],
    );
    let name_width = run
        .workflow
        .stages
        .iter()
        .map(|s| s.name.chars().count() as u16)
        .max()
        .unwrap_or(0)
        .clamp(16, 40);
    let rows = run.stages.iter().enumerate().map(|(i, state)| {
        let seconds = state.attempts.last().map(|a| {
            (a.finished_at.unwrap_or_else(chrono::Utc::now) - a.started_at)
                .num_seconds()
                .max(0)
        });
        Row::new([
            Cell::from(run.workflow.stages[i].name.clone()),
            Cell::from(palette.status_span(state.status)),
            Cell::from(state.attempts.len().to_string()).style(Style::default().fg(palette.muted)),
            Cell::from(seconds.map(elapsed).unwrap_or_else(|| "–".into()))
                .style(Style::default().fg(palette.muted)),
        ])
    });
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(name_width),
                Constraint::Length(18),
                Constraint::Length(8),
                Constraint::Fill(1),
            ],
        )
        .column_spacing(3)
        .header(Row::new(["STAGE", "STATUS", "ATTEMPTS", "ELAPSED"]).style(palette.header_style()))
        .block(palette.panel(" Progress "))
        .highlight_symbol("▌")
        .highlight_spacing(HighlightSpacing::Always)
        .row_highlight_style(palette.selected_style()),
        chunks[2],
        &mut TableState::default().with_selected(Some(run.cursor.min(run.stages.len() - 1))),
    );
    let cols = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(chunks[3]);
    let visible = cols[0].height.saturating_sub(2) as usize;
    let end = app.logs.len().saturating_sub(app.log_scroll);
    let start = end.saturating_sub(visible);
    let output_block = palette.panel_with("Output", &[("PgUp/PgDn", "scroll")]);
    if app.logs.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "Harness output appears here. Some harnesses emit their final response only when the stage finishes. Full logs are saved on disk.",
                Style::default().fg(palette.muted).add_modifier(Modifier::ITALIC),
            ))
            .wrap(Wrap { trim: false })
            .block(output_block),
            cols[0],
        );
    } else {
        frame.render_widget(
            Paragraph::new(
                app.logs
                    .iter()
                    .skip(start)
                    .take(end - start)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .block(output_block)
            .style(Style::default().fg(palette.muted)),
            cols[0],
        );
    }
    let handoff = run
        .stages
        .get(run.cursor.min(run.stages.len() - 1))
        .and_then(|s| s.handoff.as_ref());
    let summary: Vec<Line> = if let Some(error) = &run.error {
        vec![
            Line::styled(
                "Error",
                Style::default()
                    .fg(palette.red)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            Line::styled(
                storage::clean_terminal(error),
                Style::default().fg(palette.red),
            ),
        ]
    } else if let Some(h) = handoff {
        let mut lines = vec![
            Line::styled(
                if h.structured {
                    "Structured handoff"
                } else {
                    "Text handoff · inspect before advancing"
                },
                palette.header_style(),
            ),
            Line::raw(""),
        ];
        lines.extend(
            storage::clean_terminal(&h.summary)
                .lines()
                .map(|line| Line::raw(line.to_string())),
        );
        lines
    } else {
        vec![
            Line::styled(
                run.current_name().to_string(),
                Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            Line::styled("Workspace", palette.header_style()),
            Line::raw(short_path(&run.workspace, usize::MAX)),
            Line::raw(""),
            Line::styled("Artifacts", palette.header_style()),
            Line::raw(short_path(&run.directory, usize::MAX)),
        ]
    };
    frame.render_widget(
        Paragraph::new(summary)
            .wrap(Wrap { trim: false })
            .block(palette.panel_with("Handoff", &[("h", "inspect")])),
        cols[1],
    );
}

fn draw_history(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) {
    if app.history.is_empty() {
        empty_state(
            frame,
            palette,
            area,
            palette.panel(" Saved runs "),
            "No saved runs yet",
            "Runs for this project are saved automatically. Start a workflow from Workflow [r].",
        );
        return;
    }
    let rows = app.history.iter().map(|run| {
        Row::new([
            Cell::from(run.created_at.format("%b %d  %H:%M").to_string())
                .style(Style::default().fg(palette.muted)),
            Cell::from(palette.status_span(run.status)),
            Cell::from(run.workflow.name.clone()),
            Cell::from(storage::clean_terminal(&run.task.replace('\n', " ")))
                .style(Style::default().fg(palette.muted)),
        ])
    });
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(14),
                Constraint::Length(18),
                Constraint::Fill(1),
                Constraint::Fill(2),
            ],
        )
        .header(
            Row::new(["STARTED", "STATUS", "WORKFLOW", "TASK"])
                .style(palette.header_style())
                .bottom_margin(1),
        )
        .block(palette.panel_with("Saved runs", &[("Enter", "open in monitor")]))
        .highlight_symbol("▌")
        .highlight_spacing(HighlightSpacing::Always)
        .row_highlight_style(palette.selected_style()),
        area,
        &mut TableState::default()
            .with_selected(Some(app.history_selected.min(app.history.len() - 1))),
    );
}
fn draw_harnesses(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) {
    let rows = catalog::known_harnesses(&app.settings)
        .into_iter()
        .map(|name| {
            let program = adapters::program(&name, &app.settings.adapters);
            let path = adapters::executable(&program);
            let found = path.is_some();
            Row::new([
                Cell::from(name),
                Cell::from(if found {
                    "● installed"
                } else {
                    "○ not found"
                })
                .style(Style::default().fg(if found {
                    palette.accent
                } else {
                    palette.muted
                })),
                Cell::from(path.map(|p| short_path(&p, usize::MAX)).unwrap_or(program))
                    .style(Style::default().fg(palette.muted)),
            ])
        });
    let block = palette.panel(" Harnesses ").title(
        Line::styled(
            " uses existing logins and provider settings ",
            Style::default().fg(palette.muted),
        )
        .right_aligned(),
    );
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(12),
                Constraint::Length(13),
                Constraint::Min(20),
            ],
        )
        .header(
            Row::new(["HARNESS", "STATUS", "PATH / COMMAND"])
                .style(palette.header_style())
                .bottom_margin(1),
        )
        .block(block)
        .highlight_symbol("▌")
        .highlight_spacing(HighlightSpacing::Always)
        .row_highlight_style(palette.selected_style()),
        area,
        &mut TableState::default().with_selected(Some(app.harness_selected)),
    );
}

fn draw_modal(frame: &mut Frame<'_>, modal: &mut Modal, palette: Palette) {
    let (width, height) = match modal {
        Modal::Stage { .. } => (100, 35),
        Modal::Loading(_) => (64, 7),
        _ => (95, 26),
    };
    let rect = popup(frame.area(), width, height);
    frame.render_widget(Clear, rect);
    match modal {
        Modal::Repositories(picker) => picker.draw(frame, rect, palette),
        Modal::Import(importer) => importer.draw(frame, rect, palette),
        Modal::Harness(editor) => editor.draw(frame, rect, palette),
        Modal::Input { title, text, .. } => {
            palette.style_textarea(text);
            let block = palette.modal(title, &[]);
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).split(inner);
            frame.render_widget(&*text, chunks[0]);
            let mut keys = palette.hints(&[("Ctrl+S", "apply"), ("Esc", "cancel")]);
            keys.spans.push(Span::styled(
                "   ·  standard text selection and paste",
                Style::default().fg(palette.muted),
            ));
            frame.render_widget(
                Paragraph::new(vec![Line::raw(""), keys]).wrap(Wrap { trim: false }),
                chunks[1],
            );
        }
        Modal::Stage { editor, .. } => {
            let block = palette.modal(
                "Edit stage",
                &[("Tab", "next field"), ("←/→", "cycle choice")],
            );
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let chunks = Layout::vertical([Constraint::Min(8), Constraint::Length(2)]).split(inner);
            let halves =
                Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
                    .split(chunks[0]);
            let value_width = halves[0].width.saturating_sub(4) as usize;
            let rows = FIELDS.iter().enumerate().map(|(i, label)| {
                let value = content(&editor.fields[i]).replace('\n', " ");
                let choice = [1, 2, 6].contains(&i);
                ListItem::new(vec![
                    Line::styled(*label, Style::default().fg(palette.muted)),
                    if value.is_empty() {
                        Line::styled("—", Style::default().fg(palette.line))
                    } else if choice {
                        Line::styled(
                            format!(
                                "‹ {} ›",
                                storage::truncate(&value, value_width.saturating_sub(4))
                            ),
                            Style::default().fg(palette.fg),
                        )
                    } else {
                        Line::styled(
                            storage::truncate(&value, value_width),
                            Style::default().fg(palette.fg),
                        )
                    },
                ])
            });
            frame.render_stateful_widget(
                List::new(rows)
                    .highlight_style(Style::default().bg(palette.selection_bg))
                    .highlight_symbol("▌ ")
                    .highlight_spacing(HighlightSpacing::Always)
                    .block(Block::default().padding(Padding::right(2))),
                halves[0],
                &mut ListState::default().with_selected(Some(editor.selected)),
            );
            let field = &mut editor.fields[editor.selected];
            palette.style_textarea(field);
            field.set_block(
                palette
                    .panel(format!(" {} ", FIELDS[editor.selected]))
                    .border_style(Style::default().fg(palette.accent)),
            );
            frame.render_widget(&*field, halves[1]);
            let mut keys = palette.hints(&[("Ctrl+S", "apply stage"), ("Esc", "cancel")]);
            keys.spans.push(Span::styled(
                "   ·  model IDs are open-ended; blank inherits the default",
                Style::default().fg(palette.muted),
            ));
            frame.render_widget(
                Paragraph::new(vec![Line::raw(""), keys]).wrap(Wrap { trim: false }),
                chunks[1],
            );
        }
        Modal::Picker {
            title,
            items,
            selected,
            filter,
            note,
        } => {
            let block = palette.modal(title, &[]);
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let choices = matching_choices(items, filter);
            *selected = (*selected).min(choices.len().saturating_sub(1));
            let detail = choices.get(*selected).and_then(|c| c.value.detail());
            let chunks = Layout::vertical([
                Constraint::Length(3),
                Constraint::Min(3),
                Constraint::Length(if detail.is_some() { 7 } else { 3 }),
                Constraint::Length(2),
            ])
            .split(inner);
            palette.style_textarea(filter);
            filter.set_block(palette.panel(" Filter "));
            frame.render_widget(&*filter, chunks[0]);
            if choices.is_empty() {
                frame.render_widget(
                    Paragraph::new("No matches. Clear the filter to see every option.")
                        .style(Style::default().fg(palette.muted)),
                    chunks[1].inner(Margin::new(2, 1)),
                );
            }
            frame.render_stateful_widget(
                List::new(choices.iter().map(|c| ListItem::new(c.label.clone())))
                    .highlight_style(palette.selected_style())
                    .highlight_symbol("▌ ")
                    .highlight_spacing(HighlightSpacing::Always)
                    .block(Block::default().padding(Padding::vertical(1))),
                chunks[1],
                &mut ListState::default().with_selected(Some(*selected)),
            );
            let mut lines = vec![];
            if let Some(text) = detail {
                let mut parts = text.lines();
                if let Some(first) = parts.next() {
                    lines.push(Line::styled(
                        first.to_string(),
                        Style::default().fg(palette.fg),
                    ));
                }
                lines.extend(
                    parts.map(|l| Line::styled(l.to_string(), Style::default().fg(palette.muted))),
                );
                lines.push(Line::raw(""));
            }
            lines.extend(
                note.lines()
                    .map(|l| Line::styled(l.to_string(), Style::default().fg(palette.muted))),
            );
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                    Block::default()
                        .borders(Borders::TOP)
                        .border_style(Style::default().fg(palette.line)),
                ),
                chunks[2],
            );
            frame.render_widget(
                Paragraph::new(vec![
                    Line::raw(""),
                    palette.hints(&[
                        ("↑↓ / Ctrl+N/P", "select"),
                        ("Enter", "choose"),
                        ("Esc", "cancel"),
                    ]),
                ]),
                chunks[3],
            );
        }
        Modal::Text {
            title,
            body,
            scroll,
            guide,
        } => {
            let body = if *guide {
                guide_text(body, palette)
            } else {
                Text::raw(storage::clean_terminal(body))
            };
            frame.render_widget(
                Paragraph::new(body)
                    .wrap(Wrap { trim: false })
                    .scroll((*scroll, 0))
                    .block(palette.modal(title, &[("↑↓", "scroll"), ("Esc", "close")])),
                rect,
            );
        }
        Modal::Loading(message) => {
            const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let tick = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() / 80)
                .unwrap_or(0) as usize;
            let block = palette.modal("Model discovery", &[("Esc", "cancel")]);
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::raw(""),
                    Line::from(vec![
                        Span::styled(
                            format!("{} ", SPIN[tick % SPIN.len()]),
                            Style::default().fg(palette.accent),
                        ),
                        Span::raw(message.clone()),
                    ]),
                    Line::styled(
                        "No inference is performed.",
                        Style::default().fg(palette.muted),
                    ),
                ])
                .wrap(Wrap { trim: false }),
                inner,
            );
        }
    }
}

/// Render the keyboard guide: CAPS lines are headings, `key\tdescription`
/// lines become aligned bindings, everything else is a muted note.
fn guide_text(body: &str, palette: Palette) -> Text<'static> {
    body.lines()
        .map(|line| match line.split_once('\t') {
            Some((key, text)) => Line::from(vec![
                Span::styled(
                    format!(
                        "  {:<20}",
                        if key.is_empty() {
                            String::new()
                        } else {
                            theme::key_label(key)
                        }
                    ),
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(text.to_string(), Style::default().fg(palette.fg)),
            ]),
            None if !line.is_empty() && line == line.to_uppercase() => Line::styled(
                line.to_string(),
                Style::default()
                    .fg(palette.muted)
                    .add_modifier(Modifier::BOLD),
            ),
            None => Line::styled(line.to_string(), Style::default().fg(palette.muted)),
        })
        .collect::<Vec<_>>()
        .into()
}

const HELP: &str = "WORKFLOW
t\tEdit task ([Ctrl+S] applies; [Enter] adds a line)
f\tImport task/spec: browse files; [Ctrl+U] opens URL import
\tPreview and edit; [Tab] append/replace; [Ctrl+S] apply; [Esc] back
p\tBrowse repositories (type to filter; [Tab] for recent)
\t[Enter] selects a repo or opens a folder; [→] browses inside a repo
\t[←] goes to parent; [/], [~] or [Ctrl+L] enters a path; [Esc] cancels
w\tSelect/clone workflow or add a preset
R\tRename workflow
X\tDelete workflow
e / Enter\tEdit selected stage
c\tConfigure selected harness (model, effort, execution, permissions)
\t[Tab] or [Ctrl+N/P] fields; [←/→] choices; [F7] models
\t[Ctrl+S] applies; [Esc] cancels; [s] on Workflow saves
h\tPick harness
m\tDiscover models
Space\tEnable/disable stage, including optional review
v\tToggle unattended/interactive
a\tAdd stage preset (edit, then [Ctrl+S] inserts; [Esc] cancels)
d\tDuplicate stage
x\tRemove stage
J / K\tMove stage down/up
g\tToggle isolated Git worktree
l\tSet automatic repair limit
P\tPreview commands (no model calls)
s\tSave project
S\tSave global templates
r\tStart workflow

MONITOR
c\tCancel active stage; continue when idle
a\tAccept interactive handoff and advance
y\tExplicitly retry current stage
i\tRetry current stage interactively
m\tReplace current model and retry
h\tInspect handoff/error
PgUp / PgDn\tScroll output
n\tReturn to workflow editor

INTERACTIVE HARNESS
\tNative terminal keys and paste are forwarded.
\tExit the harness to inspect its handoff.
Ctrl+]\tStop a stuck stage

SELECTION MENUS
Ctrl+N / Ctrl+P\tNext/previous option (same as [↓]/[↑])

THEMES
T\tChoose theme (type to filter; [↑↓] or [Ctrl+N/P] preview)
Enter\tSave for all projects
Esc\tRestore the current theme

EVERYWHERE
1–4 / Tab\tSwitch views
?\tKeyboard guide
q\tSave workflow edits and exit (stops active work first)
Ctrl+C\tCancel work or exit

NOTES
Saved runs snapshot their workflow. Template edits affect new runs.
Retries retain filesystem changes. Disabled reviews never gate a run.
Artifacts: <project>/.model-router/runs/
Provider credentials stay with each harness. Catalogs do not prove universal access.";

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
    let mut app = App::new(project, settings, config_path, cancel);
    match theme::load(&app.theme_path) {
        Ok(theme) => app.theme = theme,
        Err(error) => {
            app.message = format!("{error:#}. Using Model Router colours; T chooses a theme.");
        }
    }
    let mut screen = Screen::new()?;
    if animate && !startup::play(&mut screen.terminal, &app.signal, app.palette())? {
        return Ok(());
    }
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
                Err(e) => {
                    app.alert = format!("{e:#}");
                    app.alert.clone()
                }
            };
            if let Some(run) = &app.run {
                if let Some(error) = &run.error {
                    app.message = error.clone();
                    app.alert = error.clone();
                }
            }
        }
        screen.terminal.draw(|frame| draw(frame, &mut app))?;
        if event::poll(Duration::from_millis(70))? {
            match event::read()? {
                Event::Key(key) => match app.key(key) {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(error) => {
                        app.message = format!("{error:#}");
                        app.alert = app.message.clone();
                    }
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
                    Some(Modal::Harness(editor)) => editor.paste(&text),
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

    fn theme_app(project: &std::path::Path) -> App {
        let mut app = App::new(
            project.into(),
            Settings::default(),
            project.join("config.toml"),
            Arc::new(AtomicBool::new(false)),
        );
        app.theme_path = project.join("appearance.json");
        app
    }

    #[test]
    fn theme_preview_is_cancelable_and_only_enter_persists() {
        let project = tempfile::tempdir().unwrap();
        let mut app = theme_app(project.path());
        app.task = "Keep my task".into();
        let settings = toml::to_string(&app.settings).unwrap();
        app.key(key(Key::Char('T'))).unwrap();
        app.key(KeyEvent::new(Key::Char('n'), Mods::CONTROL))
            .unwrap();
        assert_eq!(app.displayed_theme(), Theme::CatppuccinMocha);
        assert_eq!(app.theme, Theme::ModelRouter);
        assert!(!app.theme_path.exists());
        app.key(KeyEvent::new(Key::Char('p'), Mods::CONTROL))
            .unwrap();
        assert_eq!(app.displayed_theme(), Theme::ModelRouter);
        app.key(key(Key::Down)).unwrap();
        app.key(key(Key::Esc)).unwrap();
        assert_eq!(app.displayed_theme(), Theme::ModelRouter);
        assert!(!app.theme_path.exists());

        app.key(key(Key::Char('T'))).unwrap();
        for c in "latte".chars() {
            app.key(key(Key::Char(c))).unwrap();
        }
        assert_eq!(app.displayed_theme(), Theme::CatppuccinLatte);
        app.key(key(Key::Enter)).unwrap();
        assert!(app.modal.is_none());
        assert_eq!(app.theme, Theme::CatppuccinLatte);
        assert_eq!(
            theme::load(&app.theme_path).unwrap(),
            Theme::CatppuccinLatte
        );

        app.key(key(Key::Char('T'))).unwrap();
        for c in "no-such-theme".chars() {
            app.key(key(Key::Char(c))).unwrap();
        }
        app.key(KeyEvent::new(Key::Char('n'), Mods::CONTROL))
            .unwrap();
        app.key(key(Key::Enter)).unwrap();
        assert!(
            app.modal.is_some(),
            "An empty search must not choose a theme"
        );
        app.key(key(Key::Esc)).unwrap();
        assert_eq!(app.displayed_theme(), Theme::CatppuccinLatte);
        assert_eq!(
            theme::load(&app.theme_path).unwrap(),
            Theme::CatppuccinLatte
        );
        assert!(!app.dirty);
        assert_eq!(app.task, "Keep my task");
        assert_eq!(toml::to_string(&app.settings).unwrap(), settings);
    }

    #[test]
    fn failed_theme_save_preserves_committed_theme_and_allows_cancel() {
        let project = tempfile::tempdir().unwrap();
        let mut app = theme_app(project.path());
        let obstruction = project.path().join("not-a-directory");
        std::fs::write(&obstruction, "keep this file").unwrap();
        app.theme_path = obstruction.join("appearance.json");
        app.choose_theme();
        for c in "dracula".chars() {
            app.key(key(Key::Char(c))).unwrap();
        }
        assert!(app.key(key(Key::Enter)).is_err());
        assert!(
            matches!(&app.modal, Some(Modal::Picker { note, .. }) if note.contains("Could not apply selection"))
        );
        assert_eq!(app.theme, Theme::ModelRouter);
        app.key(key(Key::Esc)).unwrap();
        assert_eq!(app.displayed_theme(), Theme::ModelRouter);
        assert_eq!(
            std::fs::read_to_string(obstruction).unwrap(),
            "keep this file"
        );
        assert!(!app.dirty);
    }

    #[test]
    fn every_theme_reaches_pages_dialogs_and_text_inputs() {
        let project = tempfile::tempdir().unwrap();
        let mut app = theme_app(project.path());
        app.task = "Preserve editor content".into();
        for theme in Theme::ALL {
            app.theme = theme;
            let palette = theme.palette();
            for (width, height) in [(120, 36), (80, 24)] {
                let mut terminal =
                    Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
                for action in [
                    '1', '2', '3', '4', '1', 't', 'e', 'c', 'p', 'f', 'w', '?', 'T',
                ] {
                    app.modal = None;
                    app.key(key(Key::Char(action))).unwrap();
                    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
                    let buffer = terminal.backend().buffer();
                    assert_eq!(buffer[(0, 0)].bg, palette.bg, "{theme:?} {action}");
                    assert!(
                        buffer.content.iter().all(|cell| {
                            [
                                palette.bg,
                                palette.panel,
                                palette.selection_bg,
                                palette.accent,
                            ]
                            .contains(&cell.bg)
                        }),
                        "Unstyled background in {theme:?}, view {action}"
                    );
                }
                app.modal = None;
            }
        }
        assert_eq!(app.task, "Preserve editor content");
        assert!(!app.theme_path.exists());
        assert!(!app.dirty);
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

    #[test]
    fn harness_editor_applies_only_to_selected_stage_and_supports_cancel() {
        let project = tempfile::tempdir().unwrap();
        let mut app = theme_app(project.path());
        let original = serde_json::to_value(&app.settings).unwrap();
        app.key(key(Key::Char('c'))).unwrap();
        for _ in 0..5 {
            app.key(KeyEvent::new(Key::Char('n'), Mods::CONTROL))
                .unwrap();
        }
        app.key(key(Key::Right)).unwrap();
        app.key(key(Key::Esc)).unwrap();
        assert_eq!(serde_json::to_value(&app.settings).unwrap(), original);
        assert!(!app.dirty);

        app.key(key(Key::Char('c'))).unwrap();
        for _ in 0..5 {
            app.key(KeyEvent::new(Key::Char('n'), Mods::CONTROL))
                .unwrap();
        }
        app.key(key(Key::Right)).unwrap();
        app.key(key(Key::Right)).unwrap();
        app.key(KeyEvent::new(Key::Char('n'), Mods::CONTROL))
            .unwrap();
        app.key(key(Key::Right)).unwrap();
        app.key(KeyEvent::new(Key::Char('s'), Mods::CONTROL))
            .unwrap();
        assert!(app.modal.is_none());
        assert!(app.dirty);
        assert_eq!(app.stage().harness_settings["sandbox"], "workspace-write");
        assert_eq!(
            app.stage().harness_settings["approval_policy"],
            "on-request"
        );
        for i in 1..3 {
            assert_eq!(
                serde_json::to_value(&app.settings.workflows[0].stages[i]).unwrap(),
                original["workflows"][0]["stages"][i]
            );
        }
        app.key(key(Key::Char('s'))).unwrap();
        assert!(!app.dirty);
        let (saved, _) = config::load(project.path(), Some(&app.config_path)).unwrap();
        assert_eq!(
            saved.workflows[0].stages[0].harness_settings,
            app.stage().harness_settings
        );
        let saved_settings = serde_json::to_value(&app.settings).unwrap();
        app.key(key(Key::Char('c'))).unwrap();
        for _ in 0..4 {
            app.key(KeyEvent::new(Key::Char('n'), Mods::CONTROL))
                .unwrap();
        }
        app.key(KeyEvent::new(Key::Char('a'), Mods::CONTROL))
            .unwrap();
        app.key(KeyEvent::new(Key::Char('k'), Mods::CONTROL))
            .unwrap();
        app.key(key(Key::Char('0'))).unwrap();
        app.key(KeyEvent::new(Key::Char('s'), Mods::CONTROL))
            .unwrap();
        assert!(
            matches!(&app.modal, Some(Modal::Harness(e)) if e.error.contains("greater than zero"))
        );
        assert_eq!(serde_json::to_value(&app.settings).unwrap(), saved_settings);
        app.key(key(Key::Esc)).unwrap();

        // Both ways of switching harness discard incompatible structured options.
        let mut editor =
            StageEditor::new(app.stage().clone(), vec!["codex".into(), "claude".into()]);
        editor.fields[2] = textarea("claude");
        assert!(editor.finish().unwrap().harness_settings.is_empty());
        app.pick(PickValue::Harness("claude".into())).unwrap();
        assert!(app.stage().harness_settings.is_empty());
    }

    #[test]
    fn harness_model_catalog_restores_draft_on_cancel_error_and_selection() {
        let project = tempfile::tempdir().unwrap();
        let mut app = theme_app(project.path());
        let original = serde_json::to_value(&app.settings).unwrap();
        app.key(key(Key::Char('c'))).unwrap();
        app.key(KeyEvent::new(Key::Char('a'), Mods::CONTROL))
            .unwrap();
        app.key(KeyEvent::new(Key::Char('k'), Mods::CONTROL))
            .unwrap();
        for c in "draft-model".chars() {
            app.key(key(Key::Char(c))).unwrap();
        }
        app.key(key(Key::F(7))).unwrap();
        assert!(app.model_editor.is_some());
        app.key(key(Key::Esc)).unwrap();
        assert!(
            matches!(&app.modal, Some(Modal::Harness(e)) if e.finish().unwrap().model == "draft-model")
        );

        app.key(key(Key::F(7))).unwrap();
        let (sender, receiver) = mpsc::channel();
        sender.send(Err("Offline catalog".into())).unwrap();
        app.catalog = Some(receiver);
        app.poll().unwrap();
        assert!(
            matches!(&app.modal, Some(Modal::Harness(e)) if e.error.contains("Offline catalog") && e.finish().unwrap().model == "draft-model")
        );

        app.key(key(Key::F(7))).unwrap();
        let (sender, receiver) = mpsc::channel();
        sender
            .send(Ok(Catalog {
                note: "Offline fixture".into(),
                models: vec![Model {
                    id: "selected-model".into(),
                    provider: "provider".into(),
                    label: "Model".into(),
                    source: "test".into(),
                    availability: "local".into(),
                }],
            }))
            .unwrap();
        app.catalog = Some(receiver);
        app.poll().unwrap();
        app.key(key(Key::Enter)).unwrap();
        assert!(
            matches!(&app.modal, Some(Modal::Harness(e)) if e.finish().unwrap().model == "selected-model")
        );
        assert_eq!(serde_json::to_value(&app.settings).unwrap(), original);
        app.key(KeyEvent::new(Key::Char('s'), Mods::CONTROL))
            .unwrap();
        assert_eq!(app.stage().model, "selected-model");
        assert_eq!(app.stage().provider, "provider");
    }
}
