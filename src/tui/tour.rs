//! The getting-started tour: a panel above the status line that says what to
//! do next, from the Harnesses page to a first completed run.
//!
//! Each step comes from what the app shows, not from a counter: the step
//! advances when the user acts, and going back (clearing the task, closing
//! the run) brings its step back. `F1` shows or hides the panel. It opens by
//! itself on the first TUI launch on this machine; someone who used the TUI
//! before the tour existed gets a one-time status line about `F1` instead.
//! `<config dir>/tour.json` records either, so neither happens again.
use super::{
    app::{App, Page},
    theme::Palette,
    widgets::rows_for,
};
use crate::{config, engine::Status};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// In the config directory: whether the tour was finished, hidden, or offered.
const FILE: &str = "tour.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    /// A run the tour followed completed.
    Finished,
    /// Hidden with `F1` before that.
    Hidden,
    /// An earlier user was told about `F1` instead of the tour opening.
    Offered,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    outcome: Outcome,
}

pub(super) struct Tour {
    pub(super) open: bool,
    /// The Harnesses page has shown check results.
    seen_harnesses: bool,
    /// The run the tour follows: the first one started while it is open.
    run: Option<String>,
    /// Where the outcome is saved; tests point it at a scratch file.
    pub(super) path: PathBuf,
}

impl Default for Tour {
    fn default() -> Self {
        Self {
            open: false,
            seen_harnesses: false,
            run: None,
            path: config::config_dir().join(FILE),
        }
    }
}

/// What a TUI launch does about the tour.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Launch {
    /// The first launch on this machine: the tour opens.
    Open,
    /// The first launch since the tour exists, by someone who opened projects
    /// before: the status line mentions `F1` once.
    Offer,
    /// The tour was finished, hidden, or offered already.
    Quiet,
}

/// Status line text for [`Launch::Offer`].
pub(super) const OFFER: &str = "New: [F1] opens a tour that walks you to a completed run.";

/// Decides what this launch does about the tour, recording an offer. Call
/// it before the project is remembered, which fills the recent projects.
pub(super) fn on_launch() -> Launch {
    launch_at(
        &config::config_dir().join(FILE),
        crate::storage::recent_projects().is_empty(),
    )
}

fn launch_at(path: &Path, no_recent_projects: bool) -> Launch {
    if path.exists() {
        Launch::Quiet
    } else if no_recent_projects {
        Launch::Open
    } else {
        save(path, Outcome::Offered);
        Launch::Offer
    }
}

fn save(path: &Path, outcome: Outcome) {
    // A failed write only means the tour may open again next launch.
    let _ = crate::storage::write_json(path, &Saved { outcome });
}

fn saved(path: &Path) -> Option<Outcome> {
    crate::storage::read_json::<Saved>(path)
        .ok()
        .map(|saved| saved.outcome)
}

/// Where the user is on the way to a completed run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// Harnesses has not shown which agent CLIs are installed yet, e.g. for
    /// a project just opened.
    Harnesses,
    /// Checks found no installed harness.
    NoHarness,
    /// Away from Workflow before a run started.
    Workflow,
    /// Every stage is off, so no run can start.
    NoStage,
    /// An enabled stage, or one of its fan-out candidates, uses a harness
    /// that is not installed.
    Stage {
        name: String,
        harness: String,
        /// A fan-out candidate uses it, which `[h]` does not change.
        candidate: bool,
    },
    Task,
    Start,
    /// The followed run is under way.
    Watch(Watch),
    Done,
}

/// What the followed run needs, as far as the tour can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Watch {
    /// Runs is not shown.
    Away,
    /// Runs shows another run, which the run keys would act on.
    Unselected,
    Running,
    Interactive,
    Handoff,
    /// Its USD budget stopped it; retrying stops again.
    Budget,
    /// Its repair limit stopped it.
    Repair,
    /// Failed, blocked, or cancelled for another reason.
    Stopped,
}

impl Step {
    /// The step's place in the tour, out of [`STEPS`].
    fn number(&self) -> usize {
        match self {
            Self::Harnesses | Self::NoHarness => 1,
            Self::Workflow | Self::NoStage | Self::Stage { .. } => 2,
            Self::Task => 3,
            Self::Start => 4,
            Self::Watch(_) => 5,
            Self::Done => 6,
        }
    }
    fn title(&self) -> &'static str {
        match self {
            Self::Harnesses | Self::NoHarness => "Harnesses",
            Self::Workflow | Self::NoStage | Self::Stage { .. } => "Stages",
            Self::Task => "Task",
            Self::Start => "Start",
            Self::Watch(_) => "Your run",
            Self::Done => "Done",
        }
    }
}
const STEPS: usize = 6;

/// The panel's text width at the minimum 65-column screen. Every step's
/// last line, the one naming the next key, fits it, so a short panel that
/// keeps only that line never clips it.
#[cfg(test)]
const ACTION_WIDTH: usize = 59;

/// The step's text, ending with the next action; `[key]` marks a key.
fn text(step: &Step, page: Page) -> Vec<String> {
    let lines: &[&str] = match step {
        Step::Harnesses if page == Page::Harnesses => &["Checking which harnesses are installed…"],
        Step::Harnesses => &[
            "Model Router runs one task through a workflow of stages. Each stage runs an agent CLI (a harness) with one model, and hands its findings to the next.",
            "Press [3] to see which harnesses are installed.",
        ],
        Step::NoHarness if page == Page::Harnesses => &[
            "No harness is installed. Install one (Claude Code, Codex, Antigravity, OpenCode, …) and log in to it.",
            "Then press [r] to check again.",
        ],
        Step::NoHarness => &[
            "No harness is installed. Install one and log in to it.",
            "Then press [3], then [r] to check again.",
        ],
        Step::Workflow => &[
            "✓ marks installed harnesses.",
            "Press [1] to set up your first run on Workflow.",
        ],
        Step::NoStage => &[
            "Every stage is off, so the run has nothing to do.",
            "Select one with [←] [→] and press [o] to turn it on.",
        ],
        Step::Stage {
            name,
            harness,
            candidate,
        } => {
            let (uses, action) = if *candidate {
                (
                    "has a fan-out candidate on",
                    "[e], then Fan-out candidates; or [o] turns it off.",
                )
            } else {
                ("uses", "[h] picks an installed harness; [o] turns it off.")
            };
            return vec![
                format!(
                    "Stage “{name}” {uses} {harness}, which is not installed. Select it with [←] [→]."
                ),
                action.into(),
            ];
        }
        Step::Task => &[
            "The strip shows this workflow's stages, run left to right. [e] edits the selected stage; [w] switches workflow or adds a preset.",
            "Press [t], describe a small task, then [Ctrl+S].",
        ],
        Step::Start => &[
            "[P] previews the exact commands without calling a model. Each stage runs your harness with its own login, so it may use paid requests.",
            "Press [r] to start the run.",
        ],
        Step::Watch(Watch::Away) => &["Your run has started.", "Press [2] to follow it on Runs."],
        Step::Watch(Watch::Unselected) => &[
            "Another run is selected, and run keys act on it.",
            "Select yours with [↑] [↓], or [L] lists runs.",
        ],
        Step::Watch(Watch::Running) => &[
            "Each stage's output streams into the feed; [PgUp] [PgDn] scroll and [C] cancels the stage.",
            "A card appears here when the run needs you.",
        ],
        Step::Watch(Watch::Interactive) => &[
            "This stage is interactive. Work with the harness in its terminal, then exit it (/exit) to hand off to the next stage.",
            "Press [t] to open the harness's terminal.",
        ],
        Step::Watch(Watch::Handoff) => &[
            "The stage finished and wrote a handoff for the next one.",
            "[h] inspects it; [a] accepts it and moves on.",
        ],
        Step::Watch(Watch::Budget) => &[
            "The run reached its USD budget; a retry would stop again.",
            "[B] raises the cap and continues.",
        ],
        Step::Watch(Watch::Repair) => &[
            "Review asked for more repairs than the workflow allows.",
            "[R] allows one more repair pass; [h] shows the review.",
        ],
        Step::Watch(Watch::Stopped) => &[
            "The run stopped; [h] shows why.",
            "[y] retries, [m] changes model, [i] retries interactively.",
        ],
        Step::Done => &[
            "Your first run is complete. The brief shows what changed, the checks, and the cost; [d] shows the diff.",
            "On Workflow [1], [w] adds presets; [?] lists every key.",
            "[F1] closes the tour.",
        ],
    };
    lines.iter().map(|line| (*line).to_owned()).collect()
}

impl App {
    /// Shows or hides the tour. Hiding an unfinished tour saves that, so it
    /// does not open by itself again.
    pub(super) fn toggle_tour(&mut self) {
        if self.tour.open {
            self.tour.open = false;
            if saved(&self.tour.path) != Some(Outcome::Finished) {
                save(&self.tour.path, Outcome::Hidden);
            }
            self.ui.message = "Tour hidden. [F1] shows it again.".into();
        } else {
            self.tour.open = true;
            self.tour.run = None;
            self.ui.message = "Tour: the panel above says what to do next. [F1] hides it.".into();
        }
    }

    /// Follows a run started while the tour is open.
    pub(super) fn tour_started(&mut self, id: &str) {
        if self.tour.open && self.tour.run.is_none() {
            self.tour.run = Some(id.to_owned());
        }
    }

    /// Notes what the tour waits on; called before each frame.
    pub(super) fn sync_tour(&mut self) {
        if !self.tour.open {
            return;
        }
        if self.ui.page == Page::Harnesses && !self.harnesses.checks.reports.is_empty() {
            self.tour.seen_harnesses = true;
        }
        // A closed run is no longer followed; the next one started is.
        if let Some(id) = &self.tour.run {
            if !self.session.runs.iter().any(|slot| &slot.run.id == id) {
                self.tour.run = None;
            }
        }
        if self.tour_step() == Step::Done && saved(&self.tour.path) != Some(Outcome::Finished) {
            save(&self.tour.path, Outcome::Finished);
        }
    }

    pub(super) fn tour_step(&self) -> Step {
        let followed = self
            .tour
            .run
            .as_ref()
            .and_then(|id| self.session.runs.iter().find(|slot| &slot.run.id == id));
        if let Some(slot) = followed {
            return self.watch(&slot.run);
        }
        let checks = &self.harnesses.checks;
        // A project switch clears the checks; until Harnesses checks again,
        // nothing is known about the new project.
        if !self.tour.seen_harnesses || checks.reports.is_empty() {
            return Step::Harnesses;
        }
        if !checks.reports.iter().any(|report| report.installed()) {
            return Step::NoHarness;
        }
        if self.ui.page != Page::Workflow {
            return Step::Workflow;
        }
        let workflow = &self.editor.settings.workflows[self.editor.workflow];
        if !workflow.stages.iter().any(|stage| stage.enabled) {
            return Step::NoStage;
        }
        // The harnesses run creation requires: ship stages run in the router,
        // and a fan-out stage runs its candidates instead of its own harness.
        let missing = workflow
            .stages
            .iter()
            .filter(|stage| stage.enabled && stage.role != config::Role::Ship)
            .flat_map(|stage| {
                let harnesses: Vec<String> = match &stage.fan_out {
                    Some(fan) => fan
                        .candidates
                        .iter()
                        .map(|candidate| candidate.stage(stage).harness)
                        .collect(),
                    None => vec![stage.harness.clone()],
                };
                let candidate = stage.fan_out.is_some();
                harnesses
                    .into_iter()
                    .map(move |harness| (stage, harness, candidate))
            })
            .find(|(_, harness, _)| {
                checks
                    .report(harness)
                    .is_some_and(|report| !report.installed())
            });
        if let Some((stage, harness, candidate)) = missing {
            return Step::Stage {
                name: stage.name.clone(),
                harness,
                candidate,
            };
        }
        if self.editor.task.trim().is_empty() {
            Step::Task
        } else {
            Step::Start
        }
    }

    /// The step for the followed run.
    fn watch(&self, run: &crate::engine::Run) -> Step {
        if run.status == Status::Complete {
            return Step::Done;
        }
        if self.ui.page != Page::Runs {
            return Step::Watch(Watch::Away);
        }
        if !self.monitoring() || self.session.run().is_none_or(|shown| shown.id != run.id) {
            return Step::Watch(Watch::Unselected);
        }
        Step::Watch(match run.status {
            Status::Interactive => Watch::Interactive,
            Status::AwaitingConfirmation => Watch::Handoff,
            Status::Failed | Status::Blocked | Status::Cancelled => {
                if crate::cost::usd_budget_blocked(run) {
                    Watch::Budget
                } else if run.repair_blocked() {
                    Watch::Repair
                } else {
                    Watch::Stopped
                }
            }
            _ => Watch::Running,
        })
    }
}

/// `text` with each `[key]` styled as a key.
fn keyed(text: &str, palette: Palette) -> Line<'static> {
    let key = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD);
    let mut spans = vec![];
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find(']').map(|end| open + end) else {
            break;
        };
        spans.push(Span::raw(rest[..open].to_owned()));
        spans.push(Span::styled(rest[open..=close].to_owned(), key));
        rest = &rest[close + 1..];
    }
    spans.push(Span::raw(rest.to_owned()));
    Line::from(spans)
}

/// The panel's lines for the current step, or none when the tour is closed.
pub(super) fn lines(app: &App, palette: Palette) -> Vec<Line<'static>> {
    if !app.tour.open {
        return vec![];
    }
    text(&app.tour_step(), app.ui.page)
        .iter()
        .map(|text| keyed(text, palette))
        .collect()
}

/// The lines that fit in `max` rows at `width`, borders included, and the
/// rows they take. The last line says what to do, so the first ones give way.
pub(super) fn fit(
    mut lines: Vec<Line<'static>>,
    width: u16,
    max: u16,
) -> (Vec<Line<'static>>, u16) {
    let inner = width.saturating_sub(4);
    loop {
        let text: u16 = lines.iter().map(|line| rows_for(line, inner, 4)).sum();
        if lines.is_empty() || text + 2 <= max {
            let height = if lines.is_empty() { 0 } else { text + 2 };
            return (lines, height);
        }
        if lines.len() == 1 {
            // Only below the minimum width: actions fit `ACTION_WIDTH`.
            return (lines, max);
        }
        lines.remove(0);
    }
}

pub(super) fn draw(
    frame: &mut Frame<'_>,
    app: &App,
    lines: Vec<Line<'static>>,
    area: Rect,
    palette: Palette,
) {
    let step = app.tour_step();
    let title = format!("Tour · {} of {STEPS} · {}", step.number(), step.title());
    let block = palette.modal(&title, &[("F1", "hide")]);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_in_brackets_are_styled_and_the_text_is_kept() {
        let line = keyed(
            "Press [t], then [Ctrl+S] [x",
            super::super::theme::Theme::default().palette(),
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Press [t], then [Ctrl+S] [x");
        let keys: Vec<&str> = line
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(keys, ["[t]", "[Ctrl+S]"]);
    }

    #[test]
    fn the_tour_opens_on_a_first_launch_and_earlier_users_hear_of_it_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        // Opening does not record anything: finishing or hiding does.
        assert_eq!(launch_at(&path, true), Launch::Open);
        assert!(!path.exists());
        assert_eq!(launch_at(&path, false), Launch::Offer);
        assert_eq!(saved(&path), Some(Outcome::Offered));
        assert_eq!(launch_at(&path, false), Launch::Quiet);
        assert_eq!(launch_at(&path, true), Launch::Quiet);
    }

    #[test]
    fn every_step_ends_with_an_action_that_fits_the_narrowest_panel() {
        use unicode_width::UnicodeWidthStr;
        let watches = [
            Watch::Away,
            Watch::Unselected,
            Watch::Running,
            Watch::Interactive,
            Watch::Handoff,
            Watch::Budget,
            Watch::Repair,
            Watch::Stopped,
        ];
        let steps = [
            Step::Harnesses,
            Step::NoHarness,
            Step::Workflow,
            Step::NoStage,
            Step::Stage {
                name: "A stage with a long name".into(),
                harness: "opencode".into(),
                candidate: false,
            },
            Step::Stage {
                name: "A stage with a long name".into(),
                harness: "opencode".into(),
                candidate: true,
            },
            Step::Task,
            Step::Start,
            Step::Done,
        ]
        .into_iter()
        .chain(watches.map(Step::Watch));
        for step in steps {
            for page in Page::ALL {
                let lines = text(&step, page);
                let action = lines.last().unwrap();
                assert!(
                    action.width() <= ACTION_WIDTH,
                    "{step:?} on {page:?}: {action}"
                );
            }
        }
    }

    #[test]
    fn a_finished_tour_stays_finished_when_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        assert_eq!(saved(&path), None);
        save(&path, Outcome::Finished);
        assert_eq!(saved(&path), Some(Outcome::Finished));
    }
}
