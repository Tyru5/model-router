use super::{app::App, theme::Palette, widgets::short_path};
use crate::{adapters, catalog};
use crossterm::event::{KeyCode as Key, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::Style,
    text::Line,
    widgets::{Cell, HighlightSpacing, Row, Table, TableState},
};

impl App {
    pub(super) fn harnesses_key(&mut self, key: KeyEvent) {
        let harnesses = catalog::known_harnesses(&self.settings);
        match key.code {
            Key::Down | Key::Char('j') => {
                self.harness_selected =
                    (self.harness_selected + 1).min(harnesses.len().saturating_sub(1))
            }
            Key::Up | Key::Char('k') => {
                self.harness_selected = self.harness_selected.saturating_sub(1)
            }
            Key::Enter => {
                let harness = &harnesses[self.harness_selected];
                self.text(harness, format!("Executable: {}\n\n{}\n\nSelect this harness with [h] on the workflow page, then press [m] to load model selections. Use [c] on Workflow to configure the selected stage, including permissions and extra arguments.\n\nPermissions inherit the harness configuration unless overridden for a stage. Authentication stays with the harness.\n\nCustom adapters are defined in TOML. See README.md for the template contract.", adapters::program(harness, &self.settings.adapters), if harness == "amp" { "Amp uses mode keys. A mode determines its model and tools." } else { "A model must be supported by the selected harness/provider." }));
            }
            _ => {}
        }
    }
}

pub(super) fn draw_harnesses(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) {
    let rows = catalog::known_harnesses(&app.settings)
        .into_iter()
        .map(|name| {
            let program = adapters::program(&name, &app.settings.adapters);
            let path = adapters::executable(&program);
            let found = path.is_some();
            Row::new([
                Cell::from(name),
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
                Cell::from(path.map(|p| short_path(&p, usize::MAX)).unwrap_or(program))
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
