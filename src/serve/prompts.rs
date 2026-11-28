//! MCP prompts and argument completion for `serve`: ready-made instructions
//! a client can hand its model, filled in with run data.

use super::{Server, mcp::ResourceError, tools};
use serde_json::{Value, json};

/// Most recent runs offered as `run_id` completions.
const COMPLETION_RUNS: usize = 50;

struct Prompt {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    /// `(name, description, required)`.
    arguments: &'static [(&'static str, &'static str, bool)],
}

const PROMPTS: &[Prompt] = &[
    Prompt {
        name: "run_task",
        title: "Run a task through a workflow",
        description: "Instructions to run a task through a model-router workflow and report the result.",
        arguments: &[
            ("task", "What the workflow should do.", true),
            ("workflow", "Workflow name. Default: the first.", false),
        ],
    },
    Prompt {
        name: "review_run",
        title: "Review a run",
        description: "A saved run's report and review brief, with instructions to review its outcome.",
        arguments: &[("run_id", "Run ID.", true)],
    },
    Prompt {
        name: "unblock_run",
        title: "Unblock a stopped run",
        description: "A stopped or waiting run's status and current handoff, with instructions to decide whether to confirm, retry, or report.",
        arguments: &[("run_id", "Run ID.", true)],
    },
];

pub fn list() -> Vec<Value> {
    PROMPTS
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "title": p.title,
                "description": p.description,
                "arguments": p.arguments.iter().map(|(name, description, required)| json!({
                    "name": name,
                    "description": description,
                    "required": required,
                })).collect::<Vec<_>>(),
            })
        })
        .collect()
}

fn text(message: String) -> Value {
    json!({"role": "user", "content": {"type": "text", "text": message}})
}

fn embedded(uri: String, mime: &str, text: String) -> Value {
    json!({"role": "user", "content": {"type": "resource", "resource": {"uri": uri, "mimeType": mime, "text": text}}})
}

fn call(server: &Server, tool: &str, arguments: Value) -> Result<Value, ResourceError> {
    tools::call_with(server, tool, &arguments, None, &mut |_| {})
        .map_err(|e| ResourceError::InvalidParams(format!("{e:#}")))
}

pub fn get(server: &Server, name: &str, arguments: &Value) -> Result<Value, ResourceError> {
    let Some(prompt) = PROMPTS.iter().find(|p| p.name == name) else {
        return Err(ResourceError::InvalidParams(format!(
            "Unknown prompt: {name}"
        )));
    };
    let arg = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    for (key, _, required) in prompt.arguments {
        if *required && arg(key).is_none() {
            return Err(ResourceError::InvalidParams(format!(
                "Prompt {name} needs argument {key}"
            )));
        }
    }
    let pretty = |value: &Value| serde_json::to_string_pretty(value).unwrap_or_default();
    let messages = match name {
        "run_task" => {
            let task = arg("task").unwrap_or_default();
            let workflow = arg("workflow");
            // Fails early on an unknown workflow or an untrusted project.
            let invalid = |e: anyhow::Error| ResourceError::InvalidParams(format!("{e:#}"));
            let (_, settings) = server.project_and_settings(None).map_err(invalid)?;
            settings.workflow(workflow).map_err(invalid)?;
            let start = match workflow {
                Some(w) => format!("start_run with workflow {w:?} and the task below"),
                None => "start_run with the task below (default workflow)".to_owned(),
            };
            vec![text(format!(
                "Use the model-router tools to run this task.\n\n\
                 1. Call {start}.\n\
                 2. Call wait_run with the run_id, timeout_seconds below your client's tool timeout; repeat while timed_out is true.\n\
                 3. If status is awaiting_confirmation, read the stage with get_handoff and tell me what it proposes before calling confirm_stage.\n\
                 4. If status is failed, blocked, or cancelled, read get_handoff for the current stage and tell me why; retry_stage only if I agree.\n\
                 5. When it completes, call get_report and summarize the outcome, checks, unresolved items, and changed files.\n\n\
                 Task:\n{task}"
            ))]
        }
        "review_run" => {
            let run_id = arg("run_id").unwrap_or_default();
            let report = call(server, "get_report", json!({"run_id": run_id}))?;
            let mut messages = vec![
                text(format!(
                    "Review model-router run {run_id}. Judge whether it did what its task asked, \
                     whether its checks and reviews support that, and what is still unresolved. \
                     List concrete problems first, then what a follow-up run should do."
                )),
                embedded(
                    format!("model-router://runs/{run_id}/report"),
                    "application/json",
                    pretty(&report),
                ),
            ];
            let review = format!("model-router://runs/{run_id}/review");
            if let Ok(content) = super::mcp::ToolSet::read_resource(server, &review) {
                messages.push(embedded(
                    review,
                    "text/markdown",
                    content["text"].as_str().unwrap_or_default().to_owned(),
                ));
            }
            messages
        }
        "unblock_run" => {
            let run_id = arg("run_id").unwrap_or_default();
            let status = call(server, "run_status", json!({"run_id": run_id}))?;
            let mut messages = vec![
                text(format!(
                    "model-router run {run_id} has status {}. Read its current stage's handoff below. \
                     Decide whether to accept it (confirm_stage, only for awaiting_confirmation), \
                     retry the stage (retry_stage, optionally with another model), or stop and \
                     explain the blocker to me. Say which and why before calling any tool.",
                    status["status"].as_str().unwrap_or("unknown")
                )),
                text(format!("run_status:\n{}", pretty(&status))),
            ];
            // By ID at the cursor: stage names need not be unique.
            let stage = status["cursor"]
                .as_u64()
                .and_then(|i| status["stages"].get(usize::try_from(i).ok()?))
                .and_then(|s| s["id"].as_str());
            if let Some(stage) = stage {
                if let Ok(handoff) = call(
                    server,
                    "get_handoff",
                    json!({"run_id": run_id, "stage": stage}),
                ) {
                    messages.push(text(format!(
                        "get_handoff for stage {stage}:\n{}",
                        pretty(&handoff)
                    )));
                }
            }
            messages
        }
        _ => unreachable!("listed in PROMPTS"),
    };
    Ok(json!({"description": prompt.description, "messages": messages}))
}

/// Workflow names and recent run IDs that start with `value`.
pub fn complete(server: &Server, reference: &Value, argument: &str, value: &str) -> Vec<String> {
    let known = match reference.get("type").and_then(Value::as_str) {
        Some("ref/prompt") => {
            let name = reference.get("name").and_then(Value::as_str);
            PROMPTS
                .iter()
                .find(|p| Some(p.name) == name)
                .is_some_and(|p| p.arguments.iter().any(|(a, _, _)| *a == argument))
        }
        Some("ref/resource") => argument == "run_id",
        _ => false,
    };
    if !known {
        return vec![];
    }
    let candidates: Vec<String> = match argument {
        "workflow" => server
            .project_and_settings(None)
            .map(|(_, settings)| settings.workflows.into_iter().map(|w| w.name).collect())
            .unwrap_or_default(),
        "run_id" => server
            .project(None)
            .and_then(|project| crate::history::list(&project))
            .map(|entries| {
                entries
                    .into_iter()
                    .filter_map(|entry| match entry {
                        crate::history::Entry::Summary(summary) => Some(summary.id),
                        crate::history::Entry::Damaged { .. } => None,
                    })
                    .take(COMPLETION_RUNS)
                    .collect()
            })
            .unwrap_or_default(),
        _ => vec![],
    };
    candidates
        .into_iter()
        .filter(|c| c.starts_with(value))
        .collect()
}
