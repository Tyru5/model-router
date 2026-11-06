use crate::{adapters::OutputKind, config::Role, storage};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Complete,
    Pass,
    NeedsChanges,
    Blocked,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocking,
    Advisory,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub description: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    Passed,
    Failed,
    NotRun,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestEvidence {
    pub command: String,
    pub status: TestStatus,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handoff {
    pub summary: String,
    pub verdict: Verdict,
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub files_changed: Vec<String>,
    #[serde(default)]
    pub tests: Vec<TestEvidence>,
    #[serde(default)]
    pub unresolved: Vec<String>,
    /// Why no verification check can apply; a test stage then needs user acceptance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_applicable: Option<String>,
    #[serde(default = "yes")]
    pub structured: bool,
    /// The fan-out candidate a judge stage chose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
}
fn yes() -> bool {
    true
}
impl Handoff {
    pub fn requires_repair(&self) -> bool {
        self.verdict == Verdict::NeedsChanges
            || self
                .findings
                .iter()
                .any(|f| f.severity == Severity::Blocking)
            || self
                .tests
                .iter()
                .any(|t| t.required && t.status == TestStatus::Failed)
    }
    pub fn has_passing_required_check(&self) -> bool {
        self.tests
            .iter()
            .any(|t| t.required && t.status == TestStatus::Passed)
    }
    pub fn not_applicable_reason(&self) -> Option<&str> {
        self.not_applicable
            .as_deref()
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
    }
}

pub const FORMAT: &str = r#"<router-handoff>
{"summary":"What was established or changed","verdict":"complete","findings":[],"decisions":[],"files_changed":[],"tests":[{"command":"check actually run","status":"passed","required":true}],"unresolved":[]}
</router-handoff>
Use complete for normal successful stages. Reviews MUST use pass or needs_changes.
Test stages MUST return a structured handoff reporting each check actually run in tests.
Only if no check can apply, a test stage may set "not_applicable":"reason"; the user must accept it.
Use blocked if you cannot complete the task (including missing permission).
Findings use {"severity":"blocking" or "advisory","description":"specific evidence"}.
Tests use status passed, failed, or not_run. Never claim a test ran if it did not.
"#;

pub fn parse(text: &str, role: Role) -> Result<Handoff> {
    let cleaned = storage::clean_terminal(text);
    let text = cleaned.trim();
    let candidate = if let Some((_, rest)) = text.rsplit_once("<router-handoff>") {
        rest.split_once("</router-handoff>")
            .context("Incomplete handoff marker")?
            .0
            .trim()
    } else if let Some(rest) = text.strip_prefix("```json") {
        rest.trim().strip_suffix("```").unwrap_or(rest).trim()
    } else {
        text
    };
    if let Ok(mut result) = serde_json::from_str::<Handoff>(candidate) {
        ensure!(
            !result.summary.trim().is_empty(),
            "Handoff summary is empty"
        );
        if role == Role::Review {
            ensure!(
                matches!(
                    result.verdict,
                    Verdict::Pass | Verdict::NeedsChanges | Verdict::Blocked
                ),
                "Review requires an explicit pass, needs_changes, or blocked verdict"
            );
        }
        result.structured = true;
        return Ok(result);
    }
    if role == Role::Review {
        bail!(
            "Review did not return a valid structured verdict. Reopen or retry it; it has not passed"
        );
    }
    if role == Role::Test {
        bail!(
            "Test stage did not return a valid structured handoff. Reopen or retry it with test evidence; verification has not passed"
        );
    }
    ensure!(
        !text.contains("<router-handoff>"),
        "The stage returned a malformed structured handoff; inspect it and retry"
    );
    ensure!(
        !text.is_empty(),
        "Harness exited without a handoff or final response. Reopen the stage to complete it"
    );
    // Ordinary stage output can still be useful. Its lack of structure stays visible.
    Ok(Handoff {
        summary: storage::truncate(text, 12000),
        verdict: Verdict::Complete,
        findings: vec![],
        decisions: vec![],
        files_changed: vec![],
        tests: vec![],
        unresolved: vec![],
        not_applicable: None,
        structured: false,
        selected: None,
    })
}

#[derive(Debug, Default)]
pub struct Normalized {
    pub text: String,
    pub error: Option<ProtocolError>,
    pub permission_blocked: bool,
}

/// Why a harness's structured output is unusable. Reads as its message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// The harness reported an error event or result.
    Reported(String),
    /// A native structured stream with no assistant message. A handoff file
    /// written by the harness may still stand in for it.
    NoFinalResponse,
}
impl std::ops::Deref for ProtocolError {
    type Target = str;
    fn deref(&self) -> &str {
        match self {
            Self::Reported(message) => message,
            Self::NoFinalResponse => "No final assistant response found in harness output",
        }
    }
}
impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self)
    }
}
impl From<&str> for ProtocolError {
    fn from(message: &str) -> Self {
        Self::Reported(message.into())
    }
}

/// The harness session an unattended log reports: Codex's `thread.started`
/// thread, Pi's `session` event, OpenCode's `sessionID`, or the `session_id`
/// on Claude Code, Cursor, and Amp stream events.
pub fn session_id(raw: &str) -> Option<String> {
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .find_map(|event| {
            let fields: &[&str] = match event.get("type").and_then(Value::as_str) {
                Some("thread.started") => &["thread_id"],
                Some("session") => &["id"],
                _ => &["session_id", "sessionID"],
            };
            fields
                .iter()
                .find_map(|field| event.get(*field).and_then(Value::as_str))
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
        })
}

/// Only assistant/final-response fields become handoffs. Tool arguments and
/// echoed prompts must never become a review verdict.
pub fn normalize(raw: &str, kind: OutputKind) -> Normalized {
    if kind == OutputKind::Text {
        return Normalized {
            text: storage::clean_terminal(raw),
            ..Normalized::default()
        };
    }
    let cleaned = storage::clean_terminal(raw);
    let events: Vec<Value> = match serde_json::from_str(&cleaned) {
        // Claude prints every message as one array when verbose output is on.
        Ok(Value::Array(events)) => events,
        Ok(value) => vec![value],
        Err(_) => cleaned
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect(),
    };
    let mut result = Normalized::default();
    let mut recognized = false;
    // OpenCode: the message whose text parts form the handoff.
    let mut message_id: Option<String> = None;
    for event in events {
        if event.get("is_error").and_then(Value::as_bool) == Some(true) {
            // Claude's error results carry `errors` instead of `result`.
            let errors: Vec<&str> = event
                .get("errors")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            result.error = Some(ProtocolError::Reported(
                event
                    .get("result")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| (!errors.is_empty()).then(|| errors.join("\n")))
                    .unwrap_or_else(|| "Harness reported an error".into()),
            ));
        }
        if event
            .get("permission_denials")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
        {
            result.permission_blocked = true;
        }
        let kind_name = event.get("type").and_then(Value::as_str).unwrap_or("");
        let failed = kind_name == "error"
            || kind_name == "turn.failed"
            || event.get("error").is_some_and(|v| !v.is_null());
        if failed {
            result.error = Some(ProtocolError::Reported(storage::truncate(
                &event.to_string(),
                1500,
            )));
        }
        // Retries report errors (Claude `api_retry`, provider errors) before
        // the run succeeds; a later successful final event supersedes them.
        match kind {
            OutputKind::Claude | OutputKind::Cursor | OutputKind::Amp => {
                if kind_name == "result" {
                    if !failed && event.get("is_error").and_then(Value::as_bool) != Some(true) {
                        result.error = None;
                    }
                    if let Some(text) = event.get("result").and_then(Value::as_str) {
                        result.text = text.into();
                        recognized = true;
                    }
                } else if kind_name == "assistant" {
                    if let Some(content) =
                        event.pointer("/message/content").and_then(Value::as_array)
                    {
                        for block in content {
                            if block.get("type").and_then(Value::as_str) == Some("text") {
                                if let Some(text) = block.get("text").and_then(Value::as_str) {
                                    result.text.push_str(text);
                                    result.text.push('\n');
                                    recognized = true;
                                }
                            }
                        }
                    }
                }
            }
            OutputKind::Gemini => {
                if let Some(text) = event.get("response").and_then(Value::as_str) {
                    result.text = text.into();
                    recognized = true;
                }
                // JSON output carries `response`; stream-json ends with a result.
                let succeeded = event.get("response").is_some_and(Value::is_string)
                    || (kind_name == "result"
                        && event.get("status").and_then(Value::as_str) == Some("success"));
                if succeeded && !failed {
                    result.error = None;
                }
            }
            OutputKind::Codex => {
                if kind_name == "item.completed"
                    && event.pointer("/item/type").and_then(Value::as_str) == Some("agent_message")
                {
                    if let Some(text) = event.pointer("/item/text").and_then(Value::as_str) {
                        result.text = text.into();
                        recognized = true;
                    }
                }
                // Stream retries ("Reconnecting...") arrive as error events; a
                // fatal error ends the turn with turn.failed instead.
                if kind_name == "turn.completed" {
                    result.error = None;
                }
            }
            OutputKind::Pi => {
                if kind_name == "message_end"
                    && event.pointer("/message/role").and_then(Value::as_str) == Some("assistant")
                {
                    // The latest assistant message decides: Pi retries failed
                    // attempts automatically.
                    result.error = matches!(
                        event.pointer("/message/stopReason").and_then(Value::as_str),
                        Some("error" | "aborted")
                    )
                    .then(|| {
                        event
                            .pointer("/message/errorMessage")
                            .and_then(Value::as_str)
                            .unwrap_or("Pi reported an error")
                            .into()
                    });
                    if let Some(content) =
                        event.pointer("/message/content").and_then(Value::as_array)
                    {
                        result.text = content
                            .iter()
                            .filter(|v| v.get("type").and_then(Value::as_str) == Some("text"))
                            .filter_map(|v| v.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n");
                        recognized = true;
                    }
                }
            }
            OutputKind::OpenCode => {
                if kind_name == "text" {
                    if let Some(text) = event.pointer("/part/text").and_then(Value::as_str) {
                        // Each step is its own message; only the latest one
                        // with text is the final answer.
                        let id = event
                            .pointer("/part/messageID")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        if id != message_id {
                            result.text.clear();
                            message_id = id;
                        }
                        result.text.push_str(text);
                        result.text.push('\n');
                        recognized = true;
                    }
                }
                // A step that finished normally ends the turn after any retry.
                if kind_name == "step_finish"
                    && event.pointer("/part/reason").and_then(Value::as_str) == Some("stop")
                {
                    result.error = None;
                }
            }
            OutputKind::Text => {}
        }
    }
    // A native structured stream with no assistant message isn't a usable result.
    if !recognized && result.error.is_none() {
        result.error = Some(ProtocolError::NoFinalResponse);
    }
    result
}

/// Tokens and cost a harness reported, never estimated: a field is None when
/// the harness did not report it. Input excludes tokens read from cache and
/// output includes reasoning, whatever each harness's own convention.
/// Codex, Cursor, and Gemini report no cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}
impl std::ops::Add for Usage {
    type Output = Self;
    /// Sums what either side reported.
    fn add(self, other: Self) -> Self {
        fn plus<T: std::ops::Add<Output = T>>(a: Option<T>, b: Option<T>) -> Option<T> {
            match (a, b) {
                (Some(a), Some(b)) => Some(a + b),
                (a, b) => a.or(b),
            }
        }
        Self {
            input_tokens: plus(self.input_tokens, other.input_tokens),
            output_tokens: plus(self.output_tokens, other.output_tokens),
            cache_read_tokens: plus(self.cache_read_tokens, other.cache_read_tokens),
            cache_write_tokens: plus(self.cache_write_tokens, other.cache_write_tokens),
            cost_usd: plus(self.cost_usd, other.cost_usd),
        }
    }
}

/// Usage from unattended harness output; None when it reports none.
pub fn usage(raw: &str, kind: OutputKind) -> Option<Usage> {
    json_values(raw).fold(None, |found, event| add_usage(found, &event, kind))
}

fn add_usage(found: Option<Usage>, event: &Value, kind: OutputKind) -> Option<Usage> {
    let Some(part) = event_usage(event, kind) else {
        return found;
    };
    Some(match found {
        Some(sum) if summed(kind) => sum + part,
        _ => part,
    })
}

/// Largest single JSON document (Gemini's output) read whole for usage.
const DOCUMENT_LIMIT: u64 = 16 << 20;

/// Usage from an unattended harness's stdout.log, read one line at a time
/// so a long log is never held in memory. Gemini prints one JSON document,
/// read whole up to `DOCUMENT_LIMIT`; a log with no JSON lines is read the
/// same way, in case it is one pretty-printed document.
pub fn usage_file(path: &std::path::Path, kind: OutputKind) -> Option<Usage> {
    use std::io::{BufRead, Read};
    let document = |path: &std::path::Path| {
        let mut raw = String::new();
        let file = std::fs::File::open(path).ok()?;
        if file.metadata().ok()?.len() > DOCUMENT_LIMIT {
            return None;
        }
        file.take(DOCUMENT_LIMIT)
            .read_to_string(&mut raw)
            .ok()
            .and_then(|_| usage(&raw, kind))
    };
    if kind == OutputKind::Gemini {
        return document(path);
    }
    let mut reader = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let mut line = Vec::new();
    let (mut found, mut any) = (None, false);
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line);
        for event in line_values(&text) {
            any = true;
            found = add_usage(found, &event, kind);
        }
    }
    if any { found } else { document(path) }
}

/// The JSON value on one line of output, after any non-JSON prefix; arrays
/// are flattened.
fn line_values(line: &str) -> Vec<Value> {
    let line = line.trim();
    let value = serde_json::from_str::<Value>(line).ok().or_else(|| {
        let start = line.find('{').filter(|start| *start > 0)?;
        serde_json::from_str(&line[start..]).ok()
    });
    match value {
        Some(Value::Array(items)) => items,
        Some(value) if value.is_object() => vec![value],
        _ => vec![],
    }
}

/// Whether a harness reports usage per turn or step, rather than once for
/// the whole session.
fn summed(kind: OutputKind) -> bool {
    matches!(
        kind,
        OutputKind::Codex | OutputKind::Pi | OutputKind::OpenCode
    )
}

/// JSON values in output: one document, or JSON lines among other text.
/// Arrays, such as Claude's verbose JSON, are flattened.
fn json_values(raw: &str) -> impl Iterator<Item = Value> + '_ {
    let text = raw.trim();
    // Parse from the first brace only when text precedes it; otherwise the
    // second attempt would repeat the first.
    let document = serde_json::from_str::<Value>(text).ok().or_else(|| {
        let start = text.find('{').filter(|start| *start > 0)?;
        serde_json::from_str(&text[start..]).ok()
    });
    let lines = document.is_none().then(|| {
        text.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line.get(line.find('{')?..)?).ok())
    });
    document
        .into_iter()
        .chain(lines.into_iter().flatten())
        .flat_map(|value| match value {
            Value::Array(items) => items,
            value => vec![value],
        })
}

fn event_usage(event: &Value, kind: OutputKind) -> Option<Usage> {
    let count = |pointer: &str| event.pointer(pointer).and_then(Value::as_u64);
    let cost = |pointer: &str| event.pointer(pointer).and_then(Value::as_f64);
    let plus = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        (a, b) => a.or(b),
    };
    let kind_name = event.get("type").and_then(Value::as_str).unwrap_or("");
    let usage = match kind {
        // Claude Code and Amp stream JSON: the final result event.
        OutputKind::Claude | OutputKind::Amp if kind_name == "result" => Usage {
            input_tokens: count("/usage/input_tokens"),
            output_tokens: count("/usage/output_tokens"),
            cache_read_tokens: count("/usage/cache_read_input_tokens"),
            cache_write_tokens: count("/usage/cache_creation_input_tokens"),
            cost_usd: cost("/total_cost_usd"),
        },
        // cursor-agent's result event; its inputTokens already exclude cache.
        OutputKind::Cursor if kind_name == "result" => Usage {
            input_tokens: count("/usage/inputTokens"),
            output_tokens: count("/usage/outputTokens"),
            cache_read_tokens: count("/usage/cacheReadTokens"),
            cache_write_tokens: count("/usage/cacheWriteTokens"),
            cost_usd: None,
        },
        // codex exec --json; input_tokens includes cached_input_tokens.
        OutputKind::Codex if kind_name == "turn.completed" => {
            let cached = count("/usage/cached_input_tokens");
            Usage {
                input_tokens: count("/usage/input_tokens")
                    .map(|n| n.saturating_sub(cached.unwrap_or(0))),
                output_tokens: count("/usage/output_tokens"),
                cache_read_tokens: cached,
                cache_write_tokens: count("/usage/cache_write_input_tokens"),
                cost_usd: None,
            }
        }
        // pi --mode json: each assistant message_end carries pi-ai's Usage.
        OutputKind::Pi
            if kind_name == "message_end"
                && event.pointer("/message/role").and_then(Value::as_str) == Some("assistant") =>
        {
            Usage {
                input_tokens: count("/message/usage/input"),
                output_tokens: count("/message/usage/output"),
                cache_read_tokens: count("/message/usage/cacheRead"),
                cache_write_tokens: count("/message/usage/cacheWrite"),
                cost_usd: cost("/message/usage/cost/total"),
            }
        }
        // opencode run --format json: per-step totals; output excludes reasoning.
        OutputKind::OpenCode if kind_name == "step_finish" => Usage {
            input_tokens: count("/part/tokens/input"),
            output_tokens: plus(
                count("/part/tokens/output"),
                count("/part/tokens/reasoning"),
            ),
            cache_read_tokens: count("/part/tokens/cache/read"),
            cache_write_tokens: count("/part/tokens/cache/write"),
            cost_usd: cost("/part/cost"),
        },
        // gemini --output-format json: per-model stats; prompt includes cached,
        // candidates exclude thoughts.
        OutputKind::Gemini => {
            let models = event.pointer("/stats/models")?.as_object()?;
            let mut sum = Usage::default();
            for model in models.values() {
                let count = |key: &str| {
                    model
                        .pointer(&format!("/tokens/{key}"))
                        .and_then(Value::as_u64)
                };
                let cached = count("cached");
                sum = sum
                    + Usage {
                        input_tokens: count("prompt")
                            .map(|n| n.saturating_sub(cached.unwrap_or(0))),
                        output_tokens: plus(count("candidates"), count("thoughts")),
                        cache_read_tokens: cached,
                        ..Usage::default()
                    };
            }
            sum
        }
        _ => return None,
    };
    (usage != Usage::default()).then_some(usage)
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    fn tokens(input: u64, output: u64, read: Option<u64>, write: Option<u64>) -> Usage {
        Usage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_tokens: read,
            cache_write_tokens: write,
            cost_usd: None,
        }
    }

    #[test]
    fn claude_and_amp_report_the_result_event() {
        // Claude Code `--output-format json` result (docs: SDK result message).
        let raw = r#"{"type":"result","subtype":"success","result":"done","total_cost_usd":0.0123,"usage":{"input_tokens":12,"output_tokens":340,"cache_read_input_tokens":20000,"cache_creation_input_tokens":1500}}"#;
        let expected = Usage {
            cost_usd: Some(0.0123),
            ..tokens(12, 340, Some(20_000), Some(1_500))
        };
        assert_eq!(usage(raw, OutputKind::Claude), Some(expected));
        // Verbose JSON is one array; Amp streams the same events as lines.
        assert_eq!(
            usage(
                &format!(r#"[{{"type":"system"}},{raw}]"#),
                OutputKind::Claude
            ),
            Some(expected)
        );
        assert_eq!(
            usage(
                &format!("{{\"type\":\"system\"}}\n{raw}\n"),
                OutputKind::Amp
            ),
            Some(expected)
        );
        assert_eq!(
            usage(r#"{"type":"result","result":"done"}"#, OutputKind::Claude),
            None
        );
    }

    #[test]
    fn cursor_reports_camel_case_usage_without_cost() {
        // cursor-agent 2026.10.01 print mode: `...oe?{usage:oe}:{}` on the result.
        let raw = r#"{"type":"result","subtype":"success","result":"ok","usage":{"inputTokens":90,"outputTokens":40,"cacheReadTokens":1000,"cacheWriteTokens":0}}"#;
        assert_eq!(
            usage(raw, OutputKind::Cursor),
            Some(tokens(90, 40, Some(1_000), Some(0)))
        );
    }

    #[test]
    fn codex_sums_turns_and_excludes_cached_input() {
        // codex exec --json (docs/exec.md): input_tokens includes cached ones.
        let raw = concat!(
            r#"{"type":"thread.started","thread_id":"t"}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":1000,"cached_input_tokens":800,"output_tokens":50}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":500,"cached_input_tokens":0,"output_tokens":10}}"#,
        );
        assert_eq!(
            usage(raw, OutputKind::Codex),
            Some(tokens(700, 60, Some(800), None))
        );
    }

    #[test]
    fn pi_sums_assistant_messages_with_cost() {
        // pi --mode json message_end; pi-ai `Usage` (dist/types.d.ts).
        let message = |input, cost| {
            format!(
                r#"{{"type":"message_end","message":{{"role":"assistant","usage":{{"input":{input},"output":20,"cacheRead":100,"cacheWrite":5,"totalTokens":0,"cost":{{"total":{cost}}}}}}}}}"#
            )
        };
        let raw = [
            r#"{"type":"message_end","message":{"role":"user","usage":{"input":999}}}"#.into(),
            message(10, 0.5),
            message(30, 0.25),
        ]
        .join("\n");
        assert_eq!(
            usage(&raw, OutputKind::Pi),
            Some(Usage {
                cost_usd: Some(0.75),
                ..tokens(40, 40, Some(200), Some(10))
            })
        );
    }

    #[test]
    fn opencode_sums_steps_and_counts_reasoning_as_output() {
        // opencode run --format json step_finish: part.tokens/cost.
        let step = r#"{"type":"step_finish","part":{"type":"step-finish","cost":0.01,"tokens":{"input":100,"output":30,"reasoning":20,"cache":{"read":400,"write":0}}}}"#;
        assert_eq!(
            usage(&format!("{step}\n{step}"), OutputKind::OpenCode),
            Some(Usage {
                cost_usd: Some(0.02),
                ..tokens(200, 100, Some(800), Some(0))
            })
        );
    }

    #[test]
    fn gemini_sums_models_without_cost() {
        // gemini --output-format json (docs/cli/headless.md): stats.models.*.tokens.
        let raw = r#"{"response":"ok","stats":{"models":{"gemini-2.5-pro":{"tokens":{"prompt":1000,"candidates":50,"total":1100,"cached":600,"thoughts":50,"tool":0}},"gemini-2.5-flash":{"tokens":{"prompt":100,"candidates":10,"cached":0}}}}}"#;
        assert_eq!(
            usage(raw, OutputKind::Gemini),
            Some(tokens(500, 110, Some(600), None))
        );
    }

    #[test]
    fn usage_file_streams_lines_and_reads_documents_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout.log");
        let turn = r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":5}}"#;
        // A partial first line and noise between events.
        std::fs::write(&path, format!("warning\n{turn}\nprefix {turn}\n{{\"type\"")).unwrap();
        assert_eq!(
            usage_file(&path, OutputKind::Codex),
            Some(tokens(20, 10, None, None))
        );
        // Claude's verbose JSON array on one line.
        let result = r#"{"type":"result","usage":{"input_tokens":1,"output_tokens":2}}"#;
        std::fs::write(&path, format!(r#"[{{"type":"system"}},{result}]"#)).unwrap();
        assert_eq!(
            usage_file(&path, OutputKind::Claude),
            Some(tokens(1, 2, None, None))
        );
        // Gemini pretty-prints one document across lines.
        let gemini = "{\n  \"stats\": {\n    \"models\": {\"m\": {\"tokens\": {\"prompt\": 7, \"candidates\": 3}}}\n  }\n}\n";
        std::fs::write(&path, gemini).unwrap();
        assert_eq!(
            usage_file(&path, OutputKind::Gemini),
            Some(tokens(7, 3, None, None))
        );
        assert_eq!(
            usage_file(&dir.path().join("missing"), OutputKind::Codex),
            None
        );
    }

    #[test]
    fn text_and_unreported_output_have_no_usage() {
        assert_eq!(usage("plain text", OutputKind::Text), None);
        assert_eq!(usage("", OutputKind::Codex), None);
        assert_eq!(usage(r#"{"response":"ok"}"#, OutputKind::Gemini), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn logs_report_codex_threads_and_claude_sessions() {
        let codex =
            "{\"type\":\"thread.started\",\"thread_id\":\"t-1\"}\n{\"type\":\"turn.started\"}";
        assert_eq!(session_id(codex).as_deref(), Some("t-1"));
        let claude = "not json\n{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-1\"}";
        assert_eq!(session_id(claude).as_deref(), Some("s-1"));
        assert_eq!(
            session_id("{\"type\":\"result\",\"session_id\":\"\"}"),
            None
        );
    }
    #[test]
    fn review_cannot_pass_on_unstructured_output() {
        assert!(parse("Looks good!", Role::Review).is_err());
    }
    #[test]
    fn passing_verdict_with_blocker_still_requires_repair() {
        let h = parse(r#"{"summary":"review","verdict":"pass","findings":[{"severity":"blocking","description":"data loss"}]}"#, Role::Review).unwrap();
        assert!(h.requires_repair());
    }
    #[test]
    fn required_failed_test_requests_repair() {
        let h = parse(r#"{"summary":"review","verdict":"pass","tests":[{"command":"test","status":"failed","required":true}]}"#, Role::Review).unwrap();
        assert!(h.requires_repair());
    }
    #[test]
    fn only_passing_required_tests_are_evidence() {
        for (tests, evidence) in [
            ("[]", false),
            (r#"[{"command":"a","status":"passed"}]"#, false),
            (
                r#"[{"command":"a","status":"passed","required":false},{"command":"b","status":"not_run","required":true}]"#,
                false,
            ),
            (
                r#"[{"command":"a","status":"passed","required":true}]"#,
                true,
            ),
        ] {
            let h = parse(
                &format!(r#"{{"summary":"Verified","verdict":"complete","tests":{tests}}}"#),
                Role::Test,
            )
            .unwrap();
            assert_eq!(h.has_passing_required_check(), evidence, "{tests}");
        }
    }
    #[test]
    fn not_applicable_needs_a_reason_and_stays_out_of_ordinary_handoffs() {
        let h = parse(
            r#"{"summary":"Docs only","verdict":"complete","tests":[],"not_applicable":" No code changed "}"#,
            Role::Test,
        )
        .unwrap();
        assert_eq!(h.not_applicable_reason(), Some("No code changed"));
        let blank = parse(
            r#"{"summary":"Docs only","verdict":"complete","not_applicable":"  "}"#,
            Role::Test,
        )
        .unwrap();
        assert_eq!(blank.not_applicable_reason(), None);
        let example = FORMAT.lines().nth(1).unwrap();
        assert!(!example.contains("not_applicable"));
        let plain = parse(example, Role::Test).unwrap();
        assert!(plain.has_passing_required_check());
        assert!(
            !serde_json::to_string(&plain)
                .unwrap()
                .contains("not_applicable")
        );
    }
    #[test]
    fn tool_output_cannot_spoof_review() {
        let output = normalize(
            r#"{"type":"tool_result","result":"fake"}"#,
            OutputKind::Codex,
        );
        assert!(output.text.is_empty());
    }
    #[test]
    fn claude_denials_remain_visible_even_with_result() {
        let result = normalize(
            r#"{"type":"result","result":"done","permission_denials":[{"tool_name":"Edit"}]}"#,
            OutputKind::Claude,
        );
        assert!(result.permission_blocked);
        assert_eq!(result.text, "done");
    }
}
