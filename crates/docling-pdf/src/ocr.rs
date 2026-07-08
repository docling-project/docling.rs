//! OCR for scanned pages, via the PP-OCRv3 recognition model (CRNN/SVTR) run
//! with `ort`. The layout model already locates text regions on the page image
//! (it works without a text layer), so OCR only needs *recognition*: each text
//! region is cropped, split into lines by horizontal projection, and each line
//! is recognised and decoded with CTC — producing [`TextCell`]s the normal
//! layout assembly then consumes. This avoids a separate text-detection model.

use image::{imageops, imageops::FilterType, Rgb, RgbImage};
use ort::session::Session;
use ort::value::Tensor;

use crate::layout::Region;
use crate::pdfium_backend::TextCell;

const REC_HEIGHT: u32 = 48;

pub struct OcrModel {
    rec: Session,
    /// CTC classes: index 0 = blank, 1..=6623 = dictionary, 6624 = space.
    chars: Vec<String>,
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
        let rec = Session::builder()
            .map_err(|e| format!("ocr: builder: {e}"))?
            .with_intra_threads(1)
            .map_err(|e| format!("ocr: intra_threads: {e}"))?
            .commit_from_file(&rec_path)
            .map_err(|e| format!("ocr: load {rec_path}: {e}"))?;
        let dict = std::fs::read_to_string(&dict_path)
            .map_err(|e| format!("ocr: read dict {dict_path}: {e}"))?;
        let mut chars = vec![String::new()]; // blank at 0
        chars.extend(dict.lines().map(|s| s.to_string()));
        chars.push(" ".to_string());
        Ok(Self { rec, chars })
    }

    /// Recognise a single text-line image → string (CTC greedy decode).
    fn recognize(&mut self, line: &RgbImage) -> Result<String, String> {
        let (w, h) = line.dimensions();
        if w == 0 || h == 0 {
            return Ok(String::new());
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
        let input = Tensor::from_array(([1usize, 3, REC_HEIGHT as usize, new_w as usize], data))
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
        let mut out = String::new();
        let mut prev = 0usize;
        for t in 0..t_len {
            let base = t * nc;
            let mut best = 0usize;
            let mut bestv = probs[base];
            for c in 1..nc {
                if probs[base + c] > bestv {
                    bestv = probs[base + c];
                    best = c;
                }
            }
            if best != prev && best != 0 {
                if let Some(ch) = self.chars.get(best) {
                    out.push_str(ch);
                }
            }
            prev = best;
        }
        Ok(out)
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
        let mut cells = Vec::new();
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
                let text = self.recognize(&line)?.trim().to_string();
                if text.is_empty() {
                    continue;
                }
                cells.push(TextCell {
                    text,
                    l: (l + lx) as f32 / scale,
                    t: (t + ly) as f32 / scale,
                    r: (l + rx) as f32 / scale,
                    b: (t + ry) as f32 / scale,
                });
            }
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
