use super::theme::Palette;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Paragraph},
};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const DURATION: Duration = Duration::from_millis(1700);
const FRAME: Duration = Duration::from_millis(25);

/// Returns false when startup was interrupted, leaving terminal cleanup to Screen.
pub(super) fn play<B: Backend>(
    terminal: &mut Terminal<B>,
    cancel: &AtomicBool,
    palette: Palette,
) -> Result<bool> {
    let started = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let elapsed = started.elapsed();
        if elapsed >= DURATION {
            return Ok(true);
        }
        terminal.draw(|frame| draw(frame, elapsed, palette))?;
        if event::poll(FRAME.min(DURATION.saturating_sub(started.elapsed())))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    // Consume the skip key so it cannot edit or launch anything in the TUI.
                    return Ok(!(key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)));
                }
                Event::Paste(_) => return Ok(true),
                _ => {} // Resize is picked up by the next frame without restarting the clock.
            }
        }
    }
}

fn mix(from: Color, to: Color, amount: f32) -> Color {
    let (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) = (from, to) else {
        return to;
    };
    let amount = amount.clamp(0.0, 1.0);
    let channel = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * amount) as u8;
    Color::Rgb(channel(ar, br), channel(ag, bg), channel(ab, bb))
}

fn cell(frame: &mut Frame<'_>, area: Rect, x: u16, y: u16, symbol: &str, color: Color) {
    if x < area.width && y < area.height {
        frame.buffer_mut()[(area.x + x, area.y + y)]
            .set_symbol(symbol)
            .set_fg(color);
    }
}

fn centered(frame: &mut Frame<'_>, y: u16, text: &str, color: Color, bold: bool) {
    let area = frame.area();
    if y < area.height {
        frame.render_widget(
            Paragraph::new(text).alignment(Alignment::Center).style(
                Style::default().fg(color).add_modifier(if bold {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
            Rect::new(area.x, area.y + y, area.width, 1),
        );
    }
}

/// A decorative routing signal, independent of configuration, authentication, or model execution.
fn draw(frame: &mut Frame<'_>, elapsed: Duration, palette: Palette) {
    let area = frame.area();
    let seconds = elapsed.as_secs_f32();
    let reveal = (seconds / 0.22).clamp(0.12, 1.0);
    let settle = ((seconds - 1.34) / 0.25).clamp(0.0, 1.0);
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.bg)),
        area,
    );
    if area.width < 35 || area.height < 18 {
        let middle = area.height / 2;
        centered(
            frame,
            middle.saturating_sub(1),
            "MODEL ROUTER",
            mix(palette.bg, palette.accent, reveal),
            true,
        );
        let symbols = ["·  ·  ·", "●  ·  ·", "·  ●  ·", "·  ·  ●", "●  ●  ●"];
        let step = ((seconds / DURATION.as_secs_f32()) * 5.0) as usize;
        centered(
            frame,
            middle + 1,
            symbols[step.min(4)],
            palette.accent,
            false,
        );
        centered(
            frame,
            area.height.saturating_sub(1),
            "Any key to skip",
            palette.muted,
            false,
        );
        return;
    }

    let top = (area.height - 16) / 2;
    centered(
        frame,
        top,
        "M O D E L   R O U T E R",
        mix(palette.bg, palette.fg, reveal),
        true,
    );
    centered(
        frame,
        top + 2,
        "Your task. Your models. Your flow.",
        mix(palette.bg, palette.muted, reveal),
        false,
    );

    let width = 57.min(area.width.saturating_sub(4));
    let graph = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + top + 4,
        width,
        9,
    );
    let start = 1;
    let split = width / 4;
    let node = width / 2;
    let join = width - split - 1;
    let end = width - 2;
    let guide = mix(palette.bg, palette.line, reveal * (1.0 - settle * 0.35));

    for x in start..=end {
        cell(frame, graph, x, 4, "─", guide);
    }
    for y in [1, 7] {
        for x in split..=join {
            cell(frame, graph, x, y, "─", guide);
        }
    }
    for x in [split, join] {
        for y in 2..7 {
            cell(frame, graph, x, y, "│", guide);
        }
    }
    for (x, y, symbol) in [
        (split, 1, "╭"),
        (join, 1, "╮"),
        (split, 7, "╰"),
        (join, 7, "╯"),
        (split, 4, "┼"),
        (join, 4, "┼"),
    ] {
        cell(frame, graph, x, y, symbol, guide);
    }

    for (lane, (y, color)) in [(1, palette.violet), (4, palette.accent), (7, palette.blue)]
        .into_iter()
        .enumerate()
    {
        let mut route: Vec<(u16, u16)> = (start..=split).map(|x| (x, 4)).collect();
        if y < 4 {
            route.extend((y..4).rev().map(|y| (split, y)));
        } else {
            route.extend((5..=y).map(|y| (split, y)));
        }
        route.extend((split + 1..=join).map(|x| (x, y)));
        if y < 4 {
            route.extend((y + 1..=4).map(|y| (join, y)));
        } else {
            route.extend((4..y).rev().map(|y| (join, y)));
        }
        route.extend((join + 1..=end).map(|x| (x, 4)));
        let position = ((seconds - 0.12 - lane as f32 * 0.09) / 1.0) * (route.len() - 1) as f32;
        for (index, &(x, y)) in route.iter().enumerate() {
            let distance = position - index as f32;
            if (0.0..8.0).contains(&distance) {
                let intensity = (1.0 - distance / 8.0) * (1.0 - settle);
                let symbol = if distance < 1.0 {
                    "●"
                } else if distance < 3.0 {
                    "•"
                } else {
                    "·"
                };
                cell(frame, graph, x, y, symbol, mix(guide, color, intensity));
            }
        }
        let arrival = route.iter().position(|p| *p == (node, y)).unwrap_or(0) as f32;
        let activated = ((position - arrival) / 5.0).clamp(0.0, 1.0);
        let glow = mix(guide, color, activated * (1.0 - settle * 0.2));
        cell(
            frame,
            graph,
            node,
            y,
            if activated > 0.0 { "◉" } else { "○" },
            glow,
        );
        for x in [node - 2, node + 2] {
            cell(frame, graph, x, y, "─", glow);
        }
    }
    cell(
        frame,
        graph,
        start,
        4,
        "◇",
        mix(palette.bg, palette.accent, reveal),
    );
    cell(
        frame,
        graph,
        end,
        4,
        "◆",
        mix(
            guide,
            palette.accent,
            ((seconds - 1.1) / 0.3).clamp(0.0, 1.0),
        ),
    );
    centered(
        frame,
        top + 15,
        "Any key to skip · [Ctrl+C] quit",
        mix(palette.bg, palette.muted, reveal),
        false,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn startup_uses_each_themes_background() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        for theme in super::super::theme::Theme::ALL {
            let palette = theme.palette();
            terminal
                .draw(|frame| draw(frame, Duration::from_millis(850), palette))
                .unwrap();
            assert!(
                terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .all(|cell| cell.bg == palette.bg)
            );
        }
    }

    #[test]
    fn startup_renders_and_animates_at_wide_narrow_and_tiny_sizes() {
        for (width, height) in [(120, 36), (80, 24), (40, 10), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut previous = None;
            for millis in [0, 450, 850, 1300, 1699] {
                terminal
                    .draw(|frame| {
                        draw(
                            frame,
                            Duration::from_millis(millis),
                            super::super::theme::Theme::default().palette(),
                        )
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer().clone();
                if let Some(previous) = previous {
                    if width >= 40 {
                        assert_ne!(buffer, previous, "Animation must advance between frames");
                    }
                }
                let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
                if width >= 40 {
                    assert!(text.contains("Any key to skip"));
                }
                previous = Some(buffer);
            }
        }
    }
}
