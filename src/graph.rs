//! Stage order. A workflow whose stages never set `after` runs in list
//! order, each stage after the previous one. Once any stage sets `after`, the
//! workflow is a dependency graph: a stage runs after the stages it names
//! (none: from the start), and stages whose dependencies are complete may run
//! at once when they cannot conflict in the shared workspace (`parallel_safe`).
//! Disabled stages are transparent: their dependents wait on their own
//! dependencies. A fan-out's dependents also wait for its judge, which
//! applies the chosen candidate.
use crate::config::{Mode, Role, Stage, Workflow};
use anyhow::{Result, bail, ensure};

/// Whether `workflow` is a dependency graph rather than a list.
pub fn is_graph(workflow: &Workflow) -> bool {
    workflow.stages.iter().any(|s| !s.after.is_empty())
}

/// Roles whose instructions leave project files unchanged.
pub fn read_only(role: Role) -> bool {
    matches!(
        role,
        Role::Research | Role::Planning | Role::Review | Role::Summary
    )
}

/// Whether `stage` may run beside other parallel-safe stages: unattended,
/// and either read-only or a fan-out whose candidates each have a worktree.
pub fn parallel_safe(stage: &Stage) -> bool {
    stage.mode == Mode::Unattended && (stage.fan_out.is_some() || read_only(stage.role))
}

/// The enabled judge stage of enabled fan-out stage `index`; None for `auto`,
/// a disabled judge (the fan-out then judges automatically), or no fan-out.
pub fn judge_of(workflow: &Workflow, index: usize) -> Option<usize> {
    let stage = workflow.stages.get(index).filter(|s| s.enabled)?;
    let fan = stage.fan_out.as_ref().filter(|f| !f.is_auto())?;
    workflow
        .stages
        .iter()
        .position(|s| s.id == fan.judge && s.enabled && s.id != stage.id)
}

/// The fan-out stage that stage `index` judges.
pub fn judged_by(workflow: &Workflow, index: usize) -> Option<usize> {
    (0..workflow.stages.len()).find(|&f| judge_of(workflow, f) == Some(index))
}

fn bit(index: usize) -> u64 {
    1u64 << index
}

/// Direct dependencies of each stage as bit masks (at most 64 stages).
fn edges(workflow: &Workflow) -> Result<Vec<u64>> {
    let stages = &workflow.stages;
    ensure!(stages.len() <= 64, "Workflows have at most 64 stages");
    if !is_graph(workflow) {
        return Ok((0..stages.len())
            .map(|i| if i == 0 { 0 } else { bit(i - 1) })
            .collect());
    }
    let mut edges = vec![0u64; stages.len()];
    for (index, stage) in stages.iter().enumerate() {
        for (n, id) in stage.after.iter().enumerate() {
            ensure!(
                id != &stage.id,
                "Stage {} cannot run after itself",
                stage.id
            );
            ensure!(
                !stage.after[..n].contains(id),
                "Stage {} lists {id} twice in after",
                stage.id
            );
            let Some(dep) = stages.iter().position(|s| &s.id == id) else {
                bail!("Stage {} runs after unknown stage {id}", stage.id);
            };
            edges[index] |= bit(dep);
        }
    }
    // The chosen candidate reaches the workspace only when its judge applies it.
    for fan in 0..stages.len() {
        if let Some(judge) = judge_of(workflow, fan) {
            for (index, mask) in edges.iter_mut().enumerate() {
                if index != judge && index != fan && *mask & bit(fan) != 0 {
                    *mask |= bit(judge);
                }
            }
        }
    }
    Ok(edges)
}

/// Every stage each stage transitively runs after, as bit masks. Fails on
/// unknown IDs and cycles.
pub fn ancestors(workflow: &Workflow) -> Result<Vec<u64>> {
    let edges = edges(workflow)?;
    let mut closure = edges.clone();
    // At most 64 rounds of widening reach the fixed point.
    loop {
        let mut changed = false;
        for index in 0..closure.len() {
            let mut mask = closure[index];
            for (dep, deps) in closure.iter().enumerate() {
                if mask & bit(dep) != 0 {
                    mask |= deps;
                }
            }
            if mask != closure[index] {
                closure[index] = mask;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if let Some(index) = (0..closure.len()).find(|&i| closure[i] & bit(i) != 0) {
        let cycle: Vec<&str> = (0..closure.len())
            .filter(|&j| closure[j] & bit(j) != 0 && closure[index] & bit(j) != 0)
            .map(|j| workflow.stages[j].id.as_str())
            .collect();
        bail!(
            "Stage dependencies form a cycle: {} (check after and fan-out judges)",
            cycle.join(" → ")
        );
    }
    Ok(closure)
}

/// The run order the engine uses, already validated; list order if invalid.
pub fn order(workflow: &Workflow) -> Ancestry {
    let masks = ancestors(workflow).unwrap_or_else(|_| {
        (0..workflow.stages.len().min(64))
            .map(|i| bit(i) - 1)
            .collect()
    });
    Ancestry(masks)
}

/// Which stages run before which.
#[derive(Debug, Clone)]
pub struct Ancestry(Vec<u64>);
impl Ancestry {
    /// Whether stage `before` runs, transitively, before stage `index`.
    pub fn precedes(&self, before: usize, index: usize) -> bool {
        before < 64
            && self
                .0
                .get(index)
                .is_some_and(|mask| mask & bit(before) != 0)
    }
}

/// Checks `after` and fan-out fields; `Settings::validate` also checks harnesses.
pub fn validate(workflow: &Workflow) -> Result<()> {
    ancestors(workflow)?;
    let graph = is_graph(workflow);
    let stages = &workflow.stages;
    for (index, stage) in stages.iter().enumerate() {
        let Some(fan) = &stage.fan_out else {
            continue;
        };
        let name = &stage.id;
        ensure!(
            (2..=8).contains(&fan.candidates.len()),
            "Fan-out stage {name} needs 2–8 candidates"
        );
        ensure!(
            stage.mode == Mode::Unattended,
            "Fan-out stage {name} must be unattended: candidates run side by side"
        );
        ensure!(
            matches!(stage.role, Role::Implementation | Role::Custom),
            "Fan-out stage {name} must be an implementation or custom stage"
        );
        let ids = fan.ids();
        for (n, id) in ids.iter().enumerate() {
            ensure!(
                crate::config::is_id(id),
                "Fan-out candidate IDs in {name} must contain only letters, digits, '-' and '_'"
            );
            ensure!(
                !ids[..n].contains(id),
                "Fan-out stage {name} has duplicate candidate {id}"
            );
        }
        if fan.is_auto() {
            continue;
        }
        let Some(judge) = stages.iter().position(|s| s.id == fan.judge) else {
            bail!(
                "Fan-out stage {name} names unknown judge {}; use a review stage ID or auto",
                fan.judge
            );
        };
        let judging = &stages[judge];
        ensure!(
            judge != index && judging.role == Role::Review && judging.fan_out.is_none(),
            "Judge {} of fan-out stage {name} must be another review stage",
            judging.id
        );
        ensure!(
            stages
                .iter()
                .enumerate()
                .filter(|(i, s)| *i != index
                    && s.fan_out.as_ref().is_some_and(|f| f.judge == judging.id))
                .count()
                == 0,
            "Stage {} judges more than one fan-out",
            judging.id
        );
        if graph {
            ensure!(
                judging.after.contains(name),
                "Judge {} must list fan-out stage {name} in its after",
                judging.id
            );
        } else if stage.enabled && judging.enabled {
            ensure!(
                judge > index && stages[index + 1..judge].iter().all(|s| !s.enabled),
                "Judge {} must be the next enabled stage after fan-out stage {name}",
                judging.id
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Candidate, FanOut};

    fn stage(id: &str, role: Role, after: &[&str]) -> Stage {
        Stage {
            id: id.into(),
            name: id.into(),
            role,
            after: after.iter().map(|s| (*s).into()).collect(),
            ..Stage::default()
        }
    }
    fn workflow(stages: Vec<Stage>) -> Workflow {
        Workflow {
            max_repair_passes: 0,
            ..Workflow::new("w", stages)
        }
    }
    fn fan(judge: &str) -> Option<FanOut> {
        Some(FanOut {
            candidates: vec![
                Candidate {
                    harness: "codex".into(),
                    ..Candidate::default()
                },
                Candidate {
                    harness: "claude".into(),
                    ..Candidate::default()
                },
            ],
            judge: judge.into(),
        })
    }

    #[test]
    fn lists_run_in_order_and_graphs_by_dependency() {
        let list = workflow(vec![
            stage("a", Role::Research, &[]),
            stage("b", Role::Research, &[]),
            stage("c", Role::Research, &[]),
        ]);
        assert!(!is_graph(&list));
        let list_order = order(&list);
        assert!(
            list_order.precedes(0, 2) && list_order.precedes(1, 2) && !list_order.precedes(2, 1)
        );
        let graph = workflow(vec![
            stage("a", Role::Research, &[]),
            stage("b", Role::Research, &[]),
            stage("c", Role::Review, &["a", "b"]),
            stage("d", Role::Summary, &["c"]),
        ]);
        let graph_order = order(&graph);
        assert!(!graph_order.precedes(0, 1) && !graph_order.precedes(1, 0));
        assert!(
            graph_order.precedes(0, 3) && graph_order.precedes(1, 3) && graph_order.precedes(2, 3)
        );
        validate(&graph).unwrap();
    }

    #[test]
    fn invalid_dependencies_are_rejected() {
        let error = |stages| validate(&workflow(stages)).unwrap_err().to_string();
        assert!(error(vec![stage("a", Role::Research, &["x"])]).contains("unknown stage x"));
        assert!(error(vec![stage("a", Role::Research, &["a"])]).contains("after itself"));
        assert!(
            error(vec![
                stage("a", Role::Research, &["b"]),
                stage("b", Role::Research, &["a"])
            ])
            .contains("cycle: a → b")
        );
        assert!(
            error(vec![
                stage("a", Role::Research, &[]),
                stage("b", Role::Research, &["a", "a"])
            ])
            .contains("twice")
        );
    }

    #[test]
    fn fan_outs_need_a_review_judge_in_the_right_place() {
        let mut implement = stage("implement", Role::Implementation, &[]);
        implement.fan_out = fan("judge");
        let judge = stage("judge", Role::Review, &[]);
        let verify = stage("verify", Role::Test, &[]);
        validate(&workflow(vec![
            implement.clone(),
            judge.clone(),
            verify.clone(),
        ]))
        .unwrap();
        let error = |stages| validate(&workflow(stages)).unwrap_err().to_string();
        // A list needs the judge right after the fan-out.
        assert!(
            error(vec![implement.clone(), verify.clone(), judge.clone()])
                .contains("next enabled stage")
        );
        let mut skipped = verify.clone();
        skipped.enabled = false;
        validate(&workflow(vec![implement.clone(), skipped, judge.clone()])).unwrap();
        // A graph needs the judge to run after it; dependents then wait for the judge.
        let graph = vec![
            implement.clone(),
            stage("judge", Role::Review, &["implement"]),
            stage("verify", Role::Test, &["implement"]),
        ];
        validate(&workflow(graph.clone())).unwrap();
        assert!(order(&workflow(graph)).precedes(1, 2));
        assert!(
            error(vec![
                implement.clone(),
                stage("judge", Role::Review, &[]),
                stage("verify", Role::Test, &["implement"])
            ])
            .contains("must list fan-out stage implement")
        );
        let mut not_review = judge.clone();
        not_review.role = Role::Research;
        assert!(error(vec![implement.clone(), not_review]).contains("another review stage"));
        let mut unknown = implement.clone();
        unknown.fan_out = fan("nobody");
        assert!(error(vec![unknown]).contains("unknown judge"));
        let mut interactive = implement.clone();
        interactive.mode = Mode::Interactive;
        assert!(error(vec![interactive, judge.clone()]).contains("unattended"));
        let mut one = implement.clone();
        one.fan_out.as_mut().unwrap().candidates.pop();
        assert!(error(vec![one, judge.clone()]).contains("2–8 candidates"));
        let mut twins = implement.clone();
        for candidate in &mut twins.fan_out.as_mut().unwrap().candidates {
            candidate.id = "same".into();
        }
        assert!(error(vec![twins, judge.clone()]).contains("duplicate candidate same"));
        let mut reader = implement.clone();
        reader.role = Role::Research;
        assert!(error(vec![reader, judge]).contains("implementation or custom"));
    }

    #[test]
    fn disabled_judges_leave_the_fan_out_to_auto() {
        let mut implement = stage("implement", Role::Implementation, &[]);
        implement.fan_out = fan("judge");
        let mut judge = stage("judge", Role::Review, &[]);
        assert_eq!(
            judge_of(&workflow(vec![implement.clone(), judge.clone()]), 0),
            Some(1)
        );
        judge.enabled = false;
        let flow = workflow(vec![implement, judge]);
        assert_eq!(judge_of(&flow, 0), None);
        assert_eq!(judged_by(&flow, 1), None);
    }
}
