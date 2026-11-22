use super::*;

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
/// upstream handoff.
pub(super) fn prompt_for(
    run: &Run,
    target: PromptTarget<'_>,
    handoff_path: &Path,
    schema_path: &Path,
    collect_views: bool,
) -> (String, Vec<HandoffView>) {
    let stage = target.stage;
    let mut text = format!(
        "You are completing stage {:?} (role: {}) in a multi-stage workflow.\n\nORIGINAL USER TASK\n{}\n\nSTAGE INSTRUCTIONS\n{}\n\nWORKING DIRECTORY\n{}\n\nFollow the project's own instructions. Do not commit, merge, or push unless the original task explicitly requests it. Do not change router configuration, run state, or other stages' artifacts.\n",
        stage.name,
        stage.role.label(),
        run.task,
        stage.instructions,
        target.workspace.display()
    );
    let mut views = vec![];
    let mut remaining = PRIOR_BUDGET;
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
        let budget = remaining.min(PRIOR_EACH);
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
    (text, views)
}

/// Bytes of prior handoffs one prompt carries in total, and per handoff.
pub const PRIOR_BUDGET: usize = 14000;
pub const PRIOR_EACH: usize = 5000;

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
    let (prompt, handoffs) = prompt_for(
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
        out.push_str(&format!(
            "Budget: {PRIOR_BUDGET} bytes of prior handoffs, at most {PRIOR_EACH} each, nearest stage first. Full handoffs stay in each attempt's handoff.json.\n"
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
