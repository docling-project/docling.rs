//! The page walk of the PDF pipeline: per page, the geometry and links from
//! the object model (`pdf_meta`), the text layer from the pure-Rust parser
//! (`textparse`), the model-input bitmaps from the docling-parse renderer
//! plugin, the Rust raster or the Rust page renderer — and, behind the
//! `pdfium` feature, the pdfium library for `DOCLING_RS_RENDERER=pdfium`
//! (docling's pypdfium2 chain) and for a file lopdf cannot read ([`native`]).
//! The module keeps its historical name; every type in it is pdfium-free.

#[cfg(feature = "render")]
use crate::PdfError;
#[cfg(feature = "ocr-prep")]
use image::RgbImage;

/// A run of text with its bounding box, in PDF points with a **top-left** origin
/// (pdfium's native origin is bottom-left; we flip it to match docling's
/// `BoundingBox(..., origin=TOPLEFT)`).
#[derive(Debug, Clone)]
pub struct TextCell {
    pub text: String,
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

/// Pixels-per-point used to render page images. Layout is scale-invariant (it
/// scales normalized boxes by the page point size), but OCR benefits from the
/// extra resolution.
pub const RENDER_SCALE: f32 = 2.0;

/// One page's geometry, extracted text cells, and a rendered RGB image. The
/// image is rendered at [`RENDER_SCALE`] pixels per PDF point; `image px =
/// page point × scale`.
#[derive(Clone)]
pub struct PdfPage {
    pub width: f32,
    pub height: f32,
    pub scale: f32,
    pub cells: Vec<TextCell>,
    /// Same text grouped for code regions: split only at pdfium space glyphs, so
    /// monospace runs keep their source spacing instead of the prose heuristic's.
    pub code_cells: Vec<TextCell>,
    /// Per-word cells (one per word, not joined into lines) for TableFormer cell
    /// matching.
    pub word_cells: Vec<TextCell>,
    /// Checkbox squares drawn on the page (#609): assembly gives each line
    /// with one in front of it its own checkbox item. Empty without a text
    /// layer (OCR pages) or vector paths.
    pub checkboxes: Vec<crate::checkbox::CheckBox>,
    /// The page's image placements (`[l, t, r, b]`, page points, the cells'
    /// frame), as the text parser walks past them (#999) — what docling's
    /// pdf-aware OCR calls the page's bitmaps: a layout region over one whose
    /// text layer does not account for it is OCR'd even though it holds
    /// programmatic text (a scan with a typed-in value). Empty without a text
    /// layer (an OCR page is OCR'd whole anyway).
    pub bitmaps: Vec<[f32; 4]>,
    /// The rendered page bitmap. Present whenever pixels are available at all
    /// (`ocr-prep` ⊂ `ml`): the native pipeline renders it with pdfium, the
    /// browser pipeline receives it from the host canvas. Picture regions are
    /// cropped out of it.
    #[cfg(feature = "ocr-prep")]
    pub image: RgbImage,
    /// The **scale-1.0** page image the layout model runs on (parity with
    /// docling's `PyPdfiumDocumentBackend`: the layout stage calls
    /// `page.get_image(scale=1.0)`, which that backend serves as pdfium at
    /// 1.5×, PIL-BICUBIC down to point size — a *different* image from the 2×
    /// OCR/crop bitmap above, and a different resampling regime than
    /// stretching that bitmap; docling 2.123+'s default docling-parse backend
    /// renders the same request with its own Blend2D/FreeType renderer, whose
    /// glyph anti-aliasing differs — #478). `None` on paths without a pdfium
    /// renderer (browser, METS/TIFF), which fall back to stretching
    /// [`Self::image`].
    #[cfg(feature = "ocr-prep")]
    pub image_layout: Option<RgbImage>,
    /// The 2x bitmap of a page with a text layer, deferred to the page
    /// walk's helper thread (#682): [`Self::image`] holds a 1×1 placeholder
    /// until [`Self::resolve_image`] installs the render. The ML pipeline
    /// resolves it after layout — the stage that reads
    /// [`Self::image_layout`] instead — and only for a page whose detections
    /// read the bitmap (tables, pictures, …; #897), so a text-only page never
    /// draws it. `None` once resolved, and on every path that renders
    /// synchronously (scanned pages, the plugin and pdfium renderers,
    /// images, METS).
    #[cfg(feature = "render")]
    pub pending_image: Option<PendingImage>,
    /// Hyperlink annotations on the page (rect in top-left page coords + target
    /// URI), restricted to web/mail/tel schemes. Used only by strict Markdown.
    pub links: Vec<LinkAnnot>,
    /// The page's `/Rotate` value (0/90/180/270) when it was normalized away
    /// before inference: a scanned page with `/Rotate` displays its raster
    /// rotated, which turns OCR into garbage — so extraction un-rotates the
    /// bitmaps (and swaps `width`/`height`) and records the display rotation
    /// here. Assembly rotates the finished geometry *back* by this many
    /// degrees clockwise, so emitted locations and the page size stay in
    /// display space (matching docling and every PDF viewer). Always 0 for
    /// text-layer pages (their cells live in display space already) and on
    /// paths without a pdfium renderer.
    pub rotation: u16,
    /// Keep typographic punctuation as drawn — curly and low quotes, dashes,
    /// `…`, `⁄`, `•` — instead of folding it to ASCII the way docling-parse's
    /// sanitizer does (`pdf_keep_typography`, #921). Read by assembly.
    pub keep_typography: bool,
    /// The page's image placements (#1007, docling's
    /// `use_embedded_picture_images`): a picture region matching exactly one
    /// safe image gets that bitmap at native resolution instead of a render
    /// crop. Empty unless the walk was asked for them
    /// ([`TextOptions::embedded_pictures`]).
    #[cfg(feature = "render")]
    pub embedded_images: Vec<crate::embedded::EmbeddedImage>,
}

impl PdfPage {
    /// A page built from recognized cells alone — the browser pipeline's
    /// shape (#157), where the bitmap lives on the JS side. Exists so callers
    /// compile identically with and without the `ml` feature: under a
    /// feature-unified workspace build the struct carries the `image` field,
    /// which a plain literal in a non-`ml` consumer can't spell.
    #[cfg(feature = "ocr-prep")]
    pub fn from_cells(width: f32, height: f32, scale: f32, cells: Vec<TextCell>) -> Self {
        Self {
            width,
            height,
            scale,
            cells,
            code_cells: Vec::new(),
            word_cells: Vec::new(),
            checkboxes: Vec::new(),
            bitmaps: Vec::new(),
            #[cfg(feature = "ocr-prep")]
            image: RgbImage::new(0, 0),
            #[cfg(feature = "ocr-prep")]
            image_layout: None,
            #[cfg(feature = "render")]
            pending_image: None,
            links: Vec::new(),
            rotation: 0,
            keep_typography: false,
            #[cfg(feature = "render")]
            embedded_images: Vec::new(),
        }
    }

    /// Wait for a deferred 2x render (#682) — starting it first when it was
    /// left for on demand ([`BitmapRender::OnDemand`], #897) — and install it
    /// as [`Self::image`]; a no-op when nothing is pending. The ML pipeline
    /// calls it after layout for a page whose detections need the bitmap,
    /// before anything reads it.
    #[cfg(feature = "render")]
    pub fn resolve_image(&mut self) -> Result<(), PdfError> {
        if let Some(pending) = self.pending_image.take() {
            self.image = crate::timing::timed("render.wait", || pending.wait())?;
        }
        Ok(())
    }

    /// Whether [`Self::image`] is still a placeholder for a deferred 2x
    /// render ([`Self::resolve_image`] installs it).
    #[cfg(feature = "render")]
    pub fn image_pending(&self) -> bool {
        self.pending_image.is_some()
    }

    /// Nothing is going to read this page's deferred 2x bitmap (#897): the
    /// helper skips its render if it has not begun it. The page stays
    /// pending — a later [`Self::resolve_image`] still gets the bitmap,
    /// rendering it then if it was skipped.
    #[cfg(feature = "render")]
    pub fn decline_image(&self) {
        if let Some(pending) = &self.pending_image {
            pending.decline();
        }
    }

    /// Same as [`from_cells`](Self::from_cells) but carrying the rendered page
    /// bitmap, so picture regions can be cropped out of it (#157: the browser
    /// pipeline gets the same figure bytes the native one does).
    #[cfg(feature = "ocr-prep")]
    pub fn from_cells_with_image(
        width: f32,
        height: f32,
        scale: f32,
        cells: Vec<TextCell>,
        image: RgbImage,
    ) -> Self {
        Self {
            image,
            ..Self::from_cells(width, height, scale, cells)
        }
    }

    /// Un-rotate the page's bitmaps by `deg` (clockwise 90° steps) and record
    /// the compensating display rotation, composing with any rotation already
    /// recorded: the raster becomes upright for inference while assembly
    /// still maps the finished geometry back into display space. Handles both
    /// `/Rotate` normalization (extraction) and content-detected orientation
    /// (#225) — the two compose additively (axis-aligned 90° rotations
    /// commute through the dimension swaps). Link rectangles follow the
    /// raster; `width`/`height` swap on odd quarter-turns.
    #[cfg(feature = "ocr-prep")]
    pub(crate) fn unrotate(&mut self, deg: u16) {
        if deg == 0 {
            return;
        }
        use image::imageops::{rotate180, rotate270, rotate90};
        // Display = upright rotated `deg`° clockwise, so upright = display
        // rotated the complementary amount clockwise.
        let un = |img: &RgbImage| match deg {
            90 => rotate270(img),
            180 => rotate180(img),
            _ => rotate90(img),
        };
        if self.image.width() > 1 {
            self.image = un(&self.image);
        }
        self.image_layout = self.image_layout.as_ref().map(&un);
        let (width, height) = (self.width, self.height);
        // Link rects follow the raster from display into upright space (the
        // inverse of the geometry rotation assembly applies at the end).
        for l in &mut self.links {
            let (nl, nt, nr, nb) = match deg {
                90 => (l.t, width - l.r, l.b, width - l.l),
                180 => (width - l.r, height - l.b, width - l.l, height - l.t),
                _ => (height - l.b, l.l, height - l.t, l.r),
            };
            (l.l, l.t, l.r, l.b) = (nl, nt, nr, nb);
        }
        // The text layer of a page with only a sparse one (a page number
        // beside outlined text, #936) follows the raster the same way; a
        // scan has none.
        for c in self
            .cells
            .iter_mut()
            .chain(&mut self.word_cells)
            .chain(&mut self.code_cells)
        {
            let (nl, nt, nr, nb) = match deg {
                90 => (c.t, width - c.r, c.b, width - c.l),
                180 => (width - c.r, height - c.b, width - c.l, height - c.t),
                _ => (height - c.b, c.l, height - c.t, c.r),
            };
            (c.l, c.t, c.r, c.b) = (nl, nt, nr, nb);
        }
        // Checkbox squares likewise (a scan has none; kept consistent).
        for c in &mut self.checkboxes {
            let (nl, nt, nr, nb) = match deg {
                90 => (c.t, width - c.r, c.b, width - c.l),
                180 => (width - c.r, height - c.b, width - c.l, height - c.t),
                _ => (height - c.b, c.l, height - c.t, c.r),
            };
            (c.l, c.t, c.r, c.b) = (nl, nt, nr, nb);
        }
        if deg != 180 {
            (self.width, self.height) = (height, width);
        }
        self.rotation = (self.rotation + deg) % 360;
    }
}

/// A PDF link annotation: its rectangle (top-left page coordinates, matching
/// [`TextCell`]) and target URI.
#[derive(Debug, Clone)]
pub struct LinkAnnot {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
    pub uri: String,
}

#[cfg(feature = "render")]
/// A parsed PDF: per-page text cells and page images.
pub struct PdfDocument {
    pub pages: Vec<PdfPage>,
}

#[cfg(feature = "render")]
impl PdfDocument {
    /// Parse a PDF from bytes, optionally decrypting with `password`.
    ///
    /// Note: this materialises **every** page's rendered bitmap in memory at
    /// once. For large documents prefer [`for_each_page`], which streams.
    pub fn open(bytes: &[u8], password: Option<&str>) -> Result<Self, PdfError> {
        let mut pages = Vec::new();
        for_each_page::<PdfError, _>(bytes, password, true, true, None, |_, _, mut page| {
            // Every page's bitmap, as documented — not the placeholder of a
            // render still on the helper thread (#682).
            page.resolve_image()?;
            pages.push(page);
            Ok(())
        })?;
        Ok(PdfDocument { pages })
    }
}

#[cfg(feature = "render")]
/// The document's text layer: the pure-Rust text parser (docling-parse's
/// char geometry; the only text source since phase 4 of "Retiring pdfium" —
/// on the whole corpus every page it reads no text from is one pdfium read
/// no text from either, i.e. a scan for the OCR path). `None` for a file
/// lopdf cannot open at all.
fn rust_parser_cells(
    bytes: &[u8],
    password: Option<&str>,
    word_gap: Option<crate::dp_lines::WordGap>,
) -> Option<crate::textparse::PageTextParser> {
    // Only the document load happens here; pages are parsed as the walk
    // reaches them (`cells_timed`), so nothing is decoded for pages outside
    // a `--pages` window and the parse overlaps the workers' inference.
    crate::timing::timed("textparse.open", || {
        let mut parser = crate::textparse::PageTextParser::open_with_password(bytes, password)?;
        parser.set_word_gap(word_gap);
        Some(parser)
    })
}

impl crate::textparse::PageTextParser {
    /// [`cells`](Self::cells) under the `textparse` timing stage (per page).
    fn cells_timed(&mut self, index: usize) -> crate::textparse::PageParserCells {
        crate::timing::timed("textparse", || self.cells(index))
    }
}

#[cfg(feature = "render")]
/// Number of pages in a PDF, without rendering any of them — used to decide
/// whether a document is worth spinning up the parallel worker pool.
pub fn page_count(bytes: &[u8], password: Option<&str>) -> Result<usize, PdfError> {
    // The pure-Rust object model first, then the docling-parse plugin's
    // count, then pdfium (the `pdfium` feature) — the order every entry point
    // here follows.
    if let Some(meta) = crate::timing::timed("meta.open", || {
        crate::pdf_meta::PdfMeta::open_with_password(bytes, password)
    })? {
        return Ok(meta.page_count());
    }
    if let Some(dp) = crate::dparse_render::Doc::open_if_enabled(bytes, password) {
        return Ok(dp.page_count());
    }
    crate::timing::timed("pdfium.page_count", || {
        let lib = native::bind()?;
        let n = lib.open(bytes, password)?.page_count();
        Ok(n)
    })
}

#[cfg(feature = "render")]
/// Render + extract pages one at a time, handing each (owned) [`PdfPage`] to `f`.
/// Only one page bitmap is resident at a time — a rendered page is ~5 MB, so a
/// large PDF would otherwise hold gigabytes of bitmaps at once. `f` receives the
/// zero-based page index and the total page count.
///
/// `render_image` controls whether the page bitmap is rasterized at all: layout,
/// OCR, TableFormer, and picture cropping all need it, but a caller that skips
/// every one of those (the `no_ocr` fast path) doesn't, and rasterizing +
/// downsampling a page is by far the most expensive step per page — skipping it
/// is most of `no_ocr`'s speedup. `PdfPage::image` is a 1×1 placeholder when
/// `false`; do not read it.
///
/// `extract_text` decodes the page's text layer (parser or pdfium cells); pass
/// `false` when full-page OCR is forced and the cells would be discarded
/// unread (docling#4061).
///
/// `range` restricts the walk to a **0-based inclusive** page window (issue
/// #80's `--pages`); out-of-window pages are skipped *before* text extraction
/// and rasterization, so a 3-page window over a 500-page PDF costs three
/// pages, not five hundred. `f` still receives the absolute page index, so
/// downstream page numbering refers to the source document.
///
/// `E` is the caller's error type; [`PdfError`]s convert into it via `From`.
///
/// A page with a text layer may arrive with its 2x bitmap still rendering
/// (#682): call [`PdfPage::resolve_image`] before reading [`PdfPage::image`].
/// The render starts as the page leaves the walk; [`walk_pages`] with
/// [`BitmapRender::OnDemand`] starts it only when resolved (#897).
pub fn for_each_page<E, F>(
    bytes: &[u8],
    password: Option<&str>,
    render_image: bool,
    extract_text: bool,
    range: Option<(usize, usize)>,
    f: F,
) -> Result<(), E>
where
    E: From<PdfError>,
    F: FnMut(usize, usize, PdfPage) -> Result<(), E>,
{
    walk_pages(
        bytes,
        password,
        render_image,
        extract_text,
        range,
        BitmapRender::Eager,
        TextOptions::from_env(),
        f,
    )
}

#[cfg(feature = "render")]
/// When the 2x bitmap of a page with a text layer is rendered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitmapRender {
    /// As soon as the page leaves the walk, on the helper thread while
    /// layout runs (#682) — for a caller that reads every page's bitmap.
    Eager,
    /// Started like [`Self::Eager`] while the document's recent pages mostly
    /// read their bitmap, left [`Self::OnDemand`] while they mostly do not
    /// (#897). Layout reads the separate 1x render, and a page whose
    /// detections hold no table, picture or other bitmap consumer never
    /// needs the 2x one: on a paper with a figure or table on most pages
    /// the render overlaps layout as before; on prose the wasted renders go.
    /// A started render the page then declines ([`PdfPage::decline_image`])
    /// is still skipped if the helper has not begun it.
    Adaptive,
    /// Only once [`PdfPage::resolve_image`] asks for it (#897): no render is
    /// ever wasted, but a page that needs one waits for it after layout
    /// instead of having it drawn meanwhile.
    OnDemand,
}

/// The text layer's settings a page walk applies (#921): the word-gap rule
/// of the line and word cells, and whether assembly keeps typographic
/// punctuation as drawn ([`PdfPage::keep_typography`]) — plus whether the
/// walk lists each page's embedded images for the picture images (#1007).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TextOptions {
    /// `None` = the process default (`DOCLING_RS_PDF_WORD_GAP`).
    pub word_gap: Option<crate::dp_lines::WordGap>,
    pub keep_typography: bool,
    /// Collect [`PdfPage::embedded_images`] — docling's
    /// `use_embedded_picture_images` (#1007). Off by default: the walk
    /// then reads no image placements and decodes nothing.
    pub embedded_pictures: bool,
}

impl TextOptions {
    /// The process defaults: `DOCLING_RS_PDF_WORD_GAP` and
    /// `DOCLING_RS_PDF_KEEP_TYPOGRAPHY`.
    pub fn from_env() -> Self {
        Self {
            word_gap: None,
            keep_typography: crate::assemble::keep_typography_from_env(),
            embedded_pictures: false,
        }
    }
}

#[cfg(feature = "render")]
/// [`for_each_page`] with the 2x bitmaps rendered as `bitmap` says, and the
/// text layer's words split by `word_gap` (#921; `None` = the process
/// default).
#[allow(clippy::too_many_arguments)]
pub fn walk_pages<E, F>(
    bytes: &[u8],
    password: Option<&str>,
    render_image: bool,
    extract_text: bool,
    range: Option<(usize, usize)>,
    bitmap: BitmapRender,
    text: TextOptions,
    mut f: F,
) -> Result<(), E>
where
    E: From<PdfError>,
    F: FnMut(usize, usize, PdfPage) -> Result<(), E>,
{
    // The pure-Rust object model: page count, geometry, `/Rotate`, links.
    // `None` only for a file lopdf cannot read even after the parser's
    // repairs; pdfium (the `pdfium` feature) then answers for it.
    let meta = crate::timing::timed("meta.open", || {
        crate::pdf_meta::PdfMeta::open_with_password(bytes, password)
    })?;
    // The docling-parse renderer (#478, `DOCLING_RS_RENDERER=docling-parse`):
    // the page images the models see come from it when it is asked for.
    let dparse = if render_image {
        crate::dparse_render::Doc::open_if_enabled(bytes, password)
    } else {
        None
    };
    // pdfium, when it has a job (`bind_or_skip`).
    let pdfium = bind_or_skip(meta.is_some())?;
    let session = match &pdfium {
        Some(p) => Some(p.open(bytes, password)?),
        None => None,
    };
    // `extract_text = false` (full-page OCR forced, docling#4061 / 2.122):
    // the text layer would be cleared unread, so neither the pure-Rust parser
    // nor pdfium's text page is decoded at all — on vector-dense pages (CAD
    // drawings as 100k+ path segments) that decode is most of the page cost.
    let mut rust = if extract_text {
        rust_parser_cells(bytes, password, text.word_gap)
    } else {
        None
    };
    // pdfium's count when it is loaded (a damaged file can make the two
    // object models disagree, and pdfium's pages are the ones rendered), the
    // object model's otherwise.
    let total = match (&session, &meta) {
        (Some(s), _) => s.page_count(),
        (None, Some(m)) => m.page_count(),
        (None, None) => 0,
    };
    // The object model's geometry and links are used whenever it read the same
    // number of pages pdfium did (or pdfium is absent); a disagreeing file keeps
    // pdfium's answers throughout.
    //
    // Shared, because the helper thread that renders the deferred 2x bitmaps
    // (#682) outlives the walk: a page resolved on demand (#897) asks for its
    // render after the walk has handed the last page over.
    let meta = meta
        .filter(|m| {
            session
                .as_ref()
                .is_none_or(|s| s.page_count() == m.page_count())
        })
        .map(std::sync::Arc::new);
    // The pure-Rust renderer (phase 3 of retiring pdfium): the model inputs
    // of every page the plugin does not render and the Rust raster declines
    // — the default renderer.
    let renderer = match (&meta, render_image) {
        (Some(m), true) => Some(crate::render::Renderer::new(m)),
        _ => None,
    };
    let (first, last) = range.unwrap_or((0, total.saturating_sub(1)));
    // The 2x bitmap of a page with a text layer renders on a helper thread
    // (#682, `DeferredRender`) when the pure-Rust renderer is the only one
    // drawing: the plugin and pdfium hold per-document handles the walk
    // thread owns, so their pages render in place as before.
    let defer = match (&meta, &renderer, &dparse, &session) {
        (Some(m), Some(_), None, None) => DeferredRender::spawn(m.clone(), bitmap),
        _ => None,
    };
    {
        // Index the window directly: iterating pdfium's pages from page 0 and
        // skipping to `first` would load (and close) every page before the
        // window — ~0.7 ms each, 1.3 s of pure overhead for a one-page window
        // over the 1913-page .NET reference.
        for i in first..=last {
            if i >= total {
                break;
            }
            let page = match &session {
                Some(s) => Some(s.page(i)?),
                None => None,
            };
            let geom = match (&meta, &page) {
                (Some(m), _) => m.geometry(i).ok_or_else(|| no_raster(i as i32))?,
                (None, Some(p)) => p.geom(),
                (None, None) => return Err(no_raster(i as i32).into()),
            };
            let links = match (&meta, &page) {
                (Some(m), _) => m.links(i),
                (None, Some(p)) => p.links(geom.unrotated().1),
                (None, None) => Vec::new(),
            };
            let rc = rust.as_mut().map(|p| p.cells_timed(i));
            let mut extracted = extract_page(
                PageSources {
                    page: page.as_ref(),
                    dparse: dparse.as_ref(),
                    meta: meta.as_deref(),
                    renderer: renderer.as_ref(),
                    defer: defer.as_ref(),
                },
                geom,
                links,
                i as i32,
                rc,
                render_image,
            )?;
            extracted.keep_typography = text.keep_typography;
            if text.embedded_pictures && extracted.rotation == 0 {
                if let Some(m) = meta.as_deref() {
                    extracted.embedded_images = crate::timing::timed("embedded_images", || {
                        crate::embedded::page_images(m, i)
                    });
                }
            }
            f(i, total, extracted)?;
        }
        // The helper ends once this sender and every page's handle are gone.
        drop(defer);
    }
    // Tearing down the parsed document (hundreds of thousands of lopdf
    // objects on a long PDF — 250 ms for the 1913-page .NET reference) is
    // nobody's business but the allocator's: hand it to a detached thread so
    // the last page's output isn't held up by it. `Arc`, not `Rc`, in the
    // caches is what makes the parser `Send`.
    if let Some(parser) = rust {
        std::thread::spawn(move || crate::timing::timed("textparse.close", || drop(parser)));
    }
    Ok(())
}

/// One rasterized page from [`render_pages`] (#243): the absolute 1-based page
/// number in the source document, the pixel dimensions, and the PNG bytes.
#[cfg(feature = "render")]
#[derive(Debug, Clone)]
pub struct RenderedPage {
    pub page_no: usize,
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
    /// The images the page's render drew as a placeholder block or left out,
    /// each with the reason (#916) — empty when everything decoded. Only the
    /// pure-Rust renderer reports; the docling-parse plugin and pdfium
    /// renders leave it empty.
    pub omissions: Vec<crate::render::Omission>,
}

/// Upper bound, in pixels, on a rendered page bitmap's side. A crafted PDF can
/// declare an enormous `MediaBox` in a few hundred bytes; the page render then
/// asks pdfium — and `into_rgb8` — to allocate `w * h * 4` bytes. At the
/// pipeline's 3x supersample a 12000 pt box is 36000x36000 ~ 5 GB: pdfium
/// returns an opaque internal error at the extreme, and just below it the
/// `image` crate *panics* (a `TryReserveError`, not a recoverable error) when
/// the allocation fails. Real pages, even large-format (A0 at 3x ~ 10110 px),
/// stay well under this cap; it only rejects the implausible, turning an abort
/// into a clean error. Mirrors `decode_image_limited`'s guard on the
/// standalone-image path. `DOCLING_RS_MAX_RENDER_PIXELS` overrides it.
#[cfg(feature = "render")]
fn max_render_side() -> u32 {
    static M: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *M.get_or_init(|| docling_core::env::parse("DOCLING_RS_MAX_RENDER_PIXELS").unwrap_or(15_000))
}

/// Round float pixel dimensions to the `i32` pdfium wants, rejecting a page
/// whose bitmap would exceed [`max_render_side`] on either side before either
/// pdfium or `image` tries to allocate it.
#[cfg(feature = "render")]
fn checked_render_dims(
    w_px: f64,
    h_px: f64,
    page_no: usize,
) -> Result<(i32, i32), crate::PdfError> {
    let cap = max_render_side();
    let w = w_px.round().max(1.0);
    let h = h_px.round().max(1.0);
    if w > f64::from(cap) || h > f64::from(cap) {
        return Err(crate::PdfError::Pdfium(format!(
            "page {page_no}: render size {w:.0}x{h:.0} px exceeds the {cap}px per-side cap \
             (raise DOCLING_RS_MAX_RENDER_PIXELS); the page's declared size is implausibly large"
        )));
    }
    Ok((w as i32, h as i32))
}

#[cfg(feature = "render")]
/// Rasterize a PDF's pages to PNG (#243) — the lean path behind serve's
/// `to=images`: pdfium render only, no text extraction, no models, and only
/// one page bitmap resident at a time (each is PNG-encoded and dropped before
/// the next renders). `scale` is pixels per PDF point — 2.0 matches the
/// pipeline's [`RENDER_SCALE`] (144 dpi). Unlike the pipeline's render there
/// is no 1.5× supersample + downsample pass: that dance exists only because
/// TableFormer is pixel-pinned to docling's bitmaps, and nothing downstream
/// of this output is — a single render is nearly twice as fast.
///
/// `range` is a **1-based** inclusive page window (issue #80's `pages`
/// semantics: the end clamps to the document, a start past the end errors).
///
/// pdfium is not thread-safe — callers must serialize this against any other
/// pdfium use (docling-serve holds its pipeline mutex around this call for
/// exactly that reason).
pub fn render_pages(
    bytes: &[u8],
    password: Option<&str>,
    range: Option<(usize, usize)>,
    scale: f32,
) -> Result<Vec<RenderedPage>, crate::PdfError> {
    // The docling-parse renderer plugin renders the display bitmap when it is
    // asked for (full-resolution bitmap decode: this is a viewer's render,
    // not a model input); the Rust raster / renderer otherwise, pdfium under
    // `DOCLING_RS_RENDERER=pdfium` or for a file lopdf cannot read.
    let dparse = crate::dparse_render::Doc::open_if_enabled(bytes, password);
    // The object model: the page count and the Rust raster / renderer.
    let meta = crate::pdf_meta::PdfMeta::open_with_password(bytes, password)?;
    let pdfium = match &dparse {
        Some(_) => None,
        None => bind_or_skip(meta.is_some())?,
    };
    let session = match &pdfium {
        Some(p) => Some(p.open(bytes, password)?),
        None => None,
    };
    let total = match (&session, &dparse, &meta) {
        (Some(s), _, _) => s.page_count(),
        (None, Some(dp), _) => dp.page_count(),
        (None, None, Some(m)) => m.page_count(),
        (None, None, None) => 0,
    };
    let (first, last) = match range {
        None => (0, total.saturating_sub(1)),
        Some((first, last)) => {
            if first == 0 || last < first {
                return Err(PdfError::Document(format!(
                    "invalid page range {first}-{last} (pages are 1-based, first <= last)"
                )));
            }
            if first > total {
                return Err(PdfError::Document(format!(
                    "page range {first}-{last} is outside the document ({total} page(s))"
                )));
            }
            (first - 1, last.min(total) - 1)
        }
    };
    let renderer = match (&dparse, &meta) {
        // Page rasters a caller keeps decode every image at full size (the
        // shim is asked with bitmap hint 0.0 below for the same reason).
        (None, Some(m)) => Some(crate::render::Renderer::with_bitmap_hint(m, 0.0)),
        _ => None,
    };
    let mut out = Vec::with_capacity(last.saturating_sub(first) + 1);
    for i in first..=last {
        if i >= total {
            break;
        }
        // Both renderers apply /Rotate themselves, so the bitmap is the page as
        // a viewer shows it — no orientation handling needed (the pipeline's
        // scanned-page un-rotation is an OCR-conformance concern, not a display
        // one).
        // The plugin's canvas first (the renderer whenever it resolves), then
        // the Rust raster of an image-only page (pdfium's bitmap byte for
        // byte, no library needed) or the Rust page renderer, then pdfium
        // (`DOCLING_RS_RENDERER=pdfium`, or a file lopdf cannot read).
        // A page the per-side cap refuses is an error naming the cap (#916),
        // not a missing renderer.
        let rust = || -> Result<Option<RgbImage>, PdfError> {
            let Some(g) = meta.as_ref().and_then(|m| m.geometry(i)) else {
                return Ok(None);
            };
            let (tw, th) = checked_render_dims(
                f64::from(g.width * scale),
                f64::from(g.height * scale),
                i + 1,
            )?;
            Ok(rust_bitmap(
                meta.as_ref(),
                renderer.as_ref(),
                session.is_some(),
                i as i32,
                tw as u32,
                th as u32,
                0.0,
            ))
        };
        let bitmap = match (&dparse, &session) {
            (Some(dp), _) => {
                crate::timing::timed("dparse.rasterize", || dp.render(i, f64::from(scale), 0.0))
                    .map_err(PdfError::Document)?
            }
            (None, session) => match rust()? {
                Some(img) => img,
                None => {
                    let Some(s) = session else {
                        return Err(no_raster(i as i32));
                    };
                    let page = s.page(i)?;
                    let (pw, ph) = page.size();
                    let (tw, th) =
                        checked_render_dims(f64::from(pw * scale), f64::from(ph * scale), i + 1)?;
                    page.render(tw, th, "pdfium.rasterize")?
                }
            },
        };
        let mut png = Vec::new();
        bitmap
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(|e| PdfError::Document(format!("PNG-encoding page {}: {e}", i + 1)))?;
        out.push(RenderedPage {
            page_no: i + 1,
            width: bitmap.width(),
            height: bitmap.height(),
            png,
            omissions: renderer
                .as_ref()
                .map(crate::render::Renderer::take_omissions)
                .unwrap_or_default(),
        });
    }
    Ok(out)
}

#[cfg(feature = "render")]
/// The error for a page that needs a raster when the object model could not
/// be read and neither the docling-parse renderer plugin nor pdfium is there.
fn no_raster(index: i32) -> PdfError {
    PdfError::Document(format!(
        "page {}: no page renderer — the file's object model could not be read (lopdf), \
         the docling-parse renderer plugin is not installed (.docling-parse/lib) and \
         pdfium is not available (the `pdfium` feature + PDFIUM_DYNAMIC_LIB_PATH / \
         .pdfium/lib)",
        index + 1
    ))
}

/// The pure-Rust bitmap of a page at `width` × `height`: the raster of an
/// image-only page (`raster::render`, pdfium's bytes) when the page
/// qualifies, the page renderer (`render`) otherwise — `None` only when the
/// object model is not loaded, or when `DOCLING_RS_RENDERER=pdfium` asks for
/// pdfium's render of a page the raster declines *and* pdfium's page is open
/// (`pdfium_page`; a build or machine without the library degrades to the
/// Rust renderer, as [`bind_or_skip`] warned).
#[cfg(feature = "render")]
fn rust_bitmap(
    meta: Option<&crate::pdf_meta::PdfMeta>,
    renderer: Option<&crate::render::Renderer<'_>>,
    pdfium_page: bool,
    index: i32,
    width: u32,
    height: u32,
    bitmap_hint: f64,
) -> Option<RgbImage> {
    let meta = meta?;
    if let Some(img) = crate::timing::timed("raster.render", || {
        crate::raster::render(meta, index as usize, width, height)
    }) {
        return Some(img);
    }
    if pdfium_page && crate::dparse_render::choice() == crate::dparse_render::Choice::Pdfium {
        return None;
    }
    let renderer = renderer?;
    crate::timing::timed("render.page", || {
        renderer.render_with_hint(index as usize, width, height, bitmap_hint)
    })
}

#[cfg(feature = "render")]
/// Bind pdfium for a conversion, or decide it can run without it. pdfium has
/// a job in two cases only: its own render was asked for
/// (`DOCLING_RS_RENDERER=pdfium`, which only the `pdfium` feature offers), or
/// the pure-Rust object model could not read the file (`meta_ok` false) — then
/// pdfium's page tree, geometry and render stand in, and a build without the
/// feature (or without the library) fails the conversion with the hint.
/// Otherwise the library is never loaded.
fn bind_or_skip(meta_ok: bool) -> Result<Option<native::Lib>, PdfError> {
    if meta_ok && crate::dparse_render::choice() != crate::dparse_render::Choice::Pdfium {
        return Ok(None);
    }
    match native::bind() {
        Ok(p) => Ok(Some(p)),
        Err(e) if meta_ok => {
            // The library was asked for by name and is not there: say so
            // once, then degrade — the Rust renderer draws every page.
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                eprintln!(
                    "docling-pdf: DOCLING_RS_RENDERER=pdfium but the pdfium library could not be \
                     loaded ({e}); rendering with the Rust renderer"
                );
            });
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// The native sources a page may draw on, each optional: pdfium's page handle
/// (present only when [`bind_or_skip`] opened the library) and the
/// docling-parse renderer (absent when the plugin is not asked for or no
/// raster is wanted).
#[cfg(feature = "render")]
#[derive(Clone, Copy)]
struct PageSources<'a> {
    page: Option<&'a native::Page<'a>>,
    dparse: Option<&'a crate::dparse_render::Doc>,
    /// The object model, for the Rust raster of an image-only page.
    meta: Option<&'a crate::pdf_meta::PdfMeta>,
    /// The pure-Rust page renderer over the same object model (its
    /// per-document caches live for the whole conversion).
    renderer: Option<&'a crate::render::Renderer<'a>>,
    /// The helper thread that renders the 2x bitmap of pages with a text
    /// layer off the walk (#682); `None` when another renderer draws.
    defer: Option<&'a DeferredRender>,
}

/// A page's 2x bitmap, rendered on the walk's helper thread (#682): queued
/// until something asks for it ([`BitmapRender::OnDemand`]) or sent at once
/// ([`BitmapRender::Eager`], [`BitmapRender::Adaptive`]), the result
/// arriving through a one-shot channel. A sent job can be declined: the
/// helper then skips it if it has not started it, and a later resolve sends
/// it again (#897). Cloning a page clones the handle; whichever copy
/// resolves first gets the bitmap. A handle dropped while still queued, or
/// declined in time, costs nothing — the render never happens.
#[cfg(feature = "render")]
#[derive(Clone)]
pub struct PendingImage(std::sync::Arc<std::sync::Mutex<Pending>>);

/// The channel a sent job's bitmap comes back through.
#[cfg(feature = "render")]
type Reply = std::sync::mpsc::Receiver<Result<RgbImage, PdfError>>;

/// Where a [`PendingImage`] stands: the job and the helper to send it to,
/// plus — once sent — the reply channel and the flag that lets the helper
/// skip it.
#[cfg(feature = "render")]
struct Pending {
    spec: RenderSpec,
    tx: std::sync::mpsc::Sender<RenderJob>,
    sent: Option<(Reply, std::sync::Arc<std::sync::atomic::AtomicBool>)>,
    taken: bool,
    /// The document's recent verdicts, which [`BitmapRender::Adaptive`]
    /// schedules by; this page's goes in once.
    stats: std::sync::Arc<Verdicts>,
    /// This page's verdict so far (`Some(true)` = read).
    verdict: Option<bool>,
}

/// Whether the last [`Verdicts::WINDOW`] pages that reached a verdict read
/// their 2x bitmap — what [`BitmapRender::Adaptive`] decides a new page's
/// render by (#897).
#[cfg(feature = "render")]
#[derive(Default)]
struct Verdicts(std::sync::Mutex<std::collections::VecDeque<bool>>);

#[cfg(feature = "render")]
impl Verdicts {
    const WINDOW: usize = 8;

    /// Record a page's verdict, or turn a declined page's into a read one.
    fn record(&self, read: bool, replaces_decline: bool) {
        let mut w = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if replaces_decline {
            if let Some(v) = w.iter_mut().rev().find(|v| !**v) {
                *v = true;
            }
            return;
        }
        if w.len() == Self::WINDOW {
            w.pop_front();
        }
        w.push_back(read);
    }

    /// Start the next render now? Yes until a few verdicts are in (most
    /// documents' first pages are not representative either way, and a
    /// wrong "yes" costs one render, a wrong "no" one page's latency), then
    /// while at least half of the window read theirs.
    fn start_eagerly(&self) -> bool {
        let w = self.0.lock().unwrap_or_else(|p| p.into_inner());
        w.len() < 3 || 2 * w.iter().filter(|v| **v).count() >= w.len()
    }
}

#[cfg(feature = "render")]
impl Pending {
    /// Send the job (again, after a skipped one) and keep its reply channel.
    /// The helper runs while any job sender lives — this one included — so a
    /// failed send means it died (a render panicked).
    fn send(&mut self) -> Result<(), PdfError> {
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        let declined = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.tx
            .send(RenderJob {
                spec: self.spec,
                declined: declined.clone(),
                reply,
            })
            .map_err(|_| PdfError::Pdfium("page render thread ended early".into()))?;
        self.sent = Some((rx, declined));
        Ok(())
    }
}

#[cfg(feature = "render")]
impl PendingImage {
    fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Hand the job to the helper, unless that happened already.
    fn start(&self) {
        let mut p = self.lock();
        if p.sent.is_none() && !p.taken {
            // A dead helper surfaces as the page's resolve error.
            let _ = p.send();
        }
    }

    /// Nothing will read this bitmap: let the helper skip the job if it has
    /// not begun it (a job never sent stays unsent).
    fn decline(&self) {
        let mut p = self.lock();
        if p.verdict.is_none() {
            p.verdict = Some(false);
            p.stats.record(false, false);
        }
        if let Some((_, declined)) = &p.sent {
            declined.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn wait(self) -> Result<RgbImage, PdfError> {
        let mut p = self.lock();
        if p.taken {
            return Err(PdfError::Pdfium("page bitmap resolved twice".into()));
        }
        if p.verdict != Some(true) {
            p.stats.record(true, p.verdict == Some(false));
            p.verdict = Some(true);
        }
        loop {
            match &p.sent {
                None => p.send()?,
                Some((rx, declined)) => {
                    // Asked for after all: a job not yet begun renders.
                    declined.store(false, std::sync::atomic::Ordering::Relaxed);
                    match rx.recv() {
                        Ok(out) => {
                            p.taken = true;
                            p.sent = None;
                            return out;
                        }
                        // Skipped while declined (or the helper died, which
                        // the next send reports): send it again.
                        Err(_) => p.sent = None,
                    }
                }
            }
        }
    }
}

#[cfg(feature = "render")]
impl std::fmt::Debug for PendingImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PendingImage")
    }
}

/// One deferred render: the 3x supersample of page `index` at `w` × `h`
/// with the decode `hint`, box-downscaled to `dw` × `dh` — exactly the
/// synchronous path's steps, on another thread.
#[cfg(feature = "render")]
#[derive(Clone, Copy)]
struct RenderSpec {
    index: i32,
    w: u32,
    h: u32,
    hint: f64,
    dw: u32,
    dh: u32,
}

#[cfg(feature = "render")]
struct RenderJob {
    spec: RenderSpec,
    /// Set when the page turned out not to read the bitmap (#897): the
    /// helper skips the job if it gets to it after that — dropping `reply`,
    /// which tells a later resolve to send it again.
    declined: std::sync::Arc<std::sync::atomic::AtomicBool>,
    reply: std::sync::mpsc::SyncSender<Result<RgbImage, PdfError>>,
}

/// The walk's render helper (#682): a second pure-Rust renderer, with its
/// own per-document caches, on its own thread, fed one [`RenderJob`] per
/// 2x bitmap asked for. Its bitmaps are the ones the walk thread would have
/// drawn — the same renderer code over the same object model — so the
/// output is byte-identical; what changes is that the page leaves the walk
/// as soon as its 1x layout image is drawn, and the 2x bitmap lands while
/// layout runs ([`BitmapRender::Eager`]) or when the page turns out to need
/// it ([`BitmapRender::OnDemand`], #897). Not scoped to the walk: a page
/// resolved on demand asks after the walk is over, so the thread holds the
/// object model and runs until the walk and every page's handle have let go
/// of the job sender.
#[cfg(feature = "render")]
struct DeferredRender {
    tx: std::sync::mpsc::Sender<RenderJob>,
    mode: BitmapRender,
    stats: std::sync::Arc<Verdicts>,
}

#[cfg(feature = "render")]
impl DeferredRender {
    /// The helper for `meta`; `None` (render in place, as without one) when
    /// the thread cannot be spawned.
    fn spawn(meta: std::sync::Arc<crate::pdf_meta::PdfMeta>, mode: BitmapRender) -> Option<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("docling-pdf-render".into())
            .spawn(move || Self::serve(&meta, rx))
            .ok()?;
        Some(Self {
            tx,
            mode,
            stats: Default::default(),
        })
    }

    fn submit(&self, index: i32, w: u32, h: u32, hint: f64, dw: u32, dh: u32) -> PendingImage {
        let spec = RenderSpec {
            index,
            w,
            h,
            hint,
            dw,
            dh,
        };
        let pending = PendingImage(std::sync::Arc::new(std::sync::Mutex::new(Pending {
            spec,
            tx: self.tx.clone(),
            sent: None,
            taken: false,
            stats: self.stats.clone(),
            verdict: None,
        })));
        let start = match self.mode {
            BitmapRender::Eager => true,
            BitmapRender::Adaptive => self.stats.start_eagerly(),
            BitmapRender::OnDemand => false,
        };
        if start {
            pending.start();
        }
        pending
    }

    /// The helper's loop: runs until every job sender is dropped.
    fn serve(meta: &crate::pdf_meta::PdfMeta, jobs: std::sync::mpsc::Receiver<RenderJob>) {
        let renderer = crate::render::Renderer::new(meta);
        for RenderJob {
            spec,
            declined,
            reply,
        } in jobs
        {
            if declined.load(std::sync::atomic::Ordering::Relaxed) {
                if crate::timing::enabled() {
                    crate::timing::record("render.skipped", 0);
                }
                continue;
            }
            let out = rust_bitmap(
                Some(meta),
                Some(&renderer),
                false,
                spec.index,
                spec.w,
                spec.h,
                spec.hint,
            )
            .map(|big| {
                crate::timing::timed("image.resize", || fast_downscale(&big, spec.dw, spec.dh))
            })
            .ok_or_else(|| no_raster(spec.index));
            let _ = reply.send(out);
        }
    }
}

#[cfg(feature = "render")]
fn extract_page(
    sources: PageSources<'_>,
    geom: crate::pdf_meta::PageGeom,
    links: Vec<LinkAnnot>,
    index: i32,
    rust_cells: Option<crate::textparse::PageParserCells>,
    render_image: bool,
) -> Result<PdfPage, PdfError> {
    let PageSources {
        page,
        dparse,
        meta,
        renderer,
        defer,
    } = sources;
    // The page size (and the render) is the *display* frame — `/Rotate`
    // applied — while every text coordinate (pdfium's own text page, the
    // pure-Rust parser's MediaBox-based glyphs, link annotation rects) lives
    // in the unrotated frame (docling#4008, 2.121). Keep the unrotated box
    // around for the y-flips and bring every rect into the display frame.
    let width = geom.width;
    let height = geom.height;
    let rotation = geom.rotation;
    let (unrot_w, unrot_h) = geom.unrotated();

    // The text layer: the pure-Rust parser's prose, word and code cells (its
    // word grouping reproduces docling-parse's, which TableFormer matches
    // against). A page it reads nothing from is a scanned page for the OCR
    // path — pdfium's text page is gone (phase 4 of "Retiring pdfium").
    let rc = rust_cells.unwrap_or_default();
    let (mut cells, mut code_cells, mut word_cells) = (rc.prose, rc.code, rc.words);
    let mut checkboxes = rc.checkboxes;
    let mut bitmaps = rc.bitmaps;
    if rotation != 0 {
        for c in cells
            .iter_mut()
            .chain(word_cells.iter_mut())
            .chain(code_cells.iter_mut())
        {
            let (l, t, r, b) = to_display_frame((c.l, c.t, c.r, c.b), rotation, unrot_w, unrot_h);
            (c.l, c.t, c.r, c.b) = (l, t, r, b);
        }
        for c in &mut checkboxes {
            let (l, t, r, b) = to_display_frame((c.l, c.t, c.r, c.b), rotation, unrot_w, unrot_h);
            (c.l, c.t, c.r, c.b) = (l, t, r, b);
        }
        for [l, t, r, b] in &mut bitmaps {
            let d = to_display_frame((*l, *t, *r, *b), rotation, unrot_w, unrot_h);
            (*l, *t, *r, *b) = d;
        }
    }

    // The docling-parse renderer (#478, `dparse_render.rs`): the model inputs
    // come from docling-parse's Blend2D canvas, requested in docling's order
    // and with its decode hint — the scale-1.0 layout image (docling decodes
    // the page once, at its `render_scale` of 1.0), then the scale-2.0 bitmap
    // TableFormer crops from (`_render_image_at_scale` on the same decoder,
    // hence the same hint). One deliberate exception: a *scanned* page — no
    // text layer, the OCR path — keeps pdfium's bitmap. docling's 2.0/3.0
    // re-renders draw the raster its decoders reduced to 72 dpi, and Blend2D's
    // blit of a scan differs from pdfium's render + downscale in a way the
    // `ch` conformance recognizer feels (`scanned/ocr_test.pdf` read `JsON` for
    // `JSON` from either docling-parse raster, hint 1.0 or full resolution);
    // pdfium's raster is the one the scanned groundtruth was matched with, so
    // OCR keeps it (recorded in PDF_CONFORMANCE.md) — as long as pdfium is
    // loaded: a checkout with the plugin and no `libpdfium` OCRs the
    // docling-parse raster rather than failing the page (degradation over
    // failure). The canvases are `ceil`-sized where pdfium's are `round`ed;
    // every consumer maps points through `RENDER_SCALE`, not through the
    // image size, so the extra row/column is harmless.
    let scanned = cells.is_empty() && word_cells.is_empty() && code_cells.is_empty();
    let mut pending_image = None;
    let (mut dp_image, mut dp_layout) = (None, None);
    if let (true, Some(dp)) = (render_image, dparse) {
        let layout = crate::timing::timed("dparse.render_layout", || {
            dp.render(index as usize, 1.0, 1.0)
        })
        .map_err(PdfError::Document)?;
        if !scanned {
            let full = crate::timing::timed("dparse.render", || {
                dp.render(index as usize, f64::from(RENDER_SCALE), 1.0)
            })
            .map_err(PdfError::Document)?;
            dp_image = Some(full);
        }
        dp.release_page(index as usize);
        dp_layout = Some(layout);
    }
    // A scanned page's bitmap, in order: the pure-Rust raster (pdfium's bytes;
    // `raster`), pdfium itself, and — with neither — the plugin's scale-2.0
    // canvas rather than a failed page.
    let scanned_fallback = |page: Option<&native::Page<'_>>| match (page, dparse) {
        (None, Some(dp)) if scanned => {
            let full = crate::timing::timed("dparse.render", || {
                dp.render(index as usize, f64::from(RENDER_SCALE), 1.0)
            })
            .map_err(PdfError::Document)?;
            dp.release_page(index as usize);
            Ok(Some(full))
        }
        _ => Ok::<_, PdfError>(None),
    };
    let image = if let Some(img) = dp_image.take() {
        img
    } else if render_image {
        // docling's pypdfium2 backend renders at 1.5× the target scale and
        // downsamples "to make it sharper" (pypdfium2 → PIL BICUBIC). Replicate
        // exactly: the TableFormer model is pixel-sensitive, so the page bitmap
        // must match that backend's byte-for-byte (docling 2.123+'s default
        // docling-parse backend renders it itself — #478).
        // `CatmullRom` is the same a=-0.5 cubic kernel as PIL's BICUBIC.
        const SUPERSAMPLE: f32 = 1.5;
        // The 3x supersample is the largest bitmap the pipeline renders, so the
        // per-side cap is enforced here; the 1.5x layout render below is always
        // smaller and needs no separate guard.
        let (tw, th) = checked_render_dims(
            f64::from(width * RENDER_SCALE * SUPERSAMPLE),
            f64::from(height * RENDER_SCALE * SUPERSAMPLE),
            (index + 1) as usize,
        )?;
        // An image-only page (a scan) is rendered by the pure-Rust raster —
        // pdfium's bitmap byte for byte (`raster`, its oracle test) — and any
        // other page by the pure-Rust renderer (`render`), so the OCR path
        // needs no `libpdfium`; pdfium renders only when asked for
        // (`DOCLING_RS_RENDERER=pdfium`).
        let dw = (width * RENDER_SCALE).round().max(1.0) as u32;
        let dh = (height * RENDER_SCALE).round().max(1.0) as u32;
        // A page without a text layer takes this bitmap to OCR: decode its
        // images at full size (hint 0.0); a digital page's TableFormer input
        // follows docling's hint 1.0 like the layout image below.
        let hint = if cells.is_empty() { 0.0 } else { 1.0 };
        // A page with a text layer needs this bitmap only after layout
        // (TableFormer, crops): the helper thread draws it meanwhile (#682).
        // A scanned page reads it first (orientation, OCR) and renders here.
        if let (Some(defer), false) = (defer, scanned) {
            pending_image = Some(defer.submit(index, tw as u32, th as u32, hint, dw, dh));
            RgbImage::new(1, 1)
        } else {
            let big = match rust_bitmap(
                meta,
                renderer,
                page.is_some(),
                index,
                tw as u32,
                th as u32,
                hint,
            ) {
                Some(img) => Some(img),
                None if page.is_some() => {
                    let page = page.ok_or_else(|| no_raster(index))?;
                    Some(page.render(tw, th, "pdfium.render")?)
                }
                None => None,
            };
            match (big, scanned_fallback(page)?) {
                (Some(big), _) => {
                    crate::timing::timed("image.resize", || fast_downscale(&big, dw, dh))
                }
                (None, Some(canvas)) => canvas,
                (None, None) => return Err(no_raster(index)),
            }
        }
    } else {
        RgbImage::new(1, 1)
    };
    // The layout model's input image, built exactly like docling's pypdfium2
    // `get_page_image(scale=1.0)`: a pdfium render at 1.5× (pypdfium2 sizes
    // with `ceil`), PIL-BICUBIC down to the point-size image (PIL `resize`'s
    // default kernel; Python `round` = ties-to-even). Distinct from the 2×
    // bitmap above — resampling 1224→640 and 612→640 are different regimes,
    // and the heron model's borderline scores follow the pixels. "Exactly" is
    // against the pypdfium2 backend: docling 2.123+'s default docling-parse
    // backend renders this image with its own renderer (#478, see
    // docs/PDF_CONFORMANCE.md).
    let image_layout = if let Some(img) = dp_layout.take() {
        Some(img)
    } else if render_image {
        let tw = f64::from(width * 1.5).ceil().max(1.0) as i32;
        let th = f64::from(height * 1.5).ceil().max(1.0) as i32;
        let big = match rust_bitmap(
            meta,
            renderer,
            page.is_some(),
            index,
            tw as u32,
            th as u32,
            1.0,
        ) {
            Some(img) => img,
            None => {
                let page = page.ok_or_else(|| no_raster(index))?;
                page.render(tw, th, "pdfium.render_layout")?
            }
        };
        let dw = f64::from(width).round_ties_even().max(1.0) as u32;
        let dh = f64::from(height).round_ties_even().max(1.0) as u32;
        Some(crate::timing::timed("image.resize_layout", || {
            crate::resample::pil_resize(&big, dw, dh, crate::resample::PilFilter::Bicubic)
        }))
    } else {
        None
    };

    let mut links = links;
    if rotation != 0 {
        for l in &mut links {
            let (a, t, r, b) = to_display_frame((l.l, l.t, l.r, l.b), rotation, unrot_w, unrot_h);
            (l.l, l.t, l.r, l.b) = (a, t, r, b);
        }
    }

    // `/Rotate` normalization for scanned pages: pdfium renders the page as a
    // viewer displays it — `/Rotate` applied — so a rotated scan hands layout
    // and OCR a sideways/upside-down raster and the recognition output is
    // garbage. A page with a text layer needs none of this (its cells carry
    // the geometry; the models never see its pixels decide text), so the
    // normalization is gated to pages with no cells at all — exactly the set
    // the OCR path fires on. The bitmaps are un-rotated to upright (lossless
    // 90° steps), `width`/`height` swap to the upright box, and the display
    // rotation is recorded so assembly can rotate the finished geometry back
    // into display space (docling reports rotated pages in display coords).
    let mut page = PdfPage {
        width,
        height,
        scale: RENDER_SCALE,
        image_layout,
        pending_image,
        cells,
        code_cells,
        word_cells,
        checkboxes,
        bitmaps,
        image,
        links,
        rotation: 0,
        keep_typography: false,
        #[cfg(feature = "render")]
        embedded_images: Vec::new(),
    };
    if rotation != 0 && scanned && render_image {
        page.unrotate(rotation);
    }
    Ok(page)
}

#[cfg(feature = "render")]
/// The supersample→target downscale via `fast_image_resize` (SIMD convolution;
/// the same a=-0.5 Catmull-Rom kernel as `image::imageops::resize(...,
/// CatmullRom)` and PIL BICUBIC — see the render comment above). Set
/// `DOCLING_RS_SLOW_RESIZE=1` to fall back to the `image`-crate scalar resize
/// (byte-parity with the pre-SIMD pipeline, several times slower).
fn fast_downscale(big: &RgbImage, dw: u32, dh: u32) -> RgbImage {
    use fast_image_resize as fir;
    static SLOW: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let slow = *SLOW.get_or_init(|| docling_core::env::flag("DOCLING_RS_SLOW_RESIZE"));
    if !slow {
        if let Some(out) = (|| {
            let src = fir::images::ImageRef::new(
                big.width(),
                big.height(),
                big.as_raw(),
                fir::PixelType::U8x3,
            )
            .ok()?;
            let mut dst = fir::images::Image::new(dw, dh, fir::PixelType::U8x3);
            fir::Resizer::new()
                .resize(
                    &src,
                    &mut dst,
                    &fir::ResizeOptions::new()
                        .resize_alg(fir::ResizeAlg::Convolution(fir::FilterType::CatmullRom)),
                )
                .ok()?;
            RgbImage::from_raw(dw, dh, dst.into_vec())
        })() {
            return out;
        }
        // Unreachable in practice; fall through to the scalar path on any error.
    }
    image::imageops::resize(big, dw, dh, image::imageops::FilterType::CatmullRom)
}

/// Map a top-left-origin rect from a page's unrotated (MediaBox) frame into its
/// `/Rotate`d display frame — the counterpart of docling's pypdfium2
/// `_rect_to_display_frame` (docling#4008) for our y-down coordinates.
/// `unrot_w`/`unrot_h` are the unrotated page box; the display box is the same
/// for 180° and swapped for 90°/270°.
pub(crate) fn to_display_frame(
    (l, t, r, b): (f32, f32, f32, f32),
    rotation: u16,
    unrot_w: f32,
    unrot_h: f32,
) -> (f32, f32, f32, f32) {
    match rotation {
        // Page turned 90° clockwise for display: the unrotated top edge becomes
        // the display right edge, so x' runs from the old bottom edge up.
        90 => (unrot_h - b, l, unrot_h - t, r),
        180 => (unrot_w - r, unrot_h - b, unrot_w - l, unrot_h - t),
        270 => (t, unrot_w - r, b, unrot_w - l),
        _ => (l, t, r, b),
    }
}

/// One glyph: codepoint + native (y-up) box edges. `l/b/r/t` is pdfium's *tight*
/// ink box (used by the legacy `lines_from_glyphs`); `ll/lb/lr/lt` is the *loose*
/// box (font ascent/descent + advance — uniform per font/size), which the
/// docling-parse-style sanitizer needs so adjacent glyphs share a top edge.
pub(crate) struct Glyph {
    pub(crate) ch: char,
    pub(crate) l: f32,
    pub(crate) b: f32,
    pub(crate) r: f32,
    pub(crate) t: f32,
    pub(crate) ll: f32,
    pub(crate) lb: f32,
    pub(crate) lr: f32,
    pub(crate) lt: f32,
    /// Hash of the font's docling-parse name — `/FontDescriptor/FontName`,
    /// else `/Name`, else `/BaseFont` (#728: not the page's resource key, which
    /// binds one face under several keys) — 0 when not fetched. The sanitizer
    /// uses it for docling-parse's `enforce_same_font` (keeps a bold label and
    /// regular value as separate line cells, e.g. `LABEL : value`).
    pub(crate) font: u64,
    /// The loose box as the text matrix actually lays it — corners
    /// bottom-left, bottom-right, top-right, top-left as `[x0, y0, … x3, y3]`
    /// (docling-parse's `r_x0…r_y3`) — for a glyph whose baseline is **not**
    /// upright: rotated 90°/180°/270° or tilted (#528). `ll/lb/lr/lt` are then
    /// this quad's axis-aligned extent. `None` for upright text, whose quad is
    /// the `ll/lb/lr/lt` rectangle itself — that path stays exactly as it was.
    pub(crate) quad: Option<[f32; 8]>,
    /// Where the pen sets the glyph — the text origin on the baseline, rise
    /// included — and the font size in the same space, as `[x, y, size]`
    /// (docling-parse's `text_origin_x/y` and `nominal_text_height`). The
    /// word contraction tests a baseline shift on these (#921): the loose
    /// box's bottom sits a descent below the baseline, which differs between
    /// sizes, so a superscript read as shifted further than it is. `None`
    /// when unknown (a synthetic glyph); the box stands in then.
    pub(crate) pen: Option<[f32; 3]>,
    /// The glyph's font is a math face by its name (docling-parse's
    /// `has_math_positioning_semantics`): a sub- or superscript in it next to
    /// its base is attached, not a new word (#921).
    pub(crate) math_font: bool,
}

impl Glyph {
    /// The font's height on the glyph's own axis (ascent − descent): the
    /// loose box's height for upright text, the quad's left edge otherwise —
    /// a 90°-turned glyph's vertical extent is its advance, not its size.
    pub(crate) fn height(&self) -> f32 {
        match self.quad {
            Some(q) => (q[6] - q[0]).hypot(q[7] - q[1]),
            None => self.lt - self.lb,
        }
    }
}

/// How [`lines_from_glyphs`] splits a line into words. Two more modes lived
/// here — the prose gap heuristic with punctuation glue and pdfium's
/// space-glyph-only code split — for pdfium's glyph stream; the parser's
/// prose goes through `dp_lines` and pdfium's text page is gone.
#[derive(Clone, Copy, PartialEq)]
enum Grouping {
    /// Split on the inter-glyph **gap** (or a space glyph), but never glue — for
    /// the parser's code cells: the parser emits no space glyphs (a source space
    /// is a positioning gap), and its clean advance boxes make the gap reliable.
    /// There is no punctuation glue, so a real gap always splits (`et al.
    /// 2000`, not `et al.2000`) while genuinely touching tokens stay joined
    /// (`add(a,` / `b)`).
    CodeGap,
}

/// Group glyphs (document order) into words then lines, the way docling-parse
/// does: a new **word** starts where the horizontal gap to the previous glyph
/// exceeds ~0.2 × the font height (a real space is ~0.3 × height; letter
/// tracking is smaller, so titles don't shatter); a new **line** starts where
/// the baseline drops by ~half the font height (a superscript rises without
/// dropping, so it stays on its line). Coordinates are flipped to top-left.
/// See [`Grouping`] for how each mode decides word boundaries.
fn lines_from_glyphs(gs: &[Glyph], page_h: f32, mode: Grouping) -> Vec<TextCell> {
    let mut cells: Vec<TextCell> = Vec::new();
    let mut words: Vec<String> = Vec::new(); // words on the current line
    let mut word = String::new();
    // current line bounding box, native
    let (mut ll, mut lb, mut lr, mut lt) = (
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    );
    // Tallest glyph seen on the current line: the word-gap threshold is relative
    // to it, so a small-font run on the line (a superscript citation) isn't split
    // at its tight digit gaps, while a big display title isn't split at its wider
    // letter tracking. A real inter-word space is ~0.3× the font height.
    let mut line_h: f32 = 0.0;
    let mut prev: Option<&Glyph> = None;
    // A space glyph between non-space glyphs pins a word split the gap heuristic
    // can miss (tight justified spacing); it carries no geometry.
    let mut pending_space = false;

    for g in gs {
        if g.ch == ' ' {
            pending_space = true;
            continue;
        }
        let h = (g.t - g.b).abs().max(1.0);
        let (mut new_word, mut new_line) = (false, false);
        if let Some(p) = prev {
            // A new line drops the baseline *and* resets x leftward; requiring the
            // x-reset avoids a descending comma/semicolon faking a line break. A
            // *large* drop (≥1.5× the line height — a skipped line, e.g. a centered
            // page-number footer below a short last word) is always a new line,
            // even without the x-reset.
            // LTR wraps reset x leftward (`g.l < p.r`); RTL (Arabic) wraps reset
            // rightward (the new line begins at the far right). A large drop
            // (≥1.5× line height) is a new line regardless of x.
            let x_reset = if is_arabic(g.ch) || is_arabic(p.ch) {
                g.l > p.r
            } else {
                g.l < p.r
            };
            new_line = (p.b - g.b > h * 0.5 && x_reset) || (p.b - g.b > line_h.max(h) * 1.5);
            let word_gap = line_h.max(h) * 0.25;
            new_word = match mode {
                // Gap-based, no glue: a real gap always splits, touching tokens join.
                Grouping::CodeGap => new_line || pending_space || g.l - p.r > word_gap,
            };
        }
        pending_space = false;
        if new_line {
            push_word(&mut word, &mut words);
            push_line(&mut words, (ll, lb, lr, lt), page_h, &mut cells);
            (ll, lb, lr, lt) = (
                f32::INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::NEG_INFINITY,
            );
            line_h = 0.0;
        } else if new_word {
            push_word(&mut word, &mut words);
        }
        word.push(g.ch);
        ll = ll.min(g.l);
        lb = lb.min(g.b);
        lr = lr.max(g.r);
        lt = lt.max(g.t);
        line_h = line_h.max(h);
        prev = Some(g);
    }
    push_word(&mut word, &mut words);
    push_line(&mut words, (ll, lb, lr, lt), page_h, &mut cells);
    cells
}

/// Code line cells from the parser's glyph stream. The parser emits no space
/// glyphs — a source space is a positioning gap — so code cells use
/// [`Grouping::CodeGap`], which splits on the inter-glyph gap (a space
/// wherever it exceeds ~0.25× the line height) but never glues punctuation,
/// so `et al. 2000` keeps its space while `add(a,` / `b)` stay joined. The
/// parser's clean advance boxes make the gap heuristic reliable here, where
/// pdfium's overhanging loose boxes used to over-split (`f un c t i o n`).
pub(crate) fn code_cells_from_glyphs(gs: &[Glyph], page_h: f32) -> Vec<TextCell> {
    lines_from_glyphs(gs, page_h, Grouping::CodeGap)
}

fn is_arabic(c: char) -> bool {
    ('\u{0600}'..='\u{06FF}').contains(&c)
}

fn push_word(word: &mut String, words: &mut Vec<String>) {
    if !word.is_empty() {
        words.push(std::mem::take(word));
    }
}

fn push_line(
    words: &mut Vec<String>,
    bbox: (f32, f32, f32, f32),
    page_h: f32,
    cells: &mut Vec<TextCell>,
) {
    if words.is_empty() {
        return;
    }
    let text = std::mem::take(words).join(" ");
    let (l, b, r, t) = bbox;
    cells.push(TextCell {
        text,
        l,
        t: page_h - t,
        r,
        b: page_h - b,
    });
}

/// The pdfium library, behind the `pdfium` feature (phase 5 of "Retiring
/// pdfium"): the render `DOCLING_RS_RENDERER=pdfium` asks for — docling's
/// pypdfium2 chain — the last resort for a file lopdf cannot read, and the
/// oracle of the raster and object-model tests. The default build has the
/// stub below and never links or loads pdfium.
#[cfg(feature = "pdfium")]
mod native {
    use super::LinkAnnot;
    use crate::PdfError;
    use image::RgbImage;
    use pdfium_render::prelude::*;

    /// The bound library.
    pub(super) struct Lib(Pdfium);
    /// An open document.
    pub(super) struct Session<'a> {
        doc: PdfDocument<'a>,
    }
    /// A loaded page.
    pub(super) struct Page<'a>(PdfPage<'a>);

    /// Try binding pdfium from a directory (or a literal library file path):
    /// `<dir>/<platform library name>` first, else `<dir>` itself as the file.
    fn try_bind_dir(path: &str) -> Option<Box<dyn PdfiumLibraryBindings>> {
        let name = Pdfium::pdfium_platform_library_name_at_path(path);
        if let Ok(b) = Pdfium::bind_to_library(&name) {
            return Some(b);
        }
        Pdfium::bind_to_library(path).ok()
    }

    /// Bind to the pdfium dynamic library. Honors `PDFIUM_DYNAMIC_LIB_PATH` (a
    /// directory or file) first; else falls back to `.pdfium/lib` relative to
    /// the current directory (the layout `scripts/install/pdf_setup.sh`
    /// produces); else the system library.
    pub(super) fn bind() -> Result<Lib, PdfError> {
        if let Some(path) = docling_core::env::nonempty("PDFIUM_DYNAMIC_LIB_PATH") {
            if let Some(b) = try_bind_dir(&path) {
                return Ok(Lib(Pdfium::new(b)));
            }
        }
        if let Some(b) = try_bind_dir(&crate::resolve_asset(".pdfium/lib")) {
            return Ok(Lib(Pdfium::new(b)));
        }
        Pdfium::bind_to_system_library()
            .map(|b| Lib(Pdfium::new(b)))
            .map_err(Into::into)
    }

    /// `bind()` for unit tests: point `PDFIUM_DYNAMIC_LIB_PATH` at the
    /// repo-root `.pdfium/lib` first (tests run with CWD = the crate dir,
    /// where the CWD-relative default cannot see it).
    #[cfg(test)]
    pub(crate) fn bind_for_tests() -> Result<Pdfium, PdfError> {
        if std::env::var_os("PDFIUM_DYNAMIC_LIB_PATH").is_none() {
            let lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.pdfium/lib");
            if lib.is_dir() {
                std::env::set_var("PDFIUM_DYNAMIC_LIB_PATH", &lib);
            }
        }
        bind().map(|l| l.0)
    }

    impl Lib {
        pub(super) fn open<'a>(
            &'a self,
            bytes: &'a [u8],
            password: Option<&str>,
        ) -> Result<Session<'a>, PdfError> {
            crate::timing::timed("pdfium.open", || {
                self.0
                    .load_pdf_from_byte_slice(bytes, password)
                    .map(|doc| Session { doc })
                    .map_err(Into::into)
            })
        }
    }

    impl Session<'_> {
        pub(super) fn page_count(&self) -> usize {
            self.doc.pages().len() as usize
        }

        pub(super) fn page(&self, index: usize) -> Result<Page<'_>, PdfError> {
            self.doc
                .pages()
                .get(index as PdfPageIndex)
                .map(Page)
                .map_err(Into::into)
        }
    }

    impl Page<'_> {
        /// The display size in points.
        pub(super) fn size(&self) -> (f32, f32) {
            (self.0.width().value, self.0.height().value)
        }

        /// pdfium's view of the page's geometry, for the files lopdf cannot
        /// read — the same numbers `pdf_meta` computes from the object model.
        pub(super) fn geom(&self) -> crate::pdf_meta::PageGeom {
            crate::pdf_meta::PageGeom {
                width: self.0.width().value,
                height: self.0.height().value,
                rotation: match self.0.rotation() {
                    Ok(PdfPageRenderRotation::Degrees90) => 90u16,
                    Ok(PdfPageRenderRotation::Degrees180) => 180,
                    Ok(PdfPageRenderRotation::Degrees270) => 270,
                    _ => 0,
                },
            }
        }

        /// Render the page into a `w` × `h` RGB bitmap under timing `stage`.
        pub(super) fn render(
            &self,
            w: i32,
            h: i32,
            stage: &'static str,
        ) -> Result<RgbImage, PdfError> {
            let cfg = PdfRenderConfig::new()
                .set_target_width(w)
                .set_target_height(h);
            crate::timing::timed(stage, || {
                self.0
                    .render_with_config(&cfg)
                    .map(|b| b.as_image().into_rgb8())
                    .map_err(Into::into)
            })
        }

        /// Collect web/mail/tel hyperlink annotations on the page, mapping
        /// each link's rectangle into top-left page coordinates (like
        /// [`super::TextCell`]). `file://` and in-document destinations are
        /// skipped — only externally meaningful targets are rendered. pdfium
        /// occasionally lists a link twice; rects are kept as-is and the
        /// caller dedupes by resolved anchor text.
        pub(super) fn links(&self, page_h: f32) -> Vec<LinkAnnot> {
            let mut out = Vec::new();
            for link in self.0.links().iter() {
                let Some(uri) = link
                    .action()
                    .and_then(|a| a.as_uri_action().and_then(|u| u.uri().ok()))
                else {
                    continue;
                };
                let scheme_ok = ["http://", "https://", "mailto:", "tel:"]
                    .iter()
                    .any(|s| uri.starts_with(s));
                if !scheme_ok {
                    continue;
                }
                if let Ok(rect) = link.rect() {
                    out.push(LinkAnnot {
                        l: rect.left().value,
                        t: page_h - rect.top().value,
                        r: rect.right().value,
                        b: page_h - rect.bottom().value,
                        uri,
                    });
                }
            }
            out
        }
    }
}

/// The `pdfium`-less build: pdfium never binds, so every entry point falls
/// through to the pure-Rust stack, and a file the object model cannot read
/// fails with [`no_raster`]'s hint.
#[cfg(all(feature = "render", not(feature = "pdfium")))]
mod native {
    use super::LinkAnnot;
    use crate::PdfError;
    use image::RgbImage;
    use std::convert::Infallible;
    use std::marker::PhantomData;

    pub(super) struct Lib(Infallible);
    pub(super) struct Session<'a>(Infallible, PhantomData<&'a ()>);
    pub(super) struct Page<'a>(Infallible, PhantomData<&'a ()>);

    /// Reached only for a file the pure-Rust object model could not read
    /// (every entry point tries it first), so the error is about the file:
    /// naming the missing library sent users of a corrupt or non-PDF upload
    /// looking for a pdfium install that would not have helped either.
    pub(super) fn bind() -> Result<Lib, PdfError> {
        Err(PdfError::Document(crate::UNREADABLE.into()))
    }

    impl Lib {
        pub(super) fn open<'a>(
            &'a self,
            _bytes: &'a [u8],
            _password: Option<&str>,
        ) -> Result<Session<'a>, PdfError> {
            match self.0 {}
        }
    }

    impl Session<'_> {
        pub(super) fn page_count(&self) -> usize {
            match self.0 {}
        }

        pub(super) fn page(&self, _index: usize) -> Result<Page<'_>, PdfError> {
            match self.0 {}
        }
    }

    impl Page<'_> {
        pub(super) fn size(&self) -> (f32, f32) {
            match self.0 {}
        }

        pub(super) fn geom(&self) -> crate::pdf_meta::PageGeom {
            match self.0 {}
        }

        pub(super) fn render(
            &self,
            _w: i32,
            _h: i32,
            _stage: &'static str,
        ) -> Result<RgbImage, PdfError> {
            match self.0 {}
        }

        pub(super) fn links(&self, _page_h: f32) -> Vec<LinkAnnot> {
            match self.0 {}
        }
    }
}

#[cfg(all(test, feature = "pdfium"))]
pub(crate) use native::bind_for_tests;

#[cfg(test)]
mod tests {
    use super::{checked_render_dims, max_render_side, to_display_frame};

    /// #897: the adaptive schedule starts renders eagerly for the first
    /// pages, then while at least half of the last 8 verdicts read the
    /// bitmap; a declined page that turns out to read it counts as read.
    #[cfg(feature = "render")]
    #[test]
    fn adaptive_verdicts() {
        let v = super::Verdicts::default();
        assert!(v.start_eagerly(), "no verdict yet");
        v.record(false, false);
        v.record(false, false);
        assert!(v.start_eagerly(), "still warming up");
        v.record(false, false);
        assert!(!v.start_eagerly(), "three text-only pages");
        v.record(true, true); // the last declined one was read after all
        v.record(true, false);
        assert!(v.start_eagerly(), "2 of 4 read");
        for _ in 0..8 {
            v.record(false, false);
        }
        assert!(!v.start_eagerly(), "the window forgets the read ones");
        for _ in 0..4 {
            v.record(true, false);
        }
        assert!(v.start_eagerly(), "4 of the last 8 read");
    }

    /// The pure-Rust object model must answer exactly what pdfium answers —
    /// page count, display size, `/Rotate`, URI links — on every corpus PDF,
    /// or a checkout without pdfium would convert differently. Needs the
    /// library; skips cleanly without it (CI has no pdfium).
    #[test]
    #[cfg(feature = "pdfium")]
    fn pdf_meta_matches_pdfium_on_the_corpus() {
        let Ok(_) = super::bind_for_tests() else {
            eprintln!("skipping: pdfium not found");
            return;
        };
        let lib = super::native::bind().unwrap();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        for dir in ["tests/data/pdf/sources", "tests/data/scanned/sources"] {
            let Ok(rd) = std::fs::read_dir(root.join(dir)) else {
                continue;
            };
            files.extend(
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|e| e == "pdf")),
            );
        }
        files.sort();
        assert!(!files.is_empty(), "corpus not found");
        let mut checked = 0;
        for path in files {
            let bytes = std::fs::read(&path).unwrap();
            let Ok(doc) = lib.open(&bytes, None) else {
                continue; // password fixtures etc.
            };
            let Some(meta) = crate::pdf_meta::PdfMeta::open(&bytes) else {
                panic!(
                    "{}: lopdf could not read a file pdfium reads",
                    path.display()
                );
            };
            assert_eq!(meta.page_count(), doc.page_count(), "{}", path.display());
            for i in 0..meta.page_count() {
                let page = doc.page(i).unwrap();
                let want = page.geom();
                let got = meta.geometry(i).unwrap();
                assert!(
                    (got.width - want.width).abs() < 0.01
                        && (got.height - want.height).abs() < 0.01
                        && got.rotation == want.rotation,
                    "{} p{}: meta {got:?} vs pdfium {want:?}",
                    path.display(),
                    i + 1
                );
                let mut want_links = page.links(want.unrotated().1);
                let mut got_links = meta.links(i);
                let key = |l: &super::LinkAnnot| {
                    (
                        l.uri.clone(),
                        (l.l * 10.0) as i64,
                        (l.t * 10.0) as i64,
                        (l.r * 10.0) as i64,
                        (l.b * 10.0) as i64,
                    )
                };
                want_links.sort_by_key(key);
                got_links.sort_by_key(key);
                let want_keys: Vec<_> = want_links.iter().map(key).collect();
                let got_keys: Vec<_> = got_links.iter().map(key).collect();
                assert_eq!(got_keys, want_keys, "{} p{}: links", path.display(), i + 1);
                checked += 1;
            }
        }
        eprintln!("pdf_meta checked against pdfium on {checked} pages");
    }

    /// A page whose declared size renders past the per-side cap is rejected
    /// with a recoverable error, before pdfium or `image` allocates the
    /// multi-gigabyte bitmap that would otherwise abort the process; a normal
    /// page passes through with its dimensions rounded to `i32`.
    #[test]
    fn oversized_render_is_rejected_not_allocated() {
        let cap = f64::from(max_render_side());
        // A 12000 pt box at the pipeline's 3x supersample is 36000 px/side.
        let huge = checked_render_dims(cap + 1.0, 10.0, 1);
        assert!(huge.is_err(), "over-cap width must be rejected");
        let tall = checked_render_dims(10.0, cap + 1.0, 7);
        assert!(tall.is_err(), "over-cap height must be rejected");
        assert!(
            tall.unwrap_err().to_string().contains("page 7"),
            "the error names the offending page"
        );
        // A Letter page at 2x supersample (612x792 pt -> 1836x2376 px) is fine.
        assert_eq!(
            checked_render_dims(1836.4, 2375.6, 1).unwrap(),
            (1836, 2376)
        );
        // Exactly at the cap is allowed; a zero-or-negative size floors to 1.
        assert_eq!(
            checked_render_dims(cap, cap, 1).unwrap(),
            (cap as i32, cap as i32)
        );
        assert_eq!(checked_render_dims(0.0, 0.0, 1).unwrap(), (1, 1));
    }

    /// A 612×792 portrait page displayed under `/Rotate`: a rect near the
    /// unrotated top-left lands where a viewer shows it (docling#4008).
    #[test]
    fn display_frame_follows_the_page_rotation() {
        let r = (72.0, 63.0, 387.0, 74.0); // top-left origin, unrotated
        assert_eq!(to_display_frame(r, 0, 612.0, 792.0), r);
        // 90° clockwise: the page becomes 792×612; the old top edge is the
        // display right edge, old left edge the display top.
        assert_eq!(
            to_display_frame(r, 90, 612.0, 792.0),
            (718.0, 72.0, 729.0, 387.0)
        );
        // 180°: both axes mirror inside the same box.
        assert_eq!(
            to_display_frame(r, 180, 612.0, 792.0),
            (225.0, 718.0, 540.0, 729.0)
        );
        // 270°: the old top edge is the display left edge, old right edge the
        // display top.
        assert_eq!(
            to_display_frame(r, 270, 612.0, 792.0),
            (63.0, 225.0, 74.0, 540.0)
        );
    }

    #[test]
    fn display_frame_rotations_compose_to_identity() {
        let r = (10.0, 20.0, 110.0, 40.0);
        // 90° then 270° from the intermediate (792×612) box round-trips.
        let once = to_display_frame(r, 90, 612.0, 792.0);
        assert_eq!(to_display_frame(once, 270, 792.0, 612.0), r);
        let twice = to_display_frame(to_display_frame(r, 180, 612.0, 792.0), 180, 612.0, 792.0);
        assert_eq!(twice, r);
    }
}
