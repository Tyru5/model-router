//! Trust-gated lifecycle hooks (`before_stage`, `after_stage`, `before_ship`,
//! `on_failure`). Commands are argument vectors executed in their own process
//! group; no shell evaluates them.
use crate::{
    adapters::{EnvValue, Invocation, OutputKind},
    config::Hook,
    process, storage,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    BeforeStage,
    AfterStage,
    BeforeShip,
    OnFailure,
}

impl HookEvent {
    pub fn label(self) -> &'static str {
        match self {
            Self::BeforeStage => "before_stage",
            Self::AfterStage => "after_stage",
            Self::BeforeShip => "before_ship",
            Self::OnFailure => "on_failure",
        }
    }

    /// `before_stage` and `before_ship` abort on non-zero exit or timeout;
    /// `after_stage` and `on_failure` log results without masking stage outcome.
    pub fn aborts_on_failure(self) -> bool {
        matches!(self, Self::BeforeStage | Self::BeforeShip)
    }
}

#[derive(Debug, Clone, Default)]
pub struct HookContext {
    pub run_id: String,
    pub workflow: String,
    pub stage_id: Option<String>,
    pub stage_role: Option<String>,
    pub stage_status: Option<String>,
}

impl HookContext {
    fn env(&self, event: HookEvent) -> Vec<(String, EnvValue)> {
        let mut vars = vec![
            (
                "MODEL_ROUTER_HOOK".into(),
                EnvValue::Literal(event.label().into()),
            ),
            (
                "MODEL_ROUTER_RUN_ID".into(),
                EnvValue::Literal(self.run_id.clone()),
            ),
            (
                "MODEL_ROUTER_WORKFLOW".into(),
                EnvValue::Literal(self.workflow.clone()),
            ),
        ];
        if let Some(id) = &self.stage_id {
            vars.push((
                "MODEL_ROUTER_STAGE_ID".into(),
                EnvValue::Literal(id.clone()),
            ));
        }
        if let Some(role) = &self.stage_role {
            vars.push((
                "MODEL_ROUTER_STAGE_ROLE".into(),
                EnvValue::Literal(role.clone()),
            ));
        }
        if let Some(status) = &self.stage_status {
            vars.push((
                "MODEL_ROUTER_STAGE_STATUS".into(),
                EnvValue::Literal(status.clone()),
            ));
        }
        vars
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookResult {
    pub event: String,
    pub command: Vec<String>,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    /// Holds this hook's stdout.log and stderr.log.
    pub directory: PathBuf,
    pub error: Option<String>,
}

impl HookResult {
    pub fn outcome(&self) -> String {
        match (&self.error, self.passed, self.exit_code) {
            (Some(error), _, _) => error.clone(),
            (None, true, _) => "passed".into(),
            (None, false, Some(code)) => format!("exit {code}"),
            (None, false, None) => "no exit code".into(),
        }
    }
}

#[derive(Debug, Default)]
pub struct HookRun {
    pub results: Vec<HookResult>,
    pub cancelled: bool,
    /// Set when an aborting hook (`before_stage` / `before_ship`) failed or timed out.
    pub failed: Option<String>,
    /// Process group that survived termination, if any.
    pub survivor: Option<process::Group>,
}

/// Ensures `dir` and its parent components under `root` are real directories,
/// never following symlinks a stage could have left behind.
fn ensure_safe_dir(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|meta| !meta.is_dir()) {
        fs::remove_file(path)?;
    }
    match fs::create_dir(path) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e.into()),
        _ => {}
    }
    anyhow::ensure!(
        fs::symlink_metadata(path)?.is_dir(),
        "{} is not a directory",
        path.display()
    );
    Ok(())
}

/// Runs `hooks` in order for `event`, logging to `<log_root>/hooks/<event>/NN/`
/// and `<log_root>/hooks/<event>.json`.
#[allow(clippy::too_many_arguments)]
pub fn run(
    event: HookEvent,
    hooks: &[Hook],
    context: &HookContext,
    workspace: &Path,
    log_root: &Path,
    cancel: &AtomicBool,
    abort: Option<&AtomicBool>,
    mut record_group: impl FnMut(Option<process::Group>) -> Result<()>,
    mut output: impl FnMut(String),
) -> Result<HookRun> {
    let mut run_result = HookRun::default();
    if hooks.is_empty() {
        return Ok(run_result);
    }
    let hooks_root = log_root.join("hooks");
    let event_dir = hooks_root.join(event.label());
    ensure_safe_dir(&hooks_root)?;
    ensure_safe_dir(&event_dir)?;
    let env = context.env(event);
    let abort_on_failure = event.aborts_on_failure();

    for (index, hook) in hooks.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) || abort.is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            run_result.cancelled = true;
            break;
        }
        let directory = event_dir.join(format!("{:02}", index + 1));
        ensure_safe_dir(&directory)?;
        let label = format!("[hook {} {}/{}]", event.label(), index + 1, hooks.len());
        output(format!("\n{label} {}\n", shell_words::join(&hook.command)));
        let started_at = Utc::now();
        let Some((program, args)) = hook.command.split_first() else {
            let result = HookResult {
                event: event.label().into(),
                command: hook.command.clone(),
                passed: false,
                exit_code: None,
                started_at,
                finished_at: Utc::now(),
                directory,
                error: Some("Hook command is empty".into()),
            };
            output(format!("{label} Hook command is empty\n"));
            if abort_on_failure {
                run_result.failed =
                    Some(format!("Hook {} failed: command is empty", event.label()));
                run_result.results.push(result);
                break;
            }
            run_result.results.push(result);
            continue;
        };
        let invocation = Invocation {
            program: program.clone(),
            args: args.to_vec(),
            stdin: None,
            kind: OutputKind::Text,
            final_file: PathBuf::new(),
            env: env.clone(),
            files: vec![],
            assign: None,
        };
        let mut line_start = true;
        let outcome = process::capture_with_stall_and_abort(
            &invocation,
            workspace,
            &directory,
            Duration::from_secs(hook.timeout_seconds),
            None,
            cancel,
            abort,
            None,
            |group| record_group(Some(group)),
            |err, text| {
                let cleaned = storage::clean_terminal(text);
                if cleaned.is_empty() {
                    return;
                }
                if !err {
                    line_start = cleaned.ends_with('\n');
                    output(cleaned);
                    return;
                }
                let mut formatted = String::with_capacity(cleaned.len() + 9);
                for segment in cleaned.split_inclusive('\n') {
                    if line_start {
                        formatted.push_str("[stderr] ");
                    }
                    formatted.push_str(segment);
                    line_start = segment.ends_with('\n');
                }
                output(formatted);
            },
        );
        let survivor = outcome
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<process::Survivor>())
            .map(|s| s.group.clone());
        if let Some(group) = survivor {
            run_result.survivor = Some(group);
        } else {
            let _ = record_group(None);
        }
        let (passed, exit_code, error) = match outcome {
            Ok(o) if o.cancelled => {
                run_result.cancelled = true;
                (false, o.code, Some("Cancelled".into()))
            }
            Ok(o) if o.timed_out => (
                false,
                o.code,
                Some(format!("Timed out after {}s", hook.timeout_seconds)),
            ),
            Ok(o) if o.success() => (true, o.code, None),
            Ok(o) => (
                false,
                o.code,
                o.code.is_none().then(|| "Terminated by a signal".into()),
            ),
            Err(e) => (false, None, Some(format!("{e:#}"))),
        };
        let result = HookResult {
            event: event.label().into(),
            command: hook.command.clone(),
            passed,
            exit_code,
            started_at,
            finished_at: Utc::now(),
            directory: directory.clone(),
            error,
        };
        let outcome_text = result.outcome();
        output(format!(
            "{}{label} {outcome_text}\n",
            if line_start { "" } else { "\n" },
        ));
        let failed_message = (!passed && !run_result.cancelled).then(|| {
            format!(
                "Hook {} `{}` failed ({outcome_text}). Logs: {}",
                event.label(),
                shell_words::join(&hook.command),
                directory.display()
            )
        });
        run_result.results.push(result);
        if run_result.cancelled {
            break;
        }
        if let Some(message) = failed_message {
            if abort_on_failure {
                run_result.failed = Some(message);
                break;
            }
        }
    }

    let summary_path = hooks_root.join(format!("{}.json", event.label()));
    if fs::symlink_metadata(&summary_path).is_ok_and(|m| m.file_type().is_symlink()) {
        fs::remove_file(&summary_path)?;
    }
    storage::write_json(&summary_path, &run_result.results)?;
    Ok(run_result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn before_stage_aborts_on_failure_and_timeout_while_after_stage_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let attempt = dir.path().join("attempt");
        fs::create_dir_all(&attempt).unwrap();
        let ctx = HookContext {
            run_id: "r1".into(),
            workflow: "wf".into(),
            stage_id: Some("impl".into()),
            stage_role: Some("implementation".into()),
            stage_status: None,
        };
        let fail = Hook {
            command: vec![
                "python3".into(),
                "-c".into(),
                "import sys; sys.exit(2)".into(),
            ],
            timeout_seconds: 10,
        };
        let second = Hook {
            command: vec!["echo".into(), "should-not-run".into()],
            timeout_seconds: 10,
        };
        let mut out = String::new();
        let res = run(
            HookEvent::BeforeStage,
            &[fail.clone(), second.clone()],
            &ctx,
            dir.path(),
            &attempt,
            &AtomicBool::new(false),
            None,
            |_| Ok(()),
            |s| out.push_str(&s),
        )
        .unwrap();
        assert!(res.failed.is_some());
        assert_eq!(res.results.len(), 1, "aborts before second hook");
        assert!(!out.contains("should-not-run"));

        let mut out2 = String::new();
        let res2 = run(
            HookEvent::AfterStage,
            &[fail, second],
            &ctx,
            dir.path(),
            &attempt,
            &AtomicBool::new(false),
            None,
            |_| Ok(()),
            |s| out2.push_str(&s),
        )
        .unwrap();
        assert!(res2.failed.is_none(), "after_stage never masks outcome");
        assert_eq!(res2.results.len(), 2);
        assert!(!res2.results[0].passed);
        assert!(res2.results[1].passed);
        assert!(out2.contains("should-not-run"));
    }

    #[test]
    fn hook_logs_refuse_symlinks_and_pass_context_env() {
        let dir = tempfile::tempdir().unwrap();
        let attempt = dir.path().join("attempt");
        let outside = dir.path().join("outside");
        fs::create_dir_all(attempt.join("hooks/before_stage/01")).unwrap();
        fs::create_dir(&outside).unwrap();
        let victim = outside.join("victim");
        fs::write(&victim, "safe").unwrap();
        std::os::unix::fs::symlink(&victim, attempt.join("hooks/before_stage/01/stdout.log"))
            .unwrap();
        let ctx = HookContext {
            run_id: "run-42".into(),
            workflow: "Dev".into(),
            stage_id: Some("impl".into()),
            stage_role: Some("implementation".into()),
            stage_status: Some("complete".into()),
        };
        let hook = Hook {
            command: vec![
                "python3".into(),
                "-c".into(),
                "import os; print(os.environ['MODEL_ROUTER_HOOK'], os.environ['MODEL_ROUTER_RUN_ID'], os.environ['MODEL_ROUTER_STAGE_ID'])".into(),
            ],
            timeout_seconds: 10,
        };
        let res = run(
            HookEvent::BeforeStage,
            &[hook],
            &ctx,
            dir.path(),
            &attempt,
            &AtomicBool::new(false),
            None,
            |_| Ok(()),
            |_| {},
        )
        .unwrap();
        assert!(res.failed.is_none());
        assert!(res.results[0].passed);
        assert_eq!(fs::read_to_string(&victim).unwrap(), "safe");
        let log = fs::read_to_string(attempt.join("hooks/before_stage/01/stdout.log")).unwrap();
        assert_eq!(log.trim(), "before_stage run-42 impl");
    }
}
