//! docling's **item tree**, for a backend that knows the exact shape upstream
//! gives a document and wants the JSON export to reproduce it.
//!
//! [`DoclingDocument::nodes`](crate::DoclingDocument::nodes) is a flat,
//! reading-order stream tuned for Markdown / DocLang / LaTeX; the JSON export
//! rebuilds docling's parent/child structure from it with generic rules (runs
//! of list items become list groups, a heading is a flat sibling of the text
//! that follows it). Upstream's backends do not all agree on that structure:
//! the HTML backend nests everything after a heading *under* the heading,
//! splits a paragraph of mixed formatting into an `inline` group of one text
//! item per formatting run, parents a rich table cell's content to a group
//! under the table, keeps site chrome on the `furniture` layer… and numbers
//! every item in the order it *creates* them. A backend that ports those
//! rules call-for-call (HTML's `html_tree.rs`, DOCX's `docx_tree.rs`) records
//! the result here — an arena of items in
//! creation order, each with its parent and children — and the JSON export
//! ([`DoclingDocument::export_to_json`](crate::DoclingDocument::export_to_json))
//! serializes this tree instead of deriving one from the nodes. Every other
//! serializer keeps reading the flat nodes, so their output is unaffected.

use crate::{ContentLayer, FieldItem, PictureImage, Script, Table};

/// docling-core's `Formatting`: the inline styles an item carries in JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Formatting {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub script: Script,
}

/// A `list_item`'s docling fields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListMeta {
    pub enumerated: bool,
    /// docling's `marker` — the HTML backend writes `""` unless an ordered
    /// list carries an explicit `start`, then `"{n}."`.
    pub marker: String,
}

/// What an item in the tree is. Mirrors the docling-core item classes the
/// JSON `texts` / `groups` / `tables` / `pictures` / `field_regions` buckets
/// hold.
#[derive(Debug, Clone, PartialEq)]
pub enum TreeKind {
    /// A `TextItem` / `TitleItem` / `SectionHeaderItem` / `ListItem`, told
    /// apart by `label` (`text`, `title`, `section_header`, `list_item`,
    /// `caption`, `checkbox_selected`, `checkbox_unselected`, …).
    Text {
        label: String,
        text: String,
        /// docling's `orig` when it differs from `text` (the heading text
        /// before unicode cleanup, say); `None` = same as `text`.
        orig: Option<String>,
        formatting: Option<Formatting>,
        hyperlink: Option<String>,
        /// `section_header` only: docling's heading level.
        level: Option<u8>,
        /// `list_item` only.
        list: Option<ListMeta>,
    },
    /// A `CodeItem`.
    Code {
        text: String,
        orig: Option<String>,
        /// The language hint (a highlighter class token such as `python`),
        /// mapped onto docling's `CodeLanguageLabel` at export; `None` →
        /// `unknown`.
        language: Option<String>,
        formatting: Option<Formatting>,
        hyperlink: Option<String>,
        /// Caption text items in the tree — docling's `add_code(caption=…)`
        /// (an AsciiDoc block title in front of a literal block).
        captions: Vec<usize>,
    },
    /// A `GroupItem`: `label` is docling's `GroupLabel` value (`inline`,
    /// `list`, `section`, `unspecified`, …), `name` its name (`group`, `list`,
    /// `ordered list`, `header-2`, `rich_cell_group_1_0_3`, …).
    Group { label: String, name: String },
    /// A `TableItem`. `rich_cells` marks the cells docling serialized as a
    /// `RichTableCell`: `(row, col)` grid anchor → the group item (a child of
    /// the table) that holds the cell's content. `captions` are caption text
    /// items in the tree.
    Table {
        table: Table,
        rich_cells: Vec<(usize, usize, usize)>,
        captions: Vec<usize>,
    },
    /// A `PictureItem`, its caption text items and optional payload.
    /// `classification` is a `PictureClassificationLabel` value written as
    /// the picture's `meta.classification` (an HTML `<stamp>` / `<signature>`).
    Picture {
        captions: Vec<usize>,
        image: Option<PictureImage>,
        classification: Option<String>,
        /// Further predictions after `classification`, in order: the other
        /// plot types of a PPTX combination chart (docling#4773).
        extra_classes: Vec<String>,
        /// The picture-OCR enrichment's text (#645), serialized like the flat
        /// node's: `meta.description` + the `description` annotation.
        description: Option<crate::PictureDescription>,
        /// The prediction's `confidence`, when the backend writes one: the
        /// DocLang deserializer stamps `1.0`; the office and HTML backends
        /// leave it out (`None`).
        confidence: Option<f64>,
        /// A native chart's data grid (docling's `meta.tabular_chart.chart_data`,
        /// the series reconstructed as a `TableData`), for a DOCX chart drawing.
        chart: Option<Table>,
        /// The `ImageRef.dpi` docling writes for `image` when the backend
        /// read one from the file (python-pptx's `Image.dpi`: PIL's `dpi`
        /// info, rounded, 72 when absent or out of 1–2048); `None` → 72,
        /// which is what upstream's other office backends pass.
        dpi: Option<u32>,
    },
    /// A form key-value region (`field_regions` / `field_items`).
    FieldRegion { items: Vec<FieldItem> },
    /// A `KeyValueItem` (`key_value_items`): docling's `GraphData` of key and
    /// value cells and their links, written verbatim.
    KeyValueGraph {
        cells: Vec<crate::GraphCell>,
        links: Vec<crate::GraphLink>,
    },
}

/// docling's `ProvenanceItem` for a tree item, written verbatim: the
/// backend's own geometry in the page's units — a PPTX shape's EMU box,
/// whose `pages` entry is the slide size in EMU — rather than the 0–511
/// DocLang grid the flat [`Node::Located`](crate::Node::Located) carries
/// (which cannot round-trip those integers).
#[derive(Debug, Clone, PartialEq)]
pub struct TreeProv {
    /// 1-based page (slide) number.
    pub page_no: usize,
    /// `[l, t, r, b]`, exactly as docling computed them.
    pub bbox: [f64; 4],
    /// docling's `coord_origin` tag. The office backends tag their
    /// top-left-based boxes `TOPLEFT` (the PPTX backend since docling#4294 —
    /// it used to tag them `BOTTOMLEFT`, which read the tuple as `(l, b, r,
    /// t)` and swapped the vertical edges); a backend that really works in a
    /// bottom-left space sets this.
    pub bottom_left: bool,
    /// `[0, len(text)]` in characters for a text item, `[0, 0]` for a table
    /// or picture.
    pub charspan: [usize; 2],
}

/// docling's `TrackSource` — where in a time-based track (a WebVTT cue) a
/// text item came from. Written as the item's `source: [{"kind": "track", …}]`.
#[derive(Debug, Clone, PartialEq)]
pub struct TreeTrack {
    /// The cue's start offset in seconds (docling's `WebVTTTimestamp.seconds`:
    /// `h*3600 + m*60 + s + millis/1000.0`, so the float is bit-identical).
    pub start_time: f64,
    /// The cue's end offset in seconds.
    pub end_time: f64,
    /// The cue identifier line, when the cue has one.
    pub identifier: Option<String>,
    /// The `<v …>` voice annotation the text sits in, when any.
    pub voice: Option<String>,
}

/// One item of an [`ItemTree`].
#[derive(Debug, Clone, PartialEq)]
pub struct TreeItem {
    /// The parent item's index; `None` = the document body.
    pub parent: Option<usize>,
    /// Child item indices, in docling's `children` order.
    pub children: Vec<usize>,
    /// The content layer; `None` = `body`.
    pub layer: Option<ContentLayer>,
    pub kind: TreeKind,
    /// The item's `prov` entry, when the backend has page geometry for it
    /// (`None` → `prov: []`, what the HTML and DOCX backends write).
    pub prov: Option<TreeProv>,
    /// docling's `DocItem.comments`: the `comment_section` groups (or note
    /// text items) annotating this item, as item indices — written after
    /// `prov` when non-empty.
    pub comments: Vec<usize>,
    /// docling's `DocItem.source`: the track segment a text item was taken
    /// from (WebVTT cues) — written after `prov` when set.
    pub source: Option<TreeTrack>,
    /// Removed by [`ItemTree::delete`] (docling's `delete_items`): the slot
    /// stays so every other index keeps its meaning, but the item is not
    /// numbered or written.
    pub deleted: bool,
    /// Footnotes / endnotes referenced from inside this text item (#538): the
    /// note call's position (chars into the item's text) and the note's text.
    /// docling keeps notes as unlinked furniture `footnote` items, so the
    /// JSON never shows this; the Pandoc AST writes each as a `Note` there.
    pub notes: Vec<TreeNote>,
    /// This furniture `footnote` item is the body of a note some text item
    /// calls (it travels in that item's [`Self::notes`]); the Pandoc AST then
    /// leaves it out as a standalone block.
    pub note_body: bool,
    /// Link destinations at this item (#849): the identifiers a document's
    /// internal links (`hyperlink: "#id"`) point at — a DOCX bookmark in
    /// a heading, say. docling's model has no slot for them, so the JSON
    /// shows them only on the Pandoc path (`_anchors`), where a heading's
    /// first one is its `Header` id and the rest empty `Span .anchor`s.
    pub anchors: Vec<String>,
    /// The generated numbering `text` opens with, when the backend put one
    /// there (#850: a DOCX heading numbered through `w:numPr` is docling's
    /// `1.2 Title`): `"1.2"` — the authored text follows it after one
    /// space. docling's JSON keeps the numbered text; the Pandoc AST writes
    /// the authored text alone (`_number` on that path), since Pandoc
    /// numbers sections itself (`--number-sections`) and its docx reader
    /// never writes the number into the `Header`.
    pub number: Option<String>,
    /// The tracked change this run carries under
    /// [`TrackChanges::All`](crate::TrackChanges::All) (#851): the Pandoc
    /// AST wraps the run in `Span .insertion` / `.deletion` (`_revision`
    /// on that path); docling's JSON has no field for it.
    pub revision: Option<crate::Revision>,
    /// A picture's `meta.code` — docling's `CodeMetaField` (`text`,
    /// `language`): the LaTeX backend keeps a `tikzpicture`'s source there
    /// when no engine renders it (`("…", "Tikz")`).
    pub code_meta: Option<(String, String)>,
    /// The source block this item was made from (#903): an index into
    /// [`ItemTree::blocks`], set by a backend that enumerates its source's
    /// blocks (HTML). `None` when the backend does not, or for an item no
    /// single block produced.
    pub block: Option<usize>,
}

/// A block of the source document (#903, #904): the original element an
/// item was made from — or one that produced no item, with the reason —
/// so a caller can tie converted items back to the source, tell two
/// identical texts apart, and read a block's text on its own terms, free of
/// any serializer's separators.
///
/// The HTML backend enumerates, in document order, every element that
/// produced an item, every non-inline element with text of its own (text
/// nodes reached through inline elements only — a `<span>`'s text belongs
/// to the block around it), and every empty `<p>`, heading, `<li>`, `<dt>`,
/// `<dd>`, `<td>`/`<th>`, `<pre>` or `<caption>`. Pure containers (a layout
/// `<div>` with nothing but blocks inside) are not blocks. Every text node
/// of the source thus belongs to exactly one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceBlock {
    /// The element's tag name (`p`, `div`, `li`, `table`, `img`, …).
    pub tag: String,
    /// Its `id` attribute, when it has one.
    pub id: Option<String>,
    /// Its `class` attribute, as written.
    pub class: Option<String>,
    /// Every attribute of the element, by name (#910): `id` and `class`
    /// among them, and whatever else the source put there — `style`,
    /// `aria-hidden`, a RIS element's `typ`.
    pub attrs: Vec<(String, String)>,
    /// The element's path from the document root — each step the tag and
    /// the 1-based position among the parent's same-tag element children,
    /// `html/body/div[3]/p[1]` — stable for the document and unique to the
    /// element, with or without an `id`.
    pub path: String,
    /// The block's own text: its text nodes (and those of the inline
    /// elements inside it, not of nested blocks) in source order, whitespace
    /// runs collapsed to one space, trimmed. Independent of what the items
    /// made of it hold — a hidden block's text is here too.
    pub text: String,
    /// The items made from this block, in creation order — the top-level
    /// ones (an inline group, not the runs inside it; a list group, not
    /// its items, which have their `<li>` blocks). Empty when it produced
    /// none: see `omitted`.
    pub items: Vec<usize>,
    /// Why no item was made from this block; `None` when `items` is not
    /// empty.
    pub omitted: Option<BlockOmission>,
    /// The inline elements inside the block (#911) — a `<span
    /// class="sr-only">`, a `<b>`, an `<a>` — each with its range in `text`,
    /// in document order, nested ones after the ones they sit in.
    pub inlines: Vec<SourceInline>,
}

impl SourceBlock {
    /// The block's text without the inline elements `exclude` selects
    /// (#911): their ranges cut out, whitespace normalized again. A
    /// screen-reader alternative (`.sr-only`) left out, say — the caller
    /// chooses, nothing is inferred from a class.
    pub fn text_excluding(&self, exclude: impl Fn(&SourceInline) -> bool) -> String {
        let mut cut: Vec<(usize, usize)> = self
            .inlines
            .iter()
            .filter(|i| exclude(i))
            .map(|i| (i.start, i.end))
            .collect();
        if cut.is_empty() {
            return self.text.clone();
        }
        cut.sort_unstable();
        let mut out = String::with_capacity(self.text.len());
        let mut pos = 0;
        for (start, end) in cut {
            if start > pos {
                out.push_str(&self.text[pos..start]);
                out.push(' ');
            }
            pos = pos.max(end);
        }
        out.push_str(&self.text[pos.min(self.text.len())..]);
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

/// An inline element inside a [`SourceBlock`] (#911): where its text sits
/// in the block's, so a caller can select or leave out the text of one
/// element — not a whole block — by what the source says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInline {
    /// The element's tag name (`span`, `b`, `a`, …).
    pub tag: String,
    /// Every attribute, by name.
    pub attrs: Vec<(String, String)>,
    /// Its path from the document root, as [`SourceBlock::path`].
    pub path: String,
    /// The byte range of its text in [`SourceBlock::text`] — always on
    /// char boundaries, so `&text[start..end]` is the element's normalized
    /// text; empty for an element with no text.
    pub start: usize,
    pub end: usize,
}

impl SourceInline {
    /// The value of attribute `name`.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Its `id` attribute.
    pub fn id(&self) -> Option<&str> {
        self.attr("id")
    }

    /// Its `class` attribute, as written.
    pub fn class(&self) -> Option<&str> {
        self.attr("class")
    }

    /// Whether `class` is one of the element's classes.
    pub fn has_class(&self, class: &str) -> bool {
        self.class()
            .is_some_and(|c| c.split_whitespace().any(|x| x == class))
    }
}

/// Builds a [`SourceBlock`]'s normalized text while recording the inline
/// elements' ranges in it (#911) — for a backend walking its source: text
/// nodes through [`push_text`](Self::push_text), a line break as
/// whitespace, an inline element between [`open_inline`](Self::open_inline)
/// and [`close_inline`](Self::close_inline). Whitespace runs collapse to one
/// space, the text is trimmed, and a range never starts with the space
/// that separates it from what precedes.
#[derive(Debug, Default)]
pub struct SourceTextBuilder {
    text: String,
    /// Whitespace seen since the last non-space char, not yet written.
    pending: bool,
    inlines: Vec<SourceInline>,
    /// The inlines open right now, as indices into `inlines`.
    open: Vec<usize>,
}

impl SourceTextBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// A text node (or any text) of the block.
    pub fn push_text(&mut self, raw: &str) {
        for c in raw.chars() {
            if c.is_whitespace() {
                self.pending = true;
            } else {
                self.flush_space();
                self.text.push(c);
            }
        }
    }

    fn flush_space(&mut self) {
        if self.pending {
            // One space, however many elements' whitespace fell between two
            // words (an empty spacing `<span>` between two others).
            if !self.text.is_empty() && !self.text.ends_with(' ') {
                self.text.push(' ');
            }
            self.pending = false;
        }
    }

    /// An inline element starts here.
    pub fn open_inline(&mut self, tag: &str, attrs: Vec<(String, String)>, path: String) {
        self.flush_space();
        let at = self.text.len();
        self.open.push(self.inlines.len());
        self.inlines.push(SourceInline {
            tag: tag.to_string(),
            attrs,
            path,
            start: at,
            end: at,
        });
    }

    /// The innermost open inline element ends here.
    pub fn close_inline(&mut self) {
        if let Some(i) = self.open.pop() {
            self.inlines[i].end = self.text.len();
        }
    }

    /// The text and the inline ranges; the text has no trailing space.
    pub fn finish(mut self) -> (String, Vec<SourceInline>) {
        while let Some(i) = self.open.pop() {
            self.inlines[i].end = self.text.len();
        }
        if self.text.ends_with(' ') {
            self.text.pop();
            let len = self.text.len();
            for i in &mut self.inlines {
                i.start = i.start.min(len);
                i.end = i.end.min(len);
            }
        }
        (self.text, self.inlines)
    }
}

/// An element of the source that is not a block but carries an `id` or a
/// `class` (#910): a container — the `<div id="art_23">` an article's
/// paragraphs sit in — so an excerpt selected by a container's identity
/// resolves to a path and, through [`ItemTree::blocks_under`], to the blocks
/// inside it. A block's own identity is on the block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceElement {
    /// The element's tag name.
    pub tag: String,
    /// Its `id` attribute, when it has one.
    pub id: Option<String>,
    /// Its `class` attribute, as written.
    pub class: Option<String>,
    /// Every attribute, by name.
    pub attrs: Vec<(String, String)>,
    /// Its path from the document root, as [`SourceBlock::path`].
    pub path: String,
}

/// Why a [`SourceBlock`] produced no item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockOmission {
    /// No text of its own once normalized (an empty `<p>`, a `<li>` whose
    /// content is nested blocks).
    Empty,
    /// Hidden or non-content (`display: none`, `hidden`, `<script>`,
    /// `<style>`, `<noscript>`), or inside such an element.
    Hidden,
    /// In a table whose cells were flattened: the text is in the grid of
    /// the table item (the index) rather than an item of its own.
    InTable(usize),
    /// Its text went into the items of an enclosing block (the index into
    /// [`ItemTree::blocks`]): a `<p>` inside a `<li>` is the list item's
    /// text.
    Merged(usize),
    /// The walk produced nothing the backend can attribute.
    Unmapped,
}

/// A note call inside a text item ([`TreeItem::notes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNote {
    /// The call's position, in chars into the item's `text`.
    pub offset: usize,
    /// The note's plain text.
    pub text: String,
}

impl ItemTree {
    /// Where each note call of one paragraph lands (#538): `full_text` is the
    /// paragraph's text, `offsets` the calls' positions in it (chars), and
    /// the paragraph's items are those created from `first_new` on. Each text
    /// item is a trimmed slice of the paragraph (a formatting run, a link, or
    /// the whole heading / list item), matched in order; a call inside an
    /// item lands there, one between items ends the earlier (or starts the
    /// first). When no item matches, the call ends the paragraph's last text
    /// item; with no text item at all it is `None`.
    pub fn place_note_calls(
        &self,
        first_new: usize,
        full_text: &str,
        offsets: &[usize],
    ) -> Vec<Option<(usize, usize)>> {
        let texts: Vec<(usize, &str)> = (first_new..self.items.len())
            .filter(|&i| !self.items[i].deleted)
            .filter_map(|i| match &self.items[i].kind {
                TreeKind::Text { text, .. } | TreeKind::Code { text, .. } => {
                    Some((i, text.as_str()))
                }
                _ => None,
            })
            .collect();
        // Each found item's (id, start, end) in chars of `full_text`.
        let mut spans: Vec<(usize, usize, usize)> = Vec::new();
        let mut cursor = 0usize; // bytes
        for &(item, text) in &texts {
            if text.is_empty() {
                continue;
            }
            if let Some(pos) = full_text[cursor..].find(text) {
                let start_b = cursor + pos;
                let start = full_text[..start_b].chars().count();
                spans.push((item, start, start + text.chars().count()));
                cursor = start_b + text.len();
            }
        }
        offsets
            .iter()
            .map(|&offset| {
                spans
                    .iter()
                    .find(|&&(_, start, end)| offset >= start && offset <= end)
                    .map(|&(item, start, _)| (item, offset - start))
                    .or_else(
                        || match spans.iter().rev().find(|&&(_, _, end)| end <= offset) {
                            Some(&(item, start, end)) => Some((item, end - start)),
                            None => spans.first().map(|&(item, _, _)| (item, 0)),
                        },
                    )
                    .or_else(|| {
                        texts
                            .last()
                            .map(|&(item, text)| (item, text.chars().count()))
                    })
            })
            .collect()
    }
}

/// docling's item tree in creation order (see the [module docs](self)).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ItemTree {
    /// Every item, indexed by creation order — which is how docling numbers
    /// `#/texts/N`, `#/groups/N`, … within each bucket.
    pub items: Vec<TreeItem>,
    /// The body's `children`, as item indices.
    pub body: Vec<usize>,
    /// The source's blocks (#903), in document order, when the backend
    /// enumerates them (HTML); each item's [`TreeItem::block`] points into
    /// this. Empty otherwise.
    pub blocks: Vec<SourceBlock>,
    /// The source's containers that carry an `id` or a `class` (#910), in
    /// document order; see [`SourceElement`]. Empty when `blocks` is.
    pub containers: Vec<SourceElement>,
    /// The runs of this tree's `inline` groups carry their own boundary
    /// whitespace — docling-core's inline whitespace contract (schema 1.11.0,
    /// [`crate::inline`]): a backend that keeps each run's spaces, as
    /// docling's do since docling#3891, sets it. Unset, the runs are the
    /// trimmed runs the old `" "` join expected, and the export re-injects
    /// that join into them ([`Self::separate_legacy_inline_runs`]) so the
    /// output reads as it did.
    pub inline_whitespace: bool,
    /// The DocLang export serializes the flat nodes rather than this tree,
    /// which every other tree gets its DocLang from: the tree reproduces a
    /// loss of docling's the nodes avoid (USPTO's tables, emptied when
    /// docling's table regex misses them), and the DocLang follows the
    /// Markdown there.
    pub doclang_nodes: bool,
}

impl ItemTree {
    /// Append an item under `parent` (`None` = body) on `layer`, registering
    /// it as its parent's last child — docling's `add_*` calls do exactly that.
    pub fn add(
        &mut self,
        parent: Option<usize>,
        layer: Option<ContentLayer>,
        kind: TreeKind,
    ) -> usize {
        let id = self.items.len();
        self.items.push(TreeItem {
            parent,
            children: Vec::new(),
            layer,
            kind,
            prov: None,
            comments: Vec::new(),
            source: None,
            deleted: false,
            notes: Vec::new(),
            note_body: false,
            anchors: Vec::new(),
            number: None,
            revision: None,
            code_meta: None,
            block: None,
        });
        match parent {
            Some(p) => self.items[p].children.push(id),
            None => self.body.push(id),
        }
        id
    }

    /// [`add`](Self::add) with the item's provenance — docling's
    /// `add_text(…, prov=prov)`.
    pub fn add_with_prov(
        &mut self,
        parent: Option<usize>,
        layer: Option<ContentLayer>,
        kind: TreeKind,
        prov: TreeProv,
    ) -> usize {
        let id = self.add(parent, layer, kind);
        self.items[id].prov = Some(prov);
        id
    }

    /// The text of the given source blocks (#904), one line each: each
    /// block's own normalized [`SourceBlock::text`], joined with a single
    /// `\n` — no serializer decoration, no trailing separator, nothing of
    /// the blocks around them. Indices out of range are skipped.
    pub fn source_text(&self, blocks: impl IntoIterator<Item = usize>) -> String {
        let mut out = String::new();
        for b in blocks {
            let Some(block) = self.blocks.get(b) else {
                continue;
            };
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&block.text);
        }
        out
    }

    /// [`source_text`](Self::source_text) without the inline elements
    /// `exclude` selects in each block ([`SourceBlock::text_excluding`]).
    pub fn source_text_excluding(
        &self,
        blocks: impl IntoIterator<Item = usize>,
        exclude: impl Fn(&SourceInline) -> bool,
    ) -> String {
        let mut out = String::new();
        for b in blocks {
            let Some(block) = self.blocks.get(b) else {
                continue;
            };
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&block.text_excluding(&exclude));
        }
        out
    }

    /// The blocks under `path` (#910), in document order: those whose
    /// [`SourceBlock::path`] is `path` or starts with `path/` — the block
    /// itself, or everything inside a container.
    pub fn blocks_under(&self, path: &str) -> Vec<usize> {
        let prefix = format!("{path}/");
        self.blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.path == path || b.path.starts_with(&prefix))
            .map(|(i, _)| i)
            .collect()
    }

    /// The path of the element with `id` (#910): a block's or a container's,
    /// the first in document order when the source repeats an id.
    pub fn path_of_id(&self, id: &str) -> Option<&str> {
        let block = self
            .blocks
            .iter()
            .find(|b| b.id.as_deref() == Some(id))
            .map(|b| b.path.as_str());
        let container = self
            .containers
            .iter()
            .find(|c| c.id.as_deref() == Some(id))
            .map(|c| c.path.as_str());
        match (block, container) {
            (Some(b), Some(c)) => Some(if b.len() <= c.len() && b < c { b } else { c }),
            (b, c) => b.or(c),
        }
    }

    /// Append every item of `other` after this tree's, renumbering its
    /// indices (parents, children, comments, table/picture caption and
    /// rich-cell refs) and adding its body children to this body — so a
    /// backend can build independent fragments in parallel (one per PPTX
    /// slide) and still hand the export one tree in creation order, exactly
    /// as if it had been built sequentially.
    pub fn append(&mut self, other: ItemTree) {
        let off = self.items.len();
        let shift = |i: usize| i + off;
        for mut item in other.items {
            item.parent = item.parent.map(shift);
            for c in item.children.iter_mut().chain(item.comments.iter_mut()) {
                *c = shift(*c);
            }
            match &mut item.kind {
                TreeKind::Table {
                    rich_cells,
                    captions,
                    ..
                } => {
                    for (_, _, g) in rich_cells.iter_mut() {
                        *g = shift(*g);
                    }
                    for c in captions.iter_mut() {
                        *c = shift(*c);
                    }
                }
                TreeKind::Picture { captions, .. } | TreeKind::Code { captions, .. } => {
                    for c in captions.iter_mut() {
                        *c = shift(*c);
                    }
                }
                _ => {}
            }
            self.items.push(item);
        }
        self.body.extend(other.body.into_iter().map(shift));
        let block_off = self.blocks.len();
        for mut b in other.blocks {
            for i in b.items.iter_mut() {
                *i = shift(*i);
            }
            if let Some(BlockOmission::InTable(t)) = b.omitted.as_mut() {
                *t = shift(*t);
            }
            if let Some(BlockOmission::Merged(m)) = b.omitted.as_mut() {
                *m += block_off;
            }
            self.blocks.push(b);
        }
        for item in &mut self.items[off..] {
            if let Some(b) = item.block.as_mut() {
                *b += block_off;
            }
        }
        self.containers.extend(other.containers);
    }

    /// Move `id` under `new_parent`, dropping it from its current parent's
    /// children and appending it to the new one's — docling's
    /// `group_cell_elements` re-parenting of a rich cell's items.
    pub fn reparent(&mut self, id: usize, new_parent: Option<usize>) {
        let old = self.items[id].parent;
        let siblings = match old {
            Some(p) => &mut self.items[p].children,
            None => &mut self.body,
        };
        siblings.retain(|&c| c != id);
        self.items[id].parent = new_parent;
        match new_parent {
            Some(p) => self.items[p].children.push(id),
            None => self.body.push(id),
        }
    }

    /// Re-number the items in traversal order — a pre-order walk of the body
    /// through every layer, which is how docling's
    /// `DoclingDocument.concatenate` (and `_normalize_references`) re-creates
    /// a document's items: a group created after the content it was later
    /// wrapped around comes before that content afterwards. Items the walk
    /// does not reach (deleted ones) are dropped. Returns each old id's new
    /// id.
    pub fn renumber_in_traversal_order(&mut self) -> Vec<Option<usize>> {
        let mut order = Vec::with_capacity(self.items.len());
        let mut stack: Vec<usize> = self.body.iter().rev().copied().collect();
        while let Some(id) = stack.pop() {
            if self.items[id].deleted {
                continue;
            }
            order.push(id);
            stack.extend(self.items[id].children.iter().rev());
        }
        let mut new_of: Vec<Option<usize>> = vec![None; self.items.len()];
        for (new, &old) in order.iter().enumerate() {
            new_of[old] = Some(new);
        }
        let remap = |ids: &mut Vec<usize>| {
            *ids = ids.iter().filter_map(|&i| new_of[i]).collect();
        };
        let mut old_items: Vec<Option<TreeItem>> = std::mem::take(&mut self.items)
            .into_iter()
            .map(Some)
            .collect();
        for &old in &order {
            let mut item = old_items[old].take().expect("each item visited once");
            item.parent = item.parent.and_then(|p| new_of[p]);
            remap(&mut item.children);
            remap(&mut item.comments);
            match &mut item.kind {
                TreeKind::Table {
                    rich_cells,
                    captions,
                    ..
                } => {
                    rich_cells.retain_mut(|(_, _, g)| match new_of[*g] {
                        Some(n) => {
                            *g = n;
                            true
                        }
                        None => false,
                    });
                    remap(captions);
                }
                TreeKind::Picture { captions, .. } | TreeKind::Code { captions, .. } => {
                    remap(captions)
                }
                _ => {}
            }
            self.items.push(item);
        }
        remap(&mut self.body);
        for b in &mut self.blocks {
            remap(&mut b.items);
            if let Some(BlockOmission::InTable(t)) = b.omitted {
                b.omitted = Some(match new_of[t] {
                    Some(n) => BlockOmission::InTable(n),
                    None => BlockOmission::Unmapped,
                });
            }
        }
        new_of
    }

    /// Remove `id` from the tree — docling's `delete_items`, which the DOCX
    /// backend uses to drop the empty text item a blank spacer paragraph left
    /// between two items of a resumed list. The item leaves its parent's
    /// children and is neither numbered nor written; its slot stays so the
    /// indices held elsewhere stay valid.
    pub fn delete(&mut self, id: usize) {
        match self.items[id].parent {
            Some(p) => self.items[p].children.retain(|&c| c != id),
            None => self.body.retain(|&c| c != id),
        }
        self.items[id].deleted = true;
    }

    /// docling-core's `_migrate_legacy_inline_separators` (#997) over the
    /// tree: every `inline` group's runs get the `" "` the pre-1.11 join put
    /// between them ([`crate::inline::separate_legacy_runs`]) — on a plain
    /// neighbour, never inside a code or formula run, a standalone `" "` text
    /// run when both neighbours are delimited. The JSON export applies it to
    /// a tree whose backend has not set [`Self::inline_whitespace`], so a
    /// trimmed-run backend still reads `left bold right` under the faithful
    /// join, as docling-core reads a document saved before the contract.
    /// `text` changes, `orig` keeps the run's own text.
    pub fn separate_legacy_inline_runs(&mut self) {
        fn renders_text(t: &ItemTree, id: usize, depth: usize) -> bool {
            let item = &t.items[id];
            if item.deleted || depth > 64 {
                return false;
            }
            let own = match &item.kind {
                TreeKind::Text { text, .. } | TreeKind::Code { text, .. } => !text.is_empty(),
                TreeKind::Group { .. } => false,
                // docling-core resolves only `#/texts/` and `#/groups/`.
                _ => return false,
            };
            own || item.children.iter().any(|&c| renders_text(t, c, depth + 1))
        }
        let groups: Vec<usize> = (0..self.items.len())
            .filter(|&g| {
                !self.items[g].deleted
                    && matches!(&self.items[g].kind, TreeKind::Group { label, .. } if label == "inline")
            })
            .collect();
        for g in groups {
            let children: Vec<usize> = self.items[g]
                .children
                .iter()
                .copied()
                .filter(|&c| !self.items[c].deleted)
                .collect();
            let flags: Vec<(bool, bool, bool)> = children
                .iter()
                .map(|&c| {
                    let item = &self.items[c];
                    let (plain, empty_plain) = match &item.kind {
                        TreeKind::Text { label, text, .. } => (
                            label != "formula" && label != "code",
                            label == "text" && text.is_empty() && item.children.is_empty(),
                        ),
                        _ => (false, false),
                    };
                    (renders_text(self, c, 0), plain, empty_plain)
                })
                .collect();
            // Detach the texts so each run can be borrowed mutably at once.
            let mut texts: Vec<String> = children
                .iter()
                .map(|&c| match &self.items[c].kind {
                    TreeKind::Text { text, .. } | TreeKind::Code { text, .. } => text.clone(),
                    _ => String::new(),
                })
                .collect();
            let originals = texts.clone();
            let standalone = {
                let mut runs: Vec<crate::inline::LegacyRun<'_>> = texts
                    .iter_mut()
                    .zip(&flags)
                    .map(
                        |(text, &(renders_text, plain, empty_plain))| crate::inline::LegacyRun {
                            text,
                            renders_text,
                            plain,
                            empty_plain,
                        },
                    )
                    .collect();
                crate::inline::separate_legacy_runs(&mut runs)
            };
            for ((&c, new), old) in children.iter().zip(texts).zip(originals) {
                if new == old {
                    continue;
                }
                // docling-core rewrites `text` and leaves `orig` as it was.
                if let TreeKind::Text { text, orig, .. } | TreeKind::Code { text, orig, .. } =
                    &mut self.items[c].kind
                {
                    orig.get_or_insert(old);
                    *text = new;
                }
            }
            for at in standalone.into_iter().rev() {
                let (left, right) = (children[at - 1], children[at]);
                // The left run's location when both sit on one page, so a
                // page filter keeps the separator exactly when it keeps both.
                let prov = match (&self.items[left].prov, &self.items[right].prov) {
                    (Some(l), Some(r)) if l.page_no == r.page_no => Some(TreeProv {
                        charspan: [0, 1],
                        ..l.clone()
                    }),
                    _ => None,
                };
                let layer = self.items[g].layer;
                let id = self.add(
                    Some(g),
                    layer,
                    TreeKind::Text {
                        label: "text".into(),
                        text: " ".into(),
                        orig: None,
                        formatting: None,
                        hyperlink: None,
                        level: None,
                        list: None,
                    },
                );
                self.items[id].prov = prov;
                let kids = &mut self.items[g].children;
                kids.pop();
                let pos = kids.iter().position(|&k| k == right).unwrap_or(kids.len());
                kids.insert(pos, id);
            }
        }
    }

    /// docling-core's `DoclingDocument.validate_misplaced_list_items` (a
    /// model validator, so it runs whenever docling-core serializes or loads
    /// a document): every `list_item` whose parent is not a `list` group is
    /// re-homed into a new one. A pre-order walk of the body (groups
    /// included) collects them; consecutive misplaced items directly on the
    /// body share one group, any other misplaced item gets its own. Working
    /// from the last run back, each run gets a `ListGroup` (name `group`)
    /// inserted where its first item stood, the items are deleted and
    /// re-added under the group as fresh items, so they move to the end of
    /// the text numbering (#527: the DOCX backend leaves such items in rich
    /// table cells). A no-op for a well-formed tree.
    ///
    /// One deliberate divergence (#586): docling-core deletes each item *with
    /// its children* and re-adds it from its text alone, so a mixed-format
    /// item — an empty `list_item` over an `inline` group of text runs —
    /// comes back empty and its text is gone from the JSON (and from the
    /// Markdown/HTML docling serializes after that validator ran; the JSON
    /// docling saves *before* exporting still has it). Here the children
    /// follow the item into the group: the structure docling-core requires,
    /// the content the document had.
    pub fn wrap_misplaced_list_items(&mut self) {
        let is_list_item = |t: &Self, id: usize| matches!(&t.items[id].kind, TreeKind::Text { label, .. } if label == "list_item");
        let in_list_group = |t: &Self, id: usize| {
            t.items[id].parent.is_some_and(
                |p| matches!(&t.items[p].kind, TreeKind::Group { label, .. } if label == "list"),
            )
        };
        let mut runs: Vec<Vec<usize>> = Vec::new();
        // `None` = the body itself, which the walk yields first.
        let mut prev: Option<usize> = None;
        let mut stack: Vec<usize> = self.body.iter().rev().copied().collect();
        while let Some(id) = stack.pop() {
            if self.items[id].deleted {
                continue;
            }
            if is_list_item(self, id) && !in_list_group(self, id) {
                let continues =
                    prev.is_some_and(|p| is_list_item(self, p) && self.items[p].parent.is_none());
                match runs.last_mut() {
                    Some(run) if continues => run.push(id),
                    _ => runs.push(vec![id]),
                }
            }
            prev = Some(id);
            stack.extend(self.items[id].children.iter().rev().copied());
        }
        for run in runs.into_iter().rev() {
            let parent = self.items[run[0]].parent;
            let group = self.add(
                parent,
                None,
                TreeKind::Group {
                    label: "list".into(),
                    name: "group".into(),
                },
            );
            let siblings = match parent {
                Some(p) => &mut self.items[p].children,
                None => &mut self.body,
            };
            siblings.pop();
            let at = siblings
                .iter()
                .position(|&c| c == run[0])
                .unwrap_or(siblings.len());
            siblings.insert(at, group);
            for &li in &run {
                // Not `delete_subtree`: the children move to the copy (#586).
                self.delete(li);
            }
            // `add_list_item` keeps the text, marker, formatting, hyperlink
            // and first provenance — not comments or a source. The children
            // (the inline group of a mixed-format item) are carried over.
            for &li in &run {
                let children = std::mem::take(&mut self.items[li].children);
                let copy = TreeItem {
                    parent: Some(group),
                    children: children.clone(),
                    comments: Vec::new(),
                    source: None,
                    deleted: false,
                    ..self.items[li].clone()
                };
                let id = self.items.len();
                self.items.push(copy);
                for c in children {
                    self.items[c].parent = Some(id);
                }
                self.items[group].children.push(id);
            }
        }
    }

    /// The last live text-bucket item (docling's `doc.texts[-1]`).
    pub fn last_text(&self) -> Option<usize> {
        self.items.iter().rposition(|it| {
            !it.deleted && matches!(it.kind, TreeKind::Text { .. } | TreeKind::Code { .. })
        })
    }

    /// How many items of a bucket precede `id` — its `#/{bucket}/N` index.
    pub fn bucket_index(&self, id: usize) -> usize {
        let same = |k: &TreeKind| {
            std::mem::discriminant(k) == std::mem::discriminant(&self.items[id].kind)
                || matches!(
                    (k, &self.items[id].kind),
                    (TreeKind::Text { .. }, TreeKind::Code { .. })
                        | (TreeKind::Code { .. }, TreeKind::Text { .. })
                )
        };
        self.items[..id]
            .iter()
            .filter(|it| !it.deleted && same(&it.kind))
            .count()
    }

    /// The number of tables created so far (docling's `len(doc.tables)`).
    pub fn table_count(&self) -> usize {
        self.items
            .iter()
            .filter(|it| !it.deleted && matches!(it.kind, TreeKind::Table { .. }))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(t: &str) -> TreeKind {
        TreeKind::Text {
            label: "text".into(),
            text: t.into(),
            orig: None,
            formatting: None,
            hyperlink: None,
            level: None,
            list: None,
        }
    }

    /// `add` registers the item as its parent's (or the body's) last child;
    /// `reparent` moves it — a rich cell's items leave the heading they were
    /// created under for the table's group.
    #[test]
    fn add_and_reparent_keep_docling_children_order() {
        let mut t = ItemTree::default();
        let title = t.add(None, None, text("Title"));
        let a = t.add(Some(title), None, text("a"));
        let b = t.add(Some(title), None, text("b"));
        let table = t.add(
            Some(title),
            None,
            TreeKind::Table {
                table: Table::default(),
                rich_cells: Vec::new(),
                captions: Vec::new(),
            },
        );
        let group = t.add(
            Some(table),
            None,
            TreeKind::Group {
                label: "unspecified".into(),
                name: "rich_cell_group_1_0_0".into(),
            },
        );
        assert_eq!(t.body, vec![title]);
        assert_eq!(t.items[title].children, vec![a, b, table]);
        t.reparent(a, Some(group));
        assert_eq!(t.items[title].children, vec![b, table]);
        assert_eq!(t.items[group].children, vec![a]);
        assert_eq!(t.items[a].parent, Some(group));
        assert_eq!(t.table_count(), 1);
        // Text and code share the `texts` bucket.
        let code = t.add(
            None,
            None,
            TreeKind::Code {
                text: "x".into(),
                orig: None,
                language: None,
                formatting: None,
                hyperlink: None,
                captions: Vec::new(),
            },
        );
        assert_eq!(t.bucket_index(code), 3, "title, a, b precede it in `texts`");
        assert_eq!(t.bucket_index(group), 0);
        assert_eq!(t.body, vec![title, code]);
    }

    /// `append` renumbers a fragment built on its own (a slide converted in
    /// parallel) so the merged tree reads as if built in one pass: parents,
    /// children, comment back-refs and caption refs all shift together.
    #[test]
    fn append_renumbers_a_fragment_into_creation_order() {
        let mut whole = ItemTree::default();
        let slide0 = whole.add(
            None,
            None,
            TreeKind::Group {
                label: "chapter".into(),
                name: "slide-0".into(),
            },
        );
        whole.add(Some(slide0), None, text("first"));

        let mut frag = ItemTree::default();
        let slide1 = frag.add(
            None,
            None,
            TreeKind::Group {
                label: "chapter".into(),
                name: "slide-1".into(),
            },
        );
        let cap = frag.add_with_prov(
            Some(slide1),
            None,
            TreeKind::Text {
                label: "caption".into(),
                text: "Title".into(),
                orig: None,
                formatting: None,
                hyperlink: None,
                level: None,
                list: None,
            },
            TreeProv {
                page_no: 2,
                bbox: [1.0, 2.0, 3.0, 4.0],
                bottom_left: true,
                charspan: [0, 5],
            },
        );
        let pic = frag.add(
            Some(slide1),
            None,
            TreeKind::Picture {
                captions: vec![cap],
                image: None,
                extra_classes: Vec::new(),
                classification: Some("bar_chart".into()),
                description: None,
                confidence: None,
                chart: None,
                dpi: None,
            },
        );
        let note = frag.add(
            None,
            Some(ContentLayer::Notes),
            TreeKind::Group {
                label: "comment_section".into(),
                name: "comment-slide2-1".into(),
            },
        );
        frag.items[pic].comments.push(note);

        whole.append(frag);
        assert_eq!(whole.body, vec![slide0, 2, 5]);
        assert_eq!(whole.items[2].children, vec![3, 4]);
        assert_eq!(whole.items[3].parent, Some(2));
        assert_eq!(whole.items[3].prov.as_ref().map(|p| p.page_no), Some(2));
        assert!(
            matches!(&whole.items[4].kind, TreeKind::Picture { captions, .. } if captions == &[3])
        );
        assert_eq!(whole.items[4].comments, vec![5]);
        assert_eq!(whole.items[5].parent, None);
        assert_eq!(
            whole.bucket_index(4),
            0,
            "the fragment's picture is #/pictures/0"
        );
        assert_eq!(
            whole.bucket_index(5),
            2,
            "slide-0, slide-1 precede it in `groups`"
        );
    }

    /// `renumber_in_traversal_order`: docling's `concatenate` re-creates the
    /// items as a pre-order walk meets them, so a group created after the
    /// content it was wrapped around (a rich cell's group) comes first, and a
    /// deleted item disappears; every cross-reference follows.
    #[test]
    fn renumbering_follows_the_traversal() {
        let mut t = ItemTree::default();
        let a = t.add(None, None, text("a"));
        let table = t.add(
            None,
            None,
            TreeKind::Table {
                table: Table::default(),
                rich_cells: Vec::new(),
                captions: Vec::new(),
            },
        );
        let cell_text = t.add(None, None, text("cell"));
        let group = t.add(
            Some(table),
            None,
            TreeKind::Group {
                label: "unspecified".into(),
                name: "rich_cell_group_1_0_0".into(),
            },
        );
        t.reparent(cell_text, Some(group));
        if let TreeKind::Table { rich_cells, .. } = &mut t.items[table].kind {
            rich_cells.push((0, 0, group));
        }
        let gone = t.add(None, None, text("gone"));
        t.delete(gone);
        let z = t.add(None, None, text("z"));

        let new_of = t.renumber_in_traversal_order();
        assert_eq!(
            new_of,
            vec![Some(0), Some(1), Some(3), Some(2), None, Some(4)]
        );
        assert_eq!(t.items.len(), 5);
        assert_eq!(t.body, vec![0, 1, 4]);
        assert_eq!(t.items[1].children, vec![2], "the group follows its table");
        assert_eq!(t.items[2].parent, Some(1));
        assert_eq!(
            t.items[3].parent,
            Some(2),
            "the cell text follows its group"
        );
        assert!(matches!(&t.items[3].kind, TreeKind::Text { text, .. } if text == "cell"));
        assert!(
            matches!(&t.items[1].kind, TreeKind::Table { rich_cells, .. } if rich_cells == &[(0, 0, 2)])
        );
        let _ = (a, z);
    }

    /// #586: a misplaced `list_item` keeps its children when it is re-homed.
    /// docling-core's `validate_misplaced_list_items` re-adds the item from
    /// its text alone, so a mixed-format item (empty `list_item` over an
    /// `inline` group of runs) lost every run — a DOCX table cell whose list
    /// paragraph followed a `numId 0` spacer came out as an empty bullet in
    /// the JSON and the HTML. The group goes where the item stood, the copy
    /// is numbered last, the inline group and its texts move under it.
    #[test]
    fn wrapping_a_misplaced_list_item_keeps_its_runs() {
        let mut t = ItemTree::default();
        let cell = t.add(
            None,
            None,
            TreeKind::Group {
                label: "unspecified".into(),
                name: "rich_cell_group_1_0_1".into(),
            },
        );
        let before = t.add(Some(cell), None, text("before"));
        let item = t.add(
            Some(cell),
            None,
            TreeKind::Text {
                label: "list_item".into(),
                text: String::new(),
                orig: None,
                formatting: None,
                hyperlink: None,
                level: None,
                list: Some(ListMeta {
                    enumerated: false,
                    marker: String::new(),
                }),
            },
        );
        let inline = t.add(
            Some(item),
            None,
            TreeKind::Group {
                label: "inline".into(),
                name: "group".into(),
            },
        );
        let run_a = t.add(Some(inline), None, text("Second item text"));
        let run_b = t.add(Some(inline), None, text("[Optional]"));
        let after = t.add(Some(cell), None, text("after"));

        t.wrap_misplaced_list_items();

        let group = t.items[cell].children[1];
        assert_eq!(t.items[cell].children, vec![before, group, after]);
        assert!(
            matches!(&t.items[group].kind, TreeKind::Group { label, name } if label == "list" && name == "group")
        );
        assert!(t.items[item].deleted, "the original item is deleted");
        let copy = t.items[group].children[0];
        assert_ne!(copy, item);
        assert!(
            copy > after,
            "the copy is numbered after every existing item"
        );
        assert_eq!(t.items[copy].children, vec![inline]);
        assert_eq!(t.items[inline].parent, Some(copy));
        assert_eq!(t.items[inline].children, vec![run_a, run_b]);
        for id in [inline, run_a, run_b] {
            assert!(!t.items[id].deleted, "item {id} must survive the re-homing");
        }
    }
}
