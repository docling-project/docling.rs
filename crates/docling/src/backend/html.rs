//! HTML backend.
//!
//! Parses HTML with `scraper` (html5ever — the same HTML5 tree-construction
//! algorithm browsers use) and walks the DOM into a [`DoclingDocument`]. This
//! is the Rust counterpart of `docling/backend/html_backend.py`'s `_walk`.
//!
//! Scope (Phase 2): block structure (headings, paragraphs, nested lists,
//! tables, code blocks, figures/images), inline formatting (bold, italic,
//! inline code, links), key-value form regions (docling's `field_region`,
//! detected from the `keyN` / `keyN_valueM` / `keyN_marker` `id`-convention),
//! and inline visibility suppression (`hidden` / inline `display:none` /
//! `visibility:hidden`). Out of scope for now and tracked in `MIGRATION.md`:
//! browser rendering, rendered bounding boxes, stylesheet-driven (class/CSS
//! cascade) visibility suppression, and the rich per-cell table provenance the
//! Python backend computes.

use docling_core::{DoclingDocument, InlineRun, Node, Script, Table};
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
        Ok(convert_html(&source.name, source.text()?, &NoFetch))
    }
}

/// Convert an HTML document into a [`DoclingDocument`], resolving `<img>` sources
/// through `images` (use [`NoFetch`] to leave every picture a placeholder).
pub(crate) fn convert_html(name: &str, html: &str, images: &dyn ImageResolver) -> DoclingDocument {
    let mut doc = DoclingDocument::new(name);
    append_fragment(html, &mut doc.nodes, images);
    doc
}

/// Parse an HTML fragment and append its block nodes to `out`. Shared with the
/// Markdown backend, which feeds embedded raw-HTML blocks through here (as
/// docling does).
pub(crate) fn append_fragment(html: &str, out: &mut Vec<Node>, images: &dyn ImageResolver) {
    let parsed = Html::parse_document(html);
    // The document `<title>` is docling's furniture-layer title heading — it
    // precedes the body content and is excluded from Markdown/JSON. Fragments
    // (e.g. Markdown-embedded HTML) carry no `<title>`, so none is added.
    if let Some(title) = parsed.select(cached_selector!("title")).next() {
        let text = normalize_ws(&title.text().collect::<String>());
        if !text.is_empty() {
            out.push(Node::Furniture(Box::new(Node::Heading { level: 1, text })));
        }
    }

    // Prefer <body>; fall back to the root element for fragments.
    let body = parsed.select(cached_selector!("body")).next();
    let root = body.unwrap_or_else(|| parsed.root_element());
    walk_block(root, out, 0, Fmt::default(), images);
}

/// An element the page explicitly hides from rendering — the `hidden` attribute
/// or an inline `display:none` / `visibility:hidden` style. A rendering engine
/// (and docling's rendered output) drops these, so we suppress them too.
///
/// `aria-hidden="true"` is deliberately *not* treated as hidden: it removes an
/// element from the accessibility tree but leaves it visually rendered, so a
/// visual renderer keeps its text. Only inline styles are honored — a full CSS
/// cascade (class/stylesheet-driven visibility, e.g. Wikipedia's collapsed
/// menus) still needs a real browser and is out of scope.
fn is_hidden(e: &scraper::node::Element) -> bool {
    if e.attr("hidden").is_some() {
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
fn is_skipped(name: &str) -> bool {
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
            | "body"
            | "html"
    )
}

/// Walk the block-level children of `elem`, emitting [`Node`]s. Inline content
/// found directly between block elements is buffered and flushed as paragraphs.
/// `base` seeds the inline formatting — table cells pass `raw` so their text is
/// not `&<>`/`_` escaped.
fn walk_block(
    elem: ElementRef,
    nodes: &mut Vec<Node>,
    list_level: u8,
    base: Fmt,
    images: &dyn ImageResolver,
) {
    let mut inline = RunBuf::default();

    for child in elem.children() {
        match child.value() {
            HtmlNode::Text(text) => {
                let run = normalize_ws(text);
                if !run.is_empty() {
                    inline.md.push(serialize_run(&run, base, None));
                    inline.push_rich(base.to_inline_run(&run));
                }
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
                        image: e.attr("src").and_then(|s| images.resolve(s)),
                    });
                } else if name == "signature" || name == "stamp" {
                    // docling turns these into an image annotated with the kind.
                    flush_inline(&mut inline, nodes);
                    nodes.push(Node::Picture {
                        caption: None,
                        image: None,
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
                        // wrapper. A non-anchor inline wrapper (`<span><img></span>`)
                        // is left inline instead — docling never emits inline image
                        // markers, so such an image produces no output.
                        flush_inline(&mut inline, nodes);
                        nodes.push(Node::Picture {
                            caption,
                            image: src.as_deref().and_then(|s| images.resolve(s)),
                        });
                    } else {
                        collect_element(cref, base, None, &mut inline);
                    }
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
        push_inline_paragraph(nodes, finalize(&buf.md), std::mem::take(&mut buf.rich));
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
            let text = render_inline_fmt(elem, base);
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
                });
            } else {
                let (text, runs) = render_inline(elem, base);
                push_inline_paragraph(nodes, text, runs);
            }
        }
        "ul" | "ol" => walk_list(elem, name == "ol", nodes, list_level, base),
        "dl" => walk_dl(elem, nodes, list_level, base),
        "pre" => {
            // A <pre> with inline structure (links/formatting) renders each
            // segment as inline code; a plain <pre> is a code block.
            let mut runs = RunBuf::default();
            collect_runs(elem, Fmt { code: true, ..base }, None, &mut runs);
            if runs.md.len() > 1 {
                nodes.push(Node::Paragraph {
                    text: runs.md.join(" "),
                });
            } else {
                let (language, text) = extract_pre(elem);
                nodes.push(Node::Code { language, text });
            }
        }
        "table" => {
            if base.raw {
                // A table nested in a cell is flattened to its grid cells joined
                // with spaces (docling's `_collect_subtree_text`). Each grid
                // cell's text is docling's raw `get_text` (source line breaks
                // preserved as newlines), so a deeper nested table's structure
                // survives as `\n` runs that the table serializer later flattens
                // to spaces — reproducing docling's spacing byte-for-byte.
                let text = flatten_nested_table(elem);
                if !text.is_empty() {
                    nodes.push(Node::Paragraph { text });
                }
            } else if let Some(table) = parse_table(elem) {
                nodes.push(Node::Table(table));
            }
        }
        "figure" => nodes.push(Node::Picture {
            caption: figure_caption(elem),
            image: figure_img_src(elem).and_then(|s| images.resolve(&s)),
        }),
        "hr" => {}
        // A `form_region`-classed container holding `keyN`-convention fields is a
        // docling key-value region; emit it as one instead of recursing (so the
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
fn detect_field_region(elem: ElementRef) -> Option<Vec<docling_core::FieldItem>> {
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
            KvpKind::Value => field.value.get_or_insert(text),
        };
    }
    if fields.is_empty() {
        return None;
    }
    Some(fields.into_values().collect())
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

/// Emit one `ListItem` per `<li>`, recursing into nested `<ul>`/`<ol>` at a
/// deeper level. Ordered items are numbered from the list's `start` attribute.
fn walk_list(list: ElementRef, ordered: bool, nodes: &mut Vec<Node>, level: u8, base: Fmt) {
    let mut number = list
        .value()
        .attr("start")
        .and_then(|s| s.trim().parse().ok())
        .filter(|_| ordered)
        .unwrap_or(1);
    for child in list.children() {
        let Some(li) = ElementRef::wrap(child) else {
            continue;
        };
        if li.value().name() != "li" {
            continue;
        }

        // The item's own inline text, then its block content. Images fold into
        // the item text (so the list stays tight); nested lists follow as
        // adjacent items in the same run.
        let mut runs = RunBuf::default();
        collect_li_inline(li, base, &mut runs);
        let mut text = finalize(&runs.md);
        let mut nested: Vec<(&str, ElementRef)> = Vec::new();
        append_li_blocks(li, &mut text, &mut nested);
        if !text.is_empty() {
            nodes.push(Node::ListItem {
                ordered,
                number,
                // HTML sibling lists separate only on a kind flip / ordered
                // restart, both handled by the serializer's heuristic.
                first_in_list: false,
                text,
                level,
            });
        }
        number += 1;
        for (kind, el) in nested {
            match kind {
                "ol" => walk_list(el, true, nodes, level + 1, base),
                "dl" => walk_dl(el, nodes, level, base),
                _ => walk_list(el, false, nodes, level + 1, base),
            }
        }
    }
}

/// Collect a list item's own inline text. Images and nested lists are pulled out
/// as blocks (handled by `walk_li_blocks`); `<p>`/`<div>` wrappers are folded in.
fn collect_li_inline(li: ElementRef, base: Fmt, runs: &mut RunBuf) {
    for child in li.children() {
        match child.value() {
            HtmlNode::Text(t) => {
                let run = normalize_ws(t);
                if !run.is_empty() {
                    runs.md.push(serialize_run(&run, base, None));
                    runs.push_rich(base.to_inline_run(&run));
                }
            }
            HtmlNode::Element(e) => {
                let Some(cref) = ElementRef::wrap(child) else {
                    continue;
                };
                if is_hidden(e) {
                    continue;
                }
                match e.name() {
                    "ul" | "ol" | "dl" | "img" => {} // pulled out as blocks
                    "p" | "div" | "section" | "article" | "blockquote" => {
                        collect_li_inline(cref, base, runs)
                    }
                    _ => collect_element(cref, base, None, runs),
                }
            }
            _ => {}
        }
    }
}

/// Append a `<li>`'s block content: an image folds into the item text as a
/// `<!-- image -->` marker (so the list stays tight); nested lists are collected
/// for emission as adjacent items. Recurses through `<div>` wrappers.
fn append_li_blocks<'a>(
    elem: ElementRef<'a>,
    text: &mut String,
    nested: &mut Vec<(&'a str, ElementRef<'a>)>,
) {
    for child in elem.children().filter_map(ElementRef::wrap) {
        let e = child.value();
        if is_hidden(e) {
            continue;
        }
        match e.name() {
            "img" => {
                text.push('\n');
                if let Some(alt) = e.attr("alt").filter(|a| !a.is_empty()) {
                    text.push_str(&normalize_ws(alt));
                    text.push('\n');
                }
                text.push_str("<!-- image -->");
            }
            "ul" => nested.push(("ul", child)),
            "ol" => nested.push(("ol", child)),
            "dl" => nested.push(("dl", child)),
            "p" | "div" | "section" | "blockquote" => append_li_blocks(child, text, nested),
            _ => {}
        }
    }
}

/// A description list: each `<dt>` is a bold list item, each `<dd>` an item one
/// level deeper, recursing into nested `<dl>`/`<ul>`/`<ol>` (docling's rendering).
fn walk_dl(dl: ElementRef, nodes: &mut Vec<Node>, level: u8, base: Fmt) {
    let bold = Fmt { bold: true, ..base };
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
                        // docling does not blank-separate description lists.
                        first_in_list: false,
                        text,
                        level,
                    });
                }
            }
            "dd" => walk_dd(c, nodes, level + 1, base),
            _ => {}
        }
    }
}

/// A `<dd>`: its own inline text becomes an item at `level`, and any nested
/// `<dl>`/`<ul>`/`<ol>` is walked at the same level.
fn walk_dd(dd: ElementRef, nodes: &mut Vec<Node>, level: u8, base: Fmt) {
    let mut runs = RunBuf::default();
    let mut nested: Vec<(&str, ElementRef)> = Vec::new();
    for child in dd.children() {
        match child.value() {
            HtmlNode::Text(t) => {
                let run = normalize_ws(t);
                if !run.is_empty() {
                    runs.md.push(serialize_run(&run, base, None));
                    runs.push_rich(base.to_inline_run(&run));
                }
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
    let text = finalize(&runs.md);
    if !text.is_empty() {
        nodes.push(Node::ListItem {
            ordered: false,
            number: 1,
            first_in_list: false,
            text,
            level,
        });
    }
    for (kind, el) in nested {
        match kind {
            "dl" => walk_dl(el, nodes, level, base),
            "ol" => walk_list(el, true, nodes, level, base),
            _ => walk_list(el, false, nodes, level, base),
        }
    }
}

/// Active inline formatting, accumulated from ancestor tags (mirrors docling's
/// `_FORMAT_TAG_MAP`). `underline` and `script` (sub/superscript) carry no
/// Markdown marker — they only surface in DocLang, via the structured runs.
/// `raw` suppresses `&<>`/`_` escaping — docling escapes body text but not
/// table-cell text.
#[derive(Clone, Copy, Default)]
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
        }
    }
}

/// Parallel accumulator for a paragraph's inline content: the Markdown-marker
/// strings (`md`, joined/finalized for Markdown/JSON, unchanged) and the
/// structured runs (`rich`, one per text segment) that drive DocLang.
#[derive(Default)]
struct RunBuf {
    md: Vec<String>,
    rich: Vec<InlineRun>,
    /// A `<br>` was just seen: the next same-formatting text segment folds into
    /// the previous run with a newline (docling keeps `a<br>b` as one text item
    /// `"a\nb"`, not two runs).
    merge_next: bool,
}

impl RunBuf {
    /// Append a text segment as a structured run, folding it into the previous
    /// run across a pending `<br>` when the formatting matches.
    fn push_rich(&mut self, run: InlineRun) {
        if self.merge_next {
            self.merge_next = false;
            if let Some(last) = self.rich.last_mut() {
                if same_style(last, &run) {
                    last.text.push('\n');
                    last.text.push_str(&run.text);
                    return;
                }
            }
        }
        self.rich.push(run);
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
    finalize(&runs.md)
}

/// Like [`render_inline_fmt`] but also returns the structured runs, for the
/// paragraph path that emits an `InlineGroup`.
fn render_inline(elem: ElementRef, base: Fmt) -> (String, Vec<InlineRun>) {
    let mut runs = RunBuf::default();
    collect_runs(elem, base, None, &mut runs);
    (finalize(&runs.md), runs.rich)
}

/// docling represents a `<br>` with a sentinel that the serializer rewrites.
const BR_SENTINEL: &str = "\u{e000}";

/// Join runs with single spaces, then turn `<br>` sentinels into newlines,
/// stripping the spaces the join inserted around them.
fn finalize(runs: &[String]) -> String {
    let joined = runs.join(" ");
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
                let normalized = normalize_ws(text);
                if !normalized.is_empty() {
                    runs.md.push(serialize_run(&normalized, fmt, hyperlink));
                    runs.push_rich(fmt.to_inline_run(&normalized));
                }
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
            runs.md.push(BR_SENTINEL.to_string());
            runs.merge_next = true;
        }
        "a" => {
            let href = e.attr("href").map(normalize_url);
            collect_runs(elem, fmt, href.as_deref().or(hyperlink), runs);
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
fn normalize_url(href: &str) -> String {
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
        format!("`{text}`")
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
        res = format!("[{res}]({href})");
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
    let text = pre.text().collect::<String>();
    (language, text.trim_matches('\n').to_string())
}

/// Extract a language hint from a `class` like `language-rust` or `lang-rust`.
fn lang_from_class(class: &str) -> Option<String> {
    class.split_whitespace().find_map(|c| {
        c.strip_prefix("language-")
            .or_else(|| c.strip_prefix("lang-"))
            .map(str::to_string)
    })
}

fn parse_table(table: ElementRef) -> Option<Table> {
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
        out.trim().to_string()
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
fn subtree_text(elem: ElementRef, out: &mut String) {
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

fn parse_table_cells(
    table: ElementRef,
    render_cell: impl Fn(ElementRef) -> String,
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
    let (mut num_rows, mut num_cols) = (0usize, 0usize);
    for tr in &trs {
        let cells = row_cells(*tr);
        let col_count: usize = cells.iter().map(|c| span_attr(*c, "colspan")).sum();
        num_cols = num_cols.max(col_count);
        if !is_row_header(&cells) {
            num_rows += 1;
        }
    }
    if num_rows == 0 || num_cols == 0 {
        return None;
    }

    let mut grid: Vec<Vec<Option<String>>> = vec![vec![None; num_cols]; num_rows];
    let mut row_idx: isize = -1;
    let mut start_row_span: usize = 0;
    for tr in &trs {
        let cells = row_cells(*tr);
        let row_header = is_row_header(&cells);
        if row_header {
            start_row_span += 1;
        } else {
            row_idx += 1;
            start_row_span = 0;
        }
        let base = (row_idx + start_row_span as isize).max(0) as usize;

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
            let text = render_cell(cell);
            for r in start_row_span..start_row_span + rowspan {
                let gr = (row_idx + r as isize).max(0) as usize;
                for dc in 0..colspan {
                    let gc = col + dc;
                    if gr < num_rows && gc < num_cols {
                        grid[gr][gc] = Some(text.clone());
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
    (!rows.is_empty()).then_some(Table { rows })
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

fn span_attr(cell: ElementRef, name: &str) -> usize {
    cell.value()
        .attr(name)
        .and_then(|v| v.trim().parse().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(1)
}

/// Render a table cell to Markdown. docling treats a cell as "rich" (and
/// serializes its full block content — headings, paragraphs, lists, code) only
/// when it carries structure; a single plain run is emitted as plain text.
/// Either way inline text is left unescaped; the table serializer flattens
/// newlines to spaces.
fn render_cell(cell: ElementRef) -> String {
    let raw = Fmt {
        raw: true,
        ..Fmt::default()
    };
    if is_rich_cell(cell) {
        let mut nodes: Vec<Node> = Vec::new();
        // Images inside table cells stay placeholders (they fold into cell text).
        walk_block(cell, &mut nodes, 0, raw, &NoFetch);
        let mut doc = DoclingDocument::new("");
        doc.nodes = nodes;
        doc.export_to_markdown().trim().to_string()
    } else {
        render_inline_fmt(cell, raw)
    }
}

/// docling's `_is_rich_table_cell`: a cell is rich if it has a `<br>`, more than
/// one direct `<p>/<div>/<li>`, more than one text run, a single run carrying
/// formatting/link/code, or only an image/input.
fn is_rich_cell(cell: ElementRef) -> bool {
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
    let src = img.value().attr("src").map(str::to_string);
    Some((caption, src))
}

/// The `src` of a `<figure>`'s first `<img>`, for image extraction.
fn figure_img_src(fig: ElementRef) -> Option<String> {
    fig.select(cached_selector!("img"))
        .next()
        .and_then(|img| img.value().attr("src"))
        .map(str::to_string)
}

fn has_descendant(elem: ElementRef, name: &str) -> bool {
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
                    let marks = matches!(
                        e.name(),
                        "b" | "strong"
                            | "i"
                            | "em"
                            | "var"
                            | "s"
                            | "del"
                            | "strike"
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

fn figure_caption(fig: ElementRef) -> Option<String> {
    if let Some(cap) = fig.select(cached_selector!("figcaption")).next() {
        // A figure caption is plain text (formatting/links are stripped).
        let text = normalize_ws(&cap.text().collect::<String>());
        if !text.is_empty() {
            return Some(text);
        }
    }
    fig.select(cached_selector!("img"))
        .next()
        .and_then(|img| img.value().attr("alt"))
        .filter(|a| !a.is_empty())
        .map(str::to_string)
}

/// Sanitize typographic Unicode to ASCII (docling's HTML text cleanup) and
/// collapse all runs of whitespace to single spaces, trimming the ends — in a
/// single pass (this runs once per text run, so it stays allocation-light).
fn normalize_ws(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    fn convert(html: &str) -> DoclingDocument {
        let src = SourceDocument::from_bytes("t", InputFormat::Html, html.as_bytes().to_vec());
        HtmlBackend.convert(&src).unwrap()
    }

    #[test]
    fn headings_paragraphs_and_inline_formatting() {
        let doc = convert(
            "<h1>Title</h1><p>Hello <strong>bold</strong> and <em>italic</em> and \
             <a href=\"https://x.com\">link</a>.</p>",
        );
        assert_eq!(
            doc.export_to_markdown(),
            "# Title\n\nHello **bold** and *italic* and [link](https://x.com/) .\n"
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
    fn hidden_inline_styles_are_suppressed_but_aria_hidden_is_kept() {
        // display:none / visibility:hidden / the `hidden` attribute are not
        // rendered, so their text is dropped.
        let hidden = convert(
            "<p>keep</p>\
             <p style=\"display:none\">gone</p>\
             <p style=\"visibility: hidden\">gone2</p>\
             <p hidden>gone3</p>",
        );
        assert_eq!(hidden.export_to_markdown(), "keep\n");
        // aria-hidden leaves the element visually rendered, so its text stays.
        let aria = convert("<p aria-hidden=\"true\">still shown</p>");
        assert_eq!(aria.export_to_markdown(), "still shown\n");
    }

    #[test]
    fn nested_table_flattens_with_docling_spacing() {
        // A table nested in a cell flattens to its own grid joined with single
        // spaces; a deeper table inside one of those cells contributes its raw
        // subtree text, whose source line breaks survive as newlines (flattened
        // to spaces by the table serializer). The `\n` here is the source line
        // break between the innermost table's rows: docling renders `a  b`
        // (td-trailing space + newline), not `a b`.
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
        assert_eq!(table.rows[0][0], "P Q R a \nb");
        assert_eq!(table.rows[0][1], "Z");
        // The markdown serializer flattens the newline to a space.
        assert!(
            doc.export_to_markdown().contains("P Q R a  b"),
            "newline flattened to space in markdown: {}",
            doc.export_to_markdown()
        );
    }

    #[test]
    fn form_region_becomes_key_value_fields() {
        // A `form_region` container with the `keyN` / `keyN_marker` / `keyN_valueM`
        // id-convention is a docling field region: region + each item render as a
        // `<!-- missing-text -->` marker, then the item's marker/key/value texts.
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
            "<!-- missing-text -->\n\n\
             <!-- missing-text -->\n\n1\n\nRestaurant\n\nDocling\n\n\
             <!-- missing-text -->\n\n2\n\nTelephone\n\n123\n",
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
        let doc = convert_html("t", &html, &FsImageResolver::new(None));
        match &doc.nodes[0] {
            Node::Picture {
                image: Some(img),
                caption,
            } => {
                assert_eq!(caption.as_deref(), Some("k"));
                assert_eq!(img.mimetype, "image/png");
                assert_eq!((img.width, img.height), (2, 3));
            }
            other => panic!("expected an embedded image, got {other:?}"),
        }
    }
}
