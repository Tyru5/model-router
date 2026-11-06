//! Budget gates with a fake `claude` that reports usage like Claude Code's
//! stream-json result event. No network, no credentials, no inference.
//! This binary puts the fake first on PATH, so its tests share that setup.
use model_router::{
    config::{Budget, Role, Settings, Stage, Workflow},
    cost,
    engine::{self, Action, Run, Status},
    report,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{OnceLock, atomic::AtomicBool},
};

/// Prints a result whose cost and tokens come from its `--model`:
/// "cost-2.5" reports $2.50; "tokens-600" reports 600 input tokens, no cost;
/// any other model reports 1M input and 100k output tokens, no cost.
const FAKE_CLAUDE: &str = r#"#!/usr/bin/env python3
import json, sys
from pathlib import Path
args = sys.argv[1:]
model = args[args.index("--model") + 1] if "--model" in args else "default"
sys.stdin.read()
log = Path(".model-router/fake-claude.log")
log.parent.mkdir(parents=True, exist_ok=True)
with log.open("a") as f:
    f.write(model + "\n")
usage = {"input_tokens": 1000000, "output_tokens": 100000}
result = {"type": "result", "subtype": "success", "session_id": "s"}
if model.startswith("cost-"):
    result["total_cost_usd"] = float(model[5:])
    usage = {"input_tokens": 10, "output_tokens": 10}
elif model.startswith("tokens-"):
    usage = {"input_tokens": int(model[7:]), "output_tokens": 0}
handoff = {"summary": "fake claude stage on " + model, "verdict": "complete"}
result["usage"] = usage
result["result"] = "<router-handoff>" + json.dumps(handoff) + "</router-handoff>"
print(json.dumps({"type": "system", "subtype": "init", "session_id": "s"}))
print(json.dumps(result), flush=True)
"#;

fn setup() {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        let fake = dir.join("claude");
        fs::write(&fake, FAKE_CLAUDE).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![dir.clone()];
        paths.extend(std::env::split_paths(&path));
        let config = dir.join("config");
        fs::create_dir_all(&config).unwrap();
        // SAFETY: runs once, before any test in this binary reads the
        // environment or spawns a process; the others wait on this OnceLock.
        unsafe {
            std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
            // No user pricing overrides leak into the estimates.
            std::env::set_var("MODEL_ROUTER_CONFIG_DIR", config);
        }
        dir
    });
}

fn stage(id: &str, model: &str) -> Stage {
    Stage {
        id: id.into(),
        name: id.into(),
        role: Role::Implementation,
        harness: "claude".into(),
        model: model.into(),
        ..Stage::default()
    }
}

fn create(stages: Vec<Stage>, budget: Budget) -> (tempfile::TempDir, Run) {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let workflow = Workflow {
        max_parallel: 2,
        name: "Budget".into(),
        max_repair_passes: 0,
        sandbox_default: Default::default(),
        stages,
        budget,
    };
    let settings = Settings {
        workflows: vec![workflow.clone()],
        ..Settings::default()
    };
    let run = Run::create(dir.path(), &settings, &workflow, "Task".into(), false).unwrap();
    (dir, run)
}

fn drive(run: &mut Run) -> Action {
    engine::drive(run, &AtomicBool::new(false), |_| {}).unwrap()
}

fn launches(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join(".model-router/fake-claude.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn reported_cost_over_the_run_cap_blocks_before_the_next_stage() {
    let budget = Budget {
        max_usd: Some(3.0),
        max_tokens: None,
    };
    let stages = vec![
        stage("a", "cost-2"),
        stage("b", "cost-2"),
        stage("c", "cost-2"),
    ];
    let (dir, mut run) = create(stages, budget);
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!(run.status, Status::Blocked);
    let error = run.error.clone().unwrap();
    assert!(
        error.starts_with("Budget exceeded: $4.00 of $3.00 (reported)"),
        "{error}"
    );
    assert!(error.contains("--budget-usd"), "{error}");
    // Both completed stages keep their results; the third never launched.
    assert_eq!(launches(dir.path()), ["cost-2", "cost-2"]);
    assert_eq!(run.stages[1].status, Status::Complete);
    assert_eq!(run.stages[2].status, Status::Blocked);
    assert!(run.stages[2].attempts.is_empty());
    let saved = engine::load(dir.path(), &run.id).unwrap();
    assert!(cost::budget_blocked(&saved));
    let built = report::build(&saved);
    assert_eq!(built.usage.cost.text(), "$4.00 (reported)");
    assert!(report::render(&built, None).contains("Cost: $4.00 (reported) · budget $3.00"));

    // Raising the cap continues where it stopped.
    let mut run = saved;
    cost::raise_budget(&mut run, Some(10.0), None).unwrap();
    assert_eq!(drive(&mut run), Action::Complete);
    assert_eq!(launches(dir.path()).len(), 3);
    assert_eq!(run.workflow.budget.max_usd, Some(10.0));
}

#[test]
fn unknown_costs_count_only_toward_the_token_cap() {
    let budget = Budget {
        max_usd: Some(0.01),
        max_tokens: Some(1_000),
    };
    let stages = vec![
        stage("a", "tokens-600"),
        stage("b", "tokens-600"),
        stage("c", "tokens-600"),
    ];
    let (dir, mut run) = create(stages, budget);
    assert_eq!(drive(&mut run), Action::Stopped);
    let error = run.error.clone().unwrap();
    assert!(
        error.starts_with("Budget exceeded: 1.2k of 1.0k tokens"),
        "{error}"
    );
    assert_eq!(launches(dir.path()).len(), 2);
    let total = report::build(&run).usage.cost;
    assert_eq!((total.attempts, total.unknown), (2, 2));
    assert_eq!(total.text(), "cost unknown");
    // Raising the wrong cap does not help; the gate checks again.
    cost::raise_budget(&mut run, Some(5.0), None).unwrap();
    assert_eq!(drive(&mut run), Action::Stopped);
    assert_eq!(launches(dir.path()).len(), 2);
    cost::raise_budget(&mut run, None, Some(10_000)).unwrap();
    assert_eq!(drive(&mut run), Action::Complete);
}

#[test]
fn stage_caps_and_table_prices_gate_too() {
    // 1M in + 100k out on Haiku 5.5 from the table: $0.10 + $0.05.
    let mut first = stage("a", "claude-haiku-5-5");
    first.budget = Budget {
        max_usd: Some(0.1),
        max_tokens: None,
    };
    let (dir, mut run) = create(vec![first, stage("b", "cost-1")], Budget::default());
    assert_eq!(drive(&mut run), Action::Stopped);
    let error = run.error.clone().unwrap();
    assert!(
        error.starts_with("Budget exceeded: ~$0.15 of $0.10 (table) in stage a"),
        "{error}"
    );
    assert_eq!(launches(dir.path()), ["claude-haiku-5-5"]);
    // Raising lifts the exceeded stage cap with the run's.
    cost::raise_budget(&mut run, Some(1.0), None).unwrap();
    assert_eq!(run.workflow.stages[0].budget.max_usd, Some(1.0));
    assert_eq!(drive(&mut run), Action::Complete);
    let text = report::render(&report::build(&run), None);
    assert!(
        text.contains(
            "(reported $1.00 + table $0.15) · budget $1.00 · table prices are estimates as of"
        ),
        "{text}"
    );
    let summary = cost::aggregate([&run], None, &cost::Pricing::load(dir.path()));
    assert_eq!(summary.rows.len(), 2);
    assert_eq!(
        summary.total.text(),
        "~$1.15 (reported $1.00 + table $0.15)"
    );
}

#[test]
fn runs_without_a_budget_are_never_gated() {
    let (dir, mut run) = create(
        vec![stage("a", "cost-50"), stage("b", "cost-50")],
        Budget::default(),
    );
    assert_eq!(drive(&mut run), Action::Complete);
    assert_eq!(launches(dir.path()).len(), 2);
    assert!(cost::raise_budget(&mut run, None, None).is_err());
    assert!(cost::raise_budget(&mut run, Some(-1.0), None).is_err());
}

#[test]
fn cli_resumes_with_a_raised_budget_and_aggregates_history() {
    let budget = Budget {
        max_usd: Some(1.0),
        max_tokens: None,
    };
    let stages = vec![
        stage("a", "cost-2"),
        stage("b", "cost-1"),
        stage("c", "cost-1"),
    ];
    let (dir, mut run) = create(stages, budget);
    assert_eq!(drive(&mut run), Action::Stopped);
    let cli = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_model-router"))
            .arg("-C")
            .arg(dir.path())
            .args(args)
            .output()
            .unwrap()
    };
    let resumed = cli(&["resume", &run.id, "--budget-usd", "2.5"]);
    assert!(
        !resumed.status.success(),
        "b runs, then $3 > $2.50 stops c: {}",
        String::from_utf8_lossy(&resumed.stdout)
    );
    let saved = engine::load(dir.path(), &run.id).unwrap();
    assert_eq!(saved.workflow.budget.max_usd, Some(2.5));
    assert!(cost::budget_blocked(&saved), "{:?}", saved.error);
    assert!(
        cli(&["resume", &run.id, "--budget-usd", "5"])
            .status
            .success()
    );
    assert_eq!(
        engine::load(dir.path(), &run.id).unwrap().status,
        Status::Complete
    );
    let out = cli(&["cost", "--json", "--since", "1h"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["total"]["reported"]["usd"], 4.0);
    assert_eq!(json["rows"].as_array().unwrap().len(), 2);
    assert_eq!(json["rows"][0]["harness"], "claude");
    let text = String::from_utf8_lossy(&cli(&["cost"]).stdout).into_owned();
    assert!(text.contains("Total: $4.00 (reported)"), "{text}");
}
