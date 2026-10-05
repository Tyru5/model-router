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
if args.model == "malformed":
    print("Looks fine, probably!")
    sys.exit(0)
blocking = args.role.startswith("review") and number <= args.review_fail_until
handoff = {
    "summary": f"{args.role} evidence from model {args.model}, attempt {number}",
    "verdict": "needs_changes"
    if blocking
    else ("pass" if args.role.startswith("review") else "complete"),
    "findings": [{"severity": "blocking", "description": "Mock required fix"}]
    if blocking
    else [],
    "decisions": ["Offline fixture only"],
    "files_changed": [],
    "tests": [
        {
            "command": "offline acceptance check",
            "status": args.test_status,
            "required": True,
        }
    ]
    if args.test_status
    else [],
    "unresolved": [],
}
if not args.no_handoff:
    Path(args.handoff_file).write_text(json.dumps(handoff))
print("<router-handoff>" + json.dumps(handoff) + "</router-handoff>", flush=True)
