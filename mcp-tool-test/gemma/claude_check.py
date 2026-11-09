#!/usr/bin/env python3
"""The catalogue through Claude Code instead of Gemma, to see that texts
made for Gemma do not get worse for Claude: every request without a
context in a fresh headless session, with a system prompt that names its
session, no built-in tools, and the mail server behind textproxy.py (with
the texts of --texts) as its only MCP server. Neither user settings,
CLAUDE.md nor memory reach it (an empty working directory, setting sources
cut to local, --strict-mcp-config): a mail server of the user's own
settings would reach a real mailbox. One request at a time, the world set
back before each, the mailbox read after it; scored as run.py scores Gemma.

    claude_check.py TAG [--texts FILE] [--set NAME] [--model M] [--only REGEX]
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
SESSION = "garden planner"
SYSTEM = (f'You are Claude, working in the Claude Code session "{SESSION}" for the user. '
          "Answer in German, briefly.")


def session(prompt, config, model):
    env = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE")}
    workdir = WORK / "claude-cwd"
    workdir.mkdir(parents=True, exist_ok=True)
    cmd = ["claude", "-p", "--model", model, "--setting-sources", "local", "--system-prompt", SYSTEM, "--tools", "",
           "--mcp-config", str(config), "--strict-mcp-config", "--allowedTools", "mcp__mail",
           "--no-session-persistence", "--output-format", "stream-json", "--verbose", prompt]
    return subprocess.run(cmd, cwd=workdir, env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True,
                          timeout=600)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("tag")
    p.add_argument("--texts")
    p.add_argument("--set", default="cases")
    p.add_argument("--model", default="claude-sonnet-5-5")
    p.add_argument("--only", default="")
    p.add_argument("--binary", default=None, help="the imap-mcp-rs to measure")
    p.add_argument("--config", default="config.toml", help="the server's configuration in the world's run/")
    a = p.parse_args()
    import cases as catalogue
    import mailstate
    import run
    binary = str(Path(a.binary).resolve()) if a.binary else run.BINARY
    chosen = [c for c in run.pools(catalogue)[a.set] if not c["context"] and (not a.only or re.search(a.only, c["key"]))]
    WORK.mkdir(exist_ok=True)
    (WORK / f"{a.tag}.pool").write_text(a.set)
    texts_file = WORK / f"{a.tag}.texts.json"
    texts_file.write_text(Path(a.texts).read_text() if a.texts else "{}")
    rows, all_calls = [], []
    for n, case in enumerate(chosen, 1):
        run.reset_world()
        with tempfile.TemporaryDirectory(dir=WORK) as tmp:
            tmp = Path(tmp)
            (tmp / "turns").write_text("1 1")
            calls = tmp / "calls.jsonl"
            args = ["-I", str(HERE / "textproxy.py"), str(texts_file), str(calls), str(tmp / "turns"), "--",
                    binary, "--config", str(run.WORLD / "run" / a.config)]
            config = tmp / "mcp.json"
            config.write_text(json.dumps({"mcpServers": {"mail": {"command": sys.executable, "args": args}}}))
            began = time.time()
            result = session(case["q"], config, a.model)
            # Only the server of the run may be there: a tool of another server would reach a real mailbox.
            foreign = sorted(set(re.findall(r'"name":\s*"(mcp__(?!mail__)[a-z_]+)', result.stdout)))
            if foreign:
                sys.exit(f"STOP: tools of another server in the session: {foreign}")
            answer = ""
            for line in result.stdout.splitlines():
                try:
                    event = json.loads(line)
                except ValueError:
                    continue
                if event.get("type") == "result":
                    answer = event.get("result") or ""
            found = [json.loads(line) for line in open(calls)] if calls.exists() else []
        state = mailstate.read()
        rows.append({"case": case["key"], "n": n, "turns": [n, n + 1], "answer": answer, "state": state,
                     "seconds": round(time.time() - began, 1)})
        all_calls += [{**call, "turn": n} for call in found]
    with open(WORK / f"{a.tag}.jsonl", "w") as f:
        f.writelines(json.dumps(r, ensure_ascii=False) + "\n" for r in rows)
    with open(WORK / f"{a.tag}.calls.jsonl", "w") as f:
        f.writelines(json.dumps(c, ensure_ascii=False) + "\n" for c in all_calls)
    run.score(a.tag)


if __name__ == "__main__":
    main()
