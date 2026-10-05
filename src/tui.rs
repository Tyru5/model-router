use crate::{config::Settings, engine, storage};
use anyhow::{Context, Result};
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
    time::Duration,
};

mod actions;
mod app;
mod harness_editor;
mod harnesses;
mod history;
mod modal;
mod monitor;
mod path_picker;
mod startup;
mod task_importer;
#[cfg(test)]
mod tests;
mod theme;
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
) -> Result<()> {
    let old_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
        old_hook(info);
    }));
    let mut app = App::new(project, settings, config_path, cancel);
    match theme::load(&app.theme_path) {
        Ok(theme) => app.theme = theme,
        Err(error) => {
            app.message = format!("{error:#}. Using Model Router colours; T chooses a theme.");
        }
    }
    let mut screen = Screen::new()?;
    if animate && !startup::play(&mut screen.terminal, &app.signal, app.palette())? {
        return Ok(());
    }
    let _ = storage::remember_project(&app.project);
    loop {
        if app.signal.load(Ordering::Relaxed) {
            app.cancel.store(true, Ordering::Relaxed);
            app.quit_after_stop = true;
        }
        if app.poll()? {
            break;
        }
        if app.launch_interactive {
            app.launch_interactive = false;
            screen.restore();
            let result = engine::run_interactive(
                app.run.as_mut().context("No interactive run")?,
                &app.signal,
            );
            screen.resume()?;
            app.message = match result {
                Ok(()) => "Stage returned. h inspects the handoff; a advances; i reopens.".into(),
                Err(e) => {
                    app.alert = format!("{e:#}");
                    app.alert.clone()
                }
            };
            if let Some(run) = &app.run {
                if let Some(error) = &run.error {
                    app.message = error.clone();
                    app.alert = error.clone();
                }
            }
        }
        screen.terminal.draw(|frame| draw(frame, &mut app))?;
        if event::poll(Duration::from_millis(70))? {
            match event::read()? {
                Event::Key(key) => match app.key(key) {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(error) => {
                        app.message = format!("{error:#}");
                        app.alert = app.message.clone();
                    }
                },
                Event::Paste(text) => app.paste(&text),
                _ => {}
            }
        }
        if app.cancel.load(Ordering::Relaxed) && app.worker.is_none() {
            app.cancel.store(false, Ordering::Relaxed);
        }
    }
    Ok(())
}
