use super::input::InputField;
use super::{
    actions::{self, ActionId},
    app::App,
    harness_editor::HarnessEditor,
    history::{Compare, Confirm},
    path_picker::{PathPicker, PickerAction},
    task_importer::{ImportAction, TaskImporter},
    theme::{self, Palette, Theme},
    widgets::{content, menu_key, popup},
    workflow_page::{StageEditor, StageRequest},
};
use crate::{
    catalog::{self, Model},
    config::{self, Stage},
    engine, presets, storage,
};
use anyhow::{Context, Result, ensure};
use crossterm::event::{KeyCode as Key, KeyEvent, KeyModifiers as Mods};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin},
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
    Loading(String),
    Confirm(Box<Confirm>),
    /// A yes/no question before something hard to take back.
    Ask(Box<Ask>),
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
    /// Stop this run's unattended stage and open its session.
    TakeOver {
        run: String,
    },
    DeleteWorkflow {
        index: usize,
    },
    /// Unsaved workflow edits before exiting: [y] saves, [n] discards.
    SaveAndExit,
    /// Close this editor without applying its changes; [Esc] returns to it.
    DiscardDraft(Box<Modal>),
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
                    app.settings
                        .workflows
                        .get(*index)
                        .map_or("", |w| w.name.as_str()),
                    app.settings
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
                    app.config_path.display()
                ),
                &[
                    ("y / Enter", "save and exit"),
                    ("n", "exit without saving"),
                    ("Esc", "cancel"),
                ],
            ),
            Self::DiscardDraft(_) => (
                "Discard changes?",
                "This editor has changes that are not applied.".into(),
                &[("y / Enter", "discard"), ("n / Esc", "keep editing")],
            ),
        }
    }
}

impl Modal {
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
    /// Apply a picker choice; true means exit, for menu actions such as quit.
    pub(super) fn pick(&mut self, choice: PickValue) -> Result<bool> {
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
                let harness = stage.harness.clone();
                self.modal = Some(Modal::Stage {
                    editor: Box::new(StageEditor::new(
                        stage,
                        catalog::known_harnesses(&self.settings),
                    )),
                    insert_at: Some(self.selected + 1),
                });
                self.request_catalog(harness);
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
                } else if let Some((mut editor, insert_at)) = self.stage_editor.take() {
                    editor.set_model(model);
                    self.modal = Some(Modal::Stage { editor, insert_at });
                } else {
                    let stage = self.stage_mut();
                    stage.model = model.id;
                    stage.provider = model.provider;
                    self.dirty = true;
                }
            }
            PickValue::Theme(theme) => {
                theme::save(&self.theme_path, theme, self.vim_motions)?;
                self.theme = theme;
                self.message = format!("Theme saved: {}. T changes it anytime.", theme.name());
            }
            PickValue::Action(id) => return self.perform(id),
            PickValue::Compare(choice) => self.show_comparison(&choice)?,
        }
        Ok(false)
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
                self.focused
                    .run
                    .as_mut()
                    .context("No run")?
                    .command(engine::Cmd::Retry {
                        interactive: false,
                        model: Some(value.trim().into()),
                    })?;
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
                engine::edit_handoff(self.focused.run.as_mut().context("No run")?, value)?
            }
        }
        Ok(())
    }
    /// Keys for an `Ask` dialog; true means exit.
    fn ask_key(&mut self, ask: Box<Ask>, key: KeyEvent) -> Result<bool> {
        let yes = matches!(key.code, Key::Char('y') | Key::Enter)
            && key.modifiers - Mods::SHIFT == Mods::NONE;
        let no = key.code == Key::Esc
            || (key.code == Key::Char('n') && key.modifiers - Mods::SHIFT == Mods::NONE);
        if !yes && !no {
            self.modal = Some(Modal::Ask(ask));
            return Ok(false);
        }
        match *ask {
            Ask::SaveAndExit if yes => {
                self.save(false)?;
                return Ok(self.quit());
            }
            Ask::SaveAndExit if key.code == Key::Char('n') => {
                self.dirty = false;
                return Ok(self.quit());
            }
            Ask::DiscardDraft(editor) if !yes => self.modal = Some(*editor),
            _ if !yes => {}
            Ask::CancelStage { run } => {
                // The stage may have stopped while the question was open.
                if self.running(&run) {
                    self.focused.cancel.store(true, Ordering::Relaxed);
                    self.message = "Stopping stage…".into();
                } else {
                    self.message = format!("{run} is not running a stage; nothing was cancelled.");
                }
            }
            Ask::TakeOver { run } => {
                if self.running(&run) {
                    self.focused.takeover = true;
                    self.focused.cancel.store(true, Ordering::Relaxed);
                    self.message = "Stopping the unattended stage to open its session…".into();
                } else {
                    self.message =
                        format!("{run}'s stage already stopped; [t] reopens its session.");
                }
            }
            Ask::DeleteWorkflow { index } => {
                ensure!(
                    self.settings.workflows.len() > 1 && index < self.settings.workflows.len(),
                    "Keep at least one workflow"
                );
                let name = self.settings.workflows[index].name.clone();
                self.remember(format!("workflow “{name}”"));
                self.settings.workflows.remove(index);
                self.workflow = 0;
                self.selected = 0;
                self.dirty = true;
                self.message = format!("Deleted workflow “{name}” — [u] to undo.");
            }
            Ask::SaveAndExit | Ask::DiscardDraft(_) => {}
        }
        Ok(false)
    }
    /// Whether the focused run is `id` and still has a worker.
    fn running(&self, id: &str) -> bool {
        self.focused.worker.is_some() && self.focused.run.as_ref().is_some_and(|r| r.id == id)
    }
    pub(super) fn modal_key(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(modal) = self.modal.take() else {
            return Ok(false);
        };
        let modal = match modal {
            Modal::Ask(ask) => return self.ask_key(ask, key),
            Modal::Loading(_) if key.code == Key::Esc && self.cancel_switch() => {
                return Ok(false);
            }
            other => other,
        };
        let mut modal = modal;
        if let Some(field) = modal.active_input() {
            field.set_vim(self.vim_motions);
            if field.vim_key(key) {
                self.modal = Some(modal);
                return Ok(false);
            }
        }
        if key.code == Key::Esc && !matches!(modal, Modal::Repositories(_) | Modal::Import(_)) {
            let changed = match &modal {
                Modal::Stage { editor, .. } => editor.changed(),
                Modal::Harness(editor) => editor.changed(),
                _ => false,
            };
            if changed {
                self.modal = Some(Modal::Ask(Box::new(Ask::DiscardDraft(Box::new(modal)))));
                return Ok(false);
            }
            self.catalog = None;
            self.modal = self.model_editor.take().map(Modal::Harness);
            if let Some((editor, insert_at)) = self.stage_editor.take() {
                self.modal = Some(Modal::Stage { editor, insert_at });
            }
            return Ok(false);
        }
        if key.code == Key::F(7) && matches!(modal, Modal::Harness(_)) {
            if let Modal::Harness(editor) = modal {
                self.model_editor = Some(editor);
            }
            self.model_picker();
            return Ok(false);
        }
        let mut keep = true;
        let mut exit = false;
        let mut pick_model = false;
        let mut refresh_catalog = None;
        let mut open_project = None;
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
                        match editor.key(key) {
                            StageRequest::PickModel => pick_model = true,
                            StageRequest::Catalog => refresh_catalog = Some(editor.harness()),
                            StageRequest::None => {}
                        }
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
                Modal::Repositories(picker) => match picker.key(key) {
                    Ok(PickerAction::Close) => keep = false,
                    Ok(PickerAction::Select(path)) => open_project = Some(path),
                    Ok(PickerAction::Stay) => {}
                    Err(error) => picker.error = format!("{error:#}"),
                },
                Modal::Import(importer) => match importer.key(key) {
                    ImportAction::Stay => {}
                    ImportAction::Close => keep = false,
                    ImportAction::Apply(task) => {
                        self.task = task;
                        self.message = "Task imported. t edits it; r starts the workflow.".into();
                        keep = false;
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
                        Key::Char('q') => keep = false,
                        _ => {}
                    }
                    *scroll = (*scroll).min(last);
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
                        Key::Down => {
                            *selected = (*selected + 1).min(matches.len().saturating_sub(1))
                        }
                        Key::Enter if !matches.is_empty() => {
                            let value = matches[(*selected).min(matches.len() - 1)].value.clone();
                            // The menu closes before its action runs, so the action can open
                            // its own dialog and its errors reach the status line like a key's.
                            if matches!(value, PickValue::Action(_)) {
                                keep = false;
                            }
                            exit = self.pick(value)?;
                            keep = false;
                        }
                        _ => {
                            filter.input(key);
                            *selected = 0;
                        }
                    }
                }
                Modal::Loading(_) | Modal::Ask(_) => {}
                Modal::Confirm(dialog) => keep = !self.confirm_key(dialog, key),
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
        let modal = match (open_project, modal) {
            (Some(path), Modal::Repositories(picker)) => {
                self.begin_switch(path, picker);
                return result.map(|()| exit);
            }
            (_, modal) => modal,
        };
        if pick_model {
            if let Modal::Stage { editor, insert_at } = modal {
                let harness = editor.harness();
                self.stage_editor = Some((editor, insert_at));
                self.model_picker_for(harness);
                return Ok(false);
            }
        }
        if keep {
            self.modal = Some(modal);
        }
        if let Some(harness) = refresh_catalog {
            self.request_catalog(harness);
        }
        if matches!(self.modal, Some(Modal::Harness(_))) {
            // Validation is shown in the draft, so it cannot leave a stale
            // error behind the dialog after the user corrects the field.
            Ok(exit)
        } else {
            result.map(|()| exit)
        }
    }
    pub(super) fn paste(&mut self, text: &str) {
        match &mut self.modal {
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
    let (width, height) = match modal {
        Modal::Stage { .. } => (100, 35),
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
        Modal::Loading(message) => {
            const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let tick = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() / 80)
                .unwrap_or(0) as usize;
            let discovery = message.starts_with("Reading");
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
                        Span::raw(message.clone()),
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
