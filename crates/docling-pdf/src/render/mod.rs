//! The pure-Rust page renderer — phase 3 of retiring pdfium
//! (docs/PDF_CONFORMANCE.md, "Retiring pdfium"): the raster of a page's
//! vector content, text and images that the layout / TableFormer / OCR
//! stages consume — the default renderer since phase 5 (docling-parse's
//! renderer plugin draws them only under `DOCLING_RS_RENDERER=docling-parse`).
//!
//! It draws what docling-parse's `renderer<BLEND2D>` draws, in its frame:
//! the page's crop box on a `ceil(width · scale)` × `ceil(height · scale)`
//! white canvas (`resolve_canvas_size`), y down, the box mapped onto the
//! whole canvas, `/Rotate` applied to the finished pixels; content-stream
//! semantics as in [`content`]; fonts as in [`font`]; the anti-aliased
//! rasterizer, strokes, masks, gradients and image blits are tiny-skia's.
//! It is not byte-identical to that renderer — Blend2D's font engine,
//! rasterizer and JIT compositor round differently — and the docling-parse
//! shim stays the reference the PDF baselines are pinned to; this renderer
//! is measured against it (`tests::against_the_docling_parse_shim`).

pub mod color;
pub mod content;
pub mod font;
pub mod function;
pub mod geom;
pub mod image;
pub mod jbig2;
pub mod jpx;
pub mod objects;
pub mod prepass;

use std::rc::Rc;

use ::image::RgbImage;
use lopdf::Document;

use crate::pdf_meta::PdfMeta;
use geom::Mat;

/// docling-parse's `pixels_for_extent`: `ceil(extent − 1e-6)`.
pub fn pixels_for_extent(extent: f64) -> u32 {
    ((extent - 1e-6).ceil().max(1.0)) as u32
}

/// An image the render could not draw as its pixels (#916): a placeholder
/// block stands in for it (a codec that failed or is missing), or it was left
/// out. `rect` is the image's device bounding box in the output bitmap's
/// pixels (before `/Rotate` is applied to the page).
#[derive(Debug, Clone, PartialEq)]
pub struct Omission {
    /// The image XObject's object id (`None` for an inline image).
    pub object: Option<lopdf::ObjectId>,
    /// `[x0, y0, x1, y1]`.
    pub rect: [f64; 4],
    /// Why, e.g. `JPEG not decoded (Corrupt("no SOI"))`.
    pub reason: String,
}

/// A document's renderer: the per-document caches (fonts, CMYK table,
/// decoded image samples) that its page renders share. The pipeline renders
/// every page at two scales; hold one `Renderer` per document across both.
pub struct Renderer<'a> {
    meta: &'a PdfMeta,
    shared: Rc<content::Shared>,
}

impl<'a> Renderer<'a> {
    /// What the renders since the last call drew as a placeholder or left
    /// out (#916).
    pub fn take_omissions(&self) -> Vec<Omission> {
        self.shared.take_omissions()
    }

    /// A renderer decoding its JPEGs for docling's bitmap hint (1.0 pixel per
    /// PDF unit — the model-input renders).
    pub fn new(meta: &'a PdfMeta) -> Renderer<'a> {
        Renderer {
            meta,
            shared: Rc::new(content::Shared::default()),
        }
    }

    /// A renderer with an explicit `bitmap_target_pixels_per_unit`: `0.0`
    /// decodes every image at full size (the page rasters a caller keeps —
    /// `render_pages`, the VLM input — where docling-parse is asked with 0.0
    /// too).
    pub fn with_bitmap_hint(meta: &'a PdfMeta, bitmap_hint: f64) -> Renderer<'a> {
        Renderer {
            meta,
            shared: Rc::new(content::Shared::with_bitmap_hint(bitmap_hint)),
        }
    }

    /// Render page `index` at `scale` pixels per point the way docling-parse
    /// sizes its canvas (display orientation, `/Rotate` applied).
    pub fn render_scaled(&self, index: usize, scale: f64) -> Option<RgbImage> {
        let geom = self.meta.geometry(index)?;
        let w = pixels_for_extent(f64::from(geom.width) * scale);
        let h = pixels_for_extent(f64::from(geom.height) * scale);
        self.render(index, w, h)
    }

    /// Render page `index` into exactly `width` × `height` display-frame
    /// pixels (the crop box stretched onto the canvas, as every renderer
    /// here does).
    pub fn render(&self, index: usize, width: u32, height: u32) -> Option<RgbImage> {
        self.render_with_hint(index, width, height, self.shared.bitmap_hint())
    }

    /// [`Renderer::render`] with an explicit `bitmap_target_pixels_per_unit`
    /// for this one render: `1.0` decodes JPEGs the way docling's model
    /// inputs are decoded, `0.0` at full size — what a bitmap kept for OCR
    /// wants (the recognizer reads a scan's 300 dpi, not a quarter of it
    /// blitted up; docling's own OCR runs on its hint-1.0 decode, the
    /// deliberate deviation the pdfium raster of a scan already makes).
    pub fn render_with_hint(
        &self,
        index: usize,
        width: u32,
        height: u32,
        bitmap_hint: f64,
    ) -> Option<RgbImage> {
        if width == 0 || height == 0 || width > 1 << 15 || height > 1 << 15 {
            return None;
        }
        let meta = self.meta;
        let doc = meta.doc();
        let pid = meta.page_id(index)?;
        let page = doc.get_object(pid).ok()?.as_dict().ok()?;
        let geom = meta.geometry(index)?;
        let pb = crate::textparse::page_box(doc, pid);
        // Draw in the unrotated frame; the canvas is transposed for 90°/270°.
        let (cw, ch) = if geom.rotation == 90 || geom.rotation == 270 {
            (height, width)
        } else {
            (width, height)
        };
        let sx = f64::from(cw) / f64::from(pb.w).max(1e-6);
        let sy = f64::from(ch) / f64::from(pb.h).max(1e-6);
        // user (x, y) → canvas ((x − l)·sx, H − (y − b)·sy)
        let base = Mat::new(
            sx,
            0.0,
            0.0,
            -sy,
            -f64::from(pb.l) * sx,
            f64::from(ch) + f64::from(pb.b) * sy,
        );

        let content = doc.get_page_content(pid);
        let resources = page_resources(doc, pid);
        let mut interp = content::Interp::new(doc, cw, ch, self.shared.clone(), bitmap_hint)?;
        crate::timing::timed("render.content", || {
            interp.run_page(&content, resources, base);
            interp.run_widgets(page, base);
        });
        let canvas = interp.into_canvas();
        let rgb = crate::timing::timed("render.finish", || to_rgb(&canvas));
        Some(rotate(rgb, geom.rotation))
    }
}

/// One-off [`Renderer::render_scaled`] (no cache carried across pages).
pub fn render_page(meta: &PdfMeta, index: usize, scale: f64) -> Option<RgbImage> {
    Renderer::new(meta).render_scaled(index, scale)
}

/// One-off [`Renderer::render`].
pub fn render_page_sized(
    meta: &PdfMeta,
    index: usize,
    width: u32,
    height: u32,
) -> Option<RgbImage> {
    Renderer::new(meta).render(index, width, height)
}

fn page_resources(doc: &Document, pid: lopdf::ObjectId) -> Option<&lopdf::Dictionary> {
    doc.get_page_resources(pid).ok().and_then(|(inline, ids)| {
        inline.or_else(|| ids.into_iter().find_map(|id| doc.get_dictionary(id).ok()))
    })
}

/// Premultiplied RGBA over an opaque white canvas → RGB (the alpha is 255
/// everywhere the canvas started white; a blend mode can only lower colour).
fn to_rgb(pm: &tiny_skia::Pixmap) -> RgbImage {
    let (w, h) = (pm.width(), pm.height());
    let mut out = RgbImage::new(w, h);
    let data = pm.data();
    for (i, px) in out.pixels_mut().enumerate() {
        let a = u32::from(data[i * 4 + 3]);
        let un = |c: u8| -> u8 {
            if a == 0 {
                255
            } else if a == 255 {
                c
            } else {
                // Un-premultiply, then composite over white.
                let c = u32::from(c);
                (c + (255 - a)).min(255) as u8
            }
        };
        *px = ::image::Rgb([un(data[i * 4]), un(data[i * 4 + 1]), un(data[i * 4 + 2])]);
    }
    out
}

/// Rotate the finished canvas clockwise by `/Rotate` degrees.
fn rotate(img: RgbImage, rotation: u16) -> RgbImage {
    match rotation {
        90 => ::image::imageops::rotate90(&img),
        180 => ::image::imageops::rotate180(&img),
        270 => ::image::imageops::rotate270(&img),
        _ => img,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    /// Every corpus page renders at scale 1.0 to the docling-parse canvas
    /// size, with ink on it.
    #[test]
    fn renders_the_corpus() {
        // Every PDF the snapshot corpus is generated from — the pdf fixtures,
        // the scans, the LaTeX figure PDFs (matplotlib / TikZ vector art,
        // where the renderer's biggest snapshot drifts were) and the ODF
        // renders; `DOCLING_RS_SHIM_CORPUS=pdf` restricts it to the pdf dir.
        let mut files = Vec::new();
        for d in corpus_dirs() {
            collect_pdfs(&root().join(d), &mut files);
        }
        files.sort();
        assert!(!files.is_empty());
        let check = |f: &std::path::Path| {
            let bytes = std::fs::read(f).unwrap();
            let Some(meta) = PdfMeta::open(&bytes) else {
                return;
            };
            let renderer = Renderer::new(&meta);
            for i in 0..meta.page_count().min(3) {
                let g = meta.geometry(i).unwrap();
                let img = renderer
                    .render_scaled(i, 1.0)
                    .unwrap_or_else(|| panic!("{}: page {} declined", f.display(), i + 1));
                assert_eq!(
                    img.width(),
                    pixels_for_extent(f64::from(g.width)),
                    "{}",
                    f.display()
                );
                assert_eq!(
                    img.height(),
                    pixels_for_extent(f64::from(g.height)),
                    "{}",
                    f.display()
                );
                let dark = img.pixels().filter(|p| p[0] < 200).count();
                assert!(dark > 0, "{} p{}: blank render", f.display(), i + 1);
            }
        };
        // A minute of debug-build rendering serially; the files are
        // independent, so a thread per core takes them off a shared queue
        // (the crate has no rayon). A worker's panic is the test's.
        let next = std::sync::atomic::AtomicUsize::new(0);
        let workers = std::thread::available_parallelism().map_or(1, |n| n.get());
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..workers.min(files.len()))
                .map(|_| {
                    s.spawn(|| loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(f) = files.get(i) else { break };
                        check(f);
                    })
                })
                .collect();
            for h in handles {
                if let Err(e) = h.join() {
                    std::panic::resume_unwind(e);
                }
            }
        });
    }

    /// Measure this renderer against the docling-parse shim on the corpus at
    /// the two pipeline scales, when the shim is installed: prints per-file
    /// mean absolute differences and asserts a loose bound so a regression
    /// (a page gone blank, text drawn at the wrong size) fails the test.
    #[test]
    fn against_the_docling_parse_shim() {
        // Tests run with CWD = the crate; point the loader at the repo's copy.
        if std::env::var_os("DOCLING_PARSE_RENDER_LIB").is_none() {
            let lib = root().join(".docling-parse/lib");
            if lib.is_dir() {
                std::env::set_var("DOCLING_PARSE_RENDER_LIB", &lib);
            }
        }
        // The shim loads only when asked for by name (phase 5); no other test
        // in this binary resolves the plugin, so the once-only load sees this.
        std::env::set_var("DOCLING_RS_RENDERER", "docling-parse");
        let Some(plugin) = crate::dparse_render::plugin() else {
            eprintln!("docling-parse shim not installed — skipping the renderer comparison");
            return;
        };
        let _ = plugin;
        // Every PDF the snapshot corpus is generated from (the walk of
        // `renders_the_corpus`), so the gate covers what the baselines see.
        let mut files = Vec::new();
        for d in corpus_dirs() {
            collect_pdfs(&root().join(d), &mut files);
        }
        files.sort();
        eprintln!("renderer vs shim: {} PDFs", files.len());
        let mut worst = 0.0f64;
        let mut total_mad = 0.0f64;
        let mut n = 0usize;
        for f in &files {
            let bytes = std::fs::read(f).unwrap();
            let Some(meta) = PdfMeta::open(&bytes) else {
                eprintln!("{}: object model unreadable, skipped", f.display());
                continue;
            };
            let renderer = Renderer::new(&meta);
            let Some(dp) = crate::dparse_render::Doc::open_if_enabled(&bytes, None) else {
                eprintln!("{}: the shim declined the file, skipped", f.display());
                continue;
            };
            for i in 0..meta.page_count() {
                for scale in [1.0f64, 2.0] {
                    let Ok(want) = dp.render(i, scale, 1.0) else {
                        continue;
                    };
                    let Some(got) = renderer.render(i, want.width(), want.height()) else {
                        continue;
                    };
                    let mad = mean_abs_diff(&got, &want);
                    worst = worst.max(mad);
                    total_mad += mad;
                    n += 1;
                    eprintln!(
                        "{} p{} @{scale}: mean |Δ| = {mad:.2}",
                        f.file_name().unwrap().to_string_lossy(),
                        i + 1
                    );
                }
                dp.release_page(i);
            }
        }
        if n > 0 {
            eprintln!(
                "renderer vs shim: {n} renders, mean |Δ| {:.2}, worst {worst:.2}",
                total_mad / n as f64
            );
        }
        // The corpus sits at mean |Δ| ≈ 1 / 255 per channel with the worst
        // page (a photograph resampled through a different bilinear phase)
        // under 6; a page past 10 is a drawing bug, not rounding.
        assert!(
            worst < 10.0,
            "a render diverged from the shim (mean |Δ| {worst:.1})"
        );
        if n > 0 {
            assert!(
                total_mad / (n as f64) < 2.0,
                "the corpus drifted from the shim"
            );
        }
    }

    /// The source directories of the snapshot corpus; `DOCLING_RS_SHIM_CORPUS=pdf`
    /// restricts a run to the pdf fixtures.
    fn corpus_dirs() -> &'static [&'static str] {
        if std::env::var("DOCLING_RS_SHIM_CORPUS").as_deref() == Ok("pdf") {
            &["tests/data/pdf/sources"]
        } else {
            &[
                "tests/data/pdf/sources",
                "tests/data/scanned/sources",
                "tests/data/latex/sources",
                "tests/data/odf/sources",
            ]
        }
    }

    fn collect_pdfs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect_pdfs(&p, out);
            } else if p.extension().is_some_and(|e| e == "pdf") {
                out.push(p);
            }
        }
    }

    fn mean_abs_diff(a: &RgbImage, b: &RgbImage) -> f64 {
        if a.dimensions() != b.dimensions() {
            return 255.0;
        }
        let sum: u64 = a
            .as_raw()
            .iter()
            .zip(b.as_raw())
            .map(|(x, y)| u64::from((i32::from(*x) - i32::from(*y)).unsigned_abs()))
            .sum();
        sum as f64 / a.as_raw().len().max(1) as f64
    }
}

/// Synthetic pages, one drawing feature each: the frame mapping, `/Rotate`,
/// the minimum stroke width, constant alpha, clips, images (XObject and
/// inline), shadings, patterns, forms, widgets, text (fallback face and
/// Type 3). Each asserts where the ink lands, not its exact coverage — the
/// shim comparison above is the coverage oracle.
#[cfg(test)]
mod synthetic {
    use super::*;
    use lopdf::{dictionary, Dictionary, Object, Stream};

    /// A one-page document: `media` box, `page_extra` merged into the page
    /// dictionary (CropBox, Rotate, Annots), `resources`, `content`.
    fn synth(
        doc: &mut Document,
        media: [f32; 4],
        page_extra: Dictionary,
        resources: Dictionary,
        content: &str,
    ) -> Vec<u8> {
        synth_bytes(doc, media, page_extra, resources, content.as_bytes())
    }

    fn synth_bytes(
        doc: &mut Document,
        media: [f32; 4],
        page_extra: Dictionary,
        resources: Dictionary,
        content: &[u8],
    ) -> Vec<u8> {
        let pages_id = doc.new_object_id();
        let res_id = doc.add_object(resources);
        let content_id = doc.add_object(Stream::new(Dictionary::new(), content.to_vec()));
        let mut page = dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => media.iter().map(|&v| Object::Real(v)).collect::<Vec<_>>(),
            "Contents" => Object::Reference(content_id),
            "Resources" => Object::Reference(res_id),
        };
        for (k, v) in page_extra.into_iter() {
            page.set(k, v);
        }
        let page_id = doc.add_object(page);
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    /// A 200 × 100 pt page with `content` and `resources`.
    fn page(resources: Dictionary, content: &str) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        synth(
            &mut doc,
            [0.0, 0.0, 200.0, 100.0],
            Dictionary::new(),
            resources,
            content,
        )
    }

    fn page_with_contents(contents: &[&[u8]]) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let streams = contents
            .iter()
            .map(|content| {
                Object::Reference(doc.add_object(Stream::new(Dictionary::new(), content.to_vec())))
            })
            .collect::<Vec<_>>();
        synth(
            &mut doc,
            [0.0, 0.0, 200.0, 100.0],
            dictionary! { "Contents" => streams },
            Dictionary::new(),
            "",
        )
    }

    fn render(bytes: &[u8]) -> RgbImage {
        let meta = PdfMeta::open(bytes).expect("object model");
        render_page(&meta, 0, 1.0).expect("render")
    }

    fn px(img: &RgbImage, x: u32, y: u32) -> [u8; 3] {
        img.get_pixel(x, y).0
    }

    fn near(a: [u8; 3], b: [u8; 3], tol: i32) -> bool {
        a.iter()
            .zip(b)
            .all(|(&x, y)| (i32::from(x) - i32::from(y)).abs() <= tol)
    }

    const WHITE: [u8; 3] = [255, 255, 255];
    const RED: [u8; 3] = [255, 0, 0];

    /// Ink (anything not white) inside a device rectangle.
    fn ink_in(img: &RgbImage, x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
        let mut n = 0;
        for y in y0..y1 {
            for x in x0..x1 {
                if px(img, x, y) != WHITE {
                    n += 1;
                }
            }
        }
        n
    }

    #[test]
    fn fills_land_in_the_docling_parse_frame() {
        // A 200 × 100 page: canvas 200 × 100, y down, `re f` at (60, 10)
        // 40 × 30 covers x 60..100, y 60..90.
        let img = render(&page(Dictionary::new(), "1 0 0 rg 60 10 40 30 re f"));
        assert_eq!((img.width(), img.height()), (200, 100));
        assert_eq!(px(&img, 80, 75), RED);
        assert_eq!(px(&img, 61, 61), RED);
        assert_eq!(px(&img, 80, 55), WHITE);
        assert_eq!(px(&img, 105, 75), WHITE);
        assert_eq!(px(&img, 5, 5), WHITE);
    }

    #[test]
    fn leading_comment_content_stream_does_not_hide_later_streams() {
        let bytes = page_with_contents(&[
            b"% CANON_PFINF_TYPE2_TEXTOFF\n",
            b"1 0 0 rg 60 10 40 30 re f",
        ]);
        let img = render(&bytes);
        assert_eq!(px(&img, 80, 75), RED);
    }

    #[test]
    fn crop_box_is_the_canvas() {
        // CropBox [50 0 150 100] → a 100 × 100 canvas; the rectangle at
        // x 60..100 lands at canvas x 10..50.
        let mut doc = Document::with_version("1.5");
        let bytes = synth(
            &mut doc,
            [0.0, 0.0, 200.0, 100.0],
            dictionary! { "CropBox" => vec![50.into(), 0.into(), 150.into(), 100.into()] },
            Dictionary::new(),
            "1 0 0 rg 60 10 40 30 re f",
        );
        let img = render(&bytes);
        assert_eq!((img.width(), img.height()), (100, 100));
        assert_eq!(px(&img, 30, 75), RED);
        assert_eq!(px(&img, 5, 75), WHITE);
        assert_eq!(px(&img, 55, 75), WHITE);
    }

    #[test]
    fn rotate_transposes_the_canvas() {
        // /Rotate 90: the display canvas is 100 × 200 and the page's
        // bottom-left corner shows top-left.
        let mut doc = Document::with_version("1.5");
        let bytes = synth(
            &mut doc,
            [0.0, 0.0, 200.0, 100.0],
            dictionary! { "Rotate" => 90 },
            Dictionary::new(),
            "1 0 0 rg 0 0 40 20 re f",
        );
        let img = render(&bytes);
        assert_eq!((img.width(), img.height()), (100, 200));
        assert_eq!(px(&img, 10, 20), RED);
        assert_eq!(px(&img, 10, 45), WHITE);
        assert_eq!(px(&img, 90, 190), WHITE);
        let meta = PdfMeta::open(&bytes).unwrap();
        let sized = render_page_sized(&meta, 0, 50, 100).unwrap();
        assert_eq!((sized.width(), sized.height()), (50, 100));
        assert_eq!(px(&sized, 5, 10), RED);
    }

    #[test]
    fn hairline_strokes_are_one_pixel_wide() {
        // `0 w` is a hairline in PDF; docling-parse draws it at one pixel.
        let img = render(&page(Dictionary::new(), "0 w 10 50.5 m 190 50.5 l S"));
        assert!(ink_in(&img, 100, 49, 101, 51) >= 1);
        assert_eq!(ink_in(&img, 100, 40, 101, 48), 0);
        assert_eq!(ink_in(&img, 100, 52, 101, 60), 0);
        // A 4-pt line is four pixels tall.
        let img = render(&page(Dictionary::new(), "4 w 10 50 m 190 50 l S"));
        assert!(ink_in(&img, 100, 44, 101, 56) >= 4);
        assert_eq!(ink_in(&img, 100, 40, 101, 47), 0);
    }

    #[test]
    fn constant_alpha_blends_over_white() {
        let res = dictionary! {
            "ExtGState" => dictionary! {
                "GS" => dictionary! { "ca" => 0.5, "CA" => 0.25 },
            },
        };
        let img = render(&page(res, "/GS gs 0 g 20 20 160 60 re f"));
        let p = px(&img, 100, 50);
        assert!(near(p, [128, 128, 128], 2), "{p:?}");
    }

    #[test]
    fn rect_clip_cuts_the_fill() {
        let img = render(&page(
            Dictionary::new(),
            "q 0 0 100 100 re W n 0 0 1 rg 0 0 200 100 re f Q 0 0 200 10 re f",
        ));
        assert_eq!(px(&img, 50, 50), [0, 0, 255]);
        assert_eq!(px(&img, 150, 50), WHITE);
        // The clip ends with Q: the bottom strip is black across the page.
        assert_eq!(px(&img, 150, 95), [0, 0, 0]);
    }

    #[test]
    fn shape_clip_follows_the_path() {
        // A triangle clip: the fill shows inside the triangle only.
        let img = render(&page(
            Dictionary::new(),
            "q 0 0 m 200 0 l 100 100 l h W n 0 1 0 rg 0 0 200 100 re f Q",
        ));
        assert_eq!(px(&img, 100, 50), [0, 255, 0]);
        assert_eq!(px(&img, 100, 90), [0, 255, 0]);
        assert_eq!(px(&img, 5, 5), WHITE);
        assert_eq!(px(&img, 195, 5), WHITE);
    }

    #[test]
    fn image_xobject_is_blitted_with_its_orientation() {
        // 2 × 2 RGB: red green / blue black, first row at the top.
        let data = vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0];
        let mut doc = Document::with_version("1.5");
        let im = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 2, "Height" => 2, "BitsPerComponent" => 8,
                "ColorSpace" => "DeviceRGB",
            },
            data,
        ));
        let bytes = synth(
            &mut doc,
            [0.0, 0.0, 200.0, 100.0],
            Dictionary::new(),
            dictionary! { "XObject" => dictionary! { "Im0" => Object::Reference(im) } },
            "q 200 0 0 100 0 0 cm /Im0 Do Q",
        );
        let img = render(&bytes);
        // Bilinear sampling at the quadrant centres: the source colours
        // within tiny-skia's fixed-point rounding.
        assert!(near(px(&img, 50, 25), RED, 6));
        assert!(near(px(&img, 150, 25), [0, 255, 0], 6));
        assert!(near(px(&img, 50, 75), [0, 0, 255], 6));
        assert!(near(px(&img, 150, 75), [0, 0, 0], 6));
    }

    #[test]
    fn inline_image_and_stencil_mask() {
        // A 1 × 1 gray inline image over the left half, a 2 × 1 stencil
        // (bit 0 paints) in the fill colour over the right half.
        let mut content = b"q 100 0 0 100 0 0 cm BI /W 1 /H 1 /CS /G /BPC 8 ID ".to_vec();
        content.push(0x80);
        content.extend_from_slice(
            b" EI Q q 1 0 0 rg 100 0 0 100 100 0 cm BI /W 2 /H 1 /IM true /D [0 1] ID ",
        );
        content.push(0x40);
        content.extend_from_slice(b" EI Q");
        let mut doc = Document::with_version("1.5");
        let bytes = synth_bytes(
            &mut doc,
            [0.0, 0.0, 200.0, 100.0],
            Dictionary::new(),
            Dictionary::new(),
            &content,
        );
        let img = render(&bytes);
        assert!(near(px(&img, 50, 50), [128, 128, 128], 1));
        assert_eq!(px(&img, 125, 50), RED);
        assert_eq!(px(&img, 175, 50), WHITE);
    }

    #[test]
    fn axial_shading_interpolates() {
        let res = dictionary! {
            "Shading" => dictionary! {
                "Sh" => dictionary! {
                    "ShadingType" => 2, "ColorSpace" => "DeviceGray",
                    "Coords" => vec![0.into(), 0.into(), 200.into(), 0.into()],
                    "Function" => dictionary! {
                        "FunctionType" => 2, "Domain" => vec![0.into(), 1.into()],
                        "C0" => vec![0.into()], "C1" => vec![1.into()], "N" => 1,
                    },
                    "Extend" => vec![true.into(), true.into()],
                },
            },
        };
        let img = render(&page(res, "/Sh sh"));
        assert!(px(&img, 5, 50)[0] < 40);
        assert!(px(&img, 195, 50)[0] > 215);
        assert!(near(px(&img, 100, 50), [128, 128, 128], 12));
        // The same shading as a fill pattern, clipped by the path.
        let res = dictionary! {
            "Pattern" => dictionary! {
                "P" => dictionary! {
                    "PatternType" => 2,
                    "Shading" => dictionary! {
                        "ShadingType" => 2, "ColorSpace" => "DeviceGray",
                        "Coords" => vec![0.into(), 0.into(), 200.into(), 0.into()],
                        "Function" => dictionary! {
                            "FunctionType" => 2, "Domain" => vec![0.into(), 1.into()],
                            "C0" => vec![0.into()], "C1" => vec![1.into()], "N" => 1,
                        },
                    },
                },
            },
        };
        let img = render(&page(res, "/Pattern cs /P scn 0 0 100 100 re f"));
        assert!(px(&img, 5, 50)[0] < 40);
        assert!(near(px(&img, 50, 50), [64, 64, 64], 12));
        assert_eq!(px(&img, 150, 50), WHITE);
    }

    #[test]
    fn tiling_pattern_repeats_its_cell() {
        // A 20 × 20 cell with a 10 × 10 black square: half the area inked.
        let mut doc = Document::with_version("1.5");
        let pat = doc.add_object(Stream::new(
            dictionary! {
                "PatternType" => 1, "PaintType" => 1, "TilingType" => 1,
                "BBox" => vec![0.into(), 0.into(), 20.into(), 20.into()],
                "XStep" => 20, "YStep" => 20, "Resources" => Dictionary::new(),
            },
            b"0 g 0 0 10 10 re f".to_vec(),
        ));
        let bytes = synth(
            &mut doc,
            [0.0, 0.0, 200.0, 100.0],
            Dictionary::new(),
            dictionary! { "Pattern" => dictionary! { "P" => Object::Reference(pat) } },
            "/Pattern cs /P scn 0 0 200 100 re f",
        );
        let img = render(&bytes);
        let inked = ink_in(&img, 0, 0, 200, 100);
        assert!((4000..=6500).contains(&inked), "{inked}");
        // The cell at the origin: its square covers x 0..10, y 90..100.
        assert_eq!(px(&img, 5, 95), [0, 0, 0]);
        assert_eq!(px(&img, 15, 85), WHITE);
    }

    #[test]
    fn form_xobject_matrix_and_bbox() {
        // The form fills far beyond its BBox; only the BBox, moved by the
        // Matrix to x 100..150 / y 0..50, shows.
        let mut doc = Document::with_version("1.5");
        let fx = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 50.into(), 50.into()],
                "Matrix" => vec![1.into(), 0.into(), 0.into(), 1.into(), 100.into(), 0.into()],
            },
            b"1 0 0 rg -500 -500 1000 1000 re f".to_vec(),
        ));
        let bytes = synth(
            &mut doc,
            [0.0, 0.0, 200.0, 100.0],
            Dictionary::new(),
            dictionary! { "XObject" => dictionary! { "Fx" => Object::Reference(fx) } },
            "/Fx Do",
        );
        let img = render(&bytes);
        assert_eq!(px(&img, 125, 75), RED);
        assert_eq!(px(&img, 75, 75), WHITE);
        assert_eq!(px(&img, 125, 25), WHITE);
        assert_eq!(px(&img, 175, 75), WHITE);
    }

    #[test]
    fn transparency_group_alpha_reaches_its_contents() {
        // docling-parse pushes the group's `ca` down onto the contents.
        let mut doc = Document::with_version("1.5");
        let fx = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 200.into(), 100.into()],
                "Group" => dictionary! { "S" => "Transparency", "CS" => "DeviceRGB" },
            },
            b"0 g 0 0 200 100 re f".to_vec(),
        ));
        let bytes = synth(
            &mut doc,
            [0.0, 0.0, 200.0, 100.0],
            Dictionary::new(),
            dictionary! {
                "XObject" => dictionary! { "Fx" => Object::Reference(fx) },
                "ExtGState" => dictionary! { "GS" => dictionary! { "ca" => 0.5 } },
            },
            "/GS gs /Fx Do",
        );
        let img = render(&bytes);
        assert!(near(px(&img, 100, 50), [128, 128, 128], 2));
    }

    #[test]
    fn widgets_draw_their_appearance_unless_hidden() {
        let mut doc = Document::with_version("1.5");
        let ap = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            },
            b"1 0 0 rg 0 0 10 10 re f".to_vec(),
        ));
        let widget = |x0: i64, flags: i64, subtype: &str| -> Object {
            Object::Dictionary(dictionary! {
                "Type" => "Annot", "Subtype" => subtype, "F" => flags,
                "Rect" => vec![x0.into(), 20.into(), (x0 + 40).into(), 60.into()],
                "AP" => dictionary! { "N" => Object::Reference(ap) },
            })
        };
        let bytes = synth(
            &mut doc,
            [0.0, 0.0, 200.0, 100.0],
            dictionary! {
                "Annots" => vec![
                    widget(10, 4, "Widget"),
                    widget(60, 2, "Widget"),
                    widget(110, 4, "Square"),
                ],
            },
            Dictionary::new(),
            "",
        );
        let img = render(&bytes);
        // The visible widget: BBox 10 × 10 fitted to the 40 × 40 Rect.
        assert_eq!(px(&img, 30, 60), RED);
        assert_eq!(px(&img, 12, 42), RED);
        assert_eq!(px(&img, 48, 78), RED);
        assert_eq!(px(&img, 30, 30), WHITE);
        // Hidden, and a non-Widget annotation: not drawn.
        assert_eq!(px(&img, 80, 60), WHITE);
        assert_eq!(px(&img, 130, 60), WHITE);
    }

    #[test]
    fn text_paints_glyphs_from_a_fallback_face() {
        let style = font::fallback::style_for("Helvetica", None, None);
        if font::fallback::face(style).is_none() {
            eprintln!("no fallback fonts on this host — skipping");
            return;
        }
        let res = dictionary! {
            "Font" => dictionary! {
                "F1" => dictionary! {
                    "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
                },
            },
        };
        let img = render(&page(res.clone(), "BT /F1 40 Tf 20 30 Td (HELLO) Tj ET"));
        // Cap height ~0.72 em: ink between y 100−30−29 and the baseline.
        assert!(ink_in(&img, 20, 40, 160, 70) > 200);
        assert_eq!(ink_in(&img, 0, 0, 200, 35), 0);
        assert_eq!(ink_in(&img, 0, 75, 200, 100), 0);
        // Render mode 3 (invisible) draws nothing; a `Tz` of 50 % halves
        // the advance so the word ends earlier.
        let img = render(&page(
            res.clone(),
            "BT 3 Tr /F1 40 Tf 20 30 Td (HELLO) Tj ET",
        ));
        assert_eq!(ink_in(&img, 0, 0, 200, 100), 0);
        let wide = render(&page(res.clone(), "BT /F1 40 Tf 20 30 Td (HHHH) Tj ET"));
        let narrow = render(&page(res, "BT /F1 40 Tf 50 Tz 20 30 Td (HHHH) Tj ET"));
        let right = |img: &RgbImage| {
            (0..200)
                .rev()
                .find(|&x| ink_in(img, x, 0, x + 1, 100) > 0)
                .unwrap()
        };
        assert!(right(&wide) > right(&narrow) + 30);
    }

    #[test]
    fn type3_glyphs_run_their_procedures() {
        let mut doc = Document::with_version("1.5");
        let square = doc.add_object(Stream::new(
            Dictionary::new(),
            b"1000 0 0 0 750 750 d1 0 0 750 750 re f".to_vec(),
        ));
        let font = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type3",
            "FontBBox" => vec![0.into(), 0.into(), 750.into(), 750.into()],
            "FontMatrix" => vec![0.001.into(), 0.into(), 0.into(), 0.001.into(), 0.into(), 0.into()],
            "CharProcs" => dictionary! { "square" => Object::Reference(square) },
            "Encoding" => dictionary! {
                "Type" => "Encoding",
                "Differences" => vec![97.into(), Object::Name(b"square".to_vec())],
            },
            "FirstChar" => 97, "LastChar" => 97, "Widths" => vec![1000.into()],
            "Resources" => Dictionary::new(),
        });
        let bytes = synth(
            &mut doc,
            [0.0, 0.0, 200.0, 100.0],
            Dictionary::new(),
            dictionary! { "Font" => dictionary! { "F1" => Object::Reference(font) } },
            "BT /F1 40 Tf 20 20 Td 0 0 1 rg (aa) Tj ET",
        );
        let img = render(&bytes);
        // Two 30 × 30 squares 40 pt apart: x 20..50 and 60..90, y 50..80.
        assert_eq!(px(&img, 35, 65), [0, 0, 255]);
        assert_eq!(px(&img, 75, 65), [0, 0, 255]);
        assert_eq!(px(&img, 55, 65), WHITE);
        assert_eq!(px(&img, 105, 65), WHITE);
        assert_eq!(px(&img, 35, 45), WHITE);
    }

    #[test]
    fn cmyk_follows_the_docling_parse_model() {
        // docling-parse's CMYK model: pure K is (35, 31, 32), not black.
        let img = render(&page(Dictionary::new(), "0 0 0 1 k 0 0 200 100 re f"));
        assert_eq!(px(&img, 100, 50), [35, 31, 32]);
    }

    #[test]
    fn a_renderer_reuses_its_caches_across_renders() {
        // Two sizes of the same page through one Renderer: identical to
        // one-off renders (the image store and font cache change nothing).
        let data: Vec<u8> = (0..64 * 64 * 3).map(|i| (i % 251) as u8).collect();
        let mut doc = Document::with_version("1.5");
        let im = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 64, "Height" => 64, "BitsPerComponent" => 8,
                "ColorSpace" => "DeviceRGB",
            },
            data,
        ));
        let bytes = synth(
            &mut doc,
            [0.0, 0.0, 200.0, 100.0],
            Dictionary::new(),
            dictionary! { "XObject" => dictionary! { "Im0" => Object::Reference(im) } },
            "q 100 0 0 50 50 25 cm /Im0 Do Q q 20 0 0 20 0 0 cm /Im0 Do Q",
        );
        let meta = PdfMeta::open(&bytes).unwrap();
        let r = Renderer::new(&meta);
        let a1 = r.render(0, 200, 100).unwrap();
        let a2 = r.render(0, 400, 200).unwrap();
        let a3 = r.render(0, 200, 100).unwrap();
        assert_eq!(
            a1.as_raw(),
            render_page_sized(&meta, 0, 200, 100).unwrap().as_raw()
        );
        assert_eq!(
            a2.as_raw(),
            render_page_sized(&meta, 0, 400, 200).unwrap().as_raw()
        );
        assert_eq!(a1.as_raw(), a3.as_raw());
    }

    /// #598: a `JPXDecode` image draws its pixels, not a mid-gray block. The
    /// fixture is a lossless 12 × 9 gray gradient (`(x·21 + y·3) mod 256`):
    /// dark at the left edge, bright at the right; with alpha from the
    /// codestream only under `/SMaskInData`.
    #[test]
    fn jpx_image_renders_its_pixels() {
        let jp2 = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/jpx/gray_12x9.jp2"),
        )
        .unwrap();
        let mut doc = Document::with_version("1.5");
        let im = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 12, "Height" => 9,
                "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8,
                "Filter" => "JPXDecode",
            },
            jp2.clone(),
        ));
        let bytes = synth(
            &mut doc,
            [0.0, 0.0, 200.0, 100.0],
            Dictionary::new(),
            dictionary! { "XObject" => dictionary! { "Im0" => Object::Reference(im) } },
            "q 120 0 0 90 40 5 cm /Im0 Do Q",
        );
        let img = render(&bytes);
        // Image column 0 (codes 0–24) lands at device x 40–50, column 11
        // (codes 231–255) at x 150–160; the row at device y 50 is image row 4.
        let left = px(&img, 45, 50);
        let right = px(&img, 155, 50);
        assert!(left[0] < 40 && left == [left[0]; 3], "left pixel {left:?}");
        assert!(
            right[0] > 215 && right == [right[0]; 3],
            "right pixel {right:?}"
        );
        assert_eq!(px(&img, 20, 50), WHITE, "outside the image stays white");

        // The RGBA fixture: alpha is ignored without `/SMaskInData` (an
        // opaque blit) and honoured with it (the x = 0 column is fully
        // transparent, so the page shows through).
        let rgba = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/jpx/rgba_8x6.jp2"),
        )
        .unwrap();
        for (smask_in_data, transparent_left) in [(0, false), (1, true)] {
            let mut doc = Document::with_version("1.5");
            let im = doc.add_object(Stream::new(
                dictionary! {
                    "Type" => "XObject", "Subtype" => "Image",
                    "Width" => 8, "Height" => 6,
                    "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8,
                    "Filter" => "JPXDecode", "SMaskInData" => smask_in_data,
                },
                rgba.clone(),
            ));
            let bytes = synth(
                &mut doc,
                [0.0, 0.0, 200.0, 100.0],
                Dictionary::new(),
                dictionary! { "XObject" => dictionary! { "Im0" => Object::Reference(im) } },
                "q 160 0 0 60 20 20 cm /Im0 Do Q",
            );
            let img = render(&bytes);
            // Image column 0 → device x 20–40: colour (0, y·40, 128), alpha 0
            // (the bilinear blit lets a sliver of column 1, alpha 36, in).
            let p = px(&img, 30, 50);
            if transparent_left {
                assert!(near(p, WHITE, 3), "SMaskInData {smask_in_data}: {p:?}");
            } else {
                assert!(
                    p != WHITE && p[2] > 100,
                    "SMaskInData {smask_in_data}: {p:?}"
                );
            }
        }
    }

    /// A JBIG2 fixture (`tests/data/jbig2`) and its reference bitmap as the
    /// packed 1-bit samples the filter would output (0 = black).
    fn jbig2_fixture(name: &str) -> Vec<u8> {
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/data/jbig2")
                .join(name),
        )
        .unwrap()
    }

    fn jbig2_reference(name: &str) -> (usize, usize, Vec<u8>) {
        let img = ::image::load_from_memory(&jbig2_fixture(name))
            .unwrap()
            .to_luma8();
        let (w, h) = (img.width() as usize, img.height() as usize);
        let stride = w.div_ceil(8);
        let mut out = vec![0xffu8; stride * h];
        for (y, row) in img.rows().enumerate() {
            for (x, p) in row.enumerate() {
                if p.0[0] < 128 {
                    out[y * stride + x / 8] &= !(0x80 >> (x % 8));
                }
            }
        }
        (w, h, out)
    }

    /// One page drawing `image` (an image XObject stream) over the whole of
    /// a 440 × 180 pt page, at `scale` px/pt.
    fn page_with_image(stream: Stream, content: &str) -> RgbImage {
        let mut doc = Document::with_version("1.5");
        let im = doc.add_object(stream);
        let bytes = synth(
            &mut doc,
            [0.0, 0.0, 440.0, 180.0],
            Dictionary::new(),
            dictionary! { "XObject" => dictionary! { "Im0" => Object::Reference(im) } },
            content,
        );
        render(&bytes)
    }

    fn non_white_share(img: &RgbImage) -> f64 {
        let n = img.pixels().filter(|p| p.0 != WHITE).count();
        n as f64 / (img.width() as f64 * img.height() as f64)
    }

    /// #811: a JBIG2 `/ImageMask` stencil draws its pixels — the same page
    /// as the Flate and CCITT G4 (`/K -1`, the very same T.6 bytes)
    /// controls, pixel for pixel — where it used to draw nothing at all,
    /// leaving a scanned page's text out of the render.
    #[test]
    fn jbig2_stencil_renders_like_its_flate_and_ccitt_twins() {
        let (w, h, bits) = jbig2_reference("generic_mmr.png");
        let content = "q 0 0 0 rg 326 0 0 218 57 -19 cm /Im0 Do Q";
        let mask = |filter: Dictionary, data: Vec<u8>| {
            let mut d = dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "ImageMask" => true,
            };
            for (k, v) in filter.into_iter() {
                d.set(k, v);
            }
            Stream::new(d, data)
        };
        let flate = page_with_image(mask(Dictionary::new(), bits.clone()), content);
        let jbig2 = page_with_image(
            mask(
                dictionary! { "Filter" => "JBIG2Decode" },
                jbig2_fixture("generic_mmr.jbig2"),
            ),
            content,
        );
        let ccitt = page_with_image(
            mask(
                dictionary! {
                    "Filter" => "CCITTFaxDecode",
                    "DecodeParms" => dictionary! { "K" => -1, "Columns" => w as i64, "Rows" => h as i64 },
                },
                jbig2_fixture("generic_mmr.g4"),
            ),
            content,
        );
        let share = non_white_share(&flate);
        assert!(share > 0.05 && share < 0.95, "control share {share}");
        assert_eq!(jbig2.as_raw(), flate.as_raw(), "JBIG2 stencil vs Flate");
        assert_eq!(ccitt.as_raw(), flate.as_raw(), "CCITT stencil vs Flate");
    }

    /// A 1-bit `DeviceGray` JBIG2 image (not a stencil) and one whose
    /// symbol dictionary sits in `/JBIG2Globals` both draw their pixels,
    /// pixel-identical to the Flate image of the reference bitmap.
    #[test]
    fn jbig2_gray_image_and_globals_render_their_pixels() {
        let content = "q 326 0 0 218 57 -19 cm /Im0 Do Q";
        let gray = |w: usize, h: usize, extra: Dictionary, data: Vec<u8>| {
            let mut d = dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "ColorSpace" => "DeviceGray", "BitsPerComponent" => 1,
            };
            for (k, v) in extra.into_iter() {
                d.set(k, v);
            }
            Stream::new(d, data)
        };
        let (w, h, bits) = jbig2_reference("generic_mmr.png");
        let flate = page_with_image(gray(w, h, Dictionary::new(), bits), content);
        let jbig2 = page_with_image(
            gray(
                w,
                h,
                dictionary! { "Filter" => "JBIG2Decode" },
                jbig2_fixture("generic_mmr.jbig2"),
            ),
            content,
        );
        assert!(non_white_share(&flate) > 0.05);
        assert_eq!(jbig2.as_raw(), flate.as_raw(), "JBIG2 gray vs Flate");

        // The text region's symbols come from the globals stream.
        let (w, h, bits) = jbig2_reference("symbol_global.png");
        let flate = page_with_image(gray(w, h, Dictionary::new(), bits), content);
        let mut doc = Document::with_version("1.5");
        let globals = doc.add_object(Stream::new(
            Dictionary::new(),
            jbig2_fixture("symbol_global.globals.jbig2"),
        ));
        let im = doc.add_object(gray(
            w,
            h,
            dictionary! {
                "Filter" => "JBIG2Decode",
                "DecodeParms" => dictionary! { "JBIG2Globals" => Object::Reference(globals) },
            },
            jbig2_fixture("symbol_global.jbig2"),
        ));
        let bytes = synth(
            &mut doc,
            [0.0, 0.0, 440.0, 180.0],
            Dictionary::new(),
            dictionary! { "XObject" => dictionary! { "Im0" => Object::Reference(im) } },
            content,
        );
        let with_globals = render(&bytes);
        assert!(non_white_share(&flate) > 0.02);
        assert_eq!(
            with_globals.as_raw(),
            flate.as_raw(),
            "JBIG2 + globals vs Flate"
        );
    }

    /// A JBIG2 stream no decoder can read draws the documented mid-gray
    /// placeholder — for a stencil too (#811: a stencil used to draw
    /// nothing, so the page gave no sign that an image was missing).
    #[test]
    fn undecodable_jbig2_draws_the_placeholder_even_as_a_stencil() {
        let content = "q 0 0 0 rg 200 0 0 100 120 40 cm /Im0 Do Q";
        for is_mask in [true, false] {
            let mut d = dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 20, "Height" => 10, "Filter" => "JBIG2Decode",
            };
            if is_mask {
                d.set("ImageMask", true);
            } else {
                d.set("ColorSpace", "DeviceGray");
                d.set("BitsPerComponent", 1);
            }
            let img = page_with_image(Stream::new(d, b"not a JBIG2 stream".to_vec()), content);
            let inside = px(&img, 220, 90);
            assert!(
                near(inside, [128, 128, 128], 2),
                "mask {is_mask}: inside the image {inside:?}"
            );
            assert_eq!(
                px(&img, 50, 90),
                WHITE,
                "mask {is_mask}: outside stays white"
            );
        }
    }
}
