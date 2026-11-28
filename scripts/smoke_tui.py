#!/usr/bin/env python3
"""Exercise the real TUI and nested terminal with an offline mock harness.

Run: uv run --with-requirements scripts/requirements-smoke.txt python scripts/smoke_tui.py
"""

import json
import tempfile
from pathlib import Path

from smoke_common import (
    ARTIFACTS,
    ROOT,
    Terminal,
    demo_config,
    records,
    require_binary,
)


def main():
    require_binary()
    with tempfile.TemporaryDirectory(prefix="router terminal test ") as name:
        project = Path(name).resolve()
        path = project / ".model-router.toml"
        path.write_text(demo_config())
        terminal = Terminal(project)
        try:
            terminal.wait_for_text("1 Workflow", message="TUI did not render")
            assert not records(project), "Opening the router started a run"
            terminal.snapshot("workflow-120x36.txt")
            terminal.resize(80, 24)
            terminal.wait_for_line(
                r"○ Review \(optional\)",
                message="Optional review stage is hidden from the strip at 80x24",
            )
            terminal.snapshot("workflow-80x24.txt")
            terminal.resize(120, 36)
            # The Space menu reaches any action: g, then h shows Harnesses.
            terminal.send(" ")
            terminal.wait_for_text(
                "Press a group letter", message="Space menu did not open"
            )
            terminal.send("gh")
            terminal.wait_for_text(
                "╭ Harnesses", message="Space g h did not go to Harnesses"
            )
            terminal.send("1")
            # t edits the task in its panel; the stages stay in view.
            terminal.send("t")
            terminal.send(
                "\x1b[200~Build a feature with 雪\nand preserve context between stages.\x1b[201~"
            )
            terminal.wait_for(
                lambda: (
                    "[Ctrl+S] done" in terminal.text()
                    and "Build a feature with 雪" in terminal.text()
                    and "stage 1 of 4" in terminal.text()
                ),
                "Task did not open in its panel",
            )
            terminal.send(b"\x13")
            terminal.send("\x1b[C\x1b[Co")  # → → selects the review; o enables it
            terminal.wait_for_text("stage 3 of 4", message="→ did not select stages")
            terminal.send("o")  # disable it again
            terminal.send("\x1b[Dve")  # ←: implementation; interactive, then edit
            terminal.send(b"\x01\x0b")  # beginning/end-of-line deletion in text editor
            terminal.send("Interactive implementation")
            terminal.send(b"\x13")
            saved = terminal.save_config(path)
            assert saved["workflows"][0]["stages"][1]["mode"] == "interactive"
            assert (
                saved["workflows"][0]["stages"][1]["name"]
                == "Interactive implementation"
            )
            assert not saved["workflows"][0]["stages"][2]["enabled"]
            terminal.send("r")
            # The editor stays open for the next workflow; the run announces
            # its interactive stage instead of taking over the screen.
            terminal.wait_for(
                lambda: "ready in its native terminal" in terminal.text(),
                "Interactive stage was not announced",
            )
            assert "1 Workflow" in terminal.text() and "Runs ●1" in terminal.text()
            terminal.send("2")
            terminal.send("i", settle="quiet")
            terminal.wait_for(
                lambda: "MOCK INTERACTIVE READY" in terminal.text(),
                "Interactive stage did not receive a terminal",
            )
            terminal.send("done 雪\n", settle=None)
            terminal.wait_for(
                lambda: (
                    records(project)
                    and records(project)[-1]["status"] == "awaiting_confirmation"
                ),
                "No interactive checkpoint",
            )
            # The run file updates before the TUI receives the checkpoint.
            terminal.wait_for_text(
                "Inspect the handoff: [h]", message="TUI did not show the checkpoint"
            )
            # The card under the feed names the stage and its keys.
            terminal.wait_for(
                lambda: (
                    "◆ Interactive implementation needs you" in terminal.text()
                    and "[a] accept" in terminal.text()
                ),
                "Needs-you card missing",
            )
            terminal.snapshot("interactive-checkpoint.txt")
            terminal.send("h")
            terminal.wait_for(
                lambda: "implementation evidence" in terminal.text(),
                "Handoff inspector missing",
            )
            terminal.snapshot("handoff.txt")
            terminal.send(b"\x1b")
            terminal.send("a")
            terminal.wait_for(
                lambda: records(project)[-1]["status"] == "complete",
                "Run did not complete",
            )
            record = records(project)[-1]
            assert record["stages"][2]["status"] == "skipped"
            assert not (project / ".model-router/mock/review.count").exists()
            assert (
                project / ".model-router/mock/interactive-input.txt"
            ).read_text() == "done 雪"
            assert (
                "research evidence"
                in (project / ".model-router/mock/implementation.prompt").read_text()
            )
            # A finished run shows its brief, with what to do next pinned below.
            terminal.wait_for(
                lambda: (
                    "Changed during this run" in terminal.text()
                    and "[n] rerun" in terminal.text()
                ),
                "Brief did not show",
            )
            terminal.snapshot("completed.txt")
            # The sidebar lists the run under this session, not Saved.
            terminal.wait_for(
                lambda: (
                    "This session" in terminal.text()
                    and "complete · Offline demo" in terminal.text()
                ),
                "Runs sidebar did not list the session's run",
            )
            terminal.send("\r")
            # The run is still in this session, so Enter keeps it.
            terminal.wait_for(
                lambda: "Run already open in this session" in terminal.text(),
                "History did not focus the session's run",
            )
            # Closing moves it to Saved, which Enter opens again from disk.
            terminal.send("x")
            terminal.wait_for_text(
                "Saved · 1", message="Closed run did not move to Saved"
            )
            terminal.send("\r")
            terminal.wait_for_text(
                "Saved run loaded. d shows its changes; n reruns it.",
                message="Enter did not open the saved run",
            )
            terminal.send("q")
            terminal.exit()
        finally:
            terminal.cleanup()

        # An echoed valid-looking review verdict in a PTY must not approve work.
        review = f"""version = 1
[adapters.mock]
command = "python3"
args = []
interactive_args = [{json.dumps(str(ROOT / "examples/mock_harness.py"))}, "--role", "review", "--prompt-file", "{{prompt_file}}", "--handoff-file", "{{handoff_file}}", "--interactive", "--no-handoff"]
[[workflows]]
name = "Interactive review"
[[workflows.stages]]
id = "review"
name = "Review"
role = "review"
harness = "mock"
mode = "interactive"
"""
        path.write_text(review)
        terminal = Terminal(project, ["run", "--task", "Review without a handoff file"])
        try:
            terminal.wait_for(
                lambda: "MOCK INTERACTIVE READY" in terminal.text(),
                "Review did not open",
            )
            terminal.send("done\n", settle=None)
            terminal.exit(expected=1)
            assert records(project)[-1]["status"] == "blocked"
        finally:
            terminal.cleanup()
    print(
        "PASS: Space menu, in-place task editing, stage strip selection, Unicode paste, 80x24 resize, optional review, PTY input, needs-you card, handoff checkpoint, brief, history, terminal restoration, and rejection of unrecorded review verdicts."
    )
    print(f"Artifacts: {ARTIFACTS}")


if __name__ == "__main__":
    main()
