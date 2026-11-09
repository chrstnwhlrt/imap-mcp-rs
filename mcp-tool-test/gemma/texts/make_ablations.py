#!/usr/bin/env python3
"""Variant 3c minus one change each, to see what each one is worth (texts/a-*.json):
  a-instr      the server's own instructions (cut at 1000 characters by Otomo)
  a-descr      the server's own tool and parameter descriptions
  a-hide       every parameter shown, the schema as the server sends it
  a-split      text and subject as Gemma gives them (no rule for several words)
  a-newest     an empty search without the newest emails
  a-dates      dates in UTC as today
  a-errors     the server's own error texts
  a-dryrun     dry_run shown
  a-folders    the server's own error for a folder that does not exist
  a-aliases    folders only by their name on the wire
"""
import copy
import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
v2 = json.loads((HERE / "v3c.json").read_text())


def variant(name, change):
    texts = copy.deepcopy(v2)
    change(texts)
    (HERE / f"a-{name}.json").write_text(json.dumps(texts, ensure_ascii=False, indent=1))


variant("instr", lambda t: t.pop("instructions"))
variant("descr", lambda t: [spec.pop(k, None) for spec in t["tools"].values() for k in ("description", "params")])


def show_all(t):
    t.pop("strip_schema")
    t["hide_everywhere"] = []
    for spec in t["tools"].values():
        spec.pop("hide", None)


variant("hide", show_all)
variant("split", lambda t: t["emulate"].pop("split_terms"))
variant("newest", lambda t: t["emulate"].pop("newest_on_empty"))
variant("dates", lambda t: t["emulate"].pop("local_dates"))
variant("errors", lambda t: t.pop("errors"))
variant("dryrun", lambda t: t.__setitem__("hide_everywhere", ["account"]))
variant("folders", lambda t: [t["emulate"].pop("folders_on_missing"), t["emulate"].pop("folder_error_end")])
variant("aliases", lambda t: t["emulate"].pop("folder_aliases"))
print(sorted(p.name for p in HERE.glob("a-*.json")))
