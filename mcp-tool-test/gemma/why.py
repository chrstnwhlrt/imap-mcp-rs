#!/usr/bin/env python3
"""Gemma's reasoning before the first model call of every answer in run
TAG whose request matches REGEX, with the calls it made: why it took the
tool it took.

    why.py TAG REGEX [--chars N]
"""
import json
import re
import sys
from pathlib import Path

WORK = Path(__file__).resolve().parent / "work"
tag, pattern = sys.argv[1], sys.argv[2]
chars = int(sys.argv[sys.argv.index("--chars") + 1]) if "--chars" in sys.argv else 1500
for line in open(WORK / f"{tag}.bodies.jsonl"):
    row = json.loads(line)
    messages = row["body"].get("messages") or []
    # The assistant's gate asks without tools, the chat with them.
    if not row["body"].get("tools") or not messages or messages[-1].get("role") != "user":
        continue
    request = messages[-1]["content"]
    if not re.search(pattern, request):
        continue
    calls = [f"{c['function']['name']}{json.dumps(c['function'].get('arguments'), ensure_ascii=False)}" for c in row["calls"]]
    print(f"=== {request.splitlines()[-1]!r} think={row['body'].get('think')} -> {calls or row['content'][:120]!r}")
    print("   ", row["thinking"][:chars].replace("\n", "\n    "))
