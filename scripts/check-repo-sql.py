#!/usr/bin/env python3
"""Rejects raw SQL reads of the `repo` table outside crates/db.

Reads of repository rows on behalf of a user must apply the viewer's `RepoAccess`, which the
`RepoStore` methods in crates/db do. This scans `crates/**/*.rs` outside crates/db and fails on
`FROM repo` or `JOIN repo` (any case, any whitespace, `repo` as a whole word; `DELETE FROM repo`
is not a read). Integration tests (`crates/*/tests/`) are exempt; other files are exempt when
scripts/repo-sql-allow.txt lists them, or a directory holding them (an entry ending in `/`),
with the reason they need no access filter.

Usage: scripts/check-repo-sql.py   (from anywhere; exits 1 and lists the hits on failure)
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
ALLOW_FILE = ROOT / "scripts" / "repo-sql-allow.txt"
PATTERN = re.compile(r"\b(?:FROM|JOIN)\s+repo\b", re.IGNORECASE)
DELETE = re.compile(r"\bDELETE\s+$", re.IGNORECASE)


def load_allow_list():
    allowed = {}
    for n, line in enumerate(ALLOW_FILE.read_text().splitlines(), 1):
        path, _, reason = line.partition("#")
        path = path.strip()
        if not path:
            continue
        if not reason.strip():
            sys.exit(f"{ALLOW_FILE.relative_to(ROOT)}:{n}: {path} needs a `# reason`")
        allowed[path] = n
    return allowed


def main():
    allowed = load_allow_list()
    used = set()
    hits = []
    for file in sorted((ROOT / "crates").rglob("*.rs")):
        rel = file.relative_to(ROOT).as_posix()
        parts = rel.split("/")
        if parts[1] == "db" or (len(parts) > 3 and parts[2] == "tests"):
            continue
        text = file.read_text(encoding="utf-8", errors="replace")
        matches = [
            m for m in PATTERN.finditer(text) if not DELETE.search(text, 0, m.start())
        ]
        if not matches:
            continue
        entry = next(
            (a for a in allowed if a == rel or (a.endswith("/") and rel.startswith(a))), None
        )
        if entry:
            used.add(entry)
            continue
        for m in matches:
            line = text.count("\n", 0, m.start()) + 1
            hits.append(f"{rel}:{line}: {' '.join(m.group(0).split())}")
    allow_rel = ALLOW_FILE.relative_to(ROOT)
    stale = sorted(set(allowed) - used)
    for path in stale:
        print(f"{allow_rel}:{allowed[path]}: {path} has no reads of the repo table; remove it")
    if hits:
        print("raw reads of the repo table outside crates/db:")
        print("\n".join(f"  {h}" for h in hits))
        print(
            "Use a RepoStore method that takes the viewer's RepoAccess, or, for internal code\n"
            f"that acts for no user, add the file to {allow_rel} with a reason."
        )
    if hits or stale:
        sys.exit(1)


if __name__ == "__main__":
    main()
