//! XLSX (Excel) backend.
//!
//! Ports docling's `MsExcelDocumentBackend`: every worksheet is scanned for
//! contiguous rectangular data regions ("tables") via a flood fill, and each
//! region becomes a [`Node::Table`]. Sheet order is preserved; there are no
//! per-sheet headings (matching current docling output). Cell values are
//! rendered to match openpyxl's `str(value)`.
//!
//! `calamine` does the heavy lifting (ZIP, shared strings, value typing, date
//! detection); this backend contributes the region detection and the
//! openpyxl-compatible value formatting — through the cell's number format
//! since #634 ([`super::numfmt`]: `$12.50`, `10%`, `Feb-25` as Excel shows
//! them, docling PR #4628's rules).

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::sync::Arc;

use calamine::{Data, Range, Reader, Xlsx};

use super::numfmt::{self, SheetFormats};
use super::xlsx_richdata;

/// The most cells a sheet's used area (the bounding box of its non-empty
/// cells) may span before the sheet is skipped: `DOCLING_RS_SHEET_MAX_CELLS`,
/// default ten million (~320 MB of cell storage). calamine materializes a
/// sheet as a *dense* grid over that box, so a 4 KB file with one value in
/// `A1` and one in `XFD1048576` asked for 17 billion cells — a 512 GB
/// allocation that aborted the whole process (and with it a docling-serve
/// instance). Real sheets sit far below the cap: 200 000 rows × 20 columns
/// is 4 million.
fn sheet_max_cells() -> u64 {
    docling_core::env::parse::<u64>("DOCLING_RS_SHEET_MAX_CELLS").unwrap_or(10_000_000)
}

/// `Xlsx::worksheet_range` with the used area checked *before* the dense grid
/// is allocated: the non-empty cells stream through calamine's cell reader
/// (the same reader and value decoding `worksheet_range` uses), their
/// bounding box is compared against [`sheet_max_cells`], and only then does
/// `Range::from_sparse` lay them out. An oversized sheet is skipped with a
/// warning (`None`), like an unreadable one.
fn bounded_worksheet_range<RS: std::io::Read + std::io::Seek>(
    wb: &mut Xlsx<RS>,
    name: &str,
) -> Option<Range<Data>> {
    let mut reader = wb.worksheet_cells_reader(name).ok()?;
    let mut cells: Vec<calamine::Cell<Data>> = Vec::new();
    let (mut r0, mut r1, mut c0, mut c1) = (u32::MAX, 0u32, u32::MAX, 0u32);
    loop {
        match reader.next_cell() {
            Ok(Some(cell)) if matches!(cell.get_value(), calamine::DataRef::Empty) => {}
            Ok(Some(cell)) => {
                let (r, c) = cell.get_position();
                r0 = r0.min(r);
                r1 = r1.max(r);
                c0 = c0.min(c);
                c1 = c1.max(c);
                cells.push(calamine::Cell::new(
                    (r, c),
                    Data::from(cell.get_value().clone()),
                ));
            }
            Ok(None) => break,
            Err(_) => return None,
        }
    }
    if cells.is_empty() {
        return Some(Range::empty());
    }
    let area = (r1 - r0 + 1) as u64 * (c1 - c0 + 1) as u64;
    if area > sheet_max_cells() {
        eprintln!(
            "docling: sheet {name:?}: used area {}×{} cells ({area}) exceeds \
             DOCLING_RS_SHEET_MAX_CELLS ({}); sheet skipped",
            r1 - r0 + 1,
            c1 - c0 + 1,
            sheet_max_cells()
        );
        return None;
    }
    // `from_sparse` wants row order; a sheet's XML may list rows out of order.
    cells.sort_by_key(|c| c.get_position());
    Some(Range::from_sparse(cells))
}
use docling_core::{DoclingDocument, Node, PictureDescription, Table, TableCell};
use quick_xml::events::Event;
use quick_xml::Reader as XmlReader;
use rayon::prelude::*;

use crate::backend::ooxml::{resolve, Package};
use crate::backend::xlsx_drawings;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

/// A sheet's merged regions as absolute `((start_row, start_col), (end_row,
/// end_col))` cell spans.
pub(crate) type Merges = Vec<((u32, u32), (u32, u32))>;

/// One sheet's assembled content: `(bbox in cell units, node)` items in
/// discovery order, plus the sheet's comments as `(row, col, cell ref, line)` —
/// the coordinates locate the item each comment annotates, the cell ref names
/// its `comment_section` group (docling's `comment-{sheet}-{cell}`).
type SheetComment = (usize, usize, String, String);
type SheetItems = (Vec<((usize, usize, usize, usize), Node)>, Vec<SheetComment>);
/// A sheet item with its docling creation rank: `(seq, bbox, node)`.
type RankedItem = (usize, (usize, usize, usize, usize), Node);

/// Load one sheet's merged regions (`<mergeCells>`), absolute coordinates.
fn sheet_merges<R: std::io::Read + std::io::Seek>(wb: &mut Xlsx<R>, name: &str) -> Merges {
    // A sheet without `<mergeCells>` (or one calamine cannot read) has none.
    wb.merge_cells_by_sheet_name(name)
        .unwrap_or_default()
        .iter()
        .map(|d| (d.start, d.end))
        .collect()
}

#[derive(Default)]
pub struct XlsxBackend {
    /// Omit empty cells from sparse-sheet table grids (#271, opt-in) — see
    /// [`find_tables`].
    pub skip_empty: bool,
}

impl DeclarativeBackend for XlsxBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        // Binary workbook (`.xlsb`, issue #210): the same ZIP envelope with
        // binary parts instead of OPC XML — detected by the workbook part
        // (extensions lie) and handed to the slim calamine-`Xlsb` path.
        if Package::open(&source.bytes)
            .is_some_and(|mut pkg| pkg.read_bytes("xl/workbook.bin").is_some())
        {
            return convert_xlsb(source, self.skip_empty);
        }
        let cursor = Cursor::new(source.bytes.clone());
        let mut workbook: Xlsx<_> =
            Xlsx::new(cursor).map_err(|e| ConversionError::with_source("xlsx", e))?;
        let mut pkg = Package::open(&source.bytes)
            .ok_or_else(|| ConversionError::Parse("xlsx: bad zip".into()))?;

        // Every sheet is a page, in workbook order — worksheets *and*
        // chartsheets, visible and hidden alike (docling numbers them all; a
        // hidden sheet's items land in the invisible content layer).
        let metas: Vec<(String, calamine::SheetType, calamine::SheetVisible)> = workbook
            .sheets_metadata()
            .iter()
            .map(|s| (s.name.clone(), s.typ, s.visible))
            .collect();
        // Sheet name -> its part path (via workbook.xml r:id -> workbook rels).
        let wb_xml = pkg.read("xl/workbook.xml").unwrap_or_default();
        let wb_rels_raw = pkg.rels_for("xl/workbook.xml");
        let wb_rels: HashMap<String, String> = wb_rels_raw
            .iter()
            .map(|r| (r.id.clone(), resolve("xl", &r.target)))
            .collect();
        let sheet_parts: HashMap<String, String> = workbook_sheets(&wb_xml)
            .into_iter()
            .filter_map(|(name, rid)| Some((name, wb_rels.get(&rid)?.clone())))
            .collect();
        // Number formats (#634): `cellXfs` → code once per workbook, the
        // cells' style indexes per sheet (below, with the ranges).
        let xfs = Arc::new(numfmt::xlsx_xf_formats(
            &pkg.read("xl/styles.xml").unwrap_or_default(),
        ));
        let is_1904 = workbook.has_1904_epoch();
        // Images placed in cells (docling#5015), by `vm` index — read once
        // per workbook; each sheet then maps its `vm` cells onto them.
        let rich_images = xlsx_richdata::rich_value_images(&mut pkg, &wb_rels_raw);
        // A sheet's number formats and placed images, from its part's XML
        // (read once for both).
        let sheet_formats = |pkg: &mut Package, name: &str, range: &Range<Data>| {
            let xml = sheet_parts.get(name).and_then(|part| pkg.read(part));
            let cells = xml
                .as_deref()
                .map(numfmt::xlsx_cell_styles)
                .unwrap_or_default();
            // Only an error cell can hold a placed image (its `#VALUE!`
            // fallback), so a sheet without one skips the scan.
            let images = match xml.as_deref() {
                Some(xml)
                    if !rich_images.is_empty()
                        && range
                            .used_cells()
                            .any(|(_, _, d)| matches!(d, Data::Error(_))) =>
                {
                    xlsx_richdata::cells_with_rich_values(xml)
                        .into_iter()
                        .filter_map(|(at, vm)| Some((at, rich_images.get(&vm)?.clone())))
                        .collect()
                }
                _ => HashMap::new(),
            };
            SheetFormats {
                xfs: xfs.clone(),
                cells,
                images,
            }
        };

        // Threaded-comment persons (Excel 365).
        let persons = pkg
            .read("xl/persons/person.xml")
            .map(|xml| xlsx_drawings::parse_persons(&xml))
            .unwrap_or_default();

        // Load every worksheet's cell range + merged regions up front, in
        // parallel — one calamine reader per rayon worker over the shared
        // bytes (opening a reader is ~2ms; the per-sheet XML parse is what
        // dominates on many-sheet files). All ranges must be loaded before
        // any sheet's items assemble: chart series resolve by reference into
        // arbitrary sheets.
        let shared: Arc<[u8]> = Arc::from(source.bytes.as_slice());
        let mut ranges: HashMap<String, (Range<Data>, Merges, SheetFormats)> = metas
            .par_iter()
            .filter(|(_, typ, _)| matches!(typ, calamine::SheetType::WorkSheet))
            .map_init(
                || (Xlsx::new(Cursor::new(shared.clone())).ok(), pkg.clone()),
                |(wb, pkg), (name, _, _)| {
                    let wb = wb.as_mut()?;
                    let range = bounded_worksheet_range(wb, name)?;
                    let formats = sheet_formats(pkg, name, &range);
                    Some((name.clone(), (range, sheet_merges(wb, name), formats)))
                },
            )
            .flatten()
            .collect();
        // Safety net: a sheet whose parallel load failed (it shouldn't — the
        // same bytes already opened once above) loads from the main reader.
        for (name, typ, _) in &metas {
            if matches!(typ, calamine::SheetType::WorkSheet) && !ranges.contains_key(name) {
                if let Some(range) = bounded_worksheet_range(&mut workbook, name) {
                    let merges = sheet_merges(&mut workbook, name);
                    let formats = sheet_formats(&mut pkg, name, &range);
                    ranges.insert(name.clone(), (range, merges, formats));
                }
            }
        }
        let resolve_ref = |reference: &str, own_sheet: &str| -> Vec<String> {
            let Some((sheet, (min_c, min_r, max_c, max_r))) =
                xlsx_drawings::parse_range_ref(reference)
            else {
                return Vec::new();
            };
            let sheet: String = sheet.unwrap_or_else(|| own_sheet.to_string());
            let Some((range, _, _)) = ranges.get(&sheet) else {
                return Vec::new();
            };
            let (rs_r, rs_c) = range.start().unwrap_or((0, 0));
            // A range stops at its sheet's last used row and column — a
            // whole-column `$B:$B`, or one declared to row 1,048,576, reads
            // the cells there are (docling#4812); past them every cell is
            // empty, which the chart table trims anyway.
            let (re_r, re_c) = range.end().unwrap_or((0, 0));
            let max_r = max_r.min(re_r as usize);
            let max_c = max_c.min(re_c as usize);
            let mut out = Vec::new();
            for r in min_r..=max_r {
                for c in min_c..=max_c {
                    let rr = (r as u32).wrapping_sub(rs_r) as usize;
                    let cc = (c as u32).wrapping_sub(rs_c) as usize;
                    // A chart's data is the cell's value, not its display:
                    // `str(value)`, a date as ISO 8601 (docling#4812).
                    let v = if r as u32 >= rs_r && c as u32 >= rs_c {
                        range
                            .get((rr, cc))
                            .map(|d| numfmt::chart_text(d, is_1904))
                            .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    out.push(v);
                }
            }
            out
        };

        let mut doc = DoclingDocument::new(&source.name);
        // Every sheet's comments, in workbook order — the order they become
        // `comment_section` groups below, which is the order `Node::Commented`
        // indices refer to.
        let mut comments: Vec<(String, String)> = Vec::new();
        // Each sheet's items (tables, drawings, charts, comments) assemble in
        // parallel, one package clone per worker; the ordered collect and the
        // sequential merge below keep node order, page breaks, and the
        // comments tail identical to the sequential walk.
        let per_sheet: Vec<SheetItems> = metas
            .par_iter()
            .enumerate()
            .map(|(page_ix, (name, typ, _))| {
                sheet_items(SheetCtx {
                    pkg: pkg.clone(),
                    name,
                    typ: *typ,
                    page_ix,
                    metas: &metas,
                    sheet_parts: &sheet_parts,
                    ranges: &ranges,
                    persons: &persons,
                    resolve_ref: &resolve_ref,
                    skip_empty: self.skip_empty,
                    is_1904,
                })
            })
            .collect();

        // The page number of the most recent sheet that produced items — the
        // DocLang page break trails the *following* sheet's content (docling
        // serializes each sheet group before the page-break node that the
        // item iterator placed inside it).
        let mut prev_item_page: Option<usize> = None;
        for (page_ix, ((sheet_name, _, visible), (mut items, sheet_comments))) in
            metas.iter().zip(per_sheet).enumerate()
        {
            let hidden = !matches!(visible, calamine::SheetVisible::Visible);
            // Each comment takes a slot in the document-wide `comment_section`
            // run — row-major within the sheet, sheets in workbook order, which
            // is the order the groups are appended in below.
            let slots: Vec<(usize, usize, usize)> = sheet_comments
                .iter()
                .map(|(row, col, cell, text)| {
                    let slot = comments.len();
                    comments.push((format!("comment-{sheet_name}-{cell}"), text.clone()));
                    (slot, *row, *col)
                })
                .collect();
            // Every sheet is a page of the JSON (`pages`), sized like docling's
            // `_find_page_size`: the largest right/bottom edge of its items, in
            // cell units — 0×0 for a sheet without any.
            let page_no = page_ix + 1;
            if items.is_empty() {
                doc.push(Node::PageInfo {
                    page_no,
                    width: 0.0,
                    height: 0.0,
                });
                // docling still opens a group for a sheet with no items (an
                // empty chartsheet), so the sheet count survives into the JSON.
                doc.push(sheet_group(sheet_name, hidden, Vec::new()));
                continue;
            }
            // docling creates a sheet's items in three passes — the tables
            // (each label right before its table), then the images, then the
            // charts (in drawing order) — which is the order it numbers them
            // (`#/tables/N`, `#/texts/N`, `#/pictures/N`); the group's
            // children are then sorted by top coordinate, stably, so items
            // sharing a top keep that creation order too. Reproduce both: rank
            // by creation, then sort by top and carry the rank along.
            let pass = |n: &Node| match n {
                Node::Picture { .. } => 1,
                Node::Chart { .. } => 2,
                _ => 0,
            };
            items.sort_by_key(|(_, n)| pass(n));
            let mut items: Vec<RankedItem> = items
                .into_iter()
                .enumerate()
                .map(|(seq, (bbox, node))| (seq, bbox, node))
                .collect();
            items.sort_by_key(|(_, (_, t, _, _), _)| *t);
            // docling's `_find_cell_item`: a comment annotates the item whose
            // cell range covers the commented cell (resolved after the sort, so
            // the indices address the emitted order).
            let mut annotations: Vec<Vec<usize>> = vec![Vec::new(); items.len()];
            for (slot, row, col) in slots {
                if let Some(ix) = items
                    .iter()
                    // The item bboxes are half-open on the right/bottom edge
                    // (`max + 1`), like the ranges `find_tables` reports.
                    .position(|(_, (l, t, r, b), _)| {
                        (*l..*r).contains(&col) && (*t..*b).contains(&row)
                    })
                {
                    annotations[ix].push(slot);
                }
            }
            // Location provenance against the sheet's extent.
            let page_w = items
                .iter()
                .map(|(_, (_, _, r, _), _)| *r)
                .max()
                .unwrap_or(1);
            let page_h = items
                .iter()
                .map(|(_, (_, _, _, b), _)| *b)
                .max()
                .unwrap_or(1);
            doc.push(Node::PageInfo {
                page_no,
                width: page_w as f32,
                height: page_h as f32,
            });
            for (_, (l, t, r, b), node) in &mut items {
                let loc = [
                    location_value(*l, page_w),
                    location_value(*t, page_h),
                    location_value(*r, page_w),
                    location_value(*b, page_h),
                ];
                match node {
                    Node::Table(table) => table.location = Some(loc),
                    Node::Chart { location, .. } => *location = Some(loc),
                    Node::Picture { .. } => {}
                    _ => {}
                }
            }
            let mut children = Vec::with_capacity(items.len());
            for (ix, (seq, (l, t, r, b), node)) in items.into_iter().enumerate() {
                let node = if let Node::Picture { .. } = &node {
                    Node::Located {
                        location: [
                            location_value(l, page_w),
                            location_value(t, page_h),
                            location_value(r, page_w),
                            location_value(b, page_h),
                        ],
                        inner: Box::new(node),
                    }
                } else {
                    node
                };
                let node = if annotations[ix].is_empty() {
                    node
                } else {
                    Node::Commented {
                        comments: std::mem::take(&mut annotations[ix]),
                        inner: Box::new(node),
                    }
                };
                // docling's provenance for a sheet item is its cell-index box
                // verbatim (top-left origin) with a `(0, 0)` charspan — for the
                // table, the section label above it, a picture, a chart. The
                // DocLang grid above cannot carry those integers exactly, so
                // the JSON reads them from this wrapper.
                children.push(Node::Prov {
                    page_no,
                    bbox: [l as f32, t as f32, r as f32, b as f32],
                    charspan: [0, 0],
                    bottom_left: false,
                    seq: Some(seq),
                    inner: Box::new(node),
                });
            }
            // A hidden sheet's group carries the invisible layer, which the
            // serializers stamp on every item inside it — the same output the
            // old per-item `Node::Furniture` wrapper produced in DocLang, and
            // docling's shape in the JSON.
            doc.push(sheet_group(sheet_name, hidden, children));
            // DocLang page break: trails this sheet's content when an earlier
            // sheet already produced items (see module docs).
            if prev_item_page.is_some() {
                doc.push(Node::PageBreak);
            }
            prev_item_page = Some(page_ix + 1);
        }
        for (name, text) in comments {
            doc.nodes.push(Node::CommentSection {
                name,
                text,
                // The xlsx backend goes through docling-core's `add_comment`,
                // which links the note text item itself (the docx backend
                // overrides that with the group).
                refs_note_text: true,
                grouped: true,
            });
        }
        Ok(doc)
    }
}

/// docling's per-sheet group: `label: "sheet"`, named after the worksheet, on
/// the invisible content layer when the sheet is hidden. DocLang has no group
/// element, so this is transparent there; the JSON gets docling's `sheet` group
/// with the sheet's items as its children.
fn sheet_group(name: &str, hidden: bool, children: Vec<Node>) -> Node {
    Node::Group {
        label: "sheet".to_string(),
        name: Some(name.to_string()),
        layer: hidden.then_some(docling_core::ContentLayer::Invisible),
        children,
    }
}

/// Binary workbook (`.xlsb`, issue #210). calamine's `Xlsb` reader serves
/// sheet metadata and cell ranges through the same `Reader` trait as `Xlsx`,
/// so table discovery, value formatting, provenance, hidden-sheet layering
/// and the page-break convention reuse the xlsx machinery. What the binary
/// format does not expose through calamine — drawings, charts, comments,
/// merged regions — degrades to absent rather than failing the conversion.
fn convert_xlsb(
    source: &SourceDocument,
    skip_empty: bool,
) -> Result<DoclingDocument, ConversionError> {
    let cursor = Cursor::new(source.bytes.clone());
    let mut workbook: calamine::Xlsb<_> =
        calamine::Xlsb::new(cursor).map_err(|e| ConversionError::with_source("xlsb", e))?;
    let metas: Vec<(String, calamine::SheetType, calamine::SheetVisible)> = workbook
        .sheets_metadata()
        .iter()
        .map(|s| (s.name.clone(), s.typ, s.visible))
        .collect();

    let mut doc = DoclingDocument::new(&source.name);
    let mut prev_sheet_had_items = false;
    for (name, typ, visible) in &metas {
        if !matches!(typ, calamine::SheetType::WorkSheet) {
            continue;
        }
        let Ok(range) = workbook.worksheet_range(name) else {
            continue;
        };
        // The binary reader exposes no merges, so the frame is the range itself.
        let frame = sheet_frame(&range, &Merges::new());
        let (or, oc) = frame.origin;
        let mut items: Vec<((usize, usize, usize, usize), Node)> = Vec::new();
        // The binary styles part is not read: every cell prints raw (#634).
        for t in find_tables(&range, &frame, skip_empty, &SheetFormats::default(), false) {
            if let Some(label) = t.label {
                items.push((
                    (
                        oc + t.min_c,
                        or + t.min_r.saturating_sub(1),
                        oc + t.max_c + 1,
                        or + t.min_r,
                    ),
                    Node::Paragraph { text: label },
                ));
            }
            items.push((
                (
                    oc + t.min_c,
                    or + t.min_r,
                    oc + t.max_c + 1,
                    or + t.max_r + 1,
                ),
                Node::Table(t.table),
            ));
        }
        if items.is_empty() {
            continue;
        }
        items.sort_by_key(|((_, t, _, _), _)| *t);
        let page_w = items.iter().map(|((_, _, r, _), _)| *r).max().unwrap_or(1);
        let page_h = items.iter().map(|((_, _, _, b), _)| *b).max().unwrap_or(1);
        let hidden = !matches!(visible, calamine::SheetVisible::Visible);
        let mut children = Vec::with_capacity(items.len());
        for ((l, t, r, b), mut node) in items {
            if let Node::Table(table) = &mut node {
                table.location = Some([
                    location_value(l, page_w),
                    location_value(t, page_h),
                    location_value(r, page_w),
                    location_value(b, page_h),
                ]);
            }
            children.push(node);
        }
        doc.push(sheet_group(name, hidden, children));
        // Same trailing page-break convention as the xlsx path above.
        if prev_sheet_had_items {
            doc.push(Node::PageBreak);
        }
        prev_sheet_had_items = true;
    }
    Ok(doc)
}

/// Everything one sheet worker needs, bundled to stay under rayon's closure
/// and keep the call site tidy.
struct SheetCtx<'a, F: Fn(&str, &str) -> Vec<String> + Sync> {
    pkg: Package,
    name: &'a String,
    typ: calamine::SheetType,
    page_ix: usize,
    metas: &'a [(String, calamine::SheetType, calamine::SheetVisible)],
    sheet_parts: &'a HashMap<String, String>,
    ranges: &'a HashMap<String, (Range<Data>, Merges, SheetFormats)>,
    persons: &'a HashMap<String, String>,
    resolve_ref: &'a F,
    skip_empty: bool,
    is_1904: bool,
}

/// Assemble one sheet's `(bbox, node)` items and its comment lines — the
/// per-sheet half of `convert`, run under rayon; the caller merges results
/// in workbook order.
fn sheet_items<F: Fn(&str, &str) -> Vec<String> + Sync>(ctx: SheetCtx<'_, F>) -> SheetItems {
    let SheetCtx {
        mut pkg,
        name,
        typ,
        page_ix,
        metas,
        sheet_parts,
        ranges,
        persons,
        resolve_ref,
        skip_empty,
        is_1904,
    } = ctx;
    let mut comments: Vec<SheetComment> = Vec::new();
    // (bbox in cell units, node) items for this sheet/page.
    let mut items: Vec<((usize, usize, usize, usize), Node)> = Vec::new();

    if matches!(typ, calamine::SheetType::WorkSheet) {
        if let Some((range, abs_merges, formats)) = ranges.get(name) {
            let frame = sheet_frame(range, abs_merges);
            // docling's bboxes are in *absolute* cell indices; calamine's
            // range is clipped to its first non-empty row/column, and the
            // frame reaches back over any merge that starts before it.
            let (or, oc) = frame.origin;
            for mut t in find_tables(range, &frame, skip_empty, formats, is_1904) {
                set_cell_picture_page(&mut t.table, page_ix + 1);
                if let Some(label) = t.label {
                    // The label row sits directly above the table's region.
                    items.push((
                        (
                            oc + t.min_c,
                            or + t.min_r.saturating_sub(1),
                            oc + t.max_c + 1,
                            or + t.min_r,
                        ),
                        Node::Paragraph { text: label },
                    ));
                }
                items.push((
                    (
                        oc + t.min_c,
                        or + t.min_r,
                        oc + t.max_c + 1,
                        or + t.max_r + 1,
                    ),
                    Node::Table(t.table),
                ));
            }
        }
    }

    // Drawings: anchored images and chart frames.
    if let Some(part) = sheet_parts.get(name) {
        let drawing_targets: Vec<String> = pkg
            .rels_for(part)
            .iter()
            .filter(|r| r.rel_type.ends_with("/drawing"))
            .map(|r| resolve(part_dir(part), &r.target))
            .collect();
        for dpath in drawing_targets {
            let Some(dxml) = pkg.read(&dpath) else {
                continue;
            };
            let dimages = pkg.image_rels(&dpath, part_dir(&dpath));
            let drels: HashMap<String, String> = pkg
                .rels_for(&dpath)
                .iter()
                .map(|r| (r.id.clone(), resolve(part_dir(&dpath), &r.target)))
                .collect();
            for item in xlsx_drawings::parse_drawing(&dxml) {
                match item.kind {
                    xlsx_drawings::DrawingKind::Image(rid) => {
                        items.push((
                            item.bbox,
                            Node::Picture {
                                caption: None,
                                caption_href: None,
                                image: dimages.get(&rid).cloned(),
                                classification: None,
                                description: item.alt.map(PictureDescription::alt_text),
                                caption_parent: Default::default(),
                                caption_location: None,
                            },
                        ));
                    }
                    xlsx_drawings::DrawingKind::Chart(rid) => {
                        let Some(cpath) = drels.get(&rid) else {
                            continue;
                        };
                        let Some(cxml) = pkg.read(cpath) else {
                            continue;
                        };
                        let Some(spec) = xlsx_drawings::parse_chart(&cxml) else {
                            continue;
                        };
                        let table = chart_table(&spec, name, &resolve_ref);
                        let Some(table) = table else { continue };
                        items.push((
                            item.bbox,
                            Node::Chart {
                                // The back-most plot's label: one prediction,
                                // where docling gives one per plot type.
                                kind: spec.kinds[0].to_string(),
                                table,
                                caption: spec.title.clone(),
                                location: None,
                            },
                        ));
                    }
                }
            }
        }

        // Cell comments: legacy part order gives the cells; threaded
        // XML (matched by worksheet index) overrides author/time.
        let legacy: Vec<(String, String, String)> = pkg
            .rels_for(part)
            .iter()
            .filter(|r| r.rel_type.ends_with("/comments"))
            .filter_map(|r| pkg.read(&resolve(part_dir(part), &r.target)))
            .flat_map(|xml| xlsx_drawings::parse_legacy_comments(&xml))
            .collect();
        if !legacy.is_empty() {
            let ws_index = metas
                .iter()
                .filter(|(_, t, _)| matches!(t, calamine::SheetType::WorkSheet))
                .position(|(n, _, _)| n == name)
                .map(|i| i + 1)
                .unwrap_or(page_ix + 1);
            let threaded = pkg
                .read(&format!(
                    "xl/threadedComments/threadedComment{ws_index}.xml"
                ))
                .map(|xml| xlsx_drawings::parse_threaded_comments(&xml, persons))
                .unwrap_or_default();
            // Row-major over commented cells (docling scans the grid). A
            // threaded cell yields one line per message of the thread, in
            // thread order (docling#4353); the legacy comment (which holds
            // the thread flattened into one blob) only stands in when the
            // cell has no threaded messages.
            let mut cells: Vec<(usize, usize, usize, String, String)> = legacy
                .iter()
                .filter_map(|(cell, author, text)| {
                    let (c, r) = cell_ref_pub(cell)?;
                    let lines: Vec<String> = match threaded.get(cell) {
                        Some(thread) if !thread.is_empty() => thread
                            .iter()
                            .map(|(a, t, time)| match time {
                                Some(ts) => format!("[author: {a}, time: {ts}]: {t}"),
                                None => format!("[author: {a}]: {t}"),
                            })
                            .collect(),
                        _ => vec![format!("[author: {author}]: {text}")],
                    };
                    Some((r, c, cell.clone(), lines))
                })
                .flat_map(|(r, c, cell, lines)| {
                    lines
                        .into_iter()
                        .enumerate()
                        .map(move |(k, line)| (r, c, k, cell.clone(), line))
                })
                .collect();
            cells.sort_by_key(|(r, c, k, _, _)| (*r, *c, *k));
            let cells: Vec<SheetComment> = cells
                .into_iter()
                .map(|(r, c, _, cell, line)| (r, c, cell, line))
                .collect();
            comments.extend(cells);
        }
    }

    (items, comments)
}

/// The directory of an OPC part path (`xl/worksheets/sheet1.xml` → `xl/worksheets`).
fn part_dir(part: &str) -> &str {
    part.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// Public wrapper for `xlsx_drawings`' cell-ref parser (`B7` → `(col, row)`).
fn cell_ref_pub(cell: &str) -> Option<(usize, usize)> {
    let (sheet, (c, r, _, _)) = xlsx_drawings::parse_range_ref(cell)?;
    if sheet.is_some() {
        return None;
    }
    Some((c, r))
}

/// docling's `_chart_to_table_data` (docling#4812): the series of every
/// plot as columns, the categories of the first series with a category
/// reference down the first column under its plot's category axis title,
/// laid out by `build_chart_table`. Each reference reads at most its share
/// of the table's cell cap.
fn chart_table(
    spec: &xlsx_drawings::ChartSpec,
    own_sheet: &str,
    resolve_ref: &dyn Fn(&str, &str) -> Vec<String>,
) -> Option<Table> {
    if spec.series.is_empty() {
        return None;
    }
    let column_cells = (xlsx_drawings::MAX_CHART_TABLE_CELLS / (1 + spec.series.len())).max(1);
    let read = |reference: &str, max: usize| {
        let mut cells = resolve_ref(reference, own_sheet);
        cells.truncate(max);
        cells
    };
    let (categories, corner) = spec
        .series
        .iter()
        .find_map(|s| {
            let r = s.cat_ref.as_deref()?;
            Some((read(r, column_cells), s.axis_title.clone()))
        })
        .unwrap_or_default();
    let columns: Vec<(String, Vec<String>)> = spec
        .series
        .iter()
        .map(|s| {
            let values = s
                .val_ref
                .as_deref()
                .map(|r| read(r, column_cells))
                .unwrap_or_default();
            let name = match &s.name_ref {
                Some(r) => read(r, 1).into_iter().next().unwrap_or_default(),
                None => s.name_lit.clone().unwrap_or_default(),
            };
            (name, values)
        })
        .collect();
    xlsx_drawings::build_chart_table(categories, columns, corner)
}

/// Parse `<sheet name="…" r:id="…">` entries from `workbook.xml`, in order.
fn workbook_sheets(xml: &str) -> Vec<(String, String)> {
    let mut reader = XmlReader::from_str(xml);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.name().as_ref() == b"sheet" => {
                let (mut name, mut rid) = (String::new(), String::new());
                for attr in e.attributes().flatten() {
                    let value = String::from_utf8_lossy(attr.value.as_ref()).into_owned();
                    match attr.key.as_ref() {
                        b"name" => name = value,
                        b"r:id" => rid = value,
                        _ => {}
                    }
                }
                out.push((name, rid));
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Find every contiguous data region in a sheet (flood fill, strict adjacency —
/// The DocLang location resolution (docling's default `xsize`/`ysize`).
const LOC_RESOLUTION: u32 = 512;

/// Normalize a cell-index coordinate against the sheet extent and quantize it to
/// the DocLang location grid — `clamp(round(512 * coord / page), 0, 511)`,
/// matching docling's `_create_location_tokens_for_bbox` + `_quantize_to_resolution`.
pub(crate) fn location_value(coord: usize, page: usize) -> u16 {
    if page == 0 {
        return 0;
    }
    let v = (LOC_RESOLUTION as f64 * coord as f64 / page as f64).round() as i64;
    v.clamp(0, LOC_RESOLUTION as i64 - 1) as u16
}

/// The cell frame one sheet is scanned in — docling's `_find_true_data_bounds`:
/// the union of the cells carrying a value and *every* merged range, so a merge
/// that starts above or to the left of the first value still fits.
///
/// calamine's `Range` is clipped to the first non-empty row/column, and merges
/// are absolute, so rebasing a merge on the range origin underflowed for a
/// merge that begins before any data (#395: `attempt to subtract with
/// overflow`, an empty `A1:C1` above a table starting at `A3`). The frame
/// origin is the minimum of the two instead, and `shift` records how far it
/// reaches beyond the range so cell lookups can rebase back.
pub(crate) struct SheetFrame {
    /// Rows/columns the frame extends above/left of calamine's range origin.
    pub(crate) shift: (usize, usize),
    /// Frame origin in absolute cell coordinates (docling's bbox space).
    pub(crate) origin: (usize, usize),
    pub(crate) height: usize,
    pub(crate) width: usize,
    /// Frame position → the top-left of the merge covering it.
    pub(crate) merge_of: HashMap<(usize, usize), (usize, usize)>,
}

pub(crate) fn sheet_frame(range: &Range<Data>, merges: &Merges) -> SheetFrame {
    let (rs_r, rs_c) = range.start().unwrap_or((0, 0));
    // Reach back to the earliest merge start, never past cell (0, 0).
    let (mut or, mut oc) = (rs_r, rs_c);
    for &((sr, sc), _) in merges {
        or = or.min(sr);
        oc = oc.min(sc);
    }
    let shift = ((rs_r - or) as usize, (rs_c - oc) as usize);
    let mut merge_of: HashMap<(usize, usize), (usize, usize)> = HashMap::new();
    for &((sr, sc), (er, ec)) in merges {
        let tl = ((sr - or) as usize, (sc - oc) as usize);
        for r in sr..=er {
            for c in sc..=ec {
                merge_of.insert(((r - or) as usize, (c - oc) as usize), tl);
            }
        }
    }
    let (rh, rw) = range.get_size();
    let height = (rh + shift.0).max(merge_of.keys().map(|(r, _)| r + 1).max().unwrap_or(0));
    let width = (rw + shift.1).max(merge_of.keys().map(|(_, c)| c + 1).max().unwrap_or(0));
    SheetFrame {
        shift,
        origin: (or as usize, oc as usize),
        height,
        width,
        merge_of,
    }
}

/// A discovered table with its cell-index bounding box (inclusive), used to
/// compute the DocLang `<location>` provenance.
pub(crate) struct FoundTable {
    pub(crate) table: Table,
    /// A "section label" split off the region's first row (docling PR #3727):
    /// a single merged cell spanning several columns directly above a real
    /// header row is a caption, not part of the table — it emits as a
    /// separate paragraph and the table starts at the next row.
    pub(crate) label: Option<String>,
    pub(crate) min_r: usize,
    pub(crate) min_c: usize,
    pub(crate) max_r: usize,
    pub(crate) max_c: usize,
}

/// docling's default `gap_tolerance = 0`), in row-major discovery order. A cell
/// covered by a merge counts as content even if its own value is empty.
///
/// `skip_empty` (#271, opt-in — docling materialises the full box too): omit
/// empty, non-merge-covered positions from each row instead of padding the
/// region's bounding box. A ragged/diagonal region's box is mostly padding —
/// a sparse sheet inflated ~7× over its content — and every flood-fill row
/// holds at least one content cell, so whole rows never vanish. Rows keep
/// their surviving cells in column order; a table that loses cells this way
/// also drops its span/structure overlay (the grid positions no longer line
/// up), while a dense region is byte-identical to the default path.
pub(crate) fn find_tables(
    range: &Range<Data>,
    frame: &SheetFrame,
    skip_empty: bool,
    formats: &SheetFormats,
    is_1904: bool,
) -> Vec<FoundTable> {
    let SheetFrame {
        shift,
        origin,
        height,
        width,
        merge_of,
    } = frame;
    let (height, width) = (*height, *width);
    // A frame position rebased onto calamine's clipped range — `None` where the
    // frame reaches above/left of it (rows only a merge extends into).
    let value_at = |r: usize, c: usize| -> Option<&Data> {
        range.get((r.checked_sub(shift.0)?, c.checked_sub(shift.1)?))
    };
    let has_value =
        |r: usize, c: usize| -> bool { value_at(r, c).is_some_and(|d| !matches!(d, Data::Empty)) };
    let has_content =
        |r: usize, c: usize| -> bool { merge_of.contains_key(&(r, c)) || has_value(r, c) };
    // A grid position renders the value of its merge's top-left cell, if
    // merged — through that cell's number format (#634; absolute coordinates
    // are the frame's origin plus the position).
    // The image placed in a grid position's cell (docling#5015) — only an
    // error cell holds one: its `#VALUE!` is the fallback for readers without
    // rich data, and docling keys the lookup on the error type the same way.
    let image_at = |r: usize, c: usize| -> Option<&Arc<xlsx_richdata::CellImage>> {
        let (sr, sc) = merge_of.get(&(r, c)).copied().unwrap_or((r, c));
        if !matches!(value_at(sr, sc), Some(Data::Error(_))) {
            return None;
        }
        formats
            .images
            .get(&((origin.0 + sr) as u32, (origin.1 + sc) as u32))
    };
    let cell_text = |r: usize, c: usize| -> String {
        if image_at(r, c).is_some() {
            // An image cell's text is empty, as docling's `ExcelCell`; the
            // picture is the cell's content (see `cell_blocks` below).
            return String::new();
        }
        let (sr, sc) = merge_of.get(&(r, c)).copied().unwrap_or((r, c));
        value_at(sr, sc)
            .map(|d| numfmt::displayed_text(d, formats.at(origin.0 + sr, origin.1 + sc), is_1904))
            .unwrap_or_default()
    };

    let mut visited: HashSet<(usize, usize)> = HashSet::new();
    let mut tables = Vec::new();

    for r in 0..height {
        for c in 0..width {
            // docling seeds a table only from a cell that carries a value
            // (`if cell.value is None: continue`); a merge is absorbed by the
            // flood fill below, but an empty one never starts a table of its
            // own (#395).
            if !has_value(r, c) || visited.contains(&(r, c)) {
                continue;
            }
            // Flood fill from this seed over 4-connected content cells.
            let mut stack = vec![(r, c)];
            let mut cells: HashSet<(usize, usize)> = HashSet::new();
            cells.insert((r, c));
            let (mut min_r, mut max_r, mut min_c, mut max_c) = (r, r, c, c);
            // (min_r may advance past a section-label row below.)
            while let Some((cr, cc)) = stack.pop() {
                min_r = min_r.min(cr);
                max_r = max_r.max(cr);
                min_c = min_c.min(cc);
                max_c = max_c.max(cc);
                let neighbors = [
                    (cr.wrapping_sub(1), cc),
                    (cr + 1, cc),
                    (cr, cc.wrapping_sub(1)),
                    (cr, cc + 1),
                ];
                for (nr, nc) in neighbors {
                    if nr < height && nc < width && has_content(nr, nc) && cells.insert((nr, nc)) {
                        stack.push((nr, nc));
                    }
                }
            }
            visited.extend(&cells);
            // The table is the region's full bounding rectangle, gaps and
            // disconnected non-empty cells included, so the whole rectangle is
            // visited too — otherwise a stray cell inside it seeds a second,
            // duplicate fragment table (docling#4302).
            for vr in min_r..=max_r {
                for vc in min_c..=max_c {
                    visited.insert((vr, vc));
                }
            }

            // Split a leading "section label" off the region (docling's
            // `_split_leading_section_label`, PR #3727): the region is at
            // least 2×2, its first row holds exactly one non-empty cell that
            // starts at the region's first column and spans >1 columns (but
            // only 1 row), and the second row reads as a real header (≥2
            // non-empty unmerged cells). The label becomes a separate
            // paragraph; the table starts at the next row.
            let mut label: Option<String> = None;
            if max_r > min_r && max_c > min_c {
                let first_row_cells: Vec<(usize, usize)> = (min_c..=max_c)
                    .map(|c| merge_of.get(&(min_r, c)).copied().unwrap_or((min_r, c)))
                    // An image cell is content (docling: `cell.text.strip()
                    // or cell.image is not None`); the header test below
                    // counts text cells only, as docling's does.
                    .filter(|&(r, c)| {
                        !cell_text(r, c).trim().is_empty() || image_at(r, c).is_some()
                    })
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                if let [(lr, lc)] = first_row_cells[..] {
                    let span_cols = (min_c..=max_c)
                        .filter(|&c| merge_of.get(&(min_r, c)) == Some(&(lr, lc)))
                        .count();
                    let spans_rows = merge_of.get(&(min_r + 1, lc)) == Some(&(lr, lc));
                    let header_cells = (min_c..=max_c)
                        .filter(|&c| {
                            !merge_of.contains_key(&(min_r + 1, c))
                                && !cell_text(min_r + 1, c).trim().is_empty()
                        })
                        .count();
                    if (lr, lc) == (min_r, min_c)
                        && span_cols > 1
                        && !spans_rows
                        && header_cells >= 2
                    {
                        label = Some(cell_text(lr, lc));
                        min_r += 1;
                    }
                }
            }

            // Materialise the region's bounding box; gaps become empty cells —
            // or, with `skip_empty`, only the occupied positions per row.
            let mut omitted = false;
            // An image cell is a `RichTableCell` holding the picture
            // (docling#5015): its blocks are the picture with the cell's own
            // box as exact provenance (the page is the sheet's, set by the
            // caller), and its flat text is what docling's Markdown prints
            // for it — the placeholder, then the alt-text description (the
            // picture's `meta`, which docling-core serializes after the item),
            // the blocks' blank line flattened to two spaces.
            let mut cell_blocks: Vec<Vec<Vec<Node>>> = Vec::new();
            let mut any_image = false;
            let rows: Vec<Vec<String>> = (min_r..=max_r)
                .map(|gr| {
                    let mut blocks_row = Vec::new();
                    let row = (min_c..=max_c)
                        .filter(|&gc| {
                            if skip_empty && !has_content(gr, gc) {
                                omitted = true;
                                return false;
                            }
                            true
                        })
                        .map(|gc| {
                            let Some(img) = image_at(gr, gc) else {
                                blocks_row.push(Vec::new());
                                return cell_text(gr, gc);
                            };
                            any_image = true;
                            let (tr, tc) = merge_of.get(&(gr, gc)).copied().unwrap_or((gr, gc));
                            let anchor = (gr, gc) == (tr.max(min_r), tc.max(min_c));
                            blocks_row.push(if anchor {
                                vec![cell_picture(img, frame, (gr, gc), (max_r, max_c))]
                            } else {
                                Vec::new()
                            });
                            match img.alt_text.as_deref() {
                                Some(alt) => format!("<!-- image -->  {alt}"),
                                None => "<!-- image -->".to_string(),
                            }
                        })
                        .collect();
                    cell_blocks.push(blocks_row);
                    row
                })
                .collect();
            // Merge spans as OTSL continuations (docling's table cells carry
            // row/col spans from the merged regions): a covered cell continues
            // the span horizontally (`<lcel/>`), vertically (`<ucel/>`), or
            // both (`<xcel/>`) relative to the merge's top-left.
            let nrows = max_r - min_r + 1;
            let ncols = max_c - min_c + 1;
            let mut col_cont = vec![vec![false; ncols]; nrows];
            let mut row_cont = vec![vec![false; ncols]; nrows];
            let mut any_span = false;
            for gr in min_r..=max_r {
                for gc in min_c..=max_c {
                    if let Some(&(tr, tc)) = merge_of.get(&(gr, gc)) {
                        if (gr, gc) == (tr, tc) {
                            continue;
                        }
                        any_span = true;
                        if gc > tc {
                            col_cont[gr - min_r][gc - min_c] = true;
                        }
                        if gr > tr && gc == tc {
                            row_cont[gr - min_r][gc - min_c] = true;
                        }
                        if gr > tr && gc > tc {
                            // A 2-D covered cell is both (`<xcel/>`).
                            row_cont[gr - min_r][gc - min_c] = true;
                        }
                    }
                }
            }
            let structure = (any_span && !omitted).then(|| {
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
            // A compacted table keeps its geometry for the JSON through
            // first-class cells: the surviving positions at their true grid
            // offsets, one cell per merged range with its span — what the
            // default path's overlay yields (#410), minus the empty
            // positions, which is what `skip_empty` promises. The ragged
            // `rows` (Markdown, DocLang) cannot carry offsets, and padding
            // them back at export made the "sparse" JSON *larger* than the
            // dense one (16 cells for an 8-column row that holds three).
            let cells = (skip_empty && omitted).then(|| {
                let mut out = Vec::new();
                for gr in min_r..=max_r {
                    for gc in min_c..=max_c {
                        if !has_content(gr, gc) {
                            continue;
                        }
                        let (tr, tc) = merge_of.get(&(gr, gc)).copied().unwrap_or((gr, gc));
                        // A merge is one cell, anchored where it enters the
                        // box (its top-left may sit above a split-off label
                        // row or left of the clipped range).
                        if (gr, gc) != (tr.max(min_r), tc.max(min_c)) {
                            continue;
                        }
                        let covered = |r: usize, c: usize| merge_of.get(&(r, c)) == Some(&(tr, tc));
                        out.push(TableCell {
                            text: cell_text(gr, gc),
                            bbox: None,
                            start_row: gr - min_r,
                            start_col: gc - min_c,
                            row_span: (gr..=max_r).take_while(|&r| covered(r, gc)).count().max(1),
                            col_span: (gc..=max_c).take_while(|&c| covered(gr, c)).count().max(1),
                            column_header: gr == min_r,
                            row_header: false,
                            row_section: false,
                        });
                    }
                }
                out
            });
            tables.push(FoundTable {
                table: Table {
                    footnotes: Vec::new(),
                    rows,
                    location: None,
                    structure,
                    cell_blocks: any_image.then_some(cell_blocks),
                    cells,
                    caption: None,
                    caption_parent: Default::default(),
                    caption_location: None,
                    document_index: false,
                },
                label,
                min_r,
                min_c,
                max_r,
                max_c,
            });
        }
    }
    tables
}

/// The block content of an image cell (docling's `_add_cell_image`): the
/// picture, its alt text as docling's own `meta.description` (not the opt-in
/// #676 alt text of a drawing — docling#5015 writes this one itself), wrapped in the exact provenance
/// of the cell's box — absolute cell indices, top-left origin, spans
/// included, like every sheet item's. `page_no` is left 0 for the sheet's
/// caller to fill ([`set_cell_picture_page`]). The JSON export turns this
/// shape — a cell whose blocks are one [`Node::Prov`]-wrapped picture — into
/// a picture item under the table that the cell's `ref` points at.
fn cell_picture(
    img: &xlsx_richdata::CellImage,
    frame: &SheetFrame,
    (gr, gc): (usize, usize),
    (max_r, max_c): (usize, usize),
) -> Node {
    let (tr, tc) = frame.merge_of.get(&(gr, gc)).copied().unwrap_or((gr, gc));
    let covered = |r: usize, c: usize| frame.merge_of.get(&(r, c)) == Some(&(tr, tc));
    let row_span = (gr..=max_r).take_while(|&r| covered(r, gc)).count().max(1);
    let col_span = (gc..=max_c).take_while(|&c| covered(gr, c)).count().max(1);
    let (or, oc) = frame.origin;
    let (l, t) = ((oc + gc) as f32, (or + gr) as f32);
    Node::Prov {
        page_no: 0,
        bbox: [l, t, l + col_span as f32, t + row_span as f32],
        charspan: [0, 0],
        bottom_left: false,
        seq: None,
        inner: Box::new(Node::Picture {
            caption: None,
            caption_href: None,
            image: img.image.clone(),
            classification: None,
            description: img.alt_text.clone().map(PictureDescription::docling_meta),
            caption_parent: Default::default(),
            caption_location: None,
        }),
    }
}

/// Stamp the sheet's page on the in-cell pictures [`cell_picture`] built.
fn set_cell_picture_page(table: &mut Table, page: usize) {
    for cell in table.cell_blocks.iter_mut().flatten().flatten().flatten() {
        if let Node::Prov { page_no, .. } = cell {
            *page_no = page;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::DeclarativeBackend;
    use crate::{InputFormat, SourceDocument};

    /// Every sheet's items live inside that sheet's group, so tests that look
    /// for a node by kind flatten the groups away first.
    /// The item nodes, looking through sheet groups and the provenance /
    /// comment wrappers each item sits in.
    fn flatten(nodes: &[Node]) -> Vec<&Node> {
        nodes
            .iter()
            .flat_map(|n| match n {
                Node::Group { children, .. } => flatten(children),
                Node::Prov { inner, .. } | Node::Commented { inner, .. } => {
                    flatten(std::slice::from_ref(inner))
                }
                other => vec![other],
            })
            .collect()
    }

    /// #395: calamine clips a sheet's range to its first non-empty row/column,
    /// so a merge that begins before any data used to underflow the subtraction
    /// that rebased it (`attempt to subtract with overflow`). The frame reaches
    /// back to the earliest merge instead — docling's `_find_true_data_bounds`.
    #[test]
    fn a_merge_before_the_data_does_not_underflow_the_frame() {
        // Data at C3:D4, an empty merge at A1:B2 — above *and* left of it.
        let mut range: Range<Data> = Range::new((2, 2), (3, 3));
        range.set_value((2, 2), Data::String("Name".into()));
        range.set_value((2, 3), Data::String("Qty".into()));
        range.set_value((3, 2), Data::String("Bolt".into()));
        range.set_value((3, 3), Data::Int(4));
        let frame = sheet_frame(&range, &vec![((0, 0), (1, 1))]);
        assert_eq!(frame.origin, (0, 0), "the frame starts at the merge");
        assert_eq!(frame.shift, (2, 2), "two rows/columns before the range");
        assert_eq!((frame.height, frame.width), (4, 4));
        assert_eq!(frame.merge_of.get(&(0, 0)), Some(&(0, 0)));
        assert_eq!(frame.merge_of.get(&(1, 1)), Some(&(0, 0)));

        // An *empty* merge is not a table of its own: docling seeds a table
        // only from a cell that carries a value, so only the data is found —
        // and it keeps its absolute position in the frame.
        let found = find_tables(&range, &frame, false, &SheetFormats::default(), false);
        assert_eq!(found.len(), 1, "the empty merge seeds nothing");
        assert_eq!(
            found[0].table.rows,
            vec![vec!["Name", "Qty"], vec!["Bolt", "4"]]
        );
        assert_eq!(
            (
                found[0].min_r,
                found[0].min_c,
                found[0].max_r,
                found[0].max_c
            ),
            (2, 2, 3, 3)
        );
    }

    /// The reporter's own geometry (#395): an empty `B1:N1` above *and to the
    /// right of* the only value, in `A2`. The row axis underflowed; the column
    /// axis had to grow instead.
    #[test]
    fn a_merge_above_and_right_of_the_data_keeps_both_axes() {
        let mut range: Range<Data> = Range::new((1, 0), (1, 0));
        range.set_value((1, 0), Data::String("data".into()));
        let frame = sheet_frame(&range, &vec![((0, 1), (0, 13))]);
        assert_eq!(
            frame.origin,
            (0, 0),
            "up to the merge's row, out to column A"
        );
        assert_eq!(frame.shift, (1, 0), "one row before the range, no columns");
        assert_eq!(
            (frame.height, frame.width),
            (2, 14),
            "the merge's row above, and out to column N"
        );
        // Only the valued cell is a table; the empty merge beside it is not.
        let found = find_tables(&range, &frame, false, &SheetFormats::default(), false);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].table.rows, vec![vec!["data"]]);
        assert_eq!((found[0].min_r, found[0].min_c), (1, 0));
    }

    /// Without merges the frame is calamine's range, unshifted — the ordinary
    /// case, and the one the binary (xlsb) reader always takes.
    #[test]
    fn a_sheet_without_merges_keeps_the_range_frame() {
        let mut range: Range<Data> = Range::new((1, 1), (2, 2));
        range.set_value((1, 1), Data::String("a".into()));
        let frame = sheet_frame(&range, &Merges::new());
        assert_eq!(frame.origin, (1, 1));
        assert_eq!(frame.shift, (0, 0));
        assert_eq!((frame.height, frame.width), (2, 2));
        assert!(frame.merge_of.is_empty());
    }

    /// #271: `skip_empty` omits empty positions from each row of a ragged
    /// region instead of padding its bounding box; a dense region (or the
    /// default mode) is untouched, and a table that lost cells drops its
    /// span/structure overlay.
    #[test]
    fn skip_empty_compacts_ragged_regions() {
        // A 3×3 "staircase": (0,0)-(0,1), (1,1), (2,1)-(2,2) — connected via
        // column 1, bounding box 3×3 with 4 empty positions.
        let mut range: Range<Data> = Range::new((0, 0), (2, 2));
        range.set_value((0, 0), Data::String("a".into()));
        range.set_value((0, 1), Data::String("b".into()));
        range.set_value((1, 1), Data::String("c".into()));
        range.set_value((2, 1), Data::String("d".into()));
        range.set_value((2, 2), Data::String("e".into()));
        let frame = |merge_of: HashMap<(usize, usize), (usize, usize)>,
                     height: usize,
                     width: usize| SheetFrame {
            shift: (0, 0),
            origin: (0, 0),
            height,
            width,
            merge_of,
        };

        let padded = find_tables(
            &range,
            &frame(HashMap::new(), 3, 3),
            false,
            &SheetFormats::default(),
            false,
        );
        assert_eq!(
            padded[0].table.rows,
            vec![vec!["a", "b", ""], vec!["", "c", ""], vec!["", "d", "e"]],
            "default: the full bounding box materialises"
        );

        let compact = find_tables(
            &range,
            &frame(HashMap::new(), 3, 3),
            true,
            &SheetFormats::default(),
            false,
        );
        assert_eq!(
            compact[0].table.rows,
            vec![vec!["a", "b"], vec!["c"], vec!["d", "e"]],
            "skip_empty: rows keep only their occupied cells"
        );
        // …and the JSON keeps their true offsets through first-class cells.
        let placed: Vec<(&str, usize, usize, bool)> = compact[0]
            .table
            .cells
            .as_deref()
            .expect("a compacted table carries first-class cells")
            .iter()
            .map(|c| (c.text.as_str(), c.start_row, c.start_col, c.column_header))
            .collect();
        assert_eq!(
            placed,
            vec![
                ("a", 0, 0, true),
                ("b", 0, 1, true),
                ("c", 1, 1, false),
                ("d", 2, 1, false),
                ("e", 2, 2, false),
            ]
        );
        assert!(
            padded[0].table.cells.is_none(),
            "the default path keeps the overlay representation"
        );
        // Provenance keeps the true region box either way.
        assert_eq!(
            (
                compact[0].min_r,
                compact[0].min_c,
                compact[0].max_r,
                compact[0].max_c
            ),
            (0, 0, 2, 2)
        );

        // A merge-covered position counts as content (span continuation text
        // repeats), and a dense region keeps its structure overlay untouched.
        // (Vertical merge in column 0 — a full-width merge above a header row
        // would trip the #3727 section-label split instead.)
        let mut merged: HashMap<(usize, usize), (usize, usize)> = HashMap::new();
        merged.insert((0, 0), (0, 0));
        merged.insert((1, 0), (0, 0));
        let mut range2: Range<Data> = Range::new((0, 0), (1, 1));
        range2.set_value((0, 0), Data::String("tall".into()));
        range2.set_value((0, 1), Data::String("a".into()));
        range2.set_value((1, 1), Data::String("b".into()));
        let dense = find_tables(
            &range2,
            &frame(merged, 2, 2),
            true,
            &SheetFormats::default(),
            false,
        );
        assert_eq!(
            dense[0].table.rows,
            vec![vec!["tall", "a"], vec!["tall", "b"]],
            "nothing to omit: identical to the default grid"
        );
        assert!(
            dense[0].table.structure.is_some(),
            "dense region keeps its span overlay under skip_empty"
        );
        assert!(dense[0].table.cells.is_none());

        // A ragged region with a merge: the compacted rows repeat the span
        // text, the first-class cells carry the range once with its span —
        // a 3-wide merge on row 1 under a header row holding two of the
        // three columns.
        let mut merged: HashMap<(usize, usize), (usize, usize)> = HashMap::new();
        for c in 0..3 {
            merged.insert((1, c), (1, 0));
        }
        let mut range3: Range<Data> = Range::new((0, 0), (1, 2));
        range3.set_value((0, 0), Data::String("h0".into()));
        range3.set_value((0, 2), Data::String("h2".into()));
        range3.set_value((1, 0), Data::String("wide".into()));
        let ragged = find_tables(
            &range3,
            &frame(merged, 2, 3),
            true,
            &SheetFormats::default(),
            false,
        );
        assert_eq!(
            ragged[0].table.rows,
            vec![vec!["h0", "h2"], vec!["wide", "wide", "wide"]]
        );
        assert!(ragged[0].table.structure.is_none());
        let cells = ragged[0].table.cells.as_deref().unwrap();
        let shaped: Vec<(&str, usize, usize, usize, usize)> = cells
            .iter()
            .map(|c| {
                (
                    c.text.as_str(),
                    c.start_row,
                    c.start_col,
                    c.row_span,
                    c.col_span,
                )
            })
            .collect();
        assert_eq!(
            shaped,
            vec![("h0", 0, 0, 1, 1), ("h2", 0, 2, 1, 1), ("wide", 1, 0, 1, 3)]
        );
    }

    /// A minimal workbook with one sheet whose cells sit at `A1` and at `far`.
    fn tiny_xlsx(far: &str) -> Vec<u8> {
        use std::io::Write;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let parts: [(&str, String); 5] = [
            ("[Content_Types].xml", r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#.into()),
            ("_rels/.rels", r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.into()),
            ("xl/workbook.xml", r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sheet" sheetId="1" r:id="rId1"/></sheets></workbook>"#.into()),
            ("xl/_rels/workbook.xml.rels", r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#.into()),
            ("xl/worksheets/sheet1.xml", format!(r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c></row><row><c r="{far}" t="inlineStr"><is><t>b</t></is></c></row></sheetData></worksheet>"#)),
        ];
        for (name, body) in parts {
            zw.start_file(name, opts).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        zw.finish().unwrap().into_inner()
    }

    /// A 4 KB workbook with a value in `A1` and one in `XFD1048576` spans 17
    /// billion cells: calamine's dense grid for it is a 512 GB allocation
    /// that aborted the process. The used area is checked first and the
    /// sheet skipped; a compact sheet still converts.
    #[test]
    fn oversized_used_area_skips_the_sheet_instead_of_allocating() {
        let src = SourceDocument::from_bytes("x.xlsx", InputFormat::Xlsx, tiny_xlsx("XFD1048576"));
        let doc = XlsxBackend::default().convert(&src).expect("converts");
        // (Items sit inside the sheet group — look through it.)
        assert!(
            !flatten(&doc.nodes)
                .iter()
                .any(|n| matches!(n, Node::Table(_))),
            "{:?}",
            doc.nodes
        );
        // (`A2`, not `B2`: diagonal cells are two regions under docling's
        // 4-neighbour flood fill.)
        let src = SourceDocument::from_bytes("x.xlsx", InputFormat::Xlsx, tiny_xlsx("A2"));
        let doc = XlsxBackend::default().convert(&src).expect("converts");
        let table = flatten(&doc.nodes).into_iter().find_map(|n| match n {
            Node::Table(t) => Some(t),
            _ => None,
        });
        assert_eq!(table.map(|t| t.rows.len()), Some(2), "{:?}", doc.nodes);
    }

    /// docling's sheet groups and cell comments: each worksheet becomes a
    /// `sheet` group named after it, and every cell note becomes a
    /// `comment-{sheet}-{cell}` section that the item covering the cell points
    /// at (docling's `_find_cell_item`).
    #[test]
    fn sheet_groups_and_cell_comments() {
        let path = format!(
            "{}/../../tests/data/xlsx/sources/xlsx_comments.xlsx",
            env!("CARGO_MANIFEST_DIR")
        );
        let bytes = std::fs::read(&path).expect("fixture exists");
        let src = SourceDocument::from_bytes("x.xlsx", InputFormat::Xlsx, bytes);
        let doc = XlsxBackend::default().convert(&src).expect("converts");

        // The sheet's page marker leads; the group follows it.
        let Some(Node::Group {
            label,
            name,
            layer,
            children,
        }) = doc.nodes.iter().find(|n| matches!(n, Node::Group { .. }))
        else {
            panic!("no sheet group in {:?}", doc.nodes);
        };
        assert_eq!(
            (label.as_str(), name.as_deref(), *layer),
            ("sheet", Some("Sheet1"), None)
        );
        assert_eq!(children.len(), 3, "the sheet's three tables");

        // The comment sections follow the sheets, in row-major cell order; a
        // threaded cell contributes one section node per message (the JSON
        // export folds same-named neighbours into one group, docling#4353).
        let sections: Vec<&str> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::CommentSection { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            sections,
            [
                "comment-Sheet1-A1",
                "comment-Sheet1-B2",
                "comment-Sheet1-F7",
                "comment-Sheet1-F7",
                "comment-Sheet1-G12"
            ]
        );
        // A1 annotates the first table, B2 the second, F7 and G12 both land in
        // the third — the item whose cell range covers each commented cell.
        let annotated: Vec<Vec<usize>> = children
            .iter()
            .map(|c| match c {
                Node::Prov { inner, .. } => match inner.as_ref() {
                    Node::Commented { comments, .. } => comments.clone(),
                    _ => Vec::new(),
                },
                Node::Commented { comments, .. } => comments.clone(),
                _ => Vec::new(),
            })
            .collect();
        // The third table is annotated by both F7 messages and G12's.
        assert_eq!(annotated, vec![vec![0], vec![1], vec![2, 3, 4]]);
    }

    /// docling#5015 (`test_images_placed_in_cells`): an image placed in a
    /// cell is a picture under the table, which the cell — a
    /// `RichTableCell` with empty text — refers to; Excel's alt text is the
    /// picture's description. An error value without an image keeps its
    /// text, and a lone image cell is a one-cell table of it.
    #[test]
    fn images_placed_in_cells_are_rich_cell_pictures() {
        let path = format!(
            "{}/../../tests/data/xlsx/sources/xlsx_10_place_in_cell.xlsx",
            env!("CARGO_MANIFEST_DIR")
        );
        let bytes = std::fs::read(&path).expect("fixture exists");
        let src = SourceDocument::from_bytes("x.xlsx", InputFormat::Xlsx, bytes);
        let doc = XlsxBackend::default().convert(&src).expect("converts");
        let json = doc.export_to_json_value();
        let table = &json["tables"][0];
        let cells = table["data"]["table_cells"].as_array().unwrap();
        let cell = |r: u64, c: u64| {
            cells
                .iter()
                .find(|x| x["start_row_offset_idx"] == r && x["start_col_offset_idx"] == c)
                .unwrap()
        };
        for (row, pic, alt) in [(1, 0, "A red square"), (2, 1, "A green square")] {
            let cell = cell(row, 1);
            assert_eq!(cell["text"], "");
            let pic_ref = format!("#/pictures/{pic}");
            assert_eq!(cell["ref"]["$ref"], pic_ref.as_str());
            let picture = &json["pictures"][pic];
            assert_eq!(picture["parent"]["$ref"], "#/tables/0");
            assert!(picture["image"]["uri"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png"));
            // docling's own description: text only, no legacy annotation.
            assert_eq!(
                picture["meta"]["description"],
                serde_json::json!({ "text": alt })
            );
            assert_eq!(picture["annotations"], serde_json::json!([]));
            // The picture's box is its cell's, on the sheet's page.
            assert_eq!(picture["prov"][0]["page_no"], 1);
            assert_eq!(picture["prov"][0]["bbox"]["t"], row as f64);
            assert_eq!(picture["prov"][0]["bbox"]["l"], 1.0);
        }
        assert_eq!(
            table["children"],
            serde_json::json!([{ "$ref": "#/pictures/0" }, { "$ref": "#/pictures/1" }])
        );
        // An error value without an image is cell content.
        assert!(cell(1, 3).get("ref").is_none());
        assert!(!cell(1, 3)["text"].as_str().unwrap().is_empty());
        // The "Logo" sheet: one image cell, a one-cell table holding it.
        let logo = &json["tables"][1];
        assert_eq!(
            logo["data"]["table_cells"][0]["ref"]["$ref"],
            "#/pictures/2"
        );
        assert_eq!(json["pictures"][2]["prov"][0]["page_no"], 2);
        assert!(json["pictures"][2]["meta"].is_null());

        let md = doc.export_to_markdown();
        assert!(
            md.contains("| apple  | <!-- image -->  A red square   |"),
            "{md}"
        );
        assert!(md.contains("| <!-- image -->   |"), "{md}");
    }

    /// docling#5015 (`test_image_cell_next_to_a_section_label`): an image
    /// cell in the first row is content, so a merged label beside it is not
    /// split off as a section label; the header test counts text cells only.
    #[test]
    fn an_image_cell_keeps_a_merged_label_in_its_table() {
        let mut range: Range<Data> = Range::new((0, 0), (3, 2));
        range.set_value((0, 0), Data::String("Fruit list".into()));
        range.set_value((0, 2), Data::Error(calamine::CellErrorType::Value));
        for (r, row) in [
            ["name", "qty", "note"],
            ["apple", "1", "x"],
            ["pear", "2", "y"],
        ]
        .iter()
        .enumerate()
        {
            for (c, v) in row.iter().enumerate() {
                range.set_value((r as u32 + 1, c as u32), Data::String((*v).into()));
            }
        }
        let frame = sheet_frame(&range, &vec![((0, 0), (0, 1))]);
        let image = Arc::new(xlsx_richdata::CellImage {
            image: None,
            alt_text: Some("Section icon".into()),
        });
        let formats = SheetFormats {
            images: HashMap::from([((0, 2), image)]),
            ..SheetFormats::default()
        };
        let found = find_tables(&range, &frame, false, &formats, false);
        assert_eq!(found.len(), 1);
        let t = &found[0];
        assert!(t.label.is_none(), "the label stays in the table");
        assert_eq!((t.table.rows.len(), t.table.rows[0].len()), (4, 3));
        assert_eq!(t.table.rows[0][2], "<!-- image -->  Section icon");
        let blocks = t.table.cell_blocks.as_ref().expect("a rich cell");
        let [Node::Prov { bbox, inner, .. }] = blocks[0][2].as_slice() else {
            panic!("{:?}", blocks[0][2]);
        };
        assert_eq!(*bbox, [2.0, 0.0, 3.0, 1.0]);
        assert!(matches!(**inner, Node::Picture { .. }));
        assert!(blocks[0][0].is_empty() && blocks[1][2].is_empty());
        // Without the rich value the error cell is ordinary content.
        let plain = find_tables(&range, &frame, false, &SheetFormats::default(), false);
        assert!(plain[0].table.cell_blocks.is_none());
    }

    /// docling PR #3727: a merged full-width cell directly above a real
    /// header row is a section label — a paragraph before the table, not a
    /// swallowed header.
    #[test]
    fn section_label_splits_off_the_table() {
        let path = format!(
            "{}/../../tests/data/xlsx/sources/xlsx_09_section_label_header.xlsx",
            env!("CARGO_MANIFEST_DIR")
        );
        let bytes = std::fs::read(&path).expect("fixture exists");
        let src = SourceDocument::from_bytes("x.xlsx", InputFormat::Xlsx, bytes);
        let doc = XlsxBackend::default().convert(&src).expect("converts");
        let nodes = flatten(&doc.nodes);
        let para = nodes
            .iter()
            .position(|n| matches!(n, Node::Paragraph { text } if text == "Reading List"));
        let table_ix = nodes
            .iter()
            .position(|n| matches!(n, Node::Table(_)))
            .expect("a table");
        let para = para.expect("section label emitted as a paragraph");
        assert!(para < table_ix, "label precedes the table");
        if let Node::Table(t) = nodes[table_ix] {
            assert_eq!(t.rows[0][0], "#", "real header row leads the table");
            assert!(
                t.rows.iter().all(|r| r[0] != "Reading List"),
                "label absorbed into the table: {:?}",
                t.rows
            );
        }
    }

    /// Binary workbook (issue #210): the visible sheet's table converts with
    /// openpyxl-style values, the hidden sheet's group carries the invisible
    /// content layer, and the second sheet trails a page break.
    #[test]
    fn xlsb_tables_and_hidden_sheet() {
        let path = format!(
            "{}/tests/data/xlsx/sources/xlsb-tables.xlsb",
            env!("CARGO_MANIFEST_DIR")
        );
        let bytes = std::fs::read(&path).expect("fixture exists");
        let src = SourceDocument::from_bytes("x.xlsb", InputFormat::Xlsx, bytes);
        let doc = XlsxBackend::default().convert(&src).expect("converts");
        let nodes = flatten(&doc.nodes);
        let Some(Node::Table(sales)) = nodes.iter().find(|n| matches!(n, Node::Table(_))) else {
            panic!("no visible table in {:?}", doc.nodes);
        };
        assert_eq!(sales.rows[0], vec!["region", "q1", "q2"]);
        assert_eq!(sales.rows[1], vec!["EMEA", "100", "110.5"]);
        let hidden = doc.nodes.iter().any(|n| {
            matches!(
                n,
                Node::Group { layer: Some(docling_core::ContentLayer::Invisible), children, .. }
                    if children.iter().any(|c| matches!(c, Node::Table(_)))
            )
        });
        assert!(
            hidden,
            "hidden sheet's group on the invisible layer: {:?}",
            doc.nodes
        );
        assert!(doc.nodes.iter().any(|n| matches!(n, Node::PageBreak)));
    }
}
