//! Apple Keynote (`.key`) — the content a presentation holds, however its
//! container spells it, and how it becomes a [`DoclingDocument`] (docling's
//! `IWorkKeynoteDocumentBackend`, docling#4330; #466).
//!
//! A presentation is a list of slides rather than one flow of text, so it is
//! modelled here instead of reusing [`super::pages::Content`], which a Pages
//! document shapes itself to. What sits *on* a slide is the shared model — the
//! same paragraphs, tables and pictures, read by the same readers — filled in
//! by [`super::keynote_iwa`] (Keynote 6+, the `Index/*.iwa` object graph, also
//! when Keynote 2018+ zipped that index a second time) and
//! [`super::keynote_xml`] (iWork '09, the `index.apxl` XML).
//!
//! Upstream's shape, reproduced by [`emit`]: a `chapter` group named
//! `slide-{i}` per slide holding what was placed on it in reading order
//! (down, then across — Keynote stores drawables in stacking order), a page
//! of the slide's own size, every item with a `TOPLEFT` box of the drawable
//! holding it and a `charspan` over its text, presenter notes and comments on
//! the `notes` content layer under the slide (a note with a zero box, a
//! comment with none). The title placeholder's paragraphs are `title` items,
//! everything else `text` — theme style names are localised, so the
//! placeholder is what decides. Both container generations store a slide's
//! drawables in the order they are drawn, so the reading-order repair lives
//! here rather than in either reader.

use docling_core::tree::{Formatting as TreeFormatting, ItemTree, ListMeta, TreeKind, TreeProv};
use docling_core::{ContentLayer, DoclingDocument, Node};

use super::pages::{
    paragraph_node, uniform_run, Block, Comment, Formatting, Label, ListStack, Paragraph,
};

/// The slide size Keynote used before widescreen, in points. It stands in for
/// a presentation whose own size cannot be read, so that every slide still
/// gets a page of plausible dimensions rather than none.
pub(crate) const DEFAULT_SLIDE_WIDTH: f64 = 1024.0;
pub(crate) const DEFAULT_SLIDE_HEIGHT: f64 = 768.0;

/// How far apart two drawables' top edges may be and still share a row, in
/// points: 0.05 inch, the band the PowerPoint backend groups shapes into.
const SLIDE_ROW_TOLERANCE: f64 = 3.6;

/// Where a drawable with no readable geometry sorts: larger than any slide,
/// so it falls to the end while keeping its stored position relative to the
/// others there.
const UNPLACED: f64 = 1e9;

/// Where a drawable sits on its slide, in points from the top-left corner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Geometry {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

/// One block of a slide, and where the drawable holding it sits. Every block
/// a drawable yields takes that drawable's geometry, so the three bullets of
/// one text box share its box — the granularity the PowerPoint backend
/// records, since neither container writes down where a line landed.
pub(crate) struct Placed {
    pub block: Block,
    pub geometry: Option<Geometry>,
}

/// One slide: what is placed on it, its presenter notes, and what was said
/// about it. Notes and comments are kept apart from `blocks`: neither is shown
/// when the deck is presented, and both belong to the slide as a whole.
pub(crate) struct Slide {
    pub blocks: Vec<Placed>,
    pub notes: Vec<Paragraph>,
    pub comments: Vec<Comment>,
}

/// Everything one Keynote document holds.
pub(crate) struct Presentation {
    pub slides: Vec<Slide>,
    pub width: f64,
    pub height: f64,
}

/// docling's `reading_order`: a slide's drawables the way they are read —
/// down, then across. Drawables whose top edges are within
/// [`SLIDE_ROW_TOLERANCE`] of the one before them share a row and are ordered
/// left to right within it; adjacency is measured against the previous
/// drawable rather than the row's first, so a band of shapes that drift
/// downwards stays one row. Ties keep the stored order.
pub(crate) fn reading_order(placed: &[Option<Geometry>]) -> Vec<usize> {
    let mut entries: Vec<(f64, f64, usize)> = placed
        .iter()
        .enumerate()
        .map(|(position, g)| match g {
            Some(g) => (g.top, g.left, position),
            None => (UNPLACED, UNPLACED, position),
        })
        .collect();
    entries.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.2.cmp(&b.2)));

    let mut ordered = Vec::new();
    let mut row: Vec<(f64, f64, usize)> = Vec::new();
    let mut previous: Option<f64> = None;
    for entry in entries {
        if previous.is_some_and(|p| entry.0 - p > SLIDE_ROW_TOLERANCE) {
            ordered.extend(across(&row));
            row.clear();
        }
        previous = Some(entry.0);
        row.push(entry);
    }
    ordered.extend(across(&row));
    ordered
}

/// One row of drawables left to right, ties going to the earlier one.
fn across(row: &[(f64, f64, usize)]) -> Vec<usize> {
    let mut row: Vec<&(f64, f64, usize)> = row.iter().collect();
    row.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.2.cmp(&b.2)));
    row.into_iter().map(|e| e.2).collect()
}

/// docling's `titled`: relabel a paragraph by where it sits on the slide, not
/// by its style name — a list item is left alone, since the marker it carries
/// already labels it.
pub(crate) fn titled(block: Block, title: bool) -> Block {
    match block {
        Block::Paragraph(mut p) if p.list.is_none() => {
            p.label = if title { Label::Title } else { Label::Text };
            Block::Paragraph(p)
        }
        other => other,
    }
}

/// Turn the presentation into nodes and docling's item tree — upstream's
/// `convert` + `_add_slide`. The flat nodes feed Markdown / DocLang / LaTeX
/// (a group per slide, notes and comments on the notes layer, a DocLang page
/// break after every slide but the first, as the PPTX backend does); the
/// tree carries the JSON's groups, layers and provenance.
pub(crate) fn emit(presentation: Presentation, doc: &mut DoclingDocument) {
    let mut tree = ItemTree {
        // A run's text is the source's, its whitespace included (schema
        // 1.11.0, #997).
        inline_whitespace: true,
        ..ItemTree::default()
    };
    let (width, height) = (presentation.width, presentation.height);
    for (ix, slide) in presentation.slides.into_iter().enumerate() {
        let page_no = ix + 1;
        // Every slide is a page of the slide size (`doc.add_page`).
        doc.push(Node::PageInfo {
            page_no,
            width: width as f32,
            height: height as f32,
        });
        let group = tree.add(
            None,
            None,
            TreeKind::Group {
                label: "chapter".into(),
                name: format!("slide-{ix}"),
            },
        );
        let mut out = SlideOut {
            tree: &mut tree,
            group,
            page_no,
            size: (width, height),
            nodes: Vec::new(),
            lists: ListStack::default(),
            tree_lists: Vec::new(),
        };
        for placed in slide.blocks {
            out.add_block(placed);
        }
        for note in &slide.notes {
            out.add_note(note);
        }
        for comment in &slide.comments {
            out.add_comment(comment);
        }
        let children = out.nodes;
        doc.push(Node::Group {
            label: "chapter".into(),
            name: Some(format!("slide-{ix}")),
            layer: None,
            children,
        });
        if ix > 0 {
            doc.push(Node::PageBreak);
        }
    }
    doc.tree = Some(tree);
}

/// What one slide's emission accumulates.
struct SlideOut<'t> {
    tree: &'t mut ItemTree,
    /// The slide's `chapter` group in the tree.
    group: usize,
    page_no: usize,
    size: (f64, f64),
    /// The slide group's flat children.
    nodes: Vec<Node>,
    /// The flat list bookkeeping (numbering, `first_in_list`).
    lists: ListStack,
    /// docling's `_ListStack` over the tree: the `list` groups open at each
    /// depth while consecutive list items keep arriving.
    tree_lists: Vec<usize>,
}

impl SlideOut<'_> {
    /// `_slide_prov`: the item's page, the drawable's box (an empty one for a
    /// drawable Keynote did not position) and a span over its text.
    fn prov(&self, geometry: Option<Geometry>, text: &str) -> TreeProv {
        TreeProv {
            page_no: self.page_no,
            bbox: geometry
                .map(|g| [g.left, g.top, g.left + g.width, g.top + g.height])
                .unwrap_or([0.0; 4]),
            bottom_left: false,
            charspan: [0, text.chars().count()],
        }
    }

    /// The DocLang `<location>` of a drawable: its box on the 0–511 grid of
    /// the slide.
    fn location(&self, geometry: Geometry) -> [u16; 4] {
        let (w, h) = self.size;
        let grid = |v: f64, extent: f64| -> u16 {
            if extent <= 0.0 {
                return 0;
            }
            (v / extent * 511.0).round().clamp(0.0, 511.0) as u16
        };
        [
            grid(geometry.left, w),
            grid(geometry.top, h),
            grid(geometry.left + geometry.width, w),
            grid(geometry.top + geometry.height, h),
        ]
    }

    /// `_ListStack.group_for`: the tree group a list item at `depth` belongs
    /// in, opening groups down to it under the last open one (or the slide).
    fn list_group_for(&mut self, depth: usize) -> usize {
        self.tree_lists.truncate(depth + 1);
        while self.tree_lists.len() <= depth {
            let parent = self.tree_lists.last().copied().unwrap_or(self.group);
            let g = self.tree.add(
                Some(parent),
                None,
                TreeKind::Group {
                    label: "list".into(),
                    name: "list".into(),
                },
            );
            self.tree_lists.push(g);
        }
        self.tree_lists[depth]
    }

    fn close_lists(&mut self) {
        self.lists.close();
        self.tree_lists.clear();
    }

    /// `_add_block`: one block of the slide, in reading order.
    fn add_block(&mut self, placed: Placed) {
        let Placed { block, geometry } = placed;
        match block {
            Block::Paragraph(p) => self.add_paragraph(&p, geometry),
            Block::Picture(pic) => {
                // A table or a picture ends any list it follows, like body text.
                self.close_lists();
                let image = pic
                    .data
                    .as_ref()
                    .and_then(|d| super::ooxml::picture_image(&pic.name, d.clone()));
                let prov = self.prov(geometry, "");
                self.tree.add_with_prov(
                    Some(self.group),
                    None,
                    TreeKind::Picture {
                        captions: Vec::new(),
                        image: image.clone(),
                        extra_classes: Vec::new(),
                        classification: None,
                        description: None,
                        confidence: None,
                        chart: None,
                        dpi: None,
                    },
                    prov,
                );
                self.push(
                    Node::Picture {
                        caption: None,
                        caption_href: None,
                        image,
                        classification: None,
                        description: None,
                        caption_parent: Default::default(),
                        caption_location: None,
                    },
                    geometry,
                );
            }
            Block::Table(table) => {
                self.close_lists();
                let prov = self.prov(geometry, "");
                self.tree.add_with_prov(
                    Some(self.group),
                    None,
                    TreeKind::Table {
                        table: table.clone(),
                        rich_cells: Vec::new(),
                        captions: Vec::new(),
                    },
                    prov,
                );
                self.push(Node::Table(table), geometry);
            }
            // `_add_chart`: a picture classified by the chart's kind, carrying
            // its data as a table, captioned with the title the chart shows
            // (docling#4376) — the shape the PowerPoint backend gives a chart.
            // The caption comes first, with the chart's box and a charspan
            // over the title.
            Block::Chart(chart) => {
                self.close_lists();
                let caption = chart.title.as_deref().map(|title| {
                    let prov = self.prov(geometry, title);
                    self.tree.add_with_prov(
                        Some(self.group),
                        None,
                        text_kind("caption", title, None, None, None),
                        prov,
                    )
                });
                let table = chart.table();
                let prov = self.prov(geometry, "");
                self.tree.add_with_prov(
                    Some(self.group),
                    None,
                    TreeKind::Picture {
                        captions: caption.into_iter().collect(),
                        image: None,
                        extra_classes: Vec::new(),
                        classification: Some(chart.label.clone()),
                        description: None,
                        confidence: None,
                        chart: table.clone(),
                        dpi: None,
                    },
                    prov,
                );
                self.push(
                    Node::Chart {
                        kind: chart.label,
                        table: table.unwrap_or_default(),
                        caption: chart.title,
                        location: None,
                    },
                    geometry,
                );
            }
        }
    }

    /// `_add_paragraph`: a title, a heading, a list item or body text.
    fn add_paragraph(&mut self, p: &Paragraph, geometry: Option<Geometry>) {
        let text = p.text();
        let prov = self.prov(geometry, &text);
        if let Some(label) = &p.list {
            // `_add_list_item`: the item under the group open at its depth,
            // with a uniform paragraph's formatting and link.
            let group = self.list_group_for(label.depth);
            let (uniform, _) = uniform_run(&p.runs);
            self.tree.add_with_prov(
                Some(group),
                None,
                TreeKind::Text {
                    label: "list_item".into(),
                    text: text.clone(),
                    orig: None,
                    formatting: uniform.and_then(|r| r.fmt).map(tree_formatting),
                    hyperlink: uniform.and_then(|r| r.link.clone()),
                    level: None,
                    list: Some(ListMeta {
                        enumerated: label.enumerated,
                        marker: label.marker.clone(),
                    }),
                },
                prov,
            );
            let mut node = self.lists.item(p, label);
            if let (Node::ListItem { location, .. }, Some(g)) = (&mut node, geometry) {
                // The location rides on the item itself so consecutive items
                // still group into one DocLang `<list>`.
                *location = Some(self.location(g));
            }
            self.nodes.push(node);
            return;
        }
        self.close_lists();
        match p.label {
            Label::Title => {
                self.tree.add_with_prov(
                    Some(self.group),
                    None,
                    text_kind("title", &text, None, None, None),
                    prov,
                );
            }
            Label::Heading(level) => {
                self.tree.add_with_prov(
                    Some(self.group),
                    None,
                    text_kind("section_header", &text, None, None, Some(level)),
                    prov,
                );
            }
            Label::Text => {
                let group = self.group;
                self.add_runs(p, "text", None, group, Some(prov));
            }
        }
        self.push(paragraph_node(p), geometry);
    }

    /// `_add_runs` into the tree: a uniform paragraph is one item carrying
    /// its formatting and link; mixed runs become an `inline` group of items,
    /// the shape the Word and HTML backends give them. The provenance goes on
    /// the single item, or on each item of the group.
    fn add_runs(
        &mut self,
        p: &Paragraph,
        label: &str,
        layer: Option<ContentLayer>,
        parent: usize,
        prov: Option<TreeProv>,
    ) {
        let runs: Vec<&super::pages::Run> = p.runs.iter().filter(|r| !r.text.is_empty()).collect();
        let (uniform, is_uniform) = uniform_run(&p.runs);
        if is_uniform {
            let kind = text_kind(
                label,
                &p.text(),
                uniform.and_then(|r| r.fmt).map(tree_formatting),
                uniform.and_then(|r| r.link.clone()),
                None,
            );
            self.add_kind(Some(parent), layer, kind, prov);
            return;
        }
        let group = self.tree.add(
            Some(parent),
            layer,
            TreeKind::Group {
                label: "inline".into(),
                name: "group".into(),
            },
        );
        for run in runs {
            let kind = text_kind(
                label,
                &run.text,
                run.fmt.map(tree_formatting),
                run.link.clone(),
                None,
            );
            let run_prov = prov.as_ref().map(|pv| TreeProv {
                charspan: [0, run.text.chars().count()],
                ..pv.clone()
            });
            self.add_kind(Some(group), layer, kind, run_prov);
        }
    }

    fn add_kind(
        &mut self,
        parent: Option<usize>,
        layer: Option<ContentLayer>,
        kind: TreeKind,
        prov: Option<TreeProv>,
    ) -> usize {
        match prov {
            Some(prov) => self.tree.add_with_prov(parent, layer, kind, prov),
            None => self.tree.add(parent, layer, kind),
        }
    }

    /// A presenter note: a `text` on the notes layer under the slide, with a
    /// zero box — the note is not drawn on the slide at all.
    fn add_note(&mut self, note: &Paragraph) {
        let prov = self.prov(None, &note.text());
        let group = self.group;
        self.add_runs(note, "text", Some(ContentLayer::Notes), group, Some(prov));
        self.nodes.push(Node::Furniture {
            layer: ContentLayer::Notes,
            inner: Box::new(Node::Located {
                location: [0, 0, 0, 0],
                inner: Box::new(paragraph_node(note)),
            }),
        });
    }

    /// A comment: docling-core's `add_comment(text, parent=group)` — a bare
    /// `text` on the notes layer under the slide, with no provenance. Keynote
    /// draws a comment as a note stuck to the slide rather than a highlight
    /// over words, so there is nothing for it to annotate.
    fn add_comment(&mut self, comment: &Comment) {
        self.tree.add(
            Some(self.group),
            Some(ContentLayer::Notes),
            text_kind("text", &comment.text, None, None, None),
        );
        self.nodes.push(Node::Furniture {
            layer: ContentLayer::Notes,
            inner: Box::new(Node::Paragraph {
                text: super::markdown::escape_text(&comment.text),
            }),
        });
    }

    /// A flat node with the drawable's DocLang location, when it has one.
    fn push(&mut self, node: Node, geometry: Option<Geometry>) {
        match geometry {
            Some(g) => self.nodes.push(Node::Located {
                location: self.location(g),
                inner: Box::new(node),
            }),
            None => self.nodes.push(node),
        }
    }
}

fn text_kind(
    label: &str,
    text: &str,
    formatting: Option<TreeFormatting>,
    hyperlink: Option<String>,
    level: Option<u8>,
) -> TreeKind {
    TreeKind::Text {
        label: label.into(),
        text: text.into(),
        orig: None,
        formatting,
        hyperlink,
        level,
        list: None,
    }
}

/// The Pages content model's formatting as the tree's.
fn tree_formatting(f: Formatting) -> TreeFormatting {
    TreeFormatting {
        bold: f.bold,
        italic: f.italic,
        underline: f.underline,
        strikethrough: f.strike,
        script: f.script,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(left: f64, top: f64) -> Option<Geometry> {
        Some(Geometry {
            left,
            top,
            width: 10.0,
            height: 10.0,
        })
    }

    /// upstream's `test_reading_order_bands_a_row_and_reads_it_across`: two
    /// drawables within the tolerance share a row read left to right, a lower
    /// one follows.
    #[test]
    fn reading_order_bands_a_row_and_reads_it_across() {
        let placed = [at(300.0, 100.0), at(100.0, 102.0), at(100.0, 300.0)];
        assert_eq!(reading_order(&placed), vec![1, 0, 2]);
    }

    /// upstream's `test_reading_order_keeps_unplaced_drawables_last_and_in_order`.
    #[test]
    fn reading_order_keeps_unplaced_drawables_last_and_in_order() {
        let placed = [None, at(0.0, 50.0), None, at(0.0, 10.0)];
        assert_eq!(reading_order(&placed), vec![3, 1, 0, 2]);
    }

    /// The band follows the previous drawable, not the row's first: a run of
    /// shapes drifting down by less than the tolerance each stays one row.
    #[test]
    fn a_drifting_band_stays_one_row() {
        let placed = [at(200.0, 0.0), at(100.0, 3.0), at(0.0, 6.0)];
        assert_eq!(reading_order(&placed), vec![2, 1, 0]);
    }
}
