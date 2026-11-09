//! Reader for the `index.xml` of an iWork '09 (or earlier) Numbers document —
//! docling's `iwork/numbers_xml.py` (#1031).
//!
//! Everything the spreadsheet shows sits inside an `ls:workspace`, one per
//! sheet; the rest of the file also carries the tables and charts of the
//! template the document was made from, which the spreadsheet never draws.
//! A table's datasource holds only the cells a row actually uses, with no
//! coordinates, so the grid's occupancy counts put them back; a chart bound
//! to a table keeps the values it last plotted in a property list of its own.

use std::collections::HashMap;

use roxmltree::{Document, Node as XmlNode};

use super::iwork_cells::{format_bool, format_date, format_number, Number};
use super::numbers::{sort_by_sheet, Cell, Comment, Sheet, SheetTable, MAX_TABLE_CELLS};
use super::ooxml::Package;
use super::pages::{Chart, ChartSeries};
use super::pages_xml::{
    is_media, is_sf, legacy_geometry, legacy_picture, read_index_xml, SFA_NS, SF_NS,
};
use crate::error::ConversionError;

const LS_NS: &str = "http://developer.apple.com/namespaces/ls";

/// `sf:chart-direction` of a chart plotting each of its rows as a series; 1,
/// or nothing, plots each column.
const SERIES_BY_ROW: i64 = 0;
/// The member holding one chart's cached data, named after its entity id.
const CHART_SHARE_SUFFIX: &str = ".chrtshr";
/// The largest chart share read (docling's `max_file_bytes` default).
const MAX_SHARE_BYTES: usize = 100 * 1024 * 1024;

/// `LEGACY_CHART_TYPES`: an iWork '09 chart's kind by its `sf:chart-type` —
/// the position, from one, in Keynote '09's AppleScript `add chart` list —
/// as docling's picture classification (column and bar charts `bar_chart`,
/// pies `pie_chart`; area, mixed, two-axis and unknown kinds `other_chart`).
fn legacy_chart_label(chart_type: i64) -> &'static str {
    match chart_type {
        1 | 6 => "pie_chart",
        2..=5 | 7..=10 => "bar_chart",
        13 | 14 => "line_chart",
        17 => "scatter_chart",
        _ => "other_chart",
    }
}

fn sf<'a>(node: XmlNode<'a, 'a>, name: &str) -> Option<&'a str> {
    node.attribute((SF_NS, name))
}

fn sfa<'a>(node: XmlNode<'a, 'a>, name: &str) -> Option<&'a str> {
    node.attribute((SFA_NS, name))
}

/// `int_attr`: an integer attribute, tolerating absent or malformed values.
fn int_attr(node: XmlNode, name: &str) -> Option<i64> {
    sf(node, name)?.trim().parse().ok()
}

/// `float_attr` on an `sf:` attribute.
fn float_attr(node: XmlNode, name: &str) -> Option<f64> {
    sf(node, name)?.trim().parse().ok()
}

fn child<'a>(node: XmlNode<'a, 'a>, name: &str) -> Option<XmlNode<'a, 'a>> {
    node.children().find(|c| is_sf(*c, name))
}

fn descendant<'a>(node: XmlNode<'a, 'a>, name: &str) -> Option<XmlNode<'a, 'a>> {
    node.descendants().find(|c| is_sf(*c, name))
}

/// `read_content`: the sheets of an iWork '09 document, in document order.
pub(crate) fn read_content(pkg: &mut Package, member: &str) -> Result<Vec<Sheet>, ConversionError> {
    let xml = read_index_xml(pkg, member, "Numbers")?;
    let dom = Document::parse(&xml)
        .map_err(|e| ConversionError::Parse(format!("iwork: could not parse '{member}': {e}")))?;
    let shares = read_chart_shares(pkg);
    let mut sheets = Vec::new();
    for workspace in dom.descendants().filter(|n| {
        n.is_element()
            && n.tag_name().namespace() == Some(LS_NS)
            && n.tag_name().name() == "workspace"
    }) {
        let mut sheet = Sheet {
            name: workspace
                .attribute((LS_NS, "workspace-name"))
                .unwrap_or_default()
                .to_string(),
            ..Sheet::default()
        };
        for node in workspace.descendants() {
            if is_sf(node, "tabular-info") {
                sheet.tables.extend(read_table(node));
            } else if is_sf(node, "chart-info") {
                sheet.charts.extend(read_chart(node, &shares));
            } else if is_sf(node, "sticky-note") {
                sheet.comments.extend(read_comment(node));
            }
        }
        // Pictures only from the sheet's drawables lists (renditions an image
        // keeps below it are not pictures of their own).
        for drawables in sheet_drawables(workspace) {
            for drawable in drawables.children().filter(|n| is_media(*n)) {
                if let Some(picture) = legacy_picture(drawable, pkg) {
                    sheet.pictures.push((picture, legacy_geometry(drawable)));
                }
            }
        }
        sort_by_sheet(&mut sheet.tables, |t| t.geometry.as_ref());
        sort_by_sheet(&mut sheet.charts, |(_, g)| g.as_ref());
        sort_by_sheet(&mut sheet.pictures, |(_, g)| g.as_ref());
        sort_by_sheet(&mut sheet.comments, |c| c.geometry.as_ref());
        sheets.push(sheet);
    }
    Ok(sheets)
}

/// `SHEET_DRAWABLES`: `ls:page-info/sf:layers/sf:layer/sf:drawables` under a
/// workspace.
fn sheet_drawables<'a>(workspace: XmlNode<'a, 'a>) -> Vec<XmlNode<'a, 'a>> {
    let mut out = Vec::new();
    for page in workspace.children().filter(|n| {
        n.is_element()
            && n.tag_name().namespace() == Some(LS_NS)
            && n.tag_name().name() == "page-info"
    }) {
        for layers in page.children().filter(|n| is_sf(*n, "layers")) {
            for layer in layers.children().filter(|n| is_sf(*n, "layer")) {
                out.extend(layer.children().filter(|n| is_sf(*n, "drawables")));
            }
        }
    }
    out
}

/// One chart share: its series names, and per category a name and values.
#[derive(Default)]
struct Share {
    columns: Vec<String>,
    rows: Vec<(String, Vec<Option<f64>>)>,
}

/// `read_chart_shares`: every chart's cached data, by entity id; a share that
/// is missing or unreadable is left out.
fn read_chart_shares(pkg: &mut Package) -> HashMap<String, Share> {
    let names: Vec<String> = pkg
        .names()
        .filter(|n| n.ends_with(CHART_SHARE_SUFFIX))
        .map(str::to_string)
        .collect();
    let mut shares = HashMap::new();
    for name in names {
        let Some(bytes) = pkg.read_bytes(&name).filter(|b| b.len() <= MAX_SHARE_BYTES) else {
            continue;
        };
        if let Some(share) = parse_share(&bytes) {
            shares.insert(
                name[..name.len() - CHART_SHARE_SUFFIX.len()].to_string(),
                share,
            );
        }
    }
    shares
}

/// An XML property list's top-level `dict` as a [`Share`].
fn parse_share(bytes: &[u8]) -> Option<Share> {
    let text = std::str::from_utf8(bytes).ok()?;
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let dom = Document::parse_with_options(text, options).ok()?;
    let dict = dom
        .root_element()
        .children()
        .find(|n| n.has_tag_name("dict"))?;
    let mut share = Share::default();
    if let Some(columns) = dict_get(dict, "columns") {
        share.columns = columns
            .children()
            .filter(|n| n.is_element())
            .map(plist_text)
            .collect();
    }
    if let Some(rows) = dict_get(dict, "rows") {
        for row in rows.children().filter(|n| n.has_tag_name("dict")) {
            let name = dict_get(row, "rowName").map(plist_text).unwrap_or_default();
            let values = dict_get(row, "rowValues")
                .map(|a| {
                    a.children()
                        .filter(|n| n.is_element())
                        .map(|v| match v.tag_name().name() {
                            "real" | "integer" => v.text().and_then(|t| t.trim().parse().ok()),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            share.rows.push((name, values));
        }
    }
    Some(share)
}

/// A plist `dict`'s value for `key`.
fn dict_get<'a>(dict: XmlNode<'a, 'a>, key: &str) -> Option<XmlNode<'a, 'a>> {
    let mut children = dict.children().filter(|n| n.is_element());
    while let Some(k) = children.next() {
        let value = children.next()?;
        if k.has_tag_name("key") && k.text() == Some(key) {
            return Some(value);
        }
    }
    None
}

/// `str(value)` of a plist scalar.
fn plist_text(node: XmlNode) -> String {
    match node.tag_name().name() {
        "true" => "True".into(),
        "false" => "False".into(),
        _ => node.text().unwrap_or_default().to_string(),
    }
}

/// `read_table`: one table from an `sf:tabular-info`.
fn read_table(info: XmlNode) -> Option<SheetTable> {
    let model = child(info, "tabular-model")?;
    let grid = child(model, "grid")?;
    let num_rows = int_attr(grid, "numrows").unwrap_or(0);
    let num_cols = int_attr(grid, "numcols").unwrap_or(0);
    if num_rows <= 0 || num_cols <= 0 {
        return None;
    }
    let (num_rows, num_cols) = (num_rows as usize, num_cols as usize);
    if num_rows.saturating_mul(num_cols) > MAX_TABLE_CELLS {
        eprintln!(
            "warning: iwork: skipping a Numbers table declaring {num_rows} rows by {num_cols} columns"
        );
        return None;
    }
    let count = |n: XmlNode, name| int_attr(n, name).unwrap_or(0).max(0) as usize;
    Some(SheetTable {
        name: sf(model, "name").unwrap_or_default().to_string(),
        num_rows,
        num_cols,
        header_rows: count(model, "num-header-rows"),
        header_cols: count(model, "num-header-columns"),
        cells: read_cells(grid, num_rows, num_cols),
        geometry: legacy_geometry(info),
    })
}

/// `read_cells`: a datasource's flat cell list placed back onto its grid.
/// Each `sf:grid-row` says how many entries belong to it, each
/// `sf:grid-column` how many rows store something in it; a column whose quota
/// is spent is skipped, so a sparse row lands where the spreadsheet shows it.
fn read_cells(grid: XmlNode, num_rows: usize, num_cols: usize) -> Vec<Cell> {
    let (Some(datasource), Some(rows)) = (child(grid, "datasource"), child(grid, "rows")) else {
        return Vec::new();
    };
    let entries: Vec<XmlNode> = datasource.children().filter(|n| n.is_element()).collect();
    let mut quotas = column_quotas(grid, num_cols, entries.len());
    let mut cells = Vec::new();
    let mut consumed = 0usize;
    for (row_index, row) in rows
        .descendants()
        .filter(|n| is_sf(*n, "grid-row"))
        .enumerate()
    {
        if row_index >= num_rows {
            break;
        }
        let count = int_attr(row, "nc").unwrap_or(0).max(0) as usize;
        let end = (consumed + count).min(entries.len());
        let start = consumed.min(end);
        cells.extend(place_row(
            &entries[start..end],
            row_index,
            num_cols,
            quotas.as_mut(),
        ));
        consumed += count;
    }
    cells
}

/// `place_row`: one row's entries across the grid, spending each column's
/// quota. An `sf:s` stands where a merge swallowed a cell: it takes up its
/// column without being counted; a `sf:col-span` cell takes every column it
/// covers.
fn place_row(
    entries: &[XmlNode],
    row_index: usize,
    num_cols: usize,
    mut quotas: Option<&mut Vec<usize>>,
) -> Vec<Cell> {
    let mut cells = Vec::new();
    let mut column = 0usize;
    for &entry in entries {
        if is_sf(entry, "s") {
            column += 1;
            continue;
        }
        if let Some(q) = quotas.as_deref() {
            while column < num_cols && q[column] == 0 {
                column += 1;
            }
        }
        if column >= num_cols {
            break;
        }
        let span =
            (int_attr(entry, "col-span").unwrap_or(1).max(1) as usize).min(num_cols - column);
        if let Some(q) = quotas.as_deref_mut() {
            for slot in &mut q[column..column + span] {
                *slot = slot.saturating_sub(1);
            }
        }
        if let Some(text) = cell_text(entry).filter(|t| !t.is_empty()) {
            cells.push(Cell {
                row: row_index,
                col: column,
                text,
                col_span: span,
                picture: None,
            });
        }
        column += span;
    }
    cells
}

/// `column_quotas`: how many entries each column stores, `None` when the
/// counts do not describe the datasource (each row then lays out from its
/// first column).
fn column_quotas(grid: XmlNode, num_cols: usize, entry_count: usize) -> Option<Vec<usize>> {
    let columns = child(grid, "columns")?;
    let quotas: Vec<usize> = columns
        .descendants()
        .filter(|n| is_sf(*n, "grid-column"))
        .map(|c| int_attr(c, "nc").unwrap_or(0).max(0) as usize)
        .collect();
    (quotas.len() == num_cols && quotas.iter().sum::<usize>() <= entry_count).then_some(quotas)
}

/// `cell_text`: one cell rendered, `None` when it holds nothing readable
/// (`sf:g`, an empty cell, only holds its place).
fn cell_text(cell: XmlNode) -> Option<String> {
    if !cell.is_element() || cell.tag_name().namespace() != Some(SF_NS) {
        return None;
    }
    match cell.tag_name().name() {
        "t" => text_value(cell),
        "n" => float_attr(cell, "v").map(|v| format_number(Number::Double(v))),
        "cb" | "b" => float_attr(cell, "v").map(format_bool),
        "d" => float_attr(cell, "cell-date").and_then(format_date),
        "pm" => popup_text(cell),
        "f" => formula_text(cell),
        _ => None,
    }
}

/// `text_value`: a text cell's string, an attribute or an inline run.
fn text_value(cell: XmlNode) -> Option<String> {
    let content = child(cell, "ct")?;
    let text = match sfa(content, "s") {
        Some(s) => s.to_string(),
        None => content
            .descendants()
            .filter(|n| n.is_text())
            .filter_map(|n| n.text())
            .collect(),
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// `popup_text`: a pop-up menu cell, through the choice it points at.
fn popup_text(cell: XmlNode) -> Option<String> {
    let chosen = sfa(child(cell, "proxied-cell-ref")?, "IDREF")?;
    cell.descendants()
        .find(|c| sfa(*c, "ID") == Some(chosen))
        .and_then(cell_text)
}

/// `formula_text`: a formula cell's cached result, not its expression.
fn formula_text(cell: XmlNode) -> Option<String> {
    let result = child(cell, "r")?;
    for value in result.children().filter(|n| n.is_element()) {
        if is_sf(value, "rn") {
            return float_attr(value, "v").map(|v| format_number(Number::Double(v)));
        }
        if is_sf(value, "rt") {
            return text_value(value);
        }
        if is_sf(value, "rd") {
            return float_attr(value, "cell-date").and_then(format_date);
        }
        if is_sf(value, "rb") {
            return float_attr(value, "v").map(format_bool);
        }
    }
    None
}

/// `strings`: the `sf:string` children of a container, as plain text.
fn strings(container: Option<XmlNode>) -> Vec<String> {
    container
        .map(|c| {
            c.children()
                .filter(|n| n.is_element())
                .map(|s| sfa(s, "string").unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// `read_chart`: one chart from an `sf:chart-info`. A chart bound to a table
/// keeps its values in a share: the names come from the chart, the numbers
/// (and, where they disagree, the names) from the share. `None` when it names
/// neither a row nor a column.
fn read_chart(
    info: XmlNode,
    shares: &HashMap<String, Share>,
) -> Option<(Chart, Option<super::keynote::Geometry>)> {
    let mut row_names = strings(descendant(info, "chart-row_names"));
    let mut column_names = strings(descendant(info, "chart-column_names"));
    let mut rows: Vec<Vec<Option<f64>>> = Vec::new();
    let key = descendant(info, "entity-id").and_then(|e| sfa(e, "string"));
    if let Some(share) = shares.get(key.unwrap_or_default()) {
        if !share.columns.is_empty() {
            column_names = share.columns.clone();
        }
        if !share.rows.is_empty() {
            row_names = share.rows.iter().map(|(n, _)| n.clone()).collect();
            rows = share.rows.iter().map(|(_, v)| v.clone()).collect();
        }
    }
    if row_names.is_empty() && column_names.is_empty() {
        return None;
    }
    let point =
        |row: usize, column: usize| rows.get(row).and_then(|v| v.get(column)).copied().flatten();
    let by_row = descendant(info, "formula-chart-model")
        .and_then(|m| int_attr(m, "chart-direction"))
        == Some(SERIES_BY_ROW);
    let (categories, series) = if by_row {
        let series = row_names
            .iter()
            .enumerate()
            .map(|(row, name)| ChartSeries {
                name: name.clone(),
                values: (0..column_names.len()).map(|c| point(row, c)).collect(),
            })
            .collect();
        (column_names, series)
    } else {
        let series = column_names
            .iter()
            .enumerate()
            .map(|(column, name)| ChartSeries {
                name: name.clone(),
                values: (0..row_names.len()).map(|r| point(r, column)).collect(),
            })
            .collect();
        (row_names, series)
    };
    let title = descendant(info, "chart-name")
        .and_then(|n| sfa(n, "string"))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    Some((
        Chart {
            label: legacy_chart_label(int_attr(info, "chart-type").unwrap_or(0)).to_string(),
            title,
            categories,
            series,
        },
        legacy_geometry(info),
    ))
}

/// `read_comment`: an `sf:sticky-note` — its text only, iWork '09 recorded
/// neither who wrote it nor when.
fn read_comment(note: XmlNode) -> Option<Comment> {
    let text: Vec<String> = note
        .descendants()
        .filter(|n| is_sf(*n, "p"))
        .map(|p| {
            p.descendants()
                .filter(|n| n.is_text())
                .filter_map(|n| n.text())
                .collect::<String>()
                .trim()
                .to_string()
        })
        .filter(|l| !l.is_empty())
        .collect();
    if text.is_empty() {
        return None;
    }
    Some(Comment {
        text: text.join("\n"),
        author: String::new(),
        timestamp: None,
        geometry: legacy_geometry(note),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// docling's `test_iwork09_chart_kinds_follow_the_keynote_09_scripting_dictionary`.
    #[test]
    fn iwork09_chart_kinds_follow_the_scripting_dictionary() {
        assert_eq!(legacy_chart_label(1), "pie_chart");
        assert_eq!(legacy_chart_label(15), "other_chart");
        assert_eq!(legacy_chart_label(7), "bar_chart");
        assert_eq!(legacy_chart_label(0), "other_chart");
        assert_eq!(legacy_chart_label(99), "other_chart");
    }
}
