//! DocTags reader (issues #152 / #77): the token markup docling's VLMs
//! (granite-docling / SmolDocling) emit, parsed into a [`DoclingDocument`].
//!
//! DocTags is **not XML** — it is a stream of special tokens interleaved with
//! text: `<loc_N>` location tokens and OTSL table-cell markers
//! (`<fcel>Text<fcel>…<nl>`) are unclosed, captions nest inside `<otsl>`,
//! and a model under sampling pressure emits stray text, truncated elements
//! and unknown tokens. This module is therefore a tolerant lexer plus a
//! recursive-descent walk: every recognized structure maps onto the document
//! model (headings, paragraphs, lists, OTSL tables *with* span structure,
//! pictures, page furniture, code, formulas), `<loc_*>` runs become layout
//! provenance ([`Node::Located`] / per-node locations — DocTags' 0–500 grid
//! is carried as-is, matching the 0–511 DocLang convention closely enough
//! for provenance), and anything unrecognized degrades to text or is skipped
//! — never an error. Garbage in, best-effort document out: VLM output is
//! hostile by nature, and a parser that fails hard would fail every second
//! page.
//!
//! docling-parity choices, pinned by the VLM pipeline's live tests against
//! real granite-docling output:
//! - `<section_header_level_K>` → heading level `K+1` (`#` is the document
//!   title, sections start at `##` — matches docling's Markdown export);
//! - an in-`<otsl>` `<caption>` becomes a paragraph *before* the table
//!   (docling's reading order);
//! - `<page_header>`/`<page_footer>` become [`Node::PageFurniture`], which
//!   the Markdown/JSON exports omit like every other furniture.

use crate::document::{DoclingDocument, FieldItem, Node, Table, TableStructure};

/// Parse one DocTags fragment (typically one page's model output).
pub fn parse(markup: &str) -> DoclingDocument {
    parse_pages([markup])
}

/// Parse multiple per-page fragments into one document, mirroring Python
/// docling-core's `DocTagsDocument.from_doctags_and_image_pairs` +
/// `DoclingDocument.load_from_doctags` (sans images). A [`Node::PageBreak`]
/// separates pages in the model; Markdown/JSON exports omit it.
pub fn parse_pages<'a>(pages: impl IntoIterator<Item = &'a str>) -> DoclingDocument {
    let mut doc = DoclingDocument::new("doctags");
    let mut first = true;
    for page in pages {
        if !first {
            doc.nodes.push(Node::PageBreak);
        }
        first = false;
        let toks = lex(page);
        let mut i = 0;
        parse_blocks(&toks, &mut i, &mut doc.nodes, 0);
    }
    doc
}

// ---------------------------------------------------------------------------
// Lexer.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok<'a> {
    /// `<name>` (attributes, if any, are irrelevant to DocTags and dropped).
    Open(&'a str),
    /// `</name>`
    Close(&'a str),
    /// `<loc_N>`
    Loc(u16),
    /// Raw text between tokens (entities decoded).
    Text(String),
}

/// Split the markup into tags and text. A `<` that doesn't start a plausible
/// tag is literal text (`a < b`).
fn lex(markup: &str) -> Vec<Tok<'_>> {
    let mut toks = Vec::new();
    let mut rest = markup;
    let mut text = String::new();
    while let Some(lt) = rest.find('<') {
        let (before, tag_on) = rest.split_at(lt);
        text.push_str(before);
        let Some(gt) = tag_on.find('>') else {
            text.push('<');
            rest = &tag_on[1..];
            continue;
        };
        let inner = tag_on[1..gt].trim();
        let close = inner.starts_with('/');
        let name = inner
            .trim_start_matches('/')
            .trim_end_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or_default();
        if name.is_empty()
            || !name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || inner.contains('<')
        {
            // Not a tag (`a < b`, or a stray `<` right before a real tag):
            // the `<` is literal text.
            text.push('<');
            rest = &tag_on[1..];
            continue;
        }
        if !text.trim().is_empty() {
            toks.push(Tok::Text(decode_entities(text.trim())));
        }
        text.clear();
        if let Some(v) = name
            .strip_prefix("loc_")
            .and_then(|v| v.parse::<u16>().ok())
        {
            toks.push(Tok::Loc(v));
        } else if close {
            toks.push(Tok::Close(name));
        } else {
            toks.push(Tok::Open(name));
        }
        rest = &tag_on[gt + 1..];
    }
    if !rest.trim().is_empty() {
        text.push_str(rest);
    }
    if !text.trim().is_empty() {
        toks.push(Tok::Text(decode_entities(text.trim())));
    }
    toks
}

/// The model may emit XML-style entities or raw characters; accept both.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

// ---------------------------------------------------------------------------
// Parser.
// ---------------------------------------------------------------------------

/// Element names that start a block — inline text collection stops there so a
/// missing closer (truncated generation) doesn't swallow the next block.
fn is_block_start(name: &str) -> bool {
    matches!(
        name,
        "text"
            | "paragraph"
            | "title"
            | "caption"
            | "footnote"
            | "code"
            | "formula"
            | "otsl"
            | "picture"
            | "chart"
            | "ordered_list"
            | "unordered_list"
            | "list_item"
            | "page_header"
            | "page_footer"
            | "page_break"
            | "checkbox_selected"
            | "checkbox_unselected"
            | "key_value_region"
            | "field_region"
            | "doctag"
            | "doctags"
    ) || name.starts_with("section_header_level_")
}

/// Wrap `node` in [`Node::Located`] when a `loc_l,t,r,b` run was collected.
fn located(node: Node, loc: Option<[u16; 4]>) -> Node {
    match loc {
        Some(location) => Node::Located {
            location,
            inner: Box::new(node),
        },
        None => node,
    }
}

/// Collect an element's inline content: text joins with spaces, the first
/// four `<loc_N>` become its location, unknown tokens are transparent.
/// Consumes the matching closer when present; stops (without consuming) at a
/// block-start tag, so truncated output loses only its own element.
fn collect_inline(toks: &[Tok], i: &mut usize, name: &str) -> (String, Option<[u16; 4]>) {
    let mut text = String::new();
    let mut locs: Vec<u16> = Vec::new();
    while *i < toks.len() {
        match &toks[*i] {
            Tok::Close(n) if *n == name => {
                *i += 1;
                break;
            }
            Tok::Open(n) if is_block_start(n) => break,
            Tok::Close(n) if is_block_start(n) => break,
            Tok::Loc(v) => {
                if locs.len() < 4 {
                    locs.push(*v);
                }
                *i += 1;
            }
            Tok::Text(t) => {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(t);
                *i += 1;
            }
            // Unknown inline token (checkbox states, styling a future model
            // might emit): transparent.
            Tok::Open(_) | Tok::Close(_) => *i += 1,
        }
    }
    let loc = (locs.len() == 4).then(|| [locs[0], locs[1], locs[2], locs[3]]);
    (text, loc)
}

fn parse_blocks(toks: &[Tok], i: &mut usize, out: &mut Vec<Node>, list_level: u8) {
    // Location tokens seen at block level (the degraded no-structure shape
    // servers produce when they strip special tokens) attach to the next
    // bare-text paragraph.
    let mut pending_loc: Vec<u16> = Vec::new();
    while *i < toks.len() {
        match &toks[*i] {
            Tok::Loc(v) => {
                if pending_loc.len() < 4 {
                    pending_loc.push(*v);
                }
                *i += 1;
            }
            Tok::Text(t) => {
                // Stray block-level text (no wrapping element): a paragraph.
                let loc = (pending_loc.len() == 4).then(|| {
                    [
                        pending_loc[0],
                        pending_loc[1],
                        pending_loc[2],
                        pending_loc[3],
                    ]
                });
                pending_loc.clear();
                out.push(located(Node::Paragraph { text: t.clone() }, loc));
                *i += 1;
            }
            Tok::Close(_) => {
                // Stray closer (ours ended already, or the model's nesting is
                // off): skip. Returning here would drop the rest of the page.
                *i += 1;
            }
            Tok::Open(name) => {
                let name = *name;
                pending_loc.clear();
                *i += 1;
                if let Some(level) = name.strip_prefix("section_header_level_") {
                    let level: u8 = level.parse().unwrap_or(1);
                    let (text, loc) = collect_inline(toks, i, name);
                    if !text.is_empty() {
                        out.push(located(
                            Node::Heading {
                                // docling parity: section level K renders as
                                // heading K+1 ("#" is the document title).
                                level: level.saturating_add(1).clamp(2, 6),
                                text,
                            },
                            loc,
                        ));
                    }
                    continue;
                }
                match name {
                    "doctag" | "doctags" => {}
                    "page_break" => out.push(Node::PageBreak),
                    "title" => {
                        let (text, loc) = collect_inline(toks, i, name);
                        if !text.is_empty() {
                            out.push(located(Node::Heading { level: 1, text }, loc));
                        }
                    }
                    "text" | "paragraph" | "footnote" | "caption" => {
                        let (text, loc) = collect_inline(toks, i, name);
                        if !text.is_empty() {
                            out.push(located(Node::Paragraph { text }, loc));
                        }
                    }
                    "page_header" | "page_footer" => {
                        let (text, loc) = collect_inline(toks, i, name);
                        if !text.is_empty() {
                            out.push(Node::PageFurniture {
                                footer: name == "page_footer",
                                location: loc.unwrap_or([0; 4]),
                                text,
                            });
                        }
                    }
                    "code" => {
                        let (mut text, loc) = collect_inline(toks, i, name);
                        // DocTags carries the language as a `<_lang_>` token,
                        // which the lexer keeps as literal text (a leading
                        // underscore is no tag name); peel it off here.
                        let mut language = None;
                        if let Some(rest) = text.strip_prefix("<_") {
                            if let Some((lang, body)) = rest.split_once("_>") {
                                language = Some(lang.to_string()).filter(|l| !l.is_empty());
                                text = body.trim_start().to_string();
                            }
                        }
                        if !text.is_empty() {
                            out.push(located(
                                Node::Code {
                                    language,
                                    text,
                                    orig: None,
                                },
                                loc,
                            ));
                        }
                    }
                    "formula" => {
                        let (latex, loc) = collect_inline(toks, i, name);
                        if !latex.is_empty() {
                            out.push(Node::Formula {
                                orig: latex.clone(),
                                latex,
                                location: loc,
                            });
                        }
                    }
                    "ordered_list" | "unordered_list" => {
                        parse_list(toks, i, out, name == "ordered_list", list_level);
                    }
                    // A bare item outside a list wrapper (truncated output).
                    "list_item" => {
                        let (text, loc) = collect_inline(toks, i, name);
                        if !text.is_empty() {
                            out.push(Node::ListItem {
                                ordered: false,
                                number: 1,
                                first_in_list: true,
                                text,
                                level: list_level,
                                marker: None,
                                location: loc,
                                dclx: None,
                                href: None,
                                layer: None,
                            });
                        }
                    }
                    "otsl" => parse_otsl(toks, i, out),
                    "picture" | "chart" => parse_picture(toks, i, out, name),
                    // A form checkbox: the token precedes its label text.
                    "checkbox_selected" | "checkbox_unselected" => {
                        let (text, loc) = collect_inline(toks, i, name);
                        if !text.is_empty() {
                            out.push(located(
                                Node::CheckboxItem {
                                    checked: name == "checkbox_selected",
                                    text,
                                },
                                loc,
                            ));
                        }
                    }
                    // A form key-value region (DocTags' key_value_region /
                    // DocLang's field_region).
                    "key_value_region" | "field_region" => parse_field_region(toks, i, out, name),
                    // Unknown block token: transparent (its text will surface
                    // as stray block-level text above).
                    _ => {}
                }
            }
        }
    }
}

fn parse_list(toks: &[Tok], i: &mut usize, out: &mut Vec<Node>, ordered: bool, level: u8) {
    let close = if ordered {
        "ordered_list"
    } else {
        "unordered_list"
    };
    let mut number: u64 = 0;
    let mut first = true;
    while *i < toks.len() {
        match &toks[*i] {
            Tok::Close(n) if *n == close => {
                *i += 1;
                break;
            }
            Tok::Open("list_item") => {
                *i += 1;
                let (text, loc) = collect_inline(toks, i, "list_item");
                if !text.is_empty() {
                    number += 1;
                    out.push(Node::ListItem {
                        ordered,
                        number,
                        first_in_list: std::mem::take(&mut first),
                        text,
                        level,
                        marker: None,
                        location: loc,
                        dclx: None,
                        href: None,
                        layer: None,
                    });
                }
            }
            Tok::Open(n @ ("ordered_list" | "unordered_list")) => {
                let nested_ordered = *n == "ordered_list";
                *i += 1;
                parse_list(toks, i, out, nested_ordered, level + 1);
            }
            // Anything else (locs, stray text between items): skip.
            Tok::Open(n) if is_block_start(n) && *n != "list_item" => break,
            _ => *i += 1,
        }
    }
}

/// OTSL cell-marker kinds, in DocTags' unclosed-token form.
fn cell_kind(name: &str) -> Option<&'static str> {
    match name {
        "fcel" => Some("fcel"),
        "ched" => Some("ched"),
        "rhed" => Some("rhed"),
        "ecel" => Some("ecel"),
        "lcel" => Some("lcel"),
        "ucel" => Some("ucel"),
        "xcel" => Some("xcel"),
        _ => None,
    }
}

fn parse_otsl(toks: &[Tok], i: &mut usize, out: &mut Vec<Node>) {
    let mut locs: Vec<u16> = Vec::new();
    let mut caption: Option<String> = None;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut kinds: Vec<Vec<&'static str>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut kind_row: Vec<&'static str> = Vec::new();
    let mut cell: Option<(&'static str, String)> = None;

    fn close_cell(
        cell: &mut Option<(&'static str, String)>,
        row: &mut Vec<String>,
        kind_row: &mut Vec<&'static str>,
    ) {
        if let Some((kind, text)) = cell.take() {
            row.push(text.trim().to_string());
            kind_row.push(kind);
        }
    }
    fn close_row(
        cell: &mut Option<(&'static str, String)>,
        row: &mut Vec<String>,
        kind_row: &mut Vec<&'static str>,
        rows: &mut Vec<Vec<String>>,
        kinds: &mut Vec<Vec<&'static str>>,
    ) {
        close_cell(cell, row, kind_row);
        if !row.is_empty() {
            rows.push(std::mem::take(row));
            kinds.push(std::mem::take(kind_row));
        }
    }

    while *i < toks.len() {
        match &toks[*i] {
            Tok::Close("otsl") => {
                *i += 1;
                break;
            }
            Tok::Open("nl") => {
                close_row(&mut cell, &mut row, &mut kind_row, &mut rows, &mut kinds);
                *i += 1;
            }
            Tok::Open("caption") => {
                *i += 1;
                let (text, _) = collect_inline(toks, i, "caption");
                if !text.is_empty() {
                    caption = Some(text);
                }
            }
            Tok::Open(n) => {
                if let Some(kind) = cell_kind(n) {
                    close_cell(&mut cell, &mut row, &mut kind_row);
                    cell = Some((kind, String::new()));
                    *i += 1;
                } else if is_block_start(n) {
                    // Truncated table (no </otsl>): don't eat the next block.
                    break;
                } else {
                    *i += 1;
                }
            }
            Tok::Loc(v) => {
                if cell.is_none() && locs.len() < 4 {
                    locs.push(*v);
                }
                *i += 1;
            }
            Tok::Text(t) => {
                if let Some((_, text)) = cell.as_mut() {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(t);
                }
                *i += 1;
            }
            Tok::Close(_) => *i += 1,
        }
    }
    close_row(&mut cell, &mut row, &mut kind_row, &mut rows, &mut kinds);

    // docling's reading order puts the caption paragraph before the table.
    if let Some(text) = caption {
        out.push(Node::Paragraph { text });
    }
    if rows.is_empty() {
        return;
    }
    // Span/header structure from the marker kinds. `rows` keeps empty
    // strings for continuation cells (the span text is in the anchor cell),
    // which is exactly how the Markdown/DocLang serializers expect the grid.
    let structure = TableStructure {
        header_row: kinds
            .iter()
            .map(|k| {
                let mut headers = 0usize;
                let mut filled = 0usize;
                for kind in k {
                    match *kind {
                        "ched" => {
                            headers += 1;
                            filled += 1;
                        }
                        "fcel" | "rhed" => filled += 1,
                        _ => {}
                    }
                }
                filled > 0 && headers == filled
            })
            .collect(),
        col_continuation: kinds
            .iter()
            .map(|k| {
                k.iter()
                    .map(|kind| matches!(*kind, "lcel" | "xcel"))
                    .collect()
            })
            .collect(),
        row_continuation: kinds
            .iter()
            .map(|k| {
                k.iter()
                    .map(|kind| matches!(*kind, "ucel" | "xcel"))
                    .collect()
            })
            .collect(),
        row_header: kinds
            .iter()
            .map(|k| k.iter().map(|kind| *kind == "rhed").collect())
            .collect(),
        col_header: kinds
            .iter()
            .map(|k| k.iter().map(|kind| *kind == "ched").collect())
            .collect(),
    };
    out.push(Node::Table(Table {
        rows,
        location: (locs.len() == 4).then(|| [locs[0], locs[1], locs[2], locs[3]]),
        structure: Some(structure),
        cell_blocks: None,
    }));
}

fn parse_picture(toks: &[Tok], i: &mut usize, out: &mut Vec<Node>, close: &str) {
    let mut locs: Vec<u16> = Vec::new();
    let mut caption: Option<String> = None;
    while *i < toks.len() {
        match &toks[*i] {
            Tok::Close(n) if *n == close => {
                *i += 1;
                break;
            }
            Tok::Open("caption") => {
                *i += 1;
                let (text, _) = collect_inline(toks, i, "caption");
                if !text.is_empty() {
                    caption = Some(text);
                }
            }
            Tok::Open("otsl") => {
                // A chart's tabular payload: emit the picture first (below),
                // then the data table after it, like the chart backends do.
                break;
            }
            Tok::Open(n) if is_block_start(n) => break,
            Tok::Loc(v) => {
                if locs.len() < 4 {
                    locs.push(*v);
                }
                *i += 1;
            }
            // Picture-class tokens (`<other>`, `<pie_chart>`, …) and stray
            // text: dropped — docling's picture body text is not extracted.
            _ => *i += 1,
        }
    }
    out.push(located(
        Node::Picture {
            caption,
            image: None,
            classification: None,
        },
        (locs.len() == 4).then(|| [locs[0], locs[1], locs[2], locs[3]]),
    ));
}

/// A form key-value region: tolerant over both the DocTags shape
/// (`<key_1>…</key_1><value_1>…</value_1>` numbered pairs) and the DocLang
/// shape (`<field_item><marker/><key/><value/></field_item>`). A `key`
/// starts a new item; `value`/`marker` attach to the current one.
fn parse_field_region(toks: &[Tok], i: &mut usize, out: &mut Vec<Node>, close: &str) {
    let mut items: Vec<FieldItem> = Vec::new();
    let mut current: Option<FieldItem> = None;
    while *i < toks.len() {
        match &toks[*i] {
            Tok::Close(n) if *n == close => {
                *i += 1;
                break;
            }
            Tok::Open(n @ ("field_item" | "unmatched_value")) => {
                let n = *n;
                if let Some(item) = current.take() {
                    items.push(item);
                }
                current = Some(FieldItem::default());
                *i += 1;
                // `<unmatched_value>` (a value with no key) carries its text
                // directly rather than nested key/value children.
                if n == "unmatched_value" {
                    let (text, _) = collect_inline(toks, i, n);
                    if let (Some(item), false) = (current.as_mut(), text.is_empty()) {
                        item.value = Some(text);
                    }
                }
            }
            Tok::Close("field_item") => {
                if let Some(item) = current.take() {
                    items.push(item);
                }
                *i += 1;
            }
            Tok::Open(n) => {
                let n = *n;
                let is_key = n == "key" || n.starts_with("key_");
                let is_value = n == "value" || n.starts_with("value_");
                let is_marker = n == "marker";
                if !(is_key || is_value || is_marker) {
                    if is_block_start(n) {
                        break;
                    }
                    *i += 1;
                    continue;
                }
                *i += 1;
                let (text, _) = collect_inline(toks, i, n);
                if text.is_empty() {
                    continue;
                }
                if is_key {
                    // A key starts a new pair (numbered DocTags pairs come
                    // key-then-value with no item wrapper).
                    if current.as_ref().is_some_and(|c| c.key.is_some()) {
                        items.push(current.take().unwrap_or_default());
                    }
                    current.get_or_insert_with(FieldItem::default).key = Some(text);
                } else if is_value {
                    current.get_or_insert_with(FieldItem::default).value = Some(text);
                } else {
                    current.get_or_insert_with(FieldItem::default).marker = Some(text);
                }
            }
            _ => *i += 1,
        }
    }
    if let Some(item) = current.take() {
        items.push(item);
    }
    items.retain(|it| it.marker.is_some() || it.key.is_some() || it.value.is_some());
    if !items.is_empty() {
        out.push(Node::FieldRegion { items });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape live granite-docling emits (loc runs, unclosed OTSL
    /// markers with span cells, in-otsl caption, furniture headers) — from
    /// the #77 bring-up against a real endpoint.
    #[test]
    fn granite_page_parses_with_structure() {
        let page = "<doctag><page_header><loc_159><loc_59><loc_366><loc_65>Running Title</page_header>\n\
<text><loc_109><loc_75><loc_393><loc_96>Intro paragraph.</text>\n\
<section_header_level_1><loc_110><loc_106><loc_260><loc_112>5.1 Optimization</section_header_level_1>\n\
<otsl><loc_114><loc_212><loc_388><loc_297><ched>A<ched>B<lcel><nl><fcel>1<fcel>2<fcel>3<nl>\
<caption><loc_109><loc_173><loc_393><loc_206>Table 1. Caption text.</caption></otsl>\n\
<unordered_list><list_item><loc_1><loc_2><loc_3><loc_4>First</list_item><list_item>Second</list_item></unordered_list>\n\
<picture><loc_5><loc_6><loc_7><loc_8><other><caption>Fig 1.</caption></picture>\n\
</doctag>";
        let doc = parse(page);
        let md = doc.export_to_markdown();
        assert!(md.contains("Intro paragraph."), "md: {md:?}");
        assert!(md.contains("## 5.1 Optimization"), "md: {md:?}");
        // Caption precedes the table.
        assert!(
            md.find("Table 1. Caption text.").unwrap() < md.find("| A").unwrap_or(usize::MAX),
            "md: {md:?}"
        );
        assert!(
            md.contains("- First") && md.contains("- Second"),
            "md: {md:?}"
        );
        // Furniture stays out of the body.
        assert!(!md.contains("Running Title"), "md: {md:?}");
        // Geometry survives as provenance on the table.
        let table = doc
            .nodes
            .iter()
            .find_map(|n| match n {
                Node::Table(t) => Some(t),
                _ => None,
            })
            .expect("table parsed");
        assert_eq!(table.location, Some([114, 212, 388, 297]));
        let s = table.structure.as_ref().expect("structure");
        assert_eq!(s.header_row, vec![true, false]);
        assert_eq!(s.col_continuation[0], vec![false, false, true]);
    }

    /// The degraded shape token-stripping servers produce: loc runs + bare
    /// text, no structure tags at all. Text must survive as paragraphs.
    #[test]
    fn degraded_loc_and_text_becomes_paragraphs() {
        let doc = parse(
            "<loc_1><loc_2><loc_3><loc_4>First line.\n<loc_5><loc_6><loc_7><loc_8>Second line.",
        );
        let md = doc.export_to_markdown();
        assert!(md.contains("First line."), "md: {md:?}");
        assert!(md.contains("Second line."), "md: {md:?}");
    }

    /// Truncated generation: an element with no closer must not swallow the
    /// following block.
    #[test]
    fn truncated_element_does_not_eat_next_block() {
        let doc = parse("<text><loc_1><loc_2><loc_3><loc_4>Cut off\n<section_header_level_1>Next</section_header_level_1>");
        let md = doc.export_to_markdown();
        assert!(md.contains("Cut off"), "md: {md:?}");
        assert!(md.contains("## Next"), "md: {md:?}");
    }

    #[test]
    fn entities_and_stray_angle_brackets() {
        let doc = parse("<text>A &amp; B & a < b</text>");
        let md = doc.export_to_markdown();
        assert!(md.contains("A & B & a < b"), "md: {md:?}");
    }

    #[test]
    fn checkboxes_and_key_value_regions() {
        let doc = parse(
            "<text><checkbox_selected>Done item</text>\
<checkbox_unselected><loc_1><loc_2><loc_3><loc_4>Todo item\
<key_value_region><key_1><loc_1><loc_2><loc_3><loc_4>Name</key_1><value_1>John</value_1>\
<key_2>City</key_2><value_2>Berlin</value_2></key_value_region>",
        );
        // Both forms produce checkbox items (the located one sits inside a
        // Node::Located wrapper, so assert via the Markdown rendering).
        let md = doc.export_to_markdown();
        assert!(md.contains("- [x] Done item"), "md: {md:?}");
        assert!(md.contains("- [ ] Todo item"), "md: {md:?}");
        let items = doc
            .nodes
            .iter()
            .find_map(|n| match n {
                Node::FieldRegion { items } => Some(items),
                _ => None,
            })
            .expect("field region parsed");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].key.as_deref(), Some("Name"));
        assert_eq!(items[0].value.as_deref(), Some("John"));
        assert_eq!(items[1].key.as_deref(), Some("City"));
        assert_eq!(items[1].value.as_deref(), Some("Berlin"));
    }

    #[test]
    fn pages_join_with_page_breaks() {
        let doc = parse_pages(["<text>One.</text>", "<text>Two.</text>"]);
        assert!(matches!(doc.nodes[1], Node::PageBreak));
        let md = doc.export_to_markdown();
        assert!(md.contains("One.") && md.contains("Two."));
    }
}
