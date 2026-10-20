"""Changelog fragments: one file per change under changelog.d/, merged into CHANGELOG.md only at release time.

Usage:
  python tools/changelog.py new <subject> <kind> ["- bullet" ...]   create changelog.d/<subject>.<kind>.md
  python tools/changelog.py assemble                                merge the fragments under [Unreleased], empty the folder
  python tools/changelog.py stamp                                   re-record the [Unreleased] hash (after cutting a release)
  python tools/changelog.py check                                   fragments well formed, [Unreleased] untouched by hand

Fragment: `changelog.d/<subject>.<kind>.md`, kind in security|added|changed|fixed|tests, English, one to three bullets
(a bullet may continue on indented lines). Sections keep the order Security, Added, Changed, Fixed, Tests.
`.unreleased.sha256` holds the hash of the [Unreleased] text as the tool last wrote it: a direct edit of that
section breaks `check` (tests/test_changelog.py), so concurrent branches only ever add files and never conflict.
"""
from __future__ import annotations

import hashlib
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CHANGELOG = ROOT / "CHANGELOG.md"
FRAGMENTS = ROOT / "changelog.d"
STAMP = FRAGMENTS / ".unreleased.sha256"
KINDS = ["security", "added", "changed", "fixed", "tests"]
NAME = re.compile(r"[a-z0-9][a-z0-9_-]*\.(%s)\.md" % "|".join(KINDS))
HEAD = "## [Unreleased]"


def split_changelog(text: str) -> tuple[str, str, str]:
    """(before, [Unreleased] section text from its heading line, after)."""
    start = text.index(HEAD)
    nxt = re.search(r"^## \[", text[start + len(HEAD):], re.M)
    end = start + len(HEAD) + nxt.start() if nxt else len(text)
    return text[:start], text[start:end], text[end:]


def digest(section: str) -> str:
    return hashlib.sha256(section.strip().encode()).hexdigest()


def fragments() -> list[Path]:
    return sorted(p for p in FRAGMENTS.glob("*.md") if p.name != "README.md")


def problems() -> list[str]:
    out = []
    for p in fragments():
        if not NAME.fullmatch(p.name):
            out.append(f"{p.name}: name must be <subject>.<{'|'.join(KINDS)}>.md")
            continue
        lines = [ln for ln in p.read_text().splitlines() if ln.strip()]
        if not lines or not all(ln.startswith(("- ", "  ")) for ln in lines) or not lines[0].startswith("- "):
            out.append(f"{p.name}: must be bullets (`- ...`, continuation lines indented by two spaces)")
    return out


def sections(body: str) -> tuple[str, dict[str, str]]:
    parts = re.split(r"^### (.+)$", body, flags=re.M)
    return parts[0], {parts[i].strip(): parts[i + 1].strip("\n") for i in range(1, len(parts), 2)}


def assemble_text(text: str, frags: list[Path]) -> str:
    before, section, after = split_changelog(text)
    pre, secs = sections(section[len(HEAD):])
    for p in frags:
        kind = p.name.rsplit(".", 2)[1].capitalize()
        secs[kind] = (secs[kind] + "\n" if secs.get(kind) else "") + p.read_text().strip("\n")
    order = [k.capitalize() for k in KINDS]
    names = order + [k for k in secs if k not in order]
    body = "".join(f"\n### {k}\n\n{secs[k]}\n" for k in names if secs.get(k))
    return before + HEAD + pre.rstrip("\n") + "\n" + body + "\n" + after


def stamp(text: str) -> None:
    STAMP.write_text(digest(split_changelog(text)[1]) + "\n")


def unreleased_edited_by_hand(text: str) -> bool:
    return not STAMP.exists() or STAMP.read_text().strip() != digest(split_changelog(text)[1])


def main(argv: list[str]) -> int:
    cmd = argv[0] if argv else "check"
    text = CHANGELOG.read_text()
    if cmd == "new" and len(argv) >= 3:
        path = FRAGMENTS / f"{argv[1]}.{argv[2]}.md"
        if not NAME.fullmatch(path.name):
            print(f"bad name {path.name}", file=sys.stderr)
            return 2
        path.write_text("\n".join(argv[3:] or ["- "]) + "\n")
        print(path.relative_to(ROOT))
        return 0
    if cmd == "assemble":
        if bad := problems():
            print("\n".join(bad), file=sys.stderr)
            return 1
        if unreleased_edited_by_hand(text):
            print("[Unreleased] was edited by hand: move that text into fragments, then `stamp`", file=sys.stderr)
            return 1
        frags = fragments()
        new = assemble_text(text, frags)
        CHANGELOG.write_text(new)
        stamp(new)
        for p in frags:
            p.unlink()
        print(f"{len(frags)} fragment(s) merged")
        return 0
    if cmd == "stamp":
        stamp(text)
        return 0
    if cmd == "check":
        bad = problems()
        if unreleased_edited_by_hand(text):
            bad.append("CHANGELOG.md [Unreleased] edited directly: add a fragment in changelog.d/ "
                       "(tools/changelog.py new <subject> <kind>) and revert the edit")
        print("\n".join(bad), file=sys.stderr)
        return 1 if bad else 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
