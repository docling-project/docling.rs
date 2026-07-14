//! The top-level `DocumentConverter`.

use std::collections::HashSet;

use crate::backend::{
    is_deepseek_markdown, AsciiDocBackend, CsvBackend, DeclarativeBackend, DeepSeekBackend,
    DoclingJsonBackend, DocxBackend, EmailBackend, EpubBackend, JatsBackend, LatexBackend,
    MarkdownBackend, MhtmlBackend, OdfBackend, PptxBackend, UsptoBackend, WebVttBackend,
    XbrlBackend, XlsxBackend,
};

/// Whether `text` begins with an XML prolog — an `<?xml …?>` declaration or a
/// non-HTML `<!DOCTYPE …>`. Used to route XML documents that arrived with a
/// text/Markdown extension (e.g. a JATS article saved as `.txt`) to the XML
/// backends. An HTML5 `<!DOCTYPE html>` is deliberately excluded.
fn looks_like_xml(text: &str) -> bool {
    let head = text.trim_start();
    if head.starts_with("<?xml") {
        return true;
    }
    if let Some(rest) = head.get(..9) {
        if rest.eq_ignore_ascii_case("<!doctype") {
            return !head[9..]
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("html");
        }
    }
    false
}

/// Pick the concrete XML backend for a generic `.xml` source by sniffing its
/// DOCTYPE / root element (the first part of the file).
fn sniff_xml(text: &str) -> InputFormat {
    let head = &text[..text.len().min(4000)];
    if head.contains("us-patent")
        || head.contains("patent-application-publication")
        || head.contains("PATDOC")
        || head.contains("<pap-v1")
    {
        InputFormat::XmlUspto
    } else if head.contains("<doclang") {
        // A bare DocLang document saved as `.xml` (docling names them
        // `*.dclg.xml`, whose final extension is plain `xml`).
        InputFormat::XmlDoclang
    } else if crate::backend::xbrl::looks_like_xbrl(head) {
        InputFormat::XmlXbrl
    } else {
        InputFormat::XmlJats
    }
}
use crate::error::ConversionError;
use crate::format::InputFormat;
use crate::result::{ConversionResult, ConversionStatus};
use crate::source::SourceDocument;
use crate::stream::MarkdownStream;
use docling_core::ImageMode;

/// Routes a [`SourceDocument`] to the backend for its format and returns a
/// [`ConversionResult`].
///
/// The Rust analogue of `docling.document_converter.DocumentConverter`. In
/// Phase 0 the format→backend dispatch is a direct match; the Python notion of
/// per-format `FormatOption` (backend + pipeline + options) arrives with the
/// PDF/ML pipeline in a later phase.
#[derive(Debug, Default, Clone)]
pub struct DocumentConverter {
    allowed_formats: Option<HashSet<InputFormat>>,
    strict: bool,
    fetch_images: bool,
    no_table_former: bool,
    no_ocr: bool,
    use_web_browser: bool,
    /// Opt-in PDF/image enrichment models (docling's
    /// `do_picture_classification` / `do_code_enrichment` /
    /// `do_formula_enrichment`).
    enrich: docling_pdf::EnrichmentOptions,
}

impl DocumentConverter {
    /// A converter that accepts every supported format.
    pub fn new() -> Self {
        Self::default()
    }

    /// A converter restricted to an explicit set of formats. Sources of any
    /// other format are rejected with [`ConversionError::UnsupportedFormat`].
    pub fn with_allowed_formats(formats: impl IntoIterator<Item = InputFormat>) -> Self {
        Self {
            allowed_formats: Some(formats.into_iter().collect()),
            strict: false,
            fetch_images: false,
            no_table_former: false,
            no_ocr: false,
            use_web_browser: false,
            enrich: docling_pdf::EnrichmentOptions::default(),
        }
    }

    /// Select the Markdown export mode for documents this converter produces.
    ///
    /// `false` (default) makes [`crate::DoclingDocument::export_to_markdown`]
    /// reproduce docling's legacy output byte-for-byte; `true` makes it emit
    /// cleaner, more conformant Markdown (code-fence languages preserved, no
    /// inline-run spacing artifacts, no entity re-escaping). Rust-only — Python
    /// docling has no such switch.
    pub fn strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Fetch and embed external `<img>` images for HTML/EPUB sources.
    ///
    /// Off by default (matching docling's `enable_*_fetch=False`), so output is
    /// unchanged unless you opt in. When on, the HTML/EPUB backends resolve each
    /// `<img src>` — `data:` URIs, local files (relative to the source file's
    /// directory), `http(s)` URLs, and EPUB archive entries — and embed the
    /// bytes, so they survive into JSON `ImageRef`s and
    /// [`crate::DoclingDocument::export_to_markdown_with_images`].
    ///
    /// Remote `http(s)` URLs are fetched over the network; enable only for input
    /// you trust (it can otherwise be used to make the process issue requests).
    pub fn fetch_images(mut self, fetch: bool) -> Self {
        self.fetch_images = fetch;
        self
    }

    /// Skip loading and running the TableFormer table-structure model for
    /// PDF/image/METS sources.
    ///
    /// Off by default. When enabled, table regions are still detected and
    /// emitted, but their structure is reconstructed geometrically from cell
    /// positions instead of the ONNX model's predicted structure — no model
    /// load and no per-table inference, at the cost of table fidelity. Useful
    /// when parsing speed matters more than exact table structure, especially
    /// with [`convert_streaming`](Self::convert_streaming).
    pub fn no_table_former(mut self, disable: bool) -> Self {
        self.no_table_former = disable;
        self
    }

    /// Skip layout detection, OCR, and TableFormer entirely for PDF/image/METS
    /// sources — no model load, no inference of any kind.
    ///
    /// Off by default. When enabled, the PDF's embedded text cells are grouped by
    /// line and emitted as plain paragraphs in reading order: no headings, lists,
    /// tables, code blocks, or pictures, since that structure comes from the
    /// layout model. The fastest possible PDF path, but pages with no embedded
    /// text layer (scanned/image-only PDFs) yield no text at all — convert those
    /// without this flag. Implies [`no_table_former`](Self::no_table_former).
    pub fn no_ocr(mut self, disable: bool) -> Self {
        self.no_ocr = disable;
        self
    }

    /// Classify each detected picture with the DocumentFigureClassifier model
    /// (docling's `do_picture_classification`). Off by default.
    ///
    /// The full 26-class prediction distribution (bar_chart, logo, signature,
    /// …) lands on the picture item and is serialized into the docling JSON as
    /// the `classification` annotation plus the `meta.classification` field.
    /// Markdown output is unaffected. Needs `models/picture_classifier.onnx`
    /// (fetched by `scripts/install/download_dependencies.sh`); a missing
    /// model warns once and skips classification.
    pub fn do_picture_classification(mut self, enable: bool) -> Self {
        self.enrich.picture_classification = enable;
        self
    }

    /// Rewrite detected code blocks with the CodeFormulaV2 VLM (docling's
    /// `do_code_enrichment`). Off by default.
    ///
    /// The model re-reads the code crop at ~120 dpi, emits the clean source
    /// text (line breaks included) and identifies the language, which lands in
    /// the JSON `code_language` field. Needs the `models/code_formula/` graphs
    /// (fetched by `scripts/install/download_dependencies.sh`); a missing
    /// model warns once and leaves the block as extracted.
    pub fn do_code_enrichment(mut self, enable: bool) -> Self {
        self.enrich.code = enable;
        self
    }

    /// Decode display formulas to LaTeX with the CodeFormulaV2 VLM (docling's
    /// `do_formula_enrichment`). Off by default.
    ///
    /// An enriched formula renders as `$$latex$$` in Markdown and as a
    /// `formula` text item in the JSON, replacing the
    /// `<!-- formula-not-decoded -->` placeholder. Same model artifacts as
    /// [`do_code_enrichment`](Self::do_code_enrichment).
    pub fn do_formula_enrichment(mut self, enable: bool) -> Self {
        self.enrich.formula = enable;
        self
    }

    /// Pre-render HTML-routing input in a headless browser before parsing.
    ///
    /// Off by default. When enabled, HTML sources — and MHTML/EPUB, which
    /// assemble HTML from their archives — are loaded in the system Chromium
    /// (driven from Rust over the DevTools protocol — no Node/Playwright) so the
    /// CSS cascade is resolved: elements the browser computes as `display:none`
    /// (e.g. a stylesheet-collapsed nav menu) are removed before the normal HTML
    /// backend runs. This is the one behaviour a pure-Rust parse can't reproduce;
    /// everything else (structure, tables, KVP, formatting) is still handled in
    /// Rust on the cleaned HTML.
    ///
    /// Requires the crate's `web-browser` Cargo feature; without it, converting
    /// an HTML source with this enabled returns [`ConversionError::Browser`].
    pub fn use_web_browser(mut self, enable: bool) -> Self {
        self.use_web_browser = enable;
        self
    }

    /// Return `html` unchanged, or — when [`use_web_browser`](Self::use_web_browser)
    /// is on — its headless-browser-cleaned form (computed-hidden elements
    /// removed). Borrows in the common (disabled) case; only allocates when the
    /// browser actually runs.
    fn maybe_prerender<'a>(
        &self,
        html: &'a str,
    ) -> Result<std::borrow::Cow<'a, str>, ConversionError> {
        crate::backend::maybe_prerender_html(html, self.use_web_browser)
    }

    /// Convert a source document to Markdown **incrementally**, returning an
    /// iterator of Markdown chunks (with picture placeholders).
    ///
    /// Concatenating every `Ok` chunk reproduces
    /// [`convert`](Self::convert)`(...).document.export_to_markdown()`
    /// byte-for-byte. The win is for PDF, whose pages are processed in parallel:
    /// each page's Markdown is emitted in document order as soon as it is ready, so
    /// output starts before the whole document is converted. Other formats build
    /// their document up front and stream it through the same interface.
    ///
    /// Streaming is Markdown-only — JSON needs the whole node tree, so there is no
    /// streaming JSON. The conversion runs on a background thread; dropping the
    /// returned [`MarkdownStream`] cancels it.
    pub fn convert_streaming(
        &self,
        source: SourceDocument,
    ) -> Result<MarkdownStream, ConversionError> {
        self.convert_streaming_images(source, ImageMode::Placeholder)
    }

    /// Like [`convert_streaming`](Self::convert_streaming) but with an explicit
    /// picture [`ImageMode`]. Only [`ImageMode::Placeholder`] and
    /// [`ImageMode::Embedded`] are streamable; [`ImageMode::Referenced`] writes
    /// separate image files and needs the buffered
    /// [`DoclingDocument::export_to_markdown_with_images`] path, so it is rejected
    /// here.
    ///
    /// [`DoclingDocument::export_to_markdown_with_images`]: docling_core::DoclingDocument::export_to_markdown_with_images
    pub fn convert_streaming_images(
        &self,
        source: SourceDocument,
        image_mode: ImageMode,
    ) -> Result<MarkdownStream, ConversionError> {
        if image_mode == ImageMode::Referenced {
            return Err(ConversionError::Streaming(
                "referenced image mode writes image files; use convert(...).document.\
                 export_to_markdown_with_images(ImageMode::Referenced, ..) instead"
                    .into(),
            ));
        }
        if let Some(allowed) = &self.allowed_formats {
            if !allowed.contains(&source.format) {
                return Err(ConversionError::UnsupportedFormat(source.format));
            }
        }
        Ok(crate::stream::spawn(
            self.clone(),
            source,
            image_mode,
            self.strict,
            self.no_table_former,
            self.no_ocr,
            self.enrich,
        ))
    }

    /// Convert a single source document.
    pub fn convert(&self, source: SourceDocument) -> Result<ConversionResult, ConversionError> {
        if let Some(allowed) = &self.allowed_formats {
            if !allowed.contains(&source.format) {
                return Err(ConversionError::UnsupportedFormat(source.format));
            }
        }

        let mut document = match source.format {
            // A legacy APS (Automated Patent System) plain-text patent (`PATN`
            // first record) is reconstructed verbatim, mirroring docling.
            InputFormat::Md if crate::backend::uspto::looks_like_aps(source.text()?) => {
                crate::backend::uspto::convert_aps(&source)?
            }
            // A text/Markdown-typed file that is actually an XML document (e.g. a
            // JATS article saved with a `.txt` extension) routes to the XML
            // backends by content, mirroring docling's content-based detection.
            InputFormat::Md if looks_like_xml(source.text()?) => match sniff_xml(source.text()?) {
                InputFormat::XmlUspto => UsptoBackend.convert(&source)?,
                InputFormat::XmlXbrl => XbrlBackend.convert(&source)?,
                // A JATS/other XML document saved as `.txt` is reconstructed
                // generically (element-by-element), as docling does — the
                // semantic JATS backend is only used for real `.xml`/`.nxml`.
                _ => crate::backend::jats::convert_generic(&source)?,
            },
            // DeepSeek-OCR annotated Markdown (VLM token format) is detected by
            // its `<|ref|>…[[bbox]]` annotations and parsed separately.
            InputFormat::Md if is_deepseek_markdown(source.text()?) => {
                DeepSeekBackend.convert(&source)?
            }
            InputFormat::Md => MarkdownBackend {
                strict: self.strict,
            }
            .convert(&source)?,
            InputFormat::Csv => CsvBackend.convert(&source)?,
            InputFormat::Html => {
                // Optionally resolve the CSS cascade in a headless browser first
                // (strips computed-hidden elements); everything else stays in the
                // Rust HTML backend, which runs on the cleaned HTML.
                let html = self.maybe_prerender(source.text()?)?;
                if self.fetch_images {
                    let resolver = crate::backend::FsImageResolver::new(
                        source.base_dir().map(|p| p.to_path_buf()),
                    );
                    crate::backend::convert_html(&source.name, &html, &resolver)
                } else {
                    crate::backend::convert_html(&source.name, &html, &crate::backend::NoFetch)
                }
            }
            InputFormat::Asciidoc => AsciiDocBackend.convert(&source)?,
            InputFormat::Xlsx => XlsxBackend.convert(&source)?,
            InputFormat::Pptx => PptxBackend.convert(&source)?,
            InputFormat::Docx => DocxBackend.convert(&source)?,
            InputFormat::Vtt => WebVttBackend.convert(&source)?,
            InputFormat::Email => EmailBackend.convert(&source)?,
            InputFormat::Mhtml => MhtmlBackend {
                use_web_browser: self.use_web_browser,
            }
            .convert(&source)?,
            InputFormat::Epub => EpubBackend {
                fetch_images: self.fetch_images,
                use_web_browser: self.use_web_browser,
            }
            .convert(&source)?,
            InputFormat::JsonDocling => DoclingJsonBackend.convert(&source)?,
            InputFormat::Latex => LatexBackend.convert(&source)?,
            // A bare `.xml` defaults to XmlJats; sniff the content to route to the
            // right XML backend (docling distinguishes by DOCTYPE / root element).
            InputFormat::XmlJats | InputFormat::XmlUspto | InputFormat::XmlXbrl => {
                match sniff_xml(source.text()?) {
                    InputFormat::XmlUspto => UsptoBackend.convert(&source)?,
                    InputFormat::XmlXbrl => XbrlBackend.convert(&source)?,
                    _ => JatsBackend.convert(&source)?,
                }
            }
            InputFormat::Odt | InputFormat::Ods | InputFormat::Odp => {
                OdfBackend.convert(&source)?
            }
            // DocLang back in: bare XML (`.dclg`/`.dclg.xml`) or the OPC
            // archive `--to dclx` writes.
            InputFormat::XmlDoclang | InputFormat::Dclx => {
                crate::backend::DoclangBackend.convert(&source)?
            }
            InputFormat::Pdf => docling_pdf::convert_with_options(
                &source.bytes,
                None,
                &source.name,
                self.no_table_former,
                self.no_ocr,
                self.enrich,
            )
            .map_err(|e| ConversionError::Parse(e.to_string()))?,
            InputFormat::Image => docling_pdf::convert_image_with_options(
                &source.bytes,
                &source.name,
                self.no_table_former,
                self.no_ocr,
                self.enrich,
            )
            .map_err(|e| ConversionError::Parse(e.to_string()))?,
            InputFormat::MetsGbs => docling_pdf::convert_mets_gbs_with_options(
                &source.bytes,
                &source.name,
                self.no_table_former,
                self.no_ocr,
                self.enrich,
            )
            .map_err(|e| ConversionError::Parse(e.to_string()))?,
            // Audio → Whisper ASR (symphonia decode + ONNX inference); each
            // transcribed segment becomes a `[time: start-end] text` paragraph.
            InputFormat::Audio => docling_asr::convert_audio(&source.bytes, &source.name)
                .map_err(|e| ConversionError::Parse(e.to_string()))?,
        };
        // Carry the mode so `result.document.export_to_markdown()` reflects it.
        document.strict_markdown = self.strict;

        Ok(ConversionResult {
            document,
            status: ConversionStatus::Success,
            input_name: source.name,
            format: source.format,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_to_end_markdown() {
        let src =
            SourceDocument::from_bytes("doc", InputFormat::Md, b"# Hello\n\nWorld.\n".to_vec());
        let result = DocumentConverter::new().convert(src).unwrap();
        assert_eq!(result.status, ConversionStatus::Success);
        assert_eq!(result.document.export_to_markdown(), "# Hello\n\nWorld.\n");
    }

    #[test]
    fn doclang_xml_round_trips() {
        // Every input format now has a backend; DocLang XML reads back in and
        // re-exports as Markdown.
        let xml = b"<doclang version=\"0.7\">\n  <heading>Title</heading>\n  \
                    <text>Hello <bold>world</bold></text>\n</doclang>"
            .to_vec();
        let src = SourceDocument::from_bytes("doc.dclg", InputFormat::XmlDoclang, xml);
        let result = DocumentConverter::new().convert(src).unwrap();
        let md = result.document.export_to_markdown();
        assert!(md.contains("# Title"), "{md}");
        assert!(md.contains("**world**"), "{md}");
    }
}
