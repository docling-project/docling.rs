//! AFP / MO:DCA print streams (`.afp`, `application/vnd.ibm.modcap`) — a port
//! of docling's minimal `AfpDocumentBackend` (docling#4177, #1034).
//!
//! An AFP file is a flat sequence of MO:DCA *structured fields*: an `X'5A'`
//! carriage-control byte, a two-byte length (the eight-byte introducer
//! included, the `X'5A'` not), a three-byte identifier, a flag byte and two
//! reserved bytes, then the data — optionally behind an introducer extension
//! (flag `0x01`) and followed by padding (flag `0x10`). The backend keeps the
//! page boundaries (Begin/End Page) and reads the text out of each page's
//! PTOCA presentation-text data: the character strings of the Transparent
//! Data control sequences (TRN), a line ending at Begin Line or at an Absolute
//! Move Baseline to another baseline (Apache FOP places every line that way).
//!
//! Like upstream, code-page resources are not resolved: text decodes as
//! cp500 (International EBCDIC, the ebcdic backend's table, byte-for-byte
//! Python's codec), and C0/C1 controls other than tab and newline are dropped.
//! Page geometry is not reconstructed (pages are `0 × 0`, items a zero bbox
//! with a `[0, len]` charspan) and images, graphics, barcodes, object
//! containers, overlays and page segments are skipped with one warning per
//! kind — a page segment's or overlay's text is a resource, not page content,
//! and it lies outside any page, so it is never read.
//!
//! Each page is docling's `chapter` group `page-N` with one `text` item per
//! non-blank line; a page window (`page_range`) keeps the original page
//! numbers. Malformed structure — a truncated or mis-sized field, a page
//! opened inside a page, an unclosed page — is a parse error naming the byte
//! offset, as upstream's `AfpParseError`.

use std::collections::BTreeMap;

use docling_core::{DoclingDocument, Node};

use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

const INTRODUCER: u8 = 0x5A;
const BASE_INTRODUCER_LENGTH: usize = 8;
const MAX_STRUCTURED_FIELD_LENGTH: usize = 32767;
const CONTROL_SEQUENCE_PREFIX: &[u8] = b"\x2b\xd3";

const BEGIN_PAGE: [u8; 3] = [0xD3, 0xA8, 0xAF];
const END_PAGE: [u8; 3] = [0xD3, 0xA9, 0xAF];
const END_PRESENTATION_TEXT: [u8; 3] = [0xD3, 0xA9, 0x9B];
const PRESENTATION_TEXT_DATA: [u8; 3] = [0xD3, 0xEE, 0x9B];

/// The data-bearing structured fields of content architectures the backend
/// does not render — upstream counts the data records (not every descriptor
/// and bracket) so each warning measures the omitted content.
const UNSUPPORTED_CONTENT: &[([u8; 3], &str)] = &[
    ([0xD3, 0xA8, 0x5F], "page-segment resource"),
    ([0xD3, 0xA8, 0xDF], "overlay resource"),
    ([0xD3, 0xEE, 0xFB], "image data (IOCA)"),
    ([0xD3, 0xEE, 0x7B], "image raster data (IM)"),
    ([0xD3, 0xEE, 0xBB], "graphics data (GOCA)"),
    ([0xD3, 0xEE, 0xEB], "barcode data (BCOCA)"),
    ([0xD3, 0xEE, 0x92], "object-container data"),
];

/// PTOCA function types: Transparent Data, Begin Line, Absolute Move
/// Baseline (each in its unchained / chaining spelling).
const TRN_TYPES: [u8; 2] = [0xDA, 0xDB];
const BEGIN_LINE_TYPES: [u8; 2] = [0xD8, 0xD9];
const ABSOLUTE_MOVE_BASELINE_TYPES: [u8; 2] = [0xD2, 0xD3];

pub struct AfpBackend {
    /// The converter's 1-based inclusive page window; pages outside it are
    /// dropped and the rest keep their numbers.
    pub page_range: Option<(usize, usize)>,
}

/// Whether `bytes` open with a MO:DCA structured-field introducer —
/// docling's `_detect_afp` / `is_valid`: `X'5A'`, a length within 8..=32767,
/// an identifier whose class byte is `X'D3'`.
pub(crate) fn looks_like_afp(bytes: &[u8]) -> bool {
    if bytes.len() < 9 || bytes[0] != INTRODUCER {
        return false;
    }
    let length = u16::from_be_bytes([bytes[1], bytes[2]]) as usize;
    (BASE_INTRODUCER_LENGTH..=MAX_STRUCTURED_FIELD_LENGTH).contains(&length) && bytes[3] == 0xD3
}

fn parse_error(message: String) -> ConversionError {
    ConversionError::Parse(format!("afp: {message}"))
}

struct StructuredField<'a> {
    identifier: [u8; 3],
    data: &'a [u8],
    offset: usize,
}

/// Split an AFP byte stream into its structured fields — upstream's
/// `_iter_structured_fields`, with the same checks and messages.
fn structured_fields(content: &[u8]) -> Result<Vec<StructuredField<'_>>, ConversionError> {
    let mut fields = Vec::new();
    let mut offset = 0;
    while offset < content.len() {
        let remaining = content.len() - offset;
        if remaining < BASE_INTRODUCER_LENGTH + 1 {
            return Err(parse_error(format!(
                "AFP input ends with a truncated structured-field introducer at byte {offset}."
            )));
        }
        if content[offset] != INTRODUCER {
            return Err(parse_error(format!(
                "Expected AFP structured-field introducer X'5A' at byte {offset}, found X'{:02X}'.",
                content[offset]
            )));
        }
        let length = u16::from_be_bytes([content[offset + 1], content[offset + 2]]) as usize;
        if length < BASE_INTRODUCER_LENGTH {
            return Err(parse_error(format!(
                "AFP structured field at byte {offset} declares invalid length {length}; \
                 the minimum is {BASE_INTRODUCER_LENGTH}."
            )));
        }
        if length > MAX_STRUCTURED_FIELD_LENGTH {
            return Err(parse_error(format!(
                "AFP structured field at byte {offset} declares invalid length {length}; \
                 the maximum is {MAX_STRUCTURED_FIELD_LENGTH}."
            )));
        }
        let end = offset + 1 + length;
        if end > content.len() {
            return Err(parse_error(format!(
                "AFP structured field at byte {offset} declares {length} bytes, but only {} remain.",
                remaining - 1
            )));
        }
        let identifier = [
            content[offset + 3],
            content[offset + 4],
            content[offset + 5],
        ];
        let flags = content[offset + 6];
        let mut data_start = offset + 9;
        if flags & 0x01 != 0 {
            // The Structured Field Introducer extension: its first byte is
            // its own length, that byte included.
            if data_start >= end {
                return Err(parse_error(format!(
                    "AFP structured field at byte {offset} flags an introducer extension \
                     but does not contain its length."
                )));
            }
            let extension_length = content[data_start] as usize;
            if extension_length < 1 || data_start + extension_length > end {
                return Err(parse_error(format!(
                    "AFP structured field at byte {offset} has invalid introducer extension \
                     length {extension_length}."
                )));
            }
            data_start += extension_length;
        }
        let mut data_end = end;
        if flags & 0x10 != 0 {
            // Padding: its last byte is its length, or — when that is 0 —
            // the two bytes before it are.
            let mut padding_length = content[end - 1] as usize;
            if padding_length == 0 && end - data_start >= 3 {
                padding_length = u16::from_be_bytes([content[end - 3], content[end - 2]]) as usize;
            }
            if padding_length < 1 || padding_length > end - data_start {
                return Err(parse_error(format!(
                    "AFP structured field at byte {offset} has invalid padding length \
                     {padding_length}."
                )));
            }
            data_end -= padding_length;
        }
        fields.push(StructuredField {
            identifier,
            data: &content[data_start..data_end],
            offset,
        });
        offset = end;
    }
    Ok(fields)
}

/// The TRN character strings of PTOCA data with their line boundaries —
/// upstream's `_extract_ptoca_text`. A line ends at Begin Line, or at an
/// Absolute Move Baseline to a baseline other than the current one.
fn ptoca_text(data: &[u8]) -> Result<String, ConversionError> {
    let mut text = String::new();
    let mut offset = 0;
    let mut chained = false;
    let mut baseline: Option<i16> = None;
    while offset < data.len() {
        let length_offset = if chained {
            // An odd function type chains the *next* control sequence,
            // which omits `X'2BD3'`; an even type ends the chain.
            offset
        } else if data[offset..].starts_with(CONTROL_SEQUENCE_PREFIX) {
            offset + CONTROL_SEQUENCE_PREFIX.len()
        } else {
            offset += 1;
            continue;
        };
        if length_offset + 2 > data.len() {
            return Err(parse_error(format!(
                "PTOCA data ends inside a control-sequence header at byte {offset}."
            )));
        }
        let length = data[length_offset] as usize;
        if length < 2 {
            return Err(parse_error(format!(
                "PTOCA control sequence at byte {offset} declares invalid length {length}; \
                 the minimum is 2."
            )));
        }
        let end = length_offset + length;
        if end > data.len() {
            return Err(parse_error(format!(
                "PTOCA control sequence at byte {offset} declares {length} bytes, but the \
                 presentation-text object ends first."
            )));
        }
        let function_type = data[length_offset + 1];
        chained = function_type & 0x01 != 0;
        let parameters = &data[length_offset + 2..end];
        let mut new_line = BEGIN_LINE_TYPES.contains(&function_type);
        if TRN_TYPES.contains(&function_type) {
            text.extend(parameters.iter().map(|&b| cp500(b)));
        } else if new_line {
            baseline = None;
        } else if ABSOLUTE_MOVE_BASELINE_TYPES.contains(&function_type) && parameters.len() >= 2 {
            let position = i16::from_be_bytes([parameters[0], parameters[1]]);
            new_line = Some(position) != baseline;
            baseline = Some(position);
        }
        if new_line && !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        offset = end;
    }
    // Python's `unicodedata.category(c) != "Cc"`: the C0/C1 controls and DEL
    // (cp500 maps every byte, so the controls come through as themselves).
    Ok(text
        .chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect())
}

fn cp500(byte: u8) -> char {
    char::from_u32(u32::from(
        crate::backend::ebcdic::cp500_table()[byte as usize],
    ))
    .unwrap_or('\u{FFFD}')
}

struct Page {
    number: usize,
    ptoca: Vec<u8>,
    text_blocks: Vec<String>,
}

impl Page {
    /// End of a presentation-text object: its accumulated PTOCA becomes one
    /// text block.
    fn flush(&mut self) -> Result<(), ConversionError> {
        if self.ptoca.is_empty() {
            return Ok(());
        }
        let text = ptoca_text(&self.ptoca)?;
        let text = text.trim();
        if !text.is_empty() {
            self.text_blocks.push(text.to_string());
        }
        self.ptoca.clear();
        Ok(())
    }
}

/// The pages of an AFP stream and the count of skipped data records per
/// unsupported content kind — upstream's `_parse`.
fn parse(content: &[u8]) -> Result<(Vec<Page>, BTreeMap<&'static str, usize>), ConversionError> {
    let mut pages = Vec::new();
    let mut current: Option<Page> = None;
    let mut unsupported = BTreeMap::new();
    for field in structured_fields(content)? {
        let id = field.identifier;
        if id == BEGIN_PAGE {
            if current.is_some() {
                return Err(parse_error(format!(
                    "AFP Begin Page at byte {} occurs before the preceding page ends.",
                    field.offset
                )));
            }
            current = Some(Page {
                number: pages.len() + 1,
                ptoca: Vec::new(),
                text_blocks: Vec::new(),
            });
        } else if id == PRESENTATION_TEXT_DATA {
            // Presentation text outside a page (a resource's) is not page
            // content.
            if let Some(page) = current.as_mut() {
                page.ptoca.extend_from_slice(field.data);
            }
        } else if id == END_PRESENTATION_TEXT {
            if let Some(page) = current.as_mut() {
                page.flush()?;
            }
        } else if id == END_PAGE {
            let Some(mut page) = current.take() else {
                return Err(parse_error(format!(
                    "AFP End Page at byte {} has no matching Begin Page.",
                    field.offset
                )));
            };
            page.flush()?;
            pages.push(page);
        } else if let Some((_, kind)) = UNSUPPORTED_CONTENT.iter().find(|(sf, _)| *sf == id) {
            *unsupported.entry(*kind).or_insert(0) += 1;
        }
    }
    if let Some(page) = current {
        return Err(parse_error(format!(
            "AFP page {} has no matching End Page.",
            page.number
        )));
    }
    Ok((pages, unsupported))
}

impl DeclarativeBackend for AfpBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        if !looks_like_afp(&source.bytes) {
            return Err(parse_error(
                "the input does not start with a valid MO:DCA structured field".into(),
            ));
        }
        let (pages, unsupported) = parse(&source.bytes)?;
        for (kind, count) in unsupported {
            eprintln!(
                "docling: skipped {count} AFP {kind} structured field(s): the AFP backend \
                 extracts PTOCA text and page boundaries but does not render {kind}"
            );
        }
        docling_core::debug_log!(
            "afp: code-page resources are not resolved; decoding PTOCA text as cp500"
        );
        let (first, last) = self.page_range.unwrap_or((1, usize::MAX));
        let mut doc = DoclingDocument::new(&source.name);
        let mut emitted = false;
        for page in pages.iter().filter(|p| (first..=last).contains(&p.number)) {
            if emitted {
                doc.push(Node::PageBreak);
            }
            emitted = true;
            doc.push(Node::PageInfo {
                page_no: page.number,
                width: 0.0,
                height: 0.0,
            });
            let children = page
                .text_blocks
                .iter()
                .flat_map(|block| block.lines())
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(|line| Node::Prov {
                    page_no: page.number,
                    bbox: [0.0; 4],
                    // docling's charspan counts Python characters.
                    charspan: [0, line.chars().count()],
                    bottom_left: false,
                    seq: None,
                    inner: Box::new(Node::Paragraph {
                        text: line.to_string(),
                    }),
                })
                .collect();
            doc.push(Node::Group {
                label: "chapter".into(),
                name: Some(format!("page-{}", page.number)),
                layer: None,
                children,
            });
        }
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trn(text: &str) -> Vec<u8> {
        let encoded = encode_cp500(text);
        let mut out = b"\x2b\xd3".to_vec();
        out.push(encoded.len() as u8 + 2);
        out.push(0xDA);
        out.extend(encoded);
        out
    }

    fn encode_cp500(text: &str) -> Vec<u8> {
        let table = crate::backend::ebcdic::cp500_table();
        text.chars()
            .map(|c| {
                table
                    .iter()
                    .position(|&u| u32::from(u) == c as u32)
                    .unwrap() as u8
            })
            .collect()
    }

    fn mv(function_type: u8, displacement: i16) -> Vec<u8> {
        let mut out = vec![0x2b, 0xd3, 0x04, function_type];
        out.extend(displacement.to_be_bytes());
        out
    }

    #[test]
    fn chained_trn_and_control_characters() {
        let mut data = b"\x2b\xd3".to_vec();
        let first = encode_cp500("First");
        data.push(first.len() as u8 + 2);
        data.push(0xDB); // chains the next sequence
        data.extend(first);
        let second = [vec![0x00], encode_cp500("Second")].concat();
        data.push(second.len() as u8 + 2);
        data.push(0xDA);
        data.extend(second);
        data.push(0x00);
        assert_eq!(ptoca_text(&data).unwrap(), "FirstSecond");
    }

    #[test]
    fn chain_ends_with_an_even_function_type() {
        let data = [
            b"\x2b\xd3\x02\xd9".to_vec(),
            trn("Hello")[2..].to_vec(),
            trn("World"),
        ]
        .concat();
        assert_eq!(ptoca_text(&data).unwrap(), "HelloWorld");
    }

    #[test]
    fn code_points_after_an_unchained_sequence_are_not_a_chain() {
        let data = [trn("A"), encode_cp500("Hi"), trn("B")].concat();
        assert_eq!(ptoca_text(&data).unwrap(), "AB");
    }

    #[test]
    fn new_baseline_starts_a_new_line() {
        let data = [
            mv(0xD2, 100),
            mv(0xC6, 50),
            trn("Invoice 4711"),
            mv(0xD2, 120),
            mv(0xC6, 50),
            trn("Due 2026-11-01"),
            mv(0xD2, 140),
            trn("Net 30"),
        ]
        .concat();
        assert_eq!(
            ptoca_text(&data).unwrap(),
            "Invoice 4711\nDue 2026-11-01\nNet 30"
        );
        let same = [
            mv(0xD2, 100),
            trn("Hello "),
            mv(0xD2, 100),
            mv(0xC6, 300),
            trn("World"),
        ]
        .concat();
        assert_eq!(ptoca_text(&same).unwrap(), "Hello World");
    }

    #[test]
    fn rejects_truncated_or_invalid_sequences() {
        for (data, message) in [
            (&b"\x2b\xd3"[..], "ends inside a control-sequence header"),
            (b"\x2b\xd3\x01\xda", "the minimum is 2"),
            (b"\x2b\xd3\x05\xdaA", "presentation-text object ends first"),
        ] {
            let err = ptoca_text(data).unwrap_err().to_string();
            assert!(err.contains(message), "{err}");
        }
    }

    #[test]
    fn rejects_invalid_introducers() {
        for (data, message) in [
            (
                &b"\x5a\x00\x08"[..],
                "truncated structured-field introducer",
            ),
            (b"\x00\x00\x08\xd3\xa8\xa8\x00\x00\x00", "X'5A' at byte 0"),
            (b"\x5a\x00\x07\xd3\xa8\xa8\x00\x00\x00", "the minimum is 8"),
            (
                b"\x5a\x80\x00\xd3\xa8\xa8\x00\x00\x00",
                "the maximum is 32767",
            ),
            (
                b"\x5a\x00\x20\xd3\xa8\xa8\x00\x00\x00",
                "byte 0 declares 32 bytes",
            ),
            (
                b"\x5a\x00\x08\xd3\xa8\xa8\x01\x00\x00",
                "does not contain its length",
            ),
            (
                b"\x5a\x00\x09\xd3\xa8\xa8\x01\x00\x00\x00",
                "invalid introducer extension length 0",
            ),
            (
                b"\x5a\x00\x0a\xd3\xa8\xa8\x01\x00\x00\x03\xaa",
                "invalid introducer extension length 3",
            ),
            (
                b"\x5a\x00\x09\xd3\xa8\xa8\x10\x00\x00\x00",
                "invalid padding length 0",
            ),
        ] {
            let err = match structured_fields(data) {
                Ok(_) => panic!("{message}: accepted"),
                Err(e) => e.to_string(),
            };
            assert!(err.contains(message), "{err}");
        }
    }

    #[test]
    fn accepts_two_byte_padding_length() {
        let mut field = b"\x5a\x00\x12\xd3\xee\x9b\x10\x00\x00".to_vec();
        field.extend(b"payload\x00\x03\x00");
        let fields = structured_fields(&field).unwrap();
        assert_eq!(fields[0].data, b"payload");
    }
}
