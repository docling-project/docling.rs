//! A third-party input format without a fork (#680): a backend for `.rtf2`,
//! a hypothetical line-oriented markup, registered on a `DocumentConverter`.
//!
//! ```text
//! cargo run -p docling --example register_backend -- notes.rtf2
//! ```
//!
//! An `.rtf2` file is lines of `\h1 Title`, `\h2 Section`, `\p A paragraph.`
//! and `\li An item.`; it opens with the signature `{\rtf2}`, so a file that
//! arrives without its name (stdin, an upload called `blob`) is recognised
//! by its content too. The backend builds a `DoclingDocument`; everything
//! after that — Markdown, JSON, HTML, chunking, redaction — is docling.rs'.

use std::sync::Arc;

use docling::backend::DeclarativeBackend;
use docling::{BackendMatcher, ConversionError, DocumentConverter, SourceDocument};
use docling_core::{DoclingDocument, Node};

/// The `.rtf2` reader.
struct Rtf2Backend;

impl DeclarativeBackend for Rtf2Backend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let text = source.text()?;
        let mut doc = DoclingDocument::new(&source.name);
        let mut first_item = true;
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let (tag, rest) = line.split_once(' ').unwrap_or((line, ""));
            let rest = rest.trim().to_string();
            match tag {
                "{\\rtf2}" => continue,
                "\\h1" => doc.push(Node::Heading {
                    level: 1,
                    text: rest,
                }),
                "\\h2" => doc.push(Node::Heading {
                    level: 2,
                    text: rest,
                }),
                "\\li" => {
                    doc.push(Node::ListItem {
                        ordered: false,
                        number: 0,
                        first_in_list: first_item,
                        text: rest,
                        level: 0,
                        marker: None,
                        location: None,
                        split: None,
                        dclx: None,
                        href: None,
                        layer: None,
                    });
                    first_item = false;
                    continue;
                }
                "\\p" => doc.push(Node::Paragraph { text: rest }),
                other => {
                    return Err(ConversionError::Parse(format!(
                        "{}: unknown rtf2 tag {other:?}",
                        source.name
                    )))
                }
            }
            first_item = true;
        }
        Ok(doc)
    }
}

/// The converter with the `.rtf2` backend registered.
fn converter() -> DocumentConverter {
    DocumentConverter::new().register_backend(
        "rtf2",
        BackendMatcher::new()
            .extensions(["rtf2"])
            .media_types(["application/x-rtf2"])
            .sniff(|bytes| bytes.starts_with(b"{\\rtf2}")),
        Arc::new(Rtf2Backend),
        0,
    )
}

const SAMPLE: &str = "{\\rtf2}\n\\h1 Release notes\n\\p docling.rs reads formats it was never taught.\n\\h2 Added\n\\li Pluggable backends\n\\li An rtf2 example\n";

fn main() -> Result<(), ConversionError> {
    let converter = converter();
    let source = match std::env::args().nth(1) {
        Some(path) => converter.load_file(path)?,
        None => converter.load_named_bytes("sample.rtf2", SAMPLE.as_bytes().to_vec(), None)?,
    };
    let result = converter.convert(source)?;
    print!("{}", result.document.export_to_markdown());
    eprintln!(
        "converted as {} by `{}`; this converter takes: {}",
        result.format.as_str(),
        converter.registered_backends()[0].name(),
        converter
            .supported_extensions()
            .iter()
            .filter(|e| e.starts_with("rt"))
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtf2_converts_by_name_media_type_and_content() {
        let c = converter();
        let expected = "# Release notes\n\ndocling.rs reads formats it was never taught.\n\n## Added\n\n- Pluggable backends\n- An rtf2 example\n";
        for source in [
            c.load_named_bytes("notes.rtf2", SAMPLE.into(), None)
                .unwrap(),
            c.load_named_bytes("upload", SAMPLE.into(), Some("application/x-rtf2"))
                .unwrap(),
            c.load_named_bytes("blob", SAMPLE.into(), None).unwrap(),
        ] {
            let r = c.convert(source).unwrap();
            assert_eq!(r.format.as_str(), "plugin");
            assert_eq!(r.document.export_to_markdown(), expected);
        }
        assert!(c.supported_extensions().contains(&"rtf2".to_string()));
    }
}
