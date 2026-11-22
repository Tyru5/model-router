#!/usr/bin/env python3
"""A delegating stage in a real terminal: the scripted mock parent delegates
a slow task through the real bridge, the Runs page shows the task line, and
cancelling the stage stops the task too.

The workflow uses the offline mock harness; no model is ever called. Needs
an OS sandbox backend (bubblewrap on Linux, sandbox-exec on macOS), since
tasks run in their own sandbox; skipped without one.

Run: uv run --with-requirements scripts/requirements-smoke.txt python scripts/smoke_delegation.py
"""

import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from smoke_common import ARTIFACTS, ROOT, Terminal, records, require_binary, trust


def git(project, *args):
    subprocess.run(["git", *args], cwd=project, check=True, capture_output=True)


def config(project):
    mock = str(ROOT / "examples/mock_harness.py")
    script = project / "script.json"
    script.write_text(
        json.dumps(
            {
                "turns": [
                    [{"call": "delegate_task", "args": {"task": "Look around slowly"}}],
                    [{"call": "task_status", "args": {"task_id": "t1"}}],
                ],
                "handoffs": {
                    "1": {"summary": "waiting on tasks", "verdict": "blocked"}
                },
            }
        )
    )
    parent = [
        mock,
        "--role",
        "implement",
        "--prompt-file",
        "{prompt_file}",
        "--handoff-file",
        "{handoff_file}",
        "--script",
        str(script),
    ]
    task = [
        mock,
        "--role",
        "research",
        "--model",
        "{model}",
        "--prompt-file",
        "{prompt_file}",
        "--handoff-file",
        "{handoff_file}",
        "--state-dir",
        "{run_dir}",
    ]
    (project / ".model-router.toml").write_text(
        f"""version = 2
[adapters.parent]
command = {json.dumps(sys.executable)}
args = {json.dumps(parent)}
session = {{ assign = "--session-id" }}
follow_up_args = {json.dumps(parent + ["--mcp-config", "{mcp_config}", "--resume", "{session}"])}
mcp_config_args = ["--mcp-config", "{{mcp_config}}"]

[adapters.task]
command = {json.dumps(sys.executable)}
args = {json.dumps(task)}

[delegates.helper]
harness = "task"
model = "slow"

[[workflows]]
name = "Delegation smoke"
[[workflows.stages]]
id = "implement"
name = "Implement"
role = "implementation"
harness = "parent"
[workflows.stages.delegation]
delegates = ["helper"]
"""
    )
    trust(project)


def main():
    require_binary()
    if not (shutil.which("bwrap") or shutil.which("sandbox-exec")):
        print("SKIP: no OS sandbox backend for tasks")
        return
    with tempfile.TemporaryDirectory(prefix="router delegation ") as name:
        project = Path(name).resolve()
        git(project, "init", "-q")
        git(project, "config", "user.email", "smoke@example.invalid")
        git(project, "config", "user.name", "Smoke")
        (project / "README.md").write_text("hello\n")
        (project / ".gitignore").write_text(
            ".model-router/\nconfig-home/\nscript.json*\n"
        )
        git(project, "add", ".")
        git(project, "commit", "-q", "-m", "base")
        config(project)
        terminal = Terminal(project, ["--no-animation"])
        try:
            terminal.wait_for_text("1 Workflow", message="TUI did not start")
            terminal.send("t")
            terminal.send("Delegate a look around")
            terminal.send(b"\x13")
            terminal.send("r")
            terminal.wait_for_text(
                "Runs [2] follows it", message="The run did not start", timeout=30
            )
            terminal.send("2")
            terminal.wait_for_text(
                "Implement tasks", message="The task line did not appear", timeout=30
            )
            terminal.wait_for_text(
                "↳ t1 · helper", message="The task's target is missing"
            )
            terminal.snapshot("delegation-tasks-120x36.txt")
            terminal.send("C")
            terminal.send("y")
            terminal.wait_for(
                lambda: (
                    records(project) and records(project)[-1]["status"] == "cancelled"
                ),
                "Cancelling did not stop the stage",
                timeout=30,
            )
            run = records(project)[-1]
            task = run["stages"][0]["attempts"][-1]["tasks"][0]
            assert task["status"] in ("cancelled", "interrupted"), task["status"]
            terminal.send("q")
            terminal.exit()
        finally:
            terminal.cleanup()
    print("PASS: task line on the Runs page, cancelling the stage cancels its task.")
    print(f"Artifacts: {ARTIFACTS}")


if __name__ == "__main__":
    main()
