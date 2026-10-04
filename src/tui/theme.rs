use crate::{engine::Status, storage};
use anyhow::{Context, Result};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding},
};
use serde::{Deserialize, Serialize};
use std::{fs, io::ErrorKind, path::Path};
use tui_textarea::TextArea;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Theme {
    #[default]
    ModelRouter,
    CatppuccinMocha,
    Dracula,
    Nord,
    TokyoNight,
    GruvboxDark,
    OneDark,
    Monokai,
    SolarizedDark,
    CatppuccinLatte,
    GruvboxLight,
    SolarizedLight,
}

impl Theme {
    pub const ALL: [Self; 12] = [
        Self::ModelRouter,
        Self::CatppuccinMocha,
        Self::Dracula,
        Self::Nord,
        Self::TokyoNight,
        Self::GruvboxDark,
        Self::OneDark,
        Self::Monokai,
        Self::SolarizedDark,
        Self::CatppuccinLatte,
        Self::GruvboxLight,
        Self::SolarizedLight,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::ModelRouter => "Model Router",
            Self::CatppuccinMocha => "Catppuccin Mocha",
            Self::Dracula => "Dracula",
            Self::Nord => "Nord",
            Self::TokyoNight => "Tokyo Night",
            Self::GruvboxDark => "Gruvbox Dark",
            Self::OneDark => "One Dark",
            Self::Monokai => "Monokai",
            Self::SolarizedDark => "Solarized Dark",
            Self::CatppuccinLatte => "Catppuccin Latte",
            Self::GruvboxLight => "Gruvbox Light",
            Self::SolarizedLight => "Solarized Light",
        }
    }

    pub fn appearance(self) -> &'static str {
        match self {
            Self::CatppuccinLatte | Self::GruvboxLight | Self::SolarizedLight => "light",
            _ => "dark",
        }
    }

    // Palette sources and UI mappings are documented in docs/themes.md.
    pub fn palette(self) -> Palette {
        match self {
            Self::ModelRouter => Palette {
                bg: rgb(0x0d1421),
                panel: rgb(0x131d2d),
                fg: rgb(0xdee7f2),
                muted: rgb(0x8da0b4),
                accent: rgb(0x4fd1c5),
                line: rgb(0x2e4156),
                selection_bg: rgb(0x1d3d46),
                selection_fg: rgb(0x4fd1c5),
                success: rgb(0x4fd1c5),
                warn: rgb(0xf6c15b),
                red: rgb(0xfa7e7e),
                blue: rgb(0x73b8ff),
                violet: rgb(0xa691f9),
            },
            Self::CatppuccinMocha => Palette {
                bg: rgb(0x181825),
                panel: rgb(0x1e1e2e),
                fg: rgb(0xcdd6f4),
                muted: rgb(0xa6adc8),
                accent: rgb(0xcba6f7),
                line: rgb(0x585b70),
                selection_bg: rgb(0x45475a),
                selection_fg: rgb(0xcdd6f4),
                success: rgb(0xa6e3a1),
                warn: rgb(0xf9e2af),
                red: rgb(0xf38ba8),
                blue: rgb(0x89b4fa),
                violet: rgb(0xcba6f7),
            },
            Self::Dracula => Palette {
                bg: rgb(0x282a36),
                panel: rgb(0x343746),
                fg: rgb(0xf8f8f2),
                muted: rgb(0x9da8c7),
                accent: rgb(0xbd93f9),
                line: rgb(0x6272a4),
                selection_bg: rgb(0x44475a),
                selection_fg: rgb(0xf8f8f2),
                success: rgb(0x50fa7b),
                warn: rgb(0xf1fa8c),
                red: rgb(0xff5555),
                blue: rgb(0x8be9fd),
                violet: rgb(0xbd93f9),
            },
            Self::Nord => Palette {
                bg: rgb(0x2e3440),
                panel: rgb(0x3b4252),
                fg: rgb(0xeceff4),
                muted: rgb(0xd8dee9),
                accent: rgb(0x88c0d0),
                line: rgb(0x4c566a),
                selection_bg: rgb(0x434c5e),
                selection_fg: rgb(0xeceff4),
                success: rgb(0xa3be8c),
                warn: rgb(0xebcb8b),
                red: rgb(0xbf616a),
                blue: rgb(0x81a1c1),
                violet: rgb(0xb48ead),
            },
            Self::TokyoNight => Palette {
                bg: rgb(0x16161e),
                panel: rgb(0x1a1b26),
                fg: rgb(0xc0caf5),
                muted: rgb(0xa9b1d6),
                accent: rgb(0x7aa2f7),
                line: rgb(0x565f89),
                selection_bg: rgb(0x283457),
                selection_fg: rgb(0xc0caf5),
                success: rgb(0x9ece6a),
                warn: rgb(0xe0af68),
                red: rgb(0xf7768e),
                blue: rgb(0x7dcfff),
                violet: rgb(0xbb9af7),
            },
            Self::GruvboxDark => Palette {
                bg: rgb(0x282828),
                panel: rgb(0x3c3836),
                fg: rgb(0xebdbb2),
                muted: rgb(0xbdae93),
                accent: rgb(0xfabd2f),
                line: rgb(0x7c6f64),
                selection_bg: rgb(0x504945),
                selection_fg: rgb(0xfbf1c7),
                success: rgb(0xb8bb26),
                warn: rgb(0xfe8019),
                red: rgb(0xfb4934),
                blue: rgb(0x83a598),
                violet: rgb(0xd3869b),
            },
            Self::OneDark => Palette {
                bg: rgb(0x21252b),
                panel: rgb(0x282c34),
                fg: rgb(0xabb2bf),
                muted: rgb(0x828997),
                accent: rgb(0x61afef),
                line: rgb(0x5c6370),
                selection_bg: rgb(0x3e4451),
                selection_fg: rgb(0xe5e9f0),
                success: rgb(0x98c379),
                warn: rgb(0xe5c07b),
                red: rgb(0xe06c75),
                blue: rgb(0x56b6c2),
                violet: rgb(0xc678dd),
            },
            Self::Monokai => Palette {
                bg: rgb(0x272822),
                panel: rgb(0x383830),
                fg: rgb(0xf8f8f2),
                muted: rgb(0xa59f85),
                accent: rgb(0xa6e22e),
                line: rgb(0x75715e),
                selection_bg: rgb(0x49483e),
                selection_fg: rgb(0xf8f8f2),
                success: rgb(0xa6e22e),
                warn: rgb(0xe6db74),
                red: rgb(0xf92672),
                blue: rgb(0x66d9ef),
                violet: rgb(0xae81ff),
            },
            Self::SolarizedDark => Palette {
                bg: rgb(0x002b36),
                panel: rgb(0x073642),
                fg: rgb(0x93a1a1),
                muted: rgb(0x839496),
                accent: rgb(0x2aa198),
                line: rgb(0x586e75),
                selection_bg: rgb(0x586e75),
                selection_fg: rgb(0xfdf6e3),
                success: rgb(0x859900),
                warn: rgb(0xb58900),
                red: rgb(0xdc322f),
                blue: rgb(0x268bd2),
                violet: rgb(0x6c71c4),
            },
            Self::CatppuccinLatte => Palette {
                bg: rgb(0xeff1f5),
                panel: rgb(0xe6e9ef),
                fg: rgb(0x4c4f69),
                muted: rgb(0x6c6f85),
                accent: rgb(0x8839ef),
                line: rgb(0x9ca0b0),
                selection_bg: rgb(0xccd0da),
                selection_fg: rgb(0x4c4f69),
                success: rgb(0x40a02b),
                warn: rgb(0x9b6410),
                red: rgb(0xd20f39),
                blue: rgb(0x1e66f5),
                violet: rgb(0x8839ef),
            },
            Self::GruvboxLight => Palette {
                bg: rgb(0xfbf1c7),
                panel: rgb(0xebdbb2),
                fg: rgb(0x3c3836),
                muted: rgb(0x665c54),
                accent: rgb(0x076678),
                line: rgb(0xa89984),
                selection_bg: rgb(0xd5c4a1),
                selection_fg: rgb(0x282828),
                success: rgb(0x79740e),
                warn: rgb(0xaf3a03),
                red: rgb(0x9d0006),
                blue: rgb(0x076678),
                violet: rgb(0x8f3f71),
            },
            Self::SolarizedLight => Palette {
                bg: rgb(0xfdf6e3),
                panel: rgb(0xeee8d5),
                fg: rgb(0x52676f),
                muted: rgb(0x657b83),
                accent: rgb(0x006b9b),
                line: rgb(0x93a1a1),
                selection_bg: rgb(0x93a1a1),
                selection_fg: rgb(0x002b36),
                success: rgb(0x657600),
                warn: rgb(0x9c460e),
                red: rgb(0xdc322f),
                blue: rgb(0x268bd2),
                violet: rgb(0x6c71c4),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Palette {
    pub bg: Color,
    pub panel: Color,
    pub fg: Color,
    pub muted: Color,
    pub accent: Color,
    pub line: Color,
    pub selection_bg: Color,
    pub selection_fg: Color,
    pub success: Color,
    pub warn: Color,
    pub red: Color,
    pub blue: Color,
    pub violet: Color,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

impl Palette {
    pub fn panel(self, title: impl Into<Line<'static>>) -> Block<'static> {
        let title: Line<'static> = title.into();
        Block::default()
            .title(title.style(Style::default().fg(self.fg).add_modifier(Modifier::BOLD)))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(self.line))
            .style(Style::default().bg(self.panel).fg(self.fg))
            .padding(Padding::horizontal(1))
    }

    /// Panel with key hints right-aligned in its top border.
    pub fn panel_with(self, title: &str, keys: &[(&str, &str)]) -> Block<'static> {
        let block = self.panel(format!(" {title} "));
        if keys.is_empty() {
            return block;
        }
        let mut hint = self.hints(keys);
        hint.spans.insert(0, Span::raw(" "));
        hint.spans.push(Span::raw(" "));
        block.title(hint.right_aligned())
    }

    /// Dialog frame: accent border, title left, key hints right.
    pub fn modal(self, title: &str, keys: &[(&str, &str)]) -> Block<'static> {
        self.panel_with(title, keys)
            .border_style(Style::default().fg(self.accent))
    }

    /// Key/label pairs styled as `key label   key label`.
    pub fn hints(self, keys: &[(&str, &str)]) -> Line<'static> {
        let mut spans = Vec::with_capacity(keys.len() * 3);
        for (index, (key, label)) in keys.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw("   "));
            }
            spans.push(Span::styled(
                key.to_string(),
                Style::default()
                    .fg(self.accent)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!(" {label}"),
                Style::default().fg(self.muted),
            ));
        }
        Line::from(spans)
    }

    pub fn header_style(self) -> Style {
        Style::default().fg(self.muted).add_modifier(Modifier::BOLD)
    }

    pub fn selected_style(self) -> Style {
        Style::default()
            .bg(self.selection_bg)
            .fg(self.selection_fg)
            .add_modifier(Modifier::BOLD)
    }

    pub fn status_color(self, status: Status) -> Color {
        match status {
            Status::Complete => self.success,
            Status::Failed | Status::Blocked | Status::Cancelled => self.red,
            Status::Running | Status::Interactive | Status::AwaitingConfirmation => self.warn,
            _ => self.muted,
        }
    }

    pub fn status_span(self, status: Status) -> Span<'static> {
        let glyph = match status {
            Status::Complete => "✓",
            Status::Failed | Status::Blocked => "✗",
            Status::Cancelled => "⊘",
            Status::Running | Status::Interactive => "●",
            Status::AwaitingConfirmation => "◐",
            Status::Skipped => "–",
            Status::Pending => "○",
        };
        Span::styled(
            format!("{glyph} {}", status.label()),
            Style::default().fg(self.status_color(status)),
        )
    }

    pub fn style_textarea(self, text: &mut TextArea<'_>) {
        text.set_style(Style::default().fg(self.fg).bg(self.panel));
        text.set_cursor_line_style(Style::default());
        text.set_cursor_style(Style::default().fg(self.bg).bg(self.accent));
        text.set_selection_style(self.selected_style());
    }
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct Appearance {
    theme: Theme,
}

pub(super) fn load(path: &Path) -> Result<Theme> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Theme::default()),
        Err(error) => return Err(error).with_context(|| format!("Read {}", path.display())),
    };
    let appearance: Appearance = serde_json::from_slice(&bytes)
        .with_context(|| format!("Read theme from {}", path.display()))?;
    Ok(appearance.theme)
}

pub(super) fn save(path: &Path, theme: Theme) -> Result<()> {
    storage::write_json(path, &Appearance { theme })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearance_defaults_and_invalid_files_are_recoverable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("appearance.json");
        assert_eq!(load(&path).unwrap(), Theme::ModelRouter);
        fs::write(&path, "{}").unwrap();
        assert_eq!(load(&path).unwrap(), Theme::ModelRouter);
        for invalid in [
            r#"{"theme":"unknown-theme"}"#,
            "not json",
            r#"{"theme":42}"#,
        ] {
            fs::write(&path, invalid).unwrap();
            assert!(load(&path).is_err());
        }
        save(&path, Theme::CatppuccinLatte).unwrap();
        assert_eq!(load(&path).unwrap(), Theme::CatppuccinLatte);
        assert!(
            fs::read_to_string(path)
                .unwrap()
                .contains("catppuccin-latte")
        );
    }

    #[test]
    fn every_theme_roundtrips_through_user_preferences() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/appearance.json");
        let mut ids = std::collections::HashSet::new();
        for theme in Theme::ALL {
            assert!(ids.insert(serde_json::to_string(&theme).unwrap()));
            save(&path, theme).unwrap();
            assert_eq!(load(&path).unwrap(), theme);
        }
    }

    fn luminance(color: Color) -> f64 {
        let Color::Rgb(r, g, b) = color else {
            panic!("Expected RGB palette")
        };
        let channel = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }

    fn contrast(a: Color, b: Color) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn light_and_dark_themes_keep_text_and_selections_readable() {
        for theme in Theme::ALL {
            let p = theme.palette();
            for background in [p.bg, p.panel] {
                assert!(contrast(p.fg, background) >= 4.5, "{theme:?}: body text");
                assert!(
                    contrast(p.muted, background) >= 3.0,
                    "{theme:?}: secondary text"
                );
            }
            assert!(
                contrast(p.selection_fg, p.selection_bg) >= 4.5,
                "{theme:?}: selection"
            );
        }
    }
}
