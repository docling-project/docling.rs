//! Shared Office-Open-XML (OOXML) helpers for the DOCX/PPTX/XLSX backends.
//!
//! An OOXML file is a ZIP of XML "parts" plus `.rels` files that wire parts
//! together by relationship id. This module wraps the ZIP, parses relationship
//! files, resolves the (possibly `../`-relative) part paths, and counts embedded
//! pictures in a drawing part.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use docling_core::PictureImage;
use quick_xml::events::Event;
use quick_xml::Reader;
use zip::ZipArchive;

/// Hard cap on a single decompressed OOXML part (512 MiB). Real documents
/// stay far below this; the limit only exists to stop a decompression-bomb
/// part from exhausting memory. Override with `DOCLING_RS_MAX_PART_BYTES`.
pub(crate) fn max_part_bytes() -> u64 {
    std::env::var("DOCLING_RS_MAX_PART_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(512 * 1024 * 1024)
}

/// A read-only view over the parts of an OOXML package.
///
/// Cloning is cheap (the file bytes are shared behind an `Arc`, the ZIP
/// central directory is reference-counted by the `zip` crate), which gives
/// each rayon worker its own independent cursor over the same archive.
#[derive(Clone)]
pub struct Package {
    zip: ZipArchive<Cursor<std::sync::Arc<[u8]>>>,
}

impl Package {
    pub fn open(bytes: &[u8]) -> Option<Self> {
        ZipArchive::new(Cursor::new(std::sync::Arc::from(bytes)))
            .ok()
            .map(|zip| Self { zip })
    }

    /// Read a part to a string, or `None` if it is absent or not valid UTF-8.
    pub fn read(&mut self, path: &str) -> Option<String> {
        let bytes = self.read_bytes(path)?;
        String::from_utf8(bytes).ok()
    }

    /// Read a part's raw bytes (e.g. an embedded image), or `None` if absent.
    ///
    /// A single part is never allowed to inflate past [`max_part_bytes`]: an
    /// OOXML file is a ZIP, and a "zip bomb" part (a few KB deflating to many
    /// GB) would otherwise exhaust memory and abort the process. Reads stop at
    /// the cap and the oversized part is rejected (`None`) rather than
    /// truncated, so a partial part never reaches an XML parser.
    pub fn read_bytes(&mut self, path: &str) -> Option<Vec<u8>> {
        self.read_bytes_capped(path, max_part_bytes())
    }

    fn read_bytes_capped(&mut self, path: &str, cap: u64) -> Option<Vec<u8>> {
        let file = self.zip.by_name(path).ok()?;
        // Reject up front when the central directory already advertises an
        // oversized part; still cap the actual read in case the header lies.
        if file.size() > cap {
            return None;
        }
        let mut out = Vec::new();
        // read_to_end on a `.take(cap + 1)` reader: if it returns cap+1 bytes,
        // the part exceeded the cap and is rejected rather than truncated.
        file.take(cap + 1).read_to_end(&mut out).ok()?;
        if out.len() as u64 > cap {
            return None;
        }
        Some(out)
    }

    /// Map each `/image` relationship id of `part` to its extracted
    /// [`PictureImage`] (`base_dir` is the part's directory for resolving
    /// targets, e.g. `word` / `ppt`). Unreadable or undecodable images are skipped.
    pub fn image_rels(&mut self, part: &str, base_dir: &str) -> HashMap<String, PictureImage> {
        let rels = self.rels_for(part);
        let mut out = HashMap::new();
        for r in &rels {
            if !r.rel_type.ends_with("/image") {
                continue;
            }
            let path = resolve(base_dir, &r.target);
            if let Some(bytes) = self.read_bytes(&path) {
                if let Some(img) = picture_image(&path, bytes) {
                    out.insert(r.id.clone(), img);
                }
            }
        }
        out
    }

    /// The `.rels` file governing `part` (e.g. `xl/worksheets/sheet1.xml` →
    /// `xl/worksheets/_rels/sheet1.xml.rels`), parsed into relationships.
    pub fn rels_for(&mut self, part: &str) -> Vec<Relationship> {
        let (dir, file) = split_path(part);
        let rels_path = if dir.is_empty() {
            format!("_rels/{file}.rels")
        } else {
            format!("{dir}/_rels/{file}.rels")
        };
        self.read(&rels_path)
            .map(|x| parse_rels(&x))
            .unwrap_or_default()
    }
}

/// A single `<Relationship Id Type Target>` entry from a `.rels` part.
pub struct Relationship {
    pub id: String,
    pub rel_type: String,
    pub target: String,
}

/// Parse a `.rels` XML document into its relationships.
pub fn parse_rels(xml: &str) -> Vec<Relationship> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.name().as_ref() == b"Relationship" => {
                let (mut id, mut rel_type, mut target) =
                    (String::new(), String::new(), String::new());
                for attr in e.attributes().flatten() {
                    let value = String::from_utf8_lossy(attr.value.as_ref()).into_owned();
                    match attr.key.as_ref() {
                        b"Id" => id = value,
                        b"Type" => rel_type = value,
                        b"Target" => target = value,
                        _ => {}
                    }
                }
                out.push(Relationship {
                    id,
                    rel_type,
                    target,
                });
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Split a part path into its directory and file name.
fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

/// Resolve a relationship `target` against the directory of the part that owns
/// the `.rels`, collapsing `.` / `..` segments. A leading `/` is package-absolute.
pub fn resolve(base_dir: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut parts: Vec<&str> = base_dir.split('/').filter(|p| !p.is_empty()).collect();
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Build a [`PictureImage`] from image bytes, reading the pixel size from the
/// header (decode-free). Returns `None` for formats the `image` crate can't read
/// (e.g. EMF/WMF vector media).
pub fn picture_image(path: &str, data: Vec<u8>) -> Option<PictureImage> {
    let (width, height) = image::ImageReader::new(Cursor::new(&data))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    Some(PictureImage {
        mimetype: mime_for(path).to_string(),
        width,
        height,
        data,
    })
}

fn mime_for(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "webp" => "image/webp",
        _ => "image/png",
    }
}

/// The content type of a package part, resolved from `[Content_Types].xml`
/// (an exact `<Override PartName>` wins over a `<Default Extension>`).
pub fn content_type(content_types_xml: &str, part: &str) -> Option<String> {
    let mut reader = Reader::from_str(content_types_xml);
    let mut buf = Vec::new();
    let want_part = format!("/{}", part.trim_start_matches('/'));
    let ext = part.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let mut default: Option<String> = None;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) => {
                let tag = e.name();
                let attr = |key: &[u8]| -> Option<String> {
                    e.attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == key)
                        .map(|a| String::from_utf8_lossy(a.value.as_ref()).into_owned())
                };
                match tag.as_ref() {
                    b"Override" => {
                        if attr(b"PartName").as_deref() == Some(want_part.as_str()) {
                            return attr(b"ContentType");
                        }
                    }
                    b"Default"
                        if attr(b"Extension")
                            .map(|x| x.to_ascii_lowercase())
                            .as_deref()
                            == Some(ext.as_str()) =>
                    {
                        default = attr(b"ContentType");
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    default
}

#[cfg(test)]
mod zip_bomb_tests {
    use super::Package;
    use std::io::Write;

    /// A minimal in-memory OOXML-style zip with one part of `part_len` bytes.
    fn zip_with_part(name: &str, part_len: usize) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zw.start_file(name, opts).unwrap();
            zw.write_all(&vec![b'a'; part_len]).unwrap();
            zw.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn oversized_part_is_rejected_not_truncated() {
        // A highly compressible 1 MiB part in a tiny zip (deflates to ~1 KB):
        // the decompression-bomb shape. With the cap below its size, the read
        // must return None rather than a truncated buffer.
        let bytes = zip_with_part("word/document.xml", 1024 * 1024);
        assert!(bytes.len() < 64 * 1024, "part should compress tiny");
        let mut pkg = Package::open(&bytes).unwrap();
        assert!(
            pkg.read_bytes_capped("word/document.xml", 4096).is_none(),
            "a part over the cap must be rejected"
        );
        // Under a generous cap it reads back in full.
        let out = pkg
            .read_bytes_capped("word/document.xml", 8 * 1024 * 1024)
            .expect("part under the cap reads");
        assert_eq!(out.len(), 1024 * 1024);
    }
}
