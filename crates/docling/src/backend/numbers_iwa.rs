//! Reader for the object graph of a Numbers 3+ (2013 onwards) document —
//! docling's `iwork/numbers_iwa.py` (#1031).
//!
//! `TN.DocumentArchive` lists the sheets, each `TN.SheetArchive` lists what is
//! drawn on it, and every drawable read is a table, a chart, a picture or a
//! sticky note. Tables are the same `TST` archives Pages embeds; what is
//! added here is everything around them — the sheets, the frames that
//! position them, and the typed values and pictures a spreadsheet cell holds.

use std::collections::HashMap;

use super::iwork::{first_bytes, first_varint, Archive, Fields, Value};
use super::iwork_cells::{self, CellValues};
use super::keynote::Geometry;
use super::numbers::{sort_by_sheet, Cell, Comment, Sheet, SheetTable, MAX_TABLE_CELLS};
use super::ooxml::Package;
use super::pages::Picture;
use super::pages_iwa::{
    cell_values, drawable_geometry, index_objects, placements, read_archives, reference_field,
    value_list, Objects, Reader, TSD_GROUP, TSD_IMAGE, TST_TABLE_MODEL, TST_TABULAR_INFO,
};
use crate::error::ConversionError;

/// `TN.DocumentArchive`, the root of a Numbers document.
const TN_DOCUMENT_ARCHIVE: u32 = 1;
/// `TN.SheetArchive`, one tab of the document.
const TN_SHEET_ARCHIVE: u32 = 2;
/// The archive placing a sticky note on a sheet. A comment anchored to a cell
/// is stored beside the table instead, and is not read.
const TN_COMMENT_INFO: u32 = 2014;
/// The annotation a comment archive points at, and its author.
const TSK_ANNOTATION: u32 = 3056;
const TSK_AUTHOR: u32 = 212;

const DOCUMENT_SHEETS_FIELD: u32 = 1;
const SHEET_NAME_FIELD: u32 = 1;
const SHEET_DRAWABLES_FIELD: u32 = 2;
/// `TST.TableInfoArchive`: its drawable super, and its table.
const INFO_SUPER_FIELD: u32 = 1;
const INFO_MODEL_FIELD: u32 = 2;
/// `TST.TableModelArchive` fields a Pages table has no use for.
const TABLE_ROWS_FIELD: u32 = 6;
const TABLE_COLS_FIELD: u32 = 7;
const TABLE_NAME_FIELD: u32 = 8;
const TABLE_HEADER_ROWS_FIELD: u32 = 9;
const TABLE_HEADER_COLS_FIELD: u32 = 10;
const TABLE_DATA_STORE_FIELD: u32 = 4;
/// A table's data store referencing its list of cell styles, and the style
/// a list entry references.
const STORE_STYLES_FIELD: u32 = 5;
const STYLE_ENTRY_REFERENCE_FIELD: u32 = 4;
/// `TST.CellStyleArchive`, and the fields leading from it to the data of the
/// image it fills cells with: the cell properties, their fill, the image
/// fill, then a `TSP.DataReference`. A colour or gradient fill has none.
const TST_CELL_STYLE: u32 = 6004;
const IMAGE_FILL_PATH: [u32; 3] = [11, 1, 3];
const IMAGE_FILL_DATA_FIELD: u32 = 6;
/// Fields leading from a comment archive to its text, date and author.
const COMMENT_ANNOTATION_FIELD: u32 = 2;
const ANNOTATION_TEXT_FIELD: u32 = 1;
const ANNOTATION_TIME_FIELD: u32 = 2;
const ANNOTATION_AUTHOR_FIELD: u32 = 3;
const AUTHOR_NAME_FIELD: u32 = 1;

fn fail(msg: &str) -> ConversionError {
    ConversionError::Parse(format!("iwork: {msg}"))
}

/// `read_content`: the sheets of a Numbers 3+ document, in document order.
pub(crate) fn read_content(pkg: &mut Package) -> Result<Vec<Sheet>, ConversionError> {
    let archives = read_archives(pkg)?;
    let (objects, order) = index_objects(&archives);
    let document = order
        .iter()
        .filter_map(|id| objects.get(id).copied())
        .find(|a| a.ty == TN_DOCUMENT_ARCHIVE)
        .ok_or_else(|| {
            fail(
                "the Numbers document has no TN.DocumentArchive; the container may be \
                 corrupt or password-protected",
            )
        })?;
    let mut reader = Reader::new(&objects, &order, pkg, "");
    let mut pictures: HashMap<u64, Picture> = HashMap::new();
    let mut sheets = Vec::new();
    for sheet in references(&document.payload, DOCUMENT_SHEETS_FIELD)
        .into_iter()
        .filter_map(|id| typed(&objects, id, TN_SHEET_ARCHIVE))
    {
        sheets.push(read_sheet(sheet, &objects, &mut reader, &mut pictures));
    }
    Ok(sheets)
}

fn typed<'a>(objects: &Objects<'a>, id: u64, ty: u32) -> Option<&'a Archive> {
    objects.get(&id).copied().filter(|a| a.ty == ty)
}

/// The references a repeated field holds, in order.
fn references(payload: &[u8], field: u32) -> Vec<u64> {
    super::pages_iwa::reference_list(payload, field)
}

fn text_of(payload: &[u8], field: u32) -> Option<String> {
    first_bytes(payload, field).map(|b| String::from_utf8_lossy(b).into_owned())
}

/// `drawable_frame`: where a drawable sits, from its `super` field.
fn frame(payload: &[u8]) -> Option<Geometry> {
    first_bytes(payload, INFO_SUPER_FIELD).and_then(drawable_geometry)
}

/// `read_sheet`: one sheet's name and the tables, charts, pictures and notes
/// on it, each kind in [`super::numbers::sheet_order`].
fn read_sheet(
    sheet: &Archive,
    objects: &Objects,
    reader: &mut Reader,
    pictures: &mut HashMap<u64, Picture>,
) -> Sheet {
    let mut out = Sheet {
        name: text_of(&sheet.payload, SHEET_NAME_FIELD).unwrap_or_default(),
        ..Sheet::default()
    };
    for drawable in references(&sheet.payload, SHEET_DRAWABLES_FIELD)
        .into_iter()
        .filter_map(|id| objects.get(&id).copied())
    {
        match drawable.ty {
            TST_TABULAR_INFO => {
                if let Some(t) = read_table(drawable, objects, reader, pictures) {
                    out.tables.push(t);
                }
            }
            super::iwork_charts::TSCH_CHART_DRAWABLE => {
                if let Some(chart) = super::iwork_charts::iwa_chart(drawable, objects) {
                    out.charts.push((chart, frame(&drawable.payload)));
                }
            }
            TSD_IMAGE | TSD_GROUP => {
                let geometry = frame(&drawable.payload);
                for picture in reader.pictures(drawable.id) {
                    out.pictures.push((picture, geometry));
                }
            }
            TN_COMMENT_INFO => {
                if let Some(c) = read_comment(drawable, objects) {
                    out.comments.push(c);
                }
            }
            _ => {}
        }
    }
    sort_by_sheet(&mut out.tables, |t| t.geometry.as_ref());
    sort_by_sheet(&mut out.charts, |(_, g)| g.as_ref());
    sort_by_sheet(&mut out.pictures, |(_, g)| g.as_ref());
    sort_by_sheet(&mut out.comments, |c| c.geometry.as_ref());
    out
}

/// `read_table`: one table from the archive placing it on its sheet.
fn read_table(
    info: &Archive,
    objects: &Objects,
    reader: &mut Reader,
    pictures: &mut HashMap<u64, Picture>,
) -> Option<SheetTable> {
    let model = reference_field(&info.payload, INFO_MODEL_FIELD)
        .and_then(|id| typed(objects, id, TST_TABLE_MODEL))?;
    let num_rows = first_varint(&model.payload, TABLE_ROWS_FIELD)? as usize;
    let num_cols = first_varint(&model.payload, TABLE_COLS_FIELD)? as usize;
    if num_rows == 0 || num_cols == 0 {
        return None;
    }
    if num_rows.saturating_mul(num_cols) > MAX_TABLE_CELLS {
        eprintln!(
            "warning: iwork: skipping a Numbers table declaring {num_rows} rows by {num_cols} columns"
        );
        return None;
    }
    let store = first_bytes(&model.payload, TABLE_DATA_STORE_FIELD).unwrap_or(&[]);
    let values = cell_values(store, objects);
    let count = |field| first_varint(&model.payload, field).unwrap_or(0) as usize;
    Some(SheetTable {
        name: text_of(&model.payload, TABLE_NAME_FIELD).unwrap_or_default(),
        num_rows,
        num_cols,
        header_rows: count(TABLE_HEADER_ROWS_FIELD),
        header_cols: count(TABLE_HEADER_COLS_FIELD),
        cells: read_cells(
            store, objects, &values, num_rows, num_cols, reader, pictures,
        ),
        geometry: frame(&info.payload),
    })
}

/// `read_cells`: every cell of a table that holds text or a picture, in tile
/// order.
fn read_cells(
    store: &[u8],
    objects: &Objects,
    values: &CellValues,
    num_rows: usize,
    num_cols: usize,
    reader: &mut Reader,
    pictures: &mut HashMap<u64, Picture>,
) -> Vec<Cell> {
    let fills = value_list(store, STORE_STYLES_FIELD, objects, image_fill);
    let mut cells = Vec::new();
    for placed in placements(store, objects) {
        if placed.row >= num_rows || placed.col >= num_cols {
            continue;
        }
        let decoded = iwork_cells::cell(placed.storage, placed.start, values);
        let rendered = iwork_cells::render(decoded.as_ref());
        let picture = decoded
            .as_ref()
            .and_then(|d| d.style)
            .and_then(|style| fills.get(&style))
            .map(|&data| {
                pictures
                    .entry(data)
                    .or_insert_with(|| reader.data_picture(data))
                    .clone()
            });
        if rendered.is_some() || picture.is_some() {
            cells.push(Cell {
                row: placed.row,
                col: placed.col,
                text: rendered.unwrap_or_default(),
                col_span: 1,
                picture,
            });
        }
    }
    cells
}

/// `image_fill`: the data of the image a style list entry fills its cells
/// with, `None` when the style fills them with no image.
fn image_fill(entry: &[u8], objects: &Objects) -> Option<u64> {
    let style = reference_field(entry, STYLE_ENTRY_REFERENCE_FIELD)
        .and_then(|id| typed(objects, id, TST_CELL_STYLE))?;
    let mut message: &[u8] = &style.payload;
    for field in IMAGE_FILL_PATH {
        message = first_bytes(message, field)?;
    }
    reference_field(message, IMAGE_FILL_DATA_FIELD)
}

/// `read_comment`: one sticky note — its text, and who left it when; `None`
/// when it has no text.
fn read_comment(info: &Archive, objects: &Objects) -> Option<Comment> {
    let annotation = reference_field(&info.payload, COMMENT_ANNOTATION_FIELD)
        .and_then(|id| typed(objects, id, TSK_ANNOTATION))?;
    let text = text_of(&annotation.payload, ANNOTATION_TEXT_FIELD)
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() {
        return None;
    }
    let author = reference_field(&annotation.payload, ANNOTATION_AUTHOR_FIELD)
        .and_then(|id| typed(objects, id, TSK_AUTHOR))
        .and_then(|a| text_of(&a.payload, AUTHOR_NAME_FIELD))
        .unwrap_or_default();
    Some(Comment {
        text,
        author,
        timestamp: first_bytes(&annotation.payload, ANNOTATION_TIME_FIELD)
            .and_then(timestamp)
            .and_then(iwork_cells::format_timestamp),
        geometry: frame(&info.payload),
    })
}

/// `timestamp`: a `TSP.Date`, seconds from the Apple epoch as a 64-bit float.
fn timestamp(date: &[u8]) -> Option<f64> {
    Fields::new(date).find_map(|(f, v)| match (f, v) {
        (1, Value::Fixed64(bits)) => Some(f64::from_bits(bits)),
        _ => None,
    })
}
