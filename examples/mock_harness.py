#!/usr/bin/env python3
"""Offline harness fixture. Makes no network requests and uses no credentials."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time

parser = argparse.ArgumentParser()
parser.add_argument("--role", default="research")
parser.add_argument("--prompt-file", required=True)
parser.add_argument("--handoff-file", required=True)
parser.add_argument("--model", default="demo")
parser.add_argument("--review-fail-until", type=int, default=0)
parser.add_argument("--require-context", default="")
parser.add_argument("--interactive", action="store_true")
parser.add_argument("--delay", type=float, default=0)
parser.add_argument("--no-handoff", action="store_true")
parser.add_argument("--test-status", choices=["passed", "failed", "not_run"])
parser.add_argument("--test-optional", action="store_true")
parser.add_argument("--not-applicable", metavar="REASON")
parser.add_argument(
    "--write-file", metavar="PATH", help="Write a line to PATH in the workspace"
)
parser.add_argument(
    "--select", metavar="CANDIDATE", help="Judge a fan-out: choose CANDIDATE"
)
args = parser.parse_args()

root = Path(".model-router/mock")
root.mkdir(parents=True, exist_ok=True)
counter = root / (args.role + ".count")
number = int(counter.read_text()) + 1 if counter.exists() else 1
counter.write_text(str(number))
prompt = Path(args.prompt_file).read_text()
(root / (args.role + ".prompt")).write_text(prompt)
(root / (args.role + ".argv.json")).write_text(json.dumps(sys.argv))
if args.require_context and args.require_context not in prompt:
    sys.exit("Required prior-stage context is absent")
if args.model == "fail" or (args.model == "fail-once" and number == 1):
    print("Simulated authentication or harness failure", file=sys.stderr)
    sys.exit(17)
if args.model == "spawn-child":
    child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(300)"])
    (root / "child.pid").write_text(str(child.pid))
    time.sleep(300)
if args.delay:
    time.sleep(args.delay)
if args.interactive:
    assert sys.stdin.isatty() and sys.stdout.isatty(), "A real terminal is required"
    print("MOCK INTERACTIVE READY — type done to finish", flush=True)
    reply = input()
    (root / "interactive-input.txt").write_text(reply)
if args.write_file:
    target = Path(args.write_file)
    target.parent.mkdir(parents=True, exist_ok=True)
    # Model lines-N writes N lines, so fan-out candidates differ in size.
    lines = int(args.model[6:]) if args.model.startswith("lines-") else 1
    with target.open("a") as file:
        for line in range(lines):
            file.write(f"written by {args.role} attempt {number} line {line + 1}\n")
if args.model == "malformed":
    print("Looks fine, probably!")
    sys.exit(0)
review = args.role.startswith("review") or args.select is not None
blocking = review and number <= args.review_fail_until
handoff = {
    "summary": f"{args.role} evidence from model {args.model}, attempt {number}",
    "verdict": "needs_changes"
    if blocking
    else ("pass" if review else "complete"),
    "findings": [{"severity": "blocking", "description": "Mock required fix"}]
    if blocking
    else [],
    "decisions": ["Offline fixture only"],
    "files_changed": [args.write_file] if args.write_file else [],
    "tests": [
        {
            "command": "offline acceptance check",
            "status": args.test_status,
            "required": not args.test_optional,
        }
    ]
    if args.test_status and not args.not_applicable
    else [],
    "unresolved": [],
}
if args.not_applicable:
    handoff["not_applicable"] = args.not_applicable
if args.select:
    handoff["selected"] = args.select
if not args.no_handoff:
    Path(args.handoff_file).write_text(json.dumps(handoff))
print("<router-handoff>" + json.dumps(handoff) + "</router-handoff>", flush=True)
