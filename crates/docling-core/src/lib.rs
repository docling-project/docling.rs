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

pub mod base64;
pub mod chunker;
mod doclang;
pub mod doctags;
mod document;
mod json;
mod labels;
mod markdown;

pub use doclang::inline_runs_from_markdown;
pub use document::{
    inline_paragraph_node, ContentLayer, DoclingDocument, FieldItem, InlineRun, ListItemDclx, Node,
    PictureClass, PictureImage, Script, Table, TableStructure,
};
pub use labels::DocItemLabel;
pub use markdown::{ImageMode, MarkdownStreamer};
