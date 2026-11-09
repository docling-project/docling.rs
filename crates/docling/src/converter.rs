//! The top-level `DocumentConverter`.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::backend::{
    is_deepseek_markdown, AbwBackend, AsciiDocBackend, CalendarBackend, CsvBackend,
    DeclarativeBackend, DeepSeekBackend, DocBackend, DoclingJsonBackend, DocxBackend,
    EbcdicBackend, EmailBackend, EpubBackend, InterchangeBackend, JatsBackend, LatexBackend,
    LotusBackend, MarkdownBackend, MhtmlBackend, PptBackend, PptxBackend, QuattroBackend,
    RtfBackend, StarOffice5Backend, UsptoBackend, VisioBackend, WebVttBackend, WpdBackend,
    WpsBackend, XlsBackend, XlsxBackend,
};

/// Drop the embedded image bytes from every picture — the flat nodes and the
/// item tree alike — for [`DocumentConverter::keep_picture_images`]`(false)`.
/// The picture items stay (caption, OCR text, classification, geometry);
/// only their payload goes, so the JSON writes no `image` and Markdown's
/// embedded mode prints the placeholder.
fn strip_picture_images(document: &mut docling_core::DoclingDocument) {
    strip_picture_images_in(&mut document.nodes);
    if let Some(tree) = document.tree.as_mut() {
        for item in tree.items.iter_mut() {
            if let docling_core::tree::TreeKind::Picture { image, .. } = &mut item.kind {
                *image = None;
            }
        }
    }
}

/// [`strip_picture_images`] over a page batch: what the streaming PDF path
/// applies to each batch before it is serialized (#896), so the stream
/// matches the buffered Markdown.
pub(crate) fn strip_picture_images_in(nodes: &mut [docling_core::Node]) {
    docling_core::for_each_picture_in_mut(nodes, &mut |node| {
        if let docling_core::Node::Picture { image, .. } = node {
            *image = None;
        }
    });
}

/// The picture-OCR reader (#645): the OCR models behind the size floor and
/// the classifier filter, with a by-bytes cache so the same image embedded
/// twice is read once. Built by [`DocumentConverter::picture_reader`].
#[cfg(feature = "pdf")]
struct PictureReader {
    pipeline: docling_pdf::Pipeline,
    min_side: u32,
    classes: Vec<String>,
    cache: std::collections::HashMap<u64, Option<docling_core::PictureDescription>>,
}

#[cfg(feature = "pdf")]
impl PictureReader {
    /// The text read off `img`, as the picture's description annotation;
    /// `None` when the picture is skipped (too small, filtered out by its
    /// class) or carries no text.
    fn read(
        &mut self,
        img: &docling_core::PictureImage,
    ) -> Option<docling_core::PictureDescription> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        img.data.hash(&mut h);
        let key = h.finish();
        if let Some(hit) = self.cache.get(&key) {
            return hit.clone();
        }
        let text = self.read_uncached(img);
        self.cache.insert(key, text.clone());
        text
    }

    fn read_uncached(
        &mut self,
        img: &docling_core::PictureImage,
    ) -> Option<docling_core::PictureDescription> {
        // The declared size is a hint; the decoded one is the truth
        // (a 16 × 16 icon is skipped before any inference).
        if img.width.min(img.height) < self.min_side && img.width * img.height > 0 {
            return None;
        }
        let decoded = match self.pipeline.decode_picture(&img.data) {
            Ok(d) => d,
            Err(e) => {
                docling_core::debug_log!("docling: picture OCR: undecodable image: {e}");
                return None;
            }
        };
        if decoded.width().min(decoded.height()) < self.min_side {
            return None;
        }
        // The loader warned once when the classifier is missing; an
        // unclassifiable picture is read rather than silently skipped.
        if !self.classes.is_empty() {
            if let Some(preds) = self.pipeline.classify_picture(&decoded) {
                let top = preds.first().map(|p| p.class_name.as_str());
                if !top.is_some_and(|t| self.classes.iter().any(|c| c == t)) {
                    return None;
                }
            }
        }
        match self.pipeline.ocr_picture(&decoded) {
            Ok(Some(t)) => Some(docling_core::PictureDescription {
                text: t.text,
                provenance: t.provenance.to_string(),
            }),
            Ok(None) => None,
            Err(e) => {
                eprintln!("warning: picture OCR failed: {e}");
                None
            }
        }
    }
}

/// Without the `pdf` feature there is no reader; the type exists so the
/// callers compile unchanged.
#[cfg(not(feature = "pdf"))]
struct PictureReader;

#[cfg(not(feature = "pdf"))]
impl PictureReader {
    fn read(
        &mut self,
        _img: &docling_core::PictureImage,
    ) -> Option<docling_core::PictureDescription> {
        None
    }
}

/// The image type by its magic bytes, for the four the vision endpoints
/// take (OpenAI's `image_url` set: PNG, JPEG, GIF, WebP) plus BMP and TIFF,
/// which [`endpoint_image`] re-encodes.
#[cfg(feature = "vlm")]
fn image_mimetype(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        [b'B', b'M', ..] => Some("image/bmp"),
        [b'I', b'I', 0x2A, 0x00, ..] | [b'M', b'M', 0x00, 0x2A, ..] => Some("image/tiff"),
        _ => None,
    }
}

/// `img` as the endpoint takes it: PNG/JPEG/GIF/WebP bytes as they are
/// (the type read off the bytes, not the declared one — an office file's
/// `image/jpg` or a mislabelled PNG), BMP/TIFF re-encoded as PNG, anything
/// else (EMF/WMF, SVG, …) `None`.
#[cfg(feature = "vlm")]
fn endpoint_image(
    img: &docling_core::PictureImage,
) -> Option<(&'static str, std::borrow::Cow<'_, [u8]>)> {
    match image_mimetype(&img.data)? {
        m @ ("image/png" | "image/jpeg" | "image/gif" | "image/webp") => {
            Some((m, std::borrow::Cow::Borrowed(&img.data)))
        }
        _ => {
            let decoded = image::load_from_memory(&img.data).ok()?;
            let mut buf = std::io::Cursor::new(Vec::new());
            decoded.write_to(&mut buf, image::ImageFormat::Png).ok()?;
            Some(("image/png", std::borrow::Cow::Owned(buf.into_inner())))
        }
    }
}

/// Whether `text` begins with an XML prolog — an `<?xml …?>` declaration or a
/// non-HTML `<!DOCTYPE …>`. Used to route XML documents that arrived with a
/// text/Markdown extension (e.g. a JATS article saved as `.txt`) to the XML
/// backends. An HTML5 `<!DOCTYPE html>` is deliberately excluded.
fn looks_like_xml(text: &str) -> bool {
    let head = text.trim_start();
    if head.starts_with("<?xml") {
        return true;
    }
    if let Some(rest) = head.get(..9) {
        if rest.eq_ignore_ascii_case("<!doctype") {
            return !head[9..]
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("html");
        }
    }
    false
}

/// Whether an XML document is a JATS article: a JATS DOCTYPE, or an
/// `<article` root element (JATS files in the wild often carry no DOCTYPE).
fn looks_like_jats(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4000)]);
    has_jats_doctype(&head) || head.contains("<article")
}

/// Pick the concrete XML backend for a generic `.xml` source by sniffing its
/// DOCTYPE / root element (the first part of the file).
/// docling's `_guess_from_content` JATS rule for an `application/xml` input:
/// the `<!DOCTYPE …>` declaration names a JATS DTD.
fn has_jats_doctype(text: &str) -> bool {
    let Some(start) = text.find("<!DOCTYPE ") else {
        return false;
    };
    let Some(len) = text[start..].find('>') else {
        return false;
    };
    let doctype = &text[start..start + len];
    doctype.contains("JATS-journalpublishing") || doctype.contains("JATS-archive")
}

fn sniff_xml(bytes: &[u8]) -> InputFormat {
    // Lossy over the raw head (docling#4038, 2.122): the window can cut a
    // well-formed file mid-codepoint — a fixed-offset `&str` slice would
    // panic on that char boundary — and an XML document may legitimately
    // declare a non-UTF-8 encoding. Every marker matched below is ASCII, so
    // replacement characters cannot change the outcome.
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4000)]);
    let head = head.as_ref();
    // Case-insensitive: USPTO DOCTYPE/root casing varies in the wild (docling
    // PR #3801 — Grant Full Text v2.5 files were missed on casing).
    let lower = head.to_ascii_lowercase();
    if lower.contains("us-patent")
        || lower.contains("patent-application-publication")
        || lower.contains("patdoc")
        || lower.contains("<pap-v1")
    {
        InputFormat::XmlUspto
    } else if head.contains("<doclang") {
        // A bare DocLang document saved as `.xml` (docling names them
        // `*.dclg.xml`, whose final extension is plain `xml`).
        InputFormat::XmlDoclang
    } else if crate::backend::feed::looks_like_feed(head) {
        // An RSS / Atom / RDF feed (#678), by its root element.
        InputFormat::Feed
    } else if crate::backend::xbrl::looks_like_xbrl(head) {
        InputFormat::XmlXbrl
    } else if crate::backend::ris::looks_like_ris(head) {
        // Austrian RIS legal XML (#902), by its `<risdok>` root.
        InputFormat::XmlRis
    } else {
        InputFormat::XmlJats
    }
}
use crate::error::ConversionError;
use crate::format::InputFormat;
use crate::options::TextLayerOnly;
use crate::progress::{
    Callbacks, EnrichmentStep, ProgressCallback, ProgressPrinter, ProgressReporter,
};
use crate::result::{ConversionResult, ConversionStatus};
use crate::source::SourceDocument;
#[cfg(feature = "pdf")]
use crate::stream::MarkdownStream;
#[cfg(feature = "pdf")]
use docling_core::{ImageMode, MarkdownExportOptions};

/// Routes a [`SourceDocument`] to the backend for its format and returns a
/// [`ConversionResult`].
///
/// The Rust analogue of `docling.document_converter.DocumentConverter`. In
/// Phase 0 the format→backend dispatch is a direct match; the Python notion of
/// per-format `FormatOption` (backend + pipeline + options) arrives with the
/// PDF/ML pipeline in a later phase.
#[derive(Debug, Clone)]
pub struct DocumentConverter {
    allowed_formats: Option<HashSet<InputFormat>>,
    strict: bool,
    /// Which image references the declarative backends may resolve (#646):
    /// the tier, the remote host allow-list and the limit overrides. See
    /// [`Self::image_sources`]; [`Self::fetch_images`] is the pre-#646 switch.
    image_sources: crate::ImageSources,
    image_hosts: Vec<String>,
    image_limits: ImageLimitOverrides,
    list_attachments: bool,
    /// Omit empty cells from sparse spreadsheet table grids (#271, XLSX/XLS
    /// family; opt-in docling.rs extension).
    skip_empty_cells: bool,
    /// Emit Markdown tables in the compact `| a | b |` form instead of the
    /// width-padded GitHub serializer (#271, opt-in docling.rs extension).
    compact_tables: bool,
    /// Render pictures' alt text in Markdown and HTML (#676). See
    /// [`Self::picture_alt_text`].
    picture_alt_text: bool,
    /// Where the document's metadata goes in the exports (#679). See
    /// [`Self::metadata_output`].
    metadata_output: docling_core::MetadataOutput,
    /// docling-core's `MarkdownParams.page_break_placeholder`: text inserted
    /// between pages in the Markdown export; `None` (default) omits breaks.
    page_break_placeholder: Option<String>,
    /// EBCDIC copybook layout (#252): inline JSON or a file path. `None`
    /// falls back to the `<stem>.layout.json` sidecar.
    ebcdic_layout: Option<String>,
    no_table_former: bool,
    no_text_panels: bool,
    /// The text layer only, no models (#611: `--text-layer-only`, the
    /// pre-2.0 `--no-ocr`) — or `auto`, per document.
    text_layer_only: TextLayerOnly,
    /// Skip OCR, keep layout and tables — docling's `do_ocr=False` (#611:
    /// `--no-ocr`, `--skip-ocr`).
    no_ocr: bool,
    /// The password of an encrypted PDF (#611) or Office document (#625).
    password: Option<String>,
    force_full_page_ocr: bool,
    /// OCR only the pages whose text layer is broken (#1006). See
    /// [`Self::replace_broken_text_layer`].
    replace_broken_text_layer: bool,
    /// OCR mode id (docling's `OcrMode`, #254); parsed at the ML call sites.
    ocr_mode: Option<String>,
    /// OCR engine id (`ppocr` | `tesseract`, #460); parsed at the ML call
    /// sites.
    ocr_engine: Option<String>,
    /// OCR render scale in px/pt (#254); validated at the ML call sites.
    ocr_scale: Option<f32>,
    /// The PDF text layer's word-gap rule (#921); parsed at the PDF call
    /// sites. See [`Self::pdf_word_gap`].
    pdf_word_gap: Option<String>,
    /// Keep a PDF's typographic punctuation as drawn (#921); `None` = the
    /// environment's default. See [`Self::pdf_keep_typography`].
    pdf_keep_typography: Option<bool>,
    /// Report the PDF reader's repairs on the result (#934); `None` = the
    /// environment's default. See [`Self::pdf_report_repairs`].
    pdf_report_repairs: Option<bool>,
    /// RapidOCR's line orientation handling (#666); `None` = the
    /// environment's default. See [`Self::ocr_textline_orientation`].
    ocr_textline_orientation: Option<bool>,
    /// Picture-crop / page-image scale in px/pt (docling's `images_scale`,
    /// #520); `None` = the pipeline's 2.0 render.
    images_scale: Option<f32>,
    /// Keep page renders as the document's page images (docling's
    /// `generate_page_images`, #520).
    generate_page_images: bool,
    /// Pictures take their one safe embedded image instead of a render crop
    /// (docling's `use_embedded_picture_images`, #1007).
    use_embedded_picture_images: bool,
    /// Infer PDF/image section-header levels after assembly (#302, docling's
    /// `HeadingHierarchyModel`): bookmarks > numbering > font style. Off by
    /// default — heading levels then stay exactly as detected.
    heading_hierarchy: bool,
    use_web_browser: bool,
    /// Named Whisper model preset for audio sources (docling's ASR model
    /// specs, PR #3741): English-only / Distil-Whisper variants under
    /// `.models/asr/<preset>/`. `None` = the default Whisper tiny.
    asr_model: Option<String>,
    asr_lang: Option<String>,
    /// Max sampled frames per video (#138 Phase 2). `None` = the default
    /// ([`DEFAULT_VIDEO_FRAMES`]); `Some(0)` disables frame extraction;
    /// [`ALL_VIDEO_FRAMES`] keeps every distinct cut (#647).
    video_frames: Option<usize>,
    /// ffmpeg's scene score a video frame must exceed to count as a cut
    /// (#647); `None` = `DOCLING_RS_VIDEO_SCENE_THRESHOLD`, else 0.27.
    video_scene_threshold: Option<f32>,
    /// Cap on a sampled frame's longer side in px (#647); `None` =
    /// `DOCLING_RS_VIDEO_FRAME_MAX_SIDE`, else the source resolution.
    video_frame_max_side: Option<u32>,
    /// Hamming distance under which a frame's difference hash makes it a
    /// duplicate of a kept one (#647); `None` = `DOCLING_RS_VIDEO_FRAME_DEDUPE`,
    /// else no de-duplication.
    video_frame_dedupe: Option<u32>,
    /// The directory an XBRL instance's taxonomy is read from (docling's
    /// `XBRLBackendOptions.taxonomy`); `None` = the instance's own directory.
    xbrl_taxonomy: Option<PathBuf>,
    /// Opt-in PDF/image enrichment models (docling's
    /// `do_picture_classification` / `do_code_enrichment` /
    /// `do_formula_enrichment`).
    enrich: crate::EnrichmentOptions,
    /// OCR the embedded pictures of non-PDF documents (#645). See
    /// [`Self::do_picture_ocr`].
    picture_ocr: bool,
    /// DocumentFigureClassifier labels a picture must carry to be OCR'd;
    /// empty = every picture. See [`Self::picture_ocr_classes`].
    picture_ocr_classes: Vec<String>,
    /// Smallest side (px) a picture must have to be OCR'd; `None` = the
    /// environment's default. See [`Self::picture_ocr_min_side`].
    picture_ocr_min_side: Option<u32>,
    /// The picture OCR's resolution (#666); `None` = the image-input rule.
    /// See [`Self::picture_ocr_scale`].
    picture_ocr_scale: Option<f32>,
    /// Keep the embedded image bytes on every picture (the default). See
    /// [`Self::keep_picture_images`].
    keep_picture_images: bool,
    /// Describe the pictures through a vision endpoint (#670); `None` = off.
    /// See [`Self::picture_description`].
    #[cfg(feature = "vlm")]
    picture_description: Option<crate::vlm::PictureDescriptionOptions>,
    /// The PII redaction pass (#621), when asked for. See [`Self::redact_pii`].
    redact: Option<docling_core::RedactionOptions>,
    /// 1-based inclusive PDF page window (#80). See [`Self::page_range`].
    page_range: Option<(usize, usize)>,
    /// OCR recognition language for scanned PDF/image pages (`en`/`ch`).
    /// `None` = the process default (`DOCLING_RS_OCR_LANG`, else English).
    ocr_lang: Option<String>,
    /// Character encoding for text inputs (docling's
    /// `TextBackendOptions.encoding`); `None` = detect. See [`Self::encoding`].
    encoding: Option<String>,
    /// Which Jupyter notebook outputs the document keeps (#673). See
    /// [`Self::ipynb_outputs`].
    ipynb_outputs: crate::IpynbOutputs,
    /// How Office tracked changes are read (#851). See
    /// [`Self::track_changes`].
    track_changes: Option<crate::TrackChanges>,
    /// [`Self::extract_diagrams`].
    extract_diagrams: crate::DiagramFormat,
    /// [`Self::use_outline_level_for_headings`].
    use_outline_level_for_headings: bool,
    /// Directory referenced-mode streaming writes images into (#80).
    /// See [`Self::artifacts_dir`].
    artifacts_dir: String,
    /// Per-document wall-clock budget for the PDF/image pipeline (docling's
    /// `document_timeout`, #497). See [`Self::document_timeout`].
    document_timeout: Option<std::time::Duration>,
    /// Bounds on what a ZIP input may decompress (#557,
    /// [`DocumentConverter::convert_archive`]).
    archive_limits: crate::archive::ArchiveLimits,
    /// The caller's long-lived ML pipeline (#669), when one was lent with
    /// [`Self::warm_pipeline`]; `None` loads the models per conversion.
    #[cfg(feature = "pdf")]
    warm: Option<WarmPipeline>,
    /// Progress callbacks (#783): every conversion's events reach each of
    /// them. See [`Self::progress_callback`].
    progress: Callbacks,
    /// The stderr printer [`Self::show_progress`] attaches.
    progress_printer: Option<std::sync::Arc<ProgressPrinter>>,
    /// Backends registered with [`Self::register_backend`] (#680), in the
    /// order they are tried: highest priority first, registration order
    /// among equals.
    backends: std::sync::Arc<Vec<crate::plugins::RegisteredBackend>>,
}

/// The builder's overrides of [`crate::ImageLimits`] (#646): `None` = the
/// environment's value, else the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ImageLimitOverrides {
    max_bytes: Option<u64>,
    max_images: Option<usize>,
    max_total_mb: Option<u64>,
    min_bytes: Option<u64>,
}

/// Default cap on sampled frames per video. Scene changes rarely exceed this
/// in short clips, and uniform fallback at 8 keeps JSON/DCLX output (which
/// embeds the PNGs) within sane bounds.
pub const DEFAULT_VIDEO_FRAMES: usize = 8;

/// The `video_frames` count meaning "every distinct cut" (#647): no cap,
/// no even resampling of a cut-less video (its first frame alone), the
/// scene threshold and de-duplication deciding what a distinct frame is.
/// Any count at or above `u32::MAX` reads as this on the JSON surfaces, so
/// a binding whose integers are 32-bit (the npm addon) can ask for it.
pub const ALL_VIDEO_FRAMES: usize = usize::MAX;

/// Parse a user-facing page-range string (issue #80's `--pages`): `"A-B"` for
/// an inclusive 1-based window, or a single `"N"` for one page. Whitespace
/// around the numbers is tolerated. Validation against the actual page count
/// happens at convert time; this only checks the spelling (`first >= 1`,
/// `first <= last`).
pub fn parse_page_range(s: &str) -> Result<(usize, usize), String> {
    let parse_one = |part: &str| {
        part.trim()
            .parse::<usize>()
            .map_err(|_| format!("invalid page number '{}'", part.trim()))
    };
    let (first, last) = match s.split_once('-') {
        Some((a, b)) => (parse_one(a)?, parse_one(b)?),
        None => {
            let n = parse_one(s)?;
            (n, n)
        }
    };
    if first == 0 {
        return Err("pages are 1-based; the range starts at 1".into());
    }
    if last < first {
        return Err(format!("range {first}-{last} is inverted (first <= last)"));
    }
    Ok((first, last))
}

/// A PDF/image ML pipeline kept loaded across conversions (#669) — what a
/// long-lived caller (a server, an agent tool) lends its converters with
/// [`DocumentConverter::warm_pipeline`] so the layout, TableFormer and OCR
/// sessions load once instead of once per document. Cloning shares the
/// pipeline; conversions on it take turns.
///
/// The pipeline is rebuilt only when a conversion asks for different model
/// switches (text-layer only, OCR off, no TableFormer, no text panels, the
/// enrichments — what a built pipeline cannot change); a panic inside a
/// conversion discards it, so the next one starts clean.
#[cfg(feature = "pdf")]
#[derive(Clone, Default)]
pub struct WarmPipeline {
    slot: std::sync::Arc<std::sync::Mutex<WarmSlot>>,
}

#[cfg(feature = "pdf")]
type WarmSlot = Option<(PipelineSwitches, docling_pdf::Pipeline)>;

#[cfg(feature = "pdf")]
impl WarmPipeline {
    /// An empty handle; the models load on the first PDF/image conversion.
    pub fn new() -> Self {
        Self::default()
    }

    /// The pipeline for a conversion with `switches`, rebuilt when the
    /// loaded one was built with others.
    fn lease(
        &self,
        switches: PipelineSwitches,
    ) -> Result<std::sync::MutexGuard<'_, WarmSlot>, docling_pdf::PdfError> {
        let mut slot = self.slot.lock().unwrap_or_else(|poisoned| {
            // A conversion panicked mid-document: its workers may hold half a
            // page — start from a fresh pipeline rather than reuse them.
            let mut slot = poisoned.into_inner();
            *slot = None;
            slot
        });
        self.slot.clear_poison();
        if slot.as_ref().map(|(built, _)| *built) != Some(switches) {
            // Drop the old models before the new ones load, not after.
            *slot = None;
            *slot = Some((switches, switches.build()?));
        }
        Ok(slot)
    }
}

#[cfg(feature = "pdf")]
impl std::fmt::Debug for WarmPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let loaded = match self.slot.try_lock() {
            Ok(slot) => format!("{:?}", slot.as_ref().map(|(switches, _)| switches)),
            Err(_) => "in use".to_string(),
        };
        f.debug_struct("WarmPipeline")
            .field("loaded", &loaded)
            .finish()
    }
}

/// What a built [`docling_pdf::Pipeline`] cannot change: the model set it
/// loads. Every other setting is applied per conversion.
#[cfg(feature = "pdf")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PipelineSwitches {
    text_layer_only: bool,
    no_ocr: bool,
    no_table_former: bool,
    no_text_panels: bool,
    enrich: crate::EnrichmentOptions,
}

#[cfg(feature = "pdf")]
impl PipelineSwitches {
    fn of(c: &DocumentConverter) -> Self {
        Self {
            // `Auto` is the full model set with a per-conversion switch
            // (`configure_pipeline`), so the warm pipeline serves digital and
            // scanned documents in turn without a rebuild.
            text_layer_only: c.text_layer_only == TextLayerOnly::On,
            no_ocr: c.no_ocr,
            no_table_former: c.no_table_former,
            no_text_panels: c.no_text_panels,
            enrich: c.enrich,
        }
    }

    fn build(self) -> Result<docling_pdf::Pipeline, docling_pdf::PdfError> {
        Ok(docling_pdf::Pipeline::new()?
            .no_table_former(self.no_table_former)
            // docling-pdf's `Pipeline` keeps its pre-2.0 names: its `no_ocr`
            // is the text-layer fast path, its `skip_ocr` docling's do_ocr=False.
            .no_ocr(self.text_layer_only)
            .skip_ocr(self.no_ocr)
            .no_text_panels(self.no_text_panels)
            .enrichments(self.enrich))
    }
}

/// A conversion's pipeline: its own, or the warm one held for the call.
#[cfg(feature = "pdf")]
enum PipelineLease<'a> {
    Fresh(Box<docling_pdf::Pipeline>),
    Warm(std::sync::MutexGuard<'a, WarmSlot>),
}

#[cfg(feature = "pdf")]
impl std::ops::Deref for PipelineLease<'_> {
    type Target = docling_pdf::Pipeline;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Fresh(p) => p,
            Self::Warm(slot) => &slot.as_ref().expect("filled by the lease").1,
        }
    }
}

#[cfg(feature = "pdf")]
impl std::ops::DerefMut for PipelineLease<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Fresh(p) => p,
            Self::Warm(slot) => &mut slot.as_mut().expect("filled by the lease").1,
        }
    }
}

impl Default for DocumentConverter {
    fn default() -> Self {
        Self {
            allowed_formats: None,
            strict: false,
            image_sources: crate::ImageSources::None,
            image_hosts: Vec::new(),
            image_limits: ImageLimitOverrides::default(),
            list_attachments: false,
            skip_empty_cells: false,
            compact_tables: false,
            picture_alt_text: false,
            metadata_output: Default::default(),
            page_break_placeholder: None,
            ebcdic_layout: None,
            no_table_former: false,
            no_text_panels: false,
            text_layer_only: TextLayerOnly::Off,
            no_ocr: false,
            password: None,
            force_full_page_ocr: false,
            replace_broken_text_layer: false,
            ocr_mode: None,
            ocr_engine: None,
            ocr_scale: None,
            pdf_word_gap: None,
            pdf_keep_typography: None,
            pdf_report_repairs: None,
            ocr_textline_orientation: None,
            images_scale: None,
            generate_page_images: false,
            use_embedded_picture_images: false,
            heading_hierarchy: false,
            use_web_browser: false,
            asr_model: None,
            asr_lang: None,
            video_frames: None,
            video_scene_threshold: None,
            video_frame_max_side: None,
            video_frame_dedupe: None,
            xbrl_taxonomy: None,
            enrich: crate::EnrichmentOptions::default(),
            picture_ocr: false,
            picture_ocr_classes: Vec::new(),
            picture_ocr_min_side: None,
            picture_ocr_scale: None,
            keep_picture_images: true,
            #[cfg(feature = "vlm")]
            picture_description: None,
            redact: None,
            page_range: None,
            ocr_lang: None,
            encoding: None,
            ipynb_outputs: crate::IpynbOutputs::None,
            track_changes: None,
            extract_diagrams: crate::DiagramFormat::default(),
            use_outline_level_for_headings: true,
            artifacts_dir: "artifacts".to_string(),
            document_timeout: None,
            archive_limits: crate::archive::ArchiveLimits::default(),
            #[cfg(feature = "pdf")]
            warm: None,
            progress: Callbacks::default(),
            progress_printer: None,
            backends: std::sync::Arc::new(Vec::new()),
        }
    }
}

impl DocumentConverter {
    /// A converter that accepts every supported format.
    pub fn new() -> Self {
        Self::default()
    }

    /// A converter restricted to an explicit set of formats. Sources of any
    /// other format are rejected with [`ConversionError::UnsupportedFormat`].
    pub fn with_allowed_formats(formats: impl IntoIterator<Item = InputFormat>) -> Self {
        Self {
            allowed_formats: Some(formats.into_iter().collect()),
            ..Self::default()
        }
    }

    /// Convert only PDF pages `first..=last` (**1-based** inclusive, the page
    /// numbers a viewer shows — issue #80's `--pages A-B`). Out-of-window pages
    /// are skipped before rasterization, so converting 3 pages of a 500-page
    /// PDF costs 3 pages. `last` clamps to the document; a window that selects
    /// no pages at all errors at convert time. Non-PDF formats ignore the
    /// window (they convert whole).
    pub fn page_range(mut self, first: usize, last: usize) -> Self {
        self.page_range = Some((first, last));
        self
    }

    /// A wall-clock budget per document for the PDF pipeline — docling's
    /// `PipelineOptions.document_timeout` (#497); `None` = unlimited, the
    /// default. The budget starts with the conversion and is checked between
    /// pages: once spent, no further page is rendered or processed, the pages
    /// already finished become the document, and the result is a
    /// [`ConversionStatus::PartialSuccess`] carrying one
    /// [`ErrorItem`](crate::ErrorItem) (`document_backend` / `pipeline`,
    /// "document timeout of Ns exceeded after D of S pages …"). A page in
    /// flight finishes first, so a budget shorter than one page's work still
    /// yields that page. Only the paginated pipeline has pages to stop
    /// between: declarative formats (Office, HTML, …) convert whole, as in
    /// docling, and a single image is never cut. A streaming conversion
    /// emits the pages that fit and ends with
    /// [`ConversionError::Timeout`](crate::ConversionError::Timeout) as its
    /// last item.
    pub fn document_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.document_timeout = timeout;
        self
    }

    /// Receive the progress events of every conversion this converter runs
    /// (#783; docling's `DocumentConverter(progress_callback=…)`): the
    /// document's start and end, a page event per PDF / image page with the
    /// completed count (a page the budget cut off reported as failed), the
    /// picture passes' item counts — see [`crate::progress`]. Called from
    /// the converting threads, so the callback must be cheap and
    /// thread-safe; several may be added, each gets every event. A
    /// conversion with no callback costs nothing extra.
    pub fn progress_callback(mut self, callback: ProgressCallback) -> Self {
        self.progress.0.push(callback);
        self
    }

    /// Print progress to stderr with the built-in [`ProgressPrinter`]
    /// (docling's `show_progress`): a line per document, a bar for its
    /// pages and each enrichment pass, drawn in place on a terminal. Can be
    /// combined with [`Self::progress_callback`].
    pub fn show_progress(mut self, enable: bool) -> Self {
        self.progress_printer = enable.then(|| std::sync::Arc::new(ProgressPrinter::new()));
        self
    }

    /// The reporter of one document: this converter's callbacks, the
    /// printer and `callback`, for `document_index` named `document_name`.
    /// For callers that drive the PDF [`Pipeline`](docling_pdf::Pipeline)
    /// themselves (serve's warm pipeline, the CLI batch, the bindings) and
    /// bracket the conversion with its `document_started` /
    /// `document_completed`; [`Self::convert`] does this itself.
    pub fn reporter(
        &self,
        document_index: usize,
        document_name: &str,
        callback: Option<ProgressCallback>,
    ) -> ProgressReporter {
        let mut callbacks = self.progress.0.clone();
        if let Some(printer) = &self.progress_printer {
            callbacks.push(std::sync::Arc::clone(printer).callback());
        }
        callbacks.extend(callback);
        ProgressReporter::new(callbacks, document_index, document_name)
    }

    /// OCR recognition language for scanned PDF/image pages: `"en"` (the
    /// default — English PP-OCRv3, proper Latin word spacing) or `"ch"` (the
    /// multilingual model docling conformance is measured with — glues Latin
    /// words). An unknown value warns at conversion time and uses the
    /// default; explicit `DOCLING_OCR_REC_ONNX`/`DOCLING_OCR_DICT` paths win
    /// over this switch. Under [`ocr_engine`](Self::ocr_engine) `tesseract`
    /// it is Tesseract's language list instead — tessdata stems (`"deu"`,
    /// `"eng+fra"`, `"script/Cyrillic"`) or BCP-47 tags mapped onto them
    /// (`"de"`, `"zh-Hant"`), see [`docling_pdf::tesseract_lang_arg`]; `en`
    /// and `ch` keep working there. Formats that never OCR ignore it.
    pub fn ocr_lang(mut self, lang: impl Into<String>) -> Self {
        self.ocr_lang = Some(lang.into());
        self
    }

    /// Convert on `warm`'s ML pipeline (#669) instead of loading the models
    /// for each conversion: PDFs, images, SVG's raster route, METS pages and
    /// a scanned DjVu's OCR fallback all run on the one instance, which keeps
    /// its ONNX sessions between conversions. Everything else about the
    /// conversion is this converter's — page window, password, OCR and
    /// image settings are applied to the pipeline on every call, so nothing
    /// carries over from the previous caller's options. Converters sharing a
    /// handle take turns on it; the declarative formats never touch it.
    #[cfg(feature = "pdf")]
    pub fn warm_pipeline(mut self, warm: &WarmPipeline) -> Self {
        self.warm = Some(warm.clone());
        self
    }

    /// The configured ML pipeline for one conversion: a fresh one (models
    /// load per call), or the [`warm`](Self::warm_pipeline) one this
    /// converter was lent. Both are configured by the same setters, so the
    /// warm path cannot drift from the cold one.
    #[cfg(feature = "pdf")]
    fn ml_pipeline(
        &self,
        progress: Option<docling_pdf::PageProgressHook>,
    ) -> Result<PipelineLease<'_>, docling_pdf::PdfError> {
        let switches = PipelineSwitches::of(self);
        let mut lease = match &self.warm {
            None => PipelineLease::Fresh(Box::new(switches.build()?)),
            Some(warm) => PipelineLease::Warm(warm.lease(switches)?),
        };
        self.configure_pipeline(&mut lease, progress);
        Ok(lease)
    }

    /// Every per-conversion setting, set unconditionally — on a warm
    /// pipeline a setting left alone would be the previous conversion's
    /// (its progress hook included: a lent pipeline must not report this
    /// document's pages to the last document's observer).
    #[cfg(feature = "pdf")]
    fn configure_pipeline(
        &self,
        p: &mut docling_pdf::Pipeline,
        progress: Option<docling_pdf::PageProgressHook>,
    ) {
        p.set_progress(progress);
        p.set_text_layer_auto(self.text_layer_only == TextLayerOnly::Auto);
        p.set_ocr_textline_orientation(self.ocr_textline_orientation);
        p.set_ocr_lang(self.ocr_lang_choice());
        p.set_ocr_engine(self.ocr_engine_choice());
        p.set_tesseract_lang(self.tesseract_lang_choice());
        p.set_ocr_mode(self.ocr_mode_choice());
        p.set_ocr_scale(self.ocr_scale_choice());
        p.set_word_gap(self.pdf_word_gap_choice());
        p.set_keep_typography(self.pdf_keep_typography);
        p.set_replace_broken_text_layer(self.replace_broken_text_layer);
        p.set_images(docling_pdf::ImageOutput {
            scale: self.images_scale,
            page_images: self.generate_page_images,
            embedded_pictures: self.use_embedded_picture_images,
        });
        p.set_heading_hierarchy(docling_pdf::HeadingHierarchyOptions::enabled(
            self.heading_hierarchy,
        ));
        p.set_document_timeout(self.document_timeout);
        // The PDF route sets these two for itself; every other route reads
        // the whole input with OCR left to the layout.
        p.set_force_full_page_ocr(false);
        p.set_pages(None);
    }

    /// DjVu (#434): the hidden text layer by default; a scan-only DjVu (no
    /// text layer on any selected page) falls back to rasterize + OCR when the
    /// ML pipeline is built and OCR is not disabled, otherwise it degrades to
    /// an empty document with a warning. See [`crate::backend::djvu`].
    // Without the `pdf` feature there is no OCR fallback for the reporter
    // to observe.
    #[cfg_attr(not(feature = "pdf"), allow(unused_variables))]
    fn convert_djvu(
        &self,
        source: &SourceDocument,
        reporter: &std::sync::Arc<ProgressReporter>,
    ) -> Result<docling_core::DoclingDocument, ConversionError> {
        use crate::backend::djvu;
        let text = djvu::convert_text_layer(source, self.page_range)?;
        if !djvu::is_text_layer_empty(&text) {
            return Ok(text);
        }
        // No text layer anywhere in the selection.
        #[cfg(feature = "pdf")]
        if self.text_layer_only != TextLayerOnly::On && !self.no_ocr {
            let pngs = djvu::rasterize_pages(&source.bytes, self.page_range)?;
            let mut pipeline = self
                .ml_pipeline(reporter.page_hook())
                .map_err(|e| ConversionError::with_source("djvu", e))?;
            let mut doc = docling_core::DoclingDocument::new(&source.name);
            for (i, (page_no, png)) in pngs.iter().enumerate() {
                if i > 0 {
                    doc.push(docling_core::Node::PageBreak);
                }
                let mut page = pipeline
                    .convert_image(png, &source.name)
                    .map_err(|e| ConversionError::with_source("djvu", e))?;
                // An image is its own page 1; the DjVu page keeps its number so
                // a `--pages` window's JSON `pages` matches the text-layer path.
                docling_pdf::assemble::stamp_page_no(&mut page.nodes, *page_no);
                doc.nodes.extend(page.nodes);
                doc.links.extend(page.links);
            }
            return Ok(doc);
        }
        eprintln!(
            "docling: warning: DjVu '{}' has no text layer and OCR is unavailable/disabled; \
             the document is empty",
            source.name
        );
        Ok(text)
    }

    /// The parsed [`Self::ocr_lang`] choice for the ML call sites; a value
    /// that parses to nothing warns here (once per conversion) rather than
    /// erroring — same degradation the env selector applies.
    #[cfg(feature = "pdf")]
    fn ocr_lang_choice(&self) -> Option<docling_pdf::OcrLang> {
        let raw = self.ocr_lang.as_deref()?;
        // Under Tesseract the value is its language list, not a PP-OCR model
        // — see `tesseract_lang_choice`.
        if self.ocr_engine_choice() == Some(docling_pdf::OcrEngine::Tesseract) {
            return None;
        }
        let parsed = docling_pdf::OcrLang::parse(raw);
        if parsed.is_none() {
            eprintln!(
                "docling: ocr_lang {raw:?} names no language the OCR models read ({}); using \
                 the default",
                docling_pdf::OcrLang::ACCEPTED
            );
        }
        parsed
    }

    /// The parsed [`Self::ocr_engine`] choice (#460), with the same
    /// warn-and-default degradation as [`ocr_lang_choice`](Self::ocr_lang_choice).
    #[cfg(feature = "pdf")]
    fn ocr_engine_choice(&self) -> Option<docling_pdf::OcrEngine> {
        let raw = self.ocr_engine.as_deref()?;
        let parsed = docling_pdf::OcrEngine::parse(raw);
        if parsed.is_none() {
            eprintln!(
                "docling: ocr_engine {raw:?} is not {}; using the default",
                docling_pdf::OcrEngine::ACCEPTED
            );
        }
        parsed
    }

    /// Tesseract's `-l` argument from [`Self::ocr_lang`] (#460) when the
    /// engine is Tesseract: an unmappable value warns and leaves Tesseract's
    /// default language.
    #[cfg(feature = "pdf")]
    fn tesseract_lang_choice(&self) -> Option<String> {
        if self.ocr_engine_choice() != Some(docling_pdf::OcrEngine::Tesseract) {
            return None;
        }
        let raw = self.ocr_lang.as_deref()?;
        match docling_pdf::tesseract_lang_arg(raw) {
            Ok(arg) => Some(arg),
            Err(e) => {
                eprintln!("docling: {e}; using Tesseract's default language");
                None
            }
        }
    }

    /// The parsed [`Self::ocr_mode`] choice (#254), with the same
    /// warn-and-default degradation as [`ocr_lang_choice`](Self::ocr_lang_choice).
    #[cfg(feature = "pdf")]
    fn ocr_mode_choice(&self) -> Option<docling_pdf::OcrMode> {
        let raw = self.ocr_mode.as_deref()?;
        let parsed = docling_pdf::OcrMode::parse(raw);
        if parsed.is_none() {
            eprintln!(
                "docling: ocr_mode {raw:?} is not \
                 default|full_page|layout_regions|pdf_aware_layout_regions; using the default"
            );
        }
        parsed
    }

    /// The parsed [`Self::pdf_word_gap`] choice (#921), with the same
    /// warn-and-default degradation as [`ocr_mode_choice`](Self::ocr_mode_choice).
    #[cfg(any(feature = "pdf", feature = "pdf-text"))]
    fn pdf_word_gap_choice(&self) -> Option<docling_pdf::WordGap> {
        let raw = self.pdf_word_gap.as_deref()?;
        match docling_pdf::WordGap::parse(raw) {
            Ok(gap) => Some(gap),
            Err(e) => {
                eprintln!("docling: {e}; using the default");
                None
            }
        }
    }

    /// The validated [`Self::ocr_scale`] (#254): non-positive/non-finite
    /// values warn and fall back to the engine default.
    #[cfg(feature = "pdf")]
    fn ocr_scale_choice(&self) -> Option<f32> {
        let s = self.ocr_scale?;
        if !(s.is_finite() && s > 0.0) {
            eprintln!("docling: ocr_scale {s} is not a positive number; using the default");
            return None;
        }
        Some(s)
    }

    /// The validated [`Self::picture_ocr_scale`] (#666), like
    /// [`Self::ocr_scale_choice`].
    #[cfg(feature = "pdf")]
    fn picture_ocr_scale_choice(&self) -> Option<f32> {
        let s = self.picture_ocr_scale?;
        if !(s.is_finite() && s > 0.0) {
            eprintln!("docling: picture_ocr_scale {s} is not a positive number; using the default");
            return None;
        }
        Some(s)
    }

    /// [`Self::ocr_textline_orientation`] on a pipeline (#666); unset keeps
    /// the pipeline's `DOCLING_RS_OCR_TEXTLINE_ORIENTATION` default.
    #[cfg(feature = "pdf")]
    fn with_textline_orientation(&self, p: docling_pdf::Pipeline) -> docling_pdf::Pipeline {
        match self.ocr_textline_orientation {
            Some(enable) => p.ocr_textline_orientation(enable),
            None => p,
        }
    }

    /// Where [`ImageMode::Referenced`] streaming writes image files, and the
    /// link prefix used in the Markdown (default `artifacts`, matching the
    /// buffered export's convention). Relative paths resolve against the
    /// process working directory.
    pub fn artifacts_dir(mut self, dir: impl Into<String>) -> Self {
        self.artifacts_dir = dir.into();
        self
    }

    /// Decode text inputs (Markdown, CSV, AsciiDoc, WebVTT, LaTeX, the XML
    /// dialects, …) with this character encoding instead of detecting one —
    /// docling's `TextBackendOptions.encoding`
    /// (`MarkdownBackendOptions(encoding="shift_jis")`). A WHATWG encoding
    /// label (`shift_jis`, `koi8-r`, `windows-1251`, `latin1`; Python codec
    /// spellings with `_` are accepted). Nothing is guessed: bytes the
    /// encoding cannot decode fail the conversion, as does an unknown label.
    /// `None` (default) detects — a byte-order mark, then UTF-8, then
    /// windows-1252 (see [`SourceDocument::text`]). A source that already
    /// carries its own [`SourceDocument::encoding`] keeps it.
    pub fn encoding(mut self, label: Option<String>) -> Self {
        self.encoding = label;
        self
    }

    /// Jupyter notebooks (#673): which cell outputs the document keeps —
    /// [`IpynbOutputs::None`](crate::IpynbOutputs::None) (the default: the
    /// notebook as written, MarkItDown's reading) or
    /// [`IpynbOutputs::Text`](crate::IpynbOutputs::Text): stream and
    /// `text/plain` results as code blocks after their cell, raster image
    /// outputs as pictures. The CLI's `--ipynb-outputs none|text`.
    pub fn ipynb_outputs(mut self, outputs: crate::IpynbOutputs) -> Self {
        self.ipynb_outputs = outputs;
        self
    }

    /// How tracked changes in DOCX, DOC and ODT documents are read (#851),
    /// Pandoc's `--track-changes`: [`TrackChanges::Accept`](crate::TrackChanges::Accept)
    /// shows insertions and leaves deletions out,
    /// [`Reject`](crate::TrackChanges::Reject) the reverse,
    /// [`All`](crate::TrackChanges::All) keeps both as typed revisions
    /// (author, date) the Pandoc AST writes as `Span .insertion` /
    /// `.deletion`, and the reviewer comments with them. `None` (the
    /// default) is each reader's own view — DOCX and ODT accept, and the
    /// reviewer comments reach the Pandoc AST either way; an explicit
    /// `accept` / `reject` leaves them out, as Pandoc does. What each reader
    /// implements (DOCX: all three; DOC: accept and reject; ODT: accept) is
    /// in `docs/OPTIONS.md`; a mode a reader lacks is refused with
    /// [`ConversionError::UnsupportedOption`](crate::ConversionError::UnsupportedOption).
    /// The CLI's `--track-changes accept|reject|all`.
    pub fn track_changes(mut self, policy: Option<crate::TrackChanges>) -> Self {
        self.track_changes = policy;
        self
    }

    /// Structured diagrams from DOCX DrawingML drawings (#858):
    /// [`DiagramFormat::Mermaid`](crate::DiagramFormat::Mermaid), the
    /// default, adds a `mermaid` code item after a drawing's picture with
    /// the flowchart its labelled shapes and connectors describe — only
    /// from connectors that name their two end shapes; a drawing whose
    /// lines are laid by eye gets no graph, never an invented one.
    /// [`DiagramFormat::None`](crate::DiagramFormat::None) turns the
    /// extraction off. The CLI's `--extract-diagrams mermaid|none`.
    pub fn extract_diagrams(mut self, format: crate::DiagramFormat) -> Self {
        self.extract_diagrams = format;
        self
    }

    /// Word documents (DOCX, DOC): whether a paragraph style's outline
    /// level (`w:outlineLvl`, the `.doc` `sprmPOutLvl`) alone makes it a
    /// heading — docling's `use_outline_level_for_headings` (docling#4480),
    /// on by default: it is how a localized or custom heading style
    /// ("Nadpis1", "Rubrik 1") is recognized. Off for templates whose body
    /// styles carry an outline level only for the table of contents
    /// (numbered legal clauses), which would otherwise all be headings. A
    /// style named a heading keeps taking its level from the outline. The
    /// CLI's `--no-outline-level-headings`.
    pub fn use_outline_level_for_headings(mut self, on: bool) -> Self {
        self.use_outline_level_for_headings = on;
        self
    }

    /// The converter's [`encoding`](Self::encoding) applied to a source that
    /// did not set its own.
    /// Bounds on what a ZIP input may make the converter decompress (#557):
    /// entries considered, per-entry and total uncompressed size, compression
    /// ratio. [`ArchiveLimits::default`](crate::archive::ArchiveLimits) when
    /// not set. Only [`DocumentConverter::convert_archive`] reads them.
    pub fn archive_limits(mut self, limits: crate::archive::ArchiveLimits) -> Self {
        self.archive_limits = limits;
        self
    }

    /// Register `backend` for the sources `matcher` claims (#680), under
    /// `name` (what format listings print). Registered backends are tried
    /// before the built-in formats — highest `priority` first, the earliest
    /// registration first among equals — so a backend can add a format or
    /// take one over; its document then goes through the same
    /// post-processing as a built-in one. An extension the built-in tables
    /// do not know reaches a converter through [`Self::load_file`] /
    /// [`Self::load_named_bytes`], which resolve such a source as
    /// [`InputFormat::Plugin`]. See [`crate::plugins`].
    ///
    /// ```
    /// use std::sync::Arc;
    /// use docling::{BackendMatcher, ConversionError, DocumentConverter, SourceDocument};
    /// use docling::backend::DeclarativeBackend;
    /// use docling_core::{DoclingDocument, Node};
    ///
    /// struct Shout;
    /// impl DeclarativeBackend for Shout {
    ///     fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
    ///         let mut doc = DoclingDocument::new(&source.name);
    ///         doc.push(Node::Paragraph { text: source.text()?.to_uppercase() });
    ///         Ok(doc)
    ///     }
    /// }
    ///
    /// let converter = DocumentConverter::new().register_backend(
    ///     "shout",
    ///     BackendMatcher::new().extensions(["shout"]),
    ///     Arc::new(Shout),
    ///     0,
    /// );
    /// let source = converter.load_named_bytes("note.shout", b"hello".to_vec(), None)?;
    /// let md = converter.convert(source)?.document.export_to_markdown();
    /// assert_eq!(md, "HELLO\n");
    /// # Ok::<(), ConversionError>(())
    /// ```
    pub fn register_backend(
        mut self,
        name: impl Into<String>,
        matcher: crate::plugins::BackendMatcher,
        backend: std::sync::Arc<dyn crate::backend::DeclarativeBackend + Send + Sync>,
        priority: i32,
    ) -> Self {
        let mut backends = (*self.backends).clone();
        backends.push(crate::plugins::RegisteredBackend::new(
            name.into(),
            matcher,
            backend,
            priority,
        ));
        // Stable: registration order stays among equal priorities.
        backends.sort_by_key(|b| std::cmp::Reverse(b.priority()));
        self.backends = std::sync::Arc::new(backends);
        self
    }

    /// The backends registered with [`Self::register_backend`], in the
    /// order they are tried.
    pub fn registered_backends(&self) -> &[crate::plugins::RegisteredBackend] {
        &self.backends
    }

    /// The registered backend that claims `source`, if any.
    fn backend_for(&self, source: &SourceDocument) -> Option<&crate::plugins::RegisteredBackend> {
        self.backends.iter().find(|b| {
            b.matcher().matches(
                source.extension.as_deref(),
                source.media_type.as_deref(),
                &source.bytes,
            )
        })
    }

    /// Whether a registered backend claims `source` (the streaming path
    /// then converts it buffered, through [`Self::convert`]).
    pub(crate) fn claims(&self, source: &SourceDocument) -> bool {
        self.backend_for(source).is_some()
    }

    /// [`SourceDocument::from_file`] that knows this converter's registered
    /// backends (#680): a file a registered backend claims — by extension,
    /// or by its bytes — loads even when the built-in tables do not know
    /// it, as [`InputFormat::Plugin`] (a claimed file whose extension *is*
    /// a built-in format keeps that format). Without registered backends,
    /// exactly [`SourceDocument::from_file`].
    pub fn load_file(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<SourceDocument, ConversionError> {
        let path = path.as_ref();
        let bytes = std::fs::read(path)?;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        if self
            .backends
            .iter()
            .any(|b| b.matcher().matches(ext.as_deref(), None, &bytes))
        {
            let format = ext
                .as_deref()
                .and_then(InputFormat::from_extension)
                .unwrap_or(InputFormat::Plugin);
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("document")
                .to_string();
            let mut source = SourceDocument::from_bytes(name, format, bytes);
            source.extension = ext;
            source.path = Some(path.to_path_buf());
            return Ok(source);
        }
        SourceDocument::from_path_and_bytes(path, bytes)
    }

    /// [`SourceDocument::from_named_bytes`] that knows this converter's
    /// registered backends (#680), as [`Self::load_file`] does: bytes a
    /// registered backend claims — by the name's extension, the media type
    /// or the content — load even when the built-in tables do not know
    /// them, as [`InputFormat::Plugin`] unless the extension or media type
    /// names a built-in format.
    pub fn load_named_bytes(
        &self,
        name: &str,
        bytes: Vec<u8>,
        media_type: Option<&str>,
    ) -> Result<SourceDocument, ConversionError> {
        let path = std::path::Path::new(name);
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        if self
            .backends
            .iter()
            .any(|b| b.matcher().matches(ext.as_deref(), media_type, &bytes))
        {
            let format = ext
                .as_deref()
                .and_then(InputFormat::from_extension)
                .or_else(|| media_type.and_then(InputFormat::from_mime))
                .unwrap_or(InputFormat::Plugin);
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("document")
                .to_string();
            let mut source = SourceDocument::from_bytes(stem, format, bytes);
            source.extension = ext;
            source.media_type = media_type.map(str::to_string);
            return Ok(source);
        }
        SourceDocument::from_named_bytes(name, bytes, media_type)
    }

    /// Every input file extension this converter takes: the build's
    /// built-in ones ([`InputFormat::supported_extensions`]) and those of
    /// its registered backends (#680) — sorted, unique, lowercase, no dot.
    pub fn supported_extensions(&self) -> Vec<String> {
        let mut out: Vec<String> = InputFormat::supported_extensions()
            .into_iter()
            .map(str::to_string)
            .chain(
                self.backends
                    .iter()
                    .flat_map(|b| b.matcher().extension_list().iter().cloned()),
            )
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    pub(crate) fn archive_limits_ref(&self) -> crate::archive::ArchiveLimits {
        self.archive_limits
    }

    fn with_encoding(&self, mut source: SourceDocument) -> SourceDocument {
        if source.encoding.is_none() {
            source.encoding = self.encoding.clone();
        }
        source
    }

    /// Cap the number of frames sampled from a video (#138 Phase 2); `0`
    /// disables frame extraction entirely (Phase 1 behavior: transcript only).
    /// Defaults to [`DEFAULT_VIDEO_FRAMES`]. Frames are extracted with the
    /// `ffmpeg` binary when present (`DOCLING_FFMPEG` overrides the path);
    /// without it a video converts to its transcript alone.
    pub fn video_frames(mut self, max: usize) -> Self {
        self.video_frames = Some(max);
        self
    }

    /// Keep every distinct cut of a video (#647) — [`ALL_VIDEO_FRAMES`]:
    /// no cap, so what bounds the picture count is the scene threshold and
    /// the de-duplication. Memory stays one frame at a time whatever the
    /// count ([`do_picture_ocr`](Self::do_picture_ocr) +
    /// [`keep_picture_images`](Self::keep_picture_images)`(false)` reads and
    /// drops each frame before the next is decoded).
    pub fn video_frames_all(self) -> Self {
        self.video_frames(ALL_VIDEO_FRAMES)
    }

    /// ffmpeg's `scene` score (0–1, the normalized difference of a frame to
    /// its predecessor) a frame must exceed to be a cut (#647). Default
    /// 0.27 (`DOCLING_RS_VIDEO_SCENE_THRESHOLD` overrides it): a slide
    /// change in a screen recording scores well above it, a fade or a
    /// lighting change around 0.5, a hard cut near 1.0 — raise it to keep
    /// only hard cuts. Out-of-range values are clamped.
    pub fn video_scene_threshold(mut self, threshold: f32) -> Self {
        self.video_scene_threshold = Some(threshold.clamp(0.0, 1.0));
        self
    }

    /// Downscale each sampled frame inside ffmpeg so its longer side is at
    /// most `px` (#647; the aspect ratio is kept, a smaller frame is left as
    /// it is): a 1080p recording at 640 yields 640×360 PNGs, so neither the
    /// embedded images nor the OCR input ever reach memory at full size.
    /// `0` (the default, `DOCLING_RS_VIDEO_FRAME_MAX_SIDE` overrides it) =
    /// the source resolution.
    pub fn video_frame_max_side(mut self, px: u32) -> Self {
        self.video_frame_max_side = Some(px);
        self
    }

    /// Drop a sampled frame whose perceptual difference hash (64 bits over
    /// a 9×8 grey thumbnail) is within `max_distance` bits of a frame
    /// already kept (#647): the same slide after a fade, or shown again
    /// after a camera cut, becomes one picture. 4–6 is a good distance;
    /// `None` (the default, `DOCLING_RS_VIDEO_FRAME_DEDUPE` overrides it)
    /// keeps every sampled frame.
    pub fn video_frame_dedupe(mut self, max_distance: Option<u32>) -> Self {
        self.video_frame_dedupe = max_distance.map(|d| d.min(64));
        self
    }

    /// The folder holding the taxonomy an XBRL instance refers to (docling's
    /// `XBRLBackendOptions.taxonomy`): the filing's extension schema and
    /// linkbases at the relative paths its `link:schemaRef` names, plus any
    /// taxonomy packages (`.zip` with a `META-INF/catalog.xml`) that map the
    /// base taxonomies' `http(s)` URLs to files for offline use. Without it the
    /// instance's own directory is searched. Nothing is fetched remotely; a
    /// document that cannot be found only costs the fact graph the hierarchy
    /// it would have contributed.
    pub fn xbrl_taxonomy(mut self, dir: impl Into<PathBuf>) -> Self {
        self.xbrl_taxonomy = Some(dir.into());
        self
    }

    /// Select a named ASR model preset for audio sources — the English-only
    /// (`whisper_tiny_en`, `whisper_base_en`, `whisper_small_en`) and
    /// Distil-Whisper (`whisper_distil_small_en`) variants of docling's ASR
    /// model specs, or `parakeet_tdt_0.6b_v3` (NVIDIA's Parakeet TDT 0.6B v3
    /// transducer, 25 European languages detected by the model, #508 — see
    /// `docling_asr::parakeet`). `None` (default) uses Whisper tiny
    /// (multilingual) from `.models/asr/`; presets load from
    /// `.models/asr/<preset>/` (fetch them with `download_dependencies.sh
    /// --asr-model=<preset>`).
    pub fn asr_model(mut self, model: Option<String>) -> Self {
        self.asr_model = model;
        self
    }

    /// Select the ASR transcription language for audio/video sources: a
    /// Whisper code (`en`, `de`, `zh`, …) or `auto`. `None` (default) falls
    /// back to `DOCLING_RS_ASR_LANG`, and — when that is unset too — to
    /// per-file auto-detection from the first 30-second window (docling
    /// 2.116 parity). English-only presets always transcribe English.
    pub fn asr_lang(mut self, lang: Option<String>) -> Self {
        self.asr_lang = lang;
        self
    }

    /// Select the Markdown export mode for documents this converter produces.
    ///
    /// `false` (default) makes [`crate::DoclingDocument::export_to_markdown`]
    /// reproduce docling's legacy output byte-for-byte; `true` makes it emit
    /// cleaner, more conformant Markdown (code-fence languages preserved, no
    /// inline-run spacing artifacts, no entity re-escaping). Rust-only — Python
    /// docling has no such switch.
    pub fn strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Fetch and embed external `<img>` images for HTML/EPUB/MHTML/JATS/
    /// AsciiDoc/Markdown/email sources — the pre-#646 switch, kept as an
    /// alias: `true` is [`image_sources`](Self::image_sources)
    /// `(`[`ImageSources::Remote`](crate::ImageSources::Remote)`)`, `false`
    /// is `None`.
    ///
    /// Off by default (matching docling's `enable_*_fetch=False`), so output is
    /// unchanged unless you opt in. `Remote` resolves `data:` URIs, container
    /// parts (EPUB/MHTML entries, email `cid:` attachments), files under the
    /// source file's directory, and `http(s)` URLs — fetched over the network,
    /// so enable it only for input you trust (it can otherwise make the
    /// process issue requests). A server converting untrusted input wants
    /// `image_sources(ImageSources::Embedded)` instead. Unlike before #646 an
    /// absolute local path (`/etc/hosts`, `file://…`) is never read.
    pub fn fetch_images(self, fetch: bool) -> Self {
        self.image_sources(if fetch {
            crate::ImageSources::Remote
        } else {
            crate::ImageSources::None
        })
    }

    /// Which image references the declarative backends resolve (#646):
    /// [`None`](crate::ImageSources::None) (the default — every picture a
    /// placeholder), [`Embedded`](crate::ImageSources::Embedded) (`data:`
    /// URIs and parts of the same container: EPUB/MHTML entries, an email's
    /// `cid:` attachments — no filesystem, no network),
    /// [`Local`](crate::ImageSources::Local) (plus files under the source
    /// file's directory, never an absolute path or one that escapes it) and
    /// [`Remote`](crate::ImageSources::Remote) (plus `http(s)` fetches, see
    /// [`image_hosts`](Self::image_hosts)). Applies to HTML, EPUB, MHTML,
    /// JATS, AsciiDoc, ODF (`http(s)` `draw:image` references), Markdown
    /// (`![…](…)` and inline `<img>`) and email bodies.
    pub fn image_sources(mut self, sources: crate::ImageSources) -> Self {
        self.image_sources = sources;
        self
    }

    /// Restrict `Remote` fetches to these hosts (#646): exact names or
    /// `*.suffix` wildcards for any subdomain, case-insensitive; a redirect
    /// is held to the same list. Empty (the default) allows any host. The
    /// private/loopback/link-local block-list still applies to a listed
    /// host (`DOCLING_RS_ALLOW_PRIVATE_IP_FETCH` is the only way around it).
    pub fn image_hosts<I, S>(mut self, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.image_hosts = hosts
            .into_iter()
            .map(|h| h.into().trim().to_ascii_lowercase())
            .filter(|h| !h.is_empty())
            .collect();
        self
    }

    /// Largest image that resolves, in bytes (#646). Default 32 MiB
    /// (`DOCLING_RS_MAX_IMAGE_BYTES` overrides it); a larger one stays a
    /// placeholder.
    pub fn max_image_bytes(mut self, bytes: u64) -> Self {
        self.image_limits.max_bytes = Some(bytes.max(1));
        self
    }

    /// How many images resolve per document (#646); the rest stay
    /// placeholders and one warning says so. Default unlimited
    /// (`DOCLING_RS_MAX_IMAGES` overrides it).
    pub fn max_images(mut self, count: usize) -> Self {
        self.image_limits.max_images = Some(count);
        self
    }

    /// Total resolved image bytes per document, in MiB (#646). Default
    /// unlimited (`DOCLING_RS_MAX_IMAGE_TOTAL_MB` overrides it).
    pub fn max_image_total_mb(mut self, mb: u64) -> Self {
        self.image_limits.max_total_mb = Some(mb);
        self
    }

    /// Smallest image that resolves, in bytes (#646) — spacers and tracking
    /// pixels are a few dozen bytes. Default 0 (`DOCLING_RS_MIN_IMAGE_BYTES`
    /// overrides it).
    pub fn min_image_bytes(mut self, bytes: u64) -> Self {
        self.image_limits.min_bytes = Some(bytes);
        self
    }

    /// The Jupyter notebook backend (#673) under the builder's Markdown
    /// flavour, image policy and [`ipynb_outputs`](Self::ipynb_outputs).
    fn ipynb_backend(&self) -> crate::IpynbBackend {
        crate::IpynbBackend {
            strict: self.strict,
            images: self.image_policy(),
            outputs: self.ipynb_outputs,
        }
    }

    /// The image policy a conversion runs under (#646): the builder's tier
    /// and hosts, the limits from the builder where set, else the
    /// environment, else the defaults.
    pub fn image_policy(&self) -> crate::ImagePolicy {
        let env = crate::ImageLimits::from_env();
        let o = &self.image_limits;
        crate::ImagePolicy {
            sources: self.image_sources,
            hosts: self.image_hosts.clone(),
            limits: crate::ImageLimits {
                max_bytes: o.max_bytes.unwrap_or(env.max_bytes),
                max_images: o.max_images.or(env.max_images),
                max_total_bytes: o
                    .max_total_mb
                    .map(|mb| mb.saturating_mul(1024 * 1024))
                    .or(env.max_total_bytes),
                min_bytes: o.min_bytes.unwrap_or(env.min_bytes),
            },
        }
    }

    /// Append an `Attachments` section to converted emails (`.eml` / `.msg`):
    /// one list item per attachment, `name (content/type)` — names and types
    /// only, the payload is never embedded. docling's opt-in
    /// `EmailBackendOptions.list_attachments` (#251); off by default.
    pub fn list_attachments(mut self, list: bool) -> Self {
        self.list_attachments = list;
        self
    }

    /// Omit empty cells from sparse spreadsheet table grids (#271; XLSX/XLS
    /// family, opt-in — a docling.rs extension, docling materialises the full
    /// bounding box). A ragged region's box is mostly padding on sparse
    /// sheets (~7× output inflation); with this on, each row keeps only its
    /// occupied cells (merge-covered continuations included) and a table
    /// that loses cells drops its span/structure overlay. Off by default —
    /// default output stays byte-for-byte docling.
    pub fn skip_empty_cells(mut self, skip: bool) -> Self {
        self.skip_empty_cells = skip;
        self
    }

    /// Emit Markdown tables in the compact `| a | b |` / `| - | - |` form
    /// instead of docling-core's width-padded GitHub serializer (#271, all
    /// formats; opt-in — a docling.rs extension). On sparse spreadsheets the
    /// padding dominates the output size; compact rendering keeps the grid
    /// semantics and drops only whitespace. Off by default — default output
    /// stays byte-for-byte docling.
    pub fn compact_tables(mut self, compact: bool) -> Self {
        self.compact_tables = compact;
        self
    }

    /// Render pictures' author-written alt text (#676) — the `descr` /
    /// `title` of a DOCX, PPTX or XLSX picture's non-visual properties,
    /// which the backends keep as the picture's description with
    /// `created_by = "alt_text"` — in the Markdown and HTML exports: the
    /// `![alt](…)` of an image line, the line a description takes above a
    /// placeholder, the `<img alt>` and the description text in HTML. Off
    /// by default, so the default Markdown and HTML stay byte-for-byte
    /// docling's; strict Markdown renders it either way, and the JSON
    /// always carries it as `meta.description`. MarkItDown prints the alt
    /// text.
    pub fn picture_alt_text(mut self, on: bool) -> Self {
        self.picture_alt_text = on;
        self
    }

    /// Where the file's own metadata goes in the exports (#679) — an
    /// EPUB's Dublin Core, an Office package's properties, an ODF
    /// `meta.xml`, a photo's EXIF, an audio file's tags, a PDF's Info
    /// dictionary, read into `document.metadata`. `None` (the default)
    /// reads nothing and leaves every export as it was; `Json` adds a
    /// `metadata` object to the docling JSON; `Frontmatter` adds YAML front
    /// matter at the top of the Markdown as well. The PII pass
    /// ([`redact_pii`](Self::redact_pii)) covers the metadata: author
    /// fields are masked as persons, the GPS position is dropped.
    pub fn metadata_output(mut self, mode: docling_core::MetadataOutput) -> Self {
        self.metadata_output = mode;
        self
    }

    /// The metadata of `source` as [`metadata_output`](Self::metadata_output)
    /// would read it, whatever the mode — for a caller that streams a
    /// document itself and wants the front matter
    /// (`MarkdownStreamer::with_metadata`). Reads headers only; empty for a
    /// format that carries none.
    pub fn document_metadata(&self, source: &SourceDocument) -> docling_core::DocumentMetadata {
        crate::metadata::read(source, self.password.as_deref())
    }

    /// Insert `placeholder` between pages in the Markdown export — docling's
    /// `export_to_markdown(page_break_placeholder=…)` (docling-core's
    /// `MarkdownParams`, docling-serve's `md_page_break_placeholder`). Where
    /// docling marks a break between two items whose `prov.page_no` differ,
    /// the serializer here marks it between two rendered blocks separated by a
    /// page boundary — the `PageInfo` marker opening every PDF page and sheet,
    /// the `PageBreak` between slides and DjVu / DocTags pages — so a break
    /// never leads or trails the document and empty pages collapse into one.
    /// `None` (the default) keeps Markdown free of page breaks, as docling's
    /// default export is. Markdown only; JSON carries pages in `prov`.
    pub fn page_break_placeholder(mut self, placeholder: Option<String>) -> Self {
        self.page_break_placeholder = placeholder;
        self
    }

    /// The document-level settings [`convert`](Self::convert) applies to
    /// every backend's output: the serializer knobs carried on the document
    /// (`strict`, `compact_tables`, `page_break_placeholder`) so each later
    /// Markdown export agrees, and first-class cells for every table. Public
    /// for callers that run a backend themselves — docling-serve converts
    /// PDFs and images on a warm `Pipeline` and finishes
    /// them here, so a request's options reach them as they reach every
    /// other format (#547).
    pub fn finish_document(&self, document: &mut docling_core::DoclingDocument) {
        // Carry the mode so `result.document.export_to_markdown()` reflects it.
        document.strict_markdown = self.strict;
        // Compact tables (#271) is additive: the PDF backend already turns it
        // on for its own corpus; never turn it back off here.
        if self.compact_tables {
            document.compact_tables = true;
        }
        // Alt text in Markdown/HTML (#676): additive like compact tables.
        if self.picture_alt_text {
            document.picture_alt_text = true;
        }
        // The metadata output mode (#679), likewise.
        if self.metadata_output.in_json() {
            document.metadata_output = self.metadata_output;
        }
        // Page-break placeholder: a serializer knob like `strict`, carried on
        // the document so every Markdown export of it agrees.
        if self.page_break_placeholder.is_some() {
            document.page_break_placeholder = self.page_break_placeholder.clone();
        }
        // First-class cells for every table (#240): backends with page
        // geometry (the PDF TableFormer paths) set them; everything else —
        // declarative tables included — derives them from the grid plus the
        // structure overlay (real spans for DOCX/XLSX merges, HTML `th`
        // headers, ODF covered cells; 1×1 records otherwise), so the repair
        // API and the JSON `table_cells` are populated uniformly.
        for table in document.tables_mut() {
            if table.cells.is_none() {
                table.cells = Some(table.derive_cells());
            }
        }
    }

    /// The picture enrichment pass: describe the pictures through the
    /// vision endpoint when [`picture_description`](Self::picture_description)
    /// is set (#670), OCR the embedded pictures when
    /// [`do_picture_ocr`](Self::do_picture_ocr) asks for it (#645), then
    /// drop the image bytes unless [`keep_picture_images`](Self::keep_picture_images)
    /// keeps them. Runs on every conversion after the backend, before
    /// [`finish_document`](Self::finish_document). Public for the same
    /// callers as `finish_document`: a PDF or image converted on a warm
    /// `Pipeline` (docling-serve, the CLI batch, the bindings' `Pipeline`)
    /// bypasses [`convert`](Self::convert), and without this pass
    /// `keep_picture_images(false)` and the picture description never
    /// reached it (#896). `reporter` is the conversion's
    /// ([`Self::reporter`]), still open — the pass reports its progress.
    pub fn enrich_pictures(
        &self,
        document: &mut docling_core::DoclingDocument,
        source: &SourceDocument,
        reporter: &ProgressReporter,
    ) {
        let format = source.format;
        #[cfg(feature = "vlm")]
        if let Some(opts) = &self.picture_description {
            self.describe_pictures(document, source, opts, reporter);
        }
        if self.picture_ocr {
            self.ocr_pictures(document, format, reporter);
        }
        if !self.keep_picture_images {
            strip_picture_images(document);
        }
    }

    /// The picture-OCR pass itself: PDF, image and METS pages already went
    /// through the OCR pipeline, so their pictures are left alone, and a
    /// video's frames were read as they were decoded
    /// ([`convert_video`](Self::convert_video)); with OCR disabled, or
    /// without the recognizer, the pictures keep no text and one warning
    /// says why. The same image embedded twice — a DOCX walked into both
    /// the flat nodes and the item tree, a slide master's logo on every
    /// slide — is read once (cached by its bytes).
    fn ocr_pictures(
        &self,
        document: &mut docling_core::DoclingDocument,
        format: InputFormat,
        reporter: &ProgressReporter,
    ) {
        use docling_core::{tree::TreeKind, Node};

        if matches!(
            format,
            InputFormat::Pdf | InputFormat::Image | InputFormat::MetsGbs | InputFormat::Video
        ) {
            return;
        }
        let Some(mut reader) = self.picture_reader() else {
            return;
        };
        // The pass's item count (#783): every picture it will read.
        let mut total = 0usize;
        document.for_each_picture_mut(&mut |node| {
            if let Node::Picture {
                image: Some(_),
                description,
                ..
            } = node
            {
                total += usize::from(undescribed(description));
            }
        });
        if let Some(tree) = document.tree.as_ref() {
            for item in &tree.items {
                if let TreeKind::Picture {
                    image: Some(_),
                    description,
                    ..
                } = &item.kind
                {
                    total += usize::from(undescribed(description));
                }
            }
        }
        if total == 0 {
            return;
        }
        const LABEL: &str = "picture OCR";
        reporter.enrichment_progress(EnrichmentStep::PictureDescription, LABEL, 0, total);
        let mut done = 0usize;
        document.for_each_picture_mut(&mut |node| {
            if let Node::Picture {
                image: Some(img),
                description,
                ..
            } = node
            {
                if undescribed(description) {
                    *description = reader.read(img);
                    done += 1;
                    reporter.enrichment_progress(
                        EnrichmentStep::PictureDescription,
                        LABEL,
                        done,
                        total,
                    );
                }
            }
        });
        if let Some(tree) = document.tree.as_mut() {
            for item in tree.items.iter_mut() {
                if let TreeKind::Picture {
                    image: Some(img),
                    description,
                    ..
                } = &mut item.kind
                {
                    if undescribed(description) {
                        *description = reader.read(img);
                        done += 1;
                        reporter.enrichment_progress(
                            EnrichmentStep::PictureDescription,
                            LABEL,
                            done,
                            total,
                        );
                    }
                }
            }
        }
    }

    /// The picture-description pass (#670). Two walks over the document:
    /// the first collects every undescribed picture that passes the size
    /// floor and the class filter, keyed by its bytes so a logo on every
    /// slide is asked about once; the requests run on `concurrency` threads;
    /// the second walk writes the answers back. In caption mode an image
    /// input, which the layout model turned into text and figures, is also
    /// described as a whole — the caption MarkItDown gives an image file.
    #[cfg(feature = "vlm")]
    fn describe_pictures(
        &self,
        document: &mut docling_core::DoclingDocument,
        source: &SourceDocument,
        opts: &crate::vlm::PictureDescriptionOptions,
        reporter: &ProgressReporter,
    ) {
        use std::collections::HashMap;
        use std::hash::{Hash, Hasher};

        use docling_core::{tree::TreeKind, Node, PictureDescription, PictureImage};

        fn key_of(data: &[u8]) -> u64 {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            data.hash(&mut h);
            h.finish()
        }

        // Walk 1: the candidates, in document order, each distinct image once.
        let mut order: Vec<u64> = Vec::new();
        let mut images: HashMap<u64, PictureImage> = HashMap::new();
        let mut collect = |img: &PictureImage, description: &Option<PictureDescription>| {
            if !undescribed(description) {
                return;
            }
            let key = key_of(&img.data);
            if let std::collections::hash_map::Entry::Vacant(e) = images.entry(key) {
                order.push(key);
                e.insert(img.clone());
            }
        };
        document.for_each_picture_mut(&mut |node| {
            if let Node::Picture {
                image: Some(img),
                description,
                ..
            } = node
            {
                collect(img, description);
            }
        });
        if let Some(tree) = document.tree.as_ref() {
            for item in &tree.items {
                if let TreeKind::Picture {
                    image: Some(img),
                    description,
                    ..
                } = &item.kind
                {
                    collect(img, description);
                }
            }
        }
        // An image input as a whole (caption mode): the layout model split
        // it into text and figures, but the file is the picture.
        let whole = (source.format == InputFormat::Image
            && opts.mode == crate::vlm::PictureDescriptionMode::Caption)
            .then(|| PictureImage {
                mimetype: image_mimetype(&source.bytes)
                    .unwrap_or("application/octet-stream")
                    .into(),
                width: 0,
                height: 0,
                data: source.bytes.clone(),
                dpi: PictureImage::DEFAULT_DPI,
                display_size: None,
            });
        if let Some(img) = &whole {
            collect(img, &None);
        }
        if order.is_empty() {
            return;
        }

        // The filters picture OCR applies (#645): the size floor on the
        // declared, then decoded, size; the classifier's top label when
        // classes are named.
        let min_side = self
            .picture_ocr_min_side
            .or_else(|| docling_core::env::parse::<u32>("DOCLING_RS_PICTURE_OCR_MIN_SIDE"))
            .unwrap_or(32);
        let mut classifier = (!self.picture_ocr_classes.is_empty())
            .then(|| docling_pdf::Pipeline::new().ok())
            .flatten()
            .map(|p| p.no_ocr(true).no_table_former(true));
        let mut jobs: Vec<(u64, String, Vec<u8>)> = Vec::new();
        for key in &order {
            let img = &images[key];
            if img.width.min(img.height) < min_side && img.width * img.height > 0 {
                continue;
            }
            // The endpoint takes PNG/JPEG/GIF/WebP data URIs; a BMP or TIFF
            // is re-encoded, a vector or Windows metafile picture skipped.
            let Some((mimetype, bytes)) = endpoint_image(img) else {
                docling_core::debug_log!(
                    "docling: picture description: {} is not an image the endpoint takes; skipped",
                    img.mimetype
                );
                continue;
            };
            if min_side > 0 || classifier.is_some() {
                let decoded = match image::load_from_memory(&bytes) {
                    Ok(d) => d.into_rgb8(),
                    Err(e) => {
                        docling_core::debug_log!(
                            "docling: picture description: undecodable image: {e}"
                        );
                        continue;
                    }
                };
                if decoded.width().min(decoded.height()) < min_side {
                    continue;
                }
                if let Some(p) = classifier.as_mut() {
                    if let Some(preds) = p.classify_picture(&decoded) {
                        let top = preds.first().map(|c| c.class_name.as_str());
                        if !top.is_some_and(|t| self.picture_ocr_classes.iter().any(|c| c == t)) {
                            continue;
                        }
                    }
                }
            }
            jobs.push((*key, mimetype.to_string(), bytes.into_owned()));
        }
        if jobs.is_empty() {
            return;
        }

        // The requests: `concurrency` workers pull from one queue; a
        // rejected picture is warned about and skipped, an unreachable
        // endpoint stops the queue — every later picture would wait out the
        // same retries for the same answer.
        let agent = crate::vlm::agent();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let stop = std::sync::atomic::AtomicBool::new(false);
        let answers: std::sync::Mutex<HashMap<u64, String>> = std::sync::Mutex::new(HashMap::new());
        let workers = opts.concurrency.max(1).min(jobs.len());
        // The pass's item count (#783): `0` first, one event per answered
        // request from the worker that got it, the total once the pass is
        // over — also when an unreachable endpoint stopped it early.
        const LABEL: &str = "picture description";
        let total = jobs.len();
        reporter.enrichment_progress(EnrichmentStep::PictureDescription, LABEL, 0, total);
        let finished = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((key, mimetype, bytes)) = jobs.get(i) else {
                        break;
                    };
                    let outcome = opts.describe(&agent, mimetype, bytes);
                    let done = finished.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    reporter.enrichment_progress(
                        EnrichmentStep::PictureDescription,
                        LABEL,
                        done.min(total),
                        total,
                    );
                    match outcome {
                        Ok(Some(text)) => {
                            answers.lock().unwrap().insert(*key, text);
                        }
                        Ok(None) => {}
                        Err(crate::vlm::RequestError::Rejected(e)) => {
                            eprintln!("warning: picture description: {e}; the picture keeps none");
                        }
                        Err(crate::vlm::RequestError::Unavailable(e)) => {
                            if !stop.swap(true, std::sync::atomic::Ordering::Relaxed) {
                                let left = jobs.len().saturating_sub(i);
                                eprintln!(
                                    "warning: picture description: {e}; {left} picture(s) keep no \
                                     description"
                                );
                            }
                        }
                    }
                });
            }
        });
        reporter.enrichment_progress(EnrichmentStep::PictureDescription, LABEL, total, total);
        let answers = answers.into_inner().unwrap();
        if answers.is_empty() {
            return;
        }

        // Walk 2: the answers onto every picture carrying those bytes.
        let provenance = opts.provenance();
        let described = |img: &PictureImage| -> Option<PictureDescription> {
            answers
                .get(&key_of(&img.data))
                .map(|text| PictureDescription {
                    text: text.clone(),
                    provenance: provenance.clone(),
                })
        };
        document.for_each_picture_mut(&mut |node| {
            if let Node::Picture {
                image: Some(img),
                description,
                ..
            } = node
            {
                if undescribed(description) {
                    *description = described(img);
                }
            }
        });
        if let Some(tree) = document.tree.as_mut() {
            for item in tree.items.iter_mut() {
                if let TreeKind::Picture {
                    image: Some(img),
                    description,
                    ..
                } = &mut item.kind
                {
                    if undescribed(description) {
                        *description = described(img);
                    }
                }
            }
        }
        if let Some(img) = whole {
            if let Some(description) = described(&img) {
                document.nodes.push(Node::Picture {
                    caption: None,
                    caption_href: None,
                    image: self.keep_picture_images.then_some(img),
                    classification: None,
                    description: Some(description),
                    caption_parent: Default::default(),
                    caption_location: None,
                });
            }
        }
    }

    /// The reader the picture-OCR pass (#645) and the video frame hook
    /// (#647) share: the OCR models only — no layout, no TableFormer, the
    /// caller already knows each whole image is the picture — behind the
    /// size floor and the classifier filter. `None` (after one warning)
    /// when OCR is disabled or the recognizer cannot load.
    #[cfg(feature = "pdf")]
    fn picture_reader(&self) -> Option<PictureReader> {
        static WARNED: std::sync::Once = std::sync::Once::new();
        if self.no_ocr || self.text_layer_only == TextLayerOnly::On {
            WARNED.call_once(|| {
                eprintln!(
                    "warning: picture OCR requested, but OCR is disabled (no_ocr / \
                     text_layer_only); pictures keep no text"
                );
            });
            return None;
        }
        let pipeline = docling_pdf::Pipeline::new().map(|p| {
            self.with_textline_orientation(
                p.no_ocr(true)
                    .no_table_former(true)
                    .ocr_lang(self.ocr_lang_choice())
                    .ocr_engine(self.ocr_engine_choice())
                    .tesseract_lang(self.tesseract_lang_choice())
                    .ocr_scale(self.ocr_scale_choice())
                    .picture_ocr_scale(self.picture_ocr_scale_choice()),
            )
        });
        let pipeline = match pipeline {
            Ok(p) => p,
            Err(e) => {
                WARNED.call_once(|| {
                    eprintln!("warning: picture OCR unavailable ({e}); pictures keep no text");
                });
                return None;
            }
        };
        let min_side = self
            .picture_ocr_min_side
            .or_else(|| docling_core::env::parse::<u32>("DOCLING_RS_PICTURE_OCR_MIN_SIDE"))
            .unwrap_or(32);
        Some(PictureReader {
            pipeline,
            min_side,
            classes: self.picture_ocr_classes.clone(),
            cache: std::collections::HashMap::new(),
        })
    }

    /// Without the `pdf` feature there is no OCR engine to run: one warning,
    /// the pictures keep no text.
    #[cfg(not(feature = "pdf"))]
    fn picture_reader(&self) -> Option<PictureReader> {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            eprintln!(
                "warning: picture OCR requested, but this build has no OCR engine (rebuild \
                 with the `pdf` feature); pictures keep no text"
            );
        });
        None
    }

    /// The frame sampling a video gets (#647): the builder's settings, else
    /// the environment's, else the defaults.
    #[cfg(feature = "asr")]
    fn frame_options(&self) -> crate::video::FrameOptions {
        use docling_core::env;
        crate::video::FrameOptions {
            max_frames: self.video_frames.unwrap_or(DEFAULT_VIDEO_FRAMES),
            scene_threshold: self
                .video_scene_threshold
                .or_else(|| env::parse::<f32>("DOCLING_RS_VIDEO_SCENE_THRESHOLD"))
                .map_or(crate::video::DEFAULT_SCENE_THRESHOLD, |t| t.clamp(0.0, 1.0)),
            max_side: self
                .video_frame_max_side
                .or_else(|| env::parse::<u32>("DOCLING_RS_VIDEO_FRAME_MAX_SIDE"))
                .unwrap_or(0),
            dedupe: self
                .video_frame_dedupe
                .or_else(|| env::parse::<u32>("DOCLING_RS_VIDEO_FRAME_DEDUPE"))
                .map(|d| d.min(64)),
        }
    }

    /// Video (#138): the audio track transcribes through the ASR path
    /// (Phase 1), and — when the ffmpeg binary is available — sampled
    /// frames interleave with the transcript as timestamped pictures
    /// (Phase 2). Without ffmpeg: transcript only. Each frame goes through
    /// the picture enrichment as soon as it is decoded (#647): read by the
    /// OCR models when [`do_picture_ocr`](Self::do_picture_ocr) asks, its
    /// bytes dropped when [`keep_picture_images`](Self::keep_picture_images)
    /// is off — so however many frames are sampled, one is in memory at a
    /// time. The models load on the first frame; a video without one costs
    /// nothing.
    #[cfg(feature = "asr")]
    fn convert_video(
        &self,
        source: &SourceDocument,
    ) -> Result<docling_core::DoclingDocument, String> {
        use docling_core::Node;
        let opts = self.frame_options();
        let mut reader: Option<Option<PictureReader>> = None;
        let mut on_frame = |node: &mut Node| {
            if self.picture_ocr {
                let reader = reader.get_or_insert_with(|| self.picture_reader());
                if let (
                    Some(reader),
                    Node::Picture {
                        image: Some(img),
                        description,
                        ..
                    },
                ) = (reader.as_mut(), &mut *node)
                {
                    if undescribed(description) {
                        *description = reader.read(img);
                    }
                }
            }
            if !self.keep_picture_images {
                if let Node::Picture { image, .. } = node {
                    *image = None;
                }
            }
        };
        crate::video::convert_video(
            &source.bytes,
            &source.name,
            self.asr_model.as_deref(),
            self.asr_lang.as_deref(),
            &opts,
            &mut on_frame,
        )
    }

    /// The copybook layout for EBCDIC sources (#252): docling's
    /// `EbcdicLayout` JSON, inline (a string starting with `{`) or as a file
    /// path. Without it, a path-loaded source looks for a
    /// `<stem>.layout.json` sidecar; converting EBCDIC with neither is an
    /// error — the bytes are meaningless without their copybook.
    pub fn ebcdic_layout(mut self, layout: impl Into<String>) -> Self {
        self.ebcdic_layout = Some(layout.into());
        self
    }

    /// Option-typed variant of [`ebcdic_layout`](Self::ebcdic_layout) for
    /// call sites plumbing an optional flag through (`None` keeps the
    /// sidecar fallback).
    pub fn ebcdic_layout_opt(mut self, layout: Option<String>) -> Self {
        self.ebcdic_layout = layout;
        self
    }

    /// Skip loading and running the TableFormer table-structure model for
    /// PDF/image/METS sources.
    ///
    /// Off by default. When enabled, table regions are still detected and
    /// emitted, but their structure is reconstructed geometrically from cell
    /// positions instead of the ONNX model's predicted structure — no model
    /// load and no per-table inference, at the cost of table fidelity. Useful
    /// when parsing speed matters more than exact table structure, especially
    /// with [`convert_streaming`](Self::convert_streaming).
    pub fn no_table_former(mut self, disable: bool) -> Self {
        self.no_table_former = disable;
        self
    }

    /// PDF/image: keep every detected picture as a picture — disable the
    /// text-panel demotion that turns an uncaptioned, dense text-panel
    /// "picture" into paragraphs (#157). The escape hatch for
    /// image-extraction workflows and for charts the heuristic might still
    /// misjudge on scanned pages (#173).
    pub fn no_text_panels(mut self, disable: bool) -> Self {
        self.no_text_panels = disable;
        self
    }

    /// Infer PDF/image section-header levels after assembly (#302, docling's
    /// `HeadingHierarchyModel` with its default options): the PDF outline
    /// (bookmarks) is authoritative, legal/outline numbering covers headings
    /// without a bookmark match, and font size/weight/slant/case rank the
    /// rest. Off by default (docling parity): every detected heading then
    /// keeps the flat level the assembler emits. Non-PDF/image formats
    /// ignore it (their backends carry real heading levels already).
    pub fn heading_hierarchy(mut self, enable: bool) -> Self {
        self.heading_hierarchy = enable;
        self
    }

    /// Skip layout detection, OCR, and TableFormer entirely for PDF/image/METS
    /// sources — no model load, no inference of any kind (#611; this was
    /// `no_ocr` before 2.0).
    ///
    /// Off by default. When enabled, the PDF's embedded text cells are grouped by
    /// line and emitted as plain paragraphs in reading order: no headings, lists,
    /// tables, code blocks, or pictures, since that structure comes from the
    /// layout model. The fastest possible PDF path, but pages with no embedded
    /// text layer (scanned/image-only PDFs) yield no text at all — convert those
    /// without this flag. Implies [`no_table_former`](Self::no_table_former).
    ///
    /// Takes a `bool`, or [`TextLayerOnly::Auto`]: per document, the text
    /// layer alone when it carries the document (no model loaded), the full
    /// pipeline — OCR included — when it does not (a scanned / image-only
    /// file, or a vestigial layer of a few typed-in form fields over scans;
    /// `docling_pdf::text_layer_has_text`). The verdict is the whole
    /// document's, decided before any model is touched: a digital file with
    /// a few scanned pages inserted keeps the text path, and those pages
    /// come back empty as under `true`. For an SVG, `auto` is the native
    /// `<text>` extraction when the file has text and the ML image route
    /// when it has none; a DjVu already falls back to OCR by itself, which
    /// `true` alone disables.
    pub fn text_layer_only(mut self, mode: impl Into<TextLayerOnly>) -> Self {
        self.text_layer_only = mode.into();
        self
    }

    /// Never run OCR, but keep layout detection and TableFormer — docling's
    /// `do_ocr=False`, its CLI's `--no-ocr` (#244; since 2.0, #611, the
    /// meaning of `no_ocr` too). Unlike
    /// [`text_layer_only`](Self::text_layer_only) (the skip-everything fast
    /// path), structured output — headings, tables, pictures, reading order —
    /// is preserved; only text that exists solely as pixels is lost (scanned
    /// pages come back with empty regions, and the speculative OCR of large
    /// embedded images never runs). The OCR model is never loaded, and
    /// independently of this flag a *missing* OCR model degrades to the same
    /// behavior with a warning instead of failing the conversion. SVG inputs
    /// route to direct `<text>` extraction (their text is native — skipping
    /// OCR must not lose it).
    pub fn no_ocr(mut self, disable: bool) -> Self {
        self.no_ocr = disable;
        self
    }

    /// [`no_ocr`](Self::no_ocr) under its pre-2.0 name, kept as an alias.
    pub fn skip_ocr(self, disable: bool) -> Self {
        self.no_ocr(disable)
    }

    /// The password of an encrypted document: a PDF (docling's
    /// `--pdf-password`, #611) or an Office document (#625, a docling.rs
    /// extension — docling reads none): `.docx`/`.xlsx`/`.pptx` (Agile and
    /// Standard encryption), `.doc`/`.xls`/`.ppt` (RC4 and RC4 CryptoAPI).
    ///
    /// A missing or wrong PDF password fails the conversion with "the PDF is
    /// encrypted: a password is required"; PDF passwords are read on the ML
    /// pipeline only (the `pdf-text` / wasm build reads unencrypted PDFs).
    /// Office documents decrypt in every build. Without a password, or when
    /// it is wrong, the format's default password is tried (Excel's
    /// `VelvetSweatshop`, PowerPoint's for modify-password-only files);
    /// otherwise the error says the document is encrypted, or that the
    /// password is wrong.
    pub fn password(mut self, password: Option<String>) -> Self {
        self.password = password;
        self
    }

    /// [`password`](Self::password) under docling's PDF-only name.
    pub fn pdf_password(self, password: Option<String>) -> Self {
        self.password(password)
    }

    /// OCR every PDF page from its rendered image even when the page carries
    /// an embedded text layer — docling's `force_full_page_ocr`. The escape
    /// hatch for text layers that exist but lie (broken encodings, subset
    /// fonts with garbage mappings, scanned forms with a few typed-in
    /// fields). Off by default; ignored when [`no_ocr`](Self::no_ocr) or
    /// [`text_layer_only`](Self::text_layer_only) is set,
    /// mirroring docling, where it is a sub-option of `do_ocr`. Applies to
    /// PDFs only — standalone images are always OCR'd.
    pub fn force_full_page_ocr(mut self, force: bool) -> Self {
        self.force_full_page_ocr = force;
        self
    }

    /// Replace a broken PDF text layer with OCR, page by page — docling's
    /// `replace_broken_text_layer` (docling#4654, #1006). A page whose text
    /// layer a broken font mapping produced (control, U+FFFD or private-use
    /// characters; Latin letters standing for another script; too few
    /// vowels — [`docling_pdf::quality::is_broken_text_layer`]) is read with
    /// full-page OCR and its `parse_score` drops to 0; every other page
    /// keeps its text layer. The targeted alternative to
    /// [`force_full_page_ocr`](Self::force_full_page_ocr). Off by default;
    /// ignored with [`no_ocr`](Self::no_ocr) or a missing OCR model (the
    /// page keeps its layer).
    pub fn replace_broken_text_layer(mut self, replace: bool) -> Self {
        self.replace_broken_text_layer = replace;
        self
    }

    /// Which document regions feed the OCR — docling's `OcrMode` (#254):
    /// `default`, `full_page`, `layout_regions`, or
    /// `pdf_aware_layout_regions`. The default is the text-layer-aware
    /// behavior (docling's `pdf_aware_layout_regions`);
    /// `full_page`/`layout_regions` discard the text layer like
    /// [`force_full_page_ocr`](Self::force_full_page_ocr) — see
    /// [`docling_pdf::OcrMode`] for the mapping. An unknown value warns at
    /// conversion time and uses the default. PDF/image ML pipeline only.
    pub fn ocr_mode(mut self, mode: impl Into<String>) -> Self {
        self.ocr_mode = Some(mode.into());
        self
    }

    /// Which OCR engine recognizes text on scanned pages (#460): `"ppocr"`
    /// (the default — the built-in PP-OCRv3 recognizer, the conformance
    /// engine) or `"tesseract"` (the system `tesseract` binary, docling's
    /// `TesseractCliOcrOptions`; needs `tesseract-ocr` with a language pack
    /// installed — `DOCLING_TESSERACT` names the binary,
    /// `DOCLING_RS_TESSERACT_PSM` its page segmentation mode,
    /// `DOCLING_RS_TESSDATA_DIR` / `TESSDATA_PREFIX` its data). Both read
    /// the same layout-region crops; [`ocr_lang`](Self::ocr_lang) is the
    /// engine's language. An unknown value warns at conversion time and uses
    /// the default (`DOCLING_RS_OCR_ENGINE`, else PP-OCR). Formats that never
    /// OCR ignore it.
    pub fn ocr_engine(mut self, engine: impl Into<String>) -> Self {
        self.ocr_engine = Some(engine.into());
        self
    }

    /// OCR render scale in pixels per PDF point — docling's
    /// `OcrOptions.scale` (#254; docling's default 3 = 216 dpi). Unset feeds
    /// the recognizer the pipeline's own 2.0 px/pt page render; a different
    /// value resamples that render for the OCR input only, leaving layout and
    /// TableFormer pixels untouched. Non-positive values warn at conversion
    /// time and are ignored. PDF/image ML pipeline only.
    pub fn ocr_scale(mut self, scale: f32) -> Self {
        self.ocr_scale = Some(scale);
        self
    }

    /// How a PDF's text layer splits words where the file draws no space
    /// glyph, only a cursor move (#921). `adaptive` (the default; also
    /// `DOCLING_RS_PDF_WORD_GAP`) is docling-parse ≥ 7's rule, what docling
    /// itself runs on: each gap over the mean advance of the two glyphs
    /// flanking it, against a line-local two-cluster threshold (clamped to
    /// 0.18–0.35). A number `f` in (0, 4] is a fixed threshold for that
    /// ratio on lines without space glyphs (0.1–0.23 suits most body text;
    /// a line that draws its spaces keeps docling-parse's 0.35 floor). Both keep letter-spaced
    /// (Sperrsatz) words whole — equal cursor moves between letters are
    /// tracking, where docling-parse 7 cuts mid-word. `legacy` is
    /// docling-parse ≤ 6's rule: a gap separates words when it exceeds 0.33 ×
    /// the running mean advance of the cell built so far, and cells restart
    /// at every font change — so the threshold drifts along a line. All
    /// apply to the exported line text and the word cells. An unknown value
    /// warns at conversion time and uses the default. PDF text layer only.
    pub fn pdf_word_gap(mut self, gap: impl Into<String>) -> Self {
        self.pdf_word_gap = Some(gap.into());
        self
    }

    /// Keep a PDF's typographic punctuation as drawn — `„ “ ‚ ‘`, curly
    /// quotes, en/em dashes, `…`, `⁄`, `•` — instead of folding it to ASCII
    /// the way docling-parse's sanitizer does (#921; also
    /// `DOCLING_RS_PDF_KEEP_TYPOGRAPHY`). Folding is the default, for docling
    /// parity; legal citations and quoted terms lose the difference between
    /// `„…“`, `‚…‘` and ASCII quotes there. PDF text layer only.
    pub fn pdf_keep_typography(mut self, keep: bool) -> Self {
        self.pdf_keep_typography = Some(keep);
        self
    }

    /// Report what the PDF reader repaired to read the input (#934): the
    /// result's [`pdf_repairs`](crate::ConversionResult::pdf_repairs) lists
    /// every stream whose declared `/Length` does not end at its
    /// `endstream` (with the declared and the recovered length) and a
    /// padded cross-reference table — empty for a well-formed file — so a
    /// caller can refuse recovered input. Off by default (also
    /// `DOCLING_RS_PDF_REPORT_REPAIRS`): the reader repairs silently, and
    /// the check re-reads the file (one linear pass plus one parse). The
    /// conversion itself is unchanged either way. PDF input on the standard
    /// and the text-layer pipelines; `docling_pdf::pdf_repairs` is the same
    /// check without a conversion.
    pub fn pdf_report_repairs(mut self, report: bool) -> Self {
        self.pdf_report_repairs = Some(report);
        self
    }

    /// Whether this converter reports the PDF reader's repairs (#934) —
    /// [`Self::pdf_report_repairs`], else `DOCLING_RS_PDF_REPORT_REPAIRS`.
    /// For callers that stream (no `ConversionResult` to carry them) and
    /// run `pdf_repairs` themselves.
    pub fn reports_pdf_repairs(&self) -> bool {
        self.pdf_report_repairs
            .unwrap_or_else(|| docling_core::env::flag("DOCLING_RS_PDF_REPORT_REPAIRS"))
    }

    /// The text-layer settings of a text-layer-only conversion (#921).
    #[cfg(all(feature = "pdf-text", not(feature = "pdf")))]
    fn pdf_text_options(&self) -> docling_pdf::TextOptions {
        let env = docling_pdf::TextOptions::from_env();
        docling_pdf::TextOptions {
            word_gap: self.pdf_word_gap_choice(),
            keep_typography: self.pdf_keep_typography.unwrap_or(env.keep_typography),
            ..env
        }
    }

    /// RapidOCR's handling of text-line orientation (#666), off by default
    /// (`DOCLING_RS_OCR_TEXTLINE_ORIENTATION` sets the process default):
    /// detected lines are cropped along the detector's rotated rectangle, a
    /// crop at least 1.5× taller than wide is turned 90°, and the
    /// PP-LCNet text-line orientation classifier
    /// (`.models/ocr_textline_ori.onnx`) turns upside-down lines — so
    /// vertical axis titles, rotated headers, skewed and upside-down lines
    /// read as text. Applies to the PP-OCR lines of scanned pages, images
    /// and pictures alike (changing their output when on); table words and
    /// Tesseract are untouched. A missing classifier warns once and keeps
    /// the crop and the 90° turn.
    pub fn ocr_textline_orientation(mut self, enable: bool) -> Self {
        self.ocr_textline_orientation = Some(enable);
        self
    }

    /// Pixels per PDF point for picture crops and page images — docling's
    /// `images_scale` (#520). Unset delivers crops at the pipeline's own 2.0
    /// px/pt render (144 dpi), as before; another value resamples it (above
    /// 2.0 that upsamples, it does not re-render). The picture's `dpi` in the
    /// JSON export is 72·scale either way (#519). Non-finite or non-positive
    /// values are ignored. PDF/image ML pipeline only.
    pub fn images_scale(mut self, scale: f32) -> Self {
        self.images_scale = Some(scale).filter(|s| s.is_finite() && *s > 0.0);
        self
    }

    /// Keep every page's render as a page image — docling's
    /// `generate_page_images` (#520): the JSON export carries it as
    /// `pages[n].image` at [`Self::images_scale`], which docling-core's
    /// `TableItem.get_image` / `FormulaItem.get_image` crop from. Off by
    /// default (one full-page PNG per page in memory). PDF/image ML pipeline
    /// only; text-layer-only (`text_layer_only`) and streaming conversions get none.
    pub fn generate_page_images(mut self, enabled: bool) -> Self {
        self.generate_page_images = enabled;
        self
    }

    /// Give a PDF picture its embedded image instead of the render crop
    /// when the picture's region holds exactly one image XObject (mutual
    /// intersection-over-self ≥ 0.8) with nothing painted over it — docling's
    /// `use_embedded_picture_images` (#1007, docling#5143). The bitmap keeps
    /// its native pixel size (`dpi` 72, not [`Self::images_scale`]); a
    /// masked, transparent, clipped, rotated or overlapped image, a second
    /// image or text in the box, or a `/Rotate`d page keep the crop. Off by
    /// default; layout, OCR and page images are unaffected. PDF ML pipeline
    /// only.
    pub fn use_embedded_picture_images(mut self, enabled: bool) -> Self {
        self.use_embedded_picture_images = enabled;
        self
    }

    /// Classify each detected picture with the DocumentFigureClassifier model
    /// (docling's `do_picture_classification`). Off by default.
    ///
    /// The full 26-class prediction distribution (bar_chart, logo, signature,
    /// …) lands on the picture item and is serialized into the docling JSON as
    /// the `classification` annotation plus the `meta.classification` field.
    /// Markdown output is unaffected. Needs `.models/picture_classifier.onnx`
    /// (fetched by `scripts/install/download_dependencies.sh`); a missing
    /// model warns once and skips classification.
    pub fn do_picture_classification(mut self, enable: bool) -> Self {
        self.enrich.picture_classification = enable;
        self
    }

    /// OCR the pictures embedded in non-PDF documents (#645) — a DOCX/PPTX
    /// screenshot, an HTML figure, a sampled video frame — with the ML
    /// pipeline's OCR models (the PP-OCR recognizer + the `ocr_det.onnx`
    /// line detector when installed, or Tesseract under
    /// [`ocr_engine`](Self::ocr_engine)), and attach the text to the picture
    /// as docling's `PictureDescriptionData` annotation: Markdown prints it
    /// between the caption and the image placeholder, JSON/DCLX carry it as
    /// `meta.description` + the `description` annotation, the hybrid chunker
    /// puts it in the picture's chunk. Off by default — every default export
    /// is unchanged. Nothing is attached to a picture the engine reads no
    /// text from (lines under `DOCLING_RS_OCR_TEXT_SCORE` are dropped as on a
    /// scanned page). PDF, image and METS inputs are untouched: their pages
    /// go through the OCR pipeline already. With [`no_ocr`](Self::no_ocr) /
    /// [`text_layer_only`](Self::text_layer_only), or without the OCR model
    /// (or the `pdf` feature), one warning and no text. See also
    /// [`picture_ocr_classes`](Self::picture_ocr_classes),
    /// [`picture_ocr_min_side`](Self::picture_ocr_min_side) and
    /// [`keep_picture_images`](Self::keep_picture_images).
    pub fn do_picture_ocr(mut self, enable: bool) -> Self {
        self.picture_ocr = enable;
        self
    }

    /// Only OCR the pictures the DocumentFigureClassifier labels as one of
    /// `labels` (its 26 classes: `screenshot_from_computer`,
    /// `screenshot_from_manual`, `logo`, `photograph`, …), judged by its top
    /// prediction — the way to skip photos and logos on a slide deck while
    /// reading its screenshots. Empty (the default) OCRs every picture that
    /// passes the size floor. Needs `.models/picture_classifier.onnx`; a
    /// missing model warns once and the filter is waived.
    pub fn picture_ocr_classes<I, S>(mut self, labels: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.picture_ocr_classes = labels
            .into_iter()
            .map(|s| s.into().trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        self
    }

    /// The smallest side, in pixels, a picture must have to be OCR'd:
    /// bullets, icons and rules are skipped without decoding or inference.
    /// Default 32 (`DOCLING_RS_PICTURE_OCR_MIN_SIDE` overrides it); `0`
    /// reads everything.
    pub fn picture_ocr_min_side(mut self, px: u32) -> Self {
        self.picture_ocr_min_side = Some(px);
        self
    }

    /// The picture OCR's resolution (#666), in pixels per picture pixel
    /// before RapidOCR's 2000 px longer-side cap: `1.0` reads a picture at
    /// its native pixels, as RapidOCR does, shrinking only what exceeds
    /// 2000 px. Unset (`DOCLING_RS_PICTURE_OCR_SCALE`, else the image-input
    /// rule) reads it like a scanned image — [`ocr_scale`](Self::ocr_scale)
    /// when set, else 3 px/pt under the cap, which upscales a 720 × 180
    /// screenshot to 2000 × 500. Scanned pages and image inputs are
    /// unaffected. Non-positive values warn and are ignored.
    pub fn picture_ocr_scale(mut self, scale: f32) -> Self {
        self.picture_ocr_scale = Some(scale);
        self
    }

    /// Describe every picture through an OpenAI-compatible vision endpoint
    /// (#670) — MarkItDown's `llm_client` / `llm_model` / `llm_prompt` and
    /// its markitdown-ocr plugin, for every format: the embedded pictures of
    /// DOCX/PPTX/XLSX/HTML, a PDF's or scan's figure crops, sampled video
    /// frames, and (in caption mode) an image input as a whole. The answer
    /// becomes the picture's description annotation, where
    /// [`do_picture_ocr`](Self::do_picture_ocr) puts its text: Markdown
    /// prints it after the caption, JSON carries `meta.description` with the
    /// model as `created_by`. [`picture_ocr_min_side`](Self::picture_ocr_min_side)
    /// and [`picture_ocr_classes`](Self::picture_ocr_classes) select the
    /// pictures here too; identical image bytes are sent once per document;
    /// `concurrency` pictures are in flight at once. An endpoint that rejects
    /// a picture warns and the rest go on; one that cannot be reached warns
    /// once and the remaining pictures keep no description — the conversion
    /// never fails over it. Runs before the local picture OCR, which then
    /// reads only what stayed undescribed.
    #[cfg(feature = "vlm")]
    pub fn picture_description(mut self, opts: crate::vlm::PictureDescriptionOptions) -> Self {
        self.picture_description = Some(opts);
        self
    }

    /// Whether the pictures keep their embedded image bytes (the default,
    /// `true`): `false` drops them from every picture after the enrichment
    /// pass, so a document whose images were only there to be read — a slide
    /// deck of screenshots, the sampled frames of a video — exports a slim
    /// JSON/DCLX and a placeholder-only Markdown while keeping the OCR text.
    pub fn keep_picture_images(mut self, keep: bool) -> Self {
        self.keep_picture_images = keep;
        self
    }

    /// Redact personal data from every converted document (#621): after
    /// the backend builds the [`DoclingDocument`](docling_core::DoclingDocument)
    /// and before any export, chunker or stream reads it, every detected
    /// span — e-mail, phone, card number, IBAN, IP, URL credentials,
    /// national IDs, and names / organizations / locations when the NER
    /// model is installed (`.models/ner/`, the `ner` feature) — is replaced
    /// in place by a placeholder (`[EMAIL]`, `[PERSON_2]` with
    /// [`Replacement::Pseudonym`](docling_core::Replacement::Pseudonym), or
    /// a fixed string), in the flat nodes, the item tree behind the JSON,
    /// the link table, captions, hrefs, code, comments, table cells. Images
    /// are dropped by default
    /// ([`ImageRedaction`](docling_core::ImageRedaction)); `BoxOut` paints
    /// over the OCR'd lines that carry a span, `Keep` leaves them. The
    /// result's `redaction` is the [`RedactionReport`](docling_core::RedactionReport)
    /// — counts per label, never the values unless `return_mapping` asks.
    /// Off by default; unused, the output is byte-identical. A docling.rs
    /// extension — Python docling has no redaction stage — and not a
    /// compliance guarantee: pattern and model recall are what they are.
    pub fn redact_pii(mut self, opts: docling_core::RedactionOptions) -> Self {
        self.redact = Some(opts);
        self
    }

    /// Option-typed variant of [`redact_pii`](Self::redact_pii): `None`
    /// turns the pass off.
    pub fn redact_pii_opt(mut self, opts: Option<docling_core::RedactionOptions>) -> Self {
        self.redact = opts;
        self
    }

    /// Run the PII pass (#621) this converter is configured with over a
    /// document built elsewhere — docling-serve's warm pipeline and the
    /// VLM pipeline bypass [`convert`](Self::convert) — and return its
    /// report; `Ok(None)` when [`redact_pii`](Self::redact_pii) is unset.
    pub fn redact(
        &self,
        document: &mut docling_core::DoclingDocument,
    ) -> Result<Option<docling_core::RedactionReport>, ConversionError> {
        match self.redact.as_ref() {
            Some(opts) => self.redact_document(document, opts).map(Some),
            None => Ok(None),
        }
    }

    /// The PII pass over a converted document (#621): the detectors from
    /// the options, the OCR pipeline for `BoxOut` when this converter may
    /// run OCR (a `no_ocr` / `text_layer_only` converter drops the images
    /// instead, with the warning), then the walk. An invalid custom regex
    /// is a `Parse` error — the conversion cannot honour the request.
    fn redact_document(
        &self,
        document: &mut docling_core::DoclingDocument,
        opts: &docling_core::RedactionOptions,
    ) -> Result<docling_core::RedactionReport, ConversionError> {
        let detector = crate::redact::detector(opts)?;
        #[cfg(feature = "pdf")]
        {
            let mut ocr = None;
            if opts.images == docling_core::ImageRedaction::BoxOut
                && !self.no_ocr
                && self.text_layer_only != TextLayerOnly::On
            {
                // The OCR models only — no layout, no TableFormer.
                match docling_pdf::Pipeline::new() {
                    Ok(p) => {
                        ocr = Some(
                            self.with_textline_orientation(
                                p.no_ocr(true)
                                    .no_table_former(true)
                                    .ocr_lang(self.ocr_lang_choice())
                                    .ocr_engine(self.ocr_engine_choice())
                                    .tesseract_lang(self.tesseract_lang_choice())
                                    .ocr_scale(self.ocr_scale_choice()),
                            ),
                        )
                    }
                    Err(e) => crate::redact::warn_box_out_unavailable(&e.to_string()),
                }
            }
            Ok(crate::redact::run(document, opts, &detector, ocr.as_mut()))
        }
        #[cfg(not(feature = "pdf"))]
        {
            Ok(crate::redact::run(document, opts, &detector))
        }
    }

    /// Rewrite detected code blocks with the CodeFormulaV2 VLM (docling's
    /// `do_code_enrichment`). Off by default.
    ///
    /// The model re-reads the code crop at ~120 dpi, emits the clean source
    /// text (line breaks included) and identifies the language, which lands in
    /// the JSON `code_language` field. Needs the `.models/code_formula/` graphs
    /// (fetched by `scripts/install/download_dependencies.sh`); a missing
    /// model warns once and leaves the block as extracted.
    pub fn do_code_enrichment(mut self, enable: bool) -> Self {
        self.enrich.code = enable;
        self
    }

    /// Decode display formulas to LaTeX with the CodeFormulaV2 VLM (docling's
    /// `do_formula_enrichment`). Off by default.
    ///
    /// An enriched formula renders as `$$latex$$` in Markdown and as a
    /// `formula` text item in the JSON, replacing the
    /// `<!-- formula-not-decoded -->` placeholder. Same model artifacts as
    /// [`do_code_enrichment`](Self::do_code_enrichment).
    pub fn do_formula_enrichment(mut self, enable: bool) -> Self {
        self.enrich.formula = enable;
        self
    }

    /// Pre-render HTML-routing input in a headless browser before parsing.
    ///
    /// Off by default. When enabled, HTML sources — and MHTML/EPUB, which
    /// assemble HTML from their archives — are loaded in the system Chromium
    /// (driven from Rust over the DevTools protocol — no Node/Playwright) so the
    /// CSS cascade is resolved: elements the browser computes as `display:none`
    /// (e.g. a stylesheet-collapsed nav menu) are removed before the normal HTML
    /// backend runs. This is the one behaviour a pure-Rust parse can't reproduce;
    /// everything else (structure, tables, KVP, formatting) is still handled in
    /// Rust on the cleaned HTML.
    ///
    /// Requires the crate's `web-browser` Cargo feature; without it, converting
    /// an HTML source with this enabled returns [`ConversionError::Browser`].
    pub fn use_web_browser(mut self, enable: bool) -> Self {
        self.use_web_browser = enable;
        self
    }

    /// Return `html` unchanged, or — when [`use_web_browser`](Self::use_web_browser)
    /// is on — its headless-browser-cleaned form (computed-hidden elements
    /// removed). Borrows in the common (disabled) case; only allocates when the
    /// browser actually runs.
    fn maybe_prerender<'a>(
        &self,
        html: &'a str,
    ) -> Result<std::borrow::Cow<'a, str>, ConversionError> {
        crate::backend::maybe_prerender_html(html, self.use_web_browser)
    }

    /// Convert a source document to Markdown **incrementally**, returning an
    /// iterator of Markdown chunks (with picture placeholders).
    ///
    /// Concatenating every `Ok` chunk reproduces
    /// [`convert`](Self::convert)`(...).document.export_to_markdown()`
    /// byte-for-byte. The win is for PDF, whose pages are processed in parallel:
    /// each page's Markdown is emitted in document order as soon as it is ready, so
    /// output starts before the whole document is converted. Other formats build
    /// their document up front and stream it through the same interface.
    ///
    /// Streaming is Markdown-only — JSON needs the whole node tree, so there is no
    /// streaming JSON. The conversion runs on a background thread; dropping the
    /// returned [`MarkdownStream`] cancels it.
    #[cfg(feature = "pdf")]
    pub fn convert_streaming(
        &self,
        source: SourceDocument,
    ) -> Result<MarkdownStream, ConversionError> {
        self.convert_streaming_images(source, ImageMode::Placeholder)
    }

    /// Like [`convert_streaming`](Self::convert_streaming) but with an explicit
    /// picture [`ImageMode`].
    ///
    /// [`ImageMode::Referenced`] streams too (issue #80): each page's images
    /// are written to [`artifacts_dir`](Self::artifacts_dir) *as the page's
    /// Markdown is emitted* and dropped from memory, so an image-heavy PDF
    /// holds ~one page of images at a time instead of all of them until
    /// export. The chunks and files match the buffered
    /// `export_to_markdown_with_images(ImageMode::Referenced, ..)` output.
    #[cfg(feature = "pdf")]
    pub fn convert_streaming_images(
        &self,
        source: SourceDocument,
        image_mode: ImageMode,
    ) -> Result<MarkdownStream, ConversionError> {
        self.convert_streaming_with_options(
            source,
            &MarkdownExportOptions {
                image_mode,
                artifacts_dir: self.artifacts_dir.clone(),
                ..MarkdownExportOptions::default()
            },
        )
    }

    /// Like [`convert_streaming_images`](Self::convert_streaming_images) but
    /// with every [`MarkdownExportOptions`] setting (#599): content layers,
    /// picture traversal, escaping, the image placeholder. Referenced images
    /// go under `options.artifacts_dir` (not the converter's
    /// [`artifacts_dir`](Self::artifacts_dir)). The concatenated chunks match
    /// the buffered `export_to_markdown_with_options(options)` byte-for-byte.
    #[cfg(feature = "pdf")]
    pub fn convert_streaming_with_options(
        &self,
        source: SourceDocument,
        options: &MarkdownExportOptions,
    ) -> Result<MarkdownStream, ConversionError> {
        if let Some(allowed) = &self.allowed_formats {
            if !allowed.contains(&source.format) {
                return Err(ConversionError::UnsupportedFormat(source.format));
            }
        }
        let source = self.with_encoding(source);
        Ok(crate::stream::spawn(self.clone(), source, options.clone()))
    }

    /// Whether the heading-hierarchy stage (#302) is enabled — the streaming
    /// front-end buffers PDF conversions when it is (the stage needs the whole
    /// assembled document, and streamed output must stay byte-identical to
    /// buffered output).
    #[cfg(feature = "pdf")]
    pub(crate) fn heading_hierarchy_enabled(&self) -> bool {
        self.heading_hierarchy
    }

    /// Streaming internals ([`crate::stream`]) read the producer's settings
    /// off the converter clone they receive.
    #[cfg(feature = "pdf")]
    pub(crate) fn stream_settings(&self) -> crate::stream::StreamSettings {
        crate::stream::StreamSettings {
            strict: self.strict,
            no_table_former: self.no_table_former,
            no_text_panels: self.no_text_panels,
            text_layer_only: self.text_layer_only,
            no_ocr: self.no_ocr,
            password: self.password.clone(),
            force_full_page_ocr: self.force_full_page_ocr,
            replace_broken_text_layer: self.replace_broken_text_layer,
            enrich: self.enrich,
            page_range: self.page_range,
            ocr_lang: self.ocr_lang_choice(),
            ocr_engine: self.ocr_engine_choice(),
            tesseract_lang: self.tesseract_lang_choice(),
            ocr_mode: self.ocr_mode_choice(),
            ocr_scale: self.ocr_scale_choice(),
            word_gap: self.pdf_word_gap_choice(),
            keep_typography: self.pdf_keep_typography,
            ocr_textline_orientation: self.ocr_textline_orientation,
            images_scale: self.images_scale,
            embedded_pictures: self.use_embedded_picture_images,
            keep_picture_images: self.keep_picture_images,
            page_break_placeholder: self.page_break_placeholder.clone(),
            compact_tables: self.compact_tables,
            picture_alt_text: self.picture_alt_text,
            metadata_output: self.metadata_output,
            document_timeout: self.document_timeout,
            redact: self.redact.clone(),
        }
    }

    /// Convert a single source document.
    ///
    /// The source converts as its declared format (the extension's). Only if
    /// that fails is the content inspected: when it is evidently another
    /// format — RTF, an OOXML / ODF / EPUB package, an OLE Word / Excel /
    /// PowerPoint / Outlook file, PDF or HTML saved under the wrong extension
    /// (#556) — the conversion is retried once as that format, and the result
    /// reports the format it was converted as. A document that converts as
    /// declared is never inspected; when the retry fails too, or nothing is
    /// recognised, the first attempt's error stands.
    pub fn convert(&self, source: SourceDocument) -> Result<ConversionResult, ConversionError> {
        self.convert_with_progress(source, 1, None)
    }

    /// [`convert`](Self::convert) as document `document_index` of a batch
    /// (1-based; what the progress events carry — docling's `convert_all`
    /// numbering), with `callback` receiving this conversion's events on top
    /// of the converter's own ([`Self::progress_callback`]) — docling's
    /// `convert(source, progress_callback=…)`, for one converter serving
    /// several callers (#783).
    pub fn convert_with_progress(
        &self,
        source: SourceDocument,
        document_index: usize,
        callback: Option<ProgressCallback>,
    ) -> Result<ConversionResult, ConversionError> {
        let reporter = std::sync::Arc::new(self.reporter(document_index, &source.name, callback));
        reporter.document_started();
        let result = self.convert_reporting(source, &reporter);
        reporter.document_completed(match &result {
            Ok(result) => result.status,
            Err(_) => ConversionStatus::Failure,
        });
        result
    }

    fn convert_reporting(
        &self,
        source: SourceDocument,
        reporter: &std::sync::Arc<ProgressReporter>,
    ) -> Result<ConversionResult, ConversionError> {
        if let Some(allowed) = &self.allowed_formats {
            if !allowed.contains(&source.format) {
                return Err(ConversionError::UnsupportedFormat(source.format));
            }
        }
        let source = self.with_encoding(source);
        // A source a registered backend claims (#680) is that backend's:
        // its failure is not retried as the content's built-in format.
        if self.claims(&source) {
            return self.convert_as(&source, reporter);
        }
        let err = match self.convert_as(&source, reporter) {
            Ok(result) => return Ok(result),
            Err(err) => err,
        };
        let Some(format) = crate::sniff::detect(&source.bytes).filter(|f| {
            *f != source.format
                && self
                    .allowed_formats
                    .as_ref()
                    .is_none_or(|allowed| allowed.contains(f))
        }) else {
            return Err(err);
        };
        let declared = source.format;
        let retry = SourceDocument { format, ..source };
        // The retry's error replaces the first one only when it says the
        // file is encrypted (#624) — an encrypted `.ppt` named `.pptx` is
        // better reported as encrypted than as a bad ZIP.
        let result = self.convert_as(&retry, reporter).map_err(|e| {
            if crate::backend::offcrypto::is_encryption_error(&e) {
                e
            } else {
                err
            }
        })?;
        // Converted, but not as the name said: worth a line, since the
        // mislabelled file is otherwise invisible.
        eprintln!(
            "docling: warning: {}: not a valid {} file; converted as {} (its content)",
            retry.name,
            declared.as_str(),
            format.as_str()
        );
        Ok(result)
    }

    /// Convert `source` as its declared format, no retry.
    /// Whether `source` is a YouTube watch page fetched from its URL (#681)
    /// — HTML whose base URL names a video. Always false without the
    /// `youtube` feature, so such a page converts as the HTML it is.
    fn is_youtube_page(source: &SourceDocument) -> bool {
        #[cfg(feature = "youtube")]
        {
            source.format == InputFormat::Html
                && source
                    .base_url
                    .as_deref()
                    .is_some_and(|url| crate::backend::youtube::video_id(url).is_some())
        }
        #[cfg(not(feature = "youtube"))]
        {
            let _ = source;
            false
        }
    }

    /// A YouTube watch page's conversion (#681): the title, metadata and
    /// caption transcript, the language from `asr_lang`.
    #[cfg(feature = "youtube")]
    fn youtube_page(
        &self,
        source: &SourceDocument,
    ) -> Result<docling_core::DoclingDocument, ConversionError> {
        crate::backend::youtube::YoutubeBackend {
            lang: self.asr_lang.clone(),
            // The caption fetch follows the one hop YouTube's `timedtext`
            // may bounce through; the SSRF guard sees each.
            fetch: crate::fetch::FetchPolicy {
                max_redirects: 2,
                ..crate::fetch::FetchPolicy::default()
            },
        }
        .convert(source)
    }

    #[cfg(not(feature = "youtube"))]
    fn youtube_page(
        &self,
        _source: &SourceDocument,
    ) -> Result<docling_core::DoclingDocument, ConversionError> {
        unreachable!("is_youtube_page is false without the youtube feature")
    }

    fn convert_as(
        &self,
        source: &SourceDocument,
        reporter: &std::sync::Arc<ProgressReporter>,
    ) -> Result<ConversionResult, ConversionError> {
        // Problems a conversion survives (docling's `ConversionResult.errors`):
        // today the PDF pipeline's spent document budget (#497).
        #[cfg_attr(not(feature = "pdf"), allow(unused_mut))]
        let mut errors: Vec<crate::ErrorItem> = Vec::new();
        // An encrypted Office document (#624/#625) is decrypted with the
        // converter's password (or the format's default password) into the
        // file the backend reads; without a key that opens it, the error
        // says the document is encrypted — not "bad zip" or an empty result.
        let unlocked;
        let source = match crate::backend::offcrypto::unlock(source, self.password.as_deref())? {
            Some(bytes) => {
                unlocked = SourceDocument {
                    bytes,
                    ..source.clone()
                };
                &unlocked
            }
            None => source,
        };
        let registered = self.backend_for(source);
        let mut document = if let Some(registered) = registered {
            // A registered backend (#680) ahead of every built-in format.
            registered.backend().convert(source)?
        } else {
            match source.format {
                // A legacy APS (Automated Patent System) plain-text patent (`PATN`
                // first record) is reconstructed verbatim, mirroring docling.
                InputFormat::Md if crate::backend::uspto::looks_like_aps(&source.text()?) => {
                    crate::backend::uspto::convert_aps(source)?
                }
                // A text/Markdown-typed file that is actually an XML document (e.g. a
                // JATS article saved with a `.txt` extension) routes to the XML
                // backends by content, mirroring docling's content-based detection.
                InputFormat::Md if looks_like_xml(&source.text()?) => {
                    match sniff_xml(&source.bytes) {
                        InputFormat::XmlUspto => UsptoBackend.convert(source)?,
                        InputFormat::XmlXbrl => crate::backend::xbrl::convert_xbrl(
                            source,
                            self.xbrl_taxonomy.as_deref(),
                        )?,
                        InputFormat::XmlRis => crate::backend::RisBackend.convert(source)?,
                        // docling's format detection reads an XML-looking `.txt` as
                        // `application/xml` and, when its DOCTYPE names a JATS DTD
                        // (`JATS-journalpublishing…` / `JATS-archive…`), converts it
                        // with the JATS backend like a real `.nxml`; any other XML
                        // saved as `.txt` is reconstructed generically
                        // (element-by-element).
                        _ if has_jats_doctype(&source.text()?) => JatsBackend {
                            images: self.image_policy(),
                        }
                        .convert(source)?,
                        _ => crate::backend::jats::convert_generic(source)?,
                    }
                }
                // DeepSeek-OCR annotated Markdown (VLM token format) is detected by
                // its `<|ref|>…[[bbox]]` annotations and parsed separately.
                InputFormat::Md if is_deepseek_markdown(&source.text()?) => {
                    DeepSeekBackend.convert(source)?
                }
                InputFormat::Md => MarkdownBackend {
                    strict: self.strict,
                    images: self.image_policy(),
                }
                .convert(source)?,
                InputFormat::Csv => CsvBackend.convert(source)?,
                // A YouTube watch page (#681): routed by the URL it was fetched
                // from, as MarkItDown's `accepts` does — its title, metadata and
                // caption transcript, not the page's markup.
                InputFormat::Html if Self::is_youtube_page(source) => self.youtube_page(source)?,
                InputFormat::Html => {
                    // Optionally resolve the CSS cascade in a headless browser first
                    // (strips computed-hidden elements); everything else stays in the
                    // Rust HTML backend, which runs on the cleaned HTML.
                    // Bytes → text through docling's BeautifulSoup decoding order
                    // (#371): BOM, declared charset, UTF-8, windows-1252 — so a
                    // legacy windows-1252 page converts instead of failing the
                    // UTF-8 check every other text backend applies.
                    let decoded = crate::backend::decode_html_bytes(&source.bytes);
                    let html = self.maybe_prerender(&decoded)?;
                    let policy = self.image_policy();
                    if policy.sources != crate::ImageSources::None {
                        let resolver = crate::backend::FsImageResolver::new(
                            source.base_dir().map(|p| p.to_path_buf()),
                            source.base_url.clone(),
                            policy,
                        );
                        crate::backend::convert_html(&source.name, &html, &resolver)
                    } else {
                        crate::backend::convert_html(&source.name, &html, &crate::backend::NoFetch)
                    }
                }
                InputFormat::Asciidoc => AsciiDocBackend {
                    images: self.image_policy(),
                }
                .convert(source)?,
                InputFormat::Xlsx => XlsxBackend {
                    skip_empty: self.skip_empty_cells,
                }
                .convert(source)?,
                InputFormat::Pptx => PptxBackend.convert(source)?,
                // RTF (#209): a docling.rs extension — docling reaches RTF only via
                // LibreOffice; here it parses natively (hand-rolled tokenizer).
                InputFormat::Rtf => RtfBackend.convert(source)?,
                InputFormat::Visio => VisioBackend.convert(source)?,
                // AbiWord (#216): docling.rs extension, native AWML parse.
                InputFormat::Abiword => AbwBackend.convert(source)?,
                // WordPerfect 5.x/6.x+ (#216): docling.rs extension, native parse
                // of the ÿWPC function-code stream.
                InputFormat::WordPerfect => WpdBackend.convert(source)?,
                // Microsoft Works word processor (#216): docling.rs extension,
                // native parse after libwps.
                InputFormat::Works => WpsBackend.convert(source)?,
                // StarOffice 5 binaries (#215): docling.rs extension, native CFB
                // parse (docling would go through LibreOffice).
                InputFormat::StarOffice5 => StarOffice5Backend.convert(source)?,
                // DjVu (#434): docling.rs extension, pure-Rust decode (`djvu-rs`).
                // The hidden text layer is the default; a scan-only DjVu falls back
                // to rasterize + OCR when the ML pipeline is built.
                InputFormat::Djvu => self.convert_djvu(source, reporter)?,
                // DIF/SYLK/dBase (#216): docling.rs extensions, one content-sniffing
                // backend for the three table relics.
                InputFormat::Dbf | InputFormat::Dif | InputFormat::Sylk => {
                    InterchangeBackend.convert(source)?
                }
                // Lotus/Quattro/Works record streams (#216): one BOF-sniffing
                // backend for the whole DOS-era family.
                InputFormat::Lotus => LotusBackend.convert(source)?,
                // Lotus Word Pro (#216): Word Pro 97 and later.
                InputFormat::Lwp => crate::backend::LwpBackend.convert(source)?,
                // Classic Mac word processors (#216): MacWrite, Word for Mac, ClarisWorks.
                InputFormat::MacWp => crate::backend::MacWpBackend.convert(source)?,
                // Computer Graphics Metafile (#216): binary and clear-text.
                InputFormat::Cgm => crate::backend::CgmBackend.convert(source)?,
                // Hancom Hangul (#216): HWP 5 and HWPX.
                InputFormat::Hwp => crate::backend::HwpBackend {
                    track_changes: self.track_changes,
                }
                .convert(source)?,
                // Palm e-texts (#216): PalmDOC, TealDoc, zTXT.
                InputFormat::PalmDoc => crate::backend::PalmDocBackend.convert(source)?,
                // Print files (#216): text out of a report or a printer stream.
                InputFormat::Prn => crate::backend::PrnBackend.convert(source)?,
                // T602 (#216): docling.rs extension, after LibreOffice's filter.
                InputFormat::T602 => crate::backend::T602Backend.convert(source)?,
                // Quattro Pro (#216): docling.rs extension, native parse after
                // libwps (DOS/Windows record streams, QPW OLE zones).
                InputFormat::QuattroPro => QuattroBackend.convert(source)?,
                InputFormat::Docx => DocxBackend {
                    track_changes: self.track_changes,
                    extract_diagrams: self.extract_diagrams,
                    use_outline_level_for_headings: self.use_outline_level_for_headings,
                }
                .convert(source)?,
                // Legacy binary Office (issue #127): parsed natively — docling
                // proper converts these through LibreOffice first (PR #3804).
                InputFormat::Xls => XlsBackend {
                    skip_empty: self.skip_empty_cells,
                }
                .convert(source)?,
                InputFormat::Ppt => PptBackend.convert(source)?,
                InputFormat::Doc => DocBackend {
                    track_changes: self.track_changes,
                    use_outline_level_for_headings: self.use_outline_level_for_headings,
                }
                .convert(source)?,
                InputFormat::Vtt => WebVttBackend.convert(source)?,
                InputFormat::Afp => crate::backend::AfpBackend {
                    page_range: self.page_range,
                }
                .convert(source)?,
                InputFormat::Calendar => CalendarBackend.convert(source)?,
                InputFormat::Ebcdic => EbcdicBackend {
                    layout: self.ebcdic_layout.clone(),
                }
                .convert(source)?,
                InputFormat::Email => EmailBackend {
                    list_attachments: self.list_attachments,
                    images: self.image_policy(),
                }
                .convert(source)?,
                InputFormat::Mhtml => MhtmlBackend {
                    images: self.image_policy(),
                    use_web_browser: self.use_web_browser,
                }
                .convert(source)?,
                InputFormat::Epub => EpubBackend {
                    images: self.image_policy(),
                    use_web_browser: self.use_web_browser,
                }
                .convert(source)?,
                // A `.json` that is a Jupyter notebook (#673) — `"nbformat"` and
                // `"cells"` — reads as one, like MarkItDown's `application/json`
                // sniff; any other JSON is the docling-JSON backend's.
                InputFormat::JsonDocling => {
                    if crate::backend::ipynb::looks_like_notebook(&source.text()?) {
                        self.ipynb_backend().convert(source)?
                    } else {
                        DoclingJsonBackend.convert(source)?
                    }
                }
                InputFormat::Jsonl => crate::backend::text::convert_json_text(source)?,
                InputFormat::Text => crate::backend::TextBackend.convert(source)?,
                InputFormat::Ipynb => self.ipynb_backend().convert(source)?,
                // RSS / Atom / RDF feeds (#678): the title, each item's heading,
                // date and body (its HTML through the HTML backend).
                InputFormat::Feed => crate::backend::FeedBackend {
                    images: self.image_policy(),
                }
                .convert(source)?,
                // A bare `.xml` (#671): the dialects docling reads by content —
                // JATS by its DOCTYPE or `<article>` root — else one xml code
                // block with a warning, never an empty document.
                InputFormat::Xml => match sniff_xml(&source.bytes) {
                    InputFormat::XmlUspto => UsptoBackend.convert(source)?,
                    InputFormat::Feed => crate::backend::FeedBackend {
                        images: self.image_policy(),
                    }
                    .convert(source)?,
                    InputFormat::XmlXbrl => {
                        crate::backend::xbrl::convert_xbrl(source, self.xbrl_taxonomy.as_deref())?
                    }
                    InputFormat::XmlDoclang => crate::backend::DoclangBackend.convert(source)?,
                    InputFormat::XmlRis => crate::backend::RisBackend.convert(source)?,
                    _ if looks_like_jats(&source.bytes) => JatsBackend {
                        images: self.image_policy(),
                    }
                    .convert(source)?,
                    _ => crate::backend::text::convert_xml_text(source)?,
                },
                InputFormat::Latex => LatexBackend.convert(source)?,
                InputFormat::XmlRis => crate::backend::RisBackend.convert(source)?,
                // A bare `.xml` defaults to XmlJats; sniff the content to route to the
                // right XML backend (docling distinguishes by DOCTYPE / root element).
                InputFormat::XmlJats | InputFormat::XmlUspto | InputFormat::XmlXbrl => {
                    match sniff_xml(&source.bytes) {
                        InputFormat::XmlUspto => UsptoBackend.convert(source)?,
                        InputFormat::XmlXbrl => crate::backend::xbrl::convert_xbrl(
                            source,
                            self.xbrl_taxonomy.as_deref(),
                        )?,
                        InputFormat::XmlRis => crate::backend::RisBackend.convert(source)?,
                        _ => JatsBackend {
                            images: self.image_policy(),
                        }
                        .convert(source)?,
                    }
                }
                InputFormat::Odt | InputFormat::Ods | InputFormat::Odp => {
                    crate::backend::convert_odf(source, &self.image_policy(), self.track_changes)?
                }
                // DocLang back in: bare XML (`.dclg`/`.dclg.xml`) or the OPC
                // archive `--to dclx` writes.
                InputFormat::XmlDoclang | InputFormat::Dclx => {
                    crate::backend::DoclangBackend.convert(source)?
                }
                // Raw DocTags (VLM token markup, #152): the tolerant docling-core
                // parser — a best-effort document out of any markup; only a
                // table past the cell budget (#861) is an error.
                InputFormat::DocTags => {
                    let mut doc = docling_core::doctags::try_parse(&source.text()?)
                        .map_err(|e| ConversionError::with_source("doctags", e))?;
                    doc.name = source.name.clone();
                    doc
                }
                #[cfg(feature = "pdf")]
                InputFormat::Pdf => {
                    let converted = self
                        .ml_pipeline(reporter.page_hook())
                        .and_then(|mut p| {
                            p.set_force_full_page_ocr(self.force_full_page_ocr);
                            p.set_pages(self.page_range);
                            p.convert_outcome(&source.bytes, self.password.as_deref(), &source.name)
                        })
                        .map_err(|e| ConversionError::with_source("pdf", e))?;
                    // The pages the budget left out are reported as failed
                    // pages (#783), so the count reaches the total.
                    if let docling_pdf::Completion::TimedOut { pages_selected, .. } =
                        &converted.completion
                    {
                        reporter.fail_pages_cut_off(
                            self.page_range.map_or(1, |(first, _)| first),
                            *pages_selected,
                        );
                    }
                    if let Some(message) = converted.completion.message() {
                        errors.push(crate::ErrorItem::timeout(message));
                    }
                    converted.document
                }
                // SVG (#212), the ML route: rasterize (resvg, white-backed PNG at
                // ~2048px long side) and ride the image pipeline. `--no-ocr` /
                // `--text-layer-only` short-circuit to direct <text> extraction
                // instead — the SVG carries its text natively, so skipping OCR
                // must not mean losing it.
                #[cfg(feature = "pdf")]
                InputFormat::Svg if self.text_layer_only == TextLayerOnly::Off && !self.no_ocr => {
                    let png = crate::backend::svg::rasterize_png(&source.bytes)?;
                    self.ml_pipeline(reporter.page_hook())
                        .and_then(|mut p| p.convert_image(&png, &source.name))
                        .map_err(|e| ConversionError::with_source("svg", e))?
                }
                // `text_layer_only=auto` on an SVG: its own `<text>` when it
                // has any (the SVG carries its text natively), the ML image
                // route only for a file of pure graphics — the text-first
                // rule a PDF follows, applied here.
                #[cfg(feature = "pdf")]
                InputFormat::Svg if self.text_layer_only == TextLayerOnly::Auto && !self.no_ocr => {
                    match crate::backend::SvgBackend.convert(source) {
                        Ok(doc) => doc,
                        Err(_) => {
                            let png = crate::backend::svg::rasterize_png(&source.bytes)?;
                            self.ml_pipeline(reporter.page_hook())
                                .and_then(|mut p| p.convert_image(&png, &source.name))
                                .map_err(|e| ConversionError::with_source("svg", e))?
                        }
                    }
                }
                // SVG without the ML pipeline (pdf-text / wasm builds) or with
                // --no-ocr / --text-layer-only: pure-Rust <text> extraction, flat
                // paragraphs in reading order (the pdf / pdf-text split, applied
                // to SVG) — the SVG carries its text natively, so skipping OCR
                // must not mean losing it.
                InputFormat::Svg => crate::backend::SvgBackend.convert(source)?,
                // Apple iWork (#213): pure-Rust IWA text extraction, all builds.
                InputFormat::Pages | InputFormat::Numbers | InputFormat::Keynote => {
                    crate::backend::IworkBackend.convert(source)?
                }
                #[cfg(feature = "pdf")]
                InputFormat::Image => self
                    .ml_pipeline(reporter.page_hook())
                    .and_then(|mut p| p.convert_image(&source.bytes, &source.name))
                    .map_err(|e| ConversionError::with_source("image", e))?,
                #[cfg(feature = "pdf")]
                InputFormat::MetsGbs => self
                    .ml_pipeline(reporter.page_hook())
                    .and_then(|mut p| {
                        docling_pdf::convert_mets_gbs_with_pipeline(
                            &source.bytes,
                            &source.name,
                            &mut p,
                        )
                    })
                    .map_err(|e| ConversionError::with_source("mets-gbs", e))?,
                // Audio → Whisper ASR (symphonia decode + ONNX inference); each
                // transcribed segment becomes a `[time: start-end] text` paragraph.
                #[cfg(feature = "asr")]
                InputFormat::Audio => docling_asr::convert_audio_with_options(
                    &source.bytes,
                    &source.name,
                    self.asr_model.as_deref(),
                    self.asr_lang.as_deref(),
                )
                .map_err(|e| ConversionError::with_source(source.format.as_str(), e))?,
                // Video (#138): the audio track transcribes through the same ASR
                // path (Phase 1), and — when the ffmpeg binary is available —
                // sampled frames interleave with the transcript as timestamped
                // pictures (Phase 2). Without ffmpeg: transcript only.
                #[cfg(feature = "asr")]
                InputFormat::Video => self
                    .convert_video(source)
                    .map_err(|e| ConversionError::with_source(source.format.as_str(), e))?,
                // Without the full ML pipeline, `pdf-text` still converts a PDF's
                // embedded text layer (pure Rust — the wasm32 path), equivalent to
                // `--text-layer-only`: flat paragraphs, no headings/tables/pictures. A
                // scanned PDF has no text layer, so an empty document means "this
                // needs OCR" — say so instead of returning nothing.
                #[cfg(all(feature = "pdf-text", not(feature = "pdf")))]
                InputFormat::Pdf => {
                    let doc = docling_pdf::convert_text_layer_pages_opts(
                        &source.bytes,
                        &source.name,
                        self.page_range,
                        self.pdf_text_options(),
                    )
                    .map_err(|e| ConversionError::with_source("pdf", e))?;
                    if doc.nodes.is_empty() {
                        return Err(ConversionError::Parse(
                            "PDF has no embedded text layer (scanned/image-only?); OCR needs a \
                         build with the `pdf` feature"
                                .into(),
                        ));
                    }
                    doc
                }
                // Compiled without the ML pipelines: the formats stay detectable,
                // but converting them needs a build with the matching feature.
                #[cfg(not(any(feature = "pdf", feature = "pdf-text")))]
                InputFormat::Pdf => {
                    return Err(ConversionError::Parse(
                        "Pdf conversion is not compiled in (rebuild with the `pdf` feature, or \
                     `pdf-text` for text-layer-only extraction)"
                            .into(),
                    ))
                }
                #[cfg(not(feature = "pdf"))]
                InputFormat::Image | InputFormat::MetsGbs => {
                    return Err(ConversionError::Parse(format!(
                        "{:?} conversion is not compiled in (rebuild with the `pdf` feature)",
                        source.format
                    )))
                }
                #[cfg(not(feature = "asr"))]
                InputFormat::Audio | InputFormat::Video => {
                    return Err(ConversionError::Parse(format!(
                        "{} conversion is not compiled in (rebuild with the `asr` feature)",
                        source.format.as_str()
                    )))
                }
                // Resolved for a registered backend (#680) that this converter
                // does not have.
                InputFormat::Plugin => {
                    return Err(ConversionError::UnknownFormat {
                        hint: format!(
                            "'{}' was loaded for a registered backend, but none registered on \
                         this converter claims it",
                            source.name
                        ),
                    })
                }
            }
        };
        self.enrich_pictures(&mut document, source, reporter);
        // The file's own metadata (#679) — headers only, cheap — read for
        // every conversion since the Pandoc export writes it as `meta`
        // (#836) whatever `metadata_output` says; the JSON and Markdown
        // exports still show it only when asked. A docling-JSON input keeps
        // what it carried. Before the PII pass, which covers it too.
        if document.metadata.is_empty() {
            document.metadata = crate::metadata::read(source, self.password.as_deref());
        }
        // The PII pass (#621) runs on the finished model, before anything
        // reads it — after the backend, before the export-side settings.
        let redaction = self.redact(&mut document)?;
        self.finish_document(&mut document);
        // Read off the input bytes, not the converted model (#934): the
        // reader's repairs leave no trace in what it loaded.
        #[cfg(any(feature = "pdf", feature = "pdf-text"))]
        let pdf_repairs = (source.format == InputFormat::Pdf && self.reports_pdf_repairs())
            .then(|| docling_pdf::pdf_repairs(&source.bytes));
        #[cfg(not(any(feature = "pdf", feature = "pdf-text")))]
        let pdf_repairs = None;

        // What the reader read around (#912: an HTML page past the depth
        // cap, flattened) is a problem the conversion survived — reported
        // like one, so the status and every surface's error list say so.
        errors.extend(document.notices.iter().map(crate::ErrorItem::notice));
        let status = if errors.is_empty() {
            ConversionStatus::Success
        } else {
            ConversionStatus::PartialSuccess
        };
        Ok(ConversionResult {
            document,
            status,
            input_name: source.name.clone(),
            format: source.format,
            redaction,
            pdf_repairs,
            errors,
        })
    }
}

/// A picture an enrichment pass may still describe: one without a
/// description, or with only the alt text its file carried (#676) — the
/// model's or the reader's answer replaces that fallback.
fn undescribed(description: &Option<docling_core::PictureDescription>) -> bool {
    description.as_ref().is_none_or(|d| d.is_alt_text())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The warm slot records the switches it was built with and rebuilds
    /// on a change (#669).
    #[cfg(feature = "pdf")]
    #[test]
    fn warm_pipeline_tracks_its_switches() {
        let warm = WarmPipeline::new();
        let full = PipelineSwitches::of(&DocumentConverter::new());
        let text = PipelineSwitches::of(&DocumentConverter::new().text_layer_only(true));
        assert_ne!(full, text);
        // `auto` keeps the full model set: the switch is per conversion, so
        // the warm pipeline is never rebuilt between a digital and a scanned
        // document.
        let auto =
            PipelineSwitches::of(&DocumentConverter::new().text_layer_only(TextLayerOnly::Auto));
        assert_eq!(full, auto);
        let built = |w: &WarmPipeline| w.slot.lock().unwrap().as_ref().map(|(s, _)| *s);
        assert_eq!(built(&warm), None, "nothing loads before the first lease");
        drop(warm.lease(full).unwrap());
        assert_eq!(built(&warm), Some(full));
        drop(warm.lease(text).unwrap());
        assert_eq!(built(&warm), Some(text));
        // Converters share the handle.
        let converter = DocumentConverter::new().warm_pipeline(&warm);
        drop(converter.ml_pipeline(None).unwrap());
        assert_eq!(built(&warm), Some(full));
    }

    /// A conversion that panics holding the warm pipeline poisons its lock;
    /// the next one gets a fresh pipeline, not the half-used one or a panic.
    #[cfg(feature = "pdf")]
    #[test]
    fn warm_pipeline_recovers_from_a_panic() {
        let warm = WarmPipeline::new();
        let full = PipelineSwitches::of(&DocumentConverter::new());
        drop(warm.lease(full).unwrap());
        let held = warm.clone();
        let _ = std::thread::spawn(move || {
            let _slot = held.lease(full).unwrap();
            panic!("a conversion panics mid-document");
        })
        .join();
        assert!(warm.slot.is_poisoned());
        let slot = warm.lease(full).expect("recovered");
        assert!(slot.is_some());
        drop(slot);
        assert!(!warm.slot.is_poisoned());
        assert!(format!("{warm:?}").contains("WarmPipeline"));
    }

    /// A file whose extension lies converts as its content after the
    /// declared format fails (#556); junk keeps its original error, and a
    /// format the converter does not allow is not retried into.
    #[test]
    fn mislabelled_sources_retry_as_their_content() {
        let conv = DocumentConverter::new();
        let as_doc =
            |bytes: &[u8]| SourceDocument::from_bytes("x", InputFormat::Doc, bytes.to_vec());

        let rtf = conv
            .convert(as_doc(b"{\\rtf1\\ansi Hello RTF\\par}"))
            .unwrap();
        assert_eq!(rtf.format, InputFormat::Rtf);
        assert_eq!(rtf.document.export_to_markdown().trim(), "Hello RTF");

        let html = conv
            .convert(as_doc(
                b"<!DOCTYPE html><html><body><p>Hello HTML</p></body></html>",
            ))
            .unwrap();
        assert_eq!(html.format, InputFormat::Html);
        assert_eq!(html.document.export_to_markdown().trim(), "Hello HTML");

        let docx = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/data/docx/sources/docx_moveto_body.docx"),
        )
        .unwrap();
        let docx = conv.convert(as_doc(&docx)).unwrap();
        assert_eq!(docx.format, InputFormat::Docx);
        assert!(docx
            .document
            .export_to_markdown()
            .contains("avantMARK11apres"));

        let err = conv.convert(as_doc(b"neither of them")).unwrap_err();
        assert!(err.to_string().contains("doc"), "{err}");

        let only_doc = DocumentConverter::with_allowed_formats([InputFormat::Doc]);
        assert!(only_doc.convert(as_doc(b"{\\rtf1 Hello\\par}")).is_err());
    }

    /// docling reads an XML-looking `.txt` as `application/xml` and converts
    /// it with the JATS backend when its DOCTYPE names a JATS DTD; other XML
    /// under `.txt` stays the generic element-by-element reconstruction.
    #[test]
    fn jats_doctype_text_file_uses_the_jats_backend() {
        let jats = "<!DOCTYPE article PUBLIC \"-//NLM//DTD JATS (Z39.96) Journal Archiving and Interchange DTD v1.2 20190208//EN\" \"JATS-archivearticle1.dtd\">\n<article><front><article-meta><title-group><article-title>T</article-title></title-group></article-meta></front><body><sec><title>S</title><p>Body.</p></sec></body></article>";
        let src = SourceDocument::from_bytes("a.txt", InputFormat::Md, jats.as_bytes().to_vec());
        let doc = DocumentConverter::new().convert(src).unwrap().document;
        assert!(doc.tree.is_some(), "JATS tree expected");
        assert_eq!(doc.export_to_markdown().trim(), "# T\n\n## S\n\nBody.");
        let other = "<?xml version=\"1.0\"?>\n<article><body><sec><title>S</title><p>Body.</p></sec></body></article>";
        let src = SourceDocument::from_bytes("b.txt", InputFormat::Md, other.as_bytes().to_vec());
        let doc = DocumentConverter::new().convert(src).unwrap().document;
        assert!(doc.tree.is_none(), "generic XML path expected");
    }

    /// docling's `TextBackendOptions.encoding`: the converter's `encoding`
    /// decodes text inputs as named (a source's own setting wins), and an
    /// undecodable byte or unknown label is an error, not a guess.
    #[test]
    fn encoding_option_decodes_text_inputs() {
        let sjis = b"# \x93\xfa\x96\x7b\n".to_vec();
        let md = |c: &DocumentConverter, s: SourceDocument| {
            c.convert(s).map(|r| r.document.export_to_markdown())
        };
        let conv = DocumentConverter::new().encoding(Some("shift_jis".into()));
        let src = || SourceDocument::from_bytes("doc", InputFormat::Md, sjis.clone());
        assert_eq!(md(&conv, src()).unwrap().trim(), "# 日本");
        // Detection reads the same bytes as windows-1252.
        assert_ne!(
            md(&DocumentConverter::new(), src()).unwrap().trim(),
            "# 日本"
        );
        // The source's own encoding takes precedence over the converter's.
        let own = src().with_encoding(Some("shift_jis".into()));
        let latin = DocumentConverter::new().encoding(Some("latin1".into()));
        assert_eq!(md(&latin, own).unwrap().trim(), "# 日本");
        assert!(md(
            &DocumentConverter::new().encoding(Some("utf-8".into())),
            src()
        )
        .is_err());
        assert!(md(
            &DocumentConverter::new().encoding(Some("nope-1".into())),
            src()
        )
        .is_err());
    }

    #[test]
    fn end_to_end_markdown() {
        let src =
            SourceDocument::from_bytes("doc", InputFormat::Md, b"# Hello\n\nWorld.\n".to_vec());
        let result = DocumentConverter::new().convert(src).unwrap();
        assert_eq!(result.status, ConversionStatus::Success);
        assert_eq!(result.document.export_to_markdown(), "# Hello\n\nWorld.\n");
    }

    #[test]
    fn doctags_input_converts() {
        // Raw DocTags markup (#152) — the VLM token stream — as a first-class
        // input format (.doctags/.dt), through the tolerant docling-core
        // parser.
        let markup = b"<doctag><section_header_level_1><loc_1><loc_2><loc_3><loc_4>Intro</section_header_level_1><text>Body.</text></doctag>"
            .to_vec();
        let src = SourceDocument::from_bytes("page.doctags", InputFormat::DocTags, markup);
        let result = DocumentConverter::new().convert(src).unwrap();
        let md = result.document.export_to_markdown();
        assert!(md.contains("## Intro"), "{md}");
        assert!(md.contains("Body."), "{md}");
    }

    #[test]
    fn doclang_xml_round_trips() {
        // Every input format now has a backend; DocLang XML reads back in and
        // re-exports as Markdown.
        let xml = b"<doclang version=\"0.7\">\n  <heading>Title</heading>\n  \
                    <text>Hello <bold>world</bold></text>\n</doclang>"
            .to_vec();
        let src = SourceDocument::from_bytes("doc.dclg", InputFormat::XmlDoclang, xml);
        let result = DocumentConverter::new().convert(src).unwrap();
        let md = result.document.export_to_markdown();
        assert!(md.contains("# Title"), "{md}");
        assert!(md.contains("**world**"), "{md}");
    }

    #[test]
    fn sniffs_uspto_doctype_case_insensitively() {
        // docling PR #3801: Grant Full Text v2.5 files were missed when the
        // DOCTYPE casing differed.
        for head in [
            "<?xml version=\"1.0\"?><!DOCTYPE PATDOC SYSTEM \"ST32-US-Grant-025xml.dtd\"><PATDOC/>",
            "<?xml version=\"1.0\"?><!DOCTYPE patdoc SYSTEM \"st32-us-grant-025xml.dtd\"><patdoc/>",
            "<?xml version=\"1.0\"?><US-PATENT-GRANT-V4/>",
        ] {
            assert_eq!(
                super::sniff_xml(head.as_bytes()),
                InputFormat::XmlUspto,
                "head: {head}"
            );
        }
    }
}
