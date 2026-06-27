//! Format backends.
//!
//! A backend parses one source format into a [`DoclingDocument`]. This mirrors
//! docling's `DeclarativeDocumentBackend`: formats whose structure can be read
//! directly, without the page-level ML recognition pipeline.
//!
//! Paginated/ML backends (PDF, images) will get a richer trait in a later phase
//! — see `MIGRATION.md`.

use fleischwolf_core::DoclingDocument;

use crate::error::ConversionError;
use crate::source::SourceDocument;

/// Compile a regex once per call site and return a `&'static Regex`. An
/// MSRV-friendly stand-in for `LazyLock` (uses `OnceLock`, stable since 1.70).
/// In textual scope for the backend submodules declared below.
macro_rules! cached_regex {
    ($pat:expr) => {{
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| regex::Regex::new($pat).unwrap())
    }};
}

mod asciidoc;
mod csv;
mod deepseek;
mod docling_json;
mod docx;
mod email;
mod epub;
mod html;
pub(crate) mod images;
mod jats;
mod latex;
mod markdown;
mod odf;
mod omml;
mod ooxml;
mod pptx;
mod uspto;
mod webvtt;
pub(crate) mod xbrl;
mod xlsx;

pub use asciidoc::AsciiDocBackend;
pub use csv::CsvBackend;
pub use deepseek::{is_deepseek_markdown, DeepSeekBackend};
pub use docling_json::DoclingJsonBackend;
pub use docx::DocxBackend;
pub use email::EmailBackend;
pub use epub::EpubBackend;
pub(crate) use html::convert_html;
pub use html::HtmlBackend;
pub(crate) use images::{FsImageResolver, MapImageResolver, NoFetch};
pub use jats::JatsBackend;
pub use latex::LatexBackend;
pub use markdown::MarkdownBackend;
pub use odf::OdfBackend;
pub use pptx::PptxBackend;
pub use uspto::UsptoBackend;
pub use webvtt::WebVttBackend;
pub use xbrl::XbrlBackend;
pub use xlsx::XlsxBackend;

/// A backend that converts a source straight into a [`DoclingDocument`].
pub trait DeclarativeBackend {
    /// Parse `source` into a document.
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError>;
}
