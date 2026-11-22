use anyhow::{Context, Result, bail, ensure};
use model_router::{
    config::{self, Settings},
    history::{self, Criteria, StatusFilter},
    presets,
    trust::{self, Trust},
};
use std::{
    io::{self, IsTerminal, Write},
    path::Path,
};

pub fn init(
    project: &Path,
    explicit_config: Option<&Path>,
    global: bool,
    force: bool,
    preset: Option<&str>,
    detect: bool,
) -> Result<()> {
    let path = if global {
        config::config_dir().join("config.toml")
    } else {
        explicit_config
            .map(Path::to_path_buf)
            .unwrap_or_else(|| project.join(".model-router.toml"))
    };
    ensure!(
        force || !path.exists(),
        "{} already exists; use --force to replace it",
        path.display()
    );
    let mut settings = Settings::default();
    if let Some(key) = preset {
        settings.workflows = vec![
            presets::WORKFLOWS
                .iter()
                .find(|p| p.key == key)
                .expect("Clap validates workflow preset keys")
                .build(),
        ];
    }
    let mut detected: Option<Vec<&'static str>> = None;
    if detect {
        let installed: Vec<&'static str> = model_router::harness::SPECS
            .iter()
            .filter(|spec| {
                spec.programs
                    .iter()
                    .any(|prog| model_router::adapters::executable(prog).is_some())
            })
            .map(|spec| spec.name)
            .collect();
        for workflow in &mut settings.workflows {
            presets::apply_detected_harnesses(workflow, &installed);
        }
        detected = Some(installed);
    }
    settings.save(&path)?;
    let workflow = &settings.workflows[0];
    let reviews_disabled = workflow
        .stages
        .iter()
        .any(|s| s.role == config::Role::Review && !s.enabled)
        && workflow
            .stages
            .iter()
            .filter(|s| s.role == config::Role::Review)
            .all(|s| !s.enabled);
    let message = match detected.as_deref() {
        None if reviews_disabled => format!(
            "Saved {}\nWorkflow: {}. Reviews are disabled; edit each stage's model and mode before running.",
            path.display(),
            workflow.name,
        ),
        None => format!(
            "Saved {}\nWorkflow: {}. Edit each stage's model and mode before running.",
            path.display(),
            workflow.name,
        ),
        Some([]) if reviews_disabled => format!(
            "Saved {}\nWorkflow: {}. Reviews are disabled.\nNo built-in harnesses found on PATH; stage models are blank to inherit defaults. Run `model-router doctor` to check available harnesses.",
            path.display(),
            workflow.name,
        ),
        Some([]) => format!(
            "Saved {}\nWorkflow: {}.\nNo built-in harnesses found on PATH; stage models are blank to inherit defaults. Run `model-router doctor` to check available harnesses.",
            path.display(),
            workflow.name,
        ),
        Some(installed) if reviews_disabled => format!(
            "Saved {}\nWorkflow: {}. Reviews are disabled; check stage modes before running.\nDetected harnesses: {}. Stage models are blank to inherit each harness's default; setting a model is optional.",
            path.display(),
            workflow.name,
            installed.join(", "),
        ),
        Some(installed) => format!(
            "Saved {}\nWorkflow: {}. Check stage modes before running.\nDetected harnesses: {}. Stage models are blank to inherit each harness's default; setting a model is optional.",
            path.display(),
            workflow.name,
            installed.join(", "),
        ),
    };
    println!("{message}");
    Ok(())
}

pub fn completions<C: clap::CommandFactory>(shell: clap_complete::Shell, bin: &str) -> Result<()> {
    ensure!(
        !bin.is_empty()
            && bin
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)),
        "--bin must be a plain command name, got {bin:?}"
    );
    let mut command = C::command();
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut command, bin, &mut script);
    // `completions zsh | head` closes the pipe early; that is not an error.
    match io::stdout().lock().write_all(&script) {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
        result => result.context("writing the completion script")?,
    }
    Ok(())
}

/// `model-router trust`: approve the project config's dangerous sections as
/// they are now, or revoke the approval.
pub fn trust(project: &Path, explicit: Option<&Path>, yes: bool, revoke: bool) -> Result<()> {
    if revoke {
        if trust::revoke(project)? {
            println!("Revoked trust for {}", project.display());
        } else {
            println!("{} was not trusted", project.display());
        }
        return Ok(());
    }
    let loaded = config::load_config(project, explicit)?;
    if loaded.origin != config::Origin::Project {
        println!(
            "Nothing to trust: only a project's own .model-router.toml needs trust. Global and --config files are yours."
        );
        return Ok(());
    }
    let review = match loaded.trust {
        Trust::Trusted if trust::dangerous(&loaded.settings).is_empty() => {
            println!(
                "Nothing to trust: the project config runs no commands and sends no credentials."
            );
            return Ok(());
        }
        Trust::Trusted => {
            println!("Already trusted: {}", project.display());
            return Ok(());
        }
        Trust::Untrusted(review) => review,
    };
    print!("{}", review.text());
    if !yes {
        ensure!(
            io::stdin().is_terminal(),
            "Confirm from a terminal, or pass --yes"
        );
        print!("Trust these sections? [y/N] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("Not trusted.");
            return Ok(());
        }
    }
    trust::approve(&review)?;
    println!(
        "Trusted {}. A change to these sections asks again.",
        project.display()
    );
    Ok(())
}

pub fn validate(
    settings: &Settings,
    config_path: &Path,
    review: Option<&trust::Review>,
) -> Result<()> {
    println!(
        "Valid: {} workflow(s). Save path: {}",
        settings.workflows.len(),
        config_path.display()
    );
    for warning in settings
        .warnings
        .iter()
        .filter(|w| review.is_none() || !w.contains("untrusted"))
    {
        println!("Warning: {warning}");
    }
    if let Some(review) = review {
        print!("{}", review.text());
    }
    Ok(())
}

pub fn clean(
    project: &Path,
    older_than: Option<u64>,
    status: Vec<StatusFilter>,
    keep: usize,
    worktrees: bool,
    yes: bool,
) -> Result<()> {
    let plan = history::cleanup_plan(
        project,
        &Criteria {
            older_than_days: older_than,
            statuses: status,
            keep,
            worktrees,
            ..Criteria::default()
        },
    )?;
    if yes {
        print!("{}", history::apply(&plan).text(true));
    } else {
        print!("{}", plan.text(false));
        if !plan.items.is_empty() {
            println!("Preview only. Add --yes to remove them.");
        }
    }
    Ok(())
}

pub fn apply(project: &Path, id: &str, check: bool, yes: bool) -> Result<()> {
    use model_router::{engine, fanout};
    let run = engine::load(project, id)?;
    let plan = engine::apply_worktree_plan(&run)?;
    print!("{}", plan.preview());
    if check {
        let (_, outcome) = engine::apply_worktree(&run, true)?;
        match outcome {
            fanout::Applied::Clean => {
                println!("Check passed: patch applies cleanly.");
                return Ok(());
            }
            fanout::Applied::Merged => {
                println!(
                    "Check passed: patch applies via 3-way merge with checkout changes made since the run started."
                );
                return Ok(());
            }
            fanout::Applied::Conflict(files) => {
                bail!(
                    "Check failed: patch conflicts with the current checkout (affected files: {})",
                    files.join(", ")
                );
            }
        }
    }
    if !yes {
        ensure!(
            io::stdin().is_terminal(),
            "Nothing applied. Add --yes to apply these worktree changes without asking"
        );
        print!("Apply these changes to the checkout? [y/N] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("Nothing applied.");
            return Ok(());
        }
    }
    let (_, outcome) = engine::apply_worktree(&run, false)?;
    match outcome {
        fanout::Applied::Clean => println!(
            "Applied {} file(s) from run {id} into {} (uncommitted).",
            plan.files.len(),
            plan.project.display()
        ),
        fanout::Applied::Merged => println!(
            "Applied {} file(s) from run {id} into {} via 3-way merge (uncommitted).",
            plan.files.len(),
            plan.project.display()
        ),
        fanout::Applied::Conflict(files) => {
            bail!(
                "Could not apply run {id} (conflicts in {}); checkout was left unchanged",
                files.join(", ")
            );
        }
    }
    Ok(())
}

pub fn ship(project: &Path, id: &str, push: bool, draft_pr: bool, yes: bool) -> Result<()> {
    use model_router::{engine, ship, storage};
    use std::time::Duration;
    let wait = Duration::from_secs(2);
    let run = engine::load(project, id)?;
    let _run_lock = storage::lock_wait(&run.directory.join("run.lock"), wait)?;
    let _workspace_lock = if run.workspace.is_dir() {
        Some(storage::lock_wait(
            &storage::metadata_dir(&run.workspace)?.join("workspace.lock"),
            wait,
        )?)
    } else {
        None
    };
    let run = engine::load(project, id)?;
    let mut options = ship::run_options(&run);
    options.push |= push || draft_pr;
    options.draft_pr |= draft_pr;
    let (plan, dir) = ship::plan_completed(&run, &options)?;
    print!("{}", plan.preview());
    println!("Plan saved in {}", dir.display());
    if !yes {
        ensure!(
            io::stdin().is_terminal(),
            "Nothing shipped. Add --yes to ship this plan without asking"
        );
        print!("Ship it? [y/N] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("Nothing shipped.");
            return Ok(());
        }
    }
    let outcome = ship::apply(&plan, &dir, |line| print!("{line}"));
    for line in outcome.lines() {
        println!("{line}");
    }
    if let Some(error) = &outcome.error {
        bail!("Ship stopped: {error}");
    }
    Ok(())
}

/// What `model-router schedules` does.
pub enum SchedulesCommand {
    List { json: bool },
    Approve(String),
    Pause(String),
    Enable(String),
    Delete(String),
    Tick,
}

/// `model-router schedules`: the schedules a stage's agent created.
pub fn schedules(
    project: &Path,
    command: SchedulesCommand,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    use model_router::delegation::schedules;
    match command {
        SchedulesCommand::List { json } => {
            let store = schedules::load(project)?;
            let settings = schedules::trusted_settings(project).ok();
            let now = chrono::Utc::now();
            if json {
                let rows: Vec<serde_json::Value> = store
                    .schedules
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "schedule": s,
                            "approved": settings.as_ref().is_some_and(|c| schedules::approved(project, s, c)),
                            "next_due": schedules::next_due(s, now),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            if store.schedules.is_empty() {
                println!("No schedules.");
                return Ok(());
            }
            for s in &store.schedules {
                let approved = settings
                    .as_ref()
                    .is_some_and(|c| schedules::approved(project, s, c));
                let state = match (s.enabled, approved) {
                    (false, _) => "paused",
                    (true, true) => "approved",
                    (true, false) => "needs approval",
                };
                println!(
                    "{}  {state}  {}  workflow {}  task: {}{}",
                    s.id,
                    s.trigger.describe(),
                    s.workflow.as_deref().unwrap_or("(default)"),
                    model_router::storage::truncate(&s.task, 80),
                    s.paused_reason
                        .as_ref()
                        .map(|r| format!("  ({r})"))
                        .unwrap_or_default()
                );
            }
            Ok(())
        }
        SchedulesCommand::Approve(id) => {
            let schedule = schedules::approve(project, &id)?;
            println!(
                "Approved {}: {} runs workflow {} with task: {}",
                schedule.id,
                schedule.trigger.describe(),
                schedule.workflow.as_deref().unwrap_or("(default)"),
                schedule.task
            );
            Ok(())
        }
        SchedulesCommand::Pause(id) => {
            schedules::set_enabled(project, &id, false)?;
            println!("Paused {id}; enabling it again needs approval.");
            Ok(())
        }
        SchedulesCommand::Enable(id) => {
            schedules::set_enabled(project, &id, true)?;
            println!("Enabled {id}; approve it with: model-router schedules approve {id}");
            Ok(())
        }
        SchedulesCommand::Delete(id) => {
            schedules::delete(project, &id)?;
            println!("Deleted {id}.");
            Ok(())
        }
        SchedulesCommand::Tick => {
            let fired = schedules::fire_due(project, chrono::Utc::now(), None)?;
            let mut failed = 0;
            for item in fired {
                let Some(mut run) = item.run else {
                    failed += 1;
                    eprintln!(
                        "{}: launch failed: {}",
                        item.schedule,
                        item.error.unwrap_or_default()
                    );
                    continue;
                };
                // `tick` is its own run host: it drives each run to an end.
                let action = model_router::engine::drive(&mut run, cancel, |_| {});
                match action {
                    Ok(action) => println!("{}: run {} {:?}", item.schedule, run.id, action),
                    Err(error) => {
                        failed += 1;
                        eprintln!("{}: run {}: {error:#}", item.schedule, run.id);
                    }
                }
                if matches!(
                    run.status,
                    model_router::engine::Status::Failed
                        | model_router::engine::Status::Blocked
                        | model_router::engine::Status::Cancelled
                ) {
                    failed += 1;
                }
            }
            ensure!(failed == 0, "{failed} scheduled occurrence(s) failed");
            Ok(())
        }
    }
}
