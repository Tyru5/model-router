//! Optional OS-level filesystem sandbox around a stage's harness, independent
//! of the harness's own sandbox or permission modes (which still apply).
//!
//! `read-only` leaves the whole filesystem readable but writable only in the
//! attempt directory, the run's handoff directory, a private `/tmp`, the
//! harness's own state directories, and the stage's `sandbox_writable` paths.
//! `workspace-write` also makes the workspace and its Git directory writable.
//! Linux uses bubblewrap; macOS uses `sandbox-exec` with a generated profile.
//! When no backend works the stage fails, unless `sandbox_fallback = "warn"`.
use crate::config::{Sandbox, SandboxFallback, Stage, Workflow};
use anyhow::{Result, bail, ensure};
use serde::Serialize;
use std::{
    fs,
    path::{Component, Path, PathBuf},
    sync::OnceLock,
};

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    Bubblewrap,
    SandboxExec,
}
impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Self::Bubblewrap => "bwrap",
            Self::SandboxExec => "sandbox-exec",
        }
    }
}

/// A usable backend and its executable, or why there is none.
pub type Detected = std::result::Result<(Backend, PathBuf), String>;

/// The effective sandbox of one attempt; recorded in its `command.json`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Plan {
    pub mode: Sandbox,
    /// `None`: no backend works and the stage allowed running unsandboxed.
    pub backend: Option<Backend>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<PathBuf>,
    /// Writable paths that must exist.
    pub writable: Vec<PathBuf>,
    /// Writable when they exist: harness state and temporary directories.
    pub writable_if_present: Vec<PathBuf>,
    /// A private, empty `/tmp` (bubblewrap only).
    pub private_tmp: bool,
    pub network: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// Read-only paths re-exposed under the private `/tmp`.
    #[serde(skip)]
    readable: Vec<PathBuf>,
}

/// Paths of one attempt.
pub struct Paths<'a> {
    pub workspace: &'a Path,
    pub attempt_dir: &'a Path,
    pub handoff_dir: &'a Path,
    /// The harness executable as invoked; under a private `/tmp` it is re-exposed.
    pub program: &'a str,
}

/// The working backend on this machine; probed once per process.
pub fn detect() -> Detected {
    static DETECTED: OnceLock<Detected> = OnceLock::new();
    DETECTED.get_or_init(probe).clone()
}

#[cfg(target_os = "linux")]
fn probe() -> Detected {
    let Some(path) = crate::adapters::executable("bwrap") else {
        return Err("bubblewrap (bwrap) is not installed".into());
    };
    let output = std::process::Command::new(&path)
        .args([
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--die-with-parent",
            "--",
            "true",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{} could not run: {e}", path.display()))?;
    if output.status.success() {
        Ok((Backend::Bubblewrap, path))
    } else {
        Err(format!(
            "{} does not work here (unprivileged user namespaces may be disabled): {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(target_os = "macos")]
fn probe() -> Detected {
    let path = PathBuf::from("/usr/bin/sandbox-exec");
    let output = std::process::Command::new(&path)
        .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{} could not run: {e}", path.display()))?;
    if output.status.success() {
        Ok((Backend::SandboxExec, path))
    } else {
        Err(format!(
            "{} does not work here: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn probe() -> Detected {
    Err("no sandbox backend exists for this operating system".into())
}

/// One line for `doctor`.
pub fn doctor_line() -> String {
    match detect() {
        Ok((backend, path)) => {
            let version = std::process::Command::new(&path)
                .arg("--version")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                .filter(|v| !v.is_empty());
            format!(
                "Sandbox: {} available at {}{}",
                backend.label(),
                path.display(),
                version.map(|v| format!(" ({v})")).unwrap_or_default()
            )
        }
        Err(reason) => format!(
            "Sandbox: unavailable ({reason}); stages with sandbox enabled will refuse to start"
        ),
    }
}

/// The attempt's sandbox, or `None` when it is off. Fails when the mode
/// needs a backend that does not work, unless the stage falls back.
pub fn plan(workflow: &Workflow, stage: &Stage, paths: &Paths<'_>) -> Result<Option<Plan>> {
    // Probing spawns bwrap, so stages without a sandbox skip it.
    if stage.effective_sandbox(workflow) == Sandbox::Off {
        return Ok(None);
    }
    plan_with(workflow, stage, paths, detect(), home().as_deref())
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

pub fn plan_with(
    workflow: &Workflow,
    stage: &Stage,
    paths: &Paths<'_>,
    detected: Detected,
    home: Option<&Path>,
) -> Result<Option<Plan>> {
    let mode = stage.effective_sandbox(workflow);
    if mode == Sandbox::Off {
        return Ok(None);
    }
    let (backend, program, warning) = match detected {
        Ok((backend, program)) => (Some(backend), Some(program), None),
        Err(reason) if stage.sandbox_fallback == SandboxFallback::Warn => (
            None,
            None,
            Some(format!(
                "model-router: {} sandbox unavailable ({reason}); running stage {} unsandboxed because sandbox_fallback = \"warn\"",
                mode.label(),
                stage.id
            )),
        ),
        Err(reason) => bail!(
            "Stage {} needs a {} sandbox, but {reason}. Install bubblewrap (Linux), set the stage's sandbox to \"off\", or set sandbox_fallback = \"warn\" to run it unsandboxed.",
            stage.id,
            mode.label()
        ),
    };
    let private_tmp = backend == Some(Backend::Bubblewrap);
    let mut writable = vec![canonical(paths.attempt_dir), canonical(paths.handoff_dir)];
    let mut readable = vec![canonical(paths.workspace)];
    let git = git_dirs(paths.workspace);
    if mode == Sandbox::WorkspaceWrite {
        writable.push(canonical(paths.workspace));
        writable.extend(git);
    } else {
        readable.extend(git);
    }
    for extra in &stage.sandbox_writable {
        let path = expand(extra, home)?;
        ensure!(
            path.exists(),
            "Stage {} sandbox_writable path {} does not exist",
            stage.id,
            path.display()
        );
        writable.push(canonical(&path));
    }
    let mut optional: Vec<PathBuf> = vec![];
    if let Some(home) = home {
        optional.extend(
            harness_dirs(&stage.harness)
                .into_iter()
                .map(|d| home.join(d)),
        );
    }
    if let Some(tmp) = std::env::var_os("TMPDIR").filter(|t| !t.is_empty()) {
        let tmp = canonical(Path::new(&tmp));
        // The private /tmp already covers a TMPDIR under it.
        if !(private_tmp && tmp.starts_with("/tmp")) {
            optional.push(tmp);
        }
    }
    if backend == Some(Backend::SandboxExec) {
        optional.extend(["/private/tmp", "/private/var/folders"].map(PathBuf::from));
    }
    let mut optional: Vec<PathBuf> = optional
        .iter()
        .filter(|p| p.is_absolute())
        .map(|p| canonical(p))
        .collect();
    if let Some(path) = crate::adapters::executable(paths.program) {
        if let Some(parent) = canonical(&path).parent() {
            readable.push(parent.to_path_buf());
        }
    }
    tidy(&mut writable);
    tidy(&mut optional);
    optional.retain(|p| !writable.contains(p) && p != Path::new("/tmp"));
    // Only paths hidden by the private /tmp need re-exposing.
    let all: Vec<PathBuf> = writable.iter().chain(&optional).cloned().collect();
    readable
        .retain(|p| private_tmp && p.starts_with("/tmp") && !all.iter().any(|w| p.starts_with(w)));
    tidy(&mut readable);
    Ok(Some(Plan {
        mode,
        backend,
        program,
        writable,
        writable_if_present: optional,
        private_tmp,
        network: stage.sandbox_network,
        warning,
        readable,
    }))
}

fn tidy(paths: &mut Vec<PathBuf>) {
    paths.sort_by_key(|p| (p.components().count(), p.clone()));
    paths.dedup();
}

/// State and cache directories a harness writes, relative to the home
/// directory. Missing ones are skipped.
pub fn harness_dirs(harness: &str) -> Vec<String> {
    let mut dirs = vec![
        format!(".{harness}"),
        format!(".config/{harness}"),
        format!(".local/share/{harness}"),
        ".cache".into(),
        ".local/state".into(),
    ];
    let extra = crate::harness::get(harness).map_or(&[][..], |spec| spec.state_dirs);
    dirs.extend(extra.iter().map(|d| (*d).to_owned()));
    dirs
}

/// `~/x` against `home`; other paths must be absolute.
fn expand(path: &str, home: Option<&Path>) -> Result<PathBuf> {
    let expanded = match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        (Some(_), None) => bail!("Cannot expand {path}: HOME is not set"),
        _ if path == "~" => home
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow::anyhow!("Cannot expand ~: HOME is not set"))?,
        _ => PathBuf::from(path),
    };
    ensure!(
        expanded.is_absolute(),
        "sandbox_writable paths must be absolute or start with ~/: {path}"
    );
    Ok(expanded)
}

/// `path` with symlinks resolved in its longest existing prefix, so a
/// missing directory still maps to where it would be created.
pub fn canonical(path: &Path) -> PathBuf {
    let mut missing = vec![];
    let mut current = path.to_path_buf();
    loop {
        if let Ok(real) = fs::canonicalize(&current) {
            let mut out = real;
            for part in missing.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (current.file_name(), current.parent()) {
            (Some(name), Some(parent)) => {
                missing.push(name.to_os_string());
                current = parent.to_path_buf();
            }
            _ => return lexical(path),
        }
    }
}

fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// A linked worktree's Git directory and the repository's common one.
pub fn git_dirs(workspace: &Path) -> Vec<PathBuf> {
    let dot_git = workspace.join(".git");
    if !dot_git.is_file() {
        return vec![];
    }
    let Some(gitdir) = fs::read_to_string(&dot_git).ok().and_then(|text| {
        text.lines()
            .find_map(|l| l.strip_prefix("gitdir:"))
            .map(|p| workspace.join(p.trim()))
    }) else {
        return vec![];
    };
    let gitdir = canonical(&gitdir);
    let mut dirs = vec![gitdir.clone()];
    if let Ok(common) = fs::read_to_string(gitdir.join("commondir")) {
        dirs.push(canonical(&gitdir.join(common.trim())));
    }
    dirs
}

impl Plan {
    /// The program and arguments that run `program args` inside the sandbox.
    pub fn wrap(&self, program: &str, args: &[String]) -> (String, Vec<String>) {
        let (Some(backend), Some(sandbox)) = (self.backend, &self.program) else {
            return (program.to_owned(), args.to_vec());
        };
        let mut argv = match backend {
            Backend::Bubblewrap => self.bwrap_args(),
            Backend::SandboxExec => vec!["-p".into(), self.profile()],
        };
        argv.push(program.to_owned());
        argv.extend(args.iter().cloned());
        (sandbox.display().to_string(), argv)
    }

    /// bubblewrap options up to and including `--`. Later mounts win, so
    /// deeper paths come after their parents.
    pub fn bwrap_args(&self) -> Vec<String> {
        let mut args: Vec<String> = ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]
            .map(String::from)
            .to_vec();
        if self.private_tmp {
            args.extend(["--tmpfs".into(), "/tmp".into()]);
        }
        if !self.network {
            args.push("--unshare-net".into());
        }
        let mut binds: Vec<(&PathBuf, &str)> = self
            .readable
            .iter()
            .map(|p| (p, "--ro-bind"))
            .chain(self.writable.iter().map(|p| (p, "--bind")))
            .chain(self.writable_if_present.iter().map(|p| (p, "--bind-try")))
            .collect();
        binds.sort_by_key(|(p, _)| p.components().count());
        for (path, flag) in binds {
            let path = path.display().to_string();
            args.extend([flag.to_owned(), path.clone(), path]);
        }
        args.extend(["--die-with-parent".into(), "--".into()]);
        args
    }

    /// A Seatbelt profile denying writes outside the writable paths.
    pub fn profile(&self) -> String {
        let mut text = String::from(
            "(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*\n    (subpath \"/dev\")",
        );
        for path in self.writable.iter().chain(&self.writable_if_present) {
            text.push_str(&format!(
                "\n    (subpath {})",
                sbpl_string(&path.display().to_string())
            ));
        }
        text.push_str(")\n");
        if !self.network {
            text.push_str("(deny network*)\n");
        }
        text
    }

    /// One line for previews and the stage panel.
    pub fn describe(&self) -> String {
        let mut text = format!(
            "{} via {}",
            self.mode.label(),
            self.backend.map_or("nothing (unsandboxed)", Backend::label)
        );
        if !self.network {
            text.push_str(", no network");
        }
        text
    }
}

fn sbpl_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The sandbox line `preview` shows for a stage, or `None` when it is off.
pub fn preview(workflow: &Workflow, stage: &Stage) -> Option<String> {
    let mode = stage.effective_sandbox(workflow);
    if mode == Sandbox::Off {
        return None;
    }
    let backend = match detect() {
        Ok((backend, _)) => backend.label().to_owned(),
        Err(reason) if stage.sandbox_fallback == SandboxFallback::Warn => {
            format!("nothing: {reason}; runs unsandboxed")
        }
        Err(reason) => format!("nothing: {reason}; the stage will refuse to start"),
    };
    let mut writable = vec!["<attempt>", "<handoff dir>"];
    if mode == Sandbox::WorkspaceWrite {
        writable.insert(0, "<workspace> + .git");
    }
    let mut text = format!(
        "sandbox: {} via {backend}; writable: {}, private /tmp, ~/{{{}}}",
        mode.label(),
        writable.join(", "),
        harness_dirs(&stage.harness).join(",")
    );
    for extra in &stage.sandbox_writable {
        text.push_str(&format!(", {extra}"));
    }
    if !stage.sandbox_network {
        text.push_str("; no network");
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Role;

    fn workflow(default: Sandbox) -> Workflow {
        Workflow {
            name: "w".into(),
            max_repair_passes: 0,
            sandbox_default: default,
            budget: Default::default(),
            stages: vec![],
        }
    }

    fn bwrap() -> Detected {
        Ok((Backend::Bubblewrap, PathBuf::from("/usr/bin/bwrap")))
    }

    struct Dirs {
        _temp: tempfile::TempDir,
        root: PathBuf,
        workspace: PathBuf,
        attempt: PathBuf,
        handoff: PathBuf,
        home: PathBuf,
    }
    fn dirs() -> Dirs {
        let temp = tempfile::Builder::new()
            .prefix("sandbox test ")
            .tempdir()
            .unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("work space");
        let attempt = workspace.join(".model-router/runs/r/01-a/attempt-001");
        let handoff = workspace.join(".model-router/handoffs/r");
        let home = root.join("home");
        for dir in [&attempt, &handoff, &home.join(".codex")] {
            fs::create_dir_all(dir).unwrap();
        }
        Dirs {
            _temp: temp,
            root,
            workspace,
            attempt,
            handoff,
            home,
        }
    }
    fn plan_for(
        d: &Dirs,
        stage: &Stage,
        default: Sandbox,
        detected: Detected,
    ) -> Result<Option<Plan>> {
        plan_with(
            &workflow(default),
            stage,
            &Paths {
                workspace: &d.workspace,
                attempt_dir: &d.attempt,
                handoff_dir: &d.handoff,
                program: "sh",
            },
            detected,
            Some(&d.home),
        )
    }

    #[test]
    fn modes_resolve_from_stage_then_workflow_then_role() {
        let research = Stage {
            role: Role::Research,
            ..Stage::default()
        };
        let implement = Stage {
            role: Role::Implementation,
            ..Stage::default()
        };
        assert_eq!(
            research.effective_sandbox(&workflow(Sandbox::Off)),
            Sandbox::Off
        );
        assert_eq!(
            research.effective_sandbox(&workflow(Sandbox::ByRole)),
            Sandbox::ReadOnly
        );
        assert_eq!(
            implement.effective_sandbox(&workflow(Sandbox::ByRole)),
            Sandbox::WorkspaceWrite
        );
        let pinned = Stage {
            sandbox: Some(Sandbox::Off),
            ..research.clone()
        };
        assert_eq!(
            pinned.effective_sandbox(&workflow(Sandbox::ReadOnly)),
            Sandbox::Off
        );
    }

    #[test]
    fn read_only_bwrap_argv_binds_only_state_paths_writable() {
        let d = dirs();
        let stage = Stage {
            sandbox: Some(Sandbox::ReadOnly),
            sandbox_network: false,
            ..Stage::default()
        };
        let plan = plan_for(&d, &stage, Sandbox::Off, bwrap())
            .unwrap()
            .unwrap();
        assert_eq!(plan.writable, vec![d.handoff.clone(), d.attempt.clone()]);
        assert!(plan.writable_if_present.contains(&d.home.join(".codex")));
        assert!(plan.writable_if_present.contains(&d.home.join(".cache")));
        let (program, argv) = plan.wrap("codex", &["exec".into(), "a b".into()]);
        assert_eq!(program, "/usr/bin/bwrap");
        assert_eq!(
            &argv[..9],
            [
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--tmpfs",
                "/tmp"
            ]
        );
        assert!(argv.contains(&"--unshare-net".to_owned()));
        let pair = |flag: &str, path: &Path| {
            let path = path.display().to_string();
            argv.windows(3)
                .any(|w| w[0] == flag && w[1] == path && w[2] == path)
        };
        assert!(pair("--bind", &d.attempt) && pair("--bind", &d.handoff));
        assert!(!pair("--bind", &d.workspace));
        // The workspace lives under /tmp here, so the private /tmp re-exposes it read-only.
        if d.workspace.starts_with("/tmp") {
            assert!(pair("--ro-bind", &d.workspace));
            let ro = argv
                .iter()
                .position(|a| *a == d.workspace.display().to_string())
                .unwrap();
            let rw = argv
                .iter()
                .position(|a| *a == d.attempt.display().to_string())
                .unwrap();
            assert!(ro < rw, "parents must mount before nested writable paths");
        }
        assert!(pair("--bind-try", &d.home.join(".codex")));
        let tail = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(
            &argv[tail - 1..],
            ["--die-with-parent", "--", "codex", "exec", "a b"]
        );
    }

    #[test]
    fn workspace_write_includes_a_linked_worktrees_git_dirs() {
        let d = dirs();
        let common = d.root.join("main repo/.git");
        let gitdir = common.join("worktrees/ws");
        fs::create_dir_all(&gitdir).unwrap();
        fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        fs::write(
            d.workspace.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();
        let stage = Stage {
            role: Role::Implementation,
            ..Stage::default()
        };
        let plan = plan_for(&d, &stage, Sandbox::ByRole, bwrap())
            .unwrap()
            .unwrap();
        assert_eq!(plan.mode, Sandbox::WorkspaceWrite);
        for path in [&d.workspace, &gitdir, &common] {
            assert!(
                plan.writable.contains(path),
                "{path:?} in {:?}",
                plan.writable
            );
        }
        let (_, argv) = plan.wrap("x", &[]);
        let at = |p: &Path| {
            argv.iter()
                .position(|a| *a == p.display().to_string())
                .unwrap()
        };
        assert!(at(&d.workspace) < at(&d.attempt));
    }

    #[test]
    fn paths_resolve_symlinks_and_missing_tails() {
        let d = dirs();
        let link = d.root.join("link to work");
        std::os::unix::fs::symlink(&d.workspace, &link).unwrap();
        assert_eq!(canonical(&link), d.workspace);
        assert_eq!(
            canonical(&link.join("new dir")),
            d.workspace.join("new dir")
        );
        let stage = Stage {
            sandbox: Some(Sandbox::ReadOnly),
            sandbox_writable: vec!["~/.codex".into(), link.display().to_string()],
            ..Stage::default()
        };
        let plan = plan_for(&d, &stage, Sandbox::Off, bwrap())
            .unwrap()
            .unwrap();
        assert!(plan.writable.contains(&d.home.join(".codex")));
        assert!(plan.writable.contains(&d.workspace));
        assert!(!plan.writable_if_present.contains(&d.home.join(".codex")));
        for bad in ["relative/path", "~/missing"] {
            let stage = Stage {
                sandbox: Some(Sandbox::ReadOnly),
                sandbox_writable: vec![bad.into()],
                ..Stage::default()
            };
            assert!(
                plan_for(&d, &stage, Sandbox::Off, bwrap()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn unavailable_backends_refuse_unless_the_stage_falls_back() {
        let d = dirs();
        let stage = Stage {
            sandbox: Some(Sandbox::ReadOnly),
            ..Stage::default()
        };
        let error = plan_for(&d, &stage, Sandbox::Off, Err("no bwrap".into())).unwrap_err();
        assert!(
            error.to_string().contains("no bwrap")
                && error.to_string().contains("sandbox_fallback")
        );
        let warn = Stage {
            sandbox_fallback: SandboxFallback::Warn,
            ..stage.clone()
        };
        let plan = plan_for(&d, &warn, Sandbox::Off, Err("no bwrap".into()))
            .unwrap()
            .unwrap();
        assert!(plan.backend.is_none() && plan.warning.is_some());
        assert_eq!(
            plan.wrap("codex", &["x".into()]),
            ("codex".into(), vec!["x".into()])
        );
        let off = Stage::default();
        assert!(
            plan_for(&d, &off, Sandbox::Off, Err("no bwrap".into()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn seatbelt_profile_denies_writes_outside_quoted_subpaths() {
        let d = dirs();
        let stage = Stage {
            sandbox: Some(Sandbox::ReadOnly),
            sandbox_network: false,
            ..Stage::default()
        };
        let mut plan = plan_for(
            &d,
            &stage,
            Sandbox::Off,
            Ok((Backend::SandboxExec, PathBuf::from("/usr/bin/sandbox-exec"))),
        )
        .unwrap()
        .unwrap();
        assert!(!plan.private_tmp);
        plan.writable.push(PathBuf::from("/odd \"quoted\\\" dir"));
        let profile = plan.profile();
        assert!(profile.starts_with("(version 1)\n(allow default)\n(deny file-write*)"));
        assert!(profile.contains(&format!("(subpath \"{}\")", d.attempt.display())));
        assert!(
            profile.contains(r#"(subpath "/odd \"quoted\\\" dir")"#),
            "{profile}"
        );
        assert!(profile.contains("(subpath \"/private/tmp\")"));
        assert!(profile.contains("(deny network*)"));
        assert!(!profile.contains(&format!("(subpath \"{}\")", d.workspace.display())));
        let (program, argv) = plan.wrap("claude", &["-p".into()]);
        assert_eq!(program, "/usr/bin/sandbox-exec");
        assert_eq!(argv[0], "-p");
        assert_eq!(argv[1], profile);
        assert_eq!(&argv[2..], ["claude", "-p"]);
    }
}
