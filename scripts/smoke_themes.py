#!/usr/bin/env python3
"""Exercise theme previews, persistence, and picker colours in actual terminals.

Run: uv run --with pyte python scripts/smoke_themes.py
"""

import json
from pathlib import Path
import tempfile

from smoke_tui import ARTIFACTS, ROOT, Terminal, records


THEMES = [
    ("Model Router", "model-router", "0d1421"),
    # The terminal's own background, which pyte reports as "default".
    ("Terminal", "terminal", "default"),
    ("Catppuccin Mocha", "catppuccin-mocha", "181825"),
    ("Dracula", "dracula", "282a36"),
    ("Nord", "nord", "2e3440"),
    ("Tokyo Night", "tokyo-night", "16161e"),
    ("Gruvbox Dark", "gruvbox-dark", "282828"),
    ("One Dark", "one-dark", "21252b"),
    ("Monokai", "monokai", "272822"),
    ("Solarized Dark", "solarized-dark", "002b36"),
    ("Poimandres", "poimandres", "1b1e28"),
    ("Catppuccin Latte", "catppuccin-latte", "eff1f5"),
    ("Gruvbox Light", "gruvbox-light", "fbf1c7"),
    ("Solarized Light", "solarized-light", "fdf6e3"),
]


def background(terminal):
    return terminal.screen.buffer[0][0].bg


def choose(terminal, name):
    terminal.send("T")
    terminal.wait(
        lambda: "Choose theme" in terminal.text(), "Theme picker did not open"
    )
    terminal.send(name)


def main():
    with tempfile.TemporaryDirectory(prefix="router themes ") as name:
        root = Path(name)
        project, other = root / "project", root / "other"
        config_home = root / "config-home"
        prefs = config_home / "model-router/appearance.json"
        config = (ROOT / "examples/demo.toml").read_text()
        for folder in (project, other):
            folder.mkdir()
            (folder / ".model-router.toml").write_text(config)
            (folder / "notes.md").write_text("A theme must not change task contents")
        env = {
            "XDG_CONFIG_HOME": str(config_home),
            "NO_COLOR": "",
            "MODEL_ROUTER_NO_ANIMATION": "0",
        }
        terminal = Terminal(project, ["--no-animation"], extra_env=env, size=(80, 24))
        try:
            terminal.wait(lambda: "1 Workflow" in terminal.text(), "App did not start")
            assert background(terminal) == "0d1421"
            assert "[T] themes" in terminal.text()
            terminal.send("tKeep my task")
            terminal.send(b"\x13")
            terminal.send("T")
            terminal.send(b"\x0e")
            assert background(terminal) == "default", (
                "Ctrl+N did not preview the next theme"
            )
            terminal.send(b"\x10")
            assert background(terminal) == "0d1421", (
                "Ctrl+P did not preview the previous theme"
            )
            terminal.send(b"\x1b[B")
            terminal.send(b"\x1b")
            assert background(terminal) == "0d1421", "Esc did not restore the theme"
            assert not prefs.exists(), "Preview wrote preferences before Enter"

            for label, theme_id, bg in THEMES:
                choose(terminal, label)
                assert background(terminal) == bg, (
                    f"{label} preview did not repaint the app"
                )
                assert (
                    "[Ctrl+N/P] select" in terminal.text()
                    and "[Enter] saves" in terminal.text()
                )
                terminal.send("\r")
                assert "Choose theme" not in terminal.text()
                assert json.loads(prefs.read_text())["theme"] == theme_id
                assert background(terminal) == bg and "Keep my task" in terminal.text()
                terminal.snapshot(f"theme-{theme_id}-80x24.txt")
                terminal.send("f")
                assert "Import task/spec" in terminal.text()
                assert background(terminal) == bg
                terminal.send(b"\x1b")

            choose(terminal, "no-such-theme")
            terminal.send(b"\x0e\x10\r")
            assert "Choose theme" in terminal.text()
            assert json.loads(prefs.read_text())["theme"] == "solarized-light"
            terminal.send(b"\x1b")
            for page in "2341":
                terminal.send(page)
                choose(terminal, "Poimandres")
                terminal.send(b"\x1b")
                assert background(terminal) == "fdf6e3", (
                    "Preview changed the saved theme"
                )

            choose(terminal, "Poimandres")
            terminal.send("\r")
            terminal.send("p")
            terminal.send(b"\x0c")
            terminal.send(f"\x1b[200~{other}\x1b[201~\r")
            terminal.send("\r")
            terminal.wait(
                lambda: "Choose repository" not in terminal.text(),
                "Repository switch failed",
            )
            assert background(terminal) == "1b1e28", "Repository switch lost the theme"
            terminal.send("q")
            terminal.exit()
        finally:
            terminal.cleanup()

        # A new process loads the global choice before drawing its startup animation.
        terminal = Terminal(other, extra_env=env, size=(120, 36))
        try:
            terminal.wait(
                lambda: "Any key to skip" in terminal.text(), "Intro did not appear"
            )
            assert background(terminal) == "1b1e28", "Startup ignored the saved theme"
            terminal.send(" ")
            terminal.wait(
                lambda: "1 Workflow" in terminal.text(), "Intro did not finish"
            )
            terminal.send("T")
            assert "Poimandres · dark (current)" in terminal.text()
            terminal.snapshot("theme-picker-120x36.txt")
            terminal.send(b"\x1b")
            terminal.send("q")
            terminal.exit()
        finally:
            terminal.cleanup()

        # Corrupt preferences must not prevent starting or choosing a replacement.
        prefs.write_text('{"theme":"removed-theme"}')
        terminal = Terminal(project, ["--no-animation"], extra_env=env)
        try:
            terminal.wait(
                lambda: "1 Workflow" in terminal.text(),
                "Bad preferences blocked startup",
            )
            assert background(terminal) == "0d1421"
            assert "Read theme" in terminal.text()
            choose(terminal, "Catppuccin Latte")
            terminal.send("\r")
            assert json.loads(prefs.read_text())["theme"] == "catppuccin-latte"
            terminal.send("q")
            terminal.exit()
        finally:
            terminal.cleanup()

        for folder in (project, other):
            assert (folder / ".model-router.toml").read_text() == config, (
                "Theme changed a workflow"
            )
            assert not records(folder), "Theme selection started a run"
    print(
        f"PASS: all {len(THEMES)} themes, live colour previews, Ctrl+N/P, cancel/apply, empty searches, global persistence, repository switching, themed startup, corrupt preferences, and unchanged workflows."
    )
    print(f"Artifacts: {ARTIFACTS}")


if __name__ == "__main__":
    main()
