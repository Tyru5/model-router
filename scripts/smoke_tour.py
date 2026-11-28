#!/usr/bin/env python3
"""Follow the getting-started tour on a first launch to a completed run.

Offline: the demo workflow's stages run examples/mock_harness.py.

Run: uv run --with-requirements scripts/requirements-smoke.txt python scripts/smoke_tour.py
"""

import json
import tempfile
from pathlib import Path

from smoke_common import Terminal, demo_config, records, require_binary


def main():
    require_binary()
    with tempfile.TemporaryDirectory(prefix="router tour test ") as name:
        project = Path(name).resolve()
        (project / ".model-router.toml").write_text(demo_config())
        terminal = Terminal(project, ["--no-animation"], tour=True)
        tour_file = project / "config-home/model-router/tour.json"
        try:
            # A first launch opens the tour by itself.
            terminal.wait_for_text(
                "Tour · 1 of 6 · Harnesses", message="Tour did not open"
            )
            terminal.wait_for_text("Press [3]", message="Tour lacks its first key")
            terminal.snapshot("tour-start.txt")
            terminal.send("3")
            # The demo's stages run python3, which the checks find.
            terminal.wait_for_text(
                "Tour · 2 of 6 · Stages", message="Harness checks did not advance"
            )
            terminal.send("1")
            terminal.wait_for_text(
                "Tour · 3 of 6 · Task", message="Workflow did not reach the task"
            )
            terminal.send("t")
            terminal.send("\x1b[200~Add a --version flag\x1b[201~")
            terminal.send(b"\x13")
            terminal.wait_for_text(
                "Tour · 4 of 6 · Start", message="The task did not advance"
            )
            terminal.send("r")
            terminal.wait_for_text(
                "Press [2] to follow it", message="The run was not followed"
            )
            terminal.send("2")
            terminal.wait_for(
                lambda: (
                    records(project) and records(project)[-1]["status"] == "complete"
                ),
                "Run did not complete",
                timeout=30,
            )
            terminal.wait_for_text(
                "Tour · 6 of 6 · Done", message="The tour did not finish"
            )
            terminal.snapshot("tour-done.txt")
            assert json.loads(tour_file.read_text()) == {"outcome": "finished"}
            terminal.send("\x1bOP")  # F1 closes it
            terminal.wait_for_gone("Tour ·", message="F1 did not close the tour")
            terminal.send("q")
            terminal.exit()
        finally:
            terminal.cleanup()

        # A finished tour does not open again.
        terminal = Terminal(project, ["--no-animation"], tour=True)
        try:
            terminal.wait_for_text("1 Workflow", message="TUI did not render")
            terminal.drain()
            assert "Tour ·" not in terminal.text(), terminal.text()
            terminal.send("q")
            terminal.exit()
        finally:
            terminal.cleanup()

        # Someone who used the TUI before the tour existed hears of F1 once.
        config = project / "earlier-user"
        config.mkdir()
        (config / "recent-projects.json").write_text(json.dumps([str(project)]))
        env = {"MODEL_ROUTER_CONFIG_DIR": str(config)}
        for launch in ("first", "second"):
            terminal = Terminal(project, ["--no-animation"], env=env, tour=True)
            try:
                terminal.wait_for_text("1 Workflow", message="TUI did not render")
                terminal.drain()
                screen = terminal.text()
                assert "Tour ·" not in screen, screen
                offered = "New: [F1] opens a tour" in screen
                assert offered == (launch == "first"), screen
                terminal.send("q")
                terminal.exit()
            finally:
                terminal.cleanup()
        assert json.loads((config / "tour.json").read_text()) == {"outcome": "offered"}
    print(
        "PASS: first-launch tour from Harnesses to a completed run, F1, no reopen, one-time F1 offer to earlier users."
    )


if __name__ == "__main__":
    main()
