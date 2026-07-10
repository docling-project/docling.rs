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
    /// them each time (the transient path `inner` takes otherwise).
    pdf_pipeline: std::sync::Mutex<Option<docling::Pipeline>>,
    no_ocr: bool,
    no_table_former: bool,
}

#[pymethods]
impl PyDocumentConverter {
    /// Engine knobs mapped from docling's converter/`PdfPipelineOptions` on the
    /// Python side:
    /// * `fetch_images` — resolve remote/local `<img src>` for HTML/EPUB.
    /// * `do_ocr` — run OCR on scanned PDF/image pages (docling's `do_ocr`).
    /// * `do_table_structure` — recover table structure with TableFormer
    ///   (docling's `do_table_structure`).
    /// * `use_web_browser` — render HTML via headless Chrome before parsing.
    ///
    /// Markdown flavour is chosen at export time by docling-core, so there is no
    /// `strict` knob here.
    #[new]
    #[pyo3(signature = (
        fetch_images = false,
        do_ocr = true,
        do_table_structure = true,
        use_web_browser = false,
        allowed_formats = None,
    ))]
    fn new(
        fetch_images: bool,
        do_ocr: bool,
        do_table_structure: bool,
        use_web_browser: bool,
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
        Ok(Self {
            inner: base
                .fetch_images(fetch_images)
                .no_ocr(!do_ocr)
                .no_table_former(!do_table_structure)
                .use_web_browser(use_web_browser),
            pdf_pipeline: std::sync::Mutex::new(None),
            no_ocr: !do_ocr,
            no_table_former: !do_table_structure,
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
        let mut slot = self.pdf_pipeline.lock().unwrap();
        if slot.is_none() {
            let mut pipeline = docling::Pipeline::new()
                .map_err(|e| ConversionError::new_err(e.to_string()))?
                .no_table_former(self.no_table_former)
                .no_ocr(self.no_ocr);
            py.allow_threads(|| pipeline.warm_up())
                .map_err(|e| ConversionError::new_err(e.to_string()))?;
            *slot = Some(pipeline);
        }
        Ok(())
    }

    /// Convert a document from a filesystem path (str / os.PathLike).
    /// Releases the GIL for the (potentially long) conversion.
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
        if src.format == docling::InputFormat::Pdf {
            let mut slot = self.pdf_pipeline.lock().unwrap();
            if let Some(pipeline) = slot.as_mut() {
                let doc = py
                    .allow_threads(|| pipeline.convert(&src.bytes, None, &src.name))
                    .map_err(|e| ConversionError::new_err(e.to_string()))?;
                return Ok(PyNativeResult {
                    status: "success".to_string(),
                    input_name: src.name,
                    document_json: doc.export_to_json(),
                });
            }
        }
        let result = py
            .allow_threads(|| self.inner.convert(src))
            .map_err(|e| ConversionError::new_err(e.to_string()))?;
        Ok(native_result(result))
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

/// str / pathlib.Path / anything os.PathLike → PathBuf.
struct PathLike(std::path::PathBuf);

impl<'py> FromPyObject<'py> for PathLike {
    fn extract_bound(ob: &Bound<'py, PyAny>) -> PyResult<Self> {
        if let Ok(p) = ob.extract::<std::path::PathBuf>() {
            return Ok(PathLike(p));
        }
        let fspath = ob.py().import("os")?.getattr("fspath")?;
        Ok(PathLike(fspath.call1((ob,))?.extract()?))
    }
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDocumentConverter>()?;
    m.add_class::<PyNativeResult>()?;
    m.add("ConversionError", m.py().get_type::<ConversionError>())?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
