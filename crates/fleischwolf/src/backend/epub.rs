//! EPUB backend — a port of docling's `EpubDocumentBackend`.
//!
//! EPUB is a ZIP of XHTML. `META-INF/container.xml` points at the OPF package;
//! the OPF's `<spine>` gives the reading order over `<manifest>` items. Each
//! spine document's `<body>` is concatenated into one HTML document (internal
//! `*.xhtml#anchor` links rewritten to `#anchor`) which is then converted by the
//! HTML backend — exactly docling's approach.

use roxmltree::Document;

use crate::backend::ooxml::Package;
use crate::backend::{DeclarativeBackend, HtmlBackend};
use crate::error::ConversionError;
use crate::format::InputFormat;
use crate::source::SourceDocument;
use fleischwolf_core::DoclingDocument;

pub struct EpubBackend;

impl DeclarativeBackend for EpubBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let mut pkg = Package::open(&source.bytes)
            .ok_or_else(|| ConversionError::Parse("epub: not a zip".into()))?;

        let container = pkg
            .read("META-INF/container.xml")
            .ok_or_else(|| ConversionError::Parse("epub: no container.xml".into()))?;
        let opf_path = rootfile_path(&container)
            .ok_or_else(|| ConversionError::Parse("epub: no rootfile".into()))?;
        let opf = pkg
            .read(&opf_path)
            .ok_or_else(|| ConversionError::Parse(format!("epub: missing {opf_path}")))?;
        let opf_dir = opf_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");

        let spine =
            spine_files(&opf, opf_dir).map_err(|e| ConversionError::Parse(format!("epub: {e}")))?;

        let mut combined =
            String::from("<!DOCTYPE html><html><head><meta charset=\"utf-8\"/></head><body>");
        let body_re = cached_regex!(r"(?is)<body[^>]*>(.*?)</body>");
        let link_re = cached_regex!(r#"href="([^"]*\.xhtml)(#[^"]*)""#);
        for file in &spine {
            let Some(xhtml) = pkg.read(file) else {
                continue;
            };
            let body = body_re
                .captures(&xhtml)
                .map(|c| c[1].to_string())
                .unwrap_or(xhtml);
            let body = link_re.replace_all(&body, r#"href="$2""#);
            combined.push('\n');
            combined.push_str(&body);
        }
        combined.push_str("\n</body></html>");

        let html =
            SourceDocument::from_bytes(&source.name, InputFormat::Html, combined.into_bytes());
        HtmlBackend.convert(&html)
    }
}

/// `full-path` of the OPF package from `META-INF/container.xml`.
fn rootfile_path(container: &str) -> Option<String> {
    let dom = Document::parse(container).ok()?;
    dom.descendants()
        .find(|n| n.has_tag_name("rootfile"))
        .and_then(|n| n.attribute("full-path"))
        .map(str::to_string)
}

/// Spine reading order resolved to archive paths (`opf_dir/href`).
fn spine_files(opf: &str, opf_dir: &str) -> Result<Vec<String>, String> {
    let dom = Document::parse(opf).map_err(|e| e.to_string())?;
    let mut id_to_href = std::collections::HashMap::new();
    for item in dom.descendants().filter(|n| n.has_tag_name("item")) {
        if let (Some(id), Some(href)) = (item.attribute("id"), item.attribute("href")) {
            id_to_href.insert(id.to_string(), href.to_string());
        }
    }
    let mut files = Vec::new();
    for itemref in dom.descendants().filter(|n| n.has_tag_name("itemref")) {
        if let Some(href) = itemref.attribute("idref").and_then(|id| id_to_href.get(id)) {
            files.push(if opf_dir.is_empty() {
                href.clone()
            } else {
                format!("{opf_dir}/{href}")
            });
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spine_order_resolves_manifest_hrefs() {
        let opf = r#"<package xmlns="http://www.idpf.org/2007/opf">
            <manifest>
              <item id="c2" href="text/two.xhtml"/>
              <item id="c1" href="text/one.xhtml"/>
              <item id="css" href="x.css"/>
            </manifest>
            <spine><itemref idref="c1"/><itemref idref="c2"/></spine>
          </package>"#;
        // reading order follows the spine, not the manifest, with opf_dir joined
        assert_eq!(
            spine_files(opf, "epub").unwrap(),
            vec!["epub/text/one.xhtml", "epub/text/two.xhtml"]
        );
    }

    #[test]
    fn finds_opf_rootfile() {
        let container = r#"<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
            <rootfiles><rootfile full-path="epub/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
          </container>"#;
        assert_eq!(
            rootfile_path(container).as_deref(),
            Some("epub/content.opf")
        );
    }
}
