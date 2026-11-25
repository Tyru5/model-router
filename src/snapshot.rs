//! Workspace snapshots as Git tree objects. A run-private index
//! (`<run dir>/snapshot.index`) records the working tree, so the user's index,
//! HEAD, refs, and files never change. Untracked files are included and ignored
//! ones are not, which keeps the self-ignored `.model-router/` out.
//!
//! Trees are not reachable from any commit, so each is kept by a ref,
//! `refs/model-router/runs/<run id>/<tree>`, or `git gc` would prune it and
//! old runs would lose their diffs. Cleaning a run deletes its refs.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    io::{BufRead, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Upper bound for patch text held in memory or saved as changes.diff.
pub const PATCH_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    /// E.g. a file replaced by a symlink.
    TypeChanged,
}
impl ChangeKind {
    /// The `git diff --name-status` letter.
    pub fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Deleted => 'D',
            Self::TypeChanged => 'T',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub status: ChangeKind,
    /// Line counts; None for binary files.
    pub added: Option<u64>,
    pub removed: Option<u64>,
}

/// `git` for the router's own commands in a workspace or checkout. A stage
/// may have written the repository's config or hooks before the router
/// runs Git there, outside the stage's sandbox: hooks and an fsmonitor
/// command never run. (Sandboxed stages cannot write Git config or hooks;
/// see `sandbox::git_locks`.)
pub(crate) fn git_command() -> Command {
    let mut command = Command::new("git");
    command.args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
    ]);
    command
}

/// A private, fixed shadow `GIT_DIR` whose `config` keeps only allowlisted
/// repository-format settings (plus `filter.<driver>.required = true` so
/// disabled required filters still fail closed). Git processes launched with
/// this shadow `GIT_DIR` never open `.git/config` or `.git/config.worktree`,
/// eliminating both repo-local driver execution and TOCTOU config races.
pub(crate) struct SanitizedGitDir {
    dir: tempfile::TempDir,
    top_level: PathBuf,
    local_clean_filters: Vec<Vec<u8>>,
    local_smudge_filters: Vec<Vec<u8>>,
    local_merge_drivers: Vec<Vec<u8>>,
    local_merge_default: Option<Vec<u8>>,
}

impl SanitizedGitDir {
    /// Inspects `dir`'s Git repository layout and config scopes once (requiring
    /// Git 2.26+) and materializes a private `0700` shadow `GIT_DIR` with a
    /// fixed, sanitized `config` file.
    pub(crate) fn new(dir: &Path) -> Result<Self> {
        let git_dir = rev_parse_path(dir, "--git-dir")?;
        let common_dir = rev_parse_path(dir, "--git-common-dir")?;
        let top_level = rev_parse_path(dir, "--show-toplevel")?;

        let inspected = inspect_repo_config(dir, &common_dir)?;
        let shadow = tempfile::Builder::new()
            .prefix("model-router-git-")
            .tempdir_in(crate::process::host::private_temp_dir()?)
            .context("Create sanitized Git directory")?;

        fs::write(shadow.path().join("config"), &inspected.sanitized_config)
            .context("Write sanitized Git config")?;
        let head = fs::read(git_dir.join("HEAD")).context("Read repository HEAD")?;
        fs::write(shadow.path().join("HEAD"), head).context("Write shadow HEAD")?;
        link_entry(&common_dir.join("objects"), &shadow.path().join("objects"))
            .context("Link repository objects")?;
        for name in ["refs", "packed-refs", "reftable", "info", "shallow", "lfs"] {
            let src = common_dir.join(name);
            if fs::symlink_metadata(&src).is_ok() {
                link_entry(&src, &shadow.path().join(name))
                    .with_context(|| format!("Link repository {name}"))?;
            }
        }
        // `info/sparse-checkout` is per worktree; the rest of `info` is
        // shared. A linked worktree gets its own sparse rules, as Git reads
        // them, not the main worktree's.
        if git_dir != common_dir {
            let info = shadow.path().join("info");
            let _ = fs::remove_file(&info);
            fs::create_dir(&info).context("Create shadow info directory")?;
            if let Ok(entries) = fs::read_dir(common_dir.join("info")) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if name != "sparse-checkout" {
                        link_entry(&entry.path(), &info.join(&name))
                            .context("Link repository info")?;
                    }
                }
            }
            let sparse = git_dir.join("info/sparse-checkout");
            if fs::symlink_metadata(&sparse).is_ok() {
                link_entry(&sparse, &info.join("sparse-checkout"))
                    .context("Link worktree sparse-checkout rules")?;
            }
        }
        let real_index = git_dir.join("index");
        if fs::symlink_metadata(&real_index).is_ok() {
            link_entry(&real_index, &shadow.path().join("index"))
                .context("Link repository index")?;
        }
        if let Ok(entries) = fs::read_dir(&git_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with("sharedindex.") {
                    link_entry(&entry.path(), &shadow.path().join(&name))
                        .context("Link repository shared index")?;
                }
            }
        }

        Ok(Self {
            dir: shadow,
            top_level,
            local_clean_filters: inspected.local_clean_filters,
            local_smudge_filters: inspected.local_smudge_filters,
            local_merge_drivers: inspected.local_merge_drivers,
            local_merge_default: inspected.local_merge_default,
        })
    }

    /// Builds a `git` `Command` pinned to this shadow `GIT_DIR` and work tree.
    /// The caller must keep `self` alive until the command exits.
    pub(crate) fn command(&self, cwd: &Path) -> Command {
        let mut command = git_command();
        command
            .current_dir(cwd)
            .env("GIT_DIR", self.dir.path())
            .env("GIT_WORK_TREE", &self.top_level)
            .env_remove("GIT_COMMON_DIR");
        command
    }

    pub(crate) fn top_level(&self) -> &Path {
        &self.top_level
    }

    pub(crate) fn local_clean_filters(&self) -> &[Vec<u8>] {
        &self.local_clean_filters
    }

    pub(crate) fn local_smudge_filters(&self) -> &[Vec<u8>] {
        &self.local_smudge_filters
    }

    /// Repo-local `merge.<name>.driver` commands, which never run.
    pub(crate) fn local_merge_drivers(&self) -> &[Vec<u8>] {
        &self.local_merge_drivers
    }

    /// A repo-local `merge.default`, which the sanitized config drops.
    pub(crate) fn local_merge_default(&self) -> Option<&[u8]> {
        self.local_merge_default.as_deref()
    }

    pub(crate) fn into_parts(self) -> (tempfile::TempDir, PathBuf) {
        (self.dir, self.top_level)
    }
}

/// Queries one `git rev-parse` path flag at a time and strips only its single
/// trailing line terminator, preserving embedded line breaks in directory paths.
fn rev_parse_path(dir: &Path, flag: &str) -> Result<PathBuf> {
    let rev = git_command()
        .args(["rev-parse", flag])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .context("Start Git")?;
    ensure!(
        rev.status.success(),
        "git rev-parse {flag}: {}",
        String::from_utf8_lossy(&rev.stderr).trim()
    );
    let raw = rev.stdout.strip_suffix(b"\n").unwrap_or(&rev.stdout);
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    ensure!(
        !raw.is_empty(),
        "git rev-parse {flag} returned empty output"
    );
    let path = PathBuf::from(bytes_to_os(raw)?);
    let joined = if path.is_absolute() {
        path
    } else {
        dir.join(path)
    };
    std::path::absolute(&joined).with_context(|| format!("Resolve {}", joined.display()))
}

#[cfg(unix)]
fn link_entry(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(not(unix))]
fn link_entry(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::os::windows::fs::symlink_dir(src, dst)
    } else {
        std::os::windows::fs::symlink_file(src, dst)
    }
}

struct InspectedRepoConfig {
    sanitized_config: Vec<u8>,
    local_clean_filters: Vec<Vec<u8>>,
    local_smudge_filters: Vec<Vec<u8>>,
    local_merge_drivers: Vec<Vec<u8>>,
    local_merge_default: Option<Vec<u8>>,
}

struct ConfigEntry {
    section: Vec<u8>,
    subsection: Option<Vec<u8>>,
    var: Vec<u8>,
    value: Option<Vec<u8>>,
}

fn inspect_repo_config(dir: &Path, common_dir: &Path) -> Result<InspectedRepoConfig> {
    // `--list -z` preserves exact raw key and value bytes without locale regex
    // quirks, including non-UTF-8 subsection names and names containing `=`.
    let output = git_command()
        .args(["config", "--show-scope", "-z", "--list"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .context("Start Git")?;
    ensure!(
        output.status.success(),
        "Could not inspect Git config scopes in {} (requires Git 2.26+): {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    parse_and_build_sanitized_config(dir, common_dir, &output.stdout)
}

fn parse_and_build_sanitized_config(
    dir: &Path,
    common_dir: &Path,
    raw: &[u8],
) -> Result<InspectedRepoConfig> {
    let mut repo_format_version: u32 = 0;
    let mut core_entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut extensions: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut unsupported_extensions: Vec<String> = Vec::new();
    let mut partial_clone = false;
    // Trusted system/global driver entries resolved against the real checkout
    // (preserving `includeIf.gitdir` rules that would no longer match the
    // temporary shadow `GIT_DIR`).
    let mut trusted_driver_entries: Vec<ConfigEntry> = Vec::new();
    let mut trusted_lfs_storage: Option<Vec<u8>> = None;
    let mut local_lfs_storage = false;
    let mut trusted_filter_required: Vec<Vec<u8>> = Vec::new();
    // Exact raw `filter.<driver>.required` values from local/worktree scope,
    // written verbatim so Git itself parses numeric/boolean forms (e.g. `2`).
    let mut local_filter_required: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
    let mut local_clean_filters: Vec<Vec<u8>> = Vec::new();
    let mut local_smudge_filters: Vec<Vec<u8>> = Vec::new();
    // Whether the last repo-local `filter.<driver>.{clean,smudge,process}` is
    // empty: an empty value turns a trusted global command off, and is kept.
    let mut local_filter_commands: Vec<(Vec<u8>, Vec<u8>, bool)> = Vec::new();
    // Repo-local `merge.<name>.driver` (whether its last value is empty) and
    // `merge.default`: neither reaches the sanitized config.
    let mut local_merge_commands: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut local_merge_default: Option<Vec<u8>> = None;
    // Repository-local Git LFS endpoint URLs and remote selection (data, not
    // commands), kept so git-lfs can still fetch objects missing from the
    // local cache from the same remote a normal checkout would use.
    let mut lfs_endpoints: Vec<ConfigEntry> = Vec::new();

    let mut parts = raw.split(|&b| b == 0);
    while let (Some(scope), Some(entry)) = (parts.next(), parts.next()) {
        let is_trusted_scope = scope == b"system" || scope == b"global";
        let is_repo_scope = scope == b"local" || scope == b"worktree";
        if !is_trusted_scope && !is_repo_scope {
            continue;
        }
        let (key, val) = match entry.iter().position(|&b| b == b'\n') {
            Some(pos) => (&entry[..pos], Some(&entry[pos + 1..])),
            None => (entry, None),
        };
        let Some((section, rest)) = split_first(key, b'.') else {
            continue;
        };

        if section.eq_ignore_ascii_case(b"core") && !rest.contains(&b'.') {
            if is_repo_scope && rest.eq_ignore_ascii_case(b"repositoryformatversion") {
                if let Some(v) = val.and_then(|b| std::str::from_utf8(b).ok()) {
                    if let Ok(ver) = v.trim().parse::<u32>() {
                        repo_format_version = ver;
                    }
                }
                continue;
            }
            if is_safe_core_key(rest) {
                let value = val.unwrap_or(b"true");
                if !value.contains(&b'\n') && !value.contains(&b'\r') && !value.contains(&0) {
                    set_last(&mut core_entries, rest.to_ascii_lowercase(), value.to_vec());
                }
            }
            continue;
        }

        if is_trusted_scope {
            if section.eq_ignore_ascii_case(b"lfs") && rest.eq_ignore_ascii_case(b"storage") {
                if let Some(v) = val {
                    if !v.is_empty() {
                        trusted_lfs_storage = Some(v.to_vec());
                    }
                }
                continue;
            }
            if is_driver_section(section) {
                let (subsection, var) = match split_last(rest, b'.') {
                    Some((sub, var)) => (Some(sub.to_vec()), var.to_vec()),
                    None => (None, rest.to_vec()),
                };
                if section.eq_ignore_ascii_case(b"filter") && var.eq_ignore_ascii_case(b"required")
                {
                    if let Some(sub) = subsection.as_ref() {
                        if !sub.is_empty() && !trusted_filter_required.iter().any(|d| d == sub) {
                            trusted_filter_required.push(sub.clone());
                        }
                    }
                }
                trusted_driver_entries.push(ConfigEntry {
                    section: section.to_ascii_lowercase(),
                    subsection,
                    var,
                    value: val.map(<[u8]>::to_vec),
                });
            }
            continue;
        }

        if section.eq_ignore_ascii_case(b"extensions") && !rest.contains(&b'.') {
            let lower = rest.to_ascii_lowercase();
            if matches!(
                lower.as_slice(),
                b"worktreeconfig" | b"relativeworktrees" | b"noop" | b"noop-without-value"
            ) {
                continue;
            }
            if lower.as_slice() == b"partialclone" {
                partial_clone = true;
                continue;
            }
            if matches!(lower.as_slice(), b"objectformat" | b"refstorage") {
                if let Some(value) = val {
                    if !value.contains(&b'\n') && !value.contains(&b'\r') && !value.contains(&0) {
                        set_last(&mut extensions, lower, value.to_vec());
                    }
                }
            } else {
                let name = String::from_utf8_lossy(&lower).into_owned();
                if !unsupported_extensions.contains(&name) {
                    unsupported_extensions.push(name);
                }
            }
        } else if section.eq_ignore_ascii_case(b"lfs") && rest.eq_ignore_ascii_case(b"storage") {
            if val.is_some_and(|v| !v.is_empty()) {
                local_lfs_storage = true;
            }
        } else if section.eq_ignore_ascii_case(b"lfs") && rest.eq_ignore_ascii_case(b"url") {
            push_lfs_endpoint(&mut lfs_endpoints, b"lfs", None, b"url", val);
        } else if section.eq_ignore_ascii_case(b"remote")
            && (rest.eq_ignore_ascii_case(b"lfsdefault")
                || rest.eq_ignore_ascii_case(b"lfspushdefault"))
        {
            let var = rest.to_ascii_lowercase();
            push_lfs_endpoint(&mut lfs_endpoints, b"remote", None, &var, val);
        } else if section.eq_ignore_ascii_case(b"branch") {
            // git-lfs fetches from the current branch's remote.
            if let Some((sub, var)) = split_last(rest, b'.') {
                if !sub.is_empty() && var.eq_ignore_ascii_case(b"remote") {
                    push_lfs_endpoint(&mut lfs_endpoints, b"branch", Some(sub), b"remote", val);
                }
            }
        } else if section.eq_ignore_ascii_case(b"remote") {
            if let Some((sub, var)) = split_last(rest, b'.') {
                if var.eq_ignore_ascii_case(b"promisor") && !is_git_explicit_false(val) {
                    partial_clone = true;
                } else if !sub.is_empty()
                    && (var.eq_ignore_ascii_case(b"url") || var.eq_ignore_ascii_case(b"lfsurl"))
                {
                    push_lfs_endpoint(
                        &mut lfs_endpoints,
                        b"remote",
                        Some(sub),
                        &var.to_ascii_lowercase(),
                        val,
                    );
                }
            }
        } else if section.eq_ignore_ascii_case(b"merge") {
            match split_last(rest, b'.') {
                Some((name, var)) if !name.is_empty() && var.eq_ignore_ascii_case(b"driver") => {
                    if let Some(value) = val {
                        set_last(&mut local_merge_commands, name.to_vec(), value.is_empty());
                    }
                }
                None if rest.eq_ignore_ascii_case(b"default") => {
                    local_merge_default = val.filter(|v| !v.is_empty()).map(<[u8]>::to_vec);
                }
                _ => {}
            }
        } else if section.eq_ignore_ascii_case(b"filter") {
            let Some((subsection, var)) = split_last(rest, b'.') else {
                continue;
            };
            if subsection.is_empty() {
                continue;
            }
            if [&b"clean"[..], b"smudge", b"process"]
                .iter()
                .any(|name| var.eq_ignore_ascii_case(name))
            {
                if let Some(value) = val {
                    let var = var.to_ascii_lowercase();
                    let empty = value.is_empty();
                    match local_filter_commands
                        .iter_mut()
                        .find(|(d, v, _)| d == subsection && *v == var)
                    {
                        Some(entry) => entry.2 = empty,
                        None => local_filter_commands.push((subsection.to_vec(), var, empty)),
                    }
                }
            }
            if var.eq_ignore_ascii_case(b"required") {
                set_last(
                    &mut local_filter_required,
                    subsection.to_vec(),
                    val.map(<[u8]>::to_vec),
                );
            }
        }
    }
    // Disabled drivers come from the last repo-local value of each command,
    // so one a later empty value turned off blocks nothing.
    for (driver, var, empty) in &local_filter_commands {
        if *empty {
            continue;
        }
        if var != b"smudge" && !local_clean_filters.contains(driver) {
            local_clean_filters.push(driver.clone());
        }
        if var != b"clean" && !local_smudge_filters.contains(driver) {
            local_smudge_filters.push(driver.clone());
        }
    }

    ensure!(
        !local_lfs_storage,
        "Refusing Git operation in {}: repository-local `lfs.storage` is not supported by model-router's sanitized Git config; configure `lfs.storage` globally instead",
        dir.display()
    );
    ensure!(
        !partial_clone,
        "Refusing Git operation in {}: partial-clone repositories (extensions.partialClone / remote.*.promisor) are not supported by model-router's sanitized Git config; use a full clone (`git clone` without `--filter`)",
        dir.display()
    );
    ensure!(
        repo_format_version == 0 || unsupported_extensions.is_empty(),
        "Refusing Git operation in {}: unsupported repository format extension(s) ({})",
        dir.display(),
        unsupported_extensions.join(", ")
    );

    let mut sanitized_config = Vec::new();
    sanitized_config.extend_from_slice(b"[core]\n");
    if extensions.is_empty() {
        sanitized_config.extend_from_slice(b"\trepositoryformatversion = 0\n");
    } else {
        sanitized_config.extend_from_slice(b"\trepositoryformatversion = 1\n");
    }
    sanitized_config.extend_from_slice(b"\tbare = false\n");
    sanitized_config.extend_from_slice(b"\tfsmonitor = false\n");
    sanitized_config.extend_from_slice(b"\thooksPath = /dev/null\n");
    // Keep private indexes self-contained so they never depend on a
    // `sharedindex.*` created inside this temporary shadow GIT_DIR.
    sanitized_config.extend_from_slice(b"\tsplitIndex = false\n");
    for (var, value) in &core_entries {
        sanitized_config.push(b'\t');
        sanitized_config.extend_from_slice(var);
        sanitized_config.extend_from_slice(b" = \"");
        push_git_value_escaped(&mut sanitized_config, value);
        sanitized_config.extend_from_slice(b"\"\n");
    }
    if !extensions.is_empty() {
        sanitized_config.extend_from_slice(b"[extensions]\n");
        for (var, value) in &extensions {
            sanitized_config.push(b'\t');
            sanitized_config.extend_from_slice(var);
            sanitized_config.extend_from_slice(b" = \"");
            push_git_value_escaped(&mut sanitized_config, value);
            sanitized_config.extend_from_slice(b"\"\n");
        }
    }
    // Keep Git LFS pointed at the real repository's `<common_dir>/lfs` (or an
    // absolute trusted global `lfs.storage`), never the temporary shadow
    // GIT_DIR whose directory is deleted when this operation finishes.
    let lfs_storage = match trusted_lfs_storage {
        Some(path) => {
            let path_os = bytes_to_os(&path)?;
            ensure!(
                Path::new(&path_os).is_absolute(),
                "Refusing Git operation in {}: relative `lfs.storage` ({}) is not supported by model-router's sanitized Git config; configure an absolute `lfs.storage` path or unset it to use `.git/lfs`",
                dir.display(),
                String::from_utf8_lossy(&path)
            );
            path
        }
        None => path_to_bytes(&common_dir.join("lfs"))?,
    };
    write_config_entry(
        &mut sanitized_config,
        &ConfigEntry {
            section: b"lfs".to_vec(),
            subsection: None,
            var: b"storage".to_vec(),
            value: Some(lfs_storage),
        },
    )?;
    for entry in trusted_driver_entries.iter().chain(&lfs_endpoints) {
        write_config_entry(&mut sanitized_config, entry)?;
    }
    // After the trusted entries, so the empty value wins as it does in Git.
    for (driver, var, empty) in &local_filter_commands {
        if *empty {
            write_config_entry(
                &mut sanitized_config,
                &ConfigEntry {
                    section: b"filter".to_vec(),
                    subsection: Some(driver.clone()),
                    var: var.clone(),
                    value: Some(Vec::new()),
                },
            )?;
        }
    }
    for (driver, value) in &local_filter_required {
        // Never allow a repository-local `required = false` to downgrade a
        // trusted system/global `filter.<driver>.required`, while preserving
        // every other local `required` setting verbatim (including when global
        // defines only a smudge command or sets `required = false`).
        if trusted_filter_required.iter().any(|d| d == driver)
            && is_git_explicit_false(value.as_deref())
        {
            continue;
        }
        write_config_entry(
            &mut sanitized_config,
            &ConfigEntry {
                section: b"filter".to_vec(),
                subsection: Some(driver.clone()),
                var: b"required".to_vec(),
                value: value.clone(),
            },
        )?;
    }

    Ok(InspectedRepoConfig {
        sanitized_config,
        local_clean_filters,
        local_smudge_filters,
        local_merge_drivers: local_merge_commands
            .into_iter()
            .filter(|(_, empty)| !empty)
            .map(|(name, _)| name)
            .collect(),
        local_merge_default,
    })
}

fn is_git_explicit_false(val: Option<&[u8]>) -> bool {
    let Some(raw) = val else {
        return false;
    };
    let start = raw
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(raw.len());
    let end = raw
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    let lower = raw[start..end].to_ascii_lowercase();
    matches!(lower.as_slice(), b"" | b"false" | b"no" | b"off" | b"0")
}

#[cfg(unix)]
fn path_to_bytes(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    ensure!(
        !bytes.contains(&0) && !bytes.contains(&b'\r'),
        "Git path contains invalid control bytes"
    );
    Ok(bytes.to_vec())
}

#[cfg(not(unix))]
fn path_to_bytes(path: &Path) -> Result<Vec<u8>> {
    let text = path.to_str().context("Git path is not valid UTF-8")?;
    ensure!(
        !text.contains('\0') && !text.contains('\r'),
        "Git path contains invalid control bytes"
    );
    Ok(text.as_bytes().to_vec())
}

fn is_driver_section(section: &[u8]) -> bool {
    section.eq_ignore_ascii_case(b"filter")
        || section.eq_ignore_ascii_case(b"diff")
        || section.eq_ignore_ascii_case(b"difftool")
        || section.eq_ignore_ascii_case(b"merge")
        || section.eq_ignore_ascii_case(b"mergetool")
}

fn push_lfs_endpoint(
    out: &mut Vec<ConfigEntry>,
    section: &[u8],
    subsection: Option<&[u8]>,
    var: &[u8],
    val: Option<&[u8]>,
) {
    let Some(value) = val.filter(|v| !v.is_empty() && !v.contains(&b'\n')) else {
        return;
    };
    out.push(ConfigEntry {
        section: section.to_vec(),
        subsection: subsection.map(<[u8]>::to_vec),
        var: var.to_vec(),
        value: Some(value.to_vec()),
    });
}

fn write_config_entry(out: &mut Vec<u8>, entry: &ConfigEntry) -> Result<()> {
    ensure!(
        !entry.var.is_empty()
            && entry
                .var
                .iter()
                .all(|&b| b.is_ascii_alphanumeric() || b == b'-'),
        "Git config key contains invalid bytes"
    );
    out.push(b'[');
    out.extend_from_slice(&entry.section);
    if let Some(sub) = &entry.subsection {
        ensure!(
            !sub.contains(&b'\n') && !sub.contains(&b'\r') && !sub.contains(&0),
            "Git config subsection contains invalid control bytes"
        );
        out.extend_from_slice(b" \"");
        push_git_subsection_escaped(out, sub);
        out.push(b'"');
    }
    out.extend_from_slice(b"]\n\t");
    out.extend_from_slice(&entry.var);
    if let Some(val) = &entry.value {
        ensure!(
            !val.contains(&0) && !val.contains(&b'\r'),
            "Git config value contains invalid control bytes"
        );
        out.extend_from_slice(b" = \"");
        push_git_value_escaped(out, val);
        out.extend_from_slice(b"\"\n");
    } else {
        out.push(b'\n');
    }
    Ok(())
}

fn is_safe_core_key(var: &[u8]) -> bool {
    let lower = var.to_ascii_lowercase();
    matches!(
        lower.as_slice(),
        b"filemode"
            | b"ignorecase"
            | b"precomposeunicode"
            | b"symlinks"
            | b"autocrlf"
            | b"eol"
            | b"safecrlf"
            | b"longpaths"
            | b"quotepath"
            | b"protecthfs"
            | b"protectntfs"
            | b"trustctime"
            | b"checkstat"
            | b"excludesfile"
            | b"attributesfile"
    )
}

fn set_last<T>(list: &mut Vec<(Vec<u8>, T)>, key: Vec<u8>, value: T) {
    if let Some((_, existing)) = list.iter_mut().find(|(k, _)| *k == key) {
        *existing = value;
    } else {
        list.push((key, value));
    }
}

fn push_git_subsection_escaped(out: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        if b == b'\\' || b == b'"' {
            out.push(b'\\');
        }
        out.push(b);
    }
}

fn push_git_value_escaped(out: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'"' => out.extend_from_slice(b"\\\""),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x08 => out.extend_from_slice(b"\\b"),
            _ => out.push(b),
        }
    }
}

/// Checks whether any NUL-delimited path in `stdin_paths` resolves a `filter`
/// attribute (working-tree or `--cached`) naming one of `drivers`. Returns the
/// first matching `(path, driver)` pair.
pub(crate) fn find_suppressed_filter_path(
    sanitized: &SanitizedGitDir,
    work: &Path,
    stdin_paths: &[u8],
    index: Option<&Path>,
    drivers: &[Vec<u8>],
) -> Result<Option<(String, String)>> {
    find_suppressed_filter_path_in(sanitized, work, stdin_paths, index, drivers, &[false, true])
}

/// `find_suppressed_filter_path` reading attributes only from `index`, not
/// the working tree: for a proposed result that is not on disk yet.
pub(crate) fn find_suppressed_filter_path_cached(
    sanitized: &SanitizedGitDir,
    work: &Path,
    stdin_paths: &[u8],
    index: &Path,
    drivers: &[Vec<u8>],
) -> Result<Option<(String, String)>> {
    find_suppressed_filter_path_in(sanitized, work, stdin_paths, Some(index), drivers, &[true])
}

fn find_suppressed_filter_path_in(
    sanitized: &SanitizedGitDir,
    work: &Path,
    stdin_paths: &[u8],
    index: Option<&Path>,
    drivers: &[Vec<u8>],
    sources: &[bool],
) -> Result<Option<(String, String)>> {
    if drivers.is_empty() || stdin_paths.is_empty() {
        return Ok(None);
    }
    for &cached in sources {
        let stdout = check_attr(sanitized, work, stdin_paths, index, cached, "filter")?;
        let mut parts = stdout.split(|&b| b == 0);
        while let (Some(path), Some(_attr), Some(value)) =
            (parts.next(), parts.next(), parts.next())
        {
            // Check disabled driver names without skipping `check-attr`'s
            // status labels (`set`, `unset`, `unspecified`), so a disabled
            // filter carrying one of those ambiguous names fails closed.
            if !value.is_empty() && drivers.iter().any(|d| d.as_slice() == value) {
                return Ok(Some((
                    String::from_utf8_lossy(path).into_owned(),
                    String::from_utf8_lossy(value).into_owned(),
                )));
            }
        }
    }
    Ok(None)
}

/// The first NUL-delimited path in `stdin_paths` whose three-way merge the
/// repository's own config would hand to a repo-local merge driver the
/// sanitized config leaves out: a `merge` attribute (working tree or
/// `index`) naming a repo-local `merge.<name>.driver`, or no `merge`
/// attribute under a repo-local `merge.default`. Returns `(path, driver)`.
pub(crate) fn find_local_merge_driver_path(
    sanitized: &SanitizedGitDir,
    work: &Path,
    stdin_paths: &[u8],
    index: Option<&Path>,
) -> Result<Option<(String, String)>> {
    let drivers = sanitized.local_merge_drivers();
    let default = sanitized.local_merge_default();
    if (drivers.is_empty() && default.is_none()) || stdin_paths.is_empty() {
        return Ok(None);
    }
    for cached in [false, true] {
        let stdout = check_attr(sanitized, work, stdin_paths, index, cached, "merge")?;
        let mut parts = stdout.split(|&b| b == 0);
        while let (Some(path), Some(_attr), Some(value)) =
            (parts.next(), parts.next(), parts.next())
        {
            // A driver named like a status label (`unspecified`) fails closed.
            let driver = if drivers.iter().any(|d| d.as_slice() == value) {
                Some(value)
            } else if value == b"unspecified" {
                // `text` is what Git uses with no `merge.default` at all.
                default.filter(|name| *name != b"text")
            } else {
                None
            };
            if let Some(driver) = driver {
                return Ok(Some((
                    String::from_utf8_lossy(path).into_owned(),
                    String::from_utf8_lossy(driver).into_owned(),
                )));
            }
        }
    }
    Ok(None)
}

/// `git check-attr -z <attr> --stdin` output for NUL-delimited `stdin_paths`:
/// `<path> NUL <attr> NUL <value> NUL` records, from the working tree's
/// attribute files or, with `cached`, the index's.
fn check_attr(
    sanitized: &SanitizedGitDir,
    work: &Path,
    stdin_paths: &[u8],
    index: Option<&Path>,
    cached: bool,
    attr: &str,
) -> Result<Vec<u8>> {
    let mut cmd = sanitized.command(work);
    cmd.arg("check-attr");
    if cached {
        cmd.arg("--cached");
    }
    cmd.args(["-z", attr, "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(idx) = index {
        cmd.env("GIT_INDEX_FILE", idx);
    }
    let mut child = cmd.spawn().context("Start Git")?;
    let output = std::thread::scope(|scope| -> Result<std::process::Output> {
        let writer = child.stdin.take().map(|mut pipe| {
            scope.spawn(move || -> std::io::Result<()> {
                use std::io::Write;
                match pipe.write_all(stdin_paths) {
                    Ok(()) => Ok(()),
                    Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                    Err(err) => Err(err),
                }
            })
        });
        let output = child.wait_with_output().context("Wait for Git")?;
        if let Some(handle) = writer {
            if let Ok(Err(err)) = handle.join() {
                if output.status.success() {
                    return Err(err.into());
                }
            }
        }
        Ok(output)
    })?;
    ensure!(
        output.status.success(),
        "git check-attr {attr} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

/// `-c` arguments that turn off every repo-local clean and process filter,
/// for a Git command that has to run with the repository's own config. Fails
/// when a filter name cannot be written as a `-c` key.
pub(crate) fn clean_filter_overrides(dir: &Path) -> Result<Vec<String>> {
    let sanitized = SanitizedGitDir::new(dir)?;
    let mut args = Vec::new();
    for driver in sanitized.local_clean_filters() {
        let name = std::str::from_utf8(driver)
            .ok()
            .filter(|name| !name.contains(['=', '\n']))
            .context("a repo-local Git filter name cannot be turned off on the command line")?;
        for var in ["clean", "process"] {
            args.push("-c".to_owned());
            args.push(format!("filter.{name}.{var}="));
        }
    }
    Ok(args)
}

/// Refuses to check out a worktree at `commit_or_tree` when any file in it has
/// a `.gitattributes` `filter` naming a repo-local or worktree-local
/// `smudge`/`process` filter that the router disables, so worktrees never
/// silently start with unprocessed pointer files (even for optional filters).
fn refuse_suppressed_smudge_filters(
    sanitized: &SanitizedGitDir,
    commit_or_tree: &str,
) -> Result<()> {
    let drivers = sanitized.local_smudge_filters();
    if drivers.is_empty() {
        return Ok(());
    }
    let top = sanitized.top_level();
    let list_out = sanitized
        .command(top)
        .args(["ls-tree", "-r", "-z", "--name-only", commit_or_tree])
        .stdin(Stdio::null())
        .output()
        .context("Start Git")?;
    ensure!(
        list_out.status.success(),
        "git ls-tree {commit_or_tree}: {}",
        String::from_utf8_lossy(&list_out.stderr).trim()
    );
    if list_out.stdout.is_empty() {
        return Ok(());
    }
    let scratch = tempfile::Builder::new()
        .prefix("model-router-wt-attr-")
        .tempdir()
        .context("Create scratch directory for worktree attribute check")?;
    let index = scratch.path().join("index");
    let read_out = sanitized
        .command(top)
        .args(["read-tree", commit_or_tree])
        .env("GIT_INDEX_FILE", &index)
        .stdin(Stdio::null())
        .output()
        .context("Start Git")?;
    ensure!(
        read_out.status.success(),
        "git read-tree {commit_or_tree}: {}",
        String::from_utf8_lossy(&read_out.stderr).trim()
    );
    // Check `--cached` against `commit_or_tree`'s index directly (ignoring
    // unrelated working-tree `.gitattributes`).
    let mut child = sanitized
        .command(top)
        .args(["check-attr", "--cached", "-z", "filter", "--stdin"])
        .env("GIT_INDEX_FILE", &index)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Start Git")?;
    let stdin_bytes = list_out.stdout.as_slice();
    let output = std::thread::scope(|scope| -> Result<std::process::Output> {
        let writer = child.stdin.take().map(|mut pipe| {
            scope.spawn(move || -> std::io::Result<()> {
                use std::io::Write;
                match pipe.write_all(stdin_bytes) {
                    Ok(()) => Ok(()),
                    Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                    Err(err) => Err(err),
                }
            })
        });
        let output = child.wait_with_output().context("Wait for Git")?;
        if let Some(handle) = writer {
            if let Ok(Err(err)) = handle.join() {
                if output.status.success() {
                    return Err(err.into());
                }
            }
        }
        Ok(output)
    })?;
    ensure!(
        output.status.success(),
        "git check-attr filter failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let mut parts = output.stdout.split(|&b| b == 0);
    while let (Some(path), Some(_attr), Some(value)) = (parts.next(), parts.next(), parts.next()) {
        if value.is_empty() {
            continue;
        }
        if drivers.iter().any(|d| d.as_slice() == value) {
            anyhow::bail!(
                "Refusing to create worktree: {} uses repo-local Git smudge filter `{}` which is disabled for safety and would leave unprocessed files in the worktree; configure the filter globally instead",
                String::from_utf8_lossy(path),
                String::from_utf8_lossy(value)
            );
        }
        if matches!(value, b"unspecified" | b"unset" | b"set") {
            continue;
        }
    }
    Ok(())
}

/// Populates a newly created `--no-checkout` worktree at `worktree` using a
/// fixed, sanitized shadow `GIT_DIR`, so repo-local smudge/process filters can
/// never run during checkout even if `.git/config` changes concurrently.
pub(crate) fn populate_worktree(worktree: &Path, commit: &str) -> Result<()> {
    let git_dir_out = git_command()
        .args(["rev-parse", "--git-dir"])
        .current_dir(worktree)
        .stdin(Stdio::null())
        .output()
        .context("Start Git")?;
    ensure!(
        git_dir_out.status.success(),
        "git rev-parse --git-dir: {}",
        String::from_utf8_lossy(&git_dir_out.stderr).trim()
    );
    let raw = git_dir_out
        .stdout
        .strip_suffix(b"\n")
        .unwrap_or(&git_dir_out.stdout);
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    let git_dir_rel = PathBuf::from(bytes_to_os(raw)?);
    let git_dir = std::path::absolute(if git_dir_rel.is_absolute() {
        git_dir_rel
    } else {
        worktree.join(git_dir_rel)
    })?;
    let index = git_dir.join("index");

    // One sanitized config and one commit for the filter check and the
    // checkout, taken from the new worktree: its config scopes and attribute
    // sources, not the source worktree's `config.worktree`.
    let sanitized = SanitizedGitDir::new(worktree)?;
    refuse_suppressed_smudge_filters(&sanitized, commit)?;
    let run = |args: &[&str]| -> Result<()> {
        let out = sanitized
            .command(worktree)
            .args(args)
            .env("GIT_INDEX_FILE", &index)
            .stdin(Stdio::null())
            .output()
            .context("Start Git")?;
        ensure!(
            out.status.success(),
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(())
    };
    // `git worktree add` gave the new worktree the source checkout's sparse
    // settings and rules (Git 2.36 or later). Config is only read here.
    let setting = |key: &str| -> bool {
        git_command()
            .args(["config", "--type=bool", "--get", key])
            .current_dir(worktree)
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|out| out.stdout.trim_ascii() == b"true")
    };
    if setting("core.sparseCheckout") {
        // A merge into the empty index applies the sparse rules: files
        // outside them are marked skip-worktree and never written.
        let cone = if setting("core.sparseCheckoutCone") {
            "core.sparseCheckoutCone=true"
        } else {
            "core.sparseCheckoutCone=false"
        };
        run(&[
            "-c",
            "core.sparseCheckout=true",
            "-c",
            cone,
            "read-tree",
            "-m",
            "-u",
            commit,
        ])?;
    } else {
        run(&["read-tree", commit])?;
        run(&["checkout-index", "-a", "-f"])?;
    }
    let _ = sanitized
        .command(worktree)
        .args(["update-index", "-q", "--refresh"])
        .env("GIT_INDEX_FILE", &index)
        .stdin(Stdio::null())
        .output();
    Ok(())
}

fn split_first(bytes: &[u8], sep: u8) -> Option<(&[u8], &[u8])> {
    let pos = bytes.iter().position(|&b| b == sep)?;
    Some((&bytes[..pos], &bytes[pos + 1..]))
}

fn split_last(bytes: &[u8], sep: u8) -> Option<(&[u8], &[u8])> {
    let pos = bytes.iter().rposition(|&b| b == sep)?;
    Some((&bytes[..pos], &bytes[pos + 1..]))
}

#[cfg(unix)]
fn bytes_to_os(bytes: &[u8]) -> Result<OsString> {
    use std::os::unix::ffi::OsStrExt;
    ensure!(
        !bytes.contains(&0),
        "Git path or config key contains NUL byte"
    );
    Ok(std::ffi::OsStr::from_bytes(bytes).to_os_string())
}

#[cfg(not(unix))]
fn bytes_to_os(bytes: &[u8]) -> Result<OsString> {
    let text = std::str::from_utf8(bytes).context("Git path or config key is not valid UTF-8")?;
    ensure!(
        !text.contains('\0'),
        "Git path or config key contains NUL byte"
    );
    Ok(OsString::from(text))
}

fn git_raw(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = git_command()
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .context("Start Git")?;
    ensure!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

fn git_sanitized(dir: &Path, index: Option<&Path>, args: &[&str]) -> Result<Vec<u8>> {
    let sanitized = SanitizedGitDir::new(dir)?;
    let mut command = sanitized.command(dir);
    command.args(args).stdin(Stdio::null());
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = command.output().context("Start Git")?;
    ensure!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

/// Verifies that `dir`'s Git binary and repository layout are supported by
/// `SanitizedGitDir` (Git 2.26+ `--show-scope`, no partial-clone promisor
/// remotes, no unsupported format extensions) so runs fail before launch
/// instead of silently losing snapshots and diffs.
pub fn ensure_supported_git(dir: &Path) -> Result<()> {
    SanitizedGitDir::new(dir).map(|_| ())
}

/// Repository detection independent of config-scope inspection, so older Git
/// versions (< 2.26) still recognize repositories and surface the version
/// error when an affected operation runs.
pub fn is_repository(dir: &Path) -> bool {
    git_raw(dir, &["rev-parse", "--is-inside-work-tree"]).is_ok_and(|out| out.starts_with(b"true"))
}

/// The working tree as a Git tree ID. `Ok(None)` outside Git; returns the Git
/// error when `workspace` is a Git repository and snapshotting fails.
pub fn try_tree(workspace: &Path, run_dir: &Path) -> Result<Option<String>> {
    let Some(run) = run_dir.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    try_tree_with(workspace, &run_dir.join("snapshot.index"), run)
}

/// `try_tree` with its own private index, kept under run `run`'s refs.
pub fn try_tree_with(workspace: &Path, index: &Path, run: &str) -> Result<Option<String>> {
    let index = std::path::absolute(index)?;
    let Some(tree) = capture(workspace, &index)? else {
        return Ok(None);
    };
    // Best effort: an unkept tree still serves until the next gc.
    let _ = git_raw(
        workspace,
        &["update-ref", &format!("{}/{tree}", namespace(run)), &tree],
    );
    Ok(Some(tree))
}

/// The working tree as a Git tree ID. None outside Git or when Git fails; a
/// snapshot never fails a run unless the caller uses `try_tree`.
pub fn tree(workspace: &Path, run_dir: &Path) -> Option<String> {
    try_tree(workspace, run_dir).ok().flatten()
}

/// `tree` with its own private index, e.g. for a fan-out candidate's
/// worktree; kept under run `run`'s refs.
pub fn tree_with(workspace: &Path, index: &Path, run: &str) -> Option<String> {
    try_tree_with(workspace, index, run).ok().flatten()
}

/// The working tree as a Git tree ID now, through caller-provided `index` and
/// without creating a ref. Ok(None) outside Git.
pub fn capture_index(workspace: &Path, index: &Path) -> Result<Option<String>> {
    capture(workspace, &std::path::absolute(index)?)
}

/// The working tree as a Git tree ID now, through a throwaway index and
/// without a ref: for comparing against saved snapshots. Writes only Git
/// objects. Ok(None) outside Git.
pub fn current_tree(workspace: &Path) -> Result<Option<String>> {
    let scratch = tempfile::tempdir().context("Create a scratch index directory")?;
    capture(workspace, &scratch.path().join("index"))
}

/// Whether `tree` names a tree object in `dir`'s repository.
pub fn exists(dir: &Path, tree: &str) -> bool {
    git_raw(dir, &["cat-file", "-e", &format!("{tree}^{{tree}}")]).is_ok()
}

/// Makes the files of a fresh checkout `workspace` match snapshot `tree`
/// exactly, as uncommitted changes against its HEAD: the index, HEAD, and
/// refs stay as they are. Fails, saying why, when the result differs from
/// `tree` (e.g. attribute filters rewrote files).
pub fn materialize(workspace: &Path, tree: &str) -> Result<()> {
    let head = git_raw(
        workspace,
        &["rev-parse", "--verify", "--quiet", "HEAD^{tree}"],
    )
    .map(|out| String::from_utf8_lossy(&out).trim().to_owned())
    .context("The checkout has no HEAD commit")?;
    if head != tree {
        let sanitized = SanitizedGitDir::new(workspace)?;
        // The patch writes Git's stored bytes: refuse when `tree`, by its own
        // attribute rules, needs a smudge filter the router disables.
        refuse_suppressed_smudge_filters(&sanitized, tree)?;
        let scratch = tempfile::tempdir().context("Create a scratch directory")?;
        let patch = scratch.path().join("snapshot.patch");
        let file = fs::File::create(&patch)?;
        let status = sanitized
            .command(workspace)
            .args([
                "--no-pager",
                "diff",
                "--binary",
                "--full-index",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                // Whatever diff.noprefix or diff.*Prefix say, `git apply` expects these.
                "--src-prefix=a/",
                "--dst-prefix=b/",
                &head,
                tree,
            ])
            .stdin(Stdio::null())
            .stdout(file)
            .stderr(Stdio::null())
            .status()
            .context("Start Git")?;
        ensure!(status.success(), "git diff {head} {tree} failed");
        let patch = patch.to_str().context("Scratch path must be UTF-8")?;
        let apply = sanitized
            .command(workspace)
            .args(["apply", "--binary", "--whitespace=nowarn", patch])
            .stdin(Stdio::null())
            .output()
            .context("Start Git")?;
        ensure!(
            apply.status.success(),
            "git apply --binary --whitespace=nowarn {patch}: {}",
            String::from_utf8_lossy(&apply.stderr).trim()
        );
    }
    let now = current_tree(workspace)?.context("The checkout is not a Git repository")?;
    ensure!(
        now == tree,
        "The checkout does not reproduce snapshot {tree} exactly (got {now}); Git attributes or filters may rewrite files"
    );
    Ok(())
}

/// Keeps existing tree `tree` under run `run`'s refs, so cleaning another
/// run cannot prune it. False when `tree` is not in the repository.
pub fn keep(dir: &Path, run: &str, tree: &str) -> bool {
    exists(dir, tree)
        && git_raw(
            dir,
            &["update-ref", &format!("{}/{tree}", namespace(run)), tree],
        )
        .is_ok()
}

/// The ref keeping fan-out base commit `name` of run `run`.
pub fn base_ref(run: &str, name: &str) -> String {
    format!("{}/base/{name}", namespace(run))
}

fn namespace(run: &str) -> String {
    format!("refs/model-router/runs/{run}")
}

/// Deletes the refs keeping run `run`'s snapshots. Best effort; nothing to do
/// outside Git.
pub fn forget(project: &Path, run: &str) {
    let prefix = format!("{}/", namespace(run));
    let Ok(refs) = git_raw(project, &["for-each-ref", "--format=%(refname)", &prefix]) else {
        return;
    };
    let commands: String = String::from_utf8_lossy(&refs)
        .lines()
        .filter(|name| name.starts_with(&prefix))
        .map(|name| format!("delete {name}\n"))
        .collect();
    if commands.is_empty() {
        return;
    }
    let child = crate::snapshot::git_command()
        .args(["update-ref", "--stdin"])
        .current_dir(project)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        if let Some(mut input) = child.stdin.take() {
            let _ = std::io::Write::write_all(&mut input, commands.as_bytes());
        }
        let _ = child.wait();
    }
}

fn capture(workspace: &Path, index: &Path) -> Result<Option<String>> {
    let existed = index.exists();
    if !existed && !is_repository(workspace) {
        return Ok(None);
    }
    let sanitized = SanitizedGitDir::new(workspace)?;
    if !existed {
        seed(workspace, &sanitized, index)?;
    }
    // Snapshots are taken under the run lock, so a leftover lock is from an
    // interrupted add and would fail every later snapshot.
    let mut lock = OsString::from(index);
    lock.push(".lock");
    let _ = fs::remove_file(PathBuf::from(lock));
    let add_out = sanitized
        .command(workspace)
        .args(["add", "-A"])
        .env("GIT_INDEX_FILE", index)
        .stdin(Stdio::null())
        .output()
        .context("Start Git")?;
    ensure!(
        add_out.status.success(),
        "git add -A: {}",
        String::from_utf8_lossy(&add_out.stderr).trim()
    );
    let tree_out = sanitized
        .command(workspace)
        .args(["write-tree"])
        .env("GIT_INDEX_FILE", index)
        .stdin(Stdio::null())
        .output()
        .context("Start Git")?;
    ensure!(
        tree_out.status.success(),
        "git write-tree: {}",
        String::from_utf8_lossy(&tree_out.stderr).trim()
    );
    Ok(Some(
        String::from_utf8_lossy(&tree_out.stdout).trim().to_owned(),
    ))
}

/// Seeds the private index from the real one, whose stat data keeps the first
/// add fast; else from HEAD; else, for an unborn HEAD, starts empty.
fn seed(workspace: &Path, sanitized: &SanitizedGitDir, index: &Path) -> Result<()> {
    let real = git_raw(workspace, &["rev-parse", "--git-path", "index"])?;
    let real = workspace.join(String::from_utf8_lossy(&real).trim_end());
    if fs::copy(&real, index).is_ok() {
        return Ok(());
    }
    let _ = fs::remove_file(index);
    if git_raw(workspace, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok() {
        let out = sanitized
            .command(workspace)
            .args(["read-tree", "HEAD"])
            .env("GIT_INDEX_FILE", index)
            .stdin(Stdio::null())
            .output()
            .context("Start Git")?;
        ensure!(
            out.status.success(),
            "git read-tree HEAD: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// File paths that differ between two snapshot trees (one `git diff` call).
pub fn changed_paths(dir: &Path, from: &str, to: &str) -> Result<Vec<String>> {
    let out = git_sanitized(
        dir,
        None,
        &["diff", "--no-renames", "-z", "--name-only", from, to],
    )?;
    Ok(out
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect())
}

/// Files that differ between two snapshot trees.
pub fn changes(dir: &Path, from: &str, to: &str) -> Result<Vec<FileChange>> {
    let sanitized = SanitizedGitDir::new(dir)?;
    let run = |args: &[&str]| -> Result<Vec<u8>> {
        let out = sanitized
            .command(dir)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .context("Start Git")?;
        ensure!(
            out.status.success(),
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(out.stdout)
    };
    let names = run(&["diff", "--no-renames", "-z", "--name-status", from, to])?;
    let numstat = run(&["diff", "--no-renames", "-z", "--numstat", from, to])?;
    // Each record is "added\tremoved\tpath"; binary files count "-".
    let counts: HashMap<&[u8], (Option<u64>, Option<u64>)> = numstat
        .split(|&b| b == 0)
        .filter_map(|record| {
            let mut fields = record.splitn(3, |&b| b == b'\t');
            let (added, removed, path) = (fields.next()?, fields.next()?, fields.next()?);
            Some((path, (number(added), number(removed))))
        })
        .collect();
    let mut fields = names.split(|&b| b == 0);
    let mut files = vec![];
    while let (Some(code), Some(path)) = (fields.next(), fields.next()) {
        let status = match code.first() {
            Some(b'A') => ChangeKind::Added,
            Some(b'D') => ChangeKind::Deleted,
            Some(b'T') => ChangeKind::TypeChanged,
            Some(_) => ChangeKind::Modified,
            None => break,
        };
        let (added, removed) = counts.get(path).copied().unwrap_or_default();
        files.push(FileChange {
            path: String::from_utf8_lossy(path).into_owned(),
            status,
            added,
            removed,
        });
    }
    Ok(files)
}

fn number(field: &[u8]) -> Option<u64> {
    std::str::from_utf8(field).ok()?.parse().ok()
}

/// `git diff` between two trees, cut after `max` bytes with a note.
pub fn patch(dir: &Path, from: &str, to: &str, max: usize) -> Result<String> {
    const STDERR_LIMIT: u64 = 64 * 1024;
    let sanitized = SanitizedGitDir::new(dir)?;
    let mut child = sanitized
        .command(dir)
        .args([
            "--no-pager",
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            from,
            to,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Start Git")?;
    let mut stdout = child.stdout.take().context("No Git output")?;
    let mut stderr = child.stderr.take().context("No Git output")?;
    let mut bytes = Vec::new();
    let (read, error) = std::thread::scope(|scope| {
        let errors = scope.spawn(move || {
            let mut text = Vec::new();
            let _ = stderr.by_ref().take(STDERR_LIMIT).read_to_end(&mut text);
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            String::from_utf8_lossy(&text).into_owned()
        });
        let read = stdout.by_ref().take(max as u64 + 1).read_to_end(&mut bytes);
        if read.is_err() || bytes.len() > max {
            let _ = child.kill();
        }
        let error = errors.join().unwrap_or_default();
        (read, error)
    });
    let status = child.wait()?;
    read.context("Read git diff")?;
    let truncated = bytes.len() > max;
    ensure!(truncated || status.success(), "git diff: {}", error.trim());
    if truncated {
        bytes.truncate(max);
        // Trim an incomplete multi-byte UTF-8 sequence split at `max`, without
        // disturbing valid or invalid bytes earlier in the diff.
        let tail_start = bytes.len().saturating_sub(4);
        if let Some(rel) = bytes[tail_start..]
            .iter()
            .rposition(|&b| (b & 0b1100_0000) != 0b1000_0000)
        {
            let pos = tail_start + rel;
            if let Err(err) = std::str::from_utf8(&bytes[pos..]) {
                if err.error_len().is_none() {
                    bytes.truncate(pos);
                }
            }
        }
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        text.push_str(&format!(
            "\n[Patch truncated after {max} bytes. Full patch: git diff {from} {to}]\n"
        ));
    }
    Ok(text)
}

fn retain_overlap(line: &mut Vec<u8>, overlap: usize) {
    let mut start = line.len().saturating_sub(overlap).max(1);
    while start < line.len() && (line[start] & 0b1100_0000) == 0b1000_0000 {
        start += 1;
    }
    let tail_len = line.len() - start;
    line.copy_within(start.., 1);
    line[0] = b'+';
    line.truncate(1 + tail_len);
}

fn emit_added_tail(
    line: &mut Vec<u8>,
    mut rest: &[u8],
    limit: usize,
    overlap: usize,
    each: &mut impl FnMut(&str),
) {
    while !rest.is_empty() {
        let allowed = limit.saturating_sub(line.len());
        let copy_len = rest.len().min(allowed);
        line.extend_from_slice(&rest[..copy_len]);
        rest = &rest[copy_len..];
        if line.len() == limit && !rest.is_empty() {
            each(&String::from_utf8_lossy(line));
            retain_overlap(line, overlap);
        }
    }
    each(&String::from_utf8_lossy(line));
}

/// The whole `git diff` between two trees, one line at a time and never
/// cut, for scans that must see all of it. `--text` shows files Git would
/// call binary, so their added lines are read too. Very long lines are streamed
/// in bounded 1 MiB chunks with a 2 KiB overlap on added lines.
pub fn patch_lines(dir: &Path, from: &str, to: &str, mut each: impl FnMut(&str)) -> Result<()> {
    const LINE_CHUNK_LIMIT: usize = 1024 * 1024;
    // More than any text a workflow policy pattern (at most 512 bytes) can
    // match, even case-insensitively where lowercasing changes byte lengths,
    // so a match across a chunk boundary is whole in one chunk.
    const OVERLAP: usize = 2048;
    const STDERR_LIMIT: u64 = 64 * 1024;
    let sanitized = SanitizedGitDir::new(dir)?;
    let mut child = sanitized
        .command(dir)
        .args([
            "--no-pager",
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--text",
            from,
            to,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Start Git")?;
    let stdout = child.stdout.take().context("No Git output")?;
    // Drained concurrently, capped at 64 KiB + drained to sink so Git cannot block.
    let mut stderr = child.stderr.take().context("No Git output")?;
    let (read, error) = std::thread::scope(|scope| {
        let errors = scope.spawn(move || {
            let mut text = Vec::new();
            let _ = stderr.by_ref().take(STDERR_LIMIT).read_to_end(&mut text);
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            String::from_utf8_lossy(&text).into_owned()
        });
        let mut reader = std::io::BufReader::new(stdout);
        let mut line: Vec<u8> = Vec::new();
        let mut in_overflow = false;
        let mut overflow_added = false;
        let mut has_new_bytes = false;
        let read = loop {
            let buf = match std::io::BufRead::fill_buf(&mut reader) {
                Ok(buf) => buf,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => break Err(err),
            };
            if buf.is_empty() {
                if !line.is_empty() && (!in_overflow || (overflow_added && has_new_bytes)) {
                    each(&String::from_utf8_lossy(&line));
                }
                break Ok(());
            }
            if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let take = &buf[..pos];
                if !in_overflow {
                    let allowed = LINE_CHUNK_LIMIT.saturating_sub(line.len());
                    if take.len() <= allowed {
                        line.extend_from_slice(take);
                        each(&String::from_utf8_lossy(&line));
                    } else {
                        let (head, rest) = take.split_at(allowed);
                        line.extend_from_slice(head);
                        let is_added = line.first() == Some(&b'+');
                        each(&String::from_utf8_lossy(&line));
                        if is_added {
                            emit_added_tail(&mut line, rest, LINE_CHUNK_LIMIT, OVERLAP, &mut each);
                        }
                    }
                } else if overflow_added && (!take.is_empty() || has_new_bytes) {
                    emit_added_tail(&mut line, take, LINE_CHUNK_LIMIT, OVERLAP, &mut each);
                }
                line.clear();
                in_overflow = false;
                overflow_added = false;
                has_new_bytes = false;
                reader.consume(pos + 1);
                continue;
            }
            if in_overflow && !overflow_added {
                let n = buf.len();
                reader.consume(n);
                continue;
            }
            let allowed = LINE_CHUNK_LIMIT.saturating_sub(line.len());
            let copy_len = buf.len().min(allowed);
            line.extend_from_slice(&buf[..copy_len]);
            if copy_len > 0 {
                has_new_bytes = true;
            }
            reader.consume(copy_len);
            if line.len() == LINE_CHUNK_LIMIT {
                if !in_overflow {
                    overflow_added = line.first() == Some(&b'+');
                    in_overflow = true;
                    each(&String::from_utf8_lossy(&line));
                } else if overflow_added {
                    each(&String::from_utf8_lossy(&line));
                }
                if overflow_added {
                    retain_overlap(&mut line, OVERLAP);
                    has_new_bytes = false;
                } else {
                    line.clear();
                }
            }
        };
        if read.is_err() {
            let _ = child.kill();
        }
        let error = errors.join().unwrap_or_default();
        (read, error)
    });
    let status = child.wait()?;
    read.context("Read git diff")?;
    ensure!(status.success(), "git diff: {}", error.trim());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, index: Option<&Path>, args: &[&str]) -> Result<Vec<u8>> {
        let mut command = git_command();
        command.args(args).current_dir(dir).stdin(Stdio::null());
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        let output = command.output().context("Start Git")?;
        ensure!(
            output.status.success(),
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(output.stdout)
    }

    /// A temporary repository with one commit, or None when the host has no
    /// Git identity: tests never invent commit authorship.
    fn repository() -> Option<tempfile::TempDir> {
        let identity = |key| {
            let output = Command::new("git")
                .args(["config", "--get", key])
                .output()
                .ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        };
        // The host's identity when configured; else a fixture-only one, so
        // these tests never skip silently (CI sets no identity).
        let name = identity("user.name").unwrap_or_else(|| "model-router tests".into());
        let email = identity("user.email").unwrap_or_else(|| "tests@localhost".into());
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            git(dir.path(), None, args).unwrap();
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", &name]);
        run(&["config", "user.email", &email]);
        fs::write(dir.path().join("tracked.txt"), "one\ntwo\n").unwrap();
        fs::write(dir.path().join("doomed.txt"), "bye\n").unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.log\n").unwrap();
        run(&["add", "."]);
        run(&[
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "Test fixture",
        ]);
        Some(dir)
    }

    /// A run directory inside the self-ignored metadata, as the router lays it out.
    fn run_dir(workspace: &Path) -> PathBuf {
        let dir = workspace.join(".model-router/runs/fixture");
        fs::create_dir_all(&dir).unwrap();
        fs::write(workspace.join(".model-router/.gitignore"), "*\n").unwrap();
        dir
    }

    /// Sorted `git status` lines, without the optional index refresh that
    /// would let status itself rewrite the index.
    fn status(dir: &Path) -> Vec<String> {
        let output = Command::new("git")
            .args(["status", "--porcelain", "--untracked-files=all"])
            .env("GIT_OPTIONAL_LOCKS", "0")
            .current_dir(dir)
            .output()
            .unwrap();
        let mut lines: Vec<_> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect();
        lines.sort();
        lines
    }

    #[test]
    fn snapshots_capture_the_working_tree_without_touching_the_index() {
        let Some(dir) = repository() else { return };
        let workspace = dir.path();
        let run_dir = run_dir(workspace);
        let index = fs::read(workspace.join(".git/index")).unwrap();
        let head = git(workspace, None, &["rev-parse", "HEAD", "HEAD^{tree}"]).unwrap();
        let base = tree(workspace, &run_dir).unwrap();
        assert!(String::from_utf8_lossy(&head).ends_with(&format!("{base}\n")));

        fs::write(workspace.join("tracked.txt"), "one\n2\nthree\n").unwrap();
        fs::write(workspace.join("new.txt"), "fresh\n").unwrap();
        fs::write(workspace.join("image.bin"), [0u8, 159, 146, 150]).unwrap();
        fs::write(workspace.join("ignored.log"), "noise\n").unwrap();
        fs::remove_file(workspace.join("doomed.txt")).unwrap();
        let after = tree(workspace, &run_dir).unwrap();
        let files = changes(workspace, &base, &after).unwrap();
        let summary: Vec<_> = files
            .iter()
            .map(|f| (f.path.as_str(), f.status, f.added, f.removed))
            .collect();
        assert_eq!(
            summary,
            [
                ("doomed.txt", ChangeKind::Deleted, Some(0), Some(1)),
                ("image.bin", ChangeKind::Added, None, None),
                ("new.txt", ChangeKind::Added, Some(1), Some(0)),
                ("tracked.txt", ChangeKind::Modified, Some(2), Some(1)),
            ]
        );
        let text = patch(workspace, &base, &after, PATCH_LIMIT).unwrap();
        assert!(text.contains("+fresh") && text.contains("-two") && !text.contains("noise"));
        let cut = patch(workspace, &base, &after, 40).unwrap();
        assert!(cut.contains("[Patch truncated after 40 bytes"), "{cut}");
        assert!(changes(workspace, &after, &after).unwrap().is_empty());

        // The real index, HEAD, and status are exactly as the user left them.
        assert_eq!(fs::read(workspace.join(".git/index")).unwrap(), index);
        assert_eq!(
            git(workspace, None, &["rev-parse", "HEAD", "HEAD^{tree}"]).unwrap(),
            head
        );
        assert_eq!(
            status(workspace),
            [
                " D doomed.txt",
                " M tracked.txt",
                "?? image.bin",
                "?? new.txt"
            ]
        );
        assert!(run_dir.join("snapshot.index").is_file());
    }

    #[test]
    fn unborn_head_starts_from_an_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), None, &["init", "-q"]).unwrap();
        let run_dir = run_dir(dir.path());
        fs::write(dir.path().join("first.txt"), "hello\n").unwrap();
        let first = tree(dir.path(), &run_dir).unwrap();
        fs::write(dir.path().join("second.txt"), "world\n").unwrap();
        let second = tree(dir.path(), &run_dir).unwrap();
        let files = changes(dir.path(), &first, &second).unwrap();
        assert_eq!(
            files
                .iter()
                .map(|f| (f.path.as_str(), f.status))
                .collect::<Vec<_>>(),
            [("second.txt", ChangeKind::Added)]
        );
        assert!(!dir.path().join(".git/index").exists());
        assert_eq!(status(dir.path()), ["?? first.txt", "?? second.txt"]);
    }

    #[test]
    fn refs_keep_snapshots_through_gc_until_the_run_is_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), None, &["init", "-q"]).unwrap();
        let run_dir = run_dir(dir.path());
        fs::write(dir.path().join("kept.txt"), "kept\n").unwrap();
        let kept = tree(dir.path(), &run_dir).unwrap();
        let exists = |object: &str| git(dir.path(), None, &["cat-file", "-e", object]).is_ok();
        git(dir.path(), None, &["gc", "-q", "--prune=now"]).unwrap();
        assert!(exists(&kept), "gc pruned a kept snapshot");
        let refs = git(dir.path(), None, &["for-each-ref", "refs/model-router/"]).unwrap();
        assert!(String::from_utf8_lossy(&refs).contains("refs/model-router/runs/fixture/"));
        forget(dir.path(), "fixture");
        let refs = git(dir.path(), None, &["for-each-ref", "refs/model-router/"]).unwrap();
        assert!(refs.is_empty());
        git(dir.path(), None, &["gc", "-q", "--prune=now"]).unwrap();
        assert!(!exists(&kept));
    }

    #[test]
    fn non_git_workspaces_have_no_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file.txt"), "plain").unwrap();
        assert!(!is_repository(dir.path()));
        assert_eq!(tree(dir.path(), dir.path()), None);
        assert!(!dir.path().join("snapshot.index").exists());
    }

    #[test]
    fn patch_trims_incomplete_utf8_and_changed_paths_tracks_edits() {
        let Some(dir) = repository() else { return };
        let workspace = dir.path();
        let run_dir = run_dir(workspace);
        let from = tree(workspace, &run_dir).unwrap();
        // Multi-byte UTF-8 line (4-byte emoji repeatedly) + deletion + addition.
        fs::write(
            workspace.join("tracked.txt"),
            format!("{}\n", "🦀".repeat(40)),
        )
        .unwrap();
        fs::write(workspace.join("extra.txt"), "hi\n").unwrap();
        fs::remove_file(workspace.join("doomed.txt")).unwrap();
        let to = tree(workspace, &run_dir).unwrap();
        let mut paths = changed_paths(workspace, &from, &to).unwrap();
        paths.sort();
        assert_eq!(paths, ["doomed.txt", "extra.txt", "tracked.txt"]);

        let header_len = patch(workspace, &from, &to, 10_000)
            .unwrap()
            .find('🦀')
            .unwrap();
        // Cut right inside a 4-byte character (+1, +2, +3 bytes).
        for offset in 1..=3 {
            let cut = patch(workspace, &from, &to, header_len + offset).unwrap();
            let prefix = cut.split("\n[Patch truncated").next().unwrap();
            assert!(
                !prefix.ends_with('\u{FFFD}'),
                "split UTF-8 char at +{offset}: {prefix:?}"
            );
        }
    }

    #[test]
    fn patch_lines_catches_secrets_across_chunk_boundary_and_ignores_mnemonic_prefix() {
        let Some(dir) = repository() else { return };
        let workspace = dir.path();
        git(workspace, None, &["config", "diff.mnemonicPrefix", "true"]).unwrap();
        let run_dir = run_dir(workspace);
        let from = tree(workspace, &run_dir).unwrap();

        let prefix_len = 1024 * 1024 - 12;
        let secret = format!("ghp_{}", "a1b2c3d4".repeat(4));
        let mut big = "x".repeat(prefix_len - 1);
        big.push(' ');
        big.push_str(&secret);
        big.push('\n');
        fs::write(workspace.join("big.txt"), big.as_bytes()).unwrap();
        let to = tree(workspace, &run_dir).unwrap();

        let mut scan = crate::policy::SecretScan::default();
        let mut seen_plus_header = false;
        patch_lines(workspace, &from, &to, |line| {
            if line == "+++ b/big.txt" {
                seen_plus_header = true;
            }
            scan.line(line);
        })
        .unwrap();
        assert!(seen_plus_header, "expected canonical b/ header");
        assert_eq!(scan.hits().len(), 1);
        assert_eq!(scan.hits()[0].file, "big.txt");
    }

    #[test]
    fn patch_lines_keeps_the_longest_policy_pattern_whole_across_chunks() {
        let Some(dir) = repository() else { return };
        let workspace = dir.path();
        let run_dir = run_dir(workspace);
        let from = tree(workspace, &run_dir).unwrap();
        // A 510-byte literal (the most a policy pattern holds after `i:`)
        // that straddles the first 1 MiB chunk boundary.
        let banned = format!("BANNED{}", "z".repeat(504));
        let mut line = "x".repeat(1024 * 1024 - 400);
        line.push_str(&banned);
        line.push_str(" tail\n");
        fs::write(workspace.join("big.txt"), line.as_bytes()).unwrap();
        let to = tree(workspace, &run_dir).unwrap();
        let policies = vec![crate::policy::DiffPolicy {
            name: "no-banned".into(),
            action: crate::policy::PolicyAction::Block,
            pattern: format!("i:{}", banned.to_lowercase()),
            paths: vec![],
            message: "No banned text".into(),
        }];
        let mut scan = crate::policy::PolicyScan::new(&policies);
        patch_lines(workspace, &from, &to, |line| scan.line(line)).unwrap();
        assert_eq!(scan.hits().len(), 1, "{:?}", scan.hits());
    }

    #[test]
    fn snapshot_and_repo_info_do_not_run_repo_local_filters() {
        let Some(dir) = repository() else { return };
        let workspace = dir.path();
        let marker = workspace.join("pwned-by-filter");
        let run_git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(workspace)
                .status()
                .unwrap();
            assert!(status.success(), "{args:?}");
        };
        run_git(&[
            "config",
            "--local",
            "filter.evil.clean",
            &format!("sh -c 'touch {:?}; cat'", marker.display().to_string()),
        ]);
        // Subsection containing `=` must also be neutralized (not split by `-c`).
        run_git(&[
            "config",
            "--local",
            "filter.a=b.clean",
            &format!("sh -c 'touch {:?}; cat'", marker.display().to_string()),
        ]);
        run_git(&[
            "config",
            "--local",
            "merge.pwn.driver",
            &format!("sh -c 'touch {:?}'", marker.display().to_string()),
        ]);
        fs::write(
            workspace.join(".gitattributes"),
            "*.txt filter=evil\n*.eq filter=a=b\n*.mrg merge=pwn\n",
        )
        .unwrap();
        fs::write(workspace.join("filtered.txt"), "hello\n").unwrap();
        fs::write(workspace.join("subsection.eq"), "hello\n").unwrap();

        let info = crate::storage::repo_info(workspace).expect("repo info");
        assert!(info.changes >= 3);
        assert!(
            !marker.exists(),
            "repo_info executed repo-local filter command"
        );

        let run_dir = run_dir(workspace);
        assert!(tree(workspace, &run_dir).is_some());
        assert!(
            !marker.exists(),
            "snapshot::tree executed repo-local filter command"
        );

        // Verify merge.pwn.driver is omitted from SanitizedGitDir, and that a
        // concurrent `.git/config` mutation after constructing SanitizedGitDir
        // cannot affect commands executed with it.
        let sanitized = SanitizedGitDir::new(workspace).unwrap();
        let late_marker = workspace.join("late-toctou-pwned");
        run_git(&[
            "config",
            "--local",
            "filter.late.clean",
            &format!("sh -c 'touch {:?}; cat'", late_marker.display().to_string()),
        ]);
        fs::write(workspace.join(".gitattributes"), "*.late filter=late\n").unwrap();
        fs::write(workspace.join("race.late"), "hello\n").unwrap();
        let effective = sanitized
            .command(workspace)
            .args(["config", "--get", "merge.pwn.driver"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&effective.stdout).trim().is_empty(),
            "merge.pwn.driver was not omitted: {:?}",
            String::from_utf8_lossy(&effective.stdout)
        );
        let race_index = workspace.join(".model-router/runs/fixture/race.index");
        let add_out = sanitized
            .command(workspace)
            .args(["add", "-A"])
            .env("GIT_INDEX_FILE", &race_index)
            .output()
            .unwrap();
        assert!(add_out.status.success());
        assert!(
            !late_marker.exists(),
            "SanitizedGitDir reloaded mutated .git/config"
        );

        // Non-UTF-8 filter subsection names must also be preserved and neutralized.
        #[cfg(unix)]
        {
            let cfg_path = workspace.join(".git/config");
            let mut cfg = fs::read(&cfg_path).unwrap();
            cfg.extend_from_slice(b"\n[filter \"bad\xffname\"]\n\tclean = sh -c 'touch ");
            cfg.extend_from_slice(marker.to_string_lossy().as_bytes());
            cfg.extend_from_slice(b"; cat'\n");
            fs::write(&cfg_path, cfg).unwrap();
            let mut attrs = fs::read(workspace.join(".gitattributes")).unwrap();
            attrs.extend_from_slice(b"*.raw filter=bad\xffname\n");
            fs::write(workspace.join(".gitattributes"), attrs).unwrap();
            fs::write(workspace.join("bytes.raw"), "payload\n").unwrap();

            assert!(tree(workspace, &run_dir).is_some());
            assert!(
                !marker.exists(),
                "snapshot::tree executed non-UTF-8 repo-local filter"
            );
        }

        // Both optional and required local smudge filters must fail worktree
        // creation closed instead of silently producing unprocessed files.
        let wt_dir = repository().unwrap();
        let wt_ws = wt_dir.path();
        let wt_marker = wt_ws.join("smudge-pwned");
        let run_wt_git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(wt_ws)
                .status()
                .unwrap();
            assert!(status.success(), "{args:?}");
        };
        fs::write(wt_ws.join(".gitattributes"), "*.lfs filter=lfs\n").unwrap();
        fs::write(wt_ws.join("asset.lfs"), "pointer-placeholder\n").unwrap();
        run_wt_git(&["add", ".gitattributes", "asset.lfs"]);
        run_wt_git(&["commit", "-q", "-m", "add lfs file"]);
        run_wt_git(&[
            "config",
            "--local",
            "filter.lfs.smudge",
            &format!("sh -c 'touch {:?}; cat'", wt_marker.display().to_string()),
        ]);
        // Optional (required unset) local smudge filter must still refuse.
        let opt_err = crate::storage::create_worktree(wt_ws, "wt-optional", true).unwrap_err();
        assert!(
            format!("{opt_err:#}").contains("repo-local Git smudge filter"),
            "{opt_err:#}"
        );
        assert!(
            !wt_marker.exists(),
            "worktree add executed optional repo-local smudge filter"
        );
        // Fan-out worktree creation must also refuse.
        let fanout_wt = wt_ws.join(".model-router/candidates/test/c1");
        let fanout_err = crate::fanout::add_worktree(wt_ws, &fanout_wt, "HEAD").unwrap_err();
        assert!(
            format!("{fanout_err:#}").contains("repo-local Git smudge filter"),
            "{fanout_err:#}"
        );
        assert!(
            !wt_marker.exists(),
            "fanout::add_worktree executed optional repo-local smudge filter"
        );
        // Required local smudge filter also refuses.
        run_wt_git(&["config", "--local", "filter.lfs.required", "true"]);
        assert!(
            crate::storage::create_worktree(wt_ws, "wt-required", true).is_err(),
            "worktree add must fail closed when a required local smudge filter is disabled"
        );
        assert!(
            !wt_marker.exists(),
            "worktree add executed repo-local smudge filter"
        );
    }

    #[test]
    fn sanitized_git_preserves_local_excludes_file_and_split_indexes() {
        let Some(dir) = repository() else { return };
        let workspace = dir.path();
        let run_dir = run_dir(workspace);
        let excludes = workspace.join(".local-ignore");
        fs::write(&excludes, "*.secret-local\n.local-ignore\n").unwrap();
        git(
            workspace,
            None,
            &["config", "--local", "core.excludesFile", ".local-ignore"],
        )
        .unwrap();
        fs::write(workspace.join("ignored.secret-local"), "do not stage\n").unwrap();

        let before = tree(workspace, &run_dir).unwrap();
        fs::write(workspace.join("visible.txt"), "hello\n").unwrap();
        let after = tree(workspace, &run_dir).unwrap();
        let changed = changed_paths(workspace, &before, &after).unwrap();
        assert_eq!(changed, vec!["visible.txt"]);

        // Enable split-index mode in the repository and verify status, worktree
        // creation, and consecutive snapshots all succeed.
        git(
            workspace,
            None,
            &["config", "--local", "core.splitIndex", "true"],
        )
        .unwrap();
        git(workspace, None, &["update-index", "--split-index"]).unwrap();
        fs::remove_file(workspace.join("visible.txt")).unwrap();
        let info = crate::storage::repo_info(workspace).expect("repo_info on split index");
        assert_eq!(info.changes, 0, "unexpected dirty state: {info:?}");
        let snap1 = tree(workspace, &run_dir).expect("first snapshot on split index");
        fs::write(workspace.join("after-split.txt"), "ok\n").unwrap();
        let snap2 = tree(workspace, &run_dir).expect("second snapshot on split index");
        assert_eq!(
            changed_paths(workspace, &snap1, &snap2).unwrap(),
            vec!["after-split.txt"]
        );
        fs::remove_file(workspace.join("after-split.txt")).unwrap();
        let wt = crate::storage::create_worktree(workspace, "split-wt", true)
            .expect("create_worktree on split index");
        assert!(wt.join("tracked.txt").is_file());
    }

    #[test]
    fn sanitized_git_preserves_numeric_required_and_reports_clean_filter_status() {
        let Some(dir) = repository() else { return };
        let workspace = dir.path();
        let run_dir = run_dir(workspace);

        // Numeric truthy `filter.*.required = 2` must stay required in the
        // sanitized config so Git fails closed.
        fs::write(workspace.join(".gitattributes"), "*.num filter=numreq\n").unwrap();
        fs::write(workspace.join("data.num"), "payload\n").unwrap();
        git(
            workspace,
            None,
            &["config", "--local", "filter.numreq.clean", "cat"],
        )
        .unwrap();
        git(
            workspace,
            None,
            &["config", "--local", "filter.numreq.required", "2"],
        )
        .unwrap();
        assert!(
            try_tree(workspace, &run_dir).is_err(),
            "filter.*.required = 2 must fail closed"
        );

        // Clean checkout with a local clean filter that rewrites file content:
        // when stat info is stale, create_worktree must report the disabled
        // local filter instead of asking the user to commit or stash.
        let clean_dir = repository().unwrap();
        let clean_ws = clean_dir.path();
        fs::write(clean_ws.join(".gitattributes"), "*.upper filter=upper\n").unwrap();
        fs::write(clean_ws.join("note.upper"), "hello\n").unwrap();
        git(
            clean_ws,
            None,
            &["config", "--local", "filter.upper.clean", "tr a-z A-Z"],
        )
        .unwrap();
        git(clean_ws, None, &["add", ".gitattributes", "note.upper"]).unwrap();
        git(
            clean_ws,
            None,
            &["-c", "commit.gpgsign=false", "commit", "-qm", "filtered"],
        )
        .unwrap();
        // Rewrite the same working-tree bytes to invalidate index stat cache
        // while keeping native `git status` clean.
        fs::write(clean_ws.join("note.upper"), "hello\n").unwrap();
        git(clean_ws, None, &["update-index", "--really-refresh"]).ok();
        // Force a stale index entry so Git re-reads note.upper from disk.
        let _ = Command::new("touch")
            .args(["-t", "202001010000", "note.upper"])
            .current_dir(clean_ws)
            .status();
        let err = crate::storage::create_worktree(clean_ws, "wt-clean-filter", true).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("repo-local Git clean filter") && msg.contains("upper"),
            "expected disabled clean filter error, got: {msg}"
        );
    }

    #[test]
    fn clean_worktree_check_sees_dirty_submodules_without_their_filters() {
        let Some(sub_dir) = repository() else { return };
        let Some(parent_dir) = repository() else {
            return;
        };
        let parent = parent_dir.path();
        git(
            parent,
            None,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &sub_dir.path().display().to_string(),
                "sub",
            ],
        )
        .unwrap();
        git(
            parent,
            None,
            &["-c", "commit.gpgsign=false", "commit", "-qm", "sub"],
        )
        .unwrap();
        crate::storage::create_worktree(parent, "wt-sub-clean", true).unwrap();
        // A submodule-local clean filter must not run during the check.
        let marker = parent.join("filter-ran");
        let sub = parent.join("sub");
        let clean = format!("touch '{}'; cat", marker.display());
        git(&sub, None, &["config", "filter.evil.clean", &clean]).unwrap();
        fs::write(sub.join(".gitattributes"), "* filter=evil\n").unwrap();
        let err = crate::storage::create_worktree(parent, "wt-sub-dirty", true).unwrap_err();
        assert!(
            format!("{err:#}").contains("is not clean"),
            "unexpected error: {err:#}"
        );
        assert!(!marker.exists(), "submodule-local clean filter ran");
    }

    #[test]
    fn sanitized_git_preserves_include_if_gitdir_global_filter() {
        let Some(dir) = repository() else { return };
        let workspace = dir.path();
        let cfg_dir = tempfile::tempdir().unwrap();
        let included = cfg_dir.path().join("work.gitconfig");
        fs::write(
            &included,
            "[filter \"myglobal\"]\n\tclean = tr a-z A-Z\n\trequired = true\n",
        )
        .unwrap();
        let global = cfg_dir.path().join("global.gitconfig");
        let repo_canon = fs::canonicalize(workspace).unwrap();
        fs::write(
            &global,
            format!(
                "[includeIf \"gitdir:{}/\"]\n\tpath = {}\n",
                repo_canon.display(),
                included.display()
            ),
        )
        .unwrap();

        // Run `git config --show-scope -z --list` with GIT_CONFIG_GLOBAL set
        // to our includeIf config and verify inspect_repo_config carries the
        // matched global filter into the shadow config.
        let out = git_command()
            .args(["config", "--show-scope", "-z", "--list"])
            .env("GIT_CONFIG_GLOBAL", &global)
            .current_dir(&repo_canon)
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("filter.myglobal.clean"),
            "test fixture includeIf did not match: {}",
            String::from_utf8_lossy(&out.stdout)
        );

        // Verify the sanitized config written from that scope output works when
        // GIT_DIR points outside the includeIf path, keeps `lfs.storage`
        // pointed at the real repository's `.git/lfs`, preserves local
        // `required` even when global defines a smudge command under the same
        // driver name, and rejects partial-clone repos clearly.
        let prev = std::env::var_os("GIT_CONFIG_GLOBAL");
        let common_dir = repo_canon.join(".git");
        let parsed =
            parse_and_build_sanitized_config(&repo_canon, &common_dir, &out.stdout).unwrap();
        let shadow_dir = tempfile::tempdir().unwrap();
        fs::write(shadow_dir.path().join("config"), &parsed.sanitized_config).unwrap();
        let read_shadow = |key: &str| {
            let mut cmd = git_command();
            cmd.args(["config", "--file"])
                .arg(shadow_dir.path().join("config"))
                .args(["--get", key]);
            if let Some(p) = &prev {
                cmd.env("GIT_CONFIG_GLOBAL", p);
            }
            let val = cmd.output().unwrap();
            assert!(val.status.success(), "missing {key}");
            String::from_utf8_lossy(&val.stdout).trim().to_owned()
        };
        assert_eq!(read_shadow("filter.myglobal.clean"), "tr a-z A-Z");
        assert_eq!(
            PathBuf::from(read_shadow("lfs.storage")),
            common_dir.join("lfs")
        );

        // Global smudge + local clean + local required must keep `required`.
        let mixed_raw = b"global\0filter.mix.smudge\ncat\0local\0filter.mix.clean\ncat\0local\0filter.mix.required\n2\0";
        let mixed = parse_and_build_sanitized_config(&repo_canon, &common_dir, mixed_raw).unwrap();
        fs::write(
            shadow_dir.path().join("mixed.config"),
            &mixed.sanitized_config,
        )
        .unwrap();
        let mixed_req = git_command()
            .args(["config", "--file"])
            .arg(shadow_dir.path().join("mixed.config"))
            .args(["--bool", "--get", "filter.mix.required"])
            .output()
            .unwrap();
        assert!(mixed_req.status.success());
        assert_eq!(String::from_utf8_lossy(&mixed_req.stdout).trim(), "true");

        // Repo-local LFS endpoint URLs and remote selection survive.
        let lfs_raw = b"local\0remote.origin.url\nhttps://example.test/r.git\0local\0lfs.url\nhttps://lfs.example.test\0worktree\0filter.wt.smudge\ncat\0local\0filter.lc.smudge\ncat\0local\0remote.lfsdefault\nupstream\0local\0branch.main.remote\nupstream\0global\0filter.off.clean\ncat\0local\0filter.off.clean\n\0";
        let lfs = parse_and_build_sanitized_config(&repo_canon, &common_dir, lfs_raw).unwrap();
        fs::write(shadow_dir.path().join("lfs.config"), &lfs.sanitized_config).unwrap();
        let get = |key: &str| {
            let out = git_command()
                .args(["config", "--file"])
                .arg(shadow_dir.path().join("lfs.config"))
                .args(["--get", key])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        assert_eq!(get("remote.origin.url"), "https://example.test/r.git");
        assert_eq!(get("lfs.url"), "https://lfs.example.test");
        assert_eq!(get("remote.lfsdefault"), "upstream");
        assert_eq!(get("branch.main.remote"), "upstream");
        assert_eq!(get("filter.wt.smudge"), "");
        // An empty local value still turns the global command off.
        assert_eq!(get("filter.off.clean"), "");
        assert!(!lfs.local_clean_filters.contains(&b"off".to_vec()));
        assert_eq!(
            lfs.local_smudge_filters,
            vec![b"wt".to_vec(), b"lc".to_vec()]
        );

        // Partial clones (extensions.partialClone or remote.*.promisor) fail closed.
        for partial_raw in [
            &b"local\0core.repositoryformatversion\n1\0local\0extensions.partialclone\norigin\0"[..],
            &b"local\0remote.origin.promisor\ntrue\0"[..],
        ] {
            let err = parse_and_build_sanitized_config(&repo_canon, &common_dir, partial_raw)
                .err()
                .expect("partial clone must fail closed");
            assert!(
                format!("{err:#}").contains("partial-clone"),
                "unexpected error: {err:#}"
            );
        }

        // Relative trusted `lfs.storage` fails closed instead of writing into
        // the temporary shadow GIT_DIR.
        let rel_lfs_raw = b"global\0lfs.storage\nrelative-lfs-cache\0";
        let rel_lfs_err = parse_and_build_sanitized_config(&repo_canon, &common_dir, rel_lfs_raw)
            .err()
            .expect("relative lfs.storage must fail closed");
        assert!(
            format!("{rel_lfs_err:#}").contains("relative `lfs.storage`"),
            "unexpected error: {rel_lfs_err:#}"
        );

        // Disabled filters named `set` / `unset` / `unspecified` must not be
        // skipped as check-attr status labels.
        let ambig_dir = repository().unwrap();
        let ambig_ws = ambig_dir.path();
        fs::write(ambig_ws.join(".gitattributes"), "*.ambig filter=set\n").unwrap();
        fs::write(ambig_ws.join("item.ambig"), "raw\n").unwrap();
        git(ambig_ws, None, &["add", ".gitattributes", "item.ambig"]).unwrap();
        git(
            ambig_ws,
            None,
            &["-c", "commit.gpgsign=false", "commit", "-qm", "ambig"],
        )
        .unwrap();
        git(
            ambig_ws,
            None,
            &["config", "--local", "filter.set.smudge", "cat"],
        )
        .unwrap();
        let ambig_err =
            refuse_suppressed_smudge_filters(&SanitizedGitDir::new(ambig_ws).unwrap(), "HEAD")
                .unwrap_err();
        assert!(
            format!("{ambig_err:#}").contains("`set`"),
            "expected filter `set` refusal, got: {ambig_err:#}"
        );

        // Checkout paths containing line breaks must still resolve git dirs cleanly.
        #[cfg(unix)]
        {
            let parent = tempfile::tempdir().unwrap();
            let nl_repo = parent.path().join("line1\nline2");
            fs::create_dir(&nl_repo).unwrap();
            git(&nl_repo, None, &["init", "-q"]).unwrap();
            fs::write(nl_repo.join("hello.txt"), "hi\n").unwrap();
            let nl_run = run_dir(&nl_repo);
            assert!(
                try_tree(&nl_repo, &nl_run).unwrap().is_some(),
                "snapshot in newline path should succeed"
            );
        }
    }

    #[test]
    fn a_new_worktree_keeps_the_source_checkout_s_sparse_rules() {
        let Some(dir) = repository() else { return };
        let workspace = dir.path();
        fs::create_dir(workspace.join("kept")).unwrap();
        fs::create_dir(workspace.join("left-out")).unwrap();
        fs::write(workspace.join("kept/file.txt"), "kept\n").unwrap();
        fs::write(workspace.join("left-out/file.txt"), "left out\n").unwrap();
        git(workspace, None, &["add", "."]).unwrap();
        git(
            workspace,
            None,
            &["-c", "commit.gpgsign=false", "commit", "-qm", "Directories"],
        )
        .unwrap();
        git(workspace, None, &["sparse-checkout", "set", "kept"]).unwrap();
        assert!(!workspace.join("left-out").exists());

        let wt = crate::storage::create_worktree(workspace, "sparse-wt", true).unwrap();
        // Git before 2.36 does not copy sparse settings to a new worktree.
        let copied = git(&wt, None, &["config", "--get", "core.sparseCheckout"]).is_ok();
        assert!(wt.join("kept/file.txt").is_file());
        assert!(wt.join("tracked.txt").is_file());
        assert_eq!(wt.join("left-out").exists(), !copied);
        assert!(
            git(&wt, None, &["status", "--porcelain"])
                .unwrap()
                .is_empty()
        );

        // A candidate worktree made from that one keeps them too.
        let candidate = workspace.join(".model-router/candidates/run/c1");
        crate::fanout::add_worktree(&wt, &candidate, "HEAD").unwrap();
        assert!(candidate.join("kept/file.txt").is_file());
        assert_eq!(candidate.join("left-out").exists(), !copied);
    }

    #[test]
    fn a_failed_worktree_fill_rolls_back_only_its_own_worktree() {
        let Some(dir) = repository() else { return };
        let workspace = dir.path();
        // Another worktree whose directory is missing for now, and whose
        // record `git worktree prune` would drop.
        git(
            workspace,
            None,
            &["config", "gc.worktreePruneExpire", "now"],
        )
        .unwrap();
        let other = crate::storage::create_worktree(workspace, "other", true).unwrap();
        let away = workspace.join(".model-router/other-unmounted");
        fs::rename(&other, &away).unwrap();

        // Filling the next one fails: a tracked file needs a disabled filter.
        git(workspace, None, &["config", "filter.local.smudge", "cat"]).unwrap();
        fs::write(workspace.join(".gitattributes"), "*.txt filter=local\n").unwrap();
        git(workspace, None, &["add", ".gitattributes"]).unwrap();
        git(
            workspace,
            None,
            &["-c", "commit.gpgsign=false", "commit", "-qm", "Attributes"],
        )
        .unwrap();
        crate::storage::create_worktree(workspace, "fails", false).unwrap_err();
        let candidate = workspace.join(".model-router/candidates/run/c1");
        crate::fanout::add_worktree(workspace, &candidate, "HEAD").unwrap_err();

        let listed = git(workspace, None, &["worktree", "list", "--porcelain"]).unwrap();
        let listed = String::from_utf8_lossy(&listed);
        assert!(!listed.contains("/fails\n"), "{listed}");
        assert!(!listed.contains("/c1\n"), "{listed}");
        assert!(!workspace.join(".model-router/worktrees/fails").exists());
        assert!(!candidate.exists());
        git(
            workspace,
            None,
            &["rev-parse", "--verify", "model-router/fails"],
        )
        .unwrap_err();
        // The other worktree works again once its directory is back.
        fs::rename(&away, &other).unwrap();
        assert!(
            git(&other, None, &["status", "--porcelain"])
                .unwrap()
                .is_empty()
        );
    }
}
