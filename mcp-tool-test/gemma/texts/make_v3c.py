#!/usr/bin/env python3
"""Variant 3c: variant 3b with list_drafts shown again and folders found by display name or role (Gemma passes
"Entwürfe", "draft" or "Papierkorb" where the server wants Entw&APw-rfe). Writes texts/v3c.json."""
import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
texts = json.loads((HERE / "v3b.json").read_text())
texts.pop("hide_tools", None)
texts["emulate"]["folder_aliases"] = True
for name in ("draft_reply", "draft_email", "draft_forward"):
    texts["tools"][name]["params"]["replaces_uid"] = ("To change a draft the user already has: its uid from "
                                                      "list_drafts. The new version takes its place.")
(HERE / "v3c.json").write_text(json.dumps(texts, ensure_ascii=False, indent=1))
print("ok")
