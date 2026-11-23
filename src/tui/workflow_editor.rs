//! Modal editor for workflow-level settings (`W` / `Space w W`).
use super::{
    input::InputField,
    theme::Palette,
    widgets::{content, focus_textarea, textarea, wrapped_textarea},
};
use crate::{
    config::{Budget, BudgetAction, Sandbox, Workflow},
    storage,
};
use anyhow::{Context, Result, bail, ensure};
use crossterm::event::{KeyCode as Key, KeyEvent, KeyModifiers as Mods};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, HighlightSpacing, List, ListItem, ListState, Padding, Paragraph, Wrap},
};
use tui_textarea::CursorMove;

const FIELDS: &[&str] = &[
    "Max repair passes (0 disables repairs)",
    "Max parallel processes (1-16)",
    "Default router sandbox",
    "Handoff repair follow-up",
    "Workflow budget max USD (blank = none)",
    "Workflow budget max tokens (blank = none)",
    "Workflow budget on exceed",
    "Protected paths (one glob per line)",
];

const MAX_REPAIR: usize = 0;
const MAX_PARALLEL: usize = 1;
const SANDBOX_DEFAULT: usize = 2;
const HANDOFF_REPAIR: usize = 3;
const BUDGET_USD: usize = 4;
const BUDGET_TOKENS: usize = 5;
const BUDGET_ACTION: usize = 6;
const PROTECTED_PATHS: usize = 7;

const CHOICES: [usize; 3] = [SANDBOX_DEFAULT, HANDOFF_REPAIR, BUDGET_ACTION];
const MULTILINE: [usize; 1] = [PROTECTED_PATHS];

pub(super) struct WorkflowEditor {
    workflow: Workflow,
    pub(super) fields: Vec<InputField<'static>>,
    pub(super) selected: usize,
    list_state: ListState,
    pub(super) error: String,
}

impl WorkflowEditor {
    pub(super) fn new(workflow: Workflow) -> Self {
        let max_repair = workflow.max_repair_passes.to_string();
        let max_parallel = workflow.max_parallel.to_string();
        let sandbox = workflow.sandbox_default.label().to_owned();
        let handoff_repair = if workflow.handoff_repair { "on" } else { "off" }.to_owned();
        let budget_usd = workflow
            .budget
            .max_usd
            .map(format_usd_field)
            .unwrap_or_default();
        let budget_tokens = workflow
            .budget
            .max_tokens
            .map(|n| n.to_string())
            .unwrap_or_default();
        let budget_action = workflow.budget.on_exceed.label().to_owned();
        let protected = workflow.protected_paths.join("\n");
        let fields = [
            max_repair,
            max_parallel,
            sandbox,
            handoff_repair,
            budget_usd,
            budget_tokens,
            budget_action,
            protected,
        ]
        .iter()
        .map(|value| textarea(value))
        .collect();
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            workflow,
            fields,
            selected: 0,
            list_state,
            error: String::new(),
        }
    }

    pub(super) fn active_input(&mut self) -> Option<&mut InputField<'static>> {
        if CHOICES.contains(&self.selected) {
            None
        } else {
            Some(&mut self.fields[self.selected])
        }
    }

    fn choices(&self, index: usize) -> Vec<String> {
        match index {
            SANDBOX_DEFAULT => Sandbox::ALL.iter().map(|s| s.label().to_owned()).collect(),
            HANDOFF_REPAIR => vec!["off".into(), "on".into()],
            BUDGET_ACTION => vec![
                BudgetAction::Stop.label().into(),
                BudgetAction::Warn.label().into(),
            ],
            _ => vec![],
        }
    }

    pub(super) fn finish(&self) -> Result<Workflow> {
        let mut workflow = self.workflow.clone();
        workflow.max_repair_passes = content(&self.fields[MAX_REPAIR])
            .trim()
            .parse()
            .context("Max repair passes must be a nonnegative integer")?;
        workflow.max_parallel = content(&self.fields[MAX_PARALLEL])
            .trim()
            .parse()
            .context("Max parallel must be a positive integer (1-16)")?;
        let sandbox = content(&self.fields[SANDBOX_DEFAULT]);
        workflow.sandbox_default = Sandbox::ALL
            .into_iter()
            .find(|s| s.label() == sandbox)
            .context("Unknown sandbox mode")?;
        workflow.handoff_repair = content(&self.fields[HANDOFF_REPAIR]) == "on";
        workflow.budget = parse_budget(
            &content(&self.fields[BUDGET_USD]),
            &content(&self.fields[BUDGET_TOKENS]),
            &content(&self.fields[BUDGET_ACTION]),
            "Workflow",
        )?;
        workflow.protected_paths = parse_nonempty_lines(&content(&self.fields[PROTECTED_PATHS]));
        Ok(workflow)
    }

    pub(super) fn changed(&self) -> bool {
        self.finish().map_or(true, |workflow| {
            serde_json::to_value(&workflow).ok() != serde_json::to_value(&self.workflow).ok()
        })
    }

    fn focus(&mut self, index: usize) {
        self.selected = index;
        self.list_state.select(Some(index));
        focus_textarea(&mut self.fields[index]);
    }

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

    pub(super) fn key(&mut self, key: KeyEvent) {
        self.error.clear();
        if self.moves_within(&key) {
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
        } else if CHOICES.contains(&self.selected) {
            if matches!(
                key.code,
                Key::Left | Key::Right | Key::Char(' ') | Key::Enter
            ) {
                let values = self.choices(self.selected);
                if values.is_empty() {
                    return;
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
            }
        } else if key.code == Key::Enter && !MULTILINE.contains(&self.selected) {
            self.focus((self.selected + 1) % self.fields.len());
        } else {
            self.fields[self.selected].input(key);
        }
    }

    pub(super) fn paste(&mut self, text: &str) {
        self.error.clear();
        if !CHOICES.contains(&self.selected) {
            self.fields[self.selected].insert_str(text);
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame<'_>, rect: Rect, palette: Palette) {
        let title = format!(
            "Workflow settings · {} ({}/{})",
            self.workflow.name,
            self.selected + 1,
            FIELDS.len()
        );
        let block = palette.modal(
            &title,
            &[("↑/↓ / Tab", "switch field"), ("←/→", "cycle choice")],
        );
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let has_error = !self.error.is_empty();
        let chunks = Layout::vertical([
            Constraint::Min(6),
            Constraint::Length(if has_error { 2 } else { 0 }),
            Constraint::Length(2),
        ])
        .split(inner);
        let halves = Layout::horizontal([Constraint::Percentage(44), Constraint::Percentage(56)])
            .split(chunks[0]);
        let value_width = halves[0].width.saturating_sub(4) as usize;
        let rows = FIELDS.iter().enumerate().map(|(i, label)| {
            let value = field_preview(&self.fields[i], value_width);
            let choice = CHOICES.contains(&i);
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
        if CHOICES.contains(&self.selected) {
            let choices = self.choices(self.selected);
            let current = content(&self.fields[self.selected]);
            let index = choices.iter().position(|v| *v == current).unwrap_or(0);
            let panes = Layout::vertical([
                Constraint::Length(choices.len() as u16 + 2),
                Constraint::Min(1),
            ])
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
                Paragraph::new(choice_help(self.selected))
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
        if has_error {
            frame.render_widget(
                Paragraph::new(Line::styled(
                    storage::clean_terminal(&self.error),
                    Style::default().fg(palette.red),
                ))
                .wrap(Wrap { trim: false }),
                chunks[1],
            );
        }
        let mut keys = palette.hints(&[("Ctrl+S", "apply settings"), ("Esc", "cancel")]);
        keys.spans.push(Span::styled(
            field_hint(self.selected),
            Style::default().fg(palette.muted),
        ));
        frame.render_widget(
            Paragraph::new(vec![Line::raw(""), keys]).wrap(Wrap { trim: false }),
            chunks[2],
        );
    }
}

fn choice_help(index: usize) -> &'static str {
    match index {
        SANDBOX_DEFAULT => {
            "Default OS sandbox for stages that set sandbox = inherit. ←/→ or Space cycles."
        }
        HANDOFF_REPAIR => {
            "When on, an unattended resumable stage exiting 0 with missing/malformed handoff JSON gets one lightweight follow-up."
        }
        BUDGET_ACTION => {
            "stop blocks before the next stage when a cap is exceeded; warn logs a warning and continues."
        }
        _ => "",
    }
}

fn field_hint(index: usize) -> &'static str {
    match index {
        PROTECTED_PATHS => "   ·  repo-relative globs, e.g. .github/workflows/* or **/Cargo.lock",
        BUDGET_USD => "   ·  positive USD amount, e.g. 5 or 2.50; blank disables USD cap",
        BUDGET_TOKENS => {
            "   ·  positive token count, e.g. 100000 or 200k; blank disables token cap"
        }
        MAX_PARALLEL => "   ·  concurrent processes for graph workflows and fan-out stages (1-16)",
        _ => "   ·  [s] on Workflow saves to disk",
    }
}

/// Formats a USD budget for an editor field. `Display` gives the shortest
/// text that parses back to the same `f64`, with no exponent or trailing
/// zeros, so saving an unedited field never changes the cap.
pub(super) fn format_usd_field(usd: f64) -> String {
    usd.to_string()
}

/// Parses a positive token count: plain integer (commas/underscores allowed)
/// or integer/decimal with `k`/`K` (×1,000) or `m`/`M` (×1,000,000) suffix.
pub(super) fn parse_token_count(text: &str) -> Result<u64> {
    let clean: String = text
        .trim()
        .chars()
        .filter(|c| *c != ',' && *c != '_')
        .collect();
    ensure!(
        !clean.is_empty(),
        "Enter a positive token count, e.g. 50000 or 200k"
    );
    let (num, mult) = match clean.as_bytes().last().copied() {
        Some(b'k' | b'K') => (&clean[..clean.len() - 1], 1_000.0),
        Some(b'm' | b'M') => (&clean[..clean.len() - 1], 1_000_000.0),
        _ => {
            let value: u64 = clean
                .parse()
                .context("Enter a positive token count, e.g. 50000 or 200k")?;
            ensure!(value > 0, "Enter a positive token count");
            return Ok(value);
        }
    };
    let parsed: f64 = num
        .parse()
        .ok()
        .filter(|v: &f64| v.is_finite() && *v > 0.0)
        .context("Enter a positive token count, e.g. 50000 or 200k")?;
    let scaled = (parsed * mult).round();
    if !scaled.is_finite() || scaled < 1.0 || scaled > u64::MAX as f64 {
        bail!("Token count is out of range");
    }
    Ok(scaled as u64)
}

pub(super) fn parse_budget(
    usd_text: &str,
    tokens_text: &str,
    action_text: &str,
    scope: &str,
) -> Result<Budget> {
    let usd_trimmed = usd_text.trim();
    let max_usd = if usd_trimmed.is_empty() {
        None
    } else {
        let usd: f64 = usd_trimmed
            .trim_start_matches('$')
            .parse()
            .ok()
            .filter(|v: &f64| v.is_finite() && *v > 0.0)
            .with_context(|| format!("{scope} budget max_usd must be a positive amount"))?;
        Some(usd)
    };
    let tokens_trimmed = tokens_text.trim();
    let max_tokens = if tokens_trimmed.is_empty() {
        None
    } else {
        Some(
            parse_token_count(tokens_trimmed)
                .with_context(|| format!("{scope} budget max_tokens must be a positive integer"))?,
        )
    };
    let on_exceed = match action_text.trim() {
        "warn" => BudgetAction::Warn,
        _ => BudgetAction::Stop,
    };
    Ok(Budget {
        max_usd,
        max_tokens,
        on_exceed,
    })
}

pub(super) fn parse_nonempty_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

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

#[cfg(test)]
mod tests {
    use super::{format_usd_field, parse_budget};

    #[test]
    fn usd_budget_fields_round_trip_without_rounding() {
        for usd in [0.00001, 0.123456789, 2.5, 5.0, 1234.56789, 1e-9, 1e12] {
            let text = format_usd_field(usd);
            let budget = parse_budget(&text, "", "stop", "Workflow").unwrap();
            assert_eq!(budget.max_usd, Some(usd), "{text}");
        }
        assert_eq!(format_usd_field(5.0), "5");
        assert_eq!(format_usd_field(2.5), "2.5");
        assert_eq!(format_usd_field(0.00001), "0.00001");
    }
}
