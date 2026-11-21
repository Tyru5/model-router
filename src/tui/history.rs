use super::input::InputField;
use super::{
    actions::ActionId,
    app::{App, Page},
    modal::{Choice, Modal, PickValue},
    monitor::draw_run,
    theme::{Palette, Theme},
    widgets::{content, empty_state, fit, kv, menu_key, textarea},
};
use crate::{
    adapters::OutputKind,
    engine::{self, Run, Status},
    history::{self, Criteria, Entry, Plan, StatusFilter},
    storage,
};
use anyhow::{Context, Result, bail, ensure};
use crossterm::event::{KeyCode as Key, KeyEvent, KeyModifiers as Mods};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum SizeStamp {
    Summary {
        revision: u64,
        updated_millis: i64,
        attempts: usize,
    },
    Damaged,
}

impl SizeStamp {
    fn of(entry: &Entry) -> Self {
        match entry {
            Entry::Summary(summary) => Self::Summary {
                revision: summary.revision,
                updated_millis: summary.updated_at.timestamp_millis(),
                attempts: summary.attempts,
            },
            Entry::Damaged { .. } => Self::Damaged,
        }
    }
}

/// The Runs page: a sidebar of this session's runs, then the project's saved
/// runs, beside the selected one. Session runs are selected by focusing them.
pub(super) struct RunsPage {
    /// Saved runs as last read, newest first.
    pub(super) entries: Vec<Entry>,
    /// Index into the visible entries.
    pub(super) selected: usize,
    /// The sidebar selects a saved run, not the focused run of this session.
    pub(super) on_saved: bool,
    /// IDs of this session's runs, which the saved list leaves out.
    pub(super) open: Vec<String>,
    open_set: HashSet<String>,
    /// Saved runs are being read.
    pub(super) loading: bool,
    /// The list needs reading when the page is next shown.
    pub(super) stale: bool,
    pub(super) loaded_at: Option<std::time::Instant>,
    filter: InputField<'static>,
    /// Keys go to the filter field.
    filtering: bool,
    /// Index into `STATUSES`.
    status: usize,
    /// Artifact sizes by run ID, filled in by a background thread.
    pub(super) sizes: Arc<Mutex<HashMap<String, u64>>>,
    size_stamps: HashMap<String, SizeStamp>,
    size_gen: Arc<AtomicU64>,
    sizes_dirty: Arc<AtomicBool>,
    sizing: Arc<AtomicBool>,
}

impl Default for RunsPage {
    fn default() -> Self {
        Self {
            entries: vec![],
            selected: 0,
            on_saved: false,
            open: vec![],
            open_set: HashSet::new(),
            loading: false,
            stale: true,
            loaded_at: None,
            filter: InputField::default(),
            filtering: false,
            status: 0,
            sizes: Arc::default(),
            size_stamps: HashMap::new(),
            size_gen: Arc::default(),
            sizes_dirty: Arc::default(),
            sizing: Arc::default(),
        }
    }
}

impl RunsPage {
    pub(super) fn set_vim(&mut self, enabled: bool) {
        self.filter.set_vim(enabled);
    }
    pub(super) fn reset(&mut self) {
        self.entries.clear();
        self.selected = 0;
        self.loading = false;
        self.stale = true;
        self.loaded_at = None;
        self.size_gen.fetch_add(1, Ordering::Relaxed);
        self.sizes = Arc::default();
        self.size_stamps.clear();
        self.sizes_dirty.store(false, Ordering::Relaxed);
        self.sizing.store(false, Ordering::Relaxed);
    }
    pub(super) fn is_sizing(&self) -> bool {
        self.sizing.load(Ordering::Relaxed)
    }
    pub(super) fn take_sizes_dirty(&self) -> bool {
        self.sizes_dirty.swap(false, Ordering::Relaxed)
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
        has_worktree: bool,
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

impl RunsPage {
    /// Saved entries the filters show, newest first, without this session's runs.
    pub(super) fn visible(&self) -> Vec<&Entry> {
        history::filter(
            &self.entries,
            STATUSES[self.status].1,
            &content(&self.filter),
        )
        .into_iter()
        .filter(|entry| !self.open_set.contains(entry.id()))
        .collect()
    }
    /// Whether a status or text filter hides any saved runs.
    fn filtered(&self) -> bool {
        self.status > 0 || !content(&self.filter).trim().is_empty()
    }
    pub(super) fn selected_entry(&self) -> Option<Entry> {
        self.visible()
            .get(self.selected)
            .map(|entry| (*entry).clone())
    }
    fn selected_id(&self) -> Option<String> {
        self.selected_entry().map(|entry| entry.id().to_owned())
    }
    /// Keeps `id` selected when it is still shown; otherwise keeps the row in range.
    fn reselect(&mut self, id: Option<String>) {
        let visible = self.visible();
        let index = id
            .and_then(|id| visible.iter().position(|entry| entry.id() == id))
            .unwrap_or(self.selected);
        self.selected = index.min(visible.len().saturating_sub(1));
    }
    /// Shows freshly read summaries, keeping the selection, and starts
    /// sizing new or modified runs in the background.
    pub(super) fn show(&mut self, entries: Vec<Entry>, project: &std::path::Path) {
        let selected = self.selected_id();
        self.entries = entries;
        self.loaded_at = Some(std::time::Instant::now());
        self.reselect(selected);
        let my_gen = self.size_gen.fetch_add(1, Ordering::Relaxed) + 1;
        let mut next_stamps = HashMap::with_capacity(self.entries.len());
        let mut missing = vec![];
        {
            let mut sizes = self.sizes.lock().unwrap_or_else(|e| e.into_inner());
            for entry in &self.entries {
                let id = entry.id();
                let stamp = SizeStamp::of(entry);
                next_stamps.insert(id.to_owned(), stamp);
                let unchanged = self.size_stamps.get(id) == Some(&stamp) && sizes.contains_key(id);
                if !unchanged {
                    missing.push(entry.clone());
                }
            }
            sizes.retain(|id, _| next_stamps.contains_key(id));
        }
        self.size_stamps = next_stamps;
        if missing.is_empty() {
            self.sizing.store(false, Ordering::Relaxed);
            return;
        }
        self.sizing.store(true, Ordering::Relaxed);
        let (project, sizes, size_gen, sizes_dirty, sizing) = (
            project.to_path_buf(),
            self.sizes.clone(),
            self.size_gen.clone(),
            self.sizes_dirty.clone(),
            self.sizing.clone(),
        );
        thread::spawn(move || {
            for entry in missing {
                if size_gen.load(Ordering::Relaxed) != my_gen {
                    return;
                }
                let bytes = history::run_size(&project, &entry);
                if size_gen.load(Ordering::Relaxed) != my_gen {
                    return;
                }
                if let Ok(mut sizes) = sizes.lock() {
                    sizes.insert(entry.id().to_owned(), bytes);
                }
                sizes_dirty.store(true, Ordering::Relaxed);
            }
            if size_gen.load(Ordering::Relaxed) == my_gen {
                sizing.store(false, Ordering::Relaxed);
            }
        });
    }
}

impl App {
    /// Whether the sidebar selects a saved run; always when this session has none.
    pub(super) fn on_saved(&self) -> bool {
        self.runs.on_saved || self.session.runs.is_empty()
    }
    /// Whether the focused run of this session is on screen.
    pub(super) fn monitoring(&self) -> bool {
        self.ui.page == Page::Runs && !self.on_saved()
    }
    /// Shows the focused run of this session on the Runs page.
    pub(super) fn show_run(&mut self) {
        self.ui.page = Page::Runs;
        self.runs.on_saved = false;
    }
    /// Leaves this session's runs out of the saved list, keeping its selection.
    pub(super) fn sync_runs(&mut self) {
        let same = self.session.runs.len() == self.runs.open.len()
            && self
                .session
                .runs
                .iter()
                .zip(&self.runs.open)
                .all(|(slot, id)| slot.run.id == *id);
        if !same {
            let selected = self.runs.selected_id();
            self.runs.open = self.session.runs.iter().map(|s| s.run.id.clone()).collect();
            self.runs.open_set = self.runs.open.iter().cloned().collect();
            self.runs.reselect(selected);
        }
    }
    /// The selected run's record: this session's copy, or the saved one.
    /// The selected run, to rerun or compare. Both read it from the open
    /// project, so a run of this session started in another one is refused.
    fn selected_run(&mut self) -> Result<Option<Run>> {
        if self.on_saved() {
            let Some(entry) = self.runs.selected_entry() else {
                return Ok(None);
            };
            return Ok(self.load_run(&entry));
        }
        let run = self.session.run().cloned();
        if let Some(run) = &run {
            ensure!(
                run.project == self.project,
                "This run belongs to {}; open that project with [p] to rerun or compare it",
                run.project.display()
            );
        }
        Ok(run)
    }
    pub(super) fn perform_runs(&mut self, id: ActionId) -> Result<()> {
        let saved = self.runs.visible().len();
        match id {
            // One list: this session's runs, then saved ones.
            ActionId::NextRun if self.on_saved() => {
                self.runs.selected = (self.runs.selected + 1).min(saved.saturating_sub(1))
            }
            ActionId::NextRun => {
                if self.session.focus + 1 < self.session.runs.len() {
                    self.session.focus += 1;
                } else if saved > 0 {
                    self.runs.on_saved = true;
                    self.runs.selected = 0;
                }
            }
            ActionId::PreviousRun if self.on_saved() => {
                if self.runs.selected > 0 {
                    self.runs.selected -= 1;
                } else if !self.session.runs.is_empty() {
                    self.runs.on_saved = false;
                    self.session.focus = self.session.runs.len() - 1;
                }
            }
            ActionId::PreviousRun => self.session.focus = self.session.focus.saturating_sub(1),
            ActionId::RefreshRuns => self.load_runs(),
            ActionId::FilterRuns => {
                self.runs.filtering = true;
                self.runs.filter.set_vim(self.ui.vim_motions);
                super::widgets::focus_textarea(&mut self.runs.filter);
            }
            ActionId::CycleStatus => {
                let selected = self.runs.selected_id();
                self.runs.status = (self.runs.status + 1) % STATUSES.len();
                self.runs.reselect(selected);
            }
            ActionId::OpenRun => {
                // A run of this session is already live; never load a stale copy.
                if !self.on_saved() {
                    self.ui.message = "Run already open in this session.".into();
                } else if let Some(run) = self
                    .runs
                    .selected_entry()
                    .and_then(|entry| self.load_run(&entry))
                {
                    let tail = saved_output(&run);
                    // Only the keys that apply to a run in this state.
                    let message = match run.status {
                        Status::Complete => "Saved run loaded. d shows its changes; n reruns it.",
                        // Continuing would fail it; see `engine::drive`.
                        Status::Running => {
                            "Saved run loaded. It was interrupted: y retries; i retries interactively."
                        }
                        Status::AwaitingConfirmation => {
                            "Saved run loaded. h inspects the handoff; a accepts it; y retries."
                        }
                        Status::Failed | Status::Blocked | Status::Cancelled => {
                            "Saved run loaded. y retries; t reopens its session; i retries interactively."
                        }
                        Status::Interactive => "Saved run loaded. i opens its terminal.",
                        _ => "Saved run loaded. c continues; y retries; i retries interactively.",
                    };
                    self.session.adopt(run);
                    if let Some(slot) = self.session.focused_mut() {
                        for line in tail {
                            slot.push_log(line);
                        }
                    }
                    self.sync_runs();
                    self.show_run();
                    self.ui.message = message.into();
                    self.reattach_detached();
                }
            }
            ActionId::Rerun => {
                if let Some(source) = self.selected_run()? {
                    let worktree = source.worktree;
                    self.ui.open(Modal::Confirm(Box::new(Confirm {
                        kind: Kind::Rerun {
                            source: Box::new(source),
                            worktree,
                        },
                        error: String::new(),
                    })));
                }
            }
            ActionId::CompareAttempts => {
                if let Some(run) = self.selected_run()? {
                    self.compare_picker(&run);
                }
            }
            ActionId::CleanUp => {
                ensure!(
                    self.on_saved(),
                    "Close this run first ([x]); it then moves to Saved, where [D] removes it"
                );
                if let Some(entry) = self.runs.selected_entry() {
                    let id = entry.id().to_owned();
                    let plan = self.cleanup_plan(&id, false)?;
                    let has_worktree = plan
                        .project
                        .join(".model-router/worktrees")
                        .join(&id)
                        .exists();
                    self.ui.open(Modal::Confirm(Box::new(Confirm {
                        kind: Kind::Cleanup {
                            id,
                            plan,
                            worktrees: false,
                            has_worktree,
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
        self.runs.filter.set_vim(self.ui.vim_motions);
        if self.runs.filter.vim_key(key) {
            return Ok(false);
        }
        let selected = self.runs.selected_id();
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
        self.runs.reselect(selected);
        Ok(false)
    }
    pub(super) fn paste_runs_filter(&mut self, text: &str) {
        if self.ui.page == Page::Runs && self.runs.filtering {
            let selected = self.runs.selected_id();
            self.runs.filter.insert_str(text.replace(['\r', '\n'], " "));
            self.runs.reselect(selected);
        }
    }
    pub(super) fn runs_filtering(&self) -> bool {
        self.ui.page == Page::Runs && self.runs.filtering
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
            self.ui.message = "No stage in this run has two attempts to compare.".into();
            return;
        }
        self.ui.open(Modal::Picker {
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
                    .session
                    .runs
                    .iter()
                    .map(|slot| slot.run.id.clone())
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
                    has_worktree,
                },
                Key::Char('g'),
            ) => self.cleanup_plan(id, !*worktrees).map(|next| {
                *has_worktree = next
                    .project
                    .join(".model-router/worktrees")
                    .join(id)
                    .exists();
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
        let (project, settings, source) = (
            self.project.clone(),
            self.editor.settings.clone(),
            id.to_owned(),
        );
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
            self.ui.error(format!("Kept {id}: {reason}"));
        } else if let Some(item) = done.items.first() {
            self.ui.message = format!("Removed {} ({})", item.id, history::human(item.bytes()));
            if let Some(note) = &item.note {
                self.ui.message.push_str(&format!(". {note}"));
            }
        }
        self.route_stats.invalidate();
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
    let keep = lines.len().saturating_sub(super::run_slot::LOG_LINES);
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
                plan,
                worktrees,
                has_worktree,
                ..
            } => {
                let mut lines: Vec<_> = plan
                    .text(false)
                    .lines()
                    .map(|line| Line::raw(line.to_string()))
                    .collect();
                if !worktrees && *has_worktree {
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

/// The Runs page: the sidebar, and the selected run beside it. Under 100
/// columns a run of this session takes the whole width; the sidebar returns
/// once a saved run is selected.
pub(super) fn draw_runs(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    theme: Theme,
    palette: Palette,
) {
    // Narrow panes give a session's run the width, except while the filter,
    // which the sidebar holds, is being typed.
    if area.width < 98 && !app.on_saved() && !app.runs.filtering {
        return draw_run(frame, app, area, theme, palette);
    }
    let side = (area.width / 3).clamp(22, 34);
    let [sidebar, main] =
        Layout::horizontal([Constraint::Length(side), Constraint::Min(1)]).areas(area);
    let visible = app.runs.visible();
    draw_sidebar(frame, app, sidebar, palette, &visible);
    let main = Rect {
        x: main.x + 1,
        width: main.width.saturating_sub(1),
        ..main
    };
    if app.on_saved() {
        draw_saved(
            frame,
            app,
            main,
            palette,
            visible.get(app.runs.selected).copied(),
        );
    } else {
        draw_run(frame, app, main, theme, palette);
    }
}

/// A run title: the task's first line, else the workflow's name.
pub(super) fn title(task: &str, workflow: &str) -> String {
    let line = history::excerpt(task);
    if line.is_empty() {
        workflow.to_owned()
    } else {
        line
    }
}

/// This session's runs, then the saved ones, two lines each.
fn draw_sidebar(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    palette: Palette,
    visible: &[&Entry],
) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(palette.line));
    let mut inner = block.inner(area);
    frame.render_widget(block, area);
    if app.runs.filtering {
        let [field, rest] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(inner);
        draw_filter(frame, app, field, palette);
        inner = rest;
    }
    let width = usize::from(inner.width.saturating_sub(4));
    let muted = Style::default().fg(palette.muted);
    let heading = |text: String| ListItem::new(Line::styled(text, palette.header_style()));
    let item = |glyph: Span<'static>, title: &str, sub: Span<'static>| {
        ListItem::new(vec![
            Line::from(vec![
                glyph,
                Span::raw(" "),
                Span::styled(fit(title, width), Style::default().fg(palette.fg)),
            ]),
            Line::from(vec![Span::raw("  "), sub]),
        ])
    };
    let status_glyph = |status| {
        Span::styled(
            Palette::status_glyph(status),
            Style::default().fg(palette.status_color(status)),
        )
    };
    let mut items = vec![];
    let mut selected = None;
    if !app.session.runs.is_empty() {
        items.push(heading("This session".into()));
        for (slot, focused) in app.session.slots() {
            let run = &slot.run;
            if focused && !app.on_saved() {
                selected = Some(items.len());
            }
            let warn = Style::default().fg(palette.warn);
            let (glyph, sub) = if slot.needs_input() {
                (
                    Span::styled("◆", warn),
                    Span::styled(format!("needs you · {}", run.current_name()), warn),
                )
            } else if slot.in_progress() {
                (
                    status_glyph(run.status),
                    Span::styled(
                        format!("{} · {}", run.status.label(), run.current_name()),
                        muted,
                    ),
                )
            } else {
                (
                    status_glyph(run.status),
                    Span::styled(
                        format!("{} · {}", run.status.label(), run.workflow.name),
                        muted,
                    ),
                )
            };
            let sub = Span::styled(fit(&sub.content, width), sub.style);
            items.push(item(glyph, &title(&run.task, &run.workflow.name), sub));
        }
        items.push(ListItem::new(""));
    }
    let mut head = if app.runs.loading && app.runs.entries.is_empty() {
        "Saved · loading…".to_owned()
    } else {
        format!("Saved · {}", visible.len())
    };
    if app.runs.status > 0 {
        head.push_str(&format!(" {}", STATUSES[app.runs.status].0));
    }
    let search = content(&app.runs.filter);
    if !search.trim().is_empty() && !app.runs.filtering {
        head.push_str(&format!(" “{}”", fit(search.trim(), 12)));
    }
    items.push(heading(head));
    // Only rows that can be on screen: the list scrolls just far enough to
    // show the selection, so rows a screen or more from it never are.
    let at = app.runs.selected.min(visible.len().saturating_sub(1));
    let height = usize::from(inner.height);
    let shown = if app.on_saved() {
        at.saturating_sub(height)..at + height
    } else {
        0..height
    };
    for (index, entry) in visible
        .iter()
        .enumerate()
        .skip(shown.start)
        .take(shown.len())
    {
        if app.on_saved() && index == at {
            selected = Some(items.len());
        }
        let started = entry
            .created_at()
            .map_or_else(|| "–".into(), |time| time.format("%b %d %H:%M").to_string());
        items.push(match entry {
            Entry::Summary(run) => item(
                status_glyph(run.status),
                // `history::list` already reduced the task to an excerpt.
                if run.task.is_empty() {
                    &run.workflow
                } else {
                    &run.task
                },
                Span::styled(
                    // The status first, so a narrow sidebar cuts the date.
                    fit(&format!("{} · {started}", run.status.label()), width),
                    muted,
                ),
            ),
            Entry::Damaged { id, .. } => {
                let red = Style::default().fg(palette.red);
                item(
                    Span::styled("✗", red),
                    id,
                    Span::styled(fit(&format!("damaged · {started}"), width), red),
                )
            }
        });
    }
    if visible.is_empty() && !app.runs.loading {
        // Runs open in this session are listed above, not filtered out.
        let none = if !app.runs.filtered() {
            "  none yet"
        } else {
            "  none match"
        };
        items.push(ListItem::new(Line::styled(none, muted)));
    }
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("▌")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(palette.selected_style()),
        inner,
        &mut ListState::default().with_selected(selected),
    );
}

/// The filter field above the sidebar while it has focus.
fn draw_filter(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) {
    let [field, hint] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    let [label, input] =
        Layout::horizontal([Constraint::Length(2), Constraint::Min(4)]).areas(field);
    let accent = Style::default().fg(palette.accent);
    frame.render_widget(
        Span::styled("F ", accent.add_modifier(Modifier::BOLD)),
        label,
    );
    let mut text = app.runs.filter.clone();
    palette.style_textarea(&mut text);
    frame.render_widget(&text, input);
    let hint_line = match app.runs.filter.mode_hint() {
        Some(mode) => Line::styled(mode, accent),
        None => palette.hints(&[("Enter", "keep"), ("Esc", "clear")]),
    };
    frame.render_widget(Paragraph::new(hint_line), hint);
}

/// What a saved run was, and what opening it here offers.
fn draw_saved(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    palette: Palette,
    entry: Option<&Entry>,
) {
    let Some(entry) = entry else {
        let (head, body) = if !app.runs.filtered() {
            (
                "No runs yet",
                "Runs for this project are saved automatically. Write a task on Workflow [1], then [r] runs it.",
            )
        } else {
            (
                "No saved runs match",
                "[F] then [Esc] clears the filter; [s] changes which statuses show.",
            )
        };
        empty_state(frame, palette, area, Block::default(), head, body);
        return;
    };
    let size = app
        .runs
        .sizes
        .lock()
        .ok()
        .and_then(|sizes| sizes.get(entry.id()).copied())
        .map_or_else(|| "…".into(), history::human);
    let started = entry.created_at().map_or_else(
        || "–".into(),
        |time| time.format("%b %d %Y  %H:%M").to_string(),
    );
    let fg = Style::default().fg(palette.fg);
    let red = Style::default().fg(palette.red);
    let mut lines = vec![];
    match &entry {
        Entry::Summary(run) => {
            lines.push(Line::from(vec![
                Span::styled(run.workflow.clone(), fg.add_modifier(Modifier::BOLD)),
                Span::raw("   "),
                palette.status_span(run.status),
            ]));
            lines.push(Line::raw(""));
            lines.push(kv(palette, "Started", &started, true));
            lines.push(kv(
                palette,
                "Stages",
                &format!(
                    "{} of {} complete · {} attempts",
                    run.complete_stages, run.enabled_stages, run.attempts
                ),
                true,
            ));
            lines.push(kv(palette, "Size", &size, true));
            lines.push(kv(
                palette,
                "Worktree",
                if run.worktree { "yes" } else { "no" },
                run.worktree,
            ));
            if let Some(source) = &run.rerun_of {
                lines.push(kv(palette, "Rerun of", source, true));
            }
            lines.push(kv(palette, "ID", &run.id, true));
            if let Some(error) = &run.error {
                lines.push(Line::raw(""));
                lines.push(Line::styled(storage::clean_terminal(error), red));
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled("Task", palette.header_style()));
            lines.push(Line::styled(run.task.clone(), fg));
            lines.push(Line::raw(""));
            lines.push(palette.hints(&[
                ("Enter", "open here"),
                ("n", "rerun"),
                ("=", "compare attempts"),
                ("D", "remove"),
            ]));
        }
        Entry::Damaged { id, error, .. } => {
            lines.push(Line::styled(
                "Damaged run record",
                red.add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::raw(""));
            lines.push(Line::styled(storage::clean_terminal(error), red));
            lines.push(Line::raw(""));
            lines.push(kv(palette, "Started", &started, true));
            lines.push(kv(palette, "ID", id, true));
            lines.push(kv(
                palette,
                "Path",
                &entry.dir(&app.project).display().to_string(),
                true,
            ));
            lines.push(Line::raw(""));
            lines.push(palette.hints(&[("D", "preview removing it")]));
        }
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
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
            let busy =
                app.runs.loading || app.starting || (app.ui.page == Page::Runs && app.runs.stale);
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
        app.ui.theme_path = project.join("appearance.json");
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
        press(&mut app, "2");
        assert_eq!(app.runs.entries.len(), 3);
        press(&mut app, "jjj");
        assert_eq!(app.runs.selected, 2);
        assert!(shows(&mut app, "Saved · 3"));

        press(&mut app, "Fdocs");
        assert!(app.runs_filtering());
        assert_eq!(app.runs.visible().len(), 1);
        assert_eq!(app.runs.selected, 0);
        // Action keys type into the field while it has focus.
        assert_eq!(app.ui.page, Page::Runs);
        assert!(shows(&mut app, "Saved · 1"));
        app.key(key(Key::Esc)).unwrap();
        assert!(!app.runs_filtering());
        assert_eq!(app.runs.visible().len(), 3);

        press(&mut app, "Fparser");
        app.key(key(Key::Enter)).unwrap();
        assert!(!app.runs_filtering());
        assert_eq!(app.runs.visible()[0].id(), failed.id);
        assert!(shows(&mut app, "Saved · 1 “parser”"));
        // The status leads, so a narrow sidebar cuts the date instead.
        assert!(shows(&mut app, "failed · "));
        press(&mut app, "F");
        app.key(key(Key::Esc)).unwrap();

        let mut seen = vec![];
        for _ in 0..STATUSES.len() {
            press(&mut app, "s");
            let visible = app.runs.visible();
            assert!(app.runs.selected <= visible.len().saturating_sub(1));
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
        press(&mut app, "ss");
        assert!(shows(&mut app, "No saved runs match"));
        assert!(shows(&mut app, "Saved · 0 complete"));
    }

    #[test]
    fn sidebar_moves_from_session_runs_into_saved_ones() {
        let project = tempfile::tempdir().unwrap();
        let settings = settings("test-model");
        let older = create(project.path(), &settings, "Older work");
        let open = create(project.path(), &settings, "Open work");
        let mut app = app(project.path(), settings);
        app.session
            .adopt(engine::load(project.path(), &open.id).unwrap());
        press(&mut app, "2");
        // The open run is selected, and Saved leaves it out.
        assert!(!app.on_saved());
        assert_eq!(app.runs.visible().len(), 1);
        let text = screen(&mut app, 120, 30).join("\n");
        assert!(
            text.contains("This session") && text.contains("Saved · 1"),
            "{text}"
        );
        press(&mut app, "j");
        assert!(app.on_saved());
        assert_eq!(app.runs.selected_entry().unwrap().id(), older.id);
        assert!(shows(&mut app, "Older work"));
        // At 80 columns the sidebar shows while a saved run is selected...
        assert!(screen(&mut app, 80, 24).join("\n").contains("Saved · 1"));
        press(&mut app, "k");
        assert!(!app.on_saved());
        assert_eq!(app.session.run().unwrap().id, open.id);
        // ...and a run of this session gets the whole width.
        assert!(!screen(&mut app, 80, 24).join("\n").contains("Saved · 1"));
        // Closing it here moves it back to Saved.
        press(&mut app, "x");
        assert!(app.on_saved() && app.session.run().is_none());
        assert_eq!(app.runs.visible().len(), 2);
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
        press(&mut app, "2");
        assert!(shows(&mut app, "Damaged run record"));
        app.key(key(Key::Enter)).unwrap();
        settle(&mut app);
        assert!(app.session.run().is_none());
        assert_eq!(app.ui.page, Page::Runs);
        match app.ui.dialog() {
            Some(Modal::Text { title, body, .. }) => {
                assert_eq!(title, "Damaged run record");
                assert!(body.contains(&broken.display().to_string()), "{body}");
            }
            _ => panic!("expected the error"),
        }
        app.key(key(Key::Esc)).unwrap();
        press(&mut app, "n");
        assert!(matches!(app.ui.dialog(), Some(Modal::Text { .. })));
    }

    #[test]
    fn rerun_dialog_confirms_before_starting() {
        let project = tempfile::tempdir().unwrap();
        let settings = settings("test-model");
        let mut source = create(project.path(), &settings, "Port the parser\nDetails");
        assert_eq!(drive(&mut source), Action::Complete);
        let mut app = app(project.path(), settings);
        press(&mut app, "2n");
        assert!(matches!(app.ui.dialog(), Some(Modal::Confirm(_))));
        assert!(shows(&mut app, "Rerun as a new run"));
        assert!(shows(&mut app, "research · mock / test-model · unattended"));
        assert!(shows(&mut app, "Port the parser"));
        assert!(shows(&mut app, "not the current template"));
        app.key(key(Key::Esc)).unwrap();
        assert!(app.ui.dialog().is_none() && !app.session.running() && app.session.run().is_none());
        assert_eq!(run_dirs(project.path()), 1);

        press(&mut app, "n");
        app.key(key(Key::Enter)).unwrap();
        assert!(app.ui.dialog().is_none());
        assert!(app.starting, "The run is created in the background");
        settle(&mut app);
        assert!(app.session.running());
        assert_eq!(app.ui.page, Page::Runs);
        let deadline = Instant::now() + Duration::from_secs(20);
        while app.session.running() && Instant::now() < deadline {
            app.poll().unwrap();
            thread::sleep(Duration::from_millis(20));
        }
        let run = app.session.run().unwrap();
        assert_eq!(run.rerun_of.as_deref(), Some(source.id.as_str()));
        assert_eq!(run.status, Status::Complete);
        assert_eq!(run_dirs(project.path()), 2);
    }

    #[test]
    fn rerun_in_the_checkout_waits_for_a_group_the_source_left_running() {
        let project = tempfile::tempdir().unwrap();
        let settings = settings("test-model");
        let mut source = create(project.path(), &settings, "Port the parser");
        assert_eq!(drive(&mut source), Action::Complete);
        // Recovery left this group running: only its ID was recorded.
        let mut group = {
            use std::os::unix::process::CommandExt;
            std::process::Command::new("sleep")
                .arg("30")
                .process_group(0)
                .spawn()
                .unwrap()
        };
        source.retained_groups = vec![crate::process::Group {
            pgid: group.id(),
            identity: Default::default(),
        }];
        source.persist().unwrap();
        let mut app = app(project.path(), settings);
        press(&mut app, "2n");
        app.key(key(Key::Enter)).unwrap();
        settle(&mut app);
        let pgid = group.id();
        assert!(
            app.ui.message.contains(&format!("process group {pgid}: "))
                && app.ui.message.contains(&format!(
                    "model-router resume {} --release-groups",
                    source.id
                )),
            "{}",
            app.ui.message
        );
        assert!(!app.session.running());
        assert_eq!(run_dirs(project.path()), 1);
        group.kill().unwrap();
        group.wait().unwrap();
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
        press(&mut app, "2=");
        assert!(app.ui.dialog().is_none());
        assert!(
            app.ui.message.contains("two attempts"),
            "{}",
            app.ui.message
        );
        press(&mut app, "j");
        assert_eq!(app.runs.selected_entry().unwrap().id(), retried.id);
        press(&mut app, "=");
        match app.ui.dialog() {
            Some(Modal::Picker { items, .. }) => {
                assert_eq!(items[0].label, "research · attempt 1 vs 2");
            }
            _ => panic!("expected the stage picker"),
        }
        app.key(key(Key::Enter)).unwrap();
        settle(&mut app);
        match app.ui.dialog() {
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
        press(&mut app, "2j");
        assert_eq!(app.runs.selected_entry().unwrap().id(), run.id);
        press(&mut app, "D");
        assert!(matches!(app.ui.dialog(), Some(Modal::Confirm(_))));
        assert!(shows(&mut app, "Would remove 1 run"));
        assert!(shows(
            &mut app,
            &format!(".model-router/handoffs/{}", run.id)
        ));
        app.key(key(Key::Esc)).unwrap();
        assert!(app.ui.dialog().is_none());
        assert!(run.directory.exists());

        press(&mut app, "Dy");
        assert!(app.ui.dialog().is_none());
        assert!(!run.directory.exists() && kept.directory.exists());
        assert!(app.ui.message.starts_with(&format!("Removed {}", run.id)));
        assert_eq!(app.runs.entries.len(), 1);
        assert_eq!(app.runs.selected, 0);

        // A run open in this session leaves Saved, and D refuses it.
        app.session
            .adopt(engine::load(project.path(), &kept.id).unwrap());
        app.show_run();
        let error = app.key(key(Key::Char('D'))).unwrap_err().to_string();
        assert!(error.contains("Close this run first"), "{error}");
        assert!(app.ui.dialog().is_none() && kept.directory.exists());
        assert!(app.runs.visible().is_empty());
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
        press(&mut app, "2");
        app.key(key(Key::Enter)).unwrap();
        let logs: Vec<_> = app.slot().logs.iter().cloned().collect();
        assert_eq!(logs[0], "── research · attempt 1 · saved output ──");
        assert_eq!(logs.last().unwrap(), "final line");
        assert!(logs.len() <= crate::tui::run_slot::LOG_LINES);
        app.slot_mut().show_output = true;
        assert!(shows(&mut app, "final line"));
    }

    #[test]
    fn sizes_arrive_in_the_background_and_footer_fits() {
        let project = tempfile::tempdir().unwrap();
        let settings = settings("test-model");
        let run = create(project.path(), &settings, "Sized");
        let mut app = app(project.path(), settings);
        press(&mut app, "2");
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.runs.sizes.lock().unwrap().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let bytes = app.runs.sizes.lock().unwrap()[&run.id];
        assert_eq!(
            bytes,
            history::run_size(project.path(), &app.runs.entries[0])
        );
        assert!(shows(&mut app, &history::human(bytes)));
        let lines = screen(&mut app, 80, 24);
        let footer = &lines[23];
        assert!(footer.contains("[↑↓] runs"), "{footer}");
        assert!(footer.contains("[Space] menu"), "{footer}");
    }

    #[test]
    fn runs_sizes_cache_keeps_unchanged_entries_across_refreshes() {
        let project = tempfile::tempdir().unwrap();
        let settings = settings("test-model");
        let run = create(project.path(), &settings, "Cached size");
        let entries = history::list(project.path()).unwrap();
        let mut page = RunsPage::default();
        page.show(entries.clone(), project.path());
        let deadline = Instant::now() + Duration::from_secs(5);
        while page.is_sizing() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let initial = page.sizes.lock().unwrap()[&run.id];
        assert!(page.take_sizes_dirty());
        page.sizes
            .lock()
            .unwrap()
            .insert(run.id.clone(), initial + 999);
        page.show(entries, project.path());
        assert!(!page.is_sizing());
        assert_eq!(page.sizes.lock().unwrap()[&run.id], initial + 999);
    }
}
