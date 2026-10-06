//! Optional Vim motions shared by all editable TUI inputs.
use crossterm::event::{KeyCode as Key, KeyEvent, KeyModifiers as Mods};
use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
use std::ops::{Deref, DerefMut};
use tui_textarea::{CursorMove, TextArea};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Default)]
pub(super) struct InputField<'a> {
    text: TextArea<'a>,
    vim: bool,
    normal: bool,
    pending_g: bool,
}

impl From<Vec<String>> for InputField<'_> {
    fn from(lines: Vec<String>) -> Self {
        Self {
            text: TextArea::from(lines),
            ..Self::default()
        }
    }
}

impl<'a> Deref for InputField<'a> {
    type Target = TextArea<'a>;
    fn deref(&self) -> &Self::Target {
        &self.text
    }
}
impl DerefMut for InputField<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.text
    }
}
impl Widget for &InputField<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        (&self.text).render(area, buffer);
    }
}

impl InputField<'_> {
    pub fn set_vim(&mut self, enabled: bool) {
        if self.vim != enabled {
            self.vim = enabled;
            self.insert_mode();
        }
    }

    pub fn insert_mode(&mut self) {
        self.normal = false;
        self.pending_g = false;
    }

    pub fn mode_hint(&self) -> Option<&'static str> {
        self.vim.then_some(if self.pending_g {
            "VIM NORMAL · g… · Esc clears motion"
        } else if self.normal {
            "VIM NORMAL · i insert · Esc cancel"
        } else {
            "VIM INSERT · Esc normal"
        })
    }

    pub fn input(&mut self, key: KeyEvent) -> bool {
        if self.vim && self.normal {
            false
        } else {
            self.text.input(key)
        }
    }

    /// Bracketed paste is always literal text, never a sequence of Vim commands.
    pub fn insert_str(&mut self, text: impl AsRef<str>) -> bool {
        self.insert_mode();
        self.text.insert_str(text)
    }

    fn jump(&mut self, row: usize, col: usize) {
        self.text.move_cursor(CursorMove::Jump(
            row.min(u16::MAX as usize) as u16,
            col.min(u16::MAX as usize) as u16,
        ));
    }

    fn boundaries(&self) -> Vec<usize> {
        let mut col = 0;
        self.text.lines()[self.text.cursor().0]
            .graphemes(true)
            .map(|g| {
                let start = col;
                col += g.chars().count();
                start
            })
            .collect()
    }

    /// Normal mode rests on a whole character, not the insertion point after it.
    fn clamp_normal(&mut self) {
        let (row, col) = self.text.cursor();
        let col = self
            .boundaries()
            .into_iter()
            .take_while(|&c| c <= col)
            .last()
            .unwrap_or(0);
        self.jump(row, col);
    }

    fn first_nonblank(&mut self) {
        let (row, _) = self.text.cursor();
        let col = self.text.lines()[row]
            .chars()
            .position(|c| !c.is_whitespace())
            .unwrap_or(0);
        self.jump(row, col);
    }

    /// Run before dialog shortcuts, so Esc can leave Insert mode without closing.
    /// False lets the containing form handle navigation, submission, or cancellation.
    pub fn vim_key(&mut self, key: KeyEvent) -> bool {
        if !self.vim {
            return false;
        }
        if key.code == Key::Esc {
            if self.pending_g {
                self.pending_g = false;
                return true;
            }
            if self.normal {
                return false;
            }
            self.text.cancel_selection();
            self.normal = true;
            let (row, col) = self.text.cursor();
            let previous = self
                .boundaries()
                .into_iter()
                .take_while(|&c| c < col)
                .last()
                .unwrap_or(0);
            self.jump(row, previous);
            return true;
        }
        if !self.normal {
            return false;
        }
        let pending_g = std::mem::take(&mut self.pending_g);
        if key
            .modifiers
            .intersects(Mods::CONTROL | Mods::ALT | Mods::SUPER)
        {
            return false;
        }
        let (row, col) = self.text.cursor();
        match key.code {
            Key::Char('h') => {
                let previous = self
                    .boundaries()
                    .into_iter()
                    .take_while(|&c| c < col)
                    .last()
                    .unwrap_or(0);
                self.jump(row, previous);
            }
            Key::Char('l') => {
                let next = self
                    .boundaries()
                    .into_iter()
                    .find(|&c| c > col)
                    .unwrap_or(col);
                self.jump(row, next);
            }
            Key::Char('j') => self.text.move_cursor(CursorMove::Down),
            Key::Char('k') => self.text.move_cursor(CursorMove::Up),
            Key::Char('w') => self.text.move_cursor(CursorMove::WordForward),
            Key::Char('b') => self.text.move_cursor(CursorMove::WordBack),
            Key::Char('e') => self.text.move_cursor(CursorMove::WordEnd),
            Key::Char('0') => self.jump(row, 0),
            Key::Char('^') => self.first_nonblank(),
            Key::Char('$') => self.text.move_cursor(CursorMove::End),
            Key::Char('g') if pending_g => {
                self.jump(0, 0);
                self.first_nonblank();
            }
            Key::Char('g') => self.pending_g = true,
            Key::Char('G') => {
                self.jump(self.text.lines().len() - 1, 0);
                self.first_nonblank();
            }
            Key::Char(c @ ('i' | 'a' | 'I' | 'A')) => {
                match c {
                    'a' => {
                        let end = self.text.lines()[row].chars().count();
                        let next = self
                            .boundaries()
                            .into_iter()
                            .find(|&c| c > col)
                            .unwrap_or(end);
                        self.jump(row, next);
                    }
                    'I' => self.first_nonblank(),
                    'A' => self.text.move_cursor(CursorMove::End),
                    _ => {}
                }
                self.insert_mode();
                return true;
            }
            // Unsupported normal-mode commands must not accidentally insert text.
            Key::Char(_) | Key::Backspace | Key::Delete => {}
            _ => return false,
        }
        self.clamp_normal();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::widgets::{content, focus_textarea, textarea};

    fn press(field: &mut InputField<'_>, code: Key) {
        let key = KeyEvent::new(code, Mods::NONE);
        if !field.vim_key(key) {
            field.input(key);
        }
    }

    fn normal(value: &str) -> InputField<'static> {
        let mut field = textarea(value);
        field.set_vim(true);
        press(&mut field, Key::Esc);
        field
    }

    #[test]
    fn motions_navigate_words_lines_and_buffer_without_modifying_text() {
        let value = "  alpha beta\nsecond line\nlast";
        let mut field = normal(value);
        for (motion, expected) in [
            ('g', (2, 3)),
            ('g', (0, 2)),
            ('0', (0, 0)),
            ('^', (0, 2)),
            ('w', (0, 8)),
            ('e', (0, 11)),
            ('b', (0, 8)),
            ('h', (0, 7)),
            ('l', (0, 8)),
            ('j', (1, 8)),
            ('k', (0, 8)),
            ('$', (0, 11)),
            ('l', (0, 11)),
            ('G', (2, 0)),
            ('h', (2, 0)),
        ] {
            press(&mut field, Key::Char(motion));
            assert_eq!(field.cursor(), expected, "motion {motion}");
            assert_eq!(content(&field), value);
        }
    }

    #[test]
    fn insert_commands_place_text_at_the_expected_position() {
        for (command, expected) in [
            ('i', "  aXbc"),
            ('a', "  abXc"),
            ('I', "  Xabc"),
            ('A', "  abcX"),
        ] {
            let mut field = normal("  abc");
            press(&mut field, Key::Char('h'));
            press(&mut field, Key::Char(command));
            press(&mut field, Key::Char('X'));
            assert_eq!(content(&field), expected, "command {command}");
        }
    }

    #[test]
    fn focus_paste_and_disabling_reset_pending_motions() {
        let mut field = normal("abc");
        press(&mut field, Key::Char('g'));
        assert!(field.vim_key(KeyEvent::new(Key::Esc, Mods::NONE)));
        assert!(!field.vim_key(KeyEvent::new(Key::Esc, Mods::NONE)));
        for code in [
            Key::Char('x'),
            Key::Char('d'),
            Key::Backspace,
            Key::Delete,
            Key::Enter,
        ] {
            press(&mut field, code);
        }
        assert_eq!(content(&field), "abc");
        press(&mut field, Key::Char('g'));
        focus_textarea(&mut field);
        assert_eq!(field.cursor(), (0, 3));
        press(&mut field, Key::Char('j'));
        assert_eq!(content(&field), "abcj");
        press(&mut field, Key::Esc);
        press(&mut field, Key::Char('g'));
        field.insert_str("gg\n:quit");
        assert_eq!(content(&field), "abcgg\n:quitj");
        assert!(field.mode_hint().unwrap().contains("INSERT"));
        press(&mut field, Key::Esc);
        field.set_vim(false);
        assert!(field.mode_hint().is_none());
        assert!(!field.vim_key(KeyEvent::new(Key::Esc, Mods::NONE)));
    }

    #[test]
    fn unicode_and_empty_lines_have_valid_normal_cursor_positions() {
        let mut field = normal("界e\u{301}🙂");
        assert_eq!(field.cursor(), (0, 3));
        for (motion, col) in [('h', 1), ('h', 0), ('h', 0), ('l', 1), ('l', 3), ('l', 3)] {
            press(&mut field, Key::Char(motion));
            assert_eq!(field.cursor(), (0, col));
        }
        press(&mut field, Key::Char('h'));
        press(&mut field, Key::Char('a'));
        press(&mut field, Key::Char('!'));
        assert_eq!(content(&field), "界e\u{301}!🙂");
        let mut field = normal("\n");
        for c in "gg0^$hjklwebG".chars() {
            press(&mut field, Key::Char(c));
        }
        assert_eq!(content(&field), "\n");
        assert_eq!(field.cursor(), (1, 0));
    }
}
