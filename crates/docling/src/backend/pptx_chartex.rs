//! The chart types Office 2016 added — waterfall, treemap, sunburst,
//! funnel, Pareto, histogram, box-and-whisker, region map — read as
//! docling's `read_chartex` reads them (docling#5371, #1165).
//!
//! PowerPoint keeps these in a `cx:chartSpace` part (the "chartex" format,
//! content type `application/vnd.ms-office.chartex+xml`), not in the
//! `c:chartSpace` of the older chart types, and wraps their graphic frame in
//! an `mc:AlternateContent` whose `mc:Choice Requires="cx1"` holds the
//! frame and whose `mc:Fallback` holds a picture of the chart, or nothing
//! ([`super::mc::resolve_chartex_frames`] keeps the frame and notes the
//! picture). The part caches the data the chart is drawn from; what Office
//! computes when it draws — histogram bins, quartiles, Pareto percentages —
//! is not in it.

use std::collections::HashMap;

use roxmltree::{Document, Node as XmlNode};

use super::numfmt::python_float;
use super::pptx_chart::PptxChart;
use super::xlsx_drawings::{build_chart_grid, MAX_CHART_TABLE_CELLS};

/// The chartex namespace — also the `uri` of the `a:graphicData` holding a
/// chartex chart in a drawing.
pub(crate) const CHARTEX_NS: &str = "http://schemas.microsoft.com/office/drawing/2014/chartex";

/// `_LAYOUT_TO_CLASSIFICATION`: a series' `layoutId` as a classification
/// label. A histogram and the bars of a Pareto chart are `clusteredColumn`
/// series; a sunburst is a ring chart, like `c:doughnutChart`; any other
/// layout (`treemap` included) is `other_chart`.
fn layout_label(layout: &str) -> &'static str {
    match layout {
        "clusteredColumn" | "waterfall" | "funnel" => "bar_chart",
        "paretoLine" => "line_chart",
        "boxWhisker" => "box_plot",
        "sunburst" => "pie_chart",
        "regionMap" => "geographical_map",
        _ => "other_chart",
    }
}

/// The chart a part that cannot be read leaves: `other_chart`, no title, no
/// data.
pub(crate) fn unreadable() -> PptxChart {
    PptxChart {
        labels: vec!["other_chart"],
        title: None,
        table: None,
    }
}

/// `read_chartex`: the kind, title and data of a chartex part. `None` when
/// the part does not parse (the caller keeps the picture without data); a
/// part without a `cx:chart` gives [`unreadable`]'s content.
pub(crate) fn parse(xml: &str) -> Option<PptxChart> {
    let doc = Document::parse(xml).ok()?;
    let space = doc.root_element();
    let Some(chart) = child(space, "chart") else {
        return Some(unreadable());
    };
    let series: Vec<XmlNode> = child(chart, "plotArea")
        .and_then(|p| child(p, "plotAreaRegion"))
        .map(|r| children(r, "series").collect())
        .unwrap_or_default();
    let mut labels: Vec<&'static str> = Vec::new();
    for ser in &series {
        let label = layout_label(ser.attribute("layoutId").unwrap_or(""));
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    if labels.is_empty() {
        labels.push("other_chart");
    }
    let title = child(chart, "title").and_then(|t| text(child(t, "tx")));
    let corner = category_axis_title(chart);
    Some(PptxChart {
        labels,
        title,
        table: table(space, &series, &corner),
    })
}

fn is_cx(n: &XmlNode, name: &str) -> bool {
    n.is_element() && n.tag_name().name() == name && n.tag_name().namespace() == Some(CHARTEX_NS)
}

fn child<'a, 'i>(n: XmlNode<'a, 'i>, name: &str) -> Option<XmlNode<'a, 'i>> {
    n.children().find(|c| is_cx(c, name))
}

fn children<'a, 'i: 'a>(
    n: XmlNode<'a, 'i>,
    name: &'a str,
) -> impl Iterator<Item = XmlNode<'a, 'i>> + 'a {
    n.children().filter(move |c| is_cx(c, name))
}

/// `_category_axis_title`: the title of the axis with `cx:catScaling`.
fn category_axis_title(chart: XmlNode) -> String {
    child(chart, "plotArea")
        .into_iter()
        .flat_map(|p| children(p, "axis"))
        .find(|axis| child(*axis, "catScaling").is_some())
        .and_then(|axis| child(axis, "title"))
        .and_then(|t| text(child(t, "tx")))
        .unwrap_or_default()
}

/// `_text`: a `cx:tx`'s plain value (`cx:txData/cx:v`) or its rich text's
/// paragraphs (`a:p`, their `a:t` runs) joined with line breaks; `None`
/// when blank.
fn text(tx: Option<XmlNode>) -> Option<String> {
    let tx = tx?;
    let value = match child(tx, "txData").and_then(|d| child(d, "v")) {
        Some(v) => v.text().unwrap_or("").to_string(),
        None => tx
            .descendants()
            .filter(|n| n.is_element() && n.tag_name().name() == "p")
            .map(|p| {
                p.descendants()
                    .filter(|n| n.is_element() && n.tag_name().name() == "t")
                    .filter_map(|t| t.text())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n"),
    };
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// `_table`: the category levels of the first series with data, then each
/// value dimension (not typed `cat`) of each series with a `cx:dataId`, as
/// a column headed by the series name (`name (type)` with several). A
/// series without data — the Pareto line Office derives — gets no column.
fn table(space: XmlNode, series: &[XmlNode], corner: &str) -> Option<docling_core::Table> {
    let data_by_id: HashMap<&str, XmlNode> = child(space, "chartData")
        .into_iter()
        .flat_map(|d| children(d, "data"))
        .filter_map(|d| Some((d.attribute("id")?, d)))
        .collect();
    let mut category_columns: Option<Vec<(String, Vec<String>)>> = None;
    let mut value_columns: Vec<(String, Vec<String>)> = Vec::new();
    for ser in series {
        let Some(data) = child(*ser, "dataId")
            .and_then(|d| d.attribute("val"))
            .and_then(|id| data_by_id.get(id))
        else {
            continue;
        };
        if category_columns.is_none() {
            category_columns = Some(category_columns_of(*data, corner));
        }
        let name = text(child(*ser, "tx")).unwrap_or_default();
        let dims: Vec<XmlNode> = children(*data, "numDim")
            .filter(|d| d.attribute("type") != Some("cat"))
            .collect();
        for dim in &dims {
            let header = if dims.len() == 1 {
                name.clone()
            } else {
                format!("{name} ({})", dim.attribute("type").unwrap_or(""))
            };
            value_columns.push((header, level_values(child(*dim, "lvl"), true)));
        }
    }
    build_chart_grid(category_columns.unwrap_or_default(), value_columns)
}

/// `_category_columns`: the levels of the first `cat` dimension, outer
/// first (the `cx:lvl`s run inner to outer), the first headed by the
/// category axis title, the sparse parent labels filled in.
fn category_columns_of(data: XmlNode, corner: &str) -> Vec<(String, Vec<String>)> {
    let Some(dim) = data
        .children()
        .filter(|c| is_cx(c, "strDim") || is_cx(c, "numDim"))
        .find(|d| d.attribute("type") == Some("cat"))
    else {
        return Vec::new();
    };
    let numeric = dim.tag_name().name() == "numDim";
    let mut levels: Vec<Vec<String>> = children(dim, "lvl")
        .map(|l| level_values(Some(l), numeric))
        .collect();
    levels.reverse();
    fill_parent_labels(levels)
        .into_iter()
        .enumerate()
        .map(|(i, values)| {
            (
                if i == 0 {
                    corner.to_string()
                } else {
                    String::new()
                },
                values,
            )
        })
        .collect()
}

/// `_fill_parent_labels`: a row whose outer cells are empty takes the
/// parent labels of the row above, up to its first label — Excel's sparse
/// hierarchy, where an empty cell means "the parent above". An empty cell
/// after the first label stays empty: the row stops at a higher level.
fn fill_parent_labels(levels: Vec<Vec<String>>) -> Vec<Vec<String>> {
    let num_rows = levels.iter().map(Vec::len).max().unwrap_or(0);
    let mut filled: Vec<Vec<String>> = levels
        .into_iter()
        .map(|mut l| {
            l.resize(num_rows, String::new());
            l
        })
        .collect();
    for row in 1..num_rows {
        let first = filled.iter().position(|l| !l[row].is_empty()).unwrap_or(0);
        for level in filled.iter_mut().take(first) {
            level[row] = level[row - 1].clone();
        }
    }
    filled
}

/// `_level_values`: a `cx:lvl`'s points, one entry per index, sized by the
/// points present — never by `ptCount` — an index that is not a
/// non-negative integer, or is [`MAX_CHART_TABLE_CELLS`] or more, dropped;
/// numbers written the short way (`40.000000000000001` → `40`).
fn level_values(level: Option<XmlNode>, numeric: bool) -> Vec<String> {
    let Some(level) = level else {
        return Vec::new();
    };
    let mut points: HashMap<usize, String> = HashMap::new();
    for pt in children(level, "pt") {
        let idx = pt.attribute("idx").unwrap_or("");
        if idx.is_empty() || !idx.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(index) = idx.parse::<usize>() else {
            continue;
        };
        if index >= MAX_CHART_TABLE_CELLS {
            continue;
        }
        let text = pt.text().unwrap_or("");
        points.insert(
            index,
            if numeric {
                number_text(text)
            } else {
                text.to_string()
            },
        );
    }
    let len = points.keys().max().map_or(0, |m| m + 1);
    let mut values = vec![String::new(); len];
    for (i, v) in points {
        values[i] = v;
    }
    values
}

/// `_number_text`: a cached number the short way — a whole number without
/// `.0` — and anything that is not a finite number as it is.
fn number_text(text: &str) -> String {
    match text.trim().parse::<f64>() {
        Ok(f) if f.is_finite() => python_float(f),
        _ => text.to_string(),
    }
}
