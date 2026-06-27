# Migrating Docling to Rust — Fleischwolf

A port of [docling](https://github.com/docling-project/docling) from Python to
Rust. This document is the **current status**: what is migrated, how it compares
to upstream docling, and what is intentionally not done yet. (The original
phased plan is kept at the end as history.)

> **Status: the format migration is essentially complete.** Every document
> format in docling's pipeline except **audio/ASR** is supported, plus Markdown
> (legacy + a Rust-only *strict* mode), docling-native **JSON** output, and
> **image extraction**. The declarative formats are pure-Rust and checked
> byte-for-byte against *live* docling; the PDF/image/METS ML path lives in
> `fleischwolf-pdf` and is checked against a deterministic snapshot baseline.
> `cargo test` is green (unit tests + a 131-source output-regression suite).

---

## 1. Architecture

Four layers, mirroring docling's:

| Layer | docling (Python) | `fleischwolf` (Rust) |
|---|---|---|
| **Data model + serializers** | `docling-core` | `fleischwolf-core` — `DoclingDocument`, the `Node` tree, Markdown + JSON serializers, base64 |
| **Converter** | `docling/document_converter.py` | `fleischwolf` — `converter.rs` (format dispatch + XML content sniffing) |
| **Backends** | `docling/backend/*` | `fleischwolf` — `backend/*` (one per format) |
| **PDF/ML pipeline** | `docling/pipeline/*`, `docling/models/*` | `fleischwolf-pdf` — pdfium + ONNX layout/OCR + assembly |
| **CLI** | `docling/cli` | `fleischwolf-cli` |

```text
crates/
├── fleischwolf-core/   # DoclingDocument, Node model, markdown.rs, json.rs, base64.rs, labels.rs
├── fleischwolf/        # DocumentConverter, source/format detection, backend/*.rs, ooxml.rs
├── fleischwolf-pdf/    # pdfium_backend, layout (RT-DETR/ONNX), ocr (PP-OCRv3/ONNX), assemble, mets
└── fleischwolf-cli/    # `--strict`, `--to md|json`, `--images placeholder|embedded|referenced`
```

The public API is unchanged from day one:

```rust
use fleischwolf::{DocumentConverter, SourceDocument};

let result = DocumentConverter::new()
    .convert(SourceDocument::from_file("input.docx")?)?;
println!("{}", result.document.export_to_markdown());   // or .export_to_json()
```

---

## 2. Format coverage

Conformance is measured against **live** docling (run from this repo's own
sources via `scripts/conformance.sh <fmt> --live`), not the committed
groundtruth `.md` (which predates docling-core's current table serializer — see
§4). "Exact" = byte-for-byte.

### Declarative formats — pure Rust, no models

| Format | Backend | Status |
|---|---|---|
| Markdown | `markdown.rs` (pulldown-cmark) | **10/10 exact** |
| CSV | `csv.rs` (`csv` crate) | **9/9 exact** |
| HTML | `html.rs` (scraper/html5ever) | **28/32 exact** (rest need a headless browser — §5) |
| AsciiDoc | `asciidoc.rs` (regex) | **4/4 exact** |
| DeepSeek-OCR Markdown | `deepseek.rs` | **3/3 exact** (auto-detected VLM-token variant) |
| XLSX | `xlsx.rs` (calamine) | **9/9 exact** |
| PPTX | `pptx.rs` (roxmltree) | **7/7 exact** |
| DOCX | `docx.rs` (roxmltree) | core (most fixtures); residual in §5 |
| WebVTT | `webvtt.rs` | **4/4 exact** |
| Email (.eml) | `email.rs` (mail-parser) | **2/2 exact** |
| EPUB | `epub.rs` → HTML backend | core exact (shares HTML residual) |
| ODF (odt/ods/odp) | `odf.rs` | core; residual in §5 |
| JATS | `jats.rs` (roxmltree) | core ~60% (metadata + sections + paragraphs) |
| USPTO | `uspto.rs` | modern `us-patent-*-v4x` core; residual in §5 |
| XBRL | `xbrl.rs` | arelle-free core (dei facts → title, `*TextBlock` → HTML) |
| JSON-docling | `docling_json.rs` (serde_json) | reads docling's native JSON; ~51/145 round-trip exact |
| LaTeX | `latex.rs` (scanner) | simple `.tex` ≈ live; multi-file arxiv out of scope |

Shared OOXML infrastructure (`ooxml.rs`): a `zip` reader, `.rels` parsing, part
content-type resolution, and image extraction — reused by DOCX/PPTX/XLSX/EPUB.

### ML formats — `fleischwolf-pdf`, snapshot baseline

These run docling's *discriminative* PDF pipeline ported to ONNX. Output is **not
byte-for-byte** with docling (different OCR/table models — §4); it is pinned by a
deterministic snapshot (`scripts/pdf_conformance.sh`, **76/76 exact**).

| Format | How |
|---|---|
| PDF | pdfium text cells + page render → RT-DETR layout (ONNX) → PP-OCRv3 OCR for scanned pages → geometric table reconstruction → reading-order assembly |
| Images (tiff/webp/png/jpeg) | the same pipeline, image as a single page |
| METS / Google Books | `.tar.gz` of per-page hOCR + TIFF → cells from hOCR → the same layout+assembly path (no OCR needed) |

---

## 3. Output formats

| Output | API / CLI | Notes |
|---|---|---|
| **Markdown (legacy)** | `export_to_markdown()` / default | byte-for-byte docling, quirks included |
| **Markdown (strict)** | `.strict(true)` / `--strict` | Rust-only cleaner mode — **no docling equivalent** |
| **JSON** | `export_to_json()` / `--to json` | docling-core native wire format (schema 1.10.0) |
| **Image extraction** | `export_to_markdown_with_images(mode, dir)` / `--images` | `placeholder` (default) · `embedded` (base64 data URI) · `referenced` (writes PNG files) |

- **JSON** rebuilds docling's full `body`-tree-of-`$ref`s model from the `Node`
  tree (texts/groups/tables/pictures, labels, list grouping, table grids,
  formula/code items, picture `ImageRef`s). It loads back into Python
  docling-core and **~91% round-trips** byte-identically to the direct Markdown.
- **Image extraction** is wired for PDF/image (figure-region crops) and
  DOCX/PPTX (embedded blobs); JSON always embeds extracted images as data URIs.

---

## 4. Differences from upstream docling

These are deliberate or unavoidable divergences, not bugs.

1. **Simplified document model.** `fleischwolf`'s `Node` enum
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

4. **Tables match *current* docling, not the committed fixtures.** docling-core's
   Markdown table serializer emits padded GitHub tables today; the repo's
   committed groundtruth `.md` corpus predates that and uses a minimal `| - |`
   format. `fleischwolf` matches the **current/live** output — so table-bearing
   formats look correct against live docling and "wrong" against the stale `.md`.

5. **The PDF pipeline is discriminative and partial.** Ported from docling's
   standard pipeline, with substitutions:
   - **Layout** — RT-DETR (`docling-layout-heron`) exported to ONNX, run via
     `ort`. Same model family as docling.
   - **OCR** — PP-OCRv3 recognition (RapidOCR) via ONNX, *not* docling's default
     EasyOCR; different recognizer → different scanned text.
   - **Tables** — *geometric* grid reconstruction (cluster cells into rows/cols),
     **not TableFormer** (docling's autoregressive table-structure model). Table
     structure is approximate; complex spans are not recovered.
   - Output is therefore a **snapshot baseline**, never byte-for-byte with docling.

6. **Extracted image bytes are real but not byte-identical.** Cropped/embedded
   pixels are correct, but the PNG re-encoding differs from docling's, so the
   base64 in `embedded` mode / JSON `ImageRef`s won't match byte-for-byte.

7. **XML format detection sniffs content.** JATS, USPTO and XBRL all use `.xml`;
   the converter routes by content markers (`us-patent` → USPTO, `us-gaap`/`dei`
   → XBRL, else JATS) rather than the extension alone.

8. **No headless-browser pass.** A few HTML behaviours depend on rendering the
   page (nav/visibility suppression, form key-value regions, rendered bounding
   boxes) — see §5.

---

## 5. Not migrated / out of scope

Explicitly **not done**, with the reason:

- **Audio / ASR.** docling's Whisper-based speech path. A separate ML boundary
  like PDF; deferred by design.
- **VLM pipelines** (SmolDocling / remote VLM) and **enrichment models** (picture
  classification, formula understanding, code understanding). Model-bound; out of
  scope for the discriminative port.
- **TableFormer.** Replaced by geometric table reconstruction (§4.5).
- **XML DocLang / DocTags** input backend — no `.dclg` sources in the corpus to
  verify against, and not in the requested scope.
- **Older patent schemas.** USPTO covers the modern `v4x` XML only; the
  `pap-v1` / 2001-era `pa`/`pg` schemas and the legacy **APS text** (`pftaps`)
  format are not handled (two files even use HTML entities roxmltree rejects).
- **JATS article-body machinery** — tables, figures, references/citations, lists
  and formula rendering inside `<body>` (metadata + sections + paragraphs are
  done).
- **ODF deep quirks** — mixed-style list continuation, empty-list-item level
  collapse, ODS sheet→table region detection with numeric alignment, rich table
  cells.
- **DOCX long tail** — full Word multilevel list/heading *shared* numbering,
  position-sorted textbox/shape-text layout, advanced OMML + inline-equation
  spacing.
- **HTML browser-render subsystem** — nav/visibility suppression (`wiki_duck`),
  form key-value-pair regions (`kvp_data_example`), deep nested-table cell padding
  from rendered bounding boxes. ~4 HTML fixtures + KVP.
- **Image extraction for HTML/EPUB.** External `<img src>` files are not fetched
  (same as docling's default `enable_*_fetch=False`); only embedded blobs (DOCX/
  PPTX) and PDF crops are extracted.
- **PyO3 bindings** (`fleischwolf-py`) for a strangler-fig drop-in — not built.

---

## 6. Testing

- **`cargo test`** — unit tests per backend/serializer **plus an output-
  regression suite** (`crates/fleischwolf/tests/regression.rs`): every
  declarative source under `crates/fleischwolf/tests/data/<fmt>/sources/` is
  converted to legacy Markdown, strict Markdown and docling JSON and compared to
  committed fixtures (131 sources × 3). `FLEISCHWOLF_REGEN=1` refreshes them.
  The JSON fixtures double as a docling-core load check.
- **Snapshot harness** — `scripts/pdf_conformance.sh` regenerates and diffs the
  PDF/image/METS baseline (needs pdfium + the ONNX models; **76/76 exact**).
- **Live conformance** — `scripts/conformance.sh <fmt> --live` scores a format
  against the latest published docling (installed from PyPI; see
  [`COMPARING.md`](./COMPARING.md)).
- **Differential / perf** — `scripts/compare.sh`, `scripts/performance.sh`.

CI (`.github/workflows/ci.yml`) gates every pull request and master push on
`cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` and
`cargo test` (the fast pure-Rust suite — no model downloads). On master it then
runs `scripts/ci_publish.sh`, which publishes any version-bumped crate to
crates.io in dependency order and skips those already published.

---

## 7. Goals & design rules (unchanged)

- A tiny, obvious public API — one `DocumentConverter`, one `convert`, one
  `DoclingDocument` you can `export_to_markdown()` / `export_to_json()`.
- Dependency-light pure-Rust parsing for everything that isn't ML.
- Output byte-compatible with docling-core's serializers where it reasonably can
  be, so the port is a drop-in for downstream Markdown/JSON consumers.
- The ML stack is *not* reimplemented in PyTorch-equivalent Rust; it is
  quarantined behind ONNX (`ort`) inference in `fleischwolf-pdf`.

---

## Appendix — original phased plan (history)

The port followed roughly: **Phase 0** skeleton & API → **Phase 2** text/markup
(Markdown, CSV, HTML, AsciiDoc, DeepSeek) → **Phase 3** Office & e-book (DOCX,
PPTX, XLSX, EPUB, ODF) → **Phase 4** long tail (XML families, LaTeX, Email,
WebVTT, JSON) → **Phase 5–6** the PDF/image ML pipeline (pdfium + ONNX layout/OCR
+ geometric tables) → output formats (strict Markdown, JSON, image extraction).
Audio/ASR (the old "Phase 7" tail) and PyO3 interop bindings remain the main
unbuilt pieces.

## Why "Fleischwolf"? 🦀

A *Fleischwolf* (German for "meat grinder") is the machine you push anything
through to get a single, uniform mince — which is exactly what this does to
documents: PDF, DOCX, HTML, XLSX … all come out as one `DoclingDocument`. And
it's written in Rust, so Ferris the crab 🦀 still gets a seat.
