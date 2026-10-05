use crate::{adapters::OutputKind, config::Role, storage};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handoff {
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
    #[serde(default = "yes")]
    pub structured: bool,
}
fn yes() -> bool {
    true
}
impl Handoff {
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

pub const FORMAT: &str = r#"<router-handoff>
{"summary":"What was established or changed","verdict":"complete","findings":[],"decisions":[],"files_changed":[],"tests":[{"command":"check actually run","status":"passed","required":true}],"unresolved":[]}
</router-handoff>
Use complete for normal successful stages. Reviews MUST use pass or needs_changes.
Test stages MUST return a structured handoff reporting each check actually run in tests.
Only if no check can apply, a test stage may set "not_applicable":"reason"; the user must accept it.
Use blocked if you cannot complete the task (including missing permission).
Findings use {"severity":"blocking" or "advisory","description":"specific evidence"}.
Tests use status passed, failed, or not_run. Never claim a test ran if it did not.
"#;

pub fn parse(text: &str, role: Role) -> Result<Handoff> {
    let cleaned = storage::clean_terminal(text);
    let text = cleaned.trim();
    let candidate = if let Some((_, rest)) = text.rsplit_once("<router-handoff>") {
        rest.split_once("</router-handoff>")
            .context("Incomplete handoff marker")?
            .0
            .trim()
    } else if let Some(rest) = text.strip_prefix("```json") {
        rest.trim().strip_suffix("```").unwrap_or(rest).trim()
    } else {
        text
    };
    if let Ok(mut result) = serde_json::from_str::<Handoff>(candidate) {
        ensure!(
            !result.summary.trim().is_empty(),
            "Handoff summary is empty"
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
        return Ok(result);
    }
    if role == Role::Review {
        bail!(
            "Review did not return a valid structured verdict. Reopen or retry it; it has not passed"
        );
    }
    if role == Role::Test {
        bail!(
            "Test stage did not return a valid structured handoff. Reopen or retry it with test evidence; verification has not passed"
        );
    }
    ensure!(
        !text.contains("<router-handoff>"),
        "The stage returned a malformed structured handoff; inspect it and retry"
    );
    ensure!(
        !text.is_empty(),
        "Harness exited without a handoff or final response. Reopen the stage to complete it"
    );
    // Ordinary stage output can still be useful. Its lack of structure stays visible.
    Ok(Handoff {
        summary: storage::truncate(text, 12000),
        verdict: Verdict::Complete,
        findings: vec![],
        decisions: vec![],
        files_changed: vec![],
        tests: vec![],
        unresolved: vec![],
        not_applicable: None,
        structured: false,
    })
}

#[derive(Debug, Default)]
pub struct Normalized {
    pub text: String,
    pub error: Option<String>,
    pub permission_blocked: bool,
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
        if kind_name == "error"
            || kind_name == "turn.failed"
            || event.get("error").is_some_and(|v| !v.is_null())
        {
            result.error = Some(storage::truncate(&event.to_string(), 1500));
        }
        match kind {
            OutputKind::Claude | OutputKind::Cursor | OutputKind::Amp => {
                if kind_name == "result" {
                    if let Some(text) = event.get("result").and_then(Value::as_str) {
                        result.text = text.into();
                        recognized = true;
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
                        result.text.push_str(text);
                        result.text.push('\n');
                        recognized = true;
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
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
        let example = FORMAT.lines().nth(1).unwrap();
        assert!(!example.contains("not_applicable"));
        let plain = parse(example, Role::Test).unwrap();
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
}
