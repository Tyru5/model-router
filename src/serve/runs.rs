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
pub struct Registry {
    runs: Mutex<HashMap<String, Arc<Handle>>>,
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
    /// How the last drive ended: complete, confirm, interactive, stopped, or error.
    outcome: Option<String>,
    /// An error `engine::drive` returned rather than recorded in the run.
    error: Option<String>,
    backlog: VecDeque<Message>,
    subscribers: Vec<mpsc::SyncSender<Message>>,
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

impl Registry {
    fn get(&self, id: &str) -> Option<Arc<Handle>> {
        lock(&self.runs).get(id).cloned()
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
        let holder = lock(&self.runs)
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
    pub fn drive(&self, project: &Path, mut run: Run) -> Result<()> {
        let handle = {
            let mut runs = lock(&self.runs);
            let handle = runs
                .entry(run.id.clone())
                .or_insert_with(|| {
                    Arc::new(Handle {
                        project: project.to_path_buf(),
                        state: Mutex::new(State {
                            active: false,
                            workspace: run.workspace.clone(),
                            cancel: Arc::new(AtomicBool::new(false)),
                            outcome: None,
                            error: None,
                            backlog: VecDeque::new(),
                            subscribers: vec![],
                        }),
                        changed: Condvar::new(),
                    })
                })
                .clone();
            let mut state = lock(&handle.state);
            ensure!(!state.active, "Run {} is already running", run.id);
            state.active = true;
            state.workspace = run.workspace.clone();
            state.cancel = Arc::new(AtomicBool::new(false));
            state.outcome = None;
            state.error = None;
            state.backlog.clear();
            drop(state);
            handle
        };
        let cancel = lock(&handle.state).cancel.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("run {}", run.id))
            .spawn({
                let handle = handle.clone();
                move || {
                    let result = engine::drive(&mut run, &cancel, |event| {
                        handle.publish(match event {
                            Event::State(run) => {
                                Message::State(tools::status_value(&run, &Info::active()))
                            }
                            Event::Output(text) => Message::Output(text),
                        })
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
                    handle.finish(&run, outcome, error);
                }
            });
        if let Err(error) = spawned {
            lock(&handle.state).active = false;
            return Err(error.into());
        }
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

    /// Blocks until the run stops being driven here, or `timeout`. Returns
    /// whether it is still active.
    pub fn wait(&self, id: &str, timeout: Duration) -> bool {
        let Some(handle) = self.get(id) else {
            return false;
        };
        let deadline = Instant::now() + timeout;
        let mut state = lock(&handle.state);
        while state.active {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            state = handle
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        state.active
    }

    /// Recent events and a stream of later ones, while the run is active.
    pub fn subscribe(&self, id: &str) -> Option<(Vec<Message>, mpsc::Receiver<Message>)> {
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
        let handles: Vec<_> = lock(&self.runs).values().cloned().collect();
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

impl Handle {
    fn publish(&self, message: Message) {
        let mut state = lock(&self.state);
        if state.backlog.len() == BACKLOG {
            state.backlog.pop_front();
        }
        state.backlog.push_back(message.clone());
        state.subscribers.retain(|tx| {
            !matches!(
                tx.try_send(message.clone()),
                Err(mpsc::TrySendError::Disconnected(_))
            )
        });
    }

    fn finish(&self, run: &Run, outcome: &str, error: Option<String>) {
        let mut state = lock(&self.state);
        state.active = false;
        state.outcome = Some(outcome.to_owned());
        state.error = error.clone();
        let info = Info {
            active: false,
            outcome: Some(outcome.to_owned()),
            error,
        };
        let end = Message::End(tools::status_value(run, &info));
        for tx in state.subscribers.drain(..) {
            // A full queue still learns the stream ended when it disconnects.
            let _ = tx.try_send(end.clone());
        }
        drop(state);
        self.changed.notify_all();
    }
}
