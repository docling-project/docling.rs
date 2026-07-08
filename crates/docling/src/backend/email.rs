//! Email (`.eml`) backend — a port of docling's `EmailDocumentBackend`.
//!
//! The subject becomes the document title; `From:`/`To:`/`Date:` headers become
//! text paragraphs; the body (preferring `text/plain`) is split into paragraphs
//! on blank lines. All emitted text is HTML/underscore-escaped like docling-core
//! (so `<a@b>` renders as `&lt;a@b&gt;`).

use mail_parser::{Address, Message, MessageParser};

use crate::backend::markdown::escape_text;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::{DoclingDocument, Node};

pub struct EmailBackend;

impl DeclarativeBackend for EmailBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let msg = MessageParser::default()
            .parse(&source.bytes)
            .ok_or_else(|| ConversionError::Parse("email: could not parse message".into()))?;
        let mut doc = DoclingDocument::new(&source.name);

        if let Some(subject) = msg.subject().map(str::trim).filter(|s| !s.is_empty()) {
            doc.push(Node::Heading {
                level: 1,
                text: escape_text(subject),
            });
        }
        for (label, addrs) in [("From", msg.from()), ("To", msg.to())] {
            let text = format_addresses(addrs);
            if !text.is_empty() {
                doc.push(Node::Paragraph {
                    text: escape_text(&format!("{label}: {text}")),
                });
            }
        }
        if let Some(date) = msg.date() {
            doc.push(Node::Paragraph {
                text: escape_text(&format!("Date: {}", date.to_rfc3339())),
            });
        }
        for para in body_paragraphs(&msg) {
            doc.push(Node::Paragraph {
                text: escape_text(&para),
            });
        }
        Ok(doc)
    }
}

/// `"Name <email>"` per address (or bare `email`), joined with `", "`.
fn format_addresses(addr: Option<&Address>) -> String {
    let Some(addr) = addr else {
        return String::new();
    };
    addr.iter()
        .filter_map(|a| {
            let name = a.name().map(str::trim).filter(|s| !s.is_empty());
            let email = a.address().map(str::trim).filter(|s| !s.is_empty());
            match (name, email) {
                (Some(n), Some(e)) => Some(format!("{n} <{e}>")),
                (None, Some(e)) => Some(e.to_string()),
                _ => None,
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Body paragraphs (split on blank lines), preferring `text/plain`.
fn body_paragraphs(msg: &Message) -> Vec<String> {
    let re = cached_regex!(r"\n\s*\n+");
    let split = |text: &str, out: &mut Vec<String>| {
        for p in re.split(text.trim()) {
            let p = p.trim();
            if !p.is_empty() {
                out.push(p.to_string());
            }
        }
    };
    let mut out = Vec::new();
    let plain = msg.text_body_count();
    if plain > 0 {
        for i in 0..plain {
            if let Some(t) = msg.body_text(i) {
                split(&t.replace("\r\n", "\n"), &mut out);
            }
        }
        return out;
    }
    // No plain text — fall back to the raw HTML body as text (the test corpus is
    // plain-text only; full HTML→Markdown of email bodies is a later refinement).
    for i in 0..msg.html_body_count() {
        if let Some(t) = msg.body_html(i) {
            split(&t.replace("\r\n", "\n"), &mut out);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    #[test]
    fn title_headers_escaped_and_body_split() {
        let eml = "From: Alice <a@x.com>\r\nTo: Bob <b@y.com>\r\nSubject: Hi\r\n\
                   Content-Type: text/plain\r\n\r\nLine one.\r\n\r\nLine two.\r\n";
        let src = SourceDocument::from_bytes("m", InputFormat::Email, eml.as_bytes().to_vec());
        let md = EmailBackend.convert(&src).unwrap().export_to_markdown();
        // angle brackets HTML-escaped; body split into separate paragraphs.
        assert_eq!(
            md.trim(),
            "# Hi\n\nFrom: Alice &lt;a@x.com&gt;\n\nTo: Bob &lt;b@y.com&gt;\n\nLine one.\n\nLine two."
        );
    }
}
