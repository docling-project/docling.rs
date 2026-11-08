//! HTML backend.
//!
//! Parses HTML with `scraper` (html5ever — the same HTML5 tree-construction
//! algorithm browsers use) and walks the DOM into a [`DoclingDocument`]. This
//! is the Rust counterpart of `docling/backend/html_backend.py`'s `_walk`,
//! producing the flat node stream that Markdown / DocLang / LaTeX render
//! byte-for-byte. The JSON export takes a second, structure-faithful walk:
//! [`super::html_tree`] ports upstream's item-tree construction (heading
//! nesting, inline groups, rich cells, content layers, item numbering) into
//! [`DoclingDocument::tree`].
//!
//! Scope (Phase 2): block structure (headings, paragraphs, nested lists,
//! tables, code blocks, figures/images), inline formatting (bold, italic,
//! inline code, links), key-value form regions (docling's `field_region`,
//! detected from the `keyN` / `keyN_valueM` / `keyN_marker` `id`-convention),
//! and invisible-element suppression (`hidden` / `aria-hidden` / inline
//! `display:none` / `visibility:hidden`). Out of scope for now and tracked in `docs/MIGRATION.md`:
//! browser rendering, rendered bounding boxes, stylesheet-driven (class/CSS
//! cascade) visibility suppression, and the rich per-cell table provenance the
//! Python backend computes.

use docling_core::{CaptionParent, ContentLayer, DoclingDocument, InlineRun, Node, Script, Table};
use scraper::{ElementRef, Html, Node as HtmlNode, Selector};

use crate::backend::images::{ImageResolver, NoFetch};
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

/// Compile a CSS selector once per call site (mirrors `cached_regex!`), returning
/// a `&'static Selector`. `Selector::parse` is comparatively expensive, so this
/// matters for selectors evaluated per element — e.g. `has_descendant` runs per
/// table cell.
macro_rules! cached_selector {
    ($sel:literal) => {{
        static SEL: std::sync::OnceLock<Selector> = std::sync::OnceLock::new();
        SEL.get_or_init(|| Selector::parse($sel).unwrap())
    }};
}

pub struct HtmlBackend;

impl DeclarativeBackend for HtmlBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        // The bare backend never fetches images (it's also how the Markdown
        // backend feeds in embedded raw HTML). Image fetching is wired through
        // the converter, which calls `convert_html` with a real resolver.
        let html = decode_html_bytes(&source.bytes);
        Ok(convert_html(&source.name, &html, &NoFetch))
    }
}

/// Convert an HTML document into a [`DoclingDocument`], resolving `<img>` sources
/// through `images` (use [`NoFetch`] to leave every picture a placeholder).
/// Decode raw HTML bytes the way docling reads them — BeautifulSoup's
/// `UnicodeDammit` (#371): a byte-order mark wins; else the encoding the
/// document declares (an XML declaration within the first 1024 bytes, else a
/// `<meta charset>` / `http-equiv` charset within the first
/// `max(2048, 5 % of the length)` bytes — bs4's search windows and regexes);
/// else strict UTF-8; else windows-1252, which never fails. Each candidate is
/// taken only when it decodes without error, as upstream does. The one step
/// not reproduced is bs4's third-party detector (chardet / charset_normalizer),
/// consulted between the declaration and the fallbacks: it is heuristic and
/// version-dependent, and a UTF-8 or windows-1252 document — the realistic
/// legacy inputs — never reaches it. Labels resolve through the WHATWG table
/// (`encoding_rs`), so `iso-8859-1`/`latin1` decode as windows-1252 the way
/// browsers do, where Python's codec would map 0x80–0x9F to C1 controls.
pub(crate) fn decode_html_bytes(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    use encoding_rs::{Encoding, WINDOWS_1252};
    if let Some((enc, bom_len)) = Encoding::for_bom(bytes) {
        if let Some(text) =
            enc.decode_without_bom_handling_and_without_replacement(&bytes[bom_len..])
        {
            return text;
        }
    }
    if let Some(label) = declared_encoding(bytes) {
        if let Some(enc) = Encoding::for_label(label.as_bytes()) {
            if let Some(text) = enc.decode_without_bom_handling_and_without_replacement(bytes) {
                return text;
            }
        }
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return std::borrow::Cow::Borrowed(text);
    }
    WINDOWS_1252.decode_without_bom_handling(bytes).0
}

/// The encoding an HTML document declares, lowercased — bs4's
/// `EncodingDetector.find_declared_encoding(is_html=True)`: the XML
/// declaration's `encoding=` (searched in the first 1024 bytes), else the
/// first `<meta … charset=…>` (searched in the first `max(2048, len/20)`
/// bytes; the regex also covers `http-equiv` content-type declarations).
fn declared_encoding(bytes: &[u8]) -> Option<String> {
    use regex::bytes::Regex;
    use std::sync::OnceLock;
    static XML_RE: OnceLock<Regex> = OnceLock::new();
    static META_RE: OnceLock<Regex> = OnceLock::new();
    let xml_re = XML_RE.get_or_init(|| {
        Regex::new(r#"(?i)^\s*<\?.*encoding=['"](.*?)['"].*\?>"#).expect("xml decl regex")
    });
    let meta_re = META_RE.get_or_init(|| {
        Regex::new(r#"(?i)<\s*meta[^>]+charset\s*=\s*["']?([^>]*?)[ /;'">]"#).expect("meta regex")
    });
    let xml_end = bytes.len().min(1024);
    let html_end = bytes.len().min(2048.max(bytes.len() / 20));
    let found = xml_re
        .captures(&bytes[..xml_end])
        .or_else(|| meta_re.captures(&bytes[..html_end]))?;
    let label = found.get(1)?.as_bytes();
    let label = String::from_utf8_lossy(label).trim().to_ascii_lowercase();
    (!label.is_empty()).then_some(label)
}

pub(crate) fn convert_html(name: &str, html: &str, images: &dyn ImageResolver) -> DoclingDocument {
    convert_html_with(name, html, images, true)
}

/// [`convert_html`] with docling's `infer_furniture` option: `false` keeps
/// everything on the body layer, however late the first heading comes — how
/// the XBRL backend converts a filing's HTML text blocks.
pub(crate) fn convert_html_with(
    name: &str,
    html: &str,
    images: &dyn ImageResolver,
    infer_furniture: bool,
) -> DoclingDocument {
    let mut doc = DoclingDocument::new(name);
    let flattened = append_fragment_with(html, &mut doc.nodes, images, infer_furniture);
    // The flat nodes above drive Markdown / DocLang / LaTeX; the JSON export
    // takes docling's item tree, built by a call-for-call port of upstream's
    // walk ([`super::html_tree`]) so the JSON structure — heading nesting,
    // inline groups, rich cells, content layers, item numbering — is
    // upstream's. A page past the depth cap has no tree (its DOM is never
    // walked); the notice says so (#912).
    match flattened {
        Some(notice) => doc.notices.push(notice),
        None => {
            let parsed = Html::parse_document(html);
            doc.tree = Some(super::html_tree::build_tree(
                &parsed,
                images,
                infer_furniture,
                true,
            ));
        }
    }
    doc
}

/// The depth-cap notice (#912): the page nests past
/// `DOCLING_RS_MAX_HTML_DEPTH`, so it is read as flat text.
fn depth_notice() -> docling_core::Notice {
    docling_core::Notice::depth_limit("HTML", "DOCLING_RS_MAX_HTML_DEPTH", max_dom_depth())
}

/// Parse an HTML fragment and append its block nodes to `out`. Shared with the
/// Markdown backend, which feeds embedded raw-HTML blocks through here (as
/// docling does).
pub(crate) fn append_fragment(html: &str, out: &mut Vec<Node>, images: &dyn ImageResolver) {
    append_fragment_with(html, out, images, true);
}

/// [`append_fragment`] with docling's `infer_furniture` option (see
/// [`convert_html_with`]). `Some` when the page was flattened — nested past
/// the depth cap, read as text without walking a DOM (#912).
fn append_fragment_with(
    html: &str,
    out: &mut Vec<Node>,
    images: &dyn ImageResolver,
    infer_furniture: bool,
) -> Option<docling_core::Notice> {
    // html5ever's tree builder walks its stack of open elements per tag, so
    // a page nested tens of thousands of elements deep parses in quadratic
    // time — 100 000 nested `<div>`s (1 MB) took 37 s before a single node
    // was walked, well past the point where `within_depth_limit` would have
    // flattened it anyway. Estimate the nesting first with a linear tag scan
    // and, past the limit, emit the page's text without building a DOM.
    if !nesting_estimate_within(html, max_dom_depth()) {
        let text = normalize_ws(&strip_tags(html));
        if !text.is_empty() {
            out.push(Node::Paragraph { text });
        }
        return Some(depth_notice());
    }
    let parsed = Html::parse_document(html);
    // The document `<title>` is docling's furniture-layer title heading — it
    // precedes the body content and is excluded from Markdown (the JSON gets
    // it from the item tree, `html_tree`). Fragments (e.g. Markdown-embedded
    // HTML) carry no `<title>`, so none is added.
    if let Some(title) = parsed.select(cached_selector!("title")).next() {
        let text = normalize_ws(&title.text().collect::<String>());
        if !text.is_empty() {
            out.push(Node::Furniture {
                layer: docling_core::ContentLayer::Furniture,
                inner: Box::new(Node::Heading { level: 1, text }),
            });
        }
    }

    let start = out.len();
    let mut flattened = None;
    // Prefer <body>; fall back to the root element for fragments.
    let body = parsed.select(cached_selector!("body")).next();
    let root = body.unwrap_or_else(|| parsed.root_element());
    // The block/inline DOM walkers below recurse on nesting depth. A crafted
    // document with tens of thousands of nested elements (trivial to author,
    // also reachable through EPUB and Markdown-embedded HTML) would overflow
    // the stack — an uncatchable abort, i.e. a remote DoS via docling-serve.
    // Guard with a depth ceiling checked iteratively (so the check itself
    // can't overflow); past it, fall back to the flattened text so the
    // document still yields content without descending the pathological tree.
    if within_depth_limit(root, MAX_DOM_DEPTH) {
        // Warm remote-image fetches concurrently before the serial walk: a real
        // web page carries dozens of `<img>`, and fetching them one-at-a-time
        // during the walk dominates wall-clock. Collect every image src up front
        // (the walk resolves the same strings, hitting the now-warm cache).
        let srcs: Vec<String> = root
            .select(cached_selector!("img"))
            .filter_map(|img| img_src(img.value()))
            .collect();
        if !srcs.is_empty() {
            images.prefetch(&srcs);
        }
        walk_block(root, out, 0, Fmt::default(), images);
    } else {
        let text = normalize_ws(&root.text().collect::<String>());
        if !text.is_empty() {
            out.push(Node::Paragraph { text });
        }
        flattened = Some(depth_notice());
    }
    // docling's `infer_furniture`: content before the first body heading is site
    // chrome (navigation/menus/sidebars) → the `furniture` layer.
    if infer_furniture {
        mark_leading_furniture(&mut out[start..]);
    }
    flattened
}

/// The first hyperlink target inside `el` (its own `<a href>` or a descendant),
/// used as a list item's `<href>` head. `None` when the item has no link.
fn first_href(el: ElementRef) -> Option<String> {
    el.select(cached_selector!("a"))
        .next()
        .and_then(|a| a.value().attr("href"))
        .filter(|h| !h.is_empty())
        .map(str::to_string)
}

/// Tag every node before the first body heading as the `furniture` content layer
/// (docling's `infer_furniture`, default on). No body heading → nothing is tagged
/// (docling keeps such a document entirely in the body layer). A list item takes
/// the layer on its `layer` field (so items still group into one `<list>`); any
/// other node is wrapped in [`Node::Furniture`].
fn mark_leading_furniture(nodes: &mut [Node]) {
    let Some(first) = nodes.iter().position(|n| matches!(n, Node::Heading { .. })) else {
        return;
    };
    for n in &mut nodes[..first] {
        match n {
            Node::ListItem { layer, .. } => *layer = Some(ContentLayer::Furniture),
            Node::Furniture { .. } => {}
            other => {
                let inner = std::mem::replace(other, Node::PageBreak);
                *other = Node::Furniture {
                    layer: ContentLayer::Furniture,
                    inner: Box::new(inner),
                };
            }
        }
    }
}

/// An element the page explicitly hides from rendering — the `hidden` attribute,
/// `aria-hidden`, or an inline `display:none` / `visibility:hidden` style. A
/// rendering engine (and docling's rendered output) drops these, so we suppress
/// them too.
///
/// `aria-hidden` is docling's `_is_invisible_tag` rule, not a strictly visual
/// one: the attribute only removes an element from the accessibility tree, but
/// in practice it marks decorative duplicates (Wikipedia's `<img
/// class="mw-logo-icon" aria-hidden="true">` beside the captioned wordmark), and
/// docling drops them. `true`/`1`/`yes` count, as upstream. Only inline styles
/// are honored beyond that — a full CSS cascade (class/stylesheet-driven
/// visibility, e.g. Wikipedia's collapsed menus) still needs a real browser and
/// is out of scope, as is docling's `_has_rendered_presence` zero-size check.
pub(super) fn is_hidden(e: &scraper::node::Element) -> bool {
    if e.attr("hidden").is_some() {
        return true;
    }
    if e.attr("aria-hidden").is_some_and(|v| {
        let v = v.trim();
        v.eq_ignore_ascii_case("true") || v == "1" || v.eq_ignore_ascii_case("yes")
    }) {
        return true;
    }
    e.attr("style").is_some_and(|style| {
        style.split(';').any(|decl| {
            let mut it = decl.splitn(2, ':');
            match (it.next(), it.next()) {
                (Some(prop), Some(val)) => {
                    let (prop, val) = (prop.trim(), val.trim());
                    (prop.eq_ignore_ascii_case("display") && val.eq_ignore_ascii_case("none"))
                        || (prop.eq_ignore_ascii_case("visibility")
                            && val.eq_ignore_ascii_case("hidden"))
                }
                _ => false,
            }
        })
    })
}

/// Tags whose content is not document text and should be skipped wholesale.
pub(super) fn is_skipped(name: &str) -> bool {
    matches!(
        name,
        "script" | "style" | "head" | "title" | "noscript" | "template" | "svg"
    )
}

/// Block-level tags: encountering one flushes any buffered inline text.
fn is_block(name: &str) -> bool {
    matches!(
        name,
        "h1" | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "p"
            | "ul"
            | "ol"
            | "pre"
            | "table"
            | "figure"
            | "blockquote"
            | "div"
            | "section"
            | "article"
            | "main"
            | "header"
            | "footer"
            | "nav"
            | "aside"
            | "details"
            | "hr"
            | "dl"
            | "button"
            | "input"
            | "label"
            | "body"
            | "html"
    )
}

/// Walk the block-level children of `elem`, emitting [`Node`]s. Inline content
/// found directly between block elements is buffered and flushed as paragraphs.
/// `base` seeds the inline formatting — table cells pass `raw` so their text is
/// not `&<>`/`_` escaped.
/// Deepest DOM nesting the recursive walkers will descend. Real documents sit
/// in the low tens; 2000 is far above anything legitimate while keeping the
/// recursion well clear of the stack limit. Override with
/// `DOCLING_RS_MAX_HTML_DEPTH`.
const MAX_DOM_DEPTH: usize = 2000;

fn max_dom_depth() -> usize {
    docling_core::env::parse("DOCLING_RS_MAX_HTML_DEPTH").unwrap_or(MAX_DOM_DEPTH)
}

/// Elements that never take an end tag (HTML void elements) or whose end tag
/// HTML lets a document omit — they must not count towards the nesting
/// estimate, or a page written without `</p>`/`</li>` would look as deep as
/// it is long. Everything else (`div`, `span`, `table`, `ul`, `b`, `a`, …)
/// nests only as far as its explicit end tags allow.
fn html_tag_implicitly_closed(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
            | "p"
            | "li"
            | "dt"
            | "dd"
            | "tr"
            | "td"
            | "th"
            | "option"
            | "optgroup"
            | "thead"
            | "tbody"
            | "tfoot"
            | "colgroup"
            | "caption"
            | "html"
            | "head"
            | "body"
            | "rt"
            | "rp"
    )
}

/// A linear, tokenizer-free estimate of how deep the markup nests: `true`
/// when no run of open tags exceeds `limit`. Start tags of explicitly-closed
/// elements push, their end tags pop; `<script>`/`<style>` bodies, comments
/// and quoted attribute values are skipped whole. An overestimate (a real
/// page with unclosed `<div>`s) only routes the page through the text-only
/// path that `within_depth_limit` already uses for over-deep DOMs.
fn nesting_estimate_within(html: &str, limit: usize) -> bool {
    let b = html.as_bytes();
    let n = b.len();
    let mut depth = 0usize;
    let mut i = 0;
    let find_ci = |from: usize, pat: &[u8]| -> usize {
        let mut j = from;
        while j + pat.len() <= n {
            if b[j..j + pat.len()].eq_ignore_ascii_case(pat) {
                return j + pat.len();
            }
            j += 1;
        }
        n
    };
    while i < n {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        if b[i..].starts_with(b"<!--") {
            i = find_ci(i + 4, b"-->");
            continue;
        }
        if b[i..].starts_with(b"<!") || b[i..].starts_with(b"<?") {
            i = find_ci(i + 2, b">");
            continue;
        }
        let closing = b[i..].starts_with(b"</");
        let name_start = if closing { i + 2 } else { i + 1 };
        let mut j = name_start;
        while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'-' || b[j] == b':') {
            j += 1;
        }
        if j == name_start {
            // A stray `<`, not a tag.
            i += 1;
            continue;
        }
        let name = html[name_start..j].to_ascii_lowercase();
        // To the end of the tag, honouring quoted attribute values.
        let mut quote: Option<u8> = None;
        let mut prev = b' ';
        while j < n {
            let c = b[j];
            match quote {
                Some(q) if c == q => quote = None,
                Some(_) => {}
                None if c == b'"' || c == b'\'' => quote = Some(c),
                None if c == b'>' => break,
                None => {}
            }
            prev = c;
            j += 1;
        }
        i = j + 1;
        if closing {
            if !html_tag_implicitly_closed(&name) {
                depth = depth.saturating_sub(1);
            }
        } else if name == "script" || name == "style" {
            // Raw-text bodies: skip to the matching end tag.
            let end = format!("</{name}");
            i = find_ci(i, end.as_bytes());
            i = find_ci(i, b">");
        } else if prev != b'/' && !html_tag_implicitly_closed(&name) {
            depth += 1;
            if depth > limit {
                return false;
            }
        }
    }
    true
}

/// The text of `html` without its markup — the over-deep fallback's stand-in
/// for `root.text()`: tags, comments and `<script>`/`<style>` bodies dropped,
/// the five named entities and numeric references decoded.
fn strip_tags(html: &str) -> String {
    let b = html.as_bytes();
    let n = b.len();
    let mut out = String::with_capacity(n / 2);
    let mut i = 0;
    let find_ci = |from: usize, pat: &[u8]| -> usize {
        let mut j = from;
        while j + pat.len() <= n {
            if b[j..j + pat.len()].eq_ignore_ascii_case(pat) {
                return j + pat.len();
            }
            j += 1;
        }
        n
    };
    while i < n {
        if b[i] == b'<' {
            if b[i..].starts_with(b"<!--") {
                i = find_ci(i + 4, b"-->");
            } else if b[i + 1..].len() >= 6 && b[i + 1..i + 7].eq_ignore_ascii_case(b"script") {
                i = find_ci(i, b"</script");
                i = find_ci(i, b">");
            } else if b[i + 1..].len() >= 5 && b[i + 1..i + 6].eq_ignore_ascii_case(b"style") {
                i = find_ci(i, b"</style");
                i = find_ci(i, b">");
            } else {
                i = find_ci(i + 1, b">");
            }
            out.push(' ');
            continue;
        }
        if b[i] == b'&' {
            if let Some(end) = html[i..].find(';').filter(|&e| e <= 10) {
                let ent = &html[i + 1..i + end];
                let decoded = match ent {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some('\u{a0}'),
                    _ => ent
                        .strip_prefix('#')
                        .and_then(|num| {
                            num.strip_prefix(['x', 'X']).map_or_else(
                                || num.parse::<u32>().ok(),
                                |h| u32::from_str_radix(h, 16).ok(),
                            )
                        })
                        .and_then(char::from_u32),
                };
                if let Some(c) = decoded {
                    out.push(c);
                    i += end + 1;
                    continue;
                }
            }
        }
        // Copy one UTF-8 scalar.
        let ch = html[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Whether no node under `root` nests deeper than the limit. Iterative DFS —
/// this check must not itself recurse (it runs on the same hostile tree).
fn within_depth_limit(root: ElementRef, _default: usize) -> bool {
    let limit = max_dom_depth();
    let mut stack = vec![(*root, 1usize)];
    while let Some((node, depth)) = stack.pop() {
        if depth > limit {
            return false;
        }
        for child in node.children() {
            stack.push((child, depth + 1));
        }
    }
    true
}

fn walk_block(
    elem: ElementRef,
    nodes: &mut Vec<Node>,
    list_level: u8,
    base: Fmt,
    images: &dyn ImageResolver,
) {
    walk_block_nodes(elem.children(), nodes, list_level, base, images);
}

/// [`walk_block`] over some of an element's children only — upstream's
/// `_walk_nodes`.
fn walk_block_nodes<'a>(
    children: impl IntoIterator<Item = ego_tree::NodeRef<'a, HtmlNode>>,
    nodes: &mut Vec<Node>,
    list_level: u8,
    base: Fmt,
    images: &dyn ImageResolver,
) {
    let mut inline = RunBuf::default();

    for child in children {
        match child.value() {
            HtmlNode::Text(text) => {
                inline.text_node(text, base);
            }
            HtmlNode::Element(e) => {
                let Some(cref) = ElementRef::wrap(child) else {
                    continue;
                };
                let name = e.name();
                if is_skipped(name) || is_hidden(e) {
                    continue;
                }
                if name == "img" {
                    // A block-level image becomes a figure/picture, matching the
                    // Python backend (inline images inside text stay inline).
                    flush_inline(&mut inline, nodes);
                    nodes.push(Node::Picture {
                        caption: e.attr("alt").filter(|a| !a.is_empty()).map(str::to_string),
                        caption_href: None,
                        image: img_src(e).and_then(|s| images.resolve(&s)),
                        classification: None,
                        description: None,
                        caption_parent: Default::default(),
                        caption_location: None,
                    });
                } else if name == "signature" || name == "stamp" {
                    // docling turns these into an image annotated with the kind.
                    flush_inline(&mut inline, nodes);
                    nodes.push(Node::Picture {
                        caption: None,
                        caption_href: None,
                        image: None,
                        classification: None,
                        description: None,
                        caption_parent: Default::default(),
                        caption_location: None,
                    });
                    let mut label = name.to_string();
                    label[..1].make_ascii_uppercase();
                    nodes.push(Node::Paragraph { text: label });
                } else if is_block(name) {
                    flush_inline(&mut inline, nodes);
                    handle_block(cref, name, nodes, list_level, base, images);
                } else if name == "a" {
                    if let Some((caption, src)) = image_wrapper(cref) {
                        // An anchor wrapping only an image (`<a><img></a>`):
                        // docling pulls the image out as a Picture and drops the
                        // wrapper — but the anchor's href survives as the
                        // caption's hyperlink annotation (docling hangs it on
                        // the caption text item; no caption, nowhere to hang).
                        flush_inline(&mut inline, nodes);
                        let caption_href = caption
                            .is_some()
                            .then(|| e.attr("href"))
                            .flatten()
                            .filter(|h| !h.is_empty())
                            .map(normalize_url);
                        nodes.push(Node::Picture {
                            caption,
                            caption_href,
                            image: src.as_deref().and_then(|s| images.resolve(s)),
                            classification: None,
                            description: None,
                            caption_parent: Default::default(),
                            caption_location: None,
                        });
                    } else if has_descendant(cref, "img") || contains_block(cref) {
                        // An anchor with an image among other content (docling
                        // treats `img` as a block tag), or one hiding real block
                        // structure (#284 — an *unclosed* `<a name=…>` legally
                        // swallows every following block under HTML5 parsing):
                        // the wrapper is walked block-wise, so nested tables and
                        // lists come out as their own items instead of
                        // flattening into inline text. The anchor's href still
                        // reaches the pictures it wraps (Wikipedia's logo link
                        // holds a wordmark and a tagline image), exactly as in
                        // the single-image branch above.
                        flush_inline(&mut inline, nodes);
                        let start = nodes.len();
                        walk_block(cref, nodes, list_level, base, images);
                        if let Some(href) =
                            e.attr("href").filter(|h| !h.is_empty()).map(normalize_url)
                        {
                            annotate_picture_captions(&mut nodes[start..], &href);
                        }
                    } else {
                        collect_element(cref, base, None, &mut inline);
                    }
                } else if has_descendant(cref, "img") || contains_block(cref) {
                    // An inline wrapper around an image (`<span><a><img></a></span>`
                    // — docling's `img` is in its block-tag set), or an inline
                    // element hiding block structure (#284: html5ever follows the
                    // HTML5 tree-construction spec, so an unclosed `<b>`/`<font>`
                    // keeps every subsequent block as its child — Python
                    // docling's parser recovers by reparenting, and parity means
                    // walking such a wrapper block-wise). The inline formatting
                    // element's own formatting (`<b>` → bold) still reaches its
                    // *inline* children through the seeded base — the nested
                    // blocks themselves render as the reparenting recovery
                    // would leave them.
                    flush_inline(&mut inline, nodes);
                    walk_block(cref, nodes, list_level, tag_fmt(name, base), images);
                } else {
                    collect_element(cref, base, None, &mut inline);
                }
            }
            _ => {}
        }
    }

    flush_inline(&mut inline, nodes);
}

fn flush_inline(buf: &mut RunBuf, nodes: &mut Vec<Node>) {
    if !buf.md.is_empty() {
        push_inline_paragraph(nodes, finalize(buf), std::mem::take(&mut buf.rich));
    }
    *buf = RunBuf::default();
}

/// Push a paragraph of inline content as docling's `InlineGroup`. The Markdown
/// text drives Markdown/JSON output unchanged; the structured `runs` (captured
/// during the DOM walk, carrying underline/sub/superscript that Markdown cannot)
/// drive DocLang. The group serializes unwrapped (no `<text>`) once any body
/// heading has been emitted — mirroring docling, where such content is nested
/// under the heading rather than the body group.
fn push_inline_paragraph(nodes: &mut Vec<Node>, text: String, runs: Vec<InlineRun>) {
    if text.is_empty() {
        return;
    }
    // In docling's HTML backend loose content is nested under the current
    // heading, so its `InlineGroup` serializes unwrapped once a body heading has
    // been emitted; before the first heading it sits in the body group (wrapped).
    let unwrapped = nodes.iter().any(|n| matches!(n, Node::Heading { .. }));
    nodes.push(docling_core::inline_paragraph_node(text, runs, unwrapped));
}

fn handle_block(
    elem: ElementRef,
    name: &str,
    nodes: &mut Vec<Node>,
    list_level: u8,
    base: Fmt,
    images: &dyn ImageResolver,
) {
    match name {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level: u8 = name[1..].parse().unwrap_or(1);
            let text = render_heading(elem, base);
            if !text.is_empty() {
                nodes.push(Node::Heading { level, text });
            }
        }
        "p" => {
            // A paragraph whose only content is inline code becomes a code block.
            if let Some(code) = lone_code(elem) {
                nodes.push(Node::Code {
                    language: None,
                    text: code,
                    orig: None,
                    pretty: None,
                });
            } else {
                let (text, runs) = render_inline(elem, base);
                push_inline_paragraph(nodes, text, runs);
            }
        }
        "ul" | "ol" => walk_list(elem, name == "ol", nodes, list_level, base, images),
        "dl" => walk_dl(elem, nodes, list_level, base, images),
        "pre" => {
            // A <pre> with inline structure (links/formatting) renders each
            // segment as inline code; a plain <pre> is a code block.
            let mut runs = RunBuf::default();
            collect_runs(elem, Fmt { code: true, ..base }, None, &mut runs);
            if runs.md.len() > 1 {
                // Keep the structured runs, not just their Markdown join: each
                // segment is a code run (docling emits `<code>See</code>
                // <code>the docs</code>…`), and re-parsing the joined text
                // would lose the code flag on a linked segment and split the
                // separators into their own plain runs.
                push_inline_paragraph(
                    nodes,
                    pre_runs_md(elem, base),
                    std::mem::take(&mut runs.rich),
                );
            } else {
                let (language, text) = extract_pre(elem);
                nodes.push(Node::Code {
                    language,
                    text,
                    orig: None,
                    pretty: None,
                });
            }
        }
        "table" => {
            if base.raw {
                // A table nested in a cell is flattened to its grid cells joined
                // with spaces (docling's `_collect_subtree_text`). Each grid
                // cell's text is docling's raw `get_text` (source line breaks
                // preserved as newlines), so a deeper nested table's structure
                // survives as `\n` runs that the table serializer later flattens
                // to spaces — reproducing docling's spacing byte-for-byte. It is
                // a *table* part, not a text item: docling-core 2.92's GFM
                // hard-line-break rule (`\n` → `"  \n"`, docling-core#721)
                // applies to text items only, so the flattened grid goes in
                // verbatim rather than as a paragraph.
                let text = flatten_nested_table(elem);
                if !text.is_empty() {
                    nodes.push(Node::TextDump(text));
                }
            } else if let Some(mut table) = parse_table(elem) {
                // The table's own `<caption>` — looked up non-recursively, so a
                // nested table does not steal its ancestor's — is a caption
                // item created ahead of the table under the same parent and
                // linked from the table's `captions` (docling#4289).
                if let Some(text) = table_caption_text(elem) {
                    table.caption = Some(text);
                    table.caption_parent = CaptionParent::Container;
                }
                nodes.push(Node::Table(table));
            }
        }
        "figure" => {
            // docling#4050: every child except the `<figcaption>` is dispatched
            // (an `<img>` becomes a picture, block tags go through the block
            // handler, inline content accumulates as text), and the caption is
            // resolved afterwards — every picture the figure produced takes the
            // figcaption as its caption (docling's `_emit_image` looks up the
            // enclosing figure's figcaption); when no picture came out, the
            // figcaption becomes a caption item of its own, attached to the
            // figure's first item when that is a table (`TableItem.captions`).
            let start = nodes.len();
            let cap_text = figcaption_text(elem);
            let cap_href = figcaption_href(elem);
            let mut inline = RunBuf::default();
            for child in elem.children() {
                match child.value() {
                    HtmlNode::Text(text) => {
                        inline.text_node(text, base);
                    }
                    HtmlNode::Element(e) => {
                        let Some(cref) = ElementRef::wrap(child) else {
                            continue;
                        };
                        let name = e.name();
                        if name == "figcaption" || is_skipped(name) || is_hidden(e) {
                            continue;
                        }
                        if name == "img" {
                            flush_inline(&mut inline, nodes);
                            nodes.push(Node::Picture {
                                caption: e
                                    .attr("alt")
                                    .filter(|a| !a.is_empty())
                                    .map(str::to_string),
                                caption_href: None,
                                image: img_src(e).and_then(|s| images.resolve(&s)),
                                classification: None,
                                description: None,
                                caption_parent: Default::default(),
                                caption_location: None,
                            });
                        } else if is_block(name) {
                            flush_inline(&mut inline, nodes);
                            handle_block(cref, name, nodes, list_level, base, images);
                        } else if has_descendant(cref, "img") || contains_block(cref) {
                            flush_inline(&mut inline, nodes);
                            walk_block(cref, nodes, list_level, tag_fmt(name, base), images);
                        } else {
                            collect_element(cref, base, None, &mut inline);
                        }
                    }
                    _ => {}
                }
            }
            flush_inline(&mut inline, nodes);
            let produced = &mut nodes[start..];
            let mut any_picture = false;
            for node in produced.iter_mut() {
                if let Node::Picture {
                    caption,
                    caption_href,
                    ..
                } = node
                {
                    any_picture = true;
                    if cap_text.is_some() {
                        *caption = cap_text.clone();
                        *caption_href = cap_href.clone();
                    }
                }
            }
            if !any_picture {
                if let Some(text) = cap_text {
                    match produced.first_mut() {
                        // docling adds the table, then the figcaption under
                        // the same parent (#390: the caption follows the
                        // table in the container's children). A table that
                        // already carries its own `<caption>` keeps it.
                        Some(Node::Table(table)) if table.caption.is_none() => {
                            table.caption = Some(text);
                            table.caption_parent = CaptionParent::ContainerAfter;
                        }
                        _ => nodes.push(Node::Caption {
                            text,
                            href: cap_href,
                        }),
                    }
                }
            }
        }
        "hr" => {}
        // An `<input type="checkbox|radio">` is a checkbox item; its text comes
        // from the `<label for=…>` (or wrapping label), falling back to the
        // `aria-label` — docling's `_emit_input`. Other inputs emit their
        // value/placeholder/name as a text item; hidden inputs nothing.
        "input" => {
            let ty = elem.value().attr("type").unwrap_or("").to_ascii_lowercase();
            if ty == "hidden" {
                return;
            }
            if ty == "checkbox" || ty == "radio" {
                let text = checkbox_label_text(elem);
                if !text.is_empty() {
                    nodes.push(Node::CheckboxItem {
                        checked: elem.value().attr("checked").is_some(),
                        text,
                    });
                }
            } else {
                let text = ["value", "placeholder", "name"]
                    .iter()
                    .find_map(|a| {
                        elem.value()
                            .attr(a)
                            .map(str::trim)
                            .filter(|t| !t.is_empty())
                    })
                    .unwrap_or("");
                if !text.is_empty() {
                    nodes.push(Node::Paragraph {
                        text: super::markdown::escape_html(&super::markdown::escape_underscores(
                            &normalize_ws(text),
                        )),
                    });
                }
            }
        }
        // A `<label>` bound to a checkbox input was consumed as that checkbox's
        // text; any other label renders as ordinary inline content.
        "label" => {
            if !label_feeds_checkbox(elem) {
                let (text, runs) = render_inline(elem, base);
                push_inline_paragraph(nodes, text, runs);
            }
        }
        // A `form_region`-classed container holding `keyN`-convention fields is a
        // docling key-value region; emit it as one instead of recursing (so the
        // docling's `_use_footer`: everything inside a `<footer>` lands in the
        // furniture content layer (excluded from Markdown), whatever its
        // position in the document.
        "footer" => {
            let mut inner: Vec<Node> = Vec::new();
            walk_block(elem, &mut inner, list_level, base, images);
            for mut n in inner {
                match &mut n {
                    Node::ListItem { layer, .. } => *layer = Some(ContentLayer::Furniture),
                    Node::Furniture { .. } => {}
                    _ => {
                        let inner_node = std::mem::replace(&mut n, Node::PageBreak);
                        n = Node::Furniture {
                            layer: ContentLayer::Furniture,
                            inner: Box::new(inner_node),
                        };
                    }
                }
                nodes.push(n);
            }
        }
        // field divs aren't also flattened into paragraphs).
        _ if !base.raw => match detect_field_region(elem) {
            Some(items) => nodes.push(Node::FieldRegion { items }),
            None => walk_block(elem, nodes, list_level, base, images),
        },
        // Transparent containers (div, section, blockquote, …): recurse.
        _ => walk_block(elem, nodes, list_level, base, images),
    }
}

/// Detect docling's HTML key-value region: an element classed `form_region`
/// whose descendants carry the `keyN` / `keyN_valueM` / `keyN_marker` `id`
/// convention. Returns the fields ordered by their numeric key, or `None` when
/// this element is not such a region (so the caller recurses normally).
pub(super) fn detect_field_region(elem: ElementRef) -> Option<Vec<docling_core::FieldItem>> {
    let is_form_region = elem
        .value()
        .attr("class")
        .is_some_and(|c| c.split_whitespace().any(|cls| cls == "form_region"));
    if !is_form_region {
        return None;
    }
    // Collect each numbered field's parts by scanning `id`-bearing descendants.
    // A BTreeMap keeps the fields ordered by their numeric key.
    let mut fields: std::collections::BTreeMap<u32, docling_core::FieldItem> =
        std::collections::BTreeMap::new();
    for el in elem.select(cached_selector!("[id]")) {
        let Some(id) = el.value().attr("id") else {
            continue;
        };
        let Some((n, kind)) = parse_kvp_id(id) else {
            continue;
        };
        let text = normalize_ws(&el.text().collect::<String>());
        if text.is_empty() {
            continue;
        }
        let field = fields.entry(n).or_default();
        match kind {
            KvpKind::Marker => field.marker.get_or_insert(text),
            KvpKind::Key => field.key.get_or_insert(text),
            KvpKind::Value => {
                if field.value.is_none() {
                    field.value_kind = Some(form_value_kind(el).to_string());
                }
                field.value.get_or_insert(text)
            }
        };
    }
    if fields.is_empty() {
        return None;
    }
    Some(fields.into_values().collect())
}

/// docling's `_infer_form_value_kind`: a value element is `fillable` when it
/// is or holds a form control (a checkbox-like tag, `input`/`select`/
/// `textarea`, or an input-styled class), `read_only` otherwise — the
/// `field_value` item's `kind` in the JSON.
fn form_value_kind(el: ElementRef) -> &'static str {
    let is_checkbox_like = |e: &scraper::node::Element| {
        (e.name() == "input"
            && matches!(
                e.attr("type")
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
                    .as_str(),
                "checkbox" | "radio"
            ))
            || e.attr("class").is_some_and(|c| {
                c.split_whitespace()
                    .any(|k| matches!(k, "checkbox" | "checkbox-box" | "checkbox-input"))
            })
    };
    let e = el.value();
    if is_checkbox_like(e)
        || el
            .descendants()
            .skip(1)
            .any(|d| d.value().as_element().is_some_and(is_checkbox_like))
    {
        return "fillable";
    }
    let fillable_class = |k: &str| {
        matches!(
            k,
            "input"
                | "input-box"
                | "input-field"
                | "input_field"
                | "text-input"
                | "text-box"
                | "textbox"
                | "form-control"
        ) || k.ends_with("-input")
            || k.ends_with("_input")
            || k.ends_with("-input-box")
    };
    if e.attr("class")
        .is_some_and(|c| c.split_whitespace().any(fillable_class))
    {
        return "fillable";
    }
    let is_control = |name: &str| matches!(name, "input" | "select" | "textarea");
    if is_control(e.name())
        || el
            .descendants()
            .skip(1)
            .any(|d| d.value().as_element().is_some_and(|d| is_control(d.name())))
    {
        return "fillable";
    }
    "read_only"
}

/// Which part of a key-value field an element's `id` names.
enum KvpKind {
    Marker,
    Key,
    Value,
}

/// Parse docling's key-value `id` convention: `keyN` (the key), `keyN_markerN`
/// / `keyN_marker` (its marker), `keyN_valueM` (a value). Returns the field
/// number and which part it is, or `None` for any other `id`.
fn parse_kvp_id(id: &str) -> Option<(u32, KvpKind)> {
    let rest = id.strip_prefix("key")?;
    if let Ok(n) = rest.parse::<u32>() {
        return Some((n, KvpKind::Key));
    }
    let (num, suffix) = rest.split_once('_')?;
    let n = num.parse::<u32>().ok()?;
    if suffix == "marker" {
        Some((n, KvpKind::Marker))
    } else if suffix
        .strip_prefix("value")
        .is_some_and(|m| m.parse::<u32>().is_ok())
    {
        Some((n, KvpKind::Value))
    } else {
        None
    }
}

/// An `<ol start>` by the HTML rules for parsing integers — docling's
/// `_parse_ol_start` (docling#4713, #771): leading ASCII whitespace, an
/// optional `+`, then ASCII digits; what follows them is ignored (`3.5` → 3).
/// No digits (`-5`, `foo`, non-ASCII digits) or a value above 2³¹−1 is no
/// start, as in browsers.
pub(super) fn parse_ol_start(value: &str) -> Option<u64> {
    const MAX: u64 = (1 << 31) - 1;
    let rest = value.trim_start_matches([' ', '\t', '\n', '\x0c', '\r']);
    let rest = rest.strip_prefix('+').unwrap_or(rest);
    let digits = &rest[..rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len())];
    if digits.is_empty() {
        return None;
    }
    let digits = digits.trim_start_matches('0');
    if digits.len() > 10 {
        return None;
    }
    let start = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    (start <= MAX).then_some(start)
}

/// Emit one `ListItem` per `<li>`, recursing into nested `<ul>`/`<ol>` at a
/// deeper level. Ordered items are numbered from the list's `start` attribute.
fn walk_list(
    list: ElementRef,
    ordered: bool,
    nodes: &mut Vec<Node>,
    level: u8,
    base: Fmt,
    images: &dyn ImageResolver,
) {
    let start = list
        .value()
        .attr("start")
        .and_then(parse_ol_start)
        .filter(|_| ordered);
    // docling emits an enumeration `<marker>` only for an ordered list with an
    // explicit `start` attribute; a plain `<ol>` carries no marker.
    let mut has_start = start.is_some();
    let mut number = start.unwrap_or(1);
    // Each top-level `<ul>`/`<ol>` is its own docling list group; its first
    // item is flagged so the Markdown serializer separates sibling lists with
    // a blank line (nested lists at level > 0 don't need it — the serializer
    // only splits at level 0).
    let mut first = level == 0;
    // Children of `<ul>`/`<ol>` other than `<li>` are invalid HTML, but common
    // in CMS output (docling#4440). Those before the first `<li>` precede the
    // list; a nested list is a sub-list of the item before it; any other
    // content closes the list, and the items after it open a new list that
    // continues the numbering — as an explicit start, so an `<ol>` resumes
    // with its markers.
    let children: Vec<_> = list.children().collect();
    let n_leading = children
        .iter()
        .take_while(|n| n.value().as_element().is_none_or(|e| e.name() != "li"))
        .count();
    walk_block_nodes(
        children[..n_leading].iter().copied(),
        nodes,
        level,
        base,
        images,
    );
    let mut prev_item = false;
    for &child in &children[n_leading..] {
        let li = match child.value() {
            HtmlNode::Text(t) if t.trim().is_empty() => continue,
            HtmlNode::Element(e) if e.name() == "li" => ElementRef::wrap(child).expect("element"),
            HtmlNode::Element(e) if matches!(e.name(), "ul" | "ol") && prev_item => {
                let sub = ElementRef::wrap(child).expect("element");
                walk_list(sub, e.name() == "ol", nodes, level + 1, base, images);
                continue;
            }
            HtmlNode::Text(_) | HtmlNode::Element(_) => {
                let before = nodes.len();
                walk_block_nodes([child], nodes, level, base, images);
                if nodes.len() > before {
                    first = level == 0;
                    prev_item = false;
                    has_start |= ordered;
                }
                continue;
            }
            // Comments, CDATA, processing instructions.
            _ => continue,
        };

        // GFM task-list form (docling#4401): the whole `<li>` is bare checkbox
        // input(s) plus inline text — `<li><input type="checkbox" checked>
        // done</li>`. The text is the checkbox's label, so the item is the
        // checkbox alone; it used to be a list item followed by an empty
        // checkbox on a bullet of its own.
        if let Some(boxes) = task_list_checkboxes(li) {
            nodes.extend(
                boxes
                    .into_iter()
                    .map(|(checked, text)| Node::CheckboxItem { checked, text }),
            );
            number += 1;
            continue;
        }

        // The item's own inline text, then its block content. Images fold into
        // the item text (so the list stays tight); nested lists and tables
        // follow as their own nodes.
        let mut runs = RunBuf {
            keep_br: true,
            ..RunBuf::default()
        };
        collect_li_inline(li, base, &mut runs);
        let mut text = finalize(&runs);
        let mut nested: Vec<(&str, ElementRef)> = Vec::new();
        append_li_blocks(li, &mut text, &mut nested);
        // An item without text still holds its table (docling#4617): an
        // empty item keeps it, and the numbering, in place.
        if !text.is_empty() || nested.iter().any(|(kind, _)| *kind == "table") {
            nodes.push(Node::ListItem {
                ordered,
                number,
                first_in_list: std::mem::take(&mut first),
                text,
                level,
                // docling's HTML backend passes an enumeration marker only for
                // an ordered list with an explicit `start`; otherwise none.
                // an explicit `list-style-type` string on the item wins
                // (docling#4669).
                marker: list_style_marker(li.value())
                    .or_else(|| has_start.then(|| format!("{number}."))),
                location: None,
                dclx: None,
                href: first_href(li),
                layer: None,
            });
            prev_item = true;
        }
        number += 1;
        for (kind, el) in nested {
            match kind {
                "ol" => walk_list(el, true, nodes, level + 1, base, images),
                "dl" => walk_dl(el, nodes, level, base, images),
                // docling nests the table under the item, and its Markdown
                // prints the grid right below the item line; here it is the
                // node after the item, so the Markdown blank-line-separates
                // it — the table keeps its rows and columns in every format,
                // where it used to flatten into the item text.
                "table" => handle_block(el, "table", nodes, level, base, images),
                _ => walk_list(el, false, nodes, level + 1, base, images),
            }
        }
    }
}

/// docling's `_get_list_style_marker` (docling#4669): the marker an inline
/// CSS string `list-style-type` sets on a `<li>` — `<li style="list-style-type:
/// '3. ';">`, how docling's HTML export writes an item's original marker — so
/// the marker survives the round trip (the Markdown backend takes it for every
/// file with an HTML block). The first declaration with a quoted value counts.
pub(super) fn list_style_marker(li: &scraper::node::Element) -> Option<String> {
    let style = li.attr("style")?;
    let lower = style.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find("list-style-type") {
        let rest = style[from + at + "list-style-type".len()..].trim_start();
        from += at + "list-style-type".len();
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(quote) = rest.chars().next().filter(|c| matches!(c, '\'' | '"')) else {
            continue;
        };
        let Some(end) = rest[1..].find(quote) else {
            continue;
        };
        let marker = rest[1..1 + end].trim();
        return (!marker.is_empty()).then(|| marker.to_string());
    }
    None
}

/// Block-level tags whose presence in a `<li>` rules out the GFM task-list
/// form (upstream's `_BLOCK_TAGS`).
const LI_BLOCK_TAGS: &[&str] = &[
    "address",
    "details",
    "dl",
    "figure",
    "footer",
    "img",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "ol",
    "p",
    "pre",
    "signature",
    "stamp",
    "summary",
    "table",
    "ul",
];

/// docling's `_is_task_list_item` + `_emit_task_list_inputs`: when a `<li>`
/// holds only checkbox/radio `<input>`s (its own — not a nested item's), no
/// custom checkbox markup and no block content, each input is a checkbox item
/// whose text is its bound `<label>` or, failing that, the item's own text.
fn task_list_checkboxes(li: ElementRef) -> Option<Vec<(bool, String)>> {
    let own_li = |el: ElementRef| {
        el.ancestors()
            .filter_map(ElementRef::wrap)
            .find(|a| a.value().name() == "li")
            .is_some_and(|a| a.id() == li.id())
    };
    let inputs: Vec<ElementRef> = li
        .select(cached_selector!("input"))
        .filter(|i| own_li(*i))
        .collect();
    if inputs.is_empty() {
        return None;
    }
    let is_checkbox_or_radio = |i: &ElementRef| {
        matches!(
            i.value()
                .attr("type")
                .unwrap_or("")
                .to_ascii_lowercase()
                .as_str(),
            "checkbox" | "radio"
        )
    };
    if !inputs.iter().all(is_checkbox_or_radio) {
        return None;
    }
    let has_custom_checkbox = li.descendants().filter_map(ElementRef::wrap).any(|d| {
        d.value().attr("class").is_some_and(|c| {
            c.split_whitespace()
                .any(|k| matches!(k, "checkbox" | "checkbox-box" | "checkbox-input"))
        })
    });
    if has_custom_checkbox
        || li
            .descendants()
            .filter_map(ElementRef::wrap)
            .any(|d| LI_BLOCK_TAGS.contains(&d.value().name()))
    {
        return None;
    }
    let li_text = normalize_ws(&text_with_breaks(li));
    Some(
        inputs
            .iter()
            .map(|input| {
                let mut text = checkbox_label_text(*input);
                if text.is_empty() {
                    text = li_text.clone();
                }
                (input.value().attr("checked").is_some(), text)
            })
            .collect(),
    )
}

/// Collect a list item's own inline text. Images and nested lists are pulled out
/// as blocks (handled by `walk_li_blocks`); `<p>`/`<div>` wrappers are folded in.
fn collect_li_inline(li: ElementRef, base: Fmt, runs: &mut RunBuf) {
    for child in li.children() {
        match child.value() {
            HtmlNode::Text(t) => {
                runs.text_node(t, base);
            }
            HtmlNode::Element(e) => {
                let Some(cref) = ElementRef::wrap(child) else {
                    continue;
                };
                if is_hidden(e) {
                    continue;
                }
                match e.name() {
                    "ul" | "ol" | "dl" | "img" | "table" => {} // pulled out as blocks
                    "p" | "div" | "section" | "article" | "blockquote" => {
                        // A block child is a word boundary (docling#3891):
                        // `<li>a<p>para</p>` reads `a para`.
                        runs.pending = true;
                        collect_li_inline(cref, base, runs);
                        runs.pending = true;
                    }
                    _ => collect_element(cref, base, None, runs),
                }
            }
            _ => {}
        }
    }
}

/// Append a `<li>`'s block content: an image folds into the item text as a
/// `<!-- image -->` marker on its own line (with a preceding caption line when
/// the image carries an `alt`) — docling nests such images under the list
/// item, and both its Markdown and this folding render them inside the list;
/// the chunker strips the markers again (its image placeholder is empty).
/// Nested lists are collected for emission as adjacent items. Recurses through
/// `<div>` wrappers.
fn append_li_blocks<'a>(
    elem: ElementRef<'a>,
    text: &mut String,
    nested: &mut Vec<(&'a str, ElementRef<'a>)>,
) {
    fn fold_img(text: &mut String, img: ElementRef) {
        text.push('\n');
        if let Some(alt) = img.value().attr("alt").filter(|a| !a.is_empty()) {
            text.push_str(&normalize_ws(alt));
            text.push('\n');
        }
        text.push_str("<!-- image -->");
    }
    for child in elem.children().filter_map(ElementRef::wrap) {
        let e = child.value();
        if is_hidden(e) {
            continue;
        }
        match e.name() {
            "img" => fold_img(text, child),
            "ul" => nested.push(("ul", child)),
            "ol" => nested.push(("ol", child)),
            "dl" => nested.push(("dl", child)),
            "table" => nested.push(("table", child)),
            "p" | "div" | "section" | "blockquote" => append_li_blocks(child, text, nested),
            _ => {
                // Images wrapped in inline markup (`<span><a><img></a></span>`):
                // the wrapper's text is collected inline elsewhere; only its
                // images surface here, like a direct `<img>` child.
                if has_descendant(child, "img") {
                    for img in child.select(cached_selector!("img")) {
                        fold_img(text, img);
                    }
                }
            }
        }
    }
}

/// A description list: each `<dt>` is a bold list item, each `<dd>` an item one
/// level deeper, recursing into nested `<dl>`/`<ul>`/`<ol>` (docling's rendering).
fn walk_dl(
    dl: ElementRef,
    nodes: &mut Vec<Node>,
    level: u8,
    base: Fmt,
    images: &dyn ImageResolver,
) {
    let bold = Fmt { bold: true, ..base };
    // A top-level `<dl>` is its own docling list group, like a `<ul>`: its
    // first term opens a new list, so the Markdown blank-separates it from
    // a list right before it.
    let mut first = level == 0;
    for child in dl.children() {
        let Some(c) = ElementRef::wrap(child) else {
            continue;
        };
        match c.value().name() {
            "dt" => {
                let text = render_inline_fmt(c, bold);
                if !text.is_empty() {
                    nodes.push(Node::ListItem {
                        ordered: false,
                        number: 1,
                        first_in_list: std::mem::take(&mut first),
                        text,
                        level,
                        marker: None,
                        location: None,
                        dclx: None,
                        href: first_href(c),
                        layer: None,
                    });
                }
            }
            "dd" => walk_dd(c, nodes, level + 1, base, images),
            _ => {}
        }
    }
}

/// A `<dd>`: its own inline text becomes an item at `level`, and any nested
/// `<dl>`/`<ul>`/`<ol>` is walked at the same level.
fn walk_dd(
    dd: ElementRef,
    nodes: &mut Vec<Node>,
    level: u8,
    base: Fmt,
    images: &dyn ImageResolver,
) {
    let mut runs = RunBuf::default();
    let mut nested: Vec<(&str, ElementRef)> = Vec::new();
    for child in dd.children() {
        match child.value() {
            HtmlNode::Text(t) => {
                runs.text_node(t, base);
            }
            HtmlNode::Element(e) => {
                let Some(cref) = ElementRef::wrap(child) else {
                    continue;
                };
                match e.name() {
                    "dl" | "ul" | "ol" => nested.push((e.name(), cref)),
                    _ => collect_element(cref, base, None, &mut runs),
                }
            }
            _ => {}
        }
    }
    let text = finalize(&runs);
    if !text.is_empty() {
        nodes.push(Node::ListItem {
            ordered: false,
            number: 1,
            first_in_list: false,
            text,
            level,
            marker: None,
            location: None,
            dclx: None,
            href: first_href(dd),
            layer: None,
        });
    }
    for (kind, el) in nested {
        match kind {
            "dl" => walk_dl(el, nodes, level, base, images),
            "ol" => walk_list(el, true, nodes, level, base, images),
            _ => walk_list(el, false, nodes, level, base, images),
        }
    }
}

/// Active inline formatting, accumulated from ancestor tags (mirrors docling's
/// `_FORMAT_TAG_MAP`). `underline` and `script` (sub/superscript) carry no
/// Markdown marker — they only surface in DocLang, via the structured runs.
/// `raw` suppresses `&<>`/`_` escaping — docling escapes body text but not
/// table-cell text.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Fmt {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    underline: bool,
    script: Script,
    raw: bool,
}

impl Fmt {
    /// The structured [`InlineRun`] for a text segment under this formatting
    /// (a hyperlink is intentionally dropped — DocLang inline scope keeps only
    /// the anchor text).
    fn to_inline_run(self, text: &str) -> InlineRun {
        InlineRun {
            text: text.to_string(),
            bold: self.bold,
            italic: self.italic,
            underline: self.underline,
            strike: self.strike,
            script: self.script,
            code: self.code,
            formula: false,
        }
    }
}

/// Parallel accumulator for a paragraph's inline content: the Markdown-marker
/// strings (`md`, joined/finalized for Markdown/JSON, unchanged) and the
/// structured runs (`rich`, one per text segment) that drive DocLang.
#[derive(Default)]
struct RunBuf {
    md: Vec<String>,
    /// Per `md` entry: whether the source had whitespace before it — the
    /// run boundary docling's runs now carry themselves (schema 1.11.0,
    /// #997). [`finalize`] joins on it instead of a blanket space, so
    /// `<b>x</b>.` is `**x**.` and `H<sub>2</sub>O` is `H2O`.
    gaps: Vec<bool>,
    /// Whitespace seen since the last `md` entry.
    pending: bool,
    /// A list item's runs (docling#3891): a `<br>` between two differently
    /// annotated runs stays a line break instead of reading as a space.
    keep_br: bool,
    rich: Vec<InlineRun>,
    /// Every text part in order with its annotation — docling's
    /// `AnnotatedTextList`, which the heading rule (`to_single_text_element`,
    /// [`render_heading`]) reads.
    parts: Vec<TextPart>,
    /// The last plain (non-link) text run — its index in `md` and `rich`, its
    /// Markdown-side and DocLang-side text and formatting — so the next
    /// same-annotation segment can merge into it ([`RunBuf::push_text`]).
    /// Invalidated by anything pushed after it (the indices no longer point
    /// at the tails).
    last_plain: Option<(usize, usize, String, String, Fmt)>,
    /// How many runs `rich` would hold without the same-annotation merge —
    /// what the anchor-folding rule in [`collect_element`] counts (a TOC
    /// entry `<a><span>1</span><span>Etymology</span></a>` is *fragmented*
    /// whether or not its two spans merged).
    unmerged_runs: usize,
    /// A `<br>` was just seen: the next same-formatting text segment folds into
    /// the previous run with a newline (docling keeps `a<br>b` as one text item
    /// `"a\nb"`, not two runs).
    merge_next: bool,
    /// The last pushed run is a hyperlink. `InlineRun` doesn't carry the link
    /// (it's baked into the md string), but docling treats a hyperlink as its
    /// own annotation — a pending `<br>` never folds across a link boundary.
    prev_link: bool,
}

/// One annotated text part (docling's `AnnotatedText`).
struct TextPart {
    text: String,
    fmt: Fmt,
    hyperlink: Option<String>,
    /// Whitespace before it in the source (see [`RunBuf::gaps`]).
    gap: bool,
}

impl RunBuf {
    /// Push a Markdown entry, recording whether whitespace preceded it.
    fn push_md(&mut self, md: String) {
        self.md.push(md);
        self.gaps.push(std::mem::take(&mut self.pending));
    }

    /// Note a source text node's leading whitespace: a boundary before
    /// what follows.
    fn ws_before(&mut self, raw: &str) {
        if raw
            .chars()
            .next()
            .is_some_and(docling_core::inline::is_space)
        {
            self.pending = true;
        }
    }

    /// Note a source text node's trailing whitespace.
    fn ws_after(&mut self, raw: &str) {
        if raw
            .chars()
            .last()
            .is_some_and(docling_core::inline::is_space)
        {
            self.pending = true;
        }
    }

    /// A bare text node of a block (no annotation of its own): its Markdown
    /// run and structured run, with its edge whitespace as boundaries.
    fn text_node(&mut self, raw: &str, base: Fmt) {
        self.ws_before(raw);
        let run = normalize_ws(raw);
        if !run.is_empty() {
            self.push_md(serialize_run(&run, base, None));
            self.push_rich(base.to_inline_run(&run));
        }
        self.ws_after(raw);
    }

    /// Append a plain text segment (already whitespace-normalized) as a
    /// Markdown run, a structured run and a part. Two adjacent segments with
    /// the same annotation — formatting, no hyperlink — merge into one run
    /// joined by a single space: upstream's `simplify_text_elements`, under
    /// which `<b><time>6:29 <abbr>p.m.</abbr></time></b>` is the one bold run
    /// `**6:29 p.m.**`, not `**6:29** **p.m.**`. A pending `<br>` takes
    /// precedence (the newline fold in [`Self::push_rich`]).
    fn push_text(&mut self, text: &str, fmt: Fmt, hyperlink: Option<&str>) {
        self.parts.push(TextPart {
            text: text.to_string(),
            fmt,
            hyperlink: hyperlink.map(str::to_string),
            gap: self.pending,
        });
        if hyperlink.is_none() && !self.merge_next {
            if let Some((mi, ri, prev_md, prev, pfmt)) = self.last_plain.clone() {
                if pfmt == fmt && mi + 1 == self.md.len() && ri + 1 == self.rich.len() {
                    // Merged faithfully: a space only where the source has
                    // one (#997). The DocLang run keeps its old join.
                    let sep = if std::mem::take(&mut self.pending) {
                        " "
                    } else {
                        ""
                    };
                    let merged_md = format!("{prev_md}{sep}{text}");
                    let merged = format!("{prev} {text}");
                    self.md[mi] = serialize_run(&merged_md, fmt, None);
                    self.rich[ri].text = merged.clone();
                    self.last_plain = Some((mi, ri, merged_md, merged, fmt));
                    self.unmerged_runs += 1;
                    return;
                }
            }
        }
        self.push_md(serialize_run(text, fmt, hyperlink));
        if hyperlink.is_some() {
            self.push_rich_link(fmt.to_inline_run(text));
            self.last_plain = None;
            self.unmerged_runs += 1;
        } else {
            let before = self.rich.len();
            self.push_rich(fmt.to_inline_run(text));
            if self.rich.len() == before {
                self.fold_markdown_over_break(fmt);
            }
            self.unmerged_runs += usize::from(self.rich.len() > before);
            // Folded into the previous run across a `<br>`: that run's text is
            // now two segments, so nothing later merges into it.
            self.last_plain = (self.rich.len() > before).then(|| {
                (
                    self.md.len() - 1,
                    self.rich.len() - 1,
                    text.to_string(),
                    text.to_string(),
                    fmt,
                )
            });
        }
    }

    /// The Markdown side of a `<br>` fold ([`Self::push_rich`] just joined the
    /// segment onto the previous run as `a\nb`): docling wraps that one text
    /// item as a whole, so `<b>a<br>b</b>` is `**a\nb**` — which a table cell
    /// flattens to `**a b**` — and not `**a**` newline `**b**`. The md stream
    /// holds `[…, "**a**", ␤, "**b**"]` here; a run of two or more sentinels
    /// is a paragraph break and is left alone.
    fn fold_markdown_over_break(&mut self, fmt: Fmt) {
        let n = self.md.len();
        if n < 3 || self.md[n - 2] != BR_SENTINEL || (n >= 4 && self.md[n - 4] == BR_SENTINEL) {
            return;
        }
        let Some(folded) = self.rich.last().map(|r| r.text.clone()) else {
            return;
        };
        self.md.truncate(n - 2);
        self.gaps.truncate(n - 2);
        self.md[n - 3] = serialize_run(&folded, fmt, None);
    }

    /// Append a text segment as a structured run, folding it into the previous
    /// run across a pending `<br>` when the formatting matches.
    fn push_rich(&mut self, run: InlineRun) {
        if self.merge_next {
            self.merge_next = false;
            if !self.prev_link {
                if let Some(last) = self.rich.last_mut() {
                    if same_style(last, &run) {
                        last.text.push('\n');
                        last.text.push_str(&run.text);
                        return;
                    }
                }
            }
            self.drop_single_sentinel();
        }
        self.rich.push(run);
        self.prev_link = false;
    }

    /// Append a hyperlink run. A link is its own annotation in docling, so a
    /// pending `<br>` never folds into it — the boundary becomes a space.
    fn push_rich_link(&mut self, run: InlineRun) {
        if self.merge_next {
            self.merge_next = false;
            self.drop_single_sentinel();
        }
        self.rich.push(run);
        self.prev_link = true;
    }

    /// Annotation boundary: docling's extractor attaches the `<br>` newline to
    /// the *following* fragment, whose `strip()` removes it when the fragment
    /// starts a new annotation — the parts then join with a single space. Drop
    /// the single pending sentinel from the md stream (a 2+ sentinel run
    /// stays: that's a paragraph break). The caller pushes the md run before
    /// the rich run, so the sentinel sits one before the end.
    fn drop_single_sentinel(&mut self) {
        if self.keep_br {
            return;
        }
        let n = self.md.len();
        if n >= 2 && self.md[n - 2] == BR_SENTINEL && (n < 3 || self.md[n - 3] != BR_SENTINEL) {
            self.md.remove(n - 2);
            self.gaps.remove(n - 2);
            // The fragment after the dropped `<br>` starts a new annotation:
            // the boundary reads as a space.
            self.gaps[n - 2] = true;
        }
    }
}

/// Whether two runs carry identical formatting (ignoring their text).
fn same_style(a: &InlineRun, b: &InlineRun) -> bool {
    a.bold == b.bold
        && a.italic == b.italic
        && a.underline == b.underline
        && a.strike == b.strike
        && a.script == b.script
        && a.code == b.code
}

/// Collect the inline content of `elem` as a Markdown string, the docling way:
/// each text node becomes a "run" carrying its ancestor formatting, and runs are
/// re-joined with single spaces (so `<a>x</a>.` → `[x](…) .`).
fn render_inline_fmt(elem: ElementRef, base: Fmt) -> String {
    let mut runs = RunBuf::default();
    collect_runs(elem, base, None, &mut runs);
    finalize(&runs)
}

/// A heading's Markdown — docling's `_handle_heading` makes the heading ONE
/// text item with `to_single_text_element`: every part stripped and joined
/// by single spaces, code if any part is code, and a formatting or a
/// hyperlink only when every non-blank part has it (docling#4670, #770). So
/// `<h2>To the <i>Hibernia</i></h2>` is `## To the Hibernia`, while
/// `<h2><i>To the</i> <i>Hibernia</i></h2>` is `## *To the Hibernia*`.
fn render_heading(elem: ElementRef, base: Fmt) -> String {
    let mut runs = RunBuf::default();
    collect_runs(elem, base, None, &mut runs);
    if runs.parts.is_empty() {
        return finalize(&runs);
    }
    // The parts concatenated with the source's boundaries (#997).
    let mut text = String::new();
    for p in &runs.parts {
        let t = p.text.trim();
        if t.is_empty() {
            continue;
        }
        if !text.is_empty() && p.gap {
            text.push(' ');
        }
        text.push_str(t);
    }
    let plain = Fmt {
        raw: base.raw,
        ..Fmt::default()
    };
    let fmt = shared_formatting(&runs.parts).unwrap_or(plain);
    let code = runs.parts.iter().any(|p| p.fmt.code);
    let hyperlink = shared_hyperlink(&runs.parts);
    serialize_run(&text, Fmt { code, ..fmt }, hyperlink.as_deref())
}

/// The formatting every non-blank part shares, `None` when they differ or
/// none is formatted — `to_single_text_element`'s formatting (docling#4670).
/// docling's `formatting` is `None` for a part under no formatting tag, and
/// code is not part of it.
fn shared_formatting(parts: &[TextPart]) -> Option<Fmt> {
    let key = |f: &Fmt| (f.bold, f.italic, f.strike, f.underline, f.script);
    let styled =
        |f: &Fmt| f.bold || f.italic || f.strike || f.underline || f.script != Script::Baseline;
    let mut text_parts = parts.iter().filter(|p| !p.text.trim().is_empty());
    let first = text_parts.next()?.fmt;
    (styled(&first) && text_parts.all(|p| key(&p.fmt) == key(&first))).then_some(first)
}

/// The hyperlink every non-blank part shares, `None` when they differ —
/// `to_single_text_element`'s hyperlink (docling#4670): a link on one word
/// of a heading or caption is not the whole item's.
fn shared_hyperlink(parts: &[TextPart]) -> Option<String> {
    let mut text_parts = parts.iter().filter(|p| !p.text.trim().is_empty());
    let first = text_parts.next()?.hyperlink.clone()?;
    text_parts
        .all(|p| p.hyperlink.as_deref() == Some(first.as_str()))
        .then_some(first)
}

/// Like [`render_inline_fmt`] but also returns the structured runs, for the
/// paragraph path that emits an `InlineGroup`.
fn render_inline(elem: ElementRef, base: Fmt) -> (String, Vec<InlineRun>) {
    let mut runs = RunBuf::default();
    collect_runs(elem, base, None, &mut runs);
    (finalize(&runs), runs.rich)
}

/// docling represents a `<br>` with a sentinel that the serializer rewrites.
const BR_SENTINEL: &str = "\u{e000}";
const BR_CHAR: char = '\u{e000}';

/// A `<pre>` part's text: each `<br>` sentinel with at most one space on
/// either side (` ?SENT ?`) is a newline; the ends are trimmed (#769).
pub(super) fn pre_line_breaks(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let space_br = chars[i] == ' ' && chars.get(i + 1) == Some(&BR_CHAR);
        if chars[i] == BR_CHAR || space_br {
            i += if space_br { 2 } else { 1 };
            if chars.get(i) == Some(&' ') {
                i += 1;
            }
            out.push('\n');
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    // No trim: a `<pre>`'s interior runs keep their whitespace (#997); the
    // callers trim the block's own edges.
    out
}

/// upstream's `_resolve_line_breaks` (docling#4675, #769): in text that is
/// one paragraph — a heading, a caption, a list item — every `<br>`
/// sentinel with the spaces before it and the spaces and sentinels after it
/// (` *SENT[ SENT]*`) is one line break.
pub(super) fn resolve_line_breaks(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let mut j = i;
        while j < chars.len() && chars[j] == ' ' {
            j += 1;
        }
        if j < chars.len() && chars[j] == BR_CHAR {
            j += 1;
            while j < chars.len() && (chars[j] == ' ' || chars[j] == BR_CHAR) {
                j += 1;
            }
            out.push('\n');
            i = j;
        } else {
            out.extend(&chars[i..j.max(i + 1)]);
            i = j.max(i + 1);
        }
    }
    // Never trims (docling#3891): it also runs on single inline runs, whose
    // edge whitespace is the run boundary; callers trim block edges.
    out
}

/// Join runs with single spaces, then turn `<br>` sentinels into newlines,
/// stripping the spaces the join inserted around them.
fn finalize(buf: &RunBuf) -> String {
    // Joined where the source had whitespace (#997), never elsewhere.
    let mut joined = String::new();
    for (i, run) in buf.md.iter().enumerate() {
        if i > 0 && buf.gaps.get(i).copied().unwrap_or(true) {
            joined.push(' ');
        }
        joined.push_str(run);
    }
    if !joined.contains(BR_SENTINEL) {
        return joined;
    }
    // " <br> " → "\n", stripping the spaces on both sides (docling's
    // `re.sub(r" *\n *", "\n")`).
    let nl = joined.replace(BR_SENTINEL, "\n");
    let segments: Vec<&str> = nl.split('\n').collect();
    let last = segments.len() - 1;
    let mut out = String::with_capacity(nl.len());
    for (i, seg) in segments.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(match (i, i == last) {
            (0, _) => seg.trim_end(),
            (_, true) => seg.trim_start(),
            _ => seg.trim(),
        });
    }
    out.trim_matches('\n').to_string()
}

fn collect_runs(elem: ElementRef, fmt: Fmt, hyperlink: Option<&str>, runs: &mut RunBuf) {
    for child in elem.children() {
        match child.value() {
            HtmlNode::Text(text) => {
                runs.ws_before(text);
                let normalized = normalize_ws(text);
                if !normalized.is_empty() {
                    runs.push_text(&normalized, fmt, hyperlink);
                }
                runs.ws_after(text);
            }
            HtmlNode::Element(_) => {
                if let Some(cref) = ElementRef::wrap(child) {
                    collect_element(cref, fmt, hyperlink, runs);
                }
            }
            _ => {}
        }
    }
}

/// Whether an inline element hides *structured* block content (#284). html5ever
/// follows the HTML5 tree-construction spec, under which an *unclosed* inline
/// tag (`<a name=…>`, `<b>`, `<font>` — endemic in HTML from older authoring
/// tools) legally keeps every subsequent block element as its child; walked
/// as inline content, a swallowed `<table>` flattens into text with its
/// structure silently gone. Callers block-walk such a wrapper instead.
///
/// Deliberately only the blocks whose structure cannot survive inline
/// flattening — tables, lists, headings, code blocks, figures. Pure text
/// containers (`div`, `p`, `section`) do NOT trigger: a well-formed
/// `<a href><div>text</div></a>` keeps rendering as a link around the
/// flattened text (hyperlink_04 in the corpus), exactly as before.
fn contains_block(elem: ElementRef) -> bool {
    elem.descendants().skip(1).any(|n| {
        n.value().as_element().is_some_and(|e| {
            matches!(
                e.name(),
                "table"
                    | "ul"
                    | "ol"
                    | "dl"
                    | "pre"
                    | "figure"
                    | "blockquote"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
            )
        })
    })
}

/// The inline formatting an element applies to its children — the same tag
/// set [`collect_element`] matches, for callers that block-walk an inline
/// wrapper (#284) and still owe the wrapper's formatting to its inline text.
/// A `<pre>` with inline structure as Markdown: each annotated segment a
/// code span over its verbatim text (#997, docling#3891 — preformatted, so
/// every interior space is significant and stays inside its span), same-
/// annotation neighbours merged, only the block's edges trimmed.
fn pre_runs_md(pre: ElementRef, base: Fmt) -> String {
    fn walk(
        el: ElementRef,
        fmt: Fmt,
        link: Option<&str>,
        out: &mut Vec<(String, Fmt, Option<String>)>,
    ) {
        for child in el.children() {
            match child.value() {
                HtmlNode::Text(t) => push(out, t, fmt, link),
                HtmlNode::Element(e) => {
                    let Some(cref) = ElementRef::wrap(child) else {
                        continue;
                    };
                    if is_hidden(e) || matches!(e.name(), "script" | "style" | "img") {
                        continue;
                    }
                    match e.name() {
                        "br" => push(out, BR_SENTINEL, fmt, link),
                        "a" => {
                            let href = e.attr("href").map(normalize_url);
                            walk(cref, fmt, href.as_deref().or(link), out);
                        }
                        name => walk(cref, tag_fmt(name, fmt), link, out),
                    }
                }
                _ => {}
            }
        }
    }
    fn push(out: &mut Vec<(String, Fmt, Option<String>)>, t: &str, fmt: Fmt, link: Option<&str>) {
        match out.last_mut() {
            Some((text, f, l)) if *f == fmt && l.as_deref() == link => text.push_str(t),
            _ => out.push((t.to_string(), fmt, link.map(str::to_string))),
        }
    }
    let mut segs = Vec::new();
    walk(pre, Fmt { code: true, ..base }, None, &mut segs);
    if let Some(head) = segs.first_mut() {
        head.0 = head.0.trim_start_matches('\n').to_string();
    }
    if let Some(tail) = segs.last_mut() {
        tail.0 = tail.0.trim_end().to_string();
    }
    let n = segs.len();
    let mut md = String::new();
    for (i, (text, fmt, link)) in segs.iter().enumerate() {
        let text = pre_line_breaks(text);
        if text.is_empty() {
            continue;
        }
        if text.trim().is_empty() && text.contains('\n') {
            // A run of `<br>` alone: a line break between its neighbours,
            // never inside a code span.
            if 0 < i && i + 1 < n {
                md.push('\n');
            }
            continue;
        }
        md.push_str(&serialize_run(&text, *fmt, link.as_deref()));
    }
    md
}

fn tag_fmt(name: &str, base: Fmt) -> Fmt {
    match name {
        "b" | "strong" => Fmt { bold: true, ..base },
        "i" | "em" | "var" => Fmt {
            italic: true,
            ..base
        },
        "s" | "del" | "strike" => Fmt {
            strike: true,
            ..base
        },
        "code" | "kbd" | "samp" => Fmt { code: true, ..base },
        "u" | "ins" => Fmt {
            underline: true,
            ..base
        },
        "sub" => Fmt {
            script: Script::Sub,
            ..base
        },
        "sup" => Fmt {
            script: Script::Super,
            ..base
        },
        _ => base,
    }
}

/// Process one inline element, applying its own tag (formatting / link / image)
/// before recursing into its children.
fn collect_element(elem: ElementRef, fmt: Fmt, hyperlink: Option<&str>, runs: &mut RunBuf) {
    let e = elem.value();
    if is_hidden(e) {
        return;
    }
    match e.name() {
        "b" | "strong" => collect_runs(elem, Fmt { bold: true, ..fmt }, hyperlink, runs),
        "i" | "em" | "var" => collect_runs(
            elem,
            Fmt {
                italic: true,
                ..fmt
            },
            hyperlink,
            runs,
        ),
        "s" | "del" | "strike" => collect_runs(
            elem,
            Fmt {
                strike: true,
                ..fmt
            },
            hyperlink,
            runs,
        ),
        "code" | "kbd" | "samp" => collect_runs(elem, Fmt { code: true, ..fmt }, hyperlink, runs),
        // Underline and sub/superscript have no Markdown marker; they split runs
        // (as before) and now also carry their formatting into the structured runs.
        "u" | "ins" => collect_runs(
            elem,
            Fmt {
                underline: true,
                ..fmt
            },
            hyperlink,
            runs,
        ),
        "sub" => collect_runs(
            elem,
            Fmt {
                script: Script::Sub,
                ..fmt
            },
            hyperlink,
            runs,
        ),
        "sup" => collect_runs(
            elem,
            Fmt {
                script: Script::Super,
                ..fmt
            },
            hyperlink,
            runs,
        ),
        // A single <br> becomes a newline within the block (see `finalize`). In
        // the structured stream it folds the next same-formatting segment into
        // the previous run with a newline (docling keeps `a<br>b` as one item).
        "br" => {
            runs.push_md(BR_SENTINEL.to_string());
            // A line break is a boundary: what follows is not glued on
            // (a heading's `Title<br>line` reads `Title line`).
            runs.pending = true;
            runs.merge_next = true;
        }
        "a" => {
            let href = e.attr("href").map(normalize_url);
            let link = href.as_deref().or(hyperlink);
            // An anchor whose content is fragmented across elements (spans,
            // divs — e.g. a TOC entry `<a><span>1</span><span>Etymology</span></a>`
            // or a citation `<span>[</span>1<span>]</span>`) folds into a single
            // hyperlink run, its fragments joined with single spaces — docling
            // emits one text item per anchor, not one per fragment.
            let mut inner = RunBuf::default();
            collect_runs(elem, fmt, None, &mut inner);
            if inner.unmerged_runs > 1 {
                let joined = inner
                    .rich
                    .iter()
                    .map(|r| r.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                let uniform = inner.rich.windows(2).all(|w| same_style(&w[0], &w[1]));
                let run_fmt = if uniform {
                    let r = &inner.rich[0];
                    Fmt {
                        bold: r.bold,
                        italic: r.italic,
                        strike: r.strike,
                        code: r.code,
                        underline: r.underline,
                        script: r.script,
                        ..fmt
                    }
                } else {
                    fmt
                };
                // The Markdown side keeps the fragments' own boundaries
                // (#997): `<span>[</span>1<span>]</span>` is `[1]`.
                let mut joined_md = String::new();
                for (i, part) in inner.parts.iter().enumerate() {
                    if i > 0 && part.gap {
                        joined_md.push(' ');
                    }
                    joined_md.push_str(&part.text);
                }
                if inner.parts.first().is_some_and(|p| p.gap) {
                    runs.pending = true;
                }
                runs.push_md(serialize_run(&joined_md, run_fmt, link));
                runs.pending = inner.pending;
                if link.is_some() {
                    runs.push_rich_link(run_fmt.to_inline_run(&joined));
                } else {
                    runs.push_rich(run_fmt.to_inline_run(&joined));
                }
                runs.parts.push(TextPart {
                    text: joined_md,
                    fmt: run_fmt,
                    hyperlink: link.map(str::to_string),
                    gap: runs.gaps.last().copied().unwrap_or(false),
                });
                runs.last_plain = None;
                runs.unmerged_runs += 1;
            } else {
                collect_runs(elem, fmt, link, runs);
            }
        }
        // An inline image (inside text / a `<span>`) produces no output: docling
        // never emits inline image markers — only block-level, `<a>`-wrapped, and
        // `<figure>` images become `<!-- image -->` pictures.
        "img" => {}
        "script" | "style" => {}
        // Transparent container (span, time, abbr, …): recurse.
        _ => collect_runs(elem, fmt, hyperlink, runs),
    }
}

/// Normalize an absolute `http(s)` URL the way docling's `pydantic.AnyUrl` does:
/// a bare scheme + host (no path) gets a trailing slash. Other URLs (relative
/// paths, fragments) are left as-is.
pub(crate) fn normalize_url(href: &str) -> String {
    if let Some(rest) = href
        .strip_prefix("https://")
        .or_else(|| href.strip_prefix("http://"))
    {
        if !rest.is_empty() && !rest.contains('/') {
            return format!("{href}/");
        }
    }
    href.to_string()
}

/// Apply formatting markers to a single run, in docling's order: code
/// (innermost, literal) → bold → italic → strikethrough → hyperlink (outermost).
fn serialize_run(text: &str, fmt: Fmt, hyperlink: Option<&str>) -> String {
    let mut res = if fmt.code {
        docling_core::code_span(text)
    } else if fmt.raw {
        text.to_string()
    } else {
        super::markdown::escape_html(&super::markdown::escape_underscores(text))
    };
    if fmt.bold {
        res = format!("**{res}**");
    }
    if fmt.italic {
        res = format!("*{res}*");
    }
    if fmt.strike {
        res = format!("~~{res}~~");
    }
    if let Some(href) = hyperlink {
        res = docling_core::hyperlink::markdown_link(&res, href);
    }
    res
}

fn extract_pre(pre: ElementRef) -> (Option<String>, String) {
    let mut language = pre
        .select(cached_selector!("code"))
        .next()
        .and_then(|code| code.value().attr("class").map(str::to_string))
        .and_then(|c| lang_from_class(&c));
    if language.is_none() {
        language = pre.value().attr("class").and_then(lang_from_class);
    }
    // Each `<br>` is a newline (#769), with at most one space around it
    // taken along.
    let mut text = String::new();
    for node in pre.descendants() {
        match node.value() {
            HtmlNode::Text(t) => text.push_str(t),
            HtmlNode::Element(e) if e.name() == "br" => text.push(BR_CHAR),
            _ => {}
        }
    }
    if text.contains(BR_CHAR) {
        text = pre_line_breaks(&text);
    }
    // Preformatted: interior whitespace is significant, only the block's
    // edges go (HTML drops the newline right after `<pre>`).
    (
        language,
        text.trim_start_matches('\n').trim_end().to_string(),
    )
}

/// Extract a language hint from a `class` like `language-rust` or `lang-rust`.
pub(super) fn lang_from_class(class: &str) -> Option<String> {
    class.split_whitespace().find_map(|c| {
        c.strip_prefix("language-")
            .or_else(|| c.strip_prefix("lang-"))
            .map(str::to_string)
    })
}

pub(crate) fn parse_table(table: ElementRef) -> Option<Table> {
    parse_table_cells(table, render_cell)
}

/// Flatten a table nested inside another table's cell, the way docling's
/// markdown serializer does (`_collect_subtree_text`): the nested table's own
/// grid cells, joined with single spaces. Each grid cell's text is docling's
/// raw `get_text` ([`subtree_text`]) rather than the Markdown-rendered cell, so
/// a still-deeper table inside one of those cells contributes its raw subtree
/// text — with source line breaks preserved as `\n` runs that the table
/// serializer flattens to spaces at render time. Spanning cells repeat into
/// every grid slot they cover, exactly as docling's grid walk does.
fn flatten_nested_table(table: ElementRef) -> String {
    parse_table_cells(table, |cell| {
        let mut out = String::new();
        subtree_text(cell, &mut out);
        // Flattening feeds an enclosing cell's *text*; no block content. Since
        // docling#3973 every text node is collapsed to single spaces on
        // extraction (`" ".join(text.split())`), so a still-deeper table's
        // line breaks and indentation no longer survive as space runs.
        let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
        (out, Vec::new())
    })
    .map(|t| {
        t.rows
            .iter()
            .flatten()
            .filter(|c| !c.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join(" ")
    })
    .unwrap_or_default()
}

/// docling's `HTMLDocumentBackend.get_text`, including BeautifulSoup's
/// whitespace semantics: a whitespace-only text node collapses to a single
/// newline when it spans a source line break (else a single space); any other
/// text node is kept verbatim; the content of a `p`/`li`/`th`/`td` gets one
/// trailing space; `<br>` becomes a newline. Only ASCII whitespace counts
/// (BeautifulSoup leaves `&nbsp;` and friends untouched).
pub(super) fn subtree_text(elem: ElementRef, out: &mut String) {
    for child in elem.children() {
        match child.value() {
            HtmlNode::Text(t) => {
                let t: &str = t;
                if t.chars().all(|c| c.is_ascii_whitespace()) {
                    out.push(if t.contains('\n') { '\n' } else { ' ' });
                } else {
                    // docling's `_clean_unicode` replacements, applied to the
                    // verbatim text (whitespace is deliberately NOT collapsed).
                    for c in t.chars() {
                        match c {
                            '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{00ad}' | '\u{feff}'
                            | '\u{2060}' => {}
                            '\u{00a0}' | '\u{202f}' => out.push(' '),
                            '\u{2010}'..='\u{2015}' => out.push('-'),
                            '\u{2018}' | '\u{2019}' => out.push('\''),
                            '\u{201c}' | '\u{201d}' => out.push('"'),
                            '\u{2026}' => out.push_str("..."),
                            _ => out.push(c),
                        }
                    }
                }
            }
            HtmlNode::Element(e) => {
                if is_skipped(e.name()) || is_hidden(e) {
                    continue;
                }
                if e.name() == "br" {
                    out.push('\n');
                    continue;
                }
                if let Some(cref) = ElementRef::wrap(child) {
                    subtree_text(cref, out);
                    if matches!(e.name(), "p" | "li" | "th" | "td") {
                        out.push(' ');
                    }
                }
            }
            _ => {}
        }
    }
}

/// Build a table's grid from its `<tr>`s. `render_cell` yields each cell's
/// flat Markdown text, whether the cell is *rich* (docling's `RichTableCell`),
/// and — for a rich cell — its structured block content for
/// [`Table::cell_blocks`] (empty otherwise).
fn parse_table_cells(
    table: ElementRef,
    render_cell: impl Fn(ElementRef) -> (String, Vec<Node>),
) -> Option<Table> {
    // Collect this table's own rows without descending into nested tables (a
    // recursive `select` would pull a nested table's cells into the outer grid).
    let mut trs: Vec<ElementRef> = Vec::new();
    for child in table.children().filter_map(ElementRef::wrap) {
        match child.value().name() {
            "tr" => trs.push(child),
            "thead" | "tbody" | "tfoot" => {
                trs.extend(
                    child
                        .children()
                        .filter_map(ElementRef::wrap)
                        .filter(|c| c.value().name() == "tr"),
                );
            }
            _ => {}
        }
    }

    // A `<tr>` whose cells are all spanning `<th>`s is a "row header" (e.g. a
    // rowspan label alone in its `<tr>`): it doesn't advance the row index, and
    // its cells are offset into the following rows. `num_rows` therefore counts
    // only non-row-header rows; cells are clamped to the `num_rows × num_cols`
    // grid. Mirrors docling's `parse_table_data`.
    let mut num_rows = 0usize;
    let mut row_spans: Vec<Vec<(usize, usize)>> = Vec::with_capacity(trs.len());
    for tr in &trs {
        let cells = row_cells(*tr);
        row_spans.push(
            cells
                .iter()
                .map(|c| (span_attr(*c, "colspan"), span_attr(*c, "rowspan")))
                .collect(),
        );
        if !is_row_header(&cells) {
            num_rows += 1;
        }
    }
    let num_cols = table_width(&row_spans);
    // A table whose every row is a "row header" (e.g. one `<th rowspan>` and
    // no body rows to span into) would count zero rows and vanish — upstream's
    // out-of-bounds crash in the same shape (docling#3827, 2.122); it keeps
    // the cells, so treat the rows as ordinary ones instead.
    let all_row_headers = num_rows == 0 && !trs.is_empty();
    if all_row_headers {
        num_rows = trs.len();
    }
    if num_rows == 0 || num_cols == 0 {
        return None;
    }

    let mut grid: Vec<Vec<Option<String>>> = vec![vec![None; num_cols]; num_rows];
    // Per-cell `<th>` flags, span-replicated alongside the text grid — docling's
    // cell-level `column_header` (drives `<ched/>` and the chunker's dataframe
    // header detection).
    let mut th_grid: Vec<Vec<bool>> = vec![vec![false; num_cols]; num_rows];
    // Per-cell `row_header` (docling#4216): a `<th>` labelling the rows beside
    // it rather than the column above them — every cell of a spanning
    // row-header row, and a lone `<th>` in a row that also holds `<td>` data.
    let mut rh_grid: Vec<Vec<bool>> = vec![vec![false; num_cols]; num_rows];
    // Span continuations (#240): a covered position continues its anchor
    // horizontally / vertically — the source of real `TableCell` spans and
    // the DocLang `lcel`/`ucel` tokens.
    let mut col_cont: Vec<Vec<bool>> = vec![vec![false; num_cols]; num_rows];
    let mut row_cont: Vec<Vec<bool>> = vec![vec![false; num_cols]; num_rows];
    // Parallel per-cell block content for rich cells (#328), indexed like
    // `grid`. Empty for plain cells; `None` on the table when no cell is rich,
    // exactly as the docx/odf backends do.
    let mut blocks: Vec<Vec<Vec<Node>>> = vec![vec![Vec::new(); num_cols]; num_rows];
    let mut any_rich = false;
    let mut row_idx: isize = -1;
    let mut start_row_span: usize = 0;
    for tr in &trs {
        let cells = row_cells(*tr);
        let row_header = !all_row_headers && is_row_header(&cells);
        if row_header {
            start_row_span += 1;
        } else {
            row_idx += 1;
            start_row_span = 0;
        }
        let base = (row_idx + start_row_span as isize).max(0) as usize;

        // docling's `column_header` is row-level: a row is a header row only
        // when it contains no `<td>` at all; a `<th scope=row>` label next to
        // `<td>` data is a *row* header, not a column header.
        let all_th = cells.iter().all(|c| c.value().name() == "th");
        let mut col = 0;
        for cell in cells {
            let colspan = span_attr(cell, "colspan");
            let mut rowspan = span_attr(cell, "rowspan");
            if row_header {
                rowspan = rowspan.saturating_sub(1);
            }
            while col < num_cols && base < num_rows && grid[base][col].is_some() {
                col += 1;
            }
            let (text, cell_nodes) = render_cell(cell);
            // docling#4216: a row-header row's cells label the rows they span
            // into, so they are *row* headers — flagging them `column_header`
            // (as docling did until 2.126) made docling-core 2.96 fold the
            // first data row into the Markdown header (`Year - 2025`).
            let is_th = all_th && !row_header;
            let is_rh = row_header || (!all_th && cell.value().name() == "th");
            any_rich |= !cell_nodes.is_empty();
            let mut anchor_filled = false;
            for r in start_row_span..start_row_span + rowspan {
                let gr = (row_idx + r as isize).max(0) as usize;
                for dc in 0..colspan {
                    let gc = col + dc;
                    if gr < num_rows && gc < num_cols {
                        // Every position of the span repeats the cell's text,
                        // a rich cell's too (docling-core#857, #777: the
                        // serializers resolve a `RichTableCell` once and
                        // repeat it).
                        grid[gr][gc] = Some(text.clone());
                        // The blocks live on the anchor slot only: a covered
                        // position is a continuation token in DocLang, and
                        // LaTeX and the chunker resolve it to its anchor.
                        if !anchor_filled && !cell_nodes.is_empty() {
                            blocks[gr][gc] = cell_nodes.clone();
                        }
                        anchor_filled = true;
                        th_grid[gr][gc] = is_th;
                        rh_grid[gr][gc] = is_rh;
                        col_cont[gr][gc] = dc > 0;
                        row_cont[gr][gc] = r > start_row_span;
                    }
                }
            }
            col += colspan;
        }
    }

    let rows: Vec<Vec<String>> = grid
        .into_iter()
        .map(|row| row.into_iter().map(Option::unwrap_or_default).collect())
        .collect();
    (!rows.is_empty()).then_some(Table {
        footnotes: Vec::new(),
        rows,
        location: None,
        structure: Some(docling_core::TableStructure {
            col_header: th_grid,
            row_header: rh_grid,
            col_continuation: col_cont,
            row_continuation: row_cont,
            ..Default::default()
        }),
        cell_blocks: any_rich.then_some(blocks),
        cells: None,
        caption: None,
        caption_parent: Default::default(),
        caption_location: None,
    })
}

/// A `<tr>`'s direct `<td>`/`<th>` cells (not nested-table cells).
fn row_cells(tr: ElementRef) -> Vec<ElementRef> {
    tr.children()
        .filter_map(ElementRef::wrap)
        .filter(|c| matches!(c.value().name(), "td" | "th"))
        .collect()
}

/// A row is a "row header" when all its cells are spanning `<th>`s.
fn is_row_header(cells: &[ElementRef]) -> bool {
    !cells.is_empty()
        && cells
            .iter()
            .all(|c| c.value().name() == "th" && span_attr(*c, "rowspan") > 1)
}

/// docling's `_get_cell_spans`: the attribute's leading run of digits when
/// its first character is a digit (`"2px"` → 2, `" 2"` → 1 — no trimming),
/// and never below 1 (docling#4287, 2.129: a `colspan="0"` covered no grid
/// position, so the cell dropped out and every later cell shifted left;
/// HTML5's "rowspan=0 spans to the end of the row group" is not modelled,
/// the cell simply keeps one slot).
/// docling's `table_width` (`backend/utils/table_spans.py`, docling#4414 and
/// #4527): the number of columns of a table, given each row's `(colspan,
/// rowspan)` pairs. The declared width is the widest row's colspan sum, cut
/// after the last column some cell starts in — the columns right of it hold
/// no cell of their own, and browsers collapse them too. A cell goes in the
/// first column no earlier cell still covers, so a rowspan shifts the cells
/// of the rows it reaches to the right, and the table widens to keep them
/// (they used to fall off the grid); a cell covers only the columns it holds
/// inside the declared width, so an oversized colspan cannot widen every row
/// below it.
pub(super) fn table_width(rows: &[Vec<(usize, usize)>]) -> usize {
    let mut width = 0usize;
    let mut last_start: Option<usize> = None;
    for cells in rows {
        let mut col = 0usize;
        for &(col_span, _) in cells {
            last_start = last_start.max(Some(col));
            col += col_span;
        }
        width = width.max(col);
    }
    let declared = width.min(last_start.map_or(0, |c| c + 1));
    // The row, exclusive, up to which the cells placed so far cover a column.
    let mut covered_until = vec![0usize; declared];
    let mut last_start: Option<usize> = None;
    for (row_idx, cells) in rows.iter().enumerate() {
        let mut col = 0usize;
        for &(col_span, row_span) in cells {
            while covered_until.get(col).is_some_and(|&until| until > row_idx) {
                col += 1;
            }
            last_start = last_start.max(Some(col));
            for until in covered_until.iter_mut().take(col + col_span).skip(col) {
                *until = (*until).max(row_idx + row_span);
            }
            col += col_span;
        }
    }
    declared.max(last_start.map_or(0, |c| c + 1))
}

fn span_attr(cell: ElementRef, name: &str) -> usize {
    let limit = if name == "colspan" {
        MAX_COLSPAN
    } else {
        MAX_ROWSPAN
    };
    span_value(cell.value().attr(name), limit)
}

/// docling's `MAX_COLSPAN` / `MAX_ROWSPAN` (`backend/utils/table_spans.py`,
/// HTML's own limits): a declared span past them is clamped, so a
/// `colspan="999999999"` cannot make the grid builder allocate billions of
/// slots.
pub(super) const MAX_COLSPAN: usize = 1000;
pub(super) const MAX_ROWSPAN: usize = 65_534;

/// One span attribute as `_get_cell_spans` reads it — [`span_attr`]'s rule,
/// clamped to `[1, limit]` (`clamp_span`); a digit run too long to parse is
/// over every limit, so it is the limit. Shared with the JSON item tree
/// (`html_tree`), which must see the same grid.
pub(super) fn span_value(raw: Option<&str>, limit: usize) -> usize {
    let Some(v) = raw else {
        return 1;
    };
    if !v.starts_with(|c: char| c.is_numeric()) {
        return 1;
    }
    let digits: String = v.chars().take_while(|c| c.is_ascii_digit()).collect();
    match digits.trim_start_matches('0') {
        "" => 1,
        d => d.parse::<usize>().map_or(limit, |n| n.clamp(1, limit)),
    }
}

/// Render a table cell to Markdown. docling treats a cell as "rich" (and
/// serializes its full block content — headings, paragraphs, lists, code) only
/// when it carries structure; a single plain run is emitted as plain text.
/// Either way inline text is left unescaped; the table serializer flattens
/// newlines to spaces.
/// One table cell: its flat Markdown text (what `rows` — and so Markdown and
/// JSON — carry) and a rich cell's structured blocks for
/// [`Table::cell_blocks`] (#328; empty for a plain cell).
///
/// The two are produced by *separate* walks on purpose. The text walk is `raw`
/// (no Markdown escaping, nested tables flattened to their joined cell text) so
/// the flat rendering stays byte-identical to docling's. The block walk is an
/// ordinary one: a nested `<table>` stays a real [`Node::Table`], a list stays
/// list items, a heading stays a heading — which is what the DocLang and LaTeX
/// serializers render, matching upstream's `RichTableCell`.
fn render_cell(cell: ElementRef) -> (String, Vec<Node>) {
    let raw = Fmt {
        raw: true,
        ..Fmt::default()
    };
    if !is_rich_cell(cell) {
        return (render_inline_fmt(cell, raw), Vec::new());
    }
    let mut nodes: Vec<Node> = Vec::new();
    // Images inside table cells stay placeholders (they fold into cell text).
    walk_block(cell, &mut nodes, 0, raw, &NoFetch);
    let mut doc = DoclingDocument::new("");
    doc.nodes = nodes;
    // In-cell rendering: a heading inside the cell is plain text
    // (docling-core#540).
    let text = doc.export_to_table_cell_markdown().trim().to_string();

    let mut blocks: Vec<Node> = Vec::new();
    walk_block(cell, &mut blocks, 0, Fmt::default(), &NoFetch);
    wrap_cell_inline_groups(&mut blocks);
    (text, blocks)
}

/// Inside a table cell an inline group is always `<text>`-wrapped: docling
/// serializes the cell's `InlineGroup` as its own text item. Only the *body*
/// unwraps a group that follows a heading ([`push_inline_paragraph`]), and a
/// heading inside a cell must not drag that rule in with it.
fn wrap_cell_inline_groups(nodes: &mut [Node]) {
    for node in nodes {
        match node {
            Node::InlineGroup { unwrapped, .. } => *unwrapped = false,
            Node::Group { children, .. } => wrap_cell_inline_groups(children),
            _ => {}
        }
    }
}

/// docling's `_is_rich_table_cell`: a cell is rich if it has a `<br>`, more than
/// one direct `<p>/<div>/<li>`, more than one text run, a single run carrying
/// formatting/link/code, or only an image/input.
pub(super) fn is_rich_cell(cell: ElementRef) -> bool {
    if has_descendant(cell, "br") {
        return true;
    }
    let direct_blocks = cell
        .children()
        .filter_map(ElementRef::wrap)
        .filter(|c| matches!(c.value().name(), "p" | "div" | "li"))
        .count();
    if direct_blocks > 1 {
        return true;
    }
    let (runs, markup) = cell_richness(cell);
    match runs {
        0 => has_descendant(cell, "img") || has_descendant(cell, "input"),
        1 => markup,
        _ => true,
    }
}

/// If `p`'s only meaningful content is a single inline-code element, return its
/// text (docling turns such a paragraph into a code block).
fn lone_code(p: ElementRef) -> Option<String> {
    let mut code: Option<ElementRef> = None;
    for child in p.children() {
        match child.value() {
            HtmlNode::Text(t) => {
                if !t.trim().is_empty() {
                    return None;
                }
            }
            HtmlNode::Element(e) => {
                if !matches!(e.name(), "code" | "kbd" | "samp") || code.is_some() {
                    return None;
                }
                code = ElementRef::wrap(child);
            }
            _ => {}
        }
    }
    code.map(|c| c.text().collect::<String>())
}

/// If `elem` wraps exactly one image and no other text, return the image's
/// caption (its non-empty `alt`) and `src`. Used to pull `<a><img></a>` out as a
/// Picture.
fn image_wrapper(elem: ElementRef) -> Option<(Option<String>, Option<String>)> {
    let mut imgs = elem.select(cached_selector!("img"));
    let img = imgs.next()?;
    if imgs.next().is_some() || !elem.text().collect::<String>().trim().is_empty() {
        return None;
    }
    let caption = img
        .value()
        .attr("alt")
        .filter(|a| !a.is_empty())
        .map(str::to_string);
    let src = img_src(img.value());
    Some((caption, src))
}

/// Hang an enclosing anchor's href on every captioned picture it wraps —
/// docling's caption hyperlink annotation. A picture without a caption has
/// nowhere to hang it, and one that already carries its own (a nested
/// `<figcaption>` link) keeps it.
fn annotate_picture_captions(nodes: &mut [Node], href: &str) {
    for node in nodes {
        if let Node::Picture {
            caption: Some(_),
            caption_href,
            ..
        } = node
        {
            if caption_href.is_none() {
                *caption_href = Some(href.to_string());
            }
        }
    }
}

/// The hyperlink of a figure's `<figcaption>` — docling's caption hyperlink
/// annotation (the caption text keeps the anchor text inline, the href
/// rides on the caption item): only a link over the whole caption text
/// (docling#4670, #770); a link on one word is no caption hyperlink.
fn figcaption_href(fig: ElementRef) -> Option<String> {
    let figcaption = fig.select(cached_selector!("figcaption")).next()?;
    let mut runs = RunBuf::default();
    collect_runs(figcaption, Fmt::default(), None, &mut runs);
    shared_hyperlink(&runs.parts)
        .filter(|h| !h.is_empty())
        .map(|h| normalize_url(&h))
}

/// The image URL of a `<figure>`'s first `<img>`, for image extraction.
#[allow(dead_code)]
fn figure_img_src(fig: ElementRef) -> Option<String> {
    fig.select(cached_selector!("img"))
        .next()
        .and_then(|img| img_src(img.value()))
}

/// The real image URL of an `<img>`, accounting for lazy-loading: a normal
/// `src` is authoritative, but many pages leave `src` empty or a placeholder
/// and put the true URL in `data-src` (or a `srcset`), so fall back to those —
/// otherwise every lazy-loaded image would extract nothing.
pub(super) fn img_src(el: &scraper::node::Element) -> Option<String> {
    let attr = |k: &str| el.attr(k).map(str::trim).filter(|s| !s.is_empty());
    // A non-placeholder src wins.
    if let Some(s) = attr("src") {
        if !s.starts_with("data:") {
            return Some(s.to_string());
        }
    }
    // Common lazy-load conventions.
    for k in ["data-src", "data-original", "data-lazy-src", "data-lazy"] {
        if let Some(s) = attr(k) {
            return Some(s.to_string());
        }
    }
    // srcset / data-srcset: take the first candidate's URL (before its
    // descriptor, e.g. `foo.png 2x` / `foo.png 640w`).
    for k in ["srcset", "data-srcset"] {
        if let Some(s) = attr(k) {
            if let Some(u) = s
                .split(',')
                .next()
                .and_then(|c| c.split_whitespace().next())
                .filter(|u| !u.is_empty())
            {
                return Some(u.to_string());
            }
        }
    }
    // Last resort: a `data:` src (a real inline image, no lazy target).
    attr("src").map(str::to_string)
}

pub(super) fn has_descendant(elem: ElementRef, name: &str) -> bool {
    // Callers pass a small fixed set of tags; cache those selectors (this runs
    // per table cell). Anything else falls back to an on-demand parse.
    let sel = match name {
        "br" => cached_selector!("br"),
        "img" => cached_selector!("img"),
        "input" => cached_selector!("input"),
        _ => return Selector::parse(name).is_ok_and(|s| elem.select(&s).next().is_some()),
    };
    elem.select(sel).next().is_some()
}

/// Count the inline text runs in `cell` and whether any carries formatting,
/// a hyperlink, or code (matching docling's annotation list).
fn cell_richness(cell: ElementRef) -> (usize, bool) {
    fn walk(elem: ElementRef, marked: bool, count: &mut usize, markup: &mut bool) {
        for child in elem.children() {
            match child.value() {
                HtmlNode::Text(t) => {
                    if !normalize_ws(t).is_empty() {
                        *count += 1;
                        if marked {
                            *markup = true;
                        }
                    }
                }
                HtmlNode::Element(e) => {
                    let Some(cref) = ElementRef::wrap(child) else {
                        continue;
                    };
                    // docling's `_FORMAT_TAG_MAP` (plus `<a>`, whose hyperlink
                    // makes a lone run rich): underline and sub/superscript
                    // count even though Markdown has no marker for them — the
                    // formatting still reaches DocLang and LaTeX.
                    let marks = matches!(
                        e.name(),
                        "b" | "strong"
                            | "i"
                            | "em"
                            | "var"
                            | "s"
                            | "del"
                            | "strike"
                            | "u"
                            | "ins"
                            | "sub"
                            | "sup"
                            | "code"
                            | "kbd"
                            | "samp"
                            | "a"
                    );
                    walk(cref, marked || marks, count, markup);
                }
                _ => {}
            }
        }
    }
    let mut count = 0;
    let mut markup = false;
    walk(cell, false, &mut count, &mut markup);
    (count, markup)
}

/// The plain text of a `<figure>`'s `<figcaption>` (docling's caption
/// `to_single_text_element`); `None` without a figcaption or when it is blank.
fn figcaption_text(fig: ElementRef) -> Option<String> {
    caption_text(fig.select(cached_selector!("figcaption")).next()?)
}

/// A `<table>`'s own `<caption>` child (docling's `tag.find("caption",
/// recursive=False)`), as caption text.
fn table_caption_text(table: ElementRef) -> Option<String> {
    let cap = table
        .children()
        .filter_map(ElementRef::wrap)
        .find(|c| c.value().name() == "caption")?;
    caption_text(cap)
}

/// The text of a `<figcaption>`/`<caption>` element (docling's
/// `_emit_caption`).
fn caption_text(cap: ElementRef) -> Option<String> {
    // A caption is plain text (formatting/links are stripped): docling's
    // `normalize_whitespace().to_single_text_element()` concatenates the
    // source text nodes with their own boundary whitespace, one space where
    // there was any (#997: `a <a>b</a>.` is "a b.", a cite's `[`/`49`/`]`
    // spans "[49]"). A `<br>` is a line break (`_resolve_line_breaks`, #769).
    let mut buf = RunBuf::default();
    for node in cap.descendants() {
        match node.value() {
            HtmlNode::Text(t) => {
                buf.ws_before(t);
                let frag = normalize_ws(t);
                if !frag.is_empty() {
                    buf.push_md(frag);
                }
                buf.ws_after(t);
            }
            HtmlNode::Element(e) if e.name() == "br" => buf.push_md(BR_SENTINEL.to_string()),
            _ => {}
        }
    }
    let mut joined = String::new();
    for (i, frag) in buf.md.iter().enumerate() {
        if i > 0 && buf.gaps[i] {
            joined.push(' ');
        }
        joined.push_str(frag);
    }
    let text = resolve_line_breaks(&joined).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Sanitize typographic Unicode to ASCII (docling's HTML text cleanup) and
/// collapse all runs of whitespace to single spaces, trimming the ends — in a
/// single pass (this runs once per text run, so it stays allocation-light).
pub(super) fn normalize_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    // A space is pending between emitted words; flushed only before the next
    // non-space char, so leading/trailing whitespace is trimmed and runs collapse.
    let mut pending_space = false;
    for ch in s.chars() {
        match ch {
            // Zero-width / soft / joiner characters are dropped outright.
            '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{00ad}' | '\u{feff}' | '\u{2060}' => {}
            // Any whitespace (incl. the (narrow) non-breaking spaces, which are
            // Unicode-whitespace) collapses to a single pending space.
            c if c.is_whitespace() => {
                pending_space = !out.is_empty();
            }
            c => {
                if pending_space {
                    out.push(' ');
                    pending_space = false;
                }
                match c {
                    '\u{2010}'..='\u{2015}' => out.push('-'), // hyphens, dashes, horizontal bar
                    '\u{2018}' | '\u{2019}' => out.push('\''), // single quotation marks
                    '\u{201c}' | '\u{201d}' => out.push('"'), // double quotation marks
                    '\u{2026}' => out.push_str("..."),        // ellipsis
                    _ => out.push(c),
                }
            }
        }
    }
    out
}

/// The text for a checkbox `<input>`: the joined text of every `<label>` bound
/// to it (`for=` its id, or a wrapping label), else its `aria-label`.
/// An element's text with each `<br>` read as a line break — the text of a
/// checkbox (upstream's `_extract`, which turns the `<br>` sentinel into
/// `\n`), which then collapses to one line: `Task<br>line` is `Task line`,
/// not `Taskline` (docling#4675, #769).
pub(super) fn text_with_breaks(el: ElementRef) -> String {
    let mut out = String::new();
    for node in el.descendants() {
        match node.value() {
            scraper::Node::Text(t) => out.push_str(t),
            scraper::Node::Element(e) if e.name() == "br" => out.push('\n'),
            _ => {}
        }
    }
    out
}

pub(super) fn checkbox_label_text(input: ElementRef) -> String {
    let mut texts: Vec<String> = Vec::new();
    if let Some(id) = input.value().attr("id").filter(|i| !i.is_empty()) {
        let root = root_of(input);
        for label in root.select(cached_selector!("label")) {
            if label.value().attr("for") == Some(id) {
                let t = normalize_ws(&text_with_breaks(label));
                if !t.is_empty() {
                    texts.push(t);
                }
            }
        }
    }
    if texts.is_empty() {
        // input wrapped in a <label>…</label>
        let mut cur = input.parent();
        while let Some(node) = cur {
            if let Some(el) = ElementRef::wrap(node) {
                if el.value().name() == "label" {
                    let t = normalize_ws(&text_with_breaks(el));
                    if !t.is_empty() {
                        texts.push(t);
                    }
                    break;
                }
            }
            cur = node.parent();
        }
    }
    if texts.is_empty() {
        if let Some(aria) = input.value().attr("aria-label") {
            let t = normalize_ws(aria);
            if !t.is_empty() {
                texts.push(t);
            }
        }
    }
    texts.join(" ")
}

/// Whether this `<label>`'s text is consumed by a checkbox/radio input (bound
/// via `for=` or wrapping it), so it should not render again.
pub(super) fn label_feeds_checkbox(label: ElementRef) -> bool {
    let is_checkbox = |el: ElementRef| {
        el.value().name() == "input"
            && matches!(
                el.value()
                    .attr("type")
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .as_str(),
                "checkbox" | "radio"
            )
    };
    if let Some(target) = label.value().attr("for").filter(|f| !f.is_empty()) {
        let root = root_of(label);
        for input in root.select(cached_selector!("input")) {
            if input.value().attr("id") == Some(target) {
                return is_checkbox(input);
            }
        }
        return false;
    }
    // A wrapping label: consumed if it contains a checkbox input.
    label.select(cached_selector!("input")).any(is_checkbox)
}

/// The document root element containing `el` (for whole-document selects).
fn root_of(el: ElementRef) -> ElementRef {
    let mut cur = el;
    while let Some(parent) = cur.parent().and_then(ElementRef::wrap) {
        cur = parent;
    }
    cur
}

#[cfg(test)]
mod nesting_guard_tests {
    use super::*;

    #[test]
    fn estimate_ignores_void_and_optional_end_tags_and_raw_text() {
        let html = "<html><head><script>if (a<b) { x = '<div><div>'; }</script></head>\
                    <body><p>one<p>two<p>three<li>a<li>b<br><img src=x><div title='<i><i>'>\
                    <span><b>x</b></span></div><!-- <div><div><div> --></body></html>";
        assert!(nesting_estimate_within(html, 4));
        let deep = "<div>".repeat(600) + "x" + &"</div>".repeat(600);
        assert!(!nesting_estimate_within(&deep, 512));
        assert!(nesting_estimate_within(&deep, 600));
    }

    #[test]
    fn strip_tags_keeps_text_and_decodes_entities() {
        let html =
            "<div><p>a &amp; b &lt;c&gt; &#233;&#xE9;</p><script>x<y</script><!-- z --></div>";
        assert_eq!(normalize_ws(&strip_tags(html)), "a & b <c> éé");
    }

    /// The over-deep page converts to its text through the tag scan, without
    /// a DOM.
    #[test]
    fn over_deep_html_falls_back_to_text_without_parsing() {
        let deep = "<html><body>".to_string()
            + &"<div>".repeat(5000)
            + "deep text"
            + &"</div>".repeat(5000)
            + "</body></html>";
        let mut out = Vec::new();
        append_fragment(&deep, &mut out, &NoFetch);
        assert_eq!(out.len(), 1);
        assert!(
            matches!(&out[0], Node::Paragraph { text } if text == "deep text"),
            "{out:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    fn convert(html: &str) -> DoclingDocument {
        let src = SourceDocument::from_bytes("t", InputFormat::Html, html.as_bytes().to_vec());
        HtmlBackend.convert(&src).unwrap()
    }

    fn convert_bytes(html: &[u8]) -> DoclingDocument {
        let src = SourceDocument::from_bytes("t", InputFormat::Html, html.to_vec());
        HtmlBackend.convert(&src).unwrap()
    }

    /// #769 (docling#4675): one-paragraph text turns each run of `<br>`
    /// sentinels, with the spaces around them, into one line break; `<pre>`
    /// text turns each sentinel (with one space either side) into one.
    #[test]
    fn br_sentinels_resolve_to_line_breaks() {
        let b = BR_CHAR;
        assert_eq!(
            resolve_line_breaks(&format!("Title {b} line")),
            "Title\nline"
        );
        assert_eq!(
            resolve_line_breaks(&format!("a {b} {b}{b} b  c")),
            "a\nb  c"
        );
        // No trim: callers trim the block's edges (#997).
        assert_eq!(resolve_line_breaks(&format!(" {b} ")), "\n");
        assert_eq!(pre_line_breaks(&format!("Code{b}line")), "Code\nline");
        assert_eq!(pre_line_breaks(&format!("a  {b}  b{b}{b}c")), "a \n b\n\nc");
    }

    /// docling's `_handle_heading` builds the heading with
    /// `to_single_text_element`: parts stripped and joined by spaces, a
    /// formatting kept only when all of the text has it (docling#4670, #770).
    #[test]
    fn heading_keeps_only_the_formatting_of_all_its_text() {
        let md = convert("<h2>To the <i>Hibernia</i></h2>").export_to_markdown();
        assert!(md.contains("## To the Hibernia"), "{md}");
        let md = convert("<h2><b>Bold</b> then <i>italic</i></h2>").export_to_markdown();
        assert!(md.contains("## Bold then italic"), "{md}");
        let md = convert("<h2><i>To the</i> <i>Hibernia</i></h2>").export_to_markdown();
        assert!(md.contains("## *To the Hibernia*"), "{md}");
    }

    /// `simplify_text_elements`: adjacent text nodes with the same annotation
    /// are one run (`<time>` split by an `<abbr>` inside a `<b>`), while a
    /// fragmented anchor still folds into a single link.
    #[test]
    fn adjacent_same_formatting_text_merges_into_one_run() {
        let md = convert(
            "<p><b><time>May 21, 6:29 <abbr>p.m.</abbr></time></b> and <b>x</b><b>y</b> <i>z</i></p>",
        )
        .export_to_markdown();
        // Faithful merge (#997): `<b>x</b><b>y</b>` has no space between.
        assert!(md.contains("**May 21, 6:29 p.m.** and **xy** *z*"), "{md}");
        let md =
            convert("<ul><li><a href=\"#e\"><span>1</span><span>Etymology</span></a></li></ul>")
                .export_to_markdown();
        assert!(md.contains("[1Etymology](#e)"), "{md}");
    }

    /// docling#4287 (2.129): a zero `colspan`/`rowspan` covers no grid slot,
    /// which dropped the cell and shifted the ones after it; it defaults to 1
    /// like a missing or non-numeric span, and a `"2px"` still reads as 2.
    #[test]
    fn zero_spans_default_to_one() {
        let doc = convert(
            "<table><tr><td colspan=\"0\">a</td><td rowspan=\"0\">b</td><td colspan=\"2px\">c</td></tr>\
             <tr><td>d</td><td>e</td><td>f</td><td>g</td></tr></table>",
        );
        let Some(Node::Table(t)) = doc.nodes.iter().find(|n| matches!(n, Node::Table(_))) else {
            panic!("a table");
        };
        assert_eq!(
            t.rows,
            vec![vec!["a", "b", "c", "c"], vec!["d", "e", "f", "g"],]
        );
    }

    /// docling#4216: a `<tr>` whose `<th>`s each span several rows is a pivot
    /// table's row-header row — its cells label the rows they span into, so
    /// they are `row_header`, never `column_header`. Flagging them as column
    /// headers pulled the first data row into the Markdown header block once
    /// docling-core 2.96 started deriving that block from the flags.
    #[test]
    fn pivot_row_headers_are_row_headers_not_column_headers() {
        // The row-header row spans the data rows *plus itself*, exactly as a
        // spreadsheet export writes a pivot table (`example_08`).
        let doc = convert(
            "<table>\
             <tr><th>Year</th><th>Month</th><th>Revenue</th></tr>\
             <tr><th rowspan=3>2025</th></tr>\
             <tr><td>January</td><td>$134</td></tr>\
             <tr><td>February</td><td>$150</td></tr>\
             </table>",
        );
        let Some(Node::Table(t)) = doc.nodes.iter().find(|n| matches!(n, Node::Table(_))) else {
            panic!("a table");
        };
        let st = t.structure.as_ref().expect("structure");
        // Only the real column titles are column headers; the spanning `2025`
        // label is a row header replicated down its span.
        assert_eq!(
            st.col_header,
            vec![vec![true, true, true], vec![false; 3], vec![false; 3]]
        );
        assert_eq!(
            st.row_header,
            vec![
                vec![false; 3],
                vec![true, false, false],
                vec![true, false, false]
            ]
        );
        // …so the header block stays one row and the data rows stay data.
        assert_eq!(t.header_row_count(), 1);
        assert_eq!(
            doc.export_to_markdown(),
            "|   Year | Month    | Revenue   |\n|--------|----------|-----------|\n|   2025 | January  | $134      |\n|   2025 | February | $150      |\n"
        );
    }

    /// A `<th>` label beside `<td>` data in the same row is a row header too
    /// (docling's `(not col_header) and <th>` branch), and contributes no
    /// column header at all.
    #[test]
    fn a_th_label_beside_data_is_a_row_header() {
        let doc = convert(
            "<table><tr><th>Rate</th><td>5%</td></tr><tr><th>Term</th><td>3y</td></tr></table>",
        );
        let Some(Node::Table(t)) = doc.nodes.iter().find(|n| matches!(n, Node::Table(_))) else {
            panic!("a table");
        };
        let st = t.structure.as_ref().expect("structure");
        assert_eq!(st.col_header, vec![vec![false; 2], vec![false; 2]]);
        assert_eq!(st.row_header, vec![vec![true, false], vec![true, false]]);
    }

    /// #371: non-UTF-8 HTML decodes like docling's BeautifulSoup does —
    /// windows-1252 fallback without a declaration, the declared `<meta
    /// charset>` / `http-equiv` charset when present, a BOM first of all; a
    /// declaration nobody knows falls through to UTF-8.
    #[test]
    fn non_utf8_html_decodes_like_beautifulsoup() {
        let md = convert_bytes(b"<html><body><p>caf\xe9 \x93quoted\x94</p></body></html>")
            .export_to_markdown();
        // (the backend's text cleanup maps the curly quotes to ASCII, as docling's does)
        assert!(md.contains("caf\u{e9} \"quoted\""), "{md}");

        let md = convert_bytes(
            b"<html><head><meta charset=\"windows-1251\"></head><body><p>\xcf\xf0\xe8\xe2\xe5\xf2</p></body></html>",
        )
        .export_to_markdown();
        assert!(
            md.contains("\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}"),
            "{md}"
        );

        let md = convert_bytes(
            b"<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=iso-8859-15\"></head><body><p>\xa4 5</p></body></html>",
        )
        .export_to_markdown();
        assert!(md.contains("\u{20ac} 5"), "{md}");

        let mut utf16 = vec![0xff, 0xfe];
        for u in "<p>h\u{e9}</p>".encode_utf16() {
            utf16.extend_from_slice(&u.to_le_bytes());
        }
        let md = convert_bytes(&utf16).export_to_markdown();
        assert!(md.contains("h\u{e9}"), "{md}");

        let md = convert_bytes(
            "<html><head><meta charset=\"x-no-such-charset\"></head><body><p>na\u{ef}ve</p></body></html>"
                .as_bytes(),
        )
        .export_to_markdown();
        assert!(md.contains("na\u{ef}ve"), "{md}");

        assert_eq!(
            declared_encoding(b"<?xml version=\"1.0\" encoding=\"ISO-8859-2\"?><html/>").as_deref(),
            Some("iso-8859-2")
        );
    }

    /// docling#4050: a `<figure>` wrapping a table gets its `<figcaption>` as
    /// the table's caption; one wrapping plain blocks emits the blocks and a
    /// standalone caption item; one with an `<img>` keeps the picture caption.
    #[test]
    fn figures_dispatch_children_and_attach_captions() {
        let doc = convert(
            r#"<html><body>
            <figure><table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>
              <figcaption>Table cap <a href="https://x.y/z">link</a></figcaption></figure>
            <figure><p>Just text</p><figcaption><a href="/w/M">Mallard</a></figcaption></figure>
            <figure><img src="a.png" alt="alt"><figcaption>Img cap</figcaption></figure>
            </body></html>"#,
        );
        let mut tables = 0;
        for n in &doc.nodes {
            match n {
                Node::Table(t) => {
                    tables += 1;
                    assert_eq!(t.caption.as_deref(), Some("Table cap link"));
                    // #390: docling adds the figcaption after the table, under
                    // the table's own parent.
                    assert_eq!(t.caption_parent, CaptionParent::ContainerAfter);
                }
                Node::Picture {
                    caption,
                    caption_parent,
                    ..
                } => {
                    assert_eq!(caption.as_deref(), Some("Img cap"));
                    // An image caption is `add_text`'s default parent, the body.
                    assert_eq!(*caption_parent, CaptionParent::Body);
                }
                _ => {}
            }
        }
        assert_eq!(tables, 1);
        assert!(doc.nodes.iter().any(|n| matches!(
            n,
            Node::Caption { text, href: Some(h) } if text == "Mallard" && h == "/w/M"
        )));
        let md = doc.export_to_markdown();
        assert!(
            md.contains("Table cap link\n\n|   A |   B |"),
            "caption precedes the grid: {md}"
        );
        assert!(
            md.contains("Just text\n\n[Mallard](/w/M)\n\nImg cap\n\n<!-- image -->"),
            "{md}"
        );
    }

    /// #284: an *unclosed* inline tag (here `<a name>` + `<b>`) legally
    /// swallows every subsequent block under HTML5 parsing; the walker must
    /// still emit the swallowed table/list as structure, not flatten them
    /// into inline text.
    #[test]
    fn unclosed_inline_tag_does_not_swallow_tables() {
        let doc = convert(
            r#"<html><body><a name="x"><b>T</b><br>
               <table><tr><td>A</td><td>B</td></tr><tr><td>1</td><td>2</td></tr></table></body></html>"#,
        );
        let tables = doc
            .nodes
            .iter()
            .filter(|n| matches!(n, Node::Table(_)))
            .count();
        assert_eq!(tables, 1, "the swallowed table must surface as a table");
        let md = doc.export_to_markdown();
        assert!(
            md.contains("**T**"),
            "the <b> text keeps its formatting: {md}"
        );
        assert!(md.contains("|-----"), "table structure survives: {md}");
    }

    /// The counterpart guard: a *well-formed* anchor around a pure text
    /// container keeps rendering as a link (hyperlink_04 semantics) — only
    /// structured blocks (tables, lists, headings…) trigger the block walk.
    #[test]
    fn anchor_around_a_div_stays_a_link() {
        let doc =
            convert(r#"<html><body><a href="/start.html"><div>Some text.</div></a></body></html>"#);
        let md = doc.export_to_markdown();
        assert!(
            md.contains("[Some text.](/start.html)"),
            "text-only block content stays inside the link: {md}"
        );
    }

    #[test]
    fn deeply_nested_html_does_not_overflow_the_stack() {
        // ~50k nested <div> would blow the recursive walker's stack (an
        // uncatchable abort). The depth guard must fall back to flattened text
        // instead of recursing. Uses the env override to keep the test cheap.
        std::env::set_var("DOCLING_RS_MAX_HTML_DEPTH", "200");
        let depth = 4_000;
        let html = format!(
            "<html><body>{}<p>deep text</p>{}</body></html>",
            "<div>".repeat(depth),
            "</div>".repeat(depth),
        );
        let doc = convert(&html); // must return, not abort
        std::env::remove_var("DOCLING_RS_MAX_HTML_DEPTH");
        assert!(
            doc.export_to_markdown().contains("deep text"),
            "flattened fallback should preserve the text content"
        );
        // #912: the flattening is named — the variable, the effective cap,
        // what the document lacks — instead of an unexplained success.
        assert!(doc.tree.is_none());
        assert_eq!(doc.notices.len(), 1, "{:?}", doc.notices);
        assert_eq!(
            doc.notices[0].kind,
            docling_core::NoticeKind::DepthLimit {
                variable: "DOCLING_RS_MAX_HTML_DEPTH",
                limit: 200
            }
        );
        assert!(
            doc.notices[0].message.contains("DOCLING_RS_MAX_HTML_DEPTH")
                && doc.notices[0].message.contains("200")
                && doc.notices[0].message.contains("source blocks"),
            "{}",
            doc.notices[0].message
        );
        // Within the cap: a tree, no notice.
        let doc = convert("<html><body><div><p>shallow</p></div></body></html>");
        assert!(doc.tree.is_some() && doc.notices.is_empty());
    }

    #[test]
    fn shallow_html_still_walks_structurally() {
        // A normal document stays under the limit and produces real structure,
        // not the flattened fallback.
        let doc = convert("<h1>Title</h1><ul><li>a</li><li>b</li></ul>");
        let md = doc.export_to_markdown();
        assert!(md.contains("# Title"));
        assert!(md.contains("- a"));
    }

    #[test]
    fn headings_paragraphs_and_inline_formatting() {
        let doc = convert(
            "<h1>Title</h1><p>Hello <strong>bold</strong> and <em>italic</em> and \
             <a href=\"https://x.com\">link</a>.</p>",
        );
        assert_eq!(
            doc.export_to_markdown(),
            "# Title\n\nHello **bold** and *italic* and [link](https://x.com/).\n"
        );
    }

    #[test]
    fn nested_lists() {
        let doc = convert("<ul><li>one<ul><li>one-a</li></ul></li><li>two</li></ul>");
        assert_eq!(doc.export_to_markdown(), "- one\n    - one-a\n- two\n");
    }

    #[test]
    fn inline_images_produce_no_marker_but_anchor_wrapped_images_stay_pictures() {
        // An image inside text emits nothing (docling never renders inline image
        // markers); the surrounding text is unaffected.
        let inline = convert("<p>before <img src=\"x.png\" alt=\"logo\"> after</p>");
        assert_eq!(inline.export_to_markdown(), "before after\n");
        // A non-anchor wrapper around a lone image (`<span><img></span>`) is inline
        // too, so it is dropped entirely.
        let span = convert("<span><img src=\"x.png\" alt=\"logo\"></span><h2>Home</h2>");
        assert_eq!(span.export_to_markdown(), "## Home\n");
        // But an anchor wrapping only an image becomes a Picture (docling keeps
        // `<a><img></a>` as a linked image).
        let anchor = convert("<a href=\"/l\"><img src=\"x.png\" alt=\"cap\"></a>");
        assert_eq!(anchor.export_to_markdown(), "cap\n\n<!-- image -->\n");
    }

    #[test]
    fn anchor_wrapping_several_images_hangs_its_href_on_each_caption() {
        // Wikipedia's logo link holds a decorative icon plus a captioned
        // wordmark and tagline; docling drops the aria-hidden icon and hangs
        // the anchor's href on the remaining captions.
        let doc = convert(
            "<a href=\"/wiki/Main_Page\">\
               <img src=\"icon.png\" alt=\"\" aria-hidden=\"true\">\
               <img src=\"wordmark.svg\" alt=\"Wikipedia\">\
               <img src=\"tagline.svg\" alt=\"The Free Encyclopedia\">\
             </a>",
        );
        let captions: Vec<(Option<String>, Option<String>)> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Picture {
                    caption,
                    caption_href,
                    ..
                } => Some((caption.clone(), caption_href.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            captions,
            vec![
                (
                    Some("Wikipedia".to_string()),
                    Some("/wiki/Main_Page".to_string())
                ),
                (
                    Some("The Free Encyclopedia".to_string()),
                    Some("/wiki/Main_Page".to_string())
                ),
            ]
        );
    }

    #[test]
    fn hidden_elements_are_suppressed() {
        // display:none / visibility:hidden / the `hidden` attribute are not
        // rendered, so their text is dropped.
        let hidden = convert(
            "<p>keep</p>\
             <p style=\"display:none\">gone</p>\
             <p style=\"visibility: hidden\">gone2</p>\
             <p hidden>gone3</p>",
        );
        assert_eq!(hidden.export_to_markdown(), "keep\n");
        // docling's `_is_invisible_tag` also drops aria-hidden subtrees — the
        // decorative-duplicate case (Wikipedia's logo icon beside its wordmark).
        let aria = convert(
            "<p aria-hidden=\"true\">gone</p>\
             <p aria-hidden=\"1\">gone2</p>\
             <p aria-hidden=\"yes\">gone3</p>\
             <p aria-hidden=\"false\">keep</p>",
        );
        assert_eq!(aria.export_to_markdown(), "keep\n");
    }

    #[test]
    fn nested_table_flattens_with_docling_spacing() {
        // A table nested in a cell flattens to its own grid joined with single
        // spaces; a deeper table inside one of those cells contributes its
        // subtree text with every whitespace run collapsed (docling#3973 —
        // the source line break between the innermost table's rows used to
        // survive as `a  b`, td-trailing space + newline).
        let doc = convert(
            "<table><tr><td><table><tr><td>P</td><td>Q</td></tr>\n\
             <tr><td>R</td><td><table><tr><td>a</td></tr>\n\
             <tr><td>b</td></tr></table></td></tr></table></td><td>Z</td></tr></table>",
        );
        let table = doc
            .nodes
            .iter()
            .find_map(|n| match n {
                Node::Table(t) => Some(t),
                _ => None,
            })
            .expect("outer table parsed");
        assert_eq!(table.rows[0][0], "P Q R a b");
        assert_eq!(table.rows[0][1], "Z");
        assert!(
            doc.export_to_markdown().contains("P Q R a b"),
            "single-spaced in markdown: {}",
            doc.export_to_markdown()
        );
    }

    /// #328: an `<a href>` wrapping an image (or a link over a whole
    /// figcaption) rides as the caption's hyperlink annotation; the caption
    /// text and the Markdown stay plain, and a bare authority is normalized
    /// like AnyUrl. A link on part of a figcaption is none (#770).
    #[test]
    fn caption_hyperlinks_are_annotated() {
        let doc = convert(
            "<a href=\"https://www.example.com\"><img src=\"x.png\" alt=\"Clickable\"></a>             <figure><img src=\"y.png\" alt=\"Cap\">               <figcaption>An example <a href=\"#caption\">caption</a> here.</figcaption>             </figure>             <figure><img src=\"z.png\" alt=\"Cap\">               <figcaption><a href=\"#whole\">Linked caption</a></figcaption>             </figure>",
        );
        let hrefs: Vec<_> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Picture {
                    caption,
                    caption_href,
                    ..
                } => Some((caption.clone(), caption_href.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            hrefs,
            vec![
                (
                    Some("Clickable".into()),
                    Some("https://www.example.com/".into())
                ),
                (Some("An example caption here.".into()), None),
                (Some("Linked caption".into()), Some("#whole".into())),
            ]
        );
        assert_eq!(
            doc.export_to_markdown(),
            "Clickable\n\n<!-- image -->\n\nAn example caption here.\n\n<!-- image -->\n\nLinked caption\n\n<!-- image -->\n"
        );
    }

    /// #328: a rich cell carries its structured blocks in `cell_blocks` —
    /// a list stays list items, a heading stays a heading (Markdown text
    /// unchanged: the flat `rows` grid still holds the flattened rendering).
    #[test]
    fn rich_cells_carry_cell_blocks() {
        let doc = convert(
            "<table>               <tr><td>plain</td><td><ul><li>a</li><li>b</li></ul></td></tr>               <tr><td><h2>A</h2><p>text</p></td><td>x</td></tr>             </table>",
        );
        let table = doc
            .nodes
            .iter()
            .find_map(|n| match n {
                Node::Table(t) => Some(t),
                _ => None,
            })
            .expect("a table");
        assert_eq!(table.rows[0][1], "- a\n- b");
        assert_eq!(table.rows[1][0], "A\n\ntext");
        let blocks = table.cell_blocks.as_ref().expect("rich cells present");
        assert!(blocks[0][0].is_empty(), "plain cell stays block-less");
        assert!(
            matches!(
                blocks[0][1].as_slice(),
                [Node::ListItem { .. }, Node::ListItem { .. }]
            ),
            "list cell keeps its items: {:?}",
            blocks[0][1]
        );
        assert!(
            matches!(
                blocks[1][0].as_slice(),
                [Node::Heading { level: 2, .. }, Node::Paragraph { .. }]
            ),
            "heading cell keeps its structure: {:?}",
            blocks[1][0]
        );
        // A table with no rich cell keeps `cell_blocks: None` (docx/odf rule).
        let doc = convert("<table><tr><td>a</td><td>b</td></tr></table>");
        let table = doc
            .nodes
            .iter()
            .find_map(|n| match n {
                Node::Table(t) => Some(t),
                _ => None,
            })
            .unwrap();
        assert!(table.cell_blocks.is_none());
    }

    #[test]
    fn form_region_becomes_key_value_fields() {
        // A `form_region` container with the `keyN` / `keyN_marker` / `keyN_valueM`
        // id-convention is a docling field region: the region and each item
        // carry no text of their own (docling-core#724 dropped their former
        // `<!-- missing-text -->` markers); only the item's marker/key/value
        // texts render.
        let doc = convert(
            "<div class=\"form_region\">\
               <div class=\"field\">\
                 <div id=\"key1_marker\">1</div>\
                 <span id=\"key1\">Restaurant</span>\
                 <span id=\"key1_value1\">Docling</span>\
               </div>\
               <div class=\"field\">\
                 <div id=\"key2_marker\">2</div>\
                 <span id=\"key2\">Telephone</span>\
                 <span id=\"key2_value1\">123</span>\
               </div>\
             </div>",
        );
        assert_eq!(
            doc.export_to_markdown(),
            "1\n\nRestaurant\n\nDocling\n\n2\n\nTelephone\n\n123\n",
        );
        // A plain container without the id-convention stays ordinary text.
        let plain = convert("<div class=\"form_region\"><p>just text</p></div>");
        assert_eq!(plain.export_to_markdown(), "just text\n");
    }

    #[test]
    fn ordered_list_is_numbered_sequentially() {
        let doc = convert("<ol><li>first</li><li>second</li></ol>");
        assert_eq!(doc.export_to_markdown(), "1. first\n2. second\n");
    }

    /// docling#4440: a non-`<li>` child of a list closes it, the items after
    /// it open a new list that continues the numbering, and a list directly
    /// inside a list is the preceding item's sub-list — none of it dropped.
    #[test]
    fn non_li_list_children_are_kept_in_order() {
        let doc = convert("<ol><li>a</li><p>between</p><li>b</li><ol><li>c</li></ol></ol>");
        assert_eq!(
            doc.export_to_markdown(),
            "1. a\n\nbetween\n\n2. b\n    1. c\n"
        );
        let doc = convert("<ul><li>a</li><ul><li>b</li></ul><li>c</li></ul>");
        assert_eq!(doc.export_to_markdown(), "- a\n    - b\n- c\n");
    }

    /// docling#4617: a `<li>` without text keeps its table under an empty
    /// item, and the table keeps its grid instead of folding into the text.
    #[test]
    fn list_item_table_stays_a_table() {
        let doc =
            convert("<ol><li><table><tr><td>A</td><td>B</td></tr></table></li><li>c</li></ol>");
        let md = doc.export_to_markdown();
        assert!(md.starts_with("1. \n\n|"), "{md}");
        assert!(md.ends_with("\n\n2. c\n"), "{md}");
    }

    /// docling#4669: a `<li>`'s string `list-style-type` is its marker.
    #[test]
    fn list_style_type_string_is_the_marker() {
        let doc = convert(
            r#"<ol><li style="list-style-type: '3. ';">c</li><li style="color: red; list-style-type: 'iv) '">d</li></ol>"#,
        );
        assert_eq!(doc.export_to_markdown(), "3. c\n- iv) d\n");
    }

    /// docling's `table_width` (docling#4414, #4527).
    #[test]
    fn table_width_follows_rowspan_shifts_and_collapses_empty_columns() {
        // "North" covers column 0 of all three rows, pushing Q1/10 right.
        assert_eq!(
            table_width(&[vec![(1, 3)], vec![(1, 1), (1, 1)], vec![(1, 1), (1, 1)]]),
            3
        );
        // No cell starts right of column 0: the colspan's columns collapse.
        assert_eq!(table_width(&[vec![(3, 1)]]), 1);
        // An oversized colspan is cut to the columns cells start in.
        assert_eq!(table_width(&[vec![(1000, 1)], vec![(1, 1), (1, 1)]]), 2);
        let doc = convert(
            r#"<table><tr><td rowspan="3">North</td></tr><tr><td>Q1</td><td>10</td></tr><tr><td>Q2</td><td>20</td></tr></table>"#,
        );
        let md = doc.export_to_markdown();
        assert!(md.contains("10") && md.contains("20"), "{md}");
    }

    #[test]
    fn block_image_becomes_picture() {
        let doc = convert("<img src=\"x.png\" alt=\"A cat\"/>");
        assert_eq!(doc.export_to_markdown(), "A cat\n\n<!-- image -->\n");
    }

    #[test]
    fn table_with_header() {
        let doc = convert(
            "<table><thead><tr><th>Name</th><th>Age</th></tr></thead>\
             <tbody><tr><td>Ada</td><td>36</td></tr></tbody></table>",
        );
        assert_eq!(
            doc.export_to_markdown(),
            "| Name   |   Age |\n|--------|-------|\n| Ada    |    36 |\n"
        );
    }

    #[test]
    fn code_block_with_language() {
        let doc = convert("<pre><code class=\"language-rust\">let x = 1;</code></pre>");
        assert_eq!(
            doc.nodes,
            vec![Node::Code {
                language: Some("rust".into()),
                text: "let x = 1;".into(),
                orig: None,
                pretty: None,
            }]
        );
    }

    #[test]
    fn skips_script_and_style() {
        let doc = convert("<style>.a{}</style><p>visible</p><script>x()</script>");
        assert_eq!(doc.export_to_markdown(), "visible\n");
    }

    /// Encode a small distinctly-sized PNG so dimensions are easy to assert.
    fn tiny_png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([1, 2, 3]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    #[test]
    fn image_is_placeholder_by_default_but_extracted_with_a_resolver() {
        use crate::backend::images::FsImageResolver;
        use docling_core::base64::encode;

        let uri = format!("data:image/png;base64,{}", encode(&tiny_png(2, 3)));
        let html = format!("<img src=\"{uri}\" alt=\"k\"/>");

        // The bare backend leaves every image a placeholder (default behaviour).
        let plain = convert(&html);
        assert!(matches!(plain.nodes[0], Node::Picture { image: None, .. }));

        // With a resolver the data: URI is decoded and embedded.
        let doc = convert_html(
            "t",
            &html,
            &FsImageResolver::new(
                None,
                None,
                crate::backend::images::ImagePolicy::new(crate::ImageSources::Embedded),
            ),
        );
        match &doc.nodes[0] {
            Node::Picture {
                image: Some(img),
                caption,
                ..
            } => {
                assert_eq!(caption.as_deref(), Some("k"));
                assert_eq!(img.mimetype, "image/png");
                assert_eq!((img.width, img.height), (2, 3));
            }
            other => panic!("expected an embedded image, got {other:?}"),
        }
    }

    #[test]
    fn lazy_loaded_img_src_falls_back_to_data_src_and_srcset() {
        use super::img_src;
        use scraper::Html;
        let pick = |html: &str| {
            let frag = Html::parse_fragment(html);
            let img = frag.select(cached_selector!("img")).next().unwrap();
            img_src(img.value())
        };
        // A real src wins.
        assert_eq!(pick(r#"<img src="/a.png">"#).as_deref(), Some("/a.png"));
        // No src → data-src (the magenta.at lazy-load shape).
        assert_eq!(
            pick(r#"<img data-src="/b.png" class="lazyload">"#).as_deref(),
            Some("/b.png")
        );
        // A placeholder data: src with a lazy target → the lazy target.
        assert_eq!(
            pick(r#"<img src="data:image/gif;base64,R0lGOD" data-src="/c.png">"#).as_deref(),
            Some("/c.png")
        );
        // srcset → first candidate URL, descriptor stripped.
        assert_eq!(
            pick(r#"<img srcset="/d-1x.png 1x, /d-2x.png 2x">"#).as_deref(),
            Some("/d-1x.png")
        );
    }
}
