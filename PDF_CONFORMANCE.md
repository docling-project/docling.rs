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

## Enrichment models (opt-in)

docling's optional enrichment stages are ported behind the same flags
(`--enrich-picture-classes` / `--enrich-code` / `--enrich-formula`, docling's
`do_picture_classification` / `do_code_enrichment` / `do_formula_enrichment`)
and validated by `scripts/conformance/enrich_conformance.sh` against Python
docling 2.112's output on the enrichment fixtures
(`tests/data/pdf/groundtruth-enriched/`):

| Fixture | Check | Result |
|---|---|---|
| code_and_formula.pdf | Markdown, `--enrich-code --enrich-formula` | **byte-exact** (CodeFormulaV2's code rewrite, `JavaScript` language, formula LaTeX) |
| picture_classification.pdf | JSON classification annotation + meta | same class ranking; confidences match to ~3 decimals |

The CodeFormulaV2 export (`scripts/install/export_code_formula.py`) verifies
its three ONNX graphs' greedy decode **token-identical** to
`transformers.generate` before writing them. Its decoder also ships as a
dynamic INT8 quantization (`scripts/install/quantize_models.py
code-formula-decoder`, ~655 → ~165 MB, 4× less decoder RAM) that is preferred
automatically when present (`DOCLING_RS_FP32=1` opts out). Unlike the layout /
TableFormer INT8 models it is *near*-exact rather than byte-exact: greedy VLM
decoding has near-tie tokens that weight rounding can flip — on the fixture
the only drift is one extra blank line in the code block, and per-channel /
fp32-lm_head variants flip it identically, so the smaller per-tensor file is
kept. The conformance script gates fp32 byte-exact and allows the int8 leg
whitespace-only drift. The residual confidence drift on
the classifier comes from the crops: docling re-renders each region through
pdfium at the enrichment scale, while docling.rs resizes from the existing
scale-2 page render — sub-pixel differences the classifier's softmax sees in
the third decimal, and that the VLM's argmax decoding absorbs entirely on the
fixtures.

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
per worker). Each worker layout-detects up to `DOCLING_RS_PDF_LAYOUT_BATCH`
already-rendered pages per inference call (issue #73; default 4 on 8+ cores,
1 below — measured on a 4-core box the batch costs pipeline overlap: 8.1 →
9.3 s/conv on 2206.01062). Output is bit-identical at every batch size, so
the knob is purely about throughput.

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

---

## Performance — review & profiling notes

Post-migration review of the PDF processing path: where the time actually goes,
what was measured, which optimizations are validated, and a ranked backlog of
further ideas that do **not** trade away output quality.

### Results at a glance

Everything below was landed across two optimization rounds (PR #26, #27),
each change gated on corpus conformance — groundtruth distance unchanged or
better, byte-identical where the change is structural:

| Optimization | Measured effect |
|---|---|
| INT8 layout model (Conv-only static QDQ, calibrated; **default**) | layout inference **2.4×** faster; **1.83× end-to-end** on a 1913-page PDF (0.74 → 0.40 s/page) |
| INT8 TableFormer decoder (dynamic, **default**) | ~10% faster table decode, byte-identical |
| SIMD page downscale (`fast_image_resize`, same kernel; **default**) | `image.resize` stage **17×** faster (2607 → 152 ms / 16 pages) |
| TableFormer KV cache fed back as `ort` values (no per-step copy) | ~9% faster table-structure decode, byte-identical |
| One shared lazy TableFormer across the worker pool | peak RSS **3.8 → 1.9 GB** (4 workers); table-free docs 682 → 331 MB |
| Single shared line/word contraction pass | `--no-ocr` conversion ~1.25× faster, identical output |
| Per-document font + form caches in the text parser | 3–10% off `textparse` here; far more on CJK/form-heavy PDFs |
| True-KV-cache decoder export (`decoder_kv.onnx`, optional) | parity at corpus table sizes; O(past)/step for very large tables |

Cumulative head-to-head vs Python docling (measured on an 8-thread desktop,
`scripts/test/performance.sh`): **4.3× faster warm conversion, 4.7× end-to-end,
2.3–2.6× less peak memory** on the PDF ML pipeline — up from ~1.2× warm
before this work. Model sizes: layout 172 → 68 MB, TF decoder 78 → 50 MB.
Also fixed along the way: the `"` show-text operator dropped its word/char
spacing operands (real spec violation), and OCR/TableFormer sub-stages are
now visible in `DOCLING_RS_TIMING` profiles.

Measured on a 4-core AVX-512(+VNNI/AMX) Xeon, release build (`lto = "thin"`),
models from `scripts/install/download_dependencies.sh`, `DOCLING_RS_TIMING=1`.

### Where the time goes

Per-stage wall-clock share (summed across workers):

| Stage | 1913-page text-heavy PDF¹ | 16-page table-heavy paper² | scanned page³ |
|---|---:|---:|---:|
| `layout.predict` (RT-DETR ONNX) | **80.3%** | 55.4% | 64.9% |
| `image.resize` (3×→2× CatmullRom) | 14.9% | 7.9% | 18.5% |
| `tableformer` | 2.8% | 32.1% | — |
| `pdfium.render` | 1.8% | 3.7% | 16.5% |
| `textparse` + assembly | ~0.2% | ~0.3% | ~0.1% |

¹ `tests/data/pdf/large/dotnet-csharp-language-reference.pdf` — 936 s wall, ~0.49 s/page.
² `tests/data/pdf/sources/2203.01017v2.pdf`.
³ `tests/data/scanned/sources/ocr_test.pdf`.

Two conclusions drive everything below:

1. **ONNX inference is ~85–95% of PDF conversion time.** All the Rust-side text
   extraction, parsing, and assembly work combined is under 1%. Rust-code
   micro-optimizations are irrelevant to PDF throughput until the models get
   faster; model-level and preprocessing-level changes are the only levers that
   matter.
2. Within TableFormer, the **autoregressive decode loop** dominates
   (`tableformer.structure` ≈ 96% of the stage; the per-table page resample
   `tableformer.inter_area` is ~1% of a conversion).

The worker-pool topology heuristic in `lib.rs` (`workers × intra ≈ cores`,
default 2×2 on 4 cores) was re-validated: 2×2 beat both 4×1 and 1×4 on the
16-page document (11.6 s vs 12.2 s vs 15.6 s).

### Validated win: INT8 quantization (quality-checked)

`scripts/install/quantize_models.py` produces two quantized models. Point
`DOCLING_LAYOUT_ONNX` / `DOCLING_TABLEFORMER_DECODER` at them to opt in.

**These are now the default:** when the `*_int8` files sit next to the fp32
models at the default paths, the pipeline loads them automatically.
`DOCLING_RS_FP32=1` forces full precision, and an explicit
`DOCLING_LAYOUT_ONNX` / `DOCLING_TABLEFORMER_DECODER` always wins (the
conformance/groundtruth scripts pin fp32 explicitly, so snapshots stay
deterministic).

#### Layout: static QDQ INT8, **Conv ops only** (~2.4× faster layout)

Calibrated on 42 real corpus pages preprocessed exactly like
`layout.rs::predict`. Only the HGNetv2 backbone convolutions are quantized;
the transformer decoder and detection-head MatMuls stay fp32.

| Configuration | layout.predict (16-page doc) | end-to-end wall | model size |
|---|---:|---:|---:|
| fp32 baseline | 17.2 s | 16.6 s | 172 MB |
| **INT8 conv-only** | **7.2 s (2.4×)** | 11.5 s (1.45×) | 68 MB |
| + INT8 TableFormer decoder | — | **12.3 s → see note** | — |

On text-dominated documents (layout = 80% of time) the end-to-end gain
approaches ~1.7–2×; on table-heavy ones it is ~1.4×.

Full-scale run — the 1913-page `dotnet-csharp-language-reference.pdf`,
INT8 layout + INT8 TableFormer decoder vs fp32, same machine and binary,
back-to-back:

| | fp32 | INT8 | ratio |
|---|---:|---:|---:|
| wall clock | 1406 s (0.74 s/page) | **770 s (0.40 s/page)** | **1.83×** |
| `layout.predict` (summed) | 2667 s | 1350 s | 1.98× |
| output difference | — | 1199 of 52,615 Markdown lines (2.3%) | |

The 2.3% of differing lines are the same near-threshold classification flips
seen on the corpus (where groundtruth conformance measured *equal or slightly
better* under INT8 — 812 vs 833 summed diff-lines), not a systematic
degradation. With layout halved, `image.resize` becomes the next stage
(24.8% of the INT8 run), which is why backlog item 4 matters more after
quantization.

**Quality gate.** Markdown diffed across the full PDF+scanned corpus (23 files):

- Conv-only INT8: 12/23 byte-identical to fp32; remaining diffs are small
  region-classification flips. Against the committed groundtruth the summed
  diff-line distance is **812 (INT8) vs 833 (fp32)** — i.e. conformance-neutral
  (INT8 is marginally better on 3 fixtures, marginally worse on 2).
- Full INT8 (convs + MatMuls) was **rejected**: 3/23 exact, with clear quality
  loss (section headers demoted to plain text, page-footer text leaking into
  the output) — the RT-DETR head's class scores sit near the 0.3 threshold and
  cannot tolerate activation quantization.
- Dynamic (weights-only) INT8 of the whole layout model was also rejected: it
  is *slower* than fp32 (3.2 s vs 2.1 s per page-with-table) because inserted
  per-activation quantize ops outweigh the MatMul savings while the conv
  backbone stays fp32.

#### TableFormer decoder: dynamic INT8 (~10% faster tables, byte-identical)

The autoregressive tag decoder is MatMul-only; weights-only dynamic INT8
produced **byte-identical corpus output** and ~10% faster table decode
(784 → 695 ms/table), 78 → 50 MB. Small but free.

The decoder speed is *not* weight-bound — it is per-step overhead (see backlog
item 2), which is why quantization helps so little there.

### Ranked backlog of further ideas

Ordered by expected impact ÷ risk. Items 1–3 attack the 85–95%.

1. ~~**Ship/document the INT8 layout model as the default CPU
   configuration**~~ **Done on this branch:** the pipeline prefers the int8
   models when present (`DOCLING_RS_FP32=1` opts out),
   `download_dependencies.sh` fetches them by default, and
   `publish-models.yml` builds them. Biggest single validated win: ~1.4–2×
   end-to-end.
2. **TableFormer decode-loop overhead** (~800 ms/table, ~60–500 steps):
   - ~~`decode_step` copies the whole KV cache out (`ocache.to_vec()`) and back
     in every step — O(steps²·6·512) float traffic.~~ **Done on this branch:**
     the cache and the encoder's cross-K/V + `enc_out` stay owned `ort` values
     fed straight back into the next run (~9% faster structure decode,
     byte-identical output).
   - ~~The exported graph still re-embeds the **full tag sequence** every
     step.~~ **Built and measured:** `scripts/install/export_tableformer.py` now also
     exports `decoder_kv.onnx`, a true-KV-cache step (one tag in, projected
     K/V cached per layer), verified argmax-identical over a 64-step rollout
     and byte-identical on corpus output. Measured result: **parity** with
     the legacy graph on corpus-sized tables (~100–300 tokens) — ONNX Runtime
     executes the legacy graph's full-prefix re-projection as one efficient
     batched GEMM, so the O(n²) FLOPs don't become O(n²) wall time until
     tables get much larger. The Rust loop auto-detects either graph (input
     names) and prefers the smaller legacy file by default; point
     `DOCLING_TABLEFORMER_DECODER` at `decoder_kv(_int8).onnx` for
     very-large-table workloads.
3. ~~**Layout batching for the parallel path**: the pool currently runs batch-1
   inference per page.~~ **Done (issue #73)**: each pool worker drains the work
   channel opportunistically (whatever is already rendered, up to
   `DOCLING_RS_PDF_LAYOUT_BATCH` — default 4 on 8+ cores, 1 below) and
   layout-detects the batch with one inference call — batching never *waits* for pages, so it adds no
   latency when rendering is the bottleneck. Needs the dynamic-batch ONNX
   export (`scripts/install/export_layout.py`); an old fixed-batch graph
   triggers a warn-once per-page fallback. Two export subtleties keep numerics
   identical to the historical static export: a plain `dynamic_axes` export
   leaves the AIFI sincos position embedding as runtime ops that drift ~1e-6
   from the torch-folded constant (enough to flip borderline detections
   corpus-wide — groundtruth exact matches dropped 5/14 → 0/14 before the
   fix), so the exporter folds the static graph's position-embedding subgraph
   offline and splices the constant into the dynamic graph. Verified:
   groundtruth parity restored (5/14 exact, 6/14 normalized), and batch=1 ==
   batch=4 **bit-identical** across the whole corpus.
4. **The 3×→2× page downscale** (~15% of a text-heavy conversion, ~25% after
   INT8): ~~replace the scalar `image`-crate CatmullRom with a SIMD
   convolution.~~ **Done on this branch:** `fast_image_resize` with the same
   a=-0.5 Catmull-Rom kernel — `image.resize` drops **2607 → 152 ms (17×)**
   on the 16-page doc. The SIMD fixed-point path differs from the scalar one
   by ±1/255 on some pixels, which can flip borderline table cells, so it was
   gated like INT8: groundtruth distance over the corpus is **817 (SIMD) vs
   818 (scalar)** — conformance-neutral. `DOCLING_RS_SLOW_RESIZE=1` restores
   the scalar path, and `pdf_conformance.sh`/`pdf_groundtruth.sh` pin it so
   the committed snapshot baselines stay valid. (The render-side `as_image()`
   copy turned out to be a non-issue: pdfium already renders with reversed
   byte order, so it is one memcpy + one 4→3-channel pass, ~1% of total.)
5. **textparse font caching** (marginal for PDFs — textparse is ≤1% — but
   real for `no_ocr` mode where it becomes the bottleneck):
   - ~~fonts are fully re-parsed for **every page** and every Form-XObject
     invocation; decoded form content re-inflated per `Do`.~~ **Done on this
     branch:** per-document caches keyed by object id (fonts also by resource
     name, which feeds the docling-parse font hash). Identical output across
     the corpus; 3–10% off the `textparse` stage on the test fixtures (their
     ToUnicode CMaps are small — CJK/form-heavy documents benefit far more).
   - ~~`line_cells` + `word_cells` run the identical build+contract twice per
     page; one pass can emit both.~~ **Done on this branch**
     (`dp_lines::line_and_word_cells`): ~1.25× faster `--no-ocr` conversion,
     identical output.
   - `decode_code`/`decompose_ligatures` allocate a `String` per glyph
     (`textparse.rs:94-145`); decompose once at font-parse time and return
     borrowed `&str`.
   - RTL merge is O(n²) (string prepend + `Vec::remove(0)`,
     `dp_lines.rs:87-155`); accumulate reversed and flip once per line.
6. ~~**OCR line batching** (`ocr.rs::recognize`): lines are recognized one at
   a time on one thread (deliberately, for CTC determinism). Batching
   same-width buckets keeps determinism per line.~~ **Done on this branch**
   (`ocr.rs::recognize_batch`): each page's line crops are gathered first and
   equal-width lines share one recognition run (page order, batches capped at
   16). Same-width batching is **bit-identical** to sequential runs (verified:
   max output diff 0.0 over the scanned corpus's crops); the snapshot corpus
   is unchanged. Measured `ocr.page`: 195 → 176 ms on `ocr_test.pdf`, 682 →
   587 ms over `nemotron_multipage.pdf`'s 4 pages (−10–14%). The
   "several-fold" hope required *padded* batches (PaddleOCR-style, pad to
   bucket max): measured on the real crops, padding perturbs the valid
   region's probabilities by up to 0.34 through the model's global-attention
   blocks and changes the decoded text on 16/20 lines — off the table for a
   byte-stable pipeline. The remaining lever is running same-width buckets
   across the page-worker pool's idle threads (needs one extra session per
   worker: `ort`'s `Session::run` takes `&mut self`).
7. **ort session options**: checked — ONNX Runtime's C-API default is already
   `ORT_ENABLE_ALL`, so an explicit optimization level gains nothing.
   `with_optimized_model_path` (caching the optimized graph on disk) could
   still shave per-worker model-load latency; only worth it if pool spin-up
   shows up in a real deployment.

### Memory

Each pool worker used to own a full model set, so peak RSS scaled with the
pool: on a 4-worker machine ~0.4 GB of TableFormer weights+arenas were
duplicated four times even though tables appear on a minority of pages. The
pool now shares **one lazily-loaded TableFormer** behind a mutex (loaded with
the full intra-op budget, since tables serialise on it anyway; prediction is
independent of which worker runs it). Measured on the 16-page table-heavy
paper, INT8 stack:

| pool | per-worker TF (before) | shared TF (after) |
|---|---:|---:|
| 4 workers | 3816 MB | **1880 MB** |
| 2 workers | 2183 MB | **1517 MB** |
| 4 workers, table-free doc | 682 MB | **331 MB** (TableFormer never loads) |

`DOCLING_RS_PDF_WORKERS` remains the coarse memory knob on top.

### Determinism note (pre-existing, worth knowing)

Multi-threaded ONNX Runtime float reductions are **not deterministic
run-to-run**: on `2203.01017v2.pdf` two identical invocations of the same
binary can differ in a handful of borderline table cells (measured 0–20
Markdown diff-lines between repeat runs, before any of this branch's
changes). `ocr.rs` already pins its session to one thread for exactly this
reason. Regression checks for structural changes should therefore compare
outputs under `DOCLING_RS_PDF_THREADS=1` (single-thread inference is
deterministic and byte-stable); multi-threaded corpus diffs of a few lines on
table-dense fixtures are thread-scheduling jitter, not necessarily a real
change.

### Correctness notes found during review (quality, not speed)

- `textparse.rs` `"` operator: the `aw ac string "` form must set word/char
  spacing (`tw`/`tc`) from its first two operands before showing the string;
  they are currently ignored (`Tj | ' | "` share one arm), so documents using
  `"` get wrong inter-word advances. **Fixed in this branch.**
- `textparse.rs::page_size` ignores a non-zero MediaBox origin; a page with
  e.g. `[9 9 621 801]` offsets all parser cells relative to pdfium's raster.
  Rare, but cheap to guard: subtract the box origin when emitting glyph boxes.
- OCR recognition ran un-instrumented; `ocr.page` is now a timed stage (this
  branch), so scanned-corpus profiles attribute it correctly.

### Reproducing

```bash
scripts/install/download_dependencies.sh
cargo build --release

# stage timing
DOCLING_RS_TIMING=1 ./target/release/docling-rs input.pdf > /dev/null

# build the int8 models (used automatically once present)
uv venv .venv-quant && uv pip install --python .venv-quant/bin/python \
    onnx onnxruntime sympy pypdfium2 pillow numpy
.venv-quant/bin/python scripts/install/quantize_models.py

# force full precision for a run
DOCLING_RS_FP32=1 ./target/release/docling-rs input.pdf > /dev/null
```

Integration points: `scripts/install/download_dependencies.sh` fetches the
pre-quantized assets by default (`--no-int8` skips; published by
`.github/workflows/publish-models.yml`, which quantizes after export);
`scripts/install/pdf_setup.sh` quantizes locally unless `DOCLING_RS_FP32=1`;
`scripts/test/performance.sh` benchmarks whatever the pipeline default resolves to
(int8 when present, `DOCLING_RS_FP32=1` for fp32); `examples/Dockerfile`
bakes both precisions and defaults to int8 (`--build-arg INT8=0` for fp32).
