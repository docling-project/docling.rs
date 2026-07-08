#!/usr/bin/env python3
"""Validate the Rust parser extracts ALL the text docling-parse does — nothing
skipped (the orphan-symbol class of bug).

For each groundtruth PDF, compare the multiset of characters docling-parse emits
in its char cells against the multiset the Rust parser emits (via the
`textparse_glyphs` example, stderr). Alignment-free (frequency only), so RTL /
garbled text doesn't confuse it. Reports, per PDF, characters the parser drops
(docling has more) or invents (parser has more), with counts.

Run after building the example:
  cargo build --release -p docling-pdf --example textparse_glyphs
  python scripts/parser_completeness.py
"""
import collections
import re
import subprocess
import sys
import unicodedata
from pathlib import Path

from docling_parse.pdf_parser import DoclingPdfParser

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "tests/data/pdf/sources"
BIN_EXAMPLE = ["cargo", "run", "--release", "--quiet", "-p", "docling-pdf",
               "--example", "textparse_glyphs", "--"]


def docling_chars(pdf):
    p = DoclingPdfParser()
    doc = p.load(str(pdf))
    counter = collections.Counter()
    npages = doc.number_of_pages()
    for pno in range(1, npages + 1):
        try:
            d = doc.get_page(pno).export_to_dict()
        except Exception:
            continue
        for c in d.get("char_cells", []):
            for ch in c.get("text", ""):
                counter[ch] += 1
    return counter, npages


def parser_chars(pdf, npages):
    # Iterate the *known* page count — a blank page (e.g. an image-only page)
    # in the middle must not stop the scan, or trailing pages get miscounted.
    counter = collections.Counter()
    for pno in range(npages):
        r = subprocess.run(BIN_EXAMPLE + [str(pdf), str(pno)],
                           capture_output=True, text=True, cwd=ROOT)
        for ch in r.stdout.strip("\n"):
            counter[ch] += 1
    return counter


def name(ch):
    if ch == " ":
        return "SPACE"
    try:
        return unicodedata.name(ch)
    except ValueError:
        return f"U+{ord(ch):04X}"


def main():
    stems = sorted(p.stem for p in SRC.glob("*.pdf"))
    if len(sys.argv) > 1:
        stems = sys.argv[1:]
    for stem in stems:
        pdf = SRC / f"{stem}.pdf"
        d, npages = docling_chars(pdf)
        m = parser_chars(pdf, npages)
        dropped = {ch: d[ch] - m.get(ch, 0) for ch in d if d[ch] > m.get(ch, 0)}
        extra = {ch: m[ch] - d.get(ch, 0) for ch in m if m[ch] > d.get(ch, 0)}
        # ignore pure whitespace-count noise (spacing handled by the sanitizer)
        dropped = {c: n for c, n in dropped.items() if not c.isspace()}
        extra = {c: n for c, n in extra.items() if not c.isspace()}
        dn = sum(dropped.values())
        en = sum(extra.values())
        flag = "" if dn == 0 and en == 0 else "  <-- DIFF"
        print(f"{stem:32} docling={sum(d.values()):6}  parser={sum(m.values()):6}  "
              f"dropped={dn:4} invented={en:4}{flag}")
        for ch, n in sorted(dropped.items(), key=lambda x: -x[1])[:6]:
            print(f"    DROPPED  x{n:<4} {ch!r:8} {name(ch)}")
        for ch, n in sorted(extra.items(), key=lambda x: -x[1])[:6]:
            print(f"    INVENTED x{n:<4} {ch!r:8} {name(ch)}")


if __name__ == "__main__":
    main()
