#!/usr/bin/env python3
"""Exercise the real TUI and nested terminal with an offline mock harness.

Run: uv run --with pyte python scripts/smoke_tui.py
"""
import fcntl
import codecs
import json
import os
from pathlib import Path
import pty
import select
import signal
import struct
import sys
import tempfile
import termios
import time
import tomllib

import pyte

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/debug/model-router"
ARTIFACTS = ROOT / ".model-router/qa"
ARTIFACTS.mkdir(parents=True, exist_ok=True)


class Terminal:
    def __init__(self, project, extra=(), extra_env=None, size=(120, 36)):
        self.project = project
        self.pid, self.fd = pty.fork()
        if not self.pid:
            env = dict(os.environ, TERM="xterm-256color", XDG_CONFIG_HOME=str(project / "config-home"))
            env.update(extra_env or {})
            argv = [str(BINARY), "-C", str(project), "--config", str(project / ".model-router.toml"), *extra]
            os.execve(str(BINARY), argv, env)
        self.screen = pyte.Screen(*size)
        self.stream = pyte.Stream(self.screen)
        self.raw = bytearray()
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.resize(*size)
        self.exited = False

    def resize(self, cols, rows):
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.screen.resize(lines=rows, columns=cols)
        os.kill(self.pid, signal.SIGWINCH)

    def pump(self, duration=0.15):
        end = time.monotonic() + duration
        while time.monotonic() < end:
            readable, _, _ = select.select([self.fd], [], [], 0.025)
            if readable:
                try:
                    data = os.read(self.fd, 65536)
                except OSError:
                    return
                if not data:
                    return
                self.raw.extend(data)
                self.stream.feed(self.decoder.decode(data))
                if b"\x1b[6n" in data:
                    os.write(self.fd, b"\x1b[1;1R")

    def text(self):
        return "\n".join(self.screen.display)

    def wait(self, predicate, message, timeout=10):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            self.pump()
            if predicate():
                return
        raise AssertionError(message + "\n" + self.text())

    def send(self, data):
        os.write(self.fd, data.encode() if isinstance(data, str) else data)
        self.pump()

    def snapshot(self, name):
        (ARTIFACTS / name).write_text(self.text())

    def exit(self, expected=0):
        end = time.monotonic() + 10
        while time.monotonic() < end:
            self.pump()
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.exited = True
                assert os.waitstatus_to_exitcode(status) == expected, (status, self.text())
                attributes = termios.tcgetattr(self.fd)
                assert attributes[3] & termios.ICANON, "Terminal left in raw mode"
                (ARTIFACTS / "terminal-transcript.ansi").write_bytes(self.raw)
                os.close(self.fd)
                return
        raise AssertionError("Router did not exit")

    def cleanup(self):
        if not self.exited:
            try:
                os.kill(self.pid, signal.SIGKILL)
                os.waitpid(self.pid, 0)
            except ProcessLookupError:
                pass
            os.close(self.fd)


def records(project):
    return sorted((json.loads(p.read_text()) for p in project.glob(".model-router/runs/*/run.json")), key=lambda r: r["created_at"])


def main():
    assert BINARY.exists(), "Run cargo build first"
    with tempfile.TemporaryDirectory(prefix="router terminal test ") as name:
        project = Path(name)
        config = (ROOT / "examples/demo.toml").read_text().replace("examples/mock_harness.py", str(ROOT / "examples/mock_harness.py"))
        path = project / ".model-router.toml"
        path.write_text(config)
        terminal = Terminal(project)
        try:
            terminal.wait(lambda: "1 Workflow" in terminal.text(), "TUI did not render")
            assert not records(project), "Opening the router started a run"
            terminal.snapshot("workflow-120x36.txt")
            terminal.resize(80, 24)
            terminal.wait(lambda: "Review" in terminal.text(), "Optional review is hidden at 80x24")
            terminal.snapshot("workflow-80x24.txt")
            terminal.resize(120, 36)
            terminal.send("t")
            terminal.send("\x1b[200~Build a feature with 雪\nand preserve context between stages.\x1b[201~")
            terminal.send(b"\x13")
            terminal.send("jj ")  # enable review
            terminal.send(" ")  # disable it again
            terminal.send("kve")  # implementation: interactive, then edit
            terminal.send(b"\x01\x0b")  # beginning/end-of-line deletion in text editor
            terminal.send("Interactive implementation")
            terminal.send(b"\x13")
            terminal.send("s")
            saved = tomllib.loads(path.read_text())
            assert saved["workflows"][0]["stages"][1]["mode"] == "interactive"
            assert saved["workflows"][0]["stages"][1]["name"] == "Interactive implementation"
            assert not saved["workflows"][0]["stages"][2]["enabled"]
            terminal.send("r")
            terminal.wait(lambda: "MOCK INTERACTIVE READY" in terminal.text(), "Interactive stage did not receive a terminal")
            terminal.send("done 雪\n")
            terminal.wait(lambda: records(project) and records(project)[-1]["status"] == "awaiting_confirmation", "No interactive checkpoint")
            terminal.wait(lambda: "MODEL ROUTER" in terminal.text(), "TUI was not restored")
            terminal.snapshot("interactive-checkpoint.txt")
            terminal.send("h")
            terminal.wait(lambda: "implementation evidence" in terminal.text(), "Handoff inspector missing")
            terminal.snapshot("handoff.txt")
            terminal.send(b"\x1b")
            terminal.send("a")
            terminal.wait(lambda: records(project)[-1]["status"] == "complete", "Run did not complete")
            record = records(project)[-1]
            assert record["stages"][2]["status"] == "skipped"
            assert not (project / ".model-router/mock/review.count").exists()
            assert (project / ".model-router/mock/interactive-input.txt").read_text() == "done 雪"
            assert "research evidence" in (project / ".model-router/mock/implementation.prompt").read_text()
            terminal.snapshot("completed.txt")
            terminal.send("3\r")
            terminal.wait(lambda: "Saved run loaded" in terminal.text(), "History did not reload run")
            terminal.send("q")
            terminal.exit()
        finally:
            terminal.cleanup()

        # An echoed valid-looking review verdict in a PTY must not approve work.
        review = f'''version = 1
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
'''
        path.write_text(review)
        terminal = Terminal(project, ["run", "--task", "Review without a handoff file"])
        try:
            terminal.wait(lambda: "MOCK INTERACTIVE READY" in terminal.text(), "Review did not open")
            terminal.send("done\n")
            terminal.exit(expected=1)
            assert records(project)[-1]["status"] == "blocked"
        finally:
            terminal.cleanup()
    print("PASS: real TUI editing, Unicode paste, 80x24 resize, optional review, PTY input, handoff checkpoint, completion, history, terminal restoration, and rejection of unrecorded review verdicts.")
    print(f"Artifacts: {ARTIFACTS}")


if __name__ == "__main__":
    main()
