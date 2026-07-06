//! PyO3 bindings: a docling-shaped Python API over the fleischwolf converter.
//!
//! The Python-visible classes mirror docling's names and the common call
//! shape (`DocumentConverter().convert(src).document.export_to_markdown()`),
//! so swapping `from docling.document_converter import DocumentConverter` for
//! `from fleischwolf import DocumentConverter` is the whole migration for the
//! Markdown/JSON path. Model discovery/download lives on the Python side
//! (`fleischwolf.models`), mirroring how docling fetches its artifacts.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use fleischwolf::{ConversionStatus, SourceDocument};

/// The converted document: docling-core's `DoclingDocument` counterpart.
#[pyclass(name = "DoclingDocument")]
struct PyDoclingDocument {
    inner: fleischwolf::DoclingDocument,
}

#[pymethods]
impl PyDoclingDocument {
    /// Markdown export. `strict=None` keeps the converter's mode (docling-legacy
    /// byte-parity by default); `strict=True/False` overrides per call.
    #[pyo3(signature = (strict = None))]
    fn export_to_markdown(&self, strict: Option<bool>) -> String {
        match strict {
            Some(s) => self.inner.export_to_markdown_with(s),
            None => self.inner.export_to_markdown(),
        }
    }

    /// docling-core's native `DoclingDocument` JSON wire format, as a string.
    fn export_to_json(&self) -> String {
        self.inner.export_to_json()
    }

    /// docling's `export_to_dict()`: the JSON wire format as a Python dict.
    fn export_to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let json = self.inner.export_to_json();
        let loads = py.import("json")?.getattr("loads")?;
        loads.call1((json,))
    }

    /// docling's `save_as_json(path)`.
    fn save_as_json(&self, path: std::path::PathBuf) -> PyResult<()> {
        std::fs::write(&path, self.inner.export_to_json())
            .map_err(|e| PyRuntimeError::new_err(format!("save_as_json {}: {e}", path.display())))
    }

    /// docling's `save_as_markdown(path)`.
    #[pyo3(signature = (path, strict = None))]
    fn save_as_markdown(&self, path: std::path::PathBuf, strict: Option<bool>) -> PyResult<()> {
        let md = match strict {
            Some(s) => self.inner.export_to_markdown_with(s),
            None => self.inner.export_to_markdown(),
        };
        std::fs::write(&path, md).map_err(|e| {
            PyRuntimeError::new_err(format!("save_as_markdown {}: {e}", path.display()))
        })
    }
}

/// docling's `ConversionResult`: `.document`, `.status`, `.input.file`-ish name.
#[pyclass(name = "ConversionResult")]
struct PyConversionResult {
    #[pyo3(get)]
    status: String,
    #[pyo3(get)]
    input_name: String,
    document: Py<PyDoclingDocument>,
}

#[pymethods]
impl PyConversionResult {
    #[getter]
    fn document(&self, py: Python<'_>) -> Py<PyDoclingDocument> {
        self.document.clone_ref(py)
    }
}

/// docling's `DocumentConverter`. Thread-safe for sequential reuse; the heavy
/// ML models are process-wide state loaded on first PDF/image conversion.
#[pyclass(name = "DocumentConverter")]
struct PyDocumentConverter {
    inner: fleischwolf::DocumentConverter,
}

#[pymethods]
impl PyDocumentConverter {
    /// `strict` — fleischwolf-only cleaner Markdown (docling has no analogue;
    /// default False = docling-legacy byte parity). `fetch_images` — resolve
    /// remote/local `<img src>` for HTML/EPUB (docling's `enable_*_fetch`).
    #[new]
    #[pyo3(signature = (strict = false, fetch_images = false))]
    fn new(strict: bool, fetch_images: bool) -> Self {
        Self {
            inner: fleischwolf::DocumentConverter::new()
                .strict(strict)
                .fetch_images(fetch_images),
        }
    }

    /// Convert a document from a filesystem path (str / os.PathLike).
    /// Releases the GIL for the (potentially long) conversion.
    fn convert(&self, py: Python<'_>, source: PathLike) -> PyResult<PyConversionResult> {
        let path = source.0;
        let result = py
            .allow_threads(|| {
                let src = SourceDocument::from_file(&path)?;
                self.inner.convert(src)
            })
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        wrap_result(py, result)
    }

    /// Convert in-memory bytes; `name` (with extension) drives format detection,
    /// mirroring docling's `DocumentStream(name=..., stream=...)`.
    fn convert_bytes(
        &self,
        py: Python<'_>,
        name: String,
        data: Bound<'_, PyBytes>,
    ) -> PyResult<PyConversionResult> {
        let bytes = data.as_bytes().to_vec();
        let ext = std::path::Path::new(&name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let format = fleischwolf::InputFormat::from_extension(ext).ok_or_else(|| {
            PyRuntimeError::new_err(format!("cannot detect input format from name {name:?}"))
        })?;
        let result = py
            .allow_threads(|| {
                self.inner
                    .convert(SourceDocument::from_bytes(&name, format, bytes))
            })
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        wrap_result(py, result)
    }
}

fn wrap_result(py: Python<'_>, r: fleischwolf::ConversionResult) -> PyResult<PyConversionResult> {
    let status = match r.status {
        ConversionStatus::Success => "success",
        ConversionStatus::PartialSuccess => "partial_success",
        ConversionStatus::Failure => "failure",
    }
    .to_string();
    Ok(PyConversionResult {
        status,
        input_name: r.input_name,
        document: Py::new(py, PyDoclingDocument { inner: r.document })?,
    })
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
    m.add_class::<PyConversionResult>()?;
    m.add_class::<PyDoclingDocument>()?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
