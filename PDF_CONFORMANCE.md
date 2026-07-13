# PDF conformance

How close the Rust PDF pipeline gets to docling's **default** Markdown, measured
byte-for-byte against the committed groundtruth (`tests/data/pdf/groundtruth/*.md`).
The groundtruth is regenerated from **live published docling**, so it agrees with
`scripts/conformance/conformance.sh pdf`.

> Measure locally with `scripts/conformance/pdf_groundtruth.sh` (diffs the checked-in
> reference; no docling install needed) or `scripts/conformance/conformance.sh pdf` (installs
> docling and diffs against it). Both report two metrics: **strict** (byte-for-byte)
> and **whitespace-normalized** (spacing-only diffs ignored). Diff = changed lines
> vs the groundtruth (one changed line counts as 2).

## Current state

**5 / 14 strict** · **6 / 14 whitespace-normalized.** (The two Korean
image-only pages `skipped_1page`/`skipped_2pages` carry no text groundtruth and
are no longer scored.)

| PDF | diff | dominant remaining blocker |
|---|---:|---|
| picture_classification | **exact** | — |
| multi_page | **exact** | — |
| 2305.03393v1-pg9 | **exact** | — (TableFormer table, cell-for-cell) |
| right_to_left_01 | **exact** | — (RTL period attachment) |
| right_to_left_02 | **exact** | — (kashida dedup + page-number layout) |
| amt_handbook_sample | 2 *(ws-ok)* | docling's spurious fraction double space — ours is more faithful |
| code_and_formula | 5 | code block reflowed to multiple lines + trailing newline |
| 2305.03393v1 | 26 | title-page reading order + author-ID run spacing |
| normal_4pages | 44 | reading order (heading numbering, footnote order) |
| right_to_left_03 | 60 | RTL bidi |
| table_mislabeled_as_picture | 86 | layout over-detects tables (survey rendered as tables) |
| 2206.01062 | 80 | TableFormer multi-row headers + title-page reading order |
| 2203.01017v2 | 130 | TableFormer structure + reading order |
| redp5110_sampled | 194 | TOC OTSL structure (model-level); cover-page ordering |

`amt` is the 6th under the whitespace-normalized metric: its only diff is
docling's spurious double space before the `1⁄4` fraction, where our single-spaced
output is the more faithful rendering. The remaining non-exact PDFs are heavy
multi-column / table docs whose gaps are model-level (TableFormer structure,
layout classification, title-page reading order), not text-layer.

The heavy table docs improved with the docling-parse **word-cell** grouping
feeding TableFormer and the #61 layout/reading-order postprocessor
(2305.03393v1 93→30, 2203.01017v2 209→161, 2206.01062 198→92): the parser's
per-word cells reproduce docling-parse's `word_cells` byte-for-byte, so
cell-to-grid matching tracks docling more closely. See "Text reconstruction"
below. The #60 matching work (docling's `MatchingPostProcessor` ported to
`tf_match.rs`, plus docling's exact table-crop rounding chain) took
2203 157→150 and redp5110 204→202 with every other fixture unchanged; the
#62 text fixes (docling-parse's quote-normalization table — every curly
quote → `'` — and joining region cells in docling-parse index order
instead of geometric bands) then took 2203 →130, 2206 92→80, 2305 28→26,
normal_4pages 56→44, redp5110 →194, and table_mislabeled 88→86.

## DocLang (`.dclx`) conformance

Separate from the Markdown metric above: how close `--to dclx` gets to docling's
DocLang archive, scored on the extracted `document.xml` against the committed
groundtruth (`tests/data/pdf/groundtruth_dclx/*.dclx`, from published docling
2.112.0). Run `scripts/conformance/dclx_conformance.sh pdf`; sweep the tolerance
with `scripts/conformance/dclx_pdf_tol_sweep.sh`.

**PDF avg similarity: 52 % exact · 63 % at the default ±2-grid-unit tolerance**
(issue #32 target: ≥50 %). The ±2 figure is within a point of the
*geometry-ignored* ceiling (65 %), so essentially all of the coordinate
difference is absorbed by ±2 — a wider tolerance buys almost nothing.

### What the geometry tolerance is, and why it is honest

Every laid-out block in a DocLang archive carries four `<location>` provenance
tokens — its bbox as `round(512·coord/page_dim)` on a 0–511 page grid
(docling_core's `_create_location_tokens_for_bbox`). We emit the same tokens
from our layout cluster boxes (`assemble.rs`, `norm_loc`) for text, headings,
tables, pictures, list items (on `ListItem.location`), code, and the
`page_header`/`page_footer` furniture blocks. Because our heron
layout model is docling's, the boxes agree to **~1 grid unit**; the small
residual is the aspect-ratio-stretch-vs-letterbox preprocessing difference, not a
structural gap. `dclx_diff.py` therefore counts a `<location>` pair as matching
when the two values are within `DCLX_TOL` (default **2**) grid units — **text,
tags, nesting, spans, and every non-geometry line stay byte-exact, and unmatched
lines always count against the score**. The tolerance is applied **only to PDF**,
where the reference geometry comes from docling's own layout run; formats whose
geometry is read from the same source file (OOXML slides/sheets) stay exact
(`DCLX_TOL=0`). `DCLX_TOL=0` reproduces a raw `diff` line-for-line.

### Per-fixture (±2)

Text/list-heavy pages land high (multi_page 82 %, right_to_left_02 82 %,
code_and_formula 81 %, 2305-pg9 78 %, right_to_left_01 75 %, amt 72 %,
normal_4pages 71 %, redp5110 65 %, 2206 61 %); the low ones are **model-level,
not provenance**: the big table papers (2203 51 %, 2305 52 %) diverge in
TableFormer cell structure (2203 alone is ~19 k table-grid diff lines),
table_mislabeled/picture_classification in layout classification, and
skipped_1/2page (Korean image pages) + right_to_left_03 in picture detection /
bidi — the *same* blockers that cap the Markdown metric. The corpus average is
bounded by these, so raising it further is a model problem, not a serialization
one: every laid-out block kind now carries provenance, so the ±2 figure sits at
the geometry-ignored ceiling.

## How the pipeline works

pdfium extracts the glyph layer and renders each page to a bitmap; an ONNX stack
(layout detection, TableFormer, PaddleOCR) interprets it; regions are assembled in
reading order into a `DoclingDocument`. Tables use **TableFormer** (image encoder
+ autoregressive OTSL structure decoder + cell-bbox decoder, ported and exported
to ONNX in `tableformer.rs`) on a cv2-exact preprocessed crop (`resample.rs`); the
structure + matched cell text reproduce docling's padded GitHub tables (2305-pg9
is cell-for-cell exact).

### Performance / parallelism

Profiling a 14-page document (`DOCLING_RS_TIMING=1` prints an env-gated per-stage
wall-clock breakdown) shows ~80 % of the time is the two ONNX models (layout ~58 %,
TableFormer ~22 %) and ~16 % the page-image downsample — all per-page work that is
independent across pages. A multi-page PDF therefore renders on one thread (pdfium
is not thread-safe) and fans the pages out across a **pool of page-workers**, each
owning its own model set (`ort`'s `Session::run` is `&mut self`, so sessions can't
be shared), reassembled in page order. A bounded channel keeps only a handful of
page bitmaps resident, so the streaming memory profile is preserved; the output is
byte-identical to the serial path (verified across all PDF snapshots). Single-page /
image / METS inputs keep the serial path and load no helper models.

The layout model is **memory-bandwidth bound** (even one model at four intra-op
threads only reaches ~2.1× core utilisation), so the pool defaults to two intra-op
threads per worker with `workers ≈ cores / 2` (capped at 4): two threads sharing one
in-cache copy of the weights beats both one fat model and many single-thread workers.
The speed-up scales with cores and memory bandwidth. Tune per machine with
`DOCLING_RS_PDF_WORKERS` (pool size) and `DOCLING_RS_PDF_INTRA` (intra-op threads
per worker).

### Text reconstruction: a pure-Rust PDF text parser (default)

The byte-exact ceiling was the **text extractor** — pdfium's *rendered* glyph
boxes diverge from docling's own `docling-parse` C++ parser at exactly the points
that drive conformance (generated spaces, combining marks, ligature/fraction
positioning). The pipeline now ships a **pure-Rust text parser** (`textparse.rs`,
on `lopdf`) that reconstructs each glyph's box from the *font's own advance
widths* and the PDF text/graphics matrices — the same information docling-parse
uses. It is the **default** text layer; set `DOCLING_PDFIUM_TEXT=1` to fall back
to pdfium. Pages without a parseable text layer fall back to pdfium
automatically, so scanned/OCR pages are unaffected. The parser supplies **all**
text — prose, the **word cells** TableFormer matches against, and **code cells**
(`DOCLING_PDFIUM_WORDS` reverts words+code to pdfium; `DOCLING_PDFIUM_TEXT`
reverts everything). pdfium now does only page rasterisation + link annotations.

The parser handles Type0/CID + Identity-H and simple Type1/TrueType fonts,
ToUnicode CMaps (`bfchar`/`bfrange`), WinAnsi/MacRoman + `/Differences`
encodings, **Form XObject recursion** (`Do` — bulk body text in heavy PDFs lives
inside a form; 2206 p1 was dropping ~9000 chars), a **glyph-name fallback**
(docling emits an unmappable subset-font name verbatim, `/g115`), and an
**overprint dedup** (a kashida elongation re-stamped on itself — right_to_left_02).
A char-frequency validator (`scripts/test/parser_completeness.py`) confirms nothing is
silently skipped.

Its cells feed the ported **docling-parse line sanitizer** (`dp_lines.rs`, from
`src/parse/page_item_sanitators/cells.h`): a 3-pass corner-distance contraction
(LTR → RTL → LTR-reverse) with `merge_with` space insertion (one space when the
gap exceeds 0.33×avg-char-width, plus literal space glyphs), `enforce_same_font`,
ligature recomposition, and loose-box geometry. On the clean parser boxes it uses
the Euclidean corner gap (matching docling); on pdfium's loose boxes it keeps the
signed horizontal gap.

The same contraction also produces **word cells** (`dp_lines::word_cells`): a word
is a maximal run of glyphs the contraction merges *without* inserting a separator
space, so the per-word segments split at exactly the `delta < gap` points — which
reproduces docling-parse's `word_cells` byte-for-byte (377/377 on 2305-pg9). These
are the per-word tokens TableFormer matches against table-grid cells, replacing
pdfium's word cells (roadmap item 6). **Code cells** come from the parser too,
via a gap-based grouping (`Grouping::CodeGap`): the parser emits no space glyphs
(a source space is a positioning gap), so a word breaks wherever the inter-glyph
gap exceeds ~0.25× the line height, with no punctuation glue — `et al. 2000`
keeps its space while `add(a,` / `b)` stay joined. `code_and_formula` is byte-exact
(`function add(a, b) { return a + b; }`). With this, pdfium's text path is fully
retired (rasters + links only).

Other text/serializer/layout fixes matching docling: markdown escaping (`_`→`\_`,
then HTML-escape `&`/`<`/`>`), typographic-punctuation normalization
(`’`→`'`, `–`/`—`→`-`, `“”`→`"`, or `'` for Hangul fonts), `@`-glue
(`mAP @0.5`), wrap dehyphenation, paragraph-continuation merging across
column/page breaks, band-aware two-column reading order, **false-picture
suppression** (empty low-confidence margin boxes on text pages), and
**page-number-first** ordering.

## Remaining blockers (model-level)

These yield smaller or uncertain gains than the text-layer work already shipped.
The issues that tracked them (#60–#63) are **closed**: everything
heuristic-level in them landed, and what remains below is the documented
model-level (or by-design) residual each issue closed with:

1. **TableFormer structure on complex tables**
   ([#60](https://github.com/docling-project/docling.rs/issues/60)). The
   *matching* half is done: docling's `MatchingPostProcessor` (cell-class-aware
   good/bad IOU split, column-median snapping, adjacent-column de-duplication,
   best-intersection word assignment, row/column-band orphan pickup) is ported
   in `tf_match.rs` and is the default word→cell matcher, and the table crop
   reproduces docling's exact rounding chain (`round(bbox) → ×2 → ×1024/h →
   round`, banker's rounding) — 2203 157→150, redp5110 204→202, everything else
   unchanged. The rest is **model-level**: the OTSL tag stream itself differs
   from live docling on the hard crops (redp5110's TOC predicts `ched` where
   docling gets `fcel`; multi-row headers / spans on 2206, 2203), so one
   cell-structure diff still cascades through the padded columns into many row
   diffs (2206's ~92 table-row diffs trace to ~4 structure diffs). A parity
   harness (`DOCLING_RS_TF_MATCH_DUMP=dir` + `scripts/test/ref_match.py`-style
   replay through docling's Python post-processor) confirmed the ported matcher
   reproduces the reference on identical inputs, isolating the residual to the
   model predictions. `DOCLING_RS_TF_SIMPLE_MATCH=1` reverts to the pre-port
   best-overlap matcher.
2. **Layout classification**
   ([#61](https://github.com/docling-project/docling.rs/issues/61)) — *addressed
   by porting docling's `LayoutPostprocessor`.* The raw RT-DETR detections now go
   through the cleanup docling applies before assembly: per-label confidence
   thresholds (`CONFIDENCE_THRESHOLDS`, stricter than the 0.3 base — a
   picture/table/list needs ≥ 0.5), regular/picture/wrapper **bucketed** overlap
   resolution (a high-score picture no longer suppresses a lower-score table or
   table-of-contents index), the picture-vs-table cross-type rule
   (`_handle_cross_type_overlaps`), and dropping a regular region absorbed by a
   table/index/picture so it isn't emitted twice. With this, table_mislabeled's
   survey is no longer over-detected as tables (108 → 88 vs groundtruth), and
   redp5110's table-of-contents is now classified and rendered as a **table**
   (`document_index`) instead of a picture. The TOC table's remaining diff is a
   TableFormer dot-leader column-matching gap, tracked with the other
   table-structure work in
   [#60](https://github.com/docling-project/docling.rs/issues/60). *(The
   groundtruth byte counts in the table above predate this change; regenerate the
   committed snapshots with the `models-v1` models to refresh them.)*
3. **Complex title-page reading order**
   ([#62](https://github.com/docling-project/docling.rs/issues/62)). Author-block
   / abstract interleaving on the academic papers (band reading-order handles the
   full-width title; the in-column author/abstract order is still off). Two
   pieces landed: the suspected "TeX-font quote decode" gap turned out to be
   docling-parse's *sanitizer* table (every curly quote → `'`; a `"` only ever
   comes from a literal `quotedbl` glyph) — no font-program parsing needed —
   and region cells now join in docling-parse index order (docling's
   `_sort_cells`), which fixes off-baseline glyph drift like 2206's inline
   math `>` landing on the wrong line.
4. **amt fraction double space (text-layer, strict-only)**
   ([#63](https://github.com/docling-project/docling.rs/issues/63)). docling boxes glyphs
   with the embedded font's OS/2 typographic metrics, not the PDF descriptor's;
   that ~0.3 pt difference makes its justified line insert a *second* space before
   the `1⁄4` numerator. Our single-spaced output is the more faithful rendering
   (the whitespace-normalized metric credits it); reproducing docling's exact
   spacing needs an embedded-font metrics layer, which globally entangles with RTL
   box geometry (a trial that fixed one `¼` regressed `right_to_left_01`). See
   `MIGRATION.md` §4. **Resolved as by-design:** our single space is the correct
   rendering, so #63 is closed without matching docling's spurious extra space —
   forcing a byte-match would degrade output and risk the RTL geometry.
