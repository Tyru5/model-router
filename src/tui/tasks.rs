//! Background work. Each task runs on its own thread and reports one `Msg`
//! on the app's channel, tagged with the project generation it was started
//! for; `App::update` applies it and drops results for an earlier project.
use super::{
    app::{App, Opened, Started},
    completion::Loaded,
    modal::Modal,
    path_picker::FolderEntry,
};
use crate::{catalog::Catalog, doctor::Report, history::Entry, stats::Stats, storage::RepoInfo};
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
        key: String,
        result: Result<Loaded, String>,
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
    /// Routing stats of `project`; `ticket` is the Stats dialog's wait, if any.
    Stats {
        project: PathBuf,
        ticket: Option<u64>,
        result: Result<Arc<Stats>, String>,
    },
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
    #[cfg(test)]
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
    /// dropped, except a created run and a run's changes, which belong to
    /// the run.
    pub(super) fn update(&mut self, generation: u64, msg: Msg) {
        let current = generation == self.tasks.generation();
        self.ui.redraw = true;
        match msg {
            Msg::Started(result) => {
                self.starting = false;
                let started = result
                    .map_err(anyhow::Error::msg)
                    .and_then(|started| self.begin(*started));
                if let Err(error) = started {
                    self.ui.error(format!("{error:#}"));
                }
            }
            Msg::Changes { run, key, result } => {
                if let Some(slot) = self.session.runs.iter_mut().find(|s| s.run.id == run) {
                    slot.changes_read(&key, result);
                }
            }
            _ if !current => {}
            // A closed picker drops what it was waiting for.
            Msg::Catalog {
                harness,
                ticket: Some(ticket),
                result,
            } => {
                if self.ui.take_loading(ticket) {
                    self.show_catalog_result(harness, result);
                }
            }
            Msg::Catalog {
                harness,
                ticket: None,
                result,
            } => self.catalog_read(harness, result),
            Msg::Checks(result) => {
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
                self.runs.loading = false;
                match result {
                    Ok(entries) => self.runs.show(entries, &self.project),
                    Err(error) => self.ui.error(error),
                }
            }
            Msg::Repo(result) => {
                self.repo.reading = false;
                self.repo.checked = Some(std::time::Instant::now());
                // Read every few seconds; only a change needs a frame.
                self.ui.redraw = false;
                if let Ok(info) = result {
                    self.ui.redraw = info != self.repo.info;
                    self.repo.info = info;
                }
            }
            // Esc closed the wait and went back to the browser.
            Msg::Opened { ticket, result } => {
                if self.ui.take_loading(ticket) {
                    self.project_opened(result.map_err(anyhow::Error::msg));
                }
            }
            Msg::Folder { ticket, result } => {
                let dialog = self.ui.dialogs.iter_mut().find(|dialog| match dialog {
                    Modal::Repositories(picker) => picker.waits_for(ticket),
                    Modal::Import(importer) => importer.waits_for(ticket),
                    _ => false,
                });
                match dialog {
                    Some(Modal::Repositories(picker)) => picker.folder_read(ticket, result),
                    Some(Modal::Import(importer)) => importer.folder_read(ticket, result),
                    _ => false,
                };
            }
            Msg::Imported { ticket, result } => {
                for dialog in &mut self.ui.dialogs {
                    if let Modal::Import(importer) = dialog {
                        if importer.import_waits_for(ticket) {
                            importer.imported(ticket, result);
                            break;
                        }
                    }
                }
            }
            Msg::Stats {
                project,
                ticket,
                result,
            } => self.stats_collected(project, ticket, result),
            Msg::HarnessTest {
                batch,
                harness,
                result,
            } => self.harness_tested(batch, harness, result),
        }
    }
}
