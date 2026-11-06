//! The session's runs as a whole: the notice queue behind the status line,
//! the session runs dialog (`L`), jumping to a run that needs input (`N`),
//! and the notifications dialog (`F9`).
use super::{
    app::{App, Page, RunSlot},
    modal::{Ask, InputTarget, Modal},
    theme::{self, Palette},
    widgets::{elapsed, menu_key, popup},
};
use crate::{
    engine::{Action, Run, Status},
    notify::{self, Backends, NotifyEvent},
    storage,
};
use anyhow::Result;
use crossterm::event::{KeyCode as Key, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Clear, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap},
};

/// Notices kept for the history dialog.
pub(super) const NOTICES: usize = 50;

/// Something a run of this session reported, for the status line and `!`.
pub(super) struct Notice {
    pub(super) at: chrono::DateTime<chrono::Local>,
    pub(super) text: String,
    pub(super) alert: bool,
}

impl RunSlot {
    /// Waiting on the user: an interactive stage ready for its terminal, or
    /// a handoff or waiver to accept.
    pub(super) fn needs_input(&self) -> bool {
        self.worker.is_none()
            && self.run.as_ref().is_some_and(|run| {
                matches!(
                    run.status,
                    Status::Interactive | Status::AwaitingConfirmation
                )
            })
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
        if self.notices.len() == NOTICES {
            self.notices.pop_front();
        }
        self.notices.push_back(Notice {
            at: chrono::Local::now(),
            text: text.clone(),
            alert,
        });
        self.unseen_notices += 1;
        if alert {
            self.alert = text.clone();
        }
        self.message = text;
        self.redraw = true;
    }
    /// How many notices arrived beside the one on the status line.
    pub(super) fn more_notices(&self) -> usize {
        let newest = self.notices.back().is_some_and(|n| n.text == self.message);
        if newest {
            self.unseen_notices.saturating_sub(1)
        } else {
            0
        }
    }
    pub(super) fn show_notices(&mut self) {
        self.unseen_notices = 0;
        let body = if self.notices.is_empty() {
            "No notices yet. Runs of this session add one when they stop, need input, or finish."
                .to_owned()
        } else {
            self.notices
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
    /// Session runs waiting on the user.
    pub(super) fn needing_input(&self) -> usize {
        self.slots().filter(|(slot, _)| slot.needs_input()).count()
    }
    /// Focuses the next run (after the focused one, wrapping) that needs
    /// input and shows it in the Monitor.
    pub(super) fn jump_to_input(&mut self) {
        let total = self.slots().count();
        let waiting: Vec<usize> = self
            .slots()
            .enumerate()
            .filter(|(_, (slot, _))| slot.needs_input())
            .map(|(index, _)| index)
            .collect();
        let start = if self.focused.run.is_some() {
            self.focus_index + 1
        } else {
            0
        };
        let Some(index) = (0..total)
            .map(|step| (start + step) % total.max(1))
            .find(|index| waiting.contains(index))
        else {
            self.message = "No run of this session needs input.".into();
            return;
        };
        self.focus(index);
        self.page = Page::Monitor;
        self.message = self.focus_message();
    }
    /// The status line after focusing a run from the list.
    fn focus_message(&self) -> String {
        let Some(run) = &self.focused.run else {
            return String::new();
        };
        let name = format!("{} ({})", run.workflow.name, run.id);
        match run.status {
            _ if self.focused.terminal().is_some() => {
                format!("{name}: its interactive session is live. [t] attaches.")
            }
            Status::Interactive if self.focused.worker.is_none() => {
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
        self.modal = Some(Modal::SessionRuns {
            selected: self.focused.run.as_ref().map_or(0, |_| self.focus_index),
        });
    }
    /// Keys for the session runs dialog; false closes it.
    pub(super) fn session_runs_key(&mut self, selected: &mut usize, key: KeyEvent) -> bool {
        let total = self.slots().count();
        match menu_key(key) {
            Key::Up | Key::Char('k') => *selected = selected.saturating_sub(1),
            Key::Down | Key::Char('j') => *selected = (*selected + 1).min(total.saturating_sub(1)),
            Key::Enter if *selected < total => {
                self.focus(*selected);
                self.page = Page::Monitor;
                self.message = self.focus_message();
                return false;
            }
            Key::Char('n') => {
                self.jump_to_input();
                return false;
            }
            Key::Char('C') => {
                let Some((slot, _)) = self.slots().nth(*selected) else {
                    return true;
                };
                let Some(run) = &slot.run else { return true };
                if slot.worker.is_some() {
                    self.modal = Some(Modal::Ask(Box::new(Ask::CancelRun {
                        run: run.id.clone(),
                    })));
                    return false;
                }
                self.message = format!("{} is not running a stage; nothing to cancel.", run.id);
            }
            _ => {}
        }
        true
    }
    /// Stops the stage of session run `id`; false when it is not running.
    pub(super) fn cancel_run(&mut self, id: &str) -> bool {
        let slot = std::iter::once(&self.focused)
            .chain(&self.parked)
            .find(|slot| slot.run.as_ref().is_some_and(|run| run.id == id));
        match slot {
            Some(slot) if slot.worker.is_some() => {
                slot.cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }
    pub(super) fn notifications(&mut self) {
        self.modal = Some(Modal::Notifications {
            backends: self.notifications.clone(),
            selected: 0,
            status: String::new(),
        });
    }
    /// Saves `backends` with the other preferences and uses them from now on.
    pub(super) fn set_notifications(&mut self, backends: Backends) -> Result<()> {
        let preferences = theme::Preferences {
            notifications: backends.clone(),
            ..self.preferences()
        };
        theme::save(&self.theme_path, &preferences)?;
        notify::global().set_backends(backends.clone());
        self.notifications = backends;
        Ok(())
    }
    /// Keys for the notifications dialog; false closes it.
    pub(super) fn notifications_key(
        &mut self,
        backends: &mut Backends,
        selected: &mut usize,
        status: &mut String,
        key: KeyEvent,
    ) -> Result<bool> {
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
                3 => {
                    let url = backends.webhook_url.clone();
                    self.input(
                        "Webhook URL (ntfy-style: the body is the message, Title header the title)",
                        InputTarget::NotifyUrl,
                        &url,
                    );
                    return Ok(false);
                }
                _ => *status = notify::global().deliver(&NotifyEvent::test()).summary(),
            },
            _ => {}
        }
        Ok(true)
    }
    pub(super) fn preferences(&self) -> theme::Preferences {
        theme::Preferences {
            theme: self.theme,
            vim_motions: self.vim_motions,
            notifications: self.notifications.clone(),
        }
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
    let waiting = app.needing_input();
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
        .slots()
        .enumerate()
        .filter_map(|(index, (slot, focused))| {
            let run = slot.run.as_ref()?;
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
            Some(Row::new([
                Cell::from(format!("{}{}", index + 1, if focused { "*" } else { "" })),
                Cell::from(storage::truncate(&run.workflow.name, 24)),
                Cell::from(run.id.clone()).style(Style::default().fg(palette.muted)),
                Cell::from(palette.status_span(run.status)),
                Cell::from(storage::truncate(run.current_name(), 24)),
                Cell::from(elapsed((end - run.created_at).num_seconds().max(0)))
                    .style(Style::default().fg(palette.muted)),
                Cell::from(note),
            ]))
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
