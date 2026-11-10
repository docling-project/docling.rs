//! Pillow's `Image.info["dpi"]` read straight from an image's bytes — the
//! resolution docling's backends see through PIL: the image input's page size
//! (`ImageDocumentBackend`: `pixels × 72 / dpi` points, #1093) and an embedded
//! picture's `ImageRef.dpi` (python-pptx's `Image.dpi` reads the same field).
//!
//! Only the headers are parsed, never the pixels, and every format is read the
//! way Pillow's plugin reads it:
//!
//! - PNG: a `pHYs` chunk in pixels per metre (`× 0.0254`); a unit-0 chunk
//!   records an aspect ratio only.
//! - JPEG: the JFIF density per inch, or per centimetre (`× 2.54`); with no
//!   JFIF unit, an EXIF segment's `XResolution` / `ResolutionUnit` for both
//!   axes, `(72, 72)` when the EXIF lacks either tag.
//! - BMP: `biXPelsPerMeter` / `biYPelsPerMeter` `÷ 39.3701`.
//! - TIFF: `XResolution` / `YResolution` (each 1 when absent, nothing when
//!   either is 0) in inches (unit 2, or no unit tag), centimetres (3), or no
//!   unit at all (1: Pillow's `resolution`, not `dpi`).
//!
//! Everything else (GIF, WebP, a PNG without `pHYs`) records no resolution.

/// `(horizontal, vertical)` dots per inch as Pillow's `info["dpi"]`, `None`
/// when the format records none. The values are passed through as read — a
/// `(1, 1)` placeholder or a zero density included — for the caller to judge
/// (docling's image backend reads both as 72).
pub fn resolution(data: &[u8]) -> Option<(f64, f64)> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return png(data);
    }
    if data.starts_with(&[0xff, 0xd8]) {
        return jpeg(data);
    }
    if data.starts_with(b"BM") && data.len() >= 42 {
        return bmp(data);
    }
    if data.starts_with(b"II*\0") || data.starts_with(b"MM\0*") {
        return tiff(data);
    }
    None
}

fn png(data: &[u8]) -> Option<(f64, f64)> {
    let mut pos = 8;
    while pos + 8 <= data.len() {
        let be = |o: usize| u32::from_be_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
        let len = be(pos) as usize;
        let kind = &data[pos + 4..pos + 8];
        if kind == b"pHYs" && len >= 9 && pos + 8 + len <= data.len() {
            let (px, py) = (be(pos + 8), be(pos + 12));
            // Unit 1 = metre; unit 0 records an aspect ratio only.
            return (data[pos + 16] == 1).then(|| (f64::from(px) * 0.0254, f64::from(py) * 0.0254));
        }
        if kind == b"IDAT" || kind == b"IEND" {
            return None;
        }
        pos += 12 + len;
    }
    None
}

fn jpeg(data: &[u8]) -> Option<(f64, f64)> {
    let mut dpi: Option<(f64, f64)> = None;
    let mut pos = 2;
    while pos + 4 <= data.len() && data[pos] == 0xff {
        let marker = data[pos + 1];
        // Stand-alone markers (padding, RSTn, SOI) carry no length.
        if marker == 0xff || marker == 0x01 || (0xd0..=0xd8).contains(&marker) {
            pos += if marker == 0xff { 1 } else { 2 };
            continue;
        }
        // Start of scan / end of image: no more headers.
        if marker == 0xda || marker == 0xd9 {
            break;
        }
        let len = usize::from(u16::from_be_bytes([data[pos + 2], data[pos + 3]]));
        let seg = data.get(pos + 4..(pos + 2 + len).min(data.len()))?;
        if marker == 0xe0 && seg.starts_with(b"JFIF\0") && seg.len() >= 12 {
            let x = f64::from(u16::from_be_bytes([seg[8], seg[9]]));
            let y = f64::from(u16::from_be_bytes([seg[10], seg[11]]));
            match seg[7] {
                1 => dpi = Some((x, y)),
                2 => dpi = Some((x * 2.54, y * 2.54)),
                _ => {}
            }
        } else if marker == 0xe1 && seg.starts_with(b"Exif\0\0") && dpi.is_none() {
            // Pillow reads `XResolution` / `ResolutionUnit` from the EXIF only
            // when the JFIF gave no dpi, uses the one value for both axes, and
            // falls back to 72 when either tag is missing or unreadable.
            let ifd = tiff_tags(&seg[6..]);
            dpi = Some(match ifd {
                Some(Tags {
                    xres: Some(x),
                    unit: Some(unit),
                    ..
                }) => {
                    let x = if unit == 3 { x * 2.54 } else { x };
                    (x, x)
                }
                _ => (72.0, 72.0),
            });
        }
        pos += 2 + len;
    }
    dpi
}

fn bmp(data: &[u8]) -> Option<(f64, f64)> {
    // BITMAPINFOHEADER (40 bytes) and later carry the pixels per metre.
    let le = |o: usize| i32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
    let header_size = le(14);
    if header_size < 40 {
        return None;
    }
    let x = f64::from(le(38)) / 39.3701;
    // A header cut before `biYPelsPerMeter`: the horizontal value for both.
    let y = if data.len() >= 46 {
        f64::from(le(42)) / 39.3701
    } else {
        x
    };
    Some((x, y))
}

fn tiff(data: &[u8]) -> Option<(f64, f64)> {
    let tags = tiff_tags(data)?;
    // TiffImagePlugin: each resolution defaults to 1, and a zero one records
    // nothing.
    let (x, y) = (tags.xres.unwrap_or(1.0), tags.yres.unwrap_or(1.0));
    if x == 0.0 || y == 0.0 {
        return None;
    }
    // Unit 2 → inches, 3 → centimetres, absent → inches (backward
    // compatibility), 1 → `resolution`, not `dpi`.
    match tags.unit {
        Some(2) | None => Some((x, y)),
        Some(3) => Some((x * 2.54, y * 2.54)),
        _ => None,
    }
}

/// The resolution tags of a TIFF header's first IFD.
struct Tags {
    xres: Option<f64>,
    yres: Option<f64>,
    unit: Option<u16>,
}

/// A TIFF header's first IFD: `XResolution`, `YResolution`, `ResolutionUnit`;
/// `None` when the header is malformed or a resolution has a zero denominator.
fn tiff_tags(t: &[u8]) -> Option<Tags> {
    let le = match t.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |o: usize| -> Option<u16> {
        let b = [*t.get(o)?, *t.get(o + 1)?];
        Some(if le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    };
    let u32_at = |o: usize| -> Option<u32> {
        let b = [*t.get(o)?, *t.get(o + 1)?, *t.get(o + 2)?, *t.get(o + 3)?];
        Some(if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    };
    // RATIONAL: two u32 at the value offset.
    let rational = |e: usize| -> Option<f64> {
        let off = u32_at(e + 8)? as usize;
        let (num, den) = (u32_at(off)?, u32_at(off + 4)?);
        (den != 0).then(|| f64::from(num) / f64::from(den))
    };
    let ifd = u32_at(4)? as usize;
    let count = usize::from(u16_at(ifd)?);
    let mut tags = Tags {
        xres: None,
        yres: None,
        unit: None,
    };
    for i in 0..count {
        let e = ifd + 2 + 12 * i;
        match (u16_at(e)?, u16_at(e + 2)?) {
            (0x011a, 5) => tags.xres = Some(rational(e)?),
            (0x011b, 5) => tags.yres = Some(rational(e)?),
            // SHORT: inline in the value field.
            (0x0128, 3) => tags.unit = Some(u16_at(e + 8)?),
            _ => {}
        }
    }
    Some(tags)
}

#[cfg(test)]
mod tests {
    use super::resolution;

    fn png(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        for (kind, data) in chunks {
            out.extend((data.len() as u32).to_be_bytes());
            out.extend(*kind);
            out.extend(data);
            out.extend([0; 4]);
        }
        out
    }

    /// Both axes, as Pillow reads them: a `pHYs` with different horizontal and
    /// vertical densities, a JFIF density per axis, a BMP's two pixels per
    /// metre, a TIFF's two resolutions.
    #[test]
    fn both_axes_are_read() {
        let mut phys = 11811u32.to_be_bytes().to_vec();
        phys.extend(5906u32.to_be_bytes());
        phys.push(1);
        let (x, y) = resolution(&png(&[(b"IHDR", vec![0; 13]), (b"pHYs", phys)])).unwrap();
        assert!((x - 299.9994).abs() < 1e-4 && (y - 150.0124).abs() < 1e-4);

        let mut jfif = b"JFIF\0\x01\x01\x01".to_vec();
        jfif.extend(300u16.to_be_bytes());
        jfif.extend(150u16.to_be_bytes());
        jfif.extend([0, 0]);
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe0];
        jpeg.extend((jfif.len() as u16 + 2).to_be_bytes());
        jpeg.extend(jfif);
        jpeg.extend([0xff, 0xda, 0, 2]);
        assert_eq!(resolution(&jpeg), Some((300.0, 150.0)));

        let mut bmp = vec![0u8; 54];
        bmp[..2].copy_from_slice(b"BM");
        bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
        bmp[38..42].copy_from_slice(&11811i32.to_le_bytes());
        bmp[42..46].copy_from_slice(&3937i32.to_le_bytes());
        let (x, y) = resolution(&bmp).unwrap();
        assert!((x - 299.9992).abs() < 1e-3 && (y - 99.9997).abs() < 1e-3);
    }

    /// A TIFF's two resolutions; a missing one is 1 and a zero one records
    /// nothing, as TiffImagePlugin reads them.
    #[test]
    fn tiff_resolutions_default_to_one() {
        // II, IFD at 8: XResolution, YResolution (rationals after the IFD).
        let tiff = |x: Option<(u32, u32)>, y: Option<(u32, u32)>| {
            let tags: Vec<(u16, (u32, u32))> = [(0x011au16, x), (0x011b, y)]
                .into_iter()
                .filter_map(|(tag, v)| v.map(|v| (tag, v)))
                .collect();
            let mut t = b"II*\0".to_vec();
            t.extend(8u32.to_le_bytes());
            t.extend((tags.len() as u16).to_le_bytes());
            let data_at = 8 + 2 + 12 * tags.len() as u32 + 4;
            for (i, (tag, _)) in tags.iter().enumerate() {
                t.extend(tag.to_le_bytes());
                t.extend(5u16.to_le_bytes());
                t.extend(1u32.to_le_bytes());
                t.extend((data_at + 8 * i as u32).to_le_bytes());
            }
            t.extend(0u32.to_le_bytes());
            for (_, (num, den)) in &tags {
                t.extend(num.to_le_bytes());
                t.extend(den.to_le_bytes());
            }
            t
        };
        assert_eq!(
            resolution(&tiff(Some((300, 1)), Some((150, 1)))),
            Some((300.0, 150.0))
        );
        assert_eq!(resolution(&tiff(Some((300, 1)), None)), Some((300.0, 1.0)));
        assert_eq!(resolution(&tiff(None, None)), Some((1.0, 1.0)));
        assert_eq!(resolution(&tiff(Some((0, 1)), Some((300, 1)))), None);
        assert_eq!(resolution(&tiff(Some((300, 0)), None)), None);
    }

    #[test]
    fn formats_without_a_resolution() {
        assert_eq!(resolution(b"GIF89a"), None);
        assert_eq!(resolution(b"RIFF\0\0\0\0WEBPVP8 "), None);
        assert_eq!(resolution(&png(&[(b"IHDR", vec![0; 13])])), None);
    }
}
