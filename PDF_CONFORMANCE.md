# PDF conformance

How close the Rust PDF pipeline gets to docling's **default** Markdown, measured
byte-for-byte against the committed groundtruth (`tests/data/pdf/groundtruth/*.md`).
The groundtruth is regenerated from **live published docling**, so it agrees with
`scripts/conformance.sh pdf`.

> Measure locally with `scripts/pdf_groundtruth.sh` (diffs the checked-in
> reference; no docling install needed) or `scripts/conformance.sh pdf` (installs
> docling and diffs against it). Both report two metrics: **strict** (byte-for-byte)
> and **whitespace-normalized** (spacing-only diffs ignored). Diff = changed lines
> vs the groundtruth (one changed line counts as 2).

## Current state

**6 / 14 strict** · **7 / 14 whitespace-normalized.**

| PDF | diff | dominant remaining blocker |
|---|---:|---|
| picture_classification | **exact** | — |
| code_and_formula | **exact** | — |
| multi_page | **exact** | — |
| 2305.03393v1-pg9 | **exact** | — (TableFormer table, cell-for-cell) |
| right_to_left_01 | **exact** | — (RTL period attachment) |
| right_to_left_02 | **exact** | — (kashida dedup + page-number layout) |
| amt_handbook_sample | 2 *(ws-ok)* | docling's spurious fraction double space — ours is more faithful |
| normal_4pages | 54 | reading order (heading numbering, footnote order) |
| right_to_left_03 | 66 | RTL bidi |
| 2305.03393v1 | 93 | title-page reading order + author-ID run spacing |
| table_mislabeled_as_picture | 108 | layout over-detects tables (survey rendered as tables) |
| 2206.01062 | 198 | TableFormer multi-row headers + title-page reading order |
| 2203.01017v2 | 209 | TableFormer structure + reading order |
| redp5110_sampled | 226 | TOC mis-classified as a picture; cover-page ordering |

`amt` is the 7th under the whitespace-normalized metric: its only diff is
docling's spurious double space before the `1⁄4` fraction, where our single-spaced
output is the more faithful rendering. The remaining non-exact PDFs are heavy
multi-column / table docs whose gaps are model-level (TableFormer structure,
layout classification, title-page reading order), not text-layer.

## How the pipeline works

pdfium extracts the glyph layer and renders each page to a bitmap; an ONNX stack
(layout detection, TableFormer, PaddleOCR) interprets it; regions are assembled in
reading order into a `DoclingDocument`. Tables use **TableFormer** (image encoder
+ autoregressive OTSL structure decoder + cell-bbox decoder, ported and exported
to ONNX in `tableformer.rs`) on a cv2-exact preprocessed crop (`resample.rs`); the
structure + matched cell text reproduce docling's padded GitHub tables (2305-pg9
is cell-for-cell exact).

### Performance / parallelism

Profiling a 14-page document (`FLEISCHWOLF_TIMING=1` prints an env-gated per-stage
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
`FLEISCHWOLF_PDF_WORKERS` (pool size) and `FLEISCHWOLF_PDF_INTRA` (intra-op threads
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
automatically, so scanned/OCR pages are unaffected. (pdfium still provides page
rasters and word/code cells for TableFormer.)

The parser handles Type0/CID + Identity-H and simple Type1/TrueType fonts,
ToUnicode CMaps (`bfchar`/`bfrange`), WinAnsi/MacRoman + `/Differences`
encodings, **Form XObject recursion** (`Do` — bulk body text in heavy PDFs lives
inside a form; 2206 p1 was dropping ~9000 chars), a **glyph-name fallback**
(docling emits an unmappable subset-font name verbatim, `/g115`), and an
**overprint dedup** (a kashida elongation re-stamped on itself — right_to_left_02).
A char-frequency validator (`scripts/parser_completeness.py`) confirms nothing is
silently skipped.

Its cells feed the ported **docling-parse line sanitizer** (`dp_lines.rs`, from
`src/parse/page_item_sanitators/cells.h`): a 3-pass corner-distance contraction
(LTR → RTL → LTR-reverse) with `merge_with` space insertion (one space when the
gap exceeds 0.33×avg-char-width, plus literal space glyphs), `enforce_same_font`,
ligature recomposition, and loose-box geometry. On the clean parser boxes it uses
the Euclidean corner gap (matching docling); on pdfium's loose boxes it keeps the
signed horizontal gap.

Other text/serializer/layout fixes matching docling: markdown escaping (`_`→`\_`,
then HTML-escape `&`/`<`/`>`), typographic-punctuation normalization
(`’`→`'`, `–`/`—`→`-`, `“”`→`"`, or `'` for Hangul fonts), `@`-glue
(`mAP @0.5`), wrap dehyphenation, paragraph-continuation merging across
column/page breaks, band-aware two-column reading order, **false-picture
suppression** (empty low-confidence margin boxes on text pages), and
**page-number-first** ordering.

## Remaining blockers (model-level)

These yield smaller or uncertain gains than the text-layer work already shipped.

1. **TableFormer structure on complex tables.** Multi-row headers / spans on the
   big papers (2206, 2203) differ from docling's OTSL prediction; one cell-
   structure diff cascades through the padded columns into many row diffs
   (2206's ~92 table-row diffs trace to ~4 structure diffs).
2. **Layout classification.** The layout ONNX classifies redp5110's
   table-of-contents as a *picture* (docling renders it as a table) and
   table_mislabeled's survey as *tables* (docling renders lists/text) — opposite
   classifications, not a text problem.
3. **Complex title-page reading order.** Author-block / abstract interleaving on
   the academic papers (band reading-order handles the full-width title; the
   in-column author/abstract order is still off).
4. **amt fraction double space (text-layer, strict-only).** docling boxes glyphs
   with the embedded font's OS/2 typographic metrics, not the PDF descriptor's;
   that ~0.3 pt difference makes its justified line insert a *second* space before
   the `1⁄4` numerator. Our single-spaced output is the more faithful rendering
   (the whitespace-normalized metric credits it); reproducing docling's exact
   spacing needs an embedded-font metrics layer, which globally entangles with RTL
   geometry. See `PDF_PARSER_NOTES.md`.
