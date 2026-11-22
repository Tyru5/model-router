//! Readable progress lines from harness JSON events, for the Monitor and CLI.
//! Display only: the raw events stay in stdout.log, and handoffs never come
//! from these lines.
use crate::{adapters::OutputKind, storage};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

/// Longest tool argument or result excerpt shown on one line.
const EXCERPT: usize = 160;
/// Longest assistant or reasoning text shown for one event.
const MESSAGE: usize = 4000;

/// Longest partial line held while waiting for its end. Longer lines (e.g.
/// a huge event) are dropped from display with a note; stdout.log keeps them.
const PENDING_LIMIT: usize = 4 << 20;

/// Splits streamed output into whole lines and formats each one. Partial
/// lines wait for the rest of their chunk. A carriage return also ends a
/// line, so progress bars that redraw with \r do not pile up; JSON events
/// never contain a raw one.
#[derive(Debug)]
pub struct Formatter {
    kind: OutputKind,
    pending: String,
    /// Inside a line longer than `PENDING_LIMIT`, until its end.
    skipping: bool,
}

impl Formatter {
    pub fn new(kind: OutputKind) -> Self {
        Self {
            kind,
            pending: String::new(),
            skipping: false,
        }
    }

    /// Formatted text for the complete lines in `chunk`, or None when they
    /// carried nothing worth showing. Each chunk is scanned once.
    pub fn push(&mut self, mut chunk: &str) -> Option<String> {
        let mut out = Vec::new();
        if self.skipping {
            let pos = chunk.find(['\n', '\r'])?;
            self.skipping = false;
            out.push(format!("… line longer than {PENDING_LIMIT} bytes omitted"));
            chunk = &chunk[pos + 1..];
        }
        let Some(end) = chunk.rfind(['\n', '\r']) else {
            self.hold(chunk);
            return join(out.into_iter());
        };
        let tail = &chunk[end + 1..];
        let complete = &chunk[..end];
        if self.pending.is_empty() {
            for line in complete.split(['\n', '\r']) {
                push_described(&mut out, line, self.kind);
            }
        } else {
            let mut prefix = std::mem::take(&mut self.pending);
            match complete.find(['\n', '\r']) {
                Some(first_break) => {
                    let (head, rest) = (&complete[..first_break], &complete[first_break + 1..]);
                    if prefix.len() + head.len() > PENDING_LIMIT {
                        out.push(format!("… line longer than {PENDING_LIMIT} bytes omitted"));
                    } else {
                        prefix.push_str(head);
                        out.extend(describe(&prefix, self.kind));
                    }
                    for line in rest.split(['\n', '\r']) {
                        push_described(&mut out, line, self.kind);
                    }
                }
                None => {
                    if prefix.len() + complete.len() > PENDING_LIMIT {
                        out.push(format!("… line longer than {PENDING_LIMIT} bytes omitted"));
                    } else {
                        prefix.push_str(complete);
                        out.extend(describe(&prefix, self.kind));
                    }
                }
            }
        }
        self.hold(tail);
        join(out.into_iter())
    }

    fn hold(&mut self, text: &str) {
        if self.skipping {
            return;
        }
        if self.pending.len() + text.len() > PENDING_LIMIT {
            self.pending.clear();
            self.skipping = true;
        } else {
            self.pending.push_str(text);
        }
    }

    /// Formats whatever the harness left without a final newline.
    pub fn finish(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.pending);
        self.skipping = false;
        join(rest.lines().flat_map(|line| describe(line, self.kind)))
    }
}

fn push_described(out: &mut Vec<String>, line: &str, kind: OutputKind) {
    if line.len() > PENDING_LIMIT {
        out.push(format!("… line longer than {PENDING_LIMIT} bytes omitted"));
    } else {
        out.extend(describe(line, kind));
    }
}

fn join(lines: impl Iterator<Item = String>) -> Option<String> {
    let text: Vec<String> = lines.collect();
    (!text.is_empty()).then(|| text.join("\n") + "\n")
}

/// Display lines for one line of harness output. Non-JSON lines pass through;
/// events without user-facing content (deltas, hooks, bookkeeping) are dropped.
pub fn describe(line: &str, kind: OutputKind) -> Vec<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return vec![];
    }
    if kind == OutputKind::Text || kind == OutputKind::Gemini || !trimmed.starts_with('{') {
        return vec![storage::clean_terminal(line)];
    }
    let Ok(event) = serde_json::from_str::<Value>(trimmed) else {
        return vec![storage::clean_terminal(line)];
    };
    let lines = match kind {
        OutputKind::Codex => codex(&event),
        OutputKind::Claude | OutputKind::Amp => claude(&event),
        OutputKind::Cursor => cursor(&event),
        OutputKind::Pi => pi(&event),
        OutputKind::OpenCode => opencode(&event),
        OutputKind::Text | OutputKind::Gemini => unreachable!(),
    };
    lines
        .into_iter()
        .flat_map(|text| {
            storage::clean_terminal(&text)
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

fn str_at<'a>(value: &'a Value, pointer: &str) -> &'a str {
    value.pointer(pointer).and_then(Value::as_str).unwrap_or("")
}

/// One-line excerpt: whitespace collapsed, at most `max` characters.
fn excerpt(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(max.min(text.len()) + 4);
    let mut count = 0usize;
    let mut words = text.split_whitespace();
    let Some(first) = words.next() else {
        return out;
    };
    for word in std::iter::once(first).chain(words) {
        if count > 0 {
            if count == max {
                out.push('…');
                return out;
            }
            out.push(' ');
            count += 1;
        }
        for c in word.chars() {
            if count == max {
                out.push('…');
                return out;
            }
            out.push(c);
            count += 1;
        }
    }
    out
}

fn message(prefix: &str, text: &str) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return vec![];
    }
    let text = storage::truncate(text, MESSAGE);
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("");
    std::iter::once(format!("{prefix} {first}"))
        .chain(lines.map(|line| format!("  {line}")))
        .collect()
}

fn error(text: &str) -> Vec<String> {
    vec![format!("✗ {}", excerpt(text, 600))]
}

/// The arguments that identify a tool call: a command, path, pattern, or
/// URL when present, otherwise compact JSON.
fn arguments(input: &Value) -> String {
    for key in [
        "command",
        "cmd",
        "file_path",
        "filePath",
        "path",
        "absolute_path",
        "pattern",
        "query",
        "url",
        "description",
        "prompt",
    ] {
        if let Some(value) = input.get(key).and_then(Value::as_str) {
            return excerpt(value, EXCERPT);
        }
    }
    match input {
        Value::Null => String::new(),
        Value::Object(map) if map.is_empty() => String::new(),
        other => excerpt(&other.to_string(), EXCERPT),
    }
}

fn tool(name: &str, input: &Value) -> String {
    let args = arguments(input);
    if args.is_empty() {
        format!("⚙ {name}")
    } else {
        format!("⚙ {name} {args}")
    }
}

/// A tool result: its first non-empty line, and how many more there were.
fn result(text: &str, failed: bool) -> String {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines
        .next()
        .map(|l| excerpt(l, EXCERPT))
        .unwrap_or_default();
    let more = lines.count();
    let more = if more > 0 {
        format!(" (+{more} lines)")
    } else {
        String::new()
    };
    match (failed, first.is_empty()) {
        (true, _) => format!("  ↳ ✗ {first}{more}"),
        (false, true) => "  ↳ (no output)".into(),
        (false, false) => format!("  ↳ {first}{more}"),
    }
}

/// Text of a content field: a string, or text blocks.
fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn tokens(input: Option<u64>, output: Option<u64>) -> String {
    match (input, output) {
        (Some(i), Some(o)) => format!(" · in {i} · out {o} tokens"),
        _ => String::new(),
    }
}

fn codex(event: &Value) -> Vec<String> {
    let item = event.get("item").unwrap_or(&Value::Null);
    let item_type = str_at(item, "/type");
    match (str_at(event, "/type"), item_type) {
        ("thread.started", _) => vec![format!("session {}", str_at(event, "/thread_id"))],
        ("item.started", "command_execution") => {
            vec![format!("$ {}", excerpt(str_at(item, "/command"), EXCERPT))]
        }
        ("item.completed", "command_execution") => {
            let code = item.get("exit_code").and_then(Value::as_i64);
            let output = str_at(item, "/aggregated_output");
            let mut line = result(output, code.is_some_and(|c| c != 0));
            if let Some(code) = code.filter(|c| *c != 0) {
                line.push_str(&format!(" · exit {code}"));
            }
            vec![line]
        }
        ("item.completed", "agent_message") => message("●", str_at(item, "/text")),
        ("item.completed", "reasoning") => message("∴", &excerpt(str_at(item, "/text"), 400)),
        ("item.completed", "file_change") => item
            .get("changes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|c| format!("✎ {} {}", str_at(c, "/kind"), str_at(c, "/path")))
            .collect(),
        ("item.started", "mcp_tool_call") => vec![format!(
            "⚙ {}.{} {}",
            str_at(item, "/server"),
            str_at(item, "/tool"),
            arguments(item.get("arguments").unwrap_or(&Value::Null))
        )],
        ("item.started", "web_search") => vec![format!("⌕ {}", str_at(item, "/query"))],
        ("item.completed" | "item.updated", "todo_list") => {
            let items = item.get("items").and_then(Value::as_array);
            let total = items.map_or(0, Vec::len);
            let done = items.map_or(0, |items| {
                items
                    .iter()
                    .filter(|i| i.get("completed").and_then(Value::as_bool) == Some(true))
                    .count()
            });
            vec![format!("☐ plan {done}/{total} done")]
        }
        ("item.completed", "error") => error(str_at(item, "/message")),
        ("turn.completed", _) => vec![format!(
            "turn done{}",
            tokens(
                event.pointer("/usage/input_tokens").and_then(Value::as_u64),
                event
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
            )
        )],
        ("turn.failed", _) => error(str_at(event, "/error/message")),
        ("error", _) => error(str_at(event, "/message")),
        _ => vec![],
    }
}

/// Claude Code and Amp stream JSON.
fn claude(event: &Value) -> Vec<String> {
    match (str_at(event, "/type"), str_at(event, "/subtype")) {
        ("system", "init") => {
            let model = str_at(event, "/model");
            vec![format!(
                "session {}{}",
                str_at(event, "/session_id"),
                if model.is_empty() {
                    String::new()
                } else {
                    format!(" · {model}")
                }
            )]
        }
        ("assistant", _) => blocks(event.pointer("/message/content")),
        // Tool results; the user's own prompt is not repeated.
        ("user", _) => event
            .pointer("/message/content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|b| str_at(b, "/type") == "tool_result")
            .map(|b| {
                result(
                    &content_text(b.get("content").unwrap_or(&Value::Null)),
                    b.get("is_error").and_then(Value::as_bool) == Some(true),
                )
            })
            .collect(),
        ("result", _) => finished(event),
        _ => vec![],
    }
}

fn blocks(content: Option<&Value>) -> Vec<String> {
    content
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|block| match str_at(block, "/type") {
            "text" => message("●", str_at(block, "/text")),
            "thinking" => message("∴", &excerpt(str_at(block, "/thinking"), 400)),
            "tool_use" => vec![tool(
                str_at(block, "/name"),
                block.get("input").unwrap_or(&Value::Null),
            )],
            _ => vec![],
        })
        .collect()
}

/// The final result event of Claude Code, Amp, and Cursor.
fn finished(event: &Value) -> Vec<String> {
    if event.get("is_error").and_then(Value::as_bool) == Some(true) {
        let text = str_at(event, "/result");
        return error(if text.is_empty() {
            "Harness reported an error"
        } else {
            text
        });
    }
    let mut line = "✓ finished".to_owned();
    if let Some(ms) = event.get("duration_ms").and_then(Value::as_u64) {
        line.push_str(&format!(" · {}s", ms / 1000));
    }
    if let Some(cost) = event.get("total_cost_usd").and_then(Value::as_f64) {
        line.push_str(&format!(" · ${cost:.2}"));
    }
    vec![line]
}

fn cursor(event: &Value) -> Vec<String> {
    if str_at(event, "/type") != "tool_call" {
        return claude(event);
    }
    // {"tool_call": {"readToolCall": {"args": …, "result": …}}}
    let Some((name, call)) = event
        .get("tool_call")
        .and_then(Value::as_object)
        .and_then(|calls| calls.iter().next())
    else {
        return vec![];
    };
    let name = name.trim_end_matches("ToolCall");
    match str_at(event, "/subtype") {
        "started" => vec![tool(name, call.get("args").unwrap_or(&Value::Null))],
        "completed" => {
            let outcome = call.get("result").unwrap_or(&Value::Null);
            if let Some(failure) = outcome.get("error").or_else(|| outcome.get("failure")) {
                let text = failure
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| failure.to_string());
                vec![result(&text, true)]
            } else {
                let success = outcome.get("success").unwrap_or(outcome);
                let text = ["content", "stdout", "output"]
                    .iter()
                    .find_map(|k| success.get(*k).and_then(Value::as_str))
                    .unwrap_or("");
                vec![result(text, false)]
            }
        }
        _ => vec![],
    }
}

fn pi(event: &Value) -> Vec<String> {
    match str_at(event, "/type") {
        "session" => vec![format!("session {}", str_at(event, "/id"))],
        "message_end" if str_at(event, "/message/role") == "assistant" => {
            let content = event.pointer("/message/content");
            let mut lines: Vec<String> = content
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .flat_map(|block| match str_at(block, "/type") {
                    "text" => message("●", str_at(block, "/text")),
                    "thinking" => message("∴", &excerpt(str_at(block, "/thinking"), 400)),
                    _ => vec![],
                })
                .collect();
            let reason = str_at(event, "/message/stopReason");
            if reason == "error" || reason == "aborted" {
                lines.extend(error(str_at(event, "/message/errorMessage")));
            }
            lines
        }
        "tool_execution_start" => vec![tool(
            str_at(event, "/toolName"),
            event.get("args").unwrap_or(&Value::Null),
        )],
        "tool_execution_end" => vec![result(
            &content_text(event.pointer("/result/content").unwrap_or(&Value::Null)),
            event.get("isError").and_then(Value::as_bool) == Some(true),
        )],
        "auto_retry_end" if event.get("success").and_then(Value::as_bool) == Some(false) => {
            error(str_at(event, "/finalError"))
        }
        // The last assistant message says whether the run ended in an error,
        // which its own message_end already showed.
        "agent_end" => {
            let failed = event
                .get("messages")
                .and_then(Value::as_array)
                .and_then(|m| m.iter().rev().find(|m| str_at(m, "/role") == "assistant"))
                .is_some_and(|m| matches!(str_at(m, "/stopReason"), "error" | "aborted"));
            if failed {
                vec![]
            } else {
                vec!["✓ finished".into()]
            }
        }
        _ => vec![],
    }
}

fn opencode(event: &Value) -> Vec<String> {
    let part = event.get("part").unwrap_or(&Value::Null);
    match str_at(event, "/type") {
        "text" => message("●", str_at(part, "/text")),
        "reasoning" => message("∴", &excerpt(str_at(part, "/text"), 400)),
        "tool_use" => {
            let state = part.get("state").unwrap_or(&Value::Null);
            let mut lines = vec![tool(
                str_at(part, "/tool"),
                state.get("input").unwrap_or(&Value::Null),
            )];
            match str_at(state, "/status") {
                "completed" => lines.push(result(str_at(state, "/output"), false)),
                "error" => lines.push(result(str_at(state, "/error"), true)),
                _ => {}
            }
            lines
        }
        "step_finish" => vec![format!(
            "step done{}",
            tokens(
                part.pointer("/tokens/input").and_then(Value::as_u64),
                part.pointer("/tokens/output").and_then(Value::as_u64)
            )
        )],
        "error" => error(
            &event
                .pointer("/error/data/message")
                .or_else(|| event.pointer("/error/message"))
                .and_then(Value::as_str)
                .map_or_else(|| event.to_string(), str::to_owned),
        ),
        _ => vec![],
    }
}

// Structured JSONL metrics export ---------------------------------------------

/// Environment variable naming a JSONL file to append structured run and
/// stage-attempt metrics to (for CI, dashboards, or OTLP collectors).
pub const METRICS_FILE_ENV: &str = "MODEL_ROUTER_METRICS_FILE";
pub const METRICS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricRecord {
    pub schema_version: u32,
    /// `"run_start"`, `"run_end"`, `"attempt_start"`, or `"attempt_end"`.
    pub event: String,
    pub timestamp: DateTime<Utc>,
    pub run_id: String,
    pub workflow: String,
    pub project: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

fn metrics_file_slot() -> &'static Mutex<Option<PathBuf>> {
    static PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    PATH.get_or_init(|| Mutex::new(None))
}

fn metrics_write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Sets (or clears) the process-wide metrics JSONL path (`--metrics-file`).
pub fn set_metrics_file(path: Option<PathBuf>) {
    if let Ok(mut slot) = metrics_file_slot().lock() {
        *slot = path;
    }
}

/// Effective metrics JSONL path: explicit `--metrics-file` wins over
/// `MODEL_ROUTER_METRICS_FILE`.
pub fn metrics_file() -> Option<PathBuf> {
    if let Some(path) = metrics_file_slot()
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
    {
        return Some(path);
    }
    std::env::var_os(METRICS_FILE_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Appends JSONL metric records safely under an in-process mutex and (on Unix)
/// an exclusive `flock` on the target file, writing the complete batch in one
/// `O_APPEND` write so concurrent processes never interleave partial lines.
pub fn append_metrics(path: &Path, records: &[MetricRecord]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let mut payload = Vec::new();
    for record in records {
        serde_json::to_writer(&mut payload, record)?;
        payload.push(b'\n');
    }
    let _guard = metrics_write_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Create directory for {}", path.display()))?;
        }
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("Open metrics file {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        loop {
            // SAFETY: `file` is a valid open file descriptor.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err).with_context(|| format!("Lock metrics file {}", path.display()));
            }
        }
    }
    let write_res = file.write_all(&payload).and_then(|()| file.flush());
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: `file` is still open and locked by us.
        unsafe {
            libc::flock(file.as_raw_fd(), libc::LOCK_UN);
        }
    }
    write_res.with_context(|| format!("Write metrics file {}", path.display()))
}

fn run_usage(run: &crate::engine::Run) -> crate::handoff::Usage {
    let mut total = crate::handoff::Usage::default();
    for state in &run.stages {
        let candidates = state.candidate_attempts().map(|(_, a)| a);
        for attempt in state.attempts.iter().chain(candidates) {
            if let Some(usage) = attempt.usage {
                total = total + usage;
            }
        }
        for (_, turn) in state.task_turns() {
            if let Some(usage) = turn.usage {
                total = total + usage;
            }
        }
    }
    total
}

fn is_terminal(status: crate::engine::Status) -> bool {
    matches!(
        status,
        crate::engine::Status::Complete
            | crate::engine::Status::Failed
            | crate::engine::Status::Blocked
            | crate::engine::Status::Cancelled
    )
}

/// Builds structured metric records for a `Run::persist()` commit.
pub fn collect_run_metrics(
    run: &crate::engine::Run,
    started: bool,
    records: &[crate::transitions::Record],
) -> Vec<MetricRecord> {
    use crate::{cost, engine::Status, transitions::Level};
    let mut out = Vec::new();
    let base = |event: &str, timestamp: DateTime<Utc>, status: &str| MetricRecord {
        schema_version: METRICS_SCHEMA_VERSION,
        event: event.into(),
        timestamp,
        run_id: run.id.clone(),
        workflow: run.workflow.name.clone(),
        project: run.project.display().to_string(),
        status: status.into(),
        stage_id: None,
        stage_name: None,
        attempt: None,
        candidate: None,
        harness: None,
        model: None,
        role: None,
        mode: None,
        duration_ms: None,
        tokens: None,
        input_tokens: None,
        output_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        estimated_usd: None,
        exit_code: None,
    };
    if started {
        out.push(base("run_start", run.created_at, run.status.as_str()));
    }
    let mut pricing: Option<cost::Pricing> = None;
    // A run that stopped and resumed in the same commit (an automatic retry
    // after a transient failure) has not ended: only the run's last status in
    // the commit can end it.
    let last_run = records.iter().rposition(|r| r.level == Level::Run);
    for (position, rec) in records.iter().enumerate() {
        match rec.level {
            Level::Attempt if rec.to == Status::Running => {
                let Some(idx) = rec.stage_index else {
                    continue;
                };
                let Some(stage) = run.workflow.stages.get(idx) else {
                    continue;
                };
                let Some(state) = run.stages.get(idx) else {
                    continue;
                };
                let (harness, attempt_opt) = if let Some(cand_id) = rec.candidate.as_deref() {
                    let cand = state
                        .fan_out
                        .as_ref()
                        .and_then(|f| f.candidates.iter().find(|c| c.id == cand_id));
                    match cand {
                        Some(c) => {
                            let att = rec
                                .attempt
                                .and_then(|n| c.attempts.iter().find(|a| a.number == n))
                                .or_else(|| c.attempts.last());
                            (c.harness.clone(), att)
                        }
                        None => continue,
                    }
                } else {
                    let att = rec
                        .attempt
                        .and_then(|n| state.attempts.iter().find(|a| a.number == n))
                        .or_else(|| state.attempts.last());
                    (stage.harness.clone(), att)
                };
                let mut item = base("attempt_start", rec.at, rec.to.as_str());
                item.stage_id = Some(stage.id.clone());
                item.stage_name = Some(stage.name.clone());
                item.attempt = rec.attempt.or_else(|| attempt_opt.map(|a| a.number));
                item.candidate = rec.candidate.clone();
                item.harness = Some(harness);
                item.model = Some(
                    attempt_opt
                        .map(|a| a.model.clone())
                        .unwrap_or_else(|| stage.model.clone()),
                );
                item.role = Some(stage.role.label().into());
                item.mode = Some(
                    attempt_opt
                        .map(|a| a.mode.label().into())
                        .unwrap_or_else(|| stage.mode.label().into()),
                );
                out.push(item);
            }
            Level::Attempt if is_terminal(rec.to) => {
                let Some(idx) = rec.stage_index else {
                    continue;
                };
                let Some(stage) = run.workflow.stages.get(idx) else {
                    continue;
                };
                let Some(state) = run.stages.get(idx) else {
                    continue;
                };
                let (harness, attempt_opt) = if let Some(cand_id) = rec.candidate.as_deref() {
                    let cand = state
                        .fan_out
                        .as_ref()
                        .and_then(|f| f.candidates.iter().find(|c| c.id == cand_id));
                    match cand {
                        Some(c) => {
                            let att = rec
                                .attempt
                                .and_then(|n| c.attempts.iter().find(|a| a.number == n))
                                .or_else(|| c.attempts.last());
                            (c.harness.clone(), att)
                        }
                        None => continue,
                    }
                } else {
                    let att = rec
                        .attempt
                        .and_then(|n| state.attempts.iter().find(|a| a.number == n))
                        .or_else(|| state.attempts.last());
                    (stage.harness.clone(), att)
                };
                let pricing_ref = pricing.get_or_insert_with(|| cost::Pricing::load(&run.project));
                let mut item = base("attempt_end", rec.at, rec.to.as_str());
                item.stage_id = Some(stage.id.clone());
                item.stage_name = Some(stage.name.clone());
                item.attempt = rec.attempt.or_else(|| attempt_opt.map(|a| a.number));
                item.candidate = rec.candidate.clone();
                item.harness = Some(harness);
                item.role = Some(stage.role.label().into());
                if let Some(att) = attempt_opt {
                    item.model = Some(att.model.clone());
                    item.mode = Some(att.mode.label().into());
                    let ended = att.finished_at.unwrap_or(rec.at);
                    item.duration_ms = Some((ended - att.started_at).num_milliseconds().max(0));
                    item.exit_code = att.exit_code;
                    let estimate = cost::attempt_estimate(run, idx, att, pricing_ref);
                    if let Some(usage) = att.usage {
                        item.tokens = Some(estimate.tokens);
                        item.input_tokens = usage.input_tokens;
                        item.output_tokens = usage.output_tokens;
                        item.cache_read_tokens = usage.cache_read_tokens;
                        item.cache_write_tokens = usage.cache_write_tokens;
                    }
                    item.estimated_usd = estimate.usd;
                } else {
                    item.model = Some(stage.model.clone());
                    item.mode = Some(stage.mode.label().into());
                }
                out.push(item);
            }
            Level::Run if is_terminal(rec.to) && last_run == Some(position) => {
                let pricing_ref = pricing.get_or_insert_with(|| cost::Pricing::load(&run.project));
                let total = cost::run_total(run, pricing_ref);
                let usage = run_usage(run);
                let mut item = base("run_end", rec.at, rec.to.as_str());
                item.duration_ms = Some((rec.at - run.created_at).num_milliseconds().max(0));
                if total.tokens > 0 || usage.input_tokens.is_some() || usage.output_tokens.is_some()
                {
                    item.tokens = Some(total.tokens);
                    item.input_tokens = usage.input_tokens;
                    item.output_tokens = usage.output_tokens;
                    item.cache_read_tokens = usage.cache_read_tokens;
                    item.cache_write_tokens = usage.cache_write_tokens;
                }
                item.estimated_usd = total.known().then_some(total.usd);
                out.push(item);
            }
            _ => {}
        }
    }
    out
}

/// Emits structured JSONL metrics for a persisted run commit if a metrics file
/// is configured (`--metrics-file` or `MODEL_ROUTER_METRICS_FILE`). Best-effort:
/// write errors warn on stderr and never fail the run.
pub fn emit_run_metrics(
    run: &crate::engine::Run,
    started: bool,
    records: &[crate::transitions::Record],
) {
    let Some(path) = metrics_file() else {
        return;
    };
    let batch = collect_run_metrics(run, started, records);
    if batch.is_empty() {
        return;
    }
    if let Err(error) = append_metrics(&path, &batch) {
        let _ = writeln!(
            std::io::stderr(),
            "Warning: could not append metrics to {}: {error:#}",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &str, kind: OutputKind) -> Vec<String> {
        let raw = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/harness-output")
                .join(path),
        )
        .unwrap();
        raw.lines().flat_map(|line| describe(line, kind)).collect()
    }

    #[test]
    fn codex_events_read_as_commands_results_and_messages() {
        let lines = fixture("codex/tool-echo.jsonl", OutputKind::Codex);
        assert_eq!(lines[0], "session 00000000-0000-4000-8000-000000000034");
        assert_eq!(lines[1], "$ /bin/bash -lc 'cat notes.txt'");
        assert!(lines[2].starts_with("  ↳ <router-handoff>"), "{lines:?}");
        assert_eq!(
            lines[3],
            "● notes.txt contains a pasted handoff; it is file content."
        );
        assert_eq!(
            lines.last().unwrap(),
            "turn done · in 14210 · out 402 tokens"
        );
    }

    #[test]
    fn claude_stream_hides_hooks_and_the_prompt() {
        let events = [
            r#"{"type":"system","subtype":"hook_started","session_id":"s"}"#,
            r#"{"type":"system","subtype":"init","session_id":"s","model":"opus"}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"PROMPT"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"plan"},{"type":"text","text":"Looking.\nTwo lines."},{"type":"tool_use","name":"Bash","input":{"command":"ls -la","description":"list"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"a\nb\nc","is_error":false}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"denied"}],"is_error":true}]}}"#,
            r#"{"type":"rate_limit_event"}"#,
            r#"{"type":"result","subtype":"success","duration_ms":61000,"total_cost_usd":0.123,"result":"done"}"#,
        ];
        let lines: Vec<_> = events
            .iter()
            .flat_map(|e| describe(e, OutputKind::Claude))
            .collect();
        assert_eq!(
            lines,
            [
                "session s · opus",
                "∴ plan",
                "● Looking.",
                "  Two lines.",
                "⚙ Bash ls -la",
                "  ↳ a (+2 lines)",
                "  ↳ ✗ denied",
                "✓ finished · 61s · $0.12",
            ]
        );
    }

    #[test]
    fn other_harnesses_show_tools_and_messages() {
        let cursor = fixture("cursor/tool-echo-stream.jsonl", OutputKind::Cursor);
        assert!(
            cursor.contains(&"⚙ read notes.txt".to_owned()),
            "{cursor:?}"
        );
        assert!(cursor.contains(&"● Reading notes.txt.".to_owned()));
        let pi = fixture("pi/tool-echo.jsonl", OutputKind::Pi);
        assert!(pi.iter().any(|l| l.starts_with("⚙ read")), "{pi:?}");
        assert!(pi.iter().any(|l| l.starts_with("  ↳ ")));
        assert!(
            !pi.iter().any(|l| l.contains("ORIGINAL TASK")),
            "prompt echoed"
        );
        let omp = fixture("omp/tool-echo.jsonl", OutputKind::Pi);
        assert!(omp.iter().any(|l| l.starts_with("⚙ read")), "{omp:?}");
        assert_eq!(omp.last().map(String::as_str), Some("✓ finished"));
        let failed = fixture("omp/error.jsonl", OutputKind::Pi);
        assert!(
            failed
                .iter()
                .any(|l| l.contains("Insufficient account funds"))
                && !failed.iter().any(|l| l.starts_with("✓")),
            "{failed:?}"
        );
        let opencode = fixture("opencode/tool-echo.jsonl", OutputKind::OpenCode);
        assert!(
            opencode
                .iter()
                .any(|l| l == "⚙ read /workspace/project/notes.txt"),
            "{opencode:?}"
        );
        let amp = fixture("amp/success.jsonl", OutputKind::Amp);
        assert!(amp[0].starts_with("session T-"), "{amp:?}");
        assert!(!amp.iter().any(|l| l.contains("ORIGINAL TASK")));
    }

    #[test]
    fn partial_lines_wait_and_plain_text_passes_through() {
        let mut formatter = Formatter::new(OutputKind::Codex);
        assert_eq!(formatter.push(r#"{"type":"thread.st"#), None);
        assert_eq!(
            formatter.push("arted\",\"thread_id\":\"t\"}\n{\"type\":\"turn.started\"}\nplain"),
            Some("session t\n".into())
        );
        assert_eq!(formatter.finish(), Some("plain\n".into()));
        assert_eq!(describe("not json", OutputKind::Claude), ["not json"]);
    }

    #[test]
    fn carriage_returns_end_lines_and_overlong_lines_stay_bounded() {
        let mut formatter = Formatter::new(OutputKind::Text);
        assert_eq!(formatter.push("10%\r50%\r"), Some("10%\n50%\n".into()));
        assert!(formatter.pending.is_empty());
        let chunk = "x".repeat(64 * 1024);
        for _ in 0..(PENDING_LIMIT / chunk.len() + 8) {
            assert_eq!(formatter.push(&chunk), None);
            assert!(formatter.pending.len() <= PENDING_LIMIT);
        }
        let shown = formatter.push("tail\nnext\n").unwrap();
        assert!(shown.starts_with("… line longer than"), "{shown}");
        assert!(shown.ends_with("next\n") && !shown.contains("tail"));
        assert_eq!(formatter.push("after\n"), Some("after\n".into()));
        // A single oversized line ending with a newline is also dropped before parse.
        let oversized = format!("{{\"msg\":\"{}\"}}\nok\n", "a".repeat(PENDING_LIMIT + 1));
        let dropped = formatter.push(&oversized).unwrap();
        assert!(dropped.starts_with("… line longer than"), "{dropped}");
        assert!(dropped.ends_with("ok\n"));
        assert_eq!(excerpt("  one   two\tthree  ", 7), "one two…");
        assert_eq!(excerpt("🦀🦀🦀", 3), "🦀🦀🦀");
        assert_eq!(excerpt("🦀🦀🦀🦀", 3), "🦀🦀🦀…");
    }

    #[test]
    fn append_metrics_writes_valid_jsonl_under_concurrent_writers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("metrics.jsonl");
        let threads = 4usize;
        let per_thread = 25usize;
        std::thread::scope(|scope| {
            for t in 0..threads {
                let path = path.clone();
                scope.spawn(move || {
                    for i in 0..per_thread {
                        let rec = MetricRecord {
                            schema_version: METRICS_SCHEMA_VERSION,
                            event: "attempt_end".into(),
                            timestamp: Utc::now(),
                            run_id: format!("run-{t}-{i}"),
                            workflow: "wf".into(),
                            project: "/proj".into(),
                            status: "complete".into(),
                            stage_id: Some("implement".into()),
                            stage_name: Some("Implement".into()),
                            attempt: Some(1),
                            candidate: None,
                            harness: Some("codex".into()),
                            model: Some("gpt-5".into()),
                            role: Some("implementation".into()),
                            mode: Some("unattended".into()),
                            duration_ms: Some(1234),
                            tokens: Some(500),
                            input_tokens: Some(400),
                            output_tokens: Some(100),
                            cache_read_tokens: None,
                            cache_write_tokens: None,
                            estimated_usd: Some(0.01),
                            exit_code: Some(0),
                        };
                        append_metrics(&path, &[rec]).unwrap();
                    }
                });
            }
        });
        let raw = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), threads * per_thread);
        for line in lines {
            let parsed: MetricRecord = serde_json::from_str(line).unwrap();
            assert_eq!(parsed.schema_version, METRICS_SCHEMA_VERSION);
            assert_eq!(parsed.event, "attempt_end");
            assert_eq!(parsed.exit_code, Some(0));
        }
    }
}
