//! Background work. Each task runs on its own thread and reports one `Msg`
//! on the app's channel, tagged with the project generation it was started
//! for; `App::update` applies it and drops results for an earlier project.
use super::{
    app::{App, Opened, Started},
    completion::{CompletionKey, Loaded},
    modal::{Ask, Modal},
    path_picker::FolderEntry,
};
use crate::{
    catalog::Catalog, doctor::Report, engine::ApplyPlan, fanout::Applied, history::Entry,
    stats::Stats, storage::RepoInfo,
};
use std::{
    panic::{self, AssertUnwindSafe},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
};

/// A background result. Every payload is a `Result`, so a task that panics
/// still reports, with the error its spawner chose.
pub(super) enum Msg {
    /// A harness catalog: for the model picker waiting with `ticket`, or
    /// (None) for editors only.
    Catalog {
        harness: String,
        ticket: Option<u64>,
        result: Result<Catalog, String>,
    },
    /// Doctor results for the Harnesses page.
    Checks(Result<Vec<Report>, String>),
    /// Saved runs for the Runs page.
    Runs(Result<Vec<Entry>, String>),
    /// Git facts for the header.
    Repo(Result<Option<RepoInfo>, String>),
    /// A created run, ready for its driver. Applies after a project switch:
    /// the run lives in its own project.
    Started(Result<Box<Started>, String>),
    /// A project read for the switch waiting with `ticket`.
    Opened {
        ticket: u64,
        result: Result<Opened, String>,
    },
    /// Changes and patch of a finished run, computed for `key`. Applies after
    /// a project switch, like `Started`.
    Changes {
        run: String,
        key: CompletionKey,
        result: Result<Loaded, String>,
    },
    /// Planned worktree apply for confirmation; `ticket` guards cancellation.
    ApplyPlan {
        ticket: u64,
        run: String,
        result: Result<ApplyPlan, String>,
    },
    /// Completed worktree apply; `ticket` guards cancellation.
    AppliedWorktree {
        ticket: u64,
        run: String,
        files: usize,
        result: Result<Applied, String>,
    },
    /// A folder read for the path picker reading with `ticket`.
    Folder {
        ticket: u64,
        result: Result<Vec<FolderEntry>, String>,
    },
    /// A task downloaded for the importer waiting with `ticket`.
    Imported {
        ticket: u64,
        result: Result<String, String>,
    },
    /// A live harness test's report, from the Harnesses page.
    HarnessTest {
        /// The batch that started it.
        batch: u64,
        harness: String,
        result: Result<super::harnesses::TestOutcome, String>,
    },
    /// A run an agent launched (or retried) for this session to drive in the
    /// background, or one to cancel; applies after a project switch too.
    Hosted(Result<Hosted, String>),
    /// Routing stats of `project`; `ticket` is the Stats dialog's wait, if any.
    Stats {
        project: PathBuf,
        ticket: Option<u64>,
        result: Result<Arc<Stats>, String>,
    },
}

/// What the session's run host asks of the app.
pub(super) enum Hosted {
    Drive(Box<crate::engine::Run>),
    Cancel(String),
}

/// The TUI as the run host: launched runs join this session and are driven
/// in the background, without taking focus.
pub(super) struct TuiHost {
    spawner: std::sync::Mutex<Spawner>,
    /// Runs this session was asked to drive or retry: the only ones its
    /// `cancel` answers for.
    driven: std::sync::Mutex<std::collections::HashSet<String>>,
}
impl TuiHost {
    pub(super) fn new(spawner: Spawner) -> Self {
        Self {
            spawner: std::sync::Mutex::new(spawner),
            driven: Default::default(),
        }
    }
    fn claim(&self, id: &str) {
        self.driven
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_owned());
    }
}
impl crate::engine::RunHost for TuiHost {
    fn drive(&self, project: &std::path::Path, id: &str) -> anyhow::Result<()> {
        self.claim(id);
        let (project, id) = (project.to_path_buf(), id.to_owned());
        self.spawner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .spawn(
                "Adopting a launched run stopped unexpectedly.",
                move || {
                    crate::engine::load(&project, &id)
                        .map(|run| Hosted::Drive(Box::new(run)))
                        .map_err(|e| format!("{e:#}"))
                },
                Msg::Hosted,
            );
        Ok(())
    }
    fn retry(&self, project: &std::path::Path, id: &str) -> anyhow::Result<()> {
        self.claim(id);
        let (project, id) = (project.to_path_buf(), id.to_owned());
        self.spawner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .spawn(
                "Retrying a launched run stopped unexpectedly.",
                move || {
                    crate::engine::command(
                        &project,
                        &id,
                        None,
                        crate::engine::Cmd::Retry {
                            interactive: false,
                            model: None,
                        },
                    )
                    .map(|run| Hosted::Drive(Box::new(run)))
                    .map_err(|e| format!("{e:#}"))
                },
                Msg::Hosted,
            );
        Ok(())
    }
    /// Cancels a run this session drives; the app holds the request until
    /// the run is adopted if it arrives first. Any other run is not ours.
    fn cancel(&self, id: &str) -> bool {
        if !self
            .driven
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(id)
        {
            return false;
        }
        let id = id.to_owned();
        self.spawner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .spawn(
                "Cancelling a launched run stopped unexpectedly.",
                move || Ok(Hosted::Cancel(id)),
                Msg::Hosted,
            );
        true
    }
}

/// A fresh number identifying a task's result, e.g. for the dialog waiting on it.
pub(super) fn ticket() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Starts tasks that report on the app's channel. Dialogs keep a clone.
#[derive(Clone)]
pub(super) struct Spawner {
    sender: Sender<(u64, Msg)>,
    generation: u64,
    /// Tasks started and not yet received, across clones.
    outstanding: Arc<AtomicUsize>,
}

impl Spawner {
    /// Runs `work` on a thread and sends `deliver(result)`. A panic delivers
    /// `Err(lost)`.
    pub(super) fn spawn<T>(
        &self,
        lost: &'static str,
        work: impl FnOnce() -> Result<T, String> + Send + 'static,
        deliver: impl FnOnce(Result<T, String>) -> Msg + Send + 'static,
    ) {
        let (sender, generation) = (self.sender.clone(), self.generation);
        self.outstanding.fetch_add(1, Ordering::Relaxed);
        thread::spawn(move || {
            let result =
                panic::catch_unwind(AssertUnwindSafe(work)).unwrap_or_else(|_| Err(lost.into()));
            // The app may have exited.
            let _ = sender.send((generation, deliver(result)));
        });
    }
}

/// The app's end of the channel.
pub(super) struct Tasks {
    spawner: Spawner,
    receiver: Receiver<(u64, Msg)>,
}

impl Default for Tasks {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            spawner: Spawner {
                sender,
                generation: 0,
                outstanding: Arc::default(),
            },
            receiver,
        }
    }
}

impl Tasks {
    /// Counts project switches; results tagged with an earlier one are stale.
    pub(super) fn generation(&self) -> u64 {
        self.spawner.generation
    }
    /// A new project: what is still running for the old one becomes stale.
    pub(super) fn next_generation(&mut self) {
        self.spawner.generation += 1;
    }
    pub(super) fn spawner(&self) -> Spawner {
        self.spawner.clone()
    }
    /// See `Spawner::spawn`.
    pub(super) fn spawn<T>(
        &self,
        lost: &'static str,
        work: impl FnOnce() -> Result<T, String> + Send + 'static,
        deliver: impl FnOnce(Result<T, String>) -> Msg + Send + 'static,
    ) {
        self.spawner.spawn(lost, work, deliver);
    }
    /// The next result, if one has arrived.
    pub(super) fn try_recv(&self) -> Option<(u64, Msg)> {
        let received = self.receiver.try_recv().ok()?;
        self.spawner.outstanding.fetch_sub(1, Ordering::Relaxed);
        Some(received)
    }
    /// Whether tasks are still running or unread.
    pub(super) fn busy(&self) -> bool {
        self.spawner.outstanding.load(Ordering::Relaxed) > 0
    }
}

#[cfg(test)]
impl Spawner {
    /// A spawner for a dialog tested alone, with the receiving end.
    pub(super) fn detached() -> (Self, Receiver<(u64, Msg)>) {
        let tasks = Tasks::default();
        (tasks.spawner, tasks.receiver)
    }
}

impl App {
    /// Applies a background result. Results for an earlier project are
    /// dropped, except a created run, a run's changes, and worktree apply
    /// work, which belong to the run.
    pub(super) fn update(&mut self, generation: u64, msg: Msg) {
        let current = generation == self.tasks.generation();
        match msg {
            Msg::Started(result) => {
                self.ui.redraw = true;
                self.starting = false;
                let started = result
                    .map_err(anyhow::Error::msg)
                    .and_then(|started| self.begin(*started));
                if let Err(error) = started {
                    self.ui.error(format!("{error:#}"));
                }
            }
            Msg::Hosted(result) => {
                self.ui.redraw = true;
                match result {
                    Ok(Hosted::Drive(run)) => {
                        // Cancelled before it was adopted: it never starts.
                        let result = if self.cancel_on_adopt.remove(&run.id) {
                            self.runs.stale = true;
                            crate::engine::cancel_pending(
                                &run.project,
                                &run.id,
                                "Cancelled by the agent that launched it",
                            )
                            .map(drop)
                        } else {
                            self.drive_in_background(*run)
                        };
                        if let Err(error) = result {
                            self.ui.error(format!("{error:#}"));
                        }
                    }
                    Ok(Hosted::Cancel(id)) => {
                        match self.session.runs.iter().find(|s| s.run.id == id) {
                            Some(slot) => slot.cancel.store(true, Ordering::Relaxed),
                            None => {
                                self.cancel_on_adopt.insert(id);
                            }
                        }
                    }
                    Err(error) => self.ui.error(error),
                }
            }
            Msg::Changes { run, key, result } => {
                self.ui.redraw = true;
                if let Some(slot) = self.session.runs.iter_mut().find(|s| s.run.id == run) {
                    slot.changes_read(&key, result);
                }
            }
            Msg::ApplyPlan {
                ticket,
                run,
                result,
            } => {
                if self.ui.take_loading(ticket) {
                    self.ui.redraw = true;
                    match result {
                        Ok(plan) => self.ui.open(Modal::Ask(Box::new(Ask::ApplyWorktree {
                            run,
                            files: plan.files.len(),
                            preview: plan.preview(),
                        }))),
                        Err(error) => self.ui.error(error),
                    }
                }
            }
            Msg::AppliedWorktree {
                ticket,
                run,
                files,
                result,
            } => {
                if self.ui.take_loading(ticket) {
                    self.ui.redraw = true;
                    match result {
                        Ok(Applied::Clean) => {
                            self.repo.checked = None;
                            self.ui.message = format!(
                                "Applied {files} file(s) from {run} into the checkout (uncommitted)."
                            );
                        }
                        Ok(Applied::Merged) => {
                            self.repo.checked = None;
                            self.ui.message = format!(
                                "Applied {files} file(s) from {run} into the checkout via 3-way merge (uncommitted)."
                            );
                        }
                        Ok(Applied::Conflict(paths)) => {
                            self.ui.error(format!(
                                "Could not apply {run} (conflicts in {}); checkout left unchanged",
                                paths.join(", ")
                            ));
                        }
                        Err(error) => self.ui.error(error),
                    }
                }
            }
            _ if !current => {}
            // A closed picker drops what it was waiting for.
            Msg::Catalog {
                harness,
                ticket: Some(ticket),
                result,
            } => {
                self.ui.redraw = true;
                if self.ui.take_loading(ticket) {
                    self.show_catalog_result(harness, result);
                }
            }
            Msg::Catalog {
                harness,
                ticket: None,
                result,
            } => {
                self.ui.redraw = true;
                self.catalog_read(harness, result);
            }
            Msg::Checks(result) => {
                self.ui.redraw = true;
                let checks = &mut self.harnesses.checks;
                checks.checking = false;
                if std::mem::take(&mut checks.again) {
                    checks.start(&self.editor.settings, &self.project, &self.tasks);
                }
                let checks = &mut self.harnesses.checks;
                match result {
                    Ok(reports) => {
                        checks.reports = reports;
                        self.harnesses.selected = self
                            .harnesses
                            .selected
                            .min(self.harnesses.checks.reports.len().saturating_sub(1));
                    }
                    Err(_) => self
                        .ui
                        .error("Harness check failed. Press [r] to retry.".into()),
                }
            }
            Msg::Runs(result) => {
                self.ui.redraw = true;
                self.runs.loading = false;
                match result {
                    Ok(entries) => self.runs.show(entries, &self.project),
                    Err(error) => self.ui.error(error),
                }
            }
            Msg::Repo(result) => {
                self.repo.reading = false;
                self.repo.checked = Some(std::time::Instant::now());
                // Read periodically; only a change needs a frame.
                if let Ok(info) = result {
                    if info != self.repo.info {
                        self.repo.info = info;
                        self.ui.redraw = true;
                    }
                }
            }
            // Esc closed the wait and went back to the browser.
            Msg::Opened { ticket, result } => {
                if self.ui.take_loading(ticket) {
                    self.ui.redraw = true;
                    self.project_opened(result.map_err(anyhow::Error::msg));
                }
            }
            Msg::Folder { ticket, result } => {
                let dialog = self.ui.dialogs.iter_mut().find(|dialog| match dialog {
                    Modal::Repositories(picker) => picker.waits_for(ticket),
                    Modal::Import(importer) => importer.waits_for(ticket),
                    _ => false,
                });
                let applied = match dialog {
                    Some(Modal::Repositories(picker)) => picker.folder_read(ticket, result),
                    Some(Modal::Import(importer)) => importer.folder_read(ticket, result),
                    _ => false,
                };
                if applied {
                    self.ui.redraw = true;
                }
            }
            Msg::Imported { ticket, result } => {
                for dialog in &mut self.ui.dialogs {
                    if let Modal::Import(importer) = dialog {
                        if importer.import_waits_for(ticket) {
                            importer.imported(ticket, result);
                            self.ui.redraw = true;
                            break;
                        }
                    }
                }
            }
            Msg::Stats {
                project,
                ticket,
                result,
            } => {
                self.ui.redraw = true;
                self.stats_collected(project, ticket, result);
            }
            Msg::HarnessTest {
                batch,
                harness,
                result,
            } => {
                self.ui.redraw = true;
                self.harness_tested(batch, harness, result);
            }
        }
    }
}
