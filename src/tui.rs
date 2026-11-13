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
            let _ = terminal::disable_raw_mode();
            return Err(e.into());
        }
        let output_closed = Arc::new(AtomicBool::new(false));
        let mut screen = Self {
            terminal: Terminal::new(CrosstermBackend::new(ScreenOutput {
                stdout: io::stdout(),
                closed: output_closed.clone(),
            }))?,
            output_closed,
            active: true,
        };
        screen.terminal.clear()?;
        Ok(screen)
    }
    fn resume(&mut self) -> Result<()> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        self.active = true;
        self.terminal.clear()?;
        Ok(())
    }
    fn restore(&mut self) {
        if self.active {
            let _ = terminal::disable_raw_mode();
            let _ = execute!(
                io::stdout(),
                DisableBracketedPaste,
                event::DisableMouseCapture,
                LeaveAlternateScreen,
                crossterm::style::ResetColor,
                crossterm::cursor::Show
            );
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
            on_panic(tui, &|| {
                let _ = terminal::disable_raw_mode();
                let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
            });
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
    let _ = storage::remember_project(&app.project);
    app.adopt_detached();
    let mut drawn = Instant::now();
    'run: loop {
        if app.signal.load(Ordering::Relaxed) {
            app.session.cancel_all();
            app.session.quit_after_stop = true;
        }
        if app.poll()? {
            break;
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
        // Frames are drawn when something changed, and once a second for
        // elapsed times; spinners animate faster.
        let tick = if app.animating() {
            Duration::from_millis(80)
        } else {
            Duration::from_secs(1)
        };
        if app.ui.redraw || drawn.elapsed() >= tick {
            // Notifications from worker threads wait for the frame.
            let _terminal = crate::notify::terminal_lock();
            screen.terminal.draw(|frame| draw(frame, &mut app))?;
            frame_marker::frame_drawn();
            app.ui.redraw = false;
            drawn = Instant::now();
        }
        if event::poll(Duration::from_millis(70))? {
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
                if !event::poll(Duration::ZERO)? {
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
