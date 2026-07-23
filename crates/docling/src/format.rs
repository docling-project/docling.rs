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
    /// Word 97–2004 binary (`.doc`) — parsed natively (CFB + MS-DOC), no
    /// external converter (docling shells out to LibreOffice for these).
    Doc,
    /// Excel 97–2004 binary (`.xls`, BIFF8) — parsed natively via calamine.
    Xls,
    /// PowerPoint 97–2003 binary (`.ppt`) — parsed natively (CFB + MS-PPT).
    Ppt,
    Odt,
    Ods,
    Odp,
    XmlUspto,
    XmlJats,
    XmlXbrl,
    XmlDoclang,
    /// Raw DocTags markup (`.doctags`/`.dt`) — the token stream docling's
    /// VLMs emit, parsed by `docling_core::doctags` (#152).
    DocTags,
    /// A DocLang OPC archive (`.dclx`, the format `--to dclx` writes).
    Dclx,
    MetsGbs,
    JsonDocling,
    Audio,
    /// Video containers (`.mp4`/`.avi`/`.mov`/`.mkv`/`.webm`) — Phase 1 of
    /// issue #138 transcribes the audio track through the ASR pipeline
    /// (mirrors docling's `InputFormat.VIDEO`, v2.114).
    Video,
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
            InputFormat::Doc => "doc",
            InputFormat::Xls => "xls",
            InputFormat::Ppt => "ppt",
            InputFormat::Odt => "odt",
            InputFormat::Ods => "ods",
            InputFormat::Odp => "odp",
            InputFormat::XmlUspto => "xml_uspto",
            InputFormat::XmlJats => "xml_jats",
            InputFormat::XmlXbrl => "xml_xbrl",
            InputFormat::XmlDoclang => "xml_doclang",
            InputFormat::DocTags => "doctags",
            InputFormat::Dclx => "dclx",
            InputFormat::MetsGbs => "mets_gbs",
            InputFormat::JsonDocling => "json_docling",
            InputFormat::Audio => "audio",
            InputFormat::Video => "video",
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
            "doctags" | "dt" => InputFormat::DocTags,
            "dclx" => InputFormat::Dclx,
            "jpg" | "jpeg" | "png" | "tif" | "tiff" | "bmp" | "webp" => InputFormat::Image,
            "adoc" | "asciidoc" | "asc" => InputFormat::Asciidoc,
            "csv" => InputFormat::Csv,
            "xlsx" | "xlsm" => InputFormat::Xlsx,
            // Legacy binary Office (Word/Excel/PowerPoint 97–2003), issue #127.
            // Extension sets mirror docling's FormatToExtensions.
            "doc" | "dot" => InputFormat::Doc,
            "xls" | "xlt" => InputFormat::Xls,
            "ppt" | "pot" | "pps" => InputFormat::Ppt,
            "odt" | "ott" => InputFormat::Odt,
            "ods" | "ots" => InputFormat::Ods,
            "odp" | "otp" => InputFormat::Odp,
            "json" => InputFormat::JsonDocling,
            "wav" | "mp3" | "m4a" | "aac" | "ogg" | "flac" => InputFormat::Audio,
            // Upstream's FormatToExtensions[VIDEO] (docling v2.114, #3768):
            // the audio track transcribes through the same ASR path.
            "mp4" | "avi" | "mov" | "mkv" | "webm" => InputFormat::Video,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_and_video_extensions_split_like_upstream() {
        // docling v2.114 FormatToExtensions: AUDIO and VIDEO are disjoint.
        for ext in ["wav", "mp3", "m4a", "aac", "ogg", "flac"] {
            assert_eq!(InputFormat::from_extension(ext), Some(InputFormat::Audio));
        }
        for ext in ["mp4", "avi", "mov", "mkv", "webm", "MKV"] {
            assert_eq!(InputFormat::from_extension(ext), Some(InputFormat::Video));
        }
        assert_eq!(InputFormat::Video.as_str(), "video");
    }
}
