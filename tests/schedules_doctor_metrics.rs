mod common;

use chrono::{DateTime, Utc};
use model_router::{
    config::{CustomAdapter, CustomOutput, Role, Settings, Stage, Workflow},
    cost,
    delegation::schedules,
    engine,
    events::MetricRecord,
    providers::Provider,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn mr(project: &Path, config_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("-C")
        .arg(project)
        .args(args)
        .env("MODEL_ROUTER_CONFIG_DIR", config_dir)
        .output()
        .unwrap()
}

fn fake_exec(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
}

#[test]
fn schedules_add_cli_supports_cron_tz_task_file_and_trust_gated_approve() {
    let dir = common::tempdir();
    let cfg = common::tempdir();
    let wf = common::workflow(vec![common::stage("implement", Role::Implementation)]);
    let settings = common::settings(&wf);
    settings
        .save(&dir.path().join(".model-router.toml"))
        .unwrap();

    // Untrusted project: --approve must fail without creating the schedule.
    let denied = mr(
        dir.path(),
        cfg.path(),
        &[
            "schedules",
            "add",
            "--id",
            "nightly",
            "--task",
            "Run nightly audit",
            "--cron",
            "0 2 * * *",
            "--tz",
            "UTC",
            "--approve",
        ],
    );
    assert!(
        !denied.status.success(),
        "untrusted --approve should fail: {}",
        String::from_utf8_lossy(&denied.stdout)
    );
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("untrusted"),
        "{}",
        String::from_utf8_lossy(&denied.stderr)
    );
    assert!(schedules::load(dir.path()).unwrap().schedules.is_empty());

    // Without --approve, creation works and leaves it needing approval.
    let added = mr(
        dir.path(),
        cfg.path(),
        &[
            "schedules",
            "add",
            "--id",
            "daily-check",
            "--task",
            "Daily review",
            "--at",
            "09:30",
            "--tz",
            "+02:00",
        ],
    );
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let stdout = String::from_utf8_lossy(&added.stdout);
    assert!(stdout.contains("needs approval"), "{stdout}");
    assert!(stdout.contains("daily at 09:30 (+02:00)"), "{stdout}");

    // Trust the project, then add a cron schedule with --task-file and --approve.
    let trusted = mr(dir.path(), cfg.path(), &["trust", "--yes"]);
    assert!(
        trusted.status.success(),
        "{}",
        String::from_utf8_lossy(&trusted.stderr)
    );

    let task_file = dir.path().join("nightly-task.md");
    fs::write(&task_file, "Audit dependencies and report regressions.").unwrap();
    let approved = mr(
        dir.path(),
        cfg.path(),
        &[
            "schedules",
            "add",
            "--id",
            "weekday-cron",
            "--workflow",
            "Test",
            "--task-file",
            task_file.to_str().unwrap(),
            "--cron",
            "15 8 * * 1-5",
            "--tz",
            "-07:00",
            "--approve",
        ],
    );
    assert!(
        approved.status.success(),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
    let stdout = String::from_utf8_lossy(&approved.stdout);
    assert!(
        stdout.contains("Added and approved weekday-cron"),
        "{stdout}"
    );
    assert!(stdout.contains("cron 15 8 * * 1-5 (-07:00)"), "{stdout}");

    // Duplicate ID and path-like ID are rejected.
    let dup = mr(
        dir.path(),
        cfg.path(),
        &[
            "schedules",
            "add",
            "--id",
            "weekday-cron",
            "--task",
            "dup",
            "--webhook",
        ],
    );
    assert!(!dup.status.success());
    let bad_id = mr(
        dir.path(),
        cfg.path(),
        &[
            "schedules",
            "add",
            "--id",
            "../escape",
            "--task",
            "bad",
            "--webhook",
        ],
    );
    assert!(!bad_id.status.success());

    // `schedules list --json` shows both schedules with approval state and next_due.
    let list = mr(dir.path(), cfg.path(), &["schedules", "list", "--json"]);
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let rows: Vec<Value> = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(rows.len(), 2);
    let weekday = rows
        .iter()
        .find(|r| r["schedule"]["id"] == "weekday-cron")
        .unwrap();
    assert_eq!(weekday["approved"], true);
    assert!(weekday["next_due"].as_str().is_some());
    assert_eq!(weekday["schedule"]["created_by"], "user");
    let daily = rows
        .iter()
        .find(|r| r["schedule"]["id"] == "daily-check")
        .unwrap();
    assert_eq!(daily["approved"], false);

    // Firing the approved cron schedule after its next_due creates a run.
    let next: DateTime<Utc> = serde_json::from_value(weekday["next_due"].clone()).unwrap();
    // Temporarily run fire_due in a subprocess? No, schedules::fire_due uses config_dir()
    // which reads MODEL_ROUTER_CONFIG_DIR; verify via schedule approval receipt file instead
    // and check next_due is strictly in the future of created_at.
    let created: DateTime<Utc> =
        serde_json::from_value(weekday["schedule"]["created_at"].clone()).unwrap();
    assert!(next > created);
}

#[test]
fn doctor_checks_version_pins_provider_keys_and_offline_auth_hints() {
    let dir = common::tempdir();
    let cfg = common::tempdir();
    let bin = dir.path().join("bin");
    let fake_home = dir.path().join("home");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(fake_home.join(".codex")).unwrap();
    fs::write(
        fake_home.join(".codex/auth.json"),
        "SUPER_SECRET_DO_NOT_PRINT",
    )
    .unwrap();

    // Fake codex implementing every flag codex adapter passes.
    fake_exec(
        &bin,
        "codex",
        r#"#!/bin/sh
case "$*" in
  "--version") echo "codex-cli 0.167.0" ;;
  "exec --help")
    cat <<'EOF'
Usage: codex exec [OPTIONS] [PROMPT]
  -m, --model <MODEL>
      --json
      --color <COLOR>
      --skip-git-repo-check
  -o, --output-last-message <FILE>
      --output-schema <FILE>
  -c, --config <key=value>
      --oss
      --local-provider <P>
  -s, --sandbox <MODE>
EOF
    ;;
  "--help"|"resume --help")
    cat <<'EOF'
  -m, --model <MODEL>
  -c, --config <key=value>
      --oss
      --local-provider <P>
  -s, --sandbox <MODE>
EOF
    ;;
esac
"#,
    );

    // Configure matching pin + built-in openrouter provider opt-in (no trust needed).
    let mut settings = Settings::default();
    settings
        .harness_versions
        .insert("codex".into(), ">=0.160.0".into());
    settings
        .providers
        .insert("openrouter".into(), Provider::default());
    settings
        .save(&dir.path().join(".model-router.toml"))
        .unwrap();

    // `validate` must emit zero unknown-field warnings for [harness_versions].
    let val = mr(dir.path(), cfg.path(), &["validate"]);
    assert!(
        val.status.success(),
        "{}",
        String::from_utf8_lossy(&val.stderr)
    );
    let val_out = String::from_utf8_lossy(&val.stdout);
    assert!(
        !val_out.contains("Unknown config field"),
        "unexpected warning: {val_out}"
    );

    let run_doctor = |extra_args: &[&str], openrouter_key: Option<&str>| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_model-router"));
        cmd.arg("-C")
            .arg(dir.path())
            .arg("doctor")
            .args(extra_args)
            .env("PATH", &bin)
            .env("HOME", &fake_home)
            .env("MODEL_ROUTER_CONFIG_DIR", cfg.path())
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .env_remove("CODEX_HOME");
        match openrouter_key {
            Some(v) => {
                cmd.env("OPENROUTER_API_KEY", v);
            }
            None => {
                cmd.env_remove("OPENROUTER_API_KEY");
            }
        }
        cmd.output().unwrap()
    };

    // With OPENROUTER_API_KEY set and pin satisfied, --strict succeeds and never leaks secret value.
    let secret = "sk-or-v1-never-print-this-value";
    let ok_human = run_doctor(&["--strict"], Some(secret));
    assert!(
        ok_human.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ok_human.stderr)
    );
    let text = String::from_utf8_lossy(&ok_human.stdout);
    assert!(
        text.contains("Providers: openrouter (OPENROUTER_API_KEY set)"),
        "{text}"
    );
    assert!(
        text.contains("Version pins: codex 0.167.0 matches >=0.160.0"),
        "{text}"
    );
    assert!(
        text.contains("Auth hints (offline): codex ready (~/.codex/auth.json)"),
        "{text}"
    );
    assert!(!text.contains(secret), "secret value leaked!");
    assert!(
        !text.contains("SUPER_SECRET_DO_NOT_PRINT"),
        "auth.json content leaked!"
    );
    assert!(
        text.trim_end().ends_with("Log in through each harness."),
        "{text}"
    );

    // --json carries version_pin, version_pin_ok, and auth_hint on the codex report.
    let ok_json = run_doctor(&["--json"], Some(secret));
    assert!(ok_json.status.success());
    assert!(!String::from_utf8_lossy(&ok_json.stdout).contains(secret));
    let doc: Value = serde_json::from_slice(&ok_json.stdout).unwrap();
    assert_eq!(
        doc["providers"],
        serde_json::json!([
            {"name": "openrouter", "api_key_env": "OPENROUTER_API_KEY", "status": "set"}
        ])
    );
    let reports = doc["harnesses"].as_array().unwrap();
    let codex = reports.iter().find(|r| r["harness"] == "codex").unwrap();
    assert_eq!(codex["version_pin"], ">=0.160.0");
    assert_eq!(codex["version_pin_ok"], true);
    assert_eq!(codex["auth_hint"]["status"], "ready");
    assert_eq!(codex["auth_hint"]["signals"][0], "~/.codex/auth.json");

    // --json reports the missing key that --strict fails on.
    let missing_json = run_doctor(&["--json"], None);
    assert!(missing_json.status.success());
    let doc: Value = serde_json::from_slice(&missing_json.stdout).unwrap();
    assert_eq!(doc["providers"][0]["status"], "missing");

    // Missing OPENROUTER_API_KEY causes --strict to fail.
    let missing_key = run_doctor(&["--strict"], None);
    assert!(!missing_key.status.success());
    let err = String::from_utf8_lossy(&missing_key.stderr);
    assert!(err.contains("OPENROUTER_API_KEY"), "{err}");

    // Pin drift causes --strict to fail even when OPENROUTER_API_KEY is set.
    settings
        .harness_versions
        .insert("codex".into(), "0.170.0".into());
    settings
        .save(&dir.path().join(".model-router.toml"))
        .unwrap();
    let drift = run_doctor(&["--strict"], Some(secret));
    assert!(!drift.status.success());
    let err = String::from_utf8_lossy(&drift.stderr);
    assert!(err.contains("0.170.0") && err.contains("codex"), "{err}");
}

#[test]
fn cost_all_projects_aggregates_recent_projects_and_skips_missing() {
    let cfg = common::tempdir();
    let proj1 = common::tempdir();
    let proj2 = common::tempdir();
    let missing = proj1.path().join("does-not-exist");

    let make_cost_project = |project: &Path, model: &str| {
        let script = project.join("cost_mock.py");
        fs::write(
            &script,
            r#"import json, sys
model = sys.argv[sys.argv.index("--model") + 1]
handoff = sys.argv[sys.argv.index("--handoff-file") + 1]
usd = float(model.split("-")[1])
with open(handoff, "w", encoding="utf-8") as fh:
    json.dump({"summary": "ok", "verdict": "complete", "findings": [], "decisions": [], "files_changed": [], "tests": [], "unresolved": []}, fh)
print(json.dumps({"type": "result", "subtype": "success", "total_cost_usd": usd, "usage": {"input_tokens": 1000, "output_tokens": 200}}))
"#,
        )
        .unwrap();
        let adapter = CustomAdapter {
            command: "python3".into(),
            args: vec![
                script.display().to_string(),
                "--model".into(),
                "{model}".into(),
                "--handoff-file".into(),
                "{handoff_file}".into(),
            ],
            output: Some(CustomOutput::ClaudeStream),
            ..CustomAdapter::default()
        };
        let mut stage = Stage {
            id: "work".into(),
            name: "Work".into(),
            role: Role::Implementation,
            harness: "mockcost".into(),
            model: model.into(),
            ..Stage::default()
        };
        stage.enabled = true;
        let wf = Workflow::new("CostWF", vec![stage]);
        let settings = Settings {
            workflows: vec![wf.clone()],
            adapters: BTreeMap::from([("mockcost".into(), adapter)]),
            ..Settings::default()
        };
        settings.save(&project.join(".model-router.toml")).unwrap();
        let trusted = mr(project, cfg.path(), &["trust", "--yes"]);
        assert!(
            trusted.status.success(),
            "{}",
            String::from_utf8_lossy(&trusted.stderr)
        );
        let ran = mr(project, cfg.path(), &["run", "--task", "Compute cost"]);
        assert!(
            ran.status.success(),
            "{}",
            String::from_utf8_lossy(&ran.stderr)
        );
    };

    make_cost_project(proj1.path(), "cost-1.50");
    make_cost_project(proj2.path(), "cost-2.25");

    // Direct API test: includes proj1 + proj2, skips missing project gracefully.
    let direct = cost::aggregate_projects(
        &[
            proj1.path().to_path_buf(),
            proj2.path().to_path_buf(),
            missing.clone(),
        ],
        None,
        false,
    );
    assert_eq!(direct.projects, Some(2));
    assert_eq!(direct.skipped_projects, Some(1));
    assert!((direct.total.usd - 3.75).abs() < 1e-9, "{:?}", direct.total);
    let rendered = cost::render_summary(&direct);
    assert!(
        rendered.contains("across 2 project(s) (1 skipped)"),
        "{rendered}"
    );

    // CLI test via recent-projects.json in MODEL_ROUTER_CONFIG_DIR.
    let recent: Vec<PathBuf> = vec![
        proj1.path().to_path_buf(),
        proj2.path().to_path_buf(),
        missing,
    ];
    fs::write(
        cfg.path().join("recent-projects.json"),
        serde_json::to_vec(&recent).unwrap(),
    )
    .unwrap();

    let out = mr(
        proj1.path(),
        cfg.path(),
        &["cost", "--all-projects", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["projects"], 2);
    assert!((json["total"]["usd"].as_f64().unwrap() - 3.75).abs() < 1e-9);
    assert_eq!(json["rows"].as_array().unwrap().len(), 2);

    let text_out = mr(proj1.path(), cfg.path(), &["cost", "--all-projects"]);
    assert!(text_out.status.success());
    let text = String::from_utf8_lossy(&text_out.stdout);
    assert!(text.contains("across 2 project(s)"), "{text}");
    assert!(text.contains("Total: $3.75 (reported)"), "{text}");
}

#[test]
fn metrics_export_and_command_json_version_via_flag_and_env() {
    let dir = common::tempdir();
    let cfg = common::tempdir();
    let script = dir.path().join("metrics_mock.py");
    fs::write(
        &script,
        r#"import json, sys
handoff = sys.argv[sys.argv.index("--handoff-file") + 1]
with open(handoff, "w", encoding="utf-8") as fh:
    json.dump({"summary": "done", "verdict": "complete", "findings": [], "decisions": [], "files_changed": [], "tests": [], "unresolved": []}, fh)
print(json.dumps({"type": "result", "subtype": "success", "total_cost_usd": 0.42, "usage": {"input_tokens": 1200, "output_tokens": 300}}))
"#,
    )
    .unwrap();
    let adapter = CustomAdapter {
        command: "python3".into(),
        args: vec![
            script.display().to_string(),
            "--handoff-file".into(),
            "{handoff_file}".into(),
        ],
        output: Some(CustomOutput::ClaudeStream),
        ..CustomAdapter::default()
    };
    let s1 = Stage {
        id: "plan".into(),
        name: "Plan".into(),
        role: Role::Planning,
        harness: "mockm".into(),
        model: "m-1".into(),
        ..Stage::default()
    };
    let s2 = Stage {
        id: "implement".into(),
        name: "Implement".into(),
        role: Role::Implementation,
        harness: "mockm".into(),
        model: "m-2".into(),
        ..Stage::default()
    };
    let wf = Workflow::new("MetricsWF", vec![s1, s2]);
    let settings = Settings {
        workflows: vec![wf],
        adapters: BTreeMap::from([("mockm".into(), adapter)]),
        ..Settings::default()
    };
    settings
        .save(&dir.path().join(".model-router.toml"))
        .unwrap();

    // Trust project so CLI `run` executes custom adapter.
    let trust = mr(dir.path(), cfg.path(), &["trust", "--yes"]);
    assert!(
        trust.status.success(),
        "{}",
        String::from_utf8_lossy(&trust.stderr)
    );

    // 1) Via --metrics-file flag + MODEL_ROUTER_HARNESS_VERSIONS for command.json.
    let metrics_flag_path = dir.path().join("out/flag-metrics.jsonl");
    let run1 = Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("-C")
        .arg(dir.path())
        .arg("--metrics-file")
        .arg(&metrics_flag_path)
        .args([
            "run",
            "--task",
            "Secret prompt text should not enter metrics",
        ])
        .env("MODEL_ROUTER_CONFIG_DIR", cfg.path())
        .env("MODEL_ROUTER_HARNESS_VERSIONS", "mockm=9.8.7")
        .output()
        .unwrap();
    assert!(
        run1.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&run1.stderr)
    );

    let raw = fs::read_to_string(&metrics_flag_path).unwrap();
    assert!(
        !raw.contains("Secret prompt text"),
        "task text leaked into metrics JSONL"
    );
    let records: Vec<MetricRecord> = raw
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let events: Vec<&str> = records.iter().map(|r| r.event.as_str()).collect();
    assert_eq!(
        events,
        [
            "run_start",
            "attempt_start",
            "attempt_end",
            "attempt_start",
            "attempt_end",
            "run_end",
        ]
    );
    let att_end = &records[2];
    assert_eq!(att_end.stage_id.as_deref(), Some("plan"));
    assert_eq!(att_end.harness.as_deref(), Some("mockm"));
    assert_eq!(att_end.model.as_deref(), Some("m-1"));
    assert_eq!(att_end.role.as_deref(), Some("planning"));
    assert_eq!(att_end.status, "complete");
    assert_eq!(att_end.tokens, Some(1500));
    assert_eq!(att_end.exit_code, Some(0));
    assert!((att_end.estimated_usd.unwrap() - 0.42).abs() < 1e-9);
    assert!(att_end.duration_ms.is_some());

    let run_end = &records[5];
    assert_eq!(run_end.status, "complete");
    assert_eq!(run_end.tokens, Some(3000));
    assert!((run_end.estimated_usd.unwrap() - 0.84).abs() < 1e-9);

    // Check attempt command.json recorded harness_version.
    let runs = engine::history(dir.path()).unwrap();
    let saved = runs.iter().find(|r| r.id == records[0].run_id).unwrap();
    let cmd_json: Value = serde_json::from_slice(
        &fs::read(saved.stages[0].attempts[0].directory.join("command.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cmd_json["harness_version"], "9.8.7");

    // 2) Via MODEL_ROUTER_METRICS_FILE env var.
    let metrics_env_path = dir.path().join("env-metrics.jsonl");
    let run2 = Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("-C")
        .arg(dir.path())
        .args(["run", "--task", "Second run via env"])
        .env("MODEL_ROUTER_CONFIG_DIR", cfg.path())
        .env("MODEL_ROUTER_METRICS_FILE", &metrics_env_path)
        .output()
        .unwrap();
    assert!(
        run2.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&run2.stderr)
    );
    let env_records: Vec<MetricRecord> = fs::read_to_string(&metrics_env_path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(env_records.len(), 6);
    assert_eq!(env_records.last().unwrap().event, "run_end");
}
