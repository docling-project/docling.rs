//! USPTO patent XML backend (core) — a port of the modern
//! `us-patent-application`/`us-patent-grant` (v4x) path of docling's
//! `PatentUsptoDocumentBackend`. Emits the invention title (#), the ABSTRACT
//! (###) + text, the description's `<heading>`s (by their `level`) and `<p>`s,
//! and the CLAIMS. Older schemas (pap-v1, the legacy APS text format) and
//! tables/figures/maths are out of scope for the core.

use std::borrow::Cow;

use roxmltree::{Document, Node as XmlNode, ParsingOptions};

use crate::backend::markdown::escape_text;
use crate::backend::uspto_entities::NAMED_ENTITIES;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::{DoclingDocument, Node};

pub struct UsptoBackend;

impl DeclarativeBackend for UsptoBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let raw = source.text()?;
        let xml = resolve_named_entities(raw);
        let opts = ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        };
        let dom = Document::parse_with_options(&xml, opts)
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
                // docling emits the abstract as a single text item — its
                // paragraphs (with any chemistry-drawing `<p>` dropped as empty)
                // are joined into one, not split per `<p>`.
                doc.push(Node::Paragraph {
                    text: escape_text(&paras.join(" ")),
                });
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
    // Walk every child in order. Text is captured from text-node children
    // directly (not via node.text()/tail shortcuts), so text following a
    // processing instruction or comment — e.g. the leading "R" in
    // `<?in-line-formulae?>R<sup>1</sup>—CO…` — is not dropped.
    for child in node.children() {
        if child.is_text() {
            if let Some(t) = child.text() {
                out.push_str(&t.replace('\n', " "));
            }
        } else if child.is_element() {
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
        }
    }
}

/// Map a digit/sign/letter to its Unicode super- or subscript form (else
/// unchanged) — matching docling's `style_html` translation tables exactly.
fn script_char(c: char, sup: bool) -> char {
    if sup {
        match c {
            '0' => '⁰',
            '1' => '¹',
            '2' => '²',
            '3' => '³',
            '4' => '⁴',
            '5' => '⁵',
            '6' => '⁶',
            '7' => '⁷',
            '8' => '⁸',
            '9' => '⁹',
            '+' => '⁺',
            '-' | '−' => '⁻',
            '=' => '⁼',
            '(' => '⁽',
            ')' => '⁾',
            'a' => 'ª',
            'o' => 'º',
            'i' => 'ⁱ',
            'n' => 'ⁿ',
            _ => c,
        }
    } else {
        match c {
            '0' => '₀',
            '1' => '₁',
            '2' => '₂',
            '3' => '₃',
            '4' => '₄',
            '5' => '₅',
            '6' => '₆',
            '7' => '₇',
            '8' => '₈',
            '9' => '₉',
            '+' => '₊',
            '-' | '−' => '₋',
            '=' => '₌',
            '(' => '₍',
            ')' => '₎',
            'a' => 'ₐ',
            'e' => 'ₑ',
            'o' => 'ₒ',
            'x' => 'ₓ',
            _ => c,
        }
    }
}

/// Resolve non-predefined named character references (`&trade;`, `&agr;`,
/// `&lsqb;`, …) into their literal characters so roxmltree — which only knows
/// the five XML built-ins and internally declared entities — can parse legacy
/// USPTO SGML documents. Mirrors docling's `skippedEntity` handling: the ISO
/// 8879 Greek names fold onto their HTML5 counterparts inside the generated
/// table, recognized entities expand, and unrecognized ones are dropped.
///
/// The XML built-ins (`amp`/`lt`/`gt`/`quot`/`apos`) and numeric references
/// (`&#…;`) are left untouched for the parser to resolve. Entity references
/// whose name uses characters outside `[A-Za-z0-9]` (the unparsed-graphics
/// `NDATA` entities USPTO declares in its internal subset) are also dropped —
/// they are illegal in element content and would abort the parse.
fn resolve_named_entities(xml: &str) -> Cow<'_, str> {
    if !xml.contains('&') {
        return Cow::Borrowed(xml);
    }
    let mut out = String::with_capacity(xml.len());
    let mut i = 0;
    while let Some(rel) = xml[i..].find('&') {
        let amp = i + rel;
        out.push_str(&xml[i..amp]);
        // Find the terminating ';' within a bounded window (entity names are short).
        let end = xml[amp + 1..]
            .char_indices()
            .take(64)
            .find(|&(_, c)| c == ';')
            .map(|(off, _)| amp + 1 + off);
        let Some(semi) = end else {
            out.push('&');
            i = amp + 1;
            continue;
        };
        let name = &xml[amp + 1..semi];
        i = semi + 1;
        // Numeric references and the XML built-ins pass through verbatim.
        if name.starts_with('#') || matches!(name, "amp" | "lt" | "gt" | "quot" | "apos") {
            out.push('&');
            out.push_str(name);
            out.push(';');
            continue;
        }
        // Only plain-alphanumeric names can be table entities; anything else is
        // a declared graphics entity — drop it (docling skips those too).
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric()) {
            continue;
        }
        // Recognized names expand; unrecognized ones are dropped (docling skips
        // them too).
        if let Ok(idx) = NAMED_ENTITIES.binary_search_by(|&(n, _)| n.cmp(name)) {
            push_xml_escaped(&mut out, NAMED_ENTITIES[idx].1);
        }
    }
    out.push_str(&xml[i..]);
    Cow::Owned(out)
}

/// Append `s`, re-escaping the three characters that would otherwise disturb
/// the surrounding XML once this string is fed back to the parser.
fn push_xml_escaped(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    #[test]
    fn resolves_iso_and_html_entities_and_drops_unknown() {
        assert_eq!(resolve_named_entities("a&trade;b"), "a\u{2122}b");
        assert_eq!(resolve_named_entities("x&agr;y"), "x\u{3b1}y"); // ISO 8879 alpha
        assert_eq!(resolve_named_entities("p&lsqb;q&rsqb;"), "p[q]");
        assert_eq!(
            resolve_named_entities("keep &amp; and &#65;"),
            "keep &amp; and &#65;"
        );
        assert_eq!(resolve_named_entities("drop&zzznope;it"), "dropit");
        assert_eq!(resolve_named_entities("amp&AMP;ersand"), "amp&amp;ersand");
        // Declared NDATA graphics entity (dot in the name) — dropped.
        assert_eq!(resolve_named_entities("g&US001.TIF;h"), "gh");
        assert_eq!(
            resolve_named_entities("no entities here"),
            "no entities here"
        );
    }

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

    #[test]
    fn keeps_text_following_a_processing_instruction() {
        // The leading run before an <?in-line-formulae?> PI is the PI's tail;
        // it must not be dropped (docling keeps "R¹—CO", not "¹—CO").
        let xml = r#"<us-patent-application>
            <description>
              <p><?in-line-formulae description="In-line Formulae" end="lead"?>R<sup>1</sup>&#x2014;CO</p>
            </description>
          </us-patent-application>"#;
        let src = SourceDocument::from_bytes("p", InputFormat::XmlUspto, xml.as_bytes().to_vec());
        let md = UsptoBackend.convert(&src).unwrap().export_to_markdown();
        assert!(md.contains("R¹—CO"), "got:\n{md}");
    }

    #[test]
    fn abstract_paragraphs_join_into_one_text() {
        // docling emits the abstract as a single text item; a chemistry-drawing
        // <p> in the middle is dropped, the surrounding text stays one paragraph.
        let xml = r#"<us-patent-application>
            <abstract>
              <p>The invention relates to compounds of the formula (I)</p>
              <p><chemistry><img file="C00001.TIF"/></chemistry></p>
              <p>in which X has the meaning given above.</p>
            </abstract>
          </us-patent-application>"#;
        let src = SourceDocument::from_bytes("p", InputFormat::XmlUspto, xml.as_bytes().to_vec());
        let md = UsptoBackend.convert(&src).unwrap().export_to_markdown();
        assert!(
            md.contains(
                "### ABSTRACT\n\nThe invention relates to compounds of the formula (I) in which X has the meaning given above."
            ),
            "got:\n{md}"
        );
    }
}
