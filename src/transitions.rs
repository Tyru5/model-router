//! The run's status timeline: one JSON record per run, stage, or attempt
//! status change, appended to `<run dir>/transitions.jsonl` as the run state
//! that made it is saved. `Run::transition` is the only writer.

use crate::engine::Status;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

pub const FILE: &str = "transitions.jsonl";

/// Who caused a status change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    /// The engine, driving the run.
    Engine,
    /// An explicit user command: retry, take over, confirm, allow repair.
    User,
    /// Cleanup of an execution that was interrupted.
    Recovery,
}
impl Actor {
    pub fn label(self) -> &'static str {
        match self {
            Self::Engine => "engine",
            Self::User => "user",
            Self::Recovery => "recovery",
        }
    }
}

/// Whose status changes. `Attempt(stage)` is that stage's latest attempt;
/// `Candidate(stage, n)` is fan-out candidate `n` of that stage, and
/// `CandidateAttempt(stage, n)` its latest attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Run,
    Stage(usize),
    Attempt(usize),
    Candidate(usize, usize),
    CandidateAttempt(usize, usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Run,
    Stage,
    Attempt,
    /// A fan-out candidate of a stage.
    Candidate,
}
impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Stage => "stage",
            Self::Attempt => "attempt",
            Self::Candidate => "candidate",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub at: DateTime<Utc>,
    /// The run.json revision that saved this change.
    pub run_revision: u64,
    pub level: Level,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_index: Option<usize>,
    /// The attempt's number, for attempt records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<usize>,
    /// The fan-out candidate's ID, for candidate records and their attempts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    pub from: Status,
    pub to: Status,
    pub reason: String,
    pub actor: Actor,
}

/// Appends `records` and syncs them to disk. A torn last line, left by a
/// crash mid-append, is cut off first so it never runs into a new record.
pub fn append(dir: &Path, records: &[Record]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let path = dir.join(FILE);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("Open {}", path.display()))?;
    let length = file.metadata()?.len();
    if length > 0 {
        let mut text = Vec::new();
        file.read_to_end(&mut text)?;
        if text.last() != Some(&b'\n') {
            let keep = text.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            file.set_len(keep as u64)?;
        }
    }
    file.seek(SeekFrom::End(0))?;
    let mut lines = Vec::new();
    for record in records {
        serde_json::to_writer(&mut lines, record)?;
        lines.push(b'\n');
    }
    file.write_all(&lines)?;
    file.sync_data()
        .with_context(|| format!("Save {}", path.display()))
}

/// Every saved record, oldest first; none for runs saved before transitions
/// were recorded. A torn last line is ignored; damage elsewhere is an error.
pub fn read(dir: &Path) -> Result<Vec<Record>> {
    let path = dir.join(FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error).with_context(|| format!("Read {}", path.display())),
    };
    let complete = text.ends_with('\n');
    let lines: Vec<&str> = text.lines().collect();
    let mut records = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        match serde_json::from_str(line) {
            Ok(record) => records.push(record),
            Err(_) if index + 1 == lines.len() && !complete => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Parse {} line {}", path.display(), index + 1));
            }
        }
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(to: Status) -> Record {
        Record {
            at: Utc::now(),
            run_revision: 2,
            level: Level::Run,
            stage_index: None,
            attempt: None,
            candidate: None,
            from: Status::Pending,
            to,
            reason: "test".into(),
            actor: Actor::Engine,
        }
    }

    #[test]
    fn a_torn_last_line_is_ignored_and_cut_before_the_next_append() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path()).unwrap().is_empty());
        append(dir.path(), &[record(Status::Running)]).unwrap();
        let path = dir.path().join(FILE);
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("{\"at\":\"2026-");
        fs::write(&path, &text).unwrap();
        assert_eq!(read(dir.path()).unwrap(), vec![record_like(&path, 0)]);
        append(dir.path(), &[record(Status::Failed)]).unwrap();
        let saved = read(dir.path()).unwrap();
        assert_eq!(
            saved.iter().map(|r| r.to).collect::<Vec<_>>(),
            [Status::Running, Status::Failed]
        );
        // Damage before the last line is reported, not skipped.
        fs::write(
            &path,
            format!("garbage\n{}", fs::read_to_string(&path).unwrap()),
        )
        .unwrap();
        assert!(read(dir.path()).is_err());
    }

    fn record_like(path: &Path, line: usize) -> Record {
        let text = fs::read_to_string(path).unwrap();
        serde_json::from_str(text.lines().nth(line).unwrap()).unwrap()
    }
}
