//! Integration tests for stall timeouts, protected paths, handoff repair,
//! worktree apply, budget warn/billing honesty, pre-ship secret scanning,
//! and `init --detect`.
mod common;

use common::{drive, stage};
use model_router::{
    config::{
        Billing, Budget, BudgetAction, Candidate, CustomAdapter, CustomOutput, CustomSession,
        FanOut, Mode, Role, Settings, Stage, Workflow,
    },
    cost,
    engine::{self, Action, Event, Run, Status},
    fanout, presets, ship,
};
use std::{collections::BTreeMap, fs, path::Path, process::Command, sync::atomic::AtomicBool};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn init_git(dir: &Path) {
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.name", "Test User"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "initial"]);
}

#[test]
fn stall_timeout_stops_silent_stage_but_allows_active_output() {
    let dir = common::tempdir();
    let script = dir.path().join("stall.py");
    fs::write(
        &script,
        r#"import json, sys, time
mode = sys.argv[1]
handoff_path = sys.argv[2]
if mode == "silent":
    time.sleep(10)
else:
    for i in range(4):
        print(f"heartbeat {i}", flush=True)
        time.sleep(0.35)
    with open(handoff_path, "w") as f:
        json.dump({
            "schema_version": 2,
            "summary": "finished with steady output",
            "verdict": "complete",
            "findings": [],
            "tests": [],
            "files_changed": [],
            "unresolved": []
        }, f)
"#,
    )
    .unwrap();

    let adapter = |mode: &str| CustomAdapter {
        command: "python3".into(),
        args: vec![
            script.display().to_string(),
            mode.into(),
            "{handoff_file}".into(),
        ],
        ..CustomAdapter::default()
    };
    let settings = Settings {
        adapters: BTreeMap::from([
            ("silent".into(), adapter("silent")),
            ("active".into(), adapter("active")),
        ]),
        ..Settings::default()
    };

    // 1. Silent process stalls after 1s instead of waiting out timeout_seconds=30.
    let mut silent_stage = Stage {
        id: "work".into(),
        name: "Silent".into(),
        role: Role::Implementation,
        harness: "silent".into(),
        timeout_seconds: 30,
        stall_timeout_seconds: Some(1),
        ..Stage::default()
    };
    let wf = Workflow::new("Stall", vec![silent_stage.clone()]);
    let mut run = Run::create(dir.path(), &settings, &wf, "test stall".into(), false).unwrap();
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!(run.status, Status::Failed);
    assert!(
        run.error.as_deref().unwrap_or("").contains("stalled"),
        "{:?}",
        run.error
    );

    // 2. Active process emitting every 0.35s for 1.4s (> 1s stall limit) succeeds.
    silent_stage.harness = "active".into();
    let wf = Workflow::new("Active", vec![silent_stage]);
    let mut run = Run::create(dir.path(), &settings, &wf, "test active".into(), false).unwrap();
    assert_eq!(drive(&mut run), Action::Complete);
    assert_eq!(run.status, Status::Complete);
}

#[test]
fn protected_paths_block_stage_that_edits_matching_file() {
    let dir = common::tempdir();
    init_git(dir.path());
    let mut impl_stage = stage("implement", Role::Implementation);
    impl_stage.extra_args = vec!["--write-file".into(), ".github/workflows/ci.yml".into()];
    let mut wf = Workflow::new("Protected", vec![impl_stage]);
    wf.protected_paths = vec![".github/workflows/*".into()];
    let settings = common::settings(&wf);
    let mut run = Run::create(dir.path(), &settings, &wf, "touch workflow".into(), false).unwrap();
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!(run.status, Status::Blocked);
    let err = run.error.as_deref().unwrap_or("");
    assert!(err.contains("protected path"), "{err}");
    assert!(err.contains(".github/workflows/ci.yml"), "{err}");
}

/// Snapshots skip files Git ignores, so a protected file the stage writes
/// while Git ignores it is not in the tree diff. The stage still stops
/// blocked, and an ignored protected file it leaves alone does not block it.
#[test]
fn protected_paths_cover_files_git_ignores() {
    let dir = common::tempdir();
    init_git(dir.path());
    fs::write(dir.path().join(".gitignore"), ".env\n.model-router/\n").unwrap();
    git(dir.path(), &["add", ".gitignore"]);
    git(dir.path(), &["commit", "-m", "ignore .env"]);
    fs::write(dir.path().join(".env"), "TOKEN=kept\n").unwrap();

    let run_stage = |target: &str| {
        let mut impl_stage = stage("implement", Role::Implementation);
        impl_stage.extra_args = vec!["--write-file".into(), target.into()];
        let mut wf = Workflow::new("Protected", vec![impl_stage]);
        wf.protected_paths = vec![".env".into()];
        let settings = common::settings(&wf);
        let mut run = Run::create(dir.path(), &settings, &wf, "task".into(), false).unwrap();
        let action = drive(&mut run);
        (action, run)
    };

    let (action, run) = run_stage("notes.txt");
    assert_eq!(action, Action::Complete, "{:?}", run.error);

    let (action, run) = run_stage(".env");
    assert_eq!(action, Action::Stopped);
    assert_eq!(run.status, Status::Blocked);
    let err = run.error.as_deref().unwrap_or("");
    assert!(err.contains("Git-ignored file matching"), "{err}");
}

/// A fan-out candidate that edits a protected path stops blocked, and its
/// patch never reaches the workspace.
#[test]
fn protected_paths_block_fan_out_candidates_before_they_are_applied() {
    let dir = common::tempdir();
    init_git(dir.path());
    let mut impl_stage = stage("implement", Role::Implementation);
    impl_stage.extra_args = vec!["--write-file".into(), ".github/workflows/ci.yml".into()];
    impl_stage.fan_out = Some(FanOut {
        candidates: ["one", "two"]
            .into_iter()
            .map(|model| Candidate {
                harness: "mock".into(),
                provider: "implement".into(),
                model: model.into(),
                ..Candidate::default()
            })
            .collect(),
        judge: "auto".into(),
    });
    let mut wf = Workflow::new("ProtectedFanOut", vec![impl_stage]);
    wf.protected_paths = vec![".github/**".into()];
    let settings = common::settings(&wf);
    let mut run = Run::create(dir.path(), &settings, &wf, "touch workflow".into(), false).unwrap();
    assert_eq!(drive(&mut run), Action::Stopped);
    let fan = run.stages[0].fan_out.as_ref().unwrap();
    assert_eq!(fan.candidates.len(), 2);
    for candidate in &fan.candidates {
        assert_eq!(candidate.status, Status::Blocked, "{}", candidate.id);
        let error = candidate.error.as_deref().unwrap_or("");
        assert!(error.contains("protected path"), "{error}");
        assert!(error.contains(".github/workflows/ci.yml"), "{error}");
        // The diff stays available for inspection.
        assert!(candidate.changes.is_some());
    }
    assert!(fan.selection.is_none());
    assert!(!dir.path().join(".github").exists());
}

#[test]
fn handoff_repair_nudges_resumable_session_for_valid_json() {
    let dir = common::tempdir();
    let script = dir.path().join("repair_mock.py");
    fs::write(
        &script,
        r#"import json, sys
args = sys.argv[1:]
if "--resume" in args:
    # The router records the follow-up's process group in run.json when it
    # spawns it; wait for that write, then note what it recorded.
    import os, pathlib, time
    run_json = pathlib.Path(args[args.index("--run-dir") + 1]).parents[2] / "run.json"
    recorded = None
    for _ in range(200):
        recorded = json.loads(run_json.read_text())["stages"][0]["attempts"][-1].get("process_group")
        if recorded is not None:
            break
        time.sleep(0.05)
    (run_json.parent / "repair-group.json").write_text(json.dumps([recorded, os.getpgrp()]))
    handoff = args[args.index("--handoff") + 1]
    with open(handoff, "w") as f:
        json.dump({
            "schema_version": 2,
            "summary": "repaired handoff summary",
            "verdict": "pass",
            "findings": [],
            "tests": [],
            "files_changed": [],
            "unresolved": []
        }, f)
    print(json.dumps({"type": "result", "result": "ok"}))
else:
    print(json.dumps({"type": "session", "id": "sess-repair-1"}))
    print(json.dumps({"type": "result", "result": "I finished reviewing, no JSON here."}))
"#,
    )
    .unwrap();

    let adapter = CustomAdapter {
        command: "python3".into(),
        args: vec![script.display().to_string()],
        output: Some(CustomOutput::ClaudeStream),
        session: Some(CustomSession {
            assign: None,
            reported: true,
        }),
        follow_up_args: Some(vec![
            script.display().to_string(),
            "--resume".into(),
            "{session}".into(),
            "--handoff".into(),
            "{handoff_file}".into(),
            "--run-dir".into(),
            "{run_dir}".into(),
        ]),
        ..CustomAdapter::default()
    };
    let review = Stage {
        id: "review".into(),
        name: "Review".into(),
        role: Role::Review,
        harness: "repairable".into(),
        model: "m".into(),
        mode: Mode::Unattended,
        ..Stage::default()
    };
    let mut wf = Workflow::new("Repair", vec![review]);
    wf.handoff_repair = true;
    let settings = Settings {
        workflows: vec![wf.clone()],
        adapters: BTreeMap::from([("repairable".into(), adapter)]),
        ..Settings::default()
    };
    let mut run = Run::create(dir.path(), &settings, &wf, "check repair".into(), false).unwrap();
    let cancel = AtomicBool::new(false);
    let mut output = String::new();
    let action = engine::drive(&mut run, &cancel, |event| {
        if let Event::Output(text) = event {
            output.push_str(&text);
        }
    })
    .unwrap();
    assert_eq!(action, Action::Complete);
    assert!(output.contains("handoff repair nudge"), "{output}");
    assert_eq!(
        run.stages[0].handoff.as_ref().unwrap().summary,
        "repaired handoff summary"
    );
    // The follow-up's group was recorded for crash recovery while it ran,
    // and forgotten once it ended.
    let groups: Vec<Option<u32>> =
        serde_json::from_str(&fs::read_to_string(run.directory.join("repair-group.json")).unwrap())
            .unwrap();
    assert!(
        groups[0].is_some(),
        "no group recorded during the follow-up"
    );
    assert_eq!(groups[0], groups[1]);
    assert_eq!(run.stages[0].attempts[0].process_group, None);
}

#[test]
fn handoff_repair_cancellation_marks_cancelled_and_uses_attempt_schema() {
    let dir = common::tempdir();
    let script = dir.path().join("cancel_repair_mock.py");
    fs::write(
        &script,
        r#"import json, sys, time
args = sys.argv[1:]
if "--resume" in args:
    print("repair-running", file=sys.stderr, flush=True)
    time.sleep(30)
else:
    print(json.dumps({"type": "session", "id": "sess-cancel-1"}))
    print(json.dumps({"type": "result", "result": "Missing JSON"}))
"#,
    )
    .unwrap();
    let adapter = CustomAdapter {
        command: "python3".into(),
        args: vec![script.display().to_string()],
        output: Some(CustomOutput::ClaudeStream),
        session: Some(CustomSession {
            assign: None,
            reported: true,
        }),
        follow_up_args: Some(vec![
            script.display().to_string(),
            "--resume".into(),
            "{session}".into(),
            "--prompt-file".into(),
            "{prompt_file}".into(),
            "--run-dir".into(),
            "{attempt_dir}".into(),
        ]),
        ..CustomAdapter::default()
    };
    let review = Stage {
        id: "review".into(),
        name: "Review".into(),
        role: Role::Review,
        harness: "repairable".into(),
        model: "m".into(),
        mode: Mode::Unattended,
        ..Stage::default()
    };
    let mut wf = Workflow::new("RepairCancel", vec![review]);
    wf.handoff_repair = true;
    let settings = Settings {
        workflows: vec![wf.clone()],
        adapters: BTreeMap::from([("repairable".into(), adapter)]),
        ..Settings::default()
    };
    let mut run = Run::create(dir.path(), &settings, &wf, "cancel repair".into(), false).unwrap();
    let cancel = AtomicBool::new(false);
    let action = engine::drive(&mut run, &cancel, |event| {
        if let Event::Output(text) = event {
            if text.contains("repair-running") {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    })
    .unwrap();
    assert_eq!(action, Action::Stopped);
    assert_eq!(run.status, Status::Cancelled);
    let attempt_dir = &run.stages[0].attempts[0].directory;
    let repair_prompt = fs::read_to_string(attempt_dir.join("repair/prompt.md")).unwrap();
    assert!(
        repair_prompt.contains(
            &attempt_dir
                .join("handoff.schema.json")
                .display()
                .to_string()
        ),
        "{repair_prompt}"
    );
}

/// A repair that cannot run, or whose follow-up fails, says why in the
/// stage output instead of stopping silently.
#[test]
fn handoff_repair_reports_why_it_did_not_help() {
    let dir = common::tempdir();
    let script = dir.path().join("no_repair_mock.py");
    fs::write(
        &script,
        r#"import json, sys
if "--resume" in sys.argv:
    print("follow-up refused", file=sys.stderr)
    sys.exit(3)
print(json.dumps({"type": "session", "id": "sess-1"}))
print(json.dumps({"type": "result", "result": "No JSON here."}))
"#,
    )
    .unwrap();
    let adapter = |follow_up: bool| CustomAdapter {
        command: "python3".into(),
        args: vec![script.display().to_string()],
        output: Some(CustomOutput::ClaudeStream),
        session: Some(CustomSession {
            assign: None,
            reported: true,
        }),
        follow_up_args: follow_up.then(|| {
            vec![
                script.display().to_string(),
                "--resume".into(),
                "{session}".into(),
            ]
        }),
        ..CustomAdapter::default()
    };
    let output_of = |harness: &str| {
        let review = Stage {
            id: "review".into(),
            name: "Review".into(),
            role: Role::Review,
            harness: harness.into(),
            mode: Mode::Unattended,
            ..Stage::default()
        };
        let mut wf = Workflow::new("Repair", vec![review]);
        wf.handoff_repair = true;
        let settings = Settings {
            workflows: vec![wf.clone()],
            adapters: BTreeMap::from([
                ("failing".into(), adapter(true)),
                ("without".into(), adapter(false)),
            ]),
            ..Settings::default()
        };
        let mut run = Run::create(dir.path(), &settings, &wf, "check".into(), false).unwrap();
        let mut output = String::new();
        let action = engine::drive(&mut run, &AtomicBool::new(false), |event| {
            if let Event::Output(text) = event {
                output.push_str(&text);
            }
        })
        .unwrap();
        assert_eq!(action, Action::Stopped);
        assert_eq!(run.status, Status::Blocked);
        output
    };
    let output = output_of("failing");
    assert!(
        output.contains("Handoff repair failed: the follow-up exited with code 3. "),
        "{output}"
    );
    assert!(output.contains("follow-up refused"), "{output}");
    let output = output_of("without");
    assert!(
        output.contains("Handoff repair skipped: without has no unattended follow-up command"),
        "{output}"
    );
}

/// A stall timeout cannot apply to a harness that prints only at exit.
#[test]
fn stall_timeout_is_rejected_for_harnesses_that_print_only_at_exit() {
    let stalling = |harness: &str| Stage {
        id: "work".into(),
        name: "Work".into(),
        role: Role::Implementation,
        harness: harness.into(),
        stall_timeout_seconds: Some(60),
        ..Stage::default()
    };
    let settings = |stage: Stage| Settings {
        workflows: vec![Workflow::new("Stall", vec![stage])],
        adapters: BTreeMap::from([(
            "one-document".into(),
            CustomAdapter {
                command: "tool".into(),
                output: Some(CustomOutput::GeminiJson),
                ..CustomAdapter::default()
            },
        )]),
        ..Settings::default()
    };
    for harness in ["gemini", "one-document"] {
        let error = settings(stalling(harness)).validate().unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("stall_timeout_seconds"), "{error}");
        assert!(error.contains("only when it exits"), "{error}");
    }
    settings(stalling("claude")).validate().unwrap();
    // A fan-out stage runs its candidates' harnesses.
    let mut fan = stalling("claude");
    fan.fan_out = Some(FanOut {
        candidates: vec![
            Candidate {
                harness: "claude".into(),
                ..Candidate::default()
            },
            Candidate {
                harness: "gemini".into(),
                ..Candidate::default()
            },
        ],
        judge: "auto".into(),
    });
    assert!(settings(fan).validate().is_err());
    // Without a stall timeout Gemini is fine.
    let mut plain = stalling("gemini");
    plain.stall_timeout_seconds = None;
    settings(plain).validate().unwrap();
}

/// Without a stage `billing` or a router provider, the harness decides.
#[test]
fn billing_defaults_follow_the_harness() {
    let custom = BTreeMap::from([(
        "mine".to_string(),
        CustomAdapter {
            command: "python3".into(),
            ..CustomAdapter::default()
        },
    )]);
    let expected = [
        ("codex", Billing::Subscription),
        ("claude", Billing::Subscription),
        ("cursor", Billing::Subscription),
        ("gemini", Billing::Subscription),
        ("copilot", Billing::Subscription),
        ("pi", Billing::Api),
        ("omp", Billing::Api),
        ("opencode", Billing::Api),
        ("aider", Billing::Api),
        ("goose", Billing::Api),
        ("amp", Billing::Api),
        ("mine", Billing::Api),
    ];
    for (harness, billing) in expected {
        assert_eq!(
            cost::billing_for(None, false, harness, &custom),
            billing,
            "{harness}"
        );
        // A router provider is metered whatever the harness.
        assert_eq!(
            cost::billing_for(None, true, harness, &custom),
            Billing::Api,
            "{harness} routed"
        );
        // An explicit stage setting wins over both.
        for explicit in [Billing::Api, Billing::Subscription] {
            for routed in [false, true] {
                assert_eq!(
                    cost::billing_for(Some(explicit), routed, harness, &custom),
                    explicit
                );
            }
        }
    }

    // Through a run: a custom adapter's stage counts as metered API spend
    // unless the stage says otherwise.
    let stage_on = |id: &str, billing: Option<Billing>| Stage {
        id: id.into(),
        name: id.into(),
        role: Role::Research,
        harness: "mine".into(),
        billing,
        ..Stage::default()
    };
    let wf = Workflow::new(
        "Billing",
        vec![
            stage_on("default", None),
            stage_on("plan", Some(Billing::Subscription)),
        ],
    );
    let settings = Settings {
        workflows: vec![wf.clone()],
        adapters: custom,
        ..Settings::default()
    };
    let dir = common::tempdir();
    let run = Run::create(dir.path(), &settings, &wf, "billing".into(), false).unwrap();
    assert_eq!(cost::stage_billing(&run, 0), Billing::Api);
    assert_eq!(cost::stage_billing(&run, 1), Billing::Subscription);
}

#[test]
fn apply_worktree_brings_changes_back_uncommitted() {
    let dir = common::tempdir();
    init_git(dir.path());
    let mut impl_stage = stage("implement", Role::Implementation);
    impl_stage.extra_args = vec!["--write-file".into(), "added_from_worktree.txt".into()];
    let wf = Workflow::new("Worktree", vec![impl_stage]);
    let settings = common::settings(&wf);
    let mut run = Run::create(dir.path(), &settings, &wf, "add file".into(), true).unwrap();
    assert_eq!(drive(&mut run), Action::Complete);
    assert!(!dir.path().join("added_from_worktree.txt").exists());

    let (plan, check) = engine::apply_worktree(&run, true).unwrap();
    assert_eq!(check, fanout::Applied::Clean);
    assert_eq!(plan.files.len(), 1);
    assert!(!dir.path().join("added_from_worktree.txt").exists());

    let (_, applied) = engine::apply_worktree(&run, false).unwrap();
    assert_eq!(applied, fanout::Applied::Clean);
    assert!(dir.path().join("added_from_worktree.txt").exists());
    // Must remain uncommitted (HEAD unchanged, file untracked in checkout).
    assert_eq!(git(dir.path(), &["rev-list", "--count", "HEAD"]), "1");
}

#[test]
fn ship_blocks_added_secrets_and_protected_paths() {
    let dir = common::tempdir();
    init_git(dir.path());
    let secret = format!("token = \"{}1234567890abcdefghij1234567890\"\n", "ghp_");
    let writer = dir.path().join("write_secret.py");
    fs::write(
        &writer,
        format!(
            r#"import json, sys
with open("leaked.txt", "w") as f:
    f.write({secret:?})
with open(sys.argv[1], "w") as f:
    json.dump({{"schema_version": 2, "summary": "wrote file", "verdict": "complete", "findings": [], "tests": [], "files_changed": ["leaked.txt"], "unresolved": []}}, f)
"#
        ),
    )
    .unwrap();
    let impl_stage = Stage {
        id: "implement".into(),
        name: "Implement".into(),
        role: Role::Implementation,
        harness: "secret-writer".into(),
        ..Stage::default()
    };
    let wf = Workflow::new("SecretShip", vec![impl_stage]);
    let settings = Settings {
        workflows: vec![wf.clone()],
        adapters: BTreeMap::from([(
            "secret-writer".into(),
            CustomAdapter {
                command: "python3".into(),
                args: vec![writer.display().to_string(), "{handoff_file}".into()],
                ..CustomAdapter::default()
            },
        )]),
        ..Settings::default()
    };
    let mut run = Run::create(dir.path(), &settings, &wf, "leak test".into(), false).unwrap();
    assert_eq!(drive(&mut run), Action::Complete);
    let err = ship::plan(&run, &ship::Options::default()).unwrap_err();
    assert!(
        format!("{err:#}").contains("Secret scanner found potential secret"),
        "{err:#}"
    );
}

#[test]
fn budget_warn_continues_and_billing_splits_api_vs_subscription() {
    let dir = common::tempdir();
    let script = dir.path().join("cost_mock.py");
    fs::write(
        &script,
        r#"import json, sys
with open(sys.argv[1], "w") as f:
    json.dump({"schema_version": 2, "summary": "done", "verdict": "complete", "findings": [], "tests": [], "files_changed": [], "unresolved": []}, f)
print(json.dumps({"type": "result", "total_cost_usd": 2.0, "usage": {"input_tokens": 100, "output_tokens": 50}}))
"#,
    )
    .unwrap();
    let adapter = CustomAdapter {
        command: "python3".into(),
        args: vec![script.display().to_string(), "{handoff_file}".into()],
        output: Some(CustomOutput::ClaudeStream),
        ..CustomAdapter::default()
    };
    let s1 = Stage {
        id: "a".into(),
        name: "A".into(),
        role: Role::Research,
        harness: "c".into(),
        billing: Some(Billing::Api),
        ..Stage::default()
    };
    let s2 = Stage {
        id: "b".into(),
        name: "B".into(),
        role: Role::Implementation,
        harness: "c".into(),
        billing: Some(Billing::Subscription),
        ..Stage::default()
    };
    let mut wf = Workflow::new("WarnBudget", vec![s1, s2]);
    wf.budget = Budget {
        max_usd: Some(1.0),
        max_tokens: None,
        on_exceed: BudgetAction::Warn,
    };
    let settings = Settings {
        workflows: vec![wf.clone()],
        adapters: BTreeMap::from([("c".into(), adapter)]),
        ..Settings::default()
    };
    let mut run = Run::create(dir.path(), &settings, &wf, "warn budget".into(), false).unwrap();
    let cancel = AtomicBool::new(false);
    let mut output = String::new();
    let action = engine::drive(&mut run, &cancel, |event| {
        if let Event::Output(text) = event {
            output.push_str(&text);
        }
    })
    .unwrap();
    assert_eq!(action, Action::Complete);
    assert!(output.contains("on_exceed = warn"), "{output}");
    let summary = cost::aggregate([&run], None, &cost::Pricing::default());
    assert!((summary.total.api.usd - 2.0).abs() < 1e-9);
    assert!((summary.total.subscription.usd - 2.0).abs() < 1e-9);
    let rendered = cost::render_summary(&summary);
    assert!(rendered.contains("billed API $2.00"), "{rendered}");
    assert!(
        rendered.contains("subscription API-equivalent $2.00"),
        "{rendered}"
    );
}

/// Fan-out candidates' spend counts toward budgets, reports, and history,
/// including the candidates of a round that a repair rewind discarded.
#[test]
fn fan_out_candidate_spend_counts_toward_budgets_after_a_rewind() {
    let dir = common::tempdir();
    init_git(dir.path());
    let mut implement = stage("implement", Role::Implementation);
    implement.fan_out = Some(FanOut {
        candidates: ["lines-1", "lines-2"]
            .into_iter()
            .map(|model| Candidate {
                harness: "costly".into(),
                provider: "implement".into(),
                model: model.into(),
                ..Candidate::default()
            })
            .collect(),
        judge: "auto".into(),
    });
    let mut review = stage("review", Role::Review);
    review.extra_args = vec!["--review-fail-until".into(), "1".into()];
    let mut wf = Workflow::new("CostlyFanOut", vec![implement, review]);
    wf.max_repair_passes = 1;
    wf.budget = Budget {
        max_usd: Some(3.5),
        max_tokens: None,
        on_exceed: BudgetAction::Stop,
    };
    let mut settings = common::settings(&wf);
    let mut costly = settings.adapters["mock"].clone();
    costly.output = Some(CustomOutput::ClaudeStream);
    // Stage extra arguments stay with the stage's own harness.
    costly
        .args
        .extend(["--write-file", "out.txt", "--cost", "1"].map(String::from));
    settings.adapters.insert("costly".into(), costly);
    let mut run = Run::create(dir.path(), &settings, &wf, "costly".into(), false).unwrap();
    // Round one ($2) passes the gate; the review's repair rewinds, and
    // round two brings the run to $4 before the review runs again.
    assert_eq!(drive(&mut run), Action::Stopped);
    let error = run.error.clone().unwrap();
    assert!(
        error.starts_with("Budget exceeded: $4.00 of $3.50 (reported)"),
        "{error}"
    );
    assert_eq!(common::count(dir.path(), "review"), 1);
    let saved = engine::load(dir.path(), &run.id).unwrap();
    assert_eq!(saved.stages[0].discarded.len(), 2);
    assert_eq!(saved.stages[0].fan_out.as_ref().unwrap().round, 2);
    // The stage's own attempt in each round reports nothing.
    let pricing = cost::Pricing::default();
    let spent = "$4.00 (reported) · cost unknown for 2 of 6 attempts";
    assert_eq!(cost::stage_totals(&saved, &pricing)[0].text(), spent);
    let report = model_router::report::build_with(&saved, &pricing);
    assert_eq!(report.stages[0].usage.cost.text(), spent);
    assert!(report.usage.cost.text().starts_with("$4.00 (reported)"));
    let summary = cost::aggregate([&saved], None, &pricing);
    // Both rounds, under the candidates' own harness and models.
    let rows: Vec<_> = summary
        .rows
        .iter()
        .filter(|r| r.harness == "costly")
        .map(|r| (r.model.as_str(), r.cost.text()))
        .collect();
    let two = "$2.00 (reported)".to_owned();
    assert_eq!(rows, [("lines-1", two.clone()), ("lines-2", two)]);
}

#[test]
fn apply_detected_harnesses_clears_models_and_adapts_unavailable_harnesses() {
    let mut wf = Workflow::default();
    presets::apply_detected_harnesses(&mut wf, &["pi", "gemini"]);
    for stage in &wf.stages {
        assert!(stage.model.is_empty(), "stage {} kept model", stage.id);
        assert!(
            matches!(stage.harness.as_str(), "pi" | "gemini"),
            "unexpected harness {}",
            stage.harness
        );
    }
    let review = wf.stages.iter().find(|s| s.role == Role::Review).unwrap();
    assert_eq!(review.harness, "gemini");
}

#[test]
fn apply_detected_harnesses_keeps_read_only_stages_read_only() {
    let summary_of = |available: &[&str]| {
        let mut wf = Workflow::default();
        let before = wf
            .stages
            .iter()
            .find(|s| s.role == Role::Summary)
            .unwrap()
            .clone();
        // The starter summary is Codex with its read-only sandbox.
        assert_eq!(before.harness, "codex");
        assert_eq!(before.harness_settings["sandbox"], "read-only");
        presets::apply_detected_harnesses(&mut wf, available);
        wf.stages
            .into_iter()
            .find(|s| s.role == Role::Summary)
            .unwrap()
    };
    // Still installed: untouched.
    let summary = summary_of(&["codex", "pi"]);
    assert_eq!(summary.harness, "codex");
    assert_eq!(summary.harness_settings["sandbox"], "read-only");
    assert_eq!(summary.sandbox, None);
    // The new harness has a read-only setting of its own.
    let summary = summary_of(&["gemini"]);
    assert_eq!(summary.harness, "gemini");
    assert_eq!(
        summary.harness_settings,
        BTreeMap::from([("approval_mode".to_owned(), "plan".to_owned())])
    );
    assert_eq!(summary.sandbox, None);
    let summary = summary_of(&["claude"]);
    assert_eq!(
        summary.harness_settings,
        BTreeMap::from([("permission_mode".to_owned(), "plan".to_owned())])
    );
    // It has none: the router's sandbox keeps the stage read-only.
    let summary = summary_of(&["pi", "gemini"]);
    assert_eq!(summary.harness, "pi");
    assert!(summary.harness_settings.is_empty());
    assert_eq!(
        summary.sandbox,
        Some(model_router::config::Sandbox::ReadOnly)
    );
}

#[test]
fn apply_detected_harnesses_reviews_with_another_harness_than_implements() {
    let harnesses = |available: &[&str]| {
        let mut wf = Workflow::default();
        presets::apply_detected_harnesses(&mut wf, available);
        let of = |role: Role| {
            wf.stages
                .iter()
                .find(|s| s.role == role)
                .unwrap()
                .harness
                .clone()
        };
        (of(Role::Implementation), of(Role::Review))
    };
    // Whichever installed harness implements, the review uses another one,
    // even when the review's own harness was installed already.
    for available in [
        &["codex", "claude"][..],
        &["claude", "codex"],
        &["codex", "pi"],
        &["claude", "gemini", "pi"],
        &["pi", "gemini"],
    ] {
        let (implementation, review) = harnesses(available);
        assert!(
            available.contains(&implementation.as_str()),
            "{available:?}"
        );
        assert!(available.contains(&review.as_str()), "{available:?}");
        assert_ne!(implementation, review, "{available:?}");
    }
    // With one harness there is no other to choose.
    assert_eq!(harnesses(&["pi"]), ("pi".into(), "pi".into()));
}

#[test]
fn trust_gates_ship_secret_scanner_and_disabling_secret_scan() {
    let mut settings = Settings::default();
    let ship_stage = Stage {
        id: "ship".into(),
        name: "Ship".into(),
        role: Role::Ship,
        ship: Some(ship::Options {
            secret_scan: false,
            secret_scanner: vec!["gitleaks".into(), "protect".into()],
            ..ship::Options::default()
        }),
        ..Stage::default()
    };
    settings.workflows[0].stages.push(ship_stage);
    let items = model_router::trust::dangerous(&settings);
    assert!(
        items.iter().any(|i| i.contains("ship secret_scanner")),
        "{items:?}"
    );
    assert!(
        items.iter().any(|i| i.contains("ship secret_scan = false")),
        "{items:?}"
    );
    model_router::trust::ignore(&mut settings);
    let sanitized = settings.workflows[0]
        .stages
        .last()
        .unwrap()
        .ship
        .as_ref()
        .unwrap();
    assert!(sanitized.secret_scan);
    assert!(sanitized.secret_scanner.is_empty());
    let restored = model_router::trust::restore(&settings);
    let orig = restored.workflows[0]
        .stages
        .last()
        .unwrap()
        .ship
        .as_ref()
        .unwrap();
    assert!(!orig.secret_scan);
    assert_eq!(orig.secret_scanner, ["gitleaks", "protect"]);
}

/// A run in `dir` whose single stage runs `body` (Python) in the workspace
/// and writes a complete handoff.
fn scripted_run(dir: &Path, body: &str) -> Run {
    scripted_run_in(dir, body, false)
}

/// `scripted_run`, in its own worktree when `worktree`.
fn scripted_run_in(dir: &Path, body: &str, worktree: bool) -> Run {
    let script = dir.join(".git/scripted_stage.py");
    fs::write(
        &script,
        format!(
            r#"import json, sys
{body}
with open(sys.argv[1], "w") as f:
    json.dump({{"schema_version": 2, "summary": "wrote files", "verdict": "complete", "findings": [], "tests": [], "files_changed": [], "unresolved": []}}, f)
"#
        ),
    )
    .unwrap();
    let wf = Workflow::new(
        "Scripted",
        vec![Stage {
            id: "implement".into(),
            name: "Implement".into(),
            role: Role::Implementation,
            harness: "scripted".into(),
            ..Stage::default()
        }],
    );
    let settings = Settings {
        workflows: vec![wf.clone()],
        adapters: BTreeMap::from([(
            "scripted".into(),
            CustomAdapter {
                command: "python3".into(),
                args: vec![script.display().to_string(), "{handoff_file}".into()],
                ..CustomAdapter::default()
            },
        )]),
        ..Settings::default()
    };
    let mut run = Run::create(dir, &settings, &wf, "scripted task".into(), worktree).unwrap();
    assert_eq!(drive(&mut run), Action::Complete);
    run
}

/// The built-in scan reads the whole diff: a secret after more than
/// `snapshot::PATCH_LIMIT` (2 MiB) of added lines still blocks the ship.
#[test]
fn ship_secret_scan_reads_diffs_past_the_patch_limit() {
    let dir = common::tempdir();
    init_git(dir.path());
    let secret = format!("{}1234567890abcdefghij1234567890", "ghp_");
    let run = scripted_run(
        dir.path(),
        &format!(
            r#"with open("big.txt", "w") as f:
    for i in range(60000):
        f.write("filler line %06d %s\n" % (i, "x" * 40))
    f.write("token = {secret}\n")
"#
        ),
    );
    assert!(fs::metadata(dir.path().join("big.txt")).unwrap().len() > 3 * 1024 * 1024);
    let err = ship::plan(&run, &ship::Options::default()).unwrap_err();
    let err = format!("{err:#}");
    assert!(
        err.contains("Secret scanner found potential secret"),
        "{err}"
    );
    assert!(err.contains("big.txt:60001"), "{err}");
}

/// Only files planned for the commit are scanned: a secret in a file the
/// ship skips does not block it.
#[test]
fn ship_secret_scan_covers_only_planned_files() {
    let dir = common::tempdir();
    init_git(dir.path());
    let secret = format!("{}1234567890abcdefghij1234567890", "ghp_");
    let run = scripted_run(
        dir.path(),
        &format!(
            r#"with open("README.md", "a") as f:
    f.write("more\n")
with open("scratch.txt", "w") as f:
    f.write("token = {secret}\n")
"#
        ),
    );
    let options = ship::Options {
        include_untracked: false,
        ..ship::Options::default()
    };
    let plan = ship::plan(&run, &options).unwrap();
    assert_eq!(plan.files.len(), 1);
    assert_eq!(plan.files[0].path, "README.md");
    assert!(plan.skipped.iter().any(|s| s.contains("scratch.txt")));
    // Once the file is planned, the same secret blocks the ship.
    let err = ship::plan(&run, &ship::Options::default()).unwrap_err();
    assert!(format!("{err:#}").contains("scratch.txt:1"), "{err:#}");
}

/// The external scanner gets the ship's private index at plan time and at
/// commit time: its `git diff --cached` lists the planned files and never
/// what the user staged themselves.
#[test]
fn ship_external_scanner_reads_the_private_index_in_both_phases() {
    let dir = common::tempdir();
    init_git(dir.path());
    // The user's own staged change, made before the run.
    fs::write(dir.path().join("mine.txt"), "staged by the user\n").unwrap();
    git(dir.path(), &["add", "mine.txt"]);
    let run = scripted_run(
        dir.path(),
        r#"with open("added.txt", "w") as f:
    f.write("from the run\n")
"#,
    );
    let scanner = dir.path().join(".git/mock_scanner.py");
    let log = dir.path().join(".git/scanner.jsonl");
    fs::write(
        &scanner,
        r#"import json, os, subprocess, sys
staged = subprocess.run(["git", "diff", "--cached", "--name-only"], capture_output=True, text=True, check=True).stdout.split()
with open(sys.argv[1], "a") as f:
    f.write(json.dumps({"staged": staged, "args": sys.argv[2:], "env": os.environ["MODEL_ROUTER_SHIP_FILES"].split("\n")}) + "\n")
sys.exit(3 if os.path.exists(sys.argv[1] + ".fail") else 0)
"#,
    )
    .unwrap();
    let options = ship::Options {
        secret_scanner: vec![
            "python3".into(),
            scanner.display().to_string(),
            log.display().to_string(),
            "{files}".into(),
        ],
        ..ship::Options::default()
    };
    let entries = || -> Vec<serde_json::Value> {
        fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    let expected = serde_json::json!({
        "staged": ["added.txt"], "args": ["added.txt"], "env": ["added.txt"],
    });

    let plan = ship::plan(&run, &options).unwrap();
    assert_eq!(entries(), std::slice::from_ref(&expected), "plan-time scan");
    // Planning left no scratch index behind and did not touch the real one.
    assert!(fs::read_dir(&run.directory).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("ship-scan-")
    }));
    assert_eq!(
        git(dir.path(), &["diff", "--cached", "--name-only"]),
        "mine.txt"
    );

    let ship_dir = run.directory.join("ship-test");
    ship::save_plan(&plan, &ship_dir).unwrap();
    // A scanner failure at commit time stops before the commit.
    fs::write(dir.path().join(".git/scanner.jsonl.fail"), "").unwrap();
    let outcome = ship::apply(&plan, &ship_dir, |_| {});
    let error = outcome.error.unwrap_or_default();
    assert!(error.contains("refusing to ship"), "{error}");
    assert!(outcome.commit.is_none());
    assert!(!ship_dir.join("ship.index").exists());
    assert_eq!(git(dir.path(), &["rev-list", "--count", "HEAD"]), "1");
    fs::remove_file(dir.path().join(".git/scanner.jsonl.fail")).unwrap();

    let outcome = ship::apply(&plan, &ship_dir, |_| {});
    assert_eq!(outcome.error, None);
    assert_eq!(
        entries(),
        [expected.clone(), expected.clone(), expected],
        "commit-time scans"
    );
    assert!(!ship_dir.join("ship.index").exists());
    assert_eq!(
        git(dir.path(), &["show", "--name-only", "--format=", "HEAD"]),
        "added.txt"
    );
    // The user's staged file is still staged and uncommitted.
    assert_eq!(
        git(dir.path(), &["diff", "--cached", "--name-only"]),
        "mine.txt"
    );
}

/// `apply --check` reports what a real apply would do, including a 3-way
/// merge with checkout edits, and changes nothing.
#[test]
fn apply_check_predicts_merges_and_conflicts_without_touching_the_checkout() {
    let dir = common::tempdir();
    init_git(dir.path());
    let lines = |edit: &[(usize, &str)]| -> String {
        (1..=8)
            .map(|n| match edit.iter().find(|(line, _)| *line == n) {
                Some((_, text)) => format!("{text}\n"),
                None => format!("line {n}\n"),
            })
            .collect()
    };
    let shared = dir.path().join("shared.txt");
    fs::write(&shared, lines(&[])).unwrap();
    git(dir.path(), &["add", "shared.txt"]);
    git(dir.path(), &["commit", "-m", "shared"]);
    // The run edits line 3 in its worktree.
    let run = scripted_run_in(
        dir.path(),
        r#"text = open("shared.txt").read().replace("line 3\n", "line 3 from the run\n")
open("shared.txt", "w").write(text)
"#,
        true,
    );
    assert_eq!(fs::read_to_string(&shared).unwrap(), lines(&[]));
    let status = || {
        git(
            dir.path(),
            &["status", "--porcelain", "--untracked-files=no"],
        )
    };

    // Meanwhile the user edits line 6: inside the patch's context, so a
    // plain apply fails, but the changes merge.
    let user_edit = lines(&[(6, "line 6 from the user")]);
    fs::write(&shared, &user_edit).unwrap();
    let before = status();
    let (_, check) = engine::apply_worktree(&run, true).unwrap();
    assert_eq!(check, fanout::Applied::Merged);
    assert_eq!(fs::read_to_string(&shared).unwrap(), user_edit);
    assert_eq!(status(), before);

    // An edit to the same line conflicts; the check says so and a real
    // apply leaves the checkout as it was.
    let clash = lines(&[(3, "line 3 from the user")]);
    fs::write(&shared, &clash).unwrap();
    let (_, check) = engine::apply_worktree(&run, true).unwrap();
    assert!(matches!(check, fanout::Applied::Conflict(_)), "{check:?}");
    assert_eq!(fs::read_to_string(&shared).unwrap(), clash);
    let (_, applied) = engine::apply_worktree(&run, false).unwrap();
    assert!(
        matches!(applied, fanout::Applied::Conflict(_)),
        "{applied:?}"
    );
    assert_eq!(fs::read_to_string(&shared).unwrap(), clash);

    // The real apply does what the check predicted.
    fs::write(&shared, &user_edit).unwrap();
    let (_, applied) = engine::apply_worktree(&run, false).unwrap();
    assert_eq!(applied, fanout::Applied::Merged);
    assert_eq!(
        fs::read_to_string(&shared).unwrap(),
        lines(&[(3, "line 3 from the run"), (6, "line 6 from the user")])
    );
    assert_eq!(git(dir.path(), &["rev-list", "--count", "HEAD"]), "2");
}
