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
use tui_textarea::TextArea;

pub(super) fn textarea(value: &str) -> TextArea<'static> {
    TextArea::from(value.split('\n').map(str::to_owned).collect::<Vec<_>>())
}
pub(super) fn content(text: &TextArea<'_>) -> String {
    text.lines().join("\n")
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
/// Home-relative path, trimmed from the left to fit `width` columns.
pub(super) fn short_path(path: &std::path::Path, width: usize) -> String {
    let mut text = path.display().to_string();
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        if let Ok(rest) = path.strip_prefix(&home) {
            text = format!("~/{}", rest.display());
        }
    }
    let text = storage::clean_terminal(&text);
    let count = text.chars().count();
    if count <= width {
        return text;
    }
    let keep = width.saturating_sub(1);
    format!("…{}", text.chars().skip(count - keep).collect::<String>())
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
