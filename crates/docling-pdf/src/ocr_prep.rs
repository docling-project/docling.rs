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

/// Cap on the recognizer input width — a sanity bound only (#981). RapidOCR
/// resizes every crop to `48 × ceil(48 · w/h)` with no cap, and the CTC
/// recognizer reads any width; the 2400 px (50:1) cap this held before
/// squeezed every longer line horizontally — a 700 px full-width line of
/// 12 px print at 800 px page width, which the detector boxes tighter than
/// the projection strips — and the recognizer then dropped letters and
/// glued words along the whole line (`Baccalaatpoesne accurlatci usgers`)
/// while short lines on the same page read fine. 16384 (341:1) is past any
/// text line a page can carry at the render sizes allowed.
pub const REC_MAX_WIDTH: u32 = 16384;

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
        .clamp(8.0, REC_MAX_WIDTH as f32) as u32;
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
    decode_row_scored(chars, probs, nc).0
}

/// [`decode_row`] plus the line's recognition confidence: the mean probability
/// of the emitted characters (PaddleOCR's per-line score — what docling's
/// `TextCell.confidence` carries for OCR cells, feeding the `ocr_score`
/// confidence aggregate, #183). `0.0` when nothing decodes.
pub fn decode_row_scored(chars: &[String], probs: &[f32], nc: usize) -> (String, f32) {
    let mut out = String::new();
    let mut prev = 0usize;
    let mut conf_sum = 0.0f32;
    let mut conf_n = 0usize;
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
                conf_sum += bestv;
                conf_n += 1;
            }
        }
        prev = best;
    }
    let conf = if conf_n == 0 {
        0.0
    } else {
        conf_sum / conf_n as f32
    };
    (out, conf)
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

    // A column inked in nearly every row is a vertical rule — a panel border, a
    // table line — not text; left in the profile it bridges every inter-line
    // gap and the whole crop collapses into one giant "line" (a framed
    // terms-and-conditions box OCR'd as a single unreadable strip). Mask those
    // columns out of the row profile; glyph columns never come close (a
    // descender-to-ascender stack is still far from 90 % of the crop height).
    let mut col_ink = vec![0u32; w as usize];
    for y in 0..h {
        for x in 0..w {
            if luma(crop.get_pixel(x, y)) < thresh {
                col_ink[x as usize] += 1;
            }
        }
    }
    // Borders are *thin*: when "always-inked" columns make up a noticeable
    // share of the width it is not a frame but a polarity/threshold artifact
    // (an inverted dark-mode page classifies its whole background as ink) —
    // leave the profile alone and let polarity normalization handle it.
    let rule_cols = col_ink
        .iter()
        .filter(|&&c| c as f32 > 0.9 * h as f32)
        .count();
    let mask_rules = (rule_cols as f32) < 0.15 * w as f32;
    let rule = |x: u32| mask_rules && col_ink[x as usize] as f32 > 0.9 * h as f32;

    let mut profile = vec![0u32; h as usize];
    for y in 0..h {
        let mut row = 0u32;
        for x in 0..w {
            if !rule(x) && luma(crop.get_pixel(x, y)) < thresh {
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

/// [`prep_region_lines`] with the text detector's boxes as the line source
/// (#570): inside each text region, the detected lines whose center falls in
/// the region are the crops (whole, in the detector's reading order — see
/// `det_boxes_inside`); a region the detector found nothing in falls back to
/// the projection segmentation. `detected` is in image pixels (the detector ran
/// on `img`).
///
/// Why: the projection profile cuts a region into full-width strips, so on a
/// form — several fields on one baseline, separated by gaps the strip spans —
/// one crop carries `FROM: J.Smith   DATE: 3/4/92   REF: 1138` and the
/// recognizer glues or drops words (`OLDCOLDMENTHOLUIGHTS&ULTRA`). RapidOCR,
/// docling's engine, recognizes the detector's own boxes, each a single text
/// run with the detector's margin around it. On FUNSD this is the difference
/// between 0.61 and 0.85 word recall (see the issue's measurements).
///
/// The crop is the detector's rotated rectangle warped upright
/// ([`rotate_crop`], RapidOCR's `get_rotate_crop_image`), not its
/// axis-aligned bounding box (#981): a scan is never quite straight, and the
/// bounding box of a line skewed by θ is `width · tan θ` taller than the
/// line — at 0.6° a 700 px line of 12 px print gains 7 px, a third of its
/// height — so the recognizer got the long lines squeezed to two thirds of
/// their width and slanted, and dropped letters and glued words along them
/// (`Baccalaatpoesne accurlatci usgers`) while the short lines on the same
/// page read fine; the same crops read 0 garbled lines in RapidOCR with the
/// identical models. The cell keeps the axis-aligned box as its geometry.
pub fn prep_region_lines_det(
    img: &RgbImage,
    regions: &[crate::layout::Region],
    scale: f32,
    detected: &[crate::ocr_det::DetBox],
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
        let inside = det_boxes_inside(detected, l, t, r, b, iw, ih);
        if inside.is_empty() {
            let (bb, pl) = prep_region_lines(img, std::slice::from_ref(region), scale);
            bboxes.extend(bb);
            lines.extend(pl);
            continue;
        }
        for ((dl, dt, dr, db), d) in inside {
            let Some(pl) = rotate_crop(img, &d.quad).as_ref().and_then(prep_line) else {
                continue;
            };
            bboxes.push((
                dl as f32 / scale,
                dt as f32 / scale,
                dr as f32 / scale,
                db as f32 / scale,
            ));
            lines.push(pl);
        }
    }
    (bboxes, lines)
}

/// Split a text line into word tokens by a vertical ink-projection profile:
/// runs of ink columns separated by whitespace wider than ~0.6× the line
/// height (an inter-word/-column gap, not an inter-character one). Returns tight
/// `(l, 0, r, h)` boxes in line-crop pixels. The browser table path recognizes
/// these individually so each word carries its own box for column matching —
/// the native pipeline gets word boxes from the pdfium text layer instead.
pub fn segment_words(line: &RgbImage) -> Vec<(u32, u32, u32, u32)> {
    let (w, h) = line.dimensions();
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let mean: f32 = line.pixels().map(luma).sum::<f32>() / (w * h) as f32;
    let thresh = mean * 0.7;
    let mut col_ink = vec![0u32; w as usize];
    for y in 0..h {
        for x in 0..w {
            if luma(line.get_pixel(x, y)) < thresh {
                col_ink[x as usize] += 1;
            }
        }
    }
    let min_gap = ((h as f32) * 0.6).max(4.0) as u32;
    let mut words = Vec::new();
    let mut start: Option<u32> = None;
    let mut last_ink = 0u32;
    let mut gap = 0u32;
    for x in 0..w {
        if col_ink[x as usize] > 0 {
            if start.is_none() {
                start = Some(x);
            }
            last_ink = x;
            gap = 0;
        } else if let Some(s) = start {
            gap += 1;
            if gap >= min_gap {
                words.push((s, 0, last_ink + 1, h));
                start = None;
            }
        }
    }
    if let Some(s) = start {
        words.push((s, 0, last_ink + 1, h));
    }
    words
}

/// Gather word crops from a page's *table* regions (browser table path, #157
/// stage 3): crop each table region, split it into lines, split each line into
/// words ([`segment_words`]), prep each word for recognition, and keep the
/// word's page-point bbox. Recognizing table interiors is what gives the cell
/// matcher the word boxes it needs — the native pipeline reads those from
/// pdfium's text layer on digital pages, and calls this on scanned ones
/// (`OcrModel::ocr_table_words`, #173), same as the browser path.
pub fn prep_table_words(
    img: &RgbImage,
    regions: &[crate::layout::Region],
    scale: f32,
) -> (Vec<LineBox>, Vec<PrepLine>) {
    let (iw, ih) = img.dimensions();
    let mut bboxes = Vec::new();
    let mut lines = Vec::new();
    for region in regions {
        if !crate::assemble::is_table_like(region.label) {
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
            for (wx0, _, wx1, _) in segment_words(&line) {
                let word = imageops::crop_imm(&line, wx0, 0, wx1 - wx0, ry - ly).to_image();
                let Some(pl) = prep_line(&word) else {
                    continue;
                };
                bboxes.push((
                    (l + lx + wx0) as f32 / scale,
                    (t + ly) as f32 / scale,
                    (l + lx + wx1) as f32 / scale,
                    (t + ry) as f32 / scale,
                ));
                lines.push(pl);
            }
        }
    }
    (bboxes, lines)
}

/// [`prep_table_words`] with the text detector's boxes as the word source
/// (#570): inside each table region the detected boxes whose center falls in
/// it are the crops (whole) — RapidOCR's cells, which is what
/// docling's TableFormer matcher receives on a scanned table; a table the
/// detector found nothing in falls back to the projection line/word split.
pub fn prep_table_words_det(
    img: &RgbImage,
    regions: &[crate::layout::Region],
    scale: f32,
    detected: &[crate::ocr_det::DetBox],
) -> (Vec<LineBox>, Vec<PrepLine>) {
    let (iw, ih) = img.dimensions();
    let mut bboxes = Vec::new();
    let mut lines = Vec::new();
    for region in regions {
        if !crate::assemble::is_table_like(region.label) {
            continue;
        }
        let l = (region.l * scale).max(0.0) as u32;
        let t = (region.t * scale).max(0.0) as u32;
        let r = ((region.r * scale).max(0.0) as u32).min(iw);
        let b = ((region.b * scale).max(0.0) as u32).min(ih);
        if r <= l || b <= t {
            continue;
        }
        let inside = det_boxes_inside(detected, l, t, r, b, iw, ih);
        if inside.is_empty() {
            let (bb, pl) = prep_table_words(img, std::slice::from_ref(region), scale);
            bboxes.extend(bb);
            lines.extend(pl);
            continue;
        }
        // The quad's warp, as for the text lines (#981): a skewed table's
        // long cells squeeze the same way.
        for ((dl, dt, dr, db), d) in inside {
            let Some(pl) = rotate_crop(img, &d.quad).as_ref().and_then(prep_line) else {
                continue;
            };
            bboxes.push((
                dl as f32 / scale,
                dt as f32 / scale,
                dr as f32 / scale,
                db as f32 / scale,
            ));
            lines.push(pl);
        }
    }
    (bboxes, lines)
}

/// The detected boxes whose center lies in the `l..r × t..b` image-pixel
/// rectangle, in the detector's reading order, each with its axis-aligned
/// pixel rectangle clamped to the image (the cell's geometry). A box is
/// taken whole, not clipped to the region: the layout box is often tight on
/// the ink while the detector's unclip margin reaches past it, and cutting
/// that margin off clips the first and last glyphs — RapidOCR recognizes the
/// detector's box as drawn. The crop may thus poke a little outside the
/// region; the cell keeps the box geometry and assembly assigns it by
/// overlap, as for any cell.
/// A detected line's box clamped to the image, in whole pixels.
type PxRect = (u32, u32, u32, u32);

fn det_boxes_inside(
    detected: &[crate::ocr_det::DetBox],
    l: u32,
    t: u32,
    r: u32,
    b: u32,
    iw: u32,
    ih: u32,
) -> Vec<(PxRect, &crate::ocr_det::DetBox)> {
    detected
        .iter()
        .filter(|d| {
            let (cx, cy) = ((d.l + d.r) / 2.0, (d.t + d.b) / 2.0);
            cx >= l as f32 && cx < r as f32 && cy >= t as f32 && cy < b as f32
        })
        .filter_map(|d| det_box_px(d, iw, ih).map(|px| (px, d)))
        .collect()
}

/// A detector box's axis-aligned pixel rectangle, truncated and clamped to a
/// `iw × ih` image — the plain crop the page OCR recognizes; `None` when
/// nothing of it is left.
fn det_box_px(d: &crate::ocr_det::DetBox, iw: u32, ih: u32) -> Option<PxRect> {
    let dl = d.l.max(0.0) as u32;
    let dt = d.t.max(0.0) as u32;
    let dr = (d.r.max(0.0) as u32).min(iw);
    let db = (d.b.max(0.0) as u32).min(ih);
    (dr > dl && db > dt).then_some((dl, dt, dr, db))
}

/// The recognizer's crop of one detector box on `img` (#939), exactly as the
/// page OCR takes it: RapidOCR's warp of the box's quad ([`rotate_crop`] —
/// upright out of the detector's rotated rectangle, tall lines turned 90°;
/// the plain pixels for an upright box). `None` for a box with nothing
/// inside the image. Until #981 this was the axis-aligned box unless
/// `ocr_textline_orientation` was on; the warp is now the crop for every
/// detected line, the option adds only the 0/180° classifier.
pub fn det_box_crop(img: &RgbImage, d: &crate::ocr_det::DetBox) -> Option<RgbImage> {
    // Intersection first (#947): `rotate_crop` replicates border pixels, so
    // a box wholly outside the image would otherwise come back as
    // edge-colored pixels — the same box `det_boxes_inside`, which feeds
    // the page OCR, drops.
    det_box_px(d, img.width(), img.height())?;
    rotate_crop(img, &d.quad)
}

/// A text line's pixels before recognition prep: an oriented crop still to
/// classify (#666), or a projection strip already prepared.
pub enum LineSrc {
    Crop(RgbImage),
    Prepared(PrepLine),
}

/// [`prep_region_lines_det`] for `ocr_textline_orientation` (#666): the
/// same crops — each detected line warped upright out of its minimum-area
/// rectangle and turned when tall ([`rotate_crop`]) — left unprepared so
/// the orientation classifier can see them first. The boxes are the same
/// axis-aligned ones, so reading order and geometry do not move; a region
/// without detected lines falls back to the projection split as there.
pub fn region_line_crops_det(
    img: &RgbImage,
    regions: &[crate::layout::Region],
    scale: f32,
    detected: &[crate::ocr_det::DetBox],
) -> (Vec<LineBox>, Vec<LineSrc>) {
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
        let inside = det_boxes_inside(detected, l, t, r, b, iw, ih);
        if inside.is_empty() {
            let (bb, pl) = prep_region_lines(img, std::slice::from_ref(region), scale);
            bboxes.extend(bb);
            lines.extend(pl.into_iter().map(LineSrc::Prepared));
            continue;
        }
        for ((dl, dt, dr, db), d) in inside {
            let Some(crop) = rotate_crop(img, &d.quad) else {
                continue;
            };
            bboxes.push((
                dl as f32 / scale,
                dt as f32 / scale,
                dr as f32 / scale,
                db as f32 / scale,
            ));
            lines.push(LineSrc::Crop(crop));
        }
    }
    (bboxes, lines)
}

/// RapidOCR's `get_rotate_crop_image` (#666): the quad `[tl, tr, br, bl]`
/// warped onto an upright `w × h` rectangle — `w`, `h` the longer of each
/// pair of opposite edges, truncated — with bicubic sampling (OpenCV's
/// `INTER_CUBIC`, a = −0.75) and replicated borders; then, when the crop is
/// at least 1.5× taller than wide, turned 90° counter-clockwise (numpy's
/// `rot90`), so vertical text reaches the recognizer lying down. An
/// axis-aligned integer quad samples exactly its pixels — the plain crop.
/// `None` for a degenerate quad.
pub fn rotate_crop(img: &RgbImage, quad: &[(f32, f32); 4]) -> Option<RgbImage> {
    let dist = |a: (f32, f32), b: (f32, f32)| ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
    let w = dist(quad[0], quad[1]).max(dist(quad[2], quad[3])) as u32;
    let h = dist(quad[0], quad[3]).max(dist(quad[1], quad[2])) as u32;
    if w == 0 || h == 0 || img.width() == 0 || img.height() == 0 {
        return None;
    }
    let (wf, hf) = (w as f64, h as f64);
    let map = perspective(
        [(0.0, 0.0), (wf, 0.0), (wf, hf), (0.0, hf)],
        quad.map(|(x, y)| (x as f64, y as f64)),
    )?;
    let mut out = RgbImage::new(w, h);
    for (x, y, px) in out.enumerate_pixels_mut() {
        let (x, y) = (x as f64, y as f64);
        let d = map[6] * x + map[7] * y + 1.0;
        if d.abs() < 1e-12 {
            continue;
        }
        let sx = (map[0] * x + map[1] * y + map[2]) / d;
        let sy = (map[3] * x + map[4] * y + map[5]) / d;
        *px = bicubic_replicate(img, sx, sy);
    }
    Some(if h as f32 / w as f32 >= 1.5 {
        imageops::rotate270(&out)
    } else {
        out
    })
}

/// The homography taking each `from` point to its `to` point (OpenCV's
/// `getPerspectiveTransform`): `[a, b, c, d, e, f, g, h]` with
/// `x' = (ax + by + c) / (gx + hy + 1)`, `y' = (dx + ey + f) / (gx + hy + 1)`.
/// `None` for a degenerate (collinear) quad.
fn perspective(from: [(f64, f64); 4], to: [(f64, f64); 4]) -> Option<[f64; 8]> {
    let mut m = [[0f64; 9]; 8];
    for k in 0..4 {
        let ((x, y), (u, v)) = (from[k], to[k]);
        m[2 * k] = [x, y, 1.0, 0.0, 0.0, 0.0, -x * u, -y * u, u];
        m[2 * k + 1] = [0.0, 0.0, 0.0, x, y, 1.0, -x * v, -y * v, v];
    }
    // Gaussian elimination with partial pivoting.
    for col in 0..8 {
        let pivot = (col..8).max_by(|&a, &b| m[a][col].abs().total_cmp(&m[b][col].abs()))?;
        if m[pivot][col].abs() < 1e-12 {
            return None;
        }
        m.swap(col, pivot);
        let pivot_row = m[col];
        for (row, r) in m.iter_mut().enumerate() {
            let f = r[col] / pivot_row[col];
            if row != col && f != 0.0 {
                for (v, p) in r[col..].iter_mut().zip(&pivot_row[col..]) {
                    *v -= f * p;
                }
            }
        }
    }
    Some(std::array::from_fn(|i| m[i][8] / m[i][i]))
}

/// One bicubic sample at `(x, y)` (OpenCV `INTER_CUBIC`, a = −0.75),
/// coordinates outside the image clamped to its edge (`BORDER_REPLICATE`).
fn bicubic_replicate(img: &RgbImage, x: f64, y: f64) -> Rgb<u8> {
    fn weights(t: f64) -> [f64; 4] {
        const A: f64 = -0.75;
        let w0 = ((A * (t + 1.0) - 5.0 * A) * (t + 1.0) + 8.0 * A) * (t + 1.0) - 4.0 * A;
        let w1 = ((A + 2.0) * t - (A + 3.0)) * t * t + 1.0;
        let w2 = ((A + 2.0) * (1.0 - t) - (A + 3.0)) * (1.0 - t) * (1.0 - t) + 1.0;
        [w0, w1, w2, 1.0 - w0 - w1 - w2]
    }
    let (x0, y0) = (x.floor(), y.floor());
    let (wx, wy) = (weights(x - x0), weights(y - y0));
    let (mw, mh) = (img.width() as i64 - 1, img.height() as i64 - 1);
    let mut acc = [0f64; 3];
    for (j, wyj) in wy.iter().enumerate() {
        let yy = (y0 as i64 - 1 + j as i64).clamp(0, mh) as u32;
        for (i, wxi) in wx.iter().enumerate() {
            let xx = (x0 as i64 - 1 + i as i64).clamp(0, mw) as u32;
            let p = img.get_pixel(xx, yy);
            let w = wxi * wyj;
            for c in 0..3 {
                acc[c] += w * p[c] as f64;
            }
        }
    }
    Rgb(acc.map(|v| v.round().clamp(0.0, 255.0) as u8))
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

/// RapidOCR's `text_score` (docling's `RapidOcrOptions.text_score`, 0.5): a
/// recognized line whose mean emitted-character confidence falls below it is
/// dropped — a shaded bar, a halftone, a rule read as a run of letters
/// (`TUANEURHENSTGOFUPHYCLTOSHSWOYT` for a form's grey `GEOGRAPHY` band)
/// would otherwise land in the document as a heading. `DOCLING_RS_OCR_TEXT_SCORE`
/// overrides it (`0` keeps every line, the pre-#570 behavior). Cached.
pub fn text_score() -> f32 {
    static SCORE: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *SCORE.get_or_init(|| {
        text_score_from(docling_core::env::nonempty("DOCLING_RS_OCR_TEXT_SCORE").as_deref())
    })
}

/// [`text_score`] for one raw `DOCLING_RS_OCR_TEXT_SCORE` value: a score in
/// 0–1, else RapidOCR's 0.5.
fn text_score_from(raw: Option<&str>) -> f32 {
    raw.and_then(|r| r.trim().parse::<f32>().ok())
        .filter(|s| s.is_finite() && (0.0..=1.0).contains(s))
        .unwrap_or(0.5)
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

    /// #570: inside a text region the detector's boxes are the crops — one
    /// per box, clipped to the region, in the detector's order — and a region
    /// the detector saw nothing in falls back to the projection strips. The
    /// two-bar page as one region: two detected boxes → two det-shaped lines;
    /// the region's second bar split by the detector into two words → three
    /// lines where the projection found two; no boxes → the projection's two.
    #[test]
    fn detector_boxes_are_the_region_lines_with_projection_fallback() {
        let img = page();
        let region = crate::layout::Region {
            label: "text",
            score: 0.9,
            l: 0.0,
            t: 0.0,
            r: 200.0,
            b: 100.0,
        };
        let bx = |l: f32, t: f32, r: f32, b: f32| crate::ocr_det::DetBox::rect(l, t, r, b, 0.9);
        let det = [bx(8.0, 18.0, 192.0, 32.0), bx(8.0, 58.0, 122.0, 74.0)];
        let (boxes, lines) = prep_region_lines_det(&img, std::slice::from_ref(&region), 1.0, &det);
        assert_eq!(
            boxes,
            vec![(8.0, 18.0, 192.0, 32.0), (8.0, 58.0, 122.0, 74.0)]
        );
        assert_eq!(lines.len(), 2);
        // A box reaching past the region is taken whole (clipped only to the
        // image); one centered outside the region is not its line.
        let narrow = crate::layout::Region {
            r: 150.0,
            ..region.clone()
        };
        let det = [bx(-10.0, 18.0, 250.0, 32.0), bx(300.0, 60.0, 400.0, 70.0)];
        let (boxes, _) = prep_region_lines_det(&img, std::slice::from_ref(&narrow), 1.0, &det);
        assert_eq!(boxes, vec![(0.0, 18.0, 200.0, 32.0)]);
        // The detector split the second bar into two words.
        let det = [
            bx(8.0, 18.0, 192.0, 32.0),
            bx(8.0, 58.0, 60.0, 74.0),
            bx(70.0, 58.0, 122.0, 74.0),
        ];
        let (boxes, _) = prep_region_lines_det(&img, std::slice::from_ref(&region), 1.0, &det);
        assert_eq!(boxes.len(), 3);
        // No detected box in the region → the projection segmentation.
        let (fallback, _) = prep_region_lines_det(&img, std::slice::from_ref(&region), 1.0, &[]);
        let (projection, _) = prep_region_lines(&img, std::slice::from_ref(&region), 1.0);
        assert_eq!(fallback, projection);
        assert_eq!(projection.len(), 2);
        // Table regions take the same boxes as their word crops.
        let table = crate::layout::Region {
            label: "table",
            ..region.clone()
        };
        let det = [bx(8.0, 18.0, 92.0, 32.0), bx(108.0, 18.0, 192.0, 32.0)];
        let (words, _) = prep_table_words_det(&img, std::slice::from_ref(&table), 1.0, &det);
        assert_eq!(
            words,
            vec![(8.0, 18.0, 92.0, 32.0), (108.0, 18.0, 192.0, 32.0)]
        );
        let (fallback, _) = prep_table_words_det(&img, std::slice::from_ref(&table), 1.0, &[]);
        assert_eq!(
            fallback,
            prep_table_words(&img, std::slice::from_ref(&table), 1.0).0
        );
    }

    /// #939: a detector box's recognizer crop — the quad's warp, which for
    /// an integer rectangle is exactly its pixels; a box reaching past the
    /// image edge warps whole (replicated border, RapidOCR's
    /// `warpPerspective`), one with nothing inside the image → `None` (#947).
    #[test]
    fn det_box_crop_matches_the_page_ocr_crop() {
        let img = RgbImage::from_fn(60, 40, |x, y| Rgb([(x * 4) as u8, (y * 6) as u8, 7]));
        let d = crate::ocr_det::DetBox::rect(10.0, 5.0, 30.0, 25.0, 0.9);
        let plain = det_box_crop(&img, &d).expect("crop");
        assert_eq!(plain.dimensions(), (20, 20));
        assert_eq!(plain.get_pixel(0, 0), img.get_pixel(10, 5));
        assert_eq!(
            plain,
            image::imageops::crop_imm(&img, 10, 5, 20, 20).to_image()
        );
        let edge = crate::ocr_det::DetBox::rect(50.7, -3.0, 80.0, 10.0, 0.9);
        assert_eq!(det_box_crop(&img, &edge).unwrap().dimensions(), (29, 13));
        let off = crate::ocr_det::DetBox::rect(70.0, 0.0, 90.0, 10.0, 0.9);
        assert_eq!(det_box_crop(&img, &off), None);
        let tiny = RgbImage::from_pixel(2, 2, Rgb([255, 255, 255]));
        let outside = crate::ocr_det::DetBox::rect(3.0, 0.0, 5.0, 2.0, 0.9);
        assert_eq!(det_box_crop(&tiny, &outside), None);
        let inside = crate::ocr_det::DetBox::rect(0.0, 0.0, 2.0, 2.0, 0.9);
        assert_eq!(det_box_crop(&tiny, &inside).unwrap().dimensions(), (2, 2));
    }

    /// #981: a detected line on a skewed scan is cropped out of the
    /// detector's rotated rectangle, not its axis-aligned bounding box — a
    /// 60 × 12 bar at 8° has a 62 × 20 bounding box, whose crop would reach
    /// the recognizer squeezed to 146 px wide and slanted; the warp gives
    /// the bar's own 60 × 12, i.e. 240 px at the 48 px height. The cell's
    /// geometry stays the bounding box.
    #[test]
    fn a_skewed_detected_line_is_warped_upright_not_boxed() {
        let (cx, cy, hw, hh, a) = (60.0f32, 40.0f32, 30.0f32, 6.0f32, 8.0f32.to_radians());
        let img = RgbImage::from_fn(120, 80, |x, y| {
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            let (u, v) = (dx * a.cos() + dy * a.sin(), -dx * a.sin() + dy * a.cos());
            if u.abs() <= hw && v.abs() <= hh {
                Rgb([0, 0, 0])
            } else {
                Rgb([255, 255, 255])
            }
        });
        let corner = |su: f32, sv: f32| {
            (
                cx + su * hw * a.cos() - sv * hh * a.sin(),
                cy + su * hw * a.sin() + sv * hh * a.cos(),
            )
        };
        let quad = crate::ocr_det::order_corners([
            corner(-1.0, -1.0),
            corner(1.0, -1.0),
            corner(1.0, 1.0),
            corner(-1.0, 1.0),
        ]);
        let (xs, ys): (Vec<f32>, Vec<f32>) = quad.iter().copied().unzip();
        let (l, t) = (
            xs.iter().cloned().fold(f32::MAX, f32::min),
            ys.iter().cloned().fold(f32::MAX, f32::min),
        );
        let (r, b) = (
            xs.iter().cloned().fold(f32::MIN, f32::max),
            ys.iter().cloned().fold(f32::MIN, f32::max),
        );
        let det = crate::ocr_det::DetBox {
            l,
            t,
            r,
            b,
            score: 0.9,
            quad,
        };
        let region = crate::layout::Region {
            label: "text",
            score: 0.9,
            l: 0.0,
            t: 0.0,
            r: 120.0,
            b: 80.0,
        };
        let (boxes, lines) =
            prep_region_lines_det(&img, std::slice::from_ref(&region), 1.0, &[det]);
        assert_eq!(lines.len(), 1);
        let bbox_w = (((r.floor() - l.floor()) * 48.0) / (b.floor() - t.floor())).round() as usize;
        assert!(bbox_w < 160, "bounding box would give {bbox_w}");
        assert!(
            (236..=240).contains(&lines[0].w),
            "warped width {}",
            lines[0].w
        );
        // Geometry: the bounding box, in whole pixels.
        assert_eq!(
            boxes,
            vec![(
                l as u32 as f32,
                t as u32 as f32,
                r as u32 as f32,
                b as u32 as f32
            )]
        );
        let (words, _) = prep_table_words_det(
            &img,
            &[crate::layout::Region {
                label: "table",
                score: 0.9,
                l: 0.0,
                t: 0.0,
                r: 120.0,
                b: 80.0,
            }],
            1.0,
            &[det],
        );
        assert_eq!(words, boxes);
    }

    /// RapidOCR's `text_score` default, the pre-#570 "keep everything" with
    /// `0`, and garbage values ignored.
    #[test]
    fn text_score_default_is_rapidocrs() {
        assert_eq!(text_score_from(None), 0.5);
        assert_eq!(text_score_from(Some("0")), 0.0);
        assert_eq!(text_score_from(Some("0.7")), 0.7);
        for garbage in ["1.5", "-0.1", "NaN", "high", ""] {
            assert_eq!(text_score_from(Some(garbage)), 0.5, "{garbage:?}");
        }
    }

    #[test]
    fn dark_mode_pages_normalize_to_scan_polarity() {
        // The same two-bar page, inverted (light text on dark) — the raw
        // segmentation misfires (it thresholds the dark *background* as ink,
        // so the "lines" it finds are the inter-bar gaps, not the bars);
        // polarity normalization recovers the true structure.
        let mut dark = page();
        for px in dark.pixels_mut() {
            px.0 = [255 - px.0[0], 255 - px.0[1], 255 - px.0[2]];
        }
        assert_ne!(segment_lines(&dark), segment_lines(&page()));
        let fixed = normalize_polarity(dark);
        assert_eq!(segment_lines(&fixed), segment_lines(&page()));
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

#[cfg(test)]
mod word_segmentation {
    use image::{Rgb, RgbImage};

    /// A white line carrying two ink blocks separated by `gap` pixels.
    fn line_with_gap(h: u32, gap: u32) -> RgbImage {
        let w = 30 + gap + 30 + 10;
        let mut img = RgbImage::from_pixel(w, h, Rgb([255, 255, 255]));
        for (x0, x1) in [(5u32, 35u32), (35 + gap, 65 + gap)] {
            for x in x0..x1.min(w) {
                for y in h / 4..(3 * h / 4) {
                    img.put_pixel(x, y, Rgb([0, 0, 0]));
                }
            }
        }
        img
    }

    /// [`segment_words`] splits on a gap of `0.6 x line height`, which is the
    /// property the rest of the browser table path inherits: an inter-word
    /// space never splits, but a column gap wider than that does. Anything
    /// narrower stays a single box — and a single box can only ever land in one
    /// TableFormer cell, so this threshold is the floor on how tight a table's
    /// columns may be before its cells merge.
    #[test]
    fn words_split_only_on_gaps_above_six_tenths_of_the_line_height() {
        for h in [16u32, 24, 32, 40] {
            let split_at = (1..=40u32)
                .find(|&gap| super::segment_words(&line_with_gap(h, gap)).len() >= 2)
                .expect("some gap splits");
            let ratio = split_at as f32 / h as f32;
            assert!(
                (0.5..=0.65).contains(&ratio),
                "h={h}: split at {split_at}px ({ratio:.2} x height)"
            );
            // Just below the threshold the two blocks are one word.
            assert_eq!(
                super::segment_words(&line_with_gap(h, split_at - 1)).len(),
                1,
                "h={h}: a narrower gap must not split"
            );
        }
    }

    /// #666: an axis-aligned integer quad warps to exactly its pixels — the
    /// plain crop, so upright lines prepare as before — and a crop at least
    /// 1.5× taller than wide is turned 90° counter-clockwise.
    #[test]
    fn rotate_crop_is_the_plain_crop_for_an_upright_box_and_turns_tall_ones() {
        let img = RgbImage::from_fn(40, 30, |x, y| Rgb([(x * 6) as u8, (y * 8) as u8, 77]));
        let quad = [(5.0, 4.0), (25.0, 4.0), (25.0, 14.0), (5.0, 14.0)];
        let warped = super::rotate_crop(&img, &quad).unwrap();
        let plain = image::imageops::crop_imm(&img, 5, 4, 20, 10).to_image();
        assert_eq!(warped, plain);
        // 6 wide, 12 tall: turned to 12 × 6, the top row now the left column.
        let tall = [(10.0, 2.0), (16.0, 2.0), (16.0, 14.0), (10.0, 14.0)];
        let turned = super::rotate_crop(&img, &tall).unwrap();
        assert_eq!(turned.dimensions(), (12, 6));
        assert_eq!(turned.get_pixel(0, 5), img.get_pixel(10, 2));
        assert!(super::rotate_crop(&img, &[(1.0, 1.0); 4]).is_none());
    }

    /// A rectangle drawn at 30° comes out upright: its warp is a uniform
    /// dark bar, whatever the angle.
    #[test]
    fn rotate_crop_straightens_a_rotated_rectangle() {
        let (cx, cy, a) = (50.0f32, 40.0f32, 30f32.to_radians());
        let (hw, hh) = (30.0f32, 6.0f32);
        let inside = |x: f32, y: f32| {
            let (dx, dy) = (x - cx, y - cy);
            let u = dx * a.cos() + dy * a.sin();
            let v = -dx * a.sin() + dy * a.cos();
            u.abs() <= hw && v.abs() <= hh
        };
        let img = RgbImage::from_fn(100, 80, |x, y| {
            if inside(x as f32 + 0.5, y as f32 + 0.5) {
                Rgb([0, 0, 0])
            } else {
                Rgb([255, 255, 255])
            }
        });
        let corner = |su: f32, sv: f32| {
            (
                cx + su * hw * a.cos() - sv * hh * a.sin(),
                cy + su * hw * a.sin() + sv * hh * a.cos(),
            )
        };
        let quad = crate::ocr_det::order_corners([
            corner(-1.0, -1.0),
            corner(1.0, -1.0),
            corner(1.0, 1.0),
            corner(-1.0, 1.0),
        ]);
        let bar = super::rotate_crop(&img, &quad).unwrap();
        // 60 × 12, less what float edge lengths truncate.
        let (w, h) = bar.dimensions();
        assert!(
            (59..=60).contains(&w) && (11..=12).contains(&h),
            "{w} × {h}"
        );
        // The interior (away from the anti-aliased edge) is all ink.
        for y in 2..h - 2 {
            for x in 2..w - 2 {
                assert!(
                    bar.get_pixel(x, y)[0] < 60,
                    "({x}, {y}) {:?}",
                    bar.get_pixel(x, y)
                );
            }
        }
    }
}
