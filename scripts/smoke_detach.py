#!/usr/bin/env python3
"""Exercise live native-terminal detachment without providers or credentials.

Run: uv run --with-requirements scripts/requirements-smoke.txt python scripts/smoke_detach.py [path/to/model-router]
Build the binary first. All workflows, harnesses, and artifacts are temporary.
"""

import json
from pathlib import Path
import sys
import tempfile

import smoke_common
from smoke_common import alive, kill_all, require_binary, trust, wait_until

TASK = "Offline native terminal detach regression"


def Terminal(project, *, cli=False, controlling=True):
    args = ["--no-animation"] + (["run", "--task", TASK] if cli else [])
    return smoke_common.Terminal(
        project,
        args,
        env={"MODEL_ROUTER_NO_ANIMATION": "1"},
        controlling=controlling,
        config=False,
    )


HARNESS = r"""
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import termios
import time
import tty

assert sys.stdin.isatty() and sys.stdout.isatty()
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(300)"])
pids = [os.getpid(), child.pid]
Path("mock-pids.json").write_text(json.dumps(pids))
with Path("launches.txt").open("a") as launches:
    launches.write(str(os.getpid()) + "\n")
old = termios.tcgetattr(0)
inputs = []
buffer = bytearray()
ticks = 0

def emit(text):
    sys.stdout.write(text)
    sys.stdout.flush()

def snapshot():
    size = os.get_terminal_size()
    state = dict(pid=pids[0], child=pids[1], ticks=ticks,
                 cols=size.columns, rows=size.lines, inputs=inputs)
    Path("mock-state.tmp").write_text(json.dumps(state))
    Path("mock-state.tmp").replace("mock-state.json")

try:
    tty.setraw(0)
    # The sentinel is emitted exactly once. Only screen replay can restore it.
    emit("\x1b[?1049h\x1b[2J\x1b[H\x1b[1;35mDETACH SCREEN SENTINEL\x1b[0m"
         "\x1b[2;1HFAKE NATIVE READY\x1b[?2004h\x1b[5;1H> ")
    finished = False
    while not finished:
        ticks += 1
        snapshot()
        emit(f"\x1b[3;1HPROGRESS {ticks}\x1b[K\x1b[5;1H> ")
        if Path("finish-harness").exists():
            break
        if not select.select([0], [], [], 0.1)[0]:
            continue
        data = os.read(0, 4096)
        if not data:
            break
        buffer.extend(data)
        buffer[:] = buffer.replace(b"\x1b[200~", b"").replace(b"\x1b[201~", b"")
        while b"\r" in buffer or b"\n" in buffer:
            end = min(i for i, value in enumerate(buffer) if value in (10, 13))
            line = bytes(buffer[:end]).decode("utf-8")
            del buffer[:end + 1]
            inputs.append(line)
            emit("\x1b[6;1HACK " + line + "\x1b[K\x1b[5;1H> ")
            if line == "/exit":
                finished = True
                break
    snapshot()
    handoff = dict(summary="Offline detach evidence", verdict="complete",
                   decisions=["Same native process resumed"], files_changed=[],
                   tests=[], unresolved=[])
    Path(sys.argv[1]).write_text(json.dumps(handoff))
finally:
    child.terminate()
    child.wait(timeout=2)
    emit("\x1b[?2004l\x1b[?25h\x1b[?1049l")
    termios.tcsetattr(0, termios.TCSANOW, old)
"""


def workflow(project):
    harness = project / "offline_harness.py"
    harness.write_text(HARNESS)
    args = json.dumps([str(harness), "{handoff_file}"])
    (project / ".model-router.toml").write_text(f"""version = 1
[adapters.mock]
command = {json.dumps(sys.executable)}
args = {args}
interactive_args = {args}
[[workflows]]
name = "Offline detach test"
[[workflows.stages]]
id = "research"
name = "Research"
role = "research"
harness = "mock"
mode = "interactive"
""")
    trust(project)


def state(project):
    path = project / "mock-state.json"
    return json.loads(path.read_text()) if path.exists() else {}


def record(project):
    paths = list(project.glob(".model-router/runs/*/run.json"))
    assert len(paths) <= 1, "Detach created a second run"
    return json.loads(paths[0].read_text()) if paths else {}


def start(terminal, project, *, cli):
    if not cli:
        terminal.wait_for_text("Workflow", message="TUI did not render")
        terminal.send(b"t")
        terminal.send(TASK.encode() + b"\x13")
        terminal.send(b"r", settle=None)
    terminal.wait_for(
        lambda: "FAKE NATIVE READY" in terminal.text() and bool(state(project)),
        "Native terminal did not open",
    )
    assert all(alive(pid) for pid in pids(project)), "Fixture did not start"


def pids(project):
    path = project / "mock-pids.json"
    try:
        return json.loads(path.read_text())
    except (FileNotFoundError, ValueError):  # not written yet, or mid-write
        return []


def detach(terminal, project, *, cli):
    terminal.send(b"\x02", settle=None)
    terminal.wait_for(
        lambda: (
            "Harness still running" in terminal.text()
            if cli
            else "MODEL ROUTER" in terminal.text()
        ),
        "Ctrl+B did not return to the waiting view",
    )
    before = state(project)
    terminal.wait_for(
        lambda: state(project)["ticks"] >= before["ticks"] + 5,
        "Detached harness stopped making progress",
    )
    after = state(project)
    assert after["pid"] == before["pid"] and alive(after["pid"])
    assert record(project)["status"] == "running", "Detach completed the stage"
    terminal.wait_for(
        lambda: any(
            f"PROGRESS {after['ticks']}".encode() in path.read_bytes()
            for path in project.glob(".model-router/runs/**/stdout.log")
        ),
        "Detached PTY output was not drained to its log",
    )
    assert len((project / "launches.txt").read_text().splitlines()) == 1


def assert_reaped(project):
    wait_until(
        lambda: not any(alive(pid) for pid in pids(project)),
        "Harness descendants survived",
        timeout=3,
    )


def cleanup(terminal, project):
    terminal.cleanup()
    # Avoid leaving fixture descendants behind when exercising a broken binary.
    kill_all(pids(project))


def checkpoint(terminal, project):
    terminal.wait_for(
        lambda: record(project).get("status") == "awaiting_confirmation",
        "Harness exit did not create the confirmation checkpoint",
    )
    run = record(project)
    assert len(run["stages"][0]["attempts"]) == 1, "Reattachment created an attempt"
    assert len((project / "launches.txt").read_text().splitlines()) == 1
    assert_reaped(project)


def round_trip(*, cli=False):
    label = "CLI" if cli else "TUI"
    with tempfile.TemporaryDirectory(prefix="router-detach-") as directory:
        project = Path(directory).resolve()
        workflow(project)
        terminal = Terminal(project, cli=cli)
        try:
            start(terminal, project, cli=cli)
            original_pid = state(project)["pid"]
            detach(terminal, project, cli=cli)
            terminal.resize(80, 24, settle="quiet")
            replay_start = len(terminal.raw)
            terminal.send(b"t", settle=None)
            terminal.wait_for(
                lambda: (
                    "DETACH SCREEN SENTINEL" in terminal.text()
                    and b"DETACH SCREEN SENTINEL" in terminal.raw[replay_start:]
                ),
                "Reattach did not replay the unchanged native screen",
            )
            terminal.wait_for(
                lambda: (state(project)["cols"], state(project)["rows"]) == (80, 24),
                "Detached resize was not applied to the native PTY",
            )
            assert state(project)["pid"] == original_pid
            terminal.send("\x1b[200~reattached 雪\x1b[201~\r", settle=None)
            terminal.wait_for(
                lambda: state(project)["inputs"] == ["reattached 雪"],
                "Reattached Unicode paste did not reach the same harness",
            )
            terminal.wait_for(
                lambda: "ACK reattached 雪" in terminal.text(),
                "Reattached output was not visible",
            )
            terminal.resize(96, 28, settle="quiet")
            terminal.wait_for(
                lambda: (state(project)["cols"], state(project)["rows"]) == (96, 28),
                "Attached resize was not applied to the native PTY",
            )
            assert "DETACH SCREEN SENTINEL" in terminal.text()
            terminal.send(b"/exit\r", settle=None)
            checkpoint(terminal, project)
            terminal.wait_for(
                lambda: (
                    "Save and stop" in terminal.text()
                    if cli
                    else "MODEL ROUTER" in terminal.text()
                ),
                "Router was not restored after harness exit",
            )
            terminal.send(b"q\r" if cli else b"q")
            terminal.exit()
            print(
                f"PASS: {label} detach, progress, same PID, screen replay, resize, paste, /exit",
                flush=True,
            )
        finally:
            cleanup(terminal, project)


def detached_end(action, *, cli=False, controlling=True):
    label = "CLI" if cli else "TUI"
    with tempfile.TemporaryDirectory(prefix="router-detach-end-") as directory:
        project = Path(directory).resolve()
        workflow(project)
        terminal = Terminal(project, cli=cli, controlling=controlling)
        try:
            start(terminal, project, cli=cli)
            detach(terminal, project, cli=cli)
            if action == "complete":
                # No foreground key: completion must wake the waiting UI itself.
                (project / "finish-harness").touch()
                checkpoint(terminal, project)
                terminal.wait_for(
                    lambda: (
                        "Save and stop" in terminal.text()
                        if cli
                        else "MODEL ROUTER" in terminal.text()
                    ),
                    "Detached completion did not update the foreground",
                )
                terminal.send(b"q\r" if cli else b"q")
                terminal.exit()
            elif action == "hangup":
                terminal.hangup()
            else:
                # The TUI cancels with [C] and a confirmation; the CLI with c.
                terminal.send(
                    "Cy" if not cli and action == "c" else action, settle=None
                )
                terminal.wait_for(
                    lambda: record(project).get("status") == "cancelled",
                    "Detached cancellation did not persist",
                )
                if not cli and action == "c":
                    terminal.send(b"q")
                terminal.exit(expected=1 if cli else 0)
            if action != "complete":
                assert record(project)["status"] == "cancelled"
                assert_reaped(project)
            print(
                f"PASS: {label} detached {action} (controlling={controlling})",
                flush=True,
            )
        finally:
            cleanup(terminal, project)


def main():
    require_binary()
    for cli in (False, True):
        round_trip(cli=cli)
        for action in ("complete", "c", "q"):
            detached_end(action, cli=cli)
        for controlling in (True, False):
            detached_end("hangup", cli=cli, controlling=controlling)


if __name__ == "__main__":
    main()
