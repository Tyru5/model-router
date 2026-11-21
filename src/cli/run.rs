use super::inspect::print_receipts;
use anyhow::{Context, Result, bail, ensure};
use model_router::{
    config::{Mode, Role, Settings},
    cost,
    engine::{self, Action, Cmd, Event, Run, Status},
    history, notify, process, report, task_import,
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

#[allow(clippy::too_many_arguments)]
pub fn run_command(
    project: &Path,
    settings: &Settings,
    workflow: Option<&str>,
    task: Option<String>,
    task_file: Option<PathBuf>,
    task_url: Option<String>,
    worktree: bool,
    unattended: bool,
    with_review: bool,
    notify_flag: bool,
    cancel: &AtomicBool,
) -> Result<()> {
    let task = match (task, task_file, task_url) {
        (Some(task), _, _) => task,
        (_, Some(path), _) => task_import::read(&path)?,
        (_, _, Some(url)) => task_import::read_url(&url, cancel)?,
        _ => bail!("Supply --task, --task-file, or --task-url"),
    };
    let mut workflow = settings.workflow(workflow)?.clone();
    for stage in &mut workflow.stages {
        if unattended {
            stage.mode = Mode::Unattended;
        }
        if with_review && stage.role == Role::Review {
            stage.enabled = true;
        }
    }
    let mut run = Run::create(project, settings, &workflow, task, worktree)?;
    println!("Run: {}\nArtifacts: {}", run.id, run.directory.display());
    // Provider prices for estimates; never delays the run, and an
    // offline lookup just keeps the cache.
    let routed = cost::routed_providers(&run);
    if !routed.is_empty() {
        let project = project.to_path_buf();
        std::thread::spawn(move || cost::refresh_provider_prices(&project, &routed));
    }
    cli_drive(&mut run, cancel, notify::cli_enabled(notify_flag))
}

#[allow(clippy::too_many_arguments)]
pub fn resume_command(
    project: &Path,
    explicit_config: Option<&Path>,
    id: &str,
    retry: bool,
    interactive: bool,
    model: Option<String>,
    advance: bool,
    allow_repair: bool,
    retry_candidate: Option<String>,
    notify_flag: bool,
    budget_usd: Option<f64>,
    budget_tokens: Option<u64>,
    release_groups: bool,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut run = engine::load(project, id)?;
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
    engine::authorize_replay(&mut run, explicit_config, |event| {
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
    cli_drive(&mut run, cancel, notify::cli_enabled(notify_flag))
}

pub fn rerun_command(
    project: &Path,
    settings: &Settings,
    id: &str,
    worktree: bool,
    unattended: bool,
    notify_flag: bool,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut run = history::rerun(project, settings, id, worktree.then_some(true))?;
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
    cli_drive(&mut run, cancel, notify::cli_enabled(notify_flag))
}

#[allow(clippy::too_many_arguments)]
pub fn fork_command(
    project: &Path,
    settings: &Settings,
    id: &str,
    stage: &str,
    model: Option<String>,
    unattended: bool,
    no_run: bool,
    notify_flag: bool,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut run = history::fork(project, settings, id, stage, model)?;
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
        fork.snapshot.get(..10).unwrap_or(&fork.snapshot),
        run.workspace.display(),
        run.directory.display()
    );
    if no_run {
        println!(
            "Not started. Run it with: model-router -C {} resume {}",
            shell_words::quote(&project.display().to_string()),
            run.id
        );
        Ok(())
    } else {
        cli_drive(&mut run, cancel, notify::cli_enabled(notify_flag))
    }
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
        let mut stdout_broken = false;
        let mut stderr_broken = false;
        let write_out = |text: &str, broken: &mut bool| {
            if *broken {
                return;
            }
            let mut out = io::stdout().lock();
            if let Err(err) = out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
                if err.kind() == io::ErrorKind::BrokenPipe {
                    *broken = true;
                }
            }
        };
        let write_err = |text: &str, broken: &mut bool| {
            if *broken {
                return;
            }
            let mut err_out = io::stderr().lock();
            if let Err(err) = writeln!(err_out, "{text}") {
                if err.kind() == io::ErrorKind::BrokenPipe {
                    *broken = true;
                }
            }
        };
        let note = |count: usize, broken: &mut bool| {
            write_out(
                &format!(
                    "[{count} output updates dropped while output was blocked; see the attempt's stdout.log]\n"
                ),
                broken,
            );
        };
        for line in rx {
            let missed = counter.swap(0, Ordering::Relaxed);
            if missed > 0 {
                note(missed, &mut stdout_broken);
            }
            match line {
                Line::Out(text) => write_out(&text, &mut stdout_broken),
                Line::Err(text) => write_err(&text, &mut stderr_broken),
            }
        }
        let missed = counter.swap(0, Ordering::Relaxed);
        if missed > 0 {
            note(missed, &mut stdout_broken);
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

pub fn cli_drive(run: &mut Run, cancel: &AtomicBool, notify_flag: bool) -> Result<()> {
    // However this returns, let the outcome's webhook reach its endpoint.
    struct Webhooks;
    impl Drop for Webhooks {
        fn drop(&mut self) {
            notify::wait_for_webhooks();
        }
    }
    let _webhooks = Webhooks;
    loop {
        let action = drive_printing(run, cancel);
        if notify_flag {
            let outcome = action.as_ref().map(|a| *a).map_err(|e| format!("{e:#}"));
            notify::notify_outcome(run, &outcome);
        }
        let action = action?;
        match action {
            Action::Complete => {
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
