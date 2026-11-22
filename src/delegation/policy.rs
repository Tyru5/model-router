//! The privilege ceiling: harness permission settings do not compare across
//! harnesses, so every stage and task gets four portable values, compared
//! one by one. A task may never exceed its parent on any of them.
use super::config::{Delegate, Isolation};
use crate::config::{Sandbox, Stage, Workflow};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What the process may change on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Write {
    ReadOnly,
    /// Its own worktree only (enforced).
    Worktree,
    Workspace,
}

/// How far the harness's own permission settings go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Native {
    /// The native read-only choice.
    Restricted,
    Default,
    /// Any `extra_args`, or a setting in the spec's `escalating` list.
    Escalated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Network {
    Restricted,
    Open,
}

/// Whether an OS sandbox applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enforcement {
    Enforced,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ceiling {
    pub write: Write,
    pub native: Native,
    pub network: Network,
    pub enforcement: Enforcement,
    /// Paths an enforced sandbox grants beyond the workspace
    /// (`sandbox_writable`), resolved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable: Vec<PathBuf>,
}

/// The native axis of a stage: escalated by any extra argument or an
/// escalating setting, restricted by the harness's read-only choice.
pub fn native(stage: &Stage) -> Native {
    if !stage.extra_args.is_empty() || !crate::harness_settings::escalations(stage).is_empty() {
        return Native::Escalated;
    }
    let read_only = crate::harness::get(&stage.harness)
        .and_then(|spec| spec.read_only)
        .is_some_and(|(key, choice)| {
            stage.harness_settings.get(key).map(String::as_str) == Some(choice)
        });
    if read_only {
        Native::Restricted
    } else {
        Native::Default
    }
}

/// A stage's ceiling. `backend` says whether an OS sandbox backend works
/// here, so a sandboxed mode is enforced.
pub fn stage_ceiling(workflow: &Workflow, stage: &Stage, backend: bool) -> Ceiling {
    let mode = stage.effective_sandbox(workflow);
    let enforced = mode != Sandbox::Off && backend;
    let native = native(stage);
    let write = match (enforced, mode) {
        (true, Sandbox::ReadOnly) => Write::ReadOnly,
        (true, _) => Write::Workspace,
        (false, _) if native == Native::Restricted => Write::ReadOnly,
        (false, _) => Write::Workspace,
    };
    Ceiling {
        write,
        native,
        network: if enforced && !stage.sandbox_network {
            Network::Restricted
        } else {
            Network::Open
        },
        enforcement: if enforced {
            Enforcement::Enforced
        } else {
            Enforcement::None
        },
        writable: if enforced {
            crate::sandbox::writable_grants(stage)
        } else {
            vec![]
        },
    }
}

/// The ceiling of a task: tasks always run under their own enforced plan,
/// read-only for `shared` and confined to their worktree for `worktree`,
/// with the parent's network restriction.
pub fn task_ceiling(task: &Stage, isolation: Isolation, parent: &Ceiling) -> Ceiling {
    Ceiling {
        write: match isolation {
            Isolation::Shared => Write::ReadOnly,
            Isolation::Worktree => Write::Worktree,
        },
        native: native(task),
        network: parent.network,
        enforcement: Enforcement::Enforced,
        // Tasks run with no `sandbox_writable`.
        writable: vec![],
    }
}

/// An unsandboxed command the user configured (a router check, a ship
/// stage's secret scanner): it writes the workspace, with the user's
/// network and no OS sandbox.
pub const UNSANDBOXED_COMMAND: Ceiling = Ceiling {
    write: Write::Workspace,
    native: Native::Default,
    network: Network::Open,
    enforcement: Enforcement::None,
    writable: Vec::new(),
};

/// The ceilings everything a workflow would run needs: each enabled stage,
/// each fan-out candidate, each router check, and a ship stage's secret
/// scanner. A launched run's plan must stay within its launcher's.
pub fn workflow_ceilings(workflow: &Workflow, backend: bool) -> Vec<(String, Ceiling)> {
    let mut out = vec![];
    for stage in workflow.stages.iter().filter(|s| s.enabled) {
        out.push((
            format!("stage {}", stage.id),
            stage_ceiling(workflow, stage, backend),
        ));
        for candidate in stage.fan_out.iter().flat_map(|f| &f.candidates) {
            let candidate = candidate.stage(stage);
            out.push((
                format!("candidate {} of stage {}", candidate.harness, stage.id),
                stage_ceiling(workflow, &candidate, backend),
            ));
        }
        if !stage.checks.is_empty() {
            out.push((
                format!("router checks of stage {}", stage.id),
                UNSANDBOXED_COMMAND,
            ));
        }
        if stage
            .ship
            .as_ref()
            .is_some_and(|ship| !ship.secret_scanner.is_empty())
        {
            out.push((
                format!("secret scanner of stage {}", stage.id),
                UNSANDBOXED_COMMAND,
            ));
        }
    }
    out
}

/// Why `child` exceeds `parent`, or `None` when it stays within it.
pub fn exceeds(child: &Ceiling, parent: &Ceiling) -> Option<String> {
    if child.write > parent.write {
        return Some(format!(
            "it would write more ({:?}) than its parent ({:?})",
            child.write, parent.write
        ));
    }
    if child.native > parent.native {
        return Some(format!(
            "its harness permissions ({:?}) exceed its parent's ({:?})",
            child.native, parent.native
        ));
    }
    if child.network > parent.network {
        return Some("it would reach the network from a parent that cannot".into());
    }
    if child.enforcement > parent.enforcement {
        return Some("it would run unsandboxed under a sandboxed parent".into());
    }
    // An unsandboxed parent can already write anywhere.
    if parent.enforcement == Enforcement::Enforced {
        if let Some(path) = child
            .writable
            .iter()
            .find(|c| !parent.writable.iter().any(|p| c.starts_with(p)))
        {
            return Some(format!(
                "it could write {}, which its parent cannot",
                path.display()
            ));
        }
    }
    None
}

/// Config-time check of a named delegate against a stage that names it: the
/// native axis (write, network, and enforcement follow from the task's
/// isolation and the parent at launch).
pub fn check_delegate_ceiling(
    _workflow: &Workflow,
    stage: &Stage,
    name: &str,
    delegate: &Delegate,
) -> Result<()> {
    let task = super::config::delegate_stage(name, delegate, stage, crate::config::Role::Research);
    let (task_native, parent_native) = (native(&task), native(stage));
    ensure!(
        task_native <= parent_native,
        "Stage {} names delegate {name}, whose harness permissions ({task_native:?}) exceed the stage's ({parent_native:?})",
        stage.id
    );
    if delegate.isolation == Some(Isolation::Worktree) {
        ensure!(
            native(stage) != Native::Restricted,
            "Stage {} names worktree delegate {name}, but the stage itself is read-only",
            stage.id
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Role;

    fn workflow(default: Sandbox) -> Workflow {
        Workflow {
            sandbox_default: default,
            ..Workflow::default()
        }
    }

    #[test]
    fn enforced_sandbox_sets_write_and_extra_args_only_raise_native() {
        let stage = Stage {
            role: Role::Implementation,
            harness: "codex".into(),
            sandbox: Some(Sandbox::ReadOnly),
            extra_args: vec!["--foo".into()],
            ..Stage::default()
        };
        let ceiling = stage_ceiling(&workflow(Sandbox::Off), &stage, true);
        // A sandboxed read-only stage stays read-only whatever its arguments.
        assert_eq!(ceiling.write, Write::ReadOnly);
        assert_eq!(ceiling.native, Native::Escalated);
        assert_eq!(ceiling.enforcement, Enforcement::Enforced);
        let unenforced = stage_ceiling(&workflow(Sandbox::Off), &stage, false);
        assert_eq!(unenforced.write, Write::Workspace);
        assert_eq!(unenforced.enforcement, Enforcement::None);
    }

    #[test]
    fn native_read_only_without_extra_args_is_read_only_when_unsandboxed() {
        let stage = Stage {
            harness: "codex".into(),
            harness_settings: [("sandbox".to_owned(), "read-only".to_owned())].into(),
            ..Stage::default()
        };
        let ceiling = stage_ceiling(&workflow(Sandbox::Off), &stage, true);
        assert_eq!(ceiling.write, Write::ReadOnly);
        assert_eq!(ceiling.native, Native::Restricted);
        assert_eq!(ceiling.network, Network::Open);
    }

    #[test]
    fn tasks_never_exceed_their_parent() {
        let parent = Ceiling {
            write: Write::Workspace,
            native: Native::Default,
            network: Network::Restricted,
            enforcement: Enforcement::Enforced,
            writable: vec![],
        };
        let shared = Ceiling {
            write: Write::ReadOnly,
            native: Native::Default,
            network: Network::Restricted,
            enforcement: Enforcement::Enforced,
            writable: vec![],
        };
        assert_eq!(exceeds(&shared, &parent), None);
        let escalated = Ceiling {
            native: Native::Escalated,
            ..shared.clone()
        };
        assert!(exceeds(&escalated, &parent).is_some());
        let online = Ceiling {
            network: Network::Open,
            ..shared.clone()
        };
        assert!(exceeds(&online, &parent).is_some());
        let unsandboxed = Ceiling {
            enforcement: Enforcement::None,
            ..shared
        };
        assert!(exceeds(&unsandboxed, &parent).is_some());
        let read_only_parent = Ceiling {
            write: Write::ReadOnly,
            ..parent
        };
        let worktree = Ceiling {
            write: Write::Worktree,
            native: Native::Default,
            network: Network::Restricted,
            enforcement: Enforcement::Enforced,
            writable: vec![],
        };
        assert!(exceeds(&worktree, &read_only_parent).is_some());
        // Extra sandbox grants must lie within the parent's.
        let granted = Ceiling {
            writable: vec![PathBuf::from("/data/sub")],
            ..read_only_parent.clone()
        };
        assert!(exceeds(&granted, &read_only_parent).is_some());
        let wider_parent = Ceiling {
            writable: vec![PathBuf::from("/data")],
            ..read_only_parent
        };
        assert_eq!(exceeds(&granted, &wider_parent), None);
    }
}
