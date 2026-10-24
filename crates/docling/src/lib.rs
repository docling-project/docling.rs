//! docling.rs: a Rust port of [docling](https://github.com/docling-project/docling).
//!
//! The public surface mirrors the Python SDK, kept deliberately small:
//!
//! ```no_run
//! use docling::{DocumentConverter, SourceDocument};
//!
//! let converter = DocumentConverter::new();
//! let result = converter
//!     .convert(SourceDocument::from_file("input.md").unwrap())
//!     .unwrap();
//! println!("{}", result.document.export_to_markdown());
//! ```
//!
//! For the PDF/image ML pipeline (pdfium + layout/TableFormer/OCR ONNX), reuse a
//! [`Pipeline`] across documents to amortize model loading, instead of the
//! per-call [`DocumentConverter`]. Deploying as a service: `examples/Dockerfile`
//! is a 3-stage build that bakes the binary, native libs, and exported models
//! (including the KV-cached TableFormer decoder) into a slim, Python-free runtime
//! image — see the "Deploy in a container" section of the README.
//!
//! See `docs/MIGRATION.md` for the architecture, format-by-format parity
//! status, and how conformance against Python docling is measured.

pub mod archive;
pub mod chunks;
mod converter;
pub mod dclx;
pub mod email_attachments;
mod error;
pub mod fetch;
mod format;
// Allocator housekeeping (#669, #871) — docling-core's, so docling-onnx
// applies it to every ONNX Runtime process; re-exported under its old path.
pub use docling_core::memory;
mod metadata;
pub mod options;
pub mod plugins;
pub mod progress;
mod redact;
mod result;
mod sniff;
mod source;
#[cfg(feature = "pdf")]
mod stream;
#[cfg(feature = "vlm")]
pub mod vlm;

pub mod backend;
#[cfg(feature = "asr")]
pub mod video;

pub use archive::{ArchiveLimits, ArchiveOutcome};
pub use backend::images::{ImageLimits, ImagePolicy, ImageSources, DEFAULT_MAX_IMAGE_BYTES};
pub use backend::ipynb::{IpynbBackend, IpynbOutputs};
#[cfg(feature = "pdf")]
pub use converter::WarmPipeline;
pub use converter::{parse_page_range, DocumentConverter, ALL_VIDEO_FRAMES, DEFAULT_VIDEO_FRAMES};
pub use docling_core::{
    CustomPattern, ImageRedaction, PiiKind, RedactionOptions, RedactionReport, Replacement,
};
pub use email_attachments::{EmailAttachmentInfo, EmailAttachments};
pub use error::{ConversionError, EncryptionError};
pub use fetch::FetchPolicy;
pub use format::{InputFormat, OUTPUT_FORMATS};
pub use options::{
    cli_flag, merge_options, ConvertOptions, DiagramFormat, OptionInfo, OptionsError, PipelineKind,
    TextLayerOnly, OPTIONS,
};
pub use plugins::{BackendMatcher, RegisteredBackend};
pub use progress::{
    EnrichmentStep, ProgressCallback, ProgressEvent, ProgressKind, ProgressPrinter,
    ProgressReporter,
};
pub use result::{ConversionResult, ConversionStatus, ErrorItem};
pub use source::SourceDocument;
#[cfg(feature = "pdf")]
pub use stream::MarkdownStream;

// Re-export the core model so callers only need the one crate, and so
// `result.document.export_to_markdown()` works without an extra import.
pub use docling_core::base64;
pub use docling_core::chunker;
pub use docling_core::pandoc;
pub use docling_core::{
    ConfidenceReport, ContentLayer, ContentLayers, DocItemLabel, DoclingDocument,
    HtmlExportOptions, ImageMode, MarkdownExportOptions, MarkdownStreamer, Node, PictureImage,
    QualityGrade, Table,
};
pub use docling_core::{DocumentMetadata, GpsPosition, MetadataOutput};
pub use docling_core::{Revision, RevisionKind, TrackChanges};

// The reusable PDF/image pipeline (models loaded once, reused across documents),
// for callers that convert many files or want a warm, startup-excluded measurement.
#[cfg(feature = "pdf")]
pub use docling_pdf::{
    model_inventory, page_count as pdf_page_count, render_pages as render_pdf_pages,
    tesseract_lang_arg, Completion, EnrichmentOptions, HeadingHierarchyOptions, ImageOutput,
    ModelEntry, OcrEngine, OcrLang, OcrMode, PageProgress, PageProgressHook, PdfError, Pipeline,
    RenderedPage,
};
// The pure-Rust text-layer extraction (no pdfium, no models) — compiled with
// either PDF feature. The CLI uses it as the `--text-layer-only` fallback when the
// runtime assets are missing (launch blocker: a bare `cargo install` ships
// neither pdfium nor the models).
#[cfg(any(feature = "pdf", feature = "pdf-text"))]
pub use docling_pdf::convert_text_layer_pages as pdf_text_layer_pages;

/// Re-execute under an absolute `argv[0]` so ONNX Runtime finds its GPU
/// provider libraries next to the binary (#664) — see
/// `docling_onnx::reexec_for_provider_libraries`. Binaries call it first
/// thing in `main`; a no-op unless a CUDA/TensorRT build was started by bare
/// name on Linux.
pub fn reexec_for_provider_libraries() {
    #[cfg(feature = "pdf")]
    docling_pdf::ep::reexec_for_provider_libraries();
}

/// The ONNX Runtime the ML stages run on, as `(major, minor)` — the
/// statically linked one (1.31 since #523), or the library an
/// `ort-load-dynamic` build loaded. `None` without the `pdf` feature or
/// when the runtime cannot be asked.
pub fn onnx_runtime_version() -> Option<(u32, u32)> {
    #[cfg(feature = "pdf")]
    {
        docling_pdf::ep::runtime_version()
    }
    #[cfg(not(feature = "pdf"))]
    {
        None
    }
}

/// Which PDF conversion this build compiled in: the full ML pipeline (`pdf`
/// feature), the pure-Rust text-layer path (`pdf-text`, the wasm32 build), or
/// neither. Compile-time facts, exported so downstream crates (whose own
/// features can't see this crate's) can branch — e.g. docling-wasm's host
/// tests, where workspace feature unification may pull `pdf` in.
pub const PDF_ML_COMPILED: bool = cfg!(feature = "pdf");
/// True when the `pdf-text` text-layer-only PDF path is compiled in.
pub const PDF_TEXT_COMPILED: bool = cfg!(feature = "pdf-text");

/// Stand-in for `docling_pdf::EnrichmentOptions` when the `pdf` feature is
/// off: the `DocumentConverter` builder methods keep compiling (and stay
/// inert — the formats these flags affect are rejected at convert time).
#[cfg(not(feature = "pdf"))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EnrichmentOptions {
    /// Classify each picture with DocumentFigureClassifier (26 classes).
    pub picture_classification: bool,
    /// Rewrite code blocks (and detect their language) with CodeFormulaV2.
    pub code: bool,
    /// Decode display formulas to LaTeX with CodeFormulaV2.
    pub formula: bool,
}
