//! Apple Numbers — docling's `IWorkNumbersDocumentBackend` (docling#4113,
//! docling#5101, docling#5087; #1031): the content model both container
//! generations are read into (`iwork/numbers_content.py`) and its emission
//! into a `DoclingDocument`.
//!
//! Each sheet is a page and a `sheet` group. Tables, charts and pictures on
//! it become table and picture items in the order they are laid out down the
//! page; each sticky note becomes a comment in the notes layer. A table's
//! name is its caption; a cell filled with an image is a rich cell holding
//! the cell's text and the picture. Every item keeps the frame of the
//! drawable it came from as its provenance, top-left points on the sheet.
//!
//! The readers live beside this module: `numbers_iwa.rs` (Numbers 3+,
//! `Index/*.iwa`) and `numbers_xml.rs` (iWork '09, `index.xml`).

use docling_core::tree::{ItemTree, TreeKind, TreeProv};
use docling_core::{ContentLayer, DoclingDocument, Node, Table, TableCell};

use super::keynote::Geometry;
use super::pages::{Chart, Picture};

/// `MAX_TABLE_CELLS`: cells one table may declare before it is rejected as
/// implausible — the counts come from the document, and Numbers' own ceiling
/// is a million per table.
pub(crate) const MAX_TABLE_CELLS: usize = 4_000_000;

/// One cell of a table, already rendered to text; `picture` is the image the
/// cell is filled with (Numbers draws it behind the text).
#[derive(Debug, Clone)]
pub(crate) struct Cell {
    pub row: usize,
    pub col: usize,
    pub text: String,
    pub col_span: usize,
    pub picture: Option<Picture>,
}

/// One table on a sheet; `geometry` is its frame on the sheet canvas.
#[derive(Debug, Clone)]
pub(crate) struct SheetTable {
    pub name: String,
    pub num_rows: usize,
    pub num_cols: usize,
    pub header_rows: usize,
    pub header_cols: usize,
    pub cells: Vec<Cell>,
    pub geometry: Option<Geometry>,
}

/// One sticky note: Numbers 3+ records who left it and when, iWork '09
/// neither. `timestamp` is the ISO instant.
#[derive(Debug, Clone)]
pub(crate) struct Comment {
    pub text: String,
    pub author: String,
    pub timestamp: Option<String>,
    pub geometry: Option<Geometry>,
}

/// One sheet, with what is drawn on it, each kind sorted by [`sheet_order`].
#[derive(Debug, Clone, Default)]
pub(crate) struct Sheet {
    pub name: String,
    pub tables: Vec<SheetTable>,
    pub charts: Vec<(Chart, Option<Geometry>)>,
    pub pictures: Vec<(Picture, Option<Geometry>)>,
    pub comments: Vec<Comment>,
}

/// `sheet_order`: top edge, then left edge — Numbers keeps a sheet's
/// drawables in z-order, not reading order; an unplaced one sorts first.
pub(crate) fn sheet_order(geometry: Option<&Geometry>) -> (f64, f64) {
    geometry.map_or((0.0, 0.0), |g| (g.top, g.left))
}

/// Stable sort of `items` by the [`sheet_order`] of their geometry.
pub(crate) fn sort_by_sheet<T>(items: &mut [T], geometry: impl Fn(&T) -> Option<&Geometry>) {
    items.sort_by(|a, b| {
        sheet_order(geometry(a))
            .partial_cmp(&sheet_order(geometry(b)))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

enum Drawn {
    Table(SheetTable),
    Chart(Chart),
    Picture(Picture),
}

/// docling's `IWorkNumbersDocumentBackend.convert`. `page_range` is the
/// 1-based inclusive window of sheets to keep (a sheet keeps its number).
pub(crate) fn emit(
    sheets: Vec<Sheet>,
    page_range: Option<(usize, usize)>,
    doc: &mut DoclingDocument,
) {
    let mut tree = ItemTree {
        inline_whitespace: true,
        ..ItemTree::default()
    };
    let mut table_count = 0usize;
    for (ix, sheet) in sheets.into_iter().enumerate() {
        let page_no = ix + 1;
        if let Some((first, last)) = page_range {
            if page_no < first || page_no > last {
                continue;
            }
        }
        let (width, height) = sheet_extent(&sheet);
        doc.push(Node::PageInfo {
            page_no,
            width: width as f32,
            height: height as f32,
        });
        let name = if sheet.name.is_empty() {
            format!("Sheet {page_no}")
        } else {
            sheet.name.clone()
        };
        let group = tree.add(
            None,
            None,
            TreeKind::Group {
                label: "sheet".into(),
                name: name.clone(),
            },
        );
        // Tables, charts and pictures share the sheet canvas: one pass down
        // the page, not one kind after the other.
        let mut drawn: Vec<(Drawn, Option<Geometry>)> = Vec::new();
        drawn.extend(sheet.tables.into_iter().map(|t| {
            let g = t.geometry;
            (Drawn::Table(t), g)
        }));
        drawn.extend(sheet.charts.into_iter().map(|(c, g)| (Drawn::Chart(c), g)));
        drawn.extend(
            sheet
                .pictures
                .into_iter()
                .map(|(p, g)| (Drawn::Picture(p), g)),
        );
        sort_by_sheet(&mut drawn, |(_, g)| g.as_ref());
        let mut out = SheetOut {
            tree: &mut tree,
            group,
            page_no,
            size: (width, height),
            nodes: Vec::new(),
        };
        for (item, geometry) in drawn {
            match item {
                Drawn::Table(t) => {
                    table_count += 1;
                    out.add_table(t, table_count);
                }
                Drawn::Chart(c) => out.add_chart(c, geometry),
                Drawn::Picture(p) => out.add_picture(&p, geometry),
            }
        }
        let children = out.nodes;
        doc.push(Node::Group {
            label: "sheet".into(),
            name: Some(name),
            layer: None,
            children,
        });
        // `_add_sheet_comment`: each sticky note under a `comment_section` of
        // its own in the notes layer, with who left it and when.
        for (position, comment) in sheet.comments.iter().enumerate() {
            let mut meta = Vec::new();
            if !comment.author.is_empty() {
                meta.push(format!("author: {}", comment.author));
            }
            if let Some(time) = &comment.timestamp {
                meta.push(format!("time: {time}"));
            }
            let text = if meta.is_empty() {
                comment.text.clone()
            } else {
                format!("[{}]: {}", meta.join(", "), comment.text)
            };
            let section = tree.add(
                None,
                Some(ContentLayer::Notes),
                TreeKind::Group {
                    label: "comment_section".into(),
                    name: format!("comment-{}-{}", sheet.name, position + 1),
                },
            );
            tree.add(
                Some(section),
                Some(ContentLayer::Notes),
                text_kind("text", &text),
            );
            doc.push(Node::CommentSection {
                name: format!("comment-{}-{}", sheet.name, position + 1),
                text: super::markdown::escape_text(&text),
                refs_note_text: true,
                grouped: true,
            });
        }
        if ix > 0 {
            doc.push(Node::PageBreak);
        }
    }
    doc.tree = Some(tree);
}

/// `_sheet_extent`: how far a sheet's contents reach, from its top left.
fn sheet_extent(sheet: &Sheet) -> (f64, f64) {
    let geometries = sheet
        .tables
        .iter()
        .map(|t| t.geometry.as_ref())
        .chain(sheet.charts.iter().map(|(_, g)| g.as_ref()))
        .chain(sheet.pictures.iter().map(|(_, g)| g.as_ref()))
        .chain(sheet.comments.iter().map(|c| c.geometry.as_ref()));
    let (mut width, mut height) = (0f64, 0f64);
    for g in geometries.flatten() {
        width = width.max(g.left + g.width);
        height = height.max(g.top + g.height);
    }
    (width, height)
}

fn text_kind(label: &str, text: &str) -> TreeKind {
    TreeKind::Text {
        label: label.into(),
        text: text.into(),
        orig: None,
        formatting: None,
        hyperlink: None,
        level: None,
        list: None,
    }
}

struct SheetOut<'t> {
    tree: &'t mut ItemTree,
    group: usize,
    page_no: usize,
    size: (f64, f64),
    nodes: Vec<Node>,
}

impl SheetOut<'_> {
    /// `_sheet_prov`: the sheet's page and the drawable's box — an empty one
    /// for a drawable Numbers did not position — with a span over `text`.
    fn prov(&self, geometry: Option<&Geometry>, text: &str) -> TreeProv {
        TreeProv {
            page_no: self.page_no,
            bbox: geometry
                .map(|g| [g.left, g.top, g.left + g.width, g.top + g.height])
                .unwrap_or([0.0; 4]),
            bottom_left: false,
            charspan: [0, text.chars().count()],
        }
    }

    /// The DocLang `<location>` of a drawable on the 0–511 grid of the sheet.
    fn location(&self, g: &Geometry) -> [u16; 4] {
        let (w, h) = self.size;
        let grid = |v: f64, extent: f64| -> u16 {
            if extent <= 0.0 {
                return 0;
            }
            (v / extent * 511.0).round().clamp(0.0, 511.0) as u16
        };
        [
            grid(g.left, w),
            grid(g.top, h),
            grid(g.left + g.width, w),
            grid(g.top + g.height, h),
        ]
    }

    fn push(&mut self, node: Node, geometry: Option<&Geometry>) {
        match geometry {
            Some(g) => {
                let location = self.location(g);
                self.nodes.push(Node::Located {
                    location,
                    inner: Box::new(node),
                })
            }
            None => self.nodes.push(node),
        }
    }

    /// `_add_sheet_table`. `ordinal` is docling's `len(doc.tables)` once the
    /// table is added, which names its rich-cell groups.
    fn add_table(&mut self, t: SheetTable, ordinal: usize) {
        let geometry = t.geometry;
        let prov = self.prov(geometry.as_ref(), "");
        let captions: Vec<usize> = (!t.name.is_empty())
            .then(|| {
                let prov = self.prov(geometry.as_ref(), &t.name);
                self.tree
                    .add_with_prov(Some(self.group), None, text_kind("caption", &t.name), prov)
            })
            .into_iter()
            .collect();
        let mut table = grid(&t);
        let id = self.tree.add_with_prov(
            Some(self.group),
            None,
            TreeKind::Table {
                table: table.clone(),
                rich_cells: Vec::new(),
                captions,
                footnotes: Vec::new(),
            },
            prov.clone(),
        );
        // A cell filled with a picture is a rich cell: a group under the
        // table holds the cell's text and its picture, both in the frame of
        // the table.
        let mut rich = Vec::new();
        let mut blocks: Vec<Vec<Vec<Node>>> = vec![vec![Vec::new(); t.num_cols]; t.num_rows];
        for cell in t.cells.iter().filter(|c| c.picture.is_some()) {
            let group = self.tree.add(
                Some(id),
                None,
                TreeKind::Group {
                    label: "unspecified".into(),
                    name: format!("rich_cell_group_{ordinal}_{}_{}", cell.col, cell.row),
                },
            );
            let mut cell_nodes = Vec::new();
            if !cell.text.is_empty() {
                let p = self.prov(geometry.as_ref(), &cell.text);
                self.tree
                    .add_with_prov(Some(group), None, text_kind("text", &cell.text), p);
                cell_nodes.push(Node::Paragraph {
                    text: super::markdown::escape_text(&cell.text),
                });
            }
            let image = cell.picture.as_ref().and_then(picture_image);
            self.tree.add_with_prov(
                Some(group),
                None,
                picture_kind(image.clone(), Vec::new(), None, None),
                prov.clone(),
            );
            cell_nodes.push(picture_node(image));
            rich.push((cell.row, cell.col, group));
            if let Some(slot) = blocks.get_mut(cell.row).and_then(|r| r.get_mut(cell.col)) {
                *slot = cell_nodes;
            }
            if let Some(slot) = table
                .rows
                .get_mut(cell.row)
                .and_then(|r| r.get_mut(cell.col))
            {
                *slot = if cell.text.is_empty() {
                    "<!-- image -->".to_string()
                } else {
                    format!("{}  <!-- image -->", cell.text)
                };
            }
        }
        if !rich.is_empty() {
            table.cell_blocks = Some(blocks);
        }
        if let TreeKind::Table {
            rich_cells,
            table: tree_table,
            ..
        } = &mut self.tree.items[id].kind
        {
            *rich_cells = rich;
            tree_table.cell_blocks = table.cell_blocks.clone();
        }
        table.caption = (!t.name.is_empty()).then(|| super::markdown::escape_text(&t.name));
        self.push(Node::Table(table), geometry.as_ref());
    }

    /// `_add_chart`: a picture classified by the chart's kind, carrying its
    /// data as a table, captioned with its title — as on a Keynote slide.
    fn add_chart(&mut self, chart: Chart, geometry: Option<Geometry>) {
        let caption: Vec<usize> = chart
            .title
            .as_deref()
            .map(|title| {
                let prov = self.prov(geometry.as_ref(), title);
                self.tree
                    .add_with_prov(Some(self.group), None, text_kind("caption", title), prov)
            })
            .into_iter()
            .collect();
        let table = chart.table();
        let prov = self.prov(geometry.as_ref(), "");
        self.tree.add_with_prov(
            Some(self.group),
            None,
            picture_kind(None, caption, Some(chart.label.clone()), table.clone()),
            prov,
        );
        self.push(
            Node::Chart {
                kind: chart.label,
                table: table.unwrap_or_default(),
                caption: chart.title,
                location: None,
            },
            geometry.as_ref(),
        );
    }

    /// `_add_picture`.
    fn add_picture(&mut self, picture: &Picture, geometry: Option<Geometry>) {
        let image = picture_image(picture);
        let prov = self.prov(geometry.as_ref(), "");
        self.tree.add_with_prov(
            Some(self.group),
            None,
            picture_kind(image.clone(), Vec::new(), None, None),
            prov,
        );
        self.push(picture_node(image), geometry.as_ref());
    }
}

fn picture_image(picture: &Picture) -> Option<docling_core::PictureImage> {
    picture
        .data
        .as_ref()
        .and_then(|d| super::ooxml::picture_image(&picture.name, d.clone()))
}

fn picture_kind(
    image: Option<docling_core::PictureImage>,
    captions: Vec<usize>,
    classification: Option<String>,
    chart: Option<Table>,
) -> TreeKind {
    TreeKind::Picture {
        captions,
        image,
        classification,
        extra_classes: Vec::new(),
        description: None,
        confidence: None,
        chart,
        dpi: None,
    }
}

fn picture_node(image: Option<docling_core::PictureImage>) -> Node {
    Node::Picture {
        caption: None,
        caption_href: None,
        image,
        classification: None,
        description: None,
        caption_parent: Default::default(),
        caption_location: None,
    }
}

/// The table's grid: every cell at its place, spans repeated across the
/// columns they cover, with docling's header flags — a column header in the
/// header rows, a row header in the header columns below them.
fn grid(t: &SheetTable) -> Table {
    let mut rows = vec![vec![String::new(); t.num_cols]; t.num_rows];
    let mut cells = Vec::with_capacity(t.cells.len());
    for cell in &t.cells {
        if cell.row >= t.num_rows || cell.col >= t.num_cols {
            continue;
        }
        let span = cell.col_span.max(1).min(t.num_cols - cell.col);
        for c in cell.col..cell.col + span {
            rows[cell.row][c] = cell.text.clone();
        }
        cells.push(TableCell {
            text: cell.text.clone(),
            bbox: None,
            start_row: cell.row,
            start_col: cell.col,
            row_span: 1,
            col_span: span,
            column_header: cell.row < t.header_rows,
            row_header: cell.row >= t.header_rows && cell.col < t.header_cols,
            row_section: false,
        });
    }
    Table {
        rows,
        cells: Some(cells),
        ..Default::default()
    }
}
