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
    },
    /// A form key-value region (`field_regions` / `field_items`).
    FieldRegion { items: Vec<FieldItem> },
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
    /// docling's `DocItem.comments`: the `comment_section` groups (or note
    /// text items) annotating this item, as item indices — written after
    /// `prov` when non-empty.
    pub comments: Vec<usize>,
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
            comments: Vec::new(),
            deleted: false,
        });
        match parent {
            Some(p) => self.items[p].children.push(id),
            None => self.body.push(id),
        }
        id
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
}
