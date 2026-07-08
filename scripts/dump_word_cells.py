#!/usr/bin/env python3
"""Dump docling-parse word_cells for one PDF page in TOP-LEFT page-point coords
(matching docling.rs's TextCell), the oracle for the parser word-grouping port.

Usage: dump_word_cells.py <pdf> <page_1based>
Prints TSV: l<tab>t<tab>r<tab>b<tab>text  (sorted reading order as emitted)
"""
import sys
from docling_parse.pdf_parser import DoclingPdfParser


def main():
    pdf = sys.argv[1]
    pno = int(sys.argv[2])
    p = DoclingPdfParser()
    doc = p.load(pdf)
    page = doc.get_page(pno)
    d = page.export_to_dict()
    dim = d.get("dimension", {})
    rect = dim.get("rect", {}) or {}
    ph = rect.get("r_y2")
    for c in d.get("word_cells", []):
        t = c.get("text", "")
        if not t:
            continue
        r = c.get("rect", {})
        xs = [r.get("r_x0"), r.get("r_x1"), r.get("r_x2"), r.get("r_x3")]
        ys = [r.get("r_y0"), r.get("r_y1"), r.get("r_y2"), r.get("r_y3")]
        xs = [v for v in xs if v is not None]
        ys = [v for v in ys if v is not None]
        if not xs or not ys:
            continue
        l, rr = min(xs), max(xs)
        yb, yt = min(ys), max(ys)
        t_top = ph - yt if ph is not None else yt
        b_top = ph - yb if ph is not None else yb
        print(f"{l:.2f}\t{t_top:.2f}\t{rr:.2f}\t{b_top:.2f}\t{t}")


if __name__ == "__main__":
    main()
