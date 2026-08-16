#!/usr/bin/env python3
"""Drift gate: docs/config.md and server/sctl.toml.example must cover config.rs.

Extracts every `pub NAME:` field from every `pub struct ...` block in
server/src/config.rs and fails listing any field that is missing from
docs/config.md or from server/sctl.toml.example.

Coverage rules:
- docs/config.md documents a leaf field as a table row starting `| `name` |`,
  and a section/sub-table field (a field of `Config`, or a nested struct
  field like `usb_cycle_evidence`) as a heading containing `[name]` /
  `[parent.name]` — or as a table row, either counts.
- sctl.toml.example carries a leaf field as a `name = value` line (commented
  out is fine — that is the convention for optional keys) and a
  section/sub-table field as a `[name]` / `[parent.name]` header line
  (commented out is fine).
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "server/src/config.rs"
DOC = ROOT / "docs/config.md"
EXAMPLE = ROOT / "server/sctl.toml.example"

# (struct, field) pairs exempt from the docs/example requirement, each with a
# justification. Genuinely internal fields only — everything an operator can
# set belongs in both files.
ALLOW: set[tuple[str, str]] = set()

STRUCT_RE = re.compile(r"^pub struct (\w+) \{$", re.M)
FIELD_RE = re.compile(r"^\s*pub (\w+):", re.M)


def fields() -> list[tuple[str, str]]:
    text = SOURCE.read_text()
    out = []
    for m in STRUCT_RE.finditer(text):
        body = text[m.end() : text.index("\n}", m.end())]
        out.extend((m.group(1), f.group(1)) for f in FIELD_RE.finditer(body))
    return out


def in_doc(doc: str, name: str) -> bool:
    return bool(
        re.search(rf"^\| `{name}` \|", doc, re.M)
        or re.search(rf"`\[(?:[\w.]+\.)?{name}\]`", doc)
    )


def in_example(toml: str, name: str) -> bool:
    return bool(
        re.search(rf"^\s*#?\s*{name}\s*=", toml, re.M)
        or re.search(rf"^\s*#?\s*\[(?:[\w.]+\.)?{name}\]", toml, re.M)
    )


def main() -> int:
    doc, toml = DOC.read_text(), EXAMPLE.read_text()
    missing_doc, missing_example = [], []
    checked = 0
    for struct, name in fields():
        if (struct, name) in ALLOW:
            continue
        checked += 1
        if not in_doc(doc, name):
            missing_doc.append((struct, name))
        if not in_example(toml, name):
            missing_example.append((struct, name))
    for struct, name in missing_doc:
        print(f"{struct}.{name}: not documented in {DOC.name} (add a `| `{name}` |` table row)")
    for struct, name in missing_example:
        print(f"{struct}.{name}: missing from {EXAMPLE.name} (add `{name} = <default>`, commented out if optional)")
    if missing_doc or missing_example:
        return 1
    print(f"config docs gate OK: {checked} fields covered in {DOC.name} and {EXAMPLE.name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
