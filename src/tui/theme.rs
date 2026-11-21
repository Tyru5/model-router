use super::input::InputField;
use crate::{engine::Status, notify, storage};
use anyhow::{Context, Result};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding},
};
use serde::{Deserialize, Serialize};
use std::{fs, io::ErrorKind, path::Path};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Theme {
    #[default]
    ModelRouter,
    /// ANSI colours, so the terminal's own scheme draws the UI.
    Terminal,
    CatppuccinMocha,
    Dracula,
    Nord,
    TokyoNight,
    GruvboxDark,
    OneDark,
    Monokai,
    SolarizedDark,
    Poimandres,
    CatppuccinLatte,
    GruvboxLight,
    SolarizedLight,
}

impl Theme {
    pub const ALL: [Self; 14] = [
        Self::ModelRouter,
        Self::Terminal,
        Self::CatppuccinMocha,
        Self::Dracula,
        Self::Nord,
        Self::TokyoNight,
        Self::GruvboxDark,
        Self::OneDark,
        Self::Monokai,
        Self::SolarizedDark,
        Self::Poimandres,
        Self::CatppuccinLatte,
        Self::GruvboxLight,
        Self::SolarizedLight,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::ModelRouter => "Model Router",
            Self::Terminal => "Terminal",
            Self::CatppuccinMocha => "Catppuccin Mocha",
            Self::Dracula => "Dracula",
            Self::Nord => "Nord",
            Self::TokyoNight => "Tokyo Night",
            Self::GruvboxDark => "Gruvbox Dark",
            Self::OneDark => "One Dark",
            Self::Monokai => "Monokai",
            Self::SolarizedDark => "Solarized Dark",
            Self::Poimandres => "Poimandres",
            Self::CatppuccinLatte => "Catppuccin Latte",
            Self::GruvboxLight => "Gruvbox Light",
            Self::SolarizedLight => "Solarized Light",
        }
    }

    pub fn appearance(self) -> &'static str {
        match self {
            Self::Terminal => "follows terminal",
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
            // Reset keeps the terminal's own background and text; selections reverse them.
            Self::Terminal => Palette {
                bg: Color::Reset,
                panel: Color::Reset,
                fg: Color::Reset,
                muted: Color::DarkGray,
                accent: Color::Cyan,
                line: Color::DarkGray,
                selection_bg: Color::Reset,
                selection_fg: Color::Reset,
                success: Color::Green,
                warn: Color::Yellow,
                red: Color::Red,
                blue: Color::Blue,
                violet: Color::Magenta,
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
            Self::Poimandres => Palette {
                bg: rgb(0x1b1e28),
                panel: rgb(0x202430),
                fg: rgb(0xe4f0fb),
                muted: rgb(0xa6accd),
                accent: rgb(0x5de4c7),
                line: rgb(0x506477),
                selection_bg: rgb(0x303340),
                selection_fg: rgb(0xe4f0fb),
                success: rgb(0x5de4c7),
                warn: rgb(0xfffac2),
                red: rgb(0xd0679d),
                blue: rgb(0xadd7ff),
                violet: rgb(0xf087bd),
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

    /// Key/label pairs styled as `[key] label   [key] label`.
    pub fn hints(self, keys: &[(&str, &str)]) -> Line<'static> {
        let mut spans = Vec::with_capacity(keys.len() * 3);
        for (index, (key, label)) in keys.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw("   "));
            }
            spans.push(Span::styled(
                key_label(key),
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

    /// Whether the terminal's own colours draw the UI (the Terminal theme).
    fn follows_terminal(self) -> bool {
        self.bg == Color::Reset
    }

    /// Text drawn on an accent background.
    pub fn on_accent(self) -> Color {
        if self.follows_terminal() {
            Color::Black
        } else {
            self.bg
        }
    }

    pub fn selected_style(self) -> Style {
        self.highlight_style()
            .fg(self.selection_fg)
            .add_modifier(Modifier::BOLD)
    }

    /// Background of a highlighted list row; terminal colours reverse instead.
    pub fn highlight_style(self) -> Style {
        if self.follows_terminal() {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().bg(self.selection_bg)
        }
    }

    /// Added diff lines; ANSI green in the Terminal theme.
    pub fn diff_add(self) -> Style {
        Style::default().fg(self.success)
    }

    /// Removed diff lines; ANSI red in the Terminal theme.
    pub fn diff_del(self) -> Style {
        Style::default().fg(self.red)
    }

    /// `@@` hunk headers.
    pub fn diff_hunk(self) -> Style {
        Style::default().fg(self.blue)
    }

    /// `diff --git` lines that start each file.
    pub fn diff_file(self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    /// index, mode, rename, `---`, and `+++` lines.
    pub fn diff_meta(self) -> Style {
        Style::default().fg(self.violet)
    }

    /// A search match.
    pub fn search_match(self) -> Style {
        Style::default().fg(self.on_accent()).bg(self.warn)
    }

    /// The match search moved to.
    pub fn search_current(self) -> Style {
        Style::default()
            .fg(self.on_accent())
            .bg(self.accent)
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

    pub fn status_glyph(status: Status) -> &'static str {
        match status {
            Status::Complete => "✓",
            Status::Failed | Status::Blocked => "✗",
            Status::Cancelled => "⊘",
            Status::Running | Status::Interactive => "●",
            Status::AwaitingConfirmation => "◐",
            Status::Skipped => "–",
            Status::Pending => "○",
        }
    }

    pub fn status_span(self, status: Status) -> Span<'static> {
        Span::styled(
            format!("{} {}", Self::status_glyph(status), status.label()),
            Style::default().fg(self.status_color(status)),
        )
    }

    pub fn style_textarea(self, text: &mut InputField<'_>) {
        text.set_style(Style::default().fg(self.fg).bg(self.panel));
        text.set_cursor_line_style(Style::default());
        text.set_cursor_style(Style::default().fg(self.on_accent()).bg(self.accent));
        text.set_selection_style(self.selected_style());
    }
}

/// User preferences for every project, saved as `appearance.json` (its name
/// from when it held only the theme). Older files lack newer keys, which
/// then take their defaults.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct Preferences {
    pub theme: Theme,
    pub vim_motions: bool,
    pub notifications: notify::Backends,
}

pub(super) fn load(path: &Path) -> Result<Preferences> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Preferences::default()),
        Err(error) => return Err(error).with_context(|| format!("Read {}", path.display())),
    };
    let preferences: Preferences = serde_json::from_slice(&bytes)
        .with_context(|| format!("Read theme from {}", path.display()))?;
    Ok(preferences)
}

pub(super) fn save(path: &Path, preferences: &Preferences) -> Result<()> {
    storage::write_json(path, preferences)
}

/// Bracket each ` / `-separated alternative: `J / K` -> `[J] / [K]`.
pub fn key_label(key: &str) -> String {
    key.split(" / ")
        .map(|part| format!("[{part}]"))
        .collect::<Vec<_>>()
        .join(" / ")
}

/// Display width of [`key_label(key)`] without allocating a formatted string.
pub(super) fn key_label_width(key: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    key.width() + 2 * (key.matches(" / ").count() + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_theme(theme: Theme) -> Preferences {
        Preferences {
            theme,
            ..Preferences::default()
        }
    }

    #[test]
    fn preferences_keep_notifications_and_read_files_without_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("appearance.json");
        fs::write(&path, r#"{"theme":"nord","vim_motions":true}"#).unwrap();
        let mut old = load(&path).unwrap();
        assert_eq!(old.theme, Theme::Nord);
        assert!(old.vim_motions);
        assert_eq!(old.notifications, notify::Backends::default());
        old.notifications.desktop = true;
        old.notifications.webhook_url = "https://ntfy.sh/x".into();
        save(&path, &old).unwrap();
        let again = load(&path).unwrap();
        assert_eq!(again.theme, Theme::Nord);
        assert!(again.vim_motions && again.notifications.desktop);
        assert_eq!(again.notifications.webhook_url, "https://ntfy.sh/x");
        // The CLI reads the same file.
        assert!(notify::load_backends(&path).desktop);
    }

    #[test]
    fn appearance_defaults_and_invalid_files_are_recoverable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("appearance.json");
        assert_eq!(load(&path).unwrap().theme, Theme::ModelRouter);
        fs::write(&path, "{}").unwrap();
        assert_eq!(load(&path).unwrap().theme, Theme::ModelRouter);
        for invalid in [
            r#"{"theme":"unknown-theme"}"#,
            "not json",
            r#"{"theme":42}"#,
        ] {
            fs::write(&path, invalid).unwrap();
            assert!(load(&path).is_err());
        }
        save(&path, &with_theme(Theme::CatppuccinLatte)).unwrap();
        assert_eq!(load(&path).unwrap().theme, Theme::CatppuccinLatte);
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
            save(&path, &with_theme(theme)).unwrap();
            assert_eq!(load(&path).unwrap().theme, theme);
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
        // The terminal's own scheme decides Terminal's contrast.
        for theme in Theme::ALL.into_iter().filter(|t| *t != Theme::Terminal) {
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
            for (name, style) in [
                ("added", p.diff_add()),
                ("removed", p.diff_del()),
                ("hunk", p.diff_hunk()),
            ] {
                // The theme's own status colours (Nord's red is the lowest);
                // the +/− prefix tells the lines apart too.
                assert!(
                    contrast(style.fg.unwrap(), p.panel) >= 2.4,
                    "{theme:?}: {name} diff lines"
                );
            }
            for style in [p.search_match(), p.search_current()] {
                assert!(
                    contrast(style.fg.unwrap(), style.bg.unwrap()) >= 3.0,
                    "{theme:?}: search match"
                );
            }
        }
    }

    #[test]
    fn terminal_theme_uses_ansi_colours_and_reverses_selections() {
        let p = Theme::Terminal.palette();
        assert!(p.follows_terminal());
        let fields = [
            p.bg, p.panel, p.fg, p.muted, p.accent, p.line, p.success, p.warn, p.red, p.blue,
            p.violet,
        ];
        assert!(fields.iter().all(|c| !matches!(c, Color::Rgb(..))));
        // Diff and search colours are ANSI too, so they follow the terminal's scheme.
        for style in [
            p.diff_add(),
            p.diff_del(),
            p.diff_hunk(),
            p.diff_file(),
            p.diff_meta(),
            p.search_match(),
            p.search_current(),
        ] {
            assert!(
                [style.fg, style.bg]
                    .iter()
                    .flatten()
                    .all(|c| !matches!(c, Color::Rgb(..))),
                "{style:?}"
            );
        }
        assert_eq!(p.diff_add().fg, Some(Color::Green));
        assert_eq!(p.diff_del().fg, Some(Color::Red));
        assert!(p.selected_style().add_modifier.contains(Modifier::REVERSED));
        assert_eq!(p.on_accent(), Color::Black);
        let model_router = Theme::ModelRouter.palette();
        assert!(
            !model_router
                .selected_style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(model_router.on_accent(), model_router.bg);
    }

    #[test]
    fn key_label_width_matches_key_label() {
        use unicode_width::UnicodeWidthStr;
        for key in ["a", "Enter", "J / K", "↑↓ / Ctrl+N/P", "Ctrl+S", " "] {
            assert_eq!(key_label_width(key), key_label(key).width(), "{key}");
        }
    }
}
