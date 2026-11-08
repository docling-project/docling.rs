//! The one set of conversion options every surface speaks (#577).
//!
//! [`ConvertOptions`] is the serializable, surface-neutral form of what
//! configures a [`DocumentConverter`]: every knob of its builder plus the
//! pipeline selection (`standard` | `vlm`) and the `vlm_*` settings. The CLI
//! fills it from flags, docling-serve deserializes it from the query string /
//! JSON body / multipart text parts, the C ABI and the wasm module take it as
//! one JSON object, the Python and Node bindings map their keyword arguments
//! into it — and all of them then go through the same two methods:
//! [`ConvertOptions::validate`] (one set of rejection rules, one set of
//! messages) and [`ConvertOptions::apply`] (one mapping onto the builder).
//! Before this module each surface re-declared the fields, re-implemented
//! the checks and re-mapped the names, so a new option meant edits in a dozen
//! files and the surfaces drifted (the C ABI had no `list_attachments`, the
//! Node bindings never rejected `vlmMaxTokens: 0`, …).
//!
//! Every field is an `Option`: `None` means "not given", and the engine's
//! own default — defined once, in [`DocumentConverter::default`] — applies.
//! That is what lets docling-serve layer its three option sources
//! ([`merge_options`]: a later layer's `Some` wins, its `None` keeps the
//! earlier value) and what lets a binding start from a pre-configured base
//! converter and apply only what the caller said.
//!
//! The field names are the wire names: docling-serve's request options, the
//! C ABI's and the wasm module's JSON keys, the CLI flags with `_` → `-`, the
//! Node options in camelCase (napi's rename), the Python keyword arguments.
//! The few places a surface spells one differently are recorded in
//! [`OPTIONS`], the table the inventory test
//! (`crates/docling/tests/options_inventory.rs`) checks every surface's
//! documentation against — a field added here without its row, flag, kwarg
//! or OpenAPI entry fails that test rather than shipping unreachable.
//!
//! Not in here: what to *emit* (`to`, the image mode, the Pandoc API version,
//! the chunker settings — [`crate::chunks::ChunkOptions`]) and where. Those
//! depend on what the surface can do with the result (serve streams, the
//! browser cannot write files, the CLI writes several formats at once), so
//! each surface keeps its own output options next to this struct.

use std::fmt;
use std::str::FromStr;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use docling_core::{
    CustomPattern, ImageRedaction, MetadataOutput, PiiKind, RedactionOptions, Replacement,
    TrackChanges,
};

use crate::DocumentConverter;

/// Conversion options as every surface accepts them — see the module docs.
/// `None` everywhere is the engine's defaults.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConvertOptions {
    // --- output shaping carried on the document -----------------------------
    /// Cleaner, more conformant Markdown ([`DocumentConverter::strict`]).
    pub strict: Option<bool>,
    /// Compact `| a | b |` Markdown tables instead of width padding (#271).
    pub compact_tables: Option<bool>,
    /// Render pictures' alt text (OOXML `descr`/`title`, #676) in Markdown
    /// and HTML; the JSON carries it always. Strict Markdown implies it.
    pub picture_alt_text: Option<bool>,
    /// Text inserted between pages in the Markdown export (docling-core's
    /// `MarkdownParams.page_break_placeholder`); docling-serve's historical
    /// spelling `md_page_break_placeholder` is accepted as an alias.
    #[serde(alias = "md_page_break_placeholder")]
    pub page_break_placeholder: Option<String>,

    // --- declarative formats -------------------------------------------------
    /// Resolve external `<img src>` for HTML/EPUB/MHTML/JATS (network access)
    /// — the pre-#646 switch: `true` is `image_sources=remote`, `false` is
    /// `none`; an explicit `image_sources` wins.
    pub fetch_images: Option<bool>,
    /// Which image references resolve (#646): `none` (default — every picture
    /// a placeholder) | `embedded` (`data:` URIs and parts of the same
    /// container: EPUB/MHTML entries, email `cid:` attachments; no
    /// filesystem or network) | `local` (plus files under the source file's
    /// directory) | `remote` (plus `http(s)` fetches). HTML, EPUB, MHTML,
    /// JATS, AsciiDoc, ODF, Markdown and email bodies.
    pub image_sources: Option<String>,
    /// Hosts a `remote` image fetch may reach (#646): exact names or
    /// `*.suffix` wildcards, a list or a comma-separated string; unset =
    /// any host. A redirect is held to the same list.
    #[serde(default, deserialize_with = "de_string_list")]
    pub image_hosts: Option<Vec<String>>,
    /// Largest image that resolves, in bytes (#646; 32 MiB,
    /// `DOCLING_RS_MAX_IMAGE_BYTES`).
    pub max_image_bytes: Option<u64>,
    /// How many images resolve per document; the rest stay placeholders
    /// (#646; unlimited, `DOCLING_RS_MAX_IMAGES`).
    pub max_images: Option<usize>,
    /// Total resolved image bytes per document, in MiB (#646; unlimited,
    /// `DOCLING_RS_MAX_IMAGE_TOTAL_MB`).
    pub max_image_total_mb: Option<u64>,
    /// Smallest image that resolves, in bytes — skips spacer / tracking
    /// pixels (#646; 0, `DOCLING_RS_MIN_IMAGE_BYTES`).
    pub min_image_bytes: Option<u64>,
    /// Email (.eml/.msg): append an Attachments section (#251).
    pub list_attachments: Option<bool>,
    /// Omit empty cells from sparse XLSX/XLS table grids (#271).
    pub skip_empty_cells: Option<bool>,
    /// EBCDIC copybook layout (#252): inline JSON or a file path.
    pub ebcdic_layout: Option<String>,
    /// Character encoding of text inputs (a WHATWG label); unset = detect.
    pub encoding: Option<String>,
    /// Jupyter notebooks (#673): which cell outputs the document keeps —
    /// `none` (default: the notebook as written) or `text` (stream and
    /// `text/plain` results as code blocks, image outputs as pictures).
    pub ipynb_outputs: Option<String>,
    /// Document metadata (#679): where the file's own metadata (EPUB
    /// Dublin Core, Office properties, EXIF, audio tags, PDF Info) goes —
    /// `none` (default: nowhere, every export unchanged), `json` (a
    /// `metadata` object in the docling JSON) or `frontmatter` (that, and
    /// YAML front matter at the top of the Markdown).
    pub metadata: Option<String>,
    /// Tracked changes in Office documents (#851, Pandoc's
    /// `--track-changes`): `accept` (insertions shown, deletions left out),
    /// `reject` (the reverse) or `all` (both kept as typed revisions with
    /// author and date — the Pandoc AST's `Span .insertion` / `.deletion`).
    /// Unset: the readers' own view (DOCX and ODT accept; the `.doc`
    /// reader's text before #851). A mode a reader has not implemented is
    /// refused (`ConversionError::UnsupportedOption`).
    pub track_changes: Option<String>,
    /// Structured diagrams from DOCX DrawingML drawings (#858): `"mermaid"`
    /// (the default) adds a `mermaid` code item after a drawing's picture
    /// with the flowchart its shapes and connectors describe — only when
    /// every connector names its two end shapes (`a:stCxn` / `a:endCxn`); a
    /// drawing whose lines are laid by eye gets no graph (a
    /// `DOCLING_RS_DEBUG` diagnostic says why), never an invented one. The
    /// picture stays either way. `"none"` turns the extraction off.
    pub extract_diagrams: Option<String>,
    /// Word (DOCX / DOC): a paragraph style's `w:outlineLvl` alone makes it
    /// a heading (docling's `MsWordBackendOptions.use_outline_level_for_headings`,
    /// docling#4480; default `true`) — how a localized or custom heading
    /// style ("Nadpis1") is recognized. `false` stops that for templates
    /// whose body styles carry an outline level only for the TOC (numbered
    /// legal clauses); a style named a heading still takes its level from
    /// the outline either way.
    pub use_outline_level_for_headings: Option<bool>,
    /// Pre-render HTML with a headless browser (the `web-browser` feature).
    pub use_web_browser: Option<bool>,
    /// XBRL: the directory the instance's taxonomy is read from.
    pub xbrl_taxonomy: Option<String>,

    // --- audio / video -------------------------------------------------------
    /// ASR preset (`whisper_*`, `parakeet_tdt_0.6b_v3`); unset = Whisper tiny.
    pub asr_model: Option<String>,
    /// Transcription language (a Whisper code) or `auto`.
    pub asr_lang: Option<String>,
    /// Max frames sampled from a video (0 = transcript only); unset =
    /// [`crate::DEFAULT_VIDEO_FRAMES`]. `"all"` — or any count at or above
    /// `u32::MAX` — is [`crate::ALL_VIDEO_FRAMES`]: every distinct cut (#647).
    #[serde(deserialize_with = "de_video_frames")]
    pub video_frames: Option<usize>,
    /// ffmpeg's scene score (0–1) a video frame must exceed to be a cut
    /// (#647); unset = 0.27 (`DOCLING_RS_VIDEO_SCENE_THRESHOLD`).
    pub video_scene_threshold: Option<f32>,
    /// Cap on a sampled video frame's longer side in px, applied inside
    /// ffmpeg (#647); 0 / unset = the source resolution
    /// (`DOCLING_RS_VIDEO_FRAME_MAX_SIDE`).
    pub video_frame_max_side: Option<u32>,
    /// Hamming distance (0–64) of the difference hash under which a sampled
    /// frame is a duplicate of a kept one and dropped (#647); unset = keep
    /// every frame (`DOCLING_RS_VIDEO_FRAME_DEDUPE`).
    pub video_frame_dedupe: Option<u32>,

    // --- PDF / image pipeline ------------------------------------------------
    /// PDF page window, `"A-B"` or a single `"N"` (1-based inclusive, #80).
    pub pages: Option<String>,
    /// Per-document budget in seconds for the PDF pipeline (#497).
    pub document_timeout: Option<f64>,
    /// Skip the whole ML stack: text layer only (#611; `no_ocr` before 2.0).
    /// `true` / `false`, or `auto` — the text layer alone when it carries the
    /// document, the full pipeline (OCR) when it does not
    /// ([`TextLayerOnly`]).
    #[serde(deserialize_with = "de_text_layer_only")]
    pub text_layer_only: Option<TextLayerOnly>,
    /// Keep layout + TableFormer, never run OCR — docling's `do_ocr=False`,
    /// its CLI's `--no-ocr` (#611).
    pub no_ocr: Option<bool>,
    /// `no_ocr` under its pre-2.0 name (#244), still read: either one set
    /// skips OCR ([`Self::ocr_disabled`]). A field of its own rather than a
    /// serde alias, so a body that sends both spellings (a pre-2.0 form)
    /// is not a duplicate-field error.
    pub skip_ocr: Option<bool>,
    /// The password of an encrypted PDF or Office document (#611, #625).
    /// `pdf_password`, docling's PDF-only name (`--pdf-password`), is read
    /// as an alias.
    #[serde(alias = "pdf_password")]
    pub password: Option<String>,
    /// OCR every page, discarding the text layer.
    pub force_full_page_ocr: Option<bool>,
    /// OCR only the pages whose text layer is broken (#1006).
    pub replace_broken_text_layer: Option<bool>,
    /// Skip TableFormer (geometric tables instead).
    pub no_table_former: Option<bool>,
    /// Disable the text-panel heuristic (#173).
    pub no_text_panels: Option<bool>,
    /// Infer heading levels after assembly (#302).
    pub heading_hierarchy: Option<bool>,
    /// `ppocr` (default) | `tesseract` (#460).
    pub ocr_engine: Option<String>,
    /// OCR language: `en` | `ch` or a BCP-47 tag under PP-OCR, tessdata stems
    /// or BCP-47 tags under Tesseract.
    pub ocr_lang: Option<String>,
    /// docling's `OcrMode` (#254): `default` | `full_page` | `layout_regions`
    /// | `pdf_aware_layout_regions`.
    pub ocr_mode: Option<String>,
    /// OCR input scale in px per PDF point (#254); unset = the 2.0 render.
    pub ocr_scale: Option<f32>,
    /// The PDF text layer's word-gap rule (#921): `adaptive` (default,
    /// docling-parse ≥ 7's line-local threshold) | `legacy` (docling-parse
    /// ≤ 6) | a factor in (0, 4] of the flanking glyphs' mean advance.
    pub pdf_word_gap: Option<String>,
    /// Keep a PDF's typographic punctuation (`„ “ ‚ ‘`, dashes, `…`) as drawn
    /// instead of folding it to ASCII (#921). Default off — docling folds.
    pub pdf_keep_typography: Option<bool>,
    /// Report what the PDF reader repaired to read the input (#934) on the
    /// result's `pdf_repairs` — a stream whose `/Length` does not end at its
    /// `endstream`, a padded xref. Default off
    /// (`DOCLING_RS_PDF_REPORT_REPAIRS`).
    pub pdf_report_repairs: Option<bool>,
    /// RapidOCR's text-line orientation handling (#666): oriented crops, the
    /// 90° turn of tall lines and the 0/180 classifier. Default off
    /// (`DOCLING_RS_OCR_TEXTLINE_ORIENTATION`).
    pub ocr_textline_orientation: Option<bool>,
    /// Picture-crop / page-image scale in px per point, 0.1–4.0 (#520).
    pub images_scale: Option<f32>,
    /// Keep each page's render as the JSON page image (docling's
    /// `generate_page_images`, #520 — accepted as an alias).
    #[serde(alias = "generate_page_images")]
    pub page_images: Option<bool>,
    /// A PDF picture takes its one safe embedded image at native resolution
    /// instead of the render crop (docling's `use_embedded_picture_images`,
    /// #1007). Default off.
    pub use_embedded_picture_images: Option<bool>,
    /// Enrichment: classify pictures with DocumentFigureClassifier (#423).
    pub do_picture_classification: Option<bool>,
    /// Enrichment: rewrite code blocks with CodeFormulaV2 (#423).
    pub do_code_enrichment: Option<bool>,
    /// Enrichment: decode display formulas to LaTeX with CodeFormulaV2 (#423).
    pub do_formula_enrichment: Option<bool>,
    /// Enrichment: OCR the embedded pictures of non-PDF documents (#645);
    /// the text becomes the picture's description annotation.
    pub do_picture_ocr: Option<bool>,
    /// Comma-separated DocumentFigureClassifier labels a picture must be
    /// classified as to be OCR'd (#645); unset/empty = every picture.
    pub picture_ocr_classes: Option<String>,
    /// Smallest side in px a picture must have to be OCR'd (#645; default
    /// 32, `DOCLING_RS_PICTURE_OCR_MIN_SIDE`).
    pub picture_ocr_min_side: Option<u32>,
    /// Picture OCR resolution in px per picture pixel before the 2000 px cap
    /// (#666); `1.0` = RapidOCR's native pixels. Unset = the image-input
    /// rule (`DOCLING_RS_PICTURE_OCR_SCALE`).
    pub picture_ocr_scale: Option<f32>,
    /// Keep the embedded image bytes on the pictures (#645); `false` drops
    /// them after the enrichment pass. Default `true`.
    pub keep_picture_images: Option<bool>,
    /// Enrichment: describe the pictures of every format through an
    /// OpenAI-compatible vision endpoint (#670) — the answer is the
    /// picture's description annotation, as `do_picture_ocr` writes it.
    pub do_picture_description: Option<bool>,
    /// The vision endpoint (`/v1` base or the full `chat/completions` URL);
    /// falls back to `vlm_endpoint`, then `DOCLING_RS_VLM_ENDPOINT`.
    pub picture_description_endpoint: Option<String>,
    /// Model name; falls back to `vlm_model`, then `DOCLING_RS_VLM_MODEL`.
    pub picture_description_model: Option<String>,
    /// Bearer token; falls back to `vlm_api_key`, then `DOCLING_RS_VLM_API_KEY`.
    pub picture_description_api_key: Option<String>,
    /// The instruction sent with every picture; unset = the mode's prompt
    /// (MarkItDown's caption prompt, markitdown-ocr's extraction prompt).
    pub picture_description_prompt: Option<String>,
    /// `caption` (default) | `ocr`: a caption, or the text in the picture.
    pub picture_description_mode: Option<String>,
    /// Pictures in flight at once (default 1); 0 is rejected.
    pub picture_description_concurrency: Option<usize>,

    // --- PII redaction (#621) ---------------------------------------------
    /// Redact personal data from the converted document before any export.
    pub redact_pii: Option<bool>,
    /// What a span becomes: `label` (default) | `pseudonym` | `fixed:<text>`.
    pub redact_mode: Option<String>,
    /// Comma-separated kinds to redact (`email,phone,credit_card,iban,
    /// ip_address,url_credentials,national_id,person,organization,
    /// location,address`); unset = every kind.
    pub redact_kinds: Option<String>,
    /// Extra patterns, `NAME=REGEX` entries separated by newlines (the CLI
    /// flag repeats); the match (or capture group 1) is redacted as `[NAME]`.
    pub redact_pattern: Option<String>,
    /// Embedded images and page renders: `drop` (default) | `box_out` | `keep`.
    pub redact_images: Option<String>,
    /// Comma-separated terms never redacted even when a detector flags
    /// them (#694) — `support@example.com`, the company's own name.
    pub redact_allow_terms: Option<String>,
    /// Comma-separated terms always redacted, as `[REDACTED]` — a project
    /// code name, a client the NER misses.
    pub redact_deny_terms: Option<String>,
    /// Regexes (one per line) a flagged span must not match in full to be
    /// redacted — a document's own number shapes: part numbers, record IDs.
    pub redact_allow_patterns: Option<String>,
    /// The NER detector's minimum confidence, 0–1 (default 0.85).
    pub redact_ner_min_score: Option<f32>,

    // --- pipeline selection --------------------------------------------------
    /// `standard` (default) | `vlm` (#77): the remote vision model instead of
    /// the local ML stack. The `vlm_*` options are inert under `standard`.
    pub pipeline: Option<String>,
    /// VLM endpoint; falls back to `DOCLING_RS_VLM_ENDPOINT`.
    pub vlm_endpoint: Option<String>,
    /// VLM model name; falls back to `DOCLING_RS_VLM_MODEL`.
    pub vlm_model: Option<String>,
    /// Bearer token for the endpoint; falls back to `DOCLING_RS_VLM_API_KEY`.
    pub vlm_api_key: Option<String>,
    /// Per-page instruction; falls back to `DOCLING_RS_VLM_PROMPT`.
    pub vlm_prompt: Option<String>,
    /// `max_tokens` per completion (default 8192); 0 is rejected.
    pub vlm_max_tokens: Option<usize>,
}

/// How far a PDF goes before the ML stack: [`DocumentConverter::text_layer_only`].
///
/// On the wire the option is the bool it always was — `true` / `false` — or
/// the string `auto`; a text surface (query parameter, multipart field,
/// CLI `--text-layer-only=…`) also takes `1`/`0`, `yes`/`no`, `on`/`off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextLayerOnly {
    /// The full pipeline: layout, TableFormer, OCR on the pages without a
    /// text layer (the default).
    #[default]
    Off,
    /// The embedded text layer alone, no model loaded: flat paragraphs in
    /// reading order; a scanned page yields nothing (`true`).
    On,
    /// Per document: as `On` when the text layer carries the document, as
    /// `Off` — the pipeline, OCR included — when it does not (a scanned /
    /// image-only file, or a vestigial layer of a few typed-in form fields
    /// over scans). Decided from the text layer alone before any model is
    /// touched, so a digital PDF pays no model load and a scanned one loses
    /// no text. The verdict is the whole document's: a digital file with a
    /// few scanned pages inserted keeps the text path, and those pages come
    /// back empty as under `On`.
    Auto,
}

impl TextLayerOnly {
    /// The option's accepted spellings, for error messages.
    pub const ACCEPTED: &'static str = "true, false, auto";

    /// Parse the wire spelling (case-insensitive); `None` for any other.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "1" | "true" | "yes" | "on" => Some(Self::On),
            "0" | "false" | "no" | "off" => Some(Self::Off),
            _ => None,
        }
    }

    /// The wire spelling: `true`, `false`, `auto`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "false",
            Self::On => "true",
            Self::Auto => "auto",
        }
    }
}

impl From<bool> for TextLayerOnly {
    fn from(on: bool) -> Self {
        if on {
            Self::On
        } else {
            Self::Off
        }
    }
}

impl std::fmt::Display for TextLayerOnly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for TextLayerOnly {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| {
            format!(
                "text_layer_only: expected one of {}, got {s:?}",
                Self::ACCEPTED
            )
        })
    }
}

/// On the wire the two bool values stay bools — a JSON body that sent
/// `"text_layer_only": true` before the `auto` value reads back unchanged —
/// and `auto` is the string.
impl Serialize for TextLayerOnly {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Off => s.serialize_bool(false),
            Self::On => s.serialize_bool(true),
            Self::Auto => s.serialize_str("auto"),
        }
    }
}

impl<'de> Deserialize<'de> for TextLayerOnly {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Flag(bool),
            Count(u64),
            Text(String),
        }
        let parsed = match Raw::deserialize(d)? {
            Raw::Flag(b) => Some(Self::from(b)),
            Raw::Count(0) => Some(Self::Off),
            Raw::Count(1) => Some(Self::On),
            Raw::Count(_) => None,
            Raw::Text(s) => Self::parse(&s),
        };
        parsed.ok_or_else(|| {
            serde::de::Error::custom(format!(
                "text_layer_only: expected one of {}",
                Self::ACCEPTED
            ))
        })
    }
}

/// `text_layer_only` on the wire: absent, a bool, or the string `auto`
/// ([`TextLayerOnly`]).
fn de_text_layer_only<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<TextLayerOnly>, D::Error> {
    Option::<TextLayerOnly>::deserialize(d)
}

/// What a picture is asked for (#670): a caption, or the text it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PictureDescriptionMode {
    /// MarkItDown's `llm_caption`: "Write a detailed caption for this image."
    #[default]
    Caption,
    /// markitdown-ocr's extraction prompt: the text only, layout kept, no
    /// commentary — LLM-vision OCR for pictures the local OCR models read
    /// poorly (handwriting, stylized text, charts).
    Ocr,
}

impl PictureDescriptionMode {
    /// `caption` | `ocr`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "caption" => Some(Self::Caption),
            "ocr" => Some(Self::Ocr),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Caption => "caption",
            Self::Ocr => "ocr",
        }
    }

    /// The prompt sent when the options name none.
    pub fn default_prompt(self) -> &'static str {
        match self {
            Self::Caption => CAPTION_PROMPT,
            Self::Ocr => OCR_PROMPT,
        }
    }
}

/// MarkItDown's caption prompt (`_llm_caption.py`), verbatim.
pub const CAPTION_PROMPT: &str = "Write a detailed caption for this image.";

/// markitdown-ocr's extraction prompt (`_ocr_service.py`), verbatim.
pub const OCR_PROMPT: &str = "Extract all text from this image. Return ONLY the extracted text, \
                              maintaining the original layout and order. Do not add any commentary \
                              or description.";

/// A rejected option: which field, and why. The message names the field in
/// its wire spelling (`ocr_scale must be a positive number, got 0`); the CLI
/// prints it through [`OptionsError::cli_message`], which swaps in the flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionsError {
    /// The wire name of the offending option — a [`ConvertOptions`] field.
    pub field: &'static str,
    /// Why it was rejected; names the field.
    pub message: String,
}

impl OptionsError {
    fn new(field: &'static str, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }

    /// The message with the field spelled as its CLI flag (`--ocr-scale must
    /// be a positive number, got 0`).
    pub fn cli_message(&self) -> String {
        let flag = cli_flag(self.field);
        if self.message.contains(self.field) {
            self.message.replacen(self.field, &flag, 1)
        } else {
            format!("{flag}: {}", self.message)
        }
    }
}

impl fmt::Display for OptionsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OptionsError {}

/// Which conversion pipeline an option set selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineKind {
    /// The local ML stack (layout, TableFormer, OCR) — the default.
    Standard,
    /// A remote OpenAI-compatible vision model (#77).
    Vlm,
}

/// How one option is spelled on each surface, where it differs from the wire
/// name — the central documentation the inventory test holds every surface
/// to. One row per [`ConvertOptions`] field (the test checks both ways).
#[derive(Debug, Clone, Copy)]
pub struct OptionInfo {
    /// The field / wire name.
    pub name: &'static str,
    /// Other wire spellings accepted on deserialization (`serde(alias)`).
    pub aliases: &'static [&'static str],
    /// The CLI flag; `None` = the default `--name-with-dashes`.
    pub cli: Option<&'static str>,
    /// The Python `DocumentConverter(...)` keyword argument; `Some("")` when
    /// the Python surface has no such argument (the reason is in the row's
    /// comment); `None` = the field name itself.
    pub python: Option<&'static str>,
    /// The Node option (napi spells the field in camelCase); `Some("")` when
    /// the Node surface has no such option (reason in the row's comment);
    /// `None` = the camelCase of the field name.
    pub node: Option<&'static str>,
}

const fn row(name: &'static str) -> OptionInfo {
    OptionInfo {
        name,
        aliases: &[],
        cli: None,
        python: None,
        node: None,
    }
}

/// The per-surface spelling table — see [`OptionInfo`].
pub const OPTIONS: &[OptionInfo] = &[
    // Python picks Markdown strictness at export time (`export_to_markdown`
    // reads `document.strict_markdown`), so the converter takes no kwarg.
    OptionInfo {
        python: Some(""),
        ..row("strict")
    },
    row("compact_tables"),
    row("picture_alt_text"),
    // Python: `export_to_markdown(page_break_placeholder=…)`, docling's own
    // signature, not a converter kwarg.
    OptionInfo {
        aliases: &["md_page_break_placeholder"],
        python: Some(""),
        ..row("page_break_placeholder")
    },
    row("fetch_images"),
    row("image_sources"),
    row("image_hosts"),
    row("max_image_bytes"),
    row("max_images"),
    row("max_image_total_mb"),
    row("min_image_bytes"),
    row("list_attachments"),
    row("skip_empty_cells"),
    row("ebcdic_layout"),
    row("encoding"),
    row("ipynb_outputs"),
    row("metadata"),
    row("track_changes"),
    row("extract_diagrams"),
    OptionInfo {
        cli: Some("--no-outline-level-headings"),
        ..row("use_outline_level_for_headings")
    },
    // The npm addon is not built with the `web-browser` feature, so the
    // option would only ever error there.
    OptionInfo {
        node: Some(""),
        ..row("use_web_browser")
    },
    row("xbrl_taxonomy"),
    row("asr_model"),
    row("asr_lang"),
    row("video_frames"),
    row("video_scene_threshold"),
    row("video_frame_max_side"),
    row("video_frame_dedupe"),
    // Python takes docling's `page_range=(first, last)` tuple.
    OptionInfo {
        python: Some("page_range"),
        ..row("pages")
    },
    row("document_timeout"),
    // Python keeps docling's positive spellings: `do_ocr`,
    // `do_table_structure` — and the docling.rs-only `text_layer_only`.
    // `no_ocr` is docling's `--no-ocr` since 2.0 (#611); `skip_ocr` is its
    // pre-2.0 name, still read (the CLI's `--skip-ocr` too) — Python has
    // docling's `do_ocr` for both.
    row("text_layer_only"),
    OptionInfo {
        python: Some("do_ocr"),
        ..row("no_ocr")
    },
    OptionInfo {
        python: Some(""),
        ..row("skip_ocr")
    },
    // One password for PDFs and Office documents (#625); docling's
    // PDF-only `pdf_password` stays readable on every surface.
    OptionInfo {
        aliases: &["pdf_password"],
        ..row("password")
    },
    row("force_full_page_ocr"),
    row("replace_broken_text_layer"),
    OptionInfo {
        python: Some("do_table_structure"),
        ..row("no_table_former")
    },
    row("no_text_panels"),
    row("heading_hierarchy"),
    row("ocr_engine"),
    row("ocr_lang"),
    row("ocr_mode"),
    row("ocr_scale"),
    row("pdf_word_gap"),
    row("pdf_keep_typography"),
    row("pdf_report_repairs"),
    row("ocr_textline_orientation"),
    row("images_scale"),
    OptionInfo {
        aliases: &["generate_page_images"],
        python: Some("generate_page_images"),
        ..row("page_images")
    },
    row("use_embedded_picture_images"),
    OptionInfo {
        cli: Some("--enrich-picture-classes"),
        ..row("do_picture_classification")
    },
    OptionInfo {
        cli: Some("--enrich-code"),
        ..row("do_code_enrichment")
    },
    OptionInfo {
        cli: Some("--enrich-formula"),
        ..row("do_formula_enrichment")
    },
    OptionInfo {
        cli: Some("--picture-ocr"),
        ..row("do_picture_ocr")
    },
    row("picture_ocr_classes"),
    row("picture_ocr_min_side"),
    row("picture_ocr_scale"),
    OptionInfo {
        cli: Some("--no-picture-images"),
        ..row("keep_picture_images")
    },
    OptionInfo {
        cli: Some("--picture-description"),
        ..row("do_picture_description")
    },
    row("picture_description_endpoint"),
    row("picture_description_model"),
    row("picture_description_api_key"),
    row("picture_description_prompt"),
    row("picture_description_mode"),
    row("picture_description_concurrency"),
    row("redact_pii"),
    row("redact_mode"),
    row("redact_kinds"),
    row("redact_pattern"),
    row("redact_images"),
    row("redact_allow_terms"),
    row("redact_deny_terms"),
    OptionInfo {
        cli: Some("--redact-allow-pattern"),
        ..row("redact_allow_patterns")
    },
    row("redact_ner_min_score"),
    row("pipeline"),
    row("vlm_endpoint"),
    row("vlm_model"),
    row("vlm_api_key"),
    row("vlm_prompt"),
    row("vlm_max_tokens"),
];

/// The CLI flag for a [`ConvertOptions`] field: its [`OPTIONS`] row's `cli`,
/// else `--` + the name with `_` → `-`.
pub fn cli_flag(field: &str) -> String {
    OPTIONS
        .iter()
        .find(|o| o.name == field)
        .and_then(|o| o.cli)
        .map(str::to_string)
        .unwrap_or_else(|| format!("--{}", field.replace('_', "-")))
}

/// `over` on top of `base`: every field `over` sets (serializes non-null)
/// replaces `base`'s, every one it leaves unset keeps `base`'s — for any
/// all-`Option` options struct, which is why docling-serve's request struct
/// (this one plus its output fields) merges its query / body / form layers
/// through it without a per-field list.
pub fn merge_options<T: Serialize + DeserializeOwned + Default>(over: T, base: T) -> T {
    let mut merged = serde_json::to_value(base).unwrap_or(serde_json::Value::Null);
    let over = serde_json::to_value(over).unwrap_or(serde_json::Value::Null);
    if let (Some(into), Some(from)) = (merged.as_object_mut(), over.as_object()) {
        for (key, value) in from {
            if !value.is_null() {
                into.insert(key.clone(), value.clone());
            }
        }
    }
    // A struct that serialized to an object deserializes from it; the
    // fallback only guards a `T` that is not an object at all.
    serde_json::from_value(merged).unwrap_or_default()
}

/// A list option on the wire: a JSON array, or one string the text surfaces
/// (query, multipart) send — split on commas, trimmed, empties dropped.
fn de_string_list<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        List(Vec<String>),
        Text(String),
    }
    Ok(Option::<Raw>::deserialize(d)?.map(|raw| {
        let items: Vec<String> = match raw {
            Raw::List(v) => v,
            Raw::Text(s) => s.split(',').map(str::to_string).collect(),
        };
        items
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }))
}

/// `video_frames` on the wire (#647): an integer, or `"all"` (any case,
/// also a digit string, which the text surfaces send) for
/// [`crate::ALL_VIDEO_FRAMES`]; a count at or above `u32::MAX` reads as
/// "all" too, so a 32-bit binding can ask for it.
fn de_video_frames<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<usize>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Count(u64),
        Text(String),
    }
    let raw = Option::<Raw>::deserialize(d)?;
    let n = match raw {
        None => return Ok(None),
        Some(Raw::Count(n)) => n,
        Some(Raw::Text(s)) => {
            let s = s.trim();
            if s.eq_ignore_ascii_case("all") {
                return Ok(Some(crate::ALL_VIDEO_FRAMES));
            }
            s.parse::<u64>().map_err(|_| {
                serde::de::Error::custom(format!(
                    "video_frames: expected a non-negative integer or \"all\", got {s:?}"
                ))
            })?
        }
    };
    Ok(Some(if n >= u32::MAX as u64 {
        crate::ALL_VIDEO_FRAMES
    } else {
        n as usize
    }))
}

impl ConvertOptions {
    /// The image tier `image_sources` names (#646); `None` when unset (the
    /// `fetch_images` alias, else the converter's default, decides).
    pub fn image_sources(&self) -> Result<Option<crate::ImageSources>, OptionsError> {
        self.image_sources
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                s.parse::<crate::ImageSources>()
                    .map_err(|e| OptionsError::new("image_sources", e))
            })
            .transpose()
    }

    /// The notebook outputs `ipynb_outputs` names (#673); `None` when unset
    /// (the converter's default, `none`).
    pub fn ipynb_outputs(&self) -> Result<Option<crate::IpynbOutputs>, OptionsError> {
        self.ipynb_outputs
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                s.parse::<crate::IpynbOutputs>()
                    .map_err(|e| OptionsError::new("ipynb_outputs", e))
            })
            .transpose()
    }

    /// The tracked-changes policy (#851): `None` unless `track_changes` is
    /// set and non-blank; a spelling other than `accept`, `reject`, `all`
    /// is an error.
    pub fn track_changes(&self) -> Result<Option<TrackChanges>, OptionsError> {
        self.track_changes
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                s.parse::<TrackChanges>()
                    .map_err(|e| OptionsError::new("track_changes", e))
            })
            .transpose()
    }

    /// The diagram extraction (#858): `None` unless `extract_diagrams` is
    /// set and non-blank (the converter's default, `mermaid`, applies); a
    /// spelling other than `mermaid` or `none` is an error.
    pub fn extract_diagrams(&self) -> Result<Option<DiagramFormat>, OptionsError> {
        self.extract_diagrams
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                s.parse::<DiagramFormat>()
                    .map_err(|e| OptionsError::new("extract_diagrams", e))
            })
            .transpose()
    }

    /// The metadata output mode (#679): `None` unless `metadata` is set
    /// and non-blank; a spelling other than `none`, `json`, `frontmatter`
    /// is an error.
    pub fn metadata_output(&self) -> Result<Option<MetadataOutput>, OptionsError> {
        self.metadata
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                MetadataOutput::parse(s).ok_or_else(|| {
                    OptionsError::new(
                        "metadata",
                        format!(
                            "metadata must be one of {}; got {s:?}",
                            MetadataOutput::ACCEPTED
                        ),
                    )
                })
            })
            .transpose()
    }

    /// The wire names of every field, from the struct itself (an unset
    /// option serializes as `null`), so a new field is listed without a
    /// hand-kept list — the inventory test and the unknown-key checks of the
    /// JSON surfaces read it.
    pub fn field_names() -> Vec<String> {
        match serde_json::to_value(Self::default()) {
            Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
            _ => Vec::new(),
        }
    }

    /// The keys among `keys` that are neither a field, one of its aliases,
    /// nor in `also_known` — what a JSON surface that rejects typos (the C
    /// ABI, the wasm module) reports.
    pub fn unknown_keys<'a>(
        keys: impl IntoIterator<Item = &'a str>,
        also_known: &[&str],
    ) -> Vec<String> {
        let fields = Self::field_names();
        keys.into_iter()
            .filter(|k| {
                !fields.iter().any(|f| f == k)
                    && !also_known.contains(k)
                    && !OPTIONS.iter().any(|o| o.aliases.contains(k))
            })
            .map(str::to_string)
            .collect()
    }

    /// `self` on top of `base` — see [`merge_options`].
    pub fn merge_over(self, base: ConvertOptions) -> ConvertOptions {
        merge_options(self, base)
    }

    /// The readings of a text value, in the order a text surface tries
    /// them: the string itself, an integer, a decimal number, then a
    /// boolean (`1`/`true`/`yes`/`on`, `0`/`false`/`no`/`off`, any case).
    pub fn text_candidates(text: &str) -> Vec<serde_json::Value> {
        let trimmed = text.trim();
        let mut candidates = vec![serde_json::Value::String(text.to_string())];
        if let Ok(n) = trimmed.parse::<u64>() {
            candidates.push(serde_json::Value::from(n));
        } else if let Ok(f) = trimmed.parse::<f64>() {
            if let Some(n) = serde_json::Number::from_f64(f) {
                candidates.push(serde_json::Value::Number(n));
            }
        }
        match trimmed.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => candidates.push(serde_json::Value::Bool(true)),
            "0" | "false" | "no" | "off" => candidates.push(serde_json::Value::Bool(false)),
            _ => {}
        }
        candidates
    }

    /// Set one option from its text form — a query parameter, a multipart
    /// text part, the string an MCP client's form sends for every value.
    /// The first reading of [`text_candidates`](Self::text_candidates) the
    /// option accepts lands: `pages=5` stays the text `"5"`, `strict=true`
    /// / `strict=1` / `strict=yes` set the flag, `ocr_scale=1.5` the number.
    /// A name (or alias) that is no option is left alone (`Ok(false)`); a
    /// value no reading accepts — `strict=maybe`, `document_timeout=soon`
    /// — is an error naming the option.
    pub fn set_text(&mut self, name: &str, text: &str) -> Result<bool, OptionsError> {
        let Some(field) = OPTIONS
            .iter()
            .find(|o| o.name == name || o.aliases.contains(&name))
            .map(|o| o.name)
        else {
            return Ok(false);
        };
        for value in Self::text_candidates(text) {
            let mut patch = serde_json::Map::new();
            patch.insert(name.to_string(), value);
            if let Ok(parsed) =
                serde_json::from_value::<ConvertOptions>(serde_json::Value::Object(patch))
            {
                *self = parsed.merge_over(std::mem::take(self));
                return Ok(true);
            }
        }
        Err(OptionsError::new(
            field,
            format!("{name}: cannot read {text:?} as that option's value"),
        ))
    }

    /// A JSON object of options whose values may be text: what an MCP
    /// client's form sends (MCP Inspector types every value as a string —
    /// `{"force_full_page_ocr": "true", "ocr_scale": "1.5"}`), or a
    /// hand-written call. A text value is read with
    /// [`set_text`](Self::set_text); a typed value as the JSON it is, or,
    /// when the option is text (`{"pages": 3}`) or the type is off
    /// (`{"strict": 1}`), as its text; `null` leaves the option unset. An
    /// unknown key, or a value no reading accepts, is an error naming it.
    pub fn from_loose_json(
        map: serde_json::Map<String, serde_json::Value>,
    ) -> Result<Self, OptionsError> {
        let mut out = Self::default();
        for (name, value) in map {
            let known = OPTIONS
                .iter()
                .find(|o| o.name == name || o.aliases.contains(&name.as_str()))
                .map(|o| o.name);
            let Some(field) = known else {
                return Err(OptionsError::new(
                    "options",
                    format!("unknown field `{name}` (see docs/OPTIONS.md for the list)"),
                ));
            };
            match value {
                serde_json::Value::Null => {}
                serde_json::Value::String(text) => {
                    out.set_text(&name, &text)?;
                }
                other => {
                    let text = match &other {
                        serde_json::Value::Number(n) => Some(n.to_string()),
                        serde_json::Value::Bool(b) => Some(b.to_string()),
                        _ => None,
                    };
                    let mut patch = serde_json::Map::new();
                    patch.insert(name.clone(), other);
                    match serde_json::from_value::<ConvertOptions>(serde_json::Value::Object(patch))
                    {
                        Ok(parsed) => out = parsed.merge_over(out),
                        Err(e) => match text {
                            Some(t) if out.set_text(&name, &t).is_ok() => {}
                            _ => return Err(OptionsError::new(field, format!("{name}: {e}"))),
                        },
                    }
                }
            }
        }
        Ok(out)
    }

    /// Check every option that can be checked without the input document —
    /// the one set of rules all surfaces apply, before any work starts:
    /// `pages` spelling, a positive `document_timeout`, a positive
    /// `ocr_scale` and `picture_ocr_scale`, `images_scale` in 0.1–4.0, a
    /// `pdf_word_gap` docling-pdf reads, a known `pipeline`, a positive
    /// `vlm_max_tokens`, and (with the `pdf` feature, which has the engine
    /// tables) a known `ocr_engine` / `ocr_mode`, an `ocr_lang` the
    /// selected engine reads and `picture_ocr_classes` the classifier
    /// predicts. `asr_lang`, `encoding` and `ebcdic_layout` are
    /// checked against the model / codec / copybook when the conversion runs.
    pub fn validate(&self) -> Result<(), OptionsError> {
        self.page_range()?;
        self.document_timeout()?;
        self.ipynb_outputs()?;
        self.track_changes()?;
        self.extract_diagrams()?;
        self.metadata_output()?;
        if let Some(s) = self.ocr_scale {
            if !(s.is_finite() && s > 0.0) {
                return Err(OptionsError::new(
                    "ocr_scale",
                    format!("ocr_scale must be a positive number, got {s}"),
                ));
            }
        }
        if let Some(s) = self.picture_ocr_scale {
            if !(s.is_finite() && s > 0.0) {
                return Err(OptionsError::new(
                    "picture_ocr_scale",
                    format!("picture_ocr_scale must be a positive number, got {s}"),
                ));
            }
        }
        self.picture_description_mode()?;
        if self.picture_description_concurrency == Some(0) {
            return Err(OptionsError::new(
                "picture_description_concurrency",
                "picture_description_concurrency must be a positive integer, got 0",
            ));
        }
        if let Some(s) = self.images_scale {
            if !(0.1..=4.0).contains(&s) {
                return Err(OptionsError::new(
                    "images_scale",
                    format!("images_scale must be a number in 0.1-4.0, got {s}"),
                ));
            }
        }
        self.image_sources()?;
        if self.max_image_bytes == Some(0) {
            return Err(OptionsError::new(
                "max_image_bytes",
                "max_image_bytes must be a positive number of bytes, got 0",
            ));
        }
        if let Some(t) = self.video_scene_threshold {
            if !(t.is_finite() && (0.0..=1.0).contains(&t)) {
                return Err(OptionsError::new(
                    "video_scene_threshold",
                    format!("video_scene_threshold must be a number in 0.0-1.0, got {t}"),
                ));
            }
        }
        if let Some(d) = self.video_frame_dedupe {
            if d > 64 {
                return Err(OptionsError::new(
                    "video_frame_dedupe",
                    format!("video_frame_dedupe must be a Hamming distance in 0-64, got {d}"),
                ));
            }
        }
        self.pipeline()?;
        if self.vlm_max_tokens == Some(0) {
            return Err(OptionsError::new(
                "vlm_max_tokens",
                "vlm_max_tokens must be a positive integer, got 0",
            ));
        }
        #[cfg(feature = "pdf")]
        {
            self.ocr_engine()?;
            self.ocr_mode()?;
            self.ocr_lang()?;
        }
        #[cfg(any(feature = "pdf", feature = "pdf-text"))]
        self.pdf_word_gap()?;
        self.picture_ocr_classes()?;
        self.redaction()?;
        Ok(())
    }

    /// The parsed `picture_ocr_classes` (#645): the comma-separated labels,
    /// trimmed and lower-cased, empty when unset. With the `pdf` feature
    /// (which has the classifier's label table) a label the
    /// DocumentFigureClassifier never predicts is rejected — a typo would
    /// otherwise silently skip every picture.
    pub fn picture_ocr_classes(&self) -> Result<Vec<String>, OptionsError> {
        let labels: Vec<String> = self
            .picture_ocr_classes
            .as_deref()
            .unwrap_or("")
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        #[cfg(feature = "pdf")]
        if let Some(bad) = labels
            .iter()
            .find(|l| !docling_pdf::enrich::PICTURE_CLASSES.contains(&l.as_str()))
        {
            return Err(OptionsError::new(
                "picture_ocr_classes",
                format!(
                    "picture_ocr_classes: {bad:?} is not a DocumentFigureClassifier label ({})",
                    docling_pdf::enrich::PICTURE_CLASSES.join(", ")
                ),
            ));
        }
        Ok(labels)
    }

    /// The parsed `picture_description_mode` (#670): `caption` (the
    /// default) or `ocr`.
    pub fn picture_description_mode(&self) -> Result<PictureDescriptionMode, OptionsError> {
        match self.picture_description_mode.as_deref().map(str::trim) {
            None | Some("") => Ok(PictureDescriptionMode::Caption),
            Some(m) => PictureDescriptionMode::parse(m).ok_or_else(|| {
                OptionsError::new(
                    "picture_description_mode",
                    format!("picture_description_mode must be caption or ocr, got {m:?}"),
                )
            }),
        }
    }

    /// The picture-description pass's resolved options (#670): `None`
    /// unless `do_picture_description` is set; else the endpoint, model and
    /// key from the `picture_description_*` options, the `vlm_*` options,
    /// then the `DOCLING_RS_VLM_*` environment — endpoint and model being
    /// required — with the prompt, mode and concurrency given here.
    #[cfg(feature = "vlm")]
    pub fn picture_description_options(
        &self,
    ) -> Result<Option<crate::vlm::PictureDescriptionOptions>, OptionsError> {
        if self.do_picture_description != Some(true) {
            return Ok(None);
        }
        let set = |s: &Option<String>| s.clone().filter(|v| !v.trim().is_empty());
        let endpoint = set(&self.picture_description_endpoint).or_else(|| set(&self.vlm_endpoint));
        let model = set(&self.picture_description_model).or_else(|| set(&self.vlm_model));
        let mut o =
            crate::vlm::PictureDescriptionOptions::resolve(endpoint, model).map_err(|e| {
                let msg = match e {
                    crate::ConversionError::Parse(m) => m,
                    other => other.to_string(),
                }
                .replace(
                    "pass --picture-description-endpoint",
                    "set picture_description_endpoint",
                )
                .replace(
                    "pass --picture-description-model",
                    "set picture_description_model",
                );
                let field = if msg.contains("picture_description_model") {
                    "picture_description_model"
                } else {
                    "picture_description_endpoint"
                };
                OptionsError::new(field, msg)
            })?;
        if let Some(k) = set(&self.picture_description_api_key).or_else(|| set(&self.vlm_api_key)) {
            o.api_key = Some(k);
        }
        o.prompt = set(&self.picture_description_prompt);
        o.mode = self.picture_description_mode()?;
        if let Some(n) = self.picture_description_concurrency {
            o.concurrency = n;
        }
        Ok(Some(o))
    }

    /// The parsed PII redaction request (#621): `None` unless `redact_pii`
    /// is set; the mode, kinds, patterns and image mode checked — an
    /// unknown spelling or an invalid regex is rejected here, naming the
    /// field, rather than failing the conversion.
    pub fn redaction(&self) -> Result<Option<RedactionOptions>, OptionsError> {
        let mut opts = RedactionOptions::default();
        if let Some(m) = self.redact_mode.as_deref().filter(|m| !m.trim().is_empty()) {
            opts.replacement = Replacement::parse(m).ok_or_else(|| {
                OptionsError::new(
                    "redact_mode",
                    format!(
                        "redact_mode must be one of {} , got {m:?}",
                        Replacement::ACCEPTED
                    ),
                )
            })?;
        }
        if let Some(k) = self.redact_kinds.as_deref() {
            for name in k.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let kind = PiiKind::parse(name).ok_or_else(|| {
                    OptionsError::new(
                        "redact_kinds",
                        format!(
                            "redact_kinds: {name:?} is not a PII kind ({})",
                            PiiKind::ACCEPTED
                        ),
                    )
                })?;
                if !opts.kinds.contains(&kind) {
                    opts.kinds.push(kind);
                }
            }
        }
        if let Some(p) = self.redact_pattern.as_deref() {
            opts.custom_patterns = p.lines().filter_map(CustomPattern::parse).collect();
            // Compile now: the error names the field.
            docling_core::redact::PatternDetector::new(&opts)
                .map_err(|e| OptionsError::new("redact_pattern", e.0))?;
        }
        let list = |s: &Option<String>| -> Vec<String> {
            s.as_deref()
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .collect()
        };
        opts.allow_terms = list(&self.redact_allow_terms);
        opts.deny_terms = list(&self.redact_deny_terms);
        if let Some(p) = self.redact_allow_patterns.as_deref() {
            opts.allow_patterns = p
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect();
            docling_core::redact::AllowList::new(&opts).map_err(|e| {
                OptionsError::new(
                    "redact_allow_patterns",
                    format!("redact_allow_patterns: {}", e.0),
                )
            })?;
        }
        if let Some(score) = self.redact_ner_min_score {
            if !(score.is_finite() && (0.0..=1.0).contains(&score)) {
                return Err(OptionsError::new(
                    "redact_ner_min_score",
                    format!("redact_ner_min_score must be a number in 0.0-1.0, got {score}"),
                ));
            }
            opts.ner_min_score = score;
        }
        if let Some(i) = self
            .redact_images
            .as_deref()
            .filter(|i| !i.trim().is_empty())
        {
            opts.images = ImageRedaction::parse(i).ok_or_else(|| {
                OptionsError::new(
                    "redact_images",
                    format!(
                        "redact_images must be one of {}, got {i:?}",
                        ImageRedaction::ACCEPTED
                    ),
                )
            })?;
        }
        Ok(self.redact_pii.unwrap_or(false).then_some(opts))
    }

    /// Validate, then set every given option on `base` — unset ones leave the
    /// base's values (so a surface may pre-configure defaults of its own,
    /// e.g. docling-serve's `--strict`). [`DocumentConverter::from_options`]
    /// is this on a fresh converter.
    pub fn apply(&self, mut c: DocumentConverter) -> Result<DocumentConverter, OptionsError> {
        self.validate()?;
        if let Some(v) = self.strict {
            c = c.strict(v);
        }
        if let Some(v) = self.compact_tables {
            c = c.compact_tables(v);
        }
        if let Some(v) = self.picture_alt_text {
            c = c.picture_alt_text(v);
        }
        if self.page_break_placeholder.is_some() {
            c = c.page_break_placeholder(self.page_break_placeholder.clone());
        }
        if let Some(v) = self.fetch_images {
            c = c.fetch_images(v);
        }
        if let Some(sources) = self.image_sources()? {
            c = c.image_sources(sources);
        }
        if let Some(hosts) = &self.image_hosts {
            c = c.image_hosts(hosts.iter().cloned());
        }
        if let Some(v) = self.max_image_bytes {
            c = c.max_image_bytes(v);
        }
        if let Some(v) = self.max_images {
            c = c.max_images(v);
        }
        if let Some(v) = self.max_image_total_mb {
            c = c.max_image_total_mb(v);
        }
        if let Some(v) = self.min_image_bytes {
            c = c.min_image_bytes(v);
        }
        if let Some(v) = self.list_attachments {
            c = c.list_attachments(v);
        }
        if let Some(v) = self.skip_empty_cells {
            c = c.skip_empty_cells(v);
        }
        if self.ebcdic_layout.is_some() {
            c = c.ebcdic_layout_opt(self.ebcdic_layout.clone());
        }
        if self.encoding.is_some() {
            c = c.encoding(self.encoding.clone());
        }
        if let Some(outputs) = self.ipynb_outputs()? {
            c = c.ipynb_outputs(outputs);
        }
        if let Some(mode) = self.metadata_output()? {
            c = c.metadata_output(mode);
        }
        if let Some(policy) = self.track_changes()? {
            c = c.track_changes(Some(policy));
        }
        if let Some(format) = self.extract_diagrams()? {
            c = c.extract_diagrams(format);
        }
        if let Some(v) = self.use_outline_level_for_headings {
            c = c.use_outline_level_for_headings(v);
        }
        if let Some(v) = self.use_web_browser {
            c = c.use_web_browser(v);
        }
        if let Some(dir) = &self.xbrl_taxonomy {
            c = c.xbrl_taxonomy(dir);
        }
        if self.asr_model.is_some() {
            c = c.asr_model(self.asr_model.clone());
        }
        if self.asr_lang.is_some() {
            c = c.asr_lang(self.asr_lang.clone());
        }
        if let Some(n) = self.video_frames {
            c = c.video_frames(n);
        }
        if let Some(t) = self.video_scene_threshold {
            c = c.video_scene_threshold(t);
        }
        if let Some(px) = self.video_frame_max_side {
            c = c.video_frame_max_side(px);
        }
        if let Some(d) = self.video_frame_dedupe {
            c = c.video_frame_dedupe(Some(d));
        }
        if let Some((first, last)) = self.page_range()? {
            c = c.page_range(first, last);
        }
        if self.document_timeout.is_some() {
            c = c.document_timeout(self.document_timeout()?);
        }
        if let Some(v) = self.text_layer_only {
            c = c.text_layer_only(v);
        }
        if self.no_ocr.is_some() || self.skip_ocr.is_some() {
            c = c.no_ocr(self.ocr_disabled());
        }
        if self.password.is_some() {
            c = c.password(self.password.clone());
        }
        if let Some(v) = self.force_full_page_ocr {
            c = c.force_full_page_ocr(v);
        }
        if let Some(v) = self.replace_broken_text_layer {
            c = c.replace_broken_text_layer(v);
        }
        if let Some(v) = self.no_table_former {
            c = c.no_table_former(v);
        }
        if let Some(v) = self.no_text_panels {
            c = c.no_text_panels(v);
        }
        if let Some(v) = self.heading_hierarchy {
            c = c.heading_hierarchy(v);
        }
        if let Some(v) = &self.ocr_engine {
            c = c.ocr_engine(v.clone());
        }
        if let Some(v) = &self.ocr_lang {
            c = c.ocr_lang(v.clone());
        }
        if let Some(v) = &self.ocr_mode {
            c = c.ocr_mode(v.clone());
        }
        if let Some(s) = self.ocr_scale {
            c = c.ocr_scale(s);
        }
        if let Some(v) = &self.pdf_word_gap {
            c = c.pdf_word_gap(v.clone());
        }
        if let Some(v) = self.pdf_keep_typography {
            c = c.pdf_keep_typography(v);
        }
        if let Some(v) = self.pdf_report_repairs {
            c = c.pdf_report_repairs(v);
        }
        if let Some(v) = self.ocr_textline_orientation {
            c = c.ocr_textline_orientation(v);
        }
        if let Some(s) = self.images_scale {
            c = c.images_scale(s);
        }
        if let Some(v) = self.page_images {
            c = c.generate_page_images(v);
        }
        if let Some(v) = self.use_embedded_picture_images {
            c = c.use_embedded_picture_images(v);
        }
        if let Some(v) = self.do_picture_classification {
            c = c.do_picture_classification(v);
        }
        if let Some(v) = self.do_code_enrichment {
            c = c.do_code_enrichment(v);
        }
        if let Some(v) = self.do_formula_enrichment {
            c = c.do_formula_enrichment(v);
        }
        if let Some(v) = self.do_picture_ocr {
            c = c.do_picture_ocr(v);
        }
        if self.picture_ocr_classes.is_some() {
            c = c.picture_ocr_classes(self.picture_ocr_classes()?);
        }
        if let Some(v) = self.picture_ocr_min_side {
            c = c.picture_ocr_min_side(v);
        }
        if let Some(s) = self.picture_ocr_scale {
            c = c.picture_ocr_scale(s);
        }
        if let Some(v) = self.keep_picture_images {
            c = c.keep_picture_images(v);
        }
        #[cfg(feature = "vlm")]
        if let Some(o) = self.picture_description_options()? {
            c = c.picture_description(o);
        }
        if self.redact_pii.is_some() {
            c = c.redact_pii_opt(self.redaction()?);
        }
        Ok(c)
    }

    // --- typed readers, for the surfaces that drive a warm `Pipeline`
    //     themselves and need the parsed values rather than the builder -----

    /// `pages` parsed ([`crate::parse_page_range`]); `None` when unset.
    pub fn page_range(&self) -> Result<Option<(usize, usize)>, OptionsError> {
        self.pages
            .as_deref()
            .map(|p| {
                crate::parse_page_range(p)
                    .map_err(|e| OptionsError::new("pages", format!("pages: {e}")))
            })
            .transpose()
    }

    /// `document_timeout` as a duration; `None` when unset.
    pub fn document_timeout(&self) -> Result<Option<std::time::Duration>, OptionsError> {
        match self.document_timeout {
            Some(s) if s.is_finite() && s > 0.0 => Ok(Some(std::time::Duration::from_secs_f64(s))),
            Some(s) => Err(OptionsError::new(
                "document_timeout",
                format!("document_timeout must be a positive number of seconds, got {s}"),
            )),
            None => Ok(None),
        }
    }

    /// The pipeline the options select.
    pub fn pipeline(&self) -> Result<PipelineKind, OptionsError> {
        match self.pipeline.as_deref().map(str::trim) {
            None | Some("standard") => Ok(PipelineKind::Standard),
            Some("vlm") => Ok(PipelineKind::Vlm),
            Some(other) => Err(OptionsError::new(
                "pipeline",
                format!("unknown pipeline {other:?} (expected: standard, vlm)"),
            )),
        }
    }

    /// The enrichment passes (#423) — unset and `false` both mean off.
    pub fn enrichments(&self) -> crate::EnrichmentOptions {
        crate::EnrichmentOptions {
            picture_classification: self.do_picture_classification.unwrap_or(false),
            code: self.do_code_enrichment.unwrap_or(false),
            formula: self.do_formula_enrichment.unwrap_or(false),
        }
    }

    /// `ocr_engine` parsed; `None` when unset (the process default applies).
    #[cfg(feature = "pdf")]
    pub fn ocr_engine(&self) -> Result<Option<crate::OcrEngine>, OptionsError> {
        self.ocr_engine
            .as_deref()
            .map(|v| {
                crate::OcrEngine::parse(v).ok_or_else(|| {
                    OptionsError::new(
                        "ocr_engine",
                        format!("ocr_engine {v:?} is not {}", crate::OcrEngine::ACCEPTED),
                    )
                })
            })
            .transpose()
    }

    /// The engine `ocr_lang` is read against: `ocr_engine`, else the process
    /// default (`DOCLING_RS_OCR_ENGINE`, else PP-OCR).
    #[cfg(feature = "pdf")]
    pub fn effective_ocr_engine(&self) -> Result<crate::OcrEngine, OptionsError> {
        Ok(self
            .ocr_engine()?
            .unwrap_or_else(crate::OcrEngine::from_env))
    }

    /// `ocr_lang` validated against the engine (#460) and, under PP-OCR, the
    /// recognition model it selects; `None` when unset or under Tesseract
    /// (whose language list is [`Self::tesseract_lang`]).
    #[cfg(feature = "pdf")]
    pub fn ocr_lang(&self) -> Result<Option<crate::OcrLang>, OptionsError> {
        let Some(v) = self.ocr_lang.as_deref() else {
            return Ok(None);
        };
        let engine = self.effective_ocr_engine()?;
        engine.validate_lang(v).map_err(|e| {
            OptionsError::new(
                "ocr_lang",
                if e.contains("ocr_lang") {
                    e
                } else {
                    format!("ocr_lang: {e}")
                },
            )
        })?;
        Ok(match engine {
            crate::OcrEngine::PpOcr => crate::OcrLang::parse(v),
            crate::OcrEngine::Tesseract => None,
        })
    }

    /// Tesseract's `-l` argument from `ocr_lang` (#460); `None` under PP-OCR
    /// or without a language.
    #[cfg(feature = "pdf")]
    pub fn tesseract_lang(&self) -> Result<Option<String>, OptionsError> {
        let Some(v) = self.ocr_lang.as_deref() else {
            return Ok(None);
        };
        match self.effective_ocr_engine()? {
            crate::OcrEngine::Tesseract => crate::tesseract_lang_arg(v)
                .map(Some)
                .map_err(|e| OptionsError::new("ocr_lang", e)),
            crate::OcrEngine::PpOcr => Ok(None),
        }
    }

    /// Whether OCR is off: `no_ocr` or its pre-2.0 name `skip_ocr` (#611).
    /// What a surface that builds the PDF pipeline itself reads.
    pub fn ocr_disabled(&self) -> bool {
        self.no_ocr.unwrap_or(false) || self.skip_ocr.unwrap_or(false)
    }

    /// `ocr_mode` parsed (#254); `None` when unset.
    #[cfg(feature = "pdf")]
    pub fn ocr_mode(&self) -> Result<Option<crate::OcrMode>, OptionsError> {
        self.ocr_mode
            .as_deref()
            .map(|v| {
                crate::OcrMode::parse(v).ok_or_else(|| {
                    OptionsError::new(
                        "ocr_mode",
                        format!(
                            "ocr_mode {v:?} is not \
                             default|full_page|layout_regions|pdf_aware_layout_regions"
                        ),
                    )
                })
            })
            .transpose()
    }

    /// `pdf_word_gap` parsed (#921); `None` when unset.
    #[cfg(any(feature = "pdf", feature = "pdf-text"))]
    pub fn pdf_word_gap(&self) -> Result<Option<crate::PdfWordGap>, OptionsError> {
        self.pdf_word_gap
            .as_deref()
            .map(|v| crate::PdfWordGap::parse(v).map_err(|e| OptionsError::new("pdf_word_gap", e)))
            .transpose()
    }

    /// Picture-crop scale and page images for the pipeline (#519/#520).
    #[cfg(feature = "pdf")]
    pub fn image_output(&self) -> crate::ImageOutput {
        crate::ImageOutput {
            scale: self.images_scale,
            page_images: self.page_images.unwrap_or(false),
            embedded_pictures: self.use_embedded_picture_images.unwrap_or(false),
        }
    }

    /// The remote-VLM configuration (#77) when `pipeline` is `vlm`, resolved
    /// against the `DOCLING_RS_VLM_*` environment the way every surface did
    /// on its own before: endpoint and model from the options else the env
    /// (blank counts as unset), the prompt and API key likewise, `pages`
    /// composing like it does with the ML pipeline. `None` under the standard
    /// pipeline — the `vlm_*` options are then ignored, not rejected (the
    /// bindings' long-standing contract).
    #[cfg(feature = "vlm")]
    pub fn vlm_options(&self) -> Result<Option<crate::vlm::VlmOptions>, OptionsError> {
        if self.pipeline()? != PipelineKind::Vlm {
            return Ok(None);
        }
        let set = |s: &Option<String>| s.clone().filter(|v| !v.trim().is_empty());
        let mut v = crate::vlm::VlmOptions::resolve(set(&self.vlm_endpoint), set(&self.vlm_model))
            .map_err(|e| {
                // The library's message names the CLI flags; the wire spelling
                // is this surface's (the CLI swaps its flags back in).
                let msg = match e {
                    crate::ConversionError::Parse(m) => m,
                    other => other.to_string(),
                }
                .replace("pass --vlm-endpoint", "set vlm_endpoint")
                .replace("pass --vlm-model", "set vlm_model");
                let field = if msg.contains("vlm_model") {
                    "vlm_model"
                } else {
                    "vlm_endpoint"
                };
                OptionsError::new(field, msg)
            })?;
        if let Some(p) = set(&self.vlm_prompt) {
            v.prompt = Some(p);
        }
        if let Some(k) = set(&self.vlm_api_key) {
            v.api_key = Some(k);
        }
        match self.vlm_max_tokens {
            // 0 would have every page come back empty and surface as a model
            // error — a message pointing at the model, not at the option.
            Some(0) => {
                return Err(OptionsError::new(
                    "vlm_max_tokens",
                    "vlm_max_tokens must be a positive integer, got 0",
                ))
            }
            Some(n) => v.max_tokens = n,
            None => {}
        }
        v.page_range = self.page_range()?;
        Ok(Some(v))
    }
}

impl DocumentConverter {
    /// A converter configured from [`ConvertOptions`] — validated and applied
    /// onto the defaults ([`ConvertOptions::apply`] on [`Self::new`]).
    pub fn from_options(options: &ConvertOptions) -> Result<Self, OptionsError> {
        options.apply(Self::new())
    }
}

/// The structured form a drawing's diagram is extracted to (#858,
/// `extract_diagrams`): Mermaid, the default, or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagramFormat {
    /// A `flowchart TD` code item after the drawing's picture.
    #[default]
    Mermaid,
    /// No extraction: the picture alone.
    None,
}

impl DiagramFormat {
    /// The spellings `extract_diagrams` accepts.
    pub const ACCEPTED: &'static str = "mermaid, none";

    pub fn as_str(self) -> &'static str {
        match self {
            DiagramFormat::Mermaid => "mermaid",
            DiagramFormat::None => "none",
        }
    }
}

impl FromStr for DiagramFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mermaid" => Ok(DiagramFormat::Mermaid),
            "none" | "off" => Ok(DiagramFormat::None),
            other => Err(format!(
                "extract_diagrams must be one of {}; got {other:?}",
                DiagramFormat::ACCEPTED
            )),
        }
    }
}

impl fmt::Display for DiagramFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spelling table and the struct describe the same fields, both ways
    /// — the guard that makes "add a field" also mean "add its row".
    #[test]
    fn options_table_matches_the_struct() {
        let fields = ConvertOptions::field_names();
        for o in OPTIONS {
            assert!(
                fields.iter().any(|f| f == o.name),
                "no field for row {}",
                o.name
            );
            for a in o.aliases {
                assert!(!fields.iter().any(|f| f == a), "alias {a} is also a field");
            }
        }
        for f in &fields {
            assert!(
                OPTIONS.iter().any(|o| o.name == f),
                "no OPTIONS row for field {f}"
            );
        }
        assert_eq!(cli_flag("ocr_scale"), "--ocr-scale");
        assert_eq!(cli_flag("do_code_enrichment"), "--enrich-code");
    }

    /// Text values read as the option's type (#679's follow-up: the MCP
    /// form); a typed value for a text option as its text; the rest as is.
    #[test]
    fn text_values_read_as_the_options_type() {
        let mut o = ConvertOptions::default();
        assert!(o.set_text("strict", "true").unwrap());
        assert!(o.set_text("compact_tables", "YES").unwrap());
        assert!(o.set_text("ocr_scale", "1.5").unwrap());
        assert!(o.set_text("video_frames", "4").unwrap());
        assert!(o.set_text("pages", "5").unwrap());
        assert!(o.set_text("pdf_password", "0").unwrap(), "an alias");
        assert!(!o.set_text("not_an_option", "x").unwrap());
        assert_eq!(o.strict, Some(true));
        assert_eq!(o.compact_tables, Some(true));
        assert_eq!(o.ocr_scale, Some(1.5));
        assert_eq!(o.video_frames, Some(4));
        assert_eq!(o.pages.as_deref(), Some("5"));
        assert_eq!(o.password.as_deref(), Some("0"));
        let e = o.set_text("strict", "maybe").unwrap_err();
        assert_eq!(e.field, "strict");
        assert_eq!(
            e.cli_message(),
            "--strict: cannot read \"maybe\" as that option's value"
        );

        let o = ConvertOptions::from_loose_json(
            serde_json::json!({
                "force_full_page_ocr": "true",
                "ocr_scale": "2",
                "pages": 3,
                "strict": 1,
                "text_layer_only": false,
                "ocr_lang": null,
                "redact_kinds": "email, phone"
            })
            .as_object()
            .unwrap()
            .clone(),
        )
        .unwrap();
        assert_eq!(o.force_full_page_ocr, Some(true));
        assert_eq!(o.ocr_scale, Some(2.0));
        assert_eq!(o.pages.as_deref(), Some("3"));
        assert_eq!(o.strict, Some(true));
        assert_eq!(o.text_layer_only, Some(TextLayerOnly::Off));
        assert_eq!(o.ocr_lang, None);
        assert_eq!(o.redact_kinds.as_deref(), Some("email, phone"));
        let bad = |v: serde_json::Value| {
            ConvertOptions::from_loose_json(v.as_object().unwrap().clone())
                .unwrap_err()
                .to_string()
        };
        assert!(bad(serde_json::json!({"strict": "maybe"})).contains("strict: cannot read"));
        assert!(bad(serde_json::json!({"strict": [1]})).contains("strict: invalid type"));
        assert!(bad(serde_json::json!({"strictt": true})).contains("unknown field `strictt`"));
    }

    #[test]
    fn aliases_deserialize_and_unknown_keys_are_reported() {
        let o: ConvertOptions = serde_json::from_str(
            r#"{"md_page_break_placeholder": "<!-- p -->", "generate_page_images": true}"#,
        )
        .unwrap();
        assert_eq!(o.page_break_placeholder.as_deref(), Some("<!-- p -->"));
        assert_eq!(o.page_images, Some(true));
        assert_eq!(
            ConvertOptions::unknown_keys(
                ["to", "strict", "md_page_break_placeholder", "strictness"],
                &["to"]
            ),
            vec!["strictness".to_string()]
        );
    }

    /// #1007: `use_embedded_picture_images` reaches the pipeline's image
    /// output, off unless asked for.
    #[cfg(feature = "pdf")]
    #[test]
    fn use_embedded_picture_images_reaches_the_image_output() {
        let on: ConvertOptions =
            serde_json::from_str(r#"{"use_embedded_picture_images": true}"#).unwrap();
        assert!(on.image_output().embedded_pictures);
        assert!(!ConvertOptions::default().image_output().embedded_pictures);
    }

    /// #647: `video_frames` reads an integer, `"all"`, a digit string (the
    /// text surfaces) and a 32-bit-max count as the npm addon sends it;
    /// the two bounded tunables are range-checked.
    #[test]
    fn video_frames_spellings_and_video_ranges() {
        let parse = |json: &str| serde_json::from_str::<ConvertOptions>(json).unwrap();
        assert_eq!(parse(r#"{"video_frames": 3}"#).video_frames, Some(3));
        assert_eq!(parse(r#"{"video_frames": "12"}"#).video_frames, Some(12));
        assert_eq!(
            parse(r#"{"video_frames": "all"}"#).video_frames,
            Some(crate::ALL_VIDEO_FRAMES)
        );
        assert_eq!(
            parse(r#"{"video_frames": 4294967295}"#).video_frames,
            Some(crate::ALL_VIDEO_FRAMES)
        );
        assert_eq!(parse("{}").video_frames, None);
        assert!(serde_json::from_str::<ConvertOptions>(r#"{"video_frames": "some"}"#).is_err());
        // "all" survives a merge round trip (serialized as the u64 max).
        let merged = ConvertOptions {
            video_frames: Some(crate::ALL_VIDEO_FRAMES),
            ..Default::default()
        }
        .merge_over(ConvertOptions::default());
        assert_eq!(merged.video_frames, Some(crate::ALL_VIDEO_FRAMES));

        let ok = ConvertOptions {
            video_scene_threshold: Some(0.6),
            video_frame_max_side: Some(640),
            video_frame_dedupe: Some(5),
            ..Default::default()
        };
        ok.validate().unwrap();
        let bad = ConvertOptions {
            video_scene_threshold: Some(1.5),
            ..Default::default()
        };
        assert_eq!(bad.validate().unwrap_err().field, "video_scene_threshold");
        let bad = ConvertOptions {
            video_frame_dedupe: Some(65),
            ..Default::default()
        };
        assert_eq!(bad.validate().unwrap_err().field, "video_frame_dedupe");
    }

    #[test]
    fn merge_prefers_the_overlay_and_keeps_the_rest() {
        let base = ConvertOptions {
            strict: Some(true),
            ocr_scale: Some(2.0),
            pages: Some("1-3".into()),
            ..Default::default()
        };
        let over = ConvertOptions {
            strict: Some(false),
            video_frames: Some(0),
            ..Default::default()
        };
        let merged = over.merge_over(base);
        assert_eq!(merged.strict, Some(false));
        assert_eq!(merged.ocr_scale, Some(2.0));
        assert_eq!(merged.pages.as_deref(), Some("1-3"));
        assert_eq!(merged.video_frames, Some(0));
    }

    /// #921: the text layer's word-gap rule — a mode or a factor in (0, 4].
    #[cfg(any(feature = "pdf", feature = "pdf-text"))]
    #[test]
    fn pdf_word_gap_validation() {
        let bad = |o: ConvertOptions| o.validate().unwrap_err();
        let e = bad(ConvertOptions {
            pdf_word_gap: Some("wide".into()),
            ..Default::default()
        });
        assert_eq!(e.field, "pdf_word_gap");
        assert_eq!(
            e.cli_message(),
            "--pdf-word-gap must be legacy, adaptive, or a factor in (0, 4], got \"wide\""
        );
        for ok in ["legacy", "Adaptive", "0.25", "4"] {
            let o = ConvertOptions {
                pdf_word_gap: Some(ok.into()),
                ..Default::default()
            };
            assert!(o.validate().is_ok(), "{ok}");
        }
        for out in ["0", "-1", "4.5", "nan"] {
            let o = ConvertOptions {
                pdf_word_gap: Some(out.into()),
                ..Default::default()
            };
            assert!(o.validate().is_err(), "{out}");
        }
        // A JSON number reads as its text (MCP forms, the C ABI).
        let mut o = ConvertOptions::default();
        assert!(o.set_text("pdf_word_gap", "0.2").unwrap());
        assert_eq!(
            o.pdf_word_gap().unwrap(),
            Some(crate::PdfWordGap::Factor(0.2))
        );
        let o = ConvertOptions::from_loose_json(
            serde_json::json!({"pdf_word_gap": 0.3})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
        assert_eq!(
            o.pdf_word_gap().unwrap(),
            Some(crate::PdfWordGap::Factor(0.3))
        );
    }

    #[test]
    fn validation_rules_and_cli_spelling() {
        let bad = |o: ConvertOptions| o.validate().unwrap_err();
        let e = bad(ConvertOptions {
            ocr_scale: Some(0.0),
            ..Default::default()
        });
        assert_eq!(e.field, "ocr_scale");
        assert_eq!(e.to_string(), "ocr_scale must be a positive number, got 0");
        assert_eq!(
            e.cli_message(),
            "--ocr-scale must be a positive number, got 0"
        );
        // #666: the picture OCR's own scale, the same rule.
        let e = bad(ConvertOptions {
            picture_ocr_scale: Some(-1.0),
            ..Default::default()
        });
        assert_eq!(
            e.cli_message(),
            "--picture-ocr-scale must be a positive number, got -1"
        );

        let e = bad(ConvertOptions {
            images_scale: Some(9.0),
            ..Default::default()
        });
        assert_eq!(e.field, "images_scale");
        let e = bad(ConvertOptions {
            document_timeout: Some(-1.0),
            ..Default::default()
        });
        assert!(e.message.contains("document_timeout must be a positive"));
        let e = bad(ConvertOptions {
            pages: Some("5-3".into()),
            ..Default::default()
        });
        assert_eq!(e.field, "pages");
        assert!(
            e.cli_message().starts_with("--pages: "),
            "{}",
            e.cli_message()
        );
        let e = bad(ConvertOptions {
            pipeline: Some("magic".into()),
            ..Default::default()
        });
        assert!(e.message.contains("unknown pipeline"), "{e}");
        let e = bad(ConvertOptions {
            vlm_max_tokens: Some(0),
            ..Default::default()
        });
        assert_eq!(
            e.cli_message(),
            "--vlm-max-tokens must be a positive integer, got 0"
        );
        assert!(ConvertOptions::default().validate().is_ok());
    }

    #[cfg(feature = "pdf")]
    #[test]
    fn ocr_options_are_validated_against_the_engine() {
        let e = ConvertOptions {
            ocr_engine: Some("easyocr".into()),
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        assert_eq!(e.field, "ocr_engine");
        assert!(
            e.cli_message().starts_with("--ocr-engine \"easyocr\""),
            "{}",
            e.cli_message()
        );
        // `deu` is a language to Tesseract only.
        let e = ConvertOptions {
            ocr_engine: Some("ppocr".into()),
            ocr_lang: Some("deu".into()),
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        assert_eq!(e.field, "ocr_lang");
        assert!(e.message.contains("ocr_lang"), "{e}");
        let tess = ConvertOptions {
            ocr_engine: Some("tesseract".into()),
            ocr_lang: Some("deu+fra".into()),
            ..Default::default()
        };
        assert!(tess.validate().is_ok());
        assert_eq!(tess.ocr_lang().unwrap(), None);
        assert_eq!(tess.tesseract_lang().unwrap().as_deref(), Some("deu+fra"));
        let e = ConvertOptions {
            ocr_mode: Some("sideways".into()),
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        assert_eq!(e.field, "ocr_mode");
    }

    #[test]
    fn apply_sets_only_what_is_given() {
        // `strict` and the page-break text ride on the finished document, so
        // they are observable without a conversion.
        let base = DocumentConverter::new().strict(true);
        let mut doc = docling_core::DoclingDocument::new("t");
        ConvertOptions::default()
            .apply(base.clone())
            .unwrap()
            .finish_document(&mut doc);
        assert!(doc.strict_markdown, "unset keeps the base's strict");
        let mut doc = docling_core::DoclingDocument::new("t");
        ConvertOptions {
            strict: Some(false),
            page_break_placeholder: Some("<!-- p -->".into()),
            ..Default::default()
        }
        .apply(base)
        .unwrap()
        .finish_document(&mut doc);
        assert!(!doc.strict_markdown);
        assert_eq!(doc.page_break_placeholder.as_deref(), Some("<!-- p -->"));
        // A rejected option never yields a converter.
        assert!(DocumentConverter::from_options(&ConvertOptions {
            pages: Some("0-2".into()),
            ..Default::default()
        })
        .is_err());
    }

    #[cfg(feature = "vlm")]
    #[test]
    fn vlm_options_follow_the_pipeline_switch() {
        // Standard: stray vlm_* options are ignored, nothing resolves.
        let stray = ConvertOptions {
            vlm_endpoint: Some("http://127.0.0.1:9/v1".into()),
            ..Default::default()
        };
        assert!(matches!(stray.vlm_options(), Ok(None)));
        let full = ConvertOptions {
            pipeline: Some("vlm".into()),
            vlm_endpoint: Some("http://example.com/v1".into()),
            vlm_model: Some("m".into()),
            vlm_api_key: Some("sk-test".into()),
            vlm_prompt: Some("Read the page.".into()),
            vlm_max_tokens: Some(512),
            pages: Some("2-5".into()),
            ..Default::default()
        };
        let v = full.vlm_options().unwrap().expect("resolves");
        assert_eq!(v.endpoint, "http://example.com/v1");
        assert_eq!(v.model, "m");
        assert_eq!(v.api_key.as_deref(), Some("sk-test"));
        assert_eq!(v.prompt.as_deref(), Some("Read the page."));
        assert_eq!(v.max_tokens, 512);
        assert_eq!(v.page_range, Some((2, 5)));
        let e = ConvertOptions {
            vlm_max_tokens: Some(0),
            ..full.clone()
        }
        .vlm_options()
        .unwrap_err();
        assert_eq!(e.field, "vlm_max_tokens");
        // No model anywhere: the message names the wire option, the CLI the
        // flag. (`DOCLING_RS_VLM_MODEL` unset in the test environment.)
        if std::env::var_os("DOCLING_RS_VLM_MODEL").is_none() {
            let e = ConvertOptions {
                vlm_model: None,
                ..full
            }
            .vlm_options()
            .unwrap_err();
            assert_eq!(e.field, "vlm_model");
            assert!(e.message.contains("set vlm_model"), "{e}");
            assert!(
                e.cli_message().contains("--vlm-model"),
                "{}",
                e.cli_message()
            );
            assert!(e.message.contains("DOCLING_RS_VLM_MODEL"), "{e}");
        }
    }

    /// #611: `no_ocr` is docling's `do_ocr=False` and `skip_ocr` its pre-2.0
    /// name — either one turns OCR off, and a body carrying both (a pre-2.0
    /// client sending both switches) parses instead of failing on a
    /// duplicate field; `text_layer_only` and `password` — under docling's
    /// `pdf_password` too (#625) — are read.
    #[test]
    fn ocr_spellings_and_password_parse() {
        let both: ConvertOptions = serde_json::from_str(
            r#"{"no_ocr": false, "skip_ocr": true, "text_layer_only": true,
                "pdf_password": "1234"}"#,
        )
        .expect("both spellings parse");
        assert!(both.ocr_disabled());
        assert_eq!(both.text_layer_only, Some(TextLayerOnly::On));
        assert_eq!(both.password.as_deref(), Some("1234"));
        let neutral: ConvertOptions = serde_json::from_str(r#"{"password": "x"}"#).unwrap();
        assert_eq!(neutral.password.as_deref(), Some("x"));
        let new: ConvertOptions = serde_json::from_str(r#"{"no_ocr": true}"#).unwrap();
        assert!(new.ocr_disabled());
        assert!(!ConvertOptions::default().ocr_disabled());
        // Merged like every option: a later layer's spelling adds to the base.
        let merged = new.merge_over(both);
        assert!(merged.ocr_disabled());
        assert_eq!(merged.password.as_deref(), Some("1234"));
    }

    /// `text_layer_only` on the wire: the bool it always was, the string
    /// `auto`, and the text surfaces' spellings — through the typed JSON
    /// body, the loose (query / multipart) reading and a merge round trip.
    #[test]
    fn text_layer_only_reads_bools_and_auto() {
        let typed = |v: serde_json::Value| -> Option<TextLayerOnly> {
            serde_json::from_value::<ConvertOptions>(serde_json::json!({ "text_layer_only": v }))
                .unwrap()
                .text_layer_only
        };
        assert_eq!(typed(serde_json::json!(true)), Some(TextLayerOnly::On));
        assert_eq!(typed(serde_json::json!(false)), Some(TextLayerOnly::Off));
        assert_eq!(typed(serde_json::json!("auto")), Some(TextLayerOnly::Auto));
        assert_eq!(typed(serde_json::json!("AUTO")), Some(TextLayerOnly::Auto));
        assert_eq!(typed(serde_json::json!(null)), None);
        let err = serde_json::from_value::<ConvertOptions>(
            serde_json::json!({ "text_layer_only": "sometimes" }),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("true, false, auto"), "{err}");

        let loose = |v: serde_json::Value| {
            ConvertOptions::from_loose_json(
                serde_json::json!({ "text_layer_only": v })
                    .as_object()
                    .unwrap()
                    .clone(),
            )
        };
        assert_eq!(
            loose(serde_json::json!("auto")).unwrap().text_layer_only,
            Some(TextLayerOnly::Auto)
        );
        assert_eq!(
            loose(serde_json::json!("yes")).unwrap().text_layer_only,
            Some(TextLayerOnly::On)
        );
        assert_eq!(
            loose(serde_json::json!(1)).unwrap().text_layer_only,
            Some(TextLayerOnly::On)
        );
        assert_eq!(
            loose(serde_json::json!("off")).unwrap().text_layer_only,
            Some(TextLayerOnly::Off)
        );
        assert!(loose(serde_json::json!("maybe")).is_err());

        // The merge serializes and reads back: `auto` survives it, and the
        // bools stay bools on the wire.
        let auto = ConvertOptions {
            text_layer_only: Some(TextLayerOnly::Auto),
            ..Default::default()
        };
        let merged = merge_options(auto.clone(), ConvertOptions::default());
        assert_eq!(merged.text_layer_only, Some(TextLayerOnly::Auto));
        assert_eq!(
            serde_json::to_value(&auto).unwrap()["text_layer_only"],
            serde_json::json!("auto")
        );
        let on = ConvertOptions {
            text_layer_only: Some(TextLayerOnly::On),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&on).unwrap()["text_layer_only"],
            serde_json::json!(true)
        );
        assert_eq!("auto".parse::<TextLayerOnly>(), Ok(TextLayerOnly::Auto));
        assert_eq!(TextLayerOnly::from(true), TextLayerOnly::On);
        assert_eq!(TextLayerOnly::Auto.to_string(), "auto");
    }
}
