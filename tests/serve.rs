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
    time::{Duration, Instant},
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

/// A project with workflows that are quick, interactive, slow, steady (1 s),
/// and paced (three 1 s stages).
fn project() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("serve test ")
        .tempdir()
        .unwrap();
    let summary = Stage {
        enabled: false,
        ..stage("summary", Role::Summary, "mock", Mode::Unattended)
    };
    let workflow = |name: &str, stages: Vec<Stage>| {
        Workflow::new(name, stages.into_iter().chain([summary.clone()]).collect())
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
            // Three 1 s stages: its state changes while a client waits.
            workflow(
                "Paced",
                vec![
                    stage("research", Role::Research, "steadymock", Mode::Unattended),
                    stage("plan", Role::Planning, "steadymock", Mode::Unattended),
                    stage(
                        "implement",
                        Role::Implementation,
                        "steadymock",
                        Mode::Unattended,
                    ),
                ],
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
    assert!(init["result"]["capabilities"]["resources"].is_object());
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
        "fork_run",
        "apply_run",
        "get_handoffs",
        "get_cost",
        "get_stats",
        "list_schedules",
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
    // A line that is not UTF-8 is a parse error; the requests below still
    // get answers.
    let stdin = mcp.stdin.as_mut().unwrap();
    stdin.write_all(b"{\"id\": \"\xff\"}\n").unwrap();
    stdin.flush().unwrap();
    assert_eq!(mcp.read()["error"]["code"], -32700);

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
    // The retry's attempt starts on the driver thread; a cancel before then
    // stops the run with one attempt.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status = mcp.call("run_status", json!({"run_id": id}));
        if status["stages"][0]["attempts"] == 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the retry started no attempt: {status}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
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
fn server_recovery_refuses_tampered_saved_commands() {
    let dir = project();
    let mut mcp = Mcp::spawn(dir.path(), &[]);
    initialize(&mut mcp);
    let started = mcp.call("start_run", json!({"task": "Replay safety", "overrides": [{"stage": "research", "model": "fail-once"}]}));
    let id = started["run_id"].as_str().unwrap();
    let failed = mcp.call("wait_run", json!({"run_id": id, "timeout_seconds": 60}));
    assert_eq!(failed["status"], "failed");
    let path = dir
        .path()
        .join(".model-router/runs")
        .join(id)
        .join("run.json");
    let original = std::fs::read(&path).unwrap();
    let mut saved: Value = serde_json::from_slice(&original).unwrap();
    let marker = dir.path().join("unapproved-server-command");
    saved["adapters"]["mock"]["command"] = json!("touch");
    saved["adapters"]["mock"]["args"] = json!([marker]);
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    let refused = mcp.error("retry_stage", json!({"run_id": id}));
    assert!(
        refused.contains("Saved run executable settings are untrusted"),
        "{refused}"
    );
    assert!(!marker.exists());
    std::fs::write(&path, original).unwrap();
    let trust = |action| {
        Command::new(env!("CARGO_BIN_EXE_model-router"))
            .arg("-C")
            .arg(dir.path())
            .args(["trust", action])
            .env("MODEL_ROUTER_CONFIG_DIR", dir.path().join("config-home"))
            .output()
            .unwrap()
    };
    assert!(trust("--revoke").status.success());
    assert!(
        mcp.error("retry_stage", json!({"run_id": id}))
            .contains("Untrusted project config")
    );
    assert!(trust("--yes").status.success());
    mcp.call("retry_stage", json!({"run_id": id}));
    let done = mcp.call("wait_run", json!({"run_id": id, "timeout_seconds": 60}));
    assert_eq!(done["status"], "complete", "{done}");
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
    // Authentication must not wait for a declared body that never arrives.
    let mut missing_body = TcpStream::connect(&addr).unwrap();
    missing_body
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    missing_body
        .write_all(b"POST /tools/list_workflows HTTP/1.1\r\nContent-Length: 100\r\n\r\n")
        .unwrap();
    let mut denied = String::new();
    missing_body.read_to_string(&mut denied).unwrap();
    assert!(denied.starts_with("HTTP/1.1 401"), "{denied}");

    // Idle connections that never finish a request cannot lock out a
    // client that sends one: past the pending limit the oldest are cut off.
    // A client whose headers authenticated is no longer pending, even while
    // its body is still on the way.
    let mut sending = TcpStream::connect(&addr).unwrap();
    sending
        .write_all(
            format!(
                "POST /tools/list_workflows HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 2\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let mut slow = Vec::new();
    for _ in 0..40 {
        let mut stream = TcpStream::connect(&addr).unwrap();
        stream
            .write_all(b"GET /tools HTTP/1.1\r\nX-Slow: ")
            .unwrap();
        slow.push(stream);
    }
    std::thread::sleep(Duration::from_millis(100));
    let (code, body) = http(&addr, "GET", "/tools", Some(token), "");
    assert_eq!(code, 200);
    assert!(body.contains("start_run"));
    sending.write_all(b"{}").unwrap();
    sending
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut answer = String::new();
    sending.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    // The first idle connection was cut off without an answer.
    slow[0]
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut byte = [0];
    match slow[0].read(&mut byte) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        result => panic!("the oldest idle connection must be cut off: {result:?}"),
    }
    drop(slow);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(http(&addr, "GET", "/tools", Some(token), "").0, 200);
    // Authenticated requests still sending their body hold one of the 32
    // authenticated slots.
    let mut bodies: Vec<TcpStream> = (0..32)
        .map(|_| {
            let mut stream = TcpStream::connect(&addr).unwrap();
            stream
                .write_all(
                    format!(
                        "POST /tools/list_workflows HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 2\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .unwrap();
            stream
        })
        .collect();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(http(&addr, "GET", "/tools", Some(token), "").0, 503);
    // Refused at once, without waiting for the body it declared.
    let mut refused = TcpStream::connect(&addr).unwrap();
    refused
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    refused
        .write_all(
            format!(
                "POST /tools/list_workflows HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 100\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    let mut answer = String::new();
    refused.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 503"), "{answer}");
    for stream in &mut bodies {
        stream.write_all(b"{}").unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    }
    drop(bodies);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(http(&addr, "GET", "/tools", Some(token), "").0, 200);
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

/// `wait_run` with a progress token reports the run's state changes before
/// it answers.
#[test]
fn mcp_wait_run_emits_progress_for_state_changes() {
    let dir = project();
    let mut mcp = Mcp::spawn(dir.path(), &[]);
    let started = mcp.call(
        "start_run",
        json!({"workflow": "Paced", "task": "progress"}),
    );
    let run_id = started["run_id"].as_str().unwrap().to_owned();
    // The run's three stages each take a second, so its state changes
    // several times while this call waits.
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": 98,
        "method": "tools/call",
        "params": {
            "name": "wait_run",
            "arguments": {"run_id": run_id, "timeout_seconds": 60},
            "_meta": {"progressToken": "tok-1"}
        }
    }));
    let mut steps = vec![];
    let response = loop {
        let msg = mcp.read();
        if msg.get("method").and_then(Value::as_str) == Some("notifications/progress") {
            assert_eq!(msg["params"]["progressToken"], "tok-1", "{msg}");
            assert!(msg["params"]["message"].is_string(), "{msg}");
            steps.push(msg["params"]["progress"].as_u64().unwrap());
            continue;
        }
        assert_eq!(msg["id"], 98, "{msg}");
        break msg;
    };
    assert!(
        !steps.is_empty(),
        "no progress notification before the answer"
    );
    // Progress only increases, as MCP requires.
    assert!(steps.windows(2).all(|w| w[0] < w[1]), "{steps:?}");
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["status"], "complete", "{response}");
    assert_eq!(result["timed_out"], false, "{response}");
}

/// A cancelled `wait_run` never answers. The server reads requests in order
/// and registers a call before reading the next line, so the cancellation
/// below always finds call 99. Were it ignored, 99 would answer `timed_out`
/// after 1 s, before call 100, which waits 3 s on the same run.
#[test]
fn mcp_cancelled_wait_run_never_answers() {
    let dir = project();
    let mut mcp = Mcp::spawn(dir.path(), &[]);
    let started = mcp.call(
        "start_run",
        json!({"workflow": "Slow", "task": "cancel wait"}),
    );
    let run_id = started["run_id"].as_str().unwrap().to_owned();
    let wait = |id: u64, seconds: u64| {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "wait_run",
                "arguments": {"run_id": run_id, "timeout_seconds": seconds},
                "_meta": {"progressToken": format!("tok-{id}")}
            }
        })
    };
    mcp.send(wait(99, 1));
    mcp.send(json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {"requestId": 99, "reason": "client cancelled"}
    }));
    mcp.send(wait(100, 3));
    let response = loop {
        let msg = mcp.read();
        assert_ne!(
            msg.get("id"),
            Some(&json!(99)),
            "cancelled request answered: {msg}"
        );
        if msg.get("method").and_then(Value::as_str) == Some("notifications/progress") {
            // Nothing is reported for a cancelled request either.
            assert_ne!(msg["params"]["progressToken"], "tok-99", "{msg}");
            continue;
        }
        assert_eq!(msg["id"], 100, "{msg}");
        break msg;
    };
    // Call 100 outlasted 99's timeout, and the run is still going.
    assert_eq!(
        response["result"]["structuredContent"]["timed_out"], true,
        "{response}"
    );
    // The cancelled wait left the run alone, and cheap calls still answer.
    let cancelled = mcp.call("cancel_run", json!({"run_id": run_id}));
    assert_eq!(cancelled["run_id"], run_id, "{cancelled}");
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn trust_cmd(project: &Path, config_home: &Path, action: &str) {
    let out = Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("-C")
        .arg(project)
        .args(["trust", action])
        .env("MODEL_ROUTER_CONFIG_DIR", config_home)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "trust {action} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A Git repository project with file-writing, cost-reporting, and review-summary workflows.
fn git_project() -> (tempfile::TempDir, tempfile::TempDir) {
    let config_home = tempfile::Builder::new()
        .prefix("serve config ")
        .tempdir()
        .unwrap();
    let dir = tempfile::Builder::new()
        .prefix("serve git 雪 ")
        .tempdir()
        .unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.name", "Test"]);
    git(dir.path(), &["config", "user.email", "test@example.com"]);
    std::fs::write(dir.path().join("app.txt"), "initial\n").unwrap();
    std::fs::write(dir.path().join(".gitignore"), ".model-router/\n").unwrap();
    let summary = stage("summary", Role::Summary, "priced", Mode::Unattended);
    let build = Workflow::new(
        "Build",
        vec![
            stage("research", Role::Research, "priced", Mode::Unattended),
            stage(
                "implement",
                Role::Implementation,
                "writer",
                Mode::Unattended,
            ),
            summary,
        ],
    );
    let settings = Settings {
        workflows: vec![build],
        adapters: BTreeMap::from([
            ("priced".into(), mock_adapter(&["--cost", "0.25"])),
            (
                "writer".into(),
                mock_adapter(&["--write-file", "app.txt", "--cost", "0.50"]),
            ),
        ]),
        ..Settings::default()
    };
    settings
        .save(&dir.path().join(".model-router.toml"))
        .unwrap();
    git(
        dir.path(),
        &["add", "app.txt", ".gitignore", ".model-router.toml"],
    );
    git(
        dir.path(),
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "initial",
        ],
    );
    trust_cmd(dir.path(), config_home.path(), "--yes");
    (dir, config_home)
}

fn spawn_mcp_with_config(project: &Path, config_home: &Path, extra: &[&str]) -> Mcp {
    let mut child = Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("-C")
        .arg(project)
        .env("MODEL_ROUTER_CONFIG_DIR", config_home)
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
    Mcp {
        child,
        stdin: Some(stdin),
        lines,
        next: 0,
    }
}

struct HttpServer {
    child: Child,
    addr: String,
    token: String,
}

impl HttpServer {
    fn spawn(project: &Path, config_home: &Path, token: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_model-router"))
            .arg("-C")
            .arg(project)
            .env("MODEL_ROUTER_CONFIG_DIR", config_home)
            .args(["serve", "--http", "127.0.0.1:0"])
            .env("MODEL_ROUTER_SERVE_TOKEN", token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let mut lines = BufReader::new(stderr).lines();
        let addr = loop {
            let line = lines.next().expect("server announces its address").unwrap();
            if let Some(rest) = line.split_once("http://").map(|(_, r)| r) {
                break rest.split_whitespace().next().unwrap().to_owned();
            }
        };
        std::thread::spawn(move || for _ in lines {});
        Self {
            child,
            addr,
            token: token.to_owned(),
        }
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_fork_and_apply_worktree_run() {
    let (dir, config_home) = git_project();
    let outside = tempfile::tempdir().unwrap();
    let mut mcp = spawn_mcp_with_config(dir.path(), config_home.path(), &[]);
    initialize(&mut mcp);

    // Run in a Git worktree so changes can be checked and applied back.
    let started = mcp.call(
        "start_run",
        json!({"task": "Update app.txt", "worktree": true}),
    );
    let id = started["run_id"].as_str().unwrap().to_owned();
    assert_eq!(started["worktree"], true);
    let done = mcp.call("wait_run", json!({"run_id": id, "timeout_seconds": 60}));
    assert_eq!(done["status"], "complete", "{done}");

    // Default apply_run is check_only: true and leaves the checkout untouched.
    let checked = mcp.call("apply_run", json!({"run_id": id}));
    assert_eq!(checked["check_only"], true, "{checked}");
    assert_eq!(checked["applied"], false, "{checked}");
    assert_eq!(checked["outcome"], "clean", "{checked}");
    assert_eq!(checked["conflicts"], json!([]));
    assert!(
        checked["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["path"] == "app.txt"),
        "{checked}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("app.txt")).unwrap(),
        "initial\n"
    );

    // Explicit check_only: false applies uncommitted changes to the checkout.
    let applied = mcp.call("apply_run", json!({"run_id": id, "check_only": false}));
    assert_eq!(applied["check_only"], false, "{applied}");
    assert_eq!(applied["applied"], true, "{applied}");
    assert_eq!(applied["outcome"], "clean", "{applied}");
    let app_contents = std::fs::read_to_string(dir.path().join("app.txt")).unwrap();
    assert!(
        app_contents.contains("written by implement"),
        "{app_contents}"
    );

    // Conflicting checkout edit: check_only reports conflict; real apply fails and keeps checkout intact.
    let conflict_text = "conflicting line in checkout\n";
    std::fs::write(dir.path().join("app.txt"), conflict_text).unwrap();
    let conflict_check = mcp.call("apply_run", json!({"run_id": id, "check_only": true}));
    assert_eq!(conflict_check["outcome"], "conflict", "{conflict_check}");
    assert_eq!(conflict_check["applied"], false);
    assert_eq!(conflict_check["conflicts"], json!(["app.txt"]));
    let conflict_err = mcp.error("apply_run", json!({"run_id": id, "check_only": false}));
    assert!(conflict_err.contains("conflicts"), "{conflict_err}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("app.txt")).unwrap(),
        conflict_text
    );

    // Restore checkout clean before testing protected_paths and fork_run.
    git(dir.path(), &["checkout", "--", "app.txt"]);

    // fork_run with no_run: true creates the fork without starting it.
    let forked_paused = mcp.call(
        "fork_run",
        json!({
            "run_id": id,
            "stage": "implement",
            "model": "fork-paused",
            "no_run": true
        }),
    );
    assert_eq!(forked_paused["started"], false, "{forked_paused}");
    assert_eq!(forked_paused["active"], false);
    assert_eq!(forked_paused["status"], "pending");
    assert_eq!(forked_paused["fork_of"]["run"], id.as_str());
    assert_eq!(forked_paused["fork_of"]["stage"], "implement");
    assert_eq!(forked_paused["stages"][0]["status"], "complete");
    assert_eq!(forked_paused["stages"][1]["model"], "fork-paused");

    // fork_run with default no_run: false drives the fork in the background.
    let forked_live = mcp.call(
        "fork_run",
        json!({
            "run_id": id,
            "stage": "2",
            "model": "fork-live"
        }),
    );
    let fork_id = forked_live["run_id"].as_str().unwrap().to_owned();
    assert_eq!(forked_live["started"], true, "{forked_live}");
    let fork_done = mcp.call(
        "wait_run",
        json!({"run_id": fork_id, "timeout_seconds": 60}),
    );
    assert_eq!(fork_done["status"], "complete", "{fork_done}");
    assert_eq!(fork_done["stages"][0]["attempts"], 0);
    assert_eq!(fork_done["stages"][1]["model"], "fork-live");

    // Protected paths on the run block apply_run.
    let run_json_path = dir
        .path()
        .join(".model-router/runs")
        .join(&id)
        .join("run.json");
    let original_run = std::fs::read(&run_json_path).unwrap();
    let mut edited: Value = serde_json::from_slice(&original_run).unwrap();
    edited["workflow"]["protected_paths"] = json!(["app.txt"]);
    std::fs::write(&run_json_path, serde_json::to_vec(&edited).unwrap()).unwrap();

    let protected_err = mcp.error("apply_run", json!({"run_id": id}));
    assert!(
        protected_err.contains("Refusing to apply protected path"),
        "{protected_err}"
    );

    // Interactive stages on fork_run require force_unattended unless no_run is true.
    let mut interactive_run: Value = serde_json::from_slice(&original_run).unwrap();
    interactive_run["workflow"]["stages"][1]["mode"] = json!("interactive");
    std::fs::write(
        &run_json_path,
        serde_json::to_vec(&interactive_run).unwrap(),
    )
    .unwrap();
    let interactive_err = mcp.error("fork_run", json!({"run_id": id, "stage": "implement"}));
    assert!(
        interactive_err.contains("force_unattended"),
        "{interactive_err}"
    );
    let forced_fork = mcp.call(
        "fork_run",
        json!({
            "run_id": id,
            "stage": "implement",
            "force_unattended": true
        }),
    );
    let forced_id = forced_fork["run_id"].as_str().unwrap().to_owned();
    let forced_done = mcp.call(
        "wait_run",
        json!({"run_id": forced_id, "timeout_seconds": 60}),
    );
    assert_eq!(forced_done["status"], "complete", "{forced_done}");
    std::fs::write(&run_json_path, original_run).unwrap();

    // --allow-root and project trust are enforced on both tools.
    let outside_err = mcp.error(
        "apply_run",
        json!({"run_id": id, "project": outside.path().to_str().unwrap()}),
    );
    assert!(
        outside_err.contains("outside the server's allowed roots"),
        "{outside_err}"
    );
    let outside_fork = mcp.error(
        "fork_run",
        json!({"run_id": id, "stage": "implement", "project": outside.path().to_str().unwrap()}),
    );
    assert!(
        outside_fork.contains("outside the server's allowed roots"),
        "{outside_fork}"
    );
    trust_cmd(dir.path(), config_home.path(), "--revoke");
    assert!(
        mcp.error("apply_run", json!({"run_id": id}))
            .contains("Untrusted project config")
    );
    assert!(
        mcp.error("fork_run", json!({"run_id": id, "stage": "implement"}))
            .contains("Untrusted project config")
    );
}

#[test]
fn mcp_and_http_handoffs_cost_stats_and_schedules() {
    use model_router::delegation::{
        policy::{Ceiling, Enforcement, Native, Network, Write as PolicyWrite},
        schedules::{Occurrence, Schedule, Store, Trigger},
    };
    let (dir, config_home) = git_project();
    let mut mcp = spawn_mcp_with_config(dir.path(), config_home.path(), &[]);
    initialize(&mut mcp);

    let started = mcp.call("start_run", json!({"task": "Measure everything"}));
    let id = started["run_id"].as_str().unwrap().to_owned();
    let done = mcp.call("wait_run", json!({"run_id": id, "timeout_seconds": 60}));
    assert_eq!(done["status"], "complete", "{done}");

    // get_handoffs: default omits prompt; include_prompt: true returns rendered prompt.
    let handoffs = mcp.call("get_handoffs", json!({"run_id": id, "stage": "implement"}));
    assert_eq!(handoffs["run_id"], id.as_str());
    assert_eq!(handoffs["stage"], "implement");
    assert!(handoffs.get("prompt").is_none(), "{handoffs}");
    assert_eq!(handoffs["handoffs"][0]["stage"], "research");
    assert_eq!(handoffs["handoffs"][0]["sent"], true);
    assert!(handoffs["budget_total"].as_u64().unwrap() > 0);
    assert!(handoffs["budget_each"].as_u64().unwrap() > 0);

    let with_prompt = mcp.call(
        "get_handoffs",
        json!({"run_id": id, "stage": "2", "include_prompt": true}),
    );
    let prompt_text = with_prompt["prompt"].as_str().unwrap();
    assert!(prompt_text.contains("ORIGINAL USER TASK"), "{prompt_text}");
    assert!(
        prompt_text.contains("PRIOR STAGE: research"),
        "{prompt_text}"
    );

    let missing_stage = mcp.error("get_handoffs", json!({"run_id": id}));
    assert!(missing_stage.contains("--stage"), "{missing_stage}");

    // get_cost: project summary with optional since filter.
    let cost = mcp.call("get_cost", json!({}));
    assert!(!cost["rows"].as_array().unwrap().is_empty(), "{cost}");
    assert!(cost["prices_as_of"].is_string());
    let cost_since = mcp.call("get_cost", json!({"since": "7d"}));
    assert!(cost_since["since"].is_string(), "{cost_since}");
    let bad_since = mcp.error("get_cost", json!({"since": "not-a-duration"}));
    assert!(bad_since.contains("7d"), "{bad_since}");

    // get_stats: routing stats with role and min_pass shortlist.
    let stats = mcp.call("get_stats", json!({}));
    assert_eq!(stats["runs"], 1, "{stats}");
    assert!(stats["metrics"].as_array().unwrap().len() >= 2, "{stats}");
    let filtered = mcp.call(
        "get_stats",
        json!({"role": "implementation", "min_pass": 0.5}),
    );
    let metrics = filtered["metrics"].as_array().unwrap();
    assert_eq!(metrics.len(), 1, "{filtered}");
    assert_eq!(metrics[0]["role"], "implementation");
    assert!(filtered["shortlist"].is_object(), "{filtered}");
    let bad_rate = mcp.error("get_stats", json!({"min_pass": 1.5}));
    assert!(bad_rate.contains("0 to 1"), "{bad_rate}");

    // list_schedules: empty initially, then reports approval and occurrences.
    let empty_schedules = mcp.call("list_schedules", json!({}));
    assert_eq!(empty_schedules["schedules"], json!([]));

    let now = chrono::Utc::now();
    let store = Store {
        schedules: vec![Schedule {
            id: "s1".into(),
            workflow: Some("Build".into()),
            task: "Nightly check".into(),
            trigger: Trigger::Every {
                seconds: 3600,
                text: "1h".into(),
            },
            enabled: true,
            created_by: "implement".into(),
            ceiling: Ceiling {
                write: PolicyWrite::Workspace,
                native: Native::Escalated,
                network: Network::Open,
                enforcement: Enforcement::None,
                writable: vec![],
            },
            client_request_id: String::new(),
            requested_by: String::new(),
            request_fingerprint: String::new(),
            revision: 1,
            created_at: now,
            updated_at: now,
            approved_at: None,
            paused_reason: None,
            occurrences: vec![Occurrence {
                due: now,
                fired_at: now,
                run: Some(id.clone()),
                error: None,
            }],
        }],
    };
    let schedules_path = dir.path().join(".model-router/schedules.json");
    std::fs::write(&schedules_path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();

    let unapproved = mcp.call("list_schedules", json!({}));
    assert_eq!(unapproved["schedules"][0]["schedule"]["id"], "s1");
    assert_eq!(unapproved["schedules"][0]["approved"], false);
    assert_eq!(
        unapproved["schedules"][0]["schedule"]["occurrences"][0]["run"],
        id.as_str()
    );

    let approve = Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("-C")
        .arg(dir.path())
        .env("MODEL_ROUTER_CONFIG_DIR", config_home.path())
        .args(["schedules", "approve", "s1"])
        .output()
        .unwrap();
    assert!(approve.status.success(), "{approve:?}");

    let approved = mcp.call("list_schedules", json!({}));
    assert_eq!(approved["schedules"][0]["approved"], true, "{approved}");
    assert!(approved["schedules"][0]["next_due"].is_string());

    // Expose the same tools over HTTP `/tools/<name>`.
    let server = HttpServer::spawn(dir.path(), config_home.path(), "parity-token");
    for (tool, body) in [
        (
            "get_handoffs",
            json!({"run_id": id, "stage": "implement"}).to_string(),
        ),
        ("get_cost", "{}".into()),
        ("get_stats", r#"{"role":"implementation"}"#.into()),
        ("list_schedules", "{}".into()),
    ] {
        let (code, resp) = http(
            &server.addr,
            "POST",
            &format!("/tools/{tool}"),
            Some(&server.token),
            &body,
        );
        assert_eq!(code, 200, "{tool}: {resp}");
        let parsed: Value = serde_json::from_str(&resp).unwrap();
        assert!(parsed["result"].is_object(), "{tool}: {parsed}");
    }
}

#[test]
fn mcp_resources_list_read_and_safety() {
    let (dir, config_home) = git_project();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "TOP-SECRET-DATA\n").unwrap();

    let mut mcp = spawn_mcp_with_config(dir.path(), config_home.path(), &[]);
    initialize(&mut mcp);

    let started = mcp.call("start_run", json!({"task": "Produce review and diff"}));
    let id = started["run_id"].as_str().unwrap().to_owned();
    let done = mcp.call("wait_run", json!({"run_id": id, "timeout_seconds": 60}));
    assert_eq!(done["status"], "complete", "{done}");

    let listed = mcp.request("resources/list", json!({}))["result"]["resources"].clone();
    let list = listed.as_array().unwrap();
    let review_uri = format!("model-router://runs/{id}/review");
    let diff_uri = format!("model-router://runs/{id}/diff");
    let review_entry = list
        .iter()
        .find(|r| r["uri"] == review_uri.as_str())
        .expect("review resource listed");
    assert_eq!(review_entry["mimeType"], "text/markdown");
    let diff_entry = list
        .iter()
        .find(|r| r["uri"] == diff_uri.as_str())
        .expect("diff resource listed");
    assert_eq!(diff_entry["mimeType"], "text/x-diff");

    let review =
        mcp.request("resources/read", json!({"uri": review_uri}))["result"]["contents"][0].clone();
    assert_eq!(review["mimeType"], "text/markdown");
    assert!(
        review["text"]
            .as_str()
            .unwrap()
            .contains("summary evidence from model"),
        "{review}"
    );

    let diff =
        mcp.request("resources/read", json!({"uri": diff_uri}))["result"]["contents"][0].clone();
    assert_eq!(diff["mimeType"], "text/x-diff");
    assert!(diff["text"].as_str().unwrap().contains("app.txt"), "{diff}");

    // Path traversal is rejected.
    let traversal = mcp.request(
        "resources/read",
        json!({"uri": "model-router://runs/../escape/review"}),
    );
    assert_eq!(traversal["error"]["code"], -32602, "{traversal}");

    // Outside project via ?project= is blocked by --allow-root.
    let outside_uri = format!(
        "model-router://runs/{id}/review?project={}",
        outside.path().to_str().unwrap().replace(' ', "%20")
    );
    let outside_resp = mcp.request("resources/read", json!({"uri": outside_uri}));
    assert_eq!(outside_resp["error"]["code"], -32602, "{outside_resp}");
    assert!(
        outside_resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("outside the server's allowed roots"),
        "{outside_resp}"
    );

    // Symlinks swapped in for review.md are never listed or read.
    #[cfg(unix)]
    {
        let review_path = dir
            .path()
            .join(".model-router/runs")
            .join(&id)
            .join("review.md");
        std::fs::remove_file(&review_path).unwrap();
        std::os::unix::fs::symlink(&secret, &review_path).unwrap();

        let listed_after = mcp.request("resources/list", json!({}))["result"]["resources"].clone();
        assert!(
            !listed_after
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["uri"] == review_uri.as_str()),
            "symlinked review must not be listed: {listed_after}"
        );
        let symlink_read = mcp.request("resources/read", json!({"uri": review_uri}));
        assert_eq!(symlink_read["error"]["code"], -32002, "{symlink_read}");
        assert!(
            !symlink_read.to_string().contains("TOP-SECRET-DATA"),
            "{symlink_read}"
        );
    }

    // Revoking project trust blocks both resources/list and resources/read.
    trust_cmd(dir.path(), config_home.path(), "--revoke");
    let untrusted_list = mcp.request("resources/list", json!({}));
    assert!(
        untrusted_list["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Untrusted project config"),
        "{untrusted_list}"
    );
    let untrusted_read = mcp.request("resources/read", json!({"uri": diff_uri}));
    assert!(
        untrusted_read["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Untrusted project config"),
        "{untrusted_read}"
    );
}

#[test]
fn http_health_endpoint_is_unauthenticated_and_resilient() {
    let dir = project();
    let config_home = dir.path().join("config-home");
    let server = HttpServer::spawn(dir.path(), &config_home, "health-token");

    // Unauthenticated GET /health returns 200 with status and version only.
    let (code, body) = http(&server.addr, "GET", "/health", None, "");
    assert_eq!(code, 200, "{body}");
    let parsed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        parsed,
        json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")})
    );

    // Authenticated GET /health also returns 200.
    let (code, body) = http(&server.addr, "GET", "/health", Some(&server.token), "");
    assert_eq!(code, 200, "{body}");
    let parsed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["status"], "ok");

    // Non-GET /health requires auth (401 without token, 405 with token).
    let (code, _) = http(&server.addr, "POST", "/health", None, "");
    assert_eq!(code, 401);
    let (code, _) = http(&server.addr, "POST", "/health", Some(&server.token), "");
    assert_eq!(code, 405);

    // GET /health still answers 200 even when all 32 authenticated slots are full.
    let mut busy: Vec<TcpStream> = (0..32)
        .map(|_| {
            let mut stream = TcpStream::connect(&server.addr).unwrap();
            stream
                .write_all(
                    format!(
                        "POST /tools/list_workflows HTTP/1.1\r\nAuthorization: Bearer {}\r\nContent-Length: 2\r\n\r\n",
                        server.token
                    )
                    .as_bytes(),
                )
                .unwrap();
            stream
        })
        .collect();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        http(&server.addr, "GET", "/tools", Some(&server.token), "").0,
        503
    );
    let (code, body) = http(&server.addr, "GET", "/health", None, "");
    assert_eq!(code, 200, "{body}");
    let (code, body) = http(&server.addr, "GET", "/health", Some(&server.token), "");
    assert_eq!(code, 200, "{body}");

    for stream in &mut busy {
        let _ = stream.write_all(b"{}");
    }
    drop(busy);
}
