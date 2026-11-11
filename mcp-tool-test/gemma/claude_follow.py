#!/usr/bin/env python3
"""Requests with a follow-up through Claude Code, which claude_check.py
leaves out: each case asked as one conversation, the request and its
follow-ups in a single headless session over stream-json. The safeguards
are those of claude_check.py: no built-in tools, setting sources cut to
local, --strict-mcp-config with the mail server of the test world (behind
textproxy.py) as the only MCP server, an empty working directory, no
session persistence, and a stop as soon as a tool of another server shows
up, since that one could reach a real mailbox. The world is set back before
every conversation and the mailbox read after it; scored as run.py scores
Gemma, each request of the conversation on its own turn.

    claude_follow.py TAG [--set NAME] [--only REGEX] [--runs N] [--binary PATH] [--config FILE] [--model M]
"""
import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
WORK = HERE / "work"
FOREIGN = re.compile(r'"name":\s*"(mcp__(?!mail__)[a-z_]+)')


def converse(questions, config, model, turns, first_turn):
    """Ask `questions` in one session; the answers, with each request's calls under its own turn number."""
    import claude_check
    env = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE")}
    workdir = WORK / "claude-cwd"
    workdir.mkdir(parents=True, exist_ok=True)
    cmd = ["claude", "-p", "--model", model, "--setting-sources", "local", "--system-prompt", claude_check.SYSTEM,
           "--tools", "", "--mcp-config", str(config), "--strict-mcp-config", "--allowedTools", "mcp__mail",
           "--no-session-persistence", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"]
    proc = subprocess.Popen(cmd, cwd=workdir, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL, text=True)
    answers = []
    try:
        for k, question in enumerate(questions):
            turns.write_text(f"{first_turn + k} {first_turn + k}")
            proc.stdin.write(json.dumps({"type": "user", "message": {"role": "user", "content": question}}) + "\n")
            proc.stdin.flush()
            answer = ""
            for line in proc.stdout:
                if FOREIGN.search(line):
                    proc.kill()
                    sys.exit(f"STOP: tools of another server in the session: {FOREIGN.findall(line)}")
                try:
                    event = json.loads(line)
                except ValueError:
                    continue
                if event.get("type") == "result":
                    answer = event.get("result") or ""
                    break
            answers.append(answer)
    finally:
        proc.stdin.close()
        proc.wait(timeout=120)
    return answers


def main():
    p = argparse.ArgumentParser()
    p.add_argument("tag")
    p.add_argument("--set", default="km")
    p.add_argument("--only", default="")
    p.add_argument("--runs", type=int, default=1)
    p.add_argument("--model", default="claude-sonnet-5-5")
    p.add_argument("--binary", default=None, help="the imap-mcp-rs to measure")
    p.add_argument("--config", default="config.toml", help="the server's configuration in the world's run/")
    a = p.parse_args()
    import cases as catalogue
    import mailstate
    import run
    binary = str(Path(a.binary).resolve()) if a.binary else run.BINARY
    chosen = [c for c in run.pools(catalogue)[a.set] if not a.only or re.search(a.only, c["key"])]
    WORK.mkdir(exist_ok=True)
    (WORK / f"{a.tag}.pool").write_text(a.set)
    texts_file = WORK / f"{a.tag}.texts.json"
    texts_file.write_text("{}")
    rows, all_calls, turn = [], [], 0
    for _ in range(a.runs):
        for case in chosen:
            questions = case["context"] + [case["q"]]
            run.reset_world()
            with tempfile.TemporaryDirectory(dir=WORK) as tmp:
                tmp = Path(tmp)
                turns, calls = tmp / "turns", tmp / "calls.jsonl"
                server = ["-I", str(HERE / "textproxy.py"), str(texts_file), str(calls), str(turns), "--",
                          binary, "--config", str(run.WORLD / "run" / a.config)]
                config = tmp / "mcp.json"
                config.write_text(json.dumps({"mcpServers": {"mail": {"command": sys.executable, "args": server}}}))
                began = time.time()
                answers = converse(questions, config, a.model, turns, turn + 1)
                found = [json.loads(line) for line in open(calls)] if calls.exists() else []
            state = mailstate.read()
            last = turn + len(questions)
            rows.append({"case": case["key"], "n": last, "turns": [last, last + 1],
                         **({"context": [turn + 1, last]} if len(questions) > 1 else {}),
                         "answer": answers[-1], "state": state, "seconds": round(time.time() - began, 1)})
            all_calls += found
            turn = last
    with open(WORK / f"{a.tag}.jsonl", "w") as f:
        f.writelines(json.dumps(r, ensure_ascii=False) + "\n" for r in rows)
    with open(WORK / f"{a.tag}.calls.jsonl", "w") as f:
        f.writelines(json.dumps(c, ensure_ascii=False) + "\n" for c in all_calls)
    run.score(a.tag)


if __name__ == "__main__":
    main()
