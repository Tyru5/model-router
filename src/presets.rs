//! Editable starting points; presets never change saved workflows automatically.
use crate::config::{Candidate, FanOut, Mode, Role, Stage, Workflow};

pub struct StagePreset {
    pub key: &'static str,
    pub name: &'static str,
    pub purpose: &'static str,
    pub role: Role,
    pub instructions: &'static str,
}

impl StagePreset {
    pub fn build(&self) -> Stage {
        if self.role == Role::Ship {
            // The router ships; harness and model are unused.
            return Stage {
                id: self.key.into(),
                name: self.name.into(),
                role: self.role,
                model: String::new(),
                ship: Some(crate::ship::Options {
                    push: true,
                    draft_pr: true,
                    ..crate::ship::Options::default()
                }),
                ..Stage::default()
            };
        }
        let edits = matches!(self.key, "implement" | "document");
        Stage {
            id: self.key.into(),
            name: self.name.into(),
            role: self.role,
            harness: if edits { "claude" } else { "codex" }.into(),
            model: if self.key == "custom" {
                ""
            } else if edits {
                "sonnet"
            } else {
                "gpt-6-astra"
            }
            .into(),
            mode: if edits {
                Mode::Interactive
            } else {
                Mode::Unattended
            },
            instructions: self.instructions.into(),
            // Summaries read the work; Codex can enforce that.
            harness_settings: if self.role == Role::Summary {
                [("sandbox".to_owned(), "read-only".to_owned())].into()
            } else {
                Default::default()
            },
            ..Stage::default()
        }
    }
}

/// The stage preset named `key`.
pub fn stage(key: &str) -> &'static StagePreset {
    STAGES
        .iter()
        .find(|p| p.key == key)
        .expect("Stage preset exists")
}

pub const STAGES: &[StagePreset] = &[
    StagePreset {
        key: "clarify",
        name: "Clarify requirements",
        role: Role::Planning,
        purpose: "Define scope, acceptance criteria, assumptions, and open questions.",
        instructions: "Turn the original task/spec into explicit requirements, acceptance criteria, constraints, and non-goals. Inspect relevant project context. Separate facts from assumptions and identify questions that materially affect the work. If an essential decision cannot be inferred, return blocked with the exact question; otherwise record reasonable assumptions for later stages. Do not modify project files.",
    },
    StagePreset {
        key: "research",
        name: "Research",
        role: Role::Research,
        purpose: "Gather evidence from the project and relevant primary sources.",
        instructions: "Investigate the task and relevant project files. Explain existing behavior, dependencies, and constraints with file references or primary-source citations. Identify the smallest useful next step and unresolved risks. Distinguish evidence from assumptions. Do not modify project files.",
    },
    StagePreset {
        key: "diagnose",
        name: "Diagnose bug",
        role: Role::Research,
        purpose: "Reproduce a failure and identify its cause before changing code.",
        instructions: "Reproduce the reported failure using available tests, logs, and project context. Trace the cause and affected behavior; distinguish confirmed causes from hypotheses. Record reproduction steps, evidence, a proposed minimal fix, and a regression check. If reproduction is unavailable, explain the limitation. Do not modify project code or run destructive reproduction steps.",
    },
    StagePreset {
        key: "design",
        name: "Design solution",
        role: Role::Planning,
        purpose: "Choose the architecture, interfaces, and behavior for the change.",
        instructions: "Use the task and prior findings to design the smallest solution that fits this project. Describe affected components, interfaces, data flow, user-facing behavior, failure cases, and compatibility or migration needs. Compare alternatives only when they materially affect the decision. Record the chosen design and tradeoffs for implementation. Do not modify project files.",
    },
    StagePreset {
        key: "plan",
        name: "Plan implementation",
        role: Role::Planning,
        purpose: "Break the chosen approach into concrete changes and checks.",
        instructions: "Turn the task, findings, and design into an ordered implementation plan. Name the files or modules to change, dependencies between steps, acceptance checks, and meaningful tests. Keep the plan proportional to the task. Identify unresolved blockers and return blocked when implementation cannot safely proceed without an answer. Do not modify project files.",
    },
    StagePreset {
        key: "implement",
        name: "Implement",
        role: Role::Implementation,
        purpose: "Make the change using the prior findings and plan.",
        instructions: "Implement the task using prior findings and the agreed design or plan. Follow project conventions, preserve unrelated work, and make the smallest complete change. Add or update meaningful tests where needed and run appropriate checks. Report changes, test evidence, deviations from the plan, and remaining issues.",
    },
    StagePreset {
        key: "verify",
        name: "Verify tests",
        role: Role::Test,
        purpose: "Check acceptance criteria and report actual test results.",
        instructions: "Verify the implementation against the original task's acceptance criteria. Run appropriate existing automated checks and inspect relevant behavior; include manual verification where practical. Do not fix code during this stage. Record each check actually attempted in the handoff tests array, with its command or manual procedure, status, and whether it is required. Required failed or unavailable checks must remain visible; return blocked for missing prerequisites or needs_changes for failures. Never describe an unrun check as passed.",
    },
    StagePreset {
        key: "review",
        name: "Code review",
        role: Role::Review,
        purpose: "Find correctness and regression issues; optionally request repairs.",
        instructions: "Review the implementation and verification evidence against the original task. Inspect changed code and affected callers for correctness, regressions, maintainability, and missing coverage. Do not modify project files. Separate blocking findings from advisory suggestions, cite concrete evidence, and return an explicit pass or needs_changes verdict. Blocking findings and failed required checks request implementation repairs.",
    },
    StagePreset {
        key: "judge",
        name: "Judge candidates",
        role: Role::Review,
        purpose: "Compare a fan-out stage's candidates and choose the one to apply.",
        instructions: "Compare the fan-out candidates listed below against the original task. Read each candidate's handoff, router check results, and patch, and inspect its worktree with read-only commands. Weigh correctness, completeness, test evidence, and the size and risk of each change. Do not modify any files. Choose exactly one complete candidate in selected with verdict pass, and explain in summary why it beats the others; return blocked if no candidate is acceptable.",
    },
    StagePreset {
        key: "security",
        name: "Security review",
        role: Role::Review,
        purpose: "Inspect changed trust boundaries, permissions, and sensitive data handling.",
        instructions: "Review security implications of the changes in the context of the original task. Inspect relevant trust boundaries, input validation, authentication and authorization, secret handling, injection risks, and dependencies. Focus on reachable issues with concrete evidence; separate blocking findings from advisory hardening. Do not modify files, access unrelated secrets, or run intrusive probes. Return an explicit pass or needs_changes verdict; blocking findings request implementation repairs.",
    },
    StagePreset {
        key: "document",
        name: "Update documentation",
        role: Role::Custom,
        purpose: "Update usage, examples, and operational notes to match the result.",
        instructions: "Update the documentation needed for the implemented behavior: usage, configuration, examples, and migration or operational notes where relevant. Verify examples against the code and available checks. Keep edits limited to documentation and avoid claiming unverified support. Report changed documents and any gaps requiring follow-up.",
    },
    StagePreset {
        key: "summary",
        name: "Review summary",
        role: Role::Summary,
        purpose: "Brief you on what changed, how it was verified, and what to review.",
        instructions: "Write a review brief for the person who will review this work. Use the original task, the prior stage handoffs and artifacts, and the actual changes in the working directory; inspect them with read-only commands such as git status, git diff --stat, and git diff, and never run commands that change files or Git state. Put the whole brief in the handoff summary as Markdown with these sections: ## Outcome (whether the task was done, partly done, or not, in one or two sentences); ## Changes (each changed file or area: what changed and why); ## Verification (checks actually run and their results as reported by prior stages or the router, and plainly what was not verified); ## Review focus (the specific places to read closely: risky logic, behavior changes, assumptions, deviations from the plan, with file references); ## Open questions (decisions for the reviewer, known limitations, follow-ups); ## Try it (commands or steps to exercise the change, if any). Be concrete and brief and cite files. List changed files in files_changed. Leave findings, tests, and unresolved empty because earlier stages already report them. If the sandbox stops you writing the handoff file, finish with the handoff block in your final message and do not mention it in the brief. Do not modify project files, commit, push, or publish.",
    },
    StagePreset {
        key: "handoff",
        name: "Summarize handoff",
        role: Role::Custom,
        purpose: "Summarize the outcome, evidence, limitations, and next actions.",
        instructions: "Summarize the task outcome using prior stage handoffs and their artifacts. State what was established or changed, decisions, checks actually run, unresolved issues, and practical next steps. Preserve uncertainty and failed or missing checks. Do not modify project files, publish, commit, or deploy anything. Return the summary in the required handoff format.",
    },
    StagePreset {
        key: "ship",
        name: "Ship (commit + draft PR)",
        role: Role::Ship,
        purpose: "The router commits the run's changes on a new branch, pushes, and opens a draft PR, after showing you exactly what it will do.",
        instructions: "",
    },
    StagePreset {
        key: "custom",
        name: "Custom stage",
        role: Role::Custom,
        purpose: "Start with blank instructions and choose your own harness and model.",
        instructions: "",
    },
];

pub struct WorkflowPreset {
    pub key: &'static str,
    pub name: &'static str,
    pub purpose: &'static str,
    pub stages: &'static [&'static str],
    /// Candidates `(harness, model)` for a best-of-N implement stage, judged
    /// by the judge stage.
    pub candidates: &'static [(&'static str, &'static str)],
}

impl WorkflowPreset {
    pub fn build(&self) -> Workflow {
        Workflow {
            name: self.name.into(),
            max_repair_passes: 2,
            sandbox_default: Default::default(),
            budget: Default::default(),
            protected_paths: vec![],
            handoff_repair: false,
            max_parallel: if self.candidates.is_empty() {
                2
            } else {
                self.candidates.len() as u32
            },
            stages: self
                .stages
                .iter()
                .map(|key| {
                    let mut stage = stage(key).build();
                    // Selecting a workflow must never implicitly opt into a
                    // review, or into the router committing; a judge chooses a
                    // candidate rather than gating.
                    let judge = *key == "judge" && !self.candidates.is_empty();
                    stage.enabled = !matches!(stage.role, Role::Review | Role::Ship) || judge;
                    if *key == "implement" && !self.candidates.is_empty() {
                        // Candidates run side by side, so unattended.
                        stage.mode = Mode::Unattended;
                        stage.fan_out = Some(FanOut {
                            candidates: self
                                .candidates
                                .iter()
                                .map(|(harness, model)| Candidate {
                                    harness: (*harness).into(),
                                    model: (*model).into(),
                                    ..Candidate::default()
                                })
                                .collect(),
                            judge: "judge".into(),
                        });
                    }
                    stage
                })
                .collect(),
        }
    }
}

/// Adapts a starter or preset workflow to the harnesses installed on this
/// machine (`init --detect`), clearing hardcoded model IDs so each stage
/// inherits its harness's native default instead of failing on account-specific
/// names. A stage whose harness is not installed moves to the first installed
/// one. When two or more harnesses are installed, a review stage moves off the
/// harness that implements (its repair target's, else the first
/// implementation stage's), so the review is independent. A stage that was
/// read-only stays read-only on its new harness (`switch_harness`).
pub fn apply_detected_harnesses(workflow: &mut Workflow, available: &[&str]) {
    let primary = available.first().copied();
    for stage in &mut workflow.stages {
        if stage.role == Role::Ship {
            continue;
        }
        stage.model.clear();
        stage.effort.clear();
        if let Some(first) = primary {
            if !available.contains(&stage.harness.as_str()) {
                switch_harness(stage, first);
            }
        }
        if let Some(fan) = &mut stage.fan_out {
            for (index, candidate) in fan.candidates.iter_mut().enumerate() {
                candidate.model.clear();
                candidate.effort.clear();
                if !available.is_empty() && !available.contains(&candidate.harness.as_str()) {
                    candidate.harness = available[index % available.len()].to_owned();
                }
            }
        }
    }
    // Reviews last: every implementation stage has its final harness now.
    let implementer = |target: &str, stages: &[Stage]| -> Option<String> {
        let implementation = |s: &&Stage| s.role == Role::Implementation;
        stages
            .iter()
            .filter(implementation)
            .find(|s| s.id == target)
            .or_else(|| stages.iter().find(implementation))
            .map(|s| s.harness.clone())
    };
    for index in 0..workflow.stages.len() {
        if workflow.stages[index].role != Role::Review {
            continue;
        }
        let Some(implementer) =
            implementer(&workflow.stages[index].repair_target, &workflow.stages)
        else {
            continue;
        };
        let stage = &mut workflow.stages[index];
        if stage.harness != implementer {
            continue;
        }
        if let Some(other) = available.iter().find(|name| **name != implementer) {
            switch_harness(stage, other);
        }
    }
}

/// Moves `stage` to `harness`. Native settings belong to the old harness
/// and are dropped, except a read-only one (`HarnessSpec::read_only`): it
/// becomes the new harness's read-only setting, or, when that harness has
/// none, the router's own `sandbox = "read-only"`.
fn switch_harness(stage: &mut Stage, harness: &str) {
    let spec = crate::harness::get;
    let read_only = spec(&stage.harness)
        .and_then(|old| old.read_only)
        .is_some_and(|(key, choice)| {
            stage.harness_settings.get(key).map(String::as_str) == Some(choice)
        });
    stage.harness = harness.to_owned();
    stage.harness_settings.clear();
    if read_only {
        match spec(harness).and_then(|new| new.read_only) {
            Some((key, choice)) => {
                stage.harness_settings.insert(key.into(), choice.into());
            }
            None => stage.sandbox = Some(crate::config::Sandbox::ReadOnly),
        }
    }
}

pub const WORKFLOWS: &[WorkflowPreset] = &[
    WorkflowPreset {
        key: "quick-task",
        name: "Quick task",
        purpose: "Implement a clear task, verify it, optionally review, and summarize for you.",
        stages: &["implement", "verify", "review", "summary", "ship"],
        candidates: &[],
    },
    WorkflowPreset {
        key: "full-feature",
        name: "Full feature",
        purpose: "Clarify, research, design, plan, implement, verify, document, and summarize for you.",
        stages: &[
            "clarify",
            "research",
            "design",
            "plan",
            "implement",
            "verify",
            "review",
            "security",
            "document",
            "summary",
            "ship",
        ],
        candidates: &[],
    },
    WorkflowPreset {
        key: "bug-fix",
        name: "Bug fix",
        purpose: "Diagnose the cause, implement a fix, verify the regression, and summarize for you.",
        stages: &[
            "diagnose",
            "implement",
            "verify",
            "review",
            "summary",
            "ship",
        ],
        candidates: &[],
    },
    WorkflowPreset {
        key: "investigation",
        name: "Investigation",
        purpose: "Research a question and summarize findings without implementation.",
        stages: &["research", "summary"],
        candidates: &[],
    },
    WorkflowPreset {
        key: "best-of-3",
        name: "Best-of-3 implement",
        purpose: "Research, implement with three harnesses side by side, let a judge pick one, verify, and summarize.",
        stages: &["research", "implement", "judge", "verify", "summary"],
        candidates: &[
            ("claude", "sonnet"),
            ("codex", "gpt-6-astra"),
            ("opencode", ""),
        ],
    },
];

pub fn listing() -> String {
    let mut text = String::from("STAGE PRESETS\n");
    for preset in STAGES {
        text.push_str(&format!(
            "  {} ({}) — {}\n    {}\n",
            preset.key,
            preset.role.label(),
            preset.name,
            preset.purpose
        ));
    }
    text.push_str(
        "\nWORKFLOW PRESETS (reviews start disabled, as does ship; fan-out judges start enabled)\n",
    );
    for preset in WORKFLOWS {
        text.push_str(&format!(
            "  {} — {}\n    {}\n",
            preset.key, preset.name, preset.purpose
        ));
    }
    text.push_str("\nTUI: a adds a stage; w adds a workflow preset. All stages are editable.\nCLI: model-router init --preset <workflow-key>\n");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use std::collections::HashSet;

    #[test]
    fn presets_validate_roundtrip_and_keep_reviews_optional() {
        let mut keys = HashSet::new();
        for preset in STAGES {
            assert!(keys.insert(preset.key));
            let settings = Settings {
                workflows: vec![Workflow {
                    max_repair_passes: 0,
                    ..Workflow::new("Preset", vec![preset.build()])
                }],
                ..Settings::default()
            };
            settings.validate().unwrap();
        }
        let mut keys = HashSet::new();
        for preset in WORKFLOWS {
            assert!(keys.insert(preset.key));
            let settings = Settings {
                workflows: vec![preset.build()],
                ..Settings::default()
            };
            settings.validate().unwrap();
            let loaded: Settings =
                toml::from_str(&toml::to_string_pretty(&settings).unwrap()).unwrap();
            loaded.validate().unwrap();
            let judges: Vec<&str> = loaded.workflows[0]
                .stages
                .iter()
                .filter_map(|s| s.fan_out.as_ref().map(|f| f.judge.as_str()))
                .collect();
            assert!(
                loaded.workflows[0]
                    .stages
                    .iter()
                    .filter(|s| {
                        matches!(s.role, Role::Review | Role::Ship)
                            && !judges.contains(&s.id.as_str())
                    })
                    .all(|s| !s.enabled)
            );
            assert!(
                loaded.workflows[0]
                    .stages
                    .iter()
                    .filter(|s| judges.contains(&s.id.as_str()))
                    .all(|s| s.enabled)
            );
            assert_eq!(loaded.workflows[0].stages.len(), preset.stages.len());
        }
    }

    #[test]
    fn best_of_three_fans_out_implementation_to_a_judge() {
        let workflow = WORKFLOWS
            .iter()
            .find(|p| p.key == "best-of-3")
            .unwrap()
            .build();
        let ids: Vec<&str> = workflow.stages.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["research", "implement", "judge", "verify", "summary"]);
        let implement = &workflow.stages[1];
        assert_eq!(implement.mode, Mode::Unattended);
        let fan = implement.fan_out.as_ref().unwrap();
        assert_eq!(fan.judge, "judge");
        let harnesses: Vec<(&str, &str)> = fan
            .candidates
            .iter()
            .map(|c| (c.harness.as_str(), c.model.as_str()))
            .collect();
        assert_eq!(
            harnesses,
            [
                ("claude", "sonnet"),
                ("codex", "gpt-6-astra"),
                ("opencode", "")
            ]
        );
        assert!(workflow.stages[2].enabled && workflow.stages[2].role == Role::Review);
        assert_eq!(crate::graph::judge_of(&workflow, 1), Some(2));
    }

    #[test]
    fn work_ends_with_an_enabled_read_only_review_summary() {
        let summary = stage("summary").build();
        assert_eq!(summary.role, Role::Summary);
        assert_eq!(
            (summary.harness.as_str(), summary.mode),
            ("codex", Mode::Unattended)
        );
        assert_eq!(summary.harness_settings["sandbox"], "read-only");
        for workflow in [Workflow::default()]
            .into_iter()
            .chain(WORKFLOWS.iter().map(WorkflowPreset::build))
        {
            let last = workflow
                .stages
                .iter()
                .rfind(|s| s.role != Role::Ship)
                .unwrap();
            assert!(
                last.role == Role::Summary && last.enabled,
                "{}",
                workflow.name
            );
        }
    }
}
