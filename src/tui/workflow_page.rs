use super::{
    actions::ActionId,
    app::App,
    harness_editor::HarnessEditor,
    modal::{Choice, InputTarget, Modal, PickValue},
    path_picker::PathPicker,
    task_importer::TaskImporter,
    theme::Palette,
    widgets::{content, kv, or_default, textarea},
};
use crate::{
    adapters, catalog,
    config::{Mode, Role, Stage},
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
use tui_textarea::TextArea;

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
pub(super) struct StageEditor {
    stage: Stage,
    pub(super) fields: Vec<TextArea<'static>>,
    selected: usize,
    harnesses: Vec<String>,
}
impl StageEditor {
    pub(super) fn new(stage: Stage, harnesses: Vec<String>) -> Self {
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
        stage.instructions = content(&self.fields[10]);
        Ok(stage)
    }
    pub(super) fn key(&mut self, key: KeyEvent) {
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
    pub(super) fn paste(&mut self, text: &str) {
        if ![1, 2, 6].contains(&self.selected) {
            self.fields[self.selected].insert_str(text);
        }
    }
    pub(super) fn draw(&mut self, frame: &mut Frame<'_>, rect: Rect, palette: Palette) {
        let block = palette.modal(
            "Edit stage",
            &[("Tab", "next field"), ("←/→", "cycle choice")],
        );
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let chunks = Layout::vertical([Constraint::Min(8), Constraint::Length(2)]).split(inner);
        let halves = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(chunks[0]);
        let value_width = halves[0].width.saturating_sub(4) as usize;
        let rows = FIELDS.iter().enumerate().map(|(i, label)| {
            let value = content(&self.fields[i]).replace('\n', " ");
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
            &mut ListState::default().with_selected(Some(self.selected)),
        );
        let field = &mut self.fields[self.selected];
        palette.style_textarea(field);
        field.set_block(
            palette
                .panel(format!(" {} ", FIELDS[self.selected]))
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
}

impl App {
    pub(super) fn perform_workflow(&mut self, id: ActionId) -> Result<()> {
        let count = self.settings.workflows[self.workflow].stages.len();
        match id {
            ActionId::NextStage => self.selected = (self.selected + 1).min(count - 1),
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
                })
            }
            ActionId::HarnessSettings => {
                self.modal = Some(Modal::Harness(Box::new(HarnessEditor::new(
                    self.stage().clone(),
                ))))
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
                self.settings.workflows[self.workflow]
                    .stages
                    .remove(self.selected);
                self.selected = self.selected.min(count - 2);
                self.dirty = true;
            }
            ActionId::DeleteWorkflow => {
                ensure!(
                    self.settings.workflows.len() > 1,
                    "Keep at least one workflow"
                );
                self.settings.workflows.remove(self.workflow);
                self.workflow = 0;
                self.selected = 0;
                self.dirty = true;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

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
