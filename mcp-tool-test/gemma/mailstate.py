#!/usr/bin/env python3
"""The mailbox as a conversation left it: every folder with its messages
(UID, Message-ID, subject, sender, recipients, flags, attachments) and the
plain text of what the user wrote (drafts and sent mail). Read over
welt.py's own connection to the world, so it never reaches another server,
and with BODY.PEEK in read-only folders, so reading changes nothing.

    mailstate.py [LOGIN]    print the state as JSON (default login: christian)
"""
import email
import imaplib
import json
import os
import sys
from email import policy
from pathlib import Path

HERE = Path(__file__).resolve().parent
WORLD = Path(os.environ.get("IMAP_WORLD") or HERE)
sys.path.insert(0, str(WORLD))
import welt  # noqa: E402

# Folders whose messages the user wrote: their text is part of the state.
WRITTEN = ("Entw&APw-rfe", "Gesendete Elemente")


def text_of(message):
    body = message.get_body(preferencelist=("plain",))
    return body.get_content() if body is not None else ""


def read(login="christian"):
    imap = welt.connect(login)
    try:
        state = {}
        for folder in welt.folder_names(imap):
            typ, _ = imap.select(welt.quoted(folder), readonly=True)
            if typ != "OK":
                continue
            typ, data = imap.uid("SEARCH", None, "ALL")
            messages = []
            for uid in (data[0] or b"").split():
                typ, parts = imap.uid("FETCH", uid.decode(), "(FLAGS BODY.PEEK[])")
                meta, raw = next(part for part in parts if isinstance(part, tuple))
                message = email.message_from_bytes(raw, policy=policy.default)
                row = {"uid": int(uid), "message_id": str(message.get("Message-ID") or "").strip(),
                       "in_reply_to": str(message.get("In-Reply-To") or "").strip(),
                       "subject": str(message.get("Subject") or ""), "from": str(message.get("From") or ""),
                       "to": str(message.get("To") or ""), "cc": str(message.get("Cc") or ""),
                       "date": str(message.get("Date") or ""),
                       "flags": [f.decode() for f in imaplib.ParseFlags(meta)],
                       "attachments": [p.get_filename() for p in message.iter_attachments() if p.get_filename()]}
                if folder in WRITTEN:
                    row["text"] = text_of(message)
                messages.append(row)
            state[folder] = messages
        return state
    finally:
        imap.logout()


if __name__ == "__main__":
    print(json.dumps(read(*sys.argv[1:]), ensure_ascii=False, indent=1))
