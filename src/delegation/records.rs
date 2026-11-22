//! What `run.json` records of delegation: a parent attempt's wake turns and
//! the tasks it started. Every field defaults, so older runs load unchanged.
use super::config::Isolation;
use super::policy::Ceiling;
use crate::config::Role;
use crate::handoff::{Handoff, Usage};
use crate::process;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Starting,
    Running,
    /// Blocked on a question; `task_send` answers it.
    Waiting,
    Completed,
    Failed,
    /// A question that cannot be answered, or was left unanswered.
    Blocked,
    Cancelled,
    /// `mr` stopped while the task ran.
    Interrupted,
}
impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Blocked | Self::Cancelled | Self::Interrupted
        )
    }
    pub fn is_live(self) -> bool {
        matches!(self, Self::Queued | Self::Starting | Self::Running)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }
    /// The tool result's `work_state`.
    pub fn work_state(self) -> &'static str {
        if self.is_live() {
            "working"
        } else if self == Self::Waiting {
            "needs_answer"
        } else {
            "result_available"
        }
    }
}

/// The target a task runs on, resolved at `delegate_task`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedTarget {
    /// The named delegate, if one was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegate: Option<String>,
    pub harness: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub effort: String,
}

/// One harness process of a task: its first launch or a `task_send` turn.
/// The fields of an `Attempt` a process needs, with no tasks or turns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTurn {
    pub number: usize,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub directory: PathBuf,
    pub handoff_file: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_group: Option<u32>,
    #[serde(default, skip_serializing_if = "process::Identity::is_empty")]
    pub process_identity: process::Identity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_after: Option<String>,
    /// The message `task_send` sent; none for the first turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}
impl TaskTurn {
    pub fn group(&self) -> Option<process::Group> {
        self.process_group.map(|pgid| process::Group {
            pgid,
            identity: self.process_identity.clone(),
        })
    }
    pub fn set_group(&mut self, group: Option<process::Group>) {
        let (pgid, identity) = match group {
            Some(group) => (Some(group.pgid), group.identity),
            None => (None, process::Identity::default()),
        };
        self.process_group = pgid;
        self.process_identity = identity;
    }
    pub fn take_group(&mut self) -> Option<process::Group> {
        let group = self.group();
        self.set_group(None);
        group
    }
}

/// Before and after a `task_apply`, so a crash mid-apply has an answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyJournal {
    pub pre_tree: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_post_tree: Option<String>,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<DateTime<Utc>>,
    /// Set by recovery when the checkout matches neither tree.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unknown: bool,
}

/// A worktree task's full diff from its base after one turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedPatch {
    pub revision: usize,
    pub turn: usize,
    pub path: PathBuf,
    pub sha256: String,
    pub files: Vec<String>,
    pub added: u64,
    pub removed: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply: Option<ApplyJournal>,
}

/// One delegated task of a parent attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskState {
    pub id: String,
    pub client_request_id: String,
    /// SHA-256 of the request payload, for `request_conflict`.
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    pub role: Role,
    pub target: ResolvedTarget,
    pub isolation: Isolation,
    pub ceiling: Ceiling,
    pub status: TaskStatus,
    /// The brief the task was given.
    pub task: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turns: Vec<TaskTurn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<Handoff>,
    /// The handoff of the latest turn, when a later `task_send` turn ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_handoff: Option<Handoff>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<String>,
    /// The task's own worktree, for `isolation = worktree`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<PathBuf>,
    /// The snapshot the worktree started from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patches: Vec<SealedPatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// A tool result returned it terminal or waiting, or the parent cancelled it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delivered: bool,
    /// A wake prompt named it while undelivered.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub announced: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled_by_parent: bool,
    /// `task_send` messages waiting for the current turn to end.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_messages: Vec<String>,
    pub created_at: DateTime<Utc>,
}
impl TaskState {
    pub fn usage(&self) -> impl Iterator<Item = &TaskTurn> {
        self.turns.iter()
    }
    /// Whether the task's latest result is unread by the parent.
    pub fn undelivered(&self) -> bool {
        !self.delivered && !self.cancelled_by_parent
    }
}

/// A parent's wake turn: one `follow_up` process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub number: usize,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    pub directory: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Tasks the wake prompt announced.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub announced: Vec<String>,
}
