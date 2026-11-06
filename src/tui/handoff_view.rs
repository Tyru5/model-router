//! The Monitor's handoff dialog (`h`): summary, findings by severity, tests
//! by status, and the changed files, which open in `$EDITOR`.
use super::theme::Palette;
use crate::{
    handoff::{Handoff, Severity, TestStatus, Verdict},
    report, storage,
};
use crossterm::event::{KeyCode as Key, KeyEvent, KeyModifiers as Mods};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap},
};
use std::cell::Cell;

pub(super) struct HandoffView {
    pub(super) handoff: Handoff,
    /// The stage that wrote it.
    pub(super) stage: String,
    pub(super) selected: usize,
    pub(super) scroll: u16,
    /// How far the body can scroll, as last drawn.
    limit: Cell<u16>,
}

/// What a key asks of the app.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum HandoffAction {
    Stay,
    Close,
    /// Open this changed file (as the handoff names it).
    Edit(String),
}

impl HandoffView {
    pub(super) fn new(handoff: Handoff, stage: String) -> Self {
        Self {
            handoff,
            stage,
            selected: 0,
            scroll: 0,
            limit: Cell::new(u16::MAX),
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> HandoffAction {
        if key.modifiers - Mods::SHIFT != Mods::NONE {
            return HandoffAction::Stay;
        }
        let files = self.handoff.files_changed.len();
        match key.code {
            Key::Esc | Key::Char('q') => return HandoffAction::Close,
            Key::Up | Key::Char('k') => self.selected = self.selected.saturating_sub(1),
            Key::Down | Key::Char('j') => {
                self.selected = (self.selected + 1).min(files.saturating_sub(1))
            }
            Key::PageUp => self.scroll = self.scroll.saturating_sub(10),
            Key::PageDown => self.scroll = self.scroll.saturating_add(10).min(self.limit.get()),
            Key::Home => self.scroll = 0,
            Key::End => self.scroll = self.limit.get(),
            Key::Enter | Key::Char('e') => {
                if let Some(file) = self.handoff.files_changed.get(self.selected) {
                    return HandoffAction::Edit(file.clone());
                }
            }
            _ => {}
        }
        HandoffAction::Stay
    }

    /// The handoff's sections, coloured.
    pub(super) fn lines(&self, palette: Palette) -> Vec<Line<'static>> {
        let h = &self.handoff;
        let text = |s: &str| storage::clean_terminal(s);
        let muted = Style::default().fg(palette.muted);
        let (verdict, color) = match h.verdict {
            Verdict::Complete => ("complete", palette.success),
            Verdict::Pass => ("pass", palette.success),
            Verdict::NeedsChanges => ("needs changes", palette.warn),
            Verdict::Blocked => ("blocked", palette.red),
        };
        let mut lines = vec![Line::from(vec![
            Span::styled(
                self.stage.clone(),
                Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                verdict,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if h.structured {
                    "  structured handoff"
                } else {
                    "  text handoff · inspect before advancing"
                },
                muted,
            ),
        ])];
        let section = |lines: &mut Vec<Line<'static>>, title: &str| {
            lines.push(Line::raw(""));
            lines.push(Line::styled(title.to_owned(), palette.header_style()));
        };
        section(&mut lines, "Summary");
        lines.extend(
            text(&h.summary)
                .lines()
                .map(|l| Line::styled(l.to_owned(), Style::default().fg(palette.fg))),
        );
        if let Some(reason) = &h.not_applicable {
            section(&mut lines, "Verification not applicable");
            lines.push(Line::styled(
                text(reason),
                Style::default().fg(palette.warn),
            ));
        }
        if !h.findings.is_empty() {
            section(&mut lines, "Findings");
            for finding in &h.findings {
                let (glyph, label, color) = match finding.severity {
                    Severity::Blocking => ("!", "blocking", palette.red),
                    Severity::Advisory => ("·", "advisory", palette.warn),
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{glyph} {label}  "),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(text(&finding.description), Style::default().fg(palette.fg)),
                ]));
            }
        }
        if !h.tests.is_empty() {
            section(&mut lines, "Tests");
            for test in &h.tests {
                let color = match test.status {
                    TestStatus::Passed => palette.success,
                    TestStatus::Failed => palette.red,
                    TestStatus::NotRun => palette.warn,
                };
                let status = match test.status {
                    TestStatus::Passed => "passed",
                    TestStatus::Failed => "failed",
                    TestStatus::NotRun => "not run",
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} ", report::check_glyph(test.status)),
                        Style::default().fg(color),
                    ),
                    Span::styled(text(&test.command), Style::default().fg(palette.fg)),
                    Span::styled(
                        format!(
                            "  {status} · {}",
                            if test.required {
                                "required"
                            } else {
                                "optional"
                            }
                        ),
                        muted,
                    ),
                ]));
            }
        }
        for (title, items, glyph, color) in [
            ("Decisions", &h.decisions, "•", palette.fg),
            ("Unresolved", &h.unresolved, "?", palette.warn),
        ] {
            if items.is_empty() {
                continue;
            }
            section(&mut lines, title);
            for item in items {
                lines.push(Line::from(vec![
                    Span::styled(format!("{glyph} "), Style::default().fg(color)),
                    Span::styled(text(item), Style::default().fg(palette.fg)),
                ]));
            }
        }
        lines
    }

    pub(super) fn draw(&self, frame: &mut Frame<'_>, rect: Rect, palette: Palette) {
        let files = &self.handoff.files_changed;
        let keys: &[(&str, &str)] = if files.is_empty() {
            &[("PgUp/PgDn", "scroll"), ("Esc", "close")]
        } else {
            &[
                ("↑↓", "file"),
                ("e / Enter", "edit"),
                ("PgUp/PgDn", "scroll"),
                ("Esc", "close"),
            ]
        };
        let block = palette.modal("Handoff", keys);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let list_height = if files.is_empty() {
            0
        } else {
            (files.len() as u16 + 1).min(inner.height / 3).max(2)
        };
        let [body, list] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(list_height)]).areas(inner);
        let lines = self.lines(palette);
        let rows: u16 = lines
            .iter()
            .map(|l| super::widgets::rows_for(l, body.width, u16::MAX))
            .fold(0, u16::saturating_add);
        self.limit.set(rows.saturating_sub(body.height));
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll.min(self.limit.get()), 0)),
            body,
        );
        if files.is_empty() {
            return;
        }
        let items = files.iter().map(|file| {
            ListItem::new(Line::styled(
                storage::clean_terminal(file),
                Style::default().fg(palette.fg),
            ))
        });
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(palette.selected_style())
                .highlight_symbol("▌")
                .highlight_spacing(HighlightSpacing::Always)
                .block(
                    Block::default()
                        .borders(Borders::TOP)
                        .border_style(Style::default().fg(palette.line))
                        .title(Span::styled(
                            format!(" Files changed ({}) ", files.len()),
                            palette.header_style(),
                        )),
                ),
            list,
            &mut ListState::default().with_selected(Some(self.selected)),
        );
    }
}
