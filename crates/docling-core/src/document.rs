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
    /// A single list item at the given nesting `level` (0 = top). For ordered
    /// items, `number` is the display number (honoring the list's `start`); it
    /// is unused for unordered items. `first_in_list` marks the first item of a
    /// list so the serializer can blank-line-separate adjacent sibling lists.
    ListItem {
        ordered: bool,
        number: u64,
        first_in_list: bool,
        text: String,
        level: u8,
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
    /// A node in docling's `furniture` content layer (page headers/footers, the
    /// HTML `<title>`, …). Markdown and JSON omit furniture by default; DocLang
    /// renders the wrapped node with a `<layer value="furniture"/>` head.
    Furniture(Box<Node>),
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
}

impl InlineRun {
    /// A run with no active formatting (renders as bare inline text).
    pub fn is_plain(&self) -> bool {
        !self.bold
            && !self.italic
            && !self.underline
            && !self.strike
            && !self.code
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

/// A simple row-major table. `rows[0]` is the header row.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub rows: Vec<Vec<String>>,
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
