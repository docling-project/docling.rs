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
#[cfg(feature = "fetch-images")]
use std::sync::Mutex;

use docling_core::PictureImage;

/// Cap on a single fetched/decoded image, to bound memory on hostile input.
#[cfg(feature = "fetch-images")]
const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

/// Resolves an `<img src>` to the image bytes behind it, or `None` if it can't
/// (unfetchable, unreadable, or an unsupported encoding).
pub(crate) trait ImageResolver {
    fn resolve(&self, src: &str) -> Option<PictureImage>;

    /// Warm any slow (network) resolutions for `srcs` concurrently, before the
    /// serial document walk resolves them one by one. The default does nothing
    /// (resolvers whose lookups are all in-memory gain nothing from it); the
    /// remote-fetching [`FsImageResolver`] overrides it to fetch in parallel.
    fn prefetch(&self, _srcs: &[String]) {}
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
/// remote `http(s)` URLs — including relative / protocol-relative `<img src>`
/// resolved against the page's [`base_url`](Self::base_url) when the HTML was
/// itself fetched from the web.
pub(crate) struct FsImageResolver {
    base_dir: Option<PathBuf>,
    /// Only read by the gated remote-URL resolution; without `fetch-images`
    /// (e.g. the wasm32 build) it is stored-but-unused so `new`'s signature
    /// stays the same across feature shapes.
    #[cfg_attr(not(feature = "fetch-images"), allow(dead_code))]
    base_url: Option<String>,
    /// Memoized remote fetches, keyed by the resolved absolute URL: fills as
    /// [`prefetch`](Self::prefetch) warms it (concurrently) and as `resolve`
    /// hits it (serially), so each distinct URL is fetched at most once even
    /// when several relative `<img src>` resolve to it. `None` caches a miss so
    /// a failed URL isn't retried.
    #[cfg(feature = "fetch-images")]
    cache: Mutex<HashMap<String, Option<PictureImage>>>,
}

impl FsImageResolver {
    /// `base_dir` is the source HTML file's directory (relative-path reads);
    /// `base_url` is the URL the page was fetched from (relative `<img src>`
    /// resolution). Either may be `None`.
    pub(crate) fn new(base_dir: Option<PathBuf>, base_url: Option<String>) -> Self {
        Self {
            base_dir,
            base_url,
            #[cfg(feature = "fetch-images")]
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve `src` to an absolute `http(s)` URL when possible: an already-
    /// absolute URL as-is, else relative / protocol-relative / absolute-path
    /// joined against the page base URL. `None` when there is nothing remote to
    /// fetch (no base URL for a relative src).
    #[cfg(feature = "fetch-images")]
    fn absolute_http_url(&self, src: &str) -> Option<String> {
        if src.starts_with("http://") || src.starts_with("https://") {
            return Some(src.to_string());
        }
        let base = self.base_url.as_deref()?;
        let joined = url::Url::parse(base).ok()?.join(src).ok()?;
        matches!(joined.scheme(), "http" | "https").then(|| joined.to_string())
    }

    /// Fetch `url` unless a prior fetch already cached it, memoizing the result
    /// (hit or miss). Shared by the serial `resolve` path and the concurrent
    /// `prefetch` workers — the lock is held only around the map lookup/insert,
    /// never across the network call.
    #[cfg(feature = "fetch-images")]
    fn fetch_cached(&self, url: &str) -> Option<PictureImage> {
        if let Some(hit) = self.cache.lock().unwrap().get(url) {
            return hit.clone();
        }
        let img = fetch_remote(url);
        self.cache
            .lock()
            .unwrap()
            .insert(url.to_string(), img.clone());
        img
    }
}

/// How many remote images to fetch at once during [`FsImageResolver::prefetch`].
/// Image fetching is I/O-bound (mostly waiting on the network), so the default
/// runs well ahead of the core count; `DOCLING_RS_IMAGE_FETCH_CONCURRENCY`
/// overrides it, clamped to a sane range.
#[cfg(feature = "fetch-images")]
fn image_fetch_concurrency() -> usize {
    std::env::var("DOCLING_RS_IMAGE_FETCH_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(10)
        .clamp(1, 64)
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
        // Remote: an absolute URL, or a relative one resolved against the page
        // base URL (a page fetched from the web references images by relative
        // path). Only compiled with the HTTP client available.
        #[cfg(feature = "fetch-images")]
        if let Some(url) = self.absolute_http_url(src) {
            return self.fetch_cached(&url);
        }
        #[cfg(not(feature = "fetch-images"))]
        if src.starts_with("http://") || src.starts_with("https://") {
            return None;
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

    /// Fetch every distinct remote image among `srcs` concurrently, warming the
    /// cache so the subsequent serial walk resolves them without blocking. Local
    /// files and `data:` URIs are skipped (their `resolve` is already cheap).
    #[cfg(feature = "fetch-images")]
    fn prefetch(&self, srcs: &[String]) {
        // Unique absolute URLs we haven't fetched yet, preserving nothing about
        // order (fetches are independent).
        let urls: Vec<String> = {
            let cache = self.cache.lock().unwrap();
            let mut seen = std::collections::HashSet::new();
            srcs.iter()
                .filter_map(|s| self.absolute_http_url(s.trim()))
                .filter(|u| !cache.contains_key(u) && seen.insert(u.clone()))
                .collect()
        };
        if urls.len() < 2 {
            // 0 → nothing to do; 1 → the serial `resolve` fetches it just as
            // fast without spinning up a worker.
            return;
        }
        // I/O-bound work-stealing: N worker threads pull URLs off a shared
        // index and fetch (each `fetch_cached` inserts into the cache under a
        // brief lock). No new dependency, and concurrency is bounded regardless
        // of how many images the page carries.
        let workers = image_fetch_concurrency().min(urls.len());
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    match urls.get(i) {
                        Some(url) => {
                            let _ = self.fetch_cached(url);
                        }
                        None => break,
                    }
                });
            }
        });
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

/// Whether a resolved IP points back at the local host or private
/// infrastructure — the SSRF block-list (mirrors docling-serve's URL-fetch
/// guard, so a document that points `<img src>` at an internal address can't
/// reach it when image fetching runs on a server).
#[cfg(feature = "fetch-images")]
fn is_blocked_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_blocked_ip(IpAddr::V4(v4)))
        }
    }
}

/// `true` when the URL's host resolves to a blocked address and the operator
/// hasn't opted out with `DOCLING_RS_ALLOW_PRIVATE_IP_FETCH` (the same flag
/// docling-serve's URL fetch honors, for local/intranet development).
#[cfg(feature = "fetch-images")]
fn blocked_by_ssrf_guard(url: &str) -> bool {
    use std::net::ToSocketAddrs;
    let allow = std::env::var("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH")
        .map(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(false);
    if allow {
        return false;
    }
    let Ok(parsed) = url::Url::parse(url) else {
        return true;
    };
    let Some(host) = parsed.host_str() else {
        return true;
    };
    let port = parsed.port_or_known_default().unwrap_or(80);
    match (host, port).to_socket_addrs() {
        Ok(addrs) => addrs.map(|a| a.ip()).any(is_blocked_ip),
        // Unresolvable host: nothing to fetch — treat as blocked (skip).
        Err(_) => true,
    }
}

/// Fetch a remote image over HTTP(S). The mimetype comes from `Content-Type`
/// when it's an `image/*`, else it's guessed from the URL's extension.
/// Bounded: an SSRF block-list, a connect/overall timeout, and a redirect cap
/// keep one hostile or slow `<img src>` from hanging the whole conversion.
#[cfg(feature = "fetch-images")]
fn fetch_remote(url: &str) -> Option<PictureImage> {
    use std::time::Duration;
    if blocked_by_ssrf_guard(url) {
        return None;
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_global(Some(Duration::from_secs(20)))
        .max_redirects(3)
        .build()
        .into();
    let mut resp = agent.get(url).call().ok()?;
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
        let r = FsImageResolver::new(None, None);
        let img = r.resolve(p.to_str().unwrap()).expect("reads local file");
        assert_eq!((img.width, img.height), (1, 1));
        let _ = std::fs::remove_file(&p);
        // A relative path with no known base dir is refused (never reads from CWD).
        assert!(FsImageResolver::new(None, None)
            .resolve("nope/relative.png")
            .is_none());
        // data: URIs still work regardless of base dir.
        assert!(r
            .resolve(&format!("data:image/png;base64,{}", encode(RED_PNG)))
            .is_some());
    }

    #[cfg(feature = "fetch-images")]
    #[test]
    fn resolves_relative_and_protocol_relative_against_base_url() {
        // The URL join the fetch path relies on, without touching the network.
        let r = FsImageResolver::new(None, Some("https://ex.com/a/page.html".into()));
        assert_eq!(
            r.absolute_http_url("/img/x.png").as_deref(),
            Some("https://ex.com/img/x.png")
        );
        assert_eq!(
            r.absolute_http_url("y.png").as_deref(),
            Some("https://ex.com/a/y.png")
        );
        assert_eq!(
            r.absolute_http_url("//cdn.ex.com/z.png").as_deref(),
            Some("https://cdn.ex.com/z.png")
        );
        assert_eq!(
            r.absolute_http_url("https://other.com/w.png").as_deref(),
            Some("https://other.com/w.png")
        );
        // No base URL: a relative src has nothing to resolve against.
        let no_base = FsImageResolver::new(None, None);
        assert!(no_base.absolute_http_url("/img/x.png").is_none());
    }

    #[cfg(feature = "fetch-images")]
    #[test]
    fn concurrency_env_is_parsed_and_clamped() {
        use super::image_fetch_concurrency;
        std::env::set_var("DOCLING_RS_IMAGE_FETCH_CONCURRENCY", "7");
        assert_eq!(image_fetch_concurrency(), 7);
        std::env::set_var("DOCLING_RS_IMAGE_FETCH_CONCURRENCY", "0");
        assert_eq!(image_fetch_concurrency(), 10, "0 → default, never zero");
        std::env::set_var("DOCLING_RS_IMAGE_FETCH_CONCURRENCY", "9999");
        assert_eq!(image_fetch_concurrency(), 64, "clamped to the ceiling");
        std::env::set_var("DOCLING_RS_IMAGE_FETCH_CONCURRENCY", "junk");
        assert_eq!(image_fetch_concurrency(), 10);
        std::env::remove_var("DOCLING_RS_IMAGE_FETCH_CONCURRENCY");
    }

    /// prefetch fetches each distinct remote image exactly once (concurrently),
    /// caches the bytes, and dedupes repeats — proven against a tiny in-process
    /// HTTP server that counts the requests it serves.
    #[cfg(feature = "fetch-images")]
    #[test]
    fn prefetch_fetches_each_url_once_and_warms_the_cache() {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let server_hits = Arc::clone(&hits);
        // Serve RED_PNG to every GET, counting requests. Detached: the loop
        // outlives the test and the process reaps it on exit.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf); // consume the request line/headers
                server_hits.fetch_add(1, Ordering::Relaxed);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    RED_PNG.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(RED_PNG);
                let _ = stream.flush();
            }
        });

        // 127.0.0.1 is on the SSRF block-list; opt in for the test.
        std::env::set_var("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH", "1");
        let base = format!("http://{addr}/dir/page.html");
        let r = FsImageResolver::new(None, Some(base));

        // Two distinct images, one repeated — three srcs, two fetches.
        r.prefetch(&[
            "img1.png".to_string(),
            "img2.png".to_string(),
            "img1.png".to_string(),
        ]);
        assert_eq!(
            hits.load(Ordering::Relaxed),
            2,
            "each distinct URL fetched once"
        );

        // The walk now resolves from the warm cache — no further requests.
        let img = r.resolve("img1.png").expect("cached image resolves");
        assert_eq!((img.width, img.height), (1, 1));
        assert_eq!(
            r.resolve("/dir/img2.png").map(|i| (i.width, i.height)),
            Some((1, 1))
        );
        assert_eq!(
            hits.load(Ordering::Relaxed),
            2,
            "resolve served from cache, no refetch"
        );

        std::env::remove_var("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH");
    }
}
