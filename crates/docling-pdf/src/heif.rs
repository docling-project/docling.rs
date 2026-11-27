//! HEIC/HEIF image decoding (#211) through heifer (#1249): a pure-Rust
//! HEIF container parser and HEVC decoder, `#![forbid(unsafe_code)]` in all
//! three of its crates, no build script, `thiserror` its only dependency. It
//! replaced libheif-rs, which linked the system libheif + libde265 (C/C++),
//! so the feature had to stay opt-in and out of the wasm build.
//!
//! What a HEIC photo reaches layout and OCR as is unchanged:
//! - the primary image, in its display orientation (`clap`, `irot`, `imir`
//!   applied);
//! - its colours as coded — YCbCr → RGB by the file's `nclx` matrix, its ICC
//!   profile not applied, as libheif did and as docling's Pillow +
//!   pillow-heif reads it;
//! - transparency onto white ([`crate::ocr_prep::flatten_onto_white`],
//!   docling's `to_rgb`, #1149).
//!
//! Measured against libheif 1.20 on the official HEIF conformance files: the
//! luma matches to rounding; the RGB differs on a few percent of pixels at
//! sharp colour edges (chroma upsampling), which layout and OCR do not see.

use crate::ocr_prep::flatten_onto_white;

/// The largest picture decoded, in pixels: the image input's 256 MiB
/// allocation budget (`decode_image_with_max_side`) at four bytes a pixel.
const MAX_PIXELS: u64 = 256 * 1024 * 1024 / 4;

/// Decode the primary image of a HEIF/HEIC file to RGB.
///
/// `max_side` is the image input's per-side cap (`DOCLING_RS_MAX_IMAGE_PIXELS`,
/// 30000): checked on the display size the container declares, before a pixel
/// is decoded. A file whose decoder panics fails with an error instead of
/// taking the process down — a young codec, parsing untrusted input.
pub fn decode(bytes: &[u8], max_side: u32) -> Result<image::RgbImage, String> {
    let info = heifer::read_info(bytes).map_err(|e| e.to_string())?;
    if info.width > max_side || info.height > max_side {
        return Err(format!(
            "image dimensions {}x{} exceed the {max_side}px per-side cap \
             (DOCLING_RS_MAX_IMAGE_PIXELS overrides)",
            info.width, info.height
        ));
    }
    let max_pixels = (u64::from(max_side) * u64::from(max_side)).min(MAX_PIXELS);
    let options = heifer::Options::new().with_max_pixels(max_pixels);
    let img = std::panic::catch_unwind(|| heifer::decode_rgba8(bytes, &options))
        .map_err(|_| "the HEIF decoder failed on this file".to_string())?
        .map_err(|e| e.to_string())?;
    let rgba = image::RgbaImage::from_raw(img.width, img.height, img.data)
        .ok_or_else(|| "decoded pixel buffer does not match its size".to_string())?;
    Ok(flatten_onto_white(image::DynamicImage::ImageRgba8(rgba)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/data/heif/sources/inspection_report.heic"
        ))
        .expect("heic fixture")
    }

    #[test]
    fn decodes_the_primary_image() {
        let bytes = fixture();
        assert!(crate::is_heif(&bytes));
        let img = decode(&bytes, 30_000).expect("decodes");
        assert_eq!(img.dimensions(), (1200, 700));
        // A white page with dark text: mostly white, some ink.
        let px = img.as_raw();
        let white = px
            .chunks_exact(3)
            .filter(|p| p.iter().all(|&c| c > 240))
            .count();
        let ink = px
            .chunks_exact(3)
            .filter(|p| p.iter().all(|&c| c < 80))
            .count();
        let n = px.len() / 3;
        assert!(
            white > n / 2 && ink > n / 200,
            "white {white}, ink {ink} of {n}"
        );
    }

    #[test]
    fn the_size_cap_is_checked_before_decoding() {
        let err = decode(&fixture(), 1000).unwrap_err();
        assert!(err.contains("1200x700 exceed the 1000px"), "{err}");
    }

    /// Damaged files are errors, never a crash or a hang: every truncation
    /// point of the header and body, and byte flips across the file.
    #[test]
    fn damaged_files_fail_cleanly() {
        let bytes = fixture();
        let mut cuts: Vec<usize> = (0..bytes.len().min(2048)).step_by(7).collect();
        cuts.extend((2048..bytes.len()).step_by(997));
        for n in cuts {
            let _ = decode(&bytes[..n], 30_000);
        }
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..300 {
            let mut b = bytes.clone();
            for _ in 0..4 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let i = (seed as usize) % b.len();
                b[i] = (seed >> 32) as u8;
            }
            let _ = decode(&b, 30_000);
        }
    }

    #[test]
    fn a_container_without_an_image_is_an_error() {
        let stub = b"\x00\x00\x00\x18ftypheic\0\0\0\0heicmif1";
        assert!(decode(stub, 30_000).is_err());
    }
}
