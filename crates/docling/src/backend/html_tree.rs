//! docling's HTML **item tree** — a call-for-call port of
//! `docling/backend/html_backend.py`'s `HTMLDocumentBackend` (2.126, the
//! non-rendering path), producing the [`ItemTree`] the JSON export serializes.
//!
//! [`super::html`] walks the DOM into the flat node stream that drives
//! Markdown / DocLang / LaTeX byte-for-byte. Upstream's *JSON* is a different
//! shape — a tree whose rules are the backend's, not the serializer's:
//!
//! * everything after a heading is a **child of the heading** (an `h1` is the
//!   title; `h2`–`h6` are `section_header`s of level `n-1`, with `header-N`
//!   section groups filling a skipped level);
//! * a paragraph is split into **annotated parts** (one per source text node,
//!   carrying the ancestor `<b>`/`<i>`/`<u>`/`<s>`/`<sub>`/`<sup>`/`<code>`
//!   formatting and `<a href>`), adjacent parts with identical annotation are
//!   merged, and two or more parts become an **`inline` group** of one text
//!   (or `code`) item each — a single part stays a lone item carrying its
//!   `formatting` / `hyperlink`;
//! * a list item with mixed formatting is an empty `list_item` holding an
//!   inline group; a `<dt>` is a bold item whose `<dd>`s sit in a
//!   `descriptions` list group under it;
//! * a **rich table cell**'s content is walked into items that are then
//!   re-parented under a `rich_cell_group_{tables}_{col}_{row}` group child
//!   of the table, and the cell carries a `ref` to it;
//! * a picture's caption is a `caption` text item **on the body**, whatever
//!   the picture's parent;
//! * content before the first heading, a `<footer>`, and the `<title>` live on
//!   the **`furniture`** layer;
//! * items are numbered in the order upstream *creates* them.
//!
//! The port keeps upstream's control flow (and its quirks — a `<br>` sentinel
//! surviving into a `<pre>`, a heading inside a table cell resetting the
//! parent stack) so that the JSON is structurally identical to docling's on
//! the whole HTML corpus. Nothing here affects the other serializers.

use docling_core::tree::{Formatting, ItemTree, ListMeta, TreeKind};
use docling_core::{ContentLayer, Script, Table, TableCell};
use ego_tree::NodeRef;
use scraper::{ElementRef, Html, Node as HtmlNode, Selector};

use super::html::{
    checkbox_label_text, detect_field_region, is_hidden, is_rich_cell, lang_from_class,
    normalize_url, normalize_ws, subtree_text,
};
use super::images::ImageResolver;

macro_rules! sel {
    ($sel:literal) => {{
        static SEL: std::sync::OnceLock<Selector> = std::sync::OnceLock::new();
        SEL.get_or_init(|| Selector::parse($sel).unwrap())
    }};
}

/// upstream's `_BR_SENTINEL`: every `<br>` in the body is read as this
/// character, which the paragraph splitter turns into `\n` (one) or a
/// paragraph break (two or more).
const BR: char = '\u{e000}';

/// upstream's `_BLOCK_TAGS`: tags that start a distinct item.
const BLOCK_TAGS: &[&str] = &[
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

/// upstream's `_INLINE_HTML_TAGS`: wrappers whose text is buffered inline.
const INLINE_TAGS: &[&str] = &[
    "a", "abbr", "b", "bdi", "bdo", "cite", "code", "data", "dfn", "em", "i", "kbd", "label",
    "mark", "q", "s", "samp", "small", "span", "strong", "sub", "sup", "u", "var",
];

/// upstream's `_FORMAT_TAG_MAP` keys, in its order (a later tag wins a
/// conflicting `script`).
const FORMAT_TAGS: &[&str] = &[
    "b", "strong", "i", "em", "var", "s", "del", "u", "ins", "sub", "sup", "code", "kbd", "samp",
];

const CODE_TAGS: &[&str] = &["code", "kbd", "samp"];

/// upstream's `_CUSTOM_CHECKBOX_CLASSES` / `_CHECKBOX_CONTAINER_CLASSES` /
/// `_CHECKBOX_MARK_TEXTS`.
const CUSTOM_CHECKBOX_CLASSES: &[&str] = &["checkbox", "checkbox-box", "checkbox-input"];
const CHECKBOX_CONTAINER_CLASSES: &[&str] = &[
    "checkbox-container",
    "checkbox-item",
    "checkbox-option",
    "option",
];
const CHECKBOX_MARK_TEXTS: &[&str] = &["x", "✓", "✔", "☑"];

/// upstream's `AnnotatedText`: one text fragment with the annotation in force
/// where it was read.
#[derive(Debug, Clone, PartialEq)]
struct Part {
    text: String,
    hyperlink: Option<String>,
    formatting: Option<Formatting>,
    code: bool,
}

impl Part {
    fn same_annotation(&self, other: &Part) -> bool {
        self.hyperlink == other.hyperlink
            && self.formatting == other.formatting
            && self.code == other.code
    }
}

/// upstream's `_clean_unicode`.
fn clean_unicode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\u{00a0}' | '\u{202f}' => out.push(' '),
            '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{00ad}' | '\u{feff}' | '\u{2060}' => {}
            '\u{2010}'..='\u{2015}' => out.push('-'),
            '\u{2018}' | '\u{2019}' => out.push('\''),
            '\u{201c}' | '\u{201d}' => out.push('"'),
            '\u{2026}' => out.push_str("..."),
            c => out.push(c),
        }
    }
    out
}

/// Python's `" ".join(s.split())`: collapse whitespace runs, trim the ends.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Python's `re.sub(r"\s+|\n+", " ", s).strip()`.
fn squash_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending = false;
    for c in s.chars() {
        if c.is_whitespace() {
            pending = true;
        } else {
            if pending && !out.is_empty() {
                out.push(' ');
            }
            pending = false;
            out.push(c);
        }
    }
    out
}

fn classes(e: &scraper::node::Element) -> Vec<&str> {
    e.attr("class")
        .map(|c| c.split_whitespace().collect())
        .unwrap_or_default()
}

fn has_class(e: &scraper::node::Element, set: &[&str]) -> bool {
    classes(e).iter().any(|c| set.contains(c))
}

fn is_input_checkbox_or_radio(e: &scraper::node::Element) -> bool {
    e.name() == "input"
        && matches!(
            e.attr("type")
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "checkbox" | "radio"
        )
}

fn is_custom_checkbox(e: &scraper::node::Element) -> bool {
    has_class(e, CUSTOM_CHECKBOX_CLASSES)
}

fn is_checkbox_like(e: &scraper::node::Element) -> bool {
    is_input_checkbox_or_radio(e) || is_custom_checkbox(e)
}

/// upstream's `_is_checkbox_label_container`.
fn is_checkbox_label_container(el: ElementRef) -> bool {
    has_class(el.value(), CHECKBOX_CONTAINER_CLASSES)
        && el
            .children()
            .filter_map(ElementRef::wrap)
            .any(|c| is_checkbox_like(c.value()))
}

/// upstream's `_is_checkbox_label_tag`.
fn is_checkbox_label_tag(el: ElementRef) -> bool {
    if is_checkbox_like(el.value()) {
        return false;
    }
    if classes(el.value()).contains(&"checkbox-label") {
        return true;
    }
    el.parent()
        .and_then(ElementRef::wrap)
        .is_some_and(is_checkbox_label_container)
}

/// upstream's `_is_checkbox_checked`.
fn is_checkbox_checked(el: ElementRef) -> bool {
    let e = el.value();
    let truthy = |v: Option<&str>| {
        matches!(
            v.unwrap_or("").trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        )
    };
    if is_input_checkbox_or_radio(e) {
        return e.attr("checked").is_some() || truthy(e.attr("aria-checked"));
    }
    if classes(e).contains(&"checked")
        || truthy(e.attr("aria-checked"))
        || truthy(e.attr("data-checked"))
    {
        return true;
    }
    let mut text = String::new();
    subtree_text(el, &mut text);
    let text: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    CHECKBOX_MARK_TEXTS.contains(&text.to_lowercase().as_str())
}

/// upstream's `_normalize_checkbox_text`.
fn normalize_checkbox_text(text: &str) -> String {
    let compact = squash_ws(text);
    if compact.is_empty() || CHECKBOX_MARK_TEXTS.contains(&compact.to_lowercase().as_str()) {
        return String::new();
    }
    clean_unicode(&compact)
}

/// A tag whose subtree upstream removed before walking (`script`, `style`,
/// `noscript`) or hides (`hidden`, `aria-hidden`, inline `display:none`).
fn suppressed(e: &scraper::node::Element) -> bool {
    matches!(e.name(), "script" | "style" | "noscript") || is_hidden(e)
}

/// The nearest ancestor element of `node` named `name`, stopping at (and
/// excluding) `boundary` when given.
fn ancestor_named<'a>(
    node: NodeRef<'a, HtmlNode>,
    name: &str,
    boundary: Option<NodeRef<'a, HtmlNode>>,
) -> Option<ElementRef<'a>> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if boundary.is_some_and(|b| b.id() == n.id()) {
            return None;
        }
        if let Some(el) = ElementRef::wrap(n) {
            if el.value().name() == name {
                return Some(el);
            }
        }
        cur = n.parent();
    }
    None
}

/// upstream's `_has_list_ancestor`: a `ul`/`ol`/`dl` between `elem` and
/// `boundary`.
fn has_list_ancestor(elem: NodeRef<'_, HtmlNode>, boundary: NodeRef<'_, HtmlNode>) -> bool {
    let mut cur = elem.parent();
    while let Some(n) = cur {
        if n.id() == boundary.id() {
            return false;
        }
        if let Some(el) = ElementRef::wrap(n) {
            if matches!(el.value().name(), "ul" | "ol" | "dl") {
                return true;
            }
        }
        cur = n.parent();
    }
    false
}

/// upstream's `_get_cell_spans`: `colspan` / `rowspan`, each the leading
/// digit run when the attribute starts with a digit, else 1 (so `"0"` is 0).
fn cell_spans(cell: ElementRef) -> (usize, usize) {
    let num = |attr: &str| -> usize {
        let raw = cell.value().attr(attr).unwrap_or("1");
        if raw.starts_with(|c: char| c.is_ascii_digit()) {
            let digits: String = raw.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse().unwrap_or(1)
        } else {
            1
        }
    };
    (num("colspan"), num("rowspan"))
}

/// An `href` as docling's `AnnotatedText.hyperlink` serializes it: a URL
/// with a scheme goes through pydantic's `AnyUrl` (a bare `http(s)://host`
/// gains its `/`), anything else through `pathlib.Path`, which drops a
/// trailing slash and `.` segments and collapses repeated slashes (keeping a
/// protocol-relative `//host`).
pub(super) fn docling_href(href: &str) -> String {
    let has_scheme = href.split_once(':').is_some_and(|(scheme, _)| {
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    if has_scheme {
        return normalize_url(href);
    }
    let lead = if href.starts_with("//") && !href.starts_with("///") {
        "//"
    } else if href.starts_with('/') {
        "/"
    } else {
        ""
    };
    let segs: Vec<&str> = href
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let joined = segs.join("/");
    if joined.is_empty() {
        return if lead.is_empty() {
            ".".to_string()
        } else {
            lead.to_string()
        };
    }
    format!("{lead}{joined}")
}

/// upstream's `_has_inline_display_style`.
fn has_inline_display_style(e: &scraper::node::Element) -> bool {
    e.attr("style").is_some_and(|style| {
        style.split(';').any(|decl| {
            let mut it = decl.splitn(2, ':');
            match (it.next(), it.next()) {
                (Some(p), Some(v)) if p.trim().eq_ignore_ascii_case("display") => {
                    let v = v.trim().to_ascii_lowercase();
                    v.starts_with("inline") || v == "contents"
                }
                _ => false,
            }
        })
    })
}

/// upstream's `_code_language_hint`: the first highlighter class on the
/// `<pre>` or a `<code>` inside it that names a known language,
/// `language-`/`lang-`-prefixed tokens first.
fn code_language_hint(pre: ElementRef) -> Option<String> {
    let mut tokens: Vec<String> = classes(pre.value()).iter().map(|s| s.to_string()).collect();
    for code in pre.select(sel!("code")) {
        tokens.extend(classes(code.value()).iter().map(|s| s.to_string()));
    }
    tokens.sort();
    tokens.dedup();
    let known = |t: &str| {
        let bare = lang_from_class(t).unwrap_or_else(|| t.to_string());
        (docling_core::code_language_label(&bare) != "unknown").then_some(bare)
    };
    let (prefixed, bare): (Vec<&String>, Vec<&String>) = tokens.iter().partition(|t| {
        let l = t.to_ascii_lowercase();
        l.starts_with("language-") || l.starts_with("lang-")
    });
    prefixed.into_iter().chain(bare).find_map(|t| known(t))
}

/// Build docling's item tree for a parsed HTML document.
pub(super) fn build_tree(parsed: &Html, images: &dyn ImageResolver) -> ItemTree {
    let root = parsed.root_element();
    let mut w = Walker {
        tree: ItemTree::default(),
        parents: vec![None; 64],
        level: 0,
        layer: None,
        format_tags: Vec::new(),
        hyperlink: None,
        images,
    };
    // The `<title>` is a furniture-layer title item, created first.
    if let Some(title) = parsed.select(sel!("title")).next() {
        let parts: Vec<String> = title
            .text()
            .map(normalize_ws)
            .filter(|t| !t.is_empty())
            .collect();
        let text = parts.join(" ");
        w.tree.add(
            None,
            Some(ContentLayer::Furniture),
            TreeKind::Text {
                label: "title".into(),
                text: clean_unicode(&text),
                orig: Some(text.clone()).filter(|o| *o != clean_unicode(&text)),
                formatting: None,
                hyperlink: None,
                level: None,
                list: None,
            },
        );
    }
    let content = parsed.select(sel!("body")).next().unwrap_or(root);
    // `infer_furniture`: everything before the first heading (one not inside
    // a table) is site chrome on the furniture layer.
    let has_header = content
        .select(sel!("h1, h2, h3, h4, h5, h6"))
        .any(|h| ancestor_named(*h, "table", None).is_none());
    w.layer = has_header.then_some(ContentLayer::Furniture);
    w.walk(content);
    w.tree
}

struct Walker<'a> {
    tree: ItemTree,
    /// upstream's `self.parents`: the item at each nesting level (`None` =
    /// the body). Level 0 is always the body.
    parents: Vec<Option<usize>>,
    level: usize,
    layer: Option<ContentLayer>,
    format_tags: Vec<&'a str>,
    hyperlink: Option<String>,
    images: &'a dyn ImageResolver,
}

impl<'a> Walker<'a> {
    fn parent(&self) -> Option<usize> {
        self.parents[self.level]
    }

    fn set_parent(&mut self, level: usize, item: Option<usize>) {
        if level >= self.parents.len() {
            self.parents.resize(level + 1, None);
        }
        self.parents[level] = item;
    }

    /// upstream's `_formatting`: the active format tags folded into one
    /// `Formatting`, `None` when none of them carries an attribute (the code
    /// tags map to nothing).
    fn formatting(&self) -> Option<Formatting> {
        let mut f = Formatting::default();
        let mut any = false;
        for t in &self.format_tags {
            match *t {
                "b" | "strong" => f.bold = true,
                "i" | "em" | "var" => f.italic = true,
                "s" | "del" => f.strikethrough = true,
                "u" | "ins" => f.underline = true,
                "sub" => f.script = Script::Sub,
                "sup" => f.script = Script::Super,
                _ => continue,
            }
            any = true;
        }
        any.then_some(f)
    }

    fn in_code(&self) -> bool {
        self.format_tags.iter().any(|t| CODE_TAGS.contains(t))
    }

    /// `doc.add_text(label, text, parent=self.parents[self.level], …)` with an
    /// explicit annotation.
    fn add_annotated(
        &mut self,
        label: &str,
        text: String,
        orig: Option<String>,
        formatting: Option<Formatting>,
        hyperlink: Option<String>,
        parent: Option<usize>,
    ) -> usize {
        self.tree.add(
            parent,
            self.layer,
            TreeKind::Text {
                label: label.into(),
                text,
                orig,
                formatting,
                hyperlink,
                level: None,
                list: None,
            },
        )
    }

    fn add_code(
        &mut self,
        text: String,
        language: Option<String>,
        formatting: Option<Formatting>,
        hyperlink: Option<String>,
    ) -> usize {
        let parent = self.parent();
        self.tree.add(
            parent,
            self.layer,
            TreeKind::Code {
                text,
                orig: None,
                language,
                formatting,
                hyperlink,
            },
        )
    }

    fn add_group(&mut self, label: &str, name: &str, parent: Option<usize>) -> usize {
        self.tree.add(
            parent,
            self.layer,
            TreeKind::Group {
                label: label.into(),
                name: name.into(),
            },
        )
    }

    // ----- text extraction --------------------------------------------------

    /// upstream's `_extract_text_and_hyperlink_recursively`.
    fn extract(
        &mut self,
        node: NodeRef<'a, HtmlNode>,
        ignore_list: bool,
        find_parent_annotation: bool,
        keep_newlines: bool,
    ) -> Vec<Part> {
        if find_parent_annotation {
            // Any `<a href>` above re-applies the ancestors' formatting and
            // its link (the formatting alone is already on the stack).
            let mut tags: Vec<&'a str> = Vec::new();
            for &ft in FORMAT_TAGS {
                let mut cur = node.parent();
                while let Some(n) = cur {
                    if let Some(el) = ElementRef::wrap(n) {
                        if el.value().name() == ft {
                            tags.push(ft);
                        }
                    }
                    cur = n.parent();
                }
            }
            let mut cur = node.parent();
            while let Some(n) = cur {
                if let Some(el) = ElementRef::wrap(n) {
                    if el.value().name() == "a" && el.value().attr("href").is_some() {
                        let depth = self.format_tags.len();
                        self.format_tags.extend(tags);
                        let saved = self.use_hyperlink(el);
                        let out = self.extract(node, ignore_list, false, false);
                        self.restore_hyperlink(saved);
                        self.format_tags.truncate(depth);
                        return out;
                    }
                }
                cur = n.parent();
            }
        }
        match node.value() {
            HtmlNode::Text(t) => {
                if let Some(p) = node.parent().and_then(ElementRef::wrap) {
                    if suppressed(p.value()) || is_checkbox_label_container(p) {
                        return Vec::new();
                    }
                }
                let t: &str = t;
                self.text_parts(t, keep_newlines)
            }
            HtmlNode::Element(e) => {
                // A `<br>` is the sentinel character upstream substituted.
                if e.name() == "br" {
                    return self.text_parts(&BR.to_string(), keep_newlines);
                }
                let el = ElementRef::wrap(node).expect("element");
                if suppressed(e) || is_checkbox_like(e) || is_checkbox_label_tag(el) {
                    return Vec::new();
                }
                let mut out = Vec::new();
                if !ignore_list || !matches!(e.name(), "ul" | "ol" | "dl" | "table") {
                    for child in node.children() {
                        match child.value() {
                            HtmlNode::Element(ce) if FORMAT_TAGS.contains(&ce.name()) => {
                                let name: &'a str = ce.name();
                                self.format_tags.push(name);
                                out.extend(self.extract(child, ignore_list, false, keep_newlines));
                                self.format_tags.pop();
                            }
                            HtmlNode::Element(ce) if ce.name() == "a" => {
                                let cel = ElementRef::wrap(child).expect("element");
                                let saved = self.use_hyperlink(cel);
                                out.extend(self.extract(child, ignore_list, false, keep_newlines));
                                self.restore_hyperlink(saved);
                            }
                            _ => out.extend(self.extract(child, ignore_list, false, keep_newlines)),
                        }
                    }
                }
                out
            }
            _ => Vec::new(),
        }
    }

    /// One source text node as annotated parts (upstream's `NavigableString`
    /// branch).
    fn text_parts(&self, raw: &str, keep_newlines: bool) -> Vec<Part> {
        let text = if keep_newlines {
            raw.trim().to_string()
        } else {
            collapse_ws(&raw.replace(['\n', '\r'], " "))
        };
        let part = |text: String| Part {
            text,
            hyperlink: self.hyperlink.clone(),
            formatting: self.formatting(),
            code: self.in_code(),
        };
        if !text.is_empty() {
            return vec![part(text)];
        }
        if keep_newlines && raw.trim_matches(['\n', '\r']).is_empty() {
            return vec![part("\n".into())];
        }
        Vec::new()
    }

    /// upstream's `_use_hyperlink` (enter): returns what to restore.
    fn use_hyperlink(&mut self, a: ElementRef) -> Option<Option<String>> {
        let href = a.value().attr("href")?;
        if href.is_empty() {
            return None;
        }
        let old = self.hyperlink.replace(docling_href(href));
        Some(old)
    }

    fn restore_hyperlink(&mut self, saved: Option<Option<String>>) {
        if let Some(old) = saved {
            self.hyperlink = old;
        }
    }

    // ----- inline groups ----------------------------------------------------

    /// upstream's `_use_inline_group` (enter): a group when the list has more
    /// than one part (or `force`), made the current parent.
    fn open_inline_group(&mut self, parts: &[Part], force: bool) -> Option<usize> {
        if !force && parts.len() <= 1 {
            return None;
        }
        let parent = self.parent();
        let gid = self.add_group("inline", "group", parent);
        self.set_parent(self.level + 1, Some(gid));
        self.level += 1;
        Some(gid)
    }

    fn close_inline_group(&mut self, group: Option<usize>) {
        if group.is_some() {
            self.set_parent(self.level, None);
            self.level -= 1;
        }
    }

    /// Emit one annotated part as a `text` or `code` item under the current
    /// parent (upstream's per-part `add_text` / `add_code`).
    fn emit_part(&mut self, part: &Part, text: String, formatting: Option<Formatting>) -> usize {
        if part.code {
            self.add_code(text, None, formatting, part.hyperlink.clone())
        } else {
            let parent = self.parent();
            self.add_annotated(
                "text",
                text,
                None,
                formatting,
                part.hyperlink.clone(),
                parent,
            )
        }
    }

    // ----- the walk ---------------------------------------------------------

    /// upstream's `_walk`: buffer inline text across inline tags, emit at
    /// block boundaries. Returns the refs of the items this walk created
    /// directly.
    fn walk(&mut self, element: ElementRef<'a>) -> Vec<usize> {
        let mut added: Vec<usize> = Vec::new();
        let mut buffer: Vec<Part> = Vec::new();
        let element_name = element.value().name();
        for child in element.children() {
            match child.value() {
                HtmlNode::Element(e) => {
                    let name = e.name();
                    if suppressed(e) {
                        continue;
                    }
                    let cel = ElementRef::wrap(child).expect("element");
                    if name == "br" {
                        // The substituted sentinel string, read like any text.
                        buffer.extend(self.extract(child, false, true, false));
                        continue;
                    }
                    let has_block_descendants = cel.descendants().skip(1).any(|d| {
                        d.value().as_element().is_some_and(|de| {
                            BLOCK_TAGS.contains(&de.name())
                                || de.name() == "input"
                                || is_custom_checkbox(de)
                        })
                    });
                    if has_class(e, &["form_region"]) {
                        self.flush(&mut buffer, element_name, &mut added);
                        added.extend(self.handle_form_container(cel));
                        continue;
                    }
                    if is_custom_checkbox(e) {
                        self.flush(&mut buffer, element_name, &mut added);
                        if let Some(r) = self.emit_custom_checkbox(cel) {
                            added.push(r);
                        }
                        continue;
                    }
                    if name == "img" {
                        self.flush(&mut buffer, element_name, &mut added);
                        added.push(self.emit_image(cel));
                    } else if name == "input" {
                        self.flush(&mut buffer, element_name, &mut added);
                        if let Some(r) = self.emit_input(cel) {
                            added.push(r);
                        }
                    } else if FORMAT_TAGS.contains(&name) {
                        let tag: &'a str = name;
                        if has_block_descendants {
                            self.flush(&mut buffer, element_name, &mut added);
                            self.format_tags.push(tag);
                            added.extend(self.walk(cel));
                            self.format_tags.pop();
                        } else {
                            self.format_tags.push(tag);
                            buffer.extend(self.extract(child, false, true, false));
                            self.format_tags.pop();
                        }
                    } else if name == "a" {
                        if has_block_descendants {
                            self.flush(&mut buffer, element_name, &mut added);
                            let saved = self.use_hyperlink(cel);
                            added.extend(self.walk(cel));
                            self.restore_hyperlink(saved);
                        } else {
                            let saved = self.use_hyperlink(cel);
                            buffer.extend(self.extract(child, false, true, false));
                            self.restore_hyperlink(saved);
                        }
                    } else if BLOCK_TAGS.contains(&name) {
                        self.flush(&mut buffer, element_name, &mut added);
                        added.extend(self.handle_block(cel));
                    } else if has_block_descendants {
                        self.flush(&mut buffer, element_name, &mut added);
                        added.extend(self.walk(cel));
                    } else if INLINE_TAGS.contains(&name)
                        || (name == "div" && has_inline_display_style(e))
                    {
                        buffer.extend(self.extract(child, false, true, false));
                    } else {
                        self.flush(&mut buffer, element_name, &mut added);
                        added.extend(self.walk(cel));
                    }
                }
                HtmlNode::Text(t) => {
                    let t: &str = t;
                    if t.trim_matches(['\n', '\r']).is_empty() {
                        // Source line breaks between a cell's children split
                        // its inline runs into separate items.
                        if matches!(element_name, "td" | "th") && t.contains('\n') {
                            self.flush(&mut buffer, element_name, &mut added);
                        }
                        continue;
                    }
                    buffer.extend(self.extract(child, false, true, false));
                }
                _ => {}
            }
        }
        self.flush(&mut buffer, element_name, &mut added);
        added
    }

    /// upstream's `_flush_buffer`.
    fn flush(&mut self, buffer: &mut Vec<Part>, element_name: &str, added: &mut Vec<usize>) {
        if buffer.is_empty() {
            return;
        }
        let parts = simplify(std::mem::take(buffer));
        if parts.iter().all(|p| p.text.is_empty()) {
            return;
        }
        for list in split_by_newline(&parts) {
            let force = list.len() == 1 && list[0].code && !matches!(element_name, "p" | "pre");
            let group = self.open_inline_group(&list, force);
            for part in &list {
                let seg = part.text.trim();
                if seg.is_empty() {
                    continue;
                }
                let r = self.emit_part(part, clean_unicode(seg), part.formatting);
                if group.is_none() {
                    added.push(r);
                }
            }
            self.close_inline_group(group);
            if let Some(g) = group {
                added.push(g);
            }
        }
    }

    // ----- blocks -----------------------------------------------------------

    /// upstream's `_handle_block`.
    fn handle_block(&mut self, tag: ElementRef<'a>) -> Vec<usize> {
        let mut added: Vec<usize> = Vec::new();
        match tag.value().name() {
            "figure" => {
                for child in tag.children() {
                    let Some(cel) = ElementRef::wrap(child) else {
                        continue;
                    };
                    let name = cel.value().name();
                    if name == "figcaption" {
                        continue;
                    }
                    if name == "img" {
                        added.push(self.emit_image(cel));
                    } else if name == "input" {
                        if let Some(r) = self.emit_input(cel) {
                            added.push(r);
                        }
                    } else if BLOCK_TAGS.contains(&name) {
                        added.extend(self.handle_block(cel));
                    } else {
                        added.extend(self.walk(cel));
                    }
                }
                let any_picture = added
                    .iter()
                    .any(|&r| matches!(self.tree.items[r].kind, TreeKind::Picture { .. }));
                if !any_picture {
                    let caption_tag = tag
                        .children()
                        .filter_map(ElementRef::wrap)
                        .find(|c| c.value().name() == "figcaption");
                    if let Some(cap) = caption_tag {
                        let parts = self.extract(*cap, false, true, false);
                        let single = to_single(&parts);
                        if !single.text.is_empty() {
                            let text = clean_unicode(single.text.trim());
                            let parent = self.parent();
                            let cap_item = self.add_annotated(
                                "caption",
                                text,
                                Some(single.text.clone()),
                                single.formatting,
                                single.hyperlink,
                                parent,
                            );
                            if let Some(&first) = added.first() {
                                if let TreeKind::Table { captions, .. } =
                                    &mut self.tree.items[first].kind
                                {
                                    captions.push(cap_item);
                                }
                            }
                        }
                    }
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => added.extend(self.handle_heading(tag)),
            "ul" | "ol" | "dl" => added.push(self.handle_list(tag)),
            "p" | "address" | "summary" => {
                let parts = simplify(self.extract(*tag, false, true, false));
                for list in split_by_newline(&parts) {
                    let group = self.open_inline_group(&list, false);
                    for part in &list {
                        let seg = part.text.trim();
                        if seg.is_empty() {
                            continue;
                        }
                        let r = self.emit_part(part, clean_unicode(seg), part.formatting);
                        if group.is_none() {
                            added.push(r);
                        }
                    }
                    self.close_inline_group(group);
                    if let Some(g) = group {
                        added.push(g);
                    }
                }
                let imgs: Vec<ElementRef<'a>> = tag.select(sel!("img")).collect();
                for img in imgs {
                    self.emit_image(img);
                }
                let inputs: Vec<ElementRef<'a>> = tag.select(sel!("input")).collect();
                for input in inputs {
                    if let Some(r) = self.emit_input(input) {
                        added.push(r);
                    }
                }
                let boxes: Vec<ElementRef<'a>> = tag
                    .descendants()
                    .skip(1)
                    .filter_map(ElementRef::wrap)
                    .filter(|c| is_custom_checkbox(c.value()))
                    .collect();
                for cb in boxes {
                    if let Some(r) = self.emit_custom_checkbox(cb) {
                        added.push(r);
                    }
                }
            }
            "table" => added.push(self.handle_table(tag)),
            "stamp" | "signature" => {
                let class_name = tag.value().name().to_string();
                let parent = self.parent();
                let pic = self.tree.add(
                    parent,
                    self.layer,
                    TreeKind::Picture {
                        captions: Vec::new(),
                        image: None,
                        classification: Some(class_name),
                        chart: None,
                    },
                );
                let mut raw = String::new();
                subtree_text(tag, &mut raw);
                let text = clean_unicode(raw.trim());
                // `doc.add_text(parent=placeholder)`: no layer argument, so the
                // body layer.
                self.tree.add(
                    Some(pic),
                    None,
                    TreeKind::Text {
                        label: "text".into(),
                        text,
                        orig: None,
                        formatting: None,
                        hyperlink: None,
                        level: None,
                        list: None,
                    },
                );
            }
            "pre" => {
                let parts = simplify(self.extract(*tag, false, true, true));
                let hint = code_language_hint(tag);
                let group = self.open_inline_group(&parts, false);
                for part in &parts {
                    let text = clean_unicode(part.text.trim());
                    let language = hint.clone().or_else(|| detect_code_language(&text));
                    let r = self.add_code(text, language, part.formatting, part.hyperlink.clone());
                    if group.is_none() {
                        added.push(r);
                    }
                }
                self.close_inline_group(group);
                if let Some(g) = group {
                    added.push(g);
                }
            }
            "footer" => {
                let saved_layer = self.layer;
                self.layer = Some(ContentLayer::Furniture);
                let parent = self.parent();
                let g = self.add_group("section", "footer", parent);
                self.set_parent(self.level + 1, Some(g));
                self.level += 1;
                self.walk(tag);
                self.set_parent(self.level + 1, None);
                self.level -= 1;
                self.layer = saved_layer;
            }
            "details" => {
                let parent = self.parent();
                let g = self.add_group("section", "details", parent);
                self.set_parent(self.level + 1, Some(g));
                self.level += 1;
                self.walk(tag);
                self.set_parent(self.level + 1, None);
                self.level -= 1;
            }
            _ => {}
        }
        added
    }

    /// upstream's `_handle_heading`.
    fn handle_heading(&mut self, tag: ElementRef<'a>) -> Vec<usize> {
        let mut added = Vec::new();
        // A heading ends the furniture prelude.
        self.layer = None;
        let mut level: usize = tag.value().name()[1..].parse().unwrap_or(1);
        let parts = self.extract(*tag, false, true, false);
        let single = to_single(&parts);
        let text = clean_unicode(&single.text);
        let orig = Some(single.text.clone()).filter(|o| *o != text);
        if level == 1 {
            for p in self.parents.iter_mut() {
                *p = None;
            }
            self.level = 0;
            let title = self.add_annotated(
                "title",
                text,
                orig,
                single.formatting,
                single.hyperlink,
                None,
            );
            self.set_parent(1, Some(title));
            added.push(title);
        } else {
            level -= 1;
            if level > self.level {
                for i in self.level..level {
                    let parent = self.parents[i];
                    let g = self.add_group("section", &format!("header-{}", i + 1), parent);
                    self.set_parent(i + 1, Some(g));
                }
                self.level = level;
            } else if level < self.level {
                for k in (level + 2)..self.parents.len() {
                    self.parents[k] = None;
                }
                self.level = level;
            }
            let parent = self.parent();
            let heading_level = self.level as u8;
            let h = self.tree.add(
                parent,
                self.layer,
                TreeKind::Text {
                    label: "section_header".into(),
                    text,
                    orig,
                    formatting: single.formatting,
                    hyperlink: single.hyperlink,
                    level: Some(heading_level),
                    list: None,
                },
            );
            self.set_parent(self.level + 1, Some(h));
            added.push(h);
        }
        self.level += 1;
        let imgs: Vec<ElementRef<'a>> = tag.select(sel!("img")).collect();
        for img in imgs {
            added.push(self.emit_image(img));
        }
        added
    }

    // ----- lists ------------------------------------------------------------

    /// upstream's `_handle_list`.
    fn handle_list(&mut self, tag: ElementRef<'a>) -> usize {
        let name = tag.value().name();
        let is_ordered = name == "ol";
        let is_description = name == "dl";
        let start: Option<usize> = if is_ordered {
            tag.value().attr("start").and_then(|s| {
                s.chars()
                    .all(|c| c.is_ascii_digit())
                    .then(|| s.parse().ok())
                    .flatten()
            })
        } else {
            None
        };
        let group_name = if is_description {
            "description list".to_string()
        } else if is_ordered {
            match start {
                Some(s) => format!("ordered list start {s}"),
                None => "ordered list".to_string(),
            }
        } else {
            "list".to_string()
        };
        let parent = self.parent();
        let list_group = self.add_group("list", &group_name, parent);
        self.set_parent(self.level + 1, Some(list_group));
        self.level += 1;

        if is_description {
            let mut current_dt: Option<usize> = None;
            let mut dd_group: Option<usize> = None;
            let children: Vec<ElementRef<'a>> = tag
                .children()
                .filter_map(ElementRef::wrap)
                .filter(|c| matches!(c.value().name(), "dt" | "dd"))
                .collect();
            for child in children {
                if child.value().name() == "dt" {
                    dd_group = None;
                    let bold = Formatting {
                        bold: true,
                        ..Formatting::default()
                    };
                    current_dt =
                        self.add_list_item_with_content(child, list_group, false, "", Some(bold));
                    if let Some(dt) = current_dt {
                        self.set_parent(self.level + 1, Some(dt));
                    }
                } else {
                    let has_nested_dl = child
                        .children()
                        .filter_map(ElementRef::wrap)
                        .any(|c| c.value().name() == "dl");
                    if has_nested_dl {
                        dd_group = None;
                        if let Some(dt) = current_dt {
                            self.with_list_item_context(Some(dt), |w| {
                                w.process_list_item_nested_content(child)
                            });
                        }
                    } else {
                        if dd_group.is_none() {
                            if let Some(dt) = current_dt {
                                dd_group = Some(self.add_group("list", "descriptions", Some(dt)));
                            }
                        }
                        let dd_parent = dd_group.unwrap_or(list_group);
                        let dd_item =
                            self.add_list_item_with_content(child, dd_parent, false, "", None);
                        let content_parent = dd_item.unwrap_or(dd_parent);
                        self.with_list_item_context(Some(content_parent), |w| {
                            w.process_list_item_nested_content(child)
                        });
                    }
                }
            }
            self.set_parent(self.level + 1, None);
            self.level -= 1;
            return list_group;
        }

        let mut counter = 0usize;
        let items: Vec<ElementRef<'a>> = tag
            .children()
            .filter_map(ElementRef::wrap)
            .filter(|c| matches!(c.value().name(), "li" | "ul" | "ol"))
            .collect();
        for li in items {
            if matches!(li.value().name(), "ul" | "ol") {
                // Invalid HTML (a list directly inside a list): its own group
                // under this one.
                self.handle_block(li);
                continue;
            }
            let marker = match start {
                Some(s) if is_ordered => format!("{}.", s + counter),
                _ => String::new(),
            };
            let inputs: Vec<ElementRef<'a>> = li
                .select(sel!("input"))
                .filter(|i| ancestor_named(**i, "li", None).is_some_and(|l| l.id() == li.id()))
                .collect();
            let boxes: Vec<ElementRef<'a>> = li
                .descendants()
                .skip(1)
                .filter_map(ElementRef::wrap)
                .filter(|c| is_custom_checkbox(c.value()))
                .filter(|c| ancestor_named(**c, "li", None).is_some_and(|l| l.id() == li.id()))
                .collect();
            let item = self.add_list_item_with_content(li, list_group, is_ordered, &marker, None);
            if item.is_some() {
                counter += 1;
            }
            if item.is_some() || !inputs.is_empty() || !boxes.is_empty() {
                self.with_list_item_context(item, |w| {
                    for input in &inputs {
                        w.emit_input(*input);
                    }
                    for cb in &boxes {
                        w.emit_custom_checkbox(*cb);
                    }
                    w.process_list_item_nested_content(li);
                });
            } else {
                let sublists: Vec<ElementRef<'a>> = li
                    .descendants()
                    .skip(1)
                    .filter_map(ElementRef::wrap)
                    .filter(|c| matches!(c.value().name(), "ul" | "ol" | "dl"))
                    .collect();
                for sub in sublists {
                    if !has_list_ancestor(*sub, *li) {
                        self.handle_block(sub);
                    }
                }
            }
        }
        self.set_parent(self.level + 1, None);
        self.level -= 1;
        list_group
    }

    /// upstream's `_use_list_item_context`.
    fn with_list_item_context(&mut self, item: Option<usize>, f: impl FnOnce(&mut Self)) {
        match item {
            Some(it) => {
                self.set_parent(self.level + 1, Some(it));
                self.level += 1;
                f(self);
                self.set_parent(self.level + 1, None);
                self.level -= 1;
            }
            None => f(self),
        }
    }

    /// upstream's `_process_list_item_nested_content` + `_process_nested_element`.
    fn process_list_item_nested_content(&mut self, li: ElementRef<'a>) {
        for child in li.children() {
            self.process_nested_element(child, li);
        }
    }

    fn process_nested_element(&mut self, node: NodeRef<'a, HtmlNode>, li: ElementRef<'a>) {
        let Some(el) = ElementRef::wrap(node) else {
            return;
        };
        match el.value().name() {
            "img" => {
                self.emit_image(el);
            }
            "ul" | "ol" | "dl" => {
                if !has_list_ancestor(node, *li) {
                    self.handle_block(el);
                    self.set_parent(self.level + 1, None);
                }
            }
            "table" => {
                self.handle_block(el);
                self.set_parent(self.level + 1, None);
            }
            _ => {
                for child in node.children() {
                    self.process_nested_element(child, li);
                }
            }
        }
    }

    /// upstream's `_add_list_item_with_content`.
    fn add_list_item_with_content(
        &mut self,
        tag: ElementRef<'a>,
        parent: usize,
        enumerated: bool,
        marker: &str,
        extra: Option<Formatting>,
    ) -> Option<usize> {
        let parts = self.extract(*tag, true, true, false);
        let min_parts = simplify(parts);
        let joined: String = min_parts.iter().map(|p| p.text.as_str()).collect();
        let item_text = squash_ws(&joined);
        if item_text.is_empty() {
            return None;
        }
        let with_extra = |f: Option<Formatting>| match extra {
            Some(e) if e.bold => Some(Formatting {
                bold: true,
                ..f.unwrap_or_default()
            }),
            _ => f,
        };
        let meta = ListMeta {
            enumerated,
            marker: marker.to_string(),
        };
        if min_parts.len() > 1 {
            let item = self.tree.add(
                Some(parent),
                self.layer,
                TreeKind::Text {
                    label: "list_item".into(),
                    text: String::new(),
                    orig: None,
                    formatting: None,
                    hyperlink: None,
                    level: None,
                    list: Some(meta),
                },
            );
            self.set_parent(self.level + 1, Some(item));
            self.level += 1;
            let group = self.open_inline_group(&min_parts, false);
            for part in &min_parts {
                let text = clean_unicode(&squash_ws(&part.text));
                self.emit_part(part, text, with_extra(part.formatting));
            }
            self.close_inline_group(group);
            self.set_parent(self.level, None);
            self.level -= 1;
            Some(item)
        } else {
            let part = &min_parts[0];
            let text = squash_ws(&part.text);
            let clean = clean_unicode(&text);
            let orig = (text != clean).then_some(text);
            Some(self.tree.add(
                Some(parent),
                self.layer,
                TreeKind::Text {
                    label: "list_item".into(),
                    text: clean,
                    orig,
                    formatting: with_extra(part.formatting),
                    hyperlink: part.hyperlink.clone(),
                    level: None,
                    list: Some(meta),
                },
            ))
        }
    }

    // ----- tables -----------------------------------------------------------

    /// upstream's table branch of `_handle_block` with `get_html_table_row_col`
    /// and `parse_table_data`: the table item first, then the grid walked row
    /// by row — a rich cell's content is walked into items (with the parent
    /// stack preserved around it) and re-parented under a group of the table,
    /// every cell recorded with its declared spans and raw text.
    fn handle_table(&mut self, tag: ElementRef<'a>) -> usize {
        let parent = self.parent();
        let table_id = self.tree.add(
            parent,
            self.layer,
            TreeKind::Table {
                table: Table::default(),
                rich_cells: Vec::new(),
                captions: Vec::new(),
            },
        );
        // `thead` / `tbody` are unwrapped; a `tfoot`'s rows are not reached.
        let mut rows: Vec<ElementRef<'a>> = Vec::new();
        for child in tag.children().filter_map(ElementRef::wrap) {
            match child.value().name() {
                "tr" => rows.push(child),
                "thead" | "tbody" => rows.extend(
                    child
                        .children()
                        .filter_map(ElementRef::wrap)
                        .filter(|c| c.value().name() == "tr"),
                ),
                _ => {}
            }
        }
        let cells_of = |tr: ElementRef<'a>| -> Vec<ElementRef<'a>> {
            tr.children()
                .filter_map(ElementRef::wrap)
                .filter(|c| matches!(c.value().name(), "td" | "th"))
                .collect()
        };
        // `get_html_table_row_col`.
        let (mut num_rows, mut num_cols) = (0usize, 0usize);
        for tr in &rows {
            let mut col_count = 0;
            let mut is_row_header = true;
            for cell in cells_of(*tr) {
                let (col_span, row_span) = cell_spans(cell);
                col_count += col_span;
                if cell.value().name() == "td" || row_span == 1 {
                    is_row_header = false;
                }
            }
            num_cols = num_cols.max(col_count);
            if !is_row_header {
                num_rows += 1;
            }
        }
        // `parse_table_data`.
        let mut grid: Vec<Vec<Option<String>>> = vec![vec![None; num_cols]; num_rows];
        let mut cells: Vec<TableCell> = Vec::new();
        let mut rich: Vec<(usize, usize, usize)> = Vec::new();
        let mut start_row_span: usize = 0;
        let mut row_idx: isize = -1;
        for tr in &rows {
            let row_is_section = classes(tr.value()).contains(&"row_section");
            let tr_cells = cells_of(*tr);
            let mut col_header = true;
            let mut row_header = true;
            for cell in &tr_cells {
                let (_, row_span) = cell_spans(*cell);
                if cell.value().name() == "td" {
                    col_header = false;
                    row_header = false;
                } else if row_span == 1 {
                    row_header = false;
                }
            }
            if !row_header {
                row_idx += 1;
                start_row_span = 0;
            } else {
                start_row_span += 1;
            }
            let mut col_idx: usize = 0;
            for cell in tr_cells {
                let row_section = row_is_section || classes(cell.value()).contains(&"row_section");
                // The grid row this cell starts on (`start_row_span + row_idx`).
                let anchor_row = (row_idx + start_row_span as isize).max(0) as usize;
                let mut rich_group: Option<usize> = None;
                if is_rich_cell(cell) {
                    let saved_level = self.level;
                    let saved_parents = self.parents.clone();
                    let refs = self.walk(cell);
                    self.level = saved_level;
                    self.parents = saved_parents;
                    if !refs.is_empty() {
                        // Named before the occupied-slot skip below, so the
                        // column is still the previous cell's start column
                        // (0 for the first cell), not this cell's anchor.
                        let name = format!(
                            "rich_cell_group_{}_{}_{}",
                            self.tree.table_count(),
                            col_idx,
                            anchor_row
                        );
                        // `add_group(label, name, parent=table)`: no layer
                        // argument, so the body layer whatever the content's.
                        let gid = self.tree.add(
                            Some(table_id),
                            None,
                            TreeKind::Group {
                                label: "unspecified".into(),
                                name,
                            },
                        );
                        for r in refs {
                            self.tree.reparent(r, Some(gid));
                        }
                        rich_group = Some(gid);
                    }
                }
                let mut raw = String::new();
                subtree_text(cell, &mut raw);
                let text = clean_unicode(raw.trim());
                let (col_span, mut row_span) = cell_spans(cell);
                if row_header {
                    row_span = row_span.saturating_sub(1);
                }
                while col_idx < num_cols
                    && anchor_row < num_rows
                    && grid[anchor_row][col_idx].is_some()
                {
                    col_idx += 1;
                }
                for r in start_row_span..start_row_span + row_span {
                    for c in 0..col_span {
                        let gr = row_idx + r as isize;
                        if gr >= 0 && (gr as usize) < num_rows && col_idx + c < num_cols {
                            grid[gr as usize][col_idx + c] = Some(text.clone());
                        }
                    }
                }
                let is_th = cell.value().name() == "th";
                // docling#4216: a row-header row's cells label the rows they
                // span into — row headers, not column headers.
                cells.push(TableCell {
                    text: text.clone(),
                    bbox: None,
                    start_row: anchor_row,
                    start_col: col_idx,
                    row_span,
                    col_span,
                    column_header: col_header && !row_header,
                    row_header: row_header || (!col_header && is_th),
                    row_section,
                });
                if let Some(gid) = rich_group {
                    rich.push((anchor_row, col_idx, gid));
                }
                // upstream never advances `col_idx` by the span: the next
                // cell's occupied-slot skip walks it past the slots this one
                // just filled — which is why a rich cell's group is named
                // after the *previous* cell's column.
            }
        }
        let table = Table {
            rows: grid
                .into_iter()
                .map(|r| r.into_iter().map(Option::unwrap_or_default).collect())
                .collect(),
            cells: Some(cells),
            ..Table::default()
        };
        if let TreeKind::Table {
            table: t,
            rich_cells,
            ..
        } = &mut self.tree.items[table_id].kind
        {
            *t = table;
            *rich_cells = rich;
        }
        table_id
    }

    // ----- leaves -----------------------------------------------------------

    /// upstream's `_emit_image`.
    fn emit_image(&mut self, img: ElementRef<'a>) -> usize {
        let e = img.value();
        let parent = self.parent();
        let mut caption: Vec<Part> = Vec::new();
        if let Some(a) = (|| {
            let mut cur = img.parent();
            while let Some(n) = cur {
                if let Some(el) = ElementRef::wrap(n) {
                    if el.value().name() == "a" {
                        if let Some(h) = el.value().attr("href").filter(|h| !h.is_empty()) {
                            return Some(h);
                        }
                    }
                }
                cur = n.parent();
            }
            None
        })() {
            caption.push(Part {
                text: e.attr("alt").unwrap_or("").to_string(),
                hyperlink: Some(docling_href(a)),
                formatting: None,
                code: false,
            });
        }
        if let Some(fig) = ancestor_named(*img, "figure", None) {
            if let Some(cap) = fig
                .children()
                .filter_map(ElementRef::wrap)
                .find(|c| c.value().name() == "figcaption")
            {
                caption = self.extract(*cap, false, true, false);
            }
        }
        if caption.is_empty() {
            if let Some(alt) = e.attr("alt").filter(|a| !a.is_empty()) {
                caption.push(Part {
                    text: alt.to_string(),
                    hyperlink: None,
                    formatting: None,
                    code: false,
                });
            }
        }
        let single = to_single(&caption);
        let mut captions = Vec::new();
        if !single.text.is_empty() {
            let text = clean_unicode(single.text.trim());
            let orig = Some(single.text.clone()).filter(|o| *o != text);
            // `doc.add_text(label=CAPTION, …)` with no parent: the body.
            captions.push(self.add_annotated(
                "caption",
                text,
                orig,
                single.formatting,
                single.hyperlink,
                None,
            ));
        }
        let image = super::html::img_src(e).and_then(|s| self.images.resolve(&s));
        self.tree.add(
            parent,
            self.layer,
            TreeKind::Picture {
                captions,
                image,
                classification: None,
                chart: None,
            },
        )
    }

    /// upstream's `_emit_input`.
    fn emit_input(&mut self, input: ElementRef<'a>) -> Option<usize> {
        let e = input.value();
        if suppressed(e) {
            return None;
        }
        let ty = e.attr("type").unwrap_or("").trim().to_ascii_lowercase();
        if ty == "hidden" {
            return None;
        }
        let (label, text) = if is_input_checkbox_or_radio(e) {
            let label = if is_checkbox_checked(input) {
                "checkbox_selected"
            } else {
                "checkbox_unselected"
            };
            (label, normalize_checkbox_text(&checkbox_label_text(input)))
        } else {
            let text = ["value", "placeholder", "name"]
                .iter()
                .find_map(|a| e.attr(a).map(str::trim).filter(|t| !t.is_empty()))
                .unwrap_or("");
            ("text", clean_unicode(text))
        };
        let formatting = self.formatting();
        let hyperlink = self.hyperlink.clone();
        let parent = self.parent();
        Some(self.add_annotated(label, text, None, formatting, hyperlink, parent))
    }

    /// upstream's `_emit_custom_checkbox` (the class-based checkbox).
    fn emit_custom_checkbox(&mut self, cb: ElementRef<'a>) -> Option<usize> {
        if suppressed(cb.value()) {
            return None;
        }
        let label = if is_checkbox_checked(cb) {
            "checkbox_selected"
        } else {
            "checkbox_unselected"
        };
        let mut raw = String::new();
        subtree_text(cb, &mut raw);
        let text = normalize_checkbox_text(&raw);
        let formatting = self.formatting();
        let hyperlink = self.hyperlink.clone();
        let parent = self.parent();
        Some(self.add_annotated(label, text, None, formatting, hyperlink, parent))
    }

    /// upstream's `_handle_form_container`: a `form_region` with detectable
    /// key/value fields is a `field_region` under the current parent;
    /// without fields it is walked like any container.
    fn handle_form_container(&mut self, tag: ElementRef<'a>) -> Vec<usize> {
        match detect_field_region(tag) {
            Some(items) => {
                let parent = self.parent();
                vec![self
                    .tree
                    .add(parent, self.layer, TreeKind::FieldRegion { items })]
            }
            None if tag.value().name() == "table" => self.handle_block(tag),
            None => self.walk(tag),
        }
    }
}

/// upstream's `AnnotatedTextList.simplify_text_elements`: merge adjacent
/// parts with the same annotation, a space between them unless one side is
/// blank.
fn simplify(parts: Vec<Part>) -> Vec<Part> {
    let mut out: Vec<Part> = Vec::new();
    let Some(first) = parts.first() else {
        return out;
    };
    let mut cur = first.clone();
    let mut last_elm = first.text.clone();
    for p in &parts[1..] {
        if p.same_annotation(&cur) {
            let sep = if p.text.trim().is_empty() || last_elm.trim().is_empty() {
                ""
            } else {
                " "
            };
            cur.text.push_str(sep);
            cur.text.push_str(&p.text);
            last_elm = p.text.clone();
        } else {
            out.push(cur);
            cur = p.clone();
            last_elm = p.text.clone();
        }
    }
    if !cur.text.is_empty() {
        out.push(cur);
    }
    out
}

/// upstream's `AnnotatedTextList.split_by_newline`: two or more `<br>`
/// sentinels split paragraphs, a single one becomes `\n`.
fn split_by_newline(parts: &[Part]) -> Vec<Vec<Part>> {
    let mut out: Vec<Vec<Part>> = Vec::new();
    let mut active: Vec<Part> = Vec::new();
    let double = format!("{BR}{BR}");
    for p in parts {
        if !p.text.contains(BR) {
            active.push(p.clone());
            continue;
        }
        let subs: Vec<&str> = p.text.split(double.as_str()).collect();
        let last = subs.len() - 1;
        for (i, sub) in subs.iter().enumerate() {
            let text = sub.replace(BR, "\n");
            let text = strip_spaces_around_newlines(&text);
            active.push(Part { text, ..p.clone() });
            if i < last {
                out.push(std::mem::take(&mut active));
            }
        }
    }
    if !active.is_empty() {
        out.push(active);
    }
    out
}

/// Python's `re.sub(r" *\n *", "\n", text)`: the spaces on either side of
/// every newline go, the text's own ends are left alone.
fn strip_spaces_around_newlines(text: &str) -> String {
    let segs: Vec<&str> = text.split('\n').collect();
    let last = segs.len() - 1;
    segs.iter()
        .enumerate()
        .map(|(i, seg)| {
            let seg = if i > 0 {
                seg.trim_start_matches(' ')
            } else {
                seg
            };
            if i < last {
                seg.trim_end_matches(' ')
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// upstream's `AnnotatedTextList.to_single_text_element`.
fn to_single(parts: &[Part]) -> Part {
    let mut text = String::new();
    let mut formatting = None;
    let mut hyperlink = None;
    let mut code = false;
    for p in parts {
        text.push_str(p.text.trim());
        text.push(' ');
        if formatting.is_none() {
            formatting = p.formatting;
        }
        if hyperlink.is_none() {
            hyperlink = p.hyperlink.clone();
        }
        code = p.code || code;
    }
    Part {
        text: text.trim().to_string(),
        hyperlink,
        formatting,
        code,
    }
}

/// upstream's `detect_code_language` content fallback (no hint): a shebang,
/// PHP / HTML / Dockerfile markers, `#include`, JSON. Conservative; anything
/// else is `unknown`.
fn detect_code_language(text: &str) -> Option<String> {
    let head = text.trim_start();
    if let Some(rest) = head.strip_prefix("#!") {
        let first = rest.lines().next().unwrap_or("");
        let interp = first
            .rsplit('/')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .next()
            .unwrap_or("");
        let interp = if interp == "env" {
            first.split_whitespace().nth(1).unwrap_or("")
        } else {
            interp
        };
        let lang = match interp.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.') {
            "bash" | "sh" | "zsh" => "Bash",
            "python" => "Python",
            "perl" => "Perl",
            "ruby" => "Ruby",
            "node" => "JavaScript",
            _ => return None,
        };
        return Some(lang.to_string());
    }
    if text.contains("<?php") {
        return Some("PHP".into());
    }
    let lower = text.to_ascii_lowercase();
    if lower.contains("<!doctype html") || lower.contains("<html") {
        return Some("HTML".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use docling_core::tree::TreeItem;

    fn tree(html: &str) -> ItemTree {
        build_tree(&Html::parse_document(html), &super::super::images::NoFetch)
    }

    fn label(it: &TreeItem) -> String {
        match &it.kind {
            TreeKind::Text { label, .. } => label.clone(),
            TreeKind::Code { .. } => "code".into(),
            TreeKind::Group { label, name } => format!("{label}:{name}"),
            TreeKind::Table { .. } => "table".into(),
            TreeKind::Picture { .. } => "picture".into(),
            TreeKind::FieldRegion { .. } => "field_region".into(),
        }
    }

    fn text(it: &TreeItem) -> &str {
        match &it.kind {
            TreeKind::Text { text, .. } | TreeKind::Code { text, .. } => text,
            _ => "",
        }
    }

    /// upstream's part merging: same-annotation neighbours join with a
    /// space (none against a blank part), a change of annotation splits.
    #[test]
    fn simplify_merges_same_annotation_neighbours() {
        let p = |t: &str, bold: bool| Part {
            text: t.into(),
            hyperlink: None,
            formatting: bold.then_some(Formatting {
                bold: true,
                ..Formatting::default()
            }),
            code: false,
        };
        let out = simplify(vec![
            p("a", false),
            p("b", false),
            p("c", true),
            p("\n", true),
            p("d", true),
        ]);
        let texts: Vec<&str> = out.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(texts, ["a b", "c\nd"]);
    }

    /// One `<br>` is a newline inside the paragraph, two start a new one; the
    /// spaces around a newline go, the text's ends stay.
    #[test]
    fn br_sentinels_split_paragraphs() {
        let p = |t: String| Part {
            text: t,
            hyperlink: None,
            formatting: None,
            code: false,
        };
        let lists = split_by_newline(&[p(format!("a {BR} b{BR}{BR}c ")), p("d".into())]);
        let texts: Vec<Vec<&str>> = lists
            .iter()
            .map(|l| l.iter().map(|p| p.text.as_str()).collect())
            .collect();
        assert_eq!(texts, vec![vec!["a\nb"], vec!["c ", "d"]]);
        assert_eq!(strip_spaces_around_newlines("  x  \n  y  "), "  x\ny  ");
    }

    #[test]
    fn hrefs_serialize_like_docling() {
        assert_eq!(docling_href("https://example.com"), "https://example.com/");
        assert_eq!(
            docling_href("https://example.com/a/"),
            "https://example.com/a/"
        );
        assert_eq!(docling_href("mailto:a@b.c"), "mailto:a@b.c");
        assert_eq!(docling_href("#Etymology"), "#Etymology");
        assert_eq!(docling_href("/wiki/Duck/"), "/wiki/Duck");
        assert_eq!(
            docling_href("//wikimediafoundation.org/"),
            "//wikimediafoundation.org"
        );
        assert_eq!(docling_href("./a//b/./c"), "a/b/c");
    }

    #[test]
    fn cell_spans_follow_get_cell_spans() {
        let html = Html::parse_fragment(
            r#"<table><tr><td colspan="3x" rowspan="0">a</td><td rowspan="x2">b</td></tr></table>"#,
        );
        let cells: Vec<ElementRef> = html.select(sel!("td")).collect();
        assert_eq!(cell_spans(cells[0]), (3, 0));
        assert_eq!(cell_spans(cells[1]), (1, 1));
    }

    /// The shape upstream gives a small page: the `<title>` and the prelude
    /// on the furniture layer, the `h1` a title holding everything after it,
    /// a mixed-formatting paragraph an inline group of parts with
    /// `formatting` / `hyperlink`, a plain paragraph a lone text item, a code
    /// span a `code` item, the `h2` a level-1 `section_header` under the
    /// title, a footer a furniture section group.
    #[test]
    fn builds_docling_shape() {
        let html = r#"<html><head><title>Demo &amp; more</title></head><body>
            <nav>Skip to content</nav>
            <h1>Ducks</h1>
            <p>Some <b>bold</b> and a <a href="https://example.com">link</a> and <code>x</code>.</p>
            <p>Plain paragraph.</p>
            <h2>Feeding</h2>
            <p>They eat.</p>
            <footer>© 2026</footer>
        </body></html>"#;
        let t = tree(html);
        let items: Vec<(String, String, Option<usize>, Option<ContentLayer>)> = t
            .items
            .iter()
            .map(|it| (label(it), text(it).to_string(), it.parent, it.layer))
            .collect();
        assert_eq!(
            items[0],
            (
                "title".into(),
                "Demo & more".into(),
                None,
                Some(ContentLayer::Furniture)
            )
        );
        assert_eq!(
            items[1],
            (
                "text".into(),
                "Skip to content".into(),
                None,
                Some(ContentLayer::Furniture)
            )
        );
        assert_eq!(items[2], ("title".into(), "Ducks".into(), None, None));
        assert_eq!(items[3].0, "inline:group");
        assert_eq!(
            items[3].2,
            Some(2),
            "the paragraph's group hangs off the title"
        );
        let parts: Vec<(String, String)> = t.items[2 + 2..2 + 2 + 7]
            .iter()
            .map(|it| (label(it), text(it).to_string()))
            .collect();
        assert_eq!(
            parts,
            [
                ("text", "Some"),
                ("text", "bold"),
                ("text", "and a"),
                ("text", "link"),
                ("text", "and"),
                ("code", "x"),
                ("text", "."),
            ]
            .map(|(a, b)| (a.to_string(), b.to_string()))
        );
        match &t.items[5].kind {
            TreeKind::Text { formatting, .. } => assert_eq!(formatting.map(|f| f.bold), Some(true)),
            other => panic!("{other:?}"),
        }
        match &t.items[7].kind {
            TreeKind::Text { hyperlink, .. } => {
                assert_eq!(hyperlink.as_deref(), Some("https://example.com/"))
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            items[11],
            ("text".into(), "Plain paragraph.".into(), Some(2), None)
        );
        match &t.items[12].kind {
            TreeKind::Text { label, level, .. } => {
                assert_eq!(label, "section_header");
                assert_eq!(*level, Some(1));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            items[13],
            ("text".into(), "They eat.".into(), Some(12), None)
        );
        assert_eq!(
            items[14],
            (
                "section:footer".into(),
                String::new(),
                Some(12),
                Some(ContentLayer::Furniture)
            )
        );
        assert_eq!(
            items[15],
            (
                "text".into(),
                "© 2026".into(),
                Some(14),
                Some(ContentLayer::Furniture)
            )
        );
        assert_eq!(t.body, vec![0, 1, 2]);
        assert_eq!(t.items[2].children, vec![3, 11, 12]);
    }

    /// Lists: a plain `<li>` is one `list_item`; a mixed one is an empty item
    /// over an inline group; an ordered list with `start` names the group and
    /// numbers the markers; a `<dt>` is bold with its `<dd>` in a
    /// `descriptions` group.
    #[test]
    fn lists_follow_docling() {
        let t = tree(
            r#"<body><ol start="3"><li>one</li><li>two <i>it</i></li></ol>
               <dl><dt>Coffee</dt><dd>hot</dd><dd>black</dd></dl></body>"#,
        );
        let labels: Vec<String> = t.items.iter().map(label).collect();
        assert_eq!(
            labels,
            [
                "list:ordered list start 3",
                "list_item",
                "list_item",
                "inline:group",
                "text",
                "text",
                "list:description list",
                "list_item",
                "list:descriptions",
                "list_item",
                "list_item",
            ]
        );
        match &t.items[1].kind {
            TreeKind::Text {
                list: Some(l),
                text,
                ..
            } => {
                assert_eq!(
                    (text.as_str(), l.enumerated, l.marker.as_str()),
                    ("one", true, "3.")
                );
            }
            other => panic!("{other:?}"),
        }
        match &t.items[2].kind {
            TreeKind::Text {
                list: Some(l),
                text,
                ..
            } => {
                assert_eq!((text.as_str(), l.marker.as_str()), ("", "4."));
            }
            other => panic!("{other:?}"),
        }
        match &t.items[7].kind {
            TreeKind::Text {
                formatting,
                list: Some(l),
                ..
            } => {
                assert_eq!(formatting.map(|f| f.bold), Some(true));
                assert_eq!(l.marker, "");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(t.items[8].parent, Some(7));
        assert_eq!(t.items[8].children, vec![9, 10]);
    }

    /// A rich cell: its items are walked under the current parent, then
    /// re-parented into a `rich_cell_group` of the table named with
    /// upstream's lazily advanced column (the previous cell's); the cell's
    /// text is the raw `get_text`; a heading inside a cell resets the stack.
    #[test]
    fn rich_cells_group_under_the_table() {
        let t = tree(
            r#"<body><h1>T</h1><table><tr><td>plain</td><td><b>Large</b>, loud</td></tr></table></body>"#,
        );
        let labels: Vec<String> = t.items.iter().map(label).collect();
        assert_eq!(
            labels,
            [
                "title",
                "table",
                "inline:group",
                "text",
                "text",
                "unspecified:rich_cell_group_1_0_0"
            ]
        );
        assert_eq!(t.items[1].parent, Some(0));
        assert_eq!(t.items[1].children, vec![5]);
        assert_eq!(t.items[5].children, vec![2]);
        assert_eq!(t.items[2].parent, Some(5));
        match &t.items[1].kind {
            TreeKind::Table {
                table, rich_cells, ..
            } => {
                assert_eq!(rich_cells, &vec![(0, 1, 5)]);
                let cells = table.cells.as_ref().unwrap();
                assert_eq!(cells[1].text, "Large, loud");
                assert_eq!((cells[1].start_row, cells[1].start_col), (0, 1));
            }
            other => panic!("{other:?}"),
        }
        assert!(t.items[0].children.contains(&1));
    }
}
