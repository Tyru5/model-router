//! `model-router serve`: MCP over stdio and the HTTP API, driven offline
//! with the mock harness.
use model_router::config::{CustomAdapter, Mode, Role, Settings, Stage, Workflow};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

fn mock_adapter(extra: &[&str]) -> CustomAdapter {
    let mut args = vec![
        format!("{}/examples/mock_harness.py", env!("CARGO_MANIFEST_DIR")),
        "--role".into(),
        "{provider}".into(),
        "--model".into(),
        "{model}".into(),
        "--prompt-file".into(),
        "{prompt_file}".into(),
        "--handoff-file".into(),
        "{handoff_file}".into(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    CustomAdapter {
        command: "python3".into(),
        interactive_args: Some(args.clone()),
        args,
        ..CustomAdapter::default()
    }
}

fn stage(id: &str, role: Role, harness: &str, mode: Mode) -> Stage {
    Stage {
        id: id.into(),
        name: id.into(),
        role,
        harness: harness.into(),
        provider: id.into(),
        model: "test-model".into(),
        mode,
        ..Stage::default()
    }
}

/// A project with workflows that are quick, interactive, slow, and steady (1 s).
fn project() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("serve test ")
        .tempdir()
        .unwrap();
    let summary = Stage {
        enabled: false,
        ..stage("summary", Role::Summary, "mock", Mode::Unattended)
    };
    let workflow = |name: &str, stages: Vec<Stage>| Workflow {
        name: name.into(),
        max_repair_passes: 2,
        sandbox_default: Default::default(),
        budget: Default::default(),
        stages: stages.into_iter().chain([summary.clone()]).collect(),
    };
    let settings = Settings {
        workflows: vec![
            workflow(
                "Quick",
                vec![
                    stage("research", Role::Research, "mock", Mode::Unattended),
                    stage("implement", Role::Implementation, "mock", Mode::Unattended),
                ],
            ),
            workflow(
                "Pair",
                vec![stage(
                    "implement",
                    Role::Implementation,
                    "mock",
                    Mode::Interactive,
                )],
            ),
            workflow(
                "Slow",
                vec![stage(
                    "implement",
                    Role::Implementation,
                    "slowmock",
                    Mode::Unattended,
                )],
            ),
            workflow(
                "Steady",
                vec![stage(
                    "implement",
                    Role::Implementation,
                    "steadymock",
                    Mode::Unattended,
                )],
            ),
        ],
        adapters: BTreeMap::from([
            ("mock".into(), mock_adapter(&[])),
            ("slowmock".into(), mock_adapter(&["--delay", "60"])),
            ("steadymock".into(), mock_adapter(&["--delay", "1"])),
        ]),
        ..Settings::default()
    };
    settings
        .save(&dir.path().join(".model-router.toml"))
        .unwrap();
    // The project config defines custom adapters, so the user trusts it once.
    let trusted = Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("-C")
        .arg(dir.path())
        .args(["trust", "--yes"])
        .env("MODEL_ROUTER_CONFIG_DIR", dir.path().join("config-home"))
        .output()
        .unwrap();
    assert!(trusted.status.success(), "{trusted:?}");
    dir
}

struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    next: u64,
}

impl Mcp {
    fn spawn(project: &Path, extra: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_model-router"))
            .arg("-C")
            .arg(project)
            .env("MODEL_ROUTER_CONFIG_DIR", project.join("config-home"))
            .args(["serve", "--mcp"])
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            lines,
            next: 0,
        }
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn read(&self) -> Value {
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(90))
            .expect("server response");
        // Stdout must carry JSON-RPC only.
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("non-JSON stdout {line:?}: {e}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let response = self.read();
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], id, "{response}");
        response
    }

    /// A tool's structured result; panics on a tool error.
    fn call(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.call_raw(name, arguments);
        assert_eq!(result["isError"], false, "{name}: {result}");
        let text: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, result["structuredContent"]);
        result["structuredContent"].clone()
    }

    fn call_raw(&mut self, name: &str, arguments: Value) -> Value {
        let response = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        response["result"].clone()
    }

    /// A tool error's text.
    fn error(&mut self, name: &str, arguments: Value) -> String {
        let result = self.call_raw(name, arguments);
        assert_eq!(result["isError"], true, "{name} should fail: {result}");
        result["content"][0]["text"].as_str().unwrap().to_owned()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn initialize(mcp: &mut Mcp) {
    let init = mcp.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "model-router");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    mcp.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
}

#[test]
fn mcp_runs_a_workflow_to_a_report() {
    let dir = project();
    let mut mcp = Mcp::spawn(dir.path(), &[]);
    initialize(&mut mcp);
    assert_eq!(mcp.request("ping", json!({}))["result"], json!({}));

    let tools = mcp.request("tools/list", json!({}))["result"]["tools"].clone();
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "list_workflows",
        "preview_run",
        "start_run",
        "run_status",
        "wait_run",
        "cancel_run",
        "list_runs",
        "get_handoff",
        "get_report",
        "confirm_stage",
        "retry_stage",
    ] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }
    for tool in tools.as_array().unwrap() {
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
    }

    let workflows = mcp.call("list_workflows", json!({}));
    assert_eq!(workflows["workflows"][0]["name"], "Quick");
    assert_eq!(workflows["workflows"][0]["default"], true);
    assert_eq!(
        workflows["workflows"][1]["stages"][0]["mode"],
        "interactive"
    );

    let preview = mcp.call("preview_run", json!({"task": "Add a widget"}));
    let text = preview["preview"].as_str().unwrap();
    assert!(text.contains("mock_harness.py"), "{text}");
    assert!(text.contains("Task: Add a widget"), "{text}");

    let started = mcp.call(
        "start_run",
        json!({
            "task": "Add a widget",
            "overrides": [{"stage": "implement", "model": "override-model"}]
        }),
    );
    let id = started["run_id"].as_str().unwrap().to_owned();
    assert_eq!(started["worktree"], false);

    let done = mcp.call("wait_run", json!({"run_id": id, "timeout_seconds": 60}));
    assert_eq!(done["status"], "complete", "{done}");
    assert_eq!(done["timed_out"], false);
    assert_eq!(done["active"], false);
    assert_eq!(done["stages"][1]["model"], "override-model");
    let latest = &done["latest_handoff"];
    assert_eq!(latest["stage"], "implement", "{done}");
    assert!(
        latest["summary"]
            .as_str()
            .unwrap()
            .contains("model override-model"),
        "{latest}"
    );

    let report = mcp.call("get_report", json!({"run_id": id}));
    assert_eq!(report["id"], id.as_str());
    assert_eq!(report["status"], "complete");
    assert_eq!(report["stages"].as_array().unwrap().len(), 3);
    assert!(report["changes"]["state"].is_string(), "{report}");
    assert!(report["summary"].as_str().unwrap().contains("attempts"));

    let handoff = mcp.call("get_handoff", json!({"run_id": id, "stage": "research"}));
    assert_eq!(handoff["handoff"]["verdict"], "complete");
    assert_eq!(handoff["attempts"].as_array().unwrap().len(), 1);
    let by_number = mcp.call("get_handoff", json!({"run_id": id, "stage": "2"}));
    assert_eq!(by_number["stage"]["id"], "implement");

    let runs = mcp.call("list_runs", json!({"status": "complete"}));
    assert_eq!(runs["total"], 1);
    assert_eq!(runs["runs"][0]["id"], id.as_str());
    let none = mcp.call("list_runs", json!({"status": "failed"}));
    assert_eq!(none["total"], 0);

    // Stdin EOF ends the server cleanly.
    drop(mcp.stdin.take());
    let status = mcp.child.wait().unwrap();
    assert!(status.success(), "{status}");
}

#[test]
fn mcp_reports_errors_and_cancels() {
    let dir = project();
    let outside = tempfile::tempdir().unwrap();
    let mut mcp = Mcp::spawn(dir.path(), &[]);
    initialize(&mut mcp);

    // Interactive stages are refused unless forced.
    let error = mcp.error("start_run", json!({"workflow": "Pair", "task": "x"}));
    assert!(error.contains("force_unattended"), "{error}");
    let error = mcp.error(
        "start_run",
        json!({"task": "x", "overrides": [{"stage": "nope"}]}),
    );
    assert!(error.contains("no stage"), "{error}");
    let error = mcp.error("get_report", json!({"run_id": "20990101-000000-deadbeef"}));
    assert!(error.contains("No readable run"), "{error}");
    let error = mcp.error("get_report", json!({"run_id": "../escape"}));
    assert!(error.contains("Invalid run ID"), "{error}");
    let error = mcp.error(
        "list_runs",
        json!({"project": outside.path().to_str().unwrap()}),
    );
    assert!(
        error.contains("outside the server's allowed roots"),
        "{error}"
    );
    let error = mcp.error("list_runs", json!({"bogus": 1}));
    assert!(error.contains("Invalid arguments"), "{error}");
    let unknown = mcp.request("tools/call", json!({"name": "nope", "arguments": {}}));
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");
    let missing = mcp.request("no/such/method", json!({}));
    assert_eq!(missing["error"]["code"], -32601, "{missing}");
    mcp.send(json!("not an object"));
    assert_eq!(mcp.read()["error"]["code"], -32600);

    // Forced unattended, the interactive workflow completes.
    let forced = mcp.call(
        "start_run",
        json!({"workflow": "Pair", "task": "x", "force_unattended": true}),
    );
    let id = forced["run_id"].as_str().unwrap().to_owned();
    let done = mcp.call("wait_run", json!({"run_id": id, "timeout_seconds": 60}));
    assert_eq!(done["status"], "complete", "{done}");
    assert_eq!(done["stages"][0]["mode"], "unattended");
    let error = mcp.error("cancel_run", json!({"run_id": id}));
    assert!(error.contains("not running"), "{error}");

    // A slow run: a short wait times out, cancel stops it, retry restarts it.
    let slow = mcp.call("start_run", json!({"workflow": "Slow", "task": "x"}));
    let id = slow["run_id"].as_str().unwrap().to_owned();
    let waited = mcp.call("wait_run", json!({"run_id": id, "timeout_seconds": 1}));
    assert_eq!(waited["timed_out"], true, "{waited}");
    assert_eq!(waited["active"], true);
    let error = mcp.error("retry_stage", json!({"run_id": id}));
    assert!(error.contains("is running"), "{error}");
    // The checkout is busy and not a Git repository: no worktree to fall back to.
    let error = mcp.error("start_run", json!({"task": "y"}));
    assert!(error.contains("not a Git repository"), "{error}");
    let cancelled = mcp.call("cancel_run", json!({"run_id": id}));
    assert_eq!(cancelled["cancelling"], true);
    let stopped = mcp.call("wait_run", json!({"run_id": id, "timeout_seconds": 60}));
    assert_eq!(stopped["status"], "cancelled", "{stopped}");
    assert!(stopped["next"].as_str().unwrap().contains("retry_stage"));
    let error = mcp.error("confirm_stage", json!({"run_id": id}));
    assert!(error.contains("No handoff awaits confirmation"), "{error}");
    let retried = mcp.call("retry_stage", json!({"run_id": id}));
    assert_eq!(retried["active"], true, "{retried}");
    mcp.call("cancel_run", json!({"run_id": id}));
    let stopped = mcp.call("wait_run", json!({"run_id": id, "timeout_seconds": 60}));
    assert_eq!(stopped["status"], "cancelled", "{stopped}");
    assert_eq!(stopped["stages"][0]["attempts"], 2);
}

fn http(addr: &str, method: &str, path: &str, token: Option<&str>, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(90)))
        .unwrap();
    let auth = token.map_or_else(String::new, |t| format!("Authorization: Bearer {t}\r\n"));
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let code = response[9..12].parse().unwrap();
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (code, body)
}

#[test]
fn http_api_requires_its_token_and_streams_events() {
    let dir = project();
    let token = "test-token-123";
    let mut child = Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("-C")
        .arg(dir.path())
        .env("MODEL_ROUTER_CONFIG_DIR", dir.path().join("config-home"))
        .args(["serve", "--http", "127.0.0.1:0"])
        .env("MODEL_ROUTER_SERVE_TOKEN", token)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    struct Kill(Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let stderr = child.stderr.take().unwrap();
    let _child = Kill(child);
    let mut lines = BufReader::new(stderr).lines();
    let addr = loop {
        let line = lines.next().expect("server announces its address").unwrap();
        if let Some(rest) = line.split_once("http://").map(|(_, r)| r) {
            assert!(!line.contains(token), "env token must not be printed");
            break rest.split_whitespace().next().unwrap().to_owned();
        }
    };

    let (code, _) = http(&addr, "GET", "/tools", None, "");
    assert_eq!(code, 401);
    let (code, _) = http(&addr, "GET", "/tools", Some("wrong"), "");
    assert_eq!(code, 401);
    let (code, body) = http(&addr, "GET", "/tools", Some(token), "");
    assert_eq!(code, 200);
    assert!(body.contains("start_run"));
    let (code, body) = http(&addr, "POST", "/tools/list_workflows", Some(token), "");
    assert_eq!(code, 200, "{body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["result"]["workflows"][0]["name"], "Quick");
    let (code, body) = http(&addr, "POST", "/tools/get_report", Some(token), "{}");
    assert_eq!(code, 400, "{body}");
    let (code, _) = http(&addr, "POST", "/tools/nope", Some(token), "{}");
    assert_eq!(code, 404);

    let (code, body) = http(
        &addr,
        "POST",
        "/tools/start_run",
        Some(token),
        r#"{"workflow": "Steady", "task": "Stream me"}"#,
    );
    assert_eq!(code, 200, "{body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    let id = value["result"]["run_id"].as_str().unwrap().to_owned();
    let (code, events) = http(&addr, "GET", &format!("/runs/{id}/events"), Some(token), "");
    assert_eq!(code, 200);
    assert!(events.starts_with("event: state\n"), "{events}");
    assert!(events.contains("event: end\n"), "{events}");
    assert!(events.contains("event: output\n"), "{events}");
    let end = events
        .split("event: end\ndata: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap();
    let end: Value = serde_json::from_str(end).unwrap();
    assert_eq!(end["status"], "complete", "{end}");
}

#[test]
fn mcp_answers_calls_in_flight_when_stdin_closes() {
    let dir = project();
    let mut child = Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("-C")
        .arg(dir.path())
        .env("MODEL_ROUTER_CONFIG_DIR", dir.path().join("config-home"))
        .args(["serve", "--mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let call = json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
        "params": {"name": "list_workflows", "arguments": {}}});
    writeln!(child.stdin.take().unwrap(), "{call}").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let response: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(response["id"], 7);
    assert_eq!(response["result"]["isError"], false, "{response}");
}
