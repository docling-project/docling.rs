//! pdfium-based text extraction and page rendering (docling's `PdfPipeline`
//! text path uses pypdfium2 the same way).

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
    pub fn open(bytes: &[u8], password: Option<&str>) -> Result<Self, PdfiumError> {
        let pdfium = bind()?;
        let doc = pdfium.load_pdf_from_byte_slice(bytes, password)?;
        let mut pages = Vec::new();
        for page in doc.pages().iter() {
            pages.push(extract_page(&page)?);
        }
        Ok(PdfDocument { pages })
    }
}

fn extract_page(page: &pdfium_render::prelude::PdfPage<'_>) -> Result<PdfPage, PdfiumError> {
    let width = page.width().value;
    let height = page.height().value;

    let text = page.text()?;
    let mut cells = Vec::new();
    for segment in text.segments().iter() {
        let rect = segment.bounds();
        let s = segment.text();
        if s.trim().is_empty() {
            continue;
        }
        // Flip Y to a top-left origin.
        cells.push(TextCell {
            text: s,
            l: rect.left().value,
            t: height - rect.top().value,
            r: rect.right().value,
            b: height - rect.bottom().value,
        });
    }

    let tw = (width * RENDER_SCALE).round().max(1.0) as i32;
    let th = (height * RENDER_SCALE).round().max(1.0) as i32;
    let cfg = PdfRenderConfig::new()
        .set_target_width(tw)
        .set_target_height(th);
    let bitmap = page.render_with_config(&cfg)?;
    let image = bitmap.as_image().into_rgb8();

    Ok(PdfPage {
        width,
        height,
        scale: RENDER_SCALE,
        cells,
        image,
    })
}
