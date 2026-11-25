use anyhow::{Context, Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    borrow::Cow,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    atomic_write_inner(path, data, true)
}

/// Atomically writes `data` to `path` without fsyncing the file or parent
/// directory: for regenerable artifacts such as prompts and diff caches.
pub fn atomic_write_fast(path: &Path, data: &[u8]) -> Result<()> {
    atomic_write_inner(path, data, false)
}

fn atomic_write_inner(path: &Path, data: &[u8], sync: bool) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_file() {
            use std::os::unix::fs::PermissionsExt;
            let _ = temp
                .as_file()
                .set_permissions(fs::Permissions::from_mode(meta.permissions().mode()));
        }
    }
    temp.write_all(data)?;
    if sync {
        temp.as_file().sync_all()?;
    }
    temp.persist(path)
        .with_context(|| format!("Save {}", path.display()))?;
    #[cfg(unix)]
    if sync {
        if let Ok(dir) = File::open(parent) {
            if let Err(error) = dir.sync_all() {
                let unsupported = matches!(
                    error.kind(),
                    ErrorKind::InvalidInput
                        | ErrorKind::PermissionDenied
                        | ErrorKind::ReadOnlyFilesystem
                ) || error.raw_os_error() == Some(libc::EBADF);
                if !unsupported {
                    return Err(error.into());
                }
            }
        }
    }
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
pub fn write_json_fast<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    atomic_write_fast(path, &serde_json::to_vec_pretty(value)?)
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

fn flock(file: &File, operation: libc::c_int) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    loop {
        // SAFETY: `file` owns a valid open file descriptor for the duration of the call.
        let rc = unsafe { libc::flock(file.as_raw_fd(), operation) };
        if rc == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
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
        let _ = flock(&self.0, libc::LOCK_UN);
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
    flock(&file, libc::LOCK_EX | libc::LOCK_NB)
        .with_context(|| format!("Another router process is using {}", path.display()))?;
    Ok(FileLock(file, Default::default()))
}

/// `lock`, retrying for up to `wait` while another holder (often a brief
/// probe such as `is_locked`) has it. Permanent open/lock errors return at once.
pub fn lock_wait(path: &Path, wait: std::time::Duration) -> Result<FileLock> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let deadline = std::time::Instant::now() + wait;
    loop {
        match flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => return Ok(FileLock(file, Default::default())),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return Err(error).with_context(|| {
                        format!("Another router process is using {}", path.display())
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("Lock {}", path.display()));
            }
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
    match flock(&file, libc::LOCK_SH | libc::LOCK_NB) {
        Ok(()) => {
            let _ = flock(&file, libc::LOCK_UN);
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
    /// Why `git status` could not run with the sanitized Git config (a
    /// partial clone, for example); `changes` is then 0.
    pub status_error: Option<String>,
}

pub fn repo_info(project: &Path) -> Option<RepoInfo> {
    // Detection reads no working-tree file, so it needs no sanitized config
    // and still names a checkout that config refuses.
    let top = git(project, &["rev-parse", "--show-toplevel"]).ok()?;
    let name = Path::new(&top)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| top.clone());
    let status = match git_readonly(
        project,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "--ignore-submodules=dirty",
        ],
    ) {
        Ok(status) => status,
        Err(err) => {
            let (head, detached) = match git(project, &["symbolic-ref", "--short", "-q", "HEAD"]) {
                Ok(branch) => (branch, false),
                Err(_) => (
                    git(project, &["rev-parse", "--short", "HEAD"])
                        .unwrap_or_else(|_| "no commits".into()),
                    true,
                ),
            };
            return Some(RepoInfo {
                name,
                head,
                detached,
                changes: 0,
                status_error: Some(format!("{err:#}")),
            });
        }
    };
    let mut oid = None;
    let mut branch_head = None;
    let mut changes = 0usize;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("# branch.oid ") {
            oid = Some(rest.trim());
        } else if let Some(rest) = line.strip_prefix("# branch.head ") {
            branch_head = Some(rest.trim());
        } else if !line.starts_with('#') && !line.is_empty() {
            changes += 1;
        }
    }
    let (head, detached) = match branch_head {
        Some("(detached)") | None => {
            let short = oid
                .filter(|o| *o != "(initial)")
                .map(|o| o.get(..7).unwrap_or(o).to_owned())
                .unwrap_or_else(|| "no commits".into());
            (short, true)
        }
        Some(branch) => (branch.to_owned(), false),
    };
    Some(RepoInfo {
        name,
        head,
        detached,
        changes,
        status_error: None,
    })
}

/// Opens `relative` inside `base` for reading, one component at a time
/// with `O_NOFOLLOW`, and requires a regular file: a link swapped in for any
/// directory on the way cannot redirect the read.
pub fn open_beneath(base: &Path, relative: &Path) -> Result<File> {
    use std::ffi::CString;
    use std::os::unix::{ffi::OsStrExt, io::FromRawFd};
    let parts: Vec<&std::ffi::OsStr> = relative
        .components()
        .map(|c| match c {
            std::path::Component::Normal(part) => Ok(part),
            _ => Err(anyhow::anyhow!(
                "{} is not a plain relative path",
                relative.display()
            )),
        })
        .collect::<Result<_>>()?;
    let (last, dirs) = parts
        .split_last()
        .with_context(|| format!("{} names no file", relative.display()))?;
    let base_c = CString::new(base.as_os_str().as_bytes())?;
    // SAFETY: a valid NUL-terminated path; the result is checked.
    let mut fd = unsafe {
        libc::open(
            base_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    ensure!(
        fd >= 0,
        "Open {}: {}",
        base.display(),
        std::io::Error::last_os_error()
    );
    let close = |fd: i32| {
        // SAFETY: `fd` was opened above and is closed once.
        unsafe { libc::close(fd) };
    };
    for dir in dirs {
        let name = CString::new(dir.as_bytes())?;
        // SAFETY: `fd` is an open directory; `name` is NUL-terminated.
        let next = unsafe {
            libc::openat(
                fd,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        let error = std::io::Error::last_os_error();
        close(fd);
        ensure!(
            next >= 0,
            "Open {} in {}: {error}",
            relative.display(),
            base.display()
        );
        fd = next;
    }
    let name = CString::new(last.as_bytes())?;
    // SAFETY: as above.
    let file = unsafe {
        libc::openat(
            fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    let error = std::io::Error::last_os_error();
    close(fd);
    ensure!(
        file >= 0,
        "Open {} in {}: {error}",
        relative.display(),
        base.display()
    );
    // SAFETY: `file` is an open descriptor this File now owns.
    let file = unsafe { File::from_raw_fd(file) };
    ensure!(
        file.metadata()?.is_file(),
        "{} is not a regular file",
        relative.display()
    );
    Ok(file)
}

pub fn git(project: &Path, args: &[&str]) -> Result<String> {
    let mut command = crate::snapshot::git_command();
    command.args(args).current_dir(project);
    let output = command.output().context("Start Git")?;
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

pub fn git_readonly(project: &Path, args: &[&str]) -> Result<String> {
    let sanitized = crate::snapshot::SanitizedGitDir::new(project)?;
    let mut command = sanitized.command(project);
    command.args(args).env("GIT_OPTIONAL_LOCKS", "0");
    let output = command.output().context("Start Git")?;
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
    if require_clean {
        ensure_clean_checkout(project)?;
    }
    // Resolve HEAD to a fixed commit SHA once so the filter check and
    // `worktree add` operate on the exact same commit even if HEAD moves.
    let head = git(project, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let base = metadata_dir(project)?.join("worktrees");
    fs::create_dir_all(&base)?;
    let path = base.join(id);
    let path_str = path.to_str().context("Worktree path must be UTF-8")?;
    let branch = format!("model-router/{id}");
    git(
        project,
        &[
            "worktree",
            "add",
            "--no-checkout",
            "-b",
            &branch,
            path_str,
            &head,
        ],
    )?;
    if let Err(err) = crate::snapshot::populate_worktree(&path, &head) {
        let _ = git(project, &["worktree", "remove", "--force", path_str]);
        let _ = fs::remove_dir_all(&path);
        let _ = git(project, &["worktree", "prune"]);
        let _ = git(project, &["branch", "-D", &branch]);
        return Err(err);
    }
    Ok(path)
}

/// Paths of the checkout's populated submodules (gitlinks whose directory
/// has a `.git` entry, whether a file or an old-style directory).
fn populated_submodules(sanitized: &crate::snapshot::SanitizedGitDir) -> Result<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStrExt;
    let top = sanitized.top_level();
    let output = sanitized
        .command(top)
        .args(["ls-files", "--stage", "-z"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .context("Start Git")?;
    ensure!(
        output.status.success(),
        "git ls-files --stage: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let mut found = Vec::new();
    // Entries are `<mode> <object> <stage>\t<path>`; 160000 is a gitlink.
    for entry in output.stdout.split(|&b| b == 0) {
        let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
            continue;
        };
        if !entry.starts_with(b"160000 ") {
            continue;
        }
        let path = top.join(std::ffi::OsStr::from_bytes(&entry[tab + 1..]));
        if fs::symlink_metadata(path.join(".git")).is_ok() {
            found.push(path);
        }
    }
    Ok(found)
}

/// Whether the checkout at `dir` has a populated submodule.
pub(crate) fn has_populated_submodules(dir: &Path) -> Result<bool> {
    let sanitized = crate::snapshot::SanitizedGitDir::new(dir)?;
    Ok(!populated_submodules(&sanitized)?.is_empty())
}

/// `status --ignore-submodules=dirty` skips submodule work trees, so each
/// populated submodule is checked with its own sanitized Git directory.
fn ensure_clean_submodules(sanitized: &crate::snapshot::SanitizedGitDir) -> Result<()> {
    for path in populated_submodules(sanitized)? {
        ensure_clean_checkout(&path)
            .with_context(|| format!("Submodule {} is not clean", path.display()))?;
    }
    Ok(())
}

fn ensure_clean_checkout(project: &Path) -> Result<()> {
    let sanitized = crate::snapshot::SanitizedGitDir::new(project)?;
    let output = sanitized
        .command(project)
        // `dirty`: never run status inside a submodule, whose own config the
        // shadow GIT_DIR does not cover and a stage may have written.
        .args([
            "status",
            "--porcelain",
            "-z",
            // Whatever status.showUntrackedFiles says.
            "--untracked-files=normal",
            "--ignore-submodules=dirty",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .context("Start Git")?;
    ensure!(
        output.status.success(),
        "git status --porcelain -z: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    if output.stdout.is_empty() {
        return ensure_clean_submodules(&sanitized);
    }
    // If any reported dirty path uses a suppressed repo-local clean filter,
    // Git's sanitized comparison sees raw working-tree bytes against filtered
    // index blobs and cannot prove whether the file actually changed.
    let drivers = sanitized.local_clean_filters();
    if !drivers.is_empty() {
        let mut stdin_paths = Vec::new();
        let mut entries = output.stdout.split(|&b| b == 0);
        while let Some(entry) = entries.next() {
            if entry.len() <= 3 {
                continue;
            }
            let x = entry[0];
            let y = entry[1];
            stdin_paths.extend_from_slice(&entry[3..]);
            stdin_paths.push(0);
            if matches!(x, b'R' | b'C') || matches!(y, b'R' | b'C') {
                let _ = entries.next();
            }
        }
        if let Some((path, driver)) = crate::snapshot::find_suppressed_filter_path(
            &sanitized,
            sanitized.top_level(),
            &stdin_paths,
            None,
            drivers,
        )? {
            anyhow::bail!(
                "Cannot verify whether {path} is clean: repo-local Git clean filter `{driver}` is disabled for safety. Configure the filter globally or use the current directory instead"
            );
        }
    }
    anyhow::bail!(
        "Worktree mode starts from committed HEAD. Commit or stash project changes, or use the current directory instead"
    );
}

pub fn clean_terminal(text: &str) -> String {
    let bytes = text.as_bytes();
    if !bytes
        .iter()
        .any(|&b| (b < 0x20 && b != b'\n' && b != b'\t') || b == 0x7f || b == 0xc2)
    {
        return text.to_owned();
    }
    let stripped: Cow<'_, str> = if bytes.iter().any(|&b| b == 0x1b || b == 0xc2) {
        Cow::Owned(strip_ansi_escapes::strip_str(text))
    } else {
        Cow::Borrowed(text)
    };
    let mut out = String::with_capacity(stripped.len());
    for c in stripped.chars() {
        if !c.is_control() || c == '\n' || c == '\t' {
            out.push(c);
        }
    }
    out
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
    use std::process::Command;

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
        // Permanent open errors (like locking a directory) fail immediately.
        let started = std::time::Instant::now();
        assert!(lock_wait(dir.path(), std::time::Duration::from_secs(5)).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    /// Waits up to 5 s for `done`. A child another test forks holds copies
    /// of this test's descriptors, and with them its locks, until it execs.
    #[cfg(unix)]
    fn eventually(done: impl Fn() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        true
    }

    #[cfg(unix)]
    #[test]
    fn handed_over_lock_stays_held_after_drop_and_shared_probes_coexist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.lock");
        let guard = lock(&path).unwrap();
        let peer = guard.0.try_clone().unwrap();
        guard.hand_over();
        drop(guard);
        assert!(is_locked(&path));
        assert!(lock(&path).is_err());
        drop(peer);
        assert!(eventually(|| !is_locked(&path)));

        let probe_a = OpenOptions::new().read(true).open(&path).unwrap();
        let probe_b = OpenOptions::new().read(true).open(&path).unwrap();
        flock(&probe_a, libc::LOCK_SH | libc::LOCK_NB).unwrap();
        flock(&probe_b, libc::LOCK_SH | libc::LOCK_NB).unwrap();
        assert!(lock(&path).is_err());
        let _ = flock(&probe_a, libc::LOCK_UN);
        let _ = flock(&probe_b, libc::LOCK_UN);
        drop(probe_a);
        drop(probe_b);
        assert!(eventually(|| lock(&path).is_ok()));
    }

    #[test]
    fn repo_info_handles_initial_branch_detached_and_dirty_states() {
        let dir = tempfile::tempdir().unwrap();
        assert!(repo_info(dir.path()).is_none());
        let run_git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(status.success(), "{args:?}");
        };
        run_git(&["init", "-b", "main"]);
        let empty = repo_info(dir.path()).unwrap();
        assert_eq!(empty.head, "main");
        assert!(!empty.detached);
        assert_eq!(empty.changes, 0);

        fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let dirty = repo_info(dir.path()).unwrap();
        assert_eq!(dirty.changes, 1);

        run_git(&["config", "user.name", "Test"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["add", "a.txt"]);
        run_git(&["commit", "-m", "init"]);
        run_git(&["checkout", "--detach"]);
        let detached = repo_info(dir.path()).unwrap();
        assert!(detached.detached);
        assert_eq!(detached.changes, 0);
        assert!(!detached.head.is_empty() && detached.head != "no commits");
    }
}
