use anyhow::{Result, bail, ensure};
use clap::{Parser, Subcommand};
use model_router::{
    catalog,
    config::{self, Mode, Role, Settings},
    doctor,
    engine::{self, Action, Event, Run, Status},
    presets, storage, task_import, tui,
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
    /// List saved runs for the selected project.
    History,
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
        Some(Commands::History) => {
            let runs = engine::history(&project)?;
            if runs.is_empty() {
                println!("No saved runs in {}", project.display());
            }
            for run in runs {
                println!(
                    "{}  {:18} {}  {}",
                    run.id,
                    run.status.label(),
                    run.workflow.name,
                    storage::truncate(&run.task.replace('\n', " "), 90)
                );
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
                println!(
                    "\nCompleted {}\nArtifacts: {}",
                    run.id,
                    run.directory.display()
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
                if !io::stdin().is_terminal() {
                    bail!(
                        "Interactive handoff awaits inspection. Use resume {} --advance after inspecting it",
                        run.id
                    );
                }
                print!("[a] Advance  [r] Reopen stage  [q] Save and stop: ");
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
