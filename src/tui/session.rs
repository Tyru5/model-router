//! The session's runs as a whole: the notice queue behind the status line,
//! the session runs dialog (`L`), jumping to a run that needs input (`N`),
//! and the notifications dialog (`F9`).
use super::{
    app::{App, Page},
    modal::{Ask, DialogResult, InputTarget, Modal},
    run_slot::RunSlot,
    theme::{self, Palette},
    widgets::{elapsed, menu_key, popup},
};
use crate::{
    engine::{Action, Run, Status},
    notify::{self, Backends, NotifyEvent},
    storage,
};
use anyhow::{Result, ensure};
use crossterm::event::{KeyCode as Key, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Clear, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap},
};
use std::collections::VecDeque;

/// Notices kept for the history dialog.
pub(super) const NOTICES: usize = 50;

/// Something a run of this session reported, for the status line and `!`.
pub(super) struct Notice {
    pub(super) at: chrono::DateTime<chrono::Local>,
    pub(super) text: String,
    pub(super) alert: bool,
}

/// The runs started or opened in this session, in start order. Each keeps
/// going while another is focused; the Monitor shows the focused one.
#[derive(Default)]
pub(super) struct Session {
    pub(super) runs: Vec<RunSlot>,
    /// Index of the focused run; meaningless while `runs` is empty.
    pub(super) focus: usize,
    /// What runs of this session reported, newest last; `!` lists them.
    pub(super) notices: VecDeque<Notice>,
    /// Notices since the list was last opened.
    pub(super) unseen_notices: usize,
    /// The focused run's interactive stage gets the terminal on the next frame.
    pub(super) launch_interactive: bool,
    /// Exit once the active runs have stopped.
    pub(super) quit_after_stop: bool,
    /// Interactive stages left running in the background on exit, to list
    /// after the TUI closes.
    pub(super) left_running: Vec<String>,
}

impl Session {
    pub(super) fn focused(&self) -> Option<&RunSlot> {
        self.runs.get(self.focus)
    }
    pub(super) fn focused_mut(&mut self) -> Option<&mut RunSlot> {
        self.runs.get_mut(self.focus)
    }
    /// The focused run.
    pub(super) fn run(&self) -> Option<&Run> {
        self.focused().map(|slot| &slot.run)
    }
    pub(super) fn run_mut(&mut self) -> Option<&mut Run> {
        self.focused_mut().map(|slot| &mut slot.run)
    }
    /// Whether the focused run's stage is driving.
    pub(super) fn running(&self) -> bool {
        self.focused().is_some_and(|slot| slot.worker.is_some())
    }
    /// Every run, with whether it is focused.
    pub(super) fn slots(&self) -> impl Iterator<Item = (&RunSlot, bool)> {
        self.runs
            .iter()
            .enumerate()
            .map(|(index, slot)| (slot, index == self.focus))
    }
    pub(super) fn any_active(&self) -> bool {
        self.runs.iter().any(|slot| slot.worker.is_some())
    }
    /// Runs whose interactive harness is running now.
    pub(super) fn live_terminals(&self) -> usize {
        self.runs
            .iter()
            .filter(|slot| slot.terminal().is_some_and(|t| t.live()))
            .count()
    }
    /// Lets go of every running interactive harness without stopping it:
    /// each keeps running in its terminal host for a later mr.
    pub(super) fn release_terminals(&mut self) {
        for slot in &self.runs {
            if let Some(terminal) = slot.terminal().filter(|t| t.live()) {
                terminal.release();
                self.left_running
                    .push(format!("{} · {}", slot.run.id, slot.run.current_name()));
            }
        }
    }
    pub(super) fn cancel_all(&self) {
        for slot in &self.runs {
            // A released harness keeps running; cancelling would stop it.
            if slot.terminal().is_some_and(|t| t.released()) {
                continue;
            }
            if slot.worker.is_some() {
                slot.cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    /// Focuses `run` in a new slot, after the others; they keep going.
    pub(super) fn adopt(&mut self, run: Run) {
        self.runs.push(RunSlot::new(run));
        self.focus = self.runs.len() - 1;
    }
    /// Focuses the run at `index` (start order), if there is one.
    pub(super) fn focus(&mut self, index: usize) {
        if index < self.runs.len() {
            self.focus = index;
        }
    }
    /// Focuses the next or previous run, wrapping.
    pub(super) fn cycle_focus(&mut self, forward: bool) {
        let total = self.runs.len();
        if total < 2 {
            return;
        }
        self.focus = if forward {
            (self.focus + 1) % total
        } else {
            (self.focus + total - 1) % total
        };
    }
    /// Focuses the slot holding run `id`; false when none does.
    pub(super) fn focus_run(&mut self, id: &str) -> bool {
        let index = self.runs.iter().position(|slot| slot.run.id == id);
        if let Some(index) = index {
            self.focus = index;
        }
        index.is_some()
    }
    /// Removes the focused run from this session; its files stay on disk.
    pub(super) fn dismiss(&mut self) -> Result<()> {
        if self.runs.is_empty() {
            return Ok(());
        }
        ensure!(!self.running(), "Cancel the stage before closing its run");
        self.runs.remove(self.focus);
        self.focus = self.focus.min(self.runs.len().saturating_sub(1));
        Ok(())
    }
    /// Stops the stage of run `id`; false when it is not running.
    pub(super) fn cancel_run(&self, id: &str) -> bool {
        match self.runs.iter().find(|slot| slot.run.id == id) {
            Some(slot) if slot.worker.is_some() => {
                slot.cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }
    /// Runs waiting on the user.
    pub(super) fn needing_input(&self) -> usize {
        self.runs.iter().filter(|slot| slot.needs_input()).count()
    }
}

impl RunSlot {
    /// Waiting on the user: an interactive stage ready for its terminal, or
    /// a handoff or waiver to accept.
    pub(super) fn needs_input(&self) -> bool {
        self.worker.is_none()
            && matches!(
                self.run.status,
                Status::Interactive | Status::AwaitingConfirmation
            )
    }
}

/// What the status line says when a run stops driving, naming the run.
pub(super) fn notice_text(run: &Run, result: &std::result::Result<Action, String>) -> String {
    let stage = run.current_name();
    let name = format!("{} ({})", run.workflow.name, run.id);
    match result {
        Ok(Action::Interactive) => {
            format!("{name}: {stage} is ready in its native terminal. [N] jumps to it.")
        }
        Ok(Action::Confirm) if run.pending_waiver().is_some() => {
            format!(
                "{name}: {stage} reports no applicable check; accept or retry. [N] jumps to it."
            )
        }
        Ok(Action::Confirm) => format!("{name}: {stage} awaits handoff review. [N] jumps to it."),
        Ok(Action::Complete) => format!("{name} complete."),
        Ok(Action::Stopped) => format!(
            "{name} stopped: {}",
            run.error.as_deref().unwrap_or("no error recorded")
        ),
        Err(error) => format!("{name}: {error}"),
    }
}

impl App {
    /// Adds a notice and shows it on the status line.
    pub(super) fn push_notice(&mut self, text: String, alert: bool) {
        if self.session.notices.len() == NOTICES {
            self.session.notices.pop_front();
        }
        self.session.notices.push_back(Notice {
            at: chrono::Local::now(),
            text: text.clone(),
            alert,
        });
        self.session.unseen_notices += 1;
        if alert {
            self.ui.alert = text.clone();
        }
        self.ui.message = text;
        self.ui.redraw = true;
    }
    /// How many notices arrived beside the one on the status line.
    pub(super) fn more_notices(&self) -> usize {
        let newest = self
            .session
            .notices
            .back()
            .is_some_and(|n| n.text == self.ui.message);
        if newest {
            self.session.unseen_notices.saturating_sub(1)
        } else {
            0
        }
    }
    pub(super) fn show_notices(&mut self) {
        self.session.unseen_notices = 0;
        let body = if self.session.notices.is_empty() {
            "No notices yet. Runs of this session add one when they stop, need input, or finish."
                .to_owned()
        } else {
            self.session
                .notices
                .iter()
                .rev()
                .map(|n| {
                    format!(
                        "{} {} {}",
                        n.at.format("%H:%M:%S"),
                        if n.alert { "✗" } else { "›" },
                        n.text
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        self.text("Notices · newest first · [Esc] closes", body);
    }
    /// Focuses the next run (after the focused one, wrapping) that needs
    /// input and shows it in the Monitor.
    pub(super) fn jump_to_input(&mut self) {
        let runs = &self.session.runs;
        let total = runs.len();
        let start = if total > 0 { self.session.focus + 1 } else { 0 };
        let Some(index) = (0..total)
            .map(|step| (start + step) % total.max(1))
            .find(|index| runs[*index].needs_input())
        else {
            self.ui.message = "No run of this session needs input.".into();
            return;
        };
        self.session.focus(index);
        self.ui.page = Page::Monitor;
        self.ui.message = self.focus_message();
    }
    /// The status line after focusing a run from the list.
    fn focus_message(&self) -> String {
        let Some(slot) = self.session.focused() else {
            return String::new();
        };
        let run = &slot.run;
        let name = format!("{} ({})", run.workflow.name, run.id);
        match run.status {
            _ if slot.terminal().is_some() => {
                format!("{name}: its interactive session is live. [t] attaches.")
            }
            Status::Interactive if slot.worker.is_none() => {
                format!(
                    "{name}: {} waits for you. [i] opens it.",
                    run.current_name()
                )
            }
            Status::AwaitingConfirmation if run.pending_waiver().is_some() => {
                format!("{name}: [a] accepts without checks; [y] retries.")
            }
            Status::AwaitingConfirmation => {
                format!("{name}: [h] inspects the handoff; [a] advances.")
            }
            status => format!("{name}: {}.", status.label()),
        }
    }
    pub(super) fn session_runs(&mut self) {
        self.ui.open(Modal::SessionRuns {
            selected: if self.session.runs.is_empty() {
                0
            } else {
                self.session.focus
            },
        });
    }
    /// Keys for the session runs dialog.
    pub(super) fn session_runs_key(&mut self, selected: &mut usize, key: KeyEvent) -> DialogResult {
        let total = self.session.runs.len();
        match menu_key(key) {
            Key::Up | Key::Char('k') => *selected = selected.saturating_sub(1),
            Key::Down | Key::Char('j') => *selected = (*selected + 1).min(total.saturating_sub(1)),
            Key::Enter if *selected < total => {
                self.session.focus(*selected);
                self.ui.page = Page::Monitor;
                self.ui.message = self.focus_message();
                return DialogResult::Close;
            }
            Key::Char('n') => {
                self.jump_to_input();
                return DialogResult::Close;
            }
            Key::Char('C') => {
                let Some(slot) = self.session.runs.get(*selected) else {
                    return DialogResult::Stay;
                };
                let run = &slot.run;
                if slot.worker.is_some() {
                    return DialogResult::Replace(Modal::Ask(Box::new(Ask::CancelRun {
                        run: run.id.clone(),
                    })));
                }
                self.ui.message = format!("{} is not running a stage; nothing to cancel.", run.id);
            }
            _ => {}
        }
        DialogResult::Stay
    }
    pub(super) fn notifications(&mut self) {
        self.ui.open(Modal::Notifications {
            backends: self.ui.notifications.clone(),
            selected: 0,
            status: String::new(),
        });
    }
    /// Saves `backends` with the other preferences and uses them from now on.
    pub(super) fn set_notifications(&mut self, backends: Backends) -> Result<()> {
        let preferences = theme::Preferences {
            notifications: backends.clone(),
            ..self.ui.preferences()
        };
        theme::save(&self.ui.theme_path, &preferences)?;
        notify::global().set_backends(backends.clone());
        self.ui.notifications = backends;
        Ok(())
    }
    /// Keys for the notifications dialog.
    pub(super) fn notifications_key(
        &mut self,
        backends: &mut Backends,
        selected: &mut usize,
        status: &mut String,
        key: KeyEvent,
    ) -> Result<DialogResult> {
        match menu_key(key) {
            Key::Up | Key::Char('k') => *selected = selected.saturating_sub(1),
            Key::Down | Key::Char('j') => *selected = (*selected + 1).min(NOTIFY_ROWS - 1),
            Key::Char(' ') | Key::Enter => match *selected {
                0..=2 => {
                    let mut changed = backends.clone();
                    let flag = match *selected {
                        0 => &mut changed.terminal,
                        1 => &mut changed.desktop,
                        _ => &mut changed.webhook,
                    };
                    *flag = !*flag;
                    self.set_notifications(changed.clone())?;
                    *backends = changed;
                    *status = "Saved for every project.".into();
                }
                // Saving the URL reopens this dialog.
                3 => {
                    return Ok(DialogResult::Replace(Modal::input(
                        "Webhook URL (ntfy-style: the body is the message, Title header the title)",
                        InputTarget::NotifyUrl,
                        &backends.webhook_url,
                    )));
                }
                _ => *status = notify::global().deliver(&NotifyEvent::test()).summary(),
            },
            _ => {}
        }
        Ok(DialogResult::Stay)
    }
}

const NOTIFY_ROWS: usize = 5;

pub(super) fn draw_notifications(
    frame: &mut Frame<'_>,
    backends: &Backends,
    selected: usize,
    status: &str,
    palette: Palette,
) {
    let rect = popup(frame.area(), 78, 17);
    frame.render_widget(Clear, rect);
    let block = palette.modal("Notifications", &[]);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let check = |on: bool| if on { "[x]" } else { "[ ]" };
    let env_url = std::env::var(notify::URL_ENV)
        .ok()
        .filter(|u| !u.trim().is_empty());
    let url = match (&env_url, backends.webhook_url.trim()) {
        (Some(url), _) => format!("{url} (from {})", notify::URL_ENV),
        (None, "") => "not set".into(),
        (None, url) => url.to_owned(),
    };
    let rows = [
        format!(
            "{} Terminal: bell, OSC 9 and OSC 777",
            check(backends.terminal)
        ),
        format!(
            "{} Desktop: {}",
            check(backends.desktop),
            if cfg!(target_os = "macos") {
                "osascript"
            } else {
                "notify-send"
            }
        ),
        format!(
            "{} Webhook: ntfy-style POST{}",
            check(backends.webhook || env_url.is_some()),
            if env_url.is_some() {
                " (on: URL set in the environment)"
            } else {
                ""
            }
        ),
        format!("    Webhook URL… {}", storage::truncate(&url, 52)),
        "    Send a test notification".to_owned(),
    ];
    let mut lines = vec![
        Line::styled(
            "Sent when a run needs input, stops blocked or failed, or completes.",
            Style::default().fg(palette.muted),
        ),
        Line::raw(""),
    ];
    for (index, row) in rows.iter().enumerate() {
        let style = if index == selected {
            palette.selected_style()
        } else {
            Style::default().fg(palette.fg)
        };
        lines.push(Line::from(vec![
            Span::styled(if index == selected { "▌ " } else { "  " }, style),
            Span::styled(row.clone(), style),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        storage::clean_terminal(status),
        Style::default().fg(palette.accent),
    ));
    lines.push(Line::raw(""));
    lines.push(palette.hints(&[
        ("↑↓", "select"),
        ("Space / Enter", "toggle or choose"),
        ("Esc", "close"),
    ]));
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        inner.inner(ratatui::layout::Margin::new(1, 0)),
    );
}

pub(super) fn draw_session_runs(
    frame: &mut Frame<'_>,
    app: &App,
    selected: usize,
    palette: Palette,
) {
    let rect = popup(frame.area(), 100, 20);
    frame.render_widget(Clear, rect);
    let waiting = app.session.needing_input();
    let title = if waiting > 0 {
        format!("Session runs · {waiting} need input")
    } else {
        "Session runs".to_owned()
    };
    let block = palette.modal(&title, &[]);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).split(inner);
    let now = chrono::Utc::now();
    let rows: Vec<Row> = app
        .session
        .slots()
        .enumerate()
        .map(|(index, (slot, focused))| {
            let run = &slot.run;
            let end = if slot.in_progress() {
                now
            } else {
                run.updated_at
            };
            let note = if slot.needs_input() {
                Span::styled(
                    "◆ needs input",
                    Style::default()
                        .fg(palette.warn)
                        .add_modifier(Modifier::BOLD),
                )
            } else if slot.terminal().is_some() {
                Span::styled("live terminal", Style::default().fg(palette.accent))
            } else if slot.worker.is_some() {
                Span::styled("running", Style::default().fg(palette.muted))
            } else {
                Span::raw("")
            };
            Row::new([
                Cell::from(format!("{}{}", index + 1, if focused { "*" } else { "" })),
                Cell::from(storage::truncate(&run.workflow.name, 24)),
                Cell::from(run.id.clone()).style(Style::default().fg(palette.muted)),
                Cell::from(palette.status_span(run.status)),
                Cell::from(storage::truncate(run.current_name(), 24)),
                Cell::from(elapsed((end - run.created_at).num_seconds().max(0)))
                    .style(Style::default().fg(palette.muted)),
                Cell::from(note),
            ])
        })
        .collect();
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new("No runs in this session. Start one from Workflow [r].")
                .style(Style::default().fg(palette.muted)),
            chunks[0],
        );
    } else {
        frame.render_stateful_widget(
            Table::new(
                rows,
                [
                    Constraint::Length(3),
                    Constraint::Length(18),
                    Constraint::Length(16),
                    Constraint::Length(14),
                    Constraint::Length(16),
                    Constraint::Length(8),
                    Constraint::Fill(1),
                ],
            )
            .column_spacing(1)
            .header(
                Row::new(["#", "WORKFLOW", "ID", "STATUS", "STAGE", "ELAPSED", ""])
                    .style(palette.header_style()),
            )
            .highlight_symbol("▌")
            .highlight_spacing(HighlightSpacing::Always)
            .row_highlight_style(palette.selected_style()),
            chunks[0],
            &mut TableState::default().with_selected(Some(selected)),
        );
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(""),
            palette.hints(&[
                ("↑↓", "select"),
                ("Enter", "focus"),
                ("n", "next needing input"),
                ("C", "cancel stage"),
                ("Esc", "close"),
            ]),
        ]),
        chunks[1],
    );
}
