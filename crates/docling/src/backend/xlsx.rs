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
//! openpyxl-compatible value formatting.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;

use calamine::{Data, Range, Reader, Xlsx};
use docling_core::{DoclingDocument, Node, Table};
use quick_xml::events::Event;
use quick_xml::Reader as XmlReader;

use crate::backend::ooxml::{resolve, Package};
use crate::backend::xlsx_drawings;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

pub struct XlsxBackend;

impl DeclarativeBackend for XlsxBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let cursor = Cursor::new(source.bytes.clone());
        let mut workbook: Xlsx<_> =
            Xlsx::new(cursor).map_err(|e| ConversionError::Parse(format!("xlsx: {e}")))?;
        let _ = workbook.load_merged_regions();
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
        let wb_rels: HashMap<String, String> = pkg
            .rels_for("xl/workbook.xml")
            .iter()
            .map(|r| (r.id.clone(), resolve("xl", &r.target)))
            .collect();
        let sheet_parts: HashMap<String, String> = workbook_sheets(&wb_xml)
            .into_iter()
            .filter_map(|(name, rid)| Some((name, wb_rels.get(&rid)?.clone())))
            .collect();

        // Threaded-comment persons (Excel 365).
        let persons = pkg
            .read("xl/persons/person.xml")
            .map(|xml| xlsx_drawings::parse_persons(&xml))
            .unwrap_or_default();

        // Pre-load every worksheet's cell range once: chart series resolve
        // by reference into arbitrary sheets.
        let mut ranges: HashMap<String, Range<Data>> = HashMap::new();
        for (name, typ, _) in &metas {
            if matches!(typ, calamine::SheetType::WorkSheet) {
                if let Ok(range) = workbook.worksheet_range(name) {
                    ranges.insert(name.clone(), range);
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
            let Some(range) = ranges.get(&sheet) else {
                return Vec::new();
            };
            let (rs_r, rs_c) = range.start().unwrap_or((0, 0));
            let mut out = Vec::new();
            for r in min_r..=max_r {
                for c in min_c..=max_c {
                    let rr = (r as u32).wrapping_sub(rs_r) as usize;
                    let cc = (c as u32).wrapping_sub(rs_c) as usize;
                    let v = if r as u32 >= rs_r && c as u32 >= rs_c {
                        range.get((rr, cc)).map(format_cell).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    out.push(v);
                }
            }
            out
        };

        let mut doc = DoclingDocument::new(&source.name);
        let mut comments: Vec<String> = Vec::new();
        // The page number of the most recent sheet that produced items — the
        // DocLang page break trails the *following* sheet's content (docling
        // serializes each sheet group before the page-break node that the
        // item iterator placed inside it).
        let mut prev_item_page: Option<usize> = None;
        for (page_ix, (name, typ, visible)) in metas.iter().enumerate() {
            let hidden = !matches!(visible, calamine::SheetVisible::Visible);
            // (bbox in cell units, node) items for this sheet/page.
            let mut items: Vec<((usize, usize, usize, usize), Node)> = Vec::new();

            if matches!(typ, calamine::SheetType::WorkSheet) {
                if let Some(range) = ranges.get(name) {
                    let abs_merges: Vec<((u32, u32), (u32, u32))> = workbook
                        .merged_regions_by_sheet(name)
                        .iter()
                        .map(|(_, _, d)| (d.start, d.end))
                        .collect();
                    let (rs_r, rs_c) = range.start().unwrap_or((0, 0));
                    let mut merge_of: HashMap<(usize, usize), (usize, usize)> = HashMap::new();
                    for ((sr, sc), (er, ec)) in abs_merges {
                        let tl = ((sr - rs_r) as usize, (sc - rs_c) as usize);
                        for r in sr..=er {
                            for c in sc..=ec {
                                merge_of.insert(((r - rs_r) as usize, (c - rs_c) as usize), tl);
                            }
                        }
                    }
                    let (rh, rw) = range.get_size();
                    let height = rh.max(merge_of.keys().map(|(r, _)| r + 1).max().unwrap_or(0));
                    let width = rw.max(merge_of.keys().map(|(_, c)| c + 1).max().unwrap_or(0));
                    // docling's bboxes are in *absolute* cell indices; calamine's
                    // range is clipped to its first non-empty row/column.
                    let (or, oc) = (rs_r as usize, rs_c as usize);
                    for t in find_tables(range, &merge_of, height, width) {
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
                                        image: dimages.get(&rid).cloned(),
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
                                        kind: spec.kind.to_string(),
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
                        .map(|xml| xlsx_drawings::parse_threaded_comments(&xml, &persons))
                        .unwrap_or_default();
                    // Row-major over commented cells (docling scans the grid).
                    let mut cells: Vec<(usize, usize, String)> = legacy
                        .iter()
                        .filter_map(|(cell, author, text)| {
                            let (c, r) = cell_ref_pub(cell)?;
                            let line = match threaded.get(cell) {
                                Some((a, t, time)) => match time {
                                    Some(ts) => format!("[author: {a}, time: {ts}]: {t}"),
                                    None => format!("[author: {a}]: {t}"),
                                },
                                None => format!("[author: {author}]: {text}"),
                            };
                            Some((r, c, line))
                        })
                        .collect();
                    cells.sort_by_key(|(r, c, _)| (*r, *c));
                    comments.extend(cells.into_iter().map(|(_, _, line)| line));
                }
            }

            if items.is_empty() {
                continue;
            }
            // docling sorts a sheet's children by top coordinate (stable).
            items.sort_by_key(|((_, t, _, _), _)| *t);
            // Location provenance against the sheet's extent.
            let page_w = items.iter().map(|((_, _, r, _), _)| *r).max().unwrap_or(1);
            let page_h = items.iter().map(|((_, _, _, b), _)| *b).max().unwrap_or(1);
            for ((l, t, r, b), node) in &mut items {
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
            for ((l, t, r, b), node) in items {
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
                let node = if hidden {
                    Node::Furniture {
                        layer: docling_core::ContentLayer::Invisible,
                        inner: Box::new(node),
                    }
                } else {
                    node
                };
                doc.push(node);
            }
            // DocLang page break: trails this sheet's content when an earlier
            // sheet already produced items (see module docs).
            if prev_item_page.is_some() {
                doc.push(Node::PageBreak);
            }
            prev_item_page = Some(page_ix + 1);
        }
        for line in comments {
            doc.nodes.push(Node::Furniture {
                layer: docling_core::ContentLayer::Notes,
                inner: Box::new(Node::Paragraph { text: line }),
            });
        }
        Ok(doc)
    }
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

/// docling's `_chart_to_table_data`: categories down the first column (row
/// headers), one column per series (column headers), the top-left cell empty.
fn chart_table(
    spec: &xlsx_drawings::ChartSpec,
    own_sheet: &str,
    resolve_ref: &dyn Fn(&str, &str) -> Vec<String>,
) -> Option<Table> {
    if spec.series.is_empty() {
        return None;
    }
    let mut categories: Vec<String> = Vec::new();
    for s in &spec.series {
        if let Some(cat) = &s.cat_ref {
            categories = resolve_ref(cat, own_sheet);
            if !categories.is_empty() {
                break;
            }
        }
    }
    let mut columns: Vec<(String, Vec<String>)> = Vec::new();
    for s in &spec.series {
        let values = s
            .val_ref
            .as_deref()
            .map(|r| resolve_ref(r, own_sheet))
            .unwrap_or_default();
        let name = match &s.name_ref {
            Some(r) => resolve_ref(r, own_sheet)
                .into_iter()
                .next()
                .unwrap_or_default(),
            None => s.name_lit.clone().unwrap_or_default(),
        };
        columns.push((name, values));
    }
    let num_data_rows = columns
        .iter()
        .map(|(_, v)| v.len())
        .chain([categories.len()])
        .max()
        .unwrap_or(0);
    if num_data_rows == 0 {
        return None;
    }
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut header = vec![String::new()];
    header.extend(columns.iter().map(|(n, _)| n.clone()));
    rows.push(header);
    for i in 0..num_data_rows {
        let mut row = vec![categories.get(i).cloned().unwrap_or_default()];
        for (_, values) in &columns {
            row.push(values.get(i).cloned().unwrap_or_default());
        }
        rows.push(row);
    }
    let nrows = rows.len();
    let ncols = rows[0].len();
    let mut header_row = vec![false; nrows];
    header_row[0] = true;
    let mut row_header = vec![vec![false; ncols]; nrows];
    for r in row_header.iter_mut().skip(1) {
        r[0] = true;
    }
    Some(Table {
        rows,
        location: None,
        structure: Some(docling_core::TableStructure {
            header_row,
            col_continuation: Vec::new(),
            row_continuation: Vec::new(),
            row_header,
            col_header: Vec::new(),
        }),
        cell_blocks: None,
    })
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
fn location_value(coord: usize, page: usize) -> u16 {
    if page == 0 {
        return 0;
    }
    let v = (LOC_RESOLUTION as f64 * coord as f64 / page as f64).round() as i64;
    v.clamp(0, LOC_RESOLUTION as i64 - 1) as u16
}

/// A discovered table with its cell-index bounding box (inclusive), used to
/// compute the DocLang `<location>` provenance.
struct FoundTable {
    table: Table,
    min_r: usize,
    min_c: usize,
    max_r: usize,
    max_c: usize,
}

/// docling's default `gap_tolerance = 0`), in row-major discovery order. A cell
/// covered by a merge counts as content even if its own value is empty.
fn find_tables(
    range: &Range<Data>,
    merge_of: &HashMap<(usize, usize), (usize, usize)>,
    height: usize,
    width: usize,
) -> Vec<FoundTable> {
    let has_content = |r: usize, c: usize| -> bool {
        merge_of.contains_key(&(r, c))
            || range
                .get((r, c))
                .map(|d| !matches!(d, Data::Empty))
                .unwrap_or(false)
    };
    // A grid position renders the value of its merge's top-left cell, if merged.
    let cell_text = |r: usize, c: usize| -> String {
        let (sr, sc) = merge_of.get(&(r, c)).copied().unwrap_or((r, c));
        range.get((sr, sc)).map(format_cell).unwrap_or_default()
    };

    let mut visited: HashSet<(usize, usize)> = HashSet::new();
    let mut tables = Vec::new();

    for r in 0..height {
        for c in 0..width {
            if !has_content(r, c) || visited.contains(&(r, c)) {
                continue;
            }
            // Flood fill from this seed over 4-connected content cells.
            let mut stack = vec![(r, c)];
            let mut cells: HashSet<(usize, usize)> = HashSet::new();
            cells.insert((r, c));
            let (mut min_r, mut max_r, mut min_c, mut max_c) = (r, r, c, c);
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

            // Materialise the region's bounding box; gaps become empty cells.
            let rows: Vec<Vec<String>> = (min_r..=max_r)
                .map(|gr| (min_c..=max_c).map(|gc| cell_text(gr, gc)).collect())
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
            tables.push(FoundTable {
                table: Table {
                    rows,
                    location: None,
                    structure,
                    cell_blocks: None,
                },
                min_r,
                min_c,
                max_r,
                max_c,
            });
        }
    }
    tables
}

/// Render one cell to match openpyxl's `str(cell.value)`.
fn format_cell(value: &Data) -> String {
    match value {
        Data::Empty => String::new(),
        // openpyxl reads strings through an XML parser, which normalises line
        // endings (`\r\n`/`\r` → `\n`); calamine keeps them raw, so do it here.
        Data::String(s) => s.replace("\r\n", "\n").replace('\r', "\n"),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => format_number(*f),
        Data::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Data::DateTime(dt) => dt
            .as_datetime()
            .map(|d| d.to_string())
            .unwrap_or_else(|| format_number(dt.as_f64())),
        Data::DateTimeIso(s) => s.clone(),
        Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("{e:?}"),
    }
}

/// openpyxl returns an `int` for integer-valued numbers (no trailing `.0`) and a
/// `float` otherwise; mirror that.
fn format_number(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}
