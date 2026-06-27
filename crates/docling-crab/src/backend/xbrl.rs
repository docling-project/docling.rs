//! XBRL backend — a port of docling's `XbrlDocumentBackend` (without arelle).
//!
//! The document title is built from the `dei:DocumentType`,
//! `EntityRegistrantName` and `DocumentPeriodEndDate` facts. The body comes from
//! the `…TextBlock` facts (`textBlockItemType` concepts), whose values are
//! whitespace-collapsed HTML fragments converted with the HTML backend and
//! concatenated in document order (deduplicated).

use roxmltree::{Document, ParsingOptions};

use crate::backend::markdown::escape_text;
use crate::backend::{DeclarativeBackend, HtmlBackend};
use crate::error::ConversionError;
use crate::format::InputFormat;
use crate::source::SourceDocument;
use docling_crab_core::{DoclingDocument, Node};

pub struct XbrlBackend;

impl DeclarativeBackend for XbrlBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let xml = source.text()?;
        let opts = ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        };
        let dom = Document::parse_with_options(xml, opts)
            .map_err(|e| ConversionError::Parse(format!("xbrl: {e}")))?;
        let mut doc = DoclingDocument::new(&source.name);

        // Title from dei facts (last non-empty value of each, as docling's loop).
        let last = |local: &str| -> String {
            dom.descendants()
                .filter(|n| n.has_tag_name(local))
                .filter_map(|n| n.text())
                .map(str::trim)
                .rfind(|s| !s.is_empty())
                .unwrap_or("")
                .to_string()
        };
        let title = format!(
            "{} {} {}",
            last("DocumentType"),
            last("EntityRegistrantName"),
            last("DocumentPeriodEndDate")
        )
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
        if !title.is_empty() {
            doc.push(Node::Heading {
                level: 1,
                text: escape_text(&title),
            });
        }

        // Text-block facts → HTML → Markdown, in document order, deduplicated.
        let mut seen: Vec<String> = Vec::new();
        for el in dom.descendants() {
            if !el.tag_name().name().ends_with("TextBlock") {
                continue;
            }
            let Some(value) = el.text().map(str::trim).filter(|s| !s.is_empty()) else {
                continue;
            };
            let html = value.split_whitespace().collect::<Vec<_>>().join(" ");
            if seen.contains(&html) {
                continue;
            }
            seen.push(html.clone());
            let block = SourceDocument::from_bytes("block", InputFormat::Html, html.into_bytes());
            if let Ok(block_doc) = HtmlBackend.convert(&block) {
                for node in block_doc.nodes {
                    doc.push(node);
                }
            }
        }
        Ok(doc)
    }
}

/// Whether a generic `.xml` is XBRL (financial facts), used by the converter's
/// XML sniffer.
pub fn looks_like_xbrl(head: &str) -> bool {
    head.contains("us-gaap")
        || head.contains("xbrli:")
        || head.contains("dei:DocumentType")
        || head.contains("http://www.xbrl.org")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_from_dei_facts_and_html_text_block() {
        let xml = r#"<xbrl xmlns:dei="d" xmlns:us-gaap="u">
            <dei:DocumentType>10-Q</dei:DocumentType>
            <dei:EntityRegistrantName>Acme Inc.</dei:EntityRegistrantName>
            <dei:DocumentPeriodEndDate>2025-12-31</dei:DocumentPeriodEndDate>
            <us-gaap:NatureOfOperationsTextBlock>&lt;p&gt;&lt;b&gt;NOTE 1&lt;/b&gt;&lt;/p&gt;&lt;p&gt;Body.&lt;/p&gt;</us-gaap:NatureOfOperationsTextBlock>
          </xbrl>"#;
        let src = SourceDocument::from_bytes("x", InputFormat::XmlXbrl, xml.as_bytes().to_vec());
        let md = XbrlBackend.convert(&src).unwrap().export_to_markdown();
        assert!(
            md.starts_with("# 10-Q Acme Inc. 2025-12-31\n\n**NOTE 1**\n\nBody."),
            "got:\n{md}"
        );
    }
}
