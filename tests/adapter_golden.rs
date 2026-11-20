//! Golden adapter output: every built-in harness × mode × option variant,
//! recorded as `adapters::build` / `assign_session` / `resume` / `follow_up`
//! (unattended, where the harness has one) results in
//! tests/fixtures/adapter-golden/*.json. The adapter layer must reproduce
//! them byte for byte. Regenerate (only for an intended change) with
//! `MODEL_ROUTER_UPDATE_GOLDEN=1 cargo test --test adapter_golden`.
//!
//! One test function: it sets process-wide variables (PATH, harness version
//! overrides) that must not race other tests.
use model_router::{
    adapters::{self, ContextPaths, EnvValue, Invocation},
    config::{CustomAdapter, Mode, Stage},
    providers::Provider,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path};

const HARNESSES: &[&str] = &[
    "codex", "claude", "pi", "omp", "cursor", "gemini", "opencode", "aider", "goose", "amp",
    "copilot",
];

/// Native settings and every choice, as documented per harness.
const SETTINGS: &[(&str, &str, &[&str])] = &[
    (
        "codex",
        "sandbox",
        &["read-only", "workspace-write", "danger-full-access"],
    ),
    ("codex", "approval_policy", &["on-request", "never"]),
    (
        "claude",
        "permission_mode",
        &[
            "manual",
            "acceptEdits",
            "plan",
            "dontAsk",
            "auto",
            "bypassPermissions",
        ],
    ),
    ("cursor", "sandbox", &["enabled", "disabled"]),
    (
        "gemini",
        "approval_mode",
        &["default", "auto_edit", "plan", "yolo"],
    ),
    ("omp", "approval_mode", &["always-ask", "write", "yolo"]),
    ("aider", "confirmations", &["yes-always"]),
    (
        "goose",
        "mode",
        &["approve", "smart_approve", "chat", "auto"],
    ),
    ("copilot", "tool_permissions", &["allow-all-tools"]),
];

const CURRENT: &str =
    "codex=codex-cli 0.160.0,claude=2.1.318 (Claude Code),opencode=opencode v2.0.22";
const OLD: &str = "codex=codex-cli 0.40.0,claude=2.1.0 (Claude Code),opencode=1.4.3";

fn set_versions(value: &str) {
    // SAFETY: this test binary has a single test, so no thread reads the
    // environment concurrently.
    unsafe { std::env::set_var("MODEL_ROUTER_HARNESS_VERSIONS", value) };
}

fn providers() -> BTreeMap<String, Provider> {
    BTreeMap::from([
        (
            "proxy".to_string(),
            Provider {
                base_url: "http://localhost:4000/v1".into(),
                api_key_env: "PROXY_KEY".into(),
                ..Provider::default()
            },
        ),
        (
            "local".to_string(),
            Provider {
                base_url: "http://localhost:8000".into(),
                ..Provider::default()
            },
        ),
        ("groq".to_string(), Provider::default()),
    ])
}

struct Case {
    name: String,
    stage: Stage,
    versions: &'static str,
}

fn cases(harness: &str, mode: Mode) -> Vec<Case> {
    let base = Stage {
        harness: harness.into(),
        mode,
        ..Stage::default()
    };
    let with = |model: &str, provider: &str, effort: &str| Stage {
        model: model.into(),
        provider: provider.into(),
        effort: effort.into(),
        ..base.clone()
    };
    let mut list = vec![
        ("default", base.clone(), CURRENT),
        ("model", with("m-1", "", ""), CURRENT),
        ("model+provider", with("m-1", "openrouter", ""), CURRENT),
        ("model+local-provider", with("m-1", "ollama", ""), CURRENT),
        ("model+lmstudio", with("m-1", "lmstudio", ""), CURRENT),
        ("model+effort", with("m-1", "", "high"), CURRENT),
        ("effort-only", with("", "", "high"), CURRENT),
        (
            "model+provider+effort",
            with("m-1", "openrouter", "high"),
            CURRENT,
        ),
        ("routed", with("m-1", "proxy", ""), CURRENT),
        ("routed-keyless", with("m-1", "local", ""), CURRENT),
        ("routed-preset", with("m-1", "groq", ""), CURRENT),
        ("routed+effort", with("m-1", "proxy", "high"), CURRENT),
        (
            "provider-without-model",
            with("", "openrouter", ""),
            CURRENT,
        ),
        ("structured-off-old-version", with("m-1", "", "high"), OLD),
        ("old-version-routed", with("m-1", "proxy", ""), OLD),
        ("old-version-default", base.clone(), OLD),
        (
            "extra-args",
            Stage {
                extra_args: vec!["--extra-flag".into(), "v".into()],
                ..with("m-1", "", "")
            },
            CURRENT,
        ),
        (
            "user-output-schema",
            Stage {
                extra_args: vec!["--output-schema".into(), "mine.json".into()],
                ..with("m-1", "", "")
            },
            CURRENT,
        ),
        (
            "user-json-schema",
            Stage {
                extra_args: vec!["--json-schema={}".into()],
                ..with("m-1", "", "")
            },
            CURRENT,
        ),
        (
            "unknown-setting",
            Stage {
                harness_settings: BTreeMap::from([("nope".into(), "x".into())]),
                ..base.clone()
            },
            CURRENT,
        ),
    ];
    if harness == "codex" {
        // `codex exec resume` takes config overrides only: the follow-up
        // keeps these and drops the other arguments.
        list.push((
            "extra-config-args",
            Stage {
                extra_args: vec![
                    "-c".into(),
                    "a=1".into(),
                    "--config=b=2".into(),
                    "--full-auto".into(),
                    "--config".into(),
                    "c=\"x\"".into(),
                ],
                ..with("m-1", "", "")
            },
            CURRENT,
        ));
    }
    let mut owned: Vec<Case> = list
        .drain(..)
        .map(|(name, stage, versions)| Case {
            name: name.into(),
            stage,
            versions,
        })
        .collect();
    for (h, key, choices) in SETTINGS.iter().filter(|(h, _, _)| *h == harness) {
        let _ = h;
        for choice in choices.iter().chain(&[""]) {
            owned.push(Case {
                name: format!("setting:{key}={choice}"),
                stage: Stage {
                    harness_settings: BTreeMap::from([((*key).into(), (*choice).into())]),
                    ..with("m-1", "", "")
                },
                versions: CURRENT,
            });
        }
        owned.push(Case {
            name: format!("setting:{key}+invalid"),
            stage: Stage {
                harness_settings: BTreeMap::from([((*key).into(), "not-a-choice".into())]),
                ..base.clone()
            },
            versions: CURRENT,
        });
    }
    owned
}

fn normalize(text: &str, tmp: &str) -> String {
    text.replace(tmp, "$TMP")
}

fn env_json(value: &EnvValue, tmp: &str) -> Value {
    match value {
        EnvValue::Literal(v) => json!({ "literal": normalize(v, tmp) }),
        EnvValue::Var(v) => json!({ "var": v }),
        EnvValue::JsonInsert { path, value } => {
            json!({ "json_insert": { "path": path, "value": normalize(value, tmp) } })
        }
        EnvValue::UnsetPrefix => json!("unset_prefix"),
    }
}

fn invocation_json(i: &Invocation, tmp: &str) -> Value {
    json!({
        "program": i.program,
        "args": i.args.iter().map(|a| normalize(a, tmp)).collect::<Vec<_>>(),
        "stdin": i.stdin.as_deref().map(|s| normalize(s, tmp)),
        "kind": format!("{:?}", i.kind),
        "final_file": normalize(&i.final_file.display().to_string(), tmp),
        "env": i.env.iter().map(|(n, v)| json!([n, env_json(v, tmp)])).collect::<Vec<_>>(),
        "files": i.files.iter().map(|(p, text)| json!([
            normalize(&p.display().to_string(), tmp),
            normalize(text, tmp),
        ])).collect::<Vec<_>>(),
        "uses_output_schema": adapters::uses_output_schema(i),
    })
}

fn result_json(result: anyhow::Result<Invocation>, tmp: &str) -> (Value, Option<Invocation>) {
    match result {
        Ok(i) => (invocation_json(&i, tmp), Some(i)),
        Err(e) => (json!({ "error": format!("{e:#}") }), None),
    }
}

fn record(
    stage: &Stage,
    custom: &BTreeMap<String, CustomAdapter>,
    configured: &BTreeMap<String, Provider>,
    paths: &ContextPaths<'_>,
    tmp: &str,
) -> Value {
    let prompt = "Do the task.\nLine two {model} $(x)";
    let (built, invocation) = result_json(
        adapters::build(stage, custom, configured, prompt, paths),
        tmp,
    );
    let session = invocation.map(|mut i| {
        let id = adapters::assign_session(&mut i, &stage.harness);
        let args: Vec<String> = i
            .args
            .iter()
            .map(|a| match &id {
                Some(id) if a == id => "<session-id>".into(),
                _ => normalize(a, tmp),
            })
            .collect();
        json!({ "assigned": id.is_some(), "args": args })
    });
    let resumable = adapters::resumable(&stage.harness, custom);
    let resumed = resumable.then(|| {
        result_json(
            adapters::resume(stage, custom, configured, "S-1", paths),
            tmp,
        )
        .0
    });
    let mut out = json!({
        "build": built,
        "session": session,
        "resumable": resumable,
        "resume": resumed,
    });
    // Only harnesses with an unattended follow-up record one, so the others'
    // files stay as they were.
    if stage.mode == Mode::Unattended && adapters::has_follow_up(&stage.harness, custom) {
        let prompt = "Repair the handoff.\nLine two {session} $(x)";
        out["follow_up"] = result_json(
            adapters::follow_up(stage, custom, configured, "S-1", prompt, paths),
            tmp,
        )
        .0;
    }
    out
}

fn check(name: &str, value: &Value, failures: &mut Vec<String>) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/adapter-golden");
    let path = dir.join(format!("{name}.json"));
    let text = serde_json::to_string_pretty(value).unwrap() + "\n";
    if std::env::var_os("MODEL_ROUTER_UPDATE_GOLDEN").is_some() {
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, &text).unwrap();
        return;
    }
    let expected = fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("Missing {}; regenerate the golden files", path.display()));
    if expected != text {
        let wanted: Value = serde_json::from_str(&expected).unwrap();
        let mut first = String::new();
        if let (Some(a), Some(b)) = (wanted.as_object(), value.as_object()) {
            for (case, got) in b {
                if a.get(case) != Some(got) {
                    first = format!(
                        "{case}:\n  expected {}\n  got      {}",
                        a.get(case).map(Value::to_string).unwrap_or_default(),
                        got
                    );
                    break;
                }
            }
        }
        failures.push(format!("{name} differs. First case: {first}"));
    }
}

#[test]
fn adapter_output_matches_golden_files() {
    let empty_path = tempfile::tempdir().unwrap();
    // SAFETY: single-test binary; see set_versions.
    unsafe { std::env::set_var("PATH", empty_path.path()) };
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().display().to_string();
    let prompt_file = dir.path().join("prompt.md");
    let handoff_file = dir.path().join("handoff.json");
    let workspace = dir.path().join("workspace");
    let attempt = dir.path().join("attempt");
    let paths = ContextPaths {
        workspace: &workspace,
        attempt_dir: &attempt,
        prompt_file: &prompt_file,
        handoff_file: &handoff_file,
    };
    let configured = providers();
    let none = BTreeMap::new();
    let mut failures = vec![];
    for harness in HARNESSES {
        let mut out = serde_json::Map::new();
        for mode in [Mode::Unattended, Mode::Interactive] {
            for case in cases(harness, mode) {
                set_versions(case.versions);
                out.insert(
                    format!("{}/{}", mode.label(), case.name),
                    record(&case.stage, &none, &configured, &paths, &tmp),
                );
            }
        }
        check(harness, &Value::Object(out), &mut failures);
    }

    // Custom adapters keep their template behaviour.
    set_versions(CURRENT);
    let custom = BTreeMap::from([
        (
            "mine".to_string(),
            CustomAdapter {
                command: "mine-cli".into(),
                args: vec![
                    "run".into(),
                    "--model={model}".into(),
                    "--provider={provider}".into(),
                    "--effort={effort}".into(),
                    "--base={base_url}".into(),
                    "--key={api_key_env}".into(),
                    "{prompt_file}".into(),
                    "{handoff_file}".into(),
                    "{workspace}".into(),
                    "{run_dir}".into(),
                ],
                interactive_args: Some(vec!["chat".into(), "{prompt}".into()]),
                models_args: Some(vec!["models".into()]),
                prompt_stdin: true,
                session: Some(model_router::config::CustomSession {
                    assign: None,
                    reported: true,
                }),
                follow_up_args: Some(vec![
                    "continue".into(),
                    "{session}".into(),
                    "{prompt_file}".into(),
                ]),
                ..CustomAdapter::default()
            },
        ),
        (
            "plain".to_string(),
            CustomAdapter {
                command: "./bin/plain".into(),
                args: vec!["{prompt}".into()],
                ..CustomAdapter::default()
            },
        ),
    ]);
    let mut out = serde_json::Map::new();
    for name in ["mine", "plain"] {
        for mode in [Mode::Unattended, Mode::Interactive] {
            let base = Stage {
                harness: name.into(),
                mode,
                ..Stage::default()
            };
            let variants = [
                ("default", base.clone()),
                (
                    "model+effort",
                    Stage {
                        model: "m-1".into(),
                        effort: "high".into(),
                        ..base.clone()
                    },
                ),
                (
                    "native-provider",
                    Stage {
                        model: "m-1".into(),
                        provider: "openrouter".into(),
                        ..base.clone()
                    },
                ),
                (
                    "routed",
                    Stage {
                        model: "m-1".into(),
                        provider: "proxy".into(),
                        extra_args: vec!["--x".into()],
                        ..base.clone()
                    },
                ),
            ];
            for (case, stage) in variants {
                out.insert(
                    format!("{name}/{}/{case}", mode.label()),
                    record(&stage, &custom, &configured, &paths, &tmp),
                );
            }
        }
    }
    check("custom", &Value::Object(out), &mut failures);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
