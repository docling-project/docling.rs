//! Core data model for docling.rs.
//!
//! This crate is the Rust counterpart of the `docling-core` Python package: it
//! owns the unified [`DoclingDocument`] representation that every backend
//! produces and every serializer consumes. Keeping it dependency-light and
//! separate from the conversion logic mirrors the Python split between
//! `docling-core` (the schema) and `docling` (the converters).
//!
//! Phase 0 models a simplified, linear node tree that is enough to round-trip
//! through Markdown. The faithful, `$ref`-based schema that matches
//! docling-core's JSON wire format lands in Phase 1 (see `docs/MIGRATION.md`).

pub mod assets;
pub mod base64;
pub mod chunker;
pub mod confidence;
mod doclang;
mod doclang_tree;
mod doclang_xml;
pub mod doctags;
mod document;
mod encryption;
pub mod env;
mod html;
pub mod hyperlink;
pub mod image_dpi;
pub mod inline;
pub mod jpeg;
mod json;
mod labels;
mod latex;
pub mod limits;
mod markdown;
mod mathml;
pub mod memory;
pub mod metadata;
pub mod pandoc;
pub mod pdf_repair;
mod pixel_digest;
pub mod redact;
mod revisions;
pub mod tree;
mod vtt;

pub use confidence::{ConfidenceReport, OcrCellConfidence, PageConfidence, QualityGrade};
pub use doclang::inline_runs_from_markdown;
pub use doclang_xml::doclang_apply_reading_rules;
pub use document::{
    for_each_picture_in, for_each_picture_in_mut, inline_paragraph_node, CaptionParent,
    ContentLayer, ContentLayers, DoclingDocument, FieldItem, GraphCell, GraphLink,
    HtmlExportOptions, InlineRun, ListItemDclx, ListItemSplit, MarkdownExportOptions, Node, Notice,
    NoticeKind, PictureClass, PictureDescription, PictureImage, Script, ScriptRun, ScriptRunProv,
    Table, TableCell, TableStructure,
};
pub use encryption::EncryptionError;
#[doc(hidden)]
pub use html::to_html_with_picture_refs;
pub use json::code_language_label;
pub use labels::DocItemLabel;
pub use latex::escape_latex;
pub use limits::LimitExceeded;
pub use markdown::{code_fence, code_span, longest_backtick_run, ImageMode, MarkdownStreamer};
pub use metadata::{DocumentMetadata, GpsPosition, MetadataOutput};
pub use pdf_repair::PdfRepair;
pub use redact::{
    CustomPattern, ImageRedaction, PiiDetector, PiiKind, RedactionOptions, RedactionReport,
    Replacement,
};
pub use revisions::{Revision, RevisionKind, TrackChanges};
pub use vtt::VttExportOptions;
