//! Model Context Protocol over stdio: newline-delimited JSON-RPC 2.0.

use super::{Server, tools};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    io::{BufRead, Write},
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

pub const PROTOCOL_VERSION: &str = "2025-06-18";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

const INSTRUCTIONS: &str = "model-router runs a task through a workflow of agentic harness stages. \
Typical use: list_workflows, optionally preview_run, then start_run and wait_run (repeat while timed_out) \
and get_report. A run waiting at a checkpoint (status awaiting_confirmation) continues with confirm_stage; \
a failed or blocked one with retry_stage.";

/// Reads requests until EOF. Tool calls run on their own threads so a long
/// `wait_run` never blocks `cancel_run`; responses may arrive out of order.
/// Returns the tool calls still in flight, so shutdown can let them answer.
pub fn serve<W: Write + Send + 'static>(
    server: Arc<Server>,
    input: impl BufRead,
    output: W,
) -> Result<Vec<JoinHandle<()>>> {
    let output = Arc::new(Mutex::new(output));
    let mut calls: Vec<JoinHandle<()>> = vec![];
    for line in input.lines() {
        calls.retain(|call| !call.is_finished());
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                send(
                    &output,
                    &error_response(Value::Null, PARSE_ERROR, &format!("Parse error: {error}")),
                );
                continue;
            }
        };
        if let Some(response) = handle(&server, &output, message, &mut calls) {
            send(&output, &response);
        }
    }
    Ok(calls)
}

fn send<W: Write>(output: &Mutex<W>, message: &Value) {
    let mut output = output.lock().unwrap_or_else(|e| e.into_inner());
    // Compact JSON escapes newlines, so each message is one line.
    let line = message.to_string();
    if writeln!(output, "{line}")
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

/// The response to send now, if any. Tool calls answer later from a thread.
fn handle<W: Write + Send + 'static>(
    server: &Arc<Server>,
    output: &Arc<Mutex<W>>,
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
    // Notifications get no response.
    let id = id?;
    let params = object.get("params").cloned().unwrap_or(Value::Null);
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
            let name = name.to_owned();
            let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
            let (server, output) = (server.clone(), output.clone());
            let spawned = std::thread::Builder::new()
                .name(format!("tool {name}"))
                .spawn({
                    let id = id.clone();
                    move || {
                        let result = tool_result(tools::call(&server, &name, &arguments));
                        send(&output, &response(id, result));
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
