use super::tasks::{Msg, Tasks};
use super::{actions::ActionId, app::App, theme::Palette, widgets::short_path};
use crate::{
    catalog,
    config::Settings,
    doctor::{self, Report, Support},
};
use anyhow::{Result, bail};
use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Cell, HighlightSpacing, Row, Table, TableState},
};
use std::path::Path;

pub(super) type Probe = fn(&Settings, &Path) -> Vec<Report>;

/// Doctor results for the Harnesses page. Probes scan PATH and run `--version`
/// and `--help`, which is slow on some systems (e.g. WSL), so they run on a
/// thread when the page opens.
pub(super) struct HarnessChecks {
    pub(super) reports: Vec<Report>,
    /// A check is running.
    pub(super) checking: bool,
    /// Tests swap in a stub so no harness runs.
    pub(super) probe: Probe,
}
/// The Harnesses page: doctor results and the selected row.
#[derive(Default)]
pub(super) struct HarnessesPage {
    pub(super) selected: usize,
    pub(super) checks: HarnessChecks,
}

impl Default for HarnessChecks {
    fn default() -> Self {
        Self::new()
    }
}
impl HarnessChecks {
    pub(super) fn new() -> Self {
        Self {
            reports: vec![],
            checking: false,
            probe: doctor::inspect_all,
        }
    }
    /// Starts a check unless one is running. Old results stay until new ones arrive.
    pub(super) fn start(&mut self, settings: &Settings, project: &Path, tasks: &Tasks) {
        if self.checking {
            return;
        }
        self.checking = true;
        let (settings, project, probe) = (settings.clone(), project.to_path_buf(), self.probe);
        tasks.spawn(
            "Harness check failed",
            move || Ok(probe(&settings, &project)),
            Msg::Checks,
        );
    }
    pub(super) fn clear(&mut self) {
        self.reports.clear();
        self.checking = false;
    }
    pub(super) fn report(&self, harness: &str) -> Option<&Report> {
        self.reports.iter().find(|r| r.harness == harness)
    }
    /// The Workflow warning for a stage's harness; cached results only.
    pub(super) fn warning(&self, harness: &str) -> Option<String> {
        let report = self.report(harness).filter(|r| r.missing_flags())?;
        Some(format!(
            "⚠ {} {} lacks {}; see Harnesses [4]",
            report.harness,
            report.version_label(),
            missing(report).join(", ")
        ))
    }
}

/// Distinct missing flags in adapter order.
fn missing(report: &Report) -> Vec<&str> {
    let mut flags: Vec<&str> = vec![];
    for capability in report.capabilities.iter().flatten() {
        if capability.support == Support::Missing && !flags.contains(&capability.flag.as_str()) {
            flags.push(&capability.flag);
        }
    }
    flags
}

impl HarnessesPage {
    /// Harness names in display order: the last results, or the configured
    /// harnesses until the first check finishes.
    pub(super) fn names(&self, settings: &Settings) -> Vec<String> {
        if self.checks.reports.is_empty() {
            catalog::known_harnesses(settings)
        } else {
            self.checks
                .reports
                .iter()
                .map(|r| r.harness.clone())
                .collect()
        }
    }
}

impl App {
    pub(super) fn perform_harnesses(&mut self, id: ActionId) -> Result<()> {
        match id {
            ActionId::NextHarness => {
                self.harnesses.selected = (self.harnesses.selected + 1).min(
                    self.harnesses
                        .names(&self.editor.settings)
                        .len()
                        .saturating_sub(1),
                )
            }
            ActionId::PreviousHarness => {
                self.harnesses.selected = self.harnesses.selected.saturating_sub(1)
            }
            ActionId::HarnessDetails => {
                let Some(harness) = self
                    .harnesses
                    .names(&self.editor.settings)
                    .get(self.harnesses.selected)
                    .cloned()
                else {
                    return Ok(());
                };
                let body = details(&harness, self.harnesses.checks.report(&harness));
                self.text(&harness, body);
            }
            ActionId::RefreshHarnesses => {
                self.harnesses
                    .checks
                    .start(&self.editor.settings, &self.project, &self.tasks)
            }
            other => bail!("{other:?} is not a Harnesses action"),
        }
        Ok(())
    }
}

fn details(harness: &str, report: Option<&Report>) -> String {
    let mut text = String::new();
    match report {
        None => {
            text.push_str("Still checking this harness. Details appear when the check finishes.\n")
        }
        Some(r) => {
            text.push_str(&format!(
                "Executable: {}\nVersion: {} ({})\nTested versions: {}\nCapabilities: {}\n",
                r.path.as_ref().map_or_else(
                    || format!("{} (not found)", r.program),
                    |p| p.display().to_string()
                ),
                r.version.as_deref().unwrap_or("-"),
                r.tested_label(),
                if r.tested_versions.is_empty() {
                    "-".into()
                } else {
                    r.tested_versions.join(", ")
                },
                r.summary
            ));
            for c in r.capabilities.iter().flatten() {
                let modes: Vec<_> = c.modes.iter().map(|m| m.label()).collect();
                text.push_str(&format!(
                    "  {} {} ({}) · {}\n",
                    match c.support {
                        Support::Supported => "✓ supported",
                        Support::Missing => "✗ missing  ",
                        Support::Unknown => "? unknown  ",
                    },
                    c.flag,
                    modes.join(", "),
                    c.help
                ));
            }
            let notes: Vec<_> = r.notes.iter().filter(|n| *n != doctor::AUTH_NOTE).collect();
            if !notes.is_empty() {
                text.push_str("\nNotes:\n");
                for note in notes {
                    text.push_str(&format!("- {note}\n"));
                }
            }
        }
    }
    text.push_str(&format!(
        "\n{}\n\nSelect this harness with [h] on the workflow page, then press [m] to load model selections. Use [c] on Workflow to configure the selected stage, including permissions and extra arguments.\n\nPermissions inherit the harness configuration unless overridden for a stage.\n\n{}\n\nCustom adapters are defined in TOML. See README.md for the template contract.",
        doctor::AUTH_NOTE,
        match crate::harness::get(harness) {
            Some(spec) if matches!(spec.model, crate::harness::ModelPass::Mode(_)) => format!(
                "{} uses mode keys. A mode determines its model and tools.",
                spec.display
            ),
            _ => "A model must be supported by the selected harness/provider.".into(),
        }
    ));
    if let Some(spec) = crate::harness::get(harness) {
        text.push_str(&format!(
            "\n{}: {} `model-router harnesses --spec {harness}` prints its full spec.\n",
            spec.display, spec.notes
        ));
    }
    text
}

/// Truncates to `width` columns with an ellipsis.
fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.into();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

pub(super) fn draw_harnesses(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) {
    let checks = &app.harnesses.checks;
    let wide = area.width >= 110;
    let names = app.harnesses.names(&app.editor.settings);
    let name_width = names.iter().map(|n| n.chars().count()).max().unwrap_or(7);
    let name_width = name_width.clamp(7, 14);
    let version_width = if area.width >= 140 { 22 } else { 10 };
    // Borders, padding, highlight symbol, and column gaps.
    let fixed = 4 + 1 + name_width + 11 + version_width + 8 + if wide { 5 } else { 4 };
    let rest = (area.width as usize).saturating_sub(fixed);
    let (caps_width, path_width) = if wide {
        let caps = rest.min(48);
        (caps, rest.saturating_sub(caps + 1))
    } else {
        (rest, 0)
    };
    let styled = |text: String, color: Color| Cell::from(text).style(Style::default().fg(color));
    let rows = names.iter().map(|name| {
        let Some(r) = checks.report(name) else {
            return Row::new([
                Cell::from(clip(name, name_width)),
                styled(
                    if checks.checking {
                        "checking…"
                    } else {
                        "not checked"
                    }
                    .into(),
                    palette.muted,
                ),
            ]);
        };
        let installed = r.installed();
        let version = r.version_label();
        let mut cells = vec![
            Cell::from(clip(name, name_width)),
            if installed {
                styled("● installed".into(), palette.accent)
            } else {
                styled("○ not found".into(), palette.muted)
            },
            styled(
                clip(&version, version_width),
                if r.version.is_some() {
                    palette.fg
                } else {
                    palette.muted
                },
            ),
            styled(
                r.tested_label().into(),
                match r.version_tested {
                    Some(true) => palette.accent,
                    Some(false) => palette.warn,
                    None => palette.muted,
                },
            ),
            styled(
                clip(&r.summary, caps_width),
                if r.missing_flags() {
                    palette.warn
                } else if r.summary == "ok" {
                    palette.accent
                } else {
                    palette.muted
                },
            ),
        ];
        if wide {
            cells.push(styled(
                r.path.as_ref().map_or_else(
                    || clip(&r.program, path_width),
                    |p| short_path(p, path_width),
                ),
                palette.muted,
            ));
        }
        Row::new(cells).style(Style::default().fg(if installed {
            palette.fg
        } else {
            palette.muted
        }))
    });
    let note = if checks.checking {
        " checking… "
    } else {
        " uses existing logins and provider settings "
    };
    let block = palette
        .panel(" Harnesses ")
        .title(Line::styled(note, Style::default().fg(palette.muted)).right_aligned());
    let mut widths = vec![
        Constraint::Length(name_width as u16),
        Constraint::Length(11),
        Constraint::Length(version_width as u16),
        Constraint::Length(8),
        Constraint::Length(caps_width as u16),
    ];
    let mut header = vec!["HARNESS", "STATUS", "VERSION", "TESTED", "CAPABILITIES"];
    if wide {
        widths.push(Constraint::Fill(1));
        header.push("PATH");
    }
    frame.render_stateful_widget(
        Table::new(rows, widths)
            .header(
                Row::new(header)
                    .style(palette.header_style())
                    .bottom_margin(1),
            )
            .block(block)
            .highlight_symbol("▌")
            .highlight_spacing(HighlightSpacing::Always)
            .row_highlight_style(palette.selected_style()),
        area,
        &mut TableState::default().with_selected(Some(app.harnesses.selected)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Mode,
        doctor::{AUTH_NOTE, Capability},
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::{
        path::PathBuf,
        sync::{Arc, atomic::AtomicBool},
        time::{Duration, Instant},
    };

    fn capability(flag: &str, modes: &[Mode], support: Support) -> Capability {
        Capability {
            flag: flag.into(),
            modes: modes.to_vec(),
            help: "opencode run --help".into(),
            support,
        }
    }

    fn report(harness: &str, path: Option<&str>) -> Report {
        Report {
            harness: harness.into(),
            builtin: true,
            program: harness.into(),
            path: path.map(PathBuf::from),
            version: None,
            tested_versions: vec![],
            version_tested: None,
            version_captured: false,
            capabilities: None,
            summary: "not checked".into(),
            notes: vec![AUTH_NOTE.into()],
        }
    }

    fn stub(_: &Settings, _: &Path) -> Vec<Report> {
        let both = [Mode::Unattended, Mode::Interactive];
        let mut codex = report("codex", Some("/usr/bin/codex"));
        codex.version = Some("codex-cli 0.160.0".into());
        codex.tested_versions = vec!["0.160.0".into()];
        codex.version_tested = Some(true);
        codex.capabilities = Some(vec![capability("--model", &both, Support::Supported)]);
        codex.summary = "ok".into();
        let mut opencode = report("opencode", Some("/usr/bin/opencode"));
        opencode.version = Some("opencode v2.0.22".into());
        opencode.tested_versions = vec!["1.4.3".into()];
        opencode.version_tested = Some(false);
        opencode.capabilities = Some(vec![
            capability("--model", &both, Support::Supported),
            capability("--variant", &both, Support::Missing),
            capability("--interactive", &[Mode::Interactive], Support::Missing),
        ]);
        opencode.summary = "missing: --variant, --interactive (interactive)".into();
        opencode
            .notes
            .push("Detected version is not a parser-tested version.".into());
        vec![codex, opencode, report("gemini", None)]
    }

    fn app() -> App {
        let mut app = App::new(
            std::env::temp_dir(),
            Settings::default(),
            PathBuf::from("/tmp/config"),
            Arc::new(AtomicBool::new(false)),
        );
        app.harnesses.checks.probe = stub;
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        app.key(KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
    }

    fn wait(app: &mut App) {
        let start = Instant::now();
        while app.harnesses.checks.checking {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "check never finished"
            );
            app.poll().unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Rendered rows, and the foreground color where `needle` starts.
    fn render(app: &mut App, width: u16, needle: &str) -> (String, Option<Color>) {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 24)).unwrap();
        terminal
            .draw(|frame| super::super::app::draw(frame, app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        let mut color = None;
        for y in 0..buffer.area.height {
            let cells: Vec<_> = (0..buffer.area.width).map(|x| &buffer[(x, y)]).collect();
            let row: String = cells.iter().map(|c| c.symbol()).collect();
            if let Some(index) = row.find(needle) {
                let column = row[..index].chars().count();
                color = color.or(Some(cells[column].fg));
            }
            text.push_str(&row);
            text.push('\n');
        }
        (text, color)
    }

    #[test]
    fn page_checks_in_background_and_shows_doctor_columns() {
        let mut app = app();
        let palette = app.ui.palette();
        press(&mut app, KeyCode::Char('3'));
        assert!(app.harnesses.checks.checking);
        let (text, _) = render(&mut app, 80, "checking…");
        assert!(
            text.contains("codex") && text.contains("checking…"),
            "{text}"
        );
        wait(&mut app);
        assert_eq!(app.harnesses.checks.reports.len(), 3);

        let (text, color) = render(&mut app, 80, "missing: --variant");
        assert_eq!(color, Some(palette.warn), "{text}");
        assert!(
            text.contains("2.0.22") && text.contains("untested"),
            "{text}"
        );
        assert!(
            text.contains("○ not found") && !text.contains("/usr/bin"),
            "{text}"
        );
        assert_eq!(render(&mut app, 80, "untested").1, Some(palette.warn));
        assert_eq!(render(&mut app, 80, "ok ").1, Some(palette.accent));
        assert_eq!(render(&mut app, 80, "○ not found").1, Some(palette.muted));
        let (text, _) = render(&mut app, 140, "PATH");
        assert!(
            text.contains("missing: --variant, --interactive (interactive)")
                && text.contains("/usr/bin/opencode"),
            "{text}"
        );

        // Refresh keeps old rows until fresh results arrive.
        app.harnesses.checks.reports.truncate(1);
        press(&mut app, KeyCode::Char('r'));
        assert!(app.harnesses.checks.checking);
        assert_eq!(app.harnesses.checks.reports.len(), 1);
        wait(&mut app);
        assert_eq!(app.harnesses.checks.reports.len(), 3);
    }

    #[test]
    fn details_list_capabilities_notes_and_auth() {
        let mut app = app();
        press(&mut app, KeyCode::Char('3'));
        press(&mut app, KeyCode::Enter);
        let Some(super::super::modal::Modal::Text { body, .. }) = app.ui.dialog() else {
            panic!("details did not open");
        };
        assert!(body.contains("Still checking"), "{body}");
        app.ui.dialogs.clear();
        wait(&mut app);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        let Some(super::super::modal::Modal::Text { title, body, .. }) = app.ui.dialog() else {
            panic!("details did not open");
        };
        assert_eq!(title, "opencode");
        for expected in [
            "Executable: /usr/bin/opencode",
            "Version: opencode v2.0.22 (untested)",
            "Tested versions: 1.4.3",
            "✗ missing   --variant (unattended, interactive) · opencode run --help",
            "✗ missing   --interactive (interactive)",
            "✓ supported --model",
            "- Detected version is not a parser-tested version.",
            "[h] on the workflow page",
        ] {
            assert!(body.contains(expected), "{expected}\n{body}");
        }
        assert_eq!(body.matches(AUTH_NOTE).count(), 1);
    }

    #[test]
    fn workflow_warns_only_from_cached_results() {
        let mut app = app();
        app.editor.stage_mut().harness = "opencode".into();
        let warning = "⚠ opencode 2.0.22 lacks --variant, --interactive; see Harnesses [4]";
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(260, 40)).unwrap();
        let mut draw = |app: &mut App| {
            terminal
                .draw(|frame| super::super::app::draw(frame, app))
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };
        assert!(!draw(&mut app).contains("lacks"));
        assert!(
            !app.harnesses.checks.checking,
            "Workflow must not run doctor"
        );
        app.harnesses.checks.reports = stub(&app.editor.settings, &app.project);
        assert!(draw(&mut app).contains(warning));
        app.editor.stage_mut().harness = "codex".into();
        assert!(!draw(&mut app).contains("lacks"));
    }
}
