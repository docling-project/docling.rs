# Adobe Glyph List

`glyphlist.txt` is the Adobe Glyph List, table version 2.0 (September 20,
2002), unmodified: the glyph name → Unicode table a PDF reader resolves a
font's `/Differences` and builtin-encoding names with. docling-pdf compiles
it in under its `glyph-list` feature (`src/adobe_glyph_list.rs`); the wasm
build leaves it out.

* Source: Adobe's `agl-aglfn` project
  (<https://github.com/adobe-type-tools/agl-aglfn>, formerly
  `http://sourceforge.net/adobe/aglfn/`), as shipped in docling-parse 7.25.0
  (`pdf_resources/glyphs/standard/glyphlist.dat`).
* SHA-256: `5775f034295db863835e505788fec46027318e43f0d1669bbfb6a71b88549f26`
* License: BSD-3-Clause, the notice in the file's own header and in
  [`LICENSE.txt`](./LICENSE.txt).

`LICENSE.txt` also carries the notices for the Adobe-derived tables that are
compiled into the crate from Rust source rather than read from this
directory: the ITC Zapf Dingbats Glyph List and the Adobe Core 14 AFM files
behind `src/symbol_fonts.rs` and `src/std14.rs`.

Every published binary carries that file as `LICENSE-adobe-glyph-list.txt`,
next to `LICENSE-mapping-resources-pdf.txt` — the wasm package and the demo
site too, for the AFM-derived tables. A binary you build and redistribute
yourself needs both beside it as well.

`crates/docling-py/LICENSE-adobe-glyph-list.txt` is a copy of `LICENSE.txt`
(maturin ships the `LICENSE-*` files of the binding's own directory); a test
in `src/adobe_glyph_list.rs` fails when the two differ.
