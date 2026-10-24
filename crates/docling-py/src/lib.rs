//! PyO3 bindings: the Rust **document processor** behind a docling-shaped
//! Python API.
//!
//! This is a strangler-fig drop-in for Python docling's common path. The Rust
//! engine does the parsing and hands back docling-core's JSON wire format; the
//! Python layer (`docling_rs/__init__.py`) loads that into the *real*
//! `docling_core.types.doc.DoclingDocument`, so `export_to_markdown()`,
//! `export_to_dict()`, the serializers, chunkers and pipelines are docling's
//! own Python code — only the processor underneath is Rust.
//!
//! Accordingly the native module is intentionally tiny: it exposes conversion
//! entry points that return `(status, input_name, document_json)`; everything
//! document-shaped is reconstructed on the Python side. Model discovery/download
//! lives in `docling_rs.models`, mirroring how docling fetches its artifacts.

use pyo3::exceptions::{PyException, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use docling::{ConversionStatus, SourceDocument};

// docling's `ConversionError`: raised when a conversion fails (docling code does
// `except ConversionError`). Re-exported from the Python package.
pyo3::create_exception!(_native, ConversionError, PyException);
// The typed encryption cases (#636), subclasses of `ConversionError` so
// existing `except ConversionError` still catches them: `EncryptionError` for
// any encrypted document, `PasswordRequiredError` / `WrongPasswordError` for
// the two a caller prompts for a password on. The message is unchanged.
pyo3::create_exception!(_native, EncryptionError, ConversionError);
pyo3::create_exception!(_native, PasswordRequiredError, EncryptionError);
pyo3::create_exception!(_native, WrongPasswordError, EncryptionError);

/// The Python exception for a conversion failure: the typed subclass when the
/// error says the document is encrypted (#636), `ConversionError` otherwise.
fn conversion_error(e: docling::ConversionError) -> PyErr {
    typed_error(e.encryption().cloned(), e.to_string())
}

/// [`conversion_error`] for the PDF pipeline's own error (the warm path).
fn pdf_error(e: docling::PdfError) -> PyErr {
    let kind = match &e {
        docling::PdfError::Encrypted(kind) => Some(kind.clone()),
        _ => None,
    };
    typed_error(kind, e.to_string())
}

fn typed_error(kind: Option<docling::EncryptionError>, message: String) -> PyErr {
    match kind {
        Some(docling::EncryptionError::NeedPassword) => PasswordRequiredError::new_err(message),
        Some(docling::EncryptionError::WrongPassword) => WrongPasswordError::new_err(message),
        Some(_) => EncryptionError::new_err(message),
        None => ConversionError::new_err(message),
    }
}

/// Run `work` on a background thread while this (Python) thread waits with the
/// GIL released, polling `Python::check_signals` so Ctrl-C raises
/// `KeyboardInterrupt` promptly instead of stalling until the native call
/// returns. On interrupt the worker is left to finish detached and its result
/// is dropped; a conversion already in flight cannot be cancelled mid-parse.
fn run_interruptible<T, F>(py: Python<'_>, work: F) -> PyResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> PyResult<T> + Send + 'static,
{
    use std::sync::mpsc::{channel, RecvTimeoutError};
    use std::sync::Mutex;
    use std::time::Duration;

    let (tx, rx) = channel();
    // Mutex only to make the receiver Sync for `detach`; never contended.
    let rx = Mutex::new(rx);
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    loop {
        let received = py.detach(|| rx.lock().unwrap().recv_timeout(Duration::from_millis(100)));
        match received {
            Ok(result) => return result,
            Err(RecvTimeoutError::Timeout) => py.check_signals()?,
            Err(RecvTimeoutError::Disconnected) => {
                return Err(ConversionError::new_err("conversion worker panicked"))
            }
        }
    }
}

/// The Rust processor's result: a conversion status, the input name, and the
/// document as docling-core's JSON wire format. The Python layer validates the
/// JSON into a genuine `DoclingDocument`.
#[pyclass(name = "NativeResult", skip_from_py_object)]
#[derive(Clone)]
struct PyNativeResult {
    #[pyo3(get)]
    status: String,
    #[pyo3(get)]
    input_name: String,
    #[pyo3(get)]
    document_json: String,
    /// docling's `ConversionResult.errors` as `(component_type, module_name,
    /// error_message, page_no)` tuples; non-empty exactly for
    /// `partial_success`.
    #[pyo3(get)]
    errors: Vec<ErrorTuple>,
    /// What the PII pass redacted (#621) when `redact_pii` was on: counts
    /// per label; `None` otherwise.
    #[pyo3(get)]
    redaction: Option<std::collections::HashMap<String, usize>>,
    /// The document's metadata (#679) as the JSON's `metadata` object, when
    /// the converter's `metadata` mode writes it and there is any; `None`
    /// otherwise.
    #[pyo3(get)]
    metadata_json: Option<String>,
}

/// The `metadata` object of `doc`'s JSON (#679), when its output mode
/// writes one and there is any.
fn metadata_json(doc: &docling::DoclingDocument) -> Option<String> {
    (doc.metadata_output.in_json() && !doc.metadata.is_empty())
        .then(|| doc.metadata.to_json().to_string())
}

fn redaction_counts(
    report: Option<docling::RedactionReport>,
) -> Option<std::collections::HashMap<String, usize>> {
    report.map(|r| r.counts.into_iter().collect())
}

/// One entry of a ZIP archive converted through
/// `DocumentConverter.convert_archive` (#557): its path inside the archive,
/// the outcome kind — `"converted"`, `"skipped"`, `"failed"` — and either the
/// result or the reason / error message.
#[pyclass(name = "NativeArchiveItem")]
struct PyNativeArchiveItem {
    #[pyo3(get)]
    path: String,
    #[pyo3(get)]
    outcome: String,
    #[pyo3(get)]
    result: Option<PyNativeResult>,
    #[pyo3(get)]
    error: Option<String>,
}

/// One `ErrorItem` as the Python layer unpacks it.
type ErrorTuple = (String, String, String, Option<usize>);

fn error_tuples(errors: Vec<docling::ErrorItem>) -> Vec<ErrorTuple> {
    errors
        .into_iter()
        .map(|e| (e.component_type, e.module_name, e.error_message, e.page_no))
        .collect()
}

/// docling's `DocumentConverter`, reduced to its processor role. Thread-safe for
/// sequential reuse; the heavy ML models are process-wide state loaded on first
/// PDF/image conversion.
/// A Python callable registered as a backend (#680, see
/// `DocumentConverter.register_backend`): called with the source's bytes and
/// file name under the GIL; its Markdown or `DoclingDocument` becomes the
/// document.
struct PyBackend {
    name: String,
    func: Py<PyAny>,
}

impl docling::backend::DeclarativeBackend for PyBackend {
    fn convert(
        &self,
        source: &SourceDocument,
    ) -> Result<docling::DoclingDocument, docling::ConversionError> {
        use docling::backend::{DoclingJsonBackend, MarkdownBackend};
        let file_name = match &source.extension {
            Some(ext) => format!("{}.{ext}", source.name),
            None => source.name.clone(),
        };
        enum Answer {
            Markdown(String),
            Json(String),
        }
        let answer = Python::attach(|py| -> PyResult<Answer> {
            let out = self
                .func
                .call1(py, (PyBytes::new(py, &source.bytes), file_name.as_str()))?;
            let out = out.bind(py);
            if let Ok(text) = out.extract::<String>() {
                return Ok(Answer::Markdown(text));
            }
            // docling-core's own serialization: `export_to_dict` writes the
            // `$ref` aliases the JSON reader follows (`model_dump_json`
            // without `by_alias` would not).
            let dict = if out.hasattr("export_to_dict")? {
                Some(out.call_method0("export_to_dict")?)
            } else if out.is_instance_of::<pyo3::types::PyDict>() {
                Some(out.clone())
            } else {
                None
            };
            if let Some(dict) = dict {
                let json = py.import("json")?.call_method1("dumps", (dict,))?;
                return Ok(Answer::Json(json.extract()?));
            }
            Err(pyo3::exceptions::PyTypeError::new_err(format!(
                "a backend returns Markdown (str) or a DoclingDocument, not {}",
                out.get_type().name()?
            )))
        })
        .map_err(|e| docling::ConversionError::Parse(format!("backend `{}`: {e}", self.name)))?;
        match answer {
            Answer::Markdown(md) => MarkdownBackend {
                strict: false,
                images: docling::ImagePolicy::default(),
            }
            .convert(&SourceDocument::from_bytes(
                source.name.clone(),
                docling::InputFormat::Md,
                md.into_bytes(),
            )),
            Answer::Json(json) => {
                let mut doc = DoclingJsonBackend.convert(&SourceDocument::from_bytes(
                    source.name.clone(),
                    docling::InputFormat::JsonDocling,
                    json.into_bytes(),
                ))?;
                doc.name = source.name.clone();
                Ok(doc)
            }
        }
    }
}

#[pyclass(name = "DocumentConverter")]
struct PyDocumentConverter {
    inner: docling::DocumentConverter,
    /// A persistent, primed PDF pipeline once `initialize_pipeline` runs — so
    /// PDFs reuse its warm models across `convert` calls instead of reloading
    /// them each time (the transient path `inner` takes otherwise). `Arc` so
    /// the interruptible worker threads can own a handle to it.
    pdf_pipeline: std::sync::Arc<std::sync::Mutex<Option<docling::Pipeline>>>,
    /// The warm `docling::Pipeline`'s switches, in its (pre-2.0) names:
    /// `no_ocr` is `text_layer_only`, `skip_ocr` is `do_ocr=False` (#611);
    /// `text_layer_auto` is `text_layer_only="auto"`.
    no_ocr: bool,
    text_layer_auto: bool,
    skip_ocr: bool,
    no_table_former: bool,
    no_text_panels: bool,
    heading_hierarchy: bool,
    enrich: docling::EnrichmentOptions,
    /// OCR knobs the warm pipeline needs at `initialize_pipeline` time (the
    /// transient `inner` path reads them off the converter instead). Parsed /
    /// validated in `new`, so they hold engine values, not raw strings.
    force_full_page_ocr: bool,
    ocr_lang: Option<docling::OcrLang>,
    ocr_engine: Option<docling::OcrEngine>,
    /// Tesseract's `-l` argument from `ocr_lang` when the engine is Tesseract.
    tesseract_lang: Option<String>,
    ocr_mode: Option<docling::OcrMode>,
    ocr_scale: Option<f32>,
    /// RapidOCR's text-line orientation handling (#666); `None` = the
    /// environment's default.
    ocr_textline_orientation: Option<bool>,
    /// Picture-crop scale and page images (#519/#520), for the warm pipeline.
    images: docling::ImageOutput,
    page_range: Option<(usize, usize)>,
    /// docling's `document_timeout` (#497), for the warm pipeline.
    document_timeout: Option<std::time::Duration>,
    /// The password of an encrypted PDF (#611), for the warm pipeline.
    password: Option<String>,
    /// `pipeline="vlm"` (#304): resolved once in `new` (a bad configuration
    /// raises there, not mid-conversion); `convert` then routes PDF/image
    /// through the remote VLM instead of the local ML stack.
    vlm: Option<docling::vlm::VlmOptions>,
}

#[pymethods]
impl PyDocumentConverter {
    /// Engine knobs mapped from docling's converter/`PdfPipelineOptions` on the
    /// Python side:
    /// * `fetch_images` — resolve remote/local `<img src>` for HTML/EPUB/MHTML/JATS
    ///   (the alias of `image_sources="remote"`, #646).
    /// * `image_sources` / `image_hosts` / `max_image_bytes` / `max_images` /
    ///   `max_image_total_mb` / `min_image_bytes` — the image-source policy
    ///   (#646): which references resolve (`none` | `embedded` | `local` |
    ///   `remote`), the remote host allow-list, the per-document limits.
    /// * `password` — the password of an encrypted PDF (docling's
    ///   `--pdf-password` / `PdfBackendOptions.password`, #611) or Office
    ///   document (.docx/.xlsx/.pptx/.doc/.xls/.ppt, #625). `pdf_password`,
    ///   its earlier name, is still read.
    /// * `do_ocr` — run OCR on scanned PDF/image pages (docling's `do_ocr`).
    ///   `do_ocr=False` now matches docling exactly (#244): layout detection
    ///   and TableFormer still run, only OCR is skipped — previously it
    ///   disabled the whole ML stack (that fast path is the docling.rs-only
    ///   `text_layer_only=True`; `text_layer_only="auto"` takes it per
    ///   document, when the text layer carries the document, and the full
    ///   pipeline — OCR included — when it does not).
    /// * `force_full_page_ocr` — OCR every PDF page even when it carries a
    ///   text layer (docling's `force_full_page_ocr`); ignored when
    ///   `do_ocr=False`.
    /// * `do_table_structure` — recover table structure with TableFormer
    ///   (docling's `do_table_structure`).
    /// * `no_text_panels` — keep every detected picture as a picture: disable
    ///   the demotion of uncaptioned dense-text "picture" regions into
    ///   paragraphs (docling.rs-specific escape hatch for image-extraction
    ///   workflows, #173).
    /// * `heading_hierarchy` — infer PDF/image section-header levels after
    ///   assembly (docling's `HeadingHierarchyModel`, #302): PDF bookmarks
    ///   are authoritative, then legal/outline numbering, then font style.
    ///   Off by default; headings then keep the flat detected level.
    /// * `do_picture_classification` — classify pictures with the
    ///   DocumentFigureClassifier enrichment model (docling's flag of the same
    ///   name; needs .models/picture_classifier.onnx).
    /// * `do_code_enrichment` / `do_formula_enrichment` — rewrite code blocks /
    ///   decode formula LaTeX with the CodeFormulaV2 VLM (docling's flags of
    ///   the same names; need .models/code_formula/).
    /// * `do_picture_ocr` — OCR the pictures embedded in non-PDF documents
    ///   (#645) and attach the text as the picture's description annotation
    ///   (`meta.description` / `annotations`); `picture_ocr_classes` (a
    ///   comma-separated DocumentFigureClassifier label list) and
    ///   `picture_ocr_min_side` (px, default 32) filter which pictures are
    ///   read; `picture_ocr_scale` sets their OCR resolution in px per
    ///   picture pixel under RapidOCR's 2000 px cap (#666; `1.0` = native
    ///   pixels, `None` = the image-input rule);
    ///   `keep_picture_images=False` drops the image bytes afterwards.
    /// * `do_picture_description` + `picture_description_endpoint` / `_model`
    ///   / `_api_key` / `_prompt` / `_mode` (`caption` | `ocr`) /
    ///   `_concurrency` (#670): describe every picture through an
    ///   OpenAI-compatible vision endpoint; endpoint, model and key fall
    ///   back to the `vlm_*` kwargs, then `DOCLING_RS_VLM_*`.
    /// * `redact_pii` — redact personal data from the converted document
    ///   before export (#621; a docling.rs extension): e-mail, phone, card
    ///   numbers, IBANs, IPs, URL credentials, national IDs, and names /
    ///   organizations / locations with the NER model (.models/ner/).
    ///   `redact_mode` (`label` | `pseudonym` | `fixed:<text>`),
    ///   `redact_kinds` (comma-separated), `redact_pattern` (`NAME=REGEX`
    ///   lines) and `redact_images` (`drop` | `box_out` | `keep`) tune it;
    ///   the result's `redaction` carries the counts per label.
    /// * `use_web_browser` — render HTML via headless Chrome before parsing.
    /// * `page_range` — `(first, last)` 1-based inclusive PDF page window
    ///   (docling's option of the same name, #80); other formats ignore it.
    /// * `ocr_lang` — OCR recognition language for scanned pages: `"en"`
    ///   (default; proper Latin word spacing) or `"ch"` (the multilingual
    ///   docling-conformance model), or a BCP-47 tag for either language
    ///   (`"en-US"`, `"eng"`, `"zh"`, `"zh-Hans"`, `"zh-TW"`; docling's `iso:`
    ///   prefix accepted, #388). Any other language raises `ValueError`.
    /// * `ocr_mode` — which regions feed the OCR (docling's `OcrMode`, #254):
    ///   `"default"` | `"full_page"` | `"layout_regions"` |
    ///   `"pdf_aware_layout_regions"`. `full_page`/`layout_regions` discard
    ///   the text layer like `force_full_page_ocr`.
    /// * `ocr_scale` — OCR render scale in px per PDF point (docling's
    ///   `OcrOptions.scale`, #254); `None` reads the pipeline's own 2.0 px/pt
    ///   render (docling's default is 3 = 216 dpi).
    /// * `ocr_textline_orientation` — RapidOCR's text-line orientation
    ///   handling (#666): oriented line crops, tall lines turned 90°,
    ///   upside-down lines turned by the 0/180 classifier
    ///   (.models/ocr_textline_ori.onnx). `None` = off unless
    ///   `DOCLING_RS_OCR_TEXTLINE_ORIENTATION` is set.
    /// * `images_scale` — picture crops (and page images) in px per PDF
    ///   point, docling's `images_scale` (#520), 0.1–4.0; `None` keeps the
    ///   pipeline's 2.0 px/pt render. The picture `dpi` in the JSON is
    ///   72·scale either way (#519).
    /// * `generate_page_images` — keep each page's render as
    ///   `document.pages[n].image` (docling's `generate_page_images`, #520),
    ///   which docling-core's `TableItem.get_image` / `FormulaItem.get_image`
    ///   crop from.
    /// * `ocr_engine` — which OCR engine reads scanned pages (#460):
    ///   `"ppocr"` (default, the built-in PP-OCRv3 recognizer) or
    ///   `"tesseract"` (the system `tesseract` binary, docling's
    ///   `TesseractCliOcrOptions`); under Tesseract `ocr_lang` is its
    ///   language list — tessdata stems (`"deu+fra"`) or BCP-47 tags. An
    ///   unknown engine, or a language the engine cannot read, raises
    ///   `ValueError`.
    /// * `skip_empty_cells` — omit empty cells from sparse XLSX/XLS table
    ///   grids instead of materialising each region's full bounding box
    ///   (#271; docling.rs extension, off by default).
    /// * `compact_tables` — unpadded `| a | b |` Markdown tables, all
    ///   formats (#271; docling.rs extension, off by default).
    /// * `picture_alt_text` — render pictures' alt text (DOCX/PPTX/XLSX,
    ///   #676) in the engine's Markdown and HTML; the JSON carries it always
    ///   as `meta.description` (`created_by: alt_text`). Off by default.
    /// * `asr_model` — audio/video speech-recognition preset: `None` /
    ///   `"whisper_tiny"` (default), the Whisper presets
    ///   (`"whisper_tiny_en"`, `"whisper_base_en"`, `"whisper_small_en"`,
    ///   `"whisper_distil_small_en"`) or `"parakeet_tdt_0.6b_v3"` (#508;
    ///   `asr_lang` does not apply to it).
    /// * `asr_lang` — transcription language for audio/video: a Whisper code
    ///   (`"en"`, `"de"`, …) or `"auto"` (default) to detect it from the
    ///   first 30 seconds (docling 2.116 parity).
    /// * `encoding` — character encoding of text inputs (Markdown, CSV,
    ///   AsciiDoc, WebVTT, XML, …), docling's `TextBackendOptions.encoding`:
    ///   a WHATWG label or Python codec name (`"shift_jis"`, `"koi8-r"`).
    ///   `None` (default) detects — BOM, UTF-8, a statistical guess for a
    ///   non-Latin encoding (Shift_JIS, GBK, KOI8-R, …; #675), else
    ///   windows-1252; bytes the
    ///   requested encoding cannot decode raise.
    /// * `ipynb_outputs` — Jupyter notebooks (#673): `"none"` (default, the
    ///   notebook as written) or `"text"` (stream and `text/plain` results
    ///   as code blocks after their cell, image outputs as pictures).
    /// * `metadata` — document metadata (#679): `"none"` (default, nowhere),
    ///   `"json"` (a `metadata` object in the docling JSON, and
    ///   `result.metadata`) or `"frontmatter"` (YAML front matter at the top
    ///   of the Markdown as well).
    /// * `track_changes` — tracked changes in DOCX / DOC / ODT (#851,
    ///   Pandoc's `--track-changes`): `"accept"` (insertions shown,
    ///   deletions left out), `"reject"` (the reverse) or `"all"` (both,
    ///   typed with author and date in the Pandoc AST). `None` (default):
    ///   each reader's own view. DOCX reads all three, DOC accept and
    ///   reject, ODT accept; a mode a reader lacks raises.
    /// * `extract_diagrams` — DOCX DrawingML drawings (#858): `"mermaid"`
    ///   (the default) adds a `mermaid` code item after a drawing's picture
    ///   with the flowchart its labelled shapes and connectors describe —
    ///   only when every connector names its two end shapes; a drawing laid
    ///   by eye gets no graph. `"none"` turns the extraction off.
    /// * `pipeline` — `"standard"` (default) or `"vlm"` (#304): convert PDF /
    ///   image inputs by sending each page to a remote OpenAI-compatible
    ///   vision model instead of the local ML stack. The `vlm_*` kwargs
    ///   mirror the Node bindings' options and fall back to the
    ///   `DOCLING_RS_VLM_*` environment (`vlm_endpoint`, `vlm_model` — both
    ///   required; `vlm_api_key`, `vlm_prompt`; `vlm_max_tokens` defaults to
    ///   8192). With `pipeline="standard"` the `vlm_*` kwargs are ignored,
    ///   not rejected.
    ///
    /// Markdown flavour is chosen at export time by docling-core, so there is no
    /// `strict` knob here.
    #[new]
    #[pyo3(signature = (
        fetch_images = false,
        image_sources = None,
        image_hosts = None,
        max_image_bytes = None,
        max_images = None,
        max_image_total_mb = None,
        min_image_bytes = None,
        do_ocr = true,
        force_full_page_ocr = false,
        do_table_structure = true,
        no_text_panels = false,
        heading_hierarchy = false,
        use_web_browser = false,
        do_picture_classification = false,
        do_code_enrichment = false,
        do_formula_enrichment = false,
        do_picture_ocr = false,
        picture_ocr_classes = None,
        picture_ocr_min_side = None,
        picture_ocr_scale = None,
        keep_picture_images = true,
        do_picture_description = false,
        picture_description_endpoint = None,
        picture_description_model = None,
        picture_description_api_key = None,
        picture_description_prompt = None,
        picture_description_mode = None,
        picture_description_concurrency = None,
        redact_pii = false,
        redact_mode = None,
        redact_kinds = None,
        redact_pattern = None,
        redact_images = None,
        redact_allow_terms = None,
        redact_deny_terms = None,
        redact_allow_patterns = None,
        redact_ner_min_score = None,
        asr_model = None,
        asr_lang = None,
        encoding = None,
        ipynb_outputs = None,
        metadata = None,
        track_changes = None,
        extract_diagrams = None,
        video_frames = None,
        video_scene_threshold = None,
        video_frame_max_side = None,
        video_frame_dedupe = None,
        xbrl_taxonomy = None,
        page_range = None,
        ocr_lang = None,
        ocr_mode = None,
        ocr_scale = None,
        ocr_textline_orientation = None,
        ocr_engine = None,
        allowed_formats = None,
        images_scale = None,
        generate_page_images = false,
        text_layer_only = None,
        list_attachments = false,
        skip_empty_cells = false,
        compact_tables = false,
        picture_alt_text = false,
        ebcdic_layout = None,
        pipeline = None,
        vlm_endpoint = None,
        vlm_model = None,
        vlm_api_key = None,
        vlm_prompt = None,
        vlm_max_tokens = None,
        document_timeout = None,
        password = None,
        pdf_password = None,
        progress_callback = None,
        show_progress = false,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        fetch_images: bool,
        image_sources: Option<String>,
        image_hosts: Option<Vec<String>>,
        max_image_bytes: Option<u64>,
        max_images: Option<usize>,
        max_image_total_mb: Option<u64>,
        min_image_bytes: Option<u64>,
        do_ocr: bool,
        force_full_page_ocr: bool,
        do_table_structure: bool,
        no_text_panels: bool,
        heading_hierarchy: bool,
        use_web_browser: bool,
        do_picture_classification: bool,
        do_code_enrichment: bool,
        do_formula_enrichment: bool,
        do_picture_ocr: bool,
        picture_ocr_classes: Option<String>,
        picture_ocr_min_side: Option<u32>,
        picture_ocr_scale: Option<f32>,
        keep_picture_images: bool,
        do_picture_description: bool,
        picture_description_endpoint: Option<String>,
        picture_description_model: Option<String>,
        picture_description_api_key: Option<String>,
        picture_description_prompt: Option<String>,
        picture_description_mode: Option<String>,
        picture_description_concurrency: Option<usize>,
        redact_pii: bool,
        redact_mode: Option<String>,
        redact_kinds: Option<String>,
        redact_pattern: Option<String>,
        redact_images: Option<String>,
        redact_allow_terms: Option<String>,
        redact_deny_terms: Option<String>,
        redact_allow_patterns: Option<String>,
        redact_ner_min_score: Option<f32>,
        asr_model: Option<String>,
        asr_lang: Option<String>,
        encoding: Option<String>,
        ipynb_outputs: Option<String>,
        metadata: Option<String>,
        track_changes: Option<String>,
        extract_diagrams: Option<String>,
        video_frames: Option<usize>,
        video_scene_threshold: Option<f32>,
        video_frame_max_side: Option<u32>,
        video_frame_dedupe: Option<u32>,
        xbrl_taxonomy: Option<std::path::PathBuf>,
        page_range: Option<(usize, usize)>,
        ocr_lang: Option<String>,
        ocr_mode: Option<String>,
        ocr_scale: Option<f32>,
        ocr_textline_orientation: Option<bool>,
        ocr_engine: Option<String>,
        allowed_formats: Option<Vec<String>>,
        images_scale: Option<f32>,
        generate_page_images: bool,
        text_layer_only: Option<Py<PyAny>>,
        list_attachments: bool,
        skip_empty_cells: bool,
        compact_tables: bool,
        picture_alt_text: bool,
        ebcdic_layout: Option<String>,
        pipeline: Option<String>,
        vlm_endpoint: Option<String>,
        vlm_model: Option<String>,
        vlm_api_key: Option<String>,
        vlm_prompt: Option<String>,
        vlm_max_tokens: Option<usize>,
        document_timeout: Option<f64>,
        password: Option<String>,
        pdf_password: Option<String>,
        progress_callback: Option<Py<PyAny>>,
        show_progress: bool,
    ) -> PyResult<Self> {
        let password = password.or(pdf_password);
        // A malformed window (0-based, reversed) raises here instead of
        // silently selecting nothing (#518).
        let page_range = check_page_range(page_range)?;
        let text_layer_only = text_layer_only
            .map(|v| Python::attach(|py| py_text_layer_only(py, &v)))
            .transpose()?;
        // The kwargs as the shared option set (#577): docling's positive
        // spellings (`do_ocr`, `do_table_structure`) and the docling.rs-only
        // `text_layer_only` map onto the engine's switches here, `page_range`
        // onto its `"A-B"` wire form; validation and the mapping onto the
        // converter are then the library's — the same rules and messages as
        // the CLI, docling-serve and the Node bindings. Markdown flavour
        // (`strict`) and the page-break text are chosen at export time on
        // this surface, so they stay unset.
        let opts = docling::ConvertOptions {
            fetch_images: Some(fetch_images),
            image_sources,
            image_hosts,
            max_image_bytes,
            max_images,
            max_image_total_mb,
            min_image_bytes,
            list_attachments: Some(list_attachments),
            skip_empty_cells: Some(skip_empty_cells),
            compact_tables: Some(compact_tables),
            picture_alt_text: Some(picture_alt_text),
            ebcdic_layout,
            encoding,
            ipynb_outputs,
            metadata,
            track_changes,
            extract_diagrams,
            use_web_browser: Some(use_web_browser),
            xbrl_taxonomy: xbrl_taxonomy.map(|p| p.to_string_lossy().into_owned()),
            asr_model,
            asr_lang,
            video_frames,
            video_scene_threshold,
            video_frame_max_side,
            video_frame_dedupe,
            pages: page_range.map(|(first, last)| format!("{first}-{last}")),
            document_timeout,
            text_layer_only,
            no_ocr: Some(!do_ocr),
            password: password.clone(),
            force_full_page_ocr: Some(force_full_page_ocr),
            no_table_former: Some(!do_table_structure),
            no_text_panels: Some(no_text_panels),
            heading_hierarchy: Some(heading_hierarchy),
            ocr_engine,
            ocr_lang,
            ocr_mode,
            ocr_scale,
            ocr_textline_orientation,
            images_scale,
            page_images: Some(generate_page_images),
            do_picture_classification: Some(do_picture_classification),
            do_code_enrichment: Some(do_code_enrichment),
            do_formula_enrichment: Some(do_formula_enrichment),
            do_picture_ocr: Some(do_picture_ocr),
            picture_ocr_classes,
            picture_ocr_min_side,
            picture_ocr_scale,
            keep_picture_images: Some(keep_picture_images),
            do_picture_description: Some(do_picture_description),
            picture_description_endpoint,
            picture_description_model,
            picture_description_api_key,
            picture_description_prompt,
            picture_description_mode,
            picture_description_concurrency,
            redact_pii: Some(redact_pii),
            redact_mode,
            redact_kinds,
            redact_pattern,
            redact_images,
            redact_allow_terms,
            redact_deny_terms,
            redact_allow_patterns,
            redact_ner_min_score,
            pipeline,
            vlm_endpoint,
            vlm_model,
            vlm_api_key,
            vlm_prompt,
            vlm_max_tokens,
            ..docling::ConvertOptions::default()
        };
        // A rejected option is a `ValueError` at construction, not a
        // mid-conversion failure.
        let value_err = |e: docling::OptionsError| PyValueError::new_err(e.to_string());
        opts.validate().map_err(value_err)?;
        let vlm = opts.vlm_options().map_err(value_err)?;
        // `allowed_formats` (docling's converter arg) restricts which input
        // formats convert; an unknown name is an error so typos surface early.
        let base = match allowed_formats {
            Some(names) => {
                let mut formats = Vec::with_capacity(names.len());
                for name in &names {
                    formats.push(parse_format(name).ok_or_else(|| {
                        PyValueError::new_err(format!("unknown input format {name:?}"))
                    })?);
                }
                docling::DocumentConverter::with_allowed_formats(formats)
            }
            None => docling::DocumentConverter::new(),
        };
        // ZIP inputs (#557): the same `DOCLING_RS_ZIP_MAX_*` bounds the CLI
        // and serve apply.
        let mut inner = opts
            .apply(base.archive_limits(docling::ArchiveLimits::from_env()))
            .map_err(value_err)?
            // docling's `show_progress` (#783): the engine's stderr printer.
            .show_progress(show_progress);
        // docling's `progress_callback`: every conversion's events, as the
        // flat event objects `docling_rs.progress.event_from_dict` types.
        if let Some(callback) = progress_callback {
            inner = inner.progress_callback(py_progress_callback(callback));
        }
        Ok(Self {
            inner,
            pdf_pipeline: std::sync::Arc::new(std::sync::Mutex::new(None)),
            no_ocr: text_layer_only == Some(docling::TextLayerOnly::On),
            text_layer_auto: text_layer_only == Some(docling::TextLayerOnly::Auto),
            skip_ocr: !do_ocr,
            no_table_former: !do_table_structure,
            no_text_panels,
            heading_hierarchy,
            enrich: opts.enrichments(),
            force_full_page_ocr,
            // The typed values the warm pipeline is primed with in
            // `initialize_pipeline` (the transient `inner` path reads the
            // strings off the converter instead).
            ocr_lang: opts.ocr_lang().map_err(value_err)?,
            ocr_engine: opts.ocr_engine().map_err(value_err)?,
            tesseract_lang: opts.tesseract_lang().map_err(value_err)?,
            ocr_mode: opts.ocr_mode().map_err(value_err)?,
            ocr_scale,
            ocr_textline_orientation,
            images: opts.image_output(),
            page_range,
            document_timeout: opts.document_timeout().map_err(value_err)?,
            password,
            vlm,
        })
    }

    /// Eagerly load the PDF/image ML models (docling's `initialize_pipeline`), so
    /// the first PDF conversion doesn't pay the model-load cost and later ones
    /// reuse the warm pipeline. `format` mirrors docling's arg — only `"pdf"` /
    /// `"image"` have models, so other formats are a no-op. Uses the converter's
    /// configured `do_ocr` / `do_table_structure`.
    #[pyo3(signature = (format = None))]
    fn initialize_pipeline(&self, py: Python<'_>, format: Option<String>) -> PyResult<()> {
        let is_ml = match format.as_deref() {
            Some(f) => matches!(f, "pdf" | "image"),
            None => true,
        };
        // `pipeline="vlm"` loads no local models — warming would pay the ML
        // model-load cost for a pipeline that never runs (#304).
        if !is_ml || self.vlm.is_some() {
            return Ok(());
        }
        let slot = std::sync::Arc::clone(&self.pdf_pipeline);
        let no_table_former = self.no_table_former;
        let no_ocr = self.no_ocr;
        let text_layer_auto = self.text_layer_auto;
        let skip_ocr = self.skip_ocr;
        let no_text_panels = self.no_text_panels;
        let heading_hierarchy = self.heading_hierarchy;
        let enrich = self.enrich;
        let force_full_page_ocr = self.force_full_page_ocr;
        let ocr_lang = self.ocr_lang;
        let ocr_engine = self.ocr_engine;
        let tesseract_lang = self.tesseract_lang.clone();
        let ocr_mode = self.ocr_mode;
        let ocr_scale = self.ocr_scale;
        let ocr_textline_orientation = self.ocr_textline_orientation;
        let images = self.images;
        let page_range = self.page_range;
        let document_timeout = self.document_timeout;
        run_interruptible(py, move || {
            let mut slot = slot.lock().unwrap();
            if slot.is_none() {
                let mut pipeline = docling::Pipeline::new()
                    .map_err(|e| ConversionError::new_err(e.to_string()))?
                    .no_table_former(no_table_former)
                    .no_ocr(no_ocr)
                    .text_layer_auto(text_layer_auto)
                    .skip_ocr(skip_ocr)
                    .no_text_panels(no_text_panels)
                    .heading_hierarchy(docling::HeadingHierarchyOptions::enabled(heading_hierarchy))
                    // The warm path used to drop the converter's OCR knobs and
                    // page window on the floor (#254) — prime it with the full
                    // option set the transient path honors.
                    .force_full_page_ocr(force_full_page_ocr)
                    .ocr_lang(ocr_lang)
                    .ocr_engine(ocr_engine)
                    .tesseract_lang(tesseract_lang)
                    .ocr_mode(ocr_mode)
                    .ocr_scale(ocr_scale)
                    .images_scale(images.scale)
                    .generate_page_images(images.page_images)
                    .pages(page_range)
                    .document_timeout(document_timeout)
                    .enrichments(enrich);
                pipeline.set_ocr_textline_orientation(ocr_textline_orientation);
                pipeline
                    .warm_up()
                    .map_err(|e| ConversionError::new_err(e.to_string()))?;
                *slot = Some(pipeline);
            }
            Ok(())
        })
    }

    /// Register a Python callable as a backend (#680): `fn(data: bytes,
    /// name: str)` returning Markdown (`str`, read by the Markdown backend),
    /// a docling-core `DoclingDocument` or its dict (read by the docling-JSON
    /// backend). It claims the sources whose extension is in `extensions`,
    /// whose media type is in `media_types`, or whose bytes `sniff(data) ->
    /// bool` accepts; `priority` orders it among the registered backends
    /// (higher first), all of which come before the built-in formats. The
    /// callable runs with the GIL held; an exception it raises fails that
    /// conversion with its message.
    #[pyo3(signature = (name, func, extensions = None, media_types = None, sniff = None, priority = 0))]
    fn register_backend(
        &mut self,
        name: String,
        func: Py<PyAny>,
        extensions: Option<Vec<String>>,
        media_types: Option<Vec<String>>,
        sniff: Option<Py<PyAny>>,
        priority: i32,
    ) -> PyResult<()> {
        let mut matcher = docling::BackendMatcher::new()
            .extensions(extensions.unwrap_or_default())
            .media_types(media_types.unwrap_or_default());
        if let Some(check) = sniff {
            matcher = matcher.sniff(move |bytes: &[u8]| {
                Python::attach(|py| {
                    check
                        .call1(py, (PyBytes::new(py, bytes),))
                        .and_then(|r| r.is_truthy(py))
                        // A failing check claims nothing.
                        .unwrap_or(false)
                })
            });
        }
        let backend = PyBackend {
            name: name.clone(),
            func,
        };
        self.inner = std::mem::take(&mut self.inner).register_backend(
            name,
            matcher,
            std::sync::Arc::new(backend),
            priority,
        );
        Ok(())
    }

    /// The registered backends, in the order they are tried:
    /// `(name, extensions, media_types, has_sniff, priority)`.
    #[allow(clippy::type_complexity)]
    fn registered_backends(&self) -> Vec<(String, Vec<String>, Vec<String>, bool, i32)> {
        self.inner
            .registered_backends()
            .iter()
            .map(|b| {
                (
                    b.name().to_string(),
                    b.matcher().extension_list().to_vec(),
                    b.matcher().media_type_list().to_vec(),
                    b.matcher().has_sniff(),
                    b.priority(),
                )
            })
            .collect()
    }

    /// Every input file extension this converter takes: the build's and the
    /// registered backends' (#680), sorted, lowercase, no dot.
    fn supported_extensions(&self) -> Vec<String> {
        self.inner.supported_extensions()
    }

    /// Convert a document from a filesystem path (str / os.PathLike).
    /// Runs the (potentially long) conversion off the Python thread with the
    /// GIL released, so Ctrl-C interrupts it. `page_range=(first, last)`
    /// overrides the constructor's window for this call only — docling's
    /// `convert(source, page_range=…)` (#518).
    #[pyo3(signature = (source, page_range = None, progress_callback = None, document_index = 1))]
    fn convert(
        &self,
        py: Python<'_>,
        source: PathLike,
        page_range: Option<(usize, usize)>,
        progress_callback: Option<Py<PyAny>>,
        document_index: usize,
    ) -> PyResult<PyNativeResult> {
        // The converter's loader: a file only a registered backend reads
        // (#680) loads too.
        let src = self
            .inner
            .load_file(&source.0)
            .map_err(|e| ConversionError::new_err(e.to_string()))?;
        self.convert_source(
            py,
            src,
            check_page_range(page_range)?,
            progress_callback,
            document_index,
        )
    }

    /// Convert in-memory bytes; `name` (with extension) drives format detection,
    /// mirroring docling's `DocumentStream(name=..., stream=...)` — unless
    /// `format` names the format outright (an `InputFormat` id such as
    /// `"pdf"`: an email attachment called `scan.bin` sent as
    /// `application/pdf`, #564). `page_range` as in [`convert`](Self::convert).
    /// `base_url` is the URL the bytes were fetched from (#681): relative
    /// image references resolve against it, and a YouTube watch URL turns
    /// the page into its title, metadata and caption transcript.
    #[pyo3(signature = (name, data, page_range = None, format = None, base_url = None, media_type = None, progress_callback = None, document_index = 1))]
    #[allow(clippy::too_many_arguments)]
    fn convert_bytes(
        &self,
        py: Python<'_>,
        name: String,
        data: Bound<'_, PyBytes>,
        page_range: Option<(usize, usize)>,
        format: Option<String>,
        base_url: Option<String>,
        media_type: Option<String>,
        progress_callback: Option<Py<PyAny>>,
        document_index: usize,
    ) -> PyResult<PyNativeResult> {
        let page_range = check_page_range(page_range)?;
        let bytes = data.as_bytes().to_vec();
        let source = match format {
            Some(id) => {
                let format = docling::InputFormat::from_id(&id)
                    .ok_or_else(|| PyValueError::new_err(format!("unknown input format {id:?}")))?;
                SourceDocument::from_bytes(&name, format, bytes).with_media_type(media_type)
            }
            // The engine's own routing (#671): the extension, else the
            // content's signature, else plain text when the bytes are text.
            // The name stays as given (`anchors.html`, not the stem the
            // engine names a file by): docling reports a `DocumentStream`'s
            // name whole, and the `format` branch above keeps it too.
            None => {
                let mut source = self
                    .inner
                    .load_named_bytes(&name, bytes, media_type.as_deref())
                    .map_err(conversion_error)?;
                source.name = name;
                source
            }
        };
        let source = match base_url {
            Some(url) => source.with_base_url(url),
            None => source,
        };
        self.convert_source(py, source, page_range, progress_callback, document_index)
    }

    /// Convert every document inside a ZIP archive on disk (#557): one item
    /// per entry, in archive order — converted, skipped (with the reason:
    /// unsupported type, nested archive, unsafe path, over a
    /// `DOCLING_RS_ZIP_MAX_*` limit) or failed (with the error). Raises only
    /// when the file is not a readable ZIP archive. PDFs inside go through the
    /// transient pipeline, not the one `initialize_pipeline` primed.
    fn convert_archive(
        &self,
        py: Python<'_>,
        source: PathLike,
    ) -> PyResult<Vec<PyNativeArchiveItem>> {
        let bytes = std::fs::read(&source.0)
            .map_err(|e| ConversionError::new_err(format!("{}: {e}", source.0.display())))?;
        self.convert_archive_impl(py, bytes)
    }

    /// As [`convert_archive`](Self::convert_archive), for in-memory bytes.
    fn convert_archive_bytes(
        &self,
        py: Python<'_>,
        data: Bound<'_, PyBytes>,
    ) -> PyResult<Vec<PyNativeArchiveItem>> {
        let bytes = data.as_bytes().to_vec();
        self.convert_archive_impl(py, bytes)
    }
}

impl PyDocumentConverter {
    fn convert_archive_impl(
        &self,
        py: Python<'_>,
        bytes: Vec<u8>,
    ) -> PyResult<Vec<PyNativeArchiveItem>> {
        let converter = self.inner.clone();
        run_interruptible(py, move || {
            let items = converter
                .convert_archive(std::io::Cursor::new(bytes))
                .map_err(|e| ConversionError::new_err(e.to_string()))?;
            Ok(items
                .map(|item| {
                    let (outcome, result, error) = match item.outcome {
                        docling::ArchiveOutcome::Converted(r) => {
                            ("converted", Some(native_result(*r)), None)
                        }
                        docling::ArchiveOutcome::Skipped(reason) => ("skipped", None, Some(reason)),
                        docling::ArchiveOutcome::Failed(e) => ("failed", None, Some(e.to_string())),
                    };
                    PyNativeArchiveItem {
                        path: item.path,
                        outcome: outcome.to_string(),
                        result,
                        error,
                    }
                })
                .collect())
        })
    }
}

/// A per-call `page_range` (#518): 1-based and ordered, like the
/// constructor's (`last` may exceed the page count — docling's default
/// window is `(1, sys.maxsize)`).
fn check_page_range(range: Option<(usize, usize)>) -> PyResult<Option<(usize, usize)>> {
    match range {
        Some((first, last)) if first == 0 || last < first => Err(PyValueError::new_err(format!(
            "page_range must be (first, last) with 1 <= first <= last, got ({first}, {last})"
        ))),
        other => Ok(other),
    }
}

impl PyDocumentConverter {
    /// Convert a prepared [`SourceDocument`], routing PDFs through the warm
    /// pipeline when `initialize_pipeline` has primed it (otherwise the transient
    /// `inner` path, which reloads models per call).
    /// `page_range`: a per-call window overriding the constructor's (#518).
    fn convert_source(
        &self,
        py: Python<'_>,
        src: SourceDocument,
        page_range: Option<(usize, usize)>,
        progress_callback: Option<Py<PyAny>>,
        document_index: usize,
    ) -> PyResult<PyNativeResult> {
        let result =
            self.convert_source_inner(py, src, page_range, progress_callback, document_index);
        // The heap the conversion freed goes back to the OS rather than
        // staying in glibc's arenas between calls (#871; `docling::memory`,
        // as serve does).
        docling::memory::release_free_memory();
        result
    }

    fn convert_source_inner(
        &self,
        py: Python<'_>,
        src: SourceDocument,
        page_range: Option<(usize, usize)>,
        progress_callback: Option<Py<PyAny>>,
        document_index: usize,
    ) -> PyResult<PyNativeResult> {
        // This call's own progress callback (#783), on top of the
        // constructor's — docling's `convert(source, progress_callback=…)`.
        let call_progress = progress_callback.map(py_progress_callback);
        // `pipeline="vlm"` (#304) is a sibling path: the remote model does the
        // reading, so neither the warm pipeline nor the declarative converter
        // applies. A conversion failure raises `ConversionError`, docling's
        // catchable exception — never a crash.
        if let Some(mut vlm) = self.vlm.clone() {
            if page_range.is_some() {
                vlm.page_range = page_range;
            }
            let converter = self.inner.clone();
            return run_interruptible(py, move || {
                // The VLM pipeline has no page hook: its events are the
                // document's start and end.
                let reporter = converter.reporter(document_index, &src.name, call_progress);
                reporter.document_started();
                // A failed or cut-off page (#762, #763) is a
                // `partial_success` with its page error.
                let mut result = match docling::vlm::convert_vlm_result(&src, &vlm) {
                    Ok(result) => result,
                    Err(e) => {
                        reporter.document_completed(docling::ConversionStatus::Failure);
                        return Err(conversion_error(e));
                    }
                };
                // The PII pass (#621) on the VLM path too.
                result.redaction = converter
                    .redact(&mut result.document)
                    .map_err(conversion_error)?;
                reporter.document_completed(result.status);
                Ok(native_result(result))
            });
        }
        if src.format == docling::InputFormat::Pdf && self.pdf_pipeline.lock().unwrap().is_some() {
            let slot = std::sync::Arc::clone(&self.pdf_pipeline);
            let window = page_range.or(self.page_range);
            let default_window = self.page_range;
            let password = self.password.clone();
            let converter = self.inner.clone();
            return run_interruptible(py, move || {
                let mut slot = slot.lock().unwrap();
                let pipeline = slot
                    .as_mut()
                    .ok_or_else(|| ConversionError::new_err("PDF pipeline not initialized"))?;
                // The warm pipeline is shared across calls: apply this call's
                // window and put the constructor's back afterwards. Its
                // progress events (#783) bracket the conversion here, as
                // `DocumentConverter::convert` would report them.
                let reporter = std::sync::Arc::new(converter.reporter(
                    document_index,
                    &src.name,
                    call_progress,
                ));
                reporter.document_started();
                pipeline.set_pages(window);
                pipeline.set_progress(reporter.page_hook());
                let outcome = pipeline.convert_outcome(&src.bytes, password.as_deref(), &src.name);
                pipeline.set_progress(None);
                pipeline.set_pages(default_window);
                // docling's PARTIAL_SUCCESS (#497): a spent budget leaves the
                // pages done so far and says so in `errors`.
                let mut c = match outcome {
                    Ok(c) => c,
                    Err(e) => {
                        reporter.document_completed(docling::ConversionStatus::Failure);
                        return Err(pdf_error(e));
                    }
                };
                if let docling::Completion::TimedOut { pages_selected, .. } = &c.completion {
                    reporter.fail_pages_cut_off(window.map_or(1, |(first, _)| first), *pages_selected);
                }
                let errors: Vec<docling::ErrorItem> = c
                    .completion
                    .message()
                    .map(docling::ErrorItem::timeout)
                    .into_iter()
                    .collect();
                reporter.document_completed(if errors.is_empty() {
                    docling::ConversionStatus::Success
                } else {
                    docling::ConversionStatus::PartialSuccess
                });
                // The warm pipeline bypasses `DocumentConverter::convert`:
                // the PII pass (#621) is applied here.
                let redaction = converter
                    .redact(&mut c.document)
                    .map_err(conversion_error)?;
                Ok(PyNativeResult {
                    status: if errors.is_empty() {
                        "success"
                    } else {
                        "partial_success"
                    }
                    .to_string(),
                    input_name: src.name,
                    metadata_json: metadata_json(&c.document),
                    document_json: c.document.export_to_json(),
                    errors: error_tuples(errors),
                    redaction: redaction_counts(redaction),
                })
            });
        }
        let converter = match page_range {
            Some((first, last)) => self.inner.clone().page_range(first, last),
            None => self.inner.clone(),
        };
        run_interruptible(py, move || {
            let result = converter
                .convert_with_progress(src, document_index, call_progress)
                .map_err(conversion_error)?;
            Ok(native_result(result))
        })
    }
}

/// A Python callable as the engine's progress callback (#783): called with
/// the event as a flat `dict` (docling's event fields; `kind` names the
/// event) from the converting threads — attached to the interpreter for the
/// call, so the GIL serializes the callbacks like any Python code. An
/// exception the callable raises is printed and swallowed, as docling's
/// reporter logs it: an observer must not break the conversion.
fn py_progress_callback(callback: Py<PyAny>) -> docling::ProgressCallback {
    std::sync::Arc::new(move |event: &docling::ProgressEvent| {
        Python::attach(|py| {
            let outcome = progress_event_dict(py, event)
                .and_then(|dict| callback.call1(py, (dict,)).map(|_| ()));
            if let Err(e) = outcome {
                eprintln!("docling: warning: progress callback raised; ignored");
                e.print(py);
            }
        })
    })
}

/// The event's flat fields as a `dict`.
fn progress_event_dict(py: Python<'_>, event: &docling::ProgressEvent) -> PyResult<Py<PyAny>> {
    use pyo3::types::PyDict;
    let dict = PyDict::new(py);
    let serde_json::Value::Object(fields) = event.to_json() else {
        unreachable!("an event is a JSON object");
    };
    for (key, value) in &fields {
        match value {
            serde_json::Value::Bool(b) => dict.set_item(key, *b)?,
            serde_json::Value::Number(n) => match n.as_u64() {
                Some(u) => dict.set_item(key, u)?,
                None => dict.set_item(key, n.as_f64().unwrap_or_default())?,
            },
            serde_json::Value::String(s) => dict.set_item(key, s.as_str())?,
            other => dict.set_item(key, other.to_string())?,
        }
    }
    Ok(dict.into_any().unbind())
}

/// Map a docling `InputFormat` string value (as in `docling_rs.InputFormat`,
/// matching `docling::InputFormat::name()`) to the engine enum.
fn parse_format(name: &str) -> Option<docling::InputFormat> {
    use docling::InputFormat::*;
    Some(match name {
        "docx" => Docx,
        "pptx" => Pptx,
        "html" => Html,
        "image" => Image,
        "pdf" => Pdf,
        "asciidoc" => Asciidoc,
        "md" => Md,
        "csv" => Csv,
        "xlsx" => Xlsx,
        "doc" => Doc,
        "xls" => Xls,
        "ppt" => Ppt,
        "odt" => Odt,
        "ods" => Ods,
        "odp" => Odp,
        "xml_uspto" => XmlUspto,
        "xml_jats" => XmlJats,
        "xml_xbrl" => XmlXbrl,
        "xml_doclang" => XmlDoclang,
        "doctags" => DocTags,
        "mets_gbs" => MetsGbs,
        "json_docling" => JsonDocling,
        "audio" => Audio,
        "video" => Video,
        "vtt" => Vtt,
        "latex" => Latex,
        "email" => Email,
        "epub" => Epub,
        "mhtml" => Mhtml,
        "text" => Text,
        "jsonl" => Jsonl,
        "xml" => Xml,
        "ipynb" => Ipynb,
        _ => return None,
    })
}

fn native_result(r: docling::ConversionResult) -> PyNativeResult {
    let status = match r.status {
        ConversionStatus::Success => "success",
        ConversionStatus::PartialSuccess => "partial_success",
        ConversionStatus::Failure => "failure",
    }
    .to_string();
    let document_json = r.document.export_to_json();
    PyNativeResult {
        status,
        input_name: r.input_name,
        metadata_json: metadata_json(&r.document),
        document_json,
        errors: error_tuples(r.errors),
        redaction: redaction_counts(r.redaction),
    }
}

/// Bounded queue between the chunking thread and the Python iterator: a slow
/// consumer throttles the producer instead of letting chunks pile up.
const CHUNK_CHANNEL_DEPTH: usize = 64;

/// A stream of chunk records — the native side of `docling_rs.chunking`'s
/// lazy `chunk()`. A background thread parses the document and streams each
/// chunk as the chunkers produce it; iterating yields one JSON record
/// (`{text, headings, doc_items, contextualize}`) at a time, so no
/// all-chunks array is ever materialized. Waits are GIL-released and poll for
/// signals, so Ctrl-C interrupts a pending `next()`. Dropping the stream
/// early cancels the producer.
#[pyclass(name = "ChunkStream")]
struct PyChunkStream {
    /// `None` once exhausted, errored, or dropped — disconnects the producer.
    rx: std::sync::Mutex<Option<std::sync::mpsc::Receiver<Result<String, String>>>>,
    handle: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl PyChunkStream {
    /// Disconnect the producer and reap its thread.
    fn finish(&self) {
        *self.rx.lock().unwrap() = None;
        if let Some(h) = self.handle.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

#[pymethods]
impl PyChunkStream {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&self, py: Python<'_>) -> PyResult<Option<String>> {
        use std::sync::mpsc::RecvTimeoutError;
        use std::time::Duration;

        enum Recv {
            Item(Result<String, String>),
            Timeout,
            Done,
        }
        loop {
            let received = py.detach(|| match self.rx.lock().unwrap().as_ref() {
                None => Recv::Done,
                Some(rx) => match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(item) => Recv::Item(item),
                    Err(RecvTimeoutError::Timeout) => Recv::Timeout,
                    Err(RecvTimeoutError::Disconnected) => Recv::Done,
                },
            });
            match received {
                Recv::Item(Ok(record)) => return Ok(Some(record)),
                Recv::Item(Err(e)) => {
                    self.finish();
                    return Err(ConversionError::new_err(e));
                }
                Recv::Timeout => py.check_signals()?,
                Recv::Done => {
                    self.finish();
                    return Ok(None);
                }
            }
        }
    }
}

impl Drop for PyChunkStream {
    fn drop(&mut self) {
        // Disconnect first so a producer blocked on a full channel sees its
        // send fail, then reap it — no detached thread keeps chunking.
        self.finish();
    }
}

/// Chunk a document with the Rust chunkers (docling-core's
/// `HierarchicalChunker` / `HybridChunker` ported to `docling::chunker`, plus
/// docling-rag's Markdown `WindowChunker`).
///
/// `document_json` is docling-core's JSON wire format (what
/// `DoclingDocument.export_to_dict()` serializes to). `chunker` selects the
/// mode: `"hierarchical"` (structure-driven), `"hybrid"` (refines against a
/// token budget, counting tokens with the HuggingFace `tokenizer.json` at
/// `tokenizer` — or at `.models/chunk/tokenizer.json`, the path
/// `scripts/install/download_dependencies.sh` populates, when `None`), or
/// `"window"` (the document's Markdown cut into heading-bounded sections and
/// windowed with `overlap` fractional overlap — docling-rag's window chunker).
/// `size` is the chunk budget in the mode's unit: **tokens** for `"hybrid"`
/// (docling's `max_tokens`), **words** for `"window"` (docling-rag's
/// `max_words`); ignored by `"hierarchical"`.
///
/// Returns a [`PyChunkStream`] that yields one JSON record
/// `{text, headings, doc_items, contextualize}` per chunk as the background
/// thread produces it — the Python layer (`docling_rs.chunking`) turns them
/// into docling-shaped chunk objects lazily. A parse/tokenizer error surfaces
/// on the first `next()`.
#[pyfunction]
#[pyo3(signature = (
    document_json,
    chunker = "hierarchical".to_string(),
    tokenizer = None,
    size = 256,
    merge_peers = true,
    overlap = 0.05,
))]
fn chunk_document(
    document_json: String,
    chunker: String,
    tokenizer: Option<String>,
    size: usize,
    merge_peers: bool,
    overlap: f32,
) -> PyChunkStream {
    use docling::chunker::{
        contextualize, DocChunk, HierarchicalChunker, HybridChunker, WindowChunker,
    };

    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<String, String>>(CHUNK_CHANNEL_DEPTH);
    let handle = std::thread::spawn(move || {
        let source = SourceDocument::from_bytes(
            "document",
            docling::InputFormat::JsonDocling,
            document_json.into_bytes(),
        );
        let result = match docling::DocumentConverter::new().convert(source) {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Err(e.to_string()));
                return;
            }
        };
        // Once the consumer drops the stream, the send fails and the `false`
        // return cancels the walk — no work is spent on unread chunks.
        // The window chunker embeds its own (rag-style) contextualization.
        let record = |c: &DocChunk, contextualized: String| -> String {
            serde_json::json!({
                "text": c.text,
                "headings": c.headings,
                "doc_items": c.doc_items.iter().map(|i| i.self_ref.clone()).collect::<Vec<_>>(),
                // The refs above number the *re-imported* document, which
                // drifts from the caller's JSON (empty items dropped on
                // import, item-tree exports for HTML/DOCX); the per-item
                // kind and standalone text let chunking.py map each item
                // back to the caller's own refs.
                "doc_item_kinds": c.doc_items.iter().map(|i| match i.kind {
                    docling::chunker::ChunkItemKind::Text => "text",
                    docling::chunker::ChunkItemKind::Table => "table",
                    docling::chunker::ChunkItemKind::Picture => "picture",
                }).collect::<Vec<_>>(),
                "doc_item_texts": c.doc_items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(),
                "contextualize": contextualized,
            })
            .to_string()
        };
        match chunker.as_str() {
            "hybrid" => {
                let tok = match docling::chunker::HuggingFaceTokenizer::resolve(
                    tokenizer.as_deref(),
                    size,
                ) {
                    Ok(t) => t,
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                HybridChunker::new(tok)
                    .with_merge_peers(merge_peers)
                    .chunk_with(&result.document, &mut |c| {
                        tx.send(Ok(record(&c, contextualize(&c)))).is_ok()
                    });
            }
            "window" => {
                let markdown = result.document.export_to_markdown();
                WindowChunker::new(size, overlap).chunk_with(&markdown, &mut |c| {
                    tx.send(Ok(record(&c, WindowChunker::contextualize(&c))))
                        .is_ok()
                });
            }
            _ => {
                HierarchicalChunker.chunk_with(&result.document, &mut |c| {
                    tx.send(Ok(record(&c, contextualize(&c)))).is_ok()
                });
            }
        }
    });
    PyChunkStream {
        rx: std::sync::Mutex::new(Some(rx)),
        handle: std::sync::Mutex::new(Some(handle)),
    }
}

/// str / pathlib.Path / anything os.PathLike → PathBuf.
struct PathLike(std::path::PathBuf);

impl<'a, 'py> FromPyObject<'a, 'py> for PathLike {
    type Error = PyErr;

    fn extract(ob: pyo3::Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
        if let Ok(p) = ob.extract::<std::path::PathBuf>() {
            return Ok(PathLike(p));
        }
        let fspath = ob.py().import("os")?.getattr("fspath")?;
        Ok(PathLike(fspath.call1((&*ob,))?.extract()?))
    }
}

/// Referenced image files: `(path under artifacts_dir, bytes)`.
type PyArtifacts<'py> = Vec<(String, Bound<'py, PyBytes>)>;

/// Pandoc's JSON AST (#515) for a document in docling's JSON wire format
/// (what `DoclingDocument.export_to_dict()` serializes to) — the Rust
/// serializer behind `docling-rs --to pandoc`, so Python output matches the
/// CLI's. `image_mode` is `"embedded"` (default, #537) | `"placeholder"` |
/// `"referenced"`;
/// `referenced` returns the image files as `(path under artifacts_dir,
/// bytes)` pairs for the caller to write. `api_version`, when given, must be
/// one the serializer writes (1.23) — anything else raises `ValueError`.
/// `heading_depth` is `"docling"` (default) or `"source"` (#848).
#[pyfunction]
#[pyo3(signature = (
    document_json,
    image_mode = "embedded".to_string(),
    artifacts_dir = "artifacts".to_string(),
    api_version = None,
    heading_depth = None,
    track_changes = None,
))]
fn pandoc_from_json<'py>(
    py: Python<'py>,
    document_json: String,
    image_mode: String,
    artifacts_dir: String,
    api_version: Option<String>,
    heading_depth: Option<String>,
    track_changes: Option<String>,
) -> PyResult<(String, PyArtifacts<'py>)> {
    let track_changes = track_changes
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            s.parse::<docling::TrackChanges>()
                .map_err(|e| PyValueError::new_err(format!("track_changes: {e}")))
        })
        .transpose()?;
    let heading_depth = heading_depth
        .as_deref()
        .unwrap_or_default()
        .parse()
        .map_err(|e: String| PyValueError::new_err(format!("heading_depth: {e}")))?;
    let image_mode = match image_mode.as_str() {
        "placeholder" => docling::ImageMode::Placeholder,
        "embedded" => docling::ImageMode::Embedded,
        "referenced" => docling::ImageMode::Referenced,
        other => {
            return Err(PyValueError::new_err(format!(
                "unknown image_mode {other:?} (expected placeholder, embedded or referenced)"
            )))
        }
    };
    let options = docling::pandoc::PandocExportOptions {
        image_mode,
        artifacts_dir,
        api_version,
        heading_depth,
        track_changes,
        ..Default::default()
    };
    let (json, images) = py
        .detach(|| {
            let value: serde_json::Value =
                serde_json::from_str(&document_json).map_err(|e| e.to_string())?;
            docling::pandoc::from_docling_json(&value, &options).map_err(|e| e.to_string())
        })
        .map_err(PyValueError::new_err)?;
    let images = images
        .into_iter()
        .map(|(path, bytes)| (path, PyBytes::new(py, &bytes)))
        .collect();
    Ok((json, images))
}

/// The attachments of an `.eml` / Outlook `.msg` (#561) as
/// `(index, name, content_type, format, size, inline, skipped, data)` tuples
/// — `data` the payload when it was kept (`None` for an attachment over a
/// limit, or one without a payload). The limits are byte counts and default
/// to the archive limits the converter applies (`DOCLING_RS_ZIP_MAX_*`;
/// 10 000 attachments, 256 MiB each, 1 GiB in all).
/// The format `data` evidently is, by its content alone (#672 —
/// `InputFormat::detect`): the format id (`"pdf"`, `"pptx"`, `"ipynb"`,
/// `"audio"`, …), or `None` for bytes without a signature (prose, Markdown,
/// CSV, random bytes). Signatures only, no classifier.
#[pyfunction]
fn detect_format(data: Bound<'_, PyBytes>) -> Option<&'static str> {
    docling::InputFormat::detect(data.as_bytes()).map(|f| f.as_str())
}

#[pyfunction]
#[pyo3(signature = (data, max_entries = None, max_entry_size = None, max_total_size = None))]
#[allow(clippy::type_complexity)]
fn email_attachments<'py>(
    py: Python<'py>,
    data: Bound<'py, PyBytes>,
    max_entries: Option<usize>,
    max_entry_size: Option<u64>,
    max_total_size: Option<u64>,
) -> PyResult<
    Vec<(
        usize,
        String,
        Option<String>,
        Option<String>,
        u64,
        bool,
        Option<String>,
        Option<Bound<'py, PyBytes>>,
    )>,
> {
    let defaults = docling::ArchiveLimits::from_env();
    let limits = docling::ArchiveLimits {
        max_entries: max_entries.unwrap_or(defaults.max_entries),
        max_entry_size: max_entry_size.unwrap_or(defaults.max_entry_size),
        max_total_size: max_total_size.unwrap_or(defaults.max_total_size),
        ..defaults
    };
    let bytes = data.as_bytes().to_vec();
    let atts = py
        .detach(|| docling::EmailAttachments::open(&bytes, &limits))
        .map_err(|e| ConversionError::new_err(e.to_string()))?;
    Ok(atts
        .entries()
        .iter()
        .map(|e| {
            (
                e.index,
                e.name.clone(),
                e.content_type.clone(),
                e.format.map(|f| f.as_str().to_string()),
                e.size,
                e.inline,
                e.skipped.clone(),
                atts.data(e.index).map(|d| PyBytes::new(py, d)),
            )
        })
        .collect())
}

/// The ONNX Runtime execution providers compiled into this build, by their
/// `DOCLING_RS_EP` names — lets `AcceleratorDevice.MPS` select CoreML only
/// where it exists (#602). Does not resolve the provider choice itself.
#[pyfunction]
fn compiled_providers() -> Vec<&'static str> {
    docling_onnx::compiled_providers()
}

/// The MCP server (#669) — the one `docling-rs mcp` runs — for the
/// `docling-rs-mcp` launcher (`docling_rs.mcp`): `args` are the flags after
/// the command, the rest name the launcher in the server's messages. Blocks,
/// without the GIL, until the client hangs up (stdio) or Ctrl-C (HTTP);
/// returns the exit status.
#[cfg(feature = "mcp")]
#[pyfunction]
fn mcp_main(
    py: Python<'_>,
    args: Vec<String>,
    command: String,
    client_config: String,
    models_hint: String,
    extra_flags: String,
) -> u8 {
    let launcher = docling_cli::mcp::Launcher {
        command: &command,
        client_config: &client_config,
        models_hint: &models_hint,
        extra_flags: &extra_flags,
    };
    py.detach(|| docling_cli::mcp::run_with(args, &launcher))
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDocumentConverter>()?;
    m.add_class::<PyNativeResult>()?;
    m.add_class::<PyNativeArchiveItem>()?;
    m.add_class::<PyChunkStream>()?;
    m.add_function(pyo3::wrap_pyfunction!(chunk_document, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(pandoc_from_json, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(email_attachments, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(detect_format, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(compiled_providers, m)?)?;
    #[cfg(feature = "mcp")]
    m.add_function(pyo3::wrap_pyfunction!(mcp_main, m)?)?;
    m.add("ConversionError", m.py().get_type::<ConversionError>())?;
    m.add("EncryptionError", m.py().get_type::<EncryptionError>())?;
    m.add(
        "PasswordRequiredError",
        m.py().get_type::<PasswordRequiredError>(),
    )?;
    m.add(
        "WrongPasswordError",
        m.py().get_type::<WrongPasswordError>(),
    )?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}

/// `text_layer_only` as Python passes it: the bool it always was, or the
/// string `"auto"` (per document: the text layer alone when it carries the
/// document, the full pipeline — OCR included — when it does not).
fn py_text_layer_only(py: Python<'_>, v: &Py<PyAny>) -> PyResult<docling::TextLayerOnly> {
    let v = v.bind(py);
    if let Ok(flag) = v.extract::<bool>() {
        return Ok(flag.into());
    }
    if let Ok(text) = v.extract::<String>() {
        return text
            .parse::<docling::TextLayerOnly>()
            .map_err(pyo3::exceptions::PyValueError::new_err);
    }
    Err(pyo3::exceptions::PyTypeError::new_err(
        "text_layer_only: expected a bool or \"auto\"",
    ))
}
