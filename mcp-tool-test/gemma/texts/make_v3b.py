#!/usr/bin/env python3
"""Variant 3b: variant 3 with the folder error that says what to call for the trash, drafts reached through
list_emails on their folder (list_drafts left out: Gemma calls it list_draft or list_draft_emails, and Otomo's
closest names then miss it), and addresses looked up before the user is asked. Writes texts/v3b.json."""
import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
texts = json.loads((HERE / "v3.json").read_text())
texts["emulate"]["folder_error_end"] = ("To move emails to the trash, call delete_email with the same folder and "
                                        "uids; it puts them there.")
texts["hide_tools"] = ["list_drafts"]
tools = texts["tools"]
for name in ("draft_reply", "draft_email", "draft_forward"):
    tools[name]["params"]["replaces_uid"] = ("To change a draft the user already has: its uid in the drafts folder "
                                            "(list_emails on the folder with role drafts from list_folders). The "
                                            "new version takes its place.")
for name in ("draft_email", "draft_forward"):
    tools[name]["params"]["to"] = ("Email addresses, one per entry, such as \"ben@example.de\" or \"Ben "
                                   "<ben@example.de>\". Look an address up before asking the user: search_emails "
                                   "from: the name; for a person named by role, list_emails INBOX shows who wrote.")
(HERE / "v3b.json").write_text(json.dumps(texts, ensure_ascii=False, indent=1))
print("ok")
