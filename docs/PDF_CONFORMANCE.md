# PDF conformance

How close the Rust PDF pipeline gets to docling's **default** Markdown, measured
byte-for-byte against the committed groundtruth (`tests/data/pdf/groundtruth/*.md`).
The groundtruth **mirrors upstream docling's own `tests/data/pdf/groundtruth`**
(taken at docling v2.130.0-26; upstream last regenerated it on 2026-09-10 for
docling-ibm-models 4.0.1 — docling#4122/#4200/#4213), which docling writes with
its default backend since 2.123 (threaded docling-parse, its own page renderer)
and `do_ocr=False` (`tests/test_e2e_conversion.py`: layout + TableFormer with
cell matching, CPU). The numbers in this document are measured with
`scripts/conformance/pdf_groundtruth.sh`, which runs the pipeline the same way —
`--skip-ocr`, the model inputs rendered by docling-parse's renderer
(`DOCLING_RS_RENDERER=docling-parse`, #478) — and pins the conformance model
set (fp32 layout/OCR env overrides below); `scripts/conformance/conformance.sh
pdf` runs the *default* (int8, English-OCR) models over every source PDF, so
its totals differ from this table. Before this refresh the groundtruth was an
older, pypdfium2-era docling's and the pipeline was scored with OCR on.

> Measure locally with `scripts/conformance/pdf_groundtruth.sh` (diffs the checked-in
> reference; no docling install needed) or `scripts/conformance/conformance.sh pdf` (installs
> docling and diffs against it). Both report two metrics: **strict** (byte-for-byte)
> and **whitespace-normalized** (spacing-only diffs ignored). Diff = changed lines
> vs the groundtruth (one changed line counts as 2).
>
> CI runs both too (`.github/workflows/pdf-conformance.yml`): on every PR that
> touches the PDF stack, the serializers, the fixtures or the baselines, it
> fetches the models and the docling-parse shim, regenerates `tests/snapshots/`
> and fails on drift — the regenerated tree is the run's artifact, so a
> baseline is refreshed from it without a local model setup — and prints the
> groundtruth numbers, warning when the strict count no longer matches the
> headline above. Before it, a change that moved the output could land with a
> stale snapshot (#814 found `right_to_left_03`'s that way).

## Current state

**10 / 18 strict** · **11 / 18 whitespace-normalized** against upstream's
current groundtruth (docling ≥ 2.123: docling-parse render, `do_ocr=False`,
`compact_tables=True`; 18 fixtures — `table_misidentified_as_form`
(docling#4064) joined the corpus with this refresh). Total 328 diff lines with
the docling-parse renderer the baselines are measured with (374 before #528
recovered redp5110's rotated table headers, 346 before #609 kept glyphs stacked
at one x apart, 342 before #813 ported docling's caption matcher — 2203.01017v2
19 → 17 and 2206.01062 14 → 10, the captions docling leaves loose now stay
loose here too, and `right_to_left_03` reads exact; 330 before the text
parser learnt the Adobe Glyph List's Latin names — 2203.01017v2's `/Lslash`
reference author, 17 → 15); the default build's pure-Rust
renderer scored 454 on the same files before that change ("The PDF stack"
below). (The two Korean image-only pages `skipped_1page` /
`skipped_2pages` carry no text groundtruth and are not scored.)

| PDF | diff | dominant remaining blocker |
|---|---:|---|
| picture_classification | **exact** | — |
| multi_page | **exact** | — |
| 2305.03393v1-pg9 | **exact** | — (TableFormer table, cell-for-cell) |
| right_to_left_01 | **exact** | — (RTL period attachment) |
| right_to_left_02 | **exact** | — (kashida dedup + page-number layout) |
| base14_fonts_rot90 / _rot180 / _rot270 | **exact** | — (`/Rotate` display-frame normalization, docling#4008; our own fixtures, groundtruth = the unrotated upstream file) |
| code_and_formula | **exact** | — (flat legacy code, line-preserving `pretty` in strict) |
| amt_handbook_sample | 2 *(ws-ok)* | docling's spurious fraction double space — ours is more faithful |
| right_to_left_03 | **exact** | — (bidi run order of a numbered heading) |
| 2305.03393v1 | 4 | one author-block cluster (model-borderline) |
| normal_4pages | 12 | docling folds the page-number glyph into the heading cluster (`## 들어가며 1`) where we emit it as its own item, and the `※` footnotes order |
| 2203.01017v2 | 15 | the `c. Structure predicted by TableFormer:` item's slot under Figure 1, accent spacing in the references (`Herv´e D´ejean`) |
| 2206.01062 | 10 | author-block cluster splits (model-borderline), Table 5's merged first column |
| table_misidentified_as_form | 48 | the form container's nested table / picture (docling#4064): docling nests them inside the form region and keeps the heading, we flatten the region |
| table_mislabeled_as_picture | 85 | the survey over-detected as tables; docling keeps a cell's leading indentation (`\|   They work in parallel…`) |
| redp5110_sampled | 152 | TOC: docling's cell matching puts the dot leaders into the page-number column (`. . . . . . . vii`), ours keeps them with the title (`. vii`); cover-page ordering |

Measured on the current tree with `scripts/conformance/pdf_groundtruth.sh`
(`--skip-ocr --compact-tables`, docling-parse renderer). Two ports landed with
this refresh because the new groundtruth surfaced them: docling's
`ListItemMarkerProcessor` (a leading `-`/bullet/`N.`/`a)` marker is split off
the item text — 2305's OTSL list reads `- "C" cell`, not `- - "C" cell`; a
compound `3.a.` marker rides in the text, `- 3.a. If all…`), and
`--compact-tables` on the streaming Markdown path (it reached only
`--no-stream` before). Against the previous, pypdfium2-era groundtruth this
tree scored 9/17 strict.

## The PDF stack: what the models see, and what renders it

docling ≥ 2.123 converts through `ThreadedDoclingParseDocumentBackend`:
every page image its layout, TableFormer, OCR and enrichment stages consume
comes out of docling-parse's **own renderer** (`blend2d_renderer.h`: glyph
outlines from FreeType, filled by Blend2D's analytic anti-aliasing, embedded
programs first and its bundled fallback faces otherwise, no supersampling).
pdfium — the `PyPdfiumDocumentBackend` chain this pipeline used to reproduce,
1.5× render + PIL-BICUBIC — is a backend docling still ships but no longer
runs by default, and the two renders differ on ~10 % of a text page's pixels
(the anti-aliasing of every glyph edge and hairline, not the content): on a
borderline region heron labels a whole table the other way (#478).

The pipeline is therefore built around docling-parse's frame, and nothing in
the default build is a native PDF library:

| need | answered by |
|---|---|
| page count, geometry, `/Rotate`, link annotations | `pdf_meta.rs` — the lopdf object model, `CropBox ∩ MediaBox` with pdfium's fallbacks, rotation normalized as `CPDF_Page::GetPageRotation` does; checked identical to pdfium on every corpus page (`pdfium_backend::tests::pdf_meta_matches_pdfium_on_the_corpus`, `--features pdfium`) |
| the text layer | `textparse.rs` — the only source since the pdfium text fallback was measured to add nothing on any page of the corpus and removed (the parser and pdfium were empty on exactly the same 38 pages) |
| an image-only page's bitmap (scans) | `raster/` — pdfium's rendering of such a page **byte for byte** (`CStretchEngine`, libjpeg-exact `DCTDecode` incl. the reduced IDCTs, `FaxDecoder`, stencil masks, `LoadPalette`): 22/22 fixture renders and 165/165 synthesized pages identical to `FPDF_RenderPageBitmap`. It decodes `JBIG2Decode` scans (#811, through the renderer's decoder, the packed bitmap pdfium's JBig2 module hands `CPDF_DIB`) and declines (and the page renderer below draws) JPX (decoded there, #598), CMYK/Lab/Separation/DeviceN and non-sRGB ICC images, `/SMask` and colour-key masks, 16-bit samples, non-axis-aligned placements, transparency groups, pages with drawn paths or visible text |
| the model inputs of every other page | `render/` — the pure-Rust page renderer, docling-parse's twin (below); `DOCLING_RS_RENDERER=docling-parse` takes them from docling-parse's renderer itself through the `dlopen`ed shim |
| a file lopdf cannot read; `DOCLING_RS_RENDERER=pdfium` | pdfium, only in a build with the opt-in `pdfium` cargo feature (`docling-pdf`, forwarded by `docling`, `docling-cli`, `docling-serve`); without it the choice warns once and renders in Rust, and an unreadable object model fails with the install hint |

**Encrypted PDFs** open through lopdf: the empty user password most
"protected" files carry is tried silently, the document's password
(`--password`, also docling's `--pdf-password` — the `password` option on
every surface since #611/#625, `PdfMeta::open_with_password`) decrypts at load,
and a missing one is the error docling raises (`pdf: the PDF is encrypted: a
password is required` — the `pdf_password` snapshot records it; a wrong one
says `… and the password is wrong`).

### The renderer (`crates/docling-pdf/src/render/`)

≈9,000 lines reproducing docling-parse's drawing rules in its frame:

* **Canvas and frame** (`render/mod.rs`): `ceil(extent − 1e-6)` pixels per
  side, the crop box stretched onto the whole canvas, drawn unrotated and
  `/Rotate` applied to the finished pixels, premultiplied RGBA over white →
  RGB. A `Renderer` owns one document's caches (parsed fonts, the CMYK table,
  decoded image samples under a 256 MB budget) across the two scales the
  pipeline renders every page at.
* **Content streams** (`render/content.rs`): the graphics state, paths and
  their painting operators, clipping (a rectangle clip is a box
  intersection; any other path a coverage mask over the shape's box, cached
  by path hash), ExtGState alpha / blend modes / line parameters, colour
  operators over DeviceGray/RGB/CMYK, Indexed, Lab, ICCBased (by `/N`),
  Separation / DeviceN (PDF functions of types 0/2/3/4 in
  `render/function.rs`) and Pattern spaces; text objects with every
  `Tf`/`Td`/`TJ`/`Tz`/`Ts`/`Tr` rule; Form XObjects; transparency groups
  flattened the way `enter_transparency_group` does (no soft masks);
  shadings of all seven types and tiling patterns; Widget annotation
  appearances (the only annotations docling-parse draws); and the details a
  byte oracle exposed — minimum stroke width one pixel, line width scaled by
  √|det CTM|, dashes drawn, CMYK through docling-parse's Yule–Nielsen
  Neugebauer model (`render/color.rs`; pure K is (35, 31, 32)), an
  unresolvable glyph as its thin blue box.
* **Fonts** (`render/font/`): TrueType/OpenType and bare CFF through
  `ttf-parser` — with the Type 2 `dotsection` operator (`12 0`, a Type 1
  hint Adobe's converters leave in front of every dot) stripped from the
  charstrings first (`dotsection.rs`, #531): the spec says to ignore it, but
  ttf-parser abandons the glyph there, which drew no period and dot-less
  `i`s, so FrameMaker index pages lost their dot leaders and heron read
  them as one page-wide `table` instead of `document_index` columns —,
  Type 1 through an own charstring interpreter (`type1.rs`),
  Type 3 through the glyph procedures, selected per ISO 32000-1 9.6.6 /
  9.7.4 (`/Differences`, base encodings, `cmap` subtable rules, `post`
  names, predefined and embedded CMaps, `/CIDToGIDMap`); fonts without a
  program draw from a host face (`fallback.rs`: `.models/fonts`, then the
  Liberation / DejaVu / URW / Noto directories, `DOCLING_RS_FONT_DIRS` adds
  more, `DOCLING_RS_SYSTEM_FONTS=0` leaves the host directories out so the
  render is the deployment's alone, #633; a host without fonts draws each glyph's box — `download_dependencies.sh
  --with-fonts` fetches Liberation + DejaVu, the Docker images install the
  same two packages).
* **Images** (`render/image.rs`): the raster's decoders behind a general
  sample reader (1–16 bpc, every colour space above, `/Decode`, `/SMask`,
  stencil and colour-key masks, `/ImageMask` stencils in the fill colour),
  a JPEG decoded no larger than the page needs — docling-parse's
  `codec_reduction_shift` against its `bitmap_target_pixels_per_unit` (1.0,
  docling's `render_scale`): a 300 dpi scan is decoded at a quarter through
  libjpeg's reduced IDCT and blitted *up* onto the scale-2 canvas — then
  reduced by its integer factor `fx = sw / dst_w` with premultiplied box
  averaging before the bilinear blit; a JPEG 2000 image (`JPXDecode`) is
  decoded by `hayro-jpeg2000` (`render/jpx.rs`, #598 — within ±1 of OpenJPEG
  on the NASA scans; the reduction hint becomes its target resolution, the
  codestream's colour space stands in for a missing `/ColorSpace`, its alpha
  channel is the soft mask under `/SMaskInData`), where it used to be a
  mid-gray placeholder that left the layout model a blank rectangle: the
  picture box lost its confidence, ran over the caption below, and two
  stacked photos fused — docling-parse decodes JPX, so the shim run never
  showed it. JBIG2 decodes too since #811 (`hayro-jbig2`, the same
  project's decoder, `/JBIG2Globals` included): a scanned page's text is
  usually a JBIG2 `/ImageMask` stencil over a JPEG background, and without a
  decoder the stencil drew *nothing* — not even the placeholder — so the page
  rendered as its background alone, blank to the layout model and OCR. A
  stencil no decoder can read now draws the mid-gray placeholder like any
  other undecodable image. The one bitmap that does not follow the hint is
  a text-less page's, which goes to OCR: it decodes at full size
  (`Renderer::render_with_hint(…, 0.0)`) so the recognizer reads the scan's
  resolution, not a quarter of it blitted up — the same deliberate deviation
  the pdfium-exact raster of a scan makes under the shim.
* **A content pre-pass** (`render/prepass.rs`) for what lopdf's content
  lexer does not hand over: inline images with abbreviated or filtered
  parameters, and the Type 3 operators `d0`/`d1`.

**Measured against the shim** (`render::tests::against_the_docling_parse_shim`,
run when `.docling-parse/lib` is present — every PDF the snapshot corpus is
generated from, at scales 1.0 and 2.0; `examples/render_compare.rs` for one
page with a diff image): **334 renders, mean |Δ| 0.84 / 255 per channel,
worst 3.30** (`otsl_proof_v3`, a dense LaTeX figure: glyph edges). Before
the reduced JPEG decode the pdf fixtures alone measured 1.05 / 5.89, the
worst page a scan drawn from its full-size decode. The renderer is *not*
byte-identical to Blend2D — its analytic rasterizer, FreeType's hinting-free
outlines and its JIT compositor round differently from tiny-skia, and
Blend2D's bilinear weights are 8-bit — so the shim stays the reference the
baselines are pinned to and the test gates a regression (worst < 10,
mean < 2), not identity. What that gap does to the models, scored against
the docling-parse-rendered baselines with the two conformance scripts:

| model inputs rendered by | snapshots exact (98) | groundtruth (18 files) |
|---|---|---|
| docling-parse shim (`DOCLING_RS_RENDERER=docling-parse`, the reference) | 98 | 374 diff lines, 9 strict |
| **Rust renderer** (the default) | 60 | 429 diff lines, 9 strict |

Heron's borderline labels move with ±1/255 of anti-aliasing, so the
snapshots that drift are the ML-borderline ones: the matplotlib heat-map
figures of `2412.19437`, whose tick-label soup TableFormer reads as
differently shaped grids (131–139 lines each), `redp5110`'s TOC (103),
`html_v_otsl_intro_v2` (76), `2203.01017v2` (45). (Re-measured with #531,
which made the Rust render draw the TOC's dot leaders — the CFF
`dotsection` fix in "Fonts" above. That moved `redp5110`'s render closer
to the shim on every page — the TOC's mean |Δ| 1.92 → 1.07 — and left the
counts where they were: 60/98 either way, the TOC table a borderline read
for TableFormer under both renders (98 → 103 snapshot lines, 158 → 161
groundtruth lines). The 72/98 and 454 lines recorded before came from an
older tree; the drop happened between that run and this one, not in #531.) What separates the two
renders there is the anti-aliasing of text, not its placement: the ink per
glyph is the same (17.94 vs 17.71 mean levels on `2206.01062` p2) and there
is no sub-pixel offset (the best-aligned shift is zero), but tiny-skia puts
it on fewer, darker edge pixels — 6.6 % vs 5.5 % of the page below mid-gray
— and heron's borderline scores follow that. What would close the rest:
Blend2D's analytic coverage accumulation and its 8-bit bilinear weights,
FreeType's outline flattening, and docling-parse's font-similarity resolver
with its bundled faces where a PDF embeds no program — each a measurable
step on the same test.

Synthetic pages (`render::synthetic`, 18 tests) pin the frame mapping, the
crop box, `/Rotate`, strokes, alpha, clips, images with a stencil, shadings,
tiling patterns, form `/Matrix` + `/BBox`, group alpha, widget flags,
fallback-face text, Type 3 procedures, the CMYK model and cache reuse. Cost
in release (scale 2.0): a plain text page 50–60 ms (the shim 70–80),
`2206.01062` p1 with its 10k glyphs and six images 200 ms (shim 470), the
clip-heavy `amt_handbook_sample` p1 190 ms (shim 220); `DOCLING_RS_TIMING=1`
breaks it down (`render.content`, `render.glyph_fill`, `render.apply_clip`,
`render.clip_mask`, `render.sh`, `render.pattern_fill`, `render.image`,
`render.image_decode`).

### Development oracles

* **The docling-parse shim** (`.docling-parse/lib/libdparse_render.so` +
  `pdf_resources/`): docling-parse v7.22.1 built with one extra target, a C
  ABI over `pdf_decoder<DOCUMENT>` + `renderer<BLEND2D>`
  (`crates/docling-pdf/ffi/docling-parse-render/dparse_render.cpp`,
  reproducing `docling_threaded_renderer::worker_loop`). `download_dependencies.sh
  --with-docling-parse` fetches it from the models release
  (`docling-parse-render.yml` builds it per platform),
  `scripts/install/build_docling_parse_render.sh` builds it locally.
  `scripts/conformance/dparse_render_check.py` compared it with the Python
  package's `PageParseResult.get_image`: 176/176 corpus renders
  byte-identical, so what the conformance scripts score is exactly docling's
  input. The pipeline loads it only under `DOCLING_RS_RENDERER=docling-parse`,
  which `pdf_conformance.sh` and `pdf_groundtruth.sh` set because the
  baselines are its renders. One deliberate deviation inside that path: a
  page without a text layer keeps the Rust raster's (pdfium's) bitmap for
  OCR — Blend2D's blit of a scan reads `JsON` where pdfium's reads `JSON` —
  while its layout image is docling-parse's.
* **pdfium** (`.pdfium/lib/libpdfium.so`, `libpdfium.dylib` on macOS): only a
  build with the `pdfium` feature loads it — for `DOCLING_RS_RENDERER=pdfium`
  (docling's pypdfium2 chain), for a file lopdf cannot read, and for the
  raster / object-model oracle tests (`raster::tests::matches_pdfium_on_the_scanned_fixtures`,
  `synthesized_pages_match_pdfium`, `pdf_meta_matches_pdfium_on_the_corpus`).
  `publish-models.yml` keeps re-hosting it for those tests.

## Layout post-processing and assembly: what is ported, and where

docling's `LayoutPostprocessor`, `ReadingOrderPredictor` and
`PageAssembleModel` are ported rule by rule; each landed as a deterministic
snapshot update and is measured against the groundtruth table above. The
table maps the upstream rule to the code that carries it (the per-fixture
diff history of each port lives in the git log of these files).

| upstream rule | what it does here | where |
|---|---|---|
| per-label confidence thresholds, bucketed overlap resolution (regular / picture / wrapper), `_handle_cross_type_overlaps` (docling#4059: a coincident pair within 0.1 confidence keeps the richer label) | the raw RT-DETR detections are thresholded, then one survivor per overlapping group per bucket; a regular region absorbed by a table / index / picture is dropped so it isn't emitted twice | `layout::label_threshold`, `assemble::resolve`, `handle_cross_type_overlaps`, `drop_contained_regulars` |
| full-page picture filter (upstream since 2.15) | a `picture` covering > 90 % of the page is the page, not a figure: dropped before overlap resolution so a whole-page diagram's labels read out as text (an image input without text is an empty document, as docling's is) | `assemble::drop_full_page_pictures` |
| same-label picture dedup, `_remove_overlapping_clusters(tables, "wrapper")` | a figure detected whole and as sub-panels collapses to the whole box; tables whose boxes overlap (IoU > 0.8 or 80 % containment) keep one survivor (`area_threshold` 2.0, `conf_threshold` 0.2) | `assemble::dedup_pictures`, `remove_overlapping_specials` |
| `_assign_cells_to_clusters`, `_find_unassigned_cells` | exclusive cell assignment: each non-empty cell goes to the single best-overlapping regular region at > 0.2 intersection-over-self; cells no regular claims become orphan text regions; orphans > 80 % inside a picture or table are that special's children (a picture's are nested under it in the JSON — `Node::PictureChildren` — and left out of the reading order, like upstream's `_set_cluster_children`) | `assemble::add_orphan_regions`, `picture_parents` |
| `_adjust_cluster_bboxes`, `keep_empty_clusters=False`, the three merge rounds (#419) | once cells are final, every regular region is fitted to its cells' union, an empty regular region is dropped, and orphans inside a fitted box fold in — before TableFormer and the reading order, which never see the raw model boxes | `assemble::fit_regions_to_cells` |
| OCR'd pages: `_should_prefer_cluster` / `_select_best_cluster_from_group` (union-find over IoU > 0.8 or 80 % containment) | region-scoped OCR recognizes each region's crop, so a paragraph box over its own line boxes would read the ink twice: such groups collapse to one survivor on the group's union box | `assemble::merge_overlapping_regulars` |
| RapidOCR's crops: the recognizer reads the detector's boxes (#570) | inside each text/table region the DB detector's boxes (center in the region, taken whole) are the recognizer's line crops; a region the detector saw nothing in, or every region without the model, is split into ink-projection strips (the pre-#570 line source, `DOCLING_RS_OCR_LINES=projection`); RapidOCR's `text_score` (0.5) drops low-confidence lines. FUNSD word recall 0.57 → 0.86 on 30 forms, 0.61 → 0.90 on all 199 (Python docling 2.133: 0.85) with the PP-OCRv6 recognizer (`.models/ocr_rec_v6.onnx`, preferred when present) | `ocr_prep::prep_region_lines_det`, `prep_table_words_det`, `ocr_prep::text_score`, `ocr::resolve_rec_pair` |
| `_sort_cells` (docling-parse index order), `PageAssembleModel.sanitize_text` (docling#4052) | a region's cells serialize in source order; lines join with a space except after an *attached* dash, which fuses the wrapped word; a detached dash is kept | `assemble::cells_text` |
| `predict_merges` (docling#3888) | cross-column / cross-page paragraph continuations (a hard hyphen before a lowercase continuation joins without it; the head test accepts a trailing comma; tables are in the skip set) | `assemble::merge_continuations` |
| `_init_l2r_map` / `_init_ud_map` (docling#4093, 2.124) | two elements consecutive in assembly order, left strictly of right on one row (vertical IoU > 0.8), are linked left→right and a row is read through before the paragraph below it; the assembly rank is the region's first source cell (a table's or picture's first *interior* cell) | `reading_order::init_l2r` / `init_ud`, `assemble::cluster_cids` |
| `_find_to_captions` — `_rank_caption_candidates` + `_match_caption` (#265, #813) | a `caption` reaches the graphics (`picture`, `table` / `document_index`, `code`) in the unbroken run on either side of it in reading order — above *and* below, never across intervening text or the gutter — ranked by the shortest box gap (ties to the preceding graphic), and the graphics are matched by augmenting paths, a caption with a second choice yielding to one with none; #813 was the picture arm only looking *below* its figure, so a `Figure N:` label printed above never bound. Caption text is markdown-escaped on every arm | `assemble::pair_graphic_captions` |
| `form` / `key_value_region` containers (docling#4064) | everything > 80 % inside is a child, ordered among itself and emitted as one block where the container sits (ordering only — the children stay top-level items, no `form_area` group node) | `assemble::order_with_containers` |
| a picture ≥ 80 % inside a TableFormer table (docling#3906) | nested in the cell covering it: the cell reads `text  <!-- image -->`, the JSON grid keeps the text | `assemble::match_table_pictures` |
| `ListItemMarkerProcessor` | a leading `-` / bullet / `N.` / `a)` marker is split off the item text; a compound `3.a.` rides in the text | `docling-core` list-item processing |
| `_match_hyperlink` | the URI whose annotation rects cover ≥ 0.5 of the region, accumulated per URI, on **footnote** items only — both committed groundtruth generations carry the link into the document only there. The item is a `footnote` text whose JSON carries the raw text plus `hyperlink` (the Markdown wraps it as `[text](uri)`); a footnote is a merge skip-label, so a paragraph continuing across one is merged past it | `assemble` footnote items (`Node::LabeledText`) |
| forced OCR (docling#4061) | `--force-full-page-ocr` / `ocr_mode=full_page\|layout_regions` skip the text-layer decode outright | `pdfium_backend::for_each_page(extract_text = false)` |
| RapidOCR's text detection (#429) | the `PP-OCRv6_det_small` DB detector runs over a bitmap page alongside layout (or ahead of the orientation probe, which reuses its boxes, #571) and adds the lines no recognized cell covers (> 30 % overlap = covered, cumulatively) as orphan cells (confidence 1.0); boxes are ordered by RapidOCR's exact `sorted_boxes` (top edge, then adjacent swaps within 10 px — the first port chained rows transitively and scrambled a two-column newspaper's lines once the boxes became the crops, #570); input capped at RapidOCR's `max_side_len`, 2000 px (`DOCLING_RS_OCR_DET_MAX_SIDE`; 960 was the pre-#570 budget, ~⅓ the time, −0.02 FUNSD recall); without the model OCR stays region-scoped on projection strips | `ocr_det.rs`, `Worker::detect_alongside` |
| `OcrOptions.scale` = 3 + RapidOCR's `max_side_len` = 2000 on image inputs | a standalone image (its own scale-1.0 page) is read by OCR at 3 px/pt shrunk to a 2000 px longer side — docling's effective resolution (#570; a 754 × 1000 form at 2.0); rendered PDF pages keep the 2.0 px/pt render the baselines are pinned to (docling would read them at 2.52 on Letter — a deliberate divergence, kept for the baselines; `--ocr-scale` overrides either) | `page_ocr_scale` |
| pdfium's `CPDF_Page` frame, docling#4008 | glyphs, link rects and the page size live in the `CropBox ∩ MediaBox` frame with `/Rotate` applied to the display frame, so a trimmed book page or a rotated digital page lines its cells up with the render | `textparse::page_box`, `pdfium_backend::to_display_frame` |
| docling-parse's page-box filter (#529) | a glyph is kept only when its whole char box (advance × the font's ascent / descent) lies inside the display box — the CropBox, else the MediaBox — edges included: a FrameMaker print slug beside the CropBox or a tiled page's neighbouring text beyond the MediaBox never becomes a cell (it used to be clamped onto the page edge, often to zero width), and a line crossing the edge is cut at the last glyph that fits, as docling-parse cuts it; no corpus page changes | `textparse::on_page` |
| docling-parse's char rect (`page_cell.h`, #528) | a glyph drawn with a non-upright text matrix — the `0 s -s 0 tx ty Tm` runs landscape `/Rotate 90` pages are built from, 180°/270°, or tilted — keeps its rotated quad, so the corner-distance contraction reads the line in its own reading order and the cell box is the quad's extent (before: zero-width boxes, every such run dropped; 180° read backwards). Upright glyphs keep the plain loose rectangle, so upright output is byte-identical. redp5110's 90° column headers (`*JOBCTL`, `QIBM_DB_SECADM`, …) now reach TableFormer and the table matches the groundtruth cell for cell (180 → 152 diff lines); the ODF-exported presentations — landscape slides drawn onto a portrait page through a 90° `cm` — get their text layer instead of OCR | `textparse::show_text`, `dp_lines::build_cells` |
| docling-parse `create_word_cells`, the TeX math encodings, the quote-normalization table | word cells are docling-parse's second contraction over the char cells (space glyphs as barriers, erased after); `CMSY*`/`CMMI*` without an `/Encoding` decode by the TeXbook tables; every curly quote → `'` | `dp_lines.rs`, `textparse.rs` |

**Deliberate deviations** (completeness over the metric; each costs a few
diff lines and recovers content docling drops):

* **Text panels** (#157): an *uncaptioned* `picture` that is really a dense,
  wide, multi-line text panel is demoted to per-paragraph `text` regions
  instead of shipping as pixels (`assemble::recover_text_panels`); a
  captioned figure keeps its crop.
* **Tables inside pictures**: a text-less `table` cluster > 50 % under a
  picture on a digital page (a screenshot of a table) has its word crops
  OCR'd so TableFormer's grid serializes with text.
* **A heading's body labelled `page_footer`**: a one-line paragraph in the
  bottom margin directly under a `section_header` that has no other body
  (within 2.5 line heights, same column, ≥ 40 % of the page width) is that
  heading's text, not furniture — the layout model labels the bottom margin
  by position (a CV's `Languages` line: `page_footer` 0.88 vs `text` 0.49,
  fp32 and int8 alike), and Markdown drops furniture, so docling loses the
  line (`assemble::reclaim_heading_body_footers`).
* **Runaway TableFormer rows**: a decode that emits `lcel` until `MAX_STEPS`
  without a row break is rejected once a row passes 256 tags and the region
  takes the geometric table path (docling ends at a 1 × 1 table there).
* **Picture-in-table captions**: a picture inside a table that pairs with a
  caption stays a standalone figure (upstream nests it and loses the
  caption).
* **Side-by-side figure captions** (#813): a row of pictures reads as a run
  of graphics followed by the run of their captions, and docling's
  `_find_to_captions` serves only the first caption — for the others the
  caption before them breaks the run, so they stay loose text under the row.
  A caption still loose after the upstream matching, whose only separation
  from a graphic run is other captions, takes the nearest unclaimed graphic
  of that run it sits under or over (horizontal overlap, within three line
  heights; `assemble::pair_graphic_captions`). A run broken by *text* stays
  loose as upstream leaves it (2206.01062's Figure 4 behind its hash line is
  in the groundtruth that way).
* **A caption the model called `text`**: a `text` region whose words are a
  caption label — `Table 5:`, `Figure 2.1.`, `Fig. 3:`, `Listing 4:` (the
  word, a number, then `:` or `.`; `Table 5 shows…` is prose) — next to a
  graphic in reading order, overlapping it horizontally within 25 pt, is
  relabelled `caption` before the matcher runs
  (`assemble::promote_labelled_captions`). The layout model labels a long
  caption block `text` now and then (2206.01062's four-line Table 5 caption,
  `text` at 0.99), and the table then went uncaptioned in JSON / DocLang
  while the paragraph stayed loose. docling has no such rule — its own run
  of that page folds the caption into the preceding paragraph — so the
  Markdown is unchanged there and the gain is the `captions` ref.
* **DocLang picture children** are not printed (upstream's DocLang picture
  serializer prints them).

**Baseline refreshes to know about.** The PDF baselines run outside CI (they
need the models and the shim), so a `docling-core` serializer change reaches
them late: the docling-core 2.96 table-header rule (#362) and docling#4216's
header flags were applied by re-serializing docling's own committed JSON
(`tests/data/pdf/groundtruth/*.json`) — the committed `.md` is exactly that
serialization, which is what keeps the refresh auditable — and regenerating
the snapshots.

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

## VLM pipeline conformance (#153/#311 — measured)

The remote-VLM pipeline (#77, `--pipeline vlm`) has its own comparison
harness: `scripts/conformance/vlm_conformance.sh` runs the PDF corpus through
docling.rs *and* Python docling's `VlmPipeline`, both against the same
endpoint (`scripts/dev/granite_vlm_server.py` — the only server class that
keeps granite-docling's DocTags tokens intact), and reports per-fixture
whitespace-normalized similarity plus byte-exactness. Known accepted
asymmetry: each side renders pages at its own scale, so some drift is
render-induced rather than parser-induced — triage before attributing.

**A GPU for the shim is a hard requirement, not a convenience** — measured
while dry-running the harness end-to-end for #311 (v1.23, 4-vCPU container,
fp32 `transformers` CPU inference of granite-docling-258M): a routine
academic page took **12 313 s (3.4 h) to generate 3 385 chars** (~0.1 tok/s;
a near-empty page still decodes at ~0.5 tok/s), while both clients cap a
page request at 600 s — the Rust agent's global timeout and
`vlm_convert.py`'s default alike. On CPU every real page therefore times out
client-side while the single-threaded shim grinds on as an orphan, blocking
the next request. That dry run did validate everything up to the model —
shim serving, both converters driving it, caching, scoring — and is what
shaped the harness fixes (venv `python3`, output-dir creation,
`--timeout`/`VLM_TIMEOUT`, the busy-shim probe hint, no-retry-on-timeout).
Reproduce with:

```bash
python scripts/dev/granite_vlm_server.py            # on a CUDA machine
scripts/conformance/setup-docling.sh
VLM_TIMEOUT=3600 scripts/conformance/vlm_conformance.sh   # prints the table + mean
```

### Measured results (#311)

Measured 2026-09-02 against `ibm-granite/granite-docling-258M` served by the
shim on an RTX 3080 Laptop (CUDA, bf16, greedy) — both sides drove the same
server; pages take roughly 390–590 s each at this model size on that GPU:

| fixture | sim% | byte-exact |
|---|---:|---|
| 2203.01017v2.pdf | 84.2 | no |
| 2206.01062.pdf | 59.2 | no |
| 2305.03393v1-pg9.pdf | 99.8 | no |
| 2305.03393v1.pdf | 58.8 | no |
| amt_handbook_sample.pdf | 98.6 | no |
| base14_fonts.pdf | 100.0 | **yes** |
| code_and_formula.pdf | 95.5 | no |
| docling-rs-demotion-repro.pdf | 94.2 | no |
| multi_page.pdf | 99.7 | no |
| normal_4pages.pdf | 67.5 | no |
| picture_classification.pdf | 100.0 | **yes** |
| redp5110_sampled.pdf | 78.9 | no |
| right_to_left_01.pdf | 97.8 | no |
| right_to_left_02.pdf | 69.1 | no |
| right_to_left_03.pdf | 100.0 | **yes** |
| skipped_1page.pdf | 98.4 | no |
| skipped_2pages.pdf | 95.7 | no |
| table_mislabeled_as_picture.pdf | 80.9 | no |

**Mean 87.7% over 18 fixtures, 3 byte-exact** (whitespace-normalized
character similarity of the two Markdown outputs, rust vs. Python docling).

Reading the numbers:

- Two conversions of the *same* page legitimately differ: each side renders
  at its own scale (docling.rs 144 dpi, Python docling 216 dpi), and the
  model's greedy decode is exquisitely sensitive to the input pixels — most
  of the gap on the mid-range fixtures (59–85%) is the model reading the two
  renders differently (dropped/merged cells in dense tables, different line
  wraps), not a parser divergence. Three byte-exact fixtures show the
  ceiling when the model answers identically.
- The long dense-table papers (2206.01062, 2305.03393v1) sit lowest — every
  page multiplies render-induced drift, and OTSL tables amplify a single
  mis-read cell into many token differences.
- Outputs cached from runs where a client timed out mid-corpus are
  unreliable — delete `target/vlm-conformance/` after an aborted run. A few
  entries above (2203.01017v2, 2305.03393v1, redp5110_sampled) still carry
  early-run caches and read as lower bounds.

## Bedrock LLM comparison (speed + fuzzy conformance)

`scripts/conformance/bedrock_conformance.sh` benchmarks docling.rs against an
Amazon Bedrock model (Nova by default) prompted to extract each corpus PDF as
Markdown: docling.rs runs one warm CLI batch (set `DOCLING_RS_EP=cuda` for a
GPU run), Bedrock gets a timed Converse call per PDF, and both outputs score
against the committed groundtruth with a normalized line-similarity
percentage (byte-exactness is meaningless for an LLM, so both sides get the
same fuzzy metric). Needs boto3 plus `AWS_BEDROCK_REGION` /
`AWS_BEDROCK_ACCESS_KEY_ID` / `AWS_BEDROCK_SECRET_ACCESS_KEY` in the env; the
model, token cap and prompt are overridable (see the script header — note
Nova Micro is text-only per AWS docs, so PDF input may require
`AWS_BEDROCK_MODEL_ID=eu.amazon.nova-lite-v1:0`).

Measured baseline (2026-08, `eu.amazon.nova-lite-v1:0` in eu-central-1,
docling.rs on CPU with the conformance model pins) over the 14
groundtruth-scored PDFs:

| | total | per doc | mean similarity | converted |
|---|---|---|---|---|
| docling.rs (cpu) | 59.8 s | 4.3 s | **90.3 %** | 14/14 |
| nova-lite | 273.0 s | 21.0 s | 12.5 % | 13/14 |

Per file docling.rs scores 50.8–100 % (the low end is the same
`right_to_left_03` / `table_mislabeled_as_picture` tail the strict metric
tracks); Nova Lite tops out at 47.2 % (`multi_page`), returns 0 % on the RTL
fixtures, and one document failed with a `ModelErrorException` retry-please
error. A GPU run (`DOCLING_RS_EP=cuda`) shrinks the docling.rs column
roughly another order of magnitude (~0.1 s/page on a consumer RTX 3080).

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
its backend at the enrichment scale, while docling.rs resizes from the existing
scale-2 page render — sub-pixel differences the classifier's softmax sees in
the third decimal, and that the VLM's argmax decoding absorbs entirely on the
fixtures.

## How the pipeline works

A pure-Rust parser (lopdf) reads the glyph layer and the page metadata, the
pure-Rust renderer draws each page to a bitmap ("The PDF stack" above); an
ONNX stack (layout detection, TableFormer, PaddleOCR) interprets it; regions
are assembled in reading order into a `DoclingDocument`. Note on OCR models: everything in this
document — snapshots, groundtruth, the conformance numbers — is measured with the
multilingual `ch_PP-OCRv3` recognition model (docling parity), which
`scripts/conformance/pdf_*.sh` pin via `DOCLING_OCR_REC_ONNX`/`DOCLING_OCR_DICT`.
The *runtime* default is the English `en_PP-OCRv3` pair (the `ch_` model glues
Latin words together); `DOCLING_RS_OCR_LANG=ch` restores the conformance model. Tables use **TableFormer** (image encoder
+ autoregressive OTSL structure decoder + cell-bbox decoder, ported and exported
to ONNX in `tableformer.rs`) on a cv2-exact preprocessed crop (`resample.rs`); the
structure + matched cell text reproduce docling's padded GitHub tables (2305-pg9
is cell-for-cell exact).

**Runaway rows (a deliberate deviation).** On dense tables under a multi-level
header (a 10 × 28 adjustment grid below three header rows) the decoder emits
`ched ched`, then `lcel` until `MAX_STEPS`, and never a row break. docling
2.129 (docling-ibm-models 4.0.3) decodes the same 1,025-tag sequence and ends
at a 1 × 1 table only because `html_to_otsl` knows colspans 2–20 and drops the
1022-wide span, after which its matcher loses 53 % of the words; the port kept
the span and emitted 1 × 1023 cells repeating the header (3 of 272 words
kept). `BboxBook` now stops once a row reaches `MAX_ROW_TAGS` (256; a 448-px
input cannot resolve that many columns, and real tables stay far below it) and
the structure is rejected, so the region takes the geometric table path: 12 ×
17 with all 272 words, and 2.8 s of decoding instead of 14.3 s. A long table
that fills `MAX_STEPS` with ordinary rows is unaffected. Snapshots 97/97
byte-identical; groundtruth unchanged.

**Heading levels (#302, opt-in).** With `--heading-hierarchy` (off by default —
everything in this document is measured with it off), a post-assembly stage
ports docling's `HeadingHierarchyModel`: section-header levels are assigned
from the PDF outline (bookmarks, fuzzily matched by title + page; a matched
list item is promoted to a heading), else legal/outline numbering, else font
style (glyph-height clustering + the conservative font-name weight/slant
parser). It runs on the assembled node stream (`heading_hierarchy.rs`,
`outline.rs`, `font_style.rs` in docling-pdf) and rewrites only heading
levels, so the default-off output — and every snapshot below — is untouched.

**Rotated scans.** Two normalization passes run before any inference, both only
on pages with no text layer (exactly the OCR set), both mapped back to
display-space geometry at assembly via `PdfPage::rotation`:

1. **`/Rotate` metadata** (`pdfium_backend::extract_page`): the page renders
   as displayed, so a declared rotation is un-rotated losslessly and
   `width`/`height` swap. Pinned by `crates/docling/tests/scanned.rs` — all
   four `/Rotate` orientations of `ocr_test.pdf` OCR byte-identically.
2. **Content-based orientation** (`orient.rs`, #225): a physically rotated
   raster (`/Rotate 0` — sideways phone photo, landscape-fed sheet) is probed
   with the recognizer itself, the classic OSD trick: take the text
   detector's boxes (#571; the projection strips without the model), rotate
   them with the page into each 90° hypothesis, recognize up to 6 of the
   widest, score by Σ(confidence × chars). An upright page early-exits after
   one probe round (≥20 chars at ≥0.90 mean confidence); a rotated hypothesis
   must read real text (≥8 chars at ≥0.55), beat upright by 1.2× *and* read
   more confidently by 0.10 to win — thin evidence (blank/line-art pages) is
   a no-op, and any probe failure degrades to "assume upright". The margin
   is what #571 needed: on projection strips a sparse form read at the same
   poor confidence every way round and the character count sent 9 of
   FUNSD's 199 upright forms sideways; on detector boxes all 199 read upright
   at ≥ 0.97 and take the early exit. The detector's boxes are kept for the
   OCR pass of an upright page, so detection still runs once. Scores are
   deterministic (single-threaded rec, fixed probe selection), so snapshots
   hold. `DOCLING_RS_OCR_ORIENTATION=off` disables the pass;
   `DOCLING_RS_DEBUG=1` prints per-hypothesis scores. Pinned by the
   `ocr_test_raster*` fixtures (the same lossless raster physically rotated
   inside the page): all four convert byte-identically.

### Performance / parallelism

Profiling a 14-page document (`DOCLING_RS_TIMING=1` prints an env-gated per-stage
wall-clock breakdown) shows ~80 % of the time is the two ONNX models (layout ~58 %,
TableFormer ~22 %) and ~16 % the page-image downsample — all per-page work that is
independent across pages. A multi-page PDF therefore renders on one thread (one
`Renderer` per document, its font and image caches shared across the pages)
and fans the pages out across a **pool of page-workers**, each
owning its own model set (`ort`'s `Session::run` is `&mut self`, so sessions can't
be shared), reassembled in page order. A bounded channel keeps only a handful of
page bitmaps resident, so the streaming memory profile is preserved; the output is
byte-identical to the serial path (verified across all PDF snapshots). Single-page /
image / METS inputs keep the serial path and load no helper models.

The layout model is **memory-bandwidth bound** (even one model at four intra-op
threads only reaches ~2.1× core utilisation), so the pool defaults to two intra-op
threads per worker with `workers ≈ cores / 2` (ceiling 16 — a memory bound of
~0.4 GB of model sessions per worker, not a performance cap; #324 follow-up
measured the old cap of 4 leaving ~1.2× on the table on a 16-core machine): two threads sharing one
in-cache copy of the weights beats both one fat model and many single-thread workers.
The speed-up scales with cores and memory bandwidth. Tune per machine with
`DOCLING_RS_PDF_WORKERS` (pool size) and `DOCLING_RS_PDF_INTRA` (intra-op threads
per worker). Each worker layout-detects up to `DOCLING_RS_PDF_LAYOUT_BATCH`
already-rendered pages per inference call (issue #73; default: per-page on
the CPU provider and under CoreML, 4 under CUDA/TensorRT/DirectML — CoreML
only takes static-shaped graphs and only the per-page graph is static,
#602; #338: every CPU
measurement favors per-page, 8.5 vs 9.3 s/conv on a 4-core x86 box and ~2×
on a 16-core M4 Max, while GPU dispatch overhead still amortizes). In
per-page mode the model's dynamic `batch` axis is pinned to 1 at session
creation (#339): the free dimension blocks ONNX Runtime's channels-last conv
transform (~1.4× on Apple-silicon CPU); x86 measured neutral. Output is
bit-identical at every batch size, so the knob is purely about throughput.

### Text reconstruction: a pure-Rust PDF text parser (default)

The byte-exact ceiling was the **text extractor** — pdfium's *rendered* glyph
boxes diverge from docling's own `docling-parse` C++ parser at exactly the points
that drive conformance (generated spaces, combining marks, ligature/fraction
positioning). The pipeline now ships a **pure-Rust text parser** (`textparse.rs`,
on `lopdf`) that reconstructs each glyph's box from the *font's own advance
widths* and the PDF text/graphics matrices — the same information docling-parse
uses. It is the **only** text layer (the pdfium fallback it once had was measured
to add nothing on any corpus page and removed): a page it reads no text from
is a scanned page for the OCR path. The parser supplies **all** text — prose, the **word cells**
TableFormer matches against, and **code cells**.

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
gap exceeds 0.33×avg-char-width, plus literal space glyphs), `enforce_same_font`
(fonts compared by docling-parse's font name — `/FontDescriptor/FontName`, `/Name`,
`/BaseFont` — not by the page's resource key, so one face bound as `/F1` and
re-encoded as `/F2` is one font and a word drawn across the two stays whole, #728),
ligature recomposition, and loose-box geometry, with the Euclidean corner gap on the
parser's boxes (matching docling).

**Word cells** come from a second contraction over the same char cells
(`create_word_cells`, the parity table above): the word factors
(adjacency gate 0.33, space threshold 2 × 0.33) with space glyphs as hard
word-boundary barriers erased after the contraction — verified against the installed
docling-parse oracle (redp5110 pages byte-exact). These are the per-word
tokens TableFormer matches against table-grid cells. **Code cells** come from the parser too,
via a gap-based grouping (`Grouping::CodeGap`): the parser emits no space glyphs
(a source space is a positioning gap), so a word breaks wherever the inter-glyph
gap exceeds ~0.25× the line height, with no punctuation glue — `et al. 2000`
keeps its space while `add(a,` / `b)` stay joined. `code_and_formula` is byte-exact
(`function add(a, b) { return a + b; }`).

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
   diffs (at the time, 2206's ~92 table-row diffs traced to ~4 structure
   diffs; today its one remaining table diff is a single header rowspan). A parity
   harness (`DOCLING_RS_TF_MATCH_DUMP=dir` + `scripts/test/tf_match_reference.py`-style
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
   survey over-detection dropped sharply at the time (108 → 88 vs groundtruth;
   54 today — over-detection remains its dominant blocker), and
   redp5110's table-of-contents is now classified and rendered as a **table**
   (`document_index`) instead of a picture. The TOC table's remaining diff is a
   TableFormer dot-leader column-matching gap — later found to be largely
   word-cell tokenization (the create_word_cells port took redp5110 164→73)
   — tracked with the other
   table-structure work in
   [#60](https://github.com/docling-project/docling.rs/issues/60). *(The
   per-fixture byte counts quoted in this item are from its era; the committed
   snapshots and the Current-state table above are regenerated with every
   parity change.)*
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
5. **The model-input renderer**
   ([#478](https://github.com/docling-project/docling.rs/issues/478)). docling
   2.123+ renders the page images its models consume with docling-parse's
   Blend2D/FreeType renderer; the pure-Rust renderer reproduces its frame and
   drawing rules with tiny-skia's coverage values (mean |Δ| ≈ 1 / 255 against
   the shim — "The PDF stack" above), so the default build's model inputs are
   close to, not identical with, docling's, and heron's borderline labels
   move on the ML-borderline fixtures (60/98 snapshots exact
   against baselines pinned to the shim). Closing that gap means the 8-bit
   coverage values themselves.

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
| Single shared line/word contraction pass | `--text-layer-only` (then `--no-ocr`) conversion ~1.25× faster, identical output |
| Per-document font + form caches in the text parser | 3–10% off `textparse` here; far more on CJK/form-heavy PDFs |
| True-KV-cache decoder export (`decoder_kv.onnx`, optional) | parity at corpus table sizes; O(past)/step for very large tables |
| Dynamic-batch `decoder_kv.onnx`: a page's tables decode in one lockstep loop | decode steps shared across tables; byte-identical (see round four below) |
| Per-layer caches in loop-owned buffers (`decoder_kv_layers.onnx`, #871) | no per-step allocation that grows with the table (the arm64 OOM); long tables ~24% faster; bit-identical (round six) |

Cumulative head-to-head vs Python docling (measured on an 8-thread desktop,
`scripts/test/performance.sh`): **4.3× faster warm conversion, 4.7× end-to-end,
2.3–2.6× less peak memory** on the PDF ML pipeline — up from ~1.2× warm
before this work. Model sizes: layout 172 → 68 MB, TF decoder 78 → 50 MB.

A re-measure on the final stack (hoisted-KV TableFormer decoder default per
#97; different desktop, Jul 2026) with `performance.sh
tests/data/pdf/sources/2305.03393v1-pg9.pdf` — a single table-dense page, so
the page-parallel worker pool sits mostly idle and this bounds the *low* end
of the speedup range: **3.0× end-to-end** (16.3 → 5.4 s avg over 5 runs),
**2.0× warm conversion** (9.1 → 4.7 s/doc), **1.9× less peak memory**
(1589 → 857 MB).
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
| page render (then pdfium; the Rust renderer's share is of the same order, 50–200 ms a page) | 1.8% | 3.7% | 16.5% |
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
16-page document (11.6 s vs 12.2 s vs 15.6 s; separate run from the INT8
table below, hence the ±0.1 s vs its 11.5 s).

### Validated win: INT8 quantization (quality-checked)

`scripts/install/quantize_models.py` produces two quantized models. Point
`DOCLING_LAYOUT_ONNX` / `DOCLING_TABLEFORMER_DECODER` at them to opt in.

**These are now the default:** when the `*_int8` files sit next to the fp32
models at the default paths, the pipeline loads them automatically.
`DOCLING_RS_FP32=1` forces full precision, and an explicit
`DOCLING_LAYOUT_ONNX` / `DOCLING_TABLEFORMER_DECODER` always wins (the
conformance/groundtruth scripts pin fp32 explicitly, so snapshots stay
deterministic).

#### TableFormer encoder: not quantized, but half its size (#374)

The encoder is a ~20-Conv ResNet backbone feeding a 6-layer transformer, and
the transformer Gemms — not the convs — are its cost. INT8 was measured and
rejected: conv-only static QDQ keeps fidelity (enc_out / cross-K/V cosine
≥ 0.995) but is not faster than ORT's fp32 conv kernels, and quantizing the
attention MatMuls collapses cross-attention fidelity to ~0.85 (garbled table
structure). What *was* wrong with the published 215 MiB `encoder.onnx` is
exporter waste: the explicit all-false attention mask handed to the
transformer encoder was materialized as a zero `[1,8,784,784]` fp32 constant
(18.8 MiB) baked into each of the six layers — 112.6 MiB of `x + 0` around
103 MiB of real weights (42.6 MiB Conv, 60 MiB MatMul/Gemm).
`scripts/install/strip_zero_masks.py` (run by the export) removes those `Add`
nodes; the stripped graph's outputs are bit-identical to the original's
(onnxruntime, max |diff| = 0), so the republished encoder changes nothing but
the download. On top of that, `encoder_fp16.onnx` (`quantize_models.py
tableformer-encoder-fp16`) stores the same graph's weights as fp16 behind a
`Cast` back to fp32 — ORT folds the cast at load, so compute, speed and
memory are those of the fp32 encoder and only the file shrinks, 103 → 54 MB
(4.2× below the original 226 MB). Fidelity gate on the 54 calibration inputs:
cosine ≥ 0.999999, relative L2 error ≤ 1.5e-3 per output tensor; and over the
full 101-file snapshot corpus the fp16 encoder's Markdown is **byte-identical**
to the fp32 encoder's (same machine, same binary, `diff -rq` empty). It is
therefore preferred when present, like the INT8 decoder; `DOCLING_RS_FP32=1`
or an explicit `DOCLING_TABLEFORMER_ENCODER` keeps the fp32 file. INT8 for
the encoder stays off the table.

#### Layout: static QDQ INT8, **Conv ops only** (~2.4× faster layout)

Calibrated on 42 real corpus pages preprocessed exactly like
`layout.rs::predict`. Only the HGNetv2 backbone convolutions are quantized;
the transformer decoder and detection-head MatMuls stay fp32.

| Configuration | layout.predict (16-page doc)¹ | end-to-end wall | model size |
|---|---:|---:|---:|
| fp32 baseline | 17.2 s | 16.6 s | 172 MB |
| **INT8 conv-only** | **7.2 s (2.4×)** | 11.5 s (1.45×) | 68 MB |
| + INT8 TableFormer decoder | — | 12.3 s² | — |

¹ `layout.predict` is summed across the parallel page workers, so it can
exceed the end-to-end wall.
² Separate run; within run-to-run noise of the 11.5 s conv-only wall — the
INT8 decoder's own win is per-table (~10 % faster tables, byte-identical;
see its section below), not end-to-end on this document.

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

**Quality gate** (measured at INT8-selection time over the then-23-file
PDF+scanned corpus):

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

##### 7-bit weights: the same model on every x86 CPU

The quantizer emits **7-bit** weights (`reduce_range=True`). u8 activations
times s8 weights go through `VPMADDUBSW` on CPUs without VNNI, and that
instruction sums two products into one int16 slot: full-range weights reach
255·127·2 = 64770 and saturate at 32767, so a model that measures perfectly on
the machine that quantized it can drop whole regions on a plain AVX2 CPU.
Not hypothetical — a publish run's agreement gate lost **83 of 505** confident
detections (two of them tables) on a runner without VNNI, while the same recipe
over a structurally identical export lost **1** on a VNNI box. 7 bits caps the
pair product at 255·64·2 = 32640, below saturation.

It costs nothing. Same VNNI machine, same corpus, 4 intra-op threads (the
ratios are the point, not the absolute times — this is a shared container, not
the benchmark box the table above was measured on):

| layout_heron | agreement gate (505 confident fp32 detections) | 640×640 inference |
|---|---:|---:|
| fp32 | — | 548 ms |
| INT8, full-range weights | 1 lost (0.20%) | 290 ms |
| **INT8, 7-bit weights** | **0 lost (0.00%)** | **282 ms** |

`quantize_models.py` checks the weight range right after quantizing
(`check_weight_range`), before the accuracy gate. Reading weights is
hardware-independent, so a recipe that reintroduces full-range weights fails on
the publishing machine instead of on a user's laptop — the accuracy gate alone
cannot catch this, since it only ever exercises the ISA it happens to run on.
An int8 layout model fetched before this change is full-range: on a non-VNNI
CPU, refetch after the next models publish, or set `DOCLING_RS_FP32=1`.

##### BatchNorm folded before quantization (~1.4× faster int8 layout)

A re-profile (Sep 2026, 4-core Xeon, ORT node profiler on the int8 graph)
found ~40% of layout inference in fp32 elementwise ops that the fp32 path
never executes: `Mul` 13%, `Add` 12%, `DequantizeLinear` 12%,
`QuantizeLinear` 4%. The HGNetv2 export spells eval-mode BatchNorm as
`Conv → Mul(1,C,1,1) → Add(1,C,1,1)`; in an fp32 session ONNX Runtime's
ConvMulFusion/ConvAddFusion fold that into one FusedConv at load, but the
QDQ quantizer wraps the Conv in Quantize/Dequantize pairs that block the
fusion, so every one of the backbone's 110 normalizations ran as
`DQ → Mul → Add → Q` over the full activation tensor. `quantize_models.py`
now folds the 55 pairs into the conv weights and bias first
(`fold_conv_affine`: `(W∗x)·s + t == (W·s)∗x + t`, exact up to the fp32
rounding the runtime fusion performs as well) and the graph collapses to
back-to-back QLinearConvs.

Folding alone moved int8/fp32 agreement sideways (mean |Δscore| over the
1045 above-threshold fp32 detections on the calibration pages 0.049 → 0.051),
so the activation ranges now come from `CalibMovingAverage` — per-page
min/max averaged instead of corpus-wide extremes, so one outlier page no
longer sets the u8 grid for all. Entropy and percentile calibration were
tried on the same graph and landed in between (0.046 / 0.043), on top of
needing every activation sample in RAM (the 52-page set does not fit in
15 GB). Same box, same corpus, single-thread ORT for the output diffs:

| layout_heron_int8 recipe | 2 thr | 4 thr | mean \|Δscore\| vs fp32 | lost (of 1045) | Markdown diff-lines vs fp32 (24 docs) | vs groundtruth (17 docs) |
|---|---:|---:|---:|---:|---:|---:|
| previous (unfolded, MinMax) | 602 ms | 381 ms | 0.049 | 38 | 327 | 564 |
| folded, MinMax | 453 ms | 289 ms | 0.051 | 48 | 345 | 576 |
| folded, MinMax moving average | 424 ms | 289 ms | 0.037 | 28 | 290 | 563 |
| **+ stem convs `embedder.0/1` fp32** | **≈ same¹** | **≈ same¹** | **0.022** | **15** | **146** | **423** |

¹ Interleaved bench, 30 iterations: 350.3 vs 348.6 ms (2 thr), 247.4 vs
249.9 ms (4 thr) against the all-int8 row — inside run-to-run noise. The
whole stem in fp32 (`embedder.2` too) would have cost ~12% for 0.020 / 17;
`embedder.0` alone buys little (0.035 / 19).

The quantization error concentrates at the front of the backbone: the two
stem convs see the raw pixels at 320×320, the largest maps in the graph, and
their u8 rounding rides through every later stage. Leaving those two in fp32
takes the int8 model's distance to fp32 output from 327 to **146** diff-lines
(`redp5110_sampled` 99 → 2, `right_to_left_03` 46 → 0) and its groundtruth
distance from 564 to **423** — fp32 itself sits at 411, so the int8 default
now costs 3% of conformance instead of 37%. Entropy and percentile
calibration were also tried (in between MinMax and moving average, and they
need every activation sample in RAM). Both gates pass on every row (0/505
confident detections lost; weights within 7 bits). Pipeline effect,
back-to-back cold CLI runs, default 2×2 pool, with the two changes below:
the 60-page slice of the .NET reference 34.5–34.9 s → **21.0–21.4 s
(−39%)**; the table-heavy `2206.01062` 13.2–13.4 s → **9.1–9.8 s (−30%)**.

##### Text layer parsed per page, not up front

`for_each_page` used to run the pure-Rust text parser over *every* page of
the document before rendering the first one: a 6.5 s serial prefix on the
1913-page .NET reference that the page-worker pool sat idle through, and a
`--pages` window still paid for all 1913 pages (14% of a 60-page slice's
wall time). `PageTextParser` keeps the loaded document and the font/form
caches and parses a page when the render walk reaches it — 0.58 s to open
the document plus ~2.4 ms per selected page, overlapped with the workers'
inference. Same glyph walk, same shared caches, same contraction per page:
Markdown output is byte-identical over the PDF corpus, the scanned fixtures
and the 60-page slice (single-thread ORT, old vs new binary).
`DOCLING_RS_TIMING` now reports `textparse.open` plus a per-page `textparse`.

##### Pages parked on a busy TableFormer, not blocked

The pool shares one TableFormer behind a mutex (the memory win above), and a
worker whose page had a table used to block on it for the whole of another
worker's decode: on the 60-page slice ~9.5 s of the `tableformer` stage was
one worker idle (stage 22.5 s vs `tableformer.structure` 11.5 s +
`inter_area` 1.5 s), and pool topology could not buy it back — 2×2, 4×1 and
3×1 all landed within noise. `finish_page` is now `prepare_page` → table
stage → `complete_page`, and the pool path (`Worker::run_pool`, shared by
the buffered and streaming conversions) tries the slot without blocking:
when it is held, the prepared page is parked and the worker keeps pulling
from the render channel, retrying parked pages before every pull. While
pages are parked the pull is non-blocking, so an empty channel means "wait
for the TableFormer", never "hold work while waiting for the renderer". At
most two parked pages per worker (their bitmaps are the memory cost); past
that, or once the channel closes, the worker waits as before. Output does
not depend on completion order — both callers reassemble by page index —
verified byte-identical over 23 documents on a 2-worker pool with
single-thread sessions. On `2206.01062` the stage's wait fell from ~10 s to
~1.9 s (stage 8.3 s vs structure 6.0 s + inter_area 0.4 s).

##### Pillow-exact resize over rows

`pil_resize` (the docling-parity 1.5×→1× layout image on the render thread,
and the 640×640 layout input in each worker) went through
`get_pixel`/`put_pixel` per tap and walked the vertical pass by column:
~30 ms per page, slower than the SIMD 3×→2× downscale of a bigger image.
Both passes now run over raw rows with one i32 accumulator row for the
vertical pass. The arithmetic is unchanged and purely integer, so the bytes
are identical (the Pillow reference hashes pin that); `image.resize_layout`
is 11–12 ms per page. The render thread's remaining per-page work — the two
renders plus the two downscales — is not the bottleneck at 4 cores but caps a
many-core pool at roughly 9 pages/s.

##### Round two (Sep 2026): the fixed costs

With inference trimmed, a second profile of the merged stack looked at what a
conversion pays regardless of page count. All four changes below are
output-neutral: Markdown over the PDF corpus, the scanned/rotated fixtures and
the 60-page slice is byte-identical with and without them, on the serial path
(single-thread ORT) and on a 2-worker pool.

- **The page window loaded every page.** `for_each_page` walked
  `pages.iter()` from page 0 and skipped to `first`, so every page before the
  window was loaded and closed — ~0.7 ms each. A one-page `--text-layer-only`
  window over the 1913-page .NET reference took 3.1 s, of which the parser
  accounted for 0.4 s; indexing the window with `pages.get(i)` brings it to
  **0.85 s** (the full pipeline on that page: 4.1 → 2.1 s, `--pages 1-60`
  no-ocr 2.7 → 1.0 s). The parser's teardown — 250 ms of lopdf objects on
  that document — now happens on a detached thread (`textparse.close` still
  reports it) instead of delaying the last page.
- **Session creation is graph optimization.** Creating the int8 layout
  session costs ~0.8 s (4 threads) and the TableFormer encoder ~1 s; each
  process, each pool worker. ONNX Runtime can serialize the optimized graph
  and load it with the optimizer off in ~0.15 s, running the identical
  kernels. `docling_onnx::commit` keeps such a cache
  (`$XDG_CACHE_HOME/docling-rs/graphs`, `DOCLING_RS_GRAPH_CACHE_DIR`,
  `DOCLING_RS_NO_GRAPH_CACHE=1`), keyed on the model file, the session
  options that shape the graph, the ONNX Runtime API version and the CPU's
  SIMD features (the saved graph carries hardware-specific NCHWc kernels);
  CPU provider only, and every failure on the cached path falls back to the
  ordinary load. A one-page digital PDF: **1.26 → 0.68 s** wall (the first
  run after a model or machine change pays ~0.16 s extra to write the cache).
- **OCR ran on one core while three idled.** Recognition is pinned to a
  single intra-op thread for determinism (multi-threaded float reductions
  flip CTC argmaxes on low-confidence characters), and it is linear in line
  width — ~0.17 ms per pixel column, no batching benefit, no new-shape
  penalty (measured). Lines are independent, so `OcrModel` now holds one
  single-thread session per lane (the worker's thread budget;
  `DOCLING_RS_OCR_SESSIONS` overrides) and deals same-width batches across
  them by index; each line still meets exactly the single-thread kernel
  path. On `ocr_test.pdf`, 4 lanes: `ocr.rec` 374 → 188 ms, `orient.score`
  429 → 225 ms, wall 1.62 → 1.33 s; `nemotron_multipage` 6.5 → 4.1 s.
- **The orientation probe was a second OCR pass.** Sub-stage timers showed
  `orient.detect` on a scan is ~95% recognition: it reads the six *widest*
  lines, and on a small scan those are most of the page's text — the probe
  (467 ms) cost more than `ocr.rec` for the whole page (393 ms). The lanes
  halve it; the remaining lever is a smaller probe budget, not taken here
  because it changes the evidence the decision is made on.

Pool-path effect of this round, back-to-back runs: `2203.01017v2` 9.8 →
8.0–8.1 s (its in-picture table OCR rides the lanes), `2206.01062` 9.1–9.8 →
8.2–9.6 s, the 60-page slice 21.0–21.4 → 19.4–21.6 s.
##### Round three (Sep 2026): inside a table

Sub-stage timers (`tf.preprocess`, `tf.encoder`, `tf.decode_loop`,
`tf.bbox`, alongside `tf.decode_step`) split the ~1 s per table on
`2206.01062` (4 threads) into: decode loop 0.53 s (~107 steps, ~5 ms each),
bbox head 0.26 s, encoder 0.20 s, page→1024 resample 0.11 s, preprocessing
5 ms. Three exact fixes came out of that, one dead end, and one wall.

- **The bbox head fought the memory-pattern planner.** Its `tag_h` input is
  `[ncells, 512]` and every table has a different cell count, so ONNX
  Runtime re-planned buffer reuse on every run — and on this graph the plan
  is worse than none: 290 ms vs 54 ms for a 100-cell table, 560 vs 94 ms
  for 200 cells (repeat runs, 4 threads; the decoder session already ran
  without the planner for the same reason). Off now; the head still pays
  first-shape allocation per table because every table *is* a new shape.
- **One 1024-px frame per page.** The page→1024 INTER_AREA resample (a
  full-page f64 box filter, 110–170 ms) ran once per *table*;
  `TableFormer::page_1024` builds it once per page and every table crops
  from it. `2203.01017v2`: 8 → 4 resamples.
- **The decoder runs on one intra-op thread.** A step is 49 small GEMMs
  over a single token: it streams the layer weights (~70 MB per step) rather
  than computing, so threads only add synchronisation — isolated, 4.1 ms per
  step on 1 thread vs 5.5 on 4 (4.9 vs 7.1 once the KV cache is 100+ long).
  In the pool a table decode also stops taking every core from the other
  workers' layout inference, and a single-thread session has a fixed
  reduction order, so table structure no longer varies run-to-run on
  near-tie tokens (the conformance scripts pinned one thread for exactly
  that; the default matches them now). The encoder keeps the shared budget:
  680 ms single-threaded vs 165 on four.
- **Dead end: graph simplification.** The exported step graph has 449 nodes,
  ~415 executed per step, 275 of them Reshape/Transpose/Squeeze/Unsqueeze/
  Concat. `onnxsim` takes it to 383 nodes, bit-identical — and no faster
  (5.2 → 5.2 ms): ONNX Runtime already folds what can be folded at load, and
  the shape ops that remain are dispatch noise next to the GEMMs (42% of
  node time).
- **The wall.** What is left per table is the encoder's real compute (a
  448×448 CNN + transformer pass, 165 ms on 4 threads) and the decoder's
  weight streaming. Both move only with the model: INT8/fp16 weights for the
  step (the dynamic-INT8 `decoder_kv` was already tried and rejected — it
  flips near-tie tokens on redp5110's TOC), or batching several tables'
  steps through one run so the weights stream once for all of them. The
  first is not byte-exact by construction; the second turned out to be —
  round four below.

Back-to-back, same machine, default pool: `2206.01062` 9.4–9.6 → 8.8 s,
`2203.01017v2` 8.0–8.1 → 7.9 s, `2305.03393v1-pg9` (one 55-step table on
one page) 2.4 s. Output is byte-identical to the round-two references on the
serial path and on a 2-worker pool over the same 32 documents.

#### Round four (Sep 2026): a page's tables decode together

The decode step streams ~70 MB of layer weights for one token, so the
obvious lever left after round three was to push several tables' tokens
through the same step. It needed a dynamic-batch `decoder_kv.onnx` and a
loop that keeps the tables' KV caches aligned — and, unlike the earlier
guess, no masking at all.

- **The export** (`export_tableformer.py`, `DecodeKVHoistedBatched`): `tag`
  is `[B,1]`, the caches `[L,B,H,past,hd]`, every cross tensor `[B,…]`; B=1
  is the same graph, so the artifact stays a drop-in for a one-table loop.
  Two things had to be exact. First, ORT must fuse the batched graph the
  way it fuses the B=1 one: with a symbolic batch axis a 3-D activation
  stays MatMul-then-Add (a different rounding of the bias, ~1e-6 in the
  logits) while the B=1 graph gets MatMul+Add→Gemm through constant-shape
  Reshapes, and a fully 2-D module loses the Add+LayerNorm→
  SkipLayerNormalization fusion instead. The module therefore runs each
  linear on an explicit 2-D view and keeps the residual stream 3-D — the
  optimized graph then has the published one's kernel mix (49 Gemm, 18
  SkipLayerNorm, 12 FusedMatMul), and B=1 reproduces the published
  `decoder_kv.onnx` **bit for bit** over 3 random tables × 48 greedy steps.
  Second, the script's batching gate asserts that two tables decoded in one
  batch give, step by step, the logits and hidden states each gives alone
  (row b of an `[B,K]·[K,N]` GEMM is the `[1,K]` result in MLAS).
- **The loop** (`tableformer.rs::decode_batch`): every table of a page is
  encoded, the per-layer cross tensors are stacked along the batch axis
  (one ~20 MB copy per table per page), and one loop steps all of them. The
  caches start at `past=0` for every row and grow in lockstep, so nothing
  is padded or masked; a table that emits `<end>` keeps its row (fed `END`,
  output ignored) until the last one finishes, and each table then runs its
  own bbox head. Detected from the decoder's `tag` input having a symbolic
  batch axis, so the older fixed-`[1,1]` export keeps decoding one table at
  a time; a page with one table takes exactly that path; a failing batched
  run falls back to it.
- **What it buys, and what it can't.** Per-op profile of one step (1
  thread, past=50): the 49 Gemm total **6.8 ms at B=1 and 6.8 ms at B=2** —
  the weights do stream once for all rows — but each table brings its own
  cross-attention: q·Kᵀ and softmax·V over 784 image positions × 6 layers
  read ~19 MB of that table's `cross_kt`/`cross_v` per step (FusedMatMul
  1.7 → 3.2 ms, MatMul 1.5 → 2.6 ms for B 1 → 2). So a step costs 10.5 ms
  alone, 13.5 for two tables, 19.6 for four, 34 for eight: **−35% per table
  at two tables per page, −55% at four**, and that is the bound — the
  cross-attention reads are the model's, not the loop's.
- **Measured** (`DOCLING_RS_TIMING=1`, same binary, published vs
  dynamic-batch decoder): `2203.01017v2` (two tables on each of its pages)
  decode loop 3.1 → 2.5 s (297 single steps → 178 shared ones), serial wall
  25.2 → 24.6 s, default pool 10.0 → 9.4 s; `2206.01062` decode 5.6 →
  4.7 s, pool 10.4 → 9.8 s; `redp5110_sampled` (one table per page) flat,
  as it must be. Pool wall moves less than the decode does because the
  deferral machinery of round two already overlaps a page's tables with
  other pages' layout. Output byte-identical to the round-three references
  on the serial path and on a 2-worker pool over the same 32 documents.
- **Memory:** a page's tables are now all held encoded at once (per-layer
  cross tensors ~19 MB each) plus their stacked copy, against which the
  hoisted decoder's never-read stacked `cross_k`/`cross_v` (2×9.6 MB per
  table) are dropped at encode time now instead of living for the table's
  lifetime. Net on `2203.01017v2` (default pool): peak RSS 1.74 → 1.81 GB.

The dynamic-batch `decoder_kv.onnx` reaches users through the models
release (`publish-models.yml` re-run); until then the shipped decoder simply
takes the one-table-at-a-time path.

#### Round five (Sep 2026): the page→1024 resample, and a cache that cannot be poisoned

Two small items left over from round four's profile of master.

- **`inter_area`** (the page → 1024 px `cv2.INTER_AREA` box filter every
  table crop is cut from; `tableformer.inter_area`, ~0.16 s per table page
  in the pool, 43 ms in isolation) kept its addition order — horizontal
  taps in increasing source column, then vertical taps in increasing source
  row, f64 — and changed only its shape: the shrunk source rows are now
  computed on demand as the vertical pass reaches them and kept in a ring
  of a few rows (a source row feeds at most two output rows) instead of a
  30 MB `sh × dw` intermediate written and re-read, the horizontal taps run
  over the contiguous byte span they cover, and the vertical pass is a flat
  `f64` axpy. **43 → 18 ms** per page render (1224×1584 → 791×1024, one
  thread); a test asserts the bytes equal the naive per-pixel form on six
  geometries, and the 32-document corpus is byte-identical on the serial
  path and the 2-worker pool.
- **Graph cache guard** (`docling_onnx::commit`). ONNX Runtime serializes
  the optimized graph as a side effect of session creation and does not
  fail the session when that write comes up short: a full disk truncates
  the file and the session still initializes from memory. The cache then
  published the prefix, and every later process tried it, failed, removed
  it and rebuilt — a ~50 s cold start for the TableFormer graphs, seen once
  right after a disk-full episode in the round-four session. The commit
  path now checks the file's protobuf skeleton before renaming it into the
  cache (each top-level field's declared length must end within the file;
  a truncation lands inside the multi-megabyte `graph` field), a handful of
  reads and seeks, never a parse of the weights.

#### Round six (Oct 2026): per-layer caches in buffers the loop owns (#871)

Not a speed round — an out-of-memory one. On linux/arm64 (ONNX Runtime 1.31)
with the CPU arena off (`DOCLING_RS_NO_ARENA`, serve's default) a PDF with a
~700-step table ran a 6 GiB container out of memory: 4.3 GB of glibc heap
was *free* 608 steps into the first table, 530 MB in use. Every step of
`decoder_kv.onnx` made the KV cache one token longer and freed the old one —
and copied it several times over on the way: ONNX Runtime's optimized graph
splits the stacked `[L,B,H,past,hd]` input into the six layers, each layer
concatenates its token, K is transposed for q·Kᵀ, and the layers are stacked
and concatenated again into the outputs. Each of those buffers is a
`malloc` one token larger than its predecessor, and on arm64 glibc reused
25% of the freed blocks (87% on amd64), so they piled up.

- **The graph** (`scripts/install/kv_layers.py` → `decoder_kv_layers.onnx`):
  a rewrite of `decoder_kv.onnx`, not a re-export. The cache comes in and
  goes out per layer — `cache_kt_<i>` `[B,H,hd,past]` (K already transposed)
  and `cache_v_<i>` `[B,H,past,hd]` — so a step's only cache-sized work is
  one `Concat` per layer and tensor. The attention reads exactly the floats
  it read before (Kᵀ in the same layout, the same `FusedMatMul` with
  `alpha=s²`), and the rewrite keeps the exporter's `value_info` so ONNX
  Runtime still fuses Add+LayerNorm→SkipLayerNormalization (without it the
  residual stream rounds differently, ~5e-6 in the logits). Bit-identical
  to `decoder_kv.onnx` — logits, hidden states and caches — over the whole
  1024-step range, one table and two batched (`kv_layers.py --verify`, run
  by `export_tableformer.py`). Weights untouched: the new 0.25 MB graph
  reads `decoder_kv.onnx.data`.
- **The loop** (`tableformer.rs`, `DecoderStyle::KvLayered`): the caches live
  in two buffers per decode (a table, or a page's batch), each holding the
  12 tensors in regions sized for `MAX_STEPS` tokens — `calloc`'d, so pages
  become resident only as far as the table reaches. A step reads its caches
  from one buffer and binds the outputs to the other
  (`OutputSelector::preallocate` over `docling_onnx::borrowed_f32`, the one
  `unsafe` in it: an owned ONNX Runtime value over a slice the loop keeps
  borrowed for the step), so ONNX Runtime writes the next caches in place
  and allocates nothing that grows with the table; the next step swaps the
  buffers.
- **Measured** (x86_64, release CLI, `DOCLING_RS_NO_ARENA=1`; an
  `LD_PRELOAD` counter of allocations ≥ 64 KiB, three 79-row tables of
  ~790 steps): TableFormer's large allocations **44,600 → 620** (36.1 →
  8.0 GiB allocated over the run, 843 → 27 distinct sizes; what is left is
  the encoder, once per table); the same PDF converts in **69.0 → 52.2 s**
  — the per-step Split/Transpose/re-stack copies were real work too. Peak
  RSS on x86 is unchanged (~2.1 GB; amd64 reused the blocks), Markdown and
  JSON byte-identical on the 8 table-bearing corpus PDFs and that one.
  The same change pins glibc's mmap threshold in every process that loads
  a model (`docling_core::memory`, until then serve and MCP only), which on
  the reporter's machine alone avoided the OOM, at 0.66–1.28 GB above
  2.22.0's peak.

`decoder_kv_layers.onnx` reaches users through the models release: an
installation without it keeps decoding with `decoder_kv.onnx`.

#### TableFormer decoder: dynamic INT8 (~10% faster tables, byte-identical)

The autoregressive tag decoder is MatMul-only; weights-only dynamic INT8
produced **byte-identical corpus output** and ~10% faster table decode
(784 → 695 ms/table), 78 → 50 MB. Small but free.

The decoder speed is *not* weight-bound — it is per-step overhead (see backlog
item 2), which is why quantization helps so little there.

### GPU execution providers (#74) — validated on GPU (#108)

The ONNX sessions (layout, TableFormer×3, OCR recognition, both enrichment
models) accept alternative ONNX Runtime execution providers behind cargo
features: `cuda`, `tensorrt`, `directml` (Windows), `coreml` (macOS). CPU
stays the default in every configuration — the features only compile a
provider in; `DOCLING_RS_EP` selects one at runtime:

| `DOCLING_RS_EP` | behavior |
|---|---|
| unset | `auto` in a build with a CUDA-class (or XNNPACK) feature compiled in (a GPU build should use the GPU); CPU in a default build. CoreML is opt-in (#602): a `coreml` build stays on CPU, and the implicit `auto` leaves CoreML out |
| `cpu` | CPU, byte-for-byte the pre-#74 code path (no EP registered) |
| `cuda` \| `tensorrt` \| `directml` \| `coreml` | that provider, **error-on-failure**: an explicitly requested accelerator that can't initialize fails the conversion instead of silently degrading to a 10×-slower CPU run; requesting one that isn't compiled in warns once and stays on CPU |
| `auto` | every compiled-in provider registered in order TensorRT → CUDA → CoreML → DirectML; ONNX Runtime falls back down the list to CPU at session creation (for images deployed on mixed fleets). CoreML only when `auto` is set by name |

When a GPU provider is selected the model resolution skips the int8 defaults
in favor of fp32 (`decoder_kv.onnx` stays preferred): the int8 exports are
QDQ graphs calibrated for CPU kernels — on GPU they add de-quantize traffic
and their conformance was only ever validated on CPU. An explicit
`DOCLING_*_ONNX` path override still wins over this policy. (Until #602 the
Python bindings set `DOCLING_LAYOUT_ONNX` to the cached int8 graph
themselves, so a source-built GPU wheel ran int8 layout; they now hand the
cache over as `DOCLING_RS_MODELS_DIR` only.)

CoreML specifics (#602, M4 Max, 25-page manual): the `NeuralNetwork` model
format is the default — byte-identical to the CPU provider (3.5 s vs 3.7 s,
~41% GPU) — while `MLProgram` (`DOCLING_RS_COREML_FORMAT=mlprogram`, 2.05 s)
changes the layout detections on 14 of 25 pages and is an opt-in with a
notice; the layout batch stays per-page, since CoreML takes static-shaped
partitions only and a batched session leaves the batch axis free (the old
batch-4 default ran 0% on the GPU, 8.4 s).

Verified without GPU hardware (this is what CI's `ep-features` matrix
covers): default/`cpu`/`auto`/unknown/uncompiled-request configurations all
produce byte-identical corpus output on a CPU-only build; on a
`--features cuda` build with no usable CUDA, `auto` falls back to CPU with
fp32 models selected (output byte-identical to `DOCLING_RS_FP32=1`) and
`DOCLING_RS_EP=cuda` fails loudly at the first session load. In CLI batch
mode (`--input`/`--output`) that first failure aborts the whole batch —
every remaining PDF would fail identically, so they are reported as
`skipped` instead of producing one error line per file.

#### Measured on real hardware (issue #108)

`scripts/test/gpu_benchmark.sh` — every corpus PDF (+ the scanned set)
under `cpu` and `cuda`, best of 3 cold CLI runs each, outputs
byte-compared. Machine: **NVIDIA GeForce RTX 3080 Laptop (16 GB), driver
566.07 · AMD Ryzen 9 5900HX, 16 logical cores** (both providers on the
fp32 models, per the policy above).

**Output equivalence:** 21 of 22 fixtures byte-identical to CPU; one
(`2203.01017v2`, the heaviest layout) differs by 2 markdown lines — fp32
CUDA kernels are not bit-identical to fp32 CPU kernels, so a borderline
detection can flip; groundtruth-distance parity is the standard here, and
byte-parity on 21/22 exceeds it. The entire 2-line diff is one label flip
on one borderline region: the caption fragment
`c. Structure predicted by TableFormer:` comes out as a `list_item`
(`- c. …`) on CPU and as plain `text` on CUDA — same content, same
position, one class score straddling the 0.3 threshold.

**Corpus total (best-of-3): CPU 124.5 s · CUDA 101.2 s → 1.23×** — but the
aggregate hides a clean size split:

| segment | speedup (best) |
|---|---|
| multi-page digital (9–39 pages: arXiv papers, redp5110) | **1.5–2.1×** (`2305.03393v1`: 13.6 s → 7.0 s) |
| mid-size digital (4–5 pages) | 1.1–1.3× |
| 1–2-page digital | 0.75–1.0× — CUDA EP init + host↔device traffic never amortizes |
| scanned/OCR-heavy | 0.65–0.85× — dominated by the page render + OCR pre/post on CPU |

The corpus is small-document-biased; on a genuinely large document the
init noise vanishes and the ONNX stages dominate — that is the regime the
GPU features exist for. The 1913-page .NET C# language reference (same
machine, single cold run each):

| provider | wall time | speedup |
|---|---|---|
| `cpu` | 15 min 13 s (767 % CPU) | — |
| `cuda` | **1 min 45 s** (321 % CPU) | **8.7×** |

Practical guidance: the break-even for a cold CLI run sits around 3–4
pages. Below that, or for OCR-heavy scans, stay on CPU; for batches or
services use the warm `Pipeline` / `docling-serve`, which pays EP
initialization once per process instead of once per file and moves the
break-even to roughly "any document with a table". The cold-vs-best gap on
the CUDA column (~1.5–2.5 s) is that per-process EP initialization made
visible. (Timing methodology: the script reads the monotonic clock —
wall-clock `date` proved able to step backwards under NTP mid-benchmark.)

<details>
<summary>Per-file results (seconds, best of 3; cold = run 1 incl. model/EP init)</summary>

| file | cpu cold | cpu best | cuda cold | cuda best | speedup (best) | output |
|---|---|---|---|---|---|---|
| 2203.01017v2 | 19.93 | 18.13 | 13.31 | 10.38 | 1.75x | 2 diff lines |
| 2206.01062 | 15.34 | 14.84 | 10.86 | 9.70 | 1.53x | identical |
| 2305.03393v1-pg9 | 3.87 | 3.87 | 6.25 | 3.98 | 0.97x | identical |
| 2305.03393v1 | 13.55 | 13.55 | 9.94 | 7.01 | 1.93x | identical |
| amt_handbook_sample | 2.63 | 2.50 | 4.74 | 3.26 | 0.77x | identical |
| code_and_formula | 3.13 | 3.13 | 5.14 | 3.09 | 1.01x | identical |
| multi_page | 5.12 | 5.12 | 6.24 | 4.30 | 1.19x | identical |
| normal_4pages | 7.67 | 6.34 | 7.04 | 4.76 | 1.33x | identical |
| picture_classification | 2.56 | 2.56 | 5.24 | 2.95 | 0.87x | identical |
| redp5110_sampled | 16.53 | 16.53 | 11.73 | 8.05 | 2.05x | identical |
| right_to_left_01 | 1.94 | 1.94 | 5.01 | 2.59 | 0.75x | identical |
| right_to_left_02 | 2.03 | 2.03 | 4.64 | 2.68 | 0.76x | identical |
| right_to_left_03 | 3.26 | 3.26 | 5.95 | 4.09 | 0.80x | identical |
| skipped_1page | 3.62 | 3.38 | 4.79 | 2.96 | 1.14x | identical |
| skipped_2pages | 3.99 | 3.75 | 5.89 | 3.32 | 1.13x | identical |
| table_mislabeled_as_picture | 4.67 | 4.48 | 6.71 | 4.51 | 0.99x | identical |
| nemotron_multipage | 4.83 | 4.76 | 9.16 | 6.18 | 0.77x | identical |
| ocr_test | 2.53 | 2.50 | 5.22 | 3.34 | 0.75x | identical |
| ocr_test_rotated_180 | 2.64 | 2.64 | 4.27 | 3.10 | 0.85x | identical |
| ocr_test_rotated_270 | 2.35 | 2.29 | 3.50 | 3.50 | 0.65x | identical |
| ocr_test_rotated_90 | 2.37 | 2.35 | 5.46 | 3.48 | 0.68x | identical |
| sample_with_rotation_mismatch | 4.54 | 4.54 | 4.74 | 3.92 | 1.16x | identical |

</details>

### Backlog

Landed from the earlier ranked list (each measured above): the int8 layout
model as the CPU default, the KV-cache TableFormer decoder with hoisted
cross-attention (#97) and the dynamic-batch decode of a page's tables, layout
batching in the pool (#73 — dynamic-batch export with the position embedding
folded offline, bit-identical at every batch size; default per-page on CPU,
4 on GPU, #338), the SIMD page downscale (`fast_image_resize`, same
Catmull-Rom kernel, ±1/255 — `DOCLING_RS_SLOW_RESIZE=1` and the conformance
scripts pin the scalar path the snapshots were generated with), per-document
font / form caches and one shared line/word contraction in the text parser,
same-width OCR batching and single-thread OCR lanes.

Still open, none of them large:

* `decode_code` / `decompose_ligatures` allocate a `String` per glyph
  (`textparse.rs`); decompose once at font-parse time and return borrowed
  `&str`.
* The RTL merge is O(n²) (string prepend in `merge_with`, `dp_lines.rs`);
  accumulate reversed and flip once per line.
* Same-width OCR buckets could run across the page-worker pool's idle
  threads (one extra session per worker).
* The orientation probe reads the six widest lines — most of a small scan's
  text; a smaller budget would halve it again but changes the evidence the
  decision is made on.
* Padded OCR batches (PaddleOCR-style) were measured and rejected: padding
  perturbs the valid region's probabilities through the model's
  global-attention blocks and changes the decoded text on 16/20 lines.

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
### Where the time goes

For the declarative formats (everything but PDF/images), `cargo run --release
-p docling --example profile_declarative` times parse, Markdown export and
JSON export separately for every file in the corpus and lists the slowest.
Parsing is milliseconds per document and Markdown export is negligible; JSON
export dominates on table-heavy documents because docling's schema repeats
every table cell in the `grid`, and it builds a `serde_json::Value` tree
before printing. The table builder now fills that grid from an index instead
of a hash map of cloned cells: on the corpus's heaviest JSON (a 13 MB patent
export) the export went from 256 ms to 179 ms, on an EBCDIC table dump from
123 ms to 66 ms, byte-identical output. Printing the tree is ~10 % of the
remaining cost; the rest is the `Value` allocation itself, so a further
step would be serializing straight from the document.

For the PDF/image ML pipeline, `scripts/test/profile_pdf.sh` runs the release
binary over the PDF corpus with `DOCLING_RS_TIMING=1` and sums the pipeline's
per-stage wall-clock (`crates/docling-pdf/src/timing.rs`) into one table. On
the 88-page corpus the cost is almost entirely model inference: TableFormer
structure recognition (the autoregressive OTSL decode — ~1400 decode steps
across the corpus's tables) and the per-page layout model together account for
~85 % of it, with a one-time ONNX session/graph init paid on the first table
page. Everything outside the models — both page renders, the two resamples,
text-layer parsing and assembly — was under ~6 % combined on the pdfium
chain; the pure-Rust renderer that replaced it costs more (on the 18-file
corpus of the MarkItDown comparison, 80 pages, the two renders of every page
sum to a third of the CPU time the models take), so since #682 the renders
are scheduled off the model's path rather than before it: a page's 2× bitmap
draws on a helper thread while layout reads its 1× image, the serial path
renders the next page while the worker processes the current one, and the
worker pool — two-thread workers, which the layout model's weak intra-op
scaling makes faster than one four-thread worker — takes every document of
four pages or more. Output is byte-identical; the model cost itself is bounded
by the layout and TableFormer models, so the levers there stay the INT8
models, the KV-cached decoder and GPU execution providers.

What a TableFormer decode step costs, measured on the same box with
`DOCLING_RS_ORT_PROFILE` (ORT's per-operator profile; see
`scripts/dev/ort_profile_summary.py`) and a per-step timer: 4.4 ms with an
empty cache, 5.7 ms once 100–150 tokens are cached, 6.2 ms past 150 — about
10 µs per cached token — and 6.0 ms for two tables batched. The floor is
memory traffic, not arithmetic: a step streams the decoder's ~90 MB of fp32
weights through 49 single-token GEMMs (40–50 % of kernel time) plus 19 MB of
per-table cross-attention K/V (about a quarter); the cache-dependent part is
the `Split`/`Concat`/`Transpose` copies the exported graph makes of the
growing cache (≈10 % at 85 cached tokens), and the rest is ~330 nodes of
shape plumbing and per-node executor overhead. Three things were measured
not to pay and are not in the tree: ONNX Runtime IO binding for the step
(cross tensors bound once, outputs pre-bound: 5.41 vs 5.34 ms per step,
byte-identical output — the copies are inside the graph, not in the
`Run()` call); a decoder session per pool worker instead of the shared slot
(wall time unchanged on 9-, 18- and 60-page documents, +0.5 GB RSS — two
weight-streaming decodes share one memory bus); and more intra-op threads
(the step does not scale). What would: re-exporting the decoder with
per-layer caches that need no `Split`/`Concat`/`Transpose` (bit-exact in
principle, new model files), narrower weights (INT8/fp16 — a numerics
change), or a GPU execution provider.


## ONNX Runtime 1.31 (#523)

Since ort 2.0.0-rc.14 the default builds — CLI, wheels, npm, containers,
crates — statically link ONNX Runtime **1.31** (pyke's `ms@1.31.0`
binaries; `docling-rs --version` prints the linked version). Measured
against the 1.28 build of the same commit on a 4-vCPU x86-64 box (release,
INT8 layout, `--jobs 1`, same models, same graph cache rebuilt per runtime):

| | ORT 1.28 | ORT 1.31 |
|---|---:|---:|
| 18-PDF corpus (80 pages), wall | 38.9 s | 40.0 s |
| `2206.01062.pdf` (9 pages, 5 tables), wall | 6.9–7.5 s | 8.4 s |
| `redp5110_sampled.pdf` (18 pages, scans + tables), wall | 10.0–10.9 s | 10.1 s |
| `layout.predict`, 9 pages (2 threads) | 3.98 s | 3.80–3.97 s |
| `tf.encoder`, 5 tables | 1.25 s | 1.17 s |
| `tf.bbox`, 5 tables | 1.48 s | 0.47 s |
| `tf.decode_step`, 458 steps | 2.34 s (5.1 ms/step) | 3.0–3.4 s (6.5–7.5 ms/step) |

The one regression is the TableFormer decode step: its 49 single-token
fp32 GEMMs run at ~100 µs each on 1.31 against ~56 µs on 1.28 (ORT's own
per-operator profile, `DOCLING_RS_ORT_PROFILE`; the same 331-node graph,
the same 1-thread session), so a table-heavy page is ~10–15 % slower and
the corpus ~3 %. 2 intra-op threads recover about half of it at the
price of run-to-run determinism, so the session stays single-threaded.
The bbox head got three times faster and layout, the encoder and OCR are
unchanged.

Output: the 18-PDF corpus converts to byte-identical Markdown on both
runtimes; `--to json` differs in one file (`redp5110_sampled`): two table
cells' box edges move by 0.01 pt, two table regions' edges by 1.2–1.5 pt,
and one picture crop is 1 px wider — the float-accumulation noise of a
different GEMM kernel, below every tolerance the DocLang comparison uses.
`scripts/conformance/pdf_groundtruth.sh` and `pdf_conformance.sh` report
the same files with the same line counts on both runtimes (9/18 strict and
74/98 snapshots exact with the pure-Rust renderer — the committed
snapshots are pinned to the docling-parse renderer, that drift predates
the bump and is not the runtime's). The picture classifier runs fully
optimized (the #517 cap only fires under `ort-load-dynamic` with an older
library) and matches docling on the enrichment fixtures; the ASR tests
pass on the Whisper tiny fixtures (the Parakeet int8 preset was not
re-measured here).
