#!/usr/bin/env python3
"""What imap-mcp-rs hands a model, captured against the mail world: the
initialize result with the instructions, tools/list exactly as served, and
for every tool example calls with their real results, error cases
included. Resets the world before and after, since the examples write.

    dump.py [--binary PATH] [--config PATH]

Writes run/dump.json (everything, machine-readable) and run/dump.md (the
same for reading), named after the configuration: run/config-voice.toml
gives run/dump-voice.json and run/dump-voice.md. Without --binary it takes
target/release/imap-mcp-rs of this repository, without --config
run/config.toml (`welt.py config`).
"""
import argparse
import datetime
import json
import subprocess
import sys
import types
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
sys.path.insert(0, str(HERE))
import welt  # noqa: E402


class Server:
    """A JSON-RPC session with the server over stdio."""

    def __init__(self, cmd):
        self.p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, text=True)
        self.n = 0

    def call(self, method, params=None):
        self.n += 1
        request = {"jsonrpc": "2.0", "id": self.n, "method": method}
        if params is not None:
            request["params"] = params
        self.p.stdin.write(json.dumps(request) + "\n")
        self.p.stdin.flush()
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError(f"server ended during {method}")
            message = json.loads(line)
            if message.get("id") == self.n:
                if "error" in message:
                    raise RuntimeError(f"{method}: {message['error']}")
                return message["result"]

    def notify(self, method):
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": method}) + "\n")
        self.p.stdin.flush()

    def close(self):
        self.p.stdin.close()
        self.p.wait(timeout=20)


def examples(uid, documents):
    """(tool, arguments, what it shows). `uid` maps a world key to its UID;
    UIDs of drafts written during the run are filled in on the way."""
    yesterday = (datetime.date.today() - datetime.timedelta(days=1)).isoformat()
    return [
        ("list_accounts", {}, "Konten und Rechte"),
        ("account_health", {}, "Verbindungszustand"),
        ("list_folders", {}, "Ordner mit Rollen; Namen modified-UTF-7, display_name zum Anzeigen"),
        ("list_emails", {"folder": "INBOX", "limit": 5}, "die fünf neuesten Mails, volle Zeilen"),
        ("list_emails", {"folder": "INBOX", "unread_only": True, "compact": True}, "ungelesene, kompakt"),
        ("get_email", {"folder": "INBOX", "uid": uid["mama-sonntag"]}, "eine Mail vollständig"),
        ("get_email", {"folder": "INBOX", "uid": uid.get("postfach-warnung", 0)},
         "Mail, deren Textteil mehr sagt als ihr HTML (body_parts_diverge)"),
        ("get_thread", {"folder": "INBOX", "uid": uid["jonas-zusage"], "include_body": False},
         "Thread mit der Antwort aus Gesendete Elemente, als Zusammenfassungen"),
        ("search_emails", {"from": "Stadtwerke"}, "Absender über alle Ordner"),
        ("search_emails", {"text": "Steuererklärung", "folder": "INBOX"}, "Volltext mit Umlaut"),
        ("search_emails", {"text": "Telekom-Rechnung"}, "Wort mit Bindestrich, als seine Teile gesucht"),
        ("search_emails", {"since": yesterday, "is_read": False, "folder": "INBOX"}, "ungelesen seit gestern"),
        ("download_attachment", {"folder": "INBOX", "uid": uid["stadtwerke-oktober"], "index": 0},
         "PDF-Anhang speichern"),
        ("list_drafts", {}, "Entwürfe"),
        ("draft_reply", {"folder": "INBOX", "uid": uid["mama-sonntag"],
                         "body": "Hallo Mama,\n\nwir kommen gern, bis Sonntag!"}, "Antwortentwurf"),
        ("draft_forward", {"folder": "INBOX", "uid": uid["dhl-paket"], "to": ["ben@example.de"],
                           "body": "Ben, das Paket kommt heute Nachmittag."}, "Weiterleitung als Entwurf"),
        ("draft_email", {"to": ["s.kaya@steuer.example"], "subject": "Unterlagen",
                         "body": "Hallo Frau Kaya,\n\nanbei die Lohnsteuerbescheinigung 2025.",
                         "attachments": [str(documents / "Lohnsteuerbescheinigung-2025.pdf")],
                         "replaces_uid": uid["entwurf-kaya"]},
         "neuer Entwurf mit Anhang, ersetzt den angefangenen"),
        ("delete_draft", {"uids": ["<draft_reply>"]}, "den Antwortentwurf wieder löschen"),
        ("mark_as_read", {"folder": "INBOX", "uids": [uid["dhl-paket"]]}, "als gelesen markieren"),
        ("mark_as_unread", {"folder": "INBOX", "uids": [uid["dhl-paket"]]}, "wieder ungelesen"),
        ("flag_email", {"folder": "INBOX", "uids": [uid["stadtwerke-oktober"]]}, "markieren"),
        ("unflag_email", {"folder": "INBOX", "uids": [uid["stadtwerke-oktober"]]}, "Markierung entfernen"),
        ("move_email", {"folder": "INBOX", "uids": [uid["stadtwerke-oktober"]], "target_folder": "Rechnungen",
                        "dry_run": True}, "Vorschau"),
        ("move_email", {"folder": "INBOX", "uids": [uid["stadtwerke-oktober"]], "target_folder": "Rechnungen"},
         "verschieben"),
        ("delete_email", {"folder": "INBOX", "uids": [uid["radhaus-newsletter"]], "dry_run": True}, "Vorschau"),
        ("delete_email", {"folder": "INBOX", "uids": [uid["radhaus-newsletter"]]}, "in den Papierkorb"),
        # What failures look like. Otomo puts "Error of the tool: " in front.
        ("get_email", {"folder": "INBOX", "uid": 999}, "Fehler: unbekannte UID"),
        ("list_emails", {"folder": "Archiv"}, "Fehler: Ordnername, den es nicht gibt"),
        # Names as people say them resolve to the wire name (since the folder
        # resolution): a role word and the readable form of an encoded name.
        ("list_emails", {"folder": "Papierkorb", "limit": 3}, "Rollenwort statt Ordnername, aufgelöst"),
        ("list_emails", {"folder": "Gelöschte Elemente", "limit": 3},
         "lesbarer Name statt modified UTF-7, aufgelöst"),
        ("move_email", {"folder": "INBOX", "uids": [uid["mama-sonntag"]], "target_folder": "INBOX"},
         "Fehler: Ziel gleich Quelle"),
        ("delete_email", {"folder": welt.TRASH, "uids": [1]}, "Fehler: aus dem Papierkorb nur permanent"),
        ("search_emails", {}, "Fehler: kein Kriterium"),
    ]


def text_of(result):
    return "".join(block.get("text", "") for block in result.get("content", []) if block.get("type") == "text")


def pretty(text):
    try:
        return json.dumps(json.loads(text), ensure_ascii=False, indent=2)
    except ValueError:
        return text


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--binary", default=str(REPO / "target" / "release" / "imap-mcp-rs"))
    parser.add_argument("--config", default=str(welt.RUN / "config.toml"))
    args = parser.parse_args()

    welt.cmd_reset(types.SimpleNamespace(minimal=False))
    manifest = json.loads((welt.RUN / "manifest.json").read_text())
    uid = {m["key"]: m["uid"] for m in manifest["accounts"]["Privat"]["messages"]}

    server = Server([args.binary, "--config", args.config])
    init = server.call("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                      "clientInfo": {"name": "gemma-dump", "version": "0"}})
    server.notify("notifications/initialized")
    tools = server.call("tools/list")["tools"]

    calls = []
    for tool, arguments, purpose in examples(uid, welt.RUN / "dokumente"):
        if arguments.get("uids") == ["<draft_reply>"]:
            arguments = {"uids": [next(c["uid_saved"] for c in calls if c["tool"] == "draft_reply")]}
        result = server.call("tools/call", {"name": tool, "arguments": arguments})
        text = text_of(result)
        entry = {"tool": tool, "purpose": purpose, "arguments": arguments,
                 "isError": result.get("isError", False), "text": text, "bytes": len(text.encode())}
        if tool == "draft_reply" and not entry["isError"]:
            entry["uid_saved"] = json.loads(text).get("uid")
        calls.append(entry)
    server.close()
    welt.cmd_reset(types.SimpleNamespace(minimal=False))

    instructions = init.get("instructions", "")
    sizes = {t["name"]: len(json.dumps(t, ensure_ascii=False).encode()) for t in tools}
    dump = {
        "created": datetime.datetime.now().astimezone().isoformat(),
        "binary": args.binary,
        "server": init.get("serverInfo"),
        "protocol": init.get("protocolVersion"),
        "instructions": instructions,
        "instructions_chars": len(instructions),
        "tools_list_bytes": len(json.dumps(tools, ensure_ascii=False).encode()),
        "tool_bytes": sizes,
        "tools": tools,
        "calls": calls,
    }
    name = Path(args.config).stem.replace("config", "dump", 1)
    (welt.RUN / f"{name}.json").write_text(json.dumps(dump, ensure_ascii=False, indent=2) + "\n")

    out = [f"# Dump von imap-mcp-rs {init.get('serverInfo', {}).get('version', '?')}", "",
           f"Erstellt {dump['created']} gegen die Welt aus welt.py, Protokoll {dump['protocol']}.", "",
           f"- tools/list: {dump['tools_list_bytes']} Bytes für {len(tools)} Werkzeuge",
           f"- Instructions: {len(instructions)} Zeichen", "",
           "| Werkzeug | Bytes | Beschreibung (Zeichen) |", "|---|---:|---:|"]
    for tool in sorted(tools, key=lambda t: -sizes[t["name"]]):
        out.append(f"| {tool['name']} | {sizes[tool['name']]} | {len(tool.get('description', ''))} |")
    out += ["", "## Instructions", "", "```", instructions, "```", ""]
    for tool in tools:
        out += [f"## {tool['name']}", "", tool.get("description", ""), "", "inputSchema:", "", "```json",
                json.dumps(tool.get("inputSchema", {}), ensure_ascii=False, indent=2), "```", ""]
        for call in (c for c in calls if c["tool"] == tool["name"]):
            flag = "isError: true" if call["isError"] else "ok"
            out += [f"### {call['purpose']} ({flag}, {call['bytes']} Bytes)", "", "```json",
                    json.dumps(call["arguments"], ensure_ascii=False), "```", "", "```json",
                    pretty(call["text"]), "```", ""]
    (welt.RUN / f"{name}.md").write_text("\n".join(out))
    errors = [c for c in calls if c["isError"]]
    print(f"{welt.RUN / (name + '.json')}\n{welt.RUN / (name + '.md')}\n{len(calls)} Aufrufe, davon {len(errors)} "
          "Fehler: " + ", ".join(f"{c['tool']}" for c in errors))


if __name__ == "__main__":
    sys.exit(main())
