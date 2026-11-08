//! docling's item tree for OpenDocument files — the structure the JSON export
//! serializes.
//!
//! `odf.rs` walks `content.xml` into the flat node stream Markdown / DocLang /
//! LaTeX render. Upstream's `opendocument_backend` hands docling-core a
//! different shape: a paragraph of several formatting/hyperlink runs is an
//! `inline` group of one `text` item per run, a heading or title with mixed
//! runs is an empty block over such a group, lists are `list` groups whose
//! items carry docling's marker (`1.`, or the level style's `num-suffix`) and
//! nest under the previous item, a table cell holding lists, several
//! paragraphs, images or a nested table is a `RichTableCell` whose content
//! lives in a `rich_cell_group_{table}_{col}_{row}` group under the table,
//! an embedded chart is a classified picture with its data, a presentation
//! is one `chapter` group per slide, and a spreadsheet one `section` group
//! per sheet with every table's cell-range provenance on the sheet's page.
//! Every item is numbered in creation order.
//!
//! This module ports that construction call-for-call (docling 2.129,
//! `_add_odf_children` / `_add_odf_paragraph` / `_add_odf_list` /
//! `_add_table_from_odf`, the `Odt`/`Odp`/`Ods` converters) into a
//! [`docling_core::tree::ItemTree`], reading the XML through `odf.rs`'s own
//! style, run, list and cell helpers so both walks read the file alike.

use std::collections::{HashMap, HashSet, VecDeque};

use docling_core::tree::{Formatting, ItemTree, ListMeta, TreeKind, TreeNote, TreeProv};
use docling_core::{ContentLayer, PictureDescription, PictureImage, Script, Table, TableCell};
use roxmltree::Node as XmlNode;

use super::odf::{
    attr, cell_has_image, collect_runs, comment_threads, element_has_text, image_can_be_bitmap,
    in_text_box, inline_chart, is_list_tag, is_rich_cell, is_slide_title_element,
    is_transparent_container, item_has_content, item_headings, level_is_enumerated, level_start,
    list_has_direct_text, list_has_renderable, list_items, list_level,
    list_starts_with_empty_nested, odf_item_content, odf_marker, ods_cell_text, para_plain_text,
    paragraph_list_style, paragraph_style_names, repeat, section_is_hidden,
    slide_has_visible_title, slide_is_hidden, text_boxes, Annotation, Fmt, MasterPart, OdfLevel,
    Run, Styles,
};

/// The tree plus the spreadsheet pages docling adds (`page_no`, width,
/// height) — the JSON's `pages` map comes from the flat stream's markers, so
/// the caller pushes them there.
pub(super) struct Built {
    pub(super) tree: ItemTree,
    pub(super) pages: Vec<(usize, f64, f64)>,
}

/// docling's `_OdfListState`: what a following sibling list may continue.
#[derive(Clone, Copy)]
struct ListState {
    group: usize,
    last_item: Option<usize>,
    enumerated: bool,
    counter: i64,
}

/// The `list` group a run of style-numbered paragraphs (#826) shares.
struct StyleListGroup {
    style: String,
    group: usize,
    enumerated: bool,
    counter: i64,
    level: Option<OdfLevel>,
}

/// A comment thread (#829): its first comment's holding `<text:p>` /
/// `<text:h>`, the comments, and whether its group exists yet.
struct CommentThread {
    anchor: Option<roxmltree::NodeId>,
    comments: Vec<usize>,
    attached: bool,
}

/// The shapes of a notes page that show the slide or a field rather than the
/// speaker notes (docling's `_ODP_NOTES_PAGE_FIELD_CLASSES`): the slide
/// thumbnail, and the slide number, date, header and footer fields.
pub(super) const NOTES_PAGE_FIELD_CLASSES: &[&str] =
    &["page", "page-number", "date-time", "header", "footer"];

struct Walker<'s> {
    tree: ItemTree,
    styles: &'s Styles,
    tables: usize,
    /// `<text:note>` calls placed in text items: `(item, offset, note)` —
    /// hung on the items once the note bodies exist (#538).
    note_calls: Vec<(usize, usize, roxmltree::NodeId)>,
    /// The body's reviewer comments and their threads (#829).
    annotations: Vec<Annotation>,
    threads: Vec<CommentThread>,
}

/// `<office:text>` body children.
pub(super) fn build_text(
    body: XmlNode,
    styles: &Styles,
    annotations: &[Annotation],
    furniture: &[MasterPart],
) -> Built {
    let mut w = Walker::new(styles);
    // `_detach_annotations` (docling#4436, #829): the comments and their
    // threads, attached as `comment_section` groups right after the item
    // holding each thread's first comment is created.
    w.annotations = annotations.to_vec();
    w.threads = comment_threads(annotations)
        .into_iter()
        .map(|comments| CommentThread {
            anchor: annotations[comments[0]].anchor,
            comments,
            attached: false,
        })
        .collect();
    w.add_children(body.children().filter(XmlNode::is_element), None, None);
    // The master pages' headers and footers (#830): the DOCX backend's
    // `_add_header_footer` shape — a furniture-layer `page header` /
    // `page footer` section per part, holding the part's blocks.
    for part in furniture {
        let group = w.tree.add(
            None,
            Some(ContentLayer::Furniture),
            TreeKind::Group {
                label: "section".into(),
                name: if part.footer {
                    "page footer".into()
                } else {
                    "page header".into()
                },
            },
        );
        w.add_children(
            part.el.children().filter(XmlNode::is_element),
            Some(group),
            Some(ContentLayer::Furniture),
        );
    }
    // `_add_footnotes` (docling#4375): each note body a furniture `footnote`
    // text on the body, after the whole walk.
    let mut bodies = std::collections::HashMap::new();
    for (note, text) in super::odf::note_bodies(body) {
        let id = w.tree.add(
            None,
            Some(ContentLayer::Furniture),
            TreeKind::Text {
                label: "footnote".into(),
                text: text.clone(),
                orig: None,
                formatting: None,
                hyperlink: None,
                level: None,
                list: None,
            },
        );
        // A comment in the note's body hangs on the footnote item.
        let note_el = body.document().get_node(note).expect("note node");
        w.attach_comments(
            note_el
                .descendants()
                .filter(|n| n.has_tag_name("p") || n.has_tag_name("h"))
                .map(|n| n.id()),
            Some(id),
        );
        bodies.insert(note, (id, text));
    }
    // #538: each note on the text item that calls it, for the Pandoc AST.
    for (item, offset, note) in std::mem::take(&mut w.note_calls) {
        if let Some((body_id, text)) = bodies.get(&note) {
            w.tree.items[item].notes.push(TreeNote {
                offset,
                text: text.clone(),
            });
            w.tree.items[*body_id].note_body = true;
        }
    }
    // `attach_rest`: a thread whose anchor made no item — an empty
    // paragraph, a plain table cell's — goes under the root, last.
    for t in 0..w.threads.len() {
        if !w.threads[t].attached {
            w.attach_thread(t, None);
        }
    }
    Built {
        tree: w.tree,
        pages: Vec::new(),
    }
}

/// `<office:presentation>`: `OdpDocumentBackend.convert`.
pub(super) fn build_presentation(pres: XmlNode, styles: &Styles) -> Built {
    let mut w = Walker::new(styles);
    for (idx, page) in pres
        .children()
        .filter(|c| c.has_tag_name("page"))
        .enumerate()
    {
        let slide_name = attr(page, "name")
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("slide-{}", idx + 1));
        let slide = w.tree.add(
            None,
            None,
            TreeKind::Group {
                label: "chapter".into(),
                name: format!("slide-{idx}"),
            },
        );
        if !slide_has_visible_title(page) {
            w.tree.add(
                Some(slide),
                None,
                text_kind("title", &slide_name, None, None),
            );
        }
        w.walk_slide(page, slide);
        // `_hide_slide` (#753, docling#4767): the slide group and its body
        // items move to the invisible layer.
        if slide_is_hidden(page, styles) {
            for item in &mut w.tree.items[slide..] {
                if item.layer.is_none() {
                    item.layer = Some(ContentLayer::Invisible);
                }
            }
        }
    }
    Built {
        tree: w.tree,
        pages: Vec::new(),
    }
}

/// `<office:spreadsheet>`: `OdsDocumentBackend.convert`.
pub(super) fn build_spreadsheet(sheet: XmlNode, styles: &Styles) -> Built {
    let mut w = Walker::new(styles);
    let mut pages = Vec::new();
    for (ix, table) in sheet
        .children()
        .filter(|c| c.has_tag_name("table"))
        .enumerate()
    {
        let page_no = ix + 1;
        let layer = (attr(table, "display") == Some("false")).then_some(ContentLayer::Invisible);
        let group = w.tree.add(
            None,
            layer,
            TreeKind::Group {
                label: "section".into(),
                name: format!("sheet: {}", attr(table, "name").unwrap_or("")),
            },
        );
        let first = w.tree.items.len();
        w.convert_sheet(table, group, page_no, layer);
        // `_find_page_size`: the extent of the page's items in cell units
        // (`right − left`, `bottom − top`; 0×0 for a sheet without any).
        let mut ext: Option<(f64, f64, f64, f64)> = None;
        for item in &w.tree.items[first..] {
            if let Some(p) = &item.prov {
                let [l, t, r, b] = p.bbox;
                ext = Some(match ext {
                    None => (l, t, r, b),
                    Some((el, et, er, eb)) => (el.min(l), et.min(t), er.max(r), eb.max(b)),
                });
            }
        }
        let (w_, h_) = ext.map_or((0.0, 0.0), |(l, t, r, b)| (r - l, b - t));
        pages.push((page_no, w_, h_));
    }
    Built {
        tree: w.tree,
        pages,
    }
}

fn text_kind(
    label: &str,
    text: &str,
    formatting: Option<Formatting>,
    hyperlink: Option<&str>,
) -> TreeKind {
    TreeKind::Text {
        label: label.into(),
        text: text.into(),
        orig: None,
        formatting,
        hyperlink: hyperlink.map(str::to_string),
        level: None,
        list: None,
    }
}

/// docling's `Formatting` for a run, `None` when nothing is set
/// (`_formatting_or_none`).
fn formatting_of(fmt: Fmt) -> Option<Formatting> {
    (fmt != Fmt::default()).then_some(Formatting {
        bold: fmt.bold,
        italic: fmt.italic,
        underline: fmt.underline,
        strikethrough: fmt.strike,
        script: match fmt.script {
            1 => Script::Sub,
            2 => Script::Super,
            _ => Script::Baseline,
        },
    })
}

/// `_normalize_odf_text_runs`: merge same-format/same-link neighbours and
/// trim the paragraph's two ends (`trim_run_edges`). A hyperlink boundary is
/// not special under the whitespace contract (docling#3891): the boundary
/// space stays in whichever run owns it, and the serializer places it
/// outside the link markup.
fn normalize_runs(runs: Vec<Run>) -> Vec<Run> {
    let mut merged: Vec<Run> = Vec::new();
    for run in runs {
        if run.text.is_empty() {
            continue;
        }
        if let Some(last) = merged.last_mut() {
            if last.fmt == run.fmt && last.href == run.href {
                last.text.push_str(&run.text);
                continue;
            }
        }
        merged.push(run);
    }
    docling_core::inline::trim_run_edges(merged, |r| r.text.as_str(), |r, t| r.text = t, |_| true)
}

/// `_odf_text_from_runs`.
fn text_from_runs(runs: &[Run]) -> String {
    runs.iter()
        .map(|r| r.text.as_str())
        .collect::<String>()
        .trim()
        .to_string()
}

/// `_odf_text_runs(element)`, normalized.
fn runs_of(el: XmlNode, styles: &Styles) -> Vec<Run> {
    let mut runs = Vec::new();
    collect_runs(el, styles, Fmt::default(), &mut runs);
    normalize_runs(runs)
}

/// `_clean_odf_text_lines`: stripped, non-blank lines.
fn clean_lines_vec(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// `_odf_image_href` on a `<draw:image>`.
fn image_href<'a>(img: XmlNode<'a, '_>) -> Option<&'a str> {
    attr(img, "href")
}

/// `_strip_odf_image_reference_text`: LibreOffice writes `(Pictures/x.png)`
/// placeholders into a paragraph's text for its anchored images.
fn strip_image_refs(text: &str, images: &[XmlNode]) -> String {
    let mut remaining = text.to_string();
    for img in images {
        if let Some(href) = image_href(*img) {
            let href = href.trim();
            for r in [href, href.strip_prefix("./").unwrap_or(href)] {
                remaining = remaining.replace(&format!("({r})"), "");
            }
        }
    }
    remaining
}

/// A table's cell positions: the anchor cell at each grid position it
/// starts (repeats expanded), and the covered positions.
struct Grid<'a, 'i> {
    anchors: HashMap<(usize, usize), XmlNode<'a, 'i>>,
    covered: HashSet<(usize, usize)>,
}

/// Expand a table's rows/cells with their `number-*-repeated` counts —
/// odfdo's `traverse()`. Without `within`, only cells with content (and the
/// covered positions) are recorded — enough for the data bounds — and an
/// empty repeated stretch (a sheet padded to a million blank rows) only
/// advances the index; with `within` bounds every anchor inside them is
/// recorded, empty ones included, as docling records a `TableCell` for each.
/// A cell is materialised at most 1024 times, as the flat spreadsheet walk
/// does.
fn grid_of<'a, 'i>(
    table: XmlNode<'a, 'i>,
    within: Option<(usize, usize, usize, usize)>,
) -> Grid<'a, 'i> {
    let mut anchors = HashMap::new();
    let mut covered = HashSet::new();
    let rows = table.descendants().filter(|n| {
        n.has_tag_name("table-row")
            && n.ancestors().find(|a| a.has_tag_name("table")) == Some(table)
    });
    let inside = |r: usize, c: usize| {
        within.is_none_or(|(r0, r1, c0, c1)| (r0..=r1).contains(&r) && (c0..=c1).contains(&c))
    };
    let mut r = 0usize;
    for row in rows {
        let rrep = repeat(row, "number-rows-repeated");
        if let Some((_, r1, _, _)) = within {
            if r > r1 {
                break;
            }
        }
        let mut c = 0usize;
        let mut row_cells: Vec<(usize, XmlNode, bool)> = Vec::new();
        for cell in row
            .children()
            .filter(|n| n.has_tag_name("table-cell") || n.has_tag_name("covered-table-cell"))
        {
            let crep = repeat(cell, "number-columns-repeated");
            let is_covered = cell.has_tag_name("covered-table-cell");
            if is_covered || within.is_some() || cell_has_content(cell) {
                for k in 0..crep.min(1024) {
                    row_cells.push((c + k, cell, is_covered));
                }
            }
            c += crep;
        }
        if !row_cells.is_empty() {
            for k in 0..rrep.min(1024) {
                for (cc, cell, is_covered) in &row_cells {
                    if !inside(r + k, *cc) {
                        continue;
                    }
                    if *is_covered {
                        covered.insert((r + k, *cc));
                    } else {
                        anchors.insert((r + k, *cc), *cell);
                    }
                }
            }
        }
        r += rrep;
    }
    Grid { anchors, covered }
}

fn span(cell: XmlNode, name: &str) -> usize {
    attr(cell, name)
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(1)
}

/// `_odf_cell_has_content`: text or an image.
fn cell_has_content(cell: XmlNode) -> bool {
    !cell_text(cell).is_empty() || cell_has_image(cell)
}

/// `_odf_cell_text`: a rich cell's child lines (image placeholders
/// stripped); a typed cell's `str(cell.value)` — a string cell's paragraph
/// text as written, a number/percentage/currency's `office:value`, a date or
/// time's value attribute, a boolean as Python spells it; otherwise the
/// cell's child lines, or nothing.
fn cell_text(cell: XmlNode) -> String {
    let lines: Vec<String> = cell
        .children()
        .filter(XmlNode::is_element)
        .flat_map(element_text_lines)
        .collect();
    let child_text = lines.join("\n");
    if rich_content(cell) {
        let images: Vec<XmlNode> = cell
            .descendants()
            .filter(|n| n.has_tag_name("image"))
            .collect();
        return strip_image_refs(&child_text, &images);
    }
    if let Some(value_type) = attr(cell, "value-type") {
        let value = match value_type {
            "string" => Some(
                cell.children()
                    .filter(|c| c.has_tag_name("p") || c.has_tag_name("h"))
                    .map(para_plain_text)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            "float" | "percentage" | "currency" => attr(cell, "value").map(str::to_string),
            "date" => attr(cell, "date-value").map(str::to_string),
            "time" => attr(cell, "time-value").map(str::to_string),
            "boolean" => attr(cell, "boolean-value").map(|b| {
                if b == "true" {
                    "True".to_string()
                } else {
                    "False".to_string()
                }
            }),
            _ => None,
        };
        if let Some(v) = value {
            return v;
        }
    }
    if !child_text.is_empty() {
        return child_text;
    }
    if cell.children().any(|c| c.is_element()) {
        return String::new();
    }
    clean_lines_vec(&para_plain_text(cell)).join("\n")
}

/// `_odf_cell_has_rich_content` without the style table: the structural
/// signals (image, list, header, nested table, several paragraphs, or any
/// paragraph in an untyped cell).
fn rich_content(cell: XmlNode) -> bool {
    if cell_has_image(cell) {
        return true;
    }
    let mut paragraphs = 0;
    for child in cell.children().filter(XmlNode::is_element) {
        match child.tag_name().name() {
            n if is_list_tag(n) => {
                if !element_text_lines(child).is_empty() {
                    return true;
                }
            }
            "h" if !clean_lines_vec(&para_plain_text(child)).is_empty() => return true,
            "table" if super::odf::table_has_content(child) => return true,
            "p" if !clean_lines_vec(&para_plain_text(child)).is_empty() => paragraphs += 1,
            _ => {}
        }
    }
    paragraphs > 1 || (attr(cell, "value-type").is_none() && paragraphs > 0)
}

/// `_odf_element_text_lines`.
fn element_text_lines(el: XmlNode) -> Vec<String> {
    match el.tag_name().name() {
        n if is_list_tag(n) => list_items(el).flat_map(element_text_lines).collect(),
        "list-item" | "list-header" => {
            let lines: Vec<String> = el
                .children()
                .filter(XmlNode::is_element)
                .flat_map(element_text_lines)
                .collect();
            if lines.is_empty() {
                clean_lines_vec(&para_plain_text(el))
            } else {
                lines
            }
        }
        "p" | "h" => clean_lines_vec(&para_plain_text(el)),
        _ => {
            let lines: Vec<String> = el
                .children()
                .filter(XmlNode::is_element)
                .flat_map(element_text_lines)
                .collect();
            if lines.is_empty() {
                clean_lines_vec(&para_plain_text(el))
            } else {
                lines
            }
        }
    }
}

/// `_find_true_data_bounds`: the smallest rectangle holding every cell with
/// content, every covered position and every span's extent; `(0, 0, 0, 0)`
/// for an empty table.
fn data_bounds(grid: &Grid) -> (usize, usize, usize, usize) {
    let mut bounds: Option<(usize, usize, usize, usize)> = None;
    let mut extend = |r0: usize, r1: usize, c0: usize, c1: usize| {
        bounds = Some(match bounds {
            None => (r0, r1, c0, c1),
            Some((a, b, c, d)) => (a.min(r0), b.max(r1), c.min(c0), d.max(c1)),
        });
    };
    for (&(r, c), cell) in &grid.anchors {
        extend(r, r, c, c);
        let (rs, cs) = (
            span(*cell, "number-rows-spanned"),
            span(*cell, "number-columns-spanned"),
        );
        if rs > 1 || cs > 1 {
            extend(r, r + rs - 1, c, c + cs - 1);
        }
    }
    for &(r, c) in &grid.covered {
        extend(r, r, c, c);
    }
    bounds.unwrap_or((0, 0, 0, 0))
}

impl<'s> Walker<'s> {
    fn new(styles: &'s Styles) -> Self {
        Walker {
            tree: ItemTree {
                // The runs keep their boundary whitespace (schema 1.11.0, #997).
                inline_whitespace: true,
                ..ItemTree::default()
            },
            styles,
            tables: 0,
            note_calls: Vec::new(),
            annotations: Vec::new(),
            threads: Vec::new(),
        }
    }

    /// `_OdfComments.attach` (#829): the threads anchored on one of the
    /// `anchors` elements get their `comment_section` group under `holder`.
    fn attach_comments(
        &mut self,
        anchors: impl Iterator<Item = roxmltree::NodeId>,
        holder: Option<usize>,
    ) {
        let anchors: Vec<roxmltree::NodeId> = anchors.collect();
        if anchors.is_empty() {
            return;
        }
        for t in 0..self.threads.len() {
            let thread = &self.threads[t];
            if !thread.attached && thread.anchor.is_some_and(|a| anchors.contains(&a)) {
                self.attach_thread(t, holder);
            }
        }
    }

    /// `_OdfComments._add_group`: a notes-layer `comment_section` group named
    /// `comment-{thread}` under `holder` (the root without one), one `text`
    /// per comment of the thread, and — when the holder is a document item,
    /// not an `inline` group — the group's ref in the holder's `comments`.
    fn attach_thread(&mut self, t: usize, holder: Option<usize>) {
        self.threads[t].attached = true;
        let group = self.tree.add(
            holder,
            Some(ContentLayer::Notes),
            TreeKind::Group {
                label: "comment_section".into(),
                name: format!("comment-{}", t + 1),
            },
        );
        for c in self.threads[t].comments.clone() {
            let text = self.annotations[c].text.clone();
            self.tree.add(
                Some(group),
                Some(ContentLayer::Notes),
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
        if let Some(h) = holder {
            if !matches!(self.tree.items[h].kind, TreeKind::Group { .. }) {
                self.tree.items[h].comments.push(group);
            }
        }
    }

    /// The item a paragraph-like block made at `parent`'s level since
    /// `first_new`: the last one, a heading over its inline group included
    /// — the comment holder.
    fn block_item(&self, first_new: usize, parent: Option<usize>) -> Option<usize> {
        (first_new..self.tree.items.len())
            .rev()
            .find(|&i| self.tree.items[i].parent == parent && !self.tree.items[i].deleted)
    }

    /// `_add_odf_text_runs`: one run is the item, several an `inline` group of
    /// items each carrying `label`.
    fn add_text_runs(
        &mut self,
        runs: &[Run],
        label: &str,
        parent: Option<usize>,
        layer: Option<ContentLayer>,
    ) -> Option<usize> {
        match runs {
            [] => None,
            [run] => Some(self.tree.add(
                parent,
                layer,
                text_kind(
                    label,
                    &run.text,
                    formatting_of(run.fmt),
                    run.href.as_deref(),
                ),
            )),
            _ => {
                let group = self.inline_group(parent, layer);
                for run in runs {
                    self.tree.add(
                        Some(group),
                        layer,
                        text_kind(
                            label,
                            &run.text,
                            formatting_of(run.fmt),
                            run.href.as_deref(),
                        ),
                    );
                }
                Some(group)
            }
        }
    }

    fn inline_group(&mut self, parent: Option<usize>, layer: Option<ContentLayer>) -> usize {
        self.tree.add(
            parent,
            layer,
            TreeKind::Group {
                label: "inline".into(),
                name: "group".into(),
            },
        )
    }

    /// `_add_odf_rich_block`: a title/heading from runs — one run is the
    /// block itself, several an empty block over an `inline` group of `text`
    /// runs. `block` builds the kind from (text, formatting, hyperlink).
    fn add_rich_block(
        &mut self,
        runs: &[Run],
        parent: Option<usize>,
        layer: Option<ContentLayer>,
        block: impl Fn(&str, Option<Formatting>, Option<&str>) -> TreeKind,
    ) {
        match runs {
            [] => {}
            [run] => {
                self.tree.add(
                    parent,
                    layer,
                    block(&run.text, formatting_of(run.fmt), run.href.as_deref()),
                );
            }
            _ => {
                let item = self.tree.add(parent, layer, block("", None, None));
                let group = self.inline_group(Some(item), layer);
                for run in runs {
                    self.tree.add(
                        Some(group),
                        layer,
                        text_kind(
                            "text",
                            &run.text,
                            formatting_of(run.fmt),
                            run.href.as_deref(),
                        ),
                    );
                }
            }
        }
    }

    /// `_add_odf_heading`.
    fn add_heading(&mut self, el: XmlNode, parent: Option<usize>, layer: Option<ContentLayer>) {
        let level = attr(el, "outline-level")
            .or_else(|| attr(el, "level"))
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(1)
            .max(1) as u8;
        let runs = runs_of(el, self.styles);
        self.add_rich_block(&runs, parent, layer, |text, f, h| TreeKind::Text {
            label: "section_header".into(),
            text: text.into(),
            orig: None,
            formatting: f,
            hyperlink: h.map(str::to_string),
            level: Some(level),
            list: None,
        });
    }

    /// `_add_odf_charts`: every `<draw:frame>` under (or being) `el` that
    /// embeds a chart object becomes a classified picture with its data.
    fn add_charts(
        &mut self,
        el: XmlNode,
        parent: Option<usize>,
        layer: Option<ContentLayer>,
    ) -> usize {
        let mut frames: Vec<XmlNode> = Vec::new();
        if el.has_tag_name("frame") {
            frames.push(el);
        }
        // A frame inside a text box is the box's own block (#831).
        frames.extend(
            el.descendants()
                .filter(|n| n.has_tag_name("frame") && *n != el && !in_text_box(*n, el)),
        );
        let mut count = 0;
        for frame in frames {
            let Some(obj) = frame.children().find(|c| c.has_tag_name("object")) else {
                continue;
            };
            let name = attr(obj, "href").unwrap_or("").trim_start_matches("./");
            let info = self
                .styles
                .charts
                .get(name)
                .cloned()
                .or_else(|| inline_chart(obj, self.styles));
            let Some(info) = info else {
                continue;
            };
            self.tree.add(
                parent,
                layer,
                TreeKind::Picture {
                    captions: Vec::new(),
                    image: None,
                    extra_classes: Vec::new(),
                    classification: Some(info.kind),
                    description: None,
                    confidence: None,
                    chart: Some(info.table),
                    dpi: None,
                },
            );
            count += 1;
        }
        count
    }

    /// `_add_odf_images`: a picture per `<draw:image>` PIL can open — an
    /// embedded bitmap part — skipping a chart's `ObjectReplacements/`
    /// preview once the chart itself was emitted. Returns how many were added.
    fn add_images(
        &mut self,
        images: &[XmlNode],
        parent: Option<usize>,
        layer: Option<ContentLayer>,
        skip_object_replacements: bool,
    ) -> usize {
        let mut count = 0;
        for img in images {
            let href = image_href(*img).unwrap_or("");
            if skip_object_replacements
                && href
                    .trim_start_matches("./")
                    .starts_with("ObjectReplacements/")
            {
                continue;
            }
            let Some(image) = self.image_payload(*img, href) else {
                continue;
            };
            // The frame's alt text and displayed size (#834).
            self.tree.add(
                parent,
                layer,
                TreeKind::Picture {
                    captions: Vec::new(),
                    image: super::odf::with_frame_display(Some(image), *img),
                    extra_classes: Vec::new(),
                    classification: None,
                    description: super::odf::frame_alt(*img).map(PictureDescription::alt_text),
                    confidence: None,
                    chart: None,
                    dpi: None,
                },
            );
            count += 1;
        }
        count
    }

    /// `_image_ref_from_odf_image`: the decoded embedded part, when the
    /// reference can be a bitmap and the package holds it.
    fn image_payload(&self, img: XmlNode, href: &str) -> Option<PictureImage> {
        if !image_can_be_bitmap(img, href) {
            return None;
        }
        let name = href.trim_start_matches("./").trim_start_matches('#');
        self.styles.images.get(name).cloned()
    }

    /// `_add_odf_paragraph`.
    fn add_paragraph(&mut self, el: XmlNode, parent: Option<usize>, layer: Option<ContentLayer>) {
        let chart_count = self.add_charts(el, parent, layer);
        let images: Vec<XmlNode> = el
            .descendants()
            .filter(|n| n.has_tag_name("image") && !in_text_box(*n, el))
            .collect();
        let image_count = self.add_images(&images, parent, layer, chart_count > 0);
        let mut runs = runs_of(el, self.styles);
        let mut text = text_from_runs(&runs);
        if !images.is_empty() {
            let stripped = strip_image_refs(&text, &images).trim().to_string();
            if stripped != text {
                runs = if stripped.is_empty() {
                    Vec::new()
                } else {
                    vec![Run {
                        text: stripped.clone(),
                        fmt: Fmt::default(),
                        href: None,
                    }]
                };
                text = stripped;
            }
        }
        if image_count > 0 && strip_image_refs(&text, &images).trim().is_empty() {
            return;
        }
        if chart_count > 0 && (text.contains("ObjectReplacements") || text.is_empty()) {
            return;
        }
        let names = paragraph_style_names(self.styles, attr(el, "style-name"));
        if names.iter().any(|n| n == "Title") {
            self.add_rich_block(&runs, parent, layer, |t, f, h| text_kind("title", t, f, h));
        } else if names.iter().any(|n| n == "Subtitle") {
            self.add_rich_block(&runs, parent, layer, |t, f, h| TreeKind::Text {
                label: "section_header".into(),
                text: t.into(),
                orig: None,
                formatting: f,
                hyperlink: h.map(str::to_string),
                level: Some(1),
                list: None,
            });
        } else {
            self.add_text_runs(&runs, "text", parent, layer);
        }
    }

    /// `_add_odf_children`: sibling blocks, a list continuing the previous
    /// sibling list's numbering when it opens with an empty nested item.
    fn add_children<'a, 'i: 'a>(
        &mut self,
        els: impl Iterator<Item = XmlNode<'a, 'i>>,
        parent: Option<usize>,
        layer: Option<ContentLayer>,
    ) {
        let mut prev: Option<ListState> = None;
        let mut style_run: Option<StyleListGroup> = None;
        for el in els {
            if is_list_tag(el.tag_name().name()) {
                style_run = None;
                prev = self.add_list(el, parent, layer, false, 1, prev);
            } else if let Some((style, level)) = el
                .has_tag_name("p")
                .then(|| paragraph_list_style(self.styles, attr(el, "style-name")))
                .flatten()
            {
                prev = None;
                self.add_style_list_paragraph(el, style, level, parent, layer, &mut style_run);
            } else {
                prev = None;
                style_run = None;
                self.add_child(el, parent, layer);
            }
        }
    }

    /// A `<text:p>` numbered through its paragraph style (#826): an item of
    /// the `list` group the run of such paragraphs shares — a new group when
    /// the list style changes — with the marker `add_list` would give it.
    fn add_style_list_paragraph(
        &mut self,
        el: XmlNode,
        style: String,
        level: i64,
        parent: Option<usize>,
        layer: Option<ContentLayer>,
        run: &mut Option<StyleListGroup>,
    ) {
        let runs = runs_of(el, self.styles);
        let text = text_from_runs(&runs);
        if text.is_empty() {
            return;
        }
        if run.as_ref().is_none_or(|r| r.style != style) {
            let lv = super::odf::list_level(self.styles, Some(&style), level);
            let group = self.tree.add(
                parent,
                layer,
                TreeKind::Group {
                    label: "list".into(),
                    name: "list".into(),
                },
            );
            *run = Some(StyleListGroup {
                style,
                group,
                enumerated: lv.is_some_and(|l| l.numbered),
                counter: lv.map_or(0, |l| l.start - 1),
                level: lv.cloned(),
            });
        }
        let Some(r) = run.as_mut() else {
            return;
        };
        r.counter += 1;
        let marker = if r.enumerated {
            odf_marker(r.counter, r.level.as_ref())
        } else {
            String::new()
        };
        let meta = ListMeta {
            enumerated: r.enumerated,
            marker,
        };
        let group = r.group;
        let first_new = self.tree.items.len();
        let item = self.add_list_item(group, layer, &runs, &text, meta);
        self.anchor_notes(el, first_new);
        self.attach_comments(std::iter::once(el.id()), Some(item));
        self.add_text_boxes(el, parent, layer);
    }

    /// The text boxes `el` anchors (#831), each a `textbox` section group of
    /// its blocks after the anchoring item — the DOCX backend's
    /// `_handle_textbox_content` shape.
    fn add_text_boxes(&mut self, el: XmlNode, parent: Option<usize>, layer: Option<ContentLayer>) {
        for tb in text_boxes(el) {
            let group = self.tree.add(
                parent,
                layer,
                TreeKind::Group {
                    label: "section".into(),
                    name: "textbox".into(),
                },
            );
            self.add_children(
                tb.children().filter(XmlNode::is_element),
                Some(group),
                layer,
            );
        }
    }

    /// The `<text:note>` calls in `el` (a paragraph, heading or list item
    /// whose items were created from `first_new` on), placed in those items
    /// ([`ItemTree::place_note_calls`], #538).
    fn anchor_notes(&mut self, el: XmlNode, first_new: usize) {
        if !el.descendants().any(|n| n.has_tag_name("note")) {
            return;
        }
        let (text, calls) = super::odf::note_calls(el);
        let offsets: Vec<usize> = calls.iter().map(|c| c.0).collect();
        let placed = self.tree.place_note_calls(first_new, &text, &offsets);
        for ((_, note), place) in calls.into_iter().zip(placed) {
            if let Some((item, offset)) = place {
                self.note_calls.push((item, offset, note));
            }
        }
    }

    /// `_add_odf_child`.
    fn add_child(&mut self, el: XmlNode, parent: Option<usize>, layer: Option<ContentLayer>) {
        let first_new = self.tree.items.len();
        match el.tag_name().name() {
            "h" => {
                self.add_heading(el, parent, layer);
                self.anchor_notes(el, first_new);
                let holder = self.block_item(first_new, parent);
                self.attach_comments(std::iter::once(el.id()), holder);
                self.add_text_boxes(el, parent, layer);
            }
            "p" => {
                self.add_paragraph(el, parent, layer);
                self.anchor_notes(el, first_new);
                // A comment on a paragraph that made no item (an empty one)
                // waits for `attach_rest`, as upstream's does.
                if let Some(holder) = self.block_item(first_new, parent) {
                    self.attach_comments(std::iter::once(el.id()), Some(holder));
                }
                self.add_text_boxes(el, parent, layer);
            }
            n if is_list_tag(n) => {
                self.add_list(el, parent, layer, false, 1, None);
            }
            "table" => {
                self.add_table(el, parent, layer, None, None);
            }
            n if is_transparent_container(n) => {
                // A section or an index (#823, docling#4750) is transparent;
                // a hidden one's content goes to the invisible layer (#753,
                // docling#4860).
                let layer = if section_is_hidden(el) {
                    Some(ContentLayer::Invisible)
                } else {
                    layer
                };
                self.add_children(el.children().filter(XmlNode::is_element), parent, layer)
            }
            "frame" => {
                let charts = self.add_charts(el, parent, layer);
                let images: Vec<XmlNode> = el
                    .descendants()
                    .filter(|n| n.has_tag_name("image") && !in_text_box(*n, el))
                    .collect();
                self.add_images(&images, parent, layer, charts > 0);
                self.add_text_boxes(el, parent, layer);
            }
            _ => {
                // Any other element with images (a `draw:custom-shape`, …).
                let images: Vec<XmlNode> = el
                    .descendants()
                    .filter(|n| n.has_tag_name("image"))
                    .collect();
                self.add_images(&images, parent, layer, false);
            }
        }
    }

    /// `_odf_list_item_text_runs` (`flatten_nested_text=False`): the item's
    /// direct paragraphs' runs; without any, and without a nested list, the
    /// item's whole text as one run.
    fn item_runs(&self, item: XmlNode) -> Vec<Run> {
        let mut runs = Vec::new();
        let mut has_nested = false;
        for child in item.children().filter(XmlNode::is_element) {
            match child.tag_name().name() {
                n if is_list_tag(n) => has_nested = true,
                "p" => collect_runs(child, self.styles, Fmt::default(), &mut runs),
                _ => {}
            }
        }
        let mut runs = normalize_runs(runs);
        if runs.is_empty() && !has_nested && item_headings(item).next().is_none() {
            let text = text_from_runs(&runs_of(item, self.styles));
            if !text.is_empty() {
                runs.push(Run {
                    text,
                    fmt: Fmt::default(),
                    href: None,
                });
            }
        }
        runs
    }

    /// One `list_item` of `group`: a single run is the item itself, several
    /// an empty item over an `inline` group of `text` runs; `text` is the
    /// fallback for an item without runs.
    fn add_list_item(
        &mut self,
        group: usize,
        layer: Option<ContentLayer>,
        runs: &[Run],
        text: &str,
        meta: ListMeta,
    ) -> usize {
        if runs.len() <= 1 {
            let (t, f, h) = match runs.first() {
                Some(r) => (r.text.clone(), formatting_of(r.fmt), r.href.clone()),
                None => (text.to_string(), None, None),
            };
            self.tree.add(
                Some(group),
                layer,
                TreeKind::Text {
                    label: "list_item".into(),
                    text: t,
                    orig: None,
                    formatting: f,
                    hyperlink: h,
                    level: None,
                    list: Some(meta),
                },
            )
        } else {
            let id = self.tree.add(
                Some(group),
                layer,
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
            let inline = self.inline_group(Some(id), layer);
            for run in runs {
                self.tree.add(
                    Some(inline),
                    layer,
                    text_kind(
                        "text",
                        &run.text,
                        formatting_of(run.fmt),
                        run.href.as_deref(),
                    ),
                );
            }
            id
        }
    }

    /// A list item's `<text:h>` children (#827) as headings under `parent`.
    fn add_item_headings(
        &mut self,
        item: XmlNode,
        parent: Option<usize>,
        layer: Option<ContentLayer>,
    ) {
        for h in item_headings(item) {
            let first_new = self.tree.items.len();
            self.add_heading(h, parent, layer);
            self.anchor_notes(h, first_new);
        }
    }

    /// `_add_odf_list`.
    fn add_list(
        &mut self,
        list: XmlNode,
        parent: Option<usize>,
        layer: Option<ContentLayer>,
        enumerated: bool,
        level: i64,
        continued: Option<ListState>,
    ) -> Option<ListState> {
        let styles = self.styles;
        if !list_has_renderable(list, styles) {
            return None;
        }
        let style_enum = level_is_enumerated(styles, list, level, enumerated);
        let should_continue = continued.is_some_and(|c| c.last_item.is_some())
            && list_starts_with_empty_nested(list, styles);
        if !should_continue && !list_has_direct_text(list, styles) {
            for item in list_items(list) {
                let (_, nested) = odf_item_content(item, styles);
                self.add_item_headings(item, parent, layer);
                for n in nested {
                    self.add_list(n, parent, layer, style_enum, level + 1, None);
                }
            }
            return None;
        }
        let (group, current_enum, mut counter, mut previous) = match (should_continue, continued) {
            (true, Some(c)) => (c.group, c.enumerated, c.counter, c.last_item),
            _ => (
                self.tree.add(
                    parent,
                    layer,
                    TreeKind::Group {
                        label: "list".into(),
                        name: "list".into(),
                    },
                ),
                style_enum,
                level_start(styles, list, level) - 1,
                None,
            ),
        };
        for item in list_items(list) {
            let (text, nested) = odf_item_content(item, styles);
            let has_content = item_has_content(item, styles);
            let nested: Vec<XmlNode> = nested
                .into_iter()
                .filter(|n| list_has_renderable(*n, styles))
                .collect();
            // The item's headings (#827) are `section_header`s of the list's
            // parent, created before the item.
            self.add_item_headings(item, parent, layer);
            if !has_content && nested.is_empty() {
                continue;
            }
            if !has_content {
                let nested_parent = previous.or(Some(group));
                for n in nested {
                    self.add_list(n, nested_parent, layer, style_enum, level + 1, None);
                }
                continue;
            }
            counter += 1;
            // `_odf_list_marker` with the level's number format and affixes
            // (#835): `a)`, `iii.`, `(3)`.
            let marker = if current_enum {
                odf_marker(counter, list_level(styles, attr(list, "style-name"), level))
            } else {
                String::new()
            };
            let first_new = self.tree.items.len();
            let runs = self.item_runs(item);
            let meta = ListMeta {
                enumerated: current_enum,
                marker,
            };
            let item_id = self.add_list_item(group, layer, &runs, &text, meta);
            self.anchor_notes(item, first_new);
            self.attach_comments(
                item.children()
                    .filter(|c| c.has_tag_name("p"))
                    .map(|c| c.id()),
                Some(item_id),
            );
            for p in item.children().filter(|c| c.has_tag_name("p")) {
                self.add_text_boxes(p, Some(item_id), layer);
            }
            previous = Some(item_id);
            for n in nested {
                self.add_list(n, Some(item_id), layer, style_enum, level + 1, None);
            }
        }
        Some(ListState {
            group,
            last_item: previous,
            enumerated: current_enum,
            counter,
        })
    }

    /// `_add_table_from_odf`: the table over `bounds` (the true data bounds
    /// when `None`), created before its cells; a rich cell's content walks
    /// into a `rich_cell_group_{table}_{col}_{row}` group under it.
    fn add_table(
        &mut self,
        table: XmlNode,
        parent: Option<usize>,
        layer: Option<ContentLayer>,
        bounds: Option<(usize, usize, usize, usize)>,
        prov: Option<TreeProv>,
    ) -> Option<usize> {
        let bounds = bounds.unwrap_or_else(|| data_bounds(&grid_of(table, None)));
        let (min_r, max_r, min_c, max_c) = bounds;
        let grid = grid_of(table, Some(bounds));
        let (height, width) = (max_r - min_r + 1, max_c - min_c + 1);
        if width == 0 || height == 0 {
            return None;
        }
        self.tables += 1;
        let kind = TreeKind::Table {
            table: Table {
                rows: vec![vec![String::new(); width]; height],
                cells: Some(Vec::new()),
                ..Table::default()
            },
            rich_cells: Vec::new(),
            captions: Vec::new(),
        };
        let table_id = match prov {
            Some(p) => self.tree.add_with_prov(parent, layer, kind, p),
            None => self.tree.add(parent, layer, kind),
        };
        let mut cells: Vec<TableCell> = Vec::new();
        let mut rich: Vec<(usize, usize, usize)> = Vec::new();
        let mut positions: Vec<(&(usize, usize), &XmlNode)> = grid
            .anchors
            .iter()
            .filter(|((r, c), _)| (min_r..=max_r).contains(r) && (min_c..=max_c).contains(c))
            .collect();
        positions.sort_by_key(|(pos, _)| **pos);
        for (&(r, c), cell) in positions {
            let (rs, cs) = (
                span(*cell, "number-rows-spanned"),
                span(*cell, "number-columns-spanned"),
            );
            let (ar, ac) = (r - min_r, c - min_c);
            let text = cell_text(*cell);
            if is_rich_cell(*cell, self.styles) {
                // `len(doc.tables) - 1` at this moment: a nested table walked
                // for an earlier rich cell of this table counts.
                let table_ix = self.tables - 1;
                let group = self.tree.add(
                    Some(table_id),
                    layer,
                    TreeKind::Group {
                        label: "unspecified".into(),
                        name: format!("rich_cell_group_{table_ix}_{ac}_{ar}"),
                    },
                );
                for child in cell.children().filter(XmlNode::is_element) {
                    self.add_child(child, Some(group), layer);
                }
                rich.push((ar, ac, group));
            } else {
                // A comment in a plain cell: its group under the table, no
                // back-reference (the cell is no item).
                let anchors: Vec<roxmltree::NodeId> = cell
                    .descendants()
                    .filter(|n| n.has_tag_name("p") || n.has_tag_name("h"))
                    .map(|n| n.id())
                    .collect();
                for t in 0..self.threads.len() {
                    let thread = &self.threads[t];
                    if !thread.attached && thread.anchor.is_some_and(|a| anchors.contains(&a)) {
                        self.threads[t].attached = true;
                        let group = self.tree.add(
                            Some(table_id),
                            Some(ContentLayer::Notes),
                            TreeKind::Group {
                                label: "comment_section".into(),
                                name: format!("comment-{}", t + 1),
                            },
                        );
                        for c in self.threads[t].comments.clone() {
                            let text = self.annotations[c].text.clone();
                            self.tree.add(
                                Some(group),
                                Some(ContentLayer::Notes),
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
                    }
                }
            }
            cells.push(TableCell {
                text,
                bbox: None,
                start_row: ar,
                start_col: ac,
                row_span: rs,
                col_span: cs,
                column_header: ar == 0,
                row_header: false,
                row_section: false,
            });
        }
        if let TreeKind::Table {
            table, rich_cells, ..
        } = &mut self.tree.items[table_id].kind
        {
            for cell in &cells {
                if let Some(slot) = table
                    .rows
                    .get_mut(cell.start_row)
                    .and_then(|r| r.get_mut(cell.start_col))
                {
                    *slot = cell.text.clone();
                }
            }
            table.cells = Some(cells);
            *rich_cells = rich;
        }
        Some(table_id)
    }

    // ------------------------------------------------------------ presentation

    /// `_walk_slide`.
    fn walk_slide(&mut self, page: XmlNode, slide: usize) {
        let mut seen_text = false;
        for el in page.children().filter(XmlNode::is_element) {
            let tag = el.tag_name().name();
            if tag == "notes" {
                self.walk_slide_notes(el, slide);
                continue;
            }
            if tag == "par" {
                continue;
            }
            let has_text = element_has_text(el);
            let is_title = is_slide_title_element(el, !seen_text);
            if has_text {
                seen_text = true;
            }
            if tag == "frame" {
                self.walk_slide_frame(el, slide, is_title);
            } else {
                self.walk_textbox_children(
                    el.children().filter(XmlNode::is_element),
                    slide,
                    is_title,
                    None,
                );
            }
        }
    }

    /// `_walk_slide_notes` (#862, docling#4869, 2.141): a slide's speaker
    /// notes go to the notes layer under the slide's group, as in the
    /// PowerPoint backend. A notes page also shows the slide's thumbnail and
    /// can show its number, date, header and footer, which are not notes.
    fn walk_slide_notes(&mut self, notes: XmlNode, slide: usize) {
        for el in notes.children().filter(XmlNode::is_element) {
            if attr(el, "class").is_some_and(|c| NOTES_PAGE_FIELD_CLASSES.contains(&c)) {
                continue;
            }
            if el.has_tag_name("frame") {
                let boxes: Vec<XmlNode> = el
                    .descendants()
                    .filter(|n| n.has_tag_name("text-box"))
                    .collect();
                for tb in boxes {
                    self.walk_textbox_children(
                        tb.children().filter(XmlNode::is_element),
                        slide,
                        false,
                        Some(ContentLayer::Notes),
                    );
                }
            } else {
                self.walk_textbox_children(
                    el.children().filter(XmlNode::is_element),
                    slide,
                    false,
                    Some(ContentLayer::Notes),
                );
            }
        }
    }

    /// `_walk_slide_frame`: charts, then tables, then images, then text boxes.
    fn walk_slide_frame(&mut self, frame: XmlNode, slide: usize, is_title: bool) {
        let charts = self.add_charts(frame, Some(slide), None);
        let tables: Vec<XmlNode> = frame
            .descendants()
            .filter(|n| n.has_tag_name("table") && !n.ancestors().any(|a| a.has_tag_name("object")))
            .collect();
        for tbl in tables {
            self.add_table(tbl, Some(slide), None, None, None);
        }
        let images: Vec<XmlNode> = frame
            .descendants()
            .filter(|n| n.has_tag_name("image"))
            .collect();
        self.add_images(&images, Some(slide), None, charts > 0);
        let boxes: Vec<XmlNode> = frame
            .descendants()
            .filter(|n| n.has_tag_name("text-box"))
            .collect();
        for tb in boxes {
            self.walk_textbox_children(
                tb.children().filter(XmlNode::is_element),
                slide,
                is_title,
                None,
            );
        }
    }

    /// `_walk_textbox_children`: headings, paragraphs as `title` (a title
    /// element) or `text`, lists continuing across siblings — on `layer`
    /// (the speaker notes' notes layer, else the body).
    fn walk_textbox_children<'a, 'i: 'a>(
        &mut self,
        els: impl Iterator<Item = XmlNode<'a, 'i>>,
        slide: usize,
        is_title: bool,
        layer: Option<ContentLayer>,
    ) {
        let mut prev: Option<ListState> = None;
        for el in els {
            match el.tag_name().name() {
                "h" => {
                    prev = None;
                    self.add_heading(el, Some(slide), layer);
                }
                "p" => {
                    prev = None;
                    let runs = runs_of(el, self.styles);
                    self.add_text_runs(
                        &runs,
                        if is_title { "title" } else { "text" },
                        Some(slide),
                        layer,
                    );
                }
                n if is_list_tag(n) => {
                    prev = self.add_list(el, Some(slide), layer, false, 1, prev);
                }
                _ => {}
            }
        }
    }

    // ------------------------------------------------------------ spreadsheet

    /// `_convert_sheet_table` + `_find_images_in_sheet`: the sheet's
    /// disconnected data regions (4-neighbour flood fill, no gap) as tables
    /// with their cell-range provenance, then its images.
    fn convert_sheet(
        &mut self,
        table: XmlNode,
        group: usize,
        page_no: usize,
        layer: Option<ContentLayer>,
    ) {
        let grid = grid_of(table, None);
        let (min_r, max_r, min_c, max_c) = data_bounds(&grid);
        let has_any = !grid.anchors.is_empty() || !grid.covered.is_empty();
        if has_any {
            let has_content = |r: usize, c: usize| -> bool {
                (min_r..=max_r).contains(&r)
                    && (min_c..=max_c).contains(&c)
                    && (grid.anchors.contains_key(&(r, c)) || grid.covered.contains(&(r, c)))
            };
            let mut order: Vec<(usize, usize)> = grid
                .anchors
                .keys()
                .chain(grid.covered.iter())
                .copied()
                .collect();
            order.sort_unstable();
            let mut visited: HashSet<(usize, usize)> = HashSet::new();
            for (ri, ci) in order {
                if visited.contains(&(ri, ci)) || !has_content(ri, ci) {
                    continue;
                }
                let mut region: HashSet<(usize, usize)> = HashSet::new();
                let mut queue: VecDeque<(usize, usize)> = VecDeque::new();
                queue.push_back((ri, ci));
                region.insert((ri, ci));
                let (mut r0, mut r1, mut c0, mut c1) = (ri, ri, ci, ci);
                while let Some((r, c)) = queue.pop_front() {
                    r0 = r0.min(r);
                    r1 = r1.max(r);
                    c0 = c0.min(c);
                    c1 = c1.max(c);
                    for (dr, dc) in [(0i64, 1i64), (0, -1), (1, 0), (-1, 0)] {
                        let (nr, nc) = (r as i64 + dr, c as i64 + dc);
                        if nr < 0 || nc < 0 {
                            continue;
                        }
                        let key = (nr as usize, nc as usize);
                        if !region.contains(&key) && has_content(key.0, key.1) {
                            region.insert(key);
                            queue.push_back(key);
                        }
                    }
                }
                visited.extend(region.iter().copied());
                self.add_table(
                    table,
                    Some(group),
                    layer,
                    Some((r0, r1, c0, c1)),
                    Some(TreeProv {
                        page_no,
                        bbox: [c0 as f64, r0 as f64, (c1 + 1) as f64, (r1 + 1) as f64],
                        bottom_left: false,
                        charspan: [0, 0],
                    }),
                );
            }
        } else if !cell_has_content_at_a1(table) {
            // No data at all: no tables.
        }
        // `_find_images_in_sheet`: every picture anchored to the sheet itself
        // (in `<table:shapes>`) with a default (0, 0, 1, 1) box. A picture
        // anchored to a cell is added with its rich cell, once (#758,
        // docling#4678).
        let images: Vec<XmlNode> = table
            .children()
            .filter(|n| n.has_tag_name("shapes"))
            .flat_map(|shapes| shapes.descendants())
            .filter(|n| n.has_tag_name("image"))
            .collect();
        for img in images {
            let href = image_href(img).unwrap_or("");
            let Some(image) = self.image_payload(img, href) else {
                continue;
            };
            self.tree.add_with_prov(
                Some(group),
                layer,
                TreeKind::Picture {
                    captions: Vec::new(),
                    image: Some(image),
                    extra_classes: Vec::new(),
                    classification: None,
                    description: None,
                    confidence: None,
                    chart: None,
                    dpi: None,
                },
                TreeProv {
                    page_no,
                    bbox: [0.0, 0.0, 1.0, 1.0],
                    bottom_left: false,
                    charspan: [0, 0],
                },
            );
        }
    }
}

fn cell_has_content_at_a1(table: XmlNode) -> bool {
    table
        .descendants()
        .find(|n| n.has_tag_name("table-cell"))
        .is_some_and(cell_has_content)
}

// `ods_cell_text` keeps the spreadsheet cell reading the flat walk uses; the
// tree's `cell_text` follows docling's `_odf_cell_text`, which agrees on the
// corpus. Kept referenced so the two stay side by side.
#[allow(dead_code)]
fn _ods_cell_text_alias(cell: XmlNode) -> String {
    ods_cell_text(cell)
}

#[cfg(test)]
mod tests {
    use crate::backend::DeclarativeBackend;
    use crate::{InputFormat, SourceDocument};
    use docling_core::tree::{ItemTree, TreeItem, TreeKind};
    use docling_core::{DoclingDocument, Node};

    const NS: &str = r#"xmlns:office="o" xmlns:text="x" xmlns:table="t" xmlns:style="s" xmlns:fo="f" xmlns:draw="d" xmlns:presentation="p""#;

    fn convert(xml: &str, fmt: InputFormat) -> DoclingDocument {
        let src = SourceDocument::from_bytes("t.fodt", fmt, xml.as_bytes().to_vec());
        super::super::odf::OdfBackend.convert(&src).unwrap()
    }

    fn label(it: &TreeItem) -> String {
        match &it.kind {
            TreeKind::Text { label, .. } => label.clone(),
            TreeKind::Code { .. } => "code".into(),
            TreeKind::Group { label, name } => format!("{label}:{name}"),
            TreeKind::Table { .. } => "table".into(),
            TreeKind::Picture { .. } => "picture".into(),
            TreeKind::FieldRegion { .. } => "field_region".into(),
            TreeKind::KeyValueGraph { .. } => "key_value_region".into(),
        }
    }

    fn text(it: &TreeItem) -> &str {
        match &it.kind {
            TreeKind::Text { text, .. } => text,
            _ => "",
        }
    }

    /// An ODT body: a heading of mixed runs is an empty `section_header` over
    /// an inline group, a bold span splits a paragraph into an inline group,
    /// a numbered list carries `1.` markers, and a two-paragraph cell is a
    /// `RichTableCell` whose paragraphs live in a `rich_cell_group_0_1_0`
    /// group under the table (a typed one-paragraph cell stays plain).
    #[test]
    fn text_document_tree_has_upstreams_shape() {
        let xml = format!(
            r#"<office:document {NS}>
            <office:automatic-styles>
              <style:style style:name="B" style:family="text"><style:text-properties fo:font-weight="bold"/></style:style>
              <text:list-style style:name="L1"><text:list-level-style-number text:level="1" style:num-suffix=")" style:num-format="1"/></text:list-style>
            </office:automatic-styles>
            <office:body><office:text>
              <text:h text:outline-level="2">Deep <text:span text:style-name="B">bold</text:span></text:h>
              <text:p>Plain para.</text:p>
              <text:p>Mixed <text:span text:style-name="B">bold</text:span> tail</text:p>
              <text:list text:style-name="L1"><text:list-item><text:p>one</text:p></text:list-item><text:list-item><text:p>two</text:p></text:list-item></text:list>
              <table:table><table:table-row>
                <table:table-cell office:value-type="string"><text:p>plain</text:p></table:table-cell>
                <table:table-cell><text:p>a</text:p><text:p>b</text:p></table:table-cell>
              </table:table-row></table:table>
            </office:text></office:body></office:document>"#
        );
        let doc = convert(&xml, InputFormat::Odt);
        let t: &ItemTree = doc.tree.as_ref().expect("tree");
        let labels: Vec<String> = t.items.iter().map(label).collect();
        assert_eq!(
            labels,
            [
                "section_header",
                "inline:group",
                "text",
                "text", // heading + runs
                "text", // plain para
                "inline:group",
                "text",
                "text",
                "text", // mixed para
                "list:list",
                "list_item",
                "list_item",
                "table",
                "unspecified:rich_cell_group_0_1_0",
                "text",
                "text",
            ]
        );
        assert_eq!(text(&t.items[0]), "");
        assert!(matches!(
            &t.items[0].kind,
            TreeKind::Text { level: Some(2), .. }
        ));
        assert!(matches!(&t.items[3].kind, TreeKind::Text { formatting: Some(f), .. } if f.bold));
        assert!(
            matches!(&t.items[10].kind, TreeKind::Text { list: Some(l), .. } if l.enumerated && l.marker == "1)")
        );
        assert!(
            matches!(&t.items[11].kind, TreeKind::Text { list: Some(l), .. } if l.marker == "2)")
        );
        let TreeKind::Table {
            table, rich_cells, ..
        } = &t.items[12].kind
        else {
            panic!()
        };
        assert_eq!(rich_cells, &[(0, 1, 13)]);
        let cells = table.cells.as_ref().unwrap();
        assert_eq!(
            cells.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            ["plain", "a\nb"]
        );
        assert_eq!(t.items[13].parent, Some(12));
        assert_eq!(t.items[14].parent, Some(13));
    }

    /// #823: an index body's title, entries and the blocks inside it are
    /// items in document order, between the surrounding paragraphs, with
    /// no group of their own — docling#4750's shape.
    #[test]
    fn index_body_items_are_body_items() {
        let xml = format!(
            r#"<office:document {NS}><office:body><office:text>
              <text:p>Intro.</text:p>
              <text:table-of-content text:name="TOC">
                <text:table-of-content-source><text:index-title-template>T</text:index-title-template></text:table-of-content-source>
                <text:index-body>
                  <text:index-title><text:p>Contents</text:p></text:index-title>
                  <text:p>Chapter One</text:p>
                  <text:h text:outline-level="1">Chapter One</text:h>
                  <table:table><table:table-row><table:table-cell office:value-type="string"><text:p>Cell</text:p></table:table-cell></table:table-row></table:table>
                </text:index-body>
              </text:table-of-content>
              <text:p>Outro.</text:p>
            </office:text></office:body></office:document>"#
        );
        let doc = convert(&xml, InputFormat::Odt);
        let t: &ItemTree = doc.tree.as_ref().expect("tree");
        let items: Vec<(String, String)> = t
            .items
            .iter()
            .map(|it| (label(it), text(it).to_string()))
            .collect();
        assert_eq!(
            items,
            [
                ("text".to_string(), "Intro.".to_string()),
                ("text".to_string(), "Contents".to_string()),
                ("text".to_string(), "Chapter One".to_string()),
                ("section_header".to_string(), "Chapter One".to_string()),
                ("table".to_string(), String::new()),
                ("text".to_string(), "Outro.".to_string()),
            ]
        );
        assert!(t.items.iter().all(|it| it.parent.is_none()));
    }

    /// #826: style-numbered paragraphs are `list` groups of `list_item`s —
    /// one group per run of the same list style, markers along the run — the
    /// DOCX backend's shape for the same document.
    #[test]
    fn style_list_name_paragraphs_form_list_groups() {
        let xml = format!(
            r#"<office:document {NS}>
            <office:styles>
              <text:list-style style:name="LB"><text:list-level-style-bullet text:level="1" text:bullet-char="•"/></text:list-style>
              <text:list-style style:name="LN"><text:list-level-style-number text:level="1" style:num-format="1"/></text:list-style>
              <style:style style:name="Bul" style:family="paragraph" style:list-style-name="LB"/>
              <style:style style:name="Num" style:family="paragraph" style:list-style-name="LN"/>
            </office:styles>
            <office:body><office:text>
              <text:p>Intro:</text:p>
              <text:p text:style-name="Bul">one</text:p>
              <text:p text:style-name="Bul">two</text:p>
              <text:p text:style-name="Num">three</text:p>
              <text:p text:style-name="Num">four</text:p>
            </office:text></office:body></office:document>"#
        );
        let doc = convert(&xml, InputFormat::Odt);
        let t: &ItemTree = doc.tree.as_ref().expect("tree");
        let labels: Vec<String> = t.items.iter().map(label).collect();
        assert_eq!(
            labels,
            [
                "text",
                "list:list",
                "list_item",
                "list_item",
                "list:list",
                "list_item",
                "list_item"
            ]
        );
        assert_eq!(t.items[2].parent, Some(1));
        assert_eq!(t.items[5].parent, Some(4));
        assert!(
            matches!(&t.items[2].kind, TreeKind::Text { list: Some(l), .. } if !l.enumerated && l.marker.is_empty())
        );
        assert!(
            matches!(&t.items[5].kind, TreeKind::Text { list: Some(l), .. } if l.enumerated && l.marker == "1.")
        );
        assert!(
            matches!(&t.items[6].kind, TreeKind::Text { list: Some(l), .. } if l.marker == "2.")
        );
    }

    /// #827: headings inside list items are `section_header`s with their
    /// outline level, body-level siblings like every ODT heading; a list of
    /// headings alone opens no `list` group.
    #[test]
    fn headings_inside_list_items_are_section_headers() {
        let xml = format!(
            r#"<office:document {NS}><office:body><office:text>
              <text:list><text:list-item>
                <text:h text:outline-level="1">Context</text:h>
                <text:list><text:list-item><text:h text:outline-level="2">Background</text:h></text:list-item></text:list>
              </text:list-item></text:list>
              <text:p>Body.</text:p>
              <text:list><text:list-item><text:list><text:list-item><text:h text:outline-level="2">Target</text:h></text:list-item></text:list></text:list-item></text:list>
            </office:text></office:body></office:document>"#
        );
        let doc = convert(&xml, InputFormat::Odt);
        let t: &ItemTree = doc.tree.as_ref().expect("tree");
        let items: Vec<(String, String, Option<u8>)> = t
            .items
            .iter()
            .map(|it| {
                let level = match &it.kind {
                    TreeKind::Text { level, .. } => *level,
                    _ => None,
                };
                (label(it), text(it).to_string(), level)
            })
            .collect();
        assert_eq!(
            items,
            [
                ("section_header".to_string(), "Context".to_string(), Some(1)),
                (
                    "section_header".to_string(),
                    "Background".to_string(),
                    Some(2)
                ),
                ("text".to_string(), "Body.".to_string(), None),
                ("section_header".to_string(), "Target".to_string(), Some(2)),
            ]
        );
        assert!(t.items.iter().all(|it| it.parent.is_none()));
    }

    /// #829: each comment thread is a notes-layer `comment_section` group
    /// right after the item holding its first comment, that item's
    /// `comments` pointing at it; a reply's text joins the thread; a
    /// comment in a plain cell hangs on the table, one on an empty
    /// paragraph on the root at the end — docling#4436's shape.
    #[test]
    fn annotations_become_comment_sections() {
        let xml = format!(
            r#"<office:document {NS} xmlns:dc="dc" xmlns:loext="lo"><office:body><office:text>
              <text:h text:outline-level="1">Budget<office:annotation office:name="c1"><dc:creator>Alpha</dc:creator><text:p>Rename</text:p></office:annotation></text:h>
              <text:p>Total <office:annotation loext:parent-name="c1"><dc:creator>Bravo</dc:creator><text:p>Agreed</text:p></office:annotation>500.</text:p>
              <text:list><text:list-item><text:p>Item<office:annotation><text:p>Scope</text:p></office:annotation></text:p></text:list-item></text:list>
              <table:table><table:table-row><table:table-cell office:value-type="string"><text:p>Cell<office:annotation><text:p>In cell</text:p></office:annotation></text:p></table:table-cell></table:table-row></table:table>
              <text:p><office:annotation><text:p>Orphan</text:p></office:annotation></text:p>
            </office:text></office:body></office:document>"#
        );
        let doc = convert(&xml, InputFormat::Odt);
        let t: &ItemTree = doc.tree.as_ref().expect("tree");
        let items: Vec<(String, String, Option<usize>)> = t
            .items
            .iter()
            .map(|it| (label(it), text(it).to_string(), it.parent))
            .collect();
        assert_eq!(
            items,
            [
                ("section_header".to_string(), "Budget".to_string(), None),
                (
                    "comment_section:comment-1".to_string(),
                    String::new(),
                    Some(0)
                ),
                (
                    "text".to_string(),
                    "[author: Alpha]: Rename".to_string(),
                    Some(1)
                ),
                (
                    "text".to_string(),
                    "[author: Bravo]: Agreed".to_string(),
                    Some(1)
                ),
                ("text".to_string(), "Total 500.".to_string(), None),
                ("list:list".to_string(), String::new(), None),
                ("list_item".to_string(), "Item".to_string(), Some(5)),
                (
                    "comment_section:comment-2".to_string(),
                    String::new(),
                    Some(6)
                ),
                ("text".to_string(), "Scope".to_string(), Some(7)),
                ("table".to_string(), String::new(), None),
                (
                    "comment_section:comment-3".to_string(),
                    String::new(),
                    Some(9)
                ),
                ("text".to_string(), "In cell".to_string(), Some(10)),
                ("comment_section:comment-4".to_string(), String::new(), None),
                ("text".to_string(), "Orphan".to_string(), Some(12)),
            ]
        );
        assert_eq!(t.items[0].comments, [1]);
        assert_eq!(t.items[6].comments, [7]);
        assert!(t.items[9].comments.is_empty());
        assert_eq!(t.items[1].layer, Some(docling_core::ContentLayer::Notes));
        assert_eq!(t.items[2].layer, Some(docling_core::ContentLayer::Notes));
        assert_eq!(
            doc.export_to_markdown(),
            "## Budget\n\nTotal 500.\n\n- Item\n\n| Cell   |\n|--------|\n"
        );
        let json = doc.export_to_json();
        assert!(json.contains("\"comment_section\""), "{json}");
    }

    /// #830: a furniture-layer `page header` / `page footer` section per
    /// master page part after the body, its paragraphs and tables inside —
    /// the DOCX backend's tree shape.
    #[test]
    fn master_page_parts_are_furniture_sections() {
        let xml = format!(
            r#"<office:document {NS}>
            <office:master-styles><style:master-page style:name="Standard">
              <style:header><text:p>Head</text:p><table:table><table:table-row><table:table-cell office:value-type="string"><text:p>V</text:p></table:table-cell></table:table-row></table:table></style:header>
              <style:footer><text:p>Foot</text:p></style:footer>
            </style:master-page></office:master-styles>
            <office:body><office:text><text:p>Body.</text:p></office:text></office:body></office:document>"#
        );
        let doc = convert(&xml, InputFormat::Odt);
        let t: &ItemTree = doc.tree.as_ref().expect("tree");
        let items: Vec<(
            String,
            String,
            Option<usize>,
            Option<docling_core::ContentLayer>,
        )> = t
            .items
            .iter()
            .map(|it| (label(it), text(it).to_string(), it.parent, it.layer))
            .collect();
        let f = Some(docling_core::ContentLayer::Furniture);
        assert_eq!(
            items,
            [
                ("text".to_string(), "Body.".to_string(), None, None),
                ("section:page header".to_string(), String::new(), None, f),
                ("text".to_string(), "Head".to_string(), Some(1), f),
                ("table".to_string(), String::new(), Some(1), f),
                ("section:page footer".to_string(), String::new(), None, f),
                ("text".to_string(), "Foot".to_string(), Some(4), f),
            ]
        );
        assert_eq!(doc.export_to_markdown(), "Body.\n");
    }

    /// #831: a text box is a `textbox` section group after the anchoring
    /// paragraph's item, its paragraphs, list and table inside — the DOCX
    /// backend's shape; the anchoring paragraph keeps its own text only.
    #[test]
    fn text_boxes_are_textbox_sections() {
        let xml = format!(
            r#"<office:document {NS}><office:body><office:text>
              <text:p>Anchor.<draw:frame><draw:text-box><text:p>First.</text:p><text:list><text:list-item><text:p>item</text:p></text:list-item></text:list><table:table><table:table-row><table:table-cell office:value-type="string"><text:p>Cell</text:p></table:table-cell></table:table-row></table:table></draw:text-box></draw:frame></text:p>
              <text:p>After.</text:p>
            </office:text></office:body></office:document>"#
        );
        let doc = convert(&xml, InputFormat::Odt);
        let t: &ItemTree = doc.tree.as_ref().expect("tree");
        let items: Vec<(String, String, Option<usize>)> = t
            .items
            .iter()
            .map(|it| (label(it), text(it).to_string(), it.parent))
            .collect();
        assert_eq!(
            items,
            [
                ("text".to_string(), "Anchor.".to_string(), None),
                ("section:textbox".to_string(), String::new(), None),
                ("text".to_string(), "First.".to_string(), Some(1)),
                ("list:list".to_string(), String::new(), Some(1)),
                ("list_item".to_string(), "item".to_string(), Some(3)),
                ("table".to_string(), String::new(), Some(1)),
                ("text".to_string(), "After.".to_string(), None),
            ]
        );
    }

    /// A presentation: a `chapter` group per slide named `slide-{0-based}`,
    /// the title element's paragraph a `title`, a slide without a visible
    /// title getting its name as one.
    #[test]
    fn presentation_tree_groups_slides() {
        let xml = format!(
            r#"<office:document {NS}><office:body><office:presentation>
              <draw:page draw:name="First"><draw:frame presentation:class="title"><draw:text-box><text:p>Hello</text:p></draw:text-box></draw:frame>
                <draw:frame><draw:text-box><text:p>Body</text:p></draw:text-box></draw:frame></draw:page>
              <draw:page draw:name="Second"><draw:frame><draw:text-box><text:p>Only body</text:p></draw:text-box></draw:frame></draw:page>
            </office:presentation></office:body></office:document>"#
        );
        let doc = convert(&xml, InputFormat::Odp);
        let t = doc.tree.as_ref().expect("tree");
        let labels: Vec<String> = t.items.iter().map(label).collect();
        assert_eq!(
            labels,
            [
                "chapter:slide-0",
                "title",
                "text",
                "chapter:slide-1",
                "title",
                "text"
            ]
        );
        assert_eq!(text(&t.items[1]), "Hello");
        assert_eq!(
            text(&t.items[4]),
            "Second",
            "the slide name stands in for a title"
        );
        assert_eq!(t.body, vec![0, 3]);
    }

    /// A spreadsheet: a `section` group `sheet: {name}` per sheet, each
    /// disconnected data region a table with its cell-range provenance
    /// (top-left origin, `charspan [0, 0]`), typed numbers as their
    /// `office:value`, and the sheet sized by its items' extent as a page.
    #[test]
    fn spreadsheet_tree_has_regions_with_provenance() {
        let xml = format!(
            r#"<office:document {NS}><office:body><office:spreadsheet>
              <table:table table:name="Data">
                <table:table-row><table:table-cell/><table:table-cell office:value-type="string"><text:p>Title</text:p></table:table-cell></table:table-row>
                <table:table-row><table:table-cell table:number-columns-repeated="2"/></table:table-row>
                <table:table-row><table:table-cell/><table:table-cell office:value-type="string"><text:p>Year</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="120"><text:p>120</text:p></table:table-cell></table:table-row>
              </table:table>
            </office:spreadsheet></office:body></office:document>"#
        );
        let doc = convert(&xml, InputFormat::Ods);
        let t = doc.tree.as_ref().expect("tree");
        let labels: Vec<String> = t.items.iter().map(label).collect();
        assert_eq!(labels, ["section:sheet: Data", "table", "table"]);
        let prov = |i: usize| {
            t.items[i]
                .prov
                .as_ref()
                .map(|p| (p.bbox, p.bottom_left, p.charspan))
        };
        assert_eq!(prov(1), Some(([1.0, 0.0, 2.0, 1.0], false, [0, 0])));
        assert_eq!(prov(2), Some(([1.0, 2.0, 3.0, 3.0], false, [0, 0])));
        let TreeKind::Table { table, .. } = &t.items[2].kind else {
            panic!()
        };
        assert_eq!(
            table
                .cells
                .as_ref()
                .unwrap()
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>(),
            ["Year", "120"]
        );
        let page = doc.nodes.iter().find_map(|n| match n {
            Node::PageInfo {
                page_no,
                width,
                height,
            } => Some((*page_no, *width, *height)),
            _ => None,
        });
        assert_eq!(page, Some((1, 2.0, 3.0)), "right − left, bottom − top");
    }
}
