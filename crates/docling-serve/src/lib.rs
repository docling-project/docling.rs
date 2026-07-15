//! `docling-rs serve` — a long-running HTTP server over the docling.rs
//! converter, the analogue of Python's `docling-serve`.
//!
//! Endpoints:
//!
//! | Method | Path          | Description                                        |
//! |--------|---------------|----------------------------------------------------|
//! | POST   | `/v1/convert` | convert an upload (multipart) or a URL (JSON body) |
//! | GET    | `/health`     | liveness probe                                     |
//! | GET    | `/ready`      | readiness probe (200 once models are warm)         |
//!
//! `POST /v1/convert` accepts either `multipart/form-data` with a `file` part
//! (the filename's extension selects the input format) or an
//! `application/json` body `{"url": "https://…", "file_name"?: "override.pdf"}`.
//! Options ride along as multipart text parts, JSON fields, or query
//! parameters (body wins over query):
//!
//! - `to` — `md` (default) | `json` | `dclx` | `chunks`
//! - `strict` — cleaner Markdown instead of docling-legacy output
//! - `images` — `placeholder` (default) | `embedded` (Markdown only)
//! - `no_ocr`, `no_table_former` — PDF/image pipeline switches
//! - `fetch_images` — resolve external `<img src>` for HTML/EPUB
//!
//! Markdown converts through the streaming serializer and the response body
//! streams page by page (chunked transfer); `json`/`dclx`/`chunks` buffer.
//!
//! One warm [`Pipeline`] (layout/OCR/TableFormer sessions) is shared across
//! requests behind a mutex — PDF/image conversions serialize on it instead of
//! reloading models. Declarative formats convert on blocking threads and run
//! concurrently. A semaphore bounds total in-flight conversions
//! (`--concurrency`); excess requests queue.
//!
//! Security: URL fetching makes the server issue outbound requests (SSRF
//! surface) — bind to loopback (the default) or front with a policy proxy,
//! and gate it with `--no-url-fetch` where the input should be uploads only.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use docling::{
    DoclingDocument, DocumentConverter, ImageMode, InputFormat, Pipeline, SourceDocument,
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;

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
    /// Allow `{"url": …}` inputs (outbound fetch — SSRF surface).
    pub allow_url_fetch: bool,
    /// Default `strict` for requests that don't set it.
    pub strict: bool,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            addr: "127.0.0.1:5001".into(),
            concurrency: 2,
            max_body_bytes: 256 * 1024 * 1024,
            warmup: false,
            allow_url_fetch: true,
            strict: false,
        }
    }
}

struct AppState {
    /// Warm ML pipeline (mutable ONNX sessions) — one PDF/image conversion at
    /// a time, but the models stay loaded across requests.
    pipeline: Mutex<Option<Pipeline>>,
    /// Bounds total in-flight conversions (`Arc` so a permit can move into
    /// a streaming response's worker and outlive the handler).
    permits: Arc<Semaphore>,
    ready: AtomicBool,
    cfg: ServeConfig,
}

/// Build the router (exposed separately from [`serve`] for tests).
pub fn router(cfg: ServeConfig) -> Router {
    let state = Arc::new(AppState {
        pipeline: Mutex::new(None),
        permits: Arc::new(Semaphore::new(cfg.concurrency.max(1))),
        ready: AtomicBool::new(!cfg.warmup),
        cfg: cfg.clone(),
    });
    if cfg.warmup {
        let st = state.clone();
        // Blocking model load off the runtime; readiness flips when done.
        tokio::task::spawn_blocking(move || {
            match Pipeline::new() {
                Ok(p) => *st.pipeline.lock().unwrap() = Some(p),
                Err(e) => eprintln!("warmup: pipeline load failed: {e}"),
            }
            st.ready.store(true, Ordering::Release);
        });
    }
    Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/ready", get(ready))
        .route("/v1/convert", post(convert))
        .layer(DefaultBodyLimit::max(cfg.max_body_bytes))
        .with_state(state)
}

/// Bind and serve until SIGINT/SIGTERM; in-flight requests finish (graceful
/// shutdown).
pub async fn serve(cfg: ServeConfig) -> Result<(), String> {
    let addr = cfg.addr.clone();
    let app = router(cfg);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("cannot bind {addr}: {e}"))?;
    eprintln!("docling-serve listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| format!("server error: {e}"))
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
    no_table_former: Option<bool>,
    fetch_images: Option<bool>,
}

impl ConvertOptions {
    fn merge_over(self, base: ConvertOptions) -> ConvertOptions {
        ConvertOptions {
            to: self.to.or(base.to),
            strict: self.strict.or(base.strict),
            images: self.images.or(base.images),
            no_ocr: self.no_ocr.or(base.no_ocr),
            no_table_former: self.no_table_former.or(base.no_table_former),
            fetch_images: self.fetch_images.or(base.fetch_images),
        }
    }
}

/// JSON body for URL inputs.
#[derive(Debug, Deserialize)]
struct UrlRequest {
    url: String,
    /// Overrides the name (and thus format-selecting extension) taken from
    /// the URL path's last segment.
    file_name: Option<String>,
    #[serde(flatten)]
    options: ConvertOptions,
}

enum ApiError {
    Bad(String),
    Unsupported(String),
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ApiError::Bad(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::Unsupported(m) => (StatusCode::UNPROCESSABLE_ENTITY, m),
            ApiError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (status, Json(json!({"error": msg}))).into_response()
    }
}

async fn convert(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ConvertOptions>,
    headers: HeaderMap,
    body: axum::extract::Request,
) -> Result<Response, ApiError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let (source, options) = if content_type.starts_with("multipart/form-data") {
        let multipart = Multipart::from_request(body, &())
            .await
            .map_err(|e| ApiError::Bad(format!("bad multipart body: {e}")))?;
        read_multipart(multipart, query).await?
    } else if content_type.starts_with("application/json") {
        let bytes = axum::body::to_bytes(body.into_body(), state.cfg.max_body_bytes)
            .await
            .map_err(|e| ApiError::Bad(format!("bad body: {e}")))?;
        let req: UrlRequest = serde_json::from_slice(&bytes)
            .map_err(|e| ApiError::Bad(format!("bad JSON body: {e}")))?;
        if !state.cfg.allow_url_fetch {
            return Err(ApiError::Unsupported(
                "URL inputs are disabled (--no-url-fetch); upload the file instead".into(),
            ));
        }
        let options = req.options.clone().merge_over(query);
        let url = req.url.clone();
        let name = req.file_name.clone();
        let source = tokio::task::spawn_blocking(move || fetch_url(&url, name.as_deref()))
            .await
            .map_err(|e| ApiError::Internal(format!("fetch task: {e}")))??;
        (source, options)
    } else {
        return Err(ApiError::Bad(
            "expected multipart/form-data (file upload) or application/json ({\"url\": …})".into(),
        ));
    };

    let to = options.to.clone().unwrap_or_else(|| "md".into());
    if !matches!(to.as_str(), "md" | "markdown" | "json" | "dclx" | "chunks") {
        return Err(ApiError::Bad(format!(
            "unknown to='{to}' (expected: md, json, dclx, chunks)"
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

    // Bound total in-flight conversions; excess requests queue here. The
    // permit is owned so the streaming path can hold it until the response
    // body finishes, not just until the handler returns.
    let permit = state
        .permits
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| ApiError::Internal(format!("semaphore: {e}")))?;

    let is_markdown = matches!(to.as_str(), "md" | "markdown");
    if is_markdown {
        return stream_markdown(state.clone(), source, options, image_mode, permit).await;
    }
    let _permit = permit;

    // Buffered outputs: convert on a blocking thread, then serialize.
    let st = state.clone();
    let name = source.name.clone();
    let document = tokio::task::spawn_blocking(move || convert_document(&st, source, &options))
        .await
        .map_err(|e| ApiError::Internal(format!("convert task: {e}")))??;

    Ok(match to.as_str() {
        "json" => (
            [(header::CONTENT_TYPE, "application/json")],
            document.export_to_json(),
        )
            .into_response(),
        "chunks" => {
            let mut warnings: Vec<String> = Vec::new();
            let mut records = docling::chunks::chunk_records(&document, &mut |m| warnings.push(m));
            if !warnings.is_empty() {
                records["warnings"] = json!(warnings);
            }
            Json(records).into_response()
        }
        "dclx" => {
            let bytes = docling::dclx::to_dclx_bytes(&document);
            (
                [
                    (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{name}.dclx\""),
                    ),
                ],
                bytes,
            )
                .into_response()
        }
        _ => unreachable!("validated above"),
    })
}

/// Read the multipart request: a `file` part (bytes + filename) plus optional
/// text parts mirroring the query options.
async fn read_multipart(
    mut multipart: Multipart,
    query: ConvertOptions,
) -> Result<(SourceDocument, ConvertOptions), ApiError> {
    let mut file: Option<(String, Vec<u8>)> = None;
    let mut body_opts = ConvertOptions::default();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::Bad(format!("bad multipart field: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                let file_name = field
                    .file_name()
                    .map(|s| s.to_string())
                    .ok_or_else(|| ApiError::Bad("file part needs a filename".into()))?;
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| ApiError::Bad(format!("reading upload: {e}")))?;
                file = Some((file_name, bytes.to_vec()));
            }
            "to" | "images" => {
                let v = text_field(field).await?;
                match name.as_str() {
                    "to" => body_opts.to = Some(v),
                    _ => body_opts.images = Some(v),
                }
            }
            "strict" | "no_ocr" | "no_table_former" | "fetch_images" => {
                let v = text_field(field).await?;
                let b = matches!(v.as_str(), "1" | "true" | "yes" | "on");
                match name.as_str() {
                    "strict" => body_opts.strict = Some(b),
                    "no_ocr" => body_opts.no_ocr = Some(b),
                    "no_table_former" => body_opts.no_table_former = Some(b),
                    _ => body_opts.fetch_images = Some(b),
                }
            }
            _ => {} // unknown parts are ignored
        }
    }
    let (file_name, bytes) = file.ok_or_else(|| ApiError::Bad("missing 'file' part".into()))?;
    let source = source_from_named_bytes(&file_name, bytes)?;
    Ok((source, body_opts.merge_over(query)))
}

async fn text_field(field: axum::extract::multipart::Field<'_>) -> Result<String, ApiError> {
    field
        .text()
        .await
        .map_err(|e| ApiError::Bad(format!("reading field: {e}")))
}

/// Build a [`SourceDocument`] from a filename (extension → format) and bytes.
fn source_from_named_bytes(file_name: &str, bytes: Vec<u8>) -> Result<SourceDocument, ApiError> {
    let ext = std::path::Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| ApiError::Bad(format!("no extension on '{file_name}'")))?;
    let format = InputFormat::from_extension(ext)
        .ok_or_else(|| ApiError::Unsupported(format!("unrecognized extension '.{ext}'")))?;
    let stem = std::path::Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("document")
        .to_string();
    Ok(SourceDocument::from_bytes(stem, format, bytes))
}

/// Fetch a URL input (blocking; run on the blocking pool). The name comes
/// from `file_name` or the URL path's last segment.
fn fetch_url(url: &str, file_name: Option<&str>) -> Result<SourceDocument, ApiError> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(ApiError::Bad(format!("unsupported URL scheme in '{url}'")));
    }
    let mut response = ureq::get(url)
        .call()
        .map_err(|e| ApiError::Bad(format!("fetching {url}: {e}")))?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| ApiError::Bad(format!("reading {url}: {e}")))?;
    let name = file_name
        .map(|s| s.to_string())
        .or_else(|| {
            url.split('/')
                .next_back()
                .map(|s| s.split(['?', '#']).next().unwrap_or(s).to_string())
                .filter(|s| !s.is_empty())
        })
        .ok_or_else(|| {
            ApiError::Bad("cannot derive a file name from the URL; pass file_name".into())
        })?;
    source_from_named_bytes(&name, bytes)
}

/// Convert to a [`DoclingDocument`], routing PDF/image through the warm
/// pipeline and everything else through the declarative converter.
fn convert_document(
    state: &AppState,
    source: SourceDocument,
    options: &ConvertOptions,
) -> Result<DoclingDocument, ApiError> {
    match source.format {
        InputFormat::Pdf | InputFormat::Image => {
            let mut guard = state.pipeline.lock().unwrap();
            let pipeline = warm_pipeline(&mut guard, options)?;
            let doc = match source.format {
                InputFormat::Pdf => pipeline.convert(&source.bytes, None, &source.name),
                _ => pipeline.convert_image(&source.bytes, &source.name),
            }
            .map_err(|e| ApiError::Internal(e.to_string()))?;
            Ok(doc)
        }
        _ => {
            let converter = request_converter(state, options);
            converter
                .convert(source)
                .map(|r| r.document)
                .map_err(|e| ApiError::Unsupported(e.to_string()))
        }
    }
}

/// The lazily-loaded warm pipeline. Pipeline switches (`no_ocr`,
/// `no_table_former`) are per-instance, so a request that changes them
/// rebuilds the pipeline (model sessions reload); steady-state traffic with
/// stable options keeps the warm one.
fn warm_pipeline<'a>(
    slot: &'a mut Option<Pipeline>,
    options: &ConvertOptions,
) -> Result<&'a mut Pipeline, ApiError> {
    let no_ocr = options.no_ocr.unwrap_or(false);
    let no_tf = options.no_table_former.unwrap_or(false);
    if no_ocr || no_tf {
        let p = Pipeline::new()
            .map_err(|e| ApiError::Internal(e.to_string()))?
            .no_ocr(no_ocr)
            .no_table_former(no_tf);
        *slot = Some(p);
    } else if slot.is_none() {
        *slot = Some(Pipeline::new().map_err(|e| ApiError::Internal(e.to_string()))?);
    }
    Ok(slot.as_mut().expect("just filled"))
}

/// Per-request declarative converter (construction is cheap — it's
/// configuration, models don't apply).
fn request_converter(state: &AppState, options: &ConvertOptions) -> DocumentConverter {
    DocumentConverter::new()
        .strict(options.strict.unwrap_or(state.cfg.strict))
        .fetch_images(options.fetch_images.unwrap_or(false))
        .no_ocr(options.no_ocr.unwrap_or(false))
        .no_table_former(options.no_table_former.unwrap_or(false))
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
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, String>>(8);
    let st = state.clone();
    tokio::task::spawn_blocking(move || {
        // Held until this worker (and thus the response body) is done.
        let _permit = permit;
        let send = |item: Result<String, String>| {
            // The receiver disappearing means the client went away — stop.
            tx.blocking_send(item).is_ok()
        };
        match source.format {
            InputFormat::Pdf | InputFormat::Image => {
                // Buffered document → streamed serialization is pointless for
                // images (one step); PDFs stream page by page through the
                // warm pipeline's converter equivalent: convert, then stream
                // the serializer output. (True page-by-page pipeline
                // streaming holds the model mutex anyway, so the wall-clock
                // is the same; the client still gets incremental output.)
                match convert_document(&st, source, &options) {
                    Ok(mut doc) => {
                        doc.strict_markdown = options.strict.unwrap_or(st.cfg.strict);
                        let md = match image_mode {
                            ImageMode::Placeholder => doc.export_to_markdown(),
                            _ => {
                                doc.export_to_markdown_with_images(image_mode, "artifacts")
                                    .0
                            }
                        };
                        send(Ok(md));
                    }
                    Err(e) => {
                        send(Err(api_error_message(e)));
                    }
                }
            }
            _ => {
                let converter = request_converter(&st, &options);
                match converter.convert_streaming_images(source, image_mode) {
                    Ok(stream) => {
                        for chunk in stream {
                            match chunk {
                                Ok(s) => {
                                    if !send(Ok(s)) {
                                        return;
                                    }
                                }
                                Err(e) => {
                                    send(Err(e.to_string()));
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        send(Err(e.to_string()));
                    }
                }
            }
        }
    });

    // First chunk decides the status code; later errors abort the stream
    // mid-body (the client sees a truncated response).
    let mut rx = rx;
    let first = rx.recv().await;
    match first {
        None => Err(ApiError::Internal("converter produced no output".into())),
        Some(Err(e)) => Err(ApiError::Unsupported(e)),
        Some(Ok(first_chunk)) => {
            use tokio_stream::StreamExt;
            let rest = tokio_stream::wrappers::ReceiverStream::new(rx);
            let stream = tokio_stream::once(Ok(first_chunk)).chain(rest).map(|item| {
                item.map(String::into_bytes).map_err(|e| {
                    std::io::Error::other(format!("conversion failed mid-stream: {e}"))
                })
            });
            Ok((
                [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
                Body::from_stream(stream),
            )
                .into_response())
        }
    }
}

fn api_error_message(e: ApiError) -> String {
    match e {
        ApiError::Bad(m) | ApiError::Unsupported(m) | ApiError::Internal(m) => m,
    }
}
