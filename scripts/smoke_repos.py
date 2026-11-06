#!/usr/bin/env python3
"""Exercise repository selection in a real terminal, without model calls.

Run: uv run --with-requirements scripts/requirements-smoke.txt python scripts/smoke_repos.py
"""

import json
from pathlib import Path
import subprocess
import tempfile
import tomllib

from smoke_common import ARTIFACTS, ROOT, Terminal, records


def main():
    with tempfile.TemporaryDirectory(prefix="router repo picker ") as name:
        project = Path(name)
        group = project / "workspaces"
        alpha = group / "alpha"
        beta = group / "beta 雪"
        broken = group / "broken"
        plain = group / "plain folder"
        plain.mkdir(parents=True)
        for repo in (alpha, beta, broken):
            subprocess.run(["git", "init", "--quiet", str(repo)], check=True)

        config = (ROOT / "examples/demo.toml").read_text()
        path = project / ".model-router.toml"
        path.write_text(config)
        (alpha / ".model-router.toml").write_text(
            config.replace('name = "Offline demo"', 'name = "Alpha workflow"')
        )
        (beta / ".model-router.toml").write_text(
            config.replace('name = "Offline demo"', 'name = "Beta workflow"')
        )
        (broken / ".model-router.toml").write_text("invalid TOML [")

        terminal = Terminal(project)
        try:
            terminal.wait_for(
                lambda: "1 Workflow" in terminal.text(), "TUI did not render"
            )
            terminal.send("e")
            terminal.send(b"\x01\x0b")
            terminal.send("Saved before repo switch")
            terminal.send(b"\x13")
            assert (
                tomllib.loads(path.read_text())["workflows"][0]["stages"][0]["name"]
                != "Saved before repo switch"
            )
            terminal.send("p")
            terminal.wait_for(
                lambda: "Choose repository" in terminal.text(),
                "Repository browser did not open",
            )
            terminal.send(b"\x1b[C")  # browse the highlighted current project
            terminal.send("workspaces\r")  # enter the grouping folder
            terminal.wait_for(
                lambda: "[repo]" in terminal.text() and "alpha" in terminal.text(),
                "Nearby repos missing",
            )
            terminal.snapshot("repos-120x36.txt")
            terminal.resize(80, 24)
            terminal.wait_for(
                lambda: (
                    "Ctrl+L" in terminal.text()
                    and "[Tab] recent/folders" in terminal.text()
                ),
                "Repository controls hidden at 80x24",
            )
            terminal.snapshot("repos-80x24.txt")
            terminal.send("alpha\r")
            terminal.wait_for(
                lambda: (
                    "Choose repository" not in terminal.text()
                    and "Alpha workflow" in terminal.text()
                ),
                "Repo-specific configuration was not loaded",
            )
            terminal.wait_for_file(
                path,
                lambda text: 'name = "Saved before repo switch"' in text,
                "Switching repositories did not autosave the workflow",
            )

            # No matches must not pick an unrelated repo. Bad config stays recoverable.
            terminal.send("pno-such-repo\r")
            terminal.wait_for(
                lambda: "No matching folders" in terminal.text(),
                "Empty search did not stay open",
            )
            terminal.send(b"\x01\x0b")
            terminal.send("broken\r")
            terminal.wait_for(
                lambda: (
                    "Parse" in terminal.text()
                    and "Choose repository" in terminal.text()
                ),
                "Invalid config did not report an in-dialog error",
            )
            terminal.snapshot("repos-invalid-config.txt")
            terminal.send(b"\x1b")
            assert "Alpha workflow" in terminal.text(), "Failed switch changed project"

            terminal.send("pbeta")
            terminal.send(
                b"\x1b[C"
            )  # right browses inside a repo instead of selecting it
            terminal.wait_for(
                lambda: (
                    "Use this folder" in terminal.text() and "beta" in terminal.text()
                ),
                "Cannot browse inside a repo",
            )
            terminal.send(b"\x1b[D")  # left returns to parent and highlights beta
            terminal.send("\r")
            terminal.wait_for(
                lambda: "Beta workflow" in terminal.text(),
                "Parent navigation lost the selected repo",
            )
            terminal.send("p\talpha\r")
            terminal.wait_for(
                lambda: (
                    "Alpha workflow" in terminal.text()
                    and "Choose repository" not in terminal.text()
                ),
                "Recent repo selection failed",
            )

            # Pasting a path is literal; invalid paths leave the directory intact.
            terminal.send("p")
            terminal.send(b"\x0c")
            terminal.send("\x1b[200~./does-not-exist\x1b[201~\r")
            terminal.wait_for(
                lambda: "Project not found" in terminal.text(),
                "Bad path did not report a recoverable error",
            )
            terminal.send(b"\x1b")  # dismiss path editor only
            assert "Choose repository" in terminal.text()
            terminal.send(b"\x0c")
            terminal.send(f"\x1b[200~{plain}\x1b[201~\r")
            terminal.wait_for(
                lambda: "Use this folder" in terminal.text(),
                "Direct path did not open a directory",
            )
            terminal.send("\r")
            terminal.wait_for(
                lambda: (
                    "Choose repository" not in terminal.text()
                    and "plain folder" in terminal.text()
                ),
                "Non-Git directory selection failed",
            )
            terminal.send("q")
            terminal.exit()
        finally:
            terminal.cleanup()

        recent_path = project / "config-home/model-router/recent-projects.json"
        recent = json.loads(recent_path.read_text())
        assert recent[:3] == [str(plain), str(alpha), str(beta)], recent
        assert all(
            not records(repo) for repo in (project, alpha, beta, broken, plain)
        ), "Picking a repository launched a workflow"

        terminal = Terminal(project)
        try:
            terminal.wait_for(
                lambda: "1 Workflow" in terminal.text(), "TUI did not restart"
            )
            terminal.send("p\tbeta\r")
            terminal.wait_for(
                lambda: (
                    "Beta workflow" in terminal.text()
                    and "Choose repository" not in terminal.text()
                ),
                "Recent repositories did not survive restart",
            )
            terminal.send("q")
            terminal.exit()
        finally:
            terminal.cleanup()
    print(
        "PASS: repository browsing, filtering, parent navigation, Unicode paths, recent persistence, 80x24 layout, workflow autosave, per-repo config, invalid config/path recovery, non-Git projects, and no model launches."
    )
    print(f"Artifacts: {ARTIFACTS}")


if __name__ == "__main__":
    main()
