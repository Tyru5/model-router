#!/usr/bin/env python3
"""Review a finished run's changes in a real terminal: the diff dialog's file
list, hunk and file jumps, search, and the $PAGER / $VISUAL hand-offs.

The workflow uses the offline mock harness; every real harness name on PATH
is a stub that exits 97, so no model is ever called.

Run: uv run --with-requirements scripts/requirements-smoke.txt python scripts/smoke_review.py
"""

import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile

from smoke_common import ARTIFACTS, ROOT, Terminal, records, require_binary, trust

HARNESSES = [
    "codex",
    "claude",
    "gemini",
    "pi",
    "omp",
    "opencode",
    "aider",
    "goose",
    "amp",
    "copilot",
    "cursor-agent",
    "agent",
]

RECORDER = """import json, sys
from pathlib import Path
out = Path(sys.argv[1])
args = sys.argv[2:]
record = {"argv": args}
if args and Path(args[-1]).is_file():
    record["text"] = Path(args[-1]).read_text()
out.write_text(json.dumps(record))
"""


def stub_harnesses(bin_dir):
    bin_dir.mkdir()
    for name in HARNESSES:
        path = bin_dir / name
        path.write_text("#!/bin/sh\nexit 97\n")
        path.chmod(0o755)


def git(project, *args):
    subprocess.run(["git", *args], cwd=project, check=True, capture_output=True)


def workflow(project):
    mock = str(ROOT / "examples/mock_harness.py")

    def adapter(name, target=None):
        args = [
            mock,
            "--role",
            name,
            "--prompt-file",
            "{prompt_file}",
            "--handoff-file",
            "{handoff_file}",
        ]
        if target:
            args += ["--write-file", target]
        return f"[adapters.{name}]\ncommand = {json.dumps(sys.executable)}\nargs = {json.dumps(args)}\n"

    (project / ".model-router.toml").write_text(
        "version = 1\n"
        + adapter("first", "notes/added.txt")
        + adapter("second", "src/app.txt")
        + adapter("summary")
        + """[[workflows]]
name = "Review smoke"
[[workflows.stages]]
id = "first"
name = "Add notes"
role = "implementation"
harness = "first"
[[workflows.stages]]
id = "second"
name = "Edit app"
role = "implementation"
harness = "second"
[[workflows.stages]]
id = "summary"
name = "Review summary"
role = "summary"
harness = "summary"
"""
    )
    trust(project)


def main():
    require_binary()
    with tempfile.TemporaryDirectory(prefix="router review ") as name:
        project = Path(name).resolve()
        stub_harnesses(project / "stub-bin")
        git(project, "init", "-q")
        git(project, "config", "user.email", "smoke@example.invalid")
        git(project, "config", "user.name", "Smoke")
        (project / "src").mkdir()
        (project / "src/app.txt").write_text("first line\nsecond line\n")
        (project / ".gitignore").write_text(
            ".model-router/\nconfig-home/\nstub-bin/\n*.json\n*.py\n"
        )
        git(project, "add", ".")
        git(project, "commit", "-q", "-m", "base")
        workflow(project)
        recorder = project / "recorder.py"
        recorder.write_text(RECORDER)
        pager_out = project / "pager.json"
        editor_out = project / "editor.json"
        env = {
            "PATH": f"{project / 'stub-bin'}{os.pathsep}{os.environ['PATH']}",
            # Quoted: the project path has a space, and $PAGER is split like a shell would.
            "PAGER": shlex.join([sys.executable, str(recorder), str(pager_out)]),
            "VISUAL": shlex.join([sys.executable, str(recorder), str(editor_out)]),
        }
        terminal = Terminal(project, ["--no-animation"], env=env)
        try:
            terminal.wait_for_text("1 Workflow", message="TUI did not start")
            terminal.send("t")
            terminal.send("Change two files")
            terminal.send(b"\x13")
            terminal.send("r")
            terminal.wait_for(
                lambda: (
                    records(project) and records(project)[-1]["status"] == "complete"
                ),
                "Run did not complete",
                timeout=30,
            )
            terminal.send("2")
            terminal.wait_for_text("2 files", message="Changes were not computed")
            terminal.send("d")
            terminal.wait_for_text(
                "Changes during this run · 2 files · +2 −0",
                message="Diff dialog did not open with its stat",
            )
            assert terminal.find_line(r"A notes/added\.txt \+1 −0"), terminal.text()
            assert terminal.find_line(r"M src/app\.txt \+1 −0"), terminal.text()
            assert terminal.has("+written by first attempt 1")
            terminal.snapshot("review-diff-120x36.txt")
            terminal.send("]")
            terminal.wait_for_text(
                "+written by second attempt 1", message="] did not show the next file"
            )
            terminal.send("[")
            terminal.wait_for_text("+written by first attempt 1")
            terminal.send("/written")
            terminal.wait_for_text("/written▏", message="Search prompt missing")
            terminal.send("\r")
            terminal.wait_for_text(
                "/written · 1/2", message="Search did not count matches"
            )
            terminal.send("n")
            terminal.wait_for_text("/written · 2/2", message="n did not step matches")
            terminal.wait_for_text("+written by second attempt 1")
            terminal.snapshot("review-search-120x36.txt")

            # The pager gets the coloured patch; the dialog comes back after.
            terminal.send("o", settle="quiet")
            paged = json.loads(
                terminal.wait_for_file(pager_out, message="Pager did not run")
            )
            assert (
                "\x1b[32m+written by first attempt 1 line 1\x1b[0m" in paged["text"]
            ), paged
            terminal.wait_for_text(
                "Returned from", message="TUI did not resume after the pager"
            )
            assert terminal.has("Changes during this run"), "Diff dialog closed"

            # The editor opens the shown file at its first changed line.
            terminal.send("e", settle="quiet")
            edited = json.loads(
                terminal.wait_for_file(editor_out, message="Editor did not run")
            )
            opened = Path(edited["argv"][-1]).resolve()
            assert opened == (project / "src/app.txt").resolve(), edited
            terminal.wait_for_text("Returned from")
            terminal.send(b"\x1b")
            terminal.send(b"\x1b")
            terminal.wait_for_gone(
                "Changes during this run", message="Esc did not close"
            )
            terminal.send("q")
            terminal.exit()
        finally:
            terminal.cleanup()
    print(
        "PASS: diff dialog stat and file list, file/hunk jumps, search with n/N, pager and editor hand-offs with terminal restore."
    )
    print(f"Artifacts: {ARTIFACTS}")


if __name__ == "__main__":
    main()
