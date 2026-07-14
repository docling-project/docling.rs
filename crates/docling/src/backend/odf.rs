//! OpenDocument backend (`.odt`/`.ods`/`.odp`) — a port of docling's
//! `OpenDocument*` backends. ODF is a ZIP whose `content.xml` holds the body;
//! `styles.xml` plus `content.xml`'s automatic styles define text/paragraph/list
//! styles. Paragraph styles map to Title/Subtitle/Heading; `<text:h>` maps to a
//! heading by outline level; runs (`<text:span>`) carry bold/italic/strike/sub-
//! superscript resolved through the style parent chain; lists nest by depth.

use std::collections::{HashMap, HashSet, VecDeque};

use roxmltree::{Document, Node as XmlNode};

use crate::backend::markdown::escape_text;
use crate::backend::ooxml::Package;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::{
    inline_paragraph_node, DoclingDocument, InlineRun, Node, Script, Table, TableStructure,
};

pub struct OdfBackend;

#[derive(Default, Clone, Copy, PartialEq)]
struct Fmt {
    bold: bool,
    italic: bool,
    strike: bool,
    underline: bool,
    script: u8, // 0 none, 1 sub, 2 super
}

impl Fmt {
    /// The structured [`InlineRun`] for a text segment under this formatting —
    /// the ODF analogue of the DOCX backend's `to_inline_run` (underline and
    /// sub/superscript have no Markdown marker, so they only survive here).
    fn to_inline_run(self, text: &str) -> InlineRun {
        InlineRun {
            text: text.to_string(),
            bold: self.bold,
            italic: self.italic,
            underline: self.underline,
            strike: self.strike,
            script: match self.script {
                1 => Script::Sub,
                2 => Script::Super,
                _ => Script::Baseline,
            },
            code: false,
            formula: false,
        }
    }
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

/// One list level's rendering: bullet vs numbered, its `start-value`, and the
/// prefix/suffix wrapping an enumerated marker (`num-prefix`/`num-suffix`).
#[derive(Default, Clone)]
struct OdfLevel {
    numbered: bool,
    start: i64,
    prefix: String,
    suffix: String,
}

/// List style name → level (1-based) → level rendering.
type ListStyles = HashMap<String, HashMap<i64, OdfLevel>>;

/// An embedded chart object: its docling classification (`bar_chart`, …) and
/// data grid, keyed in [`Styles::charts`] by the object name a `<draw:object>`
/// references (e.g. `Object 1`).
#[derive(Clone)]
struct ChartInfo {
    kind: String,
    table: Table,
}

struct Styles {
    map: HashMap<String, StyleInfo>,
    lists: ListStyles,
    /// Embedded chart objects by name (`Object 1` → chart). Empty for documents
    /// with no charts.
    charts: HashMap<String, ChartInfo>,
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
        let mut styles = parse_styles(&content_dom, styles_dom.as_ref());
        styles.charts = load_charts(&mut pkg, &content_dom, &styles);

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
                            let start = attr(lv, "start-value")
                                .and_then(|v| v.parse().ok())
                                .map(|n: i64| n.max(1))
                                .unwrap_or(1);
                            let prefix = attr(lv, "num-prefix").unwrap_or("").to_string();
                            let suffix = attr(lv, "num-suffix").unwrap_or("").to_string();
                            levels.insert(
                                level,
                                OdfLevel {
                                    numbered,
                                    start,
                                    prefix,
                                    suffix,
                                },
                            );
                        }
                        lists.insert(name.to_string(), levels);
                    }
                }
                _ => {}
            }
        }
    }
    Styles {
        map,
        lists,
        charts: HashMap::new(),
    }
}

/// Load every embedded chart object a `<draw:object>` references. Each object is
/// a sub-package part `{name}/content.xml` holding a `<chart:chart>`; we keep its
/// classification and data `<table:table>` (parsed like any ODF table). Objects
/// that are not charts (or fail to parse) are skipped.
fn load_charts(
    pkg: &mut Package,
    content: &Document,
    styles: &Styles,
) -> HashMap<String, ChartInfo> {
    let mut charts = HashMap::new();
    for obj in content.descendants().filter(|n| n.has_tag_name("object")) {
        let Some(href) = attr(obj, "href") else {
            continue;
        };
        let name = href.trim_start_matches("./");
        if charts.contains_key(name) {
            continue;
        }
        let Some(xml) = pkg.read(&format!("{name}/content.xml")) else {
            continue;
        };
        let Ok(dom) = Document::parse(&xml) else {
            continue;
        };
        if !dom.descendants().any(|n| n.has_tag_name("chart")) {
            continue;
        }
        // Classification per docling: the first mapped `chart:class` on a
        // `<chart:chart>` (never the `<office:chart>` wrapper, which carries no
        // class), else on a `<chart:series>`, else "other_chart".
        let kind = dom
            .descendants()
            .filter(|n| n.has_tag_name("chart"))
            .find_map(|n| attr(n, "class").and_then(chart_kind))
            .or_else(|| {
                dom.descendants()
                    .filter(|n| n.has_tag_name("series"))
                    .find_map(|n| attr(n, "class").and_then(chart_kind))
            })
            .unwrap_or("other_chart")
            .to_string();
        let Some(table_node) = dom.descendants().find(|n| n.has_tag_name("table")) else {
            continue;
        };
        if let Some(table) = parse_table(table_node, styles) {
            charts.insert(name.to_string(), ChartInfo { kind, table });
        }
    }
    charts
}

/// Map an ODF `chart:class` (`chart:bar`, `chart:line`, …) to docling's
/// `PictureClassificationLabel` chart kind — exactly docling's
/// `_ODF_CHART_CLASS_TO_PICTURE_CLASSIFICATION` (unmapped classes fall back
/// to `other_chart` at the call site).
fn chart_kind(class: &str) -> Option<&'static str> {
    match class {
        "chart:bar" => Some("bar_chart"),
        "chart:line" => Some("line_chart"),
        "chart:circle" | "chart:pie" => Some("pie_chart"),
        "chart:scatter" => Some("scatter_plot"),
        _ => None,
    }
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
#[derive(Clone)]
struct Run {
    text: String,
    fmt: Fmt,
}

/// Collect runs from a paragraph/heading element (recursing spans).
fn collect_runs(el: XmlNode, styles: &Styles, base: Fmt, out: &mut Vec<Run>) {
    // docling's `_odf_text_runs` reads only `element.text` — the lxml *head*
    // text before the first element child — plus the children's own runs.
    // Text that trails a child element (its lxml `tail`) survives only when
    // the child itself yields nothing (an empty inline marker like
    // `<text:soft-page-break/>` falls back to `text_recursive`, which is its
    // tail) or when the child is a `<text:line-break/>` (whose run is built
    // from `text_recursive` too). A tail after a content-bearing child is
    // dropped: "with <span>bold</span>, and <span>italic</span> formatting"
    // loses the ", and" and " formatting" fragments.
    let mut seen_element = false;
    for child in el.children() {
        if child.is_text() {
            if seen_element {
                continue;
            }
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
                    let before = out.len();
                    collect_runs(child, styles, fmt, out);
                    seen_element |= out.len() > before || child.children().any(|c| c.is_element());
                }
                // The line-break run is `text_recursive`-based, so its tail
                // rides along — the following text node stays collected.
                "line-break" => out.push(Run {
                    text: "\n".into(),
                    fmt: base,
                }),
                "tab" => {
                    out.push(Run {
                        text: "\t".into(),
                        fmt: base,
                    });
                    seen_element = true;
                }
                "s" => {
                    // <text:s text:c="n"> = n spaces (default 1)
                    let n: usize = attr(child, "c").and_then(|v| v.parse().ok()).unwrap_or(1);
                    out.push(Run {
                        text: " ".repeat(n),
                        fmt: base,
                    });
                    seen_element = true;
                }
                // docling's `_odf_text_runs` recurses into every child, so an
                // image's `<svg:desc>`/`<svg:title>` text is picked up too.
                _ => {
                    let before = out.len();
                    collect_runs(child, styles, base, out);
                    seen_element |= out.len() > before || child.children().any(|c| c.is_element());
                }
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
        // docling's `_normalize_odf_text_runs` strips the paragraph at both ends
        // (e.g. a leading `<text:line-break/>`), keeping only internal breaks.
        .trim()
        .to_string()
}

/// The structured [`InlineRun`]s for a paragraph — one per format group, the
/// DocLang-only counterpart of [`runs_to_text`]. Adjacent same-format runs are
/// merged (raw text kept, so inter-run spacing survives as docling emits it),
/// then the whole paragraph is stripped at its two ends (docling's
/// `_normalize_odf_text_runs`). Empty groups drop out.
fn runs_to_inline(runs: Vec<Run>) -> Vec<InlineRun> {
    let mut merged: Vec<Run> = Vec::new();
    for r in runs {
        if let Some(last) = merged.last_mut() {
            if last.fmt == r.fmt {
                last.text.push_str(&r.text);
                continue;
            }
        }
        merged.push(r);
    }
    if let Some(first) = merged.first_mut() {
        first.text = first.text.trim_start().to_string();
    }
    if let Some(last) = merged.last_mut() {
        last.text = last.text.trim_end().to_string();
    }
    merged
        .into_iter()
        .filter(|r| !r.text.is_empty())
        .map(|r| r.fmt.to_inline_run(&r.text))
        .collect()
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
    walk_blocks(text.children().filter(XmlNode::is_element), styles, doc);
}

/// Walk a run of sibling blocks, threading list-continuation state. Numbering
/// continues across consecutive `<text:list>` siblings when the next list opens
/// with an empty nested item (docling's `_OdfListState`); any non-list block
/// resets the continuation.
fn walk_blocks<'a, 'i: 'a>(
    els: impl Iterator<Item = XmlNode<'a, 'i>>,
    styles: &Styles,
    doc: &mut DoclingDocument,
) {
    let mut prev_state: Option<ListCont> = None;
    for el in els {
        if el.tag_name().name() == "list" {
            prev_state = add_odf_list(el, styles, doc, 0, 1, false, prev_state.take());
        } else {
            prev_state = None;
            handle_block(el, styles, doc, 0, &mut Vec::new());
        }
    }
}

fn handle_block(
    el: XmlNode,
    styles: &Styles,
    doc: &mut DoclingDocument,
    list_level: u8,
    counters: &mut Vec<u64>,
) {
    let _ = counters;
    match el.tag_name().name() {
        "h" => {
            emit_paragraph_images(el, styles, doc);
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
            // docling's `_add_odf_paragraph` emits a paragraph's pictures before
            // its text.
            emit_paragraph_images(el, styles, doc);
            let names = paragraph_style_names(styles, attr(el, "style-name"));
            let mut runs = Vec::new();
            collect_runs(el, styles, Fmt::default(), &mut runs);
            let text = runs_to_text(runs.clone());
            if text.is_empty() {
                return;
            }
            if names.iter().any(|n| n == "Title") {
                doc.push(Node::Heading { level: 1, text });
            } else if names.iter().any(|n| n == "Subtitle") {
                doc.push(Node::Heading { level: 2, text });
            } else {
                // Styled `<text:span>` runs → a rich `InlineGroup` (Markdown/JSON
                // still render `text`, so their output is unchanged); a plain
                // paragraph collapses back to `Node::Paragraph`.
                doc.push(inline_paragraph_node(text, runs_to_inline(runs), false));
            }
        }
        "list" => {
            add_odf_list(el, styles, doc, list_level, 1, false, None);
        }
        "table" => {
            if let Some(table) = parse_table(el, styles) {
                doc.push(Node::Table(table));
            }
        }
        _ => {}
    }
}

/// Emit the graphics anchored in a paragraph: each `<draw:frame>` yields one
/// node.
fn emit_paragraph_images(el: XmlNode, styles: &Styles, doc: &mut DoclingDocument) {
    for frame in el.descendants().filter(|n| n.has_tag_name("frame")) {
        emit_frame_graphic(frame, styles, doc);
    }
}

/// Emit one node for a `<draw:frame>`: a chart when it wraps a known embedded
/// chart object, else a single picture placeholder from its first real image
/// (an `ObjectReplacements/` preview and alternate encodings collapse into that
/// one picture — docling emits one `PictureItem` per frame, not per bitmap).
/// Returns whether a node was emitted.
fn emit_frame_graphic(frame: XmlNode, styles: &Styles, doc: &mut DoclingDocument) -> bool {
    if let Some(obj) = frame.children().find(|c| c.has_tag_name("object")) {
        let name = attr(obj, "href").unwrap_or("").trim_start_matches("./");
        if let Some(info) = styles.charts.get(name) {
            doc.push(Node::Chart {
                kind: info.kind.clone(),
                table: info.table.clone(),
                caption: None,
                location: None,
            });
            return true;
        }
    }
    let has_real_image = frame.children().any(|c| {
        c.has_tag_name("image")
            && !attr(c, "href")
                .unwrap_or("")
                .trim_start_matches("./")
                .starts_with("ObjectReplacements/")
    });
    if has_real_image {
        doc.push(Node::Picture {
            caption: None,
            image: None,
        });
        return true;
    }
    false
}

/// Continuation state carried across sibling `<text:list>` elements — docling's
/// `_OdfListState`. Carries the marker affixes so a continued list keeps the
/// original level's `num-prefix`/`num-suffix` even when its own `<text:list>`
/// element resolves to a bare style.
#[derive(Clone)]
struct ListCont {
    enumerated: bool,
    counter: i64,
    has_last: bool,
    prefix: String,
    suffix: String,
}

/// A list's item elements (`<text:list-item>` / `<text:list-header>`).
fn list_items<'a, 'i>(list: XmlNode<'a, 'i>) -> impl Iterator<Item = XmlNode<'a, 'i>> {
    list.children()
        .filter(|c| c.has_tag_name("list-item") || c.has_tag_name("list-header"))
}

/// An item's rendered text (its direct paragraphs' runs, cleaned to single lines)
/// and its directly-nested `<text:list>` elements. Mirrors docling's
/// `_odf_list_item_content` with `flatten_nested_text=False`.
fn odf_item_content<'a, 'i>(
    item: XmlNode<'a, 'i>,
    styles: &Styles,
) -> (String, Vec<XmlNode<'a, 'i>>) {
    let mut parts: Vec<String> = Vec::new();
    let mut nested: Vec<XmlNode> = Vec::new();
    for child in item.children().filter(XmlNode::is_element) {
        match child.tag_name().name() {
            "list" => nested.push(child),
            "p" | "h" => {
                let mut runs = Vec::new();
                collect_runs(child, styles, Fmt::default(), &mut runs);
                let text = clean_lines(&runs_to_text(runs));
                if !text.is_empty() {
                    parts.push(text);
                }
            }
            _ => {}
        }
    }
    (parts.join(" "), nested)
}

/// Split on newlines, strip each line, drop the blanks, re-join with spaces —
/// docling's `_clean_odf_text_lines` joined.
fn clean_lines(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a list renders anything (any item with text, or a renderable nested list).
fn list_has_renderable(list: XmlNode, styles: &Styles) -> bool {
    list_items(list).any(|item| {
        let (text, nested) = odf_item_content(item, styles);
        !text.is_empty() || nested.iter().any(|n| list_has_renderable(*n, styles))
    })
}

/// Whether any item carries direct text (vs. only nested lists).
fn list_has_direct_text(list: XmlNode, styles: &Styles) -> bool {
    list_items(list).any(|item| !odf_item_content(item, styles).0.is_empty())
}

/// Whether the first item is empty but wraps a renderable nested list — the
/// signal that this list continues the previous one's numbering.
fn list_starts_with_empty_nested(list: XmlNode, styles: &Styles) -> bool {
    if let Some(item) = list_items(list).next() {
        let (text, nested) = odf_item_content(item, styles);
        return text.is_empty() && nested.iter().any(|n| list_has_renderable(*n, styles));
    }
    false
}

/// A list level's rendering (bullet vs numbered) from the list's own style, else
/// the inherited `fallback` — docling's `_odf_list_level_is_enumerated`.
fn level_is_enumerated(styles: &Styles, list: XmlNode, level: i64, fallback: bool) -> bool {
    attr(list, "style-name")
        .and_then(|name| styles.lists.get(name))
        .and_then(|levels| levels.get(&level))
        .map(|lv| lv.numbered)
        .unwrap_or(fallback)
}

/// A list level's `start-value` (default 1).
fn level_start(styles: &Styles, list: XmlNode, level: i64) -> i64 {
    attr(list, "style-name")
        .and_then(|name| styles.lists.get(name))
        .and_then(|levels| levels.get(&level))
        .map(|lv| lv.start)
        .unwrap_or(1)
}

/// A level's `num-prefix`/`num-suffix` — the affixes wrapping an enumerated
/// marker (e.g. `"" / "."` → `1.`).
fn level_affixes(styles: &Styles, list: XmlNode, level: i64) -> (String, String) {
    attr(list, "style-name")
        .and_then(|name| styles.lists.get(name))
        .and_then(|levels| levels.get(&level))
        .map(|lv| (lv.prefix.clone(), lv.suffix.clone()))
        .unwrap_or_default()
}

/// Emit an ODF list as flat [`Node::ListItem`]s — a port of docling's
/// `_add_odf_list`. `depth` is the Markdown nesting level for items of this list;
/// `style_level` (1-based) drives style lookups; empty items collapse (their
/// nested list attaches to the previous item) and a list that opens with an empty
/// nested item continues the previous list's numbering.
fn add_odf_list(
    list: XmlNode,
    styles: &Styles,
    doc: &mut DoclingDocument,
    depth: u8,
    style_level: i64,
    enumerated_fallback: bool,
    continued: Option<ListCont>,
) -> Option<ListCont> {
    if !list_has_renderable(list, styles) {
        return None;
    }
    let style_enum = level_is_enumerated(styles, list, style_level, enumerated_fallback);
    let should_continue = continued.as_ref().map(|c| c.has_last).unwrap_or(false)
        && list_starts_with_empty_nested(list, styles);

    // A list with no direct text of its own (and not continuing) is transparent:
    // its items' nested lists take its place at the same depth.
    if !should_continue && !list_has_direct_text(list, styles) {
        for item in list_items(list) {
            let (_text, nested) = odf_item_content(item, styles);
            for n in nested {
                add_odf_list(n, styles, doc, depth, style_level + 1, style_enum, None);
            }
        }
        return None;
    }

    let (mut counter, current_enum) = match (should_continue, &continued) {
        (true, Some(c)) => (c.counter, c.enumerated),
        _ => (level_start(styles, list, style_level) - 1, style_enum),
    };
    // Enumerated-marker affixes: reuse the continued list's so numbering that
    // spans sibling `<text:list>`s keeps one suffix; else this level's own.
    let (prefix, suffix) = match (should_continue, &continued) {
        (true, Some(c)) => (c.prefix.clone(), c.suffix.clone()),
        _ => level_affixes(styles, list, style_level),
    };
    let mut has_last = should_continue;
    // A non-continued `<text:list>` opens a fresh docling ListGroup: its first
    // rendered item carries the fresh-list flag (a following sibling group at
    // the same depth gets its own `<list>` / top-level Markdown blank line).
    let mut first = !should_continue;

    for item in list_items(list) {
        let (text, nested) = odf_item_content(item, styles);
        let nested: Vec<XmlNode> = nested
            .into_iter()
            .filter(|n| list_has_renderable(*n, styles))
            .collect();
        if text.is_empty() && nested.is_empty() {
            continue;
        }
        if text.is_empty() {
            // Empty item: its nested list collapses under the previous item.
            for n in &nested {
                add_odf_list(
                    *n,
                    styles,
                    doc,
                    depth + 1,
                    style_level + 1,
                    style_enum,
                    None,
                );
            }
            continue;
        }
        counter += 1;
        let (ordered, number, marker) = if current_enum {
            let n = counter.max(0) as u64;
            // docling renders an enumerated marker inside the `<ldiv>`:
            // `<marker>{prefix}{n}{suffix}</marker>` (e.g. `1.`).
            (true, n, Some(format!("{prefix}{n}{suffix}")))
        } else {
            (false, 0, None)
        };
        doc.push(Node::ListItem {
            ordered,
            number,
            first_in_list: std::mem::take(&mut first),
            text,
            level: depth,
            marker,
            location: None,
            dclx: None,
            href: None,
            layer: None,
        });
        has_last = true;
        for n in &nested {
            add_odf_list(
                *n,
                styles,
                doc,
                depth + 1,
                style_level + 1,
                style_enum,
                None,
            );
        }
    }

    Some(ListCont {
        enumerated: current_enum,
        counter,
        has_last,
        prefix,
        suffix,
    })
}

// ---------------------------------------------------------------- tables

fn parse_table(table: XmlNode, styles: &Styles) -> Option<Table> {
    // Only rows of *this* table — `descendants()` would also pull in the rows of
    // any nested table (which a rich cell renders on its own).
    let tr_rows: Vec<XmlNode> = table
        .descendants()
        .filter(|n| {
            n.has_tag_name("table-row")
                && n.ancestors().find(|a| a.has_tag_name("table")) == Some(table)
        })
        .collect();
    if tr_rows.is_empty() {
        return None;
    }

    // Each `<table:table-cell>`/`<table:covered-table-cell>` occupies one column
    // (times its `number-columns-repeated`). ODF emits an explicit
    // `covered-table-cell` for every spanned-over position, so col/row spans need
    // no extra bookkeeping: the anchor carries the text and the covered cells are
    // blank — exactly docling's grid.
    let cells_of = |tr: &XmlNode| -> Vec<(bool, usize)> {
        tr.children()
            .filter(|c| c.has_tag_name("table-cell") || c.has_tag_name("covered-table-cell"))
            .map(|c| {
                (
                    c.has_tag_name("covered-table-cell"),
                    repeat(c, "number-columns-repeated"),
                )
            })
            .collect()
    };
    let num_cols = tr_rows
        .iter()
        .map(|tr| cells_of(tr).iter().map(|(_, r)| r).sum::<usize>())
        .max()
        .unwrap_or(0);
    if num_cols == 0 {
        return None;
    }

    // The text grid plus the OTSL span overlay. ODF marks a span on its anchor
    // (`number-columns-spanned`/`number-rows-spanned`) and emits an explicit
    // `covered-table-cell` at every covered position; we mark the anchor's
    // rectangle so a covered cell becomes `<lcel/>` (horizontal), `<ucel/>`
    // (vertical) or `<xcel/>` (both). Text stays on the anchor only, so
    // Markdown/JSON are unchanged.
    let nrows = tr_rows.len();
    let mut grid = vec![vec![String::new(); num_cols]; nrows];
    let mut col_cont = vec![vec![false; num_cols]; nrows];
    let mut row_cont = vec![vec![false; num_cols]; nrows];
    // Parallel per-cell block content for rich cells (lists / multiple
    // paragraphs / nested tables). Empty for plain cells.
    let mut blocks: Vec<Vec<Vec<Node>>> = vec![vec![Vec::new(); num_cols]; nrows];
    let mut any_rich = false;
    for (ri, tr) in tr_rows.iter().enumerate() {
        let mut ci = 0usize;
        for tc in tr
            .children()
            .filter(|c| c.has_tag_name("table-cell") || c.has_tag_name("covered-table-cell"))
        {
            let reps = repeat(tc, "number-columns-repeated");
            if tc.has_tag_name("covered-table-cell") {
                // A covered position: its continuation kind is set by the anchor
                // whose rectangle reaches it; here we only advance the column.
                ci = (ci + reps).min(num_cols);
                continue;
            }
            let text = cell_text(tc, styles);
            let cell_nodes = cell_blocks_of(tc, styles);
            any_rich |= !cell_nodes.is_empty();
            let cspan = repeat(tc, "number-columns-spanned");
            let rspan = repeat(tc, "number-rows-spanned");
            for _ in 0..reps {
                if ci >= num_cols {
                    break;
                }
                grid[ri][ci] = text.clone();
                blocks[ri][ci] = cell_nodes.clone();
                let r_end = (ri + rspan).min(nrows);
                let c_end = (ci + cspan).min(num_cols);
                for rr in ri..r_end {
                    for cc in ci..c_end {
                        if rr == ri && cc == ci {
                            continue;
                        }
                        // docling's `TableData.grid` repeats a *plain* anchor's
                        // text into every covered position (visible in
                        // Markdown/JSON). A rich cell dedups instead: its
                        // covered occurrences serialize empty (the visited-set
                        // suppresses the repeated `RichTableCell` content).
                        // DocLang ignores either way — continuation cells are
                        // token-only.
                        if cell_nodes.is_empty() {
                            grid[rr][cc] = text.clone();
                        }
                        col_cont[rr][cc] |= cc > ci;
                        row_cont[rr][cc] |= rr > ri;
                    }
                }
                ci += 1;
            }
        }
    }

    // An all-empty grid keeps docling's `(0, 0, 0, 0)` fallback bounds — the
    // table renders as a 1×1 empty cell (`|    |`), not nothing.
    let (min_r, max_r, min_c, max_c) = data_bounds(&grid).unwrap_or((0, 0, 0, 0));
    let slice = |g: Vec<Vec<bool>>| -> Vec<Vec<bool>> {
        g[min_r..=max_r]
            .iter()
            .map(|row| row[min_c..=max_c].to_vec())
            .collect()
    };
    let rows: Vec<Vec<String>> = grid[min_r..=max_r]
        .iter()
        .map(|row| row[min_c..=max_c].to_vec())
        .collect();
    // docling marks the first (trimmed) row of an ODF table as the header band.
    let header_row = (0..rows.len()).map(|r| r == 0).collect();
    let cell_blocks = any_rich.then(|| {
        blocks[min_r..=max_r]
            .iter()
            .map(|row| row[min_c..=max_c].to_vec())
            .collect()
    });
    Some(Table {
        rows,
        location: None,
        structure: Some(TableStructure {
            header_row,
            col_continuation: slice(col_cont),
            row_continuation: slice(row_cont),
            row_header: Vec::new(),
            col_header: Vec::new(),
        }),
        cell_blocks,
    })
}

/// The smallest rectangle covering all non-empty cells as `(min_r, max_r,
/// min_c, max_c)` — docling's `_find_true_data_bounds`; `None` when empty.
fn data_bounds(grid: &[Vec<String>]) -> Option<(usize, usize, usize, usize)> {
    let non_empty = |r: &Vec<String>| r.iter().any(|c| !c.trim().is_empty());
    let (min_r, max_r) = (
        grid.iter().position(non_empty)?,
        grid.iter().rposition(non_empty)?,
    );
    let ncols = grid[0].len();
    let col_used = |c: usize| (min_r..=max_r).any(|r| !grid[r][c].trim().is_empty());
    let min_c = (0..ncols).find(|&c| col_used(c))?;
    let max_c = (0..ncols).rposition(col_used)?;
    Some((min_r, max_r, min_c, max_c))
}

/// A table cell's Markdown. A *rich* cell (lists, multiple paragraphs, nested
/// tables or images) renders its full block content — formatting kept — flattened
/// to a single line; a *plain* cell renders its unformatted text. Mirrors
/// docling's `_odf_cell_is_rich` / `_odf_cell_text` split.
fn cell_text(tc: XmlNode, styles: &Styles) -> String {
    if is_rich_cell(tc, styles) {
        rich_cell_markdown(tc, styles)
    } else {
        plain_cell_text(tc)
    }
}

/// A *rich* cell's DocLang block content — the structured counterpart of
/// [`rich_cell_markdown`], built by walking the cell's children exactly like a
/// document body (so lists, headings, paragraphs with inline runs and nested
/// tables all render through the same code). Empty for a plain cell, whose flat
/// [`cell_text`] the serializer uses instead. Markdown/JSON never consult this.
fn cell_blocks_of(tc: XmlNode, styles: &Styles) -> Vec<Node> {
    if !is_rich_cell(tc, styles) {
        return Vec::new();
    }
    let mut sub = DoclingDocument::new("");
    walk_blocks(tc.children().filter(XmlNode::is_element), styles, &mut sub);
    sub.nodes
}

/// Whether a cell must render as rich block content — docling's
/// `_odf_cell_has_rich_content`: it holds an image, a renderable list, a header,
/// a nested table with content, a paragraph with an image, more than one
/// non-empty paragraph, or any non-empty paragraph in a cell without a typed
/// value (`office:value-type`) — presentation tables never type their cells, so
/// their text cells are all rich (docling's `cell.value is None` clause).
fn is_rich_cell(tc: XmlNode, styles: &Styles) -> bool {
    if cell_has_image(tc) {
        return true;
    }
    let mut non_empty_paragraphs = 0;
    for child in tc.children().filter(XmlNode::is_element) {
        match child.tag_name().name() {
            "list" if list_has_renderable(child, styles) => return true,
            "h" if !clean_lines(&para_plain_text(child)).is_empty() => return true,
            "table" if table_has_content(child) => return true,
            "p" => {
                if !clean_lines(&para_plain_text(child)).is_empty() {
                    non_empty_paragraphs += 1;
                }
                if cell_has_image(child) {
                    return true;
                }
            }
            _ => {}
        }
    }
    non_empty_paragraphs > 1 || (attr(tc, "value-type").is_none() && non_empty_paragraphs > 0)
}

/// A cell/paragraph holding a bitmap image (`<draw:image>`).
fn cell_has_image(n: XmlNode) -> bool {
    n.descendants().any(|d| d.has_tag_name("image"))
}

/// Whether a nested table has any non-empty cell.
fn table_has_content(table: XmlNode) -> bool {
    table
        .descendants()
        .filter(|n| n.has_tag_name("table-cell"))
        .any(|c| !plain_cell_text(c).trim().is_empty() || cell_has_image(c))
}

/// A plain cell's text: its paragraphs' unformatted text, blank-line-joined
/// (docling's `str(cell.value)` — subscripts inline, no Markdown markers).
fn plain_cell_text(tc: XmlNode) -> String {
    tc.children()
        .filter(|c| c.has_tag_name("p") || c.has_tag_name("h"))
        .map(para_plain_text)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// A paragraph's unformatted text, expanding `<text:s>`/`<text:tab>`/
/// `<text:line-break>` but dropping span formatting.
fn para_plain_text(el: XmlNode) -> String {
    let mut out = String::new();
    para_plain_into(el, &mut out);
    out
}

fn para_plain_into(el: XmlNode, out: &mut String) {
    for child in el.children() {
        if child.is_text() {
            out.push_str(child.text().unwrap_or(""));
        } else if child.is_element() {
            match child.tag_name().name() {
                "line-break" => out.push('\n'),
                "tab" => out.push('\t'),
                "s" => {
                    let n: usize = attr(child, "c").and_then(|v| v.parse().ok()).unwrap_or(1);
                    out.push_str(&" ".repeat(n));
                }
                _ => para_plain_into(child, out),
            }
        }
    }
}

/// Render a rich cell's block content to Markdown, then flatten to a single line
/// (docling's `RichTableCell` group serialized into the cell). A nested table is
/// flattened to its space-joined cell texts.
fn rich_cell_markdown(tc: XmlNode, styles: &Styles) -> String {
    let mut sub = DoclingDocument::new("");
    let mut prev_state: Option<ListCont> = None;
    for child in tc.children().filter(XmlNode::is_element) {
        match child.tag_name().name() {
            "list" => {
                prev_state = add_odf_list(child, styles, &mut sub, 0, 1, false, prev_state.take());
            }
            "table" => {
                prev_state = None;
                if let Some(table) = parse_table(child, styles) {
                    let text = table
                        .rows
                        .iter()
                        .flatten()
                        .filter(|c| !c.is_empty())
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" ");
                    if !text.is_empty() {
                        sub.push(Node::Paragraph { text });
                    }
                }
            }
            "p" | "h" => {
                prev_state = None;
                handle_block(child, styles, &mut sub, 0, &mut Vec::new());
            }
            _ => {}
        }
    }
    sub.export_to_markdown()
        .replace('\n', " ")
        .trim()
        .to_string()
}

// ---------------------------------------------------------------- spreadsheet

fn walk_spreadsheet(sheet: XmlNode, _styles: &Styles, doc: &mut DoclingDocument) {
    for table in sheet.children().filter(|c| c.has_tag_name("table")) {
        add_ods_sheet(table, doc);
    }
}

/// Split an ODS sheet into its disconnected data regions and emit each as a
/// separate table — a port of docling's `_convert_sheet_table` /
/// `_find_data_tables_in_sheet` (strict `gap_tolerance = 0` flood fill, singleton
/// cells kept as 1×1 tables). Numeric columns right-align via the shared table
/// serializer.
fn add_ods_sheet(table: XmlNode, doc: &mut DoclingDocument) {
    // Build a sparse content grid: (row, col) → cell text, expanding
    // `number-{rows,columns}-repeated` (empty repeats only advance the index, so a
    // sheet padded to millions of empty cells stays cheap).
    let mut cells: HashMap<(usize, usize), String> = HashMap::new();
    let mut row_idx = 0usize;
    for row in table.children().filter(|c| c.has_tag_name("table-row")) {
        let rrep = repeat(row, "number-rows-repeated");
        let mut row_cells: Vec<(usize, String)> = Vec::new();
        let mut col_idx = 0usize;
        let mut row_has_content = false;
        for cell in row
            .children()
            .filter(|c| c.has_tag_name("table-cell") || c.has_tag_name("covered-table-cell"))
        {
            let crep = repeat(cell, "number-columns-repeated");
            let covered = cell.has_tag_name("covered-table-cell");
            let text = ods_cell_text(cell);
            if !text.is_empty() || covered {
                row_has_content = true;
                for c in 0..crep.min(1024) {
                    row_cells.push((col_idx + c, text.clone()));
                }
            }
            col_idx += crep;
        }
        if row_has_content {
            for r in 0..rrep.min(1024) {
                for (c, text) in &row_cells {
                    cells.insert((row_idx + r, *c), text.clone());
                }
            }
        }
        row_idx += rrep;
    }
    if cells.is_empty() {
        return;
    }

    let min_row = cells.keys().map(|(r, _)| *r).min().unwrap();
    let max_row = cells.keys().map(|(r, _)| *r).max().unwrap();
    let min_col = cells.keys().map(|(_, c)| *c).min().unwrap();
    let max_col = cells.keys().map(|(_, c)| *c).max().unwrap();

    // Flood-fill connected content cells (4-directional, immediate neighbours
    // only) in row-major scan order, so region order matches docling's.
    let mut visited: HashSet<(usize, usize)> = HashSet::new();
    for ri in min_row..=max_row {
        for ci in min_col..=max_col {
            if visited.contains(&(ri, ci)) || !cells.contains_key(&(ri, ci)) {
                continue;
            }
            let mut region: HashSet<(usize, usize)> = HashSet::new();
            let mut queue: VecDeque<(usize, usize)> = VecDeque::new();
            queue.push_back((ri, ci));
            region.insert((ri, ci));
            let (mut rmin, mut rmax, mut cmin, mut cmax) = (ri, ri, ci, ci);
            while let Some((r, c)) = queue.pop_front() {
                rmin = rmin.min(r);
                rmax = rmax.max(r);
                cmin = cmin.min(c);
                cmax = cmax.max(c);
                for (dr, dc) in [(0i64, 1i64), (0, -1), (1, 0), (-1, 0)] {
                    let nr = r as i64 + dr;
                    let nc = c as i64 + dc;
                    if nr < 0 || nc < 0 {
                        continue;
                    }
                    let key = (nr as usize, nc as usize);
                    if !region.contains(&key) && cells.contains_key(&key) {
                        region.insert(key);
                        queue.push_back(key);
                    }
                }
            }
            visited.extend(region.iter().copied());

            let rows: Vec<Vec<String>> = (rmin..=rmax)
                .map(|r| {
                    (cmin..=cmax)
                        .map(|c| cells.get(&(r, c)).cloned().unwrap_or_default())
                        .collect()
                })
                .collect();
            doc.push(Node::Table(Table {
                rows,
                location: None,
                structure: None,
                cell_blocks: None,
            }));
        }
    }
}

/// A `number-*-repeated` attribute, at least 1.
fn repeat(node: XmlNode, name: &str) -> usize {
    attr(node, name)
        .and_then(|v| v.parse().ok())
        .filter(|&n: &usize| n >= 1)
        .unwrap_or(1)
}

/// An ODS cell's plain text — its paragraphs' text, newline-joined (github tables
/// are unescaped, matching docling's `_odf_cell_text` display).
fn ods_cell_text(cell: XmlNode) -> String {
    cell.children()
        .filter(|c| c.has_tag_name("p") || c.has_tag_name("h"))
        .map(|p| {
            p.descendants()
                .filter(|n| n.is_text())
                .filter_map(|n| n.text())
                .collect::<String>()
        })
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------- presentation

/// Port of docling's `OdpDocumentBackend`: each `<draw:page>` gets its slide
/// name as a title when no element on it is a visible title (a frame with
/// `presentation:class="title"`, or a `draw:custom-shape` carrying the slide's
/// first text); speaker notes (`<presentation:notes>`) and animations are
/// dropped; a title element's paragraphs become the slide title.
fn walk_presentation(pres: XmlNode, styles: &Styles, doc: &mut DoclingDocument) {
    for (idx, page) in pres
        .children()
        .filter(|c| c.has_tag_name("page"))
        .enumerate()
    {
        let name = attr(page, "name")
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("slide-{}", idx + 1));
        if !slide_has_visible_title(page) {
            doc.push(Node::Heading {
                level: 1,
                text: name,
            });
        }
        walk_slide(page, styles, doc);
    }
}

/// docling's `_odf_image_can_be_bitmap`: an explicit `draw:mime-type` decides
/// (raster `image/*` only); otherwise the href's suffix — vector/preview
/// formats (`.svm`, `.svg`, `.emf`, `.wmf`, `.pdf`) never yield a picture.
fn image_can_be_bitmap(img: XmlNode, href: &str) -> bool {
    if let Some(mime) = attr(img, "mime-type") {
        return mime.starts_with("image/") && mime != "image/svg+xml";
    }
    let name = href.rsplit('/').next().unwrap_or(href);
    let suffix = match name.rsplit_once('.') {
        Some((_, ext)) => ext.to_ascii_lowercase(),
        None => String::new(),
    };
    matches!(
        suffix.as_str(),
        "" | "bmp" | "gif" | "jpeg" | "jpg" | "png" | "tif" | "tiff" | "webp"
    )
}

/// Any non-blank text anywhere under the element (docling's
/// `_clean_odf_text_lines(text_recursive)` non-emptiness).
fn element_has_text(el: XmlNode) -> bool {
    el.descendants()
        .filter(|n| n.is_text())
        .any(|n| !n.text().unwrap_or("").trim().is_empty())
}

/// docling's `_is_slide_title_element`: an explicit `presentation:class="title"`,
/// or a `draw:custom-shape` holding the slide's first text content.
fn is_slide_title_element(el: XmlNode, is_first_text_content: bool) -> bool {
    if attr(el, "class") == Some("title") {
        return true;
    }
    is_first_text_content && el.tag_name().name() == "custom-shape"
}

fn slide_has_visible_title(page: XmlNode) -> bool {
    let mut seen_text = false;
    for el in page.children().filter(XmlNode::is_element) {
        let tag = el.tag_name().name();
        if tag == "notes" || tag == "par" {
            continue;
        }
        if is_slide_title_element(el, !seen_text) {
            return true;
        }
        if element_has_text(el) {
            seen_text = true;
        }
    }
    false
}

fn walk_slide(page: XmlNode, styles: &Styles, doc: &mut DoclingDocument) {
    let mut seen_text = false;
    for el in page.children().filter(XmlNode::is_element) {
        let tag = el.tag_name().name();
        // Speaker notes and animation trees never reach the document.
        if tag == "notes" || tag == "par" {
            continue;
        }
        let has_text = element_has_text(el);
        let is_title = is_slide_title_element(el, !seen_text);
        if has_text {
            seen_text = true;
        }
        if tag == "frame" {
            walk_slide_frame(el, styles, doc, is_title);
        } else {
            walk_textbox_children(
                el.children().filter(XmlNode::is_element),
                styles,
                doc,
                is_title,
            );
        }
    }
}

/// docling's `_walk_slide_frame` order: embedded charts, tables, images (the
/// chart's `ObjectReplacements/` preview is skipped once the chart itself is
/// emitted), then text boxes.
fn walk_slide_frame(frame: XmlNode, styles: &Styles, doc: &mut DoclingDocument, is_title: bool) {
    let mut chart_count = 0usize;
    if let Some(obj) = frame.children().find(|c| c.has_tag_name("object")) {
        let name = attr(obj, "href").unwrap_or("").trim_start_matches("./");
        if let Some(info) = styles.charts.get(name) {
            doc.push(Node::Chart {
                kind: info.kind.clone(),
                table: info.table.clone(),
                caption: None,
                location: None,
            });
            chart_count += 1;
        }
    }
    for tbl in frame.descendants().filter(|n| n.has_tag_name("table")) {
        if let Some(t) = parse_table(tbl, styles) {
            doc.push(Node::Table(t));
        }
    }
    for img in frame.descendants().filter(|n| n.has_tag_name("image")) {
        let href = attr(img, "href").unwrap_or("");
        if chart_count > 0
            && href
                .trim_start_matches("./")
                .starts_with("ObjectReplacements/")
        {
            continue;
        }
        if !image_can_be_bitmap(img, href) {
            continue;
        }
        doc.push(Node::Picture {
            caption: None,
            image: None,
        });
    }
    for tb in frame.descendants().filter(|n| n.has_tag_name("text-box")) {
        walk_textbox_children(
            tb.children().filter(XmlNode::is_element),
            styles,
            doc,
            is_title,
        );
    }
}

/// docling's `_walk_textbox_children`: headings keep their outline level, a
/// title element's paragraphs become the slide TITLE (a `#` heading), other
/// paragraphs plain text, and sibling lists continue their numbering.
fn walk_textbox_children<'a, 'i: 'a>(
    els: impl Iterator<Item = XmlNode<'a, 'i>>,
    styles: &Styles,
    doc: &mut DoclingDocument,
    is_title: bool,
) {
    let mut prev_state: Option<ListCont> = None;
    for el in els {
        match el.tag_name().name() {
            "h" => {
                prev_state = None;
                handle_block(el, styles, doc, 0, &mut Vec::new());
            }
            "p" => {
                prev_state = None;
                let mut runs = Vec::new();
                collect_runs(el, styles, Fmt::default(), &mut runs);
                let text = runs_to_text(runs.clone());
                if text.is_empty() {
                    continue;
                }
                if is_title {
                    doc.push(Node::Heading { level: 1, text });
                } else {
                    doc.push(inline_paragraph_node(text, runs_to_inline(runs), false));
                }
            }
            "list" => {
                prev_state = add_odf_list(el, styles, doc, 0, 1, false, prev_state.take());
            }
            _ => {}
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

    #[test]
    fn ods_sheet_splits_into_regions() {
        // A title cell (isolated by an empty row) and a 2×2 data block become two
        // separate tables (strict gap-tolerance flood fill).
        let xml = r#"<root xmlns:table="t" xmlns:text="x">
          <table:table>
            <table:table-row><table:table-cell/>
              <table:table-cell><text:p>Title</text:p></table:table-cell></table:table-row>
            <table:table-row><table:table-cell/></table:table-row>
            <table:table-row><table:table-cell/>
              <table:table-cell><text:p>H1</text:p></table:table-cell>
              <table:table-cell><text:p>H2</text:p></table:table-cell></table:table-row>
            <table:table-row><table:table-cell/>
              <table:table-cell><text:p>1</text:p></table:table-cell>
              <table:table-cell><text:p>2</text:p></table:table-cell></table:table-row>
          </table:table></root>"#;
        let dom = Document::parse(xml).unwrap();
        let table = dom.descendants().find(|n| n.has_tag_name("table")).unwrap();
        let mut doc = DoclingDocument::new("t");
        add_ods_sheet(table, &mut doc);
        let tables: Vec<&Table> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Table(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(tables.len(), 2, "title singleton + data region");
        assert_eq!(tables[0].rows, vec![vec!["Title".to_string()]]);
        assert_eq!(tables[1].rows.len(), 2, "header + one data row");
        assert_eq!(tables[1].rows[0], vec!["H1".to_string(), "H2".to_string()]);
    }

    #[test]
    fn list_continues_across_empty_nested_item() {
        // A numbered `<text:list>` followed by a second list that opens with an
        // empty item wrapping a nested list continues the numbering (3.) while the
        // nested bullets collapse under the previous item (level 1).
        let xml = r#"<root xmlns:text="x" xmlns:style="s">
          <style:list-style style:name="L1">
            <text:list-level-style-number text:level="1"/></style:list-style>
          <style:list-style style:name="L2">
            <text:list-level-style-bullet text:level="1"/>
            <text:list-level-style-bullet text:level="2"/></style:list-style>
          <office:body xmlns:office="o"><office:text>
            <text:list text:style-name="L1">
              <text:list-item><text:p>one</text:p></text:list-item>
              <text:list-item><text:p>two</text:p></text:list-item>
            </text:list>
            <text:list text:style-name="L2">
              <text:list-item><text:list>
                <text:list-item><text:p>bullet</text:p></text:list-item>
              </text:list></text:list-item>
              <text:list-item><text:p>three</text:p></text:list-item>
            </text:list>
          </office:text></office:body></root>"#;
        let dom = Document::parse(xml).unwrap();
        let styles = parse_styles(&dom, None);
        let body = dom.descendants().find(|n| n.has_tag_name("text")).unwrap();
        let mut doc = DoclingDocument::new("t");
        walk_text(body, &styles, &mut doc);
        let items: Vec<(u64, u8, &str)> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::ListItem {
                    number,
                    level,
                    text,
                    ..
                } => Some((*number, *level, text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            items,
            vec![
                (1, 0, "one"),
                (2, 0, "two"),
                (0, 1, "bullet"),
                (3, 0, "three"),
            ]
        );
    }

    fn table_of(xml: &str) -> Table {
        let dom = Document::parse(xml).unwrap();
        let styles = parse_styles(&dom, None);
        let table = dom.descendants().find(|n| n.has_tag_name("table")).unwrap();
        parse_table(table, &styles).unwrap()
    }

    #[test]
    fn rich_cell_renders_list_plain_cell_drops_formatting() {
        let xml = r#"<root xmlns:table="t" xmlns:text="x">
          <table:table><table:table-row>
            <table:table-cell><text:p>List:</text:p><text:list>
              <text:list-item><text:p>a</text:p></text:list-item>
              <text:list-item><text:p>b</text:p></text:list-item></text:list></table:table-cell>
            <table:table-cell><text:p>plain <text:span>bold</text:span></text:p></table:table-cell>
          </table:table-row></table:table></root>"#;
        let t = table_of(xml);
        // Rich cell: the list is rendered with markers and flattened into the cell.
        assert_eq!(t.rows[0][0], "List:  - a - b");
        // Plain cell: single paragraph, no Markdown markers.
        assert_eq!(t.rows[0][1], "plain bold");
    }

    #[test]
    fn merged_cells_leave_covered_columns_blank() {
        let xml = r#"<root xmlns:table="t" xmlns:text="x">
          <table:table>
            <table:table-row>
              <table:table-cell table:number-columns-spanned="2"><text:p>Wide</text:p></table:table-cell>
              <table:covered-table-cell/>
              <table:table-cell><text:p>C</text:p></table:table-cell>
            </table:table-row>
            <table:table-row>
              <table:table-cell><text:p>x</text:p></table:table-cell>
              <table:table-cell><text:p>y</text:p></table:table-cell>
              <table:table-cell><text:p>z</text:p></table:table-cell>
            </table:table-row>
          </table:table></root>"#;
        let t = table_of(xml);
        assert_eq!(t.rows[0], vec!["Wide", "", "C"]);
        assert_eq!(t.rows[1], vec!["x", "y", "z"]);
    }
}
