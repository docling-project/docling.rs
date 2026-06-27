//! USPTO patent XML backend (core) — a port of the modern
//! `us-patent-application`/`us-patent-grant` (v4x) path of docling's
//! `PatentUsptoDocumentBackend`. Emits the invention title (#), the ABSTRACT
//! (###) + text, the description's `<heading>`s (by their `level`) and `<p>`s,
//! and the CLAIMS. Older schemas (pap-v1, the legacy APS text format) and
//! tables/figures/maths are out of scope for the core.

use roxmltree::{Document, Node as XmlNode, ParsingOptions};

use crate::backend::markdown::escape_text;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_crab_core::{DoclingDocument, Node};

pub struct UsptoBackend;

impl DeclarativeBackend for UsptoBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let xml = source.text()?;
        let opts = ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        };
        let dom = Document::parse_with_options(xml, opts)
            .map_err(|e| ConversionError::Parse(format!("uspto: {e}")))?;
        let mut doc = DoclingDocument::new(&source.name);

        if let Some(title) = dom
            .descendants()
            .find(|n| n.has_tag_name("invention-title"))
            .map(node_text)
            .filter(|s| !s.is_empty())
        {
            doc.push(Node::Heading {
                level: 1,
                text: escape_text(&title),
            });
        }

        if let Some(abs) = dom.descendants().find(|n| n.has_tag_name("abstract")) {
            let paras = paragraphs(abs);
            if !paras.is_empty() {
                doc.push(Node::Heading {
                    level: 3,
                    text: "ABSTRACT".into(),
                });
                for p in paras {
                    doc.push(Node::Paragraph {
                        text: escape_text(&p),
                    });
                }
            }
        }

        if let Some(desc) = dom.descendants().find(|n| n.has_tag_name("description")) {
            walk_description(desc, &mut doc);
        }

        if let Some(claims) = dom.descendants().find(|n| n.has_tag_name("claims")) {
            doc.push(Node::Heading {
                level: 3,
                text: "CLAIMS".into(),
            });
            for claim in claims.children().filter(|c| c.has_tag_name("claim")) {
                for ct in claim.children().filter(|c| c.has_tag_name("claim-text")) {
                    let t = node_text(ct);
                    if !t.is_empty() {
                        doc.push(Node::Paragraph {
                            text: escape_text(&t),
                        });
                    }
                }
            }
        }
        Ok(doc)
    }
}

/// Walk `<description>`: `<heading level="N">` → a heading (`#`×(N+2)), `<p>` →
/// a paragraph; recurse into other containers.
fn walk_description(node: XmlNode, doc: &mut DoclingDocument) {
    for child in node.children().filter(XmlNode::is_element) {
        match child.tag_name().name() {
            "heading" => {
                let level = child
                    .attribute("level")
                    .and_then(|v| v.parse::<u8>().ok())
                    .unwrap_or(1);
                let t = node_text(child);
                if !t.is_empty() {
                    doc.push(Node::Heading {
                        level: level + 2,
                        text: escape_text(&t),
                    });
                }
            }
            "p" => {
                let t = node_text(child);
                if !t.is_empty() {
                    doc.push(Node::Paragraph {
                        text: escape_text(&t),
                    });
                }
            }
            "maths" | "tables" | "table" => {}
            _ => walk_description(child, doc),
        }
    }
}

/// Each `<p>` descendant's normalized text.
fn paragraphs(node: XmlNode) -> Vec<String> {
    node.descendants()
        .filter(|n| n.has_tag_name("p"))
        .map(node_text)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Recursive normalized text of a node, skipping `<maths>` (docling drops formulas).
fn node_text(node: XmlNode) -> String {
    let mut s = String::new();
    raw_text(node, &mut s);
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn raw_text(node: XmlNode, out: &mut String) {
    if let Some(t) = node.text() {
        out.push_str(&t.replace('\n', " "));
    }
    for child in node.children() {
        if child.is_element() {
            match child.tag_name().name() {
                // <sup>/<sub> digits and signs render as Unicode super/subscript.
                "sup" | "sub" => {
                    let mut inner = String::new();
                    raw_text(child, &mut inner);
                    let sup = child.tag_name().name() == "sup";
                    out.extend(inner.chars().map(|c| script_char(c, sup)));
                }
                "maths" => {}
                _ => raw_text(child, out),
            }
            if let Some(tail) = child.tail() {
                out.push_str(&tail.replace('\n', " "));
            }
        }
    }
}

/// Map a digit/sign/letter to its Unicode super- or subscript form (else
/// unchanged) — matching docling's `style_html` translation tables exactly.
fn script_char(c: char, sup: bool) -> char {
    if sup {
        match c {
            '0' => '⁰', '1' => '¹', '2' => '²', '3' => '³', '4' => '⁴', '5' => '⁵',
            '6' => '⁶', '7' => '⁷', '8' => '⁸', '9' => '⁹', '+' => '⁺',
            '-' | '−' => '⁻', '=' => '⁼', '(' => '⁽', ')' => '⁾',
            'a' => 'ª', 'o' => 'º', 'i' => 'ⁱ', 'n' => 'ⁿ',
            _ => c,
        }
    } else {
        match c {
            '0' => '₀', '1' => '₁', '2' => '₂', '3' => '₃', '4' => '₄', '5' => '₅',
            '6' => '₆', '7' => '₇', '8' => '₈', '9' => '₉', '+' => '₊',
            '-' | '−' => '₋', '=' => '₌', '(' => '₍', ')' => '₎',
            'a' => 'ₐ', 'e' => 'ₑ', 'o' => 'ₒ', 'x' => 'ₓ',
            _ => c,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    #[test]
    fn title_abstract_headings_and_scripts() {
        let xml = r#"<us-patent-application>
            <us-bibliographic-data-application>
              <invention-title>A Device</invention-title>
            </us-bibliographic-data-application>
            <abstract><p>An H<sub>2</sub>O cell at 10<sup>-3</sup>.</p></abstract>
            <description>
              <heading level="1">BACKGROUND</heading>
              <p>Body of NO<sub>3</sub><sup>-</sup>.</p>
              <heading level="2">Detail</heading>
            </description>
          </us-patent-application>"#;
        let src = SourceDocument::from_bytes("p", InputFormat::XmlUspto, xml.as_bytes().to_vec());
        let md = UsptoBackend.convert(&src).unwrap().export_to_markdown();
        assert!(
            md.starts_with(
                "# A Device\n\n### ABSTRACT\n\nAn H₂O cell at 10⁻³.\n\n### BACKGROUND\n\nBody of NO₃⁻.\n\n#### Detail"
            ),
            "got:\n{md}"
        );
    }
}
