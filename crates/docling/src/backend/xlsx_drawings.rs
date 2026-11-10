//! XLSX drawings, charts, and cell comments — the non-grid halves of docling's
//! `MsExcelDocumentBackend`.
//!
//! Drawings (`xl/drawings/drawingN.xml`) anchor images and chart frames to
//! cell ranges: a `twoCellAnchor` spans `from..to` (docling's bbox is
//! `(from.col, from.row, to.col+1, to.row+1)`), a `oneCellAnchor` covers a
//! single cell, and a chart sheet's `absoluteAnchor` has no cell range at all
//! (docling's bbox is `(0, 0, 0, 0)`). Charts (`xl/charts/chartN.xml`) carry their series as
//! *references* back into the workbook (`'Sheet1'!$B$2:$B$7`), which docling
//! resolves against the live cell values; the reconstructed grid (categories
//! down the first column as row headers, one column per series) becomes the
//! picture's tabular-chart annotation. Comments pair the legacy
//! `xl/commentsN.xml` part with the Excel-365 threaded-comment XML, preferring
//! the latter's author/timestamp.

use std::collections::HashMap;

use roxmltree::{Document, Node as XmlNode};

/// A drawing anchor: what it holds and its cell-range bbox
/// `(left_col, top_row, right_col_excl, bottom_row_excl)`.
pub struct DrawingItem {
    pub bbox: (usize, usize, usize, usize),
    pub kind: DrawingKind,
    /// The picture's alt text (#676): `xdr:nvPicPr/xdr:cNvPr/@descr`
    /// (else `@title`); `None` for a chart or an untitled picture.
    pub alt: Option<String>,
}

pub enum DrawingKind {
    /// `<xdr:pic>` with an `<a:blip r:embed>` relationship id.
    Image(String),
    /// `<xdr:graphicFrame>` referencing a chart part by relationship id.
    Chart(String),
}

/// Parse a spreadsheet drawing part into its anchored items, in document order.
pub fn parse_drawing(xml: &str) -> Vec<DrawingItem> {
    let Ok(dom) = Document::parse(xml) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for anchor in dom.root_element().children().filter(|n| {
        matches!(
            n.tag_name().name(),
            "twoCellAnchor" | "oneCellAnchor" | "absoluteAnchor"
        )
    }) {
        let cell = |tag: &str| -> Option<(usize, usize)> {
            let n = anchor.children().find(|c| c.has_tag_name(tag))?;
            let num = |t: &str| {
                n.children()
                    .find(|c| c.has_tag_name(t))
                    .and_then(|c| c.text())
                    .and_then(|s| s.trim().parse::<usize>().ok())
            };
            Some((num("col")?, num("row")?))
        };
        // A chart *sheet* anchors its one chart absolutely (`<absoluteAnchor>`
        // with an EMU position, no cell range) — the anchor type docling's
        // `_anchor_to_tuple` knows nothing about and maps to `(0, 0, 0, 0)`,
        // which is the bbox its chart-sheet pictures carry (#405). Skipping
        // the anchor, as we did, left the sheet's group empty.
        let bbox = if anchor.has_tag_name("absoluteAnchor") {
            (0, 0, 0, 0)
        } else {
            let Some((fc, fr)) = cell("from") else {
                continue;
            };
            match cell("to") {
                Some((tc, tr)) => (fc, fr, tc + 1, tr + 1),
                None => (fc, fr, fc + 1, fr + 1),
            }
        };
        let mut alt = None;
        let kind = if let Some(blip) = anchor.descendants().find(|n| {
            n.has_tag_name("blip") && !n.ancestors().any(|a| a.has_tag_name("graphicFrame"))
        }) {
            alt = blip
                .ancestors()
                .find(|a| a.has_tag_name("pic"))
                .and_then(|p| p.children().find(|c| c.has_tag_name("nvPicPr")))
                .and_then(|n| n.children().find(|c| c.has_tag_name("cNvPr")))
                .and_then(super::ooxml::alt_text_of);
            match blip.attributes().find(|a| a.name() == "embed") {
                Some(a) => DrawingKind::Image(a.value().to_string()),
                None => continue,
            }
        } else if let Some(chart) = anchor.descendants().find(|n| n.has_tag_name("chart")) {
            match chart.attributes().find(|a| a.name() == "id") {
                Some(a) => DrawingKind::Chart(a.value().to_string()),
                None => continue,
            }
        } else {
            continue;
        };
        out.push(DrawingItem { bbox, kind, alt });
    }
    out
}

/// A chart's declarative content: docling's classification label, the title
/// (caption), and the series with their workbook references.
pub struct ChartSpec {
    /// The chart's main label: the back-most plot's.
    pub kind: &'static str,
    /// One label per plot type, back-most first (docling#4812's
    /// `_chart_classifications`; a combination chart has several).
    pub kinds: Vec<&'static str>,
    pub title: Option<String>,
    /// The series of every plot, in drawing order (docling#4812).
    pub series: Vec<SeriesSpec>,
}

pub struct SeriesSpec {
    /// The series name: a resolvable reference, or a literal value.
    pub name_ref: Option<String>,
    pub name_lit: Option<String>,
    /// Categories (`c:cat` / `c:xVal`) reference.
    pub cat_ref: Option<String>,
    /// Values (`c:val` / `c:yVal`) reference.
    pub val_ref: Option<String>,
    /// Cached category/value points (`strCache`/`numCache` `c:pt` entries) —
    /// the only data DOCX/PPTX charts carry (no workbook to resolve against).
    pub cat_cache: Vec<String>,
    pub val_cache: Vec<String>,
    /// Cached series name (the `c:tx` reference's `strCache`).
    pub name_cache: Option<String>,
    /// The title of the axis that holds the series' plot's categories (the
    /// plot's x axis; none for a pie or doughnut) — the top-left cell of an
    /// XLSX chart's data table (docling#4812).
    pub axis_title: String,
}

/// docling's `_CHART_TAGNAME_TO_CLASSIFICATION`.
fn classification(tag: &str) -> Option<&'static str> {
    Some(match tag {
        "barChart" | "bar3DChart" => "bar_chart",
        "lineChart" | "line3DChart" => "line_chart",
        "pieChart" | "pie3DChart" | "doughnutChart" => "pie_chart",
        "scatterChart" => "scatter_chart",
        "areaChart" | "area3DChart" => "other_chart",
        _ => return None,
    })
}

/// Parse `chartN.xml` into a [`ChartSpec`]. Every plot of `c:plotArea`
/// counts (a combination chart has one per chart type, docling#4812): its
/// label, its series and its category axis. Unknown plot kinds fall back to
/// `other_chart`.
pub fn parse_chart(xml: &str) -> Option<ChartSpec> {
    let dom = Document::parse(xml).ok()?;
    let plot_area = dom.descendants().find(|n| n.has_tag_name("plotArea"))?;
    let plots: Vec<XmlNode> = plot_area
        .children()
        .filter(|n| n.tag_name().name().ends_with("Chart"))
        .collect();
    if plots.is_empty() {
        return None;
    }
    let mut kinds: Vec<&'static str> = Vec::new();
    for plot in &plots {
        let label = classification(plot.tag_name().name()).unwrap_or("other_chart");
        if !kinds.contains(&label) {
            kinds.push(label);
        }
    }
    // The DOCX and PPTX backends classify by the first plot docling's map
    // knows; the XLSX one by the back-most plot. They agree but for a
    // combination chart whose first plot is of an unknown kind.
    let kind = plots
        .iter()
        .find_map(|p| classification(p.tag_name().name()))
        .unwrap_or("other_chart");

    // `c:chart/c:title`: its rich-text runs, else a cell-linked title's
    // cached text (`c:strRef`) — never an axis title.
    let title = dom
        .descendants()
        .find(|n| n.has_tag_name("chart"))
        .and_then(|c| c.children().find(|n| n.has_tag_name("title")))
        .and_then(title_text);

    // A reference formula from a `c:tx`/`c:cat`/`c:val`-style node: its
    // `numRef`/`strRef` child's `c:f` text (docling's `_ref_formula`).
    let ref_formula = |node: XmlNode| -> Option<String> {
        node.children()
            .find(|c| matches!(c.tag_name().name(), "numRef" | "strRef"))
            .and_then(|r| r.children().find(|c| c.has_tag_name("f")))
            .and_then(|f| f.text())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let mut series = Vec::new();
    for plot in &plots {
        let axis_title = x_axis(plot_area, *plot)
            .and_then(|axis| axis.children().find(|n| n.has_tag_name("title")))
            .and_then(title_text)
            .unwrap_or_default();
        for ser in plot.children().filter(|n| n.has_tag_name("ser")) {
            let child = |tag: &str| ser.children().find(|c| c.has_tag_name(tag));
            let name_ref = child("tx").and_then(ref_formula);
            let name_lit = child("tx")
                .and_then(|tx| tx.children().find(|c| c.has_tag_name("v")))
                .and_then(|v| v.text())
                .map(str::to_string);
            let cat_ref = child("cat")
                .and_then(ref_formula)
                .or_else(|| child("xVal").and_then(ref_formula));
            let val_ref = child("val")
                .and_then(ref_formula)
                .or_else(|| child("yVal").and_then(ref_formula));
            let cat_cache = child("cat")
                .or_else(|| child("xVal"))
                .map(cache_points)
                .unwrap_or_default();
            let val_cache = child("val")
                .or_else(|| child("yVal"))
                .map(cache_points)
                .unwrap_or_default();
            let name_cache = child("tx")
                .map(cache_points)
                .and_then(|v| v.into_iter().next());
            series.push(SeriesSpec {
                name_ref,
                name_lit,
                cat_ref,
                val_ref,
                cat_cache,
                val_cache,
                name_cache,
                axis_title: axis_title.clone(),
            });
        }
    }
    Some(ChartSpec {
        kind,
        kinds,
        title,
        series,
    })
}

/// openpyxl's `plot.x_axis`: the axis the plot's first `c:axId` names — a
/// category or date axis, a scatter or bubble plot's x value axis. A pie or
/// doughnut plot has none (docling's `_category_axis_title`).
fn x_axis<'a, 'i>(plot_area: XmlNode<'a, 'i>, plot: XmlNode) -> Option<XmlNode<'a, 'i>> {
    if matches!(
        plot.tag_name().name(),
        "pieChart" | "pie3DChart" | "doughnutChart" | "ofPieChart"
    ) {
        return None;
    }
    let id = plot
        .children()
        .find(|n| n.has_tag_name("axId"))?
        .attribute("val")?;
    plot_area.children().find(|axis| {
        matches!(
            axis.tag_name().name(),
            "catAx" | "dateAx" | "valAx" | "serAx"
        ) && axis
            .children()
            .find(|n| n.has_tag_name("axId"))
            .and_then(|n| n.attribute("val"))
            == Some(id)
    })
}

/// A chart or axis title's text (docling's `_title_text`): its rich-text
/// runs joined, else a cell-linked title's cached text; `None` when empty.
fn title_text(title: XmlNode) -> Option<String> {
    let runs: String = title
        .descendants()
        .filter(|n| n.has_tag_name("t"))
        .filter_map(|n| n.text())
        .collect();
    let text = match runs.trim() {
        "" => cache_points(title)
            .into_iter()
            .filter(|v| !v.is_empty())
            .collect::<String>(),
        runs => runs.to_string(),
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The cached points under a `c:cat`/`c:val`/`c:tx` node (docling's
/// `_read_chart_cache`): the first `numCache`, `strCache`, `numLit` or
/// `strLit` under it, each `c:pt`'s `c:v` at its `@idx`, the gaps empty and
/// the list `c:ptCount` long — so every series keeps its rows aligned (the
/// points used to be packed, shifting the values after a gap). Numbers
/// render openpyxl-style (no trailing `.0`) — DOCX/PPTX charts carry their
/// data only in these caches.
fn cache_points(node: XmlNode) -> Vec<String> {
    let Some(cache) = ["numCache", "strCache", "numLit", "strLit"]
        .iter()
        .find_map(|tag| node.descendants().find(|c| c.has_tag_name(*tag)))
    else {
        return Vec::new();
    };
    let mut points: Vec<(usize, String)> = Vec::new();
    for pt in cache.children().filter(|c| c.has_tag_name("pt")) {
        let Some(idx) = pt.attribute("idx").unwrap_or("0").parse::<usize>().ok() else {
            continue;
        };
        let v = pt
            .children()
            .find(|c| c.has_tag_name("v"))
            .and_then(|v| v.text())
            .map(format_cached_value)
            .unwrap_or_default();
        // A repeated index keeps its last value, as a dict does.
        match points.iter_mut().find(|(i, _)| *i == idx) {
            Some(slot) => slot.1 = v,
            None => points.push((idx, v)),
        }
    }
    if points.is_empty() {
        return Vec::new();
    }
    let count = cache
        .children()
        .find(|c| c.has_tag_name("ptCount"))
        .and_then(|c| c.attribute("val"))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    // A crafted `ptCount` or `@idx` cannot allocate past the table's cell
    // cap; points beyond it are dropped, as the table would cut them.
    let last = points.iter().map(|(i, _)| i + 1).max().unwrap_or(0);
    let mut out = vec![String::new(); last.max(count).min(MAX_CHART_TABLE_CELLS)];
    for (i, v) in points {
        if let Some(slot) = out.get_mut(i) {
            *slot = v;
        }
    }
    out
}

/// A cached value, numbers normalized like openpyxl's `str(value)` (an
/// integer-valued float loses its `.0`).
fn format_cached_value(v: &str) -> String {
    match v.trim().parse::<f64>() {
        Ok(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 => {
            format!("{}", f as i64)
        }
        Ok(f) => format!("{f}"),
        Err(_) => v.to_string(),
    }
}

/// docling's `MAX_CHART_TABLE_CELLS` (`backend/utils/charts.py`): the most
/// cells one chart's data table holds.
pub const MAX_CHART_TABLE_CELLS: usize = 100_000;

/// docling's `build_chart_table` (`backend/utils/charts.py`, shared by the
/// PPTX and XLSX backends since docling#4812): trailing rows with no
/// category and no value are dropped (interior gaps stay), the table is
/// capped at [`MAX_CHART_TABLE_CELLS`], then laid out by [`chart_table`].
pub fn build_chart_table(
    mut categories: Vec<String>,
    mut columns: Vec<(String, Vec<String>)>,
    corner: String,
) -> Option<docling_core::Table> {
    let declared = columns
        .iter()
        .map(|(_, v)| v.len())
        .chain([categories.len()])
        .max()
        .unwrap_or(0);
    let populated = (0..declared)
        .rev()
        .find(|&row| {
            categories.get(row).is_some_and(|c| !c.is_empty())
                || columns
                    .iter()
                    .any(|(_, v)| v.get(row).is_some_and(|s| !s.is_empty()))
        })
        .map_or(0, |row| row + 1);
    if populated == 0 {
        return None;
    }
    // The header row counts against the cap (docling#5030, #1020): with it,
    // `num_cols` columns hold at most `MAX / num_cols - 1` data rows. Leaving
    // it out let 1 series × 50,000 points emit 100,002 cells.
    let num_cols = 1 + columns.len();
    let rows = if (populated + 1) * num_cols > MAX_CHART_TABLE_CELLS {
        (MAX_CHART_TABLE_CELLS / num_cols).saturating_sub(1).max(1)
    } else {
        populated
    };
    categories.truncate(rows);
    categories.resize(rows, String::new());
    for (_, values) in &mut columns {
        values.truncate(rows);
    }
    chart_table(categories, columns, corner)
}

/// docling's `_chart_to_table_data` grid from resolved series columns:
/// categories down the first column (row headers), one column per series
/// (column headers), the top-left cell empty. Shared by the XLSX backend
/// (which resolves references into sheets) and the DOCX/PPTX backends
/// (which use the embedded caches).
pub fn chart_table_from_columns(
    categories: Vec<String>,
    columns: Vec<(String, Vec<String>)>,
) -> Option<docling_core::Table> {
    chart_table(categories, columns, String::new())
}

/// [`chart_table_from_columns`] with `corner` in the top-left cell — the
/// PPTX backend puts the category axis title there (docling#4773).
pub fn chart_table(
    categories: Vec<String>,
    columns: Vec<(String, Vec<String>)>,
    corner: String,
) -> Option<docling_core::Table> {
    let num_data_rows = columns
        .iter()
        .map(|(_, v)| v.len())
        .chain([categories.len()])
        .max()
        .unwrap_or(0);
    if num_data_rows == 0 || columns.is_empty() {
        return None;
    }
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut header = vec![corner];
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
    Some(docling_core::Table {
        footnotes: Vec::new(),
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
        cells: None,
        caption: None,
        caption_parent: Default::default(),
        caption_location: None,
        document_index: false,
    })
}

/// A chart's data table built purely from the embedded caches (the DOCX/PPTX
/// path — no workbook). `None` when the chart carries no cached data.
pub fn chart_table_from_caches(spec: &ChartSpec) -> Option<docling_core::Table> {
    if spec.series.is_empty() {
        return None;
    }
    let categories = spec
        .series
        .iter()
        .map(|s| s.cat_cache.clone())
        .find(|c| !c.is_empty())
        .unwrap_or_default();
    let columns: Vec<(String, Vec<String>)> = spec
        .series
        .iter()
        .map(|s| {
            let name = s
                .name_cache
                .clone()
                .or_else(|| s.name_lit.clone())
                .unwrap_or_default();
            (name, s.val_cache.clone())
        })
        .collect();
    chart_table_from_columns(categories, columns)
}

/// 0-based inclusive range bounds `(min_col, min_row, max_col, max_row)`.
pub type RangeBounds = (usize, usize, usize, usize);

/// Split a range reference (`'Duck Observations'!$B$2:$B$7`) into the sheet
/// name (unquoted, `''` unescaped; `None` when unqualified) and its bounds.
pub fn parse_range_ref(reference: &str) -> Option<(Option<String>, RangeBounds)> {
    let (sheet, cells) = match reference.rsplit_once('!') {
        Some((s, c)) => {
            let s = s.trim();
            let name = if s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2 {
                s[1..s.len() - 1].replace("''", "'")
            } else {
                s.to_string()
            };
            (Some(name), c)
        }
        None => (None, reference),
    };
    let mut corners = cells.split(':');
    let start = corners.next()?;
    let a = cell_ref(start).or_else(|| Some((column_ref(start)?, 0)))?;
    let b = match corners.next() {
        // A whole-column reference (`$B:$B`) runs to Excel's last row; the
        // reader stops at the sheet's last used one (docling#4812).
        Some(c) => cell_ref(c).or_else(|| Some((column_ref(c)?, EXCEL_MAX_ROW)))?,
        None => a,
    };
    Some((
        sheet,
        (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)),
    ))
}

/// Excel's last row, 0-based.
const EXCEL_MAX_ROW: usize = 1_048_575;

/// `$B` → the 0-based column of a whole-column reference.
fn column_ref(s: &str) -> Option<usize> {
    let s = s.replace('$', "");
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_alphabetic()) || s.len() > 3 {
        return None;
    }
    let col = s.chars().fold(0usize, |acc, c| {
        acc * 26 + (c.to_ascii_uppercase() as usize - 'A' as usize + 1)
    });
    Some(col - 1)
}

/// `$B$7` → 0-based `(col, row)`.
fn cell_ref(s: &str) -> Option<(usize, usize)> {
    let s = s.replace('$', "");
    let letters: String = s.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    let digits: String = s.chars().skip_while(|c| c.is_ascii_alphabetic()).collect();
    if letters.is_empty() || digits.is_empty() {
        return None;
    }
    let col = letters.chars().fold(0usize, |acc, c| {
        acc * 26 + (c.to_ascii_uppercase() as usize - 'A' as usize + 1)
    });
    Some((col - 1, digits.parse::<usize>().ok()? - 1))
}

/// Parse the legacy comments part (`xl/commentsN.xml`) into per-cell
/// `(ref, author, text)` entries, in part order.
pub fn parse_legacy_comments(xml: &str) -> Vec<(String, String, String)> {
    let Ok(dom) = Document::parse(xml) else {
        return Vec::new();
    };
    let authors: Vec<String> = dom
        .descendants()
        .find(|n| n.has_tag_name("authors"))
        .map(|a| {
            a.children()
                .filter(|c| c.has_tag_name("author"))
                .map(|c| c.text().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();
    let mut out = Vec::new();
    for c in dom.descendants().filter(|n| n.has_tag_name("comment")) {
        let cell = c
            .attributes()
            .find(|a| a.name() == "ref")
            .map(|a| a.value().to_string())
            .unwrap_or_default();
        let author = c
            .attributes()
            .find(|a| a.name() == "authorId")
            .and_then(|a| a.value().parse::<usize>().ok())
            .and_then(|i| authors.get(i).cloned())
            .unwrap_or_default();
        let text: String = c
            .descendants()
            .filter(|n| n.has_tag_name("t"))
            .filter_map(|n| n.text())
            .collect();
        out.push((cell, author, text.trim().to_string()));
    }
    out
}

/// Parse a threaded-comments part into `ref -> (author, text, time)` using the
/// persons map (`personId -> displayName`).
pub fn parse_threaded_comments(
    xml: &str,
    persons: &HashMap<String, String>,
) -> HashMap<String, Vec<(String, String, Option<String>)>> {
    let Ok(dom) = Document::parse(xml) else {
        return HashMap::new();
    };
    // Per cell: (id, parentId, (author, text, time)) in document order.
    let mut per_cell: HashMap<String, Vec<ThreadEntry>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for c in dom
        .descendants()
        .filter(|n| n.has_tag_name("threadedComment"))
    {
        let attr = |name: &str| {
            c.attributes()
                .find(|a| a.name() == name)
                .map(|a| a.value().to_string())
        };
        let Some(cell) = attr("ref") else { continue };
        let author = attr("personId")
            .and_then(|id| persons.get(&id).cloned())
            .unwrap_or_else(|| "Unknown".to_string());
        let text = c
            .children()
            .find(|n| n.has_tag_name("text"))
            .and_then(|t| t.text())
            .unwrap_or("")
            .to_string();
        let time = attr("dT").map(|t| format_comment_time(&t));
        if !per_cell.contains_key(&cell) {
            order.push(cell.clone());
        }
        per_cell.entry(cell).or_default().push((
            attr("id"),
            attr("parentId"),
            (author, text, time),
        ));
    }
    order
        .into_iter()
        .filter_map(|cell| {
            let entries = per_cell.remove(&cell)?;
            Some((cell, order_comment_thread(entries)))
        })
        .collect()
}

/// One `threadedComment`: its `id`, its `parentId`, and `(author, text, time)`.
type ThreadEntry = (
    Option<String>,
    Option<String>,
    (String, String, Option<String>),
);

/// Order one cell's `(id, parentId, comment)` entries as a thread — docling's
/// `_order_comment_thread` (docling#4353). OOXML does not fix the element
/// order of threaded comments, so the thread follows the id/parentId links:
/// each comment comes before its replies, and replies to the same comment
/// are sorted by timestamp, then document order. A comment whose parent is
/// missing is a root. Comments that no root reaches (a parentId cycle) are
/// kept at the end, in document order.
fn order_comment_thread(entries: Vec<ThreadEntry>) -> Vec<(String, String, Option<String>)> {
    // The rendered time is `YYYY-MM-DDTHH:MM:SS(.fff)`, so it sorts as text;
    // an absent time sorts after every present one (`(True, datetime.min)`).
    let sort_key = |i: usize| -> (bool, String, usize) {
        let time = entries[i].2 .2.clone();
        (time.is_none(), time.unwrap_or_default(), i)
    };
    let ids: std::collections::HashSet<&str> = entries
        .iter()
        .filter_map(|(id, _, _)| id.as_deref())
        .filter(|id| !id.is_empty())
        .collect();
    let mut roots: Vec<usize> = Vec::new();
    let mut children: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, (_, parent, _)) in entries.iter().enumerate() {
        match parent.as_deref().filter(|p| ids.contains(p)) {
            Some(p) => children.entry(p).or_default().push(i),
            None => roots.push(i),
        }
    }
    let mut ordered: Vec<usize> = Vec::new();
    let mut visited = vec![false; entries.len()];
    // Depth-first from the roots; a reverse-sorted stack pops the earliest.
    let mut stack = roots;
    stack.sort_by_key(|&i| std::cmp::Reverse(sort_key(i)));
    while let Some(i) = stack.pop() {
        if visited[i] {
            continue;
        }
        visited[i] = true;
        ordered.push(i);
        if let Some(mut replies) = entries[i]
            .0
            .as_deref()
            .and_then(|id| children.get(id).cloned())
        {
            replies.sort_by_key(|&i| std::cmp::Reverse(sort_key(i)));
            stack.extend(replies);
        }
    }
    ordered.extend((0..entries.len()).filter(|&i| !visited[i]));
    ordered.into_iter().map(|i| entries[i].2.clone()).collect()
}

/// `xl/persons/person.xml` → `id -> displayName`.
pub fn parse_persons(xml: &str) -> HashMap<String, String> {
    let Ok(dom) = Document::parse(xml) else {
        return HashMap::new();
    };
    dom.descendants()
        .filter(|n| n.has_tag_name("person"))
        .filter_map(|p| {
            let get = |name: &str| {
                p.attributes()
                    .find(|a| a.name() == name)
                    .map(|a| a.value().to_string())
            };
            Some((get("id")?, get("displayName")?))
        })
        .collect()
}

/// A threaded comment's `dT` timestamp rendered like docling — Python's
/// `datetime.isoformat(timespec="milliseconds")`: the fraction padded/truncated
/// to exactly three digits, a `Z` suffix becoming `+00:00`.
fn format_comment_time(raw: &str) -> String {
    let (base, tz) = match raw.strip_suffix('Z') {
        Some(b) => (b, "+00:00"),
        None => (raw, ""),
    };
    let (secs, frac) = match base.split_once('.') {
        Some((s, f)) => (s, f),
        None => (base, ""),
    };
    let mut ms = frac.to_string();
    ms.truncate(3);
    while ms.len() < 3 {
        ms.push('0');
    }
    format!("{secs}.{ms}{tz}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(n: usize) -> (Vec<String>, Vec<String>) {
        (
            (0..n).map(|i| format!("c{i}")).collect(),
            (0..n).map(|i| i.to_string()).collect(),
        )
    }

    /// docling#5030's `test_chart_data_table_capped_at_max_cells` (#1020):
    /// 60k points in 2 series are cut so the header row fits under the cap —
    /// 3 columns hold `MAX / 3 - 1` data rows.
    #[test]
    fn chart_data_table_capped_at_max_cells() {
        let (categories, values) = points(60_000);
        let table = build_chart_table(
            categories,
            vec![("s1".into(), values.clone()), ("s2".into(), values)],
            String::new(),
        )
        .unwrap();
        let capped = MAX_CHART_TABLE_CELLS / 3 - 1;
        assert_eq!(table.rows[0].len(), 3);
        assert_eq!(table.rows.len(), capped + 1);
        assert!(table.rows.len() * 3 <= MAX_CHART_TABLE_CELLS);
        assert_eq!(table.rows[1][0], "c0");
        assert_eq!(table.rows[capped][0], format!("c{}", capped - 1));
    }

    /// docling#5030's `test_chart_data_table_cap_reached_exactly`: 1 series
    /// × 50k points is 100,002 cells with the header row — capped to exactly
    /// `MAX` cells, where the data-rows-only check let it through.
    #[test]
    fn chart_data_table_cap_reached_exactly() {
        let (categories, values) = points(50_000);
        let table =
            build_chart_table(categories, vec![("s1".into(), values)], String::new()).unwrap();
        assert_eq!(table.rows[0].len(), 2);
        assert_eq!(table.rows.len(), MAX_CHART_TABLE_CELLS / 2);
        assert_eq!(table.rows.len() * 2, MAX_CHART_TABLE_CELLS);
    }

    /// #405: a chart *sheet* anchors its chart with `<absoluteAnchor>` — no
    /// cell range — which docling's `_anchor_to_tuple` maps to `(0, 0, 0, 0)`.
    /// Skipping the anchor left the sheet's group empty.
    #[test]
    fn a_chart_sheets_absolute_anchor_is_a_zero_box_chart() {
        let xml = r#"<wsDr xmlns="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing">
            <absoluteAnchor><pos x="0" y="0"/><ext cx="0" cy="0"/>
              <graphicFrame><a:graphic xmlns:a="a"><a:graphicData uri="chart">
                <c:chart xmlns:c="c" xmlns:r="r" r:id="rId1"/>
              </a:graphicData></a:graphic></graphicFrame><clientData/>
            </absoluteAnchor>
            <oneCellAnchor><from><col>2</col><colOff>0</colOff><row>3</row><rowOff>0</rowOff></from>
              <ext cx="1" cy="1"/>
              <graphicFrame><a:graphic xmlns:a="a"><a:graphicData uri="chart">
                <c:chart xmlns:c="c" xmlns:r="r" r:id="rId2"/>
              </a:graphicData></a:graphic></graphicFrame><clientData/>
            </oneCellAnchor>
        </wsDr>"#;
        let items = parse_drawing(xml);
        let got: Vec<(_, String)> = items
            .iter()
            .map(|i| {
                (
                    i.bbox,
                    match &i.kind {
                        DrawingKind::Chart(id) => id.clone(),
                        DrawingKind::Image(id) => format!("img:{id}"),
                    },
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ((0, 0, 0, 0), "rId1".to_string()),
                ((2, 3, 3, 4), "rId2".to_string()),
            ]
        );
    }

    #[test]
    fn range_refs() {
        assert_eq!(
            parse_range_ref("'Duck Observations'!$B$2:$B$7"),
            Some((Some("Duck Observations".to_string()), (1, 1, 1, 6)))
        );
        assert_eq!(
            parse_range_ref("Sheet1!$A$1"),
            Some((Some("Sheet1".to_string()), (0, 0, 0, 0)))
        );
        assert_eq!(cell_ref("$AB$10"), Some((27, 9)));
        // A whole-column reference runs to Excel's last row (docling#4812).
        assert_eq!(
            parse_range_ref("Sheet1!$B:$B"),
            Some((Some("Sheet1".to_string()), (1, 0, 1, EXCEL_MAX_ROW)))
        );
    }

    /// docling's `_read_chart_cache`: points land at their `@idx`, the gaps
    /// empty and the list `ptCount` long, so a series keeps its rows.
    #[test]
    fn chart_cache_keeps_gaps() {
        let xml = r#"<c:chartSpace xmlns:c="c" xmlns:a="a"><c:chart>
            <c:title><c:tx><c:strRef><c:f>S!$A$1</c:f><c:strCache><c:ptCount val="1"/>
              <c:pt idx="0"><c:v>Linked title</c:v></c:pt></c:strCache></c:strRef></c:tx></c:title>
            <c:plotArea>
            <c:barChart><c:ser><c:tx><c:v>s1</c:v></c:tx>
              <c:val><c:numRef><c:f>S!$B$2:$B$5</c:f><c:numCache><c:ptCount val="4"/>
                <c:pt idx="0"><c:v>1</c:v></c:pt><c:pt idx="2"><c:v>3.5</c:v></c:pt>
              </c:numCache></c:numRef></c:val></c:ser><c:axId val="7"/><c:axId val="8"/></c:barChart>
            <c:lineChart><c:ser><c:val><c:numLit><c:ptCount val="1"/><c:pt idx="0"><c:v>9</c:v></c:pt></c:numLit></c:val></c:ser><c:axId val="7"/></c:lineChart>
            <c:catAx><c:axId val="7"/><c:title><c:tx><c:rich><a:p><a:r><a:t>Year</a:t></a:r></a:p></c:rich></c:tx></c:title></c:catAx>
            </c:plotArea></c:chart></c:chartSpace>"#;
        let spec = parse_chart(xml).expect("chart");
        assert_eq!(spec.kinds, ["bar_chart", "line_chart"]);
        assert_eq!(spec.title.as_deref(), Some("Linked title"));
        assert_eq!(spec.series.len(), 2, "every plot's series");
        assert_eq!(spec.series[0].val_cache, ["1", "", "3.5", ""]);
        assert_eq!(spec.series[1].val_cache, ["9"]);
        assert_eq!(spec.series[0].axis_title, "Year");
    }

    #[test]
    fn comment_time() {
        assert_eq!(
            format_comment_time("2026-06-18T17:15:52.31"),
            "2026-06-18T17:15:52.310"
        );
        assert_eq!(
            format_comment_time("2026-06-18T17:15:52"),
            "2026-06-18T17:15:52.000"
        );
        assert_eq!(
            format_comment_time("2026-06-18T17:15:52.3123Z"),
            "2026-06-18T17:15:52.312+00:00"
        );
    }
}
