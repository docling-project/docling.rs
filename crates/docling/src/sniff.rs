//! Content-based format detection for the retry after a failed conversion
//! (#556).
//!
//! The format comes from the file extension, and a document converts through
//! that format's backend with no look at its content first — the common case
//! pays nothing. Only when that conversion fails does [`detect`] read the
//! bytes, and when they are evidently another format the converter retries
//! once with it. Real-world archives carry plenty of mislabelled files: in one
//! corpus of 1,573 legacy `.doc` files, 185 were something else — RTF, Word
//! templates and OOXML packages saved as `.doc`, HTML exported "as Word" —
//! which used to fail with "not a compound file".
//!
//! The markers are the formats' own signatures, nothing heuristic: the RTF
//! group opener, the PDF header, a ZIP package's part names (OOXML, ODF and
//! EPUB `mimetype`), an OLE compound file's stream names, HTML markup. A
//! file none of them recognise is left with its original error.
//!
//! [`detect_any`] — [`InputFormat::detect`] — is the wider set a nameless
//! input needs (#672: stdin, an upload, a `data:` URL, an email attachment,
//! a file with no or an unknown extension): the above plus the raster image
//! headers, DjVu, the audio and video containers by their magic (RIFF WAVE /
//! AVI, ID3, FLAC, Ogg, ADTS and MPEG audio frames, ISO BMFF by brand,
//! EBML), WordPerfect's prefix, Apple iWork packages (the IWA bundle, the
//! iWork '09 XML package), and — for text — WebVTT, DocTags, SVG, AbiWord,
//! XML (a feed, JATS, USPTO, XBRL: the `xml` route reads the dialect), JSON
//! (a Jupyter notebook, else the docling-JSON route that renders generic
//! JSON too), a MIME message (`multipart/related` is MHTML, the rest email)
//! and LaTeX. Still no classifier: plain prose, Markdown, CSV and AsciiDoc
//! are not told apart from each other, so they are left to the extension
//! (or the plain-text route, #671).

use std::io::{Cursor, Read};

use crate::format::InputFormat;

/// The format `bytes` evidently are, if any of the signatures matches.
pub(crate) fn detect(bytes: &[u8]) -> Option<InputFormat> {
    if looks_like_pdf(bytes) {
        return Some(InputFormat::Pdf);
    }
    let text = text_head(bytes);
    if text.starts_with(b"{\\rtf") {
        return Some(InputFormat::Rtf);
    }
    // A flat Word for Windows 1.x / 2.0 file (#566): no container to look
    // into, its FIB is the first bytes.
    if crate::backend::doc::is_word2(bytes) {
        return Some(InputFormat::Doc);
    }
    if bytes.starts_with(b"PK\x03\x04") {
        return zip_kind(bytes);
    }
    if crate::backend::cfb::CompoundFile::detect(bytes) {
        return cfb_kind(bytes);
    }
    if looks_like_html(text) {
        return Some(InputFormat::Html);
    }
    None
}

/// [`detect`] plus the formats a failed conversion never needs re-routing
/// to but a nameless input (stdin, a `data:` URL, an extensionless URL
/// with a generic `Content-Type`, a file with no or an unknown extension)
/// does — see the module docs. The public [`InputFormat::detect`].
pub(crate) fn detect_any(bytes: &[u8]) -> Option<InputFormat> {
    if let Some(f) = detect(bytes) {
        return Some(f);
    }
    if looks_like_image(bytes) {
        return Some(InputFormat::Image);
    }
    if crate::backend::looks_like_djvu(bytes) {
        return Some(InputFormat::Djvu);
    }
    if let Some(f) = media_kind(bytes) {
        return Some(f);
    }
    // Every WordPerfect 5.0+ file starts with `ÿWPC`.
    if bytes.starts_with(b"\xffWPC") {
        return Some(InputFormat::WordPerfect);
    }
    // A Palm e-text: the database's type and creator at offset 60 (#216).
    if crate::backend::looks_like_palmdoc(bytes) {
        return Some(InputFormat::PalmDoc);
    }
    // Word for Macintosh's magic, ClarisWorks' `BOBO` (#216); MacWrite has
    // no signature beyond a version word and comes by its extension.
    if crate::backend::macwp::looks_like_mac_word(bytes)
        || crate::backend::macwp::looks_like_clarisworks(bytes)
    {
        return Some(InputFormat::MacWp);
    }
    // Lotus Word Pro's `WordPro` prefix and `LWP7` header tag (#216).
    if crate::backend::lwp::looks_like_lwp(bytes) {
        return Some(InputFormat::Lwp);
    }
    // A Computer Graphics Metafile's BEGIN METAFILE element (#216).
    if crate::backend::cgm::looks_like_binary_cgm(bytes)
        || crate::backend::cgm::looks_like_clear_text_cgm(bytes)
    {
        return Some(InputFormat::Cgm);
    }
    // A saved T602 document opens with its code-page setting (#216).
    if crate::backend::looks_like_t602(bytes) {
        return Some(InputFormat::T602);
    }
    text_kind(bytes)
}

/// Audio and video containers by their magic. The ISO BMFF brands tell
/// `M4A`/`M4B`/`M4P` (audio) from the rest (MP4, MOV, 3GP — video);
/// Matroska/WebM count as video (the video route copes with an audio-only
/// track). No decoder is opened: the magic suffices.
fn media_kind(bytes: &[u8]) -> Option<InputFormat> {
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") {
        return match &bytes[8..12] {
            b"WAVE" => Some(InputFormat::Audio),
            b"AVI " => Some(InputFormat::Video),
            _ => None,
        };
    }
    if bytes.starts_with(b"ID3") || bytes.starts_with(b"fLaC") || bytes.starts_with(b"OggS") {
        return Some(InputFormat::Audio);
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return Some(match &bytes[8..12] {
            // HEIF / AVIF stills share the container (#211).
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1" | b"avif" | b"avis" => {
                InputFormat::Image
            }
            brand
                if brand.starts_with(b"M4A")
                    || brand.starts_with(b"M4B")
                    || brand.starts_with(b"M4P") =>
            {
                InputFormat::Audio
            }
            _ => InputFormat::Video,
        });
    }
    if bytes.starts_with(b"\x1A\x45\xDF\xA3") {
        return Some(InputFormat::Video);
    }
    // An MPEG audio frame (an MP3 without an ID3 tag) or an ADTS AAC frame:
    // the 11-bit sync word, a version and a layer that are not the reserved
    // ones (ADTS is "MPEG-4, layer 0" in the same bits).
    if bytes.len() >= 4
        && bytes[0] == 0xFF
        && (bytes[1] & 0xE0) == 0xE0
        && (bytes[1] & 0x18) != 0x08
    {
        let layer = bytes[1] & 0x06;
        let adts = layer == 0 && (bytes[1] & 0x10) == 0x10;
        if adts || layer != 0 {
            return Some(InputFormat::Audio);
        }
    }
    None
}

/// The text formats a signature at the head tells apart — see the module
/// docs. `None` for prose, Markdown, CSV and the like.
fn text_kind(bytes: &[u8]) -> Option<InputFormat> {
    let head = text_head(bytes);
    if head.is_empty() {
        return None;
    }
    if head.starts_with(b"WEBVTT") {
        return Some(InputFormat::Vtt);
    }
    if head[0] == b'<' {
        let lower = head.to_ascii_lowercase();
        let has = |needle: &[u8]| lower.windows(needle.len()).any(|w| w == needle);
        if has(b"<doctag") || has(b"<loc_") || has(b"<otsl>") {
            return Some(InputFormat::DocTags);
        }
        if has(b"<svg") {
            return Some(InputFormat::Svg);
        }
        if has(b"<abiword") {
            return Some(InputFormat::Abiword);
        }
        if lower.starts_with(b"<?xml")
            || lower.starts_with(b"<!doctype")
            || looks_like_xml_root(&lower, bytes)
        {
            return Some(InputFormat::Xml);
        }
        return None;
    }
    if head[0] == b'{' || head[0] == b'[' {
        return json_kind(bytes);
    }
    if head[0] == b'\\' {
        let has = |needle: &[u8]| head.windows(needle.len()).any(|w| w == needle);
        if has(b"\\documentclass") || has(b"\\begin{document}") {
            return Some(InputFormat::Latex);
        }
        return None;
    }
    message_kind(head)
}

/// A bare XML document without a declaration: an element start tag at the
/// head (`<rss `, `<feed xmlns=`, `<article>`) whose end tag closes the
/// file — so `<b>bold</b> words` (inline markup in prose) is not one.
/// (Markup that is HTML was taken earlier.)
fn looks_like_xml_root(lower: &[u8], bytes: &[u8]) -> bool {
    let name_end = lower[1..]
        .iter()
        .position(|&c| !(c.is_ascii_alphanumeric() || matches!(c, b'_' | b':' | b'-' | b'.')))
        .map(|i| i + 1)
        .unwrap_or(lower.len());
    let name = &lower[1..name_end];
    let starts_like_a_tag = name
        .first()
        .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_');
    let delimited = matches!(
        lower.get(name_end),
        Some(b' ' | b'>' | b'/' | b'\n' | b'\r' | b'\t')
    );
    if !(starts_like_a_tag && delimited) {
        return false;
    }
    let tail = &bytes[bytes.len().saturating_sub(512)..];
    let tail = tail.to_ascii_lowercase();
    let end = tail
        .iter()
        .rposition(|c| !c.is_ascii_whitespace())
        .map_or(0, |i| i + 1);
    let mut close = Vec::with_capacity(name.len() + 3);
    close.extend_from_slice(b"</");
    close.extend_from_slice(name);
    close.push(b'>');
    tail[..end].ends_with(&close)
}

/// Well-formed JSON: a Jupyter notebook by its keys, a Box Note by its `doc`
/// node (#1033), else the docling-JSON route, which renders a
/// `DoclingDocument` and keeps any other JSON as a code block (#671). The whole input is validated, nothing is kept.
fn json_kind(bytes: &[u8]) -> Option<InputFormat> {
    let text = std::str::from_utf8(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes)).ok()?;
    serde_json::from_str::<serde::de::IgnoredAny>(text).ok()?;
    Some(if crate::backend::ipynb::looks_like_notebook(text) {
        InputFormat::Ipynb
    } else if crate::backend::boxnote::looks_like_boxnote(text) {
        // A Box Note (#1033): a top-level `doc` node of type `doc`.
        InputFormat::Boxnote
    } else {
        InputFormat::JsonDocling
    })
}

/// A MIME message by its header block: `Name: value` lines from the first
/// line on, at least two of them the message headers everyone writes. A
/// `multipart/related` body is a web archive (MHTML); the rest is email.
fn message_kind(head: &[u8]) -> Option<InputFormat> {
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    fn is_header(line: &str) -> Option<&str> {
        let (name, rest) = line.split_once(':')?;
        (!name.is_empty()
            && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            && (rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')))
        .then_some(name)
    }
    let mut lines = text.lines();
    is_header(lines.next()?)?;
    const KNOWN: &[&str] = &[
        "from",
        "to",
        "cc",
        "subject",
        "date",
        "received",
        "return-path",
        "message-id",
        "mime-version",
        "content-type",
        "delivered-to",
        "sender",
        "reply-to",
        "snapshot-content-location",
    ];
    let mut known = 0;
    let mut related = false;
    for line in text.lines().take_while(|l| !l.trim().is_empty()) {
        if line.contains("multipart/related") {
            related = true;
        }
        if let Some(name) = is_header(line) {
            if KNOWN.contains(&name) || name.starts_with("x-") {
                known += 1;
            }
        }
    }
    if known < 2 {
        return None;
    }
    Some(if related {
        InputFormat::Mhtml
    } else {
        InputFormat::Email
    })
}

/// Whether `bytes` are text (#671): the fallback route for an unknown or
/// missing extension. Not empty, no NUL byte, and either valid UTF-8 (a
/// BOM allowed) with control characters other than tab, newline, carriage
/// return and form feed under one per cent, or — a single-byte legacy
/// encoding — the same test on the bytes themselves with the high half
/// counted as letters. Random bytes, archives and binaries fail on NULs and
/// control characters; a `.log` in windows-1251 passes.
pub(crate) fn looks_like_text(bytes: &[u8]) -> bool {
    let b = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    if b.is_empty() || b.contains(&0) {
        return false;
    }
    let is_control = |c: u32| c < 0x20 && !matches!(c, 0x09 | 0x0A | 0x0D | 0x0C) || c == 0x7F;
    let (total, control) = match std::str::from_utf8(b) {
        Ok(s) => (
            s.chars().count(),
            s.chars().filter(|&c| is_control(c as u32)).count(),
        ),
        Err(_) => (b.len(), b.iter().filter(|&&c| is_control(c as u32)).count()),
    };
    control * 100 < total
}

/// The magic of the raster formats the image pipeline decodes: PNG, JPEG,
/// GIF, BMP, TIFF (both byte orders), WebP.
fn looks_like_image(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xFF\xD8\xFF")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || bytes.starts_with(b"BM")
        || bytes.starts_with(b"II*\0")
        || bytes.starts_with(b"MM\0*")
        || (bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP")
}

/// A PDF header within the first KiB — where readers (pdfium, docling-parse)
/// accept it after leading junk.
pub(crate) fn looks_like_pdf(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(1024)]
        .windows(5)
        .any(|w| w == b"%PDF-")
}

/// The first KiB past a UTF-8 BOM and leading whitespace.
fn text_head(bytes: &[u8]) -> &[u8] {
    let b = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let start = b
        .iter()
        .position(|c| !c.is_ascii_whitespace())
        .unwrap_or(b.len());
    &b[start..b.len().min(start + 1024)]
}

/// Markup that opens an HTML document: a doctype, `<html>`, or a fragment
/// whose head names one (Word's "Save as Web Page" starts with comments and
/// conditional markup before `<html>`).
fn looks_like_html(head: &[u8]) -> bool {
    if head.first() != Some(&b'<') {
        return false;
    }
    let lower = head.to_ascii_lowercase();
    let has = |needle: &[u8]| lower.windows(needle.len()).any(|w| w == needle);
    has(b"<!doctype html")
        || has(b"<html")
        || lower.starts_with(b"<head")
        || lower.starts_with(b"<body")
}

/// A ZIP package by its parts: OOXML main parts, the ODF / EPUB `mimetype`.
fn zip_kind(bytes: &[u8]) -> Option<InputFormat> {
    let mut zip = crate::backend::ooxml::open_zip(|| Cursor::new(bytes)).ok()?;
    let has = |zip: &zip::ZipArchive<_>, name: &str| zip.file_names().flatten().any(|n| n == name);
    if has(&zip, "word/document.xml") {
        return Some(InputFormat::Docx);
    }
    if has(&zip, "xl/workbook.xml") || has(&zip, "xl/workbook.bin") {
        return Some(InputFormat::Xlsx);
    }
    if has(&zip, "ppt/presentation.xml") {
        return Some(InputFormat::Pptx);
    }
    if has(&zip, "visio/document.xml") {
        return Some(InputFormat::Visio);
    }
    if let Some(f) = iwork_kind(&mut zip) {
        return Some(f);
    }
    let mut mimetype = String::new();
    zip.by_name("mimetype")
        .ok()?
        .take(256)
        .read_to_string(&mut mimetype)
        .ok()?;
    let mime = mimetype.trim();
    match mime {
        "application/epub+zip" => Some(InputFormat::Epub),
        _ if crate::backend::hwp::is_hwpx_mime(mime) => Some(InputFormat::Hwp),
        _ if mime.starts_with("application/vnd.oasis.opendocument.text") => Some(InputFormat::Odt),
        _ if mime.starts_with("application/vnd.oasis.opendocument.spreadsheet") => {
            Some(InputFormat::Ods)
        }
        _ if mime.starts_with("application/vnd.oasis.opendocument.presentation") => {
            Some(InputFormat::Odp)
        }
        _ => None,
    }
}

/// An Apple iWork package: the IWA bundle (`Index/Document.iwa`, at the root
/// or under a `<name>.pages/` directory, or zipped a second time into an
/// `Index.zip`) — Keynote by its slide archives, Pages and Numbers by the
/// root archive's message type (`TP.DocumentArchive` = 10000,
/// `TN.DocumentArchive` = 1) — or the iWork '09 XML package (`index.xml` /
/// `index.apxl`, by its root element).
fn iwork_kind(zip: &mut zip::ZipArchive<Cursor<&[u8]>>) -> Option<InputFormat> {
    let names: Vec<String> = zip.file_names().flatten().map(|n| n.into_owned()).collect();
    let is_slide = |n: &str| {
        n.rsplit_once("Index/").is_some_and(|(_, m)| {
            m.starts_with("Slide-") || m.starts_with("Slide.") || m.starts_with("MasterSlide")
        })
    };
    let read_capped = |zip: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str, cap: u64| {
        let mut buf = Vec::new();
        zip.by_name(name)
            .ok()?
            .take(cap)
            .read_to_end(&mut buf)
            .ok()?;
        Some(buf)
    };
    if let Some(index) = names
        .iter()
        .find(|n| n.ends_with("index.xml") || n.ends_with("index.apxl"))
    {
        let head = read_capped(zip, index, 2048)?;
        let lower = head.to_ascii_lowercase();
        let has = |needle: &[u8]| lower.windows(needle.len()).any(|w| w == needle);
        return if has(b"<sl:document") {
            Some(InputFormat::Pages)
        } else if has(b"<ls:document") {
            Some(InputFormat::Numbers)
        } else if has(b"<key:presentation") {
            Some(InputFormat::Keynote)
        } else {
            None
        };
    }
    // The bundle's index: the members themselves, or the ones inside Index.zip.
    let (members, document): (Vec<String>, Option<Vec<u8>>) =
        match names.iter().find(|n| n.ends_with("Index/Document.iwa")) {
            Some(doc) => (names.clone(), read_capped(zip, doc, 8 << 20)),
            None => {
                let inner = names
                    .iter()
                    .find(|n| *n == "Index.zip" || n.ends_with("/Index.zip"))?;
                let bytes = read_capped(zip, inner, 64 << 20)?;
                let mut inner =
                    crate::backend::ooxml::open_zip(|| Cursor::new(bytes.as_slice())).ok()?;
                let members: Vec<String> = inner
                    .file_names()
                    .flatten()
                    .map(|n| n.into_owned())
                    .collect();
                let doc = members
                    .iter()
                    .find(|n| n.ends_with("Index/Document.iwa"))?
                    .clone();
                let document = read_capped(&mut inner, &doc, 8 << 20);
                (members, document)
            }
        };
    if members.iter().any(|n| is_slide(n)) {
        return Some(InputFormat::Keynote);
    }
    let stream = crate::backend::iwork::decode_iwa(&document?).ok()?;
    let mut archives = Vec::new();
    crate::backend::iwork::parse_archives(&stream, &mut archives, false);
    match archives.first()?.ty {
        10000 => Some(InputFormat::Pages),
        1 => Some(InputFormat::Numbers),
        _ => None,
    }
}

/// An OLE compound file by its root streams: Word, Excel, PowerPoint,
/// Outlook. Root only — an e-mail carrying a Word attachment holds a
/// `WordDocument` stream too, one storage down.
fn cfb_kind(bytes: &[u8]) -> Option<InputFormat> {
    let cfb = crate::backend::cfb::CompoundFile::open(bytes)?;
    let root: Vec<&str> = cfb
        .children_of(None)
        .into_iter()
        .filter(|&i| !cfb.is_storage(i))
        .map(|i| cfb.entry_name(i))
        .collect();
    let has = |name: &str| root.contains(&name);
    if has("WordDocument") {
        Some(InputFormat::Doc)
    } else if has("Workbook") || has("Book") {
        Some(InputFormat::Xls)
    } else if has("PowerPoint Document") {
        Some(InputFormat::Ppt)
    } else if has("__properties_version1.0") {
        Some(InputFormat::Email)
    } else if has("FileHeader") && has("DocInfo") {
        // Hancom Hangul 5 (#216).
        Some(InputFormat::Hwp)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        let mut w = zip::ZipWriter::new(&mut out);
        for (name, data) in entries {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
        out.into_inner()
    }

    #[test]
    fn signatures() {
        assert_eq!(
            detect(b"\xEF\xBB\xBF \r\n{\\rtf1\\ansi x}"),
            Some(InputFormat::Rtf)
        );
        assert_eq!(detect(b"junk%PDF-1.7\n"), Some(InputFormat::Pdf));
        assert_eq!(
            detect(b"<!-- saved --><!DOCTYPE html><html><body>x</body></html>"),
            Some(InputFormat::Html)
        );
        assert_eq!(
            detect(b"<html xmlns:o=\"urn:schemas-microsoft-com:office:office\">"),
            Some(InputFormat::Html)
        );
        assert_eq!(detect(b"plain text"), None);
        assert_eq!(detect(b"<?xml version=\"1.0\"?><article/>"), None);
        assert_eq!(detect(b""), None);
    }

    #[test]
    fn zip_packages_by_their_parts() {
        let docx = zip_with(&[
            ("[Content_Types].xml", b"<Types/>"),
            ("word/document.xml", b"<w/>"),
        ]);
        assert_eq!(detect(&docx), Some(InputFormat::Docx));
        let xlsx = zip_with(&[("xl/workbook.xml", b"<w/>")]);
        assert_eq!(detect(&xlsx), Some(InputFormat::Xlsx));
        let pptx = zip_with(&[("ppt/presentation.xml", b"<p/>")]);
        assert_eq!(detect(&pptx), Some(InputFormat::Pptx));
        let odt = zip_with(&[("mimetype", b"application/vnd.oasis.opendocument.text")]);
        assert_eq!(detect(&odt), Some(InputFormat::Odt));
        let ods = zip_with(&[(
            "mimetype",
            b"application/vnd.oasis.opendocument.spreadsheet",
        )]);
        assert_eq!(detect(&ods), Some(InputFormat::Ods));
        let epub = zip_with(&[("mimetype", b"application/epub+zip")]);
        assert_eq!(detect(&epub), Some(InputFormat::Epub));
        let other = zip_with(&[("readme.txt", b"hi")]);
        assert_eq!(detect(&other), None);
    }

    /// A nameless input (stdin, a `data:` URL, an extensionless URL or
    /// file) is typed by its signature alone — text without markup stays
    /// unknown rather than guessed.
    #[test]
    fn detect_any_reads_signatures() {
        let docx = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/data/docx/sources/lorem_ipsum.docx"),
        )
        .unwrap();
        assert_eq!(detect_any(&docx), Some(InputFormat::Docx));
        assert_eq!(detect_any(b"%PDF-1.7\n%..."), Some(InputFormat::Pdf));
        for image in [
            &b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"[..],
            b"\xFF\xD8\xFF\xE0\0\x10JFIF",
            b"RIFF\0\0\0\0WEBPVP8 ",
        ] {
            assert_eq!(detect_any(image), Some(InputFormat::Image));
        }
        assert_eq!(
            detect_any(b"<!DOCTYPE html><html><body>x</body></html>"),
            Some(InputFormat::Html)
        );
        assert_eq!(detect_any(b"{\\rtf1\\ansi x}"), Some(InputFormat::Rtf));
        assert_eq!(detect_any(b"just some words"), None);
        assert_eq!(detect_any(b""), None);
        // A WAV file is a RIFF container too — audio, not an image.
        assert_eq!(
            detect_any(b"RIFF\0\0\0\0WAVEfmt "),
            Some(InputFormat::Audio)
        );
    }
}
