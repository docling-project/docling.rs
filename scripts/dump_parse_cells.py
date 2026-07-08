#!/usr/bin/env python3
"""Dump docling-parse textline cells per PDF as JSON in TOP-LEFT page-point coords
(matching docling.rs's TextCell), for the parser-overhaul ceiling experiment.

Output: <out>/<stem>.cells.json = {"pages":[{"w":..,"h":..,
        "cells":[{"text","l","t","r","b"}, ...]}, ...]}
"""
import json
import sys
from pathlib import Path
from docling_parse.pdf_parser import DoclingPdfParser


def main():
    out = Path(sys.argv[-1])
    out.mkdir(parents=True, exist_ok=True)
    p = DoclingPdfParser()
    for pdf in sys.argv[1:-1]:
        pdf = Path(pdf)
        doc = p.load(str(pdf))
        pages = []
        npages = doc.number_of_pages()
        for pno in range(1, npages + 1):
            try:
                page = doc.get_page(pno)
                d = page.export_to_dict()
            except Exception as e:
                print(f"  {pdf.stem} p{pno} SKIP: {e}", file=sys.stderr)
                pages.append({"pw": None, "ph": None, "cells": []})
                continue
            dim = d.get("dimension", {})
            rect = dim.get("rect", {}) or {}
            pw = rect.get("r_x2"); ph = rect.get("r_y2")
            cells = []
            for c in d.get("textline_cells", []):
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
                yb, yt = min(ys), max(ys)  # bottom, top in bottom-left coords
                cells.append({"text": t, "l": l, "r": rr, "yb": yb, "yt": yt})
            pages.append({"pw": pw, "ph": ph, "cells": cells})
        # convert to top-left now that we know ph
        for pg in pages:
            ph = pg["ph"]
            for c in pg["cells"]:
                if ph is not None:
                    c["t"] = ph - c.pop("yt")
                    c["b"] = ph - c.pop("yb")
                else:
                    c["t"] = c.pop("yt"); c["b"] = c.pop("yb")
        (out / f"{pdf.stem}.cells.json").write_text(
            json.dumps({"pages": pages}, ensure_ascii=False))
        print(f"{pdf.stem}: {sum(len(p['cells']) for p in pages)} cells", file=sys.stderr)


if __name__ == "__main__":
    main()
