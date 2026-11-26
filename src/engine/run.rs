use super::*;

impl Run {
    pub fn create(
        project: &Path,
        settings: &Settings,
        workflow: &Workflow,
        task: String,
        worktree: bool,
    ) -> Result<Self> {
        let placement = if worktree {
            Placement::Worktree
        } else {
            Placement::Project
        };
        Self::create_at(project, settings, workflow, task, placement)
    }

    pub fn create_at(
        project: &Path,
        settings: &Settings,
        workflow: &Workflow,
        task: String,
        placement: Placement,
    ) -> Result<Self> {
        Self::create_at_from(project, settings, workflow, task, placement, None)
    }

    pub(crate) fn create_at_from(
        project: &Path,
        settings: &Settings,
        workflow: &Workflow,
        task: String,
        placement: Placement,
        source: Option<&Run>,
    ) -> Result<Self> {
        Self::create_inner(
            project, settings, workflow, task, placement, source, None, true, true,
        )
    }

    pub(crate) fn create_rerun_from(
        project: &Path,
        settings: &Settings,
        workflow: &Workflow,
        task: String,
        placement: Placement,
        source: &Run,
        rerun_of: String,
    ) -> Result<Self> {
        Self::create_inner(
            project,
            settings,
            workflow,
            task,
            placement,
            Some(source),
            Some(rerun_of),
            true,
            true,
        )
    }

    pub(crate) fn create_for_fork(
        project: &Path,
        settings: &Settings,
        workflow: &Workflow,
        task: String,
        source: &Run,
    ) -> Result<Self> {
        Self::create_inner(
            project,
            settings,
            workflow,
            task,
            Placement::WorktreeFromHead,
            Some(source),
            None,
            false,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_inner(
        project: &Path,
        settings: &Settings,
        workflow: &Workflow,
        task: String,
        placement: Placement,
        source: Option<&Run>,
        rerun_of: Option<String>,
        capture_baseline: bool,
        persist: bool,
    ) -> Result<Self> {
        crate::config::validate_task(&task)?;
        // A new run in the checkout (`run`, `rerun`, the TUI, `serve`) must
        // not join a harness another run may have left running there.
        if matches!(placement, Placement::Project) {
            refuse_unproven_in_checkout(project)?;
        }
        crate::trust::authorize_settings(project, settings)?;
        let snapshot = Settings {
            workflows: vec![workflow.clone()],
            ..settings.clone()
        };
        snapshot.validate()?;
        ensure!(
            workflow.stages.iter().any(|s| s.enabled),
            "Enable at least one stage"
        );
        // Catch every missing selected harness before any stage mutates the project.
        let candidates: Vec<Stage> = workflow
            .stages
            .iter()
            .filter(|s| s.enabled)
            .flat_map(|s| {
                s.fan_out
                    .iter()
                    .flat_map(|f| &f.candidates)
                    .map(|c| c.stage(s))
            })
            .collect();
        let is_repo = snapshot::is_repository(project);
        ensure!(
            candidates.is_empty() || is_repo,
            "Fan-out stages need a Git repository: candidates run in Git worktrees"
        );
        if is_repo {
            snapshot::ensure_supported_git(project)?;
        }
        for stage in workflow
            .stages
            .iter()
            // Ship stages run in the router, without a harness.
            .filter(|s| s.enabled && s.fan_out.is_none() && s.role != Role::Ship)
            .chain(&candidates)
        {
            let program = adapters::program(&stage.harness, &settings.adapters);
            ensure!(
                adapters::executable_at(&program, project).is_some(),
                "{} is not installed or executable: {}",
                stage.harness,
                program
            );
            if let Some(provider) =
                providers::route(&stage.harness, &stage.provider, &settings.providers)
            {
                ensure!(
                    provider.api_key_env.is_empty()
                        || std::env::var_os(&provider.api_key_env).is_some_and(|v| !v.is_empty()),
                    "Set {} to use provider {} in stage {}",
                    provider.api_key_env,
                    stage.provider,
                    stage.name
                );
            }
            if stage.mode == Mode::Interactive {
                if let Some(custom) = settings.adapters.get(&stage.harness) {
                    ensure!(
                        custom.interactive_args.is_some(),
                        "{} has no interactive_args",
                        stage.harness
                    );
                }
            }
        }
        let project = fs::canonicalize(project).context("Project directory does not exist")?;
        ensure!(project.is_dir(), "Project must be a directory");
        let id = format!(
            "{}-{}",
            Utc::now().format("%Y%m%d-%H%M%S"),
            &uuid::Uuid::new_v4().to_string()[..8]
        );
        struct RollbackCreate {
            project: PathBuf,
            id: String,
            worktree: Option<PathBuf>,
            directory: Option<PathBuf>,
            armed: bool,
        }
        impl Drop for RollbackCreate {
            fn drop(&mut self) {
                if !self.armed {
                    return;
                }
                if let Some(path) = &self.worktree {
                    storage::discard_worktree(&self.project, path);
                    let _ = storage::git(
                        &self.project,
                        &["branch", "-D", &format!("model-router/{}", self.id)],
                    );
                }
                snapshot::forget(&self.project, &self.id);
                if let Some(dir) = &self.directory {
                    let _ = fs::remove_dir_all(dir);
                }
            }
        }
        let worktree = placement != Placement::Project;
        let workspace = if worktree {
            storage::create_worktree(&project, &id, placement == Placement::Worktree)?
        } else {
            project.clone()
        };
        let mut rollback = RollbackCreate {
            project: project.clone(),
            id: id.clone(),
            worktree: worktree.then(|| workspace.clone()),
            directory: None,
            armed: true,
        };
        let directory = storage::metadata_dir(&project)?.join("runs").join(&id);
        fs::create_dir_all(&directory)?;
        rollback.directory = Some(directory.clone());
        let baseline = if capture_baseline {
            snapshot::tree(&workspace, &directory)
        } else {
            None
        };
        let now = Utc::now();
        let mut run = Self {
            version: 1,
            revision: 0,
            id,
            created_at: now,
            updated_at: now,
            project,
            workspace,
            baseline,
            directory,
            worktree,
            task,
            rerun_of,
            fork_of: None,
            workflow: workflow.clone(),
            adapters: settings.adapters.clone(),
            providers: settings.providers.clone(),
            delegates: workflow
                .stages
                .iter()
                .filter_map(|stage| stage.delegation.as_ref())
                .flat_map(|delegation| &delegation.delegates)
                .filter_map(|name| {
                    settings
                        .delegates
                        .get(name)
                        .map(|delegate| (name.clone(), delegate.clone()))
                })
                .collect(),
            launched_by: None,
            creation_source: Default::default(),
            mcp: None,
            stages: workflow
                .stages
                .iter()
                .map(|s| StageState {
                    status: if s.enabled {
                        Status::Pending
                    } else {
                        Status::Skipped
                    },
                    attempts: vec![],
                    handoff: None,
                    waiver: None,
                    fan_out: None,
                    inherited: None,
                    discarded: vec![],
                    skipped_reason: None,
                })
                .collect(),
            orphan_groups: vec![],
            retained_groups: vec![],
            cursor: 0,
            repair_passes: 0,
            extra_repair_passes: 0,
            repair_feedback: None,
            repair_review: None,
            status: Status::Pending,
            error: None,
            resume: None,
            journal: vec![],
            hold: None,
            view: None,
        };
        crate::trust::approve_run(&run, settings, source)?;
        if persist {
            run.persist()?;
        }
        rollback.armed = false;
        Ok(run)
    }

    /// Saves run.json, then the summary that history listing reads instead.
    /// The summary records run.json's revision and size, so listing can tell
    /// when the two disagree.
    pub fn persist(&mut self) -> Result<()> {
        let prev_revision = self.revision;
        let prev_updated = self.updated_at;
        self.revision += 1;
        self.updated_at = Utc::now();
        let result = (|| -> Result<()> {
            let record = serde_json::to_vec_pretty(&*self)?;
            storage::atomic_write(&self.directory.join("run.json"), &record)?;
            let summary = crate::history::RunSummary {
                run_bytes: Some(record.len() as u64),
                ..crate::history::RunSummary::from(&*self)
            };
            storage::write_json(&self.directory.join("summary.json"), &summary)?;
            for record in &mut self.journal {
                record.run_revision = self.revision;
            }
            transitions::append(&self.directory, &self.journal)
        })();
        if result.is_err() {
            self.revision = prev_revision;
            self.updated_at = prev_updated;
            return result;
        }
        crate::events::emit_run_metrics(self, prev_revision == 0, &self.journal);
        self.journal.clear();
        Ok(())
    }

    /// The only place run, stage, and attempt statuses change. Refuses moves
    /// `legal` does not allow; records the others for transitions.jsonl.
    /// Moving to the current status is a no-op and is not recorded.
    pub fn transition(&mut self, change: Transition) -> Result<()> {
        let Transition {
            target,
            to,
            actor,
            reason,
        } = change;
        let (level, stage_index, enabled, conditional) = match target {
            Target::Run => (Level::Run, None, true, false),
            Target::Stage(i)
            | Target::Attempt(i)
            | Target::Candidate(i, _)
            | Target::CandidateAttempt(i, _) => {
                let stage = self
                    .workflow
                    .stages
                    .get(i)
                    .with_context(|| format!("No stage {}", i + 1))?;
                let level = match target {
                    Target::Stage(_) => Level::Stage,
                    Target::Candidate(..) => Level::Candidate,
                    _ => Level::Attempt,
                };
                (
                    level,
                    Some(i),
                    stage.enabled,
                    !stage.when_changed.is_empty(),
                )
            }
        };
        let (slot, attempt, candidate) = match target {
            Target::Run => (&mut self.status, None, None),
            Target::Stage(i) => (&mut self.stages[i].status, None, None),
            Target::Attempt(i) => {
                let attempt = self.stages[i]
                    .attempts
                    .last_mut()
                    .with_context(|| format!("Stage {} has no attempt", i + 1))?;
                (&mut attempt.status, Some(attempt.number), None)
            }
            Target::Candidate(i, c) | Target::CandidateAttempt(i, c) => {
                let candidate = self.stages[i]
                    .fan_out
                    .as_mut()
                    .and_then(|f| f.candidates.get_mut(c))
                    .with_context(|| format!("Stage {} has no candidate {}", i + 1, c + 1))?;
                let id = Some(candidate.id.clone());
                if matches!(target, Target::Candidate(..)) {
                    (&mut candidate.status, None, id)
                } else {
                    let attempt = candidate
                        .attempts
                        .last_mut()
                        .with_context(|| format!("Candidate {} has no attempt", candidate.id))?;
                    (&mut attempt.status, Some(attempt.number), id)
                }
            }
        };
        let from = *slot;
        if from == to {
            return Ok(());
        }
        ensure!(
            legal(level, from, to, enabled, conditional),
            "Illegal {} transition{}{}: {} → {} ({reason})",
            level.label(),
            stage_index.map_or_else(String::new, |i| format!(" in stage {}", i + 1)),
            candidate
                .as_ref()
                .map_or_else(String::new, |c| format!(" candidate {c}")),
            from.as_str(),
            to.as_str()
        );
        *slot = to;
        self.journal.push(transitions::Record {
            at: Utc::now(),
            run_revision: self.revision,
            level,
            stage_index,
            attempt,
            candidate,
            from,
            to,
            reason,
            actor,
        });
        Ok(())
    }

    pub(super) fn set(
        &mut self,
        target: Target,
        to: Status,
        actor: Actor,
        reason: impl Into<String>,
    ) -> Result<()> {
        self.transition(Transition::new(target, to, actor, reason))
    }

    /// Applies `cmd` here and saves it, under the run lock; see `command`.
    /// On error this run is left as it was.
    pub fn command(&mut self, cmd: Cmd) -> Result<()> {
        *self = command(&self.project, &self.id, Some(self.revision), cmd)?;
        Ok(())
    }
    /// Groups recovery left running because it could not prove they are
    /// this run's; see `refuse_unproven`.
    pub fn retained_groups(&self) -> &[process::Group] {
        &self.retained_groups
    }
    pub fn current_name(&self) -> &str {
        self.workflow
            .stages
            .get(self.cursor)
            .map_or("Finished", |s| s.name.as_str())
    }

    pub(super) fn check_revision(&self) -> Result<()> {
        let saved: Self = storage::read_json(&self.directory.join("run.json"))?;
        ensure!(
            saved.revision == self.revision,
            "This run changed in another process; reload it before continuing"
        );
        Ok(())
    }

    /// Explicit retry may preserve filesystem effects of the previous attempt.
    /// Changes this run in memory only; `command` applies it under the lock.
    pub fn retry(&mut self, interactive: bool, model: Option<String>) -> Result<()> {
        ensure!(
            self.cursor < self.stages.len(),
            "This run is already complete"
        );
        let index = self.cursor;
        ensure!(
            !(interactive && self.workflow.stages[index].fan_out.is_some()),
            "Fan-out candidates run unattended; retry without interactive, or retry one candidate"
        );
        if self.status == Status::Running {
            self.interrupt_candidates(INTERRUPTED)?;
            if self.graph() {
                for i in 0..self.stages.len() {
                    if i == index || self.stages[i].status != Status::Running {
                        continue;
                    }
                    if self.stages[i]
                        .attempts
                        .last()
                        .is_some_and(|a| a.status == Status::Running)
                    {
                        self.set(
                            Target::Attempt(i),
                            Status::Failed,
                            Actor::Recovery,
                            INTERRUPTED,
                        )?;
                        let attempt = self.stages[i].attempts.last_mut().context("No attempt")?;
                        attempt.error = Some(INTERRUPTED.into());
                        attempt.finished_at.get_or_insert_with(Utc::now);
                    }
                    self.set(
                        Target::Stage(i),
                        Status::Failed,
                        Actor::Recovery,
                        INTERRUPTED,
                    )?;
                }
            }
        }
        if self.stages[index]
            .attempts
            .last()
            .is_some_and(|a| a.status == Status::Running)
        {
            let message = "Previous execution was interrupted; retry was explicitly requested";
            self.set(
                Target::Attempt(index),
                Status::Failed,
                Actor::Recovery,
                message,
            )?;
            let attempt = self.stages[index]
                .attempts
                .last_mut()
                .context("No attempt")?;
            attempt.finished_at = Some(Utc::now());
            attempt.error = Some(message.into());
        }
        if interactive {
            self.workflow.stages[index].mode = Mode::Interactive;
        }
        let mut reason = String::from("Retry");
        if let Some(model) = model {
            reason = format!("Retry with model {model}");
            self.workflow.stages[index].model = model;
        }
        if interactive {
            reason.push_str(" interactively");
        }
        self.reset(index, Actor::User, &reason)?;
        self.error = None;
        self.resume = None;
        self.set(Target::Run, Status::Pending, Actor::User, reason)
    }

    /// Reopens the current stage's last session in the harness's native
    /// interactive CLI: an unattended attempt, or an interactive one awaiting
    /// review. The stage must have stopped; filesystem changes are kept, as
    /// with retry. In memory only; `command` applies it under the lock.
    pub fn take_over(&mut self) -> Result<()> {
        ensure!(
            self.cursor < self.stages.len(),
            "This run is already complete"
        );
        ensure!(
            self.status.stopped() || self.status == Status::AwaitingConfirmation,
            "Stop the stage before taking it over"
        );
        let stage = &self.workflow.stages[self.cursor];
        ensure!(
            stage.fan_out.is_none(),
            "Fan-out stages have no session to take over; open a candidate's worktree, or retry one candidate"
        );
        ensure!(
            adapters::resumable(&stage.harness, &self.adapters),
            "{} sessions cannot be resumed; retry interactively with [i] instead",
            stage.harness
        );
        let attempt = self.stages[self.cursor]
            .attempts
            .last()
            .context("This stage has not run yet")?;
        let session = attempt
            .session
            .clone()
            .or_else(|| {
                let head = read_head(&attempt.directory.join("stdout.log"), 256 * 1024).ok()?;
                handoff::session_id(&head)
            })
            .context("The harness did not report a session for this attempt; retry interactively with [i] instead")?;
        // Checked before anything moves: run.json may name any session.
        ensure!(
            adapters::is_session_id(&session),
            "The recorded session {session:?} is not a harness session ID; retry interactively with [i] instead"
        );
        set_aside_handoff(&attempt.handoff_file, &attempt.directory)?;
        let reason = format!("Take over session {session}");
        self.resume = Some(Resume {
            session,
            handoff_file: attempt.handoff_file.clone(),
        });
        self.workflow.stages[self.cursor].mode = Mode::Interactive;
        self.reset(self.cursor, Actor::User, &reason)?;
        self.error = None;
        self.set(Target::Run, Status::Interactive, Actor::User, reason)
    }

    /// Allows one more review repair pass after the workflow's limit stopped
    /// the run, then sends the review's findings to the implementation stage
    /// as an automatic repair would. `model-router resume <id> --allow-repair`.
    /// In memory only; `command` applies it under the lock.
    pub fn allow_repair(&mut self) -> Result<()> {
        ensure!(
            self.repair_blocked(),
            "Only a run stopped by its repair limit can get another repair pass"
        );
        self.extra_repair_passes += 1;
        self.error = None;
        if let Some(attempt) = self.stages[self.cursor].attempts.last_mut() {
            attempt.error = None;
        }
        self.advance(Actor::User)
    }

    /// Whether the run stopped because its review still requests changes
    /// after every repair pass it allows: what `allow_repair` accepts.
    pub fn repair_blocked(&self) -> bool {
        self.status == Status::Blocked
            && self.cursor < self.stages.len()
            && self.review_failures().is_some()
            && self.repair_passes >= self.repair_limit()
    }

    /// Repair passes the run allows: the workflow's limit plus those the
    /// user added.
    pub fn repair_limit(&self) -> u32 {
        self.workflow
            .max_repair_passes
            .saturating_add(self.extra_repair_passes)
    }

    /// Return stage `from` and every stage after it (in a graph, those that
    /// depend on it) to pending, discarding handoffs, acceptances, and fan-out
    /// rounds.
    pub(super) fn reset(&mut self, from: usize, actor: Actor, reason: &str) -> Result<()> {
        let order = self.order();
        for i in from..self.stages.len() {
            if i != from && self.graph() && !order.precedes(from, i) {
                continue;
            }
            let to = if self.workflow.stages[i].enabled {
                Status::Pending
            } else {
                Status::Skipped
            };
            self.set(Target::Stage(i), to, actor, reason)?;
            self.clear_fan_out(i);
            let state = &mut self.stages[i];
            state.handoff = None;
            state.waiver = None;
            state.inherited = None;
            state.skipped_reason = None;
        }
        // A graph's dependents may come earlier in the list.
        if self.graph() {
            for i in (0..from).filter(|&i| order.precedes(from, i)) {
                let to = if self.workflow.stages[i].enabled {
                    Status::Pending
                } else {
                    Status::Skipped
                };
                self.set(Target::Stage(i), to, actor, reason)?;
                self.clear_fan_out(i);
                let state = &mut self.stages[i];
                state.handoff = None;
                state.waiver = None;
                state.inherited = None;
                state.skipped_reason = None;
            }
        }
        Ok(())
    }

    /// Evaluates stage `index`'s `when_changed` globs. Returns `Some(reason)`
    /// when the stage should be skipped in a Git repo because no files changed
    /// so far in the run match any glob. Outside Git (or if snapshots cannot be
    /// inspected), returns `None` so the stage runs normally (fail-safe).
    pub(crate) fn should_skip_when_changed(&self, index: usize) -> Option<String> {
        let stage = self.workflow.stages.get(index)?;
        if stage.when_changed.is_empty() {
            return None;
        }
        // A judge with candidates waiting always runs: their changes are in
        // their own worktrees, not the workspace, until it chooses one, and
        // skipping it would apply none.
        let judging = graph::judged_by(&self.workflow, index)
            .and_then(|fan| self.stages[fan].fan_out.as_ref())
            .is_some_and(|round| {
                !round.candidates.is_empty() && round.selection.as_ref().is_none_or(|s| !s.applied)
            });
        if judging {
            return None;
        }
        let from = self.baseline.as_deref()?;
        if !snapshot::is_repository(&self.workspace) {
            return None;
        }
        let to = match snapshot::current_tree(&self.workspace) {
            Ok(Some(tree)) => tree,
            Ok(None) => return None,
            Err(_) => self.change_range().ok()?.1.to_owned(),
        };
        let changed = if from == to {
            vec![]
        } else {
            snapshot::changed_paths(&self.workspace, from, &to).ok()?
        };
        let matched = changed.iter().any(|path| {
            stage
                .when_changed
                .iter()
                .any(|pat| crate::policy::matches_glob(pat, path))
        });
        if matched {
            None
        } else {
            Some(format!(
                "Skipped (when_changed): {} changed file(s) matched none of {}",
                changed.len(),
                stage.when_changed.join(", ")
            ))
        }
    }

    /// Enforces `workflow.review_quorum` when > 1: requires at least N distinct
    /// enabled non-judge review stages to have completed. No-op when unset or 1.
    pub fn check_review_quorum(&self) -> Result<()> {
        let required = self.workflow.review_quorum.unwrap_or(1);
        if required <= 1 {
            return Ok(());
        }
        let mut enabled_reviews = 0usize;
        let mut passed_reviews = 0usize;
        for (i, (stage, state)) in self.workflow.stages.iter().zip(&self.stages).enumerate() {
            if stage.enabled
                && stage.role == Role::Review
                && graph::judged_by(&self.workflow, i).is_none()
            {
                enabled_reviews += 1;
                if state.status == Status::Complete {
                    passed_reviews += 1;
                }
            }
        }
        ensure!(
            passed_reviews >= required,
            "Review quorum not met: {passed_reviews} of {required} distinct review stage(s) passed ({enabled_reviews} enabled). Enable and pass at least {required} distinct review stages"
        );
        Ok(())
    }

    pub(super) fn clear_fan_out(&mut self, stage: usize) {
        if let Some(round) = self.stages[stage].fan_out.take() {
            for mut candidate in round.candidates {
                self.orphan_groups.extend(
                    candidate
                        .attempts
                        .iter_mut()
                        .filter_map(Attempt::take_group),
                );
                // Its spend still counts.
                self.stages[stage].discarded.push(candidate);
            }
        }
    }

    /// Whether the workflow is a dependency graph; see `graph`.
    pub fn graph(&self) -> bool {
        graph::is_graph(&self.workflow)
    }
    /// Which stages run before which.
    pub fn order(&self) -> graph::Ancestry {
        graph::order(&self.workflow)
    }
    /// The first stage not yet complete or disabled; the stage commands act on.
    pub(super) fn focus(&self) -> usize {
        self.stages
            .iter()
            .position(|s| !matches!(s.status, Status::Complete | Status::Skipped))
            .unwrap_or(self.stages.len())
    }
    /// Sets the run's status, unless stages run side by side: then stops are
    /// collected and `settle` decides once they all end.
    pub(super) fn set_run(
        &mut self,
        to: Status,
        actor: Actor,
        reason: impl Into<String>,
    ) -> Result<()> {
        let reason = reason.into();
        if let Some(hold) = &mut self.hold {
            if to.stopped() {
                hold.stops.push((self.cursor, reason));
            }
            return Ok(());
        }
        self.set(Target::Run, to, actor, reason)
    }

    /// Rerun one stopped candidate of the stopped fan-out stage `stage`; the
    /// other candidates keep their results. In memory only; `command`
    /// applies it under the lock.
    pub fn retry_candidate(&mut self, stage: &str, candidate: &str) -> Result<()> {
        let index = self
            .workflow
            .stages
            .iter()
            .position(|s| s.id == stage)
            .with_context(|| format!("No stage {stage}"))?;
        ensure!(
            self.status.stopped() && self.stages[index].status.stopped(),
            "Only a stopped fan-out stage of a stopped run can retry a candidate"
        );
        ensure!(
            self.graph() || self.cursor == index,
            "The run stopped at another stage"
        );
        let c = self.stages[index]
            .fan_out
            .as_ref()
            .and_then(|f| f.candidates.iter().position(|c| c.id == candidate))
            .with_context(|| format!("Stage {stage} has no candidate {candidate}"))?;
        let (applied, cand_status) = {
            let fan = self.stages[index].fan_out.as_ref().context("No fan-out")?;
            (
                fan.selection.as_ref().is_some_and(|s| s.applied),
                fan.candidates[c].status,
            )
        };
        ensure!(
            !applied,
            "A candidate was already applied; retry the fan-out stage to start a new round"
        );
        ensure!(
            cand_status.stopped(),
            "Candidate {candidate} is {}; only a stopped candidate can be retried",
            cand_status.label()
        );
        if let Some(fan) = self.stages[index].fan_out.as_mut() {
            if fan.selection.as_ref().is_some_and(|s| !s.applied) {
                fan.selection = None;
            }
        }
        let reason = format!("Retry candidate {candidate}");
        self.set(
            Target::Candidate(index, c),
            Status::Pending,
            Actor::User,
            &reason,
        )?;
        self.set(Target::Stage(index), Status::Pending, Actor::User, &reason)?;
        self.stages[index].handoff = None;
        self.cursor = index;
        self.error = None;
        self.resume = None;
        self.set(Target::Run, Status::Pending, Actor::User, reason)
    }

    /// Confirming a checkpoint also accepts its pending waiver; it never needs
    /// a second step. In memory only; `command` applies it under the lock.
    pub fn confirm(&mut self) -> Result<()> {
        ensure!(
            self.status == Status::AwaitingConfirmation && self.cursor < self.stages.len(),
            "No handoff awaits confirmation"
        );
        if self.workflow.stages[self.cursor].role == Role::Ship {
            crate::trust::authorize_run(self)?;
            return crate::ship::confirm(self);
        }
        if let Some(reason) = self.pending_waiver().map(str::to_owned) {
            self.stages[self.cursor].waiver = Some(Waiver {
                reason,
                accepted_at: Utc::now(),
            });
        }
        self.advance(Actor::User)
    }

    /// Replaces the summary of the non-review handoff awaiting confirmation.
    pub(super) fn edit_summary(&mut self, summary: String) -> Result<()> {
        ensure!(
            self.status == Status::AwaitingConfirmation
                && self
                    .workflow
                    .stages
                    .get(self.cursor)
                    .is_some_and(|s| s.role != Role::Review),
            "Only an interactive non-review handoff summary can be edited"
        );
        ensure!(
            !summary.trim().is_empty() && summary.len() <= 12000,
            "Enter a summary of 1–12000 bytes"
        );
        let state = &mut self.stages[self.cursor];
        let handoff = state.handoff.as_mut().context("No handoff")?;
        handoff.summary = summary;
        handoff
            .decisions
            .push("Handoff summary supplied or edited by the user".into());
        if let Some(attempt) = state.attempts.last() {
            storage::write_json(&attempt.directory.join("handoff.json"), handoff)?;
        }
        Ok(())
    }

    /// The not-applicable reason when this checkpoint awaits acceptance of a test
    /// stage without verification evidence.
    pub fn pending_waiver(&self) -> Option<&str> {
        if self.status != Status::AwaitingConfirmation
            || !matches!(self.decide(), Ok(Decision::AwaitWaiver))
        {
            return None;
        }
        self.stages
            .get(self.cursor)?
            .handoff
            .as_ref()?
            .not_applicable_reason()
    }

    /// Trees bounding the changes made during this run: the baseline and the
    /// latest attempt's final snapshot, so earlier user changes are excluded.
    pub fn change_range(&self) -> std::result::Result<(&str, &str), &'static str> {
        let from = self
            .baseline
            .as_deref()
            .ok_or("no baseline snapshot was recorded")?;
        // The last to finish: stages side by side can end out of order.
        let latest = self
            .stages
            .iter()
            .flat_map(|s| &s.attempts)
            .max_by_key(|a| (a.finished_at.unwrap_or(a.started_at), a.started_at));
        let to = match latest {
            Some(attempt) => attempt
                .tree_after
                .as_deref()
                .ok_or("the latest attempt has no snapshot")?,
            None => from,
        };
        Ok((from, to))
    }

    /// The current stage's failed required router checks, described.
    pub(super) fn failed_checks(&self) -> Option<String> {
        let checks = self.stages[self.cursor]
            .attempts
            .last()
            .map_or(&[][..], |a| &a.checks);
        let mut failed = checks
            .iter()
            .filter(|c| c.required && c.status != TestStatus::Passed);
        let check = failed.next()?;
        let more = failed.count();
        Some(format!(
            "Required check failed: {} ({}){}. Logs: {}",
            shell_words::join(&check.command),
            check.outcome(),
            if more > 0 {
                format!("; {more} more failed")
            } else {
                String::new()
            },
            check.directory.display()
        ))
    }

    /// Why the current review stage requests a repair: its handoff, its failed
    /// required router checks, or both. None for other stages, a review that
    /// passed, or one that reports a block.
    pub(super) fn review_failures(&self) -> Option<Option<String>> {
        let stage = self.workflow.stages.get(self.cursor)?;
        let handoff = self.stages[self.cursor].handoff.as_ref()?;
        if stage.role != Role::Review || !stage.enabled || handoff.verdict == Verdict::Blocked {
            return None;
        }
        let checks = self.failed_checks();
        (handoff.requires_repair() || checks.is_some()).then_some(checks)
    }

    /// What the current stage's handoff and checks lead to.
    pub(super) fn decide(&self) -> Result<Decision> {
        let stage = self
            .workflow
            .stages
            .get(self.cursor)
            .context("This run is already complete")?;
        let state = &self.stages[self.cursor];
        let handoff = state.handoff.as_ref().context("Missing handoff")?;
        if handoff.verdict == Verdict::Blocked {
            return Ok(Decision::Block(format!(
                "Stage reports a block: {}",
                handoff.summary
            )));
        }
        // A judge chooses; it never requests repairs.
        if let Some(fan) = graph::judged_by(&self.workflow, self.cursor) {
            return Ok(self.judgement(fan, handoff));
        }
        // A failed required check in an enabled review requests a repair, as
        // its blocking findings do; anywhere else it blocks the stage.
        if let Some(checks) = self.review_failures() {
            let mut feedback = format!(
                "Review {} requested changes:\n{}",
                stage.name,
                serde_json::to_string_pretty(handoff)?
            );
            if let Some(checks) = &checks {
                feedback.push_str(&format!("\nRouter checks: {checks}"));
            }
            let note = checks.map_or_else(String::new, |c| format!(" {c}"));
            if self.repair_passes >= self.repair_limit() {
                return Ok(Decision::Block(format!(
                    "Review still requests changes after {} repair passes.{note} Inspect its findings, then allow another pass with: model-router -C {} resume {} --allow-repair",
                    self.repair_passes,
                    self.project.display(),
                    self.id
                )));
            }
            let order = self.order();
            let target = (0..self.stages.len()).rev().find(|&i| {
                let s = &self.workflow.stages[i];
                order.precedes(i, self.cursor)
                    && s.enabled
                    && s.role == Role::Implementation
                    && (stage.repair_target.is_empty() || s.id == stage.repair_target)
            });
            return Ok(match target {
                Some(target) => Decision::Repair { target, feedback },
                None => Decision::Block(format!(
                    "Review requested changes, but no enabled upstream implementation stage exists.{note}"
                )),
            });
        }
        if let Some(checks) = self.failed_checks() {
            return Ok(Decision::Block(checks));
        }
        if stage.role == Role::Test {
            let checks = state.attempts.last().map_or(&[][..], |a| &a.checks);
            if handoff.requires_repair()
                || handoff
                    .tests
                    .iter()
                    .any(|test| test.required && test.status == TestStatus::NotRun)
            {
                return Ok(Decision::Block("Test stage reports incomplete verification; inspect its findings and required checks before retrying".into()));
            }
            let router_pass = checks
                .iter()
                .any(|c| c.required && c.status == TestStatus::Passed);
            // `require_router_checks`: the model's own report is not evidence.
            let model_pass = !stage.require_router_checks && handoff.has_passing_required_check();
            if router_pass || model_pass || state.waiver.is_some() {
                return Ok(Decision::Advance);
            }
            return Ok(match handoff.not_applicable_reason() {
                Some(_) => Decision::AwaitWaiver,
                None if stage.require_router_checks => Decision::Block(format!(
                    "Test stage requires a passing required router check (require_router_checks); {}. Add or fix the stage's required checks and retry, or have it report not_applicable with a reason, which requires your explicit acceptance",
                    if handoff.has_passing_required_check() {
                        "model-reported passes do not count"
                    } else {
                        "none passed"
                    }
                )),
                None => Decision::Block("Test stage reported no passing required check. Retry it with evidence, or have it report not_applicable with a reason, which requires your explicit acceptance".into()),
            });
        }
        if handoff.verdict == Verdict::NeedsChanges {
            return Ok(Decision::Block(
                "Stage reports incomplete work; inspect its handoff and retry".into(),
            ));
        }
        Ok(Decision::Advance)
    }

    /// Acts on the current stage's finished handoff: blocks, waits for the
    /// user's acceptance, sends a review's request to a repair, or moves on.
    pub(crate) fn advance(&mut self, actor: Actor) -> Result<()> {
        let index = self.cursor;
        let name = self
            .workflow
            .stages
            .get(index)
            .context("This run is already complete")?
            .name
            .clone();
        match self.decide()? {
            Decision::Block(message) => self.stop(Status::Blocked, message, actor, true),
            Decision::AwaitWaiver => {
                let reason = "Test stage reports not applicable; awaiting acceptance";
                self.set(Target::Attempt(index), Status::Complete, actor, reason)?;
                self.set(
                    Target::Stage(index),
                    Status::AwaitingConfirmation,
                    actor,
                    reason,
                )?;
                self.set_run(Status::AwaitingConfirmation, actor, reason)
            }
            Decision::Select { fan, candidate } => {
                let by = self.workflow.stages[index].id.clone();
                let summary = self.stages[index]
                    .handoff
                    .as_ref()
                    .map(|h| h.summary.clone())
                    .unwrap_or_default();
                match self.select(fan, candidate, &by, &summary)? {
                    Ok(()) => {
                        if let Some(tree) = snapshot::tree(&self.workspace, &self.directory) {
                            if let Some(attempt) = self.stages[index].attempts.last_mut() {
                                attempt.tree_after = Some(tree);
                            }
                        }
                        self.complete(index, actor)
                    }
                    Err(message) => self.stop(Status::Blocked, message, actor, true),
                }
            }
            Decision::Repair { target, feedback } => {
                let reason = format!(
                    "Review {name} requested a repair at {}",
                    self.workflow.stages[target].name
                );
                self.set(Target::Attempt(index), Status::Complete, actor, &reason)?;
                // Stages still running see it once they end; see `settle`.
                if let Some(hold) = &mut self.hold {
                    hold.repair.get_or_insert((index, target, feedback, reason));
                    return Ok(());
                }
                self.repair_feedback = Some(feedback);
                self.repair_review = Some(index);
                self.repair_passes += 1;
                // Invalidate all downstream handoffs, including previously passing reviews.
                self.reset(target, actor, &reason)?;
                self.cursor = target;
                self.set(Target::Run, Status::Pending, actor, reason)
            }
            Decision::Advance => self.complete(index, actor),
        }
    }

    /// Completes stage `index` and moves on.
    pub(super) fn complete(&mut self, index: usize, actor: Actor) -> Result<()> {
        let reason = format!("Stage {} complete", self.workflow.stages[index].name);
        self.set(Target::Attempt(index), Status::Complete, actor, &reason)?;
        self.set(Target::Stage(index), Status::Complete, actor, &reason)?;
        // The review that asked for the repair passed: later stages, and
        // later retries, must not see its old request.
        if self.repair_review == Some(index) {
            self.repair_feedback = None;
            self.repair_review = None;
        }
        self.cursor = if self.graph() {
            self.focus()
        } else {
            index + 1
        };
        self.set_run(Status::Pending, actor, reason)
    }

    /// An interactive stage's handoff waits for the user at the checkpoint.
    pub(super) fn checkpoint(&mut self) -> Result<()> {
        let index = self.cursor;
        let reason = "Interactive handoff awaits inspection";
        self.set(
            Target::Attempt(index),
            Status::Complete,
            Actor::Engine,
            reason,
        )?;
        self.set(
            Target::Stage(index),
            Status::AwaitingConfirmation,
            Actor::Engine,
            reason,
        )?;
        self.set(
            Target::Run,
            Status::AwaitingConfirmation,
            Actor::Engine,
            reason,
        )
    }

    /// Stops the run at the current stage. With `attempt`, also records the
    /// error on the stage's latest attempt, which this execution created or
    /// just finished; without, the error belongs to no attempt of this
    /// execution, e.g. a launch that failed before its attempt was recorded,
    /// or a cancel between stages.
    pub(crate) fn stop(
        &mut self,
        status: Status,
        message: String,
        actor: Actor,
        attempt: bool,
    ) -> Result<()> {
        let index = self.cursor;
        let has_attempt = attempt
            && self
                .stages
                .get(index)
                .is_some_and(|s| !s.attempts.is_empty());
        if has_attempt {
            self.set(Target::Attempt(index), status, actor, &message)?;
            let attempt = self.stages[index]
                .attempts
                .last_mut()
                .context("No attempt")?;
            attempt.error = Some(message.clone());
            attempt.finished_at.get_or_insert_with(Utc::now);
        }
        if index < self.stages.len() {
            self.set(Target::Stage(index), status, actor, &message)?;
        }
        self.set_run(status, actor, message.clone())?;
        self.error = Some(message);
        Ok(())
    }
}

/// Best-of-N stages: rounds, candidates, judging, and applying the winner.
impl Run {
    /// Marks every running fan-out candidate failed: its process died with
    /// an earlier router. Never retried silently.
    pub(super) fn interrupt_candidates(&mut self, message: &str) -> Result<()> {
        self.interrupt_tasks(message);
        for index in 0..self.stages.len() {
            let count = self.stages[index]
                .fan_out
                .as_ref()
                .map_or(0, |f| f.candidates.len());
            for c in 0..count {
                if self.candidate(index, c).status == Status::Running {
                    self.stop_candidate(index, c, Status::Failed, message.into(), Actor::Recovery)?;
                }
            }
        }
        Ok(())
    }

    /// Marks every live delegated task, and its running turn, interrupted:
    /// recovery does not replay delegation; the user retries the stage.
    pub(super) fn interrupt_tasks(&mut self, message: &str) {
        use crate::delegation::records::TaskStatus;
        super::delegate_apply::settle_interrupted_applies(self);
        let now = Utc::now();
        for state in &mut self.stages {
            for attempt in &mut state.attempts {
                for task in &mut attempt.tasks {
                    if !task.status.is_live() {
                        continue;
                    }
                    task.status = TaskStatus::Interrupted;
                    task.error = Some(message.into());
                    for turn in task.turns.iter_mut().filter(|t| t.status.is_live()) {
                        turn.status = TaskStatus::Interrupted;
                        turn.finished_at.get_or_insert(now);
                        turn.error = Some(message.into());
                    }
                }
            }
        }
    }

    pub(super) fn candidate(&self, index: usize, c: usize) -> &CandidateState {
        &self.stages[index]
            .fan_out
            .as_ref()
            .expect("A fan-out stage has candidates")
            .candidates[c]
    }
    pub(super) fn candidate_mut(&mut self, index: usize, c: usize) -> &mut CandidateState {
        &mut self.stages[index]
            .fan_out
            .as_mut()
            .expect("A fan-out stage has candidates")
            .candidates[c]
    }

    /// Ends candidate `c` with `status`, and its running attempt.
    pub(super) fn stop_candidate(
        &mut self,
        index: usize,
        c: usize,
        status: Status,
        message: String,
        actor: Actor,
    ) -> Result<()> {
        if self
            .candidate(index, c)
            .attempts
            .last()
            .is_some_and(|a| a.status == Status::Running)
        {
            self.set(Target::CandidateAttempt(index, c), status, actor, &message)?;
            let attempt = self
                .candidate_mut(index, c)
                .attempts
                .last_mut()
                .context("No attempt")?;
            attempt.error = Some(message.clone());
            attempt.finished_at.get_or_insert_with(Utc::now);
            attempt.set_group(None);
        }
        self.set(Target::Candidate(index, c), status, actor, &message)?;
        self.candidate_mut(index, c).error = Some(message);
        Ok(())
    }

    /// Starts a round of fan-out stage `index`: a new attempt for the stage,
    /// and, unless an earlier round left candidates to retry, a base commit
    /// of the workspace and pending candidates.
    /// Re-snapshots fan-out stage `index`'s candidate base after its
    /// `before_stage` hooks, so candidates start from the files those hooks
    /// prepared. Only a round whose candidates have not started yet moves:
    /// started candidates' patches are relative to the base they saw. The
    /// stage attempt keeps its pre-hook `tree_before`, as ordinary stages do.
    pub(super) fn refresh_fan_out_base(&mut self, index: usize) -> Result<()> {
        let Some(state) = self.stages[index].fan_out.as_ref() else {
            return Ok(());
        };
        if state.candidates.iter().any(|c| !c.attempts.is_empty()) {
            return Ok(());
        }
        let base = snapshot::try_tree(&self.workspace, &self.directory)
            .context("Snapshot the workspace after before_stage hooks")?
            .context("Snapshot the workspace after before_stage hooks")?;
        if base == state.base {
            return Ok(());
        }
        let stage_id = self.workflow.stages[index].id.clone();
        let reference = snapshot::base_ref(&self.id, &format!("{stage_id}-{:03}", state.round));
        let base_commit = fanout::base_commit(&self.workspace, &base, &reference)?;
        if let Some(state) = self.stages[index].fan_out.as_mut() {
            state.base = base;
            state.base_commit = base_commit;
        }
        self.persist()
    }

    pub(super) fn start_fan_out(&mut self, index: usize) -> Result<()> {
        let stage = self.workflow.stages[index].clone();
        let fan = stage.fan_out.clone().context("Not a fan-out stage")?;
        let number = self.stages[index].attempts.len() + 1;
        let directory = self
            .directory
            .join(format!("{:02}-{}", index + 1, stage.id))
            .join(format!("attempt-{number:03}"));
        fs::create_dir_all(&directory)?;
        if self.stages[index].fan_out.is_none() {
            let base = snapshot::try_tree(&self.workspace, &self.directory)
                .context("Fan-out needs a Git snapshot of the workspace")?
                .context("Fan-out needs a Git snapshot of the workspace")?;
            let reference = snapshot::base_ref(&self.id, &format!("{}-{number:03}", stage.id));
            let base_commit = fanout::base_commit(&self.workspace, &base, &reference)?;
            let candidates = fan
                .candidates
                .iter()
                .zip(fan.ids())
                .map(|(c, id)| CandidateState {
                    id,
                    harness: c.harness.clone(),
                    model: c.model.clone(),
                    provider: c.provider.clone(),
                    effort: c.effort.clone(),
                    status: Status::Pending,
                    attempts: vec![],
                    handoff: None,
                    changes: None,
                    error: None,
                })
                .collect();
            self.stages[index].fan_out = Some(FanOutState {
                base,
                base_commit,
                round: number,
                candidates,
                selection: None,
            });
        }
        let state = self.stages[index].fan_out.as_ref().context("No fan-out")?;
        let pending = state
            .candidates
            .iter()
            .filter(|c| c.status == Status::Pending)
            .count();
        let base = state.base.clone();
        self.stages[index].attempts.push(Attempt {
            number,
            started_at: Utc::now(),
            finished_at: None,
            model: format!("{} candidates", fan.candidates.len()),
            mode: Mode::Unattended,
            status: Status::Pending,
            exit_code: None,
            directory: directory.clone(),
            handoff_file: directory.join("handoff.json"),
            error: None,
            checks: vec![],
            tree_before: Some(base),
            tree_after: None,
            checks_tree: None,
            ignored_protected_before: None,
            usage: None,
            session: None,
            process_group: None,
            process_identity: Default::default(),
            terminal: None,
            handoff_source: None,
            worktree: None,
            tasks: vec![],
            turns: vec![],
            launched_runs: vec![],
        });
        let reason = format!("Fan-out round {number} started: {pending} candidate(s)");
        self.resume = None;
        self.error = None;
        self.set(
            Target::Attempt(index),
            Status::Running,
            Actor::Engine,
            &reason,
        )?;
        self.set(
            Target::Stage(index),
            Status::Running,
            Actor::Engine,
            &reason,
        )?;
        self.set(Target::Run, Status::Running, Actor::Engine, reason)?;
        self.persist()
    }

    /// Prepares candidate `c` of fan-out stage `index` in a new worktree from
    /// the round's base, with the stage's prompt as that candidate sees it.
    pub(super) fn launch_candidate(
        &mut self,
        index: usize,
        c: usize,
    ) -> Result<(Prepared, PathBuf)> {
        let stage = self.workflow.stages[index].clone();
        let config = stage
            .fan_out
            .as_ref()
            .and_then(|f| f.candidates.get(c))
            .context("No such candidate")?
            .clone();
        let candidate = config.stage(&stage);
        let state = self.stages[index].fan_out.as_ref().context("No fan-out")?;
        let (base, base_commit, round) =
            (state.base.clone(), state.base_commit.clone(), state.round);
        let id = state.candidates[c].id.clone();
        let number = state.candidates[c].attempts.len() + 1;
        let stage_dir = self
            .directory
            .join(format!("{:02}-{}", index + 1, stage.id));
        let directory = if round <= 1 {
            stage_dir
                .join("candidates")
                .join(&id)
                .join(format!("attempt-{number:03}"))
        } else {
            stage_dir
                .join(format!("round-{round:03}"))
                .join("candidates")
                .join(&id)
                .join(format!("attempt-{number:03}"))
        };
        fs::create_dir_all(&directory)?;
        let leaf = if round <= 1 {
            format!("{id}-{number:03}")
        } else {
            format!("r{round:03}-{id}-{number:03}")
        };
        let worktree = fanout::candidates_dir(&self.project, &self.id)?
            .join(&stage.id)
            .join(&leaf);
        struct RollbackWorktree {
            project: PathBuf,
            worktree: PathBuf,
            armed: bool,
        }
        impl Drop for RollbackWorktree {
            fn drop(&mut self) {
                if self.armed {
                    let _ = fanout::remove_worktrees(&self.project, &self.worktree);
                }
            }
        }
        fanout::add_worktree(&self.workspace, &worktree, &base_commit)?;
        let mut rollback = RollbackWorktree {
            project: self.project.clone(),
            worktree: worktree.clone(),
            armed: true,
        };
        // A project inside a larger repository works in the same subdirectory.
        let cwd = worktree.join(fanout::prefix(&self.workspace));
        let handoff_file = storage::metadata_dir(&cwd)?
            .join("handoffs")
            .join(&self.id)
            .join(format!("{}-{leaf}.json", stage.id));
        fs::create_dir_all(handoff_file.parent().context("No handoff directory")?)?;
        ensure!(
            fs::symlink_metadata(&handoff_file).is_err(),
            "Handoff path already exists: {}",
            handoff_file.display()
        );
        // The prompt names this copy of the handoff schema.
        let schema_file = directory.join(handoff::SCHEMA_FILE);
        storage::atomic_write_fast(&schema_file, handoff::SCHEMA.as_bytes())?;
        let text = prompt_for(
            self,
            PromptTarget {
                cursor: index,
                stage: &candidate,
                workspace: &cwd,
                view: Some(c),
            },
            &handoff_file,
            &schema_file,
            false,
        )
        .0;
        let prompt_file = directory.join("prompt.md");
        storage::atomic_write_fast(&prompt_file, text.as_bytes())?;
        let paths = ContextPaths {
            workspace: &cwd,
            attempt_dir: &directory,
            prompt_file: &prompt_file,
            handoff_file: &handoff_file,
            mcp: None,
        };
        let mut invocation =
            adapters::build(&candidate, &self.adapters, &self.providers, &text, &paths)?;
        let session = adapters::assign_session(&mut invocation, &candidate.harness);
        let sandbox = crate::sandbox::plan(
            &self.workflow,
            &candidate,
            &crate::sandbox::Paths {
                workspace: &cwd,
                attempt_dir: &directory,
                handoff_dir: handoff_file.parent().context("No handoff directory")?,
                program: &invocation.program,
            },
        )?;
        let harness_version = adapters::detected_version(&candidate.harness, &invocation.program);
        storage::write_json_fast(
            &directory.join("command.json"),
            &serde_json::json!({ "program": invocation.program, "harness_version": harness_version, "args": invocation.args, "stdin_from": invocation.stdin.as_ref().map(|_| "prompt.md"), "env": invocation.env.iter().map(|(name, _)| name).collect::<Vec<_>>(), "workspace": cwd, "model": candidate.model, "provider": candidate.provider, "effort": candidate.effort, "mode": candidate.mode, "candidate": id, "sandbox": sandbox }),
        )?;
        let state = self.candidate_mut(index, c);
        state.harness = config.harness.clone();
        state.model = config.model.clone();
        state.provider = config.provider.clone();
        state.effort = config.effort.clone();
        state.error = None;
        state.handoff = None;
        state.changes = None;
        state.attempts.push(Attempt {
            number,
            started_at: Utc::now(),
            finished_at: None,
            model: candidate.model.clone(),
            mode: Mode::Unattended,
            status: Status::Pending,
            exit_code: None,
            directory: directory.clone(),
            handoff_file: handoff_file.clone(),
            error: None,
            checks: vec![],
            tree_before: Some(base),
            tree_after: None,
            checks_tree: None,
            ignored_protected_before: None,
            usage: None,
            session,
            process_group: None,
            process_identity: Default::default(),
            terminal: None,
            handoff_source: None,
            worktree: Some(worktree),
            tasks: vec![],
            turns: vec![],
            launched_runs: vec![],
        });
        let reason = format!(
            "Candidate {id} attempt {number} started: {} {}",
            candidate.harness, candidate.model
        );
        self.set(
            Target::CandidateAttempt(index, c),
            Status::Running,
            Actor::Engine,
            &reason,
        )?;
        self.set(
            Target::Candidate(index, c),
            Status::Running,
            Actor::Engine,
            reason,
        )?;
        self.persist()?;
        rollback.armed = false;
        Ok((
            Prepared {
                invocation,
                sandbox,
                directory,
                handoff_file,
                reattached: false,
                retry_from: None,
            },
            cwd,
        ))
    }

    /// Applies the results produced in a candidate worker thread onto candidate
    /// `c`'s state. Returns a surviving process group ID kept in `retained_groups`.
    pub(super) fn apply_candidate_post(
        &mut self,
        (index, c): (usize, usize),
        post: Result<CandidatePost>,
        ended: DateTime<Utc>,
    ) -> Result<Option<u32>> {
        if let Some(attempt) = self.candidate_mut(index, c).attempts.last_mut() {
            attempt.set_group(None);
            attempt.finished_at.get_or_insert(ended);
        }
        let post = match post {
            Ok(post) => post,
            Err(error) => {
                self.stop_candidate(
                    index,
                    c,
                    Status::Failed,
                    format!("Could not complete candidate bookkeeping: {error:#}"),
                    Actor::Engine,
                )?;
                return Ok(None);
            }
        };
        let mut survivor_pgid = None;
        if let Some(group) = post.survivor {
            survivor_pgid = Some(group.pgid);
            if self.retained_groups.iter().all(|g| g.pgid != group.pgid) {
                self.retained_groups.push(group);
            }
        }
        let state = self.candidate_mut(index, c);
        state.handoff = post.handoff;
        state.changes = post.changes;
        if let Some(attempt) = state.attempts.last_mut() {
            attempt.exit_code = post.exit_code;
            attempt.usage = post.usage;
            attempt.handoff_source = post.handoff_source;
            attempt.checks_tree = post.checks_tree;
            attempt.checks = post.checks;
            attempt.tree_after = post.tree_after;
        }
        if let Some((status, message)) = post.stop {
            self.stop_candidate(index, c, status, message, Actor::Engine)?;
            return Ok(survivor_pgid);
        }
        let reason = format!("Candidate {} complete", self.candidate(index, c).id);
        self.set(
            Target::CandidateAttempt(index, c),
            Status::Complete,
            Actor::Engine,
            &reason,
        )?;
        self.set(
            Target::Candidate(index, c),
            Status::Complete,
            Actor::Engine,
            reason,
        )?;
        Ok(survivor_pgid)
    }

    /// Whether no candidate of stage `index` waits or runs.
    pub(super) fn fan_out_done(&self, index: usize) -> bool {
        self.stages[index].fan_out.as_ref().is_some_and(|f| {
            f.candidates
                .iter()
                .all(|c| !matches!(c.status, Status::Pending | Status::Running))
        })
    }

    /// Ends the round of fan-out stage `index` once its candidates have:
    /// stops when none succeeded or the run was cancelled, waits for the
    /// judge stage, or picks and applies a winner automatically.
    pub(super) fn settle_fan_out(&mut self, index: usize, cancelled: bool) -> Result<()> {
        self.cursor = index;
        let stage = self.workflow.stages[index].clone();
        // A live budget monitor stopped a candidate: stop here while a cap is
        // still exceeded.
        let budget_stop = self.stages[index]
            .fan_out
            .as_ref()
            .is_some_and(|f| {
                f.candidates.iter().any(|c| {
                    c.error
                        .as_deref()
                        .is_some_and(|e| e.starts_with(crate::cost::EXCEEDED))
                })
            })
            .then(|| crate::cost::check_budget(self))
            .flatten();
        let state = self.stages[index].fan_out.as_ref().context("No fan-out")?;
        let complete = state
            .candidates
            .iter()
            .filter(|c| c.status == Status::Complete)
            .count();
        let total = state.candidates.len();
        let failures: Vec<String> = state
            .candidates
            .iter()
            .filter(|c| c.status != Status::Complete)
            .map(|c| {
                format!(
                    "{} {}{}",
                    c.id,
                    c.status.label(),
                    c.error
                        .as_deref()
                        .map(|e| format!(" ({})", e.lines().next().unwrap_or("")))
                        .unwrap_or_default()
                )
            })
            .collect();
        let auto = state.selection.as_ref().is_none_or(|s| !s.applied)
            && graph::judge_of(&self.workflow, index).is_none();
        let finish = |run: &mut Run| {
            let tree = snapshot::tree(&run.workspace, &run.directory);
            if let Some(attempt) = run.stages[index].attempts.last_mut() {
                attempt.finished_at.get_or_insert_with(Utc::now);
                attempt.tree_after = tree;
            }
        };
        if cancelled
            || state
                .candidates
                .iter()
                .any(|c| c.status == Status::Cancelled)
                && complete == 0
        {
            let message = format!(
                "Fan-out stopped before every candidate finished ({}). Worktrees are retained; retry a candidate (resume --retry-candidate {}/<id>) or the stage",
                failures.join("; "),
                stage.id
            );
            self.stop(Status::Cancelled, message, Actor::Engine, true)?;
            finish(self);
            return Ok(());
        }
        if let Some(reason) = budget_stop {
            // No winner is applied past a cap; `resume --budget-*` raises it
            // and settles again with the candidates that finished.
            self.stop(Status::Blocked, reason, Actor::Engine, true)?;
            finish(self);
            return Ok(());
        }
        if complete == 0 {
            let message = format!(
                "No fan-out candidate succeeded: {}. Retry a candidate (resume --retry-candidate {}/<id>) or the stage",
                failures.join("; "),
                stage.id
            );
            self.stop(Status::Blocked, message, Actor::Engine, true)?;
            finish(self);
            return Ok(());
        }
        if !auto {
            let judge = graph::judge_of(&self.workflow, index)
                .map_or_else(String::new, |j| self.workflow.stages[j].name.clone());
            let handoff: Handoff = serde_json::from_value(serde_json::json!({
                "summary": format!("{complete} of {total} fan-out candidates finished; the judge stage {judge:?} chooses one, then the router applies its changes to the workspace."),
                "verdict": "complete",
                "unresolved": failures,
            }))?;
            if let Some(attempt) = self.stages[index].attempts.last() {
                storage::write_json(&attempt.directory.join("handoff.json"), &handoff)?;
            }
            self.stages[index].handoff = Some(handoff);
            finish(self);
            return self.advance(Actor::Engine);
        }
        let state = self.stages[index].fan_out.as_ref().context("No fan-out")?;
        let winner = auto_pick(state).context("No complete candidate")?;
        let chosen = &state.candidates[winner];
        let reason = format!(
            "auto: {} required checks, {} changed lines, among {complete} complete candidates",
            if chosen.checks_pass() {
                "passing"
            } else {
                "failing"
            },
            chosen.changes.as_ref().map_or(0, CandidateChanges::lines)
        );
        let outcome = self.select(index, winner, crate::config::AUTO_JUDGE, &reason)?;
        finish(self);
        match outcome {
            Ok(()) => self.advance(Actor::Engine),
            Err(message) => self.stop(Status::Blocked, message, Actor::Engine, true),
        }
    }

    /// Why a judge's handoff chooses, or fails to choose, a candidate of
    /// fan-out stage `fan`.
    pub(super) fn judgement(&self, fan: usize, handoff: &Handoff) -> Decision {
        let Some(state) = self.stages[fan].fan_out.as_ref() else {
            return Decision::Block(format!(
                "Fan-out stage {} has no candidates to judge",
                self.workflow.stages[fan].name
            ));
        };
        let ids = state
            .candidates
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let Some(selected) = handoff
            .selected
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Decision::Block(format!(
                "The judge chose no candidate: its handoff needs \"selected\" set to one of {ids}. Retry it"
            ));
        };
        match state.candidates.iter().position(|c| c.id == selected) {
            Some(c) if state.candidates[c].status == Status::Complete => {
                Decision::Select { fan, candidate: c }
            }
            Some(c) => Decision::Block(format!(
                "The judge chose candidate {selected}, which is {}. Retry the judge",
                state.candidates[c].status.label()
            )),
            None => Decision::Block(format!(
                "The judge chose unknown candidate {selected}; candidates: {ids}. Retry it"
            )),
        }
    }

    /// Applies candidate `c` of fan-out stage `fan` to the workspace and
    /// records the choice; the inner error explains a conflict, which leaves
    /// the workspace unchanged.
    pub(super) fn select(
        &mut self,
        fan: usize,
        c: usize,
        by: &str,
        reason: &str,
    ) -> Result<std::result::Result<(), String>> {
        let state = self.stages[fan].fan_out.as_ref().context("No fan-out")?;
        if let Some(done) = state.selection.as_ref().filter(|s| s.applied) {
            return Ok(Err(format!(
                "Candidate {} was already applied; retry the fan-out stage to choose again",
                done.candidate
            )));
        }
        let candidate = &state.candidates[c];
        let id = candidate.id.clone();
        let changes = candidate
            .changes
            .clone()
            .context("The candidate's changes were not recorded")?;
        let worktree = candidate
            .attempts
            .last()
            .and_then(|a| a.worktree.clone())
            .unwrap_or_default();
        let ids = state
            .candidates
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let mut handoff = candidate
            .handoff
            .clone()
            .context("The candidate has no handoff")?;
        // The last gate before a candidate's patch reaches the workspace,
        // whoever chose it and whatever its recorded status.
        if let Some(detail) =
            crate::policy::protected_detail(&self.workflow.protected_paths, &changes.files)
        {
            return Ok(Err(format!(
                "Candidate {id} modified protected path(s): {detail}. The workspace is unchanged. Choose another candidate, or update workflow protected_paths and retry this stage"
            )));
        }
        if crate::policy::has_block_policies(&self.workflow.policies) {
            let refusing = "refusing to apply; the workspace is unchanged";
            let Some(tree) = candidate
                .attempts
                .last()
                .and_then(|a| a.tree_after.as_deref())
            else {
                return Ok(Err(format!(
                    "Candidate {id}'s worktree snapshot was not recorded, so workflow policies cannot be verified; {refusing}"
                )));
            };
            // Its patch was made from the base to this tree, whose objects
            // the workspace's repository shares.
            if let Err(error) = crate::policy::scan_tree_policies(
                &self.workflow.policies,
                &self.workspace,
                (&state.base, tree),
                &format!(" in candidate {id}"),
                refusing,
            ) {
                return Ok(Err(format!("{error:#}")));
            }
        }
        let current = snapshot::tree(&self.workspace, &self.directory)
            .context("Snapshot the workspace before applying the candidate")?;
        let scratch = self.stages[fan]
            .attempts
            .last()
            .map_or_else(|| self.directory.clone(), |a| a.directory.clone());
        let applied = fanout::apply(
            &self.workspace,
            &self.directory.join("snapshot.index"),
            &current,
            &changes.files,
            &changes.patch,
            &scratch,
        )?;
        let outcome = match &applied {
            fanout::Applied::Clean | fanout::Applied::Merged => Ok(()),
            fanout::Applied::Conflict(files) => Err(format!(
                "Candidate {id}'s changes conflict with workspace changes made since the candidates started ({}). The workspace is unchanged. The candidate's worktree stays at {} and its patch at {}: apply it yourself (git apply --3way <patch>), or undo the conflicting changes and retry this stage",
                files.join(", "),
                worktree.display(),
                changes.patch.display()
            )),
        };
        if outcome.is_ok() {
            handoff.decisions.push(format!(
                "Chosen by {by} from fan-out candidates {ids}{}",
                if applied == fanout::Applied::Merged {
                    "; merged with workspace changes"
                } else {
                    ""
                }
            ));
            if let Some(attempt) = self.stages[fan].attempts.last() {
                storage::write_json(&attempt.directory.join("handoff.json"), &handoff)?;
            }
            self.stages[fan].handoff = Some(handoff);
        }
        if let Some(state) = self.stages[fan].fan_out.as_mut() {
            state.selection = Some(Selection {
                candidate: id,
                by: by.into(),
                reason: storage::truncate(reason, 2000),
                applied: outcome.is_ok(),
                at: Utc::now(),
            });
        }
        Ok(outcome)
    }

    /// After stages ran side by side: applies a held repair request, then
    /// stops the run at the first stopped stage, waits at a checkpoint, or
    /// returns it to pending.
    pub(super) fn settle(&mut self) -> Result<()> {
        let hold = self.hold.take().unwrap_or_default();
        if let Some(index) = (0..self.stages.len()).find(|&i| self.stages[i].status.stopped()) {
            // Never leave a parallel stage (e.g. one whose repair was deferred)
            // stuck in Running when stopping at a sibling stage.
            for i in 0..self.stages.len() {
                if self.stages[i].status == Status::Running {
                    let to = if self.workflow.stages[i].enabled {
                        Status::Pending
                    } else {
                        Status::Skipped
                    };
                    self.set(
                        Target::Stage(i),
                        to,
                        Actor::Engine,
                        "Deferred because another stage stopped",
                    )?;
                }
            }
            let status = self.stages[index].status;
            let message = hold
                .stops
                .iter()
                .find(|(i, _)| *i == index)
                .map(|(_, m)| m.clone())
                .or_else(|| {
                    self.stages[index]
                        .attempts
                        .last()
                        .and_then(|a| a.error.clone())
                })
                .unwrap_or_else(|| {
                    format!(
                        "Stage {} {}",
                        self.workflow.stages[index].name,
                        status.label()
                    )
                });
            self.cursor = index;
            self.error = Some(message.clone());
            return self.set(Target::Run, status, Actor::Engine, message);
        }
        if let Some((review, target, feedback, reason)) = hold.repair {
            self.repair_feedback = Some(feedback);
            self.repair_review = Some(review);
            self.repair_passes += 1;
            // Invalidate all downstream handoffs, including previously passing reviews.
            self.reset(target, Actor::Engine, &reason)?;
            for i in 0..self.stages.len() {
                if self.stages[i].status == Status::Running {
                    let to = if self.workflow.stages[i].enabled {
                        Status::Pending
                    } else {
                        Status::Skipped
                    };
                    self.set(
                        Target::Stage(i),
                        to,
                        Actor::Engine,
                        "Deferred for repair pass",
                    )?;
                }
            }
            self.cursor = target;
        }
        if let Some(index) = self
            .stages
            .iter()
            .position(|s| s.status == Status::AwaitingConfirmation)
        {
            self.cursor = index;
            return self.set(
                Target::Run,
                Status::AwaitingConfirmation,
                Actor::Engine,
                "Stage awaits acceptance",
            );
        }
        self.cursor = self.focus();
        self.error = None;
        self.set(
            Target::Run,
            Status::Pending,
            Actor::Engine,
            "Side-by-side stages ended",
        )
    }

    /// Recovers a graph run whose router died: every running candidate,
    /// attempt, and stage failed.
    pub(super) fn recover(&mut self) -> Result<()> {
        self.interrupt_candidates(INTERRUPTED)?;
        let running: Vec<usize> = (0..self.stages.len())
            .filter(|&i| self.stages[i].status == Status::Running)
            .collect();
        let stopped = if running.is_empty() {
            vec![self.focus()]
        } else {
            running
        };
        let count = self.stages.len();
        for index in stopped.iter().copied().filter(|&i| i < count) {
            if self.stages[index]
                .attempts
                .last()
                .is_some_and(|a| a.status == Status::Running)
            {
                self.set(
                    Target::Attempt(index),
                    Status::Failed,
                    Actor::Recovery,
                    INTERRUPTED,
                )?;
                let attempt = self.stages[index]
                    .attempts
                    .last_mut()
                    .context("No attempt")?;
                attempt.error = Some(INTERRUPTED.into());
                attempt.finished_at.get_or_insert_with(Utc::now);
            }
            self.set(
                Target::Stage(index),
                Status::Failed,
                Actor::Recovery,
                INTERRUPTED,
            )?;
        }
        self.cursor = stopped[0].min(self.stages.len());
        self.set(Target::Run, Status::Failed, Actor::Recovery, INTERRUPTED)?;
        self.error = Some(INTERRUPTED.into());
        Ok(())
    }
}

/// Outputs produced by a candidate worker thread before updating `Run`.
#[derive(Default)]
pub(super) struct CandidatePost {
    pub(super) exit_code: Option<i32>,
    pub(super) usage: Option<handoff::Usage>,
    pub(super) handoff_source: Option<HandoffSource>,
    pub(super) handoff: Option<Handoff>,
    pub(super) checks_tree: Option<String>,
    pub(super) checks: Vec<CheckResult>,
    pub(super) tree_after: Option<String>,
    pub(super) changes: Option<CandidateChanges>,
    pub(super) stop: Option<(Status, String)>,
    pub(super) survivor: Option<process::Group>,
    /// The harness exited on its own: not cancelled, timed out, or stalled.
    pub(super) exited: bool,
}

/// A candidate its live budget monitor stopped mid-stream: blocked with the
/// budget reason and its usage kept. Nothing else of its output is evaluated.
pub(super) fn budget_stopped_candidate(
    prepared: &Prepared,
    exit_code: Option<i32>,
    live_usage: Option<handoff::Usage>,
    reason: String,
) -> CandidatePost {
    let mut post = CandidatePost {
        exit_code,
        ..CandidatePost::default()
    };
    post.usage = handoff::usage_file(
        &prepared.directory.join("stdout.log"),
        prepared.invocation.kind,
    )
    .or(live_usage);
    post.stop = Some((Status::Blocked, reason));
    post
}

/// Runs candidate post-processing in the worker thread: exit normalization,
/// handoff resolution, router checks, worktree snapshot, patch, and policy check.
#[allow(clippy::too_many_arguments)]
pub(super) fn post_process_candidate(
    candidate: &Stage,
    prepared: &Prepared,
    cwd: &Path,
    worktree: &Path,
    base: &str,
    run_id: &str,
    protected_paths: &[String],
    policies: &[crate::policy::DiffPolicy],
    outcome_result: Result<process::Outcome>,
    journal: &SpawnJournal,
    cancel: &AtomicBool,
    abort: &AtomicBool,
    on_spawn: &mut impl FnMut(process::Group),
    output: &mut impl FnMut(String),
) -> Result<CandidatePost> {
    let mut post = CandidatePost::default();
    let outcome = match outcome_result {
        Ok(outcome) => outcome,
        Err(error) => {
            post.survivor = error
                .downcast_ref::<process::Survivor>()
                .map(|s| s.group.clone());
            post.stop = Some((Status::Failed, format!("{error:#}")));
            return Ok(post);
        }
    };
    post.exit_code = outcome.code;
    post.usage = handoff::usage_file(
        &prepared.directory.join("stdout.log"),
        prepared.invocation.kind,
    );
    let normalized = match evaluate_outcome(Mode::Unattended, prepared, &outcome)? {
        Exit::Succeeded(normalized) => normalized,
        Exit::Stopped(status, message) => {
            post.stop = Some((status, message));
            return Ok(post);
        }
    };
    let (source, handoff) = match resolve_handoff(candidate, prepared, normalized)? {
        Resolved::Handoff(source, handoff) => (source, *handoff),
        Resolved::Stopped(status, message) => {
            post.stop = Some((status, message));
            return Ok(post);
        }
    };
    storage::write_json(&prepared.directory.join("handoff.json"), &handoff)?;
    let verdict = handoff.verdict;
    let summary = handoff.summary.clone();
    post.handoff_source = Some(source);
    post.handoff = Some(handoff);
    if matches!(verdict, Verdict::Blocked | Verdict::NeedsChanges) {
        post.stop = Some((
            Status::Blocked,
            format!(
                "Candidate reports incomplete work: {}",
                storage::truncate(&summary, 500)
            ),
        ));
        return Ok(post);
    }
    if !candidate.checks.is_empty() {
        post.checks_tree = snapshot::tree_with(worktree, &candidate_index(prepared), run_id);
        let active_pgid = std::cell::Cell::new(None);
        let (results, cancelled) = checks::run_with_abort(
            &candidate.checks,
            cwd,
            &prepared.directory,
            cancel,
            Some(abort),
            |group| {
                match group {
                    Some(group) => {
                        active_pgid.set(Some(group.pgid));
                        journal.add(&group)?;
                        on_spawn(group);
                    }
                    None => {
                        if let Some(pgid) = active_pgid.take() {
                            journal.recorded(pgid);
                        }
                    }
                }
                Ok(())
            },
            output,
        )?;
        post.checks = results;
        if cancelled {
            post.stop = Some((
                Status::Cancelled,
                "Candidate cancelled during checks; its worktree is retained".into(),
            ));
            return Ok(post);
        }
    }
    let tree = snapshot::tree_with(worktree, &candidate_index(prepared), run_id)
        .context("Snapshot the candidate's worktree")?;
    let patch = prepared.directory.join("changes.diff");
    fanout::write_patch(worktree, base, &tree, &patch)?;
    let (files, added, removed) = fanout::stat(worktree, base, &tree)?;
    let protected = crate::policy::protected_detail(protected_paths, &files);
    // The text diff, not the `git apply` patch: that one is `--binary` and
    // quotes non-ASCII paths.
    let policy_err = crate::policy::scan_tree_policies(
        policies,
        worktree,
        (base, &tree),
        " in candidate",
        "its worktree is retained",
    )
    .err();
    post.tree_after = Some(tree);
    post.changes = Some(CandidateChanges {
        files,
        added,
        removed,
        patch,
    });
    if let Some(detail) = protected {
        post.stop = Some((
            Status::Blocked,
            format!(
                "Candidate modified protected path(s): {detail}. Its worktree is retained; update workflow protected_paths or retry the stage"
            ),
        ));
    } else if let Some(error) = policy_err {
        post.stop = Some((Status::Blocked, format!("{error:#}")));
    }
    Ok(post)
}
