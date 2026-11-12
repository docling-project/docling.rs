//! StarOffice 5 binary backend (`.sdw`/`.sgl`/`.sda`/`.sdd`/`.vor`) — a docling.rs
//! extension (#215); docling reaches these only via LibreOffice, whose modern
//! import goes through the reverse-engineered libstaroffice library.
//!
//! The container is CFB (the same [`CompoundFile`] the `.doc`/`.xls`/`.ppt`
//! backends use); the document kind comes from the stream it holds, so a
//! `.vor` template of any application dispatches by content:
//!
//! - **`StarWriterDocument`** (StarWriter 3–5, `.sdw`): a record tree — one
//!   byte of record type plus a 24-bit size, each record opening with a flag
//!   byte whose low nibble is the number of prologue bytes to skip. Text
//!   nodes (`'T'`) carry a Pascal-style string (16-bit length, 8-bit chars);
//!   containers are discovered by validation (a payload that parses as an
//!   exact record sequence is recursed). This is text-level extraction:
//!   paragraphs in document order, tables flattening to their cell texts and
//!   redline-deleted fragments kept (they live inline in the node string).
//!   A global (master) document (`.sgl`, #216) is the same stream.
//! - **`StarDrawDocument3`** (StarDraw / StarImpress 3–5, `.sda`/`.sdd`):
//!   chunked (four ASCII bytes + version + total size). Master pages
//!   (`DrMP`) hold the layout placeholders ("Doubleclick to edit …") and are
//!   skipped whole; each drawing page (`DrPg`) with real objects becomes a
//!   section whose text comes from the embedded outliner blocks (`xV4B`
//!   magic; per paragraph a text string, a style name and an attribute list
//!   whose `0x0f9d` entry is the outline depth). Notes pages — recognized by
//!   their `~LT~Notizen`-styled text objects — are dropped like docling
//!   drops speaker notes elsewhere; object-less pages (the handout) too.
//!
//! Strings decode through Windows-1252 (the format predates Unicode; other
//! source charsets degrade readably). StarCalc (`.sdc`) has a different cell
//! record model and stays a follow-up.

use crate::backend::cfb::CompoundFile;
use crate::backend::rtf::decode_byte;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::{DoclingDocument, Node};

pub struct StarOffice5Backend;

impl DeclarativeBackend for StarOffice5Backend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let cfb = CompoundFile::open(&source.bytes).ok_or_else(|| {
            ConversionError::Parse("staroffice: not an OLE2 compound file".into())
        })?;
        if let Some(sw) = cfb.stream("StarWriterDocument") {
            return convert_writer(&sw, &source.name);
        }
        if let Some(draw) = cfb.stream("StarDrawDocument3") {
            return convert_draw(&draw, &source.name);
        }
        if cfb.stream("StarCalcDocument").is_some() {
            return Err(ConversionError::Parse(
                "staroffice: StarCalc spreadsheets (.sdc) are not supported yet — \
                 open the file in LibreOffice and save as .ods"
                    .into(),
            ));
        }
        Err(ConversionError::Parse(
            "staroffice: no StarWriterDocument or StarDrawDocument3 stream — \
             not a StarOffice 5 writer/draw/impress document"
                .into(),
        ))
    }
}

/// One byte → char via the shared Windows-1252 table.
fn latin(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| decode_byte(b, 1252)).collect()
}

// ---------------------------------------------------------------- StarWriter

/// A record's header: type byte + 24-bit little-endian size (the size counts
/// the 4 header bytes too).
fn sw_record(d: &[u8], off: usize) -> Option<(u8, usize)> {
    if off + 4 > d.len() {
        return None;
    }
    let t = d[off];
    let size = d[off + 1] as usize | (d[off + 2] as usize) << 8 | (d[off + 3] as usize) << 16;
    // Record types are printable ASCII ('!', '0', 'N', 'T', …); anything else
    // marks a misparse.
    if size < 4 || off + size > d.len() || !(0x21..=0x7e).contains(&t) {
        return None;
    }
    Some((t, size))
}

/// Walk a record sequence spanning exactly `[off, end)`, collecting `'T'` text
/// nodes; returns `false` when the bytes do not chain as records (the caller's
/// signal that this payload is a leaf, not a container). Containers are
/// discovered by that same validation, recursively.
fn sw_walk(d: &[u8], mut off: usize, end: usize, depth: usize, out: &mut Vec<String>) -> bool {
    if depth > 24 {
        return false;
    }
    let mut any = false;
    while off < end {
        let Some((t, size)) = sw_record(d, off) else {
            return false;
        };
        if off + size > end {
            return false;
        }
        let payload = &d[off + 4..off + size];
        if t == b'T' {
            if let Some(text) = sw_text(payload) {
                out.push(text);
            }
        } else if !payload.is_empty() {
            // The flag byte's low nibble is the prologue length; a payload
            // that then parses as an exact record sequence is a container.
            let skip = 1 + (payload[0] & 0x0f) as usize;
            if skip <= payload.len() {
                let mut sub = Vec::new();
                if sw_walk(d, off + 4 + skip, off + size, depth + 1, &mut sub) {
                    out.extend(sub);
                }
            }
        }
        any = true;
        off += size;
    }
    any && off == end
}

/// A `'T'` text node's string: flag byte, `flags & 0x0f` prologue bytes, then
/// a 16-bit length and that many 8-bit characters.
fn sw_text(payload: &[u8]) -> Option<String> {
    let flags = *payload.first()?;
    let p = 1 + (flags & 0x0f) as usize;
    let len = u16::from_le_bytes([*payload.get(p)?, *payload.get(p + 1)?]) as usize;
    let bytes = payload.get(p + 2..p + 2 + len)?;
    let text = latin(bytes);
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn convert_writer(d: &[u8], name: &str) -> Result<DoclingDocument, ConversionError> {
    if d.len() < 8 || &d[..2] != b"SW" || &d[3..6] != b"HDR" {
        return Err(ConversionError::Parse(
            "staroffice: StarWriterDocument stream without an SW*HDR header".into(),
        ));
    }
    // The fixed header's length varies by version; the record area is the
    // first offset from which the whole stream chains as records.
    let mut texts = Vec::new();
    for start in 8..d.len().min(0x100) {
        let mut trial = Vec::new();
        if sw_walk(d, start, d.len(), 0, &mut trial) && !trial.is_empty() {
            texts = trial;
            break;
        }
    }
    if texts.is_empty() {
        return Err(ConversionError::Parse(
            "staroffice: no text records found in the StarWriter document".into(),
        ));
    }
    let mut doc = DoclingDocument::new(name);
    for text in texts {
        doc.push(Node::Paragraph { text });
    }
    Ok(doc)
}

// ---------------------------------------------------------- StarDraw/Impress

/// One outliner paragraph: its text, style name and outline depth.
struct DrawPara {
    text: String,
    style: String,
    depth: u16,
}

/// Parse an outliner block at `off` (the `xV4B` magic): version char, one
/// byte, 32-bit body size, then a sync word, three bytes, a paragraph count
/// and per paragraph text + style strings, an `0xaffe` marker and an
/// attribute list (`which` 0x0f9d carries the outline depth in its value's
/// high word).
fn draw_outliner(d: &[u8], off: usize) -> Option<Vec<DrawPara>> {
    let size = u32::from_le_bytes(d.get(off + 6..off + 10)?.try_into().ok()?) as usize;
    let body = d.get(off + 10..off + 10 + size)?;
    let u16_at = |p: usize| -> Option<u16> {
        Some(u16::from_le_bytes(body.get(p..p + 2)?.try_into().ok()?))
    };
    let mut p = 2 + 3; // sync word + three bytes
    let count = u16_at(p)?;
    p += 2;
    let mut paras = Vec::new();
    for _ in 0..count {
        let len = u16_at(p)? as usize;
        p += 2;
        let text = latin(body.get(p..p + len)?);
        p += len;
        let slen = u16_at(p)? as usize;
        p += 2;
        let style = latin(body.get(p..p + slen)?);
        p += slen;
        if body.get(p..p + 2) != Some(&[0xfe, 0xaf]) {
            return None;
        }
        p += 2;
        let nattr = u16_at(p)? as usize;
        p += 2;
        let mut depth = 0u16;
        for _ in 0..nattr {
            let which = u16_at(p)?;
            let value = u32::from_le_bytes(body.get(p + 2..p + 6)?.try_into().ok()?);
            if which == 0x0f9d {
                depth = (value >> 16) as u16;
            }
            p += 6;
        }
        p += 2;
        paras.push(DrawPara { text, style, depth });
    }
    Some(paras)
}

/// The `[start, end)` spans of every `DrPg` (page) chunk: four ASCII tag
/// bytes, a 16-bit version and a 32-bit size that counts the whole chunk.
/// `DrMP` master-page chunks are recognized the same way but only skipped —
/// their placeholder texts must never leak into a page.
fn draw_spans(d: &[u8]) -> Vec<(usize, usize)> {
    let mut pages = Vec::new();
    let mut off = 0usize;
    while off + 10 <= d.len() {
        let tag = &d[off..off + 4];
        if tag == b"DrPg" || tag == b"DrMP" {
            let ver = u16::from_le_bytes([d[off + 4], d[off + 5]]);
            let size = u32::from_le_bytes(d[off + 6..off + 10].try_into().unwrap()) as usize;
            if ver < 0x100 && size >= 10 && off + size <= d.len() {
                if tag == b"DrPg" {
                    pages.push((off, off + size));
                }
                // Page chunks never nest in each other; skip the whole span.
                off += size;
                continue;
            }
        }
        off += 1;
    }
    pages
}

fn convert_draw(d: &[u8], name: &str) -> Result<DoclingDocument, ConversionError> {
    let pages = draw_spans(d);
    if pages.is_empty() {
        return Err(ConversionError::Parse(
            "staroffice: no DrPg page chunks in the StarDraw document".into(),
        ));
    }

    let mut doc = DoclingDocument::new(name);
    let mut emitted = 0usize;
    for &(start, end) in &pages {
        let span = &d[start..end];
        // The handout page carries no drawing objects at all.
        if !contains(span, b"DrOb") {
            continue;
        }
        // Collect this page's outliner texts.
        let mut paras: Vec<DrawPara> = Vec::new();
        let mut off = 0usize;
        while off + 10 <= span.len() {
            if &span[off..off + 4] == b"xV4B" {
                if let Some(mut block) = draw_outliner(span, off) {
                    paras.append(&mut block);
                }
            }
            off += 1;
        }
        // A notes page announces itself through its placeholder style — and
        // docling drops speaker notes across formats, so the page goes whole.
        if paras.iter().any(|p| p.style.contains("~LT~Notizen")) {
            continue;
        }
        emitted += 1;
        doc.push(Node::Heading {
            level: 1,
            text: format!("page-{emitted}"),
        });
        let mut first = true;
        for para in &paras {
            let text = para.text.trim();
            if text.is_empty() {
                continue;
            }
            if para.depth > 0 {
                doc.push(Node::ListItem {
                    ordered: false,
                    number: 0,
                    first_in_list: std::mem::take(&mut first),
                    text: text.to_string(),
                    level: para.depth as u8 - 1,
                    marker: None,
                    location: None,
                    split: None,
                    dclx: None,
                    href: None,
                    layer: None,
                });
            } else {
                first = true;
                doc.push(Node::Paragraph {
                    text: text.to_string(),
                });
            }
        }
    }
    if emitted == 0 {
        return Err(ConversionError::Parse(
            "staroffice: no content pages in the StarDraw document".into(),
        ));
    }
    Ok(doc)
}

/// Naive subsequence search (the spans are small; no need for memmem).
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assemble an SW record: type + 24-bit size + payload.
    fn rec(t: u8, payload: &[u8]) -> Vec<u8> {
        let size = payload.len() + 4;
        let mut out = vec![t, size as u8, (size >> 8) as u8, (size >> 16) as u8];
        out.extend_from_slice(payload);
        out
    }

    /// A text node payload: flag byte (low nibble = prologue length),
    /// prologue, u16 length, bytes.
    fn text_payload(flags: u8, text: &[u8]) -> Vec<u8> {
        let mut p = vec![flags];
        p.extend(std::iter::repeat_n(0u8, (flags & 0x0f) as usize));
        p.extend_from_slice(&(text.len() as u16).to_le_bytes());
        p.extend_from_slice(text);
        p
    }

    #[test]
    fn sw_walk_extracts_text_through_containers() {
        // N{ T"Erster" T"" T"Zwei\x94ter" } — the flag byte's low nibble
        // varies, the empty node drops out and 0x94 decodes as cp1252 ”.
        let mut body = Vec::new();
        body.extend(rec(b'T', &text_payload(0x02, b"Erster")));
        body.extend(rec(b'T', &text_payload(0x13, b"")));
        body.extend(rec(b'T', &text_payload(0x02, b"Zwei\x94ter")));
        let mut container_payload = vec![0x04u8, 0, 0, 0, 0]; // flags 4 + prologue
        container_payload.extend(&body);
        let stream = rec(b'N', &container_payload);
        let mut out = Vec::new();
        assert!(sw_walk(&stream, 0, stream.len(), 0, &mut out));
        assert_eq!(
            out,
            vec!["Erster".to_string(), "Zwei\u{201d}ter".to_string()]
        );
    }

    #[test]
    fn writer_needs_the_sw_header() {
        let err = convert_writer(b"NOTSWFILE___", "x").unwrap_err();
        assert!(err.to_string().contains("SW*HDR"), "{err}");
    }

    /// A `DrPg` with an object and an outliner block converts to a page
    /// section; the master (`DrMP`) placeholder and the notes page
    /// (`~LT~Notizen` style) are skipped.
    #[test]
    fn draw_pages_keep_content_and_skip_masters_and_notes() {
        fn outliner(paras: &[(&str, &str, u16)]) -> Vec<u8> {
            let mut body = vec![0x2d, 0x01, 0, 0, 0]; // sync + three bytes
            body.extend((paras.len() as u16).to_le_bytes());
            for (text, style, depth) in paras {
                body.extend((text.len() as u16).to_le_bytes());
                body.extend_from_slice(text.as_bytes());
                body.extend((style.len() as u16).to_le_bytes());
                body.extend_from_slice(style.as_bytes());
                body.extend([0xfe, 0xaf]);
                body.extend(1u16.to_le_bytes()); // one attribute
                body.extend(0x0f9du16.to_le_bytes());
                body.extend(((*depth as u32) << 16).to_le_bytes());
                body.extend([0, 0]);
            }
            let mut out = b"xV4B1\0".to_vec();
            out.extend((body.len() as u32).to_le_bytes());
            out.extend(body);
            out
        }
        fn chunk(tag: &[u8; 4], content: &[u8]) -> Vec<u8> {
            let mut out = tag.to_vec();
            out.extend(0x0cu16.to_le_bytes());
            out.extend(((content.len() + 10) as u32).to_le_bytes());
            out.extend_from_slice(content);
            out
        }
        let mut stream = Vec::new();
        stream.extend(chunk(
            b"DrMP",
            &[
                b"DrOb".to_vec(),
                outliner(&[("Doubleclick", "std~LT~Titel", 0)]),
            ]
            .concat(),
        ));
        stream.extend(chunk(
            b"DrPg",
            &[
                b"DrOb".to_vec(),
                outliner(&[("Titel der Seite", "std~LT~Titel", 0), ("Punkt", "std", 1)]),
            ]
            .concat(),
        ));
        stream.extend(chunk(b"DrPg", b"DrOb".as_ref())); // shapes, no text
        stream.extend(chunk(
            b"DrPg",
            &[
                b"DrOb".to_vec(),
                outliner(&[("Notiz", "std~LT~Notizen", 0)]),
            ]
            .concat(),
        ));
        let doc = convert_draw(&stream, "d").unwrap();
        let md = doc.export_to_markdown();
        assert!(md.contains("# page-1"), "{md}");
        assert!(md.contains("Titel der Seite"), "{md}");
        assert!(md.contains("- Punkt"), "{md}");
        assert!(md.contains("# page-2"), "second page kept:\n{md}");
        assert!(!md.contains("Doubleclick"), "master skipped:\n{md}");
        assert!(!md.contains("Notiz"), "notes page skipped:\n{md}");
        assert!(!md.contains("page-3"), "notes page not counted:\n{md}");
    }
}
