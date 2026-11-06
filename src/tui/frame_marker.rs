//! Test hook: counts drawn frames into a side file so terminal smoke tests can
//! wait for the next redraw instead of sleeping. Off unless
//! `MODEL_ROUTER_FRAME_MARKER=1` and `MODEL_ROUTER_FRAME_FILE` names the file.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

static FILE: OnceLock<Option<PathBuf>> = OnceLock::new();
static FRAMES: AtomicU64 = AtomicU64::new(0);

/// Call after each completed draw of the main TUI loop.
pub(super) fn frame_drawn() {
    let file = FILE.get_or_init(|| {
        let enabled = std::env::var("MODEL_ROUTER_FRAME_MARKER").is_ok_and(|v| v == "1");
        enabled
            .then(|| std::env::var_os("MODEL_ROUTER_FRAME_FILE").map(PathBuf::from))
            .flatten()
    });
    if let Some(file) = file {
        write(file, FRAMES.fetch_add(1, Ordering::Relaxed) + 1);
    }
}

/// Replaces the file atomically so readers never see a partial count.
fn write(file: &Path, frame: u64) {
    let temp = file.with_extension("tmp");
    if fs::write(&temp, frame.to_string()).is_ok() {
        let _ = fs::rename(&temp, file);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn writes_the_latest_frame_number() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("frames");
        super::write(&file, 1);
        super::write(&file, 42);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "42");
        assert!(!file.with_extension("tmp").exists());
    }
}
