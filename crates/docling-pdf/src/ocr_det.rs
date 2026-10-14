//! PP-OCR text *detection* for bitmap pages (#429): the DB (Differentiable
//! Binarization) detector RapidOCR runs in front of its recognizer, ported so
//! text the layout model gives no region — a diagram's labels, a chart's
//! ticks, a stamp, a page number in the margin — is still read.
//!
//! The pipeline's OCR has always been *recognition-only*: PP-OCRv3 rec runs
//! on the lines inside layout regions, and nothing else on a page is looked
//! at. docling's engines (RapidOCR, EasyOCR, Tesseract) instead detect text
//! lines over the whole bitmap and every detected line becomes a cell — the
//! ones no layout cluster claims turn into orphan text clusters (confidence
//! 1.0) and read out in the flow. `ModalNet-19.png` shows the gap: the layout
//! model scores only `SoftMax` above its 0.5 threshold (docling's own layout
//! run yields *zero* clusters there), yet docling reads `MatMul`, `SoftMax`,
//! `Mask (opt.)`, `Scale`, `Q`, `K` — all from its detector.
//!
//! Model: RapidOCR's `PP-OCRv6_det_small.onnx` (the one docling 2.127's
//! RapidOCR default resolves to; `.models/ocr_det.onnx`, `DOCLING_OCR_DET_ONNX`
//! overrides). Pre/post-processing follow `rapidocr/ch_ppocr_det`
//! (`DetPreProcess` / `DBPostProcess`) with RapidOCR's config: shorter side
//! scaled up to 736 (`limit_type: min`), sides rounded to multiples of 32,
//! BGR channel order (cv2 input), `(x/255 − 0.5)/0.5`; probability threshold
//! 0.3, 2×2 dilation, per-blob minimum-area rectangle, `box_score_fast` ≥ 0.5,
//! unclip ratio 1.6, RapidOCR's row-then-column box order. Two deliberate
//! simplifications: only outer blob boundaries are considered (OpenCV's
//! `RETR_LIST` also walks hole contours, whose boxes fail the score gate
//! anyway), and a detected quad is handed on as its axis-aligned bounding
//! box — the recognizer's line prep crops rectangles, and rotated lines are
//! not what documents lose today. The quad rides along on [`DetBox::quad`]:
//! with `ocr_textline_orientation` on (#666) the line prep warps it upright
//! the way RapidOCR's `get_rotate_crop_image` does.
//!
//! Since #570 the detector is also the recognizer's *line source* inside the
//! layout regions (`ocr_prep::prep_region_lines_det`), as it is for RapidOCR:
//! the projection split that preceded it cut a region into full-width strips,
//! which on forms — several fields per baseline — glued and dropped words
//! (FUNSD word recall 0.57 → 0.68 on this change alone; `DOCLING_RS_OCR_LINES=
//! projection` restores the strips). Detected boxes outside every region are
//! still recognized and placed as orphan text (`uncovered_lines`). Missing
//! model → no detection, quietly (`DOCLING_RS_DEBUG` reports it): regions
//! fall back to the projection split, the margins go unread.

use image::RgbImage;

/// RapidOCR `Det.limit_side_len` with `limit_type: min`.
pub const LIMIT_SIDE_LEN: u32 = 736;
/// `DBPostProcess(thresh=…)`: probability → binary mask.
pub const THRESH: f32 = 0.3;
/// `DBPostProcess(box_thresh=…)`: minimum mean probability inside a box.
pub const BOX_THRESH: f32 = 0.5;
/// `DBPostProcess(unclip_ratio=…)`.
pub const UNCLIP_RATIO: f32 = 1.6;
/// `DBPostProcess.min_size`: the shortest side a blob's rectangle may have.
const MIN_SIZE: f32 = 3.0;
/// `TextDetector._BOX_SORT_Y_THRESHOLD`: boxes whose top-left `y` differ by
/// less than this share a row when ordering.
const BOX_SORT_Y_THRESHOLD: f32 = 10.0;

/// One detected text line: its axis-aligned box in *input image pixels* and
/// the DB score (mean probability inside the pre-unclip rectangle).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DetBox {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
    pub score: f32,
    /// The detector's (unclipped) minimum-area rectangle the box bounds,
    /// `[tl, tr, br, bl]` in the same pixels (#666) — what RapidOCR crops.
    pub quad: [(f32, f32); 4],
}

impl DetBox {
    /// An axis-aligned box, its quad the rectangle itself.
    pub fn rect(l: f32, t: f32, r: f32, b: f32, score: f32) -> Self {
        Self {
            l,
            t,
            r,
            b,
            score,
            quad: [(l, t), (r, t), (r, b), (l, b)],
        }
    }
}

/// `DetPreProcess.resize`: the network input size for a `w × h` image —
/// shorter side scaled up to [`LIMIT_SIDE_LEN`] (never down), both sides
/// rounded to a multiple of 32. `None` when a side rounds to zero.
pub fn det_input_size(w: u32, h: u32) -> Option<(u32, u32)> {
    det_input_size_capped(w, h, max_side_cap())
}

/// Default cap on the detector input's longer side: RapidOCR's
/// `Global.max_side_len` (2000) — the longest side its whole OCR input is
/// shrunk to before detection, and so what docling's detector sees. Until
/// #570 this was PaddleOCR's 960 (`det_limit_side_len`, `limit_type: max`):
/// while the detector only supplemented the region pass that cost about a
/// third of the detection time and moved outputs by noise-level amounts;
/// now that its boxes are the recognizer's crops the resolution is recall —
/// 960 reads 0.834 of FUNSD's words where 2000 reads 0.856 (30 forms, same
/// recognizer). A Letter page at the 2.0 px/pt render goes in at 1216 ×
/// 1600; `DOCLING_RS_OCR_DET_MAX_SIDE=0` lifts the cap entirely, 960 restores
/// the old budget.
pub const DEFAULT_MAX_SIDE: u32 = 2000;

/// `DOCLING_RS_OCR_DET_MAX_SIDE`: the cap on the detector input's longer
/// side — [`DEFAULT_MAX_SIDE`] unless set, `0` = uncapped (RapidOCR's rule).
/// The DB net is the costliest OCR stage on a scanned page and its cost is
/// linear in input pixels; a tighter cap trades small-print recall for time.
fn max_side_cap() -> u32 {
    static CAP: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        docling_core::env::parse::<u32>("DOCLING_RS_OCR_DET_MAX_SIDE").unwrap_or(DEFAULT_MAX_SIDE)
    })
}

/// [`det_input_size`] with an explicit longer-side cap (`0` = none): the cap
/// scales the image down first, then RapidOCR's shorter-side rule applies to
/// what is left (so a capped input never exceeds the cap).
pub fn det_input_size_capped(w: u32, h: u32, max_side: u32) -> Option<(u32, u32)> {
    if w == 0 || h == 0 {
        return None;
    }
    let (w, h) = (w as f32, h as f32);
    // RapidOCR's rule first (shorter side up to 736, never down), then the
    // cap pulls the longer side back if that overshoots it.
    let mut ratio = if w.min(h) < LIMIT_SIDE_LEN as f32 {
        LIMIT_SIDE_LEN as f32 / w.min(h)
    } else {
        1.0
    };
    if max_side > 0 && w.max(h) * ratio > max_side as f32 {
        ratio = max_side as f32 / w.max(h);
    }
    let round32 = |v: f32| ((v as i64 as f32 / 32.0).round() * 32.0) as i64;
    let (rw, rh) = (round32(w * ratio), round32(h * ratio));
    (rw > 0 && rh > 0).then_some((rw as u32, rh as u32))
}

/// The NCHW float input for the detector: the image resized to
/// [`det_input_size`] (bilinear, cv2's default), channels in **BGR** order
/// (RapidOCR feeds a cv2 image), normalized `(x/255 − 0.5)/0.5`. Returns the
/// tensor and its `(width, height)`.
pub fn prep_det_input(img: &RgbImage) -> Option<(Vec<f32>, u32, u32)> {
    let (w, h) = det_input_size(img.width(), img.height())?;
    prep_det_input_at(img, w, h)
}

/// [`prep_det_input`] with an explicit cap on the input's longer side
/// ([`det_input_size_capped`], `0` = none) instead of the process-wide
/// `DOCLING_RS_OCR_DET_MAX_SIDE`, for a caller whose box thresholds were
/// tuned at one input scale.
pub fn prep_det_input_capped(img: &RgbImage, max_side: u32) -> Option<(Vec<f32>, u32, u32)> {
    let (w, h) = det_input_size_capped(img.width(), img.height(), max_side)?;
    prep_det_input_at(img, w, h)
}

fn prep_det_input_at(img: &RgbImage, w: u32, h: u32) -> Option<(Vec<f32>, u32, u32)> {
    let resized = if (w, h) == img.dimensions() {
        img.clone()
    } else {
        resize_bilinear(img, w, h)
    };
    let n = (w * h) as usize;
    let mut data = vec![0f32; 3 * n];
    for (i, px) in resized.pixels().enumerate() {
        // B, G, R planes.
        data[i] = px[2] as f32 / 127.5 - 1.0;
        data[n + i] = px[1] as f32 / 127.5 - 1.0;
        data[2 * n + i] = px[0] as f32 / 127.5 - 1.0;
    }
    Some((data, w, h))
}

/// Bilinear resize (cv2's default `INTER_LINEAR`) — `fast_image_resize`'s SIMD
/// convolution with the triangle kernel, the scalar `image` crate resize with
/// `DOCLING_RS_SLOW_RESIZE=1` (same kernel, several times slower; the
/// scalar path is also the fallback should the SIMD one refuse the buffer).
pub(crate) fn resize_bilinear(img: &RgbImage, w: u32, h: u32) -> RgbImage {
    #[cfg(feature = "ml")]
    {
        use fast_image_resize as fir;
        static SLOW: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let slow = *SLOW.get_or_init(|| docling_core::env::flag("DOCLING_RS_SLOW_RESIZE"));
        if !slow {
            let fast = || {
                let src = fir::images::ImageRef::new(
                    img.width(),
                    img.height(),
                    img.as_raw(),
                    fir::PixelType::U8x3,
                )
                .ok()?;
                let mut dst = fir::images::Image::new(w, h, fir::PixelType::U8x3);
                fir::Resizer::new()
                    .resize(
                        &src,
                        &mut dst,
                        &fir::ResizeOptions::new()
                            .resize_alg(fir::ResizeAlg::Convolution(fir::FilterType::Bilinear)),
                    )
                    .ok()?;
                RgbImage::from_raw(w, h, dst.into_vec())
            };
            if let Some(out) = fast() {
                return out;
            }
        }
    }
    image::imageops::resize(img, w, h, image::imageops::FilterType::Triangle)
}

/// `DBPostProcess.__call__`: text boxes from the detector's `w × h`
/// probability map, mapped onto a `dest_w × dest_h` image (the one the map was
/// computed from). Boxes come back in RapidOCR's reading order.
pub fn db_boxes(prob: &[f32], w: usize, h: usize, dest_w: u32, dest_h: u32) -> Vec<DetBox> {
    if prob.len() < w * h || w == 0 || h == 0 {
        return Vec::new();
    }
    // Binarize, then dilate with a 2×2 kernel (cv2.dilate, anchor at the
    // kernel center = its bottom-right cell, so a pixel lights up when it or
    // its left / upper / upper-left neighbour is set).
    let seg = |x: usize, y: usize| prob[y * w + x] > THRESH;
    let mut mask = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            mask[y * w + x] = seg(x, y)
                || (x > 0 && seg(x - 1, y))
                || (y > 0 && seg(x, y - 1))
                || (x > 0 && y > 0 && seg(x - 1, y - 1));
        }
    }
    let mut quads: Vec<([(f32, f32); 4], f32)> = Vec::new();
    let mut seen = vec![false; w * h];
    let mut stack = Vec::new();
    let mut component = Vec::new();
    for start in 0..w * h {
        if !mask[start] || seen[start] {
            continue;
        }
        // 8-connected blob (cv2.findContours' outer boundary connectivity).
        component.clear();
        seen[start] = true;
        stack.push(start);
        while let Some(i) = stack.pop() {
            component.push(i);
            let (x, y) = (i % w, i / w);
            for dy in -1i64..=1 {
                for dx in -1i64..=1 {
                    let (nx, ny) = (x as i64 + dx, y as i64 + dy);
                    if nx < 0 || ny < 0 || nx >= w as i64 || ny >= h as i64 {
                        continue;
                    }
                    let j = ny as usize * w + nx as usize;
                    if mask[j] && !seen[j] {
                        seen[j] = true;
                        stack.push(j);
                    }
                }
            }
        }
        if quads.len() >= 1000 {
            // `max_candidates`.
            break;
        }
        // The contour points are pixel centers on the blob's boundary; the
        // minimum-area rectangle of the boundary is that of the whole blob.
        let boundary: Vec<(f32, f32)> = component
            .iter()
            .copied()
            .filter(|&i| {
                let (x, y) = (i % w, i / w);
                x == 0
                    || y == 0
                    || x + 1 == w
                    || y + 1 == h
                    || !mask[i - 1]
                    || !mask[i + 1]
                    || !mask[i - w]
                    || !mask[i + w]
            })
            .map(|i| ((i % w) as f32, (i / w) as f32))
            .collect();
        let Some((corners, sside)) = min_area_rect(&boundary) else {
            continue;
        };
        if sside < MIN_SIZE {
            continue;
        }
        let score = box_score_fast(prob, w, h, &corners);
        if score < BOX_THRESH {
            continue;
        }
        let Some((expanded, sside)) = unclip(&corners) else {
            continue;
        };
        if sside < MIN_SIZE + 2.0 {
            continue;
        }
        // Into the source image's pixel grid.
        let mapped: [(f32, f32); 4] = std::array::from_fn(|k| {
            let (x, y) = expanded[k];
            (
                (x / w as f32 * dest_w as f32)
                    .round()
                    .clamp(0.0, dest_w as f32),
                (y / h as f32 * dest_h as f32)
                    .round()
                    .clamp(0.0, dest_h as f32),
            )
        });
        quads.push((mapped, score));
    }
    // `filter_det_res`: drop boxes whose rectangle is ≤ 3 px on a side.
    let mut boxes: Vec<DetBox> = quads
        .into_iter()
        .filter_map(|(q, score)| {
            let side =
                |a: (f32, f32), b: (f32, f32)| ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
            let (rw, rh) = (side(q[0], q[1]).floor(), side(q[0], q[3]).floor());
            if rw <= 3.0 || rh <= 3.0 {
                return None;
            }
            let xs = q.iter().map(|p| p.0);
            let ys = q.iter().map(|p| p.1);
            Some(DetBox {
                l: xs.clone().fold(f32::MAX, f32::min),
                t: ys.clone().fold(f32::MAX, f32::min),
                r: xs.fold(f32::MIN, f32::max),
                b: ys.fold(f32::MIN, f32::max),
                score,
                quad: q,
            })
        })
        .collect();
    sort_boxes(&mut boxes);
    boxes
}

/// `TextDetector.sorted_boxes`: by top edge, then a bubble pass that swaps
/// *adjacent* boxes whose tops lie within [`BOX_SORT_Y_THRESHOLD`] when the
/// lower one starts further left — RapidOCR's exact algorithm. The first
/// port (#429) grouped boxes into rows transitively (consecutive tops within
/// the threshold = one row) and sorted each row by `x`; on a two-column
/// newspaper every line top is within 10 px of the next line's in the other
/// column, the whole page chained into one "row", and the lines of a column
/// came out ordered by their left edge — scrambled — once they became the
/// recognizer's crops (#570). RapidOCR's local swaps only reorder boxes that
/// genuinely share a baseline.
pub fn sort_boxes(boxes: &mut [DetBox]) {
    boxes.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.l.total_cmp(&b.l)));
    for i in 0..boxes.len().saturating_sub(1) {
        let mut j = i;
        loop {
            let (upper, lower) = (j, j + 1);
            if (boxes[lower].t - boxes[upper].t).abs() < BOX_SORT_Y_THRESHOLD
                && boxes[lower].l < boxes[upper].l
            {
                boxes.swap(upper, lower);
            } else {
                break;
            }
            if j == 0 {
                break;
            }
            j -= 1;
        }
    }
}

/// A candidate enclosing rectangle: its area, corners and shorter side.
type RectCandidate = (f32, [(f32, f32); 4], f32);

/// `cv2.minAreaRect` over a point set (rotating calipers on the convex hull):
/// the four corners of the smallest enclosing rectangle and its shorter side,
/// in the order `get_mini_boxes` returns (top-left, top-right, bottom-right,
/// bottom-left, for an upright box). `None` for fewer than one point.
pub fn min_area_rect(points: &[(f32, f32)]) -> Option<([(f32, f32); 4], f32)> {
    let hull = convex_hull(points);
    if hull.is_empty() {
        return None;
    }
    if hull.len() <= 2 {
        // A single pixel or a straight run: a degenerate rectangle along it.
        let (a, b) = (hull[0], *hull.last().unwrap());
        return Some((order_corners([a, b, b, a]), 0.0));
    }
    let mut best: Option<RectCandidate> = None;
    for i in 0..hull.len() {
        let (p, q) = (hull[i], hull[(i + 1) % hull.len()]);
        let (ex, ey) = (q.0 - p.0, q.1 - p.1);
        let len = (ex * ex + ey * ey).sqrt();
        if len < 1e-6 {
            continue;
        }
        let (ux, uy) = (ex / len, ey / len);
        let (vx, vy) = (-uy, ux);
        let (mut umin, mut umax, mut vmin, mut vmax) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
        for &(x, y) in &hull {
            let u = x * ux + y * uy;
            let v = x * vx + y * vy;
            umin = umin.min(u);
            umax = umax.max(u);
            vmin = vmin.min(v);
            vmax = vmax.max(v);
        }
        let area = (umax - umin) * (vmax - vmin);
        if best.as_ref().is_none_or(|(a, _, _)| area < *a) {
            let corner = |u: f32, v: f32| (u * ux + v * vx, u * uy + v * vy);
            let corners = [
                corner(umin, vmin),
                corner(umax, vmin),
                corner(umax, vmax),
                corner(umin, vmax),
            ];
            best = Some((area, corners, (umax - umin).min(vmax - vmin)));
        }
    }
    best.map(|(_, corners, sside)| (order_corners(corners), sside))
}

/// `get_mini_boxes`' corner order: sort by x, then the left pair top-first and
/// the right pair top-first → `[tl, tr, br, bl]`.
pub(crate) fn order_corners(mut c: [(f32, f32); 4]) -> [(f32, f32); 4] {
    c.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (i1, i4) = if c[1].1 > c[0].1 { (0, 1) } else { (1, 0) };
    let (i2, i3) = if c[3].1 > c[2].1 { (2, 3) } else { (3, 2) };
    [c[i1], c[i2], c[i3], c[i4]]
}

/// Andrew's monotone chain; counter-clockwise, no collinear duplicates.
fn convex_hull(points: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let mut pts: Vec<(f32, f32)> = points.to_vec();
    pts.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    let cross = |o: (f32, f32), a: (f32, f32), b: (f32, f32)| {
        (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
    };
    let mut lower: Vec<(f32, f32)> = Vec::new();
    for &p in &pts {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0.0 {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper: Vec<(f32, f32)> = Vec::new();
    for &p in pts.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0.0 {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

/// `box_score_fast`: mean probability over the pixels inside the rectangle
/// (a convex quad — the pixel-center test replaces `cv2.fillPoly`).
fn box_score_fast(prob: &[f32], w: usize, h: usize, quad: &[(f32, f32); 4]) -> f32 {
    let xmin = quad
        .iter()
        .map(|p| p.0)
        .fold(f32::MAX, f32::min)
        .floor()
        .clamp(0.0, (w - 1) as f32) as usize;
    let xmax = quad
        .iter()
        .map(|p| p.0)
        .fold(f32::MIN, f32::max)
        .ceil()
        .clamp(0.0, (w - 1) as f32) as usize;
    let ymin = quad
        .iter()
        .map(|p| p.1)
        .fold(f32::MAX, f32::min)
        .floor()
        .clamp(0.0, (h - 1) as f32) as usize;
    let ymax = quad
        .iter()
        .map(|p| p.1)
        .fold(f32::MIN, f32::max)
        .ceil()
        .clamp(0.0, (h - 1) as f32) as usize;
    let (mut sum, mut n) = (0f64, 0usize);
    for y in ymin..=ymax {
        for x in xmin..=xmax {
            if inside_convex(quad, (x as f32, y as f32)) {
                sum += prob[y * w + x] as f64;
                n += 1;
            }
        }
    }
    if n == 0 {
        0.0
    } else {
        (sum / n as f64) as f32
    }
}

/// Point-in-convex-polygon (boundary counts as inside), any winding.
fn inside_convex(quad: &[(f32, f32); 4], p: (f32, f32)) -> bool {
    let mut pos = false;
    let mut neg = false;
    for i in 0..4 {
        let (a, b) = (quad[i], quad[(i + 1) % 4]);
        let cross = (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0);
        pos |= cross > 1e-6;
        neg |= cross < -1e-6;
    }
    !(pos && neg)
}

/// `unclip` + `get_mini_boxes`: grow the rectangle outward by
/// `area · unclip_ratio / perimeter` (the polygon offset of a rectangle is a
/// rounded rectangle whose minimum-area rectangle is the original grown by
/// the offset on every side). Returns the corners and the shorter side.
fn unclip(quad: &[(f32, f32); 4]) -> Option<([(f32, f32); 4], f32)> {
    let side = |a: (f32, f32), b: (f32, f32)| ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
    let (wlen, hlen) = (side(quad[0], quad[1]), side(quad[1], quad[2]));
    let perimeter = 2.0 * (wlen + hlen);
    if perimeter < 1e-6 {
        return None;
    }
    let d = wlen * hlen * UNCLIP_RATIO / perimeter;
    let (cx, cy) = (
        quad.iter().map(|p| p.0).sum::<f32>() / 4.0,
        quad.iter().map(|p| p.1).sum::<f32>() / 4.0,
    );
    // Unit axes of the rectangle.
    let (ux, uy) = if wlen > 1e-6 {
        (
            (quad[1].0 - quad[0].0) / wlen,
            (quad[1].1 - quad[0].1) / wlen,
        )
    } else {
        (1.0, 0.0)
    };
    let (vx, vy) = if hlen > 1e-6 {
        (
            (quad[2].0 - quad[1].0) / hlen,
            (quad[2].1 - quad[1].1) / hlen,
        )
    } else {
        (-uy, ux)
    };
    let (hw, hh) = (wlen / 2.0 + d, hlen / 2.0 + d);
    let corner = |su: f32, sv: f32| {
        (
            cx + su * hw * ux + sv * hh * vx,
            cy + su * hw * uy + sv * hh * vy,
        )
    };
    let corners = order_corners([
        corner(-1.0, -1.0),
        corner(1.0, -1.0),
        corner(1.0, 1.0),
        corner(-1.0, 1.0),
    ]);
    Some((corners, (wlen + 2.0 * d).min(hlen + 2.0 * d)))
}

/// The detected lines the recognizer still has to read (#429): those not
/// already covered by the region-scoped pass. `detected` is in image pixels
/// at `scale` px/pt; `regions` and `cells` are the page's layout regions and
/// the cells recognized so far, in page points. A line counts as covered when
/// it lies mostly (> 50 %) inside a text-like or table region — whose lines
/// the region pass segmented and recognized, whatever it made of them — when
/// the recognized cells inside it sum to > 30 % of its area, or when a line
/// accepted earlier in reading order overlaps it by > 30 %. Cumulative on
/// purpose: DB happily spans two adjacent columns in one box, and judged cell
/// by cell such a box overlaps every cell a little while re-reading all of
/// them (the `old_newspaper` scan doubled a paragraph that way); and DB can
/// emit a word and its whole line. Returns `text` regions in page points for
/// the recognizer's line prep. Shared by the native worker and the browser
/// pipeline.
pub fn uncovered_lines(
    detected: &[DetBox],
    scale: f32,
    regions: &[crate::layout::Region],
    cells: &[crate::pdfium_backend::TextCell],
) -> Vec<crate::layout::Region> {
    let mut accepted: Vec<crate::layout::Region> = Vec::new();
    for d in detected.iter().map(|d| crate::layout::Region {
        label: "text",
        score: d.score,
        l: d.l / scale,
        t: d.t / scale,
        r: d.r / scale,
        b: d.b / scale,
    }) {
        let da = ((d.r - d.l) * (d.b - d.t)).max(1.0);
        let inter = |l: f32, t: f32, r: f32, b: f32| {
            (d.r.min(r) - d.l.max(l)).max(0.0) * (d.b.min(b) - d.t.max(t)).max(0.0)
        };
        let in_region = regions.iter().any(|r| {
            (crate::ocr_prep::is_text_label(r.label) || crate::assemble::is_table_like(r.label))
                && inter(r.l, r.t, r.r, r.b) / da > 0.5
        });
        let by_cells: f32 = cells.iter().map(|c| inter(c.l, c.t, c.r, c.b)).sum::<f32>() / da;
        let by_accepted = accepted
            .iter()
            .any(|u| inter(u.l, u.t, u.r, u.b) / da > 0.3);
        if !in_region && by_cells <= 0.3 && !by_accepted {
            accepted.push(d);
        }
    }
    accepted
}

#[cfg(feature = "ml")]
pub use session::DetModel;

#[cfg(feature = "ml")]
mod session {
    use super::{db_boxes, prep_det_input, DetBox};
    use image::RgbImage;
    use ort::session::Session;
    use ort::value::Tensor;

    /// The detector session. Loaded lazily by the page worker alongside the
    /// recognizer; absent model → the pipeline runs recognition-only.
    pub struct DetModel {
        session: Session,
    }

    /// `DOCLING_OCR_DET_ONNX`, else `.models/ocr_det.onnx` through the asset
    /// resolver (CWD, `DOCLING_RS_MODELS_DIR`, exe dir).
    pub(crate) fn resolve_det_path() -> String {
        docling_core::env::nonempty("DOCLING_OCR_DET_ONNX")
            .unwrap_or_else(|| crate::resolve_asset(".models/ocr_det.onnx"))
    }

    impl DetModel {
        /// Load the detector with `intra` intra-op threads (the worker's layout
        /// thread budget; DB is a plain conv net, so the output is stable
        /// across thread counts to the precision a 0.3 threshold sees).
        pub fn load(intra: usize) -> Result<Self, String> {
            let path = resolve_det_path();
            if !std::path::Path::new(&path).exists() {
                return Err(format!("text detection model not found at {path}"));
            }
            let builder = docling_onnx::session_builder()
                .map_err(|e| format!("ocr-det: builder: {e}"))?
                .with_intra_threads(intra.max(1))
                .map_err(|e| format!("ocr-det: intra_threads: {e}"))?;
            let builder = docling_onnx::apply(builder).map_err(|e| format!("ocr-det: {e}"))?;
            let session = docling_onnx::commit(builder, &path, "det")
                .map_err(|e| format!("ocr-det: load {path}: {e}"))?;
            Ok(Self { session })
        }

        /// Detect text lines on `img`; boxes in `img` pixels, reading order.
        pub fn detect(&mut self, img: &RgbImage) -> Result<Vec<DetBox>, String> {
            let Some((data, w, h)) = crate::timing::timed("ocr.det.prep", || prep_det_input(img))
            else {
                return Ok(Vec::new());
            };
            let input = Tensor::from_array(([1usize, 3, h as usize, w as usize], data))
                .map_err(|e| format!("ocr-det: input: {e}"))?;
            let name = self.session.inputs()[0].name().to_string();
            let outputs = crate::timing::timed("ocr.det.net", || {
                self.session
                    .run(ort::inputs![name.as_str() => input])
                    .map_err(|e| format!("ocr-det: run: {e}"))
            })?;
            let (shape, prob) = outputs[0]
                .try_extract_tensor::<f32>()
                .map_err(|e| format!("ocr-det: output: {e}"))?;
            let dims: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
            let (ph, pw) = match dims.as_slice() {
                [_, _, ph, pw] => (*ph, *pw),
                _ => return Err(format!("ocr-det: unexpected output shape {dims:?}")),
            };
            Ok(crate::timing::timed("ocr.det.post", || {
                db_boxes(prob, pw, ph, img.width(), img.height())
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_size_scales_the_short_side_to_736_in_multiples_of_32() {
        // RapidOCR's uncapped rule. 445 × 884: short side 445 → ×1.654;
        // 736 × 1462 → 736 × 1472.
        assert_eq!(det_input_size_capped(445, 884, 0), Some((736, 1472)));
        // Already ≥ 736 on the short side: unchanged bar the /32 rounding.
        assert_eq!(det_input_size_capped(1335, 2652, 0), Some((1344, 2656)));
        assert_eq!(det_input_size(0, 10), None);
        // The default cap (2000, RapidOCR's) applies when the env knob is
        // unset — a Letter render stays at its shorter-side-rule size.
        assert_eq!(det_input_size(1224, 1584), Some((1216, 1600)));
        // A longer-side cap scales a big page down (1224 × 1584 → 736 × 960
        // for 960) and leaves a small image's shorter-side upscale alone
        // (445 × 884 still goes to 736 × 1472 under a 1500 cap, 480 × 960
        // under 960).
        assert_eq!(det_input_size_capped(1224, 1584, 960), Some((736, 960)));
        assert_eq!(det_input_size_capped(445, 884, 1500), Some((736, 1472)));
        assert_eq!(det_input_size_capped(445, 884, 960), Some((480, 960)));
        assert_eq!(det_input_size_capped(1224, 1584, 0), Some((1216, 1600)));
    }

    #[test]
    fn det_input_is_bgr_normalized() {
        let mut img = RgbImage::new(736, 736);
        img.put_pixel(0, 0, image::Rgb([255, 0, 128]));
        let (data, w, h) = prep_det_input(&img).unwrap();
        assert_eq!((w, h), (736, 736));
        let n = (w * h) as usize;
        // Blue plane first: pixel (0,0) has B=128 → ~0.0039; G=0 → -1; R=255 → 1.
        assert!((data[0] - (128.0 / 127.5 - 1.0)).abs() < 1e-6);
        assert_eq!(data[n], -1.0);
        assert_eq!(data[2 * n], 1.0);
    }

    #[test]
    fn min_area_rect_of_an_upright_and_a_tilted_blob() {
        let pts: Vec<(f32, f32)> = (0..20)
            .flat_map(|x| (0..5).map(move |y| (x as f32, y as f32)))
            .collect();
        let (c, sside) = min_area_rect(&pts).unwrap();
        assert!((sside - 4.0).abs() < 1e-3);
        assert!(
            (c[0].0 - 0.0).abs() < 1e-3 && (c[0].1 - 0.0).abs() < 1e-3,
            "{c:?}"
        );
        assert!(
            (c[2].0 - 19.0).abs() < 1e-3 && (c[2].1 - 4.0).abs() < 1e-3,
            "{c:?}"
        );
        // The same strip rotated 45°: the tight rectangle is ~19 × 4, not the
        // axis-aligned 16 × 16 box.
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let rot: Vec<(f32, f32)> = pts
            .iter()
            .map(|&(x, y)| (x * s - y * s + 50.0, x * s + y * s + 50.0))
            .collect();
        let (_, sside) = min_area_rect(&rot).unwrap();
        assert!((sside - 4.0).abs() < 1e-2, "{sside}");
    }

    /// Two text-like blobs on a probability map → two boxes, grown by the
    /// unclip distance (area·1.6/perimeter) on every side, in reading order,
    /// scaled to the destination image; a faint blob below `box_thresh` and a
    /// speck below `min_size` are dropped.
    #[test]
    fn db_boxes_from_a_synthetic_probability_map() {
        let (w, h) = (128usize, 64usize);
        let mut prob = vec![0f32; w * h];
        let blob = |prob: &mut Vec<f32>, l: usize, t: usize, r: usize, b: usize, p: f32| {
            for y in t..b {
                for x in l..r {
                    prob[y * w + x] = p;
                }
            }
        };
        blob(&mut prob, 70, 10, 110, 20, 0.9); // right, upper row
        blob(&mut prob, 10, 12, 50, 22, 0.9); // left, same row (top within 10)
        blob(&mut prob, 10, 40, 60, 48, 0.35); // above thresh but mean < box_thresh
        blob(&mut prob, 100, 50, 102, 52, 0.9); // speck
        let boxes = db_boxes(&prob, w, h, 256, 128);
        assert_eq!(boxes.len(), 2, "{boxes:?}");
        // Left blob first (same row, smaller x). Its rectangle spans pixel
        // centers 10..49 × 12..21 (39 × 9 after dilation shifts by one:
        // 10..50 × 12..22 → 40 × 10), unclip d = 400·1.6/100 = 6.4.
        let a = &boxes[0];
        assert!(a.l < boxes[1].l);
        // The dilated rectangle takes in one zero-probability rim row and
        // column, so the mean sits a little under the blob's 0.9 — RapidOCR
        // scores the dilated contour on the raw map the same way.
        assert!(a.score > 0.75 && a.score < 0.9, "{}", a.score);
        // Doubled for the 2× destination scale: l ≈ (10 − 6.4)·2, r ≈ (50 + 6.4)·2.
        assert!(
            (a.l - 7.0).abs() <= 2.0 && (a.r - 113.0).abs() <= 2.0,
            "{a:?}"
        );
        assert!(
            (a.t - 11.0).abs() <= 2.0 && (a.b - 57.0).abs() <= 2.0,
            "{a:?}"
        );
    }

    /// The coverage rule: a line inside a text region, one whose area
    /// recognized cells mostly fill (cumulatively — two half-covering cells
    /// count), and a duplicate of an accepted line are all skipped; a line in
    /// the open is kept, converted to page points.
    #[test]
    fn uncovered_lines_skip_what_the_region_pass_read() {
        use crate::layout::Region;
        use crate::pdfium_backend::TextCell;
        let bx = |l: f32, t: f32, r: f32, b: f32| DetBox::rect(l, t, r, b, 0.9);
        let regions = vec![Region {
            label: "text",
            score: 0.9,
            l: 0.0,
            t: 0.0,
            r: 100.0,
            b: 20.0,
        }];
        let cell = |l: f32, r: f32| TextCell {
            text: "x".into(),
            l,
            t: 50.0,
            r,
            b: 60.0,
        };
        let cells = vec![cell(0.0, 50.0), cell(50.0, 100.0)];
        let detected = vec![
            bx(0.0, 0.0, 200.0, 40.0), // inside the text region (page pts 0..100 × 0..20)
            bx(0.0, 100.0, 200.0, 120.0), // two cells cover it half each → covered
            bx(0.0, 300.0, 200.0, 320.0), // in the open → kept
            bx(20.0, 302.0, 100.0, 318.0), // a word of the accepted line → duplicate
        ];
        let out = uncovered_lines(&detected, 2.0, &regions, &cells);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(
            (out[0].l, out[0].t, out[0].r, out[0].b),
            (0.0, 150.0, 100.0, 160.0)
        );
        assert_eq!(out[0].label, "text");
    }

    #[test]
    fn boxes_sort_by_row_then_column() {
        let bx = |l: f32, t: f32| DetBox::rect(l, t, l + 10.0, t + 10.0, 1.0);
        let mut boxes = vec![
            bx(50.0, 100.0),
            bx(10.0, 105.0),
            bx(30.0, 20.0),
            bx(5.0, 200.0),
        ];
        sort_boxes(&mut boxes);
        let order: Vec<(f32, f32)> = boxes.iter().map(|b| (b.l, b.t)).collect();
        assert_eq!(
            order,
            vec![(30.0, 20.0), (10.0, 105.0), (50.0, 100.0), (5.0, 200.0)]
        );
        // Two columns whose line tops interleave within the threshold (a
        // newspaper): each column's lines must stay top-to-bottom, and a
        // right-column line sorts after the left-column line it shares a
        // baseline with — RapidOCR's result, not one page-wide "row".
        let mut cols = vec![
            bx(97.0, 154.0),
            bx(96.0, 173.0),
            bx(600.0, 160.0),
            bx(97.0, 191.0),
            bx(601.0, 178.0),
            bx(600.0, 197.0),
        ];
        sort_boxes(&mut cols);
        let order: Vec<(f32, f32)> = cols.iter().map(|b| (b.l, b.t)).collect();
        assert_eq!(
            order,
            vec![
                (97.0, 154.0),
                (600.0, 160.0),
                (96.0, 173.0),
                (601.0, 178.0),
                (97.0, 191.0),
                (600.0, 197.0)
            ]
        );
    }
}
