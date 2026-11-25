//! Programs the TUI hands the terminal to: `$PAGER`, `$VISUAL`/`$EDITOR`,
//! and `git difftool`. Commands are argv vectors; `$PAGER` and `$EDITOR` are
//! split with shell-words, never run through a shell.
//!
//! The main loop leaves the alternate screen and raw mode, runs the program
//! with the terminal, and restores the TUI afterwards.
use anyhow::{Context, Result, bail, ensure};
use std::{
    ffi::OsString,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

/// A program waiting for the terminal.
pub(super) struct Launch {
    pub(super) argv: Vec<OsString>,
    pub(super) cwd: Option<PathBuf>,
    pub(super) env: Vec<(OsString, OsString)>,
    pub(super) env_remove: Vec<OsString>,
    /// Deleted once the program exits, e.g. the coloured diff a pager reads.
    pub(super) file: Option<tempfile::TempPath>,
    /// Deleted once the program exits, e.g. a fixed sanitized shadow `GIT_DIR`.
    pub(super) dir: Option<tempfile::TempDir>,
}

impl std::fmt::Debug for Launch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Launch")
            .field("argv", &self.argv)
            .field("cwd", &self.cwd)
            .finish()
    }
}

impl Launch {
    /// Runs the program with the terminal; returns a status-line message.
    pub(super) fn run(self) -> Result<String> {
        let (program, args) = self.argv.split_first().context("Empty command")?;
        let name = program.to_string_lossy().into_owned();
        let mut command = Command::new(program);
        command.args(args);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        for key in &self.env_remove {
            command.env_remove(key);
        }
        let status = command
            .status()
            .with_context(|| format!("Could not start {name}"))?;
        drop(self.file);
        drop(self.dir);
        Ok(if status.success() {
            format!("Returned from {name}.")
        } else {
            format!("{name} exited with {status}.")
        })
    }
}

impl super::app::App {
    /// Reports how the program ended, in the status line and in a diff
    /// dialog, which covers the status line.
    pub(super) fn finish_external(&mut self, result: Result<String>) {
        let message = match result {
            Ok(message) => message,
            Err(error) => {
                self.ui.alert = format!("{error:#}");
                self.ui.alert.clone()
            }
        };
        if let Some(super::modal::Modal::Diff(view)) = self.ui.dialog_mut() {
            view.note = crate::storage::clean_terminal(&message);
        }
        self.ui.message = message;
        self.ui.redraw = true;
    }
}

fn split(value: &str, what: &str) -> Result<Vec<OsString>> {
    let words = shell_words::split(value).with_context(|| format!("Could not parse {what}"))?;
    ensure!(!words.is_empty(), "{what} is empty");
    Ok(words.into_iter().map(OsString::from).collect())
}

/// The program's file name without directories or `.exe`.
fn program_name(argv: &[OsString]) -> String {
    let name = argv
        .first()
        .and_then(|p| Path::new(p).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.strip_suffix(".exe").unwrap_or(&name).to_owned()
}

/// `$PAGER` as argv, `less -R` when unset; `less` always gets `-R` so the
/// colours show.
pub(super) fn pager(env: Option<&str>) -> Result<Vec<OsString>> {
    let value = env.map(str::trim).filter(|v| !v.is_empty());
    let mut argv = split(value.unwrap_or("less -R"), "$PAGER")?;
    if program_name(&argv) == "less"
        && !argv.iter().skip(1).any(|arg| {
            let arg = arg.to_string_lossy();
            arg == "--RAW-CONTROL-CHARS"
                || arg == "--raw-control-chars"
                || (arg.starts_with('-')
                    && !arg.starts_with("--")
                    && (arg.contains('R') || arg.contains('r')))
        })
    {
        argv.push("-R".into());
    }
    Ok(argv)
}

/// `$VISUAL`, else `$EDITOR`, else `vi`, as argv.
pub(super) fn editor(visual: Option<&str>, editor: Option<&str>) -> Result<Vec<OsString>> {
    let value = [visual, editor]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|v| !v.is_empty())
        .unwrap_or("vi");
    split(value, "$EDITOR")
}

/// The editor's argv to open `file`, at `line` when the editor takes one:
/// `+line file` for terminal editors, `-g file:line` for VS Code and its
/// forks, `file:line` for Helix, Sublime Text, and Zed. Others get the file.
pub(super) fn open_at(mut argv: Vec<OsString>, file: &Path, line: Option<usize>) -> Vec<OsString> {
    let at = |line: usize| {
        let mut target = file.as_os_str().to_owned();
        target.push(format!(":{line}"));
        target
    };
    match (program_name(&argv).as_str(), line) {
        (
            "vi" | "vim" | "nvim" | "gvim" | "mvim" | "view" | "nano" | "pico" | "emacs"
            | "emacsclient" | "micro" | "kak" | "mg" | "joe" | "jed" | "ne",
            Some(line),
        ) => {
            argv.push(format!("+{line}").into());
            argv.push(file.into());
        }
        ("code" | "code-insiders" | "codium" | "vscodium" | "cursor" | "windsurf", Some(line)) => {
            argv.push("-g".into());
            argv.push(at(line));
        }
        ("hx" | "helix" | "subl" | "sublime_text" | "zed", Some(line)) => argv.push(at(line)),
        _ => argv.push(file.into()),
    }
    argv
}

/// Reads a variable without failing on non-UTF-8 values.
pub(super) fn var(name: &str) -> Option<String> {
    std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
}

/// Opens `file` in the user's editor, at `line` when known.
pub(super) fn edit(file: &Path, line: Option<usize>, cwd: &Path) -> Result<Launch> {
    let base = editor(var("VISUAL").as_deref(), var("EDITOR").as_deref())?;
    Ok(Launch {
        argv: open_at(base, file, line),
        cwd: Some(cwd.into()),
        env: vec![],
        env_remove: vec![],
        file: None,
        dir: None,
    })
}

/// Shows `text` in the pager from a temporary file.
pub(super) fn page_text(text: &str) -> Result<Launch> {
    let mut file = tempfile::Builder::new()
        .prefix("model-router-")
        .suffix(".diff")
        .tempfile()
        .context("Create a temporary file for the pager")?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    let path = file.into_temp_path();
    let mut argv = pager(var("PAGER").as_deref())?;
    argv.push(path.to_path_buf().into());
    Ok(Launch {
        argv,
        cwd: None,
        env: vec![],
        env_remove: vec![],
        file: Some(path),
        dir: None,
    })
}

/// Shows a file in the pager.
pub(super) fn page_file(path: &Path) -> Result<Launch> {
    ensure!(path.is_file(), "{} does not exist yet", path.display());
    let mut argv = pager(var("PAGER").as_deref())?;
    argv.push(path.into());
    Ok(Launch {
        argv,
        cwd: None,
        env: vec![],
        env_remove: vec![],
        file: None,
        dir: None,
    })
}

/// `git difftool --dir-diff base after` in `dir`, when `diff.tool` is set
/// outside the repository's own config. Repo-local difftool, mergetool, and
/// filter entries are refused, and `git difftool` runs against a fixed,
/// sanitized shadow `GIT_DIR` so concurrent `.git/config` changes cannot
/// inject drivers after this check.
pub(super) fn difftool(dir: &Path, base: &str, after: &str) -> Result<Launch> {
    let local = crate::snapshot::git_command()
        .args(["config", "--show-scope", "--name-only", "-z", "--list"])
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .output()
        .context("Start Git")?;
    ensure!(
        local.status.success(),
        "Could not verify Git difftool config scope (requires Git 2.26+): {}",
        String::from_utf8_lossy(&local.stderr).trim()
    );
    let repo_keys = repo_local_difftool_keys(&local.stdout);
    ensure!(
        repo_keys.is_empty(),
        "Refusing repo-local Git difftool/filter config ({}); configure diff.tool globally with `git config --global diff.tool <tool>`",
        repo_keys.join(", ")
    );
    let sanitized = crate::snapshot::SanitizedGitDir::new(dir)?;
    let output = sanitized
        .command(dir)
        .args(["config", "--get", "diff.tool"])
        .stdin(std::process::Stdio::null())
        .output()
        .context("Start Git")?;
    let mut launch = difftool_with(&String::from_utf8_lossy(&output.stdout), dir, base, after)?;
    let (shadow_dir, top_level) = sanitized.into_parts();
    launch.env = vec![
        (
            OsString::from("GIT_DIR"),
            shadow_dir.path().as_os_str().to_os_string(),
        ),
        (OsString::from("GIT_WORK_TREE"), top_level.into_os_string()),
    ];
    launch.env_remove = vec![OsString::from("GIT_COMMON_DIR")];
    launch.dir = Some(shadow_dir);
    Ok(launch)
}

/// Keys from `git config --show-scope --name-only -z --list` whose scope is
/// repository-controlled (`local` or `worktree`) and that control difftool,
/// mergetool, diff, or filter execution.
fn repo_local_difftool_keys(output: &[u8]) -> Vec<String> {
    let mut keys = Vec::new();
    let mut parts = output.split(|&b| b == 0);
    while let (Some(scope), Some(key)) = (parts.next(), parts.next()) {
        if scope != b"local" && scope != b"worktree" {
            continue;
        }
        if !is_difftool_risky_key(key) {
            continue;
        }
        let key_str = String::from_utf8_lossy(key).into_owned();
        if !key_str.is_empty() && !keys.iter().any(|k| k == &key_str) {
            keys.push(key_str);
        }
    }
    keys
}

fn is_difftool_risky_key(key: &[u8]) -> bool {
    let Some(pos) = key.iter().position(|&b| b == b'.') else {
        return false;
    };
    let (section, rest) = (&key[..pos], &key[pos + 1..]);
    if section.eq_ignore_ascii_case(b"difftool")
        || section.eq_ignore_ascii_case(b"mergetool")
        || section.eq_ignore_ascii_case(b"filter")
    {
        return true;
    }
    if section.eq_ignore_ascii_case(b"diff") {
        return rest.eq_ignore_ascii_case(b"tool")
            || rest.eq_ignore_ascii_case(b"guitool")
            || rest.eq_ignore_ascii_case(b"external")
            || rest.rsplit(|&b| b == b'.').next().is_some_and(|var| {
                var.eq_ignore_ascii_case(b"textconv") || var.eq_ignore_ascii_case(b"command")
            });
    }
    if section.eq_ignore_ascii_case(b"merge") {
        return rest.eq_ignore_ascii_case(b"tool")
            || rest.eq_ignore_ascii_case(b"guitool")
            || rest
                .rsplit(|&b| b == b'.')
                .next()
                .is_some_and(|var| var.eq_ignore_ascii_case(b"driver"));
    }
    false
}

fn difftool_with(tool: &str, dir: &Path, base: &str, after: &str) -> Result<Launch> {
    if tool.trim().is_empty() {
        bail!("No difftool configured; set one with `git config --global diff.tool <tool>`");
    }
    Ok(Launch {
        argv: difftool_argv(base, after),
        cwd: Some(dir.into()),
        env: vec![],
        env_remove: vec![],
        file: None,
        dir: None,
    })
}

fn difftool_argv(base: &str, after: &str) -> Vec<OsString> {
    [
        "git",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
        "difftool",
        "--dir-diff",
        base,
        after,
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(argv: &[OsString]) -> Vec<String> {
        argv.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn pager_defaults_to_less_with_colour_and_splits_words() {
        assert_eq!(strings(&pager(None).unwrap()), ["less", "-R"]);
        assert_eq!(strings(&pager(Some("  ")).unwrap()), ["less", "-R"]);
        assert_eq!(strings(&pager(Some("less")).unwrap()), ["less", "-R"]);
        assert_eq!(
            strings(&pager(Some("less -FRX")).unwrap()),
            ["less", "-FRX"]
        );
        assert_eq!(
            strings(&pager(Some("/usr/bin/less --RAW-CONTROL-CHARS")).unwrap()),
            ["/usr/bin/less", "--RAW-CONTROL-CHARS"]
        );
        assert_eq!(
            strings(&pager(Some("'/opt/my pager/bat' --paging=always")).unwrap()),
            ["/opt/my pager/bat", "--paging=always"]
        );
        // No shell runs: metacharacters stay literal words.
        assert_eq!(
            strings(&pager(Some("more; rm -rf x")).unwrap()),
            ["more;", "rm", "-rf", "x"]
        );
        assert!(pager(Some("less 'unterminated")).is_err());
    }

    #[test]
    fn editor_prefers_visual_then_editor_then_vi() {
        assert_eq!(
            strings(&editor(Some("nvim"), Some("nano")).unwrap()),
            ["nvim"]
        );
        assert_eq!(
            strings(&editor(Some(""), Some("nano -w")).unwrap()),
            ["nano", "-w"]
        );
        assert_eq!(strings(&editor(None, None).unwrap()), ["vi"]);
    }

    #[test]
    fn editor_argv_puts_the_line_where_each_editor_reads_it() {
        let file = Path::new("src/my file.rs");
        let open =
            |editor: &str, line| strings(&open_at(split(editor, "$EDITOR").unwrap(), file, line));
        for terminal in [
            "vi",
            "vim",
            "nvim",
            "nano",
            "emacs",
            "/usr/local/bin/nvim",
            "emacsclient -t",
        ] {
            let argv = open(terminal, Some(12));
            assert_eq!(
                argv[argv.len() - 2..],
                ["+12", "src/my file.rs"],
                "{terminal}"
            );
        }
        assert_eq!(
            open("emacsclient -t", Some(3)),
            ["emacsclient", "-t", "+3", "src/my file.rs"]
        );
        assert_eq!(open("hx", Some(7)), ["hx", "src/my file.rs:7"]);
        assert_eq!(open("helix", Some(7)), ["helix", "src/my file.rs:7"]);
        assert_eq!(
            open("code --wait", Some(9)),
            ["code", "--wait", "-g", "src/my file.rs:9"]
        );
        assert_eq!(
            open("cursor", Some(9)),
            ["cursor", "-g", "src/my file.rs:9"]
        );
        assert_eq!(open("subl -w", Some(2)), ["subl", "-w", "src/my file.rs:2"]);
        assert_eq!(open("ed", Some(2)), ["ed", "src/my file.rs"]);
        assert_eq!(open("vim", None), ["vim", "src/my file.rs"]);
        assert_eq!(open("code", None), ["code", "src/my file.rs"]);
    }

    #[test]
    fn difftool_compares_the_run_trees() {
        assert_eq!(
            strings(&difftool_argv("abc", "def")),
            [
                "git",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
                "difftool",
                "--dir-diff",
                "abc",
                "def",
            ]
        );
        let dir = Path::new(".");
        // With no tool set, it explains how to configure one.
        let error = difftool_with("\n", dir, "a", "b").unwrap_err().to_string();
        assert!(error.contains("git config --global diff.tool"), "{error}");
        let launch = difftool_with("meld\n", dir, "a", "b").unwrap();
        assert_eq!(launch.cwd.as_deref(), Some(dir));
    }

    #[test]
    fn difftool_refuses_repo_local_config() {
        assert_eq!(
            repo_local_difftool_keys(
                b"global\0diff.tool\0local\0difftool.meld.cmd\0worktree\0diff.tool\0local\0user.name\0"
            ),
            vec!["difftool.meld.cmd", "diff.tool"]
        );
        for (key, value) in [
            ("difftool.meld.cmd", "echo pwned"),
            ("mergetool.vimdiff.path", "/tmp/evil.sh"),
            ("mergetool.foo.cmd", "echo pwned"),
            ("filter.evil.smudge", "echo pwned"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let status = Command::new("git")
                .args(["init", "-q"])
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(status.success());
            let status = Command::new("git")
                .args(["config", "--local", key, value])
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(status.success());
            let error = difftool(dir.path(), "abc", "def").unwrap_err().to_string();
            assert!(
                error.contains("repo-local Git difftool/filter config"),
                "{key}: {error}"
            );
            assert!(error.contains(key), "{key}: {error}");
        }
    }

    #[test]
    fn launch_reports_exit_status_and_missing_programs() {
        let ok = Launch {
            argv: vec!["true".into()],
            cwd: None,
            env: vec![],
            env_remove: vec![],
            file: None,
            dir: None,
        };
        assert_eq!(ok.run().unwrap(), "Returned from true.");
        let failed = Launch {
            argv: vec!["false".into()],
            cwd: None,
            env: vec![],
            env_remove: vec![],
            file: None,
            dir: None,
        };
        assert!(failed.run().unwrap().starts_with("false exited with"));
        let missing = Launch {
            argv: vec!["model-router-no-such-program".into()],
            cwd: None,
            env: vec![],
            env_remove: vec![],
            file: None,
            dir: None,
        };
        assert!(missing.run().is_err());
    }
}
