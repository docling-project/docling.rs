# Pure-Rust PDF text parser — WIP notes & roadmap

Goal: replace pdfium's text-extraction layer with a pure-Rust parser whose
character cells match docling's `docling-parse` C++ parser, so the PDF pipeline
can reach docling byte-conformance (and eventually drop pdfium for text — pdfium
would stay only for page rasterisation).

## Why (the measured case)

docling-parse and pdfium disagree on glyph geometry at exactly the points that
break conformance: pdfium gives generated **spaces a zero-width box**, gives
**combining diacritics a real-width box**, and lands ligature/fraction glyphs at
different x. A ceiling experiment — injecting docling-parse's own cells into our
pipeline (keeping our layout + TableFormer) — measured:

| Cells used | Exact |
|---|---|
| pdfium (baseline) | 4/14 |
| docling-parse cells injected | **6/14** (amt + right_to_left_01 flip to exact) |
| + the one `right_to_left_02` `11`-page-number layout fix | **7/14 = 50%** |

So the text parser is the lever; 50% is reachable.

## What's built (`crates/fleischwolf-pdf/src/textparse.rs`)

Opt-in via `DOCLING_RUST_PARSER=1` (default pipeline is unchanged). Pdfium still
provides page rasters + word/code cells; the parser only replaces prose line
cells, fed through the existing `dp_lines` sanitizer.

- Content-stream interpreter: `cm/q/Q`, `BT/ET`, `Tf/Td/TD/Tm/T*/Tc/Tw/Tz/TL/Ts`,
  `Tj/TJ/'/"` with text + graphics matrices.
- **Advance-width geometry** from the font (spaces get real width; combining
  marks get zero advance) — the whole point.
- Fonts: Type0/CID + Identity-H (`/W`, `/DW`), simple Type1/TrueType
  (`/FirstChar`+`/Widths`, `/MissingWidth`), FontDescriptor ascent/descent.
- Encodings: ToUnicode CMap (`bfchar`, `bfrange` scalar **and** array forms,
  structural tokenizer for back-to-back `<..><..>` hex); WinAnsi + MacRoman base
  encodings; `/Differences` via a small Adobe-glyph-name subset.

## Current result: 5/14 — and the parser is now the DEFAULT text layer

`code_and_formula`, `multi_page`, `picture_classification`, `2305.03393v1-pg9`,
**`right_to_left_01`** byte-exact (the last is parser-only — pdfium gives 4/14).
The parser is wired as the default; set `DOCLING_PDFIUM_TEXT=1` to fall back to
pdfium's text layer. A page with no parseable text layer falls back to pdfium
automatically, so scanned/edge-case pages are unaffected.

Remaining: `amt`=2 (blocker B), `right_to_left_02`=8 (blocker C). Everything else
is a heavy multi-column doc that is not byte-exact for layout/table reasons
independent of the text parser.

## Blocker A — DONE (commit a036133)

A lone punctuation glyph set in a separate punctuation font now bridges fonts
next to RTL text, so the Arabic sentence period attaches (`العمل.`).
`right_to_left_01` is **EXACT**.

## Completeness validation — "nothing is skipped"

`scripts/parser_completeness.py` compares, per PDF, the *multiset* of characters
docling-parse emits against the parser's (alignment-free, so garbled RTL doesn't
confuse it). It surfaced two whole classes of silently-dropped text, both fixed:

1. **Form XObject text** (`Do` operator). Bulk body text in heavy PDFs lives
   inside a Form XObject, reached only via `Do`; the parser walked just the page
   content stream and dropped it (2206 p1 dropped ~9000 chars). `page_glyphs` is
   now a recursive `run_content` that decodes the form's stream, concatenates its
   `/Matrix`, and recurses with the form's own `/Resources` (depth-guarded).

2. **Glyph-name fallback.** docling emits an unresolvable `/Differences` glyph
   name verbatim (`/g115`, `/SM590000`) when a subsetted font has no usable
   Unicode mapping (redp5110's bulleted list, IBM BookMaster). The parser dropped
   them (low codes outside WinAnsi). `decode_code` now mirrors docling for
   synthetic GID-style names; `glyph_name_to_char` was widened to the AGL
   algorithmic subset (single letters, digit/punctuation names, `.suffix`).

After both fixes every previously text-exact fixture stays `dropped=0
invented=0`, and the heavy docs are near-complete (redp5110 33070/33073 chars).
The residue is the punctuation-normalization class below.

## Blocker B — amt fraction double space (ROOT-CAUSED; blocked on font metrics)

Diff: `up to  1 / 4` / `from  1 / 4` have a **double** space; `1 / 6` and
`3 / 8` stay single. Fully traced through docling's contraction:

- The fractions are separate glyphs (`1`, `⁄`, `4`); the `⁄` (U+2044) is in a
  **different font**, so the contraction fragments there. The numerator `1` is a
  small **raised** glyph (~4 pt above the baseline).
- docling **absorbs** the raised `1` into the preceding line. Because the
  Euclidean corner gap (≈4.0, dominated by the vertical raise) exceeds
  `delta = avg·0.33`, `merge_with` inserts a *generated* space — on top of the
  explicit space char → **double**. Whether it absorbs hinges on `eps0 = avg·1.0`
  vs that ≈4.0 gap, a knife-edge that flips per line on `avg_char_width`. ¼'s
  lines clear it; ⅙/⅜'s don't (their numerator stays a standalone cell → single).

- **Why the parser misses it:** docling boxes every glyph with the embedded
  font's *typographic* ascent/descent (TrueType **OS/2 sTypoAscender/Descender**,
  e.g. Times 693/−216), proven by every glyph on a line sharing one box height
  (8.47 pt) while the raised fraction digit gets its own (4.7 pt). The parser
  uses the PDF descriptor's `/Ascent 897 /Descent −250` (≈30 % taller), so the
  loose box hangs ~0.3 pt lower and the gap reads 4.30 instead of 4.00 — just
  past `eps0`, so nothing absorbs and every fraction stays single.

- **Attempted fix + why reverted:** reading OS/2 metrics from `/FontFile2` (a
  compact sfnt reader) moved the gap to 4.17 and flipped *one* of the two ¼'s to
  double — but it **regressed `right_to_left_01`** (Arabic box geometry shifted)
  and still didn't fix the second ¼. A faithful fix needs the embedded font's
  exact per-font metrics *and* a way to keep the Arabic path stable — i.e. the
  box-geometry layer has to match docling globally, not per-case. Left for a
  dedicated font-metrics effort; a magic-number nudge is too fragile to ship.

## Blocker C — right_to_left_02 (open, two parts)
1. Layout: the top `11` page number is classified as a picture (`<!-- image -->`);
   the recovered orphan lands at the bottom. docling labels it `text`, first.
   This is an RT-DETR layout-model classification difference, not a text issue.
2. Text: the parser emits ~25 extra `و` (waw) on the scanned-garbled Arabic
   (`قويووووة` vs `قويوووة`) — a kashida/tatweel elongation the parser renders as
   repeated glyphs where docling collapses them. Needs the parser to match
   docling's tatweel handling.

## Future improvements (validated by the completeness pass)

- **Punctuation normalization.** docling-parse normalizes typographic punctuation
  to ASCII in its C++ layer (`’`→`'`, `–`/`—`→`-`, curly→straight quotes) while
  the parser faithfully emits ToUnicode's forms. This is the dominant residual
  diff on the Latin heavy docs (2305: 38→93 vs pdfium; normal_4pages = 74, almost
  all apostrophes) and the main reason the parser *raises* diff-lines on a few
  non-exact docs even though it raises the exact count. A normalization table
  matching docling's would help broadly — but must be verified not to disturb the
  5 exact files.
- **Embedded-font metrics** (OS/2 typo ascent/descent, see blocker B) — needed for
  fraction/superscript box fidelity, but globally entangled with RTL geometry.
- **Embedded TrueType `cmap`/`post` recovery.** Identity-H fonts with a *stub*
  ToUnicode (only a codespacerange) need the embedded font program's cmap to
  recover Unicode (2206 p1 drops ~591 caps). Requires a TrueType table reader.

## Roadmap to 7/14
1. ~~Blocker A~~ — DONE (rtl_01 exact).
2. ~~Make the parser the conformance default~~ — DONE (5/14; opt-out via
   `DOCLING_PDFIUM_TEXT`).
3. Blocker B (fraction double space) → amt exact → 6/14. **Blocked on a
   font-metrics layer** (see above); not a knob-twist.
4. Blocker C (layout `11` + kashida) → right_to_left_02 exact → 7/14.
5. Long term: drop pdfium's text path (keep it for rasterisation).

## Tooling (under `scripts/`)

- `parser_completeness.py` — per-PDF char-frequency diff docling-parse vs the
  parser; the "nothing skipped" validator that surfaced the Form-XObject and
  glyph-name drops. Run after `cargo build --example textparse_glyphs`.
- `dump_parse_cells.py` — docling-parse textline cells → JSON/TSV (the oracle).
- `docling_dump_all.py` — full docling items (label/page/bbox/text) per PDF.
- `textparse_dump` example — the Rust parser's cells; `TSV_OUT=1` emits the
  injection TSV for ceiling experiments.
- `textparse_glyphs` example — `<pdf> <page>`: raw glyph chars (stdout) + boxes
  (stderr), for char-cell comparison.
- `probe_page` example — `<pdf> <page>`: operator histogram, fonts (with
  BaseFont), and XObject subtypes for a page (debugging dropped text).

Also in this branch: `assemble::add_orphan_regions` — docling-parity orphan-cell
clustering (emits text the layout detector missed, e.g. amt's stray `.`).
