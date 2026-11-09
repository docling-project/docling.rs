//! EPUB backend — a port of docling's `EpubDocumentBackend`.
//!
//! EPUB is a ZIP of XHTML. `META-INF/container.xml` points at the OPF package;
//! the OPF's `<spine>` gives the reading order over `<manifest>` items. Each
//! spine document's `<body>` is concatenated into one HTML document (internal
//! `*.xhtml#anchor` links rewritten to `#anchor`) which is then converted by the
//! HTML backend — exactly docling's approach.

use std::collections::HashMap;

use roxmltree::Document;

use crate::backend::images::ImagePolicy;
use crate::backend::ooxml::{self, Package};
use crate::backend::{
    convert_html, maybe_prerender_html, DeclarativeBackend, MapImageResolver, NoFetch,
};
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::{DoclingDocument, PictureImage};

#[derive(Default)]
pub struct EpubBackend {
    /// Under [`ImageSources::Embedded`] and above, `<img>` sources are read
    /// out of the EPUB archive and embedded as [`PictureImage`]s (the
    /// analogue of docling's image fetch; #646 — an archive entry is a part
    /// of the same container, no filesystem or network is touched).
    pub images: ImagePolicy,
    /// Pre-render the concatenated spine HTML in a headless browser first
    /// (mirrors [`crate::DocumentConverter::use_web_browser`]).
    pub use_web_browser: bool,
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
        // Images extracted from the archive, keyed by their resolved in-archive
        // path (which each `<img src>` is rewritten to during concatenation).
        let mut images: HashMap<String, PictureImage> = HashMap::new();
        for file in &spine {
            let Some(xhtml) = read_content(&mut pkg, file) else {
                continue;
            };
            let body = body_re
                .captures(&xhtml)
                .map(|c| c[1].to_string())
                .unwrap_or(xhtml);
            let body = rewrite_internal_links(&body);
            // Each `<img src>` is relative to *this* spine file's directory, so
            // resolve + extract here, before the bodies are flattened together.
            let body = if self.images.sources.embedded() {
                let dir = file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                extract_images(&body, dir, &mut pkg, &mut images)
            } else {
                body.into_owned()
            };
            combined.push('\n');
            combined.push_str(&body);
        }
        combined.push_str("\n</body></html>");

        let combined = maybe_prerender_html(&combined, self.use_web_browser)?;
        let doc = if self.images.sources.embedded() {
            convert_html(
                &source.name,
                &combined,
                &MapImageResolver::new(images, &self.images),
            )
        } else {
            convert_html(&source.name, &combined, &NoFetch)
        };
        Ok(doc)
    }
}

/// A link into another content document is reduced to its anchor: the file
/// it names is merged into this one. A content document is XHTML by its
/// manifest media-type, not its name, so `.xht`, `.htm` and `.html`
/// (Calibre's choice) count too (docling#4293); a link with a scheme or a
/// protocol-relative one belongs to a host and is left alone. An attribute
/// value is as legal in single quotes as in double ones, so both are matched
/// and the link keeps its own quote (docling#5170) — the `regex` crate has no
/// backreference for upstream's `(["'])…\1`, hence one alternative per quote.
fn rewrite_internal_links(body: &str) -> std::borrow::Cow<'_, str> {
    let link_re = cached_regex!(
        r#"href=(?:"([^"]*\.(?:xhtml|xht|html?))(#[^"]*)"|'([^']*\.(?:xhtml|xht|html?))(#[^']*)')"#
    );
    link_re.replace_all(body, |caps: &regex::Captures| {
        let (q, file, frag) = match caps.get(1) {
            Some(file) => ('"', file.as_str(), &caps[2]),
            None => ('\'', &caps[3], &caps[4]),
        };
        if is_external_href(file) {
            caps[0].to_string()
        } else {
            format!("href={q}{frag}{q}")
        }
    })
}

/// Rewrite each in-archive `<img src>` in `body` to its resolved archive path and
/// read the image bytes into `images`. `data:`/remote sources are left untouched
/// (they stay placeholders for EPUB). `dir` is the spine file's directory.
/// A spine content document as text: UTF-8, or UTF-16 when it opens with a
/// Whether an href leaves the book: a scheme (`https:`, `mailto:`) or a
/// protocol-relative `//host` — upstream's `(?!\w+:|//)`.
fn is_external_href(href: &str) -> bool {
    if href.starts_with("//") {
        return true;
    }
    let scheme_len = href
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
        .count();
    scheme_len > 0 && href[scheme_len..].starts_with(':')
}

/// UTF-16 byte order mark (docling#4351 — `xhtml_data.decode("utf-8")` used to
/// drop such a file). The BOM itself is not part of the text.
fn read_content(pkg: &mut Package, path: &str) -> Option<String> {
    let text = decode_content(pkg.read_bytes(path)?)?;
    if let Err(e) = super::xml_depth::check(&text, path) {
        eprintln!("docling: {e}; part skipped");
        return None;
    }
    Some(text)
}

/// docling's `_decode_content_file`: UTF-16 behind a UTF-16 BOM, else UTF-8.
fn decode_content(bytes: Vec<u8>) -> Option<String> {
    Some(match bytes.as_slice() {
        [0xFF, 0xFE, rest @ ..] | [0xFE, 0xFF, rest @ ..] => {
            let big_endian = bytes[0] == 0xFE;
            let units: Vec<u16> = rest
                .chunks_exact(2)
                .map(|c| {
                    if big_endian {
                        u16::from_be_bytes([c[0], c[1]])
                    } else {
                        u16::from_le_bytes([c[0], c[1]])
                    }
                })
                .collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::from_utf8(bytes).ok()?,
    })
}

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

/// `full-path` of the OPF package from `META-INF/container.xml`. It is a
/// URL (a relative IRI reference inside the container) while the archive
/// stores the literal file name, so its percent-escapes are decoded like the
/// manifest hrefs' (docling#5168): `OEBPS/my%20book.opf` names
/// `OEBPS/my book.opf`.
pub(crate) fn rootfile_path(container: &str) -> Option<String> {
    let dom = Document::parse(container).ok()?;
    dom.descendants()
        .find(|n| n.has_tag_name("rootfile"))
        .and_then(|n| n.attribute("full-path"))
        .map(percent_decode)
}

/// Spine reading order resolved to archive paths (`opf_dir/href`).
fn spine_files(opf: &str, opf_dir: &str) -> Result<Vec<String>, String> {
    let dom = Document::parse(opf).map_err(|e| e.to_string())?;
    let mut id_to_href = std::collections::HashMap::new();
    for item in dom.descendants().filter(|n| n.has_tag_name("item")) {
        if let (Some(id), Some(href)) = (item.attribute("id"), item.attribute("href")) {
            // A manifest href is a URL while the archive stores the literal
            // file name, so percent-escapes are decoded here (docling#4199).
            id_to_href.insert(id.to_string(), percent_decode(href));
        }
    }
    let mut files = Vec::new();
    for itemref in dom.descendants().filter(|n| n.has_tag_name("itemref")) {
        if let Some(href) = itemref.attribute("idref").and_then(|id| id_to_href.get(id)) {
            // `posixpath.normpath(posixpath.join(opf_dir, href))` (docling#4261,
            // 2.129): an href that steps out of the package directory
            // (`../Text/ch1.xhtml` from `OEBPS/`) resolves to the archive
            // path it names instead of a literal `OEBPS/../Text/…` no entry
            // matches.
            files.push(normpath(&if opf_dir.is_empty() {
                href.clone()
            } else {
                format!("{opf_dir}/{href}")
            }));
        }
    }
    Ok(files)
}

/// Python's `posixpath.normpath`: collapse `//` and `.`, resolve `..` against
/// the preceding segment (a leading `..` that cannot be resolved is kept, as
/// is a leading `/`).
fn normpath(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => match out.last() {
                Some(&last) if last != ".." => {
                    out.pop();
                }
                _ if absolute => {}
                _ => out.push(".."),
            },
            s => out.push(s),
        }
    }
    let joined = out.join("/");
    match (absolute, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    }
}

/// Decode `%XX` escapes (UTF-8 bytes) in a manifest href — `urllib.parse.unquote`
/// for the escapes that matter to an archive path; a malformed escape is kept
/// verbatim, and invalid UTF-8 is replaced rather than rejected.
fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use crate::backend::images::ImageSources;
    #[test]
    fn utf16_content_documents_decode_behind_their_bom() {
        // docling#4351: a UTF-16 content document used to be dropped.
        let le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("<p>hé</p>".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(super::decode_content(le).as_deref(), Some("<p>hé</p>"));
        let be: Vec<u8> = [0xFE, 0xFF]
            .into_iter()
            .chain("<p>x</p>".encode_utf16().flat_map(u16::to_be_bytes))
            .collect();
        assert_eq!(super::decode_content(be).as_deref(), Some("<p>x</p>"));
        assert_eq!(
            super::decode_content(b"<p>y</p>".to_vec()).as_deref(),
            Some("<p>y</p>")
        );
        assert_eq!(super::decode_content(vec![0xC3, 0x28]), None);
    }

    use super::*;

    /// docling#4261 (2.129): a manifest href stepping out of the OPF directory
    /// resolves like `posixpath.normpath`.
    #[test]
    fn manifest_hrefs_stepping_out_of_the_opf_dir_normalize() {
        assert_eq!(normpath("OEBPS/../Text/ch1.xhtml"), "Text/ch1.xhtml");
        assert_eq!(normpath("OEBPS/./a//b.xhtml"), "OEBPS/a/b.xhtml");
        assert_eq!(normpath("../x.xhtml"), "../x.xhtml");
        assert_eq!(normpath("a/.."), ".");
        let opf = r#"<package><manifest><item id="a" href="../Text/ch1.xhtml"/></manifest>
                     <spine><itemref idref="a"/></spine></package>"#;
        assert_eq!(
            spine_files(opf, "OEBPS").unwrap(),
            vec!["Text/ch1.xhtml".to_string()]
        );
    }

    /// docling#4199: manifest hrefs are URLs, the archive holds the literal
    /// names — `chapter%201.xhtml` is `chapter 1.xhtml` in the zip.
    #[test]
    fn manifest_hrefs_are_percent_decoded() {
        let opf = r#"<package xmlns="http://www.idpf.org/2007/opf">
            <manifest>
              <item id="a" href="chapter%201.xhtml"/>
              <item id="b" href="%C3%A9pilogue.xhtml"/>
              <item id="c" href="plain.xhtml"/>
              <item id="d" href="odd%2.xhtml"/>
            </manifest>
            <spine><itemref idref="a"/><itemref idref="b"/><itemref idref="c"/><itemref idref="d"/></spine>
          </package>"#;
        assert_eq!(
            spine_files(opf, "").unwrap(),
            vec![
                "chapter 1.xhtml",
                "\u{e9}pilogue.xhtml",
                "plain.xhtml",
                "odd%2.xhtml"
            ]
        );
    }

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

    /// docling#4293: a cross-document link is reduced to its anchor whatever
    /// the content document's extension; a link to a host is left alone.
    #[test]
    fn internal_links_of_every_extension_become_anchors() {
        let fix = |html: &str| -> String { rewrite_internal_links(html).into_owned() };
        for ext in ["xhtml", "html", "htm", "xht"] {
            assert_eq!(
                fix(&format!("<a href=\"../text/ch2.{ext}#n1\">")),
                "<a href=\"#n1\">"
            );
        }
        assert_eq!(
            fix("<a href=\"https://example.com/page.html#about\">"),
            "<a href=\"https://example.com/page.html#about\">"
        );
        assert_eq!(
            fix("<a href=\"//example.com/page.html#about\">"),
            "<a href=\"//example.com/page.html#about\">"
        );
        assert_eq!(
            fix("<a href=\"mailto:a@example.com\">"),
            "<a href=\"mailto:a@example.com\">"
        );
    }

    #[test]
    fn extracts_archive_images_only_when_fetching() {
        use crate::format::InputFormat;
        use docling_core::{DoclingDocument, Node};

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
        let plain = EpubBackend::default().convert(&src).unwrap();
        assert!(embedded(&plain).is_empty());

        // Fetching: real image bytes are pulled out of the archive.
        let fetched = EpubBackend {
            images: ImagePolicy::new(ImageSources::Embedded),
            ..Default::default()
        }
        .convert(&src)
        .unwrap();
        let imgs = embedded(&fetched);
        assert!(!imgs.is_empty(), "expected extracted archive images");
        assert!(imgs
            .iter()
            .all(|img| img.width > 0 && img.height > 0 && !img.data.is_empty()));
    }

    /// An in-memory EPUB: `container.xml` naming `opf_ref`, the package
    /// stored at `opf_name`, and one spine item per `(file, body)`.
    fn build_epub(opf_ref: &str, opf_name: &str, chapters: &[(&str, &str)]) -> Vec<u8> {
        use std::io::Write;
        let mut out = std::io::Cursor::new(Vec::new());
        let mut w = zip::ZipWriter::new(&mut out);
        let opts = zip::write::SimpleFileOptions::default();
        let mut put = |name: &str, body: &str| {
            w.start_file(name, opts).unwrap();
            w.write_all(body.as_bytes()).unwrap();
        };
        put("mimetype", "application/epub+zip");
        put(
            "META-INF/container.xml",
            &format!(
                r#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="{opf_ref}" media-type="application/oebps-package+xml"/></rootfiles></container>"#
            ),
        );
        let dir = opf_name.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let (mut manifest, mut spine) = (String::new(), String::new());
        for (i, (file, body)) in chapters.iter().enumerate() {
            manifest.push_str(&format!(
                r#"<item id="c{i}" href="{file}" media-type="application/xhtml+xml"/>"#
            ));
            spine.push_str(&format!(r#"<itemref idref="c{i}"/>"#));
            put(
                &format!("{dir}/{file}"),
                &format!(
                    r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><body>{body}</body></html>"#
                ),
            );
        }
        put(
            opf_name,
            &format!(
                r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata/><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
            ),
        );
        w.finish().unwrap();
        out.into_inner()
    }

    fn convert_epub(bytes: Vec<u8>) -> Result<DoclingDocument, ConversionError> {
        use crate::format::InputFormat;
        let src = SourceDocument::from_bytes("book", InputFormat::Epub, bytes);
        EpubBackend::default().convert(&src)
    }

    /// docling#5170 (`test_epub_internal_links_in_either_quote_style`): a
    /// single-quoted cross-chapter link is reduced to its anchor like a
    /// double-quoted one, and a single-quoted absolute URL keeps its host.
    #[test]
    fn internal_links_in_either_quote_style() {
        let abs = "https://example.com/page.html#about";
        let bytes = build_epub(
            "OEBPS/content.opf",
            "OEBPS/content.opf",
            &[
                (
                    "chapter1.xhtml",
                    &format!(
                        "<p>See <a href='chapter2.xhtml#note2'>single</a>.</p>\
                         <p>See <a href=\"chapter2.xhtml#note2\">double</a>.</p>\
                         <p>Read <a href='{abs}'>about</a>.</p>"
                    ),
                ),
                ("chapter2.xhtml", r#"<p id="note2">Note 2.</p>"#),
            ],
        );
        let md = convert_epub(bytes).unwrap().export_to_markdown();
        assert!(md.contains("[single](#note2)"), "{md}");
        assert!(md.contains("[double](#note2)"), "{md}");
        assert!(md.contains(&format!("[about]({abs})")), "{md}");
        assert!(!md.contains("chapter2.xhtml"), "{md}");

        assert_eq!(
            rewrite_internal_links("<a href='../t/ch2.html#n1'>"),
            "<a href='#n1'>"
        );
        assert_eq!(
            rewrite_internal_links("<a href='//example.com/p.htm#x'>"),
            "<a href='//example.com/p.htm#x'>"
        );
    }

    /// docling#5168 (`test_epub_percent_encoded_package_path_is_read`): the
    /// rootfile's `full-path` is a URL, so `my%20book.opf` reads the
    /// archive's `my book.opf` instead of failing the conversion.
    #[test]
    fn percent_encoded_package_path_is_read() {
        let bytes = build_epub(
            "OEBPS/my%20book.opf",
            "OEBPS/my book.opf",
            &[("chapter-0.xhtml", "<p>Chapter 0 body.</p>")],
        );
        let md = convert_epub(bytes).unwrap().export_to_markdown();
        assert!(md.contains("Chapter 0 body."), "{md}");
        assert_eq!(
            rootfile_path(
                r#"<container><rootfiles><rootfile full-path="OEBPS/my%20book.opf"/></rootfiles></container>"#
            )
            .as_deref(),
            Some("OEBPS/my book.opf")
        );
    }
}
