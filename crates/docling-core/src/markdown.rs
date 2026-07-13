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
/// Only [`ImageMode::Placeholder`] and [`ImageMode::Embedded`] are streamable:
/// [`ImageMode::Referenced`] needs a side-channel for the image bytes, which only
/// the buffered [`to_markdown_images`] provides.
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
}

impl MarkdownStreamer {
    /// Create a streamer. `compact_tables` mirrors [`DoclingDocument::compact_tables`].
    pub fn new(strict: bool, images: ImageMode, compact_tables: bool) -> Self {
        debug_assert!(
            images != ImageMode::Referenced,
            "referenced image mode is not streamable; use to_markdown_images"
        );
        Self {
            strict,
            images,
            compact_tables,
            emitted_any: false,
            links: Vec::new(),
        }
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
            // Referenced mode is rejected at construction, so the artifact sink is
            // never touched.
            artifacts_dir: String::new(),
            artifacts: Vec::new(),
            pic_index: 0,
        };
        let mut blocks: Vec<String> = Vec::new();
        render(nodes, &mut blocks, &mut ctx);
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

    for item in items {
        let Node::ListItem {
            ordered,
            number,
            first_in_list,
            text,
            level,
            marker: _,
            location: _,
            dclx: _,
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
        if let Some((prev_ordered, prev_number)) = prev[level] {
            let new_list = *first_in_list
                || prev_ordered != *ordered
                || (*ordered && *number != prev_number + 1);
            if new_list {
                lines.push(String::new());
            }
        }

        let indent = "    ".repeat(level);
        let marker = if *ordered {
            format!("{number}.")
        } else {
            "-".to_string()
        };
        lines.push(format!("{indent}{marker} {}", strict_text(text, strict)));
        prev[level] = Some((*ordered, *number));
    }

    // A run consisting only of furniture (content-layer-filtered) items yields no
    // lines; pushing an empty block here would surface as a stray blank line.
    if !lines.is_empty() {
        blocks.push(lines.join("\n"));
    }
}

fn render_one(node: &Node, blocks: &mut Vec<String>, ctx: &mut Ctx) {
    match node {
        Node::Heading { level, text } => {
            let hashes = "#".repeat((*level).clamp(1, 6) as usize);
            blocks.push(format!("{hashes} {}", strict_text(text, ctx.strict)));
        }
        // An empty body paragraph (docling's blank-line text item) contributes
        // nothing to Markdown — only DocLang/JSON keep it.
        Node::Paragraph { text } if text.is_empty() => {}
        Node::Paragraph { text } => blocks.push(strict_text(text, ctx.strict)),
        Node::CheckboxItem { checked, text } => {
            let mark = if *checked { "- [x] " } else { "- [ ] " };
            blocks.push(strict_text(&format!("{mark}{text}"), ctx.strict));
        }
        Node::Code { language, text } => {
            // Legacy docling never emits a language on the fence; strict keeps it.
            let lang = match language {
                Some(l) if ctx.strict => l.as_str(),
                _ => "",
            };
            blocks.push(format!("```{lang}\n{text}\n```"));
        }
        Node::Table(table) => {
            let rendered = render_table(table, ctx.compact_tables);
            if !rendered.is_empty() {
                blocks.push(rendered);
            }
        }
        Node::Picture { caption, image } => {
            if let Some(cap) = caption {
                if !cap.is_empty() {
                    blocks.push(cap.clone());
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
                    blocks.push(cap.clone());
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
        Node::Group { children, .. } => render(children, blocks, ctx),
        Node::FieldRegion { items } => {
            // docling renders the region container (which carries no text of its
            // own) as a `<!-- missing-text -->` marker, then each field item the
            // same way, followed by that item's marker/key/value as separate
            // paragraphs.
            blocks.push(MISSING_TEXT.to_string());
            for item in items {
                blocks.push(MISSING_TEXT.to_string());
                for part in [&item.marker, &item.key, &item.value].into_iter().flatten() {
                    blocks.push(strict_text(part, ctx.strict));
                }
            }
        }
        // A rich inline group renders exactly like a paragraph of its Markdown
        // text — the structured runs are DocLang-only.
        Node::InlineGroup { md_text, .. } => blocks.push(strict_text(md_text, ctx.strict)),
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
        // Layout provenance is DocLang-only; render the wrapped node.
        Node::Located { inner, .. } => render_one(inner, blocks, ctx),
        // Page breaks are DocLang-only; docling omits them from Markdown.
        Node::PageBreak => {}
        // Handled by the run-merging branch in `render`.
        Node::ListItem { .. } => unreachable!("list items are rendered in runs"),
    }
}

/// docling's placeholder for a structural node (a field region / item) that has
/// no text of its own.
const MISSING_TEXT: &str = "<!-- missing-text -->";

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
            format!("![Image]({path})")
        }
        // Placeholder, or any mode with no extracted image.
        _ => "<!-- image -->".to_string(),
    }
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
/// table. Row 0 is the header.
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

fn render_table(table: &Table, compact: bool) -> String {
    if table.rows.is_empty() {
        return String::new();
    }
    let num_cols = table.rows.iter().map(Vec::len).max().unwrap_or(0);
    if num_cols == 0 {
        return String::new();
    }

    // Escaped, rectangular grid (ragged rows padded with empty cells). `tabulate`
    // strips data cells of surrounding whitespace but leaves the header row as-is.
    let grid: Vec<Vec<String>> = table
        .rows
        .iter()
        .enumerate()
        .map(|(r, row)| {
            (0..num_cols)
                .map(|c| {
                    let cell = escape_cell(row.get(c).map(String::as_str).unwrap_or(""));
                    if r == 0 {
                        cell
                    } else {
                        cell.trim().to_string()
                    }
                })
                .collect()
        })
        .collect();

    if compact {
        // Compact: cells joined by " | ", no padding, single-dash separators.
        let render_row = |r: usize| -> String { format!("| {} |", grid[r].join(" | ")) };
        let mut lines = Vec::with_capacity(grid.len() + 1);
        lines.push(render_row(0));
        let sep: Vec<&str> = (0..num_cols).map(|_| "-").collect();
        lines.push(format!("| {} |", sep.join(" | ")));
        for r in 1..grid.len() {
            lines.push(render_row(r));
        }
        return lines.join("\n");
    }

    // Display width (Unicode scalar count — good enough for now).
    let dw = |s: &str| s.chars().count();
    let data_rows = 1..grid.len();

    // A column is right-aligned when at least one data cell is numeric and every
    // non-empty data cell is numeric — matching `tabulate`'s column typing, where
    // empty cells are "missing" (ignored) and a number may carry thousands
    // separators (`7,015`), which a plain `f64` parse rejects.
    let right: Vec<bool> = (0..num_cols)
        .map(|c| {
            let mut any = false;
            for r in data_rows.clone() {
                let t = grid[r][c].trim();
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

    // Column width = max(header_width + MIN_PADDING(2), max data-cell width).
    let width: Vec<usize> = (0..num_cols)
        .map(|c| {
            let mut w = dw(&grid[0][c]) + 2;
            for r in data_rows.clone() {
                w = w.max(dw(&grid[r][c]));
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
    let render_row = |r: usize| -> String {
        let cells: Vec<String> = (0..num_cols).map(|c| fmt_cell(&grid[r][c], c)).collect();
        format!("|{}|", cells.join("|"))
    };

    let mut lines = Vec::with_capacity(grid.len() + 1);
    lines.push(render_row(0));
    let sep: Vec<String> = (0..num_cols).map(|c| "-".repeat(width[c] + 2)).collect();
    lines.push(format!("|{}|", sep.join("|")));
    for r in data_rows {
        lines.push(render_row(r));
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
        let want = to_markdown_images(doc, strict, images, "artifacts").0;
        let mut streamer = MarkdownStreamer::new(strict, images, doc.compact_tables);
        let mut got = String::new();
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
        got.push_str(&streamer.finish());
        assert_eq!(
            got, want,
            "streamed output diverged (splits={splits:?}, strict={strict})"
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
        });
        doc.push(Node::Table(Table {
            rows: vec![vec!["a".into(), "b".into()], vec!["1".into(), "2".into()]],
            location: None,
            structure: None,
            cell_blocks: None,
        }));
        doc.push(Node::Picture {
            caption: Some("Fig 1".into()),
            image: None,
        });
        doc.add_paragraph("Last paragraph.");

        // A run of list items must never straddle a split, so try splits that fall
        // on safe block boundaries (the streaming PDF assembler guarantees this).
        for &strict in &[false, true] {
            for &images in &[ImageMode::Placeholder, ImageMode::Embedded] {
                for splits in [&[][..], &[1][..], &[2][..], &[4][..], &[1, 4, 6][..]] {
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
