use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use model_router::{
    catalog,
    config::{self, Mode, Role, Settings},
    doctor,
    engine::{self, Action, Event, Run, Status},
    history::{self, Criteria, Entry, StatusFilter},
    presets, task_import, tui,
};
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Parser)]
#[command(
    version,
    about = "Route a task through your preferred harnesses and models",
    long_about = "A native TUI and CLI for cross-harness workflows. Running without a subcommand opens the TUI. No models are launched until you start a run."
)]
struct Cli {
    #[arg(short = 'C', long, global = true, default_value = ".")]
    project: PathBuf,
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Open the TUI immediately, without its startup animation.
    #[arg(long, global = true)]
    no_animation: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}
#[derive(Subcommand)]
enum Commands {
    /// Write an editable starter or preset workflow (reviews disabled).
    Init {
        #[arg(long)]
        global: bool,
        #[arg(long)]
        force: bool,
        /// Start from a built-in workflow instead of the default starter.
        #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(presets::WORKFLOWS.iter().map(|p| p.key)))]
        preset: Option<String>,
    },
    /// List built-in stage and workflow presets without starting a harness.
    Presets,
    /// Validate the workflow file without starting any harness.
    Validate,
    /// Summarize a saved run: changes, checks, unresolved items, time, usage.
    Report {
        id: String,
        #[arg(long)]
        json: bool,
        /// Print the patch of changes made during the run.
        #[arg(long, conflicts_with = "json")]
        diff: bool,
    },
    /// Show the exact argument structure without launching a model.
    Preview {
        #[arg(short, long)]
        workflow: Option<String>,
        #[arg(long, default_value = "<your task>")]
        task: String,
    },
    /// Execute a workflow and persist its state and artifacts.
    Run {
        #[arg(short, long)]
        workflow: Option<String>,
        #[arg(long, conflicts_with_all = ["task_file", "task_url"])]
        task: Option<String>,
        /// Import a local Markdown or UTF-8 text task/spec (up to 32 KiB).
        #[arg(long, conflicts_with = "task_url")]
        task_file: Option<PathBuf>,
        /// Import a direct text URL or GitHub issue/file URL (up to 32 KiB).
        #[arg(long)]
        task_url: Option<String>,
        #[arg(long)]
        worktree: bool,
        #[arg(long)]
        unattended: bool,
        #[arg(long)]
        with_review: bool,
    },
    /// Resume a run. Retry and model changes are explicit.
    Resume {
        id: String,
        #[arg(long)]
        retry: bool,
        #[arg(long)]
        interactive: bool,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, conflicts_with_all = ["retry", "interactive", "model"])]
        advance: bool,
    },
    /// Start a new run from a saved run's task and workflow snapshot.
    Rerun {
        id: String,
        /// Use an isolated Git worktree. Default: the saved run's choice.
        #[arg(long)]
        worktree: bool,
        #[arg(long)]
        unattended: bool,
    },
    /// Compare two attempts of a stage, or a rerun's stage with its source's.
    Compare {
        id: String,
        /// Stage ID. Default: the stage retried most recently.
        #[arg(long)]
        stage: Option<String>,
        /// Attempt numbers. Default: the stage's last two.
        #[arg(long, num_args = 2, value_names = ["A", "B"], conflicts_with = "source")]
        attempts: Vec<usize>,
        /// Compare the stage's last attempt with the rerun source's last attempt.
        #[arg(long)]
        source: bool,
    },
    /// Preview removing saved runs and their artifacts; --yes removes them.
    Clean {
        /// Only runs last updated more than DAYS days ago.
        #[arg(long, value_name = "DAYS")]
        older_than: Option<u64>,
        /// Only these statuses, comma-separated (as for history --status).
        #[arg(long, value_name = "S,..", value_delimiter = ',')]
        status: Vec<StatusFilter>,
        /// Never remove the N newest runs.
        #[arg(long, value_name = "N", default_value_t = 0)]
        keep: usize,
        /// Also remove each run's worktree when it is clean. Branches stay.
        #[arg(long)]
        worktrees: bool,
        /// Remove instead of previewing.
        #[arg(long)]
        yes: bool,
    },
    /// List saved runs, newest first, including damaged records.
    History {
        /// Only these statuses: active, pending, running, interactive,
        /// awaiting_confirmation, complete, failed, blocked, cancelled, damaged.
        #[arg(long, value_name = "S", value_delimiter = ',')]
        status: Vec<StatusFilter>,
        /// Only runs whose ID, workflow, task, or status contains every word.
        #[arg(long, value_name = "TEXT")]
        search: Option<String>,
        /// Show at most N runs.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        #[arg(long)]
        json: bool,
        /// Include sizes of run and handoff directories.
        #[arg(long)]
        sizes: bool,
    },
    /// List model/mode selections without sending an inference request.
    Models { harness: String },
    /// Check harness executables, versions, and flags without a model request.
    Doctor {
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// Fail when an installed harness lacks a flag the router passes.
        #[arg(long)]
        strict: bool,
    },
}

fn main() {
    if let Err(error) = main_result() {
        // Terminal loss can close stderr too; reporting it must not panic.
        let _ = writeln!(io::stderr(), "Error: {error:#}");
        std::process::exit(1);
    }
}

fn main_result() -> Result<()> {
    let cli = Cli::parse();
    let project = catalog::check_project(&cli.project)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let signal = cancel.clone();
    ctrlc::set_handler(move || {
        signal.store(true, Ordering::Relaxed);
    })?;
    if matches!(&cli.command, Some(Commands::Presets)) {
        print!("{}", presets::listing());
        return Ok(());
    }
    if let Some(Commands::Init {
        global,
        force,
        preset,
    }) = &cli.command
    {
        let path = if *global {
            config::config_dir().join("config.toml")
        } else {
            cli.config
                .clone()
                .unwrap_or_else(|| project.join(".model-router.toml"))
        };
        ensure!(
            *force || !path.exists(),
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
        settings.save(&path)?;
        println!(
            "Saved {}\nWorkflow: {}. Reviews are disabled; edit each stage's model and mode before running.",
            path.display(),
            settings.workflows[0].name
        );
        return Ok(());
    }
    let (settings, config_path) = config::load(&project, cli.config.as_deref())?;
    match cli.command {
        None => {
            ensure!(
                io::stdin().is_terminal() && io::stdout().is_terminal(),
                "The TUI requires a terminal. Try model-router --help for CLI commands"
            );
            let animate = !cli.no_animation
                && std::env::var_os("MODEL_ROUTER_NO_ANIMATION").is_none_or(|v| v != "1")
                && std::env::var_os("TERM").is_none_or(|v| v != "dumb");
            tui::run(project, settings, config_path, cancel, animate)?;
        }
        Some(Commands::Init { .. } | Commands::Presets) => unreachable!(),
        Some(Commands::Validate) => println!(
            "Valid: {} workflow(s). Save path: {}",
            settings.workflows.len(),
            config_path.display()
        ),
        Some(Commands::Report { id, json, diff }) => {
            use model_router::report;
            let run = engine::load(&project, &id)?;
            if diff {
                print!("{}", report::diff(&run)?);
            } else if json {
                let mut value = serde_json::to_value(report::build(&run))?;
                value["changes"] = serde_json::to_value(report::changes(&run))?;
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!(
                    "Run {} · {}\nArtifacts: {}\n",
                    run.id,
                    run.workflow.name,
                    run.directory.display()
                );
                print!(
                    "{}",
                    report::render(&report::build(&run), Some(&report::changes(&run)))
                );
            }
        }
        Some(Commands::Preview { workflow, task }) => println!(
            "{}",
            engine::preview(
                &settings,
                settings.workflow(workflow.as_deref())?,
                &project,
                &task
            )?
        ),
        Some(Commands::Doctor { json, strict }) => {
            let reports = doctor::inspect_all(&settings, &project);
            if json {
                println!("{}", serde_json::to_string_pretty(&reports)?);
            } else {
                print!("{}", doctor::table(&reports));
                println!("\n{}", doctor::AUTH_NOTE);
            }
            ensure!(
                !strict || !reports.iter().any(doctor::Report::missing_flags),
                "An installed harness lacks a flag the router passes"
            );
        }
        Some(Commands::Models { harness }) => {
            let catalog = catalog::discover(&harness, &settings, &project)?;
            for model in catalog.models {
                println!(
                    "{}{}{}\t{}\t{}",
                    if model.provider.is_empty() {
                        ""
                    } else {
                        &model.provider
                    },
                    if model.provider.is_empty() { "" } else { "/" },
                    if model.id.is_empty() {
                        "(default)"
                    } else {
                        &model.id
                    },
                    model.source,
                    model.availability
                );
            }
            println!("\n{}", catalog.note);
        }
        Some(Commands::History {
            status,
            search,
            limit,
            json,
            sizes,
        }) => {
            let entries = history::list(&project)?;
            let mut shown = history::filter(&entries, &status, search.as_deref().unwrap_or(""));
            shown.truncate(limit.unwrap_or(usize::MAX));
            let size = |entry: &Entry| sizes.then(|| history::run_size(&project, entry));
            if json {
                let mut values = vec![];
                for entry in &shown {
                    let mut value = match entry {
                        Entry::Summary(summary) => serde_json::to_value(summary)?,
                        Entry::Damaged { id, path, error } => serde_json::json!({
                            "id": id, "status": "damaged", "path": path, "error": error
                        }),
                    };
                    if let Some(bytes) = size(entry) {
                        value["size"] = bytes.into();
                    }
                    values.push(value);
                }
                println!("{}", serde_json::to_string_pretty(&values)?);
                return Ok(());
            }
            if entries.is_empty() {
                println!("No saved runs in {}", project.display());
            } else if shown.is_empty() {
                println!("No runs match ({} saved)", entries.len());
            }
            let mut total = 0;
            for entry in &shown {
                let bytes = size(entry).map_or_else(String::new, |bytes| {
                    total += bytes;
                    format!("{:>8}  ", history::human(bytes))
                });
                match entry {
                    Entry::Summary(run) => println!(
                        "{}  {:18} {bytes}{}  {}",
                        run.id,
                        run.status.label(),
                        run.workflow,
                        run.task
                    ),
                    Entry::Damaged { id, error, .. } => println!("DAMAGED {id}  {bytes}{error}"),
                }
            }
            if sizes && !shown.is_empty() {
                println!("Total {} in {} runs", history::human(total), shown.len());
            }
        }
        Some(Commands::Run {
            workflow,
            task,
            task_file,
            task_url,
            worktree,
            unattended,
            with_review,
        }) => {
            let task = match (task, task_file, task_url) {
                (Some(task), _, _) => task,
                (_, Some(path), _) => task_import::read(&path)?,
                (_, _, Some(url)) => task_import::read_url(&url, &cancel)?,
                _ => bail!("Supply --task, --task-file, or --task-url"),
            };
            let mut workflow = settings.workflow(workflow.as_deref())?.clone();
            for stage in &mut workflow.stages {
                if unattended {
                    stage.mode = Mode::Unattended;
                }
                if with_review && stage.role == Role::Review {
                    stage.enabled = true;
                }
            }
            let mut run = Run::create(&project, &settings, &workflow, task, worktree)?;
            println!("Run: {}\nArtifacts: {}", run.id, run.directory.display());
            cli_drive(&mut run, &cancel)?;
        }
        Some(Commands::Resume {
            id,
            retry,
            interactive,
            model,
            advance,
        }) => {
            let mut run = engine::load(&project, &id)?;
            if advance {
                run.confirm()?;
            }
            if retry || interactive || model.is_some() {
                run.retry(interactive, model)?;
            }
            cli_drive(&mut run, &cancel)?;
        }
        Some(Commands::Rerun {
            id,
            worktree,
            unattended,
        }) => {
            let mut run = history::rerun(&project, &settings, &id, worktree.then_some(true))?;
            if unattended {
                for stage in &mut run.workflow.stages {
                    stage.mode = Mode::Unattended;
                }
                run.persist()?;
            }
            println!(
                "Run: {}\nRerun of: {id}\nArtifacts: {}",
                run.id,
                run.directory.display()
            );
            cli_drive(&mut run, &cancel)?;
        }
        Some(Commands::Compare {
            id,
            stage,
            attempts,
            source,
        }) => {
            let run = engine::load(&project, &id)?;
            let index = match &stage {
                Some(stage) => run
                    .workflow
                    .stages
                    .iter()
                    .position(|s| s.id == *stage)
                    .with_context(|| format!("Run {id} has no stage {stage}"))?,
                None if source => run
                    .stages
                    .iter()
                    .rposition(|s| !s.attempts.is_empty())
                    .context("This run has no attempts yet")?,
                None => history::retried_stage(&run)
                    .context("No stage in this run has two attempts to compare")?,
            };
            let comparison = if source {
                let origin = run.rerun_of.as_deref().context("This run is not a rerun")?;
                let origin = engine::load(&project, origin)?;
                history::compare_rerun(&origin, &run, &run.workflow.stages[index].id)?
            } else {
                let numbers: Vec<_> = run.stages[index]
                    .attempts
                    .iter()
                    .map(|a| a.number)
                    .collect();
                let (a, b) = match (attempts.as_slice(), numbers.as_slice()) {
                    ([a, b], _) | ([], [.., a, b]) => (*a, *b),
                    _ => bail!(
                        "Stage {} has {} attempt(s); give two with --attempts A B",
                        run.workflow.stages[index].id,
                        numbers.len()
                    ),
                };
                history::compare(&run, index, a, b)?
            };
            print!("{}", comparison.text());
        }
        Some(Commands::Clean {
            older_than,
            status,
            keep,
            worktrees,
            yes,
        }) => {
            let plan = history::cleanup_plan(
                &project,
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
        }
    }
    Ok(())
}

fn cli_drive(run: &mut Run, cancel: &AtomicBool) -> Result<()> {
    loop {
        let action = engine::drive(run, cancel, |event| match event {
            Event::State(run) => eprintln!("[{}] {}", run.status.label(), run.current_name()),
            Event::Output(text) => {
                print!("{text}");
                let _ = io::stdout().flush();
            }
        })?;
        match action {
            Action::Complete => {
                use model_router::report;
                println!(
                    "\nCompleted {}\nArtifacts: {}",
                    run.id,
                    run.directory.display()
                );
                print!(
                    "\n{}",
                    report::render(&report::build(run), Some(&report::changes(run)))
                );
                return Ok(());
            }
            Action::Stopped => {
                engine::ensure_runnable(run)?;
                bail!("Run stopped");
            }
            Action::Interactive => {
                if !io::stdin().is_terminal() {
                    bail!(
                        "Stage {} is interactive. Resume {} from a terminal",
                        run.current_name(),
                        run.id
                    );
                }
                engine::run_interactive(run, cancel)?;
            }
            Action::Confirm => {
                if let Some(handoff) = &run.stages[run.cursor].handoff {
                    println!("\n{}\n", serde_json::to_string_pretty(handoff)?);
                }
                let waiver = run.pending_waiver().map(str::to_owned);
                if let Some(reason) = &waiver {
                    println!(
                        "{} reported no passing required check. Not applicable: {reason}\n",
                        run.current_name()
                    );
                }
                if !io::stdin().is_terminal() {
                    if waiver.is_some() {
                        bail!(
                            "Run awaits acceptance of a not-applicable verification. Use resume {} --advance to accept it",
                            run.id
                        );
                    }
                    bail!(
                        "Interactive handoff awaits inspection. Use resume {} --advance after inspecting it",
                        run.id
                    );
                }
                print!(
                    "{}",
                    if waiver.is_some() {
                        "[a] Accept without checks  [r] Reopen stage  [q] Save and stop: "
                    } else {
                        "[a] Advance  [r] Reopen stage  [q] Save and stop: "
                    }
                );
                io::stdout().flush()?;
                let mut answer = String::new();
                io::stdin().read_line(&mut answer)?;
                match answer.trim() {
                    "a" => run.confirm()?,
                    "r" => run.retry(true, None)?,
                    _ => {
                        println!(
                            "Saved. Resume with: model-router -C {} resume {}",
                            shell_words::quote(&run.project.display().to_string()),
                            run.id
                        );
                        return Ok(());
                    }
                }
            }
        }
        if run.status == Status::Cancelled {
            engine::ensure_runnable(run)?;
        }
    }
}
