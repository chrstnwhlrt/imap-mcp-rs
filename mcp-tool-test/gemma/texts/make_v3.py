#!/usr/bin/env python3
"""Variant 3: variant 2 with the server's error for a folder that does not exist (the folders named, the trash
through delete_email), the drafts named as the user knows them, and an empty search that says to use one of the
newest emails where it is the one meant. Writes texts/v3.json next to itself."""
import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
texts = json.loads((HERE / "v2.json").read_text())
texts["errors"] = [rule for rule in texts["errors"] if "doesn't exist" not in rule[0]]
texts["emulate"]["folders_on_missing"] = True
texts["emulate"]["empty_hint"]["search_emails"] = (
    "No match. Terms match as parts of words: try one distinctive word, or text instead of from (from sees only "
    "the sender's name and address). newest lists the newest emails of INBOX: where one of them is the one the "
    "user means, as for a person named by role or a word in another form, go on with it.")
tools = texts["tools"]
tools["list_drafts"]["description"] = ("Drafts the user has not sent yet (Entwürfe), newest first. To change one, read "
                                       "it with get_email and write it anew with replaces_uid: its uid.")
tools["draft_forward"]["description"] = tools["draft_forward"]["description"].replace(
    "(search_emails with from: a name gives an address)",
    "(search_emails with from: a name gives an address; a person named by role is among the senders of the "
    "newest emails)")
(HERE / "v3.json").write_text(json.dumps(texts, ensure_ascii=False, indent=1))
print("ok", len(texts["instructions"]))
