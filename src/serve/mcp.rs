//! Model Context Protocol over stdio: newline-delimited JSON-RPC 2.0.

use super::{Server, tools};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{self, BufRead, Write},
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
pub fn serve<W: Write + Send + 'static>(
    server: Arc<Server>,
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
fn handle<W: Write + Send + 'static>(
    server: &Arc<Server>,
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
        "initialize" => response(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {
                    "name": "model-router",
                    "title": "model-router",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": INSTRUCTIONS,
            }),
        ),
        "ping" => response(id, json!({})),
        "tools/list" => response(id, json!({"tools": tools::list()})),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(error_response(id, INVALID_PARAMS, "Missing tool name"));
            };
            if !tools::exists(name) {
                return Some(error_response(
                    id,
                    INVALID_PARAMS,
                    &format!("Unknown tool: {name}"),
                ));
            }
            let blocking = tools::blocks(name);
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
                        let result = tools::call_with(
                            &server,
                            &name,
                            &arguments,
                            Some(&cancel),
                            &mut progress,
                        );
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
        Err(error) => json!({
            "content": [{"type": "text", "text": format!("{error:#}")}],
            "isError": true,
        }),
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
}
