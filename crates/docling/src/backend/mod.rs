//! Format backends.
//!
//! A backend parses one source format into a [`DoclingDocument`]. This mirrors
//! docling's `DeclarativeDocumentBackend`: formats whose structure can be read
//! directly, without the page-level ML recognition pipeline.
//!
//! Paginated/ML backends (PDF, images) will get a richer trait in a later phase
//! — see `MIGRATION.md`.

use docling_core::DoclingDocument;

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
#[cfg(feature = "web-browser")]
pub(crate) mod browser;
mod csv;
mod deepseek;
mod doclang;
mod docling_json;
mod docx;
mod email;
mod epub;
mod html;
pub(crate) mod images;
pub(crate) mod jats;
mod latex;
mod markdown;
mod mhtml;
mod odf;
mod omml;
mod ooxml;
mod pptx;
pub(crate) mod uspto;
mod uspto_entities;
mod webvtt;
pub(crate) mod xbrl;
mod xlsx;
mod xlsx_drawings;

pub use asciidoc::AsciiDocBackend;
pub use csv::CsvBackend;
pub use deepseek::{is_deepseek_markdown, DeepSeekBackend};
pub use doclang::DoclangBackend;
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
pub use mhtml::MhtmlBackend;
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

/// Optional headless-browser HTML pre-render, shared by every HTML-routing path
/// (the direct HTML backend via the converter, plus MHTML/EPUB, which assemble
/// HTML from their archives). Returns `html` unchanged unless `use_web_browser`
/// is set, in which case the browser strips computed-hidden elements — requiring
/// the `web-browser` feature, else a clear error rather than a silent no-op.
pub(crate) fn maybe_prerender_html(
    html: &str,
    use_web_browser: bool,
) -> Result<std::borrow::Cow<'_, str>, ConversionError> {
    if !use_web_browser {
        return Ok(std::borrow::Cow::Borrowed(html));
    }
    #[cfg(feature = "web-browser")]
    {
        browser::render_visible_html(html)
            .map(std::borrow::Cow::Owned)
            .map_err(ConversionError::Browser)
    }
    #[cfg(not(feature = "web-browser"))]
    {
        Err(ConversionError::Browser(
            "this build has no web-browser support; rebuild with `--features web-browser`".into(),
        ))
    }
}
