#!/usr/bin/env python3
"""Verify file/URL task imports in the real TUI with offline fixtures.

Run: uv run --with-requirements scripts/requirements-smoke.txt python scripts/smoke_imports.py
"""

import json
import os
from pathlib import Path
import shutil
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from smoke_common import ARTIFACTS, ROOT, Terminal, demo_config, records


SLOW_SERVED = threading.Event()


class Documents(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        assert self.headers.get("Authorization") is None
        if self.path == "/slow.md":
            time.sleep(0.9)
        body = b"# Remote text spec\n\n- Keep the imported task editable\n"
        self.send_response(403 if self.path == "/denied.md" else 200)
        self.send_header("Content-Type", "text/markdown")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except BrokenPipeError:
            pass
        if self.path == "/slow.md":
            SLOW_SERVED.set()


def paste(terminal, text):
    terminal.send(f"\x1b[200~{text}\x1b[201~")


def escape(terminal, count):
    for _ in range(count):
        terminal.send(b"\x1b")


def verify_task(terminal, text):
    # The Workflow summary shows only three lines; inspect the full task editor.
    terminal.send("t")
    terminal.wait_for(
        lambda: text in terminal.text(), "Task editor lost imported edits"
    )
    escape(terminal, 1)


def url_import(terminal, url):
    terminal.send("f")
    terminal.send(b"\x15")  # Ctrl+U
    terminal.wait_for(
        lambda: "Import from URL" in terminal.text(), "URL entry did not open"
    )
    paste(terminal, url)
    terminal.send("\r")


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Documents)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{server.server_port}"
    try:
        with tempfile.TemporaryDirectory(prefix="router task import ") as name:
            project = Path(name).resolve()
            config = demo_config()
            (project / ".model-router.toml").write_text(config)
            docs = project / "docs"
            docs.mkdir()
            source = docs / "Feature 雪.md"
            raw = "\ufeff# Imported feature 雪\r\n\r\n- Preserve task context\r\n- Keep $(literal)\r\n".encode()
            expected = (
                "# Imported feature 雪\n\n- Preserve task context\n- Keep $(literal)\n"
            )
            source.write_bytes(raw)
            (docs / "binary.md").write_bytes(b"binary\x00text")
            (docs / "large.md").write_text("x" * 33000)
            bin_dir = project / "bin"
            bin_dir.mkdir()
            shutil.copyfile(ROOT / "examples/mock_github.py", bin_dir / "gh")
            (bin_dir / "gh").chmod(0o700)
            trace = project / "github-requests.jsonl"
            terminal = Terminal(
                project,
                ["--no-animation"],
                env={
                    "PATH": str(bin_dir) + os.pathsep + os.environ.get("PATH", ""),
                    "ROUTER_TEST_GH_TRACE": str(trace),
                },
                size=(80, 24),
            )
            try:
                terminal.wait_for(
                    lambda: "1 Workflow" in terminal.text(), "TUI did not render"
                )
                assert "[f] import" in terminal.text(), (
                    "Import control is not discoverable"
                )
                terminal.send("t")
                paste(terminal, "Existing instructions")
                terminal.send(b"\x13")
                terminal.send("fdocs\rFeature\r")
                terminal.wait_for(
                    lambda: "Preview task/spec" in terminal.text(),
                    "File preview did not open",
                )
                assert "Existing instructions" in terminal.text(), (
                    "Import did not default to appending"
                )
                terminal.snapshot("import-preview-80x24.txt")
                escape(terminal, 2)
                terminal.wait_for(
                    lambda: (
                        "Preview task/spec" not in terminal.text()
                        and "Existing instructions" in terminal.text()
                    ),
                    "Cancel lost the original task",
                )
                assert not records(project), "Preview started a workflow"
                terminal.send("fdocs\rFeature\r")
                terminal.wait_for(
                    lambda: "Preview task/spec" in terminal.text(),
                    "Second preview missing",
                )
                terminal.send(b"\x13")
                terminal.wait_for(
                    lambda: "Task imported" in terminal.text(), "Import did not apply"
                )
                assert source.read_bytes() == raw, (
                    "Importer modified the source document"
                )
                source.unlink()  # The workflow must use its imported snapshot.
                terminal.send("r")
                terminal.wait_for(
                    lambda: (
                        records(project)
                        and records(project)[-1]["status"] == "complete"
                    ),
                    "Imported task did not complete",
                )
                snapshot = "Existing instructions\n\n---\n\n" + expected
                assert records(project)[-1]["task"] == snapshot
                for stage in ("research", "implementation"):
                    assert (
                        expected
                        in (project / f".model-router/mock/{stage}.prompt").read_text()
                    )
                # Starting a run frees the editor for the next task.
                terminal.wait_for(
                    lambda: "Existing instructions" not in terminal.text(),
                    "Starting a run did not clear the task",
                )
                terminal.send("t")
                paste(terminal, "Existing instructions")
                terminal.send(b"\x13")

                # Invalid local documents keep the current task and browser available.
                for file, error in (
                    ("binary.md", "text document"),
                    ("large.md", "32 KiB"),
                ):
                    terminal.send("f")
                    terminal.send(b"\x0c")
                    paste(terminal, str(docs / file))
                    terminal.send("\r")
                    terminal.wait_for(
                        lambda: error in terminal.text(),
                        f"{file} did not report an import error",
                    )
                    escape(terminal, 2)
                    assert "Existing instructions" in terminal.text()

                url_import(terminal, base + "/spec.md")
                terminal.wait_for(
                    lambda: "Preview task/spec" in terminal.text(),
                    "Text URL was not imported",
                )
                terminal.send("\t")  # switch default Append to Replace
                assert "Existing instructions" not in terminal.text()
                paste(terminal, "Edited draft\n\n")
                terminal.send(b"\x13")
                terminal.wait_for(
                    lambda: (
                        "Preview task/spec" not in terminal.text()
                        and "Task imported" in terminal.text()
                    ),
                    "Editable URL draft did not apply",
                )
                verify_task(terminal, "Edited draft")

                url_import(terminal, base + "/slow.md")
                terminal.wait_for(
                    lambda: "Fetching task/spec" in terminal.text(),
                    "Slow import did not remain responsive",
                )
                escape(terminal, 3)
                # Let the cancelled response arrive and a few frames handle it.
                terminal.wait_for(SLOW_SERVED.is_set, "Slow response never sent")
                terminal.wait_frame(count=5)
                assert "Preview task/spec" not in terminal.text(), (
                    "Cancelled result reopened the preview"
                )
                verify_task(terminal, "Edited draft")
                url_import(terminal, base + "/denied.md")
                terminal.wait_for(
                    lambda: "returned an error" in terminal.text(),
                    "HTTP failure did not report an error",
                )
                escape(terminal, 2)
                verify_task(terminal, "Edited draft")

                terminal.resize(120, 36)
                url_import(terminal, "https://github.com/acme/project/issues/7")
                terminal.wait_for(
                    lambda: "Preview task/spec" in terminal.text(),
                    "GitHub issue was not imported",
                )
                terminal.send("\t")
                terminal.snapshot("import-github-120x36.txt")
                assert (
                    "GitHub issue spec" in terminal.text()
                    and "Acceptance criteria" in terminal.text()
                )
                terminal.send(b"\x13")
                terminal.wait_for(
                    lambda: "Task imported" in terminal.text(),
                    "GitHub issue did not apply",
                )
                url_import(
                    terminal,
                    "https://github.com/acme/project/blob/feature/new/docs/spec.md",
                )
                terminal.wait_for(
                    lambda: "Preview task/spec" in terminal.text(),
                    "GitHub file was not imported",
                )
                terminal.send("\t")
                terminal.send(b"\x13")
                terminal.wait_for(
                    lambda: "GitHub file spec" in terminal.text(),
                    "GitHub file did not replace the task",
                )
                requests = [json.loads(line) for line in trace.read_text().splitlines()]
                assert all(
                    request[:5]
                    == ["api", "--hostname", "github.com", "--method", "GET"]
                    for request in requests
                )
                assert any(
                    "ref=refs%2Fheads%2Ffeature%2Fnew" in request[5]
                    for request in requests
                )
                assert (
                    len(records(project)) == 1
                    and records(project)[0]["task"] == snapshot
                )
                terminal.send("q")
                terminal.exit()
            finally:
                terminal.cleanup()
    finally:
        server.shutdown()
        server.server_close()
    print(
        "PASS: local/URL/GitHub import, file navigation, append/replace preview, editing, cancellation, errors, source preservation, immutable run snapshots, 80x24/120x36 layouts, and terminal cleanup."
    )
    print(f"Artifacts: {ARTIFACTS}")


if __name__ == "__main__":
    main()
