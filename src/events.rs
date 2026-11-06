//! Readable progress lines from harness JSON events, for the Monitor and CLI.
//! Display only: the raw events stay in stdout.log, and handoffs never come
//! from these lines.
use crate::{adapters::OutputKind, storage};
use serde_json::Value;

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
    pub fn push(&mut self, chunk: &str) -> Option<String> {
        let Some(end) = chunk.rfind(['\n', '\r']) else {
            self.hold(chunk);
            return None;
        };
        let mut complete = std::mem::take(&mut self.pending);
        let skipped = std::mem::take(&mut self.skipping);
        complete.push_str(&chunk[..end]);
        self.hold(&chunk[end + 1..]);
        let mut lines = complete.split(['\n', '\r']);
        // An overlong line's tail ends at the chunk's first line break.
        let first = lines.next().unwrap_or("");
        let note = skipped.then(|| format!("… line longer than {PENDING_LIMIT} bytes omitted"));
        let first = (!skipped).then_some(first);
        join(
            note.into_iter().chain(
                first
                    .into_iter()
                    .chain(lines)
                    .flat_map(|line| describe(line, self.kind)),
            ),
        )
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
        return vec![line.to_owned()];
    }
    let Ok(event) = serde_json::from_str::<Value>(trimmed) else {
        return vec![line.to_owned()];
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
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max).collect::<String>())
    }
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
    }
}
