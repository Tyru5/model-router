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
    mut record_group: impl FnMut(Option<u32>) -> Result<()>,
    mut output: impl FnMut(String),
) -> Result<(Vec<CheckResult>, bool)> {
    let mut results = Vec::new();
    let mut cancelled = false;
    for (index, check) in checks.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
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
        let invocation = Invocation {
            program: check.command[0].clone(),
            args: check.command[1..].to_vec(),
            stdin: None,
            kind: OutputKind::Text,
            final_file: PathBuf::new(),
            env: vec![],
            files: vec![],
            assign: None,
        };
        let started_at = Utc::now();
        let mut line_start = true;
        let outcome = process::capture_with(
            &invocation,
            workspace,
            &directory,
            Duration::from_secs(check.timeout_seconds),
            cancel,
            None,
            |pgid| record_group(Some(pgid)),
            |err, text| {
                line_start = text.ends_with('\n');
                output(format!(
                    "{}{}",
                    if err { "[stderr] " } else { "" },
                    storage::clean_terminal(text)
                ))
            },
        );
        record_group(None)?;
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
}
