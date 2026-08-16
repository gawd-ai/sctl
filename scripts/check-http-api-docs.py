#!/usr/bin/env python3
"""Drift gate: docs/http-api.md must match the registered HTTP routes.

Extracts every `.route("...", method(handler))` registration from
server/src/main.rs and server/src/tunnel/relay.rs, extracts every
### `METHOD /path` heading from docs/http-api.md, and fails listing any
route registered-but-undocumented or documented-but-unregistered.

Relay rule: a `/d/{serial}/api/X` wrapper is documented by the `/api/X`
device section (relay access is the same path prefixed, per the docs'
"Relay access" section), unless it has its own heading.
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SOURCES = [ROOT / "server/src/main.rs", ROOT / "server/src/tunnel/relay.rs"]
DOC = ROOT / "docs/http-api.md"

ALLOW = {
    # Generic relay passthrough: documented in prose ("Relay access to device
    # APIs") — it has no fixed method/path of its own to head a section with.
    ("ANY", "/d/{serial}/api/{*rest}"),
}

ROUTE_RE = re.compile(r'\.route\(\s*"([^"]+)"')
METHOD_RE = re.compile(r"\b(get|post|put|delete|patch|any)\(")
HEADING_RE = re.compile(r"^### `([A-Z]+) (\S+)`\s*$")


def registered() -> set[tuple[str, str]]:
    pairs = set()
    for src in SOURCES:
        text = src.read_text()
        matches = list(ROUTE_RE.finditer(text))
        for i, m in enumerate(matches):
            # Method tokens live between this path and the next .route() —
            # capped so a trailing route can't swallow handler-body code.
            end = matches[i + 1].start() if i + 1 < len(matches) else len(text)
            window = text[m.end() : min(end, m.end() + 400)]
            for method in METHOD_RE.findall(window):
                pairs.add((method.upper(), m.group(1)))
    return pairs


def documented() -> set[tuple[str, str]]:
    pairs = set()
    for line in DOC.read_text().splitlines():
        if h := HEADING_RE.match(line):
            pairs.add((h.group(1), h.group(2)))
    return pairs


def main() -> int:
    reg, doc = registered(), documented()
    undocumented = sorted(
        (m, p)
        for m, p in reg - ALLOW - doc
        if not (p.startswith("/d/{serial}") and (m, p.removeprefix("/d/{serial}")) in doc)
    )
    phantom = sorted(doc - reg - ALLOW)
    for m, p in undocumented:
        print(f"registered but undocumented: {m} {p}  (add: ### `{m} {p}`)")
    for m, p in phantom:
        print(f"documented but unregistered: {m} {p}  (stale heading in {DOC.name})")
    if undocumented or phantom:
        return 1
    print(f"http-api docs gate OK: {len(reg)} registered routes, {len(doc)} documented sections")
    return 0


if __name__ == "__main__":
    sys.exit(main())
