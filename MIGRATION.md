# Migrating Docling to Rust — `docling-crab`

This document is the plan for porting [docling](https://github.com/docling-project/docling)
from Python to Rust. It maps the existing architecture onto Rust, fixes the
target public API, and breaks the work into phases that each ship a **working,
testable vertical slice** rather than a big-bang rewrite.

> Status: **Phase 0 complete; Phase 2 underway.** The workspace builds, tests
> pass, and the headline API converts Markdown, CSV, **HTML**, **AsciiDoc**, and
> the **DeepSeek-OCR Markdown** VLM variant today — each byte-for-byte against
> live docling on the upstream fixtures (CSV 9/9, Markdown 10/10, AsciiDoc 4/4,
> DeepSeek 3/3, HTML 28/32). Run `cargo test`,
> `cargo run -p docling-crab-cli -- crates/docling-crab/sample.html`, and
> `scripts/conformance.sh <format>` (e.g. `html`, `asciidoc`, `md_deepseek`) to
> score output against upstream docling. See [`COMPARING.md`](./COMPARING.md) for
> the Python-vs-Rust comparison workflow.

---

## 1. Goals & non-goals

**Goals**

- A tiny, obvious public API (see §3). One `DocumentConverter`, one `convert`,
  one `DoclingDocument` you can `export_to_markdown()`.
- Pure-Rust, dependency-light parsing for the "easy" formats (Markdown, CSV,
  HTML, DOCX/PPTX/XLSX, EPUB, XML families).
- Output that is byte-compatible with docling-core's serializers where it
  reasonably can be, so the Rust port is a drop-in for downstream Markdown/JSON
  consumers.
- A `DoclingDocument` model that round-trips docling-core JSON.

**Non-goals (at least at first)**

- Reimplementing the ML stack (layout analysis, table-structure, OCR, VLMs) in
  Rust. Those are huge, model-weight-bound, and already well served by
  PyTorch/ONNX. The plan (§7) is to treat them as a pluggable inference
  boundary — call ONNX Runtime from Rust, or a remote model server, or shell out
  — not to rewrite the models.
- 100% format parity on day one. We sequence formats by value/effort (§6).

---

## 2. What docling actually is (the parts we must port)

From mapping the Python package, docling has four layers. The Rust port keeps
the same layering — it is a clean architecture and translates directly.

| Layer | Python location | Responsibility | Rust home |
|---|---|---|---|
| **Data model** | `docling-core` (external dep) | `DoclingDocument`, `DocItem`, labels, serializers (`export_to_markdown`, JSON) | `docling-crab-core` |
| **Converter** | `docling/document_converter.py` | Format detection, dispatch to backend+pipeline, `ConversionResult` | `docling-crab` (`converter.rs`) |
| **Backends** | `docling/backend/*` | Parse one input format → `DoclingDocument` (declarative) or pages (paginated) | `docling-crab` (`backend/*`) |
| **Pipelines + models** | `docling/pipeline/*`, `docling/models/*` | Page-level ML: layout, OCR, tables, reading order, VLMs | `docling-crab-pipeline` + inference boundary (later) |

Key Python entry points to mirror (for reference while porting):

- `DocumentConverter.convert()` — `docling/document_converter.py`
- `AbstractDocumentBackend` / `DeclarativeDocumentBackend` — `docling/backend/abstract_backend.py`
- `SimplePipeline` (declarative formats) — `docling/pipeline/simple_pipeline.py`
- `StandardPdfPipeline` (the heavy PDF/ML path) — `docling/pipeline/standard_pdf_pipeline.py`
- `InputFormat`, `FormatToExtensions`, `FormatToMimeType` — `docling/datamodel/base_models.py`

The crucial structural insight: **most formats are "declarative."** DOCX, PPTX,
XLSX, HTML, Markdown, CSV, ODF, EPUB, the XML families — all go straight to a
`DoclingDocument` with *no ML at all* (`SimplePipeline` just calls
`backend.convert()`). Only PDF and images need the recognition pipeline. So a
large fraction of docling's value is portable to pure Rust immediately, and the
hard ML part is quarantined behind one boundary.

---

## 3. Target public API

The whole point. This is what the user types, and it already works in Phase 0:

```rust
use docling_crab::{DocumentConverter, SourceDocument};

let converter = DocumentConverter::new();
let result = converter
    .convert(SourceDocument::from_file("input.docx").unwrap())
    .unwrap();
println!("{}", result.document.export_to_markdown());
```

Design rules:

- `DocumentConverter::new()` takes no required arguments. Configuration is
  opt-in (`DocumentConverter::with_allowed_formats([...])`, and later a builder
  for pipeline options).
- `SourceDocument::from_file` detects the format from the extension; there's
  also `from_bytes(name, format, bytes)` for in-memory input.
- `convert` returns `Result<ConversionResult, ConversionError>` — explicit
  errors, no exceptions. `ConversionResult { document, status, .. }`.
- `result.document` is a `DoclingDocument` with `export_to_markdown()` (and,
  later, `export_to_html()`, `export_to_json()`, `to_json_string()`).

Everything beyond this is layered on without changing these four lines.

---

## 4. Workspace layout

```text
docling-crab/
├── Cargo.toml                      # workspace
├── MIGRATION.md                    # this file
├── README.md
└── crates/
    ├── docling-crab-core/          # data model + serializers (≈ docling-core)
    │   └── src/{document,labels,markdown}.rs
    ├── docling-crab/               # converter, source loading, backends (≈ docling)
    │   └── src/{converter,source,format,result,error}.rs
    │       └── backend/{markdown,csv}.rs
    └── docling-crab-cli/           # thin CLI (≈ docling.cli.main)
```

Future crates (introduced when their phase lands, kept separate so the core
stays light):

- `docling-crab-pipeline` — page model, `StandardPdfPipeline` analogue.
- `docling-crab-models` — inference boundary (ONNX / remote / FFI).
- `docling-crab-py` — optional PyO3 bindings, so the Rust core can be dropped
  under the existing Python package incrementally (strangler-fig migration).

---

## 5. Dependency mapping (Python → Rust)

The Phase-0 scaffold is intentionally **std-only** so it builds offline. Each
crate below is added in the phase that needs it.

| Concern | Python | Rust crate | Phase |
|---|---|---|---|
| Data validation / schema | `pydantic` | `serde` + `serde_json` | 1 |
| Format sniffing | `filetype` | `infer` | 1 |
| Markdown parse | `marko` | `pulldown-cmark` | 2 |
| HTML parse | `beautifulsoup4` + `lxml` | `scraper` / `html5ever` | 2 |
| XML (JATS, USPTO, DocLang) | `lxml` | `quick-xml` / `roxmltree` | 2 |
| Office Open XML (DOCX/PPTX/XLSX) | `python-docx`, `python-pptx`, `openpyxl` | `zip` + `quick-xml` (hand-rolled OOXML) | 3 |
| ODF (ODT/ODS/ODP) | `odfdo` | `zip` + `quick-xml` | 3 |
| EPUB | stdlib `zipfile` | `zip` + `quick-xml` | 3 |
| CSV | stdlib `csv` | `csv` | 2 |
| AsciiDoc / DeepSeek-OCR tokens | stdlib `re` | `regex` | 2 |
| LaTeX | `pylatexenc` | hand-rolled / `nom` | 4 |
| Email | `mail-parser` | `mail-parser` (Rust crate, same name) | 4 |
| PDF page access | `pypdfium2` | `pdfium-render` | 5 |
| Images | `pillow`, `numpy` | `image` | 5 |
| ML runtime | `torch`, `transformers`, `docling-ibm-models` | `ort` (ONNX Runtime) or remote/FFI | 6 |
| OCR | `rapidocr`, `easyocr`, `tesserocr` | `ort`-hosted RapidOCR / `tesseract` FFI | 6 |
| HTTP (remote sources/services) | `requests`, `httpx` | `reqwest` | 4 |
| Parallelism | threads / `ThreadedStandardPdfPipeline` | `rayon` / `std::thread` | 5 |
| CLI | `typer` + `rich` | `clap` | 4 |
| Progress | `tqdm` | `indicatif` | 4 |

---

## 6. Format roadmap (sequenced by value ÷ effort)

Declarative formats first — they're pure parsing and cover most non-PDF usage.

1. **Markdown** ✅ (Phase 0) — done, round-trips; 10/10 fixtures byte-for-byte.
2. **CSV** ✅ (Phase 0) — done via the `csv` crate; 9/9.
3. **HTML** ✅ (Phase 2) — done via `scraper`/html5ever: headings, paragraphs,
   nested lists, tables (colspan/rowspan grid, row-header offsetting), code
   blocks, images, inline emphasis/links. 28/32 byte-for-byte; remaining gaps
   are browser-rendering-dependent (see §6a) and tracked in
   [`COMPARING.md`](./COMPARING.md).
4. **AsciiDoc** ✅ (Phase 2) — done via a `regex`-backed line parser: 4/4.
5. **DeepSeek-OCR Markdown** ✅ (Phase 2) — the VLM annotation-token variant of
   Markdown, auto-detected and parsed into nodes: 3/3.
6. **XLSX** ✅ (Phase 3) — done via `calamine` + the shared `ooxml` zip/rels
   helper: flood-fill region detection, merged cells, openpyxl-compatible value
   formatting, chartsheet/hidden-sheet skipping, embedded-image counting. 8/9.
7. **PPTX** ✅ (Phase 3) — done via `roxmltree` + the shared `ooxml` helper:
   per-slide shape tree → titles/paragraphs/lists/tables/pictures, bullet
   inheritance, merged table cells, embedded-image validation. 7/7.
8. **DOCX** ⚙️ (Phase 3, core) — paragraphs, styled headings, inline
   formatting, lists (paragraph + style numbering), tables with merges, images
   via `roxmltree` + the `ooxml` helper. 10/26; equations/rich-cells/complex
   layout deferred.
8. **EPUB** — zip of XHTML; reuses HTML backend.
9. **ODT / ODS / ODP** — `quick-xml` over the ODF zip.
10. **XML families** (JATS, USPTO, DocLang, XBRL) — `quick-xml`, one backend each.
11. **LaTeX, Email, WebVTT** — smaller parsers.
12. **JSON (docling)** — trivial once `serde` model exists; effectively a load.
13. **PDF / Images** — the big one; needs the pipeline (Phase 5–6).
14. **Audio (ASR)** — out of scope for core; remote/FFI to a Whisper backend.

### 6a. Deferred: the headless-browser rendering subsystem

A handful of HTML fixtures (and the HTML-form **key-value-pair / `form_region`**
extraction in `kvp_data_example`) can't be matched by parsing alone: upstream
docling renders the page in a **headless browser** and uses the resulting layout
— element visibility (to drop nav/menu/sidebar chrome), rendered bounding boxes
(to pad nested-table cells and to pair form keys with values), and synthesised
`<!-- missing-text -->` placeholders. This is a whole subsystem (a browser
engine + a layout-driven pass over the DOM), out of scope for the pure-parse
backends. It is the right home for the last ~4 HTML fixtures and KVP, and is
**deferred to a later phase** (likely alongside, or after, the PDF pipeline,
since both are layout/vision problems). Until then those fixtures are documented
as known, intentional gaps in [`COMPARING.md`](./COMPARING.md).

---

## 7. The hard part: PDF and the ML pipeline

PDF is where docling earns its reputation, and it's the riskiest port. Strategy:

- **Separate text-extraction from understanding.** `pdfium-render` gives us
  page text, words, and bounding boxes in pure-ish Rust (libpdfium FFI). Many
  born-digital PDFs need nothing more than geometry-based reading-order
  heuristics to produce a decent `DoclingDocument`. Ship that first — a
  "text-only PDF" backend with no neural models.

- **Quarantine the neural models behind one trait.** Define an inference
  boundary (e.g. `LayoutModel`, `TableStructureModel`, `OcrEngine` traits) with
  swappable implementations:
  - `ort` (ONNX Runtime) running exported docling-ibm-models weights in-process;
  - a remote model server (the Python repo already speaks KServe v2 / Triton);
  - PyO3/FFI calling the existing Python model code during transition.
  This lets the Rust converter exist and be useful long before any model is
  reimplemented, and avoids rewriting PyTorch.

- **Port the orchestration, not the math.** `StandardPdfPipeline`'s value is its
  staged, parallel page processing (preprocess → layout → OCR → table → reading
  order → assemble). That orchestration maps cleanly to `rayon` + channels; the
  per-stage model call is just a trait method.

Reading order, page assembly, and heading-hierarchy inference are partly
rule-based in Python and are good pure-Rust port candidates independent of the
neural models.

---

## 8. Data model & wire compatibility

The single most important compatibility surface is **docling-core's JSON**.
Downstream tools (chunkers, the docling JSON backend, integrations) consume it.

- Phase 1 replaces the simplified Phase-0 `Node` tree with a faithful model:
  docling-core stores `texts`, `tables`, `pictures`, `groups` arrays plus a
  `body` tree of `$ref` references (`#/texts/0`, …). We mirror that with
  `serde`, deriving `Serialize`/`Deserialize` to match field names exactly.
- Lock it down with **golden tests**: take real `*.json` outputs from the Python
  repo's `tests/data`, deserialize → reserialize in Rust, and assert equality.
  Do the same for `export_to_markdown` against the Python `.md` fixtures.
- The Python repo's existing regression fixtures (`tests/data/**`) become the
  Rust port's conformance suite — this is the objective definition of "correct."

---

## 9. Phased plan

Each phase is independently shippable and ends green (`cargo test`).

- **Phase 0 — Skeleton & API (done).** Workspace, `DocumentConverter`,
  `SourceDocument`, `DoclingDocument`, Markdown serializer, Markdown + CSV
  backends, CLI, tests. Proves the headline API.
- **Phase 1 — Real data model.** `serde`-based docling-core-compatible
  `DoclingDocument` (refs/arrays), `export_to_json`, format sniffing via
  `infer`, golden round-trip tests against Python JSON fixtures.
- **Phase 2 — Text/markup formats.** HTML ✅ (`scraper`); next: robust Markdown
  (`pulldown-cmark`), robust CSV (`csv` crate), JSON-docling load. Conformance
  vs Python `.md` via `scripts/conformance.sh` (see `COMPARING.md`).
- **Phase 3 — Office & e-book.** DOCX, PPTX, XLSX, EPUB, ODF via `zip`+`quick-xml`.
  This is where the literal `input.docx` example becomes real.
- **Phase 4 — Long tail + UX.** XML families, AsciiDoc, LaTeX, Email, WebVTT;
  `clap` CLI with output-format flags; `reqwest` for URL sources.
- **Phase 5 — PDF (text path).** `pdfium-render` text+geometry backend, page
  model, rule-based reading order/assembly, `rayon` parallelism. No neural nets yet.
- **Phase 6 — ML boundary.** `LayoutModel`/`OcrEngine`/`TableStructureModel`
  traits with an `ort` (ONNX) implementation and a remote-server implementation;
  wire into the PDF/image pipeline.
- **Phase 7 — Interop & adoption.** `docling-crab-py` (PyO3) bindings so the Rust
  core can replace Python backends one format at a time inside the existing
  package (strangler-fig), plus benchmarks vs the Python implementation.

---

## 10. Testing & verification strategy

- **Unit tests** per backend/serializer (Phase 0 already has 8).
- **Golden/conformance tests** driven by the Python repo's `tests/data` fixtures
  — the source of truth for both JSON and Markdown output (Phase 1+).
- **Differential testing**: run Python docling and `docling-crab` on the same
  corpus, diff the Markdown/JSON, triage divergences. Treat the Python output as
  the oracle until parity, then as a regression guard.
- **CI**: `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`.

---

## 11. Open questions / risks

- **docling-core schema drift.** It's an actively developed external package
  (`>=2.84.0`). We pin a target schema version and track its changelog; the
  golden tests catch drift.
- **PDF fidelity.** The neural layout/table models are the moat. A pure-Rust
  text-only PDF path will be visibly worse on scanned/complex PDFs until the ML
  boundary (Phase 6) is in place — set expectations accordingly.
- **OOXML/ODF edge cases.** `python-docx`/`odfdo` absorb years of
  real-world-file quirks. Expect a long tail of fixture-driven fixes.
- **libpdfium distribution.** `pdfium-render` needs a `pdfium` dynamic library;
  decide between bundling, system, or building.
- **ML reimplementation scope.** If/when models are ported off ONNX into native
  Rust inference, that's a separate, much larger effort — explicitly out of
  scope for this plan.

---

## 12. Why "crab"? 🦀

Ferris, the Rust mascot, is a crab. `docling-crab` = docling, in Rust.
