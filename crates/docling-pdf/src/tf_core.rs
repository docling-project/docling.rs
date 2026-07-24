//! ONNX-free half of the TableFormer pipeline: the 448 encoder-input
//! preprocessing, the autoregressive loop's structure corrections and bbox
//! bookkeeping, span merging, and the OTSL→grid layout. Everything here is pure
//! Rust (no `ort`), so the browser build (#157 stage 3) runs the *same*
//! conformance-critical logic the native pipeline does and delegates only the
//! three ONNX graphs (encoder / decoder / bbox) to ONNX Runtime Web. Keeping
//! one implementation is what keeps the wasm table structure identical to the
//! native CPU path; drift can only come from the runtime kernels.
//!
//! `tableformer.rs` (native, `ml` feature) owns the `ort` sessions and the
//! owned-value KV-cache fast path; it calls into here for the parts that don't
//! touch the runtime.

use image::RgbImage;

/// The encoder's fixed square input side.
pub const SIDE: u32 = 448;
// Verbatim from docling's tm_config.json image_normalization (more digits than
// f32 holds; kept exact for provenance).
#[allow(clippy::excessive_precision)]
pub const MEAN: [f32; 3] = [0.94247851, 0.94254675, 0.94292611];
#[allow(clippy::excessive_precision)]
pub const STD: [f32; 3] = [0.17910956, 0.17940403, 0.17931663];
/// Cap on decode steps (docling's generation limit).
pub const MAX_STEPS: usize = 1024;
/// The decoder hidden width, and the bbox decoder's per-cell `tag_h` stride.
pub const EMBED_DIM: usize = 512;

/// OTSL structure tokens (TableModel04_rs wordmap indices).
pub const START: i64 = 2;
pub const END: i64 = 3;
pub const ECEL: i64 = 4; // empty cell
pub const FCEL: i64 = 5; // full (content) cell
pub const LCEL: i64 = 6; // left-looking: extends the cell to its left (colspan)
pub const UCEL: i64 = 7; // up-looking: extends the cell above (rowspan)
pub const XCEL: i64 = 8; // cross: spans both ways
pub const NL: i64 = 9; // new row
pub const CHED: i64 = 10; // column header
pub const RHED: i64 = 11; // row header
pub const SROW: i64 = 12; // section row

const CELL_TAGS: [i64; 6] = [FCEL, ECEL, XCEL, CHED, RHED, SROW];

/// A predicted table cell: an OTSL grid position (with spans) + its box in the
/// 448 image normalized cxcywh, the OTSL tag, and the bbox decoder's cell
/// class (docling's `cell_class`; 2 = full, ≤1 = predicted empty).
#[derive(Debug, Clone)]
pub struct TableCell {
    pub row: usize,
    pub col: usize,
    pub colspan: usize,
    pub rowspan: usize,
    pub tag: i64,
    pub class: i64,
    pub cx: f32,
    pub cy: f32,
    pub w: f32,
    pub h: f32,
}

/// Resize `img` to `SIDE×SIDE` (bilinear, aligned to docling's half-pixel
/// centers) and normalize, laid out `(C, W, H)` as the exported encoder expects
/// — the raw `[1,3,SIDE,SIDE]` float buffer. The native path wraps this in an
/// `ort` tensor; the browser path hands it to ONNX Runtime Web directly.
pub fn preprocess_input(img: &RgbImage) -> Vec<f32> {
    let nn = (SIDE * SIDE) as usize;
    let side = SIDE as usize;
    let (sw, sh) = (img.width() as i32, img.height() as i32);
    let sxr = sw as f32 / SIDE as f32;
    let syr = sh as f32 / SIDE as f32;
    let mut data = vec![0f32; 3 * nn];
    for h in 0..side {
        let fy = (h as f32 + 0.5) * syr - 0.5;
        let wy = fy - fy.floor();
        let y0c = (fy.floor() as i32).clamp(0, sh - 1) as u32;
        let y1c = (fy.floor() as i32 + 1).clamp(0, sh - 1) as u32;
        for w in 0..side {
            let fx = (w as f32 + 0.5) * sxr - 0.5;
            let wx = fx - fx.floor();
            let x0c = (fx.floor() as i32).clamp(0, sw - 1) as u32;
            let x1c = (fx.floor() as i32 + 1).clamp(0, sw - 1) as u32;
            let p00 = img.get_pixel(x0c, y0c);
            let p01 = img.get_pixel(x1c, y0c);
            let p10 = img.get_pixel(x0c, y1c);
            let p11 = img.get_pixel(x1c, y1c);
            let idx = w * side + h; // (C, W, H): c*n + w*H + h
            for c in 0..3 {
                let top = p00[c] as f32 * (1.0 - wx) + p01[c] as f32 * wx;
                let bot = p10[c] as f32 * (1.0 - wx) + p11[c] as f32 * wx;
                let v = top * (1.0 - wy) + bot * wy;
                data[c * nn + idx] = (v / 255.0 - MEAN[c]) / STD[c];
            }
        }
    }
    data
}

/// docling's two structure corrections, applied to a raw argmax tag: `xcel`
/// collapses to `lcel` (its `line_num` is never incremented, so this fires on
/// every row), and an `lcel` right after a `ucel` becomes a full cell.
pub fn correct(raw: i64, prev_ucel: bool) -> i64 {
    let mut tag = raw;
    if tag == XCEL {
        tag = LCEL;
    }
    if prev_ucel && tag == LCEL {
        tag = FCEL;
    }
    tag
}

/// The autoregressive loop's per-step state, mirroring docling's `predict`
/// bookkeeping (`tag_H_buf` / `bboxes_to_merge`): which decoder hidden states
/// feed the bbox decoder and how horizontal spans merge. Both the native and
/// browser loops step the decoder themselves (sync vs async `ort`) and feed
/// each result through [`step`](Self::step); everything else stays here so the
/// two paths can't drift.
#[derive(Default)]
pub struct BboxBook {
    /// The decoder input prefix (`[START]`, then every emitted tag).
    pub tags: Vec<i64>,
    /// The emitted OTSL structure tokens (no `START`/`END`).
    pub otsl: Vec<i64>,
    /// Per-bbox-cell decoder hidden states, flattened `[n, EMBED_DIM]`.
    pub hiddens: Vec<f32>,
    /// Number of hidden states collected (`hiddens.len() / EMBED_DIM`).
    pub n: usize,
    /// Span merges: `cur_bbox_ind → partner` (`-1` → the last box).
    pub merge: std::collections::HashMap<usize, i64>,
    prev_ucel: bool,
    skip: bool,
    first_lcel: bool,
    bbox_ind: usize,
    cur_bbox_ind: usize,
}

impl BboxBook {
    pub fn new() -> Self {
        Self {
            tags: vec![START],
            skip: true, // first tag after <start> is skipped
            first_lcel: true,
            ..Default::default()
        }
    }

    /// Feed one raw decoded tag and its hidden state. Returns `false` when the
    /// corrected tag is `END` (stop decoding) — the tag is not recorded then.
    pub fn step(&mut self, raw: i64, hidden: &[f32]) -> bool {
        let tag = correct(raw, self.prev_ucel);
        if tag == END {
            return false;
        }
        // docling's tag_H_buf / bboxes_to_merge bookkeeping.
        if !self.skip && matches!(tag, FCEL | ECEL | CHED | RHED | SROW | NL | UCEL) {
            self.hiddens.extend_from_slice(hidden);
            self.n += 1;
            if !self.first_lcel {
                self.merge.insert(self.cur_bbox_ind, self.bbox_ind as i64);
            }
            self.bbox_ind += 1;
        }
        if tag != LCEL {
            self.first_lcel = true;
        } else if self.first_lcel {
            self.hiddens.extend_from_slice(hidden);
            self.n += 1;
            self.first_lcel = false;
            self.cur_bbox_ind = self.bbox_ind;
            self.merge.insert(self.cur_bbox_ind, -1);
            self.bbox_ind += 1;
        }
        self.skip = matches!(tag, NL | UCEL | XCEL);
        self.prev_ucel = tag == UCEL;
        self.otsl.push(tag);
        self.tags.push(tag);
        true
    }
}

/// docling's `mergebboxes` (cxcywh): the union box of a horizontal span's first
/// and last cell.
fn mergebboxes(b1: [f32; 4], b2: [f32; 4]) -> [f32; 4] {
    let new_w = (b2[0] + b2[2] / 2.0) - (b1[0] - b1[2] / 2.0);
    let new_h = (b2[1] + b2[3] / 2.0) - (b1[1] - b1[3] / 2.0);
    let new_left = b1[0] - b1[2] / 2.0;
    let new_top = (b2[1] - b2[3] / 2.0).min(b1[1] - b1[3] / 2.0);
    [new_left + new_w / 2.0, new_top + new_h / 2.0, new_w, new_h]
}

/// Apply docling's span merges: each merge key combines its box with the partner
/// (`-1` → the last box); partners are dropped. The merged cell keeps the
/// *first* box's class, matching docling's `outputs_class1.append(cls1)`.
pub fn merge_spans(
    boxes: &[[f32; 4]],
    classes: &[i64],
    merge: &std::collections::HashMap<usize, i64>,
) -> (Vec<[f32; 4]>, Vec<i64>) {
    let skip: std::collections::HashSet<usize> = merge
        .values()
        .filter(|&&v| v >= 0)
        .map(|&v| v as usize)
        .collect();
    let mut out = Vec::new();
    let mut out_classes = Vec::new();
    for (i, &b) in boxes.iter().enumerate() {
        let class = classes.get(i).copied().unwrap_or(2);
        if let Some(&j) = merge.get(&i) {
            let partner = if j < 0 { boxes.len() - 1 } else { j as usize };
            out.push(mergebboxes(b, boxes[partner.min(boxes.len() - 1)]));
            out_classes.push(class);
        } else if !skip.contains(&i) {
            out.push(b);
            out_classes.push(class);
        }
    }
    (out, out_classes)
}

/// Lay the OTSL tag stream onto a grid (docling's `_build_table_cells`, OTSL
/// mode): cell tags create cells at (row, col); `lcel`/`ucel`/`xcel` are spans
/// (counted toward the column index but not cells). Colspan/rowspan are read off
/// the grid (consecutive `lcel`/`ucel` to the right/below). `boxes` are indexed
/// by cell order and aligned with the cells.
pub fn build_table_cells(otsl: &[i64], boxes: &[[f32; 4]], classes: &[i64]) -> Vec<TableCell> {
    // 2D grid of tags (rows split on NL) for span lookups.
    let mut grid: Vec<Vec<i64>> = vec![Vec::new()];
    for &t in otsl {
        if t == NL {
            grid.push(Vec::new());
        } else {
            grid.last_mut().unwrap().push(t);
        }
    }
    let mut cells = Vec::new();
    let mut cell_id = 0usize;
    for (r, row) in grid.iter().enumerate() {
        for (c, &tag) in row.iter().enumerate() {
            if !CELL_TAGS.contains(&tag) {
                continue;
            }
            let mut colspan = 1;
            while c + colspan < row.len() && matches!(row[c + colspan], LCEL | XCEL) {
                colspan += 1;
            }
            let mut rowspan = 1;
            while r + rowspan < grid.len()
                && grid[r + rowspan]
                    .get(c)
                    .is_some_and(|&t| matches!(t, UCEL | XCEL))
            {
                rowspan += 1;
            }
            let b = boxes.get(cell_id).copied().unwrap_or([0.0; 4]);
            // docling defaults a class-less cell to 2 (full).
            let class = classes.get(cell_id).copied().unwrap_or(2);
            cells.push(TableCell {
                row: r,
                col: c,
                colspan,
                rowspan,
                tag,
                class,
                cx: b[0],
                cy: b[1],
                w: b[2],
                h: b[3],
            });
            cell_id += 1;
        }
    }
    cells
}

/// Index of the maximum. Uses Rust's `max_by` (ties resolve to the *last*
/// index; the decoder/bbox float logits don't produce exact ties in practice).
/// Kept verbatim from the native path so the two stay bit-identical.
pub fn argmax(v: &[f32]) -> usize {
    v.iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, _)| i)
        .unwrap_or(0)
}

use crate::pdfium_backend::TextCell;
use crate::tf_match::{PdfWord, TfCell};

/// The ONNX-free tail of TableFormer row prediction: match the page's word
/// cells into the predicted structure `cells` and expand spans into a dense
/// `rows × cols` text grid. `region` is `(l, t, r, b)` in page points; `cells`
/// are in the 448 image (normalized cxcywh). Shared by the native pipeline
/// (after `predict_table_structure`) and the browser path (after the ort-web
/// decode loop) — identical from here on. `None` when nothing matched.
pub fn table_rows(
    cells: &[TableCell],
    region: [f32; 4],
    words: &[TextCell],
) -> Option<Vec<Vec<String>>> {
    // Words that belong to the table: non-empty text, ≥80 % of the word's area
    // inside the table region (docling's `get_cells_in_bbox` ios test). Ids stay
    // the page-level word indices so text joins in stream order.
    let table_words: Vec<PdfWord> = words
        .iter()
        .enumerate()
        .filter(|(_, w)| !w.text.trim().is_empty())
        .filter_map(|(wi, w)| {
            let (l, t, r, b) = (w.l as f64, w.t as f64, w.r as f64, w.b as f64);
            let area = (r - l) * (b - t);
            let iw = (r.min(region[2] as f64) - l.max(region[0] as f64)).max(0.0);
            let ih = (b.min(region[3] as f64) - t.max(region[1] as f64)).max(0.0);
            if area > 0.0 && iw * ih / area > 0.8 {
                Some(PdfWord {
                    id: wi,
                    bbox: [l, t, r, b],
                    text: w.text.trim().to_string(),
                })
            } else {
                None
            }
        })
        .collect();

    if !table_words.is_empty() && !simple_match() {
        return docling_match_rows(cells, region, &table_words, words);
    }

    let (rw, rh) = (region[2] - region[0], region[3] - region[1]);

    // Cell boxes in page points (top-left), aligned with `cells`.
    let boxes: Vec<[f32; 4]> = cells
        .iter()
        .map(|c| {
            [
                region[0] + (c.cx - c.w / 2.0) * rw,
                region[1] + (c.cy - c.h / 2.0) * rh,
                region[0] + (c.cx + c.w / 2.0) * rw,
                region[1] + (c.cy + c.h / 2.0) * rh,
            ]
        })
        .collect();

    // Assign each word to the cell it overlaps most (intersection / word area).
    let mut cell_words: Vec<Vec<usize>> = vec![Vec::new(); cells.len()];
    for (wi, w) in words.iter().enumerate() {
        let wa = ((w.r - w.l) * (w.b - w.t)).max(1.0);
        let mut best: Option<(f32, usize)> = None;
        for (ci, b) in boxes.iter().enumerate() {
            let ix = (w.r.min(b[2]) - w.l.max(b[0])).max(0.0);
            let iy = (w.b.min(b[3]) - w.t.max(b[1])).max(0.0);
            let io = ix * iy / wa;
            if io > 0.0 && best.is_none_or(|(bo, _)| io > bo) {
                best = Some((io, ci));
            }
        }
        if let Some((_, ci)) = best {
            cell_words[ci].push(wi);
        }
    }

    let num_rows = cells.iter().map(|c| c.row + c.rowspan).max().unwrap_or(0);
    let num_cols = cells.iter().map(|c| c.col + c.colspan).max().unwrap_or(0);
    if num_rows == 0 || num_cols == 0 {
        return None;
    }
    let mut grid = vec![vec![String::new(); num_cols]; num_rows];
    for (ci, c) in cells.iter().enumerate() {
        // Keep words in text-stream order (their word index), matching docling's
        // cell text assembly — geometric re-sorting scrambles wrapped cells.
        let wis = std::mem::take(&mut cell_words[ci]);
        let text = wis
            .iter()
            .map(|&i| words[i].text.trim())
            .collect::<Vec<_>>()
            .join(" ");
        let text = normalize_cell_text(text);
        // Spanned cells repeat their text across the covered grid positions.
        for row in grid.iter_mut().skip(c.row).take(c.rowspan) {
            for cell in row.iter_mut().skip(c.col).take(c.colspan) {
                *cell = text.clone();
            }
        }
    }
    Some(grid)
}

/// `DOCLING_RS_TF_SIMPLE_MATCH=1` reverts to the pre-#60 best-overlap word
/// assignment (A/B escape hatch for the docling matching post-processor).
fn simple_match() -> bool {
    std::env::var("DOCLING_RS_TF_SIMPLE_MATCH").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// docling glues `@` to whatever follows it (`mAP @0.5`, an email): the PDF's
/// word cells split `@` from the next token, and joining with a space would
/// widen the cell and — via the column pad — shift every row. The groundtruth
/// never contains "@ ", so this is always the right normalization.
fn normalize_cell_text(text: String) -> String {
    text.replace("@ ", "@")
}

/// docling's matched-cell grid assembly (`tf_predictor.predict` with
/// `do_cell_matching=True`): run the ported matching post-processor, group the
/// word→cell assignments per grid position, compress the surviving row/column
/// ids to sequential indexes, and expand spans into a dense `rows × cols` text
/// grid. Matching runs in docling's coordinate space — the table bbox rounded
/// to integers, everything ×2 (its page scale) — so the post-processor's
/// absolute rounding agrees.
fn docling_match_rows(
    cells: &[TableCell],
    region: [f32; 4],
    table_words: &[PdfWord],
    words: &[TextCell],
) -> Option<Vec<Vec<String>>> {
    const SCALE: f64 = 2.0; // docling's table-structure page scale
    let sl = (region[0] as f64).round_ties_even() * SCALE;
    let st = (region[1] as f64).round_ties_even() * SCALE;
    let sr = (region[2] as f64).round_ties_even() * SCALE;
    let sb = (region[3] as f64).round_ties_even() * SCALE;
    let (w2, h2) = (sr - sl, sb - st);

    let tf_cells: Vec<TfCell> = cells
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let (cx, cy) = (c.cx as f64, c.cy as f64);
            let (w, h) = (c.w as f64, c.h as f64);
            TfCell {
                bbox: [
                    sl + (cx - w / 2.0) * w2,
                    st + (cy - h / 2.0) * h2,
                    sl + (cx + w / 2.0) * w2,
                    st + (cy + h / 2.0) * h2,
                ],
                cell_id: i,
                row_id: c.row,
                column_id: c.col,
                cell_class: c.class,
                colspan_val: if c.colspan > 1 { c.colspan } else { 0 },
                rowspan_val: if c.rowspan > 1 { c.rowspan } else { 0 },
            }
        })
        .collect();

    let scaled_words: Vec<PdfWord> = table_words
        .iter()
        .map(|w| PdfWord {
            id: w.id,
            bbox: [
                w.bbox[0] * SCALE,
                w.bbox[1] * SCALE,
                w.bbox[2] * SCALE,
                w.bbox[3] * SCALE,
            ],
            text: w.text.clone(),
        })
        .collect();

    // Debug (native only): dump the matcher inputs as JSON lines for a
    // side-by-side run against docling's Python post-processor.
    #[cfg(feature = "ml")]
    if let Ok(dir) = std::env::var("DOCLING_RS_TF_MATCH_DUMP") {
        if !dir.is_empty() {
            dump_match_inputs(&dir, &tf_cells, &scaled_words);
        }
    }

    let (cells_wo, final_matches) =
        crate::tf_match::match_and_post_process(tf_cells, &scaled_words);

    // `_merge_tf_output`: group per (column, row) in ascending-pdf-id order; the
    // first word's table cell fixes the group's offsets and spans.
    struct Merged {
        start_row: usize,
        start_col: usize,
        row_span: usize,
        col_span: usize,
        word_ids: Vec<usize>,
    }
    let mut merged: Vec<Merged> = Vec::new();
    let mut key_ix: std::collections::HashMap<(usize, usize), usize> =
        std::collections::HashMap::new();
    for (&pdf_id, list) in &final_matches {
        let tm = list[0].table_cell_id;
        let Some(cell) = cells_wo.iter().find(|c| c.cell_id == tm) else {
            continue;
        };
        match key_ix.entry((cell.column_id, cell.row_id)) {
            std::collections::hash_map::Entry::Occupied(e) => {
                merged[*e.get()].word_ids.push(pdf_id);
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(merged.len());
                merged.push(Merged {
                    start_row: cell.row_id,
                    start_col: cell.column_id,
                    row_span: cell.rowspan_val.max(1),
                    col_span: cell.colspan_val.max(1),
                    word_ids: vec![pdf_id],
                });
            }
        }
    }
    if merged.is_empty() {
        return None;
    }

    // `multi_table_predict`'s sort_row_col_indexes: compress the surviving
    // row/column ids to gap-free indexes.
    let mut start_cols: Vec<usize> = merged.iter().map(|m| m.start_col).collect();
    start_cols.sort_unstable();
    start_cols.dedup();
    let mut start_rows: Vec<usize> = merged.iter().map(|m| m.start_row).collect();
    start_rows.sort_unstable();
    start_rows.dedup();
    let mut num_rows = 0;
    let mut num_cols = 0;
    for m in &mut merged {
        m.start_col = start_cols.binary_search(&m.start_col).expect("own value");
        m.start_row = start_rows.binary_search(&m.start_row).expect("own value");
        num_cols = num_cols.max(m.start_col + m.col_span);
        num_rows = num_rows.max(m.start_row + m.row_span);
    }
    if num_rows == 0 || num_cols == 0 {
        return None;
    }

    let mut grid = vec![vec![String::new(); num_cols]; num_rows];
    for m in &merged {
        let text = m
            .word_ids
            .iter()
            .map(|&i| words[i].text.trim())
            .collect::<Vec<_>>()
            .join(" ");
        let text = normalize_cell_text(text);
        for row in grid.iter_mut().skip(m.start_row).take(m.row_span) {
            for cell in row.iter_mut().skip(m.start_col).take(m.col_span) {
                *cell = text.clone();
            }
        }
    }
    Some(grid)
}

/// Append one JSON line per table into `<dir>/tf_match_dump.jsonl` with the
/// exact matcher inputs (hand-rolled JSON to avoid a serde dependency).
#[cfg(feature = "ml")]
fn dump_match_inputs(dir: &str, tf_cells: &[TfCell], words: &[PdfWord]) {
    use std::io::Write;
    let cells: Vec<String> = tf_cells
        .iter()
        .map(|c| {
            format!(
                r#"{{"bbox":[{},{},{},{}],"cell_id":{},"row_id":{},"column_id":{},"cell_class":{},"colspan_val":{},"rowspan_val":{}}}"#,
                c.bbox[0], c.bbox[1], c.bbox[2], c.bbox[3],
                c.cell_id, c.row_id, c.column_id, c.cell_class,
                c.colspan_val, c.rowspan_val
            )
        })
        .collect();
    let ws: Vec<String> = words
        .iter()
        .map(|w| {
            format!(
                r#"{{"id":{},"bbox":[{},{},{},{}],"text":{}}}"#,
                w.id,
                w.bbox[0],
                w.bbox[1],
                w.bbox[2],
                w.bbox[3],
                serde_json_escape(&w.text)
            )
        })
        .collect();
    let line = format!(
        r#"{{"table_cells":[{}],"pdf_cells":[{}]}}"#,
        cells.join(","),
        ws.join(",")
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{dir}/tf_match_dump.jsonl"))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Minimal JSON string escaping for the parity dump.
#[cfg(feature = "ml")]
fn serde_json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrections() {
        assert_eq!(correct(XCEL, false), LCEL); // xcel → lcel
        assert_eq!(correct(LCEL, true), FCEL); // lcel after ucel → fcel
        assert_eq!(correct(XCEL, true), FCEL); // xcel → lcel → fcel
        assert_eq!(correct(FCEL, false), FCEL);
        assert_eq!(correct(LCEL, false), LCEL);
    }

    #[test]
    fn argmax_behaviour() {
        assert_eq!(argmax(&[0.1, 0.9, 0.3]), 1);
        assert_eq!(argmax(&[0.5, 0.5]), 1); // Rust max_by ties to the last index
        assert_eq!(argmax(&[]), 0);
    }

    #[test]
    fn book_skips_first_and_collects_hiddens() {
        // <start> then a 2x1 row: FCEL FCEL NL, END. The first FCEL after start
        // is NOT skipped (skip only guards the tag right after start-consumed
        // rows); verify hidden collection count and merge stays empty.
        let mut b = BboxBook::new();
        let h = [1.0f32; EMBED_DIM];
        assert!(b.step(FCEL, &h)); // skip=true initially → not collected
        assert!(b.step(FCEL, &h));
        assert!(b.step(NL, &h));
        assert!(!b.step(END, &h)); // stop
        assert_eq!(b.otsl, vec![FCEL, FCEL, NL]);
        // first FCEL skipped (skip=true), second FCEL + NL collected → n=2
        assert_eq!(b.n, 2);
        assert_eq!(b.hiddens.len(), 2 * EMBED_DIM);
        assert!(b.merge.is_empty());
    }

    #[test]
    fn book_merges_horizontal_span() {
        // FCEL LCEL: the LCEL is the first-lcel of a horizontal span → records a
        // merge partner (-1 placeholder) for the span's leading cell.
        let mut b = BboxBook::new();
        let h = [0.0f32; EMBED_DIM];
        b.step(FCEL, &h); // skipped (skip=true)
        b.step(FCEL, &h); // collected, bbox_ind 0→1
        b.step(LCEL, &h); // first-lcel: cur=1, merge{1:-1}, bbox_ind 1→2
        assert_eq!(b.merge.get(&1), Some(&-1));
    }

    #[test]
    fn build_cells_spans() {
        // Row 0: FCEL LCEL  (a 1x2 colspan)
        // Row 1: FCEL ECEL
        let otsl = vec![FCEL, LCEL, NL, FCEL, ECEL];
        let boxes = vec![[0.0; 4]; 3];
        let classes = vec![2, 2, 2];
        let cells = build_table_cells(&otsl, &boxes, &classes);
        assert_eq!(cells.len(), 3);
        assert_eq!((cells[0].colspan, cells[0].rowspan), (2, 1));
        assert_eq!((cells[0].row, cells[0].col), (0, 0));
        assert_eq!((cells[1].row, cells[1].col), (1, 0));
    }
}
