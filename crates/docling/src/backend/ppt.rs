//! PPT (PowerPoint 97–2003 binary, [MS-PPT]) backend — issue #127.
//!
//! Native parsing, no external converter (docling proper shells out to
//! LibreOffice — `docling` PR #3804). The format is a CFB container whose
//! `PowerPoint Document` stream is a tree of tagged records; the drawing
//! layer inside each slide is OfficeArt ([`officeart`]).
//!
//! Slide content is assembled from two cooperating sources:
//! - the `SlideListWithText` (SLWT) inside the `DocumentContainer` holds the
//!   slide's *text blocks* (`TextHeaderAtom` + `TextCharsAtom`/`TextBytesAtom`
//!   per block, `SlidePersistAtom` opening each slide's group);
//! - each `Slide` container's OfficeArt drawing holds the *shapes* with their
//!   geometry (anchors) — a placeholder shape references its SLWT block by
//!   index (`OutlineTextRefAtom`), a plain textbox embeds its own text atoms.
//!
//! Shapes are walked with their anchors, SLWT references are resolved, and
//! items are emitted in geometric order (top-to-bottom, left-to-right).
//! A **group** of shapes whose child anchors tile a ≥2×≥2 grid is
//! reconstructed into a [`Node::Table`] — this is how legacy PPT stores
//! tables (a table *is* a shape group), so docling's PPTX table output has a
//! native equivalent here. SLWT blocks no shape consumed are appended after,
//! so text never goes missing on files that only fill the SLWT. Titles become
//! headings, other text paragraphs (lines split on `\r`); slides are
//! separated by page breaks, matching the PPTX backend's shape.

use docling_core::{DoclingDocument, Node, Table};

use crate::backend::cfb::CompoundFile;
use crate::backend::officeart::Records;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

const RT_DOCUMENT: u16 = 0x03E8; // DocumentContainer
const RT_SLIDE: u16 = 0x03EE; // SlideContainer
const RT_SLIDE_LIST_WITH_TEXT: u16 = 0x0FF0;
const RT_SLIDE_PERSIST_ATOM: u16 = 0x03F3;
const RT_TEXT_HEADER_ATOM: u16 = 0x0F9F;
const RT_OUTLINE_TEXT_REF_ATOM: u16 = 0x0F9E;
const RT_TEXT_CHARS_ATOM: u16 = 0x0FA0;
const RT_TEXT_BYTES_ATOM: u16 = 0x0FA8;
const RT_STYLE_TEXT_PROP_ATOM: u16 = 0x0FA1;
const RT_STYLE_TEXT_PROP9_ATOM: u16 = 0x0FAC;
const RT_BINARY_TAG_DATA: u16 = 0x138B;
const OA_CLIENT_DATA: u16 = 0xF011;

// OfficeArt ([MS-ODRAW]) record types.
const OA_DG_CONTAINER: u16 = 0xF002;
const OA_SPGR_CONTAINER: u16 = 0xF003;
const OA_SP_CONTAINER: u16 = 0xF004;
const OA_FSPGR: u16 = 0xF009;
const OA_CHILD_ANCHOR: u16 = 0xF00F;
const OA_CLIENT_ANCHOR: u16 = 0xF010;
const OA_CLIENT_TEXTBOX: u16 = 0xF00D;

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

        // SLWT text blocks per slide, in presentation order.
        let mut slwt: Vec<Vec<TextBlock>> = Vec::new();
        for (header, body) in Records::new(&stream) {
            if header.rec_type != RT_DOCUMENT {
                continue;
            }
            for (h2, b2) in Records::new(body) {
                // instance 0 = the slide list (1 = masters, 2 = notes).
                if h2.rec_type == RT_SLIDE_LIST_WITH_TEXT && h2.instance == 0 {
                    collect_slwt_slides(b2, &mut slwt);
                }
            }
        }

        // Shape items per slide, from each Slide container's drawing.
        let slide_shapes: Vec<Vec<ShapeItem>> = Records::new(&stream)
            .filter(|(h, _)| h.rec_type == RT_SLIDE)
            .map(|(_, body)| slide_items(body))
            .collect();

        let n = slwt.len().max(slide_shapes.len());
        let mut doc = DoclingDocument::new(&source.name);
        let mut first = true;
        for i in 0..n {
            let blocks = slwt.get(i).cloned().unwrap_or_default();
            let shapes = slide_shapes.get(i).cloned().unwrap_or_default();
            let nodes = assemble_slide(blocks, shapes);
            if nodes.is_empty() {
                continue;
            }
            if !first {
                doc.push(Node::PageBreak);
            }
            first = false;
            for node in nodes {
                doc.push(node);
            }
        }
        Ok(doc)
    }
}

/// One SLWT text block: title flag + raw text (runs already concatenated).
#[derive(Clone, Default)]
struct TextBlock {
    is_title: bool,
    text: String,
    styles: Vec<ParaStyle>,
    consumed: bool,
}

/// One paragraph's list-relevant style, from the `StyleTextPropAtom`
/// paragraph runs (bullet flag, indent level) merged with the PP9
/// `StyleTextProp9Atom` autonumber extension (numbered lists).
#[derive(Clone, Copy, Default)]
struct ParaStyle {
    /// Characters covered by this run (paragraph text + terminator).
    count: usize,
    indent: u8,
    bullet: bool,
    /// `(scheme, start)` when the paragraph auto-numbers (PP9).
    autonum: Option<(u16, u16)>,
}

/// Parse a `StyleTextPropAtom` body's paragraph-level runs. Each run is
/// `{count u32, indentLevel u16, TextPFException}`; the exception's optional
/// fields are sized by its masks ([MS-PPT] 2.9.31), which this walks exactly
/// so the next run starts at the right offset. Returns runs until `text_len`
/// is covered (the last run also covers the block terminator).
fn parse_para_styles(body: &[u8], text_len: usize) -> Vec<ParaStyle> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut covered = 0usize;
    while covered <= text_len && pos + 10 <= body.len() {
        let count =
            u32::from_le_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]) as usize;
        let indent = u16::from_le_bytes([body[pos + 4], body[pos + 5]]);
        let masks =
            u32::from_le_bytes([body[pos + 6], body[pos + 7], body[pos + 8], body[pos + 9]]);
        pos += 10;
        let mut bullet = false;
        // bulletFlags: present when any of hasBullet/font/color/size masks set.
        if masks & 0x0000_000F != 0 {
            if pos + 2 > body.len() {
                break;
            }
            bullet = body[pos] & 0x01 != 0;
            pos += 2;
        }
        // Remaining optional fields, in on-disk order, sized per masks.
        for (bit, size) in [
            (0x0000_0080u32, 2usize), // bulletChar
            (0x0000_0010, 2),         // bulletFontRef
            (0x0000_0040, 2),         // bulletSize
            (0x0000_0020, 4),         // bulletColor
            (0x0000_0800, 2),         // textAlignment
            (0x0000_1000, 2),         // lineSpacing
            (0x0000_2000, 2),         // spaceBefore
            (0x0000_4000, 2),         // spaceAfter
            (0x0000_0100, 2),         // leftMargin
            (0x0000_0400, 2),         // indent
            (0x0000_8000, 2),         // defaultTabSize
        ] {
            if masks & bit != 0 {
                pos += size;
            }
        }
        if masks & 0x0010_0000 != 0 {
            // tabStops: count u16 + count × 4 bytes.
            if pos + 2 > body.len() {
                break;
            }
            let n = u16::from_le_bytes([body[pos], body[pos + 1]]) as usize;
            pos += 2 + n * 4;
        }
        for (bit, size) in [
            (0x0001_0000u32, 2usize), // fontAlign
            (0x000E_0000, 2),         // wrap flags (one field for the three bits)
            (0x0020_0000, 2),         // textDirection
        ] {
            if masks & bit != 0 {
                pos += size;
            }
        }
        if pos > body.len() {
            break;
        }
        covered += count;
        out.push(ParaStyle {
            count,
            indent: indent.min(u8::MAX as u16) as u8,
            bullet,
            autonum: None,
        });
    }
    out
}

/// Parse a PP9 `StyleTextProp9Atom` (inside the shape's `___PPT9` binary tag):
/// per paragraph `{TextPFException9, TextCFException9, TextSIException}`.
/// Only the autonumber fields are read; a non-empty exception this parser
/// doesn't model ends the walk (styles parsed so far still apply).
fn parse_para_styles9(body: &[u8]) -> Vec<Option<(u16, u16)>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= body.len() {
        let masks = u32::from_le_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        pos += 4;
        let mut has_auto = false;
        let mut scheme_start = None;
        if masks & 0x0080_0000 != 0 {
            pos += 2; // bulletBlipRef
        }
        if masks & 0x0100_0000 != 0 {
            has_auto = body.get(pos).is_some_and(|&b| b != 0);
            pos += 2; // fBulletHasAutoNumber
        }
        if masks & 0x0200_0000 != 0 {
            if pos + 4 > body.len() {
                break;
            }
            let scheme = u16::from_le_bytes([body[pos], body[pos + 1]]);
            let start = u16::from_le_bytes([body[pos + 2], body[pos + 3]]);
            scheme_start = Some((scheme, start.max(1)));
            pos += 4;
        }
        if masks & !0x0380_0000 != 0 {
            // A PF9 field this parser doesn't size — stop cleanly.
            break;
        }
        // TextCFException9 + TextSIException: only the all-empty form is
        // modeled; anything else ends the walk.
        let Some(cf) = body.get(pos..pos + 4) else {
            break;
        };
        if cf != [0, 0, 0, 0] {
            break;
        }
        pos += 4;
        let Some(si) = body.get(pos..pos + 4) else {
            break;
        };
        if si != [0, 0, 0, 0] {
            break;
        }
        pos += 4;
        out.push(
            (has_auto || scheme_start.is_some())
                .then_some(scheme_start)
                .flatten(),
        );
    }
    out
}

/// Walk a SlideListWithText body: `SlidePersistAtom` starts a new slide, a
/// `TextHeaderAtom` starts a new text block, text atoms append to it.
fn collect_slwt_slides(body: &[u8], slides: &mut Vec<Vec<TextBlock>>) {
    for (h, b) in Records::new(body) {
        match h.rec_type {
            RT_SLIDE_PERSIST_ATOM => slides.push(Vec::new()),
            RT_TEXT_HEADER_ATOM => {
                let tx = b
                    .get(..4)
                    .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
                    .unwrap_or(u32::MAX);
                if let Some(slide) = slides.last_mut() {
                    slide.push(TextBlock {
                        is_title: tx == TX_TITLE || tx == TX_CENTER_TITLE,
                        ..TextBlock::default()
                    });
                }
            }
            RT_TEXT_CHARS_ATOM => {
                if let Some(block) = slides.last_mut().and_then(|s| s.last_mut()) {
                    block.text.push_str(&utf16_text(b));
                }
            }
            RT_TEXT_BYTES_ATOM => {
                if let Some(block) = slides.last_mut().and_then(|s| s.last_mut()) {
                    block.text.push_str(&bytes_text(b));
                }
            }
            RT_STYLE_TEXT_PROP_ATOM => {
                if let Some(block) = slides.last_mut().and_then(|s| s.last_mut()) {
                    let len = block.text.chars().count();
                    block.styles.extend(parse_para_styles(b, len));
                }
            }
            _ => {}
        }
    }
}

/// The text carried by one shape: embedded atoms, or a reference to the
/// slide's SLWT block by index.
#[derive(Clone, Default)]
struct ShapeText {
    is_title: bool,
    text: String,
    styles: Vec<ParaStyle>,
    outline_ref: Option<u32>,
}

/// A shape's anchor rectangle: `(left, top, right, bottom)`.
type Anchor = (i32, i32, i32, i32);

/// One item discovered in a slide's drawing, in encounter order.
#[derive(Clone)]
enum ShapeItem {
    Text {
        anchor: Option<Anchor>,
        text: ShapeText,
    },
    Table {
        table: Table,
    },
}

/// Extract a slide's drawing items: find the OfficeArtDgContainer, walk its
/// root group's children — plain shapes become text items, nested groups are
/// tried as tables (a legacy PPT table *is* a group whose child anchors tile
/// a grid) and otherwise flattened.
fn slide_items(slide_body: &[u8]) -> Vec<ShapeItem> {
    let Some(dg) = find_container(slide_body, OA_DG_CONTAINER, 0) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for (h, b) in Records::new(dg) {
        if h.rec_type == OA_SPGR_CONTAINER {
            // Root group: first SpContainer is the canvas frame (FSPGR).
            for (h2, b2) in Records::new(b) {
                match h2.rec_type {
                    OA_SP_CONTAINER if !has_record(b2, OA_FSPGR) => {
                        if let Some(item) = shape_item(b2) {
                            items.push(item);
                        }
                    }
                    OA_SPGR_CONTAINER => group_items(b2, &mut items, 0),
                    _ => {}
                }
            }
        }
    }
    items
}

/// Handle one group container: reconstruct a table from the child grid, else
/// flatten the children as ordinary items (recursing into nested groups).
fn group_items(group_body: &[u8], out: &mut Vec<ShapeItem>, depth: usize) {
    if depth > 16 {
        return;
    }
    let mut cells: Vec<(Anchor, ShapeText)> = Vec::new();
    let mut children: Vec<ShapeItem> = Vec::new();
    for (h, b) in Records::new(group_body) {
        match h.rec_type {
            OA_SP_CONTAINER => {
                if has_record(b, OA_FSPGR) {
                    // The group frame: geometry container only, not a cell.
                    continue;
                }
                if let Some(ShapeItem::Text { anchor, text }) = shape_item(b) {
                    // Border/line shapes are degenerate rectangles; they are
                    // not cells (they'd mint phantom rows/columns).
                    if let Some((l, t, r, b)) = anchor {
                        if (r - l).abs() > 1 && (b - t).abs() > 1 {
                            cells.push(((l, t, r, b), text.clone()));
                        }
                    }
                    children.push(ShapeItem::Text { anchor, text });
                }
            }
            OA_SPGR_CONTAINER => group_items(b, &mut children, depth + 1),
            _ => {}
        }
    }
    if let Some(table) = grid_table(&cells) {
        out.push(ShapeItem::Table { table });
    } else {
        out.append(&mut children);
    }
}

/// Parse one SpContainer into a text item (anchor + text/outline reference).
fn shape_item(sp_body: &[u8]) -> Option<ShapeItem> {
    let anchor = shape_anchor(sp_body);
    let mut text = ShapeText::default();
    let mut autonums: Vec<Option<(u16, u16)>> = Vec::new();
    for (h, b) in Records::new(sp_body) {
        match h.rec_type {
            OA_CLIENT_TEXTBOX => {
                for (h2, b2) in Records::new(b) {
                    match h2.rec_type {
                        RT_TEXT_HEADER_ATOM => {
                            let tx = b2
                                .get(..4)
                                .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
                                .unwrap_or(u32::MAX);
                            text.is_title = tx == TX_TITLE || tx == TX_CENTER_TITLE;
                        }
                        RT_OUTLINE_TEXT_REF_ATOM => {
                            text.outline_ref = b2
                                .get(..4)
                                .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]));
                        }
                        RT_TEXT_CHARS_ATOM => text.text.push_str(&utf16_text(b2)),
                        RT_TEXT_BYTES_ATOM => text.text.push_str(&bytes_text(b2)),
                        RT_STYLE_TEXT_PROP_ATOM => {
                            let len = text.text.chars().count();
                            text.styles.extend(parse_para_styles(b2, len));
                        }
                        _ => {}
                    }
                }
            }
            // The PP9 extension rides in the shape's client data as a
            // `___PPT9` binary tag holding a StyleTextProp9Atom: the
            // per-paragraph autonumber (numbered list) info.
            OA_CLIENT_DATA => {
                if let Some(blob) = find_container(b, RT_BINARY_TAG_DATA, 0) {
                    for (h3, b3) in Records::new(blob) {
                        if h3.rec_type == RT_STYLE_TEXT_PROP9_ATOM {
                            autonums = parse_para_styles9(b3);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    for (style, auto) in text.styles.iter_mut().zip(autonums) {
        style.autonum = auto;
    }
    Some(ShapeItem::Text { anchor, text })
}

/// A shape's anchor: the child anchor (within a group, 4×i32) or the PPT
/// client anchor (on the slide, 4×i16 as top/left/right/bottom, or 4×i32).
fn shape_anchor(sp_body: &[u8]) -> Option<Anchor> {
    for (h, b) in Records::new(sp_body) {
        match h.rec_type {
            OA_CHILD_ANCHOR if b.len() >= 16 => {
                let v: Vec<i32> = b[..16]
                    .chunks_exact(4)
                    .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                return Some((v[0], v[1], v[2], v[3])); // l, t, r, b
            }
            OA_CLIENT_ANCHOR if b.len() >= 8 => {
                if b.len() >= 16 {
                    let v: Vec<i32> = b[..16]
                        .chunks_exact(4)
                        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                        .collect();
                    return Some((v[1], v[0], v[3], v[2])); // t,l,r,b → l,t,r,b
                }
                let v: Vec<i32> = b[..8]
                    .chunks_exact(2)
                    .map(|c| i16::from_le_bytes([c[0], c[1]]) as i32)
                    .collect();
                return Some((v[1], v[0], v[3], v[2])); // t,l,r,b → l,t,r,b
            }
            _ => {}
        }
    }
    None
}

/// Reconstruct a table from cell shapes when their anchors tile a grid:
/// cluster the child anchors' left/top edges into column/row boundaries and
/// require at least a 2×2 grid with most positions covered. Merged cells span
/// the boundaries they cross (OTSL-style continuations, like the XLSX/DOCX
/// backends emit).
fn grid_table(cells: &[(Anchor, ShapeText)]) -> Option<Table> {
    if cells.len() < 4 {
        return None;
    }
    let tolerance = edge_tolerance(cells);
    let col_edges = cluster(cells.iter().map(|((l, ..), _)| *l), tolerance);
    let row_edges = cluster(cells.iter().map(|((_, t, ..), _)| *t), tolerance);
    let (nrows, ncols) = (row_edges.len(), col_edges.len());
    if nrows < 2 || ncols < 2 {
        return None;
    }
    // Most grid positions must be covered for this to read as a table.
    if cells.len() * 10 < nrows * ncols * 6 {
        return None;
    }

    let index_of = |edges: &[i32], v: i32| -> usize {
        edges
            .iter()
            .position(|&e| (v - e).abs() <= tolerance)
            .unwrap_or_else(|| edges.iter().filter(|&&e| e < v).count().saturating_sub(1))
    };
    let mut grid: Vec<Vec<Option<String>>> = vec![vec![None; ncols]; nrows];
    let mut col_cont = vec![vec![false; ncols]; nrows];
    let mut row_cont = vec![vec![false; ncols]; nrows];
    for ((l, t, r, b), text) in cells {
        let ci = index_of(&col_edges, *l);
        let ri = index_of(&row_edges, *t);
        // The span covers every further boundary strictly inside (l, r)/(t, b).
        let col_span = 1 + col_edges[ci + 1..]
            .iter()
            .take_while(|&&e| e < *r - tolerance)
            .count();
        let row_span = 1 + row_edges[ri + 1..]
            .iter()
            .take_while(|&&e| e < *b - tolerance)
            .count();
        let value = text.text.replace('\r', "\n").trim().to_string();
        for rr in ri..(ri + row_span).min(nrows) {
            for cc in ci..(ci + col_span).min(ncols) {
                let cell = grid.get_mut(rr).and_then(|row| row.get_mut(cc))?;
                if cell.is_none() {
                    *cell = Some(value.clone());
                }
                if (rr, cc) == (ri, ci) {
                    continue;
                }
                if cc > ci {
                    col_cont[rr][cc] = true;
                }
                if rr > ri && cc == ci {
                    row_cont[rr][cc] = true;
                }
                if rr > ri && cc > ci {
                    row_cont[rr][cc] = true;
                }
            }
        }
    }
    let rows: Vec<Vec<String>> = grid
        .into_iter()
        .map(|row| row.into_iter().map(Option::unwrap_or_default).collect())
        .collect();
    let any_span = col_cont
        .iter()
        .flatten()
        .chain(row_cont.iter().flatten())
        .any(|&x| x);
    let structure = any_span.then(|| {
        let mut header_row = vec![false; nrows];
        if let Some(h) = header_row.first_mut() {
            *h = true;
        }
        docling_core::TableStructure {
            header_row,
            col_continuation: col_cont,
            row_continuation: row_cont,
            row_header: Vec::new(),
            col_header: Vec::new(),
        }
    });
    Some(Table {
        rows,
        location: None,
        structure,
        cell_blocks: None,
    })
}

/// Cluster tolerance scaled from the cells' typical size, so the grid check
/// works whatever coordinate space the anchors use.
fn edge_tolerance(cells: &[(Anchor, ShapeText)]) -> i32 {
    let avg_w: i32 = cells
        .iter()
        .map(|((l, _, r, _), _)| (r - l).abs())
        .sum::<i32>()
        / cells.len().max(1) as i32;
    (avg_w / 8).max(2)
}

/// Sort + merge values within `tolerance` into representative edges.
fn cluster(values: impl Iterator<Item = i32>, tolerance: i32) -> Vec<i32> {
    let mut v: Vec<i32> = values.collect();
    v.sort_unstable();
    let mut out: Vec<i32> = Vec::new();
    for x in v {
        match out.last() {
            Some(&last) if (x - last).abs() <= tolerance => {}
            _ => out.push(x),
        }
    }
    out
}

/// `true` if the record tree body directly contains a record of `rec_type`.
fn has_record(body: &[u8], rec_type: u16) -> bool {
    Records::new(body).any(|(h, _)| h.rec_type == rec_type)
}

/// Depth-first search for the first container of `rec_type`.
fn find_container(body: &[u8], rec_type: u16, depth: usize) -> Option<&[u8]> {
    if depth > 16 {
        return None;
    }
    for (h, b) in Records::new(body) {
        if h.rec_type == rec_type {
            return Some(b);
        }
        if h.version == 0xF {
            if let Some(found) = find_container(b, rec_type, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

/// Merge one slide's SLWT blocks and drawing shapes into nodes: shapes emit
/// in geometric order (resolving outline references into the blocks), then
/// any block no shape consumed is appended, so nothing is lost.
fn assemble_slide(mut blocks: Vec<TextBlock>, shapes: Vec<ShapeItem>) -> Vec<Node> {
    let mut nodes = Vec::new();
    let mut list = ListState::default();
    for item in shapes {
        match item {
            ShapeItem::Table { table } => {
                nodes.push(Node::Table(table));
                list = ListState::default();
            }
            ShapeItem::Text { text, .. } => {
                let (is_title, content, styles, autonums) = match text.outline_ref {
                    Some(ix) => match blocks.get_mut(ix as usize) {
                        Some(block) => {
                            block.consumed = true;
                            // Outline text: bullets come from the block's own
                            // style runs; the shape may add PP9 autonumbers.
                            let autos: Vec<_> = text.styles.iter().map(|s| s.autonum).collect();
                            (
                                block.is_title,
                                block.text.clone(),
                                block.styles.clone(),
                                autos,
                            )
                        }
                        None => continue,
                    },
                    None => {
                        // Embedded text: mark the matching SLWT twin (if any)
                        // consumed so the tail append doesn't duplicate it.
                        if let Some(block) = blocks
                            .iter_mut()
                            .find(|b| !b.consumed && b.text == text.text)
                        {
                            block.consumed = true;
                        }
                        let autos: Vec<_> = text.styles.iter().map(|s| s.autonum).collect();
                        (text.is_title, text.text.clone(), text.styles.clone(), autos)
                    }
                };
                push_text(
                    &mut nodes, is_title, &content, &styles, &autonums, &mut list,
                );
            }
        }
    }
    for block in blocks.iter().filter(|b| !b.consumed) {
        let autos: Vec<_> = block.styles.iter().map(|s| s.autonum).collect();
        push_text(
            &mut nodes,
            block.is_title,
            &block.text,
            &block.styles,
            &autos,
            &mut list,
        );
    }
    nodes
}

/// Running list context across a slide's emitted nodes, for `first_in_list`
/// and ordered-item numbering.
#[derive(Default)]
struct ListState {
    /// `Some(ordered)` while the previous node was a list item.
    prev: Option<bool>,
    /// Next number for an ordered run.
    next_number: u64,
}

/// Emit a text run as heading/paragraph/list-item nodes, one per
/// `\r`-separated line, list-classifying each line by its paragraph style.
fn push_text(
    nodes: &mut Vec<Node>,
    is_title: bool,
    text: &str,
    styles: &[ParaStyle],
    autonums: &[Option<(u16, u16)>],
    list: &mut ListState,
) {
    // Map each paragraph to its style run by cumulative character position.
    let mut run_ix = 0usize;
    let mut run_left = styles.first().map(|s| s.count).unwrap_or(usize::MAX);
    for line in text.split('\r') {
        let chars = line.chars().count() + 1; // + terminator
        let style = styles.get(run_ix).copied().unwrap_or_default();
        let autonum = autonums.get(run_ix).copied().flatten().or(style.autonum);
        // Advance the run cursor.
        if run_left <= chars {
            run_ix += 1;
            run_left = styles.get(run_ix).map(|s| s.count).unwrap_or(usize::MAX);
        } else {
            run_left -= chars;
        }

        // docling keeps the run's own spacing (a trailing space in the
        // source survives into Markdown); only whitespace-only lines drop.
        if line.trim().is_empty() {
            continue;
        }
        if is_title {
            nodes.push(Node::Heading {
                level: 1,
                text: line.to_string(),
            });
            list.prev = None;
            continue;
        }
        let ordered = autonum.is_some();
        if style.bullet || ordered {
            let first = list.prev != Some(ordered);
            if first && ordered {
                list.next_number = autonum.map(|(_, start)| start as u64).unwrap_or(1);
            }
            let number = if ordered {
                let n = list.next_number;
                list.next_number += 1;
                n
            } else {
                1
            };
            nodes.push(Node::ListItem {
                ordered,
                number,
                first_in_list: first,
                text: line.to_string(),
                level: style.indent,
                marker: None,
                location: None,
                dclx: None,
                href: None,
                layer: None,
            });
            list.prev = Some(ordered);
        } else {
            nodes.push(Node::Paragraph {
                text: line.to_string(),
            });
            list.prev = None;
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
        assert!(headings > 0, "expected slide titles: {:?}", doc.nodes);
    }

    #[test]
    fn reconstructs_grouped_shapes_into_a_table() {
        let doc = PptBackend
            .convert(&fixture("powerpoint_sample.ppt"))
            .expect("converts");
        let tables: Vec<&Table> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Table(t) => Some(t),
                _ => None,
            })
            .collect();
        assert!(!tables.is_empty(), "expected a reconstructed table");
        let t = tables[0];
        assert!(
            t.rows.len() >= 2 && t.rows[0].len() >= 2,
            "grid too small: {:?}",
            t.rows
        );
    }

    #[test]
    fn grid_table_rejects_a_column_of_shapes() {
        // Four stacked shapes (one column) must NOT read as a table.
        let cells: Vec<(Anchor, ShapeText)> = (0..4)
            .map(|i| ((0, i * 100, 200, i * 100 + 90), ShapeText::default()))
            .collect();
        assert!(grid_table(&cells).is_none());
    }

    #[test]
    fn grid_table_builds_2x2_with_span() {
        let mk = |s: &str| ShapeText {
            text: s.into(),
            ..ShapeText::default()
        };
        // 2×2 grid; the top row's single cell spans both columns.
        let cells = vec![
            ((0, 0, 200, 90), mk("header spans")),
            ((0, 100, 100, 190), mk("a")),
            ((100, 100, 200, 190), mk("b")),
            ((0, 200, 100, 290), mk("c")),
            ((100, 200, 200, 290), mk("d")),
        ];
        let t = grid_table(&cells).expect("is a table");
        assert_eq!(t.rows.len(), 3);
        // Spanned text repeats across the covered cells (docling's table-grid
        // convention); the continuation flags mark the span for DocLang.
        assert_eq!(
            t.rows[0],
            vec!["header spans".to_string(), "header spans".to_string()]
        );
        assert_eq!(t.rows[1], vec!["a".to_string(), "b".to_string()]);
        let s = t.structure.expect("span structure");
        assert!(s.col_continuation[0][1], "top row spans");
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        let src = SourceDocument::from_bytes("x.ppt", InputFormat::Ppt, vec![0u8; 128]);
        assert!(PptBackend.convert(&src).is_err());
    }
}
