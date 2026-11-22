//! Who drives runs an agent launches (`run_launch`) or a schedule fires:
//! `serve` and the TUI register a host; without one (`mr run`) launched runs
//! are created pending and the user resumes them.
use super::*;
use std::sync::{Arc, RwLock};

pub trait RunHost: Send + Sync {
    /// Starts driving saved run `id` until it completes, stops, or waits.
    fn drive(&self, project: &Path, id: &str) -> Result<()>;
    /// Retries the current stage of stopped run `id` and drives it.
    fn retry(&self, project: &Path, id: &str) -> Result<()>;
    /// Cancels run `id` if this host drives it.
    fn cancel(&self, id: &str) -> bool;
}

static HOST: RwLock<Option<Arc<dyn RunHost>>> = RwLock::new(None);

/// Registers the process's run host.
pub fn set_run_host(host: Arc<dyn RunHost>) {
    *HOST.write().unwrap_or_else(|e| e.into_inner()) = Some(host);
}

/// The registered run host, if any.
pub fn run_host() -> Option<Arc<dyn RunHost>> {
    HOST.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Cancels a saved run that nothing drives yet (created pending).
pub fn cancel_pending(project: &Path, id: &str, reason: &str) -> Result<Run> {
    let directory = storage::run_dir(project, id)?;
    let _lock = storage::lock_wait(&directory.join("run.lock"), LOCK_WAIT)?;
    let mut run = load(project, id)?;
    ensure!(
        run.status == Status::Pending,
        "Run {id} is {}; only a run nothing drives yet can be cancelled here",
        run.status.label()
    );
    run.cursor = run.focus();
    run.stop(Status::Cancelled, reason.into(), Actor::User, false)?;
    run.persist()?;
    Ok(run)
}
