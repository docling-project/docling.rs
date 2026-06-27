//! EPUB backend — a port of docling's `EpubDocumentBackend`.
//!
//! EPUB is a ZIP of XHTML. `META-INF/container.xml` points at the OPF package;
//! the OPF's `<spine>` gives the reading order over `<manifest>` items. Each
//! spine document's `<body>` is concatenated into one HTML document (internal
//! `*.xhtml#anchor` links rewritten to `#anchor`) which is then converted by the
//! HTML backend — exactly docling's approach.

use std::collections::HashMap;

use roxmltree::Document;

use crate::backend::ooxml::{self, Package};
use crate::backend::{convert_html, DeclarativeBackend, MapImageResolver, NoFetch};
use crate::error::ConversionError;
use crate::source::SourceDocument;
use fleischwolf_core::{DoclingDocument, PictureImage};

pub struct EpubBackend {
    /// When set, `<img>` sources are read out of the EPUB archive and embedded
    /// as [`PictureImage`]s (the analogue of docling's image fetch).
    pub fetch_images: bool,
}

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
        // Images extracted from the archive, keyed by their resolved in-archive
        // path (which each `<img src>` is rewritten to during concatenation).
        let mut images: HashMap<String, PictureImage> = HashMap::new();
        for file in &spine {
            let Some(xhtml) = pkg.read(file) else {
                continue;
            };
            let body = body_re
                .captures(&xhtml)
                .map(|c| c[1].to_string())
                .unwrap_or(xhtml);
            let body = link_re.replace_all(&body, r#"href="$2""#);
            // Each `<img src>` is relative to *this* spine file's directory, so
            // resolve + extract here, before the bodies are flattened together.
            let body = if self.fetch_images {
                let dir = file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                extract_images(&body, dir, &mut pkg, &mut images)
            } else {
                body.into_owned()
            };
            combined.push('\n');
            combined.push_str(&body);
        }
        combined.push_str("\n</body></html>");

        let doc = if self.fetch_images {
            convert_html(&source.name, &combined, &MapImageResolver::new(images))
        } else {
            convert_html(&source.name, &combined, &NoFetch)
        };
        Ok(doc)
    }
}

/// Rewrite each in-archive `<img src>` in `body` to its resolved archive path and
/// read the image bytes into `images`. `data:`/remote sources are left untouched
/// (they stay placeholders for EPUB). `dir` is the spine file's directory.
fn extract_images(
    body: &str,
    dir: &str,
    pkg: &mut Package,
    images: &mut HashMap<String, PictureImage>,
) -> String {
    let img_re = cached_regex!(r"(?is)<img\b[^>]*>");
    let src_re = cached_regex!(r#"(?is)\bsrc\s*=\s*"([^"]*)""#);
    img_re
        .replace_all(body, |caps: &regex::Captures| {
            let tag = &caps[0];
            let Some(raw) = src_re.captures(tag).map(|m| m[1].to_string()) else {
                return tag.to_string();
            };
            if raw.is_empty()
                || raw.starts_with("data:")
                || raw.starts_with("http://")
                || raw.starts_with("https://")
            {
                return tag.to_string();
            }
            let archive_path = ooxml::resolve(dir, &raw);
            if !images.contains_key(&archive_path) {
                if let Some(pic) = pkg
                    .read_bytes(&archive_path)
                    .and_then(|bytes| ooxml::picture_image(&archive_path, bytes))
                {
                    images.insert(archive_path.clone(), pic);
                }
            }
            src_re
                .replace(tag, format!(r#"src="{archive_path}""#).as_str())
                .into_owned()
        })
        .into_owned()
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

    #[test]
    fn extracts_archive_images_only_when_fetching() {
        use crate::format::InputFormat;
        use fleischwolf_core::{DoclingDocument, Node};

        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/data/epub/sources/epub_purvis_poetry.epub"
        );
        // The corpus lives outside the packaged crate; skip if it isn't present.
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let src = SourceDocument::from_bytes("epub_purvis_poetry", InputFormat::Epub, bytes);

        let embedded = |doc: &DoclingDocument| {
            doc.nodes
                .iter()
                .filter_map(|n| match n {
                    Node::Picture {
                        image: Some(img), ..
                    } => Some(img),
                    _ => None,
                })
                .cloned()
                .collect::<Vec<_>>()
        };

        // Default: pictures stay placeholders (no archive reads).
        let plain = EpubBackend {
            fetch_images: false,
        }
        .convert(&src)
        .unwrap();
        assert!(embedded(&plain).is_empty());

        // Fetching: real image bytes are pulled out of the archive.
        let fetched = EpubBackend { fetch_images: true }.convert(&src).unwrap();
        let imgs = embedded(&fetched);
        assert!(!imgs.is_empty(), "expected extracted archive images");
        assert!(imgs
            .iter()
            .all(|img| img.width > 0 && img.height > 0 && !img.data.is_empty()));
    }
}
