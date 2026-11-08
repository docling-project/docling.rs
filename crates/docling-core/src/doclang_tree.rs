//! DocLang straight from docling's item tree (#884) — docling-core's
//! `DocLangDocSerializer` ported call-for-call over an [`ItemTree`], the
//! structure the JSON export writes. Every backend that builds a tree gets
//! its DocLang from here (the LaTeX and RIS ones first, the rest with
//! DocLang 0.9, whose inline runs carry their own whitespace — which only
//! the tree's runs do).
//!
//! The flat serializer (`doclang.rs`) reads the Markdown-flavoured node
//! stream and reproduces `minidom.toprettyxml`'s layout fragment by fragment.
//! That stream cannot say what the tree says: a footnote's label, a
//! caption's or a table cell's inline formulas, a list item's raw text (a
//! LaTeX `*` re-parsed as italics), a paragraph docling never wrote as
//! Markdown. Here the item tree is serialized the way docling-core does it —
//! the *minified* markup first (every part joined by the record delimiter
//! `"\n"`, an inline group's runs concatenated as they are), then the same
//! post-processing `serialize_doc` runs ([`crate::doclang_xml::finish`]):
//! parse, unwrap each `<content>` that reads the same bare, pretty-print with
//! every text block body on one line (DocLang 0.9 reads a line break inside
//! a body as a space), expand the self-closed tokens that may not be
//! self-closing. The layout then falls out of upstream's own rules instead of
//! being rebuilt case by case.
//!
//! Scope: what docling-core's serializer does for documents without
//! provenance or form structure. An item the port does not cover — a
//! located item, a field region — makes
//! [`export`] return `None`, and the caller falls back to the flat
//! serializer, so no content is lost.
//!
//! One deliberate divergence: a TikZ picture's `meta.code` is left out.
//! docling-core writes it as `<custom><code>` holding the field's Python
//! repr (`confidence=None created_by=None text='…' language=<…>`) — a
//! serialization accident the Markdown export leaves out too
//! (docs/MIGRATION.md § LaTeX); the JSON keeps the code.

use std::collections::HashMap;

use crate::doclang::{code_lang_label, is_rtl, Pictures};
use crate::tree::{ItemTree, TreeKind};
use crate::ContentLayer;

/// docling-core 2.106's `_DOCLANG_VERSION` — what docling 2.148 writes.
const VERSION: &str = "0.9";

/// The pretty-printing delimiter (`_get_delim` with an indentation set).
const DELIM: &str = "\n";

/// The serialized document and, when asked for, the picture assets its
/// `<src>` references name.
pub(crate) type Export = (String, Vec<(String, Vec<u8>)>);

/// Serialize `tree` to DocLang, pictures written per `pictures` (docling's
/// `image_mode`; `Referenced` is `save_as_doclang_archive`'s run: every body
/// picture with an image gets `<src uri="assets/image_….png"/>`). `None`
/// when the tree holds something the port does not cover (module docs).
pub(crate) fn export(tree: &ItemTree, pictures: Pictures) -> Option<Export> {
    let outside = tree.items.iter().filter(|it| !it.deleted).find_map(|it| {
        if it.prov.is_some() {
            Some("provenance".to_string())
        } else {
            unsupported(&it.kind)
        }
    });
    if let Some(why) = outside {
        crate::debug_log!("doclang: item tree outside the port ({why}), serializing the nodes");
        return None;
    }
    // The document docling-core serializes: its `validate_misplaced_list_items`
    // has run on it, and a tree of trimmed runs reads with the pre-1.11 join
    // re-injected (#997) — what the JSON export writes too.
    let mut tree = tree.clone();
    if !tree.inline_whitespace {
        tree.separate_legacy_inline_runs();
    }
    tree.wrap_misplaced_list_items();
    let tree = &tree;
    let mut ser = Ser::new(tree, pictures);
    let parts = ser.get_parts(None, false);
    let body = parts.join(DELIM);
    let xml = format!("<doclang version=\"{VERSION}\">{body}</doclang>");
    Some((
        crate::doclang_xml::finish(&xml),
        ser.assets.unwrap_or_default(),
    ))
}

/// Kinds and payloads outside the port (module docs).
fn unsupported(kind: &TreeKind) -> Option<String> {
    match kind {
        TreeKind::FieldRegion { .. } => Some("field region".into()),
        TreeKind::Text { label, .. } if text_token(label).is_none() => {
            Some(format!("text label {label}"))
        }
        _ => None,
    }
}

/// The element a text item of `label` serializes as — `DocLangTextSerializer`'s
/// mapping — `None` for a label the port does not cover. Titles and section
/// headers (`<heading>`) and list items (`<ldiv/>`, virtual text) get their
/// wrapper from the caller.
fn text_token(label: &str) -> Option<&'static str> {
    Some(match label {
        "title" | "section_header" => "heading",
        "list_item" => "text",
        // v0.5: a caption is only valid in a host's head; a standalone one is text.
        "caption" | "checkbox_selected" | "checkbox_unselected" => "text",
        "empty_value" | "handwritten_text" | "paragraph" | "reference" | "grading_scale" => "text",
        "text" => "text",
        "footnote" => "footnote",
        "formula" => "formula",
        "page_header" => "page_header",
        "page_footer" => "page_footer",
        _ => return None,
    })
}

/// docling-core's `_escape_text` (AUTO escape and wrap modes).
fn escape(text: &str) -> String {
    crate::doclang_xml::escape_text(text, true)
}

/// docling-core's `_escape_xml_attr`.
fn escape_attr(v: &str) -> String {
    v.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn wrap(text: &str, tag: &str) -> String {
    format!("<{tag}>{text}</{tag}>")
}

struct Ser<'t> {
    tree: &'t ItemTree,
    visited: Vec<bool>,
    /// docling's `_captions_of_some_item`: a host serializes these itself.
    caption: Vec<bool>,
    /// `_satellite_descendants`: content inside a caption, which no outer
    /// walk may emit again.
    in_caption: Vec<bool>,
    /// Picture → its `_with_pictures_refs` image index.
    pic_index: HashMap<usize, usize>,
    pictures: Pictures,
    assets: Option<Vec<(String, Vec<u8>)>>,
    /// The referenced assets' digests and PNG bytes, decoded up front in
    /// parallel (#898); empty for the other modes.
    digests: crate::pixel_digest::Assets,
}

impl<'t> Ser<'t> {
    fn new(tree: &'t ItemTree, pictures: Pictures) -> Self {
        let n = tree.items.len();
        let mut caption = vec![false; n];
        for it in &tree.items {
            for &c in captions_of(&it.kind) {
                caption[c] = true;
            }
        }
        let mut in_caption = vec![false; n];
        for c in (0..n).filter(|&c| caption[c]) {
            let mut stack: Vec<usize> = tree.items[c].children.clone();
            while let Some(d) = stack.pop() {
                in_caption[d] = true;
                stack.extend(tree.items[d].children.iter().copied());
            }
        }
        // `_with_pictures_refs` counts every body-layer picture of the
        // default `iterate_items` walk (a picture's own children unvisited),
        // image or not.
        let mut pic_index = HashMap::new();
        let mut order = Vec::new();
        for &b in &tree.body {
            pre_order(tree, b, &mut order);
        }
        for id in order {
            let it = &tree.items[id];
            if matches!(it.kind, TreeKind::Picture { .. }) && it.layer.is_none() {
                let next = pic_index.len();
                pic_index.insert(id, next);
            }
        }
        let digests = if matches!(pictures, Pictures::Referenced { .. }) {
            let mut ids: Vec<usize> = pic_index.keys().copied().collect();
            ids.sort_unstable();
            crate::pixel_digest::Assets::prepare(ids.into_iter().filter_map(|id| {
                match &tree.items[id].kind {
                    TreeKind::Picture {
                        image: Some(img), ..
                    } => Some(img.data.as_slice()),
                    _ => None,
                }
            }))
        } else {
            crate::pixel_digest::Assets::empty()
        };
        Ser {
            tree,
            visited: vec![false; n],
            caption,
            in_caption,
            pic_index,
            pictures,
            assets: matches!(pictures, Pictures::Referenced { collect: true }).then(Vec::new),
            digests,
        }
    }

    /// `DocSerializer.get_parts`: the walk below `root` (the body when
    /// `None`), each item not yet visited serialized in pre-order — what an
    /// item's serializer consumed (an inline group, a list's items, a rich
    /// cell) is visited by then and skipped.
    fn get_parts(&mut self, root: Option<usize>, inline: bool) -> Vec<String> {
        let mut order = Vec::new();
        match root {
            Some(r) => pre_order(self.tree, r, &mut order),
            None => {
                for &b in &self.tree.body {
                    pre_order(self.tree, b, &mut order);
                }
            }
        }
        let skip_satellites = root.is_none_or(|r| !self.in_caption[r] && !self.caption[r]);
        let mut parts = Vec::new();
        for id in order {
            if self.visited[id] || (skip_satellites && self.in_caption[id]) {
                continue;
            }
            self.visited[id] = true;
            let part = self.serialize(id, inline);
            if !part.is_empty() {
                parts.push(part);
            }
        }
        parts
    }

    /// `DocSerializer.serialize` for one item: the serializer its class picks.
    fn serialize(&mut self, id: usize, inline: bool) -> String {
        self.visited[id] = true;
        let tree = self.tree;
        let item = &tree.items[id];
        match &item.kind {
            TreeKind::Group { label, .. } if is_list_label(label) => self.list(id, inline),
            TreeKind::Group { label, .. } if label == "inline" => self.inline_group(id),
            // `DocLangFallbackSerializer`: a plain group is transparent.
            TreeKind::Group { .. } => self.get_parts(Some(id), false).join(DELIM),
            TreeKind::Text { .. } | TreeKind::Code { .. } => {
                if self.caption[id] {
                    String::new()
                } else {
                    self.text(id, inline)
                }
            }
            TreeKind::Table { .. } => self.table(id),
            TreeKind::Picture { .. } => self.picture(id),
            // `DocLangKeyValueSerializer` writes nothing; a field region never
            // gets here (`export` declines the tree).
            TreeKind::KeyValueGraph { .. } | TreeKind::FieldRegion { .. } => String::new(),
        }
    }

    fn is_list_item(&self, id: usize) -> bool {
        matches!(&self.tree.items[id].kind, TreeKind::Text { label, .. } if label == "list_item")
    }

    fn is_list_group(&self, id: usize) -> bool {
        matches!(&self.tree.items[id].kind, TreeKind::Group { label, .. } if is_list_label(label))
    }

    fn is_inline_group(&self, id: usize) -> bool {
        matches!(&self.tree.items[id].kind, TreeKind::Group { label, .. } if label == "inline")
    }

    /// `_list_item_segment_sibling`: a nested list or a picture shares the
    /// item's `<ldiv>` segment.
    fn segment_sibling(&self, id: usize) -> bool {
        self.is_list_group(id) || matches!(self.tree.items[id].kind, TreeKind::Picture { .. })
    }

    /// `_list_item_has_segment_siblings`: the item holds a nested list or a
    /// picture, or a nested list follows it in its own list.
    fn has_segment_siblings(&self, id: usize) -> bool {
        let item = &self.tree.items[id];
        if item.children.iter().any(|&c| self.segment_sibling(c)) {
            return true;
        }
        let Some(parent) = item.parent.filter(|&p| self.is_list_group(p)) else {
            return false;
        };
        let siblings = &self.tree.items[parent].children;
        siblings
            .iter()
            .skip_while(|&&c| c != id)
            .skip(1)
            .any(|&c| self.is_list_group(c))
    }

    /// `DocLangListSerializer`: the items' segments (each nested list and
    /// picture of an item right after it), joined and closed by the delimiter.
    fn list(&mut self, id: usize, inline: bool) -> String {
        let tree = self.tree;
        let mut segments = Vec::new();
        let children = &tree.items[id].children;
        for &c in children {
            if self.is_list_group(c) {
                if self.visited[c] {
                    continue;
                }
                self.visited[c] = true;
                let s = self.serialize(c, inline);
                if !s.is_empty() {
                    segments.push(s);
                }
                continue;
            }
            if !self.is_list_item(c) || self.visited[c] {
                continue;
            }
            self.visited[c] = true;
            let s = self.serialize(c, inline);
            if !s.is_empty() {
                segments.push(s);
            }
            for &sub in &tree.items[c].children {
                if !self.segment_sibling(sub) || self.visited[sub] {
                    continue;
                }
                self.visited[sub] = true;
                let s = self.serialize(sub, inline);
                if !s.is_empty() {
                    segments.push(s);
                }
            }
        }
        if segments.is_empty() {
            return String::new();
        }
        let ordered = children.first().is_some_and(|&c| {
            matches!(&tree.items[c].kind, TreeKind::Text { label, list: Some(m), .. }
                if label == "list_item" && m.enumerated)
        });
        let open = if ordered {
            "<list class=\"ordered\">"
        } else {
            "<list>"
        };
        format!("{open}{}{DELIM}</list>", segments.join(DELIM))
    }

    /// `DocLangInlineSerializer`: the runs concatenated as they are — no
    /// delimiter, each run carries its own significant whitespace (DocLang
    /// 0.9: a line break would read as a space; `<content>` is the
    /// whitespace channel, the pretty-printer lays the element out) — after
    /// the group's head (its layer); wrapped in `<text>` unless the group's
    /// parent is a text item (a list item's only when its segment holds more
    /// than the text), and a group nested in a group only when it has a head.
    fn inline_group(&mut self, id: usize) -> String {
        let tree = self.tree;
        let item = &tree.items[id];
        let mut should_wrap = match item.parent {
            None => true,
            Some(p) if self.is_list_item(p) => self.has_segment_siblings(p),
            Some(p) => !matches!(
                tree.items[p].kind,
                TreeKind::Text { .. } | TreeKind::Code { .. }
            ),
        };
        let mut head = String::new();
        if should_wrap {
            if let Some(layer) = item.layer {
                head.push_str(&layer_token(layer));
            }
        }
        if item.parent.is_some_and(|p| self.is_inline_group(p)) {
            should_wrap = !head.is_empty();
        }
        let runs: String = self.get_parts(Some(id), true).concat();
        let text = head + &runs;
        if should_wrap {
            wrap(&text, "text")
        } else {
            text
        }
    }

    /// `_element_head_prefix` without provenance or meta: label, href, layer,
    /// then the caption.
    fn head(&self, id: usize, label: Option<&str>, caption: &str, inline: bool) -> String {
        let item = &self.tree.items[id];
        let mut s = String::new();
        if let Some(l) = label {
            s.push_str(&format!("<label value=\"{}\"/>", escape_attr(l)));
        }
        if !inline {
            if let TreeKind::Text {
                hyperlink: Some(uri),
                ..
            }
            | TreeKind::Code {
                hyperlink: Some(uri),
                ..
            } = &item.kind
            {
                s.push_str(&format!("<href uri=\"{}\"/>", escape_attr(uri)));
            }
            if let Some(layer) = item.layer {
                s.push_str(&layer_token(layer));
            }
        }
        s.push_str(caption);
        s
    }

    /// `serialize_captions`: each caption as a `<caption>` satellite — its
    /// head, its own text, then its children (an inline group of runs).
    fn captions(&mut self, id: usize) -> String {
        let tree = self.tree;
        let mut out = String::new();
        for &c in captions_of(&tree.items[id].kind) {
            let TreeKind::Text { text, .. } = &tree.items[c].kind else {
                continue;
            };
            let mut s = self.head(c, None, "", false);
            if !text.is_empty() {
                s.push_str(&escape(text));
            }
            for &child in &tree.items[c].children {
                s.push_str(&self.serialize(child, false));
            }
            if !s.is_empty() {
                out.push_str(&wrap(&s, "caption"));
            }
        }
        out
    }

    /// `DocLangTextSerializer._serialize_single_item`.
    fn text(&mut self, id: usize, inline: bool) -> String {
        let tree = self.tree;
        let item = &tree.items[id];
        let (label, text, formatting) = match &item.kind {
            TreeKind::Text {
                label,
                text,
                formatting,
                ..
            } => (label.as_str(), text.as_str(), *formatting),
            TreeKind::Code {
                text, formatting, ..
            } => ("code", text.as_str(), *formatting),
            _ => unreachable!("text() on a non-text item"),
        };
        let is_list_item = label == "list_item";
        let open: Option<String> = match (&item.kind, label) {
            (TreeKind::Code { .. }, _) => Some("<code>".into()),
            (_, "title") => Some("<heading>".into()),
            (TreeKind::Text { level, .. }, "section_header") => {
                // Clamped like docling-core: level 1 is the title.
                let level = (level.unwrap_or(1) as usize + 1).min(6);
                Some(if level == 1 {
                    "<heading>".into()
                } else {
                    format!("<heading level=\"{level}\">")
                })
            }
            // `_determine_list_item_wrapper` without provenance: a text item
            // is bare (virtual text) unless its segment holds more.
            (_, "list_item") => {
                (!text.is_empty() && self.has_segment_siblings(id)).then(|| "<text>".into())
            }
            (_, "checkbox_selected" | "checkbox_unselected") => {
                let in_text = item.parent.is_some_and(|p| {
                    matches!(&tree.items[p].kind,
                        TreeKind::Text { text, .. } | TreeKind::Code { text, .. } if text.is_empty())
                });
                (!in_text).then(|| "<text>".into())
            }
            _ => Some(format!("<{}>", text_token(label).unwrap_or("text"))),
        };

        let ldiv = match &item.kind {
            TreeKind::Text { list: Some(m), .. } if is_list_item && !m.marker.is_empty() => {
                format!("<ldiv><marker>{}</marker></ldiv>", escape(&m.marker))
            }
            _ if is_list_item => "<ldiv/>".to_string(),
            _ => String::new(),
        };

        let code_label = match &item.kind {
            TreeKind::Code { language, .. } => language.as_deref().and_then(code_lang_label),
            _ => None,
        };
        // A code item is a floating item: its captions ride in its head.
        let caption = match &item.kind {
            TreeKind::Code { .. } => self.captions(id),
            _ => String::new(),
        };
        let mut res = self.head(id, code_label, &caption, inline);

        let children = &item.children;
        let checkbox = match label {
            "checkbox_selected" => Some("<checkbox class=\"selected\"/>"),
            "checkbox_unselected" => Some("<checkbox class=\"unselected\"/>"),
            _ => None,
        };
        let own = |ser: &Self| -> String {
            let mut t = ser.post_process(&escape(text), formatting);
            if label == "handwritten_text" {
                t = wrap(&t, "handwriting");
            } else if let Some(cb) = checkbox {
                t = format!("{cb}{t}");
            }
            t
        };
        let text_part = match children.first() {
            Some(&first) if self.is_inline_group(first) => {
                let runs = self.serialize(first, false);
                if text.is_empty() {
                    runs
                } else {
                    // Two runs of one inline flow: a plain space between them.
                    [own(self), runs]
                        .into_iter()
                        .filter(|p| !p.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ")
                }
            }
            Some(_) if text.is_empty() => {
                let mut subs = Vec::new();
                for &c in children {
                    if is_list_item && self.segment_sibling(c) {
                        continue;
                    }
                    subs.push(self.serialize(c, false));
                }
                subs.join(if inline { "" } else { DELIM })
            }
            _ => own(self),
        };
        res.push_str(&text_part);

        // An inline run of plain text is the host's text node, unwrapped.
        let bare_in_inline = inline
            && matches!(
                label,
                "text" | "handwritten_text" | "checkbox_selected" | "checkbox_unselected"
            );
        if let Some(open) = open.filter(|_| !bare_in_inline) {
            let tag = open[1..]
                .split([' ', '>'])
                .next()
                .unwrap_or("text")
                .to_string();
            res = format!("{open}{res}</{tag}>");
        }
        format!("{ldiv}{res}")
    }

    /// docling-core's `post_process` with DocLang's hooks: the formatting
    /// wrappers, then `<rtl>` for a right-to-left text.
    fn post_process(&self, text: &str, formatting: Option<crate::tree::Formatting>) -> String {
        let mut res = text.to_string();
        if let Some(f) = formatting {
            if f.bold {
                res = wrap(&res, "bold");
            }
            if f.italic {
                res = wrap(&res, "italic");
            }
            if f.underline {
                res = wrap(&res, "underline");
            }
            if f.strikethrough {
                res = wrap(&res, "strikethrough");
            }
            match f.script {
                crate::Script::Sub => res = wrap(&res, "subscript"),
                crate::Script::Super => res = wrap(&res, "superscript"),
                crate::Script::Baseline => {}
            }
        }
        if is_rtl(text) {
            res = wrap(&res, "rtl");
        }
        res
    }

    /// `DocLangTableSerializer` for a table without provenance: the caption
    /// head, then the OTSL grid — a rich cell's content serialized in place.
    fn table(&mut self, id: usize) -> String {
        let tree = self.tree;
        let TreeKind::Table {
            table, rich_cells, ..
        } = &tree.items[id].kind
        else {
            unreachable!("table() on a non-table item");
        };
        let rich: HashMap<(usize, usize), usize> =
            rich_cells.iter().map(|&(r, c, g)| ((r, c), g)).collect();
        let caption = self.captions(id);
        let otsl = self.otsl(table, &rich);
        let head = self.head(id, None, &caption, false);
        wrap(&format!("{head}{otsl}"), "table")
    }

    /// `_emit_otsl` without cell locations: the grid's OTSL tokens, a rich
    /// cell's content serialized in place.
    fn otsl(&mut self, table: &crate::Table, rich: &HashMap<(usize, usize), usize>) -> String {
        let data = crate::json::raw_table_data(table);
        let mut otsl = String::new();
        if data.has_cells() {
            let (rows, cols) = data.dims();
            for i in 0..rows {
                for j in 0..cols {
                    let cell = data.at(i, j);
                    let (r0, c0) = (cell.start_row, cell.start_col);
                    if r0 != i || c0 != j {
                        // A spanned slot: its content went out at the origin.
                        otsl.push_str(match (r0 != i, c0 != j) {
                            (true, true) => "<xcel/>",
                            (true, false) => "<ucel/>",
                            _ => "<lcel/>",
                        });
                        continue;
                    }
                    let rich_group = rich.get(&(i, j)).copied();
                    let content = match rich_group {
                        Some(g) => self.serialize(g, false).trim().to_string(),
                        None => cell.text.trim().to_string(),
                    };
                    let token = match (cell.column_header, cell.row_header) {
                        (true, true) => "corn",
                        _ if content.is_empty() => "ecel",
                        (true, false) => "ched",
                        (false, true) => "rhed",
                        _ if cell.row_section => "srow",
                        _ => "fcel",
                    };
                    otsl.push_str(&format!("<{token}/>"));
                    if !content.is_empty() && token != "ecel" {
                        otsl.push_str(&match rich_group {
                            Some(_) => content,
                            None => escape(&content),
                        });
                    }
                }
                otsl.push_str("<nl/>");
            }
        }
        otsl
    }

    /// `DocLangPictureSerializer`: the head (the classification's label,
    /// the captions, the description), then the body — the `<src>`, a
    /// chart's `<tabular>`, the picture's own children.
    fn picture(&mut self, id: usize) -> String {
        let tree = self.tree;
        let caption = self.captions(id);
        let mut body = String::new();
        if let TreeKind::Picture {
            image: Some(img), ..
        } = &tree.items[id].kind
        {
            // `REFERENCED` names the asset `_with_pictures_refs` saved — body
            // pictures only: any other keeps the data URI `ImageRef.from_pil`
            // gave it, which both modes write as is; `EMBEDDED` inlines it.
            let embedded = || Some(crate::pixel_digest::docling_data_uri(img).1);
            let uri = match self.pictures {
                Pictures::Placeholder => None,
                Pictures::Embedded => embedded(),
                Pictures::Referenced { .. } if !self.pic_index.contains_key(&id) => embedded(),
                Pictures::Referenced { .. } => self.pic_index.get(&id).copied().map(|idx| {
                    let path = format!(
                        "assets/image_{idx:06}_{}.png",
                        self.digests.digest(&img.data)
                    );
                    if self.assets.is_some() {
                        let bytes = self.digests.take_png(&img.data);
                        if let Some(assets) = self.assets.as_mut() {
                            assets.push((path.clone(), bytes));
                        }
                    }
                    path
                }),
            };
            if let Some(uri) = uri {
                body.push_str(&format!("<src uri=\"{}\"/>", escape_attr(&uri)));
            }
        }
        // A chart's data grid follows the `<src>` as its `<tabular>` (the
        // picture then `class="chart"`) — for a picture classified as a
        // chart only; any other keeps it out (`tabular_chart` is never a
        // `<custom>` field).
        let chart = match &tree.items[id].kind {
            TreeKind::Picture {
                chart: Some(t),
                classification: Some(c),
                ..
            } if is_chart_class(c) => Some(t),
            _ => None,
        };
        if let Some(t) = chart {
            let otsl = self.otsl(t, &HashMap::new());
            if !otsl.is_empty() {
                body.push_str(&wrap(&otsl, "tabular"));
            }
        }
        let caps = captions_of(&tree.items[id].kind);
        for &c in &tree.items[id].children {
            if caps.contains(&c) {
                continue;
            }
            body.push_str(&self.serialize(c, false));
        }
        // The classification is the head's `<label>` (AUTO label mode: not
        // when `undefined`); a description follows the captions.
        let (label, mut caption) = (picture_label(&tree.items[id].kind), caption);
        if let TreeKind::Picture {
            description: Some(d),
            ..
        } = &tree.items[id].kind
        {
            caption.push_str(&wrap(&escape(&d.text), "description"));
        }
        let head = self.head(id, label, &caption, false);
        let open = if body.contains("<tabular>") && chart.is_some() {
            "<picture class=\"chart\">"
        } else {
            "<picture>"
        };
        format!("{open}{head}{body}</picture>")
    }
}

/// `_picture_is_chart`: the main prediction is one of docling's chart classes.
fn is_chart_class(class: &str) -> bool {
    matches!(
        class,
        "pie_chart"
            | "bar_chart"
            | "stacked_bar_chart"
            | "line_chart"
            | "flow_chart"
            | "scatter_chart"
            | "heatmap"
    )
}

/// `_picture_classification_label_value` under the AUTO label mode: the
/// main prediction's class (`_picture_classification_label_to_doclang` maps
/// `other` to DocLang's `other`, i.e. keeps it), none for `undefined`.
fn picture_label(kind: &TreeKind) -> Option<&str> {
    match kind {
        TreeKind::Picture {
            classification: Some(c),
            ..
        } => Some(c.as_str()).filter(|c| *c != "undefined"),
        _ => None,
    }
}

fn is_list_label(label: &str) -> bool {
    matches!(label, "list" | "ordered_list")
}

fn captions_of(kind: &TreeKind) -> &[usize] {
    match kind {
        TreeKind::Table { captions, .. }
        | TreeKind::Picture { captions, .. }
        | TreeKind::Code { captions, .. } => captions,
        _ => &[],
    }
}

fn layer_token(layer: ContentLayer) -> String {
    format!(
        "<layer value=\"{}\"/>",
        crate::doclang_xml::layer_value(layer)
    )
}

/// docling's `iterate_items` walk from `id` (itself first), a picture's
/// children other than its captions left out (`traverse_pictures=False`).
fn pre_order(tree: &ItemTree, id: usize, out: &mut Vec<usize>) {
    let item = &tree.items[id];
    if item.deleted {
        return;
    }
    out.push(id);
    let caps = match &item.kind {
        TreeKind::Picture { captions, .. } => Some(captions),
        _ => None,
    };
    for &c in &item.children {
        if caps.is_some_and(|caps| !caps.contains(&c)) {
            continue;
        }
        pre_order(tree, c, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(label: &str, text: &str) -> TreeKind {
        TreeKind::Text {
            label: label.into(),
            text: text.into(),
            orig: None,
            formatting: None,
            hyperlink: None,
            level: None,
            list: (label == "list_item").then(Default::default),
        }
    }

    fn group(label: &str) -> TreeKind {
        TreeKind::Group {
            label: label.into(),
            name: "group".into(),
        }
    }

    /// The item tree's shapes the flat nodes lose (#884): a CDATA list item
    /// ends its line at the list's indent, a caption's inline formula is an
    /// element, a footnote keeps its label, a LaTeX `*` stays text — the
    /// string docling-core 2.106's `export_to_doclang()` writes for the same
    /// document.
    #[test]
    fn tree_shapes_follow_docling_core() {
        let mut t = ItemTree {
            inline_whitespace: true,
            ..Default::default()
        };
        let list = t.add(None, None, group("list"));
        t.add(Some(list), None, text("list_item", "\"C\" cell"));
        t.add(Some(list), None, text("list_item", "plain"));
        let pic = t.add(
            None,
            None,
            TreeKind::Picture {
                captions: Vec::new(),
                image: None,
                classification: None,
                extra_classes: Vec::new(),
                description: None,
                confidence: None,
                chart: None,
                dpi: None,
            },
        );
        let cap = t.add(None, None, text("caption", ""));
        let runs = t.add(Some(cap), None, group("inline"));
        t.add(Some(runs), None, text("text", "size "));
        t.add(Some(runs), None, text("formula", "k"));
        if let TreeKind::Picture { captions, .. } = &mut t.items[pic].kind {
            captions.push(cap);
        }
        t.add(None, None, text("footnote", "A note."));
        t.add(None, None, text("text", "Zhang* and Liu*"));
        let (xml, _) = export(&t, Pictures::Placeholder).expect("covered tree");
        assert_eq!(
            xml,
            "<doclang version=\"0.9\">\n  <list>\n    <ldiv/>\n    <![CDATA[\"C\" cell]]>\n    \
             <ldiv/>\n    plain\n  </list>\n  <picture>\n    <caption>size <formula>k</formula></caption>\n  \
             </picture>\n  <footnote>A note.</footnote>\n  <text>Zhang* and Liu*</text>\n</doclang>"
        );
    }

    /// A located item is outside the port: the caller falls back to the nodes.
    #[test]
    fn uncovered_tree_falls_back() {
        let mut t = ItemTree::default();
        let id = t.add(None, None, text("text", "x"));
        t.items[id].prov = Some(crate::tree::TreeProv {
            page_no: 1,
            bbox: [0.0, 0.0, 1.0, 1.0],
            bottom_left: false,
            charspan: [0, 1],
        });
        assert!(export(&t, Pictures::Placeholder).is_none());
    }

    #[test]
    fn escape_matches_docling_core() {
        assert_eq!(escape("a"), "a");
        assert_eq!(escape("a & b"), "<![CDATA[a & b]]>");
        assert_eq!(escape(" a"), "<content> a</content>");
        assert_eq!(escape("x]]>y"), "<![CDATA[x]]]]><![CDATA[>y]]>");
        assert_eq!(escape("a\u{1}"), "a[U+0001]");
    }
}
