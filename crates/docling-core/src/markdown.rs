//! Markdown serializer for [`DoclingDocument`].

use crate::document::{
    ContentLayer, ContentLayers, DoclingDocument, MarkdownExportOptions, Node, Table,
};

/// What docling's Markdown serializer writes for an item it has no component
/// for — a `KeyValueItem` (the XBRL fact graph) is the one such item a backend
/// produces.
const MISSING_KEY_VALUE_ITEM: &str = "<!-- missing-key-value-item -->";

/// How pictures are rendered (mirrors docling-core's `ImageRefMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageMode {
    /// `<!-- image -->` (docling's default, and the only mode without image data).
    #[default]
    Placeholder,
    /// `![Image](data:<mime>;base64,…)` — self-contained.
    Embedded,
    /// `![Image](<artifacts>/image_NNNNNN.<ext>)`; the bytes are returned for the
    /// caller to write.
    Referenced,
}

/// Serializer state threaded through the render walk.
struct Ctx {
    strict: bool,
    /// Render pictures' alt text (#676): strict Markdown, or the document's
    /// `picture_alt_text`.
    alt_text: bool,
    /// Emit compact `| a | b |` tables instead of the padded GitHub serializer.
    compact_tables: bool,
    images: ImageMode,
    artifacts_dir: String,
    /// (relative path, bytes) for each referenced image — written by the caller.
    artifacts: Vec<(String, Vec<u8>)>,
    pic_index: usize,
    /// Rendering the block content of a rich table cell (docling-core 2.94's
    /// `in_table_cell`, docling-core#540): a heading has no valid Markdown
    /// form inside a table, so it renders as plain text without `#` markers.
    in_table_cell: bool,
    /// docling-core's `MarkdownParams.page_break_placeholder`: the text that
    /// separates two pages ([`DoclingDocument::page_break_placeholder`]).
    /// `None` omits page breaks, docling's default.
    page_break: Option<String>,
    /// A page boundary has been crossed since the last rendered block, so the
    /// next block is preceded by the placeholder. docling yields its
    /// `_PageBreakNode` between two *items* whose `prov.page_no` differ, so a
    /// boundary before the first block or after the last one emits nothing,
    /// and a run of empty pages collapses into a single break.
    pending_page_break: bool,
    /// Whether any block has been rendered yet — for a streamer, across every
    /// earlier push too — which is what makes a boundary a *pending* break.
    emitted_any: bool,
    /// The content layers rendered ([`MarkdownExportOptions::layers`], #599).
    layers: ContentLayers,
    /// The layer the items being rendered sit on: `None` (body) at the
    /// root, the group's or wrapper's layer inside a non-body group or a
    /// [`Node::Furniture`] — what the layer gate compares against the set,
    /// as every item of a hidden sheet carries that sheet's layer upstream.
    current_layer: Option<ContentLayer>,
    /// Render a picture's nested text items ([`MarkdownExportOptions::traverse_pictures`]).
    traverse_pictures: bool,
    /// Keep the backends' HTML-entity escaping (`&amp;` …) in the output;
    /// off, it is undone ([`MarkdownExportOptions::escape_html`]).
    escape_html: bool,
    /// Keep the backends' `\_` escaping; off, it is undone.
    escape_underscores: bool,
    /// What a picture prints as without image data.
    image_placeholder: String,
    /// The plain-text export ([`MarkdownExportOptions::plain_text`], #613).
    plain: bool,
}

impl Ctx {
    /// Serializer state for `options` (the page-break setting is the
    /// document's, the strictness the caller's).
    fn new(doc: &DoclingDocument, strict: bool, options: &MarkdownExportOptions) -> Self {
        Ctx {
            strict,
            alt_text: strict || doc.picture_alt_text,
            compact_tables: doc.compact_tables,
            images: options.image_mode,
            artifacts_dir: options.artifacts_dir.clone(),
            artifacts: Vec::new(),
            pic_index: 0,
            in_table_cell: false,
            page_break: doc.page_break_placeholder.clone(),
            pending_page_break: false,
            emitted_any: false,
            layers: options.layers,
            current_layer: None,
            traverse_pictures: options.traverse_pictures,
            escape_html: options.escape_html,
            escape_underscores: options.escape_underscores,
            image_placeholder: options.image_placeholder.clone(),
            plain: options.plain_text,
        }
    }

    /// Whether an item on the layer being rendered is in the chosen set.
    fn layer_visible(&self) -> bool {
        self.layers.contains(self.current_layer)
    }

    /// The backends escape inline text for Markdown when they build the
    /// nodes (`&` → `&amp;`, `_` → `\_`, what docling-core's `post_process`
    /// does at serialization time); an export that turns an escaping off
    /// undoes it here, the way the JSON export recovers the raw text. Code,
    /// formulas and table cells never pass through — upstream leaves those
    /// unescaped too.
    fn unescape(&self, text: &str) -> String {
        let mut out = std::borrow::Cow::Borrowed(text);
        if !self.escape_html
            && (out.contains("&amp;") || out.contains("&lt;") || out.contains("&gt;"))
        {
            out = std::borrow::Cow::Owned(
                out.replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&amp;", "&"),
            );
        }
        if !self.escape_underscores && out.contains("\\_") {
            out = std::borrow::Cow::Owned(out.replace("\\_", "_"));
        }
        out.into_owned()
    }

    /// A text item's inline text: the strict-mode cleanup, then the
    /// escaping the options turn off undone.
    fn text(&self, text: &str) -> String {
        let text = strict_text(text, self.strict);
        if self.plain {
            self.unescape(&plain_inline(&self.drop_image_markers(&text)))
        } else {
            self.unescape(&text)
        }
    }

    /// A caption's text (the backends escape it, but strict mode leaves it
    /// alone): the escaping the options turn off undone, and, for the plain
    /// export, its inline markup dropped.
    fn caption(&self, text: &str) -> String {
        if self.plain {
            self.unescape(&plain_inline(text))
        } else {
            self.unescape(text)
        }
    }

    /// docling-core's `_md_line_breaks` for the Markdown export; the plain
    /// export's `PlainTextTextSerializer` keeps the text's own newlines.
    fn breaks(&self, text: &str) -> String {
        if self.plain {
            text.to_string()
        } else {
            md_line_breaks(text)
        }
    }

    /// A table for the plain export: a rich cell's text is its Markdown
    /// serialization in our model (bold runs, links, hard line breaks),
    /// where upstream serializes the cell with the plain serializer — so
    /// the markup goes before the grid is laid out, which also lets a
    /// once-bold number right-align like a plain one. A simple cell holds
    /// raw text and passes through unchanged.
    fn plain_table(&self, table: &Table) -> Table {
        let mut plain = table.clone();
        for cell in plain.rows.iter_mut().flatten() {
            if cell.contains(['*', '~', '`', '[', '\n', '<']) {
                let text = self.drop_image_markers(&strip_hard_breaks(cell));
                *cell = plain_inline(&text);
            }
        }
        plain
    }

    /// Pictures some backends fold into an item's text (an HTML `<img>`
    /// inside a `<li>`, a LaTeX figure in a paragraph) arrive as baked
    /// `<!-- image -->` lines — docling's picture *children* of the item,
    /// which its serializer prints with the active image placeholder. The
    /// plain export's placeholder is empty, so such a line goes, as a
    /// picture item would.
    fn drop_image_markers(&self, text: &str) -> String {
        const MARKER: &str = "<!-- image -->";
        if !text.contains(MARKER) && !text.contains("```") {
            return text.to_string();
        }
        // Block by block (`\n\n`, the serializer's part delimiter): a block
        // left empty goes with its delimiter, as docling joins only the parts
        // that carry text.
        text.split("\n\n")
            .filter_map(|block| {
                let lines: Vec<String> = block
                    .split('\n')
                    .filter_map(|line| {
                        // A folded code block's fences go too: the plain
                        // export prints code unfenced.
                        if line.trim() == MARKER || is_fence_line(line.trim()) {
                            return None;
                        }
                        Some(line.replace(MARKER, &self.image_placeholder))
                    })
                    .collect();
                (!lines.is_empty()).then(|| lines.join("\n"))
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Render a document to a Markdown string (pictures as placeholders).
///
/// `strict` selects the serializer-level behaviours that differ between
/// docling-legacy output and cleaner Markdown — currently the code-fence
/// language (legacy drops it, strict keeps it).
pub fn to_markdown(doc: &DoclingDocument, strict: bool) -> String {
    to_markdown_images(doc, strict, ImageMode::Placeholder, "artifacts").0
}

/// Render to Markdown with an explicit picture [`ImageMode`]. Returns the
/// Markdown and, for [`ImageMode::Referenced`], the `(path, bytes)` of each image
/// the caller should write (relative to the Markdown file).
pub fn to_markdown_images(
    doc: &DoclingDocument,
    strict: bool,
    images: ImageMode,
    artifacts_dir: &str,
) -> (String, Vec<(String, Vec<u8>)>) {
    to_markdown_with_options(
        doc,
        strict,
        &MarkdownExportOptions {
            image_mode: images,
            artifacts_dir: artifacts_dir.to_string(),
            ..MarkdownExportOptions::default()
        },
    )
}

/// Render to Markdown per `options` ([`MarkdownExportOptions`], #599):
/// content layers, picture traversal, escaping, the image placeholder and
/// the image mode. Returns the Markdown and the referenced-image artifacts.
pub fn to_markdown_with_options(
    doc: &DoclingDocument,
    strict: bool,
    options: &MarkdownExportOptions,
) -> (String, Vec<(String, Vec<u8>)>) {
    let mut ctx = Ctx::new(doc, strict, options);
    let mut blocks: Vec<String> = Vec::new();
    render(&doc.nodes, &mut blocks, &mut ctx);
    let mut body = blocks.join("\n\n");
    // Strict mode only: turn recovered source hyperlinks into Markdown links.
    // docling's standard pipeline drops them, so doing this in legacy mode would
    // diverge from docling — hence strict-only, leaving conformance output intact.
    if strict && !options.plain_text && !doc.links.is_empty() {
        body = apply_links(&body, &doc.links);
    }
    // The YAML front matter (#679) opens the document when its output mode
    // asks for it; never in the plain-text export.
    let front = if doc.metadata_output.front_matter() && !options.plain_text {
        doc.metadata.to_front_matter()
    } else {
        String::new()
    };
    let md = match (front.is_empty(), body.is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!("{body}\n"),
        (false, true) => front,
        (false, false) => format!("{front}\n{body}\n"),
    };
    (md, ctx.artifacts)
}

/// Render the block content of a *rich table cell* to Markdown — what
/// docling-core's table serializer does for a `RichTableCell`
/// (`doc_serializer.serialize(item, in_table_cell=True)`): the cell's
/// paragraphs, lists and flattened nested tables render as in a document, but a
/// heading loses its `#` markers (docling-core#540 — the Markdown spec has no
/// headings inside tables). Pictures stay placeholders. The caller flattens the
/// result into its cell text; the table serializer later turns the newlines
/// into spaces.
pub fn to_markdown_table_cell(doc: &DoclingDocument, strict: bool) -> String {
    let mut ctx = Ctx::new(doc, strict, &MarkdownExportOptions::default());
    ctx.in_table_cell = true;
    ctx.artifacts_dir = String::new();
    // A rich cell is one page's content; its sub-document carries no page
    // boundaries and docling's `_iterate_items` runs the page-break scan
    // over the document root only.
    ctx.page_break = None;
    let mut blocks: Vec<String> = Vec::new();
    render(&doc.nodes, &mut blocks, &mut ctx);
    blocks.join("\n\n")
}

/// Wrap each recovered link's anchor text in Markdown `[anchor](href)`. Anchors
/// arrive cleaned (curly quotes/dashes already normalized) but un-escaped, so we
/// match against the body's HTML-escaped (`&`/`<`/`>`) form, the way prose nodes
/// were serialized. Links are consumed in document order from a moving cursor, so
/// a repeated anchor (e.g. two "issues") links its successive occurrences rather
/// than all pointing at the first. An anchor that can't be located is skipped
/// (its text may have been split across a line wrap or table cell).
fn apply_links(body: &str, links: &[(String, String)]) -> String {
    let mut out = body.to_string();
    let mut cursor = 0usize;
    for (anchor, href) in links {
        let anchor = anchor
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        if anchor.is_empty() {
            continue;
        }
        if let Some(rel) = out[cursor..].find(&anchor) {
            let at = cursor + rel;
            // Don't relink inside an already-emitted `](` Markdown link target.
            // An unsafe destination (#860) leaves the anchor as prose.
            let replacement = crate::hyperlink::markdown_link(&anchor, href);
            out.replace_range(at..at + anchor.len(), &replacement);
            cursor = at + replacement.len();
        }
    }
    out
}

/// Like [`apply_links`] but over a single chunk, consuming from a shared queue so
/// the same `[anchor](href)` rewriting can be applied incrementally as Markdown is
/// streamed out. Each queued link is matched (in document order) against `chunk`
/// and rewritten in place; a link whose anchor is not in this chunk is carried
/// forward in the queue for a later chunk. Anchors are recovered in document
/// order and a chunk is always a contiguous run of whole blocks, so this
/// reproduces [`apply_links`]' single moving cursor: the link lands in whichever
/// chunk contains its anchor, identically to the buffered path. (A link whose
/// anchor never appears is carried to the end and dropped — the same no-op
/// `apply_links` performs for an unlocatable anchor.)
fn apply_links_chunk(chunk: &str, queue: &mut Vec<(String, String)>) -> String {
    let mut out = chunk.to_string();
    let mut cursor = 0usize;
    let mut carried: Vec<(String, String)> = Vec::new();
    for (anchor_raw, href) in std::mem::take(queue) {
        let anchor = anchor_raw
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        if anchor.is_empty() {
            continue;
        }
        if let Some(rel) = out[cursor..].find(&anchor) {
            let at = cursor + rel;
            let replacement = crate::hyperlink::markdown_link(&anchor, &href);
            out.replace_range(at..at + anchor.len(), &replacement);
            cursor = at + replacement.len();
        } else {
            // Not in this chunk; try again when its block is flushed.
            carried.push((anchor_raw, href));
        }
    }
    *queue = carried;
    out
}

/// Incremental Markdown serializer: feed finalized, in-document-order batches of
/// [`Node`]s and receive Markdown chunks whose concatenation is **byte-identical**
/// to [`to_markdown_images`] over the same nodes. This is the streaming
/// counterpart of the buffered serializer — used to emit a document's Markdown in
/// chunks (e.g. page by page, as the parallel PDF pipeline finishes pages) instead
/// of building the whole string up front.
///
/// [`ImageMode::Placeholder`] and [`ImageMode::Embedded`] render inline.
/// [`ImageMode::Referenced`] additionally hands each picture's bytes out through
/// [`take_artifacts`](Self::take_artifacts) — construct with
/// [`with_artifacts`](Self::with_artifacts) and drain after every push so the
/// bytes can be written to disk as pages finish instead of accumulating for the
/// whole document (issue #80's memory-bounded image handling).
///
/// Each [`push`](Self::push) must contain whole blocks in reading order: a caller
/// must not split a run of list items across two pushes (the run would render as
/// two separate lists). Finalized PDF page batches already satisfy this.
pub struct MarkdownStreamer {
    strict: bool,
    images: ImageMode,
    compact_tables: bool,
    /// [`DoclingDocument::picture_alt_text`] (#676).
    picture_alt_text: bool,
    /// The YAML front matter (#679) still to be written, ahead of the first
    /// chunk — or alone, from [`finish`](Self::finish), when nothing else
    /// comes.
    front_matter: Option<String>,
    /// Whether any non-empty chunk has been emitted yet (drives `\n\n` joins and
    /// the trailing newline).
    emitted_any: bool,
    /// Recovered links not yet placed (strict mode), consumed in document order.
    links: Vec<(String, String)>,
    /// Referenced mode: the link prefix, the not-yet-drained `(path, bytes)`
    /// artifacts, and the running image number (continues across pushes so the
    /// stream matches the buffered serializer's `image_000000…` numbering).
    artifacts_dir: String,
    artifacts: Vec<(String, Vec<u8>)>,
    pic_index: usize,
    /// [`DoclingDocument::page_break_placeholder`] and the boundary carried
    /// over from the previous push (a page batch opens with its page marker,
    /// so the break it implies is paid by that batch's first block).
    page_break: Option<String>,
    pending_page_break: bool,
    /// Whether an earlier push rendered an *item* — the page-break scan's
    /// state ([`Ctx`]'s `emitted_any`). It differs from `emitted_any` (text
    /// written) only when every block of the earlier pushes was empty and
    /// dropped (#605): a page break before the next item is still owed then,
    /// exactly as in the buffered export.
    items_emitted: bool,
    /// A picture a push ended on, in the placeholder mode: whether its
    /// marker prints depends on the item after it (#858, a `mermaid` code
    /// item takes its place), which the next push brings — it joins that
    /// push, or [`finish`](Self::finish) renders it.
    held: Option<Node>,
    /// The export options beyond the image mode (#599): layers, picture
    /// traversal, escaping, the placeholder — [`with_export_options`](Self::with_export_options).
    layers: ContentLayers,
    traverse_pictures: bool,
    escape_html: bool,
    escape_underscores: bool,
    image_placeholder: String,
    plain: bool,
}

impl MarkdownStreamer {
    /// Create a streamer. `compact_tables` mirrors [`DoclingDocument::compact_tables`].
    /// For [`ImageMode::Referenced`] use [`with_artifacts`](Self::with_artifacts).
    pub fn new(strict: bool, images: ImageMode, compact_tables: bool) -> Self {
        debug_assert!(
            images != ImageMode::Referenced,
            "referenced image mode needs an artifacts dir; use with_artifacts"
        );
        Self::with_artifacts(strict, images, compact_tables, "artifacts")
    }

    /// Like [`new`](Self::new) but with the artifacts link prefix, allowing
    /// [`ImageMode::Referenced`]: pictures render as
    /// `![Image](<artifacts_dir>/image_NNNNNN.<ext>)` and each push's image
    /// bytes wait in [`take_artifacts`](Self::take_artifacts) for the caller to
    /// write. The concatenated chunks and the artifact list match the buffered
    /// [`to_markdown_images`] byte-for-byte.
    pub fn with_artifacts(
        strict: bool,
        images: ImageMode,
        compact_tables: bool,
        artifacts_dir: &str,
    ) -> Self {
        Self {
            strict,
            images,
            compact_tables,
            picture_alt_text: false,
            front_matter: None,
            emitted_any: false,
            links: Vec::new(),
            artifacts_dir: artifacts_dir.to_string(),
            artifacts: Vec::new(),
            pic_index: 0,
            page_break: None,
            pending_page_break: false,
            items_emitted: false,
            held: None,
            layers: ContentLayers::BODY,
            traverse_pictures: false,
            escape_html: true,
            escape_underscores: true,
            image_placeholder: "<!-- image -->".to_string(),
            plain: false,
        }
    }

    /// Take every setting of `options` ([`MarkdownExportOptions`], #599):
    /// the image mode and artifacts directory the constructors took, plus the
    /// content layers, picture traversal, escaping and image placeholder.
    /// The concatenated chunks match the buffered
    /// [`to_markdown_with_options`] byte-for-byte. Set before the first
    /// [`push`](Self::push).
    pub fn with_export_options(mut self, options: &MarkdownExportOptions) -> Self {
        self.images = options.image_mode;
        self.artifacts_dir = options.artifacts_dir.clone();
        self.layers = options.layers;
        self.traverse_pictures = options.traverse_pictures;
        self.escape_html = options.escape_html;
        self.escape_underscores = options.escape_underscores;
        self.image_placeholder = options.image_placeholder.clone();
        self.plain = options.plain_text;
        self
    }

    /// Insert `placeholder` between pages, mirroring
    /// [`DoclingDocument::page_break_placeholder`] for the buffered path (the
    /// concatenated chunks stay byte-identical to it). `None` — the default —
    /// omits page breaks. Set before the first [`push`](Self::push).
    pub fn with_page_break_placeholder(mut self, placeholder: Option<String>) -> Self {
        self.page_break = placeholder;
        self
    }

    /// Render pictures' alt text (#676) — [`DoclingDocument::picture_alt_text`]
    /// for the stream; strict Markdown renders it regardless. Set before the
    /// first [`push`](Self::push).
    pub fn with_picture_alt_text(mut self, on: bool) -> Self {
        self.picture_alt_text = on;
        self
    }

    /// Open the stream with the YAML front matter of `metadata` (#679) —
    /// what the buffered export writes for a document whose
    /// [`DoclingDocument::metadata_output`] is `Frontmatter`. Empty
    /// metadata writes nothing; the plain-text export never writes it.
    pub fn with_metadata(mut self, metadata: &crate::metadata::DocumentMetadata) -> Self {
        self.front_matter = (!self.plain)
            .then(|| metadata.to_front_matter())
            .filter(|f| !f.is_empty());
        self
    }

    /// The `(relative path, bytes)` of images rendered by pushes since the last
    /// drain ([`ImageMode::Referenced`] only — empty otherwise). Paths are
    /// relative to the Markdown file, i.e. they start with the configured
    /// artifacts dir.
    pub fn take_artifacts(&mut self) -> Vec<(String, Vec<u8>)> {
        std::mem::take(&mut self.artifacts)
    }

    /// Render one finalized batch of nodes (plus any links recovered from the same
    /// span, in document order) into the next Markdown chunk. Returns an empty
    /// string when the batch produces no output (e.g. empty tables/pictures), in
    /// which case nothing should be written.
    pub fn push(&mut self, nodes: &[Node], links: &[(String, String)]) -> String {
        self.push_holding(nodes, links, true)
    }

    /// [`push`](Self::push); with `hold`, a picture the batch ends on is
    /// held back for the next push (`finish` renders without holding).
    fn push_holding(&mut self, nodes: &[Node], links: &[(String, String)], hold: bool) -> String {
        self.links.extend(links.iter().cloned());
        // A picture held back from the last push leads this one; a picture
        // this push ends on is held back in turn (placeholder mode only —
        // the one in which the next item can change what it prints).
        let images = self.images;
        let ends_on_picture = |nodes: &[Node]| {
            matches!(images, ImageMode::Placeholder)
                && matches!(nodes.last(), Some(Node::Picture { .. }))
        };
        let mut owned: Vec<Node> = Vec::new();
        let mut nodes = nodes;
        if self.held.is_some() || (hold && ends_on_picture(nodes)) {
            owned.extend(self.held.take());
            owned.extend(nodes.iter().cloned());
            if hold && ends_on_picture(&owned) {
                self.held = owned.pop();
            }
            nodes = &owned;
        }
        let mut ctx = Ctx {
            strict: self.strict,
            alt_text: self.strict || self.picture_alt_text,
            compact_tables: self.compact_tables,
            images: self.images,
            artifacts_dir: std::mem::take(&mut self.artifacts_dir),
            artifacts: std::mem::take(&mut self.artifacts),
            pic_index: self.pic_index,
            in_table_cell: false,
            page_break: std::mem::take(&mut self.page_break),
            pending_page_break: self.pending_page_break,
            emitted_any: self.items_emitted,
            layers: self.layers,
            current_layer: None,
            traverse_pictures: self.traverse_pictures,
            escape_html: self.escape_html,
            escape_underscores: self.escape_underscores,
            image_placeholder: std::mem::take(&mut self.image_placeholder),
            plain: self.plain,
        };
        let mut blocks: Vec<String> = Vec::new();
        render(nodes, &mut blocks, &mut ctx);
        self.image_placeholder = std::mem::take(&mut ctx.image_placeholder);
        self.artifacts_dir = std::mem::take(&mut ctx.artifacts_dir);
        self.artifacts = std::mem::take(&mut ctx.artifacts);
        self.pic_index = ctx.pic_index;
        self.page_break = std::mem::take(&mut ctx.page_break);
        self.pending_page_break = ctx.pending_page_break;
        self.items_emitted = ctx.emitted_any;
        if blocks.is_empty() {
            return String::new();
        }
        let mut body = blocks.join("\n\n");
        if self.strict && !self.plain && !self.links.is_empty() {
            body = apply_links_chunk(&body, &mut self.links);
        }
        let chunk = if let Some(front) = self.front_matter.take() {
            format!("{front}\n{body}")
        } else if self.emitted_any {
            format!("\n\n{body}")
        } else {
            body
        };
        self.emitted_any = true;
        chunk
    }

    /// Emit the trailing newline that finishes the document (empty if no content
    /// was produced). Call exactly once, after the final [`push`](Self::push).
    pub fn finish(mut self) -> String {
        let mut out = String::new();
        if self.held.is_some() {
            out.push_str(&self.push_holding(&[], &[], false));
        }
        if let Some(front) = self.front_matter {
            out.push_str(&front);
        } else if self.emitted_any {
            out.push('\n');
        }
        out
    }
}

/// In `strict` mode, rewrite inline text for readability rather than byte-for-byte
/// docling fidelity: undo the legacy `\_` underscore escaping, and tighten stray
/// spaces around punctuation (`[ 37 , 36 ]` → `[37, 36]`, `( x )` → `(x)`). This
/// cleans up both the PDF backend's glyph-split spacing and the space the legacy
/// emphasis serialization leaves before punctuation (`*a* ,` → `*a*,`).
/// Legacy/default output keeps docling's spacing untouched. Only inline text
/// nodes pass through here — code blocks and table cells are left alone.
fn strict_text(text: &str, strict: bool) -> String {
    if !strict {
        return text.to_string();
    }
    // A list item's leading GFM task box (`[ ] todo`, a docling-JSON check
    // list item, #1033) is markup, not a stray space inside brackets.
    if let Some(rest) = text.strip_prefix("[ ] ") {
        return format!("[ ] {}", strict_text(rest, strict));
    }
    text.replace("\\_", "_")
        .replace(" ,", ",")
        .replace(" .", ".")
        .replace(" ;", ";")
        .replace(" )", ")")
        .replace("( ", "(")
        .replace(" ]", "]")
        .replace("[ ", "[")
}

/// The longest run of consecutive backticks in `text` (docling-core
/// 2.103.1's `_longest_backtick_run`, #859).
pub fn longest_backtick_run(text: &str) -> usize {
    let (mut best, mut run) = (0, 0);
    for c in text.chars() {
        if c == '`' {
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
    }
    best
}

/// `text` as a Markdown code span no backtick run inside can close
/// (docling-core's `_code_span`, #859): one backtick more than the longest
/// run in it, and a space on both sides when the text starts or ends with
/// a backtick — the pad CommonMark strips again — so `` a`b `` is
/// ``` ``a`b`` ``` and `` `x `` is ``` `` `x `` ```.
pub fn code_span(text: &str) -> String {
    let ticks = "`".repeat(longest_backtick_run(text) + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{ticks}{pad}{text}{pad}{ticks}")
}

/// A fenced code block whose fence out-lengths every backtick run in `body`
/// (docling-core's `_code_block`, #859): three backticks, or one more than
/// the longest run inside, so a body quoting a ``` fence cannot close the
/// block early. `language` rides on the opening fence (empty for none).
pub fn code_fence(language: &str, body: &str) -> String {
    let fence = "`".repeat(longest_backtick_run(body).max(2) + 1);
    format!("{fence}{language}\n{body}\n{fence}")
}

/// The code span (or flattened fence) opening at `chars[i]`, a run of
/// backticks: `(content start, content end, index after the closing run)`.
/// The closing run is the first later run of exactly the same length — a
/// run of another length is literal inside the span, as in CommonMark. A
/// span of two or more backticks sheds the one-space pad [`code_span`]
/// adds; a single-backtick span keeps its content as it stands, the shape
/// every reader of the baked text relied on.
pub(crate) fn code_span_bounds(chars: &[char], i: usize) -> Option<(usize, usize, usize)> {
    let n = chars[i..].iter().take_while(|&&c| c == '`').count();
    if n == 0 {
        return None;
    }
    let mut k = i + n;
    while k < chars.len() {
        if chars[k] != '`' {
            k += 1;
            continue;
        }
        let run = chars[k..].iter().take_while(|&&c| c == '`').count();
        if run == n {
            let (mut s, mut e) = (i + n, k);
            if n > 1
                && e - s >= 2
                && chars[s] == ' '
                && chars[e - 1] == ' '
                && chars[s..e].iter().any(|&c| c != ' ')
            {
                s += 1;
                e -= 1;
            }
            return Some((s, e, k + run));
        }
        k += run;
    }
    None
}

/// A line that is a code fence: three or more backticks and nothing else.
fn is_fence_line(line: &str) -> bool {
    line.len() >= 3 && line.bytes().all(|b| b == b'`')
}

/// The plain export's inline text (#613): the Markdown markers docling-core's
/// `PlainTextDocSerializer` never writes — `serialize_bold` / `_italic` /
/// `_strikethrough` return the text, `serialize_hyperlink` the label, and
/// `format_code_blocks=False` drops inline code's backticks. The backends
/// bake those markers into an item's Markdown text (`***x***`, `**x**`,
/// `*x*`, `~~x~~`, `` `x` ``, `[label](url)`, the set DocLang's run parser
/// reads too), so they are taken out here, nested ones included. A marker
/// counts only when it closes and its content is tight against both
/// markers (CommonMark's flanking rule): `2 * 3 * 4` is arithmetic and IBM
/// i's `*USE … have *OBJMGT` are special values, not emphasis, and stay as
/// they are — docling's own emphasis around a space (WebVTT's `* *`) stays
/// literal too. Inline `$…$` formulas pass through untouched.
pub(crate) fn plain_inline(text: &str) -> String {
    if !text.contains(['*', '~', '`', '[']) {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    plain_inline_into(&chars, &mut out);
    out
}

fn plain_inline_into(chars: &[char], out: &mut String) {
    // A backslash-escaped marker (`\*`, the RIS backend's pseudonym stars,
    // #902) closes no span.
    let find = |open: usize, pat: &[char]| -> Option<usize> {
        chars
            .get(open..)?
            .windows(pat.len())
            .enumerate()
            .position(|(k, w)| w == pat && (open + k == 0 || chars[open + k - 1] != '\\'))
            .map(|p| open + p)
    };
    // An emphasis span opening at `i` with marker `pat`: its closing index
    // when the content is non-empty and tight against both markers.
    let span = |i: usize, pat: &[char]| -> Option<usize> {
        if !chars[i..].starts_with(pat) {
            return None;
        }
        let open = i + pat.len();
        let end = find(open, pat)?;
        (end > open && !chars[open].is_whitespace() && !chars[end - 1].is_whitespace())
            .then_some(end)
    };
    let mut i = 0;
    while i < chars.len() {
        // `\*` is a literal star, which is what the Markdown renders.
        if chars[i] == '\\' && chars.get(i + 1) == Some(&'*') {
            out.push('*');
            i += 2;
            continue;
        }
        let mut matched = false;
        for pat in [&['*', '*', '*'][..], &['*', '*'], &['~', '~'], &['*']] {
            if let Some(end) = span(i, pat) {
                plain_inline_into(&chars[i + pat.len()..end], out);
                i = end + pat.len();
                matched = true;
                break;
            }
        }
        if matched {
            continue;
        }
        // Inline code: content tight against both backticks, as docling
        // writes it — a backtick opening a quotation (`` `like this’ ``,
        // JATS prose) stays.
        if chars[i] == '`' {
            if let Some((s, e, after)) = code_span_bounds(chars, i).filter(|&(s, e, _)| {
                e > s && !chars[s].is_whitespace() && !chars[e - 1].is_whitespace()
            }) {
                out.extend(&chars[s..e]);
                i = after;
                continue;
            }
        }
        // An inline formula is LaTeX, printed as it is — its brackets and
        // stars are not Markdown.
        if chars[i] == '$' {
            if let Some(end) = find(i + 1, &['$']) {
                out.extend(&chars[i..=end]);
                i = end + 1;
                continue;
            }
        }
        // A link: the `]` closing this `[` (brackets nest) directly followed
        // by `(` — a citation `[23,24]` before a later link is no anchor.
        if chars[i] == '[' {
            if let Some(close) = closing_bracket(chars, i) {
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(endp) = crate::doclang::link_dest_end(chars, close + 2) {
                        plain_inline_into(&chars[i + 1..close], out);
                        i = endp + 1;
                        continue;
                    }
                }
            }
        }
        // A run of the same marker that opened nothing stays literal as a
        // whole, so `***` before a lone `*` is not re-read from its middle.
        let c = chars[i];
        out.push(c);
        i += 1;
        if matches!(c, '*' | '~') {
            while i < chars.len() && chars[i] == c {
                out.push(c);
                i += 1;
            }
        }
    }
}

/// The index of the `]` matching the `[` at `open`, nested brackets
/// balanced; `None` when it never closes.
fn closing_bracket(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (k, c) in chars.iter().enumerate().skip(open + 1) {
        match c {
            '[' => depth += 1,
            ']' if depth == 0 => return Some(k),
            ']' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// docling-core 2.92's `_md_line_breaks` (docling-core#721): a single `\n`
/// inside an item's text becomes a GFM hard line break (`"  \n"`, two trailing
/// spaces) so renderers honour it; a blank line (`\n\n`) is a paragraph break
/// and stays as is — the document scope already joins blocks with `\n\n`.
/// Applied to body text, list items and standalone caption items, never to
/// code/formulas, nor to the captions a table or picture carries: docling-core's
/// `serialize_captions` prints those as they are (#769).
fn md_line_breaks(text: &str) -> String {
    if !text.contains('\n') {
        return text.to_string();
    }
    text.split("\n\n")
        .map(|para| para.replace('\n', "  \n"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Undo [`md_line_breaks`] on a rich table cell's flattened Markdown so the
/// non-Markdown exports (JSON `text`, LaTeX cells) see the cell's raw line
/// breaks, as docling's do — a rich cell's text is its Markdown serialization
/// in our model, and the two trailing spaces are a Markdown-only marker.
pub(crate) fn strip_hard_breaks(text: &str) -> String {
    if text.contains("  \n") {
        text.replace("  \n", "\n")
    } else {
        text.to_string()
    }
}

/// docling-core's `_heading_line_breaks`: a GFM heading cannot span lines, so a
/// newline inside heading text collapses to a space (`# Hello World`, not a
/// broken `# Hello\nWorld`).
fn heading_line_breaks(text: &str) -> String {
    text.replace('\n', " ")
}

fn render(nodes: &[Node], blocks: &mut Vec<String>, ctx: &mut Ctx) {
    let mut i = 0;
    while i < nodes.len() {
        let before = blocks.len();
        match &nodes[i] {
            // A page boundary: the explicit `PageBreak` (slides, DjVu / DocTags
            // pages) or the `PageInfo` marker that opens every PDF page and
            // spreadsheet sheet — a sheet boundary carries both, and the flag
            // absorbs the pair into one break. docling's `_iterate_items`
            // yields a `_PageBreakNode` only between two items on different
            // pages (a group's leading item counts for the group), which is
            // exactly "a boundary between two rendered blocks": nothing before
            // the first block, nothing after the last, consecutive boundaries
            // — empty or furniture-only pages — collapsed into one.
            Node::PageBreak | Node::PageInfo { .. } => {
                if ctx.page_break.is_some() && ctx.emitted_any {
                    ctx.pending_page_break = true;
                }
                i += 1;
            }
            Node::ListItem { .. } => {
                let start = i;
                i += 1;
                loop {
                    match nodes.get(i) {
                        Some(Node::ListItem { .. } | Node::ListText { .. }) => i += 1,
                        // An empty paragraph between two list items is absorbed
                        // into the run — docling keeps such a ListGroup
                        // contiguous rather than splitting it.
                        Some(Node::Paragraph { text })
                            if text.is_empty()
                                && matches!(nodes.get(i + 1), Some(Node::ListItem { .. })) =>
                        {
                            i += 1
                        }
                        _ => break,
                    }
                }
                render_list_run(&nodes[start..i], blocks, ctx);
            }
            other => {
                let before_item = blocks.len();
                render_one(other, blocks, ctx);
                // A picture that is the drawing a `mermaid` code item was
                // extracted from (#858): in the placeholder mode the
                // `<!-- image -->` says nothing the graph does not, so the
                // marker — the last block a picture pushes — goes; an
                // embedded or referenced image is content and stays.
                if matches!(other, Node::Picture { .. })
                    && matches!(ctx.images, ImageMode::Placeholder)
                    && blocks.len() > before_item
                    && matches!(
                        nodes.get(i + 1),
                        Some(Node::Code { language: Some(l), .. }) if l == "mermaid"
                    )
                {
                    blocks.pop();
                }
                i += 1;
            }
        }
        if blocks.len() > before {
            // docling-core joins only the parts that carry text
            // (`"\n\n".join(p.text for p in parts if p.text)`, at the
            // document and every group scope), so an item that renders an
            // empty block — a picture or chart under `image_placeholder: ""`,
            // an empty field part or inline group — adds no blank lines
            // (#605). It still counts as an item for the page-break scan:
            // upstream's page-break marker sits between the *items* of two
            // pages and is never empty itself, so the placeholder below still
            // goes in (an empty placeholder keeps its doubled delimiter).
            let kept: Vec<String> = blocks.drain(before..).filter(|b| !b.is_empty()).collect();
            blocks.extend(kept);
            if ctx.pending_page_break {
                // The placeholder is a block of its own, joined by the document
                // delimiter like docling's `_PageBreakSerResult` part — an
                // empty placeholder therefore leaves the doubled `\n\n`
                // upstream leaves too.
                if let Some(placeholder) = &ctx.page_break {
                    blocks.insert(before, placeholder.clone());
                }
                ctx.pending_page_break = false;
            }
            ctx.emitted_any = true;
        }
    }
}

/// Render a contiguous run of list items.
///
/// Ordered items use their explicit `number`. A new sibling list (marked by
/// `first_in_list`) at the same depth is separated by a blank line, matching
/// docling-core's serializer.
fn render_list_run(items: &[Node], blocks: &mut Vec<String>, ctx: &Ctx) {
    let mut lines: Vec<String> = Vec::new();
    // Whether a top-level item has been rendered yet — a fresh-list flag on
    // the very first item opens nothing.
    let mut any_top = false;

    for item in items {
        if let Node::ListText { level, text, glue } = item {
            let text = ctx.text(text);
            match lines.last_mut() {
                Some(last) if *glue => last.push_str(&text),
                _ => lines.push(format!("{}{text}", "    ".repeat(*level as usize))),
            }
            continue;
        }
        let Node::ListItem {
            ordered,
            number,
            first_in_list,
            text,
            level,
            marker: orig_marker,
            location: _,
            dclx: _,
            split: _,
            href: _,
            layer,
        } = item
        else {
            continue;
        };
        // docling's content-layer filtering: an item renders when its layer
        // — its own, else the layer of the group it sits in — is in the set
        // (body only by default, so a furniture list item is omitted).
        if !ctx.layers.contains(layer.or(ctx.current_layer)) {
            continue;
        }
        let level = *level as usize;

        // A new sibling list at the top level gets a blank line — and only the
        // backend knows where one starts (`first_in_list`: Word's `numId`
        // changing, an HTML `<ul>` closing, a Markdown bullet switching
        // `-`→`*`). The serializer used to guess it from a kind flip or a
        // number gap as well, which split lists docling keeps whole (an
        // AsciiDoc `1.` … `5.`, mixed `*`/`1.` markers) — #385. Only at the
        // top level: nested sibling groups are children of a list item, and
        // docling joins an item's children without blank lines.
        if level == 0 {
            if any_top && *first_in_list {
                lines.push(String::new());
            }
            any_top = true;
        }

        let indent = "    ".repeat(level);
        lines.push(format!(
            "{indent}{} {}",
            list_item_marker(orig_marker.as_deref().unwrap_or(""), *ordered, *number),
            list_item_text(text, ctx)
        ));
    }

    // A run consisting only of furniture (content-layer-filtered) items yields no
    // lines; pushing an empty block here would surface as a stray blank line.
    if !lines.is_empty() {
        blocks.push(lines.join("\n"));
    }
}

/// docling-core's list item marker (`orig_list_item_marker_mode=AUTO`,
/// `ensure_valid_list_item_marker=True`; docling-core 2.100+): a marker that
/// is already valid Markdown — `-`, `*`, `+`, or digits and a dot (Python's
/// `\d+\.` admits every Unicode decimal digit, so a DOCX `decimalFullWidth`
/// `１.` stays as it is, docling#4336) — prints verbatim; any other marker
/// holding a letter or digit is informative (`a)`, `①`, `第九条`) and prints
/// after a `-`; a markerless item of an enumerated list is numbered by its
/// position, and every other item gets a `-`.
fn list_item_marker(marker: &str, ordered: bool, number: u64) -> String {
    let already_valid = matches!(marker, "-" | "*" | "+")
        || marker
            .strip_suffix('.')
            .is_some_and(|d| !d.is_empty() && d.chars().all(char::is_numeric));
    if already_valid {
        return marker.to_string();
    }
    let outer = if ordered && marker.is_empty() {
        format!("{number}.")
    } else {
        "-".to_string()
    };
    if marker.chars().any(char::is_alphanumeric) {
        format!("{outer} {marker}")
    } else {
        outer
    }
}

/// A list item's Markdown body. The GFM hard-line-break rule (docling-core#721)
/// applies to the item's own text; pictures the HTML backend folded into the
/// item (`"\n[alt\n]<!-- image -->"` per `<img>` inside the `<li>`) are
/// docling's picture *children* of the item, which its serializer prints after
/// the item line with plain newlines — so a folded tail keeps its newlines
/// unmarked. The tail is recognised structurally: every line after the first is
/// an image marker or an alt caption directly followed by one.
fn list_item_text(text: &str, ctx: &Ctx) -> String {
    let escaped = ctx.text(text);
    if ctx.plain {
        return escaped;
    }
    if let Some((own, tail)) = escaped.split_once('\n') {
        if is_folded_child_tail(tail) {
            return format!("{}\n{tail}", md_line_breaks(own));
        }
    }
    md_line_breaks(&escaped)
}

/// Whether everything after a list item's own first line is a folded *child*
/// block rather than a continuation of the item's text: an image marker
/// (optionally preceded by its caption/alt line) or a fenced code block. The
/// AsciiDoc backend indents such a block to the item's own depth (as
/// docling-core's list serializer does for each part it emits), so a leading
/// indent is ignored here.
fn is_folded_child_tail(tail: &str) -> bool {
    const MARKER: &str = "<!-- image -->";
    let mut lines = tail.split('\n').peekable();
    let mut any = false;
    while let Some(line) = lines.next() {
        let line = line.trim_start();
        if line == MARKER {
            any = true;
        } else if is_fence_line(line) {
            // Skip the block's body; an unclosed fence is not a folded child.
            // The closing fence is the opening one's length (#859).
            loop {
                match lines.next() {
                    Some(l) if l.trim_start() == line => break,
                    Some(_) => {}
                    None => return false,
                }
            }
            any = true;
        } else if lines.next().map(str::trim_start) == Some(MARKER) {
            any = true; // an alt caption line, then its marker
        } else {
            return false;
        }
    }
    any
}

/// Render one item: docling's content-layer gate first — an item whose layer
/// (the group's or wrapper's it sits in, else the body) is outside the set
/// renders nothing, while a container still walks its children — then the
/// item itself.
fn render_one(node: &Node, blocks: &mut Vec<String>, ctx: &mut Ctx) {
    match node {
        // Containers and layer wrappers decide for their children.
        Node::Group { .. }
        | Node::Furniture { .. }
        | Node::PageFurniture { .. }
        | Node::FurnitureText { .. }
        | Node::CommentSection { .. }
        | Node::PictureChildren(_)
        | Node::Commented { .. }
        | Node::ScriptText { .. }
        | Node::Located { .. }
        | Node::Prov { .. }
        | Node::Track { .. }
        | Node::PageBreak
        | Node::PageInfo { .. }
        | Node::DoclangOnly(_) => {}
        _ if !ctx.layer_visible() => return,
        _ => {}
    }
    render_item(node, blocks, ctx)
}

fn render_item(node: &Node, blocks: &mut Vec<String>, ctx: &mut Ctx) {
    match node {
        Node::Heading { level, text } => {
            if ctx.plain {
                // `PlainTextTextSerializer`: the heading's text as it is —
                // no `#`, its newlines kept.
                blocks.push(ctx.text(text));
                return;
            }
            let text = heading_line_breaks(&ctx.text(text));
            if ctx.in_table_cell {
                // docling-core#540: no `#` markers inside a table cell.
                blocks.push(text);
            } else {
                let hashes = "#".repeat((*level).clamp(1, 6) as usize);
                blocks.push(format!("{hashes} {text}"));
            }
        }
        // An empty body paragraph (docling's blank-line text item) contributes
        // nothing to Markdown — only DocLang/JSON keep it.
        Node::Paragraph { text } if text.is_empty() => {}
        Node::Paragraph { text } => blocks.push(ctx.breaks(&ctx.text(text))),
        Node::LabeledText { .. } => render_item(&node.labeled_as_paragraph(), blocks, ctx),
        // A standalone caption item renders like a text item; its hyperlink
        // annotation becomes a Markdown link around the whole caption.
        Node::Caption { text, .. } if text.is_empty() => {}
        Node::Caption { text, href } => {
            let body = ctx.breaks(&ctx.text(text));
            blocks.push(match href {
                Some(url) if !ctx.plain => crate::hyperlink::markdown_link(&body, url),
                _ => body,
            });
        }
        Node::CheckboxItem { checked, text } => {
            // docling-core#955: the box needs a space after it, unless the
            // text brings its own (a DocLang `<checkbox/> y` reads `" y"`).
            let mark = if *checked { "- [x]" } else { "- [ ]" };
            let sep = if text.starts_with(' ') { "" } else { " " };
            blocks.push(ctx.breaks(&ctx.text(&format!("{mark}{sep}{text}"))));
        }
        Node::Code {
            language,
            text,
            pretty,
            ..
        } => {
            // Legacy docling never emits a language on the fence; strict keeps
            // it. A `mermaid` block (#858, an extraction docling has no
            // counterpart for) keeps its tag in either mode: without it the
            // graph is a code block no renderer draws.
            let lang = match language {
                Some(l) if ctx.strict || l == "mermaid" => l.as_str(),
                _ => "",
            };
            // Strict prefers the line-preserving rendering when the backend
            // supplied one (PDF); legacy stays on docling's flat `text`.
            let body = match pretty {
                Some(p) if ctx.strict => p.as_str(),
                _ => text.as_str(),
            };
            // The plain export (`format_code_blocks=False`) prints the code
            // as it is.
            blocks.push(if ctx.plain {
                body.to_string()
            } else {
                code_fence(lang, body)
            });
        }
        // A CodeFormula-decoded display formula renders as docling's `$$…$$`
        // (the un-enriched pipeline emits a placeholder paragraph instead).
        Node::Formula { latex, .. } => blocks.push(format!("$${latex}$$")),
        Node::Table(table) => {
            // docling renders a table's caption as a text line before the grid.
            // `caption` is already escaped (backend convention), like a paragraph.
            if let Some(cap) = &table.caption {
                if !cap.is_empty() {
                    blocks.push(ctx.text(cap));
                }
            }
            let rendered = if ctx.plain {
                render_table(&ctx.plain_table(table), ctx.compact_tables)
            } else {
                render_table(table, ctx.compact_tables)
            };
            if !rendered.is_empty() {
                blocks.push(rendered);
            }
            // docling-core#675: the footnotes follow the grid, one paragraph
            // each (`serialize_footnotes` with a `\n\n` delimiter).
            for note in table.footnotes.iter().filter(|n| !n.is_empty()) {
                blocks.push(ctx.text(note));
            }
        }
        // Classification predictions don't affect docling's Markdown output;
        // a description (the picture-OCR text, #645) prints between the
        // caption and the image placeholder — docling's
        // `MarkdownPictureSerializer` order: captions, annotations, image.
        // Alt text (#676) is rendered only under strict Markdown or the
        // document's `picture_alt_text`, and then where Markdown puts it: as
        // the `![alt](…)` of an image line, or — with the placeholder, which
        // has no alt slot — as the line a description takes.
        Node::Picture {
            caption,
            image,
            description,
            ..
        } => {
            if let Some(cap) = caption {
                if !cap.is_empty() {
                    blocks.push(ctx.caption(cap));
                }
            }
            let alt = description
                .as_ref()
                .filter(|d| d.is_alt_text() && ctx.alt_text && !d.text.is_empty());
            let marker = picture_marker(image.as_ref(), ctx, alt.map(|d| d.text.as_str()));
            // A docling-native `meta.description` (docling#5015) is the
            // item's meta, which docling-core's serializer appends *after*
            // the item; the legacy-annotation kinds precede the image.
            let native = description
                .as_ref()
                .filter(|d| d.is_docling_meta() && !d.text.is_empty());
            if let Some(desc) = description.as_ref().filter(|d| !d.is_docling_meta()) {
                let shown = !desc.is_alt_text() || (ctx.alt_text && !marker.starts_with("!["));
                if !desc.text.is_empty() && shown {
                    blocks.push(ctx.breaks(&ctx.text(&desc.text)));
                }
            }
            blocks.push(marker);
            if let Some(desc) = native {
                blocks.push(ctx.breaks(&ctx.text(&desc.text)));
            }
        }
        // A chart renders as docling's picture-with-meta markdown: the caption,
        // the placeholder, the humanized classification ("line_chart" ->
        // "Line chart"), then the chart's data grid as a regular table.
        Node::Chart {
            kind,
            table,
            caption,
            ..
        } => {
            if let Some(cap) = caption {
                if !cap.is_empty() {
                    blocks.push(ctx.caption(cap));
                }
            }
            blocks.push(picture_marker(None, ctx, None));
            blocks.push(humanize_label(kind));
            let rendered = if ctx.plain {
                render_table(&ctx.plain_table(table), false)
            } else {
                render_table(table, false)
            };
            if !rendered.is_empty() {
                blocks.push(rendered);
            }
        }
        // A DocLang-only node is omitted from Markdown.
        Node::DoclangOnly(_) => {}
        // A group on a non-body layer (a hidden spreadsheet sheet): its items
        // sit on that layer, as every item of such a sheet does upstream — so
        // they render exactly when the set includes it (#599), nothing by
        // default.
        Node::Group {
            layer: Some(layer),
            children,
            ..
        } => {
            let outer = ctx.current_layer.replace(*layer);
            render(children, blocks, ctx);
            ctx.current_layer = outer;
        }
        Node::Group { children, .. } => render(children, blocks, ctx),
        Node::FieldRegion { items } => {
            // The region container and each field item carry no text of their
            // own; docling-core 2.93 (#724) serializes them to nothing (older
            // releases emitted a `<!-- missing-text -->` marker for each), so
            // only an item's marker/key/value appear, as separate paragraphs.
            for item in items {
                for part in [&item.marker, &item.key, &item.value].into_iter().flatten() {
                    blocks.push(ctx.breaks(&ctx.text(part)));
                }
            }
        }
        // docling's Markdown serializer has no component for a `KeyValueItem`
        // and writes its fallback placeholder in the item's place.
        Node::KeyValueGraph { .. } => blocks.push(MISSING_KEY_VALUE_ITEM.to_string()),
        // A rich inline group renders exactly like a paragraph of its Markdown
        // text — the structured runs are DocLang-only.
        Node::InlineGroup { md_text, .. } => blocks.push(ctx.breaks(&ctx.text(md_text))),
        // A plain-text backend dump renders verbatim as a single block.
        Node::TextDump(text) => {
            if !text.is_empty() {
                blocks.push(text.clone());
            }
        }
        // Furniture (page headers/footers, HTML `<title>`, notes) is out of
        // the default body-only export, mirroring docling; a set that
        // includes its layer renders the wrapped item like a body item (#599).
        Node::Furniture { layer, inner } => {
            let outer = ctx.current_layer.replace(*layer);
            render_one(inner, blocks, ctx);
            ctx.current_layer = outer;
        }
        // A page header/footer or a `.doc` furniture paragraph: upstream's
        // `page_header` / `page_footer` / `footnote` text item, a plain
        // paragraph when the furniture layer renders.
        Node::PageFurniture { text, .. } | Node::FurnitureText { text, .. } => {
            if ctx.layers.furniture && !text.is_empty() {
                blocks.push(ctx.breaks(&ctx.text(text)));
            }
        }
        // A picture's contained text: docling's Markdown picture serializer
        // prints the caption and the image, never the children — unless the
        // export traverses pictures, when the item walk yields them after
        // the picture and they render as the text items they are (#599).
        Node::PictureChildren(children) => {
            if ctx.traverse_pictures {
                render(children, blocks, ctx);
            }
        }
        // A comment lives in the notes layer — out like other furniture
        // unless that layer renders (the note's `[author: …]: text` as a
        // paragraph); the annotation on a body item is JSON-only, so render
        // the item.
        Node::CommentSection { text, .. } => {
            if ctx.layers.notes && !text.is_empty() {
                blocks.push(ctx.breaks(&ctx.text(text)));
            }
        }
        Node::Commented { inner, .. } => render_one(inner, blocks, ctx),
        // The item's text is already the runs' text (docling's Markdown of
        // its inline group: the runs joined, no markup — `H2O`).
        Node::ScriptText { inner, .. } => render_one(inner, blocks, ctx),
        // Layout provenance is DocLang-only; render the wrapped node.
        Node::Located { inner, .. } | Node::Prov { inner, .. } | Node::Track { inner, .. } => {
            render_one(inner, blocks, ctx)
        }
        // Page breaks are DocLang-only; docling omits them from Markdown.
        Node::PageBreak => {}
        // Page markers feed the JSON export only.
        Node::PageInfo { .. } => {}
        // A link destination (#852) feeds the JSON export only.
        Node::Anchor { .. } => {}
        // Runs of adjacent list items are merged by `render`; a stray single
        // item (a hand-built document, or a `Located` wrapper around one)
        // still renders as its own one-item list instead of panicking —
        // `nodes` is public API, so every representable tree must serialize.
        Node::ListItem { .. } => render_list_run(std::slice::from_ref(node), blocks, ctx),
        // Outside a list run (a hand-built document): a paragraph.
        Node::ListText { text, .. } if text.is_empty() => {}
        Node::ListText { text, .. } => blocks.push(ctx.breaks(&ctx.text(text))),
    }
}

/// The Markdown for a picture under the active [`ImageMode`]; Referenced mode also
/// records the bytes in `ctx.artifacts` for the caller to write.
/// docling-core's `_humanize_text`: underscores to spaces, first letter
/// capitalized ("line_chart" -> "Line chart").
fn humanize_label(label: &str) -> String {
    let text = label.replace('_', " ");
    let mut chars = text.chars();
    match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
        None => text,
    }
}

/// The image line's alt: docling's `Image`, or the picture's alt text (#676)
/// on one line with its `]` escaped, so it cannot close the bracket.
fn image_alt(alt: Option<&str>) -> String {
    match alt {
        Some(text) => text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .replace('\\', "\\\\")
            .replace(']', "\\]"),
        None => "Image".to_string(),
    }
}

fn picture_marker(image: Option<&crate::PictureImage>, ctx: &mut Ctx, alt: Option<&str>) -> String {
    match (ctx.images, image) {
        // docling embeds the `ImageRef`'s PNG (a JPEG re-encoded).
        (ImageMode::Embedded, Some(img)) => {
            format!(
                "![{}]({})",
                image_alt(alt),
                crate::pixel_digest::docling_data_uri(img).1
            )
        }
        (ImageMode::Referenced, Some(img)) => {
            let path = format!(
                "{}/image_{:06}.{}",
                ctx.artifacts_dir,
                ctx.pic_index,
                ext_for(&img.mimetype)
            );
            ctx.pic_index += 1;
            ctx.artifacts.push((path.clone(), img.data.clone()));
            format!("![{}]({})", image_alt(alt), escape_uri_path(&path))
        }
        // Placeholder, or any mode with no extracted image.
        _ => ctx.image_placeholder.clone(),
    }
}

/// Encode a URL or filesystem path as a Markdown link destination —
/// docling-core's `MarkdownPictureSerializer._escape_uri_path`
/// (docling-core#698, 2.94). Handles URLs of any scheme as well as POSIX and
/// Windows paths, keeps relative paths relative and never double-encodes:
/// backslashes become `/` (a backslash is both the Windows separator and a
/// Markdown escape), a UNC share `//host/…` and an absolute Windows path
/// `C:/…` become RFC 8089 `file://` URLs (the one spelling a renderer cannot
/// misread as a scheme-relative URL or a `C:` scheme), a URL keeps its
/// scheme / authority / delimiters with only the components encoded, and
/// everything else is percent-encoded as a path. `%` is kept so an
/// already-encoded destination stays as it is; spaces and parentheses are
/// encoded because they would end (or unbalance) a Markdown inline link.
pub(crate) fn escape_uri_path(value: &str) -> String {
    const KEEP: &str = "/%:@+,;=~$!&'*";
    let s = value.replace('\\', "/");
    if let Some(rest) = s.strip_prefix("//") {
        // A fileshare: `file://<host>/<path>`, the host possibly empty.
        let rest = rest.trim_start_matches('/');
        let (host, tail) = rest.split_once('/').unwrap_or((rest, ""));
        return format!("file://{host}{}", percent_quote(&format!("/{tail}"), KEEP));
    }
    let bytes = s.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/' {
        // A Windows path with a drive letter: `file:///C:/…`.
        return format!("file:///{}", percent_quote(&s, KEEP));
    }
    // A URL keeps its scheme, authority and delimiters; only its components are
    // encoded. A single-character scheme cannot be real (it is a drive letter,
    // handled above), so it is read as a path — like `urlsplit`.
    if let Some((scheme, rest)) = s.split_once(':') {
        let valid_scheme = scheme.len() > 1
            && scheme.as_bytes()[0].is_ascii_alphabetic()
            && scheme
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'));
        if valid_scheme {
            let (authority, rest) = match rest.strip_prefix("//") {
                Some(r) => {
                    let end = r.find(['/', '?', '#']).unwrap_or(r.len());
                    (Some(&r[..end]), &r[end..])
                }
                None => (None, rest),
            };
            let (before_frag, fragment) = rest.split_once('#').unwrap_or((rest, ""));
            let (path, query) = before_frag.split_once('?').unwrap_or((before_frag, ""));
            let mut out = format!("{scheme}:");
            if let Some(a) = authority {
                out.push_str("//");
                out.push_str(a);
            }
            out.push_str(&percent_quote(path, KEEP));
            if !query.is_empty() {
                out.push('?');
                out.push_str(&percent_quote(query, KEEP));
            }
            if !fragment.is_empty() {
                out.push('#');
                out.push_str(&percent_quote(fragment, KEEP));
            }
            return out;
        }
    }
    // A relative or root-relative local path.
    percent_quote(&s, KEEP)
}

/// `urllib.parse.quote(s, safe)`: unreserved ASCII (`A–Z a–z 0–9 _ . - ~`) and
/// the `safe` set stay, every other byte of the UTF-8 encoding becomes `%XX`.
fn percent_quote(s: &str, safe: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(b, b'_' | b'.' | b'-' | b'~')
            || (b.is_ascii() && safe.contains(b as char));
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub(crate) fn ext_for(mimetype: &str) -> &str {
    match mimetype {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/tiff" => "tif",
        "image/svg+xml" => "svg",
        _ => "png",
    }
}

/// Render a table. `compact` selects between two serializers:
///
/// - **padded** (default) — docling-core's `tabulate(tablefmt="github")`: columns
///   are padded to a fixed width (header width + a minimum padding of 2, or the
///   widest data cell); numeric columns (every data cell parses as a number) are
///   right-aligned, others left-aligned; separators are plain dashes of
///   `width + 2`. Matches current published docling (DOCX/HTML conformance).
/// - **compact** — `| a | b |` cells with single-dash `| - | - |` separators, no
///   width padding. Matches the committed PDF groundtruth corpus, which predates
///   the padded serializer.
///
/// Each cell is first escaped (`\n` → space, `|` → `&#124;`) so it can't break the
/// table. The header row is the table's leading `column_header` block flattened
/// to one row ([`Table::header_row_count`] + [`flatten_header_rows`],
/// docling-core#723); alignment and widths are computed over the body rows.
/// Whether a table cell counts as a number for column alignment, matching
/// `tabulate`'s detection: an ordinary float/int (`f64`-parseable, covering
/// `1e2`/`inf`/`+1.5`) **or** a thousands-separated number like `7,015`.
fn is_number_cell(t: &str) -> bool {
    t.parse::<f64>().is_ok() || is_thousands_number(t)
}

/// A number with comma thousands-separators, per `tabulate`'s
/// `_float_with_thousands_separators` regex
/// (`^(([+-]?[0-9]{1,3})(?:,([0-9]{3}))*)?(?(1)\.[0-9]*|\.[0-9]+)?$`): the
/// integer part is 1–3 digits then any number of `,ddd` groups; the fraction is
/// optional (and, without an integer part, must have at least one digit).
fn is_thousands_number(t: &str) -> bool {
    let b = t.as_bytes();
    let mut i = 0;
    let start = i;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    // First digit chunk: 1–3 digits.
    let d0 = i;
    while i < b.len() && b[i].is_ascii_digit() && i - d0 < 3 {
        i += 1;
    }
    let has_int = i > d0;
    if has_int {
        // Subsequent `,ddd` groups (exactly three digits each).
        while i + 3 < b.len() + 1
            && b.get(i) == Some(&b',')
            && b.get(i + 1).is_some_and(u8::is_ascii_digit)
            && b.get(i + 2).is_some_and(u8::is_ascii_digit)
            && b.get(i + 3).is_some_and(u8::is_ascii_digit)
        {
            i += 4;
        }
    } else {
        // A sign only counts with an integer part.
        i = start;
    }
    // Optional fraction.
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let f0 = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if !has_int && i == f0 {
            return false; // `.` with no digits and no integer part
        }
    } else if !has_int {
        return false; // neither integer nor fractional part
    }
    i == b.len()
}

/// The single GFM header row for a table: the leading header rows (see
/// [`Table::header_row_count`]) flattened per column, texts joined with
/// `" - "` after dropping consecutive duplicates — docling-core's
/// `_flatten_header_rows` (docling-core#723). The duplicate rule is what
/// keeps a row-spanning header from being joined to itself (the grid repeats
/// its text into every row it covers); it is position-based, so two stacked
/// levels sharing a label collapse too — GFM has one header row, and upstream
/// accepts that loss. No header rows → one empty header cell per column.
fn flatten_header_rows(header_rows: &[Vec<String>], num_cols: usize) -> Vec<String> {
    (0..num_cols)
        .map(|c| {
            let mut parts: Vec<&str> = Vec::new();
            for row in header_rows {
                let text = row.get(c).map(String::as_str).unwrap_or("");
                if !text.is_empty() && parts.last() != Some(&text) {
                    parts.push(text);
                }
            }
            parts.join(" - ")
        })
        .collect()
}

pub(crate) fn render_table(table: &Table, compact: bool) -> String {
    if table.rows.is_empty() {
        return String::new();
    }
    let num_cols = table.rows.iter().map(Vec::len).max().unwrap_or(0);
    if num_cols == 0 {
        return String::new();
    }

    // Escaped, rectangular grid (ragged rows padded with empty cells). The
    // header block is resolved to the one row GFM allows (docling-core#723);
    // `tabulate` strips data cells of surrounding whitespace but leaves the
    // header texts as-is.
    let num_headers = table.header_row_count().min(table.rows.len());
    let escaped = |r: usize| -> Vec<String> {
        (0..num_cols)
            .map(|c| escape_cell(table.rows[r].get(c).map(String::as_str).unwrap_or("")))
            .collect()
    };
    let header_rows: Vec<Vec<String>> = (0..num_headers).map(escaped).collect();
    let header = flatten_header_rows(&header_rows, num_cols);
    let body: Vec<Vec<String>> = (num_headers..table.rows.len())
        .map(|r| {
            escaped(r)
                .into_iter()
                .map(|c| c.trim().to_string())
                .collect()
        })
        .collect();

    if compact {
        // Compact: cells joined by " | ", no padding, single-dash separators.
        let render_row = |row: &[String]| -> String { format!("| {} |", row.join(" | ")) };
        let mut lines = Vec::with_capacity(body.len() + 2);
        lines.push(render_row(&header));
        let sep: Vec<&str> = (0..num_cols).map(|_| "-").collect();
        lines.push(format!("| {} |", sep.join(" | ")));
        for row in &body {
            lines.push(render_row(row));
        }
        return lines.join("\n");
    }

    // Display width (Unicode scalar count — good enough for now).
    let dw = |s: &str| s.chars().count();

    // A column is right-aligned when at least one body cell is numeric and every
    // non-empty body cell is numeric — matching `tabulate`'s column typing, where
    // empty cells are "missing" (ignored) and a number may carry thousands
    // separators (`7,015`), which a plain `f64` parse rejects.
    let right: Vec<bool> = (0..num_cols)
        .map(|c| {
            let mut any = false;
            for row in &body {
                let t = row[c].trim();
                if t.is_empty() {
                    continue;
                }
                if !is_number_cell(t) {
                    return false;
                }
                any = true;
            }
            any
        })
        .collect();

    // Column width = max(header_width + MIN_PADDING(2), max body-cell width).
    let width: Vec<usize> = (0..num_cols)
        .map(|c| {
            let mut w = dw(&header[c]) + 2;
            for row in &body {
                w = w.max(dw(&row[c]));
            }
            w
        })
        .collect();

    let fmt_cell = |s: &str, c: usize| -> String {
        let pad = " ".repeat(width[c].saturating_sub(dw(s)));
        let body = if right[c] {
            format!("{pad}{s}")
        } else {
            format!("{s}{pad}")
        };
        format!(" {body} ")
    };
    let render_row = |row: &[String]| -> String {
        let cells: Vec<String> = (0..num_cols).map(|c| fmt_cell(&row[c], c)).collect();
        format!("|{}|", cells.join("|"))
    };

    let mut lines = Vec::with_capacity(body.len() + 2);
    lines.push(render_row(&header));
    let sep: Vec<String> = (0..num_cols).map(|c| "-".repeat(width[c] + 2)).collect();
    lines.push(format!("|{}|", sep.join("|")));
    for row in &body {
        lines.push(render_row(row));
    }
    lines.join("\n")
}

/// Escape a table cell so it can't break the markdown table: newlines become
/// spaces and pipes become the `&#124;` HTML entity (matches docling-core).
fn escape_cell(s: &str) -> String {
    s.replace('\n', " ").replace('|', "&#124;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PictureImage, TableCell, TableStructure};

    /// #385: where one list ends and the next begins is the backend's call
    /// (`first_in_list`), never the serializer's. An ordered run `1.` → `5.`
    /// is one list (an AsciiDoc numbered list around a nested one), and so are
    /// mixed bullet/ordered items the backend did not separate; only a flagged
    /// item opens a new list and earns the blank line.
    #[test]
    fn list_boundaries_come_from_the_backend_not_the_numbering() {
        let item = |ordered: bool, number: u64, first_in_list: bool, text: &str| Node::ListItem {
            ordered,
            number,
            first_in_list,
            text: text.into(),
            level: 0,
            marker: None,
            location: None,
            split: None,
            dclx: None,
            href: None,
            layer: None,
        };
        let md = |items: Vec<Node>| {
            let mut doc = DoclingDocument::new("t");
            for n in items {
                doc.push(n);
            }
            doc.export_to_markdown()
        };
        // A number gap alone is not a boundary.
        assert_eq!(
            md(vec![
                item(true, 1, true, "one"),
                item(true, 5, false, "five")
            ]),
            "1. one\n5. five\n"
        );
        // Nor is a kind flip the backend did not flag …
        assert_eq!(
            md(vec![
                item(false, 0, true, "bullet"),
                item(true, 1, false, "one"),
                item(false, 0, false, "bullet two"),
            ]),
            "- bullet\n1. one\n- bullet two\n"
        );
        // … while a flagged item is one, whatever its number says.
        assert_eq!(
            md(vec![
                item(true, 1, true, "a"),
                item(true, 2, false, "b"),
                item(true, 3, true, "new list, continuing count"),
            ]),
            "1. a\n2. b\n\n3. new list, continuing count\n"
        );
    }

    #[test]
    fn renders_headings_paragraphs_and_lists() {
        let mut doc = DoclingDocument::new("demo");
        doc.add_heading(1, "Title");
        doc.add_paragraph("Hello world.");
        doc.push(Node::ListItem {
            ordered: false,
            number: 1,
            first_in_list: true,
            text: "first".into(),
            level: 0,
            marker: None,
            location: None,
            split: None,
            dclx: None,
            href: None,
            layer: None,
        });
        doc.push(Node::ListItem {
            ordered: false,
            number: 2,
            first_in_list: false,
            text: "second".into(),
            level: 0,
            marker: None,
            location: None,
            split: None,
            dclx: None,
            href: None,
            layer: None,
        });
        let md = doc.export_to_markdown();
        assert_eq!(md, "# Title\n\nHello world.\n\n- first\n- second\n");
    }

    /// docling-core 2.92 (#721): a single newline inside an item's text is a
    /// GFM hard line break, a blank line stays a paragraph break, and a heading
    /// collapses its newline to a space. Nested-table dumps stay verbatim.
    #[test]
    fn single_newlines_become_gfm_hard_line_breaks() {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::Heading {
            level: 1,
            text: "Hello\nWorld".into(),
        });
        doc.push(Node::Paragraph {
            text: "line one\nline two\n\npara two".into(),
        });
        doc.push(Node::ListItem {
            ordered: false,
            number: 1,
            first_in_list: true,
            text: "item\ncontinued".into(),
            level: 0,
            marker: None,
            location: None,
            split: None,
            dclx: None,
            href: None,
            layer: None,
        });
        doc.push(Node::TextDump("A1 B1 \n\n\nC1".into()));
        assert_eq!(
            doc.export_to_markdown(),
            "# Hello World\n\nline one  \nline two\n\npara two\n\n- item  \ncontinued\n\nA1 B1 \n\n\nC1\n"
        );
    }

    /// docling-core#540: inside a rich table cell a heading is plain text;
    /// docling-core#724: a field region renders only its items' key/value text.
    #[test]
    fn table_cell_mode_and_field_regions() {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::Heading {
            level: 2,
            text: "A  text".into(),
        });
        doc.push(Node::Paragraph {
            text: "body".into(),
        });
        assert_eq!(to_markdown_table_cell(&doc, false), "A  text\n\nbody");
        assert_eq!(doc.export_to_markdown(), "## A  text\n\nbody\n");

        let mut doc = DoclingDocument::new("f");
        doc.push(Node::FieldRegion {
            items: vec![crate::FieldItem {
                marker: None,
                key: Some("Name:".into()),
                value: Some("John Doe".into()),
                value_kind: None,
            }],
        });
        assert_eq!(doc.export_to_markdown(), "Name:\n\nJohn Doe\n");
    }

    #[test]
    fn strict_renders_recovered_links_legacy_does_not() {
        let mut doc = DoclingDocument::new("cv");
        doc.add_paragraph("Find me on LinkedIn or GitHub.");
        doc.links = vec![
            ("LinkedIn".into(), "https://www.linkedin.com/in/x/".into()),
            ("GitHub".into(), "https://github.com/x/".into()),
        ];
        // Legacy/docling mode: links are left untouched (conformance preserved).
        assert_eq!(doc.export_to_markdown(), "Find me on LinkedIn or GitHub.\n");
        // Strict mode: anchors become Markdown links.
        assert_eq!(
            doc.export_to_markdown_with(true),
            "Find me on [LinkedIn](https://www.linkedin.com/in/x/) or [GitHub](https://github.com/x/).\n"
        );
    }

    #[test]
    fn strict_links_match_escaped_anchor_and_consume_in_order() {
        let mut doc = DoclingDocument::new("d");
        // The PDF assembler HTML-escapes prose, so by serialization time the body
        // already carries `&amp;`; the anchor is stored un-escaped. The matcher must
        // escape the anchor to find it. Two identical anchors link in document order.
        doc.add_paragraph("AI &amp; ML here, and issues here, then issues there.");
        doc.links = vec![
            ("AI & ML".into(), "https://a/".into()),
            ("issues".into(), "https://first/".into()),
            ("issues".into(), "https://second/".into()),
        ];
        assert_eq!(
            doc.export_to_markdown_with(true),
            "[AI &amp; ML](https://a/) here, and [issues](https://first/) here, then [issues](https://second/) there.\n"
        );
    }

    /// docling-core#698: the referenced-image destination is percent-encoded —
    /// upstream's own case table (paths, Windows flavours, UNC, URLs) plus
    /// idempotency on the encoded result.
    #[test]
    fn referenced_image_destinations_are_escaped() {
        let cases = [
            (
                "doc_artifacts/image_000001_ab12.png",
                "doc_artifacts/image_000001_ab12.png",
            ),
            (
                "My Report_artifacts/img.png",
                "My%20Report_artifacts/img.png",
            ),
            ("artifacts/img (1).png", "artifacts/img%20%281%29.png"),
            ("100%_scale/a#b?c.png", "100%_scale/a%23b%3Fc.png"),
            ("/home/a b/img.png", "/home/a%20b/img.png"),
            (
                "My Report_artifacts\\img.png",
                "My%20Report_artifacts/img.png",
            ),
            (
                "C:/Users/me/My Docs/img.png",
                "file:///C:/Users/me/My%20Docs/img.png",
            ),
            ("C:\\Users\\me\\img.png", "file:///C:/Users/me/img.png"),
            (
                "//server/share/My Docs/img.png",
                "file://server/share/My%20Docs/img.png",
            ),
            ("\\\\server\\share\\img.png", "file://server/share/img.png"),
            ("file:///home/a b/img.png", "file:///home/a%20b/img.png"),
            (
                "s3://bucket/My Report_artifacts/img.png",
                "s3://bucket/My%20Report_artifacts/img.png",
            ),
            (
                "https://example.com:8080/a b.png?w=1&h=2#frag",
                "https://example.com:8080/a%20b.png?w=1&h=2#frag",
            ),
            (
                "https://example.com/img (1).png",
                "https://example.com/img%20%281%29.png",
            ),
            ("caf\u{e9}/im\u{e4}ge.png", "caf%C3%A9/im%C3%A4ge.png"),
        ];
        for (input, expected) in cases {
            assert_eq!(escape_uri_path(input), expected, "input {input:?}");
            assert_eq!(
                escape_uri_path(expected),
                expected,
                "idempotent {expected:?}"
            );
        }
        // The whole marker, through the referenced-image export.
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::Picture {
            caption: None,
            caption_href: None,
            image: Some(PictureImage {
                dpi: PictureImage::DEFAULT_DPI,
                mimetype: "image/png".into(),
                width: 1,
                height: 1,
                data: b"x".to_vec(),
                display_size: None,
            }),
            classification: None,
            description: None,
            caption_parent: Default::default(),
            caption_location: None,
        });
        let (md, files) = doc
            .export_to_markdown_with_images(ImageMode::Referenced, "My Report (final)_artifacts");
        assert!(
            md.contains("![Image](My%20Report%20%28final%29_artifacts/image_000000.png)"),
            "got:\n{md}"
        );
        // The file path handed back for writing stays unescaped.
        assert_eq!(files[0].0, "My Report (final)_artifacts/image_000000.png");
    }

    /// Pictures the HTML backend folds into a list item print after the item
    /// line with plain newlines; a `<br>` newline in the item's own text is
    /// still a GFM hard line break.
    #[test]
    fn folded_list_item_pictures_keep_plain_newlines() {
        let doc = DoclingDocument::new("t");
        let ctx = Ctx::new(&doc, false, &MarkdownExportOptions::default());
        assert_eq!(
            list_item_text("Step\n<!-- image -->", &ctx),
            "Step\n<!-- image -->"
        );
        assert_eq!(
            list_item_text("Step\nAlt text\n<!-- image -->\n<!-- image -->", &ctx),
            "Step\nAlt text\n<!-- image -->\n<!-- image -->"
        );
        assert_eq!(
            list_item_text("line one\nline two", &ctx),
            "line one  \nline two"
        );
    }

    /// docling-core#723: the header block is the leading run of rows on which a
    /// `column_header` cell starts, flattened per column with " - ".
    #[test]
    fn stacked_header_rows_flatten_into_one() {
        let mut t = Table {
            rows: vec![
                vec!["".into(), "% of Total".into(), "% of Total".into()],
                vec!["class".into(), "Train".into(), "Test".into()],
                vec!["Caption".into(), "2.04".into(), "1.77".into()],
            ],
            ..Default::default()
        };
        t.structure = Some(TableStructure {
            header_row: vec![true, true, false],
            col_continuation: vec![
                vec![false, false, true],
                vec![false, false, false],
                vec![false, false, false],
            ],
            ..Default::default()
        });
        assert_eq!(t.header_row_count(), 2);
        assert_eq!(
            render_table(&t, true),
            "| class | % of Total - Train | % of Total - Test |\n| - | - | - |\n| Caption | 2.04 | 1.77 |"
        );
        // padded: widths from the flattened header, alignment from body rows
        assert_eq!(
            render_table(&t, false),
            "| class   |   % of Total - Train |   % of Total - Test |\n\
             |---------|----------------------|---------------------|\n\
             | Caption |                 2.04 |                1.77 |"
        );
    }

    /// A header spanning two rows is repeated into the second row by the grid;
    /// that row is not a header row unless another header cell starts there.
    #[test]
    fn vertically_spanning_header_does_not_extend_the_block() {
        let mut t = Table {
            rows: vec![
                vec!["Name".into(), "Value".into()],
                vec!["Name".into(), "1".into()],
                vec!["x".into(), "2".into()],
            ],
            ..Default::default()
        };
        t.structure = Some(TableStructure {
            col_header: vec![vec![true, true], vec![true, false], vec![false, false]],
            row_continuation: vec![vec![false, false], vec![true, false], vec![false, false]],
            ..Default::default()
        });
        assert_eq!(t.header_row_count(), 1);
        assert_eq!(
            render_table(&t, true),
            "| Name | Value |\n| - | - |\n| Name | 1 |\n| x | 2 |"
        );
    }

    /// Flags that begin on a later row promote nothing: every row stays in the
    /// body under an empty header row (tabulate's `headers=["", ""]`).
    #[test]
    fn header_flags_not_on_row_zero_keep_all_rows_in_the_body() {
        let mut t = Table {
            rows: vec![
                vec!["1".into(), "2".into()],
                vec!["a".into(), "b".into()],
                vec!["333".into(), "4".into()],
            ],
            ..Default::default()
        };
        t.structure = Some(TableStructure {
            header_row: vec![false, true, false],
            ..Default::default()
        });
        assert_eq!(t.header_row_count(), 0);
        assert_eq!(
            render_table(&t, false),
            "|     |    |\n|-----|----|\n| 1   | 2  |\n| a   | b  |\n| 333 | 4  |"
        );
    }

    /// A pivot table's row headers (`<th rowspan>`) carry `row_header`, not
    /// `column_header` (docling#4216), so the data row beside them is not
    /// pulled into the header block — what this port used to reach with a
    /// deviation now falls out of the flags themselves.
    #[test]
    fn pivot_row_headers_do_not_extend_the_header() {
        let mut t = Table {
            rows: vec![
                vec!["Year".into(), "Month".into()],
                vec!["2025".into(), "January".into()],
                vec!["2025".into(), "February".into()],
            ],
            ..Default::default()
        };
        t.structure = Some(TableStructure {
            col_header: vec![vec![true, true], vec![false, false], vec![false, false]],
            row_header: vec![vec![false, false], vec![true, false], vec![true, false]],
            row_continuation: vec![vec![false, false], vec![false, false], vec![true, false]],
            ..Default::default()
        });
        assert_eq!(t.header_row_count(), 1);
        assert_eq!(
            render_table(&t, true),
            "| Year | Month |\n| - | - |\n| 2025 | January |\n| 2025 | February |"
        );
    }

    /// No `column_header` anywhere (first-class cells without flags) → row 0
    /// stays the header, as before.
    #[test]
    fn unflagged_cells_keep_row_zero_as_header() {
        let mut t = Table {
            rows: vec![vec!["h".into()], vec!["d".into()]],
            ..Default::default()
        };
        t.cells = Some(
            [(0usize, "h"), (1, "d")]
                .into_iter()
                .map(|(r, text)| TableCell {
                    text: text.into(),
                    bbox: None,
                    start_row: r,
                    start_col: 0,
                    row_span: 1,
                    col_span: 1,
                    column_header: false,
                    row_header: false,
                    row_section: false,
                })
                .collect(),
        );
        assert_eq!(t.header_row_count(), 1);
        assert_eq!(render_table(&t, true), "| h |\n| - |\n| d |");
    }

    /// A table with first-class 1×1 cells, `column_header` where `ched` says.
    fn flagged_table(rows: &[&[&str]], ched: impl Fn(usize, usize) -> bool) -> Table {
        let mut cells = Vec::new();
        for (r, row) in rows.iter().enumerate() {
            for (c, text) in row.iter().enumerate() {
                cells.push(TableCell {
                    text: (*text).into(),
                    bbox: None,
                    start_row: r,
                    start_col: c,
                    row_span: 1,
                    col_span: 1,
                    column_header: ched(r, c),
                    row_header: false,
                    row_section: false,
                });
            }
        }
        Table {
            rows: rows
                .iter()
                .map(|row| row.iter().map(|t| (*t).to_string()).collect())
                .collect(),
            cells: Some(cells),
            ..Default::default()
        }
    }

    /// #604 (docling-core#766, 2.97): a row whose first-column label is a
    /// `column_header` but whose other cells carry body text stops the header
    /// block. Expected outputs are docling-core 2.99's, byte for byte.
    #[test]
    fn rows_with_body_text_stop_the_header_block() {
        // The issue's form: labels flagged on every row, a real header on top.
        let t = flagged_table(
            &[
                &["Item", "Q1", "Q2"],
                &["Sales", "10", "20"],
                &["Costs", "5", "7"],
            ],
            |r, c| r == 0 || c == 0,
        );
        assert_eq!(t.header_row_count(), 1);
        assert_eq!(
            render_table(&t, false),
            "| Item   |   Q1 |   Q2 |\n\
             |--------|------|------|\n\
             | Sales  |   10 |   20 |\n\
             | Costs  |    5 |    7 |"
        );
        // Labels on every row and no header row: row 0 stops, later rows are
        // flagged → nothing promotable, the body keeps every row.
        let t = flagged_table(&[&["Sales", "10", "20"], &["Costs", "5", "7"]], |_, c| {
            c == 0
        });
        assert_eq!(t.header_row_count(), 0);
        assert_eq!(
            render_table(&t, false),
            "|       |    |    |\n\
             |-------|----|----|\n\
             | Sales | 10 | 20 |\n\
             | Costs |  5 |  7 |"
        );
        let t = flagged_table(
            &[&["a", "1", "2"], &["b", "3", "4"], &["c", "5", "6"]],
            |r, c| c == 0 && r < 2,
        );
        assert_eq!(t.header_row_count(), 0);
        // Row 0 stops on its body text, but no later row carries a flag:
        // row 0 is the header anyway, as for an unflagged table.
        let t = flagged_table(&[&["a", "1", "2"], &["b", "3", "4"]], |r, c| {
            r == 0 && c == 0
        });
        assert_eq!(t.header_row_count(), 1);
        assert_eq!(
            render_table(&t, false),
            "| a   |   1 |   2 |\n\
             |-----|-----|-----|\n\
             | b   |   3 |   4 |"
        );
        // Blank cells beside a flagged label are not body text.
        let t = flagged_table(&[&["Item", "", ""], &["x", "1", "2"]], |_, c| c == 0);
        assert_eq!(t.header_row_count(), 1);
        assert_eq!(
            render_table(&t, false),
            "| Item   |    |    |\n\
             |--------|----|----|\n\
             | x      |  1 |  2 |"
        );
    }

    #[test]
    fn renders_compact_table() {
        let mut doc = DoclingDocument::new("t");
        // The compact form is opt-in (the PDF backend sets it); default output uses
        // the padded GitHub serializer (covered by the regression fixtures).
        doc.compact_tables = true;
        doc.push(Node::Table(Table {
            footnotes: Vec::new(),
            rows: vec![vec!["a".into(), "b".into()], vec!["1".into(), "2".into()]],
            location: None,
            structure: None,
            cell_blocks: None,
            cells: None,
            caption: None,
            caption_parent: Default::default(),
            caption_location: None,
            document_index: false,
        }));
        let md = doc.export_to_markdown();
        assert_eq!(md, "| a | b |\n| - | - |\n| 1 | 2 |\n");
    }

    #[test]
    fn renders_padded_github_table_by_default() {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::Table(Table {
            footnotes: Vec::new(),
            rows: vec![vec!["a".into(), "b".into()], vec!["1".into(), "2".into()]],
            location: None,
            structure: None,
            cell_blocks: None,
            cells: None,
            caption: None,
            caption_parent: Default::default(),
            caption_location: None,
            document_index: false,
        }));
        let md = doc.export_to_markdown();
        // Numeric data columns are right-aligned; columns padded to header+2.
        assert_eq!(md, "|   a |   b |\n|-----|-----|\n|   1 |   2 |\n");
    }

    #[test]
    fn strict_unescapes_inline_underscores_legacy_keeps_them() {
        let mut doc = DoclingDocument::new("t");
        doc.add_heading(1, "a\\_b");
        doc.add_paragraph("x\\_y");
        doc.push(Node::ListItem {
            ordered: false,
            number: 1,
            first_in_list: true,
            text: "i\\_j".into(),
            level: 0,
            marker: None,
            location: None,
            split: None,
            dclx: None,
            href: None,
            layer: None,
        });
        // Legacy reproduces docling's `\_` escaping byte-for-byte.
        assert_eq!(doc.export_to_markdown(), "# a\\_b\n\nx\\_y\n\n- i\\_j\n");
        // Strict prefers literal underscores (Rust-only readability mode).
        assert_eq!(doc.export_to_markdown_with(true), "# a_b\n\nx_y\n\n- i_j\n");
    }

    /// Drive a document's nodes through [`MarkdownStreamer`] in the given page
    /// splits and assert the concatenated chunks equal the buffered serializer.
    fn assert_stream_matches(
        doc: &DoclingDocument,
        strict: bool,
        images: ImageMode,
        splits: &[usize],
    ) {
        let (want, want_artifacts) = to_markdown_images(doc, strict, images, "artifacts");
        let mut streamer =
            MarkdownStreamer::with_artifacts(strict, images, doc.compact_tables, "artifacts")
                .with_page_break_placeholder(doc.page_break_placeholder.clone());
        let mut got = String::new();
        let mut got_artifacts = Vec::new();
        let mut start = 0;
        for &end in splits {
            // Links only matter in strict mode; feed them all with the first batch
            // that has content (document order is preserved by the queue).
            let links = if start == 0 {
                doc.links.as_slice()
            } else {
                &[]
            };
            got.push_str(&streamer.push(&doc.nodes[start..end], links));
            // Referenced mode: drain per push, as a real caller writing files
            // page by page would — numbering must continue across drains.
            got_artifacts.extend(streamer.take_artifacts());
            start = end;
        }
        got.push_str(&streamer.push(
            &doc.nodes[start..],
            if start == 0 {
                doc.links.as_slice()
            } else {
                &[]
            },
        ));
        got_artifacts.extend(streamer.take_artifacts());
        got.push_str(&streamer.finish());
        assert_eq!(
            got, want,
            "streamed output diverged (splits={splits:?}, strict={strict})"
        );
        assert_eq!(
            got_artifacts, want_artifacts,
            "streamed artifacts diverged (splits={splits:?}, strict={strict})"
        );
    }

    #[test]
    fn streaming_is_byte_identical_to_buffered() {
        let mut doc = DoclingDocument::new("d");
        doc.add_heading(1, "Title");
        doc.add_paragraph("First paragraph.");
        doc.push(Node::ListItem {
            ordered: false,
            number: 1,
            first_in_list: true,
            text: "a".into(),
            level: 0,
            marker: None,
            location: None,
            split: None,
            dclx: None,
            href: None,
            layer: None,
        });
        doc.push(Node::ListItem {
            ordered: false,
            number: 2,
            first_in_list: false,
            text: "b".into(),
            level: 0,
            marker: None,
            location: None,
            split: None,
            dclx: None,
            href: None,
            layer: None,
        });
        doc.push(Node::Code {
            language: Some("rust".into()),
            text: "let x = 1;".into(),
            orig: None,
            pretty: None,
        });
        doc.push(Node::Table(Table {
            footnotes: Vec::new(),
            rows: vec![vec!["a".into(), "b".into()], vec!["1".into(), "2".into()]],
            location: None,
            structure: None,
            cell_blocks: None,
            cells: None,
            caption: None,
            caption_parent: Default::default(),
            caption_location: None,
            document_index: false,
        }));
        doc.push(Node::Picture {
            caption: Some("Fig 1".into()),
            caption_href: None,
            image: Some(PictureImage {
                dpi: PictureImage::DEFAULT_DPI,
                mimetype: "image/png".into(),
                width: 2,
                height: 2,
                data: b"png-one".to_vec(),
                display_size: None,
            }),
            classification: None,
            description: None,
            caption_parent: Default::default(),
            caption_location: None,
        });
        doc.add_paragraph("Last paragraph.");
        // A second embedded picture, so referenced mode must keep numbering
        // (`image_000001`) across chunk boundaries.
        doc.push(Node::Picture {
            caption: None,
            caption_href: None,
            image: Some(PictureImage {
                dpi: PictureImage::DEFAULT_DPI,
                mimetype: "image/png".into(),
                width: 2,
                height: 2,
                data: b"png-two".to_vec(),
                display_size: None,
            }),
            classification: None,
            description: None,
            caption_parent: Default::default(),
            caption_location: None,
        });

        // A run of list items must never straddle a split, so try splits that fall
        // on safe block boundaries (the streaming PDF assembler guarantees this).
        for &strict in &[false, true] {
            for &images in &[
                ImageMode::Placeholder,
                ImageMode::Embedded,
                ImageMode::Referenced,
            ] {
                for splits in [&[][..], &[1][..], &[2][..], &[4][..], &[1, 4, 6, 7][..]] {
                    assert_stream_matches(&doc, strict, images, splits);
                }
            }
        }
    }

    #[test]
    fn streaming_applies_recovered_links_in_strict_mode() {
        let mut doc = DoclingDocument::new("d");
        doc.add_paragraph("See LinkedIn for details.");
        doc.add_paragraph("And GitHub too.");
        doc.links = vec![
            ("LinkedIn".into(), "https://lnkd/".into()),
            ("GitHub".into(), "https://gh/".into()),
        ];
        // The second anchor lives in the second block, so it must be carried across
        // the page boundary and placed when that block streams out.
        assert_stream_matches(&doc, true, ImageMode::Placeholder, &[1]);
    }

    /// A three-page document with one empty page in the middle and page
    /// markers of both kinds, as the backends emit them.
    fn paged_doc() -> DoclingDocument {
        let mut doc = DoclingDocument::new("p");
        doc.push(Node::PageInfo {
            page_no: 1,
            width: 100.0,
            height: 100.0,
        });
        doc.add_heading(1, "Title");
        doc.add_paragraph("Page one.");
        // Page two: a marker plus furniture only — renders nothing.
        doc.push(Node::PageBreak);
        doc.push(Node::PageInfo {
            page_no: 2,
            width: 100.0,
            height: 100.0,
        });
        doc.push(Node::PageFurniture {
            footer: true,
            location: [0, 500, 511, 511],
            text: "2".into(),
        });
        doc.push(Node::PageBreak);
        doc.push(Node::PageInfo {
            page_no: 3,
            width: 100.0,
            height: 100.0,
        });
        doc.add_paragraph("Page three.");
        // A trailing boundary with nothing after it.
        doc.push(Node::PageBreak);
        doc
    }

    #[test]
    fn page_break_placeholder_lands_between_pages_only() {
        let mut doc = paged_doc();
        // Off by default: docling's Markdown carries no page breaks.
        assert_eq!(
            doc.export_to_markdown(),
            "# Title\n\nPage one.\n\nPage three.\n"
        );
        doc.page_break_placeholder = Some("<!-- page break -->".into());
        // One break for the 1→3 transition (the empty page 2 and the doubled
        // PageBreak+PageInfo markers collapse), none before the first block,
        // none for the trailing boundary.
        assert_eq!(
            doc.export_to_markdown(),
            "# Title\n\nPage one.\n\n<!-- page break -->\n\nPage three.\n"
        );
        // An empty placeholder is still a (blank) part, as upstream's
        // `str.replace(marker, "")` leaves the delimiters around it.
        doc.page_break_placeholder = Some(String::new());
        assert_eq!(
            doc.export_to_markdown(),
            "# Title\n\nPage one.\n\n\n\nPage three.\n"
        );
    }

    #[test]
    fn page_break_placeholder_never_leads_a_single_page() {
        let mut doc = DoclingDocument::new("one");
        doc.page_break_placeholder = Some("---".into());
        doc.push(Node::PageBreak);
        doc.push(Node::PageInfo {
            page_no: 1,
            width: 10.0,
            height: 10.0,
        });
        doc.add_paragraph("Only page.");
        assert_eq!(doc.export_to_markdown(), "Only page.\n");
        // Two boundaries with no content between them: still one break.
        doc.push(Node::PageBreak);
        doc.push(Node::PageBreak);
        doc.add_paragraph("Next.");
        assert_eq!(doc.export_to_markdown(), "Only page.\n\n---\n\nNext.\n");
    }

    #[test]
    fn page_break_placeholder_streams_byte_identical() {
        let mut doc = paged_doc();
        doc.page_break_placeholder = Some("<!-- page break -->".into());
        // Split at every page marker (how the PDF pipeline pushes page batches)
        // and at odd places inside a page: the pending break must survive a
        // push that renders nothing (page two) and land on page three's block.
        for splits in [
            &[3usize][..],
            &[3, 6],
            &[3, 6, 8],
            &[1, 2, 3, 4, 5, 6, 7, 8, 9],
            &[8],
        ] {
            assert_stream_matches(&doc, false, ImageMode::Placeholder, splits);
            assert_stream_matches(&doc, true, ImageMode::Placeholder, splits);
        }
    }

    #[test]
    fn strict_tightens_punctuation_spacing_legacy_keeps_it() {
        let mut doc = DoclingDocument::new("t");
        doc.add_paragraph("see [ 37 , 36 ] and ( x ) .");
        // Legacy keeps docling's spacing byte-for-byte.
        assert_eq!(doc.export_to_markdown(), "see [ 37 , 36 ] and ( x ) .\n");
        // Strict tightens punctuation for readable Markdown.
        assert_eq!(doc.export_to_markdown_with(true), "see [37, 36] and (x).\n");
    }

    // ----- MarkdownExportOptions (#599) ------------------------------------

    /// A document touching every option: furniture, a comment, a hidden
    /// sheet group, a picture with nested text, escaped text.
    fn options_doc() -> DoclingDocument {
        use crate::document::ContentLayer;
        let mut doc = DoclingDocument::new("opts");
        doc.push(Node::PageFurniture {
            footer: false,
            location: [0, 0, 0, 0],
            text: "FAX COVER \\_ R&amp;D".into(),
        });
        doc.add_heading(1, "R&amp;D \\_ report");
        doc.add_paragraph("Body a\\_b &lt;tag&gt;");
        doc.push(Node::Picture {
            caption: Some("Figure 1".into()),
            caption_href: None,
            image: None,
            classification: None,
            description: None,
            caption_parent: Default::default(),
            caption_location: None,
        });
        doc.push(Node::PictureChildren(vec![
            Node::Paragraph {
                text: "Field: Name".into(),
            },
            Node::Paragraph {
                text: "Field: Date".into(),
            },
        ]));
        doc.push(Node::CommentSection {
            name: "comment-1".into(),
            text: "[author: A]: note".into(),
            refs_note_text: false,
            grouped: true,
        });
        doc.push(Node::Group {
            label: "section".into(),
            name: Some("sheet: Hidden".into()),
            layer: Some(ContentLayer::Invisible),
            children: vec![Node::Paragraph {
                text: "hidden cell".into(),
            }],
        });
        doc.push(Node::Furniture {
            layer: ContentLayer::Furniture,
            inner: Box::new(Node::Paragraph {
                text: "Page 1 of 2".into(),
            }),
        });
        doc.push(Node::FurnitureText {
            label: "page_footer".into(),
            text: "footer line".into(),
        });
        doc
    }

    /// The defaults are docling's: the options export is the plain export,
    /// byte for byte — and the plain export is unchanged (furniture, notes,
    /// hidden sheets and picture children out; escaping kept).
    #[test]
    fn default_options_are_the_default_export() {
        let doc = options_doc();
        let plain = doc.export_to_markdown();
        assert_eq!(
            plain,
            "# R&amp;D \\_ report\n\nBody a\\_b &lt;tag&gt;\n\nFigure 1\n\n<!-- image -->\n"
        );
        let (with_default, artifacts) =
            doc.export_to_markdown_with_options(&MarkdownExportOptions::default());
        assert_eq!(with_default, plain);
        assert!(artifacts.is_empty());
    }

    #[test]
    fn layers_render_furniture_notes_and_hidden_sheets() {
        use crate::document::ContentLayer;
        let doc = options_doc();
        let md = |layers: ContentLayers| {
            doc.export_to_markdown_with_options(&MarkdownExportOptions {
                layers,
                ..MarkdownExportOptions::default()
            })
            .0
        };
        assert_eq!(
            md(ContentLayers::BODY.with(ContentLayer::Furniture)),
            "FAX COVER \\_ R&amp;D\n\n# R&amp;D \\_ report\n\nBody a\\_b &lt;tag&gt;\n\nFigure 1\n\n<!-- image -->\n\nPage 1 of 2\n\nfooter line\n",
            "a page header, a furniture-wrapped paragraph and a furniture text item render as paragraphs"
        );
        assert_eq!(
            md(ContentLayers::BODY.with(ContentLayer::Notes)),
            "# R&amp;D \\_ report\n\nBody a\\_b &lt;tag&gt;\n\nFigure 1\n\n<!-- image -->\n\n[author: A]: note\n"
        );
        assert_eq!(
            md(ContentLayers::BODY.with(ContentLayer::Invisible)),
            "# R&amp;D \\_ report\n\nBody a\\_b &lt;tag&gt;\n\nFigure 1\n\n<!-- image -->\n\nhidden cell\n",
            "a hidden sheet's items sit on the sheet's layer"
        );
        assert_eq!(
            md(ContentLayers::NONE.with(ContentLayer::Furniture)),
            "FAX COVER \\_ R&amp;D\n\nPage 1 of 2\n\nfooter line\n",
            "without the body layer only the furniture prints"
        );
        assert_eq!(md(ContentLayers::NONE), "");
        assert_eq!(
            md(ContentLayers::ALL),
            "FAX COVER \\_ R&amp;D\n\n# R&amp;D \\_ report\n\nBody a\\_b &lt;tag&gt;\n\nFigure 1\n\n<!-- image -->\n\n[author: A]: note\n\nhidden cell\n\nPage 1 of 2\n\nfooter line\n"
        );
    }

    /// A list item carries its own layer: a furniture list renders with the
    /// furniture layer and stays out of the body export.
    #[test]
    fn layers_apply_to_list_items() {
        use crate::document::ContentLayer;
        let mut doc = DoclingDocument::new("lists");
        doc.push(Node::ListItem {
            ordered: false,
            number: 1,
            first_in_list: true,
            text: "nav link".into(),
            level: 0,
            marker: None,
            location: None,
            split: None,
            dclx: None,
            href: None,
            layer: Some(ContentLayer::Furniture),
        });
        doc.add_paragraph("body");
        assert_eq!(doc.export_to_markdown(), "body\n");
        let (md, _) = doc.export_to_markdown_with_options(&MarkdownExportOptions {
            layers: ContentLayers::BODY.with(ContentLayer::Furniture),
            ..MarkdownExportOptions::default()
        });
        assert_eq!(md, "- nav link\n\nbody\n");
    }

    #[test]
    fn traverse_pictures_prints_the_nested_text_after_the_picture() {
        let doc = options_doc();
        let (md, _) = doc.export_to_markdown_with_options(&MarkdownExportOptions {
            traverse_pictures: true,
            ..MarkdownExportOptions::default()
        });
        assert_eq!(
            md,
            "# R&amp;D \\_ report\n\nBody a\\_b &lt;tag&gt;\n\nFigure 1\n\n<!-- image -->\n\nField: Name\n\nField: Date\n"
        );
    }

    #[test]
    fn escaping_can_be_turned_off_per_kind() {
        let doc = options_doc();
        let (md, _) = doc.export_to_markdown_with_options(&MarkdownExportOptions {
            escape_html: false,
            ..MarkdownExportOptions::default()
        });
        assert_eq!(
            md,
            "# R&D \\_ report\n\nBody a\\_b <tag>\n\nFigure 1\n\n<!-- image -->\n"
        );
        let (md, _) = doc.export_to_markdown_with_options(&MarkdownExportOptions {
            escape_underscores: false,
            ..MarkdownExportOptions::default()
        });
        assert_eq!(
            md,
            "# R&amp;D _ report\n\nBody a_b &lt;tag&gt;\n\nFigure 1\n\n<!-- image -->\n"
        );
        // Code and table cells are never touched (upstream leaves them
        // unescaped too, so nothing was escaped there to undo).
        let mut doc = DoclingDocument::new("code");
        doc.push(Node::Code {
            language: None,
            text: "a &amp; b \\_ c".into(),
            orig: None,
            pretty: None,
        });
        let (md, _) = doc.export_to_markdown_with_options(&MarkdownExportOptions {
            escape_html: false,
            escape_underscores: false,
            ..MarkdownExportOptions::default()
        });
        assert_eq!(md, "```\na &amp; b \\_ c\n```\n");
    }

    /// #859 (docling-core 2.103.1): a fence out-lengths every backtick run
    /// in the block and a span its text, so neither can be closed early; the
    /// readers of the baked spans take the longer form back.
    #[test]
    fn fences_and_spans_out_length_inner_backticks() {
        let code = |text: &str| Node::Code {
            language: Some("md".into()),
            text: text.into(),
            orig: None,
            pretty: None,
        };
        let fence_of = |text: &str| {
            let mut doc = DoclingDocument::new("d");
            doc.push(code(text));
            (doc.export_to_markdown(), doc.export_to_markdown_with(true))
        };
        assert_eq!(fence_of("x").0, "```\nx\n```\n");
        assert_eq!(fence_of("a ` b").0, "```\na ` b\n```\n");
        assert_eq!(fence_of("a ``` b").0, "````\na ``` b\n````\n");
        assert_eq!(
            fence_of("a ``` b ```` c"),
            (
                "`````\na ``` b ```` c\n`````\n".to_string(),
                "`````md\na ``` b ```` c\n`````\n".to_string()
            )
        );
        assert_eq!(code_span("x"), "`x`");
        assert_eq!(code_span("a`b"), "``a`b``");
        assert_eq!(code_span("a ``` b"), "````a ``` b````");
        assert_eq!(code_span("`x"), "`` `x ``");
        assert_eq!(code_span("x`"), "`` x` ``");
        for (span, text) in [
            ("`x`", "x"),
            ("``a`b``", "a`b"),
            ("`` `x ``", "`x"),
            ("````a ``` b````", "a ``` b"),
        ] {
            assert_eq!(
                plain_inline(&format!("see {span} here")),
                format!("see {text} here")
            );
        }
        // The plain export prints a folded fence's body unfenced, whatever
        // the fence's length.
        let mut doc = DoclingDocument::new("d");
        doc.push(code("a ``` b"));
        assert_eq!(doc.export_to_text(), "a ``` b");
        // Streamed and buffered agree on the longer fence.
        let mut doc = DoclingDocument::new("d");
        doc.add_paragraph("Before.");
        doc.push(code("a ``` b ```` c"));
        doc.add_paragraph("After.");
        assert_stream_matches(&doc, false, ImageMode::Placeholder, &[1, 2]);
        assert_stream_matches(&doc, true, ImageMode::Placeholder, &[1, 2]);
    }

    /// #858: a `mermaid` code item extracted from a drawing takes the
    /// place of the drawing's `<!-- image -->` in the placeholder mode —
    /// in the buffered export and in a stream cut anywhere around the pair;
    /// a picture nothing follows keeps its marker.
    #[test]
    fn mermaid_code_item_replaces_its_picture_placeholder() {
        let picture = || Node::Picture {
            caption: None,
            caption_href: None,
            image: None,
            classification: None,
            description: None,
            caption_parent: Default::default(),
            caption_location: None,
        };
        let mut doc = DoclingDocument::new("d");
        doc.add_paragraph("Before.");
        doc.push(picture());
        doc.push(Node::Code {
            language: Some("mermaid".into()),
            text: "flowchart TD\n  A --> B".into(),
            orig: None,
            pretty: None,
        });
        doc.add_paragraph("After.");
        doc.push(picture());
        let md = doc.export_to_markdown();
        assert_eq!(
            md,
            "Before.\n\n```mermaid\nflowchart TD\n  A --> B\n```\n\nAfter.\n\n<!-- image -->\n"
        );
        assert_eq!(doc.export_to_markdown_with(true), md);
        for splits in [&[1][..], &[2], &[1, 2], &[2, 3], &[1, 2, 3, 4]] {
            assert_stream_matches(&doc, false, ImageMode::Placeholder, splits);
            assert_stream_matches(&doc, true, ImageMode::Placeholder, splits);
        }
        // A stream that ends on the picture still prints it.
        let mut doc = DoclingDocument::new("d");
        doc.add_paragraph("Only.");
        doc.push(picture());
        assert_eq!(doc.export_to_markdown(), "Only.\n\n<!-- image -->\n");
        assert_stream_matches(&doc, false, ImageMode::Placeholder, &[1]);
        assert_stream_matches(&doc, false, ImageMode::Placeholder, &[2]);
    }

    #[test]
    fn image_placeholder_is_configurable() {
        let doc = options_doc();
        let (md, _) = doc.export_to_markdown_with_options(&MarkdownExportOptions {
            image_placeholder: "[figure]".into(),
            ..MarkdownExportOptions::default()
        });
        assert!(md.contains("Figure 1\n\n[figure]\n"), "{md}");
        assert!(!md.contains("<!-- image -->"));
    }

    /// #605: an empty `image_placeholder` prints nothing — no empty block, so
    /// no extra blank lines — while the page-break scan still counts the
    /// picture as an item. Every expectation is docling-core 2.99's own
    /// `export_to_markdown(image_placeholder="", page_break_placeholder=…)`
    /// (plus the trailing newline this port always writes).
    #[test]
    fn empty_image_placeholder_prints_nothing() {
        fn picture(caption: Option<&str>) -> Node {
            Node::Picture {
                caption: caption.map(Into::into),
                caption_href: None,
                image: None,
                classification: None,
                description: None,
                caption_parent: Default::default(),
                caption_location: None,
            }
        }
        fn page(no: usize) -> Node {
            Node::PageInfo {
                page_no: no,
                width: 100.0,
                height: 100.0,
            }
        }
        fn para(text: &str) -> Node {
            Node::Paragraph { text: text.into() }
        }
        let options = MarkdownExportOptions {
            image_placeholder: String::new(),
            ..MarkdownExportOptions::default()
        };
        // (nodes, page-break placeholder, expected)
        let cases: Vec<(Vec<Node>, Option<&str>, &str)> = vec![
            (
                vec![page(1), para("a"), picture(None), para("b")],
                None,
                "a\n\nb\n",
            ),
            (
                vec![page(1), para("a"), picture(Some("Figure 1")), para("b")],
                None,
                "a\n\nFigure 1\n\nb\n",
            ),
            (
                vec![
                    page(1),
                    para("a"),
                    page(2),
                    picture(None),
                    page(3),
                    para("b"),
                ],
                Some("<!-- pb -->"),
                "a\n\n<!-- pb -->\n\n<!-- pb -->\n\nb\n",
            ),
            (
                vec![
                    page(1),
                    para("a"),
                    page(2),
                    picture(None),
                    page(3),
                    para("b"),
                ],
                Some(""),
                "a\n\n\n\n\n\nb\n",
            ),
            (
                vec![page(1), picture(None), page(2), para("b")],
                Some("<!-- pb -->"),
                "<!-- pb -->\n\nb\n",
            ),
            (
                vec![page(1), picture(None), page(2), para("b")],
                Some(""),
                "\n\nb\n",
            ),
            (
                vec![page(1), para("a"), page(2), picture(None)],
                Some("<!-- pb -->"),
                "a\n\n<!-- pb -->\n",
            ),
        ];
        for (nodes, placeholder, expected) in cases {
            let mut doc = DoclingDocument::new("t");
            doc.page_break_placeholder = placeholder.map(Into::into);
            for node in nodes {
                doc.push(node);
            }
            let (md, _) = doc.export_to_markdown_with_options(&options);
            assert_eq!(md, expected, "page break {placeholder:?}");
            // The streamer agrees at every chunking, one node per push
            // included — an all-empty push still owes the next page break.
            for size in 1..=doc.nodes.len() {
                let mut streamer = MarkdownStreamer::new(false, ImageMode::Placeholder, false)
                    .with_export_options(&options)
                    .with_page_break_placeholder(placeholder.map(Into::into));
                let mut out = String::new();
                for chunk in doc.nodes.chunks(size) {
                    out.push_str(&streamer.push(chunk, &[]));
                }
                out.push_str(&streamer.finish());
                assert_eq!(
                    out, expected,
                    "chunks of {size}, page break {placeholder:?}"
                );
            }
        }
        // The default placeholder is untouched.
        let mut doc = DoclingDocument::new("t");
        doc.push(para("a"));
        doc.push(picture(None));
        doc.push(para("b"));
        assert_eq!(doc.export_to_markdown(), "a\n\n<!-- image -->\n\nb\n");
    }

    /// The streamer takes the same options and its chunks concatenate to the
    /// buffered export.
    #[test]
    fn streamer_matches_buffered_export_with_options() {
        let doc = options_doc();
        let options = MarkdownExportOptions {
            layers: ContentLayers::ALL,
            traverse_pictures: true,
            escape_html: false,
            escape_underscores: false,
            image_placeholder: "(img)".into(),
            ..MarkdownExportOptions::default()
        };
        let (buffered, _) = doc.export_to_markdown_with_options(&options);
        let mut streamer = MarkdownStreamer::new(false, ImageMode::Placeholder, false)
            .with_export_options(&options);
        let mut out = String::new();
        for chunk in doc.nodes.chunks(2) {
            out.push_str(&streamer.push(chunk, &[]));
        }
        out.push_str(&streamer.finish());
        assert_eq!(out, buffered);
        assert!(
            buffered.contains("(img)") && buffered.contains("R&D _ report"),
            "{buffered}"
        );
    }

    /// #902: an escaped star is a literal one in the plain text, and closes
    /// no emphasis span around it.
    #[test]
    fn plain_text_reads_escaped_stars() {
        assert_eq!(plain_inline("A\\*\\*\\* GmbH"), "A*** GmbH");
        assert_eq!(
            plain_inline("**FN \\*0\\*271\\*h** and *h\\*.at*"),
            "FN *0*271*h and h*.at"
        );
    }

    /// #613: the plain export of a document holding every decoration the
    /// Markdown export writes, and the streamer agreeing with it.
    #[test]
    fn plain_text_drops_the_markdown_decoration() {
        let item = |text: &str, ordered: bool, number: u64, level: u8| Node::ListItem {
            ordered,
            number,
            first_in_list: number == 1 && level == 0,
            text: text.into(),
            level,
            marker: None,
            location: None,
            split: None,
            dclx: None,
            href: None,
            layer: None,
        };
        let mut doc = DoclingDocument::new("plain");
        doc.add_heading(1, "Title with **bold**");
        doc.add_heading(2, "Section");
        doc.add_paragraph("Some **bold**, *italic*, ~~gone~~ and `code` with a [link](https://x.org).\nNext line.");
        doc.push(item("*one*", false, 1, 0));
        doc.push(item("[two](https://y.org)", false, 2, 0));
        doc.push(item("nested", true, 1, 1));
        doc.push(Node::CheckboxItem {
            checked: true,
            text: "done".into(),
        });
        doc.push(Node::Code {
            language: Some("rust".into()),
            text: "let x = **y**;".into(),
            orig: None,
            pretty: None,
        });
        doc.push(Node::Picture {
            caption: Some("Figure *1*".into()),
            caption_href: None,
            image: None,
            classification: None,
            description: None,
            caption_parent: Default::default(),
            caption_location: None,
        });
        doc.push(Node::Caption {
            text: "Linked caption".into(),
            href: Some("https://z.org".into()),
        });
        doc.push(Node::Table(Table {
            rows: vec![
                vec!["**n**".into(), "rich".into()],
                vec!["**2**".into(), "a  \nb".into()],
            ],
            ..Default::default()
        }));
        doc.add_paragraph("R&amp;D \\_ x");
        let text = doc.export_to_text();
        assert_eq!(
            text,
            "Title with bold\n\n\
             Section\n\n\
             Some bold, italic, gone and code with a link.\nNext line.\n\n\
             - one\n- two\n    1. nested\n\n\
             - [x] done\n\n\
             let x = **y**;\n\n\
             Figure 1\n\n\
             Linked caption\n\n\
             |   n | rich   |\n\
             |-----|--------|\n\
             |   2 | a b    |\n\n\
             R&D _ x"
        );

        let options = MarkdownExportOptions::plain_text();
        let (buffered, _) = doc.export_to_markdown_with_options(&options);
        assert_eq!(buffered, format!("{text}\n"));
        let mut streamer = MarkdownStreamer::new(false, ImageMode::Placeholder, false)
            .with_export_options(&options);
        let mut out = String::new();
        for chunk in doc.nodes.chunks(3) {
            out.push_str(&streamer.push(chunk, &[]));
        }
        out.push_str(&streamer.finish());
        assert_eq!(out, buffered);
    }

    /// #613: text that only looks like Markdown stays as it is.
    #[test]
    fn plain_inline_keeps_literal_markers() {
        for (input, want) in [
            ("**bold** and *it* and ***both***", "bold and it and both"),
            ("**[label](https://a.b/c_(d))** end", "label end"),
            (
                "grant *USE and *OBJMGT authority",
                "grant *USE and *OBJMGT authority",
            ),
            ("2 * 3 * 4", "2 * 3 * 4"),
            ("a ** b", "a ** b"),
            ("the `quoted’ and `x` term", "the `quoted’ and x term"),
            ("see [12] and [docs](u)", "see [12] and docs"),
            ("formula $[a](b)$ stays", "formula $[a](b)$ stays"),
            ("price $5 and **bold**", "price $5 and bold"),
            ("[outer [inner] text](u)", "outer [inner] text"),
            ("unclosed **bold", "unclosed **bold"),
        ] {
            assert_eq!(plain_inline(input), want, "{input}");
        }
    }
}
