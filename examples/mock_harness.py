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
parser.add_argument(
    "--cost",
    type=float,
    metavar="USD",
    help="Report USD spent, as a Claude stream-json result line",
)
parser.add_argument(
    "--state-dir", help="Where counters go (a read-only sandboxed task's own directory)"
)
parser.add_argument("--session-id", help="Session the router assigned")
parser.add_argument("--resume", metavar="SESSION", help="A follow-up turn on SESSION")
parser.add_argument(
    "--mcp-config", help="A delegating stage's MCP config (Claude's format)"
)
parser.add_argument(
    "--script",
    help="Delegation script: JSON {turns: [[action, ...], ...]} of tool calls per turn",
)
args = parser.parse_args()


def mcp_turn(config_path, script_path, prompt_text):
    """Starts the router's bridge from the MCP config and replays this
    turn's tool calls; every result goes to SCRIPT.calls.jsonl."""
    script = json.loads(Path(script_path).read_text())
    turn_file = Path(script_path + ".turn")
    turn = int(turn_file.read_text()) + 1 if turn_file.exists() else 1
    turn_file.write_text(str(turn))
    log = Path(script_path + ".calls.jsonl")
    def record(entry):
        with log.open("a") as f:
            f.write(json.dumps(entry) + "\n")
    record({"turn": turn, "prompt": prompt_text})
    turns = script.get("turns", [])
    actions = turns[turn - 1] if turn <= len(turns) else []
    if not actions:
        return turn, script
    server = json.loads(Path(config_path).read_text())["mcpServers"]["model-router"]
    env = dict(os.environ)
    env.update(server.get("env", {}))
    bridge = subprocess.Popen(
        [server["command"]] + server.get("args", []),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        env=env,
        text=True,
    )
    next_id = [0]
    def rpc(method, params):
        next_id[0] += 1
        bridge.stdin.write(json.dumps({"jsonrpc": "2.0", "id": next_id[0], "method": method, "params": params}) + "\n")
        bridge.stdin.flush()
        while True:
            line = bridge.stdout.readline()
            if not line:
                return {"error": "bridge closed"}
            message = json.loads(line)
            if message.get("id") == next_id[0]:
                return message
    rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "mock"}})
    for action in actions:
        if "call" in action:
            reply = rpc("tools/call", {"name": action["call"], "arguments": action.get("args", {})})
            result = reply.get("result", {})
            record({
                "turn": turn,
                "call": action["call"],
                "error": result.get("isError", False),
                "result": result.get("structuredContent"),
            })
    bridge.stdin.close()
    bridge.wait(timeout=30)
    return turn, script


root = Path(args.state_dir) if args.state_dir else Path(".model-router/mock")
root.mkdir(parents=True, exist_ok=True)
counter = root / (args.role + ".count")
number = int(counter.read_text()) + 1 if counter.exists() else 1
counter.write_text(str(number))
prompt = Path(args.prompt_file).read_text()
(root / (args.role + ".prompt")).write_text(prompt)
if args.script and args.mcp_config:
    turn, script = mcp_turn(args.mcp_config, args.script, prompt)
    handoff = (script.get("handoffs") or {}).get(str(turn)) or {
        "summary": f"parent turn {turn}",
        "verdict": "complete",
    }
    handoff.setdefault("findings", [])
    Path(args.handoff_file).write_text(json.dumps(handoff))
    print("<router-handoff>" + json.dumps(handoff) + "</router-handoff>", flush=True)
    sys.exit(0)
if args.model == "slow":
    # A delegated task its parent cancels.
    time.sleep(60)
if args.model == "ask" and not args.resume:
    # A delegated task that needs a decision first.
    handoff = {
        "summary": "need a decision",
        "verdict": "blocked",
        "unresolved": ["Which name should the function get?"],
    }
    Path(args.handoff_file).write_text(json.dumps(handoff))
    print("<router-handoff>" + json.dumps(handoff) + "</router-handoff>", flush=True)
    sys.exit(0)
if args.model == "ask":
    answer = prompt.strip().splitlines()[0] if prompt.strip() else ""
    handoff = {"summary": "answered: " + answer, "verdict": "complete"}
    Path(args.handoff_file).write_text(json.dumps(handoff))
    print("<router-handoff>" + json.dumps(handoff) + "</router-handoff>", flush=True)
    sys.exit(0)
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
    "verdict": "needs_changes" if blocking else ("pass" if review else "complete"),
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
if args.cost is not None:
    usage = {"input_tokens": 10, "output_tokens": 10}
    result = {"type": "result", "total_cost_usd": args.cost, "usage": usage}
    print(json.dumps(result), flush=True)
