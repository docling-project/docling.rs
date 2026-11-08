//! DocLang 0.9's post-processing of the minified markup — docling-core
//! 2.106's `serialize_doc` after the parts are joined: the markup parsed into
//! a DOM (`xml.dom.minidom`'s view of it), each `<content>` that reads the
//! same unwrapped removed (`_unwrap_redundant_content`), the tree laid out
//! with every text block body on one line (`_pretty_print`, after
//! `_collapse_whitespace`), and the tokens that may not be self-closing
//! expanded (`<ldiv/>` → `<ldiv></ldiv>`).
//!
//! DocLang 0.9 reads whitespace by rule (spec "Content encoding and
//! whitespace handling"): a whitespace sequence in a text block is one space,
//! none at the block's edges, `<content>` is literal. So a line break inside
//! a body reads as a space (`2<superscript>nd</superscript>` must not become
//! `2 nd`) and the layout is free everywhere else — which is what
//! `_pretty_print` relies on and the reader (`apply_whitespace_rules`)
//! implements.

use std::sync::OnceLock;

/// `_ELEMENT_HEAD_TAGS`.
const ELEMENT_HEAD_TAGS: &[&str] = &[
    "label",
    "layer",
    "href",
    "location",
    "caption",
    "description",
    "summary",
    "custom",
    "thread",
    "xref",
    "hour",
    "minute",
    "second",
    "centisecond",
];

/// `_FORMATTING_TAGS`.
const FORMATTING_TAGS: &[&str] = &[
    "bold",
    "italic",
    "underline",
    "strikethrough",
    "subscript",
    "superscript",
];

/// `_RAW_TEXT_TAGS` (plus the formatting tags): elements whose body allows
/// raw text; a text block spans nested ones (`<bold>` in `<text>`).
const RAW_TEXT_TAGS: &[&str] = &[
    "text",
    "heading",
    "caption",
    "description",
    "summary",
    "footnote",
    "page_header",
    "page_footer",
    "formula",
    "code",
    "key",
    "value",
    "field_heading",
    "hint",
    "marker",
    "handwriting",
    "rtl",
    "content",
    "chapter",
    "voice",
];

/// `_VIRTUAL_TEXT_INLINE_TAGS` (plus the formatting tags): tags that may
/// continue a virtual `<text>` (a list item or a cell given as unwrapped
/// text).
const VIRTUAL_TEXT_INLINE_TAGS: &[&str] = &["handwriting", "rtl", "content", "br", "checkbox"];

/// `_VIRTUAL_TEXT_SEMANTIC_TAGS`: inline within a virtual `<text>`, a block
/// on their own.
const VIRTUAL_TEXT_SEMANTIC_TAGS: &[&str] = &["code", "formula"];

/// `_WHITESPACE_OPAQUE_TAGS`: left as written.
const WHITESPACE_OPAQUE_TAGS: &[&str] = &["custom", "track"];

/// `_INLINE_ITEM_HOSTS`, `_VIRTUAL_TEXT_HOSTS`, `_FIELD_HOSTS`.
const INLINE_ITEM_HOSTS: &[&str] = &["text", "caption", "footnote", "page_header", "page_footer"];
const VIRTUAL_TEXT_HOSTS: &[&str] = &["list", "table"];
const FIELD_HOSTS: &[&str] = &["key", "value"];

/// docling-core's tokens that may not be self-closing (`DocLangToken` minus
/// `DocLangVocabulary.IS_SELFCLOSING`): `serialize_doc` expands `<x/>` to
/// `<x></x>` for these.
const NON_SELFCLOSING: &[&str] = &[
    "bold",
    "caption",
    "code",
    "content",
    "custom",
    "description",
    "doclang",
    "field_heading",
    "field_item",
    "field_region",
    "footnote",
    "formula",
    "group",
    "handwriting",
    "head",
    "heading",
    "hint",
    "index",
    "italic",
    "key",
    "list",
    "marker",
    "page_footer",
    "page_header",
    "picture",
    "rtl",
    "strikethrough",
    "subscript",
    "summary",
    "superscript",
    "table",
    "tabular",
    "text",
    "underline",
    "value",
    "xref",
];

/// `_XML_WHITESPACE`.
fn is_xml_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

fn is_formatting(tag: &str) -> bool {
    FORMATTING_TAGS.contains(&tag)
}

fn is_raw_text(tag: &str) -> bool {
    RAW_TEXT_TAGS.contains(&tag) || is_formatting(tag)
}

fn is_head(tag: &str) -> bool {
    ELEMENT_HEAD_TAGS.contains(&tag)
}

fn is_vt_inline(tag: &str) -> bool {
    VIRTUAL_TEXT_INLINE_TAGS.contains(&tag) || is_formatting(tag)
}

fn is_vt_semantic(tag: &str) -> bool {
    VIRTUAL_TEXT_SEMANTIC_TAGS.contains(&tag)
}

/// `_COLLAPSIBLE_WHITESPACE_RE`: whitespace DocLang does not read as written
/// — anything but a single space.
pub(crate) fn has_collapsible_ws(text: &str) -> bool {
    text.contains(['\t', '\r', '\n']) || text.contains("  ")
}

/// One node of the parsed markup.
enum Kind {
    Elem {
        name: String,
        attrs: Vec<(String, String)>,
    },
    /// A text node; `cdata` for a CDATA section (minidom's `CDATASection`,
    /// a `Text` subclass: every rule below treats it as text).
    Text { data: String, cdata: bool },
}

struct Node {
    kind: Kind,
    parent: Option<usize>,
    children: Vec<usize>,
}

/// The parsed markup as an arena: node ids are indices, a detached node
/// simply stays unreferenced.
pub(crate) struct Dom {
    n: Vec<Node>,
    root: usize,
}

/// A token of a flattened text block (`_WsToken`).
#[derive(Clone)]
enum Tok {
    /// An unprotected text node.
    Text(usize),
    /// A `<content>` element.
    Content(usize),
    /// Anything else visible (`<br/>`, an inline `<picture>`), by tag.
    Visible(String),
}

impl Dom {
    /// Parse our own minified markup (well-formed, attributes
    /// double-quoted); `None` when it holds no element.
    pub(crate) fn parse(xml: &str) -> Option<Dom> {
        let mut dom = Dom {
            n: Vec::new(),
            root: 0,
        };
        let mut pos = 0;
        let top = dom.parse_children(xml, &mut pos, None);
        dom.root = top.into_iter().find(|&id| dom.tag(id).is_some())?;
        Some(dom)
    }

    fn push(&mut self, kind: Kind, parent: Option<usize>) -> usize {
        self.n.push(Node {
            kind,
            parent,
            children: Vec::new(),
        });
        self.n.len() - 1
    }

    fn parse_children(&mut self, s: &str, pos: &mut usize, parent: Option<usize>) -> Vec<usize> {
        let mut ids: Vec<usize> = Vec::new();
        while *pos < s.len() {
            let rest = &s[*pos..];
            if let Some(body) = rest.strip_prefix("<![CDATA[") {
                let end = body.find("]]>").unwrap_or(body.len());
                let id = self.push(
                    Kind::Text {
                        data: body[..end].to_string(),
                        cdata: true,
                    },
                    parent,
                );
                ids.push(id);
                *pos += "<![CDATA[".len() + end + "]]>".len();
            } else if rest.starts_with("</") {
                *pos += rest.find('>').map_or(rest.len(), |e| e + 1);
                break;
            } else if rest.starts_with("<?") || rest.starts_with("<!--") {
                // Declarations and comments are disregarded.
                let close = if rest.starts_with("<?") { "?>" } else { "-->" };
                *pos += rest.find(close).map_or(rest.len(), |e| e + close.len());
            } else if rest.starts_with("<!") {
                // A document type declaration, its internal subset included.
                let mut depth = 0usize;
                let end = rest
                    .char_indices()
                    .find(|&(_, c)| match c {
                        '[' => {
                            depth += 1;
                            false
                        }
                        ']' => {
                            depth = depth.saturating_sub(1);
                            false
                        }
                        '>' => depth == 0,
                        _ => false,
                    })
                    .map_or(rest.len(), |(i, _)| i + 1);
                *pos += end;
            } else if rest.starts_with('<') {
                let end = tag_end(rest);
                let tag = &rest[1..end];
                *pos += end + 1;
                let (tag, self_closed) = match tag.strip_suffix('/') {
                    Some(t) => (t, true),
                    None => (tag, false),
                };
                let (name, attrs) = parse_tag(tag);
                let id = self.push(Kind::Elem { name, attrs }, parent);
                if !self_closed {
                    let kids = self.parse_children(s, pos, Some(id));
                    self.n[id].children = kids;
                }
                ids.push(id);
            } else {
                let end = rest.find('<').unwrap_or(rest.len());
                let text = decode_entities(&rest[..end]);
                *pos += end;
                // expat hands character data over in pieces; minidom appends
                // a piece to a text node it follows, so one run is one node.
                if let Some(&last) = ids.last() {
                    if let Kind::Text { data, cdata: false } = &mut self.n[last].kind {
                        data.push_str(&text);
                        continue;
                    }
                }
                let id = self.push(
                    Kind::Text {
                        data: text,
                        cdata: false,
                    },
                    parent,
                );
                ids.push(id);
            }
        }
        ids
    }

    fn tag(&self, id: usize) -> Option<&str> {
        match &self.n[id].kind {
            Kind::Elem { name, .. } => Some(name),
            Kind::Text { .. } => None,
        }
    }

    fn text(&self, id: usize) -> Option<&str> {
        match &self.n[id].kind {
            Kind::Text { data, .. } => Some(data),
            Kind::Elem { .. } => None,
        }
    }

    fn is_elem(&self, id: usize) -> bool {
        self.tag(id).is_some()
    }

    fn kids(&self, id: usize) -> Vec<usize> {
        self.n[id].children.clone()
    }

    /// `_is_virtual_text_part`.
    fn is_vt_part(&self, id: usize) -> bool {
        match self.tag(id) {
            None => true,
            Some(t) => is_vt_inline(t) || is_vt_semantic(t),
        }
    }

    /// `_is_virtual_text`: the nodes hold raw text or text-level elements of
    /// their own.
    fn is_virtual_text(&self, nodes: &[usize]) -> bool {
        nodes.iter().any(|&id| match &self.n[id].kind {
            Kind::Text { data, .. } => !data.trim_matches(is_xml_ws).is_empty(),
            Kind::Elem { name, .. } => is_vt_inline(name),
        })
    }

    /// `_for_each_text_block`: `visit` each text block under `el`, innermost
    /// first.
    fn for_each_text_block(&mut self, el: usize, mode: Mode) {
        let Some(tag) = self.tag(el) else { return };
        if WHITESPACE_OPAQUE_TAGS.contains(&tag) {
            return;
        }
        if is_raw_text(tag) {
            let kids = self.kids(el);
            self.visit(&kids, mode);
            return;
        }
        // Not a text block itself; unwrapped text between its child elements
        // is a virtual <text>. A `<content>` unwrapped by a visit is replaced
        // by its one text child in place, so the snapshot stays aligned.
        let mut segment: Vec<usize> = Vec::new();
        for node in self.kids(el) {
            let head = self.tag(node).is_some_and(is_head);
            if self.is_vt_part(node) || head {
                segment.push(node);
                continue;
            }
            self.visit_segment(&segment, mode);
            segment.clear();
            if self.is_elem(node) {
                self.for_each_text_block(node, mode);
            }
        }
        self.visit_segment(&segment, mode);
    }

    fn visit_segment(&mut self, segment: &[usize], mode: Mode) {
        if self.is_virtual_text(segment) {
            self.visit(segment, mode);
            return;
        }
        // No text of its own: whitespace between blocks, read on their own.
        let loose: Vec<usize> = segment
            .iter()
            .copied()
            .filter(|&n| self.tag(n).is_none_or(is_head))
            .collect();
        self.visit(&loose, mode);
        for &node in segment {
            if self.tag(node).is_some_and(is_vt_semantic) {
                self.for_each_text_block(node, mode);
            }
        }
    }

    /// `_flatten_text_block`.
    fn flatten(&mut self, nodes: &[usize], mode: Mode) -> Vec<Tok> {
        let mut out = Vec::new();
        for &node in nodes {
            let Some(tag) = self.tag(node).map(str::to_string) else {
                out.push(Tok::Text(node));
                continue;
            };
            if is_head(&tag) {
                // An element head is read separately, as if removed.
                self.for_each_text_block(node, mode);
            } else if tag == "content" {
                out.push(Tok::Content(node));
            } else if tag == "page_break" {
                // A page boundary within the text, transparent to its reading.
            } else if is_raw_text(&tag) {
                let kids = self.kids(node);
                out.extend(self.flatten(&kids, mode));
            } else {
                if tag != "br" {
                    self.for_each_text_block(node, mode);
                }
                out.push(Tok::Visible(tag));
            }
        }
        out
    }

    /// `_content_text`.
    fn content_text(&self, el: usize) -> String {
        self.n[el]
            .children
            .iter()
            .filter_map(|&c| self.text(c))
            .collect()
    }

    /// `_read_text_block`: each token's reading — rules 2-4 for text nodes,
    /// literal for `<content>`, `"\0"` for any other visible item.
    fn read_text_block(&self, tokens: &[Tok]) -> Vec<String> {
        let mut reading: Vec<String> = vec![String::new(); tokens.len()];
        let mut seen_content = false;
        let mut in_sequence = false;
        let mut pending: Vec<usize> = Vec::new();
        for (idx, tok) in tokens.iter().enumerate() {
            match tok {
                Tok::Text(id) => {
                    for ch in self.text(*id).unwrap_or("").chars() {
                        if is_xml_ws(ch) {
                            if !in_sequence {
                                pending.push(idx);
                                in_sequence = true;
                            }
                            continue;
                        }
                        // resolve_pending: content follows.
                        if seen_content {
                            for &p in &pending {
                                reading[p].push(' ');
                            }
                        }
                        pending.clear();
                        in_sequence = false;
                        seen_content = true;
                        reading[idx].push(ch);
                    }
                }
                Tok::Content(_) | Tok::Visible(_) => {
                    let literal = match tok {
                        Tok::Content(id) => self.content_text(*id),
                        _ => "\0".to_string(),
                    };
                    if !literal.is_empty() {
                        if seen_content {
                            for &p in &pending {
                                reading[p].push(' ');
                            }
                        }
                        pending.clear();
                        seen_content = true;
                        reading[idx].push_str(&literal);
                        in_sequence = false;
                    }
                }
            }
        }
        reading
    }

    fn visit(&mut self, nodes: &[usize], mode: Mode) {
        match mode {
            Mode::Unwrap => self.unwrap_visit(nodes),
            Mode::Read => self.read_visit(nodes),
        }
    }

    /// `_unwrap_redundant_content`'s visitor.
    fn unwrap_visit(&mut self, nodes: &[usize]) {
        let tokens = self.flatten(nodes, Mode::Unwrap);
        let reading = self.read_text_block(&tokens);
        let host = nodes.first().and_then(|&n| self.n[n].parent);
        let host_tag = host.and_then(|h| self.tag(h)).unwrap_or("").to_string();
        let keeps_space_beside = |item: &str| -> bool {
            match item {
                "checkbox" => {
                    INLINE_ITEM_HOSTS.contains(&host_tag.as_str())
                        || VIRTUAL_TEXT_HOSTS.contains(&host_tag.as_str())
                        || FIELD_HOSTS.contains(&host_tag.as_str())
                }
                "picture" => INLINE_ITEM_HOSTS.contains(&host_tag.as_str()),
                _ => false,
            }
        };
        // Whether an edge space would be lost beside the first neighbour
        // that reads as something.
        let needs_wrapper = |neighbours: Vec<usize>, last: bool| -> bool {
            let Some(nb) = neighbours.into_iter().find(|&i| !reading[i].is_empty()) else {
                return true; // dropped at the block edge
            };
            if let Tok::Visible(tag) = &tokens[nb] {
                return !keeps_space_beside(tag);
            }
            let touching = if last {
                reading[nb].chars().last()
            } else {
                reading[nb].chars().next()
            };
            touching == Some(' ') // would merge with a neighbouring space
        };
        let mut unwrap: Vec<usize> = Vec::new();
        for (idx, tok) in tokens.iter().enumerate() {
            let Tok::Content(el) = *tok else { continue };
            let text = &reading[idx];
            if text.is_empty() || has_collapsible_ws(text) {
                continue;
            }
            let edge_space = text.starts_with(' ') || text.ends_with(' ');
            if edge_space && self.in_nested_block(el, host) {
                continue; // the deserializer trims the edges of that block
            }
            let parent = self.n[el].parent;
            if text.trim_matches(' ').is_empty() {
                let parent_raw = parent.and_then(|p| self.tag(p)).is_some_and(is_raw_text);
                let others: Vec<usize> = nodes
                    .iter()
                    .copied()
                    .filter(|&n| n != el && self.n[n].parent == parent)
                    .collect();
                if !parent_raw && !self.is_virtual_text(&others) {
                    continue; // bare, it would no longer make its item a virtual <text>
                }
            }
            if text.starts_with(' ') && needs_wrapper((0..idx).rev().collect(), true) {
                continue;
            }
            if text.ends_with(' ') && needs_wrapper((idx + 1..tokens.len()).collect(), false) {
                continue;
            }
            unwrap.push(el);
        }
        for el in unwrap {
            self.unwrap_element(el);
        }
    }

    /// Whether `el` sits in an element the deserializer reads as a block
    /// (`<value><text>`), below `host`.
    fn in_nested_block(&self, el: usize, host: Option<usize>) -> bool {
        let mut ancestor = self.n[el].parent;
        while ancestor != host {
            let Some(a) = ancestor else { return false };
            if self
                .tag(a)
                .is_none_or(|t| !(is_vt_inline(t) || is_vt_semantic(t)))
            {
                return true;
            }
            ancestor = self.n[a].parent;
        }
        false
    }

    /// Move `el`'s children into its parent at its place and drop it.
    fn unwrap_element(&mut self, el: usize) {
        let Some(parent) = self.n[el].parent else {
            return;
        };
        let kids = std::mem::take(&mut self.n[el].children);
        for &k in &kids {
            self.n[k].parent = Some(parent);
        }
        let siblings = &mut self.n[parent].children;
        if let Some(pos) = siblings.iter().position(|&c| c == el) {
            siblings.splice(pos..=pos, kids);
        }
        self.n[el].parent = None;
    }

    /// `_apply_whitespace_rules`' visitor: each text node holds exactly its
    /// own reading.
    fn read_visit(&mut self, nodes: &[usize]) {
        let tokens = self.flatten(nodes, Mode::Read);
        let reading = self.read_text_block(&tokens);
        for (tok, text) in tokens.iter().zip(reading) {
            if let Tok::Text(id) = tok {
                if let Kind::Text { data, .. } = &mut self.n[*id].kind {
                    *data = text;
                }
            }
        }
    }

    /// `_unwrap_redundant_content`.
    pub(crate) fn unwrap_redundant_content(&mut self) {
        let root = self.root;
        self.for_each_text_block(root, Mode::Unwrap);
    }

    /// `_apply_whitespace_rules`: rewrite the text nodes as DocLang 0.9
    /// reads them.
    pub(crate) fn apply_whitespace_rules(&mut self) {
        let root = self.root;
        self.for_each_text_block(root, Mode::Read);
    }

    /// `_collapse_whitespace`: whitespace runs outside `<content>` (and
    /// `<custom>`) become one space, which reads the same.
    fn collapse_whitespace(&mut self, node: usize) {
        for child in self.kids(node) {
            match &mut self.n[child].kind {
                Kind::Text { data, .. } => *data = collapse_ws_runs(data),
                Kind::Elem { name, .. } => {
                    if name != "content" && name != "custom" {
                        self.collapse_whitespace(child);
                    }
                }
            }
        }
    }

    /// minidom's `toxml()` of one node.
    fn toxml(&self, id: usize, out: &mut String) {
        match &self.n[id].kind {
            Kind::Text { data, cdata: true } => {
                out.push_str("<![CDATA[");
                out.push_str(data);
                out.push_str("]]>");
            }
            Kind::Text { data, .. } => write_data(out, data, false),
            Kind::Elem { name, attrs } => {
                out.push('<');
                out.push_str(name);
                for (k, v) in attrs {
                    out.push(' ');
                    out.push_str(k);
                    out.push_str("=\"");
                    write_data(out, v, true);
                    out.push('"');
                }
                if self.n[id].children.is_empty() {
                    out.push_str("/>");
                    return;
                }
                out.push('>');
                for &c in &self.n[id].children {
                    self.toxml(c, out);
                }
                out.push_str("</");
                out.push_str(name);
                out.push('>');
            }
        }
    }

    fn inline(&self, nodes: &[usize]) -> String {
        let mut s = String::new();
        for &n in nodes {
            self.toxml(n, &mut s);
        }
        s.trim_matches(' ').to_string()
    }

    /// `_pretty_print`: indent the element structure, each text block body
    /// on one line.
    pub(crate) fn pretty_print(&mut self, indent: &str) -> String {
        let root = self.root;
        self.collapse_whitespace(root);
        let mut lines: Vec<String> = Vec::new();
        self.write(root, 0, indent, &mut lines);
        lines.join("\n")
    }

    fn write(&self, el: usize, depth: usize, indent: &str, lines: &mut Vec<String>) {
        let pad = indent.repeat(depth);
        let Kind::Elem { name, attrs } = &self.n[el].kind else {
            return;
        };
        let mut attr_s = String::new();
        for (k, v) in attrs {
            attr_s.push_str(&format!(" {k}=\"{}\"", escape_xml_attr(v)));
        }
        let mut nodes: Vec<usize> = self.n[el].children.clone();
        if nodes.is_empty() {
            lines.push(format!("{pad}<{name}{attr_s}/>"));
            return;
        }
        let open = format!("<{name}{attr_s}>");
        let close = format!("</{name}>");
        if !is_raw_text(name) && !nodes.iter().any(|&n| self.is_elem(n)) {
            // Text only, e.g. a custom field value: written as is.
            let mut body = String::new();
            for &n in &nodes {
                self.toxml(n, &mut body);
            }
            lines.push(format!("{pad}{open}{body}{close}"));
            return;
        }
        if is_raw_text(name) {
            let mut head: Vec<usize> = Vec::new();
            while let Some(&first) = nodes.first() {
                let blank = self
                    .text(first)
                    .is_some_and(|t| t.trim_matches(' ').is_empty());
                let head_el = self.tag(first).is_some_and(is_head);
                if !(blank || head_el) {
                    break;
                }
                nodes.remove(0);
                if head_el {
                    head.push(first);
                }
            }
            if head.is_empty() {
                lines.push(format!("{pad}{open}{}{close}", self.inline(&nodes)));
                return;
            }
            lines.push(format!("{pad}{open}"));
            for h in head {
                self.write(h, depth + 1, indent, lines);
            }
            let body = self.inline(&nodes);
            if !body.is_empty() {
                lines.push(format!("{pad}{indent}{body}"));
            }
            lines.push(format!("{pad}{close}"));
            return;
        }
        lines.push(format!("{pad}{open}"));
        // Unwrapped text in a container (a virtual <text>) is a text block of
        // its own; inline <code> and <formula> belong to it, written flat.
        let mut segment: Vec<usize> = Vec::new();
        let flush = |segment: &mut Vec<usize>, lines: &mut Vec<String>| {
            if self.is_virtual_text(segment) {
                lines.push(format!("{pad}{indent}{}", self.inline(segment)));
            } else {
                for &block in segment.iter() {
                    if self.is_elem(block) {
                        self.write(block, depth + 1, indent, lines);
                    }
                }
            }
            segment.clear();
        };
        for &node in &nodes {
            if self.is_vt_part(node) {
                segment.push(node);
                continue;
            }
            flush(&mut segment, lines);
            if self.is_elem(node) {
                self.write(node, depth + 1, indent, lines);
            }
        }
        flush(&mut segment, lines);
        lines.push(format!("{pad}{close}"));
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Unwrap,
    Read,
}

/// `_XML_WHITESPACE_RUN_RE.sub(" ", …)`.
fn collapse_ws_runs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if is_xml_ws(c) {
            if !in_ws {
                out.push(' ');
            }
            in_ws = true;
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

/// `_create_layer_token`'s value: DocLang has no notes or invisible layer, so
/// docling-core maps them to the closest ones (lossy) — notes to furniture,
/// invisible to background.
pub(crate) fn layer_value(layer: crate::ContentLayer) -> &'static str {
    match layer {
        crate::ContentLayer::Furniture | crate::ContentLayer::Notes => "furniture",
        crate::ContentLayer::Invisible | crate::ContentLayer::Background => "background",
    }
}

/// `_escape_text` (AUTO escape and wrap modes): XML-illegal characters as
/// `[U+XXXX]`, a CDATA section for text holding `"' &<>` (a literal `]]>`
/// split across two), a `<content>` wrapper for text whose reading the
/// whitespace rules would change — edge whitespace, or whitespace other than
/// single spaces (`wrap_multiline`; off, only the edges count).
pub(crate) fn escape_text(text: &str, wrap_multiline: bool) -> String {
    let text = crate::doclang::sanitize_xml_illegal(text);
    let text = text.as_ref();
    let wrap = text != text.trim() || (wrap_multiline && has_collapsible_ws(text));
    let mut t = if text.contains(['"', '\'', '&', '<', '>']) {
        format!("<![CDATA[{}]]>", text.replace("]]>", "]]]]><![CDATA[>"))
    } else {
        text.to_string()
    };
    if wrap {
        t = format!("<content>{t}</content>");
    }
    t
}

/// `_escape_xml_attr`.
pub(crate) fn escape_xml_attr(v: &str) -> String {
    v.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `serialize_doc`'s post-processing with `pretty_indentation="  "` and the
/// default `WrapMode.AUTO` / `preserve_empty_non_selfclosing`: parse, unwrap
/// the redundant `<content>`, pretty-print, expand the non-self-closing
/// tokens. Unparsable markup comes back as it is.
pub(crate) fn finish(xml: &str) -> String {
    let Some(mut dom) = Dom::parse(xml) else {
        return xml.to_string();
    };
    dom.unwrap_redundant_content();
    let text = dom.pretty_print("  ");
    expand_non_selfclosing(&text)
}

/// DocLang 0.9's reading of a document's markup — what docling-core's
/// deserializer runs before anything else (`_apply_whitespace_rules`): every
/// text node rewritten to exactly its reading — a whitespace sequence one
/// space, none outside text blocks or at their edges, `<content>` (and
/// `<custom>`, `<track>`) as written. The markup comes back as the root
/// element alone (the declaration, a doctype and comments dropped), CDATA
/// sections kept; `None` when it holds no element.
pub fn doclang_apply_reading_rules(xml: &str) -> Option<String> {
    let mut dom = Dom::parse(xml)?;
    dom.apply_whitespace_rules();
    let mut out = String::with_capacity(xml.len());
    dom.toxml(dom.root, &mut out);
    Some(out)
}

/// `<x/>` → `<x></x>` for the tokens that may not be self-closing.
pub(crate) fn expand_non_selfclosing(text: &str) -> String {
    static EXPAND: OnceLock<regex::Regex> = OnceLock::new();
    let re = EXPAND.get_or_init(|| {
        regex::Regex::new(&format!(
            r"<\s*({})(\s[^>]*)?/\s*>",
            NON_SELFCLOSING.join("|")
        ))
        .expect("static pattern")
    });
    re.replace_all(text, "<${1}${2}></${1}>").into_owned()
}

/// The index of the `>` closing the tag that starts `s`, skipping quoted
/// attribute values.
fn tag_end(s: &str) -> usize {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match (c, quote) {
            ('"' | '\'', None) => quote = Some(c),
            (c, Some(q)) if c == q => quote = None,
            ('>', None) => return i,
            _ => {}
        }
    }
    s.len()
}

fn parse_tag(tag: &str) -> (String, Vec<(String, String)>) {
    let tag = tag.trim();
    let (name, mut rest) = tag.split_once(char::is_whitespace).unwrap_or((tag, ""));
    let mut attrs = Vec::new();
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim().to_string();
        let after = &rest[eq + 1..];
        let Some(open) = after.find(['"', '\'']) else {
            break;
        };
        let q = after[open..].chars().next().unwrap_or('"');
        let value_and_rest = &after[open + 1..];
        let close = value_and_rest.find(q).unwrap_or(value_and_rest.len());
        // XML attribute-value normalization: a literal tab or line break is
        // read as a space.
        let value = decode_entities(&value_and_rest[..close]).replace(['\t', '\n', '\r'], " ");
        attrs.push((key, value));
        rest = value_and_rest.get(close + 1..).unwrap_or("");
    }
    (name.to_string(), attrs)
}

pub(crate) fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let Some(semi) = tail.find(';') else {
            out.push_str(tail);
            return out;
        };
        let entity = &tail[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => out.push(c),
            None => out.push_str(&tail[..=semi]),
        }
        rest = &tail[semi + 1..];
    }
    out.push_str(rest);
    out
}

/// minidom's `_write_data`.
fn write_data(out: &mut String, text: &str, attr: bool) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            '\r' if attr => out.push_str("&#13;"),
            '\n' if attr => out.push_str("&#10;"),
            '\t' if attr => out.push_str("&#9;"),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_go_on_one_line_and_heads_on_their_own() {
        let xml = "<doclang version=\"0.9\"><text>Inline math: <formula>E = mc^2</formula></text>\n\
                   <text><location value=\"1\"/>\n<location value=\"2\"/>\nHello\n world</text></doclang>";
        assert_eq!(
            finish(xml),
            "<doclang version=\"0.9\">\n  <text>Inline math: <formula>E = mc^2</formula></text>\n  \
             <text>\n    <location value=\"1\"/>\n    <location value=\"2\"/>\n    Hello world\n  </text>\n</doclang>"
        );
    }

    #[test]
    fn content_is_unwrapped_only_where_it_reads_the_same() {
        // A trailing space before `<bold>` reads the same bare; one at the
        // block's end would be lost.
        let xml = "<doclang><text><content>Normal </content><bold>bold</bold></text>\
                   <text><bold>x</bold><content> tail </content></text>\
                   <text><content>a\nb</content></text></doclang>";
        assert_eq!(
            finish(xml),
            "<doclang>\n  <text>Normal <bold>bold</bold></text>\n  \
             <text><bold>x</bold><content> tail </content></text>\n  \
             <text><content>a\nb</content></text>\n</doclang>"
        );
    }

    #[test]
    fn virtual_text_in_a_list_stays_flat() {
        let xml = "<doclang><list><ldiv/>\nItem <bold>one</bold>\n<ldiv/>\nTwo\n</list></doclang>";
        assert_eq!(
            finish(xml),
            "<doclang>\n  <list>\n    <ldiv/>\n    Item <bold>one</bold>\n    <ldiv/>\n    Two\n  </list>\n</doclang>"
        );
    }

    #[test]
    fn reading_rules_collapse_and_trim() {
        let mut dom =
            Dom::parse("<doclang><text>\n  a   <bold>b</bold>\n c \n</text></doclang>").unwrap();
        dom.apply_whitespace_rules();
        let mut out = String::new();
        let root = dom.root;
        dom.toxml(root, &mut out);
        assert_eq!(out, "<doclang><text>a <bold>b</bold> c</text></doclang>");
    }
}
