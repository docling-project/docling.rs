//! #1250: a raster drawn as hundreds of 1-pixel-tall inline images, each
//! under its own rectangular clip (what a PostScript driver + Ghostscript
//! print of a screenshot looks like), renders with faint seams wherever a
//! strip boundary falls mid-pixel. The ink-projection line split read every
//! seam period of such a picture as a 4-px "text line" as wide as the
//! picture, each stretched to ~8 200 px for the recognizer — the speculative
//! picture OCR then spent ~10 GB and two minutes on a picture whose text it
//! threw away. The split must see the striped picture as what it is: not
//! dozens of thin full-width lines.
#![cfg(feature = "render")]

#[path = "support/one_page_pdf.rs"]
mod one_page_pdf;

use docling_pdf::ocr_prep::{prep_line, segment_lines};
use image::imageops;

/// Image pixels per side and the strips' height in points (0.6 pt = 1.2 px at
/// the 2.0 render scale: four of every five boundaries fall mid-pixel).
const W: usize = 576;
const H: usize = 480;
const STRIP_PT: f32 = 0.6;
const X0: f32 = 40.0;
const Y0: f32 = 300.0;

/// The issue's "terminal window": a near-black image with a few light
/// text-like bars on it.
fn pixel(x: usize, y: usize) -> [u8; 3] {
    let bar = (x % 60) < 30 && y > 8 && y < 470;
    if bar {
        [0xc8, 0xc8, 0xc8]
    } else {
        [0x14, 0x14, 0x14]
    }
}

/// A one-page PDF drawing the image either as `H` clipped strips (`clipped`)
/// or as `H` unclipped strips.
fn strips_pdf(clipped: bool) -> Vec<u8> {
    let mut content: Vec<u8> =
        b"BT /F1 12 Tf 40 700 Td (A few lines of real text) Tj ET\n".to_vec();
    let w_pt = W as f32 * STRIP_PT;
    for row in 0..H {
        // Row 0 is the image's top.
        let y = Y0 + (H - 1 - row) as f32 * STRIP_PT;
        content.extend_from_slice(b"q ");
        if clipped {
            content.extend_from_slice(
                format!(
                    "{X0} {y} m {} {y} l {} {} l {X0} {} l h W n ",
                    X0 + w_pt,
                    X0 + w_pt,
                    y + STRIP_PT,
                    y + STRIP_PT
                )
                .as_bytes(),
            );
        }
        content.extend_from_slice(
            format!(
                "{w_pt} 0 0 {STRIP_PT} {X0} {y} cm BI /W {W} /H 1 /CS /RGB /BPC 8 /F /AHx ID\n"
            )
            .as_bytes(),
        );
        for x in 0..W {
            for c in pixel(x, row) {
                content.extend_from_slice(format!("{c:02x}").as_bytes());
            }
        }
        content.extend_from_slice(b">\nEI Q\n");
    }
    one_page_pdf::pdf_bytes(&content)
}

/// The picture's crop of the 2.0-scale render, and the recognizer widths its
/// projection lines would be stretched to.
fn picture_lines(pdf: &[u8]) -> (usize, Vec<usize>) {
    let pages = docling_pdf::render_pages(pdf, None, None, 2.0).expect("render");
    let page = &pages[0];
    let img = image::load_from_memory(&page.png).expect("png").to_rgb8();
    let s = 2.0f32;
    let (page_h, w_pt, h_pt) = (842.0f32, W as f32 * STRIP_PT, H as f32 * STRIP_PT);
    let l = (X0 * s) as u32;
    let t = ((page_h - Y0 - h_pt) * s) as u32;
    let crop = imageops::crop_imm(&img, l, t, (w_pt * s) as u32, (h_pt * s) as u32).to_image();
    let lines = segment_lines(&crop);
    let widths = lines
        .iter()
        .filter_map(|&(lx, ly, rx, ry)| {
            prep_line(&imageops::crop_imm(&crop, lx, ly, rx - lx, ry - ly).to_image()).map(|p| p.w)
        })
        .collect();
    (lines.len(), widths)
}

#[test]
fn clipped_strips_do_not_split_into_seam_lines() {
    let (n_unclipped, w_unclipped) = picture_lines(&strips_pdf(false));
    let (n_clipped, w_clipped) = picture_lines(&strips_pdf(true));
    eprintln!("unclipped: {n_unclipped} lines {w_unclipped:?}");
    eprintln!("clipped: {n_clipped} lines {w_clipped:?}");
    let total: usize = w_clipped.iter().sum();
    let total_unclipped: usize = w_unclipped.iter().sum();
    assert!(
        total <= 2 * total_unclipped.max(512),
        "clipped strips: {n_clipped} lines, {total} px of recognizer width \
         (unclipped: {n_unclipped} lines, {total_unclipped} px)"
    );
}
