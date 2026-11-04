//! Content-based page-orientation detection for the OCR path (#225).
//!
//! `/Rotate` normalization (see `pdfium_backend::extract_page`) only helps
//! when the rotation is *declared*: a phone photo taken sideways or a sheet
//! fed into the scanner in landscape has `/Rotate 0` and looks upright to the
//! PDF layer, so layout and OCR run on a sideways/upside-down raster and
//! recognize noise — silently. This module detects the raster's own
//! orientation and reports the angle to un-rotate by, composing with the
//! metadata normalization through the same [`PdfPage::unrotate`] machinery.
//!
//! There is no orientation model to run. Instead the recognizer itself is
//! the judge, the classic OSD trick: take the page's text lines, recognize a
//! handful of the widest under each orientation hypothesis, and score each
//! hypothesis by how much confident text falls out. Upright text decodes
//! many characters at high confidence; sideways/upside-down text decodes
//! few, badly. An upright page early-exits after scoring only its own
//! hypothesis, so the common case pays a few small recognition runs; the
//! probe count is capped, so cost does not grow with page density.
//!
//! The lines come from the PP-OCR text detector when it is installed (#571):
//! its boxes are real text runs, which upright read at the confidence a
//! recognizer gives real text, and which rotated with the page into each
//! hypothesis read as the garbage they then are — a clean signal. The
//! projection strips the probe started with (#225) are the fallback without
//! the detector: on a sparse form — fields spread over a baseline,
//! handwriting, stamps, boxes — a strip spans several fields and reads at a
//! confidence no better than garbage, and on 9 of FUNSD's 199 upright forms
//! the vote then went to a rotation, losing most of their text. The decision
//! also asks that the winner read *better*, not merely *more*: a rotated
//! hypothesis must beat 0° in mean confidence by a margin, the "rotated page
//! reads better than the original" check an upright page cannot fail.
//!
//! Scores are deterministic (single-threaded recognition, fixed probe
//! selection), so pinned snapshots stay stable. Failures degrade to "assume
//! upright" — detection must never make a conversion worse than not having
//! run at all. `DOCLING_RS_OCR_ORIENTATION=off` disables the pass;
//! `DOCLING_RS_DEBUG=1` prints per-hypothesis scores.
//!
//! [`PdfPage::unrotate`]: crate::PdfPage::unrotate

use image::imageops::{rotate180, rotate270, rotate90};
use image::RgbImage;

use crate::layout::Region;
use crate::ocr::OcrModel;
use crate::ocr_det::DetBox;
use crate::ocr_prep::{prep_region_lines, prep_region_lines_det_boxed, PrepLine};
use crate::Recognizer;
use docling_core::debug_log;

/// Whether the detection pass runs (`DOCLING_RS_OCR_ORIENTATION`, default
/// `auto`; `off`/`0`/`false` disable, anything else warns and stays auto).
pub(crate) fn enabled() -> bool {
    let raw = docling_core::env::nonempty("DOCLING_RS_OCR_ORIENTATION").unwrap_or_default();
    let v = raw.trim().to_ascii_lowercase();
    match v.as_str() {
        "" | "auto" | "on" | "1" | "true" => true,
        "off" | "0" | "false" | "none" => false,
        _ => {
            eprintln!(
                "docling-pdf: DOCLING_RS_OCR_ORIENTATION={raw:?} is not auto|off; using auto"
            );
            true
        }
    }
}

/// How many line crops one hypothesis recognizes at most. The widest lines
/// carry the most characters — a handful is plenty of signal, and the cap
/// keeps the pass O(1) recognition runs regardless of page size.
const PROBES: usize = 6;

/// Accept "upright" without probing the other three hypotheses when the 0°
/// probe alone reads at least this confidently. Covers the overwhelmingly
/// common case (a correctly scanned page) at the cost of one probe round.
const UPRIGHT_CONF: f32 = 0.90;
const UPRIGHT_CHARS: usize = 20;

/// A rotated hypothesis must beat 0° by this factor to overturn it: the
/// recognizer is noisy on garbage input, and a no-op must stay the default
/// when the signal is thin.
const OVERTURN: f32 = 1.2;
/// ...and read more *confidently* than 0° by this much (#571): a page that
/// really is rotated reads at garbage confidence upright and at text
/// confidence in the right hypothesis — a wide gap — while an upright page
/// whose strips read poorly (sparse forms, handwriting, stamps) gives every
/// hypothesis about the same low confidence, and the character count alone
/// then decided for a rotation.
const CONF_MARGIN: f32 = 0.10;
/// ...and clear this floor: a page whose *best* hypothesis still reads almost
/// nothing (blank page, pure line-art) has no orientation evidence at all.
const MIN_CHARS: usize = 8;
const MIN_CONF: f32 = 0.55;
/// [`overturns`]: a rotated hypothesis reading this many times the upright
/// characters at [`UPRIGHT_CONF`] wins without the confidence margin.
const DOMINANT: usize = 4;

/// One hypothesis' evidence: Σ(confidence × chars) over its probe lines, and
/// the raw character count.
struct Score {
    weighted: f32,
    chars: usize,
}

impl Score {
    fn mean_conf(&self) -> f32 {
        if self.chars == 0 {
            0.0
        } else {
            self.weighted / self.chars as f32
        }
    }
}

/// Cut `img` into line crops — the detector's `boxes` (in `img` pixels) when
/// there are any, the projection strips otherwise — and score the `PROBES`
/// widest through the recognizer.
fn probe(img: &RgbImage, ocr: &mut OcrModel, boxes: &[DetBox]) -> Result<Score, String> {
    // The whole page as one text region, in image-pixel "points" (scale 1.0):
    // the same line prep OCR uses per layout region, minus the layout model
    // — which cannot be trusted on the very pages this pass is for.
    let page = Region {
        label: "text",
        score: 1.0,
        l: 0.0,
        t: 0.0,
        r: img.width() as f32,
        b: img.height() as f32,
    };
    // The *boxed* crops, not the page OCR's warped ones (#981): the probe
    // asks whether the raster is sideways, and `rotate_crop` would answer
    // it before the question is put — a sideways line's tall box comes out
    // of the warp already turned upright, so the 0° hypothesis reads a 90°
    // page fine and the page is never un-rotated.
    let (_, mut lines) = crate::timing::timed("orient.prep", || {
        if boxes.is_empty() {
            prep_region_lines(img, std::slice::from_ref(&page), 1.0)
        } else {
            prep_region_lines_det_boxed(img, std::slice::from_ref(&page), 1.0, boxes)
        }
    });
    // Widest first — most characters per recognition run. Stable order (width,
    // then original index) keeps the selection deterministic.
    let mut order: Vec<usize> = (0..lines.len()).collect();
    order.sort_by(|&a, &b| lines[b].w.cmp(&lines[a].w).then(a.cmp(&b)));
    order.truncate(PROBES);
    order.sort_unstable();
    // Extract by descending index so earlier indices stay valid.
    let probes: Vec<PrepLine> = order.iter().rev().map(|&i| lines.swap_remove(i)).collect();
    let (weighted, chars) = crate::timing::timed("orient.score", || ocr.score_lines(&probes))?;
    Ok(Score { weighted, chars })
}

/// Detect the clockwise angle the page content is rotated by in the raster
/// (`0`/`90`/`180`/`270`); un-rotating by the returned angle makes it upright.
/// Any internal failure returns 0 — the page converts as-is, exactly as it
/// would have without this pass. The PP-OCR engine runs the four-way probe
/// below on the text detector's `boxes` (in `img` pixels; empty = the
/// projection strips); Tesseract (#460) asks its own OSD (`scale` is the
/// px/pt of `img`, its dpi hint).
pub(crate) fn detect(img: &RgbImage, ocr: &mut Recognizer, scale: f32, boxes: &[DetBox]) -> u16 {
    match ocr {
        Recognizer::PpOcr(model) => detect_by_probe(img, model, boxes),
        Recognizer::Tesseract(t) => t.detect_orientation(img, scale).unwrap_or(0),
    }
}

/// `boxes` (in the pixels of a `w × h` image) as they land on that image
/// un-rotated by `deg` — the `PdfPage::unrotate` convention: `rotate270` for
/// 90, `rotate180`, `rotate90` for 270. A sideways line's tall narrow box
/// comes out as the wide box the recognizer expects.
pub(crate) fn rotate_boxes(boxes: &[DetBox], w: f32, h: f32, deg: u16) -> Vec<DetBox> {
    boxes
        .iter()
        .map(|d| {
            let (l, t, r, b) = match deg {
                // rotate270 (90° counter-clockwise): (x, y) → (y, w − x).
                90 => (d.t, w - d.r, d.b, w - d.l),
                180 => (w - d.r, h - d.b, w - d.l, h - d.t),
                // rotate90 (90° clockwise): (x, y) → (h − y, x).
                270 => (h - d.b, d.l, h - d.t, d.r),
                _ => (d.l, d.t, d.r, d.b),
            };
            // The quad's corners follow the same pixels (#666).
            let point = |(x, y): (f32, f32)| match deg {
                90 => (y, w - x),
                180 => (w - x, h - y),
                270 => (h - y, x),
                _ => (x, y),
            };
            DetBox {
                l,
                t,
                r,
                b,
                score: d.score,
                quad: crate::ocr_det::order_corners(d.quad.map(point)),
            }
        })
        .collect()
}

/// The recognize-four-ways probe, see the module docs.
fn detect_by_probe(img: &RgbImage, ocr: &mut OcrModel, boxes: &[DetBox]) -> u16 {
    let fail = |e: String| {
        debug_log!("docling-pdf: orientation probe failed ({e}); assuming upright");
        0
    };
    let s0 = match probe(img, ocr, boxes) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    if s0.chars >= UPRIGHT_CHARS && s0.mean_conf() >= UPRIGHT_CONF {
        debug_log!(
            "docling-pdf: orientation 0° reads {} chars at {:.2} — upright, no probes",
            s0.chars,
            s0.mean_conf()
        );
        return 0;
    }
    // Hypothesis "content is rotated `deg`° clockwise" ⇒ test the bitmap
    // un-rotated by `deg` (the inverse), same convention as `PdfPage::unrotate`.
    let hypotheses: [(u16, RgbImage); 3] = [
        (90, rotate270(img)),
        (180, rotate180(img)),
        (270, rotate90(img)),
    ];
    debug_log!(
        "docling-pdf: orientation 0°: {} chars at {:.2} (weighted {:.1})",
        s0.chars,
        s0.mean_conf(),
        s0.weighted
    );
    let (mut best_deg, mut best) = (
        0u16,
        Score {
            weighted: 0.0,
            chars: 0,
        },
    );
    let (w, h) = (img.width() as f32, img.height() as f32);
    for (deg, rotated) in &hypotheses {
        let s = match probe(rotated, ocr, &rotate_boxes(boxes, w, h, *deg)) {
            Ok(s) => s,
            Err(e) => return fail(e),
        };
        debug_log!(
            "docling-pdf: orientation {deg}°: {} chars at {:.2} (weighted {:.1})",
            s.chars,
            s.mean_conf(),
            s.weighted
        );
        if s.weighted > best.weighted {
            (best_deg, best) = (*deg, s);
        }
    }
    if best_deg != 0 && overturns(&s0, &best) {
        return best_deg;
    }
    0
}

/// Whether the best rotated hypothesis `best` overturns "upright" `s0`.
/// Only on clear evidence: the winner must read real text (floors), beat
/// the 0° hypothesis by a margin, *and* read more confidently than it (#571)
/// — more characters at the same poor confidence is a sparse upright page,
/// not a rotated one — unless it reads [`UPRIGHT_CONF`]-grade text and
/// [`DOMINANT`]× the characters: a page whose upright layer is one confident
/// `Page 1` footer beside a sideways table (#936) read 9 chars at 0.90
/// upright against 429 at 0.99 turned, and whether the margin held hinged on
/// the footer's second decimal (0.88 at one detector resolution, 0.90 at
/// another, #945). That many confident characters is not the #571 case.
fn overturns(s0: &Score, best: &Score) -> bool {
    best.chars >= MIN_CHARS
        && best.mean_conf() >= MIN_CONF
        && best.weighted > OVERTURN * s0.weighted
        && (best.mean_conf() >= s0.mean_conf() + CONF_MARGIN
            || (best.mean_conf() >= UPRIGHT_CONF && best.chars >= DOMINANT * s0.chars.max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(chars: usize, conf: f32) -> Score {
        Score {
            weighted: conf * chars as f32,
            chars,
        }
    }

    /// The overturn rule: #571's sparse upright page (more characters at the
    /// same poor confidence) stays upright; #936's confident footer beside a
    /// sideways table turns whether the footer reads 0.88 or 0.90.
    #[test]
    fn overturn_needs_a_confidence_margin_or_dominant_confident_text() {
        assert!(!overturns(&score(40, 0.62), &score(90, 0.63)));
        assert!(!overturns(&score(40, 0.62), &score(300, 0.70)));
        assert!(overturns(&score(10, 0.88), &score(464, 0.99)));
        assert!(overturns(&score(9, 0.90), &score(429, 0.99)));
        // Confident but not dominant: a near-tie needs the margin.
        assert!(!overturns(&score(100, 0.90), &score(130, 0.92)));
        // Floors still apply.
        assert!(!overturns(&score(1, 0.5), &score(6, 0.99)));
    }

    /// A box follows the pixels through each un-rotation: on a 100 × 60 image
    /// the line at (10, 5)–(50, 15) lands where `rotate270` / `rotate180` /
    /// `rotate90` put those pixels, and un-rotating by 0 is the identity.
    #[test]
    fn boxes_rotate_with_the_image() {
        let bx = DetBox::rect(10.0, 5.0, 50.0, 15.0, 0.9);
        let at = |deg: u16| {
            let r = rotate_boxes(std::slice::from_ref(&bx), 100.0, 60.0, deg);
            (r[0].l, r[0].t, r[0].r, r[0].b)
        };
        assert_eq!(at(0), (10.0, 5.0, 50.0, 15.0));
        // rotate270: (x, y) → (y, 100 − x): x 10..50 → y 50..90, y 5..15 → x 5..15.
        assert_eq!(at(90), (5.0, 50.0, 15.0, 90.0));
        assert_eq!(at(180), (50.0, 45.0, 90.0, 55.0));
        // rotate90: (x, y) → (60 − y, x): y 5..15 → x 45..55, x 10..50 → y 10..50.
        assert_eq!(at(270), (45.0, 10.0, 55.0, 50.0));
        // Pixel check: the image crate agrees on where a marked pixel goes.
        let mut img = RgbImage::new(100, 60);
        img.put_pixel(10, 5, image::Rgb([255, 0, 0]));
        let r = rotate270(&img);
        assert_eq!(
            r.get_pixel(5, 89)[0],
            255,
            "rotate270 maps (10,5) → (5, 89)"
        );
        let r = rotate90(&img);
        assert_eq!(
            r.get_pixel(54, 10)[0],
            255,
            "rotate90 maps (10,5) → (54, 10)"
        );
    }
}
