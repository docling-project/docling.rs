//! Layout-driven assembly: map detected [`Region`]s + text cells to a
//! [`DoclingDocument`], mirroring docling's page-assembly + reading-order.
//!
//! Overlapping detections are resolved greedily by score, each text cell is
//! assigned to its best-containing region, regions are ordered in reading order
//! (two-column aware), and each becomes a typed node by its layout label.

use docling_crab_core::{DoclingDocument, Node, PictureImage, Table};

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
            ca.cmp(&cb).then(a.t.total_cmp(&b.t)).then(a.l.total_cmp(&b.l))
        });
    } else {
        regions.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.l.total_cmp(&b.l)));
    }
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
    inside
        .iter()
        .map(|c| c.text.trim())
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
            let t = c.text.trim();
            if cols[ci].is_empty() {
                cols[ci] = t.to_string();
            } else {
                cols[ci].push(' ');
                cols[ci].push_str(t);
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

/// Assemble one page from its (already overlap-resolved) layout regions and
/// text cells.
pub fn assemble_page(page: &PdfPage, mut regions: Vec<Region>, doc: &mut DoclingDocument) {
    order_regions(&mut regions, page.width);

    for region in &regions {
        if is_skipped(region.label) {
            continue;
        }
        if region.label == "picture" {
            // Caption text, if any, is emitted by its own `caption` region.
            // The figure pixels are cropped from the page render for image export.
            doc.push(Node::Picture {
                caption: None,
                image: crop_region(page, region),
            });
            continue;
        }
        let text = region_text(region, &page.cells);
        if text.is_empty() {
            continue;
        }
        match region.label {
            "title" => doc.push(Node::Heading { level: 1, text }),
            "section_header" => doc.push(Node::Heading { level: 2, text }),
            "list_item" => doc.push(Node::ListItem {
                ordered: false,
                number: 0,
                first_in_list: false,
                text,
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
            // text, caption, footnote, formula, code → paragraph
            _ => doc.push(Node::Paragraph { text }),
        }
    }
}
