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
    /// Named Whisper model preset for audio sources (English-only /
    /// Distil-Whisper variants under `.models/asr/<preset>/`).
    pub asr_model: Option<String>,
    /// ASR transcription language for audio/video: a Whisper code (`"en"`,
    /// `"de"`, …) or `"auto"` (default) — detected from the first 30 seconds.
    pub asr_lang: Option<String>,
    /// Character encoding of text inputs (Markdown, CSV, AsciiDoc, WebVTT,
    /// XML, …) — docling's `TextBackendOptions.encoding`: a WHATWG label
    /// (`"shift_jis"`, `"koi8-r"`, `"windows-1251"`). Unset = detect (BOM,
    /// UTF-8, then windows-1252); bytes it cannot decode fail the conversion.
    pub encoding: Option<String>,
    /// Max frames sampled from a video input as timestamped pictures (needs
    /// the ffmpeg binary at runtime; `0` = transcript only). Default 8.
    pub video_frames: Option<u32>,
    /// Convert only this PDF page window: `"A-B"` or a single page `"N"`
    /// (1-based inclusive — issue #80). Other formats ignore it.
    pub pages: Option<String>,
    /// OCR recognition language for scanned PDF/image pages: `"en"` (default;
    /// proper Latin word spacing) or `"ch"` (the multilingual
    /// docling-conformance model), or a BCP-47 tag for either language —
    /// `"en-US"`, `"eng"`, `"zh"`, `"zh-Hans"`, `"zh-TW"`, docling's `iso:`
    /// prefix accepted (#388); script and region subtags are ignored. Any
    /// other language is an error. Formats that never OCR ignore it.
    pub ocr_lang: Option<String>,
    /// Which regions feed the OCR (docling's `OcrMode`, #254): `"default"` |
    /// `"full_page"` | `"layout_regions"` | `"pdf_aware_layout_regions"`.
    /// `full_page`/`layout_regions` discard the text layer like
    /// `forceFullPageOcr`.
    pub ocr_mode: Option<String>,
    /// OCR render scale in px per PDF point (docling's `OcrOptions.scale`,
    /// #254); unset reads the pipeline's own 2.0 px/pt render (docling's
    /// default is 3 = 216 dpi).
    pub ocr_scale: Option<f64>,
    /// Email (.eml/.msg): append an Attachments section — names and content
    /// types only, never the payload (#251). Default `false`.
    pub list_attachments: Option<bool>,
    /// Omit empty cells from sparse XLSX/XLS table grids (#271; docling.rs
    /// extension). Default `false`.
    pub skip_empty_cells: Option<bool>,
    /// Unpadded `| a | b |` Markdown tables, all formats (#271; docling.rs
    /// extension). Default `false`.
    pub compact_tables: Option<bool>,
    /// EBCDIC (#252): copybook layout as inline `EbcdicLayout` JSON or a
    /// file path; defaults to the `<stem>.layout.json` sidecar.
    pub ebcdic_layout: Option<String>,
    /// Keep layout + TableFormer, never OCR (#244) — docling's independent
    /// `do_ocr=False`. Structured output survives; text that exists only as
    /// pixels (scanned pages, text inside images) comes back empty.
    /// Default `false`.
    pub skip_ocr: Option<bool>,
    /// OCR every PDF page even when it carries an embedded text layer
    /// (docling's `force_full_page_ocr`) — for text layers that exist but lie.
    /// Default `false`.
    pub force_full_page_ocr: Option<bool>,
    /// Keep every detected picture as a picture: disable the demotion of
    /// uncaptioned dense-text "picture" regions into paragraphs (the escape
    /// hatch for image-extraction workflows, #173). Default `false`.
    pub no_text_panels: Option<bool>,
    /// Infer PDF/image section-header levels after assembly (docling's
    /// `HeadingHierarchyModel`, #302): PDF bookmarks are authoritative, then
    /// legal/outline numbering, then font style. Default `false` — headings
    /// then keep the flat detected level.
    pub heading_hierarchy: Option<bool>,
    /// Classify every PDF/image picture with DocumentFigureClassifier
    /// (docling's `do_picture_classification`, #423): the 26-class prediction
    /// lands on the JSON picture item's `classification`. Needs
    /// `.models/picture_classifier.onnx`; a missing model warns and skips the
    /// pass. Default `false`.
    pub do_picture_classification: Option<bool>,
    /// Rewrite detected code blocks (and detect their language) with the
    /// CodeFormulaV2 VLM (docling's `do_code_enrichment`, #423). Needs
    /// `.models/code_formula/`; an autoregressive decode per code region —
    /// seconds each on CPU. Default `false`.
    pub do_code_enrichment: Option<bool>,
    /// Decode display formulas to LaTeX with CodeFormulaV2 (docling's
    /// `do_formula_enrichment`, #423): Markdown renders `$$latex$$` instead
    /// of the formula placeholder comment. Same model and cost as
    /// `doCodeEnrichment`. Default `false`.
    pub do_formula_enrichment: Option<bool>,
    /// `"standard"` (default) or `"vlm"` (#77): replace the whole ONNX stack —
    /// layout, OCR, TableFormer — with a remote OpenAI-compatible vision
    /// endpoint, which converts each rendered page on its own. PDF and image
    /// inputs only; any other format is rejected rather than silently falling
    /// back, matching the CLI's `--pipeline vlm`.
    ///
    /// Selecting it explicitly is deliberate: the `DOCLING_RS_VLM_*` variables
    /// below are fallbacks, never triggers. A stale `DOCLING_RS_VLM_ENDPOINT`
    /// left in the environment must not silently route every PDF over the
    /// network.
    ///
    /// By the same rule the `vlm_*` options below are read **only** under
    /// `"vlm"`. Passing them with the standard pipeline is not an error and
    /// does not switch pipelines — they are ignored, exactly as the CLI drops
    /// a stray `--vlm-endpoint` given without `--pipeline vlm`. They configure
    /// the VLM; `pipeline` alone selects it.
    pub pipeline: Option<String>,
    /// VLM server: the base `/v1` URL or the full `…/chat/completions` one —
    /// the suffix is appended when missing. Falls back to
    /// `$DOCLING_RS_VLM_ENDPOINT`; required when `pipeline` is `"vlm"`.
    pub vlm_endpoint: Option<String>,
    /// VLM model name as the server knows it (e.g. `"granite-docling"`).
    /// Falls back to `$DOCLING_RS_VLM_MODEL`; required when `pipeline` is `"vlm"`.
    pub vlm_model: Option<String>,
    /// Bearer token for the VLM endpoint; local servers (LM Studio, Ollama,
    /// vLLM) need none. Falls back to `$DOCLING_RS_VLM_API_KEY`.
    pub vlm_api_key: Option<String>,
    /// Instruction sent with every page image, overriding docling's
    /// DocLang-eliciting default. Falls back to `$DOCLING_RS_VLM_PROMPT`.
    pub vlm_prompt: Option<String>,
    /// `max_tokens` for each page completion. Default `8192` — a dense page of
    /// DocLang runs long, and a truncated answer loses the tail of the page.
    pub vlm_max_tokens: Option<u32>,
    /// Emit cleaner, more conformant Markdown (code-fence languages preserved,
    /// no inline-run spacing artifacts) instead of docling's byte-for-byte
    /// legacy output. Markdown only. Default `false`.
    pub strict: Option<bool>,
    /// For HTML/EPUB/MHTML/JATS, resolve external `<img src>` (data: URIs, local
    /// files, http(s) URLs, EPUB/MHTML archive parts, JATS `<graphic>` files)
    /// and embed the bytes. Off by default; when on,
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
    /// `"markdown"` (default), `"json"` (docling-core DoclingDocument wire
    /// format) or `"latex"` (a complete LaTeX document, #317).
    pub to: Option<String>,
    /// Picture handling for Markdown: `"placeholder"` (default), `"embedded"`
    /// (base64 data URIs inline), or `"referenced"` (returns image files in
    /// `images`). Ignored for JSON, which always embeds images as data URIs.
    pub image_mode: Option<String>,
    /// Directory name used in `referenced` image links. Default `"artifacts"`.
    pub artifacts_dir: Option<String>,
    /// Text inserted between pages in Markdown output — docling's
    /// `export_to_markdown(page_break_placeholder=…)`, e.g.
    /// `"<!-- page break -->"`. A break lands only between two rendered
    /// blocks on different pages (PDF/image pages, slides, sheets, DjVu
    /// pages) — never first or last, empty pages collapse. Unset: no page
    /// breaks, docling's default. Markdown only.
    pub page_break_placeholder: Option<String>,
}

/// All options for the one-shot module-level functions (converter config +
/// output options in a single object).
#[napi(object)]
#[derive(Clone, Default)]
pub struct ConvertOptions {
    pub strict: Option<bool>,
    pub fetch_images: Option<bool>,
    /// Named Whisper model preset for audio sources.
    pub asr_model: Option<String>,
    /// ASR transcription language for audio/video: a Whisper code (`"en"`,
    /// `"de"`, …) or `"auto"` (default) — detected from the first 30 seconds.
    pub asr_lang: Option<String>,
    /// Character encoding of text inputs (docling's
    /// `TextBackendOptions.encoding`); unset = detect.
    pub encoding: Option<String>,
    /// Max frames sampled from a video input (`0` = transcript only).
    pub video_frames: Option<u32>,
    /// PDF page window `"A-B"` (or `"N"`), 1-based inclusive (#80).
    pub pages: Option<String>,
    /// OCR recognition language for scanned pages: `"en"` (default) | `"ch"`,
    /// or a BCP-47 tag for either (`"en-US"`, `"zh-Hans"`, #388).
    pub ocr_lang: Option<String>,
    /// Which regions feed the OCR (docling's `OcrMode`, #254): `"default"` |
    /// `"full_page"` | `"layout_regions"` | `"pdf_aware_layout_regions"`.
    pub ocr_mode: Option<String>,
    /// OCR render scale in px per PDF point (docling's `OcrOptions.scale`,
    /// #254); unset reads the pipeline's own 2.0 px/pt render.
    pub ocr_scale: Option<f64>,
    /// Email (.eml/.msg): append an Attachments section — names and content
    /// types only, never the payload (#251). Default `false`.
    pub list_attachments: Option<bool>,
    /// Omit empty cells from sparse XLSX/XLS table grids (#271). Default
    /// `false`.
    pub skip_empty_cells: Option<bool>,
    /// Unpadded Markdown tables (#271). Default `false`.
    pub compact_tables: Option<bool>,
    /// EBCDIC (#252): copybook layout as inline `EbcdicLayout` JSON or a
    /// file path; defaults to the `<stem>.layout.json` sidecar.
    pub ebcdic_layout: Option<String>,
    /// Keep layout + TableFormer, never OCR (#244) — docling's independent
    /// `do_ocr=False`. Structured output survives; text that exists only as
    /// pixels (scanned pages, text inside images) comes back empty.
    /// Default `false`.
    pub skip_ocr: Option<bool>,
    /// OCR every PDF page even when it carries a text layer (docling's
    /// `force_full_page_ocr`). Default `false`.
    pub force_full_page_ocr: Option<bool>,
    /// Keep every detected picture as a picture: disable text-panel demotion
    /// (#173). Default `false`.
    pub no_text_panels: Option<bool>,
    /// Infer PDF/image section-header levels after assembly (#302). Default
    /// `false`.
    pub heading_hierarchy: Option<bool>,
    /// Opt-in enrichment models (#423): picture classification, code rewrite
    /// + language, formula LaTeX. See [`ConverterOptions`]. Default `false`.
    pub do_picture_classification: Option<bool>,
    pub do_code_enrichment: Option<bool>,
    pub do_formula_enrichment: Option<bool>,
    /// `"standard"` (default) or `"vlm"` (#77): convert PDF/image pages
    /// through a remote OpenAI-compatible vision endpoint instead of the ONNX
    /// stack. The `vlm_*` options below take effect only under `"vlm"` and are
    /// ignored otherwise. See [`ConverterOptions::pipeline`] for the full
    /// contract.
    pub pipeline: Option<String>,
    /// VLM server, base `/v1` or full `…/chat/completions` URL. Falls back to
    /// `$DOCLING_RS_VLM_ENDPOINT`.
    pub vlm_endpoint: Option<String>,
    /// VLM model name. Falls back to `$DOCLING_RS_VLM_MODEL`.
    pub vlm_model: Option<String>,
    /// Bearer token for the VLM endpoint. Falls back to `$DOCLING_RS_VLM_API_KEY`.
    pub vlm_api_key: Option<String>,
    /// Per-page instruction, overriding docling's default DocLang prompt.
    /// Falls back to `$DOCLING_RS_VLM_PROMPT`.
    pub vlm_prompt: Option<String>,
    /// `max_tokens` per page completion. Default `8192`.
    pub vlm_max_tokens: Option<u32>,
    pub allowed_formats: Option<Vec<String>>,
    pub to: Option<String>,
    pub image_mode: Option<String>,
    pub artifacts_dir: Option<String>,
    /// See [`OutputOptions::page_break_placeholder`].
    pub page_break_placeholder: Option<String>,
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
    asr_model: Option<String>,
    asr_lang: Option<String>,
    encoding: Option<String>,
    video_frames: Option<usize>,
    page_range: Option<(usize, usize)>,
    ocr_lang: Option<String>,
    ocr_mode: Option<String>,
    ocr_scale: Option<f32>,
    list_attachments: bool,
    skip_empty_cells: bool,
    compact_tables: bool,
    ebcdic_layout: Option<String>,
    skip_ocr: bool,
    force_full_page_ocr: bool,
    no_text_panels: bool,
    heading_hierarchy: bool,
    /// Opt-in enrichment passes (#423), all off by default.
    enrich: docling::EnrichmentOptions,
    /// `Some` only for `pipeline: "vlm"` (#77), already resolved against the
    /// `DOCLING_RS_VLM_*` environment. Its presence *is* the pipeline switch:
    /// [`run_convert`] short-circuits the whole ML stack when it is set.
    vlm: Option<docling::vlm::VlmOptions>,
    allowed_formats: Option<Vec<InputFormat>>,
    to: OutputKind,
    image_mode: ImageMode,
    artifacts_dir: String,
    /// docling's `page_break_placeholder` for the Markdown export.
    page_break_placeholder: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum OutputKind {
    Markdown,
    Json,
    /// A complete LaTeX document (docling 2.124's `--to latex`, #317).
    Latex,
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

fn build_config(o: ConvertOptions) -> Result<ConvertConfig> {
    let allowed = match o.allowed_formats {
        Some(list) => Some(
            list.iter()
                .map(|s| parse_format(s))
                .collect::<Result<Vec<_>>>()?,
        ),
        None => None,
    };
    let page_range = parse_pages(o.pages.as_deref())?;
    Ok(ConvertConfig {
        strict: o.strict.unwrap_or(false),
        fetch_images: o.fetch_images.unwrap_or(false),
        asr_model: o.asr_model,
        asr_lang: o.asr_lang,
        encoding: o.encoding,
        video_frames: o.video_frames.map(|n| n as usize),
        page_range,
        ocr_lang: parse_ocr_lang(o.ocr_lang)?,
        ocr_mode: parse_ocr_mode(o.ocr_mode)?,
        ocr_scale: parse_ocr_scale(o.ocr_scale)?,
        list_attachments: o.list_attachments.unwrap_or(false),
        skip_empty_cells: o.skip_empty_cells.unwrap_or(false),
        compact_tables: o.compact_tables.unwrap_or(false),
        ebcdic_layout: o.ebcdic_layout,
        skip_ocr: o.skip_ocr.unwrap_or(false),
        force_full_page_ocr: o.force_full_page_ocr.unwrap_or(false),
        no_text_panels: o.no_text_panels.unwrap_or(false),
        heading_hierarchy: o.heading_hierarchy.unwrap_or(false),
        enrich: enrichments(
            o.do_picture_classification,
            o.do_code_enrichment,
            o.do_formula_enrichment,
        ),
        vlm: resolve_vlm(
            o.pipeline.as_deref(),
            o.vlm_endpoint,
            o.vlm_model,
            o.vlm_api_key,
            o.vlm_prompt,
            o.vlm_max_tokens,
            page_range,
        )?,
        allowed_formats: allowed,
        to: parse_output_kind(o.to.as_deref())?,
        image_mode: parse_image_mode(o.image_mode.as_deref())?,
        artifacts_dir: o.artifacts_dir.unwrap_or_else(|| "artifacts".to_string()),
        page_break_placeholder: o.page_break_placeholder,
    })
}

/// Resolve the `pipeline` selection into the VLM options the conversion needs
/// (#77), or `None` for the standard ONNX pipeline.
///
/// [`docling::vlm::VlmOptions::resolve`] does the endpoint/model fallback onto
/// `DOCLING_RS_VLM_ENDPOINT` / `_MODEL` and reads `_PROMPT` / `_API_KEY` itself,
/// so going through it — rather than building the struct field by field — is
/// what makes the environment behave identically from Node and from the CLI.
/// The explicit options then override whatever the environment supplied.
///
/// Resolution happens at option-parsing time, not at conversion time, so a
/// missing endpoint fails fast on the call (or on the `DocumentConverter`
/// constructor) instead of after a file has been read.
///
/// The standard branch drops the `vlm_*` arguments instead of rejecting them:
/// an option configures the VLM, only `pipeline` selects it, and that is one
/// rule shared with the CLI — which parses `--vlm-endpoint` / `--vlm-model`
/// and ignores them without `--pipeline vlm`. Pinned by
/// `standard_pipeline_ignores_vlm_options` below and by the Node-side smoke
/// check, so the ignore stays a decision rather than resurfacing as a bug.
/// The three enrichment switches as the engine's option set (#423); unset
/// and `false` both mean off.
fn enrichments(
    picture_classification: Option<bool>,
    code: Option<bool>,
    formula: Option<bool>,
) -> docling::EnrichmentOptions {
    docling::EnrichmentOptions {
        picture_classification: picture_classification.unwrap_or(false),
        code: code.unwrap_or(false),
        formula: formula.unwrap_or(false),
    }
}

fn resolve_vlm(
    pipeline: Option<&str>,
    endpoint: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    prompt: Option<String>,
    max_tokens: Option<u32>,
    page_range: Option<(usize, usize)>,
) -> Result<Option<docling::vlm::VlmOptions>> {
    // An empty string counts as unset, the way `docling_core::env::nonempty`
    // treats the variables these options fall back to. `vlmEndpoint:
    // process.env.VLM_URL ?? ''` is an easy shape to write, and it must reach
    // the env fallback rather than resolve to an empty endpoint.
    let set = |s: Option<String>| s.filter(|v| !v.trim().is_empty());
    match pipeline {
        // Intentionally without inspecting the `vlm_*` arguments: see above.
        None | Some("standard") => Ok(None),
        Some("vlm") => {
            let (endpoint, model) = (set(endpoint), set(model));
            let (api_key, prompt) = (set(api_key), set(prompt));
            let mut v = docling::vlm::VlmOptions::resolve(endpoint, model).map_err(|e| {
                // InvalidArg, not GenericFailure: a missing endpoint/model is a
                // bad call, not a conversion that went wrong.
                Error::new(Status::InvalidArg, e.to_string())
            })?;
            if prompt.is_some() {
                v.prompt = prompt;
            }
            if api_key.is_some() {
                v.api_key = api_key;
            }
            // Validated like `ocrScale`: 0 would have every page come back
            // empty and surface as "the model's responses contained no
            // parseable DocLang" — a message pointing at the model, not at the
            // option. (napi applies JS ToUint32, so a negative number arrives
            // here as a huge one; the server rejects that on its own terms.)
            match max_tokens {
                Some(0) => {
                    return Err(Error::new(
                        Status::InvalidArg,
                        "vlmMaxTokens must be greater than 0",
                    ))
                }
                Some(n) => v.max_tokens = n as usize,
                None => {}
            }
            // `pages` composes with the VLM exactly as with the ML pipeline —
            // only the selected pages are rendered and sent.
            v.page_range = page_range;
            Ok(Some(v))
        }
        Some(other) => Err(Error::new(
            Status::InvalidArg,
            format!("unknown pipeline '{other}' (expected: standard, vlm)"),
        )),
    }
}

/// Validate an `ocrLang` option (`"en"`/`"ch"` or a BCP-47 tag for English /
/// Chinese, #388); an unknown language is an error.
fn parse_ocr_lang(s: Option<String>) -> Result<Option<String>> {
    match s {
        Some(v) if docling::OcrLang::parse(&v).is_some() => Ok(Some(v)),
        Some(v) => Err(Error::from_reason(format!(
            "ocrLang {v:?} is not a supported OCR language ({})",
            docling::OcrLang::ACCEPTED
        ))),
        None => Ok(None),
    }
}

/// Validate an `ocrMode` option (#254); an unknown id is an error.
fn parse_ocr_mode(s: Option<String>) -> Result<Option<String>> {
    match s {
        Some(v) if docling::OcrMode::parse(&v).is_some() => Ok(Some(v)),
        Some(v) => Err(Error::from_reason(format!(
            "ocrMode {v:?} is not default|full_page|layout_regions|pdf_aware_layout_regions"
        ))),
        None => Ok(None),
    }
}

/// Validate an `ocrScale` option (#254); non-positive values are an error.
fn parse_ocr_scale(s: Option<f64>) -> Result<Option<f32>> {
    match s {
        Some(v) if v.is_finite() && v > 0.0 => Ok(Some(v as f32)),
        Some(v) => Err(Error::from_reason(format!(
            "ocrScale must be a positive number, got {v}"
        ))),
        None => Ok(None),
    }
}

/// `"A-B"` / `"N"` → the converter's 1-based inclusive page window (#80).
fn parse_pages(s: Option<&str>) -> Result<Option<(usize, usize)>> {
    s.map(|v| docling::parse_page_range(v).map_err(|e| Error::from_reason(format!("pages: {e}"))))
        .transpose()
}

fn build_converter(cfg: &ConvertConfig) -> RsConverter {
    let base = match &cfg.allowed_formats {
        Some(list) => RsConverter::with_allowed_formats(list.iter().copied()),
        None => RsConverter::new(),
    };
    let base = base
        .strict(cfg.strict)
        .fetch_images(cfg.fetch_images)
        .list_attachments(cfg.list_attachments)
        .skip_empty_cells(cfg.skip_empty_cells)
        .compact_tables(cfg.compact_tables)
        .page_break_placeholder(cfg.page_break_placeholder.clone())
        .ebcdic_layout_opt(cfg.ebcdic_layout.clone())
        .skip_ocr(cfg.skip_ocr)
        .force_full_page_ocr(cfg.force_full_page_ocr)
        .no_text_panels(cfg.no_text_panels)
        .heading_hierarchy(cfg.heading_hierarchy)
        .do_picture_classification(cfg.enrich.picture_classification)
        .do_code_enrichment(cfg.enrich.code)
        .do_formula_enrichment(cfg.enrich.formula)
        .asr_model(cfg.asr_model.clone())
        .asr_lang(cfg.asr_lang.clone())
        .encoding(cfg.encoding.clone());
    let base = match cfg.video_frames {
        Some(max) => base.video_frames(max),
        None => base,
    };
    let base = match cfg.page_range {
        Some((first, last)) => base.page_range(first, last),
        None => base,
    };
    let base = match &cfg.ocr_lang {
        Some(lang) => base.ocr_lang(lang.clone()),
        None => base,
    };
    let base = match &cfg.ocr_mode {
        Some(mode) => base.ocr_mode(mode.clone()),
        None => base,
    };
    match cfg.ocr_scale {
        Some(s) => base.ocr_scale(s),
        None => base,
    }
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
        OutputKind::Latex => (doc.export_to_latex(), Vec::new()),
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

/// Enforce the `allowedFormats` restriction. The VLM branches convert without
/// going through `DocumentConverter`, which is where that check normally lives
/// — so they have to run it themselves, with the same error the standard path
/// raises, or the restriction would silently lapse under `pipeline: "vlm"`.
fn check_allowed(source: &SourceDocument, cfg: &ConvertConfig) -> Result<()> {
    match &cfg.allowed_formats {
        Some(allowed) if !allowed.contains(&source.format) => Err(convert_err(
            docling::ConversionError::UnsupportedFormat(source.format),
        )),
        _ => Ok(()),
    }
}

/// Run a buffered conversion and render it per the config. Runs off the JS
/// thread for the async path, so it must stay free of napi/JS types.
fn run_convert(source: SourceDocument, cfg: &ConvertConfig) -> Result<RawResult> {
    // #77: the remote VLM replaces the entire ML stack — no ONNX model is
    // loaded, and every other converter knob (OCR, TableFormer, text panels)
    // has nothing to act on. `convert_vlm` fails the whole document if a single
    // page fails, so there is no partial_success to report here.
    if let Some(vlm) = &cfg.vlm {
        check_allowed(&source, cfg)?;
        let format = source.format.as_str().to_string();
        let mut document = docling::vlm::convert_vlm(&source, vlm).map_err(convert_err)?;
        // The serializer knobs `DocumentConverter::convert` would have applied
        // (converter.rs) — this path never reaches it, so they are set here or
        // they silently lapse under `pipeline: "vlm"`.
        document.strict_markdown = cfg.strict;
        document.compact_tables = cfg.compact_tables;
        document.page_break_placeholder = cfg.page_break_placeholder.clone();
        return Ok(render_doc(
            document,
            cfg,
            source.name,
            format,
            "success".to_string(),
        ));
    }
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
/// HTML/EPUB/JATS image fetching) resolves relative `<img src>` / `<graphic>`
/// paths against the file's directory.
#[napi]
pub fn convert_file(path: String, options: Option<ConvertOptions>) -> Result<ConvertResult> {
    let o = options.unwrap_or_default();
    let cfg = build_config(o)?;
    let source = SourceDocument::from_file(&path).map_err(convert_err)?;
    Ok(run_convert(source, &cfg)?.into_js())
}

/// Convert in-memory bytes.
#[napi]
pub fn convert(input: ConvertInput, options: Option<ConvertOptions>) -> Result<ConvertResult> {
    let o = options.unwrap_or_default();
    let cfg = build_config(o)?;
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
    let cfg = build_config(o)?;
    Ok(AsyncTask::new(ConvertFileTask { path, cfg }))
}

/// Async (Promise-returning) [`convert`].
#[napi(ts_return_type = "Promise<ConvertResult>")]
pub fn convert_async(
    input: ConvertInput,
    options: Option<ConvertOptions>,
) -> Result<AsyncTask<ConvertBytesTask>> {
    let o = options.unwrap_or_default();
    let cfg = build_config(o)?;
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
    asr_model: Option<String>,
    asr_lang: Option<String>,
    encoding: Option<String>,
    video_frames: Option<usize>,
    page_range: Option<(usize, usize)>,
    ocr_lang: Option<String>,
    ocr_mode: Option<String>,
    ocr_scale: Option<f32>,
    list_attachments: bool,
    skip_empty_cells: bool,
    compact_tables: bool,
    ebcdic_layout: Option<String>,
    skip_ocr: bool,
    force_full_page_ocr: bool,
    no_text_panels: bool,
    heading_hierarchy: bool,
    enrich: docling::EnrichmentOptions,
    // Resolved once in the constructor and cloned per call: a converter is
    // configuration, so a missing endpoint should surface at `new`, and the
    // `DOCLING_RS_VLM_*` environment should be read at the same moment every
    // other option is.
    vlm: Option<docling::vlm::VlmOptions>,
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
        let page_range = parse_pages(o.pages.as_deref())?;
        Ok(Self {
            strict: o.strict.unwrap_or(false),
            fetch_images: o.fetch_images.unwrap_or(false),
            asr_model: o.asr_model.clone(),
            asr_lang: o.asr_lang.clone(),
            encoding: o.encoding.clone(),
            video_frames: o.video_frames.map(|n| n as usize),
            page_range,
            ocr_lang: parse_ocr_lang(o.ocr_lang.clone())?,
            ocr_mode: parse_ocr_mode(o.ocr_mode.clone())?,
            ocr_scale: parse_ocr_scale(o.ocr_scale)?,
            list_attachments: o.list_attachments.unwrap_or(false),
            skip_empty_cells: o.skip_empty_cells.unwrap_or(false),
            compact_tables: o.compact_tables.unwrap_or(false),
            ebcdic_layout: o.ebcdic_layout.clone(),
            skip_ocr: o.skip_ocr.unwrap_or(false),
            force_full_page_ocr: o.force_full_page_ocr.unwrap_or(false),
            no_text_panels: o.no_text_panels.unwrap_or(false),
            heading_hierarchy: o.heading_hierarchy.unwrap_or(false),
            enrich: enrichments(
                o.do_picture_classification,
                o.do_code_enrichment,
                o.do_formula_enrichment,
            ),
            vlm: resolve_vlm(
                o.pipeline.as_deref(),
                o.vlm_endpoint.clone(),
                o.vlm_model.clone(),
                o.vlm_api_key.clone(),
                o.vlm_prompt.clone(),
                o.vlm_max_tokens,
                page_range,
            )?,
            allowed_formats: allowed,
        })
    }

    fn config(&self, out: Option<OutputOptions>) -> Result<ConvertConfig> {
        let out = out.unwrap_or_default();
        Ok(ConvertConfig {
            strict: self.strict,
            fetch_images: self.fetch_images,
            asr_model: self.asr_model.clone(),
            asr_lang: self.asr_lang.clone(),
            encoding: self.encoding.clone(),
            video_frames: self.video_frames,
            page_range: self.page_range,
            ocr_lang: self.ocr_lang.clone(),
            ocr_mode: self.ocr_mode.clone(),
            ocr_scale: self.ocr_scale,
            list_attachments: self.list_attachments,
            skip_empty_cells: self.skip_empty_cells,
            compact_tables: self.compact_tables,
            ebcdic_layout: self.ebcdic_layout.clone(),
            skip_ocr: self.skip_ocr,
            force_full_page_ocr: self.force_full_page_ocr,
            no_text_panels: self.no_text_panels,
            heading_hierarchy: self.heading_hierarchy,
            enrich: self.enrich,
            vlm: self.vlm.clone(),
            allowed_formats: self.allowed_formats.clone(),
            to: parse_output_kind(out.to.as_deref())?,
            image_mode: parse_image_mode(out.image_mode.as_deref())?,
            artifacts_dir: out.artifacts_dir.unwrap_or_else(|| "artifacts".to_string()),
            page_break_placeholder: out.page_break_placeholder,
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
    /// non-null `err` on failure. Every image mode streams here, `referenced`
    /// included (its links resolve against `artifactsDir`) — unlike the warm
    /// [`Pipeline`]'s streaming, which rejects it. Prefer the
    /// `streamFileMarkdown` async-generator wrapper in JS over calling this
    /// directly.
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
            // #77: nothing streams out of the VLM — a whole page is one request
            // and the answer only parses once complete, so there is no earlier
            // moment to emit. Convert buffered and push the document as a single
            // chunk, which keeps the generator's contract intact (concatenating
            // the chunks still reproduces the buffered Markdown byte-for-byte).
            if let Some(vlm) = &cfg.vlm {
                if let Err(e) = check_allowed(&source, &cfg) {
                    callback.call(Err(e), ThreadsafeFunctionCallMode::NonBlocking);
                    return;
                }
                let doc = match docling::vlm::convert_vlm(&source, vlm) {
                    Ok(d) => d,
                    Err(e) => {
                        callback.call(Err(convert_err(e)), ThreadsafeFunctionCallMode::NonBlocking);
                        return;
                    }
                };
                // `with_artifacts`, not `new`: `new` carries a debug_assert
                // against `Referenced` (it has no artifacts dir), which this
                // path can reach — `convertFileStreaming` accepts every image
                // mode, unlike the warm `Pipeline`'s streaming, which rejects
                // `referenced` up front. Mirrors the buffered branch above,
                // which honours both `compactTables` and `artifactsDir`.
                let mut streamer = MarkdownStreamer::with_artifacts(
                    cfg.strict,
                    image_mode,
                    cfg.compact_tables,
                    &cfg.artifacts_dir,
                )
                .with_page_break_placeholder(cfg.page_break_placeholder.clone());
                for chunk in [streamer.push(&doc.nodes, &doc.links), streamer.finish()] {
                    if !chunk.is_empty() {
                        callback.call(Ok(Some(chunk)), ThreadsafeFunctionCallMode::NonBlocking);
                    }
                }
                // End-of-stream sentinel.
                callback.call(Ok(None), ThreadsafeFunctionCallMode::NonBlocking);
                return;
            }
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
    /// Construct the pipeline. `strict` (cleaner Markdown) and the three
    /// enrichment switches (`doPictureClassification`, `doCodeEnrichment`,
    /// `doFormulaEnrichment`, #423) are read — the enrichment passes are
    /// per-instance state, so pick them here; `fetchImages` /
    /// `allowedFormats` don't apply to the PDF/image pipeline.
    #[napi(constructor)]
    pub fn new(options: Option<ConverterOptions>) -> Result<Self> {
        let options = options.unwrap_or_default();
        // This class exists to keep the ONNX models warm across calls. The VLM
        // pipeline loads no models, so there is nothing to keep warm and no
        // reuse to gain — refuse rather than quietly converting through the
        // very stack the caller asked to replace (#77).
        match options.pipeline.as_deref() {
            None | Some("standard") => {}
            Some("vlm") => {
                return Err(Error::new(
                    Status::InvalidArg,
                    "Pipeline keeps the ONNX models warm; the 'vlm' pipeline loads no models, \
                     so it has nothing to reuse. Use DocumentConverter (or convertFile / \
                     convertFileAsync) with pipeline: 'vlm'.",
                ))
            }
            Some(other) => {
                return Err(Error::new(
                    Status::InvalidArg,
                    format!("unknown pipeline '{other}' (expected: standard, vlm)"),
                ))
            }
        }
        let strict = options.strict.unwrap_or(false);
        let pipeline = RsPipeline::new()
            .map_err(convert_err)?
            .enrichments(enrichments(
                options.do_picture_classification,
                options.do_code_enrichment,
                options.do_formula_enrichment,
            ));
        Ok(Self {
            inner: Arc::new(Mutex::new(pipeline)),
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
    doc.page_break_placeholder = cfg.page_break_placeholder.clone();
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
    let mut streamer = MarkdownStreamer::new(strict, cfg.image_mode, false)
        .with_page_break_placeholder(cfg.page_break_placeholder.clone());
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
        asr_model: None,
        asr_lang: None,
        encoding: None,
        video_frames: None,
        page_range: None,
        list_attachments: false,
        skip_empty_cells: false,
        compact_tables: false,
        ebcdic_layout: None,
        skip_ocr: false,
        force_full_page_ocr: false,
        no_text_panels: false,
        heading_hierarchy: false,
        enrich: docling::EnrichmentOptions::default(),
        // The warm `Pipeline` is the ONNX-models class; `Pipeline::new` rejects
        // `pipeline: "vlm"` outright, so nothing reaches here with one set.
        vlm: None,
        ocr_lang: None,
        ocr_mode: None,
        ocr_scale: None,
        allowed_formats: None,
        to: parse_output_kind(out.to.as_deref())?,
        image_mode: parse_image_mode(out.image_mode.as_deref())?,
        artifacts_dir: out.artifacts_dir.unwrap_or_else(|| "artifacts".to_string()),
        page_break_placeholder: out.page_break_placeholder,
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
    /// `.models/chunk/tokenizer.json` (populated by
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
        // Explicit path, or .models/chunk/tokenizer.json (the download script's
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
        "doc",
        "xls",
        "ppt",
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
        "audio",
        "video",
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
        Some("latex") => Ok(OutputKind::Latex),
        Some(other) => Err(Error::new(
            Status::InvalidArg,
            format!("unknown `to` '{other}' (expected: markdown, json, latex)"),
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
        "doctags" | "dt" => InputFormat::DocTags,
        "mets_gbs" => InputFormat::MetsGbs,
        "email" => InputFormat::Email,
        "latex" => InputFormat::Latex,
        "audio" => InputFormat::Audio,
        "video" => InputFormat::Video,
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

#[cfg(test)]
mod tests {
    use super::*;

    // napi-derive gates its N-API registration glue behind `cfg(not(test))`, so
    // the cdylib's lib target still links as an ordinary test binary and the
    // pure option-resolution helpers can be pinned here. This is the *enforced*
    // gate for them: `cargo test --workspace` runs these, whereas the Node
    // smoke test needs a built addon and no workflow invokes it.

    /// The standard pipeline builds no VLM options — and stays that way with
    /// *every* `vlm_*` argument set, rather than failing the call. The ignore
    /// is CLI parity (a stray `--vlm-endpoint` is parsed and dropped there
    /// too); pinning it here keeps it a decision instead of a regression
    /// someone files later. Its caller-side twin is the "vlm* options without
    /// pipeline: 'vlm' are ignored" check in `test/smoke.mjs`.
    #[test]
    fn standard_pipeline_ignores_vlm_options() {
        for p in [None, Some("standard")] {
            let got =
                resolve_vlm(p, None, None, None, None, None, None).expect("standard pipeline");
            assert!(got.is_none(), "pipeline {p:?} must not build VLM options");

            let got = resolve_vlm(
                p,
                Some("http://127.0.0.1:1/v1".into()),
                Some("granite-docling".into()),
                Some("sekret".into()),
                Some("Describe this page.".into()),
                Some(512),
                Some((2, 4)),
            )
            .expect("vlm options must not fail the standard pipeline");
            assert!(got.is_none(), "pipeline {p:?} must ignore the vlm options");
        }
    }

    #[test]
    fn unknown_pipeline_is_rejected() {
        let err = resolve_vlm(Some("granite"), None, None, None, None, None, None).unwrap_err();
        assert_eq!(err.status, Status::InvalidArg);
        assert!(
            err.reason.contains("unknown pipeline"),
            "reason: {}",
            err.reason
        );
    }

    /// Every explicit option must land on the resolved struct — including the
    /// four `resolve_vlm` applies itself on top of `VlmOptions::resolve`
    /// (api_key, prompt, max_tokens, page_range). Precedence against a *set*
    /// `DOCLING_RS_VLM_*` is not asserted here: `std::env::set_var` is unsound
    /// under the parallel test harness.
    #[test]
    fn explicit_vlm_options_all_reach_the_resolved_struct() {
        let got = resolve_vlm(
            Some("vlm"),
            Some("http://127.0.0.1:1/v1".into()),
            Some("granite-docling".into()),
            Some("sekret".into()),
            Some("Describe this page.".into()),
            Some(512),
            Some((2, 4)),
        )
        .expect("vlm pipeline")
        .expect("vlm pipeline yields options");
        assert_eq!(got.endpoint, "http://127.0.0.1:1/v1");
        assert_eq!(got.model, "granite-docling");
        assert_eq!(got.api_key.as_deref(), Some("sekret"));
        assert_eq!(got.prompt.as_deref(), Some("Describe this page."));
        assert_eq!(got.max_tokens, 512);
        assert_eq!(got.page_range, Some((2, 4)));
    }

    /// A missing endpoint is a bad call, not a failed conversion — and it has
    /// to surface while options are parsed, before any file is read.
    #[test]
    fn vlm_without_an_endpoint_is_an_invalid_arg() {
        if std::env::var_os("DOCLING_RS_VLM_ENDPOINT").is_some() {
            eprintln!("skipping: DOCLING_RS_VLM_ENDPOINT is set in this environment");
            return;
        }
        let err =
            resolve_vlm(Some("vlm"), None, Some("m".into()), None, None, None, None).unwrap_err();
        assert_eq!(err.status, Status::InvalidArg);
        assert!(
            err.reason.contains("DOCLING_RS_VLM_ENDPOINT"),
            "reason: {}",
            err.reason
        );
    }

    /// `vlmEndpoint: process.env.VLM_URL ?? ''` must reach the env fallback,
    /// not resolve to an empty endpoint — the same rule `env::nonempty`
    /// applies to the variables these options fall back to.
    #[test]
    fn blank_vlm_options_count_as_unset() {
        if std::env::var_os("DOCLING_RS_VLM_ENDPOINT").is_some() {
            eprintln!("skipping: DOCLING_RS_VLM_ENDPOINT is set in this environment");
            return;
        }
        let err = resolve_vlm(
            Some("vlm"),
            Some("   ".into()),
            Some("m".into()),
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(
            err.reason.contains("DOCLING_RS_VLM_ENDPOINT"),
            "reason: {}",
            err.reason
        );
    }
}
