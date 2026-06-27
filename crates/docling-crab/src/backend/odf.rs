//! OpenDocument backend (`.odt`/`.ods`/`.odp`) — a port of docling's
//! `OpenDocument*` backends. ODF is a ZIP whose `content.xml` holds the body;
//! `styles.xml` plus `content.xml`'s automatic styles define text/paragraph/list
//! styles. Paragraph styles map to Title/Subtitle/Heading; `<text:h>` maps to a
//! heading by outline level; runs (`<text:span>`) carry bold/italic/strike/sub-
//! superscript resolved through the style parent chain; lists nest by depth.

use std::collections::HashMap;

use roxmltree::{Document, Node as XmlNode};

use crate::backend::markdown::escape_text;
use crate::backend::ooxml::Package;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_crab_core::{DoclingDocument, Node, Table};

pub struct OdfBackend;

#[derive(Default, Clone, Copy, PartialEq)]
struct Fmt {
    bold: bool,
    italic: bool,
    strike: bool,
    underline: bool,
    script: u8, // 0 none, 1 sub, 2 super
}

#[derive(Default, Clone)]
struct StyleInfo {
    parent: Option<String>,
    display_name: Option<String>,
    bold: Option<bool>,
    italic: Option<bool>,
    strike: Option<bool>,
    underline: Option<bool>,
    script: Option<u8>,
}

/// List style level → is-numbered (vs bullet).
type ListStyles = HashMap<String, HashMap<i64, bool>>;

struct Styles {
    map: HashMap<String, StyleInfo>,
    lists: ListStyles,
}

impl DeclarativeBackend for OdfBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let mut pkg = Package::open(&source.bytes)
            .ok_or_else(|| ConversionError::Parse("odf: not a zip".into()))?;
        let content = pkg
            .read("content.xml")
            .ok_or_else(|| ConversionError::Parse("odf: no content.xml".into()))?;
        let styles_xml = pkg.read("styles.xml").unwrap_or_default();

        let content_dom =
            Document::parse(&content).map_err(|e| ConversionError::Parse(format!("odf: {e}")))?;
        let styles_dom = Document::parse(&styles_xml).ok();
        let styles = parse_styles(&content_dom, styles_dom.as_ref());

        let mut doc = DoclingDocument::new(&source.name);
        let Some(body) = content_dom.descendants().find(|n| n.has_tag_name("body")) else {
            return Ok(doc);
        };
        for office in body.children().filter(XmlNode::is_element) {
            match office.tag_name().name() {
                "text" => walk_text(office, &styles, &mut doc),
                "spreadsheet" => walk_spreadsheet(office, &styles, &mut doc),
                "presentation" => walk_presentation(office, &styles, &mut doc),
                _ => {}
            }
        }
        Ok(doc)
    }
}

// ---------------------------------------------------------------- styles

fn parse_styles(content: &Document, styles: Option<&Document>) -> Styles {
    let mut map = HashMap::new();
    let mut lists = HashMap::new();
    for dom in [Some(content), styles].into_iter().flatten() {
        for s in dom.descendants() {
            match s.tag_name().name() {
                "style" => {
                    if let Some(name) = attr(s, "name") {
                        map.insert(name.to_string(), style_info(s));
                    }
                }
                "list-style" => {
                    if let Some(name) = attr(s, "name") {
                        let mut levels = HashMap::new();
                        for lv in s.children().filter(XmlNode::is_element) {
                            let level: i64 =
                                attr(lv, "level").and_then(|v| v.parse().ok()).unwrap_or(1);
                            let numbered = lv.tag_name().name() == "list-level-style-number";
                            levels.insert(level, numbered);
                        }
                        lists.insert(name.to_string(), levels);
                    }
                }
                _ => {}
            }
        }
    }
    Styles { map, lists }
}

fn style_info(s: XmlNode) -> StyleInfo {
    let mut info = StyleInfo {
        parent: attr(s, "parent-style-name").map(str::to_string),
        display_name: attr(s, "display-name").map(str::to_string),
        ..Default::default()
    };
    if let Some(tp) = s.children().find(|c| c.has_tag_name("text-properties")) {
        info.bold = attr(tp, "font-weight").map(is_bold);
        info.italic = attr(tp, "font-style").map(|v| v == "italic" || v == "oblique");
        info.strike = attr(tp, "text-line-through-style").map(|v| v != "none");
        info.underline = attr(tp, "text-underline-style").map(|v| v != "none");
        info.script = attr(tp, "text-position").map(|v| {
            if v.starts_with("super") {
                2
            } else if v.starts_with("sub") {
                1
            } else {
                0
            }
        });
    }
    info
}

fn is_bold(v: &str) -> bool {
    v == "bold" || v.parse::<i32>().map(|n| n >= 600).unwrap_or(false)
}

/// Resolve a text/paragraph style's formatting through its parent chain.
fn resolve_fmt(styles: &Styles, name: Option<&str>, base: Fmt) -> Fmt {
    let mut fmt = base;
    let mut chain = Vec::new();
    let mut cur = name.map(str::to_string);
    let mut seen = std::collections::HashSet::new();
    while let Some(n) = cur {
        if !seen.insert(n.clone()) {
            break;
        }
        if let Some(info) = styles.map.get(&n) {
            chain.push(info.clone());
            cur = info.parent.clone();
        } else {
            break;
        }
    }
    // Apply parent-first so the most-derived style wins.
    for info in chain.into_iter().rev() {
        if let Some(b) = info.bold {
            fmt.bold = b;
        }
        if let Some(i) = info.italic {
            fmt.italic = i;
        }
        if let Some(s) = info.strike {
            fmt.strike = s;
        }
        if let Some(u) = info.underline {
            fmt.underline = u;
        }
        if let Some(sc) = info.script {
            fmt.script = sc;
        }
    }
    fmt
}

/// The set of style names a paragraph resolves to (own, parent, display).
fn paragraph_style_names(styles: &Styles, name: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(n) = name {
        out.push(n.to_string());
        if let Some(info) = styles.map.get(n) {
            if let Some(p) = &info.parent {
                out.push(p.clone());
            }
            if let Some(d) = &info.display_name {
                out.push(d.clone());
            }
        }
    }
    out
}

// ---------------------------------------------------------------- text runs

/// One formatted run of text.
struct Run {
    text: String,
    fmt: Fmt,
}

/// Collect runs from a paragraph/heading element (recursing spans).
fn collect_runs(el: XmlNode, styles: &Styles, base: Fmt, out: &mut Vec<Run>) {
    for child in el.children() {
        if child.is_text() {
            if let Some(t) = child.text() {
                out.push(Run {
                    text: t.to_string(),
                    fmt: base,
                });
            }
        } else if child.is_element() {
            match child.tag_name().name() {
                "span" => {
                    let fmt = resolve_fmt(styles, attr(child, "style-name"), base);
                    collect_runs(child, styles, fmt, out);
                }
                "line-break" => out.push(Run {
                    text: "\n".into(),
                    fmt: base,
                }),
                "tab" => out.push(Run {
                    text: "\t".into(),
                    fmt: base,
                }),
                "s" => {
                    // <text:s text:c="n"> = n spaces (default 1)
                    let n: usize = attr(child, "c").and_then(|v| v.parse().ok()).unwrap_or(1);
                    out.push(Run {
                        text: " ".repeat(n),
                        fmt: base,
                    });
                }
                "a" | "ruby" | "ruby-base" => collect_runs(child, styles, base, out),
                _ => {}
            }
        }
    }
}

/// Merge adjacent same-format runs, serialize each (markers), join with spaces —
/// docling-core's inline-group serialization (un-stripped, so spaces double up).
fn runs_to_text(mut runs: Vec<Run>) -> String {
    // merge adjacent same-fmt
    let mut merged: Vec<Run> = Vec::new();
    for r in runs.drain(..) {
        if let Some(last) = merged.last_mut() {
            if last.fmt == r.fmt {
                last.text.push_str(&r.text);
                continue;
            }
        }
        merged.push(r);
    }
    merged
        .iter()
        .map(|r| serialize_run(&r.text, r.fmt))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end()
        .to_string()
}

fn serialize_run(text: &str, fmt: Fmt) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut s = escape_text(text);
    if fmt.bold {
        s = format!("**{s}**");
    }
    if fmt.italic {
        s = format!("*{s}*");
    }
    if fmt.strike {
        s = format!("~~{s}~~");
    }
    s
}

// ---------------------------------------------------------------- text doc

fn walk_text(text: XmlNode, styles: &Styles, doc: &mut DoclingDocument) {
    // List numbering continues across consecutive `<text:list>` siblings (ODF
    // splits a single logical list into several elements); a non-list block
    // resets it.
    let mut counters: Vec<u64> = Vec::new();
    for el in text.children().filter(XmlNode::is_element) {
        if el.tag_name().name() != "list" {
            counters.clear();
        }
        handle_block(el, styles, doc, 0, &mut counters);
    }
}

fn handle_block(
    el: XmlNode,
    styles: &Styles,
    doc: &mut DoclingDocument,
    list_level: u8,
    counters: &mut Vec<u64>,
) {
    match el.tag_name().name() {
        "h" => {
            let level = attr(el, "outline-level")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(1)
                .max(1);
            let mut runs = Vec::new();
            collect_runs(el, styles, Fmt::default(), &mut runs);
            let text = runs_to_text(runs);
            if !text.is_empty() {
                doc.push(Node::Heading {
                    level: (level + 1) as u8,
                    text,
                });
            }
        }
        "p" => {
            let names = paragraph_style_names(styles, attr(el, "style-name"));
            let mut runs = Vec::new();
            collect_runs(el, styles, Fmt::default(), &mut runs);
            let text = runs_to_text(runs);
            if text.is_empty() {
                return;
            }
            if names.iter().any(|n| n == "Title") {
                doc.push(Node::Heading { level: 1, text });
            } else if names.iter().any(|n| n == "Subtitle") {
                doc.push(Node::Heading { level: 2, text });
            } else {
                doc.push(Node::Paragraph { text });
            }
        }
        "list" => {
            let style = attr(el, "style-name");
            walk_list(el, styles, doc, list_level, style, counters);
        }
        "table" => {
            if let Some(table) = parse_table(el, styles) {
                doc.push(Node::Table(table));
            }
        }
        _ => {}
    }
}

fn walk_list(
    list: XmlNode,
    styles: &Styles,
    doc: &mut DoclingDocument,
    level: u8,
    list_style: Option<&str>,
    counters: &mut Vec<u64>,
) {
    let numbered = list_style
        .and_then(|s| styles.lists.get(s))
        .and_then(|levels| levels.get(&((level + 1) as i64)))
        .copied()
        .unwrap_or(false);
    while counters.len() <= level as usize {
        counters.push(0);
    }
    for item in list.children().filter(|c| c.has_tag_name("list-item")) {
        // The first block of the item is its text; nested lists indent deeper.
        for child in item.children().filter(XmlNode::is_element) {
            match child.tag_name().name() {
                "p" | "h" => {
                    let mut runs = Vec::new();
                    collect_runs(child, styles, Fmt::default(), &mut runs);
                    let text = runs_to_text(runs);
                    if text.is_empty() {
                        continue;
                    }
                    let number = if numbered {
                        counters[level as usize] += 1;
                        counters[level as usize]
                    } else {
                        0
                    };
                    doc.push(Node::ListItem {
                        ordered: numbered,
                        number,
                        first_in_list: false,
                        text,
                        level,
                    });
                }
                "list" => {
                    let s = attr(child, "style-name").or(list_style);
                    walk_list(child, styles, doc, level + 1, s, counters);
                }
                _ => {}
            }
        }
    }
    counters.truncate((level + 1) as usize);
}

// ---------------------------------------------------------------- tables

fn parse_table(table: XmlNode, styles: &Styles) -> Option<Table> {
    let mut rows = Vec::new();
    for tr in table.descendants().filter(|n| n.has_tag_name("table-row")) {
        let mut cells = Vec::new();
        for tc in tr.children().filter(|c| c.has_tag_name("table-cell")) {
            let repeat: usize = attr(tc, "number-columns-repeated")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            let span: usize = attr(tc, "number-columns-spanned")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            let text = cell_text(tc, styles);
            for _ in 0..(repeat.max(1) * span.max(1)) {
                cells.push(text.clone());
            }
        }
        // Trailing empty repeated cells inflate rows; trim trailing blanks.
        while cells.last().map(|c| c.is_empty()).unwrap_or(false) {
            cells.pop();
        }
        if !cells.is_empty() {
            rows.push(cells);
        }
    }
    if rows.is_empty() {
        return None;
    }
    Some(Table { rows })
}

fn cell_text(tc: XmlNode, styles: &Styles) -> String {
    let mut parts = Vec::new();
    for p in tc.children().filter(|c| c.has_tag_name("p") || c.has_tag_name("h")) {
        let mut runs = Vec::new();
        collect_runs(p, styles, Fmt::default(), &mut runs);
        let t = runs_to_text(runs);
        if !t.is_empty() {
            parts.push(t);
        }
    }
    parts.join(" ")
}

// ---------------------------------------------------------------- spreadsheet

fn walk_spreadsheet(sheet: XmlNode, styles: &Styles, doc: &mut DoclingDocument) {
    for table in sheet.children().filter(|c| c.has_tag_name("table")) {
        if let Some(t) = parse_table(table, styles) {
            doc.push(Node::Table(t));
        }
    }
}

// ---------------------------------------------------------------- presentation

fn walk_presentation(pres: XmlNode, styles: &Styles, doc: &mut DoclingDocument) {
    for page in pres.children().filter(|c| c.has_tag_name("page")) {
        for frame in page.descendants().filter(|n| n.has_tag_name("frame")) {
            for tb in frame.children().filter(|c| c.has_tag_name("text-box")) {
                for el in tb.children().filter(XmlNode::is_element) {
                    handle_block(el, styles, doc, 0, &mut Vec::new());
                }
            }
            for table in frame.children().filter(|c| c.has_tag_name("table")) {
                if let Some(t) = parse_table(table, styles) {
                    doc.push(Node::Table(t));
                }
            }
        }
    }
}

/// Attribute by local name (ODF attributes are namespaced, e.g. `text:style-name`).
fn attr<'a>(node: XmlNode<'a, '_>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|a| a.name() == name)
        .map(|a| a.value())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_resolves_through_parent_chain() {
        // P2 → P1 → Strong (bold). T1 adds italic directly.
        let content = r#"<root xmlns:style="s" xmlns:fo="f">
            <style:style style:name="Strong" style:family="text">
              <style:text-properties fo:font-weight="bold"/></style:style>
            <style:style style:name="P1" style:family="text" style:parent-style-name="Strong"/>
            <style:style style:name="P2" style:family="text" style:parent-style-name="P1"/>
            <style:style style:name="T1" style:family="text">
              <style:text-properties fo:font-style="italic"/></style:style>
          </root>"#;
        let dom = Document::parse(content).unwrap();
        let styles = parse_styles(&dom, None);
        let f = resolve_fmt(&styles, Some("P2"), Fmt::default());
        assert!(f.bold && !f.italic, "bold inherited through P2→P1→Strong");
        let t = resolve_fmt(&styles, Some("T1"), Fmt::default());
        assert!(t.italic && !t.bold);
    }
}
