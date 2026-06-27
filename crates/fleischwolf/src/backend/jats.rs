//! JATS XML backend — a port of docling's `JatsDocumentBackend` (scientific
//! article XML). Emits the title, authors, affiliations and abstract from
//! `article-meta`, then walks `<body>` sections (headings + paragraphs) and the
//! `<back>` matter. Inline markup is flattened to text (docling does the same
//! pending styled-run support); formula tags are skipped.

use roxmltree::{Document, Node as XmlNode, ParsingOptions};

use crate::backend::markdown::escape_text;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use fleischwolf_core::{DoclingDocument, Node};

pub struct JatsBackend;

const SKIP_TEXT: &[&str] = &["term", "disp-formula", "inline-formula"];

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
        for tag in ["body", "back"] {
            if let Some(node) = dom.descendants().find(|n| n.has_tag_name(tag)) {
                walk(node, 0, &mut doc);
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

/// Walk a body/back subtree: sections → headings, paragraphs → text.
fn walk(node: XmlNode, level: u8, doc: &mut DoclingDocument) {
    for child in node.children().filter(XmlNode::is_element) {
        match child.tag_name().name() {
            "sec" | "ack" => {
                let title = child
                    .children()
                    .find(|c| c.has_tag_name("title") || c.has_tag_name("label"))
                    .map(node_text)
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        (child.has_tag_name("ack")).then(|| "Acknowledgements".to_string())
                    });
                if let Some(t) = title {
                    doc.push(Node::Heading {
                        level: level + 2,
                        text: escape_text(&t),
                    });
                }
                walk(child, level + 1, doc);
            }
            "p" => {
                let t = node_text(child);
                if !t.is_empty() {
                    doc.push(Node::Paragraph {
                        text: escape_text(&t),
                    });
                }
            }
            "title" | "label" => {} // consumed by the enclosing sec
            _ => walk(child, level, doc),
        }
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
}
