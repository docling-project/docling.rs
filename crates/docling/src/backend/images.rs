//! Image resolution for the declarative backends (HTML, EPUB, MHTML,
//! AsciiDoc, JATS, ODF, Markdown, email) under one **source policy** (#646).
//!
//! docling's backends can pull the actual image bytes behind an `<img src>`
//! into the document (so they survive into JSON `ImageRef`s and `--images
//! embedded|referenced`). docling gates that with two booleans
//! (`enable_remote_fetch` / `enable_local_fetch`); this crate's gate is
//! [`ImageSources`], a tier a conversion may not exceed:
//!
//! - [`ImageSources::None`] — nothing resolves (the default, docling's
//!   `enable_*_fetch=False`).
//! - [`ImageSources::Embedded`] — `data:` URIs and parts of the same
//!   container: EPUB archive entries, MHTML parts, an email's `cid:`
//!   attachments. No filesystem, no network — the tier a server converting
//!   untrusted input can run.
//! - [`ImageSources::Local`] — plus files *under the source file's
//!   directory*: a relative path is joined onto it and, after `..`
//!   normalization and symlink resolution, must still be inside; absolute
//!   paths and `file://` URLs are never read (a document that names
//!   `/etc/hosts` is not a request a document converter should honour).
//! - [`ImageSources::Remote`] — plus `http(s)` fetches, optionally confined
//!   to [`ImagePolicy::hosts`]; every hop of a redirect is held to the same
//!   allow-list and to the SSRF block-list.
//!
//! [`ImageLimits`] bound what a document may make the process hold: one
//! image's size, how many resolve, their total, and a floor that skips
//! spacer/tracking pixels. A limit never fails the conversion — the picture
//! stays a placeholder and one warning per document says why.
//!
//! An [`ImageResolver`] turns a source string into an extracted
//! [`PictureImage`]:
//! - [`NoFetch`] — never resolves anything (the `None` tier).
//! - [`FsImageResolver`] — `data:` URIs, confined local files, remote URLs,
//!   each behind its tier.
//! - [`MapImageResolver`] — images pre-read from a container (EPUB entries,
//!   email `cid:` parts), keyed by the `src` string the HTML carries.

use std::collections::HashMap;
use std::fmt;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

use docling_core::PictureImage;

/// Which image references a conversion may resolve (#646), ordered: each
/// tier includes the ones before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum ImageSources {
    /// Resolve nothing — every picture stays a placeholder (the default).
    #[default]
    None,
    /// `data:` URIs and parts of the same container (EPUB entries, MHTML
    /// parts, email `cid:` attachments). No filesystem or network access.
    Embedded,
    /// `Embedded`, plus files under the source file's directory (never an
    /// absolute path, never outside that directory).
    Local,
    /// `Local`, plus remote `http(s)` fetches (optionally host-restricted).
    Remote,
}

impl ImageSources {
    /// Every tier, in order, with its wire spelling.
    pub const ALL: [ImageSources; 4] = [
        ImageSources::None,
        ImageSources::Embedded,
        ImageSources::Local,
        ImageSources::Remote,
    ];

    /// The wire spelling (`none` | `embedded` | `local` | `remote`).
    pub fn as_str(self) -> &'static str {
        match self {
            ImageSources::None => "none",
            ImageSources::Embedded => "embedded",
            ImageSources::Local => "local",
            ImageSources::Remote => "remote",
        }
    }

    /// `data:` URIs and in-container parts resolve.
    pub fn embedded(self) -> bool {
        self >= ImageSources::Embedded
    }

    /// Files under the source directory resolve.
    pub fn local(self) -> bool {
        self >= ImageSources::Local
    }

    /// Remote URLs resolve.
    pub fn remote(self) -> bool {
        self >= ImageSources::Remote
    }
}

impl fmt::Display for ImageSources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ImageSources {
    type Err = String;

    /// The wire spelling, case-insensitively; the error names the choices.
    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        ImageSources::ALL
            .into_iter()
            .find(|t| t.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| {
                format!("image_sources must be one of none, embedded, local, remote; got {s:?}")
            })
    }
}

/// Per-document bounds on resolved images (#646). Each has an environment
/// default (read by [`ImageLimits::from_env`]) and a builder override on
/// `DocumentConverter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageLimits {
    /// Largest image that resolves, in bytes (`DOCLING_RS_MAX_IMAGE_BYTES`;
    /// 32 MiB — the pre-#646 constant).
    pub max_bytes: u64,
    /// How many images resolve per document; the rest stay placeholders
    /// (`DOCLING_RS_MAX_IMAGES`; unlimited).
    pub max_images: Option<usize>,
    /// Total resolved bytes per document (`DOCLING_RS_MAX_IMAGE_TOTAL_MB`
    /// in MiB; unlimited).
    pub max_total_bytes: Option<u64>,
    /// Smallest image that resolves, in bytes — spacers and tracking pixels
    /// are a few dozen bytes (`DOCLING_RS_MIN_IMAGE_BYTES`; 0).
    pub min_bytes: u64,
}

/// The pre-#646 cap on a single image, the default of
/// [`ImageLimits::max_bytes`].
pub const DEFAULT_MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

impl Default for ImageLimits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_IMAGE_BYTES,
            max_images: None,
            max_total_bytes: None,
            min_bytes: 0,
        }
    }
}

impl ImageLimits {
    /// The environment's limits: `DOCLING_RS_MAX_IMAGE_BYTES`,
    /// `DOCLING_RS_MAX_IMAGES`, `DOCLING_RS_MAX_IMAGE_TOTAL_MB`,
    /// `DOCLING_RS_MIN_IMAGE_BYTES`; an unset or unreadable value is the
    /// default.
    pub fn from_env() -> Self {
        use docling_core::env;
        let defaults = Self::default();
        Self {
            max_bytes: env::parse::<u64>("DOCLING_RS_MAX_IMAGE_BYTES")
                .filter(|&b| b > 0)
                .unwrap_or(defaults.max_bytes),
            max_images: env::parse::<usize>("DOCLING_RS_MAX_IMAGES"),
            max_total_bytes: env::parse::<u64>("DOCLING_RS_MAX_IMAGE_TOTAL_MB")
                .map(|mb| mb.saturating_mul(1024 * 1024)),
            min_bytes: env::parse::<u64>("DOCLING_RS_MIN_IMAGE_BYTES").unwrap_or(0),
        }
    }
}

/// What a conversion may resolve and how much (#646): the tier, the remote
/// host allow-list, the limits.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImagePolicy {
    pub sources: ImageSources,
    /// Hosts a `Remote` fetch may reach: exact names, or `*.suffix`
    /// wildcards matching any subdomain; empty = any host. Compared
    /// case-insensitively; a redirect is held to the same list.
    pub hosts: Vec<String>,
    pub limits: ImageLimits,
}

impl ImagePolicy {
    /// The policy of a given tier with the environment's limits and no host
    /// restriction.
    pub fn new(sources: ImageSources) -> Self {
        Self {
            sources,
            hosts: Vec::new(),
            limits: ImageLimits::from_env(),
        }
    }

    /// Whether `host` passes [`hosts`](Self::hosts).
    pub fn host_allowed(&self, host: &str) -> bool {
        if self.hosts.is_empty() {
            return true;
        }
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.hosts.iter().any(|entry| {
            let entry = entry.trim().trim_end_matches('.').to_ascii_lowercase();
            match entry.strip_prefix("*.") {
                Some(suffix) => {
                    host.len() > suffix.len() + 1 && host.ends_with(&format!(".{suffix}"))
                }
                None => host == entry,
            }
        })
    }
}

/// The per-document budget every resolver draws on (#646): the count and
/// the bytes resolved so far against [`ImageLimits`]. Shared across the
/// concurrent prefetch workers, so it is a mutex; a refused image warns
/// once per document (the first refusal names the limit).
pub(crate) struct Budget {
    limits: ImageLimits,
    state: Mutex<BudgetState>,
}

#[derive(Default)]
struct BudgetState {
    count: usize,
    total: u64,
    warned: bool,
}

impl Budget {
    pub(crate) fn new(limits: ImageLimits) -> Self {
        Self {
            limits,
            state: Mutex::new(BudgetState::default()),
        }
    }

    /// Whether an image of `len` bytes may resolve, charging it to the
    /// budget when it may. A refusal warns once per document.
    pub(crate) fn admit(&self, len: u64) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let refused = if len > self.limits.max_bytes {
            Some(format!(
                "an image of {len} bytes exceeds max_image_bytes={}",
                self.limits.max_bytes
            ))
        } else if len < self.limits.min_bytes {
            // Too small to be content: skipped quietly, no budget charged,
            // and not worth a warning (that is the point of the floor).
            return false;
        } else if self.limits.max_images.is_some_and(|max| state.count >= max) {
            Some(format!(
                "max_images={} reached",
                self.limits.max_images.unwrap_or(0)
            ))
        } else if self
            .limits
            .max_total_bytes
            .is_some_and(|max| state.total.saturating_add(len) > max)
        {
            Some(format!(
                "max_image_total_mb={} reached",
                self.limits.max_total_bytes.unwrap_or(0) / (1024 * 1024)
            ))
        } else {
            None
        };
        match refused {
            Some(why) => {
                if !state.warned {
                    state.warned = true;
                    eprintln!("warning: images: {why}; further images stay placeholders");
                }
                false
            }
            None => {
                state.count += 1;
                state.total = state.total.saturating_add(len);
                true
            }
        }
    }

    /// Charge a decoded picture, or drop it when the budget refuses it.
    pub(crate) fn filter(&self, img: Option<PictureImage>) -> Option<PictureImage> {
        img.filter(|i| self.admit(i.data.len() as u64))
    }
}

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

/// Filesystem/network resolver for standalone HTML, AsciiDoc and Markdown
/// under an [`ImagePolicy`]: `data:` URIs inline (`Embedded`), files under
/// the source document's directory (`Local`), and remote `http(s)` URLs
/// (`Remote`) — including relative / protocol-relative `<img src>` resolved
/// against the page's [`base_url`](Self::base_url) when the HTML was itself
/// fetched from the web.
pub(crate) struct FsImageResolver {
    base_dir: Option<PathBuf>,
    /// Only read by the gated remote-URL resolution; without `fetch-images`
    /// (e.g. the wasm32 build) it is stored-but-unused so `new`'s signature
    /// stays the same across feature shapes.
    #[cfg_attr(not(feature = "fetch-images"), allow(dead_code))]
    base_url: Option<String>,
    policy: ImagePolicy,
    budget: Budget,
    /// Memoized remote fetches, keyed by the resolved absolute URL: fills as
    /// [`prefetch`](Self::prefetch) warms it (concurrently) and as `resolve`
    /// hits it (serially), so each distinct URL is fetched at most once even
    /// when several relative `<img src>` resolve to it. `None` caches a miss so
    /// a failed URL isn't retried.
    #[cfg(feature = "fetch-images")]
    cache: Mutex<HashMap<String, Option<PictureImage>>>,
}

impl FsImageResolver {
    /// `base_dir` is the source file's directory (relative-path reads under
    /// `Local`); `base_url` is the URL the page was fetched from (relative
    /// `<img src>` resolution under `Remote`). Either may be `None`.
    pub(crate) fn new(
        base_dir: Option<PathBuf>,
        base_url: Option<String>,
        policy: ImagePolicy,
    ) -> Self {
        Self {
            base_dir,
            base_url,
            budget: Budget::new(policy.limits),
            policy,
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
    /// never across the network call. The budget is charged on the fetch,
    /// once per distinct URL.
    #[cfg(feature = "fetch-images")]
    fn fetch_cached(&self, url: &str) -> Option<PictureImage> {
        if let Some(hit) = self.cache.lock().unwrap().get(url) {
            return hit.clone();
        }
        let img = self.budget.filter(fetch_remote_with(url, &self.policy));
        self.cache
            .lock()
            .unwrap()
            .insert(url.to_string(), img.clone());
        img
    }

    /// The file a relative `src` names under `Local`: joined onto
    /// `base_dir`, canonicalized (so `..` and symlinks are resolved), and
    /// kept only when it is still inside the canonical `base_dir`. `None`
    /// for an absolute path, a `file://` URL, a path that escapes, no
    /// `base_dir` (an in-memory source — never the working directory), or
    /// a file that does not exist.
    fn confined_local_path(&self, src: &str) -> Option<PathBuf> {
        if !self.policy.sources.local() {
            return None;
        }
        if src.starts_with("file:") || has_scheme(src) {
            return None;
        }
        // A relative `src` is a URL path (docling#5082, #1180): its query and
        // fragment are not part of the file name, and it is percent-decoded
        // once (`figure%20one.png?v=1#preview` → `figure one.png`) before the
        // confinement below. The name as written is the fallback, so a file
        // whose name holds a literal `%`, `?` or `#` still resolves.
        let path = src.split(['?', '#']).next().unwrap_or("");
        let decoded = String::from_utf8(percent_decode(path)).ok();
        decoded
            .as_deref()
            .into_iter()
            .chain((decoded.as_deref() != Some(src)).then_some(src))
            .find_map(|candidate| self.confined(candidate))
    }

    /// [`Self::confined_local_path`] for one candidate path.
    fn confined(&self, src: &str) -> Option<PathBuf> {
        // A decoded `%2F%2F…` or `%5C…` is absolute on every host.
        if src.is_empty() || src.starts_with(['/', '\\']) {
            return None;
        }
        let rel = Path::new(src);
        if rel.is_absolute() || rel.has_root() {
            return None;
        }
        // A `C:` drive prefix is absolute on Windows and a plain component
        // elsewhere; refuse it everywhere so a document means the same
        // thing on every host.
        if rel
            .components()
            .any(|c| matches!(c, std::path::Component::Prefix(_)))
        {
            return None;
        }
        let base = self.base_dir.as_ref()?.canonicalize().ok()?;
        let full = base.join(rel).canonicalize().ok()?;
        full.starts_with(&base).then_some(full)
    }
}

/// Whether `src` starts with a URL scheme (`scheme:`), so a path check
/// never reads `file:…`, `c:…` or an unknown scheme as a relative file.
fn has_scheme(src: &str) -> bool {
    let Some(colon) = src.find(':') else {
        return false;
    };
    let scheme = &src[..colon];
    !scheme.is_empty()
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        && scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
}

/// How many remote images to fetch at once during [`FsImageResolver::prefetch`].
/// Image fetching is I/O-bound (mostly waiting on the network), so the default
/// runs well ahead of the core count; `DOCLING_RS_IMAGE_FETCH_CONCURRENCY`
/// overrides it, clamped to a sane range.
#[cfg(feature = "fetch-images")]
fn image_fetch_concurrency() -> usize {
    docling_core::env::parse::<usize>("DOCLING_RS_IMAGE_FETCH_CONCURRENCY")
        .filter(|&n| n > 0)
        .unwrap_or(10)
        .clamp(1, 64)
}

impl ImageResolver for FsImageResolver {
    fn resolve(&self, src: &str) -> Option<PictureImage> {
        self.resolve_raw(src).map(upright)
    }

    #[cfg(feature = "fetch-images")]
    fn prefetch(&self, srcs: &[String]) {
        self.prefetch_remote(srcs);
    }
}

impl FsImageResolver {
    fn resolve_raw(&self, src: &str) -> Option<PictureImage> {
        let src = src.trim();
        if src.is_empty() {
            return None;
        }
        if src.starts_with("data:") {
            if !self.policy.sources.embedded() {
                return None;
            }
            return self.budget.filter(from_data_uri(src));
        }
        // Remote: an absolute URL, or a relative one resolved against the page
        // base URL (a page fetched from the web references images by relative
        // path). Only compiled with the HTTP client available.
        #[cfg(feature = "fetch-images")]
        if self.policy.sources.remote() {
            if let Some(url) = self.absolute_http_url(src) {
                return self.fetch_cached(&url);
            }
        }
        if src.starts_with("http://") || src.starts_with("https://") || src.starts_with("//") {
            return None;
        }
        // A local file, only under `Local` and only inside the source's
        // directory.
        let full = self.confined_local_path(src)?;
        let data = std::fs::read(&full).ok()?;
        if !self.budget.admit(data.len() as u64) {
            return None;
        }
        super::ooxml::picture_image(full.to_str().unwrap_or(src), data)
    }

    /// Fetch every distinct remote image among `srcs` concurrently, warming the
    /// cache so the subsequent serial walk resolves them without blocking. Local
    /// files and `data:` URIs are skipped (their `resolve` is already cheap).
    #[cfg(feature = "fetch-images")]
    fn prefetch_remote(&self, srcs: &[String]) {
        if !self.policy.sources.remote() {
            return;
        }
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

/// Resolver backed by images already extracted from a container — an EPUB's
/// archive entries, an email's `cid:` parts — keyed by the `src` string the
/// HTML carries (rewritten to the resolved archive path, or the normalized
/// `cid:` id). Under `Embedded` and above; `data:` URIs decode too, so an
/// email body with an inline `data:` image embeds it.
pub(crate) struct MapImageResolver {
    images: HashMap<String, PictureImage>,
    sources: ImageSources,
    budget: Budget,
}

impl MapImageResolver {
    pub(crate) fn new(images: HashMap<String, PictureImage>, policy: &ImagePolicy) -> Self {
        Self {
            images,
            sources: policy.sources,
            budget: Budget::new(policy.limits),
        }
    }
}

impl ImageResolver for MapImageResolver {
    fn resolve(&self, src: &str) -> Option<PictureImage> {
        self.resolve_raw(src).map(upright)
    }
}

impl MapImageResolver {
    fn resolve_raw(&self, src: &str) -> Option<PictureImage> {
        if !self.sources.embedded() {
            return None;
        }
        let src = src.trim();
        if src.starts_with("data:") {
            return self.budget.filter(from_data_uri(src));
        }
        // A `cid:` reference is matched by its normalized id, whatever the
        // body's spelling (`CID:<x@y>`); anything else by the exact key.
        let hit = if src.len() >= 4 && src[..4].eq_ignore_ascii_case("cid:") {
            self.images.get(&normalize_cid(src))
        } else {
            self.images.get(src)
        };
        self.budget.filter(hit.cloned())
    }
}

/// docling's `_normalize_content_id`: `cid:<id>` lower-cased, whatever
/// spelling (`<…>` brackets, a `cid:` prefix, case) the header or `src` used.
pub(crate) fn normalize_cid(value: &str) -> String {
    let mut id = value.trim();
    if id.len() >= 4 && id[..4].eq_ignore_ascii_case("cid:") {
        id = &id[4..];
    }
    format!("cid:{}", id.trim_matches(['<', '>']).to_lowercase())
}

/// Decode a `data:[<mime>][;base64],<payload>` image URI.
pub(crate) fn from_data_uri(uri: &str) -> Option<PictureImage> {
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

#[cfg(feature = "fetch-images")]
use crate::fetch::is_blocked_ip;

/// `true` when the URL's host resolves to a blocked address and the operator
/// hasn't opted out with `DOCLING_RS_ALLOW_PRIVATE_IP_FETCH` (the same flag
/// docling-serve's URL fetch honors, for local/intranet development). The
/// host allow-list of a policy does **not** lift this block: a request may
/// name the hosts it wants, it may not reach the metadata service through
/// them (a deliberate narrowing of #646's proposal).
#[cfg(feature = "fetch-images")]
fn blocked_by_ssrf_guard(url: &str) -> bool {
    use std::net::ToSocketAddrs;
    if docling_core::env::flag("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH") {
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

/// Whether `url` may be fetched under `policy`: a `http(s)` URL whose host
/// passes the allow-list and the SSRF guard. Checked on the first URL and
/// on every redirect target.
#[cfg(feature = "fetch-images")]
fn url_permitted(url: &str, policy: &ImagePolicy) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    policy.host_allowed(host) && !blocked_by_ssrf_guard(url)
}

/// Fetch a remote image over HTTP(S) under `policy`. The mimetype comes from
/// `Content-Type` when it's an `image/*`, else it's guessed from the URL's
/// extension. Bounded: the host allow-list, an SSRF block-list, the
/// per-image size cap, a connect/overall timeout, and a redirect cap keep
/// one hostile or slow `<img src>` from hanging the whole conversion. The
/// caller charges the per-document budget.
#[cfg(feature = "fetch-images")]
pub(crate) fn fetch_remote_with(url: &str, policy: &ImagePolicy) -> Option<PictureImage> {
    use std::time::Duration;
    if !url_permitted(url, policy) {
        return None;
    }
    // Redirects are followed by hand so the allow-list and the SSRF guard
    // see every hop: with the client following them itself only the first
    // URL was checked, and a public host could 30x-bounce the fetch onto a
    // private address or off the allow-list. The connect goes only to the
    // addresses the guarded resolver validated (#752).
    let agent = crate::fetch::guarded_agent(
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(Duration::from_secs(20)))
            .max_redirects(0)
            .build(),
        false,
    );
    let mut target = url.to_string();
    let mut resp = None;
    for _ in 0..=3 {
        let r = agent.get(&target).call().ok()?;
        if !r.status().is_redirection() {
            resp = Some(r);
            break;
        }
        let location = r.headers().get("location")?.to_str().ok()?;
        target = url::Url::parse(&target)
            .ok()?
            .join(location)
            .ok()?
            .to_string();
        if !url_permitted(&target, policy) {
            return None;
        }
    }
    let mut resp = resp?;
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
        .limit(policy.limits.max_bytes)
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
        dpi: PictureImage::DEFAULT_DPI,
        mimetype: mimetype.into(),
        width,
        height,
        data,
        display_size: None,
    })
}

/// A resolved picture with its EXIF orientation applied (docling#5246,
/// #1148): docling's image resource loader (HTML, Markdown, AsciiDoc, JATS,
/// EPUB) and its MHTML branch run `ImageOps.exif_transpose`, so a camera
/// photo stored sideways with an orientation tag is kept upright. A tagged
/// image becomes upright pixels re-encoded as PNG (docling's
/// `ImageRef.from_pil` writes PNG anyway), width and height swapped for
/// orientations 5–8; an untagged one — every image without EXIF — keeps
/// its bytes, so outputs and digests do not move. An image that does not
/// decode is left as it is. The office backends read their pictures
/// without it, as docling's do.
pub(crate) fn upright(pic: PictureImage) -> PictureImage {
    match upright_png(&pic.data) {
        Some((width, height, data)) => PictureImage {
            mimetype: "image/png".into(),
            width,
            height,
            data,
            ..pic
        },
        None => pic,
    }
}

/// The upright PNG of a tagged image (`width`, `height`, bytes); `None` for
/// an untagged or undecodable one.
fn upright_png(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    use image::metadata::Orientation;
    use image::ImageDecoder;
    let mut decoder = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?
        .into_decoder()
        .ok()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    if orientation == Orientation::NoTransforms {
        return None;
    }
    let mut img = image::DynamicImage::from_decoder(decoder).ok()?;
    img.apply_orientation(orientation);
    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some((img.width(), img.height(), png))
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

    fn policy(sources: ImageSources) -> ImagePolicy {
        ImagePolicy {
            sources,
            hosts: Vec::new(),
            limits: ImageLimits::default(),
        }
    }

    fn red_uri() -> String {
        format!("data:image/png;base64,{}", encode(RED_PNG))
    }

    #[test]
    fn map_resolver_returns_by_key_under_embedded() {
        let img = from_data_uri(&red_uri()).unwrap();
        let mut map = HashMap::new();
        map.insert("images/x.png".to_string(), img.clone());
        let r = MapImageResolver::new(map.clone(), &policy(ImageSources::Embedded));
        assert_eq!(r.resolve("images/x.png"), Some(img.clone()));
        assert!(r.resolve("images/missing.png").is_none());
        assert_eq!(r.resolve(&red_uri()), Some(img.clone()));
        // The `None` tier resolves nothing, container parts included.
        let off = MapImageResolver::new(map, &policy(ImageSources::None));
        assert!(off.resolve("images/x.png").is_none());
        assert!(off.resolve(&red_uri()).is_none());
    }

    /// docling#5246's fixture (#1148): a 200 × 100 JPEG, a black 20 × 20
    /// corner top-left, EXIF orientation 6 — an `APP1` written by hand.
    fn exif6_jpeg() -> Vec<u8> {
        let img = image::RgbImage::from_fn(200, 100, |x, y| {
            if x < 20 && y < 20 {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([255, 255, 255])
            }
        });
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        // TIFF header (little endian), one IFD entry: Orientation (0x0112),
        // SHORT, count 1, value 6; no next IFD.
        let mut exif = b"Exif\0\0II*\0\x08\0\0\0\x01\0".to_vec();
        exif.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0]);
        let mut app1 = vec![0xFF, 0xE1];
        app1.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        app1.extend_from_slice(&exif);
        [&jpeg[..2], &app1, &jpeg[2..]].concat()
    }

    /// The picture is upright: 100 × 200, the dark corner at the top right.
    fn assert_upright(pic: &PictureImage) {
        assert_eq!(
            (pic.width, pic.height, pic.mimetype.as_str()),
            (100, 200, "image/png")
        );
        let px = image::load_from_memory(&pic.data).unwrap().to_rgb8();
        assert!(
            px.get_pixel(pic.width - 3, 2).0[0] < 64,
            "dark corner top right"
        );
        assert!(px.get_pixel(2, 2).0[0] > 192, "light top left");
    }

    /// docling#5246 (#1148): `test_create_image_ref_applies_exif_orientation`
    /// (a `data:` URI), `test_embedded_image_follows_exif_orientation` (an
    /// MHTML `cid:` part, through the map resolver) and a relative local file
    /// under `Local` — each picture comes out upright, width and height
    /// swapped; an untagged image keeps its bytes.
    #[test]
    fn resolved_pictures_follow_the_exif_orientation() {
        let jpeg = exif6_jpeg();
        let uri = format!(
            "data:image/jpeg;base64,{}",
            docling_core::base64::encode(&jpeg)
        );
        let dir = std::env::temp_dir().join(format!("docling.rs_exif_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("photo.jpg"), &jpeg).unwrap();
        let r = FsImageResolver::new(Some(dir.clone()), None, policy(ImageSources::Local));
        assert_upright(&r.resolve(&uri).unwrap());
        assert_upright(&r.resolve("photo.jpg").unwrap());
        let mut map = HashMap::new();
        map.insert(
            "cid:photo@x".to_string(),
            build_picture("image/jpeg", jpeg).unwrap(),
        );
        let m = MapImageResolver::new(map, &policy(ImageSources::Embedded));
        assert_upright(&m.resolve("cid:photo@x").unwrap());
        assert_eq!(m.resolve(&red_uri()), from_data_uri(&red_uri()), "untagged");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// docling#5082 (#1180, `test_percent_escaped_local_image` and
    /// `…_keeps_confinement`): a relative `src` is percent-decoded once,
    /// its query and fragment dropped; a decoded path that escapes the base
    /// directory, or reads as absolute, is still refused.
    #[test]
    fn percent_escaped_local_images_resolve_within_the_base_dir() {
        let root = std::env::temp_dir().join(format!("docling.rs_img1180_{}", std::process::id()));
        let base = root.join("doc");
        std::fs::create_dir_all(&base).unwrap();
        for name in [
            "figure one.png",
            "图 片.png",
            "figure%20one.png",
            "figure#one.png",
        ] {
            std::fs::write(base.join(name), RED_PNG).unwrap();
        }
        std::fs::write(root.join("outside.png"), RED_PNG).unwrap();
        let r = FsImageResolver::new(Some(base.clone()), None, policy(ImageSources::Local));
        for src in [
            "figure%20one.png?v=1#preview",
            "%E5%9B%BE%20%E7%89%87.png?v=1#preview",
            "figure%2520one.png?v=1#preview",
            "figure%23one.png?v=1#preview",
            "figure one.png",
        ] {
            assert!(r.resolve(src).is_some(), "{src}");
        }
        for src in [
            "%2e%2e/outside.png",
            "%2F%2Fimages.test/outside.png",
            "%5C..%5Coutside.png",
        ] {
            assert!(r.resolve(src).is_none(), "{src}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sources_parse_and_order() {
        assert_eq!(
            "Embedded".parse::<ImageSources>(),
            Ok(ImageSources::Embedded)
        );
        assert_eq!(" remote ".parse::<ImageSources>(), Ok(ImageSources::Remote));
        assert!("all".parse::<ImageSources>().is_err());
        assert!(ImageSources::Remote.local() && ImageSources::Remote.embedded());
        assert!(!ImageSources::Local.remote() && ImageSources::Local.embedded());
        assert!(!ImageSources::None.embedded());
        assert_eq!(ImageSources::default(), ImageSources::None);
    }

    #[test]
    fn host_allow_list_matches_exact_and_wildcard() {
        let mut p = policy(ImageSources::Remote);
        assert!(p.host_allowed("anything.example"));
        p.hosts = vec!["cdn.example.com".into(), "*.static.example".into()];
        assert!(p.host_allowed("cdn.example.com"));
        assert!(p.host_allowed("CDN.Example.COM"));
        assert!(!p.host_allowed("example.com"));
        assert!(!p.host_allowed("evil-cdn.example.com"));
        assert!(p.host_allowed("a.static.example"));
        assert!(p.host_allowed("b.a.static.example"));
        assert!(!p.host_allowed("static.example"));
    }

    /// #646 acceptance 1: under `Embedded` only the `data:` image resolves —
    /// no file is read (a path to a real file stays a placeholder) and no
    /// network is touched (a URL to a counting local server sees no hit).
    #[test]
    fn embedded_resolves_data_uris_only() {
        let dir = std::env::temp_dir().join(format!("docling.rs_img646_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("local.png"), RED_PNG).unwrap();
        let r = FsImageResolver::new(Some(dir.clone()), None, policy(ImageSources::Embedded));
        assert!(r.resolve(&red_uri()).is_some());
        assert!(
            r.resolve("local.png").is_none(),
            "no filesystem under Embedded"
        );
        assert!(r.resolve(dir.join("local.png").to_str().unwrap()).is_none());
        assert!(r.resolve("/etc/hosts").is_none());
        assert!(r.resolve("https://example.com/x.png").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #646 acceptance 2: under `Local` a file inside the source directory
    /// resolves, one that escapes it (`..`, an absolute path, `file://`, a
    /// symlink pointing out) is refused; without a base directory nothing
    /// local resolves (never the working directory).
    #[test]
    fn local_reads_are_confined_to_the_base_dir() {
        let root = std::env::temp_dir().join(format!("docling.rs_img646l_{}", std::process::id()));
        let base = root.join("doc");
        std::fs::create_dir_all(base.join("img")).unwrap();
        std::fs::write(base.join("img/inside.png"), RED_PNG).unwrap();
        std::fs::write(root.join("outside.png"), RED_PNG).unwrap();
        let r = FsImageResolver::new(Some(base.clone()), None, policy(ImageSources::Local));
        assert!(r.resolve("img/inside.png").is_some());
        assert!(
            r.resolve("img/../img/inside.png").is_some(),
            "`..` that stays inside"
        );
        assert!(
            r.resolve("../outside.png").is_none(),
            "escapes the base dir"
        );
        assert!(r.resolve("img/../../outside.png").is_none());
        assert!(
            r.resolve(root.join("outside.png").to_str().unwrap())
                .is_none(),
            "absolute"
        );
        assert!(r
            .resolve(&format!("file://{}", base.join("img/inside.png").display()))
            .is_none());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("outside.png"), base.join("img/link.png"))
                .unwrap();
            assert!(
                r.resolve("img/link.png").is_none(),
                "symlink out of the base dir"
            );
        }
        assert!(r.resolve(&red_uri()).is_some(), "Local includes Embedded");
        assert!(
            FsImageResolver::new(None, None, policy(ImageSources::Local))
                .resolve("img/inside.png")
                .is_none()
        );
        // Remote includes Local.
        let rr = FsImageResolver::new(Some(base.clone()), None, policy(ImageSources::Remote));
        assert!(rr.resolve("img/inside.png").is_some());
        assert!(rr.resolve("../outside.png").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// #646 acceptance 6: the per-document limits — a count cap keeps the
    /// first image and refuses the rest, a size cap and a size floor refuse
    /// per image, a total cap stops once the sum is reached.
    #[test]
    fn limits_keep_the_budget() {
        let mut p = policy(ImageSources::Embedded);
        p.limits.max_images = Some(1);
        let r = FsImageResolver::new(None, None, p.clone());
        assert!(r.resolve(&red_uri()).is_some());
        assert!(
            r.resolve(&red_uri()).is_none(),
            "second image over max_images=1"
        );

        let mut p = policy(ImageSources::Embedded);
        p.limits.max_bytes = (RED_PNG.len() - 1) as u64;
        assert!(FsImageResolver::new(None, None, p)
            .resolve(&red_uri())
            .is_none());

        let mut p = policy(ImageSources::Embedded);
        p.limits.min_bytes = (RED_PNG.len() + 1) as u64;
        assert!(FsImageResolver::new(None, None, p)
            .resolve(&red_uri())
            .is_none());

        let mut p = policy(ImageSources::Embedded);
        p.limits.max_total_bytes = Some((RED_PNG.len() * 2) as u64);
        let r = FsImageResolver::new(None, None, p);
        assert!(r.resolve(&red_uri()).is_some());
        assert!(r.resolve(&red_uri()).is_some());
        assert!(
            r.resolve(&red_uri()).is_none(),
            "third image over the total"
        );
    }

    #[test]
    fn limits_read_the_environment() {
        std::env::set_var("DOCLING_RS_MAX_IMAGES", "3");
        std::env::set_var("DOCLING_RS_MAX_IMAGE_TOTAL_MB", "2");
        std::env::set_var("DOCLING_RS_MIN_IMAGE_BYTES", "64");
        std::env::set_var("DOCLING_RS_MAX_IMAGE_BYTES", "1000");
        let l = ImageLimits::from_env();
        assert_eq!(l.max_images, Some(3));
        assert_eq!(l.max_total_bytes, Some(2 * 1024 * 1024));
        assert_eq!(l.min_bytes, 64);
        assert_eq!(l.max_bytes, 1000);
        for k in [
            "DOCLING_RS_MAX_IMAGES",
            "DOCLING_RS_MAX_IMAGE_TOTAL_MB",
            "DOCLING_RS_MIN_IMAGE_BYTES",
            "DOCLING_RS_MAX_IMAGE_BYTES",
        ] {
            std::env::remove_var(k);
        }
        assert_eq!(ImageLimits::from_env(), ImageLimits::default());
    }

    #[cfg(feature = "fetch-images")]
    #[test]
    fn resolves_relative_and_protocol_relative_against_base_url() {
        // The URL join the fetch path relies on, without touching the network.
        let r = FsImageResolver::new(
            None,
            Some("https://ex.com/a/page.html".into()),
            policy(ImageSources::Remote),
        );
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
        let no_base = FsImageResolver::new(None, None, policy(ImageSources::Remote));
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
        let r = FsImageResolver::new(None, Some(base), policy(ImageSources::Remote));

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

        // Left set on purpose: the host allow-list test below shares it and
        // the two run concurrently.
    }

    /// #646 acceptance 3: under `Remote` with an allow-list, a URL on another
    /// host is refused before any connection, and a redirect from an allowed
    /// host to one off the list is refused too — proven against a local
    /// server that counts its hits and bounces `/bounce` elsewhere.
    #[cfg(feature = "fetch-images")]
    #[test]
    fn remote_host_allow_list_holds_for_redirects() {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let server_hits = Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                server_hits.fetch_add(1, Ordering::Relaxed);
                let response = if request.starts_with("GET /bounce") {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://localhost:{}/ok.png\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        addr.port()
                    )
                    .into_bytes()
                } else {
                    let mut r = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        RED_PNG.len()
                    )
                    .into_bytes();
                    r.extend_from_slice(RED_PNG);
                    r
                };
                let _ = stream.write_all(&response);
                let _ = stream.flush();
            }
        });
        std::env::set_var("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH", "1");

        let mut p = policy(ImageSources::Remote);
        p.hosts = vec!["127.0.0.1".into()];
        let r = FsImageResolver::new(None, None, p.clone());
        assert!(r
            .resolve(&format!("http://127.0.0.1:{}/ok.png", addr.port()))
            .is_some());
        assert_eq!(hits.load(Ordering::Relaxed), 1);
        // Another host: refused without a connection.
        assert!(r
            .resolve(&format!("http://localhost:{}/ok.png", addr.port()))
            .is_none());
        assert_eq!(
            hits.load(Ordering::Relaxed),
            1,
            "no request to a host off the list"
        );
        // A redirect off the list: the first hop is served, the second refused.
        assert!(r
            .resolve(&format!("http://127.0.0.1:{}/bounce", addr.port()))
            .is_none());
        assert_eq!(
            hits.load(Ordering::Relaxed),
            2,
            "the bounce target was never fetched"
        );

        // Without the allow-list the same redirect is followed.
        let open = FsImageResolver::new(None, None, policy(ImageSources::Remote));
        assert!(open
            .resolve(&format!("http://127.0.0.1:{}/bounce", addr.port()))
            .is_some());
        assert_eq!(hits.load(Ordering::Relaxed), 4);

        // Below `Remote`, a URL is never fetched at all.
        let local = FsImageResolver::new(None, None, policy(ImageSources::Local));
        assert!(local
            .resolve(&format!("http://127.0.0.1:{}/ok.png", addr.port()))
            .is_none());
        assert_eq!(hits.load(Ordering::Relaxed), 4);
    }
}
