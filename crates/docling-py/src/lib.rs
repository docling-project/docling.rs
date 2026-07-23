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
        let received =
            py.detach(|| rx.lock().unwrap().recv_timeout(Duration::from_millis(100)));
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
#[pyclass(name = "NativeResult")]
struct PyNativeResult {
    #[pyo3(get)]
    status: String,
    #[pyo3(get)]
    input_name: String,
    #[pyo3(get)]
    document_json: String,
}

/// docling's `DocumentConverter`, reduced to its processor role. Thread-safe for
/// sequential reuse; the heavy ML models are process-wide state loaded on first
/// PDF/image conversion.
#[pyclass(name = "DocumentConverter")]
struct PyDocumentConverter {
    inner: docling::DocumentConverter,
    /// A persistent, primed PDF pipeline once `initialize_pipeline` runs — so
    /// PDFs reuse its warm models across `convert` calls instead of reloading
    /// them each time (the transient path `inner` takes otherwise). `Arc` so
    /// the interruptible worker threads can own a handle to it.
    pdf_pipeline: std::sync::Arc<std::sync::Mutex<Option<docling::Pipeline>>>,
    no_ocr: bool,
    no_table_former: bool,
    enrich: docling::EnrichmentOptions,
}

#[pymethods]
impl PyDocumentConverter {
    /// Engine knobs mapped from docling's converter/`PdfPipelineOptions` on the
    /// Python side:
    /// * `fetch_images` — resolve remote/local `<img src>` for HTML/EPUB.
    /// * `do_ocr` — run OCR on scanned PDF/image pages (docling's `do_ocr`).
    /// * `do_table_structure` — recover table structure with TableFormer
    ///   (docling's `do_table_structure`).
    /// * `do_picture_classification` — classify pictures with the
    ///   DocumentFigureClassifier enrichment model (docling's flag of the same
    ///   name; needs models/picture_classifier.onnx).
    /// * `do_code_enrichment` / `do_formula_enrichment` — rewrite code blocks /
    ///   decode formula LaTeX with the CodeFormulaV2 VLM (docling's flags of
    ///   the same names; need models/code_formula/).
    /// * `use_web_browser` — render HTML via headless Chrome before parsing.
    /// * `page_range` — `(first, last)` 1-based inclusive PDF page window
    ///   (docling's option of the same name, #80); other formats ignore it.
    /// * `ocr_lang` — OCR recognition language for scanned pages: `"en"`
    ///   (default; proper Latin word spacing) or `"ch"` (the multilingual
    ///   docling-conformance model).
    ///
    /// Markdown flavour is chosen at export time by docling-core, so there is no
    /// `strict` knob here.
    #[new]
    #[pyo3(signature = (
        fetch_images = false,
        do_ocr = true,
        do_table_structure = true,
        use_web_browser = false,
        do_picture_classification = false,
        do_code_enrichment = false,
        do_formula_enrichment = false,
        asr_model = None,
        video_frames = None,
        page_range = None,
        ocr_lang = None,
        allowed_formats = None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        fetch_images: bool,
        do_ocr: bool,
        do_table_structure: bool,
        use_web_browser: bool,
        do_picture_classification: bool,
        do_code_enrichment: bool,
        do_formula_enrichment: bool,
        asr_model: Option<String>,
        video_frames: Option<usize>,
        page_range: Option<(usize, usize)>,
        ocr_lang: Option<String>,
        allowed_formats: Option<Vec<String>>,
    ) -> PyResult<Self> {
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
        let enrich = docling::EnrichmentOptions {
            picture_classification: do_picture_classification,
            code: do_code_enrichment,
            formula: do_formula_enrichment,
        };
        // `video_frames` caps the frames sampled from a video input (0 =
        // transcript only; extraction needs the ffmpeg binary at runtime).
        let base = match video_frames {
            Some(max) => base.video_frames(max),
            None => base,
        };
        // `page_range=(first, last)` converts only that 1-based inclusive PDF
        // page window, docling's option of the same name (#80).
        let base = match page_range {
            Some((first, last)) => base.page_range(first, last),
            None => base,
        };
        // `ocr_lang` — validated here so a typo raises instead of degrading.
        let base = match ocr_lang {
            Some(lang) => {
                if docling::OcrLang::parse(&lang).is_none() {
                    return Err(PyValueError::new_err(format!(
                        "ocr_lang {lang:?} is not \"en\"|\"ch\""
                    )));
                }
                base.ocr_lang(lang)
            }
            None => base,
        };
        Ok(Self {
            inner: base
                .fetch_images(fetch_images)
                .asr_model(asr_model)
                .no_ocr(!do_ocr)
                .no_table_former(!do_table_structure)
                .use_web_browser(use_web_browser)
                .do_picture_classification(do_picture_classification)
                .do_code_enrichment(do_code_enrichment)
                .do_formula_enrichment(do_formula_enrichment),
            pdf_pipeline: std::sync::Arc::new(std::sync::Mutex::new(None)),
            no_ocr: !do_ocr,
            no_table_former: !do_table_structure,
            enrich,
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
        if !is_ml {
            return Ok(());
        }
        let slot = std::sync::Arc::clone(&self.pdf_pipeline);
        let no_table_former = self.no_table_former;
        let no_ocr = self.no_ocr;
        let enrich = self.enrich;
        run_interruptible(py, move || {
            let mut slot = slot.lock().unwrap();
            if slot.is_none() {
                let mut pipeline = docling::Pipeline::new()
                    .map_err(|e| ConversionError::new_err(e.to_string()))?
                    .no_table_former(no_table_former)
                    .no_ocr(no_ocr)
                    .enrichments(enrich);
                pipeline
                    .warm_up()
                    .map_err(|e| ConversionError::new_err(e.to_string()))?;
                *slot = Some(pipeline);
            }
            Ok(())
        })
    }

    /// Convert a document from a filesystem path (str / os.PathLike).
    /// Runs the (potentially long) conversion off the Python thread with the
    /// GIL released, so Ctrl-C interrupts it.
    fn convert(&self, py: Python<'_>, source: PathLike) -> PyResult<PyNativeResult> {
        let src = SourceDocument::from_file(&source.0)
            .map_err(|e| ConversionError::new_err(e.to_string()))?;
        self.convert_source(py, src)
    }

    /// Convert in-memory bytes; `name` (with extension) drives format detection,
    /// mirroring docling's `DocumentStream(name=..., stream=...)`.
    fn convert_bytes(
        &self,
        py: Python<'_>,
        name: String,
        data: Bound<'_, PyBytes>,
    ) -> PyResult<PyNativeResult> {
        let bytes = data.as_bytes().to_vec();
        let ext = std::path::Path::new(&name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let format = docling::InputFormat::from_extension(ext).ok_or_else(|| {
            ConversionError::new_err(format!("cannot detect input format from name {name:?}"))
        })?;
        self.convert_source(py, SourceDocument::from_bytes(&name, format, bytes))
    }
}

impl PyDocumentConverter {
    /// Convert a prepared [`SourceDocument`], routing PDFs through the warm
    /// pipeline when `initialize_pipeline` has primed it (otherwise the transient
    /// `inner` path, which reloads models per call).
    fn convert_source(&self, py: Python<'_>, src: SourceDocument) -> PyResult<PyNativeResult> {
        if src.format == docling::InputFormat::Pdf && self.pdf_pipeline.lock().unwrap().is_some() {
            let slot = std::sync::Arc::clone(&self.pdf_pipeline);
            return run_interruptible(py, move || {
                let mut slot = slot.lock().unwrap();
                let pipeline = slot
                    .as_mut()
                    .ok_or_else(|| ConversionError::new_err("PDF pipeline not initialized"))?;
                let doc = pipeline
                    .convert(&src.bytes, None, &src.name)
                    .map_err(|e| ConversionError::new_err(e.to_string()))?;
                Ok(PyNativeResult {
                    status: "success".to_string(),
                    input_name: src.name,
                    document_json: doc.export_to_json(),
                })
            });
        }
        let converter = self.inner.clone();
        run_interruptible(py, move || {
            let result = converter
                .convert(src)
                .map_err(|e| ConversionError::new_err(e.to_string()))?;
            Ok(native_result(result))
        })
    }
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
        "mets_gbs" => MetsGbs,
        "json_docling" => JsonDocling,
        "audio" => Audio,
        "video" => Video,
        "vtt" => Vtt,
        "latex" => Latex,
        "email" => Email,
        "epub" => Epub,
        "mhtml" => Mhtml,
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
        document_json,
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
/// `tokenizer` — or at `models/chunk/tokenizer.json`, the path
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

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDocumentConverter>()?;
    m.add_class::<PyNativeResult>()?;
    m.add_class::<PyChunkStream>()?;
    m.add_function(pyo3::wrap_pyfunction!(chunk_document, m)?)?;
    m.add("ConversionError", m.py().get_type::<ConversionError>())?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
