# Migrating Docling to Rust — docling.rs

A port of [docling](https://github.com/docling-project/docling) from Python to
Rust. This document is the **current status**: what is migrated, how it compares
to upstream docling, and what is intentionally not done yet. (The original
phased plan is kept at the end as history.)

## The migration in numbers

| | Lines of code | Files |
|---|---|---|
| Upstream Python: `docling` 2.114.0 (code wheel `docling-slim`) | 70,132 | 242 |
| Upstream Python: `docling-core` 2.87.1 (document model, serializers, chunkers) | 30,888 | 103 |
| **Upstream total** | **101,020** | **345** |
| docling.rs — the port itself (`docling-core`, `docling`, `docling-pdf`, `docling-asr`, `docling-cli`) | 47,456 | — |
| docling.rs — beyond upstream's packages (HTTP API, RAG, Python/Node/wasm bindings) | 11,327 | — |
| **docling.rs total (`crates/*/src/**.rs`)** | **58,783** | **121** |

Roughly **half the line count for the same behavior** — despite Rust carrying
type/lifetime annotations Python doesn't — because the port reimplements from
observed behavior rather than translating structure, and because byte-for-byte
conformance testing against live docling (not code review) is what pins
correctness. The Python side also leans on compiled dependencies that the Rust
side had to re-port or re-integrate natively (docling-parse's C++ PDF text
extraction became `textparse.rs`; HF `transformers` inference became hand-rolled
ONNX pipelines), so the scope ratio understates the ported surface.

**Timeline:** the first commit landed **2026-06-27**; the migration — every
input format including PDF/ML, ASR and video, plus the serve/RAG/bindings
extras — was done by **2026-07-23**: **26 days**, ~600 commits (development continues
past that point — serve async API, confidence scores, PDF-parity work), migrated by
[artiz](https://github.com/artiz) + Claude (Anthropic's Claude Code, doing the
bulk of the porting under review).

> **Status: the format migration is complete.** Every document format in
> docling's pipeline is supported — including **audio/ASR** (Whisper via ONNX,
> in `docling-asr`) — plus Markdown (legacy + a Rust-only *strict* mode),
> docling-native **JSON** output, **DocLang (`.dclx`)** output (docling 2.110's
> OPC archive), **image extraction**, and **MHTML** (docling#4184's
> `InputFormat.MHTML`). The declarative formats are pure-Rust and checked byte-for-byte
> against *live* docling; the PDF/image/METS ML path lives in `docling-pdf`
> (a pure-Rust PDF text parser + pdfium rasterization + ONNX
> layout/TableFormer/OCR + a port of docling-parse's line sanitizer) and is also
> measured byte-for-byte against live docling — **6 / 14 PDF fixtures exact, 7 / 14
> whitespace-normalized** (see `PDF_CONFORMANCE.md`), with a snapshot baseline
> guarding against regressions. `cargo test` is green (unit tests + a 159-source
> output-regression suite).

**At a glance** (for a first-time reader from the docling side):

| | |
|---|---|
| **What** | A Rust port of docling's converter, backends, and discriminative PDF/ASR pipelines; same `convert → DoclingDocument → export_to_markdown()/json()` shape, single static binary, no Python/torch at runtime |
| **Conformance** | Declarative formats byte-for-byte vs *live* PyPI docling (most 100%, see §2); `.dclx` DocLang output ≈97% mean vs docling's own `.dclx`, OOXML all byte-exact (§2); PDF ML path 6/14 fixtures byte-exact, rest close; every optimization is gated on this not regressing |
| **Performance** | PDF ML pipeline **4.3× faster warm / 4.7× end-to-end** than Python docling at 2.3–2.6× less peak RAM (INT8 + SIMD, conformance-validated); declarative formats 20–60× warm, ~60× less RAM; XLSX sheets / PPTX slides additionally fan out over rayon (~2–3× on many-sheet/slide files, conformance byte-identical); details + methodology in [`PDF_CONFORMANCE.md`](./PDF_CONFORMANCE.md) |
| **Models** | docling's own checkpoints, no retraining: layout heron and TableFormer are format-converted to ONNX by `scripts/install/export_layout.py` / `export_tableformer.py` (CodeFormula by `export_code_formula.py`); PP-OCRv3 and Whisper tiny ship as their upstream ONNX exports; INT8 variants are calibrated post-training quantizations (`scripts/install/quantize_models.py`) |
| **Tracking upstream** | See [§9](#9-keeping-up-with-upstream-docling): conformance is measured against the *latest published* docling on demand, so an upstream release that changes output surfaces as a concrete per-fixture diff |
| **Not ported (by design)** | local in-process VLM full-page inference (§5 — the remote OpenAI-compatible VLM pipeline **is** ported, #77); inline formatting is baked into text rather than structured fields (§4). The optional enrichment models (picture classification, code, formulas) **are** ported — opt-in `do_picture_classification` / `do_code_enrichment` / `do_formula_enrichment`, ONNX like the rest of the stack |

---

## 1. Architecture

The layers mirror docling's:

| Layer | docling (Python) | `docling.rs` (Rust) |
|---|---|---|
| **Data model + serializers** | `docling-core` | `docling-core` — `DoclingDocument`, the `Node` tree, Markdown + JSON serializers, base64 |
| **Converter** | `docling/document_converter.py` | `docling.rs` — `converter.rs` (format dispatch + XML content sniffing) |
| **Backends** | `docling/backend/*` | `docling.rs` — `backend/*` (one per format) |
| **PDF/ML pipeline** | `docling/pipeline/*`, `docling/models/*` | `docling-pdf` — pdfium + ONNX layout/OCR + assembly |
| **Audio/ASR pipeline** | `docling/pipeline/asr_pipeline.py` | `docling-asr` — symphonia decode + log-mel + ONNX Whisper |
| **Chunking** | `docling-core` chunkers (`HierarchicalChunker`/`HybridChunker`) | `docling-core::chunker`, re-exported as `docling::chunker` |
| **CLI** | `docling/cli` | `docling-cli` (incl. warm batch mode: `--input GLOB --output DIR [--jobs N]`) |
| **Beyond upstream's packages** | docling-serve (separate repo) | `docling-serve` (HTTP API), `docling-rag`, Python/Node/wasm bindings, GPU execution providers (`cuda`/`tensorrt`/`directml`/`coreml` features, `DOCLING_RS_EP`) |

```text
crates/
├── docling-core/   # DoclingDocument, Node model, markdown/json/doclang/doctags serializers, chunker.rs, confidence.rs
├── docling/        # DocumentConverter, source/format detection, backend/*.rs, ooxml.rs
├── docling-pdf/    # pdfium_backend, layout (RT-DETR/ONNX), ocr (PP-OCRv3/ONNX), assemble, mets
├── docling-asr/    # audio decode (symphonia), mel.rs, whisper.rs (ONNX), tokenizer.rs
├── docling-onnx/   # shared ONNX Runtime EP selection (DOCLING_RS_EP, cuda/tensorrt/directml/coreml features)
├── docling-cli/    # `--strict`, `--to md|json|dclx|chunks|latex`, `--images …`, `--pages`, `--ocr-lang`, serve subcommand
├── docling-node/   # Node.js/Bun N-API bindings (napi-rs), published to npm as `docling.rs`
├── docling-py/     # PyO3 bindings (maturin), published to PyPI as `docling-rs` (strangler-fig over docling-core)
├── docling-rag/    # RAG layer on top of the converter (chunking, embeddings, vector search, REST API)
├── docling-serve/  # HTTP conversion API (docling-serve analogue): sync + async/batch /v1/convert over a warm pipeline
└── docling-wasm/   # WebAssembly bindings: declarative converters + text-layer PDF in the browser
```

The public API is unchanged from day one:

```rust
use docling::{DocumentConverter, SourceDocument};

let result = DocumentConverter::new()
    .convert(SourceDocument::from_file("input.docx")?)?;
println!("{}", result.document.export_to_markdown());   // or .export_to_json()
```

---

## 2. Format coverage

Conformance is measured against the latest **published** docling (installed from
PyPI; run via `scripts/conformance/conformance.sh <fmt>`), not the committed groundtruth
`.md` (which predates docling-core's current table serializer — see §4).
"Exact" = byte-for-byte.

### Declarative formats — pure Rust, no models

| Format | Backend | Status |
|---|---|---|
| Markdown | `markdown.rs` (pulldown-cmark) | **10/10 exact**; #319 (docling 2.122's docling#3817): tables written without edge pipes (`Region \| Q1` over `--- \| ---`) are normalized up front so pulldown parses them like the canonical spelling; a leading UTF-8 BOM no longer hides the first heading (docling#4109) |
| CSV | `csv.rs` (`csv` crate) | **9/9 exact**; `.tsv` routes here too (#208 — the delimiter sniffing already covers tabs; a docling.rs extension, upstream accepts only `.csv`); #319: a quoted field spanning lines no longer defeats delimiter sniffing (4 KiB fallback sample, docling#3985) and an Excel-style leading BOM stays out of the first header cell (docling#4098) |
| HTML | `html.rs` (scraper/html5ever) | **32/32 exact** modulo the stored corpus's compact-table format (docling 2.126 groundtruth; #371: non-UTF-8 input decodes in BeautifulSoup's `UnicodeDammit` order — BOM, then the declared XML/`<meta charset>`/`http-equiv` encoding (bs4's search windows, WHATWG labels via `encoding_rs`, so `iso-8859-1` reads as windows-1252 like a browser), then strict UTF-8, then windows-1252 — instead of failing the UTF-8 check; bs4's optional chardet/charset_normalizer guess between declaration and fallback is the one step not reproduced; #364: docling#4050's `<figure>` dispatch — every child but the `<figcaption>` is walked, a figure's pictures take the figcaption as caption, a figure without a picture attaches the figcaption to its leading table or emits it as a standalone caption item (`wiki_duck`'s `[Mallard duckling preening](/wiki/Mallard)`); pictures folded into a list item print after the item line with plain newlines, the GFM hard-break rule applying to the item's own text only); `wiki_duck` — rich table cells, caption run spacing, indicator images, `<footer>` furniture all match docling 2.112; #284: an *unclosed* inline tag (`<a name=…>`, `<b>`, `<font>` — endemic in legacy authoring-tool HTML) legally swallows subsequent blocks under html5ever's spec parsing, where Python's parser recovers by reparenting — inline wrappers hiding structured blocks (tables, lists, headings, code, figures) are now block-walked so the structure surfaces; pure text containers under a well-formed `<a href>` keep rendering as links; `aria-hidden` subtrees are dropped like docling's `_is_invisible_tag` does (Wikipedia's decorative logo icon and its sticky-header duplicates), and an anchor wrapping several images hangs its href on each captioned picture; #382: docling#4216's header flags — a `<tr>` of `<th>`s that each span several rows is a pivot table's *row*-header row, so its cells are `row_header` and not `column_header` (they label the rows they span into, and flagging them as column headers made docling-core 2.96 fold the first data row into the Markdown header), and a lone `<th>` beside `<td>` data is a row header too; all 66 cells of `example_08` now match upstream's regenerated groundtruth flag for flag. **JSON: structurally identical to docling's on 32/32** (every key and value but `origin`/`version`; `wiki_duck` modulo one caption item — its stored JSON predates docling#4050, whose figure dispatch the refreshed Markdown groundtruth already shows). The flat node stream that Markdown / DocLang / LaTeX render is not the shape upstream's *JSON* has, so `html_tree.rs` ports `HTMLDocumentBackend`'s document construction call-for-call into a docling-core `ItemTree` (`DoclingDocument::tree`) that the JSON export serializes instead of deriving a tree from the nodes: everything after a heading nests under it (`h1` = title, `h2`–`h6` = `section_header` level `n-1`, `header-N` section groups filling a skipped level); a paragraph is docling's annotated parts (one per source text node with its ancestor `<b>`/`<i>`/`<u>`/`<s>`/`<sub>`/`<sup>`/`<code>` formatting and `<a href>`), adjacent same-annotation parts merged, two or more parts an `inline` group of one `text`/`code` item each carrying `formatting` and `hyperlink`, one part a lone item; `<br>` splits (`\n` for one, a new paragraph for two); a mixed-formatting `<li>` is an empty `list_item` over an inline group, a `<dt>` a bold item whose `<dd>`s form a `descriptions` list group under it, `<ul>` directly inside `<ul>` its own nested group; a rich table cell's items are re-parented under a `rich_cell_group_{tables}_{col}_{row}` group of the table (named with upstream's lazily advanced column, i.e. the *previous* cell's), the cell carrying `ref` to it, cell text docling's raw `get_text`, declared spans unclamped; a picture's caption is a body-level `caption` item; the `<title>`, everything before the first heading and a `<footer>` (a `section` group) are `furniture`; `<details>` a `section` group; `field_value` items carry `kind`; items are numbered in upstream's creation order. Markdown / DocLang / LaTeX / chunks are untouched (they keep reading the nodes) |
| AsciiDoc | `asciidoc.rs` (regex) | **4/4 exact**; #365 ports docling 2.125–2.126. docling#4118: the item pattern widens to `*`, `-`, `.`/`..`/`...`, `1.`, `a.` (a dotted marker carries its own depth, so `..` nests under `.` with no indentation), a `....` literal block becomes a code item, a lone `+` keeps the next literal block or image *inside* the open item (printed after the item line, indented to its depth, like the pictures the HTML backend folds into an `<li>`), and a non-list line now ends the list and is parsed instead of being swallowed. docling#4156: image loading moves behind the converter's `fetch_images` (docling's `fetch_images` + `enable_local_fetch`/`enable_remote_fetch`) — by default an `image::` macro yields a picture with **no** `ImageRef`, where docling ≤ 2.125 fabricated a 128×128, 70-dpi `file://…` one; docling#4173 dropped `max_image_data_base64_bytes`, which was never ported. Numbering follows docling-core: an explicit `12.` marker prints verbatim, every other enumerated item is numbered by its *position among the group's children* — where a nested list is a sibling of the items, so it takes a position (`. a` / `.. b` / `. c` → `1.`, `1.`, `3.`) — and a group whose first item is a bullet renders all of them as bullets. Three deviations: (a) a line that is exactly `word.` stays a paragraph, as it does for docling reading a **stream**; reading the same bytes from a **path** docling keeps the line's newline, so the widened `\w+\.` pattern makes `Intro.` an empty list item and loses the text; (b) symmetrically, our unterminated `....` block matches docling-on-a-path — docling-on-a-stream appends a trailing blank line to it; (c) a paragraph still pending when a list opens is emitted as a paragraph, where docling re-parents it into that list group and glues it to the last item. (A fourth is gone with #385: a break in *explicit* markers — `1.` then `5.`, or `*` beside `1.` — no longer reads as a new list, since the serializers take every list boundary from the backend's flag instead of guessing it from the numbering.) A picture or literal block nested in a list item is carried in the item's own text (this node model is flat), so Markdown, LaTeX and DocLang render it exactly where docling does, while the JSON has no separate `picture`/`code` item for it and an in-list picture keeps no fetched bytes — the same tradeoff the HTML backend already makes for images inside an `<li>`. Pre-existing: the line that closes a table (docling's `elif in_table` arm consumes it) is parsed here instead of dropped, so a text line written directly under the last table row survives |
| DeepSeek-OCR Markdown | `deepseek.rs` | **3/3 exact** (auto-detected VLM-token variant); Unlimited-OCR grounding output (#322 / docling#3944) normalizes into this shape and shares the parser |
| Chandra layout HTML | `chandra.rs` (#322) | docling 2.123–2.125's `parse_chandra_html` semantics on our node model: `data-bbox`/`data-label` divs → headings, text, span-aware tables (Form-held tables included, docling#4135), lists, figures, formulas, code, page furniture; `<br>` is spacing (docling#4092); 0–1000 boxes → DocLang 0–511 `Located` provenance |
| XLSX | `xlsx.rs` (calamine) | **13/13 exact** Markdown vs docling 2.127.0 (groundtruth refreshed to it, the #395 reporter's file mirrored as `xlsx_empty_merge_before_data`), and the **JSON is structurally identical for 13/13** — everything but `origin`/`version` and, where a sheet holds a raster image, the PNG bytes PIL re-encodes: items are numbered as docling creates them (a sheet's tables with their labels, then its images, then its charts, in drawing order — the `seq` a `Node::Prov` carries) while the group's children follow docling's position sort, stable over that creation order; every sheet is a page (`pages`, sized like docling's `_find_page_size` — the largest right/bottom edge of its items in cell units, 0×0 for an empty sheet) and every item carries docling's provenance verbatim — its cell-index box, top-left origin, `charspan` `[0, 0]` — through a `Node::Prov` wrapper the JSON reads (the 0–511 DocLang grid cannot round-trip those integers; DocLang, Markdown, LaTeX and the chunker render the wrapped node unchanged); a chart's title caption is a sibling of the picture in the sheet group, listed before it, with the chart's box and a charspan over the caption text, as docling's office backends add it (PPTX charts get the same); `TableData.orientation: rot_0` is written for every table and chart grid; a picture's `image.size` is floats and `image` precedes `annotations` — pydantic field order; #405: a chart *sheet* exposes its chart the way a worksheet's does — the sheet's `absoluteAnchor` drawing (no cell range; docling's bbox is `(0, 0, 0, 0)`) was skipped, leaving the group empty — and every chart picture now carries docling's `meta` in the JSON: `classification` with the chart kind as its one prediction and `tabular_chart.chart_data`, the series reconstructed as a `TableData` (row 0 the series names as column headers, the category column as row headers), for XLSX and PPTX/ODF/DOCX charts alike; `xlsx_chart_sheet` (data sheet + chart sheet + the same chart on a worksheet) is mirrored with 2.127 groundtruth and the stale `xlsx_03_chartsheet` groundtruth refreshed; #410: a merged range is *one* `TableCell` in the JSON — the anchor's offsets with `row_span`/`col_span`, repeated across the grid positions it covers, as docling writes it — where we wrote a 1×1 cell per covered position with the anchor's text copied into each (Markdown was already right, the expanded grid). The serializer derives the spans from the structure overlay's continuation flags, so backends without first-class cells (XLSX, PPTX `rowSpan`/`gridSpan`, DocLang) all get them; `xlsx_merged_range` mirrored, and the refreshed `xlsx_01`/`_04`/`_07` and `powerpoint_sample` JSON now match upstream's cell spans exactly; #395: a sheet is scanned in docling's `_find_true_data_bounds` frame — the union of the valued cells and *every* merged range — so a merge that starts above or left of the first value no longer underflows the rebase onto calamine's clipped range (it panicked), and, as upstream does, an empty merged range no longer seeds a table of its own (docling seeds only from a cell that carries a value) (#366: `.xltx`/`.xltm` templates route to the same reader, docling#4178) (incl. chart captions/classification/data grids); the JSON carries docling's structure too — one `sheet` group per worksheet (named after it, `invisible` content layer for a hidden sheet, which puts its tables in the JSON instead of dropping them) and every cell note as a `comment-{sheet}-{cell}` `comment_section` on the notes layer, with `comments` back-refs on the item whose cell range covers the commented cell (docling's `_find_cell_item`). Note the upstream asymmetry the refs mirror: a spreadsheet comment goes through docling-core's `add_comment`, which links the **note text** (`#/texts/N`), while the docx backend overwrites that with the **group** (`#/groups/N`) so replies group together. Markdown and DocLang are unaffected — docling emits no sheet heading (that `## Sheet1` lives only in older stored groundtruth) and DocLang has no group element |
| XLSB (binary Excel 2007+) | `xlsx.rs` → calamine's `Xlsb` reader (#210) | **docling.rs extension — upstream has no XLSB backend**; tables, hidden-sheet layering and page breaks match the XLSX path; drawings/charts/comments/merges aren't exposed by the binary reader |
| PPTX | `pptx.rs` (roxmltree) | **8/8 exact**; #390 (docling#4190): a chart's title caption is a child of the slide group, listed ahead of the chart picture, with the chart's box and a charspan over the caption text, and every paragraph's `charspan` is its own (docling used to reuse the shape's) — the mirrored `pptx_chart` groundtruth is upstream's post-#4190 copy and our JSON is structurally identical to it (the only residue is the 0–511 grid quantization of the boxes noted below); a subtitle placeholder stays a `text` paragraph on purpose (docling#3785, re-settled in #4190) — not a bug; #406: a paragraph whose bullet or numbering comes from the owning shape's list style (`a:txBody/a:lstStyle/a:lvl{N}pPr/a:buChar|a:buAutoNum`, selected by the paragraph's `a:pPr/@lvl`) is a list item, as docling's `_get_effective_list_marker` resolves it — a `buChar`/`buBlip` bullets it, a `buAutoNum` numbers it, a `buNone` keeps it a paragraph, and an indented paragraph with no marker anywhere is a list item (docling's `level > 0` fallback); the level only selects which level properties supply the marker, the items are not nested. Not walked yet: the layout placeholder's list style and the master's `p:txStyles`, for which the body-placeholder default still stands in; #402: a slide's speaker notes and review comments reach the **JSON** as text items on docling's `notes` content layer (they were parsed all along, but only DocLang rendered them, so a consumer reading JSON alone could not see them). Markdown is unchanged — it serializes the body layer. Each slide is also a `chapter` group named `slide-{0-based}`, as docling writes it, and every slide is recorded as a page sized in EMU, so items carry provenance: on `powerpoint_sample` the group tree (9 groups, same refs, names, labels and parents), the page map and every `charspan` are identical to docling's, and the `prov` boxes agree to within one DocLang grid step (≤ 0.013 in — our JSON expands the 0–511 `<location>` grid both sides round to, where docling's JSON keeps the raw EMU; the same quantization the PDF path has always had). An item with no geometry — a speaker note — carries docling's zero bbox rather than a box spanning the slide. Page *furniture* is still dropped from our JSON where docling keeps it (55 items on `wiki_duck`), which is a wider change of its own, and a slide comment is a bare notes-layer text where docling wraps it in a `comment_section` group. (docling 2.120 parity — #320: 3-D chart variants are `other_chart` without a data grid like python-pptx's unsupported plots (docling#3972), a shape at x = 0 EMU keeps its own bbox (docling#3990); #249: slide shapes convert in visual reading order — top-sorted, 0.05"-tolerance row banding, left-to-right within a row — instead of XML/z-order, at slide level and inside groups); docling#4089 (EMF/WMF pictures kept as positioned pictures without image data when LibreOffice is absent) already matches the placeholder-picture behaviour here |
| DOCX | `docx.rs` (roxmltree) | **33/33 exact** modulo the committed corpus's compact-table format (§4; 26/33 byte-identical to upstream's stored `.md`, the other 7 differ only in `\| - \|` vs padded separators) — #385: a list item's `first_in_list` is Word's list identity — a `numId` the previous list item did not carry, or body text since it, opens a new list; the same `numId` continues one across nesting and across an empty spacer paragraph (docling's `_manage_list_structure` and its ListGroup cache) — and the serializers draw every list boundary from that flag, no longer guessing it from a kind flip or a number gap (`docx_lists`/`docx_list_blank_spacer` unchanged, `unit_test_headers_numbered` now nests its list groups as upstream does); #409: a code paragraph's text is python-docx's `Paragraph.text`, as docling's `raw_paragraph_text` is — so an in-paragraph `<w:br/>` (its own run, or between the `w:t`s of one run) is a newline and a tab is a tab, where flattening the subtree to `w:t` alone joined `total = a + b` and `print(total)` into one line of the fenced block; plain table cells read the same way (python-docx's `cell.text`); #408: the trailing run group of a paragraph closes under the format that *opened* it, as in docling's `_get_paragraph_elements` — a whitespace-only run never opens a group, so bold/italic text followed by Word's unformatted trailing-space run keeps its `**…**`/`*…*` (we took the last run's plain format and dropped the markers, in body text and list items alike); both reporter files are mirrored (`docx_bold_tail`, `docx_code_breaks`) and match upstream's Markdown byte-for-byte; #400: a run's inner content follows python-docx's `CT_R.text` — where docling's paragraph text comes from — so `<w:noBreakHyphen/>` is the plain `-` it renders as (dropping it glued `In` + `Transit` into `InTransit`), `<w:ptab/>` is a tab, and a `<w:br>` is a newline only when it is a *line* break: a page or column break has no text equivalent there; #389: docling#3946/#3951 — a table cell Word wrapped in a content control (`w:tr/w:sdt/w:sdtContent/w:tc`, how it writes cover pages and document-property fields) is collected as a cell of its row, so a 1×1 layout table keeps its content and a wrapped cell no longer slides every later cell of the row a column to the left; the mirrored `docx_sdt_table_cells` fixture matches upstream's grid and text items exactly; docling 2.126 parity (#363): docling#3917 resolves a style's numbering through its `basedOn` chain with `numId` and `ilvl` inherited independently (Word's stock `heading 2` carries only `ilvl` and takes `numId` from `heading 1`; refreshed `unit_test_headers_numbered` adds the inherited `2.3` heading), docling#4087 renders list markers with the level's `numFmt` — letter (a…z, aa…), roman, `decimalZero` — per placeholder and per hierarchical part, and keeps the `lvlText` suffix on non-decimal levels (`%2)` → `a)`, refreshed `docx_lists` Test 10); docling 2.125 parity: #320 mirrors docling#3952's content-control-with-picture fixture, already handled by the `<w:sdt>` walk, docling#3729's Strict OOXML pair (`Strict.docx`/`Transitional.docx` — the `purl.oclc.org` namespaces already resolve through the namespace-agnostic walk), docling#3760's refreshed `unit_test_headers_numbered` (a heading whose numbering level has `numFmt none` — Word's invisible numbering — keeps its outline `numPr` but gets no computed `1.2` prefix; only the visible formats decimal/roman/letter/decimalZero number a heading) and docling#4036's textbox groundtruth (pictures anchored inside textboxes) and reviewer comments as first-class `comment_section` groups — each `w:comment` becomes a notes-layer group named `comment-{w:id}` holding the `[author: … , time: …]: text` note, and every body item covered by its `w:commentRangeStart`/`End` (or anchored by a bare `w:commentReference`) carries docling's `comments: [{"$ref": "#/groups/N"}]` back-ref, so the JSON groups/refs are byte-identical to upstream's; Markdown, LaTeX and DocLang are unchanged (DocLang keeps the flat `<layer value="notes"/>` item upstream writes); #248: all sections' headers/footers as furniture incl. first-page + regular pairs, resumed ordered-list numbering per `numId`, body text after a blank spacer stays inside the list group; #270 / docling#3961: headings detected by the style's `w:outlineLvl` — localized/custom heading styles ("Nadpis1", "Rubrik 1") promote by OOXML's own language-independent marker, outlineLvl 9 stays body text, Title/Subtitle-ish styles keep their own branch; the name check also covers the style *id* and one-hop `basedOn` id/name, so a custom style based on "Heading 2" or an English "Heading1" id under a localized display name promotes without any outline level — direct formatting with no style at all stays body text, as in docling). **JSON: structurally identical to docling's on 36/36** (every key and value but `origin`/`version`, and the `image` payload of the pictures upstream renders through LibreOffice — blip-less DrawingML shapes, the textbox shapes of `textbox`, the EMF of `test_emf_docx` — which carry no payload here; a raster picture's bytes are the embedded file's where PIL re-encodes to PNG). The flat node stream is not the shape upstream's JSON has, so `docx_tree.rs` ports `MsWordDocumentBackend`'s construction call-for-call into the docling-core `ItemTree` the JSON export serializes (the same XML readers as the Markdown walk — styles, numbering, run text, code detection, list markers, chart parts): everything after a `Title`/heading nests under it, a skipped heading level is a `header-N` section group; a paragraph is python-docx's run/hyperlink/content-control sequence grouped by `Formatting` (`_get_paragraph_elements`; bold also from the paragraph mark's run properties and the style's `basedOn` chain), two or more groups an `inline` group of `text` items each with `formatting` and `hyperlink` (`AnyUrl`-normalized, `Path` otherwise), one group a lone item; `_manage_list_structure`'s open-parents stack (`list` groups per `numId`/`ilvl`, the group cache a blank spacer paragraph keeps and the empty text item it *deletes* when the list resumes, `header-`less `list_item`s with `enumerated`/`marker`, a mixed-format or equation item an empty `list_item` over an inline group); code paragraphs merged into the previous `code` item under upstream's last-child / last-text / same-layer rule with buffered blank lines; standalone `formula` items and inline `text`/`formula` alternation; tables with python-docx's `tblGrid` column count, `gridBefore`, `vMerge` row spans, raw `cell.text`, `$…$` equations, and rich cells (several paragraphs, non-paragraph blocks, blips, formatted runs, a code style) walked in an isolated list context then re-parented under `rich_cell_group_{tables}_{col}_{row}` with the cell's `ref`; a 1×1 table walked as body; pictures per `a:blip`/`v:imagedata` (a `picture_area` group for several, `invisible` layer for a spacer), one payload-less picture per blip-less DrawingML paragraph, charts as classified pictures with `tabular_chart` data and a body-level caption; textboxes (`txbxContent`, VML, `wps:txbx`) a `textbox` section with their paragraphs deduplicated as upstream does, `shape-text` for DrawingML text outside any; headers/footers `page header`/`page footer` furniture sections; reviewer comments `comment_section` groups on the notes layer with `comments` back-refs on the paragraph's items. Markdown / DocLang / LaTeX / chunks read the flat nodes and are unchanged |
| DOC (Word 97–2004) | `doc.rs` (native [MS-DOC]: CFB + piece table + PAPX/CHPX/STSH + Escher) | byte-identical Markdown to the DOCX backend on fixtures converted to `.doc` (headings, ordered/bullet lists, tables, bold/italic, and embedded pictures — inline PICF + floating shapes with decoded PNG/JPEG bytes); docling reaches these only by shelling out to LibreOffice (PR 3804); styles that are not the built-in Heading 1–9/Title still promote to headings through their `sprmPOutLvl` (#270 — a LibreOffice-written legacy file names its heading styles in the document language with a "user" sti, leaving the outline level as the only marker) |
| XLS (Excel 97–2004) | `xls.rs` (calamine BIFF8 + the XLSX region detection) | byte-identical to the XLSX backend on converted fixtures |
| PPT (PowerPoint 97–2003) | `ppt.rs` (native [MS-PPT] + OfficeArt shape walker) | **byte-identical to the PPTX backend** on the sample fixture: tables reconstructed from shape-group geometry (spans included), bullet lists (StyleTextProp) and numbered lists (PP9 autonumber), titles, z-order |
| WebVTT | `webvtt.rs` | **4/4 exact**; #366: karaoke cue timestamps (`<00:00:00.389>`) are stripped without splitting the span (docling-core#744), text after a multi-line voice span stays in reading order (docling#4105), bare CR / CRLF line terminators parse (docling-core#749, docling#4157) — pinned by unit tests |
| EBCDIC (.ebc) | `ebcdic.rs` (native decode tables + copybook layouts, #252) | **3/3 exact** vs live docling 2.119 on the mirrored `ebcdic-parser`-derived corpus — incl. the four-schema packed-decimal sample and `Decimal`-spec rendering (`0.0000`, `0E-7`); layout via `ebcdic_layout` option (all surfaces) or the `<stem>.layout.json` sidecar (docling.rs convenience; upstream requires explicit backend options) |
| Email (.eml, .msg) | `email.rs` (mail-parser) + `msg.rs` (native CFB/MAPI → RFC 822 projection, #251 — docling reaches .msg via python-oxmsg) | **4/4 exact** incl. both .msg fixtures and the opt-in `list_attachments` section (docling 2.119's `EmailBackendOptions.list_attachments`, plumbed as lib builder / CLI `--list-attachments` / serve `list_attachments` / py kwarg / Node `listAttachments`); `.eml` `Date:` now spells UTC as `+00:00` like Python's `isoformat()` (was `Z`) |
| EPUB | `epub.rs` → HTML backend | #366: manifest hrefs are percent-decoded before the archive lookup (docling#4199, `chapter%201.xhtml` → `chapter 1.xhtml`); **0/1** — the single fixture is 4 diff lines (heading-italic nesting + a bold-run join, the HTML inline residual); **JSON 1/1 structurally identical** through the HTML item tree |
| ODF (odt/ods/odp) | `odf.rs` | **7/7 exact** on the native files (docling 2.120.3 parity — #320: `<text:a>` hyperlinks render as Markdown links with docling's target classification and boundary-whitespace rule (docling#3949); a `draw:image` whose `xlink:href` is not a package part is never read from disk and yields no picture unless it is an `http(s)` URL fetched under `fetch_images` (docling#4015) — the phantom placeholder text_document_02.odt used to emit is gone) — slide-title/name headings, shape text, speaker-notes drop, chart classification + data tables, merged-cell semantics (plain repeat vs rich dedup), and text after inline elements (docling 2.115's tail fix, #255 — the old run-tail dropping quirk is gone on both sides); sections walked (docling#3852), dangling `draw:object`s skipped (docling#3876) |
| JATS | `jats.rs` (roxmltree) | **5/5 byte-identical to docling-core 2.96's export of docling 2.126's groundtruth** (#391 ports docling#4172, unreleased: a **structured abstract** keeps its `<sec>`s — each a heading one level below the abstract's (`hlevel + 2`, the abstract's being `hlevel + 1`, both with `hlevel` still 0 since the abstract precedes the body walk) with one text item per `<p>`, an untitled section's paragraphs directly under the abstract heading, a sections-only abstract with no plain text item, a section without paragraphs dropped and a nested `<sec>` not descended into — where we flattened every section into one `Title: text` paragraph; the abstract's own heading is its first `title` **or `label`** child (we read `title` only, so a label-only abstract fell back to "Abstract" for us and not for docling); the new `pmc2231364` fixture mirrored with its groundtruth. The nested-list half of docling#4172 does not apply: it wraps a sub-list in a LIST group to silence a docling-core deprecation warning, and our node model carries nesting in the item's level, not in parent refs — Markdown is unchanged there. **Known upstream quirk, replicated:** docling's `_parse_title` joins `elem.text` of each `title-group` child (the text before its first child element), so an `<article-title>` with inline markup is cut at the markup — `pmc2231364`'s title comes out as `# Global transcriptional response of`, as in upstream's groundtruth. We used to emit the full descendant text; `parse_title` now matches docling byte-for-byte (a one-line switch back to `node_text` if upstream fixes it) — no other fixture has markup in its title, which is why it never surfaced; #392 ports docling#4041 (unreleased): under `fetch_images` a `<fig>`'s `<graphic xlink:href>` — direct or inside `<alternatives>`, in document order — is read from the source **file's** directory and embedded (JSON `image`, `--images embedded`; Markdown keeps the placeholder marker), where `add_figure` hardcoded no image and the XML arm was the one image-bearing declarative backend the converter never handed `fetch_images`. docling's contract is kept: an absolute href is refused with a warning but a later relative rendition still counts, a candidate resolving outside the directory aborts the whole figure, an extensionless href is probed with `.jpg .jpeg .png .tif .tiff .gif`, `.svg` and remote hrefs are skipped, an undecodable file warns and falls through, existence is checked before decoding so probe misses do not warn per suffix, and a figure that resolves nothing warns once. An in-memory source or one fetched from a URL embeds nothing (docling's `_load_figure_image` needs a local `base_path`); docling's `source_uri` maps onto `SourceDocument.path`. docling's separate `enable_local_fetch` has no counterpart — our one `fetch_images` flag is the opt-in, as for HTML/AsciiDoc. Upstream's twelve temp-dir cases are unit tests here; (the stored `.md` predates the 2.96 header flattening and uses compact tables); #364: docling#4029 `<ext-link xlink:href>` hyperlinks on inline runs (`[text](url)`, runs coalesce only with equal formatting *and* link, blank hrefs ignored, URLs normalized like pydantic's `AnyUrl`), the new `ptag100.xml` fixture (docling#3726) mirrored, block `<tex-math>` emitted as a formula item so multi-line `$$…$$` bodies keep plain newlines, and a no-break space inside a citation kept verbatim (only ASCII whitespace collapses) |
| USPTO | `uspto.rs` | **1/5 exact (2/5 whitespace-normalized)** on the sources live docling converts — it errors on the other 5 (those are validated byte-exact via `.dclx`), and its APS-text *Markdown* export is empty where ours emits the text dump (the `.dclx` matches exactly — §5) |
| XBRL | `xbrl.rs` | arelle-free core (dei facts → title, `*TextBlock` → HTML); *vs committed groundtruth* 0/2 (30 / 346 diff lines) — live docling needs arelle, which the conformance venv doesn't ship |
| JSON-docling | `docling_json.rs` (serde_json) | #390: a caption's parent survives the round trip — on its picture/table (a PDF conversion), on the item's container ahead of or behind it (a chart, an HTML figure table), or on the body; #403: a picture's `image` (docling's `ImageRef`) comes back with the document — a `data:` URI decoded to the picture's bytes, a *referenced* image (a path or `file:` URL, as `--images referenced` writes them) read from disk relative to the JSON file — so `--images embedded`/`referenced` work on docling-JSON input as they do on the original document, and a JSON→JSON round trip keeps the image; an unreadable reference degrades to the `<!-- image -->` placeholder. Reads docling's native JSON back into the `Node` model ($ref body tree walked through text items' `children` too — docling nests a section's content under its heading, #362 — formatting, list nesting, table grids). Measured by re-serializing docling's *own* documents (`tests/data/pdf/groundtruth/*.json`) and diffing against docling-core 2.96's render of the same file: **14/14 byte-exact** (#384 took it from 479 diff lines to 19, #385 to none — the last 19 were the Markdown serializer's guessed list boundaries). A caption leads its picture and its table and trails its code block (a `CodeItem` is docling's only floating text item); a marker docling-core already considers valid Markdown prints verbatim, so a reference list continuing over a page break keeps its numbering, while any other marker forces a bullet and is kept in front of the text (`- (1) Human Annotation`) and an unmarked item is numbered by its position among the group's children, nested groups included; checkbox items, unclaimed caption items and `$$`-wrapped formulas render, and code and formulas are left unescaped. |
| DocLang (`.dclg`/`.dclx`) | `doclang.rs` (roxmltree) | **15/15 exact** vs live docling reading the same archives back (`tests/data/doclang`); the inverse of the `.dclx` output serializer, incl. docling's round-trip losses (list-item formatting, hyperlink targets) |
| DocTags (`.doctags`/`.dt`) | `docling-core::doctags` | reads the SmolDocling/granite-docling token stream back into a `DoclingDocument` (#152) |
| LaTeX | `latex.rs` (scanner) | simple `.tex` ≈ live (0/2 exact, but within 2 / 9 diff lines); multi-file arXiv projects convert too — 6 projects carry committed groundtruth and snapshot pinning (`tests/snapshots/latex`) |
| MHTML (.mhtml/.mht) | `mhtml.rs` (mail-parser) → HTML backend | docling's `InputFormat.MHTML` (docling#4184, unreleased — it unwraps the archive inside its HTML backend); #386 mirrors its fixture (`tests/data/mhtml/sources/example.mhtml`, a Blink save of example.com; upstream ships no groundtruth for it — its tests assert programmatically — so its expected outputs live in our regression corpus as `example_docling.mhtml`, beside our older `example.mhtml`, a different save of the same page with the same Markdown) and its test matrix: the root is the first `multipart/related` entity, its `start` parameter names the root part by `Content-ID`, a `multipart/alternative` root yields its last HTML alternative, and a bare `text/html` message is its own root; no `Content-Type`, no `multipart/related`, an empty one, a `start` naming nothing or a non-HTML part, or an empty page **fail** the conversion as docling's do (we used to return an empty document, and to take the first `text/html` part anywhere); image resources are the selected scope's `image/*` parts only, keyed by `Content-Location` verbatim and resolved against the archive base, and by the normalized `cid:` (case-insensitive, brackets stripped), first part wins a duplicate key; the base is the root's `Content-Location` — a remote URL kept, `//host` made `https:`, a local path / `file:` URI / Windows drive path (either slash form) remapped under docling's synthetic `thismessage:/` so `<img src>` and `Content-Location` meet without touching the filesystem; archive images (and `data:` URIs) are embedded only under `fetch_images`, as docling's `HTMLBackendOptions.fetch_images` gates them — the default leaves placeholders (we used to embed unconditionally). Not ported: docling's fallback to its shared image loader (files beside the archive, remote fetches) for a reference the archive lacks. Kept beyond docling: `use_web_browser` pre-renders the page (docling rejects `render_page` for MHTML). Markdown on the mirrored fixture is identical |
| RTF (.rtf) | `rtf.rs` (hand-rolled control-word tokenizer, #209) | **native, where docling reads RTF through LibreOffice** (upstream gained that path after this backend); paragraphs + bold/italic/strike runs, stylesheet/outline headings, `\listtext` lists (incl. multilevel numbering; list identity from `\ls`, or a top-level marker-kind flip when a writer omits it — #385), `\trowd` tables with `\cellx`-grid merge recovery, textbox content, HYPERLINK fields, embedded PNG/JPEG pictures, cp1250/1251/1252 + `\uN` unicode. Conformance (`scripts/conformance/rtf_conformance.sh`): 30 of the 31 corpus files in `tests/data/rtf/sources/` are LibreOffice-generated from the DOCX/DOC fixtures and diffed against **our own conversion of the source document** — 4/30 exact, 6/30 whitespace-normalized; the rest is dominated by LibreOffice round-trip artifacts (style bold/italic materialized into runs, equations linearized to text, checkbox form fields), not parser losses. #387 adds the 31st, `legacy_sample`, mirrored from upstream with **docling's own groundtruth**, so it is diffed against that real reference instead: **exact**, tables rendered without width padding to match the stored corpus (`--compact-tables`) |
| SVG (.svg) | `svg.rs` + resvg rasterization (#212) | **docling.rs extension — docling does not accept SVG input**; mirrors the pdf/pdf-text split: ML builds rasterize (resvg, white-backed PNG, ~2048px long side) and ride the image pipeline (layout + OCR + tables); `pdf-text`/wasm builds and `--no-ocr` extract `<text>` elements directly — transform-aware (translate/scale/rotate/matrix) reading order, root `<title>`/`<desc>` as heading/lead paragraph, unrendered subtrees (`defs`, `clipPath`, `display:none`, …) skipped |
| StarOffice / OpenOffice 1.x & flat ODF (.sxw/.stw/.sxg, .sxi/.sti, .sxc/.stc, .fodt/.fods/.fodp) | `odf.rs` (shared ODF parser + local-name mapping layer, #215) | **docling.rs extension — docling reaches these only via LibreOffice**; the OO1.x predecessor schema differs from ODF mostly in namespace URIs, which this parser never matches on — the mapping layer covers the real deltas: `office:body` as the direct content container (dispatched by `office:class`), `ordered-list`/`unordered-list`, `tab-stop`, `text:level` headings, `style:properties` with `text-crossing-out`/`text-underline`. Flat ODF is the same document XML uncompressed in one file: styles ride the content DOM, embedded charts become inline `draw:object` documents, inline `binary-data` images gate on their decoded raster magic (SVM previews stay out). UOF is out of scope |
| dBase / DIF / SYLK (.dbf, .dif, .slk/.sylk) | `interchange.rs` (#216) | **docling.rs extension — docling reads none of them**; native parsers, content-sniffed inside one backend so a misnamed file still converts. DIF and SYLK are sheet snapshots and run through the same flood-fill region splitting as ODS sheets — a `.dif`/`.slk` LibreOffice saves from a sheet converts **byte-identically** to our conversion of the `.ods` itself (verified on the corpus in `tests/data/interchange/`). dBase converts as one table: field names as the header row, deleted records skipped, `D` dates as ISO, `L` logicals as true/false, memo fields (a `.dbt` sidecar) empty, cp1252 high bytes |
| Lotus 1-2-3 / Symphony / MS Works (.wk1–.wk4, .wks, .wrk, .123) | `lotus.rs` (#216) | **docling.rs extension — docling reads none of them**; native record-stream parsers following Gnumeric's lotus-123 importer, content-sniffed on the BOF so a misnamed file still converts (`.wks` is ambiguous: 1-2-3 rel 1A and MS Works v3 both used it — the BOF opcode decides). WK1/WKS cells (INTEGER/NUMBER/LABEL/FORMULA caches + STRING results), WK3/WK4/123 cells (extended floats, SMALLNUM, packed numbers, FORMULASTRING, multi-sheet), Works v3 cells incl. the packed-f32 SMALL_FLOAT. Sheets split into data regions like ODS: a `.wk1` of a sheet's data converts **byte-identically** to the `.slk` of the same sheet (pinned in `tests/data/lotus/`). Read-verified against LibreOffice's Lotus/Works import on the committed corpus (LO itself drops WK1 string-formula results; we keep the cached STRING record, following Gnumeric). Quattro Pro and the rest of the umbrella stay demand-gated |
| Quattro Pro (.wq1, .wq2, .wb1–.wb3, .qpw) | `quattro.rs` (#216) | **docling.rs extension — docling reaches Quattro Pro only via LibreOffice (libwps)**; native parse of all generations after libwps' readers: DOS 1–4/5 record streams (BOF 0x5120/0x5121; cells `[fmt][col u8][sheet][row i16]` / `[col][sheet][row][style]`, pascal-string labels in CP 437, formula caches + 0x33 string results), Windows 1–8 streams (BOF 0x1001/0x1002, `.wb3` = BOF 0x1007 in the OLE `PerfectOffice_MAIN` stream; C-string labels, CP 1252) and QPW 9–X9 (OLE `NativeContent_MAIN` zones: 0x407 string table, 0x601/0xA01 sheet/column, 0xC01 typed cell runs with list/increment packing and libwps' packed 4-byte floats, 0xC02 string results). Formulas contribute their cached value; each sheet runs through the ODS flood-fill region splitting. Corpus: the six CC0 format-corpus samples (wq1, wq2, wb1, wb2, wb3, qpw) |
| MS Works 6–9 spreadsheet (.xlr) | `xls.rs` via extension routing (#216) | **docling.rs extension**; Works 6–9 saved spreadsheets as an Excel 97 BIFF8 `Workbook` stream in an OLE container under the `.xlr` extension, so the XLS reader (calamine) takes them unchanged. Unverified against a real file — no public `.xlr` sample exists (format-corpus, Tika, LibreOffice and govdocs1 have none); a non-BIFF `.xlr` (Works 5 or older) fails with the XLS reader's error |
| AbiWord (.abw, .zabw, .awt) | `abw.rs` (roxmltree; gzip via flate2 for .zabw, #216) | **docling.rs extension — docling reaches AbiWord only via LibreOffice (libabw)**; native AWML parse: Title/heading-N styles map like DOCX, `<c>` runs carry bold/italic/strike (underline and sub/superscript survive into DocLang inline runs), `xlink:href` anchors become links, list paragraphs (listid + list_label marker, label text and tab dropped) with per-listid numbering, attach-grid tables (spans replicate the anchor docling-style), header/footer sections (incl. -even/-first) dropped as furniture, embedded base64 images extracted as pictures. Corpus: AbiWord-CLI conversions of the DOCX fixtures diffed against our own DOCX conversion — unit_test_formatting is byte-identical; the residue elsewhere is AbiWord import artifacts (Word field codes materialized as text, list numbering downgraded to bullets, merged-cell shifts), not parser losses |
| WordPerfect 5.x / 6.x+ (.wpd, .wp, .wp5, .wp6, .wpt) | `wpd.rs` (#216) | **docling.rs extension — docling reaches WordPerfect only via LibreOffice (libwpd)**; native parse of the `ÿWPC` function-code stream, version from the prefix header: WP 5.0/5.1 (hard/soft returns, hard space/hyphens, `C0` extended characters, `C3`/`C4` attribute pairs, fixed- and variable-length functions skipped by size) and WP 6.x+ (default international chars 1–32, single-byte and `0xD0` end-of-line-group codes with libwpd's semantics — soft line ends `0xCD`–`0xCF`/subgroups 1–3 wrap, deletable soft ends at hyphenation points join the word, hard ends break the paragraph, cell/row/table-off marks build the table with the next cell's column span and bound-from-above flag —, `F0` extended characters, `F1` undo regions — deleted text is dropped — `F2`/`F3` attributes; WP 5.x tables via the `0xDC`/`0xDD` table groups that begin cells and rows). Bold/italic/strike reach Markdown, underline and sub/superscript the DocLang inline runs; table cells/rows collect into a table (padded to the widest row); the WP character sets 0–14 map through Tika's tables (Multinational 1 #9, the typographic apostrophe, to U+2019). Not extracted: header/footer and footnote text (6.x prefix packets, 5.x function payloads), styles/outline numbering. Refused with a targeted error: encrypted documents, WP 4.2 and older (no prefix header), WordPerfect for Macintosh 3.x (file type 44), non-document file types. Corpus: Apache Tika's three WordPerfect fixtures (`tika_wp6.wpd`, govdocs1 `tika_wp50.wp`/`tika_wp51.wp`), LibreOffice's libwpd `WP5.wp`/`WP6.wpd` and the Open Preservation format-corpus WP 6.1 sample; a 1.1 MB WP 6.1 DOS thesis converts to 72 paragraphs with every hyphenated line rejoined (175 fragments before the libwpd code tables) — every string Tika's tests assert is present (`AND FURTHER`, `test1-2`, `Surrounded by her family`, `STUDY RESULTS: Existing condition`, `Seattle nonstop flights.`), the deleted `this was deleted.` is not |
| Microsoft Works word processor 2–9 (.wps) | `wps.rs` (#216) | **docling.rs extension — docling reaches Works only via LibreOffice (libwps)**; native parse after libwps' readers of both generations: Works 2.x DOS / 3 / 4 (`WPS4`: 256-byte header, text limits, BTEC PLC → 128-byte FDPC pages with bold/italic/strike/underline/sub-superscript blobs, CP 850 / CP 1252 text with paragraph/line/page codes, footnote definitions cut out of the body and appended; the Windows versions' OLE `MN0` stream) and Works 2000 / 6–9 (`WPS8`: OLE `CONTENTS` with the chained CHNK index, UTF-16 `TEXT` zone typed by the `STRS` PLC — main, notes, frames, header, footer —, `BTEC` → `FDPC` pages of libwps' tagged font records). Header/footer zones dropped as furniture; Works tables not rebuilt (cell text follows the body as paragraphs); spreadsheets/databases and Works for Macintosh refused with a targeted error. Corpus: LibreOffice's five libwps smoke files (Works 2.00A DOS, 3.0, 4.5, 5.0, 6.0) pin detection and the header/index/zone walk; content decoding is pinned by synthetic WPS4/WPS8 streams in the unit tests — no public corpus with real Works prose exists (govdocs1 has none) |
| StarOffice 5 binaries (.sdw, .sda/.sdd, .vor) | `staroffice5.rs` (CFB via the shared `cfb.rs`, record/chunk walk per libstaroffice's reverse engineering, #215) | **docling.rs extension — docling reaches these only via LibreOffice (libstaroffice)**; text-level extraction. StarWriter (`StarWriterDocument` stream, SW3–SW5): record tree (type byte + 24-bit size, flag-byte prologues), `'T'` text nodes as paragraphs in document order — tables flatten to cell texts, inline redline fragments kept. StarDraw/StarImpress (`StarDrawDocument3` stream): pages (`DrPg`) as sections with outliner (`xV4B`) texts, outline depth as list nesting; master pages, the object-less handout and `~LT~Notizen` notes pages dropped (docling drops speaker notes too). `.vor` templates dispatch by the contained stream. Strings decode as cp1252. StarCalc (.sdc) has a different cell-record model and errors with a targeted save-as-.ods message — a follow-up |
| First-class table cells + repair API | `docling-core` `TableCell` (text, bbox, span rectangle, header roles), `Table::cells`, `Table::{cell_at, cell_text, set_cell_text, cell_bbox, set_cell_bbox, find_cell_by_bbox, update_cell_by_bbox}`, `DoclingDocument::tables[_mut]` (#238, #240) | **docling.rs counterpart of Python docling's `TableCell`**: the PDF TableFormer paths emit real per-cell records (page-point bboxes, row/col spans from the OTSL grid, `ched`/`rhed`/`srow` header roles), the JSON export serializes them verbatim (`table_cells` with `bbox`/span offsets; the grid repeats spanning cells like docling's `TableData.grid`) — so the Python/Node bindings see them — and the DocLang structure overlay derives from them (real `<lcel/>`/`<ucel/>`/`<ched/>` tokens for PDF tables). bbox lookup is best-IoU; updating a spanning cell through any covered position updates the record and every covered grid slot. Declarative tables derive their cells from the structure overlay (DOCX/XLSX merges, HTML spans + `th` headers, ODF covered cells, USPTO CALS) — verified against the mirrored Python groundtruth: 43/56 corpus files with identical per-table (cells, spans, headers), and every xlsx/html span fixture exact; the residue is pre-existing table-count/nested-table divergences and Python backends' empty-cell omission quirks (pptx, one word_tables cell), not cell records |
| Visio (.vsdx, .vsdm) | `visio.rs` (OPC zip + XML, same `Package` machinery as DOCX/PPTX, #214) | **docling.rs extension — docling has no Visio reader**; each page a level-1 section, shape text in reading order (top-to-bottom/left-to-right, group children through the parent coordinate system, master default-text inheritance), connectors resolved via `<Connects>` into a From/To(/Label) relations table; background pages skipped. Legacy binary .vsd and 2003-XML .vdx are follow-ups |
| DjVu (.djvu, .djv) | `djvu.rs` + the `djvu-rs` crate (#434) | **docling.rs extension — docling has no DjVu reader**; pure-Rust decode (MIT, no GPL deps, no DjVuLibre binary), so it runs in every build incl. wasm. Mirrors the pdf/pdf-text split with the **text layer as the default**: the hidden per-page OCR zone tree becomes blank-line-delimited paragraphs (physical scan lines reflowed into prose), `PageBreak` between pages, `--pages A-B` honored; every page opens with a `PageInfo` marker (size in points from the INFO chunk's pixels ÷ dpi) and each paragraph carries the box of its text-layer zones (word → line → paragraph zones, whichever level aligns with the text by count), so the JSON has docling's `pages` map and per-item `prov` like a PDF — what docling-core's `pages` filter and `export_to_markdown(page_break_placeholder=…)` key on; a scan-only DjVu (no text layer on any selected page) rasterizes through `djvu-rs` (fitted to `DOCLING_RS_DJVU_RENDER_PX`, 2500 px) and rides the image ML pipeline, or degrades to an empty document with a warning without ML. Three own fixtures (3/11/84 pages) pinned in the regression suite |
| Apple iWork (.pages, .numbers, .key) | `iwork.rs` (zip + Snappy-framed protobuf IWA, generic wire walk; field/type numbers per numbers-parser / keynote-parser reverse engineering and docling's Pages reader, #213, #318) + `pages.rs`/`pages_iwa.rs`/`pages_xml.rs` (the Pages content model and its two readers, mirroring upstream's `docling/backend/iwork/` package, #383) | **Pages: 4/4 exact** Markdown on upstream's committed `tests/data/pages/` groundtruth (docling#4062, mirrored with its `.json`; #383) — the full `IWorkPagesDocumentBackend` feature set in both container generations: tables **placed in the text flow** at their U+FFFC anchor (not appended), floating **text boxes** and groups after the body, **character formatting** (bold/italic/underline/strikethrough, super/subscript) with the spaces around a run kept and the paragraph trimmed once at its ends, **hyperlinks**, **lists** (bullet/numbered from `TSWP.ListStyle` label types and the list-depth run table; iWork '09 `sf:liststyle`), **inline images** (`TSD.ImageArchive` → `Data/` member, `sf:media`), **headers/footers/footnotes** on the furniture layer, **reviewer comments** on the notes layer with `[author]: text` and `comments` back-refs on the annotated item (docling-core's bare `add_comment` shape — no `comment_section` group), Pages 5.2+ table cell storage (v4/v5 cell layouts, rich-text cells through `TST_TEXT_REF`). Run tables follow upstream exactly: a paragraph-style entry without a reference clears the style, so a template paragraph whose style Pages left unreferenced is body text, and an anonymous (unnamed) paragraph style is body text. iWork '09 `index.xml(.gz)`: `sf:p` runs walked node by node (`sf:span` character styles, `sf:link`), `sf:ghost-text(-ref)` placeholders skipped (#366, docling#4170), `sf:tabular-model` grids, `sf:header/footer/footnotes` furniture, `sf:annotation` comments anchored via `sf:annotation-field`. JSON: body order, body texts, tables, notes-layer comments and their back-refs identical to upstream's groundtruth; **two shared-exporter caveats** — furniture-layer items (page header/footer, footnote) stay out of the JSON as for every backend, and text items carry no `formatting`/`hyperlink` fields (the formatting reaches Markdown and DocLang). A password-protected package fails with docling's message (Pages hides encryption behind an undefined compression method). Mixed-run paragraphs render as docling-core does: each run wrapped as-is and the runs joined with one space (so `Plain ` + `bold` reads `Plain  **bold**`). **Numbers and Keynote remain docling.rs extensions** (upstream has no reader): Keynote slide text (master placeholders skipped, package order), Numbers sheets/tables as headings with shared-string cell text as a list; nested-dir and zipped-package layouts unwrap (upstream rejects them); numeric cells and full grid reconstruction for Numbers/Keynote tables are follow-ups |
| HEIC/HEIF (.heic, .heif) | libheif via `docling-pdf/heif` (opt-in cargo feature, #211) | **docling.rs extension** (docling reads HEIC only where Pillow can); content-sniffed (`ftyp` brands — misnamed `.jpg` iPhone photos still route correctly), decoded to RGB and fed to the standard image ML pipeline; without the feature the error says `rebuild with --features heif` instead of a generic decode failure. Native dependency, so wasm/default builds stay pure Rust |

Shared OOXML infrastructure (`ooxml.rs`): a `zip` reader, `.rels` parsing, part
content-type resolution, and image extraction — reused by DOCX/PPTX/XLSX/EPUB.

### ML formats — `docling-pdf`

These run docling's *discriminative* PDF pipeline ported to ONNX. They are now
measured **byte-for-byte against live docling** (the committed PDF groundtruth is
regenerated from it): **6 / 14 exact (7 / 14 whitespace-normalized)**, the rest
close — see `PDF_CONFORMANCE.md`. A deterministic snapshot baseline
(`scripts/conformance/pdf_conformance.sh`) still guards against regressions.

| Format | How |
|---|---|
| PDF | **pure-Rust text parser** (`textparse.rs`, font-advance glyph boxes) + pdfium page render → RT-DETR layout (ONNX) → **TableFormer** table structure (ONNX) → PP-OCRv3 OCR for scanned pages → **docling-parse line sanitizer** (`dp_lines.rs`) + reading-order assembly (#419: layout boxes are refitted to the cells they claim, empty ones dropped and contained orphans folded in before the reading order runs, as docling's `LayoutPostprocessor` does — a model box that cut a line in half no longer strands that line after its paragraph; 2206 82→52, 2305 20→18, normal_4pages 20→16 diff lines vs groundtruth). `--pages A-B` (docling's `page_range`, #80) converts a 1-based page window, skipping the rest before rasterization; `--images referenced` streams each page's image files to the artifacts dir as the page is emitted (memory-bounded, #80); `--ocr-lang en|ch` picks the OCR recognition model (en default — the ch_ conformance model glues Latin words); scanned pages with `/Rotate` are un-rotated to upright before layout/OCR and their geometry mapped back to display coords (all four orientations of `tests/data/scanned/` OCR to the same groundtruth text); table captions attach by reading-order adjacency (docling's `_find_to_captions`, #265) and ride on the table across all exports — Markdown above the grid, JSON `TableItem.captions` refs, DocLang `<caption>` |
| Images (tiff/webp/png/jpeg/gif/bmp) | the same pipeline, image as a single page |
| METS / Google Books | `.tar.gz` of per-page hOCR + TIFF → cells from hOCR → the same layout+assembly path (no OCR needed) |
| Audio (wav/mp3/flac/ogg/aac/m4a) and video audio tracks (mp4/mov/mkv/webm — docling's `InputFormat.VIDEO`, Phase 1 of #138) | `docling-asr`: **symphonia** decode (no ffmpeg) → 16 kHz mono → ported log-mel front-end → **Whisper tiny** encoder/decoder (ONNX, greedy with OpenAI's timestamp rules — docling's ASR defaults) → `[time: start-end] text` paragraphs. Frames (Phase 2 of #138): when the `ffmpeg` binary is present at runtime, up to `--video-frames N` (default 8) scene-change frames (evenly spaced fallback) interleave with the transcript as `[time: <ts>]`-captioned pictures with embedded PNGs; no ffmpeg → transcript only, no audio track → frames only. Codecs symphonia can't decode in-process — Ogg Opus, AVI containers — go through the same optional ffmpeg binary when present (#190); without it they fail with a targeted install hint. Transcription language: auto-detected per file from the first 30-second window (Whisper's `language=None` / docling 2.116 default, #180); pin with `asr_lang` (all surfaces) or `DOCLING_RS_ASR_LANG`; English-only presets skip detection. |

### DocLang (`.dclx`) coverage

The `.dclx` DocLang output (§3) is scored against docling's own `.dclx` archives
with `scripts/conformance/dclx_conformance.sh` — the extracted `document.xml`
line-diffed, similarity `= 100·(1 − difflines / max_lines)`. **≈97% mean over the
136-fixture non-PDF corpus** (issue #32 target: ≥90%), per source format.
HTML rich table cells (#328) carry their block content — lists, ordered lists,
nested tables, headings, `<pre>` runs — as `Table::cell_blocks`, so the
DocLang `<fcel/>` bodies match upstream's `RichTableCell` serialization
(`table_03`–`table_06`, `table_with_heading_02`,
`html_inline_group_in_table_cell`, `html_rich_table_cells` and
`hyperlink_05` byte-exact — a picture caption also carries its hyperlink
annotation: an `<a href>` wrapping the image, or the first link inside a
`<figcaption>`, emits the block-form `<caption>` with an `<href uri=…/>`
head and docling's `hyperlink` field on the JSON caption item).
Robustness tracks docling-core 2.88/2.89 (#253): XML-illegal control
characters serialize as visible `[U+XXXX]` markers, a literal `]]>` splits
across CDATA sections, and deep section headers clamp to heading level 6
instead of emitting out-of-range tokens — all round-trip pinned; the reader
side of docling-core#689/#695 (tag-shaped literals in OTSL cells) never
applied here, since the single-pass roxmltree reader doesn't re-parse text
fragments as XML.

| Format | `.dclx` similarity | Format | `.dclx` similarity |
|---|---|---|---|
| CSV / AsciiDoc / Email | **100%** | JATS | 95% |
| XLSX | **100%** | Markdown | 92% |
| DOCX / PPTX | **100%** | LaTeX | 91% |
| USPTO | 98% | HTML | 97% |
| ODF | 96% | WebVTT | 81% |

This effort was tracked as
[issue #32](https://github.com/docling-project/docling.rs/issues/32) — **closed,
both targets met** (non-PDF ≥90%: 94%; PDF ≥50%: 63% at ±2). Its children
(#38–#41, #44, all closed) landed the ODF, USPTO legacy-entity, elife XML,
wiki_duck and APS-plain-text work — `pftaps` is byte-exact (§5). The PDF path
emits full layout `<location>` provenance (text, headings, tables, pictures,
list items, code, and page-header/footer furniture), scored against a
16-fixture DocLang groundtruth with a ±2-grid-unit geometry tolerance —
**63% mean** (§3, `PDF_CONFORMANCE.md`); the residual is model-level
(TableFormer OTSL structure, layout classification — the closed-as-model-level
blockers of `PDF_CONFORMANCE.md`), not serialization.

The same geometry also reaches the **JSON export** (#171): PDF conversions
populate docling's `pages` map (`{"1": {"size": {...}, "page_no": 1}}`) and
per-item `prov` (`page_no` + BOTTOMLEFT-origin bbox in points + `charspan`),
so `DoclingDocument.load_from_json(...)` in Python docling-core gets working
bounding-box highlighting, page attribution and coordinate filtering — from
the CLI, serve, and the Python/Node bindings alike. One caveat vs Python
docling: coordinates round-trip through the DocLang 0–511 grid, so they carry
a quantization of up to ~page-size/512 (≈1.6 pt on A4); `charspan` always
starts at 0 (docling.rs does not track sub-item spans).

Where a picture's or table's **caption item hangs** in the JSON tree follows
the backend, as it does upstream (#390, `docling_core::CaptionParent`): the
PDF pipeline parents a layout caption to the picture or table itself (the
caption is the item's first child, as docling's reading-order model attaches
it); every declarative backend — HTML, JATS, LaTeX, Markdown, EPUB, AsciiDoc,
DeepSeek — leaves docling's `add_text` default, so the caption is a `#/body`
child even when the item sits in a group or under a heading (upstream's
groundtruth: 103 body-parented captions against 36 on pictures and 18 on
tables, all of the latter PDF); the office backends hang a chart's title
caption off the chart's container — the sheet group, the slide `chapter`
group, docx's current parent (docling#4190) — listed ahead of the picture
that references it; and an HTML `<figure>`-wrapped table's figcaption sits
under the table's parent *behind* the table (docling#4050). Until #390 every
caption hung off its own picture or table. A docling-JSON input keeps the
parent it came with. Markdown, DocLang and LaTeX are unaffected — they place
a caption by its item, whatever its parent.

---

### Chunking conformance

docling-core's **HierarchicalChunker** and **HybridChunker** (the RAG chunk
generators) are ported as `docling::chunker` and scored against live docling
running the same chunkers on the same 83-document corpus
(`scripts/conformance/gen_chunks.py` generates the groundtruth,
`scripts/conformance/chunks_conformance.sh` compares the records' text +
headings + contextualization — the payload an embedding model sees):

| Chunker | Identical chunk records | Fully-exact documents |
|---|---|---|
| Hierarchical | **555 / 562 (98.8%)** | 79 / 83 |
| Hybrid (MiniLM tokenizer, 256 tokens) | **300 / 312 (96.2%)** | 76 / 83 |

The port reproduces docling's semantics end-to-end: heading-path metadata with
level shadowing, triplet table serialization over `export_to_dataframe`
semantics (multi-row headers joined with `.`, span-aware header detection,
single-column/flatten fallbacks), rich-cell re-serialization, `semchunk`'s
recursive splitter-hierarchy algorithm, the line-based table splitter (down to
the `\n` it prepends to carried-over segments and the `max_tokens` argument
docling's pydantic model silently drops), and peer merging. Token counts are
byte-compatible with `transformers` (HF `tokenizers` with MiniLM's fixed-length
padding disabled).

On the large-document benchmark (`wiki_duck.html`, 89 hierarchical / 115 hybrid
groundtruth chunks) **100% / 100% of docling's chunk records are reproduced
identically** (order-aligned) — the former HTML-backend model gaps (rich table
cells with inline markup and span de-duplication, figure-caption run spacing,
indicator images, `<br>` annotation-boundary handling) are closed. Corpus-wide:
hierarchical 98.8%, hybrid 96.2% record-identical. The chunker-era
work (checkbox inputs, fragmented-anchor folding, `<button>` blocks) plus the
#81 parity fixes also lifted the HTML `.dclx` similarity: 88% mean (was 84%).

## 3. Output formats

| Output | API / CLI | Notes |
|---|---|---|
| **Markdown (legacy)** | `export_to_markdown()` / default | byte-for-byte docling, quirks included. A blank line separates two *sibling* lists only where the backend flagged the second (`first_in_list`) — never inferred from a kind flip or a number gap (#385). Tracks docling-core 2.96's serializer: a referenced image's link destination is percent-encoded like upstream's `_escape_uri_path` (docling-core#698 — spaces and parentheses in the artifacts dir, `\\` → `/`, absolute Windows and UNC paths as `file://` URLs, URLs component-encoded, never double-encoded; the artifact *file path* handed back stays raw); a table's Markdown header is the leading block of rows on which a `column_header` cell *starts*, stacked header rows flattened per column with ` - ` (`% of Total - Train`), flags that begin below row 0 promoting nothing and unflagged tables keeping row 0 (docling-core#723/#756 — alignment and widths come from the body rows). The pivot-table deviation this port carried is gone: docling#4216 flags a spanning `<th>` row as `row_header` rather than `column_header`, so upstream no longer folds a pivot table's first data row into the header (`Year - 2025 | Month - January`) and `header_row_count` is now docling-core's rule verbatim (docling-core#765 resolved on the backend side); a single newline inside an item's text is a GFM hard line break (`"  \n"`, docling-core#721 — a blank line stays a paragraph break, a heading collapses its newline to a space), a heading inside a rich table cell renders as plain text (docling-core#540), and field regions / field items emit nothing of their own — no more `<!-- missing-text -->` markers (docling-core#724). The hard-break marker is Markdown-only: JSON cell text and LaTeX cells see the raw newlines |
| **Markdown (strict)** | `.strict(true)` / `--strict` | Rust-only cleaner mode — **no docling equivalent** |
| **JSON** | `export_to_json()` / `--to json` | docling-core native wire format (schema 1.10.0) |
| **DocLang (`.dclx`)** | `export_to_doclang()` · `docling::dclx::save_as_dclx()` / `--to dclx` | DocLang 0.7 XML (`<doclang>`), and the OPC archive docling 2.110's `save_as_doclang()` writes |
| **LaTeX (`.tex`)** | `export_to_latex()` / `--to latex` (#317) | docling 2.124's `LaTeXDocSerializer` with default params, scored against upstream's own `docling --to latex`: **93/116 shared declarative fixtures byte-exact** (98 modulo upstream's duplicated formatted list items / headings — #328's HTML `cell_blocks` made the rich-cell bucket exact: in-cell lists render as `\begin{itemize}`, nested tables as nested `tabular`s, in-cell headings as plain text); remaining gaps are underline / sub / superscript (no Markdown form) and a few list groupings. Serve `to=latex`, Node `to: 'latex'`; Python bindings use upstream docling-core's serializer on the reconstructed document. Deviations: headings deeper than `\subsubsection` degrade to `\paragraph`/`\subparagraph` where upstream raises; upstream's duplicated text for formatted items ([docling-core#740](https://github.com/docling-project/docling-core/issues/740)) is not reproduced; docling-core#743 (2.95) fixed the inline-group double serialization (docling-core#740) that this port never reproduced, so the two agree again |
| **Page breaks in Markdown** | `DocumentConverter::page_break_placeholder(Some(text))` / `--page-break-placeholder TEXT` / serve `md_page_break_placeholder` / Node `pageBreakPlaceholder` | docling-core's `MarkdownParams.page_break_placeholder` (`export_to_markdown(page_break_placeholder=…)`, docling-serve's `md_page_break_placeholder`). Upstream yields a `_PageBreakNode` between two body items whose `prov.page_no` differ (a group counts by its first item) and substitutes the placeholder as a part of its own, joined by the `\n\n` delimiter; docling.rs marks a break between two rendered blocks separated by a page boundary (`PageInfo` per PDF page / sheet, `PageBreak` between slides, DjVu and DocTags pages) — the same result: never first or last, empty or furniture-only pages collapse into one break, an empty placeholder leaves the doubled blank line upstream leaves. `None` (default) = no breaks, docling's default. Buffered and streamed (`MarkdownStreamer::with_page_break_placeholder`) output byte-identical |
| **Image extraction** | `export_to_markdown_with_images(mode, dir)` / `--images` | `placeholder` (default) · `embedded` (base64 data URI) · `referenced` (writes PNG files) |

- **DocLang** reproduces docling-core's `DocLangDocSerializer` (`minidom.toprettyxml`
  layout) directly: headings, rich inline runs (`<bold>`/`<italic>`/`<underline>`/
  `<strikethrough>`/`<sub|superscript>`), lists with enumeration `<marker>`s, OTSL
  tables (`<ched>`/`<fcel>`/`<lcel>`…) with per-cell `<location>`, code, formulas,
  pictures and furniture. Conformance is scored against docling's own `.dclx`
  archives (`scripts/conformance/dclx_conformance.sh`): **≈97% mean similarity over
  the 136-fixture non-PDF corpus** (issue #32's ≥90% target) — every OOXML fixture
  (docx/pptx/xlsx) plus csv/asciidoc/email byte-exact, uspto/jats in the
  mid-to-high 90s, html (#328 rich cells + caption hyperlinks) 97%, md/odf/latex low 90s, webvtt in the 80s (full table
  in §2). `wiki_duck` — the one HTML fixture still short of exact — is at 88%:
  furniture layer tokens, `<rtl>` direction markers, parenthesized link
  destinations and `aria-hidden` chrome are now upstream's; what remains is
  docling's `_list_item_has_segment_siblings` wrap rule (a list item takes a
  `<text>` wrapper only when it *owns* a nested list or picture — even an empty
  `<ul>` counts — where we wrap whenever any deeper item follows in the same
  list), plus `<sup>` reference markers and the footer's inline groups. The format-by-format work was
  tracked as [issue #32](https://github.com/docling-project/docling.rs/issues/32) and its
  children (#38–#41, #44) — all closed, targets met. This is an **output** format;
  a DocLang *input* backend is still out of scope (§5). For **PDF**, where the
  reference `<location>` geometry comes from docling's own layout run, the metric
  is scored with a ±2-grid-unit geometry tolerance (text/structure still
  byte-exact): **52% exact · 63% at ±2** (against the ≥50% target); the remaining
  gap is model-level (TableFormer/layout/reading order), not serialization — see
  [`PDF_CONFORMANCE.md`](./PDF_CONFORMANCE.md).

- **JSON** rebuilds docling's full `body`-tree-of-`$ref`s model from the `Node`
  tree (texts/groups/tables/pictures, labels, list grouping, table grids,
  formula/code items, picture `ImageRef`s). It loads back into Python
  docling-core and **~91% round-tripped** byte-identically to the direct
  Markdown when measured at the JSON-export milestone. A backend whose
  upstream tree the flat nodes cannot express hands the export a
  `docling_core::tree::ItemTree` instead (`DoclingDocument::tree`, built by
  the HTML backend's `html_tree.rs` and the DOCX backend's `docx_tree.rs`):
  docling's items in creation order with their parents, children, layers,
  formatting, hyperlinks and comment back-refs, serialized as they are —
  which is how the HTML and DOCX JSON are structurally identical to
  upstream's (see those rows).
- **Image extraction** is wired for PDF/image (figure-region crops) and DOCX/PPTX
  (embedded blobs) by default, and — opt-in via
  `DocumentConverter::fetch_images` (`--fetch-images`) — for HTML/EPUB `<img src>`:
  `data:` URIs, local files (relative to the source), remote `http(s)` URLs, and
  EPUB archive entries. Off by default, matching docling's `enable_*_fetch=False`.
  JSON always embeds extracted images as data URIs.

---

## 4. Differences from upstream docling

These are deliberate or unavoidable divergences, not bugs.

1. **Simplified document model.** `docling.rs`'s `Node` enum
   (`Heading`/`Paragraph`/`ListItem`/`Code`/`Table`/`Picture`/`Group`) is flatter
   than docling-core's `DocItem` graph. JSON export *reconstructs* the full
   `$ref` wire format from it; JSON input maps the other way.

2. **Inline formatting is baked into text.** Bold/italic/links/inline-math are
   stored as Markdown markers inside the text string, where docling keeps
   structured `formatting`/`hyperlink` fields. Consequence: for those spans the
   exported JSON carries the *rendered* text rather than structured fields, and
   ~9% of JSON→Markdown round-trips differ (URLs/`&`/`_` re-escaped by docling).

3. **`strict` Markdown mode is Rust-only.** Default output reproduces docling's
   legacy quirks (`***x*** .` run-spacing, dropped code-fence languages, `\_` and
   entity re-escaping); `strict` produces cleaner Markdown. docling has no such
   switch. All conformance numbers are measured in **legacy** mode.

4. **Tables use docling-core's padded GitHub format.** All backends emit the
   width-padded `tabulate(tablefmt="github")` tables that current published
   docling produces (columns padded to header-width+2 or the widest data cell,
   numeric columns right-aligned). The PDF groundtruth was regenerated from live
   docling to match. (An earlier compact `| - |` variant — to match a stale
   committed corpus — was reverted; the `compact_tables` option still exists but
   no backend sets it.)

5. **The PDF pipeline is discriminative and byte-measured.** Ported from
   docling's standard pipeline:
   - **Layout** — RT-DETR (`docling-layout-heron`) exported to ONNX, run via
     `ort`. Same model family as docling.
   - **Hostile-input limits** — Markdown block nesting stops at 100 levels
     (markdown-it's `maxNesting`, docling's own Markdown parser), deeper
     quotes/lists are skipped; docling-JSON `children` references are walked
     once each and at most 256 deep (a cycle converts like its acyclic twin);
     XML inputs and OOXML/ODF parts nested past `DOCLING_RS_MAX_XML_DEPTH`
     (512) are rejected; a spreadsheet sheet whose used area exceeds
     `DOCLING_RS_SHEET_MAX_CELLS` (10M) is skipped; HTML past
     `DOCLING_RS_MAX_HTML_DEPTH` is emitted as text without a DOM; a PDF page
     rasterizing past `DOCLING_RS_MAX_RENDER_PIXELS` (15000 px/side) is rejected
     before the multi-GB bitmap is allocated.
   - **OCR** — PP-OCRv3 recognition (RapidOCR) via ONNX, *not* docling's default
     EasyOCR; different recognizer → different scanned text. Recognition runs
     on the lines inside layout regions; RapidOCR's PP-OCRv6 DB **text
     detector** (#429, the model docling's RapidOCR default runs) then sweeps
     the whole bitmap, and detected lines no recognized cell covers are
     recognized and placed as orphan text — docling's behavior for text its
     layout model gives no cluster (diagram labels, stamps, margin notes).
     Lines inside a kept picture/table stay silent children, as upstream. The
     detector input is capped at 960 px on the longer side by default
     (PaddleOCR's own default; RapidOCR's uncapped rule via
     `DOCLING_RS_OCR_DET_MAX_SIDE=0`) — ~⅓ of the detection time for
     noise-level output differences. The browser pipeline runs the same
     detector through onnxruntime-web when `ocr_det.onnx` is available.
   - **Tables** — **TableFormer** (image encoder + autoregressive OTSL structure
     decoder + cell-bbox decoder, ported to ONNX), on a cv2-exact preprocessed
     crop. Reproduces docling's padded GitHub tables — `2305-pg9` is cell-for-cell
     exact; multi-row headers / spans on the dense papers still differ.
   - **Text** — a **pure-Rust PDF text parser** (`textparse.rs`, on `lopdf`)
     reconstructs glyph boxes from font advance widths + the text/graphics matrices
     (matching docling-parse's geometry, not pdfium's rendered boxes); handles
     Type0/CID + simple fonts, ToUnicode/encodings, Form XObject recursion, a
     glyph-name fallback, and overprint dedup; glyphs are placed in pdfium's
     page frame — the CropBox ∩ MediaBox box with its lower-left corner as
     the origin, inherited through the page tree — so a trimmed or offset
     page lines up with the rendered bitmap and docling's `prov` boxes. It is
     the default text layer (`DOCLING_PDFIUM_TEXT=1` falls back to pdfium). Its cells feed a port of
     docling-parse's line sanitizer (`dp_lines.rs`): 3-pass corner-distance
     contraction with gap-proportional space insertion, `enforce_same_font`,
     ligature recomposition, loose-box geometry. Plus docling's markdown escaping,
     docling-parse's typographic-punctuation table (every curly quote → `'`),
     wrap dehyphenation, paragraph-continuation merging, docling's rule-based
     reading-order predictor (2.127's same-row links included, #424) with
     cluster cells joined in docling-parse index order, and false-picture /
     page-number layout fixes. The parser is now the **sole** text
     source — pdfium does only page rasterisation + link annotations. Its per-word
     cells reproduce docling-parse's `word_cells` byte-for-byte (377/377 on
     `2305-pg9`), which is what TableFormer matches against; a char-frequency
     validator (`scripts/test/parser_completeness.py`) confirms nothing is silently
     dropped (Form-XObject text and glyph-name-only fonts were the two classes it
     surfaced and fixed).
   - Output is measured **byte-for-byte against live docling** (PDF_CONFORMANCE.md):
     **6 / 14 exact, 7 / 14 whitespace-normalized**, the rest close. The remaining
     gaps are model-level (TableFormer structure on complex tables, layout
     classification, title-page reading order) plus `amt`'s fraction spacing — a
     docling quirk from its embedded-font OS/2 metrics that our single-spaced output
     renders more faithfully; matching it exactly needs a font-metrics layer that
     entangles with the RTL box geometry. The full per-fixture breakdown and the
     model-level blockers live in `PDF_CONFORMANCE.md`.

6. **Extracted image bytes are real but not byte-identical.** Cropped/embedded
   pixels are correct, but the PNG re-encoding differs from docling's, so the
   base64 in `embedded` mode / JSON `ImageRef`s won't match byte-for-byte.

7. **XML format detection sniffs content.** JATS, USPTO and XBRL all use `.xml`;
   the converter routes by content markers (`us-patent` → USPTO, `us-gaap`/`dei`
   → XBRL, else JATS) rather than the extension alone.

8. **Headless-browser pass is opt-in.** Form key-value regions, inline
   visibility, and nested-table cell flattening (docling's exact spacing) are
   all handled statically by default — no browser. Only stylesheet-driven
   (CSS-cascade) visibility suppression needs a rendered page, available behind
   the optional `web-browser` feature / `--use-web-browser` flag (Rust-driven
   Chromium) — see §5.

9. **Confidence pages are keyed 1-based.** The PDF/image pipeline attaches a
   docling-`ConfidenceReport`-shaped report (same grade thresholds, same
   nanmean/nanquantile aggregation; `table_score` unset like upstream) that
   docling-serve surfaces per response (v1.25 parity). Its `pages` map is
   keyed by the **real 1-based page number** — consistent with the JSON
   export's `pages` map and `--pages` windows — where Python docling keys by
   its 0-based internal page index. Unset scores serialize as `null`, not
   `NaN`.

10. **Page rasterization over HTTP** (#243). `to=images` on docling-serve's
    `/v1/convert` (sync, async, batch) renders a PDF's pages to PNG through
    pdfium without running any conversion — the per-page base64 JSON covers
    the PDF-to-image use case Python docling-serve served; `pages=A-B`
    windows and `scale` (pixels per PDF point, default 2.0 = 144 dpi) apply,
    capped at `DOCLING_RS_MAX_RASTER_PAGES` (100) pages per request. The CLI
    counterpart is `--to images` + `--scale`, writing `<stem>_page_NNNN.png`
    files (no cap — the pages land on the caller's own disk).

11. **`do_ocr` and `do_table_structure` are independent** (#244), as in
    docling: `skip_ocr` (`--skip-ocr`, serve/Node `skip_ocr`, Python
    `do_ocr=False`) keeps layout detection and TableFormer but never loads or
    runs OCR — pixel-only text comes back empty instead of erroring. The
    Python binding's `do_ocr=False` previously skipped the whole ML stack
    (layout and tables included), which docling's `do_ocr` never meant; that
    fast path is now the docling.rs-only `text_layer_only=True` kwarg
    (`--no-ocr` / `no_ocr` elsewhere). A missing OCR model also degrades to
    the `skip_ocr` behavior with a one-time warning — docling errors there —
    matching the repo-wide degradation-over-failure convention.

12. **Sparse spreadsheets can skip empty cells** (#271, docling.rs-only
    options, both off by default): `skip_empty_cells` omits empty positions
    from each XLSX/XLS table row instead of materialising every region's
    full bounding box (docling pads the box too; the related upstream #3328
    tracks the RAM cost of its one-TableCell-per-cell materialisation on
    large sheets), and `compact_tables` renders Markdown tables unpadded for
    every format. Default output stays byte-for-byte docling.

13. **`OcrMode` and `OcrOptions.scale`** (docling 2.116/2.117, #254) are
    mirrored on every surface as `ocr_mode` (`--ocr-mode`,
    `DOCLING_RS_OCR_MODE`) and `ocr_scale` (`--ocr-scale`,
    `DOCLING_RS_OCR_SCALE`). `pdf_aware_layout_regions` — upstream's new
    default, OCR gated by layout regions and the PDF text layer — is the
    architecture this port always had, so the default behavior is already
    aligned; `full_page` and `layout_regions` both map onto the
    `force_full_page_ocr` machinery (the whole-page vs per-region *detector*
    distinction has no analogue in the det-free PP-OCR recognizer here).
    `ocr_scale` resamples the OCR input from the pipeline's 2.0 px/pt page
    render instead of re-rendering (docling renders natively at
    `72 × scale` dpi, default 3); unset keeps the pinned 144 dpi baseline,
    so conformance snapshots never move. **OCR language spellings** (#388,
    docling#4075's BCP-47 canonicalization): upstream reads a bare value as
    the engine's native code and an `iso:`-prefixed one as a BCP-47 tag
    reduced to language + script (region dropped), then maps it per engine —
    RapidOCR's `en` and `zh-Hans` → `ch`. This engine ships exactly those two
    PP-OCRv3 recognizers, so `ocr_lang` on every surface (`--ocr-lang`,
    `DOCLING_RS_OCR_LANG`, serve, Python, Node, RAG's `RAG_OCR_LANG`) accepts
    `en`/`ch` plus any English or Chinese tag with or without the prefix
    (`en-US`, `eng`, `english`, `zh`, `zh-Hans`, `zh-TW`, `zho`, `chinese`,
    EasyOCR's `ch_sim`); script/region subtags are ignored, a traditional-
    script request gets the multilingual `ch` recognizer (upstream would pick
    RapidOCR's separate `chinese_cht`, not shipped here), and any other
    language is rejected (warn-and-default only for the env var).

14. **Per-request chunking configuration** (docling 2.117–2.119
    service-datamodel, #256): `to=chunks` accepts `chunker`
    (`hierarchical`|`hybrid`, docling's `ChunkerType`; unset returns both),
    `chunk_tokenizer` (a server-local *relative* `tokenizer.json` path —
    absolute paths and `..` are rejected, unlike upstream's
    fetch-any-HF-model semantics), `chunk_max_tokens` and
    `chunk_merge_peers` on serve, with matching `--chunker`/`--chunk-*` CLI
    flags; the `DOCLING_CHUNK_*` env knobs remain the operator defaults. An
    explicit `chunker=hybrid` without a usable tokenizer fails loudly (400)
    instead of the legacy silent skip. Out of the same sync window but *not*
    ported: upstream's `do_pdf_heading_hierarchy` (`HeadingHierarchyModel`
    — PDF section-header levels from bookmarks/numbering/font style; our
    PDF path emits docling's pre-2.117 flat `##` levels, so the option has
    nothing to switch yet) and `dclx` in the `ArtifactRef` union (only
    meaningful once the sources/targets wire shape lands — #139).

15. **Cloud source/target passthrough** (#139, upstream docling#3795 /
    docling-jobkit): the serve JSON body accepts docling's
    service-datamodel `sources`/`target` shape — `kind`-tagged `file`
    (base64) and `http` (URL + headers) sources always, and `s3` /
    `azure_blob` / `google_cloud_storage` sources *and* targets behind the
    opt-in `cloud` cargo feature (a feature-gated `object_store`
    dependency, so the default build keeps its single-HTTP-stack graph).
    Coordinate field names mirror upstream's `*Coordinates` models;
    a cloud target uploads each converted output as `<stem>.<ext>` under
    the prefix and answers with upstream's `RemoteTargetResult` kind plus
    per-item outcomes. All outbound kinds sit behind `--allow-url-fetch`.
    The jobkit `zip` (one archive of the rendered outputs in the response
    body — a batch download) and `put` (HTTP PUT each output to a
    caller-minted, typically pre-signed, URL — behind `--allow-url-fetch` +
    the SSRF check) targets are covered too (#303), with no
    `object_store`/`cloud`-feature involvement. Not covered, as
    upstream-jobkit-specific or OAuth-bound: `google_drive` and
    `presigned_url` (and with the latter, the `ArtifactRef` union — still
    parked; `put` with a caller-minted URL covers the pre-signed use case).
    Without the feature the cloud kinds parse and answer a clear
    rebuild-with-`--features cloud` error.

16. **Serve observability** (#297, Python docling-serve's OTel
    integration): the same *posture* — Prometheus-style metrics on by
    default, OTLP traces opt-in, `/metrics`+`/health`+`/ready` excluded
    from request telemetry, `OTEL_SERVICE_NAME` defaulting to
    `docling-serve` — but not the same mechanism. Requests log through
    `tracing` (`RUST_LOG`, default `info`); `GET /metrics` serves
    hand-rolled dependency-free counters (requests by status class,
    in-flight gauge, latency histogram with buckets stretched to
    minutes-long ML conversions, conversions by outcome) instead of
    Python's OTel-SDK `PrometheusMetricReader`, so metric *names* differ
    from a Python deployment's. OTLP/gRPC span export sits behind the
    opt-in `otel` cargo feature and activates only when
    `OTEL_EXPORTER_OTLP_ENDPOINT` is set. Not covered: OTLP *metric* and
    *log* export (Prometheus scrape and stderr logs stand in), and
    Python's `OTEL_*` sampler knobs.

17. **PDF heading levels** (#302, docling's `HeadingHierarchyModel`): the
    heading-hierarchy stage is ported — bookmarks (a pure-lopdf outline
    reader: titles, depths, XYZ/FitH/FitBH/FitR destinations) > legal/outline
    numbering (the full marker grammar incl. ambiguous single-letter
    Roman/alpha resolution) > font style (a hand-rolled port of docling's
    conservative font-name parser plus size clustering), with docling's
    fuzzy bookmark matcher (a `difflib.SequenceMatcher.ratio` port) and
    list-item promotion. Off by default on every surface, like upstream —
    all conformance numbers are measured with it off. Deliberate
    divergences: style aggregates over text-layer *glyphs* rather than
    parsed line cells (same signal, finer granularity), OCR-only headings
    carry no style signal (docling reads OCR cell heights), and enabling
    the stage buffers a streaming Markdown conversion into one chunk (the
    stage needs the whole assembled document; streamed output stays
    byte-identical to buffered).

---

## 5. Not migrated / out of scope

Nothing here blocks day-to-day conversion: every remaining item is either a
deliberate scope boundary or a cosmetic, single-fixture polish gap.

**Out of scope by design:**

- **Local VLM full-page inference** (SmolDocling-class models in-process).
  Model-bound; out of scope for the discriminative port. The **remote** VLM
  pipeline (#77) *is* implemented: `--pipeline vlm --vlm-endpoint URL
  --vlm-model NAME` renders pages via pdfium, converts them through any
  OpenAI-compatible vision endpoint (LM Studio / Ollama / vLLM / hosted) and
  parses the returned DocLang with the existing reader — see the README's
  "VLM pipeline" section. Also on the Node bindings as `pipeline: 'vlm'`
  (`vlmEndpoint` / `vlmModel` / `vlmApiKey` / `vlmPrompt` / `vlmMaxTokens`),
  on the Python bindings as the same-named constructor kwargs, and on serve
  as `pipeline=vlm` + `vlm_*` request options (#304; a request-supplied
  `vlm_endpoint` sits behind `--allow-url-fetch` + the SSRF check, or pin it
  server-side via `DOCLING_RS_VLM_*`). Measured against Python docling's
  `VlmPipeline` on the same granite-docling endpoint: **87.7% mean
  similarity over the 18-fixture PDF corpus, 3 byte-exact** (#311; drift is
  mostly render-scale-induced — see PDF_CONFORMANCE.md). (**Audio/ASR is now done** — see §2; Opus and AVI,
  which symphonia cannot decode, use the optional ffmpeg fallback. The **enrichment
  models are now done** too: DocumentFigureClassifier-v2.5 for
  `do_picture_classification` and CodeFormulaV2 — an Idefics3-class VLM,
  exported to a three-graph ONNX set with a KV-cached greedy decode verified
  token-identical to `transformers.generate` — for `do_code_enrichment` /
  `do_formula_enrichment`; opt-in flags on the converter/CLI/Python bindings,
  and since #423 on docling-serve — `do_picture_classification` /
  `do_code_enrichment` / `do_formula_enrichment` as query, multipart and JSON
  body options, Python docling-serve's names, in the OpenAPI spec and the web
  UI; the warm pipeline is rebuilt when a request's enrichment mix differs
  from the cached instance's, as for the model switches — and the Node
  bindings (`doPictureClassification` / `doCodeEnrichment` /
  `doFormulaEnrichment`, also on `new Pipeline()`);
  conformance-checked by `scripts/conformance/enrich_conformance.sh`.)

**Now migrated (previously listed here):**

- **XML DocLang input backend.** Reading `.dclg`/`.dclg.xml` (bare DocLang XML)
  and `.dclx` archives back into a `DoclingDocument` — the corpus gap closed
  itself once `--to dclx` shipped: docling's own `.dclx` groundtruth archives
  are the sources, and docling 2.112 reads them natively (`InputFormat.DCLX`),
  so the backend is scored live like every other format — **15/15 exact**,
  reproducing docling's own round-trip semantics (whitespace collapse vs
  verbatim CDATA/`<content>`, span text re-expansion, the formatting docling
  drops on list items and hyperlink targets).

- **DOCX grouped/anchored drawings and floating text frames.** Blip-less
  DrawingML shapes yield docling's one-rendered-picture-per-paragraph as a
  placeholder (docling rasterizes them through LibreOffice; the Markdown
  placeholder is identical without rendering), pictures are emitted for
  heading/list/checkbox paragraphs too, and the textbox de-duplication matches
  docling's per-paragraph scope — `drawingml` and `textbox` are exact,
  **DOCX is 27/27**.

- **Legacy APS-text patents.** USPTO covers the modern `v4x` XML, the 2001-era
  `pap-v15` applications (`pa`) and `PATDOC`/ST.32 grants (`pg`) with their CALS
  tables, **and** the legacy **APS plain text** (`pftaps`): docling routes it to
  its plain-text backend (one DocLang `<text>` dump), and docling.rs reproduces
  that serialization byte-exactly — the `.dclx` is a perfect match
  ([issue #44](https://github.com/docling-project/docling.rs/issues/44), done).

- **ODF presentation frames** — done, **all native files exact**: `.odp`
  slides get their title frame (or slide name) as the title, free shape text,
  chart pictures with classification ("Bar chart") + data tables, and the
  speaker-notes drop; `.odt` merged cells repeat their text like docling's
  plain `TableData` grid (while rich cells dedup), and paragraph runs
  reproduce docling's lxml head-text semantics (a tail after a styled span is
  dropped). Everything else on ODF was already done: mixed-style list
  continuation, empty-list-item level collapse, ODS sheet→table region
  detection with numeric alignment, and rich table cells.

**Minor known gaps (cosmetic, tracked per-fixture):**

- ~~**`wiki_duck` offline rendering.**~~ **Closed** — the HTML corpus is now
  32/32 Markdown-exact against live docling 2.112, `wiki_duck` included. What
  finished it (issue #81): rich table cells serialized with inline markup and
  docling's `visited`-set span de-duplication, `to_single_text_element`
  figure-caption run spacing, `mw:File` indicator images (alt caption +
  placeholder), `<footer>` → furniture layer, and `<br>`
  annotation-boundary handling. The HTML subsystem also covers key-value form
  regions, inline visibility suppression, deep nested-table cell flattening
  with BeautifulSoup whitespace semantics, and — behind the optional
  `web-browser` feature / `--use-web-browser` flag — CSS-cascade visibility
  suppression via Rust-driven Chromium.


---

## 6. Extensions

- **`docling-rag`** — documents → chunking → embeddings → vector search,
  with swappable embedders (Ollama/Gemini/local ONNX), stores
  (SQLite+sqlite-vec / PostgreSQL+pgvector), LLM, sources and queues, plus an
  eval harness and a REST API. See the crate README.
- **`docling-node`** — Node.js/Bun N-API bindings (npm package): the full
  converter surface plus the chunkers, Markdown/chunk streaming, a warm
  `Pipeline` for many PDFs, and the remote VLM pipeline (`pipeline: 'vlm'`).
- **`docling-wasm`** — WebAssembly bindings: the declarative converters (and
  digital PDFs via the opt-in `pdf-text` text-layer feature — the same
  extraction as `--no-ocr`, no pdfium/ONNX) run fully client-side in the
  browser, ~1.9 MB gzipped; scanned PDFs return a "needs OCR" error. Python
  docling has no equivalent. See the crate README.
- **`docling-py`** — PyO3 bindings (PyPI package `docling-rs`): a strangler-fig
  drop-in for docling's Python API where the Rust engine is the document
  processor and `result.document` is a genuine `docling_core` `DoclingDocument`,
  so its `export_to_markdown()` / `export_to_dict()` / chunkers are docling's
  own code.

## 7. Testing

- **`cargo test`** — unit tests per backend/serializer **plus an output-
  regression suite** (`crates/docling/tests/regression.rs`): every
  declarative source under `crates/docling/tests/data/<fmt>/sources/` is
  converted to legacy Markdown, strict Markdown and docling JSON and compared to
  committed fixtures (159 sources × 3). `DOCLING_RS_REGEN=1` refreshes them.
  The JSON fixtures double as a docling-core load check.
- **Snapshot harness** — `scripts/conformance/pdf_conformance.sh` regenerates and diffs the
  PDF/image/METS baseline (needs pdfium + the ONNX models; **94 outputs, all
  matching the committed baseline**).
- **Conformance** — `scripts/conformance/conformance.sh <fmt>` scores a format against the
  latest published docling (installed from PyPI; how-to in §9).
- **VLM / DocLang extras** — `scripts/conformance/vlm_conformance.sh` (VLM
  pipeline vs Python docling's `VlmPipeline`) and
  `scripts/conformance/dclx_pdf_tol_sweep.sh` (geometry-tolerance sweep for
  the PDF `.dclx` diff).
- **Differential / perf** — `scripts/conformance/compare.sh`, `scripts/test/performance.sh`.
  The PDF pipeline's profiling data, the INT8/SIMD optimization results
  (4.3× warm vs Python docling on the ML pipeline), and the remaining
  performance backlog live in [`PDF_CONFORMANCE.md`](./PDF_CONFORMANCE.md).

CI (`.github/workflows/ci.yml`) gates every pull request and master push on
`cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` and
`cargo test` (the fast pure-Rust suite — no model downloads). fmt/clippy run on a
**pinned** toolchain (`LINT_TOOLCHAIN` in the workflow) so a new stable can't fail
CI on unrelated commits; tests run on current `stable`. On master it then runs
`scripts/ci/release.sh`: it derives the next version from the conventional-commit
messages since the last `v*` tag (`feat:` → minor, `fix:`/`perf:` → patch, a
`type!:`/`BREAKING CHANGE` → major; docs/chore/ci/etc → no release), bumps the
workspace version, commits + tags it (with `[skip ci]`, via the `RELEASE_PAT`
admin token — needed to satisfy the master ruleset — so it
doesn't loop), and publishes the crates with `scripts/ci/ci_publish.sh` in
dependency order — skipping any version already on crates.io.

---

## 8. Goals & design rules (unchanged)

- A tiny, obvious public API — one `DocumentConverter`, one `convert`, one
  `DoclingDocument` you can `export_to_markdown()` / `export_to_json()`.
- Dependency-light pure-Rust parsing for everything that isn't ML.
- Output byte-compatible with docling-core's serializers where it reasonably can
  be, so the port is a drop-in for downstream Markdown/JSON consumers.
- The ML stack is *not* reimplemented in PyTorch-equivalent Rust; it is
  quarantined behind ONNX (`ort`) inference in `docling-pdf`.

---

## 9. Keeping up with upstream docling

The port is built to be *measured against* upstream rather than merely
inspired by it, which makes tracking new docling releases a mechanical
process instead of a guess:

1. **Detect drift.** `scripts/conformance/conformance.sh <fmt>` installs the **latest
   published docling from PyPI** into an isolated venv and byte-diffs both
   engines' Markdown over the committed corpus, per fixture. An upstream
   release that changes output (a serializer tweak, a new label, a model
   bump) shows up as a concrete per-fixture diff — not as silent divergence.
   `scripts/conformance/compare.sh` does the same for a single ad-hoc document.
2. **Classify each diff.** Either upstream changed *serialization/logic* —
   port the change to the matching backend/serializer (the crate layout in §1
   maps one-to-one to docling's modules, so the port target is usually
   obvious) — or upstream shipped *new models*, in which case
   `scripts/install/export_layout.py` / `export_tableformer.py` re-export the new
   checkpoints to ONNX, `scripts/install/quantize_models.py` re-quantizes, and
   `.github/workflows/publish-models.yml` republishes the model release
   (bump the tag when the export itself changes).
3. **Re-gate.** `scripts/conformance/pdf_conformance.sh` (deterministic snapshot baseline)
   plus the 159-source regression suite in `cargo test` confirm nothing else
   moved. The committed PDF groundtruth is regenerated from live docling
   (`scripts/conformance/pdf_groundtruth.sh`) whenever upstream output legitimately
   changes, so "exact" always means *exact against current docling*.
4. **New formats/features** follow the same recipe the existing 30 formats
   did: a backend module + fixtures + conformance scoring, tracked in §2.

### Running the comparison yourself

The yardstick is **Markdown output**: both projects expose the same operation —
`convert(file).document.export_to_markdown()` — so diffing the two Markdown
strings is a direct, apples-to-apples comparison. Two axes: **correctness**
(A, B) and **performance** (C). Current numbers live in §2; this section is
how to reproduce them.

#### Local docling setup

The comparison scripts install the **latest published** `docling` from PyPI into
an isolated `docling.rs/.venv-compare` (via `uv`) on first run:

```bash
scripts/conformance/setup-docling.sh      # optional; the other scripts call this automatically
```

Published docling 2.x bundles every format backend plus the full PDF pipeline
(torch + models), so the first install pulls a few hundred MB. For the
declarative formats the Python side still calls the format backend directly (see
`scripts/conformance/docling_convert.py`) rather than `DocumentConverter`, so it avoids
paying the `torch` import cost on every run — the same conversion work, kept
apples-to-apples with what `docling.rs` does.

### A. Scoring against docling across a corpus

This repo ships a regression corpus under `tests/data/<format>/`:

```text
tests/data/html/sources/example_01.html          # input
tests/data/html/groundtruth/example_01.html.md    # older committed reference
```

`conformance.sh` scores the Rust port against the **latest published docling**
(installed from PyPI on first run — see `_common.sh`), per format:

```bash
scripts/conformance/conformance.sh html
scripts/conformance/conformance.sh docx
```

It prints a per-fixture diff-line count and a summary:

```text
FIXTURE                                        DIFF-LINES
example_01.html                                         5
example_02.html                                      EXACT
...
Exact (strict):                10 / 32
Whitespace-normalized matches: 12 / 32
```

The second metric ignores spacing-only differences (collapsing runs of
whitespace, trimming line ends) — useful when our output is the more faithful
one, e.g. dropping docling's spurious double space in a fraction. A row that
matches only after normalization is flagged `N (ws-ok)`.

> The reference is always the installed docling. The committed groundtruth `.md`
> is used only as a fallback for sources docling can't convert — it predates
> docling-core's current serializer (e.g. its compact `| - |` tables), so it is
> not the source of truth.

### B. Live, head-to-head on any file

To compare on a file that isn't in the corpus — or to confirm the groundtruth
hasn't drifted — run both implementations and diff:

```bash
scripts/conformance/compare.sh tests/data/html/sources/example_03.html
scripts/conformance/compare.sh /path/to/your/own.html
```

`compare.sh` runs the local Python docling backend and the Rust CLI on the same
file, normalizes trailing newlines, and shows a unified diff (or `✅ IDENTICAL`).
The local docling install is set up automatically on first run (see above).

Do it by hand if you prefer:

```bash
# Python (using the local install in .venv-compare)
.venv-compare/bin/python scripts/conformance/docling_convert.py in.html > py.md

# Rust
cargo run -p docling-cli -- in.html > rs.md

diff -u py.md rs.md
```

### C. Performance (time, CPU, memory)

`scripts/test/performance.sh` measures the processing cost of each engine on one
file — wall-clock time, CPU utilization, and peak resident memory — using GNU
`/usr/bin/time`. The Rust side is built in `--release`; the Python side runs the
installed docling (declarative backends, no `torch` import).

```bash
scripts/test/performance.sh tests/data/html/sources/wiki_duck.html 10   # 10 runs
```

```text
================ end-to-end (whole process) ================
ENGINE                     RUNS   TIME-min   TIME-avg      CPU     PEAK-MEM
docling (python)              6      1.39s      1.41s     363%     125.5 MB
docling.rs (rust)           6   0.00755s   0.00755s     100%       4.8 MB

  wall-time speedup (avg):  186.8x faster (rust)
  peak-memory ratio:        26.4x less (rust)

================ conversion only (startup excluded) ========
  python (warm, in-process): 0.4736s/doc, peak 134.6 MB
  rust   (whole process incl. startup): 0.00755s/doc — startup is negligible
  warm-conversion speedup:   62.7x faster (rust)
```

**Reading the numbers fairly.** The end-to-end Python time includes interpreter
startup plus importing docling/beautifulsoup4/numpy (~0.3–0.6s), which dominates
on small inputs — a real cost for one-shot CLI use, but not representative of a
long-running service. The script therefore also reports a **warm** number:
Python imports once, then converts in a loop, isolating the actual parse work.
Rust's process startup is ~1 ms, so its end-to-end figure already *is* its warm
figure. Use larger inputs (e.g. `wiki_duck.html`) to see steady-state behavior;
tiny files mostly measure Python's startup.

### Worked example

`tests/data/html/sources/example_01.html` → Python (left) vs Rust (right):

```diff
  # Introduction

  This is the first paragraph of the introduction.

  ## Background

  Some background information here.

  Example image

  <!-- image -->

  - First item in unordered list
  - Second item in unordered list

  1. First item in ordered list
  2. Second item in ordered list
-
- 42. First item in ordered list with start
- 43. Second item in ordered list with start
+ 3. First item in ordered list with start
+ 4. Second item in ordered list with start
```

Headings, paragraphs, the image placeholder, unordered list, and the first
ordered list are byte-identical. The only difference is the `<ol start="42">`
case — see the divergence table below.

### How to read the numbers

`conformance.sh` counts **diff lines** (`diff` `<`/`>` markers): one changed line
shows as `2`. It reports two summary counts — **Exact (strict)** byte-for-byte and
**Whitespace-normalized matches** (spacing-only diffs ignored; a fixture that
matches only after normalization is flagged `N (ws-ok)`). The point isn't the
absolute score — it's the trend as gaps in the table get closed, and catching
regressions when a change makes a previously-matching fixture diverge.

For CI, gate on the summary (e.g. fail if the exact-match count drops): it
compares against the docling version actually installed, so it won't flag
differences that are really just a stale committed corpus.

What this cannot absorb automatically: upstream features that need new model
*architectures* (the VLM full-page pipeline — out of scope per §5) and
places where the document models intentionally differ (§4). Those are
documented divergences rather than drift.

---

## Appendix — original phased plan (history)

The port followed roughly: **Phase 0** skeleton & API → **Phase 2** text/markup
(Markdown, CSV, HTML, AsciiDoc, DeepSeek) → **Phase 3** Office & e-book (DOCX,
PPTX, XLSX, EPUB, ODF) → **Phase 4** long tail (XML families, LaTeX, Email,
WebVTT, JSON) → **Phase 5–6** the PDF/image ML pipeline (pdfium + ONNX layout/OCR
+ geometric tables) → output formats (strict Markdown, JSON, image extraction) →
**Phase 7** audio/ASR (symphonia + ONNX Whisper). The Node.js/Bun (`docling-node`)
and Python (`docling-py`, PyO3) interop bindings followed.

## The meat-grinder mascot 🦀

The mascot — a duck feeding a document into a meat grinder
([`docs/assets/logo.svg`](./assets/logo.svg)) — captures what this does: a
grinder is the machine you push anything through to get a single, uniform mince,
which is exactly what happens to documents here — PDF, DOCX, HTML, XLSX … all
come out as one `DoclingDocument`. And it's written in Rust, so Ferris the crab
🦀 still gets a seat.
