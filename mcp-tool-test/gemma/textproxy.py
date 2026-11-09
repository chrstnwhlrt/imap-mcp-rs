#!/usr/bin/env python3
"""The MCP server between the assistant and imap-mcp-rs in the measurement.
Every message passes through; the texts of TEXTS.json take the place of the
server's, and every tool call is kept with its answer in CALLS.jsonl, under
the number of the model request it came in (TURNS, which proxy.py counts).

    textproxy.py TEXTS.json CALLS.jsonl TURNS -- COMMAND [ARGS...]

TEXTS.json, every part optional:
  {"instructions": str,
   "flatten": true,               # $ref resolved, a oneOf of constants as a plain enum
   "strip_schema": true,          # $schema, format, minimum and the "null" of optional types left out
   "hide_everywhere": [param],    # left out of every tool's schema (the server takes its default)
   "hide_tools": [name],          # left out of the list
   "tools": {name: {"name": shown name, "title": str, "description": str,
                    "params": {param: description},
                    "rename": {shown param: real param},
                    "hide": [param, ...],          # left out of the schema
                    "required": [param, ...]}},    # in place of the server's list
   "errors": [[regex, replacement], ...]}          # applied to the text of every error
A tool shown under another name is called by its own, a renamed parameter
by its own; a hidden one is left to the server's default.

"emulate" tries a change of the server before it has it:
  {"split_terms": true}   search_emails: a text or subject of several words
      searches for all of its words of four letters or more (text_all,
      subject_all), as one term each
  {"split_terms": "stopwords"}   the same by the server's rule: "phrases"
      stay whole, punctuation off the ends of words, stop words out unless
      nothing else is left
  {"newest_on_empty": 10}   an empty search result lists the newest emails
      of INBOX (uid, from, subject, date) in the field newest
  {"folders_on_missing": true}   the error for a folder that does not exist
      names the folders (display name in brackets) and the way to the trash
  {"focus": true}   the server keeps the email last read (get_email) or found
      alone (a search or list with one row); mark, flag, move, delete and
      draft_reply/draft_forward without uids (uid) act on it, and without one
      the call is refused with what to do
  {"folder_aliases": true}   folder and target_folder also take a folder's
      display name ("Entwürfe") and, in any case, the name of its role
      (drafts, sent, trash, inbox, in English or German)
  {"empty_hint": {tool: note}}   a result of that tool with nothing in it
      (returned 0) gets the field note
  {"local_dates": "Europe/Berlin"}   every date and internal_date of a result
      in that zone with its offset, date_original left out
"""
import copy
import datetime
import json
import re
import subprocess
import sys
import threading
from zoneinfo import ZoneInfo

TEXTS, CALLS, TURNS = sys.argv[1:4]
COMMAND = sys.argv[sys.argv.index("--") + 1:]
texts = json.loads(open(TEXTS).read() or "{}")
shown_to_real = {spec.get("name", name): name for name, spec in texts.get("tools", {}).items()}
emulate = texts.get("emulate", {})
focus = {"email": None}
FOCUS_MANY = {"mark_as_read", "mark_as_unread", "flag_email", "unflag_email", "move_email", "delete_email"}
FOCUS_ONE = {"draft_reply", "draft_forward"}
NO_FOCUS = ("No email is in focus: find it first (search_emails or list_emails, or get_email), or give folder "
            "and uids.")
pending = {}
LOCK = threading.Lock()
WRITE = threading.Lock()


def turn():
    try:
        return int(open(TURNS).read().split()[0])
    except (OSError, ValueError, IndexError):
        return 0


def resolve(node, defs):
    """`node` with every local $ref replaced by its definition, and a oneOf
    of constants as a plain enum whose values the description names."""
    if isinstance(node, list):
        return [resolve(n, defs) for n in node]
    if not isinstance(node, dict):
        return node
    if "$ref" in node:
        target = copy.deepcopy(defs[node["$ref"].rsplit("/", 1)[-1]])
        node = {**{k: v for k, v in node.items() if k != "$ref"}, **target}
    node = {k: resolve(v, defs) for k, v in node.items() if k != "$defs"}
    if "oneOf" in node and all(isinstance(v, dict) and "const" in v for v in node["oneOf"]):
        values = node.pop("oneOf")
        node["type"] = "string"
        node["enum"] = [v["const"] for v in values]
        named = "; ".join(f"{v['const']}: {v['description']}" for v in values if v.get("description"))
        node["description"] = " ".join(filter(None, [node.get("description"), named]))
    return node


def stripped(node):
    """`node` without what changes nothing for the caller: $schema, format, minimum, and "null" beside the type of
    a field that is not required anyway."""
    if isinstance(node, list):
        return [stripped(n) for n in node]
    if not isinstance(node, dict):
        return node
    node = {k: stripped(v) for k, v in node.items() if k not in ("$schema", "format", "minimum")}
    if isinstance(node.get("type"), list) and "null" in node["type"]:
        rest = [t for t in node["type"] if t != "null"]
        node["type"] = rest[0] if len(rest) == 1 else rest
    return node


STOPWORDS = set("""der die das dem den des ein eine einem einen einer eines und oder vom von mit für im in am an auf aus
bei zu zum zur über nach um the a an of and or to for on in at from by with about mail mails email emails e-mail
e-mails nachricht nachrichten message messages""".split())
EDGES = ".,;:!?()[]{}'\""


def pieces(value):
    """The server's terms of a text or subject: "phrases" whole, the other words without punctuation at their
    ends and without stop words, unless nothing else is left."""
    phrases = [p for p in re.findall(r'"([^"]*)"', value) if p.strip()]
    words = [w.strip(EDGES) for w in re.sub(r'"[^"]*"', " ", value).split()]
    words = [w for w in words if w]
    kept = [w for w in words if w.lower() not in STOPWORDS]
    if not kept and not phrases:
        kept = words
    return phrases + kept


def split_terms(given, rule):
    """A text or subject of several words as all of its terms (text_all, subject_all)."""
    for single, every in (("text", "text_all"), ("subject", "subject_all")):
        value = given.get(single)
        if not isinstance(value, str):
            continue
        if rule == "stopwords":
            terms = pieces(value)
        elif len(value.split()) > 1 and not given.get(every):
            terms = [w for w in (w.strip(".,;:!?\"'") for w in value.split()) if len(w) >= 4]
        else:
            continue
        if not terms:
            continue
        given.pop(single)
        if len(terms) == 1 and not given.get(every):
            given[single] = terms[0]
        else:
            given[every] = list(given.get(every) or []) + terms


def utf7_name(name):
    """A folder name of IMAP's modified UTF-7 as it reads."""
    import base64

    def decoded(match):
        if not match.group(1):
            return "&"
        raw = match.group(1).replace(",", "/")
        return base64.b64decode(raw + "=" * (-len(raw) % 4)).decode("utf-16-be")
    return re.sub(r"&([^-]*)-", decoded, name)


ROLE_WORDS = {
    "drafts": {"drafts", "draft", "entwurf", "entwürfe", "entwuerfe"},
    "sent": {"sent", "sent items", "sent mail", "gesendet", "gesendete", "gesendete elemente", "gesendete objekte"},
    "trash": {"trash", "deleted", "deleted items", "papierkorb", "gelöscht", "gelöschte elemente", "mülleimer"},
    "inbox": {"inbox", "posteingang", "eingang"},
}
# The world's folders of each role, as list_folders tells them.
ROLE_FOLDERS = {"drafts": "Entw&APw-rfe", "sent": "Gesendete Elemente", "trash": "Gel&APY-schte Elemente",
                "inbox": "INBOX"}


def folder_named(value):
    """The folder a model means: its wire name, its display name in any case, or the name of its role."""
    if not isinstance(value, str):
        return value
    wanted = value.strip().lower()
    for wire in ROLE_FOLDERS.values():
        if wire.lower() == wanted or utf7_name(wire).lower() == wanted:
            return wire
    for role, words in ROLE_WORDS.items():
        if wanted in words:
            return ROLE_FOLDERS[role]
    if wanted == "rechnungen":
        return "Rechnungen"
    return value


def folder_error(name):
    """The server's error for a folder that does not exist, with the folders of the mailbox."""
    import os
    sys.path.insert(0, os.environ.get("IMAP_WORLD", "."))
    import welt
    imap = welt.connect("christian")
    try:
        names = welt.folder_names(imap)
    finally:
        imap.logout()
    names = ["INBOX"] + sorted(n for n in names if n != "INBOX")
    shown = [n if utf7_name(n) == n else f"{n} ({utf7_name(n)})" for n in names[:20]]
    more = f" ... and {len(names) - 20} more, see list_folders" if len(names) > 20 else ""
    return (f'Folder "{name}" does not exist. Folders: {", ".join(shown)}{more}. '
            + emulate.get("folder_error_end", "To delete, use delete_email; it moves to the trash."))


def newest(count):
    """The newest emails of INBOX as the server would list them on an empty search: read from the world."""
    import os
    sys.path.insert(0, os.environ.get("IMAP_WORLD", "."))
    import email
    from email import policy
    import welt
    imap = welt.connect("christian")
    try:
        imap.select("INBOX", readonly=True)
        typ, data = imap.uid("SEARCH", None, "ALL")
        uids = (data[0] or b"").split()[-count:]
        rows = []
        for uid in reversed(uids):
            typ, parts = imap.uid("FETCH", uid.decode(), "(BODY.PEEK[HEADER.FIELDS (FROM SUBJECT DATE)])")
            raw = next(part for part in parts if isinstance(part, tuple))[1]
            message = email.message_from_bytes(raw, policy=policy.default)
            rows.append({"uid": int(uid), "folder": "INBOX", "from": str(message.get("From") or ""),
                         "subject": str(message.get("Subject") or ""), "date": str(message.get("Date") or "")})
        return rows
    finally:
        imap.logout()


def local_dates(node, zone):
    if isinstance(node, list):
        return [local_dates(n, zone) for n in node]
    if not isinstance(node, dict):
        return node
    out = {}
    for key, value in node.items():
        if key == "date_original":
            continue
        if key in ("date", "internal_date") and isinstance(value, str) and value.endswith("Z"):
            moment = datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))
            value = moment.astimezone(zone).isoformat()
        out[key] = local_dates(value, zone)
    return out


def shown_tools(tools):
    out = []
    hidden = set(texts.get("hide_tools", []))
    everywhere = texts.get("hide_everywhere", [])
    for tool in tools:
        if tool["name"] in hidden:
            continue
        spec = texts.get("tools", {}).get(tool["name"], {})
        tool = copy.deepcopy(tool)
        schema = tool.get("inputSchema", {})
        if texts.get("flatten"):
            schema = resolve(schema, schema.get("$defs", {}))
        if texts.get("strip_schema"):
            schema = stripped(schema)
        props = schema.get("properties", {})
        for param in spec.get("hide", []) + everywhere:
            props.pop(param, None)
            if param in schema.get("required", []):
                schema["required"].remove(param)
        for shown, real in spec.get("rename", {}).items():
            if real in props:
                props[shown] = props.pop(real)
                schema["required"] = [shown if r == real else r for r in schema.get("required", [])]
        for param, description in spec.get("params", {}).items():
            if param in props:
                props[param]["description"] = description
        if "required" in spec:
            schema["required"] = spec["required"]
        tool["inputSchema"] = schema
        for key in ("description", "title"):
            if key in spec:
                tool[key] = spec[key]
        tool["name"] = spec.get("name", tool["name"])
        out.append(tool)
    return out


def from_client(child):
    for line in sys.stdin:
        try:
            message = json.loads(line)
        except ValueError:
            child.stdin.write(line)
            child.stdin.flush()
            continue
        if "id" in message and "method" in message:
            entry = {"method": message["method"]}
            if message["method"] == "tools/call":
                params = message.setdefault("params", {})
                shown = params.get("name")
                real = shown_to_real.get(shown, shown)
                arguments = dict(params.get("arguments") or {})
                rename = texts.get("tools", {}).get(real, {}).get("rename", {})
                params["name"] = real
                params["arguments"] = {rename.get(k, k): v for k, v in arguments.items()}
                if emulate.get("split_terms") and real == "search_emails":
                    split_terms(params["arguments"], emulate["split_terms"])
                if emulate.get("focus") and real in FOCUS_MANY | FOCUS_ONE:
                    given = params["arguments"]
                    key = "uids" if real in FOCUS_MANY else "uid"
                    if not given.get(key):
                        with LOCK:
                            held = focus["email"]
                        if held is None:
                            entry.update(turn=turn(), name=real, shown=shown, arguments=arguments, sent=dict(given))
                            reply = {"jsonrpc": "2.0", "id": message["id"], "result": {
                                "content": [{"type": "text", "text": json.dumps(
                                    {"error": NO_FOCUS, "retryable": False})}], "isError": True}}
                            with open(CALLS, "a") as f:
                                f.write(json.dumps({**{k: entry[k] for k in ("turn", "name", "shown", "arguments", "sent")},
                                                    "error": True, "text": NO_FOCUS}, ensure_ascii=False) + "\n")
                            send(reply)
                            continue
                        given["folder"] = held[0]
                        given[key] = [held[1]] if key == "uids" else held[1]
                if emulate.get("folder_aliases"):
                    for key in ("folder", "target_folder"):
                        if key in params["arguments"]:
                            params["arguments"][key] = folder_named(params["arguments"][key])
                entry.update(turn=turn(), name=real, shown=shown, arguments=arguments,
                             sent=dict(params["arguments"]))
            with LOCK:
                pending[message["id"]] = entry
        child.stdin.write(json.dumps(message, ensure_ascii=False) + "\n")
        child.stdin.flush()
    child.stdin.close()


def to_client(child):
    for line in child.stdout:
        try:
            message = json.loads(line)
        except ValueError:
            sys.stdout.write(line)
            sys.stdout.flush()
            continue
        with LOCK:
            entry = pending.pop(message.get("id"), None) if "id" in message and "method" not in message else None
        result = message.get("result")
        if entry and isinstance(result, dict):
            if entry["method"] == "tools/list" and "tools" in result:
                result["tools"] = shown_tools(result["tools"])
            elif entry["method"] in ("server/discover", "initialize") and texts.get("instructions") is not None:
                result["instructions"] = texts["instructions"]
        if entry and entry["method"] == "tools/call":
            content = (result or {}).get("content") or []
            if result and not result.get("isError") and content and isinstance(content[0], dict):
                try:
                    shown_result = json.loads(content[0].get("text", ""))
                except ValueError:
                    shown_result = None
                if isinstance(shown_result, dict):
                    changed = False
                    if emulate.get("local_dates"):
                        shown_result = local_dates(shown_result, ZoneInfo(emulate["local_dates"]))
                        changed = True
                    hint = emulate.get("empty_hint", {}).get(entry["name"])
                    if hint and shown_result.get("returned") == 0:
                        shown_result["hint"] = hint
                        changed = True
                    if (emulate.get("newest_on_empty") and entry["name"] == "search_emails"
                            and shown_result.get("returned") == 0):
                        shown_result["newest"] = newest(emulate["newest_on_empty"])
                        changed = True
                    if emulate.get("focus"):
                        with LOCK:
                            if entry["name"] == "get_email" and isinstance(shown_result.get("email"), dict):
                                focus["email"] = (shown_result["email"].get("folder"), shown_result["email"].get("uid"))
                            elif entry["name"] in ("search_emails", "list_emails"):
                                rows = shown_result.get("emails") or []
                                focus["email"] = (rows[0].get("folder"), rows[0].get("uid")) if len(rows) == 1 else None
                    if changed:
                        content[0]["text"] = json.dumps(shown_result, ensure_ascii=False, separators=(",", ":"))
                        if isinstance(result.get("structuredContent"), dict):
                            result["structuredContent"] = shown_result
            if (result or {}).get("isError") and emulate.get("folders_on_missing") and content:
                try:
                    error = json.loads(content[0].get("text", ""))
                except ValueError:
                    error = None
                missing = re.match(r"Mailbox doesn't exist: ([^\n]*?)(?: \([\d.+ ]+secs\))?\.?$",
                                   (error or {}).get("error", "")) if isinstance(error, dict) else None
                if missing:
                    error["error"] = folder_error(missing.group(1))
                    content[0]["text"] = json.dumps(error, ensure_ascii=False, separators=(",", ":"))
            if (result or {}).get("isError"):
                for block in content:
                    if isinstance(block, dict) and "text" in block:
                        for pattern, replacement in texts.get("errors", []):
                            block["text"] = re.sub(pattern, replacement, block["text"])
            row = {k: entry[k] for k in ("turn", "name", "shown", "arguments", "sent")}
            row.update(error=bool((result or {}).get("isError")) or "error" in message,
                       text=" ".join(c.get("text", "") for c in content if isinstance(c, dict)) or
                       json.dumps(message.get("error"), ensure_ascii=False))
            with open(CALLS, "a") as f:
                f.write(json.dumps(row, ensure_ascii=False) + "\n")
        send(message)


def send(message):
    with WRITE:
        sys.stdout.write(json.dumps(message, ensure_ascii=False) + "\n")
        sys.stdout.flush()


def main():
    child = subprocess.Popen(COMMAND, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
    reader = threading.Thread(target=from_client, args=(child,), daemon=True)
    reader.start()
    to_client(child)
    child.wait()


main()
