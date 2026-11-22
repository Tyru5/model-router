//! Custom adapters declared in TOML: the spec fields they share with
//! built-in harnesses, backward compatibility, and trust classification.
use model_router::{
    adapters::{self, ContextPaths, OutputKind},
    config::{self, CustomAdapter, CustomOutput, Mode, Stage},
    trust,
};
use std::collections::BTreeMap;

const OLD: &str = r#"version = 2
[adapters.legacy]
command = "legacy-cli"
args = ["run", "{prompt}"]
interactive_args = ["chat", "{prompt}"]
models_args = ["models"]
prompt_stdin = true
"#;

const NEW: &str = r#"version = 2
[adapters.spec]
command = "spec-cli"
args = ["exec", "--json"]
interactive_args = ["chat"]
output = "claude-stream"
session = { assign = "--session-id" }
resume_args = ["chat", "--resume", "{session}"]
model_flag = "--model"
effort_flag = "--effort"
prompt = { flag = "-p" }

[adapters.reported]
command = "reported-cli"
args = ["run"]
output = "codex-jsonl"
session = { reported = true }
resume_args = ["resume", "{session}"]
prompt = "arg"
"#;

fn stage(harness: &str, mode: Mode) -> Stage {
    Stage {
        harness: harness.into(),
        model: "m-1".into(),
        effort: "high".into(),
        mode,
        ..Stage::default()
    }
}

fn build(
    adapters: &BTreeMap<String, CustomAdapter>,
    stage: &Stage,
) -> anyhow::Result<adapters::Invocation> {
    let dir = tempfile::tempdir().unwrap();
    let paths = ContextPaths {
        workspace: dir.path(),
        attempt_dir: dir.path(),
        prompt_file: dir.path(),
        handoff_file: dir.path(),
        mcp: None,
    };
    adapters::build(stage, adapters, &BTreeMap::new(), "task", &paths)
}

#[test]
fn old_adapters_parse_build_and_save_unchanged() {
    let settings = config::parse(OLD).unwrap();
    assert!(settings.warnings.is_empty(), "{:?}", settings.warnings);
    settings.validate().unwrap();
    let legacy = &settings.adapters["legacy"];
    assert!(legacy.output.is_none() && legacy.session.is_none() && legacy.prompt.is_none());
    let i = build(&settings.adapters, &stage("legacy", Mode::Unattended)).unwrap();
    assert_eq!(i.args, ["run", "task"]);
    assert_eq!(i.stdin.as_deref(), Some("task"));
    assert_eq!(i.kind, OutputKind::Text);
    assert!(!adapters::resumable("legacy", &settings.adapters));
    // Saved configs gain no new keys.
    let saved = toml::to_string(&settings.adapters["legacy"]).unwrap();
    for key in ["output", "session", "resume_args", "model_flag", "prompt ="] {
        assert!(!saved.contains(key), "{saved}");
    }
    // The trust line (and so the digest) is what it was.
    assert_eq!(
        trust::dangerous(&settings),
        [
            "custom adapter legacy: legacy-cli run {prompt}; interactive: chat {prompt}; models: models"
        ]
    );
}

#[test]
fn new_fields_drive_prompt_model_effort_output_session_and_resume() {
    let settings = config::parse(NEW).unwrap();
    assert!(settings.warnings.is_empty(), "{:?}", settings.warnings);
    settings.validate().unwrap();
    let adapters = &settings.adapters;
    assert_eq!(adapters["spec"].output, Some(CustomOutput::ClaudeStream));

    let mut i = build(adapters, &stage("spec", Mode::Unattended)).unwrap();
    assert_eq!(
        i.args,
        [
            "exec", "--json", "--model", "m-1", "--effort", "high", "-p", "task"
        ]
    );
    assert!(i.stdin.is_none());
    assert_eq!(i.kind, OutputKind::Claude);
    let id = adapters::assign_session(&mut i, "spec").unwrap();
    assert_eq!(i.args[i.args.len() - 2..], ["--session-id".to_owned(), id]);
    // An explicit session flag wins.
    let mut chosen = build(
        adapters,
        &Stage {
            extra_args: vec!["--session-id=mine".into()],
            ..stage("spec", Mode::Unattended)
        },
    )
    .unwrap();
    assert_eq!(adapters::assign_session(&mut chosen, "spec"), None);

    assert!(adapters::resumable("spec", adapters));
    let dir = tempfile::tempdir().unwrap();
    let paths = ContextPaths {
        workspace: dir.path(),
        attempt_dir: dir.path(),
        prompt_file: dir.path(),
        handoff_file: dir.path(),
        mcp: None,
    };
    let resumed = adapters::resume(
        &stage("spec", Mode::Unattended),
        adapters,
        &BTreeMap::new(),
        "S-1",
        &paths,
    )
    .unwrap();
    assert_eq!(
        resumed.args,
        [
            "chat", "--resume", "S-1", "--model", "m-1", "--effort", "high"
        ]
    );
    assert!(resumed.stdin.is_none());

    // Reported sessions come from the output events; nothing is assigned.
    let mut reported = build(adapters, &stage("reported", Mode::Unattended)).unwrap();
    assert_eq!(reported.args, ["run", "task"]);
    assert_eq!(reported.kind, OutputKind::Codex);
    assert_eq!(adapters::assign_session(&mut reported, "reported"), None);
    assert!(adapters::resumable("reported", adapters));
    let resumed = adapters::resume(
        &stage("reported", Mode::Interactive),
        adapters,
        &BTreeMap::new(),
        "T-9",
        &paths,
    )
    .unwrap();
    assert_eq!(resumed.args, ["resume", "T-9"]);
    // Interactive runs need interactive_args.
    assert!(build(adapters, &stage("reported", Mode::Interactive)).is_err());
}

#[test]
fn invalid_new_fields_are_rejected() {
    for (field, error) in [
        ("model_flag = \"model\"", "model_flag must be a flag"),
        (
            "effort_flag = \"--effort level\"",
            "effort_flag must be a flag",
        ),
        ("prompt = { flag = \"p\" }", "prompt flag must be a flag"),
        (
            "prompt_stdin = true\nprompt = \"arg\"",
            "both prompt_stdin and another prompt",
        ),
        ("session = {}", "exactly one of assign or reported"),
        (
            "session = { assign = \"--s\", reported = true }",
            "exactly one of assign or reported",
        ),
        (
            "session = { assign = \"s\" }",
            "session assign must be a flag",
        ),
        (
            "resume_args = [\"{session}\"]",
            "resume_args needs a session",
        ),
    ] {
        let text = format!("version = 2\n[adapters.x]\ncommand = \"x\"\n{field}\n");
        let message = config::parse(&text)
            .and_then(|s| s.validate())
            .unwrap_err()
            .to_string();
        assert!(message.contains(error), "{field}: {message}");
    }
    let unknown = "version = 2\n[adapters.x]\ncommand = \"x\"\noutput = \"xml\"\n";
    assert!(config::parse(unknown).is_err());
    let typo = "version = 2\n[adapters.x]\ncommand = \"x\"\nsession = { asign = \"--s\" }\n";
    assert!(config::parse(typo).is_err());
}

#[test]
fn new_fields_that_reach_the_executable_need_trust() {
    let settings = config::parse(NEW).unwrap();
    let items = trust::dangerous(&settings);
    assert_eq!(
        items,
        [
            "custom adapter reported: reported-cli run; resume: resume {session}; prompt: arg",
            "custom adapter spec: spec-cli exec --json; interactive: chat; resume: chat --resume {session}; model flag: --model; effort flag: --effort; prompt flag: -p; session flag: --session-id",
        ]
    );
    // Changing any of them changes the digest.
    let mut changed = settings.clone();
    changed.adapters.get_mut("spec").unwrap().effort_flag = Some("--reasoning".into());
    assert_ne!(
        trust::digest(&items),
        trust::digest(&trust::dangerous(&changed))
    );
    // Untrusted, the adapter keeps its shape but cannot launch anything.
    let mut ignored = settings.clone();
    trust::ignore(&mut ignored);
    ignored.validate().unwrap();
    let spec = &ignored.adapters["spec"];
    assert_eq!(spec.command, trust::BLOCKED_COMMAND);
    assert!(spec.model_flag.is_none() && spec.effort_flag.is_none() && spec.prompt.is_none());
    assert_eq!(spec.resume_args, Some(vec![]));
    let i = build(&ignored.adapters, &stage("spec", Mode::Unattended)).unwrap();
    assert_eq!(i.program, trust::BLOCKED_COMMAND);
    assert!(adapters::executable(&i.program).is_none());
    // Saving restores what was ignored.
    let restored = trust::restore(&ignored);
    assert_eq!(
        restored.adapters["spec"].model_flag.as_deref(),
        Some("--model")
    );
}
