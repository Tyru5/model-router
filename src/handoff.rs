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
    let mut value = schema().clone();
    strip_annotations(&mut value, false);
    value.to_string()
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
    let mut value = schema().clone();
    strip_annotations(&mut value, true);
    tighten(&mut value);
    // Native output is always the current version.
    value["properties"]["schema_version"] = json!({"type": "integer", "enum": [SCHEMA_VERSION]});
    value.to_string()
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
            errors.push(format!(
                "{at} is {value}; expected one of {}",
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
            rejected.push("marker: the last <router-handoff> block is not closed".into());
            break;
        };
        // An unclosed opening marker before this one is not part of it.
        let block = &after[..end];
        let block = block.rsplit_once(OPEN).map_or(block, |(_, inner)| inner);
        markers.push((block.trim().to_owned(), HandoffSource::Marker));
        rest = &after[end + CLOSE.len()..];
    }
    let mut fences = vec![];
    let mut rest = text;
    while let Some(start) = rest.find("```json") {
        let after = &rest[start + "```json".len()..];
        let end = after.find("```").unwrap_or(after.len());
        fences.push((after[..end].trim().to_owned(), HandoffSource::Fence));
        rest = &after[(end + 3).min(after.len())..];
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

/// The stage's handoff and where it came from. Tries native structured
/// output, then the handoff file, then the response's last marker block,
/// fenced JSON, or whole-JSON text; the first valid one wins. Reviews and
/// test stages fail without one; other stages fall back to the response
/// text, marked unstructured.
pub fn resolve(sources: Sources<'_>, role: Role) -> Result<(Handoff, HandoffSource)> {
    let mut rejected = vec![];
    let native = sources
        .native
        .map(|json| (json.to_owned(), HandoffSource::Native));
    // A file may hold markers or a fence too; it is still the file.
    let file: Vec<_> = sources
        .file
        .map(|text| candidates(&storage::clean_terminal(text), &mut rejected))
        .unwrap_or_default()
        .into_iter()
        .map(|(json, _)| (json, HandoffSource::File))
        .collect();
    let text = sources.text.map(storage::clean_terminal);
    let text = text.as_deref().map(str::trim);
    let inline = text
        .map(|text| candidates(text, &mut rejected))
        .unwrap_or_default();
    for (json, source) in native.into_iter().chain(file).chain(inline) {
        match structured(&json, role) {
            Ok(handoff) => return Ok((handoff, source)),
            Err(error) => {
                let error = format!("{}: {error:#}", source.label());
                if !rejected.contains(&error) {
                    rejected.push(error);
                }
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
    let text = text.or(sources.file.map(str::trim)).unwrap_or("");
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

#[derive(Debug, Default)]
pub struct Normalized {
    pub text: String,
    pub error: Option<String>,
    pub permission_blocked: bool,
    /// Claude Code's `structured_output`, as JSON, when launched with `--json-schema`.
    pub structured: Option<String>,
}

/// The harness session an unattended log reports: Codex's `thread.started`
/// thread, Pi's `session` event, OpenCode's `sessionID`, or the `session_id`
/// on Claude Code, Cursor, and Amp stream events.
pub fn session_id(raw: &str) -> Option<String> {
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .find_map(|event| {
            let fields: &[&str] = match event.get("type").and_then(Value::as_str) {
                Some("thread.started") => &["thread_id"],
                Some("session") => &["id"],
                _ => &["session_id", "sessionID"],
            };
            fields
                .iter()
                .find_map(|field| event.get(*field).and_then(Value::as_str))
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
        })
}

/// Only assistant/final-response fields become handoffs. Tool arguments and
/// echoed prompts must never become a review verdict.
pub fn normalize(raw: &str, kind: OutputKind) -> Normalized {
    if kind == OutputKind::Text {
        return Normalized {
            text: storage::clean_terminal(raw),
            ..Normalized::default()
        };
    }
    let cleaned = storage::clean_terminal(raw);
    let events: Vec<Value> = match serde_json::from_str(&cleaned) {
        // Claude prints every message as one array when verbose output is on.
        Ok(Value::Array(events)) => events,
        Ok(value) => vec![value],
        Err(_) => cleaned
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect(),
    };
    let mut result = Normalized::default();
    let mut recognized = false;
    // OpenCode: the message whose text parts form the handoff.
    let mut message_id: Option<String> = None;
    for event in events {
        if event.get("is_error").and_then(Value::as_bool) == Some(true) {
            // Claude's error results carry `errors` instead of `result`.
            let errors: Vec<&str> = event
                .get("errors")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            result.error = Some(
                event
                    .get("result")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| (!errors.is_empty()).then(|| errors.join("\n")))
                    .unwrap_or_else(|| "Harness reported an error".into()),
            );
        }
        if event
            .get("permission_denials")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
        {
            result.permission_blocked = true;
        }
        let kind_name = event.get("type").and_then(Value::as_str).unwrap_or("");
        let failed = kind_name == "error"
            || kind_name == "turn.failed"
            || event.get("error").is_some_and(|v| !v.is_null());
        if failed {
            result.error = Some(storage::truncate(&event.to_string(), 1500));
        }
        // Retries report errors (Claude `api_retry`, provider errors) before
        // the run succeeds; a later successful final event supersedes them.
        match kind {
            OutputKind::Claude | OutputKind::Cursor | OutputKind::Amp => {
                if kind_name == "result" {
                    if !failed && event.get("is_error").and_then(Value::as_bool) != Some(true) {
                        result.error = None;
                    }
                    if let Some(text) = event.get("result").and_then(Value::as_str) {
                        result.text = text.into();
                        recognized = true;
                    }
                    if let Some(output) = event.get("structured_output").filter(|v| v.is_object()) {
                        result.structured = Some(output.to_string());
                    }
                } else if kind_name == "assistant" {
                    if let Some(content) =
                        event.pointer("/message/content").and_then(Value::as_array)
                    {
                        for block in content {
                            if block.get("type").and_then(Value::as_str) == Some("text") {
                                if let Some(text) = block.get("text").and_then(Value::as_str) {
                                    result.text.push_str(text);
                                    result.text.push('\n');
                                    recognized = true;
                                }
                            }
                        }
                    }
                }
            }
            OutputKind::Gemini => {
                if let Some(text) = event.get("response").and_then(Value::as_str) {
                    result.text = text.into();
                    recognized = true;
                }
                // JSON output carries `response`; stream-json ends with a result.
                let succeeded = event.get("response").is_some_and(Value::is_string)
                    || (kind_name == "result"
                        && event.get("status").and_then(Value::as_str) == Some("success"));
                if succeeded && !failed {
                    result.error = None;
                }
            }
            OutputKind::Codex => {
                if kind_name == "item.completed"
                    && event.pointer("/item/type").and_then(Value::as_str) == Some("agent_message")
                {
                    if let Some(text) = event.pointer("/item/text").and_then(Value::as_str) {
                        result.text = text.into();
                        recognized = true;
                    }
                }
                // Stream retries ("Reconnecting...") arrive as error events; a
                // fatal error ends the turn with turn.failed instead.
                if kind_name == "turn.completed" {
                    result.error = None;
                }
            }
            OutputKind::Pi => {
                if kind_name == "message_end"
                    && event.pointer("/message/role").and_then(Value::as_str) == Some("assistant")
                {
                    // The latest assistant message decides: Pi retries failed
                    // attempts automatically.
                    result.error = matches!(
                        event.pointer("/message/stopReason").and_then(Value::as_str),
                        Some("error" | "aborted")
                    )
                    .then(|| {
                        event
                            .pointer("/message/errorMessage")
                            .and_then(Value::as_str)
                            .unwrap_or("Pi reported an error")
                            .into()
                    });
                    if let Some(content) =
                        event.pointer("/message/content").and_then(Value::as_array)
                    {
                        result.text = content
                            .iter()
                            .filter(|v| v.get("type").and_then(Value::as_str) == Some("text"))
                            .filter_map(|v| v.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n");
                        recognized = true;
                    }
                }
            }
            OutputKind::OpenCode => {
                if kind_name == "text" {
                    if let Some(text) = event.pointer("/part/text").and_then(Value::as_str) {
                        // Each step is its own message; only the latest one
                        // with text is the final answer.
                        let id = event
                            .pointer("/part/messageID")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        if id != message_id {
                            result.text.clear();
                            message_id = id;
                        }
                        result.text.push_str(text);
                        result.text.push('\n');
                        recognized = true;
                    }
                }
                // A step that finished normally ends the turn after any retry.
                if kind_name == "step_finish"
                    && event.pointer("/part/reason").and_then(Value::as_str) == Some("stop")
                {
                    result.error = None;
                }
            }
            OutputKind::Text => {}
        }
    }
    // A native structured stream with no assistant message isn't a usable result.
    if !recognized && result.error.is_none() {
        result.error = Some("No final assistant response found in harness output".into());
    }
    result
}

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

    #[test]
    fn text_and_unreported_output_have_no_usage() {
        assert_eq!(usage("plain text", OutputKind::Text), None);
        assert_eq!(usage("", OutputKind::Codex), None);
        assert_eq!(usage(r#"{"response":"ok"}"#, OutputKind::Gemini), None);
    }
}

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
        let native = r#"{"schema_version":2,"summary":"Reviewed","verdict":"needs_changes","findings":[{"severity":"blocking","description":"bug"}],"decisions":[],"files_changed":[],"tests":[],"unresolved":[],"not_applicable":null,"artifacts":[{"kind":"file","path_or_url":"a.rs","description":null}],"confidence":null,"next_steps":[]}"#;
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
}
