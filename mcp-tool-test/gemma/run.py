#!/usr/bin/env python3
"""Whether Gemma 4 E4B uses the mail tools right, as the voice assistant
Otomo puts them in front of it: its own system prompt, its gate, its
built-in tools and the tools of imap-mcp-rs as its MCP client lists them.
The real server answers, against the mail world of welt.py (Dovecot in a
container), so nothing reaches a real mailbox. textproxy.py between them
puts the texts of a variant in place (--texts) and keeps every call;
proxy.py in front of Ollama keeps every model call with its reasoning, for
why.py. Before every conversation the world is set back (welt.py reset),
after it the mailbox is read (mailstate.py), so that a check sees what the
conversation changed.

    run.py TAG [--runs N] [--only REGEX] [--texts FILE] [--set NAME]
    run.py TAG --judge        score the answers of TAG again

The checkout of Otomo, with a release build of otomo-probe and its
config.toml, is OTOMO_DIR (default: otomo next to this repository); the
identity of that configuration that asks is OTOMO_IDENTITY (default:
Christian). The folder of welt.py is IMAP_WORLD (default: this folder).

Writes work/TAG.jsonl (one line per scored answer, with the mailbox after
its conversation), TAG.calls.jsonl (the tool calls), TAG.bodies.jsonl (the
model calls) and TAG.txt (the score).
"""
import argparse
import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import time
import tomllib
from collections import Counter, defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
WORK = HERE / "work"
WORLD = Path(os.environ.get("IMAP_WORLD") or HERE)
OTOMO = Path(os.environ.get("OTOMO_DIR") or WORLD.parents[2] / "otomo")
SERVER = "mail"
IDENTITY = os.environ.get("OTOMO_IDENTITY") or "Christian"
BINARY = str(WORLD.parents[1] / "target" / "release" / "imap-mcp-rs")


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait_for(port, seconds=10):
    end = time.time() + seconds
    while time.time() < end:
        try:
            socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
            return
        except OSError:
            time.sleep(0.1)
    sys.exit(f"nothing answers on port {port}")


def sections(text):
    """The configuration as its tables: (header, lines), the lines before the first table under the header None."""
    found, header, lines = [], None, []
    for line in text.splitlines(keepends=True):
        if line.startswith("["):
            found.append((header, lines))
            header, lines = line.split("#")[0].strip(), [line]
        else:
            lines.append(line)
    found.append((header, lines))
    return found


def assistant_config(path, proxy, server_args):
    """The assistant's own configuration for the run: IDENTITY alone, with the mail server as its only tool server,
    never asking before a call (an open question would carry over into the next request), and the proxy in front of
    Ollama. Returns it with the address of Ollama. Never printed: it holds the assistant's secrets."""
    text = path.read_text()
    config = tomllib.loads(text)
    upstream = config.get("llm", {}).get("ollama", {}).get("base_url")
    if not upstream or text.count(f'"{upstream}"') != 1:
        sys.exit(f"{path} needs llm.ollama.base_url, once, for the proxy to take its place")
    kept = []
    for header, lines in sections(text):
        if header == "[[mcp.servers]]" or (header or "").startswith(("[identities.", "[mcp.servers.")):
            continue
        if header == "[[identities]]":
            if not any(re.match(rf'\s*name\s*=\s*"{IDENTITY}"', line) for line in lines):
                continue
            lines = [re.sub(r"^(\s*servers\s*=\s*)\[[^\]]*\]", rf'\1["{SERVER}"]', line) for line in lines
                     if not re.match(r"\s*ids\s*=", line)]
        kept.append((header, lines))
    block = (f'[[mcp.servers]]\nname = "{SERVER}"\ntransport = "stdio"\nconfirm = "never"\n'
             f'command = {json.dumps(sys.executable)}\nargs = {json.dumps(server_args)}\n\n')
    at = next(i for i, (header, _) in enumerate(kept) if header == "[mcp]")
    kept.insert(at + 1, ("[[mcp.servers]]", [block]))
    text = "".join(line for _, lines in kept for line in lines).replace(f'"{upstream}"', f'"{proxy}"')
    checked = tomllib.loads(text)
    identities = checked.get("identities", [])
    assert [i["name"] for i in identities] == [IDENTITY] and identities[0]["servers"] == [SERVER], "identities"
    assert [s["name"] for s in checked["mcp"]["servers"]] == [SERVER], "servers"
    return text, upstream


def turn_spans(questions, log):
    """Where each request of a run stands in the proxy's log: the numbers of its own model requests up to the next
    request of the run ("turns"), and those of the requests asked before it in its conversation ("context")."""
    entries = [json.loads(line) for line in open(log)] if log.exists() else []
    found, at = [], 0
    for question in questions:
        hit = next((i for i in range(at, len(entries)) if question.removeprefix("+") in entries[i]["text"]), None)
        found.append(hit)
        at = at if hit is None else hit + 1
    spans, begun = [], None
    for k, (question, hit) in enumerate(zip(questions, found)):
        if not question.startswith("+"):
            begun = hit
        if hit is None:
            spans.append({})
            continue
        following = next((i for i in found[k + 1:] if i is not None), len(entries))
        span = {"turns": [entries[hit]["asked"], entries[following]["asked"] if following < len(entries)
                          else entries[-1]["asked"] + 1]}
        if question.startswith("+") and begun is not None and begun < hit:
            span["context"] = [entries[begun]["asked"], entries[hit]["asked"]]
        spans.append(span)
    return spans


def reset_world():
    result = subprocess.run([sys.executable, "-I", str(WORLD / "welt.py"), "reset"], capture_output=True, text=True)
    if result.returncode:
        sys.exit("welt.py reset failed: " + result.stderr[-500:])


CONFIG = "config.toml"


def ask(tag, cases, runs, texts, binary=BINARY, server_args=()):
    import mailstate
    probe = OTOMO / "target" / "release" / "otomo-probe"
    WORK.mkdir(exist_ok=True)
    files = {k: WORK / f"{tag}.{k}" for k in ("calls.jsonl", "bodies.jsonl", "turns", "texts.json")}
    for path in list(files.values()) + [WORK / f"{tag}.turns.log", WORK / f"{tag}.log"]:
        path.unlink(missing_ok=True)
    files["texts.json"].write_text(Path(texts).read_text() if texts else "{}")
    model_port = free_port()
    # The assistant starts the server in its own folder: a path given relative to here would not be found there.
    binary = str(Path(binary).resolve()) if Path(binary).exists() else binary
    server = ["-I", str(HERE / "textproxy.py"), str(files["texts.json"]), str(files["calls.jsonl"]),
              str(files["turns"]), "--", binary, "--config", str(WORLD / "run" / CONFIG), *server_args]
    text, upstream = assistant_config(OTOMO / "config.toml", f"http://127.0.0.1:{model_port}", server)
    proxy = subprocess.Popen([sys.executable, "-I", str(HERE / "proxy.py"), str(model_port), upstream,
                              str(files["bodies.jsonl"]), str(files["turns"])],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    questions, rows = [], []
    try:
        wait_for(model_port)
        with tempfile.TemporaryDirectory(dir=WORK) as tmp:
            path = Path(tmp) / "config.toml"
            path.touch(mode=0o600)
            path.write_text(text)
            for _ in range(runs):
                for case in cases:
                    asked = [q if i == 0 else "+" + q for i, q in enumerate(case["context"] + [case["q"]])]
                    reset_world()
                    began = time.time()
                    result = subprocess.run(["nix", "develop", "-c", str(probe), "--config", str(path), "chat",
                                             "--json", "--identity", IDENTITY, *asked],
                                            cwd=OTOMO, capture_output=True, text=True,
                                            env={**os.environ, "RUST_LOG": "info"})
                    with open(WORK / f"{tag}.log", "a") as log:
                        log.write(result.stderr)
                    answers = [json.loads(line) for line in result.stdout.splitlines() if line.startswith("{")][1:]
                    if len(answers) != len(asked):
                        answers = (answers + [{"answer": "", "error": "no answer"}] * len(asked))[:len(asked)]
                    state = mailstate.read()
                    questions += asked
                    for i, answer in enumerate(answers):
                        last = i == len(asked) - 1
                        rows.append({"case": case["key"] if last else None, **answer,
                                     **({"state": state, "seconds": round(time.time() - began, 1)} if last else {})})
    finally:
        proxy.terminate()
        proxy.wait()
    spans = turn_spans(questions, WORK / f"{tag}.turns.log")
    with open(WORK / f"{tag}.jsonl", "w") as f:
        for n, (row, span) in enumerate(zip(rows, spans), 1):
            if row["case"] is not None:
                f.write(json.dumps({**row, "n": n, **span}, ensure_ascii=False) + "\n")


def pools(catalogue):
    return {"cases": catalogue.CASES, "held": catalogue.HELD, "fresh": catalogue.FRESH, "extra": catalogue.EXTRA,
            "nurlesen": catalogue.NUR_LESEN, "freshnurlesen": catalogue.FRESH_NUR_LESEN}


def score(tag, only=""):
    import cases as catalogue
    pool = WORK / f"{tag}.pool"
    by_key = {}
    for case in catalogue.EXTRA + catalogue.FRESH + catalogue.HELD + catalogue.CASES:
        by_key[case["key"]] = case
    for case in pools(catalogue)[pool.read_text().strip()] if pool.exists() else []:
        by_key[case["key"]] = case
    calls = [json.loads(line) for line in open(WORK / f"{tag}.calls.jsonl")] if (WORK / f"{tag}.calls.jsonl").exists() else []
    rows = [json.loads(line) for line in open(WORK / f"{tag}.jsonl")]
    per_case, groups, notes = defaultdict(list), defaultdict(lambda: [0, 0]), Counter()
    out = []
    for row in rows:
        case = by_key.get(row["case"])
        if case is None or (only and not re.search(only, row["case"])):
            continue
        lo, hi = row.get("turns", [None, None])
        mine = [c for c in calls if lo is not None and lo <= c["turn"] < hi]
        ctx = row.get("context")
        context = [c for c in calls if ctx and ctx[0] <= c["turn"] < ctx[1]]
        answer = row.get("answer") or ""
        if lo is None:
            ok, why = False, "turn not found"
        else:
            ok, why = case["check"](mine, context, answer, row.get("state") or {})
        if ok and case.get("says") and not case["says"].search(answer):
            ok, why = False, "answer: " + answer[:100]
        for note in catalogue.notes_of(mine, answer, row.get("state") or {}):
            notes[note] += 1
        refused = sum(1 for c in mine if c["error"])
        if refused:
            notes["refused calls"] += refused
        per_case[row["case"]].append((ok, why))
        groups[case["group"]][0] += ok
        groups[case["group"]][1] += 1
        out.append({"case": row["case"], "ok": ok, "why": why, "answer": answer,
                    "calls": [{"name": c["name"], "arguments": c["arguments"], "error": c["error"],
                               **({"text": c["text"][:200]} if c["error"] else {})} for c in mine],
                    "seconds": row.get("seconds")})
    right = sum(ok for results in per_case.values() for ok, _ in results)
    total = sum(len(results) for results in per_case.values())
    calls_per = [len(o["calls"]) for o in out]
    lines = [f"{tag}: {right}/{total} right"]
    lines.append("groups: " + ", ".join(f"{g} {r}/{t}" for g, (r, t) in sorted(groups.items())))
    lines.append("notes: " + ", ".join(f"{k} {v}" for k, v in notes.most_common()))
    if out:
        seconds = [o["seconds"] for o in out if o["seconds"]]
        lines.append(f"calls per request {sum(calls_per) / len(out):.1f}, seconds per conversation "
                     f"{sum(seconds) / max(1, len(seconds)):.1f}")
    for key, results in per_case.items():
        hits = sum(ok for ok, _ in results)
        whys = "; ".join(sorted({why for ok, why in results if not ok}))
        lines.append(f"  {hits}/{len(results)}  {key[:110]}" + (f"  -- {whys}" if whys else ""))
    (WORK / f"{tag}.txt").write_text("\n".join(lines) + "\n")
    (WORK / f"{tag}.scored.jsonl").write_text("".join(json.dumps(o, ensure_ascii=False) + "\n" for o in out))
    print("\n".join(lines))


def main():
    p = argparse.ArgumentParser()
    p.add_argument("tag")
    p.add_argument("--runs", type=int, default=1)
    p.add_argument("--only", default="")
    p.add_argument("--texts")
    p.add_argument("--judge", action="store_true")
    p.add_argument("--set", choices=["cases", "held", "fresh", "extra", "nurlesen", "freshnurlesen"], default="cases",
                   help="the set of requests to ask")
    p.add_argument("--binary", default=BINARY, help="the imap-mcp-rs to measure (default: this repository's release build)")
    p.add_argument("--server-arg", action="append", default=[], help="an argument for the server")
    p.add_argument("--config", default="config.toml", help="the server's configuration in the world's run/ (config-nur-lesen.toml: both accounts read only, for --set nurlesen)")
    a = p.parse_args()
    global CONFIG
    CONFIG = a.config
    import cases as catalogue
    if not a.judge:
        chosen = [c for c in pools(catalogue)[a.set] if not a.only or re.search(a.only, c["key"])]
        WORK.mkdir(exist_ok=True)
        (WORK / f"{a.tag}.pool").write_text(a.set)
        ask(a.tag, chosen, a.runs, a.texts, a.binary, a.server_arg)
    score(a.tag, a.only)


if __name__ == "__main__":
    main()
