//! Input format enumeration and detection.
//!
//! Mirrors `docling.datamodel.base_models.InputFormat` and its
//! `FormatToExtensions` map.

/// A document format supported (or planned) by docling.rs backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputFormat {
    Docx,
    Pptx,
    Html,
    Image,
    Pdf,
    Asciidoc,
    Md,
    Csv,
    Xlsx,
    Odt,
    Ods,
    Odp,
    XmlUspto,
    XmlJats,
    XmlXbrl,
    XmlDoclang,
    MetsGbs,
    JsonDocling,
    Audio,
    Vtt,
    Latex,
    Email,
    Epub,
    /// MIME HTML archive (`.mhtml`/`.mht`) — a docling.rs extension; docling
    /// has no MHTML backend.
    Mhtml,
}

impl InputFormat {
    /// Stable string identifier, matching the Python enum values.
    pub fn as_str(self) -> &'static str {
        match self {
            InputFormat::Docx => "docx",
            InputFormat::Pptx => "pptx",
            InputFormat::Html => "html",
            InputFormat::Image => "image",
            InputFormat::Pdf => "pdf",
            InputFormat::Asciidoc => "asciidoc",
            InputFormat::Md => "md",
            InputFormat::Csv => "csv",
            InputFormat::Xlsx => "xlsx",
            InputFormat::Odt => "odt",
            InputFormat::Ods => "ods",
            InputFormat::Odp => "odp",
            InputFormat::XmlUspto => "xml_uspto",
            InputFormat::XmlJats => "xml_jats",
            InputFormat::XmlXbrl => "xml_xbrl",
            InputFormat::XmlDoclang => "xml_doclang",
            InputFormat::MetsGbs => "mets_gbs",
            InputFormat::JsonDocling => "json_docling",
            InputFormat::Audio => "audio",
            InputFormat::Vtt => "vtt",
            InputFormat::Latex => "latex",
            InputFormat::Email => "email",
            InputFormat::Epub => "epub",
            InputFormat::Mhtml => "mhtml",
        }
    }

    /// Best-effort format detection from a file extension (case-insensitive).
    ///
    /// Ambiguous extensions (notably bare `xml`) resolve to a single default
    /// here; real disambiguation needs content sniffing, which Phase 1 adds.
    pub fn from_extension(ext: &str) -> Option<Self> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "docx" | "dotx" | "docm" | "dotm" => InputFormat::Docx,
            "pptx" | "potx" | "ppsx" | "pptm" | "potm" | "ppsm" => InputFormat::Pptx,
            "pdf" => InputFormat::Pdf,
            "md" | "txt" | "text" | "qmd" | "rmd" => InputFormat::Md,
            "html" | "htm" | "xhtml" => InputFormat::Html,
            "xml" | "nxml" => InputFormat::XmlJats,
            "dclg" => InputFormat::XmlDoclang,
            "jpg" | "jpeg" | "png" | "tif" | "tiff" | "bmp" | "webp" => InputFormat::Image,
            "adoc" | "asciidoc" | "asc" => InputFormat::Asciidoc,
            "csv" => InputFormat::Csv,
            "xlsx" | "xlsm" => InputFormat::Xlsx,
            "odt" | "ott" => InputFormat::Odt,
            "ods" | "ots" => InputFormat::Ods,
            "odp" | "otp" => InputFormat::Odp,
            "json" => InputFormat::JsonDocling,
            "wav" | "mp3" | "m4a" | "aac" | "ogg" | "flac" | "mp4" | "avi" | "mov" => {
                InputFormat::Audio
            }
            "vtt" => InputFormat::Vtt,
            "tex" | "latex" => InputFormat::Latex,
            "eml" => InputFormat::Email,
            "epub" => InputFormat::Epub,
            "mhtml" | "mht" => InputFormat::Mhtml,
            // METS/Google Books scan packages ship as `*.tar.gz`.
            "gz" | "targz" => InputFormat::MetsGbs,
            _ => return None,
        })
    }
}
