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
//! See `MIGRATION.md` for the architecture, the Python → Rust mapping, and the
//! phased plan. Phase 0 ships the converter plumbing plus Markdown and CSV
//! backends; PDF/DOCX/HTML and the ML pipeline land in later phases.

mod converter;
pub mod dclx;
mod error;
mod format;
mod result;
mod source;
mod stream;

pub mod backend;

pub use converter::DocumentConverter;
pub use error::ConversionError;
pub use format::InputFormat;
pub use result::{ConversionResult, ConversionStatus};
pub use source::SourceDocument;
pub use stream::MarkdownStream;

// Re-export the core model so callers only need the one crate, and so
// `result.document.export_to_markdown()` works without an extra import.
pub use docling_core::chunker;
pub use docling_core::{
    DocItemLabel, DoclingDocument, ImageMode, MarkdownStreamer, Node, PictureImage, Table,
};

// The reusable PDF/image pipeline (models loaded once, reused across documents),
// for callers that convert many files or want a warm, startup-excluded measurement.
pub use docling_pdf::{EnrichmentOptions, Pipeline};
