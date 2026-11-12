//! Chandra-OCR-2 HTML-with-bbox parser (issue #322).
//!
//! Chandra emits HTML where each layout element is a top-level
//! `<div data-bbox="x0 y0 x1 y1" data-label="Label">content</div>`, boxes on a
//! 0–1000 normalized grid. This ports docling's `parse_chandra_html`
//! (docling#4092's `<br>` spacing and docling#4135's Form-holding-a-table
//! included), mapped onto our node model: labels become the closest node kind,
//! boxes become [`Node::Located`] provenance rescaled to DocLang's 0–511 grid.
//! Same tolerance contract as the other VLM grammars — hostile model output
//! degrades to text, never to an error.

// Only the VLM pipeline consumes the parser; the detector stays public API in
// every build (the wasm/pdf-text build compiles this module without `vlm`).
#![cfg_attr(not(feature = "vlm"), allow(dead_code))]

use docling_core::{DoclingDocument, Node};
use regex::Regex;
use scraper::{ElementRef, Html};

use crate::backend::markdown::escape_text;

/// True when a VLM response looks like Chandra layout HTML: top-level divs
/// carrying both `data-bbox` and `data-label`.
pub fn looks_like_chandra(text: &str) -> bool {
    text.contains("data-bbox=") && text.contains("data-label=")
}

/// One `<div …>…</div>` block. Chandra's blocks are flat (its own prompt asks
/// for "the simplest possible HTML structure"), so a non-greedy match per
/// block mirrors docling's `_DIV_PATTERN`.
fn div_re() -> &'static Regex {
    cached_regex!(r"(?s)<div\s+([^>]*?)>(.*?)</div>")
}

fn attr_re(name: &str, attrs: &str) -> Option<String> {
    let re = match name {
        "data-bbox" => cached_regex!(r#"data-bbox="(\d+\s+\d+\s+\d+\s+\d+)""#),
        _ => cached_regex!(r#"data-label="([^"]+)""#),
    };
    re.captures(attrs).map(|c| c[1].to_string())
}

/// Parse the pages of a Chandra response run into one document.
pub(crate) fn parse_chandra_pages(name: &str, fragments: &[String]) -> DoclingDocument {
    let mut doc = DoclingDocument::new(name);
    for fragment in fragments {
        parse_fragment(fragment, &mut doc);
    }
    doc
}

fn parse_fragment(content: &str, doc: &mut DoclingDocument) {
    for caps in div_re().captures_iter(content) {
        let attrs = &caps[1];
        let inner = &caps[2];
        let (Some(bbox), Some(label)) = (attr_re("data-bbox", attrs), attr_re("data-label", attrs))
        else {
            continue;
        };
        let location = parse_bbox(&bbox);
        for node in nodes_for(&label, inner) {
            doc.push(located(node, location));
        }
    }
}

/// Chandra's 0–1000 normalized box → DocLang's 0–511 grid.
fn parse_bbox(raw: &str) -> Option<[u16; 4]> {
    let vals: Vec<u16> = raw
        .split_whitespace()
        .filter_map(|v| v.parse::<u32>().ok())
        .map(|v| ((v.min(1000) * 511 + 500) / 1000) as u16)
        .collect();
    vals.try_into().ok()
}

fn located(node: Node, location: Option<[u16; 4]>) -> Node {
    match location {
        Some(location) => Node::Located {
            location,
            inner: Box::new(node),
        },
        None => node,
    }
}

/// docling's `_LABEL_MAP` + per-label emission, on our node model.
fn nodes_for(label: &str, inner: &str) -> Vec<Node> {
    match label {
        // docling#4135: Chandra labels fill-in tables "Form"; when the block
        // actually holds a `<table>`, parse it as one.
        "Table" | "Form" if inner.to_lowercase().contains("<table") => parse_table_html(inner)
            .map(Node::Table)
            .into_iter()
            .collect(),
        "List-Group" => {
            let items = parse_list_html(inner);
            let items = if items.is_empty() {
                vec![strip_tags(inner)]
            } else {
                items
            };
            items
                .into_iter()
                .enumerate()
                .map(|(i, text)| Node::ListItem {
                    ordered: false,
                    number: 0,
                    first_in_list: i == 0,
                    text: escape_text(&text),
                    level: 0,
                    marker: None,
                    location: None,
                    split: None,
                    dclx: None,
                    href: None,
                    layer: None,
                })
                .collect()
        }
        "Figure" | "Image" | "Diagram" => vec![Node::Picture {
            caption: None,
            caption_href: None,
            image: None,
            classification: None,
            description: None,
            caption_parent: Default::default(),
            caption_location: None,
        }],
        "Title" => text_node(inner, |t| Node::Heading { level: 1, text: t }),
        "Section-Header" => text_node(inner, |t| Node::Heading { level: 2, text: t }),
        // Page furniture is excluded from Markdown, like docling's
        // page_header/page_footer labels under the default export set.
        "Page-Header" | "Page-Footer" => text_node(inner, |t| Node::Furniture {
            layer: docling_core::ContentLayer::Furniture,
            inner: Box::new(Node::Paragraph { text: t }),
        }),
        // docling maps these to formula text items ($$…$$ in Markdown).
        "Equation-Block" | "Chemical-Block" => {
            let text = strip_tags(inner);
            if text.is_empty() {
                Vec::new()
            } else {
                vec![Node::Formula {
                    orig: text.clone(),
                    latex: text,
                    location: None,
                }]
            }
        }
        "Code-Block" => {
            let text = strip_tags(inner);
            if text.is_empty() {
                Vec::new()
            } else {
                vec![Node::Code {
                    language: None,
                    text,
                    orig: None,
                    pretty: None,
                }]
            }
        }
        // Text, Caption, Footnote, Table-Of-Contents, Complex-Block,
        // Bibliography, Blank-Page, unknown labels: a paragraph.
        _ => text_node(inner, |t| Node::Paragraph { text: t }),
    }
}

fn text_node(inner: &str, make: impl Fn(String) -> Node) -> Vec<Node> {
    let text = strip_tags(inner);
    if text.is_empty() {
        Vec::new()
    } else {
        vec![make(escape_text(&text))]
    }
}

/// docling's `_strip_tags` with docling#4092's rule: a `<br>` is spacing, so
/// it becomes a space before the tags are dropped and whitespace collapses.
fn strip_tags(html: &str) -> String {
    let spaced = cached_regex!(r"(?i)<br\s*/?>").replace_all(html, " ");
    let text = cached_regex!(r"<[^>]+>").replace_all(&spaced, "");
    let text = html_escape_decode(&text);
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The entities Chandra's plain text realistically carries (scraper decodes
/// the table/list paths; this covers the regex-stripped one).
fn html_escape_decode(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

/// Parse the block's `<table>` through the HTML backend's table machinery
/// (colspan/rowspan replication, `<th>` header flags — the same shape
/// docling's `_parse_table_html` builds).
fn parse_table_html(inner: &str) -> Option<docling_core::Table> {
    let dom = Html::parse_fragment(inner);
    let table = descendant_table(dom.root_element())?;
    crate::backend::html::parse_table(table)
}

fn descendant_table<'a>(root: ElementRef<'a>) -> Option<ElementRef<'a>> {
    root.descendants()
        .filter_map(ElementRef::wrap)
        .find(|e| e.value().name() == "table")
}

/// Each `<li>`'s collapsed text, in document order.
fn parse_list_html(inner: &str) -> Vec<String> {
    let dom = Html::parse_fragment(inner);
    dom.root_element()
        .descendants()
        .filter_map(ElementRef::wrap)
        .filter(|e| e.value().name() == "li")
        .map(|li| {
            li.text()
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|t| !t.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> DoclingDocument {
        parse_chandra_pages("page", &[content.to_string()])
    }

    /// docling#4092: `<br>` is spacing, not concatenation.
    #[test]
    fn br_tags_preserve_spacing() {
        let doc =
            parse(r#"<div data-bbox="0 0 1000 1000" data-label="Text">Hello<br/>World</div>"#);
        assert_eq!(doc.export_to_markdown(), "Hello World\n");
    }

    /// Upstream's chandra_simple fixture: headers, text, a table.
    #[test]
    fn simple_fixture_parses_blocks() {
        let content = include_str!("../../tests/data/chandra/chandra_simple.html");
        let doc = parse(content);
        let md = doc.export_to_markdown();
        assert!(md.contains("order to compute the TED score"), "{md}");
        // The Page-Header block is furniture — absent from Markdown.
        assert!(!md.contains("Optimized Table Tokenization"), "{md}");
        assert!(
            doc.nodes.iter().any(|n| matches!(
                n,
                Node::Located { inner, .. } if matches!(**inner, Node::Table(_))
            )),
            "expected a table"
        );
    }

    /// Upstream's multiblock fixture keeps every block, including the figure.
    #[test]
    fn multiblock_fixture_keeps_pictures() {
        let content = include_str!("../../tests/data/chandra/chandra_multiblock.html");
        let doc = parse(content);
        assert!(doc.nodes.iter().any(|n| matches!(
            n,
            Node::Located { inner, .. } if matches!(**inner, Node::Picture { .. })
        )));
    }

    /// Upstream's list-group fixture: list items with text.
    #[test]
    fn list_group_fixture_yields_items() {
        let content = include_str!("../../tests/data/chandra/chandra_list_group.html");
        let doc = parse(content);
        let md = doc.export_to_markdown();
        assert!(md.contains("- "), "expected list items: {md}");
    }

    /// docling#4135: a "Form" block holding a `<table>` parses as a table —
    /// the fixture carries four of them.
    #[test]
    fn form_tables_parse_as_tables() {
        let content = include_str!("../../tests/data/chandra/chandra_form_table.html");
        let doc = parse(content);
        let tables = doc
            .nodes
            .iter()
            .filter(|n| {
                matches!(
                    n,
                    Node::Located { inner, .. } if matches!(**inner, Node::Table(_))
                )
            })
            .count();
        assert_eq!(tables, 4);
    }
}
