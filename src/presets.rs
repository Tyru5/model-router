//! Editable starting points; presets never change saved workflows automatically.
use crate::config::{Mode, Role, Stage, Workflow};

pub struct StagePreset {
    pub key: &'static str,
    pub name: &'static str,
    pub purpose: &'static str,
    pub role: Role,
    pub instructions: &'static str,
}

impl StagePreset {
    pub fn build(&self) -> Stage {
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
            ..Stage::default()
        }
    }
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
        key: "handoff",
        name: "Summarize handoff",
        role: Role::Custom,
        purpose: "Summarize the outcome, evidence, limitations, and next actions.",
        instructions: "Summarize the task outcome using prior stage handoffs and their artifacts. State what was established or changed, decisions, checks actually run, unresolved issues, and practical next steps. Preserve uncertainty and failed or missing checks. Do not modify project files, publish, commit, or deploy anything. Return the summary in the required handoff format.",
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
}

impl WorkflowPreset {
    pub fn build(&self) -> Workflow {
        Workflow {
            name: self.name.into(),
            max_repair_passes: 2,
            stages: self
                .stages
                .iter()
                .map(|key| {
                    let mut stage = STAGES
                        .iter()
                        .find(|p| p.key == *key)
                        .expect("Workflow preset references an existing stage")
                        .build();
                    // Selecting a workflow must never implicitly opt into a review.
                    stage.enabled = stage.role != Role::Review;
                    stage
                })
                .collect(),
        }
    }
}

pub const WORKFLOWS: &[WorkflowPreset] = &[
    WorkflowPreset {
        key: "quick-task",
        name: "Quick task",
        purpose: "Implement a clear task, verify it, then optionally review.",
        stages: &["implement", "verify", "review"],
    },
    WorkflowPreset {
        key: "full-feature",
        name: "Full feature",
        purpose: "Clarify, research, design, plan, implement, verify, document, and hand off.",
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
            "handoff",
        ],
    },
    WorkflowPreset {
        key: "bug-fix",
        name: "Bug fix",
        purpose: "Diagnose the cause, implement a fix, and verify the regression.",
        stages: &["diagnose", "implement", "verify", "review"],
    },
    WorkflowPreset {
        key: "investigation",
        name: "Investigation",
        purpose: "Research a question and summarize findings without implementation.",
        stages: &["research", "handoff"],
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
    text.push_str("\nWORKFLOW PRESETS (reviews start disabled)\n");
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
                    name: "Preset".into(),
                    max_repair_passes: 0,
                    stages: vec![preset.build()],
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
            assert!(
                loaded.workflows[0]
                    .stages
                    .iter()
                    .filter(|s| s.role == Role::Review)
                    .all(|s| !s.enabled)
            );
            assert_eq!(loaded.workflows[0].stages.len(), preset.stages.len());
        }
    }
}
