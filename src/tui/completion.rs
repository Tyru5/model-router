//! The brief a finished run shows: what changed, which checks ran, what is
//! unresolved, and what each stage took and spent. Git work runs
//! on a background thread; the view shows "Computing changes…" until it lands.
use super::{
    app::App,
    run_slot::RunSlot,
    tasks::{Msg, Tasks},
    theme::Palette,
    widgets::{key_chips, pad, rows_for},
};
use crate::{
    engine::{Run, Status},
    handoff::TestStatus,
    report::{self, Changes, ItemKind, RunReport, StageReport, UsageTotal},
    snapshot::ChangeKind,
    storage,
};
use anyhow::{Result, bail};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

/// Changes and patch computed for one finished run state.
pub(super) struct Loaded {
    key: String,
    changes: Changes,
    patch: std::result::Result<String, String>,
}

/// The run's report, rebuilt only when the run changes, not every frame.
pub(super) fn cached_report(slot: &RunSlot) -> std::rc::Rc<RunReport> {
    let run = &slot.run;
    let stamp = format!("{} {}", key(run), run.revision);
    let mut cache = slot.cache.borrow_mut();
    match &cache.report {
        Some((cached, report)) if *cached == stamp => report.clone(),
        _ => {
            let report = std::rc::Rc::new(report::build(run));
            cache.report = Some((stamp, report.clone()));
            report
        }
    }
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

/// The directory Git ran in for this run: its workspace, else the project.
pub(super) fn run_dir(run: &Run) -> std::path::PathBuf {
    if run.workspace.is_dir() {
        run.workspace.clone()
    } else {
        run.project.clone()
    }
}

/// The top of the repository holding `dir`, which Git paths start from.
pub(super) fn repo_root(dir: &std::path::Path) -> std::path::PathBuf {
    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim_end_matches(['\n', '\r'])
                .to_owned()
        })
        .filter(|root| !root.is_empty())
        .map_or_else(|| dir.to_path_buf(), std::path::PathBuf::from)
}

/// The patch is kept whole (Git caps it); the diff view lays out only what
/// shows and bounds each file's rows.
fn load(run: &Run) -> Loaded {
    let patch = report::diff(run).map_err(|e| format!("{e:#}"));
    Loaded {
        key: key(run),
        changes: report::changes(run),
        patch,
    }
}

impl RunSlot {
    /// Whether the run shows its brief rather than the output feed.
    pub(super) fn results_shown(&self) -> bool {
        self.worker.is_none() && !self.show_output && finished(&self.run)
    }

    fn loaded(&self) -> Option<&Loaded> {
        self.changes.as_ref().filter(|l| l.key == key(&self.run))
    }

    /// Computed changes for `key`; kept when still the ones waited for.
    pub(super) fn changes_read(&mut self, key: &str, result: Result<Loaded, String>) {
        if self.changes_pending.as_deref() != Some(key) {
            return;
        }
        self.changes_pending = None;
        if let Ok(loaded) = result {
            self.changes = Some(loaded);
        }
    }

    /// Starts Git work for a newly finished run. Returns the status line for
    /// a pending waiver, once per checkpoint.
    fn poll_completion(&mut self, tasks: &Tasks) -> Option<String> {
        let run = &self.run;
        let mut message = None;
        if let Some(reason) = run.pending_waiver() {
            let shown = format!("{} {}", run.id, run.revision);
            if self.waiver_shown != shown {
                message = Some(format!(
                    "The test stage reported verification is not applicable: {}. [a] accepts without checks; [y] retries.",
                    storage::clean_terminal(reason)
                ));
                self.waiver_shown = shown;
            }
        }
        if self.worker.is_some() || !finished(run) {
            return message;
        }
        let key = key(run);
        let current = self.changes.as_ref().is_some_and(|l| l.key == key)
            || self.changes_pending.as_ref() == Some(&key);
        if current {
            return message;
        }
        // A newly finished run opens on its brief, unless a search is being
        // typed into its feed: the next keys still belong to the query.
        self.show_output = self.log_search.as_ref().is_some_and(|s| s.typing);
        self.results_scroll = 0;
        self.changes_pending = Some(key.clone());
        let run = run.clone();
        let id = run.id.clone();
        tasks.spawn(
            "Computing changes stopped unexpectedly",
            move || Ok(load(&run)),
            move |result| Msg::Changes {
                run: id,
                key,
                result,
            },
        );
        message
    }
}

impl App {
    /// Whether the focused run shows its brief.
    pub(super) fn results_shown(&self) -> bool {
        self.session.focused().is_some_and(RunSlot::results_shown)
    }

    /// See `RunSlot::poll_completion`; only the focused run is followed.
    pub(super) fn poll_completion(&mut self) {
        let tasks = &self.tasks;
        let message = self
            .session
            .focused_mut()
            .and_then(|slot| slot.poll_completion(tasks));
        if let Some(message) = message {
            self.ui.message = message;
        }
    }

    /// Where the focused run's files are.
    pub(super) fn run_dir(&self) -> Result<std::path::PathBuf> {
        Ok(run_dir(
            self.session
                .run()
                .ok_or_else(|| anyhow::anyhow!("No run selected"))?,
        ))
    }

    /// The focused run once it has finished.
    fn finished_slot(&mut self, reason: &str) -> Result<&mut RunSlot> {
        match self.session.focused_mut() {
            Some(slot) if slot.worker.is_none() && finished(&slot.run) => Ok(slot),
            _ => bail!("{reason}"),
        }
    }

    pub(super) fn toggle_results(&mut self) -> Result<()> {
        let slot = self.finished_slot("Results appear when the run finishes")?;
        slot.show_output = !slot.show_output;
        Ok(())
    }

    pub(super) fn show_diff(&mut self) -> Result<()> {
        let slot = self.finished_slot("The diff is available when the run finishes")?;
        let Some(patch) = slot.loaded().map(|loaded| loaded.patch.clone()) else {
            self.ui.message = "Computing changes…".into();
            return Ok(());
        };
        match &patch {
            Ok(patch) if patch.trim().is_empty() => {
                self.ui.message = "No files changed during this run.".into()
            }
            Ok(patch) => {
                let view = super::diff_view::DiffView::new(patch.clone());
                self.ui.open(super::modal::Modal::Diff(Box::new(view)));
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

/// Cost and tokens in table width, e.g. "~$0.25 · 2.0k/400 tok": the known
/// cost, "~" when any part is estimated; "partial" when some attempts
/// reported nothing. The header names the sources.
fn usage_cell(total: &UsageTotal) -> String {
    if total.attempts == 0 {
        return "–".into();
    }
    if total.reported == 0 {
        return "not reported".into();
    }
    let sum = &total.sum;
    let mut parts = vec![];
    if total.cost.known() {
        parts.push(total.cost.amount());
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
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "Brief",
            Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  from the review summary stage",
            Style::default().fg(palette.muted),
        ),
    ])];
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

/// Stage rows under a heading: status, harness, attempts, active time,
/// checks, and usage. Narrow panes leave out the harness.
fn stage_lines(report: &RunReport, width: u16, palette: Palette) -> Vec<Line<'static>> {
    let name_width = report
        .stages
        .iter()
        .map(|s| s.name.width())
        .max()
        .unwrap_or(0)
        .clamp(8, 24);
    let harness = |stage: &StageReport| {
        if stage.model.is_empty() {
            stage.harness.clone()
        } else {
            format!("{} {}", stage.harness, stage.model)
        }
    };
    let harness_width = report
        .stages
        .iter()
        .map(|s| harness(s).width())
        .max()
        .unwrap_or(0)
        .clamp(7, 28);
    let show_harness = usize::from(width) >= name_width + harness_width + 56;
    let row = |cells: [String; 6]| {
        let [name, harness, attempts, active, checks, usage] = cells;
        let mut text = format!("{}  ", pad(&name, name_width + 2));
        if show_harness {
            text.push_str(&format!("{}  ", pad(&harness, harness_width)));
        }
        text.push_str(&format!(
            "{}  {}  {}  {usage}",
            pad(&attempts, 8),
            pad(&active, 7),
            pad(&checks, 6)
        ));
        text
    };
    let mut lines = vec![Line::styled(
        row(["Stages", "HARNESS", "ATTEMPTS", "ACTIVE", "CHECKS", "USAGE"].map(String::from)),
        palette.header_style(),
    )];
    for stage in &report.stages {
        let ran = stage.attempts > 0;
        let text = row([
            format!("  {}", stage.name),
            harness(stage),
            attempts_cell(stage),
            if ran {
                report::duration(stage.active_seconds)
            } else {
                "–".into()
            },
            checks_cell(stage.checks.iter().map(|c| c.status)),
            usage_cell(&stage.usage),
        ]);
        // The glyph replaces the indent, coloured by the stage's status.
        let rest = text.chars().skip(1).collect::<String>();
        lines.push(Line::from(vec![
            Span::styled(
                Palette::status_glyph(stage.status),
                Style::default().fg(palette.status_color(stage.status)),
            ),
            Span::styled(
                rest,
                Style::default().fg(if ran { palette.fg } else { palette.muted }),
            ),
        ]));
    }
    lines
}

/// Wall and active time, attempts, usage, and cost for the whole run.
fn total_lines(report: &RunReport, palette: Palette) -> Vec<Line<'static>> {
    let value = |known: bool| Style::default().fg(if known { palette.fg } else { palette.muted });
    vec![
        Line::styled("Totals", palette.header_style()),
        Line::styled(report::summary(report), Style::default().fg(palette.fg)),
        Line::from(vec![
            Span::styled("Usage ", palette.header_style()),
            Span::styled(
                report::usage_text(&report.usage),
                value(report.usage.reported > 0),
            ),
        ]),
        Line::from(vec![
            Span::styled("Cost ", palette.header_style()),
            Span::styled(
                report::cost_text(&report.usage.cost, &report.budget),
                value(report.usage.cost.known()),
            ),
        ]),
    ]
}

/// What a finished run offers next, the likeliest first. A stopped fan-out
/// stage cannot be retried interactively, nor can a ship stage, which never
/// runs a harness. A run its repair limit stopped leads with another repair
/// pass; one its budget stopped offers only a raise, as a retry would stop
/// again on the same cap.
fn next_keys(run: &Run) -> Vec<(&'static str, &'static str)> {
    let current = run.workflow.stages.get(run.cursor);
    let fan_out = current.is_some_and(|s| s.fan_out.is_some());
    let ship = current.is_some_and(|s| s.role == crate::config::Role::Ship);
    let keys: &[(&str, &str)] = if crate::cost::budget_blocked(run) {
        if crate::cost::usd_budget_blocked(run) {
            &[("B", "raise budget"), ("d", "diff"), ("o", "output")]
        } else {
            &[("d", "diff"), ("o", "output")]
        }
    } else if run.repair_blocked() {
        &[
            ("R", "allow repair"),
            ("y", "retry"),
            ("i", "interactive"),
            ("m", "model"),
            ("d", "diff"),
            ("o", "output"),
        ]
    } else if run.status == Status::Complete {
        &[
            ("d", "diff"),
            ("o", "output"),
            ("h", "handoff"),
            ("n", "rerun"),
            ("x", "close"),
        ]
    } else {
        &[
            ("y", "retry"),
            ("i", "interactive"),
            ("m", "model"),
            ("d", "diff"),
            ("o", "output"),
        ]
    };
    keys.iter()
        .copied()
        .filter(|(key, _)| !((fan_out || ship) && *key == "i"))
        .collect()
}

/// The finished run's brief in one scrolling column: its error, the review
/// brief, what changed, checks and unresolved items, each stage, and the
/// totals. What to do next stays pinned below.
pub(super) fn draw_completion(frame: &mut Frame<'_>, slot: &RunSlot, area: Rect, palette: Palette) {
    let report = cached_report(slot);
    let mut lines = vec![];
    if let Some(error) = &report.error {
        lines.push(Line::styled(
            format!("Error: {}", storage::clean_terminal(error)),
            Style::default()
                .fg(palette.red)
                .add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::raw(""));
    }
    // The review brief leads: it is what the person reads first.
    lines.extend(review_lines(&report, palette));
    lines.extend(changes_lines(slot.loaded(), &report, palette));
    lines.push(Line::raw(""));
    lines.extend(findings_lines(&report, palette));
    lines.push(Line::raw(""));
    lines.extend(stage_lines(&report, area.width, palette));
    lines.push(Line::raw(""));
    lines.extend(total_lines(&report, palette));

    let next = key_chips(palette, &next_keys(&slot.run), area.width.into());
    let [body, bar] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(next_height(&next, area.width)),
    ])
    .areas(area);
    // Rows the wrapped text takes, less what fits: how far it can scroll.
    let rows: u16 = lines
        .iter()
        .map(|line| rows_for(line, body.width, u16::MAX))
        .fold(0, u16::saturating_add);
    let limit = rows.saturating_sub(body.height);
    slot.results_limit.set(limit);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((slot.results_scroll.min(limit), 0)),
        body,
    );
    draw_next(frame, next, limit > 0, bar, palette);
}

/// Rows the Next bar takes: its rule, then its keys.
pub(super) fn next_height(keys: &Line<'_>, width: u16) -> u16 {
    rows_for(keys, width, 1) + 1
}

/// A rule titled Next over the keys for what a finished run offers;
/// `scrolls` adds the scroll keys to the rule.
pub(super) fn draw_next(
    frame: &mut Frame<'_>,
    keys: Line<'static>,
    scrolls: bool,
    area: Rect,
    palette: Palette,
) {
    let mut block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(palette.line))
        .title(Span::styled(" Next ", palette.header_style()));
    if scrolls {
        block = block.title(
            Line::styled(" PgUp/PgDn scroll ", Style::default().fg(palette.muted)).right_aligned(),
        );
    }
    frame.render_widget(
        Paragraph::new(keys).wrap(Wrap { trim: false }).block(block),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::super::app::Page;
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
        // Runs of this session belong to the open project.
        let mut run = run;
        run.project = app.project.clone();
        app.session.adopt(run);
        app.ui.page = Page::Runs;
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
    fn finished_runs_show_their_brief_at_both_sizes() {
        let mut app = app(sample());
        assert!(app.results_shown());
        let text = screen(&mut app, 80, 24);
        assert!(text.contains("Computing changes…"), "{text}");
        app.slot_mut().changes = Some(tracked());
        // What changed and what was checked lead, under the header's totals.
        let lead = [
            "✓ complete  2m 10s  ·  cost $0.25+? reported  ·  repairs 1/2",
            "✓ Implement ━▶ ✓ Verify ┄▶ – Review",
            "Changed during this run",
            "1 file, +40 -5",
            "M src/lib.rs  +40 -5",
            "Checks performed",
            "✓ cargo test  required · router · Verify",
            "[d] diff   [o] output",
        ];
        let rest = [
            "Unresolved",
            "! Implement: blocking finding: Data loss",
            "Stages",
            "✓ Implement",
            "2 ↻1",
            "1m 10s",
            "1/3 ✗",
            "wall 2m 10s · active 1m 40s · 3 attempts · 1 retry",
            "usage reported by 2 of 3",
            "Cost $0.25 (reported) · cost unknown for 2 of 3 attempts",
        ];
        let lacks = |text: &str, expected: &[&str]| {
            expected
                .iter()
                .find(|e| !text.contains(*e))
                .map(|e| format!("lacks {e:?}\n{text}"))
        };
        let text = screen(&mut app, 120, 60);
        assert_eq!(lacks(&text, &lead).or(lacks(&text, &rest)), None);
        // The footer offers no keys that do nothing on a finished run.
        let footer = text.lines().last().unwrap();
        for key in ["[a]", "[c]", "[y]"] {
            assert!(!footer.contains(key), "{footer}");
        }
        let text = screen(&mut app, 80, 24);
        assert_eq!(lacks(&text, &lead), None);
        // A short pane scrolls the brief to the rest, keeping the Next bar.
        app.slot_mut().results_scroll = u16::MAX;
        let text = screen(&mut app, 80, 24);
        assert_eq!(lacks(&text, &rest[2..]), None);
        assert!(text.contains("[d] diff"), "{text}");
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
        results.slot_mut().changes = Some(tracked());
        let text = screen(&mut results, 120, 40);
        for expected in [
            "Fan-out · Implement",
            "judge auto: c1 applied (auto: passing)",
            "★ c1 claude sonnet complete · 1 file +5 −1",
            "· c2 codex default failed · Harness exited",
        ] {
            assert!(text.contains(expected), "lacks {expected:?}\n{text}");
        }
        // While it runs, a line under the strip lists the candidates.
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
        app.slot_mut().changes = Some(Loaded {
            key: key(app.session.run().unwrap()),
            changes: Changes::NotGit,
            patch: Err("Changes are unavailable (not a Git repository)".into()),
        });
        let text = screen(&mut app, 120, 36);
        assert!(text.contains("✗ failed"), "{text}");
        assert!(text.contains("✗ Required check failed"), "{text}");
        assert!(text.contains("Error: Required check failed"), "{text}");
        assert!(text.contains("[y] retry   [i] interactive"), "{text}");
        // A ship stage never runs a harness, so it has no interactive retry.
        app.slot_mut().run.cursor = 0;
        app.slot_mut().run.workflow.stages[0].role = crate::config::Role::Ship;
        let shipped = screen(&mut app, 120, 36);
        assert!(shipped.contains("[y] retry   [m] model"), "{shipped}");
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
        assert_eq!(app.ui.message, "Computing changes…");
        assert!(app.ui.dialog().is_none());
        app.slot_mut().changes = Some(tracked());
        app.show_diff().unwrap();
        assert!(
            matches!(app.ui.dialog(), Some(super::super::modal::Modal::Diff(view)) if view.patch.contains("+fresh line"))
        );
        app.ui.dialogs.clear();
        app.toggle_results().unwrap();
        assert!(!app.results_shown());
        assert!(screen(&mut app, 80, 24).contains("[o] brief"));
        app.toggle_results().unwrap();
        assert!(app.results_shown());
        let mut running = sample();
        running.status = Status::AwaitingConfirmation;
        app.session.adopt(running);
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
        app.slot_mut().show_output = true;
        app.poll_completion();
        assert!(app.slot().changes_pending.is_some());
        assert!(
            !app.slot().show_output,
            "A newly finished run opens on its results"
        );
        for _ in 0..500 {
            app.poll().unwrap();
            if app.slot().changes_pending.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let loaded = app
            .session
            .focused()
            .unwrap()
            .loaded()
            .expect("changes loaded");
        assert!(matches!(loaded.changes, Changes::NotGit));
        // The same state does not start another job.
        app.poll_completion();
        assert!(app.slot().changes_pending.is_none());
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
            app.ui.message,
            "The test stage reported verification is not applicable: Docs only. [a] accepts without checks; [y] retries."
        );
        assert!(
            app.slot().changes_pending.is_none(),
            "Unfinished runs compute no changes"
        );
        // Announced once per checkpoint, so later messages stay visible.
        app.ui.message = "Other".into();
        app.poll_completion();
        assert_eq!(app.ui.message, "Other");
        assert!(!app.results_shown());
        for (width, height) in [(80, 24), (120, 36)] {
            let text = screen(&mut app, width, height);
            assert!(
                text.contains("◆ Verify: verification not applicable"),
                "{text}"
            );
            assert!(text.contains("Docs only"), "{text}");
            assert!(text.contains("[a] accept without checks"), "{text}");
            assert!(
                text.contains("◆ Verify"),
                "the strip marks the waiting stage"
            );
        }
    }

    #[test]
    fn a_handoff_waiting_on_the_person_shows_a_card_under_its_output() {
        let mut run = sample();
        run.status = Status::AwaitingConfirmation;
        run.cursor = 0;
        // Stages after the one waiting have not run yet.
        run.stages[1].status = Status::Pending;
        let mut handoff =
            crate::handoff::Handoff::unstructured("Scoped the key.\nTwo\nThree\nFour".into());
        handoff.structured = true;
        handoff.files_changed = vec!["a.rs".into(), "b.rs".into()];
        handoff.unresolved = vec!["Sweep stale keys?".into()];
        run.stages[0].handoff = Some(handoff);
        let mut app = app(run);
        app.slot_mut()
            .push_log("── Implement · attempt 2 · claude sonnet ──".into());
        app.slot_mut().push_log("● Fixed it".into());
        let text = screen(&mut app, 120, 36);
        for expected in [
            // Log headings become stage headings, the output under them indented.
            "Implement  attempt 2  claude sonnet",
            "  ● Fixed it",
            "◆ Implement needs you",
            "Accept it to continue to Verify.",
            "2 files changed   1 unresolved",
            "Scoped the key.",
            "… [h] shows the rest",
            "[a] accept   [t] reopen session",
            "◆ Implement ━▶ ○ Verify",
        ] {
            assert!(text.contains(expected), "lacks {expected:?}\n{text}");
        }
        assert!(!text.contains("Four"), "{text}");
        // At 80 columns keys that do not fit are left out, never cut from their labels.
        let narrow = screen(&mut app, 80, 24);
        assert!(narrow.contains("[a] accept"), "{narrow}");
        for line in narrow.lines().filter(|line| line.starts_with(" │")) {
            let inner = line.trim_end().trim_end_matches('│').trim_end();
            assert!(
                !inner.ends_with(']') || inner.ends_with("More: [Space]"),
                "{narrow}"
            );
        }
        // Diff waits for the run to finish, so the footer leaves it out.
        let footer = text.lines().last().unwrap();
        assert!(!footer.contains("[d] diff"), "{footer}");
        // The command menu lists only what applies to a run of this session.
        app.key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(':'),
            crossterm::event::KeyModifiers::NONE,
        ))
        .unwrap();
        let Some(super::super::modal::Modal::Picker { items, .. }) = app.ui.dialog() else {
            panic!("command menu");
        };
        assert!(items.iter().any(|c| c.label.contains("Accept handoff")));
        for absent in ["Open the selected saved run", "Preview removing"] {
            assert!(!items.iter().any(|c| c.label.contains(absent)), "{absent}");
        }
        app.ui.dialogs.clear();
        // A review's verdict cannot be rewritten, so its summary is not offered.
        app.slot_mut().run.workflow.stages[0].role = crate::config::Role::Review;
        let text = screen(&mut app, 120, 36);
        assert!(text.contains("[a] accept"), "{text}");
        assert!(!text.contains("[e] edit summary"), "{text}");
        // Nor at a ship plan, which has no session to reopen.
        app.slot_mut().run.workflow.stages[0].role = crate::config::Role::Ship;
        let text = screen(&mut app, 120, 36);
        assert!(
            text.contains("[a] accept   [y] retry   [h] handoff"),
            "{text}"
        );
        app.slot_mut().run.workflow.stages[0].role = crate::config::Role::Implementation;
        // Nor for a harness that cannot resume a session.
        app.slot_mut().run.workflow.stages[0].harness = "mock".into();
        let text = screen(&mut app, 120, 36);
        assert!(
            text.contains("[a] accept   [y] retry   [e] edit summary"),
            "{text}"
        );
        app.slot_mut().run.workflow.stages[0].harness = "claude".into();
        // In a graph the next stage depends on what finishes, so none is named.
        let first = app.slot().run.workflow.stages[0].id.clone();
        app.slot_mut().run.workflow.stages[1].after = vec![first];
        let text = screen(&mut app, 120, 36);
        assert!(text.contains("Accept it to continue the run."), "{text}");
        app.slot_mut().run.workflow.stages[1].after.clear();
        // A run left running by an earlier execution leads with retry.
        app.slot_mut().run.status = Status::Running;
        let text = screen(&mut app, 120, 36);
        assert!(text.contains("◆ Implement was interrupted"), "{text}");
        assert!(text.contains("[y] retry   [i]"), "{text}");
        app.slot_mut().run.status = Status::AwaitingConfirmation;
        // A long summary is cut, never the keys under it.
        app.slot_mut().run.stages[0]
            .handoff
            .as_mut()
            .unwrap()
            .summary = "word ".repeat(400);
        let narrow = screen(&mut app, 80, 24);
        assert!(narrow.contains("[a] accept"), "{narrow}");
        assert!(narrow.contains("More: [Space]"), "{narrow}");
        // A fan-out stage has no session to reopen.
        app.slot_mut().run.workflow.stages[0].fan_out = Some(crate::config::FanOut {
            candidates: vec![],
            judge: "auto".into(),
        });
        let text = screen(&mut app, 120, 36);
        assert!(text.contains("[a] accept   [y] retry"), "{text}");
        assert!(!text.contains("[t] reopen"), "{text}");
        // Off stages are passed over in naming the next one.
        app.slot_mut().run.workflow.stages[1].enabled = false;
        let text = screen(&mut app, 120, 36);
        assert!(text.contains("Accept it to finish the run."), "{text}");
    }

    #[test]
    fn a_search_left_from_the_live_run_does_not_take_the_briefs_keys() {
        let mut app = app(sample());
        app.slot_mut().log_search = Some(super::super::output_search::LogSearch {
            query: "cache".into(),
            typing: false,
            current: 0,
        });
        assert!(app.results_shown());
        app.key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('n'),
            crossterm::event::KeyModifiers::NONE,
        ))
        .unwrap();
        assert!(
            matches!(
                app.ui.dialog(),
                Some(super::super::modal::Modal::Confirm(_))
            ),
            "[n] reruns from the brief"
        );
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
