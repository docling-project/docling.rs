//! docling-crab: a Rust port of [docling](https://github.com/docling-project/docling).
//!
//! The public surface mirrors the Python SDK, kept deliberately small:
//!
//! ```no_run
//! use docling_crab::{DocumentConverter, SourceDocument};
//!
//! let converter = DocumentConverter::new();
//! let result = converter
//!     .convert(SourceDocument::from_file("input.md").unwrap())
//!     .unwrap();
//! println!("{}", result.document.export_to_markdown());
//! ```
//!
//! See `MIGRATION.md` for the architecture, the Python → Rust mapping, and the
//! phased plan. Phase 0 ships the converter plumbing plus Markdown and CSV
//! backends; PDF/DOCX/HTML and the ML pipeline land in later phases.

mod converter;
mod error;
mod format;
mod result;
mod source;

pub mod backend;

pub use converter::DocumentConverter;
pub use error::ConversionError;
pub use format::InputFormat;
pub use result::{ConversionResult, ConversionStatus};
pub use source::SourceDocument;

// Re-export the core model so callers only need the one crate, and so
// `result.document.export_to_markdown()` works without an extra import.
pub use docling_crab_core::{DocItemLabel, DoclingDocument, ImageMode, Node, PictureImage, Table};
