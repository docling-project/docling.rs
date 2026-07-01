//! pdfium-based text extraction and page rendering.
//!
//! Text is reconstructed the way docling's `docling-parse` does it, so the
//! output spacing matches the groundtruth: the page's **character** stream is
//! grouped into **words** (split at a horizontal gap wider than a fraction of
//! the font height — font-relative, so letter-tracking in display titles does
//! not split a word) and words into **lines** (by baseline). pdfium-render's
//! safe API only exposes whole style runs / `GetBoundedText`, so the character
//! loop is driven through the raw `PdfiumLibraryBindings` FFI on a second handle
//! to the same bytes (no fork; stays publishable).

use image::RgbImage;
use pdfium_render::prelude::*;

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
    pub image: RgbImage,
    /// Hyperlink annotations on the page (rect in top-left page coords + target
    /// URI), restricted to web/mail/tel schemes. Used only by strict Markdown.
    pub links: Vec<LinkAnnot>,
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

/// A parsed PDF: per-page text cells and page images.
pub struct PdfDocument {
    pub pages: Vec<PdfPage>,
}

/// Whether to use the docling-parse line sanitizer ([`crate::dp_lines`]) for prose
/// reconstruction — the default. Set `DOCLING_LEGACY_LINES` to fall back to the
/// older gap-heuristic `lines_from_glyphs`.
pub(crate) fn use_dp_lines() -> bool {
    std::env::var("DOCLING_LEGACY_LINES").is_err()
}

/// Whether to source **word** cells from the pure-Rust parser (roadmap item 6),
/// the default. The parser's `word_cells` reproduce docling-parse's word grouping
/// byte-for-byte — the per-word tokens TableFormer matches table-grid cells
/// against — which moves table extraction closer to docling on the heavy
/// multi-column fixtures. Set `DOCLING_PDFIUM_WORDS` to keep pdfium's word cells,
/// or `DOCLING_PDFIUM_TEXT` to fall back to pdfium for all text.
pub(crate) fn use_parser_words() -> bool {
    std::env::var("DOCLING_PDFIUM_WORDS").is_err() && std::env::var("DOCLING_PDFIUM_TEXT").is_err()
}

/// Whether to source **code** cells from the parser too (the default) — the last
/// text layer to leave pdfium, fully retiring its text path. The parser's
/// gap-based code grouping ([`code_cells_from_glyphs`]) reconstructs monospace
/// spacing from positioning gaps (`function add(a, b) { … }`), so it no longer
/// drops the inter-token spaces the old space-glyph-only grouping lost
/// (`functionadd`). Reverts to pdfium with `DOCLING_PDFIUM_WORDS` (alongside word
/// cells) or `DOCLING_PDFIUM_TEXT` (all text).
pub(crate) fn use_parser_code() -> bool {
    std::env::var("DOCLING_PDFIUM_WORDS").is_err() && std::env::var("DOCLING_PDFIUM_TEXT").is_err()
}

/// Try binding pdfium from a directory (or a literal library file path):
/// `<dir>/<platform library name>` first, else `<dir>` itself as the file.
fn try_bind_dir(path: &str) -> Option<Box<dyn pdfium_render::prelude::PdfiumLibraryBindings>> {
    let name = Pdfium::pdfium_platform_library_name_at_path(path);
    if let Ok(b) = Pdfium::bind_to_library(&name) {
        return Some(b);
    }
    Pdfium::bind_to_library(path).ok()
}

/// Bind to the pdfium dynamic library. Honors `PDFIUM_DYNAMIC_LIB_PATH` (a
/// directory or file) first; else falls back to `.pdfium/lib` relative to the
/// current directory (the layout `scripts/download_dependencies.sh` and
/// `scripts/pdf_setup.sh` both produce); else the system library.
fn bind() -> Result<Pdfium, PdfiumError> {
    if let Ok(path) = std::env::var("PDFIUM_DYNAMIC_LIB_PATH") {
        if let Some(b) = try_bind_dir(&path) {
            return Ok(Pdfium::new(b));
        }
    }
    // No env var (or it didn't resolve): fall back to `.pdfium/lib` relative to
    // the current directory — mirroring `layout.rs`/`ocr.rs`'s `models/…`
    // defaults — the layout `scripts/download_dependencies.sh` (and
    // `scripts/pdf_setup.sh`) produce, so a checkout with the dependencies
    // downloaded next to it needs no env var at all.
    if let Some(b) = try_bind_dir(".pdfium/lib") {
        return Ok(Pdfium::new(b));
    }
    Pdfium::bind_to_system_library().map(Pdfium::new)
}

impl PdfDocument {
    /// Parse a PDF from bytes, optionally decrypting with `password`.
    ///
    /// Note: this materialises **every** page's rendered bitmap in memory at
    /// once. For large documents prefer [`for_each_page`], which streams.
    pub fn open(bytes: &[u8], password: Option<&str>) -> Result<Self, PdfiumError> {
        let pdfium = bind()?;
        let ffi = FfiText::load(pdfium.bindings(), bytes, password);
        let doc = pdfium.load_pdf_from_byte_slice(bytes, password)?;
        let mut rust = rust_parser_cells(bytes);
        let mut pages = Vec::new();
        for (i, page) in doc.pages().iter().enumerate() {
            let rc = rust.as_mut().and_then(|v| v.get_mut(i).map(std::mem::take));
            pages.push(extract_page(&page, &ffi, i as i32, rc)?);
        }
        Ok(PdfDocument { pages })
    }
}

/// Per-page prose line cells from the pure-Rust text parser. This is the
/// **default** text layer (it matches docling-parse's char geometry and is a
/// strict improvement on byte-conformance — e.g. it recovers the Arabic
/// sentence-period attachment in `right_to_left_01`). Set `DOCLING_PDFIUM_TEXT`
/// to fall back to pdfium's text layer. The parser returns an empty page when a
/// PDF (or a page) has no parseable text layer; the caller keeps pdfium's cells
/// in that case, so scanned/edge-case pages are unaffected.
fn rust_parser_cells(bytes: &[u8]) -> Option<Vec<crate::textparse::PageParserCells>> {
    if std::env::var("DOCLING_PDFIUM_TEXT").is_ok() {
        return None;
    }
    Some(crate::timing::timed("textparse", || {
        crate::textparse::pdf_all_cells(bytes)
    }))
}

/// Number of pages in a PDF, without rendering any of them — used to decide
/// whether a document is worth spinning up the parallel worker pool.
pub fn page_count(bytes: &[u8], password: Option<&str>) -> Result<usize, PdfiumError> {
    let pdfium = bind()?;
    let doc = pdfium.load_pdf_from_byte_slice(bytes, password)?;
    Ok(doc.pages().len() as usize)
}

/// Render + extract pages one at a time, handing each (owned) [`PdfPage`] to `f`.
/// Only one page bitmap is resident at a time — a rendered page is ~5 MB, so a
/// large PDF would otherwise hold gigabytes of bitmaps at once. `f` receives the
/// zero-based page index and the total page count.
///
/// `E` is the caller's error type; pdfium errors convert into it via `From`.
pub fn for_each_page<E, F>(bytes: &[u8], password: Option<&str>, mut f: F) -> Result<(), E>
where
    E: From<PdfiumError>,
    F: FnMut(usize, usize, PdfPage) -> Result<(), E>,
{
    let pdfium = bind()?;
    let ffi = FfiText::load(pdfium.bindings(), bytes, password);
    let doc = pdfium.load_pdf_from_byte_slice(bytes, password)?;
    let mut rust = rust_parser_cells(bytes);
    let pages = doc.pages();
    let total = pages.len() as usize;
    for (i, page) in pages.iter().enumerate() {
        let rc = rust.as_mut().and_then(|v| v.get_mut(i).map(std::mem::take));
        let extracted = extract_page(&page, &ffi, i as i32, rc)?;
        f(i, total, extracted)?;
    }
    Ok(())
}

fn extract_page(
    page: &pdfium_render::prelude::PdfPage<'_>,
    ffi: &FfiText<'_>,
    index: i32,
    rust_cells: Option<crate::textparse::PageParserCells>,
) -> Result<PdfPage, PdfiumError> {
    let width = page.width().value;
    let height = page.height().value;

    let (mut cells, mut code_cells, mut word_cells) =
        crate::timing::timed("ffi.page_cells", || ffi.page_cells(index, height));
    if cells.is_empty() {
        cells = segment_cells(&page.text()?, height);
    }
    // Default: use the pure-Rust text parser instead of pdfium's text layer
    // (override with `DOCLING_PDFIUM_TEXT`). Prose line cells always come from the
    // parser; word and code cells do too unless `DOCLING_PDFIUM_WORDS` keeps them
    // on pdfium (the parser's word grouping reproduces docling-parse's, which
    // TableFormer matches against — roadmap item 6). A page the parser couldn't
    // read (no text layer) keeps pdfium's cells.
    if let Some(rc) = rust_cells {
        if !rc.prose.is_empty() {
            cells = rc.prose;
        }
        if use_parser_words() && !rc.words.is_empty() {
            word_cells = rc.words;
        }
        if use_parser_code() && !rc.code.is_empty() {
            code_cells = rc.code;
        }
    }

    // docling renders at 1.5× the target scale and downsamples "to make it
    // sharper" (pypdfium2 → PIL BICUBIC). Replicate exactly: the TableFormer
    // model is pixel-sensitive, so the page bitmap must match byte-for-byte.
    // `CatmullRom` is the same a=-0.5 cubic kernel as PIL's BICUBIC.
    const SUPERSAMPLE: f32 = 1.5;
    let tw = (width * RENDER_SCALE * SUPERSAMPLE).round().max(1.0) as i32;
    let th = (height * RENDER_SCALE * SUPERSAMPLE).round().max(1.0) as i32;
    let cfg = PdfRenderConfig::new()
        .set_target_width(tw)
        .set_target_height(th);
    let big = crate::timing::timed("pdfium.render", || {
        page.render_with_config(&cfg)
            .map(|b| b.as_image().into_rgb8())
    })?;
    let dw = (width * RENDER_SCALE).round().max(1.0) as u32;
    let dh = (height * RENDER_SCALE).round().max(1.0) as u32;
    let image = crate::timing::timed("image.resize", || {
        image::imageops::resize(&big, dw, dh, image::imageops::FilterType::CatmullRom)
    });

    Ok(PdfPage {
        width,
        height,
        scale: RENDER_SCALE,
        cells,
        code_cells,
        word_cells,
        image,
        links: extract_links(page, height),
    })
}

/// Collect web/mail/tel hyperlink annotations on a page, mapping each link's
/// rectangle into top-left page coordinates (like [`TextCell`]). `file://` and
/// in-document destinations are skipped — only externally meaningful targets are
/// rendered. pdfium occasionally lists a link twice; rects are kept as-is and the
/// caller dedupes by resolved anchor text.
fn extract_links(page: &pdfium_render::prelude::PdfPage<'_>, page_h: f32) -> Vec<LinkAnnot> {
    let mut out = Vec::new();
    for link in page.links().iter() {
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

/// Fallback line cells from pdfium-render's style segments (one cell per
/// segment). Used only when the raw-FFI text page can't be loaded.
fn segment_cells(text: &PdfPageText, page_h: f32) -> Vec<TextCell> {
    text.segments()
        .iter()
        .filter_map(|seg| {
            let s = seg.text();
            if s.trim().is_empty() {
                return None;
            }
            let r = seg.bounds();
            Some(TextCell {
                text: s,
                l: r.left().value,
                t: page_h - r.top().value,
                r: r.right().value,
                b: page_h - r.bottom().value,
            })
        })
        .collect()
}

/// A second, raw-FFI handle on the same PDF used to drive the character loop
/// (`FPDFText_GetUnicode`/`GetCharBox`) that pdfium-render's safe API doesn't
/// expose. Closes the document on drop.
struct FfiText<'a> {
    bindings: &'a dyn PdfiumLibraryBindings,
    doc: FPDF_DOCUMENT,
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
    /// Hash of the PDF font name + flags (0 when not fetched). The sanitizer uses
    /// it for docling-parse's `enforce_same_font` (keeps a bold label and regular
    /// value as separate line cells, e.g. `LABEL : value`).
    pub(crate) font: u64,
}

impl<'a> FfiText<'a> {
    fn load(bindings: &'a dyn PdfiumLibraryBindings, bytes: &[u8], password: Option<&str>) -> Self {
        let doc = bindings.FPDF_LoadMemDocument(bytes, password);
        FfiText { bindings, doc }
    }

    /// Reconstruct line cells for page `index` (zero-based) via the
    /// chars→words→lines grouping. Returns `(prose_cells, code_cells)` — the same
    /// glyphs grouped two ways (gap-heuristic for prose, space-glyph-only for
    /// code). Both empty on any failure (caller falls back).
    fn page_cells(&self, index: i32, page_h: f32) -> (Vec<TextCell>, Vec<TextCell>, Vec<TextCell>) {
        let empty = || (Vec::new(), Vec::new(), Vec::new());
        if self.doc.is_null() {
            return empty();
        }
        let b = self.bindings;
        let page = b.FPDF_LoadPage(self.doc, index);
        if page.is_null() {
            return empty();
        }
        let tp = b.FPDFText_LoadPage(page);
        let out = if tp.is_null() {
            empty()
        } else {
            let dp = use_dp_lines();
            let g = glyphs(b, tp, dp);
            b.FPDFText_ClosePage(tp);
            // Prose line cells: the docling-parse-style sanitizer (behind a flag
            // while it's validated) or the legacy gap-heuristic reconstruction.
            let prose = if dp {
                crate::dp_lines::line_cells(&g, page_h, false)
            } else {
                lines_from_glyphs(&g, page_h, Grouping::Prose)
            };
            (
                prose,
                lines_from_glyphs(&g, page_h, Grouping::CodeSpaceOnly),
                words_from_glyphs(&g, page_h),
            )
        };
        b.FPDF_ClosePage(page);
        out
    }
}

impl Drop for FfiText<'_> {
    fn drop(&mut self) {
        if !self.doc.is_null() {
            self.bindings.FPDF_CloseDocument(self.doc);
        }
    }
}

/// Read every glyph (codepoint + native box) from the text page, in document
/// order. A space glyph is kept as a word-boundary marker (NaN box, char `' '`);
/// pdfium emits these on most lines and they pin word splits exactly. Hard line
/// breaks are dropped (line structure comes from geometry); the gap heuristic in
/// [`lines_from_glyphs`] is the fallback for the lines pdfium leaves space-less.
/// Debug helper: the raw pdfium glyph stream (codepoint + native bottom-left
/// box) for a page, in pdfium's character order. For comparing against
/// docling-parse's char cells.
pub fn debug_glyphs(bytes: &[u8], index: i32) -> Vec<(char, f32, f32)> {
    let Ok(pdfium) = bind() else {
        return Vec::new();
    };
    let ffi = FfiText::load(pdfium.bindings(), bytes, None);
    if ffi.doc.is_null() {
        return Vec::new();
    }
    let b = ffi.bindings;
    let page = b.FPDF_LoadPage(ffi.doc, index);
    if page.is_null() {
        return Vec::new();
    }
    let tp = b.FPDFText_LoadPage(page);
    let mut out = Vec::new();
    if !tp.is_null() {
        for g in glyphs(b, tp, true) {
            out.push((g.ch, g.ll, g.lr));
        }
        b.FPDFText_ClosePage(tp);
    }
    b.FPDF_ClosePage(page);
    out
}

/// One text object on a page, for the hidden-layer diagnostic.
#[derive(Debug, Clone)]
pub struct DebugTextObject {
    /// True when the object is drawn invisibly (text render mode 3) — the marker of
    /// a hidden duplicate text layer.
    pub invisible: bool,
    /// Bounding box in native PDF points (bottom-left origin).
    pub l: f32,
    pub b: f32,
    pub r: f32,
    pub t: f32,
    /// The object's text (best-effort; empty if it could not be read).
    pub text: String,
}

/// Diagnostic: every text object on page `index`, each tagged visible/invisible
/// (via the object-level [`FPDFTextObj_GetTextRenderMode`], which — unlike the
/// per-character render-mode API — is available on the default pdfium binding).
/// A hidden duplicate text layer shows up as invisible objects repeating the
/// visible text. Used by the `dump_render_modes` example.
///
/// [`FPDFTextObj_GetTextRenderMode`]: pdfium_render::prelude::PdfiumLibraryBindings::FPDFTextObj_GetTextRenderMode
pub fn debug_text_objects(bytes: &[u8], index: i32) -> Vec<DebugTextObject> {
    let Ok(pdfium) = bind() else {
        return Vec::new();
    };
    let ffi = FfiText::load(pdfium.bindings(), bytes, None);
    if ffi.doc.is_null() {
        return Vec::new();
    }
    let b = ffi.bindings;
    let page = b.FPDF_LoadPage(ffi.doc, index);
    if page.is_null() {
        return Vec::new();
    }
    let tp = b.FPDFText_LoadPage(page);
    let mut out = Vec::new();
    let n = b.FPDFPage_CountObjects(page);
    for i in 0..n {
        let obj = b.FPDFPage_GetObject(page, i);
        if obj.is_null() || b.FPDFPageObj_GetType(obj) != FPDF_PAGEOBJ_TEXT as i32 {
            continue;
        }
        let (mut l, mut bot, mut r, mut top) = (0f32, 0f32, 0f32, 0f32);
        if b.FPDFPageObj_GetBounds(obj, &mut l, &mut bot, &mut r, &mut top) == 0 {
            continue;
        }
        let invisible = b.FPDFTextObj_GetTextRenderMode(obj) == INVISIBLE_RENDER_MODE;
        let text = if tp.is_null() {
            String::new()
        } else {
            // FPDFTextObj_GetText returns the count of UTF-16 code units, including
            // the trailing NUL; call once for the size, once to fill.
            let need = b.FPDFTextObj_GetText(obj, tp, std::ptr::null_mut(), 0);
            if need <= 1 {
                String::new()
            } else {
                let mut buf = vec![0u16; need as usize];
                b.FPDFTextObj_GetText(obj, tp, buf.as_mut_ptr(), need);
                if let Some(&0) = buf.last() {
                    buf.pop();
                }
                String::from_utf16_lossy(&buf)
            }
        };
        out.push(DebugTextObject {
            invisible,
            l,
            b: bot,
            r,
            t: top,
            text,
        });
    }
    if !tp.is_null() {
        b.FPDFText_ClosePage(tp);
    }
    b.FPDF_ClosePage(page);
    out
}

/// Hash a glyph's PDF font name + flags, for `enforce_same_font`. 0 if unavailable.
fn font_hash(b: &dyn PdfiumLibraryBindings, tp: FPDF_TEXTPAGE, i: i32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut flags: std::os::raw::c_int = 0;
    let len = b.FPDFText_GetFontInfo(tp, i, std::ptr::null_mut(), 0, &mut flags);
    if len == 0 {
        return 0;
    }
    let mut buf = vec![0u8; len as usize];
    b.FPDFText_GetFontInfo(
        tp,
        i,
        buf.as_mut_ptr() as *mut std::os::raw::c_void,
        len,
        &mut flags,
    );
    let mut h = std::collections::hash_map::DefaultHasher::new();
    buf.hash(&mut h);
    flags.hash(&mut h);
    h.finish()
}

/// pdfium text render mode 3: the glyph is drawn with neither fill nor stroke —
/// an invisible glyph. Web-to-PDF exporters put a hidden plain-text copy of
/// syntax-highlighted code (and other "copy"/accessibility layers) in this mode,
/// which the char-level text API then extracts as a duplicate of the visible text.
const INVISIBLE_RENDER_MODE: i32 = 3;

fn glyphs(b: &dyn PdfiumLibraryBindings, tp: FPDF_TEXTPAGE, fetch_font: bool) -> Vec<Glyph> {
    let n = b.FPDFText_CountChars(tp);
    let mut out = Vec::with_capacity(n.max(0) as usize);
    for i in 0..n {
        let ch = match char::from_u32(b.FPDFText_GetUnicode(tp, i)) {
            Some(c) => c,
            None => continue,
        };
        if ch == '\r' || ch == '\n' {
            continue;
        }
        // Spaces are font-neutral (0): pdfium's generated spaces carry a default
        // font that would otherwise block every word↔space merge under
        // enforce_same_font; docling-parse's spaces inherit the run's font.
        let font = if fetch_font && !ch.is_whitespace() {
            font_hash(b, tp, i)
        } else {
            0
        };
        let (mut l, mut r, mut bot, mut top) = (0f64, 0f64, 0f64, 0f64);
        let has_box = b.FPDFText_GetCharBox(tp, i, &mut l, &mut r, &mut bot, &mut top) != 0;
        // Loose box: font ascent/descent + glyph advance, uniform per font/size.
        let mut lr = FS_RECTF {
            left: 0.0,
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
        };
        let (ll, lb, lrt, ltop) = if b.FPDFText_GetLooseCharBox(tp, i, &mut lr) != 0 {
            (lr.left, lr.bottom, lr.right, lr.top)
        } else if has_box {
            (l as f32, bot as f32, r as f32, top as f32)
        } else {
            (f32::NAN, 0.0, 0.0, 0.0)
        };
        if ch.is_whitespace() {
            // Keep the space *with its box* (the docling-parse-style line sanitizer
            // needs literal space glyphs); NaN `l` if pdfium reports no box (the
            // legacy `lines_from_glyphs` ignores the box and only flags a space).
            out.push(Glyph {
                ch: ' ',
                l: if has_box { l as f32 } else { f32::NAN },
                b: if has_box { bot as f32 } else { 0.0 },
                r: if has_box { r as f32 } else { 0.0 },
                t: if has_box { top as f32 } else { 0.0 },
                ll,
                lb,
                lr: lrt,
                lt: ltop,
                font,
            });
            continue;
        }
        if !has_box {
            continue;
        }
        out.push(Glyph {
            ch,
            l: l as f32,
            b: bot as f32,
            r: r as f32,
            t: top as f32,
            ll,
            lb,
            lr: lrt,
            lt: ltop,
            font,
        });
    }
    // pdfium splits the Arabic lam-alef ligature into two chars at the *same* x
    // (it's one glyph) in visual order — `alef-variant, lam`. docling-parse and
    // logical order are `lam, alef-variant`. Detect the ligature by the shared x
    // and swap. The shared-x test reliably distinguishes a true ligature from a
    // genuine `alef + lam` sequence (the article `ال`, or `فعالة`), whose two
    // glyphs sit at different x and must NOT be reordered.
    for i in 0..out.len().saturating_sub(1) {
        let same_x = out[i].l.is_finite()
            && out[i + 1].l.is_finite()
            && (out[i].l - out[i + 1].l).abs() < 1.0;
        if same_x
            && matches!(out[i].ch, '\u{0622}' | '\u{0623}' | '\u{0625}' | '\u{0627}')
            && out[i + 1].ch == '\u{0644}'
        {
            out.swap(i, i + 1);
        }
    }
    // Reconstruct degenerate (zero-width) loose space boxes by spanning the gap to
    // the next glyph on the same line, so the sanitizer keeps them as word
    // separators rather than dropping them (which would merge `Information systems`
    // → `Informationsystems`). pdfium gives generated spaces a zero-width box at a
    // wrong baseline; a wrap (different baseline) or a touching gap is left alone.
    for i in 0..out.len() {
        if out[i].ch != ' ' || (out[i].lr - out[i].ll).abs() >= 0.5 {
            continue;
        }
        let prev = out[..i]
            .iter()
            .rev()
            .find(|g| g.ch != ' ' && g.ll.is_finite())
            .map(|g| (g.lr, g.lb, g.lt));
        let next = out[i + 1..]
            .iter()
            .find(|g| g.ch != ' ' && g.ll.is_finite())
            .map(|g| (g.ll, g.lb));
        if let (Some((plr, plb, plt)), Some((nll, nlb))) = (prev, next) {
            let line_h = (plt - plb).abs().max(1.0);
            if (plb - nlb).abs() < line_h * 0.5 && nll > plr + 0.5 {
                out[i].ll = plr;
                out[i].lr = nll;
                out[i].lb = plb;
                out[i].lt = plt;
            }
        }
    }
    out
}

/// How [`lines_from_glyphs`] splits a line into words.
#[derive(Clone, Copy, PartialEq)]
enum Grouping {
    /// Gap heuristic + punctuation glue (`engines,`, `[37`, `98.5`) — prose.
    Prose,
    /// Split only at literal space glyphs, never glue — pdfium code cells.
    /// pdfium's monospace listings carry a real space glyph at every source space,
    /// and its overhanging loose boxes would make the gap heuristic over-split
    /// (`f un c t i o n`), so honouring just the spaces reproduces the spacing.
    CodeSpaceOnly,
    /// Split on the inter-glyph **gap** (or a space glyph), but never glue — for
    /// the parser's code cells: the parser emits no space glyphs (a source space
    /// is a positioning gap), and its clean advance boxes make the gap reliable.
    /// Unlike [`Grouping::Prose`] there is no punctuation glue, so a real gap
    /// always splits (`et al. 2000`, not `et al.2000`) while genuinely touching
    /// tokens stay joined (`add(a,` / `b)`).
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
            // Don't split before closing punctuation, after opening punctuation, or
            // after a period that runs into a digit/lowercase letter — docling
            // keeps `engines,` / `[37` / `i.e.` / `98.5` together even across a
            // space or gap.
            let glued = is_close_punct(g.ch)
                || is_open_punct(p.ch)
                || (p.ch.is_ascii_digit() && g.ch.is_ascii_digit())
                || (p.ch == '.'
                    && !pending_space
                    && (g.ch.is_ascii_digit() || g.ch.is_ascii_lowercase()));
            let word_gap = line_h.max(h) * 0.25;
            new_word = if mode == Grouping::CodeSpaceOnly {
                new_line || pending_space
            } else if mode == Grouping::CodeGap {
                // Gap-based, no glue: a real gap always splits, touching tokens join.
                new_line || pending_space || g.l - p.r > word_gap
            } else if is_arabic(g.ch) || is_arabic(p.ch) {
                // RTL runs right-to-left, so the inter-word gap is `p.l - g.r`. A
                // real word space has a gap; pdfium also emits spurious zero-gap
                // space glyphs inside words (`التي`), so require the gap rather
                // than trusting a bare space glyph.
                new_line || (p.l - g.r > word_gap && !glued)
            } else {
                new_line || ((pending_space || g.l - p.r > word_gap) && !glued)
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

/// Code line cells from the **parser**'s glyph stream. Unlike pdfium — whose
/// monospace listings carry explicit space glyphs (so [`Grouping::CodeSpaceOnly`]
/// keeps their spacing) — the parser emits no space glyphs: a source space is a
/// positioning gap. So code cells use [`Grouping::CodeGap`], which splits on the
/// inter-glyph gap (a space wherever it exceeds ~0.25× the line height) but never
/// glues punctuation, so `et al. 2000` keeps its space while `add(a,` / `b)` stay
/// joined. The parser's clean advance boxes make the gap heuristic reliable here,
/// where pdfium's overhanging loose boxes would over-split (`f un c t i o n`).
pub(crate) fn code_cells_from_glyphs(gs: &[Glyph], page_h: f32) -> Vec<TextCell> {
    lines_from_glyphs(gs, page_h, Grouping::CodeGap)
}

/// Per-word cells (each word's text + top-left bbox), using the same word/line
/// splitting as [`lines_from_glyphs`] but emitting one cell per word instead of
/// joining into lines — the legacy gap-heuristic word grouping, kept for the
/// pdfium word path (`DOCLING_PDFIUM_WORDS`). The default parser path uses
/// [`crate::dp_lines::word_cells`] instead.
pub(crate) fn words_from_glyphs(gs: &[Glyph], page_h: f32) -> Vec<TextCell> {
    let mut cells = Vec::new();
    let mut word = String::new();
    let inf = (
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    );
    let (mut wl, mut wb, mut wr, mut wt) = inf;
    let mut line_h: f32 = 0.0;
    let mut prev: Option<&Glyph> = None;
    let mut pending_space = false;
    for g in gs {
        if g.ch == ' ' {
            pending_space = true;
            continue;
        }
        let h = (g.t - g.b).abs().max(1.0);
        let mut new_line = false;
        let mut new_word = false;
        if let Some(p) = prev {
            // LTR wraps reset x leftward (`g.l < p.r`); RTL (Arabic) wraps reset
            // rightward (the new line begins at the far right). A large drop
            // (≥1.5× line height) is a new line regardless of x.
            let x_reset = if is_arabic(g.ch) || is_arabic(p.ch) {
                g.l > p.r
            } else {
                g.l < p.r
            };
            new_line = (p.b - g.b > h * 0.5 && x_reset) || (p.b - g.b > line_h.max(h) * 1.5);
            // No digit-digit glue here (unlike the prose grouping): table cells in
            // adjacent columns are numeric and a column gap must still split them
            // (`0.965` `0.934`, not `0.9650.934`). Intra-number digits have no gap
            // so they stay together regardless.
            let glued = is_close_punct(g.ch)
                || is_open_punct(p.ch)
                || (p.ch == '.'
                    && !pending_space
                    && (g.ch.is_ascii_digit() || g.ch.is_ascii_lowercase()));
            let word_gap = line_h.max(h) * 0.25;
            new_word = new_line || ((pending_space || g.l - p.r > word_gap) && !glued);
        }
        pending_space = false;
        if new_word && !word.is_empty() {
            cells.push(TextCell {
                text: std::mem::take(&mut word),
                l: wl,
                t: page_h - wt,
                r: wr,
                b: page_h - wb,
            });
            (wl, wb, wr, wt) = inf;
        }
        if new_line {
            line_h = 0.0;
        }
        word.push(g.ch);
        wl = wl.min(g.l);
        wb = wb.min(g.b);
        wr = wr.max(g.r);
        wt = wt.max(g.t);
        line_h = line_h.max(h);
        prev = Some(g);
    }
    if !word.is_empty() {
        cells.push(TextCell {
            text: word,
            l: wl,
            t: page_h - wt,
            r: wr,
            b: page_h - wb,
        });
    }
    cells
}

fn is_arabic(c: char) -> bool {
    ('\u{0600}'..='\u{06FF}').contains(&c)
}

fn is_close_punct(c: char) -> bool {
    matches!(
        c,
        ',' | '.' | ';' | '!' | '?' | ')' | ']' | '}' | '%' | '\'' | '\u{2019}' | '\u{2018}'
    )
}

fn is_open_punct(c: char) -> bool {
    // `@` glues to what follows (`mAP @0.5`, `bpf@zurich`, `@decorator`).
    matches!(c, '(' | '[' | '{' | '@')
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
