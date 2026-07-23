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
//! See `docs/MIGRATION.md` for the architecture, the Python → Rust mapping, and the
//! phased plan. Phase 0 ships the converter plumbing plus Markdown and CSV
//! backends; PDF/DOCX/HTML and the ML pipeline land in later phases.

pub mod chunks;
mod converter;
pub mod dclx;
mod error;
mod format;
mod result;
mod source;
#[cfg(feature = "pdf")]
mod stream;

pub mod backend;
#[cfg(feature = "asr")]
pub mod video;

pub use converter::{parse_page_range, DocumentConverter, DEFAULT_VIDEO_FRAMES};
pub use error::ConversionError;
pub use format::InputFormat;
pub use result::{ConversionResult, ConversionStatus};
pub use source::SourceDocument;
#[cfg(feature = "pdf")]
pub use stream::MarkdownStream;

// Re-export the core model so callers only need the one crate, and so
// `result.document.export_to_markdown()` works without an extra import.
pub use docling_core::chunker;
pub use docling_core::{
    DocItemLabel, DoclingDocument, ImageMode, MarkdownStreamer, Node, PictureImage, Table,
};

// The reusable PDF/image pipeline (models loaded once, reused across documents),
// for callers that convert many files or want a warm, startup-excluded measurement.
#[cfg(feature = "pdf")]
pub use docling_pdf::{EnrichmentOptions, OcrLang, Pipeline};

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
