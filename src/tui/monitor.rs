use super::{
    actions::ActionId,
    app::{App, Page},
    completion::{checks_cell, draw_completion},
    handoff_view::HandoffView,
    modal::{Ask, InputTarget, Modal},
    output_search::{Wrapped, visible_lines},
    run_slot::{RunSlot, SETTLE},
    theme::Palette,
    widgets::{elapsed, empty_state, short_path},
};
use crate::{
    adapters,
    config::Mode,
    engine::{self, Cmd, Status},
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
                let run = self.session.run().context("No run selected")?;
                self.ui.modal = Some(Modal::Ask(Box::new(Ask::CancelStage {
                    run: run.id.clone(),
                })));
            }
            ActionId::TakeOver => {
                let slot = self.session.focused().context("No run selected")?;
                if slot.terminal().is_some() {
                    self.session.launch_interactive = true;
                    return Ok(());
                }
                let run = &slot.run;
                // A stage waiting for its terminal opens as it is.
                if slot.worker.is_none() && run.status == Status::Interactive {
                    self.session.launch_interactive = true;
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
                if slot.worker.is_some() {
                    ensure!(
                        stage.mode == Mode::Unattended,
                        "This stage is already interactive"
                    );
                    self.ui.modal = Some(Modal::Ask(Box::new(Ask::TakeOver {
                        run: run.id.clone(),
                    })));
                } else {
                    self.session
                        .run_mut()
                        .context("No run")?
                        .command(Cmd::TakeOver)?;
                    self.session.launch_interactive = true;
                }
            }
            ActionId::PreviousSessionRun => self.session.cycle_focus(false),
            ActionId::NextSessionRun => self.session.cycle_focus(true),
            ActionId::CloseRun => {
                self.session.dismiss()?;
                self.ui.message = "Run closed here; Runs [3] reopens it.".into();
            }
            ActionId::ScrollUp if self.results_shown() => self.view(|slot| {
                for scroll in &mut slot.results_scroll {
                    *scroll = scroll.saturating_sub(5);
                }
            }),
            // Each panel stops at its own end, so scrolling back moves both at once.
            ActionId::ScrollDown if self.results_shown() => self.view(|slot| {
                let limit = slot.results_limit.get();
                for (scroll, limit) in slot.results_scroll.iter_mut().zip(limit) {
                    *scroll = scroll.saturating_add(5).min(limit);
                }
            }),
            // Scrolling up pauses the output; reaching the end follows again.
            ActionId::ScrollUp => self.view(|slot| slot.scroll_output(-10)),
            ActionId::ScrollDown => self.view(|slot| slot.scroll_output(10)),
            ActionId::OutputTop if self.results_shown() => {
                self.view(|slot| slot.results_scroll = [0; 2])
            }
            ActionId::OutputBottom if self.results_shown() => {
                self.view(|slot| slot.results_scroll = slot.results_limit.get())
            }
            ActionId::OutputTop => self.view(RunSlot::output_home),
            ActionId::OutputBottom => self.view(RunSlot::output_end),
            ActionId::FollowOutput if self.results_shown() => self.view(|slot| {
                slot.show_output = true;
                slot.output_end();
            }),
            ActionId::FollowOutput => {
                let message = self.session.focused_mut().and_then(RunSlot::toggle_follow);
                if let Some(message) = message {
                    self.ui.message = message;
                }
            }
            ActionId::FindOutput => self.view(RunSlot::start_output_search),
            ActionId::OpenLog => {
                let path = self.focused_log().context("No attempt has output yet")?;
                self.ui.external = Some(super::external::page_file(&path)?);
            }
            ActionId::Diff => self.show_diff()?,
            ActionId::ToggleResults => self.toggle_results()?,
            ActionId::Handoff => {
                let run = self.session.run().context("No run selected")?;
                let index = run.cursor.min(run.stages.len().saturating_sub(1));
                let found = run
                    .stages
                    .get(index)
                    .and_then(|s| s.handoff.as_ref().map(|h| (index, h)))
                    .or_else(|| {
                        run.stages
                            .iter()
                            .enumerate()
                            .rev()
                            .find_map(|(i, s)| s.handoff.as_ref().map(|h| (i, h)))
                    });
                match found {
                    Some((index, handoff)) => {
                        let stage = run
                            .workflow
                            .stages
                            .get(index)
                            .map_or_else(String::new, |s| s.name.clone());
                        self.ui.modal = Some(Modal::Handoff(Box::new(HandoffView::new(
                            handoff.clone(),
                            stage,
                        ))));
                    }
                    None => {
                        let body = format!(
                            "No completed handoff yet.\n\n{}\n\nArtifacts: {}",
                            run.error.as_deref().unwrap_or(""),
                            run.directory.display()
                        );
                        self.text("Handoff · [↑↓] scroll · [Esc] closes", body);
                    }
                }
            }
            ActionId::Continue => {
                // `c` pressed as a stage stops was likely meant for the
                // running stage, so it must not start the next one.
                ensure!(
                    self.session
                        .focused()
                        .and_then(|slot| slot.stopped_at)
                        .is_none_or(|at| at.elapsed() >= SETTLE),
                    "The stage just stopped; press [c] again to continue"
                );
                self.start_driver()?
            }
            ActionId::Advance => {
                self.session
                    .run_mut()
                    .context("No run")?
                    .command(Cmd::Confirm)?;
                self.start_driver()?;
            }
            ActionId::Retry => {
                self.session
                    .run_mut()
                    .context("No run")?
                    .command(Cmd::Retry {
                        interactive: false,
                        model: None,
                    })?;
                self.start_driver()?;
            }
            ActionId::RetryInteractive => {
                let run = self.session.run_mut().context("No run")?;
                // A stage already waiting for the terminal opens as it is.
                if run.status == Status::Interactive {
                    self.session.launch_interactive = true;
                } else {
                    run.command(Cmd::Retry {
                        interactive: true,
                        model: None,
                    })?;
                    self.start_driver()?;
                }
            }
            ActionId::ReplaceModel => {
                let run = self.session.run().context("No run")?;
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
                let run = self.session.run().context("No run")?;
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
            ActionId::BackToWorkflow => self.ui.page = Page::Workflow,
            other => bail!("{other:?} is not a monitor action"),
        }
        Ok(())
    }
}

impl App {
    /// Changes the focused run's view; nothing without a run.
    fn view(&mut self, change: impl FnOnce(&mut RunSlot)) {
        if let Some(slot) = self.session.focused_mut() {
            change(slot);
        }
    }

    /// Keys for a search in the focused run's output; false passes the key on.
    pub(super) fn output_search_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        let handled = self
            .session
            .focused_mut()
            .and_then(|slot| slot.output_search_key(key));
        if let Some(Some(message)) = &handled {
            self.ui.message = message.clone();
        }
        handled.is_some()
    }

    /// stdout.log of the focused attempt: the current stage's latest, else
    /// the latest of any stage.
    pub(super) fn focused_log(&self) -> Option<std::path::PathBuf> {
        let run = self.session.run()?;
        let index = run.cursor.min(run.stages.len().saturating_sub(1));
        let attempt = run
            .stages
            .get(index)
            .and_then(|s| s.attempts.last())
            .or_else(|| {
                run.stages
                    .iter()
                    .flat_map(|s| &s.attempts)
                    .max_by_key(|a| a.started_at)
            })?;
        Some(attempt.directory.join("stdout.log"))
    }
}

/// One line listing this session's runs when there is more than one;
/// false when there is nothing to list.
fn draw_session(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) -> bool {
    if app.session.runs.len() < 2 {
        return false;
    }
    let mut spans = vec![Span::styled("[ ] ", Style::default().fg(palette.accent))];
    for (index, (slot, focused)) in app.session.slots().enumerate() {
        let run = &slot.run;
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
/// stage headers, assistant text, tool calls, results, and errors. `first`
/// is the absolute number of the first log line.
pub(super) fn output_lines(
    logs: &VecDeque<String>,
    first: u64,
    width: usize,
    palette: Palette,
) -> Wrapped {
    let width = width.max(10);
    let mut lines = Vec::new();
    let mut starts = Vec::with_capacity(logs.len());
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
        starts.push(lines.len());
        for row in wrap(log, width) {
            lines.push(Line::styled(row, style));
        }
    }
    Wrapped {
        lines,
        starts,
        first,
    }
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

/// "  ·  ~$0.42 table of $5.00": spend so far, updated as each attempt
/// records its usage; empty before any attempt without a budget.
fn running_cost(slot: &RunSlot) -> String {
    let report = super::completion::cached_report(slot);
    let (cost, budget) = (report.usage.cost, report.budget);
    if cost.attempts == 0 && budget.is_empty() {
        return String::new();
    }
    let mut text = format!("  ·  cost {}", cost.short());
    if cost.known() {
        text.push_str(&format!(" {}", cost.sources()));
    }
    if let Some(cap) = budget.max_usd {
        text.push_str(&format!(" of {}", crate::report::dollars(cap)));
    }
    if let Some(cap) = budget.max_tokens {
        text.push_str(&format!(
            " · {} of {} tokens",
            crate::report::tokens(cost.tokens),
            crate::report::tokens(cap)
        ));
    }
    text
}

/// Stage rows plus fan-out candidate rows in the progress table.
fn progress_rows(run: &engine::Run) -> usize {
    run.stages.len()
        + run
            .stages
            .iter()
            .filter_map(|s| s.fan_out.as_ref())
            .map(|f| f.candidates.len())
            .sum::<usize>()
}

pub(super) fn draw_run(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) {
    let Some(slot) = app.session.focused() else {
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
    let run = &slot.run;
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length((progress_rows(run) as u16 + 3).min(10)),
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
                    "   {}  ·  repairs {}/{}{}",
                    run.id,
                    run.repair_passes,
                    run.workflow.max_repair_passes,
                    running_cost(slot)
                ),
                Style::default().fg(palette.muted),
            ),
            Span::styled(
                if slot.terminal().is_some() {
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
        draw_completion(frame, slot, rest, palette);
        return;
    }
    let name_width = run
        .workflow
        .stages
        .iter()
        .map(|s| s.name.chars().count() as u16)
        .chain(
            run.stages
                .iter()
                .filter_map(|s| s.fan_out.as_ref())
                .flat_map(|f| &f.candidates)
                .map(|c| (c.id.len() + c.harness.len() + c.model.len() + 8) as u16),
        )
        .max()
        .unwrap_or(0)
        .clamp(16, 40);
    let seconds = |attempt: Option<&engine::Attempt>| {
        attempt.map(|a| {
            (a.finished_at.unwrap_or_else(chrono::Utc::now) - a.started_at)
                .num_seconds()
                .max(0)
        })
    };
    let mut rows = vec![];
    // The cursor's row, counting the candidate rows above it.
    let mut selected = 0;
    for (index, (state, stage)) in run.stages.iter().zip(&run.workflow.stages).enumerate() {
        if index == run.cursor.min(run.stages.len().saturating_sub(1)) {
            selected = rows.len();
        }
        rows.push(Row::new([
            Cell::from(stage.name.clone()),
            Cell::from(palette.status_span(state.status)),
            Cell::from(state.attempts.len().to_string()).style(Style::default().fg(palette.muted)),
            Cell::from(checks_cell(
                state
                    .attempts
                    .last()
                    .into_iter()
                    .flat_map(|a| a.checks.iter().map(|c| c.status)),
            ))
            .style(Style::default().fg(palette.muted)),
            Cell::from(
                seconds(state.attempts.last())
                    .map(elapsed)
                    .unwrap_or_else(|| "–".into()),
            )
            .style(Style::default().fg(palette.muted)),
        ]));
        // Fan-out candidates, indented under their stage; ★ marks the choice.
        let Some(fan) = &state.fan_out else {
            continue;
        };
        let chosen = fan.selection.as_ref().map(|s| s.candidate.as_str());
        for candidate in &fan.candidates {
            rows.push(Row::new([
                Cell::from(format!(
                    "  {} {} · {}",
                    if chosen == Some(candidate.id.as_str()) {
                        "★"
                    } else {
                        "↳"
                    },
                    candidate.id,
                    if candidate.model.is_empty() {
                        candidate.harness.clone()
                    } else {
                        format!("{} {}", candidate.harness, candidate.model)
                    }
                ))
                .style(Style::default().fg(palette.muted)),
                Cell::from(palette.status_span(candidate.status)),
                Cell::from(candidate.attempts.len().to_string())
                    .style(Style::default().fg(palette.muted)),
                Cell::from(checks_cell(
                    candidate
                        .attempts
                        .last()
                        .into_iter()
                        .flat_map(|a| a.checks.iter().map(|c| c.status)),
                ))
                .style(Style::default().fg(palette.muted)),
                Cell::from(
                    seconds(candidate.attempts.last())
                        .map(elapsed)
                        .unwrap_or_else(|| "–".into()),
                )
                .style(Style::default().fg(palette.muted)),
            ]));
        }
    }
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
        &mut TableState::default().with_selected(Some(selected)),
    );
    let cols = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(chunks[3]);
    let paused = !slot.log_view.follow;
    let mut output_block = palette.panel_with(
        if paused {
            "Output · ▲ paused — End to follow"
        } else {
            "Output"
        },
        // The paused notice needs the room at narrow widths.
        if paused {
            &[]
        } else {
            &[("PgUp/PgDn", "scroll"), ("/", "search")]
        },
    );
    if let Some(status) = slot.output_search_status() {
        output_block = output_block.title_bottom(Line::styled(
            format!(" {status} "),
            Style::default().fg(palette.accent),
        ));
    }
    let visible = output_block.inner(cols[0]).height as usize;
    slot.log_visible.set(visible);
    if slot.logs.is_empty() {
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
        let key = (width, slot.log_version, app.ui.displayed_theme());
        let mut cache = slot.cache.borrow_mut();
        let lines = match &cache.output {
            Some((cached, lines)) if *cached == key => lines.clone(),
            _ => {
                let first = slot.log_version.saturating_sub(slot.logs.len() as u64);
                let lines = Rc::new(output_lines(&slot.logs, first, width, palette));
                cache.output = Some((key, lines.clone()));
                lines
            }
        };
        drop(cache);
        // Paused output keeps its top line as new output arrives.
        let top = lines.top(slot.log_view, visible);
        frame.render_widget(
            Paragraph::new(visible_lines(
                &lines,
                top,
                visible,
                slot.log_search.as_ref(),
                palette,
            ))
            .block(output_block),
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
