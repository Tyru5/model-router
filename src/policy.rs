//! Workflow path policies (`protected_paths`), declarative diff policies
//! (`policies`), and pre-ship secret scanning.
//! No shell evaluates any command; external scanners run as argument vectors.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{BufRead, BufReader, Read},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Matches a repository-relative path against a glob pattern supporting `*`
/// (within one segment), `**` (zero or more path segments), and `?` (one
/// character within a segment). Leading `/` is ignored on both sides.
pub fn matches_glob(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim().trim_start_matches('/');
    let path = path.trim_start_matches('/');
    if pattern.is_empty() || path.is_empty() {
        return false;
    }
    let mut pat_segs: Vec<&str> = Vec::new();
    for seg in pattern.split('/').filter(|s| !s.is_empty()) {
        if seg == "**" && pat_segs.last() == Some(&"**") {
            continue;
        }
        pat_segs.push(seg);
    }
    let path_segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match_segments(&pat_segs, &path_segs)
}

fn match_segments(pat: &[&str], path: &[&str]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < path.len() {
        if pi < pat.len() && pat[pi] == "**" {
            star = Some((pi, ti));
            pi += 1;
        } else if pi < pat.len() && match_segment(pat[pi], path[ti]) {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, ti));
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == "**" {
        pi += 1;
    }
    pi == pat.len()
}

fn match_segment(pattern: &str, text: &str) -> bool {
    if pattern.is_ascii() && text.is_ascii() {
        return match_segment_slice(pattern.as_bytes(), text.as_bytes(), b'*', b'?');
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    match_segment_slice(&p, &t, '*', '?')
}

fn match_segment_slice<T: Copy + Eq>(p: &[T], t: &[T], star_tok: T, q_tok: T) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        // `*` in the pattern is always a wildcard, also when the text has a
        // literal `*` there; there is no escape syntax.
        if pi < p.len() && p[pi] == star_tok {
            star = Some((pi, ti));
            pi += 1;
        } else if pi < p.len() && (p[pi] == q_tok || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, ti));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == star_tok {
        pi += 1;
    }
    pi == p.len()
}

/// Returns `(path, pattern)` pairs for every path matching a protected glob.
pub fn protected_matches(patterns: &[String], paths: &[String]) -> Vec<(String, String)> {
    let mut hits = vec![];
    for path in paths {
        if let Some(pattern) = patterns.iter().find(|pat| matches_glob(pat, path)) {
            hits.push((path.clone(), pattern.clone()));
        }
    }
    hits
}

/// The protected paths among `paths`, as `PATH (matches PATTERN), …`; None
/// when there are none.
pub fn protected_detail(patterns: &[String], paths: &[String]) -> Option<String> {
    let hits = protected_matches(patterns, paths);
    (!hits.is_empty()).then(|| {
        hits.iter()
            .map(|(path, pat)| format!("{path} (matches {pat})"))
            .collect::<Vec<_>>()
            .join(", ")
    })
}

/// What a declarative diff policy does when it matches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    /// Block stage completion, candidate selection, apply, and ship on any match
    /// in added diff lines.
    #[default]
    Block,
    /// Inject the policy message into review stage prompts when matching files
    /// (and optional line pattern) are touched.
    Advise,
}
impl PolicyAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Advise => "advise",
        }
    }
}

/// Declarative workflow diff policy (`[[workflows.policies]]`).
///
/// Pattern syntax (bounded, non-backtracking — never ReDoS):
/// - Plain text (default): case-sensitive literal substring match against each
///   added diff line (safe for code containing `*` or `?`).
/// - `i:<text>`: case-insensitive literal substring match.
/// - `word:<text>`: case-sensitive match bounded by non-identifier characters
///   (`!(alphanumeric || '_')`).
/// - `any:<a|b|c>`: matches if any non-empty `|`-separated literal substring
///   matches.
/// - `glob:<pattern>`: whole-line glob match (`*` = any chars, `?` = one char),
///   evaluated with the linear/bounded two-pointer matcher.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiffPolicy {
    pub name: String,
    pub action: PolicyAction,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    pub message: String,
}

impl DiffPolicy {
    /// Whether this policy applies to repository-relative `path`. An empty
    /// `paths` list matches every file.
    pub fn matches_path(&self, path: &str) -> bool {
        self.paths.is_empty() || self.paths.iter().any(|pat| matches_glob(pat, path))
    }
}

/// Maximum byte length of a single diff line inspected by line pattern matching.
const MAX_POLICY_LINE_BYTES: usize = 16 * 1024;

/// Matches an added diff line against a declarative policy `pattern`.
/// Bounded and non-backtracking (no regex engine, zero ReDoS).
pub fn matches_line_pattern(pattern: &str, line: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let line = if line.len() > MAX_POLICY_LINE_BYTES {
        let mut cut = MAX_POLICY_LINE_BYTES;
        while cut > 0 && !line.is_char_boundary(cut) {
            cut -= 1;
        }
        &line[..cut]
    } else {
        line
    };
    if let Some(rest) = pattern.strip_prefix("i:") {
        if rest.is_empty() {
            return false;
        }
        return line.to_lowercase().contains(&rest.to_lowercase());
    }
    if let Some(rest) = pattern.strip_prefix("glob:") {
        if rest.is_empty() {
            return false;
        }
        return match_segment(rest, line);
    }
    if let Some(rest) = pattern.strip_prefix("word:") {
        if rest.is_empty() {
            return false;
        }
        return contains_word(line, rest);
    }
    if let Some(rest) = pattern.strip_prefix("any:") {
        return rest
            .split('|')
            .filter(|part| !part.is_empty())
            .any(|part| line.contains(part));
    }
    line.contains(pattern)
}

fn is_ident_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn contains_word(line: &str, word: &str) -> bool {
    let mut offset = 0;
    while let Some(pos) = line[offset..].find(word) {
        let start = offset + pos;
        let end = start + word.len();
        let before_ok = line[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident_char(c));
        let after_ok = line[end..].chars().next().is_none_or(|c| !is_ident_char(c));
        if before_ok && after_ok {
            return true;
        }
        let step = line[start..].chars().next().map_or(1, char::len_utf8);
        offset = start + step;
    }
    false
}

/// One declarative diff policy match on an added diff line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyHit {
    pub policy: String,
    pub file: String,
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for PolicyHit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{} [{}] {}",
            self.file, self.line, self.policy, self.message
        )
    }
}

/// Streams a unified Git diff line by line and checks all `block` policies
/// against added lines in matching paths.
#[derive(Debug)]
pub struct PolicyScan<'a> {
    policies: Vec<&'a DiffPolicy>,
    state: DiffState,
    file: String,
    line_no: usize,
    hits: Vec<PolicyHit>,
    more: usize,
}

impl<'a> PolicyScan<'a> {
    pub fn new(policies: &'a [DiffPolicy]) -> Self {
        Self::with_action(policies, PolicyAction::Block)
    }

    pub fn with_action(policies: &'a [DiffPolicy], action: PolicyAction) -> Self {
        Self {
            policies: policies
                .iter()
                .filter(|p| p.action == action && !p.pattern.is_empty())
                .collect(),
            state: DiffState::Header,
            file: String::new(),
            line_no: 0,
            hits: vec![],
            more: 0,
        }
    }

    pub fn is_empty_rules(&self) -> bool {
        self.policies.is_empty()
    }

    /// Feeds the next diff line (without its trailing newline).
    pub fn line(&mut self, raw: &str) {
        if self.policies.is_empty() {
            return;
        }
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if raw.starts_with("diff --git ") {
            self.state = DiffState::Header;
            self.file.clear();
            return;
        }
        if let Some(hunk) = raw.strip_prefix("@@ ") {
            self.state = DiffState::Hunk;
            self.line_no = hunk
                .split('+')
                .nth(1)
                .and_then(|plus| plus.split([',', ' ']).next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(1);
            return;
        }
        match self.state {
            DiffState::Header => {
                if let Some(rest) = raw.strip_prefix("+++ ") {
                    let rest = rest.strip_suffix('\t').unwrap_or(rest);
                    self.file = rest.strip_prefix("b/").unwrap_or(rest).to_owned();
                }
            }
            DiffState::Hunk => {
                if let Some(added) = raw.strip_prefix('+') {
                    for policy in &self.policies {
                        if policy.matches_path(&self.file)
                            && matches_line_pattern(&policy.pattern, added)
                        {
                            if self.hits.len() < MAX_HITS {
                                self.hits.push(PolicyHit {
                                    policy: policy.name.clone(),
                                    file: self.file.clone(),
                                    line: self.line_no,
                                    message: policy.message.clone(),
                                });
                            } else {
                                self.more += 1;
                            }
                        }
                    }
                    self.line_no = self.line_no.saturating_add(1);
                } else if raw.starts_with(' ') || raw.is_empty() {
                    self.line_no = self.line_no.saturating_add(1);
                }
            }
        }
    }

    pub fn hits(&self) -> &[PolicyHit] {
        &self.hits
    }

    pub fn ensure_clean(&self, found_in: &str, refusing: &str) -> Result<()> {
        if self.hits.is_empty() {
            return Ok(());
        }
        let mut list = self
            .hits
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        if self.more > 0 {
            list.push_str(&format!("; and {} more", self.more));
        }
        bail!("Diff policy blocked{found_in}; {refusing}: {list}")
    }
}

/// Returns whether `policies` contains any blocking diff policies.
pub fn has_block_policies(policies: &[DiffPolicy]) -> bool {
    policies
        .iter()
        .any(|p| p.action == PolicyAction::Block && !p.pattern.is_empty())
}

/// Scans a unified diff string against blocking `policies`.
pub fn scan_diff_policies(policies: &[DiffPolicy], diff: &str) -> Vec<PolicyHit> {
    let mut scan = PolicyScan::new(policies);
    for raw in diff.lines() {
        scan.line(raw);
    }
    scan.hits
}

/// Streams a saved patch file through `PolicyScan`, failing closed if the file
/// cannot be read.
pub fn scan_patch_file_policies(
    policies: &[DiffPolicy],
    patch: &Path,
    found_in: &str,
    refusing: &str,
) -> Result<()> {
    if !has_block_policies(policies) {
        return Ok(());
    }
    let file = File::open(patch).with_context(|| {
        format!(
            "Read patch {} to verify workflow policies; {refusing}",
            patch.display()
        )
    })?;
    let mut reader = BufReader::new(file);
    let mut scan = PolicyScan::new(policies);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let read = reader.read_until(b'\n', &mut buf).with_context(|| {
            format!(
                "Read patch {} to verify workflow policies; {refusing}",
                patch.display()
            )
        })?;
        if read == 0 {
            break;
        }
        if buf.last() == Some(&b'\n') {
            buf.pop();
        }
        let line = String::from_utf8_lossy(&buf);
        scan.line(&line);
    }
    scan.ensure_clean(found_in, refusing)
}

/// An advisory policy matched against the run's changed files (and optional diff
/// pattern), ready to inject into a review stage prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvisoryMatch {
    pub name: String,
    pub message: String,
    pub files: Vec<String>,
}

/// Evaluates `advise` policies against `changed_files`, optionally using
/// `diff_lines` for policies that also specify a `pattern`.
pub fn advisory_matches(
    policies: &[DiffPolicy],
    changed_files: &[String],
    diff_hits: &[PolicyHit],
) -> Vec<AdvisoryMatch> {
    let mut out = vec![];
    for policy in policies.iter().filter(|p| p.action == PolicyAction::Advise) {
        if !policy.pattern.is_empty() {
            let mut files: Vec<String> = diff_hits
                .iter()
                .filter(|h| h.policy == policy.name)
                .map(|h| h.file.clone())
                .collect();
            files.sort();
            files.dedup();
            if !files.is_empty() {
                out.push(AdvisoryMatch {
                    name: policy.name.clone(),
                    message: policy.message.clone(),
                    files,
                });
            }
        } else {
            let mut files: Vec<String> = changed_files
                .iter()
                .filter(|path| policy.matches_path(path))
                .cloned()
                .collect();
            files.sort();
            files.dedup();
            if !files.is_empty() {
                out.push(AdvisoryMatch {
                    name: policy.name.clone(),
                    message: policy.message.clone(),
                    files,
                });
            }
        }
    }
    out
}

/// A high-confidence secret signature found on an added diff line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretHit {
    pub file: String,
    pub line: usize,
    pub kind: &'static str,
}

impl std::fmt::Display for SecretHit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.file, self.line, self.kind)
    }
}

/// Where a line of a unified Git diff sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffState {
    /// Between `diff --git` and the file's first `@@`: `---`/`+++` name the
    /// file here, and only here.
    Header,
    /// Inside a file's hunks, where every `+` line is added content, even
    /// one whose content starts with `++ ` or `-- `.
    Hunk,
}

/// Scans the added lines of a unified Git diff, fed one line at a time, for
/// high-confidence secret patterns. A diff of any size can be streamed
/// through it. Never keeps the matched value.
#[derive(Debug)]
pub struct SecretScan {
    state: DiffState,
    file: String,
    line_no: usize,
    hits: Vec<SecretHit>,
    /// Hits beyond `MAX_HITS`, counted only.
    more: usize,
}

/// Hits kept for the message; the rest are counted.
const MAX_HITS: usize = 20;

impl Default for SecretScan {
    fn default() -> Self {
        Self {
            state: DiffState::Header,
            file: String::new(),
            line_no: 0,
            hits: vec![],
            more: 0,
        }
    }
}

impl SecretScan {
    /// The next diff line, without its newline.
    pub fn line(&mut self, raw: &str) {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        // Hunk content always starts with ' ', '+', '-', or a backslash, so these
        // two are never content.
        if raw.starts_with("diff --git ") {
            self.state = DiffState::Header;
            self.file.clear();
            return;
        }
        if let Some(hunk) = raw.strip_prefix("@@ ") {
            self.state = DiffState::Hunk;
            self.line_no = hunk
                .split('+')
                .nth(1)
                .and_then(|plus| plus.split([',', ' ']).next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(1);
            return;
        }
        match self.state {
            DiffState::Header => {
                if let Some(rest) = raw.strip_prefix("+++ ") {
                    // Git ends a name that contains a space with a tab.
                    let rest = rest.strip_suffix('\t').unwrap_or(rest);
                    self.file = rest.strip_prefix("b/").unwrap_or(rest).to_owned();
                }
            }
            DiffState::Hunk => {
                if let Some(added) = raw.strip_prefix('+') {
                    if let Some(kind) = detect_secret_line(added) {
                        if self.hits.len() < MAX_HITS {
                            self.hits.push(SecretHit {
                                file: self.file.clone(),
                                line: self.line_no,
                                kind,
                            });
                        } else {
                            self.more += 1;
                        }
                    }
                    self.line_no = self.line_no.saturating_add(1);
                } else if raw.starts_with(' ') || raw.is_empty() {
                    // Empty: context under diff.suppressBlankEmpty.
                    self.line_no = self.line_no.saturating_add(1);
                }
            }
        }
    }
    pub fn hits(&self) -> &[SecretHit] {
        &self.hits
    }
    /// Fails with every hit (the first `MAX_HITS` listed); `refusing` ends
    /// the sentence, e.g. "refusing to ship".
    pub fn ensure_clean(&self, found_in: &str, refusing: &str) -> Result<()> {
        if self.hits.is_empty() {
            return Ok(());
        }
        let mut list = self
            .hits
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        if self.more > 0 {
            list.push_str(&format!("; and {} more", self.more));
        }
        bail!("Secret scanner found potential secret(s){found_in}; {refusing}: {list}")
    }
}

/// Scans added lines of a unified Git diff for high-confidence secret patterns.
/// Never logs the matched secret value itself.
pub fn scan_diff_secrets(diff: &str) -> Vec<SecretHit> {
    let mut scan = SecretScan::default();
    for raw in diff.lines() {
        scan.line(raw);
    }
    scan.hits
}

fn detect_secret_line(line: &str) -> Option<&'static str> {
    // PEM private key blocks.
    if line.contains("-----BEGIN ") && line.contains("PRIVATE KEY-----") {
        return Some("private key block");
    }
    // High-signal prefixed API tokens.
    let prefixed: &[(&str, usize, &'static str)] = &[
        ("sk-ant-api03-", 20, "Anthropic API key"),
        ("sk-proj-", 20, "OpenAI project API key"),
        ("sk-svcacct-", 20, "OpenAI service account key"),
        ("ghp_", 20, "GitHub personal access token"),
        ("gho_", 20, "GitHub OAuth token"),
        ("ghu_", 20, "GitHub user-to-server token"),
        ("ghs_", 20, "GitHub server-to-server token"),
        ("ghr_", 20, "GitHub refresh token"),
        ("github_pat_", 20, "GitHub fine-grained token"),
        ("xoxb-", 20, "Slack bot token"),
        ("xoxp-", 20, "Slack user token"),
        ("xoxa-", 20, "Slack app token"),
        ("xoxr-", 20, "Slack refresh token"),
        ("sk_live_", 16, "Stripe live secret key"),
        ("rk_live_", 16, "Stripe restricted live key"),
    ];
    for &(prefix, min_tail, kind) in prefixed {
        if has_token_with_prefix(line, prefix, min_tail, |c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
        }) {
            return Some(kind);
        }
    }
    // AWS access key IDs: AKIA or ASIA followed by 16 uppercase alphanumerics.
    for prefix in ["AKIA", "ASIA"] {
        if has_exact_token(line, prefix, 16, |c| {
            c.is_ascii_uppercase() || c.is_ascii_digit()
        }) {
            return Some("AWS access key ID");
        }
    }
    // Google API keys: AIza followed by 35 URL-safe chars.
    if has_exact_token(line, "AIza", 35, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
    }) {
        return Some("Google API key");
    }
    None
}

fn is_boundary(byte: Option<u8>) -> bool {
    match byte {
        None => true,
        Some(b) => !(b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
    }
}

fn has_token_with_prefix(
    line: &str,
    prefix: &str,
    min_tail: usize,
    valid: impl Fn(char) -> bool,
) -> bool {
    let bytes = line.as_bytes();
    let mut offset = 0;
    while let Some(pos) = line[offset..].find(prefix) {
        let start = offset + pos;
        let before = start.checked_sub(1).map(|i| bytes[i]);
        if is_boundary(before) {
            let tail = &line[start + prefix.len()..];
            let count = tail.chars().take_while(|&c| valid(c)).count();
            if count >= min_tail {
                return true;
            }
        }
        offset = start + prefix.len();
    }
    false
}

fn has_exact_token(
    line: &str,
    prefix: &str,
    exact_tail: usize,
    valid: impl Fn(char) -> bool,
) -> bool {
    let bytes = line.as_bytes();
    let mut offset = 0;
    while let Some(pos) = line[offset..].find(prefix) {
        let start = offset + pos;
        let before = start.checked_sub(1).map(|i| bytes[i]);
        if is_boundary(before) {
            let tail = &line[start + prefix.len()..];
            let count = tail.chars().take_while(|&c| valid(c)).count();
            if count == exact_tail {
                let after_idx = start + prefix.len() + count;
                if is_boundary(bytes.get(after_idx).copied()) {
                    return true;
                }
            }
        }
        offset = start + prefix.len();
    }
    false
}

const SCANNER_TIMEOUT: Duration = Duration::from_secs(60);
const SCANNER_OUTPUT_LIMIT: usize = 64 * 1024;

fn read_capped_stream(mut stream: impl Read, limit: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if out.len() < limit {
                    let take = n.min(limit - out.len());
                    out.extend_from_slice(&buf[..take]);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    out
}

/// Runs a user-configured external secret scanner argv (no shell) in `dir`.
/// `{files}` expands to `files`, which are also in `MODEL_ROUTER_SHIP_FILES`
/// (one per line). `index` is a private Git index holding HEAD plus exactly
/// those files as they will be committed; the scanner gets it as
/// `GIT_INDEX_FILE`, so its `git diff --cached` (`gitleaks protect --staged`)
/// reads the ship's changes and never the user's own index. Fails closed if
/// the scanner cannot start or exits non-zero.
pub fn run_external_scanner(
    dir: &Path,
    command: &[String],
    files: &[String],
    index: &Path,
) -> Result<()> {
    run_external_scanner_within(dir, command, files, index, SCANNER_TIMEOUT)
}

fn run_external_scanner_within(
    dir: &Path,
    command: &[String],
    files: &[String],
    index: &Path,
    timeout: Duration,
) -> Result<()> {
    let Some((program, args)) = command.split_first() else {
        return Ok(());
    };
    let mut expanded = Vec::with_capacity(args.len());
    for arg in args {
        if arg == "{files}" {
            expanded.extend(files.iter().cloned());
        } else {
            expanded.push(arg.clone());
        }
    }
    let mut child = Command::new(program)
        .args(&expanded)
        .current_dir(dir)
        .env("MODEL_ROUTER_SHIP_FILES", files.join("\n"))
        .env("GIT_INDEX_FILE", index)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!("Secret scanner {program:?} could not run; refusing to ship without scanning")
        })?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_reader =
        stdout.map(|s| std::thread::spawn(move || read_capped_stream(s, SCANNER_OUTPUT_LIMIT)));
    let err_reader =
        stderr.map(|s| std::thread::spawn(move || read_capped_stream(s, SCANNER_OUTPUT_LIMIT)));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "Secret scanner `{}` timed out after {}s; refusing to ship",
                        shell_words::join(command),
                        timeout.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).with_context(|| {
                    format!("Secret scanner {program:?} failed while waiting; refusing to ship")
                });
            }
        }
    };
    let stdout = out_reader.and_then(|t| t.join().ok()).unwrap_or_default();
    let stderr = err_reader.and_then(|t| t.join().ok()).unwrap_or_default();
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        let stdout = String::from_utf8_lossy(&stdout);
        let detail = if !stderr.trim().is_empty() {
            stderr.trim()
        } else {
            stdout.trim()
        };
        let detail = crate::storage::clean_terminal(&crate::storage::truncate(detail, 600));
        bail!(
            "Secret scanner `{}` failed (exit {:?}); refusing to ship{}",
            shell_words::join(command),
            status.code(),
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_segments_wildcards_and_doublestar() {
        assert!(matches_glob(
            ".github/workflows/*",
            ".github/workflows/ci.yml"
        ));
        assert!(!matches_glob(
            ".github/workflows/*",
            ".github/workflows/sub/ci.yml"
        ));
        assert!(matches_glob(".github/**", ".github/workflows/sub/ci.yml"));
        assert!(matches_glob("**/Cargo.lock", "Cargo.lock"));
        assert!(matches_glob("**/Cargo.lock", "crates/foo/Cargo.lock"));
        assert!(matches_glob("*.env*", ".env.local"));
        assert!(matches_glob("src/?.rs", "src/a.rs"));
        assert!(!matches_glob("src/?.rs", "src/ab.rs"));
        assert!(!matches_glob("Cargo.lock", "Cargo.toml"));
    }

    /// `*` and `?` in a pattern are wildcards even where the path has the
    /// same character literally.
    #[test]
    fn glob_wildcards_match_literal_wildcard_characters_in_paths() {
        assert!(matches_glob("*.txt", "*note.txt"));
        assert!(matches_glob("*.txt", "*.txt"));
        assert!(matches_glob("a*b", "a*xb"));
        assert!(matches_glob("a*b", "a*b"));
        assert!(!matches_glob("a*b", "a*xc"));
        assert!(matches_glob("?", "*"));
        assert!(matches_glob("?", "?"));
        assert!(!matches_glob("?", "**"));
        assert!(matches_glob("*", "***"));
        // `*` still stays within one segment.
        assert!(!matches_glob("*.txt", "*dir/note.txt"));
        assert!(matches_glob("**/*.txt", "we*ird/deep/*note.txt"));
        assert!(matches_glob("we*/**", "we*ird/deep/note.txt"));
        assert!(matches_glob("**/secrets/*", "a*b/secrets/*"));
        assert!(!matches_glob("**/secrets/*", "a*b/public/*"));
    }

    #[test]
    fn secret_scanner_catches_added_secrets_and_ignores_removed_or_clean() {
        let aws = format!("{}IOSFODNN7EXAMPLE", "AKIA");
        let gh = format!("{}1234567890abcdefghij1234567890", "ghp_");
        let pem = format!("-----BEGIN RSA {}-----", "PRIVATE KEY");
        let diff = format!(
            "\
diff --git a/config.txt b/config.txt
--- a/config.txt
+++ b/config.txt
@@ -1,3 +1,5 @@
 clean context line
-old = \"{aws}\"
+aws = \"{aws}\"
+normal = \"hello world\"
+token = \"{gh}\"
+{pem}
"
        );
        let hits = scan_diff_secrets(&diff);
        assert_eq!(hits.len(), 3, "{hits:?}");
        assert_eq!(hits[0].file, "config.txt");
        assert_eq!(hits[0].line, 2);
        assert_eq!(hits[0].kind, "AWS access key ID");
        assert_eq!(hits[1].line, 4);
        assert_eq!(hits[1].kind, "GitHub personal access token");
        assert_eq!(hits[2].line, 5);
        assert_eq!(hits[2].kind, "private key block");
    }

    /// An added line whose content starts with `++ ` or `-- ` looks like a
    /// file header (`+++ `, `--- `) once it has its diff prefix.
    #[test]
    fn secret_scanner_reads_added_lines_that_look_like_file_headers() {
        let gh = format!("{}1234567890abcdefghij1234567890", "ghp_");
        let aws = format!("{}IOSFODNN7EXAMPLE", "AKIA");
        let diff = format!(
            "\
diff --git a/notes.md b/notes.md
--- a/notes.md
+++ b/notes.md
@@ -1,2 +1,4 @@
 context
+++ token {gh}
--- old line
+plain
diff --git a/with space.txt b/with space.txt
new file mode 100644
--- /dev/null
+++ b/with space.txt\t
@@ -0,0 +1,2 @@
+first
+++ b/fake.txt {aws}
"
        );
        let hits = scan_diff_secrets(&diff);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(
            (hits[0].file.as_str(), hits[0].line, hits[0].kind),
            ("notes.md", 2, "GitHub personal access token")
        );
        // The header-like line did not rename the file either.
        assert_eq!(
            (hits[1].file.as_str(), hits[1].line, hits[1].kind),
            ("with space.txt", 2, "AWS access key ID")
        );
    }

    #[test]
    fn secret_scan_lists_a_bounded_number_of_hits() {
        let gh = format!("{}1234567890abcdefghij1234567890", "ghp_");
        let mut scan = SecretScan::default();
        scan.line("diff --git a/f b/f");
        scan.line("+++ b/f");
        scan.line("@@ -0,0 +1,30 @@");
        for _ in 0..30 {
            scan.line(&format!("+{gh}"));
        }
        assert_eq!(scan.hits().len(), MAX_HITS);
        let error = scan.ensure_clean("", "refusing to ship").unwrap_err();
        assert!(format!("{error}").contains("and 10 more"), "{error}");
    }

    #[test]
    fn external_scanner_fails_closed_when_missing_or_nonzero() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("private.index");
        let index = index.as_path();
        let err = run_external_scanner(
            dir.path(),
            &["definitely-not-a-real-scanner-binary-xyz".into()],
            &["a.rs".into()],
            index,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("could not run"));

        let err = run_external_scanner(
            dir.path(),
            &[
                "python3".into(),
                "-c".into(),
                "import sys; sys.exit(2)".into(),
            ],
            &["a.rs".into()],
            index,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("refusing to ship"));

        run_external_scanner(
            dir.path(),
            &[
                "python3".into(),
                "-c".into(),
                "import os, sys; assert 'a.rs' in os.environ['MODEL_ROUTER_SHIP_FILES']; assert os.environ['GIT_INDEX_FILE'].endswith('private.index')".into(),
            ],
            &["a.rs".into()],
            index,
        )
        .unwrap();
    }

    #[test]
    fn repeated_doublestar_globs_do_not_backtrack_exponentially() {
        let started = Instant::now();
        assert!(!matches_glob(
            "**/**/**/**/**/**/**/**/**/**/**/**/nope",
            "a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s/t",
        ));
        assert!(matches_glob(
            "**/**/**/target/**/**/file.rs",
            "x/y/target/z/file.rs",
        ));
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn external_scanner_times_out_and_caps_output() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("private.index");
        let err = run_external_scanner_within(
            dir.path(),
            &[
                "python3".into(),
                "-c".into(),
                "import time; time.sleep(5)".into(),
            ],
            &["a.rs".into()],
            &index,
            Duration::from_millis(120),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");

        let err = run_external_scanner_within(
            dir.path(),
            &[
                "python3".into(),
                "-c".into(),
                "import sys; sys.stdout.write('x' * 300000); sys.stderr.write('e' * 300000); sys.exit(1)".into(),
            ],
            &["a.rs".into()],
            &index,
            Duration::from_secs(5),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("refusing to ship") && msg.len() < 1500,
            "{msg}"
        );
    }

    #[test]
    fn line_pattern_modes_and_bounded_glob_matching() {
        // Default literal substring is safe for code with * and ?.
        assert!(matches_line_pattern("unwrap()", "let x = foo().unwrap();"));
        assert!(matches_line_pattern("foo()?", "let x = foo()?;"));
        assert!(!matches_line_pattern("foo()?", "let x = foo()!;"));
        // Case-insensitive substring.
        assert!(matches_line_pattern("i:fixme", "  // FIXME: remove"));
        assert!(!matches_line_pattern("i:fixme", "  // fixed"));
        // Identifier-bounded word match.
        assert!(matches_line_pattern("word:unsafe", "pub unsafe fn f() {}"));
        assert!(!matches_line_pattern(
            "word:unsafe",
            "let is_unsafe = true;"
        ));
        // Alternation.
        assert!(matches_line_pattern(
            "any:dbg!(|todo!(",
            "    todo!(\"x\");"
        ));
        assert!(!matches_line_pattern(
            "any:dbg!(|todo!(",
            "    println!(\"x\");"
        ));
        // Whole-line glob match without catastrophic backtracking.
        assert!(matches_line_pattern(
            "glob:*eval(*)*",
            "const r = eval(code);"
        ));
        assert!(!matches_line_pattern(
            "glob:*eval(*)*",
            "const r = evaluate(code);"
        ));
        let started = Instant::now();
        let evil_pat = format!("glob:{}x", "*a".repeat(25));
        let evil_line = "a".repeat(200);
        assert!(!matches_line_pattern(&evil_pat, &evil_line));
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn diff_policy_scan_checks_added_lines_and_path_globs() {
        let policies = vec![
            DiffPolicy {
                name: "no-unsafe".into(),
                action: PolicyAction::Block,
                pattern: "word:unsafe".into(),
                paths: vec!["src/**/*.rs".into()],
                message: "Avoid unsafe blocks in src/".into(),
            },
            DiffPolicy {
                name: "review-sql".into(),
                action: PolicyAction::Advise,
                pattern: String::new(),
                paths: vec!["migrations/**".into()],
                message: "Verify migration is backwards compatible".into(),
            },
        ];
        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,4 @@
 fn ok() {}
-unsafe { old(); }
+let is_unsafe = false;
+unsafe { new(); }
diff --git a/tests/helper.rs b/tests/helper.rs
--- a/tests/helper.rs
+++ b/tests/helper.rs
@@ -1,1 +1,2 @@
+unsafe { allowed_in_tests(); }
";
        let hits = scan_diff_policies(&policies, diff);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].policy, "no-unsafe");
        assert_eq!(hits[0].file, "src/lib.rs");
        assert_eq!(hits[0].line, 3);

        let advisories = advisory_matches(
            &policies,
            &["src/lib.rs".into(), "migrations/001_init.sql".into()],
            &[],
        );
        assert_eq!(advisories.len(), 1);
        assert_eq!(advisories[0].name, "review-sql");
        assert_eq!(advisories[0].files, vec!["migrations/001_init.sql"]);
    }

    #[test]
    fn scan_patch_file_fails_closed_when_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let policies = vec![DiffPolicy {
            name: "no-dbg".into(),
            action: PolicyAction::Block,
            pattern: "dbg!(".into(),
            paths: vec![],
            message: "Remove dbg!".into(),
        }];
        let err = scan_patch_file_policies(
            &policies,
            &dir.path().join("missing.patch"),
            " in candidate",
            "refusing to apply",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("refusing to apply"), "{err:#}");
    }
}
