//! Runs this server drives, each on its own thread, and their event streams.

use super::tools;
use crate::{
    engine::{self, Action, Event, Placement, Run},
    storage,
};
use anyhow::{Result, ensure};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// Events kept for subscribers that connect mid-run.
const BACKLOG: usize = 500;
/// Events queued per subscriber; a stalled one loses events, never the run.
const SUBSCRIBER_QUEUE: usize = 1024;
/// Finished run handles kept in memory; older inactive ones are evicted so
/// long-lived servers do not grow unbounded. Disk records remain intact.
const MAX_FINISHED_RUNS: usize = 128;

#[derive(Debug, Clone)]
pub enum Message {
    /// The run's status, as `run_status` reports it.
    State(Value),
    /// Harness or check output.
    Output(String),
    /// The drive thread finished; carries the final status.
    End(Value),
}

#[derive(Default)]
struct Store {
    runs: HashMap<String, Arc<Handle>>,
    finished: VecDeque<String>,
}

#[derive(Default)]
pub struct Registry {
    store: Mutex<Store>,
    /// Serializes placement decisions with run creation, so two concurrent
    /// starts cannot both choose the same checkout.
    starting: Mutex<()>,
}

pub struct Handle {
    pub project: PathBuf,
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    active: bool,
    workspace: PathBuf,
    cancel: Arc<AtomicBool>,
    /// Incremented on every state update and on completion, so waiters can
    /// observe progress without polling.
    version: u64,
    last_state: Option<Value>,
    /// How the last drive ended: complete, confirm, interactive, stopped, or error.
    outcome: Option<String>,
    /// An error `engine::drive` returned rather than recorded in the run.
    error: Option<String>,
    backlog: VecDeque<Arc<Message>>,
    subscribers: Vec<mpsc::SyncSender<Arc<Message>>>,
}

/// What the server knows about a run beyond run.json.
#[derive(Debug, Clone, Default)]
pub struct Info {
    pub active: bool,
    pub outcome: Option<String>,
    pub error: Option<String>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

struct ReserveGuard {
    handle: Arc<Handle>,
    armed: bool,
}

impl Drop for ReserveGuard {
    fn drop(&mut self) {
        if self.armed {
            let mut state = lock(&self.handle.state);
            state.active = false;
            state.version += 1;
            drop(state);
            self.handle.changed.notify_all();
        }
    }
}

fn new_handle(project: &Path, workspace: &Path) -> Arc<Handle> {
    Arc::new(Handle {
        project: project.to_path_buf(),
        state: Mutex::new(State {
            active: false,
            workspace: workspace.to_path_buf(),
            cancel: Arc::new(AtomicBool::new(false)),
            version: 0,
            last_state: None,
            outcome: None,
            error: None,
            backlog: VecDeque::new(),
            subscribers: vec![],
        }),
        changed: Condvar::new(),
    })
}

impl Registry {
    fn get(&self, id: &str) -> Option<Arc<Handle>> {
        lock(&self.store).runs.get(id).cloned()
    }

    /// Evicts the oldest inactive handles while more than `max` runs are
    /// tracked. An active run is never evicted and keeps its place in the
    /// queue, so it is evicted in turn once it has finished.
    fn evict_finished(&self, max: usize) {
        let mut store = lock(&self.store);
        let mut excess = store.finished.len().saturating_sub(max);
        let Store { runs, finished } = &mut *store;
        finished.retain(|id| {
            if excess == 0 {
                return true;
            }
            if runs
                .get(id)
                .is_some_and(|handle| lock(&handle.state).active)
            {
                return true;
            }
            runs.remove(id);
            excess -= 1;
            false
        });
    }

    pub fn info(&self, id: &str) -> Info {
        self.get(id).map_or_else(Info::default, |handle| {
            let state = lock(&handle.state);
            Info {
                active: state.active,
                outcome: state.outcome.clone(),
                error: state.error.clone(),
            }
        })
    }

    pub fn is_active(&self, id: &str) -> bool {
        self.get(id)
            .is_some_and(|handle| lock(&handle.state).active)
    }

    /// Where a new run in `project` works: a worktree when asked, or when
    /// another run (here or in another router process) is using the checkout.
    fn placement(&self, project: &Path, worktree: bool) -> Result<(Placement, Option<String>)> {
        if worktree {
            return Ok((Placement::Worktree, None));
        }
        let holder = lock(&self.store)
            .runs
            .iter()
            .find(|(_, handle)| {
                let state = lock(&handle.state);
                state.active && state.workspace == project
            })
            .map(|(id, _)| format!("run {id}"))
            .or_else(|| {
                let lock = storage::metadata_dir(project).ok()?.join("workspace.lock");
                storage::is_locked(&lock).then(|| "another router process".to_owned())
            });
        let Some(holder) = holder else {
            return Ok((Placement::Project, None));
        };
        ensure!(
            storage::repo_info(project).is_some(),
            "{holder} is using {}. It is not a Git repository, so a second run cannot get its own worktree; wait for the first to finish",
            project.display()
        );
        Ok((Placement::WorktreeFromHead, Some(holder)))
    }

    /// Creates a run and starts driving it. Returns the run and, when the
    /// checkout was busy, what was using it.
    pub fn start(
        &self,
        project: &Path,
        worktree: bool,
        create: impl FnOnce(Placement) -> Result<Run>,
    ) -> Result<(Run, Option<String>)> {
        let _starting = lock(&self.starting);
        let (placement, holder) = self.placement(project, worktree)?;
        let run = create(placement)?;
        self.drive(project, run.clone())?;
        Ok((run, holder))
    }

    /// Drives `run` on a new thread until it completes, stops, or waits.
    pub fn drive(&self, project: &Path, run: Run) -> Result<()> {
        let id = run.id.clone();
        let handle = {
            let mut store = lock(&self.store);
            let handle = store
                .runs
                .entry(id.clone())
                .or_insert_with(|| new_handle(project, &run.workspace))
                .clone();
            let mut state = lock(&handle.state);
            ensure!(!state.active, "Run {id} is already running");
            state.active = true;
            state.version += 1;
            drop(state);
            handle
        };
        let guard = ReserveGuard {
            handle: handle.clone(),
            armed: true,
        };
        self.spawn_run(&handle, run, guard)
    }

    /// Atomically reserves `id` as active before calling `prepare()` so two
    /// concurrent `confirm_stage`/`retry_stage` calls cannot race on `run.json`.
    pub fn resume_run(
        &self,
        project: &Path,
        id: &str,
        prepare: impl FnOnce() -> Result<Run>,
    ) -> Result<Run> {
        let (handle, created) = {
            let mut store = lock(&self.store);
            let created = !store.runs.contains_key(id);
            let handle = store
                .runs
                .entry(id.to_owned())
                .or_insert_with(|| new_handle(project, project))
                .clone();
            ensure!(
                handle.project == project,
                "Run {id} belongs to project {}",
                handle.project.display()
            );
            let mut state = lock(&handle.state);
            ensure!(!state.active, "Run {id} is running; wait for it first");
            state.active = true;
            state.version += 1;
            drop(state);
            (handle, created)
        };
        let guard = ReserveGuard {
            handle: handle.clone(),
            armed: true,
        };
        let run = match prepare() {
            Ok(run) => run,
            Err(err) => {
                // A handle this call made for a run that failed to load
                // (wrong project, unknown ID) would otherwise stay forever
                // and pin the run to the caller's project. It is removed while
                // still reserved, so no other call can take it over first;
                // dropping the guard then releases it for any waiter.
                if created {
                    let mut store = lock(&self.store);
                    if store.runs.get(id).is_some_and(|h| Arc::ptr_eq(h, &handle)) {
                        store.runs.remove(id);
                    }
                }
                drop(guard);
                return Err(err);
            }
        };
        let snapshot = run.clone();
        self.spawn_run(&handle, run, guard)?;
        Ok(snapshot)
    }

    fn spawn_run(&self, handle: &Arc<Handle>, mut run: Run, mut guard: ReserveGuard) -> Result<()> {
        let id = run.id.clone();
        let cancel = {
            let mut state = lock(&handle.state);
            state.active = true;
            state.workspace = run.workspace.clone();
            let cancel = Arc::new(AtomicBool::new(false));
            state.cancel = cancel.clone();
            state.version += 1;
            state.last_state = None;
            state.outcome = None;
            state.error = None;
            state.backlog.clear();
            cancel
        };
        let spawned = std::thread::Builder::new()
            .name(format!("run {id}"))
            .spawn({
                let handle = handle.clone();
                move || {
                    let pricing = crate::cost::Pricing::load(&run.project);
                    let result = engine::drive(&mut run, &cancel, |event| match event {
                        Event::State(run) => handle.publish(Message::State(
                            tools::status_value_with(&run, &Info::active(), &pricing),
                        )),
                        Event::Output(text) => handle.publish(Message::Output(text)),
                        Event::InteractiveHandoff(_) => {}
                    });
                    let (outcome, error) = match result {
                        Ok(action) => (
                            match action {
                                Action::Complete => "complete",
                                Action::Confirm => "confirm",
                                Action::Interactive => "interactive",
                                Action::Stopped => "stopped",
                            },
                            None,
                        ),
                        Err(error) => {
                            super::log(format!("run {}: {error:#}", run.id));
                            ("error", Some(format!("{error:#}")))
                        }
                    };
                    handle.finish(&run, outcome, error, &pricing);
                }
            });
        spawned?;
        guard.armed = false;
        // Tracked for eviction once it is inactive.
        {
            let mut store = lock(&self.store);
            store.finished.retain(|existing| existing != &id);
            store.finished.push_back(id);
        }
        self.evict_finished(MAX_FINISHED_RUNS);
        Ok(())
    }

    /// Asks an active run to stop. False when this server is not driving it.
    pub fn cancel(&self, id: &str) -> bool {
        self.get(id).is_some_and(|handle| {
            let state = lock(&handle.state);
            if state.active {
                state.cancel.store(true, Ordering::Relaxed);
            }
            state.active
        })
    }

    /// Blocks until the run stops being driven here, `timeout` elapses, or
    /// `cancel` is set. Invokes `on_progress` whenever the run publishes a new
    /// state snapshot. Returns whether the run is still active.
    pub fn wait_with(
        &self,
        id: &str,
        timeout: Duration,
        cancel: Option<&AtomicBool>,
        mut on_progress: impl FnMut(&Value),
    ) -> bool {
        let Some(handle) = self.get(id) else {
            return false;
        };
        let deadline = Instant::now() + timeout;
        let mut state = lock(&handle.state);
        let mut seen = state.version;
        while state.active {
            if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let step = if cancel.is_some() {
                Duration::from_millis(100).min(deadline - now)
            } else {
                deadline - now
            };
            state = handle
                .changed
                .wait_timeout(state, step)
                .unwrap_or_else(|e| e.into_inner())
                .0;
            if state.version != seen {
                seen = state.version;
                if let Some(snapshot) = state.last_state.clone() {
                    drop(state);
                    on_progress(&snapshot);
                    state = lock(&handle.state);
                }
            }
        }
        state.active
    }

    /// Recent events and a stream of later ones, while the run is active.
    pub fn subscribe(&self, id: &str) -> Option<Subscription> {
        let handle = self.get(id)?;
        let mut state = lock(&handle.state);
        if !state.active {
            return None;
        }
        let (tx, rx) = mpsc::sync_channel(SUBSCRIBER_QUEUE);
        state.subscribers.push(tx);
        Some((state.backlog.iter().cloned().collect(), rx))
    }

    /// The project a run driven here belongs to.
    pub fn project_of(&self, id: &str) -> Option<PathBuf> {
        self.get(id).map(|handle| handle.project.clone())
    }

    /// Cancels every active run and waits up to `grace` for them to stop.
    pub fn shutdown(&self, grace: Duration) {
        let handles: Vec<_> = lock(&self.store).runs.values().cloned().collect();
        for handle in &handles {
            let state = lock(&handle.state);
            if state.active {
                state.cancel.store(true, Ordering::Relaxed);
            }
        }
        let deadline = Instant::now() + grace;
        for handle in &handles {
            let mut state = lock(&handle.state);
            while state.active && Instant::now() < deadline {
                state = handle
                    .changed
                    .wait_timeout(state, deadline.saturating_duration_since(Instant::now()))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }
    }
}

impl Info {
    fn active() -> Self {
        Self {
            active: true,
            ..Self::default()
        }
    }
}

pub type Subscription = (Vec<Arc<Message>>, mpsc::Receiver<Arc<Message>>);

impl Handle {
    fn publish(&self, message: Message) {
        let notify = matches!(message, Message::State(_));
        let mut state = lock(&self.state);
        if let Message::State(value) = &message {
            state.version += 1;
            state.last_state = Some(value.clone());
        }
        let message = Arc::new(message);
        if state.backlog.len() == BACKLOG {
            state.backlog.pop_front();
        }
        state.backlog.push_back(Arc::clone(&message));
        state.subscribers.retain(|tx| {
            !matches!(
                tx.try_send(Arc::clone(&message)),
                Err(mpsc::TrySendError::Disconnected(_))
            )
        });
        drop(state);
        if notify {
            self.changed.notify_all();
        }
    }

    fn finish(
        &self,
        run: &Run,
        outcome: &str,
        error: Option<String>,
        pricing: &crate::cost::Pricing,
    ) {
        let mut state = lock(&self.state);
        state.active = false;
        state.version += 1;
        state.outcome = Some(outcome.to_owned());
        state.error = error.clone();
        let info = Info {
            active: false,
            outcome: Some(outcome.to_owned()),
            error,
        };
        let status = tools::status_value_with(run, &info, pricing);
        state.last_state = Some(status.clone());
        let end = Arc::new(Message::End(status));
        for tx in state.subscribers.drain(..) {
            // A full queue still learns the stream ended when it disconnects.
            let _ = tx.try_send(Arc::clone(&end));
        }
        drop(state);
        self.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_oldest_inactive_handles_past_cap() {
        let registry = Registry::default();
        {
            let mut store = lock(&registry.store);
            for i in 0..5 {
                let id = format!("run-{i}");
                let active = i == 0;
                store.runs.insert(
                    id.clone(),
                    Arc::new(Handle {
                        project: PathBuf::from("/tmp"),
                        state: Mutex::new(State {
                            active,
                            workspace: PathBuf::from("/tmp"),
                            cancel: Arc::new(AtomicBool::new(false)),
                            version: 0,
                            last_state: None,
                            outcome: None,
                            error: None,
                            backlog: VecDeque::new(),
                            subscribers: vec![],
                        }),
                        changed: Condvar::new(),
                    }),
                );
                store.finished.push_back(id);
            }
        }
        registry.evict_finished(2);
        {
            let store = lock(&registry.store);
            // Three over the cap: the three oldest inactive runs go. Active
            // run-0 is skipped and keeps its place in the queue.
            assert!(store.runs.contains_key("run-0"));
            assert!(!store.runs.contains_key("run-1"));
            assert!(!store.runs.contains_key("run-2"));
            assert!(!store.runs.contains_key("run-3"));
            assert!(store.runs.contains_key("run-4"));
            assert_eq!(store.finished, ["run-0", "run-4"]);
            // Nothing to do at the cap.
        }
        registry.evict_finished(2);
        assert_eq!(lock(&registry.store).runs.len(), 2);
        // Once run-0 has finished it is the oldest and is evicted in turn.
        {
            let store = lock(&registry.store);
            lock(&store.runs["run-0"].state).active = false;
        }
        registry.evict_finished(1);
        let store = lock(&registry.store);
        assert!(!store.runs.contains_key("run-0"));
        assert!(store.runs.contains_key("run-4"));
        assert_eq!(store.finished, ["run-4"]);
    }
}
