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
> (a pure-Rust PDF text/metadata parser and page renderer — no native PDF library; the docling-parse renderer plugin is a development oracle — + ONNX
> layout/TableFormer/OCR + a port of docling-parse's line sanitizer) and is also
> measured byte-for-byte against upstream's groundtruth and live docling (2.150, docling-parse 7.25.0) —
> **9 / 21 PDF fixtures exact, 511 diff lines** against the groundtruth, 6 / 18 and 475 against live docling (see `PDF_CONFORMANCE.md`), with a snapshot baseline
> guarding against regressions. `cargo test` is green (unit tests + a 159-source
> output-regression suite).

**At a glance** (for a first-time reader from the docling side):

| | |
|---|---|
| **What** | A Rust port of docling's converter, backends, and discriminative PDF/ASR pipelines; same `convert → DoclingDocument → export_to_markdown()/json()` shape, single static binary, no Python/torch at runtime |
| **Conformance** | Declarative formats byte-for-byte vs *live* PyPI docling (most 100%, see §2); `.dclx` DocLang 0.9 output 132/146 byte-exact, 99% mean vs docling 2.148's own `.dclx` (§2); PDF ML path 6/14 fixtures byte-exact, rest close; every optimization is gated on this not regressing |
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
| **Converter** | `docling/document_converter.py` | `docling.rs` — `converter.rs` (format dispatch + XML content sniffing); `sniff.rs` (`InputFormat::detect`, content detection for nameless input and the retry of a misnamed file) |
| **ZIP input** (#557, docling.rs extension — docling takes no archives) | — | `archive.rs`: `DocumentConverter::convert_archive` converts each document of a `.zip` on its own (lazy per-entry `Converted` / `Skipped` / `Failed`), entries vetted from the central directory before inflating (`ArchiveLimits`: entries, per-entry / total size, compression ratio; unsafe `..` paths, nested archives, encrypted and `__MACOSX` entries skipped); the CLI expands a named `.zip` into batch items (`out/<archive>/<entry>`), serve a `.zip` upload into a batch; #677: a lone `.zip` on stdout (or `--archive-merge` anywhere) is one document — `## <entry path>` then each entry's content, MarkItDown's `_zip_converter` shape (`DocumentConverter::convert_archive_merged`) — and a ZIP inside the archive is read under `ArchiveLimits::max_depth` (`DOCLING_RS_ZIP_MAX_DEPTH`, default 1, at most 3; one shared budget). Python `DocumentConverter.convert_archive(source)` yields an `ArchiveItem` per entry; Node `convertArchiveFile` / `convertArchive` (+ `*Async`, and the `DocumentConverter` methods) return `ArchiveItem[]`. `InputFormat` gains no variant and `convert()` still rejects `.zip` |
| **Backends** | `docling/backend/*` | `docling.rs` — `backend/*` (one per format) |
| **PDF/ML pipeline** | `docling/pipeline/*`, `docling/models/*` | `docling-pdf` — pure-Rust text layer + renderer, ONNX layout/OCR + assembly |
| **Audio/ASR pipeline** | `docling/pipeline/asr_pipeline.py` | `docling-asr` — symphonia decode + log-mel + ONNX Whisper |
| **Chunking** | `docling-core` chunkers (`HierarchicalChunker`/`HybridChunker`) | `docling-core::chunker`, re-exported as `docling::chunker` |
| **CLI** | `docling/cli` | `docling-cli` (incl. warm batch mode: `SOURCE... --output DIR`, `--input GLOB --output DIR [--jobs N]`, `--abort-on-error`, #489; repeatable `--to`, #491; `--list-input-formats` / `--list-output-formats`, #603 — Pandoc's discovery flags, which Python's CLI lacks: the extensions this build converts and the `--to` values, sorted, one per line) |
| **Beyond upstream's packages** | docling-serve (separate repo) | `docling-serve` (HTTP API), `docling-rag`, Python/Node/wasm bindings, GPU execution providers (`cuda`/`tensorrt`/`directml`/`coreml` features, `DOCLING_RS_EP`) |
| **MCP server** (#669) | docling-mcp (separate repo, Python, its own tool set around a cached `DoclingDocument`) | `docling-rs mcp` (default `mcp` feature of the CLI, rmcp): MarkItDown's MCP tool `convert_to_markdown(uri)` — same name and contract, so `markitdown-mcp` configs switch by changing the command — plus `convert(uri, to, images, options)` over the shared `ConvertOptions` and `list_input_formats()`; stdio or Streamable HTTP (`--http`); local reads confined to `--root`, URLs only with `--allow-url-fetch` through `docling::fetch` (the SSRF block-list on every redirect hop); a PDF without the layout models converts from its text layer (one stderr note) rather than failing |
| **MarkItDown facade** (#674, docling.rs extension) | — (Microsoft MarkItDown's API) | `docling_rs.markitdown`: `MarkItDown` / `StreamInfo` / `DocumentConverterResult` / its exceptions over `DocumentConverter`, docling's Markdown out (compact tables, MarkItDown's `## <sheet>` headings for XLSX), `llm_client` picture captions, text-layer PDFs until the models are downloaded; `python -m docling_rs.markitdown` for the CLI |
| **Marker facade** (docling.rs extension) | — (datalab Marker's API) | `docling_rs.marker`: Marker 2.0's module layout — `converters.pdf.PdfConverter` / `table.TableConverter` / `ocr.OCRConverter`, `models.create_model_dict`, `config.parser.ConfigParser`, the renderers' `MarkdownOutput` / `HTMLOutput` / `JSONOutput` / `ChunkOutput` / `OCRJSONOutput`, `output.text_from_rendered` / `save_output`, `schema.BlockTypes`, `settings` — over `DocumentConverter`: docling's Markdown with Marker's `![](_page_0_Picture_3.jpeg)` image names and `{page}----` separators, the block tree with Marker's ids, top-left polygons, `section_hierarchy` and `<content-ref>` html, `table_of_contents` / `page_stats` metadata; every format docling.rs reads through `PdfConverter` (Marker renders Office to PDF first); `mode`, `use_llm`, `processors` accepted and ignored; `python -m docling_rs.marker` for `marker_single` / `marker` |
| **MinerU facade** (docling.rs extension) | — (OpenDataLab MinerU 4's SDK) | `docling_rs.mineru`: `mineru.parser` (`parse` / `parse_async` → `ParseResult` with `pages`, `markdown()`, `structured_content()`, `to_dict` / `to_json` / `from_json`, `save(writer)`; `MinerUParser`; the writers; DocVortex's page-range grammar), `mineru.render` (`render(middle_json, RenderFormat.MARKDOWN \| HTML \| STRUCTURED_CONTENT \| CONTENT_LIST \| CONTENT_LIST_V2)`, the option classes, `RenderMode`) and `mineru.types` over `DocumentConverter`: the Middle JSON in DocVortex's shape (indexed blocks, normalized boxes, inline spans, visual parents with body / caption / footnote children) built from docling's items, MinerU's Markdown conventions and `save()` layout (`images/page_<p>_<type>_<index>.<ext>`); `tier` `flash` = the text layer, the others the full pipeline; `python -m docling_rs.mineru parse` for `mineru-kit parse` |

```text
crates/
├── docling-core/   # DoclingDocument, Node model, markdown/json/doclang/doctags serializers, chunker.rs, confidence.rs
├── docling/        # DocumentConverter, source/format detection, backend/*.rs, ooxml.rs
├── docling-pdf/    # pdfium_backend (the page walk), textparse, render/, raster/, layout (RT-DETR/ONNX), ocr (PP-OCRv3/ONNX), assemble, mets
├── docling-asr/    # audio decode (symphonia), mel.rs, whisper.rs (ONNX), tokenizer.rs; nemo_mel.rs, parakeet.rs, vad.rs (Parakeet TDT, #508)
├── docling-onnx/   # shared ONNX Runtime EP selection (DOCLING_RS_EP, cuda/tensorrt/directml/coreml features)
├── docling-cli/    # `--strict`, `--to md|json|html|text|dclx|chunks|latex|pandoc`, `--images …`, `--pages`, `--ocr-lang`, serve and mcp subcommands
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

### One option set across the surfaces (#577)

Every conversion option is one serializable struct, `docling::ConvertOptions`
(`crates/docling/src/options.rs`), with `validate()` and `apply()` in the
library: the CLI, docling-serve, the C ABI, the wasm module and the Python /
Node bindings parse their own input shape into it and share the rejection
rules, the messages and the mapping onto `DocumentConverter`; the engine
defaults are stated once (`DocumentConverter::default()`). Before, each
surface re-declared the fields and re-implemented the checks — the C ABI had
no `list_attachments`, the Python wrapper dropped five kwargs the native
class took, docling-serve's OpenAPI document was missing five request options
it accepted, and nothing caught any of it. The inventory test
(`crates/docling/tests/options_inventory.rs`) now holds every surface's
documentation to the central table (`docs/OPTIONS.md`, `OPTIONS` in
`options.rs`). docling has no single equivalent — its options are spread
over `PipelineOptions`, per-format backend options and docling-serve's
request model — so this is a docling.rs structure, not a parity item.

**2.0: `no_ocr` means docling's `--no-ocr`** (#611, a breaking change). A
script written for `docling convert` used to lose its headings, tables and
pictures here, because `--no-ocr` was docling.rs's skip-everything fast path.
Since 2.0 `--no-ocr` / `no_ocr` / `noOcr` / `DocumentConverter::no_ocr` is
docling's `do_ocr=False` — layout and TableFormer run, OCR never does —
`--skip-ocr` / `skip_ocr` / `skipOcr` / `::skip_ocr`, its old name, is still
read, and the fast path is `--text-layer-only` / `text_layer_only` /
`textLayerOnly` / `::text_layer_only` (Python's kwarg of that name, and
`do_ocr`, are unchanged). Replaying the corpus, `--text-layer-only` gives
byte for byte what `--no-ocr` gave on 1.104.3. The option also takes `auto`
(`--text-layer-only=auto`, `text_layer_only="auto"`, `textLayerOnly: "auto"`,
`::text_layer_only(TextLayerOnly::Auto)`): the text layer alone when it
carries the document, the full pipeline — OCR included — when it does not
(see §4, *Text layer first, OCR when there is none*). The CLI also takes docling's
spellings `--page-range`, `--image-export-mode`, `--no-tables`,
`--pdf-password` (and the `password` option on every surface — `pdf_password`
before #625, still read — which the engine always had but no caller passed)
and `--output-file`; see
`docs/OPTIONS.md`.

**Encrypted Office documents (#624, #625).** A password-protected `.doc`,
`.docx`, `.xls`, `.xlsx`, `.ppt` or `.pptx` converts with the password —
`--password` / `--password-file` on the CLI, `password` everywhere else (the
PDF password option, renamed from docling's `pdf_password`, which stays
accepted; `--pdf-password` too) — a
docling.rs extension: docling's password option is PDF-only, and an
encrypted Office file fails there. Decrypted: OOXML Agile (Office 2010+) and
Standard (2007) encryption, RC4 CryptoAPI (2002+, 40- to 128-bit) and Office
97/2000 RC4 for the binaries; the decrypted bytes are msoffcrypto-tool's
(the OOXML package, the `.doc` and `.xls` streams byte for byte; a `.ppt`'s
every persist object). Not decrypted: XOR obfuscation (Word/Excel 95), ODF
package encryption, a `.ppt`'s `Pictures` stream (the backend does not read
it); the `dataIntegrity` HMAC is not checked. Without the password the error
is `<format>: document is encrypted (a password is required to open it)`, with
a wrong one `<format>: document is encrypted and the password is wrong` (422
from the server) — where a `.ppt` used to convert to an empty document,
`.docx`/`.pptx` failed as `bad zip`, an ODF package as `no content.xml` (now
"encrypted with an unsupported scheme"), and `.xls`/`.xlsx` in calamine's
words. A `.ppt` is recognized by the `CryptSession10Container` its current
user edit references ([MS-PPT] 2.3.7). Default passwords are tried after the
given one, as Office does without prompting: PowerPoint encrypts a file that
has only a *modify* password with `/01Hannes Ruescher/01`, so those convert
without a password; Excel's `VelvetSweatshop` opens workbooks protected only
against structural edits; Word and Excel leave a modify-password-only file
unencrypted. A mislabelled encrypted file (a `.ppt` named `.pptx`) reports the
encryption, not the declared format's parse error.

**The encryption error is typed (#636; docling has no equivalent — its
backends raise `DocumentLoadError` with a backend-worded message).**
`ConversionError::encryption()` returns the
[`EncryptionError`](../crates/docling-core/src/encryption.rs) behind any of
the above — `NeedPassword`, `WrongPassword` (the two `needs_password()` is
true for), `NotDecryptable(scheme)` (ODF, iWork, WordPerfect, XOR
obfuscation), `Malformed(field)` — found on the error's `source()` chain,
whichever backend raised it (the PDF reader's `PdfError::Encrypted`, the
Office decryptor, iWork, WordPerfect). No message changed: callers that
matched text keep working, callers that match the type stop needing to.
`docling-serve` keeps the 422 and adds `"code"` to the error body
(`password_required` / `wrong_password` / `not_decryptable` /
`malformed_encryption`; on an async job's status and result too — never
on any other error; an encrypted PDF on the warm-pipeline route, which
answered 500, is this 422 now — and not in the upstream-shaped `/v1/convert/file`
envelope, which has no field for it); Python raises the `ConversionError`
subclasses `PasswordRequiredError`, `WrongPasswordError` and, for the
rest, `EncryptionError` (`docling_rs.exceptions`); the CLI message and exit
code are unchanged, as is the Node binding's error (a reason string).

### Conversion progress events (#783)

docling 2.139's `progress_callback` / `show_progress` (docling#4560) are
ported as `docling::progress`: a `ProgressCallback` on `DocumentConverter`
(`progress_callback`, every conversion; `convert_with_progress(source,
document_index, callback)` for one call and a batch position) receives
docling's event sequence per document — `document_started`, a
`page_completed` per PDF / image page with `page_no`, `success`,
`completed_pages`, `total_pages` (the page window's count; a page the
`document_timeout` cut off is reported with `success: false` once the walk
ended, so the count reaches the total, docling's `fail_unfinished_pages`),
`enrichment_progress` for the passes the converter runs on the assembled
document (picture description, picture OCR), and `document_completed` with
the result's status. The events come from the converting threads, as
docling's do; a panicking callback is caught and warned about, as docling
swallows a raising one. `docling_pdf::Pipeline::set_progress` is the
per-page hook underneath (`PageProgress` with the page number, on every
walk — buffered, streaming, images). Every surface carries it: `--progress`
/ `--no-progress` on the CLI (docling's flags; on by default when stderr is
a terminal, the pre-#783 batch dot progress replaced by the bars),
`progress_callback` / `show_progress` and the `docling_rs.progress` classes
in Python (docling's names, `convert_all` numbering `document_index`),
`progressCallback` on the async Node conversions and `showProgress`, a
`progress` object on serve's `GET /v1/status/{id}`, and
`docling_convert_with_progress` in the C ABI (events as JSON). Deliberate
differences: docling's `phase_started` events are not emitted — the
pipeline has no separate build / assemble / enrich phases to time — and the
per-page enrichments (code/formula, picture classification) run inside the
page walk, so they report through the page events, not as enrichment steps;
`show_progress` is a process-local display switch, not a `ConvertOptions`
field, so it has no serve / FFI option (serve reports the counts instead).

## 2. Format coverage

Conformance is measured against the latest **published** docling (installed from
PyPI; run via `scripts/conformance/conformance.sh <fmt>`), not the committed groundtruth
`.md` (which predates docling-core's current table serializer — see §4).
"Exact" = byte-for-byte.

Full sweep against docling **2.148.0** with docling-core **2.106.0**
(2026-11-07, `scripts/conformance/full_conformance.py` over the corpus mirrored
at docling `bcb3678` — Markdown byte-for-byte and JSON structurally, i.e. every
key and value but `origin`/`version`, re-encoded image bytes and this port's
documented additions (alt-text descriptions #676, rendered picture payloads
#858/#536); the files upstream itself cannot convert are left out, as are the
formats whose optional upstream dependency the sweep environment lacks (XBRL's
`arelle`, e-mail's `mail-parser`: their rows keep the 2.142.1 numbers); the six
LaTeX arXiv projects are checked file by file on their `main.tex`). The move
from 2.142.1 is mostly the inline whitespace contract (#997, § JSON):

| Format | Files | Markdown exact | JSON identical | Residual |
|---|---|---|---|---|
| DOCX | 38 | **38** | **38** | — (#527: a JPEG picture is stored as docling's `ImageRef.from_pil` PNG, re-encoded from libjpeg-exact pixels) |
| HTML | 32 | 31 | **32** | `html_nested_block_in_list_item`: a table inside a `<li>` is blank-line separated from its item (a divergence, § HTML; 32/32 after whitespace normalization) |
| PPTX | 8 | **8** | **8** | — (`pptx_chart`'s 3-D column chart now reads as upstream's `bar_chart`, docling#4811) |
| XLSX | 15 | **15** | **15** | — (`xlsx_10_place_in_cell`: the images placed in its cells, docling#5015, since #1019, its `#N/A` error cell since #1020; beyond that the PNG bytes PIL re-encodes) |
| CSV | 9 | **9** | **9** | — |
| Markdown | 11 | 10 | **11** | `escaped_characters`: its code spans keep their literal `& < >`, which upstream's HTML detour turns into entities (a divergence, § Markdown; the mirrored groundtruth keeps the literal lines) |
| DeepSeek-OCR Markdown | 3 | **3** | n/a | — (2.129's first-row-only `column_header` for HTML tables ported) |
| AsciiDoc | 5 | **5** | **5** | — (JSON: docling's `parents`/`indents` tree — title root, headings at their level, nested `list` groups, body-level captions) |
| LaTeX | 8 | **8** | 7 | docling 2.142's rewrite (docling#4817) ported in full — see § LaTeX. `1706.03762` differs in one `tabular` only: upstream writes a span into the cell's end offsets but leaves `row_span`/`col_span` at 1, here the two agree (a deliberate divergence). Upstream's 213 test-suite inputs (`tests/test_latex`) convert identically too, bar the same span fields (3) and, in Markdown only, the Python repr of a TikZ picture's `meta.code` docling-core prints (8) |
| JATS | 7 | 2 | **7** | the inline whitespace, image tables, `<term>`s, reference links (#1015), formatted table cells (#1016) and table footnotes (#1010) match; the Markdown of all seven matches after whitespace normalization — `pntd.0008301` / `pone.0234687` differ only in the blank lines upstream prints for a table's empty note items, the `elife-56337` copies in their one-run footnote (both divergences, § JATS) |
| USPTO | 9 | **9** | **9** | — (JSON: the SAX handlers' `level`/`parents` tree — everything under the title, `ABSTRACT`/`CLAIMS` as level-2 headings holding `paragraph` items, `<heading level>` nesting, table placeholders at `</table>` filled with `XmlTable` cells that list a spanning entry once per covered column; `tables_ipa20180000016` fails in upstream) |
| WebVTT | 4 | **4** | **4** | per-cue `source` (`kind: track`, start/end seconds, identifier, voice), inherited `formatting`, inline `WebVTT cue span` groups |
| ODF | 7 | **7** | 6 | `text_document_03` JSON: its picture's alt text also sits in a `description` annotation, which upstream's groundtruth leaves out (#1016 ported docling#4809's formatted table cells, and the mirrored HTML groundtruth is upstream's own again) |
| EPUB | 1 | **1** | **1** | — |
| Email (`.eml`) | 2 | **2** | **2** | — (`.msg` is a docling.rs extension) |
| iCalendar (`.ics`) | 4 | **4** | **4** | — (#1032, docling#4880; against upstream's committed groundtruth, `crates/docling/tests/suite/calendar.rs`) |
| iWork Pages | 3 | **3** | **3** | — (the password-protected and nested-dir fixtures fail in upstream) |
| iWork Keynote | 5 | **5** | **5** | — (against upstream's committed groundtruth; a live run of 2.142.1 here writes the float32 boxes widened, `708.9999694824219` for `709.0`) (docling#4330 + docling#4376, 2.130+; #466 — both container generations and the nested 2018 index; charts as classified pictures with their data table and title caption, the fixture built the way upstream's test builds it: the Numbers chart placed on the deck's table slide) |
| iWork Numbers | 4 | **4** | **4** | — (#1031, docling#4113 + docling#5087 + docling#5101; against upstream's committed groundtruth, `crates/docling/tests/suite/numbers.rs` — picture bytes aside, which we keep as stored; the three fixtures without groundtruth match a live run too) |
| EBCDIC | 3 | **3** | **3** | — |
| AFP (MO:DCA) | 1 | **1** | **1** | — (#1034, docling#4177; against upstream's committed groundtruth, `crates/docling/tests/suite/afp.rs`) |
| Box Note | 1 | **1** | **1** | — (#1033, against upstream's committed groundtruth) |
| DocLang (`.dclg`/`.dclx`) | 15 | 14 | **15** | read under DocLang 0.9's rules as 2.148 reads them (§ DocLang input); `equations`: an inline formula's Markdown keeps one space at each side where upstream's has two (15/15 after whitespace normalization) |

"The flat shape" is the JSON derived from the flat node stream — `text` for
docling's `paragraph`, `-` list markers, no heading nesting or inline groups —
that HTML, DOCX and PPTX left behind for the item tree (`DoclingDocument::tree`);
the same treatment is what closes those columns.

### Every extension, by category

Every format in docling's pipeline converts — plus a few docling lacks — to
Markdown, docling JSON, plain text, DocLang `.dclx`, LaTeX, HTML, Pandoc AST,
WebVTT and chunk records, with image extraction. The extension map
(`InputFormat::from_extension`, docling's `FormatToExtensions`) picks the
backend; a file that fails under its extension is checked by content and,
when it is evidently another format (an RTF or HTML file saved as `.doc`),
converted once more as that format with a warning.

| Category | Extensions | Notes |
|---|---|---|
| Text & markup | `.md` `.markdown` `.txt` `.text` `.qmd` `.rmd` · AsciiDoc `.adoc` `.asciidoc` `.asc` · HTML `.html` `.htm` `.xhtml` · MHTML `.mhtml` `.mht` · LaTeX `.tex` `.latex` · plain text & code `.py` `.rs` `.js` `.ts` `.java` `.go` `.sh` `.yaml` `.toml` `.ini` `.sql` `.log` … (and any extension whose bytes are text) · print files `.prn` · JSON Lines `.jsonl` `.ndjson` · bare `.xml` · feeds `.rss` `.atom` `.rdf` · Jupyter `.ipynb` | text encodings detected like docling's `decode_text`, or set with `encoding`; HTML JSON is docling's item tree; MHTML is parsed as MIME and routed through the HTML backend; LaTeX is docling's backend on a pylatexenc port — multi-file arXiv projects, math, `\newcommand`, `tabular`, figures; plain text (#671, MarkItDown's plain-text converter): a code extension is one code block in its language, anything else paragraphs; a `.json` that is not a `DoclingDocument`, a `.jsonl`, and an `.xml` matching no dialect (JATS/USPTO/XBRL/DocLang) are kept as one `json` / `xml` code block, never an empty document; Jupyter notebooks (#673, MarkItDown's `IpynbConverter`): markdown cells through the Markdown backend, code cells as code items in the kernel's language, raw cells as code blocks, `metadata.title` as the first heading when the notebook does not open with one, a UTF-8 BOM dropped, display formulas (`$$…$$`) as formula items, and the outputs under `ipynb_outputs` (`none` default; `text` keeps stream / `text/plain` results as code blocks and image outputs as pictures) |
| Word processing | DOCX `.docx` `.docm` `.dotx` `.dotm` · Word 97–2004 `.doc` `.dot` (Word 6/95 and Word for Windows 1.x/2.0 too) · OpenDocument `.odt` `.ott` `.fodt` · OpenOffice 1.x `.sxw` `.stw` `.sxg` · StarWriter `.sdw` `.sgl` `.vor` · T602 `.602` · AbiWord `.abw` `.zabw` `.awt` · WordPerfect `.wpd` `.wp` `.wp5` `.wp6` `.wpt` · MS Works `.wps` · EPUB `.epub` · Palm e-texts `.pdb` · Hancom Hangul `.hwp` `.hwpx` · classic Mac: MacWrite `.mw`, Word for Mac `.mcw`, ClarisWorks `.cwk` · Lotus Word Pro `.lwp` · RTF `.rtf` | DOCX JSON is docling's item tree; OMML equations come out as LaTeX; numbered headings keep their numbers |
| Presentations | PPTX `.pptx` `.pptm` `.potx` `.potm` `.ppsx` `.ppsm` · PowerPoint 97–2003 `.ppt` `.pot` `.pps` · OpenDocument `.odp` `.otp` `.fodp` · OpenOffice 1.x `.sxi` `.sti` · StarImpress/StarDraw `.sdd` `.sda` | PPTX JSON is docling's item tree — slides, lists, tables, pictures, charts, notes, comments |
| Diagrams | Visio `.vsdx` `.vsdm` · SVG `.svg` · CGM `.cgm` | Visio: pages as sections, shape text in reading order, connectors as a relations table; SVG rasterized into the image pipeline, or its `<text>` as paragraphs without ML; CGM (ISO 8632, binary and clear text) its text elements as paragraphs |
| Spreadsheets | XLSX `.xlsx` `.xlsm` `.xltx` `.xltm` · XLSB `.xlsb` · Excel 97–2004 `.xls` `.xlt` · OpenDocument `.ods` `.ots` `.fods` · OpenOffice 1.x `.sxc` `.stc` · CSV `.csv` `.tsv` · dBase `.dbf` · DIF `.dif` · SYLK `.slk` `.sylk` · Lotus 1-2-3 / Symphony `.wk1` `.wk2` `.wk3` `.wk4` `.wks` `.wrk` `.123` · Quattro Pro `.wq1` `.wq2` `.wb1` `.wb2` `.wb3` `.qpw` · MS Works `.xlr` · Kingsoft `.et` `.ett` `.eth` | cells print what Excel displays — `$12.50`, `10%`, `Feb-25`; merged cells keep their spans; chart sheets expose their chart |
| Apple iWork | Pages `.pages` · Numbers `.numbers` · Keynote `.key` | both container generations (`Index/*.iwa` and iWork '09 XML); all three mirror docling's readers, charts as classified pictures with their data table; Numbers sheets as pages of typed table grids |
| XML dialects | JATS / USPTO / XBRL / RIS (`.xml` `.nxml`, content-sniffed) · DocLang `.dclg` | XBRL converts without arelle: `dei` title, text blocks, numeric facts as docling's key-value graph, the taxonomy read offline from `xbrl_taxonomy`; RIS (#902, a docling.rs extension) is Austria's federal legal XML (`risdok`) — sections, headings, paragraphs with their formatting, lists, tables, running headers/footers as furniture |
| PDF & images | `.pdf` · `.png` `.jpg` `.jpeg` `.tif` `.tiff` `.bmp` `.webp` `.gif` · HEIC/HEIF `.heic` `.heif` (`--features heif`) · METS/GBS scan packages `.tar.gz` · DjVu `.djvu` `.djv` | pure-Rust text layer, page renderer and scan raster, ONNX layout + TableFormer + OCR (`docling-pdf`; no native PDF library in the default build, `pdfium` is an opt-in feature — [`docs/PDF_CONFORMANCE.md`](./PDF_CONFORMANCE.md)); JPEG 2000 and JBIG2 decode in Rust (#598, #811 — `/JBIG2Globals` included; an image no decoder can read draws a mid-gray placeholder, a stencil too); DjVu reads its hidden text layer without models and falls back to OCR with them |
| docling native | docling JSON `.json` · DocTags `.doctags` `.dt` · DCLX `.dclx` | DocTags — the token markup docling's VLMs emit — read through docling-core's tolerant parser, the one the VLM pipeline uses |
| Mainframe data | EBCDIC `.ebc` `.ebcdic` · AFP `.afp` | fixed-width records decoded through a COBOL copybook (docling's `EbcdicLayout`): `ebcdic_layout`, or a `<stem>.layout.json` sidecar; AFP / MO:DCA print streams (#1034, docling#4177): each page a `chapter` group of its PTOCA text lines, decoded as cp500 (code-page resources are not resolved), images / graphics / barcodes / overlays / page segments skipped with a warning; `application/vnd.ibm.modcap` and a nameless input opening with a structured-field introducer are recognized, `page_range` keeps the page numbers |
| Notes | Box Note `.boxnote` (and a `.json` with a top-level `doc` node) | #1033, docling's `BoxNoteDocumentBackend` (docling#3722, #5160, #5119) ported: the ProseMirror `doc` tree — headings (level 1 the title), paragraphs as one text or an `inline` group of styled runs (`http`/`https`/`mailto` links only), bullet / ordered / check lists with the item text from its leading paragraph, code blocks, tables with clamped spans and rich cells, images as captioned pictures; the JSON is docling's item tree; a legacy `atext`/`pool` note is rejected as unsupported, as upstream does |
| Email, calendars & subtitles | `.eml` · Outlook `.msg` · iCalendar `.ics` `.ical` `.icalendar` `.ifb` · WebVTT `.vtt` | `.msg` is projected onto RFC 822 (same output as the `.eml`); `list_attachments` names the attachments, `email_attachments()` converts them; `cid:` images embed under `image_sources=embedded`; iCalendar (#1032, docling#4880): each event / task / journal entry / free-busy block a heading with `Field: value` paragraphs, its description, and attendee / reminder / attachment lists (inline payloads never decoded), the calendar name as title; `text/calendar` and a nameless `BEGIN:VCALENDAR` input are recognized, folds are joined on the bytes before decoding, `encoding` applies |
| Audio | `.wav` `.mp3` `.mpga` `.m4a` `.aac` `.ogg` `.flac` | Rust all the way down: symphonia decodes, Whisper tiny (ONNX, docling's ASR defaults) or the Parakeet TDT 0.6B v3 preset transcribes; `[time: start-end] text` paragraphs, the timing as the JSON item's track, `--to vtt` for subtitles; language auto-detected or `asr_lang`. Ogg Opus falls back to the `ffmpeg` binary |
| Video | `.mp4` `.avi` `.mov` `.mkv` `.webm` `.mpeg` `.mpg` | the audio track transcribed the same way; with the `ffmpeg` binary on `PATH` (`DOCLING_FFMPEG`), `video_frames` scene-change frames (8 by default, `all` for every cut) interleave as `[time: <ts>]` pictures, tuned by the `video_*` options. AVI needs ffmpeg; without it a video is its transcript |

### Declarative formats — pure Rust, no models

#### Markdown

**Backend:** `markdown.rs` (pulldown-cmark)\
**Status:** vs docling 2.148.0 / docling-core 2.106.0 (the bcb3678 mirror, #996/#997): **10/11 exact** Markdown, **JSON structurally identical on 11/11** — the runs keep marko's raw fragments (`***both***.`, no invented space); `escaped_characters`' Markdown is the divergence below. Before (2.142.1): **11/11 exact** Markdown, **JSON structurally identical on 10/11** — `escaped_characters`' code spans keep their literal `&amp; &lt;` (below)

- a leading YAML frontmatter block is dropped (#1022, docling#4596, 2.149): a `---` line, a non-blank next line, up to a `---` or `...` line, when the block is a YAML *mapping* — `---\ntitle: T\n---\n\n# H` no longer reads as a `## title: T` setext heading. A `---` followed by a blank line stays a thematic break (as in Pandoc), and a block that is prose, a list, empty or broken YAML stays content. Upstream asks `yaml.safe_load`; the port checks the mapping shape itself (entries at one indentation, `- ` items and indented continuations under them, balanced flow brackets and quotes) rather than link a YAML parser into the library, so an exotic mapping — a `?` complex key, tab indentation — is kept as content
- #1040, a divergence: a document with a raw HTML block takes upstream's detour through the HTML backend, where docling writes each paragraph's `inline` group as `<span class='inline-group'>` — two neighbouring paragraphs are two adjacent spans, which its HTML backend reads back as **one** paragraph (`**2.1** First.**2.2** Second.`, one `inline` group of four runs). The detour writes a block-level `inline` group as a `<p>` instead, so the paragraphs after an HTML comment (a `<!-- markdownlint-disable -->` line, say) or a `<div>` stay one group each, as without the block
- `md_tree.rs` rebuilds marko's element tree from a second pulldown parse with the GFM extension as docling configures it (docling#4488, 2.135: tables, `~~strikethrough~~` by marko's own rule — two tildes, none inside or around, no flanking, so `~x~` stays text — and bare URLs) and ports `MarkdownDocumentBackend._iterate_elements` call for call:
  - a heading or list item is created lazily on its first run (`title`/`section_header` of level − 1; `list_item` with `enumerated` and a `{start + count}.` marker)
  - one whose content has no run — an alt-less image, inline HTML — is created with empty text (#864, docling#4870, 2.141): an image-only list item holds its picture and sub-list, and the next block no longer takes the item's place (an image-only heading used to become the following paragraph's text, an image-only list item to vanish while its sub-list repeated)
  - a paragraph or heading of several runs opens an `inline` group of one `text` per run with `formatting` (emphasis/strong) and `hyperlink` (AnyUrl-normalized)
  - a backslash escape is part of the text run around it (#1022, docling#4991, 2.149): marko's neighbouring `RawText` and escaped `Literal` nodes are merged into one run, so `snake\_case here` is one `text`, `# Head \# x` a `title` `Head # x` and `- item with \* star` a `list_item` — no `inline` group of `snake` / `_` / `case here` (or an empty title plus a group) any more; each piece keeps its `&` as an entity, so an escape still protects the entity around it (`a \&amp; b` stays `a &amp; b`, `R&D &amp; \_x &#169;` reads `R&D & _x ©`)
  - a code span is a `code` item (the container's own text when it is the only run)
  - a fenced block a `code` with `detect_code_language(hint)`
  - an image a `picture` whose `"title"` is a body-level caption and whose alt runs follow as text
  - a nested list hangs off the outer list's last item
  - a GFM table (one that may interrupt a paragraph, as docling's `_GfmParagraph` lets it) sits under the current parent — a list group, the body — with row 0 the column header, rows cut/padded to the header's width and each cell's text its stripped plain text
  - a formatted cell (emphasis, code span, link, image, strikethrough) a `RichTableCell` whose runs are items of an `inline` group under a `rich_cell_group_{table}_{col}_{row}` group of the table (`_parse_gfm_table` / `_iterate_cell_inline`; the flat Markdown prints the cell with its formatting — `**bold**`, `` `code` ``, `~~struck~~`, links — as docling's serializer does)
  - `~~x~~` a `strikethrough` run
  - hard/soft breaks merge the next run into the previous text (`\n` / space) within one paragraph only (docling#4525)
- A document with a raw HTML block takes upstream's detour for its JSON: each block a `code` item of its source between docling's two markers, the tree exported to HTML (`serialize_markdown_for_html`: every picture an `<img>` keyed by its `self_ref`, resolved back to its image), the markers cut with the `<pre><code>` around them, and the HTML backend's tree over the result (no furniture inference, no `<title>` item) — so the HTML is parsed (a `<table>` a table, everything nested under its heading) as upstream has it. The five files that take it used to keep the flat export's JSON. The flat Markdown path stays the source of the Markdown, which agrees with upstream on them.
- **Divergence:** in that HTML only a raw HTML block's code item is written raw; every other code item is escaped — as docling-core 2.106's HTML export escapes all code (docling-core#894; the HTML export here follows it, code and formula source included, #1011). docling-core 2.103 wrote all code raw, so upstream's detour reads a code span's text as markup — an inline `` `<img>` `` becomes a picture and its text is lost, `` `&amp;` `` decodes to `&` (`escaped_characters`). Here a code span stays the text the source has
- Upstream bug, not reproduced: docling 2.142.1 with docling-core **2.104.0** (what a fresh `pip install docling` resolves) prints every raw HTML block as escaped text (`&lt;table&gt;…`) — 2.104's HTML serializer escapes a code item's text, so the detour's markup stays text. Its lock (2.103.1) and its committed groundtruth parse the markup, as this port does
- re-verified vs docling 2.129
- docling#4202: every text-format input (Markdown, CSV, AsciiDoc, WebVTT, LaTeX, JATS/USPTO XML, …) decodes like docling's `decode_text`:
  - a UTF-32/UTF-8/UTF-16 BOM settles the encoding and is dropped; bytes it cannot decode are an error naming the BOM-detected encoding (`'utf-8-sig'`, `'utf-16'`, `'utf-32'`), never a windows-1252 fallback reading the mark as `ï»¿` (docling#4949, #1029)
  - otherwise strict UTF-8
  - otherwise — #675, beyond docling — a statistical guess (`chardetng`, Firefox's detector; what MarkItDown asks `charset_normalizer` for) taken when it names a non-Latin encoding (Shift_JIS / EUC-JP / GBK / Big5 / EUC-KR / KOI8-R / windows-1251 / Greek / Hebrew / Arabic / Thai) that decodes the bytes cleanly into letters, with a warning naming it (MarkItDown's cp932 `test_mskanji.csv`, a KOI8-R or GBK file read as written)
  - otherwise windows-1252 with a warning when under half of the non-ASCII bytes form UTF-8 sequences (a Latin code page other than 1252 is never guessed — its bytes read as text under 1252 as they always did; name it with `encoding`)
  - and an error for *damaged* UTF-8 (≥ 50%) or a byte cp1252 leaves undefined, where an undeclared non-UTF-8 file used to fail outright
- docling's `TextBackendOptions.encoding` (`MarkdownBackendOptions(encoding="shift_jis")`) is the `encoding` option on every surface — `DocumentConverter::encoding` / `SourceDocument::with_encoding`, `--encoding`, serve's `encoding` field/param, the Python kwarg, the Node option, the FFI key — a WHATWG label or Python codec name decoded strictly (every Python alias of a Unicode codec too — `u8`, `utf_16_le`, `utf-16-be`, `utf-32`, `utf_32_be`, …, a leading mark dropped under each, docling#4849, #1029), an undecodable byte or unknown label failing the conversion like docling's `DocumentLoadError`
- #319 (docling 2.122's docling#3817): tables written without edge pipes (`Region | Q1` over `--- | ---`) are normalized up front so pulldown parses them like the canonical spelling
- a leading UTF-8 BOM no longer hides the first heading (docling#4109)
- #466 (docling#4019, 2.126; `line_breaks.md` mirrored, exact in Markdown and JSON): a line break inside a paragraph, heading or list item is kept:
  - a hard break (two trailing spaces or a backslash) joins the runs around it with `\n`, GFM `  \n` once serialized
  - a soft one with a space
  - two runs of the same formatting merge into one item across the break (`**Bold A**` ⏎ `**Bold B**` → `**Bold A Bold B**`)
  - differently formatted ones stay separate items with the `\n` on the second
- docling#4313/#4371: a `\|` and every character-reference spelling of `|` (`&#x7C;`, `&verbar;`) is cell content — carried as `&#124;` until the row is split, so it neither opens a table nor adds a column, and in prose stays a literal pipe (`escaped_pipes.md` is our own fixture)
- a list item's blocks after its text — another paragraph, a code block, a quote, a table, raw HTML — follow the item in the flat Markdown as the blocks they are (they were dropped); the JSON already held them as the item's children. **Divergence (whitespace only):** they are blank-line separated from the item and the next one, where docling prints them on the very next line — `1. one⏎extra para` reads back as one item `one extra para` in any CommonMark renderer, so the blank lines keep the paragraph a paragraph; the list numbering goes on after them
- docling#4318 (2.130.0-33): a table cell's text is its plain text stripped as a whole, so `**C** Cadre` keeps its space.
- **Beyond docling**: a `$$…$$` paragraph of its own (one line or several — a notebook's or a README's equation) is a `formula` item — `$$latex$$` in Markdown, a `formula` text item in JSON, MathML in HTML — while inline `$x$`, a `$$…$$` inside prose or a link (`inline_and_formatting.md`'s `[$$E=mc^2$$](…)`) and a price (`$5`) stay the text they were, run for run, so the corpus output is unchanged.
- #773 (docling#4791, 2.139): an autolink (`<https://…>`, `<me@x.y>` as a `mailto:` link) and a GFM bare URL (`https://…`, `www.…` linked as `http://www.…`, an e-mail address) keep their hyperlink in headings, paragraphs and list items — marko's `Url` rule ported (the lookbehind, the domain check, the single trailing-punctuation trim, a URL whose trim leaves it touching emphasis markup not linked), and a link in a table cell prints as `[label](url)` like docling 2.140's
- `tests/data/md/upstream/` matches docling 2.140.0's JSON 3/3 and Markdown 2/3 (a link inside emphasis is the older difference: docling writes `*see* [*x*](u) *now*`, the flat path `*see [x](u) now*`).
- #760 (docling#4777, 2.139): a paragraph that is one image is a picture in every export, as the item tree has it — its title the caption (created ahead of it), its alt text a paragraph after it, its pixels only when the image policy resolves the destination — where the flat path used to keep an unresolved image as `![alt](dest)` text and caption a resolved one with its alt text
- so a document with a raw HTML block (whose JSON still comes from the flat nodes) keeps its pictures, in order, and a picture in a table cell prints as `title  <!-- image -->`
- An image inside running text stays `![alt](dest)`.

#### CSV

**Backend:** `csv.rs` (`csv` crate)\
**Status:** **9/9 exact**, **JSON 9/9 structurally identical** (vs docling 2.142.1)

- docling#4260: Python's `strict=True` quoting errors — a closing quote followed by text, a quote left open at EOF — are load errors here too, checked up front since the `csv` crate is lenient about both
- a ragged file's short rows hold only the cells they have, as docling adds one `TableCell` per value read; the grid's missing slots are its empty fillers (an empty `TableCell` was recorded for each)
- `.tsv` routes here too (#208 — the delimiter sniffing already covers tabs; a docling.rs extension, upstream accepts only `.csv`)
- #319: a quoted field spanning lines no longer defeats delimiter sniffing (4 KiB fallback sample, docling#3985) and an Excel-style leading BOM stays out of the first header cell (docling#4098)
- docling#4896 (docling `main` after 2.142.1): a header holding two candidate delimiters — a comma inside a semicolon file's `Weight (kg, net)`, the colons of a comma file's `Time (hh:mm:ss)` — is sniffed from its occurrences outside brackets; docling re-sniffs the following rows with `csv.Sniffer` instead, and this count-based sniff reaches the same split on all fifteen of its test cases
- docling#5200 (#1026, docling `main`): Excel's `sep=<char>` first line (after blank lines, behind a BOM) names the delimiter and is not a row — `sep=;` over decimal commas no longer reads as a `sep=` row; the strict-quoting check starts after it, and a named delimiter outside `, ; \t | :` is docling's load error

#### HTML

**Backend:** `html.rs` (scraper/html5ever)\
**Status:** vs docling 2.148.0 / docling-core 2.106.0 (the bcb3678 mirror, #996/#997): Markdown **31/32 exact**, 32/32 after whitespace normalization, **JSON 32/32 structurally identical** — the inline whitespace contract (`**x**.`, `H2O`) in both walks; `html_nested_block_in_list_item` is the divergence below. Before, vs docling 2.142.1 (docling-core 2.103.1): Markdown **30/32 exact**, 31/32 after whitespace normalization — `html_nested_block_in_list_item` differs by the blank lines around a table inside a `<li>` (below), `wiki_duck` by two link destinations percent-escaped as docling-core 2.104 writes them (`Houad_%28evn%29`, #860; the 2.103.1 that docling 2.142.1 locks leaves the parentheses); **JSON: structurally identical to docling's on 32/32** (every key and value but `origin`/`version`)

- #903 / #904 (extension — docling has nothing of the kind): the item tree keeps the source's blocks. `ItemTree::blocks` enumerates, in document order, every element that produced an item, every non-inline element with text of its own and every empty `<p>`/heading/`<li>`/cell, as a `SourceBlock` — tag, `id`, `class`, the element path (`html/body[1]/div[3]/p[1]`), its own whitespace-normalized text, the items made from it — and `TreeItem::block` points each item (an inline group's runs included) back at its block. A block that produced no item says why (`BlockOmission`: hidden, empty, flattened into a table's grid, merged into an enclosing block's item). `ItemTree::source_text([i, j])` reads selected blocks on their own terms, one `\n` between them and no serializer decoration — what `export_to_text()`, docling's plain-text serialization, cannot give (it keeps the blank line that separates its blocks). Every text node of the source belongs to exactly one block. Rust API only: the JSON export is unchanged (docling's schema has no slot for it)
  - #910: a block carries every attribute of its element (`attrs`), and the containers that are no blocks but carry an `id` or a `class` — the `<div id="art_23">` an article's paragraphs sit in — are `ItemTree::containers` (`SourceElement`: tag, id, class, attrs, path). `path_of_id("art_23")` resolves an identity to its path and `blocks_under(path)` to the blocks inside it, so an excerpt selected by a container's id is selected without re-parsing the source
  - #911: a block's inline elements (`inlines`: `SourceInline` — tag, attrs, path, the byte range of its text in the block's `text`) keep their identity, and `SourceBlock::text_excluding(|i| i.has_class("sr-only"))` / `ItemTree::source_text_excluding(blocks, …)` read the text without the ones the caller leaves out — a screen-reader alternative, say; the caller chooses, nothing is inferred from a class. `SourceTextBuilder` is the normalizer behind it, for any backend that enumerates blocks
  - #912: a page nested past `DOCLING_RS_MAX_HTML_DEPTH` is read as flat text, with no item tree and no source blocks — and the document says so: a `Notice` (`NoticeKind::DepthLimit { variable, limit }` and a one-line message) in `DoclingDocument::notices`, which the converter reports as a `partial_success` error item (the CLI warns, serve answers `X-Docling-Status: partial_success` with the message in `X-Docling-Errors`). The flattened text stays, as before
- docling#4287: a zero `colspan`/`rowspan` defaults to 1 like a missing one, so the cell no longer drops out of the grid; spans read python-style — the leading digits when the attribute starts with one, `"2px"` → 2
- docling 2.142's table width (`table_width`, docling#4414 / #4527): the declared width is cut after the last column a cell starts in, a rowspan shifting the cells of the rows it reaches widens the table to keep them, and a cell's spans are clamped to the table (`example_08`'s six-row span over five rows)
- the source text of a paragraph, inline run, code item and `<pre>` block is the item's `orig`, its `text` the cleaned one (docling#4434: `–`, curly quotes and `…` kept in `orig`)
- a non-`<li>` child of a `<ul>`/`<ol>` — invalid, common in CMS output — is kept in document order (docling#4440): content before the first item precedes the list, a nested list is the preceding item's sub-list, anything else closes the list and the items after it open a new one that continues the numbering. The Markdown path used to drop it (`<ul><li>a</li><ul><li>b</li></ul></ul>` lost `b`)
- a `<li>` without text that holds a table or an image is an empty list item holding it, so it and the numbering stay (docling#4617; `wiki_duck`'s two footer badges)
- **Divergence (Markdown):** a table inside a `<li>` is a table after the item, blank-line separated; docling prints its grid right below the item line. The Markdown path used to flatten the table into the item's text (`1. Lead text A B`), losing the grid in every format the flat nodes feed (DocLang, LaTeX, chunks); the item line and the grid are the same, only the blank lines differ
- `code_language` is docling's `detect_code_language`, one port (`code_language.rs`) for HTML, Markdown, DOCX and AsciiDoc: a hint (`language-*` class, fence token, `[source,lang]`) with its aliases (`py`, `js`, `yml`, …), else the content markers — shebang, PHP, HTML, Dockerfile, C/C++, Go, Rust, Python, Java, C#, SQL, TypeScript, JavaScript, JSON (`html_code_snippets`' `from … import` block is `Python`). The HTML and DOCX backends each had a partial port; DOCX now also counts a detected language as a code signal, as upstream does
- #371: non-UTF-8 input decodes in BeautifulSoup's `UnicodeDammit` order — BOM, then the declared XML/`<meta charset>`/`http-equiv` encoding (bs4's search windows, WHATWG labels via `encoding_rs`, so `iso-8859-1` reads as windows-1252 like a browser), then strict UTF-8, then windows-1252 — instead of failing the UTF-8 check
- bs4's optional chardet/charset_normalizer guess between declaration and fallback is the one step not reproduced
- #364: docling#4050's `<figure>` dispatch — every child but the `<figcaption>` is walked, a figure's pictures take the figcaption as caption, a figure without a picture attaches the figcaption to its leading table or emits it as a standalone caption item (`wiki_duck`'s `[Mallard duckling preening](/wiki/Mallard)`)
- pictures folded into a list item print after the item line with plain newlines, the GFM hard-break rule applying to the item's own text only
- `wiki_duck` — rich table cells, caption run spacing, indicator images, `<footer>` furniture all match docling 2.112
- #284: an *unclosed* inline tag (`<a name=…>`, `<b>`, `<font>` — endemic in legacy authoring-tool HTML) legally swallows subsequent blocks under html5ever's spec parsing, where Python's parser recovers by reparenting — inline wrappers hiding structured blocks (tables, lists, headings, code, figures) are now block-walked so the structure surfaces
- pure text containers under a well-formed `<a href>` keep rendering as links
- `aria-hidden` subtrees are dropped like docling's `_is_invisible_tag` does (Wikipedia's decorative logo icon and its sticky-header duplicates), and an anchor wrapping several images hangs its href on each captioned picture
- #382: docling#4216's header flags — a `<tr>` of `<th>`s that each span several rows is a pivot table's *row*-header row, so its cells are `row_header` and not `column_header` (they label the rows they span into, and flagging them as column headers made docling-core 2.96 fold the first data row into the Markdown header), and a lone `<th>` beside `<td>` data is a row header too
- all 66 cells of `example_08` now match upstream's regenerated groundtruth flag for flag.
- The flat node stream that Markdown / DocLang / LaTeX render is not the shape upstream's *JSON* has, so `html_tree.rs` ports `HTMLDocumentBackend`'s document construction call-for-call into a docling-core `ItemTree` (`DoclingDocument::tree`) that the JSON export serializes instead of deriving a tree from the nodes:
  - everything after a heading nests under it (`h1` = title, `h2`–`h6` = `section_header` level `n-1`, `header-N` section groups filling a skipped level)
  - a paragraph is docling's annotated parts (one per source text node with its ancestor `<b>`/`<i>`/`<u>`/`<s>`/`<sub>`/`<sup>`/`<code>` formatting and `<a href>`), adjacent same-annotation parts merged, two or more parts an `inline` group of one `text`/`code` item each carrying `formatting` and `hyperlink`, one part a lone item
  - `<br>` splits (`\n` for one, a new paragraph for two)
  - a mixed-formatting `<li>` is an empty `list_item` over an inline group, a `<dt>` a bold item whose `<dd>`s form a `descriptions` list group under it, `<ul>` directly inside `<ul>` its own nested group
  - a rich table cell's items are re-parented under a `rich_cell_group_{tables}_{col}_{row}` group of the table (named with upstream's lazily advanced column, i.e. the *previous* cell's), the cell carrying `ref` to it, cell text docling's raw `get_text`, declared spans unclamped
  - a picture's caption is a `caption` item under its picture, so it stays in the picture's section or rich table cell (docling#5037, #1023 — it was a body-level item before)
  - a `<pre>`'s sibling `<code>` children are one code item, not code runs of an inline group (docling#4998, #1023)
  - a table's `<tfoot>` rows are kept and come after every other row group wherever the element sits — HTML 4 put it before `<tbody>` — in the JSON and the Markdown alike (docling#4323, #1023; the JSON used to drop them and the Markdown kept source order)
  - the `<title>`, everything before the first heading and a `<footer>` (a `section` group) are `furniture`
  - `<details>` a `section` group
  - `field_value` items carry `kind`
  - items are numbered in upstream's creation order.
- Markdown / DocLang / LaTeX / chunks are untouched (they keep reading the nodes)
- #466 (docling 2.130+): a `<table>`'s own `<caption>` (looked up non-recursively) is a `caption` item created ahead of the table under the current parent and linked from the table's `captions` (`html_rich_table_cells`: four captions, JSON structurally identical to upstream's regenerated groundtruth)
- text nodes collapse every whitespace run to one space on extraction (docling#3973, so a table flattened into an ancestor's cell reads `I II III IV`, `table_06`)
- a GFM task-list item — `<li><input type=checkbox checked> done</li>` with no block content or custom checkbox markup — is one `checkbox_selected`/`checkbox_unselected` item carrying that text under the list group, not a list item plus an empty checkbox (docling#4401)
- Ported from docling 2.139 and checked document by document against 2.140 (`tests/data/html/upstream`, `html_upstream`):
  - a `<br>` in a heading, list item, `<dt>`/`<dd>`, caption or `<pre>` is a line break, not a leftover U+E000, and a checkbox's `<br>` a space (docling#4675, #769)
  - a formatting or a link is the whole heading's or caption's only when all of its text has it (docling#4670, #770)
  - an item's inline CSS string `list-style-type` (`<li style="list-style-type: '3. ';">`, how docling's HTML export writes an original marker) is its marker (docling#4669), so a marker survives the Markdown backend's HTML detour
  - `<ol start>` is read by the HTML integer rules — ` 3`, `+3`, `3.5` start at 3, `-5`, `²` or a value above 2³¹−1 at 1 (docling#4713, #771)
  - `--use-web-browser` keeps the text of a page that declares a non-UTF-8 charset (docling#4468, #772)

#### AsciiDoc

**Backend:** `asciidoc.rs` (regex)\
**Status:** **5/5 exact vs docling 2.142.1** (re-measured for #863; the content of the committed 2.140 groundtruth is unchanged in 2.142.1, which only spells its tables uncompacted), **JSON structurally identical on 5/5**, DocLang 2/2 exact (`asciidoc_04`'s archive refreshed)

- the 42 documents upstream's `test_backend_asciidoc.py` converts are mirrored in `crates/docling/tests/data/asciidoc/upstream/` with docling's Markdown and JSON — all re-run through 2.142.1, where only the two comment documents moved (their JSON is 2.142.1's) — and match **42/42** in both, as do our 15 `edge_*` probes.
- **comments (#863, docling#4663, 2.141)**: a run of consecutive `//` lines is one comment and everything between two `////` lines another (an unclosed `////` runs to the end, as Asciidoctor reads it); each becomes a `text` item on the **notes** layer under the open list item, else under the current section, without ending the list or the paragraph it sits in — a comment between two lines of a paragraph lands before the paragraph. A comment block is checked ahead of every other construct (a `----` inside `////` is comment text, not a listing), and the lines inside an example / sidebar / quote / open / passthrough block now run through the same loop, so a comment there is one too. Exports leave the notes layer out by default, so the default Markdown is unchanged. In the flat outputs (Markdown / LaTeX / DocLang with the notes layer included) a comment met inside a list is printed after the list, not between its items — a node there would split the list the default Markdown prints; the JSON keeps it under its item.
- docling `main` after 2.142.1: only exactly two slashes start a line comment — `///` and `//// text` are body text — and a comment block opened by four or more slashes closes only on an identical line (docling#4394); a symmetric title drops its closing marks, `== Intro ==` → `Intro`, while marks of another length stay (`=== Details ==`, docling#4938)
- **docling 2.149 (#1024, #1025)** — checked item for item against docling 2.149.0 on all 114 AsciiDoc inputs of upstream's `test_backend_asciidoc.py`, both JSON and Markdown:
  - tables (docling#5010): the first line's cell count — an `N*` cell counting N times, a span its width — is the column count, and the cells fill rows of that width, so a row may be written over several lines (Asciidoctor's own one-cell-per-line example was one row per line); `N+`, `.N+` and `N.M+` are spans placed at the first free position, a row span cut at the last row (they were dropped); `N*` writes the cell N times, a line continuing its text going to every copy. A span or factor counts at most 1000 — a crafted `999999999*|x` is not materialized. The Markdown repeats a spanning cell's text across what it covers, as docling-core does
  - links (docling#5115): `url[text]`, `link:target[text]`, `mailto:addr[text]`, `<url>` and bare URLs (`http(s)`, `file`, `ftp`, `irc`; at a URL start only, a bare one ending before trailing punctuation) are text runs with docling's `hyperlink` (AnyUrl/Path-normalized) in an `inline` group — they were printed verbatim (`See https://example.com[Example site].`). Named attributes follow the link text (quoted when it holds a comma, `\[` `\]` unescaped, a trailing `^` dropped); no text shows the target; a link inside a link's text stays text of the outer one; a code run keeps its link
  - block titles (docling#5144, docling#4391): a block title is one line — the next line starts the paragraph it titles (it used to run into the title: `**Procedure Do the thing.**`) — and is parsed like a paragraph: a bold paragraph keeps its links (an `inline` group of bold runs) and drops its marks when no run stands out; a picture's, table's or code block's caption joins the runs and keeps a hyperlink only when every non-blank run shares it
  - a trailing ` +` is a hard line break (docling#5175): `Roses are red, +` → `Roses are red,\nviolets are blue.`, Markdown `  \n`
  - `[start=4]` (or `[loweralpha,start=4]`, `[arabic, start="2"]`) numbers the next ordered list from 4 (docling#5180), kept past blank lines and a block title until the block it applies to
- The parser follows `_parse` / `_iter_blocks` step for step and builds docling's item tree alongside the flat nodes, mirroring the `parents` / `indents` / `list_kinds` bookkeeping:
  - the title is the root
  - a heading is parented to the nearest present ancestor level (docling 2.127+) and clears the deeper ones
  - a list opens a `list` group at the next level
- **lists (#774)**:
  - a nested list (a deeper indent, or a dotted `..` / starred `**` marker, which carries its depth) is a group under the item above it, not a sibling of the items, so the numbering runs on (`. a` / `.. b` / `. c` → `1.`, `1.`, `2.`; docling#4721)
  - an item of another marker kind nests the same way and the next item of the outer kind returns to its list (`* a` / `. b` / `* c`, or `*` beside `-`, as Asciidoctor nests by marker; docling#4822)
  - any other line ends every level of the list, so the text after a nested list is not part of it (docling#4721)
  - a lettered marker is one letter (`a.`, `B.`), so a line starting with an abbreviation (`Dr. Smith…`, `Fig. 2 …`) stays text (docling#4827)
  - the flat list items take their depth and numbers from that tree — an explicit `12.` printed verbatim, any other enumerated item numbered by its position in its group when the group's first item is enumerated, else a bullet, as docling-core's list serializer does
- **inline marks (#775)** — a paragraph with `*bold*`, `_italic_` or `` `mono` `` marks (doubled anywhere, single ones only at word boundaries, so `snake_case_name` and `2*3*4` stay plain; `*_both_*` bold and italic) is an `inline` group of one text item per run with its formatting
  - a monospace run a `code` item (docling#4824)
  - a `Node::InlineGroup` in the flat model so DocLang writes the styled runs
- **tables and delimiters (#776)**:
  - inside `|===` a blank line only separates rows and a line without a leading `|` continues the cell above (`.md or (also .markdown)`; docling#4761)
  - a row may hold one cell (the vertical layout) and `\|` is a pipe in its cell (docling#4431, docling#4378)
  - a short row keeps only the cells it has
  - a table whose rows never completed emits no empty grid (docling#4300)
  - an example / sidebar / quote / open / passthrough delimiter (`====`, `****`, `____`, `--`, `++++`) is consumed — with a closer its content is read as ordinary lines followed by a paragraph break, without one it only breaks the paragraph and never reaches the text (docling#4662)
- `//` comments and `////` comment blocks, block attributes and anchors (`[NOTE]`, `[cols=…]`, `[[id]]`) and entry attributes (`:attr:`) are no content
- a block title no floating item claims is a **bold paragraph** at the next blank line or at the end (a list group having no caption slot either; it used to swallow every line after it; docling#4431)
- a block title in front of a picture, table or literal block is that item's caption — for a code item now in its `captions`, where the tree used to list it as a child.
- A `....` literal or `----` listing block is a `code` item, a `[source,lang]` line before the listing sets `code_language` (docling#4400)
- a lone `+` keeps the next literal block or image inside the open item (printed after the item line, indented to its depth, like the pictures the HTML backend folds into an `<li>` — carried in the item's own text, so the JSON has no separate `picture`/`code` item for it and an in-list picture keeps no fetched bytes)
- a section header flushes the pending paragraph into the section it was written in (docling#4428)
- image loading sits behind the converter's `fetch_images` (docling#4156): by default an `image::` macro yields a picture with **no** `ImageRef`.
- Where docling's parser is quirky the output still follows it, as 14 `edge_*` probes in the same directory check (Markdown and JSON 14/14):
  - the line that ends a bare (no `|===`) table is consumed with it, as docling's `elif in_table` arm does
  - a paragraph still pending when a list opens is flushed into the list group and printed as one more line of the list after the item before it — an inline-formatted one glued onto that item's line, as docling-core's list serializer joins a part from an inline group (a `Node::ListText` in the flat model; every serializer but Markdown reads it as a paragraph)
  - lines split like Python's `split("\n")`, so an unterminated literal block keeps the empty line after the file's last newline.

#### DeepSeek-OCR Markdown

**Backend:** `deepseek.rs`\
**Status:** **3/3 exact vs docling 2.129**

- docling's `_parse_table_html` flags a `<th>` as `column_header` on the first row only, so a two-row `<thead>` keeps its second row in the body (spanning cells repeated, numeric columns left-aligned by that text row) where the HTML backend folds both into one ` - `-joined header
- the annotated table's header flags are trimmed to row 0 after the HTML parse (`deepseek_example` groundtruth refreshed)
- auto-detected VLM-token variant
- Unlimited-OCR grounding output (#322 / docling#3944) normalizes into this shape and shares the parser

#### Chandra layout HTML

**Backend:** `chandra.rs` (#322)\
**Status:** docling 2.123–2.125's `parse_chandra_html` semantics on our node model

- `data-bbox`/`data-label` divs → headings, text, span-aware tables (Form-held tables included, docling#4135), lists, figures, formulas, code, page furniture
- `<br>` is spacing (docling#4092)
- 0–1000 boxes → DocLang 0–511 `Located` provenance

#### XLSX

**Backend:** `xlsx.rs` (calamine)\
**Status:** vs docling 2.142.1 (the mirrored corpus is 2.142.1's): **13/14 exact** Markdown, **JSON structurally identical for 13/14** — `xlsx_02_sample_sales_data` was the 20 date cells PR #4628's number formats print (below); that PR has since merged upstream (`25cdd57`) and the mirrored groundtruth's cells are now the ones our port prints (#1020; the count was measured against 2.142.1 and is not re-run here)

- docling#4812 (2.142): a chart's data table holds the series of **every plot** of a combination chart (a column + line chart kept only its columns), the top-left cell is the title of its category axis (`col-1`, `Year` in `xlsx_01`/`_03`/`_04`), a date is ISO 8601 (`2012-01-01`, with a time `2012-01-01T12:30:00`) and every cell its value, not its displayed text; a cell-linked chart or axis title reads its cached text, a whole-column range (`$B:$B`) reads the used rows (it lost the chart), each range stops at its sheet's last used row and the table is trimmed and capped at 100,000 cells as the PPTX one is (one `build_chart_table` for both). The cached points of a DOCX/PPTX chart land at their index with the gaps empty, `ptCount` long — they were packed, shifting every value after a gap into the wrong row — and inline `numLit`/`strLit` data is read too. **Limit:** a combination chart's `classification` is its back-most plot's label alone, where docling lists one prediction per plot type (no fixture has one)
- a threaded comment's `comment_section` group is one child of the body however many messages it holds (it was listed once per message, `xlsx_comments`)
- #1019, docling#5015: an image placed **in a cell** (Excel 365's "Place in Cell" — a rich value: `c/@vm` → `xl/metadata.xml` → `xl/richData/*` → the media part; the cell's `#VALUE!` is only the fallback for older readers) is a picture under the table that the cell refers to — a `RichTableCell` with empty text and a `ref` — with the cell's box as its provenance and Excel's alt text as docling's own `meta.description` (`text` only: unlike a drawing's opt-in alt text, #676, upstream writes this one, so Markdown prints it after the placeholder — `<!-- image -->  A red square` — and HTML in the figure's meta). Only `_localImage` rich values are images (a linked data type keeps its error text); an image cell counts as content for the section-label split, as upstream's. **Not ported:** docling's `treat_singleton_as_text` option (a lone image cell as a bare picture) — the backend has no such option, so a one-cell sheet stays a one-cell table, upstream's default

- re-verified against docling 2.129: the flat `Node::Prov` stream already yields upstream's shape, so the spreadsheet backend needs no item tree
- everything but `origin`/`version` and, where a sheet holds a raster image, the PNG bytes PIL re-encodes
- items are numbered as docling creates them (a sheet's tables with their labels, then its images, then its charts, in drawing order — the `seq` a `Node::Prov` carries) while the group's children follow docling's position sort, stable over that creation order
- every sheet is a page (`pages`, sized like docling's `_find_page_size` — the largest right/bottom edge of its items in cell units, 0×0 for an empty sheet)
- every item carries docling's provenance verbatim — its cell-index box, top-left origin, `charspan` `[0, 0]` — through a `Node::Prov` wrapper the JSON reads (the 0–511 DocLang grid cannot round-trip those integers; DocLang, Markdown, LaTeX and the chunker render the wrapped node unchanged)
- a chart's title caption is a sibling of the picture in the sheet group, listed before it, with the chart's box and a charspan over the caption text, as docling's office backends add it (PPTX charts get the same)
- `TableData.orientation: rot_0` is written for every table and chart grid
- a picture's `image.size` is floats and `image` precedes `annotations` — pydantic field order
- #405: a chart *sheet* exposes its chart the way a worksheet's does — the sheet's `absoluteAnchor` drawing (no cell range; docling's bbox is `(0, 0, 0, 0)`) was skipped, leaving the group empty
- every chart picture now carries docling's `meta` in the JSON: `classification` with the chart kind as its one prediction and `tabular_chart.chart_data`, the series reconstructed as a `TableData` (row 0 the series names as column headers, the category column as row headers), for XLSX and PPTX/ODF/DOCX charts alike
- `xlsx_chart_sheet` (data sheet + chart sheet + the same chart on a worksheet) is mirrored with 2.127 groundtruth and the stale `xlsx_03_chartsheet` groundtruth refreshed
- #410: a merged range is *one* `TableCell` in the JSON — the anchor's offsets with `row_span`/`col_span`, repeated across the grid positions it covers, as docling writes it — where we wrote a 1×1 cell per covered position with the anchor's text copied into each (Markdown was already right, the expanded grid).
- The serializer derives the spans from the structure overlay's continuation flags, so backends without first-class cells (XLSX, PPTX `rowSpan`/`gridSpan`, DocLang) all get them
- `xlsx_merged_range` mirrored, and the refreshed `xlsx_01`/`_04`/`_07` and `powerpoint_sample` JSON now match upstream's cell spans exactly
- #395: a sheet is scanned in docling's `_find_true_data_bounds` frame — the union of the valued cells and *every* merged range — so a merge that starts above or left of the first value no longer underflows the rebase onto calamine's clipped range (it panicked)
- as upstream does, an empty merged range no longer seeds a table of its own (docling seeds only from a cell that carries a value)
- #366: `.xltx`/`.xltm` templates route to the same reader, docling#4178
- incl. chart captions/classification/data grids
- the JSON carries docling's structure too:
  - one `sheet` group per worksheet (named after it, `invisible` content layer for a hidden sheet, which puts its tables in the JSON instead of dropping them)
  - every cell note as a `comment-{sheet}-{cell}` `comment_section` on the notes layer, with `comments` back-refs on the item whose cell range covers the commented cell (docling's `_find_cell_item`).
- Note the upstream asymmetry the refs mirror: a spreadsheet comment goes through docling-core's `add_comment`, which links the **note text** (`#/texts/N`), while the docx backend overwrites that with the **group** (`#/groups/N`) so replies group together.
- Markdown and DocLang are unaffected — docling emits no sheet heading (that `## Sheet1` lives only in older stored groundtruth) and DocLang has no group element
- #466 (docling#4353): a threaded comment keeps *every* message of its thread — ordered root-first by the `id`/`parentId` links, replies by timestamp then document order, orphans last — one `[author: …, time: …]: …` note per message inside the cell's single `comment-{sheet}-{cell}` group and one `comments` back-ref per note (`xlsx_comments`: 5 notes, we kept one per cell)
- docling#4302: the whole bounding rectangle of a found table is marked visited, so a non-empty cell inside it that is not 4-connected to the region no longer seeds a duplicate fragment table
- #634: a cell prints the value Excel *displays* — its number format applied by docling PR #4628's rules (`backend/numfmt.rs`, a port of that PR's `_format_excel_value` / `_format_excel_number` / `_format_excel_date` and openpyxl's `is_date_format`, with the PR's own test table as unit tests and its code run through openpyxl as the oracle for the fixtures):
  - a `$`/`%` section (`"$"#,##0.00`, `0%`, `[$$-409]…`, negative and zero sections) with Excel's half-up rounding (`0.125` at `0%` → `13%`)
  - a date format token by token (`mmm-yy` → `Feb-25`, `"Due "mmm d`, `h:mm AM/PM`, `mm` next to `h`/`ss` as minutes, `h`/`d`/`m` unpadded)
  - the built-in `mm-dd-yy` (14) / `m/d/yy h:mm` (22) as ISO
  - everything the PR does not interpret — `"EUR"` and other currency words, plain `#,##0`, `00000`, `[h]:mm:ss`, conditions, other locale markers, `#` optional decimals, scientific — stays `str(value)` as before, so no cell reads worse.
- The codes come from `xl/styles.xml` + each sheet's `c/@s` (calamine exposes none of this); `.xls` reads the BIFF `FORMAT`/`XF` records and the cells' `ixfe`; `.xlsb` keeps printing raw.
- docling#4628 merged upstream (`25cdd57`): the 20 date cells of `xlsx_02_sample_sales_data` in its refreshed groundtruth (`2024-01-01 0:00:00`, …) are what our port prints, cell for cell (#1020).
- #1020: an error cell reads as Excel shows it — `#N/A`, `#VALUE!`, `#DIV/0!` (calamine's `Display`, openpyxl's text) — not as calamine's enum variant (`NA`, `Value`, `Div0`).
- #1020 (docling#5030): the 100,000-cell chart-table cap counts the header row — `MAX / cols - 1` data rows; 1 series × 50,000 points emitted 100,002 cells (shared with PPTX/DOCX charts through `build_chart_table`; the iWork charts already counted it).
- Fixtures: the issue's `xlsx_number_formats_min.xlsx`/`.xls`, docling#4619's "Order" sheet and the PR's review table (`xlsx_number_formats.xlsx`), a 1904-epoch workbook.
- No opt-out, as the PR has none
- Picture alt text (#676): the `descr`/`title` of a picture's non-visual properties is kept as the picture's description with `created_by: alt_text` — in the JSON always, in Markdown/HTML under `picture_alt_text` or strict Markdown (`![alt](…)`, a text line above a placeholder, the `<img alt>`), so the default Markdown is unchanged; MarkItDown prints it, docling drops it.

#### XLSB (binary Excel 2007+)

**Backend:** `xlsx.rs` → calamine's `Xlsb` reader (#210)\
**Status:** **docling.rs extension — upstream has no XLSB backend**

- tables, hidden-sheet layering and page breaks match the XLSX path
- drawings/charts/comments/merges aren't exposed by the binary reader

#### PPTX

**Backend:** `pptx.rs` (roxmltree)\
**Status:** **8/8 exact**; **JSON: structurally identical to docling 2.129's on 8/8** (every key and value but `origin`/`version` and the PNG bytes PIL re-encodes)

- the flat node stream is not the shape upstream's JSON has, so the slide walk fills docling's `ItemTree` alongside it, as `MsPowerpointDocumentBackend._walk_linear` does:
  - a `chapter` group per slide
  - `paragraph` (not `text`) for a non-title paragraph
  - a `list` group per run of bullets whose items carry `marker: ""` (bullet) or `"N."` (numbered)
  - a table holding only its non-empty cells — `_handle_tables` drops the blanks, skips an all-blank table, and makes every first-row cell a column header
  - a picture at the file's dpi (python-pptx's `Image.dpi`: PNG `pHYs`, JFIF density else EXIF resolution, BMP/TIFF resolution, rounded half-to-even, 72 when absent or outside 1–2048), and no picture at all for an undecodable non-metafile image
  - a chart as a classified picture with a slide-level caption
  - speaker notes as a notes-layer `text` with a zero `TOPLEFT` box
  - review comments as `comment_section` groups named `comment-slide{N}-{idx}` on the notes layer, each holding its note
  - and every item with upstream's provenance verbatim: the shape's raw EMU box as `BoundingBox.from_tuple((l, t, r, b), TOPLEFT)` — python-pptx's top-left EMU, tagged so since docling#4294 (#466; the box used to be tagged `BOTTOMLEFT`, which read the tuple as `(l, b, r, t)` and swapped the vertical edges — the eight mirrored groundtruth files are upstream's regenerated ones) — with a `charspan` over the item's own text
- Slides still convert in parallel; each fragment is renumbered into creation order by `ItemTree::append`.
- The four groundtruth files that predated docling#4190's per-paragraph charspan (`powerpoint_comments`, `_issue_2663`, `_sample`, `_unrecognized_shape`) are refreshed to 2.129's output.
- #390 (docling#4190): a chart's title caption is a child of the slide group, listed ahead of the chart picture, with the chart's box and a charspan over the caption text, and every paragraph's `charspan` is its own (docling used to reuse the shape's) — the mirrored `pptx_chart` groundtruth is upstream's post-#4190 copy and our JSON is structurally identical to it
  - a subtitle placeholder stays a `text` paragraph on purpose (docling#3785, re-settled in #4190) — not a bug
- #406: a paragraph whose bullet or numbering comes from the owning shape's list style (`a:txBody/a:lstStyle/a:lvl{N}pPr/a:buChar|a:buAutoNum`, selected by the paragraph's `a:pPr/@lvl`) is a list item, as docling's `_get_effective_list_marker` resolves it
  - a `buChar`/`buBlip` bullets it, a `buAutoNum` numbers it, a `buNone` keeps it a paragraph, and an indented paragraph with no marker anywhere is a list item (docling's `level > 0` fallback)
  - since docling#4397 (#466) the level also nests: a paragraph at a deeper `a:pPr/@lvl` opens a `list` group under the last item of the enclosing list, a shallower one pops back, every group numbers its own enumerated items, and a non-list paragraph closes them all — docling's `_OpenList` stack (`pptx_inherited_bullets` indents its inherited level-2 and level-3 items)
- #627 (docling#4581, upstream `main` after 2.135.0): the cascade continues past the shape — a placeholder reads the layout placeholder with the same `idx` (idx only, as python-pptx's `layout.placeholders.get`; a missing `idx` is 0) and, only when there is one, the master's `p:txStyles` bucket for its type (`body`/`obj`/no type → `bodyStyle`, `title` → `titleStyle`, everything else — `ctrTitle` and `subTitle` included — `otherStyle`)
  - the old "body placeholder is a list" default is gone, so the default template's "Section Header" body and any layout or master `buNone` give paragraphs, a master `buAutoNum` numbers the body, and a subtitle takes `otherStyle`'s marker (with it, a centred title can become a list item, as upstream's does)
  - a numbered group starts at its first paragraph's `a:buAutoNum/@startAt` (docling's `_get_auto_number_start`), and a bullet that joins a numbered group renders `N.` by position in Markdown, as docling-core does
  - `crates/docling/tests/data/pptx/list_cascade/` (the reporter's A–H decks plus I–K) matches upstream's Markdown and JSON 11/11
- #1018 (docling#5122, #4989), ported with upstream's unit tests (`backend/pptx_upstream_unit.rs`: `test_backend_pptx_list_markers.py`, `test_backend_pptx_picture_fill.py`):
  - a numbered item's marker follows its effective `a:buAutoNum/@type` — the same properties element that made it numbered, paragraph, shape list style, layout or master: `alpha{Lc,Uc}` / `roman{Lc,Uc}` with `Period` / `ParenR` / `ParenBoth` print `a.`, `IV.`, `(C)` (Markdown `- a. Plan`); every other scheme (`arabicParenR`, East Asian, …) keeps `N.`
  - a `startAt` outside 1..=32767 (`ST_TextBulletStartAtNum`) is ignored and the list starts at 1 — a start of 2000000000 would build ~77M-character letter markers
  - a `p:sp` filled with a picture (`p:spPr/a:blipFill/a:blip/@r:embed`) is a picture, then its text; a fill that only links (`r:link`), has an empty `r:embed` or points at a missing part adds none, and the text stays
  - `.ppt`: the PP9 `TextAutoNumberScheme` maps to the same letter / Roman markers (the decimal schemes keep the plain numbering) and the same `startAt` cap applies; the binary reader emits no pictures, so the fill half does not apply there
- #402: a slide's speaker notes and review comments reach the **JSON** as text items on docling's `notes` content layer (they were parsed all along, but only DocLang rendered them, so a consumer reading JSON alone could not see them)
  - Markdown is unchanged — it serializes the body layer.
- Each slide is also a `chapter` group named `slide-{0-based}`, as docling writes it, and every slide is recorded as a page sized in EMU, so items carry provenance
  - on `powerpoint_sample` the group tree (9 groups, same refs, names, labels and parents), the page map and every `charspan` are identical to docling's, and the `prov` boxes are docling's raw EMU (they used to be expanded from the 0–511 `<location>` grid, off by up to one grid step)
  - An item with no geometry — a speaker note — carries docling's zero bbox rather than a box spanning the slide.
- docling 2.120 parity:
  - #320: 3-D chart variants are `other_chart` without a data grid like python-pptx's unsupported plots (docling#3972), a shape at x = 0 EMU keeps its own bbox (docling#3990)
  - #249: slide shapes convert in visual reading order — top-sorted, 0.05"-tolerance row banding, left-to-right within a row — instead of XML/z-order, at slide level and inside groups
- docling#4089 (EMF/WMF pictures kept as positioned pictures, rendered through LibreOffice when it is installed and without image data otherwise) — here they are always rendered, in-process (#536, `metafile.rs`), and a metafile that draws nothing stays the payload-less picture
- #575 (a deliberate divergence): equations are kept — PowerPoint writes OMML as an `<a14:m>` paragraph child (python-pptx has no notion of it, so docling's text is the paragraph without the formula), converted here by the DOCX backend's OMML → LaTeX port (`omml.rs`)
  - a paragraph of equations alone is a `formula` item (`$$…$$` in Markdown), one with words carries them inline as `$…$`
  - the slide's `mc:AlternateContent` is settled before parsing by the DOCX backends' `mc.rs` — a text box holding an equation is an `a14` Choice with a picture Fallback, so the Choice is read (python-pptx skips the whole block, Choice and Fallback alike; any other Choice it cannot read now yields its Fallback shape instead of nothing)
  - The corpus has no such block in a shape tree (only `p14` slide transitions), so 8/8 is unchanged
- Picture alt text (#676): the `descr`/`title` of a picture's non-visual properties is kept as the picture's description with `created_by: alt_text` — in the JSON always, in Markdown/HTML under `picture_alt_text` or strict Markdown (`![alt](…)`, a text line above a placeholder, the `<img alt>`), so the default Markdown is unchanged; MarkItDown prints it, docling drops it.
- docling 2.138–2.140 parity:
  - a slide hidden with `show="0"` and a shape hidden in the Selection Pane (`p:cNvPr hidden`) go to the invisible layer, speaker notes keeping theirs (#754, docling#4647/#4733)
  - footer, date and slide-number placeholders go to the furniture layer (#755, docling#4746)
  - upstream's test decks in `crates/docling/tests/data/pptx/hidden_layers/` match docling 2.140.0's Markdown and JSON 6/6, and docling 2.148's DocLang 5/5 — a hidden slide's title, a hidden table and a hidden picture keep their `<location>` behind the `<layer value="background"/>` token (DocLang has no invisible layer; docling-core#800 writes the closest one), the picture's image a data URI as docling's archive keeps every picture off the body
  - a chart reads every plot of a combination chart — one prediction per plot type, back-most first, the series of every plot — a 3-D bar/line/pie, stock, surface or bar-of-pie plot too, labelled by its tag (`bar3DChart` a bar chart; docling#4811, after 2.142.1: `pptx_chart`'s 3-D column chart is a `bar_chart` with its data, the groundtruth mirrored from upstream `main`), a malformed series (no `c:order`) leaving the label without a table, categories on a date axis (or showing a date format) as ISO dates, the category axis title in the table's corner, the caption only from `c:chart/c:title` (#756, docling#4773) — upstream's decks in `crates/docling/tests/data/pptx/charts/` match 8/8, and `pptx_chart` stays identical to 2.140.0's output
  - a shape inside a moved, resized, nested or flipped group gets its drawn box — mapped from the group's child space through every enclosing group's `a:off`/`a:ext` and flips, rotation not applied (#766, docling#4859)
  - a hyperlink on a whole shape (`p:cNvPr/a:hlinkClick`, a URL or `ppaction://hlinkfile`, never a slide jump) goes on every text and list item the shape adds, a target without a scheme kept as a path — `[Watch the talk](https://example.com/talk)` (#767, docling#4787) — `crates/docling/tests/data/pptx/shapes/` matches 2.140.0's Markdown and JSON 5/5
- DocLang `<location>` tokens are measured from the slide's top-left corner, as docling-core derives them from the top-left provenance box (half-to-even rounding, clamped to 0–511) — they used to flip y, turning every shape's box upside down against docling's
  - a linked list item's `<href>` precedes its location
  - DocLang matches 2.140.0's byte for byte on four `shapes` decks, and its location tokens on 27 of 29 test decks (the other two are hidden-layer items, whose DocLang still drops the location)

#### DOCX

**Backend:** `docx.rs` (roxmltree)\
**Status:** vs docling 2.142.1 (the mirrored corpus is 2.142.1's): **38/38 exact** Markdown, **JSON structurally identical on 38/38** (`full_conformance.py`: picture payloads compared by presence, see below)

- docling#4958 (docling `main` after 2.142.1): hidden text (`w:vanish`, Word's Hidden font property, from the run, its character style chain or the paragraph style chain) leaves the default export — a fully hidden paragraph, heading or list item goes to the `invisible` layer (kept in the JSON, out of the Markdown; its list group stays on the layer around it), a hidden run of a shown paragraph, link or content control is dropped, as is a fully hidden paragraph of a table cell or a note (no layer per paragraph there); an equation's paragraph is never hidden. Settled on the XML before either walk reads it (`docx_hidden.rs`, like `mc.rs`); a run holding a drawing, picture or object is never cut. Headings and list items now take the paragraph's layer, as docling passes it — a header part's list item is `furniture`, no longer `body`
- docling 2.130–2.142's DOCX fixes pinned by unit tests only, ported with those tests (`backend/docx_upstream_unit.rs`, both walks):
  - docling#4465: Word's multi-level list styles ("List Bullet 2", "List Number 2", …) each own a `numId` at `w:ilvl` 0 and differ only by their left indentation — an item of another `numId` indented deeper than the open list's item (the paragraph's own `w:ind` `left`/`start`, else its numbering level's, else its style's `basedOn` chain) opens its list under that item; one indented no deeper than the item a nested list hangs from returns to the outer list, which goes on numbering; lists at one indentation stay siblings; a blank paragraph closes the nested list with the outer one, whose group a resumed outer item reuses. The Markdown matches docling 2.142.1's on the upstream tests' documents
  - docling#4735: a footnote's / endnote's text inside a tracked insertion, a content control or a table is kept (the note body is read at any depth)
  - docling#4576: a hyperlink's runs inside a wrapper (`w:ins`, `w:smartTag`, `w:customXml`, `w:fldSimple`) keep their text and link in the item tree (the flat walk already read them)
  - docling#4563/#4613: a hyperlink whose fragment-only relationship is dropped keeps its text, unlinked, in one item with its neighbours (already so here — pinned)
  - docling#4973: `_CODE_CALL_PATTERN`'s argument runs bounded at 200 characters — the same semantics, checked by a scan behind the unbounded pattern: the `regex` crate ran the counted-repetition pattern ~300× slower on a long paragraph
  - docling#4480: `use_outline_level_for_headings` (default on; `--no-outline-level-headings`, every surface — `docs/OPTIONS.md`) — off, an outline level alone no longer promotes a style that is not named a heading; the `.doc` reader honours it too
  - docling#4366 (`w:bCs` is not bold) was already ported
  - docling#5123 (#1017): a `w:num`'s `w:lvlOverride` is applied — its `w:lvl` replaces the abstract level (format, `lvlText`, indentation), its `w:startOverride` sets the instance's start when it is an integer in 0..=32767 (docling's `_MAX_START_OVERRIDE`; anything else keeps the level's `w:start`); `test_backend_msword_list_overrides.py` ported, plus a `docx_chain_parity` case
- docling#4863 (2.142): a sub-list is a child of the list item above it, not of the outer list group — the JSON tree, and with it the HTML export's nested `<ul>` inside its `<li>` (`docx_lists`, `docx_list_blank_spacer`, `docx_list_east_asian_num_fmt`, `unit_test_formatting`, `unit_test_headers_numbered`)
- docling#4456: a paragraph's numbering is its own `w:pPr/w:numPr` only — reading it from any descendant took the numbering of a paragraph inside an anchored textbox, an empty list item and a stray empty list group after every numbered textbox (`textbox`)
- docling-core 2.100's list markers: a marker holding any Unicode letter or digit is informative and prints after the bullet (`- 第九条 …`, `- （一） …`, `- ① …`); only ASCII ones did (`docx_list_east_asian_num_fmt`). The Markdown serializer applies docling-core's `orig_list_item_marker_mode=AUTO` rule to every backend's markers (an ODF list without a suffix keeps the model's `1.` marker for it)
- a straight vertical or horizontal line (a zero-width or -height `wp:extent`) is drawn too, its canvas widened by its `wp:effectExtent` — `textbox`'s arrow connectors, which docling renders through LibreOffice

- docling#4243: a fragment-only relationship target (`Target="#anchor"`) is dropped as upstream strips it before python-docx opens the package — the hyperlink keeps its text, no target
- #865 (docling#4831, 2.141): a style declared with the invalid `w:type="list"` is a numbering style, as upstream normalizes it (python-docx used to fail the whole document on it; `styles.xml` is read directly here, so it never failed). A paragraph whose `w:pStyle` names a style that is not a paragraph style — that one, a numbering, character or table style — reads as unstyled, python-docx's default paragraph style (it used to take the style's heading name or outline level); the style's `numPr` still serves the `numStyleLink`s that name it. Fixture `docx_list_style_type`
- #858: a blip-less DrawingML drawing — a group (`wpg:wgp`) of shapes (`wps:wsp`), connectors and text boxes, or one lone shape — is drawn in-process into an **SVG picture payload** (`docx_drawingml.rs`; docling renders it through LibreOffice into a PNG, or leaves it payload-less without one). Covered: the common preset geometries (rectangles, rounded, ellipse, diamond, triangles, parallelogram, trapezoid, hexagon, octagon, the flowchart presets, `can`, `foldedCorner`, `bevel`, block arrows, callouts), `custGeom` paths, straight and bent connectors, solid and theme fills (`theme1.xml` colours with Word's `lumMod`/`lumOff`/`shade`/`tint`), outlines with width, dash and the `triangle`/`stealth`/`arrow`/`oval`/`diamond` ends, `rot`/`flipH`/`flipV`, nested groups, the labels as `<text>` (size, bold, italic, colour, alignment, wrapped at the shape's insets, bullets for list paragraphs). A gradient draws as its first stop, an unknown preset as its bounding box, each with a `DOCLING_RS_DEBUG` diagnostic. Several anchored drawings in one paragraph are one picture placed by their `wp:posOffset`s, as upstream renders them. The payload is `image/svg+xml` (vector, byte-deterministic — the regression suite pins the bytes; Markdown's referenced mode writes `image_NNNNNN.svg`), at 96 px/in with the drawing's extent as its display size. The labels stay as text items too, as docling keeps them.
  - #886: the drawings of one paragraph share one space when they mix anchored (`wp:anchor`) and inline (`wp:inline`) ones: the inline drawing starts the paragraph's flow, so the anchors placed against the column and the paragraph (or line) keep their offsets around it — arrows touch the boxes they join, an annotation stays docked, an anchored box keeps its x — as LibreOffice renders them; against other frames (the page, the margins, a character), and for inline drawings alone, they still stack (`drawingml_anchor_inline_mixed`, the reporter's synthetic repro)
- JSON 36/36 — a JPEG picture is written as the PNG PIL re-encodes it to, from the same libjpeg pixels, #527
- #385: a list item's `first_in_list` is Word's list identity — a `numId` the previous list item did not carry, or body text since it, opens a new list; the same `numId` continues one across nesting and across an empty spacer paragraph (docling's `_manage_list_structure` and its ListGroup cache)
  - the serializers draw every list boundary from that flag, no longer guessing it from a kind flip or a number gap (`docx_lists`/`docx_list_blank_spacer` unchanged, `unit_test_headers_numbered` now nests its list groups as upstream does)
- #409: a code paragraph's text is python-docx's `Paragraph.text`, as docling's `raw_paragraph_text` is — so an in-paragraph `<w:br/>` (its own run, or between the `w:t`s of one run) is a newline and a tab is a tab, where flattening the subtree to `w:t` alone joined `total = a + b` and `print(total)` into one line of the fenced block
  - plain table cells read the same way (python-docx's `cell.text`)
- #408: the trailing run group of a paragraph closes under the format that *opened* it, as in docling's `_get_paragraph_elements` — a whitespace-only run never opens a group, so bold/italic text followed by Word's unformatted trailing-space run keeps its `**…**`/`*…*` (we took the last run's plain format and dropped the markers, in body text and list items alike)
  - both reporter files are mirrored (`docx_bold_tail`, `docx_code_breaks`) and match upstream's Markdown byte-for-byte
- #400: a run's inner content follows python-docx's `CT_R.text` — where docling's paragraph text comes from — so `<w:noBreakHyphen/>` is the plain `-` it renders as (dropping it glued `In` + `Transit` into `InTransit`), `<w:ptab/>` is a tab, and a `<w:br>` is a newline only when it is a *line* break: a page or column break has no text equivalent there
- #389: docling#3946/#3951 — a table cell Word wrapped in a content control (`w:tr/w:sdt/w:sdtContent/w:tc`, how it writes cover pages and document-property fields) is collected as a cell of its row, so a 1×1 layout table keeps its content and a wrapped cell no longer slides every later cell of the row a column to the left
  - the mirrored `docx_sdt_table_cells` fixture matches upstream's grid and text items exactly
- docling 2.126 parity (#363):
  - docling#3917 resolves a style's numbering through its `basedOn` chain with `numId` and `ilvl` inherited independently (Word's stock `heading 2` carries only `ilvl` and takes `numId` from `heading 1`; refreshed `unit_test_headers_numbered` adds the inherited `2.3` heading)
  - docling#4087 renders list markers with the level's `numFmt` — letter (a…z, aa…), roman, `decimalZero` — per placeholder and per hierarchical part, and keeps the `lvlText` suffix on non-decimal levels (`%2)` → `a)`, refreshed `docx_lists` Test 10)
- docling 2.125 parity: #320 mirrors
  - docling#3952's content-control-with-picture fixture, already handled by the `<w:sdt>` walk
  - docling#3729's Strict OOXML pair (`Strict.docx`/`Transitional.docx` — the `purl.oclc.org` namespaces already resolve through the namespace-agnostic walk)
  - docling#3760's refreshed `unit_test_headers_numbered` (a heading whose numbering level has `numFmt none` — Word's invisible numbering — keeps its outline `numPr` but gets no computed `1.2` prefix; only the visible formats decimal/roman/letter/decimalZero number a heading)
  - docling#4036's textbox groundtruth (pictures anchored inside textboxes)
  - reviewer comments as first-class `comment_section` groups — each `w:comment` becomes a notes-layer group named `comment-{w:id}` holding the `[author: … , time: …]: text` note, and every body item covered by its `w:commentRangeStart`/`End` (or anchored by a bare `w:commentReference`) carries docling's `comments: [{"$ref": "#/groups/N"}]` back-ref, so the JSON groups/refs are byte-identical to upstream's; Markdown, LaTeX and DocLang are unchanged (DocLang keeps the flat item upstream writes, on `<layer value="furniture"/>` — DocLang has no notes layer, docling-core#800)
- #248: all sections' headers/footers as furniture incl. first-page + regular pairs
  - and the even-page pair when `word/settings.xml` has `<w:evenAndOddHeaders/>` (#590, a divergence until docling#4614 read them too in 2.142): Word shows that header/footer on every even page
  - emitted after the regular part of each kind (first-page → regular → even), skipped when the setting is off (an `even` reference is then a leftover Word ignores too) — `docx_even_headers_footers`, and Word's own save of it
  - resumed ordered-list numbering per `numId`, body text after a blank spacer stays inside the list group
- #270 / docling#3961: headings detected by the style's `w:outlineLvl` — localized/custom heading styles ("Nadpis1", "Rubrik 1") promote by OOXML's own language-independent marker, outlineLvl 9 stays body text, Title/Subtitle-ish styles keep their own branch
  - the name check also covers the style *id* and one-hop `basedOn` id/name, so a custom style based on "Heading 2" or an English "Heading1" id under a localized display name promotes without any outline level — direct formatting with no style at all stays body text, as in docling
- **JSON: structurally identical to docling's on 38/38** (every key and value but `origin`/`version`, and the `image` payload of the pictures upstream renders through LibreOffice — blip-less DrawingML shapes and the textbox shapes of `textbox`, drawn here into SVG payloads instead (#858), and the EMF of `test_emf_docx`, rendered here by `metafile.rs` instead (#536: EMF/WMF records → SVG → resvg PNG, so the pixels differ from LibreOffice's); a raster picture's bytes are the embedded file's where PIL re-encodes to PNG)
- The flat node stream is not the shape upstream's JSON has, so `docx_tree.rs` ports `MsWordDocumentBackend`'s construction call-for-call into the docling-core `ItemTree` the JSON export serializes (the same XML readers as the Markdown walk — styles, numbering, run text, code detection, list markers, chart parts):
  - everything after a `Title`/heading nests under it, a skipped heading level is a `header-N` section group
  - a paragraph is python-docx's run/hyperlink/content-control sequence grouped by `Formatting` (`_get_paragraph_elements`; bold also from the paragraph mark's run properties and the style's `basedOn` chain), two or more groups an `inline` group of `text` items each with `formatting` and `hyperlink` (`AnyUrl`-normalized, `Path` otherwise), one group a lone item
  - `_manage_list_structure`'s open-parents stack (`list` groups per `numId`/`ilvl`, the group cache a blank spacer paragraph keeps and the empty text item it *deletes* when the list resumes, `header-`less `list_item`s with `enumerated`/`marker`, a mixed-format or equation item an empty `list_item` over an inline group)
  - code paragraphs merged into the previous `code` item under upstream's last-child / last-text / same-layer rule with buffered blank lines
  - standalone `formula` items and inline `text`/`formula` alternation
  - tables with python-docx's `tblGrid` column count, widened to the widest row (`w:gridBefore` + the cells' `w:gridSpan`s + `w:gridAfter`) when cells reach past it — a deliberate divergence until docling#5179 (merged 2026-11-05; unreleased as of docling 2.147.0) adopted the same rule for docling#5006: before it upstream dropped a table without `w:tblGrid` (python-docx could not load it) and cut every cell beyond the declared grid; Markdown always kept them, so the JSON agrees with it — `tables_outside_their_grid_are_in_both_chains` —, `gridBefore`, `vMerge` row spans, raw `cell.text`, `$…$` equations, and rich cells (several paragraphs, non-paragraph blocks, blips, formatted runs, a code style) walked in an isolated list context then re-parented under `rich_cell_group_{tables}_{col}_{row}` with the cell's `ref`; a 1×1 table walked as body
  - pictures per `a:blip`/`v:imagedata` (a `picture_area` group for several, `invisible` layer for a spacer), one payload-less picture per blip-less DrawingML paragraph, charts as classified pictures with `tabular_chart` data and a body-level caption
  - textboxes (`txbxContent`, VML, `wps:txbx`) a `textbox` section with every paragraph kept — **not** deduplicated by text as upstream's `_handle_textbox_content` does: that dedup existed for the `mc:AlternateContent` copy of a box, which `mc.rs` settles before parsing (#572 — the first `mc:Choice` whose `Requires` namespaces the backend reads, `wps`/`wpg`/`wpc`/`wp14`/`w14`/`a14`, else the `mc:Fallback`, at every level), and it dropped a paragraph legitimately repeated in a box (#576's `textbox_repeated_lines`, the reporter's three-line box, pinned as `.docx` and Word's `.doc` save — 3/3 paragraphs in both, docling keeps 2)
  - a body-level `mc:AlternateContent` is read through its selected branch — docling 2.142 does the same (docling#4615; before it python-docx's `body.iterchildren()` skipped the block with its paragraphs), and the branch rule is its `_select_alternate_content_branch`: the first `mc:Choice` whose `Requires` namespaces are all read, else the `mc:Fallback`; docling's set (`w`, `a`, `c`, `r`, `wp`, `mc`, `v`, `w10`, `wps`, `w14`, `a14`) plus `wpg`, `wpc` and `wp14`, whose group / canvas text boxes both walks read where docling takes their fallback — as Word, its `.doc` save and anydoc read it (`alternate_content_body`, `alternate_content_textbox`; the 38 mirrored fixtures are unchanged), `shape-text` for DrawingML text outside any
  - headers/footers `page header`/`page footer` furniture sections
  - reviewer comments `comment_section` groups on the notes layer with `comments` back-refs on the paragraph's items
- Markdown / DocLang / LaTeX / chunks read the flat nodes and are unchanged
- #466 (docling#4319): a heading style's level clamps into OOXML's 1–9 (`Heading 111` → 9, as `Heading 0` → 1 already did)
- #466 (docling 2.130+; `docx_list_east_asian_num_fmt`, `docx_list_resumed_after_table` and the extended `docx_lists` mirrored — 38/38 exact, JSON structurally identical for all three):
  - docling#4336's East Asian `w:numFmt` markers — `chineseCounting`, `chineseCountingThousand`, `chineseLegalSimplified`, `ideographDigital`, `ideographTraditional`, `ideographZodiac`, `japaneseCounting`, `decimalFullWidth`, `decimalEnclosedCircle`, rendered per ECMA-376 §17.18.59 with Word 16.112's tie-breaks (grouped Chinese numbers write 〇 for a run of zeros, 十 without a leading 一, an empty marker from 1,000,000) into the level's `lvlText` (`第%1条` → `第九条`, `（一）`, `①`), the item `enumerated` with that `marker` in the JSON
  - in Markdown docling-core's `orig_list_item_marker_mode=AUTO` rule is applied on our side — a marker of Unicode digits and a dot (`１.`) prints verbatim, one holding an ASCII letter or digit stays a text prefix behind the bullet, one without any (`第九条`, `甲.`) is dropped and the item is a plain `-`
  - docling#4188: a list starting above `w:ilvl` 0 maps its levels through `_slot_for` (subtracting the start level, clamping shallower items to the list base) so its items stay list items
  - docling#4305: a cached list group is reused only when nothing but blank paragraphs follows it in its parent — a table in between opens a new group instead of placing the items before it
  - docling#4374: footnote and endnote bodies (`word/footnotes.xml` / `word/endnotes.xml` through the document's relationships, separator placeholders skipped, a note's paragraphs space-joined) are furniture-layer `footnote` items after the comments — Markdown leaves them out like headers/footers, the JSON carries them (own `footnotes_endnotes.docx` fixture)
  - docling#4282 (2.130.0-33): a `Title`/`Heading` resets the list context with the parents stack, so a list after a numbered heading stays in its section.
- #527: a rich table cell spanning several grid positions (`w:gridSpan` / `w:vMerge` holding more than one paragraph, a list or a picture) prints its content once, at its top-left position, the covered positions empty — docling-core's `RichTableCell` is serialized by reference, so Markdown, HTML and LaTeX visit it once
  - blank paragraphs that a reused list group deletes from such a cell shift docling's cell provenance onto the next live items, and that regrouping (and the empty inline group it leaves on the body, a DocLang `<text></text>`) is reproduced
  - the JSON wraps list items outside a list group in a `group` ListGroup like docling-core's `validate_misplaced_list_items` and never writes a deleted child as an empty `$ref`, so it loads in docling-core
- docling 2.133 parity (#587): `<w:bCs>` (complex-script bold) is no longer a bold signal for the JSON's `formatting` or for the rich-cell test, as upstream's `_get_format_from_run` now spells out — Word emits it as a font-theme artefact next to `w:szCs` without the user applying bold, and writes `<w:b>` even for Arabic text when they do
  - a cell whose only run property was `bCs` became a RichTableCell with a `<strong>` in the HTML (`docx_bcs_only_cell`, and Word's own save of it) while the Markdown walk, which never read `bCs`, printed it plain
  - with one deliberate divergence (#586): the re-homed item keeps its children, where docling-core re-adds it from its text alone and a mixed-format item (an empty `list_item` over an `inline` group of runs — a cell's list paragraph after a `numId 0` spacer, the reporter's `docx_list_item_runs_in_cell`) comes back as an empty bullet, its runs gone from docling's JSON after export and from its Markdown/HTML (the JSON docling saves before exporting still has them; our Markdown reads the flat cell text and was never affected)
- #589: runs inside an inline custom XML markup element (`w:customXml`, the wrapper Word's XML-mapping and some generators put around a span) reach the Markdown — the run walk treats it as the transparent wrapper it is, like `smartTag` / `ins` / `fldSimple` / `sdt`, as the plain-text and JSON walks already did
  - `Prefix text. INSIDE_CUSTOM_XML Suffix text.` used to print with the middle missing (`docx_custom_xml_inline`; a *block-level* `w:customXml` around paragraphs is still skipped, as docling's `body.iterchildren()` walk skips it)
- #588 (a deliberate divergence): the glyphs of the symbol-encoded Windows fonts are read as the characters they show
  - a run in `Symbol`, `Wingdings`, `Wingdings 2`/`3` or `Webdings` (`w:rFonts w:ascii`/`w:hAnsi`) has each glyph code, the byte (`8D`) or its Private Use Area image (`U+F08D`), mapped to Unicode
  - #742: the table is docling-core's shared `symbol_fonts.json`, docling-project/docling-core#889, vendored byte-identical as `backend/symbol_fonts.json` and read at first use, so Python docling's DOCX reader and docling-parse produce the same characters — `Symbol` and `ZapfDingbats`: Adobe's encodings as the Unicode Consortium publishes them, Greek letters included (`a b p` in Symbol *is* `α β π`) and the corporate-use bracket pieces and serif/sans-serif ®©™ as their standard characters, never a private-use code; the Wingdings/Webdings families: the normative `UCS;ID` source references of L2/12-368, which differ from Pandoc's table in 107 entries — Wingdings `8D` is ➋, `6C` ⚫, `FC` ✓
  - a run in `ZapfDingbats` maps too, and a PDF-style name (`ABCDEF+Wingdings-Regular`, `SymbolMT`) resolves like docling-core's
  - a `w:sym` (Insert ▸ Symbol: `w:font` + `w:char`) becomes that glyph too, while a font without a table keeps the code point as Word stores it
  - python-docx hands docling the PUA code points unchanged (invisible, or tofu) and drops every `w:sym`, so a Symbol bullet, a Wingdings check mark or a formula's Greek vanished from the Markdown
  - Pandoc's reader maps them, and the output here is Pandoc's line for line (bar Symbol 0x20, a plain space here, Pandoc's no-break one) on the reporter's synthetic file and Word's native save (`docx_symbol_font_glyphs`, `docx_symbol_font_glyphs_word`)
  - `backend/symbol_fonts.rs` holds the tables, the Markdown, JSON and every other export read through them
  - a symbol font inherited from a *style* rather than set on the run is not resolved
- #532 (a deliberate divergence): a table cell whose content is a text box (VML `v:textbox` or DrawingML `wps:txbx`) prints the box's text in that cell in Markdown, LaTeX and DocLang — docling takes such a cell as plain (a text box is not one of `_is_rich_table_cell`'s triggers), so python-docx's `cell.text` leaves it empty and the box lands before the table as a `textbox` section
  - the JSON, and the HTML rendered from it, keep docling's tree (`docx_textbox_only_cell`, `docx_two_textboxes_cell`)
  - every paragraph of a text box is read, as docling's `.//w:p` does, so a table inside a text box reaches the output (`docx_textbox_table_cell`)
  - a block content control (`w:sdt`) inside a rich cell is walked like the cell itself — docling's `_walk_linear` — instead of printing empty (`docx_sdt_block_cell`)
- #545 (a deliberate divergence): a tracked move's destination (`w:moveTo`) reads like a tracked insertion — its text appears where Word shows it, the source (`w:moveFrom`) stays out like a deletion — in Markdown and the JSON alike, where docling (python-docx) drops it (`docx_move_tracked`, `docx_moveto_body`, `docx_moveto_cell`)
  - a JSON table cell's `text` also keeps tracked insertions and move destinations, which python-docx's `_Cell.text` skips — a plain cell holding only inserted text had lost it from the JSON
- #628: the two DOCX walks (the flat `Node` stream the Markdown/DocLang/LaTeX read, the item tree the JSON/HTML/Pandoc read) are compared in-crate on every DOCX fixture and on a generated matrix of single-feature packages — every inline/block wrapper (`sdt`, `ins`, `moveTo`, `del`, `customXml`, `smartTag`, `fldSimple`, hyperlink, footnote reference, comment range, VML/DrawingML text box, nested table, list item, heading) in every container (body, cell, nested cell, text box, text box in a cell, block `sdt` in a cell) — as the *set of words* each chain carries (order and formatting are out of scope, #532 places a cell's text box differently on purpose)
  - the first run found that a plain cell's text behind an inline `w:sdt` / `w:customXml` / `w:smartTag` / `w:fldSimple` was in the Markdown and missing from the JSON (python-docx's `CT_P.text` reads `w:r | w:hyperlink` only) — a deliberate divergence now, like the tracked changes above: the cell's `text` keeps it, bar a checkbox control's ☐/☒ glyph, which stays out as upstream has it since the state is the cell's `checkbox_*` child items (`backend/docx_chain_parity.rs`; the mirrored corpus's JSON is unchanged)
- Picture alt text (#676): the `descr`/`title` of a picture's non-visual properties is kept as the picture's description with `created_by: alt_text` — in the JSON always, in Markdown/HTML under `picture_alt_text` or strict Markdown (`![alt](…)`, a text line above a placeholder, the `<img alt>`), so the default Markdown is unchanged; MarkItDown prints it, docling drops it.
- Ported from docling 2.140 and checked document by document against it (`tests/data/docx/upstream`, `docx_upstream`):
  - a text box in a table cell is emitted once, inside its cell (docling#4757, #757)
  - a list resumed after blank paragraphs in a cell stays one list (docling#4674, #764)
  - a level an `abstractNum` lacks is taken through its `w:numStyleLink` numbering style, a caption numbered through a heading-relative template (`Tabell %1.%7`) carries `Tabell 1.2` in its text instead of becoming a list item, and a numbered heading seeds its `numId`'s counter (docling#4727, #765)
- Paragraph-level bold (`_get_format_from_run`'s paragraph-mark `w:pPr/w:rPr/w:b` and paragraph-style `basedOn`-chain steps) reaches the Markdown of body paragraphs, list items, checkbox items and rich-cell paragraphs as it reaches the JSON — Word's bold Caption style prints `**Tabell 1.1 …**` — hyperlink text included (docling 2.140 builds a link's runs against the paragraph, so the JSON walk now applies the mark to them too)
  - a heading or title prints its plain text in both, as upstream's `paragraph.text`, so neither a bold heading style nor a bold run puts `**` in it
  - a plain (non-rich) cell stays unformatted (`style_bold` probe, `word95_structures`; the Word 6/95 `.doc` reader applies its stylesheet's bold the same way, as docling reads a `.doc` through its `.docx` conversion)
- #841: a custom heading style is recognized through its whole `w:basedOn` chain — `SubSectionTitleL2` → `SectionTitleL2` → Heading 2 is a level-2 heading, numbered or not, in both the flat walk and the item tree — and a style's `w:outlineLvl` is inherited the same way (`heading_labels` / `inherited_outline`, bounded, cycle-safe)
  - an explicit body-text outline (9) on the way up ends the climb, as the `.doc` backend has read `istdBase` since #824
  - **Beyond docling**, whose `_get_label_and_level` stops after one hop and prints the two-hop style as bold text (`doc_inherited_heading_styles.docx`, now its `.doc` twin's Markdown byte for byte)
- #849: a `w:hyperlink w:anchor` resolves against the document's `w:bookmarkStart w:name`s (docling reads `r:id` only, so an internal link was bare text): the link's `hyperlink` is `#id` in both walks — Markdown `[Jump](#target-section)`, JSON `"hyperlink": "#target-section"`
  - the id is the heading's Pandoc auto-identifier for a bookmark in a heading (two bookmarks on one heading share it) and the bookmark's own name elsewhere (characters outside letters, digits, `_`, `-`, `.` as `-`), a repeated id suffixed `-1`, `-2`, … so each names one destination
  - `_GoBack` is skipped
  - `r:id` wins over `w:anchor`, a relationship the part lacks or a bookmark the document lacks is a `DOCLING_RS_DEBUG` diagnostic and bare text, never a failed conversion
  - The destinations reach the Pandoc AST (`_anchors`, Pandoc-path JSON only: docling's model has no field for them, and the docling JSON stays as it was)
- #850: the item tree records the numbering prefix it generated for a heading (`TreeItem::number`) apart from the authored text, for the Pandoc AST
- #851: `track_changes` (`accept` | `reject` | `all`, Pandoc's `--track-changes`; `DocumentConverter::track_changes`, `--track-changes`) reads `w:ins` / `w:del` / `w:moveTo` / `w:moveFrom` runs in both walks as the view asks — docling reads the accepted view only — and drops inserted / deleted table rows (`w:trPr/w:ins|w:del`) under `reject` / an explicit `accept`
  - under `all` a run keeps its typed revision (`TreeItem::revision`: kind, `w:author`, `w:date`) for the Pandoc AST and the Markdown strikes a deletion through
  - Unset is docling's reading, unchanged (every row kept)
  - Not read: paragraph-mark revisions, property changes, cell revisions — see `docs/OPTIONS.md`

#### DOC (Word 97–2004)

**Backend:** `doc.rs` (native [MS-DOC]: CFB + piece table + PAPX/CHPX/STSH + Escher)\
**Status:** byte-identical Markdown to the DOCX backend on fixtures converted to `.doc`

- headings, ordered/bullet lists, tables, bold/italic, and embedded pictures — inline PICF + floating shapes with decoded PNG/JPEG bytes; EMF/WMF BLIPs (inflated per their `OfficeArtMetafileHeader`, a WMF sized by its `ptSize`) rendered to PNG, #536
- docling reaches these only by shelling out to LibreOffice (PR 3804)
- vs docling 2.142.1's groundtruth for its two `.doc` fixtures (`greek_heading_styles`, `legacy_sample`, both in the regression suite): the same text and structure but for what the LibreOffice route changes — a `Title` paragraph is docling's level-1 section header there (`##`), the document title here (`#`, as docling reads the `Title` style of a `.docx`), and a literal U+2011 non-breaking hyphen stays U+2011 here where LibreOffice's `w:noBreakHyphen` reaches docling as `-` (Word's own non-breaking hyphen, the 0x1E control character, is `-` here too)
- styles that are not the built-in Heading 1–9/Title still promote to headings through their `sprmPOutLvl` (#270 — a LibreOffice-written legacy file names its heading styles in the document language with a "user" sti, leaving the outline level as the only marker)
- #641: a numbered heading keeps its number — Word attaches the heading's list to the *style* (`sprmPIlfo`/`sprmPIlvl` in the Heading style's PAPX, the paragraphs carrying only `istd`), which the stylesheet reader now keeps and a paragraph without its own list sprms inherits through the `istdBase` chain, `ilfo` and `ilvl` independently (the DOCX backend's `style_numbering`, docling#3917)
  - a heading whose level numbers visibly gets docling's `1` / `1.1` heading prefix (`_is_numbered_heading`, docling#3760: a bullet or `none`-format level leaves the text alone)
  - a non-heading paragraph in a list style becomes the list item its `.docx` twin is — the reporter's file converts byte-identically to its `.docx` and to docling's LibreOffice route (`doc_numbered_heading_styles.doc`/`.docx`)
- #512: the container's own streams are read from the root storage first, so a `.doc` embedding another Word document (its `WordDocument`/`1Table`/`Data` under `ObjectPool/`) no longer pairs the outer FIB with the inner table stream (`doc: bad piece table`) — output matches LibreOffice → DOCX on the issue's five samples
- the FIB is read through its declared `csw`/`cslw`/`cbRgFcLcb` counts with bounds checks (a truncated FIB is an error naming the field, an `fc`/`lcb` beyond the writer's array is "absent", overflowing ranges no longer panic on 32-bit targets)
- CFB v3 stream sizes ignore an uninitialized high 32 bits ([MS-CFB] 2.6.3)
- **Word 6.0/95** (`nFib` 101–105, previously `bad FIB magic`) converts — 8-bit Windows-1252 or, with `fExtChar` (Far East editions), UTF-16
  - its paragraphs match LibreOffice on Apache POI's Word 6/95 samples (`poi_word95.doc`, `poi_word6_sections2.doc` fixtures) bar LibreOffice's ASCII-flattened quotes, and on POI's Far East `Bug51944.doc` 25/28, the rest off by one Far-East-encoded character each
- #640: no longer text-only — the pre-97 PAPX / CHPX bin tables (2-byte page numbers, 7-byte `BX`es, the single-byte sprm set with its own operand sizes, a PAPX counted as `2·cb` bytes), stylesheet (8-bit Pascal names, the built-in Heading `sti`s) and section table (`sprmSGprfIhdt` names the header / footer stories `PlcfHdd` holds) go through the Word 97 story walker
  - so tables (cell / row marks + `sprmPFInTable` / `sprmPTtp`, multi-paragraph and merged-row cells), headings, bold / italic (a run's 0x80 / 0x81 operand is relative to the style and resolved through the based-on chain — Word 95 writes the same byte for a bold run in a plain style and a plain run in a bold heading style), fields, headers / footers (furniture) and footnotes come out
  - the issue's two files (`word6_tables`, `word95_structures`, Word 6.0 and Word 95) convert to Markdown byte-identical to the `.docx` Word saved from each, and POI's samples gain their bold runs and a footer
  - not read: text boxes, endnotes, merged cells' spans (autonumber lists since #700 / #828, pictures since #832)
- other versions get an error naming `wIdent`/`nFib` — except **Word for Windows 1.x/2.0** (#566, `wIdent` 0xA59B/0xA59C/0xA5DB, `nFib` ≤ 63): a flat file, not a compound one, which used to fail as `not a compound file`
  - its FIB at byte 0 locates the text (`fcMin`/`ccpText`) and, through 6-byte (`fc`, `cb`) bin-table pairs at 0xA0/0xA6, the 512-byte PAPX/CHPX FKP pages, so the main story converts as paragraphs, tables (`\r\x07` cell marks, the row-end paragraph's PAPX) and bold/italic runs — byte-identical Markdown to what Word's own `.docx` re-save of the reporter's file converts to (`word2_pcjs.doc` fixture; docling fails the file)
  - #573: the Word 2.0 stylesheet (`fcStshf`/`cbStshf`, the `ww1` layout — `cstcStd`, names, CHPX, PAPX and ESTCP blocks) is read for the style code each PAPX opens with, so the standard heading styles (`stc` 254 = heading 1 … 246 = heading 9) and a user style based on one convert to the `##`…`#` headings the file's `.docx` re-save gives (`word2_heading_styles.doc`)
  - pictures and fast-saved (`fComplex`) files are not read, the last with an error saying so
- #521: a file whose last sector is short — pre-97 writers did not pad to a whole 512-byte sector, so many real-world Word 6/95 files failed with `not a compound file` / `no WordDocument stream` — reads that sector as far as the file goes when only its unused tail is missing (the directory, the mini stream or a stream ending inside it)
  - a sector starting past the end of file, or one the chain still needs bytes from, is a truncated file and fails as `damaged compound file` / `<stream> stream unreadable`, also for `.ppt`, `.msg`, `.wps` and the StarOffice/Quattro containers sharing the reader
- #535: the stories after the main text are read too
  - text boxes (`PlcftxbxTxt`, linked to the anchoring shape by its `lid`) at their anchor as a `textbox` section group, or as more paragraphs of the table cell they are anchored in (where the DOCX backend's Markdown loses them), a text box no anchor places at the end of the body
  - each section's headers / footers (`PlcfHdd`) as `page_header` / `page_footer` furniture
  - footnote and endnote bodies as `footnote` furniture (JSON / DocLang only, like the DOCX backend; the Pandoc AST appends them as notes)
- #574: a header's or footer's text boxes (the `ccpHdrTxbx` story through `PlcftxbxHdrTxt`, anchored by `PlcSpaHdr` at a CP of the header story) are `page_header` / `page_footer` furniture ahead of that part's own paragraphs, where the DOCX backend puts its `textbox` group (`doc_header_textbox.doc`)
- #588: inserted symbols (`sprmCSymbol` on the `0x28` placeholder, which used to print as `(`) and characters of runs set in a symbol font (`sprmCRgFtc0`/`sprmCRgFtc2` into the font table `SttbfFfn`) read as their Unicode glyphs through the same tables as the DOCX backend (`doc_symbol_font_glyphs`, `doc_symbol_font_glyphs_word` — the same Markdown as their `.docx` twins)
  - a font set by the style alone is not resolved
- #697: only the header/footer stories Word shows are read — the first-page pair with the section's `sprmSFTitlePage`, the even pair with the DOP's facing pages — as furniture `page header` / `page footer` sections of `text` items, the `.docx` twin's JSON shape (`doc_header_variants.doc`)
- #699: comments — the annotation story (`PlcfandRef` / `PlcfandTxt`, the reviewers from `GrpXstAtnOwners`, the dates from `AtrdExtra`) — are notes-layer `comment_section`s with docling's `[author: … (initials), time: …]: text` note and the `comments` back-reference on the paragraph holding the mark, as the DOCX backend's (`doc_comment.doc`)
- #700: Word 6/95 autonumbers (`sprmPAnld`, `sprmPNLvlAnm`) are list items — `N.` markers, roman and letter formats, bullets — printed inside table cells as the `.docx` rich cell prints them (`word6_lists.doc`, `word6_bullets.doc`; the `.doc` reader has no item tree, so a cell's JSON stays its text)
- #567: body text, headings, list items and the furniture stories are Markdown-escaped like every other backend's text nodes (docling-core's `serialize_run`: `_` → `\_`, then `& < >`) — `OBJ_DIR` no longer opens an emphasis span, the JSON export keeps the raw text, and table cells stay unescaped as in the DOCX backend
- #777: a table's columns come from each row's cell boundaries (`sprmTDefTable`'s `rgdxaCenter`, the `w:gridCol`s Word writes for the `.docx`), so a cell spanning several columns is one cell with its `col_span`, its text repeated in every column it covers as the DOCX backend's
- #824: a custom style based on Heading 1–9 or Title through `istdBase` (a corporate "SectionTitleL1") is a heading — `style_heading_level` walks the based-on chain as the DOCX backend reads `w:basedOn` (the first ancestor that is a built-in heading style or carries an outline level 0–8 gives the level; an explicit level 9, Word's body text, ends the walk), where before only the paragraph's own style was read
  - `doc_inherited_heading_styles.doc`: its two-hop style is a heading here where the DOCX backend prints docling's one-hop bold text
- #825: `sprmPHugePapx` (0x6646, 0x6645 in Word 97) is followed into the Data stream — a cell or row-end paragraph whose grpprl outgrows its 512-byte FKP page keeps only that sprm in the page, and without the PrcData it points at the paragraph lost `sprmPFInTable`, so one row's cells fell out as text between two one-row tables (`doc_huge_papx_table.doc`: one table and one paragraph, as its `.docx` twin)
- #828: a Word 6/95 list the paragraph *style* defines — "List Bullet" keeps the `sprmPAnld` in its PAPX UPX, the paragraphs holding only the `istd` — prints its items as list items: the stylesheet reader keeps the style's autonumber and a paragraph without one takes the first up its based-on chain (`style_anld`), as #641 does for Word 97's list references (`word6_style_lists.doc`, the `.docx` twin's bullets)
- #832: Word 6/95 inline pictures keep their image — `sprmCPicLocation` is read with its count byte (a bare long desynced the CHPX), the PICF is found in the WordDocument stream (a Word 6 file has no Data stream) and the WMF / EMF bits after its header render to the picture's PNG through `metafile::render`, `dxaGoal` / `dyaGoal` sizing a WMF without a placeable header (`word6_inline_picture.doc`, the `.docx` twin's PNG)
  - a WMF whose 18-byte header is written as 16 bytes (`mtNoParameters` dropped, the ticket's generator) is read from its first record, as LibreOffice reads it
- #851: `sprmCFRMark` / `sprmCFRMarkDel` (Word 6/95: opcodes 66 / 65) mark a run inserted / deleted
  - `track_changes=accept` leaves the deleted runs out and `reject` the inserted ones (`Keep new value.` / `Keep old value.` for a fixture that read `Keep oldnew value.`), `all` is refused (`ConversionError::UnsupportedOption`: the reviewer table is not read)
  - unset keeps every run's text, as before
- #852: a `HYPERLINK` field's instruction is read instead of dropped
  - `HYPERLINK "url"` links its result to the URL, `HYPERLINK \l "bookmark"` to the bookmark's destination (`#id`), `HYPERLINK "file" \l "bookmark"` to `file#bookmark` (the file is not opened)
  - any other field keeps its result as plain text, as before
- The bookmarks (`SttbfBkmk` names, `PlcfBkf` starts) become destinations with the DOCX reader's identifiers (#849: a heading's Pandoc auto-identifier, else the sanitized name, `-n` on a repeat, `_GoBack` skipped), as `Node::Anchor`s before the paragraph they start in (a cell's are not placed) — the Pandoc AST's `Header` id / `Span .anchor`
- An unresolved `\l` bookmark is a `DOCLING_RS_DEBUG` diagnostic and plain text
- The `0x01` data anchor Word keeps in a `HYPERLINK`'s instruction no longer prints a picture placeholder per link
- The link rides in the flat chain's Markdown text (`[Jump](#target-section)`), as the AbiWord reader's does — the JSON and HTML show that text

#### XLS (Excel 97–2004)

**Backend:** `xls.rs` (calamine BIFF8 + the XLSX region detection)\
**Status:** byte-identical to the XLSX backend on converted fixtures

- #634: number formats from the Workbook stream's `FORMAT`/`XF` records and the numeric cells' `ixfe` (`NUMBER`/`RK`/`MULRK`/`FORMULA`), applied by the same rules
  - the issue's `.xls` matches its `.xlsx` bar the date its writer stored as the built-in `mm-dd-yy` (ISO, as the rules say)

#### PPT (PowerPoint 97–2003)

**Backend:** `ppt.rs` (native [MS-PPT] + OfficeArt shape walker)\
**Status:** **byte-identical to docling's own `.ppt` output** (LibreOffice → its PPTX backend) on the sample fixture and the #627 decks

- tables reconstructed from shape-group geometry (spans included), bullet lists (StyleTextProp) and numbered lists (PP9 autonumber), titles, z-order
- #623: **version 4** compound files (4096-byte sectors — PowerPoint writes one when an edited `.ppt` is saved in place) used to fail as `damaged compound file`: the shared CFB reader placed sector 0 right after the 512-byte header, where v4 pads the header to a whole sector
  - it now reads them, also for `.doc`, `.msg`, `.wps` and the StarOffice/Quattro containers (`.xls` goes through calamine, which already did) — `ppt_cfb_v4_edit_save.ppt` fixture
- #627: docling reads a `.ppt` through LibreOffice's PPTX export, which writes every paragraph's *resolved* bullet, so the list markers here resolve the same way
  - a paragraph's own `StyleTextPropAtom` `fHasBullet` (only when its `hasBullet` mask bit is set; the other bullet-flag bits used to read as an explicit "no bullet")
  - else the bullet its master's `TextMasterStyleAtom` gives its text type and indent level (an unset level takes the one below it; center/half/quarter body start from Body, the centred title from Title — LibreOffice's `PPTStyleSheet`), the master being the one the slide's `SlideAtom.masterIdRef` names (PowerPoint 2007+ writes every layout as a master of its own)
- A master's numbering lives only in the PP9 extension, which LibreOffice does not turn into a list, so docling — and we — read such a body as paragraphs
- Shapes are read in docling's reading order (rows by top edge, 0.05" tolerance, left to right; a group by its frame), not drawing order
- On the #627 reporter's seven PowerPoint-saved decks (`crates/docling/tests/data/ppt/list_cascade/`) the Markdown is byte-identical to docling's (an untouched "Title and Content" body was paragraphs, a "Section Header" slide read its title first)
  - `powerpoint_sample.ppt` now matches docling's `.ppt` output too (its footnote line and rectangle text had been out of reading order)
- the same eleven decks as **LibreOffice** saves them (`list_cascade_lo/`, `soffice --convert-to ppt` — every paragraph's bullet written out, PP9 autonumbers for the numbered ones, a master per layout) are byte-identical to docling 2.137.0's Markdown on all 11/11 too
  - list items nest and number per text frame the way docling's PPTX backend does (its `_OpenList` stack, docling#4397) — a deeper indent level nests only relative to the list already open, so a level-2 numbered run after plain text is a flat list, a shallower item pops back, and a bullet joining a numbered group takes the group's numbering (`3.`, not `-`)
  - before, the indent level was the nesting depth outright and a bullet ended the numbered run

#### WebVTT

**Backend:** `webvtt.rs`\
**Status:** **4/4 exact, JSON 4/4 structurally identical to docling 2.129**

- the item tree carries each cue's `TrackSource` (`kind: track`, start/end seconds, cue identifier, `<v>` voice), the formatting inherited from `<b>`/`<i>`/`<u>` spans and an inline `WebVTT cue span` group per multi-span line
- cue blocks parse like docling-core's `WebVTTCueBlock` (identifier line, cue settings ignored, malformed timings drop the block)
- #366: karaoke cue timestamps (`<00:00:00.389>`) are stripped without splitting the span (docling-core#744)
  - text after a multi-line voice span stays in reading order (docling#4105)
  - bare CR / CRLF line terminators parse (docling-core#749, docling#4157)
- a span still open at the end of a cue is closed there and keeps its text, a stray end tag is ignored, a misnested cue is still dropped (docling-core#875, 2.102; #761 — `unclosed_spans.vtt`) — pinned by unit tests

#### EBCDIC (.ebc)

**Backend:** `ebcdic.rs` (native decode tables + copybook layouts, #252)\
**Status:** **3/3 exact** vs live docling 2.119 on the mirrored `ebcdic-parser`-derived corpus

- incl. the four-schema packed-decimal sample and `Decimal` rendering in fixed-point at every scale (`0.0000`, `0.0000000` — docling#4296 dropped `str()`'s `0E-7`, #466; `ola013k` groundtruth refreshed)
- layout via `ebcdic_layout` option (all surfaces) or the `<stem>.layout.json` sidecar (docling.rs convenience; upstream requires explicit backend options)

#### AFP (.afp)

**Backend:** `afp.rs`\
**Status:** **1/1 exact, JSON 1/1 identical** against upstream's committed groundtruth (docling#4177, #1034)

- a port of docling's minimal `AfpDocumentBackend`: MO:DCA structured fields (introducer extension and padding stripped, the same malformed-input checks and byte offsets), Begin/End Page as pages, the PTOCA Transparent Data strings of each presentation-text object (chained control sequences, a line per Begin Line or per Absolute Move Baseline to a new baseline), C0/C1 controls dropped
- text decodes as cp500 — the ebcdic backend's table — since code-page resources are not resolved; pages are `0 × 0` and items carry a zero bbox, as upstream
- unsupported content (IOCA / IM images, GOCA graphics, BCOCA barcodes, object containers, overlays, page segments) is counted and warned about once per kind; a resource's presentation text lies outside any page and is never read
- DocLang: docling-core 2.106's export raises on upstream's own document (it divides by the zero page size); ours prints the text with a page break between pages

#### Email (.eml, .msg)

**Backend:** `email.rs` (mail-parser) + `msg.rs` (native CFB/MAPI → RFC 822 projection, #251 — docling reaches .msg via python-oxmsg)\
**Status:** **4/4 exact** incl. both .msg fixtures and the opt-in `list_attachments` section

- `list_attachments`: docling 2.119's `EmailBackendOptions.list_attachments`, plumbed as lib builder / CLI `--list-attachments` / serve `list_attachments` / py kwarg / Node `listAttachments`
- `.eml` `Date:` now spells UTC as `+00:00` like Python's `isoformat()` (was `Z`)
- docling#4242/#4248 (2.128–2.129): header values are collapsed to one line of single-spaced text (`" ".join(value.split())` — subject, display names, attachment names/types)
  - a display name holding an RFC 5322 special (`()<>[]:;@,\"`) is quoted with `\` and `"` escaped so `Name <email>` parses back
  - CRLF / a lone CR in a body are normalised before the paragraph split
- #466 (docling#4295): a blank or whitespace-only `text/plain` part no longer wins over the real `text/html` beside it — only a part that produced text does
  - the HTML fallback now converts the part through the HTML backend and splits its Markdown into paragraphs (docling's `_convert_html_part`), instead of dumping raw markup
- **attachment payloads** (#561, a docling.rs extension — docling lists names only): `EmailAttachments` / `DocumentConverter::convert_email_attachments`, py `email_attachments()`, Node `emailAttachments()` list every attachment with a safe name, media type, size, inline flag and the `InputFormat` it converts as (extension → media type → signature), a forwarded `message/rfc822` part or embedded `.msg` as an `.eml` entry
  - references and OLE objects have no payload
  - `ArchiveLimits` bound what is kept
- #564: a text attachment's payload is the bytes as sent with only the transfer encoding undone (mail-parser's decoded body would re-encode a windows-1252 HTML file as UTF-8 and turn undecodable bytes into U+FFFD)
  - `inline` marks images of the body only (a PDF disposed `inline` is an attachment)
  - names are unique within the message (`report-2.pdf`) and shed bidi-format characters, a forwarded subject with `/` is one name
  - the `.msg` fixed-property scan reads each storage at its own header offset with the record type checked (a FILETIME's value bytes used to pass for the attach-method record about once in 11 000 attachments — "no payload")
- the bindings carry the detected format into `convert` (py `as_stream().format`, Node `format: att.format`), default their limits from `DOCLING_RS_ZIP_MAX_*` like the converter, and Node gets `emailAttachments*Async`

#### iCalendar

**Backend:** `calendar.rs`\
**Status:** **4/4 exact, JSON 4/4 identical** against upstream's committed groundtruth (docling#4880, #1032)

- a port of docling's `CalendarDocumentBackend` (`calendar/parser.py`, `values.py`, `render.py`): RFC 5545 content lines (parameters, quoted values, RFC 6868 caret escapes, TEXT unescaping, comma lists), the component tree with docling's containment recovery (a BEGIN the open component cannot hold closes it, a stray END is ignored), dates / durations / recurrence rules in words (a rule with a part the description cannot carry stays raw), all-day ends exclusive, `Z` over a TZID
- folds are joined on the bytes before decoding (a fold may split a UTF-8 character) unless the input is UTF-16/32 — a wide BOM, or a requested `encoding` that is not ASCII compatible — which is decoded first and unfolded as text
- the JSON is docling's item tree (`title`, `section_header` levels 1/2, `text` fields, `list` groups named after their heading); the flat Markdown prints `& < >` verbatim, as upstream's groundtruth does
- detection: `.ics` `.ical` `.icalendar` `.ifb`, `text/calendar`, and a nameless input whose first non-blank line is `BEGIN:VCALENDAR` (UTF-16 behind its BOM too); a named `.txt` keeps its own route

#### EPUB

**Backend:** `epub.rs` → HTML backend\
**Status:** **1/1 exact, JSON 1/1 structurally identical** through the HTML item tree

- #366: manifest hrefs are percent-decoded before the archive lookup (docling#4199, `chapter%201.xhtml` → `chapter 1.xhtml`)
- docling#4261 (2.129): the spine path is `posixpath.normpath(join(opf_dir, href))`, so an href stepping out of the OPF directory (`../Text/ch1.xhtml`) resolves to the archive entry it names
- the flat Markdown path now applies docling's heading rule (`to_single_text_element`: parts joined by spaces, the first formatting found wrapping the whole heading, `## *To the Hibernia*`) and merges adjacent same-annotation text nodes into one run (`simplify_text_elements`: `<b><time>… <abbr>p.m.</abbr></time></b>` is one bold run)
- #466 (docling#4351): a content document that opens with a UTF-16 byte order mark is decoded as UTF-16 instead of being dropped
- docling#4293 (2.130.0-33): cross-document links of every content-document extension (`.xhtml`, `.xht`, `.htm`, `.html`) become anchors; a link with a scheme or `//host` is left alone
- #1027 (docling#5170): a single-quoted cross-document link (`href='ch2.xhtml#n2'`) becomes an anchor too, keeping its quote; a single-quoted absolute URL keeps its host
- #1027 (docling#5168): `container.xml`'s rootfile `full-path` is percent-decoded like the manifest hrefs, so `OEBPS/my%20book.opf` reads `OEBPS/my book.opf` instead of failing the conversion

#### ODF (odt/ods/odp)

**Backend:** `odf.rs`\
**Status:** vs docling 2.148.0 (#997, #1016): **7/7 exact**, JSON 6/7 — `text_document_03`'s formatted table cells (docling#4809) now match; its JSON differs only in its picture's alt text, which we also carry as a `description` annotation and upstream's groundtruth leaves out; the runs keep their hyperlink-boundary whitespace (`belief, **Lorem Ipsum…**.`). Before: **7/7 exact**; **JSON: structurally identical to docling 2.129's on 7/7** (`odf_presentation_01` / `_02` against docling 2.142.1, #862)

- #862 (docling#4869, 2.141): an ODP slide's speaker notes (`<presentation:notes>`) are items on the **notes** layer under the slide's group, after the slide's content — the notes page's text boxes walked like a slide's (headings, paragraphs, lists), its thumbnail and slide-number / date / header / footer placeholders skipped. Exports leave the layer out by default, so the Markdown is unchanged; `odf_presentation_01`'s three notes reach the JSON (docling 2.129 dropped them). Both presentations' groundtruth is refreshed from docling 2.142.1 — besides the notes, its Markdown now repeats a merged cell's text in every slot it spans (docling-core#857), as ours already did, and `_02`'s picture is PIL's newer PNG encoding.
- #527: `odf_presentation_02`'s JPEG is written as the PNG PIL re-encodes it to — pixel-identical — and the flat nodes carry the package's pictures too, so DocLang names and packages them
- `odf_tree.rs` ports `opendocument_backend`'s construction call for call over `odf.rs`'s style/run/list/cell readers:
  - a paragraph of several formatting or hyperlink runs (`_normalize_odf_text_runs`: same-format neighbours merged, whitespace at a hyperlink boundary stripped, blank edge runs dropped) is an `inline` group of `text` items
  - a heading/title of mixed runs an empty block over such a group
  - a list a `list` group whose items carry `enumerated` and `{counter}{num-suffix}` markers (`.` by default), continue across sibling lists and nest under the previous item
  - a table's cells all recorded (empty ones too) with spans and `column_header` on row 0
  - a cell holding lists, several paragraphs, images or a nested table a `RichTableCell` whose content walks into a `rich_cell_group_{tables−1}_{col}_{row}` group under the table
  - a typed cell's text `str(cell.value)`
  - an embedded chart a classified picture with `tabular_chart` data
  - a bitmap part a picture *with* its `image` payload (the package's `Pictures/` parts are decoded for the tree; the flat nodes keep their unloaded placeholders)
  - a presentation one `chapter` group per slide (`slide-{n}`, the slide name as `title` when no element is one)
  - a spreadsheet one `section` group `sheet: {name}` per sheet (`invisible` layer when hidden) with every 4-neighbour data region a table carrying its cell-range provenance (`TOPLEFT`, `charspan [0, 0]`) and the sheet a page sized `right − left` × `bottom − top`
- The JSON export also reproduces docling-core's `validate_document` — the pydantic validator every `DoclingDocument` passes through when docling wraps it in a `ConversionResult` — which clamps every provenance box (and a one-page table's cell boxes) into `[0, width] × [0, height]` of its page *in place*
  - a spreadsheet region whose page was sized `right − left` × `bottom − top` therefore loses its offset in docling's own JSON (`odf_table_with_title_01`: `(1, 3, 3, 10)` → `(1, 3, 2, 9)`), and so does ours
- `full_conformance.py` measures against a document wrapped the same way on the native files — docling 2.120.3 parity:
  - #320: `<text:a>` hyperlinks render as Markdown links with docling's target classification and boundary-whitespace rule (docling#3949); a `draw:image` whose `xlink:href` is not a package part is never read from disk and yields no picture unless it is an `http(s)` URL fetched under `fetch_images` (docling#4015) — the phantom placeholder text_document_02.odt used to emit is gone
  - slide-title/name headings, shape text, speaker-notes drop, chart classification + data tables, merged-cell semantics (plain repeat vs rich dedup), and text after inline elements (docling 2.115's tail fix, #255 — the old run-tail dropping quirk is gone on both sides)
  - sections walked (docling#3852), dangling `draw:object`s skipped (docling#3876)
- #466 (docling#4375, ODT): a `text:note` no longer splices its citation and body into the citing sentence — the body is a furniture-layer `footnote` item after the body walk (its descendant text concatenated, as odfpy's `text_content`), out of the Markdown like headers/footers (own `footnotes.odt` fixture)
- Ported from docling 2.138–2.140 and checked document by document against it (`tests/data/odf/upstream`, `odf_upstream`):
  - a hidden section (`text:display="none"`), hidden slide (`presentation:visibility="hidden"`) or hidden sheet is kept on the invisible layer, in Markdown too (docling#4767, docling#4860, #753)
  - a picture in an ODS cell is added once, in its rich cell, and the pictures anchored to the sheet (`table:shapes`) are kept (docling#4678, #758)
  - a blank merged range none of whose rows holds a value is a separator: it no longer joins the tables above and below it into one (docling#4992, docling `main` after 2.142.1); every other merge keeps continuing its table
  - a list item holding only an image, a chart or a note keeps its number and its nested list, with an empty text (docling#4693, #759), and a marker other than `N.` (`1)`) prints behind a bullet as docling-core does (`- 1) text`)
- #823 (docling#4750): a `<text:table-of-content>` and the other index containers (`alphabetical-index`, `illustration-index`, `table-index`, `object-index`, `user-index`, `bibliography`) are transparent like a section — their `index-body` / `index-title` children walk as body blocks in document order (the TOC title and entries, and whatever LibreOffice left inside the index body converting a Word file with an unclosed TOC field: headings, paragraphs, tables), a hidden one on the invisible layer
  - the `*-source` template is not content (`odt_toc_index_body.odt`, its `.docx` twin's Markdown byte for byte)
- #826: a paragraph numbered through its paragraph style's `style:list-style-name` — set on the style or inherited through `parent-style-name`, an empty value cancelling it; ODF 1.3's `style:list-level` for the level — with no `<text:list>` around it is a list item: one list per run of consecutive paragraphs of the same list style, the level's `start-value` / affixes applied, `list` groups of `list_item`s in the JSON
  - **Beyond docling**, whose ODT backend reads `<text:list>` only (its DOCX backend reads the same list from the style's `w:numPr`): the `.docx` twin's output is the reference (`odt_style_list_name.odt`, byte-identical Markdown)
- #827: a `<text:h>` inside a `<text:list-item>` — Writer's outline numbering, one nested list per outline level — is a heading of its `outline-level`, outside the list (an item of headings alone is no list item; a list of them opens no `list` group; a heading item among bullets leaves the bullets a list); the number the outline list would print is not added
  - **Beyond docling**, whose `_odf_list_item_content` reads the heading as the item's text: the `.docx` twin's `##` / `###` headings are the reference (`odt_headings_in_list.odt`, byte-identical Markdown)
- #829 (docling#4436, 2.140): `<office:annotation>` comments stay out of the body text and are notes-layer `comment_section` groups `comment-{n}` — one per thread (a reply's `loext:parent-name` joins its parent's), created right after the item made from the paragraph holding the thread's first comment and parented to it, one `text` per comment with docling's `[author: Name (initials), time: date]: text` note, the item's `comments` pointing at the group (a plain cell's under the table without one, an empty paragraph's on the root at the end)
  - DocLang writes each as a text on the furniture layer (DocLang's closest to notes, docling-core#800), Markdown / LaTeX leave them out (`odt_annotations.odt`, its twin's Markdown byte for byte)
- #830: the master pages' headers and footers (`styles.xml` `<office:master-styles>`; a flat file's own DOM) — every shown `style:header` / `style:footer` and `-left` / `-first` variant with content, master page by master page, headers first — are furniture after the body: a furniture-layer `page header` / `page footer` section per part in the JSON holding its paragraphs, tables and lists, furniture-wrapped nodes in DocLang, nothing in the Markdown
  - **Beyond docling**, whose ODT backend never reads master pages (its DOCX backend's `_add_header_footer` is the shape followed): `odt_master_page_header_footer.odt` converts to its twin's furniture sections
- #831: a `<draw:text-box>`'s blocks — paragraphs, lists, tables — are items after the anchoring paragraph (a `textbox` section group in the JSON, the DOCX backend's shape), no longer run into its text with no separator, a box in a block-level frame included, a box in a box with the outer box's paragraph
  - a table in a box stays a table, where the DOCX backend flattens it to its cells' paragraphs (docling's `.//w:p`)
  - **Beyond docling**, whose ODT backend reads the box as the paragraph's text (`odt_text_box.odt`, its twin's Markdown but for that table)
- #851: the reader gives the accepted view (its `text:tracked-changes` region skipped, deletions left out)
  - `track_changes=accept` names it, `reject` / `all` are refused (`ConversionError::UnsupportedOption`) rather than answered with it
- #1016 (docling#4809): a table cell (ODT, ODS, ODP) with a run that has formatting (bold, italic, strike, underline, a script) or a hyperlink is a rich cell, even when it is a typed one-paragraph cell — `text_document_03`'s `**bold**`, `~~strikethrough~~`, `*italic*`, `x`+superscript `2` and `H`+subscript `2`+`O` cells keep their runs (Markdown matches upstream's groundtruth; JSON cell for cell); a span whose style sets no formatting stays plain; a table nested in a rich cell is still flattened to its cells' plain text, as upstream prints it

#### JATS

**Backend:** `jats.rs` (roxmltree)\
**Status:** vs docling at `bcb3678` (#997, #1015, #1016, #1010): **2/7 Markdown exact, 7/7 after whitespace normalization, JSON 7/7** — the inline whitespace matches throughout (`*B*. *malayi*`, `(CO2)`), and so do image tables, reference links, formatted table cells and table footnotes; the Markdown differs only in blank lines (`pntd.0008301`, `pone.0234687`) and in the three `elife-56337` copies' one-run footnote, both divergences below. Before, vs docling 2.142.1: **4/7 Markdown exact, 7/7 after whitespace normalization**; **JSON 7/7 structurally identical** — the three `elife-56337` copies differ in how a footnote of one run prints (below)

- docling#4826 (2.142): a `<sub-article>`/`<response>` — an eLife decision letter, the author response — converts like a `<sec>`: its body and back matter under a heading with its title, a nested one under the heading of the one that holds it; they were dropped, and the article's own body and back are no longer looked up inside one
- docling#4361 (2.142): an abstract's `<p>`s are walked as body paragraphs, so their italics, bold, sub- and superscripts stay (`*Yersinia pestis*`, `CO` + subscript `2`) — one text item or inline group per paragraph, where they were one flattened text item; a structured abstract's plain paragraphs come first and are kept beside its sections (they were dropped); a footnote keeps its runs, label and styling (`footnote` items, an inline group when there are several). docling-core joins an inline group's runs with a space, so the Markdown reads `*in vitro* .` and `CO 2` as upstream's does
- every run's XML line breaks and the indentation around them collapse to one space (`_normalize_xml_line_breaks`), where a line break became a space and kept the indentation after it
- **Divergence (Markdown):** a footnote of one run prints on its bullet (`- No competing interests declared.`); docling 2.142 prints an empty bullet with the text on the next line, its list item being empty with the text item under it — the JSON is the same
- #1015 ports three content fixes (docling `bcb3678`):
  - docling#5078: a `<table-wrap>` holding a `<graphic>` (direct or in `<alternatives>`) and no `<table>` — a table published as an image, as PMC often ships them — is a picture with its label + caption, like a `<fig>`, and its image is embedded under the same `fetch_images` policy; the caption and the table used to vanish together. A wrap with both cells and a graphic stays one table
  - docling#5177: an inline `<term>` keeps its text (`The <term>PCR</term> step` was `The step`), in headings, paragraphs and captions alike
  - docling#4813: a `<mixed-citation>` is walked like a paragraph, so an `<ext-link>` keeps its target and `<italic>`/`<bold>` their styling — one run stays a single list item (with its formatting / link), several become an empty list item holding an inline group. Six references in `pntd.0008301`, `pone.0234687` and `ptag100` gain their links, as in upstream's groundtruth
  - an `<ext-link>`'s scheme and host fold to lower case as pydantic's `AnyUrl` stores them (`http://Srtm.Csi.Cgiar.Org/` → `http://srtm.csi.cgiar.org/`); the link text keeps its case
  - the runs keep their boundary whitespace (the inline contract), so the Markdown reads `ttps://[www…](…)` as upstream's does
- #1010 (docling#4818): every `<fn>` directly under a `<table-wrap-foot>` is one `footnote` item of its table — an empty item under the table, listed in its `footnotes` in source order, holding an inline group of the note's runs (`footnote` runs, an inline formula a `formula` run; labels, `<p>` boundaries, styling, sub/superscripts and `<ext-link>` targets kept). A note with no text is skipped, a bare `<p>` in the foot stays dropped, and the article-level `<fn-group>` is unchanged. Markdown, HTML and LaTeX print the notes after the table (`pntd.0008301`'s `EVI = enhanced vegetation index; …`, `pone.0234687`'s `aCO2e, carbon dioxide equivalent.`), and the docling-JSON reader reads such a note from its runs. docling's 21 tests are ported (`jats_table_footnotes.rs`), except the label-filtered exports, which our exports have no parameter for.
  - **Divergence (Markdown):** for a table with several notes, docling-core first prints the notes' own empty texts joined by blank lines (its `serialize_footnotes`), then each note from its runs; we print the notes alone, so `pntd.0008301` and `pone.0234687` are short of those blank lines and nothing else.
  - **Divergence (LaTeX):** a note's superscript or subscript prints as plain text (`sup` where docling writes `$^{sup}$`) — the flat note carries Markdown, which has no script markup; the JSON and HTML keep the script.
- #1016 (docling#4809): a table cell with a styled, script, linked or inline-formula run is a `RichTableCell` — its runs in a `rich_cell_group_{T}_{col}_{row}` group under the table (one item, or an inline group), the JSON cell keeping its plain text; a `<p>`/`<list-item>` boundary inside the cell keeps a space; an unmapped tag (`<named-content>`) or a blank script leaves the cell plain. Markdown prints the runs (`*W*. *bancrofti* &amp; mixed`, escaped like body text), DocLang gets them as the cell's content. Every table row of `elife-56337`, `pntd.0008301` and `pone.0234687` now matches upstream's groundtruth, and the rich cells match its JSON cell for cell

- #391 ports docling#4172, unreleased: a **structured abstract** keeps its `<sec>`s — where we flattened every section into one `Title: text` paragraph
  - each a heading one level below the abstract's (`hlevel + 2`, the abstract's being `hlevel + 1`, both with `hlevel` still 0 since the abstract precedes the body walk) with one text item per `<p>`
  - an untitled section's paragraphs directly under the abstract heading
  - a sections-only abstract with no plain text item
  - a section without paragraphs dropped
  - a nested `<sec>` not descended into
- the abstract's own heading is its first `title` **or `label`** child (we read `title` only, so a label-only abstract fell back to "Abstract" for us and not for docling)
- the new `pmc2231364` fixture mirrored with its groundtruth
- The nested-list half of docling#4172 does not apply: it wraps a sub-list in a LIST group to silence a docling-core deprecation warning, and our node model carries nesting in the item's level, not in parent refs — Markdown is unchanged there
- **Known upstream quirk, replicated:** docling's `_parse_title` joins `elem.text` of each `title-group` child (the text before its first child element), so an `<article-title>` with inline markup is cut at the markup — `pmc2231364`'s title comes out as `# Global transcriptional response of`, as in upstream's groundtruth
- We used to emit the full descendant text; `parse_title` now matches docling byte-for-byte (a one-line switch back to `node_text` if upstream fixes it) — no other fixture has markup in its title, which is why it never surfaced
- #392 ports docling#4041 (unreleased): under `fetch_images` a `<fig>`'s `<graphic xlink:href>` — direct or inside `<alternatives>`, in document order — is read from the source **file's** directory and embedded (JSON `image`, `--images embedded`; Markdown keeps the placeholder marker), where `add_figure` hardcoded no image and the XML arm was the one image-bearing declarative backend the converter never handed `fetch_images`
- docling's contract is kept:
  - an absolute href is refused with a warning but a later relative rendition still counts
  - a candidate resolving outside the directory aborts the whole figure
  - an extensionless href is probed with `.jpg .jpeg .png .tif .tiff .gif`
  - `.svg` and remote hrefs are skipped
  - an undecodable file warns and falls through
  - existence is checked before decoding so probe misses do not warn per suffix
  - a figure that resolves nothing warns once
- An in-memory source or one fetched from a URL embeds nothing (docling's `_load_figure_image` needs a local `base_path`); docling's `source_uri` maps onto `SourceDocument.path`
- docling's separate `enable_local_fetch` has no counterpart — our one `fetch_images` flag is the opt-in, as for HTML/AsciiDoc
- Upstream's twelve temp-dir cases are unit tests here
- (the stored `.md` predates the 2.96 header flattening and uses compact tables)
- #364:
  - docling#4029 `<ext-link xlink:href>` hyperlinks on inline runs (`[text](url)`, runs coalesce only with equal formatting *and* link, blank hrefs ignored, URLs normalized like pydantic's `AnyUrl`)
  - the new `ptag100.xml` fixture (docling#3726) mirrored
  - block `<tex-math>` emitted as a formula item so multi-line `$$…$$` bodies keep plain newlines
  - a no-break space inside a citation kept verbatim (only ASCII whitespace collapses)
- `walk_linear` threads docling's `parent` through the item tree:
  - the title item is the root (an empty one when the article has none)
  - authors / affiliations / the abstract heading hang off it
  - a `<sec>` heading parents what follows — including, as in upstream where `new_parent` is set once per element, the *siblings* after the section (`ptag100`'s back-matter paragraphs under `Funding`)
  - a `<list>` is a `list` group whose items hold their nested lists as further groups
  - a paragraph of several runs an `inline` group of `text`/`formula` items with `formatting` and `hyperlink`
  - figure and table captions body-level `caption` items the picture / table references
  - a `<fn-group>` a heading over a `footnotes` list of empty items each holding an inline group with the `footnote` text
  - a `<ref-list>` a level-1 heading over a `list` group of citation items
- an XML-looking `.txt` whose DOCTYPE names a JATS DTD converts through this backend (docling's detection reads it as `application/xml`), so `elife-56337.txt` counts too
- #466 (docling#4272): a citation `<name>` with only a `surname` or only `given-names` is kept (the present parts, space-joined) rather than skipped, and an empty `<lpage/>` adds no `–`
- #768 (docling#4770, 2.138): a `<list>`'s `list-type` numbers its items
  - `order`, `alpha-lower`/`-upper`, `roman-lower`/`-upper`, trimmed and in any case, make every item `enumerated` (Markdown `1.` `2.`, DocLang `<list class="ordered">`)
  - anything else is a bullet list
  - a nested list goes by its own attribute
- a `<list-item>`'s `<label>` is its `marker`, no longer merged into the text, and prints as docling-core's list serializer prints a marker (a `\d+.` label verbatim, any other label with a letter or digit behind a bullet as `- (a) first`, DocLang in the item's `<ldiv>`)
- the item text joins its `<p>` children only, so loose text beside them is dropped as in docling
- Verified against docling 2.138.0 on 19 constructed cases (Markdown, JSON, DocLang); the corpus has bullet lists only and is unchanged
- One cosmetic difference: a `*` or `+` label, which docling-core keeps as the bullet, prints as `- text` here, since the shared serializer always bullets with `-`

#### USPTO

**Backend:** `uspto.rs`\
**Status:** **9/9 exact vs docling 2.129, JSON 9/9** — was 1/5

- the JSON tree is the one docling's SAX handlers (`PatentUsptoIce`/`GrantV2`/`AppV1`) build:
  - the title opens `parents[2]`
  - a `<heading level="N">` sits at level N+1 (or the lowest known level) and parents what follows
  - `ABSTRACT` and `CLAIMS` are level-2 headings under the title with `paragraph` children that leave the level alone
  - a `<table>` is a placeholder recorded at `</table>` under the current parent and filled by index from a regex re-parse of the raw text (all tables stay empty when the counts differ, as upstream)
  - each spanning `<entry>` listed once per covered column with the whole span on every replica
  - rows padded to the widest tgroup
- docling#5202 (#1028): ISO 8879's `&sgr;` is the small sigma σ and `&sfgr;` the final ς (`&sgr;` read as ς, and `&sfgr;`, unknown, dropped its letter); no corpus patent uses either
- the legacy APS plain-text patent (`pftaps*.txt`) goes through a port of docling's `PatentUsptoGrantAps` (title, `ABSTRACT`, `PAC` caption headings, paragraphs, `CLAIMS`; Markdown, JSON item tree and `.dclx` all exact)
- the last three XML differences were docling's table and claim mechanics, now ported:
  - CALS cells the unified tgroup span pushes past the widest tgroup's column count are clipped like docling-core's `TableData.grid` (`pa20010031492`'s 3 columns, `ipa20110039701`'s 4)
  - a table's cells lose their named entities because docling re-parses each `<table>` with lxml, which knows only the XML built-ins (`ZEOCIN&thinsp;&trade;` → `ZEOCIN`, the built-ins after an undefined one in the same text run dropped too)
  - a `<p>` wrapping `<tables>` keeps its own text as a paragraph after them (`Table 2: …` captions with their substituent lists)
  - ICE claims are one paragraph per `<claim>` (nested `<claim-text>` pieces joined by spaces) with an in-claim table recorded before the CLAIMS heading
  - PATDOC claims reproduce the SAX handler's spacing (raw `<PDAT>` concatenation, one space per `</PARA>`, `PARA`-level whitespace riding into the next `<PDAT>`)
- `tables_ipa20180000016.xml` fails in upstream itself
- Earlier: **1/5 exact (2/5 whitespace-normalized)** on the sources live docling converts — it errors on the other 5 (those are validated byte-exact via `.dclx`), and its APS-text *Markdown* export is empty where ours emits the text dump (the `.dclx` matches exactly — §5)

#### XBRL

**Backend:** `xbrl.rs` + `xbrl_dts.rs`\
**Status:** **2/2 Markdown exact, 2/2 JSON identical** vs upstream groundtruth (the upstream test passes the fixture's `*-taxonomy` dir)

- docling's `XbrlDocumentBackend` without arelle (#466):
  - dei facts → title
  - `textBlockItemType` facts (typed from the taxonomy schema, `…TextBlock`/markup by shape when it is out of reach) → HTML, each block converted with `infer_furniture=False, add_title=False` and merged the way `DoclingDocument.concatenate` numbers items (traversal order, its rich-cell ref quirk included)
  - numeric facts → the `key_value_items[0]` `GraphData` (key cell + value/period/unit/decimals cells, `to_child` concept hierarchy from the presentation linkbase, `summation-item` arcs with `weight:` cells)
- The taxonomy is discovered offline from the `xbrl_taxonomy` directory (schemaRef → xsd → imports/linkbaseRefs; remote URLs through taxonomy packages' `META-INF/catalog.xml`)

#### YouTube URL

**Backend:** `youtube.rs` (the URL fetch + the WebVTT backend)\
**Status:** **docling.rs extension (#681)** — docling has no YouTube reader (MarkItDown's `_youtube_converter` has, over `youtube-transcript-api`)

- HTML fetched from a `youtube.com/watch?v=` / `youtu.be` / shorts / embed / live URL is routed by that URL
- `ytInitialPlayerResponse` gives the title, views, keywords, runtime, description and the caption tracks
- the track for `asr_lang` (English, else the first; written over auto-generated; YouTube's `tlang` translation when the video has no such track) is fetched as WebVTT from `timedtext` and read by the WebVTT backend — timed items, `--to vtt` round-trips them
- auto-generated tracks lose their rolling repeats and word-timing tags
- MarkItDown's shape (`# YouTube`, `## title`, `### Video Metadata`, `### Description`, `### Transcript`)
- a video without captions is an error
- Fixtures: `tests/data/youtube` (hand-written to the page's shape — YouTube answers 429 to automated requests), served locally in `tests/youtube.rs`

#### RIS legal XML (`risdok`)

**Backend:** `ris.rs` (roxmltree)\
**Status:** **docling.rs extension (#902)** — docling has no RIS reader (a `risdok` file saved as `.xml` came out as one `xml` code block, before #671 empty from the JATS reader); no upstream reference exists, so `crates/docling/tests/data/ris/` pins our own output for a public decision of the Austrian data-protection authority (`DSBT_20250904_2025_0_699_550_00.xml`, RIS open data), and docling-core 2.104 loads the JSON and writes the same DocLang from it

- the open-data documents of `ogd.ris.bka.gv.at` (decisions, federal law): a `.xml` whose root is `<risdok>` (namespace `http://www.bka.gv.at`, or none) by the `xml` route's dialect sniff, or the `xml_ris` format id
- in source order, under one `section` group per `abschnitt` (named `abschnitt {nr}`) so every item keeps the section it came from:
  - `ueberschrift` → section header (`titel` / `g1` level 1, `g2`…`g4` deeper)
  - `absatz` → paragraph; `b` / `i` / `u` / `sup` / `sub` keep their formatting (an inline group in the JSON), a `feld` / `span` reads as its text, a `tab` as a space
  - a bullet `absatz` — a glyph before its first `tab` (`-`, `•`, Word's Symbol-font `U+F0B7`, …) or none in a hanging indent — is a list item, consecutive ones one list; numbered paragraphs (`1.1.`, `3.1.10.`) stay paragraphs, the number part of the legal text
  - `table` → table (`colspan` / `rowspan`; Word's zero-height layout row dropped)
  - `kzinhalt` / `fzinhalt` (running header / footer) → `page_header` / `page_footer` furniture, out of the Markdown
  - `metadaten`, `layoutdaten`, `abstand` skipped; an unknown element read through
- JSON and HTML from the item tree, DocLang too (docling-core's serializer over the tree, #884); Markdown from the flat nodes
- #909: the item tree keeps the source's blocks as the HTML backend does (#903, #910, #911): `ItemTree::blocks` — every element that produced an item, every element with text of its own, every empty `absatz` / `ueberschrift` / cell — with the tag, every attribute (`typ`, `ct`, `halign`, an `abschnitt`'s `nr`), the path `risdok/nutzdaten[1]/abschnitt[1]/absatz[2]`, the normalized text with the inline runs' (`b`, `span`, `feld`) ranges; `TreeItem::block` on every item but the list group a run of bullet paragraphs shares; an element under `metadaten` / `layoutdaten` / `abstand` a block the reading skips (`Hidden`), a cell's text in its table's grid (`InTable`). `source_text([i, j])` reads selected elements with one `\n` between them
- **Divergences from docling's conventions** (no upstream to follow):
  - the Markdown escapes `*` (`\*`) besides docling's `_` and HTML specials — RIS pseudonymizes with stars (`A*** GmbH`, `FN *0*271*h`), which would otherwise turn into emphasis; the plain-text export reads `\*` as a star (a literal `\*` elsewhere — an AbiWord field code — now prints as `*` there too, which is what its Markdown renders)
  - a styled run's markers hug its text (`„*Werbeagentur*“`) — what docling-core 2.106's faithful inline join does too (#997; it used to join an inline group's runs with spaces)

#### RSS / Atom / RDF feeds

**Backend:** `feed.rs` (roxmltree + the HTML backend)\
**Status:** **docling.rs extension (#678)** — docling has no feed backend (a feed saved as `.xml` went to the JATS reader and came out empty)

- MarkItDown's `_rss_converter` shape:
  - the feed's title as the title
  - its description / subtitle as a paragraph
  - then per item a level-2 heading, a `Published on:` (`pubDate`, `dc:date`) / `Updated on:` (Atom `updated`) line and the body
- the body — through the HTML backend, relative links and image sources resolved against the item's `link` (or `xml:base`, or the feed's link):
  - RSS `description` then `content:encoded`
  - Atom `summary` then `content` by its `type` (`text`, `html`, `xhtml`; a binary type skipped)
  - RDF `description`
- RSS 1.0 (RDF), which MarkItDown rejects, reads too
- the articles an item links to are not fetched
- (`feed/README.md`: the Rust and Go blogs' Atom, W3C's RSS, Debian's security RDF, MarkItDown's `test_rss.xml`)

#### JSON-docling

**Backend:** `docling_json.rs` (serde_json)\
**Status:** Measured by re-serializing docling's *own* documents (`tests/data/pdf/groundtruth/*.json`) and diffing against docling-core's render of the same file: **15/15 byte-exact** (docling-core 2.103.1 on the docling-parse 7 groundtruth refreshed for #921; 14/14 against 2.96 before; #384 took it from 479 diff lines to 19, #385 to none — the last 19 were the Markdown serializer's guessed list boundaries)

- #390: a caption's parent survives the round trip — on its picture/table (a PDF conversion), on the item's container ahead of or behind it (a chart, an HTML figure table), or on the body
- #403: a picture's `image` (docling's `ImageRef`) comes back with the document
  - a `data:` URI decoded to the picture's bytes
  - a *referenced* image (a path or `file:` URL, as `--images referenced` writes them) read from disk relative to the JSON file
  - so `--images embedded`/`referenced` work on docling-JSON input as they do on the original document, and a JSON→JSON round trip keeps the image
- an unreadable reference degrades to the `<!-- image -->` placeholder
- #975: a picture's description and classification come back with the document — `meta.description` (`text`, `created_by`) and `meta.classification.predictions`, else the deprecated `description` / `classification` annotations an older document carries alone — so a described document fed back in chunks on its descriptions and re-exports with them. This is the path the Python `docling_rs.chunking` chunkers take (a `DoclingDocument` is serialized to docling-JSON and re-imported natively), so a document described by docling-serve and chunked client-side used to lose its pictures' text: the chunks carried the captions only
- #899: `prov` and `pages` survive the round trip — the JSON export of a docling-JSON input carries every item's page, box (in its own `coord_origin`) and charspan and the document's page map, and the DocLang export the `<location>` tokens and `<page_break/>`s docling-core derives from them (`multi_page.json`: byte-identical to docling-core 2.104's render). Markdown is unchanged: the page markers render nothing
  - a list item's box rides the DocLang 512 grid (the serializers group a list from adjacent items, which a wrapper would split): it comes back within a grid cell (under a point on A4) with charspan `[0, len]`; a list that runs onto another page keeps that page's items in the list, without a box
- #1014: a reference written by field name, `{"cref": …}` (docling-core's `RefItem` accepts both, `populate_by_name`), reads like `{"$ref": …}` — such files (`table_footnotes_496.json`, docling-core's test documents) converted to an empty document; a linked text item keeps its `hyperlink` on the item, so the HTML export of a docling-JSON input writes `<a href>` instead of the Markdown link spelled out (Markdown unchanged; a bold/italic/struck link keeps the Markdown form)
- #1042: a table whose cells state `column_header: false` everywhere reloads without a header — row 0 used to be promoted to one, so the reloaded document's triplet chunks and its re-export differed from the original's; a JSON without the keys keeps the row-0 default
- Reads docling's native JSON back into the `Node` model ($ref body tree walked through text items' `children` too — docling nests a section's content under its heading, #362 — formatting, list nesting, table grids)
- A caption leads its picture and its table and trails its code block (a `CodeItem` is docling's only floating text item)
- a marker docling-core already considers valid Markdown prints verbatim, so a reference list continuing over a page break keeps its numbering
- any other marker forces a bullet and is kept in front of the text (`- (1) Human Annotation`)
- an unmarked item is numbered by its position among the group's children, nested groups included
- checkbox items, unclaimed caption items and `$$`-wrapped formulas render, and code and formulas are left unescaped

#### DocLang (`.dclg`/`.dclx`)

**Backend:** `doclang.rs` (roxmltree)\
**Status:** **14/15 Markdown exact (15/15 whitespace-normalized), JSON 15/15 structurally identical** vs docling 2.148.0 / docling-core 2.106.0 (DocLang 0.9); upstream's own `multi_captions_footnotes[_top_level]` fixtures and the reading cases of its `test_inline_whitespace_contract.py` in `crates/docling/tests/suite/doclang_{upstream,reading}.rs

- docling-core ≥ 2.93 serializes a `<field_region>` / `<field_item>` to nothing (the `<!-- missing-text -->` placeholders per container are gone, only each item's key and value remain)
- a rich cell's `<text>` is serialized as Markdown *before* the cell flatten, so a `<content>` newline is a GFM hard break and lands as three spaces in the table (`Rich cell   A nested table`) — `docx_rich_cells`/`kvp_data_example` groundtruth refreshed
- measured reading the same archives back (`tests/data/doclang`)
- the inverse of the `.dclx` output serializer, incl. docling's round-trip losses (list-item formatting, hyperlink targets)
- `doclang_tree.rs` ports docling-core's `DocLangDocDeserializer` call for call on the same DOM:
  - every mixed (or empty) `<text>` an `inline` group of per-run items with `formatting`
  - list items as plain text / `<text>`-wrapped / *virtual text* per its `_parse_list` cases (marker `""` unless an `<ldiv>` carries one)
  - OTSL cells with spans and per-cell `<location>` boxes
  - rich cells as `unspecified` groups under the table
  - captions and footnotes as body-level items created before their float
  - a `<label>` as the picture classification (`confidence: 1.0`) and a `<tabular>` as its chart data
  - `<src>` images read from the archive
  - `<location>` quartets as `prov` on the 512×512 page grid with one page per `<page_break>`
- CDATA sections are re-split the way minidom sees them (blank pieces skipped, verbatim otherwise)
- DocLang 0.8/0.9 (docling-core 2.106):
  - the reading rules run first, as the deserializer's `_apply_whitespace_rules` does (`docling_core::doclang_apply_reading_rules`): each text node then holds its reading — a whitespace sequence one space, none at a text block's edges, `<content>` literal, only space/tab/CR/LF whitespace (a no-break space at an edge stays) — and both walks read it as is: `H<subscript>2</subscript>O`, `a   b` → `a b`, a formula's `<content>` double spaces kept
  - every `<caption>` of a float's head is read (DocLang 0.8, docling-core#880), and a nameless `<group>` wrapping one float with `<footnote>` / `<text>` satellites is that float — its footnotes the float's own `footnotes` (`TreeItem::footnotes`, the JSON refs; HTML writes them in the `<figure>` / after the `<table>` as `<div class="footnote">`, docling-core#675), extra captions after the head's; Markdown prints the captions joined by a space and the footnotes after the float (after a code block, then its captions), as docling's does
  - `<track>` is skipped with a warning, its captions and cues with it
  - `<checkbox/> y` is one checkbox item whose text is the run after the symbol, verbatim (`" y"`, docling-core#955), in a `<text>` and in a list item's or a cell's runs; Markdown does not double that space (`- [x] y`)
  - `<layer value="background"/>` is the background layer (`ContentLayer::Background`, out of the default exports), no longer the body
  - an OTSL cell's text is its parts' reading concatenated; a cell holding an element is rich — an `inline` group of its runs when they are virtual text, else an `unspecified` group of its blocks
  - a styled hyperlink keeps its formatting (`***italic and bold hyperlink***`), as docling-core 2.106 does

#### DocTags (`.doctags`/`.dt`)

**Backend:** `docling-core::doctags`\
**Status:** reads the SmolDocling/granite-docling token stream back into a `DoclingDocument` (#152)

#### LaTeX

**Backend:** `latex.rs` + `latex_walker.rs`\
**Status:** **8/8 Markdown exact, 7/8 JSON identical** vs docling 2.148.0 / docling-core 2.106.0 too (#997: the Markdown concatenates the runs, `states $h_t$,`); **8/8 DocLang exact** vs docling bcb3678 (docling-core 2.106, DocLang 0.9), the six arXiv projects included; upstream's own `tests/test_latex` inputs 205/213 Markdown, 210/213 JSON, every difference one of the divergences below

- docling's `LatexDocumentBackend` ported handler for handler (#466; docling 2.142's arXiv rewrite, docling#4817, plus #4974, #4517, #4644, #4579, #4502, #4475, #4441, #4325, #4340) on a port of pylatexenc 2.11's tolerant `LatexWalker`
- `example_01/02` plus the six arXiv projects (`1706.03762`, `2305.03393`, `2310.06825`, `2412.19437`, `2501.00089`, `arXiv-2501.01300v2`), `\input` files parsed in place and decoded like the main file (UTF-8, cp1252, latin-1)
- The walker port is checked node for node against pylatexenc's own dump, parsed with docling's context, on every fixture and on all 213 upstream test inputs (`scripts/dev/pylatexenc_dump.py` … `latex_walker::tests::dump_file`), because upstream's output is the node stream's:
  - pylatexenc's default macro specs plus docling's (`\title`, `\caption`, `\captionof`, `\bibitem`, `\parbox`, the affiliation commands and `\thanks` take their arguments; `thebibliography` its `{99}`; `\paragraph{…}` is still unknown, so its title is a sibling group)
  - the document's argument-less macros registered ahead of them (`\def\b{…}` does not read a following `$` as the accent's argument)
  - chars nodes ending at every macro, the swallowed post-macro space, comments, specials (`~`, `--`, `` `` `` dropped — upstream has no handler for them)
  - tolerant recovery from stray braces and `\end`s; an unclosed environment keeps the file's trailing whitespace
- Preprocessing: every macro, environment, theorem, length, counter and conditional definition is masked out of the source before the parse and kept, with the preamble's ignored commands (`\documentclass`, `\usepackage`, …), as **furniture**-layer text items ahead of everything else (the default Markdown leaves them out); zero-argument shortcuts to an environment delimiter or `$$` (`\def\be{\begin{equation}}`) are expanded where used; `\hfuzz`/`\vfuzz` assignments dropped
- Handlers:
  - the text buffer of strings and **inline formulas**, flushed by structural macros/environments/paragraph breaks/every `\item` (docling#4340, #4644): a run with inline math is an empty item holding an `inline` group of `text` and `formula` parts — paragraphs, headings, captions, list items, footnotes and front matter alike; Markdown joins the parts with spaces as docling-core does (`Inline math:  $E = mc^2$`, the text run's own space kept: whitespace that rendered Markdown collapses, followed byte for byte)
  - whitespace runs holding a line break collapse to one space in every item but formulas and code
  - front matter in `metadata` / `authors` / `affiliations` groups, `\and`-split, `\parbox` unwrapped, `\inst{1}` markers as `^{1}` formulas, `\thanks` / `\footnote` / `\altaffiliation` as footnotes (a body `\footnote` in place)
  - `equation`, `align` & co. keep their `\begin`/`\end` inside the formula (only `displaymath`/`math`/`dmath` lose them) — an `align` body is not valid math without them
  - accent and letter macros (`\'e` → é, `\ss` → ß, latex2text's combining marks, NFC; docling#4517)
  - `\caption` as rich `caption` text: in a `figure` the picture's caption (no `Image: <path>` caption there), in a `table` / `longtable` the table's — Markdown prints it ahead of the table
  - `\newcommand` bodies expanded by text (math-bearing ones inline), argument-less macros resolved to a fixed point; citations `[key]`; `\href[options]{url}{text}` → `[text](url)` inline in its paragraph, heading or list item, the URL as written (#1021, docling#4600 — it used to split the paragraph and glue the URL to the text); theorem/proof markers; description terms `Term: …` (docling#4579); `thebibliography` as one `[key] text` list item per `\bibitem`
  - `tabular` with the `\multicolumn`/`\multirow` lookup at the node's *document* offset into the environment's own text (usually a miss, then the macro's groups are cell text), spans clamped to the HTML table model's caps (docling#4974); a cell holding math is a rich cell (a group under the table)
  - figures: a raster file embedded as PNG like upstream's `ImageRef.from_pil`; a PDF's first page rendered at 144 dpi, the size pypdfium2's `render(scale=2)` gives (ceiling of the scaled page) — by docling-pdf's renderer, so the pixels differ; a build without the `pdf` feature (wasm) leaves the picture without a payload
  - a `tikzpicture` is a picture carrying its source as `meta.code` (language `Tikz`), upstream's fallback without Tectonic (no engine here)
  - code environments keep their first line's indentation (docling#4502)
- DocLang (#884): the backend's item tree is serialized as docling-core's `DocLangDocSerializer` does it (`docling-core/src/doclang_tree.rs`; since DocLang 0.9 every tree's DocLang, § DocLang) — the minified markup, then `serialize_doc`'s post-processing — instead of the flat nodes, which cannot say a footnote is a `<footnote>`, carry a caption's or a table cell's inline formulas (`<caption><content>…</content><formula>…</formula>…`, `<fcel/><text><formula>…`), or keep a list item's raw text (`weight\_decay`) or a paragraph's (`Zhang*` read as italics). All eight `groundtruth/*.dclg` byte-exact (were 85–96% on the arXiv projects); docling-core 2.104's `export_to_doclang()` of our JSON equals our DocLang on all 253 LaTeX strings of upstream's `tests/test_latex` (the TikZ `meta.code` below aside). The `.dclg` is docling's placeholder image mode — `export_to_doclang_with_images(ImageMode::Placeholder)`; `export_to_doclang()` stays the archive's `document.xml` with its `<src uri="assets/…"/>` references, like every format (`example_01/02`'s `groundtruth_dclx` exact)
- Deviations:
  - a spanning cell's `row_span` / `col_span` agree with its offsets, where upstream leaves them at 1 (`1706.03762`'s one table)
  - Markdown and DocLang leave out the Python repr docling-core prints for a TikZ picture's `meta.code` (`confidence=None created_by=None text='…' language=…`; DocLang's `<custom><code>…</code></custom>`); the JSON keeps the code
  - line endings are translated for byte input too (upstream reads a path in text mode, a stream raw)
- docling#4325 (2.130.0-33): `tabular*`, `tabularx` and `longtable` are tables (longtable with docling's own `[{` spec; `\endhead` & co. no cell content)

#### MHTML (.mhtml/.mht)

**Backend:** `mhtml.rs` (mail-parser) → HTML backend\
**Status:** docling's `InputFormat.MHTML` (docling#4184, unreleased — it unwraps the archive inside its HTML backend)

- #386 mirrors its fixture (`tests/data/mhtml/sources/example.mhtml`, a Blink save of example.com; upstream ships no groundtruth for it — its tests assert programmatically — so its expected outputs live in our regression corpus as `example_docling.mhtml`, beside our older `example.mhtml`, a different save of the same page with the same Markdown) and its test matrix:
  - the root is the first `multipart/related` entity
  - its `start` parameter names the root part by `Content-ID`
  - a `multipart/alternative` root yields its last HTML alternative
  - a bare `text/html` message is its own root
- no `Content-Type`, no `multipart/related`, an empty one, a `start` naming nothing or a non-HTML part, or an empty page **fail** the conversion as docling's do (we used to return an empty document, and to take the first `text/html` part anywhere)
- image resources are the selected scope's `image/*` parts only, keyed by `Content-Location` verbatim and resolved against the archive base, and by the normalized `cid:` (case-insensitive, brackets stripped), first part wins a duplicate key
- the base is the root's `Content-Location`:
  - a remote URL kept
  - `//host` made `https:`
  - a local path / `file:` URI / Windows drive path (either slash form) remapped under docling's synthetic `thismessage:/` so `<img src>` and `Content-Location` meet without touching the filesystem
- archive images (and `data:` URIs) are embedded only under `fetch_images`, as docling's `HTMLBackendOptions.fetch_images` gates them — the default leaves placeholders (we used to embed unconditionally)
- Not ported: docling's fallback to its shared image loader (files beside the archive, remote fetches) for a reference the archive lacks
- Kept beyond docling: `use_web_browser` pre-renders the page (docling rejects `render_page` for MHTML)
- Markdown on the mirrored fixture is identical

#### RTF (.rtf)

**Backend:** `rtf.rs` (hand-rolled control-word tokenizer, #209)\
**Status:** **native, where docling reads RTF through LibreOffice** (upstream gained that path after this backend)

- paragraphs + bold/italic/strike runs, stylesheet/outline headings, `\listtext` lists (incl. multilevel numbering; list identity from `\ls`, or a top-level marker-kind flip when a writer omits it — #385), `\trowd` tables with `\cellx`-grid merge recovery, textbox content, HYPERLINK fields, embedded PNG/JPEG pictures, cp1250/1251/1252 + `\uN` unicode
- Conformance (`scripts/conformance/rtf_conformance.sh`): 30 of the 31 corpus files in `tests/data/rtf/sources/` are LibreOffice-generated from the DOCX/DOC fixtures and diffed against **our own conversion of the source document** — 4/30 exact, 6/30 whitespace-normalized
- the rest is dominated by LibreOffice round-trip artifacts (style bold/italic materialized into runs, equations linearized to text, checkbox form fields), not parser losses
- #387 adds the 31st, `legacy_sample`, mirrored from upstream with **docling's own groundtruth**, so it is diffed against that real reference instead: **exact**, tables rendered without width padding to match the stored corpus (`--compact-tables`)
- #578: equations — `{\mmath{\*\moMath …}}`, OMML transliterated into control words (`\msSup`, `\mr`, `{\mchr \u8721?}`, …) — are rebuilt as OMML and converted by the DOCX backend's `omml.rs`, so an RTF equation reads as the same equation saved as `.docx` (`Synthetic equation:  $E=mc^{2}$`, docling's inline spacing; a paragraph of equations alone is a `$$…$$` `formula`)
- the `{\mmathPict …}` picture Word writes after each one for readers without math is dropped, where it used to surface as an `<!-- image -->` ahead of the paragraph and the formula itself was lost
- An `\mmath` group holding only a picture (LibreOffice's save of a formula it could not express) still yields that picture
- Upstream reads RTF through LibreOffice, whose conversion keeps the equation; the mirrored LibreOffice saves `omml_frac_superscript`, `omml_multi_equation_paragraph` and `table_with_equations` now match their DOCX originals' groundtruth equations

#### SVG (.svg)

**Backend:** `svg.rs` + resvg rasterization (#212)\
**Status:** **docling.rs extension — docling does not accept SVG input**

- mirrors the pdf/pdf-text split:
  - ML builds rasterize (resvg, white-backed PNG, ~2048px long side) and ride the image pipeline (layout + OCR + tables)
  - `pdf-text`/wasm builds, `--no-ocr` and `--text-layer-only` extract `<text>` elements directly — transform-aware (translate/scale/rotate/matrix) reading order, root `<title>`/`<desc>` as heading/lead paragraph, unrendered subtrees (`defs`, `clipPath`, `display:none`, …) skipped

#### StarOffice / OpenOffice 1.x & flat ODF (.sxw/.stw/.sxg, .sxi/.sti, .sxc/.stc, .fodt/.fods/.fodp)

**Backend:** `odf.rs` (shared ODF parser + local-name mapping layer, #215)\
**Status:** **docling.rs extension — docling reaches these only via LibreOffice**

- the OO1.x predecessor schema differs from ODF mostly in namespace URIs, which this parser never matches on — the mapping layer covers the real deltas:
  - `office:body` as the direct content container (dispatched by `office:class`)
  - `ordered-list`/`unordered-list`
  - `tab-stop`
  - `text:level` headings
  - `style:properties` with `text-crossing-out`/`text-underline`
- Flat ODF is the same document XML uncompressed in one file:
  - styles ride the content DOM
  - embedded charts become inline `draw:object` documents
  - inline `binary-data` images gate on their decoded raster magic (SVM previews stay out)
- UOF is out of scope

#### dBase / DIF / SYLK (.dbf, .dif, .slk/.sylk)

**Backend:** `interchange.rs` (#216)\
**Status:** **docling.rs extension — docling reads none of them**

- native parsers, content-sniffed inside one backend so a misnamed file still converts
- DIF and SYLK are sheet snapshots and run through the same flood-fill region splitting as ODS sheets
- a `.dif`/`.slk` LibreOffice saves from a sheet converts **byte-identically** to our conversion of the `.ods` itself (verified on the corpus in `tests/data/interchange/`)
- dBase converts as one table:
  - field names as the header row
  - deleted records skipped
  - `D` dates as ISO
  - `L` logicals as true/false
  - memo fields (a `.dbt` sidecar) empty
  - cp1252 high bytes

#### Lotus 1-2-3 / Symphony / MS Works (.wk1–.wk4, .wks, .wrk, .123)

**Backend:** `lotus.rs` (#216)\
**Status:** **docling.rs extension — docling reads none of them**

- native record-stream parsers following Gnumeric's lotus-123 importer, content-sniffed on the BOF so a misnamed file still converts (`.wks` is ambiguous: 1-2-3 rel 1A and MS Works v3 both used it — the BOF opcode decides)
- WK1/WKS cells (INTEGER/NUMBER/LABEL/FORMULA caches + STRING results)
- WK3/WK4/123 cells (extended floats, SMALLNUM, packed numbers, FORMULASTRING, multi-sheet)
- Works v3 cells incl. the packed-f32 SMALL_FLOAT
- Sheets split into data regions like ODS: a `.wk1` of a sheet's data converts **byte-identically** to the `.slk` of the same sheet (pinned in `tests/data/lotus/`)
- Read-verified against LibreOffice's Lotus/Works import on the committed corpus (LO itself drops WK1 string-formula results; we keep the cached STRING record, following Gnumeric)
- Quattro Pro and the rest of the umbrella stay demand-gated

#### Quattro Pro (.wq1, .wq2, .wb1–.wb3, .qpw)

**Backend:** `quattro.rs` (#216)\
**Status:** **docling.rs extension — docling reaches Quattro Pro only via LibreOffice (libwps)**

- native parse of all generations after libwps' readers:
  - DOS 1–4/5 record streams (BOF 0x5120/0x5121; cells `[fmt][col u8][sheet][row i16]` / `[col][sheet][row][style]`, pascal-string labels in CP 437, formula caches + 0x33 string results)
  - Windows 1–8 streams (BOF 0x1001/0x1002, `.wb3` = BOF 0x1007 in the OLE `PerfectOffice_MAIN` stream; C-string labels, CP 1252)
  - QPW 9–X9 (OLE `NativeContent_MAIN` zones: 0x407 string table, 0x601/0xA01 sheet/column, 0xC01 typed cell runs with list/increment packing and libwps' packed 4-byte floats, 0xC02 string results)
- Formulas contribute their cached value
- each sheet runs through the ODS flood-fill region splitting
- Corpus: the six CC0 format-corpus samples (wq1, wq2, wb1, wb2, wb3, qpw)

#### MS Works 6–9 spreadsheet (.xlr)

**Backend:** `xls.rs` via extension routing (#216)\
**Status:** **docling.rs extension**

- Works 6–9 saved spreadsheets as an Excel 97 BIFF8 `Workbook` stream in an OLE container under the `.xlr` extension, so the XLS reader (calamine) takes them unchanged
- Unverified against a real file — no public `.xlr` sample exists (format-corpus, Tika, LibreOffice and govdocs1 have none)
- a non-BIFF `.xlr` (Works 5 or older) fails with the XLS reader's error

#### Kingsoft Spreadsheets (.et, .ett, .eth)

**Backend:** `xls.rs` via extension routing (#216)\
**Status:** **docling.rs extension**

- WPS Office's spreadsheet format is an Excel 97 BIFF8 `Workbook` stream in an OLE container under its own extension (`.et`; `.ett` / `.eth` its templates), so the XLS reader takes it unchanged
- Unverified against a real file — no public `.et` sample was found (LibreOffice's test corpus has none); a WPS file in another container fails with the XLS reader's error

#### T602 (.602)

**Backend:** `t602.rs` (#216, after LibreOffice's T602 import filter)\
**Status:** **docling.rs extension** — docling reaches T602 only through LibreOffice

- the Czechoslovak DOS word processor's 8-bit text: `@` setting lines (`@CT n` picks the code page — 0 Kamenický, the default; 1 Latin-2 / CP852; 2 KOI8-ČS — the rest are layout and skipped), `.` dot-command and comment lines left out, `0x1A` ends the text
- style control bytes as runs: `0x02` bold, `0x04` italic, `0x0F` wide (read as bold), `0x13` underline, `0x14` / `0x16` super- / subscript
- **Divergence from LibreOffice's default:** T602 has no paragraph mark (its text is hard-wrapped to the page), so a blank line ends a paragraph and the lines between read through — LibreOffice's "reformat paragraphs" option; its default keeps each line with a line break. A soft hyphen at a wrap (`0xAD 0x8D`) joins the word. A block drawn with box characters (a frame, a table) stays preformatted; a line opening with a `-` / `*` / `•` bullet starts a list item
- **Divergence:** LibreOffice's code-page tables approximate (Kamenický `ĺ` as `ô`, its line-drawing range as letters); the code pages here are the standard ones (`dos_codepage.rs`)
- Corpus: LibreOffice's two T602 test files (`crates/docling/tests/data/t602/`)

#### Palm e-texts (.pdb)

**Backend:** `palmdoc.rs` (#216)\
**Status:** **docling.rs extension** — docling has no reader (LibreOffice reads them through libe-book)

- a Palm database (78-byte header, record table) whose type and creator say what it holds: PalmDOC / AportisDoc (`TEXt`/`REAd`) and TealDoc (`TEXt`/`TlDc`) with PalmDOC's LZ77 compression or none, zTXT / Weasel Reader (`zTXT`/`GPlm`) with one zlib stream over its records
- blank-line-separated paragraphs (their lines read through), or a paragraph per line in a text with no blank lines; TealDoc's `<HEADER TEXT=…>` is a heading, `<LINK TEXT=…>` its text, its other tags carry none
- UTF-8 when the text is, else Windows-1252; the database name is the document title (`metadata.title`)
- eReader (Peanut Press), Plucker and Mobipocket books are other formats in the same container and are refused by name; a `.pdb` that is no Palm database (a Protein Data Bank file) reads as plain text. `.pbd`, #216's spelling, routes here too
- Corpus: LibreOffice's PalmDOC, TealDoc and zTXT test files (`crates/docling/tests/data/palmdoc/`)

#### Hancom Hangul (.hwp, .hwpx)

**Backend:** `hwp.rs` (#216, after Hancom's published "HWP 5.0 Document File Structure" and OWPML / KS X 6101; hwplib and hwpxlib for the record layouts)\
**Status:** **docling.rs extension** — docling has no reader (LibreOffice imports HWP 5 partly, HWPX not at all)

- **HWP 5** (Hangul 2002 on): an OLE compound file — `FileHeader` (signature, flags), `DocInfo` (the character and paragraph shapes, the embedded binaries), one `BodyText/SectionN` stream per section, raw-deflated when the document is compressed; a stream is tagged records (tag, nesting level, size), a paragraph its `PARA_HEADER`, UTF-16 `PARA_TEXT` and character-shape runs, an extended control character in the text standing for the next `CTRL_HEADER`
- **HWPX** (Hangul 2010 on): a ZIP with the `application/hwp+zip` mimetype — `Contents/header.xml` (`hh:charPr`, `hh:paraPr`), `Contents/sectionN.xml` (`hp:p` / `hp:run` / `hp:t`), `Contents/content.hpf` (the `BinData/` images)
- a paragraph whose shape carries an outline head (the `개요 1…7` styles) is a heading of that depth, a number / bullet head an ordered / bullet list item; bold, italic, underline, strike-through and super-/subscript from the character shapes (underline and the scripts reach DocLang)
- tables with their spans and caption (a cell's paragraphs and nested tables joined into its text); text boxes and pictures after the paragraph they are anchored in (a floating object does not split the text around it); an equation keeps its Hangul equation script as text (it is not LaTeX: `{a} over {b}`); ruby text (덧말) after its base in parentheses; composed characters (글자 겹치기) and text art keep their text
- page headers / footers and foot- / endnotes are furniture items (`page_header` / `page_footer` / `footnote`), as the Word 97 reader keeps its stories — out of the Markdown, in the JSON and DocLang
- tracked changes (`track_changes`, #851): HWPX's inline deletion / insertion marks read `accept` (the default) or `reject`; HWP 5's text is read as stored (`reject` refused), `all` refused in both
- one embedded image decoded once however many pictures show it; past the per-part byte budget (`DOCLING_RS_MAX_PART_BYTES`) a picture keeps its place without its payload — hwplib's `big_file.hwp` places one 3.9 MB photo 16 384 times
- a password-protected document raises the typed encryption error; a distribution document (배포용 문서, its body encrypted into `ViewText` for viewing only) and the HWP 3.x format of Hangul 96/97 are refused by name; form controls (buttons, combo boxes), chart objects and OLE contents carry no text here
- content-sniffed (the `FileHeader` + `DocInfo` streams; the HWPX mimetype), so a nameless upload converts
- Corpus: seven hwplib and six hwpxlib samples, Apache-2.0 (`crates/docling/tests/data/hwp/`)

#### Classic Macintosh word processors (.mw, .mcw, .cwk)

**Backend:** `macwp.rs` (#216, after libmwaw's MacWrite, Word for Mac and ClarisWorks readers)\
**Status:** **docling.rs extension** — docling has no reader (LibreOffice reads them through libmwaw)

- told apart by content whatever the extension (Mac files often had none): Word for Mac by its magic (`FE32` 1.0, `FE34` 3.0, `FE37` `001C` / `0023` 4.0 / 5.0), ClarisWorks / AppleWorks 1–6 by `BOBO` at offset 4 — both also sniffed for nameless input — and MacWrite by its version word (3 = 1.0–2.2, 6 = 4.5–5.0) under its extension; text is Mac Roman
- **MacWrite**: the window records (main text, header, footer) and their 16-byte paragraph entries (5-byte entries and typed blocks in version 3); a text block raw or nibble-compressed (`" etnroaisdlhcfp"`, `0xF` escaping a literal byte), its style runs as bold / italic / underline / sub-/superscript; header and footer paragraphs are furniture
- **Word for Mac**: the text run (from 0x80 in 1.0, 0x100 in 3.0, the header's begin in 4.0 / 5.0) or, for a fast-saved 4.0 / 5.0 file, its piece table; 4.0 / 5.0 split the main text, the footnotes (furniture `footnote`) and the headers / footers (furniture `page_header`) by the header's lengths; 1.0 and 3.0 give no split, so their header, footer and footnote paragraphs follow the body text; a form feed is a page break
- **ClarisWorks**: every `DSET` zone of file type 1 is a text — its character count and text chunks after the paragraph, font, token and size blocks; zone 1 is the main text, a header / footer / footnote zone furniture, a text box's or a table cell's text a paragraph after the main text (the table grid itself is not rebuilt); a spreadsheet, database or painting with no text zone is refused by name
- **Not read:** Word's and ClarisWorks' character and paragraph styles (bold, headings), pictures, MacWrite Pro / MacWrite II (another format, refused by name)
- Corpus: six libmwaw test files from LibreOffice (`crates/docling/tests/data/mac_wp/`)

#### Lotus Word Pro (.lwp)

**Backend:** `lwp.rs` (#216, after LibreOffice's `lotuswordpro` filter)\
**Status:** **docling.rs extension** — docling has no reader

- Word Pro 97 – Millennium 9.8's object store: the file header behind an old-style object header, the object index (root over leaf pages, delta-compressed ids, the time table indexed ids point into), each object's variable header and its data, run-length decompressed when flagged
- the text is in stories: a story names the layout it flows into — the page (the body), a header or footer, a table cell, a frame, a footnote — and the head of its paragraph list; a paragraph's fribs give its text (code-page text switching to UTF-16 at a NUL — "packed unicode"; a code-page modifier picks Windows-125x / CJK encodings), tabs, line breaks, hard spaces and page breaks; a revision marked deleted is left out (the accepted view)
- the body story first, then the other content stories (table cells, frames) as paragraphs in object order — **the table grid and the frame anchors are not rebuilt**; headers, footers and footnotes are furniture (`page_header` / `page_footer` / `footnote`); the "continued on / from" templates, drop caps and the numbering definitions' stories are skipped
- a paragraph whose style is named `Heading N` (`Überschrift N`, `Titre N`) is a heading; numbering, bullets and character formatting live in style objects and the font table and are not read (a numbered paragraph keeps its text, not its number)
- Word Pro 96 files (file revision < 11) and the Bento-packaged, PKWARE-imploded form are refused by name — as LibreOffice's filter supports only Word Pro 97 on; content-sniffed (`WordPro` + `LWP7`)
- Corpus: three LibreOffice test files (`crates/docling/tests/data/lwp/`); every file of LibreOffice's `lotuswordpro` test corpus, its fuzzing and crash cases included, converts or errors cleanly

#### Computer Graphics Metafile (.cgm)

**Backend:** `cgm.rs` (#216, after ISO/IEC 8632-3 and -4)\
**Status:** **docling.rs extension** — docling has no reader (LibreOffice imports CGM into Draw)

- the vector-drawing format of technical illustration (S1000D / ATA iSpec 2200 graphics, aircraft and automotive manuals, 1990s clip art): what a converter can take from a drawing is its text, so every `TEXT` / `RESTRICTED TEXT` element is a paragraph in drawing order, a text continued by `APPEND TEXT` (its `not final` flag) joined into one; the metafile's name is the document title, and a metafile of several pictures heads each picture's text with its name
- **Divergence from LibreOffice:** the geometry is not rendered (no picture item, no image) — the drawing's labels are the document
- binary encoding (8632-3): element headers (class, id, short or partitioned long length), the VDC size from `VDC TYPE` and the `VDC INTEGER` / `REAL PRECISION` in force, long-form strings; clear-text encoding (8632-4): `BEGMF` / `BEGPIC` / `TEXT` / `RESTRTEXT` / `APNDTEXT` statements, `'` / `"` strings with doubled quotes, `%` comments; the character encoding (8632-2) is refused by name
- strings as UTF-8 when they are, else Latin-1 (the default G1 set); character-set switching (ISO 2022 escapes) is not followed
- content-sniffed (a `BEGIN METAFILE` element whose name fills it, or a leading `BEGMF`); a `.cgm` with any `BEGIN METAFILE` header reads
- Corpus: three LibreOffice test files (`crates/docling/tests/data/cgm/`)

#### Print files (.prn)

**Backend:** `print_file.rs` (#216; not `prn.rs` — `PRN` is a reserved device name on Windows, which cannot check such a path out)\
**Status:** **docling.rs extension** — docling has no reader

- a DOS or Windows "print to file": a report's fixed-width text (Lotus 1-2-3, dBase, the Generic / Text Only printer) or a printer stream for a PCL (LaserJet) or ESC/P (Epson) printer
- printer codes out: PCL's parameterized escape sequences (their `W` data blocks — raster, font downloads — skipped by length) and the PJL job header; ESC/P commands by their parameter counts, bit images by length
- overstrikes undone: a backspace overprints (`_⌫X` reads `X`), a bare carriage return's second pass (bold, underline) is dropped; a form feed is a page break
- UTF-8 when the bytes are, else code page 437; blank-line-separated blocks become paragraphs, a block in columns (most lines hold an inner run of three or more spaces — a report's table) stays preformatted so the columns line up
- PostScript is a program, not text: refused, with `ps2pdf` named
- Corpus: two fixtures of our own (`crates/docling/tests/data/print_file/`)

#### AbiWord (.abw, .zabw, .awt)

**Backend:** `abw.rs` (roxmltree; gzip via flate2 for .zabw, #216)\
**Status:** **docling.rs extension — docling reaches AbiWord only via LibreOffice (libabw)**

- native AWML parse:
  - Title/heading-N styles map like DOCX
  - `<c>` runs carry bold/italic/strike (underline and sub/superscript survive into DocLang inline runs)
  - `xlink:href` anchors become links
  - list paragraphs (listid + list_label marker, label text and tab dropped) with per-listid numbering
  - attach-grid tables (spans replicate the anchor docling-style)
  - header/footer sections (incl. -even/-first) dropped as furniture
  - embedded base64 images extracted as pictures
- Corpus: AbiWord-CLI conversions of the DOCX fixtures diffed against our own DOCX conversion — unit_test_formatting is byte-identical
- the residue elsewhere is AbiWord import artifacts (Word field codes materialized as text, list numbering downgraded to bullets, merged-cell shifts), not parser losses

#### WordPerfect 5.x / 6.x+ (.wpd, .wp, .wp5, .wp6, .wpt)

**Backend:** `wpd.rs` (#216)\
**Status:** **docling.rs extension — docling reaches WordPerfect only via LibreOffice (libwpd)**

- native parse of the `ÿWPC` function-code stream, version from the prefix header:
  - WP 5.0/5.1 (hard/soft returns, hard space/hyphens, `C0` extended characters, `C3`/`C4` attribute pairs, fixed- and variable-length functions skipped by size)
  - WP 6.x+ (default international chars 1–32, single-byte and `0xD0` end-of-line-group codes with libwpd's semantics — soft line ends `0xCD`–`0xCF`/subgroups 1–3 wrap, deletable soft ends at hyphenation points join the word, hard ends break the paragraph, cell/row/table-off marks build the table with the next cell's column span and bound-from-above flag —, `F0` extended characters, `F1` undo regions — deleted text is dropped — `F2`/`F3` attributes)
  - WP 5.x tables via the `0xDC`/`0xDD` table groups that begin cells and rows
- Bold/italic/strike reach Markdown, underline and sub/superscript the DocLang inline runs
- table cells/rows collect into a table (padded to the widest row)
- the WP character sets 0–14 map through Tika's tables (Multinational 1 #9, the typographic apostrophe, to U+2019)
- Not extracted: header/footer and footnote text (6.x prefix packets, 5.x function payloads), styles/outline numbering
- Refused with a targeted error:
  - encrypted documents
  - WP 4.2 and older (no prefix header)
  - WordPerfect for Macintosh 3.x (file type 44)
  - non-document file types
- Corpus: Apache Tika's three WordPerfect fixtures (`tika_wp6.wpd`, govdocs1 `tika_wp50.wp`/`tika_wp51.wp`), LibreOffice's libwpd `WP5.wp`/`WP6.wpd` and the Open Preservation format-corpus WP 6.1 sample
- a 1.1 MB WP 6.1 DOS thesis converts to 72 paragraphs with every hyphenated line rejoined (175 fragments before the libwpd code tables)
- every string Tika's tests assert is present (`AND FURTHER`, `test1-2`, `Surrounded by her family`, `STUDY RESULTS: Existing condition`, `Seattle nonstop flights.`), the deleted `this was deleted.` is not

#### Microsoft Works word processor 2–9 (.wps)

**Backend:** `wps.rs` (#216)\
**Status:** **docling.rs extension — docling reaches Works only via LibreOffice (libwps)**

- native parse after libwps' readers of both generations:
  - Works 2.x DOS / 3 / 4 (`WPS4`: 256-byte header, text limits, BTEC PLC → 128-byte FDPC pages with bold/italic/strike/underline/sub-superscript blobs, CP 850 / CP 1252 text with paragraph/line/page codes, footnote definitions cut out of the body and appended; the Windows versions' OLE `MN0` stream)
  - Works 2000 / 6–9 (`WPS8`: OLE `CONTENTS` with the chained CHNK index, UTF-16 `TEXT` zone typed by the `STRS` PLC — main, notes, frames, header, footer —, `BTEC` → `FDPC` pages of libwps' tagged font records)
- Header/footer zones dropped as furniture
- Works tables not rebuilt (cell text follows the body as paragraphs)
- spreadsheets/databases and Works for Macintosh refused with a targeted error
- Corpus: LibreOffice's five libwps smoke files (Works 2.00A DOS, 3.0, 4.5, 5.0, 6.0) pin detection and the header/index/zone walk
- content decoding is pinned by synthetic WPS4/WPS8 streams in the unit tests — no public corpus with real Works prose exists (govdocs1 has none)

#### StarOffice 5 binaries (.sdw, .sgl, .sda/.sdd, .vor)

**Backend:** `staroffice5.rs` (CFB via the shared `cfb.rs`, record/chunk walk per libstaroffice's reverse engineering, #215)\
**Status:** **docling.rs extension — docling reaches these only via LibreOffice (libstaroffice)**

- text-level extraction
- StarWriter (`StarWriterDocument` stream, SW3–SW5):
  - record tree (type byte + 24-bit size, flag-byte prologues)
  - `'T'` text nodes as paragraphs in document order — tables flatten to cell texts, inline redline fragments kept
- StarDraw/StarImpress (`StarDrawDocument3` stream):
  - pages (`DrPg`) as sections with outliner (`xV4B`) texts, outline depth as list nesting
  - master pages, the object-less handout and `~LT~Notizen` notes pages dropped (the binary format's notes pages are not read; ODP speaker notes are notes-layer items since #862)
- `.vor` templates dispatch by the contained stream
- a StarWriter 5 global (master) document (`.sgl`, #216) is the same `StarWriterDocument` stream, read the same way: the text it stores, its linked sub-documents not followed
- Strings decode as cp1252
- StarCalc (.sdc) has a different cell-record model and errors with a targeted save-as-.ods message — a follow-up

#### First-class table cells + repair API

**Backend:** `docling-core` `TableCell` (text, bbox, span rectangle, header roles), `Table::cells`, `Table::{cell_at, cell_text, set_cell_text, cell_bbox, set_cell_bbox, find_cell_by_bbox, update_cell_by_bbox}`, `DoclingDocument::tables[_mut]` (#238, #240)\
**Status:** **docling.rs counterpart of Python docling's `TableCell`**

- the PDF TableFormer paths emit real per-cell records (page-point bboxes, row/col spans from the OTSL grid, `ched`/`rhed`/`srow` header roles)
- the JSON export serializes them verbatim (`table_cells` with `bbox`/span offsets; the grid repeats spanning cells like docling's `TableData.grid`) — so the Python/Node bindings see them
- the DocLang structure overlay derives from them (real `<lcel/>`/`<ucel/>`/`<ched/>` tokens for PDF tables)
- bbox lookup is best-IoU
- updating a spanning cell through any covered position updates the record and every covered grid slot
- Declarative tables derive their cells from the structure overlay (DOCX/XLSX merges, HTML spans + `th` headers, ODF covered cells, USPTO CALS)
- verified against the mirrored Python groundtruth: 43/56 corpus files with identical per-table (cells, spans, headers), and every xlsx/html span fixture exact
- the residue is pre-existing table-count/nested-table divergences and Python backends' empty-cell omission quirks (pptx, one word_tables cell), not cell records

#### Visio (.vsdx, .vsdm)

**Backend:** `visio.rs` (OPC zip + XML, same `Package` machinery as DOCX/PPTX, #214)\
**Status:** **docling.rs extension — docling has no Visio reader**

- each page a level-1 section
- shape text in reading order (top-to-bottom/left-to-right, group children through the parent coordinate system, master default-text inheritance)
- connectors resolved via `<Connects>` into a From/To(/Label) relations table
- background pages skipped
- Legacy binary .vsd and 2003-XML .vdx are follow-ups

#### DjVu (.djvu, .djv)

**Backend:** `djvu.rs` + the `djvu-rs` crate (#434)\
**Status:** **docling.rs extension — docling has no DjVu reader**

- pure-Rust decode (MIT, no GPL deps, no DjVuLibre binary), so it runs in every build incl. wasm
- Mirrors the pdf/pdf-text split with the **text layer as the default**:
  - the hidden per-page OCR zone tree becomes blank-line-delimited paragraphs (physical scan lines reflowed into prose)
  - `PageBreak` between pages
  - `--pages A-B` honored
- every page opens with a `PageInfo` marker (size in points from the INFO chunk's pixels ÷ dpi)
- each paragraph carries the box of its text-layer zones (word → line → paragraph zones, whichever level aligns with the text by count)
- so the JSON has docling's `pages` map and per-item `prov` like a PDF — what docling-core's `pages` filter and `export_to_markdown(page_break_placeholder=…)` key on
- a scan-only DjVu (no text layer on any selected page) rasterizes through `djvu-rs` (fitted to `DOCLING_RS_DJVU_RENDER_PX`, 2500 px) and rides the image ML pipeline, or degrades to an empty document with a warning without ML
- Three own fixtures (3/11/84 pages) pinned in the regression suite

#### Apple iWork (.pages, .numbers, .key)

**Backend:** `iwork.rs` (zip + Snappy-framed protobuf IWA, generic wire walk; field/type numbers per numbers-parser / keynote-parser reverse engineering and docling's Pages reader, #213, #318) + `pages.rs`/`pages_iwa.rs`/`pages_xml.rs` (the Pages content model and its two readers, mirroring upstream's `docling/backend/iwork/` package, #383)\
**Status:** **Pages: 4/4 exact** Markdown on upstream's committed `tests/data/pages/` groundtruth (docling#4062, mirrored with its `.json`; #383)

- the full `IWorkPagesDocumentBackend` feature set in both container generations:
  - tables **placed in the text flow** at their U+FFFC anchor (not appended)
  - floating **text boxes** and groups after the body
  - **character formatting** (bold/italic/underline/strikethrough, super/subscript) with the spaces around a run kept and the paragraph trimmed once at its ends
  - **hyperlinks**
  - **lists** (bullet/numbered from `TSWP.ListStyle` label types and the list-depth run table; iWork '09 `sf:liststyle`, whose `sf:list-level` is the rung of the style's ladder — rung 0 the unlabelled one body text sits on, docling's reading since 2.131 that the Keynote '09 reader already followed; a sub-list hangs from the list item above it in the JSON, docling#4863)
  - **inline images** (`TSD.ImageArchive` → `Data/` member, `sf:media`)
  - **headers/footers/footnotes** on the furniture layer
  - **reviewer comments** on the notes layer with `[author]: text` and `comments` back-refs on the annotated item (docling-core's bare `add_comment` shape — no `comment_section` group)
  - Pages 5.2+ table cell storage (v4/v5 cell layouts, rich-text cells through `TST_TEXT_REF`)
- Run tables follow upstream exactly: a paragraph-style entry without a reference clears the style, so a template paragraph whose style Pages left unreferenced is body text, and an anonymous (unnamed) paragraph style is body text
- iWork '09 `index.xml(.gz)`:
  - `sf:p` runs walked node by node (`sf:span` character styles, `sf:link`)
  - `sf:ghost-text(-ref)` placeholders skipped (#366, docling#4170)
  - `sf:tabular-model` grids
  - `sf:header/footer/footnotes` furniture
  - `sf:annotation` comments anchored via `sf:annotation-field`
- JSON: body order, body texts, tables, notes-layer comments and their back-refs identical to upstream's groundtruth
- **two shared-exporter caveats**:
  - furniture-layer items (page header/footer, footnote) stay out of the JSON as for the other flat-node backends (only the PDF pipeline's page headers/footers reach it)
  - text items carry no `formatting`/`hyperlink` fields (the formatting reaches Markdown and DocLang)
- A password-protected package fails with docling's message (Pages hides encryption behind an undefined compression method)
- Mixed-run paragraphs render as docling-core 2.106 does (#997): each run keeps its own whitespace, its markers hoisted inside it, and the runs concatenate (so `Plain ` + `bold` reads `Plain **bold**`)
- **Keynote: 5/5 exact Markdown and structurally identical JSON** on upstream's `tests/data/keynote/` corpus (docling#4330 + charts docling#4376, 2.130+; #466) through `keynote.rs`, `iwork_charts.rs` (the shared `TSCH.ChartDrawableArchive` reader: kind → classification label, the grid by row or column, the title when shown) (the presentation model and its emission), `keynote_iwa.rs` (Keynote 6+ `Index/*.iwa`, also Keynote 2018+'s index zipped a second time into `<dir>/Index.zip` with the `Data/` members left beside it) and `keynote_xml.rs` (iWork '09 `index.apxl`, optionally gzipped), mirroring upstream's `IWorkKeynoteDocumentBackend`:
  - a `chapter` group `slide-{i}` per slide and a page of the slide's own size (1024×768 when unreadable)
  - the slide's drawables in **reading order** — down, then across, top edges within 3.6 pt banding a row, adjacency measured against the previous drawable, unpositioned ones last — each item with the `TOPLEFT` box of the drawable holding it and a `charspan` over its text
  - the **title placeholder's** paragraphs `title` and every other paragraph `text` (theme style names are localised, so the placeholder decides)
  - lists from the list-style run table **including the ladder a style inherits from its parent** (Keynote leaves the bullets on the theme's style; iWork '09 spells it `sf:parent-ident` through the paragraph style)
  - tables and pictures placed like any drawable
  - **presenter notes** as notes-layer `text` items with a zero box
  - **comments** (Keynote's sticky notes, `[author]: text` for 2013+, plain text for '09, which records no author) as notes-layer texts without provenance
  - the slide-number placeholder and everything a master slide draws left out
  - an iWork '09 table keeps every datasource cell's grid position (a number cell is an empty cell, not a shift along the column)
- Charts (`TSCH.ChartDrawableArchive`, docling#4376) are not read yet
- **Numbers: 4/4 exact Markdown and identical JSON** on upstream's committed `tests/data/numbers/` groundtruth (docling#4113 with docling#5087 and docling#5101; #1031), the three fixtures upstream ships without one matching a live run of upstream's `bcb3678` too, through `numbers.rs` (the sheet model and its emission), `numbers_iwa.rs` (Numbers 3+ `Index/*.iwa`), `numbers_xml.rs` (iWork '09 `index.xml`, optionally gzipped) and `iwork_cells.rs` (the packed cell storage, shared with the Pages tables), mirroring upstream's `IWorkNumbersDocumentBackend`:
  - each sheet a page (sized to what is drawn on it) and a `sheet` group; `page_range` selects sheets, keeping their page numbers
  - tables as **real grids**: caption from the table's name, `TOPLEFT` box of the frame placing it, Numbers' own header rows and columns (`column_header` above them, `row_header` beside them), sorted down the sheet with the charts and pictures (drawables are stored in z-order)
  - typed cells rendered as docling renders them: numbers without a trailing `.0` or binary noise, a **decimal128 exactly** (all 34 digits, no decimal context), dates `YYYY-MM-DD HH:MM:SS` from the 2001 epoch, durations as Python's `timedelta`, booleans `True`/`False`, the cached result of a formula; a date or duration no datetime can hold (a corrupt `1e308`, `NaN`) leaves its cell empty
  - both cell layouts (version 4 before 2017, version 5 since) and the tile b-tree, rows offset by their tile; an iWork '09 datasource's sparse rows placed back by the grid's per-column occupancy counts, `sf:col-span` spans, pop-up menu cells through the choice they point at
  - a cell **filled with an image** a rich cell (an `unspecified` group `rich_cell_group_{table}_{col}_{row}` holding the cell's text, if any, then the picture, both in the table's frame), the image found under its code page 437 reading when the member name is UTF-8 without the flag; colour and gradient fills make nothing
  - **charts** as pictures classified by kind (`TSCH.ChartType`, or `sf:chart-type` in the order of Keynote '09's scripting dictionary), carrying the data they plot as a table (the cached `.chrtshr` property list for iWork '09, read in `sf:chart-direction`) and captioned with their title
  - pictures (`TSD.ImageArchive`, a group's pictures in the group's frame; `sf:media` on the sheet's drawables layer)
  - **sticky notes** as `comment_section` groups `comment-{sheet}-{n}` holding a notes-layer text, `[author: …, time: …]: text` for 2013+ (iWork '09 recorded neither)
- deliberate divergences:
  - pictures keep the bytes the document stores (upstream re-encodes every one to PNG through Pillow), as for Pages and Keynote — the JSON differs only in those `image.uri`/`mimetype` values
  - the iWork backend options upstream adds with it are not ported: `sheet_names`, the `max_*` archive limits (our ZIP limits are `DOCLING_RS_ZIP_*`), and `render_chart_images` (LibreOffice redraws a chart from its data); a chart's picture carries no image
  - nested-dir and zipped-package (`Index.zip`) layouts still unwrap (upstream rejects them)
- the old text-level reader (sheet/table headings and a list of the shared strings) is gone

#### HEIC/HEIF (.heic, .heif)

**Backend:** libheif via `docling-pdf/heif` (opt-in cargo feature, #211)\
**Status:** **docling.rs extension** (docling reads HEIC only where Pillow can)

- content-sniffed (`ftyp` brands — misnamed `.jpg` iPhone photos still route correctly), decoded to RGB and fed to the standard image ML pipeline
- without the feature the error says `rebuild with --features heif` instead of a generic decode failure
- Native dependency, so wasm/default builds stay pure Rust


Shared OOXML infrastructure (`ooxml.rs`): a `zip` reader, `.rels` parsing, part
content-type resolution, and image extraction — reused by DOCX/PPTX/XLSX/EPUB.

### ML formats — `docling-pdf`

These run docling's *discriminative* PDF pipeline ported to ONNX. They are now
measured **byte-for-byte against docling** (the committed PDF groundtruth mirrors
upstream's; live docling 2.150 differs from it on five files): **9 / 21
exact, 511 diff lines** against the groundtruth, 6 / 18 and 475 against live docling 2.150, the rest close — see `PDF_CONFORMANCE.md`. A deterministic snapshot baseline
(`scripts/conformance/pdf_conformance.sh`) still guards against regressions.

#### PDF

**Status:** **pure-Rust text parser** (`textparse.rs`, font-advance glyph boxes) + page render → RT-DETR layout (ONNX) → **TableFormer** table structure (ONNX) → PP-OCRv3 OCR for scanned pages → **docling-parse line sanitizer** (`dp_lines.rs`) + reading-order assembly

- page render: the **pure-Rust renderer** by default — the docling-parse renderer plugin (the canvas docling 2.123+ feeds its models, the renders the baselines are pinned to, #478, `PDF_CONFORMANCE.md`) is a development oracle selected with `DOCLING_RS_RENDERER=docling-parse`
- `render/`: content streams, clips, transparency as docling-parse flattens it, TrueType/CFF/Type 1/Type 3 glyphs and host fallback faces, shadings, patterns, images, widgets
  - host fallback faces: `.models/fonts`, `DOCLING_RS_FONT_DIRS`, then the host's
  - #633: `DOCLING_RS_SYSTEM_FONTS=0` stops at the first two, so a deployment that ships its fonts renders the same layout input on every host
  - the reporter's paper scored its title 0.77 vs 0.83 and found a footnote on Linux only, Liberation Sans vs Arial; pinning the directories made Windows byte-identical to Linux
  - the published Docker images set `=0` with the two packages they install pinned in `DOCLING_RS_FONT_DIRS`
  - `DOCLING_RS_DEBUG=1` names the face each style resolved to
- mean |Δ| 0.84/255 per channel against the shim over the snapshot corpus
- on the shim-rendered baselines 75/101 snapshots exact and 647 groundtruth diff lines where the shim itself is 101 / 511
- Type1C fonts carrying the deprecated CFF `dotsection` operator draw their periods and i-dots, #531
- pdfium's chain only in a build with the opt-in `pdfium` feature under `DOCLING_RS_RENDERER=pdfium`
- #419: layout boxes are refitted to the cells they claim, empty ones dropped and contained orphans folded in before the reading order runs, as docling's `LayoutPostprocessor` does
  - a model box that cut a line in half no longer strands that line after its paragraph
  - 2206 82→52, 2305 20→18, normal_4pages 20→16 diff lines vs groundtruth
- `--pages A-B` (docling's `page_range`, #80) converts a 1-based page window, skipping the rest before rasterization
- `--document-timeout SECONDS` (docling's `document_timeout`, #497) is the per-document budget checked between pages — once spent the pages done so far are the document and the result is `PARTIAL_SUCCESS` with docling's `ErrorItem`, on every surface
- `--images referenced` streams each page's image files to the artifacts dir as the page is emitted (memory-bounded, #80)
- `--ocr-lang en|ch` picks the OCR recognition model (en default — the ch_ conformance model glues Latin words)
- scanned pages with `/Rotate` are un-rotated to upright before layout/OCR and their geometry mapped back to display coords (all four orientations of `tests/data/scanned/` OCR to the same groundtruth text)
- table captions attach by reading-order adjacency (docling's `_find_to_captions`, #265) and ride on the table across all exports — Markdown above the grid, JSON `TableItem.captions` refs, DocLang `<caption>`
  - #1114: a table with no text-layer cell (one pasted as an image, converted without OCR) stays an empty table item and keeps that caption, as in docling; it used to be dropped with the caption it had been paired with
- the JSON carries page headers/footers as body-parented `page_header`/`page_footer` texts on the furniture layer, as upstream writes them (Markdown still leaves them out)
- the text inside a picture reaches the JSON as that picture's children (docling's `_add_child_elements`), as upstream writes it (Markdown, like docling's, prints only the caption)
- text outside the page's display box — the CropBox, else the MediaBox — is dropped glyph by glyph like docling-parse does (#529: a FrameMaker print slug, a tiled page's neighbouring text) instead of being clamped onto the page edge
- a CJK composite font without ToUnicode is read through Adobe's CID → Unicode table of the collection its `CIDSystemInfo` names (Japan1, GB1, CNS1, Korea1, KR; ISO 32000-1 §9.10.2), as docling-parse does
  - deliberately for every descendant font, `CIDFontType2` included, as docling-parse does
  - some producers name an Adobe collection on a subset TrueType whose CIDs are really glyph ids (`CIDToGIDMap` Identity or absent); without ToUnicode such a font reads as plausible but wrong text, as it does in docling, where docling.rs read nothing before and OCR could step in
- symbol fonts read as docling-parse 7.25 reads them (#1113, docling-parse#431)
  - a Symbol / ZapfDingbats `/ToUnicode` into the private-use symbol charset reads through the face's encoding: redp5110's `U+F0AE` is `→`, a Word bullet `U+F0B7` is `•`
  - a symbolic simple font that declares no base encoding reads through its embedded program's builtin encoding, after `/Differences` and `/ToUnicode`: 2203.01017's `Dingbats` `3` / `7` are `✓` / `✗`
  - a Symbol / ZapfDingbats font that names no encoding reads through the face's own (a non-embedded ZapfDingbats' `l` is `●`) and, without `/Widths`, advances by the face's AFM widths
  - Skia's hexadecimal glyph-index names (`/g15C`) read as unmapped, `�`
  - **deliberate divergence:** a declared encoding keeps its table; docling-parse reads a font matched to Symbol through the face anyway (its FontSpecific scheme), which turns `multi_page`'s MacRoman `SymbolMT` bullet into `- ∞ IBM MT/ST …`
- #598: a `JPXDecode` (JPEG 2000) image is decoded (`render/jpx.rs`, `hayro-jpeg2000`, within ±1 of OpenJPEG) instead of drawn as a mid-gray placeholder
  - on photo pages the blank rectangle left the layout model unsure of the picture (0.64 instead of 0.99): its box ran over the caption below, which nested inside as `text` and vanished from the Markdown, and two stacked photos split by a caption fused into one
  - decoded, the reporter's NASA pages give docling's result (caption linked and printed, two pictures) at int8 and fp32 alike — `crates/docling/tests/jpx_pictures.rs`
  - JPX scans (image-only pages) now reach OCR as pixels too
  - the floor moved to rustc 1.92 for the decoder

#### Images (tiff/webp/png/jpeg/gif/bmp)

**Status:** the same pipeline, image as a single page

- docling#4247 (2.128): the EXIF orientation tag is applied when the frame is loaded (`ImageOps.exif_transpose`), so a portrait photo no longer reaches layout and OCR on its side
- #981: a detected line is cropped out of the detector's rotated rectangle (RapidOCR's `get_rotate_crop_image`), not its axis-aligned bounding box — on a scan skewed by 0.6° the box of a 700 px line of 12 px print was a third taller than the line, and the recognizer, fed the line squeezed and slanted, dropped letters and glued words along every long line (`Baccalaatpoesne accurlatci usgers`) while RapidOCR read the same page clean with the identical models; the recognizer input width is no longer capped at 2400 px either (RapidOCR has no cap)
- #570: OCR reads an image at docling's effective resolution (3 px/pt within RapidOCR's 2000 px longer side) and recognizes the text detector's boxes
  - on FUNSD's 199 scanned forms word recall against the annotations (the issue's `funsd_repro.py` scoring, `text_items`) is **0.896**, median 0.933, 29 forms under 0.8 (docling-rs 1.96: 0.613 / 150 under 0.8; Python docling 2.133 with RapidOCR: 0.852 / 39 under 0.8)
  - 0.914 on the nine forms #571 lists against Python's 0.915
- #571: the orientation probe runs on the detector's boxes with a confidence margin, so no upright form is turned any more (9 of 199 were)
- #1093: the page is sized from the file's resolution, as docling's `ImageDocumentBackend` sizes it — `pixels × 72 / dpi` points per axis, the resolution read as Pillow's `info["dpi"]` (a PNG `pHYs`, a JPEG's JFIF density else its EXIF, a BMP's pixels per metre, a TIFF's resolution tags), 72 when the file records none or the `(1, 1)` / `(0, 0)` placeholders, the axes swapped when the EXIF orientation turned the frame a quarter. The QR bill (1021 × 1423 px at 300 DPI) is a 245.04 × 341.52 pt page, its JSON boxes docling's; an image without a resolution stays 1 px = 1 pt, as before and as in docling. Picture crops and the page image keep their pixels, their `dpi` rising by the same factor
  - **deliberate divergence:** docling also feeds its models the image *resampled to that page* — layout at 1 px/pt (245 × 342 px for the bill), OCR at 3 — and at that size its layout model reads the bill as two pictures (two `<!-- image -->` placeholders in its Markdown, the text in their children). The models here keep reading the file's own pixels, as before: only the page unit changes, so the text, the reading order and the Markdown cannot move (the bill reads as text)
  - a resolution docling rejects (not finite, zero or negative on one axis) fails its conversion; here it reads as 72

#### METS / Google Books

**Status:** `.tar.gz` of per-page hOCR + TIFF → cells from hOCR → the same layout+assembly path

- no OCR needed

#### Audio (wav/mp3/flac/ogg/aac/m4a) and video audio tracks (mp4/mov/mkv/webm — docling's `InputFormat.VIDEO`, Phase 1 of #138)

**Status:** `docling-asr`: **symphonia** decode (no ffmpeg) → 16 kHz mono → ported log-mel front-end → **Whisper tiny** encoder/decoder (ONNX, greedy with OpenAI's timestamp rules — docling's ASR defaults) → `[time: start-end] text` paragraphs in Markdown

- the JSON writes each segment as docling 2.135's ASR pipeline does (#614)
  - a text item of the words with the timing as `source: [{"kind": "track", "start_time", "end_time"}]` (it used to be the `[time: …]` string with no timing field)
  - blank segments without a track and a zero-length one stretched by docling's 1 ms, which `--to vtt` writes as subtitles
- Frames (Phase 2 of #138): when the `ffmpeg` binary is present at runtime, up to `--video-frames N` (default 8) scene-change frames (evenly spaced fallback) interleave with the transcript as `[time: <ts>]`-captioned pictures with embedded PNGs
  - #648: the scene scan covers the whole timeline — it used to stop at the N-th cut, so a long lecture's eight frames all came from its first minutes
  - more cuts than frames are spread over N equal windows, the sharpest cut per window, the first frame first, a window's midpoint when it has no cut; N−1 or fewer cuts are all kept as before
  - the chosen frames are decoded with one accurate seek each
  - no ffmpeg → transcript only, no audio track → frames only
- Codecs symphonia can't decode in-process — Ogg Opus, AVI containers — go through the same optional ffmpeg binary when present (#190); without it they fail with a targeted install hint
- Transcription language: auto-detected per file from the first 30-second window (Whisper's `language=None` / docling 2.116 default, #180)
  - pin with `asr_lang` (all surfaces) or `DOCLING_RS_ASR_LANG`
  - English-only presets skip detection
- **Parakeet TDT 0.6B v3** (#508, `asr_model=parakeet_tdt_0.6b_v3`, beyond docling's specs): NVIDIA's multilingual transducer for 25 European languages (language detected by the model), a port of onnx-asr 0.12
  - NeMo 128-mel preprocessor in Rust (≤ 2·10⁻⁵ from onnx-asr's NumPy one)
  - TDT greedy decoding (same tokens/frames as onnx-asr on the same encoder output; with the fp32 graphs the whole pipeline is token- and timestamp-identical on the en/de/ru fixtures)
  - Silero VAD spans (≤ 20 s; energy-based pause splitting without it)
  - sentence segments
- Models: `download_dependencies.sh --asr-model=<preset>`, Python `download_models(asr_model=…)`, Docker `--build-arg ASR_MODELS=…`


### DocLang (`.dclx`) coverage

The `.dclx` DocLang output (§3) is scored against docling's own `.dclx` archives
with `scripts/conformance/dclx_conformance.sh` — the extracted `document.xml`
line-diffed, similarity `= 100·(1 − difflines / max_lines)`. Against docling
2.148's archives (DocLang 0.9, `groundtruth_dclx` regenerated with
`gen_dclx.py --force`): **132/146 byte-exact, 99% mean over the non-PDF
corpus** (issue #32 target: ≥90%), per source format. What is left: the
alt-text `<description>` (DOCX and ODF, a divergence above), the
spreadsheet and slide captions' `<location>` (the flat nodes carry none),
`xlsx_10`'s in-cell images (docling#5015) and the `elife-56337.txt`
routing. A float's footnotes ride in a `<group>` with it, after it (the
port has no provenance to place one above), as `_serialize_floating_satellites`
writes an unplaced footnote.
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

| Format | `.dclx` exact / similarity | Format | `.dclx` exact / similarity |
|---|---|---|---|
| CSV | **9/9**, 100% | JATS | 6/7, 93% |
| AsciiDoc | **5/5**, 100% | Markdown | **11/11**, 100% |
| DOCX | 33/38, 99% | LaTeX | **2/2**, 100% |
| PPTX | 7/8, 99% | HTML | **32/32**, 100% |
| XLSX | 9/14, 97% | WebVTT | **4/4**, 100% |
| USPTO | **9/9**, 100% | ODF | 5/7, 98% |

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

A PDF **list item** is written as docling's `ListItemMarkerProcessor` leaves
it (#1111): `orig` with the marker, `text` without it, the `marker` itself
(`·`, `3.`, `[21]`, `a)`, empty when the item has none) and `enumerated` for
a numbered marker of any spelling. The Markdown spelling is another matter
and unchanged — docling-core's serializer prints `- [21] text` — so the node
carries the JSON's view apart (`Node::ListItem::split`), which only the PDF
pipeline fills; every other backend's items read `-` or `N.` as before.

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
running the same chunkers on the same 103-document corpus
(`scripts/conformance/gen_chunks.py` generates the groundtruth,
`scripts/conformance/chunks_conformance.sh` compares the records' text +
headings + contextualization — the payload an embedding model sees):

| Chunker | Identical chunk records | Fully-exact documents |
|---|---|---|
| Hierarchical | **624 / 676 (92.3%)** | 77 / 103 |
| Hybrid (MiniLM tokenizer, 256 tokens) | **254 / 365 (69.6%)** | 65 / 103 |

Re-measured 2026-11-08 (#1041) against groundtruth regenerated with docling
2.148.0 / docling-core 2.106.0 (`gen_chunks.py`; the earlier 475 / 563 and
224 / 312 were scored on a groundtruth older than the inline whitespace
contract, #997, and the link-URL encoding). A table's captions open its
chunk and are among its items, as docling's `TripletTableSerializer` writes
them (`Table 1: Revenue\n\n2024, Revenue = 10`, #1041). A rich heading (an
empty heading whose only child is an inline group, formula or code — what
docling makes of a partially formatted `## *a* b`) is its body's heading
context, docling-core 2.106's `test_rich_heading_chunk_metadata`
(docling-core#924): it is no longer a chunk of its own under an empty
heading. `inline_and_formatting.md`'s groundtruth is docling-core 2.106's
chunkers over the fixture's current upstream conversion (its `.yaml`).

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
groundtruth chunks) **100% / 100% of docling's chunk records were reproduced
identically** (order-aligned; 89 / 90 and 79 / 116 against the docling
2.148 groundtruth) — the former HTML-backend model gaps (rich table
cells with inline markup and span de-duplication, figure-caption run spacing,
indicator images, `<br>` annotation-boundary handling) are closed. The chunker-era
work (checkbox inputs, fragmented-anchor folding, `<button>` blocks) plus the
#81 parity fixes also lifted the HTML `.dclx` similarity: 88% mean (was 84%).

**Item refs (`DocMeta.doc_items`).** The scripts above compare text, headings
and contextualization; the items a chunk lists — what LangChain's `dl_meta`
carries, with each item's page/bbox provenance — are checked separately.
Natively (#1041) every chunker maps its refs onto the document's own JSON
export: the walk numbers the flat nodes, which is the export's numbering only
without an item tree — with one (HTML, DOCX, Markdown, ODF, JATS, LaTeX, …)
half of the text refs pointed at another item (2,662 of 5,324 on the corpus;
a captioned table also shifted every later ref by one, its caption taking no
ref). `chunker::RefMapper` ports the Python side's mapping below — kind,
reading order and text, an inline paragraph's runs, a paragraph the walk
split, a list item's marker — and leaves out an item it cannot place: on the
corpus 0 of 5,308 text refs resolve to an item whose text their chunk lacks
(2 entity-encoded code blocks aside; 2 chunks list no item, `signature_stamp_01`'s
captions, whose flat text and tree text differ), and of the 624 chunk records
whose text matches docling's, 592 list exactly docling's `doc_items` (345 /
622 before). The mapping builds the JSON export once per chunking (+90 ms
on `wiki_duck`).

On the Python side, the engine chunks a re-imported copy of the document whose
item numbering drifts from the caller's (empty items dropped, HTML/DOCX item
trees, inline runs sharing their paragraph's number), so `docling_rs.chunking`
maps every chunk item back to the caller's document by kind, reading order
and text (an item that cannot be placed is left out, never mis-pointed).
Measured with `docling_rs.langchain.DoclingLoader` against docling-core
2.99's Python `HierarchicalChunker` on the groundtruth JSON of all formats:
of the 101 documents whose chunk texts match, 95 also match `dl_meta`
exactly (80 before the mapping); on docling.rs's own conversions (HTML, DOCX,
Markdown, PPTX, XLSX, AsciiDoc, LaTeX, PDF) every listed text item resolves to
text contained in its chunk (~2,600 items checked, 0 mismatches; before the
mapping 86% of PDF, 79% of HTML, 37% of DOCX and 15% of Markdown items
pointed at the wrong item).

## 3. Output formats

### Markdown (legacy)

**API / CLI:** `export_to_markdown()` / default\
**Status:** byte-for-byte docling, quirks included

- A blank line separates two *sibling* lists only where the backend flagged the second (`first_in_list`) — never inferred from a kind flip or a number gap (#385).
- Tracks docling-core 2.97's serializer:
  - a referenced image's link destination is percent-encoded like upstream's `_escape_uri_path` (docling-core#698 — spaces and parentheses in the artifacts dir, `\\` → `/`, absolute Windows and UNC paths as `file://` URLs, URLs component-encoded, never double-encoded; the artifact *file path* handed back stays raw)
  - a table's Markdown header is the leading block of rows on which a `column_header` cell *starts*, stacked header rows flattened per column with ` - ` (`% of Total - Train`), flags that begin below row 0 promoting nothing and unflagged tables keeping row 0 (docling-core#723/#756 — alignment and widths come from the body rows)
  - a row also ends the header block when it carries body text — a non-header cell with text outside the first column — so a form whose first-column labels are flagged `column_header` on every row keeps its values in the body (docling-core#766, 2.97; #604)
- Row 0 is exempt from that check — a partly flagged first row stays the header (`| Region | 2025 - Q1 | Q2 |`) — unless a later row flags its first-column cell, which marks a column of row headers.
- The triplet chunks use the same header block, without Markdown's row-0 fallback for unflagged tables (docling-core#767, 2.102; #778).
- A merged rich cell (one holding several paragraphs, a list, a nested table) repeats in every grid position it covers, in Markdown, LaTeX and the triplet chunks, like a plain merged cell (docling-core#857, 2.102; #777) — it used to print in its first position only.
- The pivot-table deviation this port carried is gone: docling#4216 flags a spanning `<th>` row as `row_header` rather than `column_header`, so upstream no longer folds a pivot table's first data row into the header (`Year - 2025 | Month - January`).
- `header_row_count` is now docling-core's rule verbatim (docling-core#765 resolved on the backend side).
- A single newline inside an item's text is a GFM hard line break (`"  \n"`, docling-core#721 — a blank line stays a paragraph break, a heading collapses its newline to a space).
- A heading inside a rich table cell renders as plain text (docling-core#540).
- Field regions / field items emit nothing of their own — no more `<!-- missing-text -->` markers (docling-core#724).
- The hard-break marker is Markdown-only: JSON cell text and LaTeX cells see the raw newlines.
- Code fences and spans are docling-core 2.103.1's (#859): a code item's fence is three backticks or one more than the longest backtick run in its text (`` ````md `` around a body quoting a ``` fence), an inline code span one backtick more than the longest run in it, space-padded when the text starts or ends with a backtick (`` ``a`b`` ``, `` `` `x `` ``) — `docling_core::code_fence` / `code_span`, which every backend baking a span uses; the readers of the baked text (the plain export, DocLang runs, LaTeX, the chunker) take the longer form back, so no fixture moved.
- Hyperlinks are docling-core 2.104's (`is_safe_hyperlink` / `markdown_link_destination`, #860): a destination whose scheme is not `http`, `https`, `ftp`, `ftps`, `sftp`, `mailto`, `tel` or `file` (`javascript:`, `data:`, `vbscript:`, read the way a browser reads it — leading controls and spaces ignored, tabs and newlines removed, case-insensitive; a relative reference and a Windows drive are fine) keeps its text and loses the link; a live destination is written percent-escaped (`[Duck (film)](/wiki/Duck_%28film%29)`, spaces, `<>"\` and controls too, an entity's `&` as `&amp;`). `docling_core::hyperlink` is the one helper, and every backend that bakes `[text](url)` into an item's text goes through it, so LaTeX (`\href`), the Pandoc AST (`Link`) and the chunker (a hyperlink item) see the same verdict; the JSON export keeps the raw `hyperlink`, as docling's model does.

### Markdown (strict)

**API / CLI:** `.strict(true)` / `--strict`

- Rust-only cleaner mode — **no docling equivalent**.

### Markdown (options)

**API / CLI:** `export_to_markdown_with_options(&MarkdownExportOptions)` · `MarkdownStreamer::with_export_options` (#599) · `DocumentConverter::convert_streaming_with_options` · CLI `--layers body|all|LIST` / `--traverse-pictures` (#661, md/text/html, HTML takes the layers only)

- docling-core's `MarkdownParams` beyond the image mode:
  - `layers` (`included_content_layers` — page headers/footers, comments and hidden sheets render through the body's serializers when their layer is in the set; a hidden sheet's items take the sheet's layer, as upstream's do)
  - `traverse_pictures` (`CommonParams.traverse_pictures` — a picture's nested text items print after its caption and image, the way `iterate_items(traverse_pictures=True)` yields them; the reporter's bordered FUNSD form, one picture with 36 text children, is empty without it)
  - `escape_html` / `escape_underscores` (the backends escape at build time, so turning one off *undoes* that escaping — `&amp;`/`&lt;`/`&gt;` and `\_` — on text items, headings, captions, list items and form fields, never on code, formulas or table cells, like upstream's `post_process`)
  - `image_placeholder` (an empty one prints nothing: like docling-core's `"\n\n".join(p.text for p in parts if p.text)`, an item that renders empty adds no blank lines, while it still counts for the page-break placeholder, #605)
- Defaults are upstream's: the default export is byte-identical.
- The CLI sets the layers and picture traversal (Python's CLI has no such flags; docling-core's `export_to_markdown(included_content_layers=…, traverse_pictures=…)` does), streamed and buffered alike; serve, Node and wasm keep the default export.

### JSON

**API / CLI:** `export_to_json()` / `--to json`

- docling-core native wire format (schema 1.11.0).
- **Inline whitespace contract** (docling-core 2.106 / docling 2.148, #997): an `inline` group's runs carry their own boundary whitespace and the Markdown, HTML, LaTeX and Pandoc exports concatenate them — a run's edge whitespace hoisted outside its formatting and hyperlink markers (`**bold** tail`, never `**bold **`), a whitespace-only run left undecorated. The backends keep the boundaries as docling#3891's do (HTML both walks, Markdown, DOCX, JATS, ODF, AsciiDoc, LaTeX, WebVTT, the DocLang reader per DocLang 0.9's rule, Pages/Keynote); chunk text follows the Markdown, so a persisted index built before this release drifts.
  - A backend whose runs are still the trimmed ones the old `" "` join expected (an extension backend not yet moved over) leaves `ItemTree::inline_whitespace` unset, and the JSON export re-injects that join into its runs exactly as docling-core migrates a pre-1.11 document on load — an extension of ours, so such a tree's output reads as before instead of gluing words.
  - Reading docling JSON back: a document stamped below 1.11.0 is migrated the same way (`_migrate_legacy_inline_separators`: one `" "` per boundary, on a plain neighbour, never inside a code or formula run, a standalone run between two delimited ones, `orig` untouched); versionless input is left alone.
  - DocLang 0.9 reads and writes the same contract (§ DocLang): the writer serializes the item tree's runs, the only ones that carry their whitespace.
  - One deliberate divergence: a Markdown paragraph that ends in two hard-break spaces right before a table keeps them upstream (`"Line one  "`, a block edge marko leaves untrimmed there); ours trims every block's edges.
- Reading it back: the `body` tree is walked once per item — a `$ref` reached through two parents (a DAG, 2^n paths on a crafted file) or a cycle renders its subtree once, at the first reference, and nesting stops at 256. docling-core 2.103.1 instead rejects such a file at load (`validate_tree`, #861); converting it is the deliberate divergence — the content is still a document — and a crafted one costs linear time either way. A table declaring more cells than `DOCLING_RS_MAX_TABLE_CELLS` / `_MAX_TOTAL_TABLE_CELLS` is rejected like upstream.
- A reference reads under either key, `$ref` or `cref` (#1014: docling-core's `RefItem` is `populate_by_name`; upstream fixtures such as `table_footnotes_496.json` are written with `cref` and used to convert to an empty document).
- Floating items' footnotes (docling-core#675, 2.106; #1009): a table's `footnotes` are `footnote` text items under it, written after it and listed in its `footnotes`; Markdown prints them after the grid, one paragraph each, HTML as `<div class="footnote">` after `</table>` (inside the `<figure>` for a picture; a code item with captions or footnotes is wrapped in a `<figure>` holding its `<figcaption>` and notes), and the hierarchical chunker closes the table's chunk with them (they are among its doc items). `table_footnotes_496.json` matches docling-core 2.106 in Markdown, HTML and chunks. **Divergence:** a docling-JSON input's *picture* or *code* footnotes are read as standalone `footnote` items right after the element — the Markdown is docling's, but the re-exported JSON / HTML no longer link them to the item (the flat picture/code nodes carry no footnote list; no backend of ours produces one).

### Plain text (`.txt`)

**API / CLI:** `export_to_text()` · `MarkdownExportOptions::plain_text()` / `--to text` (#613)

- docling's `--to text` — docling-core's `PlainTextDocSerializer` (`export_to_text()`): the Markdown walk without `#`, emphasis / strikethrough markers, link URLs, code fences and backticks, image placeholders, GFM hard breaks or HTML/underscore escaping; bullets, numbers, checkbox marks and table grids stay.
- No trailing newline, like upstream's `<stem>.txt`.
- **134/135** declarative fixtures (those whose Markdown matches) identical to `export_to_text()` on the upstream groundtruth JSON; 15 pinned in `tests/plain_text.rs`.
- Our nodes carry inline Markdown text where upstream carries formatting flags, so the markers are taken back out by a parser.
- A marker counts only with content tight against it (CommonMark flanking), so literal text such as IBM i's `*USE … *OBJMGT` or `2 * 3 * 4` survives — the deliberate cost is docling's own emphasis around bare whitespace (WebVTT's `* *`), which stays literal.

### DocLang (`.dclx`)

**API / CLI:** `export_to_doclang()` · `docling::dclx::save_as_dclx()` / `--to dclx`

- DocLang 0.9 XML (`<doclang version="0.9">`) as docling-core 2.106 (docling 2.148) writes it, and the OPC archive its `save_as_doclang_archive()` writes — with its `assets/image_NNNNNN_<sha256>.png` parts:
  - same names (index over every body picture, digest over the decoded pixels for PNG and JPEG)
  - pixel-identical content, for all 25 picture references of the upstream `groundtruth_dclx` corpus but one furniture data URI whose PNG compression bytes differ
- **DocLang 0.9 whitespace** (docling-core#693): a whitespace sequence in a text block reads as one space, none at its edges, `<content>` literally. The writer builds the minified markup — an inline group's runs concatenated as they are, a run wrapped in `<content>` when its edges are whitespace or it holds anything but single spaces — then runs `serialize_doc`'s post-processing (`docling-core/src/doclang_xml.rs`): every `<content>` that reads the same bare is unwrapped (`_unwrap_redundant_content`), and the tree is pretty-printed with each text block's body on one line (`_pretty_print`; a line break inside a body would read as a space: `2<superscript>nd</superscript>`, not `2 nd`).
- **The item tree is the source** wherever a backend builds one and the port covers it (`docling-core/src/doclang_tree.rs`, docling-core's `DocLangDocSerializer` call for call): only the tree's runs carry their whitespace. The located documents (PDF, the spreadsheet and slide backends) and the ones without a tree go through the flat writer, which lays each text block body out as one fragment and runs the same post-processing. One exception keeps the nodes: a USPTO file whose tables docling empties (its table regex misses them, `ItemTree::doclang_nodes`) — the DocLang follows the Markdown there.
- Layers: DocLang has no `notes` or `invisible` layer — they are written as `furniture` and `background` (docling-core#800), lossy as upstream.
- A float's head holds every caption (DocLang 0.8, docling-core#880) — `<label>` (a picture's classification), `<href>`, `<layer>`, the location block, the `<caption>`s, a picture's `<description>` — then the content; a chart is `<picture class="chart">` with its `<tabular>` after the `<src>`. A picture off the body keeps its image as a data URI in the archive, as `_with_pictures_refs` leaves it.
- Deliberate divergence: a picture's alt text (our extension, #676, `created_by: alt_text`) is its `<description>`, as docling-core writes any `meta.description` — docling drops the alt text, so its archive has none.
- Every PDF of `tests/data/pdf/sources` passed the DocLang 0.7.3 reference validator (`doclang validate`, the root given the DocLang namespace); not re-run against the 0.9 schema yet.

### WebVTT (`.vtt`)

**API / CLI:** `export_to_vtt()` · `export_to_vtt_with_options(&VttExportOptions)` / `--to vtt` (#614)

- docling's `--to vtt` — docling-core's `WebVTTDocSerializer` as `save_as_vtt` runs it (`omit_voice_end`):
  - a cue per text item with a track `source` (WebVTT cues, ASR segments)
  - identical timings merged into one cue
  - `<b>`/`<i>`/`<u>` and `<v voice>` spans
  - upstream's two tag-merging rules ported (its regexes use backreferences)
  - a document without timed text is the `WEBVTT [title]` header, as upstream writes it
- **4/4 mirrored WebVTT inputs byte-identical** to docling 2.135 (both `save_as_vtt` and `export_to_vtt` defaults).
- Deliberate divergence: cue text is escaped (`&amp;`, `&lt;`) where upstream re-parses it and fails on a raw `&` or `<`.

### LaTeX (`.tex`)

**API / CLI:** `export_to_latex()` / `--to latex` (#317)\
**Status:** docling 2.124's `LaTeXDocSerializer` with default params, scored against upstream's own `docling --to latex`: **93/116 shared declarative fixtures byte-exact**

- 98 modulo upstream's duplicated formatted list items / headings — #328's HTML `cell_blocks` made the rich-cell bucket exact: in-cell lists render as `\begin{itemize}`, nested tables as nested `tabular`s, in-cell headings as plain text.
- Remaining gaps are underline / sub / superscript (no Markdown form) and a few list groupings.
- Serve `to=latex`, Node `to: 'latex'`; Python bindings use upstream docling-core's serializer on the reconstructed document.
- Deviations:
  - headings deeper than `\subsubsection` degrade to `\paragraph`/`\subparagraph` where upstream raises
  - upstream's duplicated text for formatted items ([docling-core#740](https://github.com/docling-project/docling-core/issues/740)) is not reproduced
  - docling-core#743 (2.95) fixed the inline-group double serialization (docling-core#740) that this port never reproduced, so the two agree again
- Pictures and charts follow docling-core 2.102 (docling-core#871, #779):
  - the figure carries `% annotation[classification]: …` / `% annotation[description]: …` comments after its caption
  - a chart's data grid follows the figure as a `table`
  - an alt-text description (#676, docling.rs-only) is never printed
- Title hoisting follows docling-core#983 (main, after 2.106.0; #1013): the first `\title{…}` is closed by brace depth (escaped `\{` `\}` skipped), so a formatted or brace-carrying title reaches the preamble with one `\maketitle` and every later `\title` leaves the body; released docling-core's `[^{}]*` regex still leaves such titles inline (`inline_and_formatting`, `2310.06825`)

### HTML (`.html`)

**API / CLI:** `export_to_html()` · `export_to_html_with_images(mode, dir)` · `export_to_html_with_layers(ContentLayers)` · `export_to_html_with(&HtmlExportOptions)` / `--to html` (#492, #499)

- docling-core's `HTMLDocSerializer` with default params (single-column style, body layer), rendered from the JSON structure the JSON export already reproduces.
- #499: `HTMLParams.layers` / `export_to_html(included_content_layers=…)` — `ContentLayers` picks the body/furniture/notes/invisible layers rendered (default body only, byte-identical to before), extra items going through the body's serializers like upstream's
  - pinned against docling-core 2.103.1's output for DOCX header/footer, comment and all-layer exports (`html_layers_match_docling_core`, 3/3 byte-identical)
  - library API and the CLI's `--layers` (#661), the other surfaces still export the body layer
- Pictures per the image mode like Markdown (`placeholder` = upstream's default, pictures out but for caption and meta).
- Pinned byte-for-byte against upstream's HTML groundtruth for the ODF/DOCX fixtures (8/8, picture payloads masked — upstream's `save_as_html` re-encodes them through PIL) and against docling-core 2.103.1's own `export_to_html(image_mode=EMBEDDED)` over our exported JSON for the regression corpus: **378/392 byte-identical**, the rest only in picture payload bytes (PIL re-encodes the PNG on load), two Wikipedia hrefs escaped as docling-core 2.104 writes them (`&amp;`, #860) and one XBRL instance nested deeper than Python's recursion limit
  - formulas as MathML through `docling_core::mathml`, a port of the `latex2mathml` 3.81 library upstream runs (checked against the Python package on every corpus formula and a synthetic suite, inline and block, error cases included; `<annotation encoding="TeX">` + `<div>` block wrapper as docling-core adds them, `<pre>{latex}</pre>` where the library raises)
  - a referenced picture's `src` is the Markdown destination HTML-escaped (`Q&amp;A,%20v2+final_artifacts/image_000000.png`, a URL keeping its `https://`, no `%` encoded twice — docling-core#772 2.102, #781; Python's plain `quote` used to give `Q%26A%2C…`)
- docling-core 2.103's order: a plain item's formatting and hyperlink wrap its text inside its tag — `<p><strong>bold</strong></p>`, `<h2><em>…</em></h2>` — where 2.99 wrapped the tag (`<strong><p>bold</p></strong>`); code, formulas and list items keep theirs (upstream's regenerated `docx_rich_cells` reference). The groundtruth comparison drops embedded `<img>`s on both sides: references written since 2.103 leave out the image of a picture its local-image opt-in did not load
- An item's `hyperlink` is written as docling-core 2.104's `<a href>` (#860): `html.escape`d (`&amp;`, `&quot;`, `&#x27;`), and left out — the text alone — when the scheme is not on the allow-list above (`javascript:`, `data:`).
- Served as `text/html` (`to=html`), Node `to: 'html'`, wasm `"html"`; Python needs nothing (`result.document` is upstream's).

### Pandoc AST (`.pandoc.json`)

**API / CLI:** `export_to_pandoc_json()` · `export_to_pandoc_json_with(&PandocExportOptions)` · `docling_core::pandoc::from_docling_json` / `--to pandoc [--pandoc-api-version 1.23]` (#515)\
**Status:** Rust-only — **no docling equivalent**

- The JSON serialization of Pandoc's `Pandoc` type (`pandoc-api-version` 1.23.1.1, Pandoc 3.x) for `pandoc -f json -t docx|odt|epub|rst|typst…`, walked from the same docling-JSON structure as the HTML/LaTeX exports:
  - headings, nested/ordered lists, inline formatting and links, code, inline/display math, checkboxes
  - tables (`TableHead`, row/col spans, rich cells, captions)
  - figures (`Para [Image]` / `Figure`; pictures embedded unless the image mode says otherwise, and an `Image` even without a target — classed `docling-placeholder` — so `pandoc -t docx` keeps every picture, #537; chart data as a table)
  - table/picture footnotes as `Note` in the caption and DOCX footnotes / endnotes and ODT notes as `Note` at their reference (#538; a footnote without a recorded call site, e.g. from Python docling's JSON, as a trailing `Note`)
  - key-value graphs and form field regions as `DefinitionList`s
  - other labels as `Div .docling-<label>`
- Not representable, so dropped: provenance (pages, bboxes), confidence and classification meta, form geometry (comments' authorship and the document's metadata are written since #836).
- Asking for another API version is an error (`unsupported Pandoc API version`), never a document Pandoc would reject.
- Validated:
  - every convertible declarative fixture (305) and the PDF corpus read by `pandoc -f json -t native`
  - 16 documents pinned as JSON + Pandoc's `native` reading, and a DOCX rebuilt by `pandoc -t docx` checked for its pictures and `word/footnotes.xml` (`crates/docling/tests/pandoc.rs`)
- Serve `to=pandoc` (+ `pandoc_api_version`, `application/json`), Node `to: 'pandoc'`, FFI `to=pandoc`, wasm `"pandoc"` (and the browser demo's selector), Python `docling_rs.pandoc.export_to_pandoc(doc)` / `save_as_pandoc(doc, path)`.
- #833:
  - a caption's and a footnote's formatting, links and inline group reach the `Caption` / `Note` (a bold, linked `<caption>` was bare words, a multi-run caption an empty one plus a stray `Para`)
  - a multi-run list item with a nested list is `[Plain, BulletList]` and a multi-run heading with a section under it a `Header`
  - a one-paragraph DOCX cell holding only a `w:hyperlink` stays plain, its link lost, as docling reads it (python-docx's `paragraph.runs` skips hyperlink runs) — docling's HTML of `docx_rich_tables_01` is a reference fixture here, so the cell is not made rich
- #834:
  - the `Image`'s alt text is the picture's description — the author's alt text — else its caption
  - `PictureImage::display_size` (a DOCX drawing's `wp:extent`, an ODT frame's `svg:width` / `svg:height`, inches) is the `Image`'s `width` / `height`, as Pandoc's readers write them (`_display` in the writer's JSON only; docling's `ImageRef` has no such field)
  - an ODT frame's `svg:title` (else `svg:desc`) is the picture's alt-text description, its text left in the paragraph as docling reads it
- #835:
  - `OrderedList`'s `ListAttributes` come from the items' markers — `a)` LowerAlpha OneParen, `iii.` LowerRoman from 3, `(3)` Decimal TwoParens, a bare number DefaultDelim; a single letter is Roman only when the list reads so (`i`, or a multi-letter numeral among the items) — where every list was `Decimal` / `Period` from 1
  - The ODT item tree's marker carries the level's `style:num-format` and `num-prefix` (`a)`, `iii.`, `(3)`, the DOCX twin's marker; docling writes the decimal counter and suffix, so this is **beyond docling** in the JSON, the flat Markdown keeping docling's counter)
  - a DOCX decimal template whose punctuation is a parenthesis keeps it (`(%1)` → `(3)`, `%1)` → `3)`; docling folds both into `3.`)
- #836:
  - the document's metadata is Pandoc's `meta` — `title`, `subject`, `description`, `date` (the document's own, else its creation date), `publisher`, `rights`, `identifier` as `MetaInlines`, `author` and `keywords` as `MetaList`s, `lang` as `MetaString` — whether or not `--metadata json` was asked
  - a reviewer comment on an item is Pandoc's docx reader's `--track-changes=all` shape, a `Span .comment-start` (`comment-id`, `author`, `date`, the comment's text) before the item's inlines and an empty `Span .comment-end` after them, a thread's replies as further pairs, the notes-layer `comment_section` groups never written as blocks
- #848: `--pandoc-heading-depth source` (`PandocExportOptions::heading_depth`, serve `pandoc_heading_depth`, Python `export_to_pandoc(heading_depth="source")`, Node `pandocHeadingDepth`) writes a `section_header` of level *n* as `Header n` (clamped 1–6; the title stays `Header 1`)
  - a DOCX `Heading 1` as `Header 1`, as `pandoc -f docx` reads it, so `--number-sections` and `--shift-heading-level-by` behave alike
  - the default `docling` keeps the title-reserving `Header (n+1)` (docling's own `##` for level 1, what the flat `Node::Heading` round trip and JSON consumers rely on)
- #849: an internal link's target is `#id` and a link destination (a DOCX bookmark, `_anchors` in the writer's JSON) is written as Pandoc's docx reader writes it
  - a bookmarked heading's `Header` carries Pandoc's auto-identifier of its text (`target-section`, so `#target-section` resolves as it does from `pandoc -f docx`)
  - a bookmark in body text or a cell an empty `Span .anchor` with its name
- #850: a heading the DOCX backend numbered through `w:numPr` (docling's `1.2 Title` text, `_number` in the writer's JSON) writes its authored text alone to the `Header`
  - Pandoc numbers sections itself (`--number-sections` gave `1.2 1.2 Title`) and its docx reader writes no number
  - nothing is guessed from the text, so an authored `2026 Results` keeps its digits
  - the Markdown and JSON keep docling's numbered text
- #851: a run kept under `track_changes=all` (`_revision` in the writer's JSON) is `Span .insertion` / `.deletion` with `author` and `date` — Pandoc's docx reader's `--track-changes=all` shape, `Keep [old][new] value.` for the fixture
  - an explicit `accept` / `reject` leaves the reviewer comment spans (#836) out, as Pandoc does (`PandocExportOptions::track_changes`; set by the CLI, serve, Python's `export_to_pandoc(track_changes=)` and the Node addon from the conversion option)
- #852:
  - a Markdown `[label](target)` in an item's text — the flat node chain's hyperlink (`.doc`, AbiWord, …) — is a `Link` (an `![alt](src)` is not)
  - a `Node::Anchor` of the flat chain reaches the AST as `_anchors` on the next text item

### Page breaks in Markdown

**API / CLI:** `DocumentConverter::page_break_placeholder(Some(text))` / `--page-break-placeholder TEXT` / serve `md_page_break_placeholder` / Node `pageBreakPlaceholder`

- docling-core's `MarkdownParams.page_break_placeholder` (`export_to_markdown(page_break_placeholder=…)`, docling-serve's `md_page_break_placeholder`).
- Upstream yields a `_PageBreakNode` between two body items whose `prov.page_no` differ (a group counts by its first item) and substitutes the placeholder as a part of its own, joined by the `\n\n` delimiter.
- docling.rs marks a break between two rendered blocks separated by a page boundary (`PageInfo` per PDF page / sheet, `PageBreak` between slides, DjVu and DocTags pages) — the same result:
  - never first or last
  - empty or furniture-only pages collapse into one break
  - an empty placeholder leaves the doubled blank line upstream leaves
- `None` (default) = no breaks, docling's default.
- Buffered and streamed (`MarkdownStreamer::with_page_break_placeholder`) output byte-identical.

### Image extraction

**API / CLI:** `export_to_markdown_with_images(mode, dir)` / `--images`

- `placeholder` (default) · `embedded` (base64 data URI) · `referenced` (writes PNG files).


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
  the HTML backend's `html_tree.rs`, the DOCX backend's `docx_tree.rs` and,
  alongside its flat walk, the PPTX backend): docling's items in creation
  order with their parents, children, layers, formatting, hyperlinks,
  comment back-refs and — for a backend with page geometry — their exact
  `prov` (a `TreeProv`: the backend's own box, origin tag and charspan),
  serialized as they are — which is how the HTML, DOCX and PPTX JSON are
  structurally identical to upstream's (see those rows). In the PDF/image
  JSON every text item carries its page and box (#609): a picture's, table's
  or code block's caption its own caption region — not the item's, as
  docling's `ReadingOrderModel._add_caption_or_footnote` assigns it, within
  1.5 pt of upstream's boxes on the groundtruth corpus (the 0–511 grid's step) — and a
  checkbox item its region, so chunks made of captions or checkboxes have a
  page. Glyphs stacked at one x (a chart's y-axis ticks, each set by its own
  `cm`) are separate text cells with their own boxes, as in docling-parse —
  the text layer used to glue them into one cell boxed like the first. A
  checkbox item is docling's `checkbox_selected` / `checkbox_unselected`
  text with the bare option label (it was a `text` item spelling `- [ ] …`;
  the task-list marker is Markdown's rendering). And, a deliberate
  divergence (#609): a line marked as a checkbox on the page is its own
  checkbox item even when the layout model read the checklist as one text
  block — marked by a **drawn** square in front of it (a stroked `re`, or
  four stroked edges as ReportLab draws them, 5–24 pt, unfilled or filled
  white; `checkbox.rs` finds them in the text parser's path walk, ink inside
  checks it, a colour-filled square is a legend swatch and one framing a
  letter or a digit a drawing's callout, and neither counts; a lone `x` or
  `v` in the square is a typed tick, and the square stays a checkbox)
  or by a **ballot-box glyph** it opens with (`☐ □ ▢ ◻ ❏ ❐ ❑ ❒` unchecked,
  `☑ ☒ ⊠ ⌧ ▣ 🗹 🗷 🗵` checked; a line with two, `☐ Yes ☐ No`, stays text). The
  glyph leaves the label, and on a region the model did label a checkbox the
  mark sets the state. docling has only the model's labels: on the
  reporter's checklist it prints two garbled paragraphs (`First option Third
  option` / `Second option Fourth option`), here four `- [ ]` items; on a
  `☐ / ☒ / ☑` list it prints `- [ ] ☑ Bread` (Heron reads that box as
  empty), here `- [x] Bread`. Symbol-font boxes that reach the text layer as
  Private Use Area codes or ASCII bytes (Wingdings `o`, `þ`) are not read as
  boxes — without the font the code is ambiguous.
- **Image extraction** is wired for PDF/image (figure-region crops) and DOCX/PPTX
  (embedded blobs) by default, and — opt-in via
  `DocumentConverter::image_sources` (`--image-sources`, #646;
  `fetch_images` / `--fetch-images` is the `remote` tier's alias) — for HTML,
  EPUB, MHTML, JATS, AsciiDoc, ODF, Markdown and email: `embedded` resolves
  `data:` URIs and parts of the same container (EPUB/MHTML entries, email
  `cid:` attachments) with no filesystem or network access, `local` adds files
  under the source file's directory, `remote` adds `http(s)` URLs. Off by
  default, matching docling's `enable_*_fetch=False`. docling keeps two
  booleans (`enable_remote_fetch` / `enable_local_fetch`); the tiers map onto
  them, with three deliberate differences: an absolute local path or `file://`
  URL is **never** read (docling's local fetch reads any path; a document
  converter honouring `/etc/hosts` is the surprise #646 closes), a relative
  path must stay under the source directory after `..` and symlink
  resolution, and a remote fetch can be confined to `image_hosts` (redirects
  included) and is always held to the private-address block-list. Per-document
  limits (`max_image_bytes` 32 MiB, `max_images`, `max_image_total_mb`,
  `min_image_bytes`; `DOCLING_RS_MAX_IMAGE*` / `DOCLING_RS_MIN_IMAGE_BYTES`)
  leave a picture a placeholder rather than failing. **Markdown** images
  (`![alt](data:…)`, inline `<img src="data:…">`, embedded HTML blocks) resolve
  under the same policy — docling's Markdown backend resolves none; the JSON
  tree's `PictureItem` carries the bytes, and in the flat chain a paragraph
  that is one resolved image becomes a picture (so `--images embedded`
  Markdown shows it) while the default output is unchanged. **Email** bodies:
  docling flattens an HTML body to Markdown paragraphs and loses pasted
  images; under `embedded`+ a body whose `cid:` references resolve (an
  `.eml`'s `Content-ID` parts, a `.msg`'s PR_ATTACH_CONTENT_ID attachments
  against its PR_HTML body) is appended as the HTML backend's nodes with its
  pictures in body order — a body without a resolvable image, and every
  default-tier conversion, keeps docling's paragraphs. JSON always embeds
  extracted images as data URIs.

---

### JSON structure and content layers

**JSON structure.** HTML, DOCX, PPTX, ODF, WebVTT, JATS, AsciiDoc, DocLang,
LaTeX and USPTO build docling's item tree — heading nesting, `inline`
groups with `formatting` / `hyperlink`, rich table cells, `furniture` — so
their JSON is structurally identical to upstream's; the flat backends
(XLSX, CSV, EBCDIC) already have upstream's shape. Tables carry first-class
cells (`table_cells`: text, page-point bbox, spans, header roles — #240),
and `DoclingDocument` exposes them for in-place repair before re-export
(#238): `tables_mut()`, `find_cell_by_bbox`, `set_cell_text`,
`update_cell_by_bbox`; `rows`, `structure` and `cells` are public fields.

**Content layers** (#499, #599). The HTML and Markdown exports render the
`body` layer by default, so page headers and footers (`furniture`),
reviewer comments (`notes`) and hidden content (`invisible`) stay out as
upstream leaves them out; `export_to_html_with_layers` /
`MarkdownExportOptions { layers, traverse_pictures, escape_html,
escape_underscores, image_placeholder, image_mode }` take the set to render
— byte-identical to docling-core for the same sets, and the defaults are
`export_to_markdown()` / `export_to_html()` byte for byte.
`convert_streaming_with_options` streams the same Markdown. The CLI's
`--layers body|all|LIST` and `--traverse-pictures` set them for
md/text/html (#661):

```rust
use docling::{ContentLayer, ContentLayers, MarkdownExportOptions};
let html = doc.export_to_html_with_layers(ContentLayers::BODY.with(ContentLayer::Furniture));
let (md, _) = doc.export_to_markdown_with_options(&MarkdownExportOptions {
    layers: ContentLayers::ALL, traverse_pictures: true, ..MarkdownExportOptions::default()
});
```

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
   legacy quirks (dropped code-fence languages, `\_` and
   entity re-escaping); `strict` produces cleaner Markdown. docling has no such
   switch. All conformance numbers are measured in **legacy** mode.

4. **Tables use docling-core's padded GitHub format.** All backends emit the
   width-padded `tabulate(tablefmt="github")` tables that current published
   docling produces (columns padded to header-width+2 or the widest data cell,
   numeric columns right-aligned). The `compact_tables` option (`--compact-tables`,
   the serve/Python/Node option) renders the unpadded `| - |` form of
   `export_to_markdown(compact_tables=True)` on every path, streaming included.
   The committed **PDF groundtruth is upstream's own** (`tests/data/pdf/groundtruth`,
   which docling's test suite writes with `compact_tables=True` and `do_ocr=False`),
   so `scripts/conformance/pdf_groundtruth.sh` scores the pipeline with
   `--compact-tables --skip-ocr`; every other format's groundtruth is padded.

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
     `DOCLING_RS_SHEET_MAX_CELLS` (10M) is skipped; a docling-JSON, DocTags
     or DocLang table declaring more than `DOCLING_RS_MAX_TABLE_CELLS` (1M)
     cells, or a document's tables more than
     `DOCLING_RS_MAX_TOTAL_TABLE_CELLS` (5M) together, is rejected before any
     grid is materialized (docling-core 2.103.1's caps, #861); HTML past
     `DOCLING_RS_MAX_HTML_DEPTH` is emitted as text without a DOM — and says
     so (#912: `DoclingDocument::notices`, a `partial_success`); a PDF page
     rasterizing past `DOCLING_RS_MAX_RENDER_PIXELS` (15000 px/side) is rejected
     before the multi-GB bitmap is allocated.
   - **OCR** — RapidOCR's models via ONNX: the PP-OCRv6 text detector and,
     when installed (`download_dependencies.sh`), the PP-OCRv6 recognizer —
     docling's own RapidOCR default since 2.127 — else PP-OCRv3 recognition.
     Since #570 the recognizer reads the detector's boxes inside layout
     regions (RapidOCR's crops; projection strips only as the fallback) and
     drops lines under RapidOCR's `text_score`, and since #981 crops each
     detected line as RapidOCR does — the perspective crop of the quad
     (`get_rotate_crop_image`, bicubic, replicated borders), the 90° turn of
     crops ≥ 1.5× taller than wide — instead of its axis-aligned bounding
     box, which on a slightly skewed scan was taller than the line by
     `width · tan θ` and garbled the long small-print lines (a 0.6° page:
     7 of 32 lines destroyed, 0 in RapidOCR with the identical models). The
     remaining differences are RapidOCR's batch width padding and its angle
     classifier, which `ocr_textline_orientation` (#666, off by default,
     every surface) adds: the PP-LCNet text-line orientation classifier
     (`ocr_textline_ori.onnx`, RapidOCR's `Cls` for PP-OCRv5: 80 × 160 BGR,
     batches of 6 by aspect ratio, `180` above 0.9). The reported boxes stay
     axis-aligned. Python maps docling's `RapidOcrOptions.use_cls` onto it
     when set. **Tesseract**
     (#460, `--ocr-engine tesseract` / `ocr_engine` on every surface) is the
     alternative — docling's `TesseractCliOcrOptions` port: the system binary,
     one subprocess per layout-region crop with `tsv` output, its own
     line/word segmentation inside the crop, orientation from its OSD,
     `ocr_lang` as tessdata stems or BCP-47 tags (`deu+fra`, `zh-Hant`), a
     docling-shaped `TesseractCliOcrOptions` mapped by the Python bindings
     (`lang`, `tesseract_cmd`, `path`, `psm`). Recognition runs
     on the lines inside layout regions; RapidOCR's PP-OCRv6 DB **text
     detector** (#429, the model docling's RapidOCR default runs) then sweeps
     the whole bitmap, and detected lines no recognized cell covers are
     recognized and placed as orphan text — docling's behavior for text its
     layout model gives no cluster (diagram labels, stamps, margin notes).
     Lines inside a kept picture/table stay silent children, as upstream. The
     detector input is capped at RapidOCR's 2000 px on the longer side
     since #570 (`DOCLING_RS_OCR_DET_MAX_SIDE`; 960 — PaddleOCR's default and
     the pre-#570 budget — is ~⅓ of the detection time for −0.02 FUNSD
     recall; `0` = RapidOCR's uncapped rule). That cap is the ceiling: a
     scanned page's detector input is budgeted to its own text size (#945,
     `DOCLING_RS_OCR_DET_BUDGET=auto`) — the 25th-percentile height of its
     ink-projection lines kept at 20 px in the input, so a 2 px/pt render
     of typewritten or form-sized print (24–30 px lines) detects at
     1000–1300 px, down to the 960 floor (byte-identical output on the
     scanned PDF fixtures, 30–45 % less time and arena) while a 72-dpi
     form's 8–10 px print keeps the full cap, the FUNSD case #570 raised it
     for. Standalone images (read at docling's 3 px/pt effective
     resolution) and `--ocr-scale` resamples are not budgeted: their OCR
     output moved at character level under any input change, and their
     baselines are pinned to the cap. `full` restores the cap for every
     page. The two places a page *with* a
     text layer runs the detector pay less too: the layout regions its text layer leaves empty (#931) are
     detected on their own crops at render resolution — a caption-sized
     crop instead of the whole page — and the sideways-outline orientation
     probe (#936) runs at the 960 px cap; and the page workers share one
     detector session (as they share TableFormer), so its ~250 MB arena —
     the largest input of any OCR model, grown once per worker — is held
     once per process. The browser pipeline runs the same detector through onnxruntime-web
     when `ocr_det.onnx` is available.
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
     the only text layer (pdfium's text page is gone). Its cells feed a port of
     docling-parse's line sanitizer (`dp_lines.rs`): 3-pass corner-distance
     contraction with gap-proportional space insertion, `enforce_same_font`,
     ligature recomposition, loose-box geometry. Plus docling's markdown escaping,
     docling-parse's typographic-punctuation table (every curly quote → `'`),
     wrap dehyphenation, paragraph-continuation merging, docling's rule-based
     reading-order predictor (2.127's same-row links included, #424) with
     cluster cells joined in docling-parse index order, and false-picture /
     page-number layout fixes. The parser is now the **sole** text
     source, and page count / geometry / `/Rotate` / link annotations are read
     from the same lopdf document (`pdf_meta.rs`), and an image-only page is
     rasterized in pure Rust byte-for-byte like pdfium (`raster/`: pdfium's
     stretch engine + a libjpeg-exact JPEG decoder), every other page by the
     pure-Rust renderer (`render/`) — pdfium itself is an opt-in cargo
     feature (`pdfium`: `DOCLING_RS_RENDERER=pdfium`, a file lopdf cannot
     read) and the default build links no native PDF library.
     Its per-word
     cells reproduce docling-parse's `word_cells` byte-for-byte (377/377 on
     `2305-pg9`), which is what TableFormer matches against; a char-frequency
     validator (`scripts/test/parser_completeness.py`) confirms nothing is silently
     dropped (Form-XObject text and glyph-name-only fonts were the two classes it
     surfaced and fixed).
   - Output is measured **byte-for-byte against docling** (PDF_CONFORMANCE.md):
     **9 / 21 exact, 511 diff lines** against upstream's groundtruth, the rest close. The remaining gaps are
     model-level (TableFormer structure on complex tables, layout
     classification, title-page reading order) plus docling-parse 7's own
     glyph mappings (`∞` for a list bullet, `tildelow` for `~`). The full per-fixture breakdown and the
     model-level blockers live in `PDF_CONFORMANCE.md`.

6. **Extracted image bytes are real but not byte-identical.** Cropped/embedded
   pixels are correct, but the PNG re-encoding differs from docling's, so the
   base64 in `embedded` mode / JSON `ImageRef`s won't match byte-for-byte.
   With `use_embedded_picture_images` (#1007, docling#5143) a PDF picture
   takes the embedded image it shows — native pixel size, `dpi` 72 — as
   docling's option does, so the pixels themselves now match docling's
   extraction instead of differing by renderer. One deliberate widening:
   docling (both backends) refuses an image when any text or vector content
   overlaps it, drawn before or after; here only content painted *after* it
   counts (an opaque image hides what lies beneath, so a page background or
   frame drawn first no longer forces the crop), plus the conditions docling
   leaves to its renderers — no mask, soft mask, alpha or blend mode, upright
   and unclipped, no `/Rotate` on the page. Text cells still refuse it
   wherever they are.

7. **XML format detection sniffs content.** JATS, USPTO, XBRL and RIS all use `.xml`;
   the converter routes by content markers (`us-patent` → USPTO, `us-gaap`/`dei`
   → XBRL, a `<risdok>` root → RIS, else JATS) rather than the extension alone.
   A file whose extension lies gets a second chance (#556): only when the
   conversion as its extension's format *fails* is the content checked —
   RTF's `{\rtf`, the PDF header, a ZIP package's parts (OOXML main part,
   ODF / EPUB `mimetype`), an OLE file's root streams (Word / Excel /
   PowerPoint / Outlook), HTML markup — and the file converted once more as
   that format, with a `docling: warning:` line naming both and the result's
   `format` the one used. A file that converts as named is never inspected,
   so nothing changes for it; when nothing is recognised or the retry fails
   too, the first error stands. docling decides by extension and MIME guess
   up front and fails such files (RTF / HTML / DOCX saved as `.doc`, the
   common case in legacy archives).
   An input with no usable name is typed by its content *before* converting
   (#672, `InputFormat::detect` — `docling_rs.detect_format` /
   `detectFormat` in the bindings): a file with no or an unknown extension
   (a PPTX renamed to nothing, a PDF saved as `.bin`), stdin, an upload, a
   `data:` URL, an email attachment. Signatures only, no classifier: the
   packages (OOXML, ODF, EPUB, Visio, Apple iWork), the OLE files, PDF,
   RTF, flat Word 2.0, HTML, the raster images and HEIF, DjVu, the audio
   and video containers, WordPerfect, WebVTT, DocTags, SVG, AbiWord, XML
   (the dialect read on the `xml` route), JSON (a Jupyter notebook, else
   the docling-JSON route), MIME messages (MHTML by `multipart/related`,
   else email) and LaTeX; prose, Markdown, CSV and AsciiDoc have no
   signature and read as plain text (#671). A correctly named file is never
   sniffed, so nothing changes for it. MarkItDown runs magika on every
   input; docling decides by extension and MIME guess.

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
    `/v1/convert` (sync, async, batch) renders a PDF's pages to PNG
    without running any conversion — the per-page base64 JSON covers
    the PDF-to-image use case Python docling-serve served; `pages=A-B`
    windows and `scale` (pixels per PDF point, default 2.0 = 144 dpi) apply,
    capped at `DOCLING_RS_MAX_RASTER_PAGES` (100) pages per request. The CLI
    counterpart is `--to images` + `--scale`, writing `<stem>_page_NNNN.png`
    files (no cap — the pages land on the caller's own disk).

11. **`do_ocr` and `do_table_structure` are independent** (#244), as in
    docling: `no_ocr` (`--no-ocr`, serve/Node `no_ocr`/`noOcr`, Python
    `do_ocr=False`; `skip_ocr` before 2.0, still read) keeps layout detection
    and TableFormer but never loads or runs OCR — pixel-only text comes back
    empty instead of erroring. The Python binding's `do_ocr=False` previously
    skipped the whole ML stack (layout and tables included), which docling's
    `do_ocr` never meant; that fast path is the docling.rs-only
    `text_layer_only` (`--text-layer-only` — before 2.0, #611, the CLI's
    `--no-ocr`). A missing OCR model also degrades to the `no_ocr` behavior
    with a one-time warning — docling errors there — matching the repo-wide
    degradation-over-failure convention.

12. **Sparse spreadsheets can skip empty cells** (#271, docling.rs-only
    options, both off by default): `skip_empty_cells` omits empty positions
    from each XLSX/XLS table row instead of materialising every region's
    full bounding box (the JSON keeps the surviving cells at their true
    offsets and spans through first-class cells, so `table_cells` is the
    dense list minus the empties — padding the compacted rows back at export
    used to make it larger than the dense one) (docling pads the box too; the related upstream #3328
    tracks the RAM cost of its one-TableCell-per-cell materialisation on
    large sheets), and `compact_tables` renders Markdown tables unpadded for
    every format. Default output stays byte-for-byte docling.

13. **`OcrMode` and `OcrOptions.scale`** (docling 2.116/2.117, #254) are
    mirrored on every surface as `ocr_mode` (`--ocr-mode`,
    `DOCLING_RS_OCR_MODE`) and `ocr_scale` (`--ocr-scale`,
    `DOCLING_RS_OCR_SCALE`). `pdf_aware_layout_regions` — upstream's new
    default, OCR gated by layout regions and the PDF text layer — is the
    architecture this port always had, so the default behavior is already
    aligned — with one gap closed by #931: a page with a text layer OCR'd
    only its bare pictures, so a paragraph drawn as vector outlines
    (`gs -dNoOutputFonts`, flattened print PDFs) beside one native `Page 1`
    footer dropped empty. Now every text, furniture or table region no
    programmatic text touches is OCR'd, as docling's
    `_find_pdf_aware_layout_ocr_rects` does for "clusters without any
    visible programmatic text" (the page renders on demand for it; the text
    detector runs on those regions' crops, not the page, #945). A page
    whose text layer is that thin (under 2 % of the page and at most 12
    words — a page number, a running header; a figure page's caption is
    more and skips the probe, which costs a full-page text detection) is
    also probed for a sideways raster like a scan (#936) — a landscape table turned
    90° and outlined, beside an upright page number, is un-rotated before
    layout and OCR instead of read as noise. Its second trigger, clusters
    over a bitmap, is ported by #999 (docling#4928) for scans: a scanned
    form with one typed-in value used to keep its table region off the OCR
    (the value is programmatic text), and TableFormer matched the native
    word alone — a seven-row table read `| 420 |`. A text, furniture or
    table region a page image covers at least half of is now OCR'd unless
    the text layer accounts for the image there, by docling's own rule for
    shapes (`_text_accounts_for`: a band at most two text lines thick is
    explained by any text on it, a thicker one by text over a tenth of it).
    The text parser records the image placements as it walks the page, so
    finding them costs nothing. Two deliberate narrowings of docling, which
    OCRs every cluster touching a bitmap: the half-cover rule, and the
    text-layer check, so a searchable scan's invisible layer and a digital
    page's logo beside its heading keep their text layer instead of paying
    a second read. As in docling's `PDF_FIRST` merge, the native cells all
    stay and an OCR cell is kept only where it touches none, and the
    table's tokens are docling's `_words_and_ocr_cells`: the native words
    no surviving OCR line covers, plus those lines. The third trigger,
    clusters whose text layer leaves a non-rule shape unexplained (an
    outlined glyph next to real text, docling#4209), is not ported: it
    needs the page's vector shapes. On the PDF corpus it
    fires on the attention-map figures of 1706.03762 (`Input-Input Layer5`,
    outlined titles). On docling's two Korean `skipped_*` pages, whose
    Adobe-Korea1 CID fonts carry no ToUnicode, it used to OCR a few stray
    glyphs (`[ 4-5] 8`); since #1039 those glyphs read through the predefined
    CJK collections docling-parse maps them with, so the regions hold text
    and are not OCR'd. `no_ocr` / `skip_ocr` keep the text layer
    alone; `full_page` and `layout_regions` both map onto the
    `force_full_page_ocr` machinery (the whole-page vs per-region *detector*
    distinction has no analogue in the det-free PP-OCR recognizer here).
    **`replace_broken_text_layer`** (docling#4654, #1006; off by default —
    `--replace-broken-text-layer`, every surface) ports
    `is_broken_text_layer` check for check and replaces only the page it
    judges broken with full-page OCR, its `parse_score` zeroed. Two
    deliberate differences: the text layer here records no rendering mode,
    so the check reads every cell (docling reads the visible ones) and a
    replaced page keeps none of its invisible cells (docling merges them
    back under the OCR) — an invisible layer over a scan is rarely judged
    broken, and a page that is gets its text from the scan; and with no OCR
    model to read the page the layer is kept rather than discarded
    (degradation over failure).
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
    language is rejected (warn-and-default only for the env var). Under the
    Tesseract engine (#460) the same option is Tesseract's language list:
    stems verbatim (`deu`, `script/Latin`), two-letter BCP-47 tags mapped
    onto ISO 639-2/T stems with docling's deviations (`zh-Hans` → `chi_sim`,
    `sr-Latn` → `srp_latn`, `nb` → `nor`), `en`/`ch` still accepted, the
    installed set checked when the engine loads.

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
    Roman/alpha resolution; docling 2.129's colon/dash/bracket/parenthesized
    separators — `1: Intro`, `(2) Scope`, `A - Annex`, docling#4204 — and bare
    Arabic chapter numbers accepted on a consecutive run from 1, docling#4179) > font style (a hand-rolled port of docling's
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

18. **Image outputs** (#518–#520): docling's `images_scale` and
    `generate_page_images` act on every surface (`--images-scale` /
    `--page-images`, `images_scale` / `page_images` in serve and the FFI,
    `imagesScale` / `pageImages` in Node, the Python kwargs and the
    docling-shaped `PdfPipelineOptions`). Page images land on
    `pages[n].image` in the JSON (docling-core's `PageItem.image`), which
    makes `TableItem.get_image` / `FormulaItem.get_image` work on the loaded
    document. Picture `ImageRef.dpi` is now the crop's real render scale,
    72·scale — 144 for the default 2× crops, where it said 72 (#519); office
    images keep docling's 72, and a docling-JSON input's dpi round-trips.
    Divergences: picture crops are always extracted (docling only with
    `generate_picture_images`), so the Python facade applies `images_scale`
    only once docling would render images and otherwise keeps the 2× crop;
    other scales resample the pipeline's 2.0 px/pt render instead of
    re-rendering the page (CatmullRom ≙ PIL BICUBIC, so above 2.0 it
    upsamples); text-layer-only (`text_layer_only`) pages and streamed Markdown carry
    no page image. Python's `convert(source, page_range=(a, b))` now takes
    docling's per-call window (#518; it was a native-only constructor
    kwarg).

19. **docling-serve's own API** (#615). Besides `/v1/convert`, the server
    answers upstream docling-serve's routes — `POST /v1/convert/file`,
    `/v1/convert/source`, their `/async` variants, `GET
    /v1/status/poll/{task_id}`, `/v1/result/{task_id}`, and the `/v1alpha`
    aliases — with upstream's `ConvertDocumentResponse` /
    `TaskStatusResponse` shapes, so Open WebUI's Docling loader, n8n, Dify
    and LangChain's docling-serve client work unchanged; `--api-key` /
    `DOCLING_SERVE_API_KEY` is upstream's `X-Api-Key`, and
    `DOCLING_SERVE_ENABLE_UI` / `--enable-ui` switch the web UI (#892;
    `DOCLING_RS_ENABLE_UI`, `--no-ui` — on by default here, off upstream,
    so an unset variable keeps the page). Upstream's option
    names map onto ours. Deliberate differences: options with no
    equivalent (`pdf_backend`, `table_mode`, `abort_on_error`, picture
    description, presets) are ignored rather than rejected, so a client's
    `DOCLING_PARAMS` never fails a conversion. OCR engines other than
    Tesseract run the built-in PP-OCR, and `ocr_lang` keeps the first code
    this build has a recognizer for. `image_export_mode=referenced` is
    answered as placeholders, since a response body has nowhere to put
    files. `doctags_content` is `null`, because there is no DocTags writer.
    `timings` is `{}`. A failed document answers a `200` envelope with
    `status: "failure"`, as upstream does for a conversion error.

20. **PDF text-layer typography** (#919, #920, #922, #923). Four fixes to how
    the pure-Rust text layer reads a line: `WinAnsiEncoding` is complete over
    Windows-1252's 0x80–0x9F row (`„` `€` `™` `œ` … — the table lacked them, so
    a German opening quote `„` vanished while its closing `“` stayed); a soft
    hyphen ending a line fuses the wrapped word (`Veröffent­` + `lichung` →
    `Veröffentlichung`; it was stripped before the paragraph merge could see
    it, leaving `Veröffent lichung`); a small raised note call at the right
    end of a line — a number or reference mark (`1`, `*`, `†`) or a short
    bracketed note (`(Anm. 1)`, `[3]`) — reads after that line, not ahead of
    it (any other smaller run, such as a figure label, keeps its place: moving
    those scrambled figure text). **Divergence:** a
    line-final hard hyphen before a coordinating conjunction (`und`, `oder`,
    `bzw.`, `sowie`, `and`, `or`, `et`, `ou`, `en`, `y`) is an elliptical
    compound and keeps its dash and space (`Warte- und Behaltefristen`);
    docling fuses it (`Warteund`). Every other wrapped word still fuses
    (docling#3888).

21. **Word gaps without space glyphs** (#921, `pdf_word_gap`). When a PDF
    marks a word boundary with a cursor move and no space glyph, the text
    layer decides it from the gap's width. The default (`adaptive`) is
    docling-parse ≥ 7's rule (docling-parse#351, 2026-09), what docling
    itself runs on since it pins `docling-parse>=7.22`: lines come from
    geometry, each gap is divided by the mean advance of the two glyphs
    flanking it — measured along the pen, so right-to-left text drawn in
    visual order reads as docling-parse reads it — and a line-local 1-D
    two-cluster threshold (0.18–0.35, 0.25 when the gaps do not separate)
    decides. A number `F` is a fixed threshold for that ratio on lines
    without space glyphs (0.1–0.23 suits body text); a line that draws its
    spaces keeps docling-parse 7's floor of 0.35, so a loosely kerned `f|l`
    (0.24 in EUR-Lex notices) is no boundary (and on a page that draws its
    spaces, every line keeps it — below). `legacy` keeps docling-parse ≤ 6's rule, this port's
    default until #921: a gap splits when it exceeds 0.33 × the running
    mean advance of the cell built so far in that contraction pass, and
    cells restart at every font change (`enforce_same_font`), so the
    threshold drifts along a line (#921's `und dem`, in a second font
    inside body text: gap 1.9550 pt, threshold 0.33 × 5.925 = 1.9553 pt →
    `unddem`). With the switch came docling-parse#349's quote folding —
    `“ ” „ ‟` → `"`, single quotes still `'` — which applies whatever the
    word-gap mode. Against docling 2.144 (docling-parse 7.24.1) on the
    PDF groundtruth the two took the corpus from 6 / 18 strict and 815
    diff lines to 7 / 18 and 389 (`PDF_CONFORMANCE.md`). Its word test is
    ported in full: a baseline shift is read on the pens' origins (rise
    included) against the font size, not on the loose boxes, and a glyph in
    a TeX/math face (`cmr`, `cmmi`, `msbm`, `…math…`, `symbol`) next to its
    base is a local attachment — `Lysak[0000-…]`, `LATEX`, `1025×1025`. **Divergence:
    letter spacing.** A letter-spaced (Sperrsatz) word is one constant
    cursor move between its letters, but divided by the advances of `i`,
    `l`, `t` beside `M` or `D` its gaps spread over 0.25–0.75 and straddle
    every threshold, so docling-parse 7.24.1 cuts it mid-word — `Di ens tk
    lasse I`, `Heimarbei tstari f`, `De r Bu n d e s k a n z l e r`, `N O T I
    Z E N` (#921's five fixtures: 105 of 156 letter gaps cut). Here a chain
    of three or more open letter gaps equal in points (within max(0.25 pt,
    8 %)) whose flanks hold nothing tighter is tracking and stays inside
    the word; the run's other gaps of the same width join it (`für`, `II`,
    a closing `:`), and four or more dots in a row are one leader. The
    flank test keeps justified prose apart — its equal word gaps chain up
    across one-letter words (`Peter W J Staar`, `Abs 1 Z 3 UrlG`) but sit
    between abutting letters — and one character repeated (`C C C C`, an
    OTSL figure's token row) is no word. On the five fixtures every
    letter-spaced word reads whole and all 45 cursor-move word gaps still
    split, under `adaptive` and under every `F` from 0.1 to 0.23 (the
    issue's reporter confirmed it on their side, and on 127 statute and
    collective-agreement excerpts `adaptive` introduced no new glue and no
    mid-word cut); on the
    81 PDFs of the test corpus the detection changes nothing else. On
    #921's 92-page document, leader dots aside, words match docling-parse
    7.24.1's 98.9 % under `adaptive` (98.1 % under `legacy`) and lines
    96.1 % (76.7 %) — the rest character normalization and the
    letter-spaced words above. **Divergence: one wide gap no longer sets
    the line's threshold.** docling-parse 7 feeds every gap's width into
    its k-means, so a single wide one — after a comma, before a tab stop or
    a column (0.8–5 advances) — is the high cluster on its own, the
    midpoint caps at 0.35 and the line's real word gaps (0.31–0.33 in
    tight justified text) fall under it: `jedochwerdenweiterlaufende`,
    `DieNormalarbeitszeit`, `KhurramAzeemHashmi`. Gaps enter the clustering
    at most 0.5 wide here (wider is a boundary under any threshold the rule
    can pick); on #921's 92-page collective agreement that takes the
    adaptive rule's glued word pairs from 68 to 7 (`legacy` has 52;
    measured against `pdftotext`'s words), at a cost of 18 diff lines
    against docling's groundtruth, where docling glues the same pairs (389
    → 407, all in `2203.01017v2` and `2206.01062`). **Divergence: a page
    that draws its spaces.** docling-parse 7's 0.35 floor applies to a
    line holding a space glyph; a one-word line — a table cell, a heading —
    has none, so a kerned `f|l` (0.24–0.33 advances in EUR-Lex documents)
    became `Offenlegungspf lichten`, in docling-parse 7.24.1 too. Producers
    are consistent about spaces, so when at least half of a page's lines of
    eight glyphs or more carry one, every line on it keeps the floor (no
    groundtruth file changes). Not a line whose spaces the page draws
    apart from its letters: LibreOffice sets an Arabic line's spaces first
    and then its letters in visual order, so the spaces fall to the run
    before and the line has none of its own. A space glyph sitting in one
    of the line's gaps tells it from a one-word line, and its gap is
    inferred as docling-parse infers it — under the floor its word gaps
    of a third of an advance stayed glued (`علىشراء`). On such a page a
    fixed `F` reads like
    `adaptive` (0.35 on every line, so the 63 extra EUR-Lex cuts #921's
    reporter saw at 0.10 are gone); `F` applies as given on pages that
    draw no spaces. **Extension: `pdf_keep_typography`.** docling-parse's
    sanitizer folds typographic punctuation to ASCII — `„ “ ” ‟` → `"`,
    `‚ ‘ ’ ‛` → `,`/`'`, the dashes → `-`, `…` → `...` — and so does this
    port by default. `pdf_keep_typography=true`
    (`DOCLING_RS_PDF_KEEP_TYPOGRAPHY`, `--pdf-keep-typography`) keeps them
    as drawn, so a German citation's `„Gewinn“` stays distinguishable from
    a straight quote in code or data. Line order is untouched by it.

22. **Superscripts and subscripts read as the page shows them** (#992).
    Two rules, both divergences from docling-parse 7.24.2 on purpose. (a)
    A space glyph whose advance the content stream takes back — a negative
    `TJ` offset after it that puts the pen at or before the space's start,
    so the next glyph is drawn over it — is dropped from the text layer
    (`textparse::drop_overprinted_spaces`): the page shows no gap there.
    Arbortext sets EUR-Lex's footnote markers as `( 1 )` with exactly such
    cancelled spaces around the raised digit. The rule is narrow: the next
    glyph must sit on the same line and start within the first 30 % of
    the space's advance; a kerned pair (`−100/1000 em` into a space still
    leaves two thirds of it), a justified word gap pulled in a little, and
    a trailing space before the cursor jumps to the next line's margin all
    keep their space, and a rotated run (a landscape slide drawn through a
    90° `cm`, whose glyph boxes stack on one edge) is left alone. (b) A word that starts only because its first glyph
    sits on another baseline than the glyph before it — docling-parse's
    `word_baselines_compatible`, a shift beyond 0.35 × the smaller height
    outside a math face — with no horizontal gap between the two, and
    *inside a bracket* (after an opening `(` `[` `{`, or itself starting
    with a closing one), joins the *line* text without a space
    (`dp_lines`, the `glued` word): the raised `1` is still its own word
    cell, as in docling-parse, but the line reads `(1)` where
    docling-parse's line cell reads `( 1 )` — and `( 1 )` even for a raised
    digit drawn with no spaces at all, so (a) alone does not reach the
    page's reading. Outside brackets docling's space stays: `SynthTabNet
    1 .`, a footnote line's `1 https://…`, `상해 1) 나`, the `1 /4`
    fraction and the ORCID's `Lysak[…] ,` read as docling reads them
    (joining those cost five groundtruth files when the rule had no
    bracket test). The page shows `(1)`, and pdftotext and pdfium, which
    break words by geometry alone, read `(1)`. On the corpus: the PDF
    groundtruth is unchanged (7 / 18, 407), and one snapshot moves — a
    LaTeX figure's math, `𝐤𝑡𝑅 ]}` → `𝐤𝑡𝑅]}`, the closing bracket after a
    raised index.

23. **Unmapped glyphs read `�`** (#1003, docling#5070 / docling-parse
    7.24). A painted glyph no source maps to Unicode used to vanish from the
    text layer, so a lost `⁻¹` read `mL1` and nothing downstream could tell.
    It now reads U+FFFD, as docling-parse 7.24 emits it: a bare glyph-index
    `/Differences` name (`g115`, `gid00043`, `cid42`, `glyph7`, `index9`), a
    code a composite font's ToUnicode lacks, a code no table covers, and a
    NUL a ToUnicode maps a code to (docling's own `\x00` → `\ufffd`). A
    glyph whose embedded program paints nothing is a space instead — its box
    has no height and it is a word boundary (redp5110's `/g3`). An unknown
    code-like name keeps its text without the slash (`SM590000`, as
    docling-parse reads it; we used to print `/SM590000`). A CJK composite
    font without ToUnicode reads through Adobe's CID tables first (#1039,
    the Korean `skipped_*` fixtures); only a CID no table maps is `�`. One
    deliberate difference: a composite font with neither a ToUnicode nor a
    collection table reads `�` where docling-parse takes the CID for a code
    point (redp5110's bullet, CID 114, reads `r` there). Without the
    renderer (the wasm text-layer build) there is no font program to ask,
    and every unmapped glyph reads `�`. A `bfrange` whose destination is a
    surrogate pair (`<1d59> <1d5a> <d835dec2>`) steps its last code unit and
    maps `𝛂 𝛃` (#1039's `tounicode.rs`); the range used to be dropped. On the corpus: redp5110 155 → 119
    groundtruth diff lines (373 → 337 in all), four snapshots move.

24. **Superscripts and subscripts, opt-in** (#1005, docling#4815's
    `ScriptPositionModel`; `script_positions` / `--script-positions` on
    every surface, docling's `pipeline_options.script_position_options.
    enabled` read by the Python facade). The port of
    `utils/script_position.py` is rule for rule — line splitting, baseline
    groups, the body baseline, the touch / in-word / footnote-mark tests,
    difflib's matching blocks over the NFKC- and punctuation-folded text —
    and its unit tests are ported with it (`script_position.rs`), the
    end-to-end ones on docling's fixture and its rotated, cropped and
    invisible-text pages (`tests/suite/script_position.rs`). The characters
    come from our text layer instead of pypdfium2: the loose box, the pen's
    baseline and size, `Tr` 3 / 7 left out. A marked paragraph or footnote
    is a `Node::ScriptText`; the JSON writes docling's shape (the item with
    an empty text, an inline group of the runs added after every other
    item, each run with `formatting.script` and its characters' box), so
    the HTML reads `H<sub>2</sub>O` and Markdown `H2O`. Three differences:
    list items are not rewritten (ours carry their own location and would
    leave their list under a wrapper); an item's box is the 0–511 grid
    location, so the character test's slack is one grid step wider than
    docling's 0.5 pt; and the stage runs on the PDF pipeline only, not on
    `text_layer_only` conversions. Off by default, so nothing changes
    unless it is asked for.

25. **A text shadow reads once** (a divergence from docling-parse 7.24.2 on
    purpose). A word processor exports a heading with a shadow effect as the
    same run drawn twice — the shadow, then the text over it, shifted by a
    fraction of a point (0.5–0.8 pt up and left, black under the coloured
    text, in the equipment manuals that showed it). docling-parse keeps both
    copies, so docling with its default backend reads `## Safety
    instructions Safety instructions`; with its pypdfium2 backend it reads
    `## Safety instructions`, because pdfium's text page drops the second
    copy, and that is what the page shows. `textparse::drop_shadow_runs`
    drops a stretch of at least two glyphs that repeats a stretch drawn
    before it on the page — same characters, font and size — shifted by one
    offset, of at most 0.15 em on each axis, not zero, and shorter than the
    step from the first stretch's glyph to its successor. The rule is
    narrow: text that really repeats sits an advance apart or more (a
    doubled letter, a word set twice, the same text on the next line or in
    the next table cell); a run of one character set tighter than the
    offset (dot leaders squeezed by `Tc`) steps less than it shifts; the
    characters one glyph expands to share its pen and are not a copy; and
    the per-glyph re-stamp the sanitizer already folds is untouched. On the
    corpus: the text layer of every PDF is byte-identical
    (`--text-layer-only --to json`), so the PDF groundtruth and the
    snapshots are unchanged too.

26. **An image input's page is sized from its resolution, its models read
    its pixels** (#1093). docling sizes an image's page `pixels × 72 / dpi`
    points and feeds its models the image resampled to that page; the page
    size and the JSON boxes follow docling here, the models keep reading the
    file's own pixels. A 300-DPI scan resampled to 1 px/pt loses most of its
    detail — docling's layout model reads the 300-DPI QR bill as two
    pictures — so the text, the reading order and the Markdown are those of
    the full-resolution image (the §2 Images entry has the rule).
---

## 5. Not migrated / out of scope

Nothing here blocks day-to-day conversion: every remaining item is either a
deliberate scope boundary or a cosmetic, single-fixture polish gap.

**Out of scope by design:**

- **Local VLM full-page inference** (SmolDocling-class models in-process).
  Model-bound; out of scope for the discriminative port. The **remote** VLM
  pipeline (#77) *is* implemented: `--pipeline vlm --vlm-endpoint URL
  --vlm-model NAME` renders pages (pure Rust), converts them through any
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
  mostly render-scale-induced — see PDF_CONFORMANCE.md). Per-page outcomes
  follow docling 2.139's `VlmPipeline._determine_status` (docling#4561,
  docling#4778; #762, #763): a failed request (`VLM inference failed: …`)
  or a cut-off answer (`VLM output incomplete (stop_reason=length|content_filter)`,
  text kept) is a `pipeline`/`VlmPipeline` error item with its `page_no`
  and a `partial_success`; only a document with no page output fails.
  `ErrorItem.page_no` is exposed on every surface (serve JSON `page_no`,
  Node `pageNo`, Python `ErrorItem.page_no`). (**Audio/ASR is now done** — see §2; Opus and AVI,
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
  `doFormulaEnrichment`, also on `new Pipeline()` — which since #471 reads
  every PDF/image option of `ConverterOptions`, `ocrEngine` / `ocrLang` /
  `ocrMode` / `ocrScale` / `skipOcr` / `forceFullPageOcr` / `noTextPanels` /
  `headingHierarchy` / `pages`, validated like `DocumentConverter`);
  conformance-checked by `scripts/conformance/enrich_conformance.sh`. Since
  #517 the classifier's session stops at ONNX Runtime's `EXTENDED`
  optimization level on runtimes before 1.29 — 1.26–1.28's x86 NCHWc rewrite
  returned one distribution for every picture; the linked builds carry 1.31
  since #523 and run uncapped, the cap only applies to an older
  `libonnxruntime` under `ort-load-dynamic`.)

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
  tables, **and** the legacy **APS plain text** (`pftaps`): docling's format
  detection routes a `text/plain` file opening with a `PATN` record to its
  `PatentUsptoGrantAps` parser (title, abstract, caption headings, claims),
  and docling.rs ports it — Markdown, the JSON item tree and the `.dclx` are
  exact ([issue #44](https://github.com/docling-project/docling.rs/issues/44),
  done; the earlier text-dump `.dclx` groundtruth came from routing the file
  through the Markdown backend).

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

- **Layout-free OCR recognition** (#939) — `docling_pdf::OcrModel` (with
  `RecognizedLine`) is public: `recognize_det_boxes(&img, &boxes)` reads the
  lines `ocr_det::DetModel::detect` found, `recognize_crops(&crops)` reads
  ready-made line crops, both without the layout model or page assembly
  that `Pipeline::process_pages` runs per call. Each returns one
  `{ text, confidence }` per input in input order — the confidence is the
  recognizer's mean emitted-character probability (what the page OCR
  reports), never the detector score — and drops nothing:
  `passes_text_score()` is the page OCR's RapidOCR `text_score` filter.
  A detector box is cropped exactly as the page OCR crops it
  (`ocr_prep::det_box_crop`: the quad's warp, #981, plus under
  `set_textline_orientation(true)` the 180° classifier), and the width
  batching and lanes are the page OCR's, so the same pixels read the same.
  Python docling exposes no such entry point.
- **`extract_diagrams=mermaid`** (#858) — a DOCX DrawingML flowchart as a
  `mermaid` code item after its picture: the labelled shapes are the nodes,
  the connectors that name their two end shapes (`a:stCxn` / `a:endCxn`)
  the edges, the arrow ends the direction. On by default, `none` turns it
  off on every surface (`--extract-diagrams mermaid|none`, serve
  `extract_diagrams`, Python `extract_diagrams=`, Node `extractDiagrams`);
  a drawing laid by eye gets no graph, never an invented one. In
  Markdown's placeholder mode the block takes the drawing's
  `<!-- image -->`'s place (the picture itself stays in the document and
  in the embedded / referenced modes). docling has no counterpart.
- **`pdf_repairs`** (#934) — what the PDF reader repaired to read a file,
  typed: `docling_pdf::pdf_repairs(bytes) -> Vec<PdfRepair>` (re-exported by
  `docling`), with `PdfRepair::StreamLength { object, offset, declared,
  recovered }` for every stream whose declared `/Length` does not end at its
  `endstream` and `PdfRepair::XrefEntriesPadded` for the 19-byte-xref
  fallback. The reader repairs silently (lopdf 0.45 recovers such a stream
  from its unambiguous `endstream … endobj` boundary and overwrites
  `/Length` with the recovered size, so the loaded object cannot show it);
  this re-reads the original bytes with lopdf's stream grammar, on demand,
  and an admission caller can refuse repaired input before converting. The
  option `pdf_report_repairs` (off by default; `--pdf-report-repairs`,
  serve `pdf_report_repairs` → `X-Docling-Pdf-Repairs` / a batch item's
  `pdf_repairs`, Python `pdf_report_repairs=True` → `result.pdf_repairs`,
  Node `pdfReportRepairs` → `pdfRepairs`, `DOCLING_RS_PDF_REPORT_REPAIRS`)
  runs the same check during a PDF conversion and puts the list on the
  result (`ConversionResult::pdf_repairs`); the document is the same either
  way. No false positive on the 98 PDFs of the
  test corpus; on each repaired stream the recovered length equals what
  lopdf loads. `docling_pdf::try_pdf_repairs(bytes, max_decompressed_size)`
  (#938) is the bounded, fallible form for an admission guard that caps what
  one stream may inflate to: the load stays lenient (strict parsing would
  reject the very repaired input this reports), an object stream the bound
  makes lopdf skip is `RepairInspectError::ObjectStreamSkipped` rather than a
  silent gap, and an unloadable file is `Unreadable` — `Ok` is a complete
  list, empty for a well-formed file. docling has no counterpart.
- **`RenderedPage::omissions`** (#916) — `render_pages` reports, per page, the
  images the pure-Rust renderer drew as a placeholder block or left out
  (`docling_pdf::Omission { object, rect, reason }`: `JPEG not decoded
  (Corrupt("no SOI"))`, `no JPXDecode decoder`, …), where it used to name them
  only on stderr under `DOCLING_RS_DEBUG`; a page the
  `DOCLING_RS_MAX_RENDER_PIXELS` cap refuses now fails with the cap's own
  message instead of "no page renderer". docling has no counterpart.
- **`docling-pdf` `render` feature** (#917) — `render_pages`, `RenderedPage`,
  `PdfDocument` and the `render` / `raster` modules (the pure-Rust page
  renderer, no model) without `ort` / `docling-onnx` in the dependency graph:
  `docling-pdf = { default-features = false, features = ["render"] }`. `ml`
  (the default) implies it. docling has no counterpart.
- **`docling-rag`** — documents → chunking → embeddings → vector search,
  with swappable embedders (Ollama/Gemini/local ONNX), stores
  (SQLite+sqlite-vec / PostgreSQL+pgvector), LLM, sources and queues, plus an
  eval harness and a REST API. See the crate README.
- **`docling-node`** — Node.js/Bun N-API bindings (npm package): the full
  converter surface plus the chunkers, Markdown/chunk streaming, a warm
  `Pipeline` for many PDFs, and the remote VLM pipeline (`pipeline: 'vlm'`).
- **`docling-wasm`** — WebAssembly bindings: the declarative converters (and
  digital PDFs via the opt-in `pdf-text` text-layer feature — the same
  extraction as `--text-layer-only`, no ONNX) run fully client-side in the
  browser, ~3.4 MB gzipped (the browser ML pipeline's pre/post-processing included); scanned PDFs return a "needs OCR" error. Python
  docling has no equivalent. See the crate README.
- **`docling-py`** — PyO3 bindings (PyPI package `docling-rs`): a strangler-fig
  drop-in for docling's Python API where the Rust engine is the document
  processor and `result.document` is a genuine `docling_core` `DoclingDocument`,
  so its `export_to_markdown()` / `export_to_dict()` / chunkers are docling's
  own code.
  `docling_rs.langchain` (extra `[langchain]`) ports docling's
  [langchain-docling](https://github.com/docling-project/docling-langchain):
  `DoclingLoader` (`ExportType.DOC_CHUNKS` / `MARKDOWN`, pluggable chunker and
  meta extractor) reproduces that package's own test fixtures exactly, and
  `PictureDescriptionLangChainOptions` describes pictures with any LangChain
  chat model (run on the converted document, every input format). URL sources
  (`convert("https://…")`) download first, as in docling.

- **Picture OCR for non-PDF documents** (#645) — docling only OCRs through
  its PDF/image pipeline, so the text inside a DOCX/PPTX screenshot, an HTML
  figure or a video frame is never read. `do_picture_ocr` (every surface:
  `--picture-ocr`, serve `do_picture_ocr`, Python `do_picture_ocr` /
  `PdfPipelineOptions.do_picture_ocr`, Node `doPictureOcr`) runs the same
  OCR models — the PP-OCR recognizer with the `ocr_det.onnx` line crops at
  docling's image resolution, or Tesseract under `ocr_engine` — over every
  embedded picture of a non-PDF document after the backend, and attaches the
  lines (reading order; `DOCLING_RS_OCR_TEXT_SCORE` applies) as docling's
  picture description — `meta.description` (`created_by` = the engine) plus
  the `PictureDescriptionData` annotation, the shape `PictureDescriptionBaseModel`
  writes, so a docling-core consumer reads it like a VLM caption. Markdown
  prints it between the caption and the placeholder (docling's
  `MarkdownPictureSerializer` order: captions, annotations, image), the chunkers
  put it in the picture's chunk, DocLang / LaTeX / Pandoc are unchanged
  (docling's DocTags carry no description either). Filters:
  `picture_ocr_classes` (the DocumentFigureClassifier's top label must be one
  of the listed classes; a label it never predicts is rejected) and
  `picture_ocr_min_side` (32 px, `DOCLING_RS_PICTURE_OCR_MIN_SIDE`);
  `keep_picture_images=false` drops the image bytes after the pass. Off by
  default — the default exports of every fixture are byte-identical — and a
  picture without text, `no_ocr` / `text_layer_only`, or a missing model
  attaches nothing (one warning). PDF/image/METS inputs are untouched: their
  pages went through the OCR pipeline already. The dedupe is by image bytes,
  so a DOCX (walked into both the flat nodes and the item tree) or a slide
  master's logo is read once. `picture_ocr.docx` (a 720 × 180 two-line
  screenshot and a 16 × 16 icon) is the fixture; the e2e test also reads
  the text drawn into a generated video clip's frame. Pictures read at
  docling's image resolution unless `picture_ocr_scale` (#666; every
  surface, `DOCLING_RS_PICTURE_OCR_SCALE`) sets their own: px per picture
  pixel under RapidOCR's 2000 px cap — `1.0` is RapidOCR's native-pixel
  reading (RapidOCR being the reference here, since docling never OCRs
  these pictures), where the default reads a 720 × 180 screenshot at
  2000 × 500. `ocr_textline_orientation` reads their vertical, skewed and
  upside-down lines (`tests/fixtures/textline/`).
- **Picture description through a vision endpoint (#670).** docling's
  `do_picture_description` with `PictureDescriptionApiOptions` is an engine
  pass here, on every format and surface (`--picture-description`, serve
  `do_picture_description` + docling-serve's `picture_description_api`,
  Python `PictureDescriptionApiOptions` or the flat kwargs, Node
  `doPictureDescription`): every picture — embedded office/HTML pictures,
  PDF figure crops, video frames, an image input as a whole — goes to an
  OpenAI-compatible `chat/completions` endpoint and the answer is its
  description annotation, where `do_picture_ocr` puts its text. Two prompts,
  MarkItDown's: `caption` ("Write a detailed caption for this image.") and
  `ocr` (markitdown-ocr's text extraction). The picture-OCR filters apply,
  identical bytes are sent once, `concurrency` requests run at once, and
  endpoint errors warn without failing the conversion. docling's local
  describers (`PictureDescriptionVlmOptions`, SmolVLM in process) have no
  counterpart; a LangChain chat model still runs as the Python post-pass.
- **Text layer first, OCR when there is none** — `text_layer_only=auto`
  (`--text-layer-only=auto`; `"auto"` on every JSON surface, in Python and
  Node; `TextLayerOnly::Auto` in Rust). docling runs its layout model on
  every PDF page and OCRs the pages without a text layer; its only shortcut
  is the backend's own text extraction with no pipeline at all. docling.rs
  had the same two ends — the pipeline, and `text_layer_only=true` with
  nothing for a scan — and now the middle: the embedded text layer is
  parsed first (pure Rust, no render), and when it carries the document
  (`docling_pdf::text_layer_has_text`: more than a line a page or 32
  characters in all — the same rule that calls a layer of a few typed-in
  form fields over scans vestigial) the conversion is `text_layer_only=true`'s
  byte for byte, no model loaded; otherwise the full pipeline runs and OCR
  reads the pages. The verdict is the whole document's, decided before any
  model is touched, so a digital file with a few scanned pages inserted
  keeps the text path (those pages come back empty, as under `true`). The
  switch is per conversion on the full model set: a warm pipeline (serve,
  the Python / Node `Pipeline`, the CLI batch) serving digital and scanned
  documents in turn reloads nothing. `no_ocr` wins when both are set;
  `force_full_page_ocr` is read only on the pipeline side of the verdict,
  as under `true`. On an SVG, `auto` is the native `<text>` extraction when
  the file has text and the ML image route when it has none; a DjVu already
  falls back to OCR by itself. Streaming (`convert_streaming`) takes the
  same verdict.
- **Video frame sampling tunables** (#647) — docling's video path is the
  transcript; the frames are a docling.rs extension (#138), and their
  sampling now takes options on every surface: `video_frames=all` (every
  distinct cut — no cap, a cut-less video yields its first frame alone),
  `video_scene_threshold` (ffmpeg's `scene` score a frame must exceed, 0.27;
  0.6 keeps hard cuts only), `video_frame_max_side` (a `scale` filter in
  ffmpeg's chain capping the longer side, 0 = source size) and
  `video_frame_dedupe` (a 64-bit difference hash over a 9×8 grey thumbnail;
  a frame within the Hamming distance of a kept one is dropped — a re-lit
  or re-shown slide becomes one picture; unset = keep all). Env defaults:
  `DOCLING_RS_VIDEO_SCENE_THRESHOLD`, `DOCLING_RS_VIDEO_FRAME_MAX_SIDE`,
  `DOCLING_RS_VIDEO_FRAME_DEDUPE`. Memory is bounded by construction: each
  chosen frame is decoded with one seek, handed to the converter's hook
  (picture OCR when `do_picture_ocr`, bytes dropped when
  `keep_picture_images=false`) and its file deleted before the next decode,
  so one frame is in flight whatever the count. With nothing set the
  sampling is the pre-#647 one and every default output is unchanged.
  #853: the scene scan maps the video stream alone (`-an -sn -dn`) — the
  null muxer's default `pcm_s16le` encoder had every scan decode and
  re-encode the soundtrack, and a decode-only ffmpeg build without it
  failed the scan, so a video with audio lost its keyframes; the README
  lists the ffmpeg components the frame sampling needs.
- **Document metadata** (#679) — docling keeps a document's `origin` (file
  name, hash) and nothing of what the file says about itself; MarkItDown
  prints an EPUB's Dublin Core, a photo's EXIF (through exiftool) and an
  audio file's tags. `metadata=json|frontmatter` (every surface) reads the
  file's own metadata in pure Rust — an EPUB's OPF, an Office package's
  `docProps/core.xml` + `app.xml`, an ODF `meta.xml`, EXIF (JPEG, PNG,
  TIFF, WebP, HEIF; the GPS position decoded), audio/video container tags,
  a PDF's Info dictionary — into `document.metadata`, written as a
  `metadata` object after `origin` in the JSON (a foreign key docling's
  pydantic model ignores on load) and, under `frontmatter`, as YAML front
  matter at the top of the Markdown. Off by default, so every export is
  unchanged; `redact_pii` masks the author fields and drops the position.
  The MarkItDown facade prints MarkItDown's own metadata lines from it.
- **PII redaction** (#621) — Python docling has no redaction stage; this is
  a docling.rs extension, off by default and inert unless asked for (the
  regression baselines are untouched). `DocumentConverter::redact_pii` /
  `--redact-pii` / serve `redact_pii` / Python `redact_pii` / Node
  `redactPii` run one pass over the `DoclingDocument` after the backend and
  before any serializer, chunker or stream — not over an export — so the
  item tree behind the JSON, the link table, code `orig`, key-value cells,
  captions and hrefs, comments and VTT voices are rewritten along with the
  body text, and the PDF streaming Markdown matches the buffered one byte
  for byte (one `Redactor` across the page batches). Detection:
  `docling_core::redact::PatternDetector` (pure Rust, wasm) — e-mail, phone
  (dates / versions excluded), Luhn cards, mod-97 IBANs with the registry
  lengths, IPv4/IPv6, URL credentials, US SSN / UK NINO / Aadhaar
  (Verhoeff), custom `NAME=REGEX` patterns, deny / allow terms — plus, under
  the `ner` feature with `dslim/bert-base-NER`'s ONNX export in
  `.models/ner/` (MIT; `download_dependencies.sh --with-ner`, Python
  `download_models(ner=True)`), person / organization / location spans from BIO
  wordpieces (`Address` is not a class that model has); a missing model is
  one warning. Replacement: `[EMAIL]` labels, `[EMAIL_1]` pseudonyms (one
  number per distinct value, case- and separator-folded, consistent within
  the document) or a fixed string; overlapping spans resolve longest-first,
  then card / IBAN / national ID over phone. Images: `drop` (default —
  pictures and page renders gone), `box_out` (the pipeline's OCR reads each
  image as a scale-1.0 page and paints over the lines with a hit; without
  the OCR models, a `no_ocr` converter or on the streaming path it drops),
  `keep`. The report is counts per label (`RedactionReport`; serve's
  `X-Docling-Redaction` / item `redaction`), the mapping only on explicit
  request in the library. Non-goals as in the ticket: no compliance claim,
  no cross-node entities, no cross-document pseudonyms, the source file
  untouched. Fixtures `pii_sample.*` (DOCX with a comment and a table, HTML
  with `mailto:` and a figure, `.eml`, `.vtt`, a text-layer PDF, a scanned
  PDF, a DOCX with the scan embedded) seed the same values; the e2e checks
  every export of each.

## 7. Testing

- **`cargo test`** — unit tests per backend/serializer **plus an output-
  regression suite** (`crates/docling/tests/regression.rs`): every
  declarative source under `crates/docling/tests/data/<fmt>/sources/` is
  converted to legacy Markdown, strict Markdown and docling JSON and compared to
  committed fixtures (159 sources × 3). `DOCLING_RS_REGEN=1` refreshes them.
  The JSON fixtures double as a docling-core load check.
- **Snapshot harness** — `scripts/conformance/pdf_conformance.sh` regenerates and diffs the
  PDF/image/METS baseline (needs the ONNX models + the docling-parse plugin; **94 outputs, all
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
  performance backlog live in [`PERFORMANCE.md`](./PERFORMANCE.md).

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
doesn't loop) — and builds nothing, and cuts no GitHub Release: the merge-time
job is a `cargo update --workspace` and a push, with no target cache; the
Release is cut by `release.yml` for the tags the maintainer publishes. The
crates reach crates.io from `crates-publish.yml`, one of the channels
`gh workflow run release.yml` dispatches for the release the maintainer picks
(`-f crates=false` leaves it out; `gh workflow run crates-publish.yml -f
tag=vX.Y.Z` runs it alone): a checkout of the tag, the `cargo publish` verify
builds under the `cargo-v2-publish` cache, and `scripts/ci/ci_publish.sh` in
dependency order — skipping any version already on crates.io, so a re-run
after a failed upload publishes only what is missing. The uploads
authenticate through crates.io Trusted Publishing (OIDC: the job's
`id-token: write` identity — `docling-project/docling.rs` +
`crates-publish.yml` — is what each crate's crates.io settings name as its
trusted publisher; a fresh 30-minute token is minted per crate by
`scripts/ci/crates_io_token.sh`), so there is no registry token secret to
rotate.

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
4. **New formats/features** follow the same recipe the existing 30 fo
