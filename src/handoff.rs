use crate::{adapters::OutputKind, config::Role, storage};
use anyhow::{Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::Path, sync::OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Complete,
    Pass,
    NeedsChanges,
    Blocked,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocking,
    Advisory,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub description: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    Passed,
    Failed,
    NotRun,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestEvidence {
    pub command: String,
    pub status: TestStatus,
    #[serde(default)]
    pub required: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Patch,
    File,
    Url,
    Note,
}
/// Something the stage produced for later stages or the person: a patch,
/// file, URL, or note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub path_or_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

/// The handoff schema version the prompt asks for. A handoff without
/// `schema_version` is version 1: the same fields minus artifacts,
/// confidence, and next_steps.
pub const SCHEMA_VERSION: u32 = 2;
/// JSON Schema (draft 2020-12) for a handoff; also schema/handoff.schema.json.
pub const SCHEMA: &str = include_str!("../schema/handoff.schema.json");
/// The schema's file name in each attempt directory, named in the prompt.
pub const SCHEMA_FILE: &str = "handoff.schema.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handoff {
    /// Absent in version 1 handoffs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
    pub summary: String,
    pub verdict: Verdict,
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub files_changed: Vec<String>,
    #[serde(default)]
    pub tests: Vec<TestEvidence>,
    #[serde(default)]
    pub unresolved: Vec<String>,
    /// Why no verification check can apply; a test stage then needs user acceptance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_applicable: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_steps: Vec<String>,
    #[serde(default = "yes")]
    pub structured: bool,
    /// The fan-out candidate a judge stage chose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
}
fn yes() -> bool {
    true
}
impl Handoff {
    /// Plain stage output standing in for a handoff; its lack of structure stays visible.
    pub fn unstructured(summary: String) -> Self {
        Self {
            schema_version: None,
            summary,
            verdict: Verdict::Complete,
            findings: vec![],
            decisions: vec![],
            files_changed: vec![],
            tests: vec![],
            unresolved: vec![],
            not_applicable: None,
            artifacts: vec![],
            confidence: None,
            next_steps: vec![],
            selected: None,
            structured: false,
        }
    }
    pub fn requires_repair(&self) -> bool {
        self.verdict == Verdict::NeedsChanges
            || self
                .findings
                .iter()
                .any(|f| f.severity == Severity::Blocking)
            || self
                .tests
                .iter()
                .any(|t| t.required && t.status == TestStatus::Failed)
    }
    pub fn has_passing_required_check(&self) -> bool {
        self.tests
            .iter()
            .any(|t| t.required && t.status == TestStatus::Passed)
    }
    pub fn not_applicable_reason(&self) -> Option<&str> {
        self.not_applicable
            .as_deref()
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
    }
}

/// Where a stage's handoff came from, in the order they are tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffSource {
    /// The harness's schema-constrained final response (Codex
    /// `--output-schema`, Claude Code `--json-schema`).
    Native,
    /// The file the model wrote at the handoff path.
    File,
    /// The last `<router-handoff>` block in the final response.
    Marker,
    /// The last fenced JSON block in the final response.
    Fence,
    /// The final response as one JSON object.
    Json,
    /// No structured handoff: the response text stands in as the summary.
    Unstructured,
    /// An interactive stage wrote none; the router's placeholder stands in.
    Placeholder,
}
impl HandoffSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::File => "file",
            Self::Marker => "marker",
            Self::Fence => "fence",
            Self::Json => "json",
            Self::Unstructured => "unstructured",
            Self::Placeholder => "placeholder",
        }
    }
}

const OPEN: &str = "<router-handoff>";
const CLOSE: &str = "</router-handoff>";
const PLACEHOLDER: &str = "<your result, with evidence>";
/// Example summaries from the contract, current and earlier. A handoff
/// repeating one echoed the prompt instead of reporting.
const EXAMPLE_SUMMARIES: [&str; 2] = [PLACEHOLDER, "What was established or changed"];

/// The contract's example handoff for `role`.
pub fn example(role: Role) -> String {
    let verdict = if role == Role::Review {
        "pass"
    } else {
        "complete"
    };
    format!(
        r#"{OPEN}
{{"schema_version":{SCHEMA_VERSION},"summary":"{PLACEHOLDER}","verdict":"{verdict}","findings":[],"tests":[],"files_changed":[],"decisions":[],"unresolved":[]}}
{CLOSE}"#
    )
}

/// The HANDOFF CONTRACT prompt section: how to finish, the example, and the
/// fields `role` must fill.
pub fn contract(role: Role, handoff_path: &Path, schema_path: &Path) -> String {
    const FINDINGS: &str = "findings {severity: blocking|advisory, description}";
    const TESTS: &str = "tests {command, status: passed|failed|not_run, required}";
    let (required, optional) = match role {
        Role::Review => (
            format!(
                "verdict pass or needs_changes (blocked if you cannot review); one of {FINDINGS} per problem"
            ),
            String::new(),
        ),
        Role::Test => (
            format!(
                "verdict complete (blocked if you cannot verify); {TESTS} per check you actually ran. If no check can apply, set \"not_applicable\":\"<reason>\" for the user to accept"
            ),
            String::new(),
        ),
        _ => (
            "verdict complete, or blocked if you cannot finish (e.g. no permission)".into(),
            format!("{FINDINGS}, {TESTS} for checks actually run, "),
        ),
    };
    format!(
        "\nHANDOFF CONTRACT (schema: {schema})\nEnd your reply with this block, filled in; if tools permit, also write the JSON alone to {path} (router bookkeeping, allowed when read-only; interactive: before exiting).\n{example}\nRequired: summary; {required}.\nOptional: {optional}artifacts [{{kind: patch|file|url|note, path_or_url, description}}], confidence low|medium|high, next_steps.\n",
        schema = schema_path.display(),
        path = handoff_path.display(),
        example = example(role),
    )
}

fn schema() -> &'static Value {
    static SCHEMA_VALUE: OnceLock<Value> = OnceLock::new();
    SCHEMA_VALUE.get_or_init(|| serde_json::from_str(SCHEMA).expect("handoff schema is JSON"))
}

/// The schema without annotations, as one line: Claude Code takes it inline.
pub fn compact_schema() -> String {
    static COMPACT: OnceLock<String> = OnceLock::new();
    COMPACT
        .get_or_init(|| {
            let mut value = schema().clone();
            strip_annotations(&mut value, false);
            value.to_string()
        })
        .clone()
}

/// The schema for strict structured output (Codex): every property
/// required, optional scalars nullable, closed objects, no annotations or
/// string limits. Nulls read as absent.
pub fn strict_schema() -> String {
    fn tighten(schema: &mut Value) {
        let Some(object) = schema.as_object_mut() else {
            return;
        };
        if let Some(items) = object.get_mut("items") {
            tighten(items);
        }
        let required = object.get("required").cloned().unwrap_or(json!([]));
        let Some(Value::Object(properties)) = object.get_mut("properties") else {
            return;
        };
        for (name, property) in properties.iter_mut() {
            tighten(property);
            let kind = property.get("type").and_then(Value::as_str).unwrap_or("");
            let optional = !required
                .as_array()
                .is_some_and(|r| r.iter().any(|r| r == name));
            // Arrays and booleans have defaults; an empty list or false suffices.
            if optional && !matches!(kind, "array" | "boolean") {
                property["type"] = json!([kind, "null"]);
                if let Some(Value::Array(options)) = property.get_mut("enum") {
                    options.push(Value::Null);
                }
            }
        }
        let names: Vec<Value> = properties.keys().cloned().map(Value::String).collect();
        object.insert("required".into(), Value::Array(names));
        object.insert("additionalProperties".into(), Value::Bool(false));
    }
    static STRICT: OnceLock<String> = OnceLock::new();
    STRICT
        .get_or_init(|| {
            let mut value = schema().clone();
            strip_annotations(&mut value, true);
            tighten(&mut value);
            // Native output is always the current version.
            value["properties"]["schema_version"] =
                json!({"type": "integer", "enum": [SCHEMA_VERSION]});
            value.to_string()
        })
        .clone()
}

fn strip_annotations(value: &mut Value, limits: bool) {
    match value {
        Value::Object(object) => {
            for key in ["$schema", "$id", "title", "description"] {
                object.remove(key);
            }
            if limits {
                object.remove("minLength");
            }
            // Property names are not annotations: only strip inside schemas.
            if let Some(Value::Object(properties)) = object.get_mut("properties") {
                properties
                    .values_mut()
                    .for_each(|p| strip_annotations(p, limits));
            }
            if let Some(items) = object.get_mut("items") {
                strip_annotations(items, limits);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|i| strip_annotations(i, limits)),
        _ => {}
    }
}

/// Errors in `value` against the subset of JSON Schema the handoff schema
/// uses: type, enum, required, properties, items, and minLength.
/// `closed` also enforces `additionalProperties: false`; parsing leaves it
/// off so unknown fields stay ignored, as in version 1.
fn validate(value: &Value, schema: &Value, path: &str, closed: bool, errors: &mut Vec<String>) {
    let at = if path.is_empty() { "handoff" } else { path };
    if let Some(types) = schema.get("type") {
        let allowed: Vec<&str> = match types {
            Value::String(kind) => vec![kind.as_str()],
            Value::Array(kinds) => kinds.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        let actual = match value {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        if !allowed
            .iter()
            .any(|t| *t == actual || (*t == "number" && actual == "integer"))
        {
            errors.push(format!(
                "{at} must be {}, not {actual}",
                allowed.join(" or ")
            ));
            return;
        }
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array) {
        if !options.contains(value) {
            let options: Vec<String> = options.iter().map(Value::to_string).collect();
            let shown = storage::clean_terminal(&storage::truncate(&value.to_string(), 200));
            errors.push(format!(
                "{at} is {shown}; expected one of {}",
                options.join(", ")
            ));
        }
    }
    if let (Some(min), Some(text)) = (
        schema.get("minLength").and_then(Value::as_u64),
        value.as_str(),
    ) {
        if (text.trim().chars().count() as u64) < min {
            errors.push(format!("{at} is empty"));
        }
    }
    let child = |key: &str| {
        if path.is_empty() {
            key.to_owned()
        } else {
            format!("{path}.{key}")
        }
    };
    if let Some(object) = value.as_object() {
        let required = schema.get("required").and_then(Value::as_array);
        for key in required.into_iter().flatten().filter_map(Value::as_str) {
            if !object.contains_key(key) {
                errors.push(format!("{} is missing", child(key)));
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        for (key, item) in object {
            match properties.and_then(|p| p.get(key)) {
                Some(property) => validate(item, property, &child(key), closed, errors),
                None if closed && schema.get("additionalProperties") == Some(&json!(false)) => {
                    errors.push(format!("{} is not a handoff field", child(key)));
                }
                None => {}
            }
        }
    }
    if let (Some(items), Some(list)) = (schema.get("items"), value.as_array()) {
        for (index, item) in list.iter().enumerate() {
            validate(item, items, &format!("{at}[{index}]"), closed, errors);
        }
    }
}

/// Removes null members, so an explicit null reads as an absent field.
fn drop_nulls(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.retain(|_, v| !v.is_null());
            object.values_mut().for_each(drop_nulls);
        }
        Value::Array(items) => items.iter_mut().for_each(drop_nulls),
        _ => {}
    }
}

/// One candidate's JSON as a structured handoff for `role`.
fn structured(json: &str, role: Role) -> Result<Handoff> {
    let mut value: Value =
        serde_json::from_str(json.trim()).map_err(|e| anyhow!("not a JSON object ({e})"))?;
    drop_nulls(&mut value);
    let mut errors = vec![];
    validate(&value, schema(), "", false, &mut errors);
    if !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    let mut result: Handoff = serde_json::from_value(value)?;
    if let Some(version) = result.schema_version {
        ensure!(
            (1..=SCHEMA_VERSION).contains(&version),
            "schema_version {version} is newer than this router reads ({SCHEMA_VERSION})"
        );
    }
    ensure!(
        !EXAMPLE_SUMMARIES.contains(&result.summary.trim()),
        "it repeats the contract example instead of reporting this stage"
    );
    if role == Role::Review {
        ensure!(
            matches!(
                result.verdict,
                Verdict::Pass | Verdict::NeedsChanges | Verdict::Blocked
            ),
            "Review requires an explicit pass, needs_changes, or blocked verdict"
        );
    }
    result.structured = true;
    Ok(result)
}

fn extract_marker_json_with_embedded_close(
    after: &str,
    before_first_close: usize,
) -> Option<(&str, usize)> {
    let inner_start = after[..before_first_close]
        .rfind(OPEN)
        .map_or(0, |i| i + OPEN.len());
    let candidate_region = &after[inner_start..];
    let trimmed = candidate_region.trim_start();
    if !trimmed.starts_with('{') {
        return None;
    }
    let mut de = serde_json::Deserializer::from_str(trimmed).into_iter::<Value>();
    let Some(Ok(_)) = de.next() else {
        return None;
    };
    let json_end = de.byte_offset();
    let after_json = &trimmed[json_end..];
    let ws = after_json.len() - after_json.trim_start().len();
    if after_json[ws..].starts_with(CLOSE) {
        let trimmed_offset = candidate_region.len() - trimmed.len();
        let consumed_end = inner_start + trimmed_offset + json_end + ws + CLOSE.len();
        return Some((&trimmed[..json_end], consumed_end));
    }
    None
}

fn strip_fence_json_prefix(rest: &str) -> Option<&str> {
    let s = rest.trim_start();
    let prefix = s.get(..4)?;
    if !prefix.eq_ignore_ascii_case("json") {
        return None;
    }
    let after = &s[4..];
    if after.starts_with(char::is_whitespace) {
        Some(after.trim_start())
    } else {
        None
    }
}

/// Handoff JSON in response text, most trusted first: `<router-handoff>`
/// blocks from last to first, then fenced JSON blocks from last to first,
/// then the whole text.
fn candidates(text: &str, rejected: &mut Vec<String>) -> Vec<(String, HandoffSource)> {
    let text = text.trim();
    let mut markers = vec![];
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        let after = &rest[start + OPEN.len()..];
        let Some(end) = after.find(CLOSE) else {
            if rejected.len() < 8 {
                rejected.push("marker: the last <router-handoff> block is not closed".into());
            }
            break;
        };
        // An unclosed opening marker before this one is not part of it.
        let block = &after[..end];
        let block = block.rsplit_once(OPEN).map_or(block, |(_, inner)| inner);
        let trimmed = block.trim();
        if serde_json::from_str::<Value>(trimmed).is_ok() {
            markers.push((trimmed.to_owned(), HandoffSource::Marker));
            rest = &after[end + CLOSE.len()..];
        } else if let Some((json_slice, consumed_end)) =
            extract_marker_json_with_embedded_close(after, end)
        {
            markers.push((json_slice.to_owned(), HandoffSource::Marker));
            rest = &after[consumed_end..];
        } else {
            markers.push((trimmed.to_owned(), HandoffSource::Marker));
            rest = &after[end + CLOSE.len()..];
        }
    }
    let mut fences = vec![];
    let mut offset = 0usize;
    while offset < text.len() {
        let line_end = text[offset..].find('\n').map_or(text.len(), |i| offset + i);
        let line = &text[offset..line_end];
        let next_offset = if line_end < text.len() {
            line_end + 1
        } else {
            text.len()
        };
        let trimmed = line.trim();
        if let Some(rest_after_ticks) = trimmed.strip_prefix("```") {
            if rest_after_ticks.trim().eq_ignore_ascii_case("json") {
                let content_start = next_offset;
                let mut scan = content_start;
                let mut close_span: Option<(usize, usize)> = None;
                while scan < text.len() {
                    let end = text[scan..].find('\n').map_or(text.len(), |i| scan + i);
                    let candidate_line = text[scan..end].trim();
                    if candidate_line.starts_with("```")
                        && candidate_line.bytes().all(|b| b == b'`')
                    {
                        close_span = Some((scan, if end < text.len() { end + 1 } else { end }));
                        break;
                    }
                    if end == text.len() {
                        break;
                    }
                    scan = end + 1;
                }
                if let Some((close_start, after_close)) = close_span {
                    fences.push((
                        text[content_start..close_start].trim().to_owned(),
                        HandoffSource::Fence,
                    ));
                    offset = after_close;
                    continue;
                } else if let Some(inline_end) = text[content_start..].find("```") {
                    fences.push((
                        text[content_start..content_start + inline_end]
                            .trim()
                            .to_owned(),
                        HandoffSource::Fence,
                    ));
                    offset = content_start + inline_end + 3;
                    continue;
                } else {
                    fences.push((
                        text[content_start..].trim().to_owned(),
                        HandoffSource::Fence,
                    ));
                    break;
                }
            } else if let Some(after_lang) = strip_fence_json_prefix(rest_after_ticks) {
                let body = after_lang.strip_suffix("```").unwrap_or(after_lang);
                fences.push((body.trim().to_owned(), HandoffSource::Fence));
            }
        }
        offset = next_offset;
    }
    let whole = text
        .starts_with('{')
        .then(|| (text.to_owned(), HandoffSource::Json));
    markers
        .into_iter()
        .rev()
        .chain(fences.into_iter().rev())
        .chain(whole)
        .collect()
}

/// What a finished stage left behind to read a handoff from.
#[derive(Debug, Default, Clone, Copy)]
pub struct Sources<'a> {
    /// The schema-constrained final response, when the harness was launched
    /// with a native output schema (see `native_output`).
    pub native: Option<&'a str>,
    /// The handoff file's contents.
    pub file: Option<&'a str>,
    /// The final assistant response. None for interactive terminal
    /// transcripts, which echo the prompt and its example.
    pub text: Option<&'a str>,
}

/// The schema-constrained response of a stage launched with a native output
/// schema: Claude Code's `structured_output`, or Codex's final message.
pub fn native_output(
    normalized: &Normalized,
    kind: OutputKind,
    final_message: &str,
) -> Option<String> {
    match kind {
        OutputKind::Claude => normalized.structured.clone(),
        OutputKind::Codex => Some(final_message.to_owned()),
        _ => None,
    }
}

/// A response's handoff; see `resolve`.
pub fn parse(text: &str, role: Role) -> Result<Handoff> {
    resolve(
        Sources {
            text: Some(text),
            ..Sources::default()
        },
        role,
    )
    .map(|(handoff, _)| handoff)
}

fn try_handoff_candidate(
    json: &str,
    source: HandoffSource,
    role: Role,
    rejected: &mut Vec<String>,
) -> Option<(Handoff, HandoffSource)> {
    match structured(json, role) {
        Ok(handoff) => Some((handoff, source)),
        Err(error) => {
            if rejected.len() < 8 {
                let error = format!("{}: {error:#}", source.label());
                if !rejected.contains(&error) {
                    rejected.push(error);
                }
            }
            None
        }
    }
}

/// The stage's handoff and where it came from. Tries native structured
/// output, then the handoff file, then the response's last marker block,
/// fenced JSON, or whole-JSON text; the first valid one wins. Reviews and
/// test stages fail without one; other stages fall back to the response
/// text, marked unstructured.
pub fn resolve(sources: Sources<'_>, role: Role) -> Result<(Handoff, HandoffSource)> {
    let mut rejected = vec![];
    if let Some(json) = sources.native {
        if let Some(found) = try_handoff_candidate(json, HandoffSource::Native, role, &mut rejected)
        {
            return Ok(found);
        }
    }
    let cleaned_file = sources.file.map(storage::clean_terminal);
    if let Some(file_text) = cleaned_file.as_deref() {
        // A file may hold markers or a fence too; it is still the file.
        for (json, _) in candidates(file_text, &mut rejected) {
            if let Some(found) =
                try_handoff_candidate(&json, HandoffSource::File, role, &mut rejected)
            {
                return Ok(found);
            }
        }
    }
    let cleaned_text = sources.text.map(storage::clean_terminal);
    let trimmed_text = cleaned_text.as_deref().map(str::trim);
    if let Some(text) = trimmed_text {
        for (json, source) in candidates(text, &mut rejected) {
            if let Some(found) = try_handoff_candidate(&json, source, role, &mut rejected) {
                return Ok(found);
            }
        }
    }
    let tried = if rejected.is_empty() {
        String::new()
    } else {
        format!(" ({})", rejected.join("; "))
    };
    if role == Role::Review {
        bail!(
            "Review did not return a valid structured verdict. Reopen or retry it; it has not passed{tried}"
        );
    }
    if role == Role::Test {
        bail!(
            "Test stage did not return a valid structured handoff. Reopen or retry it with test evidence; verification has not passed{tried}"
        );
    }
    let text = trimmed_text
        .or_else(|| cleaned_file.as_deref().map(str::trim))
        .unwrap_or("");
    ensure!(
        !text.contains(OPEN),
        "The stage returned a malformed structured handoff; inspect it and retry{tried}"
    );
    ensure!(
        !text.is_empty(),
        "Harness exited without a handoff or final response. Reopen the stage to complete it"
    );
    // Ordinary stage output can still be useful. Its lack of structure stays visible.
    Ok((
        Handoff::unstructured(storage::truncate(text, 12000)),
        HandoffSource::Unstructured,
    ))
}

mod normalize;
mod usage;

pub use normalize::{Normalized, ProtocolError, normalize, session_id};
pub use usage::{SessionRoots, Usage, session_usage, session_usage_in, usage, usage_file};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn logs_report_codex_threads_and_claude_sessions() {
        let codex =
            "{\"type\":\"thread.started\",\"thread_id\":\"t-1\"}\n{\"type\":\"turn.started\"}";
        assert_eq!(session_id(codex).as_deref(), Some("t-1"));
        let claude = "not json\n{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-1\"}";
        assert_eq!(session_id(claude).as_deref(), Some("s-1"));
        assert_eq!(
            session_id("{\"type\":\"result\",\"session_id\":\"\"}"),
            None
        );
        // Never a value the harness would read as one of its options.
        assert_eq!(
            session_id("{\"type\":\"result\",\"session_id\":\"--yolo\"}"),
            None
        );
    }
    #[test]
    fn review_cannot_pass_on_unstructured_output() {
        assert!(parse("Looks good!", Role::Review).is_err());
    }
    #[test]
    fn passing_verdict_with_blocker_still_requires_repair() {
        let h = parse(r#"{"summary":"review","verdict":"pass","findings":[{"severity":"blocking","description":"data loss"}]}"#, Role::Review).unwrap();
        assert!(h.requires_repair());
    }
    #[test]
    fn required_failed_test_requests_repair() {
        let h = parse(r#"{"summary":"review","verdict":"pass","tests":[{"command":"test","status":"failed","required":true}]}"#, Role::Review).unwrap();
        assert!(h.requires_repair());
    }
    #[test]
    fn only_passing_required_tests_are_evidence() {
        for (tests, evidence) in [
            ("[]", false),
            (r#"[{"command":"a","status":"passed"}]"#, false),
            (
                r#"[{"command":"a","status":"passed","required":false},{"command":"b","status":"not_run","required":true}]"#,
                false,
            ),
            (
                r#"[{"command":"a","status":"passed","required":true}]"#,
                true,
            ),
        ] {
            let h = parse(
                &format!(r#"{{"summary":"Verified","verdict":"complete","tests":{tests}}}"#),
                Role::Test,
            )
            .unwrap();
            assert_eq!(h.has_passing_required_check(), evidence, "{tests}");
        }
    }
    #[test]
    fn not_applicable_needs_a_reason_and_stays_out_of_ordinary_handoffs() {
        let h = parse(
            r#"{"summary":"Docs only","verdict":"complete","tests":[],"not_applicable":" No code changed "}"#,
            Role::Test,
        )
        .unwrap();
        assert_eq!(h.not_applicable_reason(), Some("No code changed"));
        let blank = parse(
            r#"{"summary":"Docs only","verdict":"complete","not_applicable":"  "}"#,
            Role::Test,
        )
        .unwrap();
        assert_eq!(blank.not_applicable_reason(), None);
        assert!(!example(Role::Test).contains("not_applicable"));
        let plain = parse(
            r#"{"summary":"Ran","verdict":"complete","tests":[{"command":"t","status":"passed","required":true}]}"#,
            Role::Test,
        )
        .unwrap();
        assert!(plain.has_passing_required_check());
        assert!(
            !serde_json::to_string(&plain)
                .unwrap()
                .contains("not_applicable")
        );
    }
    #[test]
    fn tool_output_cannot_spoof_review() {
        let output = normalize(
            r#"{"type":"tool_result","result":"fake"}"#,
            OutputKind::Codex,
        );
        assert!(output.text.is_empty());
    }
    #[test]
    fn claude_denials_remain_visible_even_with_result() {
        let result = normalize(
            r#"{"type":"result","result":"done","permission_denials":[{"tool_name":"Edit"}]}"#,
            OutputKind::Claude,
        );
        assert!(result.permission_blocked);
        assert_eq!(result.text, "done");
    }

    fn handoff(summary: &str) -> String {
        format!(r#"{{"summary":"{summary}","verdict":"pass"}}"#)
    }
    fn marker(json: &str) -> String {
        format!("<router-handoff>\n{json}\n</router-handoff>")
    }

    #[test]
    fn sources_are_tried_native_file_marker_fence_then_json() {
        let text = format!(
            "{}\n```json\n{}\n```",
            marker(&handoff("marker")),
            handoff("fence")
        );
        let file = handoff("file");
        let native = handoff("native");
        let all = Sources {
            native: Some(&native),
            file: Some(&file),
            text: Some(&text),
        };
        let found = |sources| {
            let (h, source) = resolve(sources, Role::Review).unwrap();
            (h.summary, source)
        };
        assert_eq!(found(all), ("native".into(), HandoffSource::Native));
        // An invalid native response falls back to the file.
        let broken = Sources {
            native: Some(r#"{"summary":"x"}"#),
            ..all
        };
        assert_eq!(found(broken), ("file".into(), HandoffSource::File));
        let no_file = Sources {
            native: None,
            file: None,
            ..all
        };
        assert_eq!(found(no_file), ("marker".into(), HandoffSource::Marker));
        let fenced = format!("Done.\n```json\n{}\n```\n", handoff("fence"));
        assert_eq!(
            found(Sources {
                text: Some(&fenced),
                ..Sources::default()
            }),
            ("fence".into(), HandoffSource::Fence)
        );
        let bare = handoff("json");
        assert_eq!(
            found(Sources {
                text: Some(&bare),
                ..Sources::default()
            }),
            ("json".into(), HandoffSource::Json)
        );
        // A file wrapped in markers still counts as the file.
        let wrapped = marker(&handoff("wrapped"));
        assert_eq!(
            found(Sources {
                file: Some(&wrapped),
                ..Sources::default()
            }),
            ("wrapped".into(), HandoffSource::File)
        );
        let (plain, source) = resolve(
            Sources {
                text: Some("Just prose"),
                ..Sources::default()
            },
            Role::Research,
        )
        .unwrap();
        assert_eq!(
            (plain.structured, source),
            (false, HandoffSource::Unstructured)
        );
    }

    #[test]
    fn echoed_examples_are_never_answers() {
        let echo = example(Role::Review);
        let legacy = marker(
            r#"{"summary":"What was established or changed","verdict":"complete","findings":[],"decisions":[],"files_changed":[],"tests":[{"command":"check actually run","status":"passed","required":true}],"unresolved":[]}"#,
        );
        // The real answer precedes a later echo of either example.
        for echoed in [&echo, &legacy] {
            let text = format!("{}\n\nPrompt was:\n{echoed}", marker(&handoff("Real")));
            assert_eq!(parse(&text, Role::Review).unwrap().summary, "Real");
        }
        // Only the example: no verdict, and no pass.
        for echoed in [&echo, &legacy] {
            let error = parse(echoed, Role::Review).unwrap_err().to_string();
            assert!(error.contains("repeats the contract example"), "{error}");
            assert!(parse(echoed, Role::Test).is_err());
            let error = parse(echoed, Role::Implementation).unwrap_err().to_string();
            assert!(error.contains("malformed"), "{error}");
        }
        // The file is checked too.
        let file = example(Role::Implementation);
        assert!(
            resolve(
                Sources {
                    file: Some(&file),
                    ..Sources::default()
                },
                Role::Test
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_fields_are_named_with_their_allowed_values() {
        let error = parse(r#"{"summary":"s","verdict":"passed"}"#, Role::Review)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(
                r#"verdict is "passed"; expected one of "complete", "pass", "needs_changes", "blocked""#
            ),
            "{error}"
        );
        let error = parse(
            r#"{"summary":"s","verdict":"complete","artifacts":[{"kind":"diff","path_or_url":"p"}],"tests":[{"command":"t"}]}"#,
            Role::Test,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(r#"artifacts[0].kind is "diff""#), "{error}");
        assert!(error.contains("tests[0].status is missing"), "{error}");
        let error = parse(
            r#"{"summary":"s","verdict":"pass","schema_version":3}"#,
            Role::Review,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("schema_version"), "{error}");
        let error = parse("<router-handoff>\n{\"summary\":", Role::Review)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not closed"), "{error}");
    }

    #[test]
    fn version_1_handoffs_read_and_write_unchanged() {
        let v1 = r#"{"summary":"Done","verdict":"complete","findings":[],"decisions":["d"],"files_changed":["a.rs"],"tests":[{"command":"t","status":"passed","required":true}],"unresolved":[],"structured":true}"#;
        // As a saved run stores it, and as a model returns it.
        let saved: Handoff = serde_json::from_str(v1).unwrap();
        assert_eq!(serde_json::to_string(&saved).unwrap(), v1);
        let parsed = parse(v1, Role::Test).unwrap();
        assert_eq!(parsed.schema_version, None);
        assert!(parsed.artifacts.is_empty() && parsed.next_steps.is_empty());
        assert_eq!(parsed.confidence, None);
        // Unknown fields stay ignored, and nulls read as absent.
        let loose = parse(
            r#"{"summary":"s","verdict":"complete","notes":"x","findings":null,"not_applicable":null}"#,
            Role::Implementation,
        )
        .unwrap();
        assert!(loose.structured && loose.findings.is_empty());

        let v2 = parse(
            r#"{"schema_version":2,"summary":"s","verdict":"complete","artifacts":[{"kind":"patch","path_or_url":"fix.diff","description":"the fix"},{"kind":"url","path_or_url":"https://x"}],"confidence":"medium","next_steps":["ship"]}"#,
            Role::Implementation,
        )
        .unwrap();
        assert_eq!(v2.schema_version, Some(2));
        assert_eq!(v2.artifacts[0].kind, ArtifactKind::Patch);
        assert_eq!(v2.artifacts[1].description, None);
        assert_eq!(v2.confidence, Some(Confidence::Medium));
        assert_eq!(v2.next_steps, ["ship"]);
    }

    fn strings(value: &Value) -> Vec<String> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn schema_and_struct_agree() {
        let schema = schema();
        assert_eq!(
            schema["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        // Every serialized field is a schema property, and back.
        let full = Handoff {
            schema_version: Some(2),
            findings: vec![Finding {
                severity: Severity::Advisory,
                description: "d".into(),
            }],
            tests: vec![TestEvidence {
                command: "t".into(),
                status: TestStatus::NotRun,
                required: false,
            }],
            not_applicable: Some("n".into()),
            artifacts: vec![Artifact {
                kind: ArtifactKind::Note,
                path_or_url: "p".into(),
                description: Some("d".into()),
            }],
            confidence: Some(Confidence::Low),
            next_steps: vec!["n".into()],
            selected: Some("c1".into()),
            ..Handoff::unstructured("s".into())
        };
        let mut value = serde_json::to_value(&full).unwrap();
        value.as_object_mut().unwrap().remove("structured");
        let mut errors = vec![];
        validate(&value, schema, "", true, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        let mut fields: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        let mut properties: Vec<_> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        fields.sort();
        properties.sort();
        assert_eq!(fields, properties);
        // Required keys are exactly the fields without serde defaults.
        assert_eq!(strings(&schema["required"]), ["summary", "verdict"]);
        assert!(serde_json::from_str::<Handoff>(r#"{"summary":"s"}"#).is_err());
        assert!(serde_json::from_str::<Handoff>(r#"{"verdict":"pass"}"#).is_err());
        let items = |name: &str| &schema["properties"][name]["items"];
        assert_eq!(
            strings(&items("findings")["required"]),
            ["severity", "description"]
        );
        assert_eq!(strings(&items("tests")["required"]), ["command", "status"]);
        assert_eq!(
            strings(&items("artifacts")["required"]),
            ["kind", "path_or_url"]
        );
        // Every enum value deserializes, and every variant is listed.
        fn same<T: Serialize + serde::de::DeserializeOwned>(schema: &Value, variants: &[T]) {
            let listed = strings(&schema["enum"]);
            for name in &listed {
                serde_json::from_value::<T>(Value::String(name.clone())).unwrap();
            }
            let names: Vec<String> = variants
                .iter()
                .map(|v| {
                    serde_json::to_value(v)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect();
            assert_eq!(names, listed);
        }
        let property = |name: &str| &schema["properties"][name];
        use Verdict::*;
        same(
            property("verdict"),
            &[Complete, Pass, NeedsChanges, Blocked],
        );
        same(
            &items("findings")["properties"]["severity"],
            &[Severity::Blocking, Severity::Advisory],
        );
        same(
            &items("tests")["properties"]["status"],
            &[TestStatus::Passed, TestStatus::Failed, TestStatus::NotRun],
        );
        same(
            &items("artifacts")["properties"]["kind"],
            &[
                ArtifactKind::Patch,
                ArtifactKind::File,
                ArtifactKind::Url,
                ArtifactKind::Note,
            ],
        );
        same(
            property("confidence"),
            &[Confidence::Low, Confidence::Medium, Confidence::High],
        );
        // The contract's examples are schema-valid handoffs.
        for role in Role::ALL {
            let example = example(role);
            let json = example.lines().nth(1).unwrap();
            let mut errors = vec![];
            validate(
                &serde_json::from_str(json).unwrap(),
                schema,
                "",
                true,
                &mut errors,
            );
            assert!(errors.is_empty(), "{role:?}: {errors:?}");
        }
    }

    #[test]
    fn strict_schema_requires_every_field_and_reads_back() {
        let strict: Value = serde_json::from_str(&strict_schema()).unwrap();
        fn closed(schema: &Value) {
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                let mut keys: Vec<_> = properties.keys().cloned().collect();
                let mut required = strings(&schema["required"]);
                keys.sort();
                required.sort();
                assert_eq!(keys, required);
                assert_eq!(schema["additionalProperties"], false);
                properties.values().for_each(closed);
            }
            if let Some(items) = schema.get("items") {
                closed(items);
            }
            for key in ["$schema", "$id", "title", "description", "minLength"] {
                assert!(schema.get(key).is_none(), "{key}");
            }
        }
        closed(&strict);
        assert_eq!(strict["properties"]["schema_version"]["enum"], json!([2]));
        assert_eq!(
            strict["properties"]["confidence"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(
            strict["properties"]["tests"]["items"]["properties"]["required"]["type"],
            "boolean"
        );
        // What Codex returns under it: every field, nulls for unset scalars.
        let native = r#"{"schema_version":2,"summary":"Reviewed","verdict":"needs_changes","findings":[{"severity":"blocking","description":"bug"}],"decisions":[],"files_changed":[],"tests":[],"unresolved":[],"not_applicable":null,"artifacts":[{"kind":"file","path_or_url":"a.rs","description":null}],"confidence":null,"next_steps":[],"selected":null}"#;
        let mut errors = vec![];
        validate(
            &serde_json::from_str(native).unwrap(),
            &strict,
            "",
            true,
            &mut errors,
        );
        assert!(errors.is_empty(), "{errors:?}");
        let parsed = parse(native, Role::Review).unwrap();
        assert!(parsed.requires_repair());
        assert_eq!(parsed.artifacts[0].description, None);
        // Claude's inline schema keeps limits but drops annotations.
        let compact: Value = serde_json::from_str(&compact_schema()).unwrap();
        assert!(compact.get("$schema").is_none() && compact.get("description").is_none());
        assert_eq!(compact["properties"]["summary"]["minLength"], 1);
        assert_eq!(
            compact["properties"]["findings"]["items"]["properties"]["description"]["type"],
            "string"
        );
    }

    #[test]
    fn claude_structured_output_is_kept_apart_from_the_text() {
        let out = normalize(
            r#"{"type":"result","result":"{\"summary\":\"s\"}","structured_output":{"summary":"s","verdict":"pass"}}"#,
            OutputKind::Claude,
        );
        assert_eq!(
            native_output(&out, OutputKind::Claude, &out.text).as_deref(),
            Some(r#"{"summary":"s","verdict":"pass"}"#)
        );
        let plain = normalize(r#"{"type":"result","result":"done"}"#, OutputKind::Claude);
        assert_eq!(native_output(&plain, OutputKind::Claude, "done"), None);
        assert_eq!(native_output(&plain, OutputKind::Pi, "done"), None);
        assert_eq!(
            native_output(&plain, OutputKind::Codex, "{}").as_deref(),
            Some("{}")
        );
    }

    #[test]
    fn reported_errors_cannot_carry_terminal_escapes() {
        // JSON-escaped ESC and BEL decode to real control bytes.
        for (line, kind) in [
            (
                r#"{"type":"result","is_error":true,"result":"quota \u001b]0;pwned\u0007\u001b[2Jexhausted"}"#,
                OutputKind::Claude,
            ),
            (
                r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"quota \u001b]0;pwned\u0007\u001b[2Jexhausted"}}"#,
                OutputKind::Pi,
            ),
            // Serialized JSON escapes only C0 controls; C1 CSI and DEL stay raw.
            (
                r#"{"type":"turn.failed","error":{"message":"quota \u009b2J\u007fexhausted"}}"#,
                OutputKind::Codex,
            ),
        ] {
            let error = normalize(line, kind).error.unwrap();
            assert!(!error.chars().any(char::is_control), "{kind:?}: {error:?}");
            assert!(
                error.contains("quota") && error.contains("exhausted"),
                "{error}"
            );
        }
    }

    #[test]
    fn normalize_accepts_error_false_and_claude_structured_output_without_result_text() {
        let out = normalize(
            r#"{"type":"result","error":false,"is_error":false,"structured_output":{"summary":"ok","verdict":"pass"}}"#,
            OutputKind::Claude,
        );
        assert!(out.error.is_none(), "{:?}", out.error);
        assert!(out.structured.is_some());

        let out_empty_err = normalize(
            r#"{"type":"result","error":"","result":"done"}"#,
            OutputKind::Claude,
        );
        assert!(out_empty_err.error.is_none(), "{:?}", out_empty_err.error);
        assert_eq!(out_empty_err.text, "done");
    }

    #[test]
    fn candidates_handle_embedded_fences_uppercase_json_and_clean_file_fallback() {
        let fenced = concat!(
            "```JSON\n",
            r#"{"summary":"Run ```sh\ncargo test\n``` before shipping","verdict":"complete"}"#,
            "\n```\n"
        );
        let (h, src) = resolve(
            Sources {
                text: Some(fenced),
                ..Sources::default()
            },
            Role::Implementation,
        )
        .unwrap();
        assert_eq!(src, HandoffSource::Fence);
        assert!(h.summary.contains("cargo test"));

        // `jsonp` must not match as a json fence.
        assert!(
            resolve(
                Sources {
                    text: Some("```jsonp\n{\"summary\":\"x\",\"verdict\":\"pass\"}\n```"),
                    ..Sources::default()
                },
                Role::Review
            )
            .is_err()
        );

        // Marker containing `</router-handoff>` inside JSON string still parses.
        let marked = r#"<router-handoff>
{"summary":"Mention </router-handoff> inside JSON","verdict":"pass"}
</router-handoff>"#;
        let (h, src) = resolve(
            Sources {
                text: Some(marked),
                ..Sources::default()
            },
            Role::Review,
        )
        .unwrap();
        assert_eq!(src, HandoffSource::Marker);
        assert!(h.summary.contains("</router-handoff>"));

        // Interactive fallback to unstructured `sources.file` strips ANSI escapes.
        let (plain, src) = resolve(
            Sources {
                file: Some("plain \x1b[31mred\x1b[0m output"),
                ..Sources::default()
            },
            Role::Implementation,
        )
        .unwrap();
        assert_eq!(src, HandoffSource::Unstructured);
        assert_eq!(plain.summary, "plain red output");
    }
}
