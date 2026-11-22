//! The delegation tools a parent's harness calls through the bridge. Each
//! call becomes a request to the stage's owner loop, the only thread that
//! reads or changes the run; this side only waits.
use super::instructions::ParentKind;
use crate::serve::mcp::{ToolError, ToolSet};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// The longest a blocking tool call waits.
pub const MAX_WAIT: Duration = Duration::from_secs(45);

/// What a tool call asks the owner loop.
#[derive(Debug, Clone)]
pub enum Request {
    Capabilities {
        search: Option<String>,
    },
    /// Starts a task; the reply is its view.
    Delegate {
        arguments: Value,
    },
    /// The views of `ids`. With `deliver`, terminal and waiting ones are
    /// marked delivered.
    Status {
        ids: Vec<String>,
        deliver: bool,
    },
    Cancel {
        id: String,
        reason: Option<String>,
    },
    Send {
        arguments: Value,
    },
    Apply {
        arguments: Value,
    },
    /// Run and schedule tools (`delegation.runs`, `delegation.schedules`).
    Other {
        tool: String,
        arguments: Value,
    },
}

pub type Reply = std::result::Result<Value, ToolError>;

/// One call waiting for the owner loop's reply.
pub struct Call {
    pub request: Request,
    pub reply: mpsc::Sender<Reply>,
}

/// Bumped by the owner loop on every task change, so waits wake.
#[derive(Default)]
pub struct Changed {
    generation: Mutex<u64>,
    signal: Condvar,
}
impl Changed {
    pub fn bump(&self) {
        let mut generation = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        *generation += 1;
        self.signal.notify_all();
    }
    fn current(&self) -> u64 {
        *self.generation.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Waits until the generation passes `seen` or `limit` elapses.
    fn wait(&self, seen: u64, limit: Duration) {
        let guard = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self
            .signal
            .wait_timeout_while(guard, limit, |generation| *generation == seen);
    }
}

/// The tools of one parent turn.
pub struct DelegationTools {
    submit: Box<dyn Fn(Call) -> bool + Send + Sync>,
    close: Arc<AtomicBool>,
    changed: Arc<Changed>,
    kind: ParentKind,
    runs: bool,
    schedules: bool,
}

impl DelegationTools {
    pub fn new(
        submit: impl Fn(Call) -> bool + Send + Sync + 'static,
        close: Arc<AtomicBool>,
        changed: Arc<Changed>,
        kind: ParentKind,
        runs: bool,
        schedules: bool,
    ) -> Self {
        Self {
            submit: Box::new(submit),
            close,
            changed,
            kind,
            runs,
            schedules,
        }
    }

    fn closed(&self) -> ToolError {
        ToolError::new(
            "parent_not_active",
            "This stage turn is over or its tools were closed",
        )
    }

    /// Sends `request` to the owner loop and waits for its answer; a closed
    /// broker answers itself, so no call outlives the turn.
    fn ask(&self, request: Request) -> Reply {
        if self.close.load(Ordering::SeqCst) {
            return Err(self.closed());
        }
        let (reply, answer) = mpsc::channel();
        if !(self.submit)(Call { request, reply }) {
            return Err(self.closed());
        }
        loop {
            match answer.recv_timeout(Duration::from_millis(200)) {
                Ok(reply) => return reply,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(self.closed()),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.close.load(Ordering::SeqCst) {
                        return Err(self.closed());
                    }
                }
            }
        }
    }

    /// Waits until one of `ids` is terminal or waiting on a question, or
    /// `limit` passes; returns every listed task (delivering the ready ones)
    /// and whether the wait timed out.
    fn wait_for(&self, ids: Vec<String>, limit: Duration, cancel: Option<&AtomicBool>) -> Reply {
        let deadline = Instant::now() + limit.min(MAX_WAIT);
        loop {
            let seen = self.changed.current();
            let views = self.ask(Request::Status {
                ids: ids.clone(),
                deliver: false,
            })?;
            let ready = views
                .as_array()
                .is_some_and(|tasks| tasks.iter().any(ready_view));
            if ready {
                let views = self.ask(Request::Status { ids, deliver: true })?;
                return Ok(json!({"tasks": views, "wait_timed_out": false}));
            }
            let now = Instant::now();
            if now >= deadline || cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return Ok(json!({"tasks": views, "wait_timed_out": true}));
            }
            if self.close.load(Ordering::SeqCst) {
                return Err(self.closed());
            }
            self.changed
                .wait(seen, (deadline - now).min(Duration::from_millis(500)));
        }
    }
}

/// A task view that a wait returns for: terminal or waiting.
fn ready_view(view: &Value) -> bool {
    matches!(
        view.get("work_state").and_then(Value::as_str),
        Some("needs_answer" | "result_available")
    )
}

fn string_arg(arguments: &Value, name: &str) -> std::result::Result<String, ToolError> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ToolError::new("invalid_request", format!("{name} is required")))
}

fn timeout_arg(arguments: &Value) -> Duration {
    arguments
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .map_or(MAX_WAIT, Duration::from_millis)
        .min(MAX_WAIT)
}

const RUN_TOOLS: [&str; 6] = [
    "run_launch",
    "run_list",
    "run_read",
    "run_wait",
    "run_retry",
    "run_cancel",
];
const SCHEDULE_TOOLS: [&str; 3] = [
    "schedule_task",
    "list_scheduled_tasks",
    "delete_scheduled_task",
];

impl ToolSet for DelegationTools {
    fn instructions(&self) -> &str {
        "model-router delegation: hand self-contained tasks to other harnesses and models. Call orchestrator_capabilities first."
    }

    fn list(&self) -> Vec<Value> {
        let mut tools = delegation_tools();
        if self.runs {
            tools.extend(run_tools());
        }
        if self.schedules {
            tools.extend(schedule_tools());
        }
        tools
    }

    fn exists(&self, name: &str) -> bool {
        self.list().iter().any(|t| t["name"] == name)
    }

    fn blocks(&self, name: &str) -> bool {
        matches!(name, "delegate_task" | "task_wait" | "run_wait")
    }

    fn call(
        &self,
        name: &str,
        arguments: &Value,
        cancel: Option<&AtomicBool>,
        _progress: &mut dyn FnMut(&Value),
    ) -> Result<Value> {
        let reply = match name {
            "orchestrator_capabilities" => self.ask(Request::Capabilities {
                search: arguments
                    .get("search")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }),
            "delegate_task" => self
                .ask(Request::Delegate {
                    arguments: arguments.clone(),
                })
                .and_then(|view| {
                    let wait = arguments.get("mode").and_then(Value::as_str) == Some("wait")
                        || self.kind == ParentKind::NoWake;
                    let id = view["task_id"].as_str().unwrap_or_default().to_owned();
                    if !wait || ready_view(&view) || id.is_empty() {
                        return Ok(view);
                    }
                    let waited = self.wait_for(vec![id], timeout_arg(arguments), cancel)?;
                    let mut view = waited["tasks"][0].clone();
                    view["wait_timed_out"] = waited["wait_timed_out"].clone();
                    Ok(view)
                }),
            "task_status" => string_arg(arguments, "task_id").and_then(|id| {
                self.ask(Request::Status {
                    ids: vec![id],
                    deliver: true,
                })
                .map(|views| views[0].clone())
            }),
            "task_wait" => {
                let ids: Vec<String> = arguments
                    .get("task_ids")
                    .and_then(Value::as_array)
                    .map(|ids| {
                        ids.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                if ids.is_empty() {
                    Err(ToolError::new("invalid_request", "task_ids is required"))
                } else {
                    self.wait_for(ids, timeout_arg(arguments), cancel)
                }
            }
            "task_cancel" => string_arg(arguments, "task_id").and_then(|id| {
                self.ask(Request::Cancel {
                    id,
                    reason: arguments
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                })
            }),
            "task_send" => self.ask(Request::Send {
                arguments: arguments.clone(),
            }),
            "task_apply" => self.ask(Request::Apply {
                arguments: arguments.clone(),
            }),
            name if RUN_TOOLS.contains(&name) || SCHEDULE_TOOLS.contains(&name) => {
                self.ask(Request::Other {
                    tool: name.to_owned(),
                    arguments: arguments.clone(),
                })
            }
            other => Err(ToolError::new(
                "invalid_request",
                format!("Unknown tool {other}"),
            )),
        };
        reply.map_err(Into::into)
    }
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
    })
}

fn client_request_id() -> Value {
    json!({"type": "string", "description": "Stable key for retries of this one call."})
}

pub fn delegation_tools() -> Vec<Value> {
    vec![
        tool(
            "orchestrator_capabilities",
            "Targets this stage may delegate to (named delegates and allowed harnesses with models), its limits, and what remains of them. Call once before the first delegate_task.",
            json!({"search": {"type": "string", "description": "Filter models by substring."}}),
            &[],
        ),
        tool(
            "delegate_task",
            "Start one task on a listed target. The task sees only `task`, never this conversation. Returns its task_id and state.",
            json!({
                "task": {"type": "string", "description": "Self-contained brief: goal, files, constraints, what to return."},
                "target": {
                    "type": "object",
                    "description": "{\"delegate\": NAME} or {\"harness\": H, \"model\"?: M, \"effort\"?: E, \"provider\"?: P}. Omitted: the stage's first named delegate.",
                    "properties": {
                        "delegate": {"type": "string"},
                        "harness": {"type": "string"},
                        "model": {"type": "string"},
                        "effort": {"type": "string"},
                        "provider": {"type": "string"}
                    },
                    "additionalProperties": false
                },
                "title": {"type": "string"},
                "role": {"type": "string", "enum": ["research", "planning", "implementation", "review", "test"]},
                "isolation": {"type": "string", "enum": ["shared", "worktree"], "description": "shared: read-only in your checkout. worktree: its own checkout; apply its patch with task_apply."},
                "mode": {"type": "string", "enum": ["async", "wait"]},
                "timeout_ms": {"type": "integer", "description": "mode wait only; at most 45000. A timeout cancels nothing."},
                "client_request_id": client_request_id()
            }),
            &["task"],
        ),
        tool(
            "task_status",
            "One task's state and, once finished, its result. Do not poll a running task.",
            json!({"task_id": {"type": "string"}}),
            &["task_id"],
        ),
        tool(
            "task_wait",
            "Wait until any listed task finishes or asks a question (at most 45 s), then return every listed task. A timeout cancels nothing.",
            json!({
                "task_ids": {"type": "array", "items": {"type": "string"}},
                "timeout_ms": {"type": "integer"}
            }),
            &["task_ids"],
        ),
        tool(
            "task_cancel",
            "Stop a task you no longer need.",
            json!({"task_id": {"type": "string"}, "reason": {"type": "string"}, "client_request_id": client_request_id()}),
            &["task_id"],
        ),
        tool(
            "task_send",
            "Send a follow-up or an answer to a task's session. queue: after its current turn. restart: stop the running turn first.",
            json!({
                "task_id": {"type": "string"},
                "message": {"type": "string"},
                "mode": {"type": "string", "enum": ["queue", "restart"]},
                "client_request_id": client_request_id()
            }),
            &["task_id", "message"],
        ),
        tool(
            "task_apply",
            "Apply one sealed patch revision of a worktree task to your checkout.",
            json!({
                "task_id": {"type": "string"},
                "revision": {"type": "integer"},
                "client_request_id": client_request_id()
            }),
            &["task_id", "revision"],
        ),
    ]
}

pub fn run_tools() -> Vec<Value> {
    vec![
        tool(
            "run_launch",
            "Create 1-20 new model-router runs, each in its own worktree. Only when the user asks for a new run.",
            json!({
                "runs": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"workflow": {"type": "string"}, "task": {"type": "string"}},
                        "required": ["task"],
                        "additionalProperties": false
                    }
                },
                "client_request_id": client_request_id()
            }),
            &["runs"],
        ),
        tool(
            "run_list",
            "This project's runs, newest first.",
            json!({"status": {"type": "string"}, "offset": {"type": "integer"}, "limit": {"type": "integer"}}),
            &[],
        ),
        tool(
            "run_read",
            "A run's status, stages, handoff summaries, and stage output tail (about 30,000 characters).",
            json!({"run_id": {"type": "string"}, "stage": {"type": "string"}, "cursor": {"type": "integer"}}),
            &["run_id"],
        ),
        tool(
            "run_wait",
            "Wait until a run completes, stops, or waits for a decision (at most 45 s).",
            json!({"run_id": {"type": "string"}, "timeout_ms": {"type": "integer"}}),
            &["run_id"],
        ),
        tool(
            "run_retry",
            "Retry the current stage of a stopped run this stage launched.",
            json!({"run_id": {"type": "string"}, "client_request_id": client_request_id()}),
            &["run_id"],
        ),
        tool(
            "run_cancel",
            "Cancel a run this stage launched.",
            json!({"run_id": {"type": "string"}, "client_request_id": client_request_id()}),
            &["run_id"],
        ),
    ]
}

pub fn schedule_tools() -> Vec<Value> {
    vec![
        tool(
            "schedule_task",
            "Create or update (schedule_id) a schedule that launches a run. It fires only after the user approves it. Only when the user asks for a schedule.",
            json!({
                "schedule_id": {"type": "string"},
                "workflow": {"type": "string"},
                "task": {"type": "string"},
                "every": {"type": "string", "description": "e.g. 30m, 6h, 1d"},
                "at": {"type": "string", "description": "HH:MM daily, server local time"},
                "webhook": {"type": "boolean"},
                "enabled": {"type": "boolean"},
                "client_request_id": client_request_id()
            }),
            &[],
        ),
        tool(
            "list_scheduled_tasks",
            "This project's schedules, with approval and executor state.",
            json!({}),
            &[],
        ),
        tool(
            "delete_scheduled_task",
            "Delete a schedule permanently.",
            json!({"schedule_id": {"type": "string"}}),
            &["schedule_id"],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listed_tools_follow_the_stage_s_permissions() {
        let tools = |runs, schedules| {
            DelegationTools::new(
                |_| false,
                Arc::new(AtomicBool::new(false)),
                Arc::new(Changed::default()),
                ParentKind::Wake,
                runs,
                schedules,
            )
        };
        let names = |t: &DelegationTools| -> Vec<String> {
            t.list()
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_owned())
                .collect()
        };
        let plain = tools(false, false);
        assert!(names(&plain).contains(&"delegate_task".to_owned()));
        assert!(!plain.exists("run_launch"));
        assert!(tools(true, false).exists("run_launch"));
        assert!(tools(false, true).exists("schedule_task"));
        // A closed owner answers parent_not_active, never hangs.
        let error = plain
            .call("task_status", &json!({"task_id": "t1"}), None, &mut |_| {})
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ToolError>().unwrap().code,
            "parent_not_active"
        );
    }
}
