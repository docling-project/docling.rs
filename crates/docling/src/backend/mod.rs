//! Format backends.
//!
//! A backend parses one source format into a [`DoclingDocument`]. This mirrors
//! docling's `DeclarativeDocumentBackend`: formats whose structure can be read
//! directly, without the page-level ML recognition pipeline.
//!
//! Paginated/ML backends (PDF, images) will get a richer trait in a later phase
//! — see `docs/MIGRATION.md`.

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

mod abw;
pub(crate) mod afp;
mod asciidoc;
pub(crate) mod boxnote;
#[cfg(feature = "web-browser")]
pub(crate) mod browser;
pub(crate) mod calendar;
pub(crate) mod cfb;
pub(crate) mod cgm;
mod chandra;
mod code_language;
mod csv;
mod deepseek;
pub(crate) mod doc;
pub(crate) mod doclang;
mod doclang_tree;
mod docling_json;
mod docx;
#[cfg(test)]
mod docx_chain_parity;
mod docx_drawingml;
mod docx_hidden;
mod docx_tree;
#[cfg(test)]
mod docx_upstream_unit;
mod dos_codepage;
mod ebcdic;
mod email;
pub(crate) mod epub;
pub mod feed;
mod html;
mod html_tree;
pub(crate) mod hwp;
pub mod images;
mod interchange;
pub mod ipynb;
pub(crate) mod iwork;
mod iwork_cells;
mod iwork_charts;
pub(crate) mod jats;
mod keynote;
mod keynote_iwa;
mod keynote_xml;
mod latex;
mod latex_walker;
mod lotus;
pub(crate) mod lwp;
pub(crate) mod macwp;
mod markdown;
mod mc;
mod md_tree;
mod metafile;
mod mhtml;
pub(crate) mod msg;
mod numbers;
mod numbers_iwa;
mod numbers_xml;
mod numfmt;
mod odf;
mod odf_tree;
pub(crate) mod offcrypto;
mod officeart;
mod omml;
pub(crate) mod ooxml;
mod pages;
mod pages_iwa;
mod pages_xml;
mod palmdoc;
mod ppt;
mod pptx;
mod pptx_chart;
#[cfg(test)]
mod pptx_upstream_unit;
mod print_file;
mod quattro;
pub(crate) mod ris;
mod rtf;
mod staroffice5;
mod styled;
pub(crate) mod svg;
mod symbol_fonts;
pub mod text;
#[cfg(feature = "youtube")]
pub mod youtube;
// DjVu (#434): pure-Rust decode via `djvu-rs`, so it builds everywhere,
// wasm included.
pub(crate) mod djvu;
pub use cgm::CgmBackend;
pub use djvu::looks_like_djvu;
mod t602;
pub(crate) mod uspto;
mod uspto_entities;
mod visio;
mod webvtt;
mod wpd;
mod wps;
pub(crate) mod xbrl;
mod xbrl_dts;
mod xls;
mod xlsx;
mod xlsx_drawings;
mod xlsx_richdata;
pub(crate) mod xml_depth;

pub use abw::AbwBackend;
pub use afp::AfpBackend;
pub use asciidoc::AsciiDocBackend;
pub use boxnote::BoxNoteBackend;
pub use calendar::CalendarBackend;
pub use chandra::looks_like_chandra;
#[cfg(feature = "vlm")]
pub(crate) use chandra::parse_chandra_pages as chandra_pages;
pub use csv::CsvBackend;
pub use deepseek::{is_deepseek_markdown, is_unlimited_ocr_markdown, DeepSeekBackend};
#[cfg(feature = "vlm")]
pub(crate) use deepseek::{
    normalize_unlimited_ocr as normalize_unlimited, parse_deepseek_text as deepseek_text,
};
pub use doc::DocBackend;
pub use doclang::DoclangBackend;
pub use docling_json::DoclingJsonBackend;
pub use docx::DocxBackend;
pub use ebcdic::EbcdicBackend;
pub use email::EmailBackend;
pub use epub::EpubBackend;
pub use feed::FeedBackend;
pub(crate) use html::convert_html;
pub(crate) use html::decode_html_bytes;
pub use html::HtmlBackend;
pub use hwp::HwpBackend;
pub(crate) use images::{FsImageResolver, MapImageResolver, NoFetch};
pub use interchange::InterchangeBackend;
pub use ipynb::{IpynbBackend, IpynbOutputs};
pub use iwork::IworkBackend;
pub use jats::JatsBackend;
pub use latex::LatexBackend;
pub use lotus::LotusBackend;
pub use lwp::LwpBackend;
pub use macwp::MacWpBackend;
pub use markdown::MarkdownBackend;
pub use mhtml::MhtmlBackend;
pub(crate) use odf::convert_odf;
pub use odf::OdfBackend;
pub(crate) use palmdoc::looks_like_palmdoc;
pub use palmdoc::PalmDocBackend;
pub use ppt::PptBackend;
pub use pptx::PptxBackend;
pub use print_file::PrnBackend;
pub use quattro::QuattroBackend;
pub use ris::RisBackend;
pub use rtf::RtfBackend;
pub use staroffice5::StarOffice5Backend;
pub use svg::SvgBackend;
pub(crate) use t602::looks_like_t602;
pub use t602::T602Backend;
pub use text::TextBackend;
pub use uspto::UsptoBackend;
pub use visio::VisioBackend;
pub use webvtt::WebVttBackend;
pub use wpd::WpdBackend;
pub use wps::WpsBackend;
pub use xbrl::XbrlBackend;
pub use xls::XlsBackend;
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
