use super::{
    actions::ActionId,
    app::App,
    modal::InputTarget,
    theme::Palette,
    widgets::{elapsed, empty_state, short_path},
};
use crate::{engine::Status, storage};
use anyhow::{Context, Result, bail, ensure};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap},
};
use std::sync::atomic::Ordering;

impl App {
    pub(super) fn perform_monitor(&mut self, id: ActionId) -> Result<()> {
        match id {
            ActionId::CancelStage => {
                self.cancel.store(true, Ordering::Relaxed);
                self.message = "Stopping stage…".into();
            }
            ActionId::ScrollUp => {
                self.log_scroll = self.log_scroll.saturating_add(10).min(self.logs.len())
            }
            ActionId::ScrollDown => self.log_scroll = self.log_scroll.saturating_sub(10),
            ActionId::Handoff => {
                let run = self.run.as_ref().context("No run selected")?;
                let index = run.cursor.min(run.stages.len().saturating_sub(1));
                let handoff = run.stages[index]
                    .handoff
                    .as_ref()
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
            ActionId::Continue => self.start_driver()?,
            ActionId::Advance => {
                self.run.as_mut().context("No run")?.confirm()?;
                self.start_driver()?;
            }
            ActionId::Retry => {
                self.run.as_mut().context("No run")?.retry(false, None)?;
                self.start_driver()?;
            }
            ActionId::RetryInteractive => {
                self.run.as_mut().context("No run")?.retry(true, None)?;
                self.start_driver()?;
            }
            ActionId::ReplaceModel => {
                let run = self.run.as_ref().context("No run")?;
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
                let run = self.run.as_ref().context("No run")?;
                ensure!(
                    run.status == Status::AwaitingConfirmation,
                    "Only interactive handoffs can be edited"
                );
                let summary = run.stages[run.cursor]
                    .handoff
                    .as_ref()
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

pub(super) fn draw_run(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) {
    let Some(run) = &app.run else {
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
        ])),
        chunks[0],
    );
    let name_width = run
        .workflow
        .stages
        .iter()
        .map(|s| s.name.chars().count() as u16)
        .max()
        .unwrap_or(0)
        .clamp(16, 40);
    let rows = run.stages.iter().enumerate().map(|(i, state)| {
        let seconds = state.attempts.last().map(|a| {
            (a.finished_at.unwrap_or_else(chrono::Utc::now) - a.started_at)
                .num_seconds()
                .max(0)
        });
        Row::new([
            Cell::from(run.workflow.stages[i].name.clone()),
            Cell::from(palette.status_span(state.status)),
            Cell::from(state.attempts.len().to_string()).style(Style::default().fg(palette.muted)),
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
                Constraint::Fill(1),
            ],
        )
        .column_spacing(3)
        .header(Row::new(["STAGE", "STATUS", "ATTEMPTS", "ELAPSED"]).style(palette.header_style()))
        .block(palette.panel(" Progress "))
        .highlight_symbol("▌")
        .highlight_spacing(HighlightSpacing::Always)
        .row_highlight_style(palette.selected_style()),
        chunks[2],
        &mut TableState::default().with_selected(Some(run.cursor.min(run.stages.len() - 1))),
    );
    let cols = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(chunks[3]);
    let visible = cols[0].height.saturating_sub(2) as usize;
    let end = app.logs.len().saturating_sub(app.log_scroll);
    let start = end.saturating_sub(visible);
    let output_block = palette.panel_with("Output", &[("PgUp/PgDn", "scroll")]);
    if app.logs.is_empty() {
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
        frame.render_widget(
            Paragraph::new(
                app.logs
                    .iter()
                    .skip(start)
                    .take(end - start)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .block(output_block)
            .style(Style::default().fg(palette.muted)),
            cols[0],
        );
    }
    let handoff = run
        .stages
        .get(run.cursor.min(run.stages.len() - 1))
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
