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
use serde::Serialize;

use docling::{
    ConversionStatus, DoclingDocument, DocumentConverter as RsConverter, ImageMode, InputFormat,
    MarkdownStreamer, Pipeline as RsPipeline, SourceDocument,
};

// ---------------------------------------------------------------------------
// Options / result shapes exposed to TypeScript.
// ---------------------------------------------------------------------------

/// Config for a reusable [`DocumentConverter`].
#[napi(object)]
#[derive(Clone, Debug, Default, Serialize)]
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
    /// UTF-8, a statistical guess for a non-Latin encoding — #675 — else
    /// windows-1252); bytes it cannot decode fail the conversion.
    pub encoding: Option<String>,
    /// Jupyter notebooks (#673): which cell outputs the document keeps —
    /// `"none"` (default, the notebook as written) or `"text"` (stream and
    /// `text/plain` results as code blocks after their cell, image outputs
    /// as pictures).
    pub ipynb_outputs: Option<String>,
    /// Document metadata (#679): `"none"` (default, nowhere), `"json"` (a
    /// `metadata` object in the docling JSON) or `"frontmatter"` (YAML
    /// front matter at the top of the Markdown as well).
    pub metadata: Option<String>,
    /// Tracked changes in DOCX / DOC / ODT (#851, Pandoc's
    /// `--track-changes`): `"accept"` (insertions shown, deletions left
    /// out), `"reject"` (the reverse) or `"all"` (both, typed with author
    /// and date in the Pandoc AST). Unset: each reader's own view. DOCX
    /// reads all three, DOC accept and reject, ODT accept; a mode a reader
    /// lacks is an error.
    pub track_changes: Option<String>,
    /// DOCX DrawingML drawings (#858): `"mermaid"` (the default) adds a
    /// `mermaid` code item after a drawing's picture with the flowchart its
    /// labelled shapes and connectors describe — only when every connector
    /// names its two end shapes; a drawing laid by eye gets no graph.
    /// `"none"` turns the extraction off.
    pub extract_diagrams: Option<String>,
    /// Max frames sampled from a video input as timestamped pictures (needs
    /// the ffmpeg binary at runtime; `0` = transcript only). Default 8.
    /// `4294967295` (`2**32 - 1`, the largest value this field takes) keeps
    /// every distinct cut (#647).
    pub video_frames: Option<u32>,
    /// ffmpeg's scene score (0–1) a video frame must exceed to count as a
    /// cut (#647). Default 0.27; 0.6 keeps hard cuts only.
    pub video_scene_threshold: Option<f64>,
    /// Downscale each sampled video frame inside ffmpeg so its longer side
    /// is at most this many px (#647); `0` / unset = the source resolution.
    pub video_frame_max_side: Option<u32>,
    /// Drop a sampled video frame whose 64-bit difference hash is within
    /// this Hamming distance (0–64) of a frame already kept (#647): the same
    /// slide after a fade becomes one picture. Unset = keep every frame;
    /// 4–6 is a good distance.
    pub video_frame_dedupe: Option<u32>,
    /// XBRL: the directory the instance's taxonomy is read from (docling's
    /// `XBRLBackendOptions.taxonomy`); unset = the instance's own directory.
    pub xbrl_taxonomy: Option<String>,
    /// Convert only this PDF page window: `"A-B"` or a single page `"N"`
    /// (1-based inclusive — issue #80). Other formats ignore it.
    pub pages: Option<String>,
    /// Per-document budget in seconds for the PDF pipeline (docling's
    /// `document_timeout`, #497); unset = unlimited. Checked between pages:
    /// once spent, the pages done so far are the document, `status` is
    /// `"partial_success"` and `errors` says why.
    pub document_timeout: Option<f64>,
    /// Print progress to stderr (#783, docling's `show_progress`): a line
    /// per document, a bar for its pages and each enrichment pass, drawn in
    /// place on a terminal. Default `false`. For your own UI pass a
    /// `progressCallback` to the async conversions instead.
    #[serde(skip_serializing)]
    pub show_progress: Option<bool>,
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
    /// RapidOCR's text-line orientation handling (#666): oriented line
    /// crops, tall lines turned 90°, upside-down lines turned by the 0/180
    /// classifier (`.models/ocr_textline_ori.onnx`). Unset = off unless
    /// `DOCLING_RS_OCR_TEXTLINE_ORIENTATION` is set.
    pub ocr_textline_orientation: Option<bool>,
    /// Picture crops (and page images) in px per PDF point — docling's
    /// `images_scale` (#520), 0.1–4.0; unset keeps the pipeline's 2.0 px/pt
    /// render. The JSON picture `dpi` is 72·scale (#519).
    pub images_scale: Option<f64>,
    /// Keep each page's render as the JSON `pages[n].image` — docling's
    /// `generate_page_images` (#520). Default `false`.
    pub page_images: Option<bool>,
    /// Which OCR engine reads scanned pages (#460): `"ppocr"` (default, the
    /// built-in PP-OCRv3 recognizer) | `"tesseract"` (the system `tesseract`
    /// binary). Under Tesseract `ocrLang` is its language list — tessdata
    /// stems (`"deu+fra"`) or BCP-47 tags.
    pub ocr_engine: Option<String>,
    /// Email (.eml/.msg): append an Attachments section — names and content
    /// types only, never the payload (#251). Default `false`.
    pub list_attachments: Option<bool>,
    /// Omit empty cells from sparse XLSX/XLS table grids (#271; docling.rs
    /// extension). Default `false`.
    pub skip_empty_cells: Option<bool>,
    /// Unpadded `| a | b |` Markdown tables, all formats (#271; docling.rs
    /// extension). Default `false`.
    pub compact_tables: Option<bool>,
    /// Render pictures' alt text (DOCX/PPTX/XLSX, #676) in Markdown and
    /// HTML; `strict` implies it, JSON has it always. Default `false`.
    pub picture_alt_text: Option<bool>,
    /// EBCDIC (#252): copybook layout as inline `EbcdicLayout` JSON or a
    /// file path; defaults to the `<stem>.layout.json` sidecar.
    pub ebcdic_layout: Option<String>,
    /// Keep layout + TableFormer, never OCR — docling's `do_ocr=False`, its
    /// CLI's `--no-ocr` (#244; since 2.0 the meaning of `noOcr`, #611).
    /// Structured output survives; text that exists only as pixels (scanned
    /// pages, text inside images) comes back empty. Default `false`.
    pub no_ocr: Option<bool>,
    /// `noOcr` under its pre-2.0 name, still read.
    pub skip_ocr: Option<bool>,
    /// Skip the whole PDF ML stack and read the embedded text layer only —
    /// the CLI's `--text-layer-only` (what `noOcr` meant before 2.0, #611).
    /// Default `false`. `"auto"` decides per document: the text layer alone
    /// when it carries the document (no model touched), the full pipeline —
    /// OCR included — when it does not.
    #[serde(serialize_with = "ser_text_layer_only")]
    pub text_layer_only: Option<Either<bool, String>>,
    /// The password of an encrypted PDF or Office document — .docx/.xlsx/
    /// .pptx/.doc/.xls/.ppt (#611, #625).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// `password` under its pre-#625 name (docling's PDF-only
    /// `--pdf-password`), still read; set one of the two.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pdf_password: Option<String>,
    /// Skip TableFormer — tables come from the layout model's geometry
    /// instead (the CLI's `--no-table-former`, #577). Default `false`.
    pub no_table_former: Option<bool>,
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
    /// OCR the pictures embedded in non-PDF documents — DOCX/PPTX
    /// screenshots, HTML figures, sampled video frames — with the pipeline's
    /// OCR models and attach the text to the picture as docling's description
    /// annotation (#645): Markdown prints it between the caption and the
    /// image placeholder, the JSON picture item carries `meta.description` +
    /// the `description` annotation, chunks include it. Needs the OCR models
    /// (`.models/ocr_rec*`, `ocr_det.onnx`; or Tesseract under `ocrEngine`);
    /// with `skipOcr` / `textLayerOnly` it warns and reads nothing. PDF/image
    /// pages are OCR'd by the pipeline itself. Default `false`.
    pub do_picture_ocr: Option<bool>,
    /// With `doPictureOcr`: only read the pictures the DocumentFigureClassifier
    /// labels as one of these comma-separated classes
    /// (`"screenshot_from_computer,screenshot_from_manual"`); unset = every
    /// picture. Needs `.models/picture_classifier.onnx`.
    pub picture_ocr_classes: Option<String>,
    /// With `doPictureOcr`: skip pictures whose smaller side is under this
    /// many pixels (icons, bullets). Default 32
    /// (`DOCLING_RS_PICTURE_OCR_MIN_SIDE`).
    pub picture_ocr_min_side: Option<u32>,
    /// The picture OCR's resolution in px per picture pixel under
    /// RapidOCR's 2000 px cap (#666): `1.0` reads pictures at their native
    /// pixels; unset keeps the image-input rule.
    pub picture_ocr_scale: Option<f64>,
    /// `false` drops the embedded image bytes from every picture after the
    /// enrichment pass (#645): slim JSON/DCLX, placeholder-only Markdown, the
    /// OCR text kept. Default `true`.
    pub keep_picture_images: Option<bool>,
    /// Describe every picture — the embedded pictures of DOCX/PPTX/XLSX/
    /// HTML, a PDF's figures, video frames, an image input as a whole —
    /// through an OpenAI-compatible vision endpoint (#670); the answer is the
    /// picture's description annotation, where `doPictureOcr` puts its text.
    /// `pictureOcrMinSide` / `pictureOcrClasses` select the pictures here
    /// too; identical images are sent once; an endpoint error warns and the
    /// conversion goes on. Default `false`.
    pub do_picture_description: Option<bool>,
    /// The vision endpoint (`/v1` base or the full `chat/completions` URL);
    /// falls back to `vlmEndpoint`, then `$DOCLING_RS_VLM_ENDPOINT`.
    pub picture_description_endpoint: Option<String>,
    /// Model name; falls back to `vlmModel`, then `$DOCLING_RS_VLM_MODEL`.
    pub picture_description_model: Option<String>,
    /// Bearer token; falls back to `vlmApiKey`, then `$DOCLING_RS_VLM_API_KEY`.
    pub picture_description_api_key: Option<String>,
    /// The instruction sent with every picture; unset = the mode's prompt.
    pub picture_description_prompt: Option<String>,
    /// `"caption"` (default; MarkItDown's caption prompt) | `"ocr"`
    /// (markitdown-ocr's text-extraction prompt).
    pub picture_description_mode: Option<String>,
    /// Pictures in flight at once (default 1).
    pub picture_description_concurrency: Option<u32>,
    /// Redact personal data from the converted document before any export
    /// (#621): e-mail, phone, card numbers (Luhn), IBANs (mod-97), IP
    /// addresses, URL credentials, national IDs, and — with the NER model
    /// under `.models/ner/` — names, organizations and locations; every
    /// string of the document model is rewritten, so Markdown, JSON, DCLX
    /// and chunks all come out clean. The result's `redaction` carries the
    /// counts per label. A docling.rs extension; default `false`.
    pub redact_pii: Option<bool>,
    /// `"label"` (default: `[EMAIL]`) | `"pseudonym"` (`[EMAIL_1]`, one
    /// number per distinct value) | `"fixed:<text>"`.
    pub redact_mode: Option<String>,
    /// Comma-separated kinds to redact (`"email,phone,credit_card,iban,
    /// ip_address,url_credentials,national_id,person,organization,location,
    /// address"`); unset = every kind.
    pub redact_kinds: Option<String>,
    /// Extra patterns as `NAME=REGEX` lines (newline-separated); the match
    /// (or capture group 1) is redacted as `[NAME]`.
    pub redact_pattern: Option<String>,
    /// Embedded images and page renders: `"drop"` (default) | `"box_out"`
    /// (OCR and paint over the lines carrying a value) | `"keep"`.
    pub redact_images: Option<String>,
    /// With `redactPii` (#694): comma-separated terms never redacted even
    /// when a detector flags them.
    pub redact_allow_terms: Option<String>,
    /// Comma-separated terms always redacted, as `[REDACTED]`.
    pub redact_deny_terms: Option<String>,
    /// Regexes, one per line, a flagged span must not match in full to be
    /// redacted — the document's own part numbers and record IDs.
    pub redact_allow_patterns: Option<String>,
    /// The NER detector's minimum confidence, 0–1 (default 0.85).
    pub redact_ner_min_score: Option<f64>,
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
    /// The alias of `imageSources: "remote"` (#646).
    pub fetch_images: Option<bool>,
    /// Which image references resolve (#646): `"none"` (default — every
    /// picture a placeholder), `"embedded"` (`data:` URIs and parts of the
    /// same container: EPUB/MHTML entries, an email's `cid:` attachments —
    /// no filesystem, no network), `"local"` (plus files under the source
    /// file's directory, never an absolute path or one that escapes it),
    /// `"remote"` (plus http(s) fetches). HTML, EPUB, MHTML, JATS, AsciiDoc,
    /// ODF, Markdown and email bodies.
    pub image_sources: Option<String>,
    /// Hosts a `"remote"` image fetch may reach (#646): exact names or
    /// `*.suffix` wildcards; redirects are held to the same list. Unset =
    /// any host.
    pub image_hosts: Option<Vec<String>>,
    /// Largest image that resolves, in bytes (#646; default 32 MiB).
    pub max_image_bytes: Option<f64>,
    /// Images resolved per document; the rest stay placeholders (#646;
    /// default unlimited).
    pub max_images: Option<u32>,
    /// Total resolved image bytes per document, in MiB (#646; default
    /// unlimited).
    pub max_image_total_mb: Option<u32>,
    /// Smallest image that resolves, in bytes — skips spacer / tracking
    /// pixels (#646; default 0).
    pub min_image_bytes: Option<u32>,
    /// Restrict the converter to these formats (ids like `"md"`, `"pdf"`, or
    /// extensions like `".html"`); anything else is rejected. Default: accept all.
    pub allowed_formats: Option<Vec<String>>,
}

/// Per-call output options (how to render the converted document).
#[napi(object)]
#[derive(Clone, Default)]
pub struct OutputOptions {
    /// `"markdown"` (default), `"json"` (docling-core DoclingDocument wire
    /// format), `"text"` (plain text, docling's `export_to_text`, #613),
    /// `"latex"` (a complete LaTeX document, #317), `"html"` (a
    /// complete HTML document, docling-core's `HTMLDocSerializer`, #492 —
    /// pictures follow `imageMode` like Markdown) or `"pandoc"` (Pandoc's
    /// JSON AST for `pandoc -f json`, #515 — pictures follow `imageMode`) or
    /// `"vtt"` (WebVTT subtitles, docling's `--to vtt`, #614: a cue per timed
    /// text item — an audio/video transcript's segments, a `.vtt` input's
    /// cues).
    pub to: Option<String>,
    /// Picture handling for Markdown: `"placeholder"` (default; `"embedded"`
    /// for `to: "pandoc"`, #537), `"embedded"` (base64 data URIs inline), or
    /// `"referenced"` (returns image files in `images`). Ignored for JSON,
    /// which always embeds images as data URIs.
    pub image_mode: Option<String>,
    /// Directory name used in `referenced` image links. Default `"artifacts"`.
    pub artifacts_dir: Option<String>,
    /// `to: "pandoc"` (#848): the `Header` depth a section heading gets —
    /// `"docling"` (default: the title is `Header 1`, a level-n section
    /// heading `Header n+1`, docling's Markdown / HTML depth) or `"source"`
    /// (`Header n`: a DOCX `Heading 1` is `Header 1`, as `pandoc -f docx`
    /// reads it).
    pub pandoc_heading_depth: Option<String>,
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
#[derive(Serialize, Clone, Default)]
pub struct ConvertOptions {
    pub strict: Option<bool>,
    pub fetch_images: Option<bool>,
    /// Image-source tier (#646): `"none"` | `"embedded"` | `"local"` | `"remote"`.
    pub image_sources: Option<String>,
    /// Remote image host allow-list (#646).
    pub image_hosts: Option<Vec<String>>,
    /// Largest image that resolves, in bytes (#646).
    pub max_image_bytes: Option<f64>,
    /// Images resolved per document (#646).
    pub max_images: Option<u32>,
    /// Total resolved image bytes per document, in MiB (#646).
    pub max_image_total_mb: Option<u32>,
    /// Smallest image that resolves, in bytes (#646).
    pub min_image_bytes: Option<u32>,
    /// Named Whisper model preset for audio sources.
    pub asr_model: Option<String>,
    /// ASR transcription language for audio/video: a Whisper code (`"en"`,
    /// `"de"`, …) or `"auto"` (default) — detected from the first 30 seconds.
    pub asr_lang: Option<String>,
    /// Character encoding of text inputs (docling's
    /// `TextBackendOptions.encoding`); unset = detect.
    pub encoding: Option<String>,
    /// Jupyter notebooks (#673): which cell outputs the document keeps —
    /// `"none"` (default, the notebook as written) or `"text"` (stream and
    /// `text/plain` results as code blocks after their cell, image outputs
    /// as pictures).
    pub ipynb_outputs: Option<String>,
    /// Document metadata (#679): `"none"` (default), `"json"` or
    /// `"frontmatter"`.
    pub metadata: Option<String>,
    /// Tracked changes (#851): `"accept"` | `"reject"` | `"all"`; see
    /// [`ConverterOptions::track_changes`].
    pub track_changes: Option<String>,
    /// Diagram extraction (#858): `"mermaid"` (default) | `"none"`; see
    /// [`ConverterOptions::extract_diagrams`].
    pub extract_diagrams: Option<String>,
    /// Max frames sampled from a video input (`0` = transcript only;
    /// `4294967295` = every distinct cut, #647).
    pub video_frames: Option<u32>,
    /// Scene score (0–1) a video frame must exceed to be a cut (#647).
    pub video_scene_threshold: Option<f64>,
    /// Cap on a sampled video frame's longer side in px (#647).
    pub video_frame_max_side: Option<u32>,
    /// Difference-hash distance (0–64) under which a sampled video frame is
    /// a duplicate and dropped (#647).
    pub video_frame_dedupe: Option<u32>,
    /// XBRL taxonomy directory (docling's `XBRLBackendOptions.taxonomy`).
    pub xbrl_taxonomy: Option<String>,
    /// PDF page window `"A-B"` (or `"N"`), 1-based inclusive (#80).
    pub pages: Option<String>,
    /// Per-document budget in seconds (docling's `document_timeout`, #497);
    /// see `ConverterOptions.documentTimeout`.
    pub document_timeout: Option<f64>,
    /// Print progress to stderr (#783, docling's `show_progress`): a line
    /// per document, a bar for its pages and each enrichment pass, drawn in
    /// place on a terminal. Default `false`. For your own UI pass a
    /// `progressCallback` to the async conversions instead.
    #[serde(skip_serializing)]
    pub show_progress: Option<bool>,
    /// OCR recognition language for scanned pages: `"en"` (default) | `"ch"`,
    /// or a BCP-47 tag for either (`"en-US"`, `"zh-Hans"`, #388).
    pub ocr_lang: Option<String>,
    /// Which regions feed the OCR (docling's `OcrMode`, #254): `"default"` |
    /// `"full_page"` | `"layout_regions"` | `"pdf_aware_layout_regions"`.
    pub ocr_mode: Option<String>,
    /// OCR render scale in px per PDF point (docling's `OcrOptions.scale`,
    /// #254); unset reads the pipeline's own 2.0 px/pt render.
    pub ocr_scale: Option<f64>,
    /// RapidOCR's text-line orientation handling (#666). See
    /// [`ConverterOptions`].
    pub ocr_textline_orientation: Option<bool>,
    /// Picture crops (and page images) in px per PDF point — docling's
    /// `images_scale` (#520), 0.1–4.0; unset keeps the pipeline's 2.0 px/pt
    /// render. The JSON picture `dpi` is 72·scale (#519).
    pub images_scale: Option<f64>,
    /// Keep each page's render as the JSON `pages[n].image` — docling's
    /// `generate_page_images` (#520). Default `false`.
    pub page_images: Option<bool>,
    /// Which OCR engine reads scanned pages (#460): `"ppocr"` (default) |
    /// `"tesseract"`.
    pub ocr_engine: Option<String>,
    /// Email (.eml/.msg): append an Attachments section — names and content
    /// types only, never the payload (#251). Default `false`.
    pub list_attachments: Option<bool>,
    /// Omit empty cells from sparse XLSX/XLS table grids (#271). Default
    /// `false`.
    pub skip_empty_cells: Option<bool>,
    /// Unpadded Markdown tables (#271). Default `false`.
    pub compact_tables: Option<bool>,
    /// Pictures' alt text (#676) in Markdown and HTML. Default `false`.
    pub picture_alt_text: Option<bool>,
    /// EBCDIC (#252): copybook layout as inline `EbcdicLayout` JSON or a
    /// file path; defaults to the `<stem>.layout.json` sidecar.
    pub ebcdic_layout: Option<String>,
    /// Keep layout + TableFormer, never OCR — docling's `do_ocr=False`, its
    /// CLI's `--no-ocr` (#244; since 2.0 the meaning of `noOcr`, #611).
    /// Structured output survives; text that exists only as pixels (scanned
    /// pages, text inside images) comes back empty. Default `false`.
    pub no_ocr: Option<bool>,
    /// `noOcr` under its pre-2.0 name, still read.
    pub skip_ocr: Option<bool>,
    /// Skip the whole PDF ML stack and read the embedded text layer only —
    /// the CLI's `--text-layer-only` (what `noOcr` meant before 2.0, #611).
    /// Default `false`. `"auto"` decides per document: the text layer alone
    /// when it carries the document (no model touched), the full pipeline —
    /// OCR included — when it does not.
    #[serde(serialize_with = "ser_text_layer_only")]
    pub text_layer_only: Option<Either<bool, String>>,
    /// The password of an encrypted PDF or Office document — .docx/.xlsx/
    /// .pptx/.doc/.xls/.ppt (#611, #625).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// `password` under its pre-#625 name (docling's PDF-only
    /// `--pdf-password`), still read; set one of the two.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pdf_password: Option<String>,
    /// Skip TableFormer — tables come from the layout model's geometry
    /// instead (the CLI's `--no-table-former`, #577). Default `false`.
    pub no_table_former: Option<bool>,
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
    /// Picture OCR for non-PDF documents (#645) and its filters. See
    /// [`ConverterOptions`]. Defaults `false` / unset / 32 / `true`.
    pub do_picture_ocr: Option<bool>,
    pub picture_ocr_classes: Option<String>,
    pub picture_ocr_min_side: Option<u32>,
    pub picture_ocr_scale: Option<f64>,
    pub keep_picture_images: Option<bool>,
    /// Describe every picture — the embedded pictures of DOCX/PPTX/XLSX/
    /// HTML, a PDF's figures, video frames, an image input as a whole —
    /// through an OpenAI-compatible vision endpoint (#670); the answer is the
    /// picture's description annotation, where `doPictureOcr` puts its text.
    /// `pictureOcrMinSide` / `pictureOcrClasses` select the pictures here
    /// too; identical images are sent once; an endpoint error warns and the
    /// conversion goes on. Default `false`.
    pub do_picture_description: Option<bool>,
    /// The vision endpoint (`/v1` base or the full `chat/completions` URL);
    /// falls back to `vlmEndpoint`, then `$DOCLING_RS_VLM_ENDPOINT`.
    pub picture_description_endpoint: Option<String>,
    /// Model name; falls back to `vlmModel`, then `$DOCLING_RS_VLM_MODEL`.
    pub picture_description_model: Option<String>,
    /// Bearer token; falls back to `vlmApiKey`, then `$DOCLING_RS_VLM_API_KEY`.
    pub picture_description_api_key: Option<String>,
    /// The instruction sent with every picture; unset = the mode's prompt.
    pub picture_description_prompt: Option<String>,
    /// `"caption"` (default; MarkItDown's caption prompt) | `"ocr"`
    /// (markitdown-ocr's text-extraction prompt).
    pub picture_description_mode: Option<String>,
    /// Pictures in flight at once (default 1).
    pub picture_description_concurrency: Option<u32>,
    /// PII redaction (#621) and its settings. See [`ConverterOptions`].
    pub redact_pii: Option<bool>,
    pub redact_mode: Option<String>,
    pub redact_kinds: Option<String>,
    pub redact_pattern: Option<String>,
    pub redact_images: Option<String>,
    /// With `redactPii` (#694): comma-separated terms never redacted even
    /// when a detector flags them.
    pub redact_allow_terms: Option<String>,
    /// Comma-separated terms always redacted, as `[REDACTED]`.
    pub redact_deny_terms: Option<String>,
    /// Regexes, one per line, a flagged span must not match in full to be
    /// redacted — the document's own part numbers and record IDs.
    pub redact_allow_patterns: Option<String>,
    /// The NER detector's minimum confidence, 0–1 (default 0.85).
    pub redact_ner_min_score: Option<f64>,
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
    /// See [`OutputOptions::pandoc_heading_depth`] (#848).
    pub pandoc_heading_depth: Option<String>,
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
    /// The problems the conversion survived — docling's
    /// `ConversionResult.errors`; non-empty exactly when `status` is
    /// `"partial_success"` (a spent `documentTimeout`, #497; a VLM page
    /// that failed or came back cut off, #762/#763).
    pub errors: Vec<ConversionErrorItem>,
    /// What the PII pass redacted (#621) when `redactPii` was on: counts
    /// per label (`{ EMAIL: 2, PHONE: 1 }`); absent otherwise.
    pub redaction: Option<std::collections::HashMap<String, u32>>,
}

/// docling's `ErrorItem`: one recorded problem of a conversion that still
/// produced a document.
#[napi(object)]
pub struct ConversionErrorItem {
    /// `"document_backend"`, `"model"`, `"doc_assembler"`, `"user_input"`,
    /// `"pipeline"`.
    pub component_type: String,
    /// The stage that recorded it (`"pipeline"` for the document budget,
    /// `"VlmPipeline"` for a VLM page).
    pub module_name: String,
    pub error_message: String,
    /// The 1-based page the problem belongs to; absent when it is
    /// document-scoped.
    pub page_no: Option<u32>,
}

fn error_items(errors: Vec<docling::ErrorItem>) -> Vec<ConversionErrorItem> {
    errors
        .into_iter()
        .map(|e| ConversionErrorItem {
            component_type: e.component_type,
            module_name: e.module_name,
            error_message: e.error_message,
            page_no: e.page_no.map(|p| p as u32),
        })
        .collect()
}

/// One entry of a ZIP archive converted by [`convert_archive`] /
/// [`convert_archive_file`] (#557): its path inside the archive and what
/// became of it.
#[napi(object)]
pub struct ArchiveItem {
    /// The entry's path inside the archive (`/`-separated).
    pub path: String,
    /// `"converted"` (`result` is set), `"skipped"` (`error` says why:
    /// unsupported type, nested archive, unsafe path, over a
    /// `DOCLING_RS_ZIP_MAX_*` limit) or `"failed"` (`error` is the
    /// conversion error; the other entries are unaffected).
    pub outcome: String,
    pub result: Option<ConvertResult>,
    pub error: Option<String>,
}

/// Send-safe [`ArchiveItem`], produced off the JS thread. Public only as a
/// [`Task`] output; not exposed to JS.
#[doc(hidden)]
pub struct RawArchiveItem {
    path: String,
    outcome: &'static str,
    result: Option<RawResult>,
    error: Option<String>,
}

impl RawArchiveItem {
    fn into_js(self) -> ArchiveItem {
        ArchiveItem {
            path: self.path,
            outcome: self.outcome.to_string(),
            result: self.result.map(RawResult::into_js),
            error: self.error,
        }
    }
}

// ---------------------------------------------------------------------------
// Internal, Send-safe conversion plumbing (shared by sync, async, streaming).
// ---------------------------------------------------------------------------

/// Fully-resolved conversion config, free of any napi/JS types so it can move
/// onto a worker thread for the async and streaming paths.
struct ConvertConfig {
    /// The shared conversion options (#577), validated on the way in — the
    /// `DocumentConverter` is built from them by the library's own mapping.
    opts: docling::ConvertOptions,
    /// `Some` only for `pipeline: "vlm"` (#77), already resolved against the
    /// `DOCLING_RS_VLM_*` environment. Its presence *is* the pipeline switch:
    /// [`run_convert`] short-circuits the whole ML stack when it is set.
    vlm: Option<docling::vlm::VlmOptions>,
    allowed_formats: Option<Vec<InputFormat>>,
    to: OutputKind,
    image_mode: ImageMode,
    artifacts_dir: String,
    /// `--pandoc-heading-depth` (#848).
    pandoc_heading_depth: docling::pandoc::HeadingDepth,
    /// docling's `page_break_placeholder` for the Markdown export (an
    /// *output* option on this surface — `OutputOptions` — so it overrides
    /// whatever the converter options carry).
    page_break_placeholder: Option<String>,
    /// `showProgress` (#783): the engine's stderr printer.
    show_progress: bool,
    /// The call's `progressCallback` (#783), bridged from its threadsafe
    /// function: every event of the conversion, as JSON.
    progress: Option<docling::ProgressCallback>,
}

#[derive(Clone, Copy, PartialEq)]
enum OutputKind {
    Markdown,
    Json,
    /// A complete LaTeX document (docling 2.124's `--to latex`, #317).
    Latex,
    /// Plain text (docling's `--to text`, `export_to_text`, #613).
    Text,
    /// WebVTT subtitles (docling's `--to vtt`, #614).
    Vtt,
    /// A complete HTML document (docling-core's `HTMLDocSerializer`, #492);
    /// pictures follow `imageMode` like the Markdown export.
    Html,
    /// Pandoc's JSON AST (#515), for `pandoc -f json`; pictures follow
    /// `imageMode` like the Markdown export.
    Pandoc,
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
    errors: Vec<docling::ErrorItem>,
    redaction: Option<docling::RedactionReport>,
}

impl RawResult {
    fn into_js(self) -> ConvertResult {
        ConvertResult {
            content: self.content,
            format: self.format,
            status: self.status,
            input_name: self.input_name,
            errors: error_items(self.errors),
            redaction: self
                .redaction
                .map(|r| r.counts.into_iter().map(|(k, v)| (k, v as u32)).collect()),
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

/// The shared option set (#577) read off a napi options object: serde
/// serializes the Rust fields — snake_case, the wire names — and the keys
/// `docling::ConvertOptions` lacks (`to`, `imageMode`, `allowedFormats`, …)
/// are this surface's own and left behind. One mapping for the one-shot
/// options, the `DocumentConverter` class and the warm `Pipeline`.
/// `textLayerOnly` on the wire: the bool, or the string (`"auto"`), as the
/// engine's option reads it.
fn ser_text_layer_only<S: serde::Serializer>(
    v: &Option<Either<bool, String>>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    match v {
        None => s.serialize_none(),
        Some(Either::A(flag)) => s.serialize_bool(*flag),
        Some(Either::B(text)) => s.serialize_str(text),
    }
}

fn shared_options<T: Serialize>(o: &T) -> Result<docling::ConvertOptions> {
    serde_json::to_value(o)
        .and_then(serde_json::from_value)
        .map_err(|e| Error::new(Status::InvalidArg, e.to_string()))
}

/// A rejected option as this surface's error: `InvalidArg` (a bad call, not
/// a conversion that went wrong), the option named as TypeScript spells it
/// (`ocrLang`, `documentTimeout`, `vlmEndpoint`).
fn option_err(e: docling::OptionsError) -> Error {
    Error::new(
        Status::InvalidArg,
        e.message.replacen(e.field, &camel_case(e.field), 1),
    )
}

/// `ocr_lang` → `ocrLang`: napi's rename of the option fields.
fn camel_case(snake: &str) -> String {
    let mut out = String::with_capacity(snake.len());
    let mut upper = false;
    for c in snake.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn build_config(o: ConvertOptions) -> Result<ConvertConfig> {
    let allowed = match &o.allowed_formats {
        Some(list) => Some(
            list.iter()
                .map(|s| parse_format(s))
                .collect::<Result<Vec<_>>>()?,
        ),
        None => None,
    };
    let opts = shared_options(&o)?;
    // Validated at option-parsing time, not at conversion time, so a bad
    // option fails fast on the call instead of after a file has been read —
    // and `pipeline: "vlm"` resolves its endpoint/model now for the same
    // reason (the standard pipeline ignores stray `vlm*` options, the CLI's
    // rule too; pinned by `standard_pipeline_ignores_vlm_options`).
    opts.validate().map_err(option_err)?;
    let vlm = opts.vlm_options().map_err(option_err)?;
    let to = parse_output_kind(o.to.as_deref())?;
    Ok(ConvertConfig {
        opts,
        vlm,
        allowed_formats: allowed,
        to,
        image_mode: parse_image_mode(o.image_mode.as_deref(), to)?,
        artifacts_dir: o.artifacts_dir.unwrap_or_else(|| "artifacts".to_string()),
        pandoc_heading_depth: parse_heading_depth(o.pandoc_heading_depth.as_deref())?,
        page_break_placeholder: o.page_break_placeholder,
        show_progress: o.show_progress.unwrap_or(false),
        progress: None,
    })
}

fn build_converter(cfg: &ConvertConfig) -> Result<RsConverter> {
    let base = match &cfg.allowed_formats {
        Some(list) => RsConverter::with_allowed_formats(list.iter().copied()),
        None => RsConverter::new(),
    };
    let mut o = cfg.opts.clone();
    if cfg.page_break_placeholder.is_some() {
        o.page_break_placeholder = cfg.page_break_placeholder.clone();
    }
    // The library's mapping onto the builder (#577); `opts` was validated
    // when the config was built, so this cannot fail.
    let converter = o
        .apply(base)
        .map_err(option_err)?
        .show_progress(cfg.show_progress);
    Ok(match &cfg.progress {
        Some(callback) => converter.progress_callback(Arc::clone(callback)),
        None => converter,
    })
}

/// The first page of a `pages` window (`"A-B"` or `"N"`), for the failed
/// page numbers a spent budget reports (#783).
fn first_page(pages: &str) -> Option<usize> {
    docling::parse_page_range(pages)
        .ok()
        .map(|(first, _)| first)
}

/// A JS progress callback (#783), bridged: each event goes to the JS thread
/// as its JSON text (`(err, json)` — the wrapper parses it and calls the
/// user's `(event) => void`), without blocking the converting thread. The
/// events of a sync conversion would only run after it returned, which is
/// why the callback is taken by the async conversions alone.
fn progress_bridge(
    callback: ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>,
) -> docling::ProgressCallback {
    Arc::new(move |event: &docling::ProgressEvent| {
        callback.call(
            Ok(event.to_json().to_string()),
            ThreadsafeFunctionCallMode::NonBlocking,
        );
    })
}

/// Render an already-converted document to Markdown/JSON per the config. The
/// document's `strict_markdown` is assumed already set by whoever produced it.
fn render_doc(
    doc: DoclingDocument,
    cfg: &ConvertConfig,
    input_name: String,
    format: String,
    status: String,
    errors: Vec<docling::ErrorItem>,
    redaction: Option<docling::RedactionReport>,
) -> RawResult {
    let (content, images) = match cfg.to {
        OutputKind::Json => (doc.export_to_json(), Vec::new()),
        OutputKind::Latex => (doc.export_to_latex(), Vec::new()),
        OutputKind::Text => (doc.export_to_text(), Vec::new()),
        OutputKind::Vtt => (doc.export_to_vtt(), Vec::new()),
        OutputKind::Html => match cfg.image_mode {
            ImageMode::Placeholder => (doc.export_to_html(), Vec::new()),
            mode => doc.export_to_html_with_images(mode, &cfg.artifacts_dir),
        },
        OutputKind::Pandoc => doc
            .export_to_pandoc_json_with(&docling::pandoc::PandocExportOptions {
                image_mode: cfg.image_mode,
                artifacts_dir: cfg.artifacts_dir.clone(),
                heading_depth: cfg.pandoc_heading_depth,
                track_changes: cfg.opts.track_changes().ok().flatten(),
                ..Default::default()
            })
            // Only an explicit `api_version` can fail, and none is set.
            .expect("Pandoc export at the default API version"),
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
        errors,
        redaction,
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
    let result = run_convert_inner(source, cfg);
    // The heap the conversion freed goes back to the OS rather than staying
    // in glibc's arenas between calls (#871; `docling::memory`, as serve does).
    docling::memory::release_free_memory();
    result
}

fn run_convert_inner(source: SourceDocument, cfg: &ConvertConfig) -> Result<RawResult> {
    // #77: the remote VLM replaces the entire ML stack — no ONNX model is
    // loaded, and every other converter knob (OCR, TableFormer, text panels)
    // has nothing to act on. A page that failed or came back cut off
    // (#762, #763) makes it a `partial_success` with that page's error.
    if let Some(vlm) = &cfg.vlm {
        check_allowed(&source, cfg)?;
        let format = source.format.as_str().to_string();
        let result = docling::vlm::convert_vlm_result(&source, vlm).map_err(convert_err)?;
        let status = status_str(result.status);
        let mut document = result.document;
        // The serializer knobs `DocumentConverter::convert` would have applied
        // (converter.rs) — this path never reaches it, so they are set here or
        // they silently lapse under `pipeline: "vlm"`.
        document.strict_markdown = cfg.opts.strict.unwrap_or(false);
        document.compact_tables = cfg.opts.compact_tables.unwrap_or(false);
        document.picture_alt_text = cfg.opts.picture_alt_text.unwrap_or(false);
        document.page_break_placeholder = cfg.page_break_placeholder.clone();
        // The PII pass (#621) lapses on this path too unless applied here.
        let redaction = build_converter(cfg)?
            .redact(&mut document)
            .map_err(convert_err)?;
        return Ok(render_doc(
            document,
            cfg,
            source.name,
            format,
            status,
            result.errors,
            redaction,
        ));
    }
    let converter = build_converter(cfg)?;
    let result = converter.convert(source).map_err(convert_err)?;
    let format = result.format.as_str().to_string();
    let status = status_str(result.status);
    Ok(render_doc(
        result.document,
        cfg,
        result.input_name,
        format,
        status,
        result.errors,
        result.redaction,
    ))
}

/// Convert every document inside a ZIP archive (#557), each through
/// [`run_convert`] with the same config (so `allowedFormats`, the VLM
/// pipeline and the output options apply per entry), in archive order. The
/// entries are vetted from the central directory before anything is
/// inflated (`docling::archive`, `DOCLING_RS_ZIP_MAX_*`); an entry that does
/// not convert is its own `skipped` / `failed` item. Errors only when the
/// bytes are not a readable ZIP archive.
fn run_convert_archive(bytes: Vec<u8>, cfg: &ConvertConfig) -> Result<Vec<RawArchiveItem>> {
    let mut archive = docling::archive::Archive::open(
        std::io::Cursor::new(bytes),
        &docling::ArchiveLimits::from_env(),
    )
    .map_err(convert_err)?;
    let entries = archive.entries().to_vec();
    Ok(entries
        .into_iter()
        .map(|info| {
            let (outcome, result, error) = match info.skipped {
                Some(reason) => ("skipped", None, Some(reason)),
                None => match archive.read(info.index).map_err(convert_err) {
                    Ok(source) => match run_convert(source, cfg) {
                        Ok(raw) => ("converted", Some(raw), None),
                        Err(e) => ("failed", None, Some(e.reason.clone())),
                    },
                    Err(e) => ("failed", None, Some(e.reason.clone())),
                },
            };
            RawArchiveItem {
                path: info.path,
                outcome,
                result,
                error,
            }
        })
        .collect())
}

/// Load a [`SourceDocument`] from an in-memory [`ConvertInput`].
fn source_from_input(input: ConvertInput) -> Result<SourceDocument> {
    match &input.format {
        Some(f) => {
            let format = parse_format(f)?;
            Ok(SourceDocument::from_bytes(
                input.name,
                format,
                input.data.to_vec(),
            ))
        }
        // The engine's own routing (#671): the extension, else the content's
        // signature, else plain text when the bytes are text.
        None => {
            SourceDocument::from_named_bytes(&input.name, input.data.to_vec(), None).map_err(|e| {
                Error::new(
                    Status::InvalidArg,
                    format!("{}: {e}; pass `format` explicitly", input.name),
                )
            })
        }
    }
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
    progress: Option<ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>>,
) -> Result<AsyncTask<ConvertFileTask>> {
    let o = options.unwrap_or_default();
    let mut cfg = build_config(o)?;
    cfg.progress = progress.map(progress_bridge);
    Ok(AsyncTask::new(ConvertFileTask { path, cfg }))
}

/// Async (Promise-returning) [`convert`].
#[napi(ts_return_type = "Promise<ConvertResult>")]
pub fn convert_async(
    input: ConvertInput,
    options: Option<ConvertOptions>,
    progress: Option<ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>>,
) -> Result<AsyncTask<ConvertBytesTask>> {
    let o = options.unwrap_or_default();
    let mut cfg = build_config(o)?;
    cfg.progress = progress.map(progress_bridge);
    let source = source_from_input(input)?;
    Ok(AsyncTask::new(ConvertBytesTask {
        source: Some(source),
        cfg,
    }))
}

/// Convert every document inside a ZIP archive on disk (#557): one
/// [`ArchiveItem`] per entry. Throws only when the file is not a readable ZIP.
#[napi]
pub fn convert_archive_file(
    path: String,
    options: Option<ConvertOptions>,
) -> Result<Vec<ArchiveItem>> {
    let cfg = build_config(options.unwrap_or_default())?;
    let bytes = std::fs::read(&path)
        .map_err(|e| Error::new(Status::GenericFailure, format!("{path}: {e}")))?;
    Ok(run_convert_archive(bytes, &cfg)?
        .into_iter()
        .map(RawArchiveItem::into_js)
        .collect())
}

/// Convert every document inside an in-memory ZIP archive (#557); `input.name`
/// names it, `input.format` is ignored.
#[napi]
pub fn convert_archive(
    input: ConvertInput,
    options: Option<ConvertOptions>,
) -> Result<Vec<ArchiveItem>> {
    let cfg = build_config(options.unwrap_or_default())?;
    Ok(run_convert_archive(input.data.to_vec(), &cfg)?
        .into_iter()
        .map(RawArchiveItem::into_js)
        .collect())
}

/// Async (Promise-returning) [`convert_archive_file`].
#[napi(ts_return_type = "Promise<Array<ArchiveItem>>")]
pub fn convert_archive_file_async(
    path: String,
    options: Option<ConvertOptions>,
) -> Result<AsyncTask<ConvertArchiveTask>> {
    let cfg = build_config(options.unwrap_or_default())?;
    Ok(AsyncTask::new(ConvertArchiveTask {
        source: ArchiveSource::Path(path),
        cfg,
    }))
}

/// Async (Promise-returning) [`convert_archive`].
#[napi(ts_return_type = "Promise<Array<ArchiveItem>>")]
pub fn convert_archive_async(
    input: ConvertInput,
    options: Option<ConvertOptions>,
) -> Result<AsyncTask<ConvertArchiveTask>> {
    let cfg = build_config(options.unwrap_or_default())?;
    Ok(AsyncTask::new(ConvertArchiveTask {
        source: ArchiveSource::Bytes(input.data.to_vec()),
        cfg,
    }))
}

enum ArchiveSource {
    Path(String),
    Bytes(Vec<u8>),
    Taken,
}

pub struct ConvertArchiveTask {
    source: ArchiveSource,
    cfg: ConvertConfig,
}

impl Task for ConvertArchiveTask {
    type Output = Vec<RawArchiveItem>;
    type JsValue = Vec<ArchiveItem>;

    fn compute(&mut self) -> Result<Vec<RawArchiveItem>> {
        let bytes = match std::mem::replace(&mut self.source, ArchiveSource::Taken) {
            ArchiveSource::Path(path) => std::fs::read(&path)
                .map_err(|e| Error::new(Status::GenericFailure, format!("{path}: {e}")))?,
            ArchiveSource::Bytes(bytes) => bytes,
            ArchiveSource::Taken => {
                return Err(Error::new(Status::GenericFailure, "conversion task reused"))
            }
        };
        run_convert_archive(bytes, &self.cfg)
    }

    fn resolve(&mut self, _env: Env, output: Vec<RawArchiveItem>) -> Result<Vec<ArchiveItem>> {
        Ok(output.into_iter().map(RawArchiveItem::into_js).collect())
    }
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
    /// The shared conversion options (#577), validated in the constructor.
    opts: docling::ConvertOptions,
    // Resolved once in the constructor and cloned per call: a converter is
    // configuration, so a missing endpoint should surface at `new`, and the
    // `DOCLING_RS_VLM_*` environment should be read at the same moment every
    // other option is.
    vlm: Option<docling::vlm::VlmOptions>,
    allowed_formats: Option<Vec<InputFormat>>,
    /// The constructor's `showProgress` (#783), for every conversion.
    show_progress: bool,
}

#[napi]
impl DocumentConverter {
    #[napi(constructor)]
    pub fn new(options: Option<ConverterOptions>) -> Result<Self> {
        let o = options.unwrap_or_default();
        let show_progress = o.show_progress.unwrap_or(false);
        let allowed = match &o.allowed_formats {
            Some(list) => Some(
                list.iter()
                    .map(|s| parse_format(s))
                    .collect::<Result<Vec<_>>>()?,
            ),
            None => None,
        };
        let opts = shared_options(&o)?;
        opts.validate().map_err(option_err)?;
        let vlm = opts.vlm_options().map_err(option_err)?;
        Ok(Self {
            opts,
            vlm,
            allowed_formats: allowed,
            show_progress,
        })
    }

    fn config(&self, out: Option<OutputOptions>) -> Result<ConvertConfig> {
        let out = out.unwrap_or_default();
        let to = parse_output_kind(out.to.as_deref())?;
        Ok(ConvertConfig {
            opts: self.opts.clone(),
            vlm: self.vlm.clone(),
            allowed_formats: self.allowed_formats.clone(),
            to,
            image_mode: parse_image_mode(out.image_mode.as_deref(), to)?,
            artifacts_dir: out.artifacts_dir.unwrap_or_else(|| "artifacts".to_string()),
            pandoc_heading_depth: parse_heading_depth(out.pandoc_heading_depth.as_deref())?,
            page_break_placeholder: out.page_break_placeholder,
            show_progress: self.show_progress,
            progress: None,
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
        progress: Option<ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>>,
    ) -> Result<AsyncTask<ConvertFileTask>> {
        let mut cfg = self.config(options)?;
        cfg.progress = progress.map(progress_bridge);
        Ok(AsyncTask::new(ConvertFileTask { path, cfg }))
    }

    /// Async (Promise-returning) bytes conversion (runs off the event loop).
    #[napi(ts_return_type = "Promise<ConvertResult>")]
    pub fn convert_async(
        &self,
        input: ConvertInput,
        options: Option<OutputOptions>,
        progress: Option<ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>>,
    ) -> Result<AsyncTask<ConvertBytesTask>> {
        let mut cfg = self.config(options)?;
        cfg.progress = progress.map(progress_bridge);
        let source = source_from_input(input)?;
        Ok(AsyncTask::new(ConvertBytesTask {
            source: Some(source),
            cfg,
        }))
    }

    /// Convert every document inside a ZIP archive on disk (#557), with this
    /// converter's config per entry (sync).
    #[napi]
    pub fn convert_archive_file(
        &self,
        path: String,
        options: Option<OutputOptions>,
    ) -> Result<Vec<ArchiveItem>> {
        let cfg = self.config(options)?;
        let bytes = std::fs::read(&path)
            .map_err(|e| Error::new(Status::GenericFailure, format!("{path}: {e}")))?;
        Ok(run_convert_archive(bytes, &cfg)?
            .into_iter()
            .map(RawArchiveItem::into_js)
            .collect())
    }

    /// Convert every document inside an in-memory ZIP archive (#557) (sync).
    #[napi]
    pub fn convert_archive(
        &self,
        input: ConvertInput,
        options: Option<OutputOptions>,
    ) -> Result<Vec<ArchiveItem>> {
        let cfg = self.config(options)?;
        Ok(run_convert_archive(input.data.to_vec(), &cfg)?
            .into_iter()
            .map(RawArchiveItem::into_js)
            .collect())
    }

    /// Async (Promise-returning) archive conversion from disk.
    #[napi(ts_return_type = "Promise<Array<ArchiveItem>>")]
    pub fn convert_archive_file_async(
        &self,
        path: String,
        options: Option<OutputOptions>,
    ) -> Result<AsyncTask<ConvertArchiveTask>> {
        let cfg = self.config(options)?;
        Ok(AsyncTask::new(ConvertArchiveTask {
            source: ArchiveSource::Path(path),
            cfg,
        }))
    }

    /// Async (Promise-returning) archive conversion from bytes.
    #[napi(ts_return_type = "Promise<Array<ArchiveItem>>")]
    pub fn convert_archive_async(
        &self,
        input: ConvertInput,
        options: Option<OutputOptions>,
    ) -> Result<AsyncTask<ConvertArchiveTask>> {
        let cfg = self.config(options)?;
        Ok(AsyncTask::new(ConvertArchiveTask {
            source: ArchiveSource::Bytes(input.data.to_vec()),
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
                    cfg.opts.strict.unwrap_or(false),
                    image_mode,
                    cfg.opts.compact_tables.unwrap_or(false),
                    &cfg.artifacts_dir,
                )
                .with_page_break_placeholder(cfg.page_break_placeholder.clone())
                .with_picture_alt_text(cfg.opts.picture_alt_text.unwrap_or(false));
                if doc.metadata_output.front_matter() {
                    streamer = streamer.with_metadata(&doc.metadata);
                }
                for chunk in [streamer.push(&doc.nodes, &doc.links), streamer.finish()] {
                    if !chunk.is_empty() {
                        callback.call(Ok(Some(chunk)), ThreadsafeFunctionCallMode::NonBlocking);
                    }
                }
                // End-of-stream sentinel.
                callback.call(Ok(None), ThreadsafeFunctionCallMode::NonBlocking);
                return;
            }
            let converter = match converter {
                Ok(c) => c,
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
    /// The constructor's `password` (#611, #625), for every conversion.
    password: Option<String>,
    /// The constructor's `showProgress` (#783), for every conversion.
    show_progress: bool,
}

/// The PDF/image options of a [`ConverterOptions`] resolved into the typed
/// values the engine's builders take (#471): what `new Pipeline(options)`
/// primes the warm [`RsPipeline`] with. Validation is the shared
/// `ConvertOptions::validate` [`DocumentConverter::new`] runs too (#577), so
/// the constructor rejects exactly what the one-shot path rejects instead of
/// quietly running the process defaults.
#[derive(Debug, PartialEq)]
struct WarmPipelineConfig {
    /// docling's `do_ocr=False` (`noOcr`).
    no_ocr: bool,
    /// The text-layer fast path (`textLayerOnly`), or its per-document
    /// `auto`.
    text_layer_only: docling::TextLayerOnly,
    /// The password of an encrypted PDF or Office document (#611, #625).
    password: Option<String>,
    no_table_former: bool,
    force_full_page_ocr: bool,
    no_text_panels: bool,
    heading_hierarchy: bool,
    page_range: Option<(usize, usize)>,
    /// docling's `document_timeout` (#497).
    document_timeout: Option<std::time::Duration>,
    ocr_engine: Option<docling::OcrEngine>,
    /// The PP-OCR recognizer; `None` under Tesseract, whose language list is
    /// `tesseract_lang` instead (#460).
    ocr_lang: Option<docling::OcrLang>,
    /// Tesseract's `-l` argument built from `ocrLang`; `None` under PP-OCR.
    tesseract_lang: Option<String>,
    ocr_mode: Option<docling::OcrMode>,
    ocr_scale: Option<f32>,
    /// RapidOCR's text-line orientation handling (#666); `None` = the
    /// environment's default.
    ocr_textline_orientation: Option<bool>,
    /// Picture-crop scale and page images (#519/#520).
    images: docling::ImageOutput,
    enrich: docling::EnrichmentOptions,
}

/// Resolve the PDF/image options for the warm [`Pipeline`] (#471). The
/// `DocumentConverter` keeps these as the validated strings and lets the
/// `docling` converter parse them per call; the warm pipeline is built once,
/// so the strings are parsed into the engine's enums here — the same mapping
/// `docling::DocumentConverter` applies, including the #460 split of
/// `ocrLang` into a PP-OCR model under PP-OCR and Tesseract's `-l` list under
/// Tesseract (against the engine the option selects, else the process's
/// `DOCLING_RS_OCR_ENGINE` default — the library's `ocr_lang()` /
/// `tesseract_lang()` readers).
fn warm_pipeline_config(o: &ConverterOptions) -> Result<WarmPipelineConfig> {
    let s = shared_options(o)?;
    s.validate().map_err(option_err)?;
    Ok(WarmPipelineConfig {
        no_ocr: s.ocr_disabled(),
        text_layer_only: s.text_layer_only.unwrap_or_default(),
        password: s.password.clone(),
        no_table_former: s.no_table_former.unwrap_or(false),
        force_full_page_ocr: s.force_full_page_ocr.unwrap_or(false),
        no_text_panels: s.no_text_panels.unwrap_or(false),
        heading_hierarchy: s.heading_hierarchy.unwrap_or(false),
        page_range: s.page_range().map_err(option_err)?,
        document_timeout: s.document_timeout().map_err(option_err)?,
        ocr_engine: s.ocr_engine().map_err(option_err)?,
        ocr_lang: s.ocr_lang().map_err(option_err)?,
        tesseract_lang: s.tesseract_lang().map_err(option_err)?,
        ocr_mode: s.ocr_mode().map_err(option_err)?,
        ocr_scale: s.ocr_scale,
        ocr_textline_orientation: s.ocr_textline_orientation,
        images: s.image_output(),
        enrich: s.enrichments(),
    })
}

#[napi]
impl Pipeline {
    /// Construct the pipeline. `strict` (cleaner Markdown), the three
    /// enrichment switches (`doPictureClassification`, `doCodeEnrichment`,
    /// `doFormulaEnrichment`, #423) and every PDF/image option the one-shot
    /// calls honour — `ocrEngine`, `ocrLang`, `ocrMode`, `ocrScale`,
    /// `noOcr`, `textLayerOnly`, `password`, `forceFullPageOcr`,
    /// `noTextPanels`, `headingHierarchy`,
    /// `pages` — are read here and apply to every conversion on this
    /// instance, validated exactly as `DocumentConverter` validates them
    /// (#471; before, only `strict` and the enrichment switches were read
    /// and a `por+eng` Tesseract pipeline silently ran PP-OCR English).
    /// `fetchImages` / `allowedFormats` and the non-PDF options don't apply
    /// to the PDF/image pipeline.
    #[napi(constructor)]
    pub fn new(options: Option<ConverterOptions>) -> Result<Self> {
        let options = options.unwrap_or_default();
        let show_progress = options.show_progress.unwrap_or(false);
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
        // Resolve (and so validate) before building: a bad `ocrLang` is an
        // error on `new`, like on `DocumentConverter`, not a warning after
        // the models have loaded.
        let warm = warm_pipeline_config(&options)?;
        let mut pipeline = RsPipeline::new()
            .map_err(convert_err)?
            // docling-pdf's pre-2.0 names: its `skip_ocr` is do_ocr=False,
            // its `no_ocr` the text-layer fast path (#611).
            .skip_ocr(warm.no_ocr)
            .no_ocr(warm.text_layer_only == docling::TextLayerOnly::On)
            .text_layer_auto(warm.text_layer_only == docling::TextLayerOnly::Auto)
            .no_table_former(warm.no_table_former)
            .force_full_page_ocr(warm.force_full_page_ocr)
            .no_text_panels(warm.no_text_panels)
            .heading_hierarchy(docling::HeadingHierarchyOptions::enabled(
                warm.heading_hierarchy,
            ))
            .ocr_engine(warm.ocr_engine)
            .ocr_lang(warm.ocr_lang)
            .tesseract_lang(warm.tesseract_lang)
            .ocr_mode(warm.ocr_mode)
            .ocr_scale(warm.ocr_scale)
            .images_scale(warm.images.scale)
            .generate_page_images(warm.images.page_images)
            .pages(warm.page_range)
            .document_timeout(warm.document_timeout)
            .enrichments(warm.enrich);
        pipeline.set_ocr_textline_orientation(warm.ocr_textline_orientation);
        Ok(Self {
            inner: Arc::new(Mutex::new(pipeline)),
            strict,
            password: warm.password,
            show_progress,
        })
    }

    /// Convert a PDF or image file, reusing the warm models.
    #[napi]
    pub fn convert_file(
        &self,
        path: String,
        options: Option<OutputOptions>,
    ) -> Result<ConvertResult> {
        let cfg = self.output_cfg(options)?;
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
        progress: Option<ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>>,
    ) -> Result<AsyncTask<PipelineFileTask>> {
        let mut cfg = self.output_cfg(options)?;
        cfg.progress = progress.map(progress_bridge);
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
        progress: Option<ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>>,
    ) -> Result<AsyncTask<PipelineBytesTask>> {
        let mut cfg = self.output_cfg(options)?;
        cfg.progress = progress.map(progress_bridge);
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
        let mut cfg = output_config(options, self.strict)?;
        cfg.opts.password = self.password.clone();
        cfg.show_progress = self.show_progress;
        Ok(cfg)
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
    if !matches!(source.format, InputFormat::Pdf | InputFormat::Image) {
        return Err(Error::new(
            Status::InvalidArg,
            format!(
                "Pipeline handles pdf and image inputs (the ML pipeline); got '{}'. \
                 Use convertFile / convert for other formats.",
                source.format.as_str()
            ),
        ));
    }
    // The warm pipeline bypasses `DocumentConverter::convert`: the converter
    // here carries the serializer knobs, the PII pass (#621) and the
    // progress events (#783), whose reporter brackets the conversion as
    // `convert` would.
    let converter = build_converter(cfg)?;
    let reporter = Arc::new(converter.reporter(1, &source.name, None));
    reporter.document_started();
    pipe.set_progress(reporter.page_hook());
    // docling's PARTIAL_SUCCESS (#497): a spent budget leaves the pages done.
    let mut errors: Vec<docling::ErrorItem> = Vec::new();
    let converted = match source.format {
        InputFormat::Pdf => pipe
            .convert_outcome(&source.bytes, cfg.opts.password.as_deref(), &source.name)
            .map(|c| {
                if let docling::Completion::TimedOut { pages_selected, .. } = &c.completion {
                    reporter.fail_pages_cut_off(
                        cfg.opts.pages.as_deref().and_then(first_page).unwrap_or(1),
                        *pages_selected,
                    );
                }
                errors.extend(c.completion.message().map(docling::ErrorItem::timeout));
                c.document
            }),
        _ => pipe.convert_image(&source.bytes, &source.name),
    };
    pipe.set_progress(None);
    let mut doc = match converted {
        Ok(doc) => doc,
        Err(e) => {
            reporter.document_completed(docling::ConversionStatus::Failure);
            return Err(convert_err(e));
        }
    };
    reporter.document_completed(if errors.is_empty() {
        docling::ConversionStatus::Success
    } else {
        docling::ConversionStatus::PartialSuccess
    });
    doc.strict_markdown = strict;
    doc.page_break_placeholder = cfg.page_break_placeholder.clone();
    let redaction = converter.redact(&mut doc).map_err(convert_err)?;
    let status = if errors.is_empty() {
        "success"
    } else {
        "partial_success"
    };
    Ok(render_doc(
        doc,
        cfg,
        source.name,
        source.format.as_str().to_string(),
        status.to_string(),
        errors,
        redaction,
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
        .with_page_break_placeholder(cfg.page_break_placeholder.clone())
        .with_picture_alt_text(cfg.opts.picture_alt_text.unwrap_or(false));
    // The front matter (#679) ahead of the first page, read the way the
    // buffered path reads it.
    if cfg
        .opts
        .metadata_output()
        .ok()
        .flatten()
        .is_some_and(|m| m.front_matter())
    {
        let metadata = docling::DocumentConverter::new()
            .password(cfg.opts.password.clone())
            .document_metadata(&source);
        streamer = streamer.with_metadata(&metadata);
    }
    let emit_chunk = |chunk: String| {
        if !chunk.is_empty() {
            callback.call(Ok(Some(chunk)), ThreadsafeFunctionCallMode::NonBlocking);
        }
    };
    match source.format {
        InputFormat::Pdf => {
            let password = cfg.opts.password.as_deref();
            let result =
                pipe.convert_streaming(&source.bytes, password, &source.name, |nodes, links| {
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
        opts: docling::ConvertOptions {
            strict: Some(strict),
            ..Default::default()
        },
        // The warm `Pipeline` is the ONNX-models class; `Pipeline::new` rejects
        // `pipeline: "vlm"` outright, so nothing reaches here with one set.
        vlm: None,
        allowed_formats: None,
        to: parse_output_kind(out.to.as_deref())?,
        image_mode: parse_image_mode(
            out.image_mode.as_deref(),
            parse_output_kind(out.to.as_deref())?,
        )?,
        artifacts_dir: out.artifacts_dir.unwrap_or_else(|| "artifacts".to_string()),
        pandoc_heading_depth: parse_heading_depth(out.pandoc_heading_depth.as_deref())?,
        page_break_placeholder: out.page_break_placeholder,
        show_progress: false,
        progress: None,
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
// Email attachments (#561).
// ---------------------------------------------------------------------------

/// One attachment of an `.eml` / Outlook `.msg`, from [`email_attachments`].
#[napi(object)]
pub struct EmailAttachment {
    /// Position among the message's attachments.
    pub index: u32,
    /// A safe file name, unique within the message (base name only, control
    /// and bidi-format characters removed; `attachment-N` when the message
    /// declares none, `<subject>.eml` for a forwarded message; a repeated
    /// name gets `-2`, `-3`, …) — safe to write into one directory as it is.
    pub name: String,
    /// The declared media type (`type/subtype`), if any.
    pub content_type: Option<String>,
    /// The format id it converts as (`"pdf"`, `"docx"`, `"email"`, …), from
    /// its extension, else its media type, else the bytes; absent when
    /// `skipped` says why it will not convert.
    pub format: Option<String>,
    /// Payload size in bytes (0 without a payload).
    pub size: i64,
    /// An image the message shows inline (part of the HTML body) rather than
    /// a file to open; a PDF disposed `inline` is still `false` (#564).
    pub inline: bool,
    /// Why it is not converted, when it is not: no payload (a reference, an
    /// OLE object), over a limit, a nested archive, an unsupported type.
    pub skipped: Option<String>,
    /// The payload, when kept: every attachment with a payload within the
    /// limits — also an unsupported type or an archive (hand a `.zip` to
    /// your own extractor). Absent over a limit or without a payload.
    pub data: Option<Buffer>,
}

/// Bounds on what [`email_attachments`] keeps, in bytes. Unset = the archive
/// defaults: 10 000 attachments, 256 MiB each, 1 GiB in all.
#[napi(object)]
#[derive(Default)]
pub struct EmailAttachmentOptions {
    pub max_entries: Option<u32>,
    pub max_entry_size: Option<i64>,
    pub max_total_size: Option<i64>,
}

/// The attachments of an in-memory `.eml` / `.msg` with their payloads. A
/// forwarded message is an `.eml` entry whose `data` is the nested message.
/// Convert one with `convert({ name: att.name, data: att.data, format: att.format })`
/// — `format` carries what the extension cannot (a `scan.bin` sent as
/// `application/pdf`, a nameless sniffed part; #564).
#[napi]
pub fn email_attachments(
    input: ConvertInput,
    options: Option<EmailAttachmentOptions>,
) -> Result<Vec<EmailAttachment>> {
    attachments_of(&input.data, options.unwrap_or_default())
}

/// [`email_attachments`] for a message file on disk.
#[napi]
pub fn email_attachments_file(
    path: String,
    options: Option<EmailAttachmentOptions>,
) -> Result<Vec<EmailAttachment>> {
    let bytes = std::fs::read(&path).map_err(convert_err)?;
    attachments_of(&bytes, options.unwrap_or_default())
}

/// Async (Promise-returning) [`email_attachments`]: a large `.msg` parses off
/// the event loop (#564).
#[napi(ts_return_type = "Promise<Array<EmailAttachment>>")]
pub fn email_attachments_async(
    input: ConvertInput,
    options: Option<EmailAttachmentOptions>,
) -> Result<AsyncTask<EmailAttachmentsTask>> {
    Ok(AsyncTask::new(EmailAttachmentsTask {
        source: ArchiveSource::Bytes(input.data.to_vec()),
        options: options.unwrap_or_default(),
    }))
}

/// Async (Promise-returning) [`email_attachments_file`].
#[napi(ts_return_type = "Promise<Array<EmailAttachment>>")]
pub fn email_attachments_file_async(
    path: String,
    options: Option<EmailAttachmentOptions>,
) -> Result<AsyncTask<EmailAttachmentsTask>> {
    Ok(AsyncTask::new(EmailAttachmentsTask {
        source: ArchiveSource::Path(path),
        options: options.unwrap_or_default(),
    }))
}

pub struct EmailAttachmentsTask {
    source: ArchiveSource,
    options: EmailAttachmentOptions,
}

/// A Send-safe [`EmailAttachment`] (raw bytes, no `Buffer`). Public only as
/// the task's output.
#[doc(hidden)]
pub struct RawEmailAttachment {
    index: u32,
    name: String,
    content_type: Option<String>,
    format: Option<String>,
    size: i64,
    inline: bool,
    skipped: Option<String>,
    data: Option<Vec<u8>>,
}

impl Task for EmailAttachmentsTask {
    type Output = Vec<RawEmailAttachment>;
    type JsValue = Vec<EmailAttachment>;

    fn compute(&mut self) -> Result<Vec<RawEmailAttachment>> {
        let bytes = match std::mem::replace(&mut self.source, ArchiveSource::Taken) {
            ArchiveSource::Path(path) => std::fs::read(&path)
                .map_err(|e| Error::new(Status::GenericFailure, format!("{path}: {e}")))?,
            ArchiveSource::Bytes(bytes) => bytes,
            ArchiveSource::Taken => {
                return Err(Error::new(Status::GenericFailure, "attachment task reused"))
            }
        };
        let options = std::mem::take(&mut self.options);
        raw_attachments_of(&bytes, options)
    }

    fn resolve(
        &mut self,
        _env: Env,
        output: Vec<RawEmailAttachment>,
    ) -> Result<Vec<EmailAttachment>> {
        Ok(output
            .into_iter()
            .map(RawEmailAttachment::into_js)
            .collect())
    }
}

impl RawEmailAttachment {
    fn into_js(self) -> EmailAttachment {
        EmailAttachment {
            index: self.index,
            name: self.name,
            content_type: self.content_type,
            format: self.format,
            size: self.size,
            inline: self.inline,
            skipped: self.skipped,
            data: self.data.map(Buffer::from),
        }
    }
}

fn attachments_of(bytes: &[u8], o: EmailAttachmentOptions) -> Result<Vec<EmailAttachment>> {
    Ok(raw_attachments_of(bytes, o)?
        .into_iter()
        .map(RawEmailAttachment::into_js)
        .collect())
}

fn raw_attachments_of(bytes: &[u8], o: EmailAttachmentOptions) -> Result<Vec<RawEmailAttachment>> {
    // The same `DOCLING_RS_ZIP_MAX_*` defaults the converter, CLI and serve
    // apply (#564); an explicit option overrides its field.
    let defaults = docling::ArchiveLimits::from_env();
    let non_negative = |v: Option<i64>, default: u64| match v {
        Some(v) if v < 0 => Err(Error::new(
            Status::InvalidArg,
            "email attachment limits must be non-negative byte counts",
        )),
        Some(v) => Ok(v as u64),
        None => Ok(default),
    };
    let limits = docling::ArchiveLimits {
        max_entries: o.max_entries.map_or(defaults.max_entries, |n| n as usize),
        max_entry_size: non_negative(o.max_entry_size, defaults.max_entry_size)?,
        max_total_size: non_negative(o.max_total_size, defaults.max_total_size)?,
        ..defaults
    };
    let atts = docling::EmailAttachments::open(bytes, &limits).map_err(convert_err)?;
    Ok(atts
        .entries()
        .iter()
        .map(|e| RawEmailAttachment {
            index: e.index as u32,
            name: e.name.clone(),
            content_type: e.content_type.clone(),
            format: e.format.map(|f| f.as_str().to_string()),
            size: e.size as i64,
            inline: e.inline,
            skipped: e.skipped.clone(),
            data: atts.data(e.index).map(<[u8]>::to_vec),
        })
        .collect())
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
        "text",
        "jsonl",
        "xml",
        "ipynb",
        "feed",
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

/// The format a buffer evidently is, by its content alone (#672 — the
/// engine's `InputFormat::detect`): the format id, or `null` for bytes
/// without a signature (prose, Markdown, CSV, random bytes). Signatures
/// only: PDF, the Office / ODF / EPUB / iWork packages, the OLE files,
/// RTF, HTML, the raster images, DjVu, audio and video containers,
/// WordPerfect, WebVTT, DocTags, SVG, AbiWord, XML, JSON and Jupyter
/// notebooks, MIME messages, LaTeX. What `convert({ name, data })` does
/// for a name with no or an unknown extension.
#[napi]
pub fn detect_format(data: Buffer) -> Option<String> {
    InputFormat::detect(&data).map(|f| f.as_str().to_string())
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
        Some("text") => Ok(OutputKind::Text),
        Some("html") => Ok(OutputKind::Html),
        Some("pandoc") => Ok(OutputKind::Pandoc),
        Some("vtt") => Ok(OutputKind::Vtt),
        Some(other) => Err(Error::new(
            Status::InvalidArg,
            format!(
                "unknown `to` '{other}' (expected: markdown, json, html, text, latex, pandoc, vtt)"
            ),
        )),
    }
}

/// `pandocHeadingDepth` (#848): `"docling"` (default) or `"source"`.
fn parse_heading_depth(s: Option<&str>) -> Result<docling::pandoc::HeadingDepth> {
    s.unwrap_or_default()
        .parse()
        .map_err(|e: String| Error::from_reason(format!("pandocHeadingDepth: {e}")))
}

/// `imageMode` for output `to`: unset is placeholder, except embedded for
/// the Pandoc AST (#537 — `pandoc -t docx` drops a target-less picture).
fn parse_image_mode(mode: Option<&str>, to: OutputKind) -> Result<ImageMode> {
    match mode.map(str::to_ascii_lowercase).as_deref() {
        None if to == OutputKind::Pandoc => Ok(ImageMode::Embedded),
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
        "text" | "plain" => InputFormat::Text,
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

    /// The VLM resolution as the one-shot options reach it: the flat
    /// `ConvertOptions` object → the shared set → `vlm_options()` (#577).
    #[allow(clippy::too_many_arguments)]
    fn resolve_vlm(
        pipeline: Option<&str>,
        endpoint: Option<String>,
        model: Option<String>,
        api_key: Option<String>,
        prompt: Option<String>,
        max_tokens: Option<u32>,
        page_range: Option<(usize, usize)>,
    ) -> Result<Option<docling::vlm::VlmOptions>> {
        let o = ConvertOptions {
            pipeline: pipeline.map(str::to_string),
            vlm_endpoint: endpoint,
            vlm_model: model,
            vlm_api_key: api_key,
            vlm_prompt: prompt,
            vlm_max_tokens: max_tokens,
            pages: page_range.map(|(a, b)| format!("{a}-{b}")),
            ..Default::default()
        };
        shared_options(&o)?.vlm_options().map_err(option_err)
    }

    /// The option names reach the error messages as TypeScript spells them.
    #[test]
    fn errors_name_options_in_camel_case() {
        assert_eq!(camel_case("ocr_lang"), "ocrLang");
        assert_eq!(camel_case("document_timeout"), "documentTimeout");
        let Err(err) = DocumentConverter::new(Some(ConverterOptions {
            document_timeout: Some(-5.0),
            ..Default::default()
        })) else {
            panic!("a negative documentTimeout must be rejected");
        };
        assert_eq!(err.status, Status::InvalidArg);
        assert!(err.reason.contains("documentTimeout"), "{}", err.reason);
        let Err(err) = DocumentConverter::new(Some(ConverterOptions {
            vlm_max_tokens: Some(0),
            pipeline: Some("vlm".into()),
            vlm_endpoint: Some("http://127.0.0.1:1/v1".into()),
            vlm_model: Some("m".into()),
            ..Default::default()
        })) else {
            panic!("vlmMaxTokens: 0 must be rejected");
        };
        assert!(err.reason.contains("vlmMaxTokens"), "{}", err.reason);
    }

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
    /// four the shared `vlm_options()` applies on top of
    /// `VlmOptions::resolve` (api_key, prompt, max_tokens, page_range). Precedence against a *set*
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

    /// `new Pipeline(options)` primes the warm engine with every PDF/image
    /// option the one-shot path honours (#471) — the same shared option set
    /// as `DocumentConverter`, resolved into the engine's typed values, with
    /// `ocrLang` read against the engine it drives (#460): Tesseract's `-l`
    /// `textLayerOnly` is the bool it always was or the string `"auto"`,
    /// which the warm pipeline keeps as its per-document switch; another
    /// string is an `InvalidArg` naming the option.
    #[test]
    fn warm_pipeline_reads_text_layer_only_auto() {
        let with = |v: Either<bool, String>| {
            warm_pipeline_config(&ConverterOptions {
                text_layer_only: Some(v),
                ..Default::default()
            })
        };
        assert_eq!(
            with(Either::A(true)).unwrap().text_layer_only,
            docling::TextLayerOnly::On
        );
        assert_eq!(
            with(Either::A(false)).unwrap().text_layer_only,
            docling::TextLayerOnly::Off
        );
        assert_eq!(
            with(Either::B("auto".into())).unwrap().text_layer_only,
            docling::TextLayerOnly::Auto
        );
        let err = with(Either::B("sometimes".into())).unwrap_err();
        assert!(
            err.reason.contains("text_layer_only"),
            "reason: {}",
            err.reason
        );
        let one_shot = shared_options(&ConvertOptions {
            text_layer_only: Some(Either::B("auto".into())),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(one_shot.text_layer_only, Some(docling::TextLayerOnly::Auto));
    }

    /// list under Tesseract, the en/ch recognizer under PP-OCR.
    #[test]
    fn warm_pipeline_reads_the_pdf_options() {
        let got = warm_pipeline_config(&ConverterOptions {
            ocr_engine: Some("tesseract".into()),
            ocr_lang: Some("por+eng".into()),
            ocr_mode: Some("full_page".into()),
            ocr_scale: Some(3.0),
            images_scale: Some(1.5),
            page_images: Some(true),
            skip_ocr: Some(true),
            force_full_page_ocr: Some(true),
            no_text_panels: Some(true),
            heading_hierarchy: Some(true),
            pages: Some("2-3".into()),
            do_formula_enrichment: Some(true),
            ..Default::default()
        })
        .expect("valid options");
        assert_eq!(got.ocr_engine, Some(docling::OcrEngine::Tesseract));
        assert_eq!(got.tesseract_lang.as_deref(), Some("por+eng"));
        assert_eq!(got.ocr_lang, None, "under Tesseract ocrLang is its -l list");
        assert_eq!(got.ocr_mode, Some(docling::OcrMode::FullPage));
        assert_eq!(got.ocr_scale, Some(3.0));
        assert_eq!(
            got.images,
            docling::ImageOutput {
                scale: Some(1.5),
                page_images: true
            }
        );
        // `skipOcr` is read as `noOcr` (its pre-2.0 name, #611).
        assert!(got.no_ocr);
        assert_eq!(got.text_layer_only, docling::TextLayerOnly::Off);
        assert!(got.force_full_page_ocr);
        assert!(got.no_text_panels);
        assert!(got.heading_hierarchy);
        assert_eq!(got.page_range, Some((2, 3)));
        assert!(got.enrich.formula);
        assert!(!got.enrich.code);

        let got = warm_pipeline_config(&ConverterOptions {
            ocr_engine: Some("ppocr".into()),
            ocr_lang: Some("zh-Hans".into()),
            ..Default::default()
        })
        .expect("valid options");
        assert_eq!(got.ocr_engine, Some(docling::OcrEngine::PpOcr));
        assert_eq!(got.ocr_lang, Some(docling::OcrLang::Ch));
        assert_eq!(got.tesseract_lang, None, "PP-OCR takes no Tesseract list");
    }

    /// No option → `None` everywhere, so the engine keeps its own defaults
    /// (`DOCLING_RS_OCR_*`, the 2.0 px/pt render) exactly as before #471.
    #[test]
    fn warm_pipeline_defaults_choose_nothing() {
        let got = warm_pipeline_config(&ConverterOptions::default()).expect("defaults");
        assert_eq!(
            got,
            WarmPipelineConfig {
                no_ocr: false,
                text_layer_only: docling::TextLayerOnly::Off,
                password: None,
                no_table_former: false,
                force_full_page_ocr: false,
                no_text_panels: false,
                heading_hierarchy: false,
                page_range: None,
                document_timeout: None,
                ocr_engine: None,
                ocr_lang: None,
                tesseract_lang: None,
                ocr_mode: None,
                ocr_scale: None,
                ocr_textline_orientation: None,
                images: docling::ImageOutput::default(),
                enrich: docling::EnrichmentOptions::default(),
            }
        );
    }

    /// The two classes share one validation contract: whatever
    /// `new DocumentConverter(o)` throws on, `new Pipeline(o)` throws on too —
    /// #471's `new Pipeline({ ocrEngine: 'tesseract', ocrLang: 'xx' })` used
    /// to construct fine and OCR in English.
    #[test]
    fn warm_pipeline_rejects_what_document_converter_rejects() {
        let bad = [
            ConverterOptions {
                ocr_engine: Some("bogus".into()),
                ..Default::default()
            },
            ConverterOptions {
                ocr_engine: Some("tesseract".into()),
                ocr_lang: Some("xx".into()),
                ..Default::default()
            },
            ConverterOptions {
                ocr_engine: Some("ppocr".into()),
                ocr_lang: Some("deu".into()),
                ..Default::default()
            },
            ConverterOptions {
                ocr_mode: Some("sideways".into()),
                ..Default::default()
            },
            ConverterOptions {
                ocr_scale: Some(0.0),
                ..Default::default()
            },
            ConverterOptions {
                images_scale: Some(9.0),
                ..Default::default()
            },
            ConverterOptions {
                pages: Some("3-1".into()),
                ..Default::default()
            },
        ];
        for o in bad {
            assert!(
                DocumentConverter::new(Some(o.clone())).is_err(),
                "DocumentConverter should reject {o:?}"
            );
            assert!(
                warm_pipeline_config(&o).is_err(),
                "Pipeline must reject what DocumentConverter rejects: {o:?}"
            );
        }
    }
}
