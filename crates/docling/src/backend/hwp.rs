//! HWP (`.hwp`, `.hwpx`) — Hancom's Hangul word processor (한글), Korea's
//! standard office format, a docling.rs extension (#216); docling has no
//! reader for it. Two generations are read, both into one stream of
//! paragraphs and tables:
//!
//! * **HWP 5** (`.hwp`, Hangul 2002 on) — an OLE compound file: a
//!   `FileHeader` (signature, version, flags: compressed, password,
//!   distribution), a `DocInfo` stream of shared properties and one
//!   `BodyText/SectionN` stream per section, both raw-deflated when the
//!   compressed flag is set. A stream is a sequence of tagged records — a
//!   32-bit header packs the tag (10 bits), the nesting level (10) and the
//!   size (12, `0xFFF` = a 32-bit size follows). A paragraph is a
//!   `PARA_HEADER` with its `PARA_TEXT` (UTF-16LE, in which code units
//!   below 32 are controls: most take 8 units, and an *extended* one —
//!   table, drawing, header, footnote, field — stands for the next
//!   `CTRL_HEADER` child, whose subtree holds the object's own paragraph
//!   lists), `PARA_CHAR_SHAPE` (the character shape in force from each
//!   position) and its controls one level down. The format is published
//!   by Hancom ("HWP 5.0 Document File Structure"); hwplib (Apache-2.0)
//!   was the second reference for the record layouts.
//! * **HWPX** (`.hwpx`, Hangul 2010 on; KS X 6101 "OWPML") — a ZIP with the
//!   `application/hwp+zip` mimetype: `Contents/header.xml` declares the
//!   character (`hh:charPr`) and paragraph (`hh:paraPr`) properties,
//!   `Contents/sectionN.xml` holds the body as `hp:p` / `hp:run` / `hp:t`,
//!   tables as `hp:tbl` / `hp:tr` / `hp:tc` with their `hp:cellAddr` and
//!   `hp:cellSpan`, and `Contents/content.hpf` lists the `BinData/` images.
//!
//! In both, a paragraph whose paragraph shape carries an *outline* head
//! (the `개요 1…7` styles) is a heading of that depth, a *number* or
//! *bullet* head a list item; bold, italic, underline, strike-through and
//! sub/superscript come from the character shapes. Tables keep their
//! spans and caption, text boxes and pictures stay where they are
//! anchored, an equation keeps its Hangul equation script as text, ruby
//! text (덧말) follows its base in parentheses, composed characters
//! (글자 겹치기) and text art keep their text; page
//! headers/footers and foot/endnotes are kept as furniture
//! (`page_header` / `page_footer` / `footnote`), the way the Word 97
//! reader keeps its stories. HWPX's inline change tracking follows
//! `track_changes` (`accept`, the default, or `reject`; HWP 5 keeps its
//! text as stored and refuses `reject`, both refuse `all`). A
//! password-protected document raises the a *distribution* document (배포용 문서, its
//! body replaced by encrypted `ViewText` streams) and the HWP 3 format of
//! the 1990s are refused by name.

use std::collections::HashMap;
use std::io::Read;

use docling_core::{DoclingDocument, Node, PictureImage, Script, Table, TableCell, TrackChanges};
use roxmltree::Node as XmlNode;

use crate::backend::cfb::CompoundFile;
use crate::backend::markdown::escape_text;
use crate::backend::ooxml::{self, Package};
use crate::backend::styled::{Para, Style};
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

#[derive(Debug, Clone, Copy, Default)]
pub struct HwpBackend {
    /// Which side of HWPX's tracked changes is read (#851's option):
    /// `accept` (or unset) leaves the deleted text out, `reject` the
    /// inserted text.
    pub track_changes: Option<TrackChanges>,
}

/// `FileHeader`'s signature, NUL-padded to 32 bytes.
const SIGNATURE: &[u8] = b"HWP Document File";
/// The HWP 3.x file's (not a compound file).
const SIGNATURE_V3: &[u8] = b"HWP Document File V3";
/// The HWPX package's `mimetype`.
const HWPX_MIME: &str = "application/hwp+zip";

/// Nesting cap for tables in cells and text boxes in text boxes — far past
/// any real document, a guard against a crafted one's recursion.
const MAX_DEPTH: usize = 64;
/// Grid cap per table (cells): docling-core's own per-table limit.
const MAX_TABLE_CELLS: usize = 1_000_000;

/// Whether a ZIP's `mimetype` member names an HWPX package.
pub(crate) fn is_hwpx_mime(mime: &str) -> bool {
    mime == HWPX_MIME
}

impl DeclarativeBackend for HwpBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let bytes = &source.bytes[..];
        let mut doc = DoclingDocument::new(&source.name);
        let policy = self.track_changes.unwrap_or_default();
        let refuse = |what: &str| {
            ConversionError::UnsupportedOption(format!(
                "hwp: track_changes={what} is not implemented by the {} reader",
                if what == "all" { "HWP" } else { "HWP 5" }
            ))
        };
        if policy == TrackChanges::All {
            return Err(refuse("all"));
        }
        if bytes.starts_with(b"PK\x03\x04") {
            hwpx(bytes, policy, &mut doc)?;
        } else if CompoundFile::detect(bytes) {
            if policy == TrackChanges::Reject {
                return Err(refuse("reject"));
            }
            hwp5(bytes, &mut doc)?;
        } else if bytes.starts_with(SIGNATURE_V3) {
            return Err(ConversionError::Parse(
                "hwp: an HWP 3.x document (Hangul 96/97) — only HWP 5 (Hangul 2002 on) and \
                 HWPX are read; re-save it from Hangul or Hancom Office"
                    .into(),
            ));
        } else {
            return Err(ConversionError::Parse(
                "hwp: neither an HWP 5 compound file nor an HWPX package".into(),
            ));
        }
        Ok(doc)
    }
}

// ------------------------------------------------------------------ items

/// What a paragraph is, from its paragraph shape's head.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Head {
    #[default]
    Body,
    /// An outline (`개요`) paragraph: a heading, `1..=6`.
    Heading(u8),
    List {
        ordered: bool,
        level: u8,
    },
}

impl Head {
    /// The head of a paragraph shape: its kind (0 none, 1 outline,
    /// 2 number, 3 bullet) and its 0-based level.
    fn from_kind(kind: u32, level: u32) -> Self {
        let level = level.min(9) as u8;
        match kind {
            1 => Head::Heading((level + 1).min(6)),
            2 | 3 => Head::List {
                ordered: kind == 2,
                level,
            },
            _ => Head::Body,
        }
    }
}

/// One block of a paragraph list, in reading order.
enum Item {
    Para(Para, Head),
    Node(Box<Node>),
}

/// The plain text of a paragraph list — a table cell, a caption, a
/// header: paragraphs and nested tables' cells joined by spaces.
fn plain_text(items: &[Item]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for item in items {
        match item {
            Item::Para(p, _) => parts.push(p.text()),
            Item::Node(n) => {
                if let Node::Table(t) = &**n {
                    for row in &t.rows {
                        parts.extend(row.iter().cloned());
                    }
                }
            }
        }
    }
    parts.retain(|p| !p.trim().is_empty());
    parts.join(" ")
}

/// Append `items` to the document: paragraphs, headings and list items
/// (numbered per level, the count reset by any other block).
fn emit(items: Vec<Item>, doc: &mut DoclingDocument) {
    let mut numbers: Vec<usize> = Vec::new();
    for item in items {
        match item {
            // An empty paragraph does not break a list: Hangul spaces
            // lists with them.
            Item::Para(p, _) if p.is_empty() => {}
            Item::Para(p, Head::Body) => {
                numbers.clear();
                if let Some(node) = p.into_node() {
                    doc.push(node);
                }
            }
            Item::Para(p, Head::Heading(level)) => {
                numbers.clear();
                doc.push(Node::Heading {
                    level,
                    text: escape_text(&p.text()),
                });
            }
            Item::Para(p, Head::List { ordered, level }) => {
                let level = level as usize;
                numbers.truncate(level + 1);
                numbers.resize(level + 1, 0);
                numbers[level] += 1;
                let first = !matches!(doc.nodes.last(), Some(Node::ListItem { .. }));
                doc.push(Node::ListItem {
                    ordered,
                    number: numbers[level] as u64,
                    first_in_list: first,
                    text: p.markdown(),
                    level: level as u8,
                    marker: None,
                    location: None,
                    split: None,
                    dclx: None,
                    href: None,
                    layer: None,
                });
            }
            Item::Node(node) => {
                numbers.clear();
                doc.push(*node);
            }
        }
    }
}

/// A furniture item per non-empty paragraph of `items`.
fn furniture(label: &str, items: &[Item], out: &mut Vec<Node>) {
    for item in items {
        let text = match item {
            Item::Para(p, _) => p.text(),
            Item::Node(n) if matches!(**n, Node::Table(_)) => {
                plain_text(std::slice::from_ref(item))
            }
            Item::Node(_) => continue,
        };
        if !text.trim().is_empty() {
            out.push(Node::FurnitureText {
                label: label.into(),
                text: escape_text(&text),
            });
        }
    }
}

/// A table from its cells (anchor position, spans, text): the dense grid
/// with each spanning cell's text over the positions it covers, and the
/// cells themselves for the JSON/DocLang structure.
/// A cell: row, column, row span, column span, text.
type Cell = (usize, usize, usize, usize, String);

fn build_table(
    rows: usize,
    cols: usize,
    cells: Vec<Cell>,
    caption: Option<String>,
) -> Option<Node> {
    // The grid the cells reach — anchors, then spans, up to the declared
    // size: a declared size alone (a crafted 65535 × 65535) builds nothing.
    let extent = |declared: usize, anchor: fn(&Cell) -> usize, span: fn(&Cell) -> usize| {
        let anchored = cells.iter().map(|c| anchor(c) + 1).max().unwrap_or(0);
        let spanned = cells
            .iter()
            .map(|c| anchor(c).saturating_add(span(c).max(1)))
            .max()
            .unwrap_or(0);
        anchored.max(spanned.min(declared))
    };
    let rows = extent(rows, |c| c.0, |c| c.2);
    let cols = extent(cols, |c| c.1, |c| c.3);
    if rows == 0 || cols == 0 || rows.saturating_mul(cols) > MAX_TABLE_CELLS {
        return None;
    }
    let mut grid = vec![vec![String::new(); cols]; rows];
    let mut out = Vec::with_capacity(cells.len());
    for (row, col, row_span, col_span, text) in cells {
        let row_span = row_span.clamp(1, rows - row);
        let col_span = col_span.clamp(1, cols - col);
        for r in grid.iter_mut().skip(row).take(row_span) {
            for c in r.iter_mut().skip(col).take(col_span) {
                c.clone_from(&text);
            }
        }
        out.push(TableCell {
            text,
            bbox: None,
            start_row: row,
            start_col: col,
            row_span,
            col_span,
            column_header: row == 0,
            row_header: false,
            row_section: false,
        });
    }
    Some(Node::Table(Table {
        rows: grid,
        cells: Some(out),
        caption: caption.filter(|c| !c.is_empty()).map(|c| escape_text(&c)),
        ..Default::default()
    }))
}

/// The embedded images of one document, decoded once each and handed to
/// every picture that shows them — within a byte budget: a document that
/// places one large photo thousands of times (hwplib's `big_file.hwp`, a
/// 3.9 MB JPEG behind 3000 pictures) would otherwise hold a copy per
/// picture. Past the budget (the per-part limit, `DOCLING_RS_MAX_PART_BYTES`)
/// a picture keeps its place without its payload, as docling keeps a
/// picture it cannot decode.
struct Images<K> {
    decoded: HashMap<K, Option<PictureImage>>,
    budget: u64,
}

impl<K: std::hash::Hash + Eq> Images<K> {
    fn new() -> Self {
        Images {
            decoded: HashMap::new(),
            budget: ooxml::max_part_bytes(),
        }
    }

    fn get(
        &mut self,
        key: K,
        decode: impl FnOnce() -> Option<PictureImage>,
    ) -> Option<PictureImage> {
        let image = self.decoded.entry(key).or_insert_with(decode).as_ref()?;
        let len = image.data.len() as u64;
        self.budget = self.budget.checked_sub(len)?;
        Some(image.clone())
    }
}

/// Ruby text (덧말) after its base, in parentheses — the plain-text form
/// of `<ruby>`.
fn push_ruby(para: &mut Para, main: &str, sub: Option<&str>) {
    para.push(main, Style::default());
    if let Some(sub) = sub.map(str::trim).filter(|s| !s.is_empty()) {
        para.push(&format!(" ({sub})"), Style::default());
    }
}

fn picture(image: Option<PictureImage>) -> Node {
    Node::Picture {
        caption: None,
        caption_href: None,
        image,
        classification: None,
        description: None,
        caption_parent: Default::default(),
        caption_location: None,
    }
}

// ------------------------------------------------------------------ HWP 5

fn u16_at(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(o..o + 2)?.try_into().ok()?))
}

fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
}

/// A length-prefixed UTF-16LE string (`WCHAR[len]` after a `u16` count).
fn wstring_at(d: &[u8], o: usize) -> Option<String> {
    let len = u16_at(d, o)? as usize;
    let units: Vec<u16> = d
        .get(o + 2..o + 2 + len * 2)?
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some(String::from_utf16_lossy(&units))
}

const fn ctrl_id(s: &[u8; 4]) -> u32 {
    (s[0] as u32) << 24 | (s[1] as u32) << 16 | (s[2] as u32) << 8 | s[3] as u32
}

// Record tags (`HWPTAG_BEGIN` = 0x10).
const TAG_BIN_DATA: u16 = 0x10 + 2;
const TAG_CHAR_SHAPE: u16 = 0x10 + 5;
const TAG_PARA_SHAPE: u16 = 0x10 + 9;
const TAG_PARA_HEADER: u16 = 0x10 + 50;
const TAG_PARA_TEXT: u16 = 0x10 + 51;
const TAG_PARA_CHAR_SHAPE: u16 = 0x10 + 52;
const TAG_CTRL_HEADER: u16 = 0x10 + 55;
const TAG_LIST_HEADER: u16 = 0x10 + 56;
const TAG_TABLE: u16 = 0x10 + 61;
const TAG_SHAPE_PICTURE: u16 = 0x10 + 69;
const TAG_EQEDIT: u16 = 0x10 + 72;

// Control ids (`CTRL_HEADER`'s first four bytes).
const CTRL_TABLE: u32 = ctrl_id(b"tbl ");
const CTRL_GSO: u32 = ctrl_id(b"gso ");
const CTRL_EQUATION: u32 = ctrl_id(b"eqed");
const CTRL_HEADER: u32 = ctrl_id(b"head");
const CTRL_FOOTER: u32 = ctrl_id(b"foot");
const CTRL_FOOTNOTE: u32 = ctrl_id(b"fn  ");
const CTRL_ENDNOTE: u32 = ctrl_id(b"en  ");
const CTRL_RUBY: u32 = ctrl_id(b"tdut");
const CTRL_COMPOSE: u32 = ctrl_id(b"tcps");

/// One record: tag, level and where its payload sits in the stream.
#[derive(Debug, Clone, Copy)]
struct Rec {
    tag: u16,
    level: u16,
    start: usize,
    end: usize,
}

/// A stream's records; a truncated last record keeps what is there.
fn records(d: &[u8]) -> Vec<Rec> {
    let mut out = Vec::new();
    let mut o = 0;
    while let Some(h) = u32_at(d, o) {
        o += 4;
        let mut size = (h >> 20) as usize;
        if size == 0xfff {
            let Some(s) = u32_at(d, o) else { break };
            size = s as usize;
            o += 4;
        }
        let end = o.saturating_add(size).min(d.len());
        out.push(Rec {
            tag: (h & 0x3ff) as u16,
            level: ((h >> 10) & 0x3ff) as u16,
            start: o,
            end,
        });
        o = end;
    }
    out
}

/// A raw-deflated stream, capped at the per-part budget.
fn inflate(raw: &[u8], what: &str) -> Result<Vec<u8>, ConversionError> {
    let cap = ooxml::max_part_bytes();
    let mut out = Vec::new();
    flate2::read::DeflateDecoder::new(raw)
        .take(cap + 1)
        .read_to_end(&mut out)
        .map_err(|_| ConversionError::Parse(format!("hwp: could not decompress {what}")))?;
    if out.len() as u64 > cap {
        return Err(ConversionError::Parse(format!(
            "hwp: {what} expands beyond the {cap} byte limit"
        )));
    }
    Ok(out)
}

/// One `BIN_DATA` entry: the `BinData/BIN%04X.ext` stream an embedded
/// picture lives in, and whether it is deflated.
struct BinItem {
    stream: String,
    compressed: bool,
}

/// What the body needs from `DocInfo`: the character and paragraph shapes
/// by id, the embedded binaries by their 1-based index.
#[derive(Default)]
struct DocInfo {
    char_styles: Vec<Style>,
    heads: Vec<Head>,
    bins: Vec<Option<BinItem>>,
}

impl DocInfo {
    fn parse(d: &[u8], compressed: bool) -> Self {
        let mut info = DocInfo::default();
        for r in records(d) {
            let body = &d[r.start..r.end];
            match r.tag {
                TAG_CHAR_SHAPE => {
                    // 7 font ids (u16), 4 × 7 per-language bytes, the base
                    // size (i32), then the property bits.
                    let p = u32_at(body, 46).unwrap_or(0);
                    info.char_styles.push(Style {
                        italic: p & 1 != 0,
                        bold: p & 2 != 0,
                        underline: (p >> 2) & 3 == 1,
                        strike: (p >> 18) & 7 != 0,
                        script: if p & (1 << 15) != 0 {
                            Script::Super
                        } else if p & (1 << 16) != 0 {
                            Script::Sub
                        } else {
                            Script::Baseline
                        },
                    });
                }
                TAG_PARA_SHAPE => {
                    let p = u32_at(body, 0).unwrap_or(0);
                    info.heads
                        .push(Head::from_kind((p >> 23) & 3, (p >> 25) & 7));
                }
                TAG_BIN_DATA => {
                    let props = u16_at(body, 0).unwrap_or(0);
                    // Type 1 = embedded: its id, then the extension.
                    let item = (props & 0xf == 1)
                        .then(|| {
                            let id = u16_at(body, 2)?;
                            let ext = wstring_at(body, 4)?;
                            Some(BinItem {
                                stream: format!("BIN{id:04X}.{ext}"),
                                compressed: match (props >> 4) & 3 {
                                    1 => true,
                                    2 => false,
                                    _ => compressed,
                                },
                            })
                        })
                        .flatten();
                    info.bins.push(item);
                }
                _ => {}
            }
        }
        info
    }

    fn style(&self, id: u32) -> Style {
        self.char_styles
            .get(id as usize)
            .copied()
            .unwrap_or_default()
    }

    fn head(&self, id: u16) -> Head {
        self.heads.get(id as usize).copied().unwrap_or_default()
    }
}

fn hwp5(bytes: &[u8], doc: &mut DoclingDocument) -> Result<(), ConversionError> {
    let cfb = CompoundFile::open(bytes)
        .ok_or_else(|| ConversionError::Parse(CompoundFile::open_error("hwp", bytes)))?;
    let header = cfb
        .stream("FileHeader")
        .ok_or_else(|| ConversionError::Parse("hwp: no FileHeader stream".into()))?;
    if !header.starts_with(SIGNATURE) {
        return Err(ConversionError::Parse(
            "hwp: the FileHeader is not an HWP document's".into(),
        ));
    }
    let flags = u32_at(&header, 36).unwrap_or(0);
    if flags & 2 != 0 {
        return Err(crate::backend::offcrypto::encrypted("HWP"));
    }
    if flags & 4 != 0 {
        return Err(ConversionError::Parse(
            "hwp: a distribution document (배포용 문서) — its text is stored encrypted for \
             viewing in Hangul only"
                .into(),
        ));
    }
    let compressed = flags & 1 != 0;
    let read = |idx: usize, what: &str| -> Result<Vec<u8>, ConversionError> {
        let raw = cfb
            .stream_by_index(idx)
            .ok_or_else(|| ConversionError::Parse(cfb.stream_error("hwp", what)))?;
        if compressed {
            inflate(&raw, what)
        } else {
            Ok(raw)
        }
    };
    let info = match cfb.root_stream("DocInfo") {
        Some(idx) => DocInfo::parse(&read(idx, "DocInfo")?, compressed),
        None => DocInfo::default(),
    };
    let body = cfb
        .children_of(None)
        .into_iter()
        .find(|&i| cfb.is_storage(i) && cfb.entry_name(i) == "BodyText")
        .ok_or_else(|| ConversionError::Parse("hwp: no BodyText storage".into()))?;
    let mut sections: Vec<(u32, usize)> = cfb
        .children_of(Some(body))
        .into_iter()
        .filter_map(|i| {
            let n = cfb.entry_name(i).strip_prefix("Section")?.parse().ok()?;
            Some((n, i))
        })
        .collect();
    sections.sort_unstable();

    let mut walker = Hwp5 {
        info: &info,
        cfb: &cfb,
        data: Vec::new(),
        recs: Vec::new(),
        furniture: Vec::new(),
        anchored: Vec::new(),
        images: Images::new(),
        depth: 0,
    };
    let mut items = Vec::new();
    for (n, idx) in sections {
        walker.data = read(idx, &format!("BodyText/Section{n}"))?;
        walker.recs = records(&walker.data);
        let mut i = 0;
        while i < walker.recs.len() {
            i = if walker.recs[i].tag == TAG_PARA_HEADER {
                walker.para(i, &mut items)
            } else {
                i + 1
            };
        }
    }
    let furniture = std::mem::take(&mut walker.furniture);
    emit(items, doc);
    doc.nodes.extend(furniture);
    Ok(())
}

/// The image of `BIN_DATA` entry `idx` (1-based), the index a
/// `SHAPE_COMPONENT_PICTURE` holds after its border (12 bytes), corners
/// (32), crop (16), padding (8) and brightness/contrast/effect bytes.
fn bin_image(info: &DocInfo, cfb: &CompoundFile, idx: usize) -> Option<PictureImage> {
    let bin = info.bins.get(idx.checked_sub(1)?)?.as_ref()?;
    // Writers differ in the extension's case (`BIN0001.PNG`).
    let name = cfb
        .stream_names()
        .find(|n| n.eq_ignore_ascii_case(&bin.stream))?
        .to_string();
    let raw = cfb.stream(&name)?;
    // An image format that compresses itself (JPEG, PNG) is often stored
    // as is under a document-wide compression flag.
    let data = match bin.compressed {
        true => inflate(&raw, &bin.stream).unwrap_or(raw),
        false => raw,
    };
    ooxml::picture_image(&bin.stream.to_ascii_lowercase(), data)
}

struct Hwp5<'a> {
    info: &'a DocInfo,
    cfb: &'a CompoundFile<'a>,
    /// The current section's bytes and records.
    data: Vec<u8>,
    recs: Vec<Rec>,
    furniture: Vec<Node>,
    /// The drawing objects anchored in the paragraph being read.
    anchored: Vec<Item>,
    images: Images<usize>,
    depth: usize,
}

impl Hwp5<'_> {
    fn body(&self, i: usize) -> &[u8] {
        let r = self.recs[i];
        &self.data[r.start..r.end]
    }

    /// The index past record `i`'s subtree (the records nested under it).
    fn subtree_end(&self, i: usize) -> usize {
        let level = self.recs[i].level;
        let mut j = i + 1;
        while j < self.recs.len() && self.recs[j].level > level {
            j += 1;
        }
        j
    }

    /// The paragraph list starting at `i` on `level` (a cell's, a text
    /// box's, a note's): its paragraphs up to the one flagged last.
    fn paras(&mut self, mut i: usize, level: u16, out: &mut Vec<Item>) -> usize {
        while i < self.recs.len()
            && self.recs[i].tag == TAG_PARA_HEADER
            && self.recs[i].level == level
        {
            let last = u32_at(self.body(i), 0).unwrap_or(0) & 0x8000_0000 != 0;
            i = self.para(i, out);
            if last {
                break;
            }
        }
        i
    }

    /// A paragraph: its text with the character shapes, and in place of
    /// each extended control the object it stands for.
    fn para(&mut self, i: usize, out: &mut Vec<Item>) -> usize {
        let level = self.recs[i].level;
        let end = self.subtree_end(i);
        let info = self.info;
        let head = info.head(u16_at(self.body(i), 8).unwrap_or(0));
        let mut text: &[u8] = &[];
        let mut shapes: Vec<(u32, u32)> = Vec::new();
        let mut ctrls: Vec<usize> = Vec::new();
        for k in i + 1..end {
            if self.recs[k].level != level + 1 {
                continue;
            }
            match self.recs[k].tag {
                TAG_PARA_TEXT => text = self.body(k),
                TAG_PARA_CHAR_SHAPE => {
                    shapes = self
                        .body(k)
                        .chunks_exact(8)
                        .map(|c| {
                            (
                                u32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                                u32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                            )
                        })
                        .collect()
                }
                TAG_CTRL_HEADER => ctrls.push(k),
                _ => {}
            }
        }
        let units: Vec<u16> = text
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let style_at = |pos: usize| {
            let id = shapes
                .iter()
                .take_while(|(p, _)| *p as usize <= pos)
                .last()
                .map_or(0, |s| s.1);
            info.style(id)
        };
        let mut para = Para::new();
        let outer = std::mem::take(&mut self.anchored);
        let mut ctrls = ctrls.into_iter();
        let mut k = 0;
        while k < units.len() {
            let u = units[k];
            match u {
                // Extended controls: each the next CTRL_HEADER.
                1..=3 | 11 | 12 | 14..=18 | 21..=23 => {
                    if let Some(c) = ctrls.next() {
                        self.control(c, &mut para, head, out);
                    }
                    k += 8;
                }
                // Inline controls: a tab, a field end, a title mark.
                4..=9 | 19 | 20 => {
                    if u == 9 {
                        para.space();
                    }
                    k += 8;
                }
                13 => break,
                24 => {
                    para.push_char('-', style_at(k));
                    k += 1;
                }
                // A line break, a bound or fixed-width space.
                10 | 30 | 31 => {
                    para.space();
                    k += 1;
                }
                0..=31 => k += 1,
                0xd800..=0xdbff => {
                    let lo = units.get(k + 1).copied().unwrap_or(0);
                    let c = char::decode_utf16([u, lo])
                        .next()
                        .and_then(Result::ok)
                        .unwrap_or('\u{fffd}');
                    para.push_char(c, style_at(k));
                    k += 2;
                }
                _ => {
                    let c = char::from_u32(u as u32).unwrap_or('\u{fffd}');
                    para.push_char(c, style_at(k));
                    k += 1;
                }
            }
        }
        out.push(Item::Para(para, head));
        out.append(&mut self.anchored);
        self.anchored = outer;
        end
    }

    /// The object a control stands for, at its place in the paragraph.
    fn control(&mut self, c: usize, para: &mut Para, head: Head, out: &mut Vec<Item>) {
        if self.depth >= MAX_DEPTH {
            return;
        }
        self.depth += 1;
        let flush = |para: &mut Para, out: &mut Vec<Item>| {
            if !para.is_empty() {
                out.push(Item::Para(para.take(), head));
            }
        };
        match u32_at(self.body(c), 0).unwrap_or(0) {
            CTRL_TABLE => {
                flush(para, out);
                if let Some(t) = self.table(c) {
                    out.push(Item::Node(Box::new(t)));
                }
            }
            // A drawing object floats beside the text: its text boxes and
            // pictures follow the paragraph that anchors it.
            CTRL_GSO => {
                let mut items = Vec::new();
                self.sublists(c, &mut items);
                self.anchored.extend(items);
            }
            CTRL_EQUATION => {
                let end = self.subtree_end(c);
                if let Some(script) = (c + 1..end)
                    .find(|&k| self.recs[k].tag == TAG_EQEDIT)
                    .and_then(|k| wstring_at(self.body(k), 4))
                {
                    para.push(&script, Style::default());
                }
            }
            // 덧말: the base text, then the ruby text (two strings after
            // the id).
            CTRL_RUBY => {
                let b = self.body(c);
                if let Some(main) = wstring_at(b, 4) {
                    let sub = wstring_at(b, 6 + 2 * main.encode_utf16().count());
                    push_ruby(para, &main, sub.as_deref());
                }
            }
            // 글자 겹치기: the characters set over one another.
            CTRL_COMPOSE => {
                if let Some(text) = wstring_at(self.body(c), 4) {
                    para.push(&text, Style::default());
                }
            }
            id @ (CTRL_HEADER | CTRL_FOOTER | CTRL_FOOTNOTE | CTRL_ENDNOTE) => {
                let mut items = Vec::new();
                self.sublists(c, &mut items);
                let label = match id {
                    CTRL_HEADER => "page_header",
                    CTRL_FOOTER => "page_footer",
                    _ => "footnote",
                };
                let mut nodes = Vec::new();
                furniture(label, &items, &mut nodes);
                self.furniture.extend(nodes);
            }
            _ => {}
        }
        self.depth -= 1;
    }

    /// Every paragraph list in a control's subtree (a text box's, a
    /// note's, a header's), and its pictures, in order.
    fn sublists(&mut self, c: usize, out: &mut Vec<Item>) {
        let end = self.subtree_end(c);
        let mut k = c + 1;
        while k < end {
            match self.recs[k].tag {
                TAG_LIST_HEADER => {
                    let level = self.recs[k].level;
                    k = self.paras(k + 1, level, out);
                    continue;
                }
                TAG_SHAPE_PICTURE => {
                    let image = u16_at(self.body(k), 71).and_then(|idx| {
                        let (info, cfb) = (self.info, self.cfb);
                        self.images
                            .get(idx as usize, || bin_image(info, cfb, idx as usize))
                    });
                    out.push(Item::Node(Box::new(picture(image))));
                }
                _ => {}
            }
            k += 1;
        }
    }

    /// A table control: an optional caption list before the `TABLE`
    /// record, one list per cell after it.
    fn table(&mut self, c: usize) -> Option<Node> {
        let end = self.subtree_end(c);
        let level = self.recs[c].level + 1;
        let (mut rows, mut cols) = (0usize, 0usize);
        let mut seen_table = false;
        let mut caption = None;
        let mut cells = Vec::new();
        let mut k = c + 1;
        while k < end {
            let r = self.recs[k];
            if r.level != level {
                k += 1;
                continue;
            }
            match r.tag {
                TAG_TABLE => {
                    let b = self.body(k);
                    rows = u16_at(b, 4).unwrap_or(0) as usize;
                    cols = u16_at(b, 6).unwrap_or(0) as usize;
                    seen_table = true;
                }
                TAG_LIST_HEADER => {
                    // The paragraph count and the list properties (u32
                    // each — the published spec's INT16 count is padded),
                    // then the cell's column, row, column span, row span.
                    let b = self.body(k);
                    let at = |o| u16_at(b, o).unwrap_or(0) as usize;
                    let (col, row, col_span, row_span) = (at(8), at(10), at(12), at(14));
                    let mut items = Vec::new();
                    let next = self.paras(k + 1, level, &mut items);
                    if seen_table {
                        cells.push((row, col, row_span, col_span, plain_text(&items)));
                    } else {
                        caption = Some(plain_text(&items));
                    }
                    k = next;
                    continue;
                }
                _ => {}
            }
            k += 1;
        }
        build_table(rows, cols, cells, caption)
    }
}

// ------------------------------------------------------------------ HWPX

fn hwpx(
    bytes: &[u8],
    policy: TrackChanges,
    doc: &mut DoclingDocument,
) -> Result<(), ConversionError> {
    let mut pkg = Package::open_slice(bytes)
        .ok_or_else(|| ConversionError::Parse("hwpx: not a readable ZIP package".into()))?;
    if pkg.any_encrypted() {
        return Err(crate::backend::offcrypto::encrypted("HWPX"));
    }
    let mut styles = HwpxStyles::default();
    if let Some(xml) = pkg.read("Contents/header.xml") {
        crate::backend::xml_depth::check(&xml, "hwpx header")?;
        if let Ok(dom) = roxmltree::Document::parse(&xml) {
            styles = HwpxStyles::parse(&dom);
        }
    }
    let mut bins = HashMap::new();
    if let Some(xml) = pkg.read("Contents/content.hpf") {
        if let Ok(dom) = roxmltree::Document::parse(&xml) {
            for item in dom.descendants().filter(|n| n.has_tag_name("item")) {
                if let (Some(id), Some(href)) = (item.attribute("id"), item.attribute("href")) {
                    bins.insert(id.to_string(), href.to_string());
                }
            }
        }
    }
    let mut sections: Vec<(u32, String)> = pkg
        .names()
        .filter_map(|n| {
            let num = n
                .strip_prefix("Contents/section")?
                .strip_suffix(".xml")?
                .parse()
                .ok()?;
            Some((num, n.to_string()))
        })
        .collect();
    sections.sort_unstable();
    if sections.is_empty() {
        return Err(ConversionError::Parse(
            "hwpx: no Contents/section*.xml part".into(),
        ));
    }
    let mut walker = Hwpx {
        styles: &styles,
        bins: &bins,
        pkg: &mut pkg,
        furniture: Vec::new(),
        images: Images::new(),
        anchored: Vec::new(),
        depth: 0,
        policy,
        deleted: false,
        inserted: false,
    };
    let mut items = Vec::new();
    for (_, name) in sections {
        let xml = walker
            .pkg
            .read(&name)
            .ok_or_else(|| ConversionError::Parse(format!("hwpx: '{name}' unreadable")))?;
        crate::backend::xml_depth::check(&xml, "hwpx section")?;
        let dom = roxmltree::Document::parse(&xml)
            .map_err(|e| ConversionError::Parse(format!("hwpx: '{name}': {e}")))?;
        walker.paras(dom.root_element(), &mut items);
    }
    let furniture = std::mem::take(&mut walker.furniture);
    emit(items, doc);
    doc.nodes.extend(furniture);
    Ok(())
}

/// `header.xml`'s character and paragraph properties by id.
#[derive(Default)]
struct HwpxStyles {
    chars: HashMap<String, Style>,
    heads: HashMap<String, Head>,
}

impl HwpxStyles {
    fn parse(dom: &roxmltree::Document) -> Self {
        let mut out = HwpxStyles::default();
        for n in dom.descendants().filter(XmlNode::is_element) {
            let Some(id) = n.attribute("id") else {
                continue;
            };
            match n.tag_name().name() {
                "charPr" => {
                    let child = |name: &str| n.children().find(|c| c.has_tag_name(name));
                    let shown = |name: &str, attr: &str| {
                        child(name)
                            .and_then(|c| c.attribute(attr))
                            .is_some_and(|v| v != "NONE")
                    };
                    out.chars.insert(
                        id.to_string(),
                        Style {
                            bold: child("bold").is_some(),
                            italic: child("italic").is_some(),
                            underline: shown("underline", "type"),
                            strike: shown("strikeout", "shape"),
                            script: if child("supscript").is_some() {
                                Script::Super
                            } else if child("subscript").is_some() {
                                Script::Sub
                            } else {
                                Script::Baseline
                            },
                        },
                    );
                }
                "paraPr" => {
                    if let Some(h) = n.descendants().find(|c| c.has_tag_name("heading")) {
                        let kind = match h.attribute("type") {
                            Some("OUTLINE") => 1,
                            Some("NUMBER") => 2,
                            Some("BULLET") => 3,
                            _ => 0,
                        };
                        let level = h.attribute("level").and_then(|l| l.parse().ok());
                        out.heads
                            .insert(id.to_string(), Head::from_kind(kind, level.unwrap_or(0)));
                    }
                }
                _ => {}
            }
        }
        out
    }
}

struct Hwpx<'a, 'b> {
    styles: &'a HwpxStyles,
    bins: &'a HashMap<String, String>,
    pkg: &'a mut Package<&'b [u8]>,
    furniture: Vec<Node>,
    images: Images<String>,
    /// The drawing objects anchored in the paragraph being read.
    anchored: Vec<Item>,
    depth: usize,
    policy: TrackChanges,
    /// Inside a tracked deletion / insertion: the begin and end marks sit
    /// in the text and may span runs and paragraphs.
    deleted: bool,
    inserted: bool,
}

fn child<'a, 'i>(n: XmlNode<'a, 'i>, name: &str) -> Option<XmlNode<'a, 'i>> {
    n.children().find(|c| c.has_tag_name(name))
}

impl Hwpx<'_, '_> {
    /// The `hp:p` children of a section or a `hp:subList`.
    fn paras(&mut self, parent: XmlNode, out: &mut Vec<Item>) {
        for p in parent.children().filter(|c| c.has_tag_name("p")) {
            self.para(p, out);
        }
    }

    fn sublist(&mut self, parent: XmlNode, out: &mut Vec<Item>) {
        if let Some(list) = child(parent, "subList") {
            self.paras(list, out);
        }
    }

    fn para(&mut self, p: XmlNode, out: &mut Vec<Item>) {
        if self.depth >= MAX_DEPTH {
            return;
        }
        self.depth += 1;
        let head = p
            .attribute("paraPrIDRef")
            .and_then(|id| self.styles.heads.get(id))
            .copied()
            .unwrap_or_default();
        let mut para = Para::new();
        let outer = std::mem::take(&mut self.anchored);
        for run in p.children().filter(|c| c.has_tag_name("run")) {
            let style = run
                .attribute("charPrIDRef")
                .and_then(|id| self.styles.chars.get(id))
                .copied()
                .unwrap_or_default();
            for el in run.children().filter(XmlNode::is_element) {
                self.inline(el, style, head, &mut para, out);
            }
        }
        out.push(Item::Para(para, head));
        out.append(&mut self.anchored);
        self.anchored = outer;
        self.depth -= 1;
    }

    fn inline(
        &mut self,
        el: XmlNode,
        style: Style,
        head: Head,
        para: &mut Para,
        out: &mut Vec<Item>,
    ) {
        let flush = |para: &mut Para, out: &mut Vec<Item>| {
            if !para.is_empty() {
                out.push(Item::Para(para.take(), head));
            }
        };
        match el.tag_name().name() {
            "t" => {
                for n in el.children() {
                    let hidden = match self.policy {
                        TrackChanges::Reject => self.inserted,
                        _ => self.deleted,
                    };
                    if let Some(text) = n.text().filter(|_| n.is_text()) {
                        if !hidden {
                            para.push(text, style);
                        }
                        continue;
                    }
                    match n.tag_name().name() {
                        "deleteBegin" => self.deleted = true,
                        "deleteEnd" => self.deleted = false,
                        "insertBegin" => self.inserted = true,
                        "insertEnd" => self.inserted = false,
                        _ if hidden => {}
                        "tab" | "lineBreak" | "nbSpace" | "fwSpace" => para.space(),
                        "hyphen" => para.push_char('-', style),
                        // Highlighting and the like wrap text of their own.
                        _ => {
                            if let Some(text) = n.text() {
                                para.push(text, style);
                            }
                        }
                    }
                }
            }
            "dutmal" => {
                let text = |name| child(el, name).and_then(|n| n.text());
                if let Some(main) = text("mainText") {
                    push_ruby(para, main, text("subText"));
                }
            }
            "compose" => {
                if let Some(text) = el.attribute("composeText") {
                    para.push(text, style);
                }
            }
            "tbl" => {
                flush(para, out);
                if let Some(t) = self.table(el) {
                    out.push(Item::Node(Box::new(t)));
                }
            }
            "ctrl" => {
                for c in el.children().filter(XmlNode::is_element) {
                    let label = match c.tag_name().name() {
                        "header" => "page_header",
                        "footer" => "page_footer",
                        "footNote" | "endNote" => "footnote",
                        _ => continue,
                    };
                    let mut items = Vec::new();
                    self.sublist(c, &mut items);
                    furniture(label, &items, &mut self.furniture);
                }
            }
            "equation" => {
                if let Some(script) = child(el, "script").and_then(|s| s.text()) {
                    para.push(script, Style::default());
                }
            }
            "switch" => {
                // A compatibility switch: the default branch is the
                // content every reader understands.
                if let Some(branch) = child(el, "default") {
                    for c in branch.children().filter(XmlNode::is_element) {
                        self.inline(c, style, head, para, out);
                    }
                }
            }
            "pic" | "container" | "rect" | "ellipse" | "arc" | "polygon" | "curve"
            | "connectLine" | "textart" | "ole" => {
                // A drawing object floats beside the text: it follows
                // the paragraph that anchors it.
                let mut items = Vec::new();
                self.shape(el, &mut items);
                self.anchored.extend(items);
            }
            _ => {}
        }
    }

    /// A drawing object: a picture's image, a shape's text box, a group's
    /// members.
    fn shape(&mut self, el: XmlNode, out: &mut Vec<Item>) {
        if self.depth >= MAX_DEPTH {
            return;
        }
        self.depth += 1;
        match el.tag_name().name() {
            "pic" => {
                let href = el
                    .descendants()
                    .find(|c| c.has_tag_name("img"))
                    .and_then(|img| img.attribute("binaryItemIDRef"))
                    .and_then(|id| self.bins.get(id));
                let image = href.and_then(|href| {
                    let pkg = &mut *self.pkg;
                    self.images.get(href.clone(), || {
                        let data = pkg.read_bytes(href)?;
                        ooxml::picture_image(&href.to_ascii_lowercase(), data)
                    })
                });
                out.push(Item::Node(Box::new(picture(image))));
            }
            "container" => {
                for c in el.children().filter(XmlNode::is_element) {
                    self.shape(c, out);
                }
            }
            // Text art: its text is an attribute, lines split by CR LF
            // (written out as `␍␊`, the control pictures).
            "textart" => {
                let mut para = Para::new();
                let text = el.attribute("text").unwrap_or("");
                para.push(
                    &text.replace(['\u{240d}', '\u{240a}'], " "),
                    Style::default(),
                );
                out.push(Item::Para(para, Head::Body));
            }
            _ => {
                if let Some(text) = child(el, "drawText") {
                    self.sublist(text, out);
                }
            }
        }
        self.depth -= 1;
    }

    fn table(&mut self, tbl: XmlNode) -> Option<Node> {
        if self.depth >= MAX_DEPTH {
            return None;
        }
        self.depth += 1;
        let num = |n: XmlNode, a: &str| {
            n.attribute(a)
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0)
        };
        let caption = child(tbl, "caption").map(|c| {
            let mut items = Vec::new();
            self.sublist(c, &mut items);
            plain_text(&items)
        });
        let mut cells = Vec::new();
        for (r, tr) in tbl.children().filter(|c| c.has_tag_name("tr")).enumerate() {
            for (c, tc) in tr.children().filter(|c| c.has_tag_name("tc")).enumerate() {
                let (row, col) = child(tc, "cellAddr")
                    .map_or((r, c), |a| (num(a, "rowAddr"), num(a, "colAddr")));
                let (rs, cs) = child(tc, "cellSpan")
                    .map_or((1, 1), |s| (num(s, "rowSpan"), num(s, "colSpan")));
                let mut items = Vec::new();
                self.sublist(tc, &mut items);
                cells.push((row, col, rs, cs, plain_text(&items)));
            }
        }
        self.depth -= 1;
        build_table(num(tbl, "rowCnt"), num(tbl, "colCnt"), cells, caption)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_header_packs_tag_level_and_size() {
        // PARA_TEXT (67) on level 1 with 4 bytes, then an extended-size
        // record (0xFFF) of 2 bytes.
        let h1: u32 = 67 | (1 << 10) | (4 << 20);
        let h2: u32 = 21 | (0xfff << 20);
        let mut d = h1.to_le_bytes().to_vec();
        d.extend_from_slice(b"a\0b\0");
        d.extend_from_slice(&h2.to_le_bytes());
        d.extend_from_slice(&2u32.to_le_bytes());
        d.extend_from_slice(b"xy");
        let recs = records(&d);
        assert_eq!(recs.len(), 2);
        assert_eq!((recs[0].tag, recs[0].level), (67, 1));
        assert_eq!(&d[recs[0].start..recs[0].end], b"a\0b\0");
        assert_eq!(
            (recs[1].tag, &d[recs[1].start..recs[1].end]),
            (21, &b"xy"[..])
        );
    }

    #[test]
    fn spans_fill_the_grid() {
        let Some(Node::Table(t)) = build_table(
            2,
            3,
            vec![
                (0, 0, 2, 2, "a".into()),
                (0, 2, 1, 1, "b".into()),
                (1, 2, 1, 1, "c".into()),
            ],
            None,
        ) else {
            panic!("no table");
        };
        assert_eq!(t.rows, vec![vec!["a", "a", "b"], vec!["a", "a", "c"]]);
        assert_eq!(t.cells.unwrap()[0].row_span, 2);
    }

    /// An HWPX package with `header.xml` declaring charPr 1 bold and
    /// paraPr 1 an outline level 0 (a heading), around `body` (`hp:p`s).
    fn hwpx_doc(
        body: &str,
        policy: Option<TrackChanges>,
    ) -> Result<DoclingDocument, ConversionError> {
        use std::io::Write;
        let header = r#"<hh:head xmlns:hh="h"><hh:charProperties>
            <hh:charPr id="0"/><hh:charPr id="1"><hh:bold/></hh:charPr></hh:charProperties>
            <hh:paraProperties><hh:paraPr id="0"><hh:heading type="NONE" level="0"/></hh:paraPr>
            <hh:paraPr id="1"><hh:heading type="OUTLINE" level="0"/></hh:paraPr>
            <hh:paraPr id="2"><hh:heading type="BULLET" level="0"/></hh:paraPr>
            </hh:paraProperties></hh:head>"#;
        let section = format!(r#"<hs:sec xmlns:hs="s" xmlns:hp="p">{body}</hs:sec>"#);
        let mut out = std::io::Cursor::new(Vec::new());
        let mut w = zip::ZipWriter::new(&mut out);
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in [
            ("mimetype", HWPX_MIME),
            ("Contents/header.xml", header),
            ("Contents/section0.xml", &section),
        ] {
            w.start_file(name, stored).unwrap();
            w.write_all(data.as_bytes()).unwrap();
        }
        w.finish().unwrap();
        HwpBackend {
            track_changes: policy,
        }
        .convert(&SourceDocument::from_bytes(
            "t.hwpx",
            crate::InputFormat::Hwp,
            out.into_inner(),
        ))
    }

    #[test]
    fn hwpx_reads_headings_styles_lists_tables_and_notes() {
        let doc = hwpx_doc(
            r#"<hp:p paraPrIDRef="1"><hp:run><hp:t>Title</hp:t></hp:run></hp:p>
            <hp:p paraPrIDRef="0"><hp:run><hp:t>plain </hp:t></hp:run>
              <hp:run charPrIDRef="1"><hp:t>bold</hp:t></hp:run>
              <hp:run><hp:ctrl><hp:footNote><hp:subList><hp:p><hp:run><hp:t>a note</hp:t>
              </hp:run></hp:p></hp:subList></hp:footNote></hp:ctrl></hp:run></hp:p>
            <hp:p paraPrIDRef="2"><hp:run><hp:t>item</hp:t></hp:run></hp:p>
            <hp:p><hp:run><hp:tbl rowCnt="2" colCnt="2">
              <hp:tr><hp:tc><hp:subList><hp:p><hp:run><hp:t>a</hp:t></hp:run></hp:p></hp:subList>
                <hp:cellAddr colAddr="0" rowAddr="0"/><hp:cellSpan colSpan="2" rowSpan="1"/></hp:tc></hp:tr>
              <hp:tr><hp:tc><hp:subList><hp:p><hp:run><hp:t>b</hp:t></hp:run></hp:p></hp:subList>
                <hp:cellAddr colAddr="0" rowAddr="1"/><hp:cellSpan colSpan="1" rowSpan="1"/></hp:tc>
                <hp:tc><hp:subList><hp:p><hp:run><hp:t>c</hp:t></hp:run></hp:p></hp:subList>
                <hp:cellAddr colAddr="1" rowAddr="1"/><hp:cellSpan colSpan="1" rowSpan="1"/></hp:tc></hp:tr>
            </hp:tbl></hp:run></hp:p>"#,
            None,
        )
        .unwrap();
        assert!(matches!(&doc.nodes[0], Node::Heading { level: 1, text } if text == "Title"));
        assert!(
            matches!(&doc.nodes[1], Node::InlineGroup { md_text, .. } if md_text == "plain **bold**")
        );
        assert!(
            matches!(&doc.nodes[2], Node::ListItem { text, ordered: false, .. } if text == "item")
        );
        let Node::Table(t) = &doc.nodes[3] else {
            panic!("no table: {:?}", doc.nodes[3]);
        };
        assert_eq!(t.rows, vec![vec!["a", "a"], vec!["b", "c"]]);
        assert!(matches!(
            &doc.nodes[4],
            Node::FurnitureText { label, text } if label == "footnote" && text == "a note"
        ));
    }

    #[test]
    fn hwpx_tracked_changes_follow_the_policy() {
        let body = r#"<hp:p><hp:run><hp:t>keep<hp:deleteBegin Id="1"/> gone<hp:deleteEnd Id="1"/>
            <hp:insertBegin Id="2"/>new<hp:insertEnd Id="2"/></hp:t></hp:run></hp:p>"#;
        let text = |doc: DoclingDocument| match &doc.nodes[0] {
            Node::Paragraph { text } => text.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(text(hwpx_doc(body, None).unwrap()), "keep new");
        assert_eq!(
            text(hwpx_doc(body, Some(TrackChanges::Reject)).unwrap()),
            "keep gone"
        );
        assert!(matches!(
            hwpx_doc(body, Some(TrackChanges::All)),
            Err(ConversionError::UnsupportedOption(_))
        ));
    }

    #[test]
    fn hwp3_and_unknown_bytes_are_refused_by_name() {
        let convert = |bytes: &[u8]| {
            HwpBackend::default().convert(&SourceDocument::from_bytes(
                "t.hwp",
                crate::InputFormat::Hwp,
                bytes.to_vec(),
            ))
        };
        let err = convert(b"HWP Document File V3.00 \x1a\x01\x02").unwrap_err();
        assert!(err.to_string().contains("HWP 3.x"), "{err}");
        assert!(convert(b"hello").is_err());
    }

    #[test]
    fn heads_map_to_headings_and_lists() {
        assert_eq!(Head::from_kind(1, 0), Head::Heading(1));
        assert_eq!(Head::from_kind(1, 6), Head::Heading(6));
        assert_eq!(
            Head::from_kind(3, 1),
            Head::List {
                ordered: false,
                level: 1
            }
        );
        assert_eq!(Head::from_kind(0, 3), Head::Body);
    }

    /// Every fixture, cut at every few bytes, converts or errors — never
    /// panics.
    #[test]
    fn truncated_files_do_not_panic() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hwp/sources");
        for entry in std::fs::read_dir(dir).unwrap() {
            let bytes = std::fs::read(entry.unwrap().path()).unwrap();
            for end in (0..bytes.len()).step_by(97) {
                let _ = HwpBackend::default().convert(&SourceDocument::from_bytes(
                    "t.hwp",
                    crate::InputFormat::Hwp,
                    bytes[..end].to_vec(),
                ));
            }
        }
    }
}
