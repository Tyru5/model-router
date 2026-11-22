//! The text a delegating parent's prompt carries. Under 4,000 bytes: Codex
//! truncates longer entries.

/// How a parent learns that its tasks finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentKind {
    /// Unattended, and its session can be resumed: the router wakes it.
    Wake,
    /// Unattended without a follow-up command: it waits in tool calls.
    NoWake,
    /// Interactive; `resumable` says whether exiting with tasks running
    /// resumes it unattended.
    Interactive { resumable: bool },
}

/// The `next` hint of an unfinished task's result.
pub fn next_hint(kind: ParentKind) -> &'static str {
    match kind {
        ParentKind::Wake => {
            "Completion resumes this session. Do not wait on it or poll it. Continue other work, and end your turn when none is left."
        }
        ParentKind::NoWake => {
            "Call task_wait with this task_id again while wait_timed_out is true."
        }
        ParentKind::Interactive { .. } => {
            "Tell the user what you wait on; read the result with task_status when they prompt you."
        }
    }
}

/// The instruction block for a delegating parent of `kind`.
pub fn text(kind: ParentKind, runs: bool, schedules: bool) -> String {
    let mut text = String::from(
        "\nDELEGATION (model-router)\n\
Ordinary work needs no delegation.\n\
- Delegate work that gains from another model or from running in parallel: a second-opinion review, an independent investigation, an isolated implementation. Fix small things yourself. Tasks cannot delegate further.\n\
- Use your harness's own subagents only for work on your own model and permissions. Use the model-router `delegate_task` tool for another harness, model, or effort, or for an isolated worktree.\n\
- Call `orchestrator_capabilities` once before your first `delegate_task`, and again only after an error. Use only listed targets. If one you need is missing or fails, say so; never substitute another, and never start agent CLIs or git worktrees yourself.\n\
- A task sees only your `task` text. Write a full brief: goal, files, constraints, and what to return. No secrets.\n\
- `shared` tasks are read-only; keep the files they read stable. `worktree` tasks change their own checkout; read their `patches`, then `task_apply` what you accept.\n\
- Give each call its own `client_request_id`; reuse it only to retry that call.\n",
    );
    text.push_str(match kind {
        ParentKind::Wake => {
            "- Follow a task's `next`: keep working, then end your turn. The router resumes you to read the results with `task_status`. Don't poll `task_status`, sleep, or loop. Until you have read every task's result, end your turn with verdict `blocked` and summary `waiting on tasks`; write your real handoff only after reading them all.\n"
        }
        ParentKind::NoWake => {
            "- This session cannot be resumed: use `mode: \"wait\"` and call `task_wait` again while `wait_timed_out` is true, until your tasks finish. Read every result before you write your handoff or exit.\n"
        }
        ParentKind::Interactive { resumable: true } => {
            "- Tasks finish while you talk with the user. Tell the user what you are waiting on; when they prompt you, read results with `task_status`. Read every result before you write your handoff. If you exit with tasks running, the router resumes you to read them.\n"
        }
        ParentKind::Interactive { resumable: false } => {
            "- Tasks finish while you talk with the user. Tell the user what you are waiting on; when they prompt you, read results with `task_status`. Read every result before you write your handoff. Unread or running tasks when you exit stop the stage.\n"
        }
    });
    text.push_str(
        "- A task that asks questions waits for `task_send` with your answer; or ask the user.\n\
- Each new review round is a new `delegate_task` with the brief, prior findings, and objections. `task_cancel` tasks you no longer need.\n",
    );
    if runs || schedules {
        text.push_str(
            "- `run_launch` and `schedule_task` only when the user asks for a new run or a schedule.\n",
        );
    }
    text
}

/// The extra line a task's prompt carries.
pub const TASK_QUESTION_LINE: &str = "To ask whoever delegated this task a question, return verdict blocked with your questions in unresolved.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_fits_codexs_limit_and_names_one_wake_rule() {
        for kind in [
            ParentKind::Wake,
            ParentKind::NoWake,
            ParentKind::Interactive { resumable: true },
            ParentKind::Interactive { resumable: false },
        ] {
            let text = text(kind, true, true);
            assert!(text.len() < 4000, "{kind:?}: {} bytes", text.len());
            assert!(text.contains("Read every result") || text.contains("reading them all"));
        }
        assert!(text(ParentKind::NoWake, false, false).contains("task_wait"));
        assert!(!text(ParentKind::Wake, false, false).contains("run_launch"));
    }
}
