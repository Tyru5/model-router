#!/usr/bin/env python3
"""Exercise stage/workflow presets in real terminals without model requests.

Run: uv run --with pyte python scripts/smoke_stages.py
"""

import json
from pathlib import Path
import tempfile
import tomllib

from smoke_tui import ARTIFACTS, ROOT, Terminal, records


def choose_stage(terminal, name):
    terminal.send("a")
    terminal.wait(
        lambda: "Add stage from preset" in terminal.text(), "Stage picker missing"
    )
    terminal.send(name)
    terminal.send("\r")
    terminal.wait(
        lambda: "Edit stage" in terminal.text(), "Preset did not open for editing"
    )


def save(terminal, path):
    terminal.send("s")
    return tomllib.loads(path.read_text())["workflows"]


def main():
    with tempfile.TemporaryDirectory(prefix="router stage presets ") as name:
        project = Path(name)
        path = project / ".model-router.toml"
        config = (
            (ROOT / "examples/demo.toml")
            .read_text()
            .replace("examples/mock_harness.py", str(ROOT / "examples/mock_harness.py"))
        )
        path.write_text(config)
        terminal = Terminal(project, ["--no-animation"])
        try:
            terminal.wait(lambda: "1 Workflow" in terminal.text(), "TUI did not start")
            terminal.send("t")
            terminal.send("Keep this task while changing workflows")
            terminal.send(b"\x13")
            terminal.send("a")
            terminal.snapshot("stage-presets-120x36.txt")
            terminal.send(b"\x1b")
            assert len(save(terminal, path)[0]["stages"]) == 3
            choose_stage(terminal, "Plan implementation")
            terminal.send(b"\x1b")
            assert len(save(terminal, path)[0]["stages"]) == 3, (
                "Cancelled editor inserted a stage"
            )
            terminal.resize(80, 24)
            terminal.pump()
            terminal.send("a")
            terminal.wait(
                lambda: "Add stage from preset" in terminal.text(),
                "Stage picker missing after resize",
            )
            terminal.send("Security")
            terminal.wait(
                lambda: "Security review" in terminal.text(), "Preset search missing"
            )
            assert "Default:" in terminal.text(), "Preset routing is hidden"
            terminal.snapshot("stage-presets-80x24.txt")
            terminal.send(b"\x1b")
            choose_stage(terminal, "Plan implementation")
            terminal.send(b"\x13")
            stages = save(terminal, path)[0]["stages"]
            assert [s["role"] for s in stages] == [
                "research",
                "planning",
                "implementation",
                "review",
            ]
            assert stages[1]["instructions"] and stages[1]["model"] == "gpt-6-astra"
            terminal.send("Jv ")  # move after implementation, toggle mode, disable
            stages = save(terminal, path)[0]["stages"]
            assert stages[2]["role"] == "planning" and not stages[2]["enabled"]
            assert stages[2]["mode"] == "interactive"
            terminal.send("d")
            stages = save(terminal, path)[0]["stages"]
            assert len(stages) == 5 and stages[2]["id"] != stages[3]["id"]
            terminal.send("x")
            choose_stage(terminal, "Custom stage")
            terminal.send(b"\x01\x0b")
            terminal.send("My custom stage")
            terminal.send(b"\x13")
            stages = save(terminal, path)[0]["stages"]
            assert stages[-1]["name"] == "My custom stage", [s["name"] for s in stages]
            terminal.send("x")
            original = save(terminal, path)[0]

            for attempt in range(2):
                terminal.send("w")
                terminal.send("Preset: Full feature")
                terminal.wait(
                    lambda: "Reviews start disabled" in terminal.text(),
                    "Preset defaults missing",
                )
                assert (
                    "[Enter] choose" in terminal.text()
                    and "[Esc] cancel" in terminal.text()
                ), "Long preset details hid keyboard controls"
                if attempt == 0:
                    terminal.snapshot("workflow-presets-80x24.txt")
                terminal.send("\r")
                workflows = save(terminal, path)
                assert len(workflows) == attempt + 2
                assert workflows[0] == original, (
                    "Preset overwrote the original workflow"
                )
                assert len(workflows[-1]["stages"]) == 10
                assert all(
                    not s["enabled"]
                    for s in workflows[-1]["stages"]
                    if s["role"] == "review"
                )
                assert "Keep this task" in terminal.text()
            assert workflows[-1]["name"] == "Full feature (2)"
            terminal.resize(120, 36)
            terminal.wait(
                lambda: "Selected stage" in terminal.text(),
                "TUI did not redraw after resizing",
            )
            terminal.send("wOffline demo\r")
            terminal.wait(
                lambda: "Offline demo" in terminal.text(),
                "Saved workflow did not reopen",
            )
            assert not records(project), "Preset selection started a harness"
            terminal.send("q")
            terminal.exit()
        finally:
            terminal.cleanup()

        terminal = Terminal(project, ["--no-animation"])
        try:
            terminal.wait(
                lambda: "Plan implementation" in terminal.text(),
                "Stage did not survive restart",
            )
            terminal.send("wFull feature (2)\r")
            terminal.wait(
                lambda: "Full feature (2)" in terminal.text(),
                "Workflow did not survive restart",
            )
            terminal.snapshot("full-feature-120x36.txt")
            terminal.send("q")
            terminal.exit()
        finally:
            terminal.cleanup()

        # A test stage needs a real handoff file in interactive mode, just like a review.
        path.write_text(f"""version = 1
[adapters.mock]
command = "python3"
args = []
interactive_args = [{json.dumps(str(ROOT / "examples/mock_harness.py"))}, "--role", "verify", "--prompt-file", "{{prompt_file}}", "--handoff-file", "{{handoff_file}}", "--interactive", "--no-handoff", "--test-status", "passed"]
[[workflows]]
name = "Interactive verification"
[[workflows.stages]]
id = "verify"
name = "Verify tests"
role = "test"
harness = "mock"
mode = "interactive"
""")
        terminal = Terminal(project, ["run", "--task", "Verify without a handoff file"])
        try:
            terminal.wait(
                lambda: "MOCK INTERACTIVE READY" in terminal.text(),
                "Verification did not open",
            )
            terminal.send("done\n")
            terminal.exit(expected=1)
            assert records(project)[-1]["status"] == "blocked"
        finally:
            terminal.cleanup()
    print(
        "PASS: preset search/details, transactional stage creation, reorder/toggle/duplicate/custom stages, workflow presets, optional reviews, persistence, task preservation, 80x24/120x36 layouts, and interactive verification handoff enforcement."
    )
    print(f"Artifacts: {ARTIFACTS}")


if __name__ == "__main__":
    main()
