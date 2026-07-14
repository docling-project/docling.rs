//! Node.js / Bun bindings for docling.rs, via napi-rs.
//!
//! The surface mirrors the Rust `DocumentConverter`: convert a file (or
//! in-memory bytes) to Markdown or docling-core JSON, with the same options —
//! strict Markdown, picture image modes, allowed-format restriction, external
//! `<img>` fetching — plus incremental Markdown streaming. Everything here is
//! thin glue; the conversion logic lives in the `docling.rs` crate.
//!
//! Two ways to call it:
//! - the module-level [`convert_file`] / [`convert`] (+ their `*_async`
//!   variants), for one-shot use;
//! - the [`DocumentConverter`] class, which holds converter config so it can be
//!   reused across many documents.

use std::sync::{Arc, Mutex};

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;

use docling::{
    ConversionStatus, DoclingDocument, DocumentConverter as RsConverter, ImageMode, InputFormat,
    MarkdownStreamer, Pipeline as RsPipeline, SourceDocument,
};

// ---------------------------------------------------------------------------
// Options / result shapes exposed to TypeScript.
// ---------------------------------------------------------------------------

/// Config for a reusable [`DocumentConverter`].
#[napi(object)]
#[derive(Clone, Default)]
pub struct ConverterOptions {
    /// Emit cleaner, more conformant Markdown (code-fence languages preserved,
    /// no inline-run spacing artifacts) instead of docling's byte-for-byte
    /// legacy output. Markdown only. Default `false`.
    pub strict: Option<bool>,
    /// For HTML/EPUB, resolve external `<img src>` (data: URIs, local files,
    /// http(s) URLs, EPUB entries) and embed the bytes. Off by default; when on,
    /// http(s) URLs are fetched over the network — enable only for trusted input.
    pub fetch_images: Option<bool>,
    /// Restrict the converter to these formats (ids like `"md"`, `"pdf"`, or
    /// extensions like `".html"`); anything else is rejected. Default: accept all.
    pub allowed_formats: Option<Vec<String>>,
}

/// Per-call output options (how to render the converted document).
#[napi(object)]
#[derive(Clone, Default)]
pub struct OutputOptions {
    /// `"markdown"` (default) or `"json"` (docling-core DoclingDocument wire format).
    pub to: Option<String>,
    /// Picture handling for Markdown: `"placeholder"` (default), `"embedded"`
    /// (base64 data URIs inline), or `"referenced"` (returns image files in
    /// `images`). Ignored for JSON, which always embeds images as data URIs.
    pub image_mode: Option<String>,
    /// Directory name used in `referenced` image links. Default `"artifacts"`.
    pub artifacts_dir: Option<String>,
}

/// All options for the one-shot module-level functions (converter config +
/// output options in a single object).
#[napi(object)]
#[derive(Clone, Default)]
pub struct ConvertOptions {
    pub strict: Option<bool>,
    pub fetch_images: Option<bool>,
    pub allowed_formats: Option<Vec<String>>,
    pub to: Option<String>,
    pub image_mode: Option<String>,
    pub artifacts_dir: Option<String>,
}

/// In-memory input for [`DocumentConverter::convert`] / [`convert`].
#[napi(object)]
pub struct ConvertInput {
    /// Logical document name (used as the docling document name).
    pub name: String,
    /// Raw file bytes.
    pub data: Buffer,
    /// Format id or extension (e.g. `"md"`, `"pdf"`, `".html"`). Omit to infer
    /// from an extension on `name`.
    pub format: Option<String>,
}

/// One extracted image file, returned for the `referenced` image mode.
#[napi(object)]
pub struct ImageArtifact {
    /// Path relative to the Markdown file (e.g. `"artifacts/image_000000.png"`).
    pub path: String,
    /// The image bytes to write at `path`.
    pub data: Buffer,
}

/// The result of a conversion.
#[napi(object)]
pub struct ConvertResult {
    /// The rendered document: Markdown or JSON, per `to`.
    pub content: String,
    /// Detected input format id (e.g. `"md"`, `"pdf"`).
    pub format: String,
    /// `"success"`, `"partial_success"`, or `"failure"`.
    pub status: String,
    /// The document name.
    pub input_name: String,
    /// For the `referenced` image mode, the image files to write next to the
    /// Markdown; empty otherwise.
    pub images: Vec<ImageArtifact>,
}

// ---------------------------------------------------------------------------
// Internal, Send-safe conversion plumbing (shared by sync, async, streaming).
// ---------------------------------------------------------------------------

/// Fully-resolved conversion config, free of any napi/JS types so it can move
/// onto a worker thread for the async and streaming paths.
struct ConvertConfig {
    strict: bool,
    fetch_images: bool,
    allowed_formats: Option<Vec<InputFormat>>,
    to: OutputKind,
    image_mode: ImageMode,
    artifacts_dir: String,
}

#[derive(Clone, Copy, PartialEq)]
enum OutputKind {
    Markdown,
    Json,
}

/// A Send-safe conversion result (raw bytes, no `Buffer`), so it can be produced
/// off the JS thread and turned into a [`ConvertResult`] on resolve. Public only
/// because it is the `Output` of the public [`Task`] impls; not exposed to JS.
#[doc(hidden)]
pub struct RawResult {
    content: String,
    format: String,
    status: String,
    input_name: String,
    images: Vec<(String, Vec<u8>)>,
}

impl RawResult {
    fn into_js(self) -> ConvertResult {
        ConvertResult {
            content: self.content,
            format: self.format,
            status: self.status,
            input_name: self.input_name,
            images: self
                .images
                .into_iter()
                .map(|(path, data)| ImageArtifact {
                    path,
                    data: data.into(),
                })
                .collect(),
        }
    }
}

fn build_config(
    strict: Option<bool>,
    fetch_images: Option<bool>,
    allowed_formats: Option<Vec<String>>,
    to: Option<String>,
    image_mode: Option<String>,
    artifacts_dir: Option<String>,
) -> Result<ConvertConfig> {
    let allowed = match allowed_formats {
        Some(list) => Some(
            list.iter()
                .map(|s| parse_format(s))
                .collect::<Result<Vec<_>>>()?,
        ),
        None => None,
    };
    Ok(ConvertConfig {
        strict: strict.unwrap_or(false),
        fetch_images: fetch_images.unwrap_or(false),
        allowed_formats: allowed,
        to: parse_output_kind(to.as_deref())?,
        image_mode: parse_image_mode(image_mode.as_deref())?,
        artifacts_dir: artifacts_dir.unwrap_or_else(|| "artifacts".to_string()),
    })
}

fn build_converter(cfg: &ConvertConfig) -> RsConverter {
    let base = match &cfg.allowed_formats {
        Some(list) => RsConverter::with_allowed_formats(list.iter().copied()),
        None => RsConverter::new(),
    };
    base.strict(cfg.strict).fetch_images(cfg.fetch_images)
}

/// Render an already-converted document to Markdown/JSON per the config. The
/// document's `strict_markdown` is assumed already set by whoever produced it.
fn render_doc(
    doc: DoclingDocument,
    cfg: &ConvertConfig,
    input_name: String,
    format: String,
    status: String,
) -> RawResult {
    let (content, images) = match cfg.to {
        OutputKind::Json => (doc.export_to_json(), Vec::new()),
        OutputKind::Markdown => match cfg.image_mode {
            ImageMode::Placeholder => (doc.export_to_markdown(), Vec::new()),
            mode => doc.export_to_markdown_with_images(mode, &cfg.artifacts_dir),
        },
    };
    RawResult {
        content,
        format,
        status,
        input_name,
        images,
    }
}

/// Run a buffered conversion and render it per the config. Runs off the JS
/// thread for the async path, so it must stay free of napi/JS types.
fn run_convert(source: SourceDocument, cfg: &ConvertConfig) -> Result<RawResult> {
    let converter = build_converter(cfg);
    let result = converter.convert(source).map_err(convert_err)?;
    let format = result.format.as_str().to_string();
    let status = status_str(result.status);
    Ok(render_doc(
        result.document,
        cfg,
        result.input_name,
        format,
        status,
    ))
}

/// Load a [`SourceDocument`] from an in-memory [`ConvertInput`].
fn source_from_input(input: ConvertInput) -> Result<SourceDocument> {
    let format = match &input.format {
        Some(f) => parse_format(f)?,
        None => infer_format(&input.name).ok_or_else(|| {
            Error::new(
                Status::InvalidArg,
                format!(
                    "could not infer a format from name '{}'; pass `format` explicitly",
                    input.name
                ),
            )
        })?,
    };
    Ok(SourceDocument::from_bytes(
        input.name,
        format,
        input.data.to_vec(),
    ))
}

// ---------------------------------------------------------------------------
// Module-level one-shot API.
// ---------------------------------------------------------------------------

/// Convert a file on disk. Detects the format from the extension and (for
/// HTML/EPUB image fetching) resolves relative `<img src>` against the file's
/// directory.
#[napi]
pub fn convert_file(path: String, options: Option<ConvertOptions>) -> Result<ConvertResult> {
    let o = options.unwrap_or_default();
    let cfg = build_config(
        o.strict,
        o.fetch_images,
        o.allowed_formats,
        o.to,
        o.image_mode,
        o.artifacts_dir,
    )?;
    let source = SourceDocument::from_file(&path).map_err(convert_err)?;
    Ok(run_convert(source, &cfg)?.into_js())
}

/// Convert in-memory bytes.
#[napi]
pub fn convert(input: ConvertInput, options: Option<ConvertOptions>) -> Result<ConvertResult> {
    let o = options.unwrap_or_default();
    let cfg = build_config(
        o.strict,
        o.fetch_images,
        o.allowed_formats,
        o.to,
        o.image_mode,
        o.artifacts_dir,
    )?;
    let source = source_from_input(input)?;
    Ok(run_convert(source, &cfg)?.into_js())
}

/// Async (Promise-returning) [`convert_file`]. The CPU-bound work runs on the
/// libuv thread pool, keeping the event loop free — use this for PDF/image.
#[napi(ts_return_type = "Promise<ConvertResult>")]
pub fn convert_file_async(
    path: String,
    options: Option<ConvertOptions>,
) -> Result<AsyncTask<ConvertFileTask>> {
    let o = options.unwrap_or_default();
    let cfg = build_config(
        o.strict,
        o.fetch_images,
        o.allowed_formats,
        o.to,
        o.image_mode,
        o.artifacts_dir,
    )?;
    Ok(AsyncTask::new(ConvertFileTask { path, cfg }))
}

/// Async (Promise-returning) [`convert`].
#[napi(ts_return_type = "Promise<ConvertResult>")]
pub fn convert_async(
    input: ConvertInput,
    options: Option<ConvertOptions>,
) -> Result<AsyncTask<ConvertBytesTask>> {
    let o = options.unwrap_or_default();
    let cfg = build_config(
        o.strict,
        o.fetch_images,
        o.allowed_formats,
        o.to,
        o.image_mode,
        o.artifacts_dir,
    )?;
    let source = source_from_input(input)?;
    Ok(AsyncTask::new(ConvertBytesTask {
        source: Some(source),
        cfg,
    }))
}

pub struct ConvertFileTask {
    path: String,
    cfg: ConvertConfig,
}

impl Task for ConvertFileTask {
    type Output = RawResult;
    type JsValue = ConvertResult;

    fn compute(&mut self) -> Result<RawResult> {
        let source = SourceDocument::from_file(&self.path).map_err(convert_err)?;
        run_convert(source, &self.cfg)
    }

    fn resolve(&mut self, _env: Env, output: RawResult) -> Result<ConvertResult> {
        Ok(output.into_js())
    }
}

pub struct ConvertBytesTask {
    // `Option` so `compute` can take ownership of the (non-Copy) source.
    source: Option<SourceDocument>,
    cfg: ConvertConfig,
}

impl Task for ConvertBytesTask {
    type Output = RawResult;
    type JsValue = ConvertResult;

    fn compute(&mut self) -> Result<RawResult> {
        let source = self
            .source
            .take()
            .ok_or_else(|| Error::new(Status::GenericFailure, "conversion task reused"))?;
        run_convert(source, &self.cfg)
    }

    fn resolve(&mut self, _env: Env, output: RawResult) -> Result<ConvertResult> {
        Ok(output.into_js())
    }
}

// ---------------------------------------------------------------------------
// Reusable converter class.
// ---------------------------------------------------------------------------

/// A reusable converter. Holds config (strict / fetch-images / allowed formats)
/// so you can convert many documents without re-parsing options each time —
/// the analogue of the Rust `DocumentConverter`.
#[napi]
pub struct DocumentConverter {
    strict: bool,
    fetch_images: bool,
    allowed_formats: Option<Vec<InputFormat>>,
}

#[napi]
impl DocumentConverter {
    #[napi(constructor)]
    pub fn new(options: Option<ConverterOptions>) -> Result<Self> {
        let o = options.unwrap_or_default();
        let allowed = match o.allowed_formats {
            Some(list) => Some(
                list.iter()
                    .map(|s| parse_format(s))
                    .collect::<Result<Vec<_>>>()?,
            ),
            None => None,
        };
        Ok(Self {
            strict: o.strict.unwrap_or(false),
            fetch_images: o.fetch_images.unwrap_or(false),
            allowed_formats: allowed,
        })
    }

    fn config(&self, out: Option<OutputOptions>) -> Result<ConvertConfig> {
        let out = out.unwrap_or_default();
        Ok(ConvertConfig {
            strict: self.strict,
            fetch_images: self.fetch_images,
            allowed_formats: self.allowed_formats.clone(),
            to: parse_output_kind(out.to.as_deref())?,
            image_mode: parse_image_mode(out.image_mode.as_deref())?,
            artifacts_dir: out.artifacts_dir.unwrap_or_else(|| "artifacts".to_string()),
        })
    }

    /// Convert a file on disk (sync).
    #[napi]
    pub fn convert_file(
        &self,
        path: String,
        options: Option<OutputOptions>,
    ) -> Result<ConvertResult> {
        let cfg = self.config(options)?;
        let source = SourceDocument::from_file(&path).map_err(convert_err)?;
        Ok(run_convert(source, &cfg)?.into_js())
    }

    /// Convert in-memory bytes (sync).
    #[napi]
    pub fn convert(
        &self,
        input: ConvertInput,
        options: Option<OutputOptions>,
    ) -> Result<ConvertResult> {
        let cfg = self.config(options)?;
        let source = source_from_input(input)?;
        Ok(run_convert(source, &cfg)?.into_js())
    }

    /// Async (Promise-returning) file conversion (runs off the event loop).
    #[napi(ts_return_type = "Promise<ConvertResult>")]
    pub fn convert_file_async(
        &self,
        path: String,
        options: Option<OutputOptions>,
    ) -> Result<AsyncTask<ConvertFileTask>> {
        let cfg = self.config(options)?;
        Ok(AsyncTask::new(ConvertFileTask { path, cfg }))
    }

    /// Async (Promise-returning) bytes conversion (runs off the event loop).
    #[napi(ts_return_type = "Promise<ConvertResult>")]
    pub fn convert_async(
        &self,
        input: ConvertInput,
        options: Option<OutputOptions>,
    ) -> Result<AsyncTask<ConvertBytesTask>> {
        let cfg = self.config(options)?;
        let source = source_from_input(input)?;
        Ok(AsyncTask::new(ConvertBytesTask {
            source: Some(source),
            cfg,
        }))
    }

    /// Stream a file's Markdown in chunks, in document order, as conversion
    /// progresses (the headline win for PDF, whose pages convert in parallel).
    ///
    /// `callback` is invoked as `(err, chunk)`: once per Markdown chunk with
    /// `chunk` a string, once with `chunk === null` at the end, or once with a
    /// non-null `err` on failure. Only `placeholder` / `embedded` image modes
    /// stream; `referenced` is rejected. Prefer the `streamFileMarkdown`
    /// async-generator wrapper in JS over calling this directly.
    #[napi]
    pub fn convert_file_streaming(
        &self,
        path: String,
        callback: ThreadsafeFunction<Option<String>, ErrorStrategy::CalleeHandled>,
        options: Option<OutputOptions>,
    ) -> Result<()> {
        let cfg = self.config(options)?;
        let converter = build_converter(&cfg);
        let image_mode = cfg.image_mode;
        // The background conversion thread owns the stream and pushes each chunk
        // through the threadsafe function (which marshals back to the JS loop).
        std::thread::spawn(move || {
            let source = match SourceDocument::from_file(&path).map_err(convert_err) {
                Ok(s) => s,
                Err(e) => {
                    callback.call(Err(e), ThreadsafeFunctionCallMode::NonBlocking);
                    return;
                }
            };
            let stream = match converter.convert_streaming_images(source, image_mode) {
                Ok(s) => s,
                Err(e) => {
                    callback.call(Err(convert_err(e)), ThreadsafeFunctionCallMode::NonBlocking);
                    return;
                }
            };
            for chunk in stream {
                match chunk {
                    Ok(s) => {
                        callback.call(Ok(Some(s)), ThreadsafeFunctionCallMode::NonBlocking);
                    }
                    Err(e) => {
                        callback.call(Err(convert_err(e)), ThreadsafeFunctionCallMode::NonBlocking);
                        return;
                    }
                }
            }
            // End-of-stream sentinel.
            callback.call(Ok(None), ThreadsafeFunctionCallMode::NonBlocking);
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Reusable warm PDF/image pipeline.
// ---------------------------------------------------------------------------

/// A reusable PDF/image pipeline that keeps the ONNX models (layout, OCR,
/// TableFormer) loaded across calls — the analogue of the Rust `Pipeline`. Use
/// this instead of the per-call `convertFile` when converting many PDFs/images:
/// the one-shot functions rebuild the pipeline (reloading every model) each
/// call, whereas this loads them once.
///
/// Handles `pdf` and `image` inputs (the ML pipeline). Models load lazily on
/// first use, so constructing a `Pipeline` is cheap; the first conversion pays
/// the model-load cost. Synchronous and single-threaded — reuse one instance
/// for a sequence of documents (e.g. behind a job queue).
#[napi]
pub struct Pipeline {
    // Arc<Mutex>: the Rust pipeline needs `&mut` to convert (models are mutable
    // sessions), and the async / streaming paths run it off the JS thread. The
    // mutex serializes conversions on one instance — concurrent `*Async` calls
    // queue rather than reload models.
    inner: Arc<Mutex<RsPipeline>>,
    strict: bool,
}

#[napi]
impl Pipeline {
    /// Construct the pipeline. Only `strict` is read (cleaner Markdown);
    /// `fetchImages` / `allowedFormats` don't apply to the PDF/image pipeline.
    #[napi(constructor)]
    pub fn new(options: Option<ConverterOptions>) -> Result<Self> {
        let strict = options.and_then(|o| o.strict).unwrap_or(false);
        Ok(Self {
            inner: Arc::new(Mutex::new(RsPipeline::new().map_err(convert_err)?)),
            strict,
        })
    }

    /// Convert a PDF or image file, reusing the warm models.
    #[napi]
    pub fn convert_file(
        &self,
        path: String,
        options: Option<OutputOptions>,
    ) -> Result<ConvertResult> {
        let cfg = output_config(options, self.strict)?;
        let source = SourceDocument::from_file(&path).map_err(convert_err)?;
        Ok(run_pipeline(&self.inner, source, &cfg, self.strict)?.into_js())
    }

    /// Convert PDF or image bytes, reusing the warm models.
    #[napi]
    pub fn convert(
        &self,
        input: ConvertInput,
        options: Option<OutputOptions>,
    ) -> Result<ConvertResult> {
        let cfg = self.output_cfg(options)?;
        let source = source_from_input(input)?;
        Ok(run_pipeline(&self.inner, source, &cfg, self.strict)?.into_js())
    }

    /// Async (Promise-returning) file conversion on the warm pipeline. The
    /// CPU-bound work runs on the libuv thread pool, keeping the event loop
    /// free; calls on the same instance run one at a time (the models are
    /// mutable sessions), so overlapping Promises queue in submission order.
    #[napi(ts_return_type = "Promise<ConvertResult>")]
    pub fn convert_file_async(
        &self,
        path: String,
        options: Option<OutputOptions>,
    ) -> Result<AsyncTask<PipelineFileTask>> {
        let cfg = self.output_cfg(options)?;
        Ok(AsyncTask::new(PipelineFileTask {
            pipe: Arc::clone(&self.inner),
            strict: self.strict,
            path,
            cfg,
        }))
    }

    /// Async (Promise-returning) bytes conversion on the warm pipeline.
    #[napi(ts_return_type = "Promise<ConvertResult>")]
    pub fn convert_async(
        &self,
        input: ConvertInput,
        options: Option<OutputOptions>,
    ) -> Result<AsyncTask<PipelineBytesTask>> {
        let cfg = self.output_cfg(options)?;
        let source = source_from_input(input)?;
        Ok(AsyncTask::new(PipelineBytesTask {
            pipe: Arc::clone(&self.inner),
            strict: self.strict,
            source: Some(source),
            cfg,
        }))
    }

    /// Stream a PDF's Markdown in chunks through the warm pipeline, in document
    /// order, as pages finish converting (an image converts in one step and
    /// arrives as a single chunk).
    ///
    /// `callback` is invoked as `(err, chunk)`: once per Markdown chunk with
    /// `chunk` a string, once with `chunk === null` at the end, or once with a
    /// non-null `err` on failure. Only `placeholder` / `embedded` image modes
    /// stream; `referenced` is rejected. Prefer the `streamFileMarkdown`
    /// async-generator wrapper in JS over calling this directly.
    #[napi]
    pub fn convert_file_streaming(
        &self,
        path: String,
        callback: ThreadsafeFunction<Option<String>, ErrorStrategy::CalleeHandled>,
        options: Option<OutputOptions>,
    ) -> Result<()> {
        let cfg = self.output_cfg(options)?;
        if cfg.image_mode == ImageMode::Referenced {
            return Err(Error::new(
                Status::InvalidArg,
                "streaming supports the 'placeholder' and 'embedded' image modes; \
                 'referenced' needs the buffered convertFile / convertFileAsync",
            ));
        }
        let pipe = Arc::clone(&self.inner);
        let strict = self.strict;
        // The background thread owns the conversion and pushes each chunk
        // through the threadsafe function (which marshals back to the JS loop).
        std::thread::spawn(move || {
            stream_pipeline(&pipe, &path, &cfg, strict, &callback);
        });
        Ok(())
    }
}

impl Pipeline {
    fn output_cfg(&self, options: Option<OutputOptions>) -> Result<ConvertConfig> {
        output_config(options, self.strict)
    }
}

/// Lock the pipeline and run one buffered conversion. Free of napi/JS handle
/// types, so the async tasks call it from the libuv pool.
fn run_pipeline(
    pipe: &Mutex<RsPipeline>,
    source: SourceDocument,
    cfg: &ConvertConfig,
    strict: bool,
) -> Result<RawResult> {
    let mut pipe = pipe.lock().map_err(|_| {
        Error::new(
            Status::GenericFailure,
            "pipeline poisoned by an earlier panic",
        )
    })?;
    let mut doc = match source.format {
        InputFormat::Pdf => pipe
            .convert(&source.bytes, None, &source.name)
            .map_err(convert_err)?,
        InputFormat::Image => pipe
            .convert_image(&source.bytes, &source.name)
            .map_err(convert_err)?,
        other => {
            return Err(Error::new(
                Status::InvalidArg,
                format!(
                    "Pipeline handles pdf and image inputs (the ML pipeline); got '{}'. \
                     Use convertFile / convert for other formats.",
                    other.as_str()
                ),
            ))
        }
    };
    doc.strict_markdown = strict;
    Ok(render_doc(
        doc,
        cfg,
        source.name,
        source.format.as_str().to_string(),
        "success".to_string(),
    ))
}

/// The streaming producer body: convert through the warm pipeline and push
/// Markdown chunks through the threadsafe callback. PDF streams page by page
/// (each page's Markdown emitted in order as it finishes); an image converts in
/// one step and streams as a single chunk through the same interface.
fn stream_pipeline(
    pipe: &Mutex<RsPipeline>,
    path: &str,
    cfg: &ConvertConfig,
    strict: bool,
    callback: &ThreadsafeFunction<Option<String>, ErrorStrategy::CalleeHandled>,
) {
    let fail = |e: Error| {
        callback.call(Err(e), ThreadsafeFunctionCallMode::NonBlocking);
    };
    let source = match SourceDocument::from_file(path).map_err(convert_err) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let mut pipe = match pipe.lock() {
        Ok(p) => p,
        Err(_) => {
            return fail(Error::new(
                Status::GenericFailure,
                "pipeline poisoned by an earlier panic",
            ))
        }
    };
    // The PDF pipeline builds its document from `DoclingDocument::new` defaults,
    // so tables use the padded GitHub serializer (compact_tables = false),
    // matching the buffered path.
    let mut streamer = MarkdownStreamer::new(strict, cfg.image_mode, false);
    let emit_chunk = |chunk: String| {
        if !chunk.is_empty() {
            callback.call(Ok(Some(chunk)), ThreadsafeFunctionCallMode::NonBlocking);
        }
    };
    match source.format {
        InputFormat::Pdf => {
            let result =
                pipe.convert_streaming(&source.bytes, None, &source.name, |nodes, links| {
                    emit_chunk(streamer.push(&nodes, &links));
                    Ok(())
                });
            if let Err(e) = result {
                return fail(convert_err(e));
            }
        }
        InputFormat::Image => match pipe.convert_image(&source.bytes, &source.name) {
            Ok(doc) => emit_chunk(streamer.push(&doc.nodes, &doc.links)),
            Err(e) => return fail(convert_err(e)),
        },
        other => {
            return fail(Error::new(
                Status::InvalidArg,
                format!(
                    "Pipeline handles pdf and image inputs (the ML pipeline); got '{}'. \
                     Use DocumentConverter.convertFileStreaming for other formats.",
                    other.as_str()
                ),
            ))
        }
    }
    emit_chunk(streamer.finish());
    // End-of-stream sentinel.
    callback.call(Ok(None), ThreadsafeFunctionCallMode::NonBlocking);
}

pub struct PipelineFileTask {
    pipe: Arc<Mutex<RsPipeline>>,
    strict: bool,
    path: String,
    cfg: ConvertConfig,
}

impl Task for PipelineFileTask {
    type Output = RawResult;
    type JsValue = ConvertResult;

    fn compute(&mut self) -> Result<RawResult> {
        let source = SourceDocument::from_file(&self.path).map_err(convert_err)?;
        run_pipeline(&self.pipe, source, &self.cfg, self.strict)
    }

    fn resolve(&mut self, _env: Env, output: RawResult) -> Result<ConvertResult> {
        Ok(output.into_js())
    }
}

pub struct PipelineBytesTask {
    pipe: Arc<Mutex<RsPipeline>>,
    strict: bool,
    // `Option` so `compute` can take ownership of the (non-Copy) source.
    source: Option<SourceDocument>,
    cfg: ConvertConfig,
}

impl Task for PipelineBytesTask {
    type Output = RawResult;
    type JsValue = ConvertResult;

    fn compute(&mut self) -> Result<RawResult> {
        let source = self
            .source
            .take()
            .ok_or_else(|| Error::new(Status::GenericFailure, "conversion task reused"))?;
        run_pipeline(&self.pipe, source, &self.cfg, self.strict)
    }

    fn resolve(&mut self, _env: Env, output: RawResult) -> Result<ConvertResult> {
        Ok(output.into_js())
    }
}

/// Build a render-only [`ConvertConfig`] from per-call output options (the
/// converter-config fields are unused when rendering a document we already have).
fn output_config(out: Option<OutputOptions>, strict: bool) -> Result<ConvertConfig> {
    let out = out.unwrap_or_default();
    Ok(ConvertConfig {
        strict,
        fetch_images: false,
        allowed_formats: None,
        to: parse_output_kind(out.to.as_deref())?,
        image_mode: parse_image_mode(out.image_mode.as_deref())?,
        artifacts_dir: out.artifacts_dir.unwrap_or_else(|| "artifacts".to_string()),
    })
}

// ---------------------------------------------------------------------------
// Chunking (docling-core's HierarchicalChunker / HybridChunker).
// ---------------------------------------------------------------------------

/// Options for the chunk* functions.
#[napi(object)]
#[derive(Clone, Default)]
pub struct ChunkOptions {
    /// `"hierarchical"` (default): one chunk per document item, docling's
    /// structure-driven chunker. `"hybrid"`: tokenization-aware refinement —
    /// splits oversized chunks and merges undersized same-heading neighbours;
    /// requires `tokenizer`.
    pub chunker: Option<String>,
    /// Path to a HuggingFace `tokenizer.json` (e.g. all-MiniLM-L6-v2's) for the
    /// hybrid chunker's token counts. When omitted, falls back to
    /// `models/chunk/tokenizer.json` (populated by
    /// `scripts/install/download_dependencies.sh`).
    pub tokenizer: Option<String>,
    /// The hybrid chunker's token budget per chunk. Default `256` (docling's
    /// default for the MiniLM embedding model).
    pub max_tokens: Option<u32>,
    /// Merge undersized peer chunks with the same headings (hybrid only).
    /// Default `true`, matching docling.
    pub merge_peers: Option<bool>,
}

/// One chunk record — the analogue of docling's `DocChunk`.
#[napi(object)]
pub struct Chunk {
    /// The chunk body (markdown-flavoured text, same as docling's `DocChunk.text`).
    pub text: String,
    /// The heading path above the chunk, outermost first; absent for content
    /// above any heading.
    pub headings: Option<Vec<String>>,
    /// JSON-pointer refs of the document items the chunk was built from
    /// (`"#/texts/12"`, `"#/tables/0"`, …).
    pub doc_items: Vec<String>,
    /// The embedding-ready rendering: heading path + text, newline-joined
    /// (docling's `chunker.contextualize(chunk)`).
    pub contextualized: String,
}

/// Resolved chunker config, free of JS types (moves onto the libuv pool).
#[derive(Clone)]
struct ChunkConfig {
    hybrid: bool,
    tokenizer: Option<String>,
    max_tokens: usize,
    merge_peers: bool,
}

fn build_chunk_config(options: Option<ChunkOptions>) -> Result<ChunkConfig> {
    let o = options.unwrap_or_default();
    let hybrid = match o.chunker.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None | Some("hierarchical") => false,
        Some("hybrid") => true,
        Some(other) => {
            return Err(Error::new(
                Status::InvalidArg,
                format!("unknown chunker '{other}' (expected: hierarchical, hybrid)"),
            ))
        }
    };
    Ok(ChunkConfig {
        hybrid,
        tokenizer: o.tokenizer,
        max_tokens: o.max_tokens.unwrap_or(256) as usize,
        merge_peers: o.merge_peers.unwrap_or(true),
    })
}

/// Run the configured chunker over a converted document. Off-thread-safe.
fn run_chunker(doc: &DoclingDocument, cfg: &ChunkConfig) -> Result<Vec<Chunk>> {
    let mut chunks = Vec::new();
    run_chunker_with(doc, cfg, &mut |c| {
        chunks.push(c);
        true
    })?;
    Ok(chunks)
}

/// Sink-driven [`run_chunker`]: `sink` receives each chunk as the chunkers
/// produce it, and a `false` return cancels the chunking. Off-thread-safe.
fn run_chunker_with(
    doc: &DoclingDocument,
    cfg: &ChunkConfig,
    sink: &mut dyn FnMut(Chunk) -> bool,
) -> Result<()> {
    use docling::chunker::{contextualize, DocChunk, HierarchicalChunker, HybridChunker};
    let mut native_sink = |c: DocChunk| -> bool {
        sink(Chunk {
            contextualized: contextualize(&c),
            text: c.text,
            headings: c.headings,
            doc_items: c.doc_items.into_iter().map(|i| i.self_ref).collect(),
        })
    };
    if cfg.hybrid {
        // Explicit path, or models/chunk/tokenizer.json (the download script's
        // default location); a clear error otherwise.
        let tok = docling::chunker::HuggingFaceTokenizer::resolve(
            cfg.tokenizer.as_deref(),
            cfg.max_tokens,
        )
        .map_err(convert_err)?;
        HybridChunker::new(tok)
            .with_merge_peers(cfg.merge_peers)
            .chunk_with(doc, &mut native_sink);
    } else {
        HierarchicalChunker.chunk_with(doc, &mut native_sink);
    }
    Ok(())
}

/// Convert a source and chunk the result. The chunk text is docling-flavoured
/// Markdown (never strict), matching what docling's chunkers emit.
fn convert_and_chunk(source: SourceDocument, cfg: &ChunkConfig) -> Result<Vec<Chunk>> {
    let result = RsConverter::new().convert(source).map_err(convert_err)?;
    run_chunker(&result.document, cfg)
}

/// Chunk a file on disk with docling's chunkers: convert it, then run the
/// hierarchical (default) or hybrid chunker over the document.
#[napi]
pub fn chunk_file(path: String, options: Option<ChunkOptions>) -> Result<Vec<Chunk>> {
    let cfg = build_chunk_config(options)?;
    let source = SourceDocument::from_file(&path).map_err(convert_err)?;
    convert_and_chunk(source, &cfg)
}

/// Async (Promise-returning) [`chunk_file`]; conversion + chunking run on the
/// libuv thread pool.
#[napi(ts_return_type = "Promise<Array<Chunk>>")]
pub fn chunk_file_async(
    path: String,
    options: Option<ChunkOptions>,
) -> Result<AsyncTask<ChunkFileTask>> {
    let cfg = build_chunk_config(options)?;
    Ok(AsyncTask::new(ChunkFileTask { path, cfg }))
}

/// Chunk in-memory bytes (same contract as [`convert`], then chunk).
#[napi]
pub fn chunk(input: ConvertInput, options: Option<ChunkOptions>) -> Result<Vec<Chunk>> {
    let cfg = build_chunk_config(options)?;
    let source = source_from_input(input)?;
    convert_and_chunk(source, &cfg)
}

/// Async (Promise-returning) [`chunk`].
#[napi(ts_return_type = "Promise<Array<Chunk>>")]
pub fn chunk_async(
    input: ConvertInput,
    options: Option<ChunkOptions>,
) -> Result<AsyncTask<ChunkBytesTask>> {
    let cfg = build_chunk_config(options)?;
    let source = source_from_input(input)?;
    Ok(AsyncTask::new(ChunkBytesTask {
        source: Some(source),
        cfg,
    }))
}

/// Chunk an already-converted document, passed as docling-core JSON (the
/// `content` of a `convert*` call with `to: "json"`) — so a document converted
/// once (e.g. through the warm PDF `Pipeline`) can be chunked without
/// re-converting.
#[napi]
pub fn chunk_document(document_json: String, options: Option<ChunkOptions>) -> Result<Vec<Chunk>> {
    let cfg = build_chunk_config(options)?;
    convert_and_chunk(json_source(document_json), &cfg)
}

/// Async (Promise-returning) [`chunk_document`].
#[napi(ts_return_type = "Promise<Array<Chunk>>")]
pub fn chunk_document_async(
    document_json: String,
    options: Option<ChunkOptions>,
) -> Result<AsyncTask<ChunkBytesTask>> {
    let cfg = build_chunk_config(options)?;
    Ok(AsyncTask::new(ChunkBytesTask {
        source: Some(json_source(document_json)),
        cfg,
    }))
}

fn json_source(document_json: String) -> SourceDocument {
    SourceDocument::from_bytes(
        "document",
        InputFormat::JsonDocling,
        document_json.into_bytes(),
    )
}

// ---------------------------------------------------------------------------
// Streaming chunking: chunks are pushed to JS as the chunkers produce them.
// ---------------------------------------------------------------------------

/// Convert `source` and stream its chunks through the threadsafe callback:
/// once per chunk, `Ok(None)` at the end, `Err` on failure. A dead callback
/// (the JS side went away) cancels the chunking.
fn stream_chunks(
    source: SourceDocument,
    cfg: &ChunkConfig,
    callback: ThreadsafeFunction<Option<Chunk>, ErrorStrategy::CalleeHandled>,
) {
    let result = match RsConverter::new().convert(source).map_err(convert_err) {
        Ok(r) => r,
        Err(e) => {
            callback.call(Err(e), ThreadsafeFunctionCallMode::NonBlocking);
            return;
        }
    };
    let outcome = run_chunker_with(&result.document, cfg, &mut |chunk| {
        callback.call(Ok(Some(chunk)), ThreadsafeFunctionCallMode::NonBlocking) == Status::Ok
    });
    match outcome {
        // End-of-stream sentinel.
        Ok(()) => {
            callback.call(Ok(None), ThreadsafeFunctionCallMode::NonBlocking);
        }
        Err(e) => {
            callback.call(Err(e), ThreadsafeFunctionCallMode::NonBlocking);
        }
    }
}

/// Chunk a file and stream each chunk as the chunkers produce it — no
/// all-chunks array is materialized, and the first chunk reaches JS while the
/// rest of the document is still being chunked.
///
/// `callback` is invoked as `(err, chunk)`: once per chunk with `chunk` a
/// `Chunk`, once with `chunk === null` at the end, or once with a non-null
/// `err` on failure. Prefer the `streamFileChunks` async-generator wrapper in
/// JS over calling this directly.
#[napi]
pub fn chunk_file_streaming(
    path: String,
    callback: ThreadsafeFunction<Option<Chunk>, ErrorStrategy::CalleeHandled>,
    options: Option<ChunkOptions>,
) -> Result<()> {
    let cfg = build_chunk_config(options)?;
    // The background thread owns the conversion + chunking and pushes each
    // chunk through the threadsafe function (which marshals to the JS loop).
    std::thread::spawn(move || {
        let source = match SourceDocument::from_file(&path).map_err(convert_err) {
            Ok(s) => s,
            Err(e) => {
                callback.call(Err(e), ThreadsafeFunctionCallMode::NonBlocking);
                return;
            }
        };
        stream_chunks(source, &cfg, callback);
    });
    Ok(())
}

/// Streaming [`chunk`]: chunk in-memory bytes, pushing each chunk through the
/// callback (same contract as [`chunk_file_streaming`]). Prefer the
/// `streamChunks` async-generator wrapper in JS.
#[napi]
pub fn chunk_streaming(
    input: ConvertInput,
    callback: ThreadsafeFunction<Option<Chunk>, ErrorStrategy::CalleeHandled>,
    options: Option<ChunkOptions>,
) -> Result<()> {
    let cfg = build_chunk_config(options)?;
    let source = source_from_input(input)?;
    std::thread::spawn(move || stream_chunks(source, &cfg, callback));
    Ok(())
}

/// Streaming [`chunk_document`]: chunk an already-converted docling-core JSON
/// document, pushing each chunk through the callback (same contract as
/// [`chunk_file_streaming`]). Prefer the `streamDocumentChunks`
/// async-generator wrapper in JS.
#[napi]
pub fn chunk_document_streaming(
    document_json: String,
    callback: ThreadsafeFunction<Option<Chunk>, ErrorStrategy::CalleeHandled>,
    options: Option<ChunkOptions>,
) -> Result<()> {
    let cfg = build_chunk_config(options)?;
    let source = json_source(document_json);
    std::thread::spawn(move || stream_chunks(source, &cfg, callback));
    Ok(())
}

pub struct ChunkFileTask {
    path: String,
    cfg: ChunkConfig,
}

impl Task for ChunkFileTask {
    type Output = Vec<Chunk>;
    type JsValue = Vec<Chunk>;

    fn compute(&mut self) -> Result<Vec<Chunk>> {
        let source = SourceDocument::from_file(&self.path).map_err(convert_err)?;
        convert_and_chunk(source, &self.cfg)
    }

    fn resolve(&mut self, _env: Env, output: Vec<Chunk>) -> Result<Vec<Chunk>> {
        Ok(output)
    }
}

pub struct ChunkBytesTask {
    // `Option` so `compute` can take ownership of the (non-Copy) source.
    source: Option<SourceDocument>,
    cfg: ChunkConfig,
}

impl Task for ChunkBytesTask {
    type Output = Vec<Chunk>;
    type JsValue = Vec<Chunk>;

    fn compute(&mut self) -> Result<Vec<Chunk>> {
        let source = self
            .source
            .take()
            .ok_or_else(|| Error::new(Status::GenericFailure, "chunking task reused"))?;
        convert_and_chunk(source, &self.cfg)
    }

    fn resolve(&mut self, _env: Env, output: Vec<Chunk>) -> Result<Vec<Chunk>> {
        Ok(output)
    }
}

// ---------------------------------------------------------------------------
// Format helpers exposed to JS.
// ---------------------------------------------------------------------------

/// The list of supported input format ids.
#[napi]
pub fn supported_formats() -> Vec<String> {
    [
        "docx",
        "pptx",
        "html",
        "image",
        "pdf",
        "asciidoc",
        "md",
        "csv",
        "xlsx",
        "odt",
        "ods",
        "odp",
        "xml_uspto",
        "xml_jats",
        "xml_xbrl",
        "mets_gbs",
        "json_docling",
        "xml_doclang",
        "dclx",
        "vtt",
        "latex",
        "email",
        "epub",
        "mhtml",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Detect a format id from a filename or extension (e.g. `"report.pdf"` →
/// `"pdf"`). Returns `null` for unknown extensions.
#[napi]
pub fn format_from_name(name: String) -> Option<String> {
    infer_format(&name).map(|f| f.as_str().to_string())
}

// ---------------------------------------------------------------------------
// Non-exported helpers.
// ---------------------------------------------------------------------------

fn infer_format(name: &str) -> Option<InputFormat> {
    let ext = name.rsplit('.').next().filter(|e| *e != name)?;
    InputFormat::from_extension(ext)
}

fn parse_output_kind(to: Option<&str>) -> Result<OutputKind> {
    match to.map(str::to_ascii_lowercase).as_deref() {
        None | Some("md") | Some("markdown") => Ok(OutputKind::Markdown),
        Some("json") => Ok(OutputKind::Json),
        Some(other) => Err(Error::new(
            Status::InvalidArg,
            format!("unknown `to` '{other}' (expected: markdown, json)"),
        )),
    }
}

fn parse_image_mode(mode: Option<&str>) -> Result<ImageMode> {
    match mode.map(str::to_ascii_lowercase).as_deref() {
        None | Some("placeholder") => Ok(ImageMode::Placeholder),
        Some("embedded") => Ok(ImageMode::Embedded),
        Some("referenced") => Ok(ImageMode::Referenced),
        Some(other) => Err(Error::new(
            Status::InvalidArg,
            format!("unknown imageMode '{other}' (expected: placeholder, embedded, referenced)"),
        )),
    }
}

/// Resolve a user-supplied format string — a format id (as reported by
/// [`supported_formats`]) or a file extension — to an [`InputFormat`].
fn parse_format(s: &str) -> Result<InputFormat> {
    let key = s.trim().trim_start_matches('.').to_ascii_lowercase();
    // Extensions first (covers ".html", "jpg", "eml", …); then format ids for
    // the ones extensions don't name (e.g. "image", "xml_uspto").
    if let Some(f) = InputFormat::from_extension(&key) {
        return Ok(f);
    }
    let f = match key.as_str() {
        "image" => InputFormat::Image,
        "asciidoc" => InputFormat::Asciidoc,
        "markdown" => InputFormat::Md,
        "xml_uspto" | "uspto" => InputFormat::XmlUspto,
        "xml_jats" | "jats" => InputFormat::XmlJats,
        "xml_xbrl" | "xbrl" => InputFormat::XmlXbrl,
        "json_docling" => InputFormat::JsonDocling,
        "xml_doclang" | "doclang" => InputFormat::XmlDoclang,
        "mets_gbs" => InputFormat::MetsGbs,
        "email" => InputFormat::Email,
        "latex" => InputFormat::Latex,
        _ => {
            return Err(Error::new(
                Status::InvalidArg,
                format!("unknown format '{s}'"),
            ))
        }
    };
    Ok(f)
}

fn status_str(status: ConversionStatus) -> String {
    match status {
        ConversionStatus::Success => "success",
        ConversionStatus::PartialSuccess => "partial_success",
        ConversionStatus::Failure => "failure",
    }
    .to_string()
}

fn convert_err(e: impl std::fmt::Display) -> Error {
    Error::new(Status::GenericFailure, e.to_string())
}
