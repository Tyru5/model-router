use super::input::InputField;
use super::{
    actions::ActionId,
    app::App,
    harness_editor::HarnessEditor,
    modal::{Ask, Choice, InputTarget, Modal, PickValue},
    path_picker::PathPicker,
    task_importer::TaskImporter,
    theme::Palette,
    widgets::{
        StripStage, content, fit, focus_textarea, kv, or_default, rows_for, stage_strip, textarea,
        wrapped_textarea,
    },
    workflow_editor::{WorkflowEditor, format_usd_field, parse_budget, parse_nonempty_lines},
};
use crate::{
    adapters, catalog,
    config::{
        Billing, BudgetAction, Candidate, Check, FanOut, Mode, Role, Sandbox, SandboxFallback,
        Stage,
    },
    delegation::Delegation,
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
    widgets::{Block, HighlightSpacing, List, ListItem, ListState, Padding, Paragraph, Wrap},
};
use tui_textarea::CursorMove;
use unicode_width::UnicodeWidthStr;

const FIELDS: &[&str] = &[
    "Name",
    "Role",
    "Harness",
    "Model / alias / mode",
    "Provider (blank = native config)",
    "Reasoning effort",
    "Execution",
    "Billing (default = infer)",
    "Timeout (seconds)",
    "Stage budget max USD (blank = none)",
    "Stage budget max tokens (blank = none)",
    "Stage budget on exceed",
    "Review repair target (stage ID; blank = nearest)",
    "Extra arguments (shell quoting; no shell execution)",
    "Run after (stage IDs, comma separated; blank = list order)",
    "Fan-out candidates (one per line: [id:] harness [model] [provider] [effort])",
    "Router checks (one per line; ? prefix = optional)",
    "Require router checks (test stages only)",
    "Router sandbox (inherit = workflow default)",
    "Sandbox writable paths (one per line; ~/ expands)",
    "Sandbox network access",
    "Sandbox fallback",
    "Delegation (TOML; blank = off)",
    "Stage instructions",
    "Ship options (TOML; ship role only)",
];
const BILLING: usize = 7;
const TIMEOUT: usize = 8;
const BUDGET_USD: usize = 9;
const BUDGET_TOKENS: usize = 10;
const BUDGET_ACTION: usize = 11;
const REPAIR_TARGET: usize = 12;
const EXTRA_ARGS: usize = 13;
const AFTER: usize = 14;
/// Multiline fields, where Enter inserts a newline.
const FAN_OUT: usize = 15;
const CHECKS: usize = 16;
const REQUIRE_CHECKS: usize = 17;
const SANDBOX: usize = 18;
const SANDBOX_WRITABLE: usize = 19;
const SANDBOX_NETWORK: usize = 20;
const SANDBOX_FALLBACK: usize = 21;
const DELEGATION: usize = 22;
const INSTRUCTIONS: usize = 23;
const SHIP: usize = 24;
const MULTILINE: [usize; 6] = [
    CHECKS,
    SANDBOX_WRITABLE,
    DELEGATION,
    INSTRUCTIONS,
    FAN_OUT,
    SHIP,
];
const CHOICES: [usize; 9] = [
    1,
    2,
    6,
    BILLING,
    BUDGET_ACTION,
    REQUIRE_CHECKS,
    SANDBOX,
    SANDBOX_NETWORK,
    SANDBOX_FALLBACK,
];
/// The sandbox choice that keeps the workflow's `sandbox_default`.
const INHERIT: &str = "inherit";
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
    list_state: ListState,
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
        let billing = stage.billing.map_or("default", Billing::label).to_owned();
        let budget_usd = stage
            .budget
            .max_usd
            .map(format_usd_field)
            .unwrap_or_default();
        let budget_tokens = stage
            .budget
            .max_tokens
            .map(|n| n.to_string())
            .unwrap_or_default();
        let budget_action = stage.budget.on_exceed.label().to_owned();
        let require_checks = if stage.require_router_checks {
            "on"
        } else {
            "off"
        }
        .to_owned();
        let sandbox = stage.sandbox.map_or(INHERIT, Sandbox::label).to_owned();
        let sandbox_writable = stage.sandbox_writable.join("\n");
        let sandbox_network = if stage.sandbox_network { "on" } else { "off" }.to_owned();
        let sandbox_fallback = match stage.sandbox_fallback {
            SandboxFallback::Refuse => "refuse",
            SandboxFallback::Warn => "warn",
        }
        .to_owned();
        let delegation = delegation_text(&stage);
        let ship = ship_text(&stage);
        let after = stage.after.join(", ");
        let fan_out = stage.fan_out.as_ref().map(fan_out_text).unwrap_or_default();
        let fields = [
            &stage.name,
            stage.role.label(),
            &stage.harness,
            &stage.model,
            &stage.provider,
            &stage.effort,
            stage.mode.label(),
            &billing,
            &stage.timeout_seconds.to_string(),
            &budget_usd,
            &budget_tokens,
            &budget_action,
            &stage.repair_target,
            &args,
            &after,
            &fan_out,
            &checks,
            &require_checks,
            &sandbox,
            &sandbox_writable,
            &sandbox_network,
            &sandbox_fallback,
            &delegation,
            &stage.instructions,
            &ship,
        ]
        .into_iter()
        .map(textarea)
        .collect();
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            stage,
            fields,
            selected: 0,
            list_state,
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
            BILLING => vec!["default".into(), "api".into(), "subscription".into()],
            BUDGET_ACTION => vec![
                BudgetAction::Stop.label().into(),
                BudgetAction::Warn.label().into(),
            ],
            REQUIRE_CHECKS => vec!["off".into(), "on".into()],
            SANDBOX => std::iter::once(INHERIT)
                .chain(Sandbox::ALL.map(Sandbox::label))
                .map(String::from)
                .collect(),
            SANDBOX_NETWORK => vec!["on".into(), "off".into()],
            SANDBOX_FALLBACK => vec!["refuse".into(), "warn".into()],
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
        stage.billing = match content(&self.fields[BILLING]).trim() {
            "api" => Some(Billing::Api),
            "subscription" => Some(Billing::Subscription),
            _ => None,
        };
        stage.timeout_seconds = content(&self.fields[TIMEOUT])
            .trim()
            .parse()
            .context("Timeout must be a positive number of seconds")?;
        stage.budget = parse_budget(
            &content(&self.fields[BUDGET_USD]),
            &content(&self.fields[BUDGET_TOKENS]),
            &content(&self.fields[BUDGET_ACTION]),
            "Stage",
        )?;
        stage.repair_target = content(&self.fields[REPAIR_TARGET]).trim().into();
        stage.extra_args = shell_words::split(&content(&self.fields[EXTRA_ARGS]))
            .context("Extra arguments have unmatched quotes")?;
        stage.checks = parse_checks(&content(&self.fields[CHECKS]), &self.stage.checks)?;
        stage.require_router_checks = content(&self.fields[REQUIRE_CHECKS]) == "on";
        stage.instructions = content(&self.fields[INSTRUCTIONS]);
        let sandbox = content(&self.fields[SANDBOX]);
        stage.sandbox = Sandbox::ALL.into_iter().find(|s| s.label() == sandbox);
        stage.sandbox_writable = parse_nonempty_lines(&content(&self.fields[SANDBOX_WRITABLE]));
        stage.sandbox_network = content(&self.fields[SANDBOX_NETWORK]) != "off";
        stage.sandbox_fallback = if content(&self.fields[SANDBOX_FALLBACK]) == "warn" {
            SandboxFallback::Warn
        } else {
            SandboxFallback::Refuse
        };
        stage.delegation = parse_delegation(&content(&self.fields[DELEGATION]))?;
        stage.ship = parse_ship(stage.role, &content(&self.fields[SHIP]))?;
        // A ship stage without options already means the defaults.
        if self.stage.ship.is_none() && stage.ship == Some(crate::ship::Options::default()) {
            stage.ship = None;
        }
        stage.after = content(&self.fields[AFTER])
            .split([',', ' '])
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect();
        stage.fan_out = parse_fan_out(&content(&self.fields[FAN_OUT]))?;
        Ok(stage)
    }
    fn focus(&mut self, index: usize) {
        self.selected = index;
        self.list_state.select(Some(index));
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
        if !MULTILINE.contains(&self.selected) || !key.modifiers.is_empty() {
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
                    for field in &mut self.fields[MODEL..=EFFORT] {
                        *field = textarea("");
                    }
                    self.catalog = None;
                    return StageRequest::Catalog;
                }
            }
        } else if key.code == Key::Enter && !MULTILINE.contains(&self.selected) {
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
        let title = format!("Edit stage ({}/{})", self.selected + 1, FIELDS.len());
        let block = palette.modal(
            &title,
            &[("↑/↓ / Tab", "switch field"), ("←/→", "cycle choice")],
        );
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let chunks = Layout::vertical([Constraint::Min(8), Constraint::Length(2)]).split(inner);
        let halves = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(chunks[0]);
        let value_width = halves[0].width.saturating_sub(4) as usize;
        let rows: Vec<ListItem<'static>> = FIELDS
            .iter()
            .enumerate()
            .map(|(i, label)| {
                let limit = value_width;
                let value = field_preview(&self.fields[i], limit);
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
            })
            .collect();
        self.list_state.select(Some(self.selected));
        frame.render_stateful_widget(
            List::new(rows)
                .highlight_style(palette.highlight_style())
                .highlight_symbol("▌ ")
                .highlight_spacing(HighlightSpacing::Always)
                .block(Block::default().padding(Padding::right(2))),
            halves[0],
            &mut self.list_state,
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
        } else if self.is_choice(self.selected) {
            let choices = self.choices(self.selected);
            let current = content(&self.fields[self.selected]);
            let index = choices.iter().position(|v| *v == current).unwrap_or(0);
            let list_height =
                (choices.len() as u16 + 2).min(halves[1].height.saturating_sub(2).max(3));
            let panes = Layout::vertical([Constraint::Length(list_height), Constraint::Min(1)])
                .split(halves[1]);
            frame.render_stateful_widget(
                List::new(choices.iter().map(|v| ListItem::new(Line::raw(v.clone()))))
                    .block(
                        palette
                            .panel(format!(" {} ", FIELDS[self.selected]))
                            .border_style(Style::default().fg(palette.accent)),
                    )
                    .highlight_style(palette.selected_style()),
                panes[0],
                &mut ListState::default().with_selected(Some(index)),
            );
            frame.render_widget(
                Paragraph::new(stage_choice_help(self.selected))
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
            } else if self.selected == REQUIRE_CHECKS {
                "   ·  test stages only: requires a passing router check instead of model-reported pass"
            } else if self.selected == BILLING {
                "   ·  default infers api for routed providers, subscription for native harnesses"
            } else if self.selected == BUDGET_USD || self.selected == BUDGET_TOKENS {
                "   ·  caps spend across this stage's attempts; blank disables"
            } else if self.selected == SANDBOX_WRITABLE {
                "   ·  one writable path per line (~/ expands); requires project trust"
            } else if self.selected == DELEGATION {
                "   ·  TOML: delegates, harnesses, models, providers, max_tasks, runs, schedules; blank = off"
            } else if self.selected == SHIP {
                "   ·  e.g. push = true, draft_pr = true; blank = defaults (commit only, confirm)"
            } else if self.selected == AFTER {
                "   ·  any stage with after makes the workflow a graph"
            } else if self.selected == FAN_OUT {
                "   ·  - leaves a value blank; a judge = auto|<review id> line picks the winner"
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

fn stage_choice_help(index: usize) -> &'static str {
    match index {
        BILLING => {
            "default infers api for routed providers, subscription for native accounts. ←/→ or Space cycles."
        }
        BUDGET_ACTION => {
            "stop blocks before the next stage when a cap is exceeded; warn logs a warning and continues."
        }
        REQUIRE_CHECKS => {
            "When on (test stages only), model-reported passes never satisfy the gate without a passing required router check."
        }
        SANDBOX => "inherit uses the workflow's sandbox_default. ←/→ or Space cycles.",
        SANDBOX_NETWORK => "off cuts the sandboxed harness off the network. ←/→ or Space cycles.",
        SANDBOX_FALLBACK => {
            "refuse fails if the sandbox cannot be set up; warn runs unsandboxed with a warning (requires trust)."
        }
        _ => "←/→ or Space cycles through options.",
    }
}

/// A stage's delegation policy as TOML lines; blank when delegation is off.
fn delegation_text(stage: &Stage) -> String {
    let Some(delegation) = &stage.delegation else {
        return String::new();
    };
    toml::to_string(delegation)
        .unwrap_or_default()
        .trim_end()
        .to_owned()
}

/// Parses the delegation TOML block; blank disables delegation.
fn parse_delegation(text: &str) -> Result<Option<Delegation>> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    let delegation: Delegation =
        toml::from_str(text).map_err(|e| anyhow::anyhow!("Delegation: {}", e.message()))?;
    Ok(Some(delegation))
}

/// Single-line prefix of `field` without copying the full multiline buffer.
fn field_preview(field: &InputField<'_>, max_chars: usize) -> String {
    let lines = field.lines();
    let cap = max_chars.saturating_add(1);
    let mut out = String::new();
    let mut chars = 0usize;
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            if chars >= cap {
                break;
            }
            out.push(' ');
            chars += 1;
        }
        for ch in line.chars() {
            if chars >= cap {
                return out;
            }
            out.push(ch);
            chars += 1;
        }
    }
    out
}

/// A ship stage's options as TOML lines; empty for other stages.
fn ship_text(stage: &Stage) -> String {
    if stage.role != Role::Ship && stage.ship.is_none() {
        return String::new();
    }
    toml::to_string(&stage.ship.clone().unwrap_or_default())
        .unwrap_or_default()
        .trim_end()
        .to_owned()
}

/// Parses the ship options field. Blank means the defaults for a ship stage
/// and none for other roles.
fn parse_ship(role: Role, text: &str) -> Result<Option<crate::ship::Options>> {
    if text.trim().is_empty() {
        return Ok((role == Role::Ship).then(crate::ship::Options::default));
    }
    ensure!(
        role == Role::Ship,
        "Ship options apply to ship stages only; clear the field or choose the ship role"
    );
    let options: crate::ship::Options =
        toml::from_str(text).map_err(|e| anyhow::anyhow!("Ship options: {}", e.message()))?;
    options.validate()?;
    Ok(Some(options))
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

/// The fan-out field: a `judge = …` line, then one candidate per line.
fn fan_out_text(fan: &FanOut) -> String {
    let mut lines = vec![format!("judge = {}", fan.judge)];
    for candidate in &fan.candidates {
        let mut words = vec![];
        if !candidate.id.is_empty() {
            words.push(format!("{}:", candidate.id));
        }
        words.push(shell_words::quote(&candidate.harness).into_owned());
        let values = [&candidate.model, &candidate.provider, &candidate.effort];
        let used = values
            .iter()
            .rposition(|v| !v.is_empty())
            .map_or(0, |i| i + 1);
        for value in &values[..used] {
            words.push(if value.is_empty() {
                "-".into()
            } else {
                shell_words::quote(value).into_owned()
            });
        }
        lines.push(words.join(" "));
    }
    lines.join("\n")
}

/// Parses the fan-out field; blank means no fan-out.
fn parse_fan_out(text: &str) -> Result<Option<FanOut>> {
    let mut judge = crate::config::AUTO_JUDGE.to_owned();
    let mut candidates = vec![];
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("judge") {
            let value = rest.trim_start().trim_start_matches(['=', ':']).trim();
            ensure!(
                !value.is_empty(),
                "Fan-out line {} needs a judge",
                number + 1
            );
            judge = value.to_owned();
            continue;
        }
        let mut words = shell_words::split(line)
            .with_context(|| format!("Fan-out line {} has unmatched quotes", number + 1))?
            .into_iter();
        let mut candidate = Candidate::default();
        let mut first = words.next().unwrap_or_default();
        if let Some(id) = first.strip_suffix(':') {
            candidate.id = id.to_owned();
            first = words.next().unwrap_or_default();
        }
        ensure!(
            !first.is_empty(),
            "Fan-out line {} needs a harness",
            number + 1
        );
        candidate.harness = first;
        let value = |word: Option<String>| word.filter(|w| w != "-").unwrap_or_default();
        candidate.model = value(words.next());
        candidate.provider = value(words.next());
        candidate.effort = value(words.next());
        ensure!(
            words.next().is_none(),
            "Fan-out line {} has more than harness, model, provider, and effort",
            number + 1
        );
        candidates.push(candidate);
    }
    Ok((!candidates.is_empty()).then_some(FanOut { candidates, judge }))
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
        let count = self.editor.settings.workflows[self.editor.workflow]
            .stages
            .len();
        match id {
            ActionId::NextStage => {
                self.editor.selected = (self.editor.selected + 1).min(count.saturating_sub(1))
            }
            ActionId::PreviousStage => {
                self.editor.selected = self.editor.selected.saturating_sub(1)
            }
            ActionId::ToggleStage => {
                let stage = self.editor.stage_mut();
                stage.enabled = !stage.enabled;
                self.editor.dirty = true;
            }
            ActionId::ToggleMode => {
                let stage = self.editor.stage_mut();
                stage.mode = if stage.mode == Mode::Interactive {
                    Mode::Unattended
                } else {
                    Mode::Interactive
                };
                self.editor.dirty = true;
            }
            ActionId::EditTask => self.input(
                "Task · [Ctrl+S] applies · [Enter] adds a line",
                InputTarget::Task,
                &self.editor.task.clone(),
            ),
            ActionId::Repositories => {
                self.ui.open(Modal::Repositories(Box::new(PathPicker::new(
                    &self.project,
                    storage::recent_projects(),
                    self.tasks.spawner(),
                )?)));
            }
            ActionId::ImportTask => {
                self.ui.open(Modal::Import(Box::new(TaskImporter::new(
                    &self.project,
                    &self.editor.task,
                    self.tasks.spawner(),
                )?)));
            }
            ActionId::ChooseWorkflow => {
                let mut items: Vec<_> = self
                    .editor
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
                self.ui.open(Modal::Picker {
                    title: "Choose workflow".into(),
                    items,
                    selected: self.editor.workflow,
                    filter: textarea(""),
                    note: "Presets add an editable workflow. Reviews start disabled, as does ship; existing workflows are kept."
                        .into(),
                });
            }
            ActionId::RenameWorkflow => self.input(
                "Rename workflow",
                InputTarget::RenameWorkflow,
                &self.editor.settings.workflows[self.editor.workflow]
                    .name
                    .clone(),
            ),
            ActionId::WorkflowSettings => {
                let workflow = self.editor.settings.workflows[self.editor.workflow].clone();
                self.ui
                    .open(Modal::Workflow(Box::new(WorkflowEditor::new(workflow))));
            }
            ActionId::RepairLimit => self.input(
                "Maximum automatic repair passes (0 disables repairs)",
                InputTarget::RepairLimit,
                &self.editor.settings.workflows[self.editor.workflow]
                    .max_repair_passes
                    .to_string(),
            ),
            ActionId::PickHarness => {
                let items = catalog::known_harnesses(&self.editor.settings)
                    .into_iter()
                    .map(|name| {
                        let installed = self
                            .harnesses
                            .checks
                            .report(&name)
                            .map(|r| r.installed())
                            .unwrap_or_else(|| {
                                adapters::executable(&adapters::program(
                                    &name,
                                    &self.editor.settings.adapters,
                                ))
                                .is_some()
                            });
                        Choice {
                            label: format!(
                                "{} · {}",
                                name,
                                if installed {
                                    "installed"
                                } else {
                                    "not installed"
                                }
                            ),
                            value: PickValue::Harness(name),
                        }
                    })
                    .collect();
                self.ui.open(Modal::Picker {
                    title: "Harness for selected stage".into(),
                    items,
                    selected: 0,
                    filter: textarea(""),
                    note: "Changing harness clears model/provider/effort and native settings; extra arguments are kept."
                        .into(),
                });
            }
            ActionId::EditStage => {
                self.ui.open(Modal::Stage {
                    editor: Box::new(StageEditor::new(
                        self.editor.stage().clone(),
                        catalog::known_harnesses(&self.editor.settings),
                    )),
                    insert_at: None,
                });
                self.request_catalog(self.editor.stage().harness.clone());
            }
            ActionId::HarnessSettings => {
                self.ui.open(Modal::Harness(Box::new(HarnessEditor::new(
                    self.editor.stage().clone(),
                    self.editor.settings.providers.clone(),
                ))));
                self.request_catalog(self.editor.stage().harness.clone());
            }
            ActionId::Models => self.model_picker_for(self.editor.stage().harness.clone()),
            ActionId::AddStage => {
                ensure!(count < 64, "A workflow can contain at most 64 stages");
                self.ui.open(Modal::Picker {
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
                let mut copy = self.editor.stage().clone();
                copy.id = Stage::default().id;
                copy.name.push_str(" copy");
                self.editor.settings.workflows[self.editor.workflow]
                    .stages
                    .insert(self.editor.selected + 1, copy);
                self.editor.selected += 1;
                self.editor.dirty = true;
            }
            ActionId::RemoveStage => {
                ensure!(count > 1, "Keep at least one stage");
                let name = self.editor.stage().name.clone();
                self.editor.remember(format!("stage “{name}”"));
                self.editor.settings.workflows[self.editor.workflow]
                    .stages
                    .remove(self.editor.selected);
                self.editor.selected = self.editor.selected.min(count - 2);
                self.editor.dirty = true;
                self.ui.message = format!("Removed stage “{name}” — [u] to undo.");
            }
            ActionId::DeleteWorkflow => {
                ensure!(
                    self.editor.settings.workflows.len() > 1,
                    "Keep at least one workflow"
                );
                self.ui.open(Modal::Ask(Box::new(Ask::DeleteWorkflow {
                    index: self.editor.workflow,
                })));
            }
            ActionId::Undo => self.ui.message = self.editor.restore(),
            ActionId::MoveStageUp => {
                if self.editor.selected > 0 {
                    self.editor.settings.workflows[self.editor.workflow]
                        .stages
                        .swap(self.editor.selected, self.editor.selected - 1);
                    self.editor.selected -= 1;
                    self.editor.dirty = true;
                }
            }
            ActionId::MoveStageDown => {
                if self.editor.selected + 1 < count {
                    self.editor.settings.workflows[self.editor.workflow]
                        .stages
                        .swap(self.editor.selected, self.editor.selected + 1);
                    self.editor.selected += 1;
                    self.editor.dirty = true;
                }
            }
            ActionId::ToggleWorktree => self.editor.worktree = !self.editor.worktree,
            ActionId::Save => self.save(false)?,
            ActionId::SaveGlobal => self.save(true)?,
            ActionId::Preview => self.text(
                "Launch preview · no model is started",
                engine::preview(
                    &self.editor.settings,
                    &self.editor.settings.workflows[self.editor.workflow],
                    &self.project,
                    &self.editor.task,
                )?,
            ),
            ActionId::Run => {
                // Creating a worktree and snapshot can take a while, so the
                // run starts in the background; the editor stays usable.
                let (project, settings, task) = (
                    self.project.clone(),
                    self.editor.settings.clone(),
                    self.editor.task.clone(),
                );
                let workflow = settings.workflows[self.editor.workflow].clone();
                crate::config::validate_task(&task)?;
                self.start_run(
                    move |placement| {
                        Run::create_at(&project, &settings, &workflow, task, placement)
                    },
                    self.editor.worktree,
                    Some(self.editor.task.clone()),
                    None,
                )?;
            }
            other => bail!("{other:?} is not a workflow action"),
        }
        Ok(())
    }
}

const NO_TASK: &str =
    "No task yet. Press [t] to write one, or [f] to import a file, URL, or GitHub issue.";

/// Whether the task is being edited in its panel on this page.
pub(super) fn editing_task(app: &App) -> bool {
    matches!(
        app.ui.dialog(),
        Some(Modal::Input {
            target: InputTarget::Task,
            ..
        })
    )
}

/// The next run: its workflow, task, a strip of its stages, and the
/// selected stage's settings below. Returns the task panel's inner area,
/// where the app draws the task editor while it is open.
pub(super) fn draw_workflow(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    palette: Palette,
) -> Rect {
    let workflow = &app.editor.settings.workflows[app.editor.workflow];
    let editing = editing_task(app);
    let width = area.width.saturating_sub(4).max(1);
    let task_text = storage::clean_terminal(&app.editor.task);
    // With no task, the hint takes the rows.
    let task_rows: u16 = if task_text.is_empty() {
        NO_TASK
    } else {
        &task_text
    }
    .lines()
    .map(|line| rows_for(&Line::raw(line.to_owned()), width, u16::MAX))
    .fold(0, u16::saturating_add)
    .max(1);
    // The task takes what it needs, up to a third of the page; the editor
    // gets room to type.
    let most = (area.height / 3).max(3);
    let task_height = if editing {
        (area.height * 2 / 5).max(5)
    } else {
        task_rows.min(most) + 2
    };
    let roomy = area.height >= 26;
    let [heading, _, task, _, label, strip, rule, detail, summary] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(u16::from(roomy)),
        Constraint::Length(task_height),
        Constraint::Length(u16::from(roomy)),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);

    // Heading: what this page is, and which workflow it runs. A long name is
    // cut so it never runs into the title.
    let mut rest = vec![];
    if app.editor.dirty {
        rest.push(Span::styled(
            "  ● unsaved",
            Style::default().fg(palette.warn),
        ));
    }
    rest.push(Span::raw("   "));
    rest.extend(palette.hints(&[("w", "switch")]).spans);
    let fixed = "New run  Workflow  ".width() + Line::from(rest.clone()).width();
    let mut right = vec![
        Span::styled("Workflow  ", Style::default().fg(palette.muted)),
        Span::styled(
            fit(
                &workflow.name,
                usize::from(heading.width).saturating_sub(fixed),
            ),
            Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
        ),
    ];
    right.extend(rest);
    frame.render_widget(Paragraph::new(Line::from(right)).right_aligned(), heading);
    frame.render_widget(
        Paragraph::new(Line::styled(
            "New run",
            Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
        )),
        heading,
    );

    // Task.
    let block = if editing {
        palette
            .panel_with("Task", &[("Ctrl+S", "done"), ("Esc", "cancel")])
            .border_style(Style::default().fg(palette.accent))
    } else {
        palette.panel_with("Task", &[("t", "edit"), ("f", "import")])
    };
    let task_inner = block.inner(task);
    if editing {
        frame.render_widget(block, task);
    } else {
        let body = if task_text.is_empty() {
            Paragraph::new(Line::styled(
                NO_TASK,
                Style::default()
                    .fg(palette.muted)
                    .add_modifier(Modifier::ITALIC),
            ))
        } else {
            Paragraph::new(task_text).style(Style::default().fg(palette.fg))
        };
        frame.render_widget(body.wrap(Wrap { trim: false }).block(block), task);
    }

    // Stages: a strip, then the selected one's settings.
    let selected = app.editor.selected;
    let count = workflow.stages.len();
    frame.render_widget(
        Paragraph::new(stage_hints(palette, label.width)).right_aligned(),
        label,
    );
    frame.render_widget(
        Paragraph::new(Line::styled("Stages", palette.header_style())),
        label,
    );
    let stages: Vec<StripStage> = workflow
        .stages
        .iter()
        .map(|stage| StripStage {
            glyph: if stage.enabled { "●" } else { "○" },
            color: if stage.enabled {
                palette.accent
            } else {
                palette.muted
            },
            name: stage.name.clone(),
            on: stage.enabled,
        })
        .collect();
    frame.render_widget(
        Paragraph::new(stage_strip(&stages, selected, strip.width.into(), palette)),
        strip,
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            "─".repeat(rule.width.into()),
            Style::default().fg(palette.line),
        )),
        rule,
    );
    let stage = app.editor.stage();
    let warning = app.harnesses.checks.warning(&stage.harness);
    let effort = app
        .catalogs
        .cached
        .get(&stage.harness)
        .and_then(|c| c.efforts_for(&stage.model, &stage.provider))
        .map(|efforts| effort_note(stage, &efforts));
    let notes = StageNotes {
        sandbox: stage.effective_sandbox(workflow),
        warning,
        effort_note: effort,
        hint: app.stats_hint(stage),
    };
    draw_stage_detail(frame, stage, (selected + 1, count), notes, detail, palette);

    // What the run will be.
    let enabled = workflow.stages.iter().filter(|s| s.enabled).count();
    frame.render_widget(
        Paragraph::new(Line::styled(
            format!(
                "{enabled} of {count} stages on   ·   repairs up to {}   ·   {}",
                workflow.max_repair_passes,
                if app.editor.worktree {
                    "isolated worktree"
                } else {
                    "current directory"
                },
            ),
            Style::default().fg(palette.muted),
        )),
        summary,
    );
    task_inner
}

/// The stage keys beside the "Stages" label, as many as fit after it.
fn stage_hints(palette: Palette, width: u16) -> Line<'static> {
    let keys = [
        ("←→", "stage"),
        ("e", "edit"),
        ("c", "harness"),
        ("o", "on/off"),
        ("a", "add"),
    ];
    let room = usize::from(width).saturating_sub("Stages".width() + 2);
    (0..=keys.len())
        .rev()
        .map(|count| palette.hints(&keys[..count]))
        .find(|line| line.width() <= room)
        .unwrap_or_default()
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

/// Derived facts shown beside a stage's own settings.
struct StageNotes {
    /// The stage's effective router sandbox.
    sandbox: Sandbox,
    /// Missing harness flags from cached doctor results.
    warning: Option<String>,
    /// Whether the catalog lists the stage's effort level.
    effort_note: Option<(String, bool)>,
    /// A routing suggestion from run history.
    hint: Option<String>,
}

/// The selected stage under the strip: its name, harness, model, and
/// execution, then its settings (in two columns when wide), instructions,
/// checks, and harness settings. `position` is its number and the count.
fn draw_stage_detail(
    frame: &mut Frame<'_>,
    stage: &Stage,
    position: (usize, usize),
    notes: StageNotes,
    area: Rect,
    palette: Palette,
) {
    let StageNotes {
        sandbox,
        warning,
        effort_note,
        hint,
    } = notes;
    let muted = Style::default().fg(palette.muted);
    let fg = Style::default().fg(palette.fg);
    let model = or_default(&stage.model, "harness default");
    let provider = or_default(&stage.provider, "harness default");
    let effort = or_default(&stage.effort, "harness default");

    let mut summary = vec![
        Span::styled(
            stage.name.clone(),
            Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
    ];
    if let Some(fan) = &stage.fan_out {
        summary.push(Span::styled(
            format!("fan-out · {} candidates", fan.candidates.len()),
            fg,
        ));
    } else {
        summary.push(Span::styled(format!("{}  {}", stage.harness, model.0), fg));
    }
    summary.push(Span::styled(
        format!("  {}", stage.mode.label()),
        Style::default().fg(if stage.mode == Mode::Interactive {
            palette.blue
        } else {
            palette.fg
        }),
    ));
    if !stage.enabled {
        summary.push(Span::styled("   ○ off", muted));
    }
    let title = Line::from(summary);
    let place = Line::styled(format!("stage {} of {}", position.0, position.1), muted);

    // Missing harness flags get their own row, wrapped, above the settings.
    let warning = warning.map(|text| Line::styled(text, Style::default().fg(palette.warn)));
    let mut facts = vec![kv(palette, "Role", stage.role.label(), true)];
    facts.extend([
        kv(palette, "Model", model.0, model.1),
        kv(
            palette,
            "Sandbox",
            sandbox.label(),
            stage.sandbox.is_some() || sandbox != Sandbox::Off,
        ),
        kv(palette, "Provider", provider.0, provider.1),
        kv(palette, "Effort", effort.0, effort.1),
    ]);
    if let Some((note, listed)) = effort_note {
        facts.push(Line::styled(
            format!("           {note}"),
            Style::default().fg(if listed { palette.muted } else { palette.warn }),
        ));
    }
    facts.extend([
        kv(
            palette,
            "Timeout",
            &format!("{} min", stage.timeout_seconds / 60),
            true,
        ),
        kv(palette, "ID", &stage.id, true),
    ]);
    if !stage.after.is_empty() {
        facts.push(kv(palette, "After", &stage.after.join(", "), true));
    }
    if let Some(billing) = stage.billing {
        facts.push(kv(palette, "Billing", billing.label(), true));
    }
    if !stage.budget.is_empty() {
        let mut parts = vec![];
        if let Some(usd) = stage.budget.max_usd {
            parts.push(crate::report::dollars(usd));
        }
        if let Some(tokens) = stage.budget.max_tokens {
            parts.push(format!("{} tok", crate::report::tokens(tokens)));
        }
        parts.push(stage.budget.on_exceed.label().to_owned());
        facts.push(kv(palette, "Budget", &parts.join(" · "), true));
    }
    if stage.require_router_checks {
        facts.push(kv(palette, "Checks", "router required", true));
    }
    if !stage.sandbox_network
        || stage.sandbox_fallback == SandboxFallback::Warn
        || !stage.sandbox_writable.is_empty()
    {
        let mut opts = vec![];
        if !stage.sandbox_network {
            opts.push("no network".to_owned());
        }
        if stage.sandbox_fallback == SandboxFallback::Warn {
            opts.push("fallback warn".to_owned());
        }
        if !stage.sandbox_writable.is_empty() {
            opts.push(format!("+{} writable", stage.sandbox_writable.len()));
        }
        facts.push(kv(palette, "Sandbox+", &opts.join(", "), true));
    }
    if let Some(delegation) = &stage.delegation {
        let mut targets = vec![];
        if !delegation.delegates.is_empty() {
            targets.push(format!("{} delegate(s)", delegation.delegates.len()));
        }
        if !delegation.harnesses.is_empty() {
            targets.push(format!("{} harness(es)", delegation.harnesses.len()));
        }
        if delegation.runs {
            targets.push("runs".into());
        }
        if delegation.schedules {
            targets.push("schedules".into());
        }
        facts.push(kv(palette, "Delegate", &targets.join(", "), true));
    }
    if let Some(fan) = &stage.fan_out {
        facts.push(kv(palette, "Judge", &fan.judge.to_string(), true));
        for (id, candidate) in fan.ids().iter().zip(&fan.candidates) {
            facts.push(Line::styled(
                format!(
                    "           {id}: {} {}",
                    candidate.harness,
                    or_default(&candidate.model, "default").0
                ),
                fg,
            ));
        }
    }

    let mut more = vec![Line::styled("Instructions", palette.header_style())];
    more.extend(
        storage::clean_terminal(&stage.instructions)
            .lines()
            .map(|line| Line::styled(line.to_string(), fg)),
    );
    if !stage.checks.is_empty() {
        more.push(Line::raw(""));
        more.push(Line::styled("Router checks", palette.header_style()));
        for check in &stage.checks {
            more.push(Line::from(vec![
                Span::styled(
                    storage::clean_terminal(&shell_words::join(&check.command)),
                    fg,
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
                    muted,
                ),
            ]));
        }
    }
    more.push(Line::raw(""));
    more.push(Line::styled("Harness settings", palette.header_style()));
    more.extend(
        harness_settings::summary(stage)
            .lines()
            .map(|line| Line::styled(line.to_string(), fg)),
    );
    more.push(Line::raw(""));
    if let Some(hint) = hint {
        // Advice from saved runs; the router never applies it.
        more.push(Line::styled(hint, muted.add_modifier(Modifier::DIM)));
    }
    more.push(Line::styled(
        if stage.role == Role::Review {
            "Optional. [o] toggles it; blocking findings request repairs."
        } else {
            "[c] configures this stage's harness; [e] edits everything else."
        },
        muted.add_modifier(Modifier::ITALIC),
    ));

    // Settings in two columns when every one fits its half, so the
    // instructions stay in view; otherwise one column that wraps.
    let column = usize::from(area.width / 2).saturating_sub(1);
    let columns = area.width >= 70 && facts.iter().all(|line| line.width() <= column);
    let half = facts.len().div_ceil(2);
    let fact_rows = if columns {
        u16::try_from(half).unwrap_or(u16::MAX)
    } else {
        facts
            .iter()
            .map(|line| rows_for(line, area.width, u16::MAX))
            .fold(0, u16::saturating_add)
    };
    let warning_rows = warning
        .as_ref()
        .map_or(0, |line| rows_for(line, area.width, 3).saturating_add(1));
    // Blank rows only when the settings all fit with them.
    let gap = u16::from(
        area.height
            >= 1u16
                .saturating_add(warning_rows)
                .saturating_add(fact_rows)
                .saturating_add(3),
    );
    let [title_row, _, warning_area, facts_area, _, more_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(gap),
        Constraint::Length(warning_rows),
        Constraint::Length(fact_rows),
        Constraint::Length(gap),
        Constraint::Min(0),
    ])
    .areas(area);
    // The position gives way when the stage's name and model need the row.
    if title.width() + place.width() + 2 <= usize::from(title_row.width) {
        frame.render_widget(Paragraph::new(place).right_aligned(), title_row);
    }
    frame.render_widget(Paragraph::new(title), title_row);
    if let Some(warning) = warning {
        frame.render_widget(
            Paragraph::new(warning).wrap(Wrap { trim: false }),
            warning_area,
        );
    }
    if columns {
        let right = facts.split_off(half);
        let [left_area, right_area] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(facts_area);
        frame.render_widget(Paragraph::new(facts), left_area);
        frame.render_widget(Paragraph::new(right), right_area);
    } else {
        frame.render_widget(Paragraph::new(facts).wrap(Wrap { trim: false }), facts_area);
    }
    frame.render_widget(Paragraph::new(more).wrap(Wrap { trim: false }), more_area);
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
        assert_eq!(editor.selected, SHIP);
        editor.key(KeyEvent::new(Key::Up, Mods::NONE));
        assert_eq!(editor.selected, INSTRUCTIONS);
        assert_eq!(editor.fields[INSTRUCTIONS].cursor(), (1, 4));
        editor.key(KeyEvent::new(Key::Char('!'), Mods::NONE));
        assert_eq!(editor.finish().unwrap().instructions, "first\nlast!");
    }

    #[test]
    fn ship_options_edit_as_toml_for_ship_stages_only() {
        let ship = crate::presets::stage("ship").build();
        let mut editor = StageEditor::new(ship, vec!["codex".into()]);
        assert!(content(&editor.fields[SHIP]).contains("draft_pr = true"));
        assert!(!editor.changed());
        editor.fields[SHIP] = textarea("push = false\nbranch = \"ship/{slug}\"");
        let options = editor.finish().unwrap().ship.unwrap();
        assert!(!options.push && !options.draft_pr && options.confirm);
        assert_eq!(options.branch, "ship/{slug}");
        editor.fields[SHIP] = textarea("draft_pr = true");
        assert!(format!("{:#}", editor.finish().unwrap_err()).contains("requires push"));
        editor.fields[SHIP] = textarea("pushh = true");
        assert!(format!("{:#}", editor.finish().unwrap_err()).contains("unknown field"));

        let plain = StageEditor::new(Stage::default(), vec!["codex".into()]);
        assert_eq!(content(&plain.fields[SHIP]), "");
        assert!(plain.finish().unwrap().ship.is_none());
        let mut plain = plain;
        plain.fields[SHIP] = textarea("push = true");
        assert!(plain.finish().is_err());
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
            .draw(|frame| {
                let notes = StageNotes {
                    sandbox: Sandbox::Off,
                    warning: None,
                    effort_note: None,
                    hint: None,
                };
                draw_stage_detail(frame, stage, (1, 1), notes, frame.area(), palette)
            })
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
    fn sandbox_choice_cycles_and_inherits_by_default() {
        let mut editor = StageEditor::new(Stage::default(), vec!["codex".into()]);
        assert_eq!(editor.finish().unwrap().sandbox, None);
        editor.focus(SANDBOX);
        editor.key(KeyEvent::from(Key::Right));
        editor.key(KeyEvent::from(Key::Right));
        assert_eq!(editor.finish().unwrap().sandbox, Some(Sandbox::ReadOnly));
        assert!(editor.changed());
        editor.key(KeyEvent::from(Key::Left));
        editor.key(KeyEvent::from(Key::Left));
        assert_eq!(editor.finish().unwrap().sandbox, None);
        assert!(!editor.changed());
    }

    #[test]
    fn long_settings_wrap_instead_of_running_into_the_next_column() {
        let palette = super::super::theme::Theme::default().palette();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 30)).unwrap();
        let stage = Stage {
            model: "openrouter/anthropic/claude-sonnet-5-5-preview".into(),
            ..Stage::default()
        };
        let warning = "⚠ opencode 2.0.22 lacks --variant, --interactive; see Harnesses [3]";
        terminal
            .draw(|frame| {
                let notes = StageNotes {
                    sandbox: Sandbox::Off,
                    warning: Some(warning.into()),
                    effort_note: None,
                    hint: None,
                };
                draw_stage_detail(frame, &stage, (1, 1), notes, frame.area(), palette)
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = (0..30)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>() + "\n")
            .collect();
        assert!(text.contains(warning), "{text}");
        assert!(
            text.contains("Model      openrouter/anthropic/claude-sonnet-5-5-preview"),
            "{text}"
        );
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

#[cfg(test)]
mod fan_out_field {
    use super::*;

    #[test]
    fn the_fan_out_field_roundtrips_and_explains_mistakes() {
        let fan = FanOut {
            candidates: vec![
                Candidate {
                    harness: "claude".into(),
                    model: "sonnet".into(),
                    ..Candidate::default()
                },
                Candidate {
                    id: "fast".into(),
                    harness: "codex".into(),
                    effort: "high".into(),
                    ..Candidate::default()
                },
            ],
            judge: "judge".into(),
        };
        let text = fan_out_text(&fan);
        assert_eq!(text, "judge = judge\nclaude sonnet\nfast: codex - - high");
        assert_eq!(parse_fan_out(&text).unwrap(), Some(fan));
        assert_eq!(parse_fan_out(" \n").unwrap(), None);
        let auto = parse_fan_out("codex\nclaude opus").unwrap().unwrap();
        assert!(auto.is_auto() && auto.candidates.len() == 2);
        assert!(parse_fan_out("codex a b c d").is_err());
        assert!(parse_fan_out("judge =").is_err());
        let mut stage = Stage {
            after: vec!["a".into(), "b".into()],
            ..Stage::default()
        };
        stage.fan_out = parse_fan_out("codex\nclaude").unwrap();
        let editor = StageEditor::new(stage.clone(), vec!["codex".into()]);
        let saved = editor.finish().unwrap();
        assert_eq!(saved.after, ["a", "b"]);
        assert_eq!(saved.fan_out, stage.fan_out);
        assert!(!editor.changed());
    }
}
