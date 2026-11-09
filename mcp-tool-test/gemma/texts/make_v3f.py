#!/usr/bin/env python3
"""Variant 3f: variant 3c with the email in focus (the one just read or found alone): mark, flag, move, delete
and reply or forward without uids act on it, for follow-ups such as "Markier die Mail als wichtig" after "Wann
kommt mein Paket?", where Otomo's history keeps no UID. Writes texts/v3f.json."""
import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
texts = json.loads((HERE / "v3c.json").read_text())
texts["emulate"]["focus"] = True
UIDS = ("UIDs of the emails, from search_emails or list_emails. Leave out, with folder, for the email just read or "
        "found alone: the one just talked about.")
FOLDER = "The emails' folder (INBOX is the inbox); left out with uids for the email just talked about."
for name in ("mark_as_read", "mark_as_unread", "flag_email", "unflag_email", "delete_email", "move_email"):
    spec = texts["tools"].setdefault(name, {})
    spec.setdefault("params", {}).update({"uids": UIDS, "folder": FOLDER})
    spec["required"] = ["target_folder"] if name == "move_email" else []
for name, required in (("draft_reply", ["body"]), ("draft_forward", ["to"])):
    spec = texts["tools"][name]
    spec["params"].update({"uid": "UID of the email; leave out, with folder, for the email just read or found "
                                  "alone: the one just talked about.",
                           "folder": "The email's folder; left out with uid for the email just talked about."})
    spec["required"] = required
(HERE / "v3f.json").write_text(json.dumps(texts, ensure_ascii=False, indent=1))
print("ok")
