//! Delegation end to end, offline: a scripted mock parent speaks MCP through
//! the real `mr __mcp-bridge` to the router, which runs mock tasks in their
//! own OS sandbox. Needs a working sandbox backend; skipped (with the reason
//! printed) elsewhere. No network, no credentials, no inference.
mod common;

use model_router::{
    config::{CustomAdapter, CustomSession, Role, Settings, Stage, Workflow},
    delegation::{Delegate, Delegation, Isolation, records::TaskStatus},
    engine::{self, Action, Run, Status},
    report, sandbox,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{OnceLock, atomic::AtomicBool},
};

fn setup() -> bool {
    static READY: OnceLock<bool> = OnceLock::new();
    *READY.get_or_init(|| {
        if let Err(reason) = sandbox::detect() {
            eprintln!("skipping delegation tests: {reason}");
            return false;
        }
        let config = tempfile::tempdir().unwrap().keep();
        // SAFETY: once, before any test in this binary spawns a process; the
        // others wait on this OnceLock.
        unsafe {
            std::env::set_var(
                "MODEL_ROUTER_BRIDGE_PROGRAM",
                env!("CARGO_BIN_EXE_model-router"),
            );
            std::env::set_var("MODEL_ROUTER_CONFIG_DIR", config);
        }
        true
    })
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn repo() -> tempfile::TempDir {
    let dir = common::tempdir();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "Test"]);
    git(dir.path(), &["config", "user.email", "test@example.com"]);
    fs::write(dir.path().join("README.md"), "hello\n").unwrap();
    fs::write(
        dir.path().join(".gitignore"),
        "/.model-router\n/script.json*\n",
    )
    .unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "init"]);
    dir
}

fn mock() -> String {
    format!("{}/examples/mock_harness.py", env!("CARGO_MANIFEST_DIR"))
}

/// The scripted parent. Without `follow_up` it cannot be woken.
fn parent(script: &Path, follow_up: bool) -> CustomAdapter {
    let base = vec![
        mock(),
        "--role".into(),
        "implement".into(),
        "--prompt-file".into(),
        "{prompt_file}".into(),
        "--handoff-file".into(),
        "{handoff_file}".into(),
        "--script".into(),
        script.display().to_string(),
    ];
    let mut follow = base.clone();
    follow.extend([
        "--mcp-config".into(),
        "{mcp_config}".into(),
        "--resume".into(),
        "{session}".into(),
    ]);
    CustomAdapter {
        command: "python3".into(),
        args: base,
        session: Some(CustomSession {
            assign: Some("--session-id".into()),
            reported: false,
        }),
        follow_up_args: follow_up.then_some(follow),
        mcp_config_args: Some(vec!["--mcp-config".into(), "{mcp_config}".into()]),
        ..CustomAdapter::default()
    }
}

/// A mock task: research evidence, a question first (model `ask`), or a
/// file written into its worktree (`writes`).
fn task_adapter(writes: Option<&str>) -> CustomAdapter {
    let mut args = vec![
        mock(),
        "--role".into(),
        "research".into(),
        "--model".into(),
        "{model}".into(),
        "--prompt-file".into(),
        "{prompt_file}".into(),
        "--handoff-file".into(),
        "{handoff_file}".into(),
        "--state-dir".into(),
        "{run_dir}".into(),
    ];
    if let Some(file) = writes {
        args.extend(["--write-file".into(), file.into()]);
    }
    let mut follow = args.clone();
    follow.extend(["--resume".into(), "{session}".into()]);
    CustomAdapter {
        command: "python3".into(),
        args,
        session: Some(CustomSession {
            assign: Some("--session-id".into()),
            reported: false,
        }),
        follow_up_args: Some(follow),
        ..CustomAdapter::default()
    }
}

struct Case {
    dir: tempfile::TempDir,
    script: PathBuf,
    run: Run,
    settings: Settings,
}

fn case(turns: Value, follow_up: bool, delegate: Delegate, tweak: impl FnOnce(&mut Stage)) -> Case {
    let dir = repo();
    let script = dir.path().join("script.json");
    fs::write(&script, serde_json::to_string(&turns).unwrap()).unwrap();
    let mut stage = Stage {
        id: "implement".into(),
        name: "Implement".into(),
        role: Role::Implementation,
        harness: "parent".into(),
        delegation: Some(Delegation {
            delegates: vec!["helper".into()],
            ..Delegation::default()
        }),
        ..Stage::default()
    };
    tweak(&mut stage);
    let workflow = Workflow {
        max_repair_passes: 0,
        ..Workflow::new("Delegation", vec![stage])
    };
    let settings = Settings {
        workflows: vec![workflow.clone()],
        adapters: BTreeMap::from([
            ("parent".into(), parent(&script, follow_up)),
            ("task".into(), task_adapter(None)),
            ("writer".into(), task_adapter(Some("added.txt"))),
        ]),
        delegates: BTreeMap::from([("helper".into(), delegate)]),
        ..Settings::default()
    };
    settings.validate().unwrap();
    let run = Run::create(
        dir.path(),
        &settings,
        &workflow,
        "Delegate things".into(),
        false,
    )
    .unwrap();
    Case {
        dir,
        script,
        run,
        settings,
    }
}

/// Saves `case`'s settings, plus an unattended `Plain` workflow, as the
/// project's own trusted config, which `run_launch` reads.
fn save_project_config(case: &Case, settings: &Settings) {
    let mut settings = settings.clone();
    settings.workflows.push(Workflow::new(
        "Plain",
        vec![Stage {
            id: "look".into(),
            name: "Look".into(),
            role: Role::Research,
            harness: "task".into(),
            model: "m".into(),
            ..Stage::default()
        }],
    ));
    settings
        .save(&case.dir.path().join(".model-router.toml"))
        .unwrap();
    let loaded = model_router::config::load_config(case.dir.path(), None).unwrap();
    if let Some(review) = loaded.review() {
        model_router::trust::approve(review).unwrap();
    }
}

fn helper(model: &str) -> Delegate {
    Delegate {
        harness: "task".into(),
        model: model.into(),
        ..Delegate::default()
    }
}

fn drive(run: &mut Run) -> Action {
    engine::drive(run, &AtomicBool::new(false), |_| {}).unwrap()
}

/// The scripted parent's calls and prompts, per turn.
fn calls(script: &Path) -> Vec<Value> {
    fs::read_to_string(format!("{}.calls.jsonl", script.display()))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn call<'a>(calls: &'a [Value], turn: u64, name: &str) -> Vec<&'a Value> {
    calls
        .iter()
        .filter(|c| c["turn"] == turn && c["call"] == name)
        .collect()
}

#[test]
fn a_shared_task_wakes_the_parent_once_and_its_result_is_read() {
    if !setup() {
        return;
    }
    let turns = json!({
        "turns": [
            [
                {"call": "orchestrator_capabilities"},
                {"call": "delegate_task", "args": {"task": "Summarize README.md", "role": "research", "client_request_id": "r1"}},
                {"call": "delegate_task", "args": {"task": "Summarize README.md", "role": "research", "client_request_id": "r1"}},
                {"call": "delegate_task", "args": {"task": "Something else", "client_request_id": "r1"}}
            ],
            [
                {"call": "task_status", "args": {"task_id": "t1"}}
            ]
        ],
        "handoffs": {"1": {"summary": "waiting on tasks", "verdict": "blocked"}, "2": {"summary": "final", "verdict": "complete"}}
    });
    let mut case = case(turns, true, helper("m"), |_| {});
    assert_eq!(
        drive(&mut case.run),
        Action::Complete,
        "{:?}",
        case.run.error
    );
    let calls = calls(&case.script);
    let capabilities = &call(&calls, 1, "orchestrator_capabilities")[0]["result"];
    assert_eq!(capabilities["delegates"][0]["delegate"], "helper");
    assert_eq!(capabilities["caller"]["supports_wake"], true);
    let delegated = call(&calls, 1, "delegate_task");
    assert_eq!(delegated[0]["result"]["task_id"], "t1");
    assert_eq!(delegated[0]["result"]["isolation"], "shared");
    assert!(
        delegated[0]["result"]["next"]
            .as_str()
            .unwrap()
            .contains("resumes")
    );
    // A retry with the same key returns the same task; another payload is a conflict.
    assert_eq!(delegated[1]["result"]["task_id"], "t1");
    assert_eq!(delegated[2]["error"], true);
    assert_eq!(delegated[2]["result"]["error"]["code"], "request_conflict");
    // One wake, announcing the finished task.
    let wake_prompt = calls
        .iter()
        .find(|c| c["turn"] == 2 && c.get("prompt").is_some())
        .unwrap()["prompt"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        wake_prompt.contains("t1 (helper): completed"),
        "{wake_prompt}"
    );
    let status = &call(&calls, 2, "task_status")[0]["result"];
    assert_eq!(status["status"], "completed");
    assert!(
        status["summary"]
            .as_str()
            .unwrap()
            .contains("research evidence"),
        "{status}"
    );
    let state = &case.run.stages[0];
    assert_eq!(state.handoff.as_ref().unwrap().summary, "final");
    let attempt = &state.attempts[0];
    assert_eq!(attempt.turns.len(), 1);
    assert_eq!(attempt.tasks.len(), 1);
    let task = &attempt.tasks[0];
    assert_eq!(task.status, TaskStatus::Completed);
    assert!(task.delivered);
    assert_eq!(task.isolation, Isolation::Shared);
    // The first turn's provisional handoff was set aside, never accepted.
    assert!(
        attempt
            .directory
            .join("turns/turn-001/handoff-file.json")
            .is_file()
    );
    // The task ran read-only in its own sandbox.
    let command: Value =
        serde_json::from_slice(&fs::read(task.turns[0].directory.join("command.json")).unwrap())
            .unwrap();
    assert_eq!(command["sandbox"]["mode"], "read-only");
    // Reloaded from disk, the run validates, and the task counts in reports.
    let saved = engine::load(case.dir.path(), &case.run.id).unwrap();
    assert_eq!(saved.stages[0].attempts[0].tasks.len(), 1);
    let built = report::build(&saved);
    assert_eq!(
        built.stages[0].usage.attempts, 2,
        "the stage attempt and the task turn"
    );
}

#[test]
fn a_question_is_answered_with_task_send_and_the_answer_comes_back() {
    if !setup() {
        return;
    }
    let turns = json!({
        "turns": [
            [{"call": "delegate_task", "args": {"task": "Write the function"}}],
            [
                {"call": "task_status", "args": {"task_id": "t1"}},
                {"call": "task_send", "args": {"task_id": "t1", "message": "Name it add"}}
            ],
            [{"call": "task_status", "args": {"task_id": "t1"}}]
        ],
        "handoffs": {"3": {"summary": "final", "verdict": "complete"}}
    });
    let mut case = case(turns, true, helper("ask"), |_| {});
    assert_eq!(
        drive(&mut case.run),
        Action::Complete,
        "{:?}",
        case.run.error
    );
    let calls = calls(&case.script);
    let asked = &call(&calls, 2, "task_status")[0]["result"];
    assert_eq!(asked["status"], "waiting");
    assert_eq!(asked["questions"][0], "Which name should the function get?");
    let answered = &call(&calls, 3, "task_status")[0]["result"];
    assert_eq!(answered["status"], "completed");
    assert_eq!(answered["latest_turn_summary"], "answered: Name it add");
    let attempt = &case.run.stages[0].attempts[0];
    assert_eq!(
        attempt.turns.len(),
        2,
        "one wake for the question, one for the answer"
    );
    assert_eq!(attempt.tasks[0].turns.len(), 2);
}

#[test]
fn a_wake_that_ends_without_reading_its_results_stops_the_stage() {
    if !setup() {
        return;
    }
    let turns = json!({
        "turns": [[{"call": "delegate_task", "args": {"task": "Look around"}}], []]
    });
    let mut case = case(turns, true, helper("m"), |_| {});
    assert_eq!(drive(&mut case.run), Action::Stopped);
    assert_eq!(case.run.status, Status::Blocked);
    let error = case.run.error.clone().unwrap();
    assert!(error.contains("results not read"), "{error}");
}

#[test]
fn a_parent_that_cannot_be_woken_waits_in_its_tool_call() {
    if !setup() {
        return;
    }
    let turns = json!({
        "turns": [[{"call": "delegate_task", "args": {"task": "Look around"}}]],
        "handoffs": {"1": {"summary": "final", "verdict": "complete"}}
    });
    let mut case = case(turns, false, helper("m"), |_| {});
    assert_eq!(
        drive(&mut case.run),
        Action::Complete,
        "{:?}",
        case.run.error
    );
    let calls = calls(&case.script);
    let result = &call(&calls, 1, "delegate_task")[0]["result"];
    assert_eq!(result["status"], "completed", "{result}");
    assert_eq!(result["wait_timed_out"], false);
    assert!(
        case.run.stages[0].attempts[0].turns.is_empty(),
        "no wake turns"
    );
}

#[test]
fn a_cancelled_task_needs_no_wake() {
    if !setup() {
        return;
    }
    let turns = json!({
        "turns": [[
            {"call": "delegate_task", "args": {"task": "Look around", "client_request_id": "slow"}},
            {"call": "task_cancel", "args": {"task_id": "t1", "reason": "not needed"}}
        ]],
        "handoffs": {"1": {"summary": "final", "verdict": "complete"}}
    });
    let mut case = case(turns, true, helper("slow"), |_| {});
    assert_eq!(
        drive(&mut case.run),
        Action::Complete,
        "{:?}",
        case.run.error
    );
    let task = &case.run.stages[0].attempts[0].tasks[0];
    assert_eq!(task.status, TaskStatus::Cancelled);
    assert!(task.cancelled_by_parent);
    assert!(case.run.stages[0].attempts[0].turns.is_empty());
}

#[test]
fn a_worktree_task_s_patch_reaches_the_checkout_only_through_task_apply() {
    if !setup() {
        return;
    }
    let turns = json!({
        "turns": [
            [{"call": "delegate_task", "args": {"task": "Add a file", "isolation": "worktree"}}],
            [
                {"call": "task_status", "args": {"task_id": "t1"}},
                {"call": "task_apply", "args": {"task_id": "t1", "revision": 1}},
                {"call": "task_apply", "args": {"task_id": "t1", "revision": 1}},
                {"call": "task_apply", "args": {"task_id": "t1", "revision": 9}}
            ]
        ],
        "handoffs": {"2": {"summary": "final", "verdict": "complete"}}
    });
    let writer = Delegate {
        harness: "writer".into(),
        ..Delegate::default()
    };
    let mut case = case(turns, true, writer, |_| {});
    let added = case.dir.path().join("added.txt");
    assert_eq!(
        drive(&mut case.run),
        Action::Complete,
        "{:?}",
        case.run.error
    );
    let calls = calls(&case.script);
    let status = &call(&calls, 2, "task_status")[0]["result"];
    assert_eq!(status["patches"][0]["files"][0], "added.txt", "{status}");
    let applies = call(&calls, 2, "task_apply");
    assert_eq!(
        applies[0]["result"]["apply"]["status"], "applied",
        "{}",
        applies[0]
    );
    assert_eq!(applies[1]["result"]["apply"]["status"], "already_applied");
    assert_eq!(applies[2]["result"]["error"]["code"], "invalid_request");
    assert!(added.is_file(), "the applied patch reached the checkout");
    let task = &case.run.stages[0].attempts[0].tasks[0];
    let worktree = task.worktree.clone().unwrap();
    assert!(worktree.starts_with(case.dir.path().join(".model-router/delegates")));
    let journal = task.patches[0].apply.as_ref().unwrap();
    assert!(journal.applied_at.is_some());
    // Reloads validate the worktree's layout and the patch's journal.
    engine::load(case.dir.path(), &case.run.id).unwrap();
}

#[test]
fn an_unlisted_target_or_an_unknown_task_is_refused() {
    if !setup() {
        return;
    }
    let turns = json!({
        "turns": [[
            {"call": "delegate_task", "args": {"task": "x", "target": {"harness": "codex"}}},
            {"call": "delegate_task", "args": {"task": "x", "target": {"delegate": "nope"}}},
            {"call": "task_status", "args": {"task_id": "t9"}}
        ]],
        "handoffs": {"1": {"summary": "final", "verdict": "complete"}}
    });
    let mut case = case(turns, true, helper("m"), |_| {});
    assert_eq!(
        drive(&mut case.run),
        Action::Complete,
        "{:?}",
        case.run.error
    );
    let calls = calls(&case.script);
    let refused = call(&calls, 1, "delegate_task");
    assert_eq!(refused[0]["result"]["error"]["code"], "target_unknown");
    assert_eq!(refused[1]["result"]["error"]["code"], "target_unknown");
    assert_eq!(
        call(&calls, 1, "task_status")[0]["result"]["error"]["code"],
        "task_not_found"
    );
    assert!(case.run.stages[0].attempts[0].tasks.is_empty());
}

#[test]
fn launched_runs_get_their_own_worktree_and_answer_to_their_launcher() {
    if !setup() {
        return;
    }
    let turns = json!({
        "turns": [[
            {"call": "run_launch", "args": {"runs": [{"workflow": "Plain", "task": "Look at README"}], "client_request_id": "l1"}},
            {"call": "run_launch", "args": {"runs": [{"workflow": "Plain", "task": "Look at README"}], "client_request_id": "l1"}},
            {"call": "run_launch", "args": {"runs": [{"workflow": "Plain", "task": "a"}, {"workflow": "Plain", "task": "b"}]}},
            {"call": "run_list"}
        ]],
        "handoffs": {"1": {"summary": "final", "verdict": "complete"}}
    });
    let mut case = case(turns, true, helper("m"), |stage| {
        let delegation = stage.delegation.as_mut().unwrap();
        delegation.runs = true;
        delegation.max_runs = 2;
    });
    save_project_config(&case, &case.settings.clone());
    assert_eq!(
        drive(&mut case.run),
        Action::Complete,
        "{:?}",
        case.run.error
    );
    let calls = calls(&case.script);
    let launches = call(&calls, 1, "run_launch");
    let first = &launches[0]["result"]["runs"][0];
    assert_eq!(
        first["driven"], false,
        "no host drives runs in tests: {first}"
    );
    assert!(first["resume"].as_str().unwrap().contains("resume"));
    let id = first["run_id"].as_str().unwrap().to_owned();
    // The same key returns the same run.
    assert_eq!(launches[1]["result"]["runs"][0]["run_id"], id);
    // Two more would exceed max_runs = 2 (one launched).
    assert_eq!(launches[2]["result"]["error"]["code"], "run_limit_exceeded");
    let listed = &call(&calls, 1, "run_list")[0]["result"]["runs"];
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["run_id"] == id.as_str())
    );
    let launched = engine::load(case.dir.path(), &id).unwrap();
    assert!(
        launched.worktree,
        "a launched run works in its own worktree"
    );
    assert_eq!(launched.creation_source.label(), "agent");
    let by = launched.launched_by.as_ref().unwrap();
    assert_eq!(
        (by.run.as_str(), by.stage.as_str()),
        (case.run.id.as_str(), "implement")
    );
    assert_eq!(launched.status, Status::Pending);
    let attempt = &case.run.stages[0].attempts[0];
    assert_eq!(attempt.launched_runs.len(), 1);
}
