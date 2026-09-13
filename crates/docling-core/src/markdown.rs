//! Markdown serializer for [`DoclingDocument`].

use crate::document::{DoclingDocument, Node, Table};

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
    let mut ctx = Ctx {
        strict,
        compact_tables: doc.compact_tables,
        images,
        artifacts_dir: artifacts_dir.to_string(),
        artifacts: Vec::new(),
        pic_index: 0,
        in_table_cell: false,
    };
    let mut blocks: Vec<String> = Vec::new();
    render(&doc.nodes, &mut blocks, &mut ctx);
    let mut body = blocks.join("\n\n");
    // Strict mode only: turn recovered source hyperlinks into Markdown links.
    // docling's standard pipeline drops them, so doing this in legacy mode would
    // diverge from docling — hence strict-only, leaving conformance output intact.
    if strict && !doc.links.is_empty() {
        body = apply_links(&body, &doc.links);
    }
    let md = if body.is_empty() {
        String::new()
    } else {
        format!("{body}\n")
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
    let mut ctx = Ctx {
        strict,
        compact_tables: doc.compact_tables,
        images: ImageMode::Placeholder,
        artifacts_dir: String::new(),
        artifacts: Vec::new(),
        pic_index: 0,
        in_table_cell: true,
    };
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
            let replacement = format!("[{anchor}]({href})");
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
            let replacement = format!("[{anchor}]({href})");
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
            emitted_any: false,
            links: Vec::new(),
            artifacts_dir: artifacts_dir.to_string(),
            artifacts: Vec::new(),
            pic_index: 0,
        }
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
        self.links.extend(links.iter().cloned());
        let mut ctx = Ctx {
            strict: self.strict,
            compact_tables: self.compact_tables,
            images: self.images,
            artifacts_dir: std::mem::take(&mut self.artifacts_dir),
            artifacts: std::mem::take(&mut self.artifacts),
            pic_index: self.pic_index,
            in_table_cell: false,
        };
        let mut blocks: Vec<String> = Vec::new();
        render(nodes, &mut blocks, &mut ctx);
        self.artifacts_dir = std::mem::take(&mut ctx.artifacts_dir);
        self.artifacts = std::mem::take(&mut ctx.artifacts);
        self.pic_index = ctx.pic_index;
        if blocks.is_empty() {
            return String::new();
        }
        let mut body = blocks.join("\n\n");
        if self.strict && !self.links.is_empty() {
            body = apply_links_chunk(&body, &mut self.links);
        }
        let chunk = if self.emitted_any {
            format!("\n\n{body}")
        } else {
            body
        };
        self.emitted_any = true;
        chunk
    }

    /// Emit the trailing newline that finishes the document (empty if no content
    /// was produced). Call exactly once, after the final [`push`](Self::push).
    pub fn finish(self) -> String {
        if self.emitted_any {
            "\n".to_string()
        } else {
            String::new()
        }
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
    text.replace("\\_", "_")
        .replace(" ,", ",")
        .replace(" .", ".")
        .replace(" ;", ";")
        .replace(" )", ")")
        .replace("( ", "(")
        .replace(" ]", "]")
        .replace("[ ", "[")
}

/// docling-core 2.92's `_md_line_breaks` (docling-core#721): a single `\n`
/// inside an item's text becomes a GFM hard line break (`"  \n"`, two trailing
/// spaces) so renderers honour it; a blank line (`\n\n`) is a paragraph break
/// and stays as is — the document scope already joins blocks with `\n\n`.
/// Applied to body text, list items and captions, never to code/formulas.
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
        match &nodes[i] {
            Node::ListItem { .. } => {
                let start = i;
                i += 1;
                loop {
                    match nodes.get(i) {
                        Some(Node::ListItem { .. }) => i += 1,
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
                render_list_run(&nodes[start..i], blocks, ctx.strict);
            }
            other => {
                render_one(other, blocks, ctx);
                i += 1;
            }
        }
    }
}

/// Render a contiguous run of list items.
///
/// Ordered items use their explicit `number`. A new sibling list (marked by
/// `first_in_list`) at the same depth is separated by a blank line, matching
/// docling-core's serializer.
fn render_list_run(items: &[Node], blocks: &mut Vec<String>, strict: bool) {
    let mut lines: Vec<String> = Vec::new();
    // Per level, the previous item's (ordered, number) so we can detect a new
    // sibling list.
    let mut prev: Vec<Option<(bool, u64)>> = Vec::new();
    // Whether the previous top-level item was a multilevel projection — an
    // ordered `1.2.`-style item rendered as a Markdown bullet (docx's DocLang
    // overlay says ordered, the flat field says bullet). Word numbers such an
    // item and its parent-level successor within one list (same `numId`), and
    // docling keeps them in one group — so the kind-flip / number-continuity
    // breaks below must not fire across it (docling#3902's
    // docx_list_blank_spacer: `- 1.2. Sub two` directly followed by
    // `2. Second section`, no blank line).
    let mut prev_projected = false;
    // Whether a deeper-level item has been rendered since the previous
    // top-level one. docling's AsciiDoc shape hangs a nested list off the
    // *group* rather than off the preceding item, so the nested group occupies
    // a position in the parent group and the next top-level item is numbered
    // past it (`3.` … `5.`). That gap is one list, not two, so it must not
    // trigger the new-sibling-list blank line below. Backends whose nested
    // lists hang off the item (HTML, DOCX, Markdown) number contiguously and
    // are unaffected.
    let mut nested_since_top = false;

    for item in items {
        let Node::ListItem {
            ordered,
            number,
            first_in_list,
            text,
            level,
            marker: _,
            location: _,
            dclx,
            href: _,
            layer,
        } = item
        else {
            continue;
        };
        // A non-body (furniture) list item is omitted from Markdown, matching
        // docling's content-layer filtering.
        if layer.is_some() {
            continue;
        }
        let level = *level as usize;

        // Returning to a shallower level ends the deeper sibling lists.
        prev.truncate(level + 1);
        while prev.len() <= level {
            prev.push(None);
        }

        // A new sibling list at the same depth gets a blank line: the kind flips
        // (`<ul>`↔`<ol>`), an ordered run breaks (`1, 2` then `42`), or the
        // backend flagged a fresh list (e.g. Markdown's bullet changing `-`→`*`).
        // Only at the top level: nested sibling groups are children of a list
        // item, and docling joins an item's children without blank lines.
        let eff_ordered = dclx.as_ref().map_or(*ordered, |d| d.ordered);
        if level == 0 {
            if let Some((prev_ordered, prev_number)) = prev[level] {
                // A projected predecessor suppresses both heuristics for an
                // ordered successor: the flat kind flip is an artifact of the
                // bullet projection, and the numbering continues the deeper
                // sequence (`1.2.` → `2.`), not this level's.
                let same_word_list = prev_projected && eff_ordered;
                let new_list = *first_in_list
                    || (!same_word_list
                        && (prev_ordered != *ordered
                            || (*ordered && !nested_since_top && *number != prev_number + 1)));
                if new_list {
                    lines.push(String::new());
                }
            }
            prev_projected = eff_ordered && !*ordered;
            nested_since_top = false;
        } else {
            nested_since_top = true;
        }

        let indent = "    ".repeat(level);
        let marker = if *ordered {
            format!("{number}.")
        } else {
            "-".to_string()
        };
        lines.push(format!("{indent}{marker} {}", list_item_text(text, strict)));
        prev[level] = Some((*ordered, *number));
    }

    // A run consisting only of furniture (content-layer-filtered) items yields no
    // lines; pushing an empty block here would surface as a stray blank line.
    if !lines.is_empty() {
        blocks.push(lines.join("\n"));
    }
}

/// A list item's Markdown body. The GFM hard-line-break rule (docling-core#721)
/// applies to the item's own text; pictures the HTML backend folded into the
/// item (`"\n[alt\n]<!-- image -->"` per `<img>` inside the `<li>`) are
/// docling's picture *children* of the item, which its serializer prints after
/// the item line with plain newlines — so a folded tail keeps its newlines
/// unmarked. The tail is recognised structurally: every line after the first is
/// an image marker or an alt caption directly followed by one.
fn list_item_text(text: &str, strict: bool) -> String {
    let escaped = strict_text(text, strict);
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
    const FENCE: &str = "```";
    let mut lines = tail.split('\n').peekable();
    let mut any = false;
    while let Some(line) = lines.next() {
        let line = line.trim_start();
        if line == MARKER {
            any = true;
        } else if line == FENCE {
            // Skip the block's body; an unclosed fence is not a folded child.
            loop {
                match lines.next() {
                    Some(l) if l.trim_start() == FENCE => break,
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

fn render_one(node: &Node, blocks: &mut Vec<String>, ctx: &mut Ctx) {
    match node {
        Node::Heading { level, text } => {
            let text = heading_line_breaks(&strict_text(text, ctx.strict));
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
        Node::Paragraph { text } => blocks.push(md_line_breaks(&strict_text(text, ctx.strict))),
        // A standalone caption item renders like a text item; its hyperlink
        // annotation becomes a Markdown link around the whole caption.
        Node::Caption { text, .. } if text.is_empty() => {}
        Node::Caption { text, href } => {
            let body = md_line_breaks(&strict_text(text, ctx.strict));
            blocks.push(match href {
                Some(url) => format!("[{body}]({url})"),
                None => body,
            });
        }
        Node::CheckboxItem { checked, text } => {
            let mark = if *checked { "- [x] " } else { "- [ ] " };
            blocks.push(md_line_breaks(&strict_text(
                &format!("{mark}{text}"),
                ctx.strict,
            )));
        }
        Node::Code {
            language,
            text,
            pretty,
            ..
        } => {
            // Legacy docling never emits a language on the fence; strict keeps it.
            let lang = match language {
                Some(l) if ctx.strict => l.as_str(),
                _ => "",
            };
            // Strict prefers the line-preserving rendering when the backend
            // supplied one (PDF); legacy stays on docling's flat `text`.
            let body = match pretty {
                Some(p) if ctx.strict => p.as_str(),
                _ => text.as_str(),
            };
            blocks.push(format!("```{lang}\n{body}\n```"));
        }
        // A CodeFormula-decoded display formula renders as docling's `$$…$$`
        // (the un-enriched pipeline emits a placeholder paragraph instead).
        Node::Formula { latex, .. } => blocks.push(format!("$${latex}$$")),
        Node::Table(table) => {
            // docling renders a table's caption as a text line before the grid.
            // `caption` is already escaped (backend convention), like a paragraph.
            if let Some(cap) = &table.caption {
                if !cap.is_empty() {
                    blocks.push(md_line_breaks(&strict_text(cap, ctx.strict)));
                }
            }
            let rendered = render_table(table, ctx.compact_tables);
            if !rendered.is_empty() {
                blocks.push(rendered);
            }
        }
        // Classification predictions don't affect docling's Markdown output.
        Node::Picture { caption, image, .. } => {
            if let Some(cap) = caption {
                if !cap.is_empty() {
                    blocks.push(md_line_breaks(cap));
                }
            }
            blocks.push(picture_marker(image.as_ref(), ctx));
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
                    blocks.push(md_line_breaks(cap));
                }
            }
            blocks.push(picture_marker(None, ctx));
            blocks.push(humanize_label(kind));
            let rendered = render_table(table, false);
            if !rendered.is_empty() {
                blocks.push(rendered);
            }
        }
        // A DocLang-only node is omitted from Markdown.
        Node::DoclangOnly(_) => {}
        // A group on a non-body layer (a hidden spreadsheet sheet) renders
        // nothing, like every other non-body item.
        Node::Group { layer: Some(_), .. } => {}
        Node::Group { children, .. } => render(children, blocks, ctx),
        Node::FieldRegion { items } => {
            // The region container and each field item carry no text of their
            // own; docling-core 2.93 (#724) serializes them to nothing (older
            // releases emitted a `<!-- missing-text -->` marker for each), so
            // only an item's marker/key/value appear, as separate paragraphs.
            for item in items {
                for part in [&item.marker, &item.key, &item.value].into_iter().flatten() {
                    blocks.push(md_line_breaks(&strict_text(part, ctx.strict)));
                }
            }
        }
        // A rich inline group renders exactly like a paragraph of its Markdown
        // text — the structured runs are DocLang-only.
        Node::InlineGroup { md_text, .. } => {
            blocks.push(md_line_breaks(&strict_text(md_text, ctx.strict)))
        }
        // A plain-text backend dump renders verbatim as a single block.
        Node::TextDump(text) => {
            if !text.is_empty() {
                blocks.push(text.clone());
            }
        }
        // Furniture (page headers/footers, HTML `<title>`) is excluded from
        // Markdown by default, mirroring docling.
        Node::Furniture { .. } => {}
        Node::PageFurniture { .. } => {}
        // A comment lives in the notes layer — omitted like other furniture;
        // the annotation on a body item is JSON-only, so render the item.
        Node::CommentSection { .. } => {}
        Node::Commented { inner, .. } => render_one(inner, blocks, ctx),
        // Layout provenance is DocLang-only; render the wrapped node.
        Node::Located { inner, .. } => render_one(inner, blocks, ctx),
        // Page breaks are DocLang-only; docling omits them from Markdown.
        Node::PageBreak => {}
        // Page markers feed the JSON export only.
        Node::PageInfo { .. } => {}
        // Runs of adjacent list items are merged by `render`; a stray single
        // item (a hand-built document, or a `Located` wrapper around one)
        // still renders as its own one-item list instead of panicking —
        // `nodes` is public API, so every representable tree must serialize.
        Node::ListItem { .. } => render_list_run(std::slice::from_ref(node), blocks, ctx.strict),
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

fn picture_marker(image: Option<&crate::PictureImage>, ctx: &mut Ctx) -> String {
    match (ctx.images, image) {
        (ImageMode::Embedded, Some(img)) => format!("![Image]({})", img.data_uri()),
        (ImageMode::Referenced, Some(img)) => {
            let path = format!(
                "{}/image_{:06}.{}",
                ctx.artifacts_dir,
                ctx.pic_index,
                ext_for(&img.mimetype)
            );
            ctx.pic_index += 1;
            ctx.artifacts.push((path.clone(), img.data.clone()));
            format!("![Image]({})", escape_uri_path(&path))
        }
        // Placeholder, or any mode with no extracted image.
        _ => "<!-- image -->".to_string(),
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

fn ext_for(mimetype: &str) -> &str {
    match mimetype {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/tiff" => "tif",
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
                mimetype: "image/png".into(),
                width: 1,
                height: 1,
                data: b"x".to_vec(),
            }),
            classification: None,
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
        assert_eq!(
            list_item_text("Step\n<!-- image -->", false),
            "Step\n<!-- image -->"
        );
        assert_eq!(
            list_item_text("Step\nAlt text\n<!-- image -->\n<!-- image -->", false),
            "Step\nAlt text\n<!-- image -->\n<!-- image -->"
        );
        assert_eq!(
            list_item_text("line one\nline two", false),
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

    /// A pivot table's row headers (`<th rowspan>`) are flagged `column_header`
    /// by docling's HTML backend; a row where data cells start alongside them
    /// stays in the body (deliberate deviation from docling-core#723, which
    /// folds `2025 | January | $134` into the header row).
    #[test]
    fn row_headers_beside_data_cells_do_not_extend_the_header() {
        let mut t = Table {
            rows: vec![
                vec!["Year".into(), "Month".into()],
                vec!["2025".into(), "January".into()],
                vec!["2025".into(), "February".into()],
            ],
            ..Default::default()
        };
        t.structure = Some(TableStructure {
            col_header: vec![vec![true, true], vec![true, false], vec![true, false]],
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

    #[test]
    fn renders_compact_table() {
        let mut doc = DoclingDocument::new("t");
        // The compact form is opt-in (the PDF backend sets it); default output uses
        // the padded GitHub serializer (covered by the regression fixtures).
        doc.compact_tables = true;
        doc.push(Node::Table(Table {
            rows: vec![vec!["a".into(), "b".into()], vec!["1".into(), "2".into()]],
            location: None,
            structure: None,
            cell_blocks: None,
            cells: None,
            caption: None,
        }));
        let md = doc.export_to_markdown();
        assert_eq!(md, "| a | b |\n| - | - |\n| 1 | 2 |\n");
    }

    #[test]
    fn renders_padded_github_table_by_default() {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::Table(Table {
            rows: vec![vec!["a".into(), "b".into()], vec!["1".into(), "2".into()]],
            location: None,
            structure: None,
            cell_blocks: None,
            cells: None,
            caption: None,
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
            MarkdownStreamer::with_artifacts(strict, images, doc.compact_tables, "artifacts");
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
            rows: vec![vec!["a".into(), "b".into()], vec!["1".into(), "2".into()]],
            location: None,
            structure: None,
            cell_blocks: None,
            cells: None,
            caption: None,
        }));
        doc.push(Node::Picture {
            caption: Some("Fig 1".into()),
            caption_href: None,
            image: Some(PictureImage {
                mimetype: "image/png".into(),
                width: 2,
                height: 2,
                data: b"png-one".to_vec(),
            }),
            classification: None,
        });
        doc.add_paragraph("Last paragraph.");
        // A second embedded picture, so referenced mode must keep numbering
        // (`image_000001`) across chunk boundaries.
        doc.push(Node::Picture {
            caption: None,
            caption_href: None,
            image: Some(PictureImage {
                mimetype: "image/png".into(),
                width: 2,
                height: 2,
                data: b"png-two".to_vec(),
            }),
            classification: None,
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

    #[test]
    fn strict_tightens_punctuation_spacing_legacy_keeps_it() {
        let mut doc = DoclingDocument::new("t");
        doc.add_paragraph("see [ 37 , 36 ] and ( x ) .");
        // Legacy keeps docling's spacing byte-for-byte.
        assert_eq!(doc.export_to_markdown(), "see [ 37 , 36 ] and ( x ) .\n");
        // Strict tightens punctuation for readable Markdown.
        assert_eq!(doc.export_to_markdown_with(true), "see [37, 36] and (x).\n");
    }
}
