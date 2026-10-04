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

pub struct FileLock(File);

impl Drop for FileLock {
    fn drop(&mut self) {
        // Closing alone can leave the lock held by a descriptor inherited by
        // another thread's child process until it execs. Release it explicitly.
        let _ = FileExt::unlock(&self.0);
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
    Ok(FileLock(file))
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

pub fn create_worktree(project: &Path, id: &str) -> Result<PathBuf> {
    ensure!(
        git(project, &["status", "--porcelain"])?.is_empty(),
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
}
