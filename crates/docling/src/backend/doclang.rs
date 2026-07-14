//! DocLang input backend — reads `.dclg`/`.dclg.xml` (bare DocLang XML) and
//! `.dclx` (the OPC archive whose `document.xml` holds the markup) back into a
//! [`DoclingDocument`], the inverse of the DocLang serializer in
//! `docling-core::doclang`. Mirrors docling's `DocLangDocumentBackend` /
//! `DocLangArchiveBackend` (docling-core's `DocLangDocDeserializer`).
//!
//! The parse is deliberately tolerant: unknown elements recurse into their
//! children, unknown attributes are ignored, and whitespace follows the
//! deserializer's rules — pretty-printed indentation collapses, while CDATA
//! sections and `<content>` wrappers keep their text verbatim.

use roxmltree::{Document, Node as XmlNode, NodeType};

use crate::backend::ooxml::Package;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::format::InputFormat;
use crate::source::SourceDocument;
use docling_core::{ContentLayer, DoclingDocument, Node, Table, TableStructure};

pub struct DoclangBackend;

impl DeclarativeBackend for DoclangBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let xml: String = if source.format == InputFormat::Dclx {
            let mut pkg = Package::open(&source.bytes)
                .ok_or_else(|| ConversionError::Parse("dclx: not a zip archive".into()))?;
            pkg.read("document.xml")
                .ok_or_else(|| ConversionError::Parse("dclx: no document.xml".into()))?
        } else {
            source.text()?.to_string()
        };
        let dom =
            Document::parse(&xml).map_err(|e| ConversionError::Parse(format!("doclang: {e}")))?;
        let root = dom.root_element();
        if root.tag_name().name() != "doclang" {
            return Err(ConversionError::Parse(format!(
                "doclang: unexpected root element <{}>",
                root.tag_name().name()
            )));
        }
        let mut doc = DoclingDocument::new(&source.name);
        walk_body(root, &mut doc.nodes);
        Ok(doc)
    }
}

// ---------------------------------------------------------------------------
// Block-level walk.
// ---------------------------------------------------------------------------

fn walk_body(parent: XmlNode, out: &mut Vec<Node>) {
    for el in parent.children().filter(|n| n.is_element()) {
        match el.tag_name().name() {
            "heading" => {
                let level: u8 = attr(el, "level")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1)
                    .max(1);
                // A heading's `<href>` head is dropped — docling's deserializer
                // keeps heading text plain.
                let inline = parse_inline(el);
                if inline.text.is_empty() {
                    continue;
                }
                push_with_layer(
                    out,
                    inline.layer,
                    Node::Heading {
                        level,
                        text: inline.text,
                    },
                );
            }
            "text" => {
                let mut inline = parse_inline(el);
                // An inline-group-shaped `<text>` (several runs) comes back on
                // the body layer even when its runs carry `<layer>` heads —
                // docling stamps the layer per child, and the group itself
                // defaults to body.
                if is_inline_group(el) {
                    inline.layer = None;
                }
                let node = if let Some(checked) = inline.checkbox {
                    Node::CheckboxItem {
                        checked,
                        text: inline.text,
                    }
                } else if inline.href.is_some() {
                    // A hyperlink text comes back as its plain anchor — the
                    // target and any styling are dropped in docling's
                    // round-trip.
                    Node::Paragraph {
                        text: strip_md_markers(&inline.text),
                    }
                } else {
                    Node::Paragraph { text: inline.text }
                };
                push_with_layer(out, inline.layer, node);
            }
            "formula" => {
                let text = raw_text(el);
                out.push(Node::Paragraph {
                    text: format!("$${text}$$"),
                });
            }
            "code" => {
                let language = el
                    .children()
                    .find(|c| c.has_tag_name("label"))
                    .and_then(|l| attr(l, "value"))
                    .map(str::to_string);
                out.push(Node::Code {
                    language,
                    text: code_text(el),
                });
            }
            "list" => parse_list(el, 0, out),
            "table" => {
                let layer = el
                    .children()
                    .find(|c| c.has_tag_name("layer"))
                    .and_then(parse_layer);
                if let Some(table) = parse_table(el) {
                    push_with_layer(out, layer, Node::Table(table));
                }
            }
            "picture" => out.push(parse_picture(el)),
            "page_break" => out.push(Node::PageBreak),
            // A bare inline run at body level (an unwrapped inline group):
            // each styled element deserializes into its own text item; the
            // plain text nodes between them are lost in docling's round-trip.
            "bold" | "italic" | "strikethrough" | "underline" | "subscript" | "superscript" => {
                let mut tmp = Inline::default();
                let mut parts = Vec::new();
                collect_inline_single(el, &mut parts, &mut tmp);
                let text = join_inline(parts);
                if !text.is_empty() {
                    out.push(Node::Paragraph { text });
                }
            }
            "field_region" => {
                // The deserialized tree keeps the region and each `<field_item>`
                // as textless containers docling's Markdown renders as
                // `<!-- missing-text -->` placeholders, followed by the item's
                // key and value texts; the item marker is dropped.
                out.push(Node::Paragraph {
                    text: "<!-- missing-text -->".to_string(),
                });
                for item in el.children().filter(|c| c.has_tag_name("field_item")) {
                    out.push(Node::Paragraph {
                        text: "<!-- missing-text -->".to_string(),
                    });
                    for part in ["key", "value"] {
                        if let Some(text) = field_part(item, part) {
                            out.push(Node::Paragraph { text });
                        }
                    }
                }
            }
            // Unknown block containers: recurse so nothing inside is lost.
            _ => walk_body(el, out),
        }
    }
}

/// A non-body `<layer>` head wraps the node in furniture (Markdown/JSON skip
/// it; the DocLang re-export keeps the layer token).
fn push_with_layer(out: &mut Vec<Node>, layer: Option<ContentLayer>, node: Node) {
    match layer {
        Some(layer) => out.push(Node::Furniture {
            layer,
            inner: Box::new(node),
        }),
        None => out.push(node),
    }
}

fn field_part(item: XmlNode, name: &str) -> Option<String> {
    item.children()
        .find(|c| c.has_tag_name(name))
        .map(flatten_text)
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Inline content.
// ---------------------------------------------------------------------------

/// Parsed inline content of a text-like element: flattened Markdown text plus
/// the head tokens (`<href>`, `<layer>`, `<checkbox>`; `<location>` is layout
/// provenance and is dropped).
#[derive(Default)]
struct Inline {
    text: String,
    href: Option<String>,
    layer: Option<ContentLayer>,
    checkbox: Option<bool>,
}

fn parse_inline(el: XmlNode) -> Inline {
    let mut inline = Inline::default();
    let mut parts: Vec<String> = Vec::new();
    collect_inline(el, &mut parts, &mut inline);
    inline.text = join_inline(parts).trim().to_string();
    inline
}

/// Collect an element's inline children into Markdown fragments. Plain text
/// nodes are whitespace-collapsed (pretty-printed indentation is layout, not
/// content); CDATA sections and `<content>` wrappers keep their text verbatim.
fn collect_inline(el: XmlNode, parts: &mut Vec<String>, inline: &mut Inline) {
    for child in el.children() {
        match child.node_type() {
            NodeType::Text => {
                let raw = child.text().unwrap_or("");
                if is_cdata(child) {
                    if !raw.is_empty() {
                        parts.push(raw.to_string());
                    }
                } else {
                    // Per-line indentation is pretty-printer layout; the line
                    // breaks *inside* one text node are content (a multi-line
                    // inline group keeps them through docling's round-trip).
                    let kept = raw
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !kept.is_empty() {
                        parts.push(kept);
                    }
                }
            }
            NodeType::Element => match child.tag_name().name() {
                "content" => {
                    // Whitespace-significant wrapper: verbatim inner text.
                    let mut s = String::new();
                    for t in child.children().filter(|n| n.node_type() == NodeType::Text) {
                        s.push_str(t.text().unwrap_or(""));
                    }
                    parts.push(s);
                }
                "bold" => parts.push(wrap_md(child, "**")),
                "italic" => parts.push(wrap_md(child, "*")),
                "strikethrough" => parts.push(wrap_md(child, "~~")),
                // No Markdown marker — the content flattens to plain text.
                "underline" | "subscript" | "superscript" => parts.push(inline_md(child)),
                "code" => parts.push(format!("`{}`", flatten_text(child))),
                // docling's inline formula serializes space-padded (" $eq$ "),
                // which stacks with a neighbour's own boundary space into the
                // double-space the reference shows.
                "formula" => parts.push(format!(" ${}$ ", raw_text(child))),
                "href" => inline.href = attr(child, "uri").map(str::to_string),
                "layer" => inline.layer = parse_layer(child),
                "checkbox" => inline.checkbox = Some(attr(child, "class") == Some("selected")),
                "location" => {}
                // Unknown inline element: keep its text.
                _ => parts.push(inline_md(child)),
            },
            _ => {}
        }
    }
}

/// An inline element's Markdown (nested markers preserved, head tokens ignored).
fn inline_md(el: XmlNode) -> String {
    let mut sub = Inline::default();
    let mut parts = Vec::new();
    collect_inline(el, &mut parts, &mut sub);
    join_inline(parts)
}

/// Wrap an element's inline Markdown in a marker pair (empty content collapses
/// to nothing rather than a bare marker pair).
fn wrap_md(el: XmlNode, marker: &str) -> String {
    let inner = inline_md(el);
    if inner.is_empty() {
        String::new()
    } else {
        format!("{marker}{inner}{marker}")
    }
}

/// Join inline fragments with single spaces — the deserializer reads each
/// pretty-printed line as one text node, and sibling nodes are separated by
/// the layout newline the emitter wrote, which renders as a space.
fn join_inline(parts: Vec<String>) -> String {
    let mut s = String::new();
    for part in parts {
        if part.is_empty() {
            continue;
        }
        if !s.is_empty() && !s.ends_with(' ') && !part.starts_with(' ') {
            s.push(' ');
        }
        s.push_str(&part);
    }
    // No trim here: iterative assembly relies on boundary spaces surviving
    // between rounds (a `<content>` trailing space + a formula's leading space
    // is the double space the reference shows). Consumers trim once at the end.
    s
}

fn parse_layer(el: XmlNode) -> Option<ContentLayer> {
    match attr(el, "value") {
        Some("furniture") => Some(ContentLayer::Furniture),
        Some("notes") => Some(ContentLayer::Notes),
        Some("invisible") => Some(ContentLayer::Invisible),
        _ => None,
    }
}

/// An element's direct text, verbatim except the outer trim — formulas keep
/// their internal spacing exactly as serialized.
fn raw_text(el: XmlNode) -> String {
    let mut s = String::new();
    for n in el.children().filter(|n| n.node_type() == NodeType::Text) {
        s.push_str(n.text().unwrap_or(""));
    }
    s.trim().to_string()
}

/// All text under an element, whitespace-collapsed except CDATA (verbatim).
fn flatten_text(el: XmlNode) -> String {
    let mut parts = Vec::new();
    for n in el.descendants().filter(|n| n.node_type() == NodeType::Text) {
        let raw = n.text().unwrap_or("");
        if is_cdata(n) {
            parts.push(raw.to_string());
        } else {
            let collapsed = collapse_ws(raw);
            if !collapsed.is_empty() {
                parts.push(collapsed);
            }
        }
    }
    join_inline(parts)
}

/// A code block's text: CDATA verbatim; plain text trimmed of the layout
/// indentation the pretty-printer added.
fn code_text(el: XmlNode) -> String {
    let mut s = String::new();
    for n in el.children().filter(|n| n.node_type() == NodeType::Text) {
        let raw = n.text().unwrap_or("");
        if is_cdata(n) {
            s.push_str(raw);
        } else if !raw.trim().is_empty() {
            s.push_str(raw.trim());
        }
    }
    s
}

/// Whether a text node came from a CDATA section (roxmltree keeps the source
/// range, so the original bytes distinguish `<![CDATA[…]]>` from plain text).
fn is_cdata(n: XmlNode) -> bool {
    let range = n.range();
    let doc_text = n.document().input_text();
    doc_text[range].starts_with("<![CDATA[")
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Drop the Markdown emphasis markers a `<text>`-wrapped list-item body picked
/// up from its styled runs (docling's list items carry plain text only).
fn strip_md_markers(s: &str) -> String {
    s.replace("**", "").replace(['*'], "").replace("~~", "")
}

/// Whether a `<text>` element holds an inline group — two or more content
/// fragments (styled runs / plain text nodes / formulas), as opposed to one
/// plain or uniformly-styled run.
fn is_inline_group(el: XmlNode) -> bool {
    let mut fragments = 0;
    for child in el.children() {
        match child.node_type() {
            NodeType::Element => match child.tag_name().name() {
                "layer" | "location" | "href" | "checkbox" => {}
                _ => fragments += 1,
            },
            NodeType::Text if !child.text().unwrap_or("").trim().is_empty() => {
                fragments += 1;
            }
            _ => {}
        }
    }
    fragments >= 2
}

/// Whether a `<text>` wrap holds exactly one styled run and no plain text —
/// the shape whose formatting docling's list-item round-trip drops.
fn single_styled_wrap(el: XmlNode) -> bool {
    let mut elements = 0;
    for child in el.children() {
        match child.node_type() {
            NodeType::Element => match child.tag_name().name() {
                "bold" | "italic" | "underline" | "strikethrough" | "subscript" | "superscript" => {
                    elements += 1
                }
                "layer" | "location" | "href" => {}
                _ => return false,
            },
            NodeType::Text if !child.text().unwrap_or("").trim().is_empty() => {
                return false;
            }
            _ => {}
        }
    }
    elements == 1
}

fn attr<'a>(node: XmlNode<'a, '_>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|a| a.name() == name)
        .map(|a| a.value())
}

// ---------------------------------------------------------------------------
// Lists.
// ---------------------------------------------------------------------------

/// Parse a `<list>`: `<ldiv/>` (or `<ldiv><marker>…</marker></ldiv>`) opens an
/// item, the siblings up to the next delimiter are its content, and a nested
/// `<list>` deepens the level. Items re-enter the flat [`Node::ListItem`]
/// stream the serializer emits from.
fn parse_list(el: XmlNode, level: u8, out: &mut Vec<Node>) {
    let ordered = attr(el, "class") == Some("ordered");
    let mut number: u64 = 0;
    let mut first = true;
    // The current item's pending state; content may arrive in several sibling
    // nodes (head tokens, then text), flushed when the next item/list starts.
    let mut pending: Option<(Inline, Option<String>)> = None;

    let flush = |pending: &mut Option<(Inline, Option<String>)>,
                 out: &mut Vec<Node>,
                 number: &mut u64,
                 first: &mut bool| {
        if let Some((inline, marker)) = pending.take() {
            *number += 1;
            let n = marker
                .as_deref()
                .and_then(|m| {
                    m.trim_end_matches(['.', ')'])
                        .rsplit(['.', ')'])
                        .next()
                        .and_then(|s| s.trim().parse::<u64>().ok())
                })
                .unwrap_or(*number);
            let text = match &inline.href {
                Some(uri) if !inline.text.trim().is_empty() => {
                    format!("[{}]({uri})", inline.text.trim())
                }
                _ => inline.text.trim().to_string(),
            };
            if text.is_empty() {
                return;
            }
            out.push(Node::ListItem {
                ordered,
                number: n,
                first_in_list: std::mem::take(first),
                text,
                level,
                marker: marker.filter(|_| ordered),
                location: None,
                dclx: None,
                href: None,
                layer: inline.layer,
            });
        }
    };

    for child in el.children() {
        match child.node_type() {
            NodeType::Element => match child.tag_name().name() {
                "ldiv" => {
                    flush(&mut pending, out, &mut number, &mut first);
                    let marker = child
                        .children()
                        .find(|c| c.has_tag_name("marker"))
                        .map(flatten_text);
                    pending = Some((Inline::default(), marker));
                }
                "list" => {
                    flush(&mut pending, out, &mut number, &mut first);
                    parse_list(child, level + 1, out);
                }
                // A `<text>`-wrapped item body (the segment-sibling wrap) or an
                // inline element of the current bare item. docling's
                // deserializer keeps a list item's plain text only — styled
                // runs flatten without their Markdown markers.
                other => {
                    if let Some((inline, _)) = pending.as_mut() {
                        let mut parts = vec![std::mem::take(&mut inline.text)];
                        match other {
                            "text" => {
                                let sub = parse_inline(child);
                                if inline.href.is_none() {
                                    inline.href = sub.href;
                                }
                                if inline.layer.is_none() {
                                    inline.layer = sub.layer;
                                }
                                // A wrap holding ONE styled run comes back as
                                // the item's plain text (docling reads the
                                // wrapped item's text without its formatting);
                                // a mixed wrap keeps its inline markers.
                                if single_styled_wrap(child) {
                                    parts.push(strip_md_markers(&sub.text));
                                } else {
                                    parts.push(sub.text);
                                }
                            }
                            // A bare styled run keeps its Markdown markers —
                            // only the `<text>` wrap flattens to plain text.
                            _ => {
                                let mut tmp = Vec::new();
                                collect_inline_single(child, &mut tmp, inline);
                                parts.extend(tmp);
                            }
                        }
                        inline.text = join_inline(parts);
                    }
                }
            },
            NodeType::Text => {
                if let Some((inline, _)) = pending.as_mut() {
                    let raw = child.text().unwrap_or("");
                    let frag = if is_cdata(child) {
                        raw.to_string()
                    } else {
                        collapse_ws(raw)
                    };
                    if !frag.is_empty() {
                        let joined = join_inline(vec![std::mem::take(&mut inline.text), frag]);
                        inline.text = joined;
                    }
                }
            }
            _ => {}
        }
    }
    flush(&mut pending, out, &mut number, &mut first);
}

/// Collect one inline element (not its siblings) into fragments.
fn collect_inline_single(el: XmlNode, parts: &mut Vec<String>, inline: &mut Inline) {
    match el.tag_name().name() {
        "bold" => parts.push(wrap_md(el, "**")),
        "italic" => parts.push(wrap_md(el, "*")),
        "strikethrough" => parts.push(wrap_md(el, "~~")),
        "underline" | "subscript" | "superscript" => parts.push(inline_md(el)),
        "code" => parts.push(format!("`{}`", flatten_text(el))),
        // Leading space per docling's inline formula serialization.
        "formula" => parts.push(format!(" ${}$ ", raw_text(el))),
        "href" => inline.href = attr(el, "uri").map(str::to_string),
        "layer" => inline.layer = parse_layer(el),
        "location" => {}
        "content" => {
            let mut s = String::new();
            for t in el.children().filter(|n| n.node_type() == NodeType::Text) {
                s.push_str(t.text().unwrap_or(""));
            }
            parts.push(s);
        }
        _ => parts.push(inline_md(el)),
    }
}

// ---------------------------------------------------------------------------
// Tables and pictures.
// ---------------------------------------------------------------------------

/// One parsed OTSL cell.
#[derive(Clone, Copy, PartialEq)]
enum CellKind {
    Filled,
    ColHeader,
    RowHeader,
    Empty,
    SpanLeft,
    SpanUp,
    SpanCross,
}

/// Parse a `<table>` (or a chart's `<tabular>`): a stream of OTSL cell tokens,
/// each followed by its content until the next token; `<nl/>` closes a row.
fn parse_table(el: XmlNode) -> Option<Table> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut kinds: Vec<Vec<CellKind>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut kind_row: Vec<CellKind> = Vec::new();
    let mut cell: Option<(CellKind, Inline)> = None;

    let close_cell = |cell: &mut Option<(CellKind, Inline)>,
                      row: &mut Vec<String>,
                      kind_row: &mut Vec<CellKind>| {
        if let Some((kind, inline)) = cell.take() {
            row.push(inline.text.trim().to_string());
            kind_row.push(kind);
        }
    };

    for child in el.children() {
        match child.node_type() {
            NodeType::Element => {
                let name = child.tag_name().name();
                let kind = match name {
                    "fcel" => Some(CellKind::Filled),
                    "ched" => Some(CellKind::ColHeader),
                    "rhed" => Some(CellKind::RowHeader),
                    "ecel" => Some(CellKind::Empty),
                    "lcel" => Some(CellKind::SpanLeft),
                    "ucel" => Some(CellKind::SpanUp),
                    "xcel" => Some(CellKind::SpanCross),
                    _ => None,
                };
                if let Some(kind) = kind {
                    close_cell(&mut cell, &mut row, &mut kind_row);
                    cell = Some((kind, Inline::default()));
                    continue;
                }
                match name {
                    "nl" => {
                        close_cell(&mut cell, &mut row, &mut kind_row);
                        if !row.is_empty() {
                            rows.push(std::mem::take(&mut row));
                            kinds.push(std::mem::take(&mut kind_row));
                        }
                    }
                    "location" => {}
                    // Cell content. Rich-cell block segments — `<text>` blocks
                    // (a checkbox segment renders its `- [x]` marker), lists,
                    // and nested tables — join with the double space a
                    // flattened blank line leaves; a plain cell's own inline
                    // elements join with single spaces.
                    other => {
                        if let Some((_, inline)) = cell.as_mut() {
                            let block = |inline: &mut Inline, seg: String| {
                                if !seg.is_empty() {
                                    if !inline.text.is_empty() {
                                        inline.text.push_str("  ");
                                    }
                                    inline.text.push_str(&seg);
                                }
                            };
                            match other {
                                "text" => {
                                    let sub = parse_inline(child);
                                    let seg = match sub.checkbox {
                                        Some(true) => format!("- [x] {}", sub.text),
                                        Some(false) => format!("- [ ] {}", sub.text),
                                        None => sub.text,
                                    };
                                    block(inline, seg);
                                }
                                "list" => block(inline, flatten_cell_list(child)),
                                "picture" => block(inline, "<!-- image -->".to_string()),
                                // A nested table flattens to its cells' plain
                                // text (docling's `_collect_subtree_text`).
                                "table" => {
                                    if let Some(t) = parse_table(child) {
                                        let seg = t
                                            .rows
                                            .iter()
                                            .flatten()
                                            .filter(|c| !c.is_empty())
                                            .map(|c| strip_md_markers(c))
                                            .collect::<Vec<_>>()
                                            .join(" ");
                                        block(inline, seg);
                                    }
                                }
                                _ => {
                                    let mut parts = Vec::new();
                                    collect_inline_single(child, &mut parts, inline);
                                    let frag = join_inline(parts);
                                    let joined =
                                        join_inline(vec![std::mem::take(&mut inline.text), frag]);
                                    inline.text = joined;
                                }
                            }
                        }
                    }
                }
            }
            NodeType::Text => {
                if let Some((_, inline)) = cell.as_mut() {
                    let raw = child.text().unwrap_or("");
                    let frag = if is_cdata(child) {
                        raw.to_string()
                    } else {
                        collapse_ws(raw)
                    };
                    if !frag.is_empty() {
                        let joined = join_inline(vec![std::mem::take(&mut inline.text), frag]);
                        inline.text = joined;
                    }
                }
            }
            _ => {}
        }
    }
    close_cell(&mut cell, &mut row, &mut kind_row);
    if !row.is_empty() {
        rows.push(row);
        kinds.push(kind_row);
    }
    if rows.is_empty() {
        return None;
    }

    let num_cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    for r in &mut rows {
        r.resize(num_cols, String::new());
    }
    let pad = |k: &Vec<CellKind>| {
        let mut v = k.clone();
        v.resize(num_cols, CellKind::Empty);
        v
    };
    // docling's deserializer expands each span back into a full-rectangle
    // `TableCell`, whose text repeats at every covered grid position — fill the
    // continuation cells from their anchor.
    for ri in 0..rows.len() {
        for ci in 0..num_cols {
            let kind = kinds[ri].get(ci).copied().unwrap_or(CellKind::Empty);
            match kind {
                CellKind::SpanLeft if ci > 0 => rows[ri][ci] = rows[ri][ci - 1].clone(),
                CellKind::SpanUp if ri > 0 => rows[ri][ci] = rows[ri - 1][ci].clone(),
                CellKind::SpanCross if ri > 0 && ci > 0 => {
                    rows[ri][ci] = rows[ri - 1][ci].clone();
                }
                _ => {}
            }
        }
    }
    let header_row = kinds
        .iter()
        .map(|k| k.contains(&CellKind::ColHeader))
        .collect();
    let col_continuation = kinds
        .iter()
        .map(|k| {
            pad(k)
                .iter()
                .map(|c| matches!(c, CellKind::SpanLeft | CellKind::SpanCross))
                .collect()
        })
        .collect();
    let row_continuation = kinds
        .iter()
        .map(|k| {
            pad(k)
                .iter()
                .map(|c| matches!(c, CellKind::SpanUp | CellKind::SpanCross))
                .collect()
        })
        .collect();
    let row_header = kinds
        .iter()
        .map(|k| pad(k).iter().map(|c| *c == CellKind::RowHeader).collect())
        .collect();
    Some(Table {
        rows,
        location: None,
        structure: Some(TableStructure {
            header_row,
            col_continuation,
            row_continuation,
            row_header,
        }),
        cell_blocks: None,
    })
}

/// Flatten a list inside a table cell: each item renders inline as
/// `- text` / `N. text`, items joined with single spaces (the cell flatten
/// turns the item newlines into spaces). Nested lists continue the run.
fn flatten_cell_list(el: XmlNode) -> String {
    let ordered = attr(el, "class") == Some("ordered");
    let mut items: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    let mut number = 0u64;
    let flush = |current: &mut Option<String>, items: &mut Vec<String>, number: &mut u64| {
        if let Some(text) = current.take() {
            let text = text.trim().to_string();
            if !text.is_empty() {
                *number += 1;
                if ordered {
                    items.push(format!("{number}. {text}"));
                } else {
                    items.push(format!("- {text}"));
                }
            }
        }
    };
    for child in el.children() {
        match child.node_type() {
            NodeType::Element => match child.tag_name().name() {
                "ldiv" => {
                    flush(&mut current, &mut items, &mut number);
                    current = Some(String::new());
                }
                "list" => {
                    flush(&mut current, &mut items, &mut number);
                    let nested = flatten_cell_list(child);
                    if !nested.is_empty() {
                        items.push(nested);
                    }
                }
                "text" => {
                    if let Some(cur) = current.as_mut() {
                        let frag = parse_inline(child).text;
                        *cur = join_inline(vec![std::mem::take(cur), frag]);
                    }
                }
                "marker" => {}
                _ => {
                    if let Some(cur) = current.as_mut() {
                        let mut tmp = Inline::default();
                        let mut parts = Vec::new();
                        collect_inline_single(child, &mut parts, &mut tmp);
                        let frag = join_inline(parts);
                        *cur = join_inline(vec![std::mem::take(cur), frag]);
                    }
                }
            },
            NodeType::Text => {
                if let Some(cur) = current.as_mut() {
                    let raw = child.text().unwrap_or("");
                    let frag = if is_cdata(child) {
                        raw.to_string()
                    } else {
                        collapse_ws(raw)
                    };
                    if !frag.is_empty() {
                        *cur = join_inline(vec![std::mem::take(cur), frag]);
                    }
                }
            }
            _ => {}
        }
    }
    flush(&mut current, &mut items, &mut number);
    items.join(" ")
}

/// Parse a `<picture>`: a chart (`class="chart"`, with its `<label>` kind and
/// `<tabular>` data grid) maps back to [`Node::Chart`]; anything else becomes a
/// plain picture with its caption (the `<src>` payload is not re-imported).
fn parse_picture(el: XmlNode) -> Node {
    let caption = el
        .children()
        .find(|c| c.has_tag_name("caption"))
        .map(|c| parse_inline(c).text)
        .filter(|s| !s.is_empty());
    if attr(el, "class") == Some("chart") {
        let kind = el
            .children()
            .find(|c| c.has_tag_name("label"))
            .and_then(|l| attr(l, "value"))
            .unwrap_or("chart")
            .to_string();
        if let Some(table) = el
            .children()
            .find(|c| c.has_tag_name("tabular"))
            .and_then(parse_table)
        {
            return Node::Chart {
                kind,
                table,
                caption,
                location: None,
            };
        }
    }
    let layer = el
        .children()
        .find(|c| c.has_tag_name("layer"))
        .and_then(parse_layer);
    let picture = Node::Picture {
        caption,
        image: None,
    };
    match layer {
        Some(layer) => Node::Furniture {
            layer,
            inner: Box::new(picture),
        },
        None => picture,
    }
}
