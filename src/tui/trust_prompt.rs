//! The one-time question before the TUI opens on a project whose config has
//! untrusted dangerous sections (see `crate::trust`).
use crate::{config::Settings, trust::Review};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal,
};
use std::io::{self, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Remember the config's current dangerous sections as trusted.
    Trust,
    /// Open the project with them ignored for this session.
    Ignore,
    Quit,
}

fn choice(key: KeyEvent) -> Option<Choice> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Choice::Quit),
        KeyCode::Char('t' | 'T') => Some(Choice::Trust),
        KeyCode::Char('i' | 'I') => Some(Choice::Ignore),
        KeyCode::Char('q' | 'Q') | KeyCode::Esc => Some(Choice::Quit),
        _ => None,
    }
}

/// Lists the dangerous sections and waits for t, i, or q.
pub fn ask(review: &Review) -> Result<Choice> {
    let mut out = io::stdout();
    write!(
        out,
        "{}\n[t] Trust and continue   [i] Continue with these sections ignored   [q] Quit\n",
        review.text()
    )?;
    out.flush()?;
    terminal::enable_raw_mode()?;
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
        }
    }
    let _restore = Restore;
    loop {
        if let Event::Key(key) = event::read()? {
            if let Some(choice) = choice(key) {
                return Ok(choice);
            }
        }
    }
}

/// Load notes for the status line: unknown fields, migrations, ignored sections.
pub fn notice(settings: &Settings) -> Option<String> {
    (!settings.warnings.is_empty()).then(|| settings.warnings.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_choose_trust_ignore_or_quit() {
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        let none = KeyModifiers::NONE;
        assert_eq!(choice(key(KeyCode::Char('t'), none)), Some(Choice::Trust));
        assert_eq!(choice(key(KeyCode::Char('i'), none)), Some(Choice::Ignore));
        assert_eq!(choice(key(KeyCode::Esc, none)), Some(Choice::Quit));
        assert_eq!(
            choice(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Choice::Quit)
        );
        assert_eq!(choice(key(KeyCode::Enter, none)), None);
        let mut settings = Settings::default();
        assert_eq!(notice(&settings), None);
        settings.warnings = vec!["a".into(), "b".into()];
        assert_eq!(notice(&settings).as_deref(), Some("a · b"));
    }
}
