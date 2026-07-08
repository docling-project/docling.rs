//! Pixel-exact reimplementations of the OpenCV resize kernels docling uses for
//! TableFormer preprocessing, so the model sees byte-identical input. Verified
//! against cv2 on docling's own bitmaps (INTER_AREA max diff 1/255, INTER_LINEAR
//! < 1e-4 in float).

use image::{Rgb, RgbImage};

/// Per-output-pixel source spans + overlap weights for area resampling.
fn area_weights(src: usize, dst: usize, scale: f64) -> Vec<Vec<(usize, f64)>> {
    (0..dst)
        .map(|d| {
            let f1 = d as f64 * scale;
            let f2 = (d + 1) as f64 * scale;
            let s1 = f1.floor() as usize;
            let s2 = (f2.ceil() as usize).min(src);
            (s1..s2)
                .map(|si| {
                    let w = (((si + 1) as f64).min(f2) - (si as f64).max(f1)) / scale;
                    (si, w)
                })
                .collect()
        })
        .collect()
}

/// `cv2.resize(..., interpolation=INTER_AREA)` for shrinking — area-weighted
/// averaging, separable (horizontal then vertical), f64 accumulation.
pub fn inter_area(src: &RgbImage, dw: u32, dh: u32) -> RgbImage {
    let (sw, sh) = (src.width() as usize, src.height() as usize);
    let (dwu, dhu) = (dw as usize, dh as usize);
    let hw = area_weights(sw, dwu, sw as f64 / dw as f64);
    let vw = area_weights(sh, dhu, sh as f64 / dh as f64);

    let mut tmp = vec![[0f64; 3]; sh * dwu]; // (sh × dw)
    for y in 0..sh {
        let row = y * dwu;
        for (dx, ws) in hw.iter().enumerate() {
            let mut acc = [0f64; 3];
            for &(si, w) in ws {
                let p = src.get_pixel(si as u32, y as u32);
                acc[0] += p[0] as f64 * w;
                acc[1] += p[1] as f64 * w;
                acc[2] += p[2] as f64 * w;
            }
            tmp[row + dx] = acc;
        }
    }
    let mut out = RgbImage::new(dw, dh);
    for (dy, ws) in vw.iter().enumerate() {
        for dx in 0..dwu {
            let mut acc = [0f64; 3];
            for &(si, w) in ws {
                let t = tmp[si * dwu + dx];
                acc[0] += t[0] * w;
                acc[1] += t[1] * w;
                acc[2] += t[2] * w;
            }
            out.put_pixel(
                dx as u32,
                dy as u32,
                Rgb([round_u8(acc[0]), round_u8(acc[1]), round_u8(acc[2])]),
            );
        }
    }
    out
}

fn round_u8(v: f64) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}
