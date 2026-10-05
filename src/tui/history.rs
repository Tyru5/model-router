use super::{actions::ActionId, app::App, theme::Palette, widgets::empty_state};
use crate::{engine, storage};
use anyhow::{Result, bail};
use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::Style,
    widgets::{Cell, HighlightSpacing, Row, Table, TableState},
};

impl App {
    pub(super) fn perform_runs(&mut self, id: ActionId) -> Result<()> {
        match id {
            ActionId::NextRun => {
                self.history_selected =
                    (self.history_selected + 1).min(self.history.len().saturating_sub(1))
            }
            ActionId::PreviousRun => {
                self.history_selected = self.history_selected.saturating_sub(1)
            }
            ActionId::RefreshRuns => self.history = engine::history(&self.project)?,
            ActionId::OpenRun => {
                if self.worker.is_none() {
                    if let Some(run) = self.history.get(self.history_selected).cloned() {
                        self.run = Some(run);
                        self.page = 1;
                        self.logs.clear();
                        self.message =
                            "Saved run loaded. c continues; y retries; i reopens interactively."
                                .into();
                    }
                }
            }
            other => bail!("{other:?} is not a Runs action"),
        }
        Ok(())
    }
}

pub(super) fn draw_history(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) {
    if app.history.is_empty() {
        empty_state(
            frame,
            palette,
            area,
            palette.panel(" Saved runs "),
            "No saved runs yet",
            "Runs for this project are saved automatically. Start a workflow from Workflow [r].",
        );
        return;
    }
    let rows = app.history.iter().map(|run| {
        Row::new([
            Cell::from(run.created_at.format("%b %d  %H:%M").to_string())
                .style(Style::default().fg(palette.muted)),
            Cell::from(palette.status_span(run.status)),
            Cell::from(run.workflow.name.clone()),
            Cell::from(storage::clean_terminal(&run.task.replace('\n', " ")))
                .style(Style::default().fg(palette.muted)),
        ])
    });
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(14),
                Constraint::Length(18),
                Constraint::Fill(1),
                Constraint::Fill(2),
            ],
        )
        .header(
            Row::new(["STARTED", "STATUS", "WORKFLOW", "TASK"])
                .style(palette.header_style())
                .bottom_margin(1),
        )
        .block(palette.panel_with("Saved runs", &[("Enter", "open in monitor")]))
        .highlight_symbol("▌")
        .highlight_spacing(HighlightSpacing::Always)
        .row_highlight_style(palette.selected_style()),
        area,
        &mut TableState::default()
            .with_selected(Some(app.history_selected.min(app.history.len() - 1))),
    );
}
