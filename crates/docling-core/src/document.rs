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
    /// Render a picture's author-written alt text (#676: the `descr` /
    /// `title` of an OOXML picture's non-visual properties, carried as a
    /// [`PictureDescription`] whose provenance is
    /// [`PictureDescription::ALT_TEXT`]) in the Markdown and HTML exports
    /// (the `![alt](…)` of an image line or the line above a placeholder;
    /// the `<img alt>` and the description text). `false` (the default)
    /// keeps it to the JSON (and DCLX) output, so the default Markdown and
    /// HTML stay byte-for-byte docling's; strict Markdown renders it
    /// regardless. Set by `DocumentConverter::picture_alt_text`.
    pub picture_alt_text: bool,
    /// What the file says about itself (#679): an EPUB's Dublin Core, an
    /// Office package's properties, a photo's EXIF, an audio file's tags —
    /// see [`crate::metadata`]. Read by the converter when
    /// [`metadata_output`](Self::metadata_output) asks for it (a document
    /// read from docling JSON keeps the `metadata` the JSON carried).
    pub metadata: crate::metadata::DocumentMetadata,
    /// Where [`metadata`](Self::metadata) goes in the exports: nowhere (the
    /// default — every export stays docling's), the JSON's `metadata`
    /// object, or that and YAML front matter at the top of the Markdown.
    /// Set by `DocumentConverter::metadata_output`.
    pub metadata_output: crate::metadata::MetadataOutput,
    /// Text the Markdown export inserts between two pages — docling-core's
    /// `MarkdownParams.page_break_placeholder` (e.g. `"<!-- page break -->"`).
    /// `None` (the default) omits page breaks from Markdown, as docling does.
    /// Set by `DocumentConverter::page_break_placeholder`. See
    /// [`crate::markdown`] for where a break lands (only between two rendered
    /// blocks that sit on different pages, never leading or trailing).
    pub page_break_placeholder: Option<String>,
    /// Hyperlinks recovered from the source, as `(anchor_text, href)` pairs in
    /// document order. docling's standard pipeline drops PDF link annotations, so
    /// these are rendered as Markdown `[anchor](href)` **only in strict mode**
    /// (legacy/docling output is left byte-for-byte unchanged). The PDF backend
    /// populates this from pdfium link annotations; other backends leave it empty.
    pub links: Vec<(String, String)>,
    /// Conversion-confidence report (#183), populated by the PDF/image ML
    /// pipeline; `None` for declarative conversions. Deliberately **not**
    /// part of any document export (docling keeps it on the conversion
    /// result, outside the document schema) — docling-serve surfaces it in
    /// the HTTP response instead.
    pub confidence: Option<crate::confidence::ConfidenceReport>,
    /// docling's item tree, when the backend built one (the HTML backend
    /// does): the JSON export serializes it instead of deriving a tree from
    /// `nodes`, so the JSON carries upstream's exact parent/child structure,
    /// item numbering, inline groups, formatting and content layers. Every
    /// other serializer reads `nodes`. See [`crate::tree`].
    pub tree: Option<crate::tree::ItemTree>,
    /// Rendered page images by 1-based page number — docling's
    /// `PageItem.image`, filled by the PDF/image pipeline only when page
    /// images are requested (docling's `generate_page_images`, #520). The
    /// JSON export writes each as the page's `image`, so docling-core's
    /// `TableItem.get_image` / `FormulaItem.get_image` can crop from it.
    /// Empty otherwise; no other export reads it.
    pub page_images: std::collections::BTreeMap<usize, PictureImage>,
    /// What the reader ran into and read around (#912): a limit it hit —
    /// an HTML page nested past `DOCLING_RS_MAX_HTML_DEPTH`, read as flat
    /// text with no item tree and no source blocks — named, with the
    /// effective limit, so a caller sees why the document lacks what it
    /// lacks instead of an unexplained success. The converter reports each
    /// as a `partial_success` error item.
    pub notices: Vec<Notice>,
}

/// Something the reader did instead of what it does by default (#912),
/// with the reason — see [`DoclingDocument::notices`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub kind: NoticeKind,
    /// One line: what was hit, the effective limit, what the document
    /// lacks because of it.
    pub message: String,
}

/// What a [`Notice`] is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoticeKind {
    /// The source nests deeper than the cap `variable` sets (its effective
    /// value `limit`): it was read as flat text — the content is there,
    /// the item tree and the source blocks are not.
    DepthLimit {
        /// The environment variable that sets the cap.
        variable: &'static str,
        limit: usize,
    },
}

impl Notice {
    /// A [`NoticeKind::DepthLimit`] notice for `what` (the format read).
    pub fn depth_limit(what: &str, variable: &'static str, limit: usize) -> Self {
        Notice {
            kind: NoticeKind::DepthLimit { variable, limit },
            message: format!(
                "{what} nested deeper than {limit} elements ({variable}): read as flat text, \
                 without the item tree and the source blocks"
            ),
        }
    }
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
        /// The original (pre-enrichment) text when the CodeFormula model
        /// rewrote `text`: docling keeps the raw extraction in the JSON `orig`
        /// field while `text` carries the model output. `None` → `orig == text`.
        orig: Option<String>,
        /// A line-preserving rendering, when the backend can reconstruct one
        /// but docling's own output for the format cannot. The PDF pipeline
        /// sets it (docling-parse joins code lines with single spaces, so
        /// `text` carries that flat docling-parity form): **strict** Markdown
        /// prefers `pretty`, every byte-conformance surface (legacy Markdown,
        /// JSON, DocLang, chunks) serializes `text`.
        pretty: Option<String>,
    },
    /// A table. The first row is treated as the header.
    Table(Table),
    /// A picture/figure, with an optional caption and (when a backend extracts
    /// it) the embedded image itself.
    Picture {
        caption: Option<String>,
        /// Hyperlink annotation on the caption (docling's caption text item
        /// `hyperlink`): the HTML backend sets it when an `<a href>` wraps the
        /// image whose `alt` became the caption. DocLang emits the block-form
        /// `<caption>` with an `<href uri=…/>` head; JSON puts `hyperlink` on
        /// the caption item; Markdown and LaTeX print the plain caption text,
        /// as docling does.
        caption_href: Option<String>,
        image: Option<PictureImage>,
        /// DocumentPictureClassifier predictions (all classes, descending
        /// confidence), when the picture-classification enrichment ran.
        /// Serialized as docling's `classification` annotation + `meta` field
        /// on the JSON picture item; Markdown/DocLang output is unaffected.
        classification: Option<Vec<PictureClass>>,
        /// Text read off the embedded image by the picture-OCR enrichment
        /// (#645) — docling's `PictureDescriptionData` annotation /
        /// `meta.description`, which is where upstream's picture-description
        /// models (VLM captioning) put their text too. JSON and DCLX carry it
        /// structurally; Markdown prints it between the caption and the
        /// image placeholder, where docling's picture serializer renders
        /// annotations. `None` when the enrichment did not run or read no
        /// text, so every default export stays unchanged.
        description: Option<PictureDescription>,
        /// Where the caption item hangs in the JSON tree (#390); see
        /// [`CaptionParent`]. Markdown, DocLang and LaTeX ignore it.
        caption_parent: CaptionParent,
        /// The caption's own box on the picture's page (0–511 grid, like
        /// [`Node::Located`]) — the PDF pipeline's caption layout region. The
        /// JSON export writes it as the caption item's `prov` (#609), as
        /// docling's `ReadingOrderModel._add_caption_or_footnote` does; `None`
        /// (every declarative backend) leaves the caption without one.
        caption_location: Option<[u16; 4]>,
    },
    /// A display-math formula item decoded by the CodeFormula enrichment:
    /// `latex` is the model's LaTeX (no `$$` wrapping), `orig` the raw glyph
    /// text extracted from the PDF. Markdown renders `$$latex$$`; JSON emits a
    /// `formula` text item (docling's un-enriched pipeline instead emits a
    /// placeholder paragraph — see the PDF assembler).
    Formula {
        latex: String,
        orig: String,
        location: Option<[u16; 4]>,
    },
    /// A standalone caption item (docling's `DocItemLabel.CAPTION` text that
    /// no picture or table claims): the HTML backend emits a `<figure>`'s
    /// `<figcaption>` this way when the figure produced no picture and its
    /// first item is not a table (docling#4050). `href` is the caption's
    /// hyperlink annotation (the first link inside the figcaption); Markdown
    /// renders it as `[text](href)`, JSON puts `hyperlink` on the caption
    /// item, DocLang emits the block-form `<caption>` with an `<href>` head.
    Caption { text: String, href: Option<String> },
    /// A body text item with a docling label of its own and an optional
    /// hyperlink — the PDF pipeline's `footnote` regions, whose item docling
    /// keeps as a `footnote` text with the link annotation covering it as its
    /// `hyperlink` (`PageAssembleModel._match_hyperlink`). The JSON writes
    /// exactly that (label, raw text, `hyperlink`); every other serializer
    /// renders it as the paragraph [`Self::labeled_markdown`] spells —
    /// docling's Markdown wraps a linked item's whole text, `[text](uri)`.
    LabeledText {
        label: String,
        text: String,
        href: Option<String>,
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
    /// A logical grouping of child nodes (e.g. a list, a section, a spreadsheet
    /// sheet). `name` is docling's group name when it differs from the label —
    /// an xlsx sheet group is `label: "sheet"`, `name: Some("Sheet1")`. `layer`
    /// puts the group *and* everything it contains on a non-body content layer
    /// (a hidden sheet is `invisible`), which is how the serializers that only
    /// render body content know to skip it; DocLang stamps each child with the
    /// layer token, exactly as a [`Node::Furniture`] wrapper on each would.
    /// DocLang has no group element, so a group is transparent there.
    Group {
        label: String,
        name: Option<String>,
        layer: Option<ContentLayer>,
        children: Vec<Node>,
    },
    /// A form key-value region (docling's `field_region`): a set of form fields,
    /// each pairing an optional marker, key, and value. Backends detect these
    /// from form structure (e.g. HTML's `keyN` / `keyN_valueM` / `keyN_marker`
    /// `id`-convention); the serializers render each item's parts as separate
    /// labelled texts (`marker` / `field_key` / `field_value`).
    FieldRegion { items: Vec<FieldItem> },
    /// docling's `KeyValueItem`: a graph of key and value cells and the links
    /// between them (docling-core's `GraphData`). The XBRL backend builds one
    /// from an instance's numeric facts and the presentation and calculation
    /// hierarchies of its taxonomy. Only the JSON export carries the graph:
    /// docling's Markdown serializer has no rendering for the item and writes
    /// its `<!-- missing-key-value-item -->` placeholder, which the Markdown
    /// export reproduces; the other serializers omit it.
    KeyValueGraph {
        cells: Vec<GraphCell>,
        links: Vec<GraphLink>,
    },
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
    /// A PDF text item whose superscripts and subscripts the opt-in
    /// script-position stage marked (#1005, docling's `ScriptPositionModel`,
    /// `script_position_options.enabled`). `inner` is the item — a paragraph
    /// or a footnote — with its text already the runs' joined text (Markdown-
    /// escaped, `E = mc2 in 1905.`): every serializer but JSON and DocLang
    /// renders it as it is. The JSON writes docling's shape — the item with
    /// an empty `text`, holding an inline group of the runs, each a text item
    /// with `formatting.script` and the box of its own characters — and
    /// DocLang writes the runs as an inline group.
    ScriptText {
        inner: Box<Node>,
        runs: Vec<ScriptRun>,
    },
    /// A node in a non-body content layer — `furniture` (page headers/footers,
    /// the HTML `<title>`, site navigation/chrome) or `notes` (docx comments).
    /// Markdown and JSON omit these layers by default; DocLang renders the wrapped
    /// node with a `<layer value="{layer}"/>` head.
    Furniture {
        layer: ContentLayer,
        inner: Box<Node>,
    },
    /// One reviewer comment: docling's notes-layer `comment_section` group
    /// holding a single text item. `name` is docling's own — `comment-{id}` for
    /// a docx `w:comment`, `comment-{sheet}-{cell}` for a spreadsheet cell note.
    /// JSON emits the group plus its notes-layer text; DocLang emits the flat
    /// `<text><layer value="notes"/>…</text>` upstream writes (its DocLang
    /// carries no group for comments); Markdown and LaTeX omit the notes layer.
    ///
    /// `refs_note_text` picks what a [`Node::Commented`] annotation points at,
    /// mirroring an upstream asymmetry: docling-core's `add_comment` appends the
    /// **note text**'s ref to each target (which is what the xlsx backend gets),
    /// while the docx backend overwrites that with the **group**'s ref so a
    /// comment's replies group together.
    ///
    /// `grouped` is whether docling wraps the note in a `comment_section`
    /// group at all: the docx and spreadsheet backends do, while backends that
    /// call docling-core's `add_comment` directly (Pages, #383) get a bare
    /// notes-layer text item under the body — JSON then emits no group and
    /// `name` is unused.
    CommentSection {
        name: String,
        text: String,
        refs_note_text: bool,
        grouped: bool,
    },
    /// A body item annotated by reviewer comments: `comments` are indices into
    /// the document's [`Node::CommentSection`] nodes, in document order. JSON
    /// emits docling's `comments: [{"$ref": …}]` on the item, each ref pointing
    /// where the section says (see [`Node::CommentSection::refs_note_text`]);
    /// every other serializer renders `inner` unchanged.
    Commented {
        comments: Vec<usize>,
        inner: Box<Node>,
    },
    /// A text node taken from a time-based track — an ASR segment (#614):
    /// A link destination (#852: a `.doc` bookmark; the DOCX item tree
    /// keeps its own, #849): the identifier the document's internal links
    /// (`#id`) point at. Rendered by nothing but the JSON builder, which
    /// hands it to the next text item for the Pandoc AST (`_anchors`).
    Anchor { id: String },
    /// docling's `TrackSource`. The JSON writes docling's ASR text item — `cue`
    /// (the segment's words) as its text, `track` as its `source: [{"kind":
    /// "track", …}]` — which the WebVTT export makes a cue of; every other
    /// serializer renders `inner` (the `[time: start-end] words` paragraph)
    /// unchanged.
    Track {
        track: crate::tree::TreeTrack,
        cue: String,
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
    /// Exact page provenance for a backend whose geometry already *is* the
    /// page's coordinate system — an XLSX item's cell-index box on its sheet,
    /// which docling writes verbatim (`bbox` in a top-left origin, the
    /// `charspan` the backend chose) and sizes the page from. The 0–511 grid
    /// of a [`Node::Located`] cannot round-trip such integers exactly, so the
    /// JSON export reads this wrapper; every other serializer renders `inner`
    /// unchanged (DocLang keeps taking its `<location>` tokens from the grid).
    Prov {
        page_no: usize,
        /// `[l, t, r, b]`, page units, top-left origin.
        bbox: [f32; 4],
        /// docling's `charspan` for the item (`[0, 0]` for an XLSX table).
        charspan: [usize; 2],
        /// The bbox's origin: docling's `BOTTOMLEFT` (a PDF page's
        /// provenance, as a docling-JSON input carries it — #899) or
        /// `TOPLEFT` (an XLSX sheet's cell box).
        bottom_left: bool,
        /// The item's creation rank among its siblings, when that differs
        /// from the node order: docling numbers `#/tables/N` / `#/texts/N` /
        /// `#/pictures/N` in the order it *creates* items (a sheet's tables,
        /// then its images, then its charts) and only afterwards sorts the
        /// container's children by position. The JSON export adds siblings in
        /// this order and lays their refs out in node order; `None` when the
        /// two orders coincide.
        seq: Option<usize>,
        inner: Box<Node>,
    },
    /// A PDF page header or footer (docling's `page_header`/`page_footer`
    /// furniture): DocLang emits `<page_header>`/`<page_footer>` with a
    /// `<layer value="furniture"/>` head, the four `<location>` tokens, then the
    /// text. The JSON writes it as a body-parented `page_header`/`page_footer`
    /// text item on the furniture layer; Markdown omits it.
    PageFurniture {
        footer: bool,
        location: [u16; 4],
        text: String,
    },
    /// A furniture-layer text item with an explicit docling label — a page
    /// header / footer paragraph (`page_header` / `page_footer`) or a note
    /// body (`footnote`) from a backend that builds no item tree (#535: the
    /// Word 97 `.doc` stories). The JSON writes a body-parented item of that
    /// label on the furniture layer; DocLang `<{label}>` with a `<layer
    /// value="furniture"/>` head (docling-core's own shape); Markdown, LaTeX
    /// and the chunker leave it out like any furniture.
    FurnitureText { label: String, text: String },
    /// The text a PDF picture contains, kept the way docling keeps it: every
    /// regular layout cluster > 80 % inside a picture cluster is that
    /// `PictureItem`'s child (`LayoutPostprocessor._set_cluster_children`,
    /// `ReadingOrderModel._add_child_elements`). Emitted right after its
    /// picture node. The JSON export writes these nodes as items parented to
    /// that picture, after its caption; docling's Markdown and LaTeX picture
    /// serializers print only the caption and the image, so every other
    /// serializer skips it — except the Markdown export asked to
    /// `traverse_pictures` ([`MarkdownExportOptions`], #599), which renders
    /// them after the picture like upstream's item walk does.
    PictureChildren(Vec<Node>),
    /// A page boundary — docling's implicit page break between pages. The PPTX
    /// backend emits one between consecutive slides. DocLang renders it as
    /// `<page_break/>`; Markdown and JSON omit it (matching docling's default
    /// exports, which carry page breaks only in the document model).
    PageBreak,
    /// A text item docling hangs in a *list group*, after the item before it
    /// (an AsciiDoc paragraph still pending when a list opened is flushed into
    /// the list's group): docling-core's list serializer prints it as one more
    /// line of the list, indented to the group's depth `level` — or, `glue`,
    /// when it is an inline group, appended to the line before it with no
    /// separator at all. `text` is Markdown, like a paragraph's. Every other
    /// serializer reads it as a paragraph.
    ListText { level: u8, text: String, glue: bool },
    /// An invisible page marker — the first node of every page the PDF paths
    /// assemble: the 1-based page number and the page size in PDF points. It
    /// carries exactly what the JSON export needs to populate docling's
    /// `pages` map and to denormalize the 0–511 `<location>` grid back into
    /// BOTTOMLEFT point bboxes for per-item `prov` (#171). Every other
    /// serializer skips it, so Markdown / DocLang / DocTags output is
    /// byte-for-byte unchanged.
    PageInfo {
        /// 1-based page number (0 = "not yet numbered": the assembler emits
        /// the marker, the document-level collector stamps the real number).
        page_no: usize,
        /// Page width in PDF points.
        width: f32,
        /// Page height in PDF points.
        height: f32,
    },
    /// A node docling keeps in the document model (and DocLang) but leaves out
    /// of the Markdown and JSON exports — e.g. an ODF *presentation*'s pictures
    /// and charts, which appear in the `.dclx` body but not in its `.md`/`.json`.
    /// DocLang renders the wrapped node in place; Markdown and JSON skip it.
    DoclangOnly(Box<Node>),
    /// A verbatim plain-text dump: one text item holding text whose line
    /// breaks are content (a nested table flattened to its cells' text, an
    /// ODF text box's raw text). Markdown/JSON render it as one text block,
    /// DocLang as one `<text>` whose `<content>` keeps the breaks.
    TextDump(String),
}

/// One run of a [`Node::ScriptText`]: a piece of the item's text at one
/// script position, with its provenance — one entry per page it is on.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptRun {
    /// The run's raw text (not Markdown-escaped), its spaces included:
    /// serializers join runs with nothing.
    pub text: String,
    pub script: Script,
    /// The run's provenance, one per page, in text order.
    pub prov: Vec<ScriptRunProv>,
    /// The item's link, on its baseline runs (each linked run exports as a
    /// link of its own, so a marker stays out of it).
    pub hyperlink: Option<String>,
}

/// Where a [`ScriptRun`] sits on one page: docling's `ProvenanceItem` with a
/// bottom-left-origin box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScriptRunProv {
    /// 1-based page number.
    pub page_no: usize,
    /// `[l, t, r, b]`, PDF points, bottom-left origin (`t > b`).
    pub bbox: [f32; 4],
    /// The run's characters on this page, as a span of its `text`.
    pub charspan: [usize; 2],
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
    /// Background content (watermarks): docling-core 2.99's `background`
    /// layer, which DocLang has and an `invisible` item is written to
    /// (docling-core#800) — read back from a DocLang document.
    Background,
}

impl ContentLayer {
    /// The `<layer value="…"/>` token value.
    pub fn value(self) -> &'static str {
        match self {
            ContentLayer::Furniture => "furniture",
            ContentLayer::Notes => "notes",
            ContentLayer::Invisible => "invisible",
            ContentLayer::Background => "background",
        }
    }
}

/// A set of content layers, `body` included — docling-core's
/// `set[ContentLayer]` (`HTMLParams.layers`,
/// `export_to_html(included_content_layers=…)`), where `body` is a member
/// like any other. [`Default`] is docling's `DEFAULT_CONTENT_LAYERS`: body
/// only, so an export built with it is unchanged from before layers could
/// be chosen (#499).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentLayers {
    /// The main content (items without an explicit layer).
    pub body: bool,
    /// [`ContentLayer::Furniture`]: page headers/footers, navigation chrome.
    pub furniture: bool,
    /// [`ContentLayer::Notes`]: reviewer comments and other editorial notes.
    pub notes: bool,
    /// [`ContentLayer::Invisible`]: hidden content (hidden sheets, …).
    pub invisible: bool,
    /// [`ContentLayer::Background`]: background content (watermarks).
    pub background: bool,
}

impl Default for ContentLayers {
    fn default() -> Self {
        Self::BODY
    }
}

impl ContentLayers {
    /// Body only — docling's `DEFAULT_CONTENT_LAYERS`.
    pub const BODY: Self = Self {
        body: true,
        furniture: false,
        notes: false,
        invisible: false,
        background: false,
    };
    /// Every layer — Python's `set(ContentLayer)`.
    pub const ALL: Self = Self {
        body: true,
        furniture: true,
        notes: true,
        invisible: true,
        background: true,
    };
    /// No layer at all (nothing renders); build a set from it with
    /// [`with`](Self::with) / [`with_body`](Self::with_body).
    pub const NONE: Self = Self {
        body: false,
        furniture: false,
        notes: false,
        invisible: false,
        background: false,
    };

    /// The set plus `layer`.
    pub const fn with(mut self, layer: ContentLayer) -> Self {
        match layer {
            ContentLayer::Furniture => self.furniture = true,
            ContentLayer::Notes => self.notes = true,
            ContentLayer::Invisible => self.invisible = true,
            ContentLayer::Background => self.background = true,
        }
        self
    }

    /// The set plus the body layer.
    pub const fn with_body(mut self) -> Self {
        self.body = true;
        self
    }

    /// Whether `layer` is in the set; `None` is the body layer.
    pub fn contains(&self, layer: Option<ContentLayer>) -> bool {
        match layer {
            None => self.body,
            Some(ContentLayer::Furniture) => self.furniture,
            Some(ContentLayer::Notes) => self.notes,
            Some(ContentLayer::Invisible) => self.invisible,
            Some(ContentLayer::Background) => self.background,
        }
    }

    /// Whether the layer spelled `name` (`body`, `furniture`, `notes`,
    /// `invisible`, `background` — the JSON `content_layer` values) is in
    /// the set; an unknown name is not.
    pub fn contains_name(&self, name: &str) -> bool {
        match name {
            "body" => self.body,
            "furniture" => self.furniture,
            "notes" => self.notes,
            "invisible" => self.invisible,
            "background" => self.background,
            _ => false,
        }
    }

    /// Parse a comma-separated list of layer names (`body,furniture`;
    /// whitespace around a name is ignored, `all` is every layer). An
    /// unknown name is the error.
    pub fn parse_list(list: &str) -> Result<Self, String> {
        let mut set = Self::NONE;
        for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            match name {
                "body" => set.body = true,
                "furniture" => set.furniture = true,
                "notes" => set.notes = true,
                "invisible" => set.invisible = true,
                "background" => set.background = true,
                "all" => set = Self::ALL,
                other => {
                    return Err(format!(
                        "unknown content layer `{other}` (expected body, furniture, notes, invisible, background or all)"
                    ))
                }
            }
        }
        Ok(set)
    }
}

/// Options of the HTML export ([`DoclingDocument::export_to_html_with`]):
/// docling-core's `HTMLParams` subset the port honours. [`Default`] is
/// upstream's default export — placeholder images, `artifacts` as the
/// referenced-image directory, the body layer only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlExportOptions {
    /// How pictures render (`HTMLParams.image_mode`): nothing but captions
    /// and meta for [`ImageMode::Placeholder`], `data:` URIs when embedded,
    /// `<img src>` paths under [`artifacts_dir`](Self::artifacts_dir) when
    /// referenced.
    pub image_mode: ImageMode,
    /// The directory referenced images are named under, the Markdown
    /// export's convention (`<artifacts_dir>/image_NNNNNN.<ext>`).
    pub artifacts_dir: String,
    /// The content layers rendered (`HTMLParams.layers`, #499): an item on a
    /// layer outside the set is skipped, its children still walked — exactly
    /// how docling-core's `get_excluded_refs` reads the set.
    pub layers: ContentLayers,
}

impl Default for HtmlExportOptions {
    fn default() -> Self {
        Self {
            image_mode: ImageMode::Placeholder,
            artifacts_dir: "artifacts".to_string(),
            layers: ContentLayers::BODY,
        }
    }
}

/// Options of the Markdown export
/// ([`DoclingDocument::export_to_markdown_with_options`], #599): the
/// docling-core `MarkdownParams` the port honours beyond the image mode.
/// [`Default`] is upstream's default export — placeholder images,
/// `artifacts` as the referenced-image directory, the body layer only, no
/// picture traversal, HTML and underscore escaping on, `<!-- image -->` —
/// so a document exported with it is byte-identical to
/// [`DoclingDocument::export_to_markdown`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownExportOptions {
    /// How pictures render (`MarkdownParams.image_mode`): the placeholder,
    /// `data:` URIs when embedded, `![Image](<artifacts_dir>/image_NNNNNN.<ext>)`
    /// when referenced (the bytes come back for the caller to write).
    pub image_mode: ImageMode,
    /// The directory referenced images are named under.
    pub artifacts_dir: String,
    /// The content layers rendered (`MarkdownParams.layers`,
    /// `export_to_markdown(included_content_layers=…)`): an item on a layer
    /// outside the set is skipped, its children still walked. The default
    /// body-only set drops page headers/footers (`furniture`), reviewer
    /// comments (`notes`) and hidden sheets (`invisible`), as upstream does; a
    /// scanned form's running header reads out with
    /// `ContentLayers::BODY.with(ContentLayer::Furniture)`. The extra items
    /// render through the body's serializers — a page header is a paragraph.
    pub layers: ContentLayers,
    /// `CommonParams.traverse_pictures`: render the text items nested in a
    /// picture (the PDF pipeline's picture children — a bordered form laid
    /// out as one picture holds every field as a child) after the picture's
    /// own caption and image, as upstream yields them. Off, a picture prints
    /// only its caption and image.
    pub traverse_pictures: bool,
    /// `MarkdownParams.escape_html`: `&`, `<` and `>` in text as `&amp;`,
    /// `&lt;`, `&gt;` (Python's `html.escape(quote=False)`). Off, `R&D` stays
    /// `R&D`.
    pub escape_html: bool,
    /// `MarkdownParams.escape_underscores`: `_` in text as `\_`. Off, the
    /// text keeps its underscores.
    pub escape_underscores: bool,
    /// `MarkdownParams.image_placeholder`: what a picture prints as when it
    /// renders no image data (the placeholder mode, or a picture without a
    /// payload); upstream's `<!-- image -->`.
    pub image_placeholder: String,
    /// docling-core's `PlainTextDocSerializer` (#613, `export_to_text`,
    /// docling's `--to text`): the Markdown walk with the decoration turned
    /// off — headings without `#`, bold/italic/strikethrough markers and
    /// inline code backticks dropped, a hyperlink reduced to its label, code
    /// blocks unfenced, no GFM hard line breaks. List bullets/numbers,
    /// checkbox marks and table grids stay. [`Self::plain_text`] pairs it
    /// with upstream's `PlainTextParams` (no escaping, an empty image
    /// placeholder).
    pub plain_text: bool,
}

impl MarkdownExportOptions {
    /// docling-core's `PlainTextParams` defaults (#613): [`Self::plain_text`]
    /// on, HTML and underscore escaping off, pictures print nothing (their
    /// captions still do), body layer only, no picture traversal — what
    /// `DoclingDocument.export_to_text()` and docling's `--to text` write.
    pub fn plain_text() -> Self {
        Self {
            escape_html: false,
            escape_underscores: false,
            image_placeholder: String::new(),
            plain_text: true,
            ..Self::default()
        }
    }
}

impl Default for MarkdownExportOptions {
    fn default() -> Self {
        Self {
            image_mode: ImageMode::Placeholder,
            artifacts_dir: "artifacts".to_string(),
            layers: ContentLayers::BODY,
            traverse_pictures: false,
            escape_html: true,
            escape_underscores: true,
            image_placeholder: "<!-- image -->".to_string(),
            plain_text: false,
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

impl Node {
    /// The paragraph a [`Node::LabeledText`] renders as outside the JSON:
    /// its text, wrapped as `[text](uri)` when it carries a hyperlink. Any
    /// other node is returned unchanged.
    pub fn labeled_as_paragraph(&self) -> std::borrow::Cow<'_, Node> {
        match self {
            Node::LabeledText { text, href, .. } => std::borrow::Cow::Owned(Node::Paragraph {
                text: match href {
                    Some(uri) => crate::hyperlink::markdown_link(text, uri),
                    None => text.clone(),
                },
            }),
            other => std::borrow::Cow::Borrowed(other),
        }
    }
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
    let single_plain = runs.len() <= 1 && runs.first().is_none_or(|r| r.is_plain());
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
    /// docling's `kind` on the `field_value` item — `read_only` or
    /// `fillable` (the value is or holds a form control); JSON-only.
    pub value_kind: Option<String>,
}

/// One node of a [`Node::KeyValueGraph`] — docling-core's `GraphCell`.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphCell {
    /// docling's `GraphCellLabel` value: `key` or `value` (it also knows
    /// `unspecified` and `checkbox`).
    pub label: String,
    /// The cell's number within its graph, what the links refer to.
    pub cell_id: usize,
    pub text: String,
    /// The text before any cleanup — for an XBRL fact key, the concept's
    /// qualified name where `text` is its local name.
    pub orig: String,
}

/// One edge of a [`Node::KeyValueGraph`] — docling-core's `GraphLink`.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphLink {
    /// docling's `GraphLinkLabel` value: `to_value`, `to_child`, `to_parent`
    /// or `to_key`.
    pub label: String,
    pub source_cell_id: usize,
    pub target_cell_id: usize,
}

/// One DocumentPictureClassifier prediction — docling-core's
/// `PictureClassificationClass` (`class_name` + `confidence`).
#[derive(Debug, Clone, PartialEq)]
pub struct PictureClass {
    /// e.g. `bar_chart`, `logo`, `signature` (the classifier's 26-label set).
    pub class_name: String,
    pub confidence: f32,
}

/// A picture's text annotation — docling-core's `PictureDescriptionData`
/// (`text` + `provenance`), the one shape upstream uses for every model that
/// writes prose about a picture. The picture-OCR enrichment (#645) fills it
/// with the lines the OCR engine read off the image, `provenance` naming the
/// engine (`ppocr` / `tesseract`) the way a VLM description names its model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PictureDescription {
    pub text: String,
    pub provenance: String,
}

impl PictureDescription {
    /// The provenance of a picture's author-written alt text (#676): the
    /// `descr` (else `title`) of an OOXML picture's non-visual properties
    /// — what DOCX, PPTX and XLSX authors type into "Alt Text". JSON writes
    /// it as `meta.description.created_by`; Markdown renders it under strict
    /// Markdown or `DoclingDocument::picture_alt_text`, HTML under the latter.
    pub const ALT_TEXT: &'static str = "alt_text";

    /// An alt-text description, see [`Self::ALT_TEXT`].
    pub fn alt_text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            provenance: Self::ALT_TEXT.to_string(),
        }
    }

    /// Whether this description is a picture's alt text.
    pub fn is_alt_text(&self) -> bool {
        self.provenance == Self::ALT_TEXT
    }

    /// The (empty) provenance of a description docling's backend writes
    /// itself as `PictureMeta.description` — `text` only, no `created_by`
    /// and no legacy `annotations` entry: the alt text of an XLSX image
    /// placed in a cell (docling#5015). Unlike [`Self::ALT_TEXT`] it is
    /// upstream's own output, so every format renders it unconditionally.
    pub const DOCLING_META: &'static str = "";

    /// A docling-native description, see [`Self::DOCLING_META`].
    pub fn docling_meta(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            provenance: Self::DOCLING_META.to_string(),
        }
    }

    /// Whether this is a docling-native description ([`Self::DOCLING_META`]).
    pub fn is_docling_meta(&self) -> bool {
        self.provenance == Self::DOCLING_META
    }
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
    /// Pixels per inch of the image relative to the page it came from —
    /// docling-core's `ImageRef.dpi`, which consumers use to map pixels back
    /// to points. A crop rendered from a PDF page at `s` px/pt is `72·s`
    /// (docling: `int(72 * images_scale)`, #519); an image embedded in an
    /// office file has no render scale and keeps docling's `72`
    /// ([`Self::DEFAULT_DPI`]).
    pub dpi: u32,
    /// The size the picture is *displayed* at in its document, in inches
    /// (`[width, height]`, #834): a DOCX drawing's `wp:extent`, an ODT
    /// frame's `svg:width` / `svg:height` — not the bitmap's pixel size
    /// above, which docling's `ImageRef.size` carries. `None` when the
    /// format has no such extent (a PDF crop, a standalone image). The
    /// Pandoc export writes it as the `Image`'s `width` / `height`
    /// attributes, as Pandoc's own readers do; docling's JSON has no field
    /// for it, so the JSON export leaves it out.
    pub display_size: Option<[f32; 2]>,
}

impl PictureImage {
    /// docling's `ImageRef` default when no render scale applies.
    pub const DEFAULT_DPI: u32 = 72;

    /// The `dpi` of an image rendered at `scale` pixels per PDF point
    /// (`72·scale`, rounded — `int()` would floor 143.99… to 143).
    pub fn dpi_for_scale(scale: f32) -> u32 {
        (72.0 * scale).round().max(1.0) as u32
    }

    /// A `data:` URI for the image (`data:<mimetype>;base64,<…>`).
    pub fn data_uri(&self) -> String {
        format!(
            "data:{};base64,{}",
            self.mimetype,
            crate::base64::encode(&self.data)
        )
    }
}

/// One table cell as a first-class object (#240) — the Rust counterpart of
/// docling's `TableCell`: its text, page geometry, grid rectangle and header
/// roles. Produced by the PDF TableFormer paths from the predicted OTSL
/// structure; `bbox` is `[l, t, r, b]` in page points with a top-left origin.
#[derive(Debug, Clone, PartialEq)]
pub struct TableCell {
    pub text: String,
    /// `[l, t, r, b]`, page points, top-left origin; `None` without geometry.
    pub bbox: Option<[f32; 4]>,
    /// Anchor grid position (0-based row/column offsets).
    pub start_row: usize,
    pub start_col: usize,
    /// Span extents (≥ 1); the covered grid positions repeat the cell's text
    /// in [`Table::rows`].
    pub row_span: usize,
    pub col_span: usize,
    /// OTSL `ched` — a column-header cell.
    pub column_header: bool,
    /// OTSL `rhed` — a row-header cell.
    pub row_header: bool,
    /// OTSL `srow` — a section-row cell.
    pub row_section: bool,
}

/// Where a picture's or table's caption text item hangs in the docling-JSON
/// tree (#390). docling's backends do not agree, and the JSON structure is
/// the only surface that shows it (Markdown, DocLang and LaTeX place a
/// caption by its item, whatever its parent): the PDF pipeline parents a
/// layout caption to the picture or table it belongs to, while every
/// declarative backend creates the caption with `doc.add_text(label=CAPTION)`
/// and no parent — the document body — even when the item itself sits in a
/// group or under a section header (JATS, LaTeX, HTML, Markdown, EPUB,
/// AsciiDoc all do). The office backends hang a chart's title caption off the
/// chart's container (the sheet group, the slide, docx's current parent,
/// docling#4190) — that is [`Node::Chart`]'s own path, not this choice. A
/// backend that adds a new captioned item picks the variant matching
/// upstream's `add_text` call for that format; the default is upstream's
/// default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptionParent {
    /// `#/body`, listed after the enclosing top-level item — docling's
    /// `add_text` default, which every declarative backend leaves alone.
    #[default]
    Body,
    /// The item's own container (its `parent`), listed ahead of the item:
    /// the caption is created first, as docling's office backends do.
    Container,
    /// The item's own container, listed *after* the item: docling's HTML
    /// backend adds a `<figure>`-wrapped table, then its `<figcaption>` under
    /// `self.parents[self.level]` (docling#4050).
    ContainerAfter,
    /// The item itself — the caption is the picture's or table's first
    /// child, as docling's PDF pipeline attaches a layout caption.
    Item,
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
    /// Optional caption (docling's `TableItem.captions`): the JATS
    /// `<table-wrap>` label+caption, an HTML `<caption>`, etc. Markdown renders
    /// it as a text line *before* the grid; JSON emits a caption text item the
    /// table references; DocLang emits a `<caption>` as the table's first child.
    /// `None` → the table has no caption.
    pub caption: Option<String>,
    /// Where the caption item hangs in the JSON tree (#390); see
    /// [`CaptionParent`]. Only the JSON export reads it.
    pub caption_parent: CaptionParent,
    /// The caption's own box on the table's page (0–511 grid) — the PDF
    /// pipeline's caption layout region, written as the caption item's
    /// `prov` (#609); see [`Node::Picture`]'s field of the same name.
    pub caption_location: Option<[u16; 4]>,
    /// The table's footnotes (docling's `TableItem.footnotes`, #1009): one
    /// text each, escaped like [`Self::caption`]. Markdown prints them after
    /// the grid, one paragraph each; JSON emits `footnote` text items under
    /// the table that its `footnotes` list references (docling-core#675).
    pub footnotes: Vec<String>,
    /// Optional per-cell bounding boxes, same shape as [`Self::rows`]: `[l, t,
    /// r, b]` in page points with a **top-left** origin (the PDF pipeline's
    /// native space). Set by the ML pipeline's TableFormer paths — a spanned
    /// cell repeats its anchor's box across the covered grid positions — and
    /// First-class cells (#240): the authoritative per-cell records —
    /// text, page geometry, spans and header roles — when the backend
    /// produces them (the PDF TableFormer paths do; declarative backends
    /// leave `None`). [`Self::rows`] stays the dense text grid every
    /// serializer renders (a spanning cell's text is replicated across its
    /// covered positions there); JSON serializes these cells verbatim when
    /// present, and the DocLang structure overlay is derived from them.
    pub cells: Option<Vec<TableCell>>,
    /// The table is a table of contents — docling's `document_index` label
    /// (#1091), which the PDF layout model gives a TOC region and docling
    /// keeps on the `TableItem`. Only the JSON export (and its read-back)
    /// tells it apart: every other serializer renders it as a table, as
    /// docling-core does.
    pub document_index: bool,
}

impl Table {
    /// A cell's text at a grid position, `None` outside the grid.
    pub fn cell_text(&self, row: usize, col: usize) -> Option<&str> {
        self.rows.get(row)?.get(col).map(String::as_str)
    }

    /// Replace the text at a grid position; `false` (and no change) outside
    /// the grid. When a first-class cell covers the position, the whole
    /// cell is updated: its record text and every grid position its span
    /// covers, so the repair shows once in Markdown, not once per covered
    /// column.
    pub fn set_cell_text(&mut self, row: usize, col: usize, text: impl Into<String>) -> bool {
        if self.rows.get(row).and_then(|r| r.get(col)).is_none() {
            return false;
        }
        let text = text.into();
        let covering = self.cells.as_mut().and_then(|cells| {
            cells.iter_mut().find(|c| {
                (c.start_row..c.start_row + c.row_span).contains(&row)
                    && (c.start_col..c.start_col + c.col_span).contains(&col)
            })
        });
        if let Some(cell) = covering {
            cell.text = text.clone();
            let (r0, r1) = (cell.start_row, cell.start_row + cell.row_span);
            let (c0, c1) = (cell.start_col, cell.start_col + cell.col_span);
            for r in self.rows.iter_mut().take(r1).skip(r0) {
                for slot in r.iter_mut().take(c1).skip(c0) {
                    *slot = text.clone();
                }
            }
        } else {
            self.rows[row][col] = text;
        }
        true
    }

    /// Derive first-class cells (#240) from the dense grid plus the
    /// [`TableStructure`] overlay — how declarative tables (DOCX/XLSX merged
    /// regions, HTML `th`/spans, ODF covered cells, USPTO CALS) get real
    /// `TableCell` records without page geometry. Anchors are the positions
    /// not marked as span continuations; extents scan the continuation grids
    /// right/down (matching the DocLang `lcel`/`ucel` reading). Header roles
    /// come from the per-cell `col_header`/`row_header` grids when present,
    /// else the `header_row` band, else docling's declarative default (row 0
    /// is the header). Without any overlay every position is a 1×1 cell.
    pub fn derive_cells(&self) -> Vec<TableCell> {
        let s = self.structure.as_ref();
        let flag = |grid: Option<&Vec<Vec<bool>>>, r: usize, c: usize| {
            grid.and_then(|g| g.get(r))
                .and_then(|row| row.get(c))
                .copied()
                .unwrap_or(false)
        };
        let col_cont = |r: usize, c: usize| flag(s.map(|s| &s.col_continuation), r, c);
        let row_cont = |r: usize, c: usize| flag(s.map(|s| &s.row_continuation), r, c);
        let is_col_header = |r: usize, c: usize| match s {
            Some(st) if !st.col_header.is_empty() => flag(Some(&st.col_header), r, c),
            Some(st) if !st.header_row.is_empty() => st.header_row.get(r).copied().unwrap_or(false),
            _ => r == 0,
        };
        let mut cells = Vec::new();
        for (r, row) in self.rows.iter().enumerate() {
            for (c, text) in row.iter().enumerate() {
                if col_cont(r, c) || row_cont(r, c) {
                    continue; // covered by a span anchor
                }
                let mut col_span = 1;
                while c + col_span < row.len() && col_cont(r, c + col_span) {
                    col_span += 1;
                }
                let mut row_span = 1;
                while r + row_span < self.rows.len() && row_cont(r + row_span, c) {
                    row_span += 1;
                }
                cells.push(TableCell {
                    text: text.clone(),
                    bbox: None,
                    start_row: r,
                    start_col: c,
                    row_span,
                    col_span,
                    column_header: is_col_header(r, c),
                    row_header: flag(s.map(|s| &s.row_header), r, c),
                    row_section: false,
                });
            }
        }
        cells
    }

    /// The number of leading grid rows that form the column header block —
    /// docling-core's `TableData._count_column_header_rows` (docling-core#767,
    /// 2.102, #778), shared by the Markdown serializer
    /// ([`Self::header_row_count`]) and the chunker's dataframe view, which
    /// must agree on where the header ends. Uses the first-class
    /// [`Self::cells`] when present (the PDF pipeline's TableFormer flags),
    /// else the cells derived from the structure overlay.
    ///
    /// - A row counts only when a `column_header` cell *starts* on it
    ///   (docling-core#723): a header spanning several rows is replicated
    ///   into each row it covers, and counting those would pull the data rows
    ///   beneath it into the header block.
    /// - A row also stops the block when it *carries body text* (#604,
    ///   docling-core#766): a cell starting on it that is not a column
    ///   header, has non-blank text and does not start in the table's first
    ///   column, which holds row labels left unmarked even on header rows. A
    ///   scanned form flags its first-column labels `column_header` on every
    ///   row; without the check each such row folded its values into the
    ///   header.
    /// - Row 0 is exempt from that check — model-predicted header rows are
    ///   often only partly flagged — unless a later row flags a cell starting
    ///   in the first column: that marks a column of row headers, not a
    ///   partly flagged header row.
    ///
    /// No fallback: a table without flags has no header rows here.
    pub fn column_header_row_count(&self) -> usize {
        if self.rows.is_empty() {
            return 0;
        }
        let derived;
        let cells: &[TableCell] = match &self.cells {
            Some(c) if !c.is_empty() => c,
            _ => {
                derived = self.derive_cells();
                &derived
            }
        };
        let num_rows = self.rows.len();
        let first_col = cells.iter().map(|c| c.start_col).min().unwrap_or(0);
        let leading_col_flagged_below = cells.iter().any(|c| {
            c.column_header && c.start_col == first_col && (1..num_rows).contains(&c.start_row)
        });
        let is_header_row = |r: usize| {
            let check_body = r > 0 || leading_col_flagged_below;
            let mut flagged = false;
            for c in cells.iter().filter(|c| c.start_row == r) {
                if c.column_header {
                    flagged = true;
                } else if check_body && c.start_col != first_col && !c.text.trim().is_empty() {
                    return false;
                }
            }
            flagged
        };
        (0..num_rows).take_while(|&r| is_header_row(r)).count()
    }

    /// The number of leading grid rows the Markdown serializer renders as the
    /// header — docling-core's `_count_header_rows`:
    /// [`Self::column_header_row_count`], plus upstream's row-0 fallback for
    /// GFM, which needs a header row: `1` when no flag sits on any grid row
    /// below the first (a spanning header cell counts on every row it
    /// covers), so tables from backends that never set the flag keep row 0
    /// as the header. When flags exist but none makes a header row, the
    /// count is `0` and every row stays in the body under an empty header.
    pub fn header_row_count(&self) -> usize {
        let count = self.column_header_row_count();
        if count > 0 || self.rows.is_empty() {
            return count;
        }
        let derived;
        let cells: &[TableCell] = match &self.cells {
            Some(c) if !c.is_empty() => c,
            _ => {
                derived = self.derive_cells();
                &derived
            }
        };
        if cells
            .iter()
            .any(|c| c.column_header && c.start_row + c.row_span.max(1) > 1)
        {
            0
        } else {
            1
        }
    }

    /// For every grid position, the position of the cell that covers it — its
    /// anchor (top-left start): itself for an unmerged position, the merge's
    /// start for one a spanning cell covers. Uses the first-class
    /// [`Self::cells`] when present, else the cells derived from the
    /// structure overlay. A rich cell's blocks ([`Self::cell_blocks`]) live on
    /// its anchor only, so a serializer that repeats a merged cell in every
    /// position it covers (docling-core#857) reads them through this.
    pub fn span_anchors(&self) -> Vec<Vec<(usize, usize)>> {
        let num_cols = self.rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut anchors: Vec<Vec<(usize, usize)>> = (0..self.rows.len())
            .map(|r| (0..num_cols).map(|c| (r, c)).collect())
            .collect();
        let derived;
        let cells: &[TableCell] = match &self.cells {
            Some(c) if !c.is_empty() => c,
            _ => {
                derived = self.derive_cells();
                &derived
            }
        };
        for cell in cells {
            let rows = cell.start_row..cell.start_row + cell.row_span.max(1);
            for row in anchors.iter_mut().take(rows.end).skip(rows.start) {
                let cols = cell.start_col..cell.start_col + cell.col_span.max(1);
                for pos in row.iter_mut().take(cols.end).skip(cols.start) {
                    *pos = (cell.start_row, cell.start_col);
                }
            }
        }
        anchors
    }

    /// The first-class cell covering a grid position, if any.
    pub fn cell_at(&self, row: usize, col: usize) -> Option<&TableCell> {
        self.cells.as_ref()?.iter().find(|c| {
            (c.start_row..c.start_row + c.row_span).contains(&row)
                && (c.start_col..c.start_col + c.col_span).contains(&col)
        })
    }

    /// A cell's bounding box (`[l, t, r, b]`, page points, top-left origin);
    /// `None` when no cell with geometry covers the position.
    pub fn cell_bbox(&self, row: usize, col: usize) -> Option<[f32; 4]> {
        self.cell_at(row, col)?.bbox
    }

    /// Set (or replace) the bounding box of the cell covering a grid
    /// position; `false` outside the text grid. A table without first-class
    /// cells materializes them first (one 1×1 cell per grid position, texts
    /// from the grid), so declarative tables can be annotated too.
    pub fn set_cell_bbox(&mut self, row: usize, col: usize, bbox: [f32; 4]) -> bool {
        if self.rows.get(row).and_then(|r| r.get(col)).is_none() {
            return false;
        }
        let rows = &self.rows;
        let cells = self.cells.get_or_insert_with(|| {
            rows.iter()
                .enumerate()
                .flat_map(|(r, cols)| {
                    cols.iter().enumerate().map(move |(c, text)| TableCell {
                        text: text.clone(),
                        bbox: None,
                        start_row: r,
                        start_col: c,
                        row_span: 1,
                        col_span: 1,
                        column_header: false,
                        row_header: false,
                        row_section: false,
                    })
                })
                .collect()
        });
        match cells.iter_mut().find(|c| {
            (c.start_row..c.start_row + c.row_span).contains(&row)
                && (c.start_col..c.start_col + c.col_span).contains(&col)
        }) {
            Some(cell) => {
                cell.bbox = Some(bbox);
                true
            }
            None => {
                cells.push(TableCell {
                    text: self.rows[row][col].clone(),
                    bbox: Some(bbox),
                    start_row: row,
                    start_col: col,
                    row_span: 1,
                    col_span: 1,
                    column_header: false,
                    row_header: false,
                    row_section: false,
                });
                true
            }
        }
    }

    /// The anchor position of the cell whose box overlaps `bbox` best
    /// (largest intersection-over-union), ties resolved in cell order.
    /// `None` when nothing overlaps or the table carries no geometry. This
    /// is the lookup half of the repair workflow: find the cell an external
    /// OCR box refers to, then [`Self::set_cell_text`] it.
    pub fn find_cell_by_bbox(&self, bbox: [f32; 4]) -> Option<(usize, usize)> {
        let area = |b: &[f32; 4]| ((b[2] - b[0]) * (b[3] - b[1])).max(0.0);
        let mut best: Option<(f32, (usize, usize))> = None;
        for cell in self.cells.as_deref()?.iter() {
            let Some(cb) = cell.bbox else { continue };
            let iw = (bbox[2].min(cb[2]) - bbox[0].max(cb[0])).max(0.0);
            let ih = (bbox[3].min(cb[3]) - bbox[1].max(cb[1])).max(0.0);
            let inter = iw * ih;
            if inter <= 0.0 {
                continue;
            }
            let iou = inter / (area(&bbox) + area(&cb) - inter).max(f32::EPSILON);
            if best.is_none_or(|(b, _)| iou > b) {
                best = Some((iou, (cell.start_row, cell.start_col)));
            }
        }
        best.map(|(_, pos)| pos)
    }

    /// Locate the cell overlapping `bbox` best and replace its text — the
    /// one-call form of the OCR-repair loop. Returns the updated anchor.
    pub fn update_cell_by_bbox(
        &mut self,
        bbox: [f32; 4],
        text: impl Into<String>,
    ) -> Option<(usize, usize)> {
        let (row, col) = self.find_cell_by_bbox(bbox)?;
        self.set_cell_text(row, col, text);
        Some((row, col))
    }
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

/// [`for_each_picture_in_mut`], read-only: every picture in `nodes`, however
/// nested, for a pass that only looks (the DocLang asset digests, #898).
pub fn for_each_picture_in<'a>(nodes: &'a [Node], f: &mut dyn FnMut(&'a Node)) {
    for node in nodes {
        match node {
            Node::Picture { .. } => f(node),
            Node::Group { children, .. } | Node::PictureChildren(children) => {
                for_each_picture_in(children, f)
            }
            Node::Located { inner, .. }
            | Node::Prov { inner, .. }
            | Node::Track { inner, .. }
            | Node::Furniture { inner, .. }
            | Node::Commented { inner, .. }
            | Node::DoclangOnly(inner) => for_each_picture_in(std::slice::from_ref(inner), f),
            Node::Table(t) => {
                if let Some(blocks) = t.cell_blocks.as_ref() {
                    for row in blocks {
                        for cell in row {
                            for_each_picture_in(cell, f);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// [`DoclingDocument::for_each_picture_mut`] over a node slice — every
/// picture in `nodes`, however nested (groups, wrappers, table cell blocks),
/// for a caller that holds page batches rather than a document (the
/// streaming PDF path, #896).
pub fn for_each_picture_in_mut(nodes: &mut [Node], f: &mut dyn FnMut(&mut Node)) {
    for node in nodes {
        match node {
            Node::Picture { .. } => f(node),
            Node::Group { children, .. } | Node::PictureChildren(children) => {
                for_each_picture_in_mut(children, f)
            }
            Node::Located { inner, .. }
            | Node::Prov { inner, .. }
            | Node::Track { inner, .. }
            | Node::Furniture { inner, .. }
            | Node::Commented { inner, .. }
            | Node::DoclangOnly(inner) => for_each_picture_in_mut(std::slice::from_mut(inner), f),
            Node::Table(t) => {
                if let Some(blocks) = t.cell_blocks.as_mut() {
                    for row in blocks {
                        for cell in row {
                            for_each_picture_in_mut(cell, f);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl DoclingDocument {
    /// Create an empty document with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            nodes: Vec::new(),
            strict_markdown: false,
            compact_tables: false,
            picture_alt_text: false,
            metadata: Default::default(),
            metadata_output: Default::default(),
            page_break_placeholder: None,
            links: Vec::new(),
            confidence: None,
            tree: None,
            page_images: std::collections::BTreeMap::new(),
            notices: Vec::new(),
        }
    }

    /// Append a node.
    /// The document's top-level tables in reading order — the read half of
    /// the post-extraction table API (#238). [`Node::Located`] wrappers (the
    /// PDF pipeline attaches layout provenance that way) are looked through;
    /// tables nested inside rich table cells (`Table::cell_blocks`) are not
    /// traversed.
    pub fn tables(&self) -> impl Iterator<Item = &Table> {
        fn unwrap_table(n: &Node) -> Option<&Table> {
            match n {
                Node::Table(t) => Some(t),
                Node::Located { inner, .. }
                | Node::Prov { inner, .. }
                | Node::Track { inner, .. } => unwrap_table(inner),
                _ => None,
            }
        }
        self.nodes.iter().filter_map(unwrap_table)
    }

    /// Mutable access to the document's top-level tables, for repair
    /// workflows (#238): locate a cell via [`Table::find_cell_by_bbox`], fix
    /// its text with [`Table::set_cell_text`], then re-export — every
    /// serializer reads the same grid.
    pub fn tables_mut(&mut self) -> impl Iterator<Item = &mut Table> {
        fn unwrap_table(n: &mut Node) -> Option<&mut Table> {
            match n {
                Node::Table(t) => Some(t),
                Node::Located { inner, .. }
                | Node::Prov { inner, .. }
                | Node::Track { inner, .. } => unwrap_table(inner),
                _ => None,
            }
        }
        self.nodes.iter_mut().filter_map(unwrap_table)
    }

    pub fn push(&mut self, node: Node) {
        self.nodes.push(node);
    }

    /// Visit every [`Node::Picture`] in the flat node stream, wherever it
    /// nests — group children, table cell blocks, a picture's own
    /// children, the located/prov/track/comment/furniture wrappers — so an
    /// enrichment that reads the embedded images (picture OCR, #645) reaches
    /// each one. The callback gets the picture node itself.
    pub fn for_each_picture_mut(&mut self, f: &mut dyn FnMut(&mut Node)) {
        for_each_picture_in_mut(&mut self.nodes, f);
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

    /// Markdown for this document as the *content of a rich table cell*
    /// (docling-core's `in_table_cell` serialization, docling-core#540):
    /// headings render as plain text since Markdown tables can't hold them.
    /// Backends build a sub-document per rich cell and flatten this into the
    /// cell text; no trailing newline.
    pub fn export_to_table_cell_markdown(&self) -> String {
        crate::markdown::to_markdown_table_cell(self, self.strict_markdown)
    }

    /// Serialize to docling-core's native JSON wire format (`DoclingDocument`
    /// schema), pretty-printed — the Rust equivalent of
    /// `DoclingDocument.export_to_dict()` / `save_as_json()`. The output loads
    /// back into Python docling-core and round-trips to the same Markdown.
    pub fn export_to_json(&self) -> String {
        let mut out = Vec::new();
        self.write_json_pretty(&mut out)
            .expect("DoclingDocument JSON is always serializable");
        String::from_utf8(out).expect("serde_json writes UTF-8")
    }

    /// The same JSON wire format as [`Self::export_to_json`], as a
    /// `serde_json::Value` — for callers that append response-level extras
    /// (docling-serve adds the confidence report, #183) before serializing.
    ///
    /// The `Value` is large — ~2.7 KB per text item — so on a long document
    /// prefer [`Self::write_json`], which writes the same JSON without it.
    pub fn export_to_json_value(&self) -> serde_json::Value {
        crate::json::to_json(self)
    }

    /// Write the JSON of [`Self::export_to_json_value`] to `writer`, compact
    /// — byte for byte `serde_json::to_writer(writer, &doc.export_to_json_value())`
    /// — without building the `Value`: the export holds its text items in a
    /// compact form and serializes them straight to the writer. Wrap a file
    /// or socket in a `BufWriter`.
    pub fn write_json<W: std::io::Write>(&self, writer: W) -> std::io::Result<()> {
        crate::json::write_json(self, writer, false).map_err(std::io::Error::from)
    }

    /// [`Self::write_json`], indented as [`Self::export_to_json`] is.
    pub fn write_json_pretty<W: std::io::Write>(&self, writer: W) -> std::io::Result<()> {
        crate::json::write_json(self, writer, true).map_err(std::io::Error::from)
    }

    /// [`Self::write_json_pretty`] with `extras` appended to the top-level
    /// object after `pages` — byte for byte `serde_json::to_writer_pretty`
    /// of [`Self::export_to_json_value`] with each `value[key] = v` set (a
    /// key the document JSON does not have), without building that `Value`:
    /// docling-serve's response adds its `confidence` report this way.
    pub fn write_json_pretty_with<W: std::io::Write>(
        &self,
        writer: W,
        extras: &[(&str, &serde_json::Value)],
    ) -> std::io::Result<()> {
        crate::json::write_json_with(self, writer, true, extras).map_err(std::io::Error::from)
    }

    /// Serialize to a complete LaTeX document — the Rust counterpart of
    /// docling-core's `LaTeXDocSerializer` with default parameters (docling
    /// 2.124's `--to latex`, #317). No trailing newline, like the upstream
    /// CLI's `<stem>.tex`.
    pub fn export_to_latex(&self) -> String {
        crate::latex::to_latex(self)
    }

    /// Serialize to Pandoc's AST as JSON (`pandoc -f json`, #515) with the
    /// default options: pictures as captioned figures without image data,
    /// body layer only, the [`PANDOC_API_VERSION`](crate::pandoc::PANDOC_API_VERSION)
    /// API. One line, no trailing newline. See [`crate::pandoc`].
    pub fn export_to_pandoc_json(&self) -> String {
        crate::pandoc::to_pandoc(self, &crate::pandoc::PandocExportOptions::default())
            .expect("the default Pandoc API version is always supported")
            .0
    }

    /// Pandoc JSON per `options` (image mode, artifacts directory, content
    /// layers, required API version). Returns the JSON and, for
    /// [`ImageMode::Referenced`], the `(path, bytes)` image files; an
    /// unsupported `api_version` is an error.
    pub fn export_to_pandoc_json_with(
        &self,
        options: &crate::pandoc::PandocExportOptions,
    ) -> Result<crate::pandoc::PandocOutput, crate::pandoc::PandocError> {
        crate::pandoc::to_pandoc(self, options)
    }

    /// Serialize to DocLang XML (`<doclang version="0.7">…`), the markup that
    /// lives inside a `.dclx` archive — the Rust counterpart of docling-core's
    /// `export_to_doclang()` with default parameters. No trailing newline; the
    /// archive writer appends exactly one.
    pub fn export_to_doclang(&self) -> String {
        self.export_doclang(crate::doclang::Pictures::Referenced { collect: false })
            .0
    }

    /// [`Self::export_to_doclang`] plus the picture assets its
    /// `<src uri="assets/image_….png"/>` references name, as `(path, bytes)`
    /// in document order — the parts docling's `save_as_doclang_archive`
    /// stores next to `document.xml`. PNG and JPEG pictures come back as PNG
    /// (a JPEG re-encoded from its libjpeg-decoded pixels); other encodings
    /// keep their own bytes for the archive writer to convert.
    pub fn export_to_doclang_with_assets(&self) -> (String, Vec<(String, Vec<u8>)>) {
        self.export_doclang(crate::doclang::Pictures::Referenced { collect: true })
    }

    /// DocLang with docling's `image_mode` for pictures (#884):
    /// [`ImageMode::Placeholder`] writes no `<src>` — docling-core's own
    /// `export_to_doclang()` default, which upstream's `.dclg` fixtures hold
    /// — [`ImageMode::Embedded`] a `data:` URI of the PNG,
    /// [`ImageMode::Referenced`] is [`Self::export_to_doclang_with_assets`].
    pub fn export_to_doclang_with_images(
        &self,
        mode: ImageMode,
    ) -> (String, Vec<(String, Vec<u8>)>) {
        use crate::doclang::Pictures;
        self.export_doclang(match mode {
            ImageMode::Placeholder => Pictures::Placeholder,
            ImageMode::Embedded => Pictures::Embedded,
            ImageMode::Referenced => Pictures::Referenced { collect: true },
        })
    }

    /// The item tree's DocLang — docling-core's `DocLangDocSerializer`
    /// ported over the tree (`doclang_tree.rs`, #884), the structure the
    /// JSON export writes — else (no tree, or one the port does not cover:
    /// provenance, picture meta, form fields) the nodes'. The flat nodes
    /// cannot carry a footnote's label, a caption's or a cell's inline
    /// formulas, text docling never wrote as Markdown, or the whitespace of
    /// an inline group's runs, which DocLang 0.9 reads as written.
    fn export_doclang(
        &self,
        pictures: crate::doclang::Pictures,
    ) -> (String, Vec<(String, Vec<u8>)>) {
        self.tree
            .as_ref()
            .filter(|t| !t.doclang_nodes)
            .and_then(|t| crate::doclang_tree::export(t, pictures))
            .unwrap_or_else(|| crate::doclang::render(&self.nodes, pictures))
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

    /// Markdown per `options` (#599) — docling-core's
    /// `export_to_markdown(included_content_layers=…, image_mode=…,
    /// image_placeholder=…, escape_html=…, escape_underscores=…,
    /// traverse_pictures=…)`: the content layers rendered, whether a
    /// picture's nested text items print, HTML and underscore escaping, the
    /// image placeholder, and the image mode with its referenced-image
    /// directory. [`Self::strict_markdown`] still picks the Markdown flavour.
    /// Returns the Markdown and, for [`ImageMode::Referenced`], the
    /// `(path, bytes)` artifacts. With [`MarkdownExportOptions::default`]
    /// the Markdown is [`Self::export_to_markdown`]'s byte for byte.
    pub fn export_to_markdown_with_options(
        &self,
        options: &MarkdownExportOptions,
    ) -> (String, Vec<(String, Vec<u8>)>) {
        crate::markdown::to_markdown_with_options(self, self.strict_markdown, options)
    }

    /// WebVTT (#614) — what docling's `--to vtt` writes
    /// (`DoclingDocument.save_as_vtt`): a cue per timed text item — a WebVTT
    /// input's cues, an audio/video transcript's segments — hours always
    /// written, the `</v>` of a lone voice span omitted. A document without
    /// timed text is the bare `WEBVTT` header. No trailing newline.
    pub fn export_to_vtt(&self) -> String {
        crate::vtt::to_vtt(self, &crate::vtt::VttExportOptions::default())
    }

    /// [`Self::export_to_vtt`] with docling-core's `WebVTTParams`
    /// (`omit_hours_if_zero`, `omit_voice_end`).
    pub fn export_to_vtt_with_options(&self, options: &crate::vtt::VttExportOptions) -> String {
        crate::vtt::to_vtt(self, options)
    }

    /// Plain text — docling-core's `DoclingDocument.export_to_text()` with
    /// its defaults (#613, docling's `--to text` / `<stem>.txt`): the
    /// Markdown export without decoration ([`MarkdownExportOptions::plain_text`]).
    /// Lists keep their bullets and numbers, tables their `|` grid. No
    /// trailing newline: upstream's `serialize().text`, which its CLI
    /// writes verbatim to `<stem>.txt`.
    pub fn export_to_text(&self) -> String {
        let (mut text, _) = crate::markdown::to_markdown_with_options(
            self,
            self.strict_markdown,
            &MarkdownExportOptions::plain_text(),
        );
        if text.ends_with('\n') {
            text.pop();
        }
        text
    }

    /// A complete HTML document — docling-core's `HTMLDocSerializer` with its
    /// defaults (#492): pictures stay out (`ImageRefMode.PLACEHOLDER`, only
    /// their captions and meta render). See [`export_to_html_with_images`]
    /// for the embedded / referenced modes and [`crate::html`]'s module docs
    /// for what is reproduced.
    ///
    /// [`export_to_html_with_images`]: Self::export_to_html_with_images
    pub fn export_to_html(&self) -> String {
        crate::html::to_html(self, &HtmlExportOptions::default()).0
    }

    /// HTML with pictures per `image_mode`: `data:` URIs when embedded, and
    /// when referenced `<img src>` paths under `artifacts_dir` whose bytes come
    /// back as `(path, bytes)` for the caller to write — the Markdown export's
    /// contract and file names.
    pub fn export_to_html_with_images(
        &self,
        image_mode: ImageMode,
        artifacts_dir: &str,
    ) -> (String, Vec<(String, Vec<u8>)>) {
        crate::html::to_html(
            self,
            &HtmlExportOptions {
                image_mode,
                artifacts_dir: artifacts_dir.to_string(),
                ..HtmlExportOptions::default()
            },
        )
    }

    /// HTML rendering the content `layers` — docling-core's
    /// `export_to_html(included_content_layers=…)` (#499). The default export
    /// is body only; `ContentLayers::BODY.with(ContentLayer::Furniture)` adds
    /// page headers/footers, `.with(ContentLayer::Notes)` reviewer comments,
    /// [`ContentLayers::ALL`] is Python's `set(ContentLayer)`. An item on a
    /// layer outside the set is skipped while its children are still walked,
    /// and the items that do render go through the same serializers as the
    /// body (a page header is a `<p>`, a comment a `<p>`, a header table a
    /// `<table>`), exactly as upstream's `HTMLParams.layers` behaves. Pictures
    /// stay placeholders; see [`export_to_html_with`] for the full option set.
    ///
    /// [`export_to_html_with`]: Self::export_to_html_with
    pub fn export_to_html_with_layers(&self, layers: ContentLayers) -> String {
        crate::html::to_html(
            self,
            &HtmlExportOptions {
                layers,
                ..HtmlExportOptions::default()
            },
        )
        .0
    }

    /// HTML per `options` — image mode, referenced-image directory and content
    /// layers in one call; the other `export_to_html*` methods are
    /// shorthands for it. Returns the HTML and, for
    /// [`ImageMode::Referenced`], the `(path, bytes)` artifacts.
    pub fn export_to_html_with(
        &self,
        options: &HtmlExportOptions,
    ) -> (String, Vec<(String, Vec<u8>)>) {
        crate::html::to_html(self, options)
    }
}

#[cfg(test)]
mod table_api_tests {
    use super::*;

    fn cell(
        text: &str,
        bbox: [f32; 4],
        (start_row, start_col): (usize, usize),
        (row_span, col_span): (usize, usize),
    ) -> TableCell {
        TableCell {
            text: text.into(),
            bbox: Some(bbox),
            start_row,
            start_col,
            row_span,
            col_span,
            column_header: false,
            row_header: false,
            row_section: false,
        }
    }

    fn table() -> Table {
        Table {
            rows: vec![
                vec!["Year".into(), "Ducks".into()],
                vec!["2019".into(), "120".into()],
            ],
            cells: Some(vec![
                cell("Year", [0.0, 0.0, 50.0, 10.0], (0, 0), (1, 1)),
                cell("Ducks", [50.0, 0.0, 100.0, 10.0], (0, 1), (1, 1)),
                cell("2019", [0.0, 10.0, 50.0, 20.0], (1, 0), (1, 1)),
                cell("120", [50.0, 10.0, 100.0, 20.0], (1, 1), (1, 1)),
            ]),
            ..Default::default()
        }
    }

    /// Declarative tables derive first-class cells from the structure
    /// overlay: continuation grids become span extents, `col_header` (or the
    /// row-0 fallback) becomes the header role — the XLSX/DOCX/HTML merge
    /// path into real `TableCell`s (#240).
    #[test]
    fn derive_cells_reads_spans_and_headers_from_structure() {
        let t = Table {
            rows: vec![
                vec!["Wide".into(), "Wide".into(), "C".into()],
                vec!["a".into(), "b".into(), "c".into()],
            ],
            structure: Some(TableStructure {
                header_row: vec![true, false],
                col_continuation: vec![vec![false, true, false], vec![false; 3]],
                row_continuation: vec![vec![false; 3], vec![false; 3]],
                row_header: Vec::new(),
                col_header: Vec::new(),
            }),
            ..Default::default()
        };
        let cells = t.derive_cells();
        assert_eq!(cells.len(), 5, "two anchors in row 0, three in row 1");
        let wide = &cells[0];
        assert_eq!((wide.col_span, wide.row_span), (2, 1));
        assert!(wide.column_header, "header_row band");
        assert!(cells.iter().skip(2).all(|c| !c.column_header));

        // Without any overlay: every position 1x1, row 0 the header
        // (docling's declarative default — the old JSON synthesis).
        let plain = Table {
            rows: vec![vec!["h".into()], vec!["x".into()]],
            ..Default::default()
        };
        let cells = plain.derive_cells();
        assert_eq!(cells.len(), 2);
        assert!(cells[0].column_header && !cells[1].column_header);
    }

    /// A spanning cell updates once: the record text and every covered grid
    /// position — a repair shows once in Markdown, not once per column.
    #[test]
    fn span_repair_updates_the_whole_cell() {
        let mut t = Table {
            rows: vec![
                vec!["Wide".into(), "Wide".into(), "C".into()],
                vec!["a".into(), "b".into(), "c".into()],
            ],
            cells: Some(vec![
                cell("Wide", [0.0, 0.0, 100.0, 10.0], (0, 0), (1, 2)),
                cell("C", [100.0, 0.0, 150.0, 10.0], (0, 2), (1, 1)),
            ]),
            ..Default::default()
        };
        // Update through the covered (non-anchor) position.
        assert!(t.set_cell_text(0, 1, "Fixed"));
        assert_eq!(
            t.rows[0],
            vec!["Fixed".to_string(), "Fixed".into(), "C".into()]
        );
        assert_eq!(t.cell_at(0, 1).unwrap().text, "Fixed");
        assert_eq!(t.cell_at(0, 1).unwrap().col_span, 2);
    }

    /// The OCR-repair loop (#238): locate a cell by an external box (best
    /// IoU), replace its text, and see the fix in the export — the grid is
    /// the single source of truth for every serializer.
    #[test]
    fn bbox_lookup_and_repair_flow_into_exports() {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::Table(table()));
        assert_eq!(doc.tables().count(), 1);

        let t = doc.tables_mut().next().unwrap();
        // A slightly-off OCR box still lands on the (1,1) cell.
        assert_eq!(t.find_cell_by_bbox([52.0, 11.0, 98.0, 19.0]), Some((1, 1)));
        assert_eq!(
            t.update_cell_by_bbox([52.0, 11.0, 98.0, 19.0], "125"),
            Some((1, 1))
        );
        assert_eq!(t.cell_text(1, 1), Some("125"));
        assert!(doc.export_to_markdown().contains("125"));

        // No overlap → no match, nothing changed.
        let t = doc.tables_mut().next().unwrap();
        assert_eq!(t.find_cell_by_bbox([500.0, 500.0, 600.0, 600.0]), None);
    }

    #[test]
    fn cell_accessors_bound_check_and_geometry_materializes() {
        let mut t = table();
        assert_eq!(t.cell_text(0, 0), Some("Year"));
        assert_eq!(t.cell_text(5, 0), None);
        assert!(!t.set_cell_text(0, 9, "x"), "outside the grid");
        assert_eq!(t.cell_bbox(1, 0), Some([0.0, 10.0, 50.0, 20.0]));

        // A geometry-less table materializes its box grid on first set.
        let mut plain = Table {
            rows: vec![vec!["a".into(), "b".into()]],
            ..Default::default()
        };
        assert_eq!(plain.cell_bbox(0, 1), None);
        assert!(!plain.set_cell_bbox(0, 5, [0.0; 4]), "outside the grid");
        assert!(plain.set_cell_bbox(0, 1, [1.0, 2.0, 3.0, 4.0]));
        assert_eq!(plain.cell_bbox(0, 1), Some([1.0, 2.0, 3.0, 4.0]));
        assert_eq!(plain.find_cell_by_bbox([1.5, 2.5, 2.5, 3.5]), Some((0, 1)));
    }
}
