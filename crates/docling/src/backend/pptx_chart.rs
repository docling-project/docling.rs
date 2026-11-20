//! A PPTX chart part read the way docling's PowerPoint backend reads it
//! through python-pptx (docling#4773, 2.139): every plot of a combination
//! chart, not just the first.
//!
//! - **Plots**: the plot area's chart elements in document order — every one
//!   of them since docling#4811: a 3-D bar/line/pie, stock, surface or
//!   bar-of-pie plot, which python-pptx has no plot class for, was skipped
//!   as `other_chart` without data; every `c:ser` has the same structure, so
//!   all are read alike now.
//! - **Classification**: one label per plot, by its tag (`bar3DChart` is a
//!   bar chart — the Word and Excel backends' mapping), in drawing order
//!   (the back-most plot first, so it stays the main prediction), each label
//!   once.
//! - **Data table**: the series of every plot as columns, in drawing order
//!   and each plot's `c:order` (a series without one is malformed: the chart
//!   keeps its labels and loses its table, as a value that is not a number
//!   does). The categories come from the first plot that has them, read
//!   from its first series; on a date axis, or a text axis showing a date
//!   number format, the cached serials read as ISO 8601 dates (1900 or 1904
//!   system). That plot's category axis title heads the category column.
//! - **Title**: `c:chart/c:title`'s rich text only — an axis title is not the
//!   chart's.
//!
//! The XLSX and DOCX backends keep their own chart readers
//! ([`super::xlsx_drawings`]): their upstream counterparts did not change.

use roxmltree::{Document, Node as XmlNode};

use super::numfmt::{civil_from_days, days_from_civil};
use std::collections::HashMap;

use super::xlsx_drawings::{build_chart_table, chart_points_to_list, max_chart_data_rows};

/// A PPTX chart as docling records it on the picture.
pub struct PptxChart {
    /// The classification labels, main first (never empty).
    pub labels: Vec<&'static str>,
    /// The chart title, the picture's caption.
    pub title: Option<String>,
    /// The data grid; `None` when no readable plot has a series or no row
    /// carries data.
    pub table: Option<docling_core::Table>,
}

/// The plot elements python-pptx lists (`CT_PlotArea.iter_xCharts`).
const PLOT_TAGS: [&str; 16] = [
    "area3DChart",
    "areaChart",
    "bar3DChart",
    "barChart",
    "bubbleChart",
    "doughnutChart",
    "line3DChart",
    "lineChart",
    "ofPieChart",
    "pie3DChart",
    "pieChart",
    "radarChart",
    "scatterChart",
    "stockChart",
    "surface3DChart",
    "surfaceChart",
];

/// docling's `_CHART_TAGNAME_TO_CLASSIFICATION` (docling#4811): a plot's
/// label by its tag; every other plot is `other_chart`.
fn plot_label(tag: &str) -> &'static str {
    match tag {
        "barChart" | "bar3DChart" => "bar_chart",
        "lineChart" | "line3DChart" => "line_chart",
        "pieChart" | "pie3DChart" | "doughnutChart" => "pie_chart",
        "scatterChart" => "scatter_chart",
        _ => "other_chart",
    }
}

/// `_XY_PLOT_TAGNAMES`: plots whose series keep their values in `c:yVal`.
fn is_xy_plot(tag: &str) -> bool {
    matches!(tag, "scatterChart" | "bubbleChart")
}

fn child<'a, 'i>(node: XmlNode<'a, 'i>, tag: &str) -> Option<XmlNode<'a, 'i>> {
    node.children().find(|c| c.has_tag_name(tag))
}

/// Parse a chart part; `None` when it is not a chart (no plot area).
pub fn parse(xml: &str) -> Option<PptxChart> {
    let dom = Document::parse(xml).ok()?;
    let chart = child(dom.root_element(), "chart")?;
    let plot_area = child(chart, "plotArea")?;
    let date1904 = child(dom.root_element(), "date1904")
        .and_then(|d| d.attribute("val"))
        .is_some_and(|v| matches!(v, "1" | "true"));
    let plots: Vec<XmlNode> = plot_area
        .children()
        .filter(|c| c.is_element() && PLOT_TAGS.contains(&c.tag_name().name()))
        .collect();

    let mut labels: Vec<&'static str> = Vec::new();
    for plot in &plots {
        let label = plot_label(plot.tag_name().name());
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    if labels.is_empty() {
        labels.push("other_chart");
    }

    Some(PptxChart {
        labels,
        title: child(chart, "title").and_then(chart_title),
        table: chart_data(plot_area, &plots, date1904),
    })
}

/// python-pptx's `chart_title.text_frame.text`, stripped: the rich text's
/// paragraphs joined with newlines (a line break is `\v`).
fn chart_title(title: XmlNode) -> Option<String> {
    let rich = child(title, "tx").and_then(|tx| child(tx, "rich"))?;
    let text = rich
        .children()
        .filter(|p| p.has_tag_name("p"))
        .map(|p| {
            let mut s = String::new();
            for el in p.children().filter(XmlNode::is_element) {
                match el.tag_name().name() {
                    "r" | "fld" => {
                        if let Some(t) = child(el, "t") {
                            s.push_str(t.text().unwrap_or(""));
                        }
                    }
                    "br" => s.push('\u{b}'),
                    _ => {}
                }
            }
            s
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// docling's `_chart_to_table_data`.
fn chart_data(
    plot_area: XmlNode,
    plots: &[XmlNode],
    date1904: bool,
) -> Option<docling_core::Table> {
    // The lists are cut to the rows the table can keep before they are
    // made (docling#5482): a crafted `c:ptCount` sizes nothing.
    let num_series: usize = plots
        .iter()
        .map(|p| p.children().filter(|c| c.has_tag_name("ser")).count())
        .sum();
    let max_rows = max_chart_data_rows(1 + num_series);
    let mut categories: Vec<String> = Vec::new();
    let mut corner = String::new();
    let mut columns: Vec<(String, Vec<String>)> = Vec::new();
    for &plot in plots {
        if categories.is_empty() {
            let axis = category_axis(plot_area, plot);
            categories = category_labels(plot, axis, date1904, max_rows);
            corner = axis.map(axis_title).unwrap_or_default();
        }
        // `_plot_series`: sorted by `c:order`, whose absence python-pptx
        // rejects as invalid XML — the table is dropped.
        let mut sers: Vec<(i64, XmlNode)> = Vec::new();
        for ser in plot.children().filter(|c| c.has_tag_name("ser")) {
            let order = child(ser, "order")
                .and_then(|o| o.attribute("val"))
                .and_then(|v| v.parse::<i64>().ok())?;
            sers.push((order, ser));
        }
        sers.sort_by_key(|(order, _)| *order);
        let xy = is_xy_plot(plot.tag_name().name());
        for (_, ser) in sers {
            let name = ser
                .children()
                .filter(|c| c.has_tag_name("tx"))
                .flat_map(|tx| tx.descendants())
                .find(|n| n.has_tag_name("pt"))
                .and_then(|pt| child(pt, "v"))
                .and_then(|v| v.text())
                .unwrap_or("")
                .to_string();
            // A value that is not a number fails python-pptx's `float()`,
            // and docling then drops the whole table.
            columns.push((name, series_values(ser, xy, max_rows)?));
        }
    }
    if columns.is_empty() {
        return None;
    }
    build_chart_table(categories, columns, corner)
}

/// docling's `_declared_points`: python-pptx reads the indexes below the
/// declared `c:ptCount` (none without one); the count only filters the
/// points present, it never sizes a list (docling#5482, #1166).
fn declared_count(source: XmlNode) -> usize {
    source
        .descendants()
        .find(|n| n.has_tag_name("ptCount"))
        .and_then(|n| n.attribute("val"))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0)
}

/// A series' values as docling's `_series_values` reads them (`c:val`; an
/// xy plot's `c:yVal`): the cached points below the declared count, the
/// first point of an index giving its value, one entry per index up to the
/// last kept one ([`chart_points_to_list`]), integer-valued numbers without
/// `.0` (docling's `_cell_text`). `None` when a point is not a number.
fn series_values(ser: XmlNode, xy: bool, max_rows: usize) -> Option<Vec<String>> {
    let Some(val) = child(ser, if xy { "yVal" } else { "val" }) else {
        return Some(Vec::new());
    };
    let count = declared_count(val);
    let mut values: HashMap<usize, String> = HashMap::new();
    for pt in val.descendants().filter(|n| n.has_tag_name("pt")) {
        let Some(idx) = pt.attribute("idx").and_then(|i| i.parse::<usize>().ok()) else {
            continue;
        };
        if idx >= count || values.contains_key(&idx) {
            continue;
        }
        let text = child(pt, "v").and_then(|v| v.text()).unwrap_or("");
        let value: f64 = text.trim().parse().ok()?;
        values.insert(idx, python_number(value));
    }
    Some(chart_points_to_list(values, max_rows, true))
}

/// A float as docling's `_cell_text` writes it: an integer value without
/// `.0`, anything else as Python's `str(float)`.
fn python_number(v: f64) -> String {
    if v.is_nan() {
        return "nan".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf" } else { "-inf" }.into();
    }
    if v.fract() == 0.0 {
        return if v == 0.0 {
            "0".into()
        } else {
            format!("{v:.0}")
        };
    }
    // Python's repr switches to scientific notation below 1e-4 (and from
    // 1e16, where every float is an integer), with a signed two-digit
    // exponent.
    let sci = format!("{v:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if exp < -4 {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exp.abs())
    } else {
        format!("{v}")
    }
}

/// The `c:catAx`/`c:dateAx` a plot is drawn against, matched by `c:axId`
/// (docling's `_category_axis`); `None` for a plot without one (pie, xy).
fn category_axis<'a, 'i>(plot_area: XmlNode<'a, 'i>, plot: XmlNode) -> Option<XmlNode<'a, 'i>> {
    let ids: Vec<&str> = plot
        .children()
        .filter(|c| c.has_tag_name("axId"))
        .filter_map(|c| c.attribute("val"))
        .collect();
    plot_area.children().find(|axis| {
        (axis.has_tag_name("catAx") || axis.has_tag_name("dateAx"))
            && axis
                .children()
                .filter(|c| c.has_tag_name("axId"))
                .filter_map(|c| c.attribute("val"))
                .any(|id| ids.contains(&id))
    })
}

/// A plot's category labels (docling's `_category_labels`: the first
/// series' `c:cat` points, first level only, below the declared count, one
/// entry per index up to the last cached label), with dates as ISO 8601
/// when its axis shows dates.
fn category_labels(
    plot: XmlNode,
    axis: Option<XmlNode>,
    date1904: bool,
    max_rows: usize,
) -> Vec<String> {
    let first_cat = plot
        .children()
        .find(|c| c.has_tag_name("ser"))
        .and_then(|ser| child(ser, "cat"));
    let mut points: HashMap<usize, String> = HashMap::new();
    if let Some(cat) = first_cat {
        let count = declared_count(cat);
        let first_level = cat.descendants().find(|n| n.has_tag_name("lvl"));
        let pts: Vec<XmlNode> = match first_level {
            Some(lvl) => lvl.children().filter(|c| c.has_tag_name("pt")).collect(),
            None => cat.descendants().filter(|n| n.has_tag_name("pt")).collect(),
        };
        for pt in pts {
            let Some(idx) = pt.attribute("idx").and_then(|i| i.parse::<usize>().ok()) else {
                continue;
            };
            if idx < count {
                let text = child(pt, "v").and_then(|v| v.text()).unwrap_or("");
                points.insert(idx, text.to_string());
            }
        }
    }
    let labels = chart_points_to_list(points, max_rows, true);
    match axis {
        Some(axis) if categories_are_dates(axis, plot) => labels
            .into_iter()
            .map(|label| serial_to_iso(&label, date1904))
            .collect(),
        _ => labels,
    }
}

/// docling's `_categories_are_dates`: a date axis always; a text axis when
/// the number format it shows — its own, unless linked to the source data,
/// then the cached categories' — is a date format.
fn categories_are_dates(axis: XmlNode, plot: XmlNode) -> bool {
    if axis.has_tag_name("dateAx") {
        return true;
    }
    let own = child(axis, "numFmt")
        .filter(|f| !matches!(f.attribute("sourceLinked"), Some("1") | Some("true")));
    let format_code = match own {
        Some(f) => f.attribute("formatCode").unwrap_or("").to_string(),
        None => plot
            .children()
            .find(|c| c.has_tag_name("ser"))
            .and_then(|ser| child(ser, "cat"))
            .map(|cat| {
                cat.descendants()
                    .filter(|n| n.has_tag_name("numCache"))
                    .filter_map(|c| child(c, "formatCode"))
                    .filter_map(|f| f.text())
                    .collect::<String>()
            })
            .unwrap_or_default(),
    };
    is_date_format(&format_code)
}

/// docling's `_is_date_format`: with the literal parts removed (quoted text,
/// escaped characters, bracketed colours/locales/conditions, padding), a `d`
/// or `y` code is a date, and an `m` code is a month (not minutes) when
/// there is no `h` or `s`.
fn is_date_format(format_code: &str) -> bool {
    let mut codes = String::new();
    let mut chars = format_code.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                for q in chars.by_ref() {
                    if q == '"' {
                        break;
                    }
                }
            }
            '[' => {
                for q in chars.by_ref() {
                    if q == ']' {
                        break;
                    }
                }
            }
            '\\' | '_' | '*' => {
                chars.next();
            }
            _ => codes.extend(c.to_lowercase()),
        }
    }
    if codes.contains('d') || codes.contains('y') {
        return true;
    }
    codes.contains('m') && !codes.contains('h') && !codes.contains('s')
}

/// docling's `_serial_to_iso`: an Excel date serial as an ISO 8601 date (a
/// date and time to the second when it has a time of day), else the text
/// unchanged — not a number, negative, or out of Python's `datetime` range.
fn serial_to_iso(text: &str, date1904: bool) -> String {
    let Ok(serial) = text.trim().parse::<f64>() else {
        return text.to_string();
    };
    if !serial.is_finite() || serial < 0.0 {
        return text.to_string();
    }
    let epoch = if date1904 {
        days_from_civil(1904, 1, 1)
    } else {
        // Excel counts the nonexistent 1900-02-29 as serial 60.
        days_from_civil(1899, 12, 30) + i64::from(serial < 60.0)
    };
    // `timedelta(seconds=round(serial * 86400))`: Python rounds half to even.
    let seconds = (serial * 86_400.0).round_ties_even();
    if seconds > 1.0e15 {
        return text.to_string();
    }
    let seconds = seconds as i64;
    let days = epoch + seconds.div_euclid(86_400);
    let secs = seconds.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    if y > 9999 {
        return text.to_string();
    }
    if secs == 0 {
        format!("{y:04}-{m:02}-{d:02}")
    } else {
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
            secs / 3600,
            secs / 60 % 60,
            secs % 60
        )
    }
}

/// docling's `_axis_title_text`: the rich text's paragraphs, each stripped,
/// joined with a space; else a cell-linked title's cached text.
fn axis_title(axis: XmlNode) -> String {
    let Some(tx) = child(axis, "title").and_then(|t| child(t, "tx")) else {
        return String::new();
    };
    let rich = child(tx, "rich")
        .map(|rich| {
            rich.children()
                .filter(|p| p.has_tag_name("p"))
                .map(|p| {
                    p.descendants()
                        .filter(|n| n.has_tag_name("t"))
                        .filter_map(|n| n.text())
                        .collect::<String>()
                        .trim()
                        .to_string()
                })
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let text = if rich.is_empty() {
        child(tx, "strRef")
            .map(|r| {
                r.descendants()
                    .filter(|n| n.has_tag_name("v"))
                    .filter_map(|n| n.text())
                    .collect::<String>()
            })
            .unwrap_or_default()
    } else {
        rich
    };
    text.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chart part with one plot `tag` holding `sers` (`c:ser` bodies).
    fn chart_xml(tag: &str, sers: &[&str]) -> String {
        let sers: String = sers.iter().map(|s| format!("<c:ser>{s}</c:ser>")).collect();
        format!(
            r#"<c:chartSpace xmlns:c="c"><c:chart><c:plotArea><c:{tag}>{sers}<c:axId val="1"/></c:{tag}><c:catAx><c:axId val="1"/></c:catAx></c:plotArea></c:chart></c:chartSpace>"#
        )
    }

    const SER_2026: &str = r#"<c:idx val="0"/><c:order val="0"/><c:tx><c:strRef><c:strCache><c:pt idx="0"><c:v>2026</c:v></c:pt></c:strCache></c:strRef></c:tx><c:cat><c:strRef><c:strCache><c:ptCount val="3"/><c:pt idx="0"><c:v>Q1</c:v></c:pt><c:pt idx="1"><c:v>Q2</c:v></c:pt><c:pt idx="2"><c:v>Q3</c:v></c:pt></c:strCache></c:strRef></c:cat><c:val><c:numRef><c:numCache><c:ptCount val="3"/><c:pt idx="0"><c:v>10</c:v></c:pt><c:pt idx="1"><c:v>20</c:v></c:pt><c:pt idx="2"><c:v>30</c:v></c:pt></c:numCache></c:numRef></c:val>"#;

    /// docling#4811: a plot python-pptx has no class for (3-D bar, line,
    /// pie, stock, surface, bar-of-pie) is read like any other and labelled
    /// by its tag; a malformed series (no `c:order`) keeps the label and
    /// drops the table instead of failing.
    #[test]
    fn every_plot_type_is_read_and_labelled_by_its_tag() {
        for (tag, label) in [
            ("bar3DChart", "bar_chart"),
            ("line3DChart", "line_chart"),
            ("pie3DChart", "pie_chart"),
            ("area3DChart", "other_chart"),
            ("stockChart", "other_chart"),
            ("surfaceChart", "other_chart"),
            ("ofPieChart", "other_chart"),
        ] {
            let chart = parse(&chart_xml(tag, &[SER_2026])).unwrap();
            assert_eq!(chart.labels, [label], "{tag}");
            let table = chart
                .table
                .unwrap_or_else(|| panic!("{tag}: no data table"));
            assert_eq!(
                table.rows,
                [["", "2026"], ["Q1", "10"], ["Q2", "20"], ["Q3", "30"]],
                "{tag}"
            );
        }
        let no_order = SER_2026.replace(r#"<c:order val="0"/>"#, "");
        let chart = parse(&chart_xml("bar3DChart", &[&no_order])).unwrap();
        assert_eq!(chart.labels, ["bar_chart"]);
        assert!(chart.table.is_none());
    }

    /// docling's `test_serial_to_iso` cases.
    /// docling#5482 (#1166): the table follows the cached points, never the
    /// declared `c:ptCount` — `ptCount="100000000000"` plus a point at index
    /// 10^9 (past the table's row cap) gives the 3-row table; it aborted on
    /// a 24 GB allocation before. A sparse point keeps its row, the gap empty.
    /// docling's `test_large_declared_point_count_adds_no_rows` and
    /// `test_sparse_points_stay_aligned_with_the_categories`.
    #[test]
    fn chart_tables_follow_the_cached_points() {
        let grid = |count: &str, extra: &str| -> Vec<Vec<String>> {
            let ser = SER_2026
                .replace(
                    r#"<c:ptCount val="3"/>"#,
                    &format!(r#"<c:ptCount val="{count}"/>"#),
                )
                .replace("</c:numCache>", &format!("{extra}</c:numCache>"));
            parse(&chart_xml("barChart", &[&ser]))
                .and_then(|c| c.table)
                .expect("table")
                .rows
        };
        let row = |a: &str, b: &str| vec![a.to_string(), b.to_string()];
        let head = vec![
            row("", "2026"),
            row("Q1", "10"),
            row("Q2", "20"),
            row("Q3", "30"),
        ];
        assert_eq!(
            grid(
                "100000000000",
                r#"<c:pt idx="1000000000"><c:v>99</c:v></c:pt>"#
            ),
            head
        );
        let mut sparse = head.clone();
        sparse.extend([row("", ""), row("", ""), row("", "60")]);
        assert_eq!(grid("6", r#"<c:pt idx="5"><c:v>60</c:v></c:pt>"#), sparse);
        // No declared count: python-pptx reads no point.
        assert!(parse(&chart_xml(
            "barChart",
            &[&SER_2026.replace(r#"<c:ptCount val="3"/>"#, "")]
        ))
        .and_then(|c| c.table)
        .is_none());
    }

    #[test]
    fn serials_map_to_iso_dates_in_both_systems() {
        for (text, date1904, want) in [
            ("40909", false, "2012-01-01"),
            ("40909.0", false, "2012-01-01"),
            ("39447", true, "2012-01-01"),
            ("40909.5", false, "2012-01-01T12:00:00"),
            ("1", false, "1900-01-01"),
            ("61", false, "1900-03-01"),
            ("Jan", false, "Jan"),
            ("", false, ""),
            ("-1", false, "-1"),
            ("1e300", false, "1e300"),
        ] {
            assert_eq!(serial_to_iso(text, date1904), want, "{text}");
        }
    }

    #[test]
    fn date_formats_are_told_from_time_and_general_ones() {
        for (code, want) in [
            ("m/d/yy", true),
            ("mmm d, yy", true),
            ("yyyy", true),
            ("mmm", true),
            ("h:mm", false),
            ("mm:ss", false),
            ("General", false),
            ("\"day\" 0", false),
            ("[Red]0.00", false),
            ("0_m", false),
        ] {
            assert_eq!(is_date_format(code), want, "{code}");
        }
    }

    #[test]
    fn numbers_read_like_python() {
        assert_eq!(python_number(85.0), "85");
        assert_eq!(python_number(0.126), "0.126");
        assert_eq!(python_number(-2.5), "-2.5");
        assert_eq!(python_number(1.5e-5), "1.5e-05");
        assert_eq!(python_number(-0.0), "0");
        assert_eq!(python_number(1e16), "10000000000000000");
    }
}
