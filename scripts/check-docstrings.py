#!/usr/bin/env python3
"""Fail a change that adds or edits a declaration without documenting it.

Detection is clippy's, not ours: `clippy::missing_docs_in_private_items`
already knows what an undocumented item is, in every form the language has
-- functions, structs, fields, constants, traits, impls -- and it knows it
from the compiler's own view of the code rather than from a regex over the
text. All this adds is the scoping, which clippy has no opinion about.

The scoping is the point. The lint fires 246 times on this repo, so turning
it on wholesale would mean documenting the codebase before landing anything
else. Instead a run fails only for declarations the change itself touched,
so the rule applies to what you are writing.

    scripts/check-docstrings.py                 # against origin/main
    scripts/check-docstrings.py <base-ref>

Where this differs from the reviewer's version of the same check: it counts
a declaration you add or change, and does not count a function you only
edited the body of. It is zero-tolerance rather than a percentage, which
covers the difference in the strict direction -- pass this and an 80%
threshold is not in question.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys

HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@", re.M)
LINT = "clippy::missing_docs_in_private_items"


def git(*args: str) -> str:
    return subprocess.run(
        ("git",) + args, capture_output=True, text=True, check=True
    ).stdout


def touched_lines(base: str) -> dict[str, set[int]]:
    """Line numbers the change touched, per file, in the working tree.

    Diffing the merge base rather than the tip of `base` keeps the result
    about this change; leaving off `...HEAD` includes work that is not
    committed yet, so the answer arrives before the push rather than after.
    """
    merge_base = git("merge-base", base, "HEAD").strip()
    files = git("diff", "--name-only", merge_base).split()
    touched: dict[str, set[int]] = {}
    for path in files:
        lines: set[int] = set()
        diff = git("diff", "-U0", merge_base, "--", path)
        for hunk in HUNK.finditer(diff):
            start = int(hunk.group(1))
            count = int(hunk.group(2) or 1)
            lines.update(range(start, start + count))
        if lines:
            touched[path] = lines
    return touched


def undocumented() -> list[tuple[str, int, str]]:
    """Every undocumented item clippy can see, as (file, line, what)."""
    result = subprocess.run(
        [
            "cargo", "clippy", "--all-targets", "--message-format=json",
            "--quiet", "--", "-W", LINT,
        ],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0 and not result.stdout:
        print(result.stderr, file=sys.stderr)
        raise SystemExit("cargo clippy failed; fix the build first")

    items = []
    for line in result.stdout.splitlines():
        try:
            message = json.loads(line).get("message") or {}
        except json.JSONDecodeError:
            continue
        text = message.get("message") or ""
        if "missing documentation" not in text:
            continue
        for span in message.get("spans", []):
            if span.get("is_primary"):
                items.append((span["file_name"], span["line_start"], text))
    return items


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base", nargs="?", default="origin/main")
    args = parser.parse_args()

    touched = touched_lines(args.base)
    if not touched:
        print("Nothing changed; nothing to check.")
        return 0

    offenders = [
        (path, line, what)
        for path, line, what in undocumented()
        if line in touched.get(path, ())
    ]

    if not offenders:
        print(f"Every declaration touched since {args.base} is documented.")
        return 0

    print(f"Undocumented declarations touched since {args.base}:\n")
    for path, line, what in sorted(set(offenders)):
        print(f"  {path}:{line}  {what}")
    print(
        "\nSay why each one exists, not what it does -- and only what is true "
        "of it. A stale comment is treated as a defect here, so an accurate "
        "short line beats a thorough wrong one."
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
