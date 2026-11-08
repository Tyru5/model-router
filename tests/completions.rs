use std::process::Command;

fn completions(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_model-router"))
        .arg("completions")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn cli_completions_cover_every_shell_and_name_the_subcommands() {
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let output = completions(&[shell]);
        assert!(
            output.status.success(),
            "{shell}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let script = String::from_utf8(output.stdout).unwrap();
        for word in [
            "model-router",
            "completions",
            "doctor",
            "resume",
            "task-file",
        ] {
            assert!(script.contains(word), "{shell} script lacks {word:?}");
        }
    }
}

#[test]
fn cli_completions_can_target_the_mr_alias_outside_a_project() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_model-router"))
        .current_dir(dir.path())
        .args(["completions", "zsh", "--bin", "mr"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let script = String::from_utf8(output.stdout).unwrap();
    assert!(
        script.contains("#compdef mr"),
        "{}",
        &script[..script.len().min(200)]
    );
    assert!(!script.contains("#compdef model-router"));
    // No config or run state is created just to print a script.
    assert!(!dir.path().join(".model-router").exists());
    assert!(!dir.path().join(".model-router.toml").exists());
}

#[test]
fn cli_completions_reject_a_bin_name_that_is_not_a_plain_word() {
    let output = completions(&["bash", "--bin", "x y"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--bin must be a plain command name"));
}
