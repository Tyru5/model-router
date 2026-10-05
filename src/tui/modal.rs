use super::{
    app::App,
    harness_editor::HarnessEditor,
    path_picker::{PathPicker, PickerAction},
    task_importer::{ImportAction, TaskImporter},
    theme::{self, Palette, Theme},
    widgets::{content, menu_key, popup},
    workflow_page::StageEditor,
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
use tui_textarea::TextArea;

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
pub(super) struct Choice {
    pub(super) label: String,
    pub(super) value: PickValue,
}
pub(super) fn matching_choices<'a>(items: &'a [Choice], filter: &TextArea<'_>) -> Vec<&'a Choice> {
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

impl App {
    pub(super) fn pick(&mut self, choice: PickValue) -> Result<()> {
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
    pub(super) fn modal_key(&mut self, key: KeyEvent) -> Result<()> {
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
            _ => {}
        }
    }
}

pub(super) fn draw_modal(frame: &mut Frame<'_>, modal: &mut Modal, palette: Palette) {
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

pub(super) const HELP: &str = "WORKFLOW
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
