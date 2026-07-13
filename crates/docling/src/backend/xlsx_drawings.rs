//! XLSX drawings, charts, and cell comments — the non-grid halves of docling's
//! `MsExcelDocumentBackend`.
//!
//! Drawings (`xl/drawings/drawingN.xml`) anchor images and chart frames to
//! cell ranges: a `twoCellAnchor` spans `from..to` (docling's bbox is
//! `(from.col, from.row, to.col+1, to.row+1)`), a `oneCellAnchor` covers a
//! single cell. Charts (`xl/charts/chartN.xml`) carry their series as
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
    for anchor in dom
        .root_element()
        .children()
        .filter(|n| matches!(n.tag_name().name(), "twoCellAnchor" | "oneCellAnchor"))
    {
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
        let Some((fc, fr)) = cell("from") else {
            continue;
        };
        let bbox = match cell("to") {
            Some((tc, tr)) => (fc, fr, tc + 1, tr + 1),
            None => (fc, fr, fc + 1, fr + 1),
        };
        let kind = if let Some(blip) = anchor.descendants().find(|n| {
            n.has_tag_name("blip") && !n.ancestors().any(|a| a.has_tag_name("graphicFrame"))
        }) {
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
        out.push(DrawingItem { bbox, kind });
    }
    out
}

/// A chart's declarative content: docling's classification label, the title
/// (caption), and the series with their workbook references.
pub struct ChartSpec {
    pub kind: &'static str,
    pub title: Option<String>,
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

/// Parse `xl/charts/chartN.xml` into a [`ChartSpec`]. The chart *kind* comes
/// from the first plot-area child docling's map knows (unknown kinds fall back
/// to `other_chart` when any `*Chart` element exists).
pub fn parse_chart(xml: &str) -> Option<ChartSpec> {
    let dom = Document::parse(xml).ok()?;
    let plot = dom.descendants().find(|n| n.has_tag_name("plotArea"))?;
    let chart_el = plot
        .children()
        .find(|n| n.tag_name().name().ends_with("Chart"))?;
    let kind = classification(chart_el.tag_name().name()).unwrap_or("other_chart");

    // Title: all `<a:t>` runs under `c:title`, concatenated.
    let title = dom
        .descendants()
        .find(|n| n.has_tag_name("title"))
        .map(|t| {
            t.descendants()
                .filter(|n| n.has_tag_name("t"))
                .filter_map(|n| n.text())
                .collect::<String>()
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

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
    for ser in chart_el.children().filter(|n| n.has_tag_name("ser")) {
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
        series.push(SeriesSpec {
            name_ref,
            name_lit,
            cat_ref,
            val_ref,
        });
    }
    Some(ChartSpec {
        kind,
        title,
        series,
    })
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
    let a = cell_ref(corners.next()?)?;
    let b = match corners.next() {
        Some(c) => cell_ref(c)?,
        None => a,
    };
    Some((
        sheet,
        (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)),
    ))
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
) -> HashMap<String, (String, String, Option<String>)> {
    let Ok(dom) = Document::parse(xml) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
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
        out.insert(cell, (author, text, time));
    }
    out
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
