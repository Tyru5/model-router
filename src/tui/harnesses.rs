use super::{actions::ActionId, app::App, theme::Palette, widgets::short_path};
use crate::{adapters, catalog, config::Settings};
use anyhow::{Result, bail};
use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::Style,
    text::Line,
    widgets::{Cell, HighlightSpacing, Row, Table, TableState},
};
use std::path::PathBuf;

/// An installation check for the Harnesses page. Checks scan PATH, which is
/// slow on some systems (e.g. WSL), so they run when the page opens.
pub(super) struct HarnessRow {
    name: String,
    program: String,
    path: Option<PathBuf>,
}

pub(super) fn scan(settings: &Settings) -> Vec<HarnessRow> {
    catalog::known_harnesses(settings)
        .into_iter()
        .map(|name| {
            let program = adapters::program(&name, &settings.adapters);
            let path = adapters::executable(&program);
            HarnessRow {
                name,
                program,
                path,
            }
        })
        .collect()
}

impl App {
    pub(super) fn perform_harnesses(&mut self, id: ActionId) -> Result<()> {
        match id {
            ActionId::NextHarness => {
                self.harness_selected =
                    (self.harness_selected + 1).min(self.harness_rows.len().saturating_sub(1))
            }
            ActionId::PreviousHarness => {
                self.harness_selected = self.harness_selected.saturating_sub(1)
            }
            ActionId::HarnessDetails => {
                let Some(row) = self.harness_rows.get(self.harness_selected) else {
                    return Ok(());
                };
                let (harness, program) = (row.name.clone(), row.program.clone());
                self.text(&harness, format!("Executable: {}\n\n{}\n\nSelect this harness with [h] on the workflow page, then press [m] to load model selections. Use [c] on Workflow to configure the selected stage, including permissions and extra arguments.\n\nPermissions inherit the harness configuration unless overridden for a stage. Authentication stays with the harness.\n\nCustom adapters are defined in TOML. See README.md for the template contract.", program, if harness == "amp" { "Amp uses mode keys. A mode determines its model and tools." } else { "A model must be supported by the selected harness/provider." }));
            }
            ActionId::RefreshHarnesses => self.harness_rows = scan(&self.settings),
            other => bail!("{other:?} is not a Harnesses action"),
        }
        Ok(())
    }
}

pub(super) fn draw_harnesses(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) {
    let rows = app.harness_rows.iter().map(|row| {
        let found = row.path.is_some();
        Row::new([
            Cell::from(row.name.clone()),
            Cell::from(if found {
                "● installed"
            } else {
                "○ not found"
            })
            .style(Style::default().fg(if found {
                palette.accent
            } else {
                palette.muted
            })),
            Cell::from(
                row.path
                    .as_ref()
                    .map_or_else(|| row.program.clone(), |p| short_path(p, usize::MAX)),
            )
            .style(Style::default().fg(palette.muted)),
        ])
    });
    let block = palette.panel(" Harnesses ").title(
        Line::styled(
            " uses existing logins and provider settings ",
            Style::default().fg(palette.muted),
        )
        .right_aligned(),
    );
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(12),
                Constraint::Length(13),
                Constraint::Min(20),
            ],
        )
        .header(
            Row::new(["HARNESS", "STATUS", "PATH / COMMAND"])
                .style(palette.header_style())
                .bottom_margin(1),
        )
        .block(block)
        .highlight_symbol("▌")
        .highlight_spacing(HighlightSpacing::Always)
        .row_highlight_style(palette.selected_style()),
        area,
        &mut TableState::default().with_selected(Some(app.harness_selected)),
    );
}
