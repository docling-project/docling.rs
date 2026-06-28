//! Layout-driven assembly: map detected [`Region`]s + text cells to a
//! [`DoclingDocument`], mirroring docling's page-assembly + reading-order.
//!
//! Overlapping detections are resolved greedily by score, each text cell is
//! assigned to its best-containing region, regions are ordered in reading order
//! (two-column aware), and each becomes a typed node by its layout label.

use fleischwolf_core::{DoclingDocument, Node, PictureImage, Table};

use crate::layout::Region;
use crate::pdfium_backend::{PdfPage, TextCell};

fn area(l: f32, t: f32, r: f32, b: f32) -> f32 {
    ((r - l).max(0.0)) * ((b - t).max(0.0))
}

/// Intersection area of two boxes.
fn inter(a: &Region, l: f32, t: f32, r: f32, b: f32) -> f32 {
    let il = a.l.max(l);
    let it = a.t.max(t);
    let ir = a.r.min(r);
    let ib = a.b.min(b);
    area(il, it, ir, ib)
}

/// Greedily keep regions by descending score, dropping a region that is mostly
/// covered by an already-kept one (RT-DETR emits overlapping duplicates).
pub fn resolve(mut regions: Vec<Region>) -> Vec<Region> {
    regions.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Region> = Vec::new();
    for r in regions {
        let ra = area(r.l, r.t, r.r, r.b).max(1.0);
        let covered = kept.iter().any(|k| {
            let i = inter(&r, k.l, k.t, k.r, k.b);
            let ka = area(k.l, k.t, k.r, k.b).max(1.0);
            // drop if most of r is inside k, or they strongly mutually overlap
            i / ra > 0.7 || i / (ra + ka - i) > 0.5
        });
        if !covered {
            kept.push(r);
        }
    }
    kept
}

/// Furniture / not-yet-emitted labels.
fn is_skipped(label: &str) -> bool {
    matches!(
        label,
        "page_header"
            | "page_footer"
            | "checkbox_selected"
            | "checkbox_unselected"
            | "form"
            | "key_value_region"
            | "document_index"
    )
}

/// Reading-order sort of regions, with two-column detection on the page.
fn order_regions(regions: &mut [Region], page_w: f32) {
    let cx = page_w / 2.0;
    let band = page_w * 0.08;
    let crossing = regions
        .iter()
        .filter(|r| r.l < cx - band && r.r > cx + band)
        .count();
    let two_col = !regions.is_empty()
        && (crossing as f32) / (regions.len() as f32) < 0.25
        && regions.iter().any(|r| r.r <= cx)
        && regions.iter().any(|r| r.l >= cx);
    if two_col {
        regions.sort_by(|a, b| {
            let ca = ((a.l + a.r) / 2.0) >= cx;
            let cb = ((b.l + b.r) / 2.0) >= cx;
            ca.cmp(&cb)
                .then(a.t.total_cmp(&b.t))
                .then(a.l.total_cmp(&b.l))
        });
    } else {
        regions.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.l.total_cmp(&b.l)));
    }
}

/// Clean a region's assembled text: undo soft-hyphen line wraps, map curly
/// quotes and the ellipsis to ASCII (matching docling), and collapse runs of
/// whitespace. pdfium emits the line-wrap hyphen as U+0002 in this corpus
/// (U+00AD elsewhere), so `word\u{2} continuation` is one hyphenated word —
/// drop the hyphen + the joining space and merge (`com\u{2} pact` → `compact`,
/// `end-to\u{2} end` → `end-toend`), exactly as docling does.
///
/// Token spacing is otherwise left as the geometric join produced it. We do not
/// tighten punctuation spacing: docling preserves the PDF's own spaces (it keeps
/// `{ ahn }`, `Name 1 .`, `[ 9 ]`), and a geometric gap heuristic diverges from
/// it more than a plain single-space join does.
fn clean_text(text: &str) -> String {
    text.replace("\u{2} ", "")
        .replace("\u{ad} ", "")
        .replace(['\u{2}', '\u{ad}'], "") // any stray wrap hyphens not at a join
        .replace(['\u{2018}', '\u{2019}'], "'") // ‘ ’ → '
        .replace(['\u{201c}', '\u{201d}'], "\"") // “ ” → "
        .replace(['\u{2013}', '\u{2014}'], "-") // – — → -
        .replace('\u{2026}', "...") // … → ...
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Cells assigned to a region (best container), in reading order, joined.
fn region_text(region: &Region, cells: &[TextCell]) -> String {
    let mut inside: Vec<&TextCell> = cells
        .iter()
        .filter(|c| {
            let ca = area(c.l, c.t, c.r, c.b).max(1.0);
            inter(region, c.l, c.t, c.r, c.b) / ca > 0.5
        })
        .collect();
    // Quantize the top coordinate into ~line bands so cells on the same line
    // sort left-to-right; this is a strict total order (a raw fuzzy comparator
    // is not transitive and makes Rust's sort panic).
    let band = inside
        .iter()
        .map(|c| (c.b - c.t).abs())
        .fold(0.0f32, f32::max)
        .max(1.0);
    inside.sort_by_key(|c| ((c.t / band).round() as i64, (c.l * 10.0) as i64));
    let joined = inside
        .iter()
        .map(|c| c.text.trim())
        .collect::<Vec<_>>()
        .join(" ");
    clean_text(&joined)
}

/// Reconstruct a table's grid geometrically from the text cells inside its
/// region: cluster cells into rows (by vertical centre) and columns (by clustered
/// left edges), then place each cell. A model-free stand-in for TableFormer that
/// recovers grid-aligned tables from the precise PDF text layer (it does not
/// resolve row/column spans).
fn reconstruct_table(region: &Region, cells: &[TextCell]) -> Vec<Vec<String>> {
    let mut inside: Vec<&TextCell> = cells
        .iter()
        .filter(|c| {
            let ca = area(c.l, c.t, c.r, c.b).max(1.0);
            inter(region, c.l, c.t, c.r, c.b) / ca > 0.5
        })
        .collect();
    if inside.is_empty() {
        return Vec::new();
    }
    inside.sort_by(|a, b| a.t.total_cmp(&b.t));

    // Rows: consecutive cells whose vertical centre is within ~0.7 line height.
    let mut rows: Vec<(f32, Vec<&TextCell>)> = Vec::new();
    for c in &inside {
        let cyc = (c.t + c.b) / 2.0;
        let lh = (c.b - c.t).abs().max(1.0);
        if let Some((ryc, row)) = rows.last_mut() {
            if (cyc - *ryc).abs() < lh * 0.7 {
                row.push(c);
                continue;
            }
        }
        rows.push((cyc, vec![c]));
    }

    // Columns: cluster left edges (merge those within a tolerance).
    let tol = {
        let mut hs: Vec<f32> = inside.iter().map(|c| (c.b - c.t).abs()).collect();
        hs.sort_by(f32::total_cmp);
        hs[hs.len() / 2].max(4.0) * 1.5
    };
    let mut lefts: Vec<f32> = inside.iter().map(|c| c.l).collect();
    lefts.sort_by(f32::total_cmp);
    let mut col_starts: Vec<f32> = Vec::new();
    for l in lefts {
        if col_starts.last().is_none_or(|&last| l - last > tol) {
            col_starts.push(l);
        }
    }
    let ncols = col_starts.len().max(1);
    let col_of = |l: f32| -> usize {
        col_starts
            .iter()
            .rposition(|&s| l + tol * 0.5 >= s)
            .unwrap_or(0)
            .min(ncols - 1)
    };

    let mut grid = Vec::with_capacity(rows.len());
    for (_, mut row) in rows {
        row.sort_by(|a, b| a.l.total_cmp(&b.l));
        let mut cols = vec![String::new(); ncols];
        for c in row {
            let ci = col_of(c.l);
            // Strip the wrap-hyphen control char so it never lands in a cell.
            let t = c.text.trim().replace(['\u{2}', '\u{ad}'], "");
            if cols[ci].is_empty() {
                cols[ci] = t;
            } else {
                cols[ci].push(' ');
                cols[ci].push_str(&t);
            }
        }
        grid.push(cols);
    }
    grid
}

/// Crop a layout region from the rendered page image and encode it as PNG (the
/// figure bytes docling stores on a `PictureItem`). Region coordinates are page
/// points; the image is rendered at `page.scale`.
fn crop_region(page: &PdfPage, region: &Region) -> Option<PictureImage> {
    let s = page.scale;
    let (iw, ih) = (page.image.width(), page.image.height());
    let x = (region.l * s).max(0.0) as u32;
    let y = (region.t * s).max(0.0) as u32;
    if x >= iw || y >= ih {
        return None;
    }
    let w = (((region.r - region.l) * s) as u32).min(iw - x);
    let h = (((region.b - region.t) * s) as u32).min(ih - y);
    if w == 0 || h == 0 {
        return None;
    }
    let sub = image::imageops::crop_imm(&page.image, x, y, w, h).to_image();
    let mut buf = std::io::Cursor::new(Vec::new());
    sub.write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(PictureImage {
        mimetype: "image/png".into(),
        width: w,
        height: h,
        data: buf.into_inner(),
    })
}

/// For each `picture` region, find the `caption` region closest below it (and
/// horizontally overlapping); docling pairs them and emits the caption first.
/// Each caption is claimed by at most one picture.
fn pair_captions(regions: &[Region]) -> Vec<Option<usize>> {
    let mut pairs = vec![None; regions.len()];
    let mut taken = vec![false; regions.len()];
    for (pi, p) in regions.iter().enumerate() {
        if p.label != "picture" {
            continue;
        }
        let mut best: Option<(usize, f32)> = None;
        for (ci, c) in regions.iter().enumerate() {
            if c.label != "caption" || taken[ci] {
                continue;
            }
            let line_h = (c.b - c.t).abs().max(1.0);
            let gap = c.t - p.b; // caption sits below the picture
            let h_overlap = (p.r.min(c.r) - p.l.max(c.l)).max(0.0);
            if gap > -line_h && gap < line_h * 3.0 && h_overlap > 0.0 {
                let dist = gap.abs();
                if best.is_none_or(|(_, bd)| dist < bd) {
                    best = Some((ci, dist));
                }
            }
        }
        if let Some((ci, _)) = best {
            pairs[pi] = Some(ci);
            taken[ci] = true;
        }
    }
    pairs
}

/// Assemble one page from its (already overlap-resolved) layout regions and
/// text cells.
pub fn assemble_page(page: &PdfPage, mut regions: Vec<Region>, doc: &mut DoclingDocument) {
    order_regions(&mut regions, page.width);
    // docling emits a figure's caption *before* the image marker. Pair each
    // picture with the caption region nearest below it and consume that caption,
    // so it isn't also emitted in its own (lower) reading-order position.
    let caption_for = pair_captions(&regions);
    let mut consumed = vec![false; regions.len()];
    for ci in caption_for.iter().flatten() {
        consumed[*ci] = true;
    }

    for (i, region) in regions.iter().enumerate() {
        if is_skipped(region.label) || consumed[i] {
            continue;
        }
        if region.label == "picture" {
            // The figure pixels are cropped from the page render for image export.
            let caption = caption_for[i]
                .map(|ci| region_text(&regions[ci], &page.cells))
                .filter(|t| !t.is_empty());
            doc.push(Node::Picture {
                caption,
                image: crop_region(page, region),
            });
            continue;
        }
        let text = region_text(region, &page.cells);
        if text.is_empty() {
            continue;
        }
        match region.label {
            // docling renders both the document title and section headers as
            // `##` (it never emits a top-level `#` for PDFs), so match that.
            "title" | "section_header" => doc.push(Node::Heading { level: 2, text }),
            // docling drops the rendered bullet glyph; the Markdown serializer
            // adds its own `- ` marker.
            "list_item" => doc.push(Node::ListItem {
                ordered: false,
                number: 0,
                first_in_list: false,
                text: text
                    .trim_start_matches(['•', '◦', '▪', '·', '*', '-'])
                    .trim_start()
                    .to_string(),
                level: 0,
            }),
            // Geometric grid reconstruction from the text layer (TableFormer
            // would refine structure / spans). Falls back to a single cell.
            "table" => {
                let rows = reconstruct_table(region, &page.cells);
                let rows = if rows.iter().any(|r| r.len() > 1) {
                    rows
                } else {
                    vec![vec![text]]
                };
                doc.push(Node::Table(Table { rows }));
            }
            // docling does not decode formulas in the standard pipeline; it emits
            // a placeholder comment rather than the (garbled) raw glyph text.
            "formula" => doc.push(Node::Paragraph {
                text: "<!-- formula-not-decoded -->".into(),
            }),
            // text, caption, footnote, code → paragraph
            _ => doc.push(Node::Paragraph { text }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::clean_text;

    #[test]
    fn clean_text_dehyphenates_and_normalizes_typography() {
        // U+0002 line-wrap hyphen + the join space → merged word (like docling).
        assert_eq!(clean_text("com\u{2} pact"), "compact");
        assert_eq!(clean_text("end-to\u{2} end deep"), "end-toend deep");
        // A stray wrap hyphen (no following join) is dropped.
        assert_eq!(clean_text("word\u{2}"), "word");
        // Typographic punctuation → ASCII.
        assert_eq!(
            clean_text("Graph\u{2019}s \u{201c}x\u{201d}"),
            "Graph's \"x\""
        );
        assert_eq!(clean_text("a\u{2026}"), "a...");
        // Whitespace collapses; a normal space-join is preserved.
        assert_eq!(clean_text("a   b\nc"), "a b c");
    }
}
