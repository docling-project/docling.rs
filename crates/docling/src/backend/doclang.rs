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
        // The archive stays open for the tree walk: a `<src uri>` names one
        // of its assets (docling's `media_root`).
        let mut pkg: Option<Package> = None;
        let xml: String = if source.format == InputFormat::Dclx {
            let p = pkg.insert(
                Package::open(&source.bytes)
                    .ok_or_else(|| ConversionError::Parse("dclx: not a zip archive".into()))?,
            );
            p.read("document.xml")
                .ok_or_else(|| ConversionError::Parse("dclx: no document.xml".into()))?
        } else {
            source.text()?.to_string()
        };
        super::xml_depth::check(&xml, "doclang")?;
        let dom = Document::parse(&xml).map_err(|e| ConversionError::with_source("doclang", e))?;
        let root = dom.root_element();
        if root.tag_name().name() != "doclang" {
            return Err(ConversionError::Parse(format!(
                "doclang: unexpected root element <{}>",
                root.tag_name().name()
            )));
        }
        check_table_budget(root)?;
        let mut doc = DoclingDocument::new(&source.name);
        walk_body(root, &mut doc.nodes);
        // The JSON export reads docling's item tree (see `doclang_tree`);
        // the deserializer lays every document out on 512×512 pages, one
        // per `<page_break>`, which the export's page map is built from.
        let (mut tree, pages) = super::doclang_tree::build(root, pkg.as_mut());
        // DocLang's reading rules give every run its own whitespace — the
        // runs schema 1.11.0 expects (#997).
        tree.inline_whitespace = true;
        doc.tree = Some(tree);
        for page_no in 1..=pages {
            doc.push(Node::PageInfo {
                page_no,
                width: super::doclang_tree::PAGE_SIZE,
                height: super::doclang_tree::PAGE_SIZE,
            });
        }
        Ok(doc)
    }
}

// ---------------------------------------------------------------------------
// Block-level walk.
// ---------------------------------------------------------------------------

/// Every OTSL table of the document against the cell budget (#861) before
/// either walk materializes a grid: the dense `rows` here and the
/// `num_rows × num_cols` grid of `doclang_tree` are `rows × the widest
/// row`, so one row of a million tokens plus a million rows of one — a few
/// megabytes of XML — would ask for 10^12 slots. Nested tables (a rich
/// cell's) are tables of their own, counted on their own.
fn check_table_budget(root: XmlNode) -> Result<(), ConversionError> {
    fn dims(el: XmlNode, rows: &mut usize, cols: &mut usize, cur: &mut usize) {
        for child in el.children().filter(|n| n.is_element()) {
            match child.tag_name().name() {
                "nl" => {
                    if *cur > 0 {
                        *rows += 1;
                        *cols = (*cols).max(*cur);
                        *cur = 0;
                    }
                }
                "fcel" | "ched" | "rhed" | "ecel" | "lcel" | "ucel" | "xcel" | "srow" | "corn" => {
                    *cur += 1
                }
                "table" | "tabular" | "index" => {}
                // A `<content>` wrapper or a cell's content: tokens may sit
                // below it (the tree walk's `otsl_parts`).
                _ => dims(child, rows, cols, cur),
            }
        }
    }
    let mut budget = docling_core::limits::TableCellBudget::new();
    for el in root
        .descendants()
        .filter(|n| n.is_element() && matches!(n.tag_name().name(), "table" | "tabular" | "index"))
    {
        let (mut rows, mut cols, mut cur) = (0usize, 0usize, 0usize);
        dims(el, &mut rows, &mut cols, &mut cur);
        if cur > 0 {
            rows += 1;
            cols = cols.max(cur);
        }
        budget
            .table(&format!("doclang <{}>", el.tag_name().name()), rows, cols)
            .map_err(|e| ConversionError::with_source("doclang", e))?;
    }
    Ok(())
}

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
                // A mixed `<text>` keeps its `<layer>`: docling-core 2.102
                // puts the inline group and its runs on the element's layer
                // (docling-core#865; it used to leave the group on the body).
                let inline = parse_inline(el);
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
            // docling-core#865: a footnote, a page header/footer and a lone
            // caption are text items of their own label — a mixed one a host
            // over its inline group — which docling's Markdown prints like
            // body text (a furniture-layer one not at all).
            "footnote" | "page_header" | "page_footer" | "caption" => {
                let inline = parse_inline(el);
                if !inline.text.trim().is_empty() {
                    push_with_layer(
                        out,
                        inline.layer,
                        Node::Paragraph {
                            text: inline.text.trim().to_string(),
                        },
                    );
                }
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
                    orig: None,
                    pretty: None,
                });
            }
            "list" => parse_list(el, 0, out),
            "table" => {
                let layer = el
                    .children()
                    .find(|c| c.has_tag_name("layer"))
                    .and_then(parse_layer);
                if let Some(mut table) = parse_table(el) {
                    // The caption, its runs kept as inline Markdown
                    // (`Table **1** : results`, docling-core#865) — it used to
                    // be dropped.
                    table.caption = el
                        .children()
                        .find(|c| c.has_tag_name("caption"))
                        .map(|c| parse_inline(c).text.trim().to_string())
                        .filter(|s| !s.is_empty());
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
                // as textless containers: docling-core's Markdown fallback
                // serializes them to nothing since 2.93 (docling-core#724 — it
                // used to write a `<!-- missing-text -->` placeholder for
                // each), so only the item's key and value texts appear; the
                // item marker is dropped.
                for item in el.children().filter(|c| c.has_tag_name("field_item")) {
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
                    // DocLang 0.9's reading rule (docling-core#693): a
                    // whitespace sequence reads as one space — a run's edge
                    // whitespace is its boundary — and none at the block's
                    // edges (`parse_inline` trims). Per-line indentation is
                    // pretty-printer layout; the line breaks *inside* one
                    // text node are content (a multi-line inline group keeps
                    // them through docling's round-trip).
                    let kept = raw
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n");
                    let lead = raw.starts_with(|c: char| c.is_ascii_whitespace());
                    let trail = raw.ends_with(|c: char| c.is_ascii_whitespace());
                    if kept.is_empty() {
                        if !raw.is_empty() {
                            parts.push(" ".to_string());
                        }
                    } else {
                        parts.push(format!(
                            "{}{kept}{}",
                            if lead { " " } else { "" },
                            if trail { " " } else { "" }
                        ));
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
                "code" => parts.push(docling_core::code_span(&flatten_text(child))),
                // An inline formula run: its neighbours carry the spaces
                // (docling-core 2.106's faithful join, #997).
                "formula" => parts.push(format!("${}$", raw_text(child))),
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
        // Edge whitespace goes outside the markers (#997).
        docling_core::inline::hoist(&inner, |core| format!("{marker}{core}{marker}"))
    }
}

/// Join inline fragments as they are: each carries its own boundary
/// whitespace (a pretty-printed layout newline reads as a space).
fn join_inline(parts: Vec<String>) -> String {
    let mut s = String::new();
    for part in parts {
        if part.is_empty() {
            continue;
        }
        // The runs carry their own boundary whitespace (DocLang 0.9,
        // schema 1.11.0, #997): concatenate, one space where both sides
        // bring one.
        if s.ends_with(' ') && part.starts_with(' ') {
            s.push_str(&part[1..]);
        } else {
            s.push_str(&part);
        }
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
            let collapsed = collapse_ws_edges(raw);
            if !collapsed.is_empty() {
                parts.push(collapsed);
            }
        }
    }
    join_inline(parts).trim().to_string()
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

/// [`collapse_ws`] keeping one space for edge whitespace — a text node's
/// boundary with its neighbours (DocLang 0.9, #997); a whitespace-only node
/// is that one space.
fn collapse_ws_edges(s: &str) -> String {
    let core = collapse_ws(s);
    if core.is_empty() {
        return if s.is_empty() {
            String::new()
        } else {
            " ".into()
        };
    }
    let lead = if s.starts_with(char::is_whitespace) {
        " "
    } else {
        ""
    };
    let trail = if s.ends_with(char::is_whitespace) {
        " "
    } else {
        ""
    };
    format!("{lead}{core}{trail}")
}

/// Drop the Markdown emphasis markers a `<text>`-wrapped list-item body picked
/// up from its styled runs (docling's list items carry plain text only).
fn strip_md_markers(s: &str) -> String {
    s.replace("**", "").replace(['*'], "").replace("~~", "")
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
                    docling_core::hyperlink::markdown_link(inline.text.trim(), uri)
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
                        collapse_ws_edges(raw)
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
        "code" => parts.push(docling_core::code_span(&flatten_text(el))),
        // An inline formula run: its neighbours carry the spaces (#997).
        "formula" => parts.push(format!("${}$", raw_text(el))),
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
                                let seg = seg.trim();
                                if !seg.is_empty() {
                                    // The text before keeps its boundary
                                    // space (#997); the block separator
                                    // replaces it.
                                    let kept = inline.text.trim_end().len();
                                    inline.text.truncate(kept);
                                    if !inline.text.is_empty() {
                                        inline.text.push_str("  ");
                                    }
                                    inline.text.push_str(seg);
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
                                    // docling-core serializes the cell's text
                                    // item as Markdown first — a line break
                                    // inside it becomes a GFM hard break
                                    // (`"  \n"`) — and only then flattens the
                                    // cell's newlines to spaces, so a
                                    // `<content>` newline is three spaces in
                                    // the table (`Rich cell   A nested table`).
                                    block(inline, hard_breaks(&seg));
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
                        collapse_ws_edges(raw)
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
            col_header: Vec::new(),
        }),
        cell_blocks: None,
        cells: None,
        caption: None,
        caption_parent: Default::default(),
        caption_location: None,
    })
}

/// docling-core's `_get_text_with_line_breaks` on a paragraph: a single
/// newline is a GFM hard break (two trailing spaces), a blank line stays.
fn hard_breaks(text: &str) -> String {
    if !text.contains('\n') {
        return text.to_string();
    }
    text.split("\n\n")
        .map(|para| para.replace('\n', "  \n"))
        .collect::<Vec<_>>()
        .join("\n\n")
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
                        collapse_ws_edges(raw)
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
        caption_href: None,
        image: None,
        classification: None,
        description: None,
        caption_parent: Default::default(),
        caption_location: None,
    };
    match layer {
        Some(layer) => Node::Furniture {
            layer,
            inner: Box::new(picture),
        },
        None => picture,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::DeclarativeBackend;
    use crate::{InputFormat, SourceDocument};

    fn md(dclg: &str) -> String {
        let src =
            SourceDocument::from_bytes("t.dclg", InputFormat::XmlDoclang, dclg.as_bytes().to_vec());
        DoclangBackend.convert(&src).unwrap().export_to_markdown()
    }

    /// #861: one row of 1001 tokens and 1000 rows of one token ask for a
    /// 1001 × 1001 grid — past the per-table cap — from two thousand
    /// tokens; the document is refused before either walk builds it. The
    /// same tokens as one wide row alone fit.
    #[test]
    fn oversized_otsl_table_is_rejected_with_a_typed_limit() {
        let wide = "<fcel/>".repeat(1001);
        let tall = "<fcel/><nl/>".repeat(1000);
        let dclg = |table: &str| format!("<doclang><table>{table}</table></doclang>");
        let convert = |dclg: String| {
            DoclangBackend.convert(&SourceDocument::from_bytes(
                "t.dclg",
                InputFormat::XmlDoclang,
                dclg.into_bytes(),
            ))
        };
        let err = convert(dclg(&format!("{wide}<nl/>{tall}"))).unwrap_err();
        let limit = err.limit().expect("typed limit");
        assert_eq!(limit.variable, "DOCLING_RS_MAX_TABLE_CELLS");
        assert_eq!(limit.actual, 1001 * 1001);
        assert!(convert(dclg(&wide)).is_ok());
        // Tokens under a `<content>` wrapper count the same.
        let err = convert(dclg(&format!("<content>{wide}<nl/>{tall}</content>"))).unwrap_err();
        assert!(err.limit().is_some(), "{err}");
    }

    fn json_of(dclg: &str) -> serde_json::Value {
        let src =
            SourceDocument::from_bytes("t.dclg", InputFormat::XmlDoclang, dclg.as_bytes().to_vec());
        serde_json::from_str(&DoclangBackend.convert(&src).unwrap().export_to_json()).unwrap()
    }

    /// The JSON is docling-core's `DocLangDocDeserializer` tree: a mixed
    /// `<text>` is an `inline` group of per-run items (an empty `<text/>` an
    /// empty text item), a `<location>` quartet is the item's `prov` on the 512
    /// page grid with `charspan = (0, len)`, a `<page_break>` opens another
    /// 512×512 page, list items carry `marker: ""` unless the `<ldiv>` says
    /// otherwise, and a CDATA code body is read the way minidom splits it —
    /// the pretty-print whitespace around it is not part of the code.
    #[test]
    fn tree_follows_the_docling_deserializer() {
        let json = json_of(
            r#"<doclang version="0.7">
  <heading level="2">
    <location value="10"/><location value="20"/><location value="30"/><location value="40"/>
    Title
  </heading>
  <text>plain <bold>strong</bold> tail</text>
  <text/>
  <list class="ordered">
    <ldiv><marker>a)</marker></ldiv>
    virtual <italic>text</italic>
    <ldiv/>
    <text>wrapped</text>
    <list><ldiv/>nested</list>
  </list>
  <page_break/>
  <code>
    <label value="Python"/>
<![CDATA[print("hi")]]>  </code>
</doclang>"#,
        );
        let texts = json["texts"].as_array().unwrap();
        let groups = json["groups"].as_array().unwrap();
        assert_eq!(texts[0]["label"], "section_header");
        assert_eq!(texts[0]["level"], 1);
        assert_eq!(texts[0]["prov"][0]["charspan"], serde_json::json!([0, 5]));
        assert_eq!(texts[0]["prov"][0]["bbox"]["r"], 30.0);
        // `plain <bold>strong</bold> tail` → inline group of three items.
        assert_eq!(groups[0]["label"], "inline");
        assert_eq!(groups[0]["name"], "group");
        assert_eq!(groups[0]["children"].as_array().unwrap().len(), 3);
        assert_eq!(texts[2]["text"], "strong");
        assert_eq!(texts[2]["formatting"]["bold"], true);
        // The empty `<text/>` is an empty text item (docling-core 2.102's
        // `_is_empty_text_block`), not an empty inline group.
        assert_eq!(texts[4]["label"], "text");
        assert_eq!(texts[4]["text"], "");
        assert_eq!(texts[4]["parent"]["$ref"], "#/body");
        // The list: `a)` marker kept, virtual text with a styled run → an
        // empty item holding an inline group; the wrapped item keeps its
        // text and parents the nested list.
        assert_eq!(groups[1]["label"], "list");
        assert_eq!(groups[1]["name"], "group");
        assert_eq!(texts[5]["label"], "list_item");
        assert_eq!(texts[5]["text"], "");
        assert_eq!(texts[5]["marker"], "a)");
        assert_eq!(texts[5]["enumerated"], true);
        assert_eq!(texts[5]["children"][0]["$ref"], "#/groups/2");
        assert_eq!(texts[8]["text"], "wrapped");
        assert_eq!(texts[8]["marker"], "");
        assert_eq!(texts[8]["children"][0]["$ref"], "#/groups/3");
        assert_eq!(texts[9]["text"], "nested");
        assert_eq!(texts[9]["parent"]["$ref"], "#/groups/3");
        // The code block: label → language, CDATA verbatim, whitespace gone.
        let code = texts.last().unwrap();
        assert_eq!(code["label"], "code");
        assert_eq!(code["text"], "print(\"hi\")");
        assert_eq!(code["code_language"], "Python");
        assert_eq!(json["pages"]["1"]["size"]["width"], 512.0);
        assert_eq!(json["pages"]["2"]["page_no"], 2);
    }

    /// Floats: the caption is a body-level item created before its table or
    /// picture, a rich cell's content hangs off an `unspecified` group under
    /// the table (the cell's `ref`), a `ched` cell is a column header with
    /// its span, and a chart picture carries its label (confidence 1.0) and
    /// `<tabular>` data.
    #[test]
    fn floats_captions_rich_cells_and_charts() {
        let json = json_of(
            r#"<doclang version="0.7">
  <table>
    <caption>Cap</caption>
    <ched/>H<lcel/><nl/>
    <fcel/><text>rich</text><list><ldiv/>item</list><fcel/>plain<nl/>
  </table>
  <picture class="chart">
    <label value="line_chart"/>
    <caption><![CDATA['col-3']]></caption>
    <tabular><ched/>c<nl/><fcel/>1<nl/></tabular>
  </picture>
</doclang>"#,
        );
        let body: Vec<&str> = json["body"]["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["$ref"].as_str().unwrap())
            .collect();
        assert_eq!(
            body,
            ["#/texts/0", "#/tables/0", "#/texts/3", "#/pictures/0"]
        );
        assert_eq!(json["texts"][0]["label"], "caption");
        assert_eq!(json["tables"][0]["captions"][0]["$ref"], "#/texts/0");
        let cells = json["tables"][0]["data"]["table_cells"].as_array().unwrap();
        assert_eq!(cells[0]["column_header"], true);
        assert_eq!(cells[0]["col_span"], 2);
        // `"".join(self._get_text(part) …)`: the parts' texts concatenate.
        assert_eq!(cells[1]["text"], "richitem");
        assert_eq!(cells[1]["ref"]["$ref"], "#/groups/0");
        assert_eq!(json["groups"][0]["label"], "unspecified");
        assert_eq!(json["groups"][0]["parent"]["$ref"], "#/tables/0");
        assert_eq!(json["texts"][1]["parent"]["$ref"], "#/groups/0");
        assert_eq!(json["groups"][1]["label"], "list");
        assert_eq!(json["groups"][1]["parent"]["$ref"], "#/groups/0");
        assert_eq!(cells[2]["text"], "plain");
        assert_eq!(json["texts"][3]["text"], "'col-3'");
        let pic = &json["pictures"][0];
        assert_eq!(pic["captions"][0]["$ref"], "#/texts/3");
        assert_eq!(
            pic["meta"]["classification"]["predictions"][0],
            serde_json::json!({ "confidence": 1.0, "class_name": "line_chart" })
        );
        assert_eq!(pic["meta"]["tabular_chart"]["chart_data"]["num_rows"], 2);
    }

    /// docling-core ≥ 2.93 renders a `<field_region>` and its `<field_item>`s
    /// to nothing (no `<!-- missing-text -->` placeholders); only each item's
    /// key and value survive, the marker is dropped.
    #[test]
    fn field_regions_render_their_keys_and_values_only() {
        let doc = r#"<doclang version="0.7"><text>Form:</text><field_region><field_item><marker>1</marker><key>Server</key><value>Quack</value></field_item></field_region><text>End</text></doclang>"#;
        assert_eq!(
            md(doc),
            "Form:

Server

Quack

End
"
        );
    }

    /// A `<content>` newline inside a rich cell's text item is a GFM hard
    /// break to docling-core's text serializer, and the table flatten then
    /// turns its newline into a space: three spaces in the cell, two between
    /// the cell's blocks (docling 2.129).
    #[test]
    fn rich_cell_line_breaks_flatten_like_docling_core() {
        let doc = r#"<doclang version="0.7"><table><ched/>A<nl/><fcel/><text><content>Rich cell
A nested table</content></text><table><fcel/>x<nl/></table><nl/></table></doclang>"#;
        assert!(
            md(doc).contains("| Rich cell   A nested table  x |"),
            "{}",
            md(doc)
        );
    }
}
