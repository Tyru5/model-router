#!/usr/bin/env python3
"""Verify real terminal loss and signal shutdown without starting a model.

Run: python3 scripts/smoke_shutdown.py [path/to/model-router]
"""

import json
from pathlib import Path
import signal
import sys
import tempfile

import smoke_common
from smoke_common import alive, kill_all, require_binary, trust, wait_until


def Terminal(project, *, animate=False, controlling=True):
    """Raw (no pyte) terminal: this script runs on a plain python3."""
    return smoke_common.Terminal(
        project,
        [] if animate else ["--no-animation"],
        env={"MODEL_ROUTER_NO_ANIMATION": "0"},
        screen=False,
        controlling=controlling,
        config=False,
    )


def active_workflow(project, *, interactive=False):
    # Keep an offline child and grandchild alive until the router cancels them.
    script = "\n".join(
        [
            "import json, os, subprocess, sys, time",
            "from pathlib import Path",
            "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(300)'])",
            "Path('mock-pids.json').write_text(json.dumps([os.getpid(), child.pid]))",
            "time.sleep(300)",
        ]
    )
    (project / ".model-router.toml").write_text(f"""version = 1
[adapters.mock]
command = {json.dumps(sys.executable)}
args = ["-c", {json.dumps(script)}]
interactive_args = ["-c", {json.dumps(script)}]
[[workflows]]
name = "Offline shutdown test"
[[workflows.stages]]
id = "research"
name = "Research"
role = "research"
harness = "mock"
mode = "{"interactive" if interactive else "unattended"}"
""")
    trust(project)


def check_hangup(
    name, *, animate=False, controlling=True, active=False, interactive=False
):
    with tempfile.TemporaryDirectory(prefix="router-shutdown-") as directory:
        project = Path(directory).resolve()
        if active:
            active_workflow(project, interactive=interactive)
        terminal = Terminal(project, animate=animate, controlling=controlling)
        pids = []
        try:
            marker = b"Any key to skip" if animate else b"Workflow"
            terminal.wait_for(
                lambda: marker in terminal.raw, "Initial screen did not render"
            )
            if active:
                terminal.send(b"t")
                terminal.send(b"Offline terminal shutdown regression\x13")
                terminal.send(b"r", settle=None)
                pids = json.loads(
                    terminal.wait_for_file(
                        project / "mock-pids.json",
                        check=lambda text: text.rstrip().endswith("]"),
                        message="Offline workflow did not start",
                    )
                )
                assert all(alive(pid) for pid in pids), (
                    "Offline workflow was not running"
                )
            terminal.hangup()
            if active:
                wait_until(
                    lambda: not any(alive(pid) for pid in pids),
                    "Terminal loss left harness processes alive",
                    timeout=3,
                )
                runs = list(project.glob(".model-router/runs/*/run.json"))
                assert len(runs) == 1, "Missing run state after terminal loss"
                run = json.loads(runs[0].read_text())
                assert run["status"] == "cancelled", run["status"]
            print(f"PASS: {name}", flush=True)
        finally:
            terminal.cleanup()
            # Also clean up offline descendants when testing the broken version.
            pid_path = project / "mock-pids.json"
            if pid_path.exists():
                pids = json.loads(pid_path.read_text())
            kill_all(pids)


def main():
    require_binary()
    check_hangup("idle terminal disconnect")
    check_hangup("startup terminal disconnect", animate=True)
    check_hangup("disconnect without a controlling terminal", controlling=False)
    check_hangup("active workflow disconnect cancels and reaps children", active=True)
    check_hangup(
        "interactive workflow disconnect cancels and reaps children",
        active=True,
        interactive=True,
    )
    check_hangup(
        "interactive disconnect without a controlling terminal",
        active=True,
        interactive=True,
        controlling=False,
    )
    for sig in (signal.SIGTERM, signal.SIGHUP):
        with tempfile.TemporaryDirectory(prefix="router-shutdown-signal-") as directory:
            terminal = Terminal(Path(directory).resolve())
            try:
                terminal.wait_for(
                    lambda: b"Workflow" in terminal.raw, "Initial screen did not render"
                )
                terminal.signal_exit(sig)
                print(
                    f"PASS: {signal.Signals(sig).name} with an open terminal",
                    flush=True,
                )
            finally:
                terminal.cleanup()


if __name__ == "__main__":
    main()
