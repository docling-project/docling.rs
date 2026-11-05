//! docling's item tree for DOCX — the structure the JSON export serializes.
//!
//! `docx.rs` walks `word/document.xml` into the flat node stream that
//! Markdown / DocLang / LaTeX render. Upstream's *JSON* has a different shape:
//! `MsWordDocumentBackend` nests everything after a heading under it (with
//! `header-N` section groups filling skipped levels), splits a paragraph of
//! mixed formatting into an `inline` group of one text item per formatting
//! run (each carrying `formatting` and `hyperlink`), manages list groups by
//! `numId`/`ilvl` through a stack of open parents, parents a rich table
//! cell's items to a `rich_cell_group_*` group under the table, wraps textbox
//! content in a `textbox` section, puts headers/footers in furniture-layer
//! `page header`/`page footer` sections, links reviewer comments through
//! `comments` back-refs — and numbers every item in creation order.
//!
//! This module ports that construction call-for-call (docling 2.126,
//! `_walk_linear` → `_handle_text_elements` / `_handle_tables` /
//! `_handle_pictures` / `_handle_textbox_content` / `_add_header_footer` /
//! `_add_comments`) into a [`docling_core::tree::ItemTree`] the JSON export
//! writes as it is. The XML readers it needs — styles, numbering, run text,
//! code detection, list markers, chart parts — are `docx.rs`'s, so the two
//! walks read the file the same way; only the item structure differs.
//!
//! What upstream reaches through its optional tools is done in-process
//! here: a blip-less DrawingML drawing (shapes, connectors, text boxes) is
//! rendered through LibreOffice upstream into a PNG picture, here into an
//! SVG payload (`docx_drawingml.rs`, #858); an EMF/WMF picture is rendered
//! to PNG in-process (`metafile.rs`, #536). A chart image is a classified
//! picture with its data and no payload (`_handle_chart`).

use std::collections::{HashMap, HashSet};

use docling_core::tree::{Formatting, ItemTree, ListMeta, TreeKind, TreeNote};
use docling_core::{
    ContentLayer, PictureDescription, PictureImage, Revision, RevisionKind, Script, Table,
    TableCell, TrackChanges,
};
use roxmltree::{Document, Node as XmlNode, NodeId};

use super::docx::{
    attr, build_enum_marker, caption_numbering_marker, chart_rels, child_elements,
    clean_checkbox_symbols, detect_code_language, footnote_texts, get_list_counter, grid_span,
    header_footer_parts, heading_labels, hyperlink_target, in_textbox, inherited_outline,
    is_code_by_font, is_code_style, is_title_style, numbered_heading_text, on_off,
    paragraph_anchors, part_rels, revision_of, row_cells, row_grid_offsets, row_kept,
    run_child_text, style_chain_bold, style_numbering, sync_heading_counter, t_text, Ctx,
    MAX_TABLE_DEPTH,
};
use super::html_tree::docling_href;
use super::ooxml::Package;

/// Open-parent slots: upstream's `parents` dict holds levels −1…9 and grows
/// as deeper list levels are opened; 64 covers any real nesting.
const LEVELS: usize = 64;

/// Upstream's `_get_paragraph_elements` tuple: `(text, formatting, hyperlink)`.
#[derive(Clone, Debug, PartialEq)]
struct Part {
    text: String,
    fmt: Option<Formatting>,
    link: Option<String>,
    /// The tracked change the run sits in, under `track_changes=all`
    /// (#851).
    revision: Option<Revision>,
}

/// A `w:footnoteReference` / `w:endnoteReference` placed in a text item
/// ([`Walker::note_calls`]).
struct NoteCall {
    /// The text item the call sits in.
    item: usize,
    /// Its position, in chars into that item's text.
    offset: usize,
    kind: &'static str,
    id: String,
}

/// A paragraph's identity: roxmltree node ids are per parsed document, and
/// header/footer parts are documents of their own.
type Key = (usize, NodeId);

/// One `_update_history` record: the previous paragraph's `numId` and
/// `ilvl` (its name and heading level are recorded upstream but never read).
#[derive(Clone, Default)]
struct Hist {
    numid: Option<String>,
    indent: Option<i64>,
}

/// Upstream's `last_list_group` / `_numid` / `_parent` cache.
#[derive(Clone)]
struct ListCache {
    group: usize,
    numid: String,
    parent: Option<usize>,
}

/// `_NestedList` (docling#4465): a list opened under an open list's item
/// for its larger left indentation — Word's "List Bullet 2" / "List Number
/// 2" styles each own a `numId` at `w:ilvl` 0 and differ from "List
/// Bullet" only by their indentation. `slot` and `start_ilevel` place the
/// nested list's own levels in the parents stack; the `outer_*` fields
/// describe the item it hangs from.
#[derive(Clone)]
struct NestedList {
    slot: i64,
    start_ilevel: i64,
    outer_numid: String,
    outer_ilevel: Option<i64>,
    outer_left_indent: i64,
}

/// The list-related state `_isolated_list_context` saves around a rich cell.
struct SavedListCtx {
    history: Vec<Hist>,
    level_at_new_list: Option<i64>,
    level_start_ilevel: i64,
    nested_lists: Vec<NestedList>,
    last_list_left_indent: i64,
    parents: Vec<Option<usize>>,
    cache: Option<ListCache>,
}

struct Walker {
    tree: ItemTree,
    /// `parents[k + 1]` is upstream's `self.parents[k]`, k from −1.
    parents: Vec<Option<usize>>,
    level: i64,
    level_at_new_list: Option<i64>,
    /// The `w:ilvl` the current list opened at (docling#4188): a list that
    /// starts above level 0 still lands at `level_at_new_list`.
    level_start_ilevel: i64,
    /// Lists nested under an open list's item by left indentation, innermost
    /// last (docling#4465).
    nested_lists: Vec<NestedList>,
    /// The last list item's effective left indentation in twips.
    last_list_left_indent: i64,
    numbered_headers: HashMap<u8, u64>,
    list_counters: HashMap<(String, i64), i64>,
    started_numids: HashSet<String>,
    last_numid: Option<String>,
    cache: Option<ListCache>,
    layer: Option<ContentLayer>,
    /// The layer around a hidden paragraph while it is added (docling#4958):
    /// the list groups it opens stay on it.
    layer_around_hidden: Option<Option<ContentLayer>>,
    history: Vec<Hist>,
    /// Which parsed document the node ids below belong to (0 = the body).
    doc_tag: usize,
    processed_textbox: HashSet<Key>,
    prev_sibling_is_code: bool,
    force_new_code_block: bool,
    pending_code_blank_lines: usize,
    /// `paragraph_to_items`: the items each paragraph produced.
    para_items: HashMap<Key, Vec<usize>>,
    /// Blank paragraphs before a resumed list item, deleted once the whole
    /// document is walked (docling#4674, #764): deleting them mid-walk would
    /// renumber the item refs a rich table cell or a comment still holds.
    deferred_blanks: Vec<usize>,
    /// Footnote / endnote references met in the body, anchored to the text
    /// item that holds them (#538) — resolved to the note bodies once those
    /// are read.
    note_calls: Vec<NoteCall>,
    /// `paragraph_comment_map`, in insertion order.
    para_comments: Vec<(Key, Vec<String>)>,
}

/// How many empty inline groups a rich table cell leaves behind on the body
/// (#527). `_create_or_reuse_parent` opens an inline group on the current
/// parent for every paragraph with more than one formatting run and adds
/// the runs to it, but the cell walk returns the runs themselves as
/// `provs_in_cell`, and `_group_cell_elements` moves them into the cell
/// group: the inline group stays where it was opened, empty, after the
/// table. Markdown, HTML and JSON-derived output never show it; DocLang
/// writes it as `<text></text>`.
pub(super) fn rich_cell_orphan_groups(tc: XmlNode, ctx: &Ctx) -> usize {
    let mut w = Walker::new();
    let provs = w.walk_linear(tc, ctx);
    let g = w.add(None, None, group_kind("unspecified", "rich_cell"));
    for p in provs {
        w.tree.reparent(p, Some(g));
    }
    w.tree
        .items
        .iter()
        .filter(|it| {
            !it.deleted
                && it.parent.is_none()
                && it.children.is_empty()
                && matches!(&it.kind, TreeKind::Group { label, .. } if label == "inline")
        })
        .count()
}

/// Build docling's item tree for a parsed `word/document.xml` body, its
/// section headers/footers and reviewer comments — `MsWordDocumentBackend.convert`.
pub(super) fn build_tree(
    pkg: &mut Package,
    body: XmlNode,
    ctx: &Ctx,
    comments: &[(String, String)],
) -> ItemTree {
    let mut w = Walker::new();
    w.extract_comment_ranges(body);
    w.walk_linear(body, ctx);
    w.add_header_footer(pkg, body, ctx);
    w.add_comments(comments);
    // `_add_footnotes_and_endnotes` (docling#4374): after the comments, each
    // note's body a furniture-layer `footnote` text on the body.
    let mut bodies: HashMap<(&'static str, String), (usize, String)> = HashMap::new();
    for note in footnote_texts(pkg, ctx.hidden) {
        let id = w.add(
            None,
            Some(ContentLayer::Furniture),
            text_kind("footnote", &note.text, None, None),
        );
        bodies.insert((note.kind, note.id), (id, note.text));
    }
    // #538: hang each note on the text item that calls it, for the Pandoc
    // AST's `Note` (docling's JSON keeps the bodies unlinked).
    for call in std::mem::take(&mut w.note_calls) {
        if let Some((body, text)) = bodies.get(&(call.kind, call.id)) {
            w.tree.items[call.item].notes.push(TreeNote {
                offset: call.offset,
                text: text.clone(),
            });
            w.tree.items[*body].note_body = true;
        }
    }
    // docling#4674 (#764): the blank paragraphs before resumed list items go
    // last, once every ref collected during the walk has been resolved.
    for blank in std::mem::take(&mut w.deferred_blanks) {
        w.tree.delete(blank);
    }
    w.tree
}

// ----- small XML helpers ------------------------------------------------------

/// Whether a node is OMML (`m:` namespace): its `t` / `r` are math, not text.
fn is_math(n: XmlNode) -> bool {
    n.tag_name()
        .namespace()
        .is_some_and(|ns| ns.contains("math"))
}

/// Whether a node is DrawingML (`a:` namespace — transitional or strict URI).
fn is_dml(n: XmlNode) -> bool {
    n.tag_name()
        .namespace()
        .is_some_and(|ns| ns.contains("drawingml"))
}

/// python-docx's `CT_R.text`.
fn run_text(r: XmlNode) -> String {
    child_elements(r).map(run_child_text).collect()
}

/// The `w:footnoteReference` / `w:endnoteReference` calls of a paragraph as
/// `(offset, kind, w:id)`, the offset in chars of the paragraph text that
/// [`Walker::iter_paragraph_content`] reads — the same traversal: runs and
/// hyperlinks directly under the paragraph (through `smartTag` / `customXml`
/// / `ins` / `moveTo` / `fldSimple`), a content control's text counted whole
/// (#538).
fn note_references(p: XmlNode) -> Vec<(usize, &'static str, String)> {
    fn walk(node: XmlNode, acc: &mut usize, out: &mut Vec<(usize, &'static str, String)>) {
        for c in child_elements(node) {
            if is_math(c) {
                continue;
            }
            match c.tag_name().name() {
                "smartTag" | "customXml" | "ins" | "moveTo" | "fldSimple" => walk(c, acc, out),
                "r" => run(c, acc, out),
                "hyperlink" => {
                    for r in c.children().filter(|n| n.has_tag_name("r") && !is_math(*n)) {
                        run(r, acc, out);
                    }
                }
                "sdt" => {
                    *acc += c
                        .descendants()
                        .filter(|n| n.has_tag_name("t") && !is_math(*n))
                        .filter(|n| n.ancestors().any(|a| a.has_tag_name("sdtContent")))
                        .map(|t| t_text(t).chars().count())
                        .sum::<usize>();
                }
                _ => {}
            }
        }
    }
    fn run(r: XmlNode, acc: &mut usize, out: &mut Vec<(usize, &'static str, String)>) {
        for c in child_elements(r) {
            let kind = match c.tag_name().name() {
                "footnoteReference" => "footnote",
                "endnoteReference" => "endnote",
                _ => {
                    *acc += run_child_text(c).chars().count();
                    continue;
                }
            };
            if let Some(id) = attr(c, "id") {
                out.push((*acc, kind, id.to_string()));
            }
        }
    }
    if !p
        .descendants()
        .any(|n| n.has_tag_name("footnoteReference") || n.has_tag_name("endnoteReference"))
    {
        return Vec::new();
    }
    let mut out = Vec::new();
    walk(p, &mut 0, &mut out);
    out
}

/// python-docx's `Paragraph.text`: the runs and hyperlinks directly under the
/// paragraph (`w:r | w:hyperlink`), nothing inside content controls or
/// tracked insertions.
fn py_paragraph_text(p: XmlNode) -> String {
    paragraph_text(p, false, TrackChanges::Accept)
}

/// The runs and hyperlinks of a paragraph; with `accepted`, also those inside
/// tracked insertions and move destinations (`w:ins`, `w:moveTo`) — the
/// paragraph as Word shows it with the changes accepted — and inside the
/// inline wrappers Word puts around a span (`w:sdt`, `w:customXml`,
/// `w:smartTag`, `w:fldSimple`; #628): python-docx's `CT_P.text` reads
/// `w:r | w:hyperlink` only, so a plain cell whose text sat in a content
/// control or a field result came out empty in docling's JSON while the
/// Markdown walk (`push_inline_text`) printed it — the same loss #545 fixed
/// for tracked changes, found by the chain-parity matrix.
fn paragraph_text(p: XmlNode, accepted: bool, policy: TrackChanges) -> String {
    let mut out = String::new();
    for c in child_elements(p) {
        if is_math(c) {
            continue;
        }
        match c.tag_name().name() {
            "r" => out.push_str(&run_text(c)),
            "hyperlink" => {
                for r in c.children().filter(|n| n.has_tag_name("r") && !is_math(*n)) {
                    out.push_str(&run_text(r));
                }
            }
            // Tracked changes (#851): the view's runs only.
            "ins" | "moveTo" if accepted && policy != TrackChanges::Reject => {
                out.push_str(&paragraph_text(c, true, policy))
            }
            "del" | "moveFrom" if accepted && policy != TrackChanges::Accept => {
                out.push_str(&paragraph_text(c, true, policy))
            }
            "customXml" | "smartTag" | "fldSimple" if accepted => {
                out.push_str(&paragraph_text(c, true, policy))
            }
            // A checkbox control's glyph (`w14:checkbox`, its `w:t` is ☐/☒)
            // stays out as in python-docx: the state is already the cell's
            // `checkbox_selected` child items, and upstream's cell `text`
            // for the mirrored `docx_checkboxes` is matched exactly.
            "sdt" if accepted && !c.descendants().any(|n| n.has_tag_name("checkbox")) => {
                for content in c.children().filter(|n| n.has_tag_name("sdtContent")) {
                    out.push_str(&paragraph_text(content, true, policy));
                }
            }
            _ => {}
        }
    }
    out
}

/// A cell's text: python-docx's `_Cell.text` (its direct paragraphs joined
/// with `\n`), except that tracked insertions and move destinations count
/// (#545), and so does text behind an inline content control, custom XML
/// tag, smart tag or simple field (#628). python-docx skips them, so docling
/// writes an empty cell for one whose text was inserted or moved there with
/// track changes on, and — a plain cell having no child items — the JSON
/// lost that text outright; Markdown always printed it.
fn py_cell_text(tc: XmlNode, policy: TrackChanges) -> String {
    tc.children()
        .filter(|n| n.has_tag_name("p"))
        .map(|p| paragraph_text(p, true, policy))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A cell's `tcPr` child element by name.
fn tc_pr<'a, 'i>(tc: XmlNode<'a, 'i>, name: &str) -> Option<XmlNode<'a, 'i>> {
    tc.children()
        .find(|n| n.has_tag_name("tcPr"))
        .and_then(|pr| pr.children().find(|n| n.has_tag_name(name)))
}

/// Upstream's `_has_blip`: a child holding a blip or a drawing anywhere.
/// docling's `_has_textbox`: `.//w:txbxContent | .//v:textbox//w:p |
/// .//wps:txbx//w:p | .//w10:wrap//w:p | .//a:bodyPr/ancestor::*//a:t |
/// .//a:txBody//a:t`. The `ancestor::*` alternative reaches the document
/// root, so an `a:bodyPr` in the element counts as soon as the document has
/// any `a:t`.
fn has_textbox(el: XmlNode) -> bool {
    let below = |n: XmlNode, names: &[&str]| {
        n.ancestors()
            .skip(1)
            .take_while(|a| a.id() != el.id())
            .any(|a| names.contains(&a.tag_name().name()))
    };
    let is_a = |n: XmlNode, name: &str| n.tag_name().name() == name && is_dml(n);
    let any_a_t = || el.document().root().descendants().any(|n| is_a(n, "t"));
    el.descendants().skip(1).any(|n| {
        n.has_tag_name("txbxContent")
            || (n.has_tag_name("p") && below(n, &["textbox", "txbx", "wrap"]))
            || (is_a(n, "t")
                && n.ancestors()
                    .skip(1)
                    .take_while(|a| a.id() != el.id())
                    .any(|a| is_a(a, "txBody")))
    }) || (el.descendants().skip(1).any(|n| is_a(n, "bodyPr")) && any_a_t())
}

fn has_blip(el: XmlNode) -> bool {
    child_elements(el).any(|item| {
        item.descendants()
            .any(|n| n.has_tag_name("blip") || n.has_tag_name("drawing"))
    })
}

/// The `w:id`s of the given marker elements inside `el`, first-seen order.
fn ids_of(el: XmlNode, tags: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in el.descendants() {
        if tags.contains(&n.tag_name().name()) {
            if let Some(id) = attr(n, "id") {
                if !out.iter().any(|x| x == id) {
                    out.push(id.to_string());
                }
            }
        }
    }
    out
}

/// Python's `re.sub(r"\s+", "", s)`.
fn strip_ws(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Upstream's `_is_invisible_spacer` on the decoded picture: a tiny image, a
/// fully transparent one, or a pure white RGB box is a layout artifact.
fn is_invisible_spacer(img: &PictureImage) -> bool {
    if (img.width as u64) * (img.height as u64) <= 25 {
        return true;
    }
    let Ok(decoded) = image::load_from_memory(&img.data) else {
        return false;
    };
    match decoded {
        image::DynamicImage::ImageRgba8(i) => i.pixels().all(|p| p.0[3] == 0),
        image::DynamicImage::ImageRgba16(i) => i.pixels().all(|p| p.0[3] == 0),
        image::DynamicImage::ImageLumaA8(i) => i.pixels().all(|p| p.0[1] == 0),
        image::DynamicImage::ImageLumaA16(i) => i.pixels().all(|p| p.0[1] == 0),
        image::DynamicImage::ImageRgb8(i) => i.pixels().all(|p| p.0 == [255, 255, 255]),
        image::DynamicImage::ImageRgb16(i) => i.pixels().all(|p| p.0 == [65535, 65535, 65535]),
        _ => false,
    }
}

fn text_kind(label: &str, text: &str, fmt: Option<Formatting>, link: Option<String>) -> TreeKind {
    TreeKind::Text {
        label: label.into(),
        text: text.into(),
        orig: None,
        formatting: fmt,
        hyperlink: link,
        level: None,
        list: None,
    }
}

fn group_kind(label: &str, name: &str) -> TreeKind {
    TreeKind::Group {
        label: label.into(),
        name: name.into(),
    }
}

impl Walker {
    fn new() -> Self {
        Walker {
            tree: ItemTree::default(),
            parents: vec![None; LEVELS],
            level: 0,
            level_at_new_list: None,
            level_start_ilevel: 0,
            nested_lists: Vec::new(),
            last_list_left_indent: 0,
            numbered_headers: HashMap::new(),
            list_counters: HashMap::new(),
            started_numids: HashSet::new(),
            last_numid: None,
            cache: None,
            layer: None,
            layer_around_hidden: None,
            history: vec![Hist::default()],
            doc_tag: 0,
            processed_textbox: HashSet::new(),
            prev_sibling_is_code: false,
            force_new_code_block: false,
            pending_code_blank_lines: 0,
            para_items: HashMap::new(),
            para_comments: Vec::new(),
            deferred_blanks: Vec::new(),
            note_calls: Vec::new(),
        }
    }

    // ----- parents stack ----------------------------------------------------

    fn key(&self, n: XmlNode) -> Key {
        (self.doc_tag, n.id())
    }

    /// `self.parents[k]` (`None` for a level never opened).
    fn parent_at(&self, k: i64) -> Option<usize> {
        if k < -1 {
            return None;
        }
        self.parents.get((k + 1) as usize).copied().flatten()
    }

    fn set_parent(&mut self, k: i64, v: Option<usize>) {
        if k >= -1 {
            if let Some(slot) = self.parents.get_mut((k + 1) as usize) {
                *slot = v;
            }
        }
    }

    /// `_get_level`: the first non-negative level with no open parent.
    fn get_level(&self) -> i64 {
        (0..(LEVELS as i64 - 1))
            .find(|&k| self.parent_at(k).is_none())
            .unwrap_or(0)
    }

    /// `for key in parents: if key >= from: parents[key] = None` (non-negative
    /// keys; −1 is never set).
    fn clear_parents_from(&mut self, from: i64) {
        for k in from.max(0)..(LEVELS as i64 - 1) {
            self.set_parent(k, None);
        }
    }

    fn is_list_group(&self, id: Option<usize>) -> bool {
        id.is_some_and(|i| {
            matches!(&self.tree.items[i].kind, TreeKind::Group { label, .. } if label == "list")
        })
    }

    /// `_sub_list_parent` (docling#4863): a sub-list belongs to the list
    /// item above it — a new list group under a list group whose last child
    /// is a list item goes under that item, as the HTML, Markdown, PPTX and
    /// ODF trees nest it.
    fn sub_list_parent(&self, parent: Option<usize>) -> Option<usize> {
        let last = parent
            .filter(|_| self.is_list_group(parent))
            .and_then(|p| self.tree.items[p].children.last().copied());
        match last.map(|c| &self.tree.items[c].kind) {
            Some(TreeKind::Text { label, .. }) if label == "list_item" => last,
            _ => parent,
        }
    }

    fn is_code(&self, id: Option<usize>) -> bool {
        id.is_some_and(|i| matches!(self.tree.items[i].kind, TreeKind::Code { .. }))
    }

    /// `_last_child_item`: the last child of `parent` (`None` = the body).
    fn last_child(&self, parent: Option<usize>) -> Option<usize> {
        match parent {
            Some(p) => self.tree.items[p].children.last().copied(),
            None => self.tree.body.last().copied(),
        }
    }

    fn add(&mut self, parent: Option<usize>, layer: Option<ContentLayer>, kind: TreeKind) -> usize {
        self.tree.add(parent, layer, kind)
    }

    // ----- history / list cache -------------------------------------------------

    fn update_history(&mut self, numid: Option<String>, indent: Option<i64>) {
        self.history.push(Hist { numid, indent });
    }

    fn prev_numid(&self) -> Option<String> {
        self.history.last().and_then(|h| h.numid.clone())
    }

    fn prev_indent(&self) -> Option<i64> {
        self.history.last().and_then(|h| h.indent)
    }

    fn clear_list_cache(&mut self) {
        self.cache = None;
    }

    /// `_end_list_on_body_text`.
    fn end_list_on_body_text(&mut self, text: &str) {
        if !text.is_empty() {
            self.clear_list_cache();
        }
    }

    fn can_reuse_list_group(&self, numid: &str, parent: Option<usize>) -> bool {
        self.last_numid.as_deref() == Some(numid)
            && self
                .cache
                .as_ref()
                .is_some_and(|c| c.numid == numid && c.parent == parent)
    }

    /// `_get_or_create_list_group`.
    fn get_or_create_list_group(
        &mut self,
        numid: &str,
        parent: Option<usize>,
        refs: &mut Vec<usize>,
    ) -> usize {
        if self.can_reuse_list_group(numid, parent) {
            // Reuse only if nothing but empty paragraphs (added when the list
            // was closed) follows the cached group in its parent; otherwise
            // the new items would land before the intervening content, e.g.
            // a table (docling#4305). The blanks are dropped on reuse.
            let group = self.cache.as_ref().map(|c| c.group).unwrap_or(0);
            let container: Vec<usize> = match parent {
                Some(p) => self.tree.items[p].children.clone(),
                None => (0..self.tree.items.len())
                    .filter(|&i| self.tree.items[i].parent.is_none())
                    .collect(),
            };
            let mut trailing_empty: Vec<usize> = Vec::new();
            for &id in container.iter().rev() {
                let blank = matches!(&self.tree.items[id].kind,
                    TreeKind::Text { text, .. } if text.trim().is_empty());
                if blank {
                    trailing_empty.push(id);
                    continue;
                }
                if id == group {
                    // Deleted at the end of the walk (`delete_deferred_blanks`):
                    // a later resume finds the same blanks again.
                    for blank in trailing_empty {
                        if !self.deferred_blanks.contains(&blank) {
                            self.deferred_blanks.push(blank);
                        }
                    }
                    return group;
                }
                break;
            }
        }
        let g = self.add(
            parent,
            self.layer_around_hidden.unwrap_or(self.layer),
            group_kind("list", "list"),
        );
        refs.push(g);
        self.cache = Some(ListCache {
            group: g,
            numid: numid.to_string(),
            parent,
        });
        g
    }

    fn save_list_ctx(&mut self) -> SavedListCtx {
        let saved = SavedListCtx {
            history: self.history.clone(),
            level_at_new_list: self.level_at_new_list,
            level_start_ilevel: self.level_start_ilevel,
            nested_lists: self.nested_lists.clone(),
            last_list_left_indent: self.last_list_left_indent,
            parents: self.parents.clone(),
            cache: self.cache.clone(),
        };
        self.clear_list_cache();
        saved
    }

    fn restore_list_ctx(&mut self, saved: SavedListCtx) {
        self.history = saved.history;
        self.level_at_new_list = saved.level_at_new_list;
        self.level_start_ilevel = saved.level_start_ilevel;
        self.nested_lists = saved.nested_lists;
        self.last_list_left_indent = saved.last_list_left_indent;
        self.parents = saved.parents;
        self.cache = saved.cache;
    }

    // ----- runs, formatting, paragraph content ------------------------------

    /// `_get_format_from_run`. `ppr_owner` is the element python-docx's
    /// `run._parent._element` resolves to — the paragraph for a run created
    /// against it (its paragraph-mark `pPr/rPr/b` counts; docling 2.140
    /// builds a hyperlink's runs as `Run(r_el, paragraph)` too, so the mark
    /// reaches link text), `None` for a run checked without a paragraph (the
    /// rich-cell test). `paragraph` enables the paragraph style chain's bold.
    fn format_from_run(
        &self,
        r: XmlNode,
        ppr_owner: Option<XmlNode>,
        paragraph: Option<XmlNode>,
        ctx: &Ctx,
    ) -> Formatting {
        let rpr = r.children().find(|n| n.has_tag_name("rPr"));
        let prop = |name: &str| rpr.and_then(|pr| pr.children().find(|n| n.has_tag_name(name)));
        let mut bold = prop("b").is_some_and(|b| on_off(attr(b, "val")));
        let raw_on = |n: XmlNode| !matches!(attr(n, "val"), Some("0" | "false"));
        // `<w:bCs>` (complex-script bold) is deliberately not a bold signal
        // here or in the paragraph mark, as in docling 2.133 (#587): Word
        // emits it as a font-theme artefact next to `w:szCs` / `rFonts
        // cstheme` without the user applying bold, and writes `<w:b>` even
        // for Arabic text when they do — so `<w:b>` alone is reliable. The
        // Markdown walk (`docx.rs`) never read `bCs`; reading it here made a
        // plain cell a RichTableCell with a `<strong>` the document has not.
        if !bold {
            // Any `<w:b>` in the run, then the paragraph mark's.
            bold = r.descendants().any(|n| n.has_tag_name("b") && raw_on(n));
        }
        if !bold {
            if let Some(owner) = ppr_owner {
                bold = owner
                    .children()
                    .find(|n| n.has_tag_name("pPr"))
                    .and_then(|pr| pr.children().find(|n| n.has_tag_name("rPr")))
                    .is_some_and(|rp| rp.children().any(|n| n.has_tag_name("b") && raw_on(n)));
            }
        }
        if !bold {
            if let Some(p) = paragraph {
                // Climb the paragraph style's `basedOn` chain for `font.bold`.
                bold = style_chain_bold(super::docx::paragraph_style_id(p, ctx), ctx);
            }
        }
        let italic = prop("i").is_some_and(|n| on_off(attr(n, "val")));
        let strikethrough = prop("strike").is_some_and(|n| on_off(attr(n, "val")));
        // python-docx's `underline`: `none` is False, any other value truthy.
        let underline = prop("u").is_some_and(|n| attr(n, "val") != Some("none"));
        let script = match prop("vertAlign").and_then(|n| attr(n, "val")) {
            Some("subscript") => Script::Sub,
            Some("superscript") => Script::Super,
            _ => Script::Baseline,
        };
        Formatting {
            bold,
            italic,
            underline,
            strikethrough,
            script,
        }
    }

    /// `_get_hyperlink_target`: the relationship's target as docling
    /// serializes it (`AnyUrl` with a scheme, `Path` without) — or, past
    /// docling, a bookmark's `#id` (#849, [`hyperlink_target`]).
    fn hyperlink_target(&self, h: XmlNode, ctx: &Ctx) -> Option<String> {
        let address = hyperlink_target(h, ctx)?;
        if address.starts_with('#') {
            return Some(address);
        }
        Some(docling_href(&address))
    }

    /// `_iter_paragraph_content`: one part per run, hyperlink or content
    /// control, recursing through `smartTag` / `customXml` / `ins` /
    /// `fldSimple` wrappers only — and `moveTo`, a tracked move's destination,
    /// which docling skips (its text vanished, #545; the source `moveFrom`
    /// stays out, like a deletion).
    fn iter_paragraph_content(&self, p: XmlNode, ctx: &Ctx) -> Vec<Part> {
        // Each child with the tracked change it sits in (#851): under
        // `accept` an insertion's runs read as plain and a deletion's are
        // left out, under `reject` the reverse, under `all` both read,
        // typed; a move's destination counts as an insertion, its source
        // as a deletion (Pandoc's reading).
        fn children_recursive<'a, 'i>(
            node: XmlNode<'a, 'i>,
            policy: TrackChanges,
            rev: Option<&Revision>,
            out: &mut Vec<(XmlNode<'a, 'i>, Option<Revision>)>,
        ) {
            for c in child_elements(node) {
                if is_math(c) {
                    out.push((c, rev.cloned()));
                    continue;
                }
                match c.tag_name().name() {
                    "smartTag" | "customXml" | "fldSimple" => {
                        children_recursive(c, policy, rev, out)
                    }
                    "ins" | "moveTo" => match policy {
                        TrackChanges::Reject => {}
                        TrackChanges::Accept => children_recursive(c, policy, rev, out),
                        TrackChanges::All => children_recursive(
                            c,
                            policy,
                            Some(&revision_of(c, RevisionKind::Insertion)),
                            out,
                        ),
                    },
                    "del" | "moveFrom" => match policy {
                        TrackChanges::Accept => {}
                        TrackChanges::Reject => children_recursive(c, policy, rev, out),
                        TrackChanges::All => children_recursive(
                            c,
                            policy,
                            Some(&revision_of(c, RevisionKind::Deletion)),
                            out,
                        ),
                    },
                    _ => out.push((c, rev.cloned())),
                }
            }
        }
        let mut children = Vec::new();
        children_recursive(p, ctx.policy(), None, &mut children);
        let mut parts = Vec::new();
        for (child, revision) in children {
            if is_math(child) {
                continue;
            }
            match child.tag_name().name() {
                "sdt" => {
                    let in_content = |n: XmlNode| {
                        n.ancestors()
                            .skip(1)
                            .take_while(|a| a.id() != child.id())
                            .any(|a| a.has_tag_name("sdtContent"))
                    };
                    let text: String = child
                        .descendants()
                        .filter(|n| n.has_tag_name("t") && !is_math(*n) && in_content(*n))
                        .map(t_text)
                        .collect();
                    if text.is_empty() {
                        continue;
                    }
                    let fmt = child
                        .descendants()
                        .find(|n| n.has_tag_name("r") && !is_math(*n) && in_content(*n))
                        .map(|r| self.format_from_run(r, Some(p), Some(p), ctx));
                    parts.push(Part {
                        text,
                        fmt,
                        link: None,
                        revision,
                    });
                }
                "r" => parts.push(Part {
                    text: run_text(child),
                    fmt: Some(self.format_from_run(child, Some(p), Some(p), ctx)),
                    link: None,
                    revision,
                }),
                "hyperlink" => {
                    // A link's runs may sit in the same wrappers as a
                    // paragraph's — a tracked insertion made inside the
                    // link text (docling#4576: python-docx's
                    // `Hyperlink.runs` reads the direct `w:r` alone).
                    let mut nested = Vec::new();
                    children_recursive(child, ctx.policy(), revision.as_ref(), &mut nested);
                    let runs: Vec<XmlNode> = nested
                        .into_iter()
                        .map(|(n, _)| n)
                        .filter(|n| n.has_tag_name("r") && !is_math(*n))
                        .collect();
                    let text: String = runs.iter().map(|r| run_text(*r)).collect();
                    let fmt = runs
                        .first()
                        .map(|r| self.format_from_run(*r, Some(p), Some(p), ctx));
                    parts.push(Part {
                        text,
                        fmt,
                        link: self.hyperlink_target(child, ctx),
                        revision,
                    });
                }
                _ => {}
            }
        }
        parts
    }

    /// `_get_paragraph_elements`: runs grouped by formatting, a hyperlink
    /// always its own element, an empty paragraph one empty element.
    fn paragraph_elements(&self, content: &[Part]) -> Vec<Part> {
        paragraph_elements(content)
    }
}

/// `_get_paragraph_elements` (see [`Walker::paragraph_elements`]).
fn paragraph_elements(content: &[Part]) -> Vec<Part> {
    {
        let full: String = content.iter().map(|c| c.text.as_str()).collect();
        if full.trim().is_empty() {
            return vec![Part {
                text: String::new(),
                fmt: None,
                link: None,
                revision: None,
            }];
        }
        let mut elements = Vec::new();
        let mut group_text = String::new();
        let mut previous_format: Option<Formatting> = None;
        // A tracked change splits a run like a format change does (#851).
        let mut previous_revision: Option<Revision> = None;
        for part in content {
            let mut text = part.text.as_str();
            if (!text.trim().is_empty()
                && (part.fmt != previous_format || part.revision != previous_revision))
                || part.link.is_some()
            {
                if !group_text.trim().is_empty() {
                    elements.push(Part {
                        text: group_text.trim().to_string(),
                        fmt: previous_format,
                        link: None,
                        revision: previous_revision.clone(),
                    });
                }
                group_text.clear();
                if part.link.is_some() {
                    elements.push(Part {
                        text: text.trim().to_string(),
                        fmt: part.fmt,
                        link: part.link.clone(),
                        revision: part.revision.clone(),
                    });
                    text = "";
                } else {
                    previous_format = part.fmt;
                    previous_revision = part.revision.clone();
                }
            }
            group_text.push_str(text);
        }
        if !group_text.trim().is_empty() {
            elements.push(Part {
                text: group_text.trim().to_string(),
                fmt: previous_format,
                link: None,
                revision: previous_revision,
            });
        }
        elements
    }
}

/// `_handle_equations_in_text`: the paragraph text with every `oMath`
/// spliced in as `<eq>latex</eq>`, plus the bookended equations — or the
/// text untouched (and no equations) when the run texts do not
/// reconstruct it.
fn equations_in_text(element: XmlNode, text: &str) -> (String, Vec<String>) {
    {
        let mut only_texts: Vec<String> = Vec::new();
        let mut only_equations: Vec<String> = Vec::new();
        let mut texts_and_equations: Vec<String> = Vec::new();
        let is_omath = |n: XmlNode| n.has_tag_name("oMath") && is_math(n);
        let push_eq = |n: XmlNode, eqs: &mut Vec<String>, all: &mut Vec<String>| {
            let latex = super::omml::to_latex(n).trim().to_string();
            if !latex.is_empty() {
                let eq = format!("<eq>{latex}</eq>");
                eqs.push(eq.clone());
                all.push(eq);
            }
        };
        let direct: Vec<XmlNode> = child_elements(element).filter(|c| is_omath(*c)).collect();
        if !direct.is_empty() {
            for child in child_elements(element) {
                if is_omath(child) {
                    push_eq(child, &mut only_equations, &mut texts_and_equations);
                } else {
                    for t in child
                        .descendants()
                        .filter(|n| n.has_tag_name("t") && !is_math(*n))
                    {
                        let s = t_text(t);
                        only_texts.push(s.to_string());
                        texts_and_equations.push(s.into_owned());
                    }
                }
            }
        } else {
            for n in element.descendants() {
                if n.has_tag_name("t") && !is_math(n) {
                    let s = t_text(n);
                    only_texts.push(s.to_string());
                    texts_and_equations.push(s.into_owned());
                } else if is_omath(n) {
                    push_eq(n, &mut only_equations, &mut texts_and_equations);
                }
            }
        }
        if only_equations.is_empty() {
            return (text.to_string(), Vec::new());
        }
        if strip_ws(&only_texts.concat()) != strip_ws(text) {
            return (text.to_string(), Vec::new());
        }
        // Re-insert the equations into the original text, keeping its
        // whitespace structure.
        let mut output = String::new();
        let mut pos = 0usize;
        for sub in &texts_and_equations {
            if sub.is_empty() {
                continue;
            }
            if sub.starts_with("<eq>") {
                output.push_str(sub);
            } else if let Some(found) = text.get(pos..).and_then(|rest| rest.find(sub.as_str())) {
                output.push_str(sub);
                pos += found + sub.len();
            } else {
                output.push_str(sub);
            }
        }
        (output, only_equations)
    }
}

impl Walker {
    // ----- labels, numbering ----------------------------------------------------

    /// `_get_heading_and_level` with `_split_text_and_number`.
    fn heading_and_level(label: &str) -> (String, Option<i64>) {
        let is_digit = |c: char| c.is_ascii_digit();
        // `re.match(r"(\D+)(\d+)$|^(\d+)(\D+)", label)`
        let parts: Option<(String, String)> = {
            let trailing = label.trim_end_matches(is_digit);
            if trailing.len() < label.len() && !trailing.is_empty() && !trailing.contains(is_digit)
            {
                Some((trailing.to_string(), label[trailing.len()..].to_string()))
            } else if label.starts_with(is_digit) {
                let digits_end = label.find(|c: char| !is_digit(c)).unwrap_or(label.len());
                let rest = &label[digits_end..];
                let text_end = rest.find(is_digit).unwrap_or(rest.len());
                (text_end > 0).then(|| {
                    (
                        label[..digits_end].to_string(),
                        rest[..text_end].to_string(),
                    )
                })
            } else {
                None
            }
        };
        let Some((a, b)) = parts else {
            return (label.to_string(), None);
        };
        let mut sorted = [a, b];
        sorted.sort();
        let mut label_str = String::new();
        let mut level: Option<i64> = Some(0);
        if sorted[0].trim().eq_ignore_ascii_case("heading") {
            label_str = "Heading".into();
            level = sorted[1].parse().ok();
        }
        if sorted[1].trim().eq_ignore_ascii_case("heading") {
            label_str = "Heading".into();
            level = sorted[0].parse().ok();
        }
        if let Some(l) = level {
            if l < 1 {
                level = Some(1);
            }
        }
        (label_str, level)
    }

    /// `_get_label_and_level`: `("Heading", n)`, `("Code", None)`,
    /// `("Title", None)`, or the style id itself.
    fn label_and_level(&self, p: XmlNode, ctx: &Ctx) -> (String, Option<i64>) {
        let style_id = super::docx::paragraph_style_id(p, ctx);
        // python-docx resolves an unstyled paragraph to the default paragraph
        // style; its id only names the label (the font-based code check still
        // runs, on the runs' own fonts — `docx.rs` reads an unstyled
        // paragraph the same way).
        let label = if style_id.is_empty() {
            "Normal"
        } else {
            style_id
        };
        let name = ctx.style_names.get(label).map(String::as_str).unwrap_or("");
        if label.contains(':') {
            let parts: Vec<&str> = label.split(':').collect();
            if parts.len() == 2 {
                return (parts[0].to_string(), parts[1].parse().ok());
            }
        }
        // The id, name and `basedOn` ancestors up the whole chain (#841;
        // docling stops at one hop), the outline level inherited likewise.
        let labels = heading_labels(label, name, ctx);
        let is_heading = labels.iter().any(|l| l.to_lowercase().contains("heading"));
        let outline = inherited_outline(label, ctx)
            .map(|l| l as i64)
            .filter(|l| (1..=9).contains(l));
        if is_heading {
            if let Some(o) = outline {
                return ("Heading".into(), Some(o));
            }
            if let Some(l) = labels.iter().find(|l| l.to_lowercase().contains("heading")) {
                return Self::heading_and_level(l);
            }
        }
        if is_code_style(style_id, ctx)
            || is_code_by_font(p, style_id, ctx, self.prev_sibling_is_code)
        {
            return ("Code".into(), None);
        }
        // docling#4480: `use_outline_level_for_headings=False` keeps an
        // outline level from promoting a style no name marks a heading.
        if let Some(o) = outline.filter(|_| ctx.outline_headings) {
            if !is_title_style(style_id, name, ctx) {
                return ("Heading".into(), Some(o));
            }
        }
        (label.to_string(), None)
    }

    /// `_get_numId_and_ilvl`: the paragraph's own `numPr`, else its style's
    /// (resolved through `basedOn`).
    fn num_id_and_ilvl(&self, p: XmlNode, ctx: &Ctx) -> (Option<String>, Option<i64>) {
        if let Some(np) = super::docx::own_num_pr(p) {
            let numid = np
                .children()
                .find(|n| n.has_tag_name("numId"))
                .and_then(|n| attr(n, "val"))
                .and_then(|v| v.parse::<i64>().ok())
                .map(|n| n.to_string());
            let ilvl = np
                .children()
                .find(|n| n.has_tag_name("ilvl"))
                .and_then(|n| attr(n, "val"))
                .and_then(|v| v.parse::<i64>().ok());
            return (numid, ilvl);
        }
        match style_numbering(super::docx::paragraph_style_id(p, ctx), ctx) {
            Some((numid, ilvl)) => (numid.parse::<i64>().ok().map(|n| n.to_string()), Some(ilvl)),
            None => (None, None),
        }
    }

    /// `_has_visible_numbering_format`.
    fn has_visible_numbering(numid: &str, ilvl: i64, ctx: &Ctx) -> bool {
        ctx.num_levels
            .get(&(numid.to_string(), ilvl))
            .is_some_and(|l| l.visible)
    }

    /// `_get_checkbox_label`: `checkbox_selected` / `checkbox_unselected`
    /// for a paragraph holding a `w14:checkbox`.
    fn checkbox_label(p: XmlNode) -> Option<&'static str> {
        let cb = p.descendants().find(|n| n.has_tag_name("checkbox"))?;
        let checked = cb
            .descendants()
            .find(|n| n.has_tag_name("checked"))
            .is_some_and(|n| attr(n, "val") == Some("1"));
        Some(if checked {
            "checkbox_selected"
        } else {
            "checkbox_unselected"
        })
    }

    // ----- the walk -----------------------------------------------------------

    /// `_walk_linear` over a container's element children.
    fn walk_linear(&mut self, container: XmlNode, ctx: &Ctx) -> Vec<usize> {
        let mut added: Vec<usize> = Vec::new();
        for element in child_elements(container) {
            let tag = element.tag_name().name();
            let blips: Vec<XmlNode> = element
                .descendants()
                .filter(|n| n.has_tag_name("blip") && !in_textbox(*n))
                .collect();
            let drawings: Vec<XmlNode> = element
                .descendants()
                .filter(|n| n.has_tag_name("drawing") && !in_textbox(*n))
                .collect();
            let vml: Vec<XmlNode> = element
                .descendants()
                .filter(|n| n.has_tag_name("imagedata") && !in_textbox(*n))
                .collect();
            let key = self.key(element);
            // docling#4757: a table or content control walks its children
            // itself, so scanning it for textboxes here would emit a
            // textbox anchored in a cell before the table and again inside
            // the cell. Textboxes (and their paragraphs) already emitted are
            // left out, so a nested walk does not repeat them either.
            if !matches!(tag, "tbl" | "sdt") && !self.processed_textbox.contains(&key) {
                let raw = Self::textbox_elements(element);
                if raw.is_empty() {
                    self.shape_text(element, &mut added);
                }
                let textboxes: Vec<XmlNode> = raw
                    .into_iter()
                    .filter(|tb| !self.processed_textbox.contains(&self.key(*tb)))
                    .collect();
                if !textboxes.is_empty() {
                    self.processed_textbox.insert(key);
                    for tb in &textboxes {
                        let k = self.key(*tb);
                        self.processed_textbox.insert(k);
                        for p in tb.descendants().filter(|n| n.has_tag_name("p")) {
                            let k = self.key(p);
                            self.processed_textbox.insert(k);
                        }
                    }
                    added.extend(self.handle_textbox_content(&textboxes, ctx));
                }
            }
            let has_text = || {
                element
                    .descendants()
                    .any(|n| n.has_tag_name("t") && !is_math(n))
            };
            if tag == "tbl" {
                added.extend(self.handle_tables(element, ctx));
            } else if tag == "sdt" {
                if let Some(content) = element.children().find(|n| n.has_tag_name("sdtContent")) {
                    added.extend(self.walk_linear(content, ctx));
                }
            } else if !blips.is_empty() {
                added.extend(self.handle_pictures(&blips, "embed", ctx));
                if tag == "p" && has_text() {
                    added.extend(self.handle_text_elements(element, ctx, false));
                }
            } else if !vml.is_empty() {
                added.extend(self.handle_pictures(&vml, "id", ctx));
                if tag == "p" && has_text() {
                    added.extend(self.handle_text_elements(element, ctx, false));
                }
            } else if !drawings.is_empty() {
                // Native charts become classified pictures with their data;
                // any other DrawingML (shapes, SmartArt) is one rendered
                // picture upstream — a payload-less one here.
                let mut others = Vec::new();
                for d in &drawings {
                    if Self::is_chart_drawing(*d) {
                        if let Some(r) = self.handle_chart(*d, ctx) {
                            added.push(r);
                        }
                    } else {
                        others.push(*d);
                    }
                }
                if !others.is_empty() {
                    self.handle_drawingml(&others, ctx);
                }
                if tag == "p" && has_text() {
                    added.extend(self.handle_text_elements(element, ctx, true));
                }
            } else if tag == "p" {
                added.extend(self.handle_text_elements(element, ctx, false));
            }
        }
        added
    }

    /// The textbox elements of a body element: `.//w:txbxContent |
    /// .//v:textbox//w:p`, else the legacy `.//wps:txbx//w:p | .//w10:wrap//w:p
    /// | .//v:textbox//w:txbxContent//w:p`.
    fn textbox_elements<'a, 'i>(element: XmlNode<'a, 'i>) -> Vec<XmlNode<'a, 'i>> {
        let below = |n: XmlNode<'a, 'i>, names: &[&str]| {
            n.ancestors()
                .skip(1)
                .take_while(|a| a.id() != element.id())
                .any(|a| names.contains(&a.tag_name().name()))
        };
        let primary: Vec<XmlNode> = element
            .descendants()
            .skip(1)
            .filter(|n| {
                n.has_tag_name("txbxContent")
                    || (n.has_tag_name("p") && !is_math(*n) && below(*n, &["textbox"]))
            })
            .collect();
        if !primary.is_empty() {
            return primary;
        }
        element
            .descendants()
            .skip(1)
            .filter(|n| {
                n.has_tag_name("p") && !is_math(*n) && below(*n, &["txbx", "wrap", "textbox"])
            })
            .collect()
    }

    /// The shape-text branch of `_walk_linear`: DrawingML text not in any
    /// textbox becomes a `shape-text` section holding one text item.
    fn shape_text(&mut self, element: XmlNode, added: &mut Vec<usize>) {
        let is_a = |n: XmlNode, name: &str| n.tag_name().name() == name && is_dml(n);
        // `.//a:bodyPr/ancestor::*//a:t | .//a:txBody//a:t` — the first
        // alternative's `ancestor::*` reaches the document root, so any
        // `a:bodyPr` pulls in every `a:t` of the document.
        let mut texts: Vec<XmlNode> = Vec::new();
        if element.descendants().any(|n| is_a(n, "bodyPr")) {
            texts.extend(
                element
                    .document()
                    .root()
                    .descendants()
                    .filter(|n| is_a(*n, "t")),
            );
        }
        for tx in element.descendants().skip(1).filter(|n| is_a(*n, "txBody")) {
            texts.extend(tx.descendants().skip(1).filter(|n| is_a(*n, "t")));
        }
        // An XPath union: node-set semantics, document order.
        texts.sort_by_key(|n| n.id().get());
        texts.dedup_by_key(|n| n.id().get());
        // docling#4757: text already emitted from another element is not
        // emitted again; what is emitted now is marked.
        texts.retain(|t| !self.processed_textbox.contains(&self.key(*t)));
        if texts.is_empty() {
            return;
        }
        let key = self.key(element);
        self.processed_textbox.insert(key);
        for t in &texts {
            let k = self.key(*t);
            self.processed_textbox.insert(k);
        }
        let content = texts
            .iter()
            .filter_map(|t| t.text())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if content.trim().is_empty() {
            return;
        }
        let level = self.get_level();
        let group = self.add(
            self.parent_at(level - 1),
            self.layer,
            group_kind("section", "shape-text"),
        );
        added.push(group);
        self.add(
            Some(group),
            self.layer,
            text_kind("text", &content, None, None),
        );
    }

    /// `_handle_textbox_content` with `_collect_textbox_paragraphs`.
    fn handle_textbox_content(&mut self, textboxes: &[XmlNode], ctx: &Ctx) -> Vec<usize> {
        let mut refs = Vec::new();
        let level = self.get_level();
        let group = self.add(
            self.parent_at(level - 1),
            self.layer,
            group_kind("section", "textbox"),
        );
        refs.push(group);
        let original = self.parent_at(level);
        self.set_parent(level, Some(group));

        // Paragraphs per container, containers in first-seen order; a
        // paragraph's position is its index among its parent's paragraphs.
        let position = |p: XmlNode| -> usize {
            p.parent()
                .map(|par| {
                    par.children()
                        .filter(|c| c.has_tag_name("p") && !is_math(*c))
                        .position(|c| c.id() == p.id())
                        .unwrap_or(0)
                })
                .unwrap_or(0)
        };
        let mut processed: HashSet<NodeId> = HashSet::new();
        // `(container, paragraph, position)` in collection order.
        let mut collected: Vec<(Option<NodeId>, XmlNode, usize)> = Vec::new();
        for &el in textboxes {
            if !processed.insert(el.id()) {
                continue;
            }
            if el.has_tag_name("p") {
                let container = el.ancestors().skip(1).find(|a| {
                    let n = a.tag_name().name();
                    n.contains("textbox") || n.contains("shape") || n.contains("txbx")
                });
                collected.push((container.map(|c| c.id()), el, position(el)));
            } else {
                let ps: Vec<XmlNode> = el
                    .descendants()
                    .skip(1)
                    .filter(|n| n.has_tag_name("p") && !is_math(*n))
                    .collect();
                for p in ps {
                    if processed.insert(p.id()) {
                        collected.push((Some(el.id()), p, position(p)));
                    }
                }
            }
        }
        // Containers in first-seen order, each sorted by position (stable).
        let mut order: Vec<Option<NodeId>> = Vec::new();
        for (c, _, _) in &collected {
            if !order.contains(c) {
                order.push(*c);
            }
        }
        let mut all: Vec<(XmlNode, usize)> = Vec::new();
        for c in order {
            let mut ps: Vec<(XmlNode, usize)> = collected
                .iter()
                .filter(|(cc, _, _)| *cc == c)
                .map(|(_, p, pos)| (*p, *pos))
                .collect();
            ps.sort_by_key(|(_, pos)| *pos);
            all.extend(ps);
        }

        // No text-keyed dedup (docling's `_handle_textbox_content` has one):
        // it existed for the `mc:AlternateContent` duplicate of a text box,
        // which is settled before parsing (`mc.rs`, #572), and it dropped a
        // paragraph legitimately repeated in a box along with it.
        for (p, _pos) in all {
            refs.extend(self.handle_text_elements(p, ctx, false));
            let blips: Vec<XmlNode> = p.descendants().filter(|n| n.has_tag_name("blip")).collect();
            let vml: Vec<XmlNode> = p
                .descendants()
                .filter(|n| n.has_tag_name("imagedata"))
                .collect();
            let drawings: Vec<XmlNode> = p
                .descendants()
                .filter(|n| n.has_tag_name("drawing"))
                .collect();
            if !blips.is_empty() {
                refs.extend(self.handle_pictures(&blips, "embed", ctx));
            } else if !vml.is_empty() {
                refs.extend(self.handle_pictures(&vml, "id", ctx));
            } else if !drawings.is_empty() {
                self.handle_drawingml(&drawings, ctx);
            }
        }
        self.set_parent(level, original);
        refs
    }

    // ----- pictures -----------------------------------------------------------

    /// `_handle_pictures` / `_handle_vml_pictures`: one picture per image
    /// reference, grouped in a `picture_area` when there are several.
    fn handle_pictures(&mut self, images: &[XmlNode], rel_attr: &str, ctx: &Ctx) -> Vec<usize> {
        let mut refs = Vec::new();
        if images.is_empty() {
            return refs;
        }
        let level = self.get_level();
        let parent = if images.len() == 1 {
            self.parent_at(level - 1)
        } else {
            Some(self.add(
                self.parent_at(level - 1),
                self.layer,
                group_kind("picture_area", "group"),
            ))
        };
        for image in images {
            let img = super::docx::with_extent(
                attr(*image, rel_attr)
                    .and_then(|id| ctx.images.get(id))
                    .cloned(),
                *image,
            );
            let spacer = img.as_ref().is_some_and(is_invisible_spacer);
            let layer = if spacer {
                Some(ContentLayer::Invisible)
            } else {
                self.layer
            };
            refs.push(self.add(
                parent,
                layer,
                TreeKind::Picture {
                    captions: Vec::new(),
                    image: img,
                    extra_classes: Vec::new(),
                    classification: None,
                    description: super::docx::drawing_alt(*image).map(PictureDescription::alt_text),
                    confidence: None,
                    chart: None,
                    dpi: None,
                },
            ));
        }
        refs
    }

    /// `_handle_drawingml`: upstream renders the paragraph's blip-less shapes
    /// through LibreOffice into one picture; here they are drawn in-process
    /// into one SVG payload (#858, `docx_drawingml`), the picture staying
    /// payload-less when nothing in them can be drawn.
    fn handle_drawingml(&mut self, drawings: &[XmlNode], ctx: &Ctx) {
        let level = self.get_level();
        let parent = self.parent_at(level - 1);
        let rendered = super::docx_drawingml::render(drawings, ctx.theme);
        let diagram = rendered
            .as_ref()
            .and_then(|r| super::docx_drawingml::diagram(r, ctx.extract_diagrams));
        let image = rendered.map(|r| r.image);
        self.add(
            parent,
            self.layer,
            TreeKind::Picture {
                captions: Vec::new(),
                image,
                extra_classes: Vec::new(),
                classification: None,
                description: None,
                confidence: None,
                chart: None,
                dpi: None,
            },
        );
        // The diagram extracted from the drawing (#858), a code item after
        // its picture, as in the flat chain.
        if let Some((language, text)) = diagram {
            self.add(
                parent,
                self.layer,
                TreeKind::Code {
                    text,
                    orig: None,
                    language: Some(language),
                    formatting: None,
                    hyperlink: None,
                    captions: Vec::new(),
                },
            );
        }
    }

    /// `_is_chart_drawing`: the drawing embeds a `c:chart` part reference.
    fn is_chart_drawing(d: XmlNode) -> bool {
        d.descendants()
            .any(|n| n.has_tag_name("chart") && attr(n, "id").is_some())
    }

    /// `_handle_chart`: a classified picture carrying the chart's data, its
    /// title a body-level caption item created first.
    fn handle_chart(&mut self, d: XmlNode, ctx: &Ctx) -> Option<usize> {
        let level = self.get_level();
        let parent = self.parent_at(level - 1);
        let spec = d
            .descendants()
            .find(|n| n.has_tag_name("chart") && attr(*n, "id").is_some())
            .and_then(|c| attr(c, "id"))
            .and_then(|id| ctx.charts.get(id));
        let caption = spec
            .and_then(|(_, title, _)| title.clone())
            .filter(|t| !t.is_empty())
            .map(|t| self.add(None, self.layer, text_kind("caption", &t, None, None)));
        Some(self.add(
            parent,
            self.layer,
            TreeKind::Picture {
                captions: caption.into_iter().collect(),
                image: None,
                extra_classes: Vec::new(),
                classification: spec.map(|(kind, _, _)| kind.clone()),
                description: None,
                confidence: None,
                chart: spec.map(|(_, _, table)| table.clone()),
                dpi: None,
            },
        ))
    }

    // ----- tables -------------------------------------------------------------

    /// `_is_rich_table_cell`.
    fn is_rich_table_cell(&self, tc: XmlNode, ctx: &Ctx) -> bool {
        let paras: Vec<XmlNode> = tc.children().filter(|n| n.has_tag_name("p")).collect();
        if paras.len() > 1 {
            return true;
        }
        if child_elements(tc).any(|c| !matches!(c.tag_name().name(), "p" | "tcPr")) {
            return true;
        }
        // docling#4757: a cell holding a textbox (or shape text) is rich, so
        // the textbox is walked inside the cell.
        if has_blip(tc) || has_textbox(tc) {
            return true;
        }
        // A `w:hyperlink` alone does not make the cell rich (#833 asked for
        // it): python-docx's `paragraph.runs` never lists a hyperlink's
        // runs, so docling reads such a cell as plain text and its HTML of
        // `docx_rich_tables_01` — a reference fixture here — has the link's
        // text alone; the cell stays plain for that parity, and the link is
        // lost with it, as in docling.
        for p in &paras {
            for r in child_elements(*p).filter(|c| c.has_tag_name("r") && !is_math(*c)) {
                if self.format_from_run(r, None, None, ctx) != Formatting::default() {
                    return true;
                }
            }
        }
        if let Some(first) = paras.first() {
            if !py_paragraph_text(*first).trim().is_empty()
                && is_code_style(super::docx::paragraph_style_id(*first, ctx), ctx)
            {
                return true;
            }
        }
        false
    }

    /// `_handle_tables`: a table item, its cells walked once over the rows
    /// with the grid column tracked explicitly; a rich cell's items are
    /// re-parented under a group of the table.
    fn handle_tables(&mut self, tbl: XmlNode, ctx: &Ctx) -> Vec<usize> {
        if ctx.table_depth.get() >= MAX_TABLE_DEPTH {
            return Vec::new();
        }
        ctx.table_depth.set(ctx.table_depth.get() + 1);
        let out = self.handle_tables_inner(tbl, ctx);
        ctx.table_depth.set(ctx.table_depth.get() - 1);
        out
    }

    fn handle_tables_inner(&mut self, tbl: XmlNode, ctx: &Ctx) -> Vec<usize> {
        let rows: Vec<XmlNode> = tbl
            .children()
            .filter(|n| n.has_tag_name("tr") && row_kept(*n, ctx))
            .collect();
        let num_rows = rows.len();
        // python-docx's `len(table.columns)` is the `w:tblGrid` column count,
        // and upstream used to drop a table without a grid (it failed to
        // load) and cut every cell past the declared grid (docling#5006).
        // Since docling#5179 (2026-11-05) its `_handle_tables` takes the
        // same width as here: the grid is at least as wide as the widest
        // row — `w:gridBefore` + the cells' `w:gridSpan`s + `w:gridAfter`,
        // the flat walk's width — so nothing the document holds is lost,
        // and a grid that covers every cell is the declared one.
        let grid = tbl.children().find(|n| n.has_tag_name("tblGrid"));
        let grid_cols = grid.map_or(0, |g| {
            g.children().filter(|n| n.has_tag_name("gridCol")).count()
        });
        let row_cols = rows
            .iter()
            .map(|r| {
                let (before, after) = row_grid_offsets(*r);
                before + after + row_cells(*r).into_iter().map(grid_span).sum::<usize>()
            })
            .max()
            .unwrap_or(0);
        if grid.is_none() && row_cols == 0 {
            // No grid and no cell: nothing to keep, dropped as upstream does.
            return Vec::new();
        }
        let num_cols = grid_cols.max(row_cols);
        if num_rows == 1 && num_cols == 1 {
            // A 1×1 table is furniture: its cell is walked as body content.
            if let Some(cell) = row_cells(rows[0]).first() {
                self.clear_list_cache();
                self.force_new_code_block = true;
                self.walk_linear(*cell, ctx);
                self.force_new_code_block = true;
            }
            return Vec::new();
        }
        let level = self.get_level();
        let table_id = self.add(
            self.parent_at(level - 1),
            self.layer,
            TreeKind::Table {
                table: Table::default(),
                rich_cells: Vec::new(),
                captions: Vec::new(),
            },
        );
        let mut cells: Vec<TableCell> = Vec::new();
        let mut rich: Vec<(usize, usize, usize)> = Vec::new();
        let mut open_cells: HashMap<usize, usize> = HashMap::new();
        for (row_idx, row) in rows.iter().enumerate() {
            let mut grid_col = row_grid_offsets(*row).0;
            for tc in row_cells(*row) {
                if grid_col >= num_cols {
                    break;
                }
                let col_span = tc_pr(tc, "gridSpan")
                    .and_then(|n| attr(n, "val"))
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(1);
                let v_merge = tc_pr(tc, "vMerge").map(|n| attr(n, "val").unwrap_or("continue"));
                if v_merge == Some("continue") {
                    if let Some(&ci) = open_cells.get(&grid_col) {
                        let spanned = &mut cells[ci];
                        spanned.row_span = row_idx + 1 - spanned.start_row;
                        grid_col += col_span;
                        continue;
                    }
                }
                let cell_text = py_cell_text(tc, ctx.policy());
                let (with_eq, equations) = equations_in_text(tc, &cell_text);
                let text = if equations.is_empty() {
                    cell_text
                } else {
                    with_eq.replace("<eq>", "$").replace("</eq>", "$")
                };
                let rich_cell = self.is_rich_table_cell(tc, ctx);
                let mut provs: Vec<usize> = Vec::new();
                if rich_cell {
                    let saved = self.save_list_ctx();
                    provs = self.walk_linear(tc, ctx);
                    self.restore_list_ctx(saved);
                }
                let mut group = None;
                if !provs.is_empty() {
                    let name = format!(
                        "rich_cell_group_{}_{}_{}",
                        self.tree.table_count(),
                        grid_col,
                        row_idx
                    );
                    let g = self.add(Some(table_id), self.layer, group_kind("unspecified", &name));
                    for &p in &provs {
                        self.tree.reparent(p, Some(g));
                    }
                    group = Some(g);
                }
                cells.push(TableCell {
                    text,
                    bbox: None,
                    start_row: row_idx,
                    start_col: grid_col,
                    row_span: 1,
                    col_span,
                    column_header: row_idx == 0,
                    row_header: false,
                    row_section: false,
                });
                if let (true, Some(g)) = (rich_cell, group) {
                    rich.push((row_idx, grid_col, g));
                }
                open_cells.insert(grid_col, cells.len() - 1);
                grid_col += col_span;
            }
        }
        let mut grid_rows = vec![vec![String::new(); num_cols]; num_rows];
        for c in &cells {
            for row in grid_rows
                .iter_mut()
                .take(c.start_row + c.row_span)
                .skip(c.start_row)
            {
                for slot in row
                    .iter_mut()
                    .take(c.start_col + c.col_span)
                    .skip(c.start_col)
                {
                    *slot = c.text.clone();
                }
            }
        }
        if let TreeKind::Table {
            table, rich_cells, ..
        } = &mut self.tree.items[table_id].kind
        {
            *table = Table {
                rows: grid_rows,
                cells: Some(cells),
                ..Table::default()
            };
            *rich_cells = rich;
        }
        vec![table_id]
    }

    // ----- paragraphs -----------------------------------------------------------

    /// `_handle_text_elements`.
    fn handle_text_elements(&mut self, p: XmlNode, ctx: &Ctx, skip_empty_text: bool) -> Vec<usize> {
        // docling#4958: a fully hidden paragraph (marked by `docx_hidden`)
        // goes to the `invisible` layer — kept, like a hidden slide, but out
        // of the default exports. List groups stay on the layer around it,
        // so a hidden first item does not hide the shown items after it.
        if super::docx_hidden::is_hidden_paragraph(p) && self.layer != Some(ContentLayer::Invisible)
        {
            let around = self.layer;
            self.layer_around_hidden = Some(around);
            self.layer = Some(ContentLayer::Invisible);
            let refs = self.handle_text_elements(p, ctx, skip_empty_text);
            self.layer = around;
            self.layer_around_hidden = None;
            return refs;
        }
        let calls = note_references(p);
        let first_new = self.tree.items.len();
        let refs = self.handle_text_elements_inner(p, ctx, skip_empty_text);
        // The bookmarks starting in this paragraph land on its first text
        // item (#849): the heading, the list item, the first run of a
        // formatted paragraph.
        if !ctx.bookmarks.is_empty() {
            let anchors = paragraph_anchors(p, ctx);
            if !anchors.is_empty() {
                if let Some(&item) = refs
                    .iter()
                    .find(|&&i| matches!(self.tree.items[i].kind, TreeKind::Text { .. }))
                {
                    self.tree.items[item].anchors = anchors;
                }
            }
        }
        if !calls.is_empty() {
            let full_text: String = self
                .iter_paragraph_content(p, ctx)
                .iter()
                .map(|c| c.text.as_str())
                .collect();
            self.anchor_note_calls(&full_text, first_new, calls);
        }
        refs
    }

    /// #538: place each note reference of a paragraph in the text item it
    /// falls in ([`ItemTree::place_note_calls`]).
    fn anchor_note_calls(
        &mut self,
        full_text: &str,
        first_new: usize,
        calls: Vec<(usize, &'static str, String)>,
    ) {
        let offsets: Vec<usize> = calls.iter().map(|c| c.0).collect();
        let placed = self.tree.place_note_calls(first_new, full_text, &offsets);
        for ((_, kind, id), place) in calls.into_iter().zip(placed) {
            if let Some((item, offset)) = place {
                self.note_calls.push(NoteCall {
                    item,
                    offset,
                    kind,
                    id,
                });
            }
        }
    }

    fn handle_text_elements_inner(
        &mut self,
        p: XmlNode,
        ctx: &Ctx,
        skip_empty_text: bool,
    ) -> Vec<usize> {
        let mut refs: Vec<usize> = Vec::new();
        let content = self.iter_paragraph_content(p, ctx);
        let mut elements = self.paragraph_elements(&content);
        let full_text: String = content.iter().map(|c| c.text.as_str()).collect();
        let (text, equations) = equations_in_text(p, &full_text);
        // Kept unstripped: code blocks preserve leading indentation.
        let raw_paragraph_text = text.clone();
        let text = text.trim().to_string();
        let key = self.key(p);
        let comment_ids = ids_of(
            p,
            &["commentRangeStart", "commentRangeEnd", "commentReference"],
        );
        let checkbox_label = Self::checkbox_label(p);

        let level0 = self.get_level();
        self.prev_sibling_is_code = self.is_code(self.last_child(self.parent_at(level0 - 1)));
        let (p_style_id, p_level) = self.label_and_level(p, ctx);
        let (mut numid, mut ilevel) = self.num_id_and_ilvl(p, ctx);
        if numid.as_deref() == Some("0") {
            numid = None;
        }
        // `_apply_caption_numbering` (#765): a heading-relative caption
        // number goes into the first non-blank element's text, and the
        // paragraph is no list item.
        if let (Some(nid), Some(ilvl)) = (numid.as_deref(), ilevel) {
            let marker = caption_numbering_marker(
                &mut self.list_counters,
                ctx.num_levels,
                nid,
                ilvl,
                &p_style_id,
            );
            if let Some(marker) = marker {
                if let Some(el) = elements.iter_mut().find(|e| !e.text.trim().is_empty()) {
                    el.text = format!("{marker} {}", el.text);
                }
                numid = None;
                ilevel = None;
            }
        }

        // Lists.
        if let (Some(nid), Some(ilvl)) = (numid.clone(), ilevel) {
            if !matches!(p_style_id.as_str(), "Title" | "Heading" | "Code") {
                let is_numbered = Self::has_visible_numbering(&nid, ilvl, ctx);
                let left_indent = super::docx::list_left_indent(p, &nid, ilvl, ctx);
                let li = if !equations.is_empty() {
                    self.add_list_item_with_equations(
                        &nid,
                        ilvl,
                        left_indent,
                        &text,
                        &equations,
                        is_numbered,
                        ctx,
                    )
                } else {
                    self.add_list_item(&nid, ilvl, left_indent, &elements, is_numbered, ctx)
                };
                refs.extend(li);
                self.update_history(numid, ilevel);
                self.record_paragraph(key, &refs, comment_ids);
                return refs;
            }
        }
        if self.prev_numid().is_some()
            && !matches!(p_style_id.as_str(), "Title" | "Heading")
            && (numid.is_none() || p_style_id == "Code")
        {
            // Close the list. A Code paragraph after a list must close it
            // even with a stray numId, then be re-parented at body level.
            // Lists nested by left indentation close with the outermost
            // list, whose group is the one cached below (docling#4465).
            self.last_numid = match self.nested_lists.first() {
                Some(n) if self.level_at_new_list.is_some() => Some(n.outer_numid.clone()),
                _ => self.prev_numid(),
            };
            self.nested_lists.clear();
            if !text.trim().is_empty() {
                self.clear_list_cache();
            } else if let Some(lanl) = self.level_at_new_list.filter(|&l| l != 0) {
                // A blank spacer paragraph keeps the group cached for reuse.
                let parent_item = self.parent_at(lanl);
                if self.is_list_group(parent_item) {
                    if let (Some(g), Some(n)) = (parent_item, self.last_numid.clone()) {
                        self.cache = Some(ListCache {
                            group: g,
                            numid: n,
                            parent: self.parent_at(lanl - 1),
                        });
                    }
                }
            }
            // Python truthiness: a `level_at_new_list` of 0 takes the
            // reset-everything branch.
            if let Some(lanl) = self.level_at_new_list.filter(|&l| l != 0) {
                self.clear_parents_from(lanl);
                self.level = lanl - 1;
                self.level_at_new_list = None;
            } else {
                self.clear_parents_from(0);
                self.level = 0;
            }
        }

        if p_style_id == "Title" {
            self.clear_parents_from(0);
            // The parents stack is cleared here, so the list context goes
            // with it (docling#4282) — the next list item opens fresh.
            self.level_at_new_list = None;
            let te = self.add(None, self.layer, text_kind("title", &text, None, None));
            self.set_parent(0, Some(te));
            refs.push(te);
        } else if p_style_id.contains("Heading") {
            // `_add_heading` clears the parents tail; a heading whose style
            // carries `w:numPr` used to leave `level_at_new_list` pointing at
            // a slot that no longer exists, and the next list item hung its
            // group off the body root (docling#4282).
            self.level_at_new_list = None;
            let is_numbered_style = numid
                .as_deref()
                .is_some_and(|n| Self::has_visible_numbering(n, ilevel.unwrap_or(0), ctx));
            refs.extend(self.add_heading(
                p_level,
                &text,
                is_numbered_style,
                numid.as_deref(),
                ilevel,
                ctx,
            ));
        } else if !equations.is_empty() {
            if py_paragraph_text(p).trim().is_empty() && !text.is_empty() {
                // Standalone equation(s) — one formula item each.
                let level = self.get_level();
                let parent = self.parent_at(level - 1);
                if equations.len() > 1 {
                    for eq in &equations {
                        let eq_text = eq.replace("<eq>", "").replace("</eq>", "");
                        let eq_text = eq_text.trim();
                        if !eq_text.is_empty() {
                            refs.push(self.add(
                                parent,
                                self.layer,
                                text_kind("formula", eq_text, None, None),
                            ));
                        }
                    }
                } else {
                    let t = text.replace("<eq>", "").replace("</eq>", "");
                    refs.push(self.add(parent, self.layer, text_kind("formula", &t, None, None)));
                }
            } else {
                // Inline equation(s): an inline group of text and formula items.
                let level = self.get_level();
                let inline = self.add(
                    self.parent_at(level - 1),
                    self.layer,
                    group_kind("inline", "group"),
                );
                refs.push(inline);
                let mut created = Vec::new();
                self.add_inline_equations_to_parent(inline, &text, &equations, &mut created);
                refs.extend(created);
            }
        } else if p_style_id == "Code" && checkbox_label.is_none() {
            let level = self.get_level();
            let parent = self.parent_at(level - 1);
            let code_text = raw_paragraph_text.trim_end().to_string();
            // Merge into the previous code block only when it is the parent's
            // last child, the most recent text item, and on the same layer.
            let last_item = self.last_child(parent);
            let merge_target = if self.force_new_code_block {
                None
            } else {
                last_item
            };
            let mergeable = merge_target.is_some_and(|mt| {
                self.is_code(Some(mt))
                    && self.tree.items[mt].layer == self.layer
                    && self.tree.last_text() == Some(mt)
            });
            if let (true, Some(mt)) = (mergeable, merge_target) {
                if !code_text.is_empty() {
                    let joiner = "\n".repeat(self.pending_code_blank_lines + 1);
                    if let TreeKind::Code { text, language, .. } = &mut self.tree.items[mt].kind {
                        text.push_str(&joiner);
                        text.push_str(&code_text);
                        if language.is_none() {
                            *language = detect_code_language(text);
                        }
                    }
                    self.pending_code_blank_lines = 0;
                } else {
                    // Buffered: written only if more code follows.
                    self.pending_code_blank_lines += 1;
                }
                refs.push(mt);
                self.force_new_code_block = false;
            } else if !text.is_empty() {
                self.pending_code_blank_lines = 0;
                let language = detect_code_language(&code_text);
                let code = self.add(
                    parent,
                    self.layer,
                    TreeKind::Code {
                        text: code_text,
                        orig: None,
                        language,
                        formatting: None,
                        hyperlink: None,
                        captions: Vec::new(),
                    },
                );
                refs.push(code);
                self.force_new_code_block = false;
            }
        } else {
            self.end_list_on_body_text(&text);
            let level = self.get_level();
            let parent = if elements.len() > 1 {
                Some(self.add(
                    self.parent_at(level - 1),
                    self.layer,
                    group_kind("inline", "group"),
                ))
            } else {
                self.parent_at(level - 1)
            };
            for el in &elements {
                let clean = if checkbox_label.is_some() {
                    clean_checkbox_symbols(&el.text)
                } else {
                    el.text.clone()
                };
                if skip_empty_text && clean.trim().is_empty() {
                    continue;
                }
                let label = checkbox_label.unwrap_or("text");
                let item = self.add(
                    parent,
                    self.layer,
                    text_kind(label, &clean, el.fmt, el.link.clone()),
                );
                self.tree.items[item].revision = el.revision.clone();
                refs.push(item);
            }
        }
        self.update_history(numid, ilevel);
        self.record_paragraph(key, &refs, comment_ids);
        refs
    }

    /// The tail of `_handle_text_elements`: remember the paragraph's items
    /// and comment ids for `_add_comments`.
    fn record_paragraph(&mut self, key: Key, refs: &[usize], comment_ids: Vec<String>) {
        if refs.is_empty() {
            return;
        }
        self.para_items.insert(key, refs.to_vec());
        if !comment_ids.is_empty() {
            self.set_para_comments(key, comment_ids);
        }
    }

    /// `_add_heading`.
    fn add_heading(
        &mut self,
        curr_level: Option<i64>,
        text: &str,
        is_numbered: bool,
        numid: Option<&str>,
        ilevel: Option<i64>,
        ctx: &Ctx,
    ) -> Vec<usize> {
        let mut refs = Vec::new();
        let level = self.get_level();
        let (current_level, parent_level, add_level) = match curr_level {
            Some(cl) => {
                if cl > level {
                    // Invisible section groups fill the skipped levels.
                    for i in level..cl {
                        let g = self.add(
                            self.parent_at(i - 1),
                            None,
                            group_kind("section", &format!("header-{i}")),
                        );
                        refs.push(g);
                        self.set_parent(i, Some(g));
                    }
                } else if cl < level {
                    self.clear_parents_from(cl);
                }
                let cl = cl.max(1);
                (cl, cl - 1, cl)
            }
            None => (self.level, self.level - 1, 1),
        };
        let authored_len = text.len();
        let text = if is_numbered {
            let text = numbered_heading_text(&mut self.numbered_headers, add_level as u8, text);
            if let Some(numid) = numid {
                sync_heading_counter(
                    &mut self.list_counters,
                    ctx.num_levels,
                    self.numbered_headers[&(add_level as u8)] as i64,
                    numid,
                    ilevel.unwrap_or(0),
                );
                // The seeded counter must survive the first list item of
                // this `numId`, which would otherwise restart it.
                self.started_numids.insert(numid.to_string());
            }
            text
        } else {
            text.to_string()
        };
        // The generated prefix, kept apart from the authored text (#850):
        // `numbered_heading_text` writes `<number> <text>`.
        let number = (is_numbered && text.len() > authored_len + 1)
            .then(|| text[..text.len() - authored_len - 1].to_string());
        // On the paragraph's layer (docling#4958 passes it: a hidden heading
        // is invisible, a header part's heading furniture).
        let hd = self.add(
            self.parent_at(parent_level),
            self.layer,
            TreeKind::Text {
                label: "section_header".into(),
                text,
                orig: None,
                formatting: None,
                hyperlink: None,
                level: Some(add_level as u8),
                list: None,
            },
        );
        self.tree.items[hd].number = number;
        self.set_parent(current_level, Some(hd));
        refs.push(hd);
        refs
    }

    /// `_add_inline_equations_to_parent`: alternating text and formula items.
    fn add_inline_equations_to_parent(
        &mut self,
        parent: usize,
        text: &str,
        equations: &[String],
        refs: &mut Vec<usize>,
    ) {
        let mut rest = text.to_string();
        for eq in equations {
            if rest.is_empty() {
                break;
            }
            let (pre, post) = match rest.split_once(eq.trim()) {
                Some((a, b)) => (a.to_string(), b.to_string()),
                None => (rest.clone(), String::new()),
            };
            rest = post;
            if !pre.is_empty() {
                refs.push(self.add(
                    Some(parent),
                    self.layer,
                    text_kind("text", &pre, None, None),
                ));
            }
            let f = eq.replace("<eq>", "").replace("</eq>", "");
            refs.push(self.add(
                Some(parent),
                self.layer,
                text_kind("formula", &f, None, None),
            ));
        }
        if !rest.is_empty() {
            refs.push(self.add(
                Some(parent),
                self.layer,
                text_kind("text", rest.trim(), None, None),
            ));
        }
    }

    /// `_manage_list_structure`: open / indent / close / continue a list by
    /// `numId` and `ilvl`, returning the created groups and the level the
    /// item goes at.
    ///
    /// `left_indent` is the item's effective left indentation in twips
    /// ([`super::docx::list_left_indent`], docling#4465): an item of another
    /// `numId` indented deeper than the open list's item opens its list
    /// under that item, and a later one indented no deeper than that item
    /// closes the nested list again and is placed as if it came right after
    /// that item. Lists at one indentation stay siblings.
    fn manage_list_structure(
        &mut self,
        numid: &str,
        ilevel: i64,
        left_indent: i64,
    ) -> (Vec<usize>, i64) {
        let mut refs = Vec::new();
        let mut level = self.get_level();
        let mut prev_indent = self.prev_indent();
        let mut prev_numid = self.prev_numid();
        if let Some(prev) = prev_numid.clone().filter(|p| p != numid) {
            if self.level_at_new_list.is_some() && self.is_list_group(self.parent_at(level - 1)) {
                let mut closed = None;
                while self
                    .nested_lists
                    .last()
                    .is_some_and(|n| left_indent <= n.outer_left_indent)
                {
                    closed = self.nested_lists.pop();
                }
                if let Some(closed) = closed {
                    // Back out to the item it hangs from.
                    self.clear_parents_from(closed.slot);
                    level = self.get_level();
                    prev_numid = Some(closed.outer_numid);
                    prev_indent = closed.outer_ilevel;
                } else if left_indent > self.last_list_left_indent {
                    // Nest under the open item.
                    self.nested_lists.push(NestedList {
                        slot: level,
                        start_ilevel: ilevel,
                        outer_numid: prev,
                        outer_ilevel: prev_indent,
                        outer_left_indent: self.last_list_left_indent,
                    });
                }
            }
        }
        let same = prev_numid.as_deref() == Some(numid);
        let use_level;
        if prev_numid.is_none() || (same && self.level_at_new_list.is_none()) {
            // Open a new list.
            self.level_at_new_list = Some(level);
            self.level_start_ilevel = ilevel;
            self.nested_lists.clear();
            self.start_numid(numid);
            let parent = self.sub_list_parent(self.parent_at(level - 1));
            let g = self.get_or_create_list_group(numid, parent, &mut refs);
            self.set_parent(level, Some(g));
            use_level = level;
            self.last_numid = Some(numid.to_string());
        } else if same
            && self.level_at_new_list.is_some()
            && prev_indent.is_some_and(|pi| pi < ilevel)
        {
            // Open an indented list.
            let pi = prev_indent.unwrap_or(0);
            for i in (self.slot_for(pi) + 1)..(self.slot_for(ilevel) + 1) {
                let g = self.add(
                    self.sub_list_parent(self.parent_at(i - 1)),
                    self.layer_around_hidden.unwrap_or(self.layer),
                    group_kind("list", "list"),
                );
                self.set_parent(i, Some(g));
                refs.push(g);
            }
            use_level = self.slot_for(ilevel);
        } else if same
            && self.level_at_new_list.is_some()
            && prev_indent.is_some_and(|pi| ilevel < pi)
        {
            // Close list levels.
            self.clear_parents_from(self.slot_for(ilevel) + 1);
            use_level = self.slot_for(ilevel);
        } else if same && self.is_list_group(self.parent_at(level - 1)) {
            // Continue the existing list.
            use_level = level - 1;
        } else if !same || !self.is_list_group(self.parent_at(level - 1)) {
            // New list sequence.
            match self.level_at_new_list {
                Some(_) => {
                    use_level = self.slot_for(ilevel);
                    self.clear_parents_from(use_level + 1);
                }
                None => {
                    use_level = level;
                    self.level_at_new_list = Some(use_level);
                    self.level_start_ilevel = ilevel;
                    self.nested_lists.clear();
                }
            }
            self.start_numid(numid);
            let parent = self.sub_list_parent(self.parent_at(use_level - 1));
            let g = self.get_or_create_list_group(numid, parent, &mut refs);
            self.set_parent(use_level, Some(g));
            self.last_numid = Some(numid.to_string());
        } else {
            use_level = level - 1;
        }
        self.last_list_left_indent = left_indent;
        (refs, use_level)
    }

    /// `_slot_for` (docling#4188): the parents slot for a Word `w:ilvl`. A
    /// list that starts at level 0 maps to `level_at_new_list + ilvl`; one
    /// that starts higher subtracts its start level so its first item lands
    /// at `level_at_new_list`, and an item shallower than the start level (a
    /// resumed list returning to level 0) clamps to the list base. Inside a
    /// list nested by left indentation (docling#4465) the levels map from
    /// that list's own slot and start level.
    fn slot_for(&self, word_ilevel: i64) -> i64 {
        if let Some(n) = self.nested_lists.last() {
            return n.slot + (word_ilevel - n.start_ilevel).max(0);
        }
        self.level_at_new_list.unwrap_or(0) + (word_ilevel - self.level_start_ilevel).max(0)
    }

    /// Counters reset only the first time a `numId` is opened: a `numId`
    /// that reappears after an intervening list resumes its numbering.
    fn start_numid(&mut self, numid: &str) {
        if !self.started_numids.contains(numid) {
            for (k, v) in self.list_counters.iter_mut() {
                if k.0 == numid {
                    *v = 0;
                }
            }
            self.started_numids.insert(numid.to_string());
        }
    }

    /// `_add_list_item_with_marker`'s marker: the counter advanced and the
    /// marker built for a numbered level, empty for a bullet.
    fn list_marker(&mut self, numid: &str, ilevel: i64, is_numbered: bool, ctx: &Ctx) -> String {
        if is_numbered {
            get_list_counter(&mut self.list_counters, ctx.num_levels, numid, ilevel);
            build_enum_marker(&self.list_counters, ctx.num_levels, numid, ilevel)
        } else {
            String::new()
        }
    }

    /// `_add_list_item`.
    fn add_list_item(
        &mut self,
        numid: &str,
        ilevel: i64,
        left_indent: i64,
        elements: &[Part],
        is_numbered: bool,
        ctx: &Ctx,
    ) -> Vec<usize> {
        if elements.is_empty() {
            return Vec::new();
        }
        let (refs, use_level) = self.manage_list_structure(numid, ilevel, left_indent);
        let marker = self.list_marker(numid, ilevel, is_numbered, ctx);
        self.add_formatted_list_item(elements, marker, is_numbered, use_level);
        refs
    }

    /// `_add_formatted_list_item`: one element → the list item carries the
    /// text, formatting and hyperlink; several → an empty list item over an
    /// inline group of text items — on the paragraph's layer, as docling
    /// passes it since docling#4958 (a hidden item is invisible, a header
    /// part's item furniture).
    fn add_formatted_list_item(
        &mut self,
        elements: &[Part],
        marker: String,
        enumerated: bool,
        level: i64,
    ) {
        let parent = self.parent_at(level);
        if !self.is_list_group(parent) || elements.is_empty() {
            return;
        }
        let list = Some(ListMeta { enumerated, marker });
        if elements.len() == 1 {
            let e = &elements[0];
            if !e.text.is_empty() {
                let item = self.add(
                    parent,
                    self.layer,
                    TreeKind::Text {
                        label: "list_item".into(),
                        text: e.text.clone(),
                        orig: None,
                        formatting: e.fmt,
                        hyperlink: e.link.clone(),
                        level: None,
                        list,
                    },
                );
                self.tree.items[item].revision = e.revision.clone();
            }
        } else {
            let item = self.add(
                parent,
                self.layer,
                TreeKind::Text {
                    label: "list_item".into(),
                    text: String::new(),
                    orig: None,
                    formatting: None,
                    hyperlink: None,
                    level: None,
                    list,
                },
            );
            let inline = self.add(Some(item), self.layer, group_kind("inline", "group"));
            for e in elements {
                if !e.text.is_empty() {
                    self.add(
                        Some(inline),
                        self.layer,
                        text_kind("text", &e.text, e.fmt, e.link.clone()),
                    );
                }
            }
        }
    }

    /// `_add_list_item_with_equations`: an empty list item over an inline
    /// group of alternating text and formula items.
    #[allow(clippy::too_many_arguments)]
    fn add_list_item_with_equations(
        &mut self,
        numid: &str,
        ilevel: i64,
        left_indent: i64,
        text: &str,
        equations: &[String],
        is_numbered: bool,
        ctx: &Ctx,
    ) -> Vec<usize> {
        let (refs, use_level) = self.manage_list_structure(numid, ilevel, left_indent);
        let marker = self.list_marker(numid, ilevel, is_numbered, ctx);
        let parent = self.parent_at(use_level);
        if !self.is_list_group(parent) {
            return refs;
        }
        let item = self.add(
            parent,
            self.layer,
            TreeKind::Text {
                label: "list_item".into(),
                text: String::new(),
                orig: None,
                formatting: None,
                hyperlink: None,
                level: None,
                list: Some(ListMeta {
                    enumerated: is_numbered,
                    marker,
                }),
            },
        );
        let inline = self.add(Some(item), self.layer, group_kind("inline", "group"));
        let mut created = Vec::new();
        self.add_inline_equations_to_parent(inline, text, equations, &mut created);
        refs
    }

    // ----- headers / footers ----------------------------------------------------

    /// `_add_header_footer`: every section's header and footer parts (a
    /// distinct first page contributes both its first-page and regular
    /// parts), each once, as a furniture-layer `page header` / `page footer`
    /// section holding the part's content.
    fn add_header_footer(&mut self, pkg: &mut Package, body: XmlNode, ctx: &Ctx) {
        let saved_layer = self.layer;
        let base_parents = self.parents.clone();
        let base_level = self.level;
        self.layer = Some(ContentLayer::Furniture);
        for (name, part) in header_footer_parts(body, ctx) {
            let Some(xml) = pkg.read(&part) else {
                continue;
            };
            let xml = super::mc::resolve_alternate_content(&xml);
            let xml = super::docx_hidden::resolve_hidden(&xml, ctx.hidden, false);
            let Ok(dom) = Document::parse(&xml) else {
                continue;
            };
            let root = dom.root_element();
            let par = child_elements(root)
                .filter(|n| n.has_tag_name("p"))
                .any(|p| !py_paragraph_text(p).trim().is_empty());
            let tables = child_elements(root).any(|n| n.has_tag_name("tbl"));
            let blip = has_blip(root);
            let txbx = root.descendants().any(|n| {
                n.has_tag_name("txbxContent")
                    || (n.has_tag_name("p")
                        && !is_math(n)
                        && n.ancestors()
                            .skip(1)
                            .any(|a| a.has_tag_name("textbox") || a.has_tag_name("txbx")))
                    || (n.has_tag_name("t")
                        && is_dml(n)
                        && n.ancestors()
                            .skip(1)
                            .any(|a| a.has_tag_name("p") && is_dml(a)))
            });
            if !(par || tables || blip || txbx) {
                continue;
            }
            // A part is parsed in isolation: the heading hierarchy is reset so
            // an open body heading cannot steal its content.
            self.doc_tag += 1;
            self.clear_parents_from(-1);
            self.set_parent(-1, None);
            self.level = 0;
            // Likewise, lists nested by left indentation in the body end here.
            self.nested_lists.clear();
            let group = self.add(None, self.layer, group_kind("section", name));
            self.set_parent(0, Some(group));
            self.force_new_code_block = true;
            self.pending_code_blank_lines = 0;
            let rels = part_rels(pkg, &part);
            let images = pkg.image_rels(&part, "word");
            let charts = chart_rels(pkg, &part);
            let part_ctx = ctx.for_part(&rels, &images, &charts);
            self.walk_linear(root, &part_ctx);
        }
        self.force_new_code_block = true;
        self.pending_code_blank_lines = 0;
        self.layer = saved_layer;
        self.parents = base_parents;
        self.level = base_level;
    }

    // ----- comments -----------------------------------------------------------

    /// `_extract_comment_ranges`: paragraphs holding a range start/end marker.
    fn extract_comment_ranges(&mut self, body: XmlNode) {
        for p in body
            .descendants()
            .filter(|n| n.has_tag_name("p") && !is_math(*n))
        {
            let ids = ids_of(p, &["commentRangeStart", "commentRangeEnd"]);
            if !ids.is_empty() {
                let key = self.key(p);
                self.set_para_comments(key, ids);
            }
        }
    }

    fn set_para_comments(&mut self, key: Key, ids: Vec<String>) {
        match self.para_comments.iter_mut().find(|(k, _)| *k == key) {
            Some(e) => e.1 = ids,
            None => self.para_comments.push((key, ids)),
        }
    }

    /// `_add_comments`: a notes-layer `comment_section` group per comment
    /// holding its text, linked from the annotated items' `comments` (the
    /// group, not the note text, so replies group together).
    fn add_comments(&mut self, comments: &[(String, String)]) {
        for (id, text) in comments {
            if text.is_empty() {
                continue;
            }
            let mut targets: Vec<usize> = Vec::new();
            for (para, ids) in &self.para_comments {
                if !ids.iter().any(|i| i == id) {
                    continue;
                }
                if let Some(items) = self.para_items.get(para) {
                    for &it in items {
                        let item = &self.tree.items[it];
                        let doc_item =
                            !item.deleted && !matches!(item.kind, TreeKind::Group { .. });
                        if doc_item && !targets.contains(&it) {
                            targets.push(it);
                        }
                    }
                }
            }
            let group = self.add(
                None,
                Some(ContentLayer::Notes),
                group_kind("comment_section", &format!("comment-{id}")),
            );
            self.add(
                Some(group),
                Some(ContentLayer::Notes),
                text_kind("text", text, None, None),
            );
            for t in targets {
                self.tree.items[t].comments.push(group);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::docx::DocxBackend;
    use crate::backend::DeclarativeBackend;
    use crate::format::InputFormat;
    use crate::source::SourceDocument;

    const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
    const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const M: &str = "http://schemas.openxmlformats.org/officeDocument/2006/math";

    /// `_get_heading_and_level`'s label parsing: the numeric part is the
    /// level (clamped to 1), a label that does not split around "heading"
    /// is no heading, a digit-less label keeps its name (a level-less heading
    /// when it says `Heading`).
    #[test]
    fn heading_labels_parse_like_upstream() {
        let h = Walker::heading_and_level;
        assert_eq!(h("Heading1"), ("Heading".into(), Some(1)));
        assert_eq!(h("heading 3"), ("Heading".into(), Some(3)));
        assert_eq!(h("Heading 0"), ("Heading".into(), Some(1)));
        assert_eq!(h("2Heading"), ("Heading".into(), Some(2)));
        // Not a heading by name: upstream returns `("", 0)` — clamped to 1.
        assert_eq!(h("MyHeading2"), ("".into(), Some(1)));
        assert_eq!(h("HeadingCustom"), ("HeadingCustom".into(), None));
    }

    /// `_get_paragraph_elements`: same-format runs merge into one stripped
    /// element, a whitespace-only run never opens a group, a hyperlink is
    /// always its own element, and a blank paragraph is one empty element.
    #[test]
    fn paragraph_elements_group_runs_by_format() {
        let plain = Some(Formatting::default());
        let bold = Some(Formatting {
            bold: true,
            ..Formatting::default()
        });
        let part = |t: &str, f: Option<Formatting>, l: Option<&str>| Part {
            text: t.into(),
            fmt: f,
            link: l.map(str::to_string),
            revision: None,
        };
        let els = paragraph_elements(&[
            part("Para", plain, None),
            part("graph ", plain, None),
            part(" ", bold, None),
            part("one", plain, None),
            part("bold", bold, None),
            part("link", plain, Some("https://x.y/")),
            part(" tail", bold, None),
        ]);
        let texts: Vec<(&str, bool, Option<&str>)> = els
            .iter()
            .map(|e| {
                (
                    e.text.as_str(),
                    e.fmt.is_some_and(|f| f.bold),
                    e.link.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            texts,
            vec![
                ("Paragraph  one", false, None),
                ("bold", true, None),
                ("link", false, Some("https://x.y/")),
                ("tail", true, None),
            ]
        );
        assert_eq!(
            paragraph_elements(&[part("  ", plain, None)]),
            vec![part("", None, None)]
        );
    }

    /// `_handle_equations_in_text`: an `oMath` between two runs is spliced
    /// into the paragraph text as `<eq>…</eq>` (whitespace kept); when the
    /// run texts do not reconstruct the paragraph text (a content control
    /// contributed to it) the text is returned untouched with no equations.
    #[test]
    fn equations_are_spliced_into_the_text() {
        let xml = format!(
            r#"<w:p xmlns:w="{W}" xmlns:m="{M}"><w:r><w:t xml:space="preserve">Area </w:t></w:r><m:oMath><m:r><m:t>x</m:t></m:r></m:oMath><w:r><w:t>.</w:t></w:r></w:p>"#
        );
        let dom = Document::parse(&xml).unwrap();
        let (text, eqs) = equations_in_text(dom.root_element(), "Area .");
        assert_eq!(eqs, vec!["<eq>x</eq>".to_string()]);
        assert_eq!(text, "Area <eq>x</eq>.");
        let (text, eqs) = equations_in_text(dom.root_element(), "Area extra.");
        assert!(eqs.is_empty());
        assert_eq!(text, "Area extra.");
    }

    /// A minimal package: styles (a `Title`, a `heading 2`, a bold `Emph`),
    /// numbering (`numId` 1: decimal then bullet), a comment, a header part
    /// and a body exercising the tree rules.
    fn tiny_docx() -> Vec<u8> {
        use std::io::Write;
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="{W}" xmlns:r="{R}"><w:body>
<w:p><w:pPr><w:pStyle w:val="Title"/></w:pPr><w:r><w:t>Doc Title</w:t></w:r></w:p>
<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>Deep</w:t></w:r></w:p>
<w:p><w:r><w:t xml:space="preserve">plain </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>bold</w:t></w:r><w:hyperlink r:id="rId1"><w:r><w:t>link</w:t></w:r></w:hyperlink></w:p>
<w:p><w:commentRangeStart w:id="0"/><w:r><w:t>Annotated</w:t></w:r><w:commentRangeEnd w:id="0"/></w:p>
<w:p><w:pPr><w:pStyle w:val="Emph"/></w:pPr><w:r><w:t>styled</w:t></w:r></w:p>
<w:p><w:pPr><w:rPr><w:b/></w:rPr></w:pPr><w:r><w:t>markbold</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>one</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>nested</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>two</w:t></w:r></w:p>
<w:p/>
<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>three</w:t></w:r></w:p>
<w:p><w:r><w:t>After</w:t></w:r></w:p>
<w:tbl><w:tblGrid><w:gridCol/><w:gridCol/></w:tblGrid><w:tr><w:tc><w:p><w:r><w:t>plain</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>a</w:t></w:r></w:p><w:p><w:r><w:t>b</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
<w:sectPr><w:headerReference w:type="default" r:id="rId2"/></w:sectPr>
</w:body></w:document>"#
        );
        let styles = format!(
            r#"<w:styles xmlns:w="{W}"><w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/></w:style><w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/></w:style><w:style w:type="paragraph" w:styleId="Emph"><w:name w:val="Emph"/><w:rPr><w:b/></w:rPr></w:style></w:styles>"#
        );
        let numbering = format!(
            r#"<w:numbering xmlns:w="{W}"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl><w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="&#8226;"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
        );
        let comments = format!(
            r#"<w:comments xmlns:w="{W}"><w:comment w:id="0" w:author="Ann Author" w:initials="AA" w:date="2026-01-01T00:00:00Z"><w:p><w:r><w:t>Note</w:t></w:r></w:p></w:comment></w:comments>"#
        );
        let header =
            format!(r#"<w:hdr xmlns:w="{W}"><w:p><w:r><w:t>Header text</w:t></w:r></w:p></w:hdr>"#);
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com" TargetMode="External"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/></Relationships>"#;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, content) in [
            ("word/document.xml", body.as_str()),
            ("word/styles.xml", styles.as_str()),
            ("word/numbering.xml", numbering.as_str()),
            ("word/comments.xml", comments.as_str()),
            ("word/header1.xml", header.as_str()),
            ("word/_rels/document.xml.rels", rels),
        ] {
            zw.start_file(name, opts).unwrap();
            zw.write_all(content.as_bytes()).unwrap();
        }
        zw.finish().unwrap().into_inner()
    }

    fn label_of(kind: &TreeKind) -> String {
        match kind {
            TreeKind::Text { label, .. } => label.clone(),
            TreeKind::Code { .. } => "code".into(),
            TreeKind::Group { label, name } => format!("{label}:{name}"),
            TreeKind::Table { .. } => "table".into(),
            TreeKind::Picture { .. } => "picture".into(),
            TreeKind::FieldRegion { .. } => "field_region".into(),
            TreeKind::KeyValueGraph { .. } => "key_value_region".into(),
        }
    }

    fn text_of(kind: &TreeKind) -> &str {
        match kind {
            TreeKind::Text { text, .. } | TreeKind::Code { text, .. } => text,
            _ => "",
        }
    }

    /// End to end on the package above — the shapes upstream's JSON has:
    /// everything nests under the title, a skipped heading level is a
    /// `header-1` section group, a mixed-formatting paragraph is an inline
    /// group whose items carry `formatting` and a normalized `hyperlink`,
    /// bold comes from the style chain and the paragraph mark too, a list is
    /// a `list` group with a nested group for the deeper `ilvl`, the blank
    /// spacer between two items of a resumed list is deleted, a rich cell's
    /// items move under a `rich_cell_group_1_1_0` group of the table, the
    /// header is a furniture `page header` section, and the annotated text
    /// carries the comment group in `comments`.
    #[test]
    fn tree_has_upstreams_shape() {
        let src = SourceDocument::from_bytes("t.docx", InputFormat::Docx, tiny_docx());
        let doc = DocxBackend::default().convert(&src).expect("converts");
        let tree = doc.tree.as_ref().expect("docx hands the JSON a tree");
        let items = &tree.items;
        let find = |t: &str| {
            items
                .iter()
                .position(|it| text_of(&it.kind) == t && !it.deleted)
                .unwrap_or_else(|| panic!("item {t:?}"))
        };
        let parent_of = |t: &str| items[find(t)].parent;

        let title = find("Doc Title");
        assert_eq!(label_of(&items[title].kind), "title");
        assert_eq!(tree.body.first(), Some(&title));
        // Heading 2 under a title: an invisible `header-1` fills level 1.
        let deep = find("Deep");
        let header1 = items[deep].parent.expect("under the filler group");
        assert_eq!(label_of(&items[header1].kind), "section:header-1");
        assert_eq!(items[header1].parent, Some(title));
        assert!(matches!(
            &items[deep].kind,
            TreeKind::Text { level: Some(2), .. }
        ));
        // The mixed paragraph: an inline group of three items under the heading.
        let inline = parent_of("plain").expect("inline group");
        assert_eq!(label_of(&items[inline].kind), "inline:group");
        assert_eq!(items[inline].parent, Some(deep));
        assert_eq!(items[inline].children.len(), 3);
        let bold = &items[find("bold")].kind;
        assert!(matches!(bold, TreeKind::Text { formatting: Some(f), .. } if f.bold));
        let link = &items[find("link")].kind;
        assert!(
            matches!(link, TreeKind::Text { hyperlink: Some(h), formatting: Some(f), .. } if h == "https://example.com/" && !f.bold)
        );
        // A one-part paragraph is a plain child of the heading; bold through
        // the style chain and through the paragraph mark's run properties.
        assert_eq!(parent_of("styled"), Some(deep));
        for t in ["styled", "markbold"] {
            assert!(
                matches!(&items[find(t)].kind, TreeKind::Text { formatting: Some(f), .. } if f.bold),
                "{t} is bold"
            );
        }
        // Lists: one group under the heading, the nested level its own group
        // under the item above it (docling#4863); the blank spacer
        // paragraph's empty item is deleted when the list resumes, and
        // numbering continues.
        let list = parent_of("one").expect("list group");
        assert_eq!(label_of(&items[list].kind), "list:list");
        assert_eq!(items[list].parent, Some(deep));
        let nested = parent_of("nested").expect("nested list group");
        assert_eq!(label_of(&items[nested].kind), "list:list");
        assert_eq!(items[nested].parent, Some(find("one")));
        assert_eq!(parent_of("two"), Some(list));
        assert_eq!(parent_of("three"), Some(list));
        let marker = |t: &str| match &items[find(t)].kind {
            TreeKind::Text { list: Some(l), .. } => (l.enumerated, l.marker.clone()),
            other => panic!("{t}: {other:?}"),
        };
        assert_eq!(marker("one"), (true, "1.".into()));
        assert_eq!(marker("nested"), (false, "".into()));
        assert_eq!(marker("three"), (true, "3.".into()));
        assert!(
            items
                .iter()
                .any(|it| it.deleted && text_of(&it.kind).is_empty()),
            "the spacer's empty text item is deleted"
        );
        assert_eq!(parent_of("After"), Some(deep), "body text closes the list");
        // The table under the heading; its rich cell's items re-parented
        // under the table's group, the plain cell kept as text.
        let table = items
            .iter()
            .position(|it| matches!(it.kind, TreeKind::Table { .. }))
            .expect("table");
        assert_eq!(items[table].parent, Some(deep));
        let rich = parent_of("a").expect("rich cell group");
        assert_eq!(
            label_of(&items[rich].kind),
            "unspecified:rich_cell_group_1_1_0"
        );
        assert_eq!(items[rich].parent, Some(table));
        assert_eq!(parent_of("b"), Some(rich));
        if let TreeKind::Table {
            table: t,
            rich_cells,
            ..
        } = &items[table].kind
        {
            let cells = t.cells.as_ref().expect("cells");
            assert_eq!(cells[0].text, "plain");
            assert_eq!(cells[1].text, "a\nb");
            assert_eq!(rich_cells, &vec![(0, 1, rich)]);
        }
        // Header: a furniture section holding the part's text.
        let hdr = parent_of("Header text").expect("page header group");
        assert_eq!(label_of(&items[hdr].kind), "section:page header");
        assert_eq!(items[hdr].layer, Some(ContentLayer::Furniture));
        assert_eq!(items[hdr].parent, None);
        assert_eq!(
            items[find("Header text")].layer,
            Some(ContentLayer::Furniture)
        );
        // The comment: a notes-layer group linked from the annotated item.
        let note = find("[author: Ann Author (AA), time: 2026-01-01T00:00:00.000+00:00]: Note");
        let group = items[note].parent.expect("comment group");
        assert_eq!(label_of(&items[group].kind), "comment_section:comment-0");
        assert_eq!(items[group].layer, Some(ContentLayer::Notes));
        assert_eq!(items[find("Annotated")].comments, vec![group]);

        // In the JSON: `comments` sits between `prov` and `orig`, the deleted
        // item is gone and the numbering is contiguous.
        let json: serde_json::Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let annotated = json["texts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["text"] == "Annotated")
            .expect("annotated text");
        let keys: Vec<&str> = annotated
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let pos = |k: &str| keys.iter().position(|x| *x == k).unwrap();
        assert!(
            pos("prov") < pos("comments") && pos("comments") < pos("orig"),
            "{keys:?}"
        );
        assert_eq!(
            annotated["comments"][0]["$ref"],
            format!("#/groups/{}", tree.bucket_index(group))
        );
        let texts = json["texts"].as_array().unwrap();
        assert!(
            texts.iter().all(|t| t["text"] != ""),
            "no empty text item survives"
        );
        for (i, t) in texts.iter().enumerate() {
            assert_eq!(t["self_ref"], format!("#/texts/{i}"));
        }
    }
}
