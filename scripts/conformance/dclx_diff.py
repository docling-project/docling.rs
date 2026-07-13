#!/usr/bin/env python3
"""Geometry-tolerant line diff for DocLang ``document.xml`` conformance scoring.

The default ``diff a b | grep -c '^[<>]'`` used by dclx_conformance.sh compares
every line byte-for-byte. That is the right measure for text and structure, but
it is *wrong* for the four ``<location value="N"/>`` provenance tokens each
laid-out block emits (l,t,r,b on docling's 0-500 normalized page grid). On the
PDF path those integers come from docling's layout clusters in the reference and
from ours; the same region is boxed a few grid units apart by the two layout
post-processors, so the geometry lines diff even when text and structure match.

This tool line-diffs exactly like the shell does, with one change: when a diff
aligns a ``<location value="N"/>`` line against another ``<location value="M"/>``
line and ``|N-M| <= tol``, the pair counts as equal. Everything else -- text,
tags, nesting, spans, and any location whose value is off by more than ``tol`` --
is still compared exactly. It never drops unmatched lines: a reference line with
no counterpart always counts against the score.

    dclx_diff.py REF.xml OURS.xml [--tol N]   # prints the diff-line count

With ``--tol 0`` the output is identical to ``diff | grep -c '^[<>]'``.
"""
from __future__ import annotations

import argparse
import difflib
import re
import sys

_LOC = re.compile(r'^\s*<location value="(-?\d+)"/>\s*$')


def _loc_val(line: str):
    m = _LOC.match(line)
    return int(m.group(1)) if m else None


def diff_lines(ref, ours, tol):
    """Count differing lines (both sides), treating location tokens within
    ``tol`` grid units of each other as equal. Mirrors ``diff | grep -c '^[<>]'``."""
    sm = difflib.SequenceMatcher(a=ref, b=ours, autojunk=False)
    diff = 0
    for tag, i1, i2, j1, j2 in sm.get_opcodes():
        if tag == "equal":
            continue
        if tag == "replace" and (i2 - i1) == (j2 - j1):
            # Equal-length replacement: pair the lines up so a block of four
            # geometry tokens is compared coordinate-by-coordinate.
            for a, b in zip(ref[i1:i2], ours[j1:j2]):
                va, vb = _loc_val(a), _loc_val(b)
                if va is not None and vb is not None and abs(va - vb) <= tol:
                    continue  # geometry agrees within tolerance
                diff += 2      # one changed line counts on both sides
        else:
            # Insertions, deletions, and ragged replacements: every unmatched
            # line counts, exactly as the raw line diff would score them.
            diff += (i2 - i1) + (j2 - j1)
    return diff


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("ref")
    ap.add_argument("ours")
    ap.add_argument("--tol", type=int, default=0,
                    help="max grid-unit difference for two <location> values to match")
    args = ap.parse_args()
    with open(args.ref, encoding="utf-8") as f:
        ref = f.read().splitlines()
    with open(args.ours, encoding="utf-8") as f:
        ours = f.read().splitlines()
    print(diff_lines(ref, ours, args.tol))
    return 0


if __name__ == "__main__":
    sys.exit(main())
