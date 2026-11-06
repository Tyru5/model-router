use super::input::InputField;
use super::theme::Palette;
use crate::storage;
use crossterm::event::{KeyCode as Key, KeyEvent, KeyModifiers as Mods};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Paragraph, Wrap},
};
use tui_textarea::CursorMove;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(super) fn textarea(value: &str) -> InputField<'static> {
    let mut field = InputField::from(value.split('\n').map(str::to_owned).collect::<Vec<_>>());
    focus_textarea(&mut field);
    field
}
/// Newly focused inputs are ready to append, including multiline values.
pub(super) fn focus_textarea(field: &mut InputField<'_>) {
    field.insert_mode();
    field.cancel_selection();
    field.move_cursor(CursorMove::Bottom);
    if field.cursor().1 != field.lines().last().map_or(0, |line| line.chars().count()) {
        field.move_cursor(CursorMove::End);
    }
}
pub(super) fn content(text: &InputField<'_>) -> String {
    text.lines().join("\n")
}
/// Soft-wrap the editor's display while keeping its buffer and undo history intact.
pub(super) fn wrapped_textarea(
    frame: &mut Frame<'_>,
    field: &mut InputField<'_>,
    area: Rect,
    block: Block<'_>,
) {
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    let width = usize::from(inner.width);
    let cursor = field.cursor();
    let selection = field.selection_range();
    let selection_style = field.selection_style();
    let mut lines = Vec::new();
    let mut cursor_row = 0;
    for (row, source) in field.lines().iter().enumerate() {
        let mut line = Line::default();
        let mut used = 0;
        let mut column = 0;
        // The trailing space gives the cursor a cell at the end of a line,
        // including when the text fills the entire display width.
        let end_cursor = cursor == (row, source.chars().count());
        for grapheme in source.graphemes(true).chain(end_cursor.then_some(" ")) {
            let count = grapheme.chars().count();
            let value = if grapheme == "\t" {
                let tab = usize::from(field.tab_length()).max(1);
                " ".repeat(tab - used % tab)
            } else {
                grapheme.to_owned()
            };
            let mut style = Style::default();
            if selection
                .is_some_and(|(start, end)| start < (row, column + count) && (row, column) < end)
            {
                style = selection_style;
            }
            for (part, display) in value.graphemes(true).enumerate() {
                let span = Span::raw(display.to_owned());
                let size = span.width();
                if used > 0 && used + size > width {
                    lines.push(line);
                    line = Line::default();
                    used = 0;
                }
                let mut display_style = style;
                if part == 0 && cursor.0 == row && (column..column + count).contains(&cursor.1) {
                    cursor_row = lines.len();
                    display_style = field.cursor_style();
                }
                line.spans.push(span.style(display_style));
                used += size;
            }
            column += count;
        }
        lines.push(line);
    }
    let top = cursor_row.saturating_sub(usize::from(inner.height) - 1);
    frame.render_widget(
        Paragraph::new(lines.into_iter().skip(top).collect::<Vec<_>>()).style(field.style()),
        inner,
    );
}
pub(super) fn menu_key(key: KeyEvent) -> Key {
    match key.code {
        Key::Char('n') if key.modifiers.contains(Mods::CONTROL) => Key::Down,
        Key::Char('p') if key.modifiers.contains(Mods::CONTROL) => Key::Up,
        code => code,
    }
}
/// Lines needed to show `line` wrapped at `width`, capped at `max`.
pub(super) fn rows_for(line: &Line<'_>, width: u16, max: u16) -> u16 {
    (line.width() as u16).div_ceil(width.max(1)).clamp(1, max)
}
pub(super) fn elapsed(seconds: i64) -> String {
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
    }
}
/// Home-relative path, trimmed from the left to fit `width` display
/// columns; wide characters (CJK, emoji) take two.
pub(super) fn short_path(path: &std::path::Path, width: usize) -> String {
    let mut text = path.display().to_string();
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        if let Ok(rest) = path.strip_prefix(&home) {
            text = format!("~/{}", rest.display());
        }
    }
    let text = storage::clean_terminal(&text);
    if text.width() <= width {
        return text;
    }
    // Keep the end, which names the folder, behind an ellipsis.
    let mut room = width.saturating_sub(1);
    let mut tail = vec![];
    for c in text.chars().rev() {
        let size = c.width().unwrap_or(0);
        if size > room {
            break;
        }
        room -= size;
        tail.push(c);
    }
    std::iter::once('…').chain(tail.into_iter().rev()).collect()
}
pub(super) fn kv(palette: Palette, label: &str, value: &str, set: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<11}"), Style::default().fg(palette.muted)),
        Span::styled(
            value.to_string(),
            Style::default().fg(if set { palette.fg } else { palette.muted }),
        ),
    ])
}
pub(super) fn or_default<'a>(value: &'a str, fallback: &'a str) -> (&'a str, bool) {
    if value.is_empty() {
        (fallback, false)
    } else {
        (value, true)
    }
}
/// Centered headline plus muted guidance inside a titled panel.
pub(super) fn empty_state(
    frame: &mut Frame<'_>,
    palette: Palette,
    area: Rect,
    block: Block<'static>,
    head: &str,
    body: &str,
) {
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let text = Text::from(vec![
        Line::styled(
            head.to_string(),
            Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::styled(body.to_string(), Style::default().fg(palette.muted)),
    ]);
    let top = inner.height.saturating_sub(4) / 2;
    let rect = Rect::new(inner.x, inner.y + top, inner.width, inner.height - top);
    frame.render_widget(
        Paragraph::new(text).centered().wrap(Wrap { trim: true }),
        rect,
    );
}
pub(super) fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(4));
    let height = height.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}
/// Fade everything already drawn so a modal reads as the foreground.
pub(super) fn dim(frame: &mut Frame<'_>, palette: Palette) {
    for cell in &mut frame.buffer_mut().content {
        cell.set_style(
            Style::default()
                .fg(palette.line)
                .bg(palette.bg)
                .remove_modifier(Modifier::all()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, style::Color};
    use tui_textarea::CursorMove;

    fn render(field: &mut InputField<'_>, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| wrapped_textarea(frame, field, frame.area(), Block::default()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn row(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    #[test]
    fn soft_wrap_reflows_and_scrolls_without_editing_the_buffer() {
        let mut field = textarea("abcdefghij\nnext");
        field.move_cursor(CursorMove::Jump(0, 0));
        field.set_cursor_style(Style::default().bg(Color::Red));
        let buffer = render(&mut field, 5, 4);
        assert_eq!(row(&buffer, 0), "abcde");
        assert_eq!(row(&buffer, 1), "fghij");
        assert_eq!(row(&buffer, 2), "next ");
        let buffer = render(&mut field, 7, 4);
        assert_eq!(row(&buffer, 0), "abcdefg");
        assert_eq!(row(&buffer, 1), "hij    ");

        field.move_cursor(CursorMove::End);
        let buffer = render(&mut field, 5, 2);
        assert_eq!(row(&buffer, 0), "fghij");
        assert_eq!(buffer[(0, 1)].bg, Color::Red);
        field.insert_char('!');
        render(&mut field, 5, 2);
        assert_eq!(content(&field), "abcdefghij!\nnext");
        field.undo();
        assert_eq!(content(&field), "abcdefghij\nnext");
    }

    #[test]
    fn soft_wrap_preserves_graphemes_and_selection_across_rows() {
        let mut field = textarea("界界e\u{301}xy");
        field.move_cursor(CursorMove::Jump(0, 0));
        field.set_selection_style(Style::default().bg(Color::Blue));
        field.set_cursor_style(Style::default().bg(Color::Red));
        field.start_selection();
        field.move_cursor(CursorMove::End);
        let buffer = render(&mut field, 4, 3);
        assert_eq!(buffer[(0, 0)].symbol(), "界");
        assert_eq!(buffer[(2, 0)].symbol(), "界");
        assert_eq!(buffer[(0, 1)].symbol(), "e\u{301}");
        assert_eq!(buffer[(0, 1)].bg, Color::Blue);
        assert_eq!(buffer[(3, 1)].bg, Color::Red);
        assert_eq!(content(&field), "界界e\u{301}xy");
    }

    #[test]
    fn short_path_fits_display_width_with_wide_characters() {
        let path = std::path::Path::new("/srv/项目/雪の日/😀app");
        for width in [4, 8, 12, 40] {
            let short = short_path(path, width);
            assert!(short.width() <= width, "{short:?} is wider than {width}");
        }
        assert_eq!(short_path(path, 40), "/srv/项目/雪の日/😀app");
        assert_eq!(short_path(path, 7), "…/😀app");
        // A wide character that does not fit is left out whole.
        assert_eq!(short_path(path, 5), "…app");
    }

    #[test]
    fn focusing_inputs_appends_to_existing_text_and_clears_selection() {
        for value in ["", "name", "first\n界é", "first\n"] {
            let mut field = textarea(value);
            let last = field.lines().len() - 1;
            let end = (last, field.lines()[last].chars().count());
            assert_eq!(field.cursor(), end);
            field.select_all();
            focus_textarea(&mut field);
            assert_eq!(field.cursor(), end);
            assert!(!field.is_selecting());
            focus_textarea(&mut field);
            assert_eq!(field.cursor(), end);
            field.insert_char('!');
            assert_eq!(content(&field), format!("{value}!"));
        }
    }
}
