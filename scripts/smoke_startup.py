#!/usr/bin/env python3
"""Verify the real startup animation, skip controls, resizing, and terminal cleanup.

Run: uv run --with-requirements scripts/requirements-smoke.txt python scripts/smoke_startup.py
"""

import os
import signal
import subprocess
import tempfile
import time
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

from smoke_common import ARTIFACTS, BINARY, ROOT, Terminal, records

BACKGROUND = (13, 20, 33)
FOREGROUND = (222, 231, 242)


def render(screen):
    """Capture the actual terminal cells and colors, not a separate animation."""
    font_path = Path("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf")
    font = (
        ImageFont.truetype(str(font_path), 18)
        if font_path.exists()
        else ImageFont.load_default(size=18)
    )
    cell_width, cell_height = 11, 24
    image = Image.new(
        "RGB", (screen.columns * cell_width, screen.lines * cell_height), BACKGROUND
    )
    draw = ImageDraw.Draw(image)
    for y, row in screen.buffer.items():
        for x, cell in row.items():
            if cell.data.strip():
                color = f"#{cell.fg}" if len(cell.fg) == 6 else FOREGROUND
                draw.text(
                    (x * cell_width, y * cell_height), cell.data, font=font, fill=color
                )
    return image


def wait_for_intro(terminal):
    terminal.wait_for(
        lambda: "Any key to skip" in terminal.text(),
        "Animation did not appear",
        timeout=3,
    )


def main():
    with tempfile.TemporaryDirectory(prefix="router startup ") as name:
        project = Path(name).resolve()
        (project / ".model-router.toml").write_text(
            (ROOT / "examples/demo.toml").read_text()
        )
        animation_env = {"MODEL_ROUTER_NO_ANIMATION": "0", "NO_COLOR": ""}

        terminal = Terminal(project, env=animation_env, size=(80, 24))
        try:
            started = time.monotonic()
            wait_for_intro(terminal)
            frames, timings, states = [], [], set()
            while "1 Workflow" not in terminal.text():
                frames.append(render(terminal.screen))
                timings.append(time.monotonic())
                states.add(terminal.text())
                terminal.pump(0.04)
                assert time.monotonic() - started < 4, (
                    "Startup did not finish automatically"
                )
            assert len(states) >= 6, "The animation did not visibly advance"
            assert not records(project), "Startup launched a workflow"
            durations = [
                max(20, int((b - a) * 1000)) for a, b in zip(timings, timings[1:])
            ]
            durations.append(300)
            frames[0].save(
                ARTIFACTS / "startup.gif",
                save_all=True,
                append_images=frames[1:],
                duration=durations,
                loop=0,
            )
            frames[len(frames) // 2].save(ARTIFACTS / "startup.png")
            terminal.snapshot("startup-to-workflow.txt")
            terminal.send("q")
            terminal.exit()
        finally:
            terminal.cleanup()

        # The skip key is consumed, so 'e' must not also open the stage editor.
        terminal = Terminal(project, env=animation_env)
        try:
            wait_for_intro(terminal)
            started = time.monotonic()
            # The TUI redraws on demand; a second frame may wait for the 1s tick.
            terminal.send("e", settle=None)
            terminal.wait_for(
                lambda: "1 Workflow" in terminal.text(),
                "Key did not skip animation",
                timeout=1,
            )
            assert time.monotonic() - started < 1
            assert "Edit stage" not in terminal.text(), (
                "Skip key leaked into the workflow editor"
            )
            terminal.send("q")
            terminal.exit()
        finally:
            terminal.cleanup()

        for extra, env in [
            (["--no-animation"], animation_env),
            ([], {"MODEL_ROUTER_NO_ANIMATION": "1"}),
        ]:
            terminal = Terminal(project, extra, env=env)
            try:
                terminal.wait_for(
                    lambda: "1 Workflow" in terminal.text(),
                    "Bypass did not open the TUI",
                    timeout=1,
                )
                assert b"Any key to skip" not in terminal.raw, (
                    "Disabled startup still animated"
                )
                terminal.send("q")
                terminal.exit()
            finally:
                terminal.cleanup()

        terminal = Terminal(project, env=animation_env)
        try:
            wait_for_intro(terminal)
            terminal.resize(40, 10, settle="clear")
            terminal.wait_for(
                lambda: (
                    "MODEL ROUTER" in terminal.text()
                    and "Any key to skip" in terminal.text()
                ),
                "Compact animation did not render",
            )
            terminal.resize(80, 24, settle="clear")
            terminal.send("\x1b[200~pasted text\x1b[201~")
            terminal.wait_for(
                lambda: "1 Workflow" in terminal.text(),
                "Paste/resize failed to return to the TUI",
            )
            terminal.send("q")
            terminal.exit()
        finally:
            terminal.cleanup()

        for interrupt in ("key", "signal"):
            terminal = Terminal(project, env=animation_env)
            try:
                wait_for_intro(terminal)
                if interrupt == "key":
                    terminal.send(b"\x03")
                else:
                    os.kill(terminal.pid, signal.SIGTERM)
                terminal.exit()
                assert b"1 Workflow" not in terminal.raw, (
                    "Interrupted startup entered the workflow"
                )
            finally:
                terminal.cleanup()

        result = subprocess.run(
            [str(BINARY), "-C", str(project), "preview"],
            text=True,
            capture_output=True,
            check=True,
        )
        assert (
            "Any key to skip" not in result.stdout and "\x1b[" not in result.stdout
        ), "Startup affected CLI output"
        assert not records(project), "Startup tests launched a model"
    print(
        "PASS: animated frames, automatic completion, skip consumption, flag/env bypass, small-screen resize, paste, Ctrl+C/SIGTERM cleanup, and unchanged CLI output."
    )
    print(f"Preview: {ARTIFACTS / 'startup.gif'}")


if __name__ == "__main__":
    main()
