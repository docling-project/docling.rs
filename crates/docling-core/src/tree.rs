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
    /// docling's `coord_origin` tag. `MsPowerpointDocumentBackend` builds a
    /// shape's box with `BoundingBox.from_tuple(…, BOTTOMLEFT)` — which reads
    /// the tuple as `(l, b, r, t)`, so the shape's top EMU lands in `b` — a
    /// quirk the JSON keeps; a speaker note's zero box is `TOPLEFT`
    /// (`BoundingBox`'s default).
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
}

/// docling's item tree in creation order (see the [module docs](self)).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ItemTree {
    /// Every item, indexed by creation order — which is how docling numbers
    /// `#/texts/N`, `#/groups/N`, … within each bucket.
    pub items: Vec<TreeItem>,
    /// The body's `children`, as item indices.
    pub body: Vec<usize>,
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
                TreeKind::Picture { captions, .. } => {
                    for c in captions.iter_mut() {
                        *c = shift(*c);
                    }
                }
                _ => {}
            }
            self.items.push(item);
        }
        self.body.extend(other.body.into_iter().map(shift));
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
                classification: Some("bar_chart".into()),
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
}
