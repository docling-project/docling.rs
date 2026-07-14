//! The unified document representation.

use crate::markdown::{to_markdown, to_markdown_images};
use crate::ImageMode;

/// The unified, format-agnostic document produced by every backend.
///
/// This is the heart of docling: backends parse their source format into a
/// `DoclingDocument`, and serializers turn it back into Markdown, HTML, JSON,
/// etc. Phase 0 uses a flat sequence of [`Node`]s; the production schema will
/// match docling-core's body-tree-with-references layout.
#[derive(Debug, Clone, PartialEq)]
pub struct DoclingDocument {
    /// Logical document name (usually the input file stem).
    pub name: String,
    /// Top-level content, in reading order.
    pub nodes: Vec<Node>,
    /// Default Markdown export mode for [`Self::export_to_markdown`]. `false`
    /// (the default) reproduces docling's legacy output byte-for-byte; `true`
    /// emits cleaner, more conformant Markdown. Set by `DocumentConverter`.
    pub strict_markdown: bool,
    /// Emit tables in the compact `| a | b |` / `| - | - |` form rather than
    /// docling-core's width-padded GitHub serializer. The PDF backend sets this
    /// (its committed groundtruth corpus predates the padded serializer); DOCX/HTML
    /// leave it `false` to match current published docling.
    pub compact_tables: bool,
    /// Hyperlinks recovered from the source, as `(anchor_text, href)` pairs in
    /// document order. docling's standard pipeline drops PDF link annotations, so
    /// these are rendered as Markdown `[anchor](href)` **only in strict mode**
    /// (legacy/docling output is left byte-for-byte unchanged). The PDF backend
    /// populates this from pdfium link annotations; other backends leave it empty.
    pub links: Vec<(String, String)>,
}

/// A single piece of document content.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// A heading. `level` is 1-6.
    Heading { level: u8, text: String },
    /// A run of body text.
    Paragraph { text: String },
    /// A form checkbox (docling's `checkbox_selected`/`checkbox_unselected`): its
    /// clean label `text` with the checked state. DocLang emits a `<checkbox>`
    /// element head; Markdown/JSON render the task-list form (`- [x] `/`- [ ] `).
    CheckboxItem { checked: bool, text: String },
    /// A single list item at the given nesting `level` (0 = top). For ordered
    /// items, `number` is the display number (honoring the list's `start`); it
    /// is unused for unordered items. `first_in_list` marks the first item of a
    /// list so the serializer can blank-line-separate adjacent sibling lists.
    ///
    /// `marker` is the DocLang enumeration marker (`"1."`, `"1.1."`, …) when the
    /// backend provides one — HTML and DOCX set it for enumerated items, so
    /// DocLang emits `<ldiv><marker>…</marker></ldiv>`; Markdown and the other
    /// declarative backends leave it `None`, giving a bare `<ldiv/>` (matching
    /// docling, whose Markdown backend passes no marker).
    ListItem {
        ordered: bool,
        number: u64,
        first_in_list: bool,
        text: String,
        level: u8,
        marker: Option<String>,
        /// Optional layout provenance (`x0,y0,x1,y1`, normalized to 0–511): the
        /// four DocLang `<location>` values emitted inside the `<list>` right
        /// after the item's `<ldiv>`. Set only by backends with real geometry
        /// (e.g. PPTX shapes); `None` for the declarative backends. Kept on the
        /// item itself (rather than a [`Node::Located`] wrapper) so consecutive
        /// items still group into one `<list>`.
        location: Option<[u16; 4]>,
        /// DocLang-only override for items whose DocLang form diverges from their
        /// flat Markdown `text`. Markdown/JSON always render the fields above; the
        /// DocLang serializer, when this is `Some`, takes the list kind, marker,
        /// and content from here instead. Used for docx multilevel numbering
        /// (Markdown shows `- 1.1. x`, DocLang an ordered `<marker>1.1.</marker>`
        /// with clean text) and inline equations/formatting in list items.
        dclx: Option<ListItemDclx>,
        /// The item's hyperlink target, when its content is a link — docling's
        /// HTML backend emits it as an `<href uri=…/>` in the item head, and the
        /// anchor's Markdown link markup is stripped from the rendered content.
        /// `None` for a plain item; ignored by Markdown/JSON.
        href: Option<String>,
        /// Non-body content layer (docling's HTML site chrome before the first
        /// heading → `furniture`). DocLang emits a `<layer value=…/>` in the item
        /// head; Markdown/JSON drop a non-body item entirely.
        layer: Option<ContentLayer>,
    },
    /// A fenced code block.
    Code {
        language: Option<String>,
        text: String,
    },
    /// A table. The first row is treated as the header.
    Table(Table),
    /// A picture/figure, with an optional caption and (when a backend extracts
    /// it) the embedded image itself.
    Picture {
        caption: Option<String>,
        image: Option<PictureImage>,
    },
    /// A chart (docling's `PictureItem` classified as a chart, carrying a
    /// `PictureTabularChartData` annotation). Markdown and JSON render it exactly
    /// like a [`Node::Picture`] placeholder (an `<!-- image -->` / `picture`
    /// item); the DocLang serializer emits `<picture class="chart">` with a
    /// `<label value="{kind}"/>` and the data `table` as a `<tabular>`.
    Chart {
        /// docling's classification label, e.g. `bar_chart`, `line_chart`.
        kind: String,
        /// The chart's data grid (row 0 is the header band).
        table: Table,
        /// The chart title (docling's caption item on the picture).
        caption: Option<String>,
        /// DocLang `<location>` provenance for the picture element.
        location: Option<[u16; 4]>,
    },
    /// A logical grouping of child nodes (e.g. a list, a section).
    Group { label: String, children: Vec<Node> },
    /// A form key-value region (docling's `field_region`): a set of form fields,
    /// each pairing an optional marker, key, and value. Backends detect these
    /// from form structure (e.g. HTML's `keyN` / `keyN_valueM` / `keyN_marker`
    /// `id`-convention); the serializers render each item's parts as separate
    /// labelled texts (`marker` / `field_key` / `field_value`).
    FieldRegion { items: Vec<FieldItem> },
    /// Rich inline content — docling's `InlineGroup`: a run of styled text
    /// segments that a backend captured with formatting (`<bold>`, `<italic>`,
    /// `<underline>`, `<strikethrough>`, sub/superscript, inline `<code>`) the
    /// flat Markdown text cannot represent. Markdown/JSON render this exactly
    /// like `Paragraph { text: md_text }` (so their output is unchanged); the
    /// DocLang serializer uses the structured `runs`. `unwrapped` is set when the
    /// group's docling parent is a heading/text (no enclosing `<text>` wrapper).
    InlineGroup {
        unwrapped: bool,
        runs: Vec<InlineRun>,
        md_text: String,
    },
    /// A node in a non-body content layer — `furniture` (page headers/footers,
    /// the HTML `<title>`, site navigation/chrome) or `notes` (docx comments).
    /// Markdown and JSON omit these layers by default; DocLang renders the wrapped
    /// node with a `<layer value="{layer}"/>` head.
    Furniture {
        layer: ContentLayer,
        inner: Box<Node>,
    },
    /// A node carrying layout provenance — the four DocLang `<location>` values
    /// (`x0,y0,x1,y1`, normalized to 0–511) docling attaches to elements from
    /// backends with real geometry (e.g. the slide shapes in PPTX). Markdown and
    /// JSON render the wrapped node unchanged; DocLang emits the `<location>`
    /// tokens as the element's first children.
    Located {
        location: [u16; 4],
        inner: Box<Node>,
    },
    /// A PDF page header or footer (docling's `page_header`/`page_footer`
    /// furniture): DocLang emits `<page_header>`/`<page_footer>` with a
    /// `<layer value="furniture"/>` head, the four `<location>` tokens, then the
    /// text. Markdown and JSON omit it like other furniture.
    PageFurniture {
        footer: bool,
        location: [u16; 4],
        text: String,
    },
    /// A page boundary — docling's implicit page break between pages. The PPTX
    /// backend emits one between consecutive slides. DocLang renders it as
    /// `<page_break/>`; Markdown and JSON omit it (matching docling's default
    /// exports, which carry page breaks only in the document model).
    PageBreak,
    /// A node docling keeps in the document model (and DocLang) but leaves out
    /// of the Markdown and JSON exports — e.g. an ODF *presentation*'s pictures
    /// and charts, which appear in the `.dclx` body but not in its `.md`/`.json`.
    /// DocLang renders the wrapped node in place; Markdown and JSON skip it.
    DoclangOnly(Box<Node>),
    /// A verbatim plain-text dump — docling's plain-text backend emits the whole
    /// file as a single text item (used for legacy USPTO APS `.txt` grants, which
    /// docling routes to plain text rather than its APS parser). The stored string
    /// is the file body, one record per line. Markdown/JSON render it as one text
    /// block; the DocLang serializer reproduces minidom's per-line layout, CDATA-
    /// escaping only the lines that need it (see `emit_text_dump`).
    TextDump(String),
}

/// Vertical text position of an [`InlineRun`] — docling's `Script`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Script {
    #[default]
    Baseline,
    Sub,
    Super,
}

/// One styled segment of a [`Node::InlineGroup`] — the docling.rs analogue of a
/// `TextItem` inside an `InlineGroup`, carrying the ancestor formatting docling
/// tracks. `text` is already whitespace-normalized/trimmed (one segment per
/// source text node). A hyperlink is intentionally not stored: DocLang drops the
/// target inside inline scope, keeping only the anchor text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InlineRun {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub script: Script,
    pub code: bool,
    /// An inline equation (`text` holds LaTeX): DocLang renders `<formula>…`,
    /// Markdown/JSON keep the `$…$` already baked into the group's `md_text`.
    pub formula: bool,
}

/// A DocLang content layer other than the default `body` (see [`Node::Furniture`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentLayer {
    /// Page headers/footers, HTML `<title>`, site navigation/chrome.
    Furniture,
    /// Editorial notes (docx reviewer comments).
    Notes,
    /// Invisible content (hidden spreadsheet sheets).
    Invisible,
}

impl ContentLayer {
    /// The `<layer value="…"/>` token value.
    pub fn value(self) -> &'static str {
        match self {
            ContentLayer::Furniture => "furniture",
            ContentLayer::Notes => "notes",
            ContentLayer::Invisible => "invisible",
        }
    }
}

/// DocLang-only content for a [`Node::ListItem`] whose DocLang form differs from
/// its flat Markdown `text` (see [`Node::ListItem::dclx`]). `ordered` picks the
/// enclosing `<list>` kind, `marker` the `<ldiv><marker>`; content is `runs`
/// (structured equations/formatting) when non-empty, else `text` re-parsed for
/// inline markers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListItemDclx {
    pub ordered: bool,
    pub marker: Option<String>,
    pub text: String,
    pub runs: Vec<InlineRun>,
}

impl InlineRun {
    /// A run with no active formatting (renders as bare inline text).
    pub fn is_plain(&self) -> bool {
        !self.bold
            && !self.italic
            && !self.underline
            && !self.strike
            && !self.code
            && !self.formula
            && self.script == Script::Baseline
    }
}

/// Build the [`Node`] for a paragraph of inline content from its structured
/// `runs` and Markdown text, applying docling's `InlineGroup` boundary:
///
/// * a single plain run (or none) → a plain [`Node::Paragraph`] (which the
///   serializers render as `<text>…</text>`, and a lone hyperlink via `<href>`);
/// * a single uniformly-formatted run, or two or more runs → a
///   [`Node::InlineGroup`]. `unwrapped` (the group's docling parent is a
///   heading, so no enclosing `<text>`) only applies to multi-run groups.
///
/// Markdown/JSON render the group's `md_text`, so their output is identical to
/// emitting a `Paragraph` — the structured runs are DocLang-only.
pub fn inline_paragraph_node(md_text: String, runs: Vec<InlineRun>, unwrapped: bool) -> Node {
    let single_plain = runs.len() <= 1 && runs.first().map_or(true, |r| r.is_plain());
    if single_plain {
        Node::Paragraph { text: md_text }
    } else {
        Node::InlineGroup {
            unwrapped: unwrapped && runs.len() >= 2,
            runs,
            md_text,
        }
    }
}

/// One entry of a [`Node::FieldRegion`]: a marker/key/value triple, any of which
/// may be absent. Mirrors docling's `field_item` with its `marker` / `field_key`
/// / `field_value` child texts.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FieldItem {
    pub marker: Option<String>,
    pub key: Option<String>,
    pub value: Option<String>,
}

/// An extracted picture's raw encoded bytes plus its mimetype and pixel size —
/// the docling.rs analogue of docling-core's `ImageRef`.
#[derive(Debug, Clone, PartialEq)]
pub struct PictureImage {
    /// e.g. `image/png`, `image/jpeg`.
    pub mimetype: String,
    pub width: u32,
    pub height: u32,
    /// The image file bytes, exactly as embedded (PNG/JPEG/…).
    pub data: Vec<u8>,
}

impl PictureImage {
    /// A `data:` URI for the image (`data:<mimetype>;base64,<…>`).
    pub fn data_uri(&self) -> String {
        format!(
            "data:{};base64,{}",
            self.mimetype,
            crate::base64::encode(&self.data)
        )
    }
}

/// A simple row-major table. By default `rows[0]` is the header row; a
/// [`TableStructure`] overlay overrides that and adds column spans.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Table {
    pub rows: Vec<Vec<String>>,
    /// Optional layout provenance: the four DocLang `<location>` values
    /// (`x0,y0,x1,y1`, each already normalized to the 0–511 resolution) emitted
    /// before the table's cells. Set only by backends with real geometry (e.g.
    /// the spreadsheet backend, whose cell grid yields a bounding box); left
    /// `None` by declarative backends, which have no coordinates.
    pub location: Option<[u16; 4]>,
    /// Optional OTSL structure overlay for backends that parse real table
    /// geometry (USPTO CALS): explicit header-row count and horizontal-span
    /// continuations. `None` → the default (row 0 is the header, no spans).
    /// `rows` still carries the full text grid (span text replicated) for
    /// Markdown/JSON; DocLang uses this overlay to emit `<ched/>`/`<lcel/>`.
    pub structure: Option<TableStructure>,
    /// Optional per-cell block content, parallel to `rows`. A *rich* cell (an
    /// ODF cell holding a list, several paragraphs, or a nested table) carries
    /// its DocLang blocks here; the DocLang serializer emits them after the
    /// cell token instead of the flat `rows` text. Markdown/JSON ignore this
    /// and render `rows`, so their output is unchanged. `None` (or an empty
    /// `Vec` for a given cell) → the flat text is used everywhere.
    pub cell_blocks: Option<Vec<Vec<Vec<Node>>>>,
}

/// OTSL structure overlay for a [`Table`], parallel to [`Table::rows`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TableStructure {
    /// Per-row: `true` if the row's non-empty cells are column headers
    /// (emitted as `<ched/>` rather than `<fcel/>`).
    pub header_row: Vec<bool>,
    /// Same shape as [`Table::rows`]; `true` where a cell continues a
    /// horizontal span from its left neighbour (emitted as `<lcel/>`).
    pub col_continuation: Vec<Vec<bool>>,
    /// Same shape as [`Table::rows`]; `true` where a cell continues a
    /// vertical span from the cell above (emitted as `<ucel/>`). Empty or all
    /// `false` when the backend has no vertical spans (e.g. USPTO CALS).
    pub row_continuation: Vec<Vec<bool>>,
    /// Same shape as [`Table::rows`]; `true` where a non-empty cell is a row
    /// header (emitted as `<rhed/>`) — a chart's category column. Empty when
    /// the table has no row headers.
    pub row_header: Vec<Vec<bool>>,
    /// Same shape as [`Table::rows`]; `true` where a cell is a *column header*
    /// cell (an HTML `<th>`). When non-empty this per-cell grid supersedes the
    /// per-row [`Self::header_row`] for `<ched/>` emission, matching docling's
    /// cell-level `column_header` flag; the chunker derives its header-row
    /// count from it.
    pub col_header: Vec<Vec<bool>>,
}

impl DoclingDocument {
    /// Create an empty document with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            nodes: Vec::new(),
            strict_markdown: false,
            compact_tables: false,
            links: Vec::new(),
        }
    }

    /// Append a node.
    pub fn push(&mut self, node: Node) {
        self.nodes.push(node);
    }

    /// Convenience: append a heading.
    pub fn add_heading(&mut self, level: u8, text: impl Into<String>) {
        self.push(Node::Heading {
            level,
            text: text.into(),
        });
    }

    /// Convenience: append a paragraph.
    pub fn add_paragraph(&mut self, text: impl Into<String>) {
        self.push(Node::Paragraph { text: text.into() });
    }

    /// Serialize the document to Markdown.
    ///
    /// The Rust equivalent of docling-core's
    /// `DoclingDocument.export_to_markdown()`. Uses [`Self::strict_markdown`] to
    /// pick between docling-legacy output (default) and the cleaner, more
    /// conformant variant.
    pub fn export_to_markdown(&self) -> String {
        to_markdown(self, self.strict_markdown)
    }

    /// Serialize to Markdown, explicitly choosing the mode regardless of
    /// [`Self::strict_markdown`]. `strict = true` produces cleaner, more
    /// conformant Markdown (code-fence languages preserved, no inline-run
    /// spacing artifacts); `strict = false` reproduces docling's legacy output.
    pub fn export_to_markdown_with(&self, strict: bool) -> String {
        to_markdown(self, strict)
    }

    /// Serialize to docling-core's native JSON wire format (`DoclingDocument`
    /// schema), pretty-printed — the Rust equivalent of
    /// `DoclingDocument.export_to_dict()` / `save_as_json()`. The output loads
    /// back into Python docling-core and round-trips to the same Markdown.
    pub fn export_to_json(&self) -> String {
        serde_json::to_string_pretty(&crate::json::to_json(self))
            .expect("DoclingDocument JSON is always serializable")
    }

    /// Serialize to DocLang XML (`<doclang version="0.7">…`), the markup that
    /// lives inside a `.dclx` archive — the Rust counterpart of docling-core's
    /// `export_to_doclang()` with default parameters. No trailing newline; the
    /// archive writer appends exactly one.
    pub fn export_to_doclang(&self) -> String {
        crate::doclang::export_to_doclang(&self.nodes)
    }

    /// Serialize to Markdown with an explicit picture [`ImageMode`] (mirrors
    /// docling's `image_mode`). Returns the Markdown and, for
    /// [`ImageMode::Referenced`], the `(relative-path, bytes)` of each image the
    /// caller should write next to the Markdown file. `artifacts_dir` is the
    /// directory name used in referenced links.
    pub fn export_to_markdown_with_images(
        &self,
        image_mode: ImageMode,
        artifacts_dir: &str,
    ) -> (String, Vec<(String, Vec<u8>)>) {
        to_markdown_images(self, self.strict_markdown, image_mode, artifacts_dir)
    }
}
