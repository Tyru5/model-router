//! Integration tests for Track 2 engine resilience features:
//! 1. Transient retry with capped exponential backoff
//! 2. Trust-gated lifecycle hooks (`before_stage`, `after_stage`, `before_ship`, `on_failure`)
//! 3. Declarative diff policies (`block` and `advise`)
//! 4. Conditional stages (`when_changed`) and review quorum (`review_quorum`)
//! 5. Live mid-stream budget enforcement
mod common;

use common::{drive, stage};
use model_router::{
    config::{
        self, Billing, Budget, BudgetAction, CustomAdapter, CustomOutput, Hook, Hooks, Retry, Role,
        Settings, Stage, Workflow,
    },
    cost,
    engine::{self, Action, Event, Run, Status},
    events,
    policy::{DiffPolicy, PolicyAction},
    ship, transitions, trust,
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

fn init_test_config_dir() -> PathBuf {
    static DIR: Mutex<Option<tempfile::TempDir>> = Mutex::new(None);
    extern "C" fn cleanup() {
        if let Ok(mut guard) = DIR.lock() {
            guard.take();
        }
    }
    let mut guard = DIR.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        let temp = tempfile::tempdir().expect("test config dir");
        // SAFETY: initialized once under the mutex before tests run, and
        // `cleanup` is a plain extern "C" function.
        unsafe {
            std::env::set_var("MODEL_ROUTER_CONFIG_DIR", temp.path());
            libc::atexit(cleanup);
        }
        *guard = Some(temp);
    }
    guard.as_ref().unwrap().path().to_path_buf()
}

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
    git(
        dir,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "initial",
        ],
    );
}

// -----------------------------------------------------------------------------
// 1. Transient retry with capped exponential backoff
// -----------------------------------------------------------------------------

#[test]
fn transient_retry_succeeds_after_provider_errors_preserving_model_and_transitions() {
    init_test_config_dir();
    let dir = common::tempdir();
    let script = dir.path().join("transient.py");
    fs::write(
        &script,
        r#"import json, pathlib, sys
count_file = pathlib.Path(sys.argv[1])
model = sys.argv[2]
handoff_path = pathlib.Path(sys.argv[3])
n = int(count_file.read_text()) if count_file.exists() else 0
n += 1
count_file.write_text(str(n))
if n == 1:
    sys.stderr.write("Error: HTTP 429 Too Many Requests (rate_limit_error)\n")
    sys.exit(1)
if n == 2:
    sys.stderr.write("API Error: 529 overloaded_error\n")
    sys.exit(1)
handoff_path.write_text(json.dumps({
    "schema_version": 2,
    "summary": f"Recovered on attempt {n} with model {model}",
    "verdict": "complete",
    "findings": [],
    "tests": [],
    "files_changed": [],
    "unresolved": []
}))
"#,
    )
    .unwrap();

    let count_file = dir.path().join("attempts.count");
    let adapter = CustomAdapter {
        command: "python3".into(),
        args: vec![
            script.display().to_string(),
            count_file.display().to_string(),
            "{model}".into(),
            "{handoff_file}".into(),
        ],
        ..CustomAdapter::default()
    };
    let settings = Settings {
        adapters: BTreeMap::from([("flaky".into(), adapter)]),
        ..Settings::default()
    };
    let mut s = Stage {
        id: "impl".into(),
        name: "Implement".into(),
        role: Role::Implementation,
        harness: "flaky".into(),
        model: "pinned-model-v1".into(),
        ..Stage::default()
    };
    s.retry = Some(Retry {
        max_attempts: 3,
        base_delay_seconds: 1,
        max_delay_seconds: 1,
    });
    let wf = Workflow::new("RetryFlow", vec![s]);
    let mut run = Run::create(dir.path(), &settings, &wf, "test retry".into(), false).unwrap();
    let mut output = String::new();
    let action = engine::drive(&mut run, &AtomicBool::new(false), |ev| {
        if let Event::Output(text) = ev {
            output.push_str(&text);
        }
    })
    .unwrap();
    assert_eq!(action, Action::Complete);
    assert_eq!(run.status, Status::Complete);
    assert_eq!(run.stages[0].attempts.len(), 3);
    assert_eq!(run.stages[0].attempts[0].status, Status::Failed);
    assert_eq!(run.stages[0].attempts[1].status, Status::Failed);
    assert_eq!(run.stages[0].attempts[2].status, Status::Complete);
    // No silent model substitution: every attempt used the exact configured model.
    for attempt in &run.stages[0].attempts {
        assert_eq!(attempt.model, "pinned-model-v1");
    }
    assert!(
        output.contains("Transient failure") && output.contains("retrying 1/3"),
        "{output}"
    );
    assert!(output.contains("retrying 2/3"), "{output}");

    let records = transitions::read(&run.directory).unwrap();
    assert!(
        records
            .iter()
            .any(|r| r.reason.contains("Transient error retry 1/3")),
        "{records:?}"
    );
    assert!(
        records
            .iter()
            .any(|r| r.reason.contains("Transient error retry 2/3")),
        "{records:?}"
    );
}

#[test]
fn non_transient_errors_do_not_retry_and_cancel_interrupts_backoff() {
    init_test_config_dir();
    let dir = common::tempdir();
    let script = dir.path().join("auth_fail.py");
    fs::write(
        &script,
        r#"import sys
mode = sys.argv[1]
if mode == "auth":
    sys.stderr.write("Error 401: Invalid API key provided\n")
    sys.exit(1)
else:
    sys.stderr.write("HTTP 503 Service Unavailable\n")
    sys.exit(1)
"#,
    )
    .unwrap();

    let adapter = |mode: &str| CustomAdapter {
        command: "python3".into(),
        args: vec![script.display().to_string(), mode.into()],
        ..CustomAdapter::default()
    };
    let settings = Settings {
        adapters: BTreeMap::from([
            ("auth".into(), adapter("auth")),
            ("503".into(), adapter("503")),
        ]),
        ..Settings::default()
    };

    // 1. Auth error fails on attempt 1 despite max_attempts = 3.
    let mut wf_auth = Workflow::new(
        "AuthFail",
        vec![Stage {
            id: "s".into(),
            name: "Auth".into(),
            role: Role::Implementation,
            harness: "auth".into(),
            ..Stage::default()
        }],
    );
    wf_auth.retry = Retry {
        max_attempts: 3,
        base_delay_seconds: 1,
        max_delay_seconds: 1,
    };
    let mut run_auth =
        Run::create(dir.path(), &settings, &wf_auth, "test auth".into(), false).unwrap();
    assert_eq!(drive(&mut run_auth), Action::Stopped);
    assert_eq!(run_auth.status, Status::Failed);
    assert_eq!(run_auth.stages[0].attempts.len(), 1);

    // 2. Cancel during backoff interrupts promptly without waiting out a 30s delay.
    let mut wf_cancel = Workflow::new(
        "CancelBackoff",
        vec![Stage {
            id: "s".into(),
            name: "Transient".into(),
            role: Role::Implementation,
            harness: "503".into(),
            ..Stage::default()
        }],
    );
    wf_cancel.retry = Retry {
        max_attempts: 2,
        base_delay_seconds: 30,
        max_delay_seconds: 60,
    };
    let mut run_cancel = Run::create(
        dir.path(),
        &settings,
        &wf_cancel,
        "test cancel".into(),
        false,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let action = engine::drive(&mut run_cancel, &cancel, |ev| {
        if let Event::Output(text) = ev {
            if text.contains("Transient failure") {
                cancel.store(true, Ordering::Relaxed);
            }
        }
    })
    .unwrap();
    assert_eq!(action, Action::Stopped);
    assert_eq!(run_cancel.status, Status::Cancelled);
    assert_eq!(run_cancel.stages[0].attempts.len(), 1);
    assert!(started.elapsed() < Duration::from_secs(3));
}

// -----------------------------------------------------------------------------
// 2. Trust-gated lifecycle hooks
// -----------------------------------------------------------------------------

#[test]
fn lifecycle_hooks_are_gated_by_trust_run_in_order_and_enforce_abort_rules() {
    init_test_config_dir();
    let dir = common::tempdir();
    init_git(dir.path());
    let marker = dir.path().join("hooks.log");
    let append_py = format!(
        "import os, pathlib, sys; p = pathlib.Path({:?}); p.write_text((p.read_text() if p.exists() else '') + sys.argv[1] + ':' + os.environ.get('MODEL_ROUTER_HOOK', '') + '\\n')",
        marker.display().to_string()
    );

    let mock_path = format!("{}/examples/mock_harness.py", env!("CARGO_MANIFEST_DIR"));
    let toml_text = format!(
        r#"version = 2

[adapters.mock]
command = "python3"
args = [{mock_path:?}, "--role", "{{provider}}", "--model", "{{model}}", "--prompt-file", "{{prompt_file}}", "--handoff-file", "{{handoff_file}}"]

[[workflows]]
name = "Hooked"
max_repair_passes = 0

[workflows.hooks]
before_stage = [{{ command = ["python3", "-c", {append_py:?}, "wf-before"], timeout_seconds = 10 }}]
after_stage = [{{ command = ["python3", "-c", {append_py:?}, "wf-after"], timeout_seconds = 10 }}]

[[workflows.stages]]
id = "impl"
name = "Implement"
role = "implementation"
harness = "mock"
provider = "implementation"
model = "test-model"

[workflows.stages.hooks]
before_stage = [{{ command = ["python3", "-c", {append_py:?}, "stage-before"], timeout_seconds = 10 }}]
after_stage = [
  {{ command = ["python3", "-c", {append_py:?}, "stage-after"], timeout_seconds = 10 }},
  {{ command = ["python3", "-c", "import sys; sys.exit(3)"], timeout_seconds = 10 }},
]
"#
    );
    fs::write(dir.path().join(".model-router.toml"), &toml_text).unwrap();

    // 1. Untrusted project config flags the hooks and neutralizes them in `.safe()`.
    let loaded = config::load_config(dir.path(), None).unwrap();
    let review = loaded.review().expect("hooks require project trust");
    let review_text = review.items.join("\n");
    assert!(
        review_text.contains("hook before_stage in Hooked:"),
        "{review_text}"
    );
    assert!(
        review_text.contains("hook after_stage in Hooked / impl:"),
        "{review_text}"
    );
    let safe_settings = loaded.clone().safe();
    assert!(safe_settings.workflows[0].hooks.is_empty());
    assert!(safe_settings.workflows[0].stages[0].hooks.is_empty());

    // 2. Preview shows the hooks once trusted.
    trust::approve(review).unwrap();
    let trusted = config::load_config(dir.path(), None).unwrap();
    assert_eq!(trusted.trust, trust::Trust::Trusted);
    let preview = engine::preview(
        &trusted.settings,
        &trusted.settings.workflows[0],
        dir.path(),
        "preview hooks",
    )
    .unwrap();
    assert!(
        preview.contains("Workflow hook (before_stage):"),
        "{preview}"
    );
    assert!(preview.contains("hook (after_stage):"), "{preview}");

    // 3. Running executes workflow + stage hooks; failing `after_stage` hook logs
    // without masking the stage's Complete outcome.
    let mut run = Run::create(
        dir.path(),
        &trusted.settings,
        &trusted.settings.workflows[0],
        "run hooks".into(),
        false,
    )
    .unwrap();
    assert_eq!(drive(&mut run), Action::Complete);
    assert_eq!(run.status, Status::Complete);
    let log = fs::read_to_string(&marker).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(
        lines,
        vec![
            "wf-before:before_stage",
            "stage-before:before_stage",
            "wf-after:after_stage",
            "stage-after:after_stage",
        ]
    );

    // 4. Tampering with a hook in saved run.json invalidates run approval digest!
    let mut tampered = run.clone();
    tampered.workflow.hooks.before_stage[0]
        .command
        .push("--tampered".into());
    let err = trust::authorize_run(&tampered).unwrap_err();
    assert!(
        format!("{err:#}").contains("changed since this run was approved"),
        "{err:#}"
    );
}

#[test]
fn before_stage_and_before_ship_abort_on_failure_and_trigger_on_failure() {
    init_test_config_dir();
    let dir = common::tempdir();
    init_git(dir.path());
    let fail_marker = dir.path().join("on_failure.log");
    let record_fail = Hook {
        command: vec![
            "python3".into(),
            "-c".into(),
            format!(
                "import os, pathlib; pathlib.Path({:?}).write_text(os.environ.get('MODEL_ROUTER_STAGE_ID', '') + ':' + os.environ.get('MODEL_ROUTER_STAGE_STATUS', ''))",
                fail_marker.display().to_string()
            ),
        ],
        timeout_seconds: 10,
    };

    // 1. Failing `before_stage` aborts before the harness ever runs, and runs `on_failure`.
    let mut s = stage("impl", Role::Implementation);
    s.hooks = Hooks {
        before_stage: vec![Hook {
            command: vec![
                "python3".into(),
                "-c".into(),
                "import sys; sys.stderr.write('precheck failed\\n'); sys.exit(2)".into(),
            ],
            timeout_seconds: 10,
        }],
        on_failure: vec![record_fail.clone()],
        ..Hooks::default()
    };
    let wf = Workflow::new("AbortBeforeStage", vec![s]);
    let settings = common::settings(&wf);
    let mut run = Run::create(dir.path(), &settings, &wf, "abort stage".into(), false).unwrap();
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!(run.status, Status::Failed);
    assert_eq!(common::count(dir.path(), "impl"), 0, "harness must not run");
    assert_eq!(fs::read_to_string(&fail_marker).unwrap(), "impl:failed");

    // 2. Failing `before_ship` aborts ship before any commit or branch is created.
    let mut impl_ok = stage("impl", Role::Implementation);
    impl_ok.provider = "implementation".into();
    impl_ok.hooks.before_stage.push(Hook {
        command: vec![
            "python3".into(),
            "-c".into(),
            "import pathlib; pathlib.Path('README.md').write_text('updated\\n')".into(),
        ],
        timeout_seconds: 10,
    });
    let mut ship_stage = Stage {
        id: "ship".into(),
        name: "Ship".into(),
        role: Role::Ship,
        ship: Some(ship::Options {
            confirm: false,
            branch: "mr/hook-blocked".into(),
            ..ship::Options::default()
        }),
        hooks: Hooks {
            before_ship: vec![Hook {
                command: vec![
                    "python3".into(),
                    "-c".into(),
                    "import sys; sys.exit(4)".into(),
                ],
                timeout_seconds: 10,
            }],
            on_failure: vec![record_fail],
            ..Hooks::default()
        },
        ..Stage::default()
    };
    let wf_ship = Workflow::new("AbortBeforeShip", vec![impl_ok, ship_stage.clone()]);
    let settings_ship = common::settings(&wf_ship);
    let mut run_ship = Run::create(
        dir.path(),
        &settings_ship,
        &wf_ship,
        "abort ship".into(),
        false,
    )
    .unwrap();
    assert_eq!(drive(&mut run_ship), Action::Stopped);
    assert_eq!(run_ship.status, Status::Blocked);
    assert!(
        run_ship
            .error
            .as_deref()
            .unwrap_or("")
            .contains("before_ship"),
        "{:?}",
        run_ship.error
    );
    let branches = git(dir.path(), &["branch", "--list", "mr/hook-blocked"]);
    assert!(
        branches.is_empty(),
        "branch must not be created: {branches}"
    );
    assert_eq!(fs::read_to_string(&fail_marker).unwrap(), "ship:blocked");
    ship_stage.hooks.before_ship.clear();
}

// -----------------------------------------------------------------------------
// 3. Declarative diff policies (`block` and `advise`)
// -----------------------------------------------------------------------------

#[test]
fn declarative_diff_policies_block_stage_fanout_apply_and_ship_and_advise_reviews() {
    init_test_config_dir();
    let dir = common::tempdir();
    let scripts = common::tempdir();
    init_git(dir.path());

    let writer = scripts.path().join("writer.py");
    fs::write(
        &writer,
        r#"import json, pathlib, sys
target = pathlib.Path(sys.argv[1])
content = sys.argv[2]
prompt_path = pathlib.Path(sys.argv[3])
handoff_path = pathlib.Path(sys.argv[4])
verdict = sys.argv[5]
if content != "READONLY":
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(content + "\n")
prompt_text = prompt_path.read_text()
handoff_path.write_text(json.dumps({
    "schema_version": 2,
    "summary": "PROMPT_HAS_ADVISORY=" + str("Verify backwards compatibility" in prompt_text),
    "verdict": verdict,
    "findings": [],
    "tests": [],
    "files_changed": [str(target.name)],
    "unresolved": []
}))
"#,
    )
    .unwrap();

    let make_adapter = |rel: &str, content: &str, verdict: &str| CustomAdapter {
        command: "python3".into(),
        args: vec![
            writer.display().to_string(),
            rel.into(),
            content.into(),
            "{prompt_file}".into(),
            "{handoff_file}".into(),
            verdict.into(),
        ],
        ..CustomAdapter::default()
    };

    let policies = vec![
        DiffPolicy {
            name: "no-forbidden-token".into(),
            action: PolicyAction::Block,
            pattern: "word:FORBIDDEN_CALL".into(),
            paths: vec!["src/**/*.rs".into()],
            message: "Do not use FORBIDDEN_CALL in src/".into(),
        },
        DiffPolicy {
            name: "migration-advisory".into(),
            action: PolicyAction::Advise,
            pattern: String::new(),
            paths: vec!["migrations/**".into()],
            message: "Verify backwards compatibility of schema changes".into(),
        },
    ];

    let settings = Settings {
        adapters: BTreeMap::from([
            (
                "bad_src".into(),
                make_adapter("src/lib.rs", "fn f() { FORBIDDEN_CALL(); }", "complete"),
            ),
            (
                "ok_migration".into(),
                make_adapter(
                    "migrations/001.sql",
                    "ALTER TABLE t ADD COLUMN c INT;",
                    "complete",
                ),
            ),
            (
                "review_pass".into(),
                make_adapter("migrations/001.sql", "READONLY", "pass"),
            ),
        ]),
        ..Settings::default()
    };

    // 1. Stage adding FORBIDDEN_CALL in src/lib.rs is blocked at stage end.
    let mut wf_block = Workflow::new(
        "PolicyBlock",
        vec![Stage {
            id: "impl".into(),
            name: "Implement".into(),
            role: Role::Implementation,
            harness: "bad_src".into(),
            ..Stage::default()
        }],
    );
    wf_block.policies = policies.clone();
    let mut run_block =
        Run::create(dir.path(), &settings, &wf_block, "test block".into(), false).unwrap();
    assert_eq!(drive(&mut run_block), Action::Stopped);
    assert_eq!(run_block.status, Status::Blocked);
    let err = run_block.error.as_deref().unwrap_or("");
    assert!(
        err.contains("no-forbidden-token") && err.contains("src/lib.rs:1"),
        "{err}"
    );
    // Clean workspace for next run.
    let _ = fs::remove_dir_all(dir.path().join("src"));

    // 2. Stage touching migrations/001.sql passes block policy and injects
    // `advise` message into the review stage prompt.
    let mut wf_advise = Workflow::new(
        "PolicyAdvise",
        vec![
            Stage {
                id: "impl".into(),
                name: "Implement".into(),
                role: Role::Implementation,
                harness: "ok_migration".into(),
                ..Stage::default()
            },
            Stage {
                id: "rev".into(),
                name: "Review".into(),
                role: Role::Review,
                harness: "review_pass".into(),
                ..Stage::default()
            },
        ],
    );
    wf_advise.policies = policies.clone();
    let mut run_advise = Run::create(
        dir.path(),
        &settings,
        &wf_advise,
        "test advise".into(),
        false,
    )
    .unwrap();
    assert_eq!(drive(&mut run_advise), Action::Complete);
    let rev_summary = &run_advise.stages[1].handoff.as_ref().unwrap().summary;
    assert_eq!(rev_summary, "PROMPT_HAS_ADVISORY=True");
    let _ = fs::remove_dir_all(dir.path().join("migrations"));

    // 3. Worktree `apply` and `ship::plan` also enforce `block` policies.
    let mut wf_wt = Workflow::new(
        "WorktreePolicy",
        vec![Stage {
            id: "impl".into(),
            name: "Implement".into(),
            role: Role::Implementation,
            harness: "ok_migration".into(),
            ..Stage::default()
        }],
    );
    let mut run_wt = Run::create(dir.path(), &settings, &wf_wt, "wt apply".into(), true).unwrap();
    assert_eq!(drive(&mut run_wt), Action::Complete);
    // Now add a policy that blocks `ALTER TABLE` before applying/shipping the finished run.
    let block_alter = DiffPolicy {
        name: "no-alter".into(),
        action: PolicyAction::Block,
        pattern: "ALTER TABLE".into(),
        paths: vec!["migrations/**".into()],
        message: "ALTER TABLE is blocked".into(),
    };
    run_wt.workflow.policies = vec![block_alter.clone()];
    let apply_err = engine::apply_worktree_plan(&run_wt).unwrap_err();
    assert!(
        format!("{apply_err:#}").contains("no-alter"),
        "{apply_err:#}"
    );
    let ship_err = ship::plan(&run_wt, &ship::Options::default()).unwrap_err();
    assert!(format!("{ship_err:#}").contains("no-alter"), "{ship_err:#}");
    wf_wt.policies.push(block_alter);
}

// -----------------------------------------------------------------------------
// 4. Conditional stages (`when_changed`) & review quorum (`review_quorum`)
// -----------------------------------------------------------------------------

#[test]
fn when_changed_skips_unmatched_stages_in_git_and_runs_outside_git() {
    init_test_config_dir();
    let dir = common::tempdir();
    init_git(dir.path());

    let mut s_impl = stage("impl", Role::Implementation);
    s_impl.provider = "implementation".into();
    s_impl.hooks.before_stage.push(Hook {
        command: vec![
            "python3".into(),
            "-c".into(),
            "import pathlib; pathlib.Path('notes.txt').write_text('changed\\n')".into(),
        ],
        timeout_seconds: 10,
    });
    let mut s_rust_only = stage("rust_check", Role::Custom);
    s_rust_only.when_changed = vec!["src/**/*.rs".into()];
    let mut s_txt_only = stage("txt_check", Role::Custom);
    s_txt_only.when_changed = vec!["*.txt".into()];

    let wf = Workflow::new(
        "Conditional",
        vec![s_impl.clone(), s_rust_only.clone(), s_txt_only.clone()],
    );
    let settings = common::settings(&wf);
    let mut run = Run::create(dir.path(), &settings, &wf, "conditional".into(), false).unwrap();
    assert_eq!(drive(&mut run), Action::Complete);
    assert_eq!(run.stages[0].status, Status::Complete);
    assert_eq!(run.stages[1].status, Status::Skipped);
    assert!(
        run.stages[1]
            .skipped_reason
            .as_deref()
            .unwrap_or("")
            .contains("when_changed"),
        "{:?}",
        run.stages[1].skipped_reason
    );
    assert_eq!(run.stages[2].status, Status::Complete);

    // Outside Git: fail-safe runs all conditional stages normally.
    let non_git = common::tempdir();
    let mut run_non_git =
        Run::create(non_git.path(), &settings, &wf, "non-git".into(), false).unwrap();
    assert_eq!(drive(&mut run_non_git), Action::Complete);
    assert_eq!(run_non_git.stages[1].status, Status::Complete);
    assert_eq!(run_non_git.stages[2].status, Status::Complete);
}

#[test]
fn review_quorum_requires_n_distinct_passing_reviews_and_blocks_otherwise() {
    init_test_config_dir();
    let dir = common::tempdir();
    init_git(dir.path());

    let mut s_impl = stage("impl", Role::Implementation);
    s_impl.provider = "implementation".into();
    let mut r1 = stage("review_1", Role::Review);
    r1.provider = "review_pass".into();
    let mut r2 = stage("review_2", Role::Review);
    r2.provider = "review_pass".into();

    // 1. Two passing reviews satisfy review_quorum = 2 (in a parallel dependency graph!).
    let mut r1_graph = r1.clone();
    r1_graph.after = vec!["impl".into()];
    let mut r2_graph = r2.clone();
    r2_graph.after = vec!["impl".into()];
    let mut wf_ok = Workflow::new("QuorumPass", vec![s_impl.clone(), r1_graph, r2_graph]);
    wf_ok.review_quorum = Some(2);
    let settings_ok = common::settings(&wf_ok);
    let mut run_ok = Run::create(
        dir.path(),
        &settings_ok,
        &wf_ok,
        "quorum pass".into(),
        false,
    )
    .unwrap();
    assert_eq!(drive(&mut run_ok), Action::Complete);
    assert_eq!(run_ok.status, Status::Complete);

    // 2. When one of the two reviews is skipped by `when_changed`, only 1 review
    // passed (< quorum 2), so the run blocks!
    let mut r2_skipped = r2;
    r2_skipped.when_changed = vec!["nonexistent_dir/**".into()];
    let mut wf_short = Workflow::new("QuorumShort", vec![s_impl, r1, r2_skipped]);
    wf_short.review_quorum = Some(2);
    let settings_short = common::settings(&wf_short);
    let mut run_short = Run::create(
        dir.path(),
        &settings_short,
        &wf_short,
        "quorum short".into(),
        false,
    )
    .unwrap();
    assert_eq!(drive(&mut run_short), Action::Stopped);
    assert_eq!(run_short.status, Status::Blocked);
    assert!(
        run_short
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Review quorum not met: 1 of 2"),
        "{:?}",
        run_short.error
    );
}

// -----------------------------------------------------------------------------
// 5. Live mid-stream budget enforcement
// -----------------------------------------------------------------------------

#[test]
fn live_mid_stream_budget_stops_streaming_stage_immediately_when_exceeded() {
    init_test_config_dir();
    let dir = common::tempdir();
    let streamer = dir.path().join("streamer.py");
    fs::write(
        &streamer,
        r#"import json, sys, time
handoff_path = sys.argv[1]
# Emit turn 1 (400 tokens)
print(json.dumps({"type": "turn.completed", "usage": {"input_tokens": 300, "output_tokens": 100}}), flush=True)
time.sleep(0.05)
# Emit turn 2 (700 more tokens -> 1100 total, exceeding 800 cap!)
print(json.dumps({"type": "turn.completed", "usage": {"input_tokens": 500, "output_tokens": 200}}), flush=True)
# Sleep long enough that only mid-stream termination finishes quickly
time.sleep(30)
with open(handoff_path, "w") as f:
    json.dump({"schema_version": 2, "summary": "should not finish", "verdict": "complete"}, f)
"#,
    )
    .unwrap();

    let adapter = CustomAdapter {
        command: "python3".into(),
        args: vec![streamer.display().to_string(), "{handoff_file}".into()],
        output: Some(CustomOutput::CodexJsonl),
        ..CustomAdapter::default()
    };
    let settings = Settings {
        adapters: BTreeMap::from([("streamer".into(), adapter)]),
        ..Settings::default()
    };
    let mut s = Stage {
        id: "stream".into(),
        name: "Stream".into(),
        role: Role::Implementation,
        harness: "streamer".into(),
        model: "gpt-5.4".into(),
        billing: Some(Billing::Api),
        timeout_seconds: 60,
        budget: Budget {
            max_tokens: Some(800),
            on_exceed: BudgetAction::Stop,
            ..Budget::default()
        },
        ..Stage::default()
    };
    let wf = Workflow::new("LiveCap", vec![s.clone()]);
    let mut run = Run::create(dir.path(), &settings, &wf, "live cap".into(), false).unwrap();
    let started = Instant::now();
    let action = drive(&mut run);
    let elapsed = started.elapsed();
    assert_eq!(action, Action::Stopped);
    assert_eq!(run.status, Status::Blocked);
    assert!(
        elapsed < Duration::from_secs(10),
        "process group should be terminated mid-stream without waiting 30s (took {elapsed:?})"
    );
    assert!(cost::budget_blocked(&run), "{:?}", run.error);
    let usage = run.stages[0].attempts[0]
        .usage
        .expect("streamed usage recorded on aborted attempt");
    assert_eq!(cost::counted_tokens(&usage), 1100);

    // Raising the token budget unblocks the run.
    s.budget.max_tokens = Some(2000);
    cost::raise_budget(&mut run, None, Some(2000)).unwrap();
    assert_eq!(run.status, Status::Pending);
}

/// A Codex-JSONL harness that reports `tokens` once, then waits `sleep`
/// seconds and writes a complete handoff (or writes it first with
/// `handoff_first`, exiting right after the usage line).
fn streamer(dir: &Path, tokens: u64, sleep: u64, handoff_first: bool) -> CustomAdapter {
    let script = dir.join(format!("streamer-{tokens}-{sleep}-{handoff_first}.py"));
    let handoff = r#"open(sys.argv[1], "w").write(json.dumps({"schema_version": 2, "summary": "done", "verdict": "complete"}))"#;
    let usage = format!(
        r#"print(json.dumps({{"type": "turn.completed", "usage": {{"input_tokens": {tokens}, "output_tokens": 0}}}}), flush=True)"#
    );
    let body = if handoff_first {
        format!("{handoff}\n{usage}\n")
    } else {
        format!("{usage}\ntime.sleep({sleep})\n{handoff}\n")
    };
    fs::write(&script, format!("import json, sys, time\n{body}")).unwrap();
    CustomAdapter {
        command: "python3".into(),
        args: vec![script.display().to_string(), "{handoff_file}".into()],
        output: Some(CustomOutput::CodexJsonl),
        ..CustomAdapter::default()
    }
}

fn token_cap(tokens: u64) -> Budget {
    Budget {
        max_tokens: Some(tokens),
        on_exceed: BudgetAction::Stop,
        ..Budget::default()
    }
}

fn streaming_stage(id: &str, role: Role, harness: &str) -> Stage {
    Stage {
        id: id.into(),
        name: id.into(),
        role,
        harness: harness.into(),
        model: "gpt-5.4".into(),
        billing: Some(Billing::Api),
        timeout_seconds: 60,
        ..Stage::default()
    }
}

#[test]
fn a_final_stage_over_the_cap_blocks_the_run_instead_of_completing_it() {
    init_test_config_dir();
    let dir = common::tempdir();
    let settings = Settings {
        adapters: BTreeMap::from([
            ("small".into(), streamer(dir.path(), 100, 0, true)),
            ("quick".into(), streamer(dir.path(), 1200, 0, true)),
        ]),
        ..Settings::default()
    };
    // The harness exits right after the usage line that crosses the cap, so
    // the live monitor never needs to signal it; sequential and graph runs.
    let last = streaming_stage("quick", Role::Implementation, "quick");
    let graph = vec![
        streaming_stage("pre", Role::Research, "small"),
        Stage {
            after: vec!["pre".into()],
            ..last.clone()
        },
    ];
    for stages in [vec![last.clone()], graph] {
        let wf = Workflow {
            budget: token_cap(1000),
            ..Workflow::new("QuickCap", stages)
        };
        let mut run = Run::create(dir.path(), &settings, &wf, "quick cap".into(), false).unwrap();
        assert_eq!(drive(&mut run), Action::Stopped, "{:?}", run.error);
        assert_eq!(run.status, Status::Blocked);
        assert!(cost::budget_blocked(&run), "{:?}", run.error);
        let state = run.stages.last().unwrap();
        assert_eq!(state.status, Status::Complete, "the stage keeps its result");
        assert!(state.attempts[0].usage.is_some());
        // Raising the cap completes the run without rerunning anything.
        cost::raise_budget(&mut run, None, Some(5000)).unwrap();
        assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);
        assert_eq!(run.stages.last().unwrap().attempts.len(), 1);
    }
}

#[test]
fn live_run_cap_counts_parallel_stages_together() {
    init_test_config_dir();
    let dir = common::tempdir();
    let settings = Settings {
        adapters: BTreeMap::from([("slow".into(), streamer(dir.path(), 600, 30, false))]),
        ..Settings::default()
    };
    let join = Stage {
        after: vec!["a".into(), "b".into()],
        ..streaming_stage("join", Role::Summary, "slow")
    };
    let wf = Workflow {
        budget: token_cap(1000),
        ..Workflow::new(
            "ParallelCap",
            vec![
                streaming_stage("a", Role::Research, "slow"),
                streaming_stage("b", Role::Planning, "slow"),
                join,
            ],
        )
    };
    let mut run = Run::create(dir.path(), &settings, &wf, "parallel cap".into(), false).unwrap();
    let started = Instant::now();
    assert_eq!(drive(&mut run), Action::Stopped, "{:?}", run.error);
    let elapsed = started.elapsed();
    assert!(cost::budget_blocked(&run), "{:?}", run.error);
    // 600 + 600 tokens cross the 1,000-token cap: both stop, neither sleeps out.
    assert!(elapsed < Duration::from_secs(20), "took {elapsed:?}");
    for state in &run.stages[..2] {
        assert_eq!(state.status, Status::Blocked, "{:?}", state.attempts);
        assert!(state.attempts[0].usage.is_some());
    }
    assert!(run.stages[2].attempts.is_empty(), "the join never starts");
}

#[test]
fn live_stage_cap_stops_fan_out_candidates() {
    init_test_config_dir();
    let dir = common::tempdir();
    init_git(dir.path());
    let settings = Settings {
        adapters: BTreeMap::from([("slow".into(), streamer(dir.path(), 600, 30, false))]),
        ..Settings::default()
    };
    let candidate = |id: &str| model_router::config::Candidate {
        id: id.into(),
        harness: "slow".into(),
        model: "gpt-5.4".into(),
        ..Default::default()
    };
    let fan = Stage {
        budget: token_cap(1000),
        fan_out: Some(model_router::config::FanOut {
            candidates: vec![candidate("c1"), candidate("c2")],
            judge: "auto".into(),
        }),
        ..streaming_stage("impl", Role::Implementation, "slow")
    };
    let wf = Workflow::new("FanOutCap", vec![fan]);
    let mut run = Run::create(dir.path(), &settings, &wf, "fan-out cap".into(), false).unwrap();
    let started = Instant::now();
    assert_eq!(drive(&mut run), Action::Stopped, "{:?}", run.error);
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(20), "took {elapsed:?}");
    assert!(cost::budget_blocked(&run), "{:?}", run.error);
    assert_eq!(run.stages[0].status, Status::Blocked);
    let fan_state = run.stages[0].fan_out.as_ref().unwrap();
    assert!(fan_state.selection.is_none(), "no winner past the cap");
    for c in &fan_state.candidates {
        assert_eq!(c.status, Status::Blocked, "{:?}", c.error);
        assert!(c.error.as_deref().unwrap().starts_with(cost::EXCEEDED));
        assert!(c.attempts[0].usage.is_some());
    }
}

#[test]
fn fanout_candidate_diff_policy_blocks_offending_candidate() {
    init_test_config_dir();
    let dir = common::tempdir();
    let scripts = common::tempdir();
    init_git(dir.path());

    // 1. Fan-out candidate that writes a blocked pattern is stopped as Blocked,
    // while the clean candidate wins auto-judgement.
    let cand_script = scripts.path().join("cand.py");
    fs::write(
        &cand_script,
        r#"import json, pathlib, sys
mode = sys.argv[1]
handoff_path = pathlib.Path(sys.argv[2])
if mode == "bad":
    pathlib.Path("app.rs").write_text("fn main() { BLOCKED_API(); }\n")
else:
    pathlib.Path("app.rs").write_text("fn main() { safe_api(); }\n")
handoff_path.write_text(json.dumps({
    "schema_version": 2,
    "summary": f"candidate {mode}",
    "verdict": "complete",
    "findings": [],
    "tests": [],
    "files_changed": ["app.rs"],
    "unresolved": []
}))
"#,
    )
    .unwrap();
    let cand_adapter = |mode: &str| CustomAdapter {
        command: "python3".into(),
        args: vec![
            cand_script.display().to_string(),
            mode.into(),
            "{handoff_file}".into(),
        ],
        ..CustomAdapter::default()
    };
    let settings = Settings {
        adapters: BTreeMap::from([
            ("bad_cand".into(), cand_adapter("bad")),
            ("good_cand".into(), cand_adapter("good")),
        ]),
        ..Settings::default()
    };
    let mut fan_stage = Stage {
        id: "impl".into(),
        name: "Implement".into(),
        role: Role::Implementation,
        harness: "good_cand".into(),
        fan_out: Some(model_router::config::FanOut {
            candidates: vec![
                model_router::config::Candidate {
                    id: "c_bad".into(),
                    harness: "bad_cand".into(),
                    ..Default::default()
                },
                model_router::config::Candidate {
                    id: "c_good".into(),
                    harness: "good_cand".into(),
                    ..Default::default()
                },
            ],
            judge: "auto".into(),
        }),
        ..Stage::default()
    };
    let mut wf = Workflow::new("FanOutPolicy", vec![fan_stage.clone()]);
    wf.policies.push(DiffPolicy {
        name: "no-blocked-api".into(),
        action: PolicyAction::Block,
        pattern: "BLOCKED_API".into(),
        paths: vec!["*.rs".into()],
        message: "BLOCKED_API is not permitted".into(),
    });
    let mut run = Run::create(dir.path(), &settings, &wf, "fanout policy".into(), false).unwrap();
    assert_eq!(drive(&mut run), Action::Complete);
    let fan_state = run.stages[0].fan_out.as_ref().unwrap();
    assert_eq!(fan_state.candidates[0].status, Status::Blocked);
    assert!(
        fan_state.candidates[0]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("no-blocked-api")
    );
    assert_eq!(fan_state.candidates[1].status, Status::Complete);
    assert_eq!(fan_state.selection.as_ref().unwrap().candidate, "c_good");
    fan_stage.fan_out = None;
}

#[test]
fn fanout_candidate_diff_policy_reads_binary_files_and_non_ascii_paths() {
    init_test_config_dir();
    let dir = common::tempdir();
    let scripts = common::tempdir();
    init_git(dir.path());

    // Candidates' banned text sits in a file Git calls binary and in a file
    // whose path Git quotes; both must stop blocked.
    let cand_script = scripts.path().join("cand.py");
    fs::write(
        &cand_script,
        r#"import json, pathlib, sys
mode = sys.argv[1]
handoff_path = pathlib.Path(sys.argv[2])
src = pathlib.Path("src")
src.mkdir(exist_ok=True)
if mode == "binary":
    (src / "blob.bin").write_bytes(b"\x00\x01BLOCKED_API\x00\n")
elif mode == "path":
    (src / "café.rs").write_text("fn main() { BLOCKED_API(); }\n")
else:
    (src / "app.rs").write_text("fn main() { safe_api(); }\n")
handoff_path.write_text(json.dumps({
    "schema_version": 2,
    "summary": f"candidate {mode}",
    "verdict": "complete",
    "findings": [],
    "tests": [],
    "files_changed": [],
    "unresolved": []
}))
"#,
    )
    .unwrap();
    let cand_adapter = |mode: &str| CustomAdapter {
        command: "python3".into(),
        args: vec![
            cand_script.display().to_string(),
            mode.into(),
            "{handoff_file}".into(),
        ],
        ..CustomAdapter::default()
    };
    let settings = Settings {
        adapters: BTreeMap::from([
            ("binary_cand".into(), cand_adapter("binary")),
            ("path_cand".into(), cand_adapter("path")),
            ("good_cand".into(), cand_adapter("good")),
        ]),
        ..Settings::default()
    };
    let candidate = |id: &str, harness: &str| config::Candidate {
        id: id.into(),
        harness: harness.into(),
        ..Default::default()
    };
    let fan_stage = Stage {
        id: "impl".into(),
        name: "Implement".into(),
        role: Role::Implementation,
        harness: "good_cand".into(),
        fan_out: Some(config::FanOut {
            candidates: vec![
                candidate("c_binary", "binary_cand"),
                candidate("c_path", "path_cand"),
                candidate("c_good", "good_cand"),
            ],
            judge: "auto".into(),
        }),
        ..Stage::default()
    };
    let mut wf = Workflow::new("FanOutPolicyText", vec![fan_stage]);
    wf.policies.push(DiffPolicy {
        name: "no-blocked-api".into(),
        action: PolicyAction::Block,
        pattern: "BLOCKED_API".into(),
        paths: vec!["src/**".into()],
        message: "BLOCKED_API is not permitted".into(),
    });
    let mut run = Run::create(dir.path(), &settings, &wf, "fanout text".into(), false).unwrap();
    assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);
    let fan_state = run.stages[0].fan_out.as_ref().unwrap();
    for (candidate, file) in fan_state.candidates[..2]
        .iter()
        .zip(["blob.bin", "café.rs"])
    {
        let error = candidate.error.as_deref().unwrap_or("");
        assert_eq!(
            candidate.status,
            Status::Blocked,
            "{}: {error}",
            candidate.id
        );
        assert!(
            error.contains("no-blocked-api") && error.contains(&format!("src/{file}")),
            "{error}"
        );
    }
    assert_eq!(fan_state.candidates[2].status, Status::Complete);
    assert_eq!(fan_state.selection.as_ref().unwrap().candidate, "c_good");
}

// -----------------------------------------------------------------------------
// 6. Retries, hooks, and run metrics across stage kinds
// -----------------------------------------------------------------------------

/// The `run_end` metric statuses the run's saved commits produced, replayed
/// from transitions.jsonl one commit (run revision) at a time.
fn run_ends(run: &Run) -> Vec<String> {
    let records = transitions::read(&run.directory).unwrap();
    let mut ends = vec![];
    let mut start = 0;
    while start < records.len() {
        let revision = records[start].run_revision;
        let end = records[start..]
            .iter()
            .position(|r| r.run_revision != revision)
            .map_or(records.len(), |n| start + n);
        ends.extend(
            events::collect_run_metrics(run, false, &records[start..end])
                .into_iter()
                .filter(|m| m.event == "run_end")
                .map(|m| m.status),
        );
        start = end;
    }
    ends
}

fn retry(base_delay_seconds: u64) -> Retry {
    Retry {
        max_attempts: 2,
        base_delay_seconds,
        max_delay_seconds: base_delay_seconds.max(1),
    }
}

/// Two parallel research stages, then a planning stage after both.
fn graph_retry_workflow(r1_model: &str, r2_model: &str, base_delay_seconds: u64) -> Workflow {
    let mut r1 = stage("r1", Role::Research);
    r1.model = r1_model.into();
    let mut r2 = stage("r2", Role::Research);
    r2.model = r2_model.into();
    let mut plan = stage("plan", Role::Planning);
    plan.after = vec!["r1".into(), "r2".into()];
    let mut wf = Workflow::new("GraphRetry", vec![r1, r2, plan]);
    wf.retry = retry(base_delay_seconds);
    wf
}

#[test]
fn graph_stage_retries_restart_cancel_during_backoff_and_yield_to_a_failed_sibling() {
    init_test_config_dir();

    // 1. A transient failure restarts the stage after its backoff; the run
    // ends once.
    let dir = common::tempdir();
    let wf = graph_retry_workflow("transient-once", "test-model", 1);
    let settings = common::settings(&wf);
    let mut run = Run::create(dir.path(), &settings, &wf, "graph retry".into(), false).unwrap();
    assert!(run.graph());
    let mut output = String::new();
    let action = engine::drive(&mut run, &AtomicBool::new(false), |ev| {
        if let Event::Output(text) = ev {
            output.push_str(&text);
        }
    })
    .unwrap();
    assert_eq!(action, Action::Complete, "{:?} {output}", run.error);
    let statuses: Vec<Status> = run.stages[0].attempts.iter().map(|a| a.status).collect();
    assert_eq!(statuses, [Status::Failed, Status::Complete]);
    assert_eq!(run.stages[1].attempts.len(), 1);
    assert_eq!(run.stages[2].status, Status::Complete);
    assert!(output.contains("retrying 1/2 in 1s"), "{output}");
    let records = transitions::read(&run.directory).unwrap();
    assert!(
        records
            .iter()
            .any(|r| r.reason.contains("Transient error retry 1/2")),
        "{records:?}"
    );
    assert_eq!(run_ends(&run), ["complete"]);

    // 2. Cancelling during the backoff stops at once, without the retry.
    let dir = common::tempdir();
    let wf = graph_retry_workflow("transient", "test-model", 30);
    let settings = common::settings(&wf);
    let mut run = Run::create(dir.path(), &settings, &wf, "graph cancel".into(), false).unwrap();
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let action = engine::drive(&mut run, &cancel, |ev| {
        if let Event::Output(text) = ev {
            if text.contains("Transient failure") {
                cancel.store(true, Ordering::Relaxed);
            }
        }
    })
    .unwrap();
    assert_eq!(action, Action::Stopped);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(run.status, Status::Cancelled);
    assert_eq!(run.stages[0].status, Status::Cancelled);
    assert_eq!(run.stages[0].attempts.len(), 1);
    assert_eq!(run.stages[2].attempts.len(), 0);
    assert_eq!(run_ends(&run), ["cancelled"]);

    // 3. A sibling's permanent failure ends the backoff early: the run stops
    // at the sibling and the retrying stage waits, never retried.
    let dir = common::tempdir();
    let wf = graph_retry_workflow("transient", "fail", 30);
    let settings = common::settings(&wf);
    let mut run = Run::create(dir.path(), &settings, &wf, "graph sibling".into(), false).unwrap();
    let started = Instant::now();
    assert_eq!(drive(&mut run), Action::Stopped);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the backoff should end when the sibling stops the run"
    );
    assert_eq!(run.status, Status::Failed);
    assert_eq!(run.cursor, 1);
    assert!(
        run.error
            .as_deref()
            .unwrap_or("")
            .contains("Simulated authentication"),
        "{:?}",
        run.error
    );
    assert_eq!(run.stages[0].status, Status::Pending);
    assert_eq!(run.stages[0].attempts.len(), 1);
    assert_eq!(run.stages[1].status, Status::Failed);
    assert_eq!(run_ends(&run), ["failed"]);
}

#[test]
fn list_stage_retry_ends_the_run_once_in_metrics() {
    init_test_config_dir();
    let dir = common::tempdir();
    let mut s = stage("impl", Role::Implementation);
    s.model = "transient-once".into();
    s.retry = Some(retry(1));
    let wf = Workflow::new("ListRetry", vec![s]);
    let settings = common::settings(&wf);
    let mut run = Run::create(dir.path(), &settings, &wf, "list retry".into(), false).unwrap();
    assert_eq!(drive(&mut run), Action::Complete);
    assert_eq!(run.stages[0].attempts.len(), 2);
    // The failed attempt still ends, as an attempt.
    let records = transitions::read(&run.directory).unwrap();
    let attempt_ends: Vec<String> = events::collect_run_metrics(&run, false, &records)
        .into_iter()
        .filter(|m| m.event == "attempt_end")
        .map(|m| m.status)
        .collect();
    assert_eq!(attempt_ends, ["failed", "complete"]);
    assert_eq!(run_ends(&run), ["complete"]);
}

/// A script that writes a blocked call on its first run, then fails with a
/// provider error; later runs only hand off.
fn blocked_then_transient(dir: &Path) -> CustomAdapter {
    let script = dir.join("blocked_then_transient.py");
    fs::write(
        &script,
        r#"import json, pathlib, sys
count_file = pathlib.Path(sys.argv[1])
handoff_path = pathlib.Path(sys.argv[2])
n = int(count_file.read_text()) + 1 if count_file.exists() else 1
count_file.write_text(str(n))
if n == 1:
    pathlib.Path("app.rs").write_text("fn main() { BLOCKED_API(); }\n")
    sys.stderr.write("HTTP 503 Service Unavailable\n")
    sys.exit(1)
handoff_path.write_text(json.dumps({"schema_version": 2, "summary": "done", "verdict": "complete"}))
"#,
    )
    .unwrap();
    CustomAdapter {
        command: "python3".into(),
        args: vec![
            script.display().to_string(),
            dir.join("count").display().to_string(),
            "{handoff_file}".into(),
        ],
        ..CustomAdapter::default()
    }
}

#[test]
fn edits_a_failed_attempt_left_before_its_automatic_retry_are_policy_checked() {
    init_test_config_dir();
    for graph in [false, true] {
        let dir = common::tempdir();
        let scripts = common::tempdir();
        init_git(dir.path());
        let mut s = stage("impl", Role::Implementation);
        s.harness = "flaky".into();
        s.retry = Some(retry(1));
        let mut stages = vec![s];
        if graph {
            let mut after = stage("check", Role::Research);
            after.after = vec!["impl".into()];
            stages.push(after);
        }
        let mut wf = Workflow::new("RetryPolicy", stages);
        wf.policies.push(DiffPolicy {
            name: "no-blocked-api".into(),
            action: PolicyAction::Block,
            pattern: "BLOCKED_API".into(),
            paths: vec!["*.rs".into()],
            message: "BLOCKED_API is not permitted".into(),
        });
        let mut settings = common::settings(&wf);
        settings
            .adapters
            .insert("flaky".into(), blocked_then_transient(scripts.path()));
        let mut run =
            Run::create(dir.path(), &settings, &wf, "retry policy".into(), false).unwrap();
        assert_eq!(drive(&mut run), Action::Stopped, "graph {graph}");
        assert_eq!(run.stages[0].attempts.len(), 2, "graph {graph}");
        assert_eq!(run.stages[0].status, Status::Blocked, "graph {graph}");
        assert_eq!(run.status, Status::Blocked, "graph {graph}");
        assert!(
            run.error
                .as_deref()
                .unwrap_or("")
                .contains("no-blocked-api"),
            "graph {graph}: {:?}",
            run.error
        );
    }
}

#[test]
fn after_stage_hook_edits_are_part_of_the_recorded_changes() {
    init_test_config_dir();
    let dir = common::tempdir();
    init_git(dir.path());
    let mut s = stage("impl", Role::Implementation);
    s.hooks.after_stage.push(Hook {
        command: vec![
            "python3".into(),
            "-c".into(),
            "import pathlib; pathlib.Path('hooked.txt').write_text('from the hook\\n')".into(),
        ],
        timeout_seconds: 10,
    });
    let wf = Workflow::new("HookChanges", vec![s]);
    let settings = common::settings(&wf);
    let mut run = Run::create(dir.path(), &settings, &wf, "hook changes".into(), false).unwrap();
    assert_eq!(drive(&mut run), Action::Complete);
    let attempt = &run.stages[0].attempts[0];
    assert_ne!(attempt.tree_after, attempt.tree_before);
    let diff = fs::read_to_string(run.directory.join("changes.diff")).unwrap();
    assert!(diff.contains("hooked.txt"), "{diff}");
    assert!(diff.contains("from the hook"), "{diff}");
}

/// A fan-out stage of two mock candidates whose counters survive their
/// worktrees, so `transient-once` fails only a candidate's first attempt.
fn fan_out_fixture(state: &Path, first_model: &str, hooks: Hooks) -> (Settings, Workflow) {
    let candidate = |id: &str, model: &str| model_router::config::Candidate {
        id: id.into(),
        harness: "mockstate".into(),
        model: model.into(),
        provider: id.into(),
        ..Default::default()
    };
    let mut fan = stage("impl", Role::Implementation);
    fan.harness = "mockstate".into();
    fan.retry = Some(retry(1));
    fan.hooks = hooks;
    fan.fan_out = Some(model_router::config::FanOut {
        candidates: vec![candidate("c1", first_model), candidate("c2", "test-model")],
        judge: "auto".into(),
    });
    let wf = Workflow::new("FanOutRetry", vec![fan]);
    let mut settings = common::settings(&wf);
    let mut adapter = settings.adapters["mock"].clone();
    adapter
        .args
        .extend(["--state-dir".into(), state.display().to_string()]);
    settings.adapters.insert("mockstate".into(), adapter);
    (settings, wf)
}

fn log_hook(log: &Path, label: &str) -> Hook {
    Hook {
        command: vec![
            "python3".into(),
            "-c".into(),
            format!(
                "import pathlib; p = pathlib.Path({:?}); p.write_text((p.read_text() if p.exists() else '') + {label:?} + '\\n')",
                log.display().to_string()
            ),
        ],
        timeout_seconds: 10,
    }
}

#[test]
fn fan_out_rounds_run_stage_hooks_once_and_retry_candidates_transiently() {
    init_test_config_dir();

    // 1. Hooks wrap the round once; a candidate's transient failure reruns
    // only that candidate.
    let dir = common::tempdir();
    let scripts = common::tempdir();
    init_git(dir.path());
    let log = scripts.path().join("hooks.log");
    let hooks = Hooks {
        before_stage: vec![log_hook(&log, "before")],
        after_stage: vec![log_hook(&log, "after")],
        on_failure: vec![log_hook(&log, "failure")],
        ..Hooks::default()
    };
    let (settings, wf) = fan_out_fixture(scripts.path(), "transient-once", hooks);
    let mut run = Run::create(dir.path(), &settings, &wf, "fan-out retry".into(), false).unwrap();
    assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);
    let fan = run.stages[0].fan_out.as_ref().unwrap();
    let c1: Vec<Status> = fan.candidates[0]
        .attempts
        .iter()
        .map(|a| a.status)
        .collect();
    assert_eq!(c1, [Status::Failed, Status::Complete]);
    assert_eq!(fan.candidates[0].status, Status::Complete);
    assert_eq!(fan.candidates[1].attempts.len(), 1);
    assert_eq!(run.stages[0].attempts.len(), 1, "one round");
    assert_eq!(fs::read_to_string(&log).unwrap(), "before\nafter\n");

    // 2. A failing before_stage hook stops the round before any candidate
    // starts, and runs on_failure.
    let dir = common::tempdir();
    let scripts = common::tempdir();
    init_git(dir.path());
    let log = scripts.path().join("hooks.log");
    let hooks = Hooks {
        before_stage: vec![Hook {
            command: vec![
                "python3".into(),
                "-c".into(),
                "import sys; sys.exit(2)".into(),
            ],
            timeout_seconds: 10,
        }],
        after_stage: vec![log_hook(&log, "after")],
        on_failure: vec![log_hook(&log, "failure")],
        ..Hooks::default()
    };
    let (settings, wf) = fan_out_fixture(scripts.path(), "test-model", hooks);
    let mut run = Run::create(dir.path(), &settings, &wf, "fan-out hook".into(), false).unwrap();
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!(run.status, Status::Failed);
    assert_eq!(run.stages[0].status, Status::Failed);
    let fan = run.stages[0].fan_out.as_ref().unwrap();
    for candidate in &fan.candidates {
        assert_eq!(candidate.status, Status::Cancelled);
        assert!(candidate.attempts.is_empty(), "no candidate may start");
    }
    assert_eq!(fs::read_to_string(&log).unwrap(), "failure\n");
}

#[test]
fn fan_out_candidates_start_from_files_before_stage_hooks_prepared() {
    init_test_config_dir();
    let dir = common::tempdir();
    let scripts = common::tempdir();
    init_git(dir.path());
    let hooks = Hooks {
        before_stage: vec![Hook {
            command: vec![
                "python3".into(),
                "-c".into(),
                "import pathlib; pathlib.Path('prepared.txt').write_text('ready\\n')".into(),
            ],
            timeout_seconds: 10,
        }],
        ..Hooks::default()
    };
    let (settings, wf) = fan_out_fixture(scripts.path(), "test-model", hooks);
    let mut run = Run::create(dir.path(), &settings, &wf, "fan-out prep".into(), false).unwrap();
    assert_eq!(drive(&mut run), Action::Complete, "{:?}", run.error);
    let fan = run.stages[0].fan_out.as_ref().unwrap();
    // The candidates' base (and so their worktrees) holds the hook's file.
    let listed = git(dir.path(), &["ls-tree", "--name-only", &fan.base_commit]);
    assert!(listed.lines().any(|l| l == "prepared.txt"), "{listed}");
    for candidate in &fan.candidates {
        assert_eq!(
            candidate.attempts[0].tree_before.as_deref(),
            Some(fan.base.as_str())
        );
    }
    assert_eq!(
        fs::read_to_string(dir.path().join("prepared.txt")).unwrap(),
        "ready\n"
    );
}
