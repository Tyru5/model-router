use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Running,
    Interactive,
    AwaitingConfirmation,
    Complete,
    Failed,
    Blocked,
    Cancelled,
    Skipped,
}
impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Interactive => "interactive",
            Self::AwaitingConfirmation => "inspect handoff",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
            Self::Skipped => "disabled",
        }
    }
    /// The saved name, as in run.json.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Interactive => "interactive",
            Self::AwaitingConfirmation => "awaiting_confirmation",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }
    pub(super) fn stopped(self) -> bool {
        matches!(self, Self::Failed | Self::Blocked | Self::Cancelled)
    }
}

/// Whether a run, stage, or attempt may move from `from` to `to`. One status
/// type serves all three, so each level admits only its own values: a run is
/// never skipped, a stage never waits for a terminal, and an attempt only runs
/// and ends. `enabled` is the stage's switch; disabled stages are skipped.
pub(super) fn legal(level: Level, from: Status, to: Status, enabled: bool) -> bool {
    use Status::*;
    match level {
        Level::Run => match from {
            Pending => matches!(
                to,
                Running | Interactive | Complete | Failed | Blocked | Cancelled
            ),
            Running => matches!(
                to,
                Pending | AwaitingConfirmation | Failed | Blocked | Cancelled
            ),
            Interactive => matches!(to, Running | Pending | Failed | Cancelled),
            AwaitingConfirmation => matches!(to, Pending | Interactive | Failed | Blocked),
            Failed | Blocked | Cancelled => matches!(to, Pending | Interactive),
            Complete | Skipped => false,
        },
        Level::Stage => match (from, to) {
            (_, Interactive) => false,
            (_, Skipped) => !enabled,
            (_, Pending) => enabled,
            (Pending, Running | Failed | Blocked | Cancelled) => true,
            (Running, Complete | AwaitingConfirmation | Failed | Blocked | Cancelled) => true,
            (AwaitingConfirmation, Complete | Failed | Blocked) => true,
            _ => false,
        },
        Level::Attempt => match (from, to) {
            (Pending, Running) => true,
            (Running, Complete | Failed | Blocked | Cancelled) => true,
            // A checkpoint or bookkeeping stops a finished attempt.
            (Complete, Failed | Blocked) => true,
            // `allow_repair`: the review completed; only the limit held it.
            (Blocked, Complete) => true,
            // Saved by versions that left interactive attempts waiting.
            (AwaitingConfirmation, Complete | Failed | Blocked) => true,
            _ => false,
        },
        // A fan-out candidate runs, ends, and runs again only when retried.
        Level::Candidate => matches!(
            (from, to),
            (Pending, Running | Failed | Cancelled)
                | (Running, Complete | Failed | Blocked | Cancelled)
                | (Failed | Blocked | Cancelled, Pending)
        ),
    }
}

/// One status change of the run, a stage, or a stage's latest attempt.
#[derive(Debug, Clone)]
pub struct Transition {
    pub target: Target,
    pub to: Status,
    pub actor: Actor,
    pub reason: String,
}
impl Transition {
    pub fn new(target: Target, to: Status, actor: Actor, reason: impl Into<String>) -> Self {
        Self {
            target,
            to,
            actor,
            reason: reason.into(),
        }
    }
}

/// A recovery or checkpoint command, applied by `command` under the run lock.
#[derive(Debug, Clone)]
pub enum Cmd {
    /// Rerun the current stage, optionally interactively or with another model.
    Retry {
        interactive: bool,
        model: Option<String>,
    },
    /// Reopen the stopped stage's session interactively.
    TakeOver,
    /// Accept the handoff at the checkpoint.
    Confirm,
    /// One more review repair pass beyond the limit.
    AllowRepair,
    /// Replace the summary of the handoff awaiting confirmation.
    EditSummary(String),
    /// Rerun one stopped candidate of a stopped fan-out stage, keeping the others.
    RetryCandidate { stage: String, candidate: String },
    /// Raise the run's USD or token cap, and any exceeded stage cap, to the
    /// given amount; unblocks a run its budget stopped. See `cost::raise_budget`.
    RaiseBudget {
        usd: Option<f64>,
        tokens: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attempt {
    pub number: usize,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub model: String,
    pub mode: Mode,
    pub status: Status,
    pub exit_code: Option<i32>,
    pub directory: PathBuf,
    pub handoff_file: PathBuf,
    pub error: Option<String>,
    /// Router-executed check results; empty when the stage has no checks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<CheckResult>,
    /// Workspace Git trees before the harness starts and after its checks;
    /// None outside Git or when Git failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_after: Option<String>,
    /// The workspace Git tree the router's checks ran against, taken just
    /// before they started: what `checks` are a receipt for. None without
    /// checks, outside Git, or in runs saved before receipts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks_tree: Option<String>,
    /// Tokens and cost the harness reported; None when it reported none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<handoff::Usage>,
    /// Harness session the router assigned at launch, so the stage can be
    /// taken over interactively. Codex reports its own in stdout.log instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The unattended or interactive harness's process group while it runs.
    /// Cleared when the harness is reaped; still set after mr itself died,
    /// so the next `drive` can stop the orphaned group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_group: Option<u32>,
    /// What tells `process_group` from a later group that reuses its ID.
    /// Empty in runs saved before this was recorded; recovery then leaves
    /// the group alone.
    #[serde(default, skip_serializing_if = "process::Identity::is_empty")]
    pub process_identity: process::Identity,
    /// The socket of an interactive harness's terminal host. Its host
    /// outlives mr, so while the attempt runs a later mr reattaches here
    /// instead of treating the attempt as interrupted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<PathBuf>,
    /// Where the accepted handoff came from; None before it was read and in
    /// runs saved before this was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_source: Option<HandoffSource>,
    /// A fan-out candidate attempt's own Git worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<PathBuf>,
    /// Tasks a delegating stage's attempt started; see `delegation`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<crate::delegation::records::TaskState>,
    /// A delegating attempt's wake turns after its first process.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turns: Vec<crate::delegation::records::Turn>,
}
impl Attempt {
    /// The recorded process group, with its identity.
    pub fn group(&self) -> Option<process::Group> {
        self.process_group.map(|pgid| process::Group {
            pgid,
            identity: self.process_identity.clone(),
        })
    }
    /// Records `group` as this attempt's running process group, or none.
    pub fn set_group(&mut self, group: Option<process::Group>) {
        let (pgid, identity) = match group {
            Some(group) => (Some(group.pgid), group.identity),
            None => (None, process::Identity::default()),
        };
        self.process_group = pgid;
        self.process_identity = identity;
    }
    pub(super) fn take_group(&mut self) -> Option<process::Group> {
        let group = self.group();
        self.set_group(None);
        group
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageState {
    pub status: Status,
    pub attempts: Vec<Attempt>,
    pub handoff: Option<Handoff>,
    /// User acceptance of a test stage that reported no applicable check,
    /// or of a ship stage's plan (see `ship::confirm`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiver: Option<Waiver>,
    /// A fan-out stage's candidates and choice; None for other stages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_out: Option<FanOutState>,
    /// In a fork, the source run's attempt directory whose handoff this
    /// stage reused without running; see `history::fork`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherited: Option<PathBuf>,
    /// Candidates of fan-out rounds that a rewind discarded, oldest first.
    /// Kept so their spend still counts toward reports and budgets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discarded: Vec<CandidateState>,
}
impl StageState {
    /// Every turn of every task the stage's attempts delegated.
    pub fn task_turns(
        &self,
    ) -> impl Iterator<
        Item = (
            &crate::delegation::records::TaskState,
            &crate::delegation::records::TaskTurn,
        ),
    > {
        self.attempts
            .iter()
            .flat_map(|a| &a.tasks)
            .flat_map(|t| t.turns.iter().map(move |turn| (t, turn)))
    }
    /// Every fan-out candidate attempt with its candidate: the current
    /// round's first, in order, then discarded rounds'.
    pub fn candidate_attempts(&self) -> impl Iterator<Item = (&CandidateState, &Attempt)> {
        self.fan_out
            .iter()
            .flat_map(|f| &f.candidates)
            .chain(&self.discarded)
            .flat_map(|c| c.attempts.iter().map(move |a| (c, a)))
    }
    /// The attempt whose router checks stand for the stage: the latest one,
    /// or for a fan-out stage whose winner reached the workspace, that
    /// candidate's latest one, since candidates run the checks and the
    /// stage's own attempt runs none.
    pub fn checked(&self) -> Option<&Attempt> {
        let chosen = self.fan_out.as_ref().and_then(|fan_out| {
            let selection = fan_out.selection.as_ref().filter(|s| s.applied)?;
            fan_out
                .candidates
                .iter()
                .find(|c| c.id == selection.candidate)?
                .attempts
                .last()
        });
        chosen.or_else(|| self.attempts.last())
    }
}

/// One round of a best-of-N stage: where candidates started, each
/// candidate, and which one reached the workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanOutState {
    /// The workspace snapshot every candidate started from.
    pub base: String,
    /// The private commit of `base` their worktrees check out.
    pub base_commit: String,
    /// Stage attempt number that opened this fan-out round; 0 in legacy records.
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub round: usize,
    pub candidates: Vec<CandidateState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<Selection>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateState {
    pub id: String,
    pub harness: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub effort: String,
    pub status: Status,
    pub attempts: Vec<Attempt>,
    pub handoff: Option<Handoff>,
    /// What the latest attempt changed, from the base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<CandidateChanges>,
    /// Why the latest attempt stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateChanges {
    pub files: Vec<String>,
    pub added: u64,
    pub removed: u64,
    /// The full patch from the base; what the router applies if chosen.
    pub patch: PathBuf,
}
impl CandidateChanges {
    pub fn lines(&self) -> u64 {
        self.added + self.removed
    }
}
impl CandidateState {
    /// Whether every required router check of its latest attempt passed.
    pub fn checks_pass(&self) -> bool {
        self.attempts.last().is_none_or(|a| {
            a.checks
                .iter()
                .all(|c| !c.required || c.status == TestStatus::Passed)
        })
    }
}
/// The judge's choice and whether its patch reached the workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Selection {
    pub candidate: String,
    /// `auto`, or the judge stage's ID.
    pub by: String,
    pub reason: String,
    pub applied: bool,
    pub at: DateTime<Utc>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Waiver {
    pub reason: String,
    pub accepted_at: DateTime<Utc>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub version: u32,
    pub revision: u64,
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub project: PathBuf,
    pub workspace: PathBuf,
    /// Workspace Git tree when the run was created; None outside Git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<String>,
    pub directory: PathBuf,
    pub worktree: bool,
    pub task: String,
    /// The saved run this one reran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerun_of: Option<String>,
    /// The saved run and stage checkpoint this run was forked from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_of: Option<Fork>,
    pub workflow: Workflow,
    pub adapters: BTreeMap<String, CustomAdapter>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, Provider>,
    /// The named delegates the workflow's stages may use, snapshotted at
    /// creation so a replay never reads the current config's.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub delegates: BTreeMap<String, crate::delegation::Delegate>,
    /// The run and stage attempt whose agent launched this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launched_by: Option<LaunchedBy>,
    /// Who created the run: the user, an agent, or a schedule.
    #[serde(default, skip_serializing_if = "CreationSource::is_user")]
    pub creation_source: CreationSource,
    pub stages: Vec<StageState>,
    /// Groups from discarded fan-out rounds, awaiting cleanup under both locks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) orphan_groups: Vec<process::Group>,
    /// Groups recovery found alive but could not prove are this run's, so
    /// left running (see `stop_orphans`). While one is alive no harness
    /// launches for the run; `release_groups` forgets them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) retained_groups: Vec<process::Group>,
    pub cursor: usize,
    pub repair_passes: u32,
    /// Repair passes the user allowed beyond the workflow's limit.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub extra_repair_passes: u32,
    pub repair_feedback: Option<String>,
    /// Index of the review whose findings `repair_feedback` holds. Stages up
    /// to and including it see the feedback; it is cleared once that review
    /// passes. None in runs saved before this was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_review: Option<usize>,
    pub status: Status,
    pub error: Option<String>,
    /// The next interactive attempt resumes this harness session instead of
    /// starting a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<Resume>,
    /// Status changes not yet saved; `persist` appends them to transitions.jsonl.
    #[serde(skip)]
    pub(super) journal: Vec<transitions::Record>,
    /// While stages run side by side, their stops and repair requests wait
    /// here and the run's own status settles once they all end.
    #[serde(skip)]
    pub(super) hold: Option<Hold>,
    /// The fan-out candidate whose prompt `prompt` renders.
    #[serde(skip)]
    pub(super) view: Option<usize>,
    /// The bridge of a delegating stage's next launch, which `prepare`
    /// renders into its invocation and sandbox.
    #[serde(skip)]
    pub(super) mcp: Option<crate::adapters::McpLaunch>,
}

/// The parent that launched a run with `run_launch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchedBy {
    pub run: String,
    pub stage: String,
    pub attempt: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CreationSource {
    #[default]
    User,
    Agent,
    Schedule,
}
impl CreationSource {
    pub fn is_user(&self) -> bool {
        *self == Self::User
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Schedule => "schedule",
        }
    }
}

/// Where a fork started: the source run, the stage it continues from, that
/// stage's attempt whose starting snapshot it reproduces, and the snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fork {
    pub run: String,
    pub stage: String,
    pub attempt: usize,
    pub snapshot: String,
    /// The continuation's model, when it replaced the stage's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Run-level outcomes deferred while several stages run; see `Run::settle`.
#[derive(Debug, Clone, Default)]
pub(super) struct Hold {
    /// The first review repair requested: (review, target, feedback, reason).
    pub(super) repair: Option<(usize, usize, String, String)>,
    /// Messages of stages that stopped, by stage.
    pub(super) stops: Vec<(usize, String)>,
}

pub(super) fn is_zero(value: &u32) -> bool {
    *value == 0
}

pub(super) fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

/// An unattended attempt's session, reopened in the native interactive CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resume {
    pub session: String,
    /// The handoff path the session's original prompt named.
    pub handoff_file: PathBuf,
}

/// Where a new run works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Project,
    /// A new Git worktree from committed HEAD; refuses uncommitted changes.
    Worktree,
    /// A worktree from HEAD although the project has uncommitted changes,
    /// e.g. another run's edits in progress. Those changes are not included.
    WorktreeFromHead,
}

#[derive(Debug, Clone)]
pub enum Event {
    State(Box<Run>),
    Output(String),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Complete,
    Interactive,
    Confirm,
    Stopped,
}

/// What a finished stage's handoff and checks lead to.
pub(super) enum Decision {
    Block(String),
    /// A test stage without evidence waits for the user to accept its reason.
    AwaitWaiver,
    Repair {
        target: usize,
        feedback: String,
    },
    /// A judge chose candidate `candidate` of fan-out stage `fan`.
    Select {
        fan: usize,
        candidate: usize,
    },
    Advance,
}
