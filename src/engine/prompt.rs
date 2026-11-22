use super::*;
use std::collections::HashSet;
use std::io::Read;

pub(super) struct PromptTarget<'a> {
    pub(super) cursor: usize,
    pub(super) stage: &'a Stage,
    pub(super) workspace: &'a Path,
    pub(super) view: Option<usize>,
}

pub fn prompt(run: &Run, handoff_path: &Path, schema_path: &Path) -> String {
    let Some(stage) = run.workflow.stages.get(run.cursor) else {
        return String::new();
    };
    prompt_for(
        run,
        PromptTarget {
            cursor: run.cursor,
            stage,
            workspace: &run.workspace,
            view: run.view,
        },
        handoff_path,
        schema_path,
        false,
    )
    .0
}

/// The prompt, and when `collect_views` is true, what it carried of each
/// upstream handoff and each configured context file.
pub(super) fn prompt_for(
    run: &Run,
    target: PromptTarget<'_>,
    handoff_path: &Path,
    schema_path: &Path,
    collect_views: bool,
) -> (String, Vec<HandoffView>, Vec<ContextFileView>) {
    let stage = target.stage;
    let mut text = format!(
        "You are completing stage {:?} (role: {}) in a multi-stage workflow.\n\nORIGINAL USER TASK\n{}\n\nSTAGE INSTRUCTIONS\n{}\n\nWORKING DIRECTORY\n{}\n\nFollow the project's own instructions. Do not commit, merge, or push unless the original task explicitly requests it. Do not change router configuration, run state, or other stages' artifacts.\n",
        stage.name,
        stage.role.label(),
        run.task,
        stage.instructions,
        target.workspace.display()
    );
    let context = resolve_context_files(
        target.workspace,
        &run.workflow.context_files,
        &stage.context_files,
    );
    if !context.entries.is_empty() {
        text.push_str("\nPROJECT CONTEXT FILES\n");
        for entry in &context.entries {
            if entry.truncated {
                text.push_str(&format!(
                    "\n--- {} ({} bytes, truncated) ---\n{}\n[truncated; full file in workspace at {}]\n",
                    entry.path,
                    entry.content.len(),
                    entry.content,
                    entry.path
                ));
            } else {
                text.push_str(&format!("\n--- {} ---\n{}", entry.path, entry.content));
                if !entry.content.ends_with('\n') {
                    text.push('\n');
                }
            }
        }
    }
    let context_views = if collect_views { context.views } else { vec![] };
    let mut views = vec![];
    let total_budget = run.workflow.effective_handoff_budget_bytes();
    let each_budget = run.workflow.effective_handoff_each_bytes();
    let mut remaining = total_budget;
    // Stages this one runs after; in a list, every earlier stage.
    let order = run.order();
    for (index, state) in run
        .stages
        .iter()
        .enumerate()
        .rev()
        .filter(|(index, _)| order.precedes(*index, target.cursor))
    {
        let name = run.workflow.stages[index].name.clone();
        let handoff = match (&state.handoff, state.status) {
            (Some(handoff), Status::Complete) => handoff,
            (_, status) => {
                if collect_views {
                    views.push(HandoffView::missing(
                        name,
                        if status == Status::Complete {
                            "complete without a handoff".into()
                        } else {
                            format!("not complete ({})", status.label())
                        },
                    ));
                }
                continue;
            }
        };
        let directory = state
            .attempts
            .last()
            .map(|a| a.directory.clone())
            .or_else(|| state.inherited.clone())
            .unwrap_or_default();
        let full = directory.join("handoff.json");
        let budget = remaining.min(each_budget);
        if budget == 0 {
            if collect_views {
                views.push(HandoffView::missing(
                    name.clone(),
                    "prior handoff budget exhausted".into(),
                ));
            }
            text.push_str(&format!(
                "\nPRIOR STAGE: {}\n[omitted: prior handoff budget exhausted; full artifact at {}]\nFull artifacts: {}\n",
                name,
                full.display(),
                directory.display()
            ));
            continue;
        }
        let (excerpt, sent) = handoff_excerpt(handoff, budget, &full);
        remaining = remaining.saturating_sub(excerpt.len());
        if collect_views {
            views.push(HandoffView::compare(
                name.clone(),
                handoff,
                &sent,
                budget,
                excerpt.len(),
            ));
        }
        text.push_str(&format!(
            "\nPRIOR STAGE: {}\n{}\nFull artifacts: {}\n",
            name,
            excerpt,
            directory.display()
        ));
    }
    // Only the repaired stages and the review that asked see its request.
    let feedback = run.repair_feedback.as_ref().filter(|_| {
        run.repair_review
            .is_none_or(|review| target.cursor == review || order.precedes(target.cursor, review))
    });
    if let Some(feedback) = feedback {
        text.push_str(&format!(
            "\nREPAIR REQUEST\n{}\n",
            storage::truncate(feedback, 4000)
        ));
    }
    if stage.role == Role::Review {
        text.push_str(&policy_advisories(run));
    }
    if !stage.checks.is_empty() {
        text.push_str("\nROUTER CHECKS\nAfter you finish, the router runs these commands in the working directory and records their results independently of your handoff:\n");
        for check in &stage.checks {
            text.push_str(&format!(
                "- {} ({})\n",
                shell_words::join(&check.command),
                if check.required {
                    "required"
                } else {
                    "optional"
                }
            ));
        }
    }
    text.push_str(&fan_out_context(run, target.cursor, target.view));
    if let Some(delegation) = &stage.delegation {
        text.push_str(&crate::delegation::instructions::text(
            super::delegate::parent_kind(run, stage),
            delegation.runs,
            delegation.schedules,
        ));
    }
    text.push_str(&handoff::contract(stage.role, handoff_path, schema_path));
    (text, views, context_views)
}

/// Advisory diff policies matching files changed so far in `run`, formatted for
/// review stage prompts.
fn policy_advisories(run: &Run) -> String {
    use crate::policy::{PolicyAction, PolicyScan, advisory_matches};
    let has_advise = run
        .workflow
        .policies
        .iter()
        .any(|p| p.action == PolicyAction::Advise);
    if !has_advise {
        return String::new();
    }
    let needs_diff = run
        .workflow
        .policies
        .iter()
        .any(|p| p.action == PolicyAction::Advise && !p.pattern.is_empty());
    let mut changed_files = vec![];
    let mut diff_hits = vec![];
    if let Some(from) = run.baseline.as_deref() {
        if snapshot::is_repository(&run.workspace) {
            let to = snapshot::current_tree(&run.workspace)
                .ok()
                .flatten()
                .or_else(|| run.change_range().ok().map(|(_, t)| t.to_owned()))
                .unwrap_or_else(|| from.to_owned());
            if from != to {
                changed_files =
                    snapshot::changed_paths(&run.workspace, from, &to).unwrap_or_default();
                if needs_diff {
                    let mut scan =
                        PolicyScan::with_action(&run.workflow.policies, PolicyAction::Advise);
                    if snapshot::patch_lines(&run.workspace, from, &to, |line| scan.line(line))
                        .is_ok()
                    {
                        diff_hits = scan.hits().to_vec();
                    }
                }
            }
        }
    } else {
        for state in &run.stages {
            if state.status == Status::Complete {
                if let Some(handoff) = &state.handoff {
                    changed_files.extend(handoff.files_changed.iter().cloned());
                }
            }
        }
    }
    let matches = advisory_matches(&run.workflow.policies, &changed_files, &diff_hits);
    if matches.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\nPOLICY ADVISORIES\nThe following workflow policy advisories apply to changed files in this run:\n",
    );
    for hit in matches {
        out.push_str(&format!(
            "- [{}] {} (files: {})\n",
            hit.name,
            hit.message,
            hit.files.join(", ")
        ));
    }
    out
}

/// Bytes of prior handoffs one prompt carries in total, and per handoff.
pub const PRIOR_BUDGET: usize = crate::config::DEFAULT_HANDOFF_BUDGET_BYTES;
pub const PRIOR_EACH: usize = crate::config::DEFAULT_HANDOFF_EACH_BYTES;
/// Maximum context files and total context bytes injected into one prompt.
pub const MAX_CONTEXT_FILES: usize = crate::config::MAX_CONTEXT_FILES;
pub const MAX_CONTEXT_BYTES: usize = crate::config::MAX_CONTEXT_BYTES;

const MAX_WALK_DEPTH: usize = 32;
const MAX_WALK_ENTRIES: usize = 50_000;

/// One resolved or skipped context file for a stage prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextFileView {
    pub path: String,
    pub included: bool,
    pub bytes: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

struct ContextEntry {
    path: String,
    content: String,
    truncated: bool,
}

struct ResolvedContext {
    entries: Vec<ContextEntry>,
    views: Vec<ContextFileView>,
}

fn is_forbidden_component(name: &str) -> bool {
    matches!(name, "" | "." | ".." | ".git" | ".model-router")
}

fn classify_open_failure(workspace: &Path, rel: &str) -> &'static str {
    let mut cur = workspace.to_path_buf();
    let mut saw_missing = false;
    for part in rel.split('/').filter(|s| !s.is_empty()) {
        cur.push(part);
        match fs::symlink_metadata(&cur) {
            Ok(meta) if meta.file_type().is_symlink() => return "symlink not allowed",
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                saw_missing = true;
                break;
            }
            Err(_) => return "unreadable",
        }
    }
    if saw_missing {
        return "not found";
    }
    match fs::symlink_metadata(workspace.join(rel)) {
        Ok(meta) if !meta.file_type().is_file() => "not a regular file",
        _ => "unreadable",
    }
}

fn glob_workspace(workspace: &Path, pattern: &str) -> Vec<String> {
    let segments: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return vec![];
    }
    let has_doublestar = segments.contains(&"**");
    let max_depth = if has_doublestar {
        MAX_WALK_DEPTH
    } else {
        segments.len().saturating_sub(1).min(MAX_WALK_DEPTH)
    };
    let prefix_len = segments
        .iter()
        .take(segments.len().saturating_sub(1))
        .take_while(|seg| !seg.contains('*') && !seg.contains('?'))
        .count();
    let mut start_dir = workspace.to_path_buf();
    let mut start_rel = String::new();
    for seg in &segments[..prefix_len] {
        if is_forbidden_component(seg) {
            return vec![];
        }
        start_dir.push(seg);
        let Ok(meta) = fs::symlink_metadata(&start_dir) else {
            return vec![];
        };
        if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
            return vec![];
        }
        if !start_rel.is_empty() {
            start_rel.push('/');
        }
        start_rel.push_str(seg);
    }

    let mut matched = vec![];
    let mut stack = vec![(start_dir, start_rel, prefix_len)];
    let mut visited = 0usize;
    while let Some((dir, rel_prefix, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_WALK_ENTRIES {
                return matched;
            }
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            let os_name = entry.file_name();
            let Some(name) = os_name.to_str() else {
                continue;
            };
            if is_forbidden_component(name) {
                continue;
            }
            let rel = if rel_prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{rel_prefix}/{name}")
            };
            if ft.is_dir() {
                if depth < max_depth {
                    stack.push((entry.path(), rel, depth + 1));
                }
            } else if ft.is_file() && crate::policy::matches_glob(pattern, &rel) {
                matched.push(rel);
            }
        }
    }
    matched
}

fn resolve_context_files(
    workspace: &Path,
    workflow_patterns: &[String],
    stage_patterns: &[String],
) -> ResolvedContext {
    if workflow_patterns.is_empty() && stage_patterns.is_empty() {
        return ResolvedContext {
            entries: vec![],
            views: vec![],
        };
    }
    let mut entries = vec![];
    let mut views = vec![];
    let mut seen = HashSet::new();
    let mut total_bytes = 0usize;
    let mut omitted_for_files = 0usize;
    let mut omitted_for_bytes = 0usize;

    for raw_pattern in workflow_patterns.iter().chain(stage_patterns.iter()) {
        let pattern = raw_pattern.trim();
        if !crate::config::is_valid_context_pattern(pattern) {
            views.push(ContextFileView {
                path: pattern.to_owned(),
                included: false,
                bytes: 0,
                truncated: false,
                reason: Some("outside workspace or forbidden path".into()),
            });
            continue;
        }
        let is_glob = pattern.contains('*') || pattern.contains('?');
        let candidates = if is_glob {
            let mut matched = glob_workspace(workspace, pattern);
            matched.sort();
            if matched.is_empty() {
                views.push(ContextFileView {
                    path: pattern.to_owned(),
                    included: false,
                    bytes: 0,
                    truncated: false,
                    reason: Some("no matching files".into()),
                });
                continue;
            }
            matched
        } else {
            let normalized = pattern
                .split('/')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("/");
            vec![normalized]
        };

        for rel in candidates {
            if !seen.insert(rel.clone()) {
                continue;
            }
            if rel.split('/').any(is_forbidden_component) {
                views.push(ContextFileView {
                    path: rel,
                    included: false,
                    bytes: 0,
                    truncated: false,
                    reason: Some("outside workspace or forbidden path".into()),
                });
                continue;
            }
            if entries.len() >= MAX_CONTEXT_FILES {
                omitted_for_files += 1;
                continue;
            }
            let remaining = MAX_CONTEXT_BYTES.saturating_sub(total_bytes);
            if remaining == 0 {
                omitted_for_bytes += 1;
                continue;
            }
            let file = match storage::open_beneath(workspace, Path::new(&rel)) {
                Ok(file) => file,
                Err(_) => {
                    let reason = classify_open_failure(workspace, &rel);
                    views.push(ContextFileView {
                        path: rel,
                        included: false,
                        bytes: 0,
                        truncated: false,
                        reason: Some(reason.into()),
                    });
                    continue;
                }
            };
            let mut buf = Vec::new();
            if file
                .take((remaining + 4) as u64)
                .read_to_end(&mut buf)
                .is_err()
            {
                views.push(ContextFileView {
                    path: rel,
                    included: false,
                    bytes: 0,
                    truncated: false,
                    reason: Some("unreadable".into()),
                });
                continue;
            }
            if buf.contains(&0) {
                views.push(ContextFileView {
                    path: rel,
                    included: false,
                    bytes: 0,
                    truncated: false,
                    reason: Some("non-UTF-8 or binary file".into()),
                });
                continue;
            }
            let (content, truncated) = if buf.len() <= remaining {
                match String::from_utf8(buf) {
                    Ok(text) => (text, false),
                    Err(_) => {
                        views.push(ContextFileView {
                            path: rel,
                            included: false,
                            bytes: 0,
                            truncated: false,
                            reason: Some("non-UTF-8 or binary file".into()),
                        });
                        continue;
                    }
                }
            } else {
                let valid = match std::str::from_utf8(&buf) {
                    Ok(s) => Some(s),
                    Err(err)
                        if err.error_len().is_none()
                            && err.valid_up_to() >= buf.len().saturating_sub(3) =>
                    {
                        std::str::from_utf8(&buf[..err.valid_up_to()]).ok()
                    }
                    Err(_) => None,
                };
                let Some(valid) = valid else {
                    views.push(ContextFileView {
                        path: rel,
                        included: false,
                        bytes: 0,
                        truncated: false,
                        reason: Some("non-UTF-8 or binary file".into()),
                    });
                    continue;
                };
                let mut end = remaining.min(valid.len());
                while end > 0 && !valid.is_char_boundary(end) {
                    end -= 1;
                }
                if end == 0 {
                    omitted_for_bytes += 1;
                    continue;
                }
                (valid[..end].to_owned(), true)
            };
            let bytes = content.len();
            total_bytes += bytes;
            views.push(ContextFileView {
                path: rel.clone(),
                included: true,
                bytes,
                truncated,
                reason: truncated.then(|| "context byte limit reached".into()),
            });
            entries.push(ContextEntry {
                path: rel,
                content,
                truncated,
            });
        }
    }

    if omitted_for_files > 0 {
        views.push(ContextFileView {
            path: format!("({omitted_for_files} more)"),
            included: false,
            bytes: 0,
            truncated: false,
            reason: Some(format!("file limit reached ({MAX_CONTEXT_FILES})")),
        });
    }
    if omitted_for_bytes > 0 {
        views.push(ContextFileView {
            path: format!("({omitted_for_bytes} more)"),
            included: false,
            bytes: 0,
            truncated: false,
            reason: Some(format!("byte limit reached ({MAX_CONTEXT_BYTES} bytes)")),
        });
    }

    ResolvedContext { entries, views }
}

/// One-line summary of resolved context files for `model-router preview`;
/// None when neither the workflow nor stage configures `context_files`.
pub(super) fn preview_context_files(
    workspace: &Path,
    workflow: &Workflow,
    stage: &Stage,
) -> Option<String> {
    if workflow.context_files.is_empty() && stage.context_files.is_empty() {
        return None;
    }
    let resolved = resolve_context_files(workspace, &workflow.context_files, &stage.context_files);
    let mut included = vec![];
    let mut skipped = vec![];
    for view in &resolved.views {
        if view.included {
            included.push(if view.truncated {
                format!("{} ({}B, truncated)", view.path, view.bytes)
            } else {
                format!("{} ({}B)", view.path, view.bytes)
            });
        } else {
            skipped.push(format!(
                "{} ({})",
                view.path,
                view.reason.as_deref().unwrap_or("skipped")
            ));
        }
    }
    let mut line = String::from("context files: ");
    if included.is_empty() {
        line.push_str("none");
    } else {
        line.push_str(&included.join(", "));
    }
    if !skipped.is_empty() {
        line.push_str(&format!("; skipped: {}", skipped.join(", ")));
    }
    Some(line)
}

/// How much of one handoff field reached the downstream prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kept {
    Whole,
    /// Strings cut or list items dropped.
    Shortened,
    /// Replaced by "omitted".
    Omitted,
}

#[derive(Debug, Clone, Serialize)]
pub struct FieldView {
    pub field: String,
    pub kept: Kept,
    /// List items in the handoff and in the prompt; None for non-lists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<(usize, usize)>,
}

/// What a downstream stage receives of one upstream handoff.
#[derive(Debug, Clone, Serialize)]
pub struct HandoffView {
    pub stage: String,
    /// False when the stage contributed no handoff; see `reason`.
    pub sent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Bytes allowed for this handoff and bytes sent.
    pub budget: usize,
    pub bytes: usize,
    /// Fields that differ from the saved handoff; whole ones are left out.
    pub fields: Vec<FieldView>,
    /// Lost decisions, unresolved items, or findings.
    pub warnings: Vec<String>,
}
impl HandoffView {
    pub(super) fn missing(stage: String, reason: String) -> Self {
        Self {
            stage,
            sent: false,
            reason: Some(reason),
            budget: 0,
            bytes: 0,
            fields: vec![],
            warnings: vec![],
        }
    }
    pub(super) fn compare(
        stage: String,
        handoff: &Handoff,
        sent: &serde_json::Value,
        budget: usize,
        bytes: usize,
    ) -> Self {
        use serde_json::Value;
        let original = serde_json::to_value(handoff).unwrap_or(Value::Null);
        let mut fields = vec![];
        let mut warnings = vec![];
        for (key, value) in original.as_object().into_iter().flatten() {
            let after = &sent[key.as_str()];
            if after == value {
                continue;
            }
            let omitted = after.as_str() == Some("omitted") && value.as_str() != Some("omitted");
            let kept = if omitted {
                Kept::Omitted
            } else {
                Kept::Shortened
            };
            let items = value.as_array().map(|list| {
                // `shorten` appends "… N more" after the items it kept.
                let after = after.as_array().map_or(&[][..], Vec::as_slice);
                let dropped = after
                    .last()
                    .and_then(Value::as_str)
                    .and_then(|t| t.strip_prefix("… ")?.strip_suffix(" more")?.parse().ok())
                    .filter(|n: &usize| after.len() - 1 + n == list.len());
                let shown = if dropped.is_some() {
                    after.len() - 1
                } else {
                    after.len()
                };
                (list.len(), shown)
            });
            let lost = match (kept, items) {
                (Kept::Omitted, Some((n, _))) => format!("all {n} omitted"),
                (_, Some((n, shown))) if shown < n => format!("{} of {n} dropped", n - shown),
                _ => "text shortened".into(),
            };
            match key.as_str() {
                "decisions" => warnings.push(format!("decisions lost: {lost}")),
                "unresolved" => warnings.push(format!("unresolved items lost: {lost}")),
                "findings" => {
                    let blocking = handoff
                        .findings
                        .iter()
                        .filter(|f| f.severity == handoff::Severity::Blocking)
                        .count();
                    warnings.push(format!(
                        "findings lost: {lost}{}",
                        if blocking > 0 {
                            format!(" ({blocking} blocking in the handoff)")
                        } else {
                            String::new()
                        }
                    ));
                }
                _ => {}
            }
            fields.push(FieldView {
                field: key.clone(),
                kept,
                items,
            });
        }
        Self {
            stage,
            sent: true,
            reason: None,
            budget,
            bytes,
            fields,
            warnings,
        }
    }
}

/// What stage `index` would receive if it started now: its exact prompt and
/// what each upstream handoff lost on the way. Reads only; no model call.
#[derive(Debug, Clone, Serialize)]
pub struct Inspection {
    pub stage: String,
    pub handoffs: Vec<HandoffView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_files: Vec<ContextFileView>,
    pub budget_bytes: usize,
    pub each_bytes: usize,
    pub prompt: String,
}

pub fn inspect_handoffs(run: &Run, stage_id: Option<&str>) -> Result<Inspection> {
    let index = match stage_id {
        Some(id) => run
            .workflow
            .stages
            .iter()
            .position(|s| s.id == id)
            .with_context(|| format!("Run {} has no stage {id}", run.id))?,
        None => {
            ensure!(
                run.cursor < run.stages.len(),
                "This run is complete; name a stage with --stage"
            );
            run.cursor
        }
    };
    let stage = &run.workflow.stages[index];
    // The paths the next attempt would get; nothing is created.
    let number = run.stages[index].attempts.len() + 1;
    let handoff = run
        .workspace
        .join(".model-router/handoffs")
        .join(&run.id)
        .join(format!("{}-{number:03}.json", stage.id));
    let schema = run
        .directory
        .join(format!("{:02}-{}", index + 1, stage.id))
        .join(format!("attempt-{number:03}"))
        .join(handoff::SCHEMA_FILE);
    let (prompt, handoffs, context_files) = prompt_for(
        run,
        PromptTarget {
            cursor: index,
            stage,
            workspace: &run.workspace,
            view: None,
        },
        &handoff,
        &schema,
        true,
    );
    Ok(Inspection {
        stage: stage.name.clone(),
        handoffs,
        context_files,
        budget_bytes: run.workflow.effective_handoff_budget_bytes(),
        each_bytes: run.workflow.effective_handoff_each_bytes(),
        prompt,
    })
}

impl Inspection {
    pub fn text(&self) -> String {
        let mut out = format!("Handoffs into {}:\n", self.stage);
        if self.handoffs.is_empty() {
            out.push_str("  none: no upstream stage\n");
        }
        for view in &self.handoffs {
            if !view.sent {
                out.push_str(&format!(
                    "  - {}: not sent ({})\n",
                    view.stage,
                    view.reason.as_deref().unwrap_or("")
                ));
                continue;
            }
            out.push_str(&format!(
                "  {} {}: {} of {} bytes{}\n",
                if view.warnings.is_empty() { "✓" } else { "!" },
                view.stage,
                view.bytes,
                view.budget,
                if view.fields.is_empty() {
                    ", whole"
                } else {
                    ""
                }
            ));
            for field in &view.fields {
                out.push_str(&format!(
                    "      {} {}{}\n",
                    match field.kept {
                        Kept::Whole => "whole",
                        Kept::Shortened => "shortened",
                        Kept::Omitted => "omitted",
                    },
                    field.field,
                    field
                        .items
                        .map(|(n, shown)| format!(" ({shown} of {n} items)"))
                        .unwrap_or_default()
                ));
            }
            for warning in &view.warnings {
                out.push_str(&format!("      warning: {warning}\n"));
            }
        }
        if !self.context_files.is_empty() {
            out.push_str("Context files:\n");
            for file in &self.context_files {
                if file.included {
                    out.push_str(&format!(
                        "  {} {}: {} bytes{}\n",
                        if file.truncated { "!" } else { "✓" },
                        file.path,
                        file.bytes,
                        if file.truncated { ", truncated" } else { "" }
                    ));
                } else {
                    out.push_str(&format!(
                        "  - {}: skipped ({})\n",
                        file.path,
                        file.reason.as_deref().unwrap_or("skipped")
                    ));
                }
            }
        }
        out.push_str(&format!(
            "Budget: {} bytes of prior handoffs, at most {} each, nearest stage first. Full handoffs stay in each attempt's handoff.json.\n",
            self.budget_bytes, self.each_bytes
        ));
        out
    }
}

/// A handoff as pretty JSON of at most about `max` bytes that stays valid
/// JSON: long strings are shortened and long lists cut, then whole fields
/// dropped, least important first, rather than cutting the text mid-object.
/// Also returns the JSON value as sent.
pub(super) fn handoff_excerpt(
    handoff: &Handoff,
    max: usize,
    full: &Path,
) -> (String, serde_json::Value) {
    let Ok(original) = serde_json::to_value(handoff) else {
        return (String::new(), serde_json::Value::Null);
    };
    let render =
        |value: &serde_json::Value| serde_json::to_string_pretty(value).unwrap_or_default();
    let mut json = render(&original);
    if json.len() <= max {
        return (json, original);
    }
    let mut value = serde_json::Value::Null;
    for (chars, items) in [(1000, 40), (400, 20), (160, 10), (60, 5)] {
        // Each pass starts from the original, so "… N more" counts what the
        // list really lost rather than re-cutting an earlier pass's marker.
        value = original.clone();
        shorten(&mut value, chars, items);
        json = render(&value);
        if json.len() <= max {
            break;
        }
    }
    for field in [
        "next_steps",
        "artifacts",
        "files_changed",
        "decisions",
        "tests",
        "unresolved",
        "findings",
    ] {
        if json.len() <= max {
            break;
        }
        if let Some(object) = value.as_object_mut().filter(|o| o.contains_key(field)) {
            object.insert(field.into(), serde_json::Value::String("omitted".into()));
            json = render(&value);
        }
    }
    (
        format!("{json}\n[truncated; full artifact at {}]", full.display()),
        value,
    )
}

/// Cuts every string to `chars` characters and every list to `items` items,
/// noting how many were left out.
pub(super) fn shorten(value: &mut serde_json::Value, chars: usize, items: usize) {
    use serde_json::Value;
    match value {
        Value::String(text) if text.chars().count() > chars => {
            *text = format!("{}…", text.chars().take(chars).collect::<String>());
        }
        Value::Array(list) => {
            if list.len() > items {
                let more = list.len() - items;
                list.truncate(items);
                list.push(Value::String(format!("… {more} more")));
            }
            list.iter_mut().for_each(|item| shorten(item, chars, items));
        }
        Value::Object(map) => map
            .values_mut()
            .for_each(|item| shorten(item, chars, items)),
        _ => {}
    }
}

pub(super) fn fan_out_context(run: &Run, index: usize, view: Option<usize>) -> String {
    if let Some(c) = view {
        let Some(state) = run.stages.get(index).and_then(|s| s.fan_out.as_ref()) else {
            return String::new();
        };
        return format!(
            "\nFAN-OUT CANDIDATE\nYou are candidate {}, one of {} candidates for this stage, each working in its own Git worktree started from the same workspace state. The working directory above is your worktree: change files only there, and do not commit. A judge compares the candidates, and the router applies only the chosen candidate's changes to the main workspace.\n",
            state.candidates[c].id,
            state.candidates.len()
        );
    }
    let Some(fan) = graph::judged_by(&run.workflow, index) else {
        return String::new();
    };
    let Some(state) = run.stages[fan].fan_out.as_ref() else {
        return String::new();
    };
    let mut text = format!(
        "\nFAN-OUT CANDIDATES\nStage {:?} ran {} candidates, each in its own Git worktree from the same starting point. Compare them against the original task and choose one; the router then applies only the chosen candidate's changes to the working directory. Inspect worktrees and patches read-only; do not modify them or the working directory.\n",
        run.workflow.stages[fan].name,
        state.candidates.len()
    );
    for candidate in &state.candidates {
        text.push_str(&format!(
            "\n- {} ({}{}{}): {}",
            candidate.id,
            candidate.harness,
            if candidate.model.is_empty() { "" } else { " " },
            candidate.model,
            candidate.status.label()
        ));
        if let Some(error) = &candidate.error {
            text.push_str(&format!(
                " — {}",
                storage::truncate(error.lines().next().unwrap_or(""), 300)
            ));
        }
        if let Some(attempt) = candidate.attempts.last() {
            if let Some(worktree) = &attempt.worktree {
                text.push_str(&format!("\n  worktree: {}", worktree.display()));
            }
            let required: Vec<_> = attempt.checks.iter().filter(|c| c.required).collect();
            if !required.is_empty() {
                text.push_str(&format!(
                    "\n  router checks: {}/{} required passed",
                    required
                        .iter()
                        .filter(|c| c.status == TestStatus::Passed)
                        .count(),
                    required.len()
                ));
            }
        }
        if let Some(changes) = &candidate.changes {
            text.push_str(&format!(
                "\n  changes: {} files, +{} −{}; patch: {}",
                changes.files.len(),
                changes.added,
                changes.removed,
                changes.patch.display()
            ));
        }
        if let Some(handoff) = &candidate.handoff {
            text.push_str(&format!(
                "\n  handoff: {}",
                storage::truncate(handoff.summary.trim(), 800).replace('\n', "\n    ")
            ));
        }
    }
    text.push_str("\n\nSet \"selected\" in your handoff to the chosen candidate's ID, for example \"selected\":\"c1\", with verdict pass, and explain the choice in summary. Choose only a complete candidate. Use verdict blocked if none is acceptable.\n");
    text
}
