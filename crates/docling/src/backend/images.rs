//! External image resolution for the HTML/EPUB backends.
//!
//! docling's HTML/EPUB backends can pull the actual image bytes behind an
//! `<img src>` into the document (so they survive into JSON `ImageRef`s and
//! `--images embedded|referenced`). This is the analogue of docling's
//! image-fetch path, off by default (matching `enable_*_fetch=False`) and turned
//! on with [`crate::DocumentConverter::fetch_images`].
//!
//! A [`ImageResolver`] turns an `<img src>` string into an extracted
//! [`PictureImage`]. Three concrete resolvers cover the cases:
//! - [`NoFetch`] — the default; never resolves anything.
//! - [`FsImageResolver`] — HTML: `data:` URIs, local files (relative to the
//!   source's directory), and remote `http(s)` URLs.
//! - [`MapImageResolver`] — EPUB: images pre-read from the archive, keyed by
//!   their resolved in-archive path.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use docling_core::PictureImage;

/// Cap on a single fetched/decoded image, to bound memory on hostile input.
const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

/// Resolves an `<img src>` to the image bytes behind it, or `None` if it can't
/// (unfetchable, unreadable, or an unsupported encoding).
pub(crate) trait ImageResolver {
    fn resolve(&self, src: &str) -> Option<PictureImage>;
}

/// The default: never extracts an image (every `<img>` stays a placeholder).
pub(crate) struct NoFetch;

impl ImageResolver for NoFetch {
    fn resolve(&self, _src: &str) -> Option<PictureImage> {
        None
    }
}

/// Filesystem/network resolver for standalone HTML. Handles `data:` URIs inline,
/// reads local files relative to the source document's directory, and fetches
/// remote `http(s)` URLs.
pub(crate) struct FsImageResolver {
    base_dir: Option<PathBuf>,
}

impl FsImageResolver {
    /// `base_dir` is the source HTML file's directory, used to resolve relative
    /// `src` paths; `None` (an in-memory source) disables relative-path reads.
    pub(crate) fn new(base_dir: Option<PathBuf>) -> Self {
        Self { base_dir }
    }
}

impl ImageResolver for FsImageResolver {
    fn resolve(&self, src: &str) -> Option<PictureImage> {
        let src = src.trim();
        if src.is_empty() {
            return None;
        }
        if src.starts_with("data:") {
            return from_data_uri(src);
        }
        if src.starts_with("http://") || src.starts_with("https://") {
            return fetch_remote(src);
        }
        // A local path (possibly `file://`). Absolute paths are read directly;
        // relative ones only when we know the source's directory — never against
        // an arbitrary working directory.
        let rel = src.strip_prefix("file://").unwrap_or(src);
        let path = Path::new(rel);
        let full = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base_dir.as_ref()?.join(rel)
        };
        let data = std::fs::read(&full).ok()?;
        super::ooxml::picture_image(full.to_str().unwrap_or(rel), data)
    }
}

/// Resolver backed by images already extracted from an archive (EPUB), keyed by
/// the `src` string the HTML carries (rewritten to the resolved archive path).
pub(crate) struct MapImageResolver {
    images: HashMap<String, PictureImage>,
}

impl MapImageResolver {
    pub(crate) fn new(images: HashMap<String, PictureImage>) -> Self {
        Self { images }
    }
}

impl ImageResolver for MapImageResolver {
    fn resolve(&self, src: &str) -> Option<PictureImage> {
        self.images.get(src.trim()).cloned()
    }
}

/// Decode a `data:[<mime>][;base64],<payload>` image URI.
fn from_data_uri(uri: &str) -> Option<PictureImage> {
    let rest = uri.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let mime = meta
        .split(';')
        .next()
        .filter(|m| !m.is_empty())
        .unwrap_or("image/png");
    let data = if meta.split(';').any(|t| t.eq_ignore_ascii_case("base64")) {
        docling_core::base64::decode(payload)?
    } else {
        percent_decode(payload)
    };
    build_picture(mime, data)
}

/// Fetch a remote image over HTTP(S). The mimetype comes from `Content-Type`
/// when it's an `image/*`, else it's guessed from the URL's extension.
fn fetch_remote(url: &str) -> Option<PictureImage> {
    let mut resp = ureq::get(url).call().ok()?;
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|c| {
            c.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        });
    let data = resp
        .body_mut()
        .with_config()
        .limit(MAX_IMAGE_BYTES)
        .read_to_vec()
        .ok()?;
    match content_type {
        Some(mime) if mime.starts_with("image/") => build_picture(mime, data),
        // No usable Content-Type: fall back to the extension in the URL path.
        _ => {
            let path = url.split(['?', '#']).next().unwrap_or(url);
            super::ooxml::picture_image(path, data)
        }
    }
}

/// Build a [`PictureImage`] from explicit mimetype + bytes, reading the pixel
/// size from the header. `None` for empty data or a format `image` can't read.
pub(crate) fn build_picture(mimetype: impl Into<String>, data: Vec<u8>) -> Option<PictureImage> {
    if data.is_empty() {
        return None;
    }
    let (width, height) = image::ImageReader::new(Cursor::new(&data))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    Some(PictureImage {
        mimetype: mimetype.into(),
        width,
        height,
        data,
    })
}

/// Minimal `%XX` percent-decoding for non-base64 `data:` URIs (rare for images).
fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use docling_core::base64::encode;

    // A 1×1 red PNG, the smallest real image to prove decode + dimension read.
    const RED_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8,
        0xcf, 0xc0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x6e, 0x2c, 0xdc, 0x33, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn decodes_base64_data_uri() {
        let uri = format!("data:image/png;base64,{}", encode(RED_PNG));
        let img = from_data_uri(&uri).expect("decodes");
        assert_eq!(img.mimetype, "image/png");
        assert_eq!((img.width, img.height), (1, 1));
        assert_eq!(img.data, RED_PNG);
    }

    #[test]
    fn rejects_garbage_data_uri() {
        assert!(from_data_uri("data:image/png;base64,not-an-image").is_none());
        assert!(from_data_uri("data:,").is_none());
    }

    #[test]
    fn nofetch_resolves_nothing() {
        assert!(NoFetch.resolve("data:image/png;base64,AAAA").is_none());
    }

    #[test]
    fn map_resolver_returns_by_key() {
        let img = from_data_uri(&format!("data:image/png;base64,{}", encode(RED_PNG))).unwrap();
        let mut map = HashMap::new();
        map.insert("images/x.png".to_string(), img.clone());
        let r = MapImageResolver::new(map);
        assert_eq!(r.resolve("images/x.png"), Some(img));
        assert!(r.resolve("images/missing.png").is_none());
    }

    #[test]
    fn fs_resolver_reads_absolute_files_but_not_relative_without_base() {
        // A real file at an absolute path is read.
        let p = std::env::temp_dir().join(format!("docling.rs_img_{}.png", std::process::id()));
        std::fs::write(&p, RED_PNG).unwrap();
        let r = FsImageResolver::new(None);
        let img = r.resolve(p.to_str().unwrap()).expect("reads local file");
        assert_eq!((img.width, img.height), (1, 1));
        let _ = std::fs::remove_file(&p);
        // A relative path with no known base dir is refused (never reads from CWD).
        assert!(FsImageResolver::new(None)
            .resolve("nope/relative.png")
            .is_none());
        // data: URIs still work regardless of base dir.
        assert!(r
            .resolve(&format!("data:image/png;base64,{}", encode(RED_PNG)))
            .is_some());
    }
}
