use super::{
    actions::ActionId,
    app::{App, SETTLE},
    completion::{checks_cell, draw_completion},
    modal::{Ask, InputTarget, Modal},
    theme::Palette,
    widgets::{elapsed, empty_state, short_path},
};
use crate::{
    adapters,
    config::Mode,
    engine::{Cmd, Status},
    storage,
};
use anyhow::{Context, Result, bail, ensure};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap},
};
use std::{collections::VecDeque, rc::Rc};
use unicode_width::UnicodeWidthChar;

impl App {
    pub(super) fn perform_monitor(&mut self, id: ActionId) -> Result<()> {
        match id {
            ActionId::CancelStage => {
                let run = self.focused.run.as_ref().context("No run selected")?;
                self.modal = Some(Modal::Ask(Box::new(Ask::CancelStage {
                    run: run.id.clone(),
                })));
            }
            ActionId::TakeOver => {
                if self.focused.terminal().is_some() {
                    self.launch_interactive = true;
                    return Ok(());
                }
                let run = self.focused.run.as_ref().context("No run selected")?;
                // A stage waiting for its terminal opens as it is.
                if self.focused.worker.is_none() && run.status == Status::Interactive {
                    self.launch_interactive = true;
                    return Ok(());
                }
                let stage = run
                    .workflow
                    .stages
                    .get(run.cursor)
                    .context("Run is complete")?;
                ensure!(
                    adapters::resumable(&stage.harness, &run.adapters),
                    "{} sessions cannot be resumed; [i] retries the stage interactively",
                    stage.harness
                );
                if self.focused.worker.is_some() {
                    ensure!(
                        stage.mode == Mode::Unattended,
                        "This stage is already interactive"
                    );
                    self.modal = Some(Modal::Ask(Box::new(Ask::TakeOver {
                        run: run.id.clone(),
                    })));
                } else {
                    self.focused
                        .run
                        .as_mut()
                        .context("No run")?
                        .command(Cmd::TakeOver)?;
                    self.launch_interactive = true;
                }
            }
            ActionId::PreviousSessionRun => self.cycle_focus(false),
            ActionId::NextSessionRun => self.cycle_focus(true),
            ActionId::CloseRun => {
                self.dismiss()?;
                self.message = "Run closed here; Runs [3] reopens it.".into();
            }
            ActionId::ScrollUp if self.results_shown() => {
                for scroll in &mut self.focused.results_scroll {
                    *scroll = scroll.saturating_sub(5);
                }
            }
            // Each panel stops at its own end, so scrolling back moves both at once.
            ActionId::ScrollDown if self.results_shown() => {
                let limit = self.focused.results_limit.get();
                for (scroll, limit) in self.focused.results_scroll.iter_mut().zip(limit) {
                    *scroll = scroll.saturating_add(5).min(limit);
                }
            }
            // Wrapped lines can outnumber log entries; drawing clamps the offset.
            ActionId::ScrollUp => {
                self.focused.log_scroll = self
                    .focused
                    .log_scroll
                    .saturating_add(10)
                    .min(self.focused.logs.len() * 4)
            }
            ActionId::ScrollDown => {
                self.focused.log_scroll = self.focused.log_scroll.saturating_sub(10)
            }
            ActionId::Diff => self.show_diff()?,
            ActionId::ToggleResults => self.toggle_results()?,
            ActionId::Handoff => {
                let run = self.focused.run.as_ref().context("No run selected")?;
                let index = run.cursor.min(run.stages.len().saturating_sub(1));
                let handoff = run
                    .stages
                    .get(index)
                    .and_then(|s| s.handoff.as_ref())
                    .or_else(|| run.stages.iter().rev().find_map(|s| s.handoff.as_ref()));
                let body = handoff
                    .map(serde_json::to_string_pretty)
                    .transpose()?
                    .unwrap_or_else(|| {
                        format!(
                            "No completed handoff yet.\n\n{}\n\nArtifacts: {}",
                            run.error.as_deref().unwrap_or(""),
                            run.directory.display()
                        )
                    });
                self.text("Handoff · [↑↓] scroll · [Esc] closes", body);
            }
            ActionId::Continue => {
                // `c` pressed as a stage stops was likely meant for the
                // running stage, so it must not start the next one.
                ensure!(
                    self.focused
                        .stopped_at
                        .is_none_or(|at| at.elapsed() >= SETTLE),
                    "The stage just stopped; press [c] again to continue"
                );
                self.start_driver()?
            }
            ActionId::Advance => {
                self.focused
                    .run
                    .as_mut()
                    .context("No run")?
                    .command(Cmd::Confirm)?;
                self.start_driver()?;
            }
            ActionId::Retry => {
                self.focused
                    .run
                    .as_mut()
                    .context("No run")?
                    .command(Cmd::Retry {
                        interactive: false,
                        model: None,
                    })?;
                self.start_driver()?;
            }
            ActionId::RetryInteractive => {
                let run = self.focused.run.as_mut().context("No run")?;
                // A stage already waiting for the terminal opens as it is.
                if run.status == Status::Interactive {
                    self.launch_interactive = true;
                } else {
                    run.command(Cmd::Retry {
                        interactive: true,
                        model: None,
                    })?;
                    self.start_driver()?;
                }
            }
            ActionId::ReplaceModel => {
                let run = self.focused.run.as_ref().context("No run")?;
                let stage = run
                    .workflow
                    .stages
                    .get(run.cursor)
                    .context("Run is complete")?;
                self.input(
                    "Replace model / mode and retry this stage",
                    InputTarget::RetryModel,
                    &stage.model.clone(),
                );
            }
            ActionId::EditSummary => {
                let run = self.focused.run.as_ref().context("No run")?;
                ensure!(
                    run.status == Status::AwaitingConfirmation,
                    "Only interactive handoffs can be edited"
                );
                let summary = run
                    .stages
                    .get(run.cursor)
                    .and_then(|s| s.handoff.as_ref())
                    .map(|h| h.summary.clone())
                    .unwrap_or_default();
                self.input(
                    "Edit interactive handoff summary",
                    InputTarget::Handoff,
                    &summary,
                );
            }
            ActionId::BackToWorkflow => self.page = 0,
            other => bail!("{other:?} is not a monitor action"),
        }
        Ok(())
    }
}

/// One line listing this session's runs when there is more than one;
/// false when there is nothing to list.
fn draw_session(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) -> bool {
    if app.slots().nth(1).is_none() {
        return false;
    }
    let mut spans = vec![Span::styled("[ ] ", Style::default().fg(palette.accent))];
    for (index, (slot, focused)) in app.slots().enumerate() {
        let Some(run) = &slot.run else { continue };
        let label = format!(
            " {} {} · {} ",
            index + 1,
            run.workflow.name,
            run.current_name()
        );
        spans.push(if focused {
            Span::styled(label, palette.selected_style())
        } else {
            Span::styled(label, Style::default().fg(palette.muted))
        });
        spans.push(palette.status_span(run.status));
        spans.push(Span::raw("  "));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    true
}

/// Output lines wrapped to `width` columns and coloured by what they show:
/// stage headers, assistant text, tool calls, results, and errors.
fn output_lines(logs: &VecDeque<String>, width: usize, palette: Palette) -> Vec<Line<'static>> {
    let width = width.max(10);
    let mut lines = Vec::new();
    let mut style = Style::default().fg(palette.muted);
    for log in logs {
        style = if log.starts_with("── ") {
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else if log.starts_with("● ") {
            Style::default().fg(palette.fg)
        } else if log.starts_with("⚙ ") || log.starts_with("$ ") || log.starts_with("⌕ ") {
            Style::default().fg(palette.blue)
        } else if log.starts_with("✎ ") {
            Style::default().fg(palette.warn)
        } else if log.starts_with("✓ ") {
            Style::default().fg(palette.success)
        } else if log.starts_with("✗ ") || log.starts_with("[stderr]") || log.contains("↳ ✗")
        {
            Style::default().fg(palette.red)
        } else if log.starts_with("∴ ") {
            Style::default()
                .fg(palette.muted)
                .add_modifier(Modifier::ITALIC)
        } else if log.starts_with("  ") && !log.starts_with("  ↳") {
            // A message's later lines keep its style.
            style
        } else {
            Style::default().fg(palette.muted)
        };
        for row in wrap(log, width) {
            lines.push(Line::styled(row, style));
        }
    }
    lines
}

/// `text` in rows of at most `width` display columns; wide characters
/// (CJK, emoji) take two. An empty line is one empty row.
pub(super) fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut rows = vec![];
    let mut row = String::new();
    let mut used = 0;
    for c in text.chars() {
        let size = c.width().unwrap_or(0);
        if used + size > width && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        row.push(c);
        used += size;
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }
    rows
}

pub(super) fn draw_run(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) {
    let Some(run) = &app.focused.run else {
        empty_state(
            frame,
            palette,
            area,
            palette.panel(" Run monitor "),
            "No run selected",
            "Start one from Workflow [r], or open a saved run from Runs [3].",
        );
        return;
    };
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length((run.stages.len() as u16 + 3).min(10)),
        Constraint::Min(3),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                run.workflow.name.clone(),
                Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            palette.status_span(run.status),
            Span::styled(
                format!(
                    "   {}  ·  repairs {}/{}",
                    run.id, run.repair_passes, run.workflow.max_repair_passes
                ),
                Style::default().fg(palette.muted),
            ),
            Span::styled(
                if app.focused.terminal().is_some() {
                    "   detached · [t] attach"
                } else {
                    ""
                },
                Style::default().fg(palette.accent),
            ),
        ])),
        chunks[0],
    );
    let listed = draw_session(frame, app, chunks[1], palette);
    if app.results_shown() {
        let top = if listed { chunks[2].y } else { chunks[1].y };
        let rest = Rect {
            y: top,
            height: area.height.saturating_sub(top - area.y),
            ..area
        };
        draw_completion(frame, app, run, rest, palette);
        return;
    }
    let name_width = run
        .workflow
        .stages
        .iter()
        .map(|s| s.name.chars().count() as u16)
        .max()
        .unwrap_or(0)
        .clamp(16, 40);
    let rows = run
        .stages
        .iter()
        .zip(&run.workflow.stages)
        .map(|(state, stage)| {
            let seconds = state.attempts.last().map(|a| {
                (a.finished_at.unwrap_or_else(chrono::Utc::now) - a.started_at)
                    .num_seconds()
                    .max(0)
            });
            Row::new([
                Cell::from(stage.name.clone()),
                Cell::from(palette.status_span(state.status)),
                Cell::from(state.attempts.len().to_string())
                    .style(Style::default().fg(palette.muted)),
                Cell::from(checks_cell(
                    state
                        .attempts
                        .last()
                        .into_iter()
                        .flat_map(|a| a.checks.iter().map(|c| c.status)),
                ))
                .style(Style::default().fg(palette.muted)),
                Cell::from(seconds.map(elapsed).unwrap_or_else(|| "–".into()))
                    .style(Style::default().fg(palette.muted)),
            ])
        });
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(name_width),
                Constraint::Length(18),
                Constraint::Length(8),
                Constraint::Length(6),
                Constraint::Fill(1),
            ],
        )
        .column_spacing(3)
        .header(
            Row::new(["STAGE", "STATUS", "ATTEMPTS", "CHECKS", "ELAPSED"])
                .style(palette.header_style()),
        )
        .block(palette.panel(" Progress "))
        .highlight_symbol("▌")
        .highlight_spacing(HighlightSpacing::Always)
        .row_highlight_style(palette.selected_style()),
        chunks[2],
        &mut TableState::default()
            .with_selected(Some(run.cursor.min(run.stages.len().saturating_sub(1)))),
    );
    let cols = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(chunks[3]);
    let output_block = palette.panel_with("Output", &[("PgUp/PgDn", "scroll")]);
    let visible = output_block.inner(cols[0]).height as usize;
    if app.focused.logs.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "Harness output appears here. Some harnesses emit their final response only when the stage finishes. Full logs are saved on disk.",
                Style::default().fg(palette.muted).add_modifier(Modifier::ITALIC),
            ))
            .wrap(Wrap { trim: false })
            .block(output_block),
            cols[0],
        );
    } else {
        // The panel's own inner width: borders and padding included.
        let width = output_block.inner(cols[0]).width as usize;
        // Wrapped lines are kept until the log, width, or theme changes.
        let key = (width, app.focused.log_version, app.displayed_theme());
        let mut cache = app.focused.cache.borrow_mut();
        let lines = match &cache.output {
            Some((cached, lines)) if *cached == key => lines.clone(),
            _ => {
                let lines = Rc::new(output_lines(&app.focused.logs, width, palette));
                cache.output = Some((key, lines.clone()));
                lines
            }
        };
        drop(cache);
        let scroll = app
            .focused
            .log_scroll
            .min(lines.len().saturating_sub(visible));
        let end = lines.len() - scroll;
        let start = end.saturating_sub(visible);
        frame.render_widget(
            Paragraph::new(lines[start..end].to_vec()).block(output_block),
            cols[0],
        );
    }
    let handoff = run
        .stages
        .get(run.cursor.min(run.stages.len().saturating_sub(1)))
        .and_then(|s| s.handoff.as_ref());
    let summary: Vec<Line> = if let Some(error) = &run.error {
        vec![
            Line::styled(
                "Error",
                Style::default()
                    .fg(palette.red)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            Line::styled(
                storage::clean_terminal(error),
                Style::default().fg(palette.red),
            ),
        ]
    } else if let Some(reason) = run.pending_waiver() {
        vec![
            Line::styled(
                "Verification not applicable",
                Style::default()
                    .fg(palette.warn)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            Line::raw("The test stage reported that no check applies:"),
            Line::styled(
                storage::clean_terminal(reason),
                Style::default().fg(palette.fg),
            ),
            Line::raw(""),
            Line::from(vec![
                Span::styled("[a]", Style::default().fg(palette.accent)),
                Span::raw(" accepts without checks   "),
                Span::styled("[y]", Style::default().fg(palette.accent)),
                Span::raw(" retries"),
            ]),
        ]
    } else if let Some(h) = handoff {
        let mut lines = vec![
            Line::styled(
                if h.structured {
                    "Structured handoff"
                } else {
                    "Text handoff · inspect before advancing"
                },
                palette.header_style(),
            ),
            Line::raw(""),
        ];
        lines.extend(
            storage::clean_terminal(&h.summary)
                .lines()
                .map(|line| Line::raw(line.to_string())),
        );
        lines
    } else {
        vec![
            Line::styled(
                run.current_name().to_string(),
                Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            Line::styled("Workspace", palette.header_style()),
            Line::raw(short_path(&run.workspace, usize::MAX)),
            Line::raw(""),
            Line::styled("Artifacts", palette.header_style()),
            Line::raw(short_path(&run.directory, usize::MAX)),
        ]
    };
    frame.render_widget(
        Paragraph::new(summary)
            .wrap(Wrap { trim: false })
            .block(palette.panel_with("Handoff", &[("h", "inspect")])),
        cols[1],
    );
}
