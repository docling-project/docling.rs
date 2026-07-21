//! PPT (PowerPoint 97–2003 binary, [MS-PPT]) backend — issue #127.
//!
//! Native parsing, no external converter (docling proper shells out to
//! LibreOffice — `docling` PR #3804). The format is a CFB container whose
//! `PowerPoint Document` stream is a tree of tagged records (8-byte header:
//! version/instance, type, length; version `0xF` marks a container).
//!
//! Slide text lives in two places, depending on how the file was saved:
//! - the `SlideListWithText` container inside the `DocumentContainer` —
//!   `SlidePersistAtom` opens each slide's group, followed by
//!   `TextHeaderAtom` (which says whether the run is a title or body) and
//!   `TextCharsAtom` (UTF-16LE) / `TextBytesAtom` (CP1252) runs;
//! - each `Slide` container's drawing (`OfficeArtClientTextbox`), same atoms.
//!
//! Both are walked: slides come from the SlideListWithText when it carries
//! text, and any slide it left empty falls back to the corresponding `Slide`
//! container walk. Titles become headings, other runs paragraphs (one per
//! line — lines in an atom are `\r`-separated); slides are separated by page
//! breaks, matching the PPTX backend's shape.

use docling_core::{DoclingDocument, Node};

use crate::backend::cfb::CompoundFile;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

const RT_DOCUMENT: u16 = 0x03E8; // DocumentContainer
const RT_SLIDE: u16 = 0x03EE; // SlideContainer
const RT_SLIDE_LIST_WITH_TEXT: u16 = 0x0FF0;
const RT_SLIDE_PERSIST_ATOM: u16 = 0x03F3;
const RT_TEXT_HEADER_ATOM: u16 = 0x0F9F;
const RT_TEXT_CHARS_ATOM: u16 = 0x0FA0;
const RT_TEXT_BYTES_ATOM: u16 = 0x0FA8;

/// Text-run types (TextHeaderAtom): 0 = title, 6 = centered title.
const TX_TITLE: u32 = 0;
const TX_CENTER_TITLE: u32 = 6;

pub struct PptBackend;

impl DeclarativeBackend for PptBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let cfb = CompoundFile::open(&source.bytes)
            .ok_or_else(|| ConversionError::Parse("ppt: not a compound file".into()))?;
        let stream = cfb
            .stream("PowerPoint Document")
            .ok_or_else(|| ConversionError::Parse("ppt: no PowerPoint Document stream".into()))?;
        if cfb.stream("EncryptedSummary").is_some() {
            return Err(ConversionError::Parse("ppt: document is encrypted".into()));
        }

        // Pass 1: slides from the SlideListWithText (presentation order).
        let mut slides: Vec<Vec<(bool, String)>> = Vec::new();
        for (header, body) in Records::new(&stream) {
            if header.rec_type != RT_DOCUMENT {
                continue;
            }
            for (h2, b2) in Records::new(body) {
                // instance 0 = the slide list (1 = masters, 2 = notes).
                if h2.rec_type == RT_SLIDE_LIST_WITH_TEXT && h2.instance == 0 {
                    collect_slwt_slides(b2, &mut slides);
                }
            }
        }

        // Pass 2: per-slide drawing text, for slides the list left empty (or
        // files with no SlideListWithText at all). Slide containers appear in
        // the stream in presentation order for straight-saved files.
        let container_texts: Vec<Vec<(bool, String)>> = Records::new(&stream)
            .filter(|(h, _)| h.rec_type == RT_SLIDE)
            .map(|(_, body)| {
                let mut runs = Vec::new();
                collect_text_runs(body, &mut runs, 0);
                runs
            })
            .collect();
        if slides.is_empty() {
            slides = container_texts;
        } else {
            for (i, runs) in container_texts.into_iter().enumerate() {
                if let Some(slot) = slides.get_mut(i) {
                    if slot.iter().all(|(_, t)| t.trim().is_empty()) {
                        *slot = runs;
                    }
                }
            }
        }

        let mut doc = DoclingDocument::new(&source.name);
        let mut first = true;
        for runs in slides {
            let has_content = runs.iter().any(|(_, t)| !t.trim().is_empty());
            if !has_content {
                continue;
            }
            if !first {
                doc.push(Node::PageBreak);
            }
            first = false;
            for (is_title, text) in runs {
                for line in text.split('\r') {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if is_title {
                        doc.push(Node::Heading {
                            level: 1,
                            text: line.to_string(),
                        });
                    } else {
                        doc.push(Node::Paragraph {
                            text: line.to_string(),
                        });
                    }
                }
            }
        }
        Ok(doc)
    }
}

/// A record header: `(version, instance, type, length)`.
struct RecordHeader {
    version: u8,
    instance: u16,
    rec_type: u16,
}

/// Iterator over the records of one container body (or the whole stream).
struct Records<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Records<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
}

impl<'a> Iterator for Records<'a> {
    type Item = (RecordHeader, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let d = self.data.get(self.pos..)?;
        if d.len() < 8 {
            return None;
        }
        let ver_inst = u16::from_le_bytes([d[0], d[1]]);
        let rec_type = u16::from_le_bytes([d[2], d[3]]);
        let len = u32::from_le_bytes([d[4], d[5], d[6], d[7]]) as usize;
        let body = d.get(8..8 + len)?;
        self.pos += 8 + len;
        Some((
            RecordHeader {
                version: (ver_inst & 0x0F) as u8,
                instance: ver_inst >> 4,
                rec_type,
            },
            body,
        ))
    }
}

/// Walk a SlideListWithText body: `SlidePersistAtom` starts a new slide, text
/// atoms attach to the current slide with their `TextHeaderAtom` type.
fn collect_slwt_slides(body: &[u8], slides: &mut Vec<Vec<(bool, String)>>) {
    let mut current_is_title = false;
    for (h, b) in Records::new(body) {
        match h.rec_type {
            RT_SLIDE_PERSIST_ATOM => slides.push(Vec::new()),
            RT_TEXT_HEADER_ATOM => {
                let tx = b
                    .get(..4)
                    .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
                    .unwrap_or(u32::MAX);
                current_is_title = tx == TX_TITLE || tx == TX_CENTER_TITLE;
            }
            RT_TEXT_CHARS_ATOM => {
                if let Some(slide) = slides.last_mut() {
                    slide.push((current_is_title, utf16_text(b)));
                }
            }
            RT_TEXT_BYTES_ATOM => {
                if let Some(slide) = slides.last_mut() {
                    slide.push((current_is_title, bytes_text(b)));
                }
            }
            _ => {}
        }
    }
}

/// Recursively collect `(is_title, text)` runs from a record tree (a `Slide`
/// container's drawing). Depth-capped: a crafted file can't recurse forever.
fn collect_text_runs(body: &[u8], out: &mut Vec<(bool, String)>, depth: usize) {
    if depth > 32 {
        return;
    }
    let mut current_is_title = false;
    for (h, b) in Records::new(body) {
        match h.rec_type {
            RT_TEXT_HEADER_ATOM => {
                let tx = b
                    .get(..4)
                    .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
                    .unwrap_or(u32::MAX);
                current_is_title = tx == TX_TITLE || tx == TX_CENTER_TITLE;
            }
            RT_TEXT_CHARS_ATOM => out.push((current_is_title, utf16_text(b))),
            RT_TEXT_BYTES_ATOM => out.push((current_is_title, bytes_text(b))),
            _ if h.version == 0xF => collect_text_runs(b, out, depth + 1),
            _ => {}
        }
    }
}

/// UTF-16LE text of a `TextCharsAtom` body.
fn utf16_text(b: &[u8]) -> String {
    b.chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .map(|u| char::from_u32(u as u32).unwrap_or('\u{FFFD}'))
        .filter(|&c| c != '\u{0000}')
        .collect()
}

/// CP1252 text of a `TextBytesAtom` body (high bytes match Latin-1 closely
/// enough for slide text; the smart-quote block goes through the same table
/// as the DOC backend).
fn bytes_text(b: &[u8]) -> String {
    b.iter().map(|&x| super::doc::cp1252(x)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InputFormat;

    fn fixture(name: &str) -> SourceDocument {
        let path = format!(
            "{}/tests/data/ppt/sources/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let bytes = std::fs::read(&path).expect("fixture exists");
        SourceDocument::from_bytes(name, InputFormat::Ppt, bytes)
    }

    #[test]
    fn extracts_slide_titles_and_text() {
        let doc = PptBackend
            .convert(&fixture("powerpoint_sample.ppt"))
            .expect("converts");
        let headings = doc
            .nodes
            .iter()
            .filter(|n| matches!(n, Node::Heading { .. }))
            .count();
        let paragraphs = doc
            .nodes
            .iter()
            .filter(|n| matches!(n, Node::Paragraph { .. }))
            .count();
        assert!(headings > 0, "expected slide titles: {:?}", doc.nodes);
        assert!(paragraphs > 0, "expected body text: {:?}", doc.nodes);
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        let src = SourceDocument::from_bytes("x.ppt", InputFormat::Ppt, vec![0u8; 128]);
        assert!(PptBackend.convert(&src).is_err());
    }
}
