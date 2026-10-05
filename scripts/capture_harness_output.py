#!/usr/bin/env python3
"""Record one real harness response as a sanitized parser fixture.

WARNING: this sends ONE real model request through the chosen harness, using
its existing login; the provider may bill it. It refuses to run without
--confirm-paid-request.

Run: cargo build && python3 scripts/capture_harness_output.py claude --confirm-paid-request

It creates a temporary project and config with one unattended stage for the
harness, with restrictive harness settings where supported (Codex read-only
sandbox, Claude plan mode, Cursor sandbox, Gemini plan approval), and runs
`model-router run --task` with a tiny task asking for a fixed router-handoff
block. The attempt's stdout.log is sanitized (prompt echoes, paths, user and
host names, emails, IDs, keys) into
tests/fixtures/harness-output/<harness>/<scenario>.<ext>, and a `captured`
manifest entry is appended. Review both, then run
`cargo test --test harness_output`.
"""

import argparse
import datetime
import getpass
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests/fixtures/harness-output"
# Output kind (adapters::OutputKind), file extension, restrictive settings.
HARNESSES = {
    "codex": ("Codex", "jsonl", {"sandbox": "read-only"}),
    "claude": ("Claude", "json", {"permission_mode": "plan"}),
    "pi": ("Pi", "jsonl", {}),
    "cursor": ("Cursor", "json", {"sandbox": "enabled"}),
    "gemini": ("Gemini", "json", {"approval_mode": "plan"}),
    "opencode": ("OpenCode", "jsonl", {}),
    "aider": ("Text", "txt", {}),
    "goose": ("Text", "txt", {}),
    "amp": ("Amp", "jsonl", {}),
    "copilot": ("Text", "txt", {}),
}
PROMPT_PLACEHOLDER = "<router prompt omitted>"
UUID = re.compile(
    r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b"
)
PREFIXED_ID = re.compile(
    r"\b(ses|msg|prt|toolu|call|req|resp|chatcmpl|item|run)_[A-Za-z0-9]{12,}\b"
)
ID_FIELD = re.compile(
    r'"(session_?[iI][dD]|thread_?[iI][dD]|request_?[iI][dD]|uuid|model_call_id'
    r'|responseId|id)"(\s*:\s*)"([^"\\]{6,})"'
)
EMAIL = re.compile(
    r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}\b"
)
SECRET = re.compile(
    r"\bsk-[A-Za-z0-9_-]{16,}"
    r"|\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}"
    r"|\bgithub_pat_[A-Za-z0-9_]{20,}"
    r"|\bxox[abprs]-[A-Za-z0-9-]{10,}"
    r"|\bAKIA[0-9A-Z]{16}\b"
    r"|\bAIza[0-9A-Za-z_-]{35}"
    r"|\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}"
    r"|(?i:\bbearer\s+)[A-Za-z0-9._~+/=-]{16,}"
)


def escaped(text):
    """Forms a harness may print: raw, and JSON-escaped with or without ASCII escapes."""
    return {
        text,
        json.dumps(text, ensure_ascii=False)[1:-1],
        json.dumps(text)[1:-1],
    }


def sanitize(text, prompts=(), paths=(), home="", user="", host=""):
    """Return (text, notes). Each distinct ID maps to one stable placeholder."""
    counts = {}
    seen = {}

    def note(label, n=1):
        counts[label] = counts.get(label, 0) + n

    def stable(value, make):
        if value not in seen:
            seen[value] = make(len(seen) + 1)
        return seen[value]

    # Prompt echoes first: they contain paths that are replaced below.
    forms = set()
    for prompt in prompts:
        for variant in (prompt, prompt.strip()):
            if variant:
                forms |= escaped(variant)
    for form in sorted(forms, key=len, reverse=True):
        if form in text:
            note("prompt echoes", text.count(form))
            text = text.replace(form, PROMPT_PLACEHOLDER)
    replacements = {(path, "/workspace/project") for path in paths if path}
    if home and home != "/":
        replacements.add((home, "/home/user"))
    for value, placeholder in sorted(
        replacements, key=lambda item: len(item[0]), reverse=True
    ):
        for form in escaped(value):
            if form in text:
                note("paths", text.count(form))
                text = text.replace(form, placeholder)

    def uuid(match):
        note("IDs")
        return stable(
            match.group(0).lower(), lambda n: f"00000000-0000-4000-8000-{n:012}"
        )

    def prefixed(match):
        note("IDs")
        return stable(match.group(0), lambda n: f"{match.group(1)}_fixture{n:012}")

    def field(match):
        key, separator, value = match.groups()
        if value.startswith(("00000000-", "<")) or "_fixture" in value:
            return match.group(0)
        note("IDs")
        placeholder = stable(value, lambda n: f"id-{n:04}")
        return f'"{key}"{separator}"{placeholder}"'

    def replace(label, placeholder):
        def substitute(match):
            note(label)
            return placeholder

        return substitute

    text = UUID.sub(uuid, text)
    text = PREFIXED_ID.sub(prefixed, text)
    text = ID_FIELD.sub(field, text)
    text = SECRET.sub(replace("secrets", "<redacted>"), text)
    text = EMAIL.sub(replace("emails", "user@example.com"), text)
    for name, placeholder, label in (
        (user, "user", "user names"),
        (host, "host", "host names"),
    ):
        if name and len(name) >= 3:
            text, n = re.subn(rf"\b{re.escape(name)}\b", placeholder, text)
            if n:
                note(label, n)
    replaced = ", ".join(f"{label} {n}" for label, n in counts.items())
    return text, f"Replaced {replaced or 'nothing'}"


def version_of(program):
    """Version token as `model-router doctor` reads it: `opencode v2.0.22` -> 2.0.22."""
    try:
        result = subprocess.run(
            [program, "--version"],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        sys.exit(f"{program} --version failed: {error}")
    lines = (result.stdout or result.stderr).splitlines()
    line = next((line.strip() for line in lines if line.strip()), "")
    for token in re.split(r"[\s(),;]+", line):
        token = token.removeprefix("v")
        if token[:1].isdigit():
            return token
    sys.exit(f"No version in {program} --version output: {line!r}")


def toml_value(value):
    # JSON strings are valid TOML basic strings.
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(item) for item in value) + "]"
    return json.dumps(value, ensure_ascii=False)


def toml_lines(table):
    return [f"{key} = {toml_value(value)}" for key, value in table.items()]


def config(args, settings):
    stage = {
        "id": "capture",
        "name": "Capture",
        "role": "research",
        "harness": args.harness,
        "model": args.model,
        "provider": args.provider,
        "mode": "unattended",
        "instructions": "Do not use tools or change files. Reply only with the requested handoff block.",
        "extra_args": args.extra_arg,
        "timeout_seconds": args.timeout,
    }
    lines = ["version = 1", "", "[[workflows]]", 'name = "Capture"', ""]
    lines += ["[[workflows.stages]]", *toml_lines(stage)]
    if settings:
        lines += ["", "[workflows.stages.harness_settings]", *toml_lines(settings)]
    return "\n".join(lines) + "\n"


def append_manifest(path, entry):
    header = (
        "" if path.exists() else "# Harness output fixtures for handoff::normalize.\n"
    )
    with path.open("a") as manifest:
        manifest.write(
            header + "\n".join(["", "[[fixture]]", *toml_lines(entry)]) + "\n"
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("harness", choices=sorted(HARNESSES))
    parser.add_argument(
        "--confirm-paid-request",
        action="store_true",
        help="acknowledge that one real, possibly billed, model request is sent",
    )
    parser.add_argument(
        "--model", default="", help="native model ID (default: harness default)"
    )
    parser.add_argument("--provider", default="")
    parser.add_argument("--scenario", default="captured-success")
    parser.add_argument(
        "--extra-arg",
        action="append",
        default=[],
        help="extra harness argument (repeatable)",
    )
    parser.add_argument(
        "--router", type=Path, default=ROOT / "target/debug/model-router"
    )
    parser.add_argument("--output-dir", type=Path, default=FIXTURES)
    parser.add_argument(
        "--timeout", type=int, default=600, help="stage timeout in seconds"
    )
    args = parser.parse_args()
    if not args.confirm_paid_request:
        sys.exit(
            f"This sends one real model request through {args.harness}, which may be billed.\n"
            "Re-run with --confirm-paid-request to proceed."
        )
    if not args.router.is_file():
        sys.exit(f"Router not found: {args.router}. Run cargo build or pass --router.")
    kind, extension, settings = HARNESSES[args.harness]
    relative = f"{args.harness}/{args.scenario}.{extension}"
    target = args.output_dir / relative
    if target.exists():
        sys.exit(f"{target} exists; choose another --scenario")
    program = args.harness
    if program == "cursor":
        program = "cursor-agent" if shutil.which("cursor-agent") else "agent"
    version = version_of(program)
    summary = f"Captured {args.harness} fixture"
    handoff = {
        "summary": summary,
        "verdict": "complete",
        "findings": [],
        "decisions": [],
        "files_changed": [],
        "tests": [],
        "unresolved": [],
    }
    task = (
        "Do not use tools or change files. Reply with exactly this block and nothing else:\n"
        f"<router-handoff>\n{json.dumps(handoff)}\n</router-handoff>"
    )
    with tempfile.TemporaryDirectory(prefix="router-capture-") as name:
        project = Path(name)
        config_path = project / "capture.toml"
        config_path.write_text(config(args, settings))
        print(f"Sending one request through {program} {version}...", file=sys.stderr)
        run = subprocess.run(
            [str(args.router), "-C", str(project), "--config", str(config_path)]
            + ["run", "--task", task],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=args.timeout + 60,
        )
        logs = sorted(
            project.glob(".model-router/runs/*/01-capture/attempt-*/stdout.log")
        )
        if run.returncode != 0 or not logs:
            sys.exit(
                f"Run failed (exit {run.returncode}); nothing saved.\n"
                f"{run.stdout[-2000:]}{run.stderr[-2000:]}"
            )
        prompt = (logs[-1].parent / "prompt.md").read_text()
        text, notes = sanitize(
            logs[-1].read_text(errors="replace"),
            prompts=[prompt, task],
            paths=[str(project), os.path.realpath(project)],
            home=str(Path.home()),
            user=getpass.getuser(),
            host=socket.gethostname(),
        )
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text)
    model = f"model {args.model}" if args.model else "harness default model"
    append_manifest(
        args.output_dir / "manifest.toml",
        {
            "file": relative,
            "harness": args.harness,
            "kind": kind,
            "version": version,
            "scenario": args.scenario,
            "provenance": "captured",
            "source": f"scripts/capture_harness_output.py, {datetime.date.today()}, {model}",
            "sanitization": notes,
            "text_contains": [summary],
            "handoff_summary": summary,
        },
    )
    print(
        f"Saved {target} ({notes}). Review it, then run: cargo test --test harness_output"
    )


if __name__ == "__main__":
    main()
