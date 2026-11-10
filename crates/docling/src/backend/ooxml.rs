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

/// Open a ZIP the way a well-formed one is laid out first: zip 9's default
/// `ArchiveOffset::Detect` finds the central directory by scanning the
/// whole archive forward for its signatures — ~1.5 instructions a byte, a
/// millisecond on a 3 MB DOCX, paid by every open (format sniffing, the
/// backend, the metadata read) — where a known offset of 0 checks the one
/// position the end record names. An archive that does not start at 0
/// (prepended data, a self-extracting stub) fails that check and is opened
/// again with detection, so nothing that opened before fails now.
pub(crate) fn open_zip<R: std::io::Read + std::io::Seek>(
    reader: impl Fn() -> R,
) -> zip::result::ZipResult<ZipArchive<R>> {
    let at_zero = zip::read::Config {
        archive_offset: zip::read::ArchiveOffset::Known(0),
    };
    ZipArchive::with_config(at_zero, reader()).or_else(|_| ZipArchive::new(reader()))
}

/// Hard cap on a single decompressed OOXML part (512 MiB). Real documents
/// stay far below this; the limit only exists to stop a decompression-bomb
/// part from exhausting memory. Override with `DOCLING_RS_MAX_PART_BYTES`.
pub(crate) fn max_part_bytes() -> u64 {
    docling_core::env::parse("DOCLING_RS_MAX_PART_BYTES").unwrap_or(512 * 1024 * 1024)
}

/// A read-only view over the parts of an OOXML package.
///
/// Cloning is cheap (the file bytes are shared behind an `Arc`, the ZIP
/// central directory is reference-counted by the `zip` crate), which gives
/// each rayon worker its own independent cursor over the same archive.
///
/// `B` is the storage: the owned, shareable copy [`Package::open`] makes,
/// or the caller's own bytes for a short read ([`Package::open_slice`]).
#[derive(Clone)]
pub struct Package<B: AsRef<[u8]> = std::sync::Arc<[u8]>> {
    zip: ZipArchive<Cursor<B>>,
    /// The member paths, decoded once: zip 9 decodes a name on every
    /// `file_names()` call and hands it back as a `Result`.
    names: std::sync::Arc<[String]>,
}

impl Package {
    pub fn open(bytes: &[u8]) -> Option<Self> {
        let bytes: std::sync::Arc<[u8]> = std::sync::Arc::from(bytes);
        Package::with_storage(|| bytes.clone())
    }
}

impl<'a> Package<&'a [u8]> {
    /// [`Package::open`] over the caller's bytes, without the owned copy
    /// a long-lived (or per-worker cloned) package keeps — for a read of a
    /// part or two, such as the document metadata (#679) every conversion
    /// takes: copying a 3 MB DOCX for its `docProps` cost more than reading
    /// them.
    pub fn open_slice(bytes: &'a [u8]) -> Option<Self> {
        Package::with_storage(|| bytes)
    }
}

impl<B: AsRef<[u8]>> Package<B> {
    fn with_storage(storage: impl Fn() -> B) -> Option<Self> {
        let zip = open_zip(|| Cursor::new(storage())).ok()?;
        let names = zip
            .file_names()
            .filter_map(Result::ok)
            .map(|n| n.into_owned())
            .collect();
        Some(Self { zip, names })
    }

    /// The package's member paths, in central-directory order (a name that
    /// is neither UTF-8 nor CP437 is left out).
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.names.iter().map(String::as_str)
    }

    /// Whether any member cannot be read because it is encrypted — docling's
    /// `is_encrypted` for iWork packages. Standard ZIP encryption sets bit 0
    /// of the general-purpose flags; Pages does not use that: it leaves the
    /// flag clear and writes a compression method outside the set ZIP defines
    /// (stored, deflate, bzip2 = 12, lzma = 14), so both signals are needed.
    /// Reads only the central directory — no member is decompressed.
    #[allow(deprecated)] // `CompressionMethod::Unsupported` is how `zip` reports methods it lacks a codec for
    pub fn any_encrypted(&mut self) -> bool {
        use zip::CompressionMethod;
        (0..self.zip.len()).any(|i| match self.zip.by_index_raw(i) {
            Ok(file) => {
                let readable = matches!(
                    file.compression(),
                    CompressionMethod::Stored
                        | CompressionMethod::Deflated
                        | CompressionMethod::Unsupported(12)
                        | CompressionMethod::Unsupported(14)
                );
                file.encrypted() || !readable
            }
            Err(_) => false,
        })
    }

    /// Read a part to a string, or `None` if it is absent, not valid UTF-8, or
    /// nested deeper than the XML guard allows (see [`super::xml_depth`]; the
    /// part is reported and skipped, like an unreadable one).
    pub fn read(&mut self, path: &str) -> Option<String> {
        let bytes = self.read_bytes(path)?;
        let text = String::from_utf8(bytes).ok()?;
        if let Err(e) = super::xml_depth::check(&text, path) {
            eprintln!("docling: {e}; part skipped");
            return None;
        }
        Some(text)
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
/// header (decode-free). A Windows metafile (EMF/WMF), which the `image`
/// crate cannot read, is rendered to PNG instead (#536); `None` for anything
/// else it can't read.
pub fn picture_image(path: &str, data: Vec<u8>) -> Option<PictureImage> {
    let dims = image::ImageReader::new(Cursor::new(&data))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok());
    let Some((width, height)) = dims else {
        return super::metafile::render(&data, None);
    };
    Some(PictureImage {
        dpi: PictureImage::DEFAULT_DPI,
        mimetype: mime_for(path).to_string(),
        width,
        height,
        data,
        display_size: None,
    })
}

/// Whether an embedded picture is a Windows metafile — a placeable WMF (its
/// `9AC6CDD7` key) or an EMF (`" EMF"` at byte 40) — docling's `_is_metafile`.
/// Pillow cannot decode either off Windows, and upstream keeps such a picture
/// (payload-less, unless LibreOffice renders it) where any other undecodable
/// image is dropped.
pub fn is_metafile(data: &[u8]) -> bool {
    data.starts_with(&[0xd7, 0xcd, 0xc6, 0x9a]) || data.get(40..44) == Some(b" EMF")
}

/// PIL's `Image.info["dpi"]` (its horizontal value) for an embedded picture,
/// read the way Pillow reads it ([`docling_core::image_dpi::resolution`]: a
/// PNG `pHYs` chunk, a JPEG's JFIF density else its EXIF resolution, a BMP's
/// pixels per metre, a TIFF's resolution tags). `None` when the format
/// records none (GIF, WebP, a PNG without `pHYs`): python-pptx's `Image.dpi`
/// then reports 72, and docling's `ImageRef.dpi` follows it.
pub fn image_dpi(data: &[u8]) -> Option<f64> {
    docling_core::image_dpi::resolution(data).map(|(x, _)| x)
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
            let opts = zip::write::SimpleFileOptions::default()
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

#[cfg(test)]
pub(crate) mod image_dpi_tests {
    use super::{image_dpi, is_metafile};

    /// A PNG of `chunks` (length + type + data + a zero CRC, which the reader
    /// never checks).
    pub(crate) fn png(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        for (kind, data) in chunks {
            out.extend((data.len() as u32).to_be_bytes());
            out.extend(*kind);
            out.extend(data);
            out.extend([0; 4]);
        }
        out
    }

    fn phys(ppu: u32, unit: u8) -> Vec<u8> {
        let mut d = ppu.to_be_bytes().to_vec();
        d.extend(ppu.to_be_bytes());
        d.push(unit);
        d
    }

    /// Pillow: a `pHYs` in metres is `px * 0.0254` dpi; an aspect-only one
    /// (unit 0) or no chunk at all records no dpi.
    #[test]
    fn png_phys_in_metres_is_dpi() {
        let ihdr = (b"IHDR", vec![0; 13]);
        let dpi = image_dpi(&png(&[
            ihdr.clone(),
            (b"pHYs", phys(11811, 1)),
            (b"IEND", vec![]),
        ]));
        assert!((dpi.unwrap() - 299.9994).abs() < 1e-4, "{dpi:?}");
        assert_eq!(
            image_dpi(&png(&[ihdr.clone(), (b"pHYs", phys(1, 0))])),
            None
        );
        assert_eq!(
            image_dpi(&png(&[ihdr, (b"IDAT", vec![0]), (b"IEND", vec![])])),
            None
        );
    }

    fn jpeg(segments: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut out = vec![0xff, 0xd8];
        for (marker, payload) in segments {
            out.extend([0xff, *marker]);
            out.extend((payload.len() as u16 + 2).to_be_bytes());
            out.extend(payload);
        }
        out.extend([0xff, 0xda, 0, 2]);
        out
    }

    fn jfif(unit: u8, density: u16) -> Vec<u8> {
        let mut d = b"JFIF\0\x01\x01".to_vec();
        d.push(unit);
        d.extend(density.to_be_bytes());
        d.extend(density.to_be_bytes());
        d.extend([0, 0]);
        d
    }

    /// Little-endian TIFF with `XResolution = num/den` and, optionally,
    /// `ResolutionUnit`.
    fn tiff(num: u32, den: u32, unit: Option<u16>) -> Vec<u8> {
        let mut t = b"II*\0".to_vec();
        t.extend(8u32.to_le_bytes());
        let n: u16 = if unit.is_some() { 2 } else { 1 };
        t.extend(n.to_le_bytes());
        let rational_at = 8 + 2 + 12 * u32::from(n) + 4;
        t.extend(0x011au16.to_le_bytes());
        t.extend(5u16.to_le_bytes());
        t.extend(1u32.to_le_bytes());
        t.extend(rational_at.to_le_bytes());
        if let Some(u) = unit {
            t.extend(0x0128u16.to_le_bytes());
            t.extend(3u16.to_le_bytes());
            t.extend(1u32.to_le_bytes());
            t.extend(u.to_le_bytes());
            t.extend([0, 0]);
        }
        t.extend(0u32.to_le_bytes()); // next IFD
        t.extend(num.to_le_bytes());
        t.extend(den.to_le_bytes());
        t
    }

    /// Pillow: the JFIF density in inches or centimetres; with no JFIF unit,
    /// the EXIF `XResolution` (centimetres × 2.54), 72 when the EXIF lacks
    /// the tags; nothing at all when neither segment says.
    #[test]
    fn jpeg_density_from_jfif_then_exif() {
        assert_eq!(image_dpi(&jpeg(&[(0xe0, jfif(1, 96))])), Some(96.0));
        let dpcm = image_dpi(&jpeg(&[(0xe0, jfif(2, 118))])).unwrap();
        assert!((dpcm - 299.72).abs() < 1e-9);
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend(tiff(300, 1, Some(2)));
        assert_eq!(
            image_dpi(&jpeg(&[(0xe0, jfif(0, 1)), (0xe1, exif)])),
            Some(300.0)
        );
        let mut exif_cm = b"Exif\0\0".to_vec();
        exif_cm.extend(tiff(100, 1, Some(3)));
        assert_eq!(image_dpi(&jpeg(&[(0xe1, exif_cm)])), Some(254.0));
        let mut exif_bare = b"Exif\0\0".to_vec();
        exif_bare.extend(tiff(300, 1, None));
        assert_eq!(
            image_dpi(&jpeg(&[(0xe1, exif_bare)])),
            Some(72.0),
            "no unit tag → Pillow's 72"
        );
        assert_eq!(image_dpi(&jpeg(&[(0xe0, jfif(0, 1))])), None);
        // A JFIF density beats a later EXIF, an earlier EXIF yields to it.
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend(tiff(300, 1, Some(2)));
        assert_eq!(
            image_dpi(&jpeg(&[(0xe1, exif.clone()), (0xe0, jfif(1, 96))])),
            Some(96.0)
        );
    }

    /// A BMP's pixels per metre ÷ 39.3701; a TIFF's resolution in inches (unit
    /// 2 or none), centimetres (3), or none at all (1).
    #[test]
    fn bmp_and_tiff_resolutions() {
        let mut bmp = vec![0u8; 54];
        bmp[..2].copy_from_slice(b"BM");
        bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
        bmp[38..42].copy_from_slice(&11811i32.to_le_bytes());
        assert!((image_dpi(&bmp).unwrap() - 299.9992).abs() < 1e-3);
        assert_eq!(image_dpi(&tiff(200, 1, Some(2))), Some(200.0));
        assert_eq!(image_dpi(&tiff(200, 1, None)), Some(200.0));
        assert_eq!(image_dpi(&tiff(100, 1, Some(3))), Some(254.0));
        assert_eq!(image_dpi(&tiff(100, 1, Some(1))), None);
        assert_eq!(image_dpi(b"GIF89a"), None);
    }

    /// docling's `_is_metafile`: a placeable WMF key or an EMF signature at 40.
    #[test]
    fn metafiles_by_signature() {
        assert!(is_metafile(&[0xd7, 0xcd, 0xc6, 0x9a, 0, 0]));
        let mut emf = vec![1u8; 44];
        emf[40..44].copy_from_slice(b" EMF");
        assert!(is_metafile(&emf));
        assert!(!is_metafile(b"\x89PNG\r\n\x1a\n"));
        assert!(!is_metafile(&[]));
    }
}

/// A picture's author-written alt text (#676): the `descr` (else `title`)
/// attribute of its non-visual properties — `wp:docPr` of a DOCX drawing,
/// `p:cNvPr` of a PPTX picture, `xdr:cNvPr` of a spreadsheet drawing —
/// trimmed; `None` when both are missing or blank. Office writes `descr`
/// (the "Alt Text" box; its generated captions read "… Description
/// automatically generated") and older documents `title`.
pub(crate) fn alt_text_of(props: roxmltree::Node) -> Option<String> {
    ["descr", "title"].iter().find_map(|name| {
        props
            .attributes()
            .find(|a| a.name() == *name)
            .map(|a| a.value().trim())
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    })
}
