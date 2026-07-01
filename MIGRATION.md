# Migrating Docling to Rust — Fleischwolf

A port of [docling](https://github.com/docling-project/docling) from Python to
Rust. This document is the **current status**: what is migrated, how it compares
to upstream docling, and what is intentionally not done yet. (The original
phased plan is kept at the end as history.)

> **Status: the format migration is essentially complete.** Every document
> format in docling's pipeline except **audio/ASR** is supported, plus Markdown
> (legacy + a Rust-only *strict* mode), docling-native **JSON** output, **image
> extraction**, and **MHTML** (a fleischwolf-only extension docling doesn't
> have). The declarative formats are pure-Rust and checked byte-for-byte
> against *live* docling; the PDF/image/METS ML path lives in `fleischwolf-pdf`
> (a pure-Rust PDF text parser + pdfium rasterization + ONNX
> layout/TableFormer/OCR + a port of docling-parse's line sanitizer) and is also
> measured byte-for-byte against live docling — **6 / 14 PDF fixtures exact, 7 / 14
> whitespace-normalized** (see `PDF_CONFORMANCE.md`), with a snapshot baseline
> guarding against regressions. `cargo test` is green (unit tests + a 133-source
> output-regression suite).

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
├── fleischwolf-cli/    # `--strict`, `--to md|json`, `--images placeholder|embedded|referenced`
└── fleischwolf-node/   # Node.js/Bun N-API bindings (napi-rs), published to npm as `fleischwolf`
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

Conformance is measured against the latest **published** docling (installed from
PyPI; run via `scripts/conformance.sh <fmt>`), not the committed groundtruth
`.md` (which predates docling-core's current table serializer — see §4).
"Exact" = byte-for-byte.

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
| ODF (odt/ods/odp) | `odf.rs` | core + list continuation + rich table cells + ODS table regions; residual in §5 |
| JATS | `jats.rs` (roxmltree) | metadata + full `<body>`/`<back>` (tables, figures, references, lists, footnotes, formulas) |
| USPTO | `uspto.rs` | modern `us-patent-*-v4x` core; residual in §5 |
| XBRL | `xbrl.rs` | arelle-free core (dei facts → title, `*TextBlock` → HTML) |
| JSON-docling | `docling_json.rs` (serde_json) | reads docling's native JSON; ~51/145 round-trip exact |
| LaTeX | `latex.rs` (scanner) | simple `.tex` ≈ live; multi-file arxiv out of scope |
| MHTML (.mhtml/.mht) | `mhtml.rs` (mail-parser) → HTML backend | **fleischwolf extension — no docling backend to compare against**; embedded images resolved by `Content-Location`/`cid:` |

Shared OOXML infrastructure (`ooxml.rs`): a `zip` reader, `.rels` parsing, part
content-type resolution, and image extraction — reused by DOCX/PPTX/XLSX/EPUB.

### ML formats — `fleischwolf-pdf`

These run docling's *discriminative* PDF pipeline ported to ONNX. They are now
measured **byte-for-byte against live docling** (the committed PDF groundtruth is
regenerated from it): **6 / 14 exact (7 / 14 whitespace-normalized)**, the rest
close — see `PDF_CONFORMANCE.md`. A deterministic snapshot baseline
(`scripts/pdf_conformance.sh`) still guards against regressions.

| Format | How |
|---|---|
| PDF | **pure-Rust text parser** (`textparse.rs`, font-advance glyph boxes) + pdfium page render → RT-DETR layout (ONNX) → **TableFormer** table structure (ONNX) → PP-OCRv3 OCR for scanned pages → **docling-parse line sanitizer** (`dp_lines.rs`) + reading-order assembly |
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
- **Image extraction** is wired for PDF/image (figure-region crops) and DOCX/PPTX
  (embedded blobs) by default, and — opt-in via
  `DocumentConverter::fetch_images` (`--fetch-images`) — for HTML/EPUB `<img src>`:
  `data:` URIs, local files (relative to the source), remote `http(s)` URLs, and
  EPUB archive entries. Off by default, matching docling's `enable_*_fetch=False`.
  JSON always embeds extracted images as data URIs.

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
   - **OCR** — PP-OCRv3 recognition (RapidOCR) via ONNX, *not* docling's default
     EasyOCR; different recognizer → different scanned text.
   - **Tables** — **TableFormer** (image encoder + autoregressive OTSL structure
     decoder + cell-bbox decoder, ported to ONNX), on a cv2-exact preprocessed
     crop. Reproduces docling's padded GitHub tables — `2305-pg9` is cell-for-cell
     exact; multi-row headers / spans on the dense papers still differ.
   - **Text** — a **pure-Rust PDF text parser** (`textparse.rs`, on `lopdf`)
     reconstructs glyph boxes from font advance widths + the text/graphics matrices
     (matching docling-parse's geometry, not pdfium's rendered boxes); handles
     Type0/CID + simple fonts, ToUnicode/encodings, Form XObject recursion, a
     glyph-name fallback, and overprint dedup. It is the default text layer
     (`DOCLING_PDFIUM_TEXT=1` falls back to pdfium). Its cells feed a port of
     docling-parse's line sanitizer (`dp_lines.rs`): 3-pass corner-distance
     contraction with gap-proportional space insertion, `enforce_same_font`,
     ligature recomposition, loose-box geometry. Plus docling's markdown escaping,
     typographic-punctuation normalization, wrap dehyphenation,
     paragraph-continuation merging, band-aware two-column reading order, and
     false-picture / page-number layout fixes.
   - Output is measured **byte-for-byte against live docling** (PDF_CONFORMANCE.md):
     **6 / 14 exact, 7 / 14 whitespace-normalized**, the rest close. The remaining
     gaps are model-level (TableFormer structure on complex tables, layout
     classification) plus `amt`'s fraction spacing (a docling quirk).

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

- **XML DocLang / DocTags** input backend — no `.dclg` sources in the corpus to
  verify against, and not in the requested scope.
- **Older patent schemas.** USPTO covers the modern `v4x` XML only; the
  `pap-v1` / 2001-era `pa`/`pg` schemas and the legacy **APS text** (`pftaps`)
  format are not handled (two files even use HTML entities roxmltree rejects).
- **ODF presentation title/shape/notes** — slide-title heading detection, free
  shape-text extraction and the drop of speaker-notes on `.odp` slides. The
  mixed-style **list continuation**, empty-list-item level collapse,
  **ODS sheet→table region detection with numeric alignment**, and **rich table
  cells** are now done (a flood-fill splits a sheet into its disconnected data
  regions; `<text:list>` siblings continue numbering across an empty nested item;
  a cell holding lists/nested tables/images/multiple paragraphs renders its full
  block content flattened into the cell while a plain cell stays unformatted, and
  merged cells leave their covered columns blank). What remains on `.odt` is
  charts/embedded-object frames (`text_document_02`).
- **DOCX grouped/anchored drawings** — position-sorted layout of grouped shapes
  and `<mc:AlternateContent>` image de-duplication (`drawingml` fixture). The
  Word multilevel list/heading *shared* numbering and **advanced OMML +
  inline-equation spacing** are now done (inline equations reproduce docling's
  inline-group spacing and stay attached to their list item; `\operatorname`
  functions, limit-label space escaping and the two-space symbol padding match
  pylatexenc byte-for-byte).
- **HTML browser-render subsystem** — nav/visibility suppression (`wiki_duck`),
  form key-value-pair regions (`kvp_data_example`), deep nested-table cell padding
  from rendered bounding boxes. ~4 HTML fixtures + KVP.


---

## 6. Extensions

- **PyO3 bindings** (`fleischwolf-py`) for a strangler-fig drop-in.
- **C++** bindings
- `fleischwolf-rag` - basic documents processing/chunking/vectorization/semantic-search system with pluggable DB support, PostgreSQL/SQLite, embedding with small ONNX local model (test options, dimensions >= 1024). 
  
## 7. Testing

- **`cargo test`** — unit tests per backend/serializer **plus an output-
  regression suite** (`crates/fleischwolf/tests/regression.rs`): every
  declarative source under `crates/fleischwolf/tests/data/<fmt>/sources/` is
  converted to legacy Markdown, strict Markdown and docling JSON and compared to
  committed fixtures (133 sources × 3). `FLEISCHWOLF_REGEN=1` refreshes them.
  The JSON fixtures double as a docling-core load check.
- **Snapshot harness** — `scripts/pdf_conformance.sh` regenerates and diffs the
  PDF/image/METS baseline (needs pdfium + the ONNX models; **91/91 exact**).
- **Conformance** — `scripts/conformance.sh <fmt>` scores a format against the
  latest published docling (installed from PyPI; see
  [`COMPARING.md`](./COMPARING.md)).
- **Differential / perf** — `scripts/compare.sh`, `scripts/performance.sh`.

CI (`.github/workflows/ci.yml`) gates every pull request and master push on
`cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` and
`cargo test` (the fast pure-Rust suite — no model downloads). fmt/clippy run on a
**pinned** toolchain (`LINT_TOOLCHAIN` in the workflow) so a new stable can't fail
CI on unrelated commits; tests run on current `stable`. On master it then runs
`scripts/release.sh`: it derives the next version from the conventional-commit
messages since the last `v*` tag (`feat:` → minor, `fix:`/`perf:` → patch, a
`type!:`/`BREAKING CHANGE` → major; docs/chore/ci/etc → no release), bumps the
workspace version, commits + tags it (with `[skip ci]`, via `GITHUB_TOKEN`, so it
doesn't loop), and publishes the crates with `scripts/ci_publish.sh` in
dependency order — skipping any version already on crates.io.

---

## 8. Goals & design rules (unchanged)

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
