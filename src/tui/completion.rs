//! The Monitor's results view for a finished run: what changed, which checks
//! ran, what is unresolved, and what each stage took and spent. Git work runs
//! on a background thread; the view shows "Computing changes…" until it lands.
use super::{app::App, theme::Palette};
use crate::{
    engine::{Run, Status},
    handoff::TestStatus,
    report::{self, Changes, ItemKind, RunReport, StageReport, UsageTotal},
    snapshot::ChangeKind,
    storage,
};
use anyhow::{Result, bail, ensure};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, HighlightSpacing, Paragraph, Row, Table, Wrap},
};
use std::{
    sync::mpsc::{self, TryRecvError},
    thread,
};

/// Diff text the dialog renders; the full patch stays in changes.diff.
const DIFF_VIEW_LIMIT: usize = 256 * 1024;

/// Changes and patch computed for one finished run state.
pub(super) struct Loaded {
    key: String,
    changes: Changes,
    patch: std::result::Result<String, String>,
}

pub(super) fn finished(run: &Run) -> bool {
    matches!(
        run.status,
        Status::Complete | Status::Failed | Status::Blocked | Status::Cancelled
    )
}

/// Identifies what the changes were computed from.
fn key(run: &Run) -> String {
    format!("{} {:?} {:?}", run.id, run.status, run.change_range())
}

fn load(run: &Run) -> Loaded {
    let patch = report::diff(run)
        .map_err(|e| format!("{e:#}"))
        .map(|mut text| {
            if text.len() > DIFF_VIEW_LIMIT {
                let mut end = DIFF_VIEW_LIMIT;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                text.push_str(&format!(
                    "\n[Shown up to {} KiB. Full patch: {}]\n",
                    DIFF_VIEW_LIMIT / 1024,
                    run.directory.join("changes.diff").display()
                ));
            }
            text
        });
    Loaded {
        key: key(run),
        changes: report::changes(run),
        patch,
    }
}

impl App {
    /// Whether the Monitor shows results rather than the output log.
    pub(super) fn results_shown(&self) -> bool {
        self.focused.worker.is_none()
            && !self.focused.show_output
            && self.focused.run.as_ref().is_some_and(finished)
    }

    fn loaded(&self) -> Option<&Loaded> {
        let run = self.focused.run.as_ref()?;
        self.focused.changes.as_ref().filter(|l| l.key == key(run))
    }

    /// Collects finished Git work, starts it for a newly finished run, and
    /// announces a pending waiver once per checkpoint.
    pub(super) fn poll_completion(&mut self) {
        if let Some((_, receiver)) = &self.focused.changes_job {
            match receiver.try_recv() {
                Ok(loaded) => {
                    self.focused.changes = Some(loaded);
                    self.focused.changes_job = None;
                }
                Err(TryRecvError::Disconnected) => self.focused.changes_job = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        let Some(run) = &self.focused.run else { return };
        if let Some(reason) = run.pending_waiver() {
            let shown = format!("{} {}", run.id, run.revision);
            if self.focused.waiver_shown != shown {
                self.message = format!(
                    "The test stage reported verification is not applicable: {}. [a] accepts without checks; [y] retries.",
                    storage::clean_terminal(reason)
                );
                self.focused.waiver_shown = shown;
            }
        }
        if self.focused.worker.is_some() || !finished(run) {
            return;
        }
        let key = key(run);
        let current = self.focused.changes.as_ref().is_some_and(|l| l.key == key)
            || self
                .focused
                .changes_job
                .as_ref()
                .is_some_and(|(k, _)| *k == key);
        if current {
            return;
        }
        // A newly finished run opens on its results.
        self.focused.show_output = false;
        self.focused.results_scroll = [0; 2];
        let run = run.clone();
        let (sender, receiver) = mpsc::channel();
        self.focused.changes_job = Some((key, receiver));
        thread::spawn(move || {
            let _ = sender.send(load(&run));
        });
    }

    pub(super) fn toggle_results(&mut self) -> Result<()> {
        ensure!(
            self.focused.worker.is_none() && self.focused.run.as_ref().is_some_and(finished),
            "Results appear when the run finishes"
        );
        self.focused.show_output = !self.focused.show_output;
        Ok(())
    }

    pub(super) fn show_diff(&mut self) -> Result<()> {
        ensure!(
            self.focused.worker.is_none() && self.focused.run.as_ref().is_some_and(finished),
            "The diff is available when the run finishes"
        );
        let Some(loaded) = self.loaded() else {
            self.message = "Computing changes…".into();
            return Ok(());
        };
        match &loaded.patch {
            Ok(patch) if patch.trim().is_empty() => {
                self.message = "No files changed during this run.".into()
            }
            Ok(patch) => {
                let patch = patch.clone();
                self.text(
                    "Changes during this run · [↑↓] scroll · [Esc] closes",
                    patch,
                );
            }
            Err(error) => bail!("{error}"),
        }
        Ok(())
    }
}

/// "2/2 ✓", "1/2 ✗", "0/1 –", or "–" when nothing ran.
pub(super) fn checks_cell(statuses: impl IntoIterator<Item = TestStatus>) -> String {
    let (mut passed, mut total, mut failed) = (0, 0, false);
    for status in statuses {
        total += 1;
        passed += usize::from(status == TestStatus::Passed);
        failed |= status == TestStatus::Failed;
    }
    match total {
        0 => "–".into(),
        _ if failed => format!("{passed}/{total} ✗"),
        _ if passed == total => format!("{passed}/{total} ✓"),
        _ => format!("{passed}/{total} –"),
    }
}

/// Cost and tokens in table width, e.g. "$0.25 · 2.0k/400"; "partial" when
/// some attempts reported nothing.
fn usage_cell(total: &UsageTotal) -> String {
    if total.attempts == 0 {
        return "–".into();
    }
    if total.reported == 0 {
        return "not reported".into();
    }
    let sum = &total.sum;
    let mut parts = vec![];
    if let Some(cost) = sum.cost_usd {
        parts.push(report::dollars(cost));
    }
    if sum.input_tokens.is_some() || sum.output_tokens.is_some() {
        let side = |n: Option<u64>| n.map_or_else(|| "?".into(), report::tokens);
        parts.push(format!(
            "{}/{} tok",
            side(sum.input_tokens),
            side(sum.output_tokens)
        ));
    }
    if total.reported < total.attempts {
        parts.push("partial".into());
    }
    parts.join(" · ")
}

fn attempts_cell(stage: &StageReport) -> String {
    match stage.retries {
        0 => stage.attempts.to_string(),
        n => format!("{} ↻{n}", stage.attempts),
    }
}

fn changes_lines(
    loaded: Option<&Loaded>,
    report: &RunReport,
    palette: Palette,
) -> Vec<Line<'static>> {
    let Some(loaded) = loaded else {
        return vec![Line::styled(
            "Computing changes…",
            Style::default()
                .fg(palette.muted)
                .add_modifier(Modifier::ITALIC),
        )];
    };
    let mut lines = vec![
        Line::styled("Changed during this run", palette.header_style()),
        Line::raw(report::changes_status(&loaded.changes)),
    ];
    match &loaded.changes {
        Changes::Tracked { files } => {
            for file in files {
                let color = match file.status {
                    ChangeKind::Added => palette.success,
                    ChangeKind::Deleted => palette.red,
                    ChangeKind::Modified | ChangeKind::TypeChanged => palette.warn,
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} ", file.status.letter()),
                        Style::default().fg(color),
                    ),
                    Span::raw(storage::clean_terminal(&file.path)),
                    Span::styled(
                        format!("  {}", report::counts(file)),
                        Style::default().fg(palette.muted),
                    ),
                ]));
            }
        }
        _ if !report.reported_files.is_empty() => {
            lines.push(Line::styled(
                "Model-reported (unverified):",
                Style::default().fg(palette.muted),
            ));
            for file in &report.reported_files {
                lines.push(Line::raw(format!("  {}", storage::clean_terminal(file))));
            }
        }
        _ => {}
    }
    lines
}

/// The review summary stage's Markdown brief, with headings emphasized.
fn review_lines(report: &RunReport, palette: Palette) -> Vec<Line<'static>> {
    let Some(review) = &report.review else {
        return vec![];
    };
    let mut lines = vec![Line::styled("Review summary", palette.header_style())];
    for line in storage::clean_terminal(review).lines() {
        let heading = line.trim_start_matches('#');
        lines.push(if heading.len() < line.len() {
            Line::styled(
                heading.trim().to_owned(),
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Line::styled(line.to_owned(), Style::default().fg(palette.fg))
        });
    }
    lines.push(Line::raw(""));
    lines
}

fn findings_lines(report: &RunReport, palette: Palette) -> Vec<Line<'static>> {
    let mut lines = vec![];
    // Best-of-N stages: the judge's choice first, then each candidate.
    for stage in &report.stages {
        let Some(fan) = &stage.fan_out else {
            continue;
        };
        lines.push(Line::styled(
            format!("Fan-out · {}", stage.name),
            palette.header_style(),
        ));
        for (index, line) in report::fan_out_text(fan).into_iter().enumerate() {
            let color = if index == 0 && !fan.applied {
                palette.warn
            } else if line.starts_with('★') {
                palette.success
            } else {
                palette.fg
            };
            lines.push(Line::styled(
                storage::clean_terminal(&line),
                Style::default().fg(color),
            ));
        }
        lines.push(Line::raw(""));
    }
    let checks: Vec<_> = report
        .stages
        .iter()
        .flat_map(|s| s.checks.iter().map(move |c| (c, s.name.as_str())))
        .collect();
    if checks.is_empty() {
        lines.push(Line::styled(
            "Checks: none run or reported",
            palette.header_style(),
        ));
    } else {
        lines.push(Line::styled("Checks performed", palette.header_style()));
    }
    for (check, stage) in checks {
        let color = match check.status {
            TestStatus::Passed => palette.success,
            TestStatus::Failed => palette.red,
            TestStatus::NotRun => palette.warn,
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} ", report::check_glyph(check.status)),
                Style::default().fg(color),
            ),
            Span::raw(storage::clean_terminal(&check.command)),
            Span::styled(
                format!("  {}", report::check_detail(check, stage)),
                Style::default().fg(palette.muted),
            ),
        ]));
    }
    lines.push(Line::raw(""));
    if report.unresolved.is_empty() {
        lines.push(Line::styled("Unresolved: none", palette.header_style()));
    } else {
        lines.push(Line::styled("Unresolved", palette.header_style()));
    }
    for item in &report.unresolved {
        let color = match item.kind {
            ItemKind::Error | ItemKind::Blocking | ItemKind::FailedCheck => palette.red,
            ItemKind::Unresolved | ItemKind::Waiver | ItemKind::OptionalCheck => palette.warn,
            ItemKind::Unstructured | ItemKind::Advisory => palette.muted,
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} ", report::item_glyph(item.kind)),
                Style::default().fg(color),
            ),
            Span::styled(
                storage::clean_terminal(&report::item_text(item)),
                Style::default().fg(if item.kind == ItemKind::Error {
                    palette.red
                } else {
                    palette.fg
                }),
            ),
        ]));
    }
    lines
}

/// Header lines, stage table, then changes beside (or above) checks and
/// unresolved items. `area` excludes the title row the Monitor draws.
pub(super) fn draw_completion(
    frame: &mut Frame<'_>,
    app: &App,
    run: &Run,
    area: Rect,
    palette: Palette,
) {
    // The report is rebuilt only when the run changes, not every frame.
    let stamp = format!("{} {}", key(run), run.revision);
    let report = {
        let mut cache = app.focused.cache.borrow_mut();
        match &cache.report {
            Some((cached, report)) if *cached == stamp => report.clone(),
            _ => {
                let report = std::rc::Rc::new(report::build(run));
                cache.report = Some((stamp, report.clone()));
                report
            }
        }
    };
    let summary = Line::styled(report::summary(&report), Style::default().fg(palette.fg));
    let usage = Line::from(vec![
        Span::styled("Usage ", palette.header_style()),
        Span::styled(
            report::usage_text(&report.usage),
            Style::default().fg(if report.usage.reported == 0 {
                palette.muted
            } else {
                palette.fg
            }),
        ),
    ]);
    let width = area.width.max(1);
    let mut header = vec![summary, usage];
    if let Some(error) = &report.error {
        header.push(Line::styled(
            format!("Error: {}", storage::clean_terminal(error)),
            Style::default().fg(palette.red),
        ));
    }
    let header_rows = header
        .iter()
        .map(|line| super::widgets::rows_for(line, width, 2))
        .sum();
    let chunks = Layout::vertical([
        Constraint::Length(header_rows),
        Constraint::Length((report.stages.len() as u16 + 3).min(10)),
        Constraint::Min(3),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(header).wrap(Wrap { trim: false }), chunks[0]);
    let name_width = report
        .stages
        .iter()
        .map(|s| s.name.chars().count() as u16)
        .max()
        .unwrap_or(0)
        .clamp(8, 24);
    let rows = report.stages.iter().map(|stage| {
        let muted = Style::default().fg(palette.muted);
        let ran = stage.attempts > 0;
        Row::new([
            Cell::from(stage.name.clone()),
            Cell::from(palette.status_span(stage.status)),
            Cell::from(attempts_cell(stage)).style(muted),
            Cell::from(if ran {
                report::duration(stage.active_seconds)
            } else {
                "–".into()
            })
            .style(muted),
            Cell::from(checks_cell(stage.checks.iter().map(|c| c.status))).style(muted),
            Cell::from(usage_cell(&stage.usage)).style(muted),
        ])
    });
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(name_width),
                Constraint::Length(12),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(6),
                Constraint::Fill(1),
            ],
        )
        .column_spacing(2)
        .header(
            Row::new(["STAGE", "STATUS", "ATTEMPTS", "ACTIVE", "CHECKS", "USAGE"])
                .style(palette.header_style()),
        )
        .block(palette.panel(" Stages "))
        .highlight_spacing(HighlightSpacing::Never),
        chunks[1],
    );
    let loaded = app.loaded();
    // The review brief leads: it is what the person reads first.
    let mut changes = review_lines(&report, palette);
    changes.extend(changes_lines(loaded, &report, palette));
    let left = if report.review.is_some() {
        "Review summary · Changes"
    } else {
        "Changes"
    };
    let findings = findings_lines(&report, palette);
    let mut limits = [0; 2];
    let mut panel = |index: usize,
                     title: &str,
                     lines: Vec<Line<'static>>,
                     keys: &[(&str, &str)],
                     rect: Rect| {
        // Rows the wrapped text takes, less what fits: how far it can scroll.
        let inner = rect.width.saturating_sub(2);
        let rows: u16 = lines
            .iter()
            .map(|line| super::widgets::rows_for(line, inner, u16::MAX))
            .fold(0, u16::saturating_add);
        limits[index] = rows.saturating_sub(rect.height.saturating_sub(2));
        let scroll = app.focused.results_scroll[index].min(limits[index]);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0))
                .block(palette.panel_with(title, keys)),
            rect,
        );
    };
    if chunks[2].width >= 110 {
        let cols = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(chunks[2]);
        panel(0, left, changes, &[("d", "diff"), ("o", "output")], cols[0]);
        panel(
            1,
            "Checks · Unresolved",
            findings,
            &[("PgUp/PgDn", "scroll")],
            cols[1],
        );
    } else {
        let mut lines = changes;
        lines.push(Line::raw(""));
        lines.extend(findings);
        panel(
            0,
            "Results",
            lines,
            &[("d", "diff"), ("o", "output"), ("PgUp/PgDn", "scroll")],
            chunks[2],
        );
    }
    app.focused.results_limit.set(limits);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{report::tests::sample, tui::app::draw};
    use ratatui::{Terminal, backend::TestBackend};
    use std::sync::{Arc, atomic::AtomicBool};

    fn app(run: Run) -> App {
        let dir = std::env::temp_dir();
        let mut app = App::new(
            dir.clone(),
            crate::config::Settings::default(),
            dir.join("model-router-test-config.toml"),
            Arc::new(AtomicBool::new(false)),
        );
        app.focused.run = Some(run);
        app.page = 1;
        app
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn tracked() -> Loaded {
        Loaded {
            key: key(&sample()),
            changes: Changes::Tracked {
                files: vec![crate::snapshot::FileChange {
                    path: "src/lib.rs".into(),
                    status: ChangeKind::Modified,
                    added: Some(40),
                    removed: Some(5),
                }],
            },
            patch: Ok("diff --git a/src/lib.rs b/src/lib.rs\n+fresh line\n".into()),
        }
    }

    #[test]
    fn finished_runs_show_results_at_both_sizes() {
        let mut app = app(sample());
        assert!(app.results_shown());
        let text = screen(&mut app, 80, 24);
        assert!(text.contains("Computing changes…"), "{text}");
        app.focused.changes = Some(tracked());
        for (width, height) in [(80, 24), (120, 36)] {
            let text = screen(&mut app, width, height);
            for expected in [
                "wall 2m 10s · active 1m 40s · 3 attempts · 1 retry",
                "usage reported by 2 of 3",
                "Changed during this run",
                "1 file, +40 -5",
                "M src/lib.rs  +40 -5",
                "Implement",
                "2 ↻1",
                "1m 10s",
                "–",
            ] {
                assert!(
                    text.contains(expected),
                    "{width}x{height} lacks {expected:?}\n{text}"
                );
            }
            if width == 120 {
                for expected in [
                    "Checks performed",
                    "✓ cargo test  required · router · Verify",
                    "Unresolved",
                    "! Implement: blocking finding: Data loss",
                    "1/3 ✗",
                ] {
                    assert!(
                        text.contains(expected),
                        "{width}x{height} lacks {expected:?}\n{text}"
                    );
                }
            }
        }
    }

    #[test]
    fn fan_out_candidates_show_in_progress_and_results() {
        let mut run = sample();
        run.stages[0].fan_out = Some(
            serde_json::from_value(serde_json::json!({
                "base": "tree",
                "base_commit": "commit",
                "candidates": [
                    {"id": "c1", "harness": "claude", "model": "sonnet", "status": "complete",
                     "attempts": [], "handoff": null,
                     "changes": {"files": ["a.rs"], "added": 5, "removed": 1, "patch": "/p1.diff"}},
                    {"id": "c2", "harness": "codex", "model": "", "status": "failed",
                     "attempts": [], "handoff": null, "error": "Harness exited"}
                ],
                "selection": {"candidate": "c1", "by": "auto", "reason": "auto: passing",
                              "applied": true, "at": "2026-01-01T00:00:00Z"}
            }))
            .unwrap(),
        );
        let mut results = app(run.clone());
        results.focused.changes = Some(tracked());
        let text = screen(&mut results, 120, 40);
        for expected in [
            "Fan-out · Implement",
            "judge auto: c1 applied (auto: passing)",
            "★ c1 claude sonnet complete · 1 file +5 −1",
            "· c2 codex default failed · Harness exited",
        ] {
            assert!(text.contains(expected), "lacks {expected:?}\n{text}");
        }
        // While it runs, the monitor lists candidates under their stage.
        run.status = Status::Running;
        let mut monitor = app(run);
        let text = screen(&mut monitor, 120, 40);
        assert!(text.contains("★ c1 · claude sonnet"), "{text}");
        assert!(text.contains("↳ c2 · codex"), "{text}");
    }

    #[test]
    fn stopped_runs_highlight_the_error_and_label_missing_changes() {
        let mut run = sample();
        run.status = Status::Failed;
        run.error = Some("Required check failed".into());
        let mut app = app(run);
        app.focused.changes = Some(Loaded {
            key: key(app.focused.run.as_ref().unwrap()),
            changes: Changes::NotGit,
            patch: Err("Changes are unavailable (not a Git repository)".into()),
        });
        let text = screen(&mut app, 120, 36);
        assert!(text.contains("✗ failed"), "{text}");
        assert!(text.contains("✗ Required check failed"), "{text}");
        assert!(text.contains("Error: Required check failed"), "{text}");
        assert!(
            text.contains("unavailable (not a Git repository)"),
            "{text}"
        );
        assert!(text.contains("Model-reported (unverified):"));
        assert!(app.show_diff().is_err());
    }

    #[test]
    fn diff_and_output_toggle_use_the_loaded_changes() {
        let mut app = app(sample());
        app.show_diff().unwrap();
        assert_eq!(app.message, "Computing changes…");
        assert!(app.modal.is_none());
        app.focused.changes = Some(tracked());
        app.show_diff().unwrap();
        assert!(
            matches!(&app.modal, Some(super::super::modal::Modal::Text { body, .. }) if body.contains("+fresh line"))
        );
        app.modal = None;
        app.toggle_results().unwrap();
        assert!(!app.results_shown());
        assert!(screen(&mut app, 80, 24).contains("Output"));
        app.toggle_results().unwrap();
        assert!(app.results_shown());
        let mut running = sample();
        running.status = Status::AwaitingConfirmation;
        app.focused.run = Some(running);
        assert!(app.toggle_results().is_err());
        assert!(app.show_diff().is_err());
    }

    #[test]
    fn background_job_loads_changes_for_a_finished_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut run = sample();
        run.project = dir.path().into();
        run.workspace = dir.path().into();
        run.directory = dir.path().join("run");
        let mut app = app(run);
        app.focused.show_output = true;
        app.poll_completion();
        assert!(app.focused.changes_job.is_some());
        assert!(
            !app.focused.show_output,
            "A newly finished run opens on its results"
        );
        for _ in 0..500 {
            app.poll_completion();
            if app.focused.changes_job.is_none() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        let loaded = app.loaded().expect("changes loaded");
        assert!(matches!(loaded.changes, Changes::NotGit));
        // The same state does not start another job.
        app.poll_completion();
        assert!(app.focused.changes_job.is_none());
    }

    #[test]
    fn pending_waiver_explains_accept_and_retry() {
        let mut run = sample();
        run.status = Status::AwaitingConfirmation;
        run.cursor = 1;
        run.workflow.stages[1].role = crate::config::Role::Test;
        run.stages[1].waiver = None;
        run.stages[1].attempts[0].checks.clear();
        let handoff = run.stages[1].handoff.as_mut().unwrap();
        handoff.tests.clear();
        handoff.not_applicable = Some("Docs only".into());
        assert_eq!(run.pending_waiver(), Some("Docs only"));
        let mut app = app(run);
        app.poll_completion();
        assert_eq!(
            app.message,
            "The test stage reported verification is not applicable: Docs only. [a] accepts without checks; [y] retries."
        );
        assert!(
            app.focused.changes_job.is_none(),
            "Unfinished runs compute no changes"
        );
        // Announced once per checkpoint, so later messages stay visible.
        app.message = "Other".into();
        app.poll_completion();
        assert_eq!(app.message, "Other");
        assert!(!app.results_shown());
        for (width, height) in [(80, 24), (120, 36)] {
            let text = screen(&mut app, width, height);
            assert!(text.contains("Verification not applicable"), "{text}");
            assert!(text.contains("[a] accepts without checks"), "{text}");
            assert!(text.contains("CHECKS"), "{text}");
        }
    }

    #[test]
    fn cells_stay_compact() {
        use TestStatus::*;
        assert_eq!(checks_cell([]), "–");
        assert_eq!(checks_cell([Passed, Passed]), "2/2 ✓");
        assert_eq!(checks_cell([Passed, Failed]), "1/2 ✗");
        assert_eq!(checks_cell([NotRun]), "0/1 –");
        let report = report::build(&sample());
        assert_eq!(
            usage_cell(&report.stages[0].usage),
            "$0.25 · 1.2k/300 tok · partial"
        );
        assert_eq!(usage_cell(&report.stages[1].usage), "800/100 tok");
        assert_eq!(usage_cell(&report.stages[2].usage), "–");
    }
}
