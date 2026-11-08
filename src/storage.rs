use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(data)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .with_context(|| format!("Save {}", path.display()))?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
/// Creates `path` as a new file, first removing whatever file or link is
/// there, so it never writes through a link that a stage able to write the
/// directory planted.
pub fn create_fresh(path: &Path) -> Result<File> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("Replace {}", path.display())),
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("Create {}", path.display()))
}
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    atomic_write(path, &serde_json::to_vec_pretty(value)?)
}
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(&fs::read(path).with_context(|| format!("Read {}", path.display()))?)
        .with_context(|| format!("Parse {}", path.display()))
}

pub fn metadata_dir(project: &Path) -> Result<PathBuf> {
    let dir = project.join(".model-router");
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    let ignore = dir.join(".gitignore");
    // Never overwrite a user's existing rules.
    if let Ok(mut file) = OpenOptions::new().write(true).create_new(true).open(ignore) {
        file.write_all(b"*\n")?;
    }
    Ok(dir)
}

/// The second field is set once the lock was handed to another process.
pub struct FileLock(File, std::sync::atomic::AtomicBool);

impl Drop for FileLock {
    fn drop(&mut self) {
        // Unlocking would release the copy handed to another process too.
        if self.1.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        // Closing alone can leave the lock held by a descriptor inherited by
        // another thread's child process until it execs. Release it explicitly.
        let _ = FileExt::unlock(&self.0);
    }
}

/// flock locks belong to the open file description, so a descriptor sent to
/// another process (see `process::host`) carries the lock with it, never free.
#[cfg(unix)]
impl FileLock {
    /// The descriptor holding the lock, to send while this side still owns it.
    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.0.as_fd()
    }

    /// A lock received from the process that held it.
    pub fn from_fd(fd: std::os::fd::OwnedFd) -> Self {
        Self(File::from(fd), Default::default())
    }

    /// Records that `fd` was sent to another process, which now holds the
    /// lock: dropping then only closes this descriptor.
    pub fn hand_over(&self) {
        self.1.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether this is a lock on the file at `path`, as for a lock received
    /// from another process.
    pub fn is(&self, path: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        match (self.0.metadata(), fs::metadata(path)) {
            (Ok(held), Ok(file)) => held.dev() == file.dev() && held.ino() == file.ino(),
            _ => false,
        }
    }
}

pub fn lock(path: &Path) -> Result<FileLock> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.try_lock_exclusive()
        .with_context(|| format!("Another router process is using {}", path.display()))?;
    Ok(FileLock(file, Default::default()))
}

/// `lock`, retrying for up to `wait` while another holder (often a brief
/// probe such as `is_locked`) has it.
pub fn lock_wait(path: &Path, wait: std::time::Duration) -> Result<FileLock> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        match lock(path) {
            Ok(lock) => return Ok(lock),
            Err(error) if std::time::Instant::now() >= deadline => return Err(error),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}

/// Whether a router process holds the lock at `path`. Never creates the
/// file. The probe takes a shared lock on its own descriptor for an instant,
/// so it never blocks other probes; owners acquire with `lock_wait`, which
/// rides out the instant.
pub fn is_locked(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).open(path) else {
        return false;
    };
    match FileExt::try_lock_shared(&file) {
        Ok(()) => {
            let _ = FileExt::unlock(&file);
            false
        }
        Err(_) => true,
    }
}

pub fn run_dir(project: &Path, id: &str) -> Result<PathBuf> {
    ensure!(
        !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "Invalid run ID"
    );
    Ok(project.join(".model-router/runs").join(id))
}

pub fn recent_projects() -> Vec<PathBuf> {
    read_json::<Vec<PathBuf>>(&crate::config::config_dir().join("recent-projects.json"))
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.is_dir())
        .collect()
}
pub fn remember_project(project: &Path) -> Result<()> {
    let mut paths = recent_projects();
    paths.retain(|p| p != project);
    paths.insert(0, project.to_path_buf());
    paths.truncate(20);
    write_json(
        &crate::config::config_dir().join("recent-projects.json"),
        &paths,
    )
}

/// What the UI shows about the project's repository, if it is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    /// Top-level directory name.
    pub name: String,
    /// Branch, or a short commit hash when HEAD is detached.
    pub head: String,
    pub detached: bool,
    /// Modified, staged, or untracked entries in `git status`.
    pub changes: usize,
}

pub fn repo_info(project: &Path) -> Option<RepoInfo> {
    let top = git(project, &["rev-parse", "--show-toplevel"]).ok()?;
    let name = Path::new(&top)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| top.clone());
    let (head, detached) = match git(project, &["symbolic-ref", "--short", "-q", "HEAD"]) {
        Ok(branch) if !branch.is_empty() => (branch, false),
        _ => (
            git(project, &["rev-parse", "--short", "HEAD"]).unwrap_or_else(|_| "no commits".into()),
            true,
        ),
    };
    let changes = git(project, &["status", "--porcelain"])
        .map(|s| s.lines().count())
        .unwrap_or(0);
    Some(RepoInfo {
        name,
        head,
        detached,
        changes,
    })
}

pub fn git(project: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(project)
        .output()
        .context("Start Git")?;
    ensure!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned())
}

/// A worktree on a new branch from HEAD. `require_clean` refuses when the
/// project has uncommitted changes, which the worktree would not include.
pub fn create_worktree(project: &Path, id: &str, require_clean: bool) -> Result<PathBuf> {
    ensure!(
        !require_clean || git(project, &["status", "--porcelain"])?.is_empty(),
        "Worktree mode starts from committed HEAD. Commit or stash project changes, or use the current directory instead"
    );
    let base = metadata_dir(project)?.join("worktrees");
    fs::create_dir_all(&base)?;
    let path = base.join(id);
    git(
        project,
        &[
            "worktree",
            "add",
            "-b",
            &format!("model-router/{id}"),
            path.to_str().context("Worktree path must be UTF-8")?,
            "HEAD",
        ],
    )?;
    Ok(path)
}

pub fn clean_terminal(text: &str) -> String {
    let stripped = strip_ansi_escapes::strip_str(text);
    stripped
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated; see full artifact]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn dropping_lock_releases_it_even_with_an_inherited_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.lock");
        let guard = lock(&path).unwrap();
        // A concurrent fork can temporarily inherit this open file description,
        // even with close-on-exec set. A clone reproduces that without timing.
        let inherited = guard.0.try_clone().unwrap();
        assert!(lock(&path).is_err());
        drop(guard);
        let next = lock(&path).expect("The owner's lock lifetime has ended");
        assert!(lock(&path).is_err(), "The next owner still has exclusivity");
        drop(next);
        drop(inherited);
    }

    #[test]
    fn probing_a_lock_neither_creates_nor_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lock");
        assert!(!is_locked(&path));
        assert!(!path.exists(), "probing must not create the lock file");
        let guard = lock(&path).unwrap();
        assert!(is_locked(&path));
        drop(guard);
        assert!(!is_locked(&path));
        // An owner waiting briefly gets the lock once its holder lets go.
        let held = lock(&path).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(held);
        });
        assert!(lock_wait(&path, std::time::Duration::from_secs(5)).is_ok());
        release.join().unwrap();
        let _held = lock(&path).unwrap();
        assert!(lock_wait(&path, std::time::Duration::from_millis(100)).is_err());
    }
}
