# docling

The converter library of [docling.rs](https://github.com/docling-project/docling.rs)
— a Rust port of IBM's [docling](https://github.com/docling-project/docling).
`DocumentConverter` turns PDF, Office, HTML, scans, audio and 40-odd other
formats into a `DoclingDocument` and exports it as Markdown, docling JSON,
HTML, text, DocLang, LaTeX, Pandoc AST, WebVTT or RAG chunks. Every other
docling.rs surface — the `docling-rs` CLI, the HTTP server, the Python, Node,
wasm and C bindings — wraps this crate, so what is documented here applies
to all of them.

```bash
cargo add docling
```

PDF, image and audio conversion need the ONNX models
([`docs/MODELS.md`](../../docs/MODELS.md)); the declarative formats need
nothing. Overview, install options and benchmarks: the
[project README](../../README.md).

## Rust API

```rust
use docling::{DocumentConverter, SourceDocument};

let converter = DocumentConverter::new();
let result = converter
    .convert(SourceDocument::from_file("input.md").unwrap())
    .unwrap();

println!("{}", result.document.export_to_markdown()); // Markdown
println!("{}", result.document.export_to_json());     // docling DoclingDocument JSON
```

A failed conversion is a `ConversionError`; when the document is encrypted,
`err.encryption()` returns the typed reason (#636) — `NeedPassword` and
`WrongPassword` are worth prompting for a password (`needs_password()`),
`NotDecryptable` / `Malformed` are not:

```rust
match converter.convert(SourceDocument::from_file("locked.pdf").unwrap()) {
    Ok(result) => println!("{}", result.document.export_to_markdown()),
    Err(e) if e.encryption().is_some_and(|k| k.needs_password()) => ask_for_password(),
    Err(e) => eprintln!("{e}"),
}
```

`docling-rs serve` answers the same case with `422 {"error": …, "code":
"password_required" | "wrong_password" | "not_decryptable" |
"malformed_encryption"}`; the Python bindings raise `PasswordRequiredError` /
`WrongPasswordError` / `EncryptionError`, subclasses of `ConversionError`.

## One option set, every surface — `ConvertOptions`

Every conversion knob is one serializable struct,
[`docling::ConvertOptions`](src/options.rs) (#577), and every
surface fills it — the CLI from flags, serve from the query string / JSON
body / multipart parts, Python and Node from keyword arguments and option
objects, the C ABI and wasm from one JSON object — then runs the same
`validate()` and `apply()`. The defaults live in
`DocumentConverter::default()`; an unset option means that default
everywhere. The ones most conversions reach for:

| Wire name | CLI | Meaning |
|---|---|---|
| `pages` | `--pages A-B` | PDF page window (#80) |
| `no_ocr` · `text_layer_only` · `force_full_page_ocr` · `replace_broken_text_layer` | `--no-ocr` · `--text-layer-only[=auto]` · `--force-full-page-ocr` · `--replace-broken-text-layer` | never OCR, layout and tables kept · the text layer alone, no ML (`auto`: per document — the text layer when it carries the document, the pipeline with OCR when it does not) · OCR every page even with a text layer · OCR only the pages whose text layer is broken (#1006) |
| `no_table_former` | `--no-table-former` | geometric tables instead of TableFormer |
| `ocr_lang` · `ocr_engine` | `--ocr-lang` · `--ocr-engine` | `en` / `ch` / BCP-47; `ppocr` or `tesseract` (#460) |
| `heading_hierarchy` | `--heading-hierarchy` | infer PDF heading levels (#302) |
| `images_scale` · `page_images` | `--images-scale` · `--page-images` | picture-crop resolution; keep page renders in the JSON (#520) |
| `document_timeout` | `--document-timeout` | per-document budget; the pages done so far become a partial success (#497) |
| `password` | `--password` | encrypted PDF and Office documents (#611, #625) |
| `image_sources` | `--image-sources` | `none` / `embedded` / `local` / `remote` — which `<img>` references resolve (#646) |
| `do_picture_classification` · `do_code_enrichment` · `do_formula_enrichment` · `do_picture_ocr` | `--enrich-picture-classes` · `--enrich-code` · `--enrich-formula` · `--picture-ocr` | the enrichment models (#423, #645) |
| `do_picture_description` + `picture_description_endpoint`, `picture_description_model`, `picture_description_mode` | `--picture-description --picture-description-mode caption\|ocr` | every picture described through an OpenAI-compatible vision endpoint — MarkItDown's caption and OCR prompts (#670) |
| `redact_pii` | `--redact-pii` | PII redaction before export (#621) |
| `pipeline` + `vlm_endpoint`, `vlm_model` | `--pipeline vlm --vlm-endpoint … --vlm-model …` | the remote vision-model pipeline (#77) |
| `strict` · `compact_tables` · `page_break_placeholder` | `--strict` · `--compact-tables` · `--page-break-placeholder` | Markdown dialect, unpadded tables, text between pages |

The full table — every option with its values, default and each surface's
spelling — is [`docs/OPTIONS.md`](../../docs/OPTIONS.md); an inventory test holds
every surface's documentation to it, so an option cannot ship on one surface
and silently miss another.

```rust
use docling::{ConvertOptions, DocumentConverter};

let options: ConvertOptions = serde_json::from_str(r#"{"pages": "1-3", "no_ocr": true}"#)?;
let converter = DocumentConverter::from_options(&options)?; // validated + applied
```

## Output formats

| `--to` / `to` | Library | What it is |
|---|---|---|
| `md` (default) | `export_to_markdown()` | docling's Markdown byte for byte; `--strict` for the cleaner dialect (below); `--images embedded\|referenced` for pictures |
| `json` | `export_to_json()` | docling-core's `DoclingDocument` wire format (schema 1.11.0): the `body` tree of `$ref`s into `texts` / `groups` / `tables` / `pictures`, labels, list groups, table grids, images as `data:` URIs. Loads straight into Python docling-core (`DoclingDocument.load_from_json`) and round-trips to the same Markdown |
| `html` | `export_to_html()` | docling-core's `HTMLDocSerializer` with its defaults: stylesheet, `<h{level+1}>`, inline groups, tables with spans and rich cells, `<figure>` pictures, MathML formulas (a literal port of `latex2mathml`). Byte-identical to docling-core on the whole declarative corpus (265/265) |
| `text` | `export_to_text()` | docling's `--to text` (#613): the Markdown with the decoration off — no `#`, no emphasis markers, links as their label, code without fences, no escaping; lists, checkboxes and table grids kept |
| `dclx` | `export_to_doclang()`, `export_to_doclang_with_images(mode)`, `docling::dclx::save_as_dclx` | DocLang — docling 2.110's XML of the tree (`<doclang version="0.7">`), pretty-printed like `minidom.toprettyxml`; `.dclx` is the OPC archive `save_as_doclang()` writes (`document.xml` + one PNG part per picture). `export_to_doclang()` is that `document.xml` (pictures as `<src uri="assets/…"/>`); `export_to_doclang_with_images(ImageMode::Placeholder)` is docling-core's own `export_to_doclang()` default, no `<src>`. Reads back in too: `.dclg` / `.dclx` are input formats (15/15 exact vs docling) |
| `latex` | `export_to_latex()` | docling 2.124's `LaTeXDocSerializer` with its defaults: `article` preamble, `\title` + `\maketitle`, sectioning, `itemize` / `enumerate`, `tabular` grids, `figure` placeholders, `verbatim`, `$$…$$`. 93 of 116 fixtures byte-exact against `docling --to latex` (98 once upstream's duplicated formatted list items — docling-core#740 — are normalized away) |
| `pandoc` | `export_to_pandoc_json()` | Pandoc's JSON AST (#515, `pandoc-api-version` 1.23.1.1), so every Pandoc writer — DOCX, ODT, EPUB, reST, Org, Typst, … — sits behind the parser; pictures embedded by default. `--pandoc-api-version` states the version a consumer needs; `--pandoc-heading-depth source` (#848; `PandocExportOptions::heading_depth`) writes a section heading at the depth its author wrote — a DOCX `Heading 1` as `Header 1`, as `pandoc -f docx` reads it — instead of docling's title-reserving `n + 1` |
| `vtt` | `export_to_vtt()` | WebVTT subtitles (#614, docling-core's `WebVTTDocSerializer`): one cue per timed item — an ASR transcript's segments, a WebVTT input's cues with voices and formatting. Untimed content is not represented |
| `chunks` | `docling::chunker` | both chunkers' records (below) |
| `images` | — | no conversion: a PDF's pages as PNG (`<stem>_page_NNNN.png`; `{"pages": […]}` on serve), honouring `--pages` and `--scale` 0.1–4.0 px/pt (#243) |

Every surface takes the same values — the CLI (batch mode writes
`<stem>.<ext>`, several formats at once with `--to md,json`), serve
(`to=`, the matching content type), Node / wasm / the C ABI (`to:`); the
Python bindings hand back docling-core's own `DoclingDocument`, so upstream's
`export_to_*` / `save_as_*` apply to it directly. Page breaks
(`--page-break-placeholder`) apply to Markdown and text.

```bash
docling-rs paper.pdf --to pandoc | pandoc -f json -t docx -o paper.docx
docling-rs talk.mp3 --to vtt > talk.vtt
docling-rs --to images --pages 2-3 --scale 1.5 paper.pdf   # paper_page_0002.png, paper_page_0003.png
```

How the JSON mirrors docling's item tree, the table-cell repair API and the
content layers the HTML / Markdown exports render: [`docs/MIGRATION.md` §
3](../../docs/MIGRATION.md#json-structure-and-content-layers); the Pandoc
mapping is in the same section's table.

An HTML conversion's tree also keeps the **source blocks** (#903, #904):
`doc.tree.blocks` lists every block of the source in document order — tag,
`id`, `class`, element path, its own normalized text, the items made from
it, or why none was — and each `TreeItem::block` points back at its block.
`tree.containers` lists the containers that carry an `id` or a `class`
(#910), `tree.path_of_id(id)` resolves either to its path and
`tree.blocks_under(path)` selects the blocks inside it.
`tree.source_text([i, j])` reads selected blocks with one newline between
them, free of any serializer's separators; a block's `inlines` carry its
inline elements with their ranges in the text, and `text_excluding` /
`source_text_excluding` leave the ones you name out (#911):

```rust
let tree = doc.tree.as_ref().unwrap();
let p79 = tree.blocks.iter().position(|b| b.text.starts_with("79 ")).unwrap();
println!("{} {:?} -> {}", tree.blocks[p79].path, tree.blocks[p79].id, tree.source_text([p79]));
```

## Chunking (docling's Hierarchical & Hybrid chunkers)

`docling_core.transforms.chunker` ported to Rust — the chunkers RAG
pipelines feed to embedding models. `HierarchicalChunker` yields one chunk
per document item (whole lists, triplet-serialized tables, picture
captions) with its heading path; `HybridChunker` refines them with a
tokenizer — splits oversized chunks (item boundaries, then docling's
`semchunk` inside text, tables line by line) and merges undersized
same-heading neighbours. Against docling's chunkers on the 83-document
corpus (`scripts/conformance/chunks_conformance.sh`): hierarchical 98.8 % /
hybrid 96.2 % identical chunk records.

```rust
use docling::chunker::{contextualize, HierarchicalChunker, HybridChunker, HuggingFaceTokenizer};

let chunks = HierarchicalChunker.chunk(&result.document);          // structure-driven
let tok = HuggingFaceTokenizer::from_file(".models/chunk/tokenizer.json", 256)?; // feature "chunking"
for chunk in HybridChunker::new(tok).chunk(&result.document) {
    let embed_me = contextualize(&chunk); // heading path + chunk text
}
```

```python
from docling_rs.chunking import HierarchicalChunker, HybridChunker
chunker = HybridChunker(max_tokens=256)
for chunk in chunker.chunk(doc):
    embed_me = chunker.contextualize(chunk)
```

`download_dependencies.sh` fetches MiniLM's tokenizer to
`.models/chunk/tokenizer.json`, which every surface picks up
(`DOCLING_CHUNK_TOKENIZER`, `DOCLING_CHUNK_MAX_TOKENS` override it;
per run `--chunker hierarchical|hybrid`, `--chunk-tokenizer`,
`--chunk-max-tokens`, `--no-chunk-merge-peers` and the matching serve
fields, #256). Also in the [Node bindings](../docling-node)
(`chunkFile` / `chunkDocument`), the [Python bindings](../docling-py)
(`docling_rs.chunking`) and the [RAG subsystem](../docling-rag).

## Image extraction

Backends that have the image populate `Node::Picture { image }`: the
PDF/image pipeline crops figure regions, DOCX / PPTX / MHTML pull embedded
blobs, Windows metafiles (EMF / WMF) in the office formats are rendered to
PNG in-process (#536). `ImageMode` — docling's `image_mode` — picks how
pictures render:

```rust
use docling::ImageMode;
// self-contained Markdown: ![Image](data:image/png;base64,…)
let (md, _) = result.document.export_to_markdown_with_images(ImageMode::Embedded, "artifacts");
// referenced: ![Image](artifacts/image_000000.png) + the bytes to write
let (md, files) = result.document.export_to_markdown_with_images(ImageMode::Referenced, "artifacts");
for (path, bytes) in files { std::fs::write(path, bytes).unwrap(); }
```

JSON always embeds the images as docling `ImageRef`s; the default Markdown
stays `<!-- image -->`, like docling (the pixels are real, the base64 is
not byte-identical to docling's — a different PNG encoder). Pictures that
are *references* — HTML / EPUB / MHTML / JATS / AsciiDoc / ODF `<img src>`,
Markdown `![…](…)`, an email's `cid:` — stay placeholders by default and
resolve under `image_sources` (#646): `embedded` (`data:` URIs and parts of
the same container; no filesystem, no network), `local` (plus files under
the source's directory), `remote` (plus `http(s)`, confined to
`image_hosts`); `max_image_bytes` / `max_images` / `max_image_total_mb` /
`min_image_bytes` bound a document — details in
[`docs/OPTIONS.md`](../../docs/OPTIONS.md) and [`docs/SECURITY.md`](../../docs/SECURITY.md).

## `strict` Markdown (Rust-only)

By default `export_to_markdown()` reproduces docling's output byte-for-byte,
quirks included (`***x*** .`, dropped code-fence languages, `\_` escaping).
`strict(true)` gives cleaner, more conformant Markdown
(`export_to_markdown_with(strict)` per call; Python docling has no such
switch):

```text
legacy:  Foo ***both*** .   |   ``` (lang dropped)   |   Name: \_\_\_
strict:  Foo ***both***.    |   ```rust (lang kept)  |   Name: ___
```

## Streaming Markdown

`convert_streaming` returns the Markdown as an iterator of chunks instead of
one string — for piping a long document to stdout, an HTTP response or a
socket as it is produced:

```rust
use std::io::Write;
use docling::{DocumentConverter, SourceDocument};

let source = SourceDocument::from_file("input.pdf").unwrap();
let mut out = std::io::stdout();
for chunk in DocumentConverter::new().convert_streaming(source).unwrap() {
    out.write_all(chunk.unwrap().as_bytes()).unwrap();
}
```

PDF is where it pays: the pipeline processes pages in parallel and streams
each page's Markdown in document order as soon as it is ready (a one-page
look-ahead lets paragraphs that wrap across a page break still merge). The
conversion runs on a background thread, the iterator applies backpressure,
dropping it cancels the work, and the concatenated chunks are byte-identical
to `export_to_markdown()`. Every image mode streams
(`convert_streaming_images(source, mode)`); `referenced` writes each page's
image files under `artifacts_dir` as that page is emitted and drops the
bytes. Streaming is Markdown-only — JSON needs every node up front. The CLI
streams by default (`--no-stream` buffers; `--to json` always does).

## Progress events

docling's conversion progress events (docling 2.139, #783): a callback on
the converter hears every conversion's events, from the threads that
convert — the document's start and end, one event per PDF / image page
with the completed count (a page the `document_timeout` cut off is
reported as failed, so the count still reaches the total), and the picture
passes' item counts. `show_progress(true)` prints the built-in stderr
bars instead (what the CLI's `--progress` shows):

```rust
use std::sync::Arc;
use docling::{DocumentConverter, ProgressKind, SourceDocument};

let converter = DocumentConverter::new().progress_callback(Arc::new(|e| {
    if let ProgressKind::PageCompleted { completed_pages, total_pages, .. } = e.kind {
        eprintln!("{}: page {completed_pages}/{total_pages}", e.document_name);
    }
}));
converter.convert(SourceDocument::from_file("input.pdf").unwrap()).unwrap();
// a batch: convert_with_progress(source, document_index, per_call_callback)
```

`docling::progress` has the types; a conversion with no callback costs
nothing. Every surface carries it: `--progress` / `--no-progress` on the CLI
(on by default on a terminal), `progress_callback` / `show_progress` in
Python, `progressCallback` / `showProgress` in Node, a `progress` object on
serve's `GET /v1/status/{id}`, and `docling_convert_with_progress` in the C
ABI.

## PDF pipeline switches

All on every surface (library builder, CLI, serve, Python, Node) — values
and defaults in [`docs/OPTIONS.md`](../../docs/OPTIONS.md):

- `--pages A-B` (#80): converts that 1-based page window only; out-of-window
  pages are skipped before rasterization.
- `--document-timeout SECONDS` (#497): a per-document budget, checked
  between pages. Once spent, the pages already finished become the document
  and the result is docling's `PARTIAL_SUCCESS` with one error item; the CLI
  writes it and exits 0, serve answers `X-Docling-Status: partial_success`,
  a stream ends with a `Timeout` error item.
- `--text-layer-only`: no ML at all — the embedded text cells as flat
  paragraphs, the fastest path; a scanned PDF comes back empty rather than
  erroring. `--text-layer-only=auto` decides per document: that fast path
  when the text layer carries the document (no model loaded), the full
  pipeline — OCR included — when it does not (a scan, or a vestigial layer
  of a few typed-in form fields); `TextLayerOnly::Auto` in the API. `--no-ocr` (docling's `--no-ocr` since 2.0, #611) keeps layout
  and TableFormer but never runs OCR; `--no-table-former` keeps OCR but
  reconstructs tables geometrically. `--force-full-page-ocr` is the
  opposite: OCR every page from its render even when a text layer exists
  (for layers that lie — broken encodings, garbage subset fonts).
- `--heading-hierarchy` (#302, docling's `HeadingHierarchyModel`): infers
  section-header levels after assembly from the PDF outline, legal/outline
  numbering and font style, in that precedence. Off by default, as in the
  docling groundtruth.
- `--no-text-panels`: keeps every detected picture a picture instead of
  demoting uncaptioned dense-text "pictures" into paragraphs (#173).
- `--skip-empty-cells`, `--compact-tables` (#271): sparse-spreadsheet
  relief — omit empty cells from XLSX table rows; unpadded `| a | b |`
  Markdown tables. `--page-break-placeholder TEXT`: docling's
  `page_break_placeholder` between rendered pages.

Scanned pages come upright before layout and OCR: a `/Rotate` flag is
applied, and a page rotated physically in the raster is probed under each
90° hypothesis on the text detector's lines and un-rotated when a rotated
reading is clearly more confident (#225, #571; `DOCLING_RS_OCR_ORIENTATION=off`
disables it). A one-line paragraph the layout model files as `page_footer`
directly under a heading with no other body — a CV's `Languages` line — is
read as that heading's text instead of vanishing with the furniture.

## OCR

OCR has docling's two stages (#429, #570): RapidOCR's PP-OCRv6 text detector
finds the lines on a scanned page or image — inside the layout regions its
boxes are the recognizer's crops, the lines no region covers come out as
orphan text — and the recognizer reads them, PP-OCRv6 when installed and the
PP-OCRv3 pairs otherwise. On FUNSD's 199 scanned forms word recall is 0.90
against the annotations (Python docling 2.133 with RapidOCR: 0.85). The
default language is `en`; the conformance corpus was generated with the
multilingual `ch` model, so byte-for-byte comparisons with Python docling
run with `--ocr-lang ch`. `--ocr-engine tesseract` (#460) runs the system
`tesseract` binary on the same crops instead, with any of its languages.
Languages, `ocr_mode`, `ocr_scale`, the Tesseract environment and the
detector knobs: [`docs/OPTIONS.md` § OCR](../../docs/OPTIONS.md#ocr-engines-and-languages).

The two stages are also usable without page layout (#939) — for a caller
that brings its own crops, e.g. from the public detector:

```rust,ignore
use docling_pdf::{ocr_det::DetModel, OcrLang, OcrModel};

let mut det = DetModel::load(1)?;
let mut ocr = OcrModel::load_with(OcrLang::En, 4)?; // 4 recognition lanes
let boxes = det.detect(&img)?; // img pixels, reading order
for (b, line) in boxes.iter().zip(ocr.recognize_det_boxes(&img, &boxes)?) {
    // line.text, line.confidence (recognition, not b.score); nothing is
    // dropped — line.passes_text_score() is the page OCR's filter.
}
```

`recognize_crops(&[RgbImage])` reads ready-made line crops the same way;
both return one result per input, in input order.

## VLM pipeline (remote endpoint)

`--pipeline vlm` (#77) replaces the discriminative ML stack with a Vision
Language Model: each page is rendered in Rust and sent to any
OpenAI-compatible vision endpoint — LM Studio, Ollama, vLLM, a hosted
service — with docling's page-conversion prompt, and the answer is parsed by
the DocLang reader. No ONNX models load.

```bash
docling-rs --pipeline vlm \
  --vlm-endpoint http://localhost:11434/v1 \
  --vlm-model granite-docling \
  paper.pdf
```

The same on Node (`pipeline: 'vlm'`, `vlmEndpoint`, …), Python
(`pipeline="vlm", vlm_endpoint=…`) and serve (`pipeline=vlm` + the `vlm_*`
request options). Selecting the pipeline is always explicit: the
`DOCLING_RS_VLM_*` environment supplies values but never switches it on, and
the `vlm_*` options are inert under the standard pipeline. On serve a
request-supplied `vlm_endpoint` needs `--allow-url-fetch`; the safer
deployment pins `DOCLING_RS_VLM_ENDPOINT` / `DOCLING_RS_VLM_MODEL` on the
server and lets requests send only `pipeline=vlm`:

```bash
curl -F file=@paper.pdf 'localhost:5001/v1/convert?pipeline=vlm'
```

Answer grammars are detected per response (#322): DocTags (granite-docling),
DocLang XML, Chandra layout HTML, Unlimited-OCR grounding output and
DeepSeek-OCR Markdown; plain prose degrades to text. Measured against Python
docling's `VlmPipeline` on the same granite-docling endpoint: 87.7 % mean
similarity over 18 fixtures, 3 byte-exact
([docs/PDF_CONFORMANCE.md](../../docs/PDF_CONFORMANCE.md)). Flags, env
fallbacks, timeouts, retries and prompts: [`docs/OPTIONS.md` § VLM](../../docs/OPTIONS.md#vlm-pipeline).
A page whose request fails or whose answer was cut off (`max_tokens`, a
content filter) makes the result a `PartialSuccess` with that page's error
(`vlm::convert_vlm_result`; #762, #763) — the other pages are kept.

## Headless-browser HTML pre-render (optional)

Almost everything in the HTML backend is pure Rust, but one thing a static
parse can't do is resolve the **CSS cascade** — whether a stylesheet- or
class-driven rule makes an element `display:none` (e.g. a collapsed nav menu).
The optional `--use-web-browser` flag renders the page in the system Chromium
first, drops every element the browser computes as hidden, then feeds the
cleaned HTML through the normal Rust backend (so all structure/table/KVP/
formatting logic still runs in Rust — the browser only decides visibility). It
applies to every HTML-routing input: direct HTML, plus MHTML and EPUB (which
assemble HTML from their archives). It's driven straight from Rust over the
DevTools protocol via
[`headless_chrome`](https://crates.io/crates/headless_chrome) — no Node,
Playwright, or other runtime.

It's gated behind the off-by-default `web-browser` Cargo feature, so the standard
build stays browser-dependency-free:

```bash
cargo run -p docling-cli --features web-browser -- --use-web-browser page.html
```

Chromium is located via `$DOCLING_RS_CHROME`/`$CHROME`, then
`$PLAYWRIGHT_BROWSERS_PATH/chromium`, else autodetected. The page's CSS must be
reachable for the cascade to resolve — inline `<style>` works offline, but a
saved page that links external stylesheets needs those fetchable (with a base
host). Without the feature, `--use-web-browser` is a clear error rather than a
silent no-op.


## Encrypted Office documents

A password-protected `.docx`, `.xlsx`, `.pptx`, `.doc`, `.xls` or `.ppt` converts
with its password (#625 — beyond docling, whose password option opens PDFs
only). It is the same option as the PDF password: `--password` (or docling's
`--pdf-password`), `--password-file PATH` to keep it out of the process list,
`password` in serve, Python and Node (`pdf_password` / `pdfPassword`, the
earlier name, still work). Files with only a *modify*
password convert without one (PowerPoint encrypts them with its built-in
default password, which is tried automatically, as is Excel's). Without the
password — or with a wrong one — the conversion fails saying so; the schemes
are listed in `docs/MIGRATION.md`.

```bash
docling-rs budget.xlsx --password-file ~/.budget-password
curl -F file=@deck.pptx -F password=1234 localhost:5001/v1/convert
```


## Repaired PDF input

The PDF reader repairs what it can — a stream whose `/Length` overstates or
understates its data is read from its `endstream` boundary — and says
nothing about it. A caller that must refuse repaired sources asks (#934):
`.pdf_report_repairs(true)` (off by default; `--pdf-report-repairs`,
`pdf_report_repairs` on serve and in Python, `pdfReportRepairs` in Node)
puts the list on the result's `pdf_repairs`, and `pdf_repairs` runs the
same check on the bytes without converting:

```rust
use docling::{pdf_repairs, DocumentConverter, PdfRepair, SourceDocument};

let result = DocumentConverter::new()
    .pdf_report_repairs(true)
    .convert(SourceDocument::from_file("in.pdf")?)?;
if !result.pdf_repairs.unwrap_or_default().is_empty() {
    // refuse
}

let bytes = std::fs::read("in.pdf")?;
for repair in pdf_repairs(&bytes) {
    if let PdfRepair::StreamLength { object, declared, recovered, .. } = repair {
        eprintln!("object {object:?}: /Length {declared:?}, recovered {recovered:?}");
    }
}
```


## Pluggable backends

Add an input format, or read a built-in one your way, without forking (#680):
any `DeclarativeBackend` registered on a converter, with a `BackendMatcher`
for the sources it claims — by extension, media type, or a check on the
bytes — and a priority (highest first):

```rust
use std::sync::Arc;
use docling::{BackendMatcher, DocumentConverter};

let converter = DocumentConverter::new().register_backend(
    "rtf2",
    BackendMatcher::new()
        .extensions(["rtf2"])
        .media_types(["application/x-rtf2"])
        .sniff(|bytes| bytes.starts_with(b"{\\rtf2}")),
    Arc::new(Rtf2Backend),
    0,
);
let source = converter.load_file("notes.rtf2")?;   // InputFormat::Plugin
let result = converter.convert(source)?;
```

Registered backends come before the built-in dispatch, so a backend can take
over a format (`.ipynb` keeps `InputFormat::Ipynb`). `load_file` /
`load_named_bytes` accept the formats only a registered backend knows;
`supported_extensions()` lists them with the built-in ones. The document gets
the converter's usual post-processing (enrichment, redaction, metadata), and
streaming converts a claimed source buffered. A backend's error is the
conversion's, with no retry as a built-in format. Not routed: archive entries.
A complete example: [`examples/register_backend.rs`](./examples/register_backend.rs)
(`cargo run -p docling --example register_backend`). Python
(`docling_rs.register_backend`) and Node.js (`registerBackend`) register
functions in their own language.

## License

MIT, like the rest of docling.rs.
