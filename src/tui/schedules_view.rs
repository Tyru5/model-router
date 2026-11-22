//! Modal dialog to inspect and manage project schedules (`u` / `Space r u`).
use super::{
    theme::Palette,
    widgets::{fit, kv, menu_key},
};
use crate::{
    delegation::schedules::{self, Schedule},
    storage,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode as Key, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap},
};
use std::path::Path;

pub(super) struct ScheduleRow {
    pub(super) schedule: Schedule,
    pub(super) approved: bool,
    /// `schedules::digest` of this row as loaded, or why there is none:
    /// approving passes it so a later edit is refused, not approved.
    pub(super) digest: std::result::Result<String, String>,
    pub(super) next_due: Option<DateTime<Utc>>,
}

pub(super) enum SchedulesAction {
    Stay,
    Close,
    Refresh,
    Approve {
        schedule: Box<Schedule>,
        digest: String,
    },
    Pause(String),
    Enable(String),
    Delete(String),
}

pub(super) struct SchedulesView {
    pub(super) rows: Vec<ScheduleRow>,
    pub(super) trust_error: Option<String>,
    pub(super) selected: usize,
    list_state: ListState,
    scroll: u16,
    pub(super) status: String,
    pub(super) error: String,
}

impl SchedulesView {
    pub(super) fn load(project: &Path) -> Result<Self> {
        let mut view = Self {
            rows: vec![],
            trust_error: None,
            selected: 0,
            list_state: ListState::default(),
            scroll: 0,
            status: String::new(),
            error: String::new(),
        };
        view.reload(project)?;
        Ok(view)
    }

    pub(super) fn reload(&mut self, project: &Path) -> Result<()> {
        let selected_id = self.current().map(|row| row.schedule.id.clone());
        let store = schedules::load(project)?;
        let (settings, trust_error) = match schedules::trusted_settings(project) {
            Ok(settings) => (Some(settings), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        let now = Utc::now();
        self.rows = store
            .schedules
            .into_iter()
            .map(|schedule| {
                let approved = settings
                    .as_ref()
                    .is_some_and(|s| schedules::approved(project, &schedule, s));
                let digest = match &settings {
                    Some(s) => {
                        schedules::digest(project, &schedule, s).map_err(|e| format!("{e:#}"))
                    }
                    None => Err("The project config is untrusted".into()),
                };
                let next_due = schedules::next_due(&schedule, now);
                ScheduleRow {
                    schedule,
                    approved,
                    digest,
                    next_due,
                }
            })
            .collect();
        self.trust_error = trust_error;
        if let Some(id) = selected_id {
            if let Some(pos) = self.rows.iter().position(|row| row.schedule.id == id) {
                self.selected = pos;
            }
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.list_state.select(if self.rows.is_empty() {
            None
        } else {
            Some(self.selected)
        });
        Ok(())
    }

    pub(super) fn current(&self) -> Option<&ScheduleRow> {
        self.rows.get(self.selected)
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> SchedulesAction {
        self.error.clear();
        self.status.clear();
        match menu_key(key) {
            Key::Up | Key::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.scroll = 0;
                SchedulesAction::Stay
            }
            Key::Down | Key::Char('j') => {
                if !self.rows.is_empty() {
                    self.selected = (self.selected + 1).min(self.rows.len() - 1);
                    self.scroll = 0;
                }
                SchedulesAction::Stay
            }
            Key::PageUp | Key::Char('K') => {
                self.scroll = self.scroll.saturating_sub(6);
                SchedulesAction::Stay
            }
            Key::PageDown | Key::Char('J') => {
                self.scroll = self.scroll.saturating_add(6);
                SchedulesAction::Stay
            }
            Key::Char('r') => SchedulesAction::Refresh,
            Key::Char('a') => {
                let Some(row) = self.current() else {
                    return SchedulesAction::Stay;
                };
                if let Some(err) = &self.trust_error {
                    self.error = err.clone();
                    return SchedulesAction::Stay;
                }
                if !row.schedule.enabled {
                    self.error = format!(
                        "Schedule {} is paused; press [e] to enable it before approving.",
                        row.schedule.id
                    );
                    return SchedulesAction::Stay;
                }
                match &row.digest {
                    Ok(digest) => SchedulesAction::Approve {
                        schedule: Box::new(row.schedule.clone()),
                        digest: digest.clone(),
                    },
                    Err(why) => {
                        self.error =
                            format!("Schedule {} cannot be approved: {why}", row.schedule.id);
                        SchedulesAction::Stay
                    }
                }
            }
            Key::Char('p') => match self.current() {
                Some(row) => SchedulesAction::Pause(row.schedule.id.clone()),
                None => SchedulesAction::Stay,
            },
            Key::Char('e') => match self.current() {
                Some(row) => SchedulesAction::Enable(row.schedule.id.clone()),
                None => SchedulesAction::Stay,
            },
            Key::Char('D') | Key::Char('x') => match self.current() {
                Some(row) => SchedulesAction::Delete(row.schedule.id.clone()),
                None => SchedulesAction::Stay,
            },
            Key::Char('q') => SchedulesAction::Close,
            _ => SchedulesAction::Stay,
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame<'_>, rect: Rect, palette: Palette) {
        let block = palette.modal(
            "Schedules · .model-router/schedules.json",
            &[
                ("a", "approve"),
                ("p", "pause"),
                ("e", "enable"),
                ("D", "delete"),
                ("r", "refresh"),
                ("Esc", "close"),
            ],
        );
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let has_banner = self.trust_error.is_some();
        let has_note = !self.error.is_empty() || !self.status.is_empty();
        let chunks = Layout::vertical([
            Constraint::Length(if has_banner { 2 } else { 0 }),
            Constraint::Min(5),
            Constraint::Length(if has_note { 2 } else { 0 }),
        ])
        .split(inner);
        if let Some(err) = &self.trust_error {
            frame.render_widget(
                Paragraph::new(Line::styled(
                    format!("⚠ {}", storage::clean_terminal(err)),
                    Style::default().fg(palette.warn),
                ))
                .wrap(Wrap { trim: false }),
                chunks[0].inner(Margin::new(1, 0)),
            );
        }
        if self.rows.is_empty() {
            frame.render_widget(
                Paragraph::new(
                    "No schedules in .model-router/schedules.json.\n\nDelegating stages with delegation.schedules = true can propose recurring runs; approve them here with [a] or via `model-router schedules approve ID`.",
                )
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(palette.muted)),
                chunks[1].inner(Margin::new(1, 1)),
            );
        } else {
            let halves =
                Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)])
                    .split(chunks[1]);
            let list_width = usize::from(halves[0].width.saturating_sub(5));
            let items = self.rows.iter().map(|row| {
                let (glyph, color, state_label) = schedule_state(row, palette);
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(glyph, Style::default().fg(color)),
                        Span::raw(" "),
                        Span::styled(
                            fit(&row.schedule.id, list_width),
                            Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    Line::styled(
                        format!(
                            "  {state_label} · {}",
                            fit(
                                &row.schedule.trigger.describe(),
                                list_width.saturating_sub(state_label.len() + 5)
                            )
                        ),
                        Style::default().fg(palette.muted),
                    ),
                ])
            });
            self.list_state
                .select(Some(self.selected.min(self.rows.len() - 1)));
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_style(palette.highlight_style())
                    .highlight_symbol("▌ ")
                    .highlight_spacing(HighlightSpacing::Always)
                    .block(
                        Block::default()
                            .borders(Borders::RIGHT)
                            .border_style(Style::default().fg(palette.line)),
                    ),
                halves[0],
                &mut self.list_state,
            );
            if let Some(row) = self.current() {
                let lines = schedule_detail_lines(row, palette);
                let max_scroll = u16::try_from(lines.len().saturating_sub(1)).unwrap_or(u16::MAX);
                self.scroll = self.scroll.min(max_scroll);
                frame.render_widget(
                    Paragraph::new(lines)
                        .wrap(Wrap { trim: false })
                        .scroll((self.scroll, 0)),
                    halves[1].inner(Margin::new(1, 0)),
                );
            }
        }
        if has_note {
            let (text, color) = if !self.error.is_empty() {
                (&self.error, palette.red)
            } else {
                (&self.status, palette.accent)
            };
            frame.render_widget(
                Paragraph::new(Line::styled(
                    storage::clean_terminal(text),
                    Style::default().fg(color),
                ))
                .wrap(Wrap { trim: false }),
                chunks[2].inner(Margin::new(1, 0)),
            );
        }
    }
}

fn schedule_state(
    row: &ScheduleRow,
    palette: Palette,
) -> (&'static str, ratatui::style::Color, &'static str) {
    match (row.schedule.enabled, row.approved) {
        (false, _) => ("○", palette.muted, "paused"),
        (true, true) => ("✓", palette.success, "approved"),
        (true, false) => ("◆", palette.warn, "needs approval"),
    }
}

fn schedule_detail_lines(row: &ScheduleRow, palette: Palette) -> Vec<Line<'static>> {
    let s = &row.schedule;
    let (_, _, state_label) = schedule_state(row, palette);
    let muted = Style::default().fg(palette.muted);
    let fg = Style::default().fg(palette.fg);
    let next_due = row
        .next_due
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "on webhook".into());
    let mut lines = vec![
        kv(palette, "ID", &s.id, true),
        kv(palette, "State", state_label, true),
        kv(palette, "Trigger", &s.trigger.describe(), true),
        kv(palette, "Next due", &next_due, row.next_due.is_some()),
        kv(
            palette,
            "Workflow",
            s.workflow.as_deref().unwrap_or("(default)"),
            true,
        ),
        kv(
            palette,
            "Created by",
            &s.created_by,
            !s.created_by.is_empty(),
        ),
        kv(palette, "Revision", &s.revision.to_string(), true),
    ];
    if let Some(reason) = &s.paused_reason {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("Paused: {}", storage::clean_terminal(reason)),
            Style::default().fg(palette.warn),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Task", palette.header_style()));
    for line in storage::clean_terminal(&s.task).lines() {
        lines.push(Line::styled(line.to_owned(), fg));
    }
    if !s.occurrences.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled("Recent firings", palette.header_style()));
        for occ in s.occurrences.iter().rev().take(6) {
            let outcome = match (&occ.run, &occ.error) {
                (Some(run_id), _) => format!("run {run_id}"),
                (None, Some(err)) => format!("error: {}", storage::clean_terminal(err)),
                (None, None) => "pending".into(),
            };
            lines.push(Line::styled(
                format!("  {}  {outcome}", occ.fired_at.format("%Y-%m-%d %H:%M")),
                muted,
            ));
        }
    }
    lines
}
