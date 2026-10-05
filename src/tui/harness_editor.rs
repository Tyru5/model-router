use super::{
    theme::Palette,
    widgets::{content, menu_key, textarea},
};
use crate::providers::Provider;
use crate::{
    adapters,
    catalog::Model,
    config::{Mode, Stage},
    harness_settings, storage,
};
use anyhow::{Context, Result, ensure};
use crossterm::event::{KeyCode as Key, KeyEvent, KeyModifiers as Mods};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, HighlightSpacing, List, ListItem, ListState, Padding, Paragraph, Wrap},
};
use std::collections::BTreeMap;
use tui_textarea::TextArea;

enum FieldKind {
    Model,
    Provider,
    Effort,
    Execution,
    Timeout,
    Native(&'static harness_settings::Setting),
    Arguments,
}
struct Field {
    kind: FieldKind,
    label: &'static str,
    help: &'static str,
    text: TextArea<'static>,
}
impl Field {
    fn choices(&self) -> Option<Vec<&'static str>> {
        match self.kind {
            FieldKind::Execution => Some(vec!["unattended", "interactive"]),
            FieldKind::Native(setting) => Some(
                std::iter::once("")
                    .chain(setting.choices.iter().copied())
                    .collect(),
            ),
            _ => None,
        }
    }
}

pub(super) struct HarnessEditor {
    stage: Stage,
    providers: BTreeMap<String, Provider>,
    fields: Vec<Field>,
    selected: usize,
    pub error: String,
}
impl HarnessEditor {
    pub fn new(stage: Stage, providers: BTreeMap<String, Provider>) -> Self {
        let mut fields = vec![Field {
            kind: FieldKind::Model,
            label: if stage.harness == "amp" {
                "Model / mode"
            } else {
                "Model / alias"
            },
            help: "Type a native model ID, alias, or Amp mode. Blank inherits the harness default. [F7] browses available selections without starting a model.",
            text: textarea(&stage.model),
        }];
        if harness_settings::supports_provider(&stage.harness) {
            fields.push(Field {
                kind: FieldKind::Provider,
                label: "Provider",
                help: "Blank inherits the harness provider. A provider override requires a model. [providers.NAME] entries in the workflow (openrouter, groq, … or your own) are routed by the router, reading their key from the environment; `model-router providers` lists them.",
                text: textarea(&stage.provider),
            });
        }
        if harness_settings::supports_effort(&stage.harness) {
            fields.push(Field {
                kind: FieldKind::Effort,
                label: "Reasoning effort",
                help: match stage.harness.as_str() {
                    "codex" => "Native reasoning effort, e.g. low, medium, high, xhigh. Availability depends on the model. Blank inherits the harness default.",
                    "claude" => "Native effort: low, medium, high, xhigh, max. Availability depends on the model. Blank inherits the harness default.",
                    "pi" => "Native thinking level: off, minimal, low, medium, high, xhigh. Availability depends on the model. Blank inherits the default.",
                    _ => "Native effort or model variant. Blank inherits the harness default. Accepted values depend on the model and harness.",
                },
                text: textarea(&stage.effort),
            });
        }
        fields.extend([
            Field {
                kind: FieldKind::Execution,
                label: "Execution",
                help: "unattended: run in the background without input. interactive: open the harness terminal, then inspect its handoff before advancing.",
                text: textarea(stage.mode.label()),
            },
            Field {
                kind: FieldKind::Timeout,
                label: "Timeout (seconds)",
                help: "Maximum stage runtime in seconds. Must be a positive whole number.",
                text: textarea(&stage.timeout_seconds.to_string()),
            },
        ]);
        for setting in harness_settings::settings(&stage.harness) {
            fields.push(Field {
                kind: FieldKind::Native(setting),
                label: setting.label,
                help: setting.help,
                text: textarea(
                    stage
                        .harness_settings
                        .get(setting.key)
                        .map(String::as_str)
                        .unwrap_or(""),
                ),
            });
        }
        fields.push(Field {
            kind: FieldKind::Arguments,
            label: "Extra arguments",
            help: "Additional native CLI arguments, including permissions not listed here. Shell quoting is supported; no shell is executed. Native settings remain in effect unless explicitly overridden. Use P on Workflow to preview the command.",
            text: textarea(&stage.extra_args.iter().map(|a| shell_words::quote(a).into_owned()).collect::<Vec<_>>().join(" ")),
        });
        Self {
            stage,
            providers,
            fields,
            selected: 0,
            error: String::new(),
        }
    }

    pub fn set_model(&mut self, model: Model) {
        self.error.clear();
        for field in &mut self.fields {
            match field.kind {
                FieldKind::Model => field.text = textarea(&model.id),
                FieldKind::Provider => field.text = textarea(&model.provider),
                _ => {}
            }
        }
    }

    pub fn finish(&self) -> Result<Stage> {
        let mut stage = self.stage.clone();
        for field in &self.fields {
            let value = content(&field.text).trim().to_owned();
            match field.kind {
                FieldKind::Model => stage.model = value,
                FieldKind::Provider => stage.provider = value,
                FieldKind::Effort => stage.effort = value,
                FieldKind::Execution => {
                    stage.mode = if value == "interactive" {
                        Mode::Interactive
                    } else {
                        Mode::Unattended
                    }
                }
                FieldKind::Timeout => {
                    stage.timeout_seconds = value
                        .parse()
                        .context("Timeout must be a positive whole number of seconds")?;
                    ensure!(
                        stage.timeout_seconds > 0,
                        "Timeout must be greater than zero"
                    );
                }
                FieldKind::Native(setting) => {
                    if value.is_empty() {
                        stage.harness_settings.remove(setting.key);
                    } else {
                        stage.harness_settings.insert(setting.key.into(), value);
                    }
                }
                FieldKind::Arguments => {
                    stage.extra_args = shell_words::split(&value)
                        .context("Extra arguments have unmatched quotes")?
                }
            }
        }
        adapters::validate_options(&stage, &self.providers)?;
        Ok(stage)
    }

    pub fn key(&mut self, key: KeyEvent) {
        match menu_key(key) {
            Key::Tab | Key::Down => self.selected = (self.selected + 1) % self.fields.len(),
            Key::BackTab | Key::Up => {
                self.selected = (self.selected + self.fields.len() - 1) % self.fields.len()
            }
            _ => {
                let field = &mut self.fields[self.selected];
                if let Some(choices) = field.choices() {
                    if matches!(
                        key.code,
                        Key::Left | Key::Right | Key::Char(' ') | Key::Enter
                    ) && !key.modifiers.contains(Mods::CONTROL)
                    {
                        let index = choices
                            .iter()
                            .position(|s| *s == content(&field.text))
                            .unwrap_or(0);
                        let next = if key.code == Key::Left {
                            (index + choices.len() - 1) % choices.len()
                        } else {
                            (index + 1) % choices.len()
                        };
                        field.text = textarea(choices[next]);
                        self.error.clear();
                    }
                } else if key.code == Key::Enter {
                    self.selected = (self.selected + 1) % self.fields.len();
                } else if field.text.input(key) {
                    self.error.clear();
                }
            }
        }
    }

    pub fn paste(&mut self, text: &str) {
        let field = &mut self.fields[self.selected];
        if field.choices().is_none() {
            field.text.insert_str(text.replace(['\r', '\n'], " "));
            self.error.clear();
        }
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, rect: Rect, palette: Palette) {
        let block = palette.modal(
            &format!(
                "Harness settings · {} · {}",
                self.stage.harness, self.stage.name
            ),
            &[("Tab", "next field"), ("←/→", "choices"), ("F7", "models")],
        );
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(if self.error.is_empty() { 1 } else { 3 }),
            Constraint::Length(if rect.width < 90 { 3 } else { 2 }),
        ])
        .split(inner);
        frame.render_widget(
            Paragraph::new("Applies to this stage. Harness default inherits native settings.")
                .style(Style::default().fg(palette.muted)),
            chunks[0],
        );
        let halves = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(chunks[1]);
        let items = self.fields.iter().map(|field| {
            let value = content(&field.text);
            ListItem::new(vec![
                Line::styled(field.label, Style::default().fg(palette.muted)),
                Line::styled(
                    storage::truncate(
                        if value.is_empty() {
                            if matches!(field.kind, FieldKind::Arguments) {
                                "None"
                            } else {
                                "Harness default"
                            }
                        } else {
                            &value
                        },
                        halves[0].width.saturating_sub(5) as usize,
                    ),
                    Style::default().fg(if value.is_empty() {
                        palette.muted
                    } else {
                        palette.fg
                    }),
                ),
            ])
        });
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(Style::default().bg(palette.selection_bg))
                .highlight_symbol("▌ ")
                .highlight_spacing(HighlightSpacing::Always)
                .block(Block::default().padding(Padding::right(2))),
            halves[0],
            &mut ListState::default().with_selected(Some(self.selected)),
        );
        let field = &mut self.fields[self.selected];
        let detail = Layout::vertical([
            Constraint::Length(
                field
                    .choices()
                    .map(|choices| choices.len() as u16 + 2)
                    .unwrap_or(3),
            ),
            Constraint::Min(1),
        ])
        .split(halves[1]);
        if let Some(choices) = field.choices() {
            let current = content(&field.text);
            let index = choices.iter().position(|v| *v == current).unwrap_or(0);
            frame.render_stateful_widget(
                List::new(choices.iter().map(|value| {
                    ListItem::new(if value.is_empty() {
                        "Harness default"
                    } else {
                        value
                    })
                }))
                .block(palette.panel(format!(" {} ", field.label)))
                .highlight_style(palette.selected_style()),
                detail[0],
                &mut ListState::default().with_selected(Some(index)),
            );
        } else {
            palette.style_textarea(&mut field.text);
            field.text.set_block(
                palette
                    .panel(format!(" {} ", field.label))
                    .border_style(Style::default().fg(palette.accent)),
            );
            frame.render_widget(&field.text, detail[0]);
        }
        frame.render_widget(
            Paragraph::new(field.help)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(palette.muted)),
            detail[1],
        );
        frame.render_widget(
            Paragraph::new(storage::clean_terminal(&self.error))
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(palette.red)),
            chunks[2],
        );
        let mut keys = palette.hints(&[
            ("Tab / Ctrl+N/P", "fields"),
            ("Ctrl+S", "apply"),
            ("Esc", "cancel"),
        ]);
        keys.spans.push(Span::styled(
            "   ·  s on Workflow saves",
            Style::default().fg(palette.muted),
        ));
        frame.render_widget(Paragraph::new(keys).wrap(Wrap { trim: false }), chunks[3]);
    }
}
