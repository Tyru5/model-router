use model_router::{
    adapters::{self, ContextPaths, OutputKind},
    config::{BUILTINS, CustomAdapter, Mode, Role, Settings, Stage},
    handoff::{self, Verdict},
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    process::{Command, Output},
};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/harness-output");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    fixture: Vec<Fixture>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    file: String,
    harness: String,
    kind: OutputKind,
    version: String,
    scenario: String,
    provenance: String,
    source: String,
    sanitization: String,
    #[serde(default)]
    text_contains: Vec<String>,
    #[serde(default)]
    text_excludes: Vec<String>,
    error_contains: Option<String>,
    #[serde(default)]
    permission_blocked: bool,
    handoff_summary: Option<String>,
    role: Option<Role>,
    verdict: Option<Verdict>,
}

fn manifest(root: &Path) -> Manifest {
    toml::from_str(&fs::read_to_string(root.join("manifest.toml")).unwrap()).unwrap()
}

/// Runs normalize (and parse for handoffs) and checks what the manifest declares.
fn check(root: &Path, f: &Fixture) {
    let name = &f.file;
    let raw = fs::read_to_string(root.join(name)).unwrap();
    let out = handoff::normalize(&raw, f.kind);
    match &f.error_contains {
        Some(expected) => assert!(
            out.error
                .as_deref()
                .is_some_and(|e| e.contains(expected.as_str())),
            "{name}: error {:?}, expected {expected:?}",
            out.error
        ),
        None => assert!(out.error.is_none(), "{name}: unexpected {:?}", out.error),
    }
    assert_eq!(out.permission_blocked, f.permission_blocked, "{name}");
    for text in &f.text_contains {
        assert!(out.text.contains(text.as_str()), "{name}: lacks {text:?}");
    }
    for text in &f.text_excludes {
        assert!(!out.text.contains(text.as_str()), "{name}: has {text:?}");
    }
    if let Some(summary) = &f.handoff_summary {
        let parsed = handoff::parse(&out.text, f.role.unwrap_or(Role::Implementation))
            .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        assert!(parsed.structured, "{name}");
        assert_eq!(&parsed.summary, summary, "{name}");
        if let Some(verdict) = f.verdict {
            assert_eq!(parsed.verdict, verdict, "{name}");
        }
    }
}

#[test]
fn every_fixture_normalizes_as_its_manifest_declares() {
    let root = Path::new(FIXTURES);
    let manifest = manifest(root);
    for fixture in &manifest.fixture {
        check(root, fixture);
    }
}

fn kind_of(harness: &str) -> OutputKind {
    let dir = tempfile::tempdir().unwrap();
    let paths = ContextPaths {
        workspace: dir.path(),
        attempt_dir: dir.path(),
        prompt_file: dir.path(),
        handoff_file: dir.path(),
    };
    let kinds: Vec<_> = [Mode::Unattended, Mode::Interactive]
        .into_iter()
        .map(|mode| {
            let stage = Stage {
                harness: harness.into(),
                model: "m".into(),
                mode,
                ..Stage::default()
            };
            adapters::build(&stage, &BTreeMap::new(), &BTreeMap::new(), "task", &paths)
                .unwrap()
                .kind
        })
        .collect();
    assert_eq!(kinds[0], kinds[1], "{harness}");
    kinds[0]
}

#[test]
fn manifest_is_labeled_matches_adapters_and_covers_each_scenario() {
    let root = Path::new(FIXTURES);
    let manifest = manifest(root);
    let mut listed = BTreeSet::new();
    for f in &manifest.fixture {
        assert!(
            matches!(f.provenance.as_str(), "captured" | "schema-derived"),
            "{}: provenance {}",
            f.file,
            f.provenance
        );
        assert!(BUILTINS.contains(&f.harness.as_str()), "{}", f.file);
        assert!(f.file.starts_with(&format!("{}/", f.harness)), "{}", f.file);
        // *.log is ignored by Git.
        assert!(!f.file.ends_with(".log"), "{}", f.file);
        assert_eq!(
            f.kind,
            kind_of(&f.harness),
            "{}: kind differs from adapter",
            f.file
        );
        for field in [&f.version, &f.scenario, &f.source, &f.sanitization] {
            assert!(!field.trim().is_empty(), "{}: empty metadata", f.file);
        }
        assert!(listed.insert(f.file.clone()), "{} listed twice", f.file);
    }
    for harness in BUILTINS {
        let scenarios: BTreeSet<_> = manifest
            .fixture
            .iter()
            .filter(|f| f.harness == *harness)
            .map(|f| f.scenario.as_str())
            .collect();
        let required: &[&str] = match (kind_of(harness), *harness) {
            (OutputKind::Text, _) => &["transcript"],
            (_, "claude") => &["success", "error", "tool-echo", "permission-denial"],
            _ => &["success", "error", "tool-echo"],
        };
        for scenario in required {
            assert!(scenarios.contains(scenario), "{harness} lacks {scenario}");
        }
    }
    for dir in fs::read_dir(root).unwrap() {
        let dir = dir.unwrap().path();
        if dir.is_dir() {
            for file in fs::read_dir(&dir).unwrap() {
                let file = file.unwrap().path();
                let relative = file.strip_prefix(root).unwrap().to_str().unwrap();
                assert!(
                    listed.contains(relative),
                    "{relative} is not in the manifest"
                );
            }
        }
    }
}

fn fake(bin: &Path, name: &str, script: &str) {
    let path = bin.join(name);
    fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// A POSIX shell harness that logs its arguments and answers --version/--help.
fn shell_harness(bin: &Path, name: &str, log: &Path, cases: &[(&str, &str)]) {
    let mut script = format!(
        "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$*\" in\n",
        log.display()
    );
    for (args, output) in cases {
        script.push_str(&format!("  '{args}') printf '%s\\n' '{output}' ;;\n"));
    }
    script.push_str("  *) exit 64 ;;\nesac\n");
    fake(bin, name, &script);
}

fn doctor(project: &Path, bin: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("-C")
        .arg(project)
        .arg("doctor")
        .args(args)
        .env("PATH", bin)
        .output()
        .unwrap()
}

#[test]
fn doctor_reports_versions_and_flags_from_help_without_running_a_model() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let log = dir.path().join("calls.log");
    // Codex exec help omits --skip-git-repo-check; everything else is listed.
    shell_harness(
        &bin,
        "codex",
        &log,
        &[
            ("--version", "codex-cli 0.160.0"),
            (
                "exec --help",
                "Usage: codex exec [OPTIONS] [PROMPT]\n  -m, --model <MODEL>\n      --json\n      --color <COLOR>\n  -o, --output-last-message <FILE>\n  -c, --config <key=value>\n      --oss\n      --local-provider <P>\n  -s, --sandbox <MODE>",
            ),
            (
                "--help",
                "  -m, --model <MODEL>\n  -c, --config <key=value>\n      --oss\n      --local-provider <P>\n  -s, --sandbox <MODE>",
            ),
            (
                "resume --help",
                "Usage: codex resume [OPTIONS] [SESSION_ID]\n  -m, --model <MODEL>\n  -c, --config <key=value>\n      --oss\n      --local-provider <P>\n  -s, --sandbox <MODE>",
            ),
        ],
    );
    shell_harness(
        &bin,
        "claude",
        &log,
        &[
            ("--version", "2.1.289 (Claude Code)"),
            (
                "--help",
                "  -p, --print       Print response and exit\n  --output-format <format>  Output format\n  --verbose  Verbose\n  --session-id <uuid>  Session\n  -r, --resume [value]  Resume\n  --model <model>   Model\n  --effort <level>  Effort\n  --permission-mode <mode>  Mode",
            ),
        ],
    );
    shell_harness(
        &bin,
        "amp",
        &log,
        &[
            ("--version", "0.0.1-fixture (released today)"),
            (
                "--help",
                "  -m, --mode <value>\n  -x, --execute [message]\n  --stream-json\n      Stream JSON",
            ),
            (
                "threads --help",
                "Commands:\n  continue  Continue an existing thread\nGlobal options:\n  -m, --mode <value>",
            ),
        ],
    );
    // No flags in help: capability support is unknown.
    shell_harness(
        &bin,
        "opencode",
        &log,
        &[("--version", "opencode v2.0.22"), ("run --help", "")],
    );
    let mut settings = Settings::default();
    settings.adapters.insert(
        "mine".into(),
        CustomAdapter {
            command: "mine-tool".into(),
            ..CustomAdapter::default()
        },
    );
    fake(&bin, "mine-tool", "#!/bin/sh\nexit 1\n");
    settings
        .save(&dir.path().join(".model-router.toml"))
        .unwrap();

    let output = doctor(dir.path(), &bin, &["--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reports: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reports.len(), BUILTINS.len() + 1);
    let report = |name: &str| reports.iter().find(|r| r["harness"] == name).unwrap();
    let codex = report("codex");
    assert_eq!(codex["version"], "codex-cli 0.160.0");
    assert_eq!(codex["version_tested"], true);
    assert_eq!(
        codex["summary"],
        "missing: --skip-git-repo-check (unattended)"
    );
    let support = |flag: &str, help: &str| {
        codex["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["flag"] == flag && c["help"] == help)
            .unwrap()["support"]
            .clone()
    };
    assert_eq!(
        support("--skip-git-repo-check", "codex exec --help"),
        "missing"
    );
    assert_eq!(support("--json", "codex exec --help"), "supported");
    assert_eq!(support("--model", "codex --help"), "supported");
    assert_eq!(report("claude")["summary"], "ok");
    assert_eq!(report("claude")["version_tested"], true);
    assert_eq!(report("amp")["version_tested"], false);
    assert_eq!(report("amp")["summary"], "ok");
    assert_eq!(report("opencode")["summary"], "unknown: help unavailable");
    assert_eq!(report("gemini")["path"], Value::Null);
    assert_eq!(report("gemini")["summary"], "not checked");
    let mine = report("mine");
    assert_eq!(mine["builtin"], false);
    assert_eq!(mine["summary"], "not declared");
    assert_eq!(mine["capabilities"], Value::Null);
    assert_eq!(mine["version"], Value::Null);
    for r in &reports {
        assert!(
            r["notes"][0]
                .as_str()
                .unwrap()
                .contains("Authentication is not checked")
        );
    }
    let calls = fs::read_to_string(&log).unwrap();
    assert!(!calls.is_empty());
    for call in calls.lines() {
        assert!(
            call == "--version" || call.ends_with("--help"),
            "ran {call:?}"
        );
    }

    let table = doctor(dir.path(), &bin, &[]);
    assert!(table.status.success());
    let table = String::from_utf8(table.stdout).unwrap();
    let row = |name: &str| {
        table
            .lines()
            .find(|line| line.starts_with(&format!("{name} ")))
            .unwrap()
            .to_owned()
    };
    assert!(table.starts_with("HARNESS"));
    let codex = row("codex");
    for cell in [
        "installed",
        "0.160.0",
        "tested",
        "missing: --skip-git-repo-check (unattended)",
    ] {
        assert!(codex.contains(cell), "{codex}");
    }
    assert!(row("amp").contains("untested"));
    assert!(row("gemini").contains("missing") && row("gemini").contains("not checked"));
    assert!(row("mine").contains("not declared"));
    assert!(table.trim_end().ends_with("Log in through each harness."));

    let strict = doctor(dir.path(), &bin, &["--strict"]);
    assert!(!strict.status.success());
    assert!(String::from_utf8_lossy(&strict.stderr).contains("lacks a flag"));
}

const FAKE_CLAUDE: &str = r#"#!/usr/bin/env python3
import json, os, re, sys
if sys.argv[1:] == ["--version"]:
    print("9.9.9 (Fake Claude)")
    sys.exit(0)
prompt = sys.stdin.read()
block = re.search(r"<router-handoff>.*?</router-handoff>", prompt, re.S).group(0)
print(json.dumps({
    "type": "result", "subtype": "success", "is_error": False, "num_turns": 1,
    "result": block, "permission_denials": [],
    "session_id": "6f1c2b9e-1d3a-4c5e-9f7a-0b1c2d3e4f5a",
    "uuid": "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d",
    "note": "cwd=%s home=%s mail=dev@corp.example key=sk-ant-api03-%s"
        % (os.getcwd(), os.path.expanduser("~"), "x" * 24),
    "echo": prompt,
}))
"#;

#[test]
fn capture_script_refuses_without_confirmation_and_sanitizes_offline_output() {
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/capture_harness_output.py"
    );
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    fake(&bin, "claude", FAKE_CLAUDE);
    let out = dir.path().join("fixtures");
    let path = std::iter::once(bin.clone())
        .chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ))
        .collect::<Vec<_>>();
    let capture = |confirm: bool| {
        Command::new("python3")
            .arg(script)
            .arg("claude")
            .args(confirm.then_some("--confirm-paid-request"))
            .arg("--router")
            .arg(env!("CARGO_BIN_EXE_model-router"))
            .arg("--output-dir")
            .arg(&out)
            .env("PATH", std::env::join_paths(&path).unwrap())
            .output()
            .unwrap()
    };
    let refused = capture(false);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--confirm-paid-request"));
    assert!(!out.exists(), "a refused capture wrote files");

    let captured = capture(true);
    assert!(
        captured.status.success(),
        "{}",
        String::from_utf8_lossy(&captured.stderr)
    );
    let text = fs::read_to_string(out.join("claude/captured-success.json")).unwrap();
    let home = std::env::var("HOME").unwrap_or_default();
    for secret in [
        "router-capture-",
        "dev@corp.example",
        "sk-ant-api03",
        "6f1c2b9e-1d3a",
        "HANDOFF CONTRACT",
    ] {
        assert!(!text.contains(secret), "{secret} survived: {text}");
    }
    assert!(
        home.len() < 2 || !text.contains(&home),
        "home path survived"
    );
    for placeholder in ["<router prompt omitted>", "user@example.com", "<redacted>"] {
        assert!(text.contains(placeholder), "{placeholder} missing: {text}");
    }
    let manifest = manifest(&out);
    assert_eq!(manifest.fixture.len(), 1);
    let entry = &manifest.fixture[0];
    assert_eq!(entry.provenance, "captured");
    assert_eq!(entry.version, "9.9.9");
    assert_eq!(entry.kind, OutputKind::Claude);
    check(&out, entry);
}
