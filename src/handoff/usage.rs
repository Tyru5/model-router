use crate::adapters::OutputKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Tokens and cost a harness reported, never estimated: a field is None when
/// the harness did not report it. Input excludes tokens read from cache and
/// output includes reasoning, whatever each harness's own convention.
/// Codex, Cursor, and Gemini report no cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}
impl std::ops::Add for Usage {
    type Output = Self;
    /// Sums what either side reported.
    fn add(self, other: Self) -> Self {
        fn plus<T: std::ops::Add<Output = T>>(a: Option<T>, b: Option<T>) -> Option<T> {
            match (a, b) {
                (Some(a), Some(b)) => Some(a + b),
                (a, b) => a.or(b),
            }
        }
        Self {
            input_tokens: plus(self.input_tokens, other.input_tokens),
            output_tokens: plus(self.output_tokens, other.output_tokens),
            cache_read_tokens: plus(self.cache_read_tokens, other.cache_read_tokens),
            cache_write_tokens: plus(self.cache_write_tokens, other.cache_write_tokens),
            cost_usd: plus(self.cost_usd, other.cost_usd),
        }
    }
}

/// Usage from unattended harness output; None when it reports none.
pub fn usage(raw: &str, kind: OutputKind) -> Option<Usage> {
    json_values(raw).fold(None, |found, event| add_usage(found, &event, kind))
}

fn add_usage(found: Option<Usage>, event: &Value, kind: OutputKind) -> Option<Usage> {
    let Some(part) = event_usage(event, kind) else {
        return found;
    };
    Some(match found {
        Some(sum) if summed(kind) => sum + part,
        _ => part,
    })
}

/// Largest single JSON document (Gemini's output) read whole for usage.
const DOCUMENT_LIMIT: u64 = 16 << 20;

/// Usage from an unattended harness's stdout.log, read one line at a time
/// so a long log is never held in memory. Gemini prints one JSON document,
/// read whole up to `DOCUMENT_LIMIT`; a log with no JSON lines is read the
/// same way, in case it is one pretty-printed document.
pub fn usage_file(path: &std::path::Path, kind: OutputKind) -> Option<Usage> {
    use std::io::{BufRead, Read};
    let document = |path: &std::path::Path| {
        let mut raw = String::new();
        let file = std::fs::File::open(path).ok()?;
        if file.metadata().ok()?.len() > DOCUMENT_LIMIT {
            return None;
        }
        file.take(DOCUMENT_LIMIT)
            .read_to_string(&mut raw)
            .ok()
            .and_then(|_| usage(&raw, kind))
    };
    if kind == OutputKind::Gemini {
        return document(path);
    }
    let mut reader = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let mut line = Vec::new();
    let (mut found, mut any) = (None, false);
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line);
        for event in line_values(&text) {
            any = true;
            found = add_usage(found, &event, kind);
        }
    }
    if any { found } else { document(path) }
}

/// The JSON value on one line of output, after any non-JSON prefix; arrays
/// are flattened.
fn line_values(line: &str) -> Vec<Value> {
    let line = line.trim();
    let value = serde_json::from_str::<Value>(line).ok().or_else(|| {
        let start = line.find('{').filter(|start| *start > 0)?;
        serde_json::from_str(&line[start..]).ok()
    });
    match value {
        Some(Value::Array(items)) => items,
        Some(value) if value.is_object() => vec![value],
        _ => vec![],
    }
}

/// Whether a harness reports usage per turn or step, rather than once for
/// the whole session.
fn summed(kind: OutputKind) -> bool {
    matches!(
        kind,
        OutputKind::Codex | OutputKind::Pi | OutputKind::OpenCode
    )
}

/// JSON values in output: one document, or JSON lines among other text.
/// Arrays, such as Claude's verbose JSON, are flattened.
fn json_values(raw: &str) -> impl Iterator<Item = Value> + '_ {
    let text = raw.trim();
    // Parse from the first brace only when text precedes it; otherwise the
    // second attempt would repeat the first.
    let document = serde_json::from_str::<Value>(text).ok().or_else(|| {
        let start = text.find('{').filter(|start| *start > 0)?;
        serde_json::from_str(&text[start..]).ok()
    });
    let lines = document.is_none().then(|| {
        text.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line.get(line.find('{')?..)?).ok())
    });
    document
        .into_iter()
        .chain(lines.into_iter().flatten())
        .flat_map(|value| match value {
            Value::Array(items) => items,
            value => vec![value],
        })
}

fn event_usage(event: &Value, kind: OutputKind) -> Option<Usage> {
    let count = |pointer: &str| event.pointer(pointer).and_then(Value::as_u64);
    let cost = |pointer: &str| event.pointer(pointer).and_then(Value::as_f64);
    let plus = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        (a, b) => a.or(b),
    };
    let kind_name = event.get("type").and_then(Value::as_str).unwrap_or("");
    let usage = match kind {
        // Claude Code and Amp stream JSON: the final result event.
        OutputKind::Claude | OutputKind::Amp if kind_name == "result" => Usage {
            input_tokens: count("/usage/input_tokens"),
            output_tokens: count("/usage/output_tokens"),
            cache_read_tokens: count("/usage/cache_read_input_tokens"),
            cache_write_tokens: count("/usage/cache_creation_input_tokens"),
            cost_usd: cost("/total_cost_usd"),
        },
        // cursor-agent's result event; its inputTokens already exclude cache.
        OutputKind::Cursor if kind_name == "result" => Usage {
            input_tokens: count("/usage/inputTokens"),
            output_tokens: count("/usage/outputTokens"),
            cache_read_tokens: count("/usage/cacheReadTokens"),
            cache_write_tokens: count("/usage/cacheWriteTokens"),
            cost_usd: None,
        },
        // codex exec --json; input_tokens includes cached_input_tokens.
        OutputKind::Codex if kind_name == "turn.completed" => {
            let cached = count("/usage/cached_input_tokens");
            Usage {
                input_tokens: count("/usage/input_tokens")
                    .map(|n| n.saturating_sub(cached.unwrap_or(0))),
                output_tokens: count("/usage/output_tokens"),
                cache_read_tokens: cached,
                cache_write_tokens: count("/usage/cache_write_input_tokens"),
                cost_usd: None,
            }
        }
        // pi --mode json: each assistant message_end carries pi-ai's Usage.
        OutputKind::Pi
            if kind_name == "message_end"
                && event.pointer("/message/role").and_then(Value::as_str) == Some("assistant") =>
        {
            Usage {
                input_tokens: count("/message/usage/input"),
                output_tokens: count("/message/usage/output"),
                cache_read_tokens: count("/message/usage/cacheRead"),
                cache_write_tokens: count("/message/usage/cacheWrite"),
                cost_usd: cost("/message/usage/cost/total"),
            }
        }
        // opencode run --format json: per-step totals; output excludes reasoning.
        OutputKind::OpenCode if kind_name == "step_finish" => Usage {
            input_tokens: count("/part/tokens/input"),
            output_tokens: plus(
                count("/part/tokens/output"),
                count("/part/tokens/reasoning"),
            ),
            cache_read_tokens: count("/part/tokens/cache/read"),
            cache_write_tokens: count("/part/tokens/cache/write"),
            cost_usd: cost("/part/cost"),
        },
        // gemini --output-format json: per-model stats; prompt includes cached,
        // candidates exclude thoughts.
        OutputKind::Gemini => {
            let models = event.pointer("/stats/models")?.as_object()?;
            let mut sum = Usage::default();
            for model in models.values() {
                let count = |key: &str| {
                    model
                        .pointer(&format!("/tokens/{key}"))
                        .and_then(Value::as_u64)
                };
                let cached = count("cached");
                sum = sum
                    + Usage {
                        input_tokens: count("prompt")
                            .map(|n| n.saturating_sub(cached.unwrap_or(0))),
                        output_tokens: plus(count("candidates"), count("thoughts")),
                        cache_read_tokens: cached,
                        ..Usage::default()
                    };
            }
            sum
        }
        _ => return None,
    };
    (usage != Usage::default()).then_some(usage)
}

/// Where harnesses keep native session logs: `CLAUDE_CONFIG_DIR` or
/// `~/.claude`, and `CODEX_HOME` or `~/.codex`.
#[derive(Debug, Clone, Default)]
pub struct SessionRoots {
    pub claude: Option<std::path::PathBuf>,
    pub codex: Option<std::path::PathBuf>,
}
impl SessionRoots {
    pub fn from_env() -> Self {
        let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
        let dir = |var: &str, default: &str| {
            std::env::var_os(var)
                .filter(|v| !v.is_empty())
                .map(std::path::PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(default)))
        };
        Self {
            claude: dir("CLAUDE_CONFIG_DIR", ".claude"),
            codex: dir("CODEX_HOME", ".codex"),
        }
    }
}

/// Usage an interactive stage spent, read best effort from the harness's
/// own session log between `since` and `until`: Claude Code's
/// `projects/*/<session>.jsonl` (assistant `message.usage`, once per message
/// ID) and Codex's `sessions/**/rollout-*.jsonl` (`token_count` totals,
/// matched by session ID or else by workspace). None for other harnesses
/// or when no log is found. Only tokens: these logs carry no cost.
pub fn session_usage(
    harness: &str,
    session: Option<&str>,
    workspace: &std::path::Path,
    since: chrono::DateTime<chrono::Utc>,
    until: chrono::DateTime<chrono::Utc>,
) -> Option<Usage> {
    session_usage_in(
        &SessionRoots::from_env(),
        harness,
        session,
        workspace,
        since,
        until,
    )
}

pub fn session_usage_in(
    roots: &SessionRoots,
    harness: &str,
    session: Option<&str>,
    workspace: &std::path::Path,
    since: chrono::DateTime<chrono::Utc>,
    until: chrono::DateTime<chrono::Utc>,
) -> Option<Usage> {
    // Log timestamps and the router's clock may disagree slightly.
    let slack = chrono::Duration::seconds(5);
    let (since, until) = (since - slack, until + slack);
    match harness {
        "claude" => claude_session_usage(roots.claude.as_deref()?, session?, since, until),
        "codex" => codex_session_usage(roots.codex.as_deref()?, session, workspace, since, until),
        _ => None,
    }
}

/// Parsed JSON lines of a log file, skipping lines that do not parse.
fn log_lines(path: &std::path::Path) -> impl Iterator<Item = Value> {
    use std::io::BufRead;
    std::fs::File::open(path)
        .ok()
        .map(std::io::BufReader::new)
        .into_iter()
        .flat_map(|reader| reader.lines().map_while(Result::ok))
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
}

fn timestamp(event: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    let text = event.get("timestamp")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

fn claude_session_usage(
    root: &std::path::Path,
    session: &str,
    since: chrono::DateTime<chrono::Utc>,
    until: chrono::DateTime<chrono::Utc>,
) -> Option<Usage> {
    // Session IDs are UUIDs; anything else cannot name a log file.
    if session.is_empty() || !session.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return None;
    }
    let name = format!("{session}.jsonl");
    let mut files = vec![];
    for project in std::fs::read_dir(root.join("projects")).ok()?.flatten() {
        let path = project.path();
        if path.join(&name).is_file() {
            files.push(path.join(&name));
        }
        // Subagent transcripts of the session, in newer Claude Code versions.
        if let Ok(entries) = std::fs::read_dir(path.join(session).join("subagents")) {
            files.extend(
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "jsonl")),
            );
        }
    }
    // Streamed messages repeat their usage; the last copy of an ID counts.
    let mut messages: std::collections::BTreeMap<String, Usage> = Default::default();
    for (index, event) in files.iter().flat_map(|f| log_lines(f)).enumerate() {
        if event.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if !timestamp(&event).is_some_and(|t| t >= since && t <= until) {
            continue;
        }
        let Some(usage) = event.pointer("/message/usage") else {
            continue;
        };
        let count = |key: &str| usage.get(key).and_then(Value::as_u64);
        let id = event
            .pointer("/message/id")
            .and_then(Value::as_str)
            .map_or_else(|| format!("line-{index}"), str::to_owned);
        messages.insert(
            id,
            Usage {
                input_tokens: count("input_tokens"),
                output_tokens: count("output_tokens"),
                cache_read_tokens: count("cache_read_input_tokens"),
                cache_write_tokens: count("cache_creation_input_tokens"),
                cost_usd: None,
            },
        );
    }
    let sum = messages.into_values().reduce(|a, b| a + b)?;
    (sum != Usage::default()).then_some(sum)
}

fn codex_session_usage(
    root: &std::path::Path,
    session: Option<&str>,
    workspace: &std::path::Path,
    since: chrono::DateTime<chrono::Utc>,
    until: chrono::DateTime<chrono::Utc>,
) -> Option<Usage> {
    let canonical =
        |path: &std::path::Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let workspace = canonical(workspace);
    let mut files = vec![];
    let mut dirs = vec![root.join("sessions")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                dirs.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
                // Logs not written since the attempt started cannot hold it.
                && meta
                    .modified()
                    .is_ok_and(|m| chrono::DateTime::<chrono::Utc>::from(m) >= since)
            {
                files.push(path);
            }
        }
    }
    let mut found: Option<Usage> = None;
    for path in files {
        let mut events = log_lines(&path);
        let Some(meta) = events
            .by_ref()
            .find(|e| e.get("type").and_then(Value::as_str) == Some("session_meta"))
        else {
            continue;
        };
        let matches = match session {
            Some(id) => meta.pointer("/payload/id").and_then(Value::as_str) == Some(id),
            None => meta
                .pointer("/payload/cwd")
                .and_then(Value::as_str)
                .is_some_and(|cwd| canonical(std::path::Path::new(cwd)) == workspace),
        };
        if !matches {
            continue;
        }
        // Totals are cumulative over the session: the attempt spent the
        // last total in its window minus the last one before it.
        let (mut before, mut end) = (None, None);
        for event in events {
            if event.pointer("/payload/type").and_then(Value::as_str) != Some("token_count") {
                continue;
            }
            let (Some(at), Some(total)) = (
                timestamp(&event),
                event.pointer("/payload/info/total_token_usage"),
            ) else {
                continue;
            };
            let count = |key: &str| total.get(key).and_then(Value::as_u64).unwrap_or(0);
            let cached = count("cached_input_tokens");
            let totals = [
                count("input_tokens").saturating_sub(cached),
                count("output_tokens"),
                cached,
            ];
            if at < since {
                before = Some(totals);
            } else if at <= until {
                end = Some(totals);
            }
        }
        let Some(end) = end else { continue };
        let before = before.unwrap_or_default();
        let spent = |i: usize| Some(end[i].saturating_sub(before[i]));
        let part = Usage {
            input_tokens: spent(0),
            output_tokens: spent(1),
            cache_read_tokens: spent(2),
            cache_write_tokens: None,
            cost_usd: None,
        };
        found = Some(match found {
            Some(sum) => sum + part,
            None => part,
        });
    }
    found
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    fn tokens(input: u64, output: u64, read: Option<u64>, write: Option<u64>) -> Usage {
        Usage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_tokens: read,
            cache_write_tokens: write,
            cost_usd: None,
        }
    }

    #[test]
    fn claude_and_amp_report_the_result_event() {
        // Claude Code `--output-format json` result (docs: SDK result message).
        let raw = r#"{"type":"result","subtype":"success","result":"done","total_cost_usd":0.0123,"usage":{"input_tokens":12,"output_tokens":340,"cache_read_input_tokens":20000,"cache_creation_input_tokens":1500}}"#;
        let expected = Usage {
            cost_usd: Some(0.0123),
            ..tokens(12, 340, Some(20_000), Some(1_500))
        };
        assert_eq!(usage(raw, OutputKind::Claude), Some(expected));
        // Verbose JSON is one array; Amp streams the same events as lines.
        assert_eq!(
            usage(
                &format!(r#"[{{"type":"system"}},{raw}]"#),
                OutputKind::Claude
            ),
            Some(expected)
        );
        assert_eq!(
            usage(
                &format!("{{\"type\":\"system\"}}\n{raw}\n"),
                OutputKind::Amp
            ),
            Some(expected)
        );
        assert_eq!(
            usage(r#"{"type":"result","result":"done"}"#, OutputKind::Claude),
            None
        );
    }

    #[test]
    fn cursor_reports_camel_case_usage_without_cost() {
        // cursor-agent 2026.10.01 print mode: `...oe?{usage:oe}:{}` on the result.
        let raw = r#"{"type":"result","subtype":"success","result":"ok","usage":{"inputTokens":90,"outputTokens":40,"cacheReadTokens":1000,"cacheWriteTokens":0}}"#;
        assert_eq!(
            usage(raw, OutputKind::Cursor),
            Some(tokens(90, 40, Some(1_000), Some(0)))
        );
    }

    #[test]
    fn codex_sums_turns_and_excludes_cached_input() {
        // codex exec --json (docs/exec.md): input_tokens includes cached ones.
        let raw = concat!(
            r#"{"type":"thread.started","thread_id":"t"}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":1000,"cached_input_tokens":800,"output_tokens":50}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":500,"cached_input_tokens":0,"output_tokens":10}}"#,
        );
        assert_eq!(
            usage(raw, OutputKind::Codex),
            Some(tokens(700, 60, Some(800), None))
        );
    }

    #[test]
    fn pi_sums_assistant_messages_with_cost() {
        // pi --mode json message_end; pi-ai `Usage` (dist/types.d.ts).
        let message = |input, cost| {
            format!(
                r#"{{"type":"message_end","message":{{"role":"assistant","usage":{{"input":{input},"output":20,"cacheRead":100,"cacheWrite":5,"totalTokens":0,"cost":{{"total":{cost}}}}}}}}}"#
            )
        };
        let raw = [
            r#"{"type":"message_end","message":{"role":"user","usage":{"input":999}}}"#.into(),
            message(10, 0.5),
            message(30, 0.25),
        ]
        .join("\n");
        assert_eq!(
            usage(&raw, OutputKind::Pi),
            Some(Usage {
                cost_usd: Some(0.75),
                ..tokens(40, 40, Some(200), Some(10))
            })
        );
    }

    #[test]
    fn opencode_sums_steps_and_counts_reasoning_as_output() {
        // opencode run --format json step_finish: part.tokens/cost.
        let step = r#"{"type":"step_finish","part":{"type":"step-finish","cost":0.01,"tokens":{"input":100,"output":30,"reasoning":20,"cache":{"read":400,"write":0}}}}"#;
        assert_eq!(
            usage(&format!("{step}\n{step}"), OutputKind::OpenCode),
            Some(Usage {
                cost_usd: Some(0.02),
                ..tokens(200, 100, Some(800), Some(0))
            })
        );
    }

    #[test]
    fn gemini_sums_models_without_cost() {
        // gemini --output-format json (docs/cli/headless.md): stats.models.*.tokens.
        let raw = r#"{"response":"ok","stats":{"models":{"gemini-2.5-pro":{"tokens":{"prompt":1000,"candidates":50,"total":1100,"cached":600,"thoughts":50,"tool":0}},"gemini-2.5-flash":{"tokens":{"prompt":100,"candidates":10,"cached":0}}}}}"#;
        assert_eq!(
            usage(raw, OutputKind::Gemini),
            Some(tokens(500, 110, Some(600), None))
        );
    }

    #[test]
    fn usage_file_streams_lines_and_reads_documents_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout.log");
        let turn = r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":5}}"#;
        // A partial first line and noise between events.
        std::fs::write(&path, format!("warning\n{turn}\nprefix {turn}\n{{\"type\"")).unwrap();
        assert_eq!(
            usage_file(&path, OutputKind::Codex),
            Some(tokens(20, 10, None, None))
        );
        // Claude's verbose JSON array on one line.
        let result = r#"{"type":"result","usage":{"input_tokens":1,"output_tokens":2}}"#;
        std::fs::write(&path, format!(r#"[{{"type":"system"}},{result}]"#)).unwrap();
        assert_eq!(
            usage_file(&path, OutputKind::Claude),
            Some(tokens(1, 2, None, None))
        );
        // Gemini pretty-prints one document across lines.
        let gemini = "{\n  \"stats\": {\n    \"models\": {\"m\": {\"tokens\": {\"prompt\": 7, \"candidates\": 3}}}\n  }\n}\n";
        std::fs::write(&path, gemini).unwrap();
        assert_eq!(
            usage_file(&path, OutputKind::Gemini),
            Some(tokens(7, 3, None, None))
        );
        assert_eq!(
            usage_file(&dir.path().join("missing"), OutputKind::Codex),
            None
        );
    }

    fn session_roots() -> SessionRoots {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/session-logs");
        SessionRoots {
            claude: Some(root.join("claude")),
            codex: Some(root.join("codex")),
        }
    }

    fn at(time: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(&format!("2026-11-05T{time}Z"))
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn claude_session_logs_count_each_message_once_within_the_attempt() {
        let roots = session_roots();
        let workspace = std::path::Path::new("/tmp/fixture-workspace");
        let session = Some("5f0c1d2e-0000-4000-8000-00000000c1a0");
        let spent = session_usage_in(
            &roots,
            "claude",
            session,
            workspace,
            at("10:00:00"),
            at("10:30:00"),
        );
        // msg_1's last copy, msg_2, and the subagent; the 09:00 message is
        // an earlier attempt's.
        assert_eq!(spent, Some(tokens(14, 58, Some(300), Some(20))));
        let earlier = session_usage_in(
            &roots,
            "claude",
            session,
            workspace,
            at("08:00:00"),
            at("09:30:00"),
        );
        assert_eq!(earlier, Some(tokens(999, 999, None, None)));
        // No session, an unknown one, or a path-like one: nothing.
        for session in [
            None,
            Some("00000000-0000-4000-8000-000000000000"),
            Some("../x"),
        ] {
            assert_eq!(
                session_usage_in(
                    &roots,
                    "claude",
                    session,
                    workspace,
                    at("10:00:00"),
                    at("10:30:00")
                ),
                None
            );
        }
    }

    #[test]
    fn codex_rollouts_subtract_totals_from_before_the_attempt() {
        let roots = session_roots();
        let workspace = std::path::Path::new("/tmp/fixture-workspace");
        // Found by workspace when the router assigned no session; the other
        // project's rollout in the same window is ignored.
        let spent = session_usage_in(
            &roots,
            "codex",
            None,
            workspace,
            at("10:00:00"),
            at("10:30:00"),
        );
        assert_eq!(spent, Some(tokens(800, 300, Some(1_200), None)));
        // A takeover names its session.
        let other = session_usage_in(
            &roots,
            "codex",
            Some("0c0de000-0000-4000-8000-000000000002"),
            workspace,
            at("10:00:00"),
            at("10:30:00"),
        );
        assert_eq!(other, Some(tokens(50, 5, Some(0), None)));
        assert_eq!(
            session_usage_in(
                &roots,
                "codex",
                None,
                std::path::Path::new("/nowhere"),
                at("10:00:00"),
                at("10:30:00")
            ),
            None
        );
        assert_eq!(
            session_usage_in(
                &roots,
                "gemini",
                None,
                workspace,
                at("10:00:00"),
                at("10:30:00")
            ),
            None
        );
    }

    #[test]
    fn text_and_unreported_output_have_no_usage() {
        assert_eq!(usage("plain text", OutputKind::Text), None);
        assert_eq!(usage("", OutputKind::Codex), None);
        assert_eq!(usage(r#"{"response":"ok"}"#, OutputKind::Gemini), None);
    }
}
