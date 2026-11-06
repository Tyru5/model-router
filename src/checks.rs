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
        fs::create_dir_all(&directory)?;
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
        };
        let started_at = Utc::now();
        let mut line_start = true;
        let outcome = process::capture(
            &invocation,
            workspace,
            &directory,
            Duration::from_secs(check.timeout_seconds),
            cancel,
            |err, text| {
                line_start = text.ends_with('\n');
                output(format!(
                    "{}{}",
                    if err { "[stderr] " } else { "" },
                    storage::clean_terminal(text)
                ))
            },
        );
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
