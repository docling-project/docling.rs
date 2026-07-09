//! JATS XML backend — a port of docling's `JatsDocumentBackend` (scientific
//! article XML). Emits the title, authors, affiliations and abstract from
//! `article-meta`, then walks `<body>` and the `<back>` matter with a port of
//! docling's `_walk_linear`: sections → headings, paragraphs → text, plus the
//! full article-body machinery — `<table-wrap>` tables (with caption), `<fig>`
//! figures (caption + picture), `<list>`/`<list-item>` bullet lists,
//! `<ref-list>` references and `<element-citation>`/`<mixed-citation>` citations,
//! `<fn-group>` footnotes, and `<disp-formula>` equations (`$$…$$`). Inline
//! markup is flattened to text (docling does the same pending styled-run
//! support).

use roxmltree::{Document, Node as XmlNode, ParsingOptions};

use crate::backend::markdown::escape_text;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::{DoclingDocument, Node, Table};

pub struct JatsBackend;

const SKIP_TEXT: &[&str] = &["term", "disp-formula", "inline-formula"];

/// Tags that, inside a `<p>`, flush the accumulated paragraph text before they
/// are handled — docling's `_walk_linear` `flush_tags`.
const FLUSH_TAGS: &[&str] = &["ack", "sec", "list", "boxed-text", "disp-formula", "fig"];

const DEFAULT_HEADER_ACKNOWLEDGMENTS: &str = "Acknowledgments";
const DEFAULT_HEADER_FOOTNOTES: &str = "Footnotes";
const DEFAULT_HEADER_REFERENCES: &str = "References";
const DEFAULT_TEXT_ETAL: &str = "et al.";

impl DeclarativeBackend for JatsBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let xml = source.text()?;
        // JATS files carry a DOCTYPE/DTD reference, which roxmltree rejects by default.
        let opts = ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        };
        let dom = Document::parse_with_options(xml, opts)
            .map_err(|e| ConversionError::Parse(format!("jats: {e}")))?;
        let mut doc = DoclingDocument::new(&source.name);

        // --- metadata -------------------------------------------------------
        if let Some(title) = parse_title(&dom) {
            doc.push(Node::Heading {
                level: 1,
                text: escape_text(&title),
            });
        }
        let (authors, affiliations) = parse_authors(&dom);
        if !authors.is_empty() {
            doc.push(Node::Paragraph {
                text: escape_text(&authors.join(", ")),
            });
        }
        if !affiliations.is_empty() {
            doc.push(Node::Paragraph {
                text: escape_text(&affiliations.join("; ")),
            });
        }
        for (label, content) in parse_abstracts(&dom) {
            if content.is_empty() {
                continue;
            }
            doc.push(Node::Heading {
                level: 2,
                text: escape_text(&label),
            });
            doc.push(Node::Paragraph {
                text: escape_text(&content),
            });
        }

        // --- body + back ----------------------------------------------------
        // `hlevel` is a running section depth carried across body and back
        // (docling's `self.hlevel`); it is balanced by each `<sec>`.
        let mut hlevel: i32 = 0;
        for tag in ["body", "back"] {
            if let Some(node) = dom.descendants().find(|n| n.has_tag_name(tag)) {
                walk_linear(node, false, &mut hlevel, &mut doc);
            }
        }
        Ok(doc)
    }
}

/// Recursive text of a node: its text + descendants + tails, skipping formula
/// tags, then whitespace-normalized — docling's `_get_text` + `_normalize`.
fn raw_text(node: XmlNode, out: &mut String) {
    if let Some(t) = node.text() {
        out.push_str(&t.replace('\n', " "));
    }
    for child in node.children() {
        if child.is_element() {
            if !SKIP_TEXT.contains(&child.tag_name().name()) {
                raw_text(child, out);
            }
            if let Some(tail) = child.tail() {
                out.push_str(&tail.replace('\n', " "));
            }
        } else if child.is_text() {
            // handled by node.text()/tail above for elements; bare text nodes here
        }
    }
}

fn node_text(node: XmlNode) -> String {
    let mut s = String::new();
    raw_text(node, &mut s);
    normalize(&s)
}

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_title(dom: &Document) -> Option<String> {
    dom.descendants()
        .find(|n| n.has_tag_name("article-meta"))
        .and_then(|meta| meta.descendants().find(|n| n.has_tag_name("article-title")))
        .map(node_text)
        .filter(|s| !s.is_empty())
}

/// Authors (`given-names surname`) and their (deduplicated) affiliation names.
fn parse_authors(dom: &Document) -> (Vec<String>, Vec<String>) {
    let Some(meta) = dom.descendants().find(|n| n.has_tag_name("article-meta")) else {
        return (Vec::new(), Vec::new());
    };
    // id -> affiliation name
    let mut aff_by_id = std::collections::HashMap::new();
    for aff in meta.descendants().filter(|n| n.has_tag_name("aff")) {
        let Some(id) = aff.attribute("id") else {
            continue;
        };
        // docling joins the affiliation's text fragments (itertext) with ", ".
        let mut name = aff
            .descendants()
            .filter(|n| n.is_text())
            .filter_map(|n| n.text())
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
            .replace('\n', " ");
        // strip a leading "<label>, " prefix
        if let Some(label) = aff
            .children()
            .find(|c| c.has_tag_name("label"))
            .and_then(|l| l.text())
        {
            name = name
                .strip_prefix(&format!("{label}, "))
                .unwrap_or(&name)
                .to_string();
        }
        aff_by_id.insert(id.to_string(), name);
    }

    let mut authors = Vec::new();
    let mut affiliations = Vec::new();
    for contrib in meta
        .descendants()
        .filter(|n| n.has_tag_name("contrib") && n.attribute("contrib-type") == Some("author"))
    {
        let name = contrib_name(contrib);
        if name.is_empty() {
            continue;
        }
        authors.push(name);
        for xref in contrib
            .children()
            .filter(|c| c.has_tag_name("xref") && c.attribute("ref-type") == Some("aff"))
        {
            if let Some(aff) = xref.attribute("rid").and_then(|id| aff_by_id.get(id)) {
                if !affiliations.contains(aff) {
                    affiliations.push(aff.clone());
                }
            }
        }
    }
    (authors, affiliations)
}

/// `prefix given-names surname suffix`, space-joined (docling `_parse_structured_name`).
fn contrib_name(contrib: XmlNode) -> String {
    let name = contrib.children().find(|c| c.has_tag_name("name"));
    let Some(name) = name else {
        return String::new();
    };
    ["prefix", "given-names", "surname", "suffix"]
        .iter()
        .filter_map(|tag| {
            name.children()
                .find(|c| c.has_tag_name(*tag))
                .and_then(|c| c.text())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Abstracts as `(label, content)`; nested sections render as `label: content`.
fn parse_abstracts(dom: &Document) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for abs in dom.descendants().filter(|n| n.has_tag_name("abstract")) {
        let content = abstract_section(abs);
        let label = abs
            .children()
            .find(|c| c.has_tag_name("title"))
            .map(node_text)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Abstract".to_string());
        out.push((label, content));
    }
    out
}

fn abstract_section(section: XmlNode) -> String {
    let mut texts = Vec::new();
    for child in section.children().filter(XmlNode::is_element) {
        match child.tag_name().name() {
            "p" => {
                let t = node_text(child);
                if !t.is_empty() {
                    texts.push(t);
                }
            }
            "sec" => {
                let inner = abstract_section(child);
                if !inner.is_empty() {
                    let label = child
                        .children()
                        .find(|c| c.has_tag_name("title") || c.has_tag_name("label"))
                        .map(node_text)
                        .filter(|s| !s.is_empty());
                    texts.push(match label {
                        Some(l) => format!("{l}: {inner}"),
                        None => inner,
                    });
                }
            }
            _ => {}
        }
    }
    normalize(&texts.join(" "))
}

/// `_get_text`, un-normalized (newlines → spaces, formula tags skipped).
fn get_text(node: XmlNode) -> String {
    let mut s = String::new();
    raw_text(node, &mut s);
    s
}

/// The un-normalized text of a node, trimmed and whitespace-normalized — used for
/// list items, captions and citations (docling `_get_text(...).strip()`, which on
/// clean JATS sources is equivalent to a whitespace collapse).
fn norm_text(node: XmlNode) -> String {
    normalize(&get_text(node))
}

/// Markdown heading level for docling heading level `dl` (docling renders a
/// heading at level `N` with `N+1` hashes; `docling.rs`'s serializer emits a
/// `Heading{level}` with `level` hashes, so `docling.rs level = dl + 1`).
fn fw_level(dl: i32) -> u8 {
    (dl + 1).clamp(1, 6) as u8
}

/// A `<sec>`/`<ack>` header text (`title|label`), or a default for `<ack>`.
fn header_text(child: XmlNode) -> Option<String> {
    child
        .children()
        .find(|c| c.has_tag_name("title") || c.has_tag_name("label"))
        .map(get_text)
        .map(|s| normalize(&s))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            child
                .has_tag_name("ack")
                .then(|| DEFAULT_HEADER_ACKNOWLEDGMENTS.to_string())
        })
}

/// A citation renders as a list item inside a list group, else a paragraph —
/// docling's `_add_citation`.
fn add_citation(doc: &mut DoclingDocument, parent_is_list: bool, text: &str) {
    if text.is_empty() {
        return;
    }
    if parent_is_list {
        doc.push(Node::ListItem {
            ordered: false,
            number: 0,
            first_in_list: false,
            text: escape_text(text),
            level: 0,
            marker: None,
            location: None,
        });
    } else {
        doc.push(Node::Paragraph {
            text: escape_text(text),
        });
    }
}

/// Port of docling's `_walk_linear`: a depth-first walk that accumulates a
/// paragraph's inline text while emitting block-level items (sections, lists,
/// figures, tables, citations, footnotes, formulas) as it goes. Returns the text
/// it could not emit (backpropagated to the enclosing paragraph).
fn walk_linear(
    node: XmlNode,
    parent_is_list: bool,
    hlevel: &mut i32,
    doc: &mut DoclingDocument,
) -> String {
    let node_tag = node.tag_name().name();
    let mut node_text = if node_tag != "term" {
        node.text()
            .map(|t| t.replace('\n', " "))
            .unwrap_or_default()
    } else {
        String::new()
    };

    for child in node.children().filter(XmlNode::is_element) {
        let mut stop_walk = false;
        let ctag = child.tag_name().name();

        // Flush accumulated paragraph text before a block-level child.
        if node_tag == "p" && !node_text.trim().is_empty() && FLUSH_TAGS.contains(&ctag) {
            doc.push(Node::Paragraph {
                text: escape_text(node_text.trim()),
            });
            node_text.clear();
        }

        // Whether the recursion below should treat `child` as a list parent.
        let mut child_in_list = parent_is_list;
        // Whether this child opened a section (so we decrement `hlevel` after).
        let mut opened_section = false;

        match ctag {
            "sec" | "ack" => {
                if let Some(text) = header_text(child) {
                    *hlevel += 1;
                    doc.push(Node::Heading {
                        level: fw_level(*hlevel),
                        text: escape_text(&text),
                    });
                    opened_section = true;
                }
            }
            "list" => {
                child_in_list = true;
            }
            "list-item" => {
                let text = norm_text(child);
                if !text.is_empty() {
                    doc.push(Node::ListItem {
                        ordered: false,
                        number: 0,
                        first_in_list: false,
                        text: escape_text(&text),
                        level: 0,
                        marker: None,
                        location: None,
                    });
                }
                stop_walk = true;
            }
            "fig" => {
                add_figure(doc, child);
                stop_walk = true;
            }
            "table-wrap" => {
                add_table(doc, child);
                stop_walk = true;
            }
            "supplementary-material" => {
                stop_walk = true;
            }
            "fn-group" => {
                add_footnote_group(doc, child, *hlevel);
                stop_walk = true;
            }
            "ref-list" if node_tag != "ref-list" => {
                let text = child
                    .children()
                    .find(|c| c.has_tag_name("title") || c.has_tag_name("label"))
                    .map(|h| normalize(&get_text(h)))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| DEFAULT_HEADER_REFERENCES.to_string());
                doc.push(Node::Heading {
                    level: fw_level(1),
                    text: escape_text(&text),
                });
                child_in_list = true;
            }
            "element-citation" => {
                let text = parse_element_citation(child);
                add_citation(doc, parent_is_list, &text);
                stop_walk = true;
            }
            "mixed-citation" => {
                let text = norm_text(child);
                add_citation(doc, parent_is_list, &text);
                stop_walk = true;
            }
            "tex-math" => {
                add_equation(doc, child);
                stop_walk = true;
            }
            "inline-formula" => {
                stop_walk = true;
            }
            _ => {}
        }

        if !stop_walk {
            let new_text = walk_linear(child, child_in_list, hlevel, doc);
            // Don't fold a flushed block's text back into an enclosing paragraph.
            let parent_is_p = node.parent().map(|p| p.has_tag_name("p")).unwrap_or(false);
            if !(parent_is_p && FLUSH_TAGS.contains(&node_tag)) {
                node_text.push_str(&new_text);
            }
            if opened_section {
                *hlevel -= 1;
            }
        }

        if let Some(tail) = child.tail() {
            node_text.push_str(&tail.replace('\n', " "));
        }
    }

    if node_tag == "p" && !node_text.trim().is_empty() {
        doc.push(Node::Paragraph {
            text: escape_text(node_text.trim()),
        });
        String::new()
    } else {
        node_text
    }
}

/// A `<disp-formula>`'s `<tex-math>` child (`…$$formula$$…`) → a `$$…$$` block.
fn add_equation(doc: &mut DoclingDocument, node: XmlNode) {
    let Some(math) = node.text() else { return };
    let parts: Vec<&str> = math.split("$$").collect();
    if parts.len() == 3 {
        doc.push(Node::Paragraph {
            text: format!("$${}$$", parts[1]),
        });
    }
}

/// A `<fig>` → its label + caption as a picture caption, then a picture marker.
fn add_figure(doc: &mut DoclingDocument, node: XmlNode) {
    let label = node
        .children()
        .find(|c| c.has_tag_name("label"))
        .map(|l| get_text(l).trim().to_string())
        .unwrap_or_default();
    let caption = node
        .children()
        .find(|c| c.has_tag_name("caption"))
        .map(caption_text)
        .unwrap_or_default();
    let sep = if !label.is_empty() && !caption.is_empty() {
        " "
    } else {
        ""
    };
    let fig_text = format!("{label}{sep}{caption}");
    doc.push(Node::Picture {
        caption: (!fig_text.is_empty()).then(|| escape_text(&fig_text)),
        image: None,
    });
}

/// A `<caption>`'s paragraphs, space-joined and trimmed (skipping any that hold
/// supplementary material) — docling's caption assembly.
fn caption_text(caption: XmlNode) -> String {
    let mut out = String::new();
    for par in caption.children().filter(XmlNode::is_element) {
        if par
            .descendants()
            .any(|d| d.has_tag_name("supplementary-material"))
        {
            continue;
        }
        out.push_str(get_text(par).trim());
        out.push(' ');
    }
    out.trim().to_string()
}

/// A `<table-wrap>` → an optional caption paragraph followed by the table grid.
fn add_table(doc: &mut DoclingDocument, node: XmlNode) {
    let content = node
        .children()
        .find(|c| c.has_tag_name("table"))
        .or_else(|| {
            node.children()
                .find(|c| c.has_tag_name("alternatives"))
                .and_then(|a| a.children().find(|c| c.has_tag_name("table")))
        });
    let Some(table_node) = content else { return };
    let Some(table) = parse_jats_table(table_node) else {
        return;
    };

    let label = node
        .children()
        .find(|c| c.has_tag_name("label"))
        .and_then(|l| l.text())
        .map(|t| t.trim().to_string())
        .unwrap_or_default();
    let caption = node
        .children()
        .find(|c| c.has_tag_name("caption"))
        .map(caption_text)
        .unwrap_or_default();
    let sep = if !label.is_empty() && !caption.is_empty() {
        " "
    } else {
        ""
    };
    let cap_text = format!("{label}{sep}{caption}");
    if !cap_text.is_empty() {
        doc.push(Node::Paragraph {
            text: escape_text(&cap_text),
        });
    }
    doc.push(Node::Table(table));
}

/// Parse a JATS/XHTML `<table>` into a row-major grid, expanding `colspan`
/// (duplicated across columns) and `rowspan` (filled down). Header rows (`<th>`
/// or `<thead>`) come first, matching docling's `parse_table_data` layout.
fn parse_jats_table(table: XmlNode) -> Option<Table> {
    // A nested table is unsupported (docling bails on `element.find("table")`).
    let rows_nodes: Vec<XmlNode> = table
        .descendants()
        .filter(|n| n.has_tag_name("tr"))
        .collect();
    if rows_nodes.iter().any(|r| {
        r.descendants()
            .any(|d| d.has_tag_name("table") && d != table)
    }) {
        return None;
    }

    // Number of columns = the widest row (accounting for colspans).
    let num_cols = rows_nodes
        .iter()
        .map(|r| {
            r.children()
                .filter(|c| c.has_tag_name("td") || c.has_tag_name("th"))
                .map(col_span)
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0);
    if rows_nodes.is_empty() || num_cols == 0 {
        return None;
    }

    let mut grid: Vec<Vec<String>> = vec![vec![String::new(); num_cols]; rows_nodes.len()];
    // Track which cells are already occupied by a rowspan from above.
    let mut filled: Vec<Vec<bool>> = vec![vec![false; num_cols]; rows_nodes.len()];
    for (ri, row) in rows_nodes.iter().enumerate() {
        let mut ci = 0usize;
        for cell in row
            .children()
            .filter(|c| c.has_tag_name("td") || c.has_tag_name("th"))
        {
            while ci < num_cols && filled[ri][ci] {
                ci += 1;
            }
            if ci >= num_cols {
                break;
            }
            let cs = col_span(cell);
            let rs = row_span(cell);
            let text = normalize(&get_text(cell));
            for r in ri..(ri + rs).min(rows_nodes.len()) {
                for c in ci..(ci + cs).min(num_cols) {
                    grid[r][c] = text.clone();
                    filled[r][c] = true;
                }
            }
            ci += cs;
        }
    }
    Some(Table {
        rows: grid,
        location: None,
        structure: None,
    })
}

fn col_span(cell: XmlNode) -> usize {
    cell.attribute("colspan")
        .and_then(|v| v.parse().ok())
        .filter(|&n: &usize| n >= 1)
        .unwrap_or(1)
}

fn row_span(cell: XmlNode) -> usize {
    cell.attribute("rowspan")
        .and_then(|v| v.parse().ok())
        .filter(|&n: &usize| n >= 1)
        .unwrap_or(1)
}

/// A `<fn-group>` → a "Footnotes" heading and a bullet list of its `<fn>` texts.
fn add_footnote_group(doc: &mut DoclingDocument, node: XmlNode, hlevel: i32) {
    let footnotes: Vec<String> = node
        .children()
        .filter(|c| c.has_tag_name("fn"))
        .map(norm_text)
        .filter(|s| !s.is_empty())
        .collect();
    if footnotes.is_empty() {
        return;
    }
    let title = node
        .children()
        .find(|c| c.has_tag_name("title"))
        .map(|t| normalize(&get_text(t)))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_HEADER_FOOTNOTES.to_string());
    doc.push(Node::Heading {
        level: fw_level(hlevel + 1),
        text: escape_text(&title),
    });
    for item in footnotes {
        doc.push(Node::ListItem {
            ordered: false,
            number: 0,
            first_in_list: false,
            text: escape_text(&item),
            level: 0,
            marker: None,
            location: None,
        });
    }
}

/// Flatten an `<element-citation>` to a single reference string — a port of
/// docling's `_parse_element_citation`.
fn parse_element_citation(node: XmlNode) -> String {
    // Author names ("surname given-names"), plus a trailing "et al." if present.
    let mut names: Vec<String> = Vec::new();
    for name in node.descendants().filter(|n| n.has_tag_name("name")) {
        let surname = name
            .children()
            .find(|c| c.has_tag_name("surname"))
            .and_then(|c| c.text())
            .map(|t| t.replace('\n', " "))
            .map(|t| t.trim().to_string());
        let given = name
            .children()
            .find(|c| c.has_tag_name("given-names"))
            .and_then(|c| c.text())
            .map(|t| t.replace('\n', " "))
            .map(|t| t.trim().to_string());
        if let (Some(s), Some(g)) = (surname, given) {
            names.push(format!("{s} {g}"));
        }
    }
    if let Some(etal) = node.descendants().find(|n| n.has_tag_name("etal")) {
        let etal_text = etal
            .text()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_TEXT_ETAL);
        names.push(etal_text.to_string());
    }
    let author_names = names.join(", ");

    // Title (the first of several possible tags).
    let title = [
        "article-title",
        "chapter-title",
        "data-title",
        "issue-title",
        "part-title",
        "trans-title",
    ]
    .iter()
    .find_map(|t| node.children().find(|c| c.has_tag_name(*t)))
    .map(get_text)
    .unwrap_or_else(|| {
        node.text()
            .map(|t| t.replace('\n', " ").trim().to_string())
            .unwrap_or_default()
    });

    let field = |name: &str| -> String {
        node.children()
            .find(|c| c.has_tag_name(name))
            .and_then(|c| c.text())
            .map(|t| t.replace('\n', " ").trim().to_string())
            .unwrap_or_default()
    };
    let source = field("source");
    let year = field("year");
    let publisher_name = field("publisher-name");
    let publisher_loc = field("publisher-loc");
    let volume = field("volume");

    // Publication identifiers (DOI/PMID/…).
    let mut pub_ids: Vec<String> = Vec::new();
    for id in node.children().filter(|c| c.has_tag_name("pub-id")) {
        let id_type = id
            .attribute("assigning-authority")
            .or_else(|| id.attribute("pub-id-type"));
        if let (Some(t), Some(text)) = (id_type, id.text()) {
            pub_ids.push(format!(
                "{}: {}",
                t.replace('\n', " ").trim().to_uppercase(),
                text.replace('\n', " ").trim()
            ));
        }
    }
    let pub_id = pub_ids.join(", ");

    // Pages: an elocation-id, or an fpage(–lpage) range.
    let page = if let Some(e) = node.children().find(|c| c.has_tag_name("elocation-id")) {
        e.text()
            .map(|t| t.replace('\n', " ").trim().to_string())
            .unwrap_or_default()
    } else if let Some(f) = node.children().find(|c| c.has_tag_name("fpage")) {
        let mut p = f
            .text()
            .map(|t| t.replace('\n', " ").trim().to_string())
            .unwrap_or_default();
        if let Some(l) = node.children().find(|c| c.has_tag_name("lpage")) {
            p.push('\u{2013}');
            p.push_str(
                l.text()
                    .map(|t| t.replace('\n', " "))
                    .unwrap_or_default()
                    .trim(),
            );
        }
        p
    } else {
        String::new()
    };

    // Assemble, mirroring docling's rstrip-and-append sequence.
    let mut text = String::new();
    if !author_names.is_empty() {
        text.push_str(author_names.trim_end_matches('.'));
        text.push_str(". ");
    }
    if !title.is_empty() {
        text.push_str(title.trim());
        text.push_str(". ");
    }
    if !source.is_empty() {
        text.push_str(&source);
        text.push_str(". ");
    }
    if !publisher_name.is_empty() {
        if !publisher_loc.is_empty() {
            text.push_str(&format!("{publisher_loc}: "));
        }
        text.push_str(&publisher_name);
        text.push_str(". ");
    }
    if !volume.is_empty() {
        rstrip_dot_space(&mut text);
        text.push_str(&format!(" {volume}. "));
    }
    if !page.is_empty() {
        rstrip_dot_space(&mut text);
        if !volume.is_empty() {
            text.push(':');
        }
        text.push_str(&page);
        text.push_str(". ");
    }
    if !year.is_empty() {
        rstrip_dot_space(&mut text);
        text.push_str(&format!(" ({year})."));
    }
    if !pub_id.is_empty() {
        while text.ends_with('.') {
            text.pop();
        }
        text.push_str(". ");
        text.push_str(&pub_id);
    }
    text
}

/// Python `str.rstrip(". ")`: drop any trailing run of `.` and space characters.
fn rstrip_dot_space(s: &mut String) {
    while matches!(s.chars().last(), Some('.') | Some(' ')) {
        s.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    #[test]
    fn metadata_and_sections() {
        let xml = r#"<article><front><article-meta>
            <title-group><article-title>My Paper</article-title></title-group>
            <contrib-group>
              <contrib contrib-type="author"><name><surname>Doe</surname><given-names>Jane</given-names></name>
                <xref ref-type="aff" rid="a1"/></contrib>
            </contrib-group>
            <aff id="a1"><label>1</label>Acme &amp; Co</aff>
            <abstract><p>Short summary.</p></abstract>
          </article-meta></front>
          <body><sec><title>Intro</title><p>Body text.</p></sec></body></article>"#;
        let src = SourceDocument::from_bytes("p", InputFormat::XmlJats, xml.as_bytes().to_vec());
        let md = JatsBackend.convert(&src).unwrap().export_to_markdown();
        // title #, author, label-stripped + escaped affiliation, ## Abstract, ## Intro
        assert!(md.starts_with("# My Paper\n\nJane Doe\n\nAcme &amp; Co\n\n## Abstract\n\nShort summary.\n\n## Intro\n\nBody text."), "got:\n{md}");
    }

    #[test]
    fn body_tables_figures_and_references() {
        let xml = r#"<article><front><article-meta>
            <title-group><article-title>T</article-title></title-group>
          </article-meta></front>
          <body><sec><title>S</title>
            <fig><label>Fig 1</label><caption><p>A caption.</p></caption><graphic/></fig>
            <table-wrap><label>Table 1</label><caption><p>Table cap.</p></caption>
              <table><thead><tr><th>Name</th><th>N</th></tr></thead>
              <tbody><tr><td>a</td><td>1</td></tr></tbody></table></table-wrap>
          </sec></body>
          <back><ref-list><title>References</title>
            <ref><mixed-citation>Doe J. A title. 2020.</mixed-citation></ref>
          </ref-list></back></article>"#;
        let src = SourceDocument::from_bytes("p", InputFormat::XmlJats, xml.as_bytes().to_vec());
        let md = JatsBackend.convert(&src).unwrap().export_to_markdown();
        assert!(
            md.contains("Fig 1 A caption.\n\n<!-- image -->"),
            "figure:\n{md}"
        );
        assert!(md.contains("Table 1 Table cap."), "table caption:\n{md}");
        assert!(md.contains("| Name"), "table grid:\n{md}");
        assert!(md.contains("## References"), "refs heading:\n{md}");
        assert!(md.contains("- Doe J. A title. 2020."), "citation:\n{md}");
    }
}
