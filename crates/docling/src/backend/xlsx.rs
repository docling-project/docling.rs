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

use crate::backend::ooxml::{count_pictures, resolve, Package};
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
        let image_counts = sheet_image_counts(&source.bytes);

        // docling only emits visible worksheets in the body: chartsheets are
        // skipped, and hidden sheets land in a non-body content layer that
        // `export_to_markdown` drops. (calamine would otherwise surface both.)
        let sheet_names: Vec<String> = workbook
            .sheets_metadata()
            .iter()
            .filter(|s| {
                matches!(s.typ, calamine::SheetType::WorkSheet)
                    && matches!(s.visible, calamine::SheetVisible::Visible)
            })
            .map(|s| s.name.clone())
            .collect();

        let mut doc = DoclingDocument::new(&source.name);
        for name in sheet_names {
            // Collect merges (absolute coords) before borrowing the range.
            let abs_merges: Vec<((u32, u32), (u32, u32))> = workbook
                .merged_regions_by_sheet(&name)
                .iter()
                .map(|(_, _, d)| (d.start, d.end))
                .collect();
            let Ok(range) = workbook.worksheet_range(&name) else {
                continue;
            };
            let (rs_r, rs_c) = range.start().unwrap_or((0, 0));
            // Map every merge-covered cell to the merge's top-left (relative
            // coords), so the value is duplicated across the span as docling does.
            let mut merge_of: HashMap<(usize, usize), (usize, usize)> = HashMap::new();
            for ((sr, sc), (er, ec)) in abs_merges {
                let tl = ((sr - rs_r) as usize, (sc - rs_c) as usize);
                for r in sr..=er {
                    for c in sc..=ec {
                        merge_of.insert(((r - rs_r) as usize, (c - rs_c) as usize), tl);
                    }
                }
            }
            // Merged regions can extend past calamine's value-based range (an
            // all-empty merged row has no values), so widen the scan to cover
            // them — docling expands the data bounds to merged cells too.
            let (rh, rw) = range.get_size();
            let height = rh.max(merge_of.keys().map(|(r, _)| r + 1).max().unwrap_or(0));
            let width = rw.max(merge_of.keys().map(|(_, c)| c + 1).max().unwrap_or(0));
            for table in find_tables(&range, &merge_of, height, width) {
                doc.push(Node::Table(table));
            }
            // docling appends one picture per embedded image, after the tables.
            for _ in 0..image_counts.get(&name).copied().unwrap_or(0) {
                doc.push(Node::Picture {
                    caption: None,
                    image: None,
                });
            }
        }
        Ok(doc)
    }
}

/// Map each sheet name to its number of embedded images, by walking
/// `workbook.xml` → workbook rels → per-sheet rels → drawing parts.
fn sheet_image_counts(bytes: &[u8]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    let Some(mut pkg) = Package::open(bytes) else {
        return counts;
    };
    let Some(workbook) = pkg.read("xl/workbook.xml") else {
        return counts;
    };
    // r:id -> sheet part path
    let rid_to_part: HashMap<String, String> = pkg
        .rels_for("xl/workbook.xml")
        .iter()
        .map(|r| (r.id.clone(), resolve("xl", &r.target)))
        .collect();

    for (name, rid) in workbook_sheets(&workbook) {
        let Some(part) = rid_to_part.get(&rid) else {
            continue;
        };
        let dir = part.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let mut n = 0;
        for rel in pkg.rels_for(part) {
            if rel.rel_type.ends_with("/drawing") {
                let drawing = resolve(dir, &rel.target);
                if let Some(xml) = pkg.read(&drawing) {
                    n += count_pictures(&xml);
                }
            }
        }
        if n > 0 {
            counts.insert(name, n);
        }
    }
    counts
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
/// docling's default `gap_tolerance = 0`), in row-major discovery order. A cell
/// covered by a merge counts as content even if its own value is empty.
fn find_tables(
    range: &Range<Data>,
    merge_of: &HashMap<(usize, usize), (usize, usize)>,
    height: usize,
    width: usize,
) -> Vec<Table> {
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
            tables.push(Table { rows });
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
