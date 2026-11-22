use crate::{config::Settings, process::AttachOutcome, storage};
use anyhow::Result;
use crossterm::{
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    io::{self, Stdout, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

mod actions;
mod app;
mod completion;
mod diff_view;
mod editor;
mod external;
mod frame_marker;
mod handoff_view;
mod harness_editor;
mod harnesses;
mod history;
mod input;
mod modal;
mod models;
mod monitor;
mod output_search;
mod path_picker;
mod run_slot;
mod session;
mod startup;
mod stats_page;
mod task_importer;
mod tasks;
#[cfg(test)]
mod tests;
mod theme;
mod tour;
pub mod trust_prompt;
mod ui;
mod widgets;
mod workflow_page;
use app::{App, draw};

struct ScreenOutput {
    stdout: Stdout,
    closed: Arc<AtomicBool>,
}

impl Write for ScreenOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.closed.load(Ordering::Relaxed) {
            Ok(bytes.len())
        } else {
            self.stdout.write(bytes)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            Ok(())
        } else {
            self.stdout.flush()
        }
    }
}

struct Screen {
    terminal: Terminal<CrosstermBackend<ScreenOutput>>,
    output_closed: Arc<AtomicBool>,
    active: bool,
}
impl Screen {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode()?;
        if let Err(e) = execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste) {
            restore_terminal();
            return Err(e.into());
        }
        let output_closed = Arc::new(AtomicBool::new(false));
        let terminal = match Terminal::new(CrosstermBackend::new(ScreenOutput {
            stdout: io::stdout(),
            closed: output_closed.clone(),
        })) {
            Ok(terminal) => terminal,
            Err(e) => {
                restore_terminal();
                return Err(e.into());
            }
        };
        let mut screen = Self {
            terminal,
            output_closed,
            active: true,
        };
        screen.terminal.clear()?;
        Ok(screen)
    }
    fn resume(&mut self) -> Result<()> {
        terminal::enable_raw_mode()?;
        self.active = true;
        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        self.terminal.clear()?;
        Ok(())
    }
    fn restore(&mut self) {
        if self.active {
            restore_terminal();
            self.active = false;
        }
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        self.restore();
        // Ratatui 0.29 retries cursor restoration in Drop and eprintln!s on failure.
        // After our best-effort restore, discard those duplicate writes so a lost
        // terminal (including stderr) cannot turn normal shutdown into a panic.
        self.output_closed.store(true, Ordering::Relaxed);
    }
}

/// Best-effort terminal cleanup shared between normal teardown, error paths,
/// and the TUI-thread panic hook.
fn restore_terminal() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        event::DisableMouseCapture,
        LeaveAlternateScreen,
        crossterm::style::ResetColor,
        crossterm::cursor::Show
    );
}

/// Restores the terminal when the TUI thread panics, and puts the previous
/// hook back when the TUI returns. Workers and tasks catch their own panics
/// and report them in the TUI, which keeps the screen, so a panic on another
/// thread leaves the terminal alone.
struct PanicHook {
    previous: Arc<PreviousHook>,
}
type PreviousHook = dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static;
impl PanicHook {
    fn install() -> Self {
        let previous: Arc<PreviousHook> = Arc::from(std::panic::take_hook());
        let chained = previous.clone();
        let tui = std::thread::current().id();
        std::panic::set_hook(Box::new(move |info| {
            on_panic(tui, &restore_terminal);
            chained(info);
        }));
        Self { previous }
    }
}
impl Drop for PanicHook {
    fn drop(&mut self) {
        // Changing the hook while panicking aborts the process.
        if !std::thread::panicking() {
            let previous = self.previous.clone();
            std::panic::set_hook(Box::new(move |info| previous(info)));
        }
    }
}
/// Checks with `timeout = 0` whether any descriptor has hung up or errored.
#[cfg(unix)]
fn any_disconnected<const N: usize>(fds: [std::os::unix::io::RawFd; N]) -> bool {
    const LOST: libc::c_short = libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
    // macOS reports POLLHUP only for descriptors polled for input; Linux
    // reports it regardless. Readable input alone is not a disconnect.
    let mut pollfds = fds.map(|fd| libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    });
    // SAFETY: `pollfds` points to `N` initialized `libc::pollfd` structs.
    let ready = unsafe { libc::poll(pollfds.as_mut_ptr(), N as libc::nfds_t, 0) };
    ready > 0 && pollfds.iter().any(|p| p.revents & LOST != 0)
}

#[cfg(unix)]
fn terminal_disconnected() -> bool {
    any_disconnected([libc::STDIN_FILENO, libc::STDOUT_FILENO])
}

#[cfg(not(unix))]
fn terminal_disconnected() -> bool {
    false
}

/// Runs `restore` only on the TUI thread.
fn on_panic(tui: std::thread::ThreadId, restore: &dyn Fn()) {
    if std::thread::current().id() == tui {
        restore();
    }
}

pub fn run(
    project: PathBuf,
    settings: Settings,
    config_path: PathBuf,
    cancel: Arc<AtomicBool>,
    animate: bool,
    detected_from: Option<PathBuf>,
) -> Result<()> {
    let _hook = PanicHook::install();
    let mut app = App::new(project, settings, config_path, cancel);
    // Runs a stage's agent launches (and schedules fire) join this session.
    crate::engine::set_run_host(std::sync::Arc::new(tasks::TuiHost::new(
        app.tasks.spawner(),
    )));
    if let Some(cwd) = detected_from {
        app.ui.message = format!(
            "Project: {} (detected above {}). p changes it.",
            app.project.display(),
            cwd.display()
        );
    }
    if let Some(note) = trust_prompt::notice(&app.editor.settings) {
        app.ui.message = if app.ui.message.is_empty() {
            note
        } else {
            format!("{} · {note}", app.ui.message)
        };
    }
    match theme::load(&app.ui.theme_path) {
        Ok(preferences) => {
            app.ui.theme = preferences.theme;
            app.ui.vim_motions = preferences.vim_motions;
            crate::notify::global().set_backends(preferences.notifications.clone());
            app.ui.notifications = preferences.notifications;
        }
        Err(error) => {
            app.ui.message = format!("{error:#}. Using Model Router colours; T chooses a theme.");
        }
    }
    let mut screen = Screen::new()?;
    if animate
        && startup::claim()
        && !startup::play(&mut screen.terminal, &app.signal, app.ui.palette())?
    {
        return Ok(());
    }
    // Before the project is remembered: an empty list means a first launch.
    match tour::on_launch() {
        tour::Launch::Open => app.tour.open = true,
        tour::Launch::Offer => {
            app.ui.message = if app.ui.message.is_empty() {
                tour::OFFER.into()
            } else {
                format!("{} · {}", tour::OFFER, app.ui.message)
            };
        }
        tour::Launch::Quiet => {}
    }
    let _ = storage::remember_project(&app.project);
    app.adopt_detached();
    let mut drawn = Instant::now();
    'run: loop {
        let disconnected = terminal_disconnected();
        if disconnected || app.signal.load(Ordering::Relaxed) {
            app.session.cancel_all();
            app.session.quit_after_stop = true;
        }
        if app.poll()? {
            break;
        }
        if disconnected {
            // Terminal is gone: avoid drawing (EIO) and avoid spinning inside
            // crossterm's POLLHUP loop while waiting for active runs to cancel.
            std::thread::sleep(Duration::from_millis(25));
            continue;
        }
        if app.take_launch() {
            let result = match app.interactive_terminal() {
                Ok(session) => {
                    let cancel = app.session.focused().map(|slot| slot.cancel.clone());
                    let cancel = cancel.unwrap_or_default();
                    screen.restore();
                    let result = session.attach(&cancel, &app.signal);
                    screen.resume()?;
                    result
                }
                Err(error) => Err(error),
            };
            app.ui.message = match result {
                Ok(AttachOutcome::Detached) => {
                    "Interactive session detached and still running. [t] reattaches; [C] stops it. /exit quits the harness.".into()
                }
                Ok(AttachOutcome::Finished) => "Harness returned. Finishing stage…".into(),
                Err(error) => {
                    app.ui.alert = format!("{error:#}");
                    app.ui.alert.clone()
                }
            };
            // Apply completion or shutdown before accepting another action.
            app.ui.redraw = true;
            continue;
        }
        if let Some(launch) = app.ui.external.take() {
            // The pager or editor owns the terminal, so Ctrl+C there is its
            // own and must not stop the router.
            let signalled = app.signal.load(Ordering::Relaxed);
            screen.restore();
            let result = launch.run();
            screen.resume()?;
            if !signalled {
                app.signal.store(false, Ordering::Relaxed);
            }
            app.finish_external(result);
            continue;
        }
        // Frames are drawn when something changed, when an active elapsed timer
        // is on screen (1 Hz), or when a loading spinner animates (~12.5 Hz).
        let tick = if app.animating() {
            Some(Duration::from_millis(80))
        } else if app.ticking() {
            Some(Duration::from_secs(1))
        } else {
            None
        };
        if app.ui.redraw || tick.is_some_and(|interval| drawn.elapsed() >= interval) {
            // Notifications from worker threads wait for the frame.
            let _terminal = crate::notify::terminal_lock();
            screen.terminal.draw(|frame| draw(frame, &mut app))?;
            frame_marker::frame_drawn();
            app.ui.redraw = false;
            drawn = Instant::now();
        }
        if event::poll(app.poll_timeout())? {
            app.ui.redraw = true;
            // Handle all queued input before drawing again, so slow frames do
            // not make typing lag one event per frame.
            for _ in 0..64 {
                match event::read()? {
                    Event::Key(key) => match app.key(key) {
                        Ok(true) => break 'run,
                        Ok(false) => {}
                        Err(error) => {
                            app.ui.error(format!("{error:#}"));
                        }
                    },
                    Event::Paste(text) => app.paste(&text),
                    _ => {}
                }
                if app.session.launch_interactive
                    || app.ui.external.is_some()
                    || !event::poll(Duration::ZERO)?
                {
                    break;
                }
            }
        }
    }
    drop(screen);
    if !app.session.left_running.is_empty() {
        println!("Still running in the background:");
        for line in &app.session.left_running {
            println!("  {line}");
        }
        println!(
            "Run mr in {} to reattach.",
            shell_words::quote(&app.project.display().to_string())
        );
    }
    Ok(())
}
