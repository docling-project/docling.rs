//! Loading a document from somewhere other than a file: a URL (#668), a
//! `data:` URI, a reader such as stdin.
//!
//! The URL fetch is the one docling-serve has run since its URL inputs — the
//! SSRF guard ([`check_outbound_url`]), the size and time bounds, the
//! name-and-format resolution from `Content-Disposition`, the URL path and
//! `Content-Type` — lifted here so the CLI and the library share it. The
//! policy is explicit ([`FetchPolicy`]): a server refuses redirects and keeps
//! the private-address block on, a CLI run on a developer's laptop follows a
//! few redirects and may be pointed at `localhost`.
//!
//! Every fetch connects through [`guarded_agent`]: the host is resolved once,
//! refused when any address it maps to is not globally routable, and the
//! connection goes to the addresses validated in that same resolution — so a
//! DNS-rebinding server cannot answer the pre-flight check with a public
//! address and the connect with `127.0.0.1` (#752, docling-core#846).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::error::ConversionError;
use crate::source::SourceDocument;

/// Whether a resolved IP may not be fetched: anything that is not a globally
/// routable unicast address — docling-core's `_is_address_safe` (`is_global`
/// and not private, loopback, link-local, reserved, multicast or
/// unspecified), the SSRF block-list behind every URL docling fetches on
/// someone else's behalf (docling-serve's URL inputs and `vlm_endpoint`, the
/// `<img src>` fetch, the CLI's URL sources). Without it a request for
/// `http://169.254.169.254/…` or `http://127.0.0.1:…` reaches cloud metadata
/// and internal services from the converter's network position. An
/// IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) is judged as its IPv4 address.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !is_global_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => !is_global_v4(v4),
            None => !is_global_v6(v6),
        },
    }
}

/// Python's `IPv4Address.is_global` (its `_private_networks` and the shared
/// 100.64.0.0/10), with multicast added as docling-core does.
fn is_global_v4(ip: Ipv4Addr) -> bool {
    let n = u32::from(ip);
    let within = |net: [u8; 4], prefix: u32| {
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        };
        n & mask == u32::from(Ipv4Addr::from(net)) & mask
    };
    // 192.0.0.9 and 192.0.0.10 (PCP / TURN anycast) are global inside the
    // otherwise non-global 192.0.0.0/29.
    if n == u32::from(Ipv4Addr::new(192, 0, 0, 9)) || n == u32::from(Ipv4Addr::new(192, 0, 0, 10)) {
        return true;
    }
    const NON_GLOBAL: &[([u8; 4], u32)] = &[
        ([0, 0, 0, 0], 8),       // "this network", incl. 0.0.0.0
        ([10, 0, 0, 0], 8),      // private
        ([100, 64, 0, 0], 10),   // carrier-grade NAT (shared)
        ([127, 0, 0, 0], 8),     // loopback
        ([169, 254, 0, 0], 16),  // link-local, cloud metadata
        ([172, 16, 0, 0], 12),   // private
        ([192, 0, 0, 0], 29),    // IETF protocol assignments
        ([192, 0, 0, 170], 31),  // NAT64 discovery
        ([192, 0, 2, 0], 24),    // TEST-NET-1
        ([192, 168, 0, 0], 16),  // private
        ([198, 18, 0, 0], 15),   // benchmarking
        ([198, 51, 100, 0], 24), // TEST-NET-2
        ([203, 0, 113, 0], 24),  // TEST-NET-3
        ([224, 0, 0, 0], 4),     // multicast
        ([240, 0, 0, 0], 4),     // reserved, incl. 255.255.255.255
    ];
    !NON_GLOBAL.iter().any(|&(net, prefix)| within(net, prefix))
}

/// Python's `IPv6Address.is_global` with docling-core's extra exclusions:
/// only the global unicast block 2000::/3 can be routable (everything else is
/// loopback, unspecified, reserved — the IPv4-compatible and NAT64
/// `64:ff9b::/96` forms included —, unique-local, link-local or
/// multicast), minus its non-global parts: the IETF protocol assignments
/// 2001::/23 (but for the global anycast and AMT/ORCHIDv2 ranges in it),
/// documentation 2001:db8::/32 and 6to4 2002::/16.
fn is_global_v6(ip: Ipv6Addr) -> bool {
    let n = u128::from(ip);
    let within = |net: Ipv6Addr, prefix: u32| {
        let mask = if prefix == 0 {
            0
        } else {
            u128::MAX << (128 - prefix)
        };
        n & mask == u128::from(net) & mask
    };
    if !within(Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0), 3) {
        return false;
    }
    const GLOBAL_IN_2001: &[(Ipv6Addr, u32)] = &[
        (Ipv6Addr::new(0x2001, 1, 0, 0, 0, 0, 0, 1), 128),
        (Ipv6Addr::new(0x2001, 1, 0, 0, 0, 0, 0, 2), 128),
        (Ipv6Addr::new(0x2001, 3, 0, 0, 0, 0, 0, 0), 32),
        (Ipv6Addr::new(0x2001, 4, 0x112, 0, 0, 0, 0, 0), 48),
        (Ipv6Addr::new(0x2001, 0x20, 0, 0, 0, 0, 0, 0), 28),
        (Ipv6Addr::new(0x2001, 0x30, 0, 0, 0, 0, 0, 0), 28),
    ];
    if within(Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23) {
        return GLOBAL_IN_2001
            .iter()
            .any(|&(net, prefix)| within(net, prefix));
    }
    !(within(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32)
        || within(Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16))
}

/// Operator opt-out of the private-address block for local development
/// (`DOCLING_RS_ALLOW_PRIVATE_IP_FETCH=1`): a URL like
/// `http://localhost:8080/doc.pdf` can then be fetched. Off by default —
/// leave it unset in production.
pub fn allow_private_ip_fetch() -> bool {
    docling_core::env::flag("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH")
}

/// SSRF pre-check for a URL supplied by whoever asked for the conversion:
/// `http(s)` scheme only, then resolve the host and refuse one that maps to
/// a private / loopback / link-local address — unless `allow_private` (or
/// the `DOCLING_RS_ALLOW_PRIVATE_IP_FETCH` opt-out) lifts the block. A
/// fetch that follows redirects must re-run this on every hop: a public URL
/// can 30x-bounce onto an internal target. A pre-flight check: the connect
/// itself goes through [`guarded_agent`], which validates the addresses it
/// connects to in the same resolution, so a rebinding DNS answer between this
/// check and the connect is refused there. Blocking (DNS).
#[cfg(feature = "fetch-url")]
pub fn check_outbound_url(url: &str, allow_private: bool) -> Result<(), String> {
    use std::net::ToSocketAddrs;
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(format!("unsupported URL scheme in '{url}'"));
    }
    let parsed = url::Url::parse(url).map_err(|e| format!("bad URL '{url}': {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| format!("no host in URL '{url}'"))?;
    let port = parsed.port_or_known_default().unwrap_or(80);
    let mut resolved = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host}: {e}"))?
        .peekable();
    if resolved.peek().is_none() {
        return Err(format!("cannot resolve {host}"));
    }
    if !allow_private && !allow_private_ip_fetch() {
        for addr in resolved {
            if is_blocked_ip(addr.ip()) {
                return Err(format!(
                    "refusing to fetch {url}: resolves to a private/loopback address \
                     (set DOCLING_RS_ALLOW_PRIVATE_IP_FETCH=1 for local development)"
                ));
            }
        }
    }
    Ok(())
}

/// The `ureq` resolver every guarded fetch connects through — docling-core's
/// `_SafeConnectionAdapter` (docling-core#846): the host is resolved once and
/// the request refused when any address is blocked ([`is_blocked_ip`]);
/// otherwise `ureq` connects to exactly those validated addresses, never
/// resolving the name again. A proxy's own address is resolved as usual: the
/// proxy resolves the target itself, so a proxied request relies on the
/// pre-flight [`check_outbound_url`] (as upstream's does).
#[cfg(any(feature = "fetch-url", feature = "fetch-images"))]
#[derive(Debug)]
pub struct GuardedResolver<R = ureq::unversioned::resolver::DefaultResolver> {
    inner: R,
    allow_private: bool,
}

#[cfg(any(feature = "fetch-url", feature = "fetch-images"))]
impl GuardedResolver {
    /// The system resolver behind the guard; `allow_private` (or the
    /// `DOCLING_RS_ALLOW_PRIVATE_IP_FETCH` opt-out) lifts the block.
    pub fn new(allow_private: bool) -> Self {
        Self::with_inner(Default::default(), allow_private)
    }
}

#[cfg(any(feature = "fetch-url", feature = "fetch-images"))]
impl<R> GuardedResolver<R> {
    pub fn with_inner(inner: R, allow_private: bool) -> Self {
        Self {
            inner,
            allow_private: allow_private || allow_private_ip_fetch(),
        }
    }
}

#[cfg(any(feature = "fetch-url", feature = "fetch-images"))]
impl<R: ureq::unversioned::resolver::Resolver> ureq::unversioned::resolver::Resolver
    for GuardedResolver<R>
{
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let addrs = self.inner.resolve(uri, config, timeout)?;
        let is_proxy = config.proxy().is_some_and(|p| {
            uri.host() == Some(p.host())
                && uri.port_u16().or(match uri.scheme_str() {
                    Some("https") => Some(443),
                    Some("http") => Some(80),
                    _ => None,
                }) == Some(p.port())
        });
        if self.allow_private || is_proxy {
            return Ok(addrs);
        }
        if let Some(blocked) = addrs.iter().find(|a| is_blocked_ip(a.ip())) {
            return Err(ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "refusing to connect to {}: resolves to a non-public address {} \
                     (set DOCLING_RS_ALLOW_PRIVATE_IP_FETCH=1 for local development)",
                    uri.host().unwrap_or_default(),
                    blocked.ip()
                ),
            )));
        }
        Ok(addrs)
    }
}

/// A `ureq` agent with `config` that connects only through
/// [`GuardedResolver`] — the agent for every fetch made on someone else's
/// behalf.
#[cfg(any(feature = "fetch-url", feature = "fetch-images"))]
pub fn guarded_agent(config: ureq::config::Config, allow_private: bool) -> ureq::Agent {
    ureq::Agent::with_parts(
        config,
        ureq::unversioned::transport::DefaultConnector::new(),
        GuardedResolver::new(allow_private),
    )
}

/// Largest URL-fetch response accepted by default (256 MiB): `read_to_end`
/// on a response is otherwise unbounded, and a hostile URL streaming an
/// endless body would exhaust memory. `DOCLING_RS_MAX_FETCH_BYTES`
/// overrides it for every policy built with [`FetchPolicy::default`].
pub const DEFAULT_MAX_FETCH_BYTES: u64 = 256 * 1024 * 1024;

/// The `Accept` header a document fetch sends: a server that negotiates
/// (a wiki, a docs site, an API that answers in Markdown or HTML) is asked
/// for Markdown first, HTML next, and anything it has otherwise — a PDF or
/// a DOCX link answers with its own type regardless.
pub const ACCEPT_DOCUMENT: &str = "text/markdown, text/html;q=0.9, */*;q=0.1";

/// How a URL is fetched — bounds and the SSRF stance.
#[derive(Debug, Clone)]
pub struct FetchPolicy {
    /// Largest response body accepted; a longer one is an error, not
    /// truncated. Default: `DOCLING_RS_MAX_FETCH_BYTES` or
    /// [`DEFAULT_MAX_FETCH_BYTES`].
    pub max_bytes: u64,
    /// Redirect hops followed, each re-checked against the SSRF guard. A
    /// server should keep it at 0 (docling-serve refuses redirects outright);
    /// the CLI follows a few. Default 0.
    pub max_redirects: u32,
    /// Connect timeout. Default 10 s.
    pub connect_timeout: std::time::Duration,
    /// Whole-transfer timeout. Default 300 s.
    pub timeout: std::time::Duration,
    /// Lift the private / loopback address block (the per-call form of
    /// `DOCLING_RS_ALLOW_PRIVATE_IP_FETCH`). Default `false`.
    pub allow_private: bool,
    /// Extra request headers sent verbatim (docling's `HttpSource.headers`,
    /// #139 — auth tokens for protected origins). `Accept` defaults to
    /// [`ACCEPT_DOCUMENT`] unless one is given here.
    pub headers: Vec<(String, String)>,
}

impl Default for FetchPolicy {
    fn default() -> Self {
        Self {
            max_bytes: docling_core::env::parse("DOCLING_RS_MAX_FETCH_BYTES")
                .unwrap_or(DEFAULT_MAX_FETCH_BYTES),
            max_redirects: 0,
            connect_timeout: std::time::Duration::from_secs(10),
            timeout: std::time::Duration::from_secs(300),
            allow_private: false,
            headers: Vec::new(),
        }
    }
}

/// What a fetch answered with, before it becomes a document: the final
/// URL, the body, and the two headers the name and format come from.
#[cfg(feature = "fetch-url")]
#[derive(Debug)]
pub struct Fetched {
    /// The URL the body came from — the last hop when redirects were followed.
    pub url: String,
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
    pub content_disposition: Option<String>,
}

/// Fetch `url` under `policy` (blocking). Every hop passes
/// [`check_outbound_url`]; a redirect past `policy.max_redirects` is an
/// error, as is a body longer than `policy.max_bytes`.
#[cfg(feature = "fetch-url")]
pub fn fetch(url: &str, policy: &FetchPolicy) -> Result<Fetched, ConversionError> {
    use std::io::Read;
    let agent = guarded_agent(
        ureq::Agent::config_builder()
            .max_redirects(0)
            .timeout_connect(Some(policy.connect_timeout))
            .timeout_global(Some(policy.timeout))
            .build(),
        policy.allow_private,
    );
    let mut target = url.to_string();
    let mut hops = 0;
    loop {
        check_outbound_url(&target, policy.allow_private).map_err(ConversionError::Fetch)?;
        let mut request = agent.get(&target);
        if !policy
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("accept"))
        {
            request = request.header("Accept", ACCEPT_DOCUMENT);
        }
        for (k, v) in &policy.headers {
            request = request.header(k, v);
        }
        let mut response = request
            .call()
            .map_err(|e| ConversionError::Fetch(format!("fetching {target}: {e}")))?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    ConversionError::Fetch(format!(
                        "fetching {target}: redirect without a Location header"
                    ))
                })?;
            hops += 1;
            if hops > policy.max_redirects {
                return Err(ConversionError::Fetch(format!(
                    "fetching {url}: too many redirects (more than {}; last to {location})",
                    policy.max_redirects
                )));
            }
            target = url::Url::parse(&target)
                .ok()
                .and_then(|base| base.join(location).ok())
                .map(String::from)
                .ok_or_else(|| {
                    ConversionError::Fetch(format!("fetching {target}: bad redirect to {location}"))
                })?;
            continue;
        }
        if !response.status().is_success() {
            return Err(ConversionError::Fetch(format!(
                "fetching {target}: HTTP {}",
                response.status()
            )));
        }
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let content_type = header("content-type");
        let content_disposition = header("content-disposition");
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(policy.max_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| ConversionError::Fetch(format!("reading {target}: {e}")))?;
        if bytes.len() as u64 > policy.max_bytes {
            return Err(ConversionError::Fetch(format!(
                "response from {target} exceeds {} bytes",
                policy.max_bytes
            )));
        }
        return Ok(Fetched {
            url: target,
            bytes,
            content_type,
            content_disposition,
        });
    }
}

/// The file name a fetch should be known by, in docling's order: the
/// `Content-Disposition` `filename` (its RFC 5987 `filename*` form first),
/// then the URL path's last segment, then `document`.
#[cfg(feature = "fetch-url")]
pub fn fetched_name(url: &str, content_disposition: Option<&str>) -> String {
    content_disposition
        .and_then(disposition_filename)
        .or_else(|| url_file_name(url))
        .unwrap_or_else(|| "document".to_string())
}

/// The last path segment of `url` (query and fragment stripped,
/// percent-decoded), or `None` for an origin / a trailing slash.
#[cfg(feature = "fetch-url")]
pub fn url_file_name(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let last = parsed.path_segments()?.next_back()?;
    if last.is_empty() {
        return None;
    }
    let decoded = percent_decode(last);
    let name = String::from_utf8_lossy(&decoded).into_owned();
    // A decoded segment that carries a path separator is not a file name.
    if name.contains(['/', '\\']) || name == "." || name == ".." {
        return None;
    }
    Some(name)
}

/// The local path a `file:` URL names (`file:///home/me/a.pdf`,
/// `file:a.pdf` is not one — RFC 8089 wants the authority slashes).
#[cfg(feature = "fetch-url")]
pub fn file_url_path(url: &str) -> Result<std::path::PathBuf, String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("bad URL '{url}': {e}"))?;
    if parsed.scheme() != "file" {
        return Err(format!("not a file: URL: '{url}'"));
    }
    parsed
        .to_file_path()
        .map_err(|()| format!("'{url}' is not a local file path"))
}

/// `filename` from a `Content-Disposition` value: `filename*=UTF-8''a%20b.pdf`
/// wins over `filename="a b.pdf"`, quotes and any directory part dropped.
pub fn disposition_filename(value: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;
    for part in value.split(';').skip(1) {
        let Some((key, raw)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let raw = raw.trim();
        if key == "filename*" {
            // RFC 5987: charset'language'percent-encoded.
            let encoded = raw.rsplit('\'').next().unwrap_or(raw);
            let decoded = String::from_utf8_lossy(&percent_decode(encoded)).into_owned();
            extended = Some(decoded);
        } else if key == "filename" {
            plain = Some(raw.trim_matches('"').to_string());
        }
    }
    extended
        .or(plain)
        .map(|n| {
            n.rsplit(['/', '\\'])
                .next()
                .unwrap_or(&n)
                .trim()
                .to_string()
        })
        .filter(|n| !n.is_empty() && n != "." && n != "..")
}

/// A parsed `data:` URI (RFC 2397): the payload decoded, its media type
/// and the optional `name=` parameter kept for the caller to type and name
/// the document with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataUrl {
    pub media_type: Option<String>,
    pub name: Option<String>,
    pub bytes: Vec<u8>,
}

/// Parse a `data:[<media type>][;name=<file name>][;base64],<payload>` URI.
pub fn parse_data_url(url: &str) -> Result<DataUrl, ConversionError> {
    let rest = strip_data_scheme(url)
        .ok_or_else(|| ConversionError::Fetch(format!("not a data: URL: '{url}'")))?;
    let (meta, payload) = rest.split_once(',').ok_or_else(|| {
        ConversionError::Fetch("malformed data: URL: no ',' before the payload".to_string())
    })?;
    let mut params = meta.split(';');
    let media_type = params.next().filter(|m| !m.is_empty()).map(str::to_string);
    let mut base64 = false;
    let mut name = None;
    for p in params {
        if p.eq_ignore_ascii_case("base64") {
            base64 = true;
        } else if let Some(v) = p.strip_prefix("name=") {
            name = Some(String::from_utf8_lossy(&percent_decode(v.trim_matches('"'))).into_owned());
        }
    }
    let bytes = if base64 {
        docling_core::base64::decode(payload.trim()).ok_or_else(|| {
            ConversionError::Fetch("malformed data: URL: invalid base64 payload".to_string())
        })?
    } else {
        percent_decode(payload)
    };
    Ok(DataUrl {
        media_type,
        name,
        bytes,
    })
}

/// Decode a `data:` URI into a document: the payload is the bytes, the
/// media type names the format ([`crate::InputFormat::from_mime`]); without
/// one the bytes are sniffed. A `name=` parameter, when present, is the
/// document's name; a `charset=` other than UTF-8
/// (`data:text/csv;charset=shift_jis,…`) is the text encoding the
/// converter decodes with ([`SourceDocument::with_encoding`]).
pub fn from_data_url(url: &str) -> Result<SourceDocument, ConversionError> {
    let data = parse_data_url(url)?;
    let source = SourceDocument::from_named_bytes(
        data.name.as_deref().unwrap_or("document"),
        data.bytes,
        data.media_type.as_deref(),
    )?;
    Ok(match data_url_charset(url) {
        Some(label) if !label.eq_ignore_ascii_case("utf-8") => source.with_encoding(Some(label)),
        _ => source,
    })
}

/// `url` after its `data:` scheme — matched without regard to case, as URI
/// schemes are (RFC 3986 §3.1: `DATA:` is the same scheme).
fn strip_data_scheme(url: &str) -> Option<&str> {
    url.get(..5)
        .filter(|scheme| scheme.eq_ignore_ascii_case("data:"))
        .map(|_| &url[5..])
}

/// The `charset=` parameter of a `data:` URI's media type, if any.
fn data_url_charset(url: &str) -> Option<String> {
    let meta = strip_data_scheme(url)?.split_once(',')?.0;
    meta.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| v.trim().trim_matches('"').to_string())
    })
}

/// Percent-decoding (`%41` → `A`); a `%` not followed by two hex digits is
/// kept as is.
pub(crate) fn percent_decode(s: &str) -> Vec<u8> {
    fn hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn blocks_internal_targets() {
        // Loopback, private ranges, link-local (incl. cloud metadata),
        // unspecified, CGNAT, and the IPv4-mapped IPv6 forms must all be
        // refused as SSRF targets.
        for s in [
            "127.0.0.1",
            "127.5.5.5",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254", // AWS/GCP metadata
            "0.0.0.0",
            "100.64.0.1", // carrier-grade NAT
            "::1",
            "fe80::1",          // link-local
            "fc00::1",          // unique-local
            "::ffff:127.0.0.1", // IPv4-mapped loopback
            "::ffff:169.254.169.254",
        ] {
            assert!(is_blocked_ip(ip(s)), "{s} should be blocked");
        }
    }

    /// #752 (docling-core#846): every address Python's `is_global` rejects —
    /// multicast, reserved, "this network", benchmarking, the protocol
    /// assignments, NAT64, 6to4, documentation, site-local — is blocked too.
    #[test]
    fn blocks_every_non_global_range() {
        for s in [
            "0.1.2.3",         // 0.0.0.0/8
            "224.0.0.1",       // multicast
            "239.255.255.250", // SSDP multicast
            "240.0.0.1",       // reserved
            "255.255.255.255", // broadcast
            "198.18.0.1",      // benchmarking
            "198.19.255.255",
            "192.0.0.1", // IETF protocol assignments
            "192.0.0.171",
            "192.0.2.1",    // TEST-NET-1
            "198.51.100.7", // TEST-NET-2
            "203.0.113.9",  // TEST-NET-3
            "ff02::1",      // multicast
            "ff0e::1",
            "64:ff9b::7f00:1", // NAT64 of 127.0.0.1
            "64:ff9b:1::1",    // local-use NAT64
            "2002:7f00:1::1",  // 6to4 of 127.0.0.1
            "2001:db8::1",     // documentation
            "2001::1",         // Teredo / protocol assignments
            "fec0::1",         // deprecated site-local
            "::7f00:1",        // IPv4-compatible
            "100::1",          // discard-only
            "::ffff:224.0.0.1",
        ] {
            assert!(is_blocked_ip(ip(s)), "{s} should be blocked");
        }
        // Global anycast inside the non-global blocks stays reachable.
        for s in ["192.0.0.9", "192.0.0.10", "2001:4:112::1", "2001:20::1"] {
            assert!(!is_blocked_ip(ip(s)), "{s} should be allowed");
        }
    }

    /// A resolver stub answering with fixed addresses.
    #[cfg(any(feature = "fetch-url", feature = "fetch-images"))]
    #[derive(Debug)]
    struct Fixed(Vec<std::net::SocketAddr>);

    #[cfg(any(feature = "fetch-url", feature = "fetch-images"))]
    impl ureq::unversioned::resolver::Resolver for Fixed {
        fn resolve(
            &self,
            _uri: &ureq::http::Uri,
            _config: &ureq::config::Config,
            _timeout: ureq::unversioned::transport::NextTimeout,
        ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
            let mut out = self.empty();
            for a in &self.0 {
                out.push(*a);
            }
            Ok(out)
        }
    }

    /// The connect-time guard (#752): one resolution, refused when any of
    /// its addresses is blocked — a public answer first and a private one
    /// second is still refused — and otherwise handed to the connector as it
    /// came, so no second lookup can rebind the host. The opt-out lifts it.
    #[cfg(any(feature = "fetch-url", feature = "fetch-images"))]
    #[test]
    fn guarded_resolver_validates_the_addresses_it_connects_to() {
        use ureq::unversioned::resolver::Resolver;
        let guard = |addrs: &[&str], allow_private: bool| GuardedResolver {
            inner: Fixed(addrs.iter().map(|a| a.parse().unwrap()).collect()),
            allow_private,
        };
        let uri: ureq::http::Uri = "http://rebind.example/doc.pdf".parse().unwrap();
        let config = ureq::config::Config::default();
        let resolve = |g: &GuardedResolver<Fixed>| {
            // `..Default::default()` (ureq 3.5+): the struct sits in ureq's
            // semver-exempt `unversioned` module and grows fields in minor
            // releases (3.5: `per_read` / `per_write`).
            let timeout = ureq::unversioned::transport::NextTimeout {
                after: ureq::unversioned::transport::time::Duration::NotHappening,
                reason: ureq::Timeout::Resolve,
                ..Default::default()
            };
            g.resolve(&uri, &config, timeout)
                .map(|a| a.iter().map(|a| a.ip().to_string()).collect::<Vec<_>>())
        };
        let err = resolve(&guard(&["93.184.216.34:80", "127.0.0.1:80"], false)).unwrap_err();
        assert!(err.to_string().contains("127.0.0.1"), "{err}");
        assert!(resolve(&guard(&["169.254.169.254:80"], false)).is_err());
        assert_eq!(
            resolve(&guard(&["93.184.216.34:80", "8.8.8.8:80"], false)).unwrap(),
            ["93.184.216.34", "8.8.8.8"]
        );
        assert_eq!(
            resolve(&guard(&["127.0.0.1:80"], true)).unwrap(),
            ["127.0.0.1"]
        );
    }

    /// A guarded agent refuses an internal target at connect time, with no
    /// pre-flight check in front of it.
    #[cfg(any(feature = "fetch-url", feature = "fetch-images"))]
    #[test]
    fn guarded_agent_refuses_internal_targets() {
        let agent = ureq::Agent::with_parts(
            ureq::Agent::config_builder().build(),
            ureq::unversioned::transport::DefaultConnector::new(),
            GuardedResolver {
                inner: ureq::unversioned::resolver::DefaultResolver::default(),
                allow_private: false,
            },
        );
        let err = agent.get("http://127.0.0.1:9/").call().unwrap_err();
        assert!(err.to_string().contains("non-public address"), "{err}");
    }

    #[test]
    fn allows_public_targets() {
        for s in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "2606:4700:4700::1111",
        ] {
            assert!(!is_blocked_ip(ip(s)), "{s} should be allowed");
        }
    }

    #[test]
    fn disposition_filename_prefers_the_extended_form() {
        assert_eq!(
            disposition_filename(
                "attachment; filename=\"fallback.pdf\"; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf"
            )
            .as_deref(),
            Some("résumé.pdf")
        );
        assert_eq!(
            disposition_filename("inline; filename=\"dir/report.docx\"").as_deref(),
            Some("report.docx")
        );
        assert_eq!(disposition_filename("attachment"), None);
        assert_eq!(disposition_filename("attachment; filename=\"\""), None);
    }

    #[cfg(feature = "fetch-url")]
    #[test]
    fn fetched_name_falls_back_to_the_url_path() {
        assert_eq!(
            fetched_name("https://h.example/a/b/Report%20Q1.pdf?x=1#top", None),
            "Report Q1.pdf"
        );
        assert_eq!(fetched_name("https://h.example/", None), "document");
        assert_eq!(fetched_name("https://h.example", None), "document");
        assert_eq!(
            fetched_name("https://h.example/x", Some("attachment; filename=y.md")),
            "y.md"
        );
    }

    #[cfg(feature = "fetch-url")]
    #[test]
    fn file_url_resolves_to_a_local_path() {
        // A drive-less `file:///tmp/…` names no Windows path (the url crate
        // refuses it there), and a host is a UNC share on Windows only — so
        // each host checks its own spellings.
        #[cfg(not(windows))]
        {
            assert_eq!(
                file_url_path("file:///tmp/a%20b.pdf").unwrap(),
                std::path::PathBuf::from("/tmp/a b.pdf")
            );
            assert!(file_url_path("file://host.example/x.pdf").is_err());
        }
        #[cfg(windows)]
        {
            assert_eq!(
                file_url_path("file:///C:/tmp/a%20b.pdf").unwrap(),
                std::path::PathBuf::from(r"C:\tmp\a b.pdf")
            );
            assert_eq!(
                file_url_path("file://host.example/x.pdf").unwrap(),
                std::path::PathBuf::from(r"\\host.example\x.pdf")
            );
        }
        assert!(file_url_path("https://h.example/x.pdf").is_err());
    }

    #[test]
    fn data_url_decodes_base64_and_percent_forms() {
        let doc = from_data_url("data:text/html;base64,PGgxPkhpPC9oMT4=").unwrap();
        assert_eq!(doc.format, InputFormat::Html);
        assert_eq!(doc.bytes, b"<h1>Hi</h1>");
        assert_eq!(doc.name, "document");
        let doc = from_data_url("data:text/markdown;name=notes.md,%23%20Title").unwrap();
        assert_eq!(doc.format, InputFormat::Md);
        assert_eq!(doc.bytes, b"# Title");
        assert_eq!(doc.name, "notes");
        // No media type: the bytes decide.
        let doc = from_data_url("data:;base64,JVBERi0xLjQK").unwrap();
        assert_eq!(doc.format, InputFormat::Pdf);
        assert!(from_data_url("data:text/plain").is_err());
        assert!(from_data_url("data:;base64,@@@").is_err());
        // A generic media type says nothing; text bytes are plain text
        // (#671), binary ones an error.
        assert_eq!(
            from_data_url("data:application/octet-stream,zzz")
                .unwrap()
                .format,
            InputFormat::Text
        );
        assert!(from_data_url("data:application/octet-stream;base64,AAECAwQF").is_err());
        // A non-UTF-8 charset is the text encoding; base64 may wrap.
        let doc = from_data_url("data:text/csv;charset=shift_jis,a,b").unwrap();
        assert_eq!(doc.encoding.as_deref(), Some("shift_jis"));
        let doc = from_data_url("data:text/csv;charset=\"UTF-8\",a,b").unwrap();
        assert_eq!(doc.encoding, None);
        let doc = from_data_url("data:text/markdown;base64,IyBUaXRs\nZQ==").unwrap();
        assert_eq!(doc.bytes, b"# Title");
        // The scheme is case-insensitive.
        let doc = from_data_url("DATA:text/html,%3Cp%3Ex%3C/p%3E").unwrap();
        assert_eq!(doc.format, InputFormat::Html);
    }
}
