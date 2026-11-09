//! Input format enumeration and detection.
//!
//! Mirrors `docling.datamodel.base_models.InputFormat` and its
//! `FormatToExtensions` map.

/// A document format supported by docling.rs backends.
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
    /// Austrian RIS legal XML (`risdok`, #902) — a docling.rs extension;
    /// docling has no RIS reader. A bare `.xml` reaches it by its root.
    XmlRis,
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
    /// EBCDIC mainframe data + JSON copybook layout (#252, docling 2.118).
    Ebcdic,
    Latex,
    Email,
    Epub,
    /// MIME HTML archive (`.mhtml`/`.mht`) — docling's `InputFormat.MHTML`
    /// (docling#4184), unwrapped and converted as HTML.
    Mhtml,
    /// Rich Text Format (`.rtf`) — a docling.rs extension (#209); docling
    /// converts RTF only by shelling out to LibreOffice.
    Rtf,
    /// Microsoft Visio (`.vsdx`, `.vsdm`) — a docling.rs extension (#214);
    /// docling has no Visio reader. Pages become sections, shape text flows
    /// in reading order, connectors become a relations table.
    Visio,
    /// SVG (`.svg`) — a docling.rs extension (#212); docling does not accept
    /// SVG input. Mirrors the pdf / pdf-text split: the ML build rasterizes
    /// (resvg) and rides the image pipeline; without ML — or under `--no-ocr` / `--text-layer-only`
    /// — `<text>` elements are extracted directly into flat paragraphs.
    Svg,
    /// Apple Pages (`.pages`) — a conformance format (#318, #383): mirrors
    /// docling's `IWorkPagesDocumentBackend` for both the 2013+ IWA package
    /// and the iWork '09 `index.xml` generation.
    Pages,
    /// Apple Numbers (`.numbers`), same IWA machinery as [`Self::Pages`].
    Numbers,
    /// Apple Keynote (`.key`), same IWA machinery as [`Self::Pages`].
    Keynote,
    /// AbiWord (`.abw`/`.zabw`/`.awt`) — a docling.rs extension (#216);
    /// AWML XML (gzip-wrapped for `.zabw`), parsed natively.
    Abiword,
    /// WordPerfect 5.x / 6.x+ documents (`.wpd`, `.wp`, `.wp5`, `.wp6`,
    /// `.wpt`) — a docling.rs extension (#216); the `ÿWPC` byte stream is
    /// parsed natively (docling reaches WordPerfect only via LibreOffice's
    /// libwpd), text-level with bold/italic/underline runs.
    WordPerfect,
    /// Microsoft Works word-processor documents (`.wps`) — a docling.rs
    /// extension (#216); Works 2.x DOS / 3 / 4 (`WPS4`) and Works 2000 / 6–9
    /// (`WPS8`, OLE `CONTENTS`) parsed natively after libwps' readers.
    Works,
    /// dBase table (`.dbf`) — a docling.rs extension (#216); the field
    /// descriptors become the header row, records the data rows.
    Dbf,
    /// Data Interchange Format (`.dif`) — a docling.rs extension (#216);
    /// sheet snapshot, split into data regions like an ODS sheet.
    Dif,
    /// SYLK (`.slk`/`.sylk`) — a docling.rs extension (#216); same
    /// sheet-region conversion as DIF.
    Sylk,
    /// Quattro Pro spreadsheets (`.wq1`/`.wq2` DOS, `.wb1`–`.wb3` Windows,
    /// `.qpw` 9–X9) — a docling.rs extension (#216) parsed natively after
    /// libwps' readers.
    QuattroPro,
    /// Lotus 1-2-3 / Symphony / MS Works spreadsheets (`.wk1`–`.wk4`,
    /// `.wks`, `.wrk`, `.123`) — a docling.rs extension (#216); the DOS-era
    /// record streams, content-sniffed on the BOF record and split into data
    /// regions like an ODS sheet.
    Lotus,
    /// StarOffice 5 binaries (`.sdw`/`.sgl`/`.sda`/`.sdd`/`.vor`) — a docling.rs
    /// extension (#215); CFB containers parsed natively (text-level
    /// extraction). The document kind comes from the stream inside, so a
    /// `.vor` template of any application dispatches by content. StarCalc
    /// (`.sdc`) is a follow-up.
    StarOffice5,
    /// T602 / Text602 (`.602`) — the Czechoslovak DOS word processor, a
    /// docling.rs extension (#216): its 8-bit text with `@` settings, style
    /// control bytes and the Kamenický / Latin-2 / KOI8-ČS code pages, read
    /// natively after LibreOffice's T602 filter.
    T602,
    /// Print files (`.prn`) — a DOS or Windows "print to file": a report's
    /// fixed-width text, or a PCL / ESC/P printer stream, a docling.rs extension
    /// (#216); escape sequences and overstrikes taken out, columns kept.
    Prn,
    /// Palm e-texts (`.pdb`) — PalmDOC / AportisDoc, TealDoc and zTXT, a
    /// docling.rs extension (#216): the Palm database's text records
    /// decompressed (PalmDOC LZ77, zlib) into paragraphs. A `.pdb` that is no
    /// Palm database reads as text.
    PalmDoc,
    /// Hancom Hangul word processor documents (`.hwp`, `.hwpx`), a docling.rs
    /// extension (#216): HWP 5 compound files and HWPX (OWPML) packages read
    /// into paragraphs, headings, lists and tables.
    Hwp,
    /// Computer Graphics Metafiles (`.cgm`, ISO/IEC 8632), a docling.rs
    /// extension (#216): the text a drawing places (labels, callouts) in the
    /// binary and clear-text encodings.
    Cgm,
    /// Classic Macintosh word processors — MacWrite (`.mw`), Word for the
    /// Macintosh 1–5 (`.mcw`) and ClarisWorks / AppleWorks 1–6 (`.cwk`), a
    /// docling.rs extension (#216): their text, told apart by content.
    MacWp,
    /// Lotus Word Pro (`.lwp`, Word Pro 97 on), a docling.rs extension (#216):
    /// the object store's stories read into paragraphs and headings.
    Lwp,
    /// DjVu scanned documents (`.djvu`/`.djv`) — a docling.rs extension (#434);
    /// docling has no DjVu reader. Decoded in pure Rust (`djvu-rs`): the
    /// hidden per-page text layer by default, rasterize + OCR for scan-only
    /// pages when the ML pipeline is built.
    Djvu,
    /// Plain text and source code (#671) — MarkItDown's plain-text
    /// converter: a code extension (`.py`, `.rs`, `.yaml`, `.sh`, …) becomes
    /// one code block in that language, anything else paragraphs; also the
    /// fallback for an unknown or missing extension whose bytes are text.
    /// Markdown-like text (`.txt`, `.md`) stays [`Md`](Self::Md), as docling
    /// reads it.
    Text,
    /// JSON Lines (`.jsonl`/`.ndjson`, #671): kept verbatim as one `json`
    /// code block. A `.json` is [`JsonDocling`](Self::JsonDocling), which
    /// renders any other JSON the same way.
    Jsonl,
    /// A bare `.xml` (#671): sniffed into the dialects docling reads — JATS,
    /// USPTO, XBRL, DocLang — or a feed ([`Feed`](Self::Feed), #678), and,
    /// matching none, kept as one `xml` code block with a warning. `.nxml`
    /// is [`XmlJats`](Self::XmlJats) outright.
    Xml,
    /// Jupyter notebook (`.ipynb`, #673): markdown cells as Markdown, code
    /// cells as code items in the kernel's language, raw cells as code
    /// blocks; outputs under `ipynb_outputs`. A `.json` holding a notebook
    /// (`"nbformat"` + `"cells"`) takes the same route.
    Ipynb,
    /// An RSS 2.0, Atom or RSS 1.0 (RDF) feed (`.rss`, `.atom`, `.rdf`,
    /// `application/rss+xml`, `application/atom+xml`, or a bare `.xml`
    /// whose root says so; #678): the feed's title, its description or
    /// subtitle, then per item a level-2 heading, its date and its body
    /// read through the HTML backend — MarkItDown's `_rss_converter`
    /// shape. A docling.rs extension: docling has no feed backend.
    Feed,
    /// An iCalendar file (`.ics`, `.ical`, `.icalendar`, `.ifb`,
    /// `text/calendar`, or a nameless input opening with `BEGIN:VCALENDAR`):
    /// events, tasks, journal entries and free/busy blocks as headings,
    /// `Field: value` paragraphs and attendee / reminder / attachment lists —
    /// docling's `InputFormat.CALENDAR` (docling#4880).
    Calendar,
    /// A format only a backend registered on the converter reads (#680,
    /// [`DocumentConverter::register_backend`](crate::DocumentConverter::register_backend)):
    /// what [`DocumentConverter::load_file`](crate::DocumentConverter::load_file)
    /// and [`load_named_bytes`](crate::DocumentConverter::load_named_bytes)
    /// resolve a source to when a registered backend claims it and the
    /// built-in tables do not know its extension or media type. Never the
    /// answer of an extension, media type or content check of their own; a
    /// converter without a backend that claims the source rejects it.
    Plugin,
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
            InputFormat::XmlRis => "xml_ris",
            InputFormat::XmlDoclang => "xml_doclang",
            InputFormat::DocTags => "doctags",
            InputFormat::Dclx => "dclx",
            InputFormat::MetsGbs => "mets_gbs",
            InputFormat::JsonDocling => "json_docling",
            InputFormat::Audio => "audio",
            InputFormat::Video => "video",
            InputFormat::Vtt => "vtt",
            InputFormat::Ebcdic => "ebc",
            InputFormat::Latex => "latex",
            InputFormat::Email => "email",
            InputFormat::Epub => "epub",
            InputFormat::Mhtml => "mhtml",
            InputFormat::Rtf => "rtf",
            InputFormat::Visio => "visio",
            InputFormat::Svg => "svg",
            InputFormat::Pages => "pages",
            InputFormat::Numbers => "numbers",
            InputFormat::Keynote => "key",
            InputFormat::Abiword => "abiword",
            InputFormat::WordPerfect => "wordperfect",
            InputFormat::Works => "works",
            InputFormat::Dbf => "dbf",
            InputFormat::Dif => "dif",
            InputFormat::Sylk => "sylk",
            InputFormat::Lotus => "lotus",
            InputFormat::QuattroPro => "quattro",
            InputFormat::StarOffice5 => "staroffice5",
            InputFormat::T602 => "t602",
            InputFormat::Prn => "prn",
            InputFormat::PalmDoc => "palmdoc",
            InputFormat::Hwp => "hwp",
            InputFormat::Cgm => "cgm",
            InputFormat::MacWp => "mac_wp",
            InputFormat::Lwp => "lwp",
            InputFormat::Djvu => "djvu",
            InputFormat::Text => "text",
            InputFormat::Jsonl => "jsonl",
            InputFormat::Xml => "xml",
            InputFormat::Ipynb => "ipynb",
            InputFormat::Feed => "feed",
            InputFormat::Calendar => "calendar",
            InputFormat::Plugin => "plugin",
        }
    }

    /// The inverse of [`as_str`](Self::as_str): the format for one of its
    /// stable identifiers (`"pdf"`, `"xml_jats"`, …; case-insensitive), as the
    /// bindings receive it. `None` for anything else.
    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id.trim().to_ascii_lowercase().as_str() {
            "docx" => InputFormat::Docx,
            "pptx" => InputFormat::Pptx,
            "html" => InputFormat::Html,
            "image" => InputFormat::Image,
            "pdf" => InputFormat::Pdf,
            "asciidoc" => InputFormat::Asciidoc,
            "md" => InputFormat::Md,
            "csv" => InputFormat::Csv,
            "xlsx" => InputFormat::Xlsx,
            "doc" => InputFormat::Doc,
            "xls" => InputFormat::Xls,
            "ppt" => InputFormat::Ppt,
            "odt" => InputFormat::Odt,
            "ods" => InputFormat::Ods,
            "odp" => InputFormat::Odp,
            "xml_uspto" => InputFormat::XmlUspto,
            "xml_jats" => InputFormat::XmlJats,
            "xml_xbrl" => InputFormat::XmlXbrl,
            "xml_ris" => InputFormat::XmlRis,
            "xml_doclang" => InputFormat::XmlDoclang,
            "doctags" => InputFormat::DocTags,
            "dclx" => InputFormat::Dclx,
            "mets_gbs" => InputFormat::MetsGbs,
            "json_docling" => InputFormat::JsonDocling,
            "audio" => InputFormat::Audio,
            "video" => InputFormat::Video,
            "vtt" => InputFormat::Vtt,
            "ebc" => InputFormat::Ebcdic,
            "latex" => InputFormat::Latex,
            "email" => InputFormat::Email,
            "epub" => InputFormat::Epub,
            "mhtml" => InputFormat::Mhtml,
            "rtf" => InputFormat::Rtf,
            "visio" => InputFormat::Visio,
            "svg" => InputFormat::Svg,
            "pages" => InputFormat::Pages,
            "numbers" => InputFormat::Numbers,
            "key" => InputFormat::Keynote,
            "abiword" => InputFormat::Abiword,
            "wordperfect" => InputFormat::WordPerfect,
            "works" => InputFormat::Works,
            "dbf" => InputFormat::Dbf,
            "dif" => InputFormat::Dif,
            "sylk" => InputFormat::Sylk,
            "lotus" => InputFormat::Lotus,
            "quattro" => InputFormat::QuattroPro,
            "staroffice5" => InputFormat::StarOffice5,
            "t602" => InputFormat::T602,
            "prn" => InputFormat::Prn,
            "palmdoc" => InputFormat::PalmDoc,
            "hwp" => InputFormat::Hwp,
            "cgm" => InputFormat::Cgm,
            "mac_wp" => InputFormat::MacWp,
            "lwp" => InputFormat::Lwp,
            "djvu" => InputFormat::Djvu,
            "text" => InputFormat::Text,
            "jsonl" => InputFormat::Jsonl,
            "xml" => InputFormat::Xml,
            "ipynb" => InputFormat::Ipynb,
            "feed" => InputFormat::Feed,
            "calendar" => InputFormat::Calendar,
            "plugin" => InputFormat::Plugin,
            _ => return None,
        })
    }

    /// Best-effort format detection from a media type (`type/subtype`,
    /// parameters and case ignored) — what an email attachment or an HTTP
    /// response declares when its name has no usable extension (#561). The
    /// generic `application/octet-stream` is `None`: it says nothing.
    pub fn from_mime(mime: &str) -> Option<Self> {
        let essence = mime
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        Some(match essence.as_str() {
            "application/pdf" | "application/x-pdf" => InputFormat::Pdf,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.template"
            | "application/vnd.ms-word.document.macroenabled.12" => InputFormat::Docx,
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            | "application/vnd.openxmlformats-officedocument.presentationml.slideshow"
            | "application/vnd.openxmlformats-officedocument.presentationml.template"
            | "application/vnd.ms-powerpoint.presentation.macroenabled.12" => InputFormat::Pptx,
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.template"
            | "application/vnd.ms-excel.sheet.macroenabled.12"
            | "application/vnd.ms-excel.sheet.binary.macroenabled.12" => InputFormat::Xlsx,
            "application/msword" => InputFormat::Doc,
            "application/vnd.ms-excel" => InputFormat::Xls,
            "application/vnd.ms-powerpoint" => InputFormat::Ppt,
            "application/vnd.oasis.opendocument.text" => InputFormat::Odt,
            "application/vnd.oasis.opendocument.spreadsheet" => InputFormat::Ods,
            "application/vnd.oasis.opendocument.presentation" => InputFormat::Odp,
            "application/rtf" | "text/rtf" => InputFormat::Rtf,
            "application/epub+zip" => InputFormat::Epub,
            "text/html" | "application/xhtml+xml" => InputFormat::Html,
            "text/markdown" | "text/x-markdown" | "text/plain" => InputFormat::Md,
            "text/csv" | "text/tab-separated-values" => InputFormat::Csv,
            "text/x-tex" | "application/x-tex" | "application/x-latex" => InputFormat::Latex,
            "text/vtt" => InputFormat::Vtt,
            "text/calendar" => InputFormat::Calendar,
            "application/json" => InputFormat::JsonDocling,
            "application/x-ndjson" | "application/jsonl" | "application/x-jsonlines" => {
                InputFormat::Jsonl
            }
            "application/xml" | "text/xml" => InputFormat::Xml,
            "application/x-ipynb+json" => InputFormat::Ipynb,
            "application/rss+xml" | "application/atom+xml" | "application/rdf+xml" => {
                InputFormat::Feed
            }
            "message/rfc822" | "application/vnd.ms-outlook" => InputFormat::Email,
            "multipart/related" | "message/rfc822-headers" => return None,
            "image/png" | "image/jpeg" | "image/tiff" | "image/bmp" | "image/webp"
            | "image/gif" | "image/heic" | "image/heif" => InputFormat::Image,
            "image/svg+xml" => InputFormat::Svg,
            // Registered and legacy DjVu spellings (#434).
            "image/vnd.djvu" | "image/x-djvu" | "image/x.djvu" => InputFormat::Djvu,
            "audio/mpeg" | "audio/mp3" | "audio/wav" | "audio/x-wav" | "audio/wave"
            | "audio/mp4" | "audio/m4a" | "audio/x-m4a" | "audio/aac" | "audio/ogg"
            | "audio/flac" => InputFormat::Audio,
            "video/mp4" | "video/quicktime" | "video/x-msvideo" | "video/x-matroska"
            | "video/webm" | "video/mpeg" => InputFormat::Video,
            // Any other `text/*` is text (#671): `text/x-python`,
            // `text/x-log`, `text/yaml`, … — MarkItDown's rule.
            t if t.starts_with("text/") => InputFormat::Text,
            _ => return None,
        })
    }

    /// Best-effort format detection from a file extension (case-insensitive).
    ///
    /// Ambiguous extensions (notably bare `xml`) resolve to a single default
    /// here; the converter's content sniffing does the real disambiguation.
    /// The mapping is the [`EXTENSIONS`] table — the one list
    /// [`supported_extensions`](Self::supported_extensions) prints too (#603).
    pub fn from_extension(ext: &str) -> Option<Self> {
        let ext = ext.to_ascii_lowercase();
        EXTENSIONS
            .iter()
            .find(|(known, _)| *known == ext)
            .map(|&(_, format)| format)
    }

    /// The file extensions that route to this format (lowercase, no dot), in
    /// [`EXTENSIONS`] order.
    pub fn extensions(self) -> impl Iterator<Item = &'static str> {
        EXTENSIONS
            .iter()
            .filter(move |(_, format)| *format == self)
            .map(|&(ext, _)| ext)
    }

    /// Whether this build converts the format. Every format stays
    /// *detectable* in every build, but a few need a cargo feature to
    /// convert: PDF the `pdf` ML pipeline or the `pdf-text` text-layer path,
    /// images and METS/GBS scan packages the `pdf` pipeline, audio and video
    /// the `asr` pipeline — without it the converter answers "not compiled
    /// in". Missing *runtime* assets (models, ffmpeg) are not a build
    /// property and do not count here: those formats degrade or fail at
    /// conversion time with a message that names what to fetch.
    pub fn is_compiled_in(self) -> bool {
        match self {
            InputFormat::Pdf => cfg!(any(feature = "pdf", feature = "pdf-text")),
            InputFormat::Image | InputFormat::MetsGbs => cfg!(feature = "pdf"),
            InputFormat::Audio | InputFormat::Video => cfg!(feature = "asr"),
            _ => true,
        }
    }

    /// Every input file extension this build converts — sorted, unique,
    /// lowercase, no dot: what `docling-rs --list-input-formats` prints
    /// (#603, Pandoc's `--list-input-formats`), so a wrapper can ask the
    /// binary instead of hard-coding a list that drifts from it. Formats the
    /// build lacks ([`is_compiled_in`](Self::is_compiled_in)) are left out,
    /// and so are two extensions that only route: `.heic`/`.heif` without the
    /// `heif` feature (libheif — the image pipeline cannot decode them) and
    /// StarCalc's `.sdc`, which reaches the StarOffice backend for its
    /// targeted "save as .ods" error, never a document.
    /// The format `bytes` evidently are, from their content alone (#672) —
    /// what decides for an input with no usable name: stdin, an upload, a
    /// `data:` URL, an email attachment, a file with no or an unknown
    /// extension ([`SourceDocument::from_file`](crate::SourceDocument::from_file)
    /// and [`from_named_bytes`](crate::SourceDocument::from_named_bytes)
    /// call it after the extension and the media type). Signatures only, no
    /// classifier: PDF, RTF, the ZIP packages (OOXML, ODF, EPUB, Visio,
    /// Apple iWork), the OLE files (Word, Excel, PowerPoint, Outlook),
    /// flat Word 2.0, HTML, the raster images, DjVu, the audio and video
    /// containers, WordPerfect, WebVTT, DocTags, SVG, AbiWord, XML (the
    /// `xml` route then reads the dialect), JSON (a Jupyter notebook, else
    /// the docling-JSON route), MIME messages (MHTML by `multipart/related`,
    /// else email) and LaTeX. Prose, Markdown, CSV and AsciiDoc have no
    /// signature and come back `None` — the converter reads such input as
    /// plain text (#671) when the extension says nothing either.
    ///
    /// A correctly named file never pays for this: the extension decides
    /// first. The Python `docling_rs.detect_format` and the Node
    /// `detectFormat` are this function.
    pub fn detect(bytes: &[u8]) -> Option<InputFormat> {
        crate::sniff::detect_any(bytes)
    }

    pub fn supported_extensions() -> Vec<&'static str> {
        let mut out: Vec<&'static str> = EXTENSIONS
            .iter()
            .filter(|(ext, format)| {
                format.is_compiled_in()
                    && *ext != "sdc"
                    && (cfg!(feature = "heif") || !matches!(*ext, "heic" | "heif"))
            })
            .map(|&(ext, _)| ext)
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// The output formats a conversion can be written as — the `--to` values of
/// the CLI and the `to` option of docling-serve, in the order the help text
/// lists them (`markdown` is accepted as an alias of `md` on both). One list,
/// so the validation, the error messages and `--list-output-formats` (#603)
/// cannot drift apart.
pub const OUTPUT_FORMATS: &[&str] = &[
    "md", "json", "html", "text", "dclx", "chunks", "images", "latex", "pandoc", "vtt",
];

/// File extension → format, the table behind
/// [`InputFormat::from_extension`] (lowercase, no dot; each extension once).
/// Extension sets follow docling's `FormatToExtensions` where upstream has
/// the format; the rest are docling.rs extensions, issue numbers inline.
const EXTENSIONS: &[(&str, InputFormat)] = {
    use InputFormat::*;
    &[
        ("docx", Docx),
        ("dotx", Docx),
        ("docm", Docx),
        ("dotm", Docx),
        ("pptx", Pptx),
        ("potx", Pptx),
        ("ppsx", Pptx),
        ("pptm", Pptx),
        ("potm", Pptx),
        ("ppsm", Pptx),
        ("pdf", Pdf),
        // `markdown`: docling#4306.
        ("md", Md),
        ("markdown", Md),
        ("txt", Md),
        ("text", Md),
        ("qmd", Md),
        ("rmd", Md),
        ("html", Html),
        ("htm", Html),
        ("xhtml", Html),
        ("xml", Xml),
        ("nxml", XmlJats),
        ("dclg", XmlDoclang),
        ("doctags", DocTags),
        ("dt", DocTags),
        ("dclx", Dclx),
        // `.gif` decodes through the same content-sniffing `image` path as
        // the rest (first frame of an animation), issue #208. `.heic`/`.heif`
        // decode behind the opt-in `heif` cargo feature (#211).
        ("jpg", Image),
        ("jpeg", Image),
        ("png", Image),
        ("tif", Image),
        ("tiff", Image),
        ("bmp", Image),
        ("webp", Image),
        ("gif", Image),
        ("heic", Image),
        ("heif", Image),
        ("adoc", Asciidoc),
        ("asciidoc", Asciidoc),
        ("asc", Asciidoc),
        // `.tsv` rides the CSV backend, whose delimiter sniffing already
        // prefers the tab when it dominates the first line (#208).
        ("csv", Csv),
        ("tsv", Csv),
        // `.xlsb` (binary Excel 2007+) parses through the same calamine
        // engine as xlsx — the backend detects the binary workbook part and
        // switches readers, issue #210. Excel templates (docling#4178, 2.126):
        // the same OOXML package under the template content type.
        ("xlsx", Xlsx),
        ("xlsm", Xlsx),
        ("xlsb", Xlsx),
        ("xltx", Xlsx),
        ("xltm", Xlsx),
        // Legacy binary Office (Word/Excel/PowerPoint 97–2003), issue #127.
        // Extension sets mirror docling's FormatToExtensions.
        ("doc", Doc),
        ("dot", Doc),
        ("xls", Xls),
        ("xlt", Xls),
        ("ppt", Ppt),
        ("pot", Ppt),
        ("pps", Ppt),
        // StarOffice / OpenOffice 1.x XML and flat ODF (#215, docling.rs
        // extensions): the shared ODF backend parses the older namespace
        // vocabulary through a local-name mapping layer, and the flat
        // variants are the same XML uncompressed in a single file. Templates
        // (`.stw`/`.sti`/`.stc`) and the Writer master document (`.sxg`) ride
        // the same parsers as their document counterparts.
        ("odt", Odt),
        ("ott", Odt),
        ("sxw", Odt),
        ("stw", Odt),
        ("sxg", Odt),
        ("fodt", Odt),
        ("ods", Ods),
        ("ots", Ods),
        ("sxc", Ods),
        ("stc", Ods),
        ("fods", Ods),
        ("odp", Odp),
        ("otp", Odp),
        ("sxi", Odp),
        ("sti", Odp),
        ("fodp", Odp),
        ("json", JsonDocling),
        ("jsonl", Jsonl),
        ("ndjson", Jsonl),
        ("ipynb", Ipynb),
        ("rss", Feed),
        ("atom", Feed),
        ("rdf", Feed),
        ("ics", Calendar),
        ("ical", Calendar),
        ("icalendar", Calendar),
        ("ifb", Calendar),
        // `.mpga` *is* MPEG audio (an mp3 stream) — symphonia probes the
        // codec from the bytes, the extension is just the alias (#208).
        ("wav", Audio),
        ("mp3", Audio),
        ("mpga", Audio),
        ("m4a", Audio),
        ("aac", Audio),
        ("ogg", Audio),
        ("flac", Audio),
        // Upstream's FormatToExtensions[VIDEO] (docling v2.114, #3768): the
        // audio track transcribes through the same ASR path. `.mpeg`/`.mpg`
        // (#208): MPEG-PS has no symphonia demuxer, so both the audio track
        // and the sampled frames come from the ffmpeg fallback; an audio-only
        // `.mpeg` still decodes in-process (the probe is content-based) and
        // converts to its transcript.
        ("mp4", Video),
        ("avi", Video),
        ("mov", Video),
        ("mkv", Video),
        ("webm", Video),
        ("mpeg", Video),
        ("mpg", Video),
        ("vtt", Vtt),
        ("tex", Latex),
        ("latex", Latex),
        ("eml", Email),
        // Outlook .msg (#251): a CFB container of MAPI streams; the email
        // backend sniffs the magic and projects it onto RFC 822.
        ("msg", Email),
        ("ebc", Ebcdic),
        // Plain text and source code (#671): the extensions MarkItDown's
        // plain-text converter would take by `text/*` media type, each as
        // one code block in its language (`crate::backend::text::code_language`)
        // — and `log`, which reads as paragraphs. Any extension not in this
        // table whose bytes are text takes the same route.
        ("py", Text),
        ("pyi", Text),
        ("rs", Text),
        ("js", Text),
        ("mjs", Text),
        ("cjs", Text),
        ("ts", Text),
        ("tsx", Text),
        ("jsx", Text),
        ("java", Text),
        ("kt", Text),
        ("kts", Text),
        ("scala", Text),
        ("groovy", Text),
        ("gradle", Text),
        ("c", Text),
        ("h", Text),
        ("cpp", Text),
        ("cc", Text),
        ("cxx", Text),
        ("hpp", Text),
        ("hh", Text),
        ("cs", Text),
        ("go", Text),
        ("rb", Text),
        ("php", Text),
        ("swift", Text),
        ("m", Text),
        ("mm", Text),
        ("dart", Text),
        ("lua", Text),
        ("pl", Text),
        ("pm", Text),
        ("r", Text),
        ("jl", Text),
        ("hs", Text),
        ("ex", Text),
        ("exs", Text),
        ("erl", Text),
        ("clj", Text),
        ("sh", Text),
        ("bash", Text),
        ("zsh", Text),
        ("fish", Text),
        ("ps1", Text),
        ("bat", Text),
        ("cmd", Text),
        ("sql", Text),
        ("css", Text),
        ("scss", Text),
        ("less", Text),
        ("yaml", Text),
        ("yml", Text),
        ("toml", Text),
        ("ini", Text),
        ("cfg", Text),
        ("conf", Text),
        ("properties", Text),
        ("env", Text),
        ("proto", Text),
        ("graphql", Text),
        ("gql", Text),
        ("dockerfile", Text),
        ("makefile", Text),
        ("mk", Text),
        ("cmake", Text),
        ("diff", Text),
        ("patch", Text),
        ("log", Text),
        ("ebcdic", Ebcdic),
        ("epub", Epub),
        ("mhtml", Mhtml),
        ("mht", Mhtml),
        ("rtf", Rtf),
        ("vsdx", Visio),
        ("vsdm", Visio),
        ("svg", Svg),
        // Apple iWork (#213).
        ("pages", Pages),
        ("numbers", Numbers),
        ("key", Keynote),
        // AbiWord (#216): AWML XML; .zabw is the same file gzip-wrapped, .awt
        // the template flavor.
        ("abw", Abiword),
        ("zabw", Abiword),
        ("awt", Abiword),
        // WordPerfect (#216): `.wp` was the DOS-era default (WP 5.x), `.wpd`
        // the Windows one; `.wpt` is the template flavor. The backend reads
        // the version from the prefix header, not the extension.
        ("wpd", WordPerfect),
        ("wp", WordPerfect),
        ("wp5", WordPerfect),
        ("wp6", WordPerfect),
        ("wpt", WordPerfect),
        // Microsoft Works word processor (#216): the backend tells the
        // generations apart by the stream (raw 2.x header, OLE MN0 for 3/4,
        // OLE CONTENTS for 2000+); .wks/.wdb are the spreadsheet and database
        // and route elsewhere.
        ("wps", Works),
        // Legacy spreadsheet-interchange relics (#216): all three parse
        // natively and content-sniff inside one backend.
        ("dbf", Dbf),
        ("dif", Dif),
        ("slk", Sylk),
        ("sylk", Sylk),
        // The Lotus family (#216): .wks is ambiguous (1-2-3 rel 1A and MS
        // Works v3 both used it) — the backend sniffs the BOF.
        ("wk1", Lotus),
        ("wk2", Lotus),
        ("wk3", Lotus),
        ("wk4", Lotus),
        ("wks", Lotus),
        ("wrk", Lotus),
        ("123", Lotus),
        // Quattro Pro (#216): the cell records differ per generation, so the
        // family has its own reader (the backend sniffs the BOF / OLE stream,
        // not the extension).
        ("wq1", QuattroPro),
        ("wq2", QuattroPro),
        ("wb1", QuattroPro),
        ("wb2", QuattroPro),
        ("wb3", QuattroPro),
        ("qpw", QuattroPro),
        // MS Works 6–9 spreadsheet (#216): a BIFF8 `Workbook` stream in an OLE
        // container — Excel 97's own layout under another extension, so the
        // XLS reader takes it.
        ("xlr", Xls),
        // Kingsoft (WPS Office) Spreadsheets (#216): `.et` and its templates
        // `.ett` / `.eth` are BIFF8 in an OLE container as well — WPS writes
        // Excel 97's `Workbook` stream under its own extension.
        ("et", Xls),
        ("ett", Xls),
        ("eth", Xls),
        // StarOffice 5 binaries (#215): .vor templates dispatch by the CFB
        // stream inside (writer/draw/impress share the container). .sdc
        // routes here too so StarCalc gets its targeted "save as .ods" error
        // instead of an unknown-extension one.
        ("sdw", StarOffice5),
        ("sda", StarOffice5),
        ("sdd", StarOffice5),
        ("sdc", StarOffice5),
        ("vor", StarOffice5),
        // The StarWriter 5 global (master) document (#216): libstaroffice
        // reads it as the `StarWriterDocument` stream it is, its linked
        // sections' text included as stored.
        ("sgl", StarOffice5),
        // T602 (#216): the Czechoslovak DOS word processor's own extension; a
        // file also opens by its `@CT` header (the content sniff).
        ("602", T602),
        // Print-to-file output (#216): report text or a PCL / ESC/P stream.
        ("prn", Prn),
        // Palm e-texts (#216): `.pdb` is a Palm database (the backend checks the
        // type/creator and reads a text `.pdb` — Protein Data Bank — as text); `.pbd`
        // is the spelling #216 lists.
        ("pdb", PalmDoc),
        ("pbd", PalmDoc),
        // Hancom Hangul (#216): HWP 5 (an OLE compound file) and HWPX (a ZIP);
        // the backend tells them apart by content.
        ("hwp", Hwp),
        ("hwpx", Hwp),
        // Computer Graphics Metafile (#216): the drawing's text.
        ("cgm", Cgm),
        // Classic Mac word processors (#216): MacWrite, Word for Mac, ClarisWorks;
        // the backend tells them apart by content.
        ("mw", MacWp),
        ("mcw", MacWp),
        ("cwk", MacWp),
        // Lotus Word Pro (#216).
        ("lwp", Lwp),
        // DjVu (#434): docling.rs extension, pure-Rust decode via `djvu-rs`.
        ("djvu", Djvu),
        ("djv", Djvu),
        // METS/Google Books scan packages ship as `*.tar.gz`.
        ("gz", MetsGbs),
        ("targz", MetsGbs),
    ]
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_and_video_extensions_split_like_upstream() {
        // docling v2.114 FormatToExtensions: AUDIO and VIDEO are disjoint
        // (docling.rs adds the MPEG aliases on top, #208).
        for ext in ["wav", "mp3", "mpga", "m4a", "aac", "ogg", "flac"] {
            assert_eq!(InputFormat::from_extension(ext), Some(InputFormat::Audio));
        }
        for ext in ["mp4", "avi", "mov", "mkv", "webm", "MKV", "mpeg", "mpg"] {
            assert_eq!(InputFormat::from_extension(ext), Some(InputFormat::Video));
        }
        assert_eq!(InputFormat::Video.as_str(), "video");
    }

    #[test]
    fn extension_aliases_route_to_existing_backends() {
        // #208/#210: aliases whose decoding machinery predated the mapping.
        assert_eq!(InputFormat::from_extension("tsv"), Some(InputFormat::Csv));
        assert_eq!(InputFormat::from_extension("gif"), Some(InputFormat::Image));
        assert_eq!(InputFormat::from_extension("xlsb"), Some(InputFormat::Xlsx));
    }

    /// #603: the table is the lookup's single source — each extension once,
    /// stored lowercase, found again case-insensitively.
    #[test]
    fn extension_table_round_trips() {
        let mut seen = std::collections::HashSet::new();
        for &(ext, format) in EXTENSIONS {
            assert!(seen.insert(ext), "duplicate extension {ext}");
            assert_eq!(ext, ext.to_ascii_lowercase(), "{ext} must be lowercase");
            assert!(!ext.starts_with('.'), "{ext} must carry no dot");
            assert_eq!(InputFormat::from_extension(ext), Some(format));
            assert_eq!(
                InputFormat::from_extension(&ext.to_ascii_uppercase()),
                Some(format)
            );
            assert!(format.extensions().any(|e| e == ext));
        }
        assert_eq!(InputFormat::from_extension("unknown"), None);
        assert_eq!(InputFormat::from_extension(""), None);
    }

    /// #603: `--list-input-formats` — sorted, unique, and exactly the
    /// extensions this build converts.
    #[test]
    fn supported_extensions_follow_the_build() {
        let list = InputFormat::supported_extensions();
        let mut sorted = list.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(list, sorted, "sorted and unique");
        for ext in [
            "docx", "rtf", "html", "xlsx", "pptx", "md", "epub", "eml", "msg",
        ] {
            assert!(list.contains(&ext), "{ext} converts in every build");
        }
        for &ext in &list {
            let format = InputFormat::from_extension(ext).expect("listed means routed");
            assert!(format.is_compiled_in(), "{ext} listed but not compiled in");
        }
        assert_eq!(
            list.contains(&"pdf"),
            cfg!(any(feature = "pdf", feature = "pdf-text"))
        );
        assert_eq!(list.contains(&"png"), cfg!(feature = "pdf"));
        assert_eq!(list.contains(&"mp3"), cfg!(feature = "asr"));
        assert_eq!(list.contains(&"mp4"), cfg!(feature = "asr"));
        assert_eq!(list.contains(&"heic"), cfg!(feature = "heif"));
        // Routed for its error message only (StarCalc is not converted).
        assert!(!list.contains(&"sdc"));
    }

    #[test]
    fn output_formats_are_unique() {
        let mut seen = std::collections::HashSet::new();
        assert!(OUTPUT_FORMATS.iter().all(|f| seen.insert(*f)));
        assert!(OUTPUT_FORMATS.contains(&"md") && OUTPUT_FORMATS.contains(&"pandoc"));
    }
}
