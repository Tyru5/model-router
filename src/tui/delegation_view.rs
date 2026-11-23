//! Modal inspector for delegated tasks of a run (`g` / `Space v g`).
use super::{
    theme::Palette,
    widgets::{fit, kv, menu_key},
};
use crate::{
    delegation::records::{SealedPatch, TaskState, TaskStatus},
    engine::{Run, Status},
    handoff::Handoff,
    snapshot, storage,
};
use anyhow::{Context, Result, ensure};
use crossterm::event::{KeyCode as Key, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

pub(super) struct TaskItem {
    pub(super) stage_name: String,
    pub(super) task: TaskState,
}

pub(super) enum DelegatedAction {
    Stay,
    Close,
    /// `dir` is the checkout the handoff's and patch's paths name.
    Handoff {
        stage: String,
        handoff: Box<Handoff>,
        dir: PathBuf,
    },
    Patch {
        patch: String,
        dir: PathBuf,
    },
    CancelTask {
        run: String,
        task: String,
    },
}

pub(super) struct DelegatedTasksView {
    pub(super) run_id: String,
    run_dir: PathBuf,
    /// The run's checkout, where an applied task's changes are.
    workspace: PathBuf,
    pub(super) live: bool,
    pub(super) tasks: Vec<TaskItem>,
    pub(super) selected: usize,
    list_state: ListState,
    scroll: u16,
    pub(super) error: String,
}

impl DelegatedTasksView {
    pub(super) fn from_run(run: &Run, live: bool) -> Self {
        let tasks = collect_tasks(run);
        let mut list_state = ListState::default();
        if !tasks.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            run_id: run.id.clone(),
            run_dir: run.directory.clone(),
            workspace: super::completion::run_dir(run),
            live,
            tasks,
            selected: 0,
            list_state,
            scroll: 0,
            error: String::new(),
        }
    }

    /// Refreshes tasks from the live run while preserving the selected task ID.
    pub(super) fn sync(&mut self, run: &Run, live: bool) {
        if run.id != self.run_id {
            return;
        }
        self.live = live;
        let selected_id = self.current().map(|item| item.task.id.clone());
        self.tasks = collect_tasks(run);
        if let Some(id) = selected_id {
            if let Some(pos) = self.tasks.iter().position(|item| item.task.id == id) {
                self.selected = pos;
            }
        }
        self.selected = self.selected.min(self.tasks.len().saturating_sub(1));
        self.list_state.select(if self.tasks.is_empty() {
            None
        } else {
            Some(self.selected)
        });
    }

    pub(super) fn current(&self) -> Option<&TaskItem> {
        self.tasks.get(self.selected)
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> DelegatedAction {
        self.error.clear();
        match menu_key(key) {
            Key::Up | Key::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.scroll = 0;
                DelegatedAction::Stay
            }
            Key::Down | Key::Char('j') => {
                if !self.tasks.is_empty() {
                    self.selected = (self.selected + 1).min(self.tasks.len() - 1);
                    self.scroll = 0;
                }
                DelegatedAction::Stay
            }
            Key::PageUp | Key::Char('K') => {
                self.scroll = self.scroll.saturating_sub(6);
                DelegatedAction::Stay
            }
            Key::PageDown | Key::Char('J') => {
                self.scroll = self.scroll.saturating_add(6);
                DelegatedAction::Stay
            }
            Key::Home => {
                self.scroll = 0;
                DelegatedAction::Stay
            }
            Key::Char('q') => DelegatedAction::Close,
            Key::Enter | Key::Char('h') => {
                let Some(item) = self.current() else {
                    return DelegatedAction::Stay;
                };
                let handoff = item
                    .task
                    .latest_handoff
                    .as_ref()
                    .or(item.task.handoff.as_ref());
                match handoff {
                    Some(h) => DelegatedAction::Handoff {
                        stage: format!("{} · {}", item.stage_name, item.task.id),
                        handoff: Box::new(h.clone()),
                        dir: files_dir(&item.task, &self.workspace),
                    },
                    None => {
                        self.error = format!("Task {} has no handoff yet.", item.task.id);
                        DelegatedAction::Stay
                    }
                }
            }
            Key::Char('d') => {
                let Some(item) = self.current() else {
                    return DelegatedAction::Stay;
                };
                let Some(patch) = item.task.patches.last() else {
                    self.error = format!("Task {} has no sealed patch.", item.task.id);
                    return DelegatedAction::Stay;
                };
                match read_sealed_patch(&self.run_dir, patch) {
                    Ok(text) if text.trim().is_empty() => {
                        self.error = format!("Task {}'s sealed patch is empty.", item.task.id);
                        DelegatedAction::Stay
                    }
                    Ok(text) => DelegatedAction::Patch {
                        patch: text,
                        dir: files_dir(&item.task, &self.workspace),
                    },
                    Err(err) => {
                        self.error = format!("Could not read sealed patch: {err:#}");
                        DelegatedAction::Stay
                    }
                }
            }
            Key::Char('C') => {
                let Some(item) = self.current() else {
                    return DelegatedAction::Stay;
                };
                if item.task.status.is_terminal() {
                    self.error = format!(
                        "Task {} is already {}.",
                        item.task.id,
                        item.task.status.label()
                    );
                    return DelegatedAction::Stay;
                }
                if !self.live {
                    self.error = format!(
                        "Run {} is not actively driving a stage; nothing to cancel.",
                        self.run_id
                    );
                    return DelegatedAction::Stay;
                }
                DelegatedAction::CancelTask {
                    run: self.run_id.clone(),
                    task: item.task.id.clone(),
                }
            }
            _ => DelegatedAction::Stay,
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame<'_>, rect: Rect, palette: Palette) {
        let title = format!("Delegated tasks · {}", self.run_id);
        let block = palette.modal(
            &title,
            &[
                ("↑↓", "select"),
                ("h", "handoff"),
                ("d", "patch"),
                ("C", "cancel task"),
                ("Esc", "close"),
            ],
        );
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if self.tasks.is_empty() {
            frame.render_widget(
                Paragraph::new("No delegated tasks recorded for this run.")
                    .style(Style::default().fg(palette.muted)),
                inner.inner(Margin::new(1, 1)),
            );
            return;
        }
        let has_error = !self.error.is_empty();
        let chunks = Layout::vertical([
            Constraint::Min(6),
            Constraint::Length(if has_error { 2 } else { 0 }),
        ])
        .split(inner);
        let halves = Layout::horizontal([Constraint::Percentage(36), Constraint::Percentage(64)])
            .split(chunks[0]);
        let list_width = usize::from(halves[0].width.saturating_sub(5));
        let rows = self.tasks.iter().map(|item| {
            let status = task_status_to_run(item.task.status);
            let target = item
                .task
                .target
                .delegate
                .as_deref()
                .unwrap_or(&item.task.target.harness);
            let subtitle = if item.task.title.is_empty() {
                item.task.task.lines().next().unwrap_or("")
            } else {
                &item.task.title
            };
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        Palette::status_glyph(status),
                        Style::default().fg(palette.status_color(status)),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        fit(&format!("{} · {target}", item.task.id), list_width),
                        Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::styled(
                    format!(
                        "  {} · {}",
                        item.task.status.label(),
                        fit(
                            &storage::clean_terminal(subtitle),
                            list_width.saturating_sub(item.task.status.label().len() + 5)
                        )
                    ),
                    Style::default().fg(palette.muted),
                ),
            ])
        });
        self.list_state
            .select(Some(self.selected.min(self.tasks.len() - 1)));
        frame.render_stateful_widget(
            List::new(rows)
                .highlight_style(palette.highlight_style())
                .highlight_symbol("▌ ")
                .highlight_spacing(HighlightSpacing::Always)
                .block(
                    Block::default()
                        .borders(Borders::RIGHT)
                        .border_style(Style::default().fg(palette.line)),
                ),
            halves[0],
            &mut self.list_state,
        );
        if let Some(item) = self.current() {
            let detail = task_detail_lines(item, palette);
            let max_scroll = u16::try_from(detail.len().saturating_sub(1)).unwrap_or(u16::MAX);
            self.scroll = self.scroll.min(max_scroll);
            frame.render_widget(
                Paragraph::new(detail)
                    .wrap(Wrap { trim: false })
                    .scroll((self.scroll, 0)),
                halves[1].inner(Margin::new(1, 0)),
            );
        }
        if has_error {
            frame.render_widget(
                Paragraph::new(Line::styled(
                    storage::clean_terminal(&self.error),
                    Style::default().fg(palette.red),
                ))
                .wrap(Wrap { trim: false }),
                chunks[1].inner(Margin::new(1, 0)),
            );
        }
    }
}

/// Where a task's changed files are: its worktree while it exists, else the
/// run's checkout, which a shared task edits and an applied patch reaches.
fn files_dir(task: &TaskState, workspace: &Path) -> PathBuf {
    task.worktree
        .clone()
        .filter(|w| w.is_dir())
        .unwrap_or_else(|| workspace.to_path_buf())
}

/// Tasks from the current stage's latest attempt, or across all stages if the
/// current stage has no delegated tasks (e.g. completed runs).
pub(super) fn collect_tasks(run: &Run) -> Vec<TaskItem> {
    if let Some(state) = run.stages.get(run.cursor) {
        if let Some(attempt) = state.attempts.last() {
            if !attempt.tasks.is_empty() {
                let stage_name = run
                    .workflow
                    .stages
                    .get(run.cursor)
                    .map_or_else(String::new, |s| s.name.clone());
                return attempt
                    .tasks
                    .iter()
                    .map(|task| TaskItem {
                        stage_name: stage_name.clone(),
                        task: task.clone(),
                    })
                    .collect();
            }
        }
    }
    let mut items = vec![];
    for (state, stage) in run.stages.iter().zip(&run.workflow.stages) {
        if let Some(attempt) = state.attempts.last() {
            for task in &attempt.tasks {
                items.push(TaskItem {
                    stage_name: stage.name.clone(),
                    task: task.clone(),
                });
            }
        }
    }
    items
}

fn task_status_to_run(status: TaskStatus) -> Status {
    match status {
        TaskStatus::Queued | TaskStatus::Starting => Status::Pending,
        TaskStatus::Running => Status::Running,
        TaskStatus::Waiting => Status::AwaitingConfirmation,
        TaskStatus::Completed => Status::Complete,
        TaskStatus::Blocked => Status::Blocked,
        TaskStatus::Cancelled => Status::Cancelled,
        TaskStatus::Failed | TaskStatus::Interrupted => Status::Failed,
    }
}

fn task_detail_lines(item: &TaskItem, palette: Palette) -> Vec<Line<'static>> {
    let task = &item.task;
    let muted = Style::default().fg(palette.muted);
    let fg = Style::default().fg(palette.fg);
    let target = match &task.target.delegate {
        Some(delegate) => format!(
            "{delegate} ({}{})",
            task.target.harness,
            if task.target.model.is_empty() {
                String::new()
            } else {
                format!(" / {}", task.target.model)
            }
        ),
        None => format!(
            "{}{}",
            task.target.harness,
            if task.target.model.is_empty() {
                String::new()
            } else {
                format!(" / {}", task.target.model)
            }
        ),
    };
    let mut lines = vec![
        kv(
            palette,
            "Task",
            &format!("{} ({})", task.id, item.stage_name),
            true,
        ),
        kv(palette, "Status", task.status.label(), true),
        kv(palette, "Role", task.role.label(), true),
        kv(palette, "Target", &target, true),
        kv(palette, "Isolation", task.isolation.label(), true),
        kv(palette, "Turns", &task.turns.len().to_string(), true),
    ];
    if let Some(error) = &task.error {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("Error: {}", storage::clean_terminal(error)),
            Style::default().fg(palette.red),
        ));
    }
    if !task.questions.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Questions (waiting for answer)",
            Style::default()
                .fg(palette.warn)
                .add_modifier(Modifier::BOLD),
        ));
        for question in &task.questions {
            lines.push(Line::styled(
                format!("  ? {}", storage::clean_terminal(question)),
                Style::default().fg(palette.warn),
            ));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Brief", palette.header_style()));
    for line in storage::clean_terminal(&task.task).lines() {
        lines.push(Line::styled(line.to_owned(), fg));
    }
    let handoff = task.latest_handoff.as_ref().or(task.handoff.as_ref());
    if let Some(handoff) = handoff {
        let verdict = match handoff.verdict {
            crate::handoff::Verdict::Complete => "complete",
            crate::handoff::Verdict::Pass => "pass",
            crate::handoff::Verdict::NeedsChanges => "needs changes",
            crate::handoff::Verdict::Blocked => "blocked",
        };
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("Handoff · {verdict} ([h] opens full view)"),
            palette.header_style(),
        ));
        for line in storage::clean_terminal(&handoff.summary).lines() {
            lines.push(Line::styled(line.to_owned(), fg));
        }
        if !handoff.findings.is_empty() || !handoff.tests.is_empty() {
            lines.push(Line::styled(
                format!(
                    "{} finding(s) · {} test(s) · {} changed file(s)",
                    handoff.findings.len(),
                    handoff.tests.len(),
                    handoff.files_changed.len()
                ),
                muted,
            ));
        }
    }
    if !task.patches.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Sealed patches ([d] opens latest diff)",
            palette.header_style(),
        ));
        for patch in &task.patches {
            let applied = match &patch.apply {
                Some(journal) if journal.applied_at.is_some() => " · applied",
                Some(journal) if journal.unknown => " · apply unknown",
                Some(_) => " · applying",
                None => "",
            };
            lines.push(Line::styled(
                format!(
                    "  rev {} (turn {}) · {} file(s) +{}/-{}{}",
                    patch.revision,
                    patch.turn,
                    patch.files.len(),
                    patch.added,
                    patch.removed,
                    applied
                ),
                fg,
            ));
            for file in patch.files.iter().take(8) {
                lines.push(Line::styled(
                    format!("    {}", storage::clean_terminal(file)),
                    muted,
                ));
            }
            if patch.files.len() > 8 {
                lines.push(Line::styled(
                    format!("    +{} more", patch.files.len() - 8),
                    muted,
                ));
            }
        }
    }
    lines
}

/// Reads a task's sealed patch safely from beneath `run_dir`, bounded to
/// `snapshot::PATCH_LIMIT` so symlinks or oversized files cannot escape or OOM.
pub(super) fn read_sealed_patch(run_dir: &Path, patch: &SealedPatch) -> Result<String> {
    let relative = if patch.path.is_absolute() {
        let base = std::fs::canonicalize(run_dir).unwrap_or_else(|_| run_dir.to_path_buf());
        let stripped = patch
            .path
            .strip_prefix(run_dir)
            .or_else(|_| patch.path.strip_prefix(&base))
            .with_context(|| {
                format!(
                    "Patch path {} is outside run directory {}",
                    patch.path.display(),
                    run_dir.display()
                )
            })?;
        stripped.to_path_buf()
    } else {
        patch.path.clone()
    };
    let file = storage::open_beneath(run_dir, &relative)?;
    let mut bytes = Vec::new();
    file.take((snapshot::PATCH_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= snapshot::PATCH_LIMIT,
        "Sealed patch exceeds {} bytes",
        snapshot::PATCH_LIMIT
    );
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
