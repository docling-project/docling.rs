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
        images,
        artifacts_dir: artifacts_dir.to_string(),
        artifacts: Vec::new(),
        pic_index: 0,
    };
    let mut blocks: Vec<String> = Vec::new();
    render(&doc.nodes, &mut blocks, &mut ctx);
    let body = blocks.join("\n\n");
    let md = if body.is_empty() {
        String::new()
    } else {
        format!("{body}\n")
    };
    (md, ctx.artifacts)
}

fn render(nodes: &[Node], blocks: &mut Vec<String>, ctx: &mut Ctx) {
    let mut i = 0;
    while i < nodes.len() {
        match &nodes[i] {
            Node::ListItem { .. } => {
                let start = i;
                while matches!(nodes.get(i), Some(Node::ListItem { .. })) {
                    i += 1;
                }
                render_list_run(&nodes[start..i], blocks);
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
fn render_list_run(items: &[Node], blocks: &mut Vec<String>) {
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
        } = item
        else {
            continue;
        };
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
        lines.push(format!("{indent}{marker} {text}"));
        prev[level] = Some((*ordered, *number));
    }

    blocks.push(lines.join("\n"));
}

fn render_one(node: &Node, blocks: &mut Vec<String>, ctx: &mut Ctx) {
    match node {
        Node::Heading { level, text } => {
            let hashes = "#".repeat((*level).clamp(1, 6) as usize);
            blocks.push(format!("{hashes} {text}"));
        }
        Node::Paragraph { text } => blocks.push(text.clone()),
        Node::Code { language, text } => {
            // Legacy docling never emits a language on the fence; strict keeps it.
            let lang = match language {
                Some(l) if ctx.strict => l.as_str(),
                _ => "",
            };
            blocks.push(format!("```{lang}\n{text}\n```"));
        }
        Node::Table(table) => {
            let rendered = render_table(table);
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
        Node::Group { children, .. } => render(children, blocks, ctx),
        // Handled by the run-merging branch in `render`.
        Node::ListItem { .. } => unreachable!("list items are rendered in runs"),
    }
}

/// The Markdown for a picture under the active [`ImageMode`]; Referenced mode also
/// records the bytes in `ctx.artifacts` for the caller to write.
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

/// Render a table the way docling-core does: `tabulate(tablefmt="github")`.
///
/// Each cell is first escaped (`\n` → space, `|` → `&#124;`) so it can't break
/// the table. Columns are padded to a fixed width; the header contributes its
/// width plus a minimum padding of 2; numeric columns (every data cell parses
/// as a number) are right-aligned, others left-aligned; the separator is plain
/// dashes of `width + 2` (github tablefmt emits no alignment colons here). Row 0
/// is the header.
fn render_table(table: &Table) -> String {
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

    // Display width (Unicode scalar count — good enough for now).
    let dw = |s: &str| s.chars().count();
    let data_rows = 1..grid.len();

    // A column is right-aligned when it has data and every data cell is numeric.
    let right: Vec<bool> = (0..num_cols)
        .map(|c| {
            !data_rows.is_empty()
                && data_rows.clone().all(|r| {
                    let t = grid[r][c].trim();
                    !t.is_empty() && t.parse::<f64>().is_ok()
                })
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
        });
        doc.push(Node::ListItem {
            ordered: false,
            number: 2,
            first_in_list: false,
            text: "second".into(),
            level: 0,
        });
        let md = doc.export_to_markdown();
        assert_eq!(md, "# Title\n\nHello world.\n\n- first\n- second\n");
    }

    #[test]
    fn renders_github_table() {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::Table(Table {
            rows: vec![vec!["a".into(), "b".into()], vec!["1".into(), "2".into()]],
        }));
        let md = doc.export_to_markdown();
        // Matches tabulate(tablefmt="github"): padded columns, numeric cells
        // right-aligned, separator of width+2 dashes.
        assert_eq!(md, "|   a |   b |\n|-----|-----|\n|   1 |   2 |\n");
    }
}
