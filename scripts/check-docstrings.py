#!/usr/bin/env python3
"""Fail a change that leaves the functions it touched undocumented.

This mirrors the docstring-coverage gate our PR reviewer applies, so the
number turns up while the change is still on your machine rather than as a
review finding after it is pushed. Same threshold, same scoping: only the
functions a diff actually touches, not the whole tree.

    scripts/check-docstrings.py                 # against origin/main
    scripts/check-docstrings.py <base-ref>
    scripts/check-docstrings.py --threshold 90 <base-ref>

Exits non-zero below the threshold, naming every undocumented function so
the output is a work list rather than a score.

"Touched" means the diff changed the function's signature, its doc comment,
or anything between that signature and the next one. That last part is an
approximation of the body -- it avoids brace matching, which Rust makes
unreliable without a parser because braces appear inside strings, char
literals and comments. It errs towards counting a function as touched,
which is the safe direction for a gate.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys

# Only where the project keeps code it expects to be documented.
INCLUDED = (".rs",)
INCLUDED_DIRS = ("src/", "tests/")

FN = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:default\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+\"[^\"]*\"\s+)?fn\s+(\w+)")
DOC = re.compile(r"^\s*(?:///|//!)")
ATTR = re.compile(r"^\s*#\[")
HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@", re.M)


def run(*args: str) -> str:
    return subprocess.run(args, capture_output=True, text=True, check=True).stdout


def merge_base(base: str) -> str:
    """Where this change diverged from `base`.

    Diffing against the merge base rather than the tip of `base` keeps the
    result about your change: commits that landed on the base branch after
    you branched are not yours to document.
    """
    return run("git", "merge-base", base, "HEAD").strip()


def changed_files(base: str) -> list[str]:
    # No `...HEAD`: this compares the working tree, so uncommitted work counts.
    # Checking only committed state would mean the answer arrives one commit
    # too late to be useful before pushing.
    out = run("git", "diff", "--name-only", base)
    return [
        f
        for f in out.splitlines()
        if f.endswith(INCLUDED) and f.startswith(INCLUDED_DIRS)
    ]


def changed_lines(base: str, path: str) -> set[int]:
    """Line numbers touched in the post-image of `path`."""
    diff = run("git", "diff", "-U0", base, "--", path)
    touched: set[int] = set()
    for hunk in HUNK.finditer(diff):
        start = int(hunk.group(1))
        count = int(hunk.group(2) or 1)
        touched.update(range(start, start + count))
    return touched


def functions(lines: list[str]) -> list[tuple[int, str, bool, int]]:
    """Every function, as (line, name, documented, doc_start).

    `doc_start` is where its doc comment and attributes begin, so a diff that
    only edits the comment still counts as touching the function.
    """
    found = []
    for i, line in enumerate(lines):
        match = FN.match(line)
        if not match:
            continue
        j = i - 1
        while j >= 0 and (DOC.match(lines[j]) or ATTR.match(lines[j])):
            j -= 1
        documented = any(DOC.match(lines[k]) for k in range(j + 1, i))
        found.append((i + 1, match.group(1), documented, j + 2))
    return found


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base", nargs="?", default="origin/main")
    parser.add_argument("--threshold", type=float, default=80.0)
    args = parser.parse_args()

    base = merge_base(args.base)
    total = 0
    documented = 0
    missing: list[str] = []

    for path in changed_files(base):
        touched = changed_lines(base, path)
        if not touched:
            continue
        try:
            with open(path, encoding="utf-8") as handle:
                lines = handle.read().split("\n")
        except FileNotFoundError:
            continue  # deleted in this change; nothing left to document
        found = functions(lines)
        for index, (line, name, has_doc, doc_start) in enumerate(found):
            # Everything up to the next function stands in for the body.
            end = found[index + 1][3] - 1 if index + 1 < len(found) else len(lines)
            if not any(n in touched for n in range(doc_start, end + 1)):
                continue
            total += 1
            documented += has_doc
            if not has_doc:
                missing.append(f"  {path}:{line}  {name}")

    if total == 0:
        print("No functions touched; nothing to check.")
        return 0

    coverage = 100.0 * documented / total
    print(
        f"Docstring coverage {coverage:.1f}% "
        f"({documented}/{total} functions touched since {args.base})"
    )
    if missing:
        print("\nUndocumented:")
        print("\n".join(missing))

    if coverage + 1e-9 < args.threshold:
        print(
            f"\nBelow the {args.threshold:.0f}% threshold. Say why each function "
            f"exists, not what it does -- and only where it is true."
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
