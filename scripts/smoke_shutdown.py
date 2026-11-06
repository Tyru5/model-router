#!/usr/bin/env python3
"""Verify real terminal loss and signal shutdown without starting a model.

Run: python3 scripts/smoke_shutdown.py [path/to/model-router]
"""

import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

ROOT = Path(__file__).resolve().parents[1]
BINARY = (
    Path(sys.argv[1]).resolve()
    if len(sys.argv) > 1
    else ROOT / "target/debug/model-router"
)


class Terminal:
    def __init__(self, project, *, animate=False, controlling=True):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 120, 0, 0))

        def setup():
            os.setsid()
            if controlling:
                fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

        env = dict(
            os.environ,
            TERM="xterm-256color",
            XDG_CONFIG_HOME=str(project / "config-home"),
            MODEL_ROUTER_NO_ANIMATION="0",
        )
        argv = [str(BINARY), "-C", str(project)]
        if not animate:
            argv.append("--no-animation")
        try:
            self.process = subprocess.Popen(
                argv,
                cwd=project,
                env=env,
                stdin=slave,
                stdout=slave,
                stderr=slave,
                preexec_fn=setup,
            )
        except BaseException:
            os.close(self.master)
            raise
        finally:
            os.close(slave)
        self.raw = bytearray()

    def pump(self, duration=0.05):
        end = time.monotonic() + duration
        while time.monotonic() < end:
            if select.select([self.master], [], [], 0.01)[0]:
                try:
                    data = os.read(self.master, 65536)
                except OSError:
                    return
                if not data:
                    return
                self.raw.extend(data)
                if b"\x1b[6n" in data:
                    os.write(self.master, b"\x1b[1;1R")

    def wait_for(self, predicate, message, timeout=5):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            self.pump()
            # Snapshot exit before the predicate, which may itself await exit.
            # A second poll afterwards races a successful termination.
            code = self.process.poll()
            if predicate():
                return
            assert code is None, (
                f"Router exited before {message}: {self.process.returncode}"
            )
        raise AssertionError(message)

    def send(self, data):
        os.write(self.master, data)
        self.pump(0.1)

    def hangup(self):
        # Let the application enter input polling after its initial render.
        self.pump(0.15)
        assert self.process.poll() is None, "Router exited before terminal loss"
        os.close(self.master)
        self.master = None
        try:
            code = self.process.wait(timeout=3)
        except subprocess.TimeoutExpired as error:
            raise AssertionError(
                "Router survived terminal loss (runaway input loop)"
            ) from error
        # I/O failure is valid when the PTY disappears during drawing; panic is not.
        assert code in (0, 1), f"Unexpected exit after terminal loss: {code}"

    def signal_exit(self, sig):
        os.kill(self.process.pid, sig)
        self.wait_for(
            lambda: self.process.poll() is not None,
            f"Router ignored signal {sig}",
            timeout=3,
        )
        assert self.process.returncode == 0, self.process.returncode
        assert termios.tcgetattr(self.master)[3] & termios.ICANON, (
            "Terminal left in raw mode"
        )

    def cleanup(self):
        if self.process.poll() is None:
            try:
                os.killpg(self.process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        self.process.wait(timeout=3)
        if self.master is not None:
            os.close(self.master)


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    if sys.platform.startswith("linux"):
        try:
            stat = Path(f"/proc/{pid}/stat").read_text()
        except FileNotFoundError:
            return False
        return stat[stat.rfind(")") + 2 :].split()[0] != "Z"
    return True


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


def trust(project):
    """Trust the project's custom adapter, as its user would once."""
    subprocess.run(
        [str(BINARY), "-C", str(project), "trust", "--yes"],
        env=dict(os.environ, XDG_CONFIG_HOME=str(project / "config-home")),
        check=True,
        capture_output=True,
    )


def check_hangup(
    name, *, animate=False, controlling=True, active=False, interactive=False
):
    with tempfile.TemporaryDirectory(prefix="router-shutdown-") as directory:
        project = Path(directory)
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
                terminal.send(b"r")
                pid_path = project / "mock-pids.json"
                terminal.wait_for(
                    lambda: pid_path.exists() and pid_path.stat().st_size > 0,
                    "Offline workflow did not start",
                )
                pids = json.loads(pid_path.read_text())
                assert all(alive(pid) for pid in pids), (
                    "Offline workflow was not running"
                )
            terminal.hangup()
            if active:
                end = time.monotonic() + 3
                while any(alive(pid) for pid in pids) and time.monotonic() < end:
                    time.sleep(0.02)
                assert not any(alive(pid) for pid in pids), (
                    "Terminal loss left harness processes alive"
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
            for pid in pids:
                if alive(pid):
                    try:
                        os.kill(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass


def main():
    assert BINARY.is_file(), "Run cargo build first"
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
            terminal = Terminal(Path(directory))
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
