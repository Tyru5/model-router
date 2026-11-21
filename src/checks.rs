//! Router-executed stage checks. Commands are argument vectors; no shell evaluates them.
use crate::{
    adapters::{Invocation, OutputKind},
    config::Check,
    handoff::TestStatus,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub command: Vec<String>,
    pub required: bool,
    pub status: TestStatus,
    pub exit_code: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    /// Holds this check's stdout.log and stderr.log.
    pub directory: PathBuf,
    pub error: Option<String>,
}
impl CheckResult {
    /// A short result, e.g. "passed", "exit 3", or the launch/timeout error.
    pub fn outcome(&self) -> String {
        match (&self.error, self.status, self.exit_code) {
            (Some(error), _, _) => error.clone(),
            (None, TestStatus::Passed, _) => "passed".into(),
            (None, _, Some(code)) => format!("exit {code}"),
            (None, _, None) => "no exit code".into(),
        }
    }
}

/// Runs checks in order, logging to `<attempt>/checks/NN/` and `<attempt>/checks.json`.
/// Returns the results and whether cancellation stopped them.
pub fn run(
    checks: &[Check],
    workspace: &Path,
    attempt: &Path,
    cancel: &AtomicBool,
    record_group: impl FnMut(Option<process::Group>) -> Result<()>,
    output: impl FnMut(String),
) -> Result<(Vec<CheckResult>, bool)> {
    run_with_abort(
        checks,
        workspace,
        attempt,
        cancel,
        None,
        record_group,
        output,
    )
}

/// Runs checks with an optional secondary `abort` flag alongside `cancel`.
pub fn run_with_abort(
    checks: &[Check],
    workspace: &Path,
    attempt: &Path,
    cancel: &AtomicBool,
    abort: Option<&AtomicBool>,
    mut record_group: impl FnMut(Option<process::Group>) -> Result<()>,
    mut output: impl FnMut(String),
) -> Result<(Vec<CheckResult>, bool)> {
    let mut results = Vec::new();
    let mut cancelled = false;
    for (index, check) in checks.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) || abort.is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            cancelled = true;
            break;
        }
        let directory = attempt.join("checks").join(format!("{:02}", index + 1));
        // The stage could write the attempt directory: logs go only into
        // real directories, never through a link it left at either name.
        for dir in [attempt.join("checks"), directory.clone()] {
            if fs::symlink_metadata(&dir).is_ok_and(|meta| !meta.is_dir()) {
                fs::remove_file(&dir)?;
            }
            match fs::create_dir(&dir) {
                Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e.into()),
                _ => {}
            }
            // Something re-created a link meanwhile: refuse, never follow it.
            anyhow::ensure!(
                fs::symlink_metadata(&dir)?.is_dir(),
                "{} is not a directory",
                dir.display()
            );
        }
        let label = format!("[check {}/{}]", index + 1, checks.len());
        output(format!("\n{label} {}\n", shell_words::join(&check.command)));
        let started_at = Utc::now();
        let Some((program, args)) = check.command.split_first() else {
            let result = CheckResult {
                command: check.command.clone(),
                required: check.required,
                status: TestStatus::Failed,
                exit_code: None,
                started_at,
                finished_at: Utc::now(),
                directory,
                error: Some("Check command is empty".into()),
            };
            output(format!("{label} Check command is empty\n"));
            results.push(result);
            continue;
        };
        let invocation = Invocation {
            program: program.clone(),
            args: args.to_vec(),
            stdin: None,
            kind: OutputKind::Text,
            final_file: PathBuf::new(),
            env: vec![],
            files: vec![],
            assign: None,
        };
        let mut line_start = true;
        let outcome = process::capture_with_stall_and_abort(
            &invocation,
            workspace,
            &directory,
            Duration::from_secs(check.timeout_seconds),
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
        let survived = outcome
            .as_ref()
            .err()
            .is_some_and(|e| e.downcast_ref::<process::Survivor>().is_some());
        if !survived {
            record_group(None)?;
        }
        let (status, exit_code, error) = match outcome {
            Ok(o) if o.cancelled => {
                cancelled = true;
                (TestStatus::NotRun, o.code, Some("Cancelled".into()))
            }
            Ok(o) if o.timed_out => (
                TestStatus::Failed,
                o.code,
                Some(format!("Timed out after {}s", check.timeout_seconds)),
            ),
            Ok(o) if o.success() => (TestStatus::Passed, o.code, None),
            Ok(o) => (
                TestStatus::Failed,
                o.code,
                o.code.is_none().then(|| "Terminated by a signal".into()),
            ),
            Err(e) => (TestStatus::Failed, None, Some(format!("{e:#}"))),
        };
        let result = CheckResult {
            command: check.command.clone(),
            required: check.required,
            status,
            exit_code,
            started_at,
            finished_at: Utc::now(),
            directory,
            error,
        };
        output(format!(
            "{}{label} {}\n",
            if line_start { "" } else { "\n" },
            result.outcome()
        ));
        results.push(result);
        if cancelled {
            break;
        }
    }
    storage::write_json(&attempt.join("checks.json"), &results)?;
    Ok((results, cancelled))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_logs_never_write_through_links_the_stage_left() {
        let dir = tempfile::tempdir().unwrap();
        let attempt = dir.path().join("attempt");
        let outside = dir.path().join("outside");
        fs::create_dir_all(attempt.join("checks/01")).unwrap();
        fs::create_dir(&outside).unwrap();
        let victim = outside.join("victim");
        fs::write(&victim, "keep").unwrap();
        // As a stage that could write its attempt directory might leave them.
        std::os::unix::fs::symlink(&victim, attempt.join("checks/01/stdout.log")).unwrap();
        std::os::unix::fs::symlink(&outside, attempt.join("checks/02")).unwrap();
        let check = |text: &str| Check {
            command: vec!["echo".into(), text.into()],
            required: true,
            ..Default::default()
        };
        let (results, cancelled) = run(
            &[check("one"), check("two")],
            dir.path(),
            &attempt,
            &AtomicBool::new(false),
            |_| Ok(()),
            |_| {},
        )
        .unwrap();
        assert!(!cancelled);
        assert!(results.iter().all(|r| r.status == TestStatus::Passed));
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
        for (n, text) in [(1, "one"), (2, "two")] {
            let log = attempt.join(format!("checks/{n:02}/stdout.log"));
            assert_eq!(fs::read_to_string(&log).unwrap(), format!("{text}\n"));
        }
    }

    #[test]
    fn empty_command_fails_cleanly_and_stderr_prefixes_lines_only() {
        let dir = tempfile::tempdir().unwrap();
        let attempt = dir.path().join("attempt");
        fs::create_dir_all(&attempt).unwrap();
        let empty = Check {
            command: vec![],
            required: true,
            ..Default::default()
        };
        let multi = Check {
            command: vec![
                "python3".into(),
                "-c".into(),
                "import sys, time; sys.stderr.write('a'); sys.stderr.flush(); time.sleep(0.05); sys.stderr.write('b\\nc\\n'); sys.stderr.flush()".into(),
            ],
            required: true,
            ..Default::default()
        };
        let mut collected = String::new();
        let (results, cancelled) = run(
            &[empty, multi],
            dir.path(),
            &attempt,
            &AtomicBool::new(false),
            |_| Ok(()),
            |text| collected.push_str(&text),
        )
        .unwrap();
        assert!(!cancelled);
        assert_eq!(results[0].status, TestStatus::Failed);
        assert_eq!(results[0].error.as_deref(), Some("Check command is empty"));
        assert_eq!(results[1].status, TestStatus::Passed);
        assert!(
            collected.contains("[stderr] ab\n[stderr] c\n"),
            "{collected:?}"
        );
    }
}
