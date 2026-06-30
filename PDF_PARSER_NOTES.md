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

## Current result: 3/14 (matches pdfium's text quality)

`code_and_formula`, `multi_page`, `picture_classification` exact; `amt`=2,
`right_to_left_01`=2 (same as pdfium). The parser extracts Latin + Arabic
correctly and no longer regresses any text-exact file.

## Why it isn't 6/14 yet — the next lever is the SANITIZER

amt/rtl_01 are stuck at 2 **identical to pdfium**, because their remaining diffs
(the justified tanwin spacing, the fraction line-wrap double space) are produced
by the `dp_lines` sanitizer, which is shared by both the pdfium and Rust paths.
The 6/14 ceiling used docling-parse's *post-sanitizer* textlines. So reaching it
needs `dp_lines` to match docling-parse's C++ contraction on those cases — a
separate fidelity effort, independent of the parser.

## Progress: sanitizer fidelity (commit f5a80ef)

The parser path now reproduces docling-parse's char cells *and* most of its
spacing. Two `dp_lines`/`textparse` fixes landed:
- **Euclidean d0** for space insertion on the clean-box parser path (matches
  docling-parse's `merge_with`); fixed the standalone tanwin.
- **q/Q restore the full text state** (Tc/Tw/Tz/TL/Tfs/Trise/font); fixed the
  character-spacing drift that broke multi_page.

Parser path now: code_and_formula / multi_page / picture_classification exact;
**amt = 2**, **right_to_left_01 = 2**. The two remaining diffs are precisely:

### Remaining blocker A — end-of-line period fragments (right_to_left_01)
Root-caused: my char cells match docling's **exactly** (the sentence period is a
separate font `/F4` glyph sitting at the justified line's left end, x≈394, while
the preceding word is at x≈519). docling emits the **whole visual line as one
textline cell** (baseline grouping + x-sort), so the period is *inside* the line
and orders correctly (`العمل.`). My `dp_lines` contraction is **adjacency**-based
(corner distance) and additionally gated by `enforce_same_font`, so the period
(font change + a justification x-gap) does **not** merge — it stays a separate
cell. Then `assemble::region_text` in dp mode joins *every* cell with a single
space, inserting one before the period (`العمل .`).

Fix options: (a) group the contraction by baseline into one line cell like
docling (bigger change), or (b) in `region_text`, when the parser path produced
fragmented cells, suppress the inter-cell space for a lone trailing punctuation
attached to the prior word — but must NOT break the cases docling keeps spaced
(`Name 1 .`, `[ 9 ]`). Needs care + the snapshot suite as a guard.

### Remaining blocker B — fraction line-wrap double space (amt)
`up to  1 / 4` (double) only on the two fractions that fall at a **column line
wrap**; docling's textline ends at the wrapped `1` with a double space. Needs the
line-wrap join to reproduce docling's trailing-space behaviour.

## Roadmap to 7/14

1. Fix blocker A (bidi neutral) → right_to_left_01 exact.
2. Fix blocker B (fraction wrap) → amt exact → **6/14**.
3. **`right_to_left_02` layout**: top `11` page number mis-classified as a
   picture; the recovered orphan lands at the bottom; docling labels it `text`
   first → fix → **7/14**.
4. Make the parser the default for the conformance path (it keeps the 3 text-
   exact files and pdfium word cells for tables; validate the heavy docs
   2203/2206/redp5110 don't regress the exact count — they're far from exact
   either way).
5. Long term: drop pdfium's text path entirely (keep it for rasterisation).

## Tooling (under `scripts/`)

- `dump_parse_cells.py` — docling-parse textline cells → JSON/TSV (the oracle).
- `docling_dump_all.py` — full docling items (label/page/bbox/text) per PDF.
- `textparse_dump` example — the Rust parser's cells; `TSV_OUT=1` emits the
  injection TSV for ceiling experiments.

Also in this branch: `assemble::add_orphan_regions` — docling-parity orphan-cell
clustering (emits text the layout detector missed, e.g. amt's stray `.`).
