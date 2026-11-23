mod cli;

use anyhow::{Result, bail, ensure};
use clap::{Parser, Subcommand};
use model_router::{
    catalog,
    config::{self, Role},
    engine,
    history::StatusFilter,
    presets, process, trust, tui,
};
use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicPtr, Ordering},
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
    /// Append structured JSONL metrics (run and stage attempt events) to PATH.
    /// Also MODEL_ROUTER_METRICS_FILE.
    #[arg(long, global = true, value_name = "PATH")]
    metrics_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Clone)]
enum SchedulesAction {
    /// This project's schedules, with approval and next due time.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Create a schedule from the CLI (pass --approve to approve immediately on a trusted project).
    Add {
        /// Optional schedule ID (letters, digits, '-' and '_', up to 64 chars).
        #[arg(long)]
        id: Option<String>,
        /// Workflow name; defaults to the first workflow in config.
        #[arg(short, long)]
        workflow: Option<String>,
        #[arg(long, conflicts_with = "task_file")]
        task: Option<String>,
        /// Read the task from a local Markdown or UTF-8 text file (up to 32 KiB).
        #[arg(long)]
        task_file: Option<PathBuf>,
        /// Fire every DUR (e.g. 30m, 6h, 1d, 2w; at least 5m).
        #[arg(long, value_name = "DUR")]
        every: Option<String>,
        /// Fire daily at HH:MM (in --tz, default local).
        #[arg(long, value_name = "HH:MM")]
        at: Option<String>,
        /// Fire on a 5-field cron expression ("M H DOM MON DOW").
        #[arg(long, value_name = "EXPR")]
        cron: Option<String>,
        /// Fire on POST /schedules/<id>/trigger to `serve --http`.
        #[arg(long)]
        webhook: bool,
        /// Timezone for --at or --cron: local (default), UTC, or fixed offset like +02:00 / -07:00.
        #[arg(long, value_name = "TZ", allow_hyphen_values = true)]
        tz: Option<String>,
        /// Approve the schedule immediately (requires a trusted project config).
        #[arg(long)]
        approve: bool,
    },
    /// Approve a schedule as it stands now; any later edit asks again.
    Approve {
        id: String,
        /// Approve only if the schedule still has this digest (from
        /// `schedules list --json` or the agent's `schedule_task` result).
        #[arg(long, value_name = "DIGEST")]
        digest: Option<String>,
    },
    /// Pause a schedule (its approval is forgotten).
    Pause { id: String },
    /// Enable a paused schedule; it then needs approval again.
    Enable { id: String },
    /// Delete a schedule permanently.
    Delete { id: String },
    /// Fire every approved schedule that is due and drive its runs to an
    /// end; exits non-zero when one failed.
    Tick,
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
        /// Detect installed harnesses, adapt stages to them, and leave stage
        /// models blank to inherit each harness's default.
        #[arg(long)]
        detect: bool,
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
        /// Include every recent project, not only this one.
        #[arg(long)]
        all_projects: bool,
    },
    /// Apply a finished worktree run's changes back into the project checkout
    /// without committing or touching Git's index or branches.
    Apply {
        id: String,
        /// Check whether the patch applies (cleanly or via 3-way merge) without
        /// modifying the checkout or index.
        #[arg(long)]
        check: bool,
        /// Apply without asking after the preview.
        #[arg(long)]
        yes: bool,
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
        #[arg(long, value_parser = cli::parse_role)]
        role: Option<Role>,
        #[arg(long)]
        json: bool,
        /// Include every recent project, not only this one.
        #[arg(long)]
        all_projects: bool,
        /// Add an advisory cost-quality shortlist: assignments whose
        /// required router checks passed at least this fraction of the time
        /// (0–1), cheapest known cost then fastest first.
        #[arg(long, value_name = "RATE", value_parser = cli::parse_rate)]
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
        #[arg(long, value_name = "RATE", value_parser = cli::parse_rate)]
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
    /// Manage scheduled runs: list, add, approve, pause, enable, delete, or
    /// fire due schedules (`tick`, for cron or systemd timers).
    Schedules {
        #[command(subcommand)]
        action: Option<SchedulesAction>,
    },
    /// Check harness executables, versions, pins, provider keys, and flags without a model request.
    Doctor {
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// Fail on missing harness flags, version pin drift, or missing configured provider keys.
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
    // The stdio MCP bridge a delegating harness starts (see
    // `delegation::bridge`); never typed by people either.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == model_router::delegation::broker::BRIDGE_ARG)
    {
        std::process::exit(model_router::delegation::bridge::run());
    }
    if let Err(error) = main_result() {
        // Terminal loss can close stderr too; reporting it must not panic.
        let _ = writeln!(io::stderr(), "Error: {error:#}");
        std::process::exit(1);
    }
}

static CANCEL_FLAG: AtomicPtr<AtomicBool> = AtomicPtr::new(std::ptr::null_mut());

extern "C" fn handle_termination_signal(_: libc::c_int) {
    let ptr = CANCEL_FLAG.load(Ordering::Acquire);
    if !ptr.is_null() {
        // SAFETY: `register_cancel_flag` leaks a cloned `Arc<AtomicBool>` once
        // for the process lifetime, and `AtomicBool::store` is lock-free and
        // async-signal-safe.
        unsafe { &*ptr }.store(true, Ordering::Relaxed);
    }
}

fn register_cancel_flag(cancel: &Arc<AtomicBool>) {
    let raw = Arc::into_raw(cancel.clone()) as *mut AtomicBool;
    if CANCEL_FLAG
        .compare_exchange(
            std::ptr::null_mut(),
            raw,
            Ordering::Release,
            Ordering::Relaxed,
        )
        .is_err()
    {
        // SAFETY: `raw` was just produced by `Arc::into_raw` and never published.
        unsafe { drop(Arc::from_raw(raw)) };
    }
}

fn install_signal_handlers(cancel: &Arc<AtomicBool>) -> Result<()> {
    register_cancel_flag(cancel);
    // SAFETY: `action` is zero-initialized, `sa_mask` is emptied via
    // `sigemptyset`, and `handle_termination_signal` is async-signal-safe.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handle_termination_signal as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = libc::SA_RESTART;
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            ensure!(
                libc::sigaction(sig, &action, std::ptr::null_mut()) == 0,
                "Register signal handler for {sig}: {}",
                io::Error::last_os_error()
            );
        }
    }
    Ok(())
}

/// From here on Ctrl+C ends only child processes (Git, `gh`): the command
/// itself finishes and reports what it already did.
pub(crate) fn defer_interrupts() -> Result<()> {
    install_signal_handlers(&Arc::new(AtomicBool::new(false)))
}

/// From here on the process and every child it starts ignore Ctrl+C, hangup,
/// and SIGTERM, so a short operation that a killed child would misreport
/// (`git apply` read as a conflict) runs to its end.
pub(crate) fn ignore_interrupts() -> Result<()> {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: SIG_IGN installs no handler code.
        ensure!(
            unsafe { libc::signal(sig, libc::SIG_IGN) } != libc::SIG_ERR,
            "Ignore signal {sig}: {}",
            io::Error::last_os_error()
        );
    }
    Ok(())
}

fn main_result() -> Result<()> {
    let cli = Cli::parse();
    if let Some(path) = &cli.metrics_file {
        model_router::events::set_metrics_file(Some(path.clone()));
    }
    match &cli.command {
        Some(Commands::Presets) => {
            print!("{}", presets::listing());
            return Ok(());
        }
        Some(Commands::Completions { shell, bin }) => {
            return cli::manage::completions::<Cli>(*shell, bin);
        }
        _ => {}
    }
    let cwd = catalog::check_project(Path::new("."))?;
    let project = match &cli.project {
        Some(path) => catalog::check_project(path)?,
        None => catalog::detect_project(&cwd)?,
    };
    // Tell the TUI when the project was found above the current directory.
    let detected_from = (cli.project.is_none() && project != cwd).then_some(cwd);
    let cancel = Arc::new(AtomicBool::new(false));
    // Only commands that watch `cancel` trade Ctrl+C's default exit for it.
    if matches!(
        cli.command,
        None | Some(
            Commands::Run { .. }
                | Commands::Resume { .. }
                | Commands::Rerun { .. }
                | Commands::Fork { .. }
                | Commands::Bench { .. }
                | Commands::Serve(_)
                | Commands::Schedules {
                    action: Some(SchedulesAction::Tick)
                }
        )
    ) {
        install_signal_handlers(&cancel)?;
    }
    match &cli.command {
        Some(Commands::Init {
            global,
            force,
            preset,
            detect,
        }) => {
            return cli::manage::init(
                &project,
                cli.config.as_deref(),
                *global,
                *force,
                preset.as_deref(),
                *detect,
            );
        }
        Some(Commands::Trust { yes, revoke }) => {
            return cli::manage::trust(&project, cli.config.as_deref(), *yes, *revoke);
        }
        Some(Commands::Schedules { action }) => {
            return cli::manage::schedules(
                &project,
                action
                    .clone()
                    .map_or(
                        cli::manage::SchedulesCommand::List { json: false },
                        |a| match a {
                            SchedulesAction::List { json } => {
                                cli::manage::SchedulesCommand::List { json }
                            }
                            SchedulesAction::Add {
                                id,
                                workflow,
                                task,
                                task_file,
                                every,
                                at,
                                cron,
                                webhook,
                                tz,
                                approve,
                            } => cli::manage::SchedulesCommand::Add {
                                id,
                                workflow,
                                task,
                                task_file,
                                every,
                                at,
                                cron,
                                webhook,
                                tz,
                                approve,
                            },
                            SchedulesAction::Approve { id, digest } => {
                                cli::manage::SchedulesCommand::Approve { id, digest }
                            }
                            SchedulesAction::Pause { id } => {
                                cli::manage::SchedulesCommand::Pause(id)
                            }
                            SchedulesAction::Enable { id } => {
                                cli::manage::SchedulesCommand::Enable(id)
                            }
                            SchedulesAction::Delete { id } => {
                                cli::manage::SchedulesCommand::Delete(id)
                            }
                            SchedulesAction::Tick => cli::manage::SchedulesCommand::Tick,
                        },
                    ),
                &cancel,
            );
        }
        _ => {}
    }
    if let Some(Commands::Serve(args)) = cli.command {
        // Loads each project's config per call; the signal flag stops it.
        return model_router::serve::run(args, project, cli.config, cancel);
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
            )
        }
        Some(
            Commands::Init { .. }
            | Commands::Presets
            | Commands::Completions { .. }
            | Commands::Trust { .. }
            | Commands::Schedules { .. }
            | Commands::Serve(_),
        ) => unreachable!(),
        Some(Commands::Validate) => cli::manage::validate(&settings, &config_path, review.as_ref()),
        Some(Commands::Report {
            id,
            json,
            diff,
            transitions,
        }) => cli::inspect::report_command(&project, &id, json, diff, transitions),
        Some(Commands::Preview { workflow, task }) => {
            println!(
                "{}",
                engine::preview(
                    &settings,
                    settings.workflow(workflow.as_deref())?,
                    &project,
                    &task
                )?
            );
            Ok(())
        }
        Some(Commands::Doctor { json, strict }) => {
            cli::inspect::doctor_command(&project, &settings, json, strict)
        }
        Some(Commands::Stats {
            role,
            json,
            all_projects,
            min_pass,
        }) => cli::inspect::stats_command(&project, role, json, all_projects, min_pass),
        Some(Commands::Bench {
            file,
            jobs,
            dry_run,
            allow_interactive,
            min_pass,
        }) => cli::inspect::bench_command(
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
        ),
        Some(Commands::Harnesses { spec }) => {
            cli::inspect::harnesses_command(&settings, spec.as_deref())
        }
        Some(Commands::Models { harness }) => {
            cli::inspect::models_command(&project, &settings, &harness)
        }
        Some(Commands::Providers { name }) => {
            cli::inspect::providers_command(&settings, name.as_deref())
        }
        Some(Commands::History {
            status,
            search,
            limit,
            json,
            sizes,
        }) => {
            cli::inspect::history_command(&project, status, search.as_deref(), limit, json, sizes)
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
        }) => cli::run::run_command(
            &project,
            &settings,
            workflow.as_deref(),
            task,
            task_file,
            task_url,
            worktree,
            unattended,
            with_review,
            notify,
            &cancel,
        ),
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
        }) => cli::run::resume_command(
            &project,
            cli.config.as_deref(),
            &id,
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
            &cancel,
        ),
        Some(Commands::Cost {
            since,
            json,
            all_projects,
        }) => cli::inspect::cost_command(&project, since.as_deref(), json, all_projects),
        Some(Commands::Apply { id, check, yes }) => cli::manage::apply(&project, &id, check, yes),
        Some(Commands::Ship {
            id,
            push,
            draft_pr,
            yes,
        }) => cli::manage::ship(&project, &id, push, draft_pr, yes),
        Some(Commands::Rerun {
            id,
            worktree,
            unattended,
            notify,
        }) => cli::run::rerun_command(
            &project, &settings, &id, worktree, unattended, notify, &cancel,
        ),
        Some(Commands::Fork {
            id,
            stage,
            model,
            unattended,
            no_run,
            notify,
        }) => cli::run::fork_command(
            &project, &settings, &id, &stage, model, unattended, no_run, notify, &cancel,
        ),
        Some(Commands::Compare {
            id,
            stage,
            attempts,
            source,
        }) => cli::inspect::compare_command(&project, &id, stage.as_deref(), &attempts, source),
        Some(Commands::Handoffs {
            id,
            stage,
            prompt,
            json,
        }) => cli::inspect::handoffs_command(&project, &id, stage.as_deref(), prompt, json),
        Some(Commands::Clean {
            older_than,
            status,
            keep,
            worktrees,
            yes,
        }) => cli::manage::clean(&project, older_than, status, keep, worktrees, yes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn termination_signal_handler_sets_and_allows_resetting_cancel_flag() {
        let cancel = Arc::new(AtomicBool::new(false));
        register_cancel_flag(&cancel);
        assert!(!cancel.load(Ordering::Relaxed));

        handle_termination_signal(libc::SIGTERM);
        assert!(cancel.load(Ordering::Relaxed));

        // Matches TUI resetting the cancel flag after returning from an external editor.
        cancel.store(false, Ordering::Relaxed);
        handle_termination_signal(libc::SIGHUP);
        assert!(cancel.load(Ordering::Relaxed));
    }
}
