use super::input::InputField;
use super::{
    actions::{self, ActionId, Group, Scope},
    app::App,
    diff_view::{DiffAction, DiffView},
    external,
    handoff_view::{HandoffAction, HandoffView},
    harness_editor::HarnessEditor,
    history::{Compare, Confirm},
    path_picker::{PathPicker, PickerAction},
    run_slot::RunSlot,
    session::draw_notifications,
    task_importer::{ImportAction, TaskImporter},
    theme::{self, Palette, Theme},
    widgets::{content, menu_key, popup, textarea},
    workflow_page::{StageEditor, StageRequest},
};
use crate::{
    catalog::{self, Model},
    config::{self, Stage},
    engine, notify, presets, storage,
};
use anyhow::{Context, Result, ensure};
use crossterm::event::{KeyCode as Key, KeyEvent, KeyModifiers as Mods};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Padding, Paragraph,
        Wrap,
    },
};
use std::sync::atomic::Ordering;

#[derive(Clone, Copy)]
pub(super) enum InputTarget {
    Task,
    NewWorkflow,
    RenameWorkflow,
    RetryModel,
    RepairLimit,
    Handoff,
    NotifyUrl,
}
#[derive(Clone)]
pub(super) enum PickValue {
    Workflow(usize),
    NewWorkflow,
    WorkflowPreset(usize),
    StagePreset(usize),
    Harness(String),
    Model(Model),
    Theme(Theme),
    Action(ActionId),
    Compare(Compare),
    /// A stopped fan-out candidate to retry: stage ID and candidate ID.
    Candidate {
        stage: String,
        candidate: String,
    },
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
            Self::Action(id) => actions::entry(*id).map(|entry| {
                std::iter::once(entry.help)
                    .chain(entry.more.iter().copied())
                    .collect::<Vec<_>>()
                    .join("\n")
            }),
            _ => None,
        }
    }
}
pub(super) struct Choice {
    pub(super) label: String,
    pub(super) value: PickValue,
}
pub(super) fn matching_choices<'a>(
    items: &'a [Choice],
    filter: &InputField<'_>,
) -> Vec<&'a Choice> {
    let query = content(filter).to_lowercase();
    items
        .iter()
        .filter(|choice| choice.label.to_lowercase().contains(&query))
        .collect()
}
pub(super) enum Modal {
    Input {
        title: String,
        target: InputTarget,
        text: InputField<'static>,
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
        filter: InputField<'static>,
        note: String,
    },
    Text {
        title: String,
        body: String,
        scroll: u16,
        guide: bool,
    },
    /// Work in the background; Esc closes it and drops the result.
    Loading(Loading),
    /// The results view's diff: files, hunks, search, and external tools.
    Diff(Box<DiffView>),
    /// A stage's handoff, with its changed files.
    Handoff(Box<HandoffView>),
    Confirm(Box<Confirm>),
    /// A yes/no question before something hard to take back.
    Ask(Box<Ask>),
    /// The session's runs; drawn by the app, which holds them.
    SessionRuns {
        selected: usize,
    },
    /// The `Space` menu for the view it opened on: groups, then the chosen
    /// group's actions.
    Menu {
        scope: Scope,
        group: Option<Group>,
    },
    /// Notification backends and a test button.
    Notifications {
        backends: notify::Backends,
        selected: usize,
        status: String,
    },
}

/// Title, question, and key hints of an `Ask` dialog.
pub(super) type AskText = (
    &'static str,
    String,
    &'static [(&'static str, &'static str)],
);
/// What an `Ask` dialog confirms.
pub(super) enum Ask {
    /// Cancel the running stage of this run.
    CancelStage {
        run: String,
    },
    /// Cancel the running stage of any run of this session.
    CancelRun {
        run: String,
    },
    /// Stop this run's unattended stage and open its session.
    TakeOver {
        run: String,
    },
    DeleteWorkflow {
        index: usize,
    },
    /// Unsaved workflow edits before exiting: [y] saves, [n] discards.
    SaveAndExit,
    /// Interactive harnesses still running on exit: [y] leaves them running
    /// in the background, [n] stops them.
    LeaveRunning,
    /// Close the editor below without applying its changes; [Esc] returns to it.
    DiscardDraft,
}
impl Ask {
    pub(super) fn text(&self, app: &App) -> AskText {
        const YES_NO: &[(&str, &str)] = &[("y / Enter", "yes"), ("n / Esc", "no")];
        match self {
            Self::CancelStage { run } => (
                "Cancel stage",
                format!(
                    "Stop the running stage of {run}? Its artifacts are kept; [y] in Monitor retries it."
                ),
                YES_NO,
            ),
            Self::CancelRun { run } => (
                "Cancel run's stage",
                format!(
                    "Stop the running stage of {run}? Its artifacts are kept; [y] in its Monitor retries it."
                ),
                YES_NO,
            ),
            Self::TakeOver { run } => (
                "Take over stage",
                format!(
                    "Stop the unattended stage of {run} and reopen its session in this terminal?"
                ),
                YES_NO,
            ),
            Self::DeleteWorkflow { index } => (
                "Delete workflow",
                format!(
                    "Delete workflow “{}” and its {} stages? [u] undoes until you leave the project.",
                    app.editor
                        .settings
                        .workflows
                        .get(*index)
                        .map_or("", |w| w.name.as_str()),
                    app.editor
                        .settings
                        .workflows
                        .get(*index)
                        .map_or(0, |w| w.stages.len())
                ),
                YES_NO,
            ),
            Self::SaveAndExit => (
                "Save changes?",
                format!(
                    "Workflow edits are not saved to {}.",
                    app.editor.config_path.display()
                ),
                &[
                    ("y / Enter", "save and exit"),
                    ("n", "exit without saving"),
                    ("Esc", "cancel"),
                ],
            ),
            Self::LeaveRunning => {
                let count = app.session.live_terminals();
                (
                    "Leave running?",
                    format!(
                        "{} keep{} running in the background after mr exits. Run mr in this project to reattach.",
                        if count == 1 {
                            "An interactive stage".to_owned()
                        } else {
                            format!("{count} interactive stages")
                        },
                        if count == 1 { "s" } else { "" }
                    ),
                    &[
                        ("y / Enter", "leave running and exit"),
                        ("n", "stop and exit"),
                        ("Esc", "cancel"),
                    ],
                )
            }
            Self::DiscardDraft => (
                "Discard changes?",
                "This editor has changes that are not applied.".into(),
                &[("y / Enter", "discard"), ("n / Esc", "keep editing")],
            ),
        }
    }
}

/// A dialog waiting on background work. Its result applies only while the
/// dialog is still on top with the same ticket, so closing it cancels.
pub(super) struct Loading {
    pub(super) text: String,
    pub(super) ticket: u64,
}
impl Loading {
    pub(super) fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ticket: super::tasks::ticket(),
        }
    }
}

/// What a key does to the dialog that received it.
pub(super) enum DialogResult {
    Stay,
    Close,
    /// Stay, with another dialog above.
    Push(Modal),
    /// Close, leaving another dialog in its place.
    Replace(Modal),
    /// Close, then have the app apply an effect.
    Emit(Effect),
}
/// What the app does once a dialog has closed.
pub(super) enum Effect {
    /// Run a page or global action, as the command menu does.
    Perform(ActionId),
    /// Exit the router.
    Exit,
}

impl Modal {
    pub(super) fn input(title: &str, target: InputTarget, value: &str) -> Self {
        Self::Input {
            title: title.into(),
            target,
            text: textarea(value),
        }
    }
    /// Whether Esc closes the dialog; others use Esc themselves.
    fn closes_on_esc(&self) -> bool {
        !matches!(
            self,
            Self::Repositories(_) | Self::Import(_) | Self::Diff(_)
        )
    }
    /// Whether an editor holds changes Esc would lose.
    fn changed(&self) -> bool {
        match self {
            Self::Stage { editor, .. } => editor.changed(),
            Self::Harness(editor) => editor.changed(),
            _ => false,
        }
    }
    /// Shows a failed key's error in the dialogs that have room for it.
    fn show_error(&mut self, error: &anyhow::Error) {
        match self {
            Self::Harness(editor) => editor.error = format!("{error:#}"),
            Self::Diff(view) => view.note = storage::clean_terminal(&format!("{error:#}")),
            Self::Picker { note, .. } => {
                *note = storage::clean_terminal(&format!(
                    "Could not apply selection: {error:#}\nEsc cancels."
                ))
            }
            _ => {}
        }
    }
    pub(super) fn active_input(&mut self) -> Option<&mut InputField<'static>> {
        match self {
            Self::Input { text, .. } => Some(text),
            Self::Picker { filter, .. } => Some(filter),
            Self::Stage { editor, .. } => editor.active_input(),
            Self::Harness(editor) => editor.active_input(),
            Self::Repositories(picker) => Some(picker.active_input()),
            Self::Import(importer) => importer.active_input(),
            _ => None,
        }
    }
}

impl App {
    /// Applies a picker choice; menu actions arrive as `Effect::Perform` instead.
    pub(super) fn pick(&mut self, choice: PickValue) -> Result<()> {
        match choice {
            PickValue::Workflow(index) => {
                self.editor.workflow = index;
                self.editor.selected = 0;
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
                    .editor
                    .settings
                    .workflows
                    .iter()
                    .any(|w| w.name == workflow.name)
                {
                    workflow.name = format!("{base} ({suffix})");
                    suffix += 1;
                }
                let mut candidate = self.editor.settings.clone();
                candidate.workflows.push(workflow);
                candidate.validate()?;
                self.editor.settings = candidate;
                self.editor.workflow = self.editor.settings.workflows.len() - 1;
                self.editor.selected = 0;
                self.editor.dirty = true;
                self.ui.message =
                    "Workflow preset added. e edits each stage; reviews start disabled.".into();
            }
            PickValue::StagePreset(index) => {
                ensure!(
                    self.editor.settings.workflows[self.editor.workflow]
                        .stages
                        .len()
                        < 64,
                    "A workflow can contain at most 64 stages"
                );
                let mut stage = presets::STAGES[index].build();
                stage.id = format!("{}-{}", stage.id, Stage::default().id);
                let harness = stage.harness.clone();
                self.ui.open(Modal::Stage {
                    editor: Box::new(StageEditor::new(
                        stage,
                        catalog::known_harnesses(&self.editor.settings),
                    )),
                    insert_at: Some(self.editor.selected + 1),
                });
                self.request_catalog(harness);
            }
            PickValue::Harness(harness) => {
                let stage = self.editor.stage_mut();
                if stage.harness != harness {
                    stage.harness_settings.clear();
                }
                stage.harness = harness;
                stage.model.clear();
                stage.provider.clear();
                stage.effort.clear();
                self.editor.dirty = true;
            }
            // The picker sits above the editor it was opened from, if any.
            PickValue::Model(model) => match self.ui.dialog_mut() {
                Some(Modal::Harness(editor)) => editor.set_model(model),
                Some(Modal::Stage { editor, .. }) => editor.set_model(model),
                _ => {
                    let stage = self.editor.stage_mut();
                    stage.model = model.id;
                    stage.provider = model.provider;
                    self.editor.dirty = true;
                }
            },
            PickValue::Theme(theme) => {
                theme::save(
                    &self.ui.theme_path,
                    &theme::Preferences {
                        theme,
                        ..self.ui.preferences()
                    },
                )?;
                self.ui.theme = theme;
                self.ui.message = format!("Theme saved: {}. T changes it anytime.", theme.name());
            }
            PickValue::Action(id) => {
                self.perform(id)?;
            }
            PickValue::Compare(choice) => self.show_comparison(&choice)?,
            PickValue::Candidate { stage, candidate } => {
                self.session
                    .run_mut()
                    .context("No run")?
                    .command(engine::Cmd::RetryCandidate {
                        stage,
                        candidate: candidate.clone(),
                    })?;
                self.start_driver()?;
                self.ui.message = format!("Retrying candidate {candidate}…");
            }
        }
        Ok(())
    }
    fn submit_input(&mut self, target: InputTarget, value: String) -> Result<()> {
        match target {
            InputTarget::Task => {
                config::validate_task(&value)?;
                self.editor.task = value;
            }
            InputTarget::NewWorkflow | InputTarget::RenameWorkflow => {
                let value = value.trim();
                ensure!(!value.is_empty(), "Workflow name is required");
                ensure!(
                    !self
                        .editor
                        .settings
                        .workflows
                        .iter()
                        .any(|w| w.name == value),
                    "That workflow name already exists"
                );
                if matches!(target, InputTarget::NewWorkflow) {
                    let mut copy = self.editor.settings.workflows[self.editor.workflow].clone();
                    copy.name = value.into();
                    self.editor.settings.workflows.push(copy);
                    self.editor.workflow = self.editor.settings.workflows.len() - 1;
                    self.editor.selected = 0;
                } else {
                    self.editor.settings.workflows[self.editor.workflow].name = value.into();
                }
                self.editor.dirty = true;
            }
            InputTarget::RetryModel => {
                self.session
                    .run_mut()
                    .context("No run")?
                    .command(engine::Cmd::Retry {
                        interactive: false,
                        model: Some(value.trim().into()),
                    })?;
                self.start_driver()?;
            }
            InputTarget::RepairLimit => {
                self.editor.settings.workflows[self.editor.workflow].max_repair_passes = value
                    .trim()
                    .parse()
                    .context("Enter a nonnegative integer")?;
                self.editor.dirty = true;
            }
            InputTarget::Handoff => {
                engine::edit_handoff(self.session.run_mut().context("No run")?, value)?
            }
            InputTarget::NotifyUrl => {
                let url = value.trim().to_owned();
                ensure!(
                    url.is_empty() || url.starts_with("https://") || url.starts_with("http://"),
                    "The webhook URL must start with https:// or http://"
                );
                let backends = notify::Backends {
                    webhook: self.ui.notifications.webhook || !url.is_empty(),
                    webhook_url: url,
                    ..self.ui.notifications.clone()
                };
                self.set_notifications(backends.clone())?;
                self.ui.open(Modal::Notifications {
                    backends,
                    selected: 3,
                    status: "Webhook URL saved.".into(),
                });
            }
        }
        Ok(())
    }
    /// Keys for an `Ask` dialog.
    fn ask_key(&mut self, ask: &Ask, key: KeyEvent) -> Result<DialogResult> {
        let yes = matches!(key.code, Key::Char('y') | Key::Enter)
            && key.modifiers - Mods::SHIFT == Mods::NONE;
        let no = key.code == Key::Esc
            || (key.code == Key::Char('n') && key.modifiers - Mods::SHIFT == Mods::NONE);
        if !yes && !no {
            return Ok(DialogResult::Stay);
        }
        let exit = |quit: bool| {
            if quit {
                DialogResult::Emit(Effect::Exit)
            } else {
                DialogResult::Close
            }
        };
        match ask {
            Ask::SaveAndExit if yes => {
                self.save(false)?;
                return Ok(exit(self.quit()));
            }
            Ask::SaveAndExit if key.code == Key::Char('n') => {
                self.editor.dirty = false;
                return Ok(exit(self.quit()));
            }
            Ask::LeaveRunning if yes => {
                self.session.release_terminals();
                return Ok(exit(self.stop_and_quit()));
            }
            Ask::LeaveRunning if key.code == Key::Char('n') => {
                return Ok(exit(self.stop_and_quit()));
            }
            _ if !yes => {}
            Ask::CancelStage { run } => {
                // The stage may have stopped while the question was open.
                if let Some(slot) = self.running(run) {
                    slot.cancel.store(true, Ordering::Relaxed);
                    self.ui.message = "Stopping stage…".into();
                } else {
                    self.ui.message =
                        format!("{run} is not running a stage; nothing was cancelled.");
                }
            }
            Ask::CancelRun { run } => {
                self.ui.message = if self.session.cancel_run(run) {
                    format!("Stopping the stage of {run}…")
                } else {
                    format!("{run} is not running a stage; nothing was cancelled.")
                };
            }
            Ask::TakeOver { run } => {
                if let Some(slot) = self.running(run) {
                    slot.takeover = true;
                    slot.cancel.store(true, Ordering::Relaxed);
                    self.ui.message = "Stopping the unattended stage to open its session…".into();
                } else {
                    self.ui.message =
                        format!("{run}'s stage already stopped; [t] reopens its session.");
                }
            }
            Ask::DeleteWorkflow { index } => {
                let index = *index;
                ensure!(
                    self.editor.settings.workflows.len() > 1
                        && index < self.editor.settings.workflows.len(),
                    "Keep at least one workflow"
                );
                let name = self.editor.settings.workflows[index].name.clone();
                self.editor.remember(format!("workflow “{name}”"));
                self.editor.settings.workflows.remove(index);
                self.editor.workflow = 0;
                self.editor.selected = 0;
                self.editor.dirty = true;
                self.ui.message = format!("Deleted workflow “{name}” — [u] to undo.");
            }
            // The editor under the question goes too.
            Ask::DiscardDraft => {
                self.ui.dialogs.pop();
            }
            Ask::SaveAndExit | Ask::LeaveRunning => {}
        }
        Ok(DialogResult::Close)
    }
    /// The focused run's slot when it is `id` and still has a worker.
    fn running(&mut self, id: &str) -> Option<&mut RunSlot> {
        self.session
            .focused_mut()
            .filter(|slot| slot.worker.is_some() && slot.run.id == id)
    }
    /// Keys for the top dialog; true means exit.
    pub(super) fn modal_key(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(mut dialog) = self.ui.dialogs.pop() else {
            return Ok(false);
        };
        let base = self.ui.dialogs.len();
        let (outcome, error) = match self.dialog_key(&mut dialog, key) {
            Ok(outcome) => (outcome, None),
            Err(error) => {
                dialog.show_error(&error);
                (DialogResult::Stay, Some(error))
            }
        };
        // A dialog that stays keeps its place under any its key opened.
        let restore = |dialogs: &mut Vec<Modal>, dialog| {
            dialogs.insert(base.min(dialogs.len()), dialog);
        };
        let mut exit = false;
        match outcome {
            DialogResult::Stay => restore(&mut self.ui.dialogs, dialog),
            DialogResult::Close => {}
            DialogResult::Push(next) => {
                restore(&mut self.ui.dialogs, dialog);
                self.ui.open(next);
            }
            DialogResult::Replace(next) => restore(&mut self.ui.dialogs, next),
            // The dialog closes before its effect, so a menu action can open
            // its own dialog and its errors reach the status line like a key's.
            DialogResult::Emit(Effect::Perform(id)) => exit = self.perform(id)?,
            DialogResult::Emit(Effect::Exit) => exit = true,
        }
        match error {
            // Validation is shown in the harness draft, so it cannot leave a
            // stale error behind the dialog once the field is corrected.
            Some(_) if matches!(self.ui.dialog(), Some(Modal::Harness(_))) => Ok(exit),
            Some(error) => Err(error),
            None => Ok(exit),
        }
    }
    /// What `key` does to `dialog`, which is off the stack meanwhile.
    fn dialog_key(&mut self, dialog: &mut Modal, key: KeyEvent) -> Result<DialogResult> {
        use DialogResult::{Close, Stay};
        if let Modal::Ask(ask) = dialog {
            return self.ask_key(ask, key);
        }
        // Vim's Normal mode takes keys first, Esc among them.
        if let Some(field) = dialog.active_input() {
            field.set_vim(self.ui.vim_motions);
            if field.vim_key(key) {
                return Ok(Stay);
            }
        }
        let save = key.modifiers.contains(Mods::CONTROL) && key.code == Key::Char('s');
        if key.code == Key::Esc && dialog.closes_on_esc() {
            return Ok(if dialog.changed() {
                DialogResult::Push(Modal::Ask(Box::new(Ask::DiscardDraft)))
            } else {
                Close
            });
        }
        Ok(match dialog {
            Modal::Input { target, text, .. } => {
                if save {
                    self.submit_input(*target, content(text))?;
                    Close
                } else {
                    text.input(key);
                    Stay
                }
            }
            Modal::Stage { editor, insert_at } if save => {
                let stage = editor.finish()?;
                let mut candidate = self.editor.settings.clone();
                let stages = &mut candidate.workflows[self.editor.workflow].stages;
                match insert_at {
                    Some(index) => stages.insert(*index, stage),
                    None => stages[self.editor.selected] = stage,
                }
                candidate.validate()?;
                self.editor.settings = candidate;
                if let Some(index) = insert_at {
                    self.editor.selected = *index;
                    self.ui.message =
                        "Stage added. [J]/[K] reorders; [Space] enables or disables it.".into();
                }
                self.editor.dirty = true;
                Close
            }
            Modal::Stage { editor, .. } => {
                match editor.key(key) {
                    // The picker opens above the editor, which keeps the draft.
                    StageRequest::PickModel => self.model_picker_for(editor.harness()),
                    StageRequest::Catalog => {
                        let harness = editor.harness();
                        match self.catalogs.cached.get(&harness) {
                            Some(catalog) => editor.set_catalog(catalog.clone()),
                            None => self.request_catalog(harness),
                        }
                    }
                    StageRequest::None => {}
                }
                Stay
            }
            Modal::Harness(editor) if save => {
                let stage = editor.finish()?;
                let mut candidate = self.editor.settings.clone();
                candidate.workflows[self.editor.workflow].stages[self.editor.selected] = stage;
                candidate.validate()?;
                self.editor.settings = candidate;
                self.editor.dirty = true;
                self.ui.message =
                    "Harness settings applied to this stage. s saves; P previews the command."
                        .into();
                Close
            }
            Modal::Harness(editor) => {
                if key.code == Key::F(7) {
                    self.model_picker_for(editor.harness().to_owned());
                } else {
                    editor.key(key);
                }
                Stay
            }
            Modal::Repositories(picker) => match picker.key(key) {
                Ok(PickerAction::Close) => Close,
                // Opens in the background above the browser, which shows
                // again if it fails.
                Ok(PickerAction::Select(path)) => {
                    self.begin_switch(path);
                    Stay
                }
                Ok(PickerAction::Stay) => Stay,
                Err(error) => {
                    picker.error = format!("{error:#}");
                    Stay
                }
            },
            Modal::Import(importer) => match importer.key(key) {
                ImportAction::Stay => Stay,
                ImportAction::Close => Close,
                ImportAction::Apply(task) => {
                    self.editor.task = task;
                    self.ui.message = "Task imported. t edits it; r starts the workflow.".into();
                    Close
                }
            },
            Modal::Text { scroll, body, .. } => {
                // Scrolling stops at the last line.
                let last =
                    u16::try_from(body.lines().count().saturating_sub(1)).unwrap_or(u16::MAX);
                match key.code {
                    Key::Up | Key::Char('k') => *scroll = scroll.saturating_sub(1),
                    Key::Down | Key::Char('j') => *scroll = scroll.saturating_add(1),
                    Key::PageDown => *scroll = scroll.saturating_add(15),
                    Key::PageUp => *scroll = scroll.saturating_sub(15),
                    Key::Home => *scroll = 0,
                    Key::End => *scroll = last,
                    Key::Char('q') => return Ok(Close),
                    _ => {}
                }
                *scroll = (*scroll).min(last);
                Stay
            }
            Modal::Picker {
                items,
                selected,
                filter,
                ..
            } => {
                let matches = matching_choices(items, filter);
                match menu_key(key) {
                    Key::Up => *selected = selected.saturating_sub(1),
                    Key::Down => *selected = (*selected + 1).min(matches.len().saturating_sub(1)),
                    Key::Enter if !matches.is_empty() => {
                        let value = matches[(*selected).min(matches.len() - 1)].value.clone();
                        if let PickValue::Action(id) = value {
                            return Ok(DialogResult::Emit(Effect::Perform(id)));
                        }
                        self.pick(value)?;
                        return Ok(Close);
                    }
                    _ => {
                        filter.input(key);
                        *selected = 0;
                    }
                }
                Stay
            }
            Modal::Diff(view) => {
                match view.key(key) {
                    DiffAction::Stay => {}
                    DiffAction::Close => return Ok(Close),
                    DiffAction::Pager => {
                        self.ui.external =
                            Some(external::page_text(&super::diff_view::ansi(&view.patch))?)
                    }
                    DiffAction::Editor(path, line) => {
                        // Git names files from the top of the repository.
                        let root = super::completion::repo_root(&self.run_dir()?);
                        self.ui.external = Some(external::edit(&root.join(path), line, &root)?);
                    }
                    DiffAction::Difftool => {
                        let run = self.session.run().context("No run")?;
                        let (from, to) = run
                            .change_range()
                            .map_err(|reason| anyhow::anyhow!("No difftool: {reason}"))?;
                        self.ui.external = Some(external::difftool(&self.run_dir()?, from, to)?);
                    }
                }
                Stay
            }
            Modal::Handoff(view) => match view.key(key) {
                HandoffAction::Stay => Stay,
                HandoffAction::Close => Close,
                HandoffAction::Edit(file) => {
                    let dir = self.run_dir()?;
                    self.ui.external = Some(external::edit(&dir.join(file), None, &dir)?);
                    Stay
                }
            },
            Modal::Menu { scope, group } => match (key.code, *group) {
                _ if key.modifiers.intersects(Mods::CONTROL | Mods::ALT) => Stay,
                (Key::Char(' '), _) => Close,
                (Key::Backspace, Some(_)) => {
                    *group = None;
                    Stay
                }
                (Key::Char(c), None) => {
                    let groups = actions::menu(*scope);
                    *group = Group::from_key(c).filter(|g| groups.iter().any(|(m, _)| m == g));
                    Stay
                }
                (Key::Char(c), Some(g)) => match actions::menu_action(*scope, g, c) {
                    Some(id) => DialogResult::Emit(Effect::Perform(id)),
                    None => Stay,
                },
                _ => Stay,
            },
            Modal::Loading(_) | Modal::Ask(_) => Stay,
            Modal::Confirm(dialog) => {
                if self.confirm_key(dialog, key) {
                    Close
                } else {
                    Stay
                }
            }
            Modal::SessionRuns { selected } => self.session_runs_key(selected, key),
            Modal::Notifications {
                backends,
                selected,
                status,
            } => self.notifications_key(backends, selected, status, key)?,
        })
    }
    pub(super) fn paste(&mut self, text: &str) {
        match self.ui.dialog_mut() {
            Some(Modal::Input { text: field, .. }) => {
                field.insert_str(text);
            }
            Some(Modal::Stage { editor, .. }) => editor.paste(text),
            Some(Modal::Picker { filter, .. }) => {
                filter.insert_str(text);
            }
            Some(Modal::Repositories(picker)) => picker.paste(text),
            Some(Modal::Import(importer)) => importer.paste(text),
            Some(Modal::Harness(editor)) => editor.paste(text),
            None => self.paste_runs_filter(text),
            _ => {}
        }
    }
}

/// `ask` is the text of an `Ask` dialog, which reads the app.
pub(super) fn draw_modal(
    frame: &mut Frame<'_>,
    modal: &mut Modal,
    ask: Option<AskText>,
    palette: Palette,
) {
    if let Modal::Menu { scope, group } = modal {
        return draw_menu(frame, *scope, *group, palette);
    }
    let (width, height) = match modal {
        Modal::Stage { .. } => (100, 35),
        // As large as the screen allows.
        Modal::Diff(_) => (u16::MAX, u16::MAX),
        Modal::Handoff(_) => (100, 32),
        Modal::Loading(_) => (64, 7),
        Modal::Ask(_) => (72, 9),
        _ => (95, 26),
    };
    let rect = popup(frame.area(), width, height);
    frame.render_widget(Clear, rect);
    match modal {
        Modal::Repositories(picker) => picker.draw(frame, rect, palette),
        Modal::Import(importer) => importer.draw(frame, rect, palette),
        Modal::Harness(editor) => editor.draw(frame, rect, palette),
        Modal::Confirm(dialog) => dialog.draw(frame, rect, palette),
        Modal::Diff(view) => view.draw(frame, rect, palette),
        Modal::Handoff(view) => view.draw(frame, rect, palette),
        Modal::Notifications {
            backends,
            selected,
            status,
        } => draw_notifications(frame, backends, *selected, status, palette),
        // Drawn by the app, which holds the runs; the menu returned above.
        Modal::SessionRuns { .. } | Modal::Menu { .. } => {}
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
        Modal::Stage { editor, .. } => editor.draw(frame, rect, palette),
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
            // Only the lines that can show are laid out, so a long diff stays
            // cheap to draw. The body was cleaned when the dialog opened.
            let block = palette.modal(title, &[("↑↓", "scroll"), ("Esc", "close")]);
            let lines = body
                .lines()
                .skip(usize::from(*scroll))
                .take(usize::from(block.inner(rect).height));
            let body = if *guide {
                guide_text(lines, palette)
            } else {
                Text::from(lines.map(|l| Line::raw(l.to_owned())).collect::<Vec<_>>())
            };
            frame.render_widget(
                Paragraph::new(body).wrap(Wrap { trim: false }).block(block),
                rect,
            );
        }
        Modal::Ask(_) => {
            let (title, question, keys) = ask.unwrap_or_default();
            let block = palette.modal(title, &[]);
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::raw(""),
                    Line::styled(
                        storage::clean_terminal(&question),
                        Style::default().fg(palette.fg),
                    ),
                    Line::raw(""),
                    palette.hints(keys),
                ])
                .wrap(Wrap { trim: false }),
                inner.inner(Margin::new(1, 0)),
            );
        }
        Modal::Loading(loading) => {
            const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let tick = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() / 80)
                .unwrap_or(0) as usize;
            let discovery = loading.text.starts_with("Reading");
            let block = palette.modal(
                if discovery {
                    "Model discovery"
                } else {
                    "Please wait"
                },
                &[("Esc", "cancel")],
            );
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
                        Span::raw(loading.text.clone()),
                    ]),
                    Line::styled(
                        if discovery {
                            "No inference is performed."
                        } else {
                            ""
                        },
                        Style::default().fg(palette.muted),
                    ),
                ])
                .wrap(Wrap { trim: false }),
                inner,
            );
        }
    }
    if let Some(hint) = modal.active_input().and_then(|field| field.mode_hint()) {
        if rect.width > 4 && rect.height > 0 {
            frame.render_widget(
                Paragraph::new(format!(" {hint} "))
                    .style(Style::default().fg(palette.accent).bg(palette.panel))
                    .centered(),
                ratatui::layout::Rect::new(rect.x + 2, rect.bottom() - 1, rect.width - 4, 1),
            );
        }
    }
}

/// The `Space` menu as a sheet above the footer, one column per group. Once a
/// group is chosen, its actions take the accent and the others go quiet.
fn draw_menu(frame: &mut Frame<'_>, scope: Scope, group: Option<Group>, palette: Palette) {
    let groups = actions::menu(scope);
    let tallest = groups
        .iter()
        .map(|(_, items)| items.len())
        .max()
        .unwrap_or(0);
    let area = frame.area();
    // Borders, group names, a gap, the actions, a gap, and the hint.
    let height = (tallest as u16 + 6).min(area.height.saturating_sub(2));
    let rect = Rect::new(
        area.x,
        area.bottom().saturating_sub(height + 1),
        area.width,
        height,
    );
    frame.render_widget(Clear, rect);
    let (title, keys, hint): (String, &[(&str, &str)], _) = match group {
        None => (
            "Menu".into(),
            &[("Esc", "close")],
            "Press a group letter, then an action letter. Space closes.",
        ),
        Some(g) => (
            format!("Menu · {}", g.name()),
            &[("Backspace", "groups"), ("Esc", "close")],
            "Press an action letter. Backspace returns to the groups.",
        ),
    };
    let block = palette.modal(&title, keys);
    let inner = block.inner(rect).inner(Margin::new(1, 0));
    frame.render_widget(block, rect);
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let columns = Layout::horizontal(
        groups
            .iter()
            .map(|_| Constraint::Ratio(1, groups.len().max(1) as u32)),
    )
    .split(body);
    let strong = |color| Style::default().fg(color).add_modifier(Modifier::BOLD);
    let quiet = Style::default().fg(palette.muted);
    for ((g, items), column) in groups.iter().zip(columns.iter()) {
        let chosen = group == Some(*g);
        let other = group.is_some() && !chosen;
        let (key_style, name_style, letter_style, label_style) = if chosen {
            (
                strong(palette.on_accent()).bg(palette.accent),
                strong(palette.fg),
                strong(palette.accent),
                Style::default().fg(palette.fg),
            )
        } else if other {
            (quiet, quiet, quiet, quiet)
        } else {
            (
                strong(palette.accent),
                Style::default().fg(palette.fg),
                quiet,
                Style::default().fg(palette.fg),
            )
        };
        let mut lines = vec![
            Line::from(vec![
                Span::styled(g.key().to_string(), key_style),
                Span::styled(format!(" {}", g.name()), name_style),
            ]),
            Line::raw(""),
        ];
        lines.extend(items.iter().map(|(letter, entry)| {
            Line::from(vec![
                Span::styled(letter.to_string(), letter_style),
                Span::styled(format!(" {}", entry.label), label_style),
            ])
        }));
        frame.render_widget(Paragraph::new(lines), *column);
    }
    frame.render_widget(Paragraph::new(Line::styled(hint, quiet)), footer);
}

/// Render the keyboard guide: CAPS lines are headings, `key\tdescription`
/// lines become aligned bindings, everything else is a muted note.
fn guide_text<'a>(lines: impl Iterator<Item = &'a str>, palette: Palette) -> Text<'static> {
    lines
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
