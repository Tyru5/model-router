use crate::{adapters::OutputKind, storage};
use serde_json::Value;

#[derive(Debug, Default)]
pub struct Normalized {
    pub text: String,
    pub error: Option<ProtocolError>,
    pub permission_blocked: bool,
    /// Claude Code's `structured_output`, as JSON, when launched with `--json-schema`.
    pub structured: Option<String>,
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
/// Decoded JSON text can hold real escape bytes, and a reported error is
/// printed to the terminal.
impl From<&str> for ProtocolError {
    fn from(message: &str) -> Self {
        Self::Reported(storage::clean_terminal(message))
    }
}

/// The harness session an unattended log reports: Codex's `thread.started`
/// thread, Pi's `session` event, OpenCode's `sessionID`, Antigravity's
/// `conversation_id`, or the `session_id` on Claude Code, Cursor, and Amp
/// stream events.
pub fn session_id(raw: &str) -> Option<String> {
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .find_map(|event| {
            let pointers: &[&str] = match event.get("type").and_then(Value::as_str) {
                Some("thread.started") => &["/thread_id"],
                Some("session") => &["/id"],
                _ => &[
                    "/session_id",
                    "/sessionID",
                    "/conversation_id",
                    "/init/conversation_id",
                    "/result/conversation_id",
                ],
            };
            pointers
                .iter()
                .find_map(|pointer| event.pointer(pointer).and_then(Value::as_str))
                .filter(|id| crate::adapters::is_session_id(id))
                .map(str::to_owned)
        })
}

fn has_control_bytes(text: &str) -> bool {
    text.bytes()
        .any(|b| (b < 0x20 && b != b'\n' && b != b'\t') || b == 0x7f || b == 0xc2)
}

fn is_error_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Number(_) => true,
    }
}

fn truncate_json_value(value: &Value, max: usize) -> String {
    struct LimitWriter {
        buf: Vec<u8>,
        limit: usize,
    }
    impl std::io::Write for LimitWriter {
        fn write(&mut self, src: &[u8]) -> std::io::Result<usize> {
            let room = self.limit.saturating_sub(self.buf.len());
            if room == 0 {
                return Err(std::io::Error::other("limit"));
            }
            let take = src.len().min(room);
            self.buf.extend_from_slice(&src[..take]);
            if take < src.len() {
                return Err(std::io::Error::other("limit"));
            }
            Ok(take)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = LimitWriter {
        buf: Vec::with_capacity(max.min(2048)),
        limit: max.saturating_add(4),
    };
    let _ = serde_json::to_writer(&mut writer, value);
    let valid = match std::str::from_utf8(&writer.buf) {
        Ok(s) => s,
        Err(error) => std::str::from_utf8(&writer.buf[..error.valid_up_to()]).unwrap_or(""),
    };
    storage::truncate(valid, max)
}

fn parse_json_clean(raw: &str) -> Option<Value> {
    if !has_control_bytes(raw) {
        serde_json::from_str::<Value>(raw.trim()).ok()
    } else {
        serde_json::from_str::<Value>(storage::clean_terminal(raw).trim()).ok()
    }
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
    let mut result = Normalized::default();
    let mut recognized = false;
    // OpenCode: the message whose text parts form the handoff.
    let mut message_id: Option<String> = None;
    let trimmed = raw.trim();
    let whole_doc = (trimmed.starts_with('[') || !trimmed.contains('\n'))
        .then(|| parse_json_clean(trimmed))
        .flatten();
    if let Some(value) = whole_doc {
        match value {
            Value::Array(events) => {
                for event in &events {
                    process_event(event, kind, &mut result, &mut recognized, &mut message_id);
                }
            }
            event => process_event(&event, kind, &mut result, &mut recognized, &mut message_id),
        }
    } else {
        let mut parsed_any = false;
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(event) = parse_json_clean(line) {
                parsed_any = true;
                process_event(&event, kind, &mut result, &mut recognized, &mut message_id);
            }
        }
        if !parsed_any && trimmed.starts_with('{') {
            if let Some(value) = parse_json_clean(trimmed) {
                match value {
                    Value::Array(events) => {
                        for event in &events {
                            process_event(
                                event,
                                kind,
                                &mut result,
                                &mut recognized,
                                &mut message_id,
                            );
                        }
                    }
                    event => {
                        process_event(&event, kind, &mut result, &mut recognized, &mut message_id);
                    }
                }
            }
        }
    }
    if !result.text.is_empty() && has_control_bytes(&result.text) {
        result.text = storage::clean_terminal(&result.text);
    }
    // A native structured stream with no assistant message isn't a usable result.
    if !recognized && result.error.is_none() {
        result.error = Some(ProtocolError::NoFinalResponse);
    }
    result
}

fn process_event(
    event: &Value,
    kind: OutputKind,
    result: &mut Normalized,
    recognized: &mut bool,
    message_id: &mut Option<String>,
) {
    if event.get("is_error").and_then(Value::as_bool) == Some(true) {
        // Claude's error results carry `errors` instead of `result`.
        let errors: Vec<&str> = event
            .get("errors")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        result.error = Some(ProtocolError::from(
            event
                .get("result")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| (!errors.is_empty()).then(|| errors.join("\n")))
                .unwrap_or_else(|| "Harness reported an error".into())
                .as_str(),
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
        || event.get("error").is_some_and(is_error_value);
    if failed {
        // JSON text keeps DEL and C1 controls such as U+009B (CSI) raw.
        result.error = Some(ProtocolError::from(
            truncate_json_value(event, 1500).as_str(),
        ));
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
                    *recognized = true;
                }
                if let Some(output) = event.get("structured_output").filter(|v| v.is_object()) {
                    result.structured = Some(output.to_string());
                    *recognized = true;
                }
            } else if kind_name == "assistant" {
                if let Some(content) = event.pointer("/message/content").and_then(Value::as_array) {
                    for block in content {
                        if block.get("type").and_then(Value::as_str) == Some("text") {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                result.text.push_str(text);
                                result.text.push('\n');
                                *recognized = true;
                            }
                        }
                    }
                }
            }
        }
        OutputKind::Antigravity => {
            let event_name = event.get("event").and_then(Value::as_str).unwrap_or("");
            if event_name == "error" {
                result.error = Some(ProtocolError::from(
                    truncate_json_value(event, 1500).as_str(),
                ));
            } else if event_name == "result"
                || (event_name.is_empty()
                    && (event.get("response").is_some()
                        || event.get("structured_output").is_some()
                        || event.get("status").is_some()))
            {
                let payload = event
                    .get("result")
                    .filter(|v| v.is_object())
                    .unwrap_or(event);
                let status = payload.get("status").and_then(Value::as_str).unwrap_or("");
                let status_failed = matches!(
                    status.to_ascii_lowercase().as_str(),
                    "error" | "failed" | "canceled" | "cancelled"
                );
                let payload_failed =
                    failed || status_failed || payload.get("error").is_some_and(is_error_value);
                if payload_failed {
                    result.error = Some(ProtocolError::from(
                        truncate_json_value(event, 1500).as_str(),
                    ));
                } else {
                    result.error = None;
                    if let Some(text) = payload.get("response").and_then(Value::as_str) {
                        result.text = text.into();
                        *recognized = true;
                    }
                    if let Some(output) = payload.get("structured_output").filter(|v| v.is_object())
                    {
                        result.structured = Some(output.to_string());
                        *recognized = true;
                    }
                }
            }
        }
        OutputKind::Codex => {
            if kind_name == "item.completed"
                && event.pointer("/item/type").and_then(Value::as_str) == Some("agent_message")
            {
                if let Some(text) = event.pointer("/item/text").and_then(Value::as_str) {
                    result.text = text.into();
                    *recognized = true;
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
                if let Some(content) = event.pointer("/message/content").and_then(Value::as_array) {
                    result.text = content
                        .iter()
                        .filter(|v| v.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|v| v.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n");
                    *recognized = true;
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
                    if id != *message_id {
                        result.text.clear();
                        *message_id = id;
                    }
                    result.text.push_str(text);
                    result.text.push('\n');
                    *recognized = true;
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
