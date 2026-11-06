"""Shared PTY harness for the terminal smoke tests.

Spawns model-router in a real pseudo-terminal, renders its output with pyte
(optional), and waits on screen content, redrawn frames, or files instead of
sleeping. Failed waits dump the screen and raw output into .model-router/qa/.

Frame waits use the router's test hook: with MODEL_ROUTER_FRAME_MARKER=1 it
writes a frame counter to MODEL_ROUTER_FRAME_FILE after every TUI draw.

pyte is imported lazily so raw-only scripts (smoke_shutdown.py) run on a plain
python3. Pinned versions: scripts/requirements-smoke.txt.
"""

import codecs
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

ROOT = Path(__file__).resolve().parents[1]
ARTIFACTS = ROOT / ".model-router/qa"
ARTIFACTS.mkdir(parents=True, exist_ok=True)
SCRIPT = Path(sys.argv[0]).stem or "smoke"


def _binary():
    if os.environ.get("MODEL_ROUTER_BIN"):
        return Path(os.environ["MODEL_ROUTER_BIN"]).resolve()
    if len(sys.argv) > 1 and Path(sys.argv[1]).is_file():
        return Path(sys.argv[1]).resolve()
    return ROOT / "target/debug/model-router"


BINARY = _binary()
DSR = b"\x1b[6n"  # cursor position query; crossterm sends it at startup
_failures = 0


def config_env(home):
    """Config dir under `home` on every OS: macOS ignores XDG_CONFIG_HOME."""
    home = Path(home)
    return {
        "XDG_CONFIG_HOME": str(home),
        "MODEL_ROUTER_CONFIG_DIR": str(home / "model-router"),
    }


def trust(project):
    """Trust the project's custom adapter, as its user would once."""
    import subprocess

    subprocess.run(
        [str(BINARY), "-C", str(project), "trust", "--yes"],
        env=dict(os.environ, **config_env(Path(project) / "config-home")),
        check=True,
        capture_output=True,
    )


def require_binary():
    assert BINARY.is_file(), f"Build model-router first (missing {BINARY})"


def demo_config():
    """The offline demo workflow, with the mock harness path made absolute."""
    return (
        (ROOT / "examples/demo.toml")
        .read_text()
        .replace("examples/mock_harness.py", str(ROOT / "examples/mock_harness.py"))
    )


def records(project):
    return sorted(
        (
            json.loads(p.read_text())
            for p in Path(project).glob(".model-router/runs/*/run.json")
        ),
        key=lambda r: r["created_at"],
    )


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


def kill_all(pids):
    for pid in pids:
        if alive(pid):
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass


def wait_until(predicate, message, timeout=5, interval=0.02):
    """Poll a condition that needs no terminal pumping."""
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if predicate():
            return
        time.sleep(interval)
    if not predicate():
        raise AssertionError(message)


def signature(path):
    """Identity of a file version. Atomic saves always change the inode."""
    try:
        stat = Path(path).stat()
    except FileNotFoundError:
        return None
    return (stat.st_ino, stat.st_mtime_ns, stat.st_size)


class Terminal:
    """model-router in a PTY.

    args: CLI args after `-C project` (and `--config` when config=True).
    screen: render with pyte (needed for text() and frame-accurate waits).
    controlling: make the PTY the child's controlling terminal.
    """

    def __init__(
        self,
        project,
        args=(),
        *,
        env=None,
        size=(120, 36),
        screen=True,
        controlling=True,
        config=True,
    ):
        self.project = Path(project)
        cols, rows = size
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self._frames_dir = tempfile.mkdtemp(prefix="mr-frames-")
        self.frame_file = Path(self._frames_dir) / "frames"
        full_env = dict(
            os.environ,
            TERM="xterm-256color",
            **config_env(self.project / "config-home"),
            MODEL_ROUTER_FRAME_MARKER="1",
            MODEL_ROUTER_FRAME_FILE=str(self.frame_file),
        )
        full_env.update(env or {})
        argv = [str(BINARY), "-C", str(self.project)]
        if config:
            argv += ["--config", str(self.project / ".model-router.toml")]
        argv += list(args)

        def setup():
            os.setsid()
            if controlling:
                fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

        try:
            self.process = subprocess.Popen(
                argv,
                cwd=self.project,
                env=full_env,
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
        self.pid = self.process.pid
        self.raw = bytearray()
        self._tail = b""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.screen = None
        if screen:
            import pyte

            self.screen = pyte.Screen(cols, rows)
            self.stream = pyte.Stream(self.screen)

    # Output -----------------------------------------------------------------

    def _feed(self, data):
        if self.screen is not None:
            self.stream.feed(self.decoder.decode(data))

    def _receive(self, data):
        """Record and render output; answer cursor queries, even split across reads."""
        self.raw.extend(data)
        scan = self._tail + data
        fed = 0
        start = 0
        while (found := scan.find(DSR, start)) != -1:
            end = found + len(DSR)
            start = end
            end -= len(self._tail)  # a whole query never fits in the tail
            self._feed(data[fed:end])
            fed = end
            row, col = (
                (self.screen.cursor.y + 1, self.screen.cursor.x + 1)
                if self.screen is not None
                else (1, 1)
            )
            self.write(f"\x1b[{row};{col}R".encode())
        self._feed(data[fed:])
        self._tail = scan[-(len(DSR) - 1) :]

    def pump(self, duration=0.05):
        """Read output for `duration` seconds. Returns False once the PTY closed."""
        end = time.monotonic() + duration
        while True:
            remaining = max(0.0, end - time.monotonic())
            if self.master is None:
                return False
            if select.select([self.master], [], [], min(remaining, 0.01))[0]:
                try:
                    data = os.read(self.master, 65536)
                except OSError:
                    return False
                if not data:
                    return False
                self._receive(data)
                continue
            if remaining <= 0:
                return True

    def drain(self, quiet=0.03, limit=2.0):
        """Read until no output arrives for `quiet` seconds."""
        end = time.monotonic() + limit
        mark = len(self.raw)
        while time.monotonic() < end:
            self.pump(quiet)
            if len(self.raw) == mark:
                return
            mark = len(self.raw)

    # Screen -----------------------------------------------------------------

    def lines(self, region=None):
        """Screen rows; region is (first, last) rows, slice semantics."""
        rows = self.screen.display
        if region is not None:
            rows = rows[slice(*region)]
        return [row.rstrip() for row in rows]

    def text(self, region=None):
        return "\n".join(self.lines(region))

    def find_line(self, pattern, region=None):
        """First screen line matching a regex, or None."""
        regex = re.compile(pattern)
        return next((line for line in self.lines(region) if regex.search(line)), None)

    def has(self, text, region=None):
        return text in self.text(region)

    def background(self, row=0, col=0):
        return self.screen.buffer[row][col].bg

    # Frames -----------------------------------------------------------------

    def frame(self):
        try:
            return int(self.frame_file.read_text())
        except (FileNotFoundError, ValueError):
            return 0

    def wait_frame(self, after=None, count=2, timeout=10, stall=1.0):
        """Wait until `count` frames were drawn after frame `after`, then drain.

        Two frames guarantee one started after input written at `after`. If no
        frame arrives for `stall` seconds the TUI is not drawing (startup
        animation, native terminal attached, CLI mode); return False then.
        """
        after = self.frame() if after is None else after
        start = time.monotonic()
        last, last_change = after, start
        while time.monotonic() - start < timeout:
            self.pump(0.01)
            current = self.frame()
            if current >= after + count:
                self.drain()
                return True
            if current != last:
                last, last_change = current, time.monotonic()
            elif time.monotonic() - last_change > stall:
                self.drain()
                return False
            if self.process.poll() is not None:
                self.drain()
                return False
        self.fail(f"No frame after {after} within {timeout}s")

    # Input ------------------------------------------------------------------

    def write(self, data):
        os.write(self.master, data.encode() if isinstance(data, str) else data)

    def send(self, data, settle="frame"):
        """Write input, then wait: "frame" for the redraw, "quiet" for output to stop."""
        before = self.frame()
        self.write(data)
        if settle == "frame":
            self.wait_frame(before)
        elif settle == "quiet":
            self.drain(0.1)

    def resize(self, cols, rows, settle="redraw"):
        """settle: "redraw" (clear + TUI frame), "clear" (clear only), "quiet"."""
        start = len(self.raw)
        before = self.frame()
        fcntl.ioctl(
            self.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0)
        )
        if self.screen is not None:
            self.screen.resize(lines=rows, columns=cols)
        os.kill(self.pid, signal.SIGWINCH)
        if settle in ("redraw", "clear"):
            # Keys sent before the full-screen redraw race the resize.
            self.wait_for(
                lambda: b"\x1b[2J" in self.raw[start:],
                "No redraw after resize",
                timeout=5,
            )
            if settle == "redraw":  # "clear" suits screens without frame marks
                self.wait_frame(before)
        elif settle == "quiet":
            self.drain(0.2)

    # Waiting ----------------------------------------------------------------

    def wait_for(self, predicate, message, timeout=10):
        end = time.monotonic() + timeout
        while True:
            self.pump(0.02)
            # Snapshot exit before the predicate, which may itself await exit.
            code = self.process.poll()
            if predicate():
                return
            if code is not None:
                self.fail(f"Router exited ({code}) before: {message}")
            if time.monotonic() >= end:
                self.fail(message)

    def wait_for_text(self, text, region=None, timeout=10, message=None):
        self.wait_for(
            lambda: text in self.text(region),
            message or f"Text not shown: {text!r}",
            timeout,
        )

    def wait_for_line(self, pattern, region=None, timeout=10, message=None):
        self.wait_for(
            lambda: self.find_line(pattern, region) is not None,
            message or f"No line matching {pattern!r}",
            timeout,
        )
        return self.find_line(pattern, region)

    def wait_for_gone(self, text, region=None, timeout=10, message=None):
        self.wait_for(
            lambda: text not in self.text(region),
            message or f"Text still shown: {text!r}",
            timeout,
        )

    def wait_for_file(self, path, check=None, message=None, timeout=10):
        """Wait until a file exists (and check(text) passes); return its text."""
        path = Path(path)
        result = {}

        def ready():
            try:
                text = path.read_text()
            except FileNotFoundError:
                return False
            if check is not None and not check(text):
                return False
            result["text"] = text
            return True

        self.wait_for(ready, message or f"File not ready: {path}", timeout)
        return result["text"]

    def wait_for_json(self, path, check, message=None, timeout=10):
        """Wait until a JSON file parses and check(value) passes; return the value."""

        def parsed(text):
            try:
                return check(json.loads(text))
            except ValueError:
                return False  # mid-write

        return json.loads(self.wait_for_file(path, parsed, message, timeout))

    def wait_for_change(self, path, before, message=None, timeout=10):
        """Wait until a file's signature differs from `before`."""
        self.wait_for(
            lambda: signature(path) not in (None, before),
            message or f"File was not written: {path}",
            timeout,
        )

    def save_config(self, path, key="s"):
        """Press the save key and return the TOML the router wrote."""
        before = signature(path)
        self.send(key)
        self.wait_for_change(path, before, f"Save did not write {path}")
        import tomllib  # Python 3.11+; only TOML-saving scripts need it

        return tomllib.loads(Path(path).read_text())

    # Lifecycle --------------------------------------------------------------

    def exit(self, expected=0, timeout=10):
        self.wait_for(
            lambda: self.process.poll() is not None, "Router did not exit", timeout
        )
        assert self.process.returncode == expected, (
            f"exit {self.process.returncode}, expected {expected}\n{self.dump('exit')}"
        )
        attributes = termios.tcgetattr(self.master)
        assert attributes[3] & termios.ICANON, "Terminal left in raw mode"
        assert attributes[3] & termios.ECHO, "Terminal echo was not restored"
        (ARTIFACTS / f"{SCRIPT}-transcript.ansi").write_bytes(self.raw)

    def hangup(self):
        """Close the terminal under the router; it must exit cleanly."""
        self.pump(0.15)  # let the application enter input polling
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
        os.kill(self.pid, sig)
        self.wait_for(
            lambda: self.process.poll() is not None, f"Router ignored signal {sig}", 3
        )
        assert self.process.returncode == 0, self.process.returncode
        assert termios.tcgetattr(self.master)[3] & termios.ICANON, (
            "Terminal left in raw mode"
        )

    def snapshot(self, name):
        (ARTIFACTS / name).write_text(self.text())

    def dump(self, reason):
        """Save screen and raw output for a failure; return the screen text."""
        global _failures
        _failures += 1
        slug = re.sub(r"[^A-Za-z0-9]+", "-", reason).strip("-")[:48]
        stem = ARTIFACTS / "failures" / f"{SCRIPT}-{_failures:02d}-{slug}"
        stem.parent.mkdir(parents=True, exist_ok=True)
        screen = self.text() if self.screen is not None else ""
        stem.with_suffix(".txt").write_text(f"{reason}\n\n{screen}\n")
        stem.with_suffix(".ansi").write_bytes(self.raw)
        return screen

    def fail(self, message):
        screen = self.dump(message)
        raise AssertionError(f"{message}\n{screen}")

    def cleanup(self):
        if self.process.poll() is None:
            try:
                os.killpg(self.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        self.process.wait(timeout=3)
        if self.master is not None:
            os.close(self.master)
            self.master = None
        shutil.rmtree(self._frames_dir, ignore_errors=True)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.cleanup()
