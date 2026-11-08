//! docling's item tree for a DocLang document — a call-for-call port of
//! docling-core's `DocLangDocDeserializer` (`transforms/deserializer/
//! doclang.py`), the parser both `DocLangDocumentBackend` (`.dclg`) and
//! `DocLangArchiveBackend` (`.dclx`) delegate to.
//!
//! The flat walk in [`super::doclang`] keeps producing the Markdown-tuned
//! node stream; this module rebuilds, on the same DOM, the exact parent/child
//! structure and creation order the JSON export must show: an inline group
//! for every mixed `<text>` (an empty `<text/>` is an empty text item), a
//! host item over that group for a located `<text>` and any mixed caption,
//! footnote or page header/footer (docling-core#865), one text item
//! per styled run with its `Formatting`, list items whose body is a
//! `<text>` wrap vs. bare *virtual text*, rich table cells parented to an
//! `unspecified` group under the table, captions and footnotes on the body,
//! `<location>` quartets as `prov` on the 512-unit page grid, and a page per
//! `<page_break>`. Whitespace, entity and CDATA handling follow minidom's
//! view of the document — a CDATA section is character data like any other,
//! stripped unless it sits in a `<content>` wrapper.

use roxmltree::Node as XmlNode;

use crate::backend::ooxml::Package;
use docling_core::tree::{Formatting, ItemTree, ListMeta, TreeKind, TreeProv};
use docling_core::{ContentLayer, PictureImage, Script, Table, TableCell};

/// docling-core's `DOCLANG_DFLT_RESOLUTION`: the square page every DocLang
/// document is laid out on.
pub(super) const PAGE_SIZE: f32 = 512.0;

/// `_ELEMENT_HEAD_TAGS`: the property tokens an element head may hold.
fn is_head_tag(name: &str) -> bool {
    matches!(
        name,
        "label"
            | "layer"
            | "href"
            | "location"
            | "caption"
            | "description"
            | "summary"
            | "custom"
            | "thread"
            | "xref"
            | "hour"
            | "minute"
            | "second"
            | "centisecond"
    )
}

/// The character-data nodes minidom sees where roxmltree has one merged
/// text node: a CDATA section is its own `CDATASection` node there, so
/// `<code>\n<![CDATA[x]]>  </code>` is three nodes — two blank ones the
/// deserializer skips and the verbatim `x` — while roxmltree hands back one
/// `"\nx  "`. Re-split on the source's CDATA markers (plain segments
/// unescaped, CDATA segments verbatim).
fn pieces(n: XmlNode) -> Vec<String> {
    // roxmltree keeps only the first piece's range on a merged node: the
    // merged text runs from there to the next sibling (or the parent's end
    // tag).
    let input = n.document().input_text();
    let start = n.range().start;
    let end = match n.next_sibling() {
        Some(sib) => sib.range().start,
        None => {
            let parent_end = n.parent().map_or(input.len(), |p| p.range().end);
            input[start..parent_end]
                .rfind("</")
                .map_or(parent_end, |p| start + p)
        }
    };
    let src = &input[start..end.max(start)];
    if !src.contains("<![CDATA[") {
        return vec![n.text().unwrap_or("").to_string()];
    }
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(p) = rest.find("<![CDATA[") {
        if p > 0 {
            out.push(unescape_xml(&rest[..p]));
        }
        let after = &rest[p + "<![CDATA[".len()..];
        let e = after.find("]]>").unwrap_or(after.len());
        out.push(after[..e].to_string());
        rest = &after[(e + "]]>".len()).min(after.len())..];
    }
    if !rest.is_empty() {
        out.push(unescape_xml(rest));
    }
    out
}

/// The predefined and numeric character references of a plain text segment.
fn unescape_xml(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let after = &rest[p..];
        match after.find(';') {
            Some(e) => {
                let ent = &after[1..e];
                let decoded = match ent {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ => ent
                        .strip_prefix('#')
                        .and_then(|num| match num.strip_prefix('x') {
                            Some(hex) => u32::from_str_radix(hex, 16).ok(),
                            None => num.parse().ok(),
                        })
                        .and_then(char::from_u32),
                };
                match decoded {
                    Some(c) => out.push(c),
                    None => out.push_str(&after[..=e]),
                }
                rest = &after[e + 1..];
            }
            None => {
                out.push_str(after);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn is_ws_text(n: XmlNode) -> bool {
    n.is_text() && n.text().unwrap_or("").trim().is_empty()
}

fn has_tag(n: XmlNode, name: &str) -> bool {
    n.is_element() && n.tag_name().name() == name
}

fn first_child<'a, 'i>(el: XmlNode<'a, 'i>, name: &str) -> Option<XmlNode<'a, 'i>> {
    el.children().find(|c| has_tag(*c, name))
}

/// `_split_element_children_head_body`: the leading run of whitespace and
/// head tokens, then everything else.
fn split_head_body<'a, 'i>(el: XmlNode<'a, 'i>) -> (Vec<XmlNode<'a, 'i>>, Vec<XmlNode<'a, 'i>>) {
    let mut head = Vec::new();
    let mut body = Vec::new();
    let mut in_body = false;
    for n in el.children() {
        if !in_body {
            if is_ws_text(n) || (n.is_element() && is_head_tag(n.tag_name().name())) {
                head.push(n);
                continue;
            }
            in_body = true;
        }
        body.push(n);
    }
    (head, body)
}

/// `_get_text` (`_trimmed_text`): the element's text, trimmed at the
/// block's outer edges only — the reading rules already gave every text node
/// its reading, so the inner boundaries stay (`Advanced <bold>Topics</bold>`
/// is not `AdvancedTopics`); `<content>` text is protected from the trim.
fn get_text(el: XmlNode) -> String {
    let mut frags = Vec::new();
    fragments(el, &mut frags);
    while let Some((text, false)) = frags.first() {
        let t = text.trim_start_matches(XML_WS);
        if t.is_empty() {
            frags.remove(0);
        } else {
            frags[0].0 = t.to_string();
            break;
        }
    }
    while let Some((text, false)) = frags.last() {
        let t = text.trim_end_matches(XML_WS);
        if t.is_empty() {
            frags.pop();
        } else {
            let last = frags.len() - 1;
            frags[last].0 = t.to_string();
            break;
        }
    }
    frags.into_iter().map(|(t, _)| t).collect()
}

/// `_collect_text`: the element's text verbatim (a run's edges are its own).
fn collect_text(el: XmlNode) -> String {
    let mut frags = Vec::new();
    fragments(el, &mut frags);
    frags.into_iter().map(|(t, _)| t).collect()
}

/// `_XML_WHITESPACE`.
const XML_WS: &[char] = &[' ', '\t', '\r', '\n'];

/// `_collect_fragments`: `(text, protected)` — the text nodes in order,
/// `<content>`'s protected, `<br/>` a newline, element heads skipped.
fn fragments(el: XmlNode, out: &mut Vec<(String, bool)>) {
    let protected = el.tag_name().name() == "content";
    for n in el.children() {
        if n.is_text() {
            out.push((n.text().unwrap_or("").to_string(), protected));
        } else if n.is_element() {
            match n.tag_name().name() {
                t if is_head_tag(t) => {}
                "br" => out.push(("\n".to_string(), false)),
                _ => fragments(n, out),
            }
        }
    }
}

/// `_is_empty_text_block`: no text and no children but element-head tokens.
fn is_empty_text_block(el: XmlNode) -> bool {
    el.children().all(|n| {
        if n.is_element() {
            is_head_tag(n.tag_name().name())
        } else {
            !n.is_text() || pieces(n).iter().all(|d| d.trim().is_empty())
        }
    })
}

/// `_get_children_simple_text_block`: the one text run a "simple" text
/// element holds — `None` once a second run (plain or styled), a non-inline
/// child, or a styled child that is itself mixed appears: the element is
/// then an inline group (docling-core 2.102 — `foo <bold>bar</bold>` used to
/// read `bar`, the styled run replacing the text before it).
fn simple_text_block(el: XmlNode) -> Option<String> {
    let mut result: Option<String> = None;
    for n in el.children() {
        if n.is_element() {
            let name = n.tag_name().name();
            if is_head_tag(name) {
                continue;
            }
            if !matches!(
                name,
                "location"
                    | "layer"
                    | "label"
                    | "br"
                    | "bold"
                    | "italic"
                    | "underline"
                    | "strikethrough"
                    | "subscript"
                    | "superscript"
                    | "rtl"
                    | "handwriting"
                    | "content"
            ) {
                return None;
            }
            match simple_text_block(n).filter(|t| !t.is_empty()) {
                Some(tmp) => {
                    if result.is_some() {
                        return None;
                    }
                    result = Some(tmp);
                }
                None if !get_text(n).trim().is_empty() => return None,
                None => {}
            }
        } else if n.is_text() {
            for data in pieces(n) {
                if data.trim().is_empty() {
                    continue;
                }
                if result.is_none() {
                    result = Some(if el.tag_name().name() == "content" {
                        data
                    } else {
                        data.trim().to_string()
                    });
                } else {
                    return None;
                }
            }
        }
    }
    result
}

/// `_extract_text_with_formatting`: a single styled child with no text
/// beside it (nested styles accumulate) gives the item its `Formatting`;
/// anything else is plain — trimmed at the block's edges for a block host
/// (`trim`), verbatim for a run inside an inline group.
fn text_with_formatting(el: XmlNode, trim: bool) -> (String, Option<Formatting>) {
    let children: Vec<XmlNode> = el
        .children()
        .filter(|c| c.is_element() && !is_head_tag(c.tag_name().name()))
        .collect();
    if let [child] = children[..] {
        let tag = child.tag_name().name();
        let text_beside = el
            .children()
            .any(|n| n.is_text() && !n.text().unwrap_or("").is_empty());
        if !text_beside
            && matches!(
                tag,
                "bold"
                    | "italic"
                    | "strikethrough"
                    | "underline"
                    | "superscript"
                    | "subscript"
                    | "rtl"
            )
        {
            let (text, fmt) = text_with_formatting(child, trim);
            let mut fmt = fmt.unwrap_or_default();
            apply_style(&mut fmt, tag);
            return (text, Some(fmt));
        }
    }
    let text = if trim { get_text(el) } else { collect_text(el) };
    (text, None)
}

/// `_plain_run_text`: a text node's or a `<content>`'s text, `None` for
/// anything else.
fn plain_run_text(n: XmlNode) -> Option<String> {
    if n.is_text() {
        Some(n.text().unwrap_or("").to_string())
    } else if has_tag(n, "content") {
        Some(collect_text(n))
    } else {
        None
    }
}

/// `_checkbox_text_block`: the checkbox and its text when the body of `el`
/// holds nothing else — the plain run right after the symbol, verbatim
/// (docling-core#955: `<checkbox/> y` is `" y"`, `<checkbox/>y` is `"y"`).
fn checkbox_text_block<'a, 'i>(el: XmlNode<'a, 'i>) -> Option<(XmlNode<'a, 'i>, String)> {
    let (_, body) = split_head_body(el);
    let nodes: Vec<XmlNode> = body
        .into_iter()
        .filter(|n| n.is_element() || (n.is_text() && !n.text().unwrap_or("").is_empty()))
        .collect();
    let first = *nodes.first().filter(|n| has_tag(**n, "checkbox"))?;
    match nodes[..] {
        [_] => Some((first, String::new())),
        [_, run] => plain_run_text(run).map(|t| (first, t)),
        _ => None,
    }
}

fn checkbox_label(el: XmlNode) -> &'static str {
    if el.attribute("class") == Some("selected") {
        "checkbox_selected"
    } else {
        "checkbox_unselected"
    }
}

fn apply_style(fmt: &mut Formatting, tag: &str) {
    match tag {
        "bold" => fmt.bold = true,
        "italic" => fmt.italic = true,
        "strikethrough" => fmt.strikethrough = true,
        "underline" => fmt.underline = true,
        "superscript" => fmt.script = Script::Super,
        "subscript" => fmt.script = Script::Sub,
        _ => {}
    }
}

fn layer_from_nodes(nodes: &[XmlNode]) -> Option<ContentLayer> {
    nodes
        .iter()
        .find(|n| has_tag(**n, "layer"))
        .and_then(|n| match n.attribute("value") {
            Some("furniture") => Some(ContentLayer::Furniture),
            Some("notes") => Some(ContentLayer::Notes),
            Some("invisible") => Some(ContentLayer::Invisible),
            Some("background") => Some(ContentLayer::Background),
            _ => None,
        })
}

fn label_from_nodes<'a>(nodes: &[XmlNode<'a, '_>]) -> Option<&'a str> {
    nodes
        .iter()
        .find(|n| has_tag(**n, "label"))
        .and_then(|n| n.attribute("value"))
        .filter(|v| !v.is_empty())
}

fn layer_of(el: XmlNode) -> Option<ContentLayer> {
    layer_from_nodes(&split_head_body(el).0)
}

/// `_code_language_label_from_doclang`: the DocLang (Linguist) label back
/// to docling's `CodeLanguageLabel` value — the inverse of the serializer's
/// `code_lang_label` — for the export's case-insensitive lookup.
fn code_language(label: &str) -> Option<String> {
    let mapped = match label {
        "other" | "undefined" | "unknown" => return None,
        "Shell" => "Bash",
        "Fortran" => "FORTRAN",
        "TeX" => "Latex",
        "Common Lisp" => "Lisp",
        "MATLAB" => "Matlab",
        "Objective-C" => "ObjectiveC",
        "Standard ML" => "SML",
        "Visual Basic .NET" => "VisualBasic",
        other => other,
    };
    Some(mapped.to_string())
}

/// Tokens the OTSL grammar spells a cell or row with.
fn otsl_token<'a>(n: XmlNode<'a, '_>) -> Option<&'a str> {
    if !n.is_element() {
        return None;
    }
    let name = n.tag_name().name();
    matches!(
        name,
        "fcel" | "ecel" | "lcel" | "ucel" | "xcel" | "nl" | "ched" | "rhed" | "srow" | "corn"
    )
    .then_some(name)
}

/// One fragment of an OTSL body after `_nodes_to_xml`'s reparse: stripped
/// character data, or an element (a token, a `<location>`, or cell content).
enum Part<'a, 'i> {
    Text(XmlNode<'a, 'i>),
    El(XmlNode<'a, 'i>),
}

/// `_otsl_extract_tokens_and_text` on the body nodes: every non-empty text
/// node (already read per the reading rules; an unwrapped `<content>` stays
/// literal) and every element, a `<content>` included, as a part.
fn otsl_parts<'a, 'i>(nodes: &[XmlNode<'a, 'i>], out: &mut Vec<Part<'a, 'i>>) {
    for n in nodes {
        if n.is_text() {
            if !n.text().unwrap_or("").is_empty() {
                out.push(Part::Text(*n));
            }
        } else if n.is_element() {
            out.push(Part::El(*n));
        }
    }
}

/// A parsed OTSL cell: the docling `TableCell` plus, for a rich cell (one
/// holding an element), its body nodes — an inline group's runs when they
/// are virtual text, else blocks under an `unspecified` group.
struct OtslCell<'a, 'i> {
    cell: TableCell,
    rich: Vec<XmlNode<'a, 'i>>,
}

pub(super) struct Builder<'p> {
    tree: ItemTree,
    page_no: usize,
    pages: usize,
    /// The `.dclx` package, when reading an archive: `<src uri>` resolves to
    /// its assets (docling's `media_root`); a bare `.dclg` keeps no image.
    pkg: Option<&'p mut Package>,
}

/// Build the tree for `root` (the `<doclang>` element). Returns the tree and
/// the number of pages (one plus the `<page_break>`s reached).
pub(super) fn build(root: XmlNode, pkg: Option<&mut Package>) -> (ItemTree, usize) {
    let mut b = Builder {
        tree: ItemTree::default(),
        page_no: 1,
        pages: 1,
        pkg,
    };
    for el in root.children().filter(|n| n.is_element()) {
        b.dispatch(el, None);
    }
    (b.tree, b.pages)
}

impl Builder<'_> {
    // ------------- provenance -------------

    /// `_provenance_from_location_nodes`: every quartet of `<location>`
    /// values among `nodes` is one box on the current page (`charspan`
    /// `[0, 0]` until a text item applies its length).
    fn provs_from_nodes(&self, nodes: &[XmlNode]) -> Vec<TreeProv> {
        let mut values: Vec<i64> = Vec::new();
        let mut provs = Vec::new();
        for n in nodes.iter().filter(|n| has_tag(**n, "location")) {
            let v = n
                .attribute("value")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            values.push(v);
            if values.len() == 4 {
                provs.push(TreeProv {
                    page_no: self.page_no,
                    bbox: [
                        values[0].min(values[2]) as f64,
                        values[1].min(values[3]) as f64,
                        values[0].max(values[2]) as f64,
                        values[1].max(values[3]) as f64,
                    ],
                    bottom_left: false,
                    charspan: [0, 0],
                });
                values.clear();
            }
        }
        provs
    }

    /// `_extract_provenance`: the element head's location quartets.
    fn provenance(&self, el: XmlNode) -> Vec<TreeProv> {
        self.provs_from_nodes(&split_head_body(el).0)
    }

    /// `add_text` with `_apply_initial_text_provenance`: the first box gets
    /// `charspan = (0, len(text))` (a Python `len`, in code points).
    fn add_text_item(
        &mut self,
        parent: Option<usize>,
        layer: Option<ContentLayer>,
        kind: TreeKind,
        provs: Vec<TreeProv>,
        text_len: Option<usize>,
    ) -> usize {
        match provs.into_iter().next() {
            Some(mut prov) => {
                if let Some(len) = text_len {
                    prov.charspan = [0, len];
                }
                self.tree.add_with_prov(parent, layer, kind, prov)
            }
            None => self.tree.add(parent, layer, kind),
        }
    }

    fn text_kind(label: &str, text: &str, formatting: Option<Formatting>) -> TreeKind {
        TreeKind::Text {
            label: label.into(),
            text: text.into(),
            orig: None,
            formatting,
            hyperlink: None,
            level: None,
            list: None,
        }
    }

    // ------------- core walkers -------------

    /// `_dispatch_element`.
    fn dispatch(&mut self, el: XmlNode, parent: Option<usize>) {
        match el.tag_name().name() {
            // docling-core#880: a media track is not supported yet — skipped
            // with all its content (its captions would read as body text).
            "track" => {
                eprintln!(
                    "warning: doclang: <track> is not supported yet; skipped with all its content"
                )
            }
            "text" | "caption" | "footnote" | "page_header" | "page_footer" | "code"
            | "formula" | "ldiv" | "bold" | "italic" | "underline" | "strikethrough"
            | "subscript" | "superscript" | "content" => self.text_like(el, parent),
            "page_break" => {
                self.page_no += 1;
                self.pages = self.pages.max(self.page_no);
            }
            "heading" => self.heading(el, parent),
            "field_region" => self.field_region(el, parent),
            "checkbox" => {
                self.tree
                    .add(parent, None, Self::text_kind(checkbox_label(el), "", None));
            }
            "list" => self.list(el, parent),
            "group" => {
                // A named group is a GroupItem; the nameless float+footnote
                // wrapper parses as one unit; anything else is transparent.
                if let Some(name) = el.attribute("name").filter(|n| !n.is_empty()) {
                    self.named_group(el, parent, name);
                } else if let Some(host) = float_group_host(el) {
                    match host.tag_name().name() {
                        "picture" => self.picture(el, parent),
                        "code" => self.code_group(el, host, parent),
                        _ => self.table(el, parent),
                    }
                } else {
                    self.walk_children(el, parent);
                }
            }
            "table" | "index" => self.table(el, parent),
            "picture" => self.picture(el, parent),
            _ => self.walk_children(el, parent),
        }
    }

    /// `_parse_named_group`: `<group name="…">` with an optional
    /// `<label value>` head (a valid `GroupLabel`, else `unspecified`).
    fn named_group(&mut self, el: XmlNode, parent: Option<usize>, name: &str) {
        let heads: Vec<XmlNode> = el
            .children()
            .filter(|c| c.is_element() && is_head_tag(c.tag_name().name()))
            .collect();
        let label = label_from_nodes(&heads)
            .filter(|l| {
                matches!(
                    *l,
                    "unspecified"
                        | "list"
                        | "ordered_list"
                        | "chapter"
                        | "section"
                        | "sheet"
                        | "slide"
                        | "form_area"
                        | "key_value_area"
                        | "comment_section"
                        | "inline"
                        | "picture_area"
                )
            })
            .unwrap_or("unspecified");
        let g = self.tree.add(
            parent,
            None,
            TreeKind::Group {
                label: label.into(),
                name: name.into(),
            },
        );
        self.walk_children(el, Some(g));
    }

    /// `_walk_children`: dispatch child elements, skipping the geometry /
    /// meta containers (a caption is read by its float, never on its own).
    fn walk_children(&mut self, el: XmlNode, parent: Option<usize>) {
        for n in el.children().filter(|n| n.is_element()) {
            if matches!(
                n.tag_name().name(),
                "head" | "location" | "layer" | "label" | "custom" | "caption" | "src"
            ) {
                continue;
            }
            self.dispatch(n, parent);
        }
    }

    // ------------- text blocks -------------

    /// `_parse_text_like`.
    fn text_like(&mut self, el: XmlNode, parent: Option<usize>) {
        let name = el.tag_name().name();
        let element_children: Vec<XmlNode> = el
            .children()
            .filter(|c| c.is_element() && !is_head_tag(c.tag_name().name()))
            .collect();
        let simple = simple_text_block(el);
        // An element without any content is an empty item, not a childless
        // inline group (docling-core 2.102's `_is_empty_text_block`).
        let is_empty = simple.is_none() && is_empty_text_block(el);
        let checkbox = if name == "text" {
            checkbox_text_block(el)
        } else {
            None
        };
        if checkbox.is_none() && !is_empty && (element_children.len() > 1 || simple.is_none()) {
            // `_parse_mixed_text_like` (docling-core#865): a mixed caption,
            // footnote or page header/footer — or a `<text>` with a location
            // — is a host item of its own label, provenance and layer over
            // an inline group; any other mixed element is the bare group,
            // its runs on the element's layer.
            if matches!(name, "caption" | "footnote" | "page_header" | "page_footer")
                || (name == "text" && !self.provenance(el).is_empty())
            {
                self.inline_host(el, parent, name);
            } else {
                let group = self.inline_group(el, parent, None);
                if let Some(layer) = layer_of(el) {
                    self.set_layer_below(group, layer);
                }
            }
            return;
        }
        let provs = self.provenance(el);
        let layer = layer_of(el);
        // A run inside an inline group owns its edge whitespace; a block
        // host does not.
        let in_group = parent.is_some_and(|p| {
            matches!(&self.tree.items[p].kind, TreeKind::Group { label, .. } if label == "inline")
        });
        let (text, mut formatting) = match &checkbox {
            Some((_, t)) => (t.clone(), None),
            None => text_with_formatting(el, !in_group),
        };
        if text.is_empty() && !is_empty && checkbox.is_none() {
            return;
        }
        match name {
            "code" => {
                let (code, lang) = code_content(el);
                if code.trim().is_empty() {
                    return;
                }
                let len = code.chars().count();
                let item = self.add_text_item(
                    parent,
                    layer,
                    TreeKind::Code {
                        text: code,
                        orig: None,
                        language: lang,
                        formatting: None,
                        hyperlink: None,
                        captions: Vec::new(),
                    },
                    provs,
                    Some(len),
                );
                // The head's captions, created after the code item.
                let caps = self.extract_captions(el);
                if let TreeKind::Code { captions, .. } = &mut self.tree.items[item].kind {
                    *captions = caps;
                }
            }
            "formula" => {
                let len = text.chars().count();
                self.add_text_item(
                    parent,
                    None,
                    Self::text_kind("formula", &text, formatting),
                    provs,
                    Some(len),
                );
            }
            "ldiv" => {}
            _ => {
                let mut label = match name {
                    "caption" => "caption",
                    "footnote" => "footnote",
                    "page_header" => "page_header",
                    "page_footer" => "page_footer",
                    _ => "text",
                };
                if matches!(
                    name,
                    "bold" | "italic" | "underline" | "strikethrough" | "subscript" | "superscript"
                ) {
                    let mut fmt = formatting.unwrap_or_default();
                    apply_style(&mut fmt, name);
                    formatting = Some(fmt);
                }
                if let Some((cb, _)) = checkbox {
                    label = checkbox_label(cb);
                } else if name == "text"
                    && element_children
                        .iter()
                        .any(|c| c.tag_name().name() == "handwriting")
                {
                    label = "handwritten_text";
                }
                let len = text.chars().count();
                self.add_text_item(
                    parent,
                    layer,
                    Self::text_kind(label, &text, formatting),
                    provs,
                    Some(len),
                );
            }
        }
    }

    /// `_parse_heading`: level 1 is the `title`, deeper levels a
    /// `section_header` one level up.
    fn heading(&mut self, el: XmlNode, parent: Option<usize>) {
        let level: i64 = el
            .attribute("level")
            .filter(|v| !v.is_empty())
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1);
        let provs = self.provenance(el);
        let layer = layer_of(el);
        let text = get_text(el);
        if text.is_empty() {
            return;
        }
        let kind = if level <= 1 {
            Self::text_kind("title", &text, None)
        } else {
            TreeKind::Text {
                label: "section_header".into(),
                text: text.clone(),
                orig: None,
                formatting: None,
                hyperlink: None,
                level: Some((level - 1).min(u8::MAX as i64) as u8),
                list: None,
            }
        };
        let len = text.chars().count();
        self.add_text_item(parent, layer, kind, provs, Some(len));
    }

    /// `_parse_field_region` + `_parse_field_item` + `_parse_field_kv` for
    /// the shape the tree model holds: a region of items, each with a key
    /// and a value (the `<marker>` has no dispatch rule upstream and is
    /// dropped). Children of another shape are not represented.
    fn field_region(&mut self, el: XmlNode, parent: Option<usize>) {
        let provs = self.provenance(el);
        let mut items = Vec::new();
        for item in el.children().filter(|c| has_tag(*c, "field_item")) {
            let part = |name: &str| {
                first_child(item, name).map(|kv| {
                    let text = text_with_formatting(kv, true).0;
                    let kind = (name == "value").then(|| {
                        if kv.attribute("class") == Some("fillable") {
                            "fillable"
                        } else {
                            "read_only"
                        }
                        .to_string()
                    });
                    (text, kind)
                })
            };
            let key = part("key");
            let value = part("value");
            items.push(docling_core::FieldItem {
                marker: None,
                key: key.map(|k| k.0),
                value_kind: value.as_ref().and_then(|v| v.1.clone()),
                value: value.map(|v| v.0),
            });
        }
        self.add_text_item(parent, None, TreeKind::FieldRegion { items }, provs, None);
    }

    // ------------- lists -------------

    /// `_parse_list`: each `<ldiv/>` opens an item whose body runs to the
    /// next delimiter; the body's shape decides between a plain item, an
    /// item whose text is a `<text>` wrap, and an empty item holding the
    /// dispatched content.
    fn list(&mut self, el: XmlNode, parent: Option<usize>) {
        let ordered = el.attribute("class") == Some("ordered");
        let group = self.tree.add(
            parent,
            None,
            TreeKind::Group {
                label: "list".into(),
                name: "group".into(),
            },
        );
        let all: Vec<XmlNode> = el.children().collect();
        let ldivs: Vec<usize> = (0..all.len())
            .filter(|&i| has_tag(all[i], "ldiv"))
            .collect();
        for (k, &start) in ldivs.iter().enumerate() {
            let end = ldivs.get(k + 1).copied().unwrap_or(all.len());
            let marker = first_child(all[start], "marker")
                .map(|m| get_text(m).trim().to_string())
                .unwrap_or_default();
            let meta = ListMeta {
                enumerated: ordered,
                marker,
            };
            let content = &all[start + 1..end];
            let content_elements: Vec<XmlNode> = content
                .iter()
                .copied()
                .filter(|n| n.is_element() && !is_head_tag(n.tag_name().name()))
                .collect();
            if content.is_empty() {
                self.add_list_item(group, "", &meta, Vec::new());
            } else if is_list_item_virtual_text(content) {
                self.list_item_virtual_text(el, group, &meta, content);
            } else if content_elements.len() == 1 {
                let content_el = content_elements[0];
                if content_el.tag_name().name() == "text" {
                    if text_wrap_is_complex(content_el) {
                        let li = self.add_list_item(group, "", &meta, Vec::new());
                        self.dispatch(content_el, Some(li));
                    } else {
                        let provs = self.provenance(content_el);
                        let text = get_text(content_el).trim().to_string();
                        self.add_list_item(group, &text, &meta, provs);
                    }
                } else {
                    let li = self.add_list_item(group, "", &meta, Vec::new());
                    self.dispatch(content_el, Some(li));
                }
            } else if content_elements.is_empty() {
                // Only whitespace after the delimiter (upstream indexes past
                // the end here): an empty item.
                self.add_list_item(group, "", &meta, Vec::new());
            } else {
                let first = content_elements[0];
                let remaining = &content_elements[1..];
                // A simple `<text>` followed only by nested lists / pictures
                // is the item's own text (the collapsed representation).
                if first.tag_name().name() == "text"
                    && remaining
                        .iter()
                        .all(|e| matches!(e.tag_name().name(), "list" | "picture"))
                    && !text_wrap_is_complex(first)
                {
                    let provs = self.provenance(first);
                    let text = get_text(first).trim().to_string();
                    let li = self.add_list_item(group, &text, &meta, provs);
                    for e in remaining {
                        self.dispatch(*e, Some(li));
                    }
                } else {
                    let li = self.add_list_item(group, "", &meta, Vec::new());
                    for e in &content_elements {
                        self.dispatch(*e, Some(li));
                    }
                }
            }
        }
    }

    fn add_list_item(
        &mut self,
        group: usize,
        text: &str,
        meta: &ListMeta,
        provs: Vec<TreeProv>,
    ) -> usize {
        let len = text.chars().count();
        self.add_text_item(
            Some(group),
            None,
            TreeKind::Text {
                label: "list_item".into(),
                text: text.into(),
                orig: None,
                formatting: None,
                hyperlink: None,
                level: None,
                list: Some(meta.clone()),
            },
            provs,
            Some(len),
        )
    }

    /// `_parse_list_item_virtual_text`: the body after `<ldiv/>` is bare
    /// text (with inline markup) rather than a `<text>` wrap.
    fn list_item_virtual_text(
        &mut self,
        list_el: XmlNode,
        group: usize,
        meta: &ListMeta,
        content: &[XmlNode],
    ) {
        let provs = self.provs_from_nodes(content);
        // Drop the leading whitespace / head tokens.
        let mut body: Vec<XmlNode> = Vec::new();
        let mut skipping = true;
        for n in content {
            if skipping && (is_ws_text(*n) || (n.is_element() && is_head_tag(n.tag_name().name())))
            {
                continue;
            }
            skipping = false;
            body.push(*n);
        }
        // Leading plain text (text nodes and `<content>` wrappers), then the rest.
        let mut leading = String::new();
        let mut rest_start = 0;
        for (i, n) in body.iter().enumerate() {
            if n.is_text() {
                leading.push_str(n.text().unwrap_or(""));
                rest_start = i + 1;
            } else if has_tag(*n, "content") {
                leading.push_str(&get_text(*n));
                rest_start = i + 1;
            } else {
                break;
            }
        }
        // Already read per the reading rules: what edge whitespace is left
        // came from a literal `<content>`.
        let rest: Vec<XmlNode> = body[rest_start..]
            .iter()
            .copied()
            .filter(|n| !is_ws_text(*n))
            .collect();
        let rest_elements: Vec<XmlNode> = rest.iter().copied().filter(|n| n.is_element()).collect();

        if !leading.is_empty()
            && !rest_elements.is_empty()
            && rest_elements
                .iter()
                .all(|e| matches!(e.tag_name().name(), "list" | "picture"))
        {
            let li = self.add_list_item(group, &leading, meta, provs);
            for e in &rest_elements {
                self.dispatch(*e, Some(li));
            }
        } else if rest.is_empty() && !leading.is_empty() {
            self.add_list_item(group, &leading, meta, provs);
        } else if is_simple_virtual_text(&body) {
            let mut text = String::new();
            for n in &body {
                if n.is_text() {
                    text.push_str(n.text().unwrap_or(""));
                } else if has_tag(*n, "content") {
                    text.push_str(&get_text(*n));
                }
            }
            self.add_list_item(group, &text, meta, provs);
        } else {
            let li = self.add_list_item(group, "", meta, provs);
            self.inline_group(list_el, Some(li), Some(body));
        }
    }

    // ------------- inline groups -------------

    /// `_parse_inline_group`: an `inline` group holding one item per child
    /// element and per non-blank text node. `nodes` overrides the
    /// element's children (a list item's virtual-text body); an empty
    /// override falls back to the children, as upstream's `nodes or …` does.
    fn inline_group(
        &mut self,
        el: XmlNode,
        parent: Option<usize>,
        nodes: Option<Vec<XmlNode>>,
    ) -> usize {
        let g = self.tree.add(
            parent,
            None,
            TreeKind::Group {
                label: "inline".into(),
                name: "group".into(),
            },
        );
        let nodes = match nodes {
            Some(v) if !v.is_empty() => v,
            _ => el.children().collect(),
        };
        // DocLang 0.9's reading rule (docling-core#693): a whitespace
        // sequence in bare text reads as one space, kept on its run as the
        // boundary with the next one, none at the block's edges. Metadata
        // elements (`<location>`, `<layer>`) take no part in the edges.
        enum Run<'a, 'i> {
            Text(String),
            El(XmlNode<'a, 'i>),
        }
        let mut runs: Vec<Run> = Vec::new();
        for n in nodes {
            if n.is_element() {
                if matches!(n.tag_name().name(), "location" | "layer") {
                    self.dispatch(n, Some(g));
                } else {
                    runs.push(Run::El(n));
                }
            } else if n.is_text() {
                let data: String = pieces(n).concat();
                let mut t = String::with_capacity(data.len());
                let mut ws = false;
                for c in data.chars() {
                    if matches!(c, ' ' | '\t' | '\n' | '\r') {
                        if !ws {
                            t.push(' ');
                        }
                        ws = true;
                    } else {
                        t.push(c);
                        ws = false;
                    }
                }
                if !t.is_empty() {
                    runs.push(Run::Text(t));
                }
            }
        }
        // The block's edges: blank runs there go, the outer runs are trimmed.
        while matches!(runs.first(), Some(Run::Text(t)) if t.trim().is_empty()) {
            runs.remove(0);
        }
        while matches!(runs.last(), Some(Run::Text(t)) if t.trim().is_empty()) {
            runs.pop();
        }
        if let Some(Run::Text(t)) = runs.first_mut() {
            *t = t.trim_start().to_string();
        }
        if let Some(Run::Text(t)) = runs.last_mut() {
            *t = t.trim_end().to_string();
        }
        // `_parse_inline_runs`: a checkbox takes the plain run right after
        // it as its text (docling-core#955), verbatim.
        let mut runs = runs.into_iter().peekable();
        while let Some(run) = runs.next() {
            match run {
                Run::El(n) if has_tag(n, "checkbox") => {
                    let text = match runs.peek() {
                        Some(Run::Text(t)) => Some(t.clone()),
                        Some(Run::El(c)) if has_tag(*c, "content") => Some(collect_text(*c)),
                        _ => None,
                    };
                    if text.is_some() {
                        runs.next();
                    }
                    let label = checkbox_label(n);
                    self.tree.add(
                        Some(g),
                        None,
                        Self::text_kind(label, &text.unwrap_or_default(), None),
                    );
                }
                Run::El(n) => self.dispatch(n, Some(g)),
                Run::Text(t) => {
                    self.tree
                        .add(Some(g), None, Self::text_kind("text", &t, None));
                }
            }
        }
        g
    }

    /// `_parse_inline_host` (docling-core#865): a labeled text-like element
    /// with mixed content is an empty item of that label — its provenance
    /// (charspan `[0, 0]`) and layer kept — holding an inline group of the
    /// body's runs, which take the host's layer. The serializer writes that
    /// shape back as `<footnote>…runs…</footnote>`, so nothing is lost in a
    /// round trip.
    fn inline_host(&mut self, el: XmlNode, parent: Option<usize>, label: &str) -> usize {
        let provs = self.provenance(el);
        let layer = layer_of(el);
        let host = self.add_text_item(
            parent,
            layer,
            Self::text_kind(label, "", None),
            provs,
            Some(0),
        );
        let (_, body) = split_head_body(el);
        self.inline_group(el, Some(host), Some(body));
        if let Some(layer) = layer {
            self.set_layer_below(host, layer);
        }
        host
    }

    /// Put `id` and everything under it on `layer` — upstream's
    /// `iterate_items(root=…)` loop setting `content_layer`.
    fn set_layer_below(&mut self, id: usize, layer: ContentLayer) {
        let mut stack = vec![id];
        while let Some(i) = stack.pop() {
            self.tree.items[i].layer = Some(layer);
            stack.extend(self.tree.items[i].children.iter().copied());
        }
    }

    // ------------- floating items -------------

    /// `_extract_caption`: a `<caption>` becomes a body-level `caption`
    /// item (docling's `add_text` without a parent), created *before* the
    /// float that references it.
    fn extract_caption(&mut self, cap: XmlNode) -> Option<usize> {
        let text = get_text(cap);
        if text.is_empty() {
            return None;
        }
        // A caption with formatting runs keeps them (docling-core#865): an
        // empty `caption` host over an inline group, not the runs' text
        // glued together.
        if simple_text_block(cap).is_none() {
            return Some(self.inline_host(cap, None, "caption"));
        }
        let provs = self.provenance(cap);
        // The caption's own `<layer>` head (a furniture logo's alt-text
        // caption), as upstream's `content_layer=_extract_layer(cap_el)`.
        Some(self.add_text_item(
            None,
            layer_of(cap),
            Self::text_kind("caption", &text, None),
            provs,
            None,
        ))
    }

    /// `_extract_captions`: every `<caption>` of an element head (or of a
    /// float group), each a body-level `caption` item — DocLang 0.8 allows
    /// several (docling-core#880).
    fn extract_captions(&mut self, el: XmlNode) -> Vec<usize> {
        el.children()
            .filter(|c| has_tag(*c, "caption"))
            .filter_map(|c| self.extract_caption(c))
            .collect()
    }

    /// `_extract_satellites`: a float group's `<text>` siblings as extra
    /// captions (how they were written before DocLang 0.8), its
    /// `<footnote>`s as the float's footnotes — body-level items, created
    /// before the float. One holding block content is a host item over it.
    fn extract_satellites(&mut self, el: XmlNode) -> (Vec<usize>, Vec<usize>) {
        let mut captions = Vec::new();
        let mut footnotes = Vec::new();
        for n in el.children().filter(|c| c.is_element()) {
            let label = match n.tag_name().name() {
                "text" => "caption",
                "footnote" => "footnote",
                _ => continue,
            };
            let nested: Vec<XmlNode> = n
                .children()
                .filter(|c| {
                    c.is_element()
                        && !is_head_tag(c.tag_name().name())
                        && !matches!(
                            c.tag_name().name(),
                            "bold"
                                | "italic"
                                | "underline"
                                | "strikethrough"
                                | "subscript"
                                | "superscript"
                                | "br"
                                | "content"
                        )
                })
                .collect();
            let item = if !nested.is_empty() {
                // `_parse_container_host`: an empty item of the label over
                // the element's body, parsed like any container's.
                let provs = self.provenance(n);
                let host = self.add_text_item(
                    None,
                    layer_of(n),
                    Self::text_kind(label, "", None),
                    provs,
                    Some(0),
                );
                for c in split_head_body(n).1.into_iter().filter(|c| c.is_element()) {
                    self.dispatch(c, Some(host));
                }
                Some(host)
            } else {
                let text = get_text(n);
                (!text.is_empty()).then(|| {
                    let provs = self.provenance(n);
                    self.add_text_item(None, None, Self::text_kind(label, &text, None), provs, None)
                })
            };
            if let Some(id) = item {
                if label == "caption" {
                    captions.push(id);
                } else {
                    footnotes.push(id);
                }
            }
        }
        (captions, footnotes)
    }

    /// `_parse_code_group`: a `<group>` wrapping a `<code>` with its
    /// satellites — the code item parsed as usual, then given them.
    fn code_group(&mut self, el: XmlNode, code_el: XmlNode, parent: Option<usize>) {
        let (extra, footnotes) = self.extract_satellites(el);
        let first = self.tree.items.len();
        self.text_like(code_el, parent);
        let code = (first..self.tree.items.len())
            .find(|&i| matches!(self.tree.items[i].kind, TreeKind::Code { .. }));
        if let Some(code) = code {
            if let TreeKind::Code { captions, .. } = &mut self.tree.items[code].kind {
                captions.extend(extra);
            }
            self.tree.items[code].footnotes.extend(footnotes);
        }
    }

    /// `_parse_table`: `<table>`/`<index>`, or a `<group>` wrapping one with
    /// its footnotes. The item is created before its rich cells' groups.
    fn table(&mut self, el: XmlNode, parent: Option<usize>) {
        let name = el.tag_name().name();
        let (otsl, captions, footnotes) = if matches!(name, "table" | "index") {
            (Some(el), self.extract_captions(el), Vec::new())
        } else {
            let (extra, footnotes) = self.extract_satellites(el);
            let otsl = first_child(el, "table").or_else(|| first_child(el, "index"));
            let mut captions = self.extract_captions(el);
            if captions.is_empty() {
                if let Some(o) = otsl {
                    captions = self.extract_captions(o);
                }
            }
            captions.extend(extra);
            (otsl, captions, footnotes)
        };
        let Some(otsl) = otsl else {
            self.tree.add(
                parent,
                None,
                TreeKind::Table {
                    footnotes,
                    table: empty_table(),
                    rich_cells: Vec::new(),
                    captions,
                },
            );
            return;
        };
        let (head, body) = split_head_body(otsl);
        let provs = self.provs_from_nodes(&head);
        let layer = layer_from_nodes(&head);
        let tbl = self.add_text_item(
            parent,
            layer,
            TreeKind::Table {
                footnotes: Vec::new(),
                table: empty_table(),
                rich_cells: Vec::new(),
                captions: captions.clone(),
            },
            provs,
            None,
        );
        let (cells, num_rows, num_cols) = self.parse_otsl(&body);
        let mut rich_cells = Vec::new();
        let mut table_cells = Vec::with_capacity(cells.len());
        for OtslCell { cell, rich } in cells {
            if !rich.is_empty() {
                let g = if is_virtual_text(&rich) {
                    // Virtual text: an inline group of the runs.
                    self.inline_group(otsl, Some(tbl), Some(rich))
                } else {
                    // `doc.add_group(parent=tbl, label=UNSPECIFIED)`: the
                    // cell's blocks hang off a group under the table.
                    let g = self.tree.add(
                        Some(tbl),
                        None,
                        TreeKind::Group {
                            label: "unspecified".into(),
                            name: "group".into(),
                        },
                    );
                    for part in rich.iter().filter(|n| n.is_element()) {
                        self.dispatch(*part, Some(g));
                    }
                    g
                };
                rich_cells.push((cell.start_row, cell.start_col, g));
            }
            table_cells.push(cell);
        }
        let table = table_from_cells(table_cells, num_rows, num_cols);
        self.tree.items[tbl].kind = TreeKind::Table {
            footnotes,
            table,
            rich_cells,
            captions,
        };
    }

    /// `_otsl_parse_texts`: cells from the token stream — spans counted from
    /// the row token grid, a leading `<location>` quartet as the cell box,
    /// element content marking a rich cell. Returns the cells and the grid
    /// size (`len(split_rows)` × the widest row).
    fn parse_otsl<'a, 'i>(
        &self,
        body: &[XmlNode<'a, 'i>],
    ) -> (Vec<OtslCell<'a, 'i>>, usize, usize) {
        let mut parts: Vec<Part<'a, 'i>> = Vec::new();
        otsl_parts(body, &mut parts);
        let token_of = |p: &Part<'a, 'i>| match p {
            Part::El(n) => otsl_token(*n),
            Part::Text(_) => None,
        };
        // Rows of tokens, split at `<nl/>` (empty rows dropped, as
        // `groupby` does).
        let mut rows: Vec<Vec<&str>> = vec![Vec::new()];
        for p in &parts {
            match token_of(p) {
                Some("nl") => rows.push(Vec::new()),
                Some(t) => rows.last_mut().unwrap().push(t),
                None => {}
            }
        }
        rows.retain(|r| !r.is_empty());
        let num_rows = rows.len();
        let num_cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        let is_origin = |t: &str| matches!(t, "fcel" | "ecel" | "ched" | "rhed" | "srow" | "corn");
        let is_cont = |t: &str| matches!(t, "lcel" | "ucel" | "xcel");

        let mut cells = Vec::new();
        let (mut r_idx, mut c_idx) = (0usize, 0usize);
        for (i, part) in parts.iter().enumerate() {
            let Some(t) = token_of(part) else {
                continue;
            };
            if is_origin(t) || is_cont(t) {
                let mut content_idx = i + 1;
                let mut bbox: Option<[f32; 4]> = None;
                let mut cell_parts: Vec<&Part<'a, 'i>> = Vec::new();
                let mut cell_text = String::new();
                if t != "ecel" && content_idx < parts.len() {
                    // A leading quartet of `<location>` elements is the box.
                    let locs: Vec<XmlNode> = parts[content_idx..]
                        .iter()
                        .take(4)
                        .map_while(|p| match p {
                            Part::El(n) if has_tag(*n, "location") => Some(*n),
                            _ => None,
                        })
                        .collect();
                    if locs.len() == 4 {
                        bbox = self
                            .provs_from_nodes(&locs)
                            .first()
                            .map(|p| p.bbox.map(|v| v as f32));
                        content_idx += 4;
                    }
                    while content_idx < parts.len() && token_of(&parts[content_idx]).is_none() {
                        let p = &parts[content_idx];
                        match p {
                            Part::Text(n) => cell_text.push_str(n.text().unwrap_or("")),
                            Part::El(n) => cell_text.push_str(&collect_text(*n)),
                        }
                        cell_parts.push(p);
                        content_idx += 1;
                    }
                }
                if !(is_cont(t) && cell_text.trim().is_empty() && cell_parts.is_empty()) {
                    let mut row_span = 1;
                    let mut col_span = 1;
                    let next_right = parts.get(content_idx).and_then(token_of);
                    if matches!(next_right, Some("lcel" | "xcel")) {
                        if let Some(row) = rows.get(r_idx) {
                            col_span += row[(c_idx + 1).min(row.len())..]
                                .iter()
                                .take_while(|t| matches!(**t, "lcel" | "xcel"))
                                .count();
                        }
                    }
                    let next_bottom = rows
                        .get(r_idx + 1)
                        .and_then(|row| row.get(c_idx))
                        .copied()
                        .unwrap_or("");
                    if matches!(next_bottom, "ucel" | "xcel") {
                        row_span += rows[r_idx + 1..]
                            .iter()
                            .take_while(|row| {
                                matches!(row.get(c_idx).copied(), Some("ucel" | "xcel"))
                            })
                            .count();
                    }
                    // A rich cell holds an element; its text is the
                    // concatenated reading of its parts (docling-core 2.106).
                    let has_element = cell_parts.iter().any(|p| matches!(p, Part::El(_)));
                    let rich: Vec<XmlNode> = if has_element {
                        cell_parts
                            .iter()
                            .map(|p| match p {
                                Part::El(n) | Part::Text(n) => *n,
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let text = cell_text;
                    let plain = rich.is_empty();
                    cells.push(OtslCell {
                        cell: TableCell {
                            text,
                            bbox,
                            start_row: r_idx,
                            start_col: c_idx,
                            row_span,
                            col_span,
                            column_header: plain && matches!(t, "ched" | "corn"),
                            row_header: plain && matches!(t, "rhed" | "corn"),
                            row_section: plain && t == "srow",
                        },
                        rich,
                    });
                }
                c_idx += 1;
            }
            if t == "nl" {
                r_idx += 1;
                c_idx = 0;
            }
        }
        (cells, num_rows, num_cols)
    }

    /// `_parse_picture`: `<picture>` or a `<group>` wrapping one. The caption
    /// item precedes the picture; the head `<label>` is its classification,
    /// a `<tabular>` its chart data, `<src>` its image (archives only), and
    /// any further content its children.
    fn picture(&mut self, el: XmlNode, parent: Option<usize>) {
        let (pic_el, captions, footnotes) = if el.tag_name().name() == "picture" {
            (Some(el), self.extract_captions(el), Vec::new())
        } else {
            let (extra, footnotes) = self.extract_satellites(el);
            let pic_el = first_child(el, "picture");
            let mut captions = self.extract_captions(el);
            if captions.is_empty() {
                if let Some(p) = pic_el {
                    captions = self.extract_captions(p);
                }
            }
            captions.extend(extra);
            (pic_el, captions, footnotes)
        };
        let (provs, layer) = match pic_el {
            Some(p) => (self.provenance(p), layer_of(p)),
            None => (Vec::new(), None),
        };
        let mut classification = None;
        let mut chart = None;
        let mut image = None;
        let mut content: Vec<XmlNode> = Vec::new();
        if let Some(p) = pic_el {
            let (head, body) = split_head_body(p);
            classification = match label_from_nodes(&head) {
                None | Some("undefined") => None,
                Some(l) => Some(l.to_string()),
            };
            let mut idx = 0;
            while idx < body.len() {
                let n = body[idx];
                if !n.is_element() {
                    idx += 1;
                    continue;
                }
                match n.tag_name().name() {
                    "src" => {
                        if self.pkg.is_some() {
                            if let Some(uri) = n.attribute("uri") {
                                image = self.load_image(uri.trim());
                            }
                        }
                    }
                    "tabular" => {
                        let (_, otsl_body) = split_head_body(n);
                        let (cells, num_rows, num_cols) = self.parse_otsl(&otsl_body);
                        let cells = cells.into_iter().map(|c| c.cell).collect();
                        chart = Some(table_from_cells(cells, num_rows, num_cols));
                    }
                    _ => break,
                }
                idx += 1;
            }
            content = body[idx..]
                .iter()
                .copied()
                .filter(|n| n.is_element())
                .collect();
        }
        let pic = self.add_text_item(
            parent,
            layer,
            TreeKind::Picture {
                captions,
                image,
                extra_classes: Vec::new(),
                classification,
                description: None,
                // `PictureClassificationPrediction(class_name, confidence=1.0)`.
                confidence: Some(1.0),
                chart,
                dpi: None,
            },
            provs,
            None,
        );
        self.tree.items[pic].footnotes = footnotes;
        for n in content {
            self.dispatch(n, Some(pic));
        }
    }

    /// `_image_ref_from_archive_uri`: a `data:` URI is decoded (and, as
    /// `ImageRef.from_pil` re-encodes it, reported as PNG); a relative path
    /// is an archive asset, typed by its extension. Anything unreadable is
    /// no image.
    fn load_image(&mut self, uri: &str) -> Option<PictureImage> {
        if uri.is_empty() {
            return None;
        }
        let (mimetype, data) = if let Some(rest) = uri.strip_prefix("data:") {
            let (_, payload) = rest.split_once(";base64,")?;
            (
                "image/png".to_string(),
                docling_core::base64::decode(payload.trim())?,
            )
        } else {
            if uri.contains("://") {
                return None;
            }
            let ext = uri.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
            let mimetype = match ext.as_str() {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "bmp" => "image/bmp",
                "webp" => "image/webp",
                "tif" | "tiff" => "image/tiff",
                "svg" => "image/svg+xml",
                _ => "image/png",
            };
            (mimetype.to_string(), self.pkg.as_mut()?.read_bytes(uri)?)
        };
        let (width, height) = crate::backend::rtf::image_size(&mimetype, &data)?;
        Some(PictureImage {
            dpi: PictureImage::DEFAULT_DPI,
            mimetype,
            width,
            height,
            data,
            display_size: None,
        })
    }
}

/// `_extract_code_content_and_language`: the text nodes verbatim (blank
/// ones dropped), `<br/>` a newline, the head `<label>` the language.
fn code_content(el: XmlNode) -> (String, Option<String>) {
    let lang = first_child(el, "label")
        .and_then(|l| l.attribute("value"))
        .filter(|v| !v.is_empty())
        .and_then(code_language);
    // `_extract_code_content_and_language`: the text nodes verbatim (the
    // reading rules have run), the element heads skipped.
    let mut out = String::new();
    for n in el.children() {
        if n.is_text() {
            out.push_str(n.text().unwrap_or(""));
        } else if n.is_element() {
            match n.tag_name().name() {
                t if is_head_tag(t) => {}
                "br" => out.push('\n'),
                _ => out.push_str(&collect_text(n)),
            }
        }
    }
    (out, lang)
}

/// `_is_virtual_text`: the nodes hold raw text or text-level elements of
/// their own (a cell given as unwrapped text, its runs inline).
fn is_virtual_text(nodes: &[XmlNode]) -> bool {
    nodes.iter().any(|n| {
        if n.is_text() {
            !n.text().unwrap_or("").trim_matches(XML_WS).is_empty()
        } else {
            matches!(
                n.tag_name().name(),
                "bold"
                    | "italic"
                    | "underline"
                    | "strikethrough"
                    | "subscript"
                    | "superscript"
                    | "handwriting"
                    | "rtl"
                    | "content"
                    | "br"
                    | "checkbox"
            )
        }
    })
}

/// `_float_group_host`: the one `<picture>`, `<table>`, `<index>` or
/// `<code>` a nameless `<group>` wraps with nothing but satellites
/// (`<text>`, `<footnote>`, `<caption>`) beside it — DocLang's convention for
/// a float's footnotes — else `None`.
fn float_group_host<'a, 'i>(el: XmlNode<'a, 'i>) -> Option<XmlNode<'a, 'i>> {
    let mut hosts = Vec::new();
    for n in el.children().filter(|c| c.is_element()) {
        match n.tag_name().name() {
            "picture" | "table" | "index" | "code" => hosts.push(n),
            "text" | "footnote" | "caption" => {}
            _ => return None,
        }
    }
    match hosts[..] {
        [host] => Some(host),
        _ => None,
    }
}

/// The list-parser's "complex `<text>` wrap" test: more than one element
/// child (`<location>`/`<layer>` aside) or no simple text block.
fn text_wrap_is_complex(text_el: XmlNode) -> bool {
    let n = text_el
        .children()
        .filter(|c| c.is_element() && !matches!(c.tag_name().name(), "location" | "layer"))
        .count();
    n > 1 || simple_text_block(text_el).is_none()
}

/// `_is_list_item_virtual_text`: what the first non-blank node after the
/// delimiter is — raw text, a head token or inline markup mean the item body
/// is virtual text; a semantic or grouping element means it is not.
fn is_list_item_virtual_text(nodes: &[XmlNode]) -> bool {
    let Some(first) = nodes.iter().find(|n| !is_ws_text(**n)) else {
        return false;
    };
    if first.is_text() {
        return true;
    }
    if !first.is_element() {
        return false;
    }
    let name = first.tag_name().name();
    if is_head_tag(name) {
        return true;
    }
    match name {
        // `_LIST_ITEM_VIRTUAL_TEXT_CONTENT_TAGS`, the FORMATTING and CONTENT
        // categories.
        "content" | "bold" | "italic" | "underline" | "strikethrough" | "superscript"
        | "subscript" | "handwriting" | "rtl" | "br" | "checkbox" | "href" | "xref" | "marker" => {
            true
        }
        // SEMANTIC and GROUPING tokens.
        "heading" | "text" | "caption" | "footnote" | "page_header" | "page_footer" | "picture"
        | "field_region" | "field_item" | "field_heading" | "field_hint" | "formula" | "code"
        | "list" | "table" | "tabular" | "group" => false,
        // Every other known token (structural, geometric, temporal, special)
        // — and an unknown tag has no category, which upstream reports as
        // an error; treat it as the tolerant "not virtual text".
        "ldiv" | "fcel" | "ecel" | "ched" | "rhed" | "corn" | "srow" | "lcel" | "ucel" | "xcel"
        | "nl" | "key" | "value" | "thread" | "page_break" | "head" | "doclang" => true,
        _ => false,
    }
}

/// `_is_simple_virtual_text_nodes`: only text and `<content>` nodes, at
/// least one of them non-blank.
fn is_simple_virtual_text(nodes: &[XmlNode]) -> bool {
    let mut any = false;
    for n in nodes {
        if n.is_text() {
            any |= !is_ws_text(*n);
        } else if has_tag(*n, "content") {
            any = true;
        } else if n.is_element() {
            return false;
        }
    }
    any
}

fn empty_table() -> Table {
    Table {
        footnotes: Vec::new(),
        rows: Vec::new(),
        location: None,
        structure: None,
        cell_blocks: None,
        cells: Some(Vec::new()),
        caption: None,
        caption_parent: Default::default(),
        caption_location: None,
    }
}

/// A [`Table`] carrying docling's cells verbatim: `rows` is the
/// `num_rows` × `num_cols` grid (each cell's text at every slot it covers),
/// which the export sizes `TableData` from; `cells` are the `table_cells`.
pub(super) fn table_from_cells(cells: Vec<TableCell>, num_rows: usize, num_cols: usize) -> Table {
    let mut rows = vec![vec![String::new(); num_cols]; num_rows];
    for c in &cells {
        let row_end = (c.start_row + c.row_span).min(num_rows);
        let col_end = (c.start_col + c.col_span).min(num_cols);
        for row in rows.iter_mut().take(row_end).skip(c.start_row) {
            for slot in row.iter_mut().take(col_end).skip(c.start_col) {
                *slot = c.text.clone();
            }
        }
    }
    Table {
        footnotes: Vec::new(),
        rows,
        location: None,
        structure: None,
        cell_blocks: None,
        cells: Some(cells),
        caption: None,
        caption_parent: Default::default(),
        caption_location: None,
    }
}
