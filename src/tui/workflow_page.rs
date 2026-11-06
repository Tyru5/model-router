use super::input::InputField;
use super::{
    actions::ActionId,
    app::App,
    harness_editor::HarnessEditor,
    modal::{Ask, Choice, InputTarget, Modal, PickValue},
    path_picker::PathPicker,
    task_importer::TaskImporter,
    theme::Palette,
    widgets::{content, focus_textarea, kv, or_default, textarea, wrapped_textarea},
};
use crate::{
    adapters, catalog,
    config::{Check, Mode, Role, Stage},
    engine::{self, Run},
    harness_settings, presets, storage,
};
use anyhow::{Context, Result, bail, ensure};
use crossterm::event::{KeyCode as Key, KeyEvent, KeyModifiers as Mods};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Cell, HighlightSpacing, List, ListItem, ListState, Padding, Paragraph, Row, Table,
        TableState, Wrap,
    },
};
use tui_textarea::CursorMove;

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
    "Router checks (one per line; ? prefix = optional)",
    "Stage instructions",
];
/// Multiline fields, where Enter inserts a newline.
const CHECKS: usize = 10;
const INSTRUCTIONS: usize = 11;
const CHOICES: [usize; 3] = [1, 2, 6];
/// The model field: chosen from the harness catalog, typed only on request.
pub(super) const MODEL: usize = 3;
const PROVIDER: usize = 4;
/// The effort field: a choice list once the catalog reports levels.
pub(super) const EFFORT: usize = 5;
/// What the stage editor asks the app to do after a key.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum StageRequest {
    None,
    /// Open the model catalog for the editor's current harness.
    PickModel,
    /// The harness changed; its catalog supplies the effort levels.
    Catalog,
}
pub(super) struct StageEditor {
    stage: Stage,
    pub(super) fields: Vec<InputField<'static>>,
    selected: usize,
    harnesses: Vec<String>,
    /// The model field accepts typing until the next catalog pick.
    typing_model: bool,
    /// The catalog of the field's harness, once read.
    catalog: Option<catalog::Catalog>,
}
impl StageEditor {
    pub(super) fn active_input(&mut self) -> Option<&mut InputField<'static>> {
        if self.is_choice(self.selected) || (self.selected == MODEL && !self.typing_model) {
            None
        } else {
            Some(&mut self.fields[self.selected])
        }
    }
    pub(super) fn new(stage: Stage, harnesses: Vec<String>) -> Self {
        let args = stage
            .extra_args
            .iter()
            .map(|a| shell_words::quote(a).into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let checks = stage
            .checks
            .iter()
            .map(|c| {
                let command = shell_words::join(&c.command);
                if c.required {
                    command
                } else {
                    format!("? {command}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
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
            &checks,
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
            typing_model: false,
            catalog: None,
        }
    }
    pub(super) fn harness(&self) -> String {
        content(&self.fields[2])
    }
    pub(super) fn set_model(&mut self, model: catalog::Model) {
        self.fields[MODEL] = textarea(&model.id);
        self.fields[PROVIDER] = textarea(&model.provider);
        self.typing_model = false;
    }
    pub(super) fn set_catalog(&mut self, catalog: catalog::Catalog) {
        self.catalog = Some(catalog);
    }
    /// Effort levels the catalog reports for the current model selection.
    fn efforts(&self) -> Option<catalog::Efforts> {
        self.catalog.as_ref()?.efforts_for(
            content(&self.fields[MODEL]).trim(),
            content(&self.fields[PROVIDER]).trim(),
        )
    }
    fn is_choice(&self, index: usize) -> bool {
        CHOICES.contains(&index) || (index == EFFORT && self.efforts().is_some())
    }
    fn choices(&self, index: usize) -> Vec<String> {
        match index {
            1 => Role::ALL.iter().map(|r| r.label().into()).collect(),
            2 => self.harnesses.clone(),
            EFFORT => {
                let mut levels = self.efforts().map(|e| e.levels).unwrap_or_default();
                let current = content(&self.fields[EFFORT]).trim().to_owned();
                if !current.is_empty() && !levels.contains(&current) {
                    levels.push(current);
                }
                levels.insert(0, String::new());
                levels
            }
            _ => vec!["unattended".into(), "interactive".into()],
        }
    }
    pub(super) fn finish(&self) -> Result<Stage> {
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
        stage.checks = parse_checks(&content(&self.fields[CHECKS]), &self.stage.checks)?;
        stage.instructions = content(&self.fields[INSTRUCTIONS]);
        Ok(stage)
    }
    fn focus(&mut self, index: usize) {
        self.selected = index;
        self.typing_model = false;
        focus_textarea(&mut self.fields[index]);
    }
    /// Whether the draft differs from the stage it opened with, or cannot be applied.
    pub(super) fn changed(&self) -> bool {
        self.finish().map_or(true, |stage| {
            serde_json::to_value(&stage).ok() != serde_json::to_value(&self.stage).ok()
        })
    }
    /// Whether Up/Down move the cursor inside a multiline field instead of
    /// leaving it: only on its first or last line do they switch fields.
    fn moves_within(&self, key: &KeyEvent) -> bool {
        if ![CHECKS, INSTRUCTIONS].contains(&self.selected) || !key.modifiers.is_empty() {
            return false;
        }
        let field = &self.fields[self.selected];
        let row = field.cursor().0;
        match key.code {
            Key::Up => row > 0,
            Key::Down => row + 1 < field.lines().len(),
            _ => false,
        }
    }
    pub(super) fn key(&mut self, key: KeyEvent) -> StageRequest {
        if self.moves_within(&key) {
            // Moved directly, so Vim's Normal mode moves too.
            self.fields[self.selected].move_cursor(if key.code == Key::Up {
                CursorMove::Up
            } else {
                CursorMove::Down
            });
        } else if matches!(key.code, Key::Tab | Key::Down)
            || (key.code == Key::Char('n') && key.modifiers.contains(Mods::CONTROL))
        {
            self.focus((self.selected + 1) % self.fields.len());
        } else if matches!(key.code, Key::BackTab | Key::Up)
            || (key.code == Key::Char('p') && key.modifiers.contains(Mods::CONTROL))
        {
            self.focus((self.selected + self.fields.len() - 1) % self.fields.len());
        } else if self.selected == MODEL && !self.typing_model {
            match key.code {
                Key::Enter | Key::Char(' ') | Key::Left | Key::Right => {
                    return StageRequest::PickModel;
                }
                Key::Backspace | Key::Delete => self.fields[MODEL] = textarea(""),
                Key::Char('i') => {
                    self.typing_model = true;
                    focus_textarea(&mut self.fields[MODEL]);
                }
                _ => {}
            }
        } else if self.selected == MODEL && key.code == Key::Enter {
            self.focus(self.selected + 1);
        } else if self.is_choice(self.selected) {
            if matches!(
                key.code,
                Key::Left | Key::Right | Key::Char(' ') | Key::Enter
            ) {
                let values = self.choices(self.selected);
                if values.is_empty() {
                    return StageRequest::None;
                }
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
                    for index in MODEL..=EFFORT {
                        self.fields[index] = textarea("");
                    }
                    self.catalog = None;
                    return StageRequest::Catalog;
                }
            }
        } else if key.code == Key::Enter && ![CHECKS, INSTRUCTIONS].contains(&self.selected) {
            self.focus((self.selected + 1) % self.fields.len());
        } else {
            self.fields[self.selected].input(key);
        }
        StageRequest::None
    }
    pub(super) fn paste(&mut self, text: &str) {
        if !self.is_choice(self.selected) && (self.selected != MODEL || self.typing_model) {
            self.fields[self.selected].insert_str(text);
        }
    }
    pub(super) fn draw(&mut self, frame: &mut Frame<'_>, rect: Rect, palette: Palette) {
        let block = palette.modal(
            "Edit stage",
            &[("↑/↓ / Tab", "switch field"), ("←/→", "cycle choice")],
        );
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let chunks = Layout::vertical([Constraint::Min(8), Constraint::Length(2)]).split(inner);
        let halves = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(chunks[0]);
        let value_width = halves[0].width.saturating_sub(4) as usize;
        let rows = FIELDS.iter().enumerate().map(|(i, label)| {
            let value = content(&self.fields[i]).replace('\n', " ");
            let choice = self.is_choice(i) || (i == MODEL && !self.typing_model);
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
                .highlight_style(palette.highlight_style())
                .highlight_symbol("▌ ")
                .highlight_spacing(HighlightSpacing::Always)
                .block(Block::default().padding(Padding::right(2))),
            halves[0],
            &mut ListState::default().with_selected(Some(self.selected)),
        );
        if self.selected == MODEL && !self.typing_model {
            let harness = self.harness();
            let current = content(&self.fields[MODEL]);
            let lines = vec![
                Line::from(vec![
                    Span::styled("Current  ", Style::default().fg(palette.muted)),
                    Span::styled(
                        if current.is_empty() {
                            format!("{harness} default")
                        } else {
                            current
                        },
                        Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::raw(""),
                palette.hints(&[("Enter", &format!("choose from the {harness} catalog"))]),
                palette.hints(&[("Backspace", "use the harness default")]),
                palette.hints(&[("i", "type a model ID the catalog does not list")]),
                Line::raw(""),
                Line::styled(
                    "The catalog is read from the harness without starting a model; a provider from the catalog fills the Provider field.",
                    Style::default().fg(palette.muted),
                ),
            ];
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                    palette
                        .panel(format!(" {} ", FIELDS[MODEL]))
                        .border_style(Style::default().fg(palette.accent)),
                ),
                halves[1],
            );
        } else if self.selected == EFFORT && self.is_choice(EFFORT) {
            let efforts = self.efforts().unwrap_or_default();
            let choices = self.choices(EFFORT);
            let current = content(&self.fields[EFFORT]);
            let index = choices.iter().position(|v| *v == current).unwrap_or(0);
            let model = content(&self.fields[MODEL]);
            let panes = Layout::vertical([
                Constraint::Length(choices.len() as u16 + 2),
                Constraint::Min(1),
            ])
            .split(halves[1]);
            frame.render_stateful_widget(
                List::new(choices.iter().map(|value| {
                    ListItem::new(if value.is_empty() {
                        Line::raw("Harness default")
                    } else if !efforts.levels.contains(value) {
                        Line::from(vec![
                            Span::raw(value.clone()),
                            Span::styled("  not listed", Style::default().fg(palette.warn)),
                        ])
                    } else if *value == efforts.default {
                        Line::from(vec![
                            Span::raw(value.clone()),
                            Span::styled("  default", Style::default().fg(palette.muted)),
                        ])
                    } else {
                        Line::raw(value.clone())
                    })
                }))
                .block(
                    palette
                        .panel(format!(" {} ", FIELDS[EFFORT]))
                        .border_style(Style::default().fg(palette.accent)),
                )
                .highlight_style(palette.selected_style()),
                panes[0],
                &mut ListState::default().with_selected(Some(index)),
            );
            frame.render_widget(
                Paragraph::new(format!(
                    "Levels from {} for {}. ←/→ or Space cycles; blank inherits the harness default.",
                    efforts.source,
                    if model.is_empty() { "any model".to_owned() } else { model }
                ))
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(palette.muted)),
                panes[1],
            );
        } else {
            let field = &mut self.fields[self.selected];
            palette.style_textarea(field);
            wrapped_textarea(
                frame,
                field,
                halves[1],
                palette
                    .panel(format!(" {} ", FIELDS[self.selected]))
                    .border_style(Style::default().fg(palette.accent)),
            );
        }
        let mut keys = palette.hints(&[("Ctrl+S", "apply stage"), ("Esc", "cancel")]);
        keys.spans.push(Span::styled(
            if self.selected == CHECKS {
                "   ·  argv per line, no shell; edit timeout_seconds in TOML"
            } else if self.selected == MODEL {
                "   ·  Enter lists the harness's models; blank inherits its default"
            } else if self.selected == EFFORT && self.catalog.is_none() {
                "   ·  reading the harness catalog for its levels…"
            } else {
                "   ·  blank model inherits the harness default"
            },
            Style::default().fg(palette.muted),
        ));
        frame.render_widget(
            Paragraph::new(vec![Line::raw(""), keys]).wrap(Wrap { trim: false }),
            chunks[1],
        );
    }
}

/// Parses the router checks field: one shell-quoted argv per nonempty line, a
/// leading `?` marks it optional. Unchanged lines keep their TOML timeout.
fn parse_checks(text: &str, existing: &[Check]) -> Result<Vec<Check>> {
    let mut checks = vec![];
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (required, line) = match line.strip_prefix('?') {
            Some(rest) => (false, rest),
            None => (true, line),
        };
        let command = shell_words::split(line)
            .with_context(|| format!("Router check line {} has unmatched quotes", number + 1))?;
        ensure!(
            !command.is_empty(),
            "Router check line {} has no command",
            number + 1
        );
        let timeout_seconds = existing
            .iter()
            .find(|c| c.command == command && c.required == required)
            .map_or(Check::default().timeout_seconds, |c| c.timeout_seconds);
        checks.push(Check {
            command,
            required,
            timeout_seconds,
        });
    }
    Ok(checks)
}

fn duration(seconds: u64) -> String {
    if seconds % 60 == 0 {
        format!("{} min", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

impl App {
    pub(super) fn perform_workflow(&mut self, id: ActionId) -> Result<()> {
        let count = self.settings.workflows[self.workflow].stages.len();
        match id {
            ActionId::NextStage => self.selected = (self.selected + 1).min(count.saturating_sub(1)),
            ActionId::PreviousStage => self.selected = self.selected.saturating_sub(1),
            ActionId::ToggleStage => {
                let stage = self.stage_mut();
                stage.enabled = !stage.enabled;
                self.dirty = true;
            }
            ActionId::ToggleMode => {
                let stage = self.stage_mut();
                stage.mode = if stage.mode == Mode::Interactive {
                    Mode::Unattended
                } else {
                    Mode::Interactive
                };
                self.dirty = true;
            }
            ActionId::EditTask => self.input(
                "Task · [Ctrl+S] applies · [Enter] adds a line",
                InputTarget::Task,
                &self.task.clone(),
            ),
            ActionId::Repositories => {
                self.modal = Some(Modal::Repositories(Box::new(PathPicker::new(
                    &self.project,
                    storage::recent_projects(),
                )?)));
            }
            ActionId::ImportTask => {
                self.modal = Some(Modal::Import(Box::new(TaskImporter::new(
                    &self.project,
                    &self.task,
                )?)));
            }
            ActionId::ChooseWorkflow => {
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
            ActionId::RenameWorkflow => self.input(
                "Rename workflow",
                InputTarget::RenameWorkflow,
                &self.settings.workflows[self.workflow].name.clone(),
            ),
            ActionId::RepairLimit => self.input(
                "Maximum automatic repair passes (0 disables repairs)",
                InputTarget::RepairLimit,
                &self.settings.workflows[self.workflow]
                    .max_repair_passes
                    .to_string(),
            ),
            ActionId::PickHarness => {
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
            ActionId::EditStage => {
                self.modal = Some(Modal::Stage {
                    editor: Box::new(StageEditor::new(
                        self.stage().clone(),
                        catalog::known_harnesses(&self.settings),
                    )),
                    insert_at: None,
                });
                self.request_catalog(self.stage().harness.clone());
            }
            ActionId::HarnessSettings => {
                self.modal = Some(Modal::Harness(Box::new(HarnessEditor::new(
                    self.stage().clone(),
                    self.settings.providers.clone(),
                ))));
                self.request_catalog(self.stage().harness.clone());
            }
            ActionId::Models => self.model_picker(),
            ActionId::AddStage => {
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
            ActionId::DuplicateStage => {
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
            ActionId::RemoveStage => {
                ensure!(count > 1, "Keep at least one stage");
                let name = self.stage().name.clone();
                self.remember(format!("stage “{name}”"));
                self.settings.workflows[self.workflow]
                    .stages
                    .remove(self.selected);
                self.selected = self.selected.min(count - 2);
                self.dirty = true;
                self.message = format!("Removed stage “{name}” — [u] to undo.");
            }
            ActionId::DeleteWorkflow => {
                ensure!(
                    self.settings.workflows.len() > 1,
                    "Keep at least one workflow"
                );
                self.modal = Some(Modal::Ask(Box::new(Ask::DeleteWorkflow {
                    index: self.workflow,
                })));
            }
            ActionId::Undo => self.undo(),
            ActionId::MoveStageUp => {
                if self.selected > 0 {
                    self.settings.workflows[self.workflow]
                        .stages
                        .swap(self.selected, self.selected - 1);
                    self.selected -= 1;
                    self.dirty = true;
                }
            }
            ActionId::MoveStageDown => {
                if self.selected + 1 < count {
                    self.settings.workflows[self.workflow]
                        .stages
                        .swap(self.selected, self.selected + 1);
                    self.selected += 1;
                    self.dirty = true;
                }
            }
            ActionId::ToggleWorktree => self.worktree = !self.worktree,
            ActionId::Save => self.save(false)?,
            ActionId::SaveGlobal => self.save(true)?,
            ActionId::Preview => self.text(
                "Launch preview · no model is started",
                engine::preview(
                    &self.settings,
                    &self.settings.workflows[self.workflow],
                    &self.project,
                    &self.task,
                )?,
            ),
            ActionId::Run => {
                // Creating a worktree and snapshot can take a while, so the
                // run starts in the background; the editor stays usable.
                let (project, settings, task) = (
                    self.project.clone(),
                    self.settings.clone(),
                    self.task.clone(),
                );
                let workflow = settings.workflows[self.workflow].clone();
                crate::config::validate_task(&task)?;
                self.start_run(
                    move |placement| {
                        Run::create_at(&project, &settings, &workflow, task, placement)
                    },
                    self.worktree,
                    Some(self.task.clone()),
                    None,
                )?;
            }
            other => bail!("{other:?} is not a workflow action"),
        }
        Ok(())
    }
}

pub(super) fn draw_workflow(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) {
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
        let stage = app.stage();
        let warning = app.harness_checks.warning(&stage.harness);
        let effort = app
            .catalogs
            .get(&stage.harness)
            .and_then(|c| c.efforts_for(&stage.model, &stage.provider))
            .map(|efforts| effort_note(stage, &efforts));
        let hint = app.stats_hint(stage);
        draw_stage_detail(frame, stage, warning, effort, hint, cols[1], palette);
    }
}

/// How the stage's effort compares with what its harness catalog lists.
fn effort_note(stage: &Stage, efforts: &catalog::Efforts) -> (String, bool) {
    if stage.effort.is_empty() {
        let default = if efforts.default.is_empty() {
            String::new()
        } else {
            format!(" ({})", efforts.default)
        };
        (format!("{} default{default}", efforts.source), true)
    } else if efforts.levels.contains(&stage.effort) {
        (format!("listed by {}", efforts.source), true)
    } else {
        (
            format!(
                "not listed by {}: {}",
                efforts.source,
                efforts.levels.join(", ")
            ),
            false,
        )
    }
}

/// `warning` flags missing harness flags from cached doctor results;
/// `effort` says whether the catalog lists the stage's effort level.
fn draw_stage_detail(
    frame: &mut Frame<'_>,
    stage: &Stage,
    warning: Option<String>,
    effort_note: Option<(String, bool)>,
    hint: Option<String>,
    area: Rect,
    palette: Palette,
) {
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
    ];
    if let Some(warning) = warning {
        lines.push(Line::styled(warning, Style::default().fg(palette.warn)));
    }
    lines.extend([
        kv(palette, "Model", model.0, model.1),
        kv(palette, "Execution", stage.mode.label(), true),
        kv(palette, "Provider", provider.0, provider.1),
        kv(palette, "Effort", effort.0, effort.1),
    ]);
    if let Some((note, listed)) = effort_note {
        lines.push(Line::styled(
            format!("         {note}"),
            Style::default().fg(if listed { palette.muted } else { palette.warn }),
        ));
    }
    lines.extend([
        kv(
            palette,
            "Timeout",
            &format!("{} min", stage.timeout_seconds / 60),
            true,
        ),
        kv(palette, "ID", &stage.id, true),
        Line::raw(""),
        Line::styled("Instructions", palette.header_style()),
    ]);
    lines.extend(
        storage::clean_terminal(&stage.instructions)
            .lines()
            .map(|line| Line::styled(line.to_string(), Style::default().fg(palette.fg))),
    );
    if !stage.checks.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled("Router checks", palette.header_style()));
        for check in &stage.checks {
            lines.push(Line::from(vec![
                Span::styled(
                    storage::clean_terminal(&shell_words::join(&check.command)),
                    Style::default().fg(palette.fg),
                ),
                Span::styled(
                    format!(
                        "  {} · {}",
                        if check.required {
                            "required"
                        } else {
                            "optional"
                        },
                        duration(check.timeout_seconds)
                    ),
                    Style::default().fg(palette.muted),
                ),
            ]));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Harness settings", palette.header_style()));
    lines.extend(
        harness_settings::summary(stage)
            .lines()
            .map(|line| Line::styled(line.to_string(), Style::default().fg(palette.fg))),
    );
    lines.push(Line::raw(""));
    if let Some(hint) = hint {
        // Advice from saved runs; the router never applies it.
        lines.push(Line::styled(
            hint,
            Style::default()
                .fg(palette.muted)
                .add_modifier(Modifier::DIM),
        ));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrow_keys_navigate_every_stage_field_including_model_and_multiline_fields() {
        let mut editor = StageEditor::new(Stage::default(), vec!["codex".into()]);
        let original = editor.finish().unwrap();
        for expected in (1..FIELDS.len()).chain(std::iter::once(0)) {
            assert_eq!(
                editor.key(KeyEvent::new(Key::Down, Mods::NONE)),
                StageRequest::None
            );
            assert_eq!(editor.selected, expected);
        }
        for expected in (0..FIELDS.len()).rev() {
            assert_eq!(
                editor.key(KeyEvent::new(Key::Up, Mods::NONE)),
                StageRequest::None
            );
            assert_eq!(editor.selected, expected);
        }
        assert_eq!(
            serde_json::to_value(editor.finish().unwrap()).unwrap(),
            serde_json::to_value(original).unwrap()
        );
    }

    #[test]
    fn entering_and_revisiting_fields_places_cursor_at_end() {
        let mut editor = StageEditor::new(
            Stage {
                name: "Example".into(),
                instructions: "first\nlast".into(),
                ..Stage::default()
            },
            vec!["codex".into()],
        );
        editor.key(KeyEvent::new(Key::Char('!'), Mods::NONE));
        assert_eq!(content(&editor.fields[0]), "Example!");
        for (next, previous) in [
            (
                KeyEvent::new(Key::Down, Mods::NONE),
                KeyEvent::new(Key::Up, Mods::NONE),
            ),
            (
                KeyEvent::new(Key::Tab, Mods::NONE),
                KeyEvent::new(Key::BackTab, Mods::NONE),
            ),
            (
                KeyEvent::new(Key::Char('n'), Mods::CONTROL),
                KeyEvent::new(Key::Char('p'), Mods::CONTROL),
            ),
            (
                KeyEvent::new(Key::Enter, Mods::NONE),
                KeyEvent::new(Key::Up, Mods::NONE),
            ),
        ] {
            editor.key(KeyEvent::new(Key::Home, Mods::NONE));
            assert_eq!(editor.fields[0].cursor(), (0, 0));
            editor.key(next);
            editor.key(previous);
            assert_eq!(editor.fields[0].cursor(), (0, 8));
        }
        editor.key(KeyEvent::new(Key::Up, Mods::NONE));
        assert_eq!(editor.selected, INSTRUCTIONS);
        assert_eq!(editor.fields[INSTRUCTIONS].cursor(), (1, 4));
        editor.key(KeyEvent::new(Key::Char('!'), Mods::NONE));
        assert_eq!(editor.finish().unwrap().instructions, "first\nlast!");
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
    fn stage_editor_wraps_instructions_without_changing_saved_lines() {
        let instructions = "abcdefghijklmnopqrstuvwxyz".repeat(3);
        let mut editor = StageEditor::new(
            Stage {
                instructions: instructions.clone(),
                ..Stage::default()
            },
            vec!["codex".into()],
        );
        editor.selected = INSTRUCTIONS;
        let palette = super::super::theme::Theme::default().palette();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 30)).unwrap();
        terminal
            .draw(|frame| editor.draw(frame, frame.area(), palette))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        // The editable pane fits one alphabet per row at this terminal width.
        assert_eq!(text.matches("abcdefghijklmnopqrstuvwxyz").count(), 1);
        assert!(text.contains("hijklmnopqrstuvwxyzabcdefg"), "{text}");
        assert_eq!(editor.finish().unwrap().instructions, instructions);
    }

    fn check(command: &[&str], required: bool, timeout_seconds: u64) -> Check {
        Check {
            command: command.iter().map(|a| (*a).into()).collect(),
            required,
            timeout_seconds,
        }
    }

    fn edit_checks(checks: Vec<Check>, text: &str) -> Result<Vec<Check>> {
        let mut editor = StageEditor::new(
            Stage {
                checks,
                ..Stage::default()
            },
            vec!["codex".into()],
        );
        editor.fields[CHECKS] = textarea(text);
        editor.finish().map(|stage| stage.checks)
    }

    #[test]
    fn router_checks_roundtrip_required_optional_and_literal_args() {
        let checks = vec![
            check(&["cargo", "test", "--locked"], true, 600),
            check(&["sh", "-c", "echo $(date) > out"], false, 600),
            check(&["npm", "run", "lint fix"], true, 600),
        ];
        let editor = StageEditor::new(
            Stage {
                checks: checks.clone(),
                ..Stage::default()
            },
            vec!["codex".into()],
        );
        let text = content(&editor.fields[CHECKS]);
        assert_eq!(
            text,
            "cargo test --locked\n? sh -c 'echo $(date) > out'\nnpm run 'lint fix'"
        );
        let result = editor.finish().unwrap().checks;
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::to_value(&checks).unwrap()
        );
        // Both optional spellings parse; blank lines are skipped.
        let parsed = edit_checks(vec![], "?make lint\n\n  ? make  'a b'  \n").unwrap();
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::to_value([
                check(&["make", "lint"], false, 600),
                check(&["make", "a b"], false, 600)
            ])
            .unwrap()
        );
    }

    #[test]
    fn router_checks_keep_timeouts_of_unchanged_lines() {
        let existing = vec![
            check(&["cargo", "test"], true, 90),
            check(&["cargo", "clippy"], false, 300),
        ];
        let result = edit_checks(
            existing,
            "cargo test\ncargo clippy\ncargo fmt --check\n? cargo test",
        )
        .unwrap();
        let timeouts: Vec<_> = result
            .iter()
            .map(|c| (c.required, c.timeout_seconds))
            .collect();
        // `cargo clippy` became required, so it is a new check with the default.
        assert_eq!(
            timeouts,
            [(true, 90), (true, 600), (true, 600), (false, 600)]
        );
    }

    #[test]
    fn empty_router_checks_field_yields_no_checks() {
        let existing = vec![check(&["cargo", "test"], true, 90)];
        assert!(edit_checks(existing.clone(), "").unwrap().is_empty());
        assert!(edit_checks(existing, " \n\n").unwrap().is_empty());
    }

    #[test]
    fn invalid_router_checks_are_editor_errors() {
        let error = edit_checks(vec![], "cargo test\necho 'open").unwrap_err();
        assert_eq!(
            error.to_string(),
            "Router check line 2 has unmatched quotes"
        );
        let error = edit_checks(vec![], "?").unwrap_err();
        assert_eq!(error.to_string(), "Router check line 1 has no command");
    }

    #[test]
    fn enter_adds_lines_to_router_checks() {
        let mut editor = StageEditor::new(Stage::default(), vec!["codex".into()]);
        editor.selected = CHECKS;
        for code in [Key::Char('a'), Key::Enter, Key::Char('b')] {
            editor.key(KeyEvent::new(code, Mods::NONE));
        }
        assert_eq!(editor.selected, CHECKS);
        assert_eq!(content(&editor.fields[CHECKS]), "a\nb");
    }

    fn render(stage: &Stage) -> String {
        let palette = super::super::theme::Theme::default().palette();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 50)).unwrap();
        terminal
            .draw(|frame| draw_stage_detail(frame, stage, None, None, None, frame.area(), palette))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    #[test]
    fn detail_panel_lists_router_checks_only_when_present() {
        let mut stage = Stage::default();
        assert!(!render(&stage).contains("Router checks"));
        stage.checks = vec![
            check(&["cargo", "test"], true, 600),
            check(&["npm", "run", "lint fix"], false, 90),
        ];
        let text = render(&stage);
        assert!(text.contains("Router checks"), "{text}");
        assert!(text.contains("cargo test  required · 10 min"), "{text}");
        assert!(
            text.contains("npm run 'lint fix'  optional · 90s"),
            "{text}"
        );
    }
}
