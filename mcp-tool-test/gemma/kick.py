#!/usr/bin/env python3
"""Reads and writes right after the server ended the session: Dovecot's
`doveadm kick` stands in for an idle timeout (Gmail and Outlook close quiet
IMAP sessions after a while). Every call has to succeed on a fresh session,
with complete results, and nothing may be written twice.

    kick.py [--binary PATH]

Builds the mail world (welt.py, without the extra mails), removes it
afterwards and exits 1 when a check fails. Without --binary it takes
target/release/imap-mcp-rs of this repository.

Before the fix a closed session read like the end of a command's answer:
LIST gave no folders (cached, so Drafts and Trash were gone until a
restart), STORE reported nothing changed and APPEND failed.
"""
import argparse
import json
import subprocess
import sys
import types
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
sys.path.insert(0, str(HERE))
import welt  # noqa: E402
from dump import Server  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--binary", default=str(REPO / "target" / "release" / "imap-mcp-rs"))
    args = parser.parse_args()

    welt.cmd_reset(types.SimpleNamespace(minimal=True))
    manifest = json.loads((welt.RUN / "manifest.json").read_text())
    uid = {m["key"]: m["uid"] for m in manifest["accounts"]["Privat"]["messages"]}
    login = welt.ACCOUNTS["Privat"]["login"]
    server = Server([args.binary, "--config", str(welt.RUN / "config.toml")])
    server.call("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                               "clientInfo": {"name": "kick", "version": "0"}})
    server.notify("notifications/initialized")

    def call(tool, arguments):
        result = server.call("tools/call", {"name": tool, "arguments": arguments})
        text = "".join(b.get("text", "") for b in result.get("content", []))
        return result.get("isError", False), text

    def kicked(tool, arguments):
        """`tool` on a session the server has just ended."""
        call("list_emails", {"folder": "INBOX", "limit": 1})  # a live session first
        subprocess.run(["podman", "exec", welt.CONTAINER, "doveadm", "kick", login], capture_output=True, check=True)
        return call(tool, arguments)

    checks = [
        ("list_folders", {}, lambda v: v["total"] == len(welt.ACCOUNTS["Privat"]["folders"])),
        ("search_emails", {"from": "Stadtwerke"}, lambda v: v["matched"] >= 1),
        ("get_email", {"folder": "INBOX", "uid": uid["mama-sonntag"]}, lambda v: v["email"]["uid"]),
        ("draft_email", {"to": ["ben@example.de"], "subject": "Nach dem Kick", "body": "Test"},
         lambda v: v.get("uid")),
        ("mark_as_read", {"folder": "INBOX", "uids": [uid["dhl-paket"]]}, lambda v: v["succeeded"]),
        ("flag_email", {"folder": "INBOX", "uids": [uid["dhl-paket"]]}, lambda v: v["succeeded"]),
        ("move_email", {"folder": "INBOX", "uids": [uid["dhl-paket"]], "target_folder": "Rechnungen"},
         lambda v: v["succeeded"]),
        ("delete_email", {"folder": "INBOX", "uids": [uid["mama-sonntag"]]}, lambda v: v["succeeded"]),
    ]
    failed = 0
    for tool, arguments, good in checks:
        is_error, text = kicked(tool, arguments)
        ok = not is_error and bool(good(json.loads(text)))
        failed += not ok
        print(f"{'ok  ' if ok else 'FAIL'} {tool:13} {'' if ok else text[:150]}")
    is_error, text = call("list_drafts", {"compact": True})
    copies = 0 if is_error else sum(d["subject"] == "Nach dem Kick" for d in json.loads(text)["drafts"])
    failed += copies != 1
    print(f"{'ok  ' if copies == 1 else 'FAIL'} the draft exists once ({copies})")
    server.close()
    welt.cmd_stop(None)
    print(f"{len(checks) + 1 - failed}/{len(checks) + 1} checks passed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
