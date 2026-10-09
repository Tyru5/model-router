# Themes

Press **T** (Shift+T) on Workflow, Monitor, Runs, or Harnesses to open the theme
picker. Type a name or `light` / `dark` to filter. Arrow keys and **Ctrl+N/P**
preview the highlighted theme immediately, including the page behind the picker.
**Enter** saves it. **Esc** discards the preview and restores the saved theme.

| Theme | Appearance | Saved ID |
| --- | --- | --- |
| Model Router | Dark; original colours | `model-router` |
| Terminal | Your terminal's colour scheme | `terminal` |
| Catppuccin Mocha | Dark; pastel purple | `catppuccin-mocha` |
| Dracula | Dark; purple and pink | `dracula` |
| Nord | Dark; cool blues | `nord` |
| Tokyo Night | Dark; blue and violet | `tokyo-night` |
| Gruvbox Dark | Dark; warm earth tones | `gruvbox-dark` |
| One Dark | Dark; blue and charcoal | `one-dark` |
| Monokai | Dark; green and pink | `monokai` |
| Solarized Dark | Dark; blue-green | `solarized-dark` |
| Poimandres | Dark; mint and pale blue | `poimandres` |
| Catppuccin Latte | Light; pastel purple | `catppuccin-latte` |
| Gruvbox Light | Light; warm cream | `gruvbox-light` |
| Solarized Light | Light; cream and blue | `solarized-light` |

The app saves `appearance.json` in its user config directory, next to the global
workflow config and recent-project list. On Linux this is
`$XDG_CONFIG_HOME/model-router/appearance.json`, or
`~/.config/model-router/appearance.json` when `XDG_CONFIG_HOME` is unset:

```json
{
  "theme": "catppuccin-mocha"
}
```

Theme changes do not modify project workflows, run snapshots, or task text.
Missing preferences use Model Router. An unreadable file, invalid JSON, or an
unknown theme produces a notice and falls back to Model Router; choosing a theme
can replace an invalid preference. Failed saves keep the previous committed
theme and leave the picker open so you can cancel or retry.

All router views, text fields, selections, error/status colours, and the startup
animation use the same palette. Native interactive harnesses retain their own
colours. `NO_COLOR` keeps its existing terminal behaviour; the saved choice is
still available when colour output is enabled again.

## Palette sources

Built-in themes adapt these upstream editor and terminal palettes:

- [Catppuccin palette](https://catppuccin.com/palette/)
- [Dracula specification](https://draculatheme.com/spec)
- [Nord colours and palettes](https://www.nordtheme.com/docs/colors-and-palettes/)
- [Tokyo Night palette](https://github.com/folke/tokyonight.nvim/tree/main/lua/tokyonight/colors)
- [Gruvbox palette](https://github.com/morhetz/gruvbox/blob/master/colors/gruvbox.vim)
- [One Dark syntax colours](https://github.com/atom/one-dark-syntax/blob/master/styles/colors.less)
- [VS Code's Monokai palette](https://github.com/microsoft/vscode/blob/main/extensions/theme-monokai/themes/monokai-color-theme.json)
- [Solarized values](https://ethanschoonover.com/solarized/#the-values)
- [Poimandres palette](https://github.com/drcmda/poimandres-theme/blob/main/src/theme.js)

UI backgrounds and highlights are mapped to each theme's available tones.

## Terminal theme

**Terminal** uses no fixed colours. It keeps the terminal's default background
and text, and draws the rest with ANSI colours, which the terminal maps to its
own scheme: accent cyan, success green, warnings yellow, errors red, and
secondary text and borders bright black. Selections reverse the terminal's
foreground and background. Changing the terminal's scheme recolours the app
immediately, with no restart. The terminal decides contrast, so a scheme whose
bright black matches its background (some Solarized ports) hides secondary text.
The startup animation appears without its fade.
Dracula's secondary text and the light themes' foreground/status accents include
contrast adjustments for small terminal text. Body and selection text are checked
for at least 4.5:1 contrast; secondary text for at least 3:1. The original
Model Router palette remains the default.

Poimandres uses the upstream dark background, mint accent, pale blue, and pink
status colours. Panels use its hover-widget background; selections use its
opaque focus colour because terminal cells do not support alpha blending.

## Verification

`cargo test --locked` covers preview/cancel/save, failed writes, preference
roundtrips, contrast, and rendering all themes in pages, dialogs, text fields,
and the startup animation. `scripts/smoke_themes.py` exercises the real terminal,
including persistent preferences, repository switching, and invalid preferences.
