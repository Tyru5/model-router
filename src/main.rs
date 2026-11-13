use anyhow::{Context, Result, bail, ensure};
use clap::{CommandFactory, Parser, Subcommand};
use model_router::{
    catalog,
    config::{self, Mode, Role, Settings},
    cost, doctor,
    engine::{self, Action, Cmd, Event, Run, Status},
    history::{self, Criteria, Entry, StatusFilter},
    notify, presets, process, providers, task_import,
    trust::{self, Trust},
    tui,
};
use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

#[derive(Parser)]
#[command(
    version,
    about = "Route a task through your preferred harnesses and models",
    long_about = "A native TUI and CLI for cross-harness workflows. Running without a subcommand opens the TUI. No models are launched until you start a run."
)]
struct Cli {
    /// Project directory. Default: the nearest directory above the current
    /// one with a .model-router.toml, else the Git root, else the current one.
    #[arg(short = 'C', long, global = true, value_name = "DIR")]
    project: Option<PathBuf>,
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
    /// Print a shell completion script for model-router (or the mr alias).
    Completions {
        /// Shell to generate for.
        shell: clap_complete::Shell,
        /// Command name the script completes; use `mr` for the alias.
        #[arg(long, default_value = "model-router", value_name = "NAME")]
        bin: String,
    },
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
        /// Include the run's status timeline (transitions.jsonl).
        #[arg(long, conflicts_with = "diff")]
        transitions: bool,
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
        /// Notify (bell/OSC, desktop, webhook per saved preferences) when the
        /// run needs input, stops, or completes. Also MODEL_ROUTER_NOTIFY=1.
        #[arg(long)]
        notify: bool,
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
        /// Allow one more review repair pass after the repair limit stopped the run.
        #[arg(long, conflicts_with_all = ["retry", "interactive", "model", "advance"])]
        allow_repair: bool,
        /// Rerun one stopped fan-out candidate, keeping the others: STAGE/CANDIDATE.
        #[arg(long, value_name = "STAGE/CANDIDATE", conflicts_with_all = ["retry", "interactive", "model", "advance", "allow_repair"])]
        retry_candidate: Option<String>,
        /// Notify (bell/OSC, desktop, webhook per saved preferences) when the
        /// run needs input, stops, or completes. Also MODEL_ROUTER_NOTIFY=1.
        #[arg(long)]
        notify: bool,
        /// Set the run's USD budget (and any exceeded stage cap) to N and continue.
        #[arg(long, value_name = "N")]
        budget_usd: Option<f64>,
        /// Set the run's token budget (and any exceeded stage cap) to N and continue.
        #[arg(long, value_name = "N")]
        budget_tokens: Option<u64>,
        /// First forget the process groups recovery left running because it
        /// could not prove they were this run's harnesses. They are not
        /// signalled; check them before releasing.
        #[arg(long)]
        release_groups: bool,
    },
    /// Estimated spend over saved runs, by harness and model.
    Cost {
        /// Only attempts started within this long ago, e.g. 7d, 12h, 2w.
        #[arg(long, value_name = "AGE")]
        since: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Commit a completed run's changes on a new branch, after a preview.
    /// Uses the run's ship stage options, else the defaults.
    Ship {
        id: String,
        /// Push the branch with --set-upstream.
        #[arg(long)]
        push: bool,
        /// Open a draft pull request with gh; implies --push.
        #[arg(long)]
        draft_pr: bool,
        /// Ship without asking after the preview.
        #[arg(long)]
        yes: bool,
    },
    /// Start a new run from a saved run's task and workflow snapshot.
    Rerun {
        id: String,
        /// Use an isolated Git worktree. Default: the saved run's choice.
        #[arg(long)]
        worktree: bool,
        #[arg(long)]
        unattended: bool,
        /// Notify (bell/OSC, desktop, webhook per saved preferences) when the
        /// run needs input, stops, or completes. Also MODEL_ROUTER_NOTIFY=1.
        #[arg(long)]
        notify: bool,
    },
    /// Start an isolated run from a saved run's checkpoint at the start of a
    /// stage, reusing its completed upstream handoffs. The original run and
    /// workspace are unchanged; nothing is merged or shipped.
    Fork {
        id: String,
        /// The stage to continue from.
        #[arg(long)]
        stage: String,
        /// Run that stage with this model instead of the saved one.
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        unattended: bool,
        /// Create the fork and print its ID without running it.
        #[arg(long)]
        no_run: bool,
        /// Notify (bell/OSC, desktop, webhook per saved preferences) when the
        /// run needs input, stops, or completes. Also MODEL_ROUTER_NOTIFY=1.
        #[arg(long)]
        notify: bool,
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
    /// Show what a stage receives of upstream handoffs: which fields were
    /// shortened or omitted, and lost decisions or findings. No model call.
    Handoffs {
        id: String,
        /// Stage ID. Default: the run's current stage.
        #[arg(long)]
        stage: Option<String>,
        /// Also print the exact prompt the stage would receive now.
        #[arg(long, conflicts_with = "json")]
        prompt: bool,
        #[arg(long)]
        json: bool,
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
    /// Outcomes per role, harness, and model across saved runs, ranked.
    /// Suggestions only; never changes a workflow.
    Stats {
        /// Only this role: research, planning, implementation, review, test, summary, custom.
        #[arg(long, value_parser = parse_role)]
        role: Option<Role>,
        #[arg(long)]
        json: bool,
        /// Include every recent project, not only this one.
        #[arg(long)]
        all_projects: bool,
        /// Add an advisory cost-quality shortlist: assignments whose
        /// required router checks passed at least this fraction of the time
        /// (0–1), cheapest known cost then fastest first.
        #[arg(long, value_name = "RATE", value_parser = parse_rate)]
        min_pass: Option<f64>,
    },
    /// Run tasks under a matrix of stage assignments, each unattended in its
    /// own Git worktree, and summarize the outcomes.
    Bench {
        /// Bench file (TOML); see examples/bench.toml.
        file: PathBuf,
        /// Runs at a time.
        #[arg(long, default_value_t = 1, value_name = "N")]
        jobs: usize,
        /// Show the matrix without running anything.
        #[arg(long)]
        dry_run: bool,
        /// Run the workflow's interactive stages unattended instead of refusing.
        #[arg(long)]
        allow_interactive: bool,
        /// Add an advisory shortlist of variants whose required router checks
        /// passed at least this fraction of the time (0–1), cheapest first.
        #[arg(long, value_name = "RATE", value_parser = parse_rate)]
        min_pass: Option<f64>,
    },
    /// List model/mode selections without sending an inference request.
    Models { harness: String },
    /// List built-in and custom harnesses, or print resolved specs as JSON.
    Harnesses {
        /// Print the resolved spec of NAME, or of every harness, as JSON.
        #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
        spec: Option<String>,
    },
    /// List router providers, or a provider's model IDs (one listing request).
    Providers {
        /// Query this provider's OpenAI-compatible /models endpoint.
        name: Option<String>,
    },
    /// Serve tools for other agents: MCP over stdio and/or a loopback HTTP API.
    Serve(model_router::serve::Args),
    /// Trust this project's .model-router.toml: its custom adapters, checks,
    /// extra arguments, escalated permissions, and custom providers.
    Trust {
        /// Trust without asking.
        #[arg(long)]
        yes: bool,
        /// Forget this project's trust; its dangerous sections are ignored again.
        #[arg(long, conflicts_with = "yes")]
        revoke: bool,
    },
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
    // An interactive harness's terminal host (see `process::host`), never
    // typed by people, so it bypasses argument parsing and project setup.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == process::host::ARG)
    {
        process::host::main();
    }
    if let Err(error) = main_result() {
        // Terminal loss can close stderr too; reporting it must not panic.
        let _ = writeln!(io::stderr(), "Error: {error:#}");
        std::process::exit(1);
    }
}

fn main_result() -> Result<()> {
    let cli = Cli::parse();
    let cwd = catalog::check_project(Path::new("."))?;
    let project = match &cli.project {
        Some(path) => catalog::check_project(path)?,
        None => catalog::detect_project(&cwd)?,
    };
    // Tell the TUI when the project was found above the current directory.
    let detected_from = (cli.project.is_none() && project != cwd).then_some(cwd);
    let cancel = Arc::new(AtomicBool::new(false));
    let signal = cancel.clone();
    ctrlc::set_handler(move || {
        signal.store(true, Ordering::Relaxed);
    })?;
    if matches!(&cli.command, Some(Commands::Presets)) {
        print!("{}", presets::listing());
        return Ok(());
    }
    if let Some(Commands::Completions { shell, bin }) = &cli.command {
        ensure!(
            !bin.is_empty()
                && bin
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)),
            "--bin must be a plain command name, got {bin:?}"
        );
        let mut command = Cli::command();
        let mut script = Vec::new();
        clap_complete::generate(*shell, &mut command, bin.as_str(), &mut script);
        // `completions zsh | head` closes the pipe early; that is not an error.
        match io::stdout().lock().write_all(&script) {
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
            result => result.context("writing the completion script")?,
        }
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
    if let Some(Commands::Serve(args)) = cli.command {
        // Loads each project's config per call; the signal flag stops it.
        return model_router::serve::run(args, project, cli.config, cancel);
    }
    if let Some(Commands::Trust { yes, revoke }) = &cli.command {
        return trust_command(&project, cli.config.as_deref(), *yes, *revoke);
    }
    let loaded = config::load_config(&project, cli.config.as_deref())?;
    let config_path = loaded.save_path.clone();
    let review = loaded.review().cloned();
    // Commands that execute a project's dangerous sections refuse until it is
    // trusted; the TUI asks; read-only commands ignore those sections.
    let settings = match (&cli.command, &review) {
        (_, None) => loaded.settings,
        (Some(Commands::Run { .. }), Some(review)) => bail!("{}", review.text().trim_end()),
        (None, Some(review)) => {
            ensure!(
                io::stdin().is_terminal() && io::stdout().is_terminal(),
                "The TUI requires a terminal. Try model-router --help for CLI commands"
            );
            match tui::trust_prompt::ask(review)? {
                tui::trust_prompt::Choice::Trust => {
                    trust::approve(review)?;
                    loaded.settings
                }
                tui::trust_prompt::Choice::Ignore => loaded.safe(),
                tui::trust_prompt::Choice::Quit => return Ok(()),
            }
        }
        (Some(Commands::Validate), Some(_)) => loaded.safe(),
        (Some(_), Some(review)) => {
            eprintln!(
                "Ignoring untrusted sections of {}; run `model-router trust` to use them.",
                review.project.join(".model-router.toml").display()
            );
            loaded.safe()
        }
    };
    match cli.command {
        None => {
            ensure!(
                io::stdin().is_terminal() && io::stdout().is_terminal(),
                "The TUI requires a terminal. Try model-router --help for CLI commands"
            );
            let animate = !cli.no_animation
                && std::env::var_os("MODEL_ROUTER_NO_ANIMATION").is_none_or(|v| v != "1")
                && std::env::var_os("TERM").is_none_or(|v| v != "dumb");
            tui::run(
                project,
                settings,
                config_path,
                cancel,
                animate,
                detected_from,
            )?;
        }
        Some(
            Commands::Init { .. }
            | Commands::Presets
            | Commands::Completions { .. }
            | Commands::Trust { .. }
            | Commands::Serve(_),
        ) => {
            unreachable!()
        }
        Some(Commands::Validate) => {
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
            if let Some(review) = &review {
                print!("{}", review.text());
            }
        }
        Some(Commands::Report {
            id,
            json,
            diff,
            transitions,
        }) => {
            use model_router::report;
            let run = engine::load(&project, &id)?;
            cost::refresh_provider_prices(&project, &cost::routed_providers(&run));
            let timeline = transitions.then(|| report::transitions(&run)).transpose()?;
            if diff {
                print!("{}", report::diff(&run)?);
            } else if json {
                let mut value = serde_json::to_value(report::build(&run))?;
                value["changes"] = serde_json::to_value(report::changes(&run))?;
                value["receipts"] = serde_json::to_value(report::receipts(&run))?;
                if let Some(timeline) = &timeline {
                    value["transitions"] = serde_json::to_value(timeline)?;
                }
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else if let Some(timeline) = &timeline {
                print!("{}", report::render_transitions(&run, timeline));
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
                print_receipts(&run);
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
                println!("\n{}", doctor::sandbox_line());
                println!("{}", doctor::tools_line(&doctor::inspect_tools(&project)));
                println!("\n{}", doctor::AUTH_NOTE);
            }
            ensure!(
                !strict || !reports.iter().any(doctor::Report::missing_flags),
                "An installed harness lacks a flag the router passes"
            );
        }
        Some(Commands::Stats {
            role,
            json,
            all_projects,
            min_pass,
        }) => stats_command(&project, role, json, all_projects, min_pass)?,
        Some(Commands::Bench {
            file,
            jobs,
            dry_run,
            allow_interactive,
            min_pass,
        }) => bench_command(
            &project,
            &settings,
            &file,
            &model_router::bench::Options {
                jobs,
                allow_interactive,
            },
            dry_run,
            min_pass,
            &cancel,
        )?,
        Some(Commands::Harnesses { spec }) => harnesses_command(&settings, spec.as_deref())?,
        Some(Commands::Models { harness }) => {
            let catalog = catalog::discover(&harness, &settings, &project)?;
            for model in catalog.models {
                println!(
                    "{}{}{}\t{}\t{}\t{}",
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
                    model.availability,
                    model.efforts.join("/")
                );
            }
            if !catalog.efforts.is_empty() {
                println!(
                    "\nEffort levels ({}): {}",
                    catalog.effort_source,
                    catalog.efforts.join(", ")
                );
            }
            println!("\n{}", catalog.note);
        }
        Some(Commands::Providers { name: Some(name) }) => {
            let provider = providers::resolve(&name, &settings.providers).with_context(|| {
                format!("Unknown provider {name}; see `model-router providers`")
            })?;
            for model in providers::models(&provider)? {
                let mut line = model.id.clone();
                if let Some(context) = model.context_length {
                    line.push_str(&format!("\tcontext {context}"));
                }
                if let (Some(prompt), Some(completion)) =
                    (model.prompt_price, model.completion_price)
                {
                    line.push_str(&format!(
                        "\t${:.2}/${:.2} per M tokens",
                        prompt * 1e6,
                        completion * 1e6
                    ));
                }
                println!("{line}");
            }
        }
        Some(Commands::Providers { name: None }) => {
            println!("PROVIDER\tSOURCE\tKEY\tBASE URL");
            for (name, provider) in providers::all(&settings.providers) {
                let key = if provider.api_key_env.is_empty() {
                    "none needed".into()
                } else {
                    let set =
                        std::env::var_os(&provider.api_key_env).is_some_and(|v| !v.is_empty());
                    format!(
                        "{} {}",
                        provider.api_key_env,
                        if set { "set" } else { "unset" }
                    )
                };
                println!(
                    "{name}\t{}\t{key}\t{}",
                    if settings.providers.contains_key(&name) {
                        "configured"
                    } else {
                        "built-in"
                    },
                    provider.base_url
                );
            }
            println!("\n{}", providers::note());
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
            notify,
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
            // Provider prices for estimates; never delays the run, and an
            // offline lookup just keeps the cache.
            let routed = cost::routed_providers(&run);
            if !routed.is_empty() {
                let project = project.clone();
                std::thread::spawn(move || cost::refresh_provider_prices(&project, &routed));
            }
            cli_drive(&mut run, &cancel, notify::cli_enabled(notify))?;
        }
        Some(Commands::Resume {
            id,
            retry,
            interactive,
            model,
            advance,
            allow_repair,
            retry_candidate,
            notify,
            budget_usd,
            budget_tokens,
            release_groups,
        }) => {
            let mut run = engine::load(&project, &id)?;
            if release_groups {
                let released = engine::release_groups(&mut run)?;
                if released.is_empty() {
                    println!("Run {id} has no process groups to release");
                }
                for group in released {
                    println!(
                        "Released process group {} without signalling it; it no longer blocks run {id}",
                        group.pgid
                    );
                }
            }
            // What recovery stopped or left running.
            engine::authorize_replay(&mut run, cli.config.as_deref(), |event| {
                if let Event::Output(text) = event {
                    print!("{text}");
                    let _ = io::stdout().flush();
                }
            })?;
            if let Some(target) = retry_candidate {
                let (stage, candidate) = target
                    .split_once('/')
                    .context("Use --retry-candidate STAGE/CANDIDATE, e.g. implement/c2")?;
                run.command(Cmd::RetryCandidate {
                    stage: stage.into(),
                    candidate: candidate.into(),
                })?;
            }
            if budget_usd.is_some() || budget_tokens.is_some() {
                run.command(Cmd::RaiseBudget {
                    usd: budget_usd,
                    tokens: budget_tokens,
                })?;
            }
            if advance {
                run.command(Cmd::Confirm)?;
            }
            if allow_repair {
                run.command(Cmd::AllowRepair)?;
            }
            if retry || interactive || model.is_some() {
                run.command(Cmd::Retry { interactive, model })?;
            }
            cli_drive(&mut run, &cancel, notify::cli_enabled(notify))?;
        }
        Some(Commands::Cost { since, json }) => {
            let since = since
                .as_deref()
                .map(cost::parse_since)
                .transpose()?
                .map(|age| chrono::Utc::now() - age);
            let runs = engine::history(&project)?;
            let mut routed = std::collections::BTreeMap::new();
            for run in &runs {
                routed.extend(cost::routed_providers(run));
            }
            cost::refresh_provider_prices(&project, &routed);
            let summary = cost::aggregate(&runs, since, &cost::Pricing::load(&project));
            if json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                print!("{}", cost::render_summary(&summary));
            }
        }
        Some(Commands::Ship {
            id,
            push,
            draft_pr,
            yes,
        }) => ship_command(&project, &id, push, draft_pr, yes)?,
        Some(Commands::Rerun {
            id,
            worktree,
            unattended,
            notify,
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
            cli_drive(&mut run, &cancel, notify::cli_enabled(notify))?;
        }
        Some(Commands::Fork {
            id,
            stage,
            model,
            unattended,
            no_run,
            notify,
        }) => {
            let mut run = history::fork(&project, &settings, &id, &stage, model)?;
            if unattended {
                for stage in &mut run.workflow.stages {
                    stage.mode = Mode::Unattended;
                }
                run.persist()?;
            }
            let fork = run.fork_of.as_ref().expect("fork records its source");
            println!(
                "Run: {}\nFork of: {id} at stage {} (attempt {} snapshot {})\nWorktree: {}\nArtifacts: {}",
                run.id,
                fork.stage,
                fork.attempt,
                &fork.snapshot[..fork.snapshot.len().min(10)],
                run.workspace.display(),
                run.directory.display()
            );
            if no_run {
                println!(
                    "Not started. Run it with: model-router -C {} resume {}",
                    shell_words::quote(&project.display().to_string()),
                    run.id
                );
            } else {
                cli_drive(&mut run, &cancel, notify::cli_enabled(notify))?;
            }
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
                let origin = history::source_of(&run).context("This run is not a rerun or fork")?;
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
        Some(Commands::Handoffs {
            id,
            stage,
            prompt,
            json,
        }) => {
            let run = engine::load(&project, &id)?;
            let inspection = engine::inspect_handoffs(&run, stage.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&inspection)?);
            } else {
                print!("{}", inspection.text());
                if prompt {
                    println!("\n----- prompt -----\n{}", inspection.prompt);
                }
            }
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

fn ship_command(project: &Path, id: &str, push: bool, draft_pr: bool, yes: bool) -> Result<()> {
    use model_router::{ship, storage};
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

/// The run's verification receipts, after a blank line; nothing without any.
fn print_receipts(run: &Run) {
    let text = model_router::report::render_receipts(&model_router::report::receipts(run));
    if !text.is_empty() {
        print!("\n{text}");
    }
}

fn parse_role(text: &str) -> std::result::Result<Role, String> {
    Role::ALL
        .into_iter()
        .find(|role| role.label() == text)
        .ok_or_else(|| {
            let names: Vec<_> = Role::ALL.iter().map(|r| r.label()).collect();
            format!("expected one of {}", names.join(", "))
        })
}

fn parse_rate(text: &str) -> std::result::Result<f64, String> {
    let rate: f64 = text.parse().map_err(|_| "expected a number".to_owned())?;
    if (0.0..=1.0).contains(&rate) {
        Ok(rate)
    } else {
        Err("expected a fraction from 0 to 1, e.g. 0.8".into())
    }
}

fn stats_command(
    project: &Path,
    role: Option<Role>,
    json: bool,
    all: bool,
    min_pass: Option<f64>,
) -> Result<()> {
    use model_router::stats::{self, Stats};
    let stats = Stats::collect(&stats::projects(project, all));
    let shortlist = min_pass.map(|threshold| {
        let rows: Vec<stats::Tradeoff> = stats
            .metrics
            .iter()
            .filter(|m| role.is_none_or(|r| m.role == r))
            .map(stats::Tradeoff::from)
            .collect();
        stats::shortlist(&rows, threshold)
    });
    if json {
        let mut value = serde_json::to_value(&stats)?;
        if let Some(role) = role {
            value["metrics"] = serde_json::to_value(
                stats
                    .metrics
                    .iter()
                    .filter(|m| m.role == role)
                    .collect::<Vec<_>>(),
            )?;
        }
        if let Some(shortlist) = &shortlist {
            value["shortlist"] = serde_json::to_value(shortlist)?;
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        print!("{}", stats.table(role));
        if let Some(shortlist) = &shortlist {
            print!("\n{}", shortlist.text());
        }
    }
    Ok(())
}

fn bench_command(
    project: &Path,
    settings: &Settings,
    file: &Path,
    options: &model_router::bench::Options,
    dry_run: bool,
    min_pass: Option<f64>,
    cancel: &AtomicBool,
) -> Result<()> {
    use model_router::bench;
    let plan = bench::Plan::new(file, &bench::load(file)?, settings, options)?;
    print!("{}", plan.preview());
    if dry_run {
        println!("\nDry run: nothing started.");
        return Ok(());
    }
    println!();
    let results = bench::run(project, settings, &plan, options, cancel, &|line| {
        eprintln!("{line}")
    })?;
    let (json, markdown) = bench::save(project, &results)?;
    println!("{}", bench::markdown(&results));
    if let Some(threshold) = min_pass {
        let text = bench::shortlist(&results, threshold).text();
        println!("{text}");
        let mut file = std::fs::OpenOptions::new().append(true).open(&markdown)?;
        write!(file, "\n```text\n{text}```\n")?;
    }
    println!("Saved {}\nSaved {}", json.display(), markdown.display());
    ensure!(
        !cancel.load(Ordering::Relaxed),
        "Bench cancelled; unfinished cells are marked skipped"
    );
    Ok(())
}

/// Lines queued for printing while the engine runs. A stalled stdout (a
/// paused pager or pipe) fills it, and further lines are dropped and
/// counted, so the engine's timeouts and cancellation keep working.
const PRINT_QUEUE: usize = 1024;

enum Line {
    Out(String),
    Err(String),
}

/// `engine::drive`, printing its events from another thread.
fn drive_printing(run: &mut Run, cancel: &AtomicBool) -> Result<Action> {
    let dropped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (tx, rx) = mpsc::sync_channel::<Line>(PRINT_QUEUE);
    let counter = dropped.clone();
    let printer = std::thread::spawn(move || {
        let note = |count: usize| {
            println!(
                "[{count} output updates dropped while output was blocked; see the attempt's stdout.log]"
            );
        };
        for line in rx {
            let missed = counter.swap(0, Ordering::Relaxed);
            if missed > 0 {
                note(missed);
            }
            match line {
                Line::Out(text) => {
                    print!("{text}");
                    let _ = io::stdout().flush();
                }
                Line::Err(text) => eprintln!("{text}"),
            }
        }
        let missed = counter.swap(0, Ordering::Relaxed);
        if missed > 0 {
            note(missed);
        }
    });
    let action = engine::drive(run, cancel, |event| {
        let line = match event {
            Event::State(run) => {
                Line::Err(format!("[{}] {}", run.status.label(), run.current_name()))
            }
            Event::Output(text) => Line::Out(text),
        };
        if tx.try_send(line).is_err() {
            dropped.fetch_add(1, Ordering::Relaxed);
        }
    });
    drop(tx);
    let _ = printer.join();
    action
}

/// `model-router trust`: approve the project config's dangerous sections as
/// they are now, or revoke the approval.
fn trust_command(project: &Path, explicit: Option<&Path>, yes: bool, revoke: bool) -> Result<()> {
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

fn cli_drive(run: &mut Run, cancel: &AtomicBool, notify: bool) -> Result<()> {
    loop {
        let action = drive_printing(run, cancel);
        if notify {
            let outcome = action.as_ref().map(|a| *a).map_err(|e| format!("{e:#}"));
            notify::notify_outcome(run, &outcome);
        }
        let action = action?;
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
                print_receipts(run);
                return Ok(());
            }
            Action::Stopped => {
                engine::ensure_runnable(run)?;
                bail!("Run stopped");
            }
            Action::Interactive => {
                if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                    bail!(
                        "Stage {} is interactive. Resume {} from a terminal",
                        run.current_name(),
                        run.id
                    );
                }
                if cli_interactive(run, cancel)? {
                    println!(
                        "{} keeps running in the background. Reattach with: model-router -C {} resume {}",
                        run.current_name(),
                        shell_words::quote(&run.project.display().to_string()),
                        run.id
                    );
                    return Ok(());
                }
            }
            Action::Confirm if run.workflow.stages[run.cursor].role == Role::Ship => {
                // The preview was printed as stage output.
                ensure!(
                    io::stdin().is_terminal(),
                    "The ship plan awaits confirmation. Use resume {} --advance to ship it",
                    run.id
                );
                print!("[a] Ship as previewed  [r] Plan again  [q] Save and stop: ");
                io::stdout().flush()?;
                let mut answer = String::new();
                io::stdin().read_line(&mut answer)?;
                match answer.trim() {
                    "a" => run.command(Cmd::Confirm)?,
                    "r" => run.command(Cmd::Retry {
                        interactive: false,
                        model: None,
                    })?,
                    _ => {
                        println!(
                            "Saved. Ship with: model-router -C {} resume {} --advance",
                            shell_words::quote(&run.project.display().to_string()),
                            run.id
                        );
                        return Ok(());
                    }
                }
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
                    "a" => run.command(Cmd::Confirm)?,
                    "r" => run.command(Cmd::Retry {
                        interactive: true,
                        model: None,
                    })?,
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

/// True when the harness was left running in the background (`d`).
fn cli_interactive(run: &mut Run, cancel: &AtomicBool) -> Result<bool> {
    use process::{AttachOutcome, InteractiveSession};

    let (cols, rows) = crossterm::terminal::size().unwrap_or((100, 30));
    let session = InteractiveSession::new(cols, rows);
    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::sync_channel(128);
        let session = &session;
        let worker = scope.spawn(move || {
            engine::run_interactive(run, cancel, session, |event| {
                let _ = tx.try_send(event);
            })
        });
        let foreground = (|| -> Result<()> {
            let mut finished = matches!(session.attach(cancel, cancel)?, AttachOutcome::Finished);
            crossterm::terminal::enable_raw_mode()?;
            struct RestoreTerminal;
            impl Drop for RestoreTerminal {
                fn drop(&mut self) {
                    let _ = crossterm::terminal::disable_raw_mode();
                }
            }
            let _terminal = RestoreTerminal;
            if !finished {
                write!(
                    io::stdout(),
                    "\r\nHarness still running. [t] Reattach  [d] Leave running in the background  [c] Cancel  [q] Stop\r\n"
                )?;
                io::stdout().flush()?;
            }
            while !worker.is_finished() {
                cli_interactive_events(&rx)?;
                let mut input = libc::pollfd {
                    fd: 0,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: stdin is borrowed; input is one initialized pollfd.
                let ready = unsafe { libc::poll(&mut input, 1, 50) };
                if ready < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error.into());
                }
                ensure!(
                    input.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) == 0,
                    "Terminal disconnected while waiting for the harness"
                );
                if input.revents & libc::POLLIN == 0 {
                    continue;
                }
                let mut bytes = [0u8; 64];
                // SAFETY: bytes is writable and stdin was reported readable.
                let count = unsafe { libc::read(0, bytes.as_mut_ptr().cast(), bytes.len()) };
                if count < 0 {
                    let error = io::Error::last_os_error();
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) {
                        continue;
                    }
                    return Err(error.into());
                }
                ensure!(
                    count != 0,
                    "Terminal input closed while waiting for the harness"
                );
                let keys = &bytes[..count as usize];
                if keys
                    .iter()
                    .any(|key| matches!(key, b'c' | b'q' | 0x03 | 0x1d))
                {
                    cancel.store(true, Ordering::Relaxed);
                    write!(io::stdout(), "\r\nStopping the stage...\r\n")?;
                    io::stdout().flush()?;
                } else if keys.contains(&b'd') && !finished && !cancel.load(Ordering::Relaxed) {
                    session.release();
                    write!(io::stdout(), "\r\nLeaving the harness running...\r\n")?;
                    io::stdout().flush()?;
                } else if keys.contains(&b't') && !finished && !cancel.load(Ordering::Relaxed) {
                    crossterm::terminal::disable_raw_mode()?;
                    finished = matches!(session.attach(cancel, cancel)?, AttachOutcome::Finished);
                    crossterm::terminal::enable_raw_mode()?;
                    if !finished {
                        write!(
                            io::stdout(),
                            "\r\nHarness still running. [t] Reattach  [d] Leave running in the background  [c] Cancel  [q] Stop\r\n"
                        )?;
                        io::stdout().flush()?;
                    }
                }
            }
            cli_interactive_events(&rx)?;
            crossterm::terminal::disable_raw_mode()?;
            Ok(())
        })();
        // Never join a live child after losing the foreground terminal.
        if foreground.is_err() {
            cancel.store(true, Ordering::Relaxed);
        }
        let result = worker
            .join()
            .map_err(|_| anyhow::anyhow!("Interactive worker panicked"))?;
        foreground?;
        result.map(|()| session.released())
    })
}

fn cli_interactive_events(events: &mpsc::Receiver<Event>) -> Result<()> {
    let mut stdout = io::stdout().lock();
    for event in events.try_iter() {
        match event {
            Event::State(run) => {
                write!(
                    stdout,
                    "\r\n[{}] {}\r\n",
                    run.status.label(),
                    run.current_name()
                )?;
            }
            Event::Output(text) => {
                // Raw mode disables newline translation. Keep check output readable.
                for line in text.split_inclusive('\n') {
                    if let Some(line) = line.strip_suffix('\n') {
                        stdout.write_all(line.as_bytes())?;
                        stdout.write_all(b"\r\n")?;
                    } else {
                        stdout.write_all(line.as_bytes())?;
                    }
                }
            }
        }
    }
    stdout.flush()?;
    Ok(())
}

/// `harnesses`: a table, or with `--spec` the resolved specs as JSON.
fn harnesses_command(settings: &Settings, spec: Option<&str>) -> Result<()> {
    use model_router::{adapters, harness};
    let names = catalog::known_harnesses(settings);
    let resolved = |name: &str| -> Result<serde_json::Value> {
        if let Some(spec) = harness::get(name) {
            let mut value = serde_json::to_value(spec)?;
            value["builtin"] = true.into();
            value["program"] = adapters::program(name, &settings.adapters).into();
            return Ok(value);
        }
        let adapter = settings
            .adapters
            .get(name)
            .with_context(|| format!("Unknown harness {name}; see `model-router harnesses`"))?;
        let mut value = serde_json::to_value(adapter)?;
        value["name"] = name.into();
        value["builtin"] = false.into();
        value["output"] = adapters::output_kind(name, &settings.adapters)
            .name()
            .into();
        value["resumable"] = adapters::resumable(name, &settings.adapters).into();
        Ok(value)
    };
    match spec {
        Some("") => {
            let all = names
                .iter()
                .map(|name| resolved(name))
                .collect::<Result<Vec<_>>>()?;
            println!("{}", serde_json::to_string_pretty(&all)?);
        }
        Some(name) => println!("{}", serde_json::to_string_pretty(&resolved(name)?)?),
        None => {
            println!("HARNESS\tKIND\tPROGRAM\tOUTPUT\tRESUME");
            for name in &names {
                println!(
                    "{name}\t{}\t{}\t{}\t{}",
                    if harness::get(name).is_some() {
                        "built-in"
                    } else {
                        "custom"
                    },
                    adapters::program(name, &settings.adapters),
                    adapters::output_kind(name, &settings.adapters).name(),
                    if adapters::resumable(name, &settings.adapters) {
                        "yes"
                    } else {
                        "no"
                    }
                );
            }
        }
    }
    Ok(())
}
