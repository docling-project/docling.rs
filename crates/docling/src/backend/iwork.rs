//! Apple iWork input (issue #213, #318): Pages / Numbers / Keynote.
//!
//! Modern iWork documents (Pages 5+/Numbers 3+/Keynote 6+, i.e. everything
//! since 2013) are zip packages whose `Index/*.iwa` members hold
//! Snappy-framed Protocol Buffer streams — the IWA format. Apple publishes no
//! schema; the message/field numbers below follow the reverse engineering in
//! the `numbers-parser` and `keynote-parser` projects (MIT) and docling's own
//! Pages reader. Only a handful of archive types matter for content, so the
//! stream is walked with a generic wire-format reader — no generated protobuf
//! code, no schema dependency.
//!
//! **Pages is a conformance format** (#318, #383): upstream docling gained a
//! Pages reader in 2.121 (docling#3934) and completed it in docling#4062
//! (tables in the text flow, text boxes, formatting, lists, images, page
//! furniture, comments), so `.pages` mirrors `IWorkPagesDocumentBackend`
//! byte-for-byte. The Pages code lives beside this module, one file per
//! upstream module: `pages.rs` (the shared content model and the emission
//! into `DoclingDocument`), `pages_iwa.rs` (Pages 5+ `Index/*.iwa`) and
//! `pages_xml.rs` (iWork '09 `index.xml`, optionally gzipped). This module
//! keeps the container handling, the IWA wire primitives the Pages, Keynote
//! and Numbers readers share, and the dispatch.
//!
//! **Keynote is a conformance format too** (#466): upstream gained a Keynote
//! reader in docling#4330 (2.130+), so `.key` mirrors
//! `IWorkKeynoteDocumentBackend` — `keynote.rs` (the presentation model and
//! its emission), `keynote_iwa.rs` (Keynote 6+ `Index/*.iwa`, and the index
//! Keynote 2018+ zips a second time) and `keynote_xml.rs` (iWork '09
//! `index.apxl`), reusing the Pages readers for everything on a slide.
//!
//! **Numbers is a conformance format as well** (#1031): upstream gained a
//! Numbers reader in docling#4113 (with docling#5087 and docling#5101), so
//! `.numbers` mirrors `IWorkNumbersDocumentBackend` — `numbers.rs` (the
//! sheet model and its emission: a page and a `sheet` group per sheet,
//! tables as real grids, charts, pictures, sticky notes), `numbers_iwa.rs`
//! (Numbers 3+ `Index/*.iwa`) and `numbers_xml.rs` (iWork '09 `index.xml`),
//! with the packed cell decoding the Pages tables share in `iwork_cells.rs`.
//!
//! The wire walk is defensive throughout: unknown fields are skipped, short
//! buffers end the walk, and a package with no extractable text converts to
//! an empty document rather than erroring (a blank presentation is not a
//! failure).

use crate::backend::ooxml::Package;
use crate::backend::{
    keynote, keynote_iwa, keynote_xml, numbers, numbers_iwa, numbers_xml, pages, pages_iwa,
    pages_xml, DeclarativeBackend,
};
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::{DoclingDocument, Table, TableCell};

/// One decoded IWA archive: object identifier, message type of its first
/// message, and that message's payload.
pub(crate) struct Archive {
    pub(crate) id: u64,
    pub(crate) ty: u32,
    pub(crate) payload: Vec<u8>,
}

#[derive(Default)]
pub struct IworkBackend {
    /// 1-based inclusive window of Numbers sheets to keep (docling's
    /// `page_range`: each sheet is a page); Pages and Keynote ignore it.
    pub page_range: Option<(usize, usize)>,
}

impl DeclarativeBackend for IworkBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let mut pkg = Package::open(&source.bytes)
            .ok_or_else(|| ConversionError::Parse("iwork: not a zip package".into()))?;
        let flavor = Flavor::of(source.format);
        let page_range = self.page_range;

        // docling's `is_encrypted`: Pages does not set the standard ZIP
        // encryption flag on a password-protected document — it writes a
        // compression method outside the ZIP-defined set — so both signals are
        // checked before any member is decompressed.
        // Typed as not decryptable (#636): no password opens it here, so a
        // caller must not prompt for one — the wording stays the same.
        if pkg.any_encrypted() {
            return Err(ConversionError::encrypted_with_message(
                "iwork",
                crate::EncryptionError::NotDecryptable("iWork package encryption".into()),
                format!(
                    "the document is password-protected; docling.rs cannot read encrypted \
                     iWork documents. Remove the password in {} and save again",
                    flavor.app()
                ),
            ));
        }

        // Keynote has its own dispatch: three container generations, images
        // that stay in the outer container when the index is nested (#466).
        if flavor == Flavor::Keynote {
            let mut doc = DoclingDocument::new(&source.name);
            keynote::emit(read_keynote(&mut pkg)?, &mut doc);
            return Ok(doc);
        }

        // A zipped *package* (the pre-single-file "bundle" layout, or a
        // user-zipped bundle directory) nests the IWA members inside an
        // Index.zip member — unwrap it and read that archive instead.
        if !pkg.names().any(|n| n.ends_with(".iwa")) {
            let inner = pkg
                .names()
                .find(|n| *n == "Index.zip" || n.ends_with("/Index.zip"))
                .map(str::to_string);
            if let Some(inner) = inner {
                if let Some(bytes) = pkg.read_bytes(&inner) {
                    pkg = Package::open(&bytes).ok_or_else(|| {
                        ConversionError::Parse("iwork: Index.zip is not a zip".into())
                    })?;
                }
            }
        }

        // `contains` rather than a prefix match: some producers nest the
        // package under an extra directory.
        let has_iwa = pkg
            .names()
            .any(|n| n.ends_with(".iwa") && n.contains("Index/"));
        let mut doc = DoclingDocument::new(&source.name);
        // iWork '09 and earlier: a plain (optionally gzipped) index.xml.
        let legacy = ["index.xml", "index.xml.gz"]
            .into_iter()
            .find(|m| pkg.names().any(|n| n == *m));
        let kind = flavor.app();
        if !has_iwa && legacy.is_none() {
            return Err(ConversionError::Parse(format!(
                "iwork: a ZIP archive, but not a {kind} document — it has neither an \
                 Index/ directory nor an index.xml"
            )));
        }
        match (flavor, legacy) {
            (Flavor::Pages, Some(member)) if !has_iwa => {
                pages::emit(pages_xml::read_content(&mut pkg, member)?, &mut doc)
            }
            (Flavor::Pages, _) => pages::emit(pages_iwa::read_content(&mut pkg)?, &mut doc),
            (_, Some(member)) if !has_iwa => {
                let sheets = numbers_xml::read_content(&mut pkg, member)?;
                numbers::emit(sheets, page_range, &mut doc);
            }
            _ => {
                let sheets = numbers_iwa::read_content(&mut pkg)?;
                numbers::emit(sheets, page_range, &mut doc);
            }
        }
        Ok(doc)
    }
}

/// docling's `IWorkKeynoteDocumentBackend._read_document`: dispatch to the
/// reader for whichever generation wrote the container. Keynote 6+ keeps
/// `Index/*.iwa` in the container itself; Keynote 2018+ zips that index a
/// second time into `<dir>/Index.zip` while the `Data/` members stay unzipped
/// beside it (so the outer container is what holds the images, under that
/// directory as a prefix); iWork '09 wrote a plain `index.apxl`, optionally
/// gzipped.
fn read_keynote(pkg: &mut Package) -> Result<keynote::Presentation, ConversionError> {
    let names: Vec<String> = pkg.names().map(str::to_string).collect();
    if names
        .iter()
        .any(|n| n.starts_with("Index/") && n.ends_with(".iwa"))
    {
        let archives = pages_iwa::read_archives(pkg)?;
        return keynote_iwa::read_content(&archives, pkg, "");
    }
    // `_is_nested_index`: the `Index.zip` at the root of the one directory
    // the package was flattened into; anything deeper is something else that
    // happens to be called that.
    let nested = names
        .iter()
        .filter(|n| {
            n.strip_suffix("Index.zip").is_some_and(|dir| {
                dir.is_empty() || (dir.ends_with('/') && dir.matches('/').count() == 1)
            })
        })
        .min()
        .cloned();
    if let Some(member) = nested {
        let bytes = pkg
            .read_bytes(&member)
            .ok_or_else(|| ConversionError::Parse(format!("iwork: could not read '{member}'")))?;
        let mut index = Package::open(&bytes)
            .ok_or_else(|| ConversionError::Parse("iwork: Index.zip is not a zip".into()))?;
        let archives = pages_iwa::read_archives(&mut index)?;
        let prefix = &member[..member.len() - "Index.zip".len()];
        return keynote_iwa::read_content(&archives, pkg, prefix);
    }
    if let Some(member) = ["index.apxl", "index.apxl.gz"]
        .into_iter()
        .find(|m| names.iter().any(|n| n == m))
    {
        return keynote_xml::read_content(pkg, member);
    }
    Err(ConversionError::Parse(
        "iwork: a ZIP archive, but not a Keynote document — it has neither an Index/ \
         directory nor an Index.zip nor an index.apxl"
            .into(),
    ))
}

#[derive(Clone, Copy, PartialEq)]
enum Flavor {
    Pages,
    Numbers,
    Keynote,
}

impl Flavor {
    /// The application a reader is pointed back at (docling's `kind`).
    fn app(self) -> &'static str {
        match self {
            Flavor::Pages => "Pages",
            Flavor::Numbers => "Numbers",
            Flavor::Keynote => "Keynote",
        }
    }

    fn of(format: crate::InputFormat) -> Self {
        match format {
            crate::InputFormat::Numbers => Flavor::Numbers,
            crate::InputFormat::Keynote => Flavor::Keynote,
            _ => Flavor::Pages,
        }
    }
}

// --- IWA container ----------------------------------------------------------

/// Un-frame and decompress one `.iwa` member: a sequence of
/// `[type: u8 = 0][length: u24 LE][raw Snappy block]` chunks (Apple frames
/// Snappy itself — this is not the standard Snappy stream format).
pub(crate) fn decode_iwa(bytes: &[u8]) -> Result<Vec<u8>, ConversionError> {
    let mut out = Vec::with_capacity(bytes.len() * 3);
    let mut pos = 0usize;
    let mut snappy = snap::raw::Decoder::new();
    while pos + 4 <= bytes.len() {
        let len = u32::from_le_bytes([bytes[pos + 1], bytes[pos + 2], bytes[pos + 3], 0]) as usize;
        let ty = bytes[pos];
        pos += 4;
        let Some(block) = bytes.get(pos..pos + len) else {
            return Err(ConversionError::Parse("iwork: truncated IWA chunk".into()));
        };
        pos += len;
        match ty {
            0 => out.extend_from_slice(
                &snappy
                    .decompress_vec(block)
                    .map_err(|e| ConversionError::Parse(format!("iwork: snappy: {e}")))?,
            ),
            // Type 1 (uncompressed) has never been observed in the wild but
            // is trivial to honor.
            1 => out.extend_from_slice(block),
            other => {
                return Err(ConversionError::Parse(format!(
                    "iwork: unknown IWA chunk type {other}"
                )))
            }
        }
    }
    Ok(out)
}

/// Split the decompressed stream into archives:
/// `[varint length][TSP.ArchiveInfo][payload per MessageInfo.length]…`.
///
/// With `all_messages` false only each archive's first message is kept —
/// follow-on messages are auxiliary (object-level undo state and the like).
/// The Pages parity path passes true: docling's `iter_objects` yields every
/// message under the archive's identifier and keys them last-wins, and
/// reproducing that keeps object lookup identical to upstream.
pub(crate) fn parse_archives(mut stream: &[u8], out: &mut Vec<Archive>, all_messages: bool) {
    while !stream.is_empty() {
        let Some((info_len, rest)) = read_varint(stream) else {
            return;
        };
        let Some(info) = rest.get(..info_len as usize) else {
            return;
        };
        let after = &rest[info_len as usize..];

        // TSP.ArchiveInfo: field 1 = identifier, field 2 = repeated MessageInfo.
        let mut id = 0u64;
        let mut messages: Vec<(u32, usize)> = Vec::new();
        for (field, value) in Fields::new(info) {
            match (field, value) {
                (1, Value::Varint(v)) => id = v,
                (2, Value::Bytes(mi)) => {
                    // TSP.MessageInfo: field 1 = type, field 3 = length.
                    let (mut ty, mut len) = (0u32, 0usize);
                    for (f, v) in Fields::new(mi) {
                        match (f, v) {
                            (1, Value::Varint(t)) => ty = t as u32,
                            (3, Value::Varint(l)) => len = l as usize,
                            _ => {}
                        }
                    }
                    messages.push((ty, len));
                }
                _ => {}
            }
        }
        let mut consumed = 0usize;
        for (i, (ty, len)) in messages.iter().enumerate() {
            let Some(payload) = after.get(consumed..consumed + len) else {
                return;
            };
            if i == 0 || all_messages {
                out.push(Archive {
                    id,
                    ty: *ty,
                    payload: payload.to_vec(),
                });
            }
            consumed += len;
        }
        let Some(next) = after.get(consumed..) else {
            return;
        };
        stream = next;
    }
}

// --- protobuf wire walking --------------------------------------------------

pub(crate) enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    #[allow(dead_code)]
    Fixed32(u32),
    #[allow(dead_code)]
    Fixed64(u64),
}

/// Iterator over a message's `(field number, value)` pairs. Malformed input
/// ends the iteration — never panics, never reads past the buffer.
pub(crate) struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Fields(buf)
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = (u32, Value<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        let (tag, rest) = read_varint(self.0)?;
        let field = (tag >> 3) as u32;
        let value = match tag & 7 {
            0 => {
                let (v, rest) = read_varint(rest)?;
                self.0 = rest;
                Value::Varint(v)
            }
            1 => {
                let v = u64::from_le_bytes(rest.get(..8)?.try_into().ok()?);
                self.0 = &rest[8..];
                Value::Fixed64(v)
            }
            2 => {
                let (len, rest) = read_varint(rest)?;
                let bytes = rest.get(..len as usize)?;
                self.0 = &rest[len as usize..];
                Value::Bytes(bytes)
            }
            5 => {
                let v = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?);
                self.0 = &rest[4..];
                Value::Fixed32(v)
            }
            // Groups (3/4) predate protobuf 2 and never appear in IWA.
            _ => return None,
        };
        Some((field, value))
    }
}

pub(crate) fn read_varint(buf: &[u8]) -> Option<(u64, &[u8])> {
    let mut value = 0u64;
    for (i, &b) in buf.iter().enumerate().take(10) {
        value |= u64::from(b & 0x7f) << (7 * i as u32);
        if b & 0x80 == 0 {
            return Some((value, &buf[i + 1..]));
        }
    }
    None
}

/// `TSP.Reference`: field 1 = the referenced archive's identifier.
pub(crate) fn reference(bytes: &[u8]) -> Option<u64> {
    Fields::new(bytes).find_map(|(f, v)| match (f, v) {
        (1, Value::Varint(id)) => Some(id),
        _ => None,
    })
}

// --- Shared wire/table helpers (also used by the Pages reader modules) ------

/// First length-delimited value of `field` (docling's `fields.get(f, [None])[0]`
/// with its `isinstance(..., bytes)` check).
pub(crate) fn first_bytes(payload: &[u8], field: u32) -> Option<&[u8]> {
    Fields::new(payload).find_map(|(f, v)| match v {
        Value::Bytes(b) if f == field => Some(b),
        _ => None,
    })
}

/// First varint value of `field`.
pub(crate) fn first_varint(payload: &[u8], field: u32) -> Option<u64> {
    Fields::new(payload).find_map(|(f, v)| match v {
        Value::Varint(n) if f == field => Some(n),
        _ => None,
    })
}

/// A `Table` from first-class cells: the dense `rows` grid every serializer
/// renders (unread positions empty), plus the cells themselves so JSON
/// carries docling's per-cell `column_header` flags (`row < header_rows`).
pub(crate) fn grid_table(num_rows: usize, num_cols: usize, cells: Vec<TableCell>) -> Table {
    let mut rows = vec![vec![String::new(); num_cols]; num_rows];
    for cell in &cells {
        if cell.start_row < num_rows && cell.start_col < num_cols {
            rows[cell.start_row][cell.start_col] = cell.text.clone();
        }
    }
    Table {
        rows,
        cells: Some(cells),
        ..Default::default()
    }
}

pub(crate) fn text_cell(text: String, row: usize, col: usize, header_rows: usize) -> TableCell {
    TableCell {
        text,
        bbox: None,
        start_row: row,
        start_col: col,
        row_span: 1,
        col_span: 1,
        column_header: row < header_rows,
        row_header: false,
        row_section: false,
    }
}

pub(crate) fn gunzip_capped(
    raw: &[u8],
    cap: u64,
    member: &str,
) -> Result<Vec<u8>, ConversionError> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(raw)
        .take(cap + 1)
        .read_to_end(&mut out)
        .map_err(|_| ConversionError::Parse(format!("iwork: could not decompress '{member}'")))?;
    if out.len() as u64 > cap {
        return Err(ConversionError::Parse(format!(
            "iwork: '{member}' expands beyond the {cap} byte limit"
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_and_fields_walk_defensively() {
        assert_eq!(read_varint(&[0x96, 0x01]), Some((150, &[][..])));
        assert_eq!(read_varint(&[0x80]), None); // truncated continuation
                                                // field 1 varint 5, field 2 bytes "ab"
        let msg = [0x08, 0x05, 0x12, 0x02, b'a', b'b'];
        let items: Vec<u32> = Fields::new(&msg).map(|(f, _)| f).collect();
        assert_eq!(items, vec![1, 2]);
        // Truncated length-delimited field ends the walk cleanly.
        let bad = [0x12, 0x0A, b'x'];
        assert_eq!(Fields::new(&bad).count(), 0);
    }
}
