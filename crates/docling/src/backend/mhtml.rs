//! MHTML (`.mhtml`/`.mht`) backend — a **docling.rs extension**; docling has
//! no MHTML backend to port.
//!
//! An MHTML archive is a MIME message ([RFC 2557], which `mail-parser`
//! conforms to): a `multipart/related` structure whose root part is the saved
//! page's `text/html`, with its resources (images, CSS, fonts) as sibling
//! parts addressed by `Content-Location` (the resource's original URL) or
//! `Content-ID` (referenced from the HTML as `cid:...`). This backend extracts
//! the root HTML and hands it to the HTML backend for full Markdown
//! extraction; `<img src>` references are resolved against the archive's own
//! parts and embedded by default — unlike standalone HTML/EPUB image fetching
//! (gated behind `fetch_images`), resolving here reads no filesystem/network,
//! just the same MIME bytes already parsed, so there is no separate opt-in
//! (matching how DOCX/PPTX embed their blobs by default).
//!
//! [RFC 2557]: https://datatracker.ietf.org/doc/html/rfc2557

use std::collections::HashMap;

use mail_parser::{MessageParser, MimeHeaders};

use crate::backend::images::build_picture;
use crate::backend::{convert_html, maybe_prerender_html, DeclarativeBackend, MapImageResolver};
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::{DoclingDocument, PictureImage};

#[derive(Default)]
pub struct MhtmlBackend {
    /// Pre-render the extracted page HTML in a headless browser first (mirrors
    /// [`crate::DocumentConverter::use_web_browser`]).
    pub use_web_browser: bool,
}

impl DeclarativeBackend for MhtmlBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let msg = MessageParser::default()
            .parse(&source.bytes)
            .ok_or_else(|| ConversionError::Parse("mhtml: could not parse MIME message".into()))?;

        // The saved page: the first (and normally only) `text/html` part. A
        // resource-only archive with no HTML root yields an empty document,
        // same as other backends' graceful handling of unusable input.
        let Some(html) = msg.html_bodies().next().and_then(|p| p.text_contents()) else {
            return Ok(DoclingDocument::new(&source.name));
        };

        let html = maybe_prerender_html(html, self.use_web_browser)?;
        let images = collect_images(&msg);
        Ok(convert_html(
            &source.name,
            &html,
            &MapImageResolver::new(images),
        ))
    }
}

/// Every image sub-part, keyed by however the root HTML addresses it: its
/// original URL (`Content-Location`, matching a rewritten `<img src>` verbatim)
/// and/or its `cid:<Content-ID>` form.
fn collect_images(msg: &mail_parser::Message) -> HashMap<String, PictureImage> {
    let mut images = HashMap::new();
    for part in &msg.parts {
        if part.is_multipart() || part.is_message() {
            continue;
        }
        let Some(ct) = part.content_type() else {
            continue;
        };
        let Some(subtype) = ct.subtype() else {
            continue;
        };
        if !ct.ctype().eq_ignore_ascii_case("image") {
            continue;
        }
        let mimetype = format!("{}/{}", ct.ctype(), subtype);
        let Some(pic) = build_picture(mimetype, part.contents().to_vec()) else {
            // Vector/unsupported-by-`image` formats (e.g. `image/svg+xml`)
            // have no decodable raster dimensions; leave the reference
            // unresolved rather than embed a dimensionless image.
            continue;
        };
        if let Some(loc) = part.content_location() {
            images.insert(loc.to_string(), pic.clone());
        }
        if let Some(id) = part.content_id() {
            images.insert(format!("cid:{}", id.trim_matches(['<', '>'])), pic);
        }
    }
    images
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;
    use docling_core::Node;

    const RED_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8,
        0xcf, 0xc0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x6e, 0x2c, 0xdc, 0x33, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    fn mhtml(html_part: &str, extra_parts: &str) -> Vec<u8> {
        format!(
            "MIME-Version: 1.0\r\n\
             Content-Type: multipart/related; boundary=\"B\"\r\n\r\n\
             --B\r\nContent-Type: text/html\r\nContent-Location: https://example.com/\r\n\r\n\
             {html_part}\r\n\
             {extra_parts}--B--\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn extracts_heading_and_paragraph_from_root_html() {
        let bytes = mhtml(
            "<html><body><h1>Title</h1><p>Body text.</p></body></html>",
            "",
        );
        let src = SourceDocument::from_bytes("p", InputFormat::Mhtml, bytes);
        let md = MhtmlBackend::default()
            .convert(&src)
            .unwrap()
            .export_to_markdown();
        assert_eq!(md.trim(), "# Title\n\nBody text.");
    }

    #[test]
    fn resolves_image_by_content_location() {
        let png_b64 = docling_core::base64::encode(RED_PNG);
        let extra = format!(
            "--B\r\nContent-Type: image/png\r\nContent-Location: https://example.com/pic.png\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n{png_b64}\r\n"
        );
        let bytes = mhtml(
            r#"<html><body><img src="https://example.com/pic.png"></body></html>"#,
            &extra,
        );
        let src = SourceDocument::from_bytes("p", InputFormat::Mhtml, bytes);
        let doc = MhtmlBackend::default().convert(&src).unwrap();
        let img = doc.nodes.iter().find_map(|n| match n {
            Node::Picture { image, .. } => image.as_ref(),
            _ => None,
        });
        let img = img.expect("image resolved from the archive");
        assert_eq!(img.mimetype, "image/png");
        assert_eq!((img.width, img.height), (1, 1));
        assert_eq!(img.data, RED_PNG);
    }

    #[test]
    fn resolves_image_by_content_id_cid_reference() {
        let png_b64 = docling_core::base64::encode(RED_PNG);
        let extra = format!(
            "--B\r\nContent-Type: image/png\r\nContent-ID: <img1@mhtml.blink>\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n{png_b64}\r\n"
        );
        let bytes = mhtml(
            r#"<html><body><img src="cid:img1@mhtml.blink"></body></html>"#,
            &extra,
        );
        let src = SourceDocument::from_bytes("p", InputFormat::Mhtml, bytes);
        let doc = MhtmlBackend::default().convert(&src).unwrap();
        let embedded = doc
            .nodes
            .iter()
            .any(|n| matches!(n, Node::Picture { image: Some(_), .. }));
        assert!(embedded, "cid: reference resolved");
    }

    #[test]
    fn plain_text_only_falls_back_via_mail_parsers_html_conversion() {
        // mail-parser synthesizes an HTML alternative from a lone text/plain
        // part, so even a resource-only archive still yields readable text.
        let bytes = b"MIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\nplain text\r\n".to_vec();
        let src = SourceDocument::from_bytes("p", InputFormat::Mhtml, bytes);
        let md = MhtmlBackend::default()
            .convert(&src)
            .unwrap()
            .export_to_markdown();
        assert_eq!(md.trim(), "plain text");
    }

    #[test]
    fn unparseable_bytes_yield_empty_document() {
        // Not a MIME message at all (no headers, no blank-line separator): the
        // parser still returns a message (mail-parser is liberal), but with no
        // html/text body to extract.
        let src =
            SourceDocument::from_bytes("p", InputFormat::Mhtml, b"not a mime message".to_vec());
        let doc = MhtmlBackend::default().convert(&src).unwrap();
        assert!(doc.nodes.is_empty());
    }
}
