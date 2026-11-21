//! Routing stats in the TUI: the Stats dialog ([I]) and the selected stage's
//! history hint. Read-only: nothing here changes a stage's assignment.
use super::{
    app::App,
    modal::{Loading, Modal},
    tasks::Msg,
};
use crate::{
    config::Stage,
    stats::{self, Stats},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

/// Fallback interval before stale stats are re-collected when no run finished.
const FALLBACK_REFRESH: Duration = Duration::from_secs(300);

/// Stats of the open project, collected in the background while the
/// Workflow page shows, on demand / run completion or after `FALLBACK_REFRESH`.
#[derive(Default)]
pub(super) struct StatsCache {
    collected: Option<(PathBuf, Instant, Arc<Stats>)>,
    pub(super) collecting: bool,
    stale: bool,
}

impl StatsCache {
    pub(super) fn invalidate(&mut self) {
        self.stale = true;
    }
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }
    /// The last stats collected for `project`.
    fn get(&self, project: &Path) -> Option<&Stats> {
        self.collected
            .as_ref()
            .filter(|(path, _, _)| path == project)
            .map(|(_, _, stats)| stats.as_ref())
    }
    /// Whether `project`'s stats are missing or old, and none are being collected.
    fn due(&self, project: &Path) -> bool {
        !self.collecting
            && (self.stale
                || self.collected.as_ref().is_none_or(|(path, at, _)| {
                    path != project || at.elapsed() >= FALLBACK_REFRESH
                }))
    }
}

impl App {
    /// Collects stats for the hint when they are due.
    pub(super) fn refresh_stats(&mut self) {
        if self.route_stats.due(&self.project) {
            self.collect_stats(None);
        }
    }

    /// Collects stats; `ticket` is the Stats dialog's wait, if any.
    fn collect_stats(&mut self, ticket: Option<u64>) {
        self.route_stats.collecting = true;
        let project = self.project.clone();
        let path = project.clone();
        self.tasks.spawn(
            "Collecting routing stats stopped unexpectedly",
            move || Ok(Arc::new(Stats::collect(&[path]))),
            move |result| Msg::Stats {
                project,
                ticket,
                result,
            },
        );
    }

    /// Opens the Stats dialog once fresh stats are collected.
    pub(super) fn show_stats(&mut self) {
        let loading = Loading::new("Collecting routing stats from saved runs…");
        self.collect_stats(Some(loading.ticket));
        self.ui.open(Modal::Loading(loading));
    }

    /// Stats that arrived; the dialog shows them if it still waits.
    pub(super) fn stats_collected(
        &mut self,
        project: PathBuf,
        ticket: Option<u64>,
        result: Result<Arc<Stats>, String>,
    ) {
        self.route_stats.collecting = false;
        let waiting = ticket.is_some_and(|ticket| self.ui.take_loading(ticket));
        match result {
            Ok(stats) => {
                if waiting {
                    self.text("Routing stats · suggestions only", stats.table(None));
                }
                if project == self.project {
                    self.route_stats.stale = false;
                }
                self.route_stats.collected = Some((project, Instant::now(), stats));
            }
            Err(error) if waiting => self.ui.error(error),
            Err(_) => {}
        }
    }

    /// "History: claude/opus succeeded 9/10 here vs 4/8 — [I] stats" when
    /// saved runs show a clearly better assignment for the stage's role.
    pub(super) fn stats_hint(&self, stage: &Stage) -> Option<String> {
        if !stage.enabled {
            return None;
        }
        let stats = self.route_stats.get(&self.project)?;
        stats::suggest(stats, stage.role, &stage.harness, &stage.model)
            .map(|s| crate::storage::clean_terminal(&format!("{} — [I] stats", s.text())))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{app::draw, modal::Modal};
    use super::*;
    use crate::{config::Settings, stats::Sample};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::sync::atomic::AtomicBool;

    fn samples(harness: &str, model: &str, clean: usize, total: usize) -> Vec<Sample> {
        (0..total)
            .map(|i| Sample {
                run: format!("run-{harness}-{i}"),
                stage: "research".into(),
                role: crate::config::Role::Research,
                harness: harness.into(),
                model: model.into(),
                attempts: 1,
                clean: i < clean,
                retries: 0,
                failed: u32::from(i >= clean),
                blocked: 0,
                repairs_caused: 0,
                repair_requests: 0,
                first_pass: None,
                seconds: Some(30),
                usage: None,
                checks_passed: 0,
                checks_total: 0,
                verified: None,
                cost_usd: None,
            })
            .collect()
    }

    #[test]
    fn stage_panel_hints_a_better_assignment_and_i_opens_stats() {
        let project = tempfile::tempdir().unwrap();
        let mut app = App::new(
            project.path().into(),
            Settings::default(),
            project.path().join("config.toml"),
            Arc::new(AtomicBool::new(false)),
        );
        app.ui.theme_path = project.path().join("appearance.json");
        app.harnesses.checks.probe = |_, _| vec![];
        let stage = app.editor.stage().clone();
        assert_eq!(app.stats_hint(&stage), None);
        let mut all = samples(&stage.harness, &stage.model, 1, 5);
        all.extend(samples("claude", "opus", 5, 5));
        let seeded = Stats {
            metrics: stats::aggregate(&all),
            ..Stats::default()
        };
        app.route_stats.collected = Some((project.path().into(), Instant::now(), Arc::new(seeded)));
        assert_eq!(
            app.stats_hint(&stage).as_deref(),
            Some("History: claude/opus succeeded 5/5 here vs 1/5 — [I] stats")
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 40)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("History: claude/opus"), "{screen}");
        // Advice never changes the stage.
        assert_eq!(app.editor.stage().harness, stage.harness);
        app.key(KeyEvent::new(KeyCode::Char('I'), KeyModifiers::SHIFT))
            .unwrap();
        // Collected fresh in the background while the dialog waits.
        assert!(matches!(app.ui.dialog(), Some(Modal::Loading(_))));
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while matches!(app.ui.dialog(), Some(Modal::Loading(_))) && Instant::now() < deadline {
            app.poll().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        match app.ui.dialog() {
            Some(Modal::Text { title, body, .. }) => {
                assert!(title.starts_with("Routing stats"), "{title}");
                assert!(body.contains("No finished stages recorded yet"), "{body}");
            }
            _ => panic!("Stats did not open"),
        }
    }
}
