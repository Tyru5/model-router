//! The workflow being edited on the Workflow page: settings, selection, the
//! task, and the undo stack.
use crate::config::{self, Settings, Stage};
use anyhow::Result;
use std::path::PathBuf;

/// Workflow snapshots kept for [u].
const UNDO_DEPTH: usize = 20;

pub(super) struct Editor {
    pub(super) settings: Settings,
    /// Where the project's settings are saved.
    pub(super) config_path: PathBuf,
    /// Index of the workflow shown.
    pub(super) workflow: usize,
    /// Index of the selected stage.
    pub(super) selected: usize,
    pub(super) task: String,
    /// Whether the next run gets its own worktree.
    pub(super) worktree: bool,
    /// Edits not yet saved.
    pub(super) dirty: bool,
    /// Workflow snapshots from before removals and deletions, newest last.
    pub(super) undo: Vec<Snapshot>,
}

/// The workflows before a removal or deletion, for [u].
pub(super) struct Snapshot {
    workflows: Vec<config::Workflow>,
    workflow: usize,
    selected: usize,
    what: String,
}

impl Editor {
    pub(super) fn new(settings: Settings, config_path: PathBuf) -> Self {
        Self {
            settings,
            config_path,
            workflow: 0,
            selected: 0,
            task: String::new(),
            worktree: false,
            dirty: false,
            undo: vec![],
        }
    }
    pub(super) fn stage(&self) -> &Stage {
        &self.settings.workflows[self.workflow].stages[self.selected]
    }
    pub(super) fn stage_mut(&mut self) -> &mut Stage {
        &mut self.settings.workflows[self.workflow].stages[self.selected]
    }
    /// Saves the settings to the project, or as global templates; returns where.
    pub(super) fn save(&mut self, global: bool) -> Result<PathBuf> {
        let path = if global {
            config::config_dir().join("config.toml")
        } else {
            self.config_path.clone()
        };
        self.settings.save(&path)?;
        self.dirty = false;
        Ok(path)
    }
    /// Saves the workflows so [u] can bring them back.
    pub(super) fn remember(&mut self, what: String) {
        if self.undo.len() == UNDO_DEPTH {
            self.undo.remove(0);
        }
        self.undo.push(Snapshot {
            workflows: self.settings.workflows.clone(),
            workflow: self.workflow,
            selected: self.selected,
            what,
        });
    }
    /// Restores the last snapshot; returns the status line.
    pub(super) fn restore(&mut self) -> String {
        let Some(snapshot) = self.undo.pop() else {
            return "Nothing to undo.".into();
        };
        self.settings.workflows = snapshot.workflows;
        self.workflow = snapshot.workflow.min(self.settings.workflows.len() - 1);
        let stages = self.settings.workflows[self.workflow].stages.len();
        self.selected = snapshot.selected.min(stages.saturating_sub(1));
        self.dirty = true;
        format!("Restored {}.", snapshot.what)
    }
}
