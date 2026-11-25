//! `task_apply`: a worktree task's sealed patch reaches the parent's
//! checkout only here, on the owner loop, with an apply journal so a crash
//! mid-apply has a known outcome.
use super::*;
use crate::delegation::{
    Isolation,
    policy::{Ceiling, Write},
    records::{ApplyJournal, TaskStatus},
};
use crate::serve::mcp::ToolError;
use serde_json::{Value, json};

fn error(code: &'static str, message: impl Into<String>) -> ToolError {
    ToolError::new(code, message)
}

/// The tree the checkout would have after `patch`, by the same steps
/// `fanout::apply` takes (a plain apply, then a three-way one) on a copy of
/// the snapshot index. None when it cannot tell (no Git 2.32 `--cached`
/// three-way merge, or a conflict).
fn predict(workspace: &Path, index: &Path, patch: &Path, scratch: &Path) -> Option<String> {
    let sanitized = snapshot::SanitizedGitDir::new(workspace).ok()?;
    let top = sanitized.top_level().to_path_buf();
    let predict = std::path::absolute(scratch.join("predict.index")).ok()?;
    fs::copy(index, &predict).ok()?;
    let run = |args: &[&str]| {
        sanitized
            .command(&top)
            .args(args)
            .env("GIT_INDEX_FILE", &predict)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
    };
    let patch = patch.to_str()?;
    let applied = run(&["apply", "--cached", "--binary", patch]).is_some()
        || run(&["apply", "--3way", "--cached", "--binary", patch]).is_some();
    let tree = applied
        .then(|| run(&["write-tree"]))
        .flatten()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    let _ = fs::remove_file(&predict);
    tree.filter(|t| !t.is_empty())
}

/// Applies revision `revision` of task `id` to the parent's checkout.
pub(super) fn apply_patch(
    run: &mut Run,
    index: usize,
    stage: &Stage,
    ceiling: &Ceiling,
    id: &str,
    revision: usize,
) -> std::result::Result<Value, ToolError> {
    if ceiling.write < Write::Workspace
        || !matches!(stage.role, Role::Implementation | Role::Test | Role::Custom)
    {
        return Err(error(
            "permission_escalation_denied",
            "Only a stage that writes its checkout (implementation, test, or custom, not read-only) can apply a task's patch",
        ));
    }
    let attempt = run.stages[index]
        .attempts
        .last()
        .ok_or_else(|| error("parent_not_active", "No attempt"))?;
    let task = attempt
        .tasks
        .iter()
        .find(|t| t.id == id)
        .ok_or_else(|| error("task_not_found", format!("No task {id}")))?;
    if task.isolation != Isolation::Worktree {
        return Err(error(
            "invalid_request",
            format!("Task {id} is a shared task; it has no patch"),
        ));
    }
    if task.status.is_live() {
        return Err(error(
            "invalid_request",
            format!("Task {id} is still running; apply a finished revision"),
        ));
    }
    let patch = task
        .patches
        .iter()
        .find(|p| p.revision == revision)
        .cloned()
        .ok_or_else(|| {
            error(
                "invalid_request",
                format!("Task {id} has no patch revision {revision}"),
            )
        })?;
    if let Some(journal) = &patch.apply {
        if journal.applied_at.is_some() {
            return Ok(json!({"status": "already_applied"}));
        }
        if journal.unknown {
            return Err(error(
                "apply_unknown",
                "An earlier apply of this revision was interrupted and the checkout matches neither tree; inspect it before applying again",
            ));
        }
    }
    // The patch bytes are the router's; compare with the hash in memory.
    let bytes = fs::read(&patch.path).map_err(|e| error("orchestration_error", e.to_string()))?;
    if super::delegate::sha256_hex(&bytes) != patch.sha256 {
        return Err(error(
            "orchestration_error",
            format!("Task {id}'s patch {revision} changed since it was sealed; it is not applied"),
        ));
    }
    if let Some(bad) = patch.files.iter().find(|f| !fanout::is_plain_repo_path(f)) {
        return Err(error(
            "protected_path",
            format!("The patch touches {bad}, which is never applied"),
        ));
    }
    if let Some(detail) =
        crate::policy::protected_detail(&run.workflow.protected_paths, &patch.files)
    {
        return Err(error(
            "protected_path",
            format!("The patch modifies protected path(s): {detail}"),
        ));
    }
    if matches!(task.status, TaskStatus::Blocked)
        && task
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with("protected_path"))
    {
        return Err(error(
            "protected_path",
            task.error.clone().unwrap_or_default(),
        ));
    }
    let current = snapshot::tree(&run.workspace, &run.directory)
        .ok_or_else(|| error("worktree_unavailable", "Could not snapshot the checkout"))?;
    let snapshot_index = run.directory.join("snapshot.index");
    let scratch = patch
        .path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| run.directory.clone());
    let expected = predict(&run.workspace, &snapshot_index, &patch.path, &scratch);
    // Journal before touching the checkout.
    set_journal(
        run,
        index,
        id,
        revision,
        Some(ApplyJournal {
            pre_tree: current.clone(),
            expected_post_tree: expected,
            started_at: Utc::now(),
            applied_at: None,
            unknown: false,
        }),
    )?;
    let applied = match fanout::apply(
        &run.workspace,
        &snapshot_index,
        &current,
        &patch.files,
        &patch.path,
        &scratch,
    ) {
        Ok(applied) => applied,
        Err(e) => {
            // A refusal wrote nothing: forget the attempt, so recovery
            // cannot take a later change to the checkout for this apply.
            if e.downcast_ref::<fanout::Refused>().is_some() {
                set_journal(run, index, id, revision, None)?;
            }
            return Err(error("orchestration_error", format!("{e:#}")));
        }
    };
    match applied {
        fanout::Applied::Clean | fanout::Applied::Merged => {
            if let Some(journal) = journal_mut(run, index, id, revision) {
                journal.applied_at = Some(Utc::now());
            }
            run.persist()
                .map_err(|e| error("orchestration_error", format!("{e:#}")))?;
            Ok(json!({
                "status": if applied == fanout::Applied::Merged { "merged" } else { "applied" },
                "files": patch.files,
            }))
        }
        fanout::Applied::Conflict(files) => {
            // The checkout is unchanged: forget the attempt.
            set_journal(run, index, id, revision, None)?;
            Err(error(
                "patch_conflict",
                format!(
                    "The patch conflicts with your changes in {}; nothing was applied",
                    files.join(", ")
                ),
            ))
        }
    }
}

fn journal_mut<'r>(
    run: &'r mut Run,
    index: usize,
    id: &str,
    revision: usize,
) -> Option<&'r mut ApplyJournal> {
    run.stages[index]
        .attempts
        .last_mut()?
        .tasks
        .iter_mut()
        .find(|t| t.id == id)?
        .patches
        .iter_mut()
        .find(|p| p.revision == revision)?
        .apply
        .as_mut()
}

fn set_journal(
    run: &mut Run,
    index: usize,
    id: &str,
    revision: usize,
    journal: Option<ApplyJournal>,
) -> std::result::Result<(), ToolError> {
    if let Some(patch) = run.stages[index]
        .attempts
        .last_mut()
        .and_then(|a| a.tasks.iter_mut().find(|t| t.id == id))
        .and_then(|t| t.patches.iter_mut().find(|p| p.revision == revision))
    {
        patch.apply = journal;
    }
    run.persist()
        .map_err(|e| error("orchestration_error", format!("{e:#}")))
}

/// After a crash: settles every apply that started and never finished, by
/// comparing the checkout with the trees its journal recorded.
pub(super) fn settle_interrupted_applies(run: &mut Run) {
    let pending = run
        .stages
        .iter()
        .flat_map(|s| &s.attempts)
        .flat_map(|a| &a.tasks)
        .flat_map(|t| &t.patches)
        .any(|p| {
            p.apply
                .as_ref()
                .is_some_and(|j| j.applied_at.is_none() && !j.unknown)
        });
    if !pending {
        return;
    }
    let current = snapshot::tree(&run.workspace, &run.directory);
    for patch in run
        .stages
        .iter_mut()
        .flat_map(|s| &mut s.attempts)
        .flat_map(|a| &mut a.tasks)
        .flat_map(|t| &mut t.patches)
    {
        let Some(journal) = patch.apply.as_mut() else {
            continue;
        };
        if journal.applied_at.is_some() || journal.unknown {
            continue;
        }
        if current.as_deref() == Some(journal.pre_tree.as_str()) {
            patch.apply = None;
        } else if current.is_some() && current == journal.expected_post_tree {
            journal.applied_at = Some(Utc::now());
        } else {
            journal.unknown = true;
        }
    }
}
