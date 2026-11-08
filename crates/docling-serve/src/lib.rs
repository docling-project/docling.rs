//! `docling-rs serve` — a long-running HTTP server over the docling.rs
//! converter, the analogue of Python's `docling-serve`.
//!
//! Endpoints:
//!
//! | Method | Path          | Description                                        |
//! |--------|---------------|----------------------------------------------------|
//! | GET    | `/`           | API docs + an interactive test form                |
//! | POST   | `/v1/convert` | convert an upload (multipart) or a URL (JSON body) |
//! | POST   | `/v1/convert/async` | same request, returns a task id (#182)       |
//! | GET    | `/v1/status/{id}` | async job status (pending/started/success/failure) |
//! | GET    | `/v1/result/{id}` | async job result (the sync response, stored)   |
//! | GET    | `/v1/config`  | server capabilities (`{"allow_url_fetch": bool}`)  |
//! | GET    | `/health`     | liveness probe                                     |
//! | GET    | `/ready`      | readiness probe (`--warmup`: 200 once models load) |
//! | GET    | `/metrics`    | Prometheus metrics (#297, see [`o11y`])            |
//! | GET    | `/openapi.yaml` | OpenAPI 3.1 description of the API               |
//! | GET    | `/logo.svg`   | the playground's logo                              |
//!
//! `POST /v1/convert` accepts either `multipart/form-data` with a `file` part
//! (the filename's extension selects the input format; **several file parts
//! make a batch**, #182 — a Markdown, text, HTML or LaTeX batch answers in
//! that format, the documents in request order: [`BATCH_SEPARATOR`] lines
//! between them for Markdown and text (Markdown streaming document by
//! document), `<hr><hr><hr>` between the documents' `page` divs of one HTML
//! page, `\clearpage` between them in one LaTeX document, a failed one as
//! an `Error converting <name>: …` line in its place; the formats that
//! cannot be joined — JSON, chunks, Pandoc, VTT (one media file's cues),
//! dclx (a binary archive), images — answer a JSON `results` array with
//! per-item status) or an `application/json` body
//! `{"url": "https://…", "file_name"?: "override.pdf"}`.
//! Options ride along as multipart text parts, JSON fields, or query
//! parameters (body wins over query):
//!
//! - `to` — `md` (default) | `json` | `html` (#492) | `text` (#613: docling's plain
//!   text, `export_to_text`) | `dclx` | `chunks` | `latex` (#317) |
//!   `pandoc` (#515: Pandoc's AST as JSON, for `pandoc -f json`) | `vtt`
//!   (#614: WebVTT subtitles, a cue per timed text item) | `images` (#243:
//!   rasterize a PDF's pages to PNG — no conversion, no models;
//!   the JSON response is `{"pages": [{"page", "width", "height",
//!   "png_base64"}]}`, combines with `pages` for a window, capped at
//!   `DOCLING_RS_MAX_RASTER_PAGES` pages per request, default 100)
//! - `scale` — `to=images` render scale in pixels per PDF point:
//!   0.1–4.0, default 2.0 (= 144 dpi, the ML pipeline's own render scale)
//! - `strict` — cleaner Markdown instead of docling-legacy output
//! - `images` — `placeholder` (default; `embedded` for `to=pandoc`, #537) | `embedded`
//!   (Markdown, HTML and the Pandoc AST)
//! - `no_ocr`, `text_layer_only`, `no_table_former`, `force_full_page_ocr`,
//!   `no_text_panels`, `heading_hierarchy` — PDF/image pipeline switches (`no_ocr`:
//!   keep layout + TableFormer, never OCR — docling's `do_ocr=False`, its
//!   `--no-ocr`; `skip_ocr` is its pre-2.0 name, still read; `text_layer_only`
//!   skips the whole ML stack, what `no_ocr` meant before 2.0, #611)
//! - `password` — the password of an encrypted PDF or Office document
//!   (#611, #625; `pdf_password`, docling's name, is read too)
//! - `do_picture_classification`, `do_code_enrichment`,
//!   `do_formula_enrichment` — the opt-in enrichment models (#423; docling's
//!   `PdfPipelineOptions` flags of the same names, the CLI's
//!   `--enrich-picture-classes` / `--enrich-code` / `--enrich-formula`):
//!   DocumentFigureClassifier over pictures, CodeFormulaV2 over code / formula
//!   regions. Off by default; a missing model warns and skips the pass
//! - `redact_pii`, `redact_mode`, `redact_kinds`, `redact_pattern`,
//!   `redact_images` — the PII redaction pass (#621, a docling.rs extension):
//!   personal data is replaced in the document model before any export
//!   (`label` | `pseudonym` | `fixed:<text>`; comma-separated kinds;
//!   `NAME=REGEX` lines; `drop` | `box_out` | `keep` for images). A single
//!   response says what was removed in `X-Docling-Redaction` (counts per
//!   label, JSON); batch / async items carry `redaction`. Never the values.
//! - `pages` — PDF page window `A-B` / `N` (1-based inclusive, #80)
//! - `document_timeout` — per-document budget in seconds for the PDF pipeline
//!   (docling's `document_timeout`, #497): checked between pages; once spent
//!   the pages done so far are the document, `status` is `partial_success`
//!   and `errors` carries the reason (batch / async items); a single
//!   response says so in `X-Docling-Status` / `X-Docling-Errors`
//! - `ocr_lang` — OCR recognition language for scanned pages: `en` (default)
//!   | `ch` (the multilingual docling-conformance model); under
//!   `ocr_engine=tesseract` a tessdata stem list (`deu+fra`) or BCP-47 tags
//! - `ocr_engine` — which OCR engine reads scanned pages (#460): `ppocr`
//!   (default, the built-in PP-OCRv3 recognizer) | `tesseract` (the
//!   server's `tesseract` binary)
//! - `ocr_mode` — which regions feed the OCR (docling's `OcrMode`, #254):
//!   `default` | `full_page` | `layout_regions` | `pdf_aware_layout_regions`
//!   (`full_page`/`layout_regions` discard the text layer like
//!   `force_full_page_ocr`)
//! - `replace_broken_text_layer` — OCR only the PDF pages whose text layer a
//!   broken font mapping produced (docling's `replace_broken_text_layer`,
//!   #1006); every other page keeps its text layer
//! - `ocr_scale` — OCR render scale in px per PDF point (docling's
//!   `OcrOptions.scale`, #254); unset reads the pipeline's own 2.0 px/pt
//!   render, docling's default is 3 (216 dpi)
//! - `pdf_word_gap` — where a PDF's text layer splits words drawn without
//!   a space glyph (#921): `adaptive` (default, docling-parse ≥ 7's
//!   line-local threshold) | `legacy` (docling-parse ≤ 6) | a factor in
//!   (0, 4] of the two flanking glyphs' mean advance
//! - `pdf_report_repairs` — report what the PDF reader repaired to read the
//!   input (#934): `X-Docling-Pdf-Repairs` on a single response, a
//!   `pdf_repairs` list on a batch / async item; off by default
//! - `pdf_keep_typography` — keep a PDF's typographic punctuation (`„ “`,
//!   `‚ ‘`, dashes, `…`) as drawn instead of folding it to ASCII (#921)
//! - `ocr_textline_orientation` — RapidOCR's text-line orientation
//!   handling (#666): oriented line crops, tall lines turned 90°, the 0/180
//!   classifier (`.models/ocr_textline_ori.onnx`); off by default
//!   (`DOCLING_RS_OCR_TEXTLINE_ORIENTATION`)
//! - `picture_ocr_scale` — the picture OCR's resolution (#666) in px per
//!   picture pixel under RapidOCR's 2000 px cap; `1.0` reads pictures at
//!   their native pixels, unset keeps the image-input rule
//! - `images_scale` — picture crops (and page images) in px per PDF point,
//!   0.1–4.0 (docling's `images_scale`, #520); unset keeps the pipeline's
//!   2.0 px/pt render. The JSON picture `dpi` is 72·scale (#519)
//! - `page_images` — keep each page's render as the JSON `pages[n].image`
//!   (docling's `generate_page_images`, #520; off by default)
//! - `use_embedded_picture_images` — a PDF picture whose box holds exactly
//!   one embedded image with nothing drawn over it gets that bitmap at native
//!   resolution instead of the render crop (docling's option, #1007; off)
//! - `fetch_images` — resolve external `<img src>` for HTML/EPUB/MHTML/JATS (outbound
//!   fetch, so honored only under `--allow-url-fetch`; the alias of
//!   `image_sources=remote`)
//! - `image_sources` — `none` | `embedded` | `local` | `remote` (#646): which image
//!   references resolve. `remote` without `--allow-url-fetch` is held to
//!   `embedded`; `local` needs `--allow-local-images` (400 otherwise).
//!   `image_hosts`, `max_images`, `max_image_bytes`, `max_image_total_mb`,
//!   `min_image_bytes` bound it.
//! - `extract_diagrams` — DOCX DrawingML drawings (#858): `mermaid`
//!   (default) adds a `mermaid` code item after a drawing's picture with the
//!   flowchart its connectors describe, when every connector names its two
//!   end shapes | `none` (the picture alone)
//! - `use_outline_level_for_headings` — Word (DOCX/DOC): `false` stops a
//!   style's outline level alone from making a heading (docling#4480;
//!   default `true`)
//! - `ipynb_outputs` — Jupyter notebooks (#673): `none` (default, the
//!   notebook as written) | `text` (stream and `text/plain` results as code
//!   blocks after their cell, image outputs as pictures)
//! - `metadata` — document metadata (#679): `none` (default) | `json` (a
//!   `metadata` object in the docling JSON) | `frontmatter` (YAML front
//!   matter at the top of the Markdown as well)
//! - `skip_empty_cells` — omit empty cells from sparse XLSX/XLS table grids
//!   (#271; docling.rs extension, off by default)
//! - `compact_tables` — unpadded `| a | b |` Markdown tables, all formats
//!   (#271; docling.rs extension, off by default)
//! - `picture_alt_text` — pictures' alt text (DOCX/PPTX/XLSX, #676) in the
//!   Markdown and HTML output; `strict` implies it, JSON has it always
//! - `pandoc_api_version` — the Pandoc API the client's `pandoc` reads (`to=pandoc`,
//!   #515): only `1.23` is written, so another version is a 400, not a document
//!   Pandoc would reject
//! - `pandoc_heading_depth` — `docling` (default) | `source`: the `Header` depth
//!   a section heading gets in the AST (#848; docling's title-reserving `n + 1`,
//!   or the depth the DOCX / ODT author wrote)
//! - `page_break_placeholder` (alias `md_page_break_placeholder`, this
//!   server's historical spelling) — text inserted between pages in Markdown
//!   output (docling-core's `MarkdownParams.page_break_placeholder`; e.g.
//!   `<!-- page break -->`).
//!   A break lands only between two rendered blocks on different pages;
//!   unset (the default) keeps docling's break-free Markdown
//! - `list_attachments` — email (.eml/.msg): append an Attachments section
//!   with names and content types (#251; payload bytes are never embedded)
//! - `ebcdic_layout` — EBCDIC (#252): the copybook layout as inline
//!   `EbcdicLayout` JSON (mandatory for the format — the bytes are
//!   meaningless without it)
//! - `xbrl_taxonomy` — XBRL (#466): a server-local relative directory holding
//!   the instance's taxonomy (extension schema and linkbases, taxonomy
//!   packages), docling's `XBRLBackendOptions.taxonomy`; without it the fact
//!   graph has no presentation/calculation hierarchy
//! - `chunker`, `chunk_tokenizer`, `chunk_max_tokens`, `chunk_merge_peers` —
//!   per-request `to=chunks` configuration (#256, mirroring docling's
//!   service-datamodel `ChunkerType`/`HybridChunkerOptions`): pick one
//!   chunker (`hierarchical` | `hybrid`; unset returns both), a server-local
//!   relative `tokenizer.json` path, the hybrid token budget (default 256 /
//!   `DOCLING_CHUNK_MAX_TOKENS`) and peer-merging (default true). An
//!   explicitly requested `chunker=hybrid` without a usable tokenizer is a
//!   400 instead of the legacy silent skip
//!
//! A JSON body may also use docling's service-datamodel `sources`/`target`
//! shape instead of the `{"url": …}` shorthand (#139, see `passthrough.rs`):
//! `kind`-tagged sources (`file` base64 uploads, `http` URLs with headers,
//! and — behind the `cloud` cargo feature — `s3` / `azure_blob` /
//! `google_cloud_storage` prefixes) plus an optional `kind`-tagged output
//! `target` (`inbody` default, or the same cloud stores: each converted
//! output uploads as `<stem>.<ext>` under the prefix and the response is a
//! `RemoteTargetResult` acknowledgment). Everything outbound — `http` and
//! cloud alike — sits behind `--allow-url-fetch`.
//!
//! Markdown converts through the streaming serializer and the response body
//! streams page by page (chunked transfer); `json`/`dclx`/`chunks` (and every
//! batch/async result) buffer.
//!
//! Responses carry the conversion-confidence report (#183) when the PDF/image
//! ML pipeline ran: an `X-Docling-Confidence` header with the document-level
//! summary (grades + scores, docling's `ConfidenceReport` semantics) on every
//! output format, and a top-level `"confidence"` key with the per-page
//! breakdown appended to `to=json` bodies. Declarative conversions have no ML
//! stages and carry neither.
//!
//! `POST /v1/convert/async` (#182) accepts exactly the `/v1/convert` request
//! and answers `202 {"task_id": …}` immediately; the job queues on the same
//! concurrency semaphore (reusing the warm pipeline) and the result is
//! fetched with `GET /v1/result/{id}` once `GET /v1/status/{id}` reports
//! `success`. Results are held for `--result-ttl` seconds; at most
//! `--queue-size` jobs may be queued/unfetched at once (429 beyond that).
//!
//! One warm [`Pipeline`] (layout/OCR/TableFormer sessions) is shared across
//! requests behind a mutex — PDF/image conversions serialize on it instead of
//! reloading models. Declarative formats convert on blocking threads and run
//! concurrently. A semaphore bounds total in-flight conversions
//! (`--concurrency`); excess requests queue. `--idle-unload-secs N`
//! (`DOCLING_RS_IDLE_UNLOAD_SECS`; #842) drops that warm pipeline after `N`
//! seconds without a PDF/image conversion — its ONNX sessions and, on a GPU,
//! their device arenas go with it (a T4 measured 7.5 GB → 170 MB) — and the
//! next such request reloads it lazily (~1 s on a GPU); `/ready` reports
//! `models: lazy` meanwhile. On a GPU the sessions also shrink their arenas
//! after every run and the page-worker pool and OCR lanes default to one
//! ([`docling_onnx::run_options`], [`docling_onnx::prefers_single_worker`]).
//!
//! Resource controls (#262/#263): thread pools size to the **cgroup-aware**
//! CPU budget (a container's CPU quota clamps them; `DOCLING_RS_TF_INTRA`
//! additionally narrows the shared TableFormer session). The server binary
//! defaults `DOCLING_RS_NO_ARENA=1` — ONNX Runtime's CPU arena grows with
//! every new page shape and never returns memory; without it (plus a
//! `malloc_trim` after each conversion) a warm server's retained RSS measured
//! ~3× lower and stopped ratcheting, at no observed latency cost. glibc's
//! mmap threshold is pinned at startup as well (#669, [`docling::memory`]),
//! so large buffers are unmapped when freed instead of fragmenting the
//! per-thread arenas: RSS then holds flat across requests (~590 MB over six
//! mixed PDFs) where the trim alone let it creep (528 -> 754 MB). A memory
//! **ceiling** (`--max-memory-mb` / `DOCLING_RS_MAX_MEMORY_MB`, else the
//! container's cgroup limit; `0` disables) drives admission control: once
//! process RSS crosses the watermark (85%, `DOCLING_RS_MEMORY_WATERMARK_PCT`)
//! new conversions answer **503 + Retry-After** instead of being accepted and
//! OOM-killing the whole server; `/v1/config` reports `max_memory_mb` and the
//! live `rss_mb`.
//!
//! Security: URL fetching makes the server issue outbound requests (SSRF
//! surface), so it is **off by default** — enable with `--allow-url-fetch`.
//! Even when enabled, targets that resolve to a private/loopback/link-local
//! address are refused and redirects are disabled. The server itself has no
//! authentication: bind to loopback (the default) or front with a policy/auth
//! proxy before exposing it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use docling::{
    ConversionError, DoclingDocument, DocumentConverter, ImageMode, InputFormat, Pipeline,
    SourceDocument,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Semaphore;

mod compat;
pub mod o11y;
mod passthrough;

/// Server configuration (see the binary's `--help` for the flag spellings).
#[derive(Clone, Debug)]
pub struct ServeConfig {
    /// Bind address, e.g. `127.0.0.1:5001`.
    pub addr: String,
    /// Maximum conversions in flight; further requests queue on the semaphore.
    pub concurrency: usize,
    /// Maximum accepted request body (multipart upload) in bytes.
    pub max_body_bytes: usize,
    /// Load the PDF/image models at startup — the serial worker, TableFormer
    /// and the multi-page worker pool — so `/ready` flips only when the first
    /// conversion would be fast, and stays unready if they fail to load. Off:
    /// models load lazily on first use and `/ready` is 200 at once.
    pub warmup: bool,
    /// Allow `{"url": …}` inputs (outbound fetch — SSRF surface). Off by
    /// default: even with the built-in private/loopback/link-local IP guard,
    /// letting a caller name the fetch target is a deliberate exposure that a
    /// deployment must opt into (`--allow-url-fetch`).
    pub allow_url_fetch: bool,
    /// Allow `image_sources=local` (#646): a request may then make the
    /// converter read image files next to a server-side source (a URL
    /// input has none; an upload has none either, so this only matters for
    /// deployments that mount documents). Off by default — refused with 400.
    pub allow_local_images: bool,
    /// Default `strict` for requests that don't set it.
    pub strict: bool,
    /// Maximum async jobs (#182) waiting or running at once; further
    /// `POST /v1/convert/async` submissions are refused with 429. Bounds the
    /// memory held by queued request bytes.
    pub queue_size: usize,
    /// How long a finished async job's result stays fetchable before it is
    /// evicted (idle results are the other thing holding memory).
    pub result_ttl_secs: u64,
    /// Memory ceiling in MB for admission control (#263). `None` = detect the
    /// container's cgroup limit at startup; `Some(0)` disables the ceiling.
    /// Once the process RSS crosses the watermark (85% of the ceiling by
    /// default; `DOCLING_RS_MEMORY_WATERMARK_PCT` overrides), new conversions
    /// answer 503 instead of being accepted and OOM-killing the whole server
    /// (exit 137 takes every in-flight request with it).
    pub max_memory_mb: Option<u64>,
    /// docling-serve's API key (#615): when set, every `/v1` and `/v1alpha`
    /// request must carry it as `X-Api-Key` (401 otherwise); health,
    /// readiness, metrics and the docs page stay open. `None` falls back to
    /// `DOCLING_SERVE_API_KEY`, upstream's variable — the way to configure it
    /// without putting the key on a command line.
    pub api_key: Option<String>,
    /// Drop the warm PDF/image pipeline — its ONNX sessions and, on a GPU,
    /// their device memory — after this many seconds without a PDF/image
    /// conversion (#842); the next one reloads it. `None` falls back to
    /// `DOCLING_RS_IDLE_UNLOAD_SECS`; `0` (or neither) keeps it for the life
    /// of the process.
    pub idle_unload_secs: Option<u64>,
    /// Serve the web UI — the playground page at `/` and its logo (#892).
    /// Off, `/` answers 404 and only the API, `/openapi.yaml` and the
    /// probes are served: a deployment that is called by machines only
    /// exposes no page. `None` falls back to `DOCLING_RS_ENABLE_UI`, then
    /// upstream docling-serve's `DOCLING_SERVE_ENABLE_UI` (`0`/`false`/`no`/
    /// `off` disable, anything else enables), then on — upstream's default
    /// is off, ours stays on, so an unset variable changes nothing.
    pub ui: Option<bool>,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            addr: "127.0.0.1:5001".into(),
            concurrency: 2,
            max_body_bytes: 256 * 1024 * 1024,
            warmup: false,
            allow_url_fetch: false,
            allow_local_images: false,
            strict: false,
            queue_size: 16,
            result_ttl_secs: 600,
            max_memory_mb: None,
            api_key: None,
            idle_unload_secs: None,
            ui: None,
        }
    }
}

/// Whether the web UI is served: [`ServeConfig::ui`], then
/// `DOCLING_RS_ENABLE_UI`, then `DOCLING_SERVE_ENABLE_UI`, then on (#892).
fn ui_enabled(cfg: &ServeConfig) -> bool {
    cfg.ui.unwrap_or_else(|| {
        ["DOCLING_RS_ENABLE_UI", "DOCLING_SERVE_ENABLE_UI"]
            .into_iter()
            .find(|k| docling_core::env::nonempty(k).is_some())
            .is_none_or(docling_core::env::flag)
    })
}

struct AppState {
    /// Warm ML pipeline (mutable ONNX sessions) — one PDF/image conversion at
    /// a time, but the models stay loaded across requests.
    pipeline: Mutex<Option<(PipelineFlags, Pipeline)>>,
    /// When the warm pipeline was last handed back by a request (or loaded):
    /// what the idle unloader (#842) measures against. Every holder of the
    /// pipeline lock goes through [`AppState::pipeline_slot`], whose guard
    /// stamps this on release.
    last_pipeline_use: Mutex<Instant>,
    /// Bounds total in-flight conversions (`Arc` so a permit can move into
    /// a streaming response's worker and outlive the handler).
    permits: Arc<Semaphore>,
    /// Async conversion jobs (#182), keyed by task id.
    jobs: Mutex<HashMap<String, Job>>,
    readiness: Mutex<Readiness>,
    /// The resolved memory ceiling (#263): the configured value, else the
    /// container's cgroup limit, else none. `0` disables.
    memory_ceiling_mb: Option<u64>,
    cfg: ServeConfig,
}

/// What `/ready` reports (#548). Without `--warmup` the models load lazily
/// on the first PDF/image request, so the server is ready at once and says
/// so (`models: lazy`); with it, readiness waits for the models — and stays
/// unready, with the reason, when they fail to load, instead of turning
/// ready over an empty pipeline.
#[derive(Clone, Debug, PartialEq)]
enum Readiness {
    Lazy,
    Warming,
    Warm,
    Failed(String),
}

/// The warm pipeline slot, locked (#842): [`Deref`](std::ops::Deref)s to the
/// `Option` and stamps [`AppState::last_pipeline_use`] when dropped, so the
/// idle clock starts when a request *finishes* with the pipeline, not when
/// it took it — a conversion that outlasts the idle window is not unloaded
/// behind its own back.
struct PipelineSlot<'a> {
    guard: MutexGuard<'a, Option<(PipelineFlags, Pipeline)>>,
    last_use: &'a Mutex<Instant>,
}

impl std::ops::Deref for PipelineSlot<'_> {
    type Target = Option<(PipelineFlags, Pipeline)>;
    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl std::ops::DerefMut for PipelineSlot<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl Drop for PipelineSlot<'_> {
    fn drop(&mut self) {
        *self.last_use.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
    }
}

impl AppState {
    /// Lock the warm pipeline slot. Recovers a poisoned lock: one crafted
    /// document that panics inside a conversion must not turn into a
    /// permanent outage of the endpoint — `warm_pipeline` rebuilds or
    /// validates whatever the slot holds.
    fn pipeline_slot(&self) -> PipelineSlot<'_> {
        PipelineSlot {
            guard: self
                .pipeline
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            last_use: &self.last_pipeline_use,
        }
    }

    /// The idle unloader's step (#842): drop the warm pipeline when it has
    /// sat unused for `idle` — skipped while a request holds it (that is
    /// not idle) and when the slot is already empty. Returns whether a
    /// pipeline was dropped. The sessions are released under the lock, so a
    /// request arriving meanwhile waits the sub-second drop and then
    /// reloads, rather than briefly doubling the models.
    fn unload_idle_pipeline(&self, idle: Duration) -> bool {
        let mut slot = match self.pipeline.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return false,
        };
        let last = *self
            .last_pipeline_use
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if slot.is_none() || last.elapsed() < idle {
            return false;
        }
        let started = Instant::now();
        drop(slot.take());
        drop(slot);
        trim_heap();
        let mut readiness = self.readiness.lock().unwrap_or_else(|p| p.into_inner());
        if *readiness == Readiness::Warm {
            *readiness = Readiness::Lazy;
        }
        eprintln!(
            "docling-serve: warm pipeline unloaded after {}s idle ({:.2}s); the next PDF/image \
             request reloads it",
            idle.as_secs(),
            started.elapsed().as_secs_f64()
        );
        true
    }

    /// Admission control (#263): `Some(refusal)` when the process RSS sits
    /// above the watermark of the memory ceiling — the request should get a
    /// 503 *now* rather than push the whole server into the kernel's OOM
    /// killer. In-flight conversions keep running; the server recovers as
    /// soon as memory is released (or, with the ONNX arena retaining it, as
    /// soon as `DOCLING_RS_NO_ARENA` deployments free theirs).
    fn overloaded(&self) -> Option<String> {
        let ceiling = self.memory_ceiling_mb.filter(|&c| c > 0)?;
        let rss = docling_core::env::rss_mb()?;
        let pct = docling_core::env::parse::<u64>("DOCLING_RS_MEMORY_WATERMARK_PCT")
            .filter(|p| (1..=100).contains(p))
            .unwrap_or(85);
        let watermark = ceiling * pct / 100;
        (rss >= watermark).then(|| {
            format!(
                "server memory is at {rss} MB of the {ceiling} MB ceiling \
                 (watermark {watermark} MB) — retry once in-flight conversions finish"
            )
        })
    }
}

/// Build the router (exposed separately from [`serve`] for tests).
pub fn router(cfg: ServeConfig) -> Router {
    // Ceiling resolution (#263): explicit flag > DOCLING_RS_MAX_MEMORY_MB >
    // the container's own cgroup limit > none. 0 anywhere disables.
    let memory_ceiling_mb = cfg
        .max_memory_mb
        .or_else(|| docling_core::env::parse::<u64>("DOCLING_RS_MAX_MEMORY_MB"))
        .or_else(docling_core::env::cgroup_memory_limit_mb);
    if let Some(c) = memory_ceiling_mb.filter(|&c| c > 0) {
        eprintln!("docling-serve: memory ceiling {c} MB (admission control, #263)");
    }
    let api_key: Option<Arc<str>> = cfg
        .api_key
        .clone()
        .filter(|k| !k.is_empty())
        .or_else(|| docling_core::env::nonempty("DOCLING_SERVE_API_KEY"))
        .map(Arc::from);
    let state = Arc::new(AppState {
        pipeline: Mutex::new(None),
        last_pipeline_use: Mutex::new(Instant::now()),
        permits: Arc::new(Semaphore::new(cfg.concurrency.max(1))),
        jobs: Mutex::new(HashMap::new()),
        readiness: Mutex::new(if cfg.warmup {
            Readiness::Warming
        } else {
            Readiness::Lazy
        }),
        memory_ceiling_mb,
        cfg: cfg.clone(),
    });
    if cfg.warmup {
        let st = state.clone();
        // Blocking model load off the runtime; readiness flips when done.
        tokio::task::spawn_blocking(move || {
            let started = std::time::Instant::now();
            // Hold the slot while loading: a request that arrives before
            // `/ready` (a client not behind the probe) waits for these models
            // instead of loading a second copy of its own.
            let mut slot = st.pipeline_slot();
            // `Pipeline::new` only configures — the models load lazily — so
            // warm up explicitly, the serial worker *and* the page-worker
            // pool a multi-page PDF fans out to (#548). A panic in a model
            // load must not leave `/ready` warming forever.
            let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut p = Pipeline::new().map_err(|e| e.to_string())?;
                p.warm_up_all().map_err(|e| e.to_string())?;
                Ok::<_, String>(p)
            }));
            let next = match loaded {
                Ok(Ok(p)) => {
                    *slot = Some((PipelineFlags::default(), p));
                    eprintln!(
                        "docling-serve: warmup done in {:.1}s",
                        started.elapsed().as_secs_f64()
                    );
                    Readiness::Warm
                }
                Ok(Err(e)) => Readiness::Failed(e),
                Err(_) => Readiness::Failed("model load panicked".into()),
            };
            drop(slot);
            if let Readiness::Failed(e) = &next {
                eprintln!("docling-serve: warmup failed, /ready stays 503: {e}");
            }
            *st.readiness.lock().unwrap_or_else(|p| p.into_inner()) = next;
        });
    }
    // The idle unloader (#842): flag > DOCLING_RS_IDLE_UNLOAD_SECS > never.
    // Polls at a fraction of the window off the runtime; the drop itself —
    // ~10 ONNX sessions, on a GPU their arenas — runs on a blocking thread.
    let idle_unload = cfg
        .idle_unload_secs
        .or_else(|| docling_core::env::parse::<u64>("DOCLING_RS_IDLE_UNLOAD_SECS"))
        .filter(|&s| s > 0)
        .map(Duration::from_secs);
    if let Some(idle) = idle_unload {
        eprintln!(
            "docling-serve: the warm pipeline unloads after {}s without a PDF/image conversion (#842)",
            idle.as_secs()
        );
        let st = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(idle_unload_period(idle));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let st = st.clone();
                // A failed join (a panic in the drop) must not stop the clock.
                let _ = tokio::task::spawn_blocking(move || st.unload_idle_pipeline(idle)).await;
            }
        });
    }
    let router = if ui_enabled(&cfg) {
        Router::new()
            // Docs + test form, like the original docling-serve's playground.
            .route(
                "/",
                get(|| async { axum::response::Html(include_str!("index.html")) }),
            )
            // The page's logo. Baked into the binary, like the page and the
            // API description, so a server needs no static-file directory.
            .route(
                "/logo.svg",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "image/svg+xml")],
                        include_str!("logo.svg"),
                    )
                }),
            )
    } else {
        // #892: no page — `/` says where the API is instead of a bare 404,
        // and the logo is gone with the page.
        eprintln!("docling-serve: web UI disabled; serving the API only");
        Router::new().route(
            "/",
            get(|| async {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({
                        "detail": "the web UI is disabled on this server; the API is \
                                   POST /v1/convert, described at /openapi.yaml"
                    })),
                )
            }),
        )
    };
    router
        // The machine-readable API description — part of the API, kept with
        // or without the page.
        .route(
            "/openapi.yaml",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/yaml")],
                    include_str!("openapi.yaml"),
                )
            }),
        )
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/ready", get(ready))
        .route("/metrics", get(o11y::metrics_endpoint))
        .route("/v1/config", get(config))
        .route("/v1/convert", post(convert))
        .route("/v1/convert/async", post(convert_async))
        .route("/v1/status/{id}", get(job_status))
        .route("/v1/result/{id}", get(job_result))
        // #615: upstream docling-serve's API — what Open WebUI, n8n, Dify
        // and LangChain's docling-serve client call (`compat.rs`).
        .route("/v1/convert/file", post(compat::convert_file))
        .route("/v1/convert/source", post(compat::convert_source))
        .route("/v1/convert/file/async", post(compat::convert_file_async))
        .route(
            "/v1/convert/source/async",
            post(compat::convert_source_async),
        )
        .route("/v1/status/poll/{id}", get(compat::status_poll))
        .route("/v1alpha/convert/file", post(compat::convert_file))
        .route("/v1alpha/convert/source", post(compat::convert_source))
        .route("/v1alpha/status/poll/{id}", get(compat::status_poll))
        .route("/v1alpha/result/{id}", get(job_result))
        .layer(DefaultBodyLimit::max(cfg.max_body_bytes))
        .layer(axum::middleware::from_fn_with_state(
            api_key,
            compat::require_api_key,
        ))
        // Request span/log + metrics (#297) — outermost, so it times the whole
        // request including body-limit rejections.
        .layer(axum::middleware::from_fn(o11y::track))
        .with_state(state)
}

/// Bind and serve until SIGINT/SIGTERM; in-flight requests finish (graceful
/// shutdown).
pub async fn serve(cfg: ServeConfig) -> Result<(), String> {
    // Logging/tracing first (#297): startup diagnostics below already flow
    // through a configured subscriber this way.
    o11y::init();
    // Large buffers mapped and returned rather than carved from the
    // per-thread arenas (#669): without it RSS crept 528 -> 754 MB over six
    // PDF requests despite the trim, with it it held at ~590 MB, at the same
    // latency. See [`docling::memory`].
    docling::memory::pin_mmap_threshold();
    let addr = cfg.addr.clone();
    let app = router(cfg);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("cannot bind {addr}: {e}"))?;
    eprintln!("docling-serve listening on http://{addr}");
    // Log the resolved model set once at startup: when two deployments
    // convert differently, this is the first thing to compare (also served
    // live at /v1/config).
    for m in docling::model_inventory() {
        if m.found {
            eprintln!(
                "docling-serve: model {:<20} {} ({:.1} MB)",
                m.stage,
                m.path,
                m.bytes as f64 / 1_048_576.0
            );
        } else {
            eprintln!("docling-serve: model {:<20} {} (MISSING)", m.stage, m.path);
        }
    }
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| format!("server error: {e}"));
    // Flush batched OTLP spans (#297) — a no-op unless export is active.
    o11y::shutdown();
    result
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    eprintln!("docling-serve: shutdown signal received, draining in-flight requests");
}

/// Capabilities the built-in UI adapts to. Currently just whether `{"url": …}`
/// inputs are accepted (`--allow-url-fetch`) — the UI greys out the URL option
/// and explains why when this is false, instead of letting the user hit a 422.
async fn config(State(state): State<Arc<AppState>>) -> Response {
    // #889: Tesseract's installed languages, asked of the binary each time
    // (a `--list-langs` run, milliseconds; a mounted traineddata shows up
    // without a restart) — the playground's `ocr_lang` hints, which a fixed
    // list got wrong on an image with three languages.
    let tesseract_languages = tokio::task::spawn_blocking(|| docling::tesseract_languages().ok())
        .await
        .ok()
        .flatten();
    Json(json!({
        "allow_url_fetch": state.cfg.allow_url_fetch,
        // #646: whether `image_sources=local` is accepted.
        "allow_local_images": state.cfg.allow_local_images,
        // #263: the resolved memory ceiling (0/absent = none) and live RSS —
        // what admission control compares.
        "max_memory_mb": state.memory_ceiling_mb,
        "rss_mb": docling_core::env::rss_mb(),
        // Which model file each pipeline stage would load right now (resolved
        // per request — CWD-relative with env overrides, so this is the truth,
        // not a startup snapshot). "Two servers convert the same PDF
        // differently" is almost always this list differing; the size column
        // distinguishes an int8 quant from an fp32 graph at a glance.
        "models": docling::model_inventory().into_iter().map(|m| json!({
            "stage": m.stage,
            "path": m.path,
            "found": m.found,
            "bytes": m.bytes,
        })).collect::<Vec<_>>(),
        "tesseract_languages": tesseract_languages,
    }))
    .into_response()
}

async fn ready(State(state): State<Arc<AppState>>) -> Response {
    let readiness = state
        .readiness
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    match readiness {
        Readiness::Lazy => Json(json!({"status": "ready", "models": "lazy"})).into_response(),
        Readiness::Warm => Json(json!({"status": "ready", "models": "warm"})).into_response(),
        Readiness::Warming => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "warming_up"})),
        )
            .into_response(),
        Readiness::Failed(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "warmup_failed", "error": error})),
        )
            .into_response(),
    }
}

/// Request options, merged from query parameters and body fields.
/// One request's options: the conversion options every surface shares
/// ([`docling::ConvertOptions`], #577 — flattened, so its field names are the
/// wire keys, with `md_page_break_placeholder`, this server's historical
/// spelling, accepted as the alias of `page_break_placeholder`) plus what
/// this surface decides about the output. Three sources feed it — the query
/// string, a JSON body, multipart text parts — through one generic path:
/// each is a set of `(name, text)` pairs read by [`Self::set_text`], layered
/// by [`Self::merge_over`]; neither names a field, so an option added to the
/// shared struct is a request option here with no edit.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ConvertOptions {
    #[serde(flatten)]
    convert: docling::ConvertOptions,
    to: Option<String>,
    images: Option<String>,
    /// `to=pandoc` (#515): the Pandoc API version the client needs; anything
    /// but the one written (1.23) is rejected.
    pandoc_api_version: Option<String>,
    /// `to=pandoc` (#848): the `Header` depth of a section heading —
    /// `docling` (default, the title-reserving `n + 1`) or `source`.
    pandoc_heading_depth: Option<String>,
    /// `to=images` render scale in pixels per PDF point (#243): default 2.0
    /// (144 dpi, the pipeline's own render scale), accepted range 0.1–4.0.
    scale: Option<f32>,
    /// Which chunker `to=chunks` returns (#256, docling's `ChunkerType`):
    /// `hierarchical` | `hybrid`; unset keeps the legacy both-chunkers shape
    /// (hierarchical always, hybrid best-effort).
    chunker: Option<String>,
    /// Hybrid tokenizer for `to=chunks` (#256): a server-local *relative*
    /// path to a HuggingFace `tokenizer.json` (absolute paths and `..` are
    /// rejected — requests select among tokenizers the operator deployed,
    /// they don't read arbitrary server files). Default:
    /// `DOCLING_CHUNK_TOKENIZER`, else `.models/chunk/tokenizer.json`.
    chunk_tokenizer: Option<String>,
    /// Hybrid chunk budget for `to=chunks` (#256, docling's `max_tokens`);
    /// default `DOCLING_CHUNK_MAX_TOKENS`, else 256.
    chunk_max_tokens: Option<usize>,
    /// Hybrid peer-merging for `to=chunks` (docling's `merge_peers`, default
    /// true, #256).
    chunk_merge_peers: Option<bool>,
}

impl ConvertOptions {
    /// `self` on top of `base`: every option `self` sets wins, the rest
    /// keep `base`'s — [`docling::merge_options`], no field list.
    fn merge_over(self, base: ConvertOptions) -> ConvertOptions {
        docling::merge_options(self, base)
    }

    /// Set one option from its text form — a query parameter or a multipart
    /// text part. The text is tried as the option's type in turn
    /// ([`docling::ConvertOptions::text_candidates`]: a string, an integer,
    /// a decimal number, then a boolean — `1`/`true`/`yes`/`on` and
    /// `0`/`false`/`no`/`off`); the first reading the option accepts
    /// lands. An unknown name is ignored (as unknown query parameters always
    /// were); a value no reading accepts — `document_timeout=soon`,
    /// `strict=maybe` — is a 400 naming the option.
    fn set_text(&mut self, name: &str, text: &str) -> Result<(), ApiError> {
        for value in docling::ConvertOptions::text_candidates(text) {
            let mut patch = serde_json::Map::new();
            patch.insert(name.to_string(), value);
            if let Ok(parsed) =
                serde_json::from_value::<ConvertOptions>(serde_json::Value::Object(patch))
            {
                *self = parsed.merge_over(std::mem::take(self));
                return Ok(());
            }
        }
        Err(ApiError::Bad(format!(
            "{name}: cannot read {text:?} as that option's value"
        )))
    }

    /// The query string's options (`?strict=true&pages=2-5`).
    fn from_query(query: HashMap<String, String>) -> Result<ConvertOptions, ApiError> {
        let mut options = ConvertOptions::default();
        for (name, text) in &query {
            options.set_text(name, text)?;
        }
        Ok(options)
    }
}

/// A rejected shared option is this API's 400, with the library's message
/// (which names the option in its wire spelling).
fn bad(e: docling::OptionsError) -> ApiError {
    ApiError::Bad(e.to_string())
}

/// JSON body: the legacy `{"url": …}` shorthand, or docling's
/// service-datamodel `sources`/`target` shape (#139) — exactly one of `url`
/// and `sources` must be present.
#[derive(Deserialize)]
struct UrlRequest {
    url: Option<String>,
    /// Overrides the name (and thus format-selecting extension) taken from
    /// the URL path's last segment (`url` shorthand only).
    file_name: Option<String>,
    /// docling `kind`-tagged source items: `file`, `http`, `s3`,
    /// `azure_blob`, `google_cloud_storage` (#139).
    sources: Option<Vec<passthrough::SourceSpec>>,
    /// docling `kind`-tagged output target; default `inbody` (the response
    /// body, exactly as without a target).
    target: Option<passthrough::TargetSpec>,
    #[serde(flatten)]
    options: ConvertOptions,
}

#[derive(Debug)]
enum ApiError {
    Bad(String),
    Unsupported(String),
    /// The document is encrypted and the request's password (or none) does
    /// not open it (#636): a 422 like any document problem, but the body
    /// carries the typed case as `code` (`password_required`,
    /// `wrong_password`, `not_decryptable`, `malformed_encryption`) so a
    /// client prompts for a password instead of matching the message.
    Encrypted {
        code: &'static str,
        message: String,
    },
    Internal(String),
    /// The async job queue is full (#182) — retry later.
    Busy(String),
    /// The memory ceiling's watermark is crossed (#263) — 503, retry later.
    Overloaded(String),
    /// The request body is over `--max-body-mb` (#619) — 413.
    TooLarge(String),
}

/// A multipart read error: 413 when the body went past `--max-body-mb`
/// (axum's `DefaultBodyLimit` surfaces it here, inside the extractor we drive
/// ourselves), 400 for a malformed body.
fn multipart_error(what: &str, e: axum::extract::multipart::MultipartError) -> ApiError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::TooLarge(format!("{what}: {e} (the server's --max-body-mb)"))
    } else {
        ApiError::Bad(format!("{what}: {e}"))
    }
}

/// A body read error (`axum::body::to_bytes` under the server's limit):
/// 413 when the limit cut it off, 400 otherwise. The limit error is
/// `http_body_util::LengthLimitError` somewhere in the source chain; it is
/// recognized by its message so the crate needs no direct dependency.
fn body_error(what: &str, e: axum::Error) -> ApiError {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&e);
    while let Some(c) = cause {
        if c.to_string().contains("length limit exceeded") {
            return ApiError::TooLarge(format!("{what}: {e} (the server's --max-body-mb)"));
        }
        cause = c.source();
    }
    ApiError::Bad(format!("{what}: {e}"))
}

/// The HTTP status + message an [`ApiError`] answers with (also stored on a
/// failed async job so `/v1/result/{id}` reproduces the sync status).
fn api_error_parts(e: ApiError) -> (StatusCode, String) {
    match e {
        ApiError::Bad(m) => (StatusCode::BAD_REQUEST, m),
        ApiError::Unsupported(m) | ApiError::Encrypted { message: m, .. } => {
            (StatusCode::UNPROCESSABLE_ENTITY, m)
        }
        ApiError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        ApiError::Busy(m) => (StatusCode::TOO_MANY_REQUESTS, m),
        ApiError::Overloaded(m) => (StatusCode::SERVICE_UNAVAILABLE, m),
        ApiError::TooLarge(m) => (StatusCode::PAYLOAD_TOO_LARGE, m),
    }
}

/// The machine-readable `code` an error body carries besides `error` —
/// today only an encrypted document's case (#636).
fn api_error_code(e: &ApiError) -> Option<&'static str> {
    match e {
        ApiError::Encrypted { code, .. } => Some(code),
        _ => None,
    }
}

/// `{"error": msg}` plus `"code"` when the error has one.
fn error_body(msg: &str, code: Option<&'static str>) -> Json<serde_json::Value> {
    let mut body = json!({"error": msg});
    if let Some(code) = code {
        body["code"] = json!(code);
    }
    Json(body)
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let overloaded = matches!(self, ApiError::Overloaded(_));
        let code = api_error_code(&self);
        let (status, msg) = api_error_parts(self);
        let mut response = (status, error_body(&msg, code)).into_response();
        if overloaded {
            // A hint for well-behaved clients; conversions run seconds, not ms.
            response
                .headers_mut()
                .insert("retry-after", header::HeaderValue::from_static("5"));
        }
        response
    }
}

/// One parsed upload: the source, or the filename with why it couldn't become
/// one (a batch converts around a bad item; a single-file request propagates
/// the error as its response).
type SourceItem = Result<SourceDocument, (String, ApiError)>;

/// Parse a conversion request body — `multipart/form-data` uploads (one or
/// more `file` parts, #182 batch) or an `application/json` body (`{"url": …}`
/// or docling's `sources`/`target` shape, #139) — into sources, the merged
/// options and the optional cloud output target. Shared by the sync and
/// async endpoints so both accept exactly the same requests (and reject bad
/// ones synchronously).
/// A parsed non-`inbody` output target: where rendered outputs go instead of
/// (or, for `Zip`, packaged inside) the response body.
enum ResolvedTarget {
    /// Cloud object store (#139: `s3` / `azure_blob` / `google_cloud_storage`).
    Cloud(passthrough::CloudCoords),
    /// One zip archive of the rendered outputs in the response body (#303).
    Zip,
    /// HTTP PUT each rendered output to this URL (#303).
    Put(String),
}

async fn parse_convert_request(
    state: &Arc<AppState>,
    query: ConvertOptions,
    headers: HeaderMap,
    body: axum::extract::Request,
) -> Result<(Vec<SourceItem>, ConvertOptions, Option<ResolvedTarget>), ApiError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    if content_type.starts_with("multipart/form-data") {
        let multipart = Multipart::from_request(body, &())
            .await
            .map_err(|e| ApiError::Bad(format!("bad multipart body: {e}")))?;
        let (sources, options) = read_multipart(multipart, query).await?;
        Ok((sources, options, None))
    } else if content_type.starts_with("application/json") {
        let bytes = axum::body::to_bytes(body.into_body(), state.cfg.max_body_bytes)
            .await
            .map_err(|e| body_error("bad body", e))?;
        let req: UrlRequest = serde_json::from_slice(&bytes)
            .map_err(|e| ApiError::Bad(format!("bad JSON body: {e}")))?;
        let options = req.options.clone().merge_over(query);
        let target = match req.target {
            None | Some(passthrough::TargetSpec::Inbody) => None,
            // #303: a zip archive answers in the response body — a download
            // shape like to=dclx, nothing outbound, so no gate applies.
            Some(passthrough::TargetSpec::Zip) => Some(ResolvedTarget::Zip),
            // #303: PUT pushes outputs wherever the caller's URL says — the
            // same outbound/SSRF surface as URL inputs, same gate (the URL's
            // resolution check runs on the blocking pool, before any upload).
            Some(passthrough::TargetSpec::Put(p)) => {
                require_outbound(state, "put targets")?;
                Some(ResolvedTarget::Put(p.url))
            }
            Some(spec) => {
                require_outbound(state, "cloud targets")?;
                Some(ResolvedTarget::Cloud(match spec {
                    passthrough::TargetSpec::S3(c) => passthrough::CloudCoords::S3(c),
                    passthrough::TargetSpec::AzureBlob(c) => passthrough::CloudCoords::Azure(c),
                    passthrough::TargetSpec::GoogleCloudStorage(c) => {
                        passthrough::CloudCoords::Gcs(c)
                    }
                    passthrough::TargetSpec::Inbody
                    | passthrough::TargetSpec::Zip
                    | passthrough::TargetSpec::Put(_) => unreachable!("matched above"),
                }))
            }
        };
        let sources = match (req.url, req.sources) {
            (Some(url), None) => {
                require_outbound(state, "URL inputs")?;
                let name = req.file_name.clone();
                let source =
                    tokio::task::spawn_blocking(move || fetch_url(&url, name.as_deref(), &[]))
                        .await
                        .map_err(|e| ApiError::Internal(format!("fetch task: {e}")))??;
                vec![Ok(source)]
            }
            (None, Some(specs)) => parse_source_specs(state, specs).await?,
            (Some(_), Some(_)) => {
                return Err(ApiError::Bad(
                    "pass either \"url\" or \"sources\", not both".into(),
                ))
            }
            (None, None) => {
                return Err(ApiError::Bad(
                    "JSON body needs \"url\" or a non-empty \"sources\" array".into(),
                ))
            }
        };
        Ok((sources, options, target))
    } else {
        Err(ApiError::Bad(
            "expected multipart/form-data (file upload) or application/json ({\"url\": …})".into(),
        ))
    }
}

/// Outbound access (URL fetch, cloud stores) is one opt-in: the server-wide
/// `--allow-url-fetch` flag (SSRF surface — docs/SECURITY.md).
fn require_outbound(state: &Arc<AppState>, what: &str) -> Result<(), ApiError> {
    if state.cfg.allow_url_fetch {
        Ok(())
    } else {
        Err(ApiError::Unsupported(format!(
            "{what} are disabled; start docling-serve with --allow-url-fetch \
             (SSRF surface — see docs/SECURITY.md), or upload the file instead"
        )))
    }
}

/// Materialize docling-shaped `sources[]` items (#139) into [`SourceItem`]s.
/// A bad `file` item fails only itself (batch semantics, like a bad upload);
/// an unreachable store or URL fails the request — there is nothing partial
/// to convert.
async fn parse_source_specs(
    state: &Arc<AppState>,
    specs: Vec<passthrough::SourceSpec>,
) -> Result<Vec<SourceItem>, ApiError> {
    if specs.is_empty() {
        return Err(ApiError::Bad("\"sources\" must not be empty".into()));
    }
    let mut items: Vec<SourceItem> = Vec::new();
    for spec in specs {
        match spec {
            passthrough::SourceSpec::File {
                base64_string,
                filename,
            } => match docling::base64::decode(base64_string.trim()) {
                Some(bytes) => items.extend(sources_from_named_bytes(&filename, bytes)),
                None => items.push(Err((
                    filename.clone(),
                    ApiError::Bad(format!("file source {filename:?}: bad base64")),
                ))),
            },
            passthrough::SourceSpec::Http { url, headers } => {
                require_outbound(state, "URL inputs")?;
                let header_vec: Vec<(String, String)> = headers.into_iter().collect();
                let source =
                    tokio::task::spawn_blocking(move || fetch_url(&url, None, &header_vec))
                        .await
                        .map_err(|e| ApiError::Internal(format!("fetch task: {e}")))??;
                items.push(Ok(source));
            }
            spec @ (passthrough::SourceSpec::S3(_)
            | passthrough::SourceSpec::AzureBlob(_)
            | passthrough::SourceSpec::GoogleCloudStorage(_)) => {
                require_outbound(state, "cloud sources")?;
                let coords = match spec {
                    passthrough::SourceSpec::S3(c) => passthrough::CloudCoords::S3(c),
                    passthrough::SourceSpec::AzureBlob(c) => passthrough::CloudCoords::Azure(c),
                    passthrough::SourceSpec::GoogleCloudStorage(c) => {
                        passthrough::CloudCoords::Gcs(c)
                    }
                    _ => unreachable!("matched above"),
                };
                let fetched = passthrough::fetch_sources(&coords)
                    .await
                    .map_err(ApiError::Bad)?;
                for (name, bytes) in fetched {
                    items.extend(sources_from_named_bytes(&name, bytes));
                }
            }
        }
    }
    Ok(items)
}

/// Validate the `to` / `images` options (shared by sync and async so an async
/// submission fails fast instead of parking a doomed job in the queue).
fn validate_output(options: &ConvertOptions) -> Result<(String, ImageMode), ApiError> {
    let to = options.to.clone().unwrap_or_else(|| "md".into());
    // The CLI's `--to` list (#603: one source, `docling::OUTPUT_FORMATS`),
    // plus the `markdown` alias of `md`.
    if to != "markdown" && !docling::OUTPUT_FORMATS.contains(&to.as_str()) {
        return Err(ApiError::Bad(format!(
            "unknown to='{to}' (expected: {})",
            docling::OUTPUT_FORMATS.join(", ")
        )));
    }
    if let Some(v) = &options.pandoc_api_version {
        docling::pandoc::check_api_version(v)
            .map_err(|e| ApiError::Bad(format!("pandoc_api_version: {e}")))?;
    }
    pandoc_heading_depth(options)?;
    // #537: `to=pandoc` embeds its pictures unless `images` says otherwise —
    // the AST is fed to `pandoc -t docx`, whose writers drop a target-less
    // picture.
    let default_images = if to == "pandoc" {
        "embedded"
    } else {
        "placeholder"
    };
    let image_mode = match options.images.as_deref().unwrap_or(default_images) {
        "placeholder" => ImageMode::Placeholder,
        "embedded" => ImageMode::Embedded,
        other => {
            return Err(ApiError::Bad(format!(
                "unknown images='{other}' (expected: placeholder, embedded)"
            )))
        }
    };
    // The shared options validate here too — before the conversion starts —
    // because the streaming path flattens later errors into a mid-stream 422,
    // and a bad option deserves a plain 400 up front.
    options.convert.validate().map_err(bad)?;
    parse_chunk_options(options)?;
    Ok((to, image_mode))
}

/// Validate and build the per-request chunking configuration (#256). The
/// tokenizer must be a server-local *relative* path with no `..` components:
/// requests pick among tokenizers the operator deployed next to the server,
/// they don't read arbitrary files (the unrestricted knob is the operator-set
/// `DOCLING_CHUNK_TOKENIZER` env).
fn parse_chunk_options(
    options: &ConvertOptions,
) -> Result<docling::chunks::ChunkOptions, ApiError> {
    let chunker = options
        .chunker
        .as_deref()
        .map(docling::chunks::ChunkerKind::parse)
        .transpose()
        .map_err(ApiError::Bad)?;
    if let Some(p) = options.chunk_tokenizer.as_deref() {
        let path = std::path::Path::new(p);
        let unsafe_component = path.components().any(|c| {
            !matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        });
        if unsafe_component || p.is_empty() {
            return Err(ApiError::Bad(format!(
                "chunk_tokenizer must be a relative path without '..' components, got {p:?}"
            )));
        }
    }
    Ok(docling::chunks::ChunkOptions {
        chunker,
        tokenizer: options.chunk_tokenizer.clone(),
        max_tokens: options.chunk_max_tokens,
        merge_peers: options.chunk_merge_peers,
    })
}

async fn convert(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: axum::extract::Request,
) -> Result<Response, ApiError> {
    let query = ConvertOptions::from_query(query)?;
    let (sources, options, target) = parse_convert_request(&state, query, headers, body).await?;
    let (to, image_mode) = validate_output(&options)?;
    // #304: bad pipeline/vlm options are a 400/422 up front (no DNS here —
    // the endpoint's SSRF resolution check runs on the blocking pool).
    resolve_vlm_options(&state, &options, false)?;
    check_picture_description(&state, &options, false)?;
    validate_target(&to, target.as_ref())?;
    // Admission control (#263): shed load before taking a conversion slot.
    if let Some(msg) = state.overloaded() {
        return Err(ApiError::Overloaded(msg));
    }

    // Bound total in-flight conversions; excess requests queue here. The
    // permit is owned so the streaming path can hold it until the response
    // body finishes, not just until the handler returns.
    let permit = state
        .permits
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| ApiError::Internal(format!("semaphore: {e}")))?;

    // A Markdown conversion streams — a batch too, document after document
    // with the separator between them; everything else (other formats and
    // cloud targets — whose outputs leave through the store, #139) buffers.
    let is_markdown = matches!(to.as_str(), "md" | "markdown");
    if is_markdown && target.is_none() {
        return stream_markdown(state.clone(), sources, options, image_mode, permit).await;
    }

    let st = state.clone();
    let rt = tokio::runtime::Handle::current();
    let stored = tokio::task::spawn_blocking(move || {
        let out = run_conversion(
            &st,
            sources,
            &options,
            &to,
            image_mode,
            target.as_ref(),
            &rt,
        );
        trim_heap();
        out
    })
    .await
    .map_err(|e| ApiError::Internal(format!("convert task: {e}")))?;
    drop(permit);
    Ok(stored?.into_response())
}

/// A cloud target combines with every buffered output format except
/// `to=images` (page PNGs are a debugging aid, not a pipeline artifact).
fn validate_target(to: &str, target: Option<&ResolvedTarget>) -> Result<(), ApiError> {
    if target.is_some() && to == "images" {
        return Err(ApiError::Bad(
            "to=images does not combine with an output target".into(),
        ));
    }
    Ok(())
}

/// One async conversion job (#182).
struct Job {
    state: JobState,
    /// When the job left the queue (finished or failed) — drives TTL eviction.
    done_at: Option<std::time::Instant>,
    /// The job's conversion progress (#783), written by the converting
    /// thread, read by `GET /v1/status/{id}`.
    progress: Arc<Mutex<JobProgress>>,
}

/// Where an async job's conversion stands (#783): the document it is on and
/// its pages / enrichment counts, from the converter's progress events. A
/// finished job keeps its last counts.
#[derive(Clone, Debug, Default, Serialize)]
struct JobProgress {
    /// Documents the job has started on, of `documents_total` (a batch
    /// request converts several).
    documents_started: usize,
    documents_total: usize,
    /// The document being converted: its 1-based index in the request and
    /// its name.
    document_index: usize,
    document_name: String,
    /// Pages finished of the pages selected — the PDF / image pipeline's.
    completed_pages: usize,
    total_pages: usize,
    /// The enrichment pass under way (the picture passes), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    enrichment: Option<EnrichmentProgress>,
}

#[derive(Clone, Debug, Serialize)]
struct EnrichmentProgress {
    step: &'static str,
    label: &'static str,
    completed_items: usize,
    total_items: usize,
}

impl JobProgress {
    /// Fold one progress event in. A job converts its documents one after
    /// another, each reported as document 1 by the conversion functions it
    /// shares with the sync endpoints, so the position in the request is
    /// counted here.
    fn record(&mut self, event: &docling::ProgressEvent) {
        use docling::ProgressKind;
        match &event.kind {
            ProgressKind::DocumentStarted => {
                self.documents_started += 1;
                self.document_index = self.documents_started;
                self.document_name = event.document_name.clone();
                self.completed_pages = 0;
                self.total_pages = 0;
                self.enrichment = None;
            }
            ProgressKind::PageCompleted {
                completed_pages,
                total_pages,
                ..
            } => {
                self.completed_pages = *completed_pages;
                self.total_pages = *total_pages;
            }
            ProgressKind::EnrichmentProgress {
                step,
                label,
                completed_items,
                total_items,
            } => {
                self.enrichment = Some(EnrichmentProgress {
                    step: step.name(),
                    label,
                    completed_items: *completed_items,
                    total_items: *total_items,
                });
            }
            _ => {}
        }
    }
}

thread_local! {
    /// The progress slot of the async job running on this blocking thread
    /// (#783), set by `submit_job` around the job's work: the conversion
    /// functions are shared with the sync endpoints, where there is no
    /// job, so the job reaches them through the thread rather than every
    /// signature between.
    static JOB_PROGRESS: std::cell::RefCell<Option<Arc<Mutex<JobProgress>>>> =
        const { std::cell::RefCell::new(None) };
}

/// Sets this thread's job progress slot for a job's work and clears it on
/// drop — also when the work panics, so a pooled thread never carries a
/// finished job's slot into the next request it serves.
struct JobProgressScope;

impl JobProgressScope {
    fn enter(progress: Arc<Mutex<JobProgress>>) -> Self {
        JOB_PROGRESS.with(|s| *s.borrow_mut() = Some(progress));
        Self
    }
}

impl Drop for JobProgressScope {
    fn drop(&mut self) {
        JOB_PROGRESS.with(|s| *s.borrow_mut() = None);
    }
}

/// This thread's job progress slot as a progress callback, `None` on a
/// sync request.
fn job_progress_callback() -> Option<docling::ProgressCallback> {
    let slot = JOB_PROGRESS.with(|s| s.borrow().clone())?;
    Some(Arc::new(move |event: &docling::ProgressEvent| {
        slot.lock().unwrap_or_else(|p| p.into_inner()).record(event);
    }))
}

enum JobState {
    /// Waiting for a conversion slot (the shared semaphore).
    Pending,
    Started,
    Success(StoredResponse),
    /// The HTTP status the sync endpoint would have answered with, the
    /// error message, and its `code` when it has one (#636).
    Failure(StatusCode, String, Option<&'static str>),
}

impl JobState {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Started => "started",
            Self::Success(_) => "success",
            Self::Failure(..) => "failure",
        }
    }
}

/// Evict finished jobs whose result has outlived the TTL. Called from the job
/// endpoints — no background sweeper thread needed, since memory only ever
/// accumulates through those same endpoints' submissions.
fn purge_expired(jobs: &mut HashMap<String, Job>, ttl_secs: u64) {
    let ttl = std::time::Duration::from_secs(ttl_secs);
    jobs.retain(|_, job| job.done_at.is_none_or(|done| done.elapsed() < ttl));
}

/// An unguessable task id. The result endpoint is unauthenticated (like the
/// rest of the API), so the id doubles as the fetch capability: 128 bits from
/// two independently random-seeded `RandomState` hashers — not a substitute
/// for real authentication (front the server with one), but not enumerable
/// either.
fn task_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h1 = std::collections::hash_map::RandomState::new().build_hasher();
    let mut h2 = std::collections::hash_map::RandomState::new().build_hasher();
    h1.write_u64(0);
    h2.write_u64(1);
    format!("{:016x}{:016x}", h1.finish(), h2.finish())
}

/// `POST /v1/convert/async` (#182): accept the same request as `/v1/convert`,
/// but return a task id immediately instead of holding the connection for the
/// duration of the conversion. The job queues on the same semaphore as sync
/// requests (reusing the warm pipeline); poll `GET /v1/status/{id}` and fetch
/// `GET /v1/result/{id}`.
async fn convert_async(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: axum::extract::Request,
) -> Result<Response, ApiError> {
    let query = ConvertOptions::from_query(query)?;
    let (mut sources, options, target) =
        parse_convert_request(&state, query, headers, body).await?;
    let (to, image_mode) = validate_output(&options)?;
    // #304: fail an async submission fast instead of parking a doomed job.
    resolve_vlm_options(&state, &options, false)?;
    check_picture_description(&state, &options, false)?;
    validate_target(&to, target.as_ref())?;
    // Admission control (#263) applies to async submissions too — a queued
    // job holds its upload bytes and will run into the same ceiling.
    if let Some(msg) = state.overloaded() {
        return Err(ApiError::Overloaded(msg));
    }
    // A single unconvertible upload fails the submission itself (a batch
    // converts around bad items) — same surface as the sync endpoint, and no
    // doomed job occupies the queue.
    if sources.len() == 1 && sources[0].is_err() {
        let (_, e) = sources.remove(0).expect_err("checked is_err");
        return Err(e);
    }

    let documents_total = sources.len();
    let id = submit_job(&state, documents_total, move |st, rt| {
        run_conversion(st, sources, &options, &to, image_mode, target.as_ref(), rt)
    })?;

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "task_id": id, "task_status": "pending" })),
    )
        .into_response())
}

/// Queue `work` as an async job (#182): a task id now, the work later on
/// the shared conversion semaphore (so it reuses the warm pipeline and
/// counts against the same concurrency as sync requests), its outcome kept
/// for `/v1/result/{id}` until the TTL evicts it. Refused with 429 when the
/// job map is full. Shared by `/v1/convert/async` and the upstream-shaped
/// async routes (#615).
fn submit_job<F>(state: &Arc<AppState>, documents_total: usize, work: F) -> Result<String, ApiError>
where
    F: FnOnce(&AppState, &tokio::runtime::Handle) -> Result<StoredResponse, ApiError>
        + Send
        + 'static,
{
    let id = task_id();
    {
        let mut jobs = state.jobs.lock().unwrap();
        purge_expired(&mut jobs, state.cfg.result_ttl_secs);
        // The bound counts jobs still holding request/result memory — queued,
        // running, or finished-but-unfetched — so a burst can't grow the map
        // (and the upload bytes it holds) without limit.
        if jobs.len() >= state.cfg.queue_size {
            return Err(ApiError::Busy(format!(
                "job queue is full ({} jobs); retry after fetching or expiring results",
                jobs.len()
            )));
        }
        jobs.insert(
            id.clone(),
            Job {
                state: JobState::Pending,
                done_at: None,
                progress: Arc::new(Mutex::new(JobProgress {
                    documents_total,
                    ..JobProgress::default()
                })),
            },
        );
    }

    let st = state.clone();
    let job_id = id.clone();
    tokio::spawn(async move {
        // Queue on the shared conversion semaphore. Closed-semaphore errors
        // only happen at shutdown; the job then just stays pending until the
        // process exits.
        let Ok(permit) = st.permits.clone().acquire_owned().await else {
            return;
        };
        let progress = if let Some(job) = st.jobs.lock().unwrap().get_mut(&job_id) {
            job.state = JobState::Started;
            Arc::clone(&job.progress)
        } else {
            return; // evicted while queued (TTL abuse would need days)
        };
        let stx = st.clone();
        let rt = tokio::runtime::Handle::current();
        let outcome = tokio::task::spawn_blocking(move || {
            // The job's progress slot (#783) for the conversions `work`
            // runs on this thread, cleared when the scope ends.
            let scope = JobProgressScope::enter(progress);
            let out = work(&stx, &rt);
            drop(scope);
            trim_heap();
            out
        })
        .await
        .map_err(|e| ApiError::Internal(format!("convert task: {e}")))
        .and_then(|r| r);
        drop(permit);
        if let Some(job) = st.jobs.lock().unwrap().get_mut(&job_id) {
            job.state = match outcome {
                Ok(stored) => JobState::Success(stored),
                Err(e) => {
                    let code = api_error_code(&e);
                    let (status, msg) = api_error_parts(e);
                    JobState::Failure(status, msg, code)
                }
            };
            job.done_at = Some(std::time::Instant::now());
        }
    });
    Ok(id)
}

/// `GET /v1/status/{id}` (#182).
async fn job_status(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let mut jobs = state.jobs.lock().unwrap();
    purge_expired(&mut jobs, state.cfg.result_ttl_secs);
    match jobs.get(&id) {
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "unknown task id (never submitted, or its result expired)"})),
        )
            .into_response(),
        Some(job) => {
            let mut body = json!({ "task_id": id, "task_status": job.state.as_str() });
            // Where the conversion stands (#783), once it started.
            if !matches!(job.state, JobState::Pending) {
                let progress = job.progress.lock().unwrap_or_else(|p| p.into_inner());
                body["progress"] = json!(*progress);
            }
            if let JobState::Failure(_, msg, code) = &job.state {
                body["error"] = json!(msg);
                if let Some(code) = code {
                    body["code"] = json!(code);
                }
            }
            Json(body).into_response()
        }
    }
}

/// `GET /v1/result/{id}` (#182): the conversion output with the same content
/// type / headers the sync endpoint would have used. Not ready yet → 202 with
/// the status body; failed → the sync endpoint's error status; unknown or
/// expired → 404. The result stays fetchable until the TTL evicts it.
async fn job_result(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let mut jobs = state.jobs.lock().unwrap();
    purge_expired(&mut jobs, state.cfg.result_ttl_secs);
    match jobs.get(&id) {
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "unknown task id (never submitted, or its result expired)"})),
        )
            .into_response(),
        Some(job) => match &job.state {
            JobState::Pending | JobState::Started => (
                StatusCode::ACCEPTED,
                Json(json!({ "task_id": id, "task_status": job.state.as_str() })),
            )
                .into_response(),
            JobState::Failure(status, msg, code) => {
                (*status, error_body(msg, *code)).into_response()
            }
            // Clone rather than remove: the result stays re-fetchable until
            // the TTL evicts it (a client retrying a dropped response must not
            // find a 404). The survived problems too, so the result carries
            // the `X-Docling-Status` / `X-Docling-Errors` the sync response
            // would have.
            JobState::Success(stored) => StoredResponse {
                errors: stored.errors.clone(),
                content_type: stored.content_type,
                disposition: stored.disposition.clone(),
                confidence: stored.confidence.clone(),
                redaction: stored.redaction.clone(),
                pdf_repairs: stored.pdf_repairs.clone(),
                body: stored.body.clone(),
            }
            .into_response(),
        },
    }
}

/// A fully materialized conversion response — what a buffered sync request
/// answers with, and what an async job (#182) stores until the client fetches
/// `/v1/result/{id}`.
struct StoredResponse {
    content_type: &'static str,
    /// `Content-Disposition` for downloads (dclx).
    disposition: Option<String>,
    /// The `X-Docling-Confidence` summary (#183), when the pipeline made one.
    confidence: Option<header::HeaderValue>,
    /// The `X-Docling-Redaction` counts (#621), when the PII pass ran.
    redaction: Option<header::HeaderValue>,
    /// The `X-Docling-Pdf-Repairs` list (#934), when the request asked
    /// `pdf_report_repairs` and the input is a PDF.
    pdf_repairs: Option<header::HeaderValue>,
    body: Vec<u8>,
    /// The problems the conversion survived (#497) — a non-empty list is a
    /// `partial_success`, announced in `X-Docling-Status` / `X-Docling-Errors`
    /// since the body is the document itself.
    errors: Vec<docling::ErrorItem>,
}

impl StoredResponse {
    fn into_response(self) -> Response {
        let mut response = ([(header::CONTENT_TYPE, self.content_type)], self.body).into_response();
        if !self.errors.is_empty() {
            let converted = Converted {
                document: DoclingDocument::new(""),
                errors: self.errors,
                redaction: None,
                pdf_repairs: None,
            };
            response.headers_mut().insert(
                "x-docling-status",
                header::HeaderValue::from_static("partial_success"),
            );
            if let Ok(v) = header::HeaderValue::from_str(&converted.errors_json().to_string()) {
                response.headers_mut().insert("x-docling-errors", v);
            }
        }
        if let Some(d) = self.disposition {
            if let Ok(v) = header::HeaderValue::from_str(&d) {
                response
                    .headers_mut()
                    .insert(header::CONTENT_DISPOSITION, v);
            }
        }
        if let Some(v) = self.confidence {
            response.headers_mut().insert("x-docling-confidence", v);
        }
        if let Some(v) = self.redaction {
            response.headers_mut().insert("x-docling-redaction", v);
        }
        if let Some(v) = self.pdf_repairs {
            response.headers_mut().insert("x-docling-pdf-repairs", v);
        }
        response
    }
}

/// Convert one or more sources on the current (blocking) thread and serialize
/// the result. A single source renders as the plain output format; multiple
/// sources (#182 batch) render as that format too when it is Markdown or
/// plain text ([`text_batch`]), else as a JSON results array with per-item
/// status — either way one bad file fails its item, not the whole batch.
/// With a cloud
/// `target` (#139) every rendered output is uploaded instead and the
/// response is a `RemoteTargetResult` acknowledgment (uploads run on the
/// runtime `rt` — `Handle::block_on` is the sanctioned bridge from a
/// blocking worker).
#[allow(clippy::too_many_arguments)]
fn run_conversion(
    state: &AppState,
    sources: Vec<SourceItem>,
    options: &ConvertOptions,
    to: &str,
    image_mode: ImageMode,
    target: Option<&ResolvedTarget>,
    rt: &tokio::runtime::Handle,
) -> Result<StoredResponse, ApiError> {
    match target {
        Some(ResolvedTarget::Cloud(coords)) => {
            return run_conversion_to_target(state, sources, options, to, image_mode, coords, rt);
        }
        Some(ResolvedTarget::Zip) => {
            return run_conversion_to_zip(state, sources, options, to, image_mode);
        }
        Some(ResolvedTarget::Put(url)) => {
            return run_conversion_to_put(state, sources, options, to, image_mode, url);
        }
        None => {}
    }
    // A Markdown / text / HTML / LaTeX batch answers in that format — for
    // Markdown what the streaming handler sends, so an async job's result is
    // the sync response.
    if sources.len() > 1 && matches!(to, "md" | "markdown" | "text" | "html" | "latex") {
        return Ok(text_batch(state, sources, options, to, image_mode));
    }
    if sources.len() == 1 {
        let source = sources
            .into_iter()
            .next()
            .expect("checked len")
            .map_err(|(_, e)| e)?;
        if to == "images" {
            let pages = rasterize_pages(state, &source, options)?;
            return Ok(StoredResponse {
                errors: Vec::new(),
                content_type: "application/json",
                disposition: None,
                confidence: None,
                redaction: None,
                pdf_repairs: None,
                body: serde_json::to_vec_pretty(&json!({ "pages": pages }))
                    .expect("page JSON serializes"),
            });
        }
        let name = source.name.clone();
        let converted = convert_document(state, source, options)?;
        let mut stored = render_stored(state, to, image_mode, &name, &converted.document, options)?;
        stored.redaction = converted.redaction_header();
        stored.pdf_repairs = converted.pdf_repairs_header();
        stored.errors = converted.errors;
        return Ok(stored);
    }
    let items: Vec<serde_json::Value> = sources
        .into_iter()
        .map(|item| {
            let source = match item {
                Ok(source) => source,
                Err((name, e)) => {
                    return json!({
                        "name": name,
                        "status": "failure",
                        "error": api_error_message(e),
                    })
                }
            };
            let name = source.name.clone();
            if to == "images" {
                return match rasterize_pages(state, &source, options) {
                    Ok(pages) => json!({
                        "name": name,
                        "status": "success",
                        "pages": pages,
                    }),
                    Err(e) => json!({
                        "name": name,
                        "status": "failure",
                        "error": api_error_message(e),
                    }),
                };
            }
            match convert_document(state, source, options) {
                Ok(converted) => {
                    let mut item =
                        batch_item(state, to, image_mode, &name, &converted.document, options);
                    mark_partial(&mut item, &converted);
                    item
                }
                Err(e) => json!({
                    "name": name,
                    "status": "failure",
                    "error": api_error_message(e),
                }),
            }
        })
        .collect();
    Ok(StoredResponse {
        errors: Vec::new(),
        content_type: "application/json",
        disposition: None,
        confidence: None,
        redaction: None,
        pdf_repairs: None,
        body: serde_json::to_vec_pretty(&json!({ "results": items }))
            .expect("batch JSON serializes"),
    })
}

/// The `<stem>.<ext>` output naming shared by the cloud, zip and put targets
/// (#139/#303) — mirrors the CLI batch naming; a duplicate stem within one
/// request gets `-2`, `-3`, … instead of overwriting/shadowing.
struct OutputNames {
    ext: &'static str,
    used: std::collections::HashSet<String>,
}

impl OutputNames {
    fn new(to: &str) -> Self {
        let ext = match to {
            "md" | "markdown" => "md",
            "json" => "json",
            "html" => "html",
            "chunks" => "chunks.json",
            "dclx" => "dclx",
            "latex" => "tex",
            "text" => "txt",
            "pandoc" => "pandoc.json",
            "vtt" => "vtt",
            _ => unreachable!("validated above"),
        };
        Self {
            ext,
            used: std::collections::HashSet::new(),
        }
    }

    fn next(&mut self, source_name: &str) -> String {
        let stem = std::path::Path::new(source_name)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document".into());
        let ext = self.ext;
        let mut key = format!("{stem}.{ext}");
        let mut n = 1;
        while !self.used.insert(key.clone()) {
            n += 1;
            key = format!("{stem}-{n}.{ext}");
        }
        key
    }
}

/// The #139 cloud-target path: convert each source, upload its rendered
/// output as `<stem>.<ext>` under the target's prefix, answer with docling's
/// `RemoteTargetResult` kind plus per-item detail. Batch semantics
/// throughout — a failing item (conversion or upload) fails only itself.
fn run_conversion_to_target(
    state: &AppState,
    sources: Vec<SourceItem>,
    options: &ConvertOptions,
    to: &str,
    image_mode: ImageMode,
    coords: &passthrough::CloudCoords,
    rt: &tokio::runtime::Handle,
) -> Result<StoredResponse, ApiError> {
    let mut names = OutputNames::new(to);
    let items: Vec<serde_json::Value> = sources
        .into_iter()
        .map(|item| {
            let source = match item {
                Ok(source) => source,
                Err((name, e)) => {
                    return json!({
                        "name": name,
                        "status": "failure",
                        "error": api_error_message(e),
                    })
                }
            };
            let name = source.name.clone();
            let rendered = convert_document(state, source, options)
                .and_then(|c| render_stored(state, to, image_mode, &name, &c.document, options));
            let stored = match rendered {
                Ok(stored) => stored,
                Err(e) => {
                    return json!({
                        "name": name,
                        "status": "failure",
                        "error": api_error_message(e),
                    })
                }
            };
            let key = names.next(&name);
            match rt.block_on(passthrough::put_object(coords, &key, stored.body)) {
                Ok(()) => json!({ "name": name, "status": "success", "key": key }),
                Err(e) => json!({ "name": name, "status": "failure", "error": e }),
            }
        })
        .collect();
    Ok(StoredResponse {
        errors: Vec::new(),
        content_type: "application/json",
        disposition: None,
        confidence: None,
        redaction: None,
        pdf_repairs: None,
        // Upstream's RemoteTargetResult is the bare kind ("no content, the
        // result has been pushed to a remote target"); the credential-free
        // target display and per-item outcomes ride along as extra keys.
        body: serde_json::to_vec_pretty(&json!({
            "kind": "RemoteTargetResult",
            "target": coords.display(),
            "results": items,
        }))
        .expect("target ack serializes"),
    })
}

/// The #303 `zip` target: convert every source, render as for `to`, answer
/// with one deflated archive of `<stem>.<ext>` entries — jobkit's batch
/// download, and the only target whose output stays in the response body.
/// Batch semantics: a failing item becomes a `<stem>.<ext>.error.txt` entry
/// carrying the message instead of sinking the whole archive; a single-source
/// request propagates the error as its response, like the inbody path.
fn run_conversion_to_zip(
    state: &AppState,
    sources: Vec<SourceItem>,
    options: &ConvertOptions,
    to: &str,
    image_mode: ImageMode,
) -> Result<StoredResponse, ApiError> {
    let single = sources.len() == 1;
    let mut names = OutputNames::new(to);
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut archive_stem: Option<String> = None;
    for item in sources {
        let (name, rendered) = match item {
            Ok(source) => {
                let name = source.name.clone();
                let rendered = convert_document(state, source, options).and_then(|c| {
                    render_stored(state, to, image_mode, &name, &c.document, options)
                });
                (name, rendered.map(|stored| stored.body))
            }
            Err((name, e)) => (name, Err(e)),
        };
        let entry = names.next(&name);
        if single {
            archive_stem = entry.split('.').next().map(str::to_string);
        }
        match rendered {
            Ok(body) => entries.push((entry, body)),
            Err(e) if single => return Err(e),
            Err(e) => entries.push((
                format!("{entry}.error.txt"),
                api_error_message(e).into_bytes(),
            )),
        }
    }
    let entry_refs: Vec<(&str, &[u8])> = entries
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    // A single input names its archive after itself; a batch gets a generic
    // name (there is no one stem to speak for it).
    let file_name = archive_stem.map_or_else(|| "converted.zip".into(), |s| format!("{s}.zip"));
    Ok(StoredResponse {
        errors: Vec::new(),
        content_type: "application/zip",
        disposition: Some(format!("attachment; filename=\"{file_name}\"")),
        confidence: None,
        redaction: None,
        pdf_repairs: None,
        body: docling::dclx::zip_bytes(entry_refs),
    })
}

/// The #303 `put` target: HTTP PUT each rendered output to the caller's URL —
/// the pre-signed S3/GCS/Azure upload shape, so credentials live in the URL
/// the caller minted, never in the request body. The URL is caller-steered
/// outbound traffic: gated behind `--allow-url-fetch` at parse time, checked
/// against the SSRF block-list here (on the blocking pool — DNS), uploaded
/// with redirects disabled so a public URL can't bounce into an internal
/// target. Answers with the same `RemoteTargetResult` acknowledgment as the
/// cloud targets; a failing item (conversion or upload) fails only itself.
fn run_conversion_to_put(
    state: &AppState,
    sources: Vec<SourceItem>,
    options: &ConvertOptions,
    to: &str,
    image_mode: ImageMode,
    url: &str,
) -> Result<StoredResponse, ApiError> {
    check_outbound_url(url)?;
    // The upload connects only to the addresses validated at connect time,
    // so a rebinding DNS answer cannot point it inward (#752).
    let agent = docling::fetch::guarded_agent(
        ureq::Agent::config_builder()
            .max_redirects(0)
            .timeout_connect(Some(std::time::Duration::from_secs(10)))
            .timeout_global(Some(std::time::Duration::from_secs(300)))
            .build(),
        false,
    );
    let mut names = OutputNames::new(to);
    let items: Vec<serde_json::Value> = sources
        .into_iter()
        .map(|item| {
            let source = match item {
                Ok(source) => source,
                Err((name, e)) => {
                    return json!({
                        "name": name,
                        "status": "failure",
                        "error": api_error_message(e),
                    })
                }
            };
            let name = source.name.clone();
            let rendered = convert_document(state, source, options)
                .and_then(|c| render_stored(state, to, image_mode, &name, &c.document, options));
            let stored = match rendered {
                Ok(stored) => stored,
                Err(e) => {
                    return json!({
                        "name": name,
                        "status": "failure",
                        "error": api_error_message(e),
                    })
                }
            };
            // Every output goes to the one URL, as upstream's PutTarget says
            // — a pre-signed URL addresses a single object, so a batch here
            // last-writer-wins; the ack's `key` names what each PUT carried.
            let key = names.next(&name);
            match agent
                .put(url)
                .header("content-type", stored.content_type)
                .send(&stored.body[..])
            {
                Ok(_) => json!({ "name": name, "status": "success", "key": key }),
                // ureq treats a non-2xx status as an error, which is exactly
                // the per-item failure we want recorded.
                Err(e) => json!({
                    "name": name,
                    "status": "failure",
                    "error": format!("PUT: {e}"),
                }),
            }
        })
        .collect();
    Ok(StoredResponse {
        errors: Vec::new(),
        content_type: "application/json",
        disposition: None,
        confidence: None,
        redaction: None,
        pdf_repairs: None,
        // The echoed target drops the query string: pre-signed URLs carry
        // their signature there, and the ack may transit logs and proxies.
        body: serde_json::to_vec_pretty(&json!({
            "kind": "RemoteTargetResult",
            "target": redact_url(url),
            "results": items,
        }))
        .expect("target ack serializes"),
    })
}

/// A URL safe to echo back: scheme://host/path, with the query string (where
/// pre-signed URLs carry their signatures) and fragment dropped.
fn redact_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) => {
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        }
        Err(_) => url.to_string(),
    }
}

/// Return freed heap to the OS after a conversion (#263). A PDF conversion
/// churns hundreds of MB of page bitmaps through glibc's allocator, which
/// keeps the freed arenas mapped — measured here, a warm server sat at
/// ~2 GB RSS after one big conversion with the actual live data a fraction
/// of that. Giving the free pages back lets admission control (and the
/// operator's dashboards) see the truth. See [`docling::memory`].
fn trim_heap() {
    docling::memory::release_free_memory();
}

/// How often the idle unloader (#842) looks: a quarter of the window, at
/// least a second and at most half a minute — an unload lands within that
/// of the window's end, and the check itself is a `try_lock` and a clock
/// read.
fn idle_unload_period(idle: Duration) -> Duration {
    (idle / 4).clamp(Duration::from_secs(1), Duration::from_secs(30))
}

#[cfg(test)]
mod idle_unload_tests {
    use super::*;

    #[test]
    fn period_is_a_bounded_fraction_of_the_window() {
        assert_eq!(
            idle_unload_period(Duration::from_secs(2)),
            Duration::from_secs(1)
        );
        assert_eq!(
            idle_unload_period(Duration::from_secs(60)),
            Duration::from_secs(15)
        );
        assert_eq!(
            idle_unload_period(Duration::from_secs(3600)),
            Duration::from_secs(30)
        );
    }

    fn state(idle_unload_secs: Option<u64>) -> Arc<AppState> {
        Arc::new(AppState {
            pipeline: Mutex::new(None),
            last_pipeline_use: Mutex::new(Instant::now()),
            permits: Arc::new(Semaphore::new(1)),
            jobs: Mutex::new(HashMap::new()),
            readiness: Mutex::new(Readiness::Warm),
            memory_ceiling_mb: None,
            cfg: ServeConfig {
                idle_unload_secs,
                ..ServeConfig::default()
            },
        })
    }

    /// `Pipeline::new()` loads no models (they are lazy), so the slot's life
    /// cycle runs in plain CI: an unused pipeline is dropped once the window
    /// passed, never before, never while a request holds it, and the stamp
    /// moves when a request hands the slot back.
    #[test]
    fn unloads_only_an_idle_pipeline() {
        let st = state(Some(1));
        *st.pipeline_slot() = Some((PipelineFlags::default(), Pipeline::new().unwrap()));
        let idle = Duration::from_millis(50);
        assert!(
            !st.unload_idle_pipeline(idle),
            "just released: not idle yet"
        );
        std::thread::sleep(Duration::from_millis(80));
        {
            let held = st.pipeline_slot();
            assert!(!st.unload_idle_pipeline(idle), "a request holds it");
            assert!(held.is_some());
        }
        assert!(
            !st.unload_idle_pipeline(idle),
            "handing it back restarted the clock"
        );
        std::thread::sleep(Duration::from_millis(80));
        assert!(st.unload_idle_pipeline(idle));
        assert!(st.pipeline_slot().is_none());
        assert_eq!(*st.readiness.lock().unwrap(), Readiness::Lazy);
        assert!(!st.unload_idle_pipeline(idle), "nothing left to unload");
    }
}

/// Server-side cap on pages rasterized per `to=images` request (#243). A small
/// upload can be a 1000-page PDF, and every rendered page is hundreds of KB of
/// PNG held in the response (or an async job's stored result) — without a cap
/// one request could balloon memory far past the body limit. Override with
/// `DOCLING_RS_MAX_RASTER_PAGES`.
fn max_raster_pages() -> usize {
    docling_core::env::parse("DOCLING_RS_MAX_RASTER_PAGES").unwrap_or(100)
}

/// `to=images` (#243): rasterize a PDF's pages to PNG — no models, no OCR — honoring the request's `pages` window and `scale`. Returns
/// the response's `pages` array; base64 in JSON mirrors the batch `dclx_base64`
/// precedent (and survives async job storage unchanged).
fn rasterize_pages(
    state: &AppState,
    source: &SourceDocument,
    options: &ConvertOptions,
) -> Result<Vec<serde_json::Value>, ApiError> {
    if source.format != InputFormat::Pdf {
        return Err(ApiError::Unsupported(format!(
            "to=images rasterizes PDF inputs only ('{}' is not a PDF); \
             other formats convert with to=md|json|dclx|chunks",
            source.name
        )));
    }
    let scale = options.scale.unwrap_or(2.0);
    if !(0.1..=4.0).contains(&scale) {
        return Err(ApiError::Bad(format!(
            "scale {scale} out of range (0.1–4.0 pixels per PDF point; 2.0 = 144 dpi)"
        )));
    }
    let range = options.convert.page_range().map_err(bad)?;
    // Enforce the page cap before rendering anything. Count with the window
    // applied — pages=A-B is exactly the documented way to rasterize a slice
    // of a document that exceeds the cap.
    let total = docling::pdf_page_count(&source.bytes, None)
        .map_err(|e| pdf_api_error(&e, ApiError::Unsupported))?;
    let selected = match range {
        Some((first, last)) if first <= total => last.min(total) - first + 1,
        Some(_) => 0, // out-of-document start — render_pages reports the error
        None => total,
    };
    let cap = max_raster_pages();
    if selected > cap {
        return Err(ApiError::Bad(format!(
            "{selected} pages exceed the rasterization cap of {cap}; narrow the request \
             with pages=A-B (or raise DOCLING_RS_MAX_RASTER_PAGES on the server)"
        )));
    }
    // The warm pipeline's mutex serializes this process's PDF work (pdfium,
    // when the `pdfium` feature renders with it, is not thread-safe) — hold
    // it for the render even though no models run here, so a rasterization
    // can't race a concurrent PDF/image conversion.
    let _pdf_owner = state.pipeline_slot();
    let password = options.convert.password.as_deref();
    let pages = docling::render_pdf_pages(&source.bytes, password, range, scale)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(pages
        .iter()
        .map(|p| {
            json!({
                "page": p.page_no,
                "width": p.width,
                "height": p.height,
                "png_base64": docling::base64::encode(&p.png),
            })
        })
        .collect())
}

/// Serialize a converted document for a buffered (non-streaming) response.
/// Responses carry the conversion-confidence report (#183) when the ML
/// pipeline produced one: as an `X-Docling-Confidence` summary header
/// everywhere, and as a top-level `"confidence"` key (with the per-page
/// breakdown) appended to the `json` body — the document keys themselves are
/// untouched, so the body still parses as a docling-JSON document.
fn render_stored(
    state: &AppState,
    to: &str,
    image_mode: ImageMode,
    name: &str,
    document: &DoclingDocument,
    options: &ConvertOptions,
) -> Result<StoredResponse, ApiError> {
    let confidence = confidence_header(document);
    Ok(match to {
        "md" | "markdown" => StoredResponse {
            errors: Vec::new(),
            content_type: "text/markdown; charset=utf-8",
            disposition: None,
            confidence,
            redaction: None,
            pdf_repairs: None,
            body: markdown_string(state, document, image_mode, options).into_bytes(),
        },
        "json" => {
            // Written straight from the typed export with the confidence
            // report appended (#875): `export_to_json_value` would hold the
            // whole document as a `Value` — ~3.4 KB per table cell — beside
            // the bytes.
            let report = document.confidence.as_ref().map(|r| r.to_json());
            let extras: Vec<(&str, &serde_json::Value)> =
                report.iter().map(|r| ("confidence", r)).collect();
            let mut body = Vec::new();
            document
                .write_json_pretty_with(&mut body, &extras)
                .expect("document JSON serializes");
            StoredResponse {
                errors: Vec::new(),
                content_type: "application/json",
                disposition: None,
                confidence,
                redaction: None,
                pdf_repairs: None,
                body,
            }
        }
        "chunks" => {
            let chunk_opts = parse_chunk_options(options)?;
            let mut warnings: Vec<String> = Vec::new();
            let mut records =
                docling::chunks::chunk_records_with(document, &chunk_opts, &mut |m| {
                    warnings.push(m)
                })
                .map_err(ApiError::Bad)?;
            if !warnings.is_empty() {
                records["warnings"] = json!(warnings);
            }
            StoredResponse {
                errors: Vec::new(),
                content_type: "application/json",
                disposition: None,
                confidence,
                redaction: None,
                pdf_repairs: None,
                body: serde_json::to_vec(&records).expect("chunk records serialize"),
            }
        }
        "dclx" => StoredResponse {
            errors: Vec::new(),
            content_type: "application/octet-stream",
            disposition: Some(format!("attachment; filename=\"{name}.dclx\"")),
            confidence,
            redaction: None,
            pdf_repairs: None,
            body: docling::dclx::to_dclx_bytes(document),
        },
        // #317: a text body like Markdown (no download disposition).
        "latex" => StoredResponse {
            errors: Vec::new(),
            content_type: "text/x-tex; charset=utf-8",
            disposition: None,
            confidence,
            redaction: None,
            pdf_repairs: None,
            body: document.export_to_latex().into_bytes(),
        },
        // #613: docling's `--to text`, a text body like Markdown.
        "text" => StoredResponse {
            errors: Vec::new(),
            content_type: "text/plain; charset=utf-8",
            disposition: None,
            confidence,
            redaction: None,
            pdf_repairs: None,
            body: text_string(state, document, options).into_bytes(),
        },
        // #614: docling's `--to vtt`, a text body like Markdown.
        "vtt" => StoredResponse {
            errors: Vec::new(),
            content_type: "text/vtt; charset=utf-8",
            disposition: None,
            confidence,
            redaction: None,
            pdf_repairs: None,
            body: document.export_to_vtt().into_bytes(),
        },
        // #515: Pandoc's AST, for `pandoc -f json`; pictures follow
        // `images` like HTML.
        "pandoc" => StoredResponse {
            errors: Vec::new(),
            content_type: "application/json",
            disposition: None,
            confidence,
            redaction: None,
            pdf_repairs: None,
            body: pandoc_string(document, image_mode, options).into_bytes(),
        },
        // #492: docling-core's HTML serializer; pictures follow `images`
        // exactly as the Markdown body does.
        "html" => StoredResponse {
            errors: Vec::new(),
            content_type: "text/html; charset=utf-8",
            disposition: None,
            confidence,
            redaction: None,
            pdf_repairs: None,
            body: html_string(document, image_mode).into_bytes(),
        },
        _ => unreachable!("validated above"),
    })
}

/// Stamp a batch / async item with docling's `partial_success` status and
/// its `errors` when the conversion survived a problem (#497).
fn mark_partial(item: &mut serde_json::Value, converted: &Converted) {
    if !converted.errors.is_empty() {
        item["status"] = json!(converted.status());
        item["errors"] = converted.errors_json();
    }
    // The PII pass's counts (#621) ride along the same way.
    if let Some(report) = &converted.redaction {
        item["redaction"] = report.counts_json();
    }
    if let Some(repairs) = &converted.pdf_repairs {
        item["pdf_repairs"] = json!(repairs);
    }
}

/// One batch item (#182) as JSON. Text outputs inline as strings, the
/// docling-JSON document as an object, binary dclx as base64; the confidence
/// summary (#183) rides along as a sibling key where it isn't already inside
/// the document JSON.
fn batch_item(
    state: &AppState,
    to: &str,
    image_mode: ImageMode,
    name: &str,
    document: &DoclingDocument,
    options: &ConvertOptions,
) -> serde_json::Value {
    let mut item = json!({ "name": name, "status": "success" });
    match to {
        "md" | "markdown" => {
            item["md"] = json!(markdown_string(state, document, image_mode, options));
        }
        "json" => {
            let mut value = document.export_to_json_value();
            if let Some(report) = &document.confidence {
                value["confidence"] = report.to_json();
            }
            item["document"] = value;
        }
        "chunks" => {
            // A per-request chunking config that can't be honored fails this
            // item (batch semantics: other items still convert).
            let mut warnings: Vec<String> = Vec::new();
            match parse_chunk_options(options).map(|o| {
                docling::chunks::chunk_records_with(document, &o, &mut |m| warnings.push(m))
            }) {
                Ok(Ok(mut records)) => {
                    if !warnings.is_empty() {
                        records["warnings"] = json!(warnings);
                    }
                    item["chunks"] = records;
                }
                Ok(Err(e)) => {
                    item["status"] = json!("failure");
                    item["error"] = json!(e);
                }
                Err(e) => {
                    let (_, msg) = api_error_parts(e);
                    item["status"] = json!("failure");
                    item["error"] = json!(msg);
                }
            }
        }
        "dclx" => {
            item["dclx_base64"] = json!(docling::base64::encode(&docling::dclx::to_dclx_bytes(
                document
            )));
        }
        "latex" => item["latex"] = json!(document.export_to_latex()),
        "text" => item["text"] = json!(text_string(state, document, options)),
        "vtt" => item["vtt"] = json!(document.export_to_vtt()),
        "html" => item["html"] = json!(html_string(document, image_mode)),
        // #515: the AST inline as an object, like the docling JSON document.
        "pandoc" => {
            item["pandoc"] = serde_json::from_str(&pandoc_string(document, image_mode, options))
                .expect("the Pandoc export is valid JSON")
        }
        _ => unreachable!("validated above"),
    }
    if to != "json" {
        if let Some(report) = &document.confidence {
            item["confidence"] = report.summary_json();
        }
    }
    item
}

/// Buffered Markdown export honoring the request's `strict` / `images`
/// options — the non-streaming counterpart of `stream_markdown`'s serializer
/// calls (batch items and async results can't stream).
fn markdown_string(
    state: &AppState,
    document: &DoclingDocument,
    image_mode: ImageMode,
    options: &ConvertOptions,
) -> String {
    let mut doc = document.clone();
    doc.strict_markdown = options.convert.strict.unwrap_or(state.cfg.strict);
    doc.page_break_placeholder = options.convert.page_break_placeholder.clone();
    match image_mode {
        ImageMode::Placeholder => doc.export_to_markdown(),
        _ => {
            doc.export_to_markdown_with_images(image_mode, "artifacts")
                .0
        }
    }
}

/// The plain-text body for `to=text` (#613): docling-core's
/// `export_to_text()`, with the request's `strict` and
/// `page_break_placeholder` applied as for Markdown (`export_to_text` takes
/// a `page_break_placeholder` upstream too). Pictures print nothing in any
/// `images` mode, as upstream's plain serializer has no image output.
fn text_string(state: &AppState, document: &DoclingDocument, options: &ConvertOptions) -> String {
    let mut doc = document.clone();
    doc.strict_markdown = options.convert.strict.unwrap_or(state.cfg.strict);
    doc.page_break_placeholder = options.convert.page_break_placeholder.clone();
    doc.export_to_text()
}

/// What separates the documents of a batch in a Markdown or plain-text
/// response: a line of dashes with a blank line on each side — a thematic
/// break in Markdown, a visible rule in text. Several inputs thus answer in
/// the format one input does, in request order, instead of a JSON array
/// whose shape depended on how many files were sent.
pub const BATCH_SEPARATOR: &str =
    "--------------------------------------------------------------------------------";

/// The separator to write after `tail` (the end of what was written so far):
/// the blank line before the dashes is only what the text does not already
/// end with, so a document ending in a newline gets no extra blank lines.
fn batch_separator(tail: &str) -> String {
    let lead = if tail.is_empty() || tail.ends_with("\n\n") {
        ""
    } else if tail.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    format!("{lead}{BATCH_SEPARATOR}\n\n")
}

/// A batch item that failed, in its place in a Markdown / text body: one
/// line naming the file and the reason, so the rest of the batch still
/// arrives (and, streamed, the body is already on its way).
fn batch_error_text(name: &str, message: &str) -> String {
    format!("Error converting {name}: {message}")
}

/// The same failure for the `X-Docling-Errors` header of a buffered batch.
fn batch_error_item(name: &str, message: &str) -> docling::ErrorItem {
    docling::ErrorItem {
        component_type: "document_backend".into(),
        module_name: name.to_string(),
        error_message: message.to_string(),
        page_no: None,
    }
}

/// A Markdown / text / HTML / LaTeX batch, buffered: the documents in
/// request order, a failed item in its place as a [`batch_error_text`] line
/// — between [`BATCH_SEPARATOR`] lines for Markdown and text, as one page
/// with `<hr><hr><hr>` between the documents' `page` divs for HTML
/// ([`merge_html`]), as one LaTeX document with `\clearpage` between them
/// ([`merge_latex`]). The failures and the problems the conversions
/// survived (#497) go in the `X-Docling-Status` / `X-Docling-Errors` headers
/// as for a single document.
fn text_batch(
    state: &AppState,
    sources: Vec<SourceItem>,
    options: &ConvertOptions,
    to: &str,
    image_mode: ImageMode,
) -> StoredResponse {
    // Each document rendered, or the failed item's name and reason.
    let mut parts: Vec<Result<String, (String, String)>> = Vec::new();
    let mut errors: Vec<docling::ErrorItem> = Vec::new();
    for item in sources {
        let source = match item {
            Ok(source) => source,
            Err((name, e)) => {
                let msg = api_error_message(e);
                errors.push(batch_error_item(&name, &msg));
                parts.push(Err((name, msg)));
                continue;
            }
        };
        let name = source.name.clone();
        match convert_document(state, source, options) {
            Ok(mut converted) => {
                errors.append(&mut converted.errors);
                let doc = &converted.document;
                parts.push(Ok(match to {
                    "text" => text_string(state, doc, options),
                    "html" => html_string(doc, image_mode),
                    "latex" => doc.export_to_latex(),
                    _ => markdown_string(state, doc, image_mode, options),
                }));
            }
            Err(e) => {
                let msg = api_error_message(e);
                errors.push(batch_error_item(&name, &msg));
                parts.push(Err((name, msg)));
            }
        }
    }
    let (content_type, body) = match to {
        "text" => ("text/plain; charset=utf-8", join_separated(parts)),
        "html" => ("text/html; charset=utf-8", merge_html(parts)),
        "latex" => ("text/x-tex; charset=utf-8", merge_latex(parts)),
        _ => ("text/markdown; charset=utf-8", join_separated(parts)),
    };
    StoredResponse {
        errors,
        content_type,
        disposition: None,
        confidence: None,
        redaction: None,
        pdf_repairs: None,
        body: body.into_bytes(),
    }
}

/// Markdown / text batch body: the parts with [`BATCH_SEPARATOR`] between
/// them — byte for byte what the streaming handler sends.
fn join_separated(parts: Vec<Result<String, (String, String)>>) -> String {
    let mut body = String::new();
    for (i, part) in parts.into_iter().enumerate() {
        if i > 0 {
            let sep = batch_separator(&body);
            body.push_str(&sep);
        }
        match part {
            Ok(text) => body.push_str(&text),
            Err((name, msg)) => body.push_str(&batch_error_text(&name, &msg)),
        }
    }
    body
}

/// Python's `html.escape` without the quotes: text into an HTML element.
fn html_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// HTML batch body: one page — the first rendered document's skeleton
/// (`<!DOCTYPE html>` … `<body>` and `</body></html>`, its `<head>`'s title
/// and style) holding every document's `<div class='page'>` in request
/// order, `<hr><hr><hr>` between them, a failed item as a `<p>` with its
/// [`batch_error_text`]. No document rendered → a bare skeleton of the
/// error paragraphs.
fn merge_html(parts: Vec<Result<String, (String, String)>>) -> String {
    /// The page's `(prefix through "<body>\n", page content, suffix from
    /// "</body>")`.
    fn split(page: &str) -> Option<(&str, &str, &str)> {
        let body_at = page.find("<body>\n")? + "<body>\n".len();
        let end_at = page.rfind("\n</body>")?;
        Some((
            &page[..body_at],
            &page[body_at..end_at],
            &page[end_at + 1..],
        ))
    }
    let skeleton = parts
        .iter()
        .find_map(|p| p.as_ref().ok())
        .and_then(|page| split(page).map(|(pre, _, post)| (pre.to_string(), post.to_string())))
        .unwrap_or_else(|| {
            (
                "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"UTF-8\"/>\n</head>\n<body>\n"
                    .to_string(),
                "</body>\n</html>".to_string(),
            )
        });
    let inner: Vec<String> = parts
        .into_iter()
        .map(|p| match p {
            Ok(page) => match split(&page) {
                Some((_, content, _)) => content.to_string(),
                None => page,
            },
            Err((name, msg)) => format!("<p>{}</p>", html_text(&batch_error_text(&name, &msg))),
        })
        .collect();
    format!(
        "{}{}\n{}",
        skeleton.0,
        inner.join("\n<hr><hr><hr>\n"),
        skeleton.1
    )
}

/// LaTeX batch body: one document — the first rendered document's preamble
/// (the class and packages are the same for every document; its `\title`
/// stays there), every document's body between `\begin{document}` and
/// `\end{document}` in request order with `\clearpage` between them, a
/// later document's `\title` moved in front of its `\maketitle` (set in the
/// body, where LaTeX accepts it), a failed item as a bold
/// [`batch_error_text`] paragraph.
fn merge_latex(parts: Vec<Result<String, (String, String)>>) -> String {
    /// `(preamble through "\begin{document}", body)`.
    fn split(tex: &str) -> Option<(&str, &str)> {
        let begin = "\\begin{document}";
        let at = tex.find(begin)? + begin.len();
        let end = tex.rfind("\\end{document}")?;
        Some((&tex[..at], tex[at..end].trim_matches('\n')))
    }
    let mut preamble: Option<String> = None;
    let mut bodies: Vec<String> = Vec::new();
    for part in parts {
        match part {
            Ok(tex) => match split(&tex) {
                Some((pre, body)) => {
                    if preamble.is_none() {
                        preamble = Some(pre.to_string());
                        bodies.push(body.to_string());
                    } else {
                        // Its own `\title{…}` line, if any, ahead of its body.
                        let title = pre
                            .lines()
                            .find(|l| l.starts_with("\\title{"))
                            .map(|l| format!("{l}\n"))
                            .unwrap_or_default();
                        bodies.push(format!("{title}{body}"));
                    }
                }
                None => bodies.push(tex),
            },
            Err((name, msg)) => bodies.push(format!(
                "\\textbf{{{}}}",
                docling_core::escape_latex(&batch_error_text(&name, &msg))
            )),
        }
    }
    let preamble = preamble.unwrap_or_else(|| {
        let empty = docling_core::DoclingDocument::new("").export_to_latex();
        split(&empty)
            .map(|(pre, _)| pre.to_string())
            .unwrap_or_else(|| "\\documentclass{article}\n\n\\begin{document}".to_string())
    });
    let bodies: Vec<String> = bodies.into_iter().filter(|b| !b.is_empty()).collect();
    if bodies.is_empty() {
        format!("{preamble}\n\n\\end{{document}}")
    } else {
        format!(
            "{preamble}\n\n{}\n\n\\end{{document}}",
            bodies.join("\n\n\\clearpage\n\n")
        )
    }
}

/// The HTML body for `to=html` (#492): the page carries its pictures per
/// `images` the way the Markdown body does — embedded as `data:` URIs, or
/// linked under `artifacts/` for `referenced` (the bytes are not served,
/// exactly as for Markdown).
fn html_string(document: &DoclingDocument, image_mode: ImageMode) -> String {
    match image_mode {
        ImageMode::Placeholder => document.export_to_html(),
        _ => {
            document
                .export_to_html_with_images(image_mode, "artifacts")
                .0
        }
    }
}

/// `pandoc_heading_depth` parsed (#848): unset is docling's depth.
fn pandoc_heading_depth(
    options: &ConvertOptions,
) -> Result<docling::pandoc::HeadingDepth, ApiError> {
    options
        .pandoc_heading_depth
        .as_deref()
        .unwrap_or_default()
        .parse()
        .map_err(|e| ApiError::Bad(format!("pandoc_heading_depth: {e}")))
}

/// The Pandoc AST for `to=pandoc` (#515), pictures per `images` (embedded
/// `data:` URIs, or captioned figures without image data by default). The
/// API version was checked by `validate_output`.
fn pandoc_string(
    document: &DoclingDocument,
    image_mode: ImageMode,
    options: &ConvertOptions,
) -> String {
    document
        .export_to_pandoc_json_with(&docling::pandoc::PandocExportOptions {
            image_mode,
            // Validated by `validate_output`.
            heading_depth: pandoc_heading_depth(options).unwrap_or_default(),
            // Validated with the conversion options (#851).
            track_changes: options.convert.track_changes().ok().flatten(),
            ..Default::default()
        })
        .expect("the Pandoc API version was validated")
        .0
}

/// The document-level confidence summary as a header value (compact JSON, no
/// per-page breakdown — headers should stay small). `None` when the
/// conversion had no ML stages (declarative formats).
fn confidence_header(document: &DoclingDocument) -> Option<header::HeaderValue> {
    let report = document.confidence.as_ref()?;
    header::HeaderValue::from_str(&report.summary_json().to_string()).ok()
}

/// Read the multipart request: one or more `file` parts (bytes + filename —
/// several files make a #182 batch; `files` is accepted as an alias) plus
/// optional text parts mirroring the query options.
async fn read_multipart(
    mut multipart: Multipart,
    query: ConvertOptions,
) -> Result<(Vec<SourceItem>, ConvertOptions), ApiError> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut body_opts = ConvertOptions::default();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| multipart_error("bad multipart field", e))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" | "files" => {
                let file_name = field
                    .file_name()
                    .map(|s| s.to_string())
                    .ok_or_else(|| ApiError::Bad("file part needs a filename".into()))?;
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| multipart_error("reading upload", e))?;
                files.push((file_name, bytes.to_vec()));
            }
            // Every other text part is an option in its wire spelling —
            // the shared set and this surface's output options alike.
            _ => {
                let v = text_field(field).await?;
                body_opts.set_text(&name, &v)?;
            }
        }
    }
    if files.is_empty() {
        return Err(ApiError::Bad("missing 'file' part".into()));
    }
    // Per-file errors (unknown extension) are deferred: a single-file request
    // propagates them as its response, a batch fails only that item.
    let sources = files
        .into_iter()
        .flat_map(|(file_name, bytes)| sources_from_named_bytes(&file_name, bytes))
        .collect();
    Ok((sources, body_opts.merge_over(query)))
}

async fn text_field(field: axum::extract::multipart::Field<'_>) -> Result<String, ApiError> {
    field
        .text()
        .await
        .map_err(|e| multipart_error("reading field", e))
}

/// The sources one named file contributes: itself, or — for a `.zip` — every
/// document inside it (#557), each its own item, so the request converts them
/// as a batch. Entries that do not convert (unsupported type, nested archive,
/// unsafe path, over an [`ArchiveLimits`](docling::ArchiveLimits) bound —
/// `DOCLING_RS_ZIP_MAX_*`) are logged and left out; an archive that cannot be
/// read, or holds nothing convertible, is one failed item.
fn sources_from_named_bytes(file_name: &str, bytes: Vec<u8>) -> Vec<SourceItem> {
    let is_zip = std::path::Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"));
    if !is_zip {
        return vec![source_from_named_bytes(file_name, bytes).map_err(|e| (file_name.into(), e))];
    }
    let limits = docling::ArchiveLimits::from_env();
    let mut archive = match docling::archive::Archive::open(std::io::Cursor::new(bytes), &limits) {
        Ok(a) => a,
        Err(e) => {
            return vec![Err((
                file_name.into(),
                ApiError::Bad(format!("{file_name}: {e}")),
            ))]
        }
    };
    let entries = archive.entries().to_vec();
    let mut items: Vec<SourceItem> = Vec::new();
    for entry in entries {
        let label = format!("{file_name}:{}", entry.path);
        if let Some(reason) = &entry.skipped {
            eprintln!("docling-serve: skipping {label}: {reason}");
            continue;
        }
        items.push(
            archive
                .read(entry.index)
                .map_err(|e| (label, ApiError::Bad(e.to_string()))),
        );
    }
    if items.is_empty() {
        items.push(Err((
            file_name.into(),
            ApiError::Bad(format!(
                "{file_name}: the archive holds no document to convert"
            )),
        )));
    }
    items
}

/// Build a [`SourceDocument`] from a filename (extension → format) and bytes.
fn source_from_named_bytes(file_name: &str, bytes: Vec<u8>) -> Result<SourceDocument, ApiError> {
    source_from_named_bytes_ct(file_name, bytes, None)
}

/// As [`source_from_named_bytes`], with an optional response `Content-Type` used
/// as a fallback when the name carries no usable extension — a URL like
/// `…/help/example-domains` has no `.html`, but the server reports
/// `text/html`, so it still converts. The resolution (extension, media type,
/// then the content's own signature — a DjVu upload under a bare name, #434)
/// is the library's [`SourceDocument::from_named_bytes`]; this maps its
/// failure onto the status: an extension docling does not know is
/// unsupported, a name that decides nothing is a bad request.
fn source_from_named_bytes_ct(
    file_name: &str,
    bytes: Vec<u8>,
    content_type: Option<&str>,
) -> Result<SourceDocument, ApiError> {
    let has_ext = std::path::Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some();
    SourceDocument::from_named_bytes(file_name, bytes, content_type).map_err(|e| {
        let msg = match e {
            docling::ConversionError::UnknownFormat { hint } => hint,
            other => other.to_string(),
        };
        if has_ext {
            ApiError::Unsupported(msg)
        } else {
            ApiError::Bad(msg)
        }
    })
}

/// docling-serve's URL-fetch policy on top of the library's defaults
/// (`DOCLING_RS_MAX_FETCH_BYTES`, 10 s connect / 300 s total — without
/// timeouts a slow-drip URL pins one spawn_blocking worker indefinitely, and
/// a handful of them starves the pool): no redirects, since a public URL
/// could 30x-bounce to an internal target and defeat the pre-check.
fn fetch_policy(headers: &[(String, String)]) -> docling::FetchPolicy {
    docling::FetchPolicy {
        max_redirects: 0,
        headers: headers.to_vec(),
        ..docling::FetchPolicy::default()
    }
}

/// SSRF pre-check shared by every request-supplied outbound URL — URL inputs
/// and the `vlm_endpoint` request option (#304): the library's
/// [`docling::fetch::check_outbound_url`] (http(s) only, host resolved and
/// refused when private/loopback/link-local unless
/// `DOCLING_RS_ALLOW_PRIVATE_IP_FETCH` lifts it), as a 400. Blocking (DNS) —
/// call on the blocking pool.
fn check_outbound_url(url: &str) -> Result<(), ApiError> {
    docling::fetch::check_outbound_url(url, false).map_err(ApiError::Bad)
}

/// Fetch a URL input (blocking; run on the blocking pool). The name comes
/// from `file_name`, else the response's `Content-Disposition`, else the
/// URL path's last segment; request headers ride along verbatim (docling's
/// `HttpSource.headers`, #139 — auth tokens for protected origins).
fn fetch_url(
    url: &str,
    file_name: Option<&str>,
    headers: &[(String, String)],
) -> Result<SourceDocument, ApiError> {
    let fetched = docling::fetch::fetch(url, &fetch_policy(headers))
        .map_err(|e| ApiError::Bad(e.to_string()))?;
    let name = file_name.map(str::to_string).unwrap_or_else(|| {
        docling::fetch::fetched_name(&fetched.url, fetched.content_disposition.as_deref())
    });
    // Record the fetch URL as the document's base URL so relative `<img src>`
    // on a fetched web page resolve against its origin when fetch_images is on.
    Ok(
        source_from_named_bytes_ct(&name, fetched.bytes, fetched.content_type.as_deref())?
            .with_base_url(fetched.url),
    )
}

/// Convert to a [`DoclingDocument`], routing PDF/image through the warm
/// pipeline and everything else through the declarative converter. Every
/// buffered conversion — sync, batch item, async job, cloud target — funnels
/// through here, which makes it the one place the per-outcome conversion
/// metric is recorded (#297; the declarative *streaming* path bypasses this
/// and records at stream end).
fn convert_document(
    state: &AppState,
    source: SourceDocument,
    options: &ConvertOptions,
) -> Result<Converted, ApiError> {
    let result = convert_document_inner(state, source, options);
    o11y::record_conversion(result.is_ok());
    result
}

/// A converted document with the problems its conversion survived —
/// docling's `ConversionResult.errors`, non-empty for a `partial_success`
/// (today: a spent `document_timeout`, #497).
struct Converted {
    document: DoclingDocument,
    errors: Vec<docling::ErrorItem>,
    /// The PII pass's counts (#621) when the request asked for redaction —
    /// `X-Docling-Redaction` on a single response, `redaction` on a batch /
    /// async item. Never the mapping.
    redaction: Option<docling_core::RedactionReport>,
    /// What the PDF reader repaired to read the input (#934), when the
    /// request asked `pdf_report_repairs` — `X-Docling-Pdf-Repairs` on a
    /// single response, `pdf_repairs` on a batch / async item.
    pdf_repairs: Option<Vec<docling::PdfRepair>>,
}

impl Converted {
    /// The `X-Docling-Pdf-Repairs` value: the list as JSON.
    fn pdf_repairs_header(&self) -> Option<header::HeaderValue> {
        let repairs = self.pdf_repairs.as_ref()?;
        header::HeaderValue::from_str(&serde_json::to_string(repairs).ok()?).ok()
    }

    /// The `X-Docling-Redaction` value: the counts as JSON.
    fn redaction_header(&self) -> Option<header::HeaderValue> {
        let report = self.redaction.as_ref()?;
        header::HeaderValue::from_str(&report.counts_json().to_string()).ok()
    }

    fn status(&self) -> &'static str {
        if self.errors.is_empty() {
            "success"
        } else {
            "partial_success"
        }
    }

    fn errors_json(&self) -> serde_json::Value {
        json!(self
            .errors
            .iter()
            .map(|e| json!({
                "component_type": e.component_type,
                "module_name": e.module_name,
                "error_message": e.error_message,
                "page_no": e.page_no,
            }))
            .collect::<Vec<_>>())
    }
}

/// Resolve the request's `pipeline` / `vlm_*` options (#304) into
/// [`docling::vlm::VlmOptions`], or `None` for the standard pipeline. The
/// contract mirrors the Node bindings: `pipeline` absent or `standard`
/// intentionally ignores stray `vlm_*` options; blank strings count as unset
/// (reaching the `DOCLING_RS_VLM_*` env fallbacks, the operator-pinned mode).
/// A request-supplied `vlm_endpoint` is the same outbound/SSRF surface as a
/// URL input: it needs `--allow-url-fetch`, and — when `check_ip` (call it on
/// the blocking pool: DNS) — the [`check_outbound_url`] resolution check.
fn resolve_vlm_options(
    state: &AppState,
    options: &ConvertOptions,
    check_ip: bool,
) -> Result<Option<docling::vlm::VlmOptions>, ApiError> {
    let o = &options.convert;
    if o.pipeline().map_err(bad)? != docling::PipelineKind::Vlm {
        return Ok(None);
    }
    if let Some(url) = o.vlm_endpoint.as_deref().filter(|v| !v.trim().is_empty()) {
        if !state.cfg.allow_url_fetch {
            return Err(ApiError::Unsupported(
                "request-supplied vlm_endpoint is disabled; start docling-serve \
                 with --allow-url-fetch (SSRF surface — see docs/SECURITY.md), \
                 or pin the endpoint server-side via DOCLING_RS_VLM_ENDPOINT"
                    .into(),
            ));
        }
        if check_ip {
            check_outbound_url(url)?;
        }
    }
    // The shared resolution (#577): env fallbacks, blank-as-unset, the
    // `max_tokens` check, `pages` composing like it does with the ML pipeline.
    o.vlm_options().map_err(bad)
}

/// The picture-description pass's endpoint (#670) is the same outbound/SSRF
/// surface as `vlm_endpoint`: a request-supplied one (its own field, or
/// `vlm_endpoint` standing in for it) needs `--allow-url-fetch` and the
/// resolution check; the server's `DOCLING_RS_VLM_ENDPOINT` needs neither.
/// The pass's other options are validated through the shared resolution.
fn check_picture_description(
    state: &AppState,
    options: &ConvertOptions,
    check_ip: bool,
) -> Result<(), ApiError> {
    let o = &options.convert;
    if o.do_picture_description != Some(true) {
        return Ok(());
    }
    let supplied = o
        .picture_description_endpoint
        .as_deref()
        .or(o.vlm_endpoint.as_deref())
        .filter(|v| !v.trim().is_empty());
    if let Some(url) = supplied {
        if !state.cfg.allow_url_fetch {
            return Err(ApiError::Unsupported(
                "request-supplied picture_description_endpoint is disabled; start docling-serve \
                 with --allow-url-fetch (SSRF surface — see docs/SECURITY.md), or pin the \
                 endpoint server-side via DOCLING_RS_VLM_ENDPOINT"
                    .into(),
            ));
        }
        if check_ip {
            check_outbound_url(url)?;
        }
    }
    o.picture_description_options().map_err(bad)?;
    Ok(())
}

fn convert_document_inner(
    state: &AppState,
    source: SourceDocument,
    options: &ConvertOptions,
) -> Result<Converted, ApiError> {
    // #304: the VLM pipeline is a sibling path — the remote model does the
    // reading, none of the local ML options apply. Resolved here (on the
    // blocking pool, where the endpoint's DNS check belongs) so every
    // buffered surface — sync, batch item, async job, cloud target — and the
    // streaming path's buffered branch pick it up in one place. A VLM failure
    // is a per-request error like any other, never a server crash.
    check_picture_description(state, options, true)?;
    if let Some(vlm) = resolve_vlm_options(state, options, true)? {
        let finish = request_converter(state, options)?;
        // A failed or cut-off page (#762, #763) is a `partial_success`
        // with its page error, the pages around it kept.
        return docling::vlm::convert_vlm_result(&source, &vlm)
            .map_err(|e| conversion_api_error(&e))
            .and_then(|result| {
                let mut document = result.document;
                finish.finish_document(&mut document);
                // The PII pass (#621) on the VLM path too.
                let redaction = finish
                    .redact(&mut document)
                    .map_err(|e| conversion_api_error(&e))?;
                Ok(Converted {
                    document,
                    errors: result.errors,
                    redaction,
                    pdf_repairs: None,
                })
            });
    }
    match source.format {
        InputFormat::Pdf | InputFormat::Image => {
            // The warm pipeline bypasses `DocumentConverter::convert`, so the
            // document-level settings it applies to every other format —
            // `compact_tables`, `strict`, `md_page_break_placeholder`, derived
            // table cells — are applied here from the same request converter
            // (#547: `compact_tables=true` used to come back padded for a
            // PDF while a DOCX in the same request shape came back compact).
            let finish = request_converter(state, options)?;
            // Recover from a poisoned lock instead of propagating the panic: a
            // single crafted PDF/image that panics inside `convert` below drops
            // the guard mid-unwind and poisons the mutex. Without this recovery
            // every later request would panic on `.lock().unwrap()` too, turning
            // one bad document into a permanent outage of this endpoint. The
            // pipeline state is rebuilt/validated by `warm_pipeline`, so reusing
            // it after a panic is safe.
            let mut guard = state.pipeline_slot();
            let pipeline = warm_pipeline(&mut guard, options)?;
            // Every per-request setting of the shared warm pipeline, read as
            // typed values off the validated option set (#577) — set
            // unconditionally so no request inherits a previous one's: the
            // page window (#80), the OCR engine / language (only a worker
            // whose cached recognition model mismatches reloads anything),
            // forcing, mode and scale (#254), picture-crop scale and page
            // images (#519/#520), the heading pass (#302), the budget (#497).
            let o = &options.convert;
            pipeline.set_pages(o.page_range().map_err(bad)?);
            pipeline.set_text_layer_auto(o.text_layer_only == Some(docling::TextLayerOnly::Auto));
            pipeline.set_ocr_engine(o.ocr_engine().map_err(bad)?);
            pipeline.set_ocr_lang(o.ocr_lang().map_err(bad)?);
            pipeline.set_tesseract_lang(o.tesseract_lang().map_err(bad)?);
            pipeline.set_force_full_page_ocr(o.force_full_page_ocr.unwrap_or(false));
            pipeline.set_replace_broken_text_layer(o.replace_broken_text_layer.unwrap_or(false));
            pipeline.set_ocr_mode(o.ocr_mode().map_err(bad)?);
            pipeline.set_ocr_scale(o.ocr_scale);
            pipeline.set_word_gap(o.pdf_word_gap().map_err(bad)?);
            pipeline.set_keep_typography(o.pdf_keep_typography);
            pipeline.set_ocr_textline_orientation(o.ocr_textline_orientation);
            pipeline.set_images(o.image_output());
            pipeline.set_heading_hierarchy(docling::HeadingHierarchyOptions::enabled(
                o.heading_hierarchy.unwrap_or(false),
            ));
            pipeline.set_document_timeout(o.document_timeout().map_err(bad)?);
            // The document's progress events (#783) — an async job's status
            // reads them; the warm pipeline bypasses `convert`, so the
            // reporter brackets the conversion here.
            let reporter = Arc::new(finish.reporter(1, &source.name, None));
            reporter.document_started();
            pipeline.set_progress(reporter.page_hook());
            let converted = match source.format {
                InputFormat::Pdf => pipeline
                    .convert_outcome(&source.bytes, o.password.as_deref(), &source.name)
                    .map(|c| {
                        if let docling::Completion::TimedOut { pages_selected, .. } = &c.completion
                        {
                            reporter.fail_pages_cut_off(
                                o.page_range().ok().flatten().map_or(1, |(first, _)| first),
                                *pages_selected,
                            );
                        }
                        Converted {
                            errors: c
                                .completion
                                .message()
                                .map(docling::ErrorItem::timeout)
                                .into_iter()
                                .collect(),
                            document: c.document,
                            redaction: None,
                            pdf_repairs: None,
                        }
                    }),
                _ => pipeline
                    .convert_image(&source.bytes, &source.name)
                    .map(|document| Converted {
                        document,
                        errors: Vec::new(),
                        redaction: None,
                        pdf_repairs: None,
                    }),
            };
            pipeline.set_progress(None);
            // The picture enrichment pass `convert` would have run (#896):
            // `keep_picture_images=false` and the picture description
            // reached every other format but never a PDF or image.
            let converted = converted.map(|mut c| {
                finish.enrich_pictures(&mut c.document, &source, &reporter);
                c
            });
            reporter.document_completed(match &converted {
                Ok(c) if c.errors.is_empty() => docling::ConversionStatus::Success,
                Ok(_) => docling::ConversionStatus::PartialSuccess,
                Err(_) => docling::ConversionStatus::Failure,
            });
            // An encrypted PDF is the client's to fix (422 + `code`, #636);
            // anything else the warm pipeline fails on keeps its status.
            let mut converted = converted.map_err(|e| pdf_api_error(&e, ApiError::Internal))?;
            finish.finish_document(&mut converted.document);
            // The warm pipeline bypasses `DocumentConverter::convert`, so the
            // PII pass (#621) is applied here, like the serializer knobs.
            converted.redaction = finish
                .redact(&mut converted.document)
                .map_err(|e| conversion_api_error(&e))?;
            // …and the repair report (#934), read off the input bytes.
            converted.pdf_repairs = (source.format == InputFormat::Pdf
                && finish.reports_pdf_repairs())
            .then(|| docling::pdf_repairs(&source.bytes));
            Ok(converted)
        }
        _ => {
            let converter = request_converter(state, options)?;
            converter
                .convert(source)
                .map(|r| Converted {
                    document: r.document,
                    errors: r.errors,
                    redaction: r.redaction,
                    pdf_repairs: r.pdf_repairs,
                })
                .map_err(|e| conversion_api_error(&e))
        }
    }
}

/// The pipeline switches a warm instance was built with, remembered alongside
/// it (#246): the switches are per-instance state a [`Pipeline`] doesn't
/// expose back, and without the record a flagged request permanently degraded
/// the cached instance — a later default request found the slot filled and
/// reused the reduced pipeline, silently returning flat no-OCR output until
/// the server restarted.
///
/// The enrichment flags (#423) belong here too: the workers copy them at load
/// and the model slots exist only for enabled passes, so a change of
/// enrichment is a rebuild, not a setter. Steady traffic with one enrichment
/// mix keeps its warm instance; only a *change* of mix reloads (the enrichment
/// models themselves load lazily on the first matching region anyway).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PipelineFlags {
    text_layer_only: bool,
    no_ocr: bool,
    no_table_former: bool,
    no_text_panels: bool,
    enrich: docling::EnrichmentOptions,
}

impl PipelineFlags {
    fn of(options: &ConvertOptions) -> Self {
        let o = &options.convert;
        Self {
            // `auto` is a per-request switch on the full model set (set with
            // the other per-request settings below), never a rebuild.
            text_layer_only: o.text_layer_only == Some(docling::TextLayerOnly::On),
            no_ocr: o.ocr_disabled(),
            no_table_former: o.no_table_former.unwrap_or(false),
            no_text_panels: o.no_text_panels.unwrap_or(false),
            enrich: o.enrichments(),
        }
    }
}

/// The lazily-loaded warm pipeline. Pipeline switches (`text_layer_only`, `no_ocr`,
/// `no_table_former`, `no_text_panels`) are per-instance, so the pipeline is
/// rebuilt exactly when the request's switches differ from the cached
/// instance's — including back to the default (#246; the old code only
/// rebuilt *toward* reduced configurations, so a degraded instance stuck).
/// Steady-state traffic with stable options — flagged or not — keeps the warm
/// one.
fn warm_pipeline<'a>(
    slot: &'a mut Option<(PipelineFlags, Pipeline)>,
    options: &ConvertOptions,
) -> Result<&'a mut Pipeline, ApiError> {
    let flags = PipelineFlags::of(options);
    if slot.as_ref().map(|(built, _)| *built) != Some(flags) {
        let p = Pipeline::new()
            .map_err(|e| ApiError::Internal(e.to_string()))?
            // docling-pdf's pre-2.0 names: its `no_ocr` is the text-layer
            // fast path, its `skip_ocr` docling's do_ocr=False (#611).
            .no_ocr(flags.text_layer_only)
            .skip_ocr(flags.no_ocr)
            .no_table_former(flags.no_table_former)
            .no_text_panels(flags.no_text_panels)
            .enrichments(flags.enrich);
        *slot = Some((flags, p));
    }
    Ok(&mut slot.as_mut().expect("just filled").1)
}

/// Per-request declarative converter (construction is cheap — it's
/// configuration, models don't apply): the shared options validated and
/// applied by [`docling::ConvertOptions::apply`] onto this server's base
/// (`--strict`), after the two policies only a server has.
fn request_converter(
    state: &AppState,
    options: &ConvertOptions,
) -> Result<DocumentConverter, ApiError> {
    let mut o = options.convert.clone();
    // The image-source policy (#646). `remote` (and its alias
    // `fetch_images=true`) pulls external `<img src>` over the network — the
    // same outbound-fetch / SSRF surface as URL inputs, so it lives behind
    // the same `--allow-url-fetch` gate: without the flag the request is
    // held to `embedded` (`data:` URIs and container parts, which need no
    // network) rather than refused — an API caller gets the safe tier
    // instead of a surprise outbound fetch (the UI greys the box). `local`
    // names server-side files, so it is refused unless the operator started
    // the server with `--allow-local-images`.
    let requested = o.image_sources().map_err(bad)?.or_else(|| {
        o.fetch_images
            .filter(|&f| f)
            .map(|_| docling::ImageSources::Remote)
    });
    match requested {
        Some(docling::ImageSources::Local) if !state.cfg.allow_local_images => {
            return Err(ApiError::Unsupported(
                "image_sources=local reads files next to the source on the server; \
                 start docling-serve with --allow-local-images to enable it, or use \
                 image_sources=embedded"
                    .into(),
            ));
        }
        Some(docling::ImageSources::Remote) if !state.cfg.allow_url_fetch => {
            o.image_sources = Some(docling::ImageSources::Embedded.to_string());
            o.fetch_images = Some(false);
        }
        Some(sources) => {
            o.image_sources = Some(sources.to_string());
            o.fetch_images = None;
        }
        None => {}
    }
    // A server-local directory, held to the same rule as `chunk_tokenizer`:
    // relative, no `..` — a request must not name arbitrary server paths.
    if let Some(dir) = o.xbrl_taxonomy.as_deref() {
        let path = std::path::Path::new(dir);
        let unsafe_component = path.components().any(|c| {
            !matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        });
        if unsafe_component || dir.is_empty() {
            return Err(ApiError::Bad(format!(
                "xbrl_taxonomy must be a relative path without '..' components, got {dir:?}"
            )));
        }
    }
    o.apply(DocumentConverter::new().strict(state.cfg.strict))
        .map_err(bad)
        // An async job's progress (#783) follows the converter: the job's
        // slot, when this thread runs one, hears every event.
        .map(|converter| match job_progress_callback() {
            Some(callback) => converter.progress_callback(callback),
            None => converter,
        })
}

/// A chunk of a streamed Markdown body: its text plus (first chunk of a
/// single pipeline conversion only) the confidence summary header —
/// computable only after conversion, which is exactly when the PDF/image
/// branch sends its single chunk.
/// A piece of a streamed Markdown body and the response headers it brings —
/// only a single document's first chunk carries any (`X-Docling-Confidence`,
/// `X-Docling-Pdf-Repairs`), since the headers go out with it.
type Chunk = (String, Vec<(&'static str, header::HeaderValue)>);

/// The streaming worker's side of the channel, keeping the end of what it
/// sent so the batch separator adds only the blank line the text lacks.
struct ChunkSender {
    tx: tokio::sync::mpsc::Sender<Result<Chunk, ApiError>>,
    tail: String,
}

impl ChunkSender {
    /// `false` when the receiver is gone — the client went away — stop.
    fn send(&mut self, item: Result<Chunk, ApiError>) -> bool {
        if let Ok((text, _)) = &item {
            if !text.is_empty() {
                self.tail = text
                    .chars()
                    .rev()
                    .take(2)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
            }
        }
        self.tx.blocking_send(item).is_ok()
    }
}

/// Markdown response: converted through the streaming serializer, body sent
/// chunked as pages finish — a batch document after document, with
/// [`BATCH_SEPARATOR`] between them and a failed document as one
/// [`batch_error_text`] line in its place. The semaphore permit moves into
/// the worker so the slot stays held until the stream ends.
async fn stream_markdown(
    state: Arc<AppState>,
    sources: Vec<SourceItem>,
    options: ConvertOptions,
    image_mode: ImageMode,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<Response, ApiError> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Chunk, ApiError>>(8);
    let st = state.clone();
    tokio::task::spawn_blocking(move || {
        // Held until this worker (and thus the response body) is done.
        let _permit = permit;
        // Freed page bitmaps go back to the OS when this worker finishes.
        struct TrimOnDrop;
        impl Drop for TrimOnDrop {
            fn drop(&mut self) {
                trim_heap();
            }
        }
        let _trim = TrimOnDrop;
        let batch = sources.len() > 1;
        let mut tx = ChunkSender {
            tx,
            tail: String::new(),
        };
        for (i, item) in sources.into_iter().enumerate() {
            if i > 0 && !tx.send(Ok((batch_separator(&tx.tail), Vec::new()))) {
                return;
            }
            let source = match item {
                Ok(source) => source,
                // A bad upload: alone, the error is the response; in a
                // batch, its line.
                Err((name, e)) if batch => {
                    let line = batch_error_text(&name, &api_error_message(e));
                    if !tx.send(Ok((line, Vec::new()))) {
                        return;
                    }
                    continue;
                }
                Err((_, e)) => {
                    tx.send(Err(e));
                    return;
                }
            };
            if !stream_one(&st, source, &options, image_mode, batch, &mut tx) {
                return;
            }
        }
    });

    // First chunk decides the status code; later errors abort the stream
    // mid-body (the client sees a truncated response).
    let mut rx = rx;
    let first = rx.recv().await;
    match first {
        // No chunks means the document converted to empty Markdown (e.g. an
        // HTML page with no extractable content) — a valid result, not a
        // server error. Return an empty 200 body rather than a 500.
        None => Ok((
            [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
            Body::empty(),
        )
            .into_response()),
        Some(Err(e)) => Err(e),
        Some(Ok((first_chunk, headers))) => {
            use tokio_stream::StreamExt;
            let rest = tokio_stream::wrappers::ReceiverStream::new(rx);
            let stream = tokio_stream::once(Ok((first_chunk, Vec::new())))
                .chain(rest)
                .map(|item| {
                    item.map(|(text, _)| text.into_bytes()).map_err(|e| {
                        std::io::Error::other(format!(
                            "conversion failed mid-stream: {}",
                            api_error_message(e)
                        ))
                    })
                });
            let mut response = (
                [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
                Body::from_stream(stream),
            )
                .into_response();
            for (name, value) in headers {
                response.headers_mut().insert(name, value);
            }
            Ok(response)
        }
    }
}

/// One document of a streamed Markdown response. `false` when the stream
/// is over: the client went away, or (alone, not in a batch) the document
/// failed — in a batch a failure is one [`batch_error_text`] line in the
/// document's place and the next document follows.
fn stream_one(
    st: &AppState,
    source: SourceDocument,
    options: &ConvertOptions,
    image_mode: ImageMode,
    batch: bool,
    tx: &mut ChunkSender,
) -> bool {
    let name = source.name.clone();
    // Alone, the error itself: the status code when nothing was sent yet, a
    // truncated body after.
    let fail = |tx: &mut ChunkSender, e: ApiError| -> bool {
        if batch {
            tx.send(Ok((
                batch_error_text(&name, &api_error_message(e)),
                Vec::new(),
            )))
        } else {
            tx.send(Err(e));
            false
        }
    };
    // #304: the VLM pipeline buffers whatever the input format — it is a
    // per-page remote conversion, and its "wrong input format" rejection
    // must reach the client instead of the declarative streamer running a
    // standard conversion the caller didn't ask for.
    let buffered = options.convert.pipeline().ok() == Some(docling::PipelineKind::Vlm)
        || matches!(source.format, InputFormat::Pdf | InputFormat::Image);
    if buffered {
        // Buffered document → streamed serialization is pointless for
        // images (one step); PDFs stream page by page through the
        // warm pipeline's converter equivalent: convert, then stream
        // the serializer output. (True page-by-page pipeline
        // streaming holds the model mutex anyway, so the wall-clock
        // is the same; the client still gets incremental output.)
        return match convert_document(st, source, options) {
            Ok(converted) => {
                // A partial document (#497) streams like a whole one; the
                // body is already on its way, so the reason is only logged.
                for e in &converted.errors {
                    eprintln!("docling-serve: partial document: {}", e.describe());
                }
                let repairs = converted.pdf_repairs_header();
                let mut doc = converted.document;
                doc.strict_markdown = options.convert.strict.unwrap_or(st.cfg.strict);
                doc.page_break_placeholder = options.convert.page_break_placeholder.clone();
                let md = match image_mode {
                    ImageMode::Placeholder => doc.export_to_markdown(),
                    _ => {
                        doc.export_to_markdown_with_images(image_mode, "artifacts")
                            .0
                    }
                };
                // The headers are one document's: none for a batch.
                let mut headers = Vec::new();
                if !batch {
                    if let Some(v) = confidence_header(&doc) {
                        headers.push(("x-docling-confidence", v));
                    }
                    if let Some(v) = repairs {
                        headers.push(("x-docling-pdf-repairs", v));
                    }
                }
                tx.send(Ok((md, headers)))
            }
            Err(e) => fail(tx, e),
        };
    }
    let converter = match request_converter(st, options) {
        Ok(c) => c,
        Err(e) => return fail(tx, e),
    };
    // The streaming path bypasses `convert_document`, so the
    // conversion metric (#297) is recorded here: success once the
    // stream drains, failure on a conversion error (a client that
    // disconnects mid-stream abandons the conversion and counts as
    // neither).
    match converter.convert_streaming_images(source, image_mode) {
        Ok(stream) => {
            for chunk in stream {
                match chunk {
                    Ok(s) => {
                        if !tx.send(Ok((s, Vec::new()))) {
                            return false;
                        }
                    }
                    // The stream's last item on a spent document
                    // budget (#497): the chunks before it are the
                    // partial document and the body is already on
                    // its way, so this ends the stream cleanly.
                    Err(ConversionError::Timeout(msg)) => {
                        eprintln!("docling-serve: partial document: {msg}");
                        break;
                    }
                    Err(e) => {
                        o11y::record_conversion(false);
                        return fail(tx, conversion_api_error(&e));
                    }
                }
            }
            o11y::record_conversion(true);
            true
        }
        Err(e) => {
            o11y::record_conversion(false);
            fail(tx, conversion_api_error(&e))
        }
    }
}

/// The HTTP shape of a conversion failure: a worker that *panicked* is a bug
/// on this side and answers 500, while every other error is about the document
/// and answers 422. Without the distinction a panic reached the client as an
/// empty 200 — the stream simply ended (#395/#396).
fn conversion_api_error(e: &ConversionError) -> ApiError {
    match e {
        ConversionError::Panic(msg) => ApiError::Internal(msg.clone()),
        // Still a 422 — the body names the case (#636).
        other => match other.encryption() {
            Some(kind) => ApiError::Encrypted {
                code: kind.code(),
                message: other.to_string(),
            },
            None => ApiError::Unsupported(other.to_string()),
        },
    }
}

/// The HTTP shape of the PDF pipeline's own error, reached where the warm
/// pipeline bypasses `DocumentConverter`: an encrypted document is the typed
/// 422 (#636), everything else goes through `fallback` with its message.
fn pdf_api_error(e: &docling::PdfError, fallback: fn(String) -> ApiError) -> ApiError {
    match e {
        docling::PdfError::Encrypted(kind) => ApiError::Encrypted {
            code: kind.code(),
            message: e.to_string(),
        },
        other => fallback(other.to_string()),
    }
}

fn api_error_message(e: ApiError) -> String {
    api_error_parts(e).1
}

#[cfg(test)]
mod pipeline_flag_tests {
    use super::{warm_pipeline, ConvertOptions, PipelineFlags};

    /// #246: the cached pipeline must be rebuilt whenever the request's
    /// switches differ from the ones it was built with — in BOTH directions.
    /// The old code only rebuilt toward reduced configurations, so a
    /// `text_layer_only=true` request permanently degraded the shared instance for
    /// every later default request. (`Pipeline::new()` loads no models —
    /// they're lazy — so this runs in plain CI.)
    #[test]
    fn warm_pipeline_rebuilds_in_both_directions() {
        let mut slot = None;
        let default_opts = ConvertOptions::default();
        let text_layer_opts = ConvertOptions {
            convert: docling::ConvertOptions {
                text_layer_only: Some(docling::TextLayerOnly::On),
                ..Default::default()
            },
            ..ConvertOptions::default()
        };
        assert!(warm_pipeline(&mut slot, &text_layer_opts).is_ok());
        assert_eq!(
            slot.as_ref().unwrap().0,
            PipelineFlags::of(&text_layer_opts)
        );
        // Back to default: the degraded instance must not be reused.
        assert!(warm_pipeline(&mut slot, &default_opts).is_ok());
        assert_eq!(slot.as_ref().unwrap().0, PipelineFlags::default());
        // And a repeat with unchanged flags keeps the warm instance (no
        // needless model reload): the stored flags stay identical, which is
        // the rebuild guard itself.
        assert!(warm_pipeline(&mut slot, &default_opts).is_ok());
        assert_eq!(slot.as_ref().unwrap().0, PipelineFlags::default());
    }

    /// #423: the enrichment passes are per-instance state like the model
    /// switches — a request asking for them rebuilds the warm pipeline with
    /// the passes enabled, a repeat keeps it, and a plain request afterwards
    /// rebuilds back so no default caller pays for (or receives) enrichment.
    #[test]
    fn enrichment_flags_are_part_of_the_rebuild_guard() {
        let mut slot = None;
        let enriched = ConvertOptions {
            convert: docling::ConvertOptions {
                do_picture_classification: Some(true),
                do_formula_enrichment: Some(true),
                ..Default::default()
            },
            ..ConvertOptions::default()
        };
        assert!(warm_pipeline(&mut slot, &enriched).is_ok());
        let built = slot.as_ref().unwrap().0;
        assert!(built.enrich.picture_classification && built.enrich.formula);
        assert!(!built.enrich.code);
        assert_ne!(built, PipelineFlags::default());
        // Explicit `false` and unset mean the same thing: off.
        let off = ConvertOptions {
            convert: docling::ConvertOptions {
                do_code_enrichment: Some(false),
                ..Default::default()
            },
            ..ConvertOptions::default()
        };
        assert_eq!(PipelineFlags::of(&off), PipelineFlags::default());
        assert!(warm_pipeline(&mut slot, &off).is_ok());
        assert_eq!(slot.as_ref().unwrap().0, PipelineFlags::default());
    }
}

#[cfg(test)]
mod ssrf_tests {
    #[test]
    fn url_fetch_off_by_default() {
        assert!(!super::ServeConfig::default().allow_url_fetch);
    }

    #[test]
    fn content_type_maps_to_format_when_extension_missing() {
        use super::{source_from_named_bytes_ct, ApiError, InputFormat};
        // `ApiError` has no `Debug`, so match rather than `.expect()`.
        let fmt = |r: Result<super::SourceDocument, ApiError>| r.ok().map(|s| s.format);

        // A URL with no extension (iana example) resolves via Content-Type.
        assert_eq!(
            fmt(source_from_named_bytes_ct(
                "example-domains",
                b"<html></html>".to_vec(),
                Some("text/html; charset=utf-8"),
            )),
            Some(InputFormat::Html)
        );
        // A usable extension still wins over the Content-Type.
        assert_eq!(
            fmt(source_from_named_bytes_ct(
                "a.pdf",
                b"%PDF".to_vec(),
                Some("text/html"),
            )),
            Some(InputFormat::Pdf)
        );
        // Neither an extension nor a known Content-Type: text bytes are
        // plain text (#671); binary bytes a 4xx (Bad), not a 500.
        assert_eq!(
            fmt(source_from_named_bytes_ct(
                "noext",
                b"x".to_vec(),
                Some("application/octet-stream")
            )),
            Some(InputFormat::Text)
        );
        assert!(matches!(
            source_from_named_bytes_ct(
                "noext",
                vec![0, 1, 2, 255, 254, 0],
                Some("application/octet-stream")
            ),
            Err(ApiError::Bad(_))
        ));
    }

    /// Serializes the tests that touch process-global env vars — without it
    /// the escape-hatch toggling below races the #304 VLM tests that depend
    /// on those very vars being unset.
    pub(super) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn private_ip_escape_hatch_defaults_off() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // The env var gates only development use; unset it must read as false
        // so the block-list is enforced by default. (Set within this test only,
        // then cleared, to avoid leaking to sibling tests.)
        std::env::remove_var("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH");
        assert!(!docling::fetch::allow_private_ip_fetch());
        // The accepted spellings are `docling_core::env::flag`'s, tested there.
        std::env::set_var("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH", "1");
        assert!(docling::fetch::allow_private_ip_fetch());
        std::env::remove_var("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH");
    }
}

/// #304: the request-side VLM option resolution — the standard-pipeline
/// ignore, the `--allow-url-fetch` gate on request-supplied endpoints, the
/// SSRF resolution check, and the operator-pinned env fallback that skips it.
#[cfg(test)]
mod vlm_tests {
    use super::{resolve_vlm_options, ApiError, AppState, ConvertOptions, ServeConfig};
    use std::sync::{Arc, Mutex};
    use tokio::sync::Semaphore;

    fn state(allow_url_fetch: bool) -> AppState {
        AppState {
            pipeline: Mutex::new(None),
            last_pipeline_use: Mutex::new(std::time::Instant::now()),
            permits: Arc::new(Semaphore::new(1)),
            jobs: Mutex::new(Default::default()),
            readiness: Mutex::new(super::Readiness::Lazy),
            memory_ceiling_mb: None,
            cfg: ServeConfig {
                allow_url_fetch,
                ..ServeConfig::default()
            },
        }
    }

    fn vlm_opts(endpoint: Option<&str>) -> ConvertOptions {
        ConvertOptions {
            convert: docling::ConvertOptions {
                pipeline: Some("vlm".into()),
                vlm_endpoint: endpoint.map(str::to_string),
                vlm_model: Some("m".into()),
                ..Default::default()
            },
            ..ConvertOptions::default()
        }
    }

    #[test]
    fn standard_pipeline_ignores_stray_vlm_options() {
        // Same contract as the Node bindings: no `pipeline=vlm`, no VLM — the
        // stray options are ignored, not rejected, and nothing is resolved.
        for pipeline in [None, Some("standard".to_string())] {
            let mut options = vlm_opts(Some("http://127.0.0.1:9/v1"));
            options.convert.pipeline = pipeline;
            let resolved = resolve_vlm_options(&state(false), &options, true);
            assert!(matches!(resolved, Ok(None)));
        }
    }

    #[test]
    fn request_endpoint_resolving_to_private_address_is_rejected() {
        let _env = super::ssrf_tests::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH");
        match resolve_vlm_options(&state(true), &vlm_opts(Some("http://127.0.0.1:9/v1")), true) {
            Err(ApiError::Bad(m)) => assert!(m.contains("private/loopback"), "{m}"),
            _ => panic!("expected Bad"),
        }
        // The handler-side (no-DNS) pass lets the same request through — the
        // check belongs to the blocking conversion path.
        assert!(resolve_vlm_options(
            &state(true),
            &vlm_opts(Some("http://127.0.0.1:9/v1")),
            false
        )
        .is_ok());
    }

    #[test]
    fn operator_pinned_endpoint_skips_the_gate_and_the_ip_check() {
        let _env = super::ssrf_tests::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Pinned mode: the operator configured the endpoint, so a local model
        // server (the common deployment) is fine and no gate applies.
        std::env::set_var("DOCLING_RS_VLM_ENDPOINT", "http://127.0.0.1:11434/v1");
        std::env::set_var("DOCLING_RS_VLM_MODEL", "granite-docling");
        let resolved = resolve_vlm_options(&state(false), &vlm_opts(None), true);
        std::env::remove_var("DOCLING_RS_VLM_ENDPOINT");
        std::env::remove_var("DOCLING_RS_VLM_MODEL");
        let v = resolved.ok().flatten().expect("pinned endpoint resolves");
        assert_eq!(v.endpoint, "http://127.0.0.1:11434/v1");
        // The request still picks the model when it says so (env was the
        // fallback for the endpoint only here).
        assert_eq!(v.model, "m");
    }

    /// Without any endpoint the error names this surface's option spelling
    /// (`vlm_endpoint`), not the CLI flag. The option rules themselves are
    /// `docling::ConvertOptions`' and tested there (`options.rs`).
    #[test]
    fn missing_endpoint_names_the_serve_option() {
        let _env = super::ssrf_tests::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("DOCLING_RS_VLM_ENDPOINT");
        let none = ConvertOptions {
            convert: docling::ConvertOptions {
                pipeline: Some("vlm".into()),
                ..Default::default()
            },
            ..ConvertOptions::default()
        };
        match resolve_vlm_options(&state(true), &none, false) {
            Err(ApiError::Bad(m)) => {
                assert!(m.contains("vlm_endpoint"), "{m}");
                assert!(!m.contains("--vlm-endpoint"), "{m}");
            }
            _ => panic!("expected Bad"),
        }
    }
}
