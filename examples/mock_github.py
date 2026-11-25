#!/usr/bin/env python3
"""Offline, read-only GitHub CLI fixture for task import tests."""

import base64
import json
import os
import sys
import time
from pathlib import Path

args = sys.argv[1:]
assert args[:5] == ["api", "--hostname", "github.com", "--method", "GET"], args
assert args[6] == "--jq", args
if trace := os.environ.get("ROUTER_TEST_GH_TRACE"):
    with Path(trace).open("a") as stream:
        stream.write(json.dumps(args) + "\n")
time.sleep(float(os.environ.get("ROUTER_TEST_GH_DELAY", "0")))
if os.environ.get("ROUTER_TEST_GH_FAILURE") == "1":
    print("HTTP 404: Not Found (check repository access)", file=sys.stderr)
    sys.exit(1)
endpoint = args[5]
if "/git/matching-refs/heads/" in endpoint:
    print(json.dumps(["refs/heads/main", "refs/heads/feature/new"]))
elif "/git/matching-refs/tags/" in endpoint:
    print(json.dumps(["refs/tags/v1.0"]))
elif "/issues/" in endpoint:
    print(
        json.dumps(
            {
                "title": "GitHub issue spec",
                "body": "## Acceptance criteria\n\n- Preserve context 雪\n- Keep $(literal) text\n",
            }
        )
    )
elif "/contents/" in endpoint:
    body = "# GitHub file spec\n\n- Preserve context 雪\n".encode()
    print(
        json.dumps(
            {
                "type": "file",
                "encoding": "base64",
                "size": len(body),
                "content": base64.b64encode(body).decode(),
            }
        )
    )
else:
    raise AssertionError(f"Unexpected GitHub endpoint: {endpoint}")
