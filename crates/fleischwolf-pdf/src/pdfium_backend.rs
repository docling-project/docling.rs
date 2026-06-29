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
    pub image: RgbImage,
}

/// A parsed PDF: per-page text cells and page images.
pub struct PdfDocument {
    pub pages: Vec<PdfPage>,
}

/// Bind to the pdfium dynamic library. Honors `PDFIUM_DYNAMIC_LIB_PATH` (a
/// directory or file), else the directory of the current exe, else the system
/// library — mirroring how a deployment ships `libpdfium` alongside the binary.
fn bind() -> Result<Pdfium, PdfiumError> {
    if let Ok(path) = std::env::var("PDFIUM_DYNAMIC_LIB_PATH") {
        let name = Pdfium::pdfium_platform_library_name_at_path(&path);
        if let Ok(b) = Pdfium::bind_to_library(&name) {
            return Ok(Pdfium::new(b));
        }
        if let Ok(b) = Pdfium::bind_to_library(&path) {
            return Ok(Pdfium::new(b));
        }
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
        let mut pages = Vec::new();
        for (i, page) in doc.pages().iter().enumerate() {
            pages.push(extract_page(&page, &ffi, i as i32)?);
        }
        Ok(PdfDocument { pages })
    }
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
    let pages = doc.pages();
    let total = pages.len() as usize;
    for (i, page) in pages.iter().enumerate() {
        let extracted = extract_page(&page, &ffi, i as i32)?;
        f(i, total, extracted)?;
    }
    Ok(())
}

fn extract_page(
    page: &pdfium_render::prelude::PdfPage<'_>,
    ffi: &FfiText<'_>,
    index: i32,
) -> Result<PdfPage, PdfiumError> {
    let width = page.width().value;
    let height = page.height().value;

    let (mut cells, code_cells) = ffi.page_cells(index, height);
    if cells.is_empty() {
        cells = segment_cells(&page.text()?, height);
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
    let bitmap = page.render_with_config(&cfg)?;
    let big = bitmap.as_image().into_rgb8();
    let dw = (width * RENDER_SCALE).round().max(1.0) as u32;
    let dh = (height * RENDER_SCALE).round().max(1.0) as u32;
    let image = image::imageops::resize(&big, dw, dh, image::imageops::FilterType::CatmullRom);

    Ok(PdfPage {
        width,
        height,
        scale: RENDER_SCALE,
        cells,
        code_cells,
        image,
    })
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

/// One glyph: codepoint + native (bottom-left) box edges.
struct Glyph {
    ch: char,
    l: f32,
    b: f32,
    r: f32,
    t: f32,
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
    fn page_cells(&self, index: i32, page_h: f32) -> (Vec<TextCell>, Vec<TextCell>) {
        if self.doc.is_null() {
            return (Vec::new(), Vec::new());
        }
        let b = self.bindings;
        let page = b.FPDF_LoadPage(self.doc, index);
        if page.is_null() {
            return (Vec::new(), Vec::new());
        }
        let tp = b.FPDFText_LoadPage(page);
        let out = if tp.is_null() {
            (Vec::new(), Vec::new())
        } else {
            let g = glyphs(b, tp);
            b.FPDFText_ClosePage(tp);
            (
                lines_from_glyphs(&g, page_h, false),
                lines_from_glyphs(&g, page_h, true),
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
fn glyphs(b: &dyn PdfiumLibraryBindings, tp: FPDF_TEXTPAGE) -> Vec<Glyph> {
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
        if ch.is_whitespace() {
            out.push(Glyph {
                ch: ' ',
                l: f32::NAN,
                b: 0.0,
                r: 0.0,
                t: 0.0,
            });
            continue;
        }
        let (mut l, mut r, mut bot, mut top) = (0f64, 0f64, 0f64, 0f64);
        if b.FPDFText_GetCharBox(tp, i, &mut l, &mut r, &mut bot, &mut top) == 0 {
            continue;
        }
        out.push(Glyph {
            ch,
            l: l as f32,
            b: bot as f32,
            r: r as f32,
            t: top as f32,
        });
    }
    out
}

/// Group glyphs (document order) into words then lines, the way docling-parse
/// does: a new **word** starts where the horizontal gap to the previous glyph
/// exceeds ~0.2 × the font height (a real space is ~0.3 × height; letter
/// tracking is smaller, so titles don't shatter); a new **line** starts where
/// the baseline drops by ~half the font height (a superscript rises without
/// dropping, so it stays on its line). Coordinates are flipped to top-left.
/// `code` mode splits words **only** at pdfium's own space glyphs and never glues
/// punctuation — monospace code has wide inter-glyph advances that the prose
/// gap heuristic mistakes for spaces (`f un c t i o n`), but pdfium emits a real
/// space glyph at every true gap, so honoring just those reproduces the source
/// spacing (`function add(a, b)`).
fn lines_from_glyphs(gs: &[Glyph], page_h: f32, code: bool) -> Vec<TextCell> {
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
            new_line = (p.b - g.b > h * 0.5 && g.l < p.r) || (p.b - g.b > line_h.max(h) * 1.5);
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
            new_word = if code {
                new_line || pending_space
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

fn is_close_punct(c: char) -> bool {
    matches!(
        c,
        ',' | '.' | ';' | '!' | '?' | ')' | ']' | '}' | '%' | '\'' | '\u{2019}' | '\u{2018}'
    )
}

fn is_open_punct(c: char) -> bool {
    matches!(c, '(' | '[' | '{')
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
