"""Build scan_with_native_word.pdf (#999): `scanned_chart_table.pdf` with one
typed-in native word, `420` in Helvetica 14 at (226, 334) — the value a filled-in
form carries over its own scanned pixels.

    python3 gen.py ../../../../../tests/data/scanned/sources/scanned_chart_table.pdf scan_with_native_word.pdf
"""
import sys

import pypdf
from pypdf.generic import DecodedStreamObject, DictionaryObject, NameObject
r = pypdf.PdfReader(sys.argv[1])
w = pypdf.PdfWriter()
w.append(r)
p = w.pages[0]
font = DictionaryObject({NameObject("/Type"): NameObject("/Font"), NameObject("/Subtype"): NameObject("/Type1"),
    NameObject("/BaseFont"): NameObject("/Helvetica"), NameObject("/Encoding"): NameObject("/WinAnsiEncoding")})
res = p["/Resources"]
res[NameObject("/Font")] = DictionaryObject({NameObject("/F1"): w._add_object(font)})
old = p.get_contents().get_data()
s = DecodedStreamObject(); s.set_data(old + b"BT /F1 14 Tf 226 334 Td (420) Tj ET\n")
p[NameObject("/Contents")] = w._add_object(s)
w.add_metadata({"/Producer": "docling.rs #999 fixture"})
with open(sys.argv[2], "wb") as f: w.write(f)
