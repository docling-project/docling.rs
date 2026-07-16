//! OCR for scanned pages, via the PP-OCRv3 recognition model (CRNN/SVTR) run
//! with `ort`. The layout model already locates text regions on the page image
//! (it works without a text layer), so OCR only needs *recognition*: each text
//! region is cropped, split into lines by horizontal projection, and each line
//! is recognised and decoded with CTC — producing [`TextCell`]s the normal
//! layout assembly then consumes. This avoids a separate text-detection model.

use std::collections::BTreeMap;

use image::{imageops, imageops::FilterType, Rgb, RgbImage};
use ort::session::Session;
use ort::value::Tensor;

use crate::layout::Region;
use crate::pdfium_backend::TextCell;

const REC_HEIGHT: u32 = 48;

/// Cap on lines per recognition run: bounds peak input-tensor memory
/// (16 × 3 × 48 × 2400 px ≈ 22 MB f32) without costing measurable batching
/// benefit — same-width groups are rarely larger.
const REC_BATCH: usize = 16;

/// A text-line crop prepared for recognition: resized to the fixed model
/// height, normalised to `[-1, 1]`, laid out CHW.
struct PrepLine {
    /// Width after the aspect-preserving resize to `REC_HEIGHT`.
    w: usize,
    /// `3 * REC_HEIGHT * w` values.
    data: Vec<f32>,
}

/// Prepare one line crop, or `None` for a degenerate (zero-sized) crop.
fn prep_line(line: &RgbImage) -> Option<PrepLine> {
    let (w, h) = line.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let new_w = ((w as f32) * REC_HEIGHT as f32 / h as f32)
        .round()
        .clamp(8.0, 2400.0) as u32;
    let resized = imageops::resize(line, new_w, REC_HEIGHT, FilterType::Triangle);
    let n = (REC_HEIGHT * new_w) as usize;
    // Normalise to [-1, 1]: (x/255 - 0.5) / 0.5.
    let mut data = vec![0f32; 3 * n];
    for (i, px) in resized.pixels().enumerate() {
        data[i] = px[0] as f32 / 127.5 - 1.0;
        data[n + i] = px[1] as f32 / 127.5 - 1.0;
        data[2 * n + i] = px[2] as f32 / 127.5 - 1.0;
    }
    Some(PrepLine {
        w: new_w as usize,
        data,
    })
}

pub struct OcrModel {
    rec: Session,
    /// CTC classes: index 0 = blank, 1..=6623 = dictionary, 6624 = space.
    chars: Vec<String>,
}

/// Greedy CTC decode of one row's `(T, C)` probabilities.
fn decode_row(chars: &[String], probs: &[f32], nc: usize) -> String {
    let mut out = String::new();
    let mut prev = 0usize;
    for row in probs.chunks_exact(nc) {
        let mut best = 0usize;
        let mut bestv = row[0];
        for (c, &v) in row.iter().enumerate().skip(1) {
            if v > bestv {
                bestv = v;
                best = c;
            }
        }
        if best != prev && best != 0 {
            if let Some(ch) = chars.get(best) {
                out.push_str(ch);
            }
        }
        prev = best;
    }
    out
}

fn luma(p: &Rgb<u8>) -> f32 {
    0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32
}

/// Layout labels whose content is recognised as running text.
fn is_text_label(label: &str) -> bool {
    matches!(
        label,
        "text"
            | "title"
            | "section_header"
            | "list_item"
            | "caption"
            | "footnote"
            | "code"
            | "formula"
    )
}

impl OcrModel {
    /// Load the recognition model (`DOCLING_OCR_REC_ONNX` / `models/ocr_rec.onnx`)
    /// and its character dictionary (`DOCLING_OCR_DICT` / `models/ppocr_keys_v1.txt`).
    pub fn load() -> Result<Self, String> {
        let rec_path = std::env::var("DOCLING_OCR_REC_ONNX")
            .unwrap_or_else(|_| crate::resolve_asset("models/ocr_rec.onnx"));
        let dict_path = std::env::var("DOCLING_OCR_DICT")
            .unwrap_or_else(|_| crate::resolve_asset("models/ppocr_keys_v1.txt"));
        // Single-threaded: ORT's multi-threaded float-reduction order varies
        // across runs, which flips the CTC argmax on low-confidence characters
        // (e.g. noisy faxes) and makes the snapshot output non-deterministic. The
        // recognition inputs are tiny per-line crops, so the throughput cost is
        // negligible.
        let builder = Session::builder()
            .map_err(|e| format!("ocr: builder: {e}"))?
            .with_intra_threads(1)
            .map_err(|e| format!("ocr: intra_threads: {e}"))?;
        let rec = crate::ep::apply(builder)
            .map_err(|e| format!("ocr: {e}"))?
            .commit_from_file(&rec_path)
            .map_err(|e| format!("ocr: load {rec_path}: {e}"))?;
        let dict = std::fs::read_to_string(&dict_path)
            .map_err(|e| format!("ocr: read dict {dict_path}: {e}"))?;
        let mut chars = vec![String::new()]; // blank at 0
        chars.extend(dict.lines().map(|s| s.to_string()));
        chars.push(" ".to_string());
        Ok(Self { rec, chars })
    }

    /// Recognise a batch of prepared *same-width* lines in one session run.
    ///
    /// Only equal widths ever share a run: same-width batching is
    /// bit-identical to one-at-a-time recognition (each sample keeps its own
    /// data and per-sample kernel reduction order — verified empirically on
    /// the scanned corpus), whereas width-padding leaks into the real
    /// timesteps through the model's global-attention blocks and measurably
    /// changes low-confidence characters.
    fn recognize_batch(&mut self, w: usize, batch: &[&PrepLine]) -> Result<Vec<String>, String> {
        let n = batch.len();
        let hw = REC_HEIGHT as usize * w;
        let mut data = vec![0f32; n * 3 * hw];
        for (i, pl) in batch.iter().enumerate() {
            data[i * 3 * hw..(i + 1) * 3 * hw].copy_from_slice(&pl.data);
        }
        let input = Tensor::from_array(([n, 3, REC_HEIGHT as usize, w], data))
            .map_err(|e| format!("ocr: input tensor: {e}"))?;
        let outputs = self
            .rec
            .run(ort::inputs!["x" => input])
            .map_err(|e| format!("ocr: rec inference: {e}"))?;
        let (shape, probs) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("ocr: extract rec: {e}"))?;
        let t_len = shape[1] as usize;
        let nc = shape[2] as usize;
        Ok((0..n)
            .map(|i| {
                decode_row(
                    &self.chars,
                    &probs[i * t_len * nc..(i + 1) * t_len * nc],
                    nc,
                )
            })
            .collect())
    }

    /// OCR a page: produce text cells (page points) for every line found inside
    /// the text regions. `scale` is image-px per page-point.
    pub fn ocr_page(
        &mut self,
        img: &RgbImage,
        regions: &[Region],
        scale: f32,
    ) -> Result<Vec<TextCell>, String> {
        let (iw, ih) = img.dimensions();
        // Gather every line crop on the page first, so equal-width lines can
        // share a recognition run regardless of which region they came from.
        let mut bboxes: Vec<(f32, f32, f32, f32)> = Vec::new();
        let mut lines: Vec<PrepLine> = Vec::new();
        for region in regions {
            if !is_text_label(region.label) {
                continue;
            }
            let l = (region.l * scale).max(0.0) as u32;
            let t = (region.t * scale).max(0.0) as u32;
            let r = ((region.r * scale).max(0.0) as u32).min(iw);
            let b = ((region.b * scale).max(0.0) as u32).min(ih);
            if r <= l || b <= t {
                continue;
            }
            let crop = imageops::crop_imm(img, l, t, r - l, b - t).to_image();
            for (lx, ly, rx, ry) in segment_lines(&crop) {
                let line = imageops::crop_imm(&crop, lx, ly, rx - lx, ry - ly).to_image();
                let Some(pl) = prep_line(&line) else {
                    continue;
                };
                bboxes.push((
                    (l + lx) as f32 / scale,
                    (t + ly) as f32 / scale,
                    (l + rx) as f32 / scale,
                    (t + ry) as f32 / scale,
                ));
                lines.push(pl);
            }
        }

        // Group page-order line indices by exact width (BTreeMap: run order is
        // deterministic) and recognise each group batched.
        let mut by_width: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (ix, pl) in lines.iter().enumerate() {
            by_width.entry(pl.w).or_default().push(ix);
        }
        let mut texts = vec![String::new(); lines.len()];
        for (w, ixs) in by_width {
            for chunk in ixs.chunks(REC_BATCH) {
                let batch: Vec<&PrepLine> = chunk.iter().map(|&i| &lines[i]).collect();
                for (&i, text) in chunk.iter().zip(self.recognize_batch(w, &batch)?) {
                    texts[i] = text;
                }
            }
        }

        // Emit cells in page order, exactly as the sequential walk did.
        let mut cells = Vec::new();
        for ((l, t, r, b), text) in bboxes.into_iter().zip(texts) {
            let text = text.trim().to_string();
            if text.is_empty() {
                continue;
            }
            cells.push(TextCell { text, l, t, r, b });
        }
        Ok(cells)
    }
}

/// Split a region crop into text lines via a horizontal ink-projection profile.
/// Returns tight `(l, t, r, b)` boxes in crop pixels.
fn segment_lines(crop: &RgbImage) -> Vec<(u32, u32, u32, u32)> {
    let (w, h) = crop.dimensions();
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let mean: f32 = crop.pixels().map(luma).sum::<f32>() / (w * h) as f32;
    let thresh = mean * 0.7; // ink = noticeably darker than the page average
    let min_ink = ((w as f32) * 0.005).max(1.0) as u32;

    let mut profile = vec![0u32; h as usize];
    for y in 0..h {
        let mut row = 0u32;
        for x in 0..w {
            if luma(crop.get_pixel(x, y)) < thresh {
                row += 1;
            }
        }
        profile[y as usize] = row;
    }

    // Maximal runs of text rows, separated by (near-)blank rows.
    let mut runs: Vec<(u32, u32)> = Vec::new();
    let mut start: Option<u32> = None;
    for y in 0..h {
        let text = profile[y as usize] >= min_ink;
        if text && start.is_none() {
            start = Some(y);
        } else if !text {
            if let Some(s) = start.take() {
                if y - s >= 4 {
                    runs.push((s, y));
                }
            }
        }
    }
    if let Some(s) = start {
        if h - s >= 4 {
            runs.push((s, h));
        }
    }

    // Tighten each line to its horizontal ink bounds.
    runs.into_iter()
        .map(|(t, b)| {
            let (mut l, mut r) = (w, 0u32);
            for y in t..b {
                for x in 0..w {
                    if luma(crop.get_pixel(x, y)) < thresh {
                        l = l.min(x);
                        r = r.max(x + 1);
                    }
                }
            }
            if l >= r {
                (0, t, w, b)
            } else {
                (l, t, r, b)
            }
        })
        .collect()
}
