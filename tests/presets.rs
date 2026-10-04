use model_router::{
    config::{self, Role},
    presets,
};
use std::{fs, process::Command};

#[test]
fn cli_presets_initialize_editable_workflows_without_overwriting_existing_files() {
    for preset in presets::WORKFLOWS {
        let dir = tempfile::tempdir().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_model-router"))
            .current_dir(dir.path())
            .args(["init", "--preset", preset.key])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let path = dir.path().join(".model-router.toml");
        let original = fs::read(&path).unwrap();
        let (settings, _) = config::load(dir.path(), Some(&path)).unwrap();
        assert_eq!(settings.workflows[0].name, preset.name);
        assert!(
            settings.workflows[0]
                .stages
                .iter()
                .filter(|s| s.role == Role::Review)
                .all(|s| !s.enabled)
        );
        let output = Command::new(env!("CARGO_BIN_EXE_model-router"))
            .current_dir(dir.path())
            .args(["init", "--preset", preset.key])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(fs::read(path).unwrap(), original);
        assert!(!dir.path().join(".model-router/runs").exists());
    }
}

#[test]
fn listing_presets_works_even_when_project_configuration_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(".model-router.toml"), "invalid TOML").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_model-router"))
        .current_dir(dir.path())
        .arg("presets")
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Security review"));
    assert!(text.contains("quick-task"));
    assert!(text.contains("reviews start disabled"));
    let output = Command::new(env!("CARGO_BIN_EXE_model-router"))
        .current_dir(dir.path())
        .args(["init", "--preset", "unknown"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(dir.path().join(".model-router.toml")).unwrap(),
        "invalid TOML"
    );
}
