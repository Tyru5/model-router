//! Model Context Protocol over stdio: newline-delimited JSON-RPC 2.0.

use super::{Server, tools};
use anyhow::Result;
use percent_encoding::percent_decode_str;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs::File,
    io::{self, BufRead, Read, Write},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
};

pub const PROTOCOL_VERSION: &str = "2025-06-18";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const SERVER_OVERLOADED: i64 = -32000;
const RESOURCE_NOT_FOUND: i64 = -32002;

const RESOURCE_PREFIX: &str = "model-router://runs/";
/// Maximum bytes returned by `resources/read`.
const MAX_RESOURCE_BYTES: usize = crate::snapshot::PATCH_LIMIT;
/// Maximum recent runs inspected by `resources/list`.
const MAX_LISTED_RUNS: usize = 20;

/// Maximum bytes read for a single JSON-RPC request line.
const MAX_LINE_BYTES: usize = 2 * 1024 * 1024;
/// Maximum concurrent in-flight MCP `tools/call` worker threads.
const MAX_CONCURRENT_CALLS: usize = 32;
/// Of those, how many may be calls that block (`tools::blocks`: `wait_run`).
/// The rest stay free for calls that answer at once, so waiting clients can
/// never keep `cancel_run` or `run_status` from running.
const MAX_BLOCKING_CALLS: usize = 24;

const INSTRUCTIONS: &str = "model-router runs a task through a workflow of agentic harness stages. \
Typical use: list_workflows, optionally preview_run, then start_run and wait_run (repeat while timed_out) \
and get_report. A run waiting at a checkpoint (status awaiting_confirmation) continues with confirm_stage; \
a failed or blocked one with retry_stage.";

/// Error returned by MCP resource handlers, mapped to JSON-RPC error codes.
#[derive(Debug, Clone)]
pub enum ResourceError {
    InvalidParams(String),
    NotFound(String),
}

/// The tools one MCP session serves: `serve`'s run tools, or a delegating
/// stage's delegation tools (`delegation::tools`).
pub trait ToolSet: Send + Sync + 'static {
    fn instructions(&self) -> &str;
    fn list(&self) -> Vec<Value>;
    fn exists(&self, name: &str) -> bool;
    /// Calls that may block for a long time get their own, smaller cap.
    fn blocks(&self, name: &str) -> bool;
    fn call(
        &self,
        name: &str,
        arguments: &Value,
        cancel: Option<&AtomicBool>,
        progress: &mut dyn FnMut(&Value),
    ) -> Result<Value>;

    /// Whether this session exposes MCP resources (`resources/list`, `resources/read`).
    fn has_resources(&self) -> bool {
        false
    }
    fn list_resources(&self) -> std::result::Result<Vec<Value>, ResourceError> {
        Ok(vec![])
    }
    fn read_resource(&self, _uri: &str) -> std::result::Result<Value, ResourceError> {
        Err(ResourceError::NotFound(
            "Resources are not supported".into(),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceKind {
    Review,
    Diff,
}

impl ResourceKind {
    fn filename(self) -> &'static str {
        match self {
            Self::Review => "review.md",
            Self::Diff => "changes.diff",
        }
    }

    fn mime_type(self) -> &'static str {
        match self {
            Self::Review => "text/markdown",
            Self::Diff => "text/x-diff",
        }
    }
}

fn parse_resource_uri(
    uri: &str,
) -> std::result::Result<(&str, ResourceKind, Option<String>), ResourceError> {
    let (base, query) = uri.split_once('?').unwrap_or((uri, ""));
    let Some(rest) = base.strip_prefix(RESOURCE_PREFIX) else {
        return Err(ResourceError::InvalidParams(format!(
            "Unsupported resource URI {uri:?}; expected model-router://runs/<run_id>/(review|diff)"
        )));
    };
    let mut parts = rest.split('/');
    let (Some(run_id), Some(kind_str), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(ResourceError::InvalidParams(format!(
            "Invalid resource URI {uri:?}; expected model-router://runs/<run_id>/(review|diff)"
        )));
    };
    let kind = match kind_str {
        "review" => ResourceKind::Review,
        "diff" => ResourceKind::Diff,
        _ => {
            return Err(ResourceError::InvalidParams(format!(
                "Unknown resource kind {kind_str:?}; expected \"review\" or \"diff\""
            )));
        }
    };
    let mut project = None;
    if !query.is_empty() {
        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            if name != "project" {
                return Err(ResourceError::InvalidParams(format!(
                    "Unsupported query parameter {name:?} in resource URI"
                )));
            }
            project = Some(
                percent_decode_str(&value.replace('+', " "))
                    .decode_utf8_lossy()
                    .into_owned(),
            );
        }
    }
    Ok((run_id, kind, project))
}

fn trim_incomplete_utf8_tail(bytes: &mut Vec<u8>) {
    let tail_start = bytes.len().saturating_sub(4);
    let mut i = bytes.len();
    while i > tail_start {
        i -= 1;
        if (bytes[i] & 0xC0) != 0x80 {
            if let Err(err) = std::str::from_utf8(&bytes[i..]) {
                if err.error_len().is_none() {
                    bytes.truncate(i);
                }
            }
            break;
        }
    }
}

fn read_bounded(file: File, limit: usize) -> io::Result<String> {
    let mut bytes = Vec::new();
    file.take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > limit;
    if truncated {
        bytes.truncate(limit);
        trim_incomplete_utf8_tail(&mut bytes);
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        text.push_str(&format!("\n[truncated at {limit} bytes]"));
    }
    Ok(text)
}

impl ToolSet for Server {
    fn instructions(&self) -> &str {
        INSTRUCTIONS
    }
    fn list(&self) -> Vec<Value> {
        tools::list()
    }
    fn exists(&self, name: &str) -> bool {
        tools::exists(name)
    }
    fn blocks(&self, name: &str) -> bool {
        tools::blocks(name)
    }
    fn call(
        &self,
        name: &str,
        arguments: &Value,
        cancel: Option<&AtomicBool>,
        progress: &mut dyn FnMut(&Value),
    ) -> Result<Value> {
        tools::call_with(self, name, arguments, cancel, progress)
    }
    fn has_resources(&self) -> bool {
        true
    }
    fn list_resources(&self) -> std::result::Result<Vec<Value>, ResourceError> {
        let project = self
            .project(None)
            .map_err(|e| ResourceError::InvalidParams(format!("{e:#}")))?;
        let entries = crate::history::list(&project)
            .map_err(|e| ResourceError::InvalidParams(format!("{e:#}")))?;
        let mut resources = Vec::new();
        for summary in entries
            .iter()
            .filter_map(|entry| match entry {
                crate::history::Entry::Summary(summary) => Some(summary),
                crate::history::Entry::Damaged { .. } => None,
            })
            .take(MAX_LISTED_RUNS)
        {
            if crate::storage::run_dir(&project, &summary.id).is_err() {
                continue;
            }
            let run_rel = Path::new(".model-router").join("runs").join(&summary.id);
            let task = crate::history::excerpt(&summary.task);
            if crate::storage::open_beneath(
                &project,
                &run_rel.join(ResourceKind::Review.filename()),
            )
            .is_ok()
            {
                resources.push(json!({
                    "uri": format!("{RESOURCE_PREFIX}{}/review", summary.id),
                    "name": format!("{} review", summary.id),
                    "description": format!("Review brief for run {} ({}): {}", summary.id, summary.workflow, task),
                    "mimeType": ResourceKind::Review.mime_type(),
                }));
            }
            if crate::storage::open_beneath(&project, &run_rel.join(ResourceKind::Diff.filename()))
                .is_ok()
            {
                resources.push(json!({
                    "uri": format!("{RESOURCE_PREFIX}{}/diff", summary.id),
                    "name": format!("{} diff", summary.id),
                    "description": format!("Diff for run {} ({}): {}", summary.id, summary.workflow, task),
                    "mimeType": ResourceKind::Diff.mime_type(),
                }));
            }
        }
        Ok(resources)
    }
    fn read_resource(&self, uri: &str) -> std::result::Result<Value, ResourceError> {
        let (run_id, kind, requested_project) = parse_resource_uri(uri)?;
        let project = self
            .project(requested_project.as_deref())
            .map_err(|e| ResourceError::InvalidParams(format!("{e:#}")))?;
        crate::storage::run_dir(&project, run_id)
            .map_err(|e| ResourceError::InvalidParams(format!("{e:#}")))?;
        if let Some(owner) = self.runs.project_of(run_id) {
            if owner != project {
                return Err(ResourceError::InvalidParams(format!(
                    "Run {run_id} belongs to project {}",
                    owner.display()
                )));
            }
        }
        let relative = Path::new(".model-router")
            .join("runs")
            .join(run_id)
            .join(kind.filename());
        let file = crate::storage::open_beneath(&project, &relative)
            .map_err(|e| ResourceError::NotFound(format!("Resource not found: {uri} ({e:#})")))?;
        let text = read_bounded(file, MAX_RESOURCE_BYTES)
            .map_err(|e| ResourceError::NotFound(format!("Failed to read {uri}: {e}")))?;
        Ok(json!({
            "uri": uri,
            "mimeType": kind.mime_type(),
            "text": text,
        }))
    }
}

/// A tool failure with a stable code, returned as `structuredContent.error`
/// so the calling model can branch on it.
#[derive(Debug, Clone)]
pub struct ToolError {
    pub code: &'static str,
    pub message: String,
}
impl ToolError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for ToolError {}

type CancelMap = HashMap<String, Vec<(u64, Arc<AtomicBool>)>>;

#[derive(Default)]
struct InFlight {
    active: AtomicUsize,
    /// The blocking calls among `active`.
    blocking: AtomicUsize,
    next_seq: AtomicU64,
    cancels: Mutex<CancelMap>,
}

struct CallSlot {
    inflight: Arc<InFlight>,
    key: String,
    seq: u64,
    blocking: bool,
}

impl Drop for CallSlot {
    fn drop(&mut self) {
        let mut cancels = self
            .inflight
            .cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entries) = cancels.get_mut(&self.key) {
            entries.retain(|(seq, _)| *seq != self.seq);
            if entries.is_empty() {
                cancels.remove(&self.key);
            }
        }
        drop(cancels);
        self.inflight.active.fetch_sub(1, Ordering::AcqRel);
        if self.blocking {
            self.inflight.blocking.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Counts one more in `counter` unless it is at `max`.
fn reserve(counter: &AtomicUsize, max: usize) -> bool {
    let mut n = counter.load(Ordering::Acquire);
    loop {
        if n >= max {
            return false;
        }
        match counter.compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(current) => n = current,
        }
    }
}

impl InFlight {
    /// A slot for one tool call, or None at the limit: `MAX_BLOCKING_CALLS`
    /// for a `blocking` call, and `MAX_CONCURRENT_CALLS` for any.
    fn acquire(
        self: &Arc<Self>,
        id: &Value,
        blocking: bool,
    ) -> Option<(CallSlot, Arc<AtomicBool>)> {
        if blocking && !reserve(&self.blocking, MAX_BLOCKING_CALLS) {
            return None;
        }
        if !reserve(&self.active, MAX_CONCURRENT_CALLS) {
            if blocking {
                self.blocking.fetch_sub(1, Ordering::AcqRel);
            }
            return None;
        }
        let key = id.to_string();
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let flag = Arc::new(AtomicBool::new(false));
        self.cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.clone())
            .or_default()
            .push((seq, flag.clone()));
        Some((
            CallSlot {
                inflight: self.clone(),
                key,
                seq,
                blocking,
            },
            flag,
        ))
    }

    fn cancel(&self, id: &Value) {
        let key = id.to_string();
        if let Some(entries) = self
            .cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            for (_, flag) in entries {
                flag.store(true, Ordering::Relaxed);
            }
        }
    }
}

enum ReadLine {
    Eof,
    Line,
    TooLong,
}

/// Reads up to `\n` into `out`, capping stored bytes at `limit`. When a line
/// exceeds `limit`, drains the rest through `\n` without growing `out`.
fn read_line_bounded(
    input: &mut impl BufRead,
    out: &mut Vec<u8>,
    limit: usize,
) -> io::Result<ReadLine> {
    out.clear();
    let mut overflow = false;
    loop {
        let buf = input.fill_buf()?;
        if buf.is_empty() {
            return Ok(if overflow {
                ReadLine::TooLong
            } else if out.is_empty() {
                ReadLine::Eof
            } else {
                ReadLine::Line
            });
        }
        let newline = buf.iter().position(|&b| b == b'\n');
        let take = newline.map_or(buf.len(), |pos| pos + 1);
        if !overflow {
            if out.len() + take > limit {
                overflow = true;
                out.clear();
            } else {
                out.extend_from_slice(&buf[..take]);
            }
        }
        input.consume(take);
        if newline.is_some() {
            return Ok(if overflow {
                ReadLine::TooLong
            } else {
                ReadLine::Line
            });
        }
    }
}

/// Reads requests until EOF, or until stdin cannot be read, which ends the
/// session the same way. Tool calls run on their own threads so a long
/// `wait_run` never blocks `cancel_run`; responses may arrive out of order.
/// Returns the tool calls still in flight, so shutdown can let them answer.
pub fn serve<T: ToolSet, W: Write + Send + 'static>(
    server: Arc<T>,
    mut input: impl BufRead,
    output: W,
) -> Vec<JoinHandle<()>> {
    let output = Arc::new(Mutex::new(output));
    let inflight = Arc::new(InFlight::default());
    let mut calls: Vec<JoinHandle<()>> = vec![];
    let mut line = Vec::new();
    loop {
        match read_line_bounded(&mut input, &mut line, MAX_LINE_BYTES) {
            Ok(ReadLine::Eof) => break,
            Ok(ReadLine::Line) => {}
            Ok(ReadLine::TooLong) => {
                send(
                    &output,
                    &error_response(
                        Value::Null,
                        PARSE_ERROR,
                        &format!("Parse error: request line exceeds {MAX_LINE_BYTES} bytes"),
                    ),
                );
                continue;
            }
            Err(error) => {
                super::log(format!("stdin unreadable: {error}"));
                break;
            }
        }
        calls.retain(|call| !call.is_finished());
        if line.trim_ascii().is_empty() {
            continue;
        }
        // Bytes, not text: a line that is not UTF-8 is one bad request.
        let message: Value = match serde_json::from_slice(&line) {
            Ok(message) => message,
            Err(error) => {
                send(
                    &output,
                    &error_response(Value::Null, PARSE_ERROR, &format!("Parse error: {error}")),
                );
                continue;
            }
        };
        if let Some(response) = handle(&server, &output, &inflight, message, &mut calls) {
            send(&output, &response);
        }
    }
    calls
}

fn send<W: Write>(output: &Mutex<W>, message: &Value) {
    // Compact JSON escapes newlines, so each message is one line.
    let Ok(mut line) = serde_json::to_vec(message) else {
        super::log("failed to serialize JSON-RPC message");
        return;
    };
    line.push(b'\n');
    let mut output = output.lock().unwrap_or_else(|e| e.into_inner());
    if output
        .write_all(&line)
        .and_then(|()| output.flush())
        .is_err()
    {
        super::log("stdout closed; dropping a response");
    }
}

fn response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn progress_notification(token: &Value, step: u64, snapshot: &Value) -> Value {
    let stage = snapshot
        .get("current_stage")
        .and_then(Value::as_str)
        .unwrap_or("");
    let status = snapshot.get("status").and_then(Value::as_str).unwrap_or("");
    let message = if stage.is_empty() {
        status.to_owned()
    } else {
        format!("{status}: {stage}")
    };
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": {
            "progressToken": token,
            "progress": step,
            "message": message,
        }
    })
}

/// The response to send now, if any. Tool calls answer later from a thread.
fn handle<T: ToolSet, W: Write + Send + 'static>(
    server: &Arc<T>,
    output: &Arc<Mutex<W>>,
    inflight: &Arc<InFlight>,
    message: Value,
    calls: &mut Vec<JoinHandle<()>>,
) -> Option<Value> {
    let Some(object) = message.as_object() else {
        return Some(error_response(
            Value::Null,
            INVALID_REQUEST,
            "Expected a JSON-RPC object; batches are not supported",
        ));
    };
    let id = object.get("id").cloned();
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        // A response to a request we never send, or garbage.
        return id
            .filter(|_| !object.contains_key("result") && !object.contains_key("error"))
            .map(|id| error_response(id, INVALID_REQUEST, "Missing method"));
    };
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    // Notifications get no response.
    let Some(id) = id else {
        if method == "notifications/cancelled" {
            if let Some(request_id) = params.get("requestId") {
                inflight.cancel(request_id);
            }
        }
        return None;
    };
    Some(match method {
        "initialize" => {
            let mut capabilities = json!({"tools": {"listChanged": false}});
            if server.has_resources() {
                capabilities["resources"] = json!({});
            }
            response(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": capabilities,
                    "serverInfo": {
                        "name": "model-router",
                        "title": "model-router",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "instructions": server.instructions(),
                }),
            )
        }
        "ping" => response(id, json!({})),
        "tools/list" => response(id, json!({"tools": server.list()})),
        "resources/list" => {
            if !server.has_resources() {
                return Some(error_response(
                    id,
                    METHOD_NOT_FOUND,
                    "Method not found: resources/list",
                ));
            }
            match server.list_resources() {
                Ok(resources) => response(id, json!({"resources": resources})),
                Err(ResourceError::InvalidParams(msg)) => error_response(id, INVALID_PARAMS, &msg),
                Err(ResourceError::NotFound(msg)) => error_response(id, RESOURCE_NOT_FOUND, &msg),
            }
        }
        "resources/read" => {
            if !server.has_resources() {
                return Some(error_response(
                    id,
                    METHOD_NOT_FOUND,
                    "Method not found: resources/read",
                ));
            }
            let Some(uri) = params.get("uri").and_then(Value::as_str) else {
                return Some(error_response(id, INVALID_PARAMS, "Missing resource uri"));
            };
            match server.read_resource(uri) {
                Ok(content) => response(id, json!({"contents": [content]})),
                Err(ResourceError::InvalidParams(msg)) => error_response(id, INVALID_PARAMS, &msg),
                Err(ResourceError::NotFound(msg)) => error_response(id, RESOURCE_NOT_FOUND, &msg),
            }
        }
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(error_response(id, INVALID_PARAMS, "Missing tool name"));
            };
            if !server.exists(name) {
                return Some(error_response(
                    id,
                    INVALID_PARAMS,
                    &format!("Unknown tool: {name}"),
                ));
            }
            let blocking = server.blocks(name);
            let Some((slot, cancel)) = inflight.acquire(&id, blocking) else {
                return Some(error_response(
                    id,
                    SERVER_OVERLOADED,
                    &if blocking {
                        format!(
                            "Too many concurrent {name} calls (limit {MAX_BLOCKING_CALLS}); retry after one returns"
                        )
                    } else {
                        "Too many concurrent tool calls".to_owned()
                    },
                ));
            };
            let name = name.to_owned();
            let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
            let progress_token = params
                .get("_meta")
                .and_then(|m| m.get("progressToken"))
                .filter(|v| v.is_string() || v.is_number())
                .cloned();
            let (server, output) = (server.clone(), output.clone());
            let spawned = std::thread::Builder::new()
                .name(format!("tool {name}"))
                .spawn({
                    let id = id.clone();
                    move || {
                        let _slot = slot;
                        let mut step = 0u64;
                        let mut progress = |snapshot: &Value| {
                            if let Some(token) = &progress_token {
                                if !cancel.load(Ordering::Relaxed) {
                                    step += 1;
                                    send(&output, &progress_notification(token, step, snapshot));
                                }
                            }
                        };
                        let result = server.call(&name, &arguments, Some(&cancel), &mut progress);
                        // MCP clients expect no response for a cancelled request.
                        if !cancel.load(Ordering::Relaxed) {
                            send(&output, &response(id, tool_result(result)));
                        }
                    }
                });
            match spawned {
                Ok(call) => {
                    calls.push(call);
                    return None;
                }
                Err(error) => response(id, tool_result(Err(error.into()))),
            }
        }
        _ => error_response(id, METHOD_NOT_FOUND, &format!("Method not found: {method}")),
    })
}

/// A `tools/call` result; tool failures are results with `isError`, not
/// protocol errors, so the model can read and correct them.
pub fn tool_result(result: Result<Value>) -> Value {
    match result {
        Ok(value) => json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&value).unwrap_or_default(),
            }],
            "structuredContent": value,
            "isError": false,
        }),
        Err(error) => match error.downcast_ref::<ToolError>() {
            Some(tool) => json!({
                "content": [{"type": "text", "text": tool.to_string()}],
                "structuredContent": {"error": {"code": tool.code, "message": tool.message}},
                "isError": true,
            }),
            None => json!({
                "content": [{"type": "text", "text": format!("{error:#}")}],
                "isError": true,
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_line_reader_rejects_oversized_and_continues() {
        let data = b"{\"a\":1}\nxxxxxxxxxxxxxxxxxxxxxxxx\n{\"b\":2}\n";
        let mut cursor = io::Cursor::new(&data[..]);
        let mut line = Vec::new();
        assert!(matches!(
            read_line_bounded(&mut cursor, &mut line, 16).unwrap(),
            ReadLine::Line
        ));
        assert_eq!(line, b"{\"a\":1}\n");
        assert!(matches!(
            read_line_bounded(&mut cursor, &mut line, 16).unwrap(),
            ReadLine::TooLong
        ));
        assert!(matches!(
            read_line_bounded(&mut cursor, &mut line, 16).unwrap(),
            ReadLine::Line
        ));
        assert_eq!(line, b"{\"b\":2}\n");
        assert!(matches!(
            read_line_bounded(&mut cursor, &mut line, 16).unwrap(),
            ReadLine::Eof
        ));
    }

    /// Blocking calls stop at their own cap, which leaves slots for calls
    /// that answer at once (`cancel_run`) however many clients wait.
    #[test]
    fn blocking_calls_leave_slots_for_quick_ones() {
        assert!(tools::blocks("wait_run"));
        assert!(!tools::blocks("cancel_run") && !tools::blocks("run_status"));
        let inflight = Arc::new(InFlight::default());
        let mut waits: Vec<_> = (0..MAX_BLOCKING_CALLS)
            .map(|n| inflight.acquire(&json!(n), true).expect("blocking slot"))
            .collect();
        assert!(inflight.acquire(&json!("one more wait"), true).is_none());
        // The refused call reserved nothing.
        assert_eq!(
            inflight.blocking.load(Ordering::Acquire),
            MAX_BLOCKING_CALLS
        );
        assert_eq!(inflight.active.load(Ordering::Acquire), MAX_BLOCKING_CALLS);
        let quick: Vec<_> = (0..MAX_CONCURRENT_CALLS - MAX_BLOCKING_CALLS)
            .map(|n| {
                inflight
                    .acquire(&json!(format!("quick {n}")), false)
                    .expect("quick slot")
            })
            .collect();
        assert!(!quick.is_empty());
        // The overall bound still holds.
        assert!(inflight.acquire(&json!("over"), false).is_none());
        assert_eq!(
            inflight.active.load(Ordering::Acquire),
            MAX_CONCURRENT_CALLS
        );
        // A finished wait frees a blocking slot, and its cancel entry.
        waits.pop();
        assert_eq!(
            inflight.blocking.load(Ordering::Acquire),
            MAX_BLOCKING_CALLS - 1
        );
        assert!(inflight.acquire(&json!("next wait"), true).is_some());
        drop((waits, quick));
        assert_eq!(inflight.active.load(Ordering::Acquire), 0);
        assert_eq!(inflight.blocking.load(Ordering::Acquire), 0);
        assert!(inflight.cancels.lock().unwrap().is_empty());
    }

    #[test]
    fn tool_names_match_listed_tools() {
        let listed: Vec<String> = tools::list()
            .into_iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect();
        for name in &listed {
            assert!(tools::exists(name), "missing {name} in tools::exists");
        }
        for &name in tools::tool_names() {
            assert!(
                listed.iter().any(|l| l == name),
                "extra {name} in TOOL_NAMES not present in tools::list"
            );
        }
        assert_eq!(listed.len(), tools::tool_names().len());
        assert!(!tools::exists("nonexistent_tool"));
    }

    #[test]
    fn duplicate_request_ids_do_not_clobber_active_cancel_flag() {
        let inflight = Arc::new(InFlight::default());
        let id = json!(42);
        let (slot_a, cancel_a) = inflight.acquire(&id, false).unwrap();
        let (slot_b, cancel_b) = inflight.acquire(&id, false).unwrap();
        drop(slot_a);
        // Dropping the first call must keep the second call cancellable.
        inflight.cancel(&id);
        assert!(cancel_b.load(Ordering::Relaxed));
        assert!(!cancel_a.load(Ordering::Relaxed));
        drop(slot_b);
        assert!(inflight.cancels.lock().unwrap().is_empty());
    }

    #[test]
    fn resource_uri_parsing_validates_shape_and_query() {
        let (run_id, kind, project) =
            parse_resource_uri("model-router://runs/20260101-120000-abcd1234/review").unwrap();
        assert_eq!(run_id, "20260101-120000-abcd1234");
        assert_eq!(kind, ResourceKind::Review);
        assert_eq!(project, None);

        let (run_id, kind, project) = parse_resource_uri(
            "model-router://runs/20260101-120000-abcd1234/diff?project=%2Ftmp%2Fmy+repo",
        )
        .unwrap();
        assert_eq!(run_id, "20260101-120000-abcd1234");
        assert_eq!(kind, ResourceKind::Diff);
        assert_eq!(project.as_deref(), Some("/tmp/my repo"));

        for bad in [
            "http://runs/abc/review",
            "model-router://runs/abc/unknown",
            "model-router://runs/abc/review/extra",
            "model-router://runs/abc",
            "model-router://runs/abc/review?foo=bar",
        ] {
            assert!(
                matches!(
                    parse_resource_uri(bad),
                    Err(ResourceError::InvalidParams(_))
                ),
                "expected invalid params for {bad}"
            );
        }
    }

    #[test]
    fn bounded_resource_read_truncates_cleanly_at_utf8_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("review.md");
        // 5 ASCII bytes + 3-byte '€' + more bytes.
        let content = format!("hello€{}", "x".repeat(50));
        std::fs::write(&path, &content).unwrap();

        let file = File::open(&path).unwrap();
        // Limit 7 cuts right inside the 3-byte '€' (bytes 5, 6, 7).
        let text = read_bounded(file, 7).unwrap();
        assert!(
            text.starts_with("hello\n[truncated at 7 bytes]"),
            "{text:?}"
        );
        assert!(!text.contains('\u{FFFD}'));

        let file = File::open(&path).unwrap();
        let full = read_bounded(file, 1024).unwrap();
        assert_eq!(full, content);
    }
}
