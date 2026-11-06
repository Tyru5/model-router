#!/usr/bin/env python3
"""Check stage-scoped harness settings in a real PTY, without inference.

Run: uv run --with-requirements scripts/requirements-smoke.txt python scripts/smoke_harness_settings.py
"""

import json
from pathlib import Path
import tempfile
import tomllib

from smoke_common import ARTIFACTS, Terminal, records

NEXT, PREVIOUS, APPLY, ESC = b"\x0e", b"\x10", b"\x13", b"\x1b"
RIGHT, F7 = b"\x1b[C", b"\x1b[18~"


def replace(terminal, text):
    terminal.send(b"\x01\x0b")
    terminal.send(f"\x1b[200~{text}\x1b[201~")


def save(terminal, path):
    return terminal.save_config(path)["workflows"][0]["stages"]


def main():
    with tempfile.TemporaryDirectory(prefix="router harness settings ") as name:
        project = Path(name)
        config = project / ".model-router.toml"
        config.write_text("""version = 1
[[workflows]]
name = "Stage settings"
[[workflows.stages]]
id = "research"
name = "Investigate"
role = "research"
harness = "codex"
model = "original-model"
instructions = "Keep this stage instruction."
[[workflows.stages]]
id = "implement"
name = "Implement"
role = "implementation"
harness = "claude"
model = "sonnet"
mode = "interactive"
""")
        codex_home = project / "codex-home"
        codex_home.mkdir()
        (codex_home / "models_cache.json").write_text(
            json.dumps(
                {
                    "models": [
                        {
                            "slug": "fixture-model",
                            "display_name": "Fixture model",
                            "supported_reasoning_levels": [
                                {"effort": "low"},
                                {"effort": "high"},
                            ],
                            "default_reasoning_level": "low",
                        },
                    ]
                }
            )
        )
        env = {"CODEX_HOME": str(codex_home), "NO_COLOR": ""}
        terminal = Terminal(project, ["--no-animation"], env=env)
        try:
            terminal.wait_for(
                lambda: "1 Workflow" in terminal.text(), "App did not start"
            )
            original = save(terminal, config)
            terminal.send("c")
            terminal.wait_for(
                lambda: "Harness settings" in terminal.text(),
                "Settings shortcut missing",
            )
            replace(terminal, "cancel-this")
            terminal.send(ESC)
            terminal.wait_for(
                lambda: "Discard changes?" in terminal.text(),
                "Esc did not ask before discarding the draft",
            )
            terminal.send("y")
            assert save(terminal, config) == original, "Cancel changed stage settings"

            terminal.send("c")
            replace(terminal, "draft-model")
            terminal.send(F7)
            terminal.wait_for(
                lambda: "codex selections" in terminal.text(), "Model picker missing"
            )
            terminal.send(ESC)
            terminal.wait_for(
                lambda: "Harness settings" in terminal.text(), "Esc lost the draft"
            )
            assert "draft-model" in terminal.text()
            terminal.send(F7)
            terminal.wait_for(
                lambda: "codex selections" in terminal.text(), "Model picker missing"
            )
            terminal.send(NEXT)
            terminal.send(PREVIOUS)
            terminal.send(NEXT)
            terminal.send("\r")
            terminal.wait_for(
                lambda: "Harness settings" in terminal.text(),
                "Model selection did not return to settings",
            )
            assert "fixture-model" in terminal.text()
            terminal.send(NEXT * 2)  # effort
            terminal.wait_for(
                lambda: "Levels from Codex local cache" in terminal.text(),
                "Catalog effort choices did not load",
            )
            terminal.send(RIGHT * 2)  # default -> low -> high
            terminal.send(NEXT * 2)  # timeout
            replace(terminal, "0")
            terminal.send(APPLY)
            terminal.wait_for(
                lambda: "Timeout must be greater than zero" in terminal.text(),
                "Validation error not visible",
            )
            assert (
                tomllib.loads(config.read_text())["workflows"][0]["stages"] == original
            )
            replace(terminal, "120")
            terminal.send(NEXT)  # sandbox
            terminal.send(RIGHT * 2)  # default -> read-only -> workspace-write
            terminal.snapshot("harness-settings-codex-120x36.txt")
            terminal.send(NEXT)  # approval policy
            terminal.send(RIGHT * 2)  # default -> on-request -> never
            terminal.send(NEXT)  # extra args
            replace(terminal, "--sandbox read-only")
            terminal.send(APPLY)
            terminal.wait_for(
                lambda: "also set in Extra arguments" in terminal.text(),
                "Conflicting permission arguments accepted",
            )
            replace(terminal, "--config 'example=\"value with spaces\"'")
            terminal.send(APPLY)
            terminal.wait_for(
                lambda: "Harness settings applied" in terminal.text(),
                "Settings did not apply",
            )
            changed = save(terminal, config)
            assert changed[0]["model"] == "fixture-model"
            assert (
                changed[0]["effort"] == "high" and changed[0]["timeout_seconds"] == 120
            )
            assert changed[0]["harness_settings"] == {
                "sandbox": "workspace-write",
                "approval_policy": "never",
            }
            assert changed[0]["extra_args"] == [
                "--config",
                'example="value with spaces"',
            ]
            assert changed[0]["instructions"] == original[0]["instructions"]
            assert changed[1] == original[1], "Editing Codex changed the Claude stage"

            terminal.resize(80, 24)
            terminal.send("jc")
            terminal.wait_for(
                lambda: (
                    "Harness settings" in terminal.text()
                    and "claude" in terminal.text()
                ),
                "Claude settings missing",
            )
            assert (
                "Provider" not in terminal.text() and "Sandbox" not in terminal.text()
            )
            terminal.send(
                NEXT * 4
            )  # Claude: model, effort, execution, timeout, permissions
            terminal.send(RIGHT * 3)  # default -> manual -> acceptEdits -> plan
            terminal.snapshot("harness-settings-claude-80x24.txt")
            assert (
                "[Ctrl+S] apply" in terminal.text()
                and "[Esc] cancel" in terminal.text()
            )
            terminal.send(APPLY)
            changed = save(terminal, config)
            assert changed[1]["harness_settings"] == {"permission_mode": "plan"}
            assert changed[1]["mode"] == "interactive"
            assert not records(project), "Editing settings started a harness"
            terminal.send("q")
            terminal.exit()
        finally:
            terminal.cleanup()

        terminal = Terminal(project, ["--no-animation"], env=env, size=(65, 20))
        try:
            terminal.wait_for(
                lambda: "1 Workflow" in terminal.text(), "App did not restart"
            )
            terminal.send("c")
            terminal.wait_for(
                lambda: "Harness settings" in terminal.text(),
                "Saved settings did not reopen",
            )
            terminal.send(NEXT * 5)
            assert "workspace-write" in terminal.text(), (
                "Saved sandbox was not restored"
            )
            assert "[Ctrl+S] apply" in terminal.text(), (
                "Apply hint hidden at minimum size"
            )
            terminal.snapshot("harness-settings-codex-65x20.txt")
            terminal.send(ESC)
            terminal.send("q")
            terminal.exit()
        finally:
            terminal.cleanup()
    print(
        "PASS: harness settings, model browsing, Ctrl+N/P, cancellation, validation, stage isolation, native permissions, save/restart, 120x36/80x24/65x20 layouts, and terminal cleanup."
    )
    print(f"Artifacts: {ARTIFACTS}")


if __name__ == "__main__":
    main()
