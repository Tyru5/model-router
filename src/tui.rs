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
mod external;
mod frame_marker;
mod handoff_view;
mod harness_editor;
mod harnesses;
mod history;
mod input;
mod modal;
mod monitor;
mod output_search;
mod path_picker;
mod run_slot;
mod session;
mod startup;
mod stats_page;
mod task_importer;
#[cfg(test)]
mod tests;
mod theme;
pub mod trust_prompt;
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

pub fn run(
    project: PathBuf,
    settings: Settings,
    config_path: PathBuf,
    cancel: Arc<AtomicBool>,
    animate: bool,
    detected_from: Option<PathBuf>,
) -> Result<()> {
    let old_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
        old_hook(info);
    }));
    let mut app = App::new(project, settings, config_path, cancel);
    if let Some(cwd) = detected_from {
        app.message = format!(
            "Project: {} (detected above {}). p changes it.",
            app.project.display(),
            cwd.display()
        );
    }
    if let Some(note) = trust_prompt::notice(&app.settings) {
        app.message = if app.message.is_empty() {
            note
        } else {
            format!("{} · {note}", app.message)
        };
    }
    match theme::load(&app.theme_path) {
        Ok(preferences) => {
            app.theme = preferences.theme;
            app.vim_motions = preferences.vim_motions;
            crate::notify::global().set_backends(preferences.notifications.clone());
            app.notifications = preferences.notifications;
        }
        Err(error) => {
            app.message = format!("{error:#}. Using Model Router colours; T chooses a theme.");
        }
    }
    let mut screen = Screen::new()?;
    if animate && !startup::play(&mut screen.terminal, &app.signal, app.palette())? {
        return Ok(());
    }
    let _ = storage::remember_project(&app.project);
    let mut drawn = Instant::now();
    'run: loop {
        if app.signal.load(Ordering::Relaxed) {
            app.session.cancel_all();
            app.quit_after_stop = true;
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
            app.message = match result {
                Ok(AttachOutcome::Detached) => {
                    "Interactive session detached and still running. [t] reattaches; [C] stops it. /exit quits the harness.".into()
                }
                Ok(AttachOutcome::Finished) => "Harness returned. Finishing stage…".into(),
                Err(error) => {
                    app.alert = format!("{error:#}");
                    app.alert.clone()
                }
            };
            // Apply completion or shutdown before accepting another action.
            app.redraw = true;
            continue;
        }
        if let Some(launch) = app.external.take() {
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
        if app.redraw || drawn.elapsed() >= tick {
            // Notifications from worker threads wait for the frame.
            let _terminal = crate::notify::terminal_lock();
            screen.terminal.draw(|frame| draw(frame, &mut app))?;
            frame_marker::frame_drawn();
            app.redraw = false;
            drawn = Instant::now();
        }
        if event::poll(Duration::from_millis(70))? {
            app.redraw = true;
            // Handle all queued input before drawing again, so slow frames do
            // not make typing lag one event per frame.
            for _ in 0..64 {
                match event::read()? {
                    Event::Key(key) => match app.key(key) {
                        Ok(true) => break 'run,
                        Ok(false) => {}
                        Err(error) => {
                            app.message = format!("{error:#}");
                            app.alert = app.message.clone();
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
    Ok(())
}
