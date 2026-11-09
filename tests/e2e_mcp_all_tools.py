#!/usr/bin/env python3
"""MCP end-to-end round: every one of the 19 tools once, as a real JSON-RPC
call against the built binary talking to GreenMail.

The Rust integration tests (integration_greenmail.rs) cover the IMAP client
layer; this script covers the MCP tool layer above it — request/response
shapes, the documented result fields (has_more, failed, retryable,
internal_date, allow_flag_change, ...), the isError flag on failed calls
and the dry_run previews, exactly as an MCP client sees them.

Prerequisites:
  ./test-server.sh                          # fresh GreenMail with seeded mail
                                            # (run right before: the sub-day
                                            # search asserts a 1-hour window)
  nix build  (or cargo build --release)     # a binary to talk to

Run:
  python3 tests/e2e_mcp_all_tools.py [path-to-binary]   # without a path:
                                            # result/ (nix build), then target/
  podman rm -f imap-test                    # stop the container when done

Exits 0 when every check passes, 1 otherwise. The run mutates the mailbox
(moves, deletes, drafts) — always give it a fresh container.
"""
import atexit
import json
import os
import shutil
import subprocess
import sys
import tempfile
from datetime import datetime, timedelta

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def find_binary():
    if len(sys.argv) > 1:
        return sys.argv[1]
    for cand in (
        "result/bin/imap-mcp-rs",
        "target/release/imap-mcp-rs",
        "target/debug/imap-mcp-rs",
    ):
        path = os.path.join(REPO, cand)
        if os.path.exists(path):
            return path
    sys.exit("no built binary found — run `nix build` or `cargo build --release` first")


# Self-contained working directory: config + the one attachment the draft
# round trip needs, so the script leaves no files in the repo.
workdir = tempfile.mkdtemp(prefix="imap-mcp-e2e-")
# Removed on every exit, a failed check or a crash included: it holds the
# config and the downloaded attachment, and each run used to leave one behind.
atexit.register(shutil.rmtree, workdir, ignore_errors=True)
attach_dir = os.path.join(workdir, "attachments")
os.makedirs(attach_dir)
attachment = os.path.join(attach_dir, "hello.txt")
with open(attachment, "w") as f:
    f.write("attachment payload for the e2e round\n")
config_path = os.path.join(workdir, "config.toml")
with open(config_path, "w") as f:
    f.write(
        f'allowed_attachment_dirs = ["{attach_dir}"]\n\n'
        "[[accounts]]\n"
        'name = "Greenmail"\n'
        'host = "127.0.0.1"\n'
        "port = 3993\n"
        'username = "test"\n'
        'email = "test@localhost"\n'
        'auth_method = "password"\n'
        'password = "password"\n'
        "accept_invalid_certs = true\n"
    )

proc = subprocess.Popen(
    [find_binary(), "--config", config_path],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.DEVNULL,
    text=True,
)
_id = 0


def rpc(method, params=None):
    global _id
    _id += 1
    req = {"jsonrpc": "2.0", "id": _id, "method": method}
    if params is not None:
        req["params"] = params
    proc.stdin.write(json.dumps(req) + "\n")
    proc.stdin.flush()
    while True:
        line = proc.stdout.readline()
        if not line:
            raise RuntimeError(f"server died during {method}")
        msg = json.loads(line)
        if msg.get("id") == _id:
            if "error" in msg:
                raise RuntimeError(f"{method} -> RPC error: {msg['error']}")
            return msg["result"]


def notify(method):
    proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": method}) + "\n")
    proc.stdin.flush()


# Every tools/call outcome as (tool, isError flag, payload is an error), for
# the isError checks at the end.
outcomes = []


def call(tool, args=None):
    res = rpc("tools/call", {"name": tool, "arguments": args or {}})
    payload = json.loads(res["content"][0]["text"])
    outcomes.append((tool, res.get("isError"), "error" in payload))
    return payload


checks = []


def local_time(stamp):
    """An ISO 8601 timestamp shown in this machine's zone: its offset is the
    one the local zone has at that instant."""
    if not stamp:
        return False
    moment = datetime.fromisoformat(stamp)
    return moment.utcoffset() == moment.astimezone().utcoffset()


def ok(name, cond, detail=""):
    checks.append((name, bool(cond), detail))
    if not cond:
        print(f"FAIL {name}: {detail}")


# --- Handshake ---
rpc(
    "initialize",
    {
        "protocolVersion": "2026-07-28",
        "capabilities": {},
        "clientInfo": {"name": "e2e", "version": "0"},
    },
)
notify("notifications/initialized")
tools = rpc("tools/list")["tools"]
names = sorted(t["name"] for t in tools)
ok("tools/list: 19 tools", len(names) == 19, f"{len(names)}: {names}")
# Annotations tell a client where to ask before a call: without them MCP
# treats every tool as destructive. Only the two deletes may be.
destructive = sorted(
    t["name"] for t in tools if not (t.get("annotations") or {}).get("readOnlyHint")
    and (t.get("annotations") or {}).get("destructiveHint", True)
)
ok(
    "tools/list: every tool annotated, only the deletes destructive",
    all(t.get("annotations") for t in tools) and destructive == ["delete_draft", "delete_email"],
    str(destructive),
)

# --- 1 list_accounts ---
acc = call("list_accounts")
a0 = acc["accounts"][0]
ok(
    "list_accounts: all six fields",
    all(
        k in a0
        for k in [
            "name",
            "email",
            "read_only",
            "allow_move",
            "allow_delete",
            "allow_flag_change",
        ]
    ),
    str(a0),
)

# --- 2 list_folders ---
folders = call("list_folders")
roles = {f.get("role"): f["name"] for f in folders["folders"] if f.get("role")}
ok("list_folders: INBOX present", any(f["name"] == "INBOX" for f in folders["folders"]))
ok("list_folders: drafts role detected", "drafts" in roles, str(roles))
ok(
    "list_folders: readable name next to an encoded one",
    any(f["name"] == "B&APw-cher" and f.get("display_name") == "Bücher" for f in folders["folders"]),
    str(folders["folders"]),
)
drafts_folder = roles.get("drafts", "Drafts")

# --- 3 list_emails ---
inbox = call("list_emails", {"folder": "INBOX", "limit": 50})
rows = inbox["emails"]
ok("list_emails: seeds present", len(rows) >= 4, f"{len(rows)}")
ok("list_emails: has_more field", "has_more" in inbox)
ok(
    "list_emails: no internal_date without a bound",
    all("internal_date" not in r for r in rows),
)
ok(
    "list_emails: no \\Recent",
    all("\\Recent" not in r.get("flags", []) for r in rows),
)
ok(
    "list_emails: dates in the machine's local time",
    all(r.get("date") is None or local_time(r["date"]) for r in rows),
    str([r.get("date") for r in rows]),
)
uid0 = rows[0]["uid"]

# --- 4 get_email ---
full = call("get_email", {"folder": "INBOX", "uid": uid0})
em = full.get("email") or {}
ok(
    "get_email: body + content warning",
    em.get("body_text") and "content_warning" in full,
    str(full)[:120],
)
# The seeds are text/plain only: no HTML part, so no synthesized body_html.
full_html = call("get_email", {"folder": "INBOX", "uid": uid0, "include_html": True})
ok(
    "get_email: text-only mail has no body_html",
    (full_html.get("email") or {}).get("body_html") is None,
    str(full_html)[:150],
)

# --- 5 get_thread ---
thread = call("get_thread", {"folder": "INBOX", "uid": uid0})
ok(
    "get_thread: >=1 message",
    thread.get("message_count", 0) >= 1 and len(thread.get("emails", [])) >= 1,
)

# --- 6 search_emails (text + sub-day since) ---
# GreenMail quirk: FROM only matches the full address, not a substring.
s1 = call("search_emails", {"from": "alice@example.com", "folder": "INBOX"})
ok("search_emails: from hit", s1.get("returned", 0) >= 1, str(s1)[:150])
since = (datetime.now() - timedelta(hours=1)).strftime("%Y-%m-%dT%H:%M")
s2 = call("search_emails", {"since": since, "folder": "INBOX", "limit": 50})
ok("search sub-day: hits", s2.get("returned", 0) >= 4, str(s2)[:150])
ok(
    "search sub-day: internal_date on every row",
    all(local_time(e.get("internal_date")) for e in s2["emails"]),
    str(s2["emails"][:1]),
)
ok(
    "search sub-day: matched == returned (cut before count)",
    s2.get("matched") == s2.get("returned"),
    f"m={s2.get('matched')} r={s2.get('returned')}",
)

# --- 7 draft_email (with attachment) ---
d1 = call(
    "draft_email",
    {
        "to": ["bob@example.com"],
        "subject": "E2E attach",
        "body": "hi",
        "attachments": [attachment],
    },
)
ok("draft_email: uid", isinstance(d1.get("uid"), int), str(d1)[:150])
# Recipients: `Name <address>` is accepted (the response echoes the address),
# a malformed entry is refused before anything is saved.
named = call(
    "draft_email",
    {"to": ["Bob Example <bob@example.com>"], "subject": "E2E named", "body": "hi"},
)
ok(
    "draft_email: Name <address> recipient",
    named.get("to") == ["bob@example.com"] and isinstance(named.get("uid"), int),
    str(named)[:150],
)
bad_rcpt = call(
    "draft_email",
    {"to": ["bob@example.com, carol@example.com"], "subject": "x", "body": "y"},
)
ok(
    "draft_email: malformed recipient refused",
    bad_rcpt.get("error") and bad_rcpt.get("retryable") is False,
    str(bad_rcpt),
)

# --- 8 get_email on the draft -> index ---
dfull = call("get_email", {"folder": drafts_folder, "uid": d1["uid"]})
atts = (dfull.get("email") or {}).get("attachments") or []
ok("draft: attachment carries index", atts and atts[0].get("index") == 0, str(atts))

# --- 9 download_attachment by index ---
dl = call("download_attachment", {"folder": drafts_folder, "uid": d1["uid"], "index": 0})
ok(
    "download_attachment: saved",
    dl.get("saved_to") and dl.get("size", 0) > 0,
    str(dl),
)

# --- 10/11 draft_reply + draft_forward ---
r1 = call("draft_reply", {"folder": "INBOX", "uid": uid0, "body": "Thanks!"})
ok("draft_reply: uid", isinstance(r1.get("uid"), int), str(r1)[:150])
f1 = call(
    "draft_forward",
    {"folder": "INBOX", "uid": uid0, "to": ["carol@example.com"], "body": "fyi"},
)
ok("draft_forward: uid", isinstance(f1.get("uid"), int), str(f1)[:150])

# --- 12 list_drafts ---
ld = call("list_drafts")
ok(
    "list_drafts: 3 drafts + fields",
    ld.get("returned", 0) >= 3 and "has_more" in ld,
    str(ld)[:150],
)

# --- 13 delete_draft (real + stale) ---
del1 = call("delete_draft", {"uids": [d1["uid"], r1["uid"], f1["uid"], named["uid"]]})
ok(
    "delete_draft: all succeeded, failed empty",
    sorted(del1.get("succeeded", []))
    == sorted([d1["uid"], r1["uid"], f1["uid"], named["uid"]])
    and del1.get("failed") == [],
    str(del1),
)
del2 = call("delete_draft", {"uids": [d1["uid"]]})
ok("delete_draft stale: failed reported", del2.get("failed") == [d1["uid"]], str(del2))

# --- 14-17 mark/flag (dry_run + real) ---
dr = call("mark_as_read", {"folder": "INBOX", "uids": [uid0], "dry_run": True})
ok(
    "mark_as_read dry_run: preview without execution",
    dr.get("dry_run") is True and "would_mark_read" in dr,
    str(dr),
)
mr = call("mark_as_read", {"folder": "INBOX", "uids": [uid0]})
ok("mark_as_read: succeeded", uid0 in mr.get("succeeded", []), str(mr))
mu = call("mark_as_unread", {"folder": "INBOX", "uids": [uid0]})
ok("mark_as_unread: succeeded", uid0 in mu.get("succeeded", []), str(mu))
fl = call("flag_email", {"folder": "INBOX", "uids": [uid0]})
ok("flag_email: succeeded", uid0 in fl.get("succeeded", []), str(fl))
uf = call("unflag_email", {"folder": "INBOX", "uids": [uid0]})
ok("unflag_email: succeeded", uid0 in uf.get("succeeded", []), str(uf))

# --- 18 move_email (dry_run + real) ---
mv_dry = call(
    "move_email",
    {"folder": "INBOX", "uids": [uid0], "target_folder": "Sent", "dry_run": True},
)
ok("move_email dry_run: preview", mv_dry.get("dry_run") is True, str(mv_dry)[:150])
# A move into the source folder moves nothing — refused, the preview too.
mv_self = call(
    "move_email",
    {"folder": "INBOX", "uids": [uid0], "target_folder": "INBOX", "dry_run": True},
)
ok(
    "move_email: same folder refused",
    mv_self.get("error") and mv_self.get("retryable") is False,
    str(mv_self),
)
mv = call("move_email", {"folder": "INBOX", "uids": [uid0], "target_folder": "Sent"})
ok(
    "move_email: succeeded + failed empty",
    uid0 in mv.get("succeeded", []) and mv.get("failed") == [],
    str(mv),
)
sent = call("list_emails", {"folder": "Sent", "limit": 10})
moved_uid = next((r["uid"] for r in sent["emails"]), None)
ok("move_email: arrived in Sent", moved_uid is not None, str(sent)[:150])

# --- 19 delete_email (real + stale) ---
de = call("delete_email", {"folder": "Sent", "uids": [moved_uid]})
ok(
    "delete_email: succeeded + failed empty",
    moved_uid in de.get("succeeded", []) and de.get("failed") == [],
    str(de),
)
de2 = call("delete_email", {"folder": "Sent", "uids": [moved_uid]})
ok("delete_email stale: failed reported", de2.get("failed") == [moved_uid], str(de2))

# --- error path: retryable ---
err = call("search_emails", {})
ok(
    "error: retryable=false for a fact",
    err.get("error") and err.get("retryable") is False,
    str(err),
)

# --- error path: a missing folder reads as the server's text ---
nf = call("list_emails", {"folder": "NoSuchFolder"})
ok(
    "error: missing folder named, with the existing ones",
    nf.get("error")
    and "Outcome" not in nf["error"]
    and "Some(" not in nf["error"]
    and "Folders: INBOX" in nf["error"]
    and nf.get("retryable") is False,
    str(nf),
)

# --- folder names as people say them: a role word, another case ---
by_role = call("list_emails", {"folder": "Papierkorb", "limit": 5})
by_case = call("list_emails", {"folder": "sent", "limit": 5})
by_readable = call("list_emails", {"folder": "Bücher", "limit": 5})
ok(
    "list_emails: role word, other case and readable name resolve to the wire name",
    by_role.get("folder") == "Trash"
    and by_case.get("folder") == "Sent"
    and by_readable.get("folder") == "B&APw-cher",
    f"{str(by_role)[:120]} {str(by_case)[:120]} {str(by_readable)[:120]}",
)

# --- account_health (after real IMAP traffic) ---
health = call("account_health")
h0 = health["accounts"][0]
ok(
    "account_health: connected + auth_method, no token fields",
    h0.get("connected") is True
    and h0.get("auth_method") == "password"
    and "oauth_token_valid" not in h0
    and "last_error" not in h0,
    str(h0),
)

# --- isError: the MCP flag for a failed tool execution ---
# A client that goes by the flag must see the same outcome as one that
# parses the JSON: every error payload flagged true, every success not.
flagged = [(t, f) for t, f, is_err in outcomes if is_err]
ok(
    "isError: true on every error result",
    len(flagged) >= 3 and all(f is True for _, f in flagged),
    str(flagged),
)
unflagged = [(t, f) for t, f, is_err in outcomes if not is_err]
ok(
    "isError: never set on a success",
    unflagged and all(f in (False, None) for _, f in unflagged),
    str([(t, f) for t, f in unflagged if f not in (False, None)]),
)

proc.stdin.close()
proc.wait(timeout=10)
passed = sum(1 for _, c, _ in checks if c)
print(f"\n{passed}/{len(checks)} checks passed")
sys.exit(0 if passed == len(checks) else 1)
