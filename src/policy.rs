//! Workflow path policies (`protected_paths`), declarative diff policies
//! (`policies`), and pre-ship secret scanning.
//! No shell evaluates any command; external scanners run as argument vectors.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
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

/// Matches an added diff line against a declarative policy `pattern`.
///
/// Reads the whole line: a match anywhere in it counts. No regex engine, so
/// no catastrophic backtracking: literal modes are linear in the line, and
/// `word:` and `glob:` are at most linear in the line times the pattern's
/// length, which comes from the workflow, not from the diff.
pub fn matches_line_pattern(pattern: &str, line: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
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

/// A path from a diff header as the repository names it. Git C-quotes a path
/// with a `"`, `\`, or control character (and, unless `core.quotePath` is
/// false, any non-ASCII byte); this undoes that quoting so `paths` globs see
/// the real path. An unquoted path is returned as is.
fn unquote_diff_path(raw: &str) -> String {
    let Some(inner) = raw
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return raw.to_owned();
    };
    let mut out = Vec::with_capacity(inner.len());
    let mut bytes = inner.bytes().peekable();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            out.push(byte);
            continue;
        }
        let Some(escaped) = bytes.next() else {
            out.push(b'\\');
            break;
        };
        match escaped {
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            b'0'..=b'7' => {
                let mut value = u32::from(escaped - b'0');
                for _ in 0..2 {
                    let Some(digit) = bytes.next_if(|d| matches!(d, b'0'..=b'7')) else {
                        break;
                    };
                    value = value * 8 + u32::from(digit - b'0');
                }
                out.push((value & 0xff) as u8);
            }
            other => out.push(other),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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
                    let path = unquote_diff_path(rest);
                    self.file = path.strip_prefix("b/").unwrap_or(&path).to_owned();
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

/// Scans the text diff between Git trees `from` and `to` in repository `dir`
/// against blocking `policies`, failing closed if the diff cannot be read.
/// Reads it as `snapshot::patch_lines` does for stage, apply, and ship scans:
/// files Git calls binary as text, and non-ASCII paths unquoted.
pub fn scan_tree_policies(
    policies: &[DiffPolicy],
    dir: &Path,
    (from, to): (&str, &str),
    found_in: &str,
    refusing: &str,
) -> Result<()> {
    if !has_block_policies(policies) {
        return Ok(());
    }
    let mut scan = PolicyScan::new(policies);
    crate::snapshot::patch_lines(dir, from, to, |line| scan.line(line))
        .with_context(|| format!("Read the diff to verify workflow policies; {refusing}"))?;
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

#[cfg(not(unix))]
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

#[cfg(unix)]
fn set_nonblocking(fd: libc::c_int) {
    // SAFETY: `fd` is an open pipe descriptor owned by the caller.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            let _ = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

/// Drains available bytes from a non-blocking pipe into `out` (capped at
/// `limit`), discarding excess bytes. Returns `true` when the stream hit EOF or
/// an unrecoverable error and should be closed.
#[cfg(unix)]
fn drain_nonblocking(stream: &mut impl Read, out: &mut Vec<u8>, limit: usize) -> bool {
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return true,
            Ok(n) => {
                if out.len() < limit {
                    let take = n.min(limit - out.len());
                    out.extend_from_slice(&buf[..take]);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return false,
            Err(_) => return true,
        }
    }
}

/// Waits for `child` and drains its `stdout`/`stderr` pipes on the calling
/// thread via `poll(2)` until `deadline`, without spawning reader threads.
/// Both pipes are dropped (closed) before returning on timeout or error.
#[cfg(unix)]
fn collect_scanner_unix(
    child: &mut std::process::Child,
    command: &[String],
    program: &str,
    pgid: u32,
    started: &Option<String>,
    timeout: Duration,
    deadline: Instant,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    use std::os::unix::io::AsRawFd;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    if let Some(s) = stdout_pipe.as_ref() {
        set_nonblocking(s.as_raw_fd());
    }
    if let Some(s) = stderr_pipe.as_ref() {
        set_nonblocking(s.as_raw_fd());
    }

    let stop_scanner = |child: &mut std::process::Child| {
        crate::process::end_group_if(
            pgid,
            Duration::from_millis(200),
            || {
                let _ = child.try_wait();
            },
            || !crate::process::group_reused(started, pgid),
        );
        let _ = child.kill();
        let _ = child.wait();
    };

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut status: Option<std::process::ExitStatus> = None;

    loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(st)) => {
                    status = Some(st);
                    // End any background children the scanner left behind
                    // before waiting on pipes, checking the leader's start
                    // time so a reused PGID is never hit.
                    crate::process::end_group_if(
                        pgid,
                        Duration::from_millis(200),
                        || {
                            let _ = child.try_wait();
                        },
                        || !crate::process::group_reused(started, pgid),
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    stop_scanner(child);
                    drop(stdout_pipe);
                    drop(stderr_pipe);
                    return Err(error).with_context(|| {
                        format!("Secret scanner {program:?} failed while waiting; refusing to ship")
                    });
                }
            }
        }

        if let Some(s) = stdout_pipe.as_mut() {
            if drain_nonblocking(s, &mut stdout, SCANNER_OUTPUT_LIMIT) {
                stdout_pipe = None;
            }
        }
        if let Some(s) = stderr_pipe.as_mut() {
            if drain_nonblocking(s, &mut stderr, SCANNER_OUTPUT_LIMIT) {
                stderr_pipe = None;
            }
        }

        if let Some(st) = status {
            if stdout_pipe.is_none() && stderr_pipe.is_none() {
                return Ok((st, stdout, stderr));
            }
        }

        let now = Instant::now();
        if now >= deadline {
            drop(stdout_pipe);
            drop(stderr_pipe);
            if status.is_none() {
                stop_scanner(child);
                bail!(
                    "Secret scanner `{}` timed out after {}s; refusing to ship",
                    shell_words::join(command),
                    timeout.as_secs()
                );
            }
            bail!(
                "Secret scanner `{}` timed out after {}s while collecting output; refusing to ship",
                shell_words::join(command),
                timeout.as_secs()
            );
        }

        let remaining = deadline.saturating_duration_since(now);
        let wait = if status.is_some() {
            remaining
        } else {
            remaining.min(Duration::from_millis(20))
        };
        let mut fds = [
            libc::pollfd {
                fd: stdout_pipe.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stderr_pipe.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if fds[0].fd >= 0 || fds[1].fd >= 0 {
            let timeout_ms = wait.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
            // SAFETY: `fds` points to 2 valid `pollfd` entries.
            let _ = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout_ms) };
        } else {
            std::thread::sleep(wait);
        }
    }
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
    let mut cmd = Command::new(program);
    // Any Git process the scanner invokes gets the router's hook and fsmonitor
    // overrides, like `ship` gives `gh`.
    const OVERRIDES: &str = "'core.hooksPath=/dev/null' 'core.fsmonitor=false'";
    let mut parameters = std::env::var_os("GIT_CONFIG_PARAMETERS").unwrap_or_default();
    if !parameters.is_empty() {
        parameters.push(" ");
    }
    parameters.push(OVERRIDES);
    cmd.args(&expanded)
        .current_dir(dir)
        .env("MODEL_ROUTER_SHIP_FILES", files.join("\n"))
        .env("GIT_INDEX_FILE", index)
        .env("GIT_CONFIG_PARAMETERS", parameters)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().with_context(|| {
        format!("Secret scanner {program:?} could not run; refusing to ship without scanning")
    })?;
    let deadline = Instant::now() + timeout;
    #[cfg(unix)]
    let (status, stdout, stderr) = {
        let pgid = child.id();
        let started = crate::process::group::started(pgid);
        collect_scanner_unix(
            &mut child, command, program, pgid, &started, timeout, deadline,
        )?
    };
    #[cfg(not(unix))]
    let (status, stdout, stderr) = {
        let out_rx = child.stdout.take().map(|s| {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let _ = tx.send(read_capped_stream(s, SCANNER_OUTPUT_LIMIT));
            });
            rx
        });
        let err_rx = child.stderr.take().map(|s| {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let _ = tx.send(read_capped_stream(s, SCANNER_OUTPUT_LIMIT));
            });
            rx
        });
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
        let recv_bounded = |rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>| -> Result<Vec<u8>> {
            let Some(rx) = rx else {
                return Ok(Vec::new());
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(bytes) => Ok(bytes),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => bail!(
                    "Secret scanner `{}` timed out after {}s while collecting output; refusing to ship",
                    shell_words::join(command),
                    timeout.as_secs()
                ),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Ok(Vec::new()),
            }
        };
        (status, recv_bounded(out_rx)?, recv_bounded(err_rx)?)
    };
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
    fn line_patterns_match_past_16_kib_of_a_long_line() {
        let filler = "x".repeat(64 * 1024);
        let line = format!("{filler} BANNED_CALL(é);");
        for pattern in [
            "BANNED_CALL(",
            "i:banned_call(",
            "word:BANNED_CALL",
            "any:nope|BANNED_CALL(",
            "glob:*BANNED_CALL(*",
        ] {
            assert!(matches_line_pattern(pattern, &line), "{pattern}");
        }
        assert!(!matches_line_pattern("word:BANNED", &line));
        // A long hostile line stays fast with a hostile glob.
        let started = Instant::now();
        let evil_pat = format!("glob:{}x", "*a".repeat(25));
        assert!(!matches_line_pattern(&evil_pat, &"a".repeat(256 * 1024)));
        assert!(started.elapsed() < Duration::from_secs(5));

        let policies = vec![DiffPolicy {
            name: "no-banned".into(),
            action: PolicyAction::Block,
            pattern: "BANNED_CALL(".into(),
            paths: vec![],
            message: "No banned calls".into(),
        }];
        let diff = format!("+++ b/min.js\n@@ -0,0 +1 @@\n+{line}\n");
        assert_eq!(scan_diff_policies(&policies, &diff).len(), 1);
    }

    #[test]
    fn diff_policy_scan_decodes_quoted_paths() {
        assert_eq!(unquote_diff_path("b/src/lib.rs"), "b/src/lib.rs");
        assert_eq!(
            unquote_diff_path(r#""b/src/caf\303\251.rs""#),
            "b/src/café.rs"
        );
        assert_eq!(
            unquote_diff_path(r#""b/a \"q\" \\ \t\n\0018.rs""#),
            "b/a \"q\" \\ \t\n\u{1}8.rs"
        );
        let policies = vec![DiffPolicy {
            name: "no-banned".into(),
            action: PolicyAction::Block,
            pattern: "BANNED".into(),
            paths: vec!["src/**".into()],
            message: "No banned calls".into(),
        }];
        let diff = "\
diff --git \"a/src/caf\\303\\251.rs\" \"b/src/caf\\303\\251.rs\"
--- /dev/null
+++ \"b/src/caf\\303\\251.rs\"
@@ -0,0 +1 @@
+BANNED();
";
        let hits = scan_diff_policies(&policies, diff);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].file, "src/café.rs");
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
    fn scan_tree_fails_closed_when_the_diff_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let policies = vec![DiffPolicy {
            name: "no-dbg".into(),
            action: PolicyAction::Block,
            pattern: "dbg!(".into(),
            paths: vec![],
            message: "Remove dbg!".into(),
        }];
        let err = scan_tree_policies(
            &policies,
            dir.path(),
            ("missing-from", "missing-to"),
            " in candidate",
            "refusing to apply",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("refusing to apply"), "{err:#}");
    }

    #[cfg(unix)]
    #[test]
    fn external_scanner_sets_git_overrides_and_reaps_process_group_on_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("index");
        // Verify GIT_CONFIG_PARAMETERS overrides reach the scanner process.
        let check_env = vec![
            "python3".into(),
            "-c".into(),
            "import os, sys; p = os.environ.get('GIT_CONFIG_PARAMETERS', ''); assert 'core.hooksPath=/dev/null' in p and 'core.fsmonitor=false' in p, p".into(),
        ];
        run_external_scanner_within(dir.path(), &check_env, &[], &index, Duration::from_secs(5))
            .unwrap();

        // Verify a timed-out scanner's child process is reaped with its group.
        let pid_file = dir.path().join("child.pid");
        let spawn_child = vec![
            "python3".into(),
            "-c".into(),
            format!(
                "import pathlib, subprocess, sys, time; p = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)']); pathlib.Path({:?}).write_text(str(p.pid)); time.sleep(60)",
                pid_file.display().to_string()
            ),
        ];
        let err = run_external_scanner_within(
            dir.path(),
            &spawn_child,
            &[],
            &index,
            Duration::from_millis(1200),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        let text = std::fs::read_to_string(&pid_file)
            .expect("scanner did not record child pid before timeout");
        let pid: libc::pid_t = text.trim().parse().expect("valid child pid");
        let is_running = |pid: libc::pid_t| -> bool {
            // SAFETY: signal 0 only checks whether the process exists.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return false;
            }
            #[cfg(target_os = "linux")]
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                if let Some(after_comm) = stat.rsplit_once(')').map(|(_, rest)| rest.trim_start()) {
                    if after_comm.starts_with('Z') {
                        return false;
                    }
                }
            }
            #[cfg(not(target_os = "linux"))]
            if let Ok(out) = Command::new("ps")
                .args(["-o", "state=", "-p", &pid.to_string()])
                .output()
            {
                let state = String::from_utf8_lossy(&out.stdout);
                let trimmed = state.trim();
                if !out.status.success() || trimmed.is_empty() || trimmed.starts_with('Z') {
                    return false;
                }
            }
            true
        };
        let wait_deadline = Instant::now() + Duration::from_secs(2);
        while is_running(pid) && Instant::now() < wait_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!is_running(pid), "scanner child {pid} survived timeout");

        // Verify a detached helper (new session) holding stdout open after the
        // scanner exits still fails closed at the scanner deadline.
        let detached_pid_file = dir.path().join("detached.pid");
        let detached_holder = vec![
            "python3".into(),
            "-c".into(),
            format!(
                "import pathlib, subprocess, sys; p = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'], start_new_session=True); pathlib.Path({:?}).write_text(str(p.pid))",
                detached_pid_file.display().to_string()
            ),
        ];
        let started = Instant::now();
        let err = run_external_scanner_within(
            dir.path(),
            &detached_holder,
            &[],
            &index,
            Duration::from_millis(500),
        )
        .unwrap_err();
        // Clean up the detached test helper immediately.
        if let Ok(text) = std::fs::read_to_string(&detached_pid_file) {
            if let Ok(dpid) = text.trim().parse::<libc::pid_t>() {
                // SAFETY: terminating our own test helper.
                unsafe {
                    libc::kill(dpid, libc::SIGKILL);
                }
            }
        }
        assert!(format!("{err:#}").contains("collecting output"), "{err:#}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "scanner hung past timeout: {:?}",
            started.elapsed()
        );
    }
}
