//! Embedded images as picture images (#1007, docling#5143's
//! `use_embedded_picture_images`): when a picture region holds exactly one
//! image XObject and nothing else draws over it, the picture's image is that
//! bitmap at its native resolution instead of a crop of the page render.
//!
//! [`page_images`] walks a page's content stream once — the CTM, the clip,
//! ExtGState transparency, paths, shadings, forms, image XObjects and inline
//! images; text is left to the page's text cells — and lists every image
//! placement in top-left page points, with the decoded pixels attached only
//! to the ones that are *safe* to hand out as they are:
//!
//! * nothing is painted after the image over its area (a later path,
//!   shading, image or Widget appearance would change what the page shows);
//! * it is opaque and composited plainly — no `/SMask`, `/Mask`,
//!   `/ImageMask`, `/SMaskInData`, no ExtGState soft mask, constant alpha
//!   below 1 or blend mode;
//! * it is drawn upright (an axis-aligned CTM, neither mirrored nor
//!   rotated: the bitmap's rows are the picture's rows) and not cut by the
//!   clip;
//! * it decodes (no placeholder block).
//!
//! [`match_region`] then applies docling's matching rule: the region and one
//! image's box must each cover ≥ 0.8 of the other (mutual
//! intersection-over-self), no second image may overlap it, and no text
//! cell may either. Anything uncertain returns `None` and the caller keeps
//! the rendered crop — the fallback is always the picture docling's default
//! produces. Upstream checks text and vector content painted *anywhere* over
//! the image; the walk here only rejects content painted *after* it (an
//! opaque image hides whatever lies beneath), so a page background or a
//! frame drawn first no longer forces the crop — a deliberate, documented
//! widening (`docs/MIGRATION.md`).
//!
//! Pages with `/Rotate` yield no placements (the bitmap would have to be
//! turned with the page): their pictures stay render crops.

use image::RgbImage;
use lopdf::{Dictionary, Document, Object};

use crate::pdf_meta::PdfMeta;
use crate::pdfium_backend::TextCell;
use crate::render::geom::{Box2, Mat};
use crate::render::objects::{
    as_dict, as_stream, get, get_bool2, get_int, get_name, name, num, nums, resource,
};

/// docling's `overlap_threshold`: the region and the image must each cover
/// this share of the other.
const OVERLAP: f32 = 0.8;
/// The share of an image's box the clip must leave visible for the bitmap
/// to still be what the page shows (anti-aliased clip edges, rounding).
const CLIP_KEEP: f64 = 0.99;
const MAX_OPS: usize = 2_000_000;
const MAX_DEPTH: usize = 12;
/// Decoded pixels kept per page (≈ 256 MiB of RGB): a page of scans past it
/// keeps its crops.
const MAX_PIXELS: u64 = 90_000_000;

/// One image placement on a page.
#[derive(Clone)]
pub struct EmbeddedImage {
    /// `[l, t, r, b]` in top-left page points (the frame regions and text
    /// cells use).
    pub bbox: [f32; 4],
    /// The bitmap at native resolution when the placement is safe to hand
    /// out as the picture (see the module docs); `None` otherwise — the
    /// placement still counts for the "exactly one image" and "no other
    /// image overlaps" rules.
    pub image: Option<RgbImage>,
}

impl std::fmt::Debug for EmbeddedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedImage")
            .field("bbox", &self.bbox)
            .field("image", &self.image.as_ref().map(|i| i.dimensions()))
            .finish()
    }
}

#[derive(Clone)]
struct State {
    ctm: Mat,
    /// The clip's bounding box in page space.
    clip: Box2,
    /// The clip is not exactly [`Self::clip`] (a non-rectangular path, a
    /// rotated rectangle, text clipping).
    clip_complex: bool,
    /// An ExtGState soft mask, constant fill alpha below 1 or a blend mode.
    tainted: bool,
    text_clip: bool,
}

enum Paint {
    /// An image placement: its page box, and the stream + resources to
    /// decode it from when it is safe.
    Image {
        bbox: Box2,
        decodable: Option<(lopdf::Stream, Option<Dictionary>)>,
    },
    /// Anything else that puts ink on the page (clipped to the clip box).
    Mark(Box2),
}

struct Walk<'a> {
    doc: &'a Document,
    ops: usize,
    paints: Vec<Paint>,
}

/// Every image placement of page `index` (0-based), the safe ones decoded.
/// Empty for a rotated page or one lopdf cannot read.
pub fn page_images(meta: &PdfMeta, index: usize) -> Vec<EmbeddedImage> {
    let Some(geom) = meta.geometry(index) else {
        return Vec::new();
    };
    if geom.rotation != 0 {
        return Vec::new();
    }
    let doc = meta.doc();
    let Some(pid) = meta.page_id(index) else {
        return Vec::new();
    };
    let pb = crate::textparse::page_box(doc, pid);
    let (l, b, w, h) = (
        f64::from(pb.l),
        f64::from(pb.b),
        f64::from(pb.w),
        f64::from(pb.h),
    );
    // user (x, y) → top-left page points (x − l, h − (y − b)).
    let base = Mat::new(1.0, 0.0, 0.0, -1.0, -l, h + b);
    let page_box = Box2::new(0.0, 0.0, w, h);
    let resources = page_resources(doc, pid).cloned();
    let content = doc.get_page_content(pid);
    let mut walk = Walk {
        doc,
        ops: 0,
        paints: Vec::new(),
    };
    let st = State {
        ctm: base,
        clip: page_box,
        clip_complex: false,
        tainted: false,
        text_clip: false,
    };
    walk.run(&content, resources.as_ref(), st, 0);
    if let Ok(page) = doc.get_dictionary(pid) {
        walk.widgets(page, base);
    }
    walk.finish()
}

/// The placement `region` (`[l, t, r, b]`, top-left page points) is a
/// picture of, by docling's rule — `None` unless exactly one image matches
/// it, that image is safe, and no other image or text cell overlaps it.
pub fn match_region<'a>(
    images: &'a [EmbeddedImage],
    cells: &[TextCell],
    region: [f32; 4],
) -> Option<&'a RgbImage> {
    let mut found = None;
    for (i, im) in images.iter().enumerate() {
        if ios(region, im.bbox) >= OVERLAP && ios(im.bbox, region) >= OVERLAP {
            if found.is_some() {
                return None;
            }
            found = Some(i);
        }
    }
    let i = found?;
    let cand = &images[i];
    let image = cand.image.as_ref()?;
    let overlaps_other = images
        .iter()
        .enumerate()
        .any(|(j, o)| j != i && overlap(cand.bbox, o.bbox) > 0.0);
    let overlaps_text = cells
        .iter()
        .filter(|c| !c.text.trim().is_empty())
        .any(|c| overlap(cand.bbox, [c.l, c.t, c.r, c.b]) > 0.0);
    (!overlaps_other && !overlaps_text).then_some(image)
}

fn overlap(a: [f32; 4], b: [f32; 4]) -> f32 {
    let w = a[2].min(b[2]) - a[0].max(b[0]);
    let h = a[3].min(b[3]) - a[1].max(b[1]);
    if w > 0.0 && h > 0.0 {
        w * h
    } else {
        0.0
    }
}

/// docling's `intersection_over_self`: the share of `a` that `b` covers.
fn ios(a: [f32; 4], b: [f32; 4]) -> f32 {
    let area = (a[2] - a[0]).max(0.0) * (a[3] - a[1]).max(0.0);
    if area <= 0.0 {
        0.0
    } else {
        overlap(a, b) / area
    }
}

fn page_resources(doc: &Document, pid: lopdf::ObjectId) -> Option<&Dictionary> {
    doc.get_page_resources(pid).ok().and_then(|(inline, ids)| {
        inline.or_else(|| ids.into_iter().find_map(|id| doc.get_dictionary(id).ok()))
    })
}

fn boxes_overlap(a: Box2, b: Box2) -> bool {
    !a.intersect(b).is_empty()
}

fn area(b: Box2) -> f64 {
    b.width() * b.height()
}

/// An axis-aligned transform (no rotation or shear).
fn axis_aligned(m: Mat) -> bool {
    let s = m.max_scale().max(1e-9);
    m.b.abs() <= 1e-6 * s && m.c.abs() <= 1e-6 * s
}

impl Walk<'_> {
    fn finish(self) -> Vec<EmbeddedImage> {
        let mut out = Vec::new();
        let mut pixels = 0u64;
        for (i, p) in self.paints.iter().enumerate() {
            let Paint::Image { bbox, decodable } = p else {
                continue;
            };
            let painted_over = self.paints[i + 1..].iter().any(|q| match q {
                Paint::Image { bbox: o, .. } | Paint::Mark(o) => boxes_overlap(*bbox, *o),
            });
            let image = match decodable {
                Some((stream, res)) if !painted_over => {
                    decode(self.doc, stream, res.as_ref(), &mut pixels)
                }
                _ => None,
            };
            out.push(EmbeddedImage {
                bbox: [
                    bbox.x0 as f32,
                    bbox.y0 as f32,
                    bbox.x1 as f32,
                    bbox.y1 as f32,
                ],
                image,
            });
        }
        out
    }

    fn mark(&mut self, st: &State, b: Box2) {
        let b = b.intersect(st.clip);
        if !b.is_empty() {
            self.paints.push(Paint::Mark(b));
        }
    }

    fn run(&mut self, content: &[u8], res: Option<&Dictionary>, init: State, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let doc = self.doc;
        let prepared = crate::render::prepass::prepare(content, doc, res);
        let Ok(ops) = lopdf::content::Content::decode(&prepared.content) else {
            return;
        };
        let mut stack: Vec<State> = Vec::new();
        let mut st = init;
        // The current path's user-space points, and whether it is one `re`.
        let mut pts: Vec<(f64, f64)> = Vec::new();
        let mut rects = 0usize;
        let mut other_segments = false;
        let mut pending_clip = false;
        let mut line_width = 1.0f64;
        let mut render_mode = 0i64;
        for op in &ops.operations {
            self.ops += 1;
            if self.ops > MAX_OPS {
                return;
            }
            let args = &op.operands;
            let n = |i: usize| args.get(i).and_then(num).unwrap_or(0.0);
            match op.operator.as_str() {
                "q" => {
                    stack.push(st.clone());
                    if stack.len() > 64 {
                        stack.remove(0);
                    }
                }
                "Q" => {
                    if let Some(s) = stack.pop() {
                        st = s;
                    }
                }
                "cm" if args.len() >= 6 => {
                    let v: Vec<f64> = (0..6).map(n).collect();
                    if let Some(m) = Mat::from_slice(&v) {
                        st.ctm = m.then(st.ctm);
                    }
                }
                "w" => line_width = n(0).abs(),
                "gs" => {
                    if let Some(gs) = args
                        .first()
                        .and_then(name)
                        .and_then(|nm| resource(doc, res, b"ExtGState", nm))
                        .and_then(|o| as_dict(doc, o))
                    {
                        if let Some(lw) = get(doc, gs, b"LW").and_then(num) {
                            line_width = lw.abs();
                        }
                        let smask = get(doc, gs, b"SMask")
                            .is_some_and(|o| !matches!(o, Object::Name(n) if n == b"None"));
                        let alpha = get(doc, gs, b"ca").and_then(num).is_some_and(|a| a < 1.0);
                        let blend = get(doc, gs, b"BM").is_some_and(|o| {
                            let nm = match o {
                                Object::Name(n) => Some(n.as_slice()),
                                Object::Array(a) => a.first().and_then(name),
                                _ => None,
                            };
                            !matches!(nm, Some(b"Normal" | b"Compatible"))
                        });
                        if smask || alpha || blend {
                            st.tainted = true;
                        }
                    }
                }
                "m" | "l" if args.len() >= 2 => {
                    pts.push((n(0), n(1)));
                    other_segments = true;
                }
                "c" if args.len() >= 6 => {
                    pts.extend([(n(0), n(1)), (n(2), n(3)), (n(4), n(5))]);
                    other_segments = true;
                }
                "v" | "y" if args.len() >= 4 => {
                    pts.extend([(n(0), n(1)), (n(2), n(3))]);
                    other_segments = true;
                }
                "re" if args.len() >= 4 => {
                    let (x, y, w, h) = (n(0), n(1), n(2), n(3));
                    pts.extend([(x, y), (x + w, y + h)]);
                    rects += 1;
                }
                "h" => {}
                "W" | "W*" => pending_clip = true,
                "S" | "s" | "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" | "n" => {
                    let bbox = path_box(&pts, st.ctm);
                    if op.operator != "n" {
                        if let Some(b) = bbox {
                            let stroked =
                                matches!(op.operator.as_str(), "S" | "s" | "B" | "B*" | "b" | "b*");
                            // A stroke reaches half its width past the path
                            // (and at least a hairline's pixel).
                            let pad = if stroked {
                                (line_width * st.ctm.max_scale() / 2.0).max(0.5)
                            } else {
                                0.0
                            };
                            let b = Box2::new(b.x0 - pad, b.y0 - pad, b.x1 + pad, b.y1 + pad);
                            self.mark(&st, b);
                        }
                    }
                    if pending_clip {
                        match bbox {
                            Some(b) => {
                                st.clip = st.clip.intersect(b);
                                if other_segments || rects != 1 || !axis_aligned(st.ctm) {
                                    st.clip_complex = true;
                                }
                            }
                            None => st.clip = Box2::new(0.0, 0.0, 0.0, 0.0),
                        }
                    }
                    pts.clear();
                    rects = 0;
                    other_segments = false;
                    pending_clip = false;
                }
                "Tr" => render_mode = n(0) as i64,
                "Tj" | "TJ" | "'" | "\"" if render_mode >= 4 => {
                    // Clipping text: what follows is cut to glyph shapes.
                    st.text_clip = true;
                }
                "ET" if st.text_clip => {
                    st.clip_complex = true;
                    st.text_clip = false;
                }
                "Do" => {
                    let Some(xobj) = args
                        .first()
                        .and_then(name)
                        .and_then(|nm| resource(doc, res, b"XObject", nm))
                    else {
                        continue;
                    };
                    let Some(stream) = as_stream(doc, xobj) else {
                        continue;
                    };
                    let is_image = match get_name(doc, &stream.dict, b"Subtype") {
                        Some(b"Image") => true,
                        Some(b"Form") => false,
                        Some(b"PS") => continue,
                        _ if stream.dict.has(b"BBox") => false,
                        _ if stream.dict.has(b"Width") => true,
                        _ => continue,
                    };
                    if is_image {
                        self.image(stream, res, &st);
                    } else {
                        self.form(stream, res, &st, depth + 1);
                    }
                }
                "BIX" => {
                    if let Some(s) = args
                        .first()
                        .and_then(crate::render::prepass::inline_index)
                        .and_then(|i| prepared.inline.get(i))
                    {
                        let s = s.clone();
                        self.image(&s, res, &st);
                    }
                }
                // A shading fills the whole clip.
                "sh" => {
                    let clip = st.clip;
                    self.mark(&st, clip);
                }
                _ => {}
            }
        }
    }

    fn image(&mut self, stream: &lopdf::Stream, res: Option<&Dictionary>, st: &State) {
        let doc = self.doc;
        let (x0, y0, x1, y1) = st.ctm.unit_bbox();
        let bbox = Box2::new(x0, y0, x1, y1);
        if bbox.intersect(st.clip).is_empty() {
            return;
        }
        let d = &stream.dict;
        let m = st.ctm;
        // Upright in the top-left frame: x grows right, the bitmap's first
        // row (unit-square y = 1) is on top.
        let upright = axis_aligned(m) && m.a > 0.0 && m.d < 0.0;
        let plain = !st.tainted
            && !get_bool2(doc, d, b"ImageMask", b"IM").unwrap_or(false)
            && !d.has(b"SMask")
            && !d.has(b"Mask")
            && get_int(doc, d, b"SMaskInData").unwrap_or(0) == 0;
        let unclipped = !st.clip_complex && area(bbox.intersect(st.clip)) >= CLIP_KEEP * area(bbox);
        let decodable = (upright && plain && unclipped).then(|| (stream.clone(), res.cloned()));
        self.paints.push(Paint::Image { bbox, decodable });
    }

    fn form(&mut self, stream: &lopdf::Stream, res: Option<&Dictionary>, st: &State, depth: usize) {
        let doc = self.doc;
        let matrix = get(doc, &stream.dict, b"Matrix")
            .and_then(|o| nums(doc, o))
            .and_then(|v| Mat::from_slice(&v))
            .unwrap_or(Mat::IDENTITY);
        let mut inner = st.clone();
        inner.ctm = matrix.then(st.ctm);
        if let Some(b) = get(doc, &stream.dict, b"BBox")
            .and_then(|o| nums(doc, o))
            .filter(|b| b.len() == 4)
        {
            let clip = Box2::new(b[0], b[1], b[2], b[3]).transformed(inner.ctm);
            inner.clip = inner.clip.intersect(clip);
            if !axis_aligned(inner.ctm) {
                inner.clip_complex = true;
            }
        }
        let own = get(doc, &stream.dict, b"Resources")
            .and_then(|o| as_dict(doc, o))
            .cloned();
        let Ok(content) = stream.decompressed_content() else {
            // Undecodable form content: assume it inks its whole box.
            let clip = inner.clip;
            self.mark(&inner, clip);
            return;
        };
        let res = own.as_ref().or(res);
        self.run(&content, res, inner, depth);
    }

    /// The Widget annotations the renderer draws over the page: their
    /// rectangles count as paint.
    fn widgets(&mut self, page: &Dictionary, base: Mat) {
        let doc = self.doc;
        let Some(Object::Array(annots)) = get(doc, page, b"Annots") else {
            return;
        };
        let st = State {
            ctm: base,
            clip: Box2::new(f64::MIN, f64::MIN, f64::MAX, f64::MAX),
            clip_complex: false,
            tainted: false,
            text_clip: false,
        };
        for a in annots {
            let Some(ad) = as_dict(doc, a) else { continue };
            if get_name(doc, ad, b"Subtype") != Some(b"Widget") || !ad.has(b"AP") {
                continue;
            }
            if let Some(r) = get(doc, ad, b"Rect")
                .and_then(|o| nums(doc, o))
                .filter(|r| r.len() == 4)
            {
                self.mark(&st, Box2::new(r[0], r[1], r[2], r[3]).transformed(base));
            }
        }
    }
}

fn path_box(pts: &[(f64, f64)], ctm: Mat) -> Option<Box2> {
    let mut it = pts.iter().map(|&(x, y)| ctm.apply(x, y));
    let (x, y) = it.next()?;
    let mut b = Box2::new(x, y, x, y);
    for (x, y) in it {
        b.x0 = b.x0.min(x);
        b.y0 = b.y0.min(y);
        b.x1 = b.x1.max(x);
        b.y1 = b.y1.max(y);
    }
    Some(b)
}

/// The image's samples as an RGB bitmap at native size; `None` when it does
/// not decode, comes out a placeholder, or the page's pixel budget is spent.
fn decode(
    doc: &Document,
    stream: &lopdf::Stream,
    res: Option<&Dictionary>,
    pixels: &mut u64,
) -> Option<RgbImage> {
    let (w, h) = crate::render::image::declared_size(doc, &stream.dict)?;
    let n = (w.max(0) as u64).saturating_mul(h.max(0) as u64);
    if *pixels + n > MAX_PIXELS {
        return None;
    }
    let loaded = crate::render::image::load(doc, stream, res, 0).ok()?;
    if loaded.placeholder_reason().is_some() {
        return None;
    }
    let mut cmyk = crate::render::color::CmykCache::default();
    let dec = crate::render::image::rasterize(&loaded, [0, 0, 0], None, &mut cmyk).ok()?;
    let pm = &dec.pixmap;
    *pixels += u64::from(pm.width()) * u64::from(pm.height());
    let data = pm.data();
    let mut out = RgbImage::new(pm.width(), pm.height());
    for (i, px) in out.pixels_mut().enumerate() {
        let p = &data[i * 4..i * 4 + 4];
        if p[3] != 255 {
            // Not opaque after all (a colour key the dictionary did not
            // announce): the render is what the page shows.
            return None;
        }
        *px = image::Rgb([p[0], p[1], p[2]]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Stream};

    fn root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    /// Page 1 of `bytes` through the real page walk with the option on: its
    /// text cells and image placements.
    fn walk_first_page(bytes: &[u8]) -> crate::pdfium_backend::PdfPage {
        let mut out = None;
        crate::pdfium_backend::walk_pages::<crate::PdfError, _>(
            bytes,
            None,
            false,
            true,
            Some((0, 0)),
            crate::pdfium_backend::BitmapRender::OnDemand,
            crate::pdfium_backend::TextOptions {
                embedded_pictures: true,
                ..Default::default()
            },
            |_, _, page| {
                out = Some(page);
                Ok(())
            },
        )
        .unwrap();
        out.expect("one page")
    }

    fn fixture(name: &str) -> crate::pdfium_backend::PdfPage {
        let bytes = std::fs::read(root().join("tests/data/pdf/sources").join(name)).unwrap();
        walk_first_page(&bytes)
    }

    /// docling's `test_extract_single_embedded_image`: the one bitmap of
    /// `picture_classification.pdf` page 1 comes out at its native 768 × 474.
    #[test]
    fn extracts_the_single_embedded_image() {
        let page = fixture("picture_classification.pdf");
        assert_eq!(page.embedded_images.len(), 1, "{:?}", page.embedded_images);
        let bbox = page.embedded_images[0].bbox;
        let img = match_region(&page.embedded_images, &page.cells, bbox).expect("safe image");
        assert_eq!(img.dimensions(), (768, 474));
        // A layout box a little off the image still matches (≥ 0.8 both ways)…
        let loose = [bbox[0] - 8.0, bbox[1] - 8.0, bbox[2] + 4.0, bbox[3] + 4.0];
        assert!(match_region(&page.embedded_images, &page.cells, loose).is_some());
        // …one twice the image's size does not.
        let h = bbox[3] - bbox[1];
        let big = [bbox[0], bbox[1] - h, bbox[2], bbox[3]];
        assert!(match_region(&page.embedded_images, &page.cells, big).is_none());
    }

    /// docling's `test_reject_embedded_image_with_overlapping_page_content`:
    /// `amt_handbook_sample.pdf`'s first bitmap shares its box with others.
    #[test]
    fn rejects_an_image_with_overlapping_page_content() {
        let page = fixture("amt_handbook_sample.pdf");
        let first = page.embedded_images.first().expect("bitmaps").bbox;
        assert!(match_region(&page.embedded_images, &page.cells, first).is_none());
    }

    /// The walk lists nothing unless asked (the option is off by default).
    #[test]
    fn walk_collects_nothing_by_default() {
        let bytes = std::fs::read(root().join("tests/data/pdf/sources/picture_classification.pdf"))
            .unwrap();
        let mut n = None;
        crate::pdfium_backend::walk_pages::<crate::PdfError, _>(
            &bytes,
            None,
            false,
            true,
            Some((0, 0)),
            crate::pdfium_backend::BitmapRender::OnDemand,
            crate::pdfium_backend::TextOptions::default(),
            |_, _, page| {
                n = Some(page.embedded_images.len());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(n, Some(0));
    }

    /// A 400 × 300 pt page drawing a 4 × 2 RGB image XObject (`/Im0`, plus
    /// `/Im1`, and `extra` merged into `/Im0`'s dictionary) with `content`.
    fn synth(content: &str, extra: lopdf::Dictionary, gs: lopdf::Dictionary) -> Vec<u8> {
        let mut doc = lopdf::Document::with_version("1.5");
        let image = |extra: lopdf::Dictionary| {
            let mut d = dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 2,
                "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8,
            };
            for (k, v) in extra.into_iter() {
                d.set(k, v);
            }
            let px: Vec<u8> = (0..8u8).flat_map(|i| [i * 30, 255 - i * 30, 100]).collect();
            Stream::new(d, px)
        };
        let im0 = doc.add_object(image(extra));
        let im1 = doc.add_object(image(lopdf::Dictionary::new()));
        let pages_id = doc.new_object_id();
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.as_bytes().to_vec()));
        let res = dictionary! {
            "XObject" => dictionary! {
                "Im0" => Object::Reference(im0),
                "Im1" => Object::Reference(im1),
            },
            "ExtGState" => dictionary! { "G0" => gs },
        };
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 400.into(), 300.into()],
            "Contents" => Object::Reference(content_id),
            "Resources" => res,
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![Object::Reference(page_id)], "Count" => 1,
            }),
        );
        let catalog = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", Object::Reference(catalog));
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    /// The image `content` places at 100 × 50 pt from (50, 150) (top-left
    /// frame: x 50–150, y 100–150), as `match_region` hands it out.
    fn matched(content: &str, extra: lopdf::Dictionary, gs: lopdf::Dictionary) -> Option<RgbImage> {
        let page = walk_first_page(&synth(content, extra, gs));
        match_region(
            &page.embedded_images,
            &page.cells,
            [50.0, 100.0, 150.0, 150.0],
        )
        .cloned()
    }

    const DRAW: &str = "q 100 0 0 50 50 150 cm /Im0 Do Q";

    fn none() -> lopdf::Dictionary {
        lopdf::Dictionary::new()
    }

    #[test]
    fn a_plain_image_comes_out_at_native_size_with_its_pixels() {
        let img = matched(DRAW, none(), none()).expect("matched");
        assert_eq!(img.dimensions(), (4, 2));
        assert_eq!(img.get_pixel(0, 0).0, [0, 255, 100]);
        assert_eq!(img.get_pixel(3, 1).0, [210, 45, 100]);
    }

    #[test]
    fn content_painted_beneath_does_not_matter_but_over_it_does() {
        let under = format!("1 0 0 rg 0 0 400 300 re f {DRAW}");
        assert!(matched(&under, none(), none()).is_some());
        let over = format!("{DRAW} 0 0 1 rg 60 160 10 10 re f");
        assert!(matched(&over, none(), none()).is_none());
        let stroke = format!("{DRAW} 0 0 m 400 300 l S");
        assert!(matched(&stroke, none(), none()).is_none());
        // Ink painted after, but elsewhere on the page, is fine.
        let beside = format!("{DRAW} 300 10 20 20 re f");
        assert!(matched(&beside, none(), none()).is_some());
        // A clip-only path paints nothing.
        let clip = format!("q 0 0 400 300 re W n {DRAW} Q");
        assert!(matched(&clip, none(), none()).is_some());
    }

    #[test]
    fn a_second_overlapping_image_makes_the_match_ambiguous() {
        let two = format!("{DRAW} q 20 0 0 20 140 140 cm /Im1 Do Q");
        assert!(matched(&two, none(), none()).is_none());
        // Drawn first, beneath ours: still a second image in the box.
        let first = format!("q 20 0 0 20 140 140 cm /Im1 Do Q {DRAW}");
        assert!(matched(&first, none(), none()).is_none());
    }

    #[test]
    fn rotated_mirrored_or_clipped_placements_keep_the_crop() {
        // Mirrored horizontally (same box).
        assert!(matched("q -100 0 0 50 150 150 cm /Im0 Do Q", none(), none()).is_none());
        // Upside down.
        assert!(matched("q 100 0 0 -50 50 200 cm /Im0 Do Q", none(), none()).is_none());
        // Cut by a rectangle clip.
        let cut = format!("q 50 150 60 50 re W n {DRAW} Q");
        assert!(matched(&cut, none(), none()).is_none());
        // A non-rectangular clip.
        let tri = format!("q 0 0 m 400 0 l 0 300 l h W n {DRAW} Q");
        assert!(matched(&tri, none(), none()).is_none());
    }

    #[test]
    fn masked_or_transparent_images_keep_the_crop() {
        let mut smask = lopdf::Dictionary::new();
        smask.set("SMask", Object::Reference((99, 0)));
        assert!(matched(DRAW, smask, none()).is_none());
        let alpha = dictionary! { "Type" => "ExtGState", "ca" => 0.5 };
        let draw = format!("/G0 gs {DRAW}");
        assert!(matched(&draw, none(), alpha).is_none());
        let multiply = dictionary! { "Type" => "ExtGState", "BM" => "Multiply" };
        assert!(matched(&draw, none(), multiply).is_none());
        let normal = dictionary! { "Type" => "ExtGState", "BM" => "Normal", "ca" => 1 };
        assert!(matched(&draw, none(), normal).is_some());
    }

    #[test]
    fn a_rotated_page_lists_no_placements() {
        let bytes = synth(DRAW, none(), none());
        let mut doc = lopdf::Document::load_mem(&bytes).unwrap();
        let pid = *doc.get_pages().values().next().unwrap();
        doc.get_dictionary_mut(pid).unwrap().set("Rotate", 90);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        assert!(walk_first_page(&out).embedded_images.is_empty());
    }

    #[test]
    fn text_over_the_image_keeps_the_crop() {
        let im = EmbeddedImage {
            bbox: [50.0, 50.0, 150.0, 100.0],
            image: Some(RgbImage::new(4, 2)),
        };
        let cell = |text: &str| TextCell {
            text: text.into(),
            l: 60.0,
            t: 60.0,
            r: 90.0,
            b: 70.0,
        };
        let region = [50.0, 50.0, 150.0, 100.0];
        assert!(match_region(std::slice::from_ref(&im), &[cell("label")], region).is_none());
        assert!(match_region(std::slice::from_ref(&im), &[cell("  ")], region).is_some());
    }
}
