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
//! | GET    | `/ready`      | readiness probe (200 once models are warm)         |
//! | GET    | `/metrics`    | Prometheus metrics (#297, see [`o11y`])            |
//! | GET    | `/openapi.yaml` | OpenAPI 3.1 description of the API               |
//! | GET    | `/logo.svg`   | the playground's logo                              |
//!
//! `POST /v1/convert` accepts either `multipart/form-data` with a `file` part
//! (the filename's extension selects the input format; **several file parts
//! make a batch**, #182 — the response is then a JSON `results` array with
//! per-item status) or an `application/json` body
//! `{"url": "https://…", "file_name"?: "override.pdf"}`.
//! Options ride along as multipart text parts, JSON fields, or query
//! parameters (body wins over query):
//!
//! - `to` — `md` (default) | `json` | `dclx` | `chunks` | `latex` (#317) | `images` (#243:
//!   rasterize a PDF's pages to PNG through pdfium — no conversion, no models;
//!   the JSON response is `{"pages": [{"page", "width", "height",
//!   "png_base64"}]}`, combines with `pages` for a window, capped at
//!   `DOCLING_RS_MAX_RASTER_PAGES` pages per request, default 100)
//! - `scale` — `to=images` render scale in pixels per PDF point:
//!   0.1–4.0, default 2.0 (= 144 dpi, the ML pipeline's own render scale)
//! - `strict` — cleaner Markdown instead of docling-legacy output
//! - `images` — `placeholder` (default) | `embedded` (Markdown only)
//! - `no_ocr`, `skip_ocr`, `no_table_former`, `force_full_page_ocr`,
//!   `no_text_panels`, `heading_hierarchy` — PDF/image pipeline switches (`skip_ocr`, #244: keep
//!   layout + TableFormer, never OCR — docling's independent `do_ocr=False`;
//!   `no_ocr` skips the whole ML stack)
//! - `do_picture_classification`, `do_code_enrichment`,
//!   `do_formula_enrichment` — the opt-in enrichment models (#423; docling's
//!   `PdfPipelineOptions` flags of the same names, the CLI's
//!   `--enrich-picture-classes` / `--enrich-code` / `--enrich-formula`):
//!   DocumentFigureClassifier over pictures, CodeFormulaV2 over code / formula
//!   regions. Off by default; a missing model warns and skips the pass
//! - `pages` — PDF page window `A-B` / `N` (1-based inclusive, #80)
//! - `ocr_lang` — OCR recognition language for scanned pages: `en` (default)
//!   | `ch` (the multilingual docling-conformance model)
//! - `ocr_mode` — which regions feed the OCR (docling's `OcrMode`, #254):
//!   `default` | `full_page` | `layout_regions` | `pdf_aware_layout_regions`
//!   (`full_page`/`layout_regions` discard the text layer like
//!   `force_full_page_ocr`)
//! - `ocr_scale` — OCR render scale in px per PDF point (docling's
//!   `OcrOptions.scale`, #254); unset reads the pipeline's own 2.0 px/pt
//!   render, docling's default is 3 (216 dpi)
//! - `fetch_images` — resolve external `<img src>` for HTML/EPUB/MHTML/JATS (outbound
//!   fetch, so honored only under `--allow-url-fetch`)
//! - `skip_empty_cells` — omit empty cells from sparse XLSX/XLS table grids
//!   (#271; docling.rs extension, off by default)
//! - `compact_tables` — unpadded `| a | b |` Markdown tables, all formats
//!   (#271; docling.rs extension, off by default)
//! - `md_page_break_placeholder` — text inserted between pages in Markdown
//!   output (docling-serve's option of the same name, docling-core's
//!   `MarkdownParams.page_break_placeholder`; e.g. `<!-- page break -->`).
//!   A break lands only between two rendered blocks on different pages;
//!   unset (the default) keeps docling's break-free Markdown
//! - `list_attachments` — email (.eml/.msg): append an Attachments section
//!   with names and content types (#251; payload bytes are never embedded)
//! - `ebcdic_layout` — EBCDIC (#252): the copybook layout as inline
//!   `EbcdicLayout` JSON (mandatory for the format — the bytes are
//!   meaningless without it)
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
//! (`--concurrency`); excess requests queue.
//!
//! Resource controls (#262/#263): thread pools size to the **cgroup-aware**
//! CPU budget (a container's CPU quota clamps them; `DOCLING_RS_TF_INTRA`
//! additionally narrows the shared TableFormer session). The server binary
//! defaults `DOCLING_RS_NO_ARENA=1` — ONNX Runtime's CPU arena grows with
//! every new page shape and never returns memory; without it (plus a
//! `malloc_trim` after each conversion) a warm server's retained RSS measured
//! ~3× lower and stopped ratcheting, at no observed latency cost. A memory
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

use std::io::Read;
use std::net::ToSocketAddrs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;

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
    /// Load the PDF/image models at startup so `/ready` flips only when the
    /// first conversion would be fast. Off: models load lazily on first use.
    pub warmup: bool,
    /// Allow `{"url": …}` inputs (outbound fetch — SSRF surface). Off by
    /// default: even with the built-in private/loopback/link-local IP guard,
    /// letting a caller name the fetch target is a deliberate exposure that a
    /// deployment must opt into (`--allow-url-fetch`).
    pub allow_url_fetch: bool,
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
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            addr: "127.0.0.1:5001".into(),
            concurrency: 2,
            max_body_bytes: 256 * 1024 * 1024,
            warmup: false,
            allow_url_fetch: false,
            strict: false,
            queue_size: 16,
            result_ttl_secs: 600,
            max_memory_mb: None,
        }
    }
}

struct AppState {
    /// Warm ML pipeline (mutable ONNX sessions) — one PDF/image conversion at
    /// a time, but the models stay loaded across requests.
    pipeline: Mutex<Option<(PipelineFlags, Pipeline)>>,
    /// Bounds total in-flight conversions (`Arc` so a permit can move into
    /// a streaming response's worker and outlive the handler).
    permits: Arc<Semaphore>,
    /// Async conversion jobs (#182), keyed by task id.
    jobs: Mutex<std::collections::HashMap<String, Job>>,
    ready: AtomicBool,
    /// The resolved memory ceiling (#263): the configured value, else the
    /// container's cgroup limit, else none. `0` disables.
    memory_ceiling_mb: Option<u64>,
    cfg: ServeConfig,
}

impl AppState {
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
    let state = Arc::new(AppState {
        pipeline: Mutex::new(None),
        permits: Arc::new(Semaphore::new(cfg.concurrency.max(1))),
        jobs: Mutex::new(std::collections::HashMap::new()),
        ready: AtomicBool::new(!cfg.warmup),
        memory_ceiling_mb,
        cfg: cfg.clone(),
    });
    if cfg.warmup {
        let st = state.clone();
        // Blocking model load off the runtime; readiness flips when done.
        tokio::task::spawn_blocking(move || {
            match Pipeline::new() {
                Ok(p) => *st.pipeline.lock().unwrap() = Some((PipelineFlags::default(), p)),
                Err(e) => eprintln!("warmup: pipeline load failed: {e}"),
            }
            st.ready.store(true, Ordering::Release);
        });
    }
    Router::new()
        // Docs + test form, like the original docling-serve's playground.
        .route(
            "/",
            get(|| async { axum::response::Html(include_str!("index.html")) }),
        )
        // The page's logo and the machine-readable API description. Both are
        // baked into the binary, so a server needs no static-file directory.
        .route(
            "/logo.svg",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "image/svg+xml")],
                    include_str!("logo.svg"),
                )
            }),
        )
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
        .layer(DefaultBodyLimit::max(cfg.max_body_bytes))
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
    Json(json!({
        "allow_url_fetch": state.cfg.allow_url_fetch,
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
    }))
    .into_response()
}

async fn ready(State(state): State<Arc<AppState>>) -> Response {
    if state.ready.load(Ordering::Acquire) {
        Json(json!({"status": "ready"})).into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "warming_up"})),
        )
            .into_response()
    }
}

/// Request options, merged from query parameters and body fields.
#[derive(Clone, Debug, Default, Deserialize)]
struct ConvertOptions {
    to: Option<String>,
    strict: Option<bool>,
    images: Option<String>,
    no_ocr: Option<bool>,
    /// Keep layout + TableFormer, never OCR (#244) — docling's independent
    /// `do_ocr=False`. `no_ocr` (the whole-stack skip) wins when both are set.
    skip_ocr: Option<bool>,
    force_full_page_ocr: Option<bool>,
    no_table_former: Option<bool>,
    /// Keep text-panel pictures as pictures (#173): disable the #157 demotion
    /// of uncaptioned dense-text "picture" regions into paragraphs.
    no_text_panels: Option<bool>,
    /// Infer PDF/image section-header levels after assembly (#302, docling's
    /// `HeadingHierarchyModel`: bookmarks > numbering > font style). Off by
    /// default — headings then keep the flat level the pipeline emits.
    heading_hierarchy: Option<bool>,
    /// Opt-in enrichment models (#423), named as docling's
    /// `PdfPipelineOptions` flags (and Python docling-serve's options):
    /// classify pictures with DocumentFigureClassifier (26 classes → the JSON
    /// picture item's `classification` annotation), rewrite code blocks with
    /// CodeFormulaV2 (+ `code_language`), decode display formulas to LaTeX.
    /// Each enabled pass lazily loads its model on the first matching region;
    /// a missing model warns once and the pass is skipped.
    do_picture_classification: Option<bool>,
    do_code_enrichment: Option<bool>,
    do_formula_enrichment: Option<bool>,
    fetch_images: Option<bool>,
    /// Email (.eml/.msg): append an Attachments section — names and content
    /// types only, never the payload (#251).
    list_attachments: Option<bool>,
    /// Omit empty cells from sparse XLSX/XLS table grids (#271, opt-in
    /// docling.rs extension).
    skip_empty_cells: Option<bool>,
    /// Compact (unpadded) Markdown tables (#271, opt-in docling.rs extension).
    compact_tables: Option<bool>,
    /// Text inserted between pages in Markdown output — docling-serve's
    /// `md_page_break_placeholder` (docling-core's
    /// `MarkdownParams.page_break_placeholder`). Unset = no page breaks.
    md_page_break_placeholder: Option<String>,
    /// EBCDIC copybook layout (#252): inline `EbcdicLayout` JSON (uploads
    /// have no filesystem, so the JSON itself rides in the request).
    ebcdic_layout: Option<String>,
    asr_model: Option<String>,
    /// ASR transcription language for audio/video input: a Whisper code
    /// (`en`, `de`, …) or `auto` (default) — detected from the first
    /// 30 seconds. Unknown codes fail the conversion with a clear error.
    asr_lang: Option<String>,
    /// Character encoding of text inputs (Markdown, CSV, AsciiDoc, WebVTT,
    /// XML, …) — docling's `TextBackendOptions.encoding`: a WHATWG label
    /// (`shift_jis`, `koi8-r`, `windows-1251`). Unset = detect (BOM, UTF-8,
    /// then windows-1252); bytes it cannot decode fail the request.
    encoding: Option<String>,
    /// Max frames sampled from a video input (0 = transcript only; needs the
    /// server to have the ffmpeg binary).
    video_frames: Option<usize>,
    /// PDF page window, `"A-B"` or a single `"N"` (1-based inclusive — #80).
    pages: Option<String>,
    /// OCR recognition language for scanned pages: `en` (default) | `ch`.
    ocr_lang: Option<String>,
    /// Which regions feed the OCR (docling's `OcrMode`, #254): `default` |
    /// `full_page` | `layout_regions` | `pdf_aware_layout_regions`.
    ocr_mode: Option<String>,
    /// OCR render scale in px per PDF point (docling's `OcrOptions.scale`,
    /// #254); unset reads the pipeline's own 2.0 px/pt render.
    ocr_scale: Option<f32>,
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
    /// Conversion pipeline (#304): `standard` (default) or `vlm` — every PDF
    /// page / image goes to a remote OpenAI-compatible vision model instead of
    /// the local ML stack. With `standard` the `vlm_*` options below are
    /// ignored, not rejected (the Node bindings' contract).
    pipeline: Option<String>,
    /// VLM endpoint for `pipeline=vlm`. A request-supplied endpoint is an
    /// outbound-URL/SSRF surface: it needs `--allow-url-fetch` and passes the
    /// same private-address check as URL inputs. Unset falls back to the
    /// operator-pinned `DOCLING_RS_VLM_ENDPOINT` — the safer default: callers
    /// pick the pipeline, the operator picks where it talks to.
    vlm_endpoint: Option<String>,
    /// VLM model name; falls back to `DOCLING_RS_VLM_MODEL`.
    vlm_model: Option<String>,
    /// Bearer token for the endpoint; falls back to `DOCLING_RS_VLM_API_KEY`.
    vlm_api_key: Option<String>,
    /// Per-page instruction; falls back to `DOCLING_RS_VLM_PROMPT`, else
    /// docling's DocLang-eliciting default prompt.
    vlm_prompt: Option<String>,
    /// `max_tokens` per completion (default 8192).
    vlm_max_tokens: Option<usize>,
}

impl ConvertOptions {
    fn merge_over(self, base: ConvertOptions) -> ConvertOptions {
        ConvertOptions {
            to: self.to.or(base.to),
            strict: self.strict.or(base.strict),
            images: self.images.or(base.images),
            no_ocr: self.no_ocr.or(base.no_ocr),
            skip_ocr: self.skip_ocr.or(base.skip_ocr),
            force_full_page_ocr: self.force_full_page_ocr.or(base.force_full_page_ocr),
            no_table_former: self.no_table_former.or(base.no_table_former),
            no_text_panels: self.no_text_panels.or(base.no_text_panels),
            heading_hierarchy: self.heading_hierarchy.or(base.heading_hierarchy),
            do_picture_classification: self
                .do_picture_classification
                .or(base.do_picture_classification),
            do_code_enrichment: self.do_code_enrichment.or(base.do_code_enrichment),
            do_formula_enrichment: self.do_formula_enrichment.or(base.do_formula_enrichment),
            fetch_images: self.fetch_images.or(base.fetch_images),
            list_attachments: self.list_attachments.or(base.list_attachments),
            skip_empty_cells: self.skip_empty_cells.or(base.skip_empty_cells),
            compact_tables: self.compact_tables.or(base.compact_tables),
            md_page_break_placeholder: self
                .md_page_break_placeholder
                .or(base.md_page_break_placeholder),
            ebcdic_layout: self.ebcdic_layout.or(base.ebcdic_layout),
            asr_model: self.asr_model.or(base.asr_model),
            asr_lang: self.asr_lang.or(base.asr_lang),
            encoding: self.encoding.or(base.encoding),
            video_frames: self.video_frames.or(base.video_frames),
            pages: self.pages.or(base.pages),
            ocr_lang: self.ocr_lang.or(base.ocr_lang),
            ocr_mode: self.ocr_mode.or(base.ocr_mode),
            ocr_scale: self.ocr_scale.or(base.ocr_scale),
            scale: self.scale.or(base.scale),
            chunker: self.chunker.or(base.chunker),
            chunk_tokenizer: self.chunk_tokenizer.or(base.chunk_tokenizer),
            chunk_max_tokens: self.chunk_max_tokens.or(base.chunk_max_tokens),
            chunk_merge_peers: self.chunk_merge_peers.or(base.chunk_merge_peers),
            pipeline: self.pipeline.or(base.pipeline),
            vlm_endpoint: self.vlm_endpoint.or(base.vlm_endpoint),
            vlm_model: self.vlm_model.or(base.vlm_model),
            vlm_api_key: self.vlm_api_key.or(base.vlm_api_key),
            vlm_prompt: self.vlm_prompt.or(base.vlm_prompt),
            vlm_max_tokens: self.vlm_max_tokens.or(base.vlm_max_tokens),
        }
    }
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

enum ApiError {
    Bad(String),
    Unsupported(String),
    Internal(String),
    /// The async job queue is full (#182) — retry later.
    Busy(String),
    /// The memory ceiling's watermark is crossed (#263) — 503, retry later.
    Overloaded(String),
}

/// The HTTP status + message an [`ApiError`] answers with (also stored on a
/// failed async job so `/v1/result/{id}` reproduces the sync status).
fn api_error_parts(e: ApiError) -> (StatusCode, String) {
    match e {
        ApiError::Bad(m) => (StatusCode::BAD_REQUEST, m),
        ApiError::Unsupported(m) => (StatusCode::UNPROCESSABLE_ENTITY, m),
        ApiError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        ApiError::Busy(m) => (StatusCode::TOO_MANY_REQUESTS, m),
        ApiError::Overloaded(m) => (StatusCode::SERVICE_UNAVAILABLE, m),
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let overloaded = matches!(self, ApiError::Overloaded(_));
        let (status, msg) = api_error_parts(self);
        let mut response = (status, Json(json!({"error": msg}))).into_response();
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
            .map_err(|e| ApiError::Bad(format!("bad body: {e}")))?;
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
            } => {
                let item = match docling::base64::decode(base64_string.trim()) {
                    Some(bytes) => {
                        source_from_named_bytes(&filename, bytes).map_err(|e| (filename.clone(), e))
                    }
                    None => Err((
                        filename.clone(),
                        ApiError::Bad(format!("file source {filename:?}: bad base64")),
                    )),
                };
                items.push(item);
            }
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
                    items.push(source_from_named_bytes(&name, bytes).map_err(|e| (name, e)));
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
    if !matches!(
        to.as_str(),
        "md" | "markdown" | "json" | "dclx" | "chunks" | "images" | "latex"
    ) {
        return Err(ApiError::Bad(format!(
            "unknown to='{to}' (expected: md, json, dclx, chunks, images, latex)"
        )));
    }
    let image_mode = match options.images.as_deref().unwrap_or("placeholder") {
        "placeholder" => ImageMode::Placeholder,
        "embedded" => ImageMode::Embedded,
        other => {
            return Err(ApiError::Bad(format!(
                "unknown images='{other}' (expected: placeholder, embedded)"
            )))
        }
    };
    // OCR mode/scale (#254) validate here too — before the conversion starts —
    // because the streaming path flattens later errors into a mid-stream 422,
    // and a bad option deserves a plain 400 up front.
    parse_ocr_mode(options.ocr_mode.as_deref())?;
    parse_ocr_scale(options.ocr_scale)?;
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
    Query(query): Query<ConvertOptions>,
    headers: HeaderMap,
    body: axum::extract::Request,
) -> Result<Response, ApiError> {
    let (sources, options, target) = parse_convert_request(&state, query, headers, body).await?;
    let (to, image_mode) = validate_output(&options)?;
    // #304: bad pipeline/vlm options are a 400/422 up front (no DNS here —
    // the endpoint's SSRF resolution check runs on the blocking pool).
    resolve_vlm_options(&state, &options, false)?;
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

    // A single Markdown conversion streams; everything else (other formats,
    // #182 batches — which need per-item framing, and cloud targets — whose
    // outputs leave through the store, #139) buffers.
    let is_markdown = matches!(to.as_str(), "md" | "markdown");
    if is_markdown && sources.len() == 1 && target.is_none() {
        let source = sources
            .into_iter()
            .next()
            .expect("checked len")
            .map_err(|(_, e)| e)?;
        return stream_markdown(state.clone(), source, options, image_mode, permit).await;
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
}

enum JobState {
    /// Waiting for a conversion slot (the shared semaphore).
    Pending,
    Started,
    Success(StoredResponse),
    /// The HTTP status the sync endpoint would have answered with, plus the
    /// error message.
    Failure(StatusCode, String),
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
fn purge_expired(jobs: &mut std::collections::HashMap<String, Job>, ttl_secs: u64) {
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
    Query(query): Query<ConvertOptions>,
    headers: HeaderMap,
    body: axum::extract::Request,
) -> Result<Response, ApiError> {
    let (mut sources, options, target) =
        parse_convert_request(&state, query, headers, body).await?;
    let (to, image_mode) = validate_output(&options)?;
    // #304: fail an async submission fast instead of parking a doomed job.
    resolve_vlm_options(&state, &options, false)?;
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
        if let Some(job) = st.jobs.lock().unwrap().get_mut(&job_id) {
            job.state = JobState::Started;
        } else {
            return; // evicted while queued (TTL abuse would need days)
        }
        let stx = st.clone();
        let opts = options;
        let rt = tokio::runtime::Handle::current();
        let outcome = tokio::task::spawn_blocking(move || {
            let out = run_conversion(&stx, sources, &opts, &to, image_mode, target.as_ref(), &rt);
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
                    let (status, msg) = api_error_parts(e);
                    JobState::Failure(status, msg)
                }
            };
            job.done_at = Some(std::time::Instant::now());
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "task_id": id, "task_status": "pending" })),
    )
        .into_response())
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
            if let JobState::Failure(_, msg) = &job.state {
                body["error"] = json!(msg);
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
            JobState::Failure(status, msg) => {
                (*status, Json(json!({"error": msg}))).into_response()
            }
            // Clone rather than remove: the result stays re-fetchable until
            // the TTL evicts it (a client retrying a dropped response must not
            // find a 404).
            JobState::Success(stored) => StoredResponse {
                content_type: stored.content_type,
                disposition: stored.disposition.clone(),
                confidence: stored.confidence.clone(),
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
    body: Vec<u8>,
}

impl StoredResponse {
    fn into_response(self) -> Response {
        let mut response = ([(header::CONTENT_TYPE, self.content_type)], self.body).into_response();
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
        response
    }
}

/// Convert one or more sources on the current (blocking) thread and serialize
/// the result. A single source renders as the plain output format; multiple
/// sources (#182 batch) render as a JSON results array with per-item status,
/// so one bad file fails its item, not the whole batch. With a cloud
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
    if sources.len() == 1 {
        let source = sources
            .into_iter()
            .next()
            .expect("checked len")
            .map_err(|(_, e)| e)?;
        if to == "images" {
            let pages = rasterize_pages(state, &source, options)?;
            return Ok(StoredResponse {
                content_type: "application/json",
                disposition: None,
                confidence: None,
                body: serde_json::to_vec_pretty(&json!({ "pages": pages }))
                    .expect("page JSON serializes"),
            });
        }
        let name = source.name.clone();
        let document = convert_document(state, source, options)?;
        return render_stored(state, to, image_mode, &name, &document, options);
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
                Ok(document) => batch_item(state, to, image_mode, &name, &document, options),
                Err(e) => json!({
                    "name": name,
                    "status": "failure",
                    "error": api_error_message(e),
                }),
            }
        })
        .collect();
    Ok(StoredResponse {
        content_type: "application/json",
        disposition: None,
        confidence: None,
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
            "chunks" => "chunks.json",
            "dclx" => "dclx",
            "latex" => "tex",
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
                .and_then(|doc| render_stored(state, to, image_mode, &name, &doc, options));
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
        content_type: "application/json",
        disposition: None,
        confidence: None,
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
                let rendered = convert_document(state, source, options)
                    .and_then(|doc| render_stored(state, to, image_mode, &name, &doc, options));
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
        content_type: "application/zip",
        disposition: Some(format!("attachment; filename=\"{file_name}\"")),
        confidence: None,
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
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .max_redirects(0)
        .timeout_connect(Some(std::time::Duration::from_secs(10)))
        .timeout_global(Some(std::time::Duration::from_secs(300)))
        .build()
        .into();
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
                .and_then(|doc| render_stored(state, to, image_mode, &name, &doc, options));
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
        content_type: "application/json",
        disposition: None,
        confidence: None,
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
/// of that. `malloc_trim` walks the arenas and gives the free pages back;
/// admission control (and the operator's dashboards) then see the truth.
/// glibc-Linux only — a no-op elsewhere (musl/mac allocators don't have the
/// retention pattern to the same degree, and no trim call to offer).
fn trim_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::malloc_trim(0);
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

/// `to=images` (#243): rasterize a PDF's pages to PNG through pdfium — no
/// models, no OCR — honoring the request's `pages` window and `scale`. Returns
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
    let range = options
        .pages
        .as_deref()
        .map(docling::parse_page_range)
        .transpose()
        .map_err(|e| ApiError::Bad(format!("pages: {e}")))?;
    // Enforce the page cap before rendering anything. Count with the window
    // applied — pages=A-B is exactly the documented way to rasterize a slice
    // of a document that exceeds the cap.
    let total = docling::pdf_page_count(&source.bytes, None)
        .map_err(|e| ApiError::Unsupported(e.to_string()))?;
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
    // pdfium is not thread-safe, and the warm pipeline's mutex is this
    // process's "who owns pdfium" lock — hold it for the render even though no
    // models run here, so a rasterization can't race a concurrent PDF/image
    // conversion inside pdfium.
    let _pdfium_owner = state
        .pipeline
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let pages = docling::render_pdf_pages(&source.bytes, None, range, scale)
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
            content_type: "text/markdown; charset=utf-8",
            disposition: None,
            confidence,
            body: markdown_string(state, document, image_mode, options).into_bytes(),
        },
        "json" => {
            let mut value = document.export_to_json_value();
            if let Some(report) = &document.confidence {
                value["confidence"] = report.to_json();
            }
            StoredResponse {
                content_type: "application/json",
                disposition: None,
                confidence,
                body: serde_json::to_vec_pretty(&value).expect("document JSON serializes"),
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
                content_type: "application/json",
                disposition: None,
                confidence,
                body: serde_json::to_vec(&records).expect("chunk records serialize"),
            }
        }
        "dclx" => StoredResponse {
            content_type: "application/octet-stream",
            disposition: Some(format!("attachment; filename=\"{name}.dclx\"")),
            confidence,
            body: docling::dclx::to_dclx_bytes(document),
        },
        // #317: a text body like Markdown (no download disposition).
        "latex" => StoredResponse {
            content_type: "text/x-tex; charset=utf-8",
            disposition: None,
            confidence,
            body: document.export_to_latex().into_bytes(),
        },
        _ => unreachable!("validated above"),
    })
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
    doc.strict_markdown = options.strict.unwrap_or(state.cfg.strict);
    doc.page_break_placeholder = options.md_page_break_placeholder.clone();
    match image_mode {
        ImageMode::Placeholder => doc.export_to_markdown(),
        _ => {
            doc.export_to_markdown_with_images(image_mode, "artifacts")
                .0
        }
    }
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
        .map_err(|e| ApiError::Bad(format!("bad multipart field: {e}")))?
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
                    .map_err(|e| ApiError::Bad(format!("reading upload: {e}")))?;
                files.push((file_name, bytes.to_vec()));
            }
            "to" | "images" => {
                let v = text_field(field).await?;
                match name.as_str() {
                    "to" => body_opts.to = Some(v),
                    _ => body_opts.images = Some(v),
                }
            }
            "asr_model" => body_opts.asr_model = Some(text_field(field).await?),
            "asr_lang" => body_opts.asr_lang = Some(text_field(field).await?),
            "encoding" => body_opts.encoding = Some(text_field(field).await?),
            "pages" => body_opts.pages = Some(text_field(field).await?),
            "ocr_lang" => body_opts.ocr_lang = Some(text_field(field).await?),
            "ocr_mode" => body_opts.ocr_mode = Some(text_field(field).await?),
            "ocr_scale" => {
                let v = text_field(field).await?;
                body_opts.ocr_scale = Some(v.parse().map_err(|_| {
                    ApiError::Bad(format!("ocr_scale must be a number, got {v:?}"))
                })?);
            }
            "ebcdic_layout" => body_opts.ebcdic_layout = Some(text_field(field).await?),
            "md_page_break_placeholder" => {
                body_opts.md_page_break_placeholder = Some(text_field(field).await?)
            }
            "pipeline" => body_opts.pipeline = Some(text_field(field).await?),
            "vlm_endpoint" => body_opts.vlm_endpoint = Some(text_field(field).await?),
            "vlm_model" => body_opts.vlm_model = Some(text_field(field).await?),
            "vlm_api_key" => body_opts.vlm_api_key = Some(text_field(field).await?),
            "vlm_prompt" => body_opts.vlm_prompt = Some(text_field(field).await?),
            "vlm_max_tokens" => {
                let v = text_field(field).await?;
                body_opts.vlm_max_tokens = Some(v.parse().map_err(|_| {
                    ApiError::Bad(format!(
                        "vlm_max_tokens must be a positive integer, got {v:?}"
                    ))
                })?);
            }
            "chunker" => body_opts.chunker = Some(text_field(field).await?),
            "chunk_tokenizer" => body_opts.chunk_tokenizer = Some(text_field(field).await?),
            "chunk_max_tokens" => {
                let v = text_field(field).await?;
                body_opts.chunk_max_tokens = Some(v.parse().map_err(|_| {
                    ApiError::Bad(format!(
                        "chunk_max_tokens must be a positive integer, got {v:?}"
                    ))
                })?);
            }
            "scale" => {
                let v = text_field(field).await?;
                body_opts.scale =
                    Some(v.parse().map_err(|_| {
                        ApiError::Bad(format!("scale must be a number, got {v:?}"))
                    })?);
            }
            "video_frames" => {
                let v = text_field(field).await?;
                body_opts.video_frames = Some(v.parse().map_err(|_| {
                    ApiError::Bad(format!(
                        "video_frames must be a non-negative integer, got {v:?}"
                    ))
                })?);
            }
            "strict"
            | "no_ocr"
            | "skip_ocr"
            | "no_table_former"
            | "force_full_page_ocr"
            | "no_text_panels"
            | "heading_hierarchy"
            | "do_picture_classification"
            | "do_code_enrichment"
            | "do_formula_enrichment"
            | "fetch_images"
            | "list_attachments"
            | "skip_empty_cells"
            | "compact_tables"
            | "chunk_merge_peers" => {
                let v = text_field(field).await?;
                let b = matches!(v.as_str(), "1" | "true" | "yes" | "on");
                match name.as_str() {
                    "strict" => body_opts.strict = Some(b),
                    "no_ocr" => body_opts.no_ocr = Some(b),
                    "skip_ocr" => body_opts.skip_ocr = Some(b),
                    "force_full_page_ocr" => body_opts.force_full_page_ocr = Some(b),
                    "no_table_former" => body_opts.no_table_former = Some(b),
                    "no_text_panels" => body_opts.no_text_panels = Some(b),
                    "heading_hierarchy" => body_opts.heading_hierarchy = Some(b),
                    "do_picture_classification" => body_opts.do_picture_classification = Some(b),
                    "do_code_enrichment" => body_opts.do_code_enrichment = Some(b),
                    "do_formula_enrichment" => body_opts.do_formula_enrichment = Some(b),
                    "list_attachments" => body_opts.list_attachments = Some(b),
                    "skip_empty_cells" => body_opts.skip_empty_cells = Some(b),
                    "compact_tables" => body_opts.compact_tables = Some(b),
                    "chunk_merge_peers" => body_opts.chunk_merge_peers = Some(b),
                    _ => body_opts.fetch_images = Some(b),
                }
            }
            _ => {} // unknown parts are ignored
        }
    }
    if files.is_empty() {
        return Err(ApiError::Bad("missing 'file' part".into()));
    }
    // Per-file errors (unknown extension) are deferred: a single-file request
    // propagates them as its response, a batch fails only that item.
    let sources = files
        .into_iter()
        .map(|(file_name, bytes)| {
            source_from_named_bytes(&file_name, bytes).map_err(|e| (file_name, e))
        })
        .collect();
    Ok((sources, body_opts.merge_over(query)))
}

async fn text_field(field: axum::extract::multipart::Field<'_>) -> Result<String, ApiError> {
    field
        .text()
        .await
        .map_err(|e| ApiError::Bad(format!("reading field: {e}")))
}

/// Build a [`SourceDocument`] from a filename (extension → format) and bytes.
fn source_from_named_bytes(file_name: &str, bytes: Vec<u8>) -> Result<SourceDocument, ApiError> {
    source_from_named_bytes_ct(file_name, bytes, None)
}

/// As [`source_from_named_bytes`], with an optional response `Content-Type` used
/// as a fallback when the name carries no usable extension — a URL like
/// `…/help/example-domains` has no `.html`, but the server reports
/// `text/html`, so it still converts.
fn source_from_named_bytes_ct(
    file_name: &str,
    bytes: Vec<u8>,
    content_type: Option<&str>,
) -> Result<SourceDocument, ApiError> {
    let ext = std::path::Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str());
    let format = ext
        .and_then(InputFormat::from_extension)
        .or_else(|| content_type.and_then(format_from_content_type))
        // A DjVu upload with a bare/renamed file name and a generic
        // Content-Type is still unambiguous from its `AT&TFORM` magic (#434).
        .or_else(|| docling::backend::looks_like_djvu(&bytes).then_some(InputFormat::Djvu))
        .ok_or_else(|| match ext {
            Some(e) => ApiError::Unsupported(format!("unrecognized extension '.{e}'")),
            None => ApiError::Bad(format!(
                "cannot determine the format of '{file_name}': no file extension \
                 and no recognized Content-Type"
            )),
        })?;
    let stem = std::path::Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("document")
        .to_string();
    Ok(SourceDocument::from_bytes(stem, format, bytes))
}

/// Map an HTTP `Content-Type` (its media-type, parameters stripped) to an input
/// format — the common web types docling can convert. Anything else returns
/// `None` and the caller reports an unknown-format error.
fn format_from_content_type(content_type: &str) -> Option<InputFormat> {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    Some(match mime.as_str() {
        "text/html" | "application/xhtml+xml" => InputFormat::Html,
        "application/pdf" => InputFormat::Pdf,
        "text/markdown" | "text/plain" => InputFormat::Md,
        "text/csv" => InputFormat::Csv,
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            InputFormat::Docx
        }
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => {
            InputFormat::Pptx
        }
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => InputFormat::Xlsx,
        "application/epub+zip" => InputFormat::Epub,
        "image/jpeg" | "image/png" | "image/tiff" | "image/bmp" | "image/webp" => {
            InputFormat::Image
        }
        // SVG (#212) is its own format, not Image: the ML build rasterizes it
        // first, and OCR-less builds extract its <text> elements instead.
        "image/svg+xml" => InputFormat::Svg,
        // DjVu (#434): pure-Rust decode of the hidden text layer (OCR fallback
        // for scan-only pages). Both registered and legacy MIME spellings.
        "image/vnd.djvu" | "image/x-djvu" | "image/x.djvu" => InputFormat::Djvu,
        // Upstream's FormatToMimeType for AUDIO and VIDEO (docling v2.114).
        "audio/wav" | "audio/x-wav" | "audio/mpeg" | "audio/mp3" | "audio/mp4" | "audio/m4a"
        | "audio/aac" | "audio/ogg" | "audio/flac" | "audio/x-flac" => InputFormat::Audio,
        "video/mp4" | "video/avi" | "video/x-msvideo" | "video/quicktime" | "video/x-matroska"
        | "video/webm" => InputFormat::Video,
        _ => return None,
    })
}

/// Largest URL-fetch response accepted (256 MiB default). Unlike the
/// request-body limit, `read_to_end` on a fetched response is otherwise
/// unbounded — a hostile URL streaming an endless body would exhaust memory.
/// Override with `DOCLING_RS_MAX_FETCH_BYTES`.
fn max_fetch_bytes() -> u64 {
    docling_core::env::parse("DOCLING_RS_MAX_FETCH_BYTES").unwrap_or(256 * 1024 * 1024)
}

/// Escape hatch for local development: when `DOCLING_RS_ALLOW_PRIVATE_IP_FETCH`
/// is set to a truthy value, the SSRF IP block-list is not enforced, so a URL
/// like `http://localhost:8080/doc.pdf` can be fetched. Off by default —
/// leave it unset in production.
fn allow_private_ip_fetch() -> bool {
    docling_core::env::flag("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH")
}

/// Reject a resolved IP that points back into the local host or infrastructure.
/// This is the core SSRF guard: without it, `{"url": "http://169.254.169.254/…"}`
/// or `http://127.0.0.1:…` would let a caller reach cloud metadata and internal
/// services from the server's network position.
fn is_blocked_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                // Carrier-grade NAT 100.64.0.0/10.
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // Unique-local fc00::/7 and link-local fe80::/10.
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4-mapped (::ffff:a.b.c.d): re-check the embedded v4.
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_blocked_ip(IpAddr::V4(v4)))
        }
    }
}

/// SSRF pre-check shared by every request-supplied outbound URL — URL inputs
/// and the `vlm_endpoint` request option (#304): http(s) scheme only, then
/// resolve the host and reject if it maps to a private/loopback/link-local
/// address (fetches additionally forbid redirects — a public URL could
/// 30x-bounce to an internal target, defeating this pre-check). This is a
/// best-effort mitigation — a DNS-rebinding race between this resolution and
/// the eventual connect remains theoretically possible; the deployment-level
/// control is to leave URL fetch disabled unless the network is trusted.
/// Blocking (DNS) — call on the blocking pool.
fn check_outbound_url(url: &str) -> Result<(), ApiError> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(ApiError::Bad(format!("unsupported URL scheme in '{url}'")));
    }
    let parsed =
        url::Url::parse(url).map_err(|e| ApiError::Bad(format!("bad URL '{url}': {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| ApiError::Bad(format!("no host in URL '{url}'")))?;
    let port = parsed.port_or_known_default().unwrap_or(80);
    let mut resolved = (host, port)
        .to_socket_addrs()
        .map_err(|e| ApiError::Bad(format!("cannot resolve {host}: {e}")))?
        .peekable();
    if resolved.peek().is_none() {
        return Err(ApiError::Bad(format!("cannot resolve {host}")));
    }
    if !allow_private_ip_fetch() {
        for addr in resolved {
            if is_blocked_ip(addr.ip()) {
                return Err(ApiError::Bad(format!(
                    "refusing to fetch {url}: resolves to a private/loopback address \
                     (set DOCLING_RS_ALLOW_PRIVATE_IP_FETCH=1 for local development)"
                )));
            }
        }
    }
    Ok(())
}

/// Fetch a URL input (blocking; run on the blocking pool). The name comes
/// from `file_name` or the URL path's last segment.
fn fetch_url(
    url: &str,
    file_name: Option<&str>,
    headers: &[(String, String)],
) -> Result<SourceDocument, ApiError> {
    check_outbound_url(url)?;
    // Bounded in time as well as size: without timeouts a slow-drip URL pins
    // one spawn_blocking worker indefinitely, and a handful of them starves
    // the pool. Generous global cap — legitimate fetches may be ~256 MiB.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .max_redirects(0)
        .timeout_connect(Some(std::time::Duration::from_secs(10)))
        .timeout_global(Some(std::time::Duration::from_secs(300)))
        .build()
        .into();
    // Request headers ride along verbatim (docling's `HttpSource.headers`,
    // #139 — auth tokens for protected origins).
    let mut request = agent.get(url);
    for (k, v) in headers {
        request = request.header(k, v);
    }
    let mut response = request
        .call()
        .map_err(|e| ApiError::Bad(format!("fetching {url}: {e}")))?;
    // Kept for format detection when the URL/name has no usable extension.
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let max_fetch = max_fetch_bytes();
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(max_fetch + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| ApiError::Bad(format!("reading {url}: {e}")))?;
    if bytes.len() as u64 > max_fetch {
        return Err(ApiError::Bad(format!(
            "response from {url} exceeds {max_fetch} bytes"
        )));
    }
    let name = file_name
        .map(|s| s.to_string())
        .or_else(|| {
            url.split('/')
                .next_back()
                .map(|s| s.split(['?', '#']).next().unwrap_or(s).to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "document".to_string());
    // Record the fetch URL as the document's base URL so relative `<img src>`
    // on a fetched web page resolve against its origin when fetch_images is on.
    Ok(source_from_named_bytes_ct(&name, bytes, content_type.as_deref())?.with_base_url(url))
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
) -> Result<DoclingDocument, ApiError> {
    let result = convert_document_inner(state, source, options);
    o11y::record_conversion(result.is_ok());
    result
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
    let set = |s: &Option<String>| s.clone().filter(|v| !v.trim().is_empty());
    match options.pipeline.as_deref() {
        None | Some("standard") => Ok(None),
        Some("vlm") => {
            let endpoint = set(&options.vlm_endpoint);
            if let Some(url) = endpoint.as_deref() {
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
            let mut v = docling::vlm::VlmOptions::resolve(endpoint, set(&options.vlm_model))
                .map_err(|e| {
                    // The library's message names the CLI flags; this surface's
                    // spelling is the request options.
                    ApiError::Bad(
                        e.to_string()
                            .replace("pass --vlm-endpoint", "pass vlm_endpoint")
                            .replace("pass --vlm-model", "pass vlm_model"),
                    )
                })?;
            if let Some(p) = set(&options.vlm_prompt) {
                v.prompt = Some(p);
            }
            if let Some(k) = set(&options.vlm_api_key) {
                v.api_key = Some(k);
            }
            // Validated like the Node bindings' `vlmMaxTokens`: 0 would have
            // every page come back empty and surface as a model error.
            match options.vlm_max_tokens {
                Some(0) => {
                    return Err(ApiError::Bad(
                        "vlm_max_tokens must be greater than 0".into(),
                    ))
                }
                Some(n) => v.max_tokens = n,
                None => {}
            }
            // `pages` composes with the VLM exactly as with the ML pipeline —
            // only the selected pages are rendered and sent.
            v.page_range = options
                .pages
                .as_deref()
                .map(docling::parse_page_range)
                .transpose()
                .map_err(|e| ApiError::Bad(format!("pages: {e}")))?;
            Ok(Some(v))
        }
        Some(other) => Err(ApiError::Bad(format!(
            "unknown pipeline '{other}' (expected: standard, vlm)"
        ))),
    }
}

fn convert_document_inner(
    state: &AppState,
    source: SourceDocument,
    options: &ConvertOptions,
) -> Result<DoclingDocument, ApiError> {
    // #304: the VLM pipeline is a sibling path — the remote model does the
    // reading, none of the local ML options apply. Resolved here (on the
    // blocking pool, where the endpoint's DNS check belongs) so every
    // buffered surface — sync, batch item, async job, cloud target — and the
    // streaming path's buffered branch pick it up in one place. A VLM failure
    // is a per-request error like any other, never a server crash.
    if let Some(vlm) = resolve_vlm_options(state, options, true)? {
        return docling::vlm::convert_vlm(&source, &vlm)
            .map_err(|e| ApiError::Unsupported(e.to_string()));
    }
    match source.format {
        InputFormat::Pdf | InputFormat::Image => {
            // Recover from a poisoned lock instead of propagating the panic: a
            // single crafted PDF/image that panics inside `convert` below drops
            // the guard mid-unwind and poisons the mutex. Without this recovery
            // every later request would panic on `.lock().unwrap()` too, turning
            // one bad document into a permanent outage of this endpoint. The
            // pipeline state is rebuilt/validated by `warm_pipeline`, so reusing
            // it after a panic is safe.
            let mut guard = state
                .pipeline
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let pipeline = warm_pipeline(&mut guard, options)?;
            // The page window (#80) is per-request configuration on the shared
            // warm pipeline — set it unconditionally so no request inherits a
            // previous one's window. Images are single-page; no window.
            let range = options
                .pages
                .as_deref()
                .map(docling::parse_page_range)
                .transpose()
                .map_err(|e| ApiError::Bad(format!("pages: {e}")))?;
            pipeline.set_pages(range);
            // OCR language likewise applies per request; only a worker whose
            // cached recognition model mismatches actually reloads anything.
            pipeline.set_ocr_lang(parse_ocr_lang(options.ocr_lang.as_deref())?);
            // Forcing, mode and scale (#254) are pure per-worker configuration
            // — set unconditionally like the page window. (This also makes
            // `force_full_page_ocr` effective on the warm path at all: it was
            // only ever applied to the declarative converter before, which
            // PDFs never route through.)
            pipeline.set_force_full_page_ocr(options.force_full_page_ocr.unwrap_or(false));
            pipeline.set_ocr_mode(parse_ocr_mode(options.ocr_mode.as_deref())?);
            pipeline.set_ocr_scale(parse_ocr_scale(options.ocr_scale)?);
            // #302: pure per-request post-processing configuration, set
            // unconditionally like the page window.
            pipeline.set_heading_hierarchy(docling::HeadingHierarchyOptions::enabled(
                options.heading_hierarchy.unwrap_or(false),
            ));
            let doc = match source.format {
                InputFormat::Pdf => pipeline.convert(&source.bytes, None, &source.name),
                _ => pipeline.convert_image(&source.bytes, &source.name),
            }
            .map_err(|e| ApiError::Internal(e.to_string()))?;
            Ok(doc)
        }
        _ => {
            let converter = request_converter(state, options)?;
            converter
                .convert(source)
                .map(|r| r.document)
                .map_err(|e| ApiError::Unsupported(e.to_string()))
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
    no_ocr: bool,
    skip_ocr: bool,
    no_table_former: bool,
    no_text_panels: bool,
    enrich: docling::EnrichmentOptions,
}

impl PipelineFlags {
    fn of(options: &ConvertOptions) -> Self {
        Self {
            no_ocr: options.no_ocr.unwrap_or(false),
            skip_ocr: options.skip_ocr.unwrap_or(false),
            no_table_former: options.no_table_former.unwrap_or(false),
            no_text_panels: options.no_text_panels.unwrap_or(false),
            enrich: enrichments(options),
        }
    }
}

/// The request's enrichment passes (#423), all off unless asked for.
fn enrichments(options: &ConvertOptions) -> docling::EnrichmentOptions {
    docling::EnrichmentOptions {
        picture_classification: options.do_picture_classification.unwrap_or(false),
        code: options.do_code_enrichment.unwrap_or(false),
        formula: options.do_formula_enrichment.unwrap_or(false),
    }
}

/// The lazily-loaded warm pipeline. Pipeline switches (`no_ocr`, `skip_ocr`,
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
            .no_ocr(flags.no_ocr)
            .skip_ocr(flags.skip_ocr)
            .no_table_former(flags.no_table_former)
            .no_text_panels(flags.no_text_panels)
            .enrichments(flags.enrich);
        *slot = Some((flags, p));
    }
    Ok(&mut slot.as_mut().expect("just filled").1)
}

/// Per-request declarative converter (construction is cheap — it's
/// configuration, models don't apply).
fn request_converter(
    state: &AppState,
    options: &ConvertOptions,
) -> Result<DocumentConverter, ApiError> {
    let mut converter = DocumentConverter::new()
        .strict(options.strict.unwrap_or(state.cfg.strict))
        // `fetch_images` pulls external `<img src>` over the network — the same
        // outbound-fetch / SSRF surface as URL inputs, so it lives behind the
        // same `--allow-url-fetch` gate. Off by default, it's silently ignored
        // rather than honored (the UI greys the box; an API caller just gets
        // placeholder images instead of a surprise outbound fetch).
        .fetch_images(state.cfg.allow_url_fetch && options.fetch_images.unwrap_or(false))
        .list_attachments(options.list_attachments.unwrap_or(false))
        .skip_empty_cells(options.skip_empty_cells.unwrap_or(false))
        .compact_tables(options.compact_tables.unwrap_or(false))
        .page_break_placeholder(options.md_page_break_placeholder.clone())
        .ebcdic_layout_opt(options.ebcdic_layout.clone())
        .asr_model(options.asr_model.clone())
        .asr_lang(options.asr_lang.clone())
        .encoding(options.encoding.clone())
        .video_frames(
            options
                .video_frames
                .unwrap_or(docling::DEFAULT_VIDEO_FRAMES),
        )
        .no_ocr(options.no_ocr.unwrap_or(false))
        .skip_ocr(options.skip_ocr.unwrap_or(false))
        .force_full_page_ocr(options.force_full_page_ocr.unwrap_or(false))
        .no_table_former(options.no_table_former.unwrap_or(false))
        .no_text_panels(options.no_text_panels.unwrap_or(false))
        .heading_hierarchy(options.heading_hierarchy.unwrap_or(false))
        .do_picture_classification(options.do_picture_classification.unwrap_or(false))
        .do_code_enrichment(options.do_code_enrichment.unwrap_or(false))
        .do_formula_enrichment(options.do_formula_enrichment.unwrap_or(false));
    if let Some(pages) = &options.pages {
        let (first, last) =
            docling::parse_page_range(pages).map_err(|e| ApiError::Bad(format!("pages: {e}")))?;
        converter = converter.page_range(first, last);
    }
    if parse_ocr_lang(options.ocr_lang.as_deref())?.is_some() {
        converter = converter.ocr_lang(options.ocr_lang.clone().expect("checked above"));
    }
    // #254: the converter path reaches the ML pipeline too (rasterized SVG),
    // so mode/scale plumb here as well — validated up front like ocr_lang.
    if parse_ocr_mode(options.ocr_mode.as_deref())?.is_some() {
        converter = converter.ocr_mode(options.ocr_mode.clone().expect("checked above"));
    }
    if let Some(s) = parse_ocr_scale(options.ocr_scale)? {
        converter = converter.ocr_scale(s);
    }
    Ok(converter)
}

/// Validate a request's `ocr_mode` (#254; None passes through — the engine
/// default).
fn parse_ocr_mode(raw: Option<&str>) -> Result<Option<docling::OcrMode>, ApiError> {
    raw.map(|v| {
        docling::OcrMode::parse(v).ok_or_else(|| {
            ApiError::Bad(format!(
                "ocr_mode {v:?} is not \
                 default|full_page|layout_regions|pdf_aware_layout_regions"
            ))
        })
    })
    .transpose()
}

/// Validate a request's `ocr_scale` (#254; None passes through — the engine
/// default).
fn parse_ocr_scale(raw: Option<f32>) -> Result<Option<f32>, ApiError> {
    match raw {
        Some(s) if !(s.is_finite() && s > 0.0) => Err(ApiError::Bad(format!(
            "ocr_scale must be a positive number, got {s}"
        ))),
        other => Ok(other),
    }
}

/// Validate a request's `ocr_lang` (None passes through — the engine default).
fn parse_ocr_lang(raw: Option<&str>) -> Result<Option<docling::OcrLang>, ApiError> {
    raw.map(|v| {
        docling::OcrLang::parse(v).ok_or_else(|| {
            ApiError::Bad(format!(
                "ocr_lang {v:?} is not a supported OCR language ({})",
                docling::OcrLang::ACCEPTED
            ))
        })
    })
    .transpose()
}

/// Markdown response: converted through the streaming serializer, body sent
/// chunked as pages finish. The semaphore permit moves into the worker so the
/// slot stays held until the stream ends.
async fn stream_markdown(
    state: Arc<AppState>,
    source: SourceDocument,
    options: ConvertOptions,
    image_mode: ImageMode,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<Response, ApiError> {
    // A chunk is its text plus (first chunk of a pipeline conversion only) the
    // confidence summary header — computable only after conversion, which is
    // exactly when the PDF/image branch sends its single chunk.
    type Chunk = (String, Option<header::HeaderValue>);
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
        let send = |item: Result<Chunk, ApiError>| {
            // The receiver disappearing means the client went away — stop.
            tx.blocking_send(item).is_ok()
        };
        // #304: the VLM pipeline buffers whatever the input format — it is a
        // per-page remote conversion, and its "wrong input format" rejection
        // must reach the client instead of the declarative streamer running a
        // standard conversion the caller didn't ask for.
        let buffered = options.pipeline.as_deref() == Some("vlm")
            || matches!(source.format, InputFormat::Pdf | InputFormat::Image);
        if buffered {
            // Buffered document → streamed serialization is pointless for
            // images (one step); PDFs stream page by page through the
            // warm pipeline's converter equivalent: convert, then stream
            // the serializer output. (True page-by-page pipeline
            // streaming holds the model mutex anyway, so the wall-clock
            // is the same; the client still gets incremental output.)
            match convert_document(&st, source, &options) {
                Ok(mut doc) => {
                    doc.strict_markdown = options.strict.unwrap_or(st.cfg.strict);
                    doc.page_break_placeholder = options.md_page_break_placeholder.clone();
                    let md = match image_mode {
                        ImageMode::Placeholder => doc.export_to_markdown(),
                        _ => {
                            doc.export_to_markdown_with_images(image_mode, "artifacts")
                                .0
                        }
                    };
                    send(Ok((md, confidence_header(&doc))));
                }
                Err(e) => {
                    send(Err(e));
                }
            }
        } else {
            let converter = match request_converter(&st, &options) {
                Ok(c) => c,
                Err(e) => {
                    send(Err(e));
                    return;
                }
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
                                if !send(Ok((s, None))) {
                                    return;
                                }
                            }
                            Err(e) => {
                                o11y::record_conversion(false);
                                send(Err(conversion_api_error(&e)));
                                return;
                            }
                        }
                    }
                    o11y::record_conversion(true);
                }
                Err(e) => {
                    o11y::record_conversion(false);
                    send(Err(conversion_api_error(&e)));
                }
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
        Some(Ok((first_chunk, confidence))) => {
            use tokio_stream::StreamExt;
            let rest = tokio_stream::wrappers::ReceiverStream::new(rx);
            let stream = tokio_stream::once(Ok((first_chunk, None)))
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
            if let Some(value) = confidence {
                response.headers_mut().insert("x-docling-confidence", value);
            }
            Ok(response)
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
        other => ApiError::Unsupported(other.to_string()),
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
    /// `no_ocr=true` request permanently degraded the shared instance for
    /// every later default request. (`Pipeline::new()` loads no models —
    /// they're lazy — so this runs in plain CI.)
    #[test]
    fn warm_pipeline_rebuilds_in_both_directions() {
        let mut slot = None;
        let default_opts = ConvertOptions::default();
        let no_ocr_opts = ConvertOptions {
            no_ocr: Some(true),
            ..ConvertOptions::default()
        };
        assert!(warm_pipeline(&mut slot, &no_ocr_opts).is_ok());
        assert_eq!(slot.as_ref().unwrap().0, PipelineFlags::of(&no_ocr_opts));
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
            do_picture_classification: Some(true),
            do_formula_enrichment: Some(true),
            ..ConvertOptions::default()
        };
        assert!(warm_pipeline(&mut slot, &enriched).is_ok());
        let built = slot.as_ref().unwrap().0;
        assert!(built.enrich.picture_classification && built.enrich.formula);
        assert!(!built.enrich.code);
        assert_ne!(built, PipelineFlags::default());
        // Explicit `false` and unset mean the same thing: off.
        let off = ConvertOptions {
            do_code_enrichment: Some(false),
            ..ConvertOptions::default()
        };
        assert_eq!(PipelineFlags::of(&off), PipelineFlags::default());
        assert!(warm_pipeline(&mut slot, &off).is_ok());
        assert_eq!(slot.as_ref().unwrap().0, PipelineFlags::default());
    }
}

#[cfg(test)]
mod ssrf_tests {
    use super::is_blocked_ip;
    use std::net::IpAddr;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn blocks_internal_targets() {
        // Loopback, private ranges, link-local (incl. cloud metadata),
        // unspecified, CGNAT, and the IPv4-mapped IPv6 forms must all be
        // refused as SSRF targets.
        for s in [
            "127.0.0.1",
            "127.5.5.5",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254", // AWS/GCP metadata
            "0.0.0.0",
            "100.64.0.1", // carrier-grade NAT
            "::1",
            "fe80::1",          // link-local
            "fc00::1",          // unique-local
            "::ffff:127.0.0.1", // IPv4-mapped loopback
            "::ffff:169.254.169.254",
        ] {
            assert!(is_blocked_ip(ip(s)), "{s} should be blocked");
        }
    }

    #[test]
    fn allows_public_targets() {
        for s in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "2606:4700:4700::1111",
        ] {
            assert!(!is_blocked_ip(ip(s)), "{s} should be allowed");
        }
    }

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
        // Neither an extension nor a known Content-Type → a 4xx (Bad), not a 500.
        assert!(matches!(
            source_from_named_bytes_ct("noext", b"x".to_vec(), Some("application/octet-stream")),
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
        assert!(!super::allow_private_ip_fetch());
        for (val, want) in [
            ("1", true),
            ("true", true),
            ("0", false),
            ("false", false),
            ("", false),
        ] {
            std::env::set_var("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH", val);
            assert_eq!(super::allow_private_ip_fetch(), want, "value {val:?}");
        }
        std::env::remove_var("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH");
    }
}

/// #304: the request-side VLM option resolution — the standard-pipeline
/// ignore, the `--allow-url-fetch` gate on request-supplied endpoints, the
/// SSRF resolution check, and the operator-pinned env fallback that skips it.
#[cfg(test)]
mod vlm_tests {
    use super::{resolve_vlm_options, ApiError, AppState, ConvertOptions, ServeConfig};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use tokio::sync::Semaphore;

    fn state(allow_url_fetch: bool) -> AppState {
        AppState {
            pipeline: Mutex::new(None),
            permits: Arc::new(Semaphore::new(1)),
            jobs: Mutex::new(Default::default()),
            ready: AtomicBool::new(true),
            memory_ceiling_mb: None,
            cfg: ServeConfig {
                allow_url_fetch,
                ..ServeConfig::default()
            },
        }
    }

    fn vlm_opts(endpoint: Option<&str>) -> ConvertOptions {
        ConvertOptions {
            pipeline: Some("vlm".into()),
            vlm_endpoint: endpoint.map(str::to_string),
            vlm_model: Some("m".into()),
            ..ConvertOptions::default()
        }
    }

    #[test]
    fn standard_pipeline_ignores_stray_vlm_options() {
        // Same contract as the Node bindings: no `pipeline=vlm`, no VLM — the
        // stray options are ignored, not rejected, and nothing is resolved.
        for pipeline in [None, Some("standard".to_string())] {
            let options = ConvertOptions {
                pipeline,
                ..vlm_opts(Some("http://127.0.0.1:9/v1"))
            };
            let resolved = resolve_vlm_options(&state(false), &options, true);
            assert!(matches!(resolved, Ok(None)));
        }
    }

    #[test]
    fn unknown_pipeline_is_rejected() {
        let options = ConvertOptions {
            pipeline: Some("magic".into()),
            ..ConvertOptions::default()
        };
        match resolve_vlm_options(&state(true), &options, false) {
            Err(ApiError::Bad(m)) => assert!(m.contains("unknown pipeline"), "{m}"),
            _ => panic!("expected Bad"),
        }
    }

    #[test]
    fn request_endpoint_is_gated_behind_allow_url_fetch() {
        // Without --allow-url-fetch a caller must not steer the server's
        // outbound traffic; the operator-pinned env mode is the alternative.
        match resolve_vlm_options(
            &state(false),
            &vlm_opts(Some("http://example.com/v1")),
            false,
        ) {
            Err(ApiError::Unsupported(m)) => assert!(m.contains("--allow-url-fetch"), "{m}"),
            _ => panic!("expected Unsupported"),
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

    #[test]
    fn vlm_max_tokens_zero_is_rejected_and_options_land() {
        let _env = super::ssrf_tests::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("DOCLING_RS_VLM_ENDPOINT");
        let mut options = vlm_opts(Some("http://example.com/v1"));
        options.vlm_max_tokens = Some(0);
        match resolve_vlm_options(&state(true), &options, false) {
            Err(ApiError::Bad(m)) => assert!(m.contains("vlm_max_tokens"), "{m}"),
            _ => panic!("expected Bad"),
        }
        // And without any endpoint at all, the error names this surface's
        // option spelling, not the CLI flag.
        let none = ConvertOptions {
            pipeline: Some("vlm".into()),
            ..ConvertOptions::default()
        };
        match resolve_vlm_options(&state(true), &none, false) {
            Err(ApiError::Bad(m)) => {
                assert!(m.contains("vlm_endpoint"), "{m}");
                assert!(!m.contains("--vlm-endpoint"), "{m}");
            }
            _ => panic!("expected Bad"),
        }
        // The full option set reaches the resolved struct.
        let mut options = vlm_opts(Some("http://example.com/v1"));
        options.vlm_api_key = Some("sk-test".into());
        options.vlm_prompt = Some("Read the page.".into());
        options.vlm_max_tokens = Some(512);
        options.pages = Some("2-5".into());
        let v = resolve_vlm_options(&state(true), &options, false)
            .ok()
            .flatten()
            .expect("resolves");
        assert_eq!(v.api_key.as_deref(), Some("sk-test"));
        assert_eq!(v.prompt.as_deref(), Some("Read the page."));
        assert_eq!(v.max_tokens, 512);
        assert_eq!(v.page_range, Some((2, 5)));
    }
}
