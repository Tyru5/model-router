//! Routing stats in the TUI: the Stats dialog ([I]) and the selected stage's
//! history hint. Read-only: nothing here changes a stage's assignment.
use super::app::App;
use crate::{
    config::Stage,
    stats::{self, Stats},
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// How long drawn hints may use stats before they are collected again.
const REFRESH: Duration = Duration::from_secs(60);

/// Stats of the open project, collected on first use and refreshed after
/// `REFRESH`, a project switch, or opening the dialog.
#[derive(Default)]
pub(super) struct StatsCache(Mutex<Option<(PathBuf, Instant, Arc<Stats>)>>);

impl StatsCache {
    pub(super) fn get(&self, project: &Path) -> Arc<Stats> {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((path, at, stats)) = slot.as_ref() {
            if path == project && at.elapsed() < REFRESH {
                return stats.clone();
            }
        }
        let stats = Arc::new(Stats::collect(&[project.to_path_buf()]));
        *slot = Some((project.to_path_buf(), Instant::now(), stats.clone()));
        stats
    }
    fn invalidate(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

impl App {
    pub(super) fn show_stats(&mut self) {
        self.route_stats.invalidate();
        let stats = self.route_stats.get(&self.project);
        self.text("Routing stats · suggestions only", stats.table(None));
    }

    /// "History: claude/opus succeeded 9/10 here vs 4/8 — [I] stats" when
    /// saved runs show a clearly better assignment for the stage's role.
    pub(super) fn stats_hint(&self, stage: &Stage) -> Option<String> {
        if !stage.enabled {
            return None;
        }
        let stats = self.route_stats.get(&self.project);
        stats::suggest(&stats, stage.role, &stage.harness, &stage.model)
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
        *app.route_stats.0.lock().unwrap() =
            Some((project.path().into(), Instant::now(), Arc::new(seeded)));
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
        match app.ui.dialog() {
            Some(Modal::Text { title, body, .. }) => {
                assert!(title.starts_with("Routing stats"), "{title}");
                assert!(body.contains("No finished stages recorded yet"), "{body}");
            }
            _ => panic!("Stats did not open"),
        }
    }
}
