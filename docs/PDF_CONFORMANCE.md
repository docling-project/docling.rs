# PDF conformance

How close the Rust PDF pipeline gets to docling's **default** Markdown. Two
references, both run the way docling's `tests/test_e2e_conversion.py` runs
(`do_ocr=False`, layout + TableFormer with cell matching, CPU,
`export_to_markdown(compact_tables=True)`):

* **upstream's committed groundtruth** — `tests/data/pdf/groundtruth/*.md`
  mirrors docling's own `tests/data/pdf/groundtruth` (mirrored at docling
  `a7760ab`, 2026-11-11). `scripts/conformance/pdf_groundtruth.sh`
  scores it with no docling install: `--skip-ocr --compact-tables`, the
  model inputs rendered by docling-parse's renderer
  (`DOCLING_RS_RENDERER=docling-parse`, #478) and the conformance model set
  pinned (fp32 layout, the `ch` OCR pair). This is the headline, and what
  CI checks the headline against.
* **live docling** — the latest published release on the same files with
  the same options (`scripts/conformance/setup-docling.sh`, then
  `scripts/conformance/pdf_live_docling.py OUTDIR tests/data/pdf/sources/*.pdf`
  and a `diff` of each stem against `pdf_groundtruth.sh`'s output).
  `scripts/conformance/conformance.sh pdf` is a different measurement: the
  *runtime* defaults (int8 models, OCR on) against docling's defaults.

Both report **strict** (byte-for-byte) and **whitespace-normalized**
matches; *diff* = changed lines against the reference (one changed line
counts as 2). `tests/snapshots/` pins the pipeline's own output
(`scripts/conformance/pdf_conformance.sh`, every PDF fixture,
deterministic). CI (`.github/workflows/pdf-conformance.yml`) runs both on
every PR that touches the PDF stack, the serializers, the fixtures or the
baselines: it fails on snapshot drift, uploads the regenerated tree to
refresh `tests/snapshots/` from, and warns when the strict count no longer
matches the headline below.

## Current state

**9 / 21 strict** against upstream's groundtruth — **511 diff lines**
(measured 2026-11-12; snapshots 101/101 exact). The mirror refresh at
docling `a7760ab` added three fixtures — `acroform_sample` (16),
`elsevier-00` (287), `newspaper-00` (26) — and changed none of the others;
the 18 files scored before went 180 → 168 with docling-parse's outline
boxes for a font whose descriptor metrics it rejects (the parity table:
`2305.03393v1` 8 → exact, `redp5110_sampled` 30 → 26), then to 182 with
docling-parse 7.25's symbol-font decoding (#1113, the parity table:
`2203.01017v2` 12 → 24, `redp5110_sampled` 26 → 28) — a move *toward*
live docling, whose 7.25 reads what the groundtruth, still 7.24, does
not.

Against **live docling 2.150.0** (docling-parse 7.25.0, docling-core
2.106.0, docling-ibm-models 4.0.4) the 18 upstream fixtures score **6 / 18
strict, 475 diff lines** (the three `base14_fonts_rot*` files are our own
fixtures, not run there). Live docling no longer reproduces upstream's
committed groundtruth on five files (60 lines): docling-parse 7.25's word
segmentation (docling-parse#437 — word gaps judged in em, no per-line
k-means) now splits the pairs docling-parse 7.22–7.24 glued and we already
split (`Khurram Azeem Hashmi`, `RELATED WORK`, `mAP @`, `Chefredaktorin
Désirée Pomper`), and its font decoding (docling-parse#431) reads
`2203.01017v2`'s `Dingbats` marks as `✓` / `✗` and redp5110's
private-use Symbol arrows as `→` where the groundtruth reads `3` / `7` and
U+F0AE — ported here (#1113). The upstream groundtruth will move to that
when docling regenerates it.

| PDF | vs groundtruth | vs live 2.150 | dominant remaining blocker |
|---|---:|---:|---|
| picture_classification | **exact** | **exact** | — |
| 2305.03393v1-pg9 | **exact** | **exact** | — (TableFormer table, cell for cell) |
| amt_handbook_sample | **exact** | **exact** | — |
| code_and_formula | **exact** | **exact** | — |
| right_to_left_03 | **exact** | **exact** | — |
| 2305.03393v1 | **exact** | **exact** | — (the figure file names' underscores: outline boxes, a cell assigned once) |
| base14_fonts_rot90 / _rot180 / _rot270 | **exact** | — | — (`/Rotate` display-frame normalization, docling#4008; our fixtures, groundtruth = the unrotated upstream file) |
| right_to_left_01 | 2 | 2 | docling-parse cuts `أيضًا` before its tanween (`أيض ً`); we keep the word |
| right_to_left_02 | 2 | 2 | one word's place at a line start (`ووف`) |
| normal_4pages | 8 | 8 | docling folds the page-number glyph into the heading (`## 들어가며 1`), we emit it as its own item; one paragraph docling breaks before its last line |
| 2203.01017v2 | 24 | 6 | line-end hyphens docling fuses (`im-`/`balance`, `appear-`/`ances`; `column- and` is our #920 divergence), `\|T \|`; the groundtruth's glued author names (`KhurramAzeemHashmi`) and its `3`/`7` for the table's check marks (docling-parse 7.24), which live docling reads like us — `Khurram Azeem Hashmi`, `✓`/`✗` (#1113) |
| 2206.01062 | 24 | 12 | `tildelow10%` (docling-parse's glyph-name mapping; we read `~10%`), the author block and one paragraph in another place; the groundtruth's glued words (`RELATEDWORK`, `mAP@`) and live's `mAP @ 0.5` vs our `mAP @0.5` |
| acroform_sample | 16 | 16 | AcroForm widgets: docling's `extract_form_fields` turns the field value (`Ada Lovelace`) and the check boxes (`- [x]` / `- [ ]`) into form items and links the last line; not ported — we read the drawn labels as one paragraph |
| newspaper-00 | 26 | 24 | line-end hyphens docling fuses (`Subventionier-`/`te`); the groundtruth glues words before a capital (`ChefredaktorinDésiréePomper`), live splits them like us — but where live splits a lowercase word from a following `W` (`unseren Werbekunden`, `im Wettbewerb`) we still glue it (docling-parse#437's em-based gap, not ported yet); `Co-Geschäftsleiterin Hug AG` one block there, two here, and a paragraph docling breaks before its last line |
| multi_page | 32 | 32 | docling-parse maps the list bullet glyph to `∞` and docling keeps it as text (`- ∞ IBM MT/ST …`); we read a bullet and drop it as the marker |
| redp5110_sampled | 28 | 26 | monospace code docling-parse spaces around punctuation (`EMPLOYEES .`, `( SESSION_USER , 'MGR' )`); a list bullet docling-parse reads as its CID (`r`), we as `�`; the groundtruth's private-use arrows (`\uF0AE HR\_SCHEMA`), which live docling reads like us — `→ HR\_SCHEMA→` (#1113) |
| table_mislabeled_as_picture | 24 | 24 | the survey over-detected as tables; blank cells docling keeps |
| table_misidentified_as_form | 38 | 38 | the form container's nested table / picture (docling#4064): docling nests them in the form region and keeps the heading, we flatten the region |
| elsevier-00 | 287 | 285 | roughly a third: soft hyphens — docling keeps a line-final U+00AD and a space (`excep­ tionally`, 57 paragraphs), we join the word; a fifth: hyperlinks — docling's `_match_hyperlink` now links every text label, not footnotes only (the journal homepage, ~50 reference entries); the rest: the two-column body's paragraph order (adjacent `Geological importance` / `Educational potential` paragraphs swapped, affiliations one item off), letter-spaced headings (`A B S T R A C T`, which we join), one table's columns |

The two Korean pages `skipped_1page` / `skipped_2pages` carry no Markdown
groundtruth and are not scored: docling keeps them out of its end-to-end
tests as fixtures for failed-page handling (`test_failed_pages.py`).

**History.** On the docling-parse ≥ 7.22 groundtruth (#921, upstream
`dc4693f`) the tree went 815 lines (6 / 18) → 389 (7 / 18; the adaptive
word gap, quote folding, RTL pen gaps) → 407 (two #921 divergences:
the clipped word-gap clustering, the 0.35 floor) → 379 (8 / 18; #998's
reading order) → 377 (#1001's cell folds) → 373 (#996, the `bcb3678`
mirror) → 337 (#1003's unmapped glyphs) → 180 through the November
assembly and reading-order ports (docling-ibm-models#185's orphan
fallbacks, lists past asides, #1098's page-break merges, #1092's
docling#4276 separators). Earlier generations: 374 → 328 against the
docling ≥ 2.123 groundtruth (docling-parse ≤ 6), 9 / 17 against the
pypdfium2-era one. The per-change detail is in the git log of this file and
of `crates/docling-pdf/src/assemble.rs`.

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
  `skrifa` / `read-fonts` (#952 replaced the unmaintained `ttf-parser`,
  RUSTSEC-2026-0192: 110 of the corpus's 119 pages pixel-identical, the
  rest within 0.013 % of their pixels, the shim-pinned baselines
  untouched), the Type 2 `dotsection` operator stripped first
  (`dotsection.rs`, #531 — it used to abandon the glyph, so FrameMaker
  index pages lost their dot leaders and heron read them as one table),
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
the docling-parse-rendered baselines with the two conformance scripts
(2026-11-11):

| model inputs rendered by | snapshots exact (101) | groundtruth (21 files) |
|---|---|---|
| docling-parse shim (`DOCLING_RS_RENDERER=docling-parse`, the reference) | 101 | 511 diff lines, 9 strict |
| **Rust renderer** (the default) | 75 | 647 diff lines, 7 strict |

The groundtruth gap is in six files: `2203.01017v2` 24 → 64 and
`redp5110_sampled` 28 → 53 (TableFormer grids), `right_to_left_03`
exact → 28, `table_mislabeled_as_picture` 24 → 40, `elsevier-00` 287 →
304, `2305.03393v1` exact → 6 (Fig. 5's caption box reaches the file
name's underscores, 0.08 pt clear of it under the shim's render); the
others move by at most 4 lines.

Heron's borderline labels move with ±1/255 of anti-aliasing, so the
snapshots that drift are the ML-borderline ones: the matplotlib heat-map
figures of `2412.19437`, whose tick-label soup TableFormer reads as
differently shaped grids, `redp5110`'s TOC, `html_v_otsl_intro_v2`,
`2203.01017v2`. What separates the two renders there is the anti-aliasing
of text, not its placement: the ink per glyph is the same and there is no
sub-pixel offset, but tiny-skia puts it on fewer, darker edge pixels (6.6 %
vs 5.5 % of `2206.01062` p2 below mid-gray). Closing the rest means
Blend2D's analytic coverage and 8-bit bilinear weights, FreeType's outline
flattening, and docling-parse's font-similarity resolver with its bundled
faces — each a measurable step on the same test.

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
| `_adjust_cluster_bboxes`, `keep_empty_clusters=False`, the three merge rounds (#419) | once cells are final, every regular region is fitted to its cells' union, an empty regular region is dropped, and orphans inside a fitted box fold in — before TableFormer and the reading order, which never see the raw model boxes. Only into a region whose *raw* box the orphan touches: `_remove_overlapping_clusters` finds partners in `regular_index`, an R-tree built once from the raw clusters and never updated, so an orphan a fitted box grew over but its model box did not reach stays an item of its own, and keeps its cell — a cell is assigned once in docling, so on a tie the orphan beats the grown box (2305.03393's Fig. 5 file-name underscores, `\_` items before the caption) | `assemble::fit_regions_to_cells`, `assign_cells` |
| OCR'd pages: `_should_prefer_cluster` / `_select_best_cluster_from_group` (union-find over IoU > 0.8 or 80 % containment) | region-scoped OCR recognizes each region's crop, so a paragraph box over its own line boxes would read the ink twice: such groups collapse to one survivor on the group's union box | `assemble::merge_overlapping_regulars` |
| RapidOCR's crops: the recognizer reads the detector's boxes (#570), warped out of the detector's rotated rectangle (#981) | inside each text/table region the DB detector's boxes (center in the region, taken whole) are the recognizer's line crops — each the quad warped upright (`get_rotate_crop_image`: bicubic, replicated borders; tall crops turned 90°), not its axis-aligned bounding box, which on a scan skewed by 0.6° is a third taller than a 700 px line of 12 px print and squeezed and slanted it into `Baccalaatpoesne accurlatci usgers` (7 of 32 lines on the reporter's scan; 0 in RapidOCR with the identical models); a region the detector saw nothing in, or every region without the model, is split into ink-projection strips (the pre-#570 line source, `DOCLING_RS_OCR_LINES=projection`); RapidOCR's `text_score` (0.5) drops low-confidence lines. FUNSD word recall 0.57 → 0.86 on 30 forms, 0.61 → 0.90 on all 199 (Python docling 2.133: 0.85) with the PP-OCRv6 recognizer (`.models/ocr_rec_v6.onnx`, preferred when present) | `ocr_prep::prep_region_lines_det`, `prep_table_words_det`, `ocr_prep::text_score`, `ocr::resolve_rec_pair` |
| `_sort_cells` (docling-parse index order), `PageAssembleModel.sanitize_text` (docling#4052) | a region's cells serialize in source order; lines join with a space except after an *attached* dash, which fuses the wrapped word; a detached dash is kept | `assemble::cells_text` |
| `predict_merges` (docling#3888) | cross-column (per page) / cross-page paragraph continuations, never two paragraphs down one column of a laid-out page; a chain that crossed a page break goes on down the new page, as upstream tests the head's page (a hard hyphen before a lowercase continuation joins without it; the head test accepts a trailing comma; tables are in the skip set). The text-layer paths, one region per line, join wrapped lines down the column under the same tests | `reading_order::predict_merges`, `assemble::merge_continuations` |
| `_init_l2r_map` / `_init_ud_map` (docling#4093, 2.124) | two elements consecutive in assembly order, left strictly of right on one row (vertical IoU > 0.8), are linked left→right and a row is read through before the paragraph below it; the assembly rank is the region's first source cell (a table's or picture's first *interior* cell) | `reading_order::init_l2r` / `init_ud`, `assemble::cluster_cids` |
| `_do_horizontal_dilation` overlap test (docling#5131), `_find_heads` + `_is_separated_above` (docling#4438) — #998 | a narrow box is widened toward its up/down neighbour only if the widened box overlaps no other element (upstream's check ran after the box was written, so it never fired; the port had none), so a box widened over the neighbouring column no longer parents its elements; heads separated by an element strictly between them that spans both horizontally read top-down (centred headings over full-width legal text, OJ L 2025/492), side-by-side columns keep the geometric order. Children still sort by the plain `__lt__`. docling#4276's separators and one-to-one edge test: see the separators row below (#1092). | `reading_order::dilate`, `is_separated_above` |
| `_init_ud_maps`' link between consecutive text that overlaps slightly (docling#4276) | two elements consecutive in assembly order (`cid`, `cid + 1`), neither a table, picture or code, whose left edges agree within 1 % of the page width and where the lower one's top reaches at most 0.25 % of the page height into the upper one, are linked upper → lower: the strictly-above test draws no edge between boxes that touch, so the upper one flowed into whatever it reached next — often the other column — and the column read its paragraphs pairwise swapped. #4276's separators and its other two rules came later (#1092, the row below). | `reading_order::init_ud`, `assemble::order_regions` |
| docling#4276's reading-order separators (`build_page_separators`, `use_reading_order_separators`, on by default) — #1092 | the page's long thin rules divide the reading order. The geometry is docling-parse's — `get_shape_lines` (stroked axis-aligned segments, cut to the clip paths' boxes) and `get_connected_shape_bounding_boxes` (each visible painted path's box, the stroke's half width included, clipped, merged greedily) — recorded by the text parser's own path walk, not the renderer plugin, so every build has it: `re` = `m` + three `l` + `h`, `s`/`f`/`b` close, specks dropped, the line width scaled by the CTM, `CA`/`ca`/`LW` from the ExtGState, a fill in an unresolved Pattern space not painted, a pending `W` captured after the painting operator. Candidates thinner than 3.5 pt (strokes: axis-aligned), merged within 1 pt until stable, kept when 8 % of the width / 5 % of the height long, crossing no text or graphic, not inside a graphic, with content on both sides; none past 5000 candidates (docling#4632). Horizontal separators are nodes of the body's up/down graph — a band a rule closes is finished first (a manual's rows of icon + text read icon, text, icon, text) — and a synchronization point for the interruption test; vertical ones block a same-row link and clamp the dilation; headers, footers and a form's children are ordered without them. With them come #4276's other two rules — a same-row link needs an empty gap (`_has_page_element_between`), the dilation widens only along a one-to-one vertical edge — and two fixes to the port: `_sort_ud_maps` sorts the up map as well as the down map (the upward walk takes the first unvisited parent), and the upward walk stops on a cycle. `DOCLING_RS_PDF_ORDER_SEPARATORS=0` turns the separators off. Not ported: upstream's per-document time budget (the candidate cap bounds the merge; above 10 000 painted paths a page's boxes are not merged and its strokes alone count). | `textparse::record_shape`, `reading_order::build_separators`, `assemble::page_separators` |
| `_readingorder_elements_to_docling_doc`'s `current_list` | only a text element that is not a list item (a heading, a paragraph, a formula, a caption, a page header or footer) closes a list: a picture, chart, table or code block between two list items goes after the list, and the items after it stay in the same list (an icon beside a procedure's step reads after the procedure, which stays one list). In a form or key-value area any other element closes the list, as upstream's `split_lists_on_non_list`. Three cases still differ. Per page: a list running on from the previous page with no page header or footer between does not hold back the new page's first pictures. A form or key-value container inside a list does not close it upstream; here its children are flattened into the page, and the first of them closes it. A table's or picture's footnote is a node of its own here and closes the list; upstream it hangs on its graphic. | `assemble::continue_lists_past_asides` |
| `normalize_extracted_text` (docling#3929, docling#5069) — #1001 | body text and TableFormer cells get the same character folds: typographic quotes / `•` / `⁄` (cells honour `pdf_keep_typography` like body text), Latin ligatures (`ﬅ` is `ſt` → `st`, Dutch `Ĳ`/`ĳ`), the spurious U+F0A0 glyph dropped, and a per-run NFKC of Arabic Presentation Forms-A/B (`ﻣﺎﺮﺱ` → `مارس`) that leaves `x²`, `H₂O` and full-width forms alone. Before, cells only joined `@ ` and kept `it’s`, `“quoted”`, `• dot`. | `assemble::fold_presentation_forms`, `normalize_grid`, `textparse::decompose_ligatures` |
| `_find_to_captions` — `_rank_caption_candidates` + `_match_caption` (#265, #813) | a `caption` reaches the graphics (`picture`, `table` / `document_index`, `code`) in the unbroken run on either side of it in reading order — above *and* below, never across intervening text or the gutter — ranked by the shortest box gap (ties to the preceding graphic), and the graphics are matched by augmenting paths, a caption with a second choice yielding to one with none; #813 was the picture arm only looking *below* its figure, so a `Figure N:` label printed above never bound. Caption text is markdown-escaped on every arm | `assemble::pair_graphic_captions` |
| `DocItemLabel.DOCUMENT_INDEX` on the `TableItem`; an unmatched `CAPTION` cluster as a body `caption` item (#1091) | a TOC region keeps `"label": "document_index"` on its JSON table (and through a JSON read-back); a caption no graphic claims is a body-level `caption` text item, not `text`, and like docling it is no candidate for the sub/superscript stage. The Markdown is unchanged either way; the DocLang export writes it as a located `<text>`, as docling-core does — the 0.9 schema has no top-level `<caption>` (#1103) | `assemble::assemble_page`, `Table::document_index` |
| `form` / `key_value_region` containers (docling#4064) | everything > 80 % inside is a child, ordered among itself and emitted as one block where the container sits (ordering only — the children stay top-level items, no `form_area` group node) | `assemble::order_with_containers` |
| a picture ≥ 80 % inside a TableFormer table (docling#3906) | nested in the cell covering it: the cell reads `text  <!-- image -->`, the JSON grid keeps the text | `assemble::match_table_pictures` |
| `ListItemMarkerProcessor` | a leading `-` / bullet / `N.` / `a)` marker is split off the item text; a compound `3.a.` rides in the text | `docling-core` list-item processing |
| `_match_hyperlink` | the URI whose annotation rects cover ≥ 0.5 of the region, accumulated per URI, on **footnote** items only — both committed groundtruth generations carry the link into the document only there. The item is a `footnote` text whose JSON carries the raw text plus `hyperlink` (the Markdown wraps it as `[text](uri)`); a footnote is a merge skip-label, so a paragraph continuing across one is merged past it | `assemble` footnote items (`Node::LabeledText`) |
| forced OCR (docling#4061) | `--force-full-page-ocr` / `ocr_mode=full_page\|layout_regions` skip the text-layer decode outright | `pdfium_backend::for_each_page(extract_text = false)` |
| RapidOCR's text detection (#429) | the `PP-OCRv6_det_small` DB detector runs over a bitmap page alongside layout (or ahead of the orientation probe, which reuses its boxes, #571) and adds the lines no recognized cell covers (> 30 % overlap = covered, cumulatively) as orphan cells (confidence 1.0); boxes are ordered by RapidOCR's exact `sorted_boxes` (top edge, then adjacent swaps within 10 px — the first port chained rows transitively and scrambled a two-column newspaper's lines once the boxes became the crops, #570); input capped at RapidOCR's `max_side_len`, 2000 px (`DOCLING_RS_OCR_DET_MAX_SIDE`; 960 was the pre-#570 budget, ~⅓ the time, −0.02 FUNSD recall), and on a scanned PDF page render budgeted under it to the page's own text size (#945, `ocr_det::scan_budget`: the 25th-percentile projection line height kept at 20 px, never under 960 — the scanned PDF fixtures read byte-identically, a 72-dpi form's small print keeps the cap; standalone images, read at 3 px/pt, stay at the cap, their output moves under any input change; `DOCLING_RS_OCR_DET_BUDGET=full` = always the cap); on a page with a text layer the text-less regions (#931) are detected on their own crops at render resolution and the orientation probe (#936) at the 960 px cap, and the page workers share one detector session, so its ~250 MB activation arena is held once per process, not per worker (#945); without the model OCR stays region-scoped on projection strips | `ocr_det.rs`, `Worker::detect_alongside` |
| `OcrOptions.scale` = 3 + RapidOCR's `max_side_len` = 2000 on image inputs | a standalone image (its own scale-1.0 page) is read by OCR at 3 px/pt shrunk to a 2000 px longer side — docling's effective resolution (#570; a 754 × 1000 form at 2.0); rendered PDF pages keep the 2.0 px/pt render the baselines are pinned to (docling would read them at 2.52 on Letter — a deliberate divergence, kept for the baselines; `--ocr-scale` overrides either) | `page_ocr_scale` |
| pdfium's `CPDF_Page` frame, docling#4008 | glyphs, link rects and the page size live in the `CropBox ∩ MediaBox` frame with `/Rotate` applied to the display frame, so a trimmed book page or a rotated digital page lines its cells up with the render | `textparse::page_box`, `pdfium_backend::to_display_frame` |
| docling-parse's page-box filter (#529) | a glyph is kept only when its whole char box (advance × the font's ascent / descent) lies inside the display box — the CropBox, else the MediaBox — edges included: a FrameMaker print slug beside the CropBox or a tiled page's neighbouring text beyond the MediaBox never becomes a cell (it used to be clamped onto the page edge, often to zero width), and a line crossing the edge is cut at the last glyph that fits, as docling-parse cuts it; no corpus page changes | `textparse::on_page` |
| docling-parse 7.25's font decoding (docling-parse#431) — #1113 | four of its rules. A Symbol / ZapfDingbats `/ToUnicode` into the private-use symbol charset (one code point in `U+F020..=U+F0FF` the face encodes) reads through the face's encoding (`fold_symbol_font_private_use`: redp5110's `U+F0AE` is `→`, a Word bullet `U+F0B7` is `•`); the face is docling-parse's base-font match, the descriptor's font name, then `/BaseFont`, by the face name it contains (`SymbolMT`, `OpenSymbol`; Wingdings keeps its codes). A symbolic simple font that declares no base encoding reads through its embedded program's builtin encoding — after `/Differences` and `/ToUnicode`, before every Latin table (`uses_builtin_encoding`, `builtin_font_encoding`: a Type 1's encoding vector, a CFF's encoding, a TrueType's (3,0) table at `0xF000 + code`; the glyph by its PostScript name, else its TrueType Unicode code point, else, blank, a space): 2203.01017's `Dingbats` `3` / `7` are `/a19` / `/a23`, `✓` / `✗`. A Symbol / ZapfDingbats font that names no encoding reads through the face's own (a non-embedded ZapfDingbats' `l` is `●`), with the face's AFM advances when it ships no `/Widths`. Skia's hexadecimal index names (`/g15C`) are unmapped. The glyph tables are docling-parse's (the Glyph List, `zapfdingbats.txt`, the legacy delimiter pieces rewritten to Unicode 3.2's: `bracerighttp` is `⎫`). Not ported: a symbolic code no table resolves as a glyph marker; a declared encoding read through the face anyway (the FontSpecific scheme — `multi_page`'s `∞`, a deliberate divergence). Against live docling 2.150, 489 → 475 lines; only `2203.01017v2` and `redp5110_sampled` move | `symbol_fonts.rs`, `textparse::parse_font` / `decode_code` / `builtin_glyph_text`, `LoadedFont::builtin_encoding_text` |
| docling-parse's invalid font metrics (`init_ascent_and_descent`, `needs_embedded_glyph_bbox`) | a descriptor whose `/Ascent` / `/Descent` do not straddle the baseline (`/Ascent 0 /Descent 0`, a math font's two positive values) gives way to a valid `/FontBBox`, an implausible one (beyond ±8000) to its `/FontBBox` edge, and the font's glyphs are boxed vertically by their embedded outline's control box (FreeType's `FT_Outline_Get_CBox`, through the renderer's font loader; the advance stays the width): an underscore is a 0.4 pt sliver under the baseline, its own line cell, and the words around it are not joined across it (`PMC5406406 \_ 003 \_ 01.png`). Without the renderer (the wasm text layer) the font keeps the `/FontBBox` box. `2305.03393v1` exact, `redp5110_sampled` −4 (a heading in docling's place) | `textparse::font_ascent_descent`, `Font::ink_y`, `LoadedFont::glyph_ink_y` |
| docling-parse's char rect (`page_cell.h`, #528) | a glyph drawn with a non-upright text matrix — the `0 s -s 0 tx ty Tm` runs landscape `/Rotate 90` pages are built from, 180°/270°, or tilted — keeps its rotated quad, so the corner-distance contraction reads the line in its own reading order and the cell box is the quad's extent (before: zero-width boxes, every such run dropped; 180° read backwards). Upright glyphs keep the plain loose rectangle, so upright output is byte-identical. redp5110's 90° column headers (`*JOBCTL`, `QIBM_DB_SECADM`, …) now reach TableFormer and the table matches the groundtruth cell for cell (180 → 152 diff lines); the ODF-exported presentations — landscape slides drawn onto a portrait page through a 90° `cm` — get their text layer instead of OCR | `textparse::show_text`, `dp_lines::build_cells` |
| docling-parse `create_word_cells`, the TeX math encodings, the quote-normalization table | word cells are docling-parse's second contraction over the char cells (space glyphs as barriers, erased after); `CMSY*`/`CMMI*` without an `/Encoding` decode by the TeXbook tables; every curly quote → `'` | `dp_lines.rs`, `textparse.rs` |
| docling-parse's `/ActualText` (7.9, `apply_actual_text`, docling-parse#300) | a marked-content span's `/ActualText` replaces the glyphs drawn inside it, by docling-parse's rules: as many characters as cells one for one, any other count the whole text on the first cell over the union of the span's boxes; an empty text removes the glyphs, a span over graphics only or with a text longer than 8 characters and 4 per cell (`/Alt` in the wrong key) is left alone, a named property list is not resolved, the outer of nested spans wins. Before, the glyphs read through the font: a zero-advance caron at a Latin code as `OresMi` (docling#3783), the Arabic marks of a LibreOffice subset whose `/ToUnicode` skips them as WinAnsi digits and brackets (`منع8ا` for `منعًا`). | `textparse::apply_actual_text` |

**Deliberate deviations** (completeness over the metric; each costs a few
diff lines and recovers content docling drops):

* **Superscripts and subscripts** (#992): a space glyph the next glyph on
  the line is drawn over — its advance taken back by a negative `TJ`
  offset, so the pen is at or before the space's start when the next glyph
  lands — is dropped before the contraction
  (`textparse::drop_overprinted_spaces`), and a word that starts only on a
  baseline shift with no horizontal gap (docling-parse's
  `word_baselines_compatible`) *inside a bracket* joins the line text
  without a space while staying its own word cell (`dp_lines`). Arbortext
  sets EUR-Lex's footnote markers as `( 1 )` with cancelled spaces around a
  raised digit; the page shows `(1)`, pdftotext and pdfium read `(1)`,
  docling-parse 7.24.2 reads `( 1 )` — and `( 1 )` for a raised digit drawn
  with no spaces at all. Outside brackets docling's space stays
  (`SynthTabNet 1 .`, `1 https://…`): with no bracket test the join cost
  five groundtruth files. A space the next glyph merely kerns into, a
  trailing space before the cursor jumps to the next line, and every space
  of a rotated run (its loose boxes stack on one edge) stay. The groundtruth
  table is unchanged; one snapshot moves (a LaTeX figure's `𝐤𝑡𝑅 ]}` →
  `𝐤𝑡𝑅]}`).
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
* **A `text` box over smaller paragraph boxes** (#1101): a low-score
  `text` region containing higher-score `text` boxes that cover only some of
  its lines (a paragraph's continuation lines, its tail) used to keep the
  leftover lines — the first lines of unrelated paragraphs — as one block.
  The stack is rebuilt top to bottom instead: a container line or contained
  box joins the next one when its last line runs on (line gap < 1.5 line
  heights, flush left and full — ≥ 3/4 of the width, or the next word would
  not have fit — and no sentence end), every other run of
  container lines is a paragraph of its own, and the container goes
  (`assemble::split_text_containers`). Only a stack is resolved: a
  contained box under half the width is a column and leaves the container
  to cell ownership, unless it is a paragraph's wrapped last line — one
  line at the left edge, directly under a full container line, nothing
  beside it (#1107, the dotnet reference's pp. 1783 / 1820). docling's regular
  `_remove_overlapping_clusters` would merge the whole group into the
  container — reading order right, paragraph breaks lost.
* **Runaway TableFormer rows**: a decode that emits `lcel` until `MAX_STEPS`
  without a row break is rejected once a row passes 256 tags and the region
  takes the geometric table path (docling ends at a 1 × 1 table there).
* **Degenerate TableFormer sequences** (docling#4592, #1000): a predicted
  OTSL with no row break at all, or one that stops within two tags of the
  1024-step budget without a final `nl` (cut mid-row), is discarded with a
  warning, as docling does. docling then emits an empty table whose
  uncovered in-box words become one text child; the port keeps its
  geometric table path for the region instead, which keeps every word and
  still yields a grid. On the corpus one region trips it: DeepSeek-V3's
  `relative_expert_load_multi_13-18` chart, over-detected as a table, whose
  decode ran to 1024 tags without closing its last row — the grid that
  scattered the axis labels across the wrong cells is now the geometric one
  (one snapshot moved; groundtruth unchanged).
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
skipped_1/2page (Korean) + right_to_left_03 in picture detection /
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

**A GPU for the shim is a hard requirement**: on CPU (fp32 `transformers`,
4 vCPUs) granite-docling-258M generated ~0.1 tok/s — 3.4 h for one
academic page — while both clients cap a page request at 600 s
(`--timeout` / `VLM_TIMEOUT`). Reproduce with:

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
that fills `MAX_STEPS` with ordinary rows keeps its structure when the budget
ends on a row break; since docling#4592 (#1000) one cut mid-row is rejected
like a sequence with no row break at all. Snapshots 97/97
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
(docling-parse 7.24 keeps an unknown code-like name as text, `SM590000`), and
**unmapped glyphs as U+FFFD** (#1003, docling#5070): a painted glyph no
source maps to Unicode — a bare glyph-index name (`/g115`), a code a
composite font's ToUnicode lacks, a NUL a ToUnicode maps to — reads `�`, so
the loss stays visible and the text-quality score counts it instead of the
glyph silently vanishing (`mL⁻¹` reading `mL1`); one whose embedded program
paints nothing is a space and a word boundary, as docling-parse reads it.
A CJK composite font without ToUnicode reads through Adobe's CID tables
first (#1039). One deliberate difference: a composite font with neither a
ToUnicode nor a collection table reads `�` where docling-parse takes the CID
as a code point (redp5110's list bullet, CID 114, reads `r` there) — and an
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

## Remaining blockers

What the table above leaves, by kind (#60–#63 tracked the model-level ones
and are closed with these residuals):

1. **TableFormer structure on hard crops** (#60). The matcher is docling's
   (`tf_match.rs`, `MatchingPostProcessor` with docling-ibm-models 4.0.1's
   orphan fallbacks, verified against docling's Python post-processor on
   identical inputs through `DOCLING_RS_TF_MATCH_DUMP`); what differs is the
   OTSL tag stream itself on a few crops (multi-row headers, spans), and one
   structure difference cascades through the padded columns.
   `DOCLING_RS_TF_SIMPLE_MATCH=1` reverts to the pre-port matcher.
2. **Layout classification** (#61). docling's `LayoutPostprocessor` is
   ported (the parity table above); what remains is the model's own reading
   of borderline regions — `table_mislabeled_as_picture`'s over-detected
   survey, `table_misidentified_as_form`'s form container.
3. **Reading order on dense pages** (#62): title-page author blocks, and
   paragraph pairs in a two-column body (`elsevier-00`).
4. **Text-layer details docling-parse decides differently**: line-end hyphen
   fusion, soft hyphens, the bullet glyph mappings (`multi_page`'s `∞`,
   redp5110's CID `r`), the spacing of monospace punctuation, docling-parse
   7.25's em-based word gap (docling-parse#437; our gap is 7.24's adaptive
   one with the #921 clip), and the last part of its font decoding
   (docling-parse#431): a symbolic font's code that no table resolves
   reads as a glyph marker there, its Latin-table letter here.
5. **Features not ported yet**: AcroForm form fields (`extract_form_fields`,
   `acroform_sample`), hyperlinks on every text label (`_match_hyperlink`;
   ported for footnotes only, the parity table above).
6. **The model-input renderer** (#478). The default build renders with the
   pure-Rust renderer, mean |Δ| ≈ 1 / 255 against docling-parse's — close,
   not identical, so heron's borderline labels move on the ML-borderline
   fixtures ("The PDF stack" above: 75/101 snapshots exact and 647
   groundtruth lines against the shim's 101/101 and 511). Closing it means Blend2D's 8-bit coverage values.

Performance — the parallelism, the profiling notes, INT8, GPU providers,
ONNX Runtime upgrades — lives in [`PERFORMANCE.md`](PERFORMANCE.md).
