use model_router::{
    adapters::{self, ContextPaths},
    config::{self, Mode, Settings, Stage},
};
use std::collections::BTreeMap;

#[test]
fn native_settings_reach_both_execution_modes_and_survive_reload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut settings = Settings::default();
    let cases = [
        (
            "codex",
            "sandbox",
            "workspace-write",
            "--sandbox",
            "workspace-write",
        ),
        (
            "codex",
            "approval_policy",
            "never",
            "--config",
            "approval_policy=\"never\"",
        ),
        (
            "claude",
            "permission_mode",
            "plan",
            "--permission-mode",
            "plan",
        ),
        ("cursor", "sandbox", "enabled", "--sandbox", "enabled"),
        (
            "gemini",
            "approval_mode",
            "auto_edit",
            "--approval-mode",
            "auto_edit",
        ),
    ];
    let paths = ContextPaths {
        workspace: dir.path(),
        attempt_dir: dir.path(),
        prompt_file: dir.path(),
        handoff_file: dir.path(),
        mcp: None,
    };
    for (harness, key, value, flag, argument) in cases {
        for mode in [Mode::Unattended, Mode::Interactive] {
            settings.workflows[0].stages[0] = Stage {
                harness: harness.into(),
                mode,
                harness_settings: BTreeMap::from([(key.into(), value.into())]),
                extra_args: vec!["--literal $(no-shell); 雪".into()],
                ..Stage::default()
            };
            let others = serde_json::to_value(&settings.workflows[0].stages[1..]).unwrap();
            settings.save(&path).unwrap();
            let (loaded, _) = config::load(dir.path(), Some(&path)).unwrap();
            assert_eq!(
                serde_json::to_value(&loaded).unwrap(),
                serde_json::to_value(&settings).unwrap()
            );
            assert_eq!(
                serde_json::to_value(&loaded.workflows[0].stages[1..]).unwrap(),
                others
            );
            let inv = adapters::build(
                &loaded.workflows[0].stages[0],
                &loaded.adapters,
                &loaded.providers,
                "Task",
                &paths,
            )
            .unwrap();
            assert!(
                inv.args.windows(2).any(|pair| pair == [flag, argument]),
                "{harness}: {:?}",
                inv.args
            );
            assert!(inv.args.iter().any(|a| a == "--literal $(no-shell); 雪"));
        }
    }
}

#[test]
fn inherited_settings_add_no_native_overrides_and_old_configs_still_load() {
    let settings: Settings = toml::from_str(include_str!("../examples/demo.toml")).unwrap();
    assert!(
        settings.workflows[0]
            .stages
            .iter()
            .all(|s| s.harness_settings.is_empty())
    );
    for harness in config::BUILTINS {
        let stage = Stage {
            harness: (*harness).into(),
            ..Stage::default()
        };
        assert!(
            model_router::harness_settings::arguments(&stage)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn invalid_native_settings_and_conflicting_raw_overrides_are_rejected() {
    let mut stage = Stage {
        harness_settings: BTreeMap::from([("sandbox".into(), "workspace-write".into())]),
        ..Stage::default()
    };
    for arguments in [
        vec!["--sandbox", "read-only"],
        vec!["--sandbox=read-only"],
        vec!["-sread-only"],
        vec!["--config", "sandbox_mode=\"read-only\""],
        vec!["--config=sandbox_mode=\"read-only\""],
        vec!["-c'sandbox_mode'=\"read-only\""],
        vec!["--dangerously-bypass-approvals-and-sandbox"],
    ] {
        stage.extra_args = arguments.into_iter().map(str::to_owned).collect();
        assert!(
            adapters::validate_options(&stage, &BTreeMap::new())
                .unwrap_err()
                .to_string()
                .contains("Extra arguments")
        );
    }
    stage.extra_args = vec!["--config".into(), "features.example=true".into()];
    adapters::validate_options(&stage, &BTreeMap::new()).unwrap();
    stage
        .harness_settings
        .insert("sandbox".into(), "made-up".into());
    assert!(adapters::validate_options(&stage, &BTreeMap::new()).is_err());
    stage
        .harness_settings
        .insert("sandbox".into(), "workspace-write".into());
    stage.harness = "claude".into();
    assert!(adapters::validate_options(&stage, &BTreeMap::new()).is_err());
    stage.harness_settings = BTreeMap::from([("permission_mode".into(), "plan".into())]);
    stage.extra_args = vec!["--permission-mode=bypassPermissions".into()];
    assert!(adapters::validate_options(&stage, &BTreeMap::new()).is_err());
    stage.harness_settings.clear();
    adapters::validate_options(&stage, &BTreeMap::new()).unwrap(); // Existing explicit raw arguments still work.
}
