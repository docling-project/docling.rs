//! `docling-rs serve` — a long-running HTTP server over the docling.rs
//! converter, the analogue of Python's `docling-serve`.
//!
//! Endpoints:
//!
//! | Method | Path          | Description                                        |
//! |--------|---------------|----------------------------------------------------|
//! | GET    | `/`           | API docs + an interactive test form                |
//! | POST   | `/v1/convert` | convert an upload (multipart) or a URL (JSON body) |
//! | GET    | `/v1/config`  | server capabilities (`{"allow_url_fetch": bool}`)  |
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
//! - `pages` — PDF page window `A-B` / `N` (1-based inclusive, #80)
//! - `ocr_lang` — OCR recognition language for scanned pages: `en` (default)
//!   | `ch` (the multilingual docling-conformance model)
//! - `fetch_images` — resolve external `<img src>` for HTML/EPUB (outbound
//!   fetch, so honored only under `--allow-url-fetch`)
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
    /// Allow `{"url": …}` inputs (outbound fetch — SSRF surface). Off by
    /// default: even with the built-in private/loopback/link-local IP guard,
    /// letting a caller name the fetch target is a deliberate exposure that a
    /// deployment must opt into (`--allow-url-fetch`).
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
            allow_url_fetch: false,
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
        // Docs + test form, like the original docling-serve's playground.
        .route(
            "/",
            get(|| async { axum::response::Html(include_str!("index.html")) }),
        )
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/ready", get(ready))
        .route("/v1/config", get(config))
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

/// Capabilities the built-in UI adapts to. Currently just whether `{"url": …}`
/// inputs are accepted (`--allow-url-fetch`) — the UI greys out the URL option
/// and explains why when this is false, instead of letting the user hit a 422.
async fn config(State(state): State<Arc<AppState>>) -> Response {
    Json(json!({ "allow_url_fetch": state.cfg.allow_url_fetch })).into_response()
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
    asr_model: Option<String>,
    /// Max frames sampled from a video input (0 = transcript only; needs the
    /// server to have the ffmpeg binary).
    video_frames: Option<usize>,
    /// PDF page window, `"A-B"` or a single `"N"` (1-based inclusive — #80).
    pages: Option<String>,
    /// OCR recognition language for scanned pages: `en` (default) | `ch`.
    ocr_lang: Option<String>,
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
            asr_model: self.asr_model.or(base.asr_model),
            video_frames: self.video_frames.or(base.video_frames),
            pages: self.pages.or(base.pages),
            ocr_lang: self.ocr_lang.or(base.ocr_lang),
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
                "URL inputs are disabled; start docling-serve with --allow-url-fetch \
                 (SSRF surface — see docs/SECURITY.md), or upload the file instead"
                    .into(),
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
            "asr_model" => body_opts.asr_model = Some(text_field(field).await?),
            "pages" => body_opts.pages = Some(text_field(field).await?),
            "ocr_lang" => body_opts.ocr_lang = Some(text_field(field).await?),
            "video_frames" => {
                let v = text_field(field).await?;
                body_opts.video_frames = Some(v.parse().map_err(|_| {
                    ApiError::Bad(format!(
                        "video_frames must be a non-negative integer, got {v:?}"
                    ))
                })?);
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
    std::env::var("DOCLING_RS_MAX_FETCH_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256 * 1024 * 1024)
}

/// Escape hatch for local development: when `DOCLING_RS_ALLOW_PRIVATE_IP_FETCH`
/// is set to a truthy value (anything but empty / `0` / `false`), the SSRF IP
/// block-list is not enforced, so a URL like `http://localhost:8080/doc.pdf`
/// can be fetched. Off by default — leave it unset in production.
fn allow_private_ip_fetch() -> bool {
    std::env::var("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH")
        .map(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
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

/// Fetch a URL input (blocking; run on the blocking pool). The name comes
/// from `file_name` or the URL path's last segment.
fn fetch_url(url: &str, file_name: Option<&str>) -> Result<SourceDocument, ApiError> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(ApiError::Bad(format!("unsupported URL scheme in '{url}'")));
    }
    // SSRF guard: resolve the host and reject if it maps to a private/loopback/
    // link-local address, and forbid redirects (a public URL could 30x-bounce
    // to an internal target, defeating this pre-check). This is a best-effort
    // mitigation — a DNS-rebinding race between this resolution and ureq's own
    // connect remains theoretically possible; the deployment-level control is
    // to leave URL fetch disabled unless the network is trusted.
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
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .max_redirects(0)
        .build()
        .into();
    let mut response = agent
        .get(url)
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
/// pipeline and everything else through the declarative converter.
fn convert_document(
    state: &AppState,
    source: SourceDocument,
    options: &ConvertOptions,
) -> Result<DoclingDocument, ApiError> {
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
        .asr_model(options.asr_model.clone())
        .video_frames(
            options
                .video_frames
                .unwrap_or(docling::DEFAULT_VIDEO_FRAMES),
        )
        .no_ocr(options.no_ocr.unwrap_or(false))
        .no_table_former(options.no_table_former.unwrap_or(false));
    if let Some(pages) = &options.pages {
        let (first, last) =
            docling::parse_page_range(pages).map_err(|e| ApiError::Bad(format!("pages: {e}")))?;
        converter = converter.page_range(first, last);
    }
    if parse_ocr_lang(options.ocr_lang.as_deref())?.is_some() {
        converter = converter.ocr_lang(options.ocr_lang.clone().expect("checked above"));
    }
    Ok(converter)
}

/// Validate a request's `ocr_lang` (None passes through — the engine default).
fn parse_ocr_lang(raw: Option<&str>) -> Result<Option<docling::OcrLang>, ApiError> {
    raw.map(|v| {
        docling::OcrLang::parse(v)
            .ok_or_else(|| ApiError::Bad(format!("ocr_lang {v:?} is not en|ch")))
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
                let converter = match request_converter(&st, &options) {
                    Ok(c) => c,
                    Err(e) => {
                        send(Err(api_error_message(e)));
                        return;
                    }
                };
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
        // No chunks means the document converted to empty Markdown (e.g. an
        // HTML page with no extractable content) — a valid result, not a
        // server error. Return an empty 200 body rather than a 500.
        None => Ok((
            [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
            Body::empty(),
        )
            .into_response()),
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

    #[test]
    fn private_ip_escape_hatch_defaults_off() {
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
