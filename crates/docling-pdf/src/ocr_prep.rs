//! ONNX-free half of the PP-OCRv3 recognition pipeline: everything before
//! and after the `session.run` call — line segmentation, crop preparation,
//! width-batching, CTC decoding, dictionary handling.
//!
//! Split out of `ocr.rs` (which keeps the `ort` session) so the browser build
//! can reuse it (issue #79 phase 2): `docling-wasm` runs these exact
//! functions and delegates only the inference call to ONNX Runtime Web on
//! the JS side. Keeping one implementation is what makes the wasm output
//! byte-comparable to the native CPU path — any drift then comes from the
//! runtime, not from pre/post-processing.

use image::{imageops, imageops::FilterType, Rgb, RgbImage};

/// PP-OCRv3's fixed input height.
pub const REC_HEIGHT: u32 = 48;

/// Cap on lines per recognition run: bounds peak input-tensor memory
/// (16 × 3 × 48 × 2400 px ≈ 22 MB f32) without costing measurable batching
/// benefit — same-width groups are rarely larger.
pub const REC_BATCH: usize = 16;

/// A text-line crop prepared for recognition: resized to the fixed model
/// height, normalised to `[-1, 1]`, laid out CHW.
pub struct PrepLine {
    /// Width after the aspect-preserving resize to [`REC_HEIGHT`].
    pub w: usize,
    /// `3 * REC_HEIGHT * w` values.
    pub data: Vec<f32>,
}

/// Prepare one line crop, or `None` for a degenerate (zero-sized) crop.
pub fn prep_line(line: &RgbImage) -> Option<PrepLine> {
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

/// The CTC class table for a recognition dictionary file: index 0 = blank,
/// then one class per dictionary line, then the space class.
pub fn dict_chars(dict: &str) -> Vec<String> {
    let mut chars = vec![String::new()]; // blank at 0
    chars.extend(dict.lines().map(|s| s.to_string()));
    chars.push(" ".to_string());
    chars
}

/// Greedy CTC decode of one row's `(T, C)` probabilities.
pub fn decode_row(chars: &[String], probs: &[f32], nc: usize) -> String {
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

pub(crate) fn luma(p: &Rgb<u8>) -> f32 {
    0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32
}

/// Split a region crop into text lines via a horizontal ink-projection profile.
/// Returns tight `(l, t, r, b)` boxes in crop pixels.
pub fn segment_lines(crop: &RgbImage) -> Vec<(u32, u32, u32, u32)> {
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

/// Layout labels whose content is recognised as running text.
pub fn is_text_label(label: &str) -> bool {
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

/// A line's page-point bounding box, `(l, t, r, b)`.
pub type LineBox = (f32, f32, f32, f32);

/// Gather every text-region line crop on a page, in page order: crop each
/// text region (page points × `scale` → image px), split it into lines, prep
/// each line, and keep the line's page-point bbox. The exact gathering the
/// native `ocr_page` does — shared so the browser path produces the same
/// cells given the same probabilities.
pub fn prep_region_lines(
    img: &RgbImage,
    regions: &[crate::layout::Region],
    scale: f32,
) -> (Vec<LineBox>, Vec<PrepLine>) {
    let (iw, ih) = img.dimensions();
    let mut bboxes = Vec::new();
    let mut lines = Vec::new();
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
    (bboxes, lines)
}

/// Normalize an image to the scan polarity every stage assumes — dark ink
/// on light paper (the segmentation threshold and the recognition model's
/// training data both bake it in): a predominantly dark page (mean luma
/// below mid-gray — a dark-mode screenshot, an inverted scan) is inverted.
/// Browser-path helper; the native pipeline never calls it (its input is
/// scanned paper, and the conformance baseline stays untouched).
pub fn normalize_polarity(mut img: RgbImage) -> RgbImage {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return img;
    }
    let mean: f32 = img.pixels().map(luma).sum::<f32>() / (w * h) as f32;
    if mean < 128.0 {
        for px in img.pixels_mut() {
            px.0 = [255 - px.0[0], 255 - px.0[1], 255 - px.0[2]];
        }
    }
    img
}

/// Whole-image line preparation for the browser OCR path (no layout model:
/// the page itself is the single text region). Returns page-order prepared
/// lines; callers that need geometry use [`segment_lines`] directly.
pub fn prep_page_lines(img: &RgbImage) -> Vec<PrepLine> {
    segment_lines(img)
        .into_iter()
        .filter_map(|(l, t, r, b)| {
            let line = imageops::crop_imm(img, l, t, r - l, b - t).to_image();
            prep_line(&line)
        })
        .collect()
}

/// Deterministic recognition batching: page-order line indices grouped by
/// exact width (equal widths share a run — bit-identical to one-at-a-time
/// recognition, see `ocr.rs`), each group split into [`REC_BATCH`] chunks.
pub fn width_batches(lines: &[PrepLine]) -> Vec<(usize, Vec<usize>)> {
    let mut by_width: std::collections::BTreeMap<usize, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (ix, pl) in lines.iter().enumerate() {
        by_width.entry(pl.w).or_default().push(ix);
    }
    let mut out = Vec::new();
    for (w, ixs) in by_width {
        for chunk in ixs.chunks(REC_BATCH) {
            out.push((w, chunk.to_vec()));
        }
    }
    out
}

/// Pack one width-batch into the model's `(N, 3, H, W)` input buffer.
pub fn batch_input(w: usize, chunk: &[usize], lines: &[PrepLine]) -> Vec<f32> {
    let hw = REC_HEIGHT as usize * w;
    let mut data = vec![0f32; chunk.len() * 3 * hw];
    for (i, &ix) in chunk.iter().enumerate() {
        data[i * 3 * hw..(i + 1) * 3 * hw].copy_from_slice(&lines[ix].data);
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic "page": white background with two black text bars.
    fn page() -> RgbImage {
        let mut img = RgbImage::from_pixel(200, 100, Rgb([255, 255, 255]));
        for y in 20..30 {
            for x in 10..190 {
                img.put_pixel(x, y, Rgb([0, 0, 0]));
            }
        }
        for y in 60..72 {
            for x in 10..120 {
                img.put_pixel(x, y, Rgb([0, 0, 0]));
            }
        }
        img
    }

    #[test]
    fn segments_and_preps_page_lines() {
        let lines = prep_page_lines(&page());
        assert_eq!(lines.len(), 2);
        for pl in &lines {
            assert_eq!(pl.data.len(), 3 * REC_HEIGHT as usize * pl.w);
        }
        // Different aspect ratios → different widths → separate batches.
        let batches = width_batches(&lines);
        assert_eq!(batches.len(), 2);
        let (w0, chunk0) = &batches[0];
        assert_eq!(
            batch_input(*w0, chunk0, &lines).len(),
            3 * REC_HEIGHT as usize * w0
        );
    }

    #[test]
    fn dark_mode_pages_normalize_to_scan_polarity() {
        // The same two-bar page, inverted (light text on dark) — the raw
        // segmentation misfires (it thresholds the dark *background* as ink);
        // polarity normalization recovers the true structure.
        let mut dark = page();
        for px in dark.pixels_mut() {
            px.0 = [255 - px.0[0], 255 - px.0[1], 255 - px.0[2]];
        }
        assert_ne!(prep_page_lines(&dark).len(), 2);
        let fixed = normalize_polarity(dark);
        assert_eq!(prep_page_lines(&fixed).len(), 2);
        // A light page passes through untouched.
        let light = page();
        assert_eq!(normalize_polarity(light.clone()), light);
    }

    #[test]
    fn ctc_decode_collapses_repeats_and_blanks() {
        // 3 classes: blank, "a", "b"; timesteps a a blank b b → "ab".
        let chars = dict_chars("a\nb");
        assert_eq!(chars.len(), 4); // blank, a, b, space
        let probs = [
            0.1, 0.8, 0.1, 0.0, // a
            0.1, 0.8, 0.1, 0.0, // a (repeat collapses)
            0.9, 0.05, 0.05, 0.0, // blank
            0.1, 0.1, 0.8, 0.0, // b
            0.1, 0.1, 0.8, 0.0, // b (repeat collapses)
        ];
        assert_eq!(decode_row(&chars, &probs, 4), "ab");
    }
}
