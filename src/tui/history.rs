use super::input::InputField;
use super::{
    actions::ActionId,
    app::App,
    modal::{Choice, Modal, PickValue},
    theme::Palette,
    widgets::{content, empty_state, kv, menu_key, textarea},
};
use crate::{
    adapters::OutputKind,
    engine::{self, Run, Status},
    history::{self, Criteria, Entry, Plan, StatusFilter},
    storage,
};
use anyhow::{Context, Result, bail};
use crossterm::event::{KeyCode as Key, KeyEvent, KeyModifiers as Mods};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    thread,
};

/// The status filters `f` cycles through.
const STATUSES: [(&str, &[StatusFilter]); 6] = [
    ("all", &[]),
    ("active", &[StatusFilter::Active]),
    ("complete", &[StatusFilter::Is(Status::Complete)]),
    (
        "failed/blocked",
        &[
            StatusFilter::Is(Status::Failed),
            StatusFilter::Is(Status::Blocked),
        ],
    ),
    ("cancelled", &[StatusFilter::Is(Status::Cancelled)]),
    ("damaged", &[StatusFilter::Damaged]),
];

/// Runs page state besides the list and its selection.
#[derive(Default)]
pub(super) struct RunsView {
    filter: InputField<'static>,
    /// Keys go to the filter field.
    filtering: bool,
    /// Index into `STATUSES`.
    status: usize,
    /// Artifact sizes by run ID, filled in by a background thread.
    sizes: Arc<Mutex<HashMap<String, u64>>>,
}

impl RunsView {
    pub(super) fn set_vim(&mut self, enabled: bool) {
        self.filter.set_vim(enabled);
    }
}

/// A Runs page confirmation dialog.
pub(super) struct Confirm {
    kind: Kind,
    error: String,
}
enum Kind {
    Rerun {
        source: Box<Run>,
        worktree: bool,
    },
    Cleanup {
        id: String,
        plan: Plan,
        worktrees: bool,
    },
}

/// A comparison chosen from the compare picker.
#[derive(Clone)]
pub(super) struct Compare {
    run: String,
    stage: usize,
    /// Attempt numbers; None compares the last attempts of the rerun's source and the run.
    attempts: Option<(usize, usize)>,
}

impl App {
    /// Entries the filters show, newest first.
    pub(super) fn visible_runs(&self) -> Vec<&Entry> {
        history::filter(
            &self.history,
            STATUSES[self.runs.status].1,
            &content(&self.runs.filter),
        )
    }
    fn selected_run(&self) -> Option<Entry> {
        self.visible_runs()
            .get(self.history_selected)
            .map(|entry| (*entry).clone())
    }
    /// Keeps `id` selected when it is still shown; otherwise keeps the row in range.
    fn reselect(&mut self, id: Option<String>) {
        let visible = self.visible_runs();
        let index = id
            .and_then(|id| visible.iter().position(|entry| entry.id() == id))
            .unwrap_or(self.history_selected);
        self.history_selected = index.min(visible.len().saturating_sub(1));
    }
    /// Shows freshly read summaries, keeping the selection, and starts
    /// sizing them in the background.
    pub(super) fn show_runs(&mut self, entries: Vec<Entry>) {
        let selected = self.selected_run().map(|entry| entry.id().to_owned());
        self.history = entries;
        self.runs_loaded = Some(std::time::Instant::now());
        self.reselect(selected);
        let sizes = Arc::new(Mutex::new(HashMap::new()));
        self.runs.sizes = sizes.clone();
        let (project, entries) = (self.project.clone(), self.history.clone());
        thread::spawn(move || {
            for entry in entries {
                // A refresh or exit dropped the map.
                if Arc::strong_count(&sizes) == 1 {
                    return;
                }
                let bytes = history::run_size(&project, &entry);
                if let Ok(mut sizes) = sizes.lock() {
                    sizes.insert(entry.id().to_owned(), bytes);
                }
            }
        });
    }
    pub(super) fn perform_runs(&mut self, id: ActionId) -> Result<()> {
        match id {
            ActionId::NextRun => {
                self.history_selected =
                    (self.history_selected + 1).min(self.visible_runs().len().saturating_sub(1))
            }
            ActionId::PreviousRun => {
                self.history_selected = self.history_selected.saturating_sub(1)
            }
            ActionId::RefreshRuns => self.load_runs(),
            ActionId::FilterRuns => {
                self.runs.filtering = true;
                self.runs.filter.set_vim(self.vim_motions);
                super::widgets::focus_textarea(&mut self.runs.filter);
            }
            ActionId::CycleStatus => {
                let selected = self.selected_run().map(|entry| entry.id().to_owned());
                self.runs.status = (self.runs.status + 1) % STATUSES.len();
                self.reselect(selected);
            }
            ActionId::OpenRun => {
                // A run of this session is already live; never load a stale copy.
                let open = self
                    .selected_run()
                    .is_some_and(|entry| self.focus_run(entry.id()));
                if open {
                    self.page = 1;
                    self.message = "Run already open in this session.".into();
                } else if let Some(run) =
                    self.selected_run().and_then(|entry| self.load_run(&entry))
                {
                    let tail = saved_output(&run);
                    self.adopt(run);
                    for line in tail {
                        self.focused.push_log(line);
                    }
                    self.page = 1;
                    self.message =
                        "Saved run loaded. c continues; y retries; t reopens its session; i retries interactively.".into();
                }
            }
            ActionId::Rerun => {
                if let Some(source) = self.selected_run().and_then(|entry| self.load_run(&entry)) {
                    let worktree = source.worktree;
                    self.modal = Some(Modal::Confirm(Box::new(Confirm {
                        kind: Kind::Rerun {
                            source: Box::new(source),
                            worktree,
                        },
                        error: String::new(),
                    })));
                }
            }
            ActionId::CompareAttempts => {
                if let Some(run) = self.selected_run().and_then(|entry| self.load_run(&entry)) {
                    self.compare_picker(&run);
                }
            }
            ActionId::CleanUp => {
                if let Some(entry) = self.selected_run() {
                    let id = entry.id().to_owned();
                    let plan = self.cleanup_plan(&id, false)?;
                    self.modal = Some(Modal::Confirm(Box::new(Confirm {
                        kind: Kind::Cleanup {
                            id,
                            plan,
                            worktrees: false,
                        },
                        error: String::new(),
                    })));
                }
            }
            other => bail!("{other:?} is not a Runs action"),
        }
        Ok(())
    }
    /// An entry's full record, loaded on demand. Shows why when it cannot be read.
    fn load_run(&mut self, entry: &Entry) -> Option<Run> {
        let error = match entry {
            Entry::Summary(summary) => match engine::load(&self.project, &summary.id) {
                Ok(run) => return Some(run),
                Err(error) => format!("{error:#}"),
            },
            Entry::Damaged { error, .. } => error.clone(),
        };
        self.text(
            "Damaged run record",
            format!(
                "{error}\n\nRun: {}\nPath: {}\n\nIt cannot be opened, rerun, or compared. [D] previews removing it.",
                entry.id(),
                entry.dir(&self.project).display()
            ),
        );
        None
    }
    /// Keys while the filter field has focus. True means exit.
    pub(super) fn runs_filter_key(&mut self, key: KeyEvent) -> Result<bool> {
        self.runs.filter.set_vim(self.vim_motions);
        if self.runs.filter.vim_key(key) {
            return Ok(false);
        }
        let selected = self.selected_run().map(|entry| entry.id().to_owned());
        match menu_key(key) {
            Key::Esc => {
                self.runs.filter = InputField::default();
                self.runs.filtering = false;
            }
            Key::Enter => self.runs.filtering = false,
            Key::Up => return self.perform(ActionId::PreviousRun).map(|_| false),
            Key::Down => return self.perform(ActionId::NextRun).map(|_| false),
            Key::Char('c') if key.modifiers.contains(Mods::CONTROL) => {
                return self.perform(ActionId::Interrupt);
            }
            Key::Tab | Key::BackTab => {}
            _ => {
                self.runs.filter.input(key);
            }
        }
        self.reselect(selected);
        Ok(false)
    }
    pub(super) fn paste_runs_filter(&mut self, text: &str) {
        if self.page == 2 && self.runs.filtering {
            let selected = self.selected_run().map(|entry| entry.id().to_owned());
            self.runs.filter.insert_str(text.replace(['\r', '\n'], " "));
            self.reselect(selected);
        }
    }
    pub(super) fn runs_filtering(&self) -> bool {
        self.page == 2 && self.runs.filtering
    }
    fn compare_picker(&mut self, run: &Run) {
        let mut items = vec![];
        for (index, state) in run.stages.iter().enumerate() {
            let name = &run.workflow.stages[index].name;
            let numbers: Vec<_> = state.attempts.iter().map(|a| a.number).collect();
            // The last two first, then every other pair, newest first.
            for (end, b) in numbers.iter().enumerate().rev() {
                for a in numbers[..end].iter().rev() {
                    items.push(Choice {
                        label: format!("{name} · attempt {a} vs {b}"),
                        value: PickValue::Compare(Compare {
                            run: run.id.clone(),
                            stage: index,
                            attempts: Some((*a, *b)),
                        }),
                    });
                }
            }
        }
        if let Some(source) = &run.rerun_of {
            for (index, state) in run.stages.iter().enumerate() {
                if !state.attempts.is_empty() {
                    items.push(Choice {
                        label: format!(
                            "{} · source {source} vs this rerun",
                            run.workflow.stages[index].name
                        ),
                        value: PickValue::Compare(Compare {
                            run: run.id.clone(),
                            stage: index,
                            attempts: None,
                        }),
                    });
                }
            }
        }
        if items.is_empty() {
            self.message = "No stage in this run has two attempts to compare.".into();
            return;
        }
        self.modal = Some(Modal::Picker {
            title: format!("Compare attempts · {}", run.id),
            items,
            selected: 0,
            filter: textarea(""),
            note: "Compares status, checks, and handoffs. The first pair per stage is its last two attempts."
                .into(),
        });
    }
    pub(super) fn show_comparison(&mut self, choice: &Compare) -> Result<()> {
        let run = engine::load(&self.project, &choice.run)?;
        let comparison = match choice.attempts {
            Some((a, b)) => history::compare(&run, choice.stage, a, b)?,
            None => {
                let source = run.rerun_of.as_deref().context("This run is not a rerun")?;
                let source = engine::load(&self.project, source)?;
                history::compare_rerun(&source, &run, &run.workflow.stages[choice.stage].id)?
            }
        };
        self.text(
            &format!("Compare · {}", comparison.stage),
            comparison.text(),
        );
        Ok(())
    }
    fn cleanup_plan(&self, id: &str, worktrees: bool) -> Result<Plan> {
        history::cleanup_plan(
            &self.project,
            &Criteria {
                worktrees,
                ids: vec![id.to_owned()],
                protect: self
                    .slots()
                    .filter_map(|(slot, _)| slot.run.as_ref())
                    .map(|run| run.id.clone())
                    .collect(),
                ..Criteria::default()
            },
        )
    }
    /// Keys for a confirmation dialog; true closes it. Esc never reaches here.
    pub(super) fn confirm_key(&mut self, dialog: &mut Confirm, key: KeyEvent) -> bool {
        let result = match (&mut dialog.kind, key.code) {
            (Kind::Rerun { worktree, .. }, Key::Char('g')) => {
                *worktree = !*worktree;
                Ok(false)
            }
            (Kind::Rerun { source, worktree }, Key::Enter) => {
                let (id, worktree) = (source.id.clone(), *worktree);
                self.start_rerun(&id, worktree).map(|()| true)
            }
            (
                Kind::Cleanup {
                    id,
                    plan,
                    worktrees,
                },
                Key::Char('g'),
            ) => self.cleanup_plan(id, !*worktrees).map(|next| {
                *plan = next;
                *worktrees = !*worktrees;
                false
            }),
            (Kind::Cleanup { plan, .. }, Key::Enter | Key::Char('y')) => {
                self.clean_up(plan).map(|()| true)
            }
            _ => Ok(false),
        };
        result.unwrap_or_else(|error| {
            dialog.error = format!("{error:#}");
            false
        })
    }
    /// Starts a rerun through the same background path as Workflow [r].
    fn start_rerun(&mut self, id: &str, worktree: bool) -> Result<()> {
        let (project, settings, source) =
            (self.project.clone(), self.settings.clone(), id.to_owned());
        self.start_run(
            move |placement| history::rerun_at(&project, &settings, &source, placement),
            worktree,
            None,
            Some(id.to_owned()),
        )
    }
    fn clean_up(&mut self, plan: &Plan) -> Result<()> {
        let done = history::apply(plan);
        if let Some((id, reason)) = done.skipped.first() {
            self.message = format!("Kept {id}: {reason}");
            self.alert = self.message.clone();
        } else if let Some(item) = done.items.first() {
            self.message = format!("Removed {} ({})", item.id, history::human(item.bytes()));
            if let Some(note) = &item.note {
                self.message.push_str(&format!(". {note}"));
            }
        }
        self.load_runs();
        Ok(())
    }
}

/// Lines of output a saved run's latest attempt left in stdout.log, formatted
/// as the Monitor shows them live.
fn saved_output(run: &Run) -> Vec<String> {
    let Some((index, attempt)) = run
        .stages
        .iter()
        .enumerate()
        .filter_map(|(index, state)| Some((index, state.attempts.last()?)))
        .max_by_key(|(_, attempt)| attempt.started_at)
    else {
        return vec![];
    };
    let Ok(raw) = crate::process::read_tail(&attempt.directory.join("stdout.log"), 256 * 1024)
    else {
        return vec![];
    };
    let stage = &run.workflow.stages[index.min(run.workflow.stages.len().saturating_sub(1))];
    let kind = if attempt.mode == crate::config::Mode::Interactive {
        OutputKind::Text
    } else {
        crate::adapters::output_kind(&stage.harness, &run.adapters)
    };
    // A tail can start mid-line; the first, partial line is dropped.
    let raw = match raw.split_once('\n') {
        Some((_, rest)) if raw.len() >= 256 * 1024 => rest.to_owned(),
        _ => raw,
    };
    let mut formatter = crate::events::Formatter::new(kind);
    let mut text = formatter.push(&raw).unwrap_or_default();
    text.extend(formatter.finish());
    let mut lines = vec![format!(
        "── {} · attempt {} · saved output ──",
        stage.name, attempt.number
    )];
    lines.extend(
        storage::clean_terminal(&text)
            .lines()
            .map(|line| storage::truncate(line, 1500)),
    );
    let keep = lines.len().saturating_sub(super::app::LOG_LINES);
    lines.drain(1..=keep.min(lines.len() - 1));
    lines
}

impl Confirm {
    pub(super) fn draw(&self, frame: &mut Frame<'_>, rect: Rect, palette: Palette) {
        let muted = Style::default().fg(palette.muted);
        let (title, keys, mut lines): (_, &[(&str, &str)], _) = match &self.kind {
            Kind::Rerun { source, worktree } => (
                "Rerun as a new run",
                &[("Enter", "start"), ("g", "workspace"), ("Esc", "cancel")],
                rerun_lines(source, *worktree, palette),
            ),
            Kind::Cleanup {
                id,
                plan,
                worktrees,
            } => {
                let mut lines: Vec<_> = plan
                    .text(false)
                    .lines()
                    .map(|line| Line::raw(line.to_string()))
                    .collect();
                let tree = plan.project.join(".model-router/worktrees").join(id);
                if !worktrees && tree.exists() {
                    lines.push(Line::raw(""));
                    lines.push(Line::styled(
                        "Its worktree stays. [g] also removes it when clean; its branch stays.",
                        muted,
                    ));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "Removal cannot be undone. Worktree changes and branches are never deleted.",
                    muted,
                ));
                (
                    "Clean up run",
                    &[
                        ("y / Enter", "remove"),
                        ("g", "worktree"),
                        ("Esc", "cancel"),
                    ],
                    lines,
                )
            }
        };
        if !self.error.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                storage::clean_terminal(&self.error),
                Style::default().fg(palette.red),
            ));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(palette.modal(title, keys)),
            rect,
        );
    }
}

fn rerun_lines(source: &Run, worktree: bool, palette: Palette) -> Vec<Line<'static>> {
    let mut lines = vec![
        kv(
            palette,
            "Source",
            &format!("{} · {}", source.id, source.status.label()),
            true,
        ),
        kv(palette, "Workflow", &source.workflow.name, true),
    ];
    let enabled: Vec<_> = source
        .workflow
        .stages
        .iter()
        .filter(|s| s.enabled)
        .collect();
    for (index, stage) in enabled.iter().enumerate() {
        let model = if stage.model.is_empty() {
            "default"
        } else {
            &stage.model
        };
        lines.push(kv(
            palette,
            if index == 0 { "Stages" } else { "" },
            &format!(
                "{}. {} · {} / {model} · {}",
                index + 1,
                stage.name,
                stage.harness,
                stage.mode.label()
            ),
            true,
        ));
    }
    let disabled = source.workflow.stages.len() - enabled.len();
    if disabled > 0 {
        lines.push(kv(
            palette,
            "",
            &format!("{disabled} disabled stage(s) stay disabled"),
            false,
        ));
    }
    lines.push(kv(palette, "Task", &history::excerpt(&source.task), true));
    lines.push(kv(
        palette,
        "Workspace",
        if worktree {
            "isolated Git worktree from committed HEAD"
        } else {
            "current directory"
        },
        true,
    ));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Uses this run's saved workflow and adapter snapshot, not the current template. The saved run is not changed.",
        Style::default().fg(palette.muted),
    ));
    lines
}

pub(super) fn draw_history(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) {
    if app.history.is_empty() {
        empty_state(
            frame,
            palette,
            area,
            palette.panel(" Saved runs "),
            "No saved runs yet",
            "Runs for this project are saved automatically. Start a workflow from Workflow [r].",
        );
        return;
    }
    let visible = app.visible_runs();
    let mut title = String::from("Saved runs");
    if app.runs.status > 0 {
        title.push_str(&format!(" · {}", STATUSES[app.runs.status].0));
    }
    let search = content(&app.runs.filter);
    if !search.trim().is_empty() && !app.runs.filtering {
        title.push_str(&format!(
            " · “{}”",
            storage::truncate(search.trim(), 24)
                .lines()
                .next()
                .unwrap_or("")
        ));
    }
    title.push_str(&format!(
        " · {} of {} runs",
        visible.len(),
        app.history.len()
    ));
    let block = palette.panel_with(&title, &[("Enter", "open")]);
    let outer = area;
    let mut area = block.inner(outer);
    frame.render_widget(block, outer);
    if app.runs.filtering {
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(area);
        let hint = if let Some(mode) = app.runs.filter.mode_hint() {
            Line::styled(mode, Style::default().fg(palette.accent))
        } else {
            palette.hints(&[("Enter", "keep"), ("Esc", "clear")])
        };
        let cols = Layout::horizontal([
            Constraint::Length(2),
            Constraint::Min(8),
            Constraint::Length(hint.width() as u16 + 1),
        ])
        .split(rows[0]);
        frame.render_widget(
            Span::styled(
                "/ ",
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            cols[0],
        );
        let mut field = app.runs.filter.clone();
        palette.style_textarea(&mut field);
        frame.render_widget(&field, cols[1]);
        frame.render_widget(Paragraph::new(hint).right_aligned(), cols[2]);
        area = rows[2];
    }
    if visible.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    "No runs match",
                    Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
                ),
                Line::styled(
                    "[/] then [Esc] clears the filter; [f] changes the status filter.",
                    Style::default().fg(palette.muted),
                ),
            ])
            .centered()
            .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let muted = Style::default().fg(palette.muted);
    let rows: Vec<_> = {
        let sizes = app.runs.sizes.lock().ok();
        visible
            .iter()
            .map(|entry| {
                let size = sizes
                    .as_ref()
                    .and_then(|sizes| sizes.get(entry.id()))
                    .map_or_else(|| "…".into(), |bytes| history::human(*bytes));
                let started = entry.created_at().map_or_else(
                    || "–".into(),
                    |time| time.format("%b %d  %H:%M").to_string(),
                );
                let (status, workflow, task) = match entry {
                    Entry::Summary(run) => (
                        Cell::from(palette.status_span(run.status)),
                        Cell::from(run.workflow.clone()),
                        Cell::from(run.task.clone()).style(muted),
                    ),
                    Entry::Damaged { id, error, .. } => (
                        Cell::from(Span::styled("✗ damaged", Style::default().fg(palette.red))),
                        Cell::from(id.clone()),
                        Cell::from(storage::clean_terminal(error))
                            .style(Style::default().fg(palette.red)),
                    ),
                };
                Row::new([
                    Cell::from(started).style(muted),
                    status,
                    Cell::from(size).style(muted),
                    workflow,
                    task,
                ])
            })
            .collect()
    };
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(14),
                Constraint::Length(18),
                Constraint::Length(9),
                Constraint::Fill(1),
                Constraint::Fill(2),
            ],
        )
        .header(
            Row::new(["STARTED", "STATUS", "SIZE", "WORKFLOW", "TASK"])
                .style(palette.header_style())
                .bottom_margin(1),
        )
        .highlight_symbol("▌")
        .highlight_spacing(HighlightSpacing::Always)
        .row_highlight_style(palette.selected_style()),
        area,
        &mut TableState::default().with_selected(Some(app.history_selected.min(visible.len() - 1))),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{CustomAdapter, Role, Settings, Stage, Workflow},
        engine::Action,
    };
    use ratatui::{Terminal, backend::TestBackend};
    use std::{
        collections::BTreeMap,
        fs,
        path::Path,
        sync::atomic::AtomicBool,
        time::{Duration, Instant},
    };

    fn key(code: Key) -> KeyEvent {
        KeyEvent::new(code, Mods::NONE)
    }
    /// Presses `keys`, letting background work (saved runs, starting runs)
    /// land after each, as frames would.
    fn press(app: &mut App, keys: &str) {
        for c in keys.chars() {
            app.key(key(Key::Char(c))).unwrap();
            settle(app);
        }
    }
    fn settle(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            app.poll().unwrap();
            let busy = app.runs_job.is_some()
                || app.starting.is_some()
                || (app.page == 2 && app.runs_stale);
            if !busy || Instant::now() > deadline {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn settings(model: &str) -> Settings {
        let adapter = CustomAdapter {
            command: "python3".into(),
            args: [
                concat!(env!("CARGO_MANIFEST_DIR"), "/examples/mock_harness.py"),
                "--role",
                "{provider}",
                "--model",
                "{model}",
                "--prompt-file",
                "{prompt_file}",
                "--handoff-file",
                "{handoff_file}",
            ]
            .map(String::from)
            .to_vec(),
            ..CustomAdapter::default()
        };
        Settings {
            workflows: vec![Workflow {
                max_repair_passes: 0,
                ..Workflow::new(
                    "Test",
                    vec![Stage {
                        id: "research".into(),
                        name: "research".into(),
                        role: Role::Research,
                        harness: "mock".into(),
                        provider: "research".into(),
                        model: model.into(),
                        ..Stage::default()
                    }],
                )
            }],
            adapters: BTreeMap::from([("mock".into(), adapter)]),
            ..Settings::default()
        }
    }
    fn create(project: &Path, settings: &Settings, task: &str) -> Run {
        Run::create(
            project,
            settings,
            &settings.workflows[0],
            task.into(),
            false,
        )
        .unwrap()
    }
    fn drive(run: &mut Run) -> Action {
        engine::drive(run, &AtomicBool::new(false), |_| {}).unwrap()
    }
    fn app(project: &Path, settings: Settings) -> App {
        let mut app = App::new(
            project.into(),
            settings,
            project.join("config.toml"),
            Arc::new(AtomicBool::new(false)),
        );
        app.theme_path = project.join("appearance.json");
        app
    }
    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| super::super::app::draw(frame, app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }
    fn shows(app: &mut App, text: &str) -> bool {
        screen(app, 120, 36).iter().any(|line| line.contains(text))
    }
    fn run_dirs(project: &Path) -> usize {
        fs::read_dir(project.join(".model-router/runs"))
            .unwrap()
            .count()
    }
    #[test]
    fn filter_and_status_cycle_keep_selection_valid() {
        let project = tempfile::tempdir().unwrap();
        let settings = settings("fail");
        let mut failed = create(project.path(), &settings, "Fix the parser");
        assert_eq!(drive(&mut failed), Action::Stopped);
        create(project.path(), &settings, "Write the docs");
        fs::create_dir_all(project.path().join(".model-router/runs/broken")).unwrap();
        let mut app = app(project.path(), settings);
        press(&mut app, "3");
        assert_eq!(app.history.len(), 3);
        press(&mut app, "jjj");
        assert_eq!(app.history_selected, 2);
        assert!(shows(&mut app, "Saved runs · 3 of 3 runs"));

        press(&mut app, "/docs");
        assert!(app.runs_filtering());
        assert_eq!(app.visible_runs().len(), 1);
        assert_eq!(app.history_selected, 0);
        // Action keys type into the field while it has focus.
        assert_eq!(app.page, 2);
        assert!(shows(&mut app, "1 of 3 runs"));
        app.key(key(Key::Esc)).unwrap();
        assert!(!app.runs_filtering());
        assert_eq!(app.visible_runs().len(), 3);

        press(&mut app, "/parser");
        app.key(key(Key::Enter)).unwrap();
        assert!(!app.runs_filtering());
        assert_eq!(app.visible_runs()[0].id(), failed.id);
        assert!(shows(&mut app, "“parser” · 1 of 3 runs"));
        press(&mut app, "/");
        app.key(key(Key::Esc)).unwrap();

        let mut seen = vec![];
        for _ in 0..STATUSES.len() {
            press(&mut app, "f");
            let visible = app.visible_runs();
            assert!(app.history_selected <= visible.len().saturating_sub(1));
            seen.push((STATUSES[app.runs.status].0, visible.len()));
        }
        assert_eq!(
            seen,
            [
                ("active", 1),
                ("complete", 0),
                ("failed/blocked", 1),
                ("cancelled", 0),
                ("damaged", 1),
                ("all", 3)
            ]
        );
        press(&mut app, "ff");
        assert!(shows(&mut app, "No runs match"));
        assert!(shows(&mut app, "Saved runs · complete · 0 of 3 runs"));
    }

    #[test]
    fn damaged_row_shows_its_error_instead_of_opening() {
        let project = tempfile::tempdir().unwrap();
        let broken = project
            .path()
            .join(".model-router/runs/20200101-000000-broken00");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join("run.json"), "{oops").unwrap();
        let mut app = app(project.path(), settings("test-model"));
        press(&mut app, "3");
        assert!(shows(&mut app, "✗ damaged"));
        app.key(key(Key::Enter)).unwrap();
        settle(&mut app);
        assert!(app.focused.run.is_none());
        assert_eq!(app.page, 2);
        match &app.modal {
            Some(Modal::Text { title, body, .. }) => {
                assert_eq!(title, "Damaged run record");
                assert!(body.contains(&broken.display().to_string()), "{body}");
            }
            _ => panic!("expected the error"),
        }
        app.key(key(Key::Esc)).unwrap();
        press(&mut app, "n");
        assert!(matches!(&app.modal, Some(Modal::Text { .. })));
    }

    #[test]
    fn rerun_dialog_confirms_before_starting() {
        let project = tempfile::tempdir().unwrap();
        let settings = settings("test-model");
        let mut source = create(project.path(), &settings, "Port the parser\nDetails");
        assert_eq!(drive(&mut source), Action::Complete);
        let mut app = app(project.path(), settings);
        press(&mut app, "3n");
        assert!(matches!(&app.modal, Some(Modal::Confirm(_))));
        assert!(shows(&mut app, "Rerun as a new run"));
        assert!(shows(&mut app, "research · mock / test-model · unattended"));
        assert!(shows(&mut app, "Port the parser"));
        assert!(shows(&mut app, "not the current template"));
        app.key(key(Key::Esc)).unwrap();
        assert!(app.modal.is_none() && app.focused.worker.is_none() && app.focused.run.is_none());
        assert_eq!(run_dirs(project.path()), 1);

        press(&mut app, "n");
        app.key(key(Key::Enter)).unwrap();
        assert!(app.modal.is_none());
        assert!(
            app.starting.is_some(),
            "The run is created in the background"
        );
        settle(&mut app);
        assert!(app.focused.worker.is_some());
        assert_eq!(app.page, 1);
        let deadline = Instant::now() + Duration::from_secs(20);
        while app.focused.worker.is_some() && Instant::now() < deadline {
            app.poll().unwrap();
            thread::sleep(Duration::from_millis(20));
        }
        let run = app.focused.run.as_ref().unwrap();
        assert_eq!(run.rerun_of.as_deref(), Some(source.id.as_str()));
        assert_eq!(run.status, Status::Complete);
        assert_eq!(run_dirs(project.path()), 2);
    }

    #[test]
    fn compare_picks_a_stage_then_shows_the_difference() {
        let project = tempfile::tempdir().unwrap();
        let settings = settings("fail-once");
        let mut retried = create(project.path(), &settings, "Retried");
        assert_eq!(drive(&mut retried), Action::Stopped);
        retried.retry(false, None).unwrap();
        assert_eq!(drive(&mut retried), Action::Complete);
        // The mock fails only a role's first call.
        let mut once = create(project.path(), &settings, "Single attempt");
        assert_eq!(drive(&mut once), Action::Complete);
        let mut app = app(project.path(), settings);
        press(&mut app, "3c");
        assert!(app.modal.is_none());
        assert!(app.message.contains("two attempts"), "{}", app.message);
        press(&mut app, "j");
        assert_eq!(app.selected_run().unwrap().id(), retried.id);
        press(&mut app, "c");
        match &app.modal {
            Some(Modal::Picker { items, .. }) => {
                assert_eq!(items[0].label, "research · attempt 1 vs 2");
            }
            _ => panic!("expected the stage picker"),
        }
        app.key(key(Key::Enter)).unwrap();
        settle(&mut app);
        match &app.modal {
            Some(Modal::Text { title, body, .. }) => {
                assert_eq!(title, "Compare · research");
                assert!(body.contains("failed → complete"), "{body}");
            }
            _ => panic!("expected the comparison"),
        }
    }

    #[test]
    fn cleanup_previews_then_removes_on_confirmation() {
        let project = tempfile::tempdir().unwrap();
        let settings = settings("test-model");
        let mut run = create(project.path(), &settings, "Old work");
        assert_eq!(drive(&mut run), Action::Complete);
        let kept = create(project.path(), &settings, "Newer work");
        let mut app = app(project.path(), settings);
        press(&mut app, "3j");
        assert_eq!(app.selected_run().unwrap().id(), run.id);
        press(&mut app, "D");
        assert!(matches!(&app.modal, Some(Modal::Confirm(_))));
        assert!(shows(&mut app, "Would remove 1 run"));
        assert!(shows(
            &mut app,
            &format!(".model-router/handoffs/{}", run.id)
        ));
        app.key(key(Key::Esc)).unwrap();
        assert!(app.modal.is_none());
        assert!(run.directory.exists());

        press(&mut app, "Dy");
        assert!(app.modal.is_none());
        assert!(!run.directory.exists() && kept.directory.exists());
        assert!(app.message.starts_with(&format!("Removed {}", run.id)));
        assert_eq!(app.history.len(), 1);
        assert_eq!(app.history_selected, 0);

        // The run open in the monitor stays.
        app.focused.run = Some(engine::load(project.path(), &kept.id).unwrap());
        press(&mut app, "D");
        app.key(key(Key::Enter)).unwrap();
        settle(&mut app);
        assert!(kept.directory.exists());
        assert!(
            app.message.contains("Open in the monitor"),
            "{}",
            app.message
        );
    }

    #[test]
    fn opening_a_saved_run_shows_the_tail_of_its_output() {
        let project = tempfile::tempdir().unwrap();
        let settings = settings("test-model");
        let mut run = create(project.path(), &settings, "Saved output");
        assert_eq!(drive(&mut run), Action::Complete);
        let attempt = &run.stages[0].attempts[0];
        let mut log: String = (0..600).map(|n| format!("old line {n}\n")).collect();
        log.push_str("\x1b[31mfinal line\x1b[0m\n");
        fs::write(attempt.directory.join("stdout.log"), log).unwrap();
        let mut app = app(project.path(), settings);
        press(&mut app, "3");
        app.key(key(Key::Enter)).unwrap();
        let logs: Vec<_> = app.focused.logs.iter().cloned().collect();
        assert_eq!(logs[0], "── research · attempt 1 · saved output ──");
        assert_eq!(logs.last().unwrap(), "final line");
        assert!(logs.len() <= crate::tui::app::LOG_LINES);
        app.focused.show_output = true;
        assert!(shows(&mut app, "final line"));
    }

    #[test]
    fn sizes_arrive_in_the_background_and_footer_fits() {
        let project = tempfile::tempdir().unwrap();
        let settings = settings("test-model");
        let run = create(project.path(), &settings, "Sized");
        let mut app = app(project.path(), settings);
        press(&mut app, "3");
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.runs.sizes.lock().unwrap().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let bytes = app.runs.sizes.lock().unwrap()[&run.id];
        assert_eq!(bytes, history::run_size(project.path(), &app.history[0]));
        assert!(shows(&mut app, &history::human(bytes)));
        let lines = screen(&mut app, 80, 24);
        let footer = lines[22..].join("\n");
        assert!(footer.contains("[↑↓] select"), "{footer}");
        assert!(footer.contains("[:] commands"), "{footer}");
    }
}
