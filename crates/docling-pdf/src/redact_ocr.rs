//! `ImageRedaction::BoxOut` for the PII redaction pass (#621): OCR an
//! embedded image with the pipeline's own recognizer (the text detector's
//! line crops when `ocr_det.onnx` is installed, Tesseract under
//! `ocr_engine`), ask the pass which lines carry personal data, and paint a
//! solid box over each such line before the image goes out in the JSON /
//! embedded Markdown. The image is read the way a standalone image input
//! is — its own scale-1.0 page, one text region covering it, docling's OCR
//! resolution for images (#570) — so what the scan pipeline would read off
//! it is what gets covered.

use image::{Rgb, RgbImage};

use crate::layout::Region;
use crate::pdfium_backend::{PdfPage, TextCell};
use crate::{decode_image_limited, ocr_input, page_ocr_scale, PdfError, Pipeline};

/// Padding around a covered line, in image pixels: the recognizer's line
/// box hugs the ink, a few pixels more hide the ascender/descender fringe.
const PAD: u32 = 2;

impl Pipeline {
    /// Box out the lines of `bytes` (an embedded PNG/JPEG/…) for which
    /// `has_pii` says so. `Ok(None)` when no line matched — the image is
    /// kept as it is; `Ok(Some(png))` the repainted image as PNG bytes;
    /// `Err` when the OCR could not run (no recognizer: the caller degrades
    /// to dropping the image).
    pub fn box_out(
        &mut self,
        bytes: &[u8],
        has_pii: &dyn Fn(&str) -> bool,
    ) -> Result<Option<Vec<u8>>, PdfError> {
        let mut image = decode_image_limited(bytes)?;
        let (w, h) = image.dimensions();
        if w < 2 || h < 2 {
            return Ok(None);
        }
        let cells = self.ocr_image_lines(&image)?;
        let hit: Vec<&TextCell> = cells.iter().filter(|c| has_pii(&c.text)).collect();
        if hit.is_empty() {
            return Ok(None);
        }
        for c in hit {
            let l = (c.l.max(0.0) as u32).saturating_sub(PAD);
            let t = (c.t.max(0.0) as u32).saturating_sub(PAD);
            let r = ((c.r.max(0.0) as u32).saturating_add(PAD)).min(w);
            let b = ((c.b.max(0.0) as u32).saturating_add(PAD)).min(h);
            for y in t..b {
                for x in l..r {
                    image.put_pixel(x, y, Rgb([0, 0, 0]));
                }
            }
        }
        let mut png = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(|e| PdfError::Pdfium(format!("box_out: png encode: {e}")))?;
        Ok(Some(png))
    }

    /// The recognizer's line cells over a whole image, in image pixels.
    /// `Err` when the recognizer is unavailable (`skip_ocr`, model missing).
    fn ocr_image_lines(&mut self, image: &RgbImage) -> Result<Vec<TextCell>, PdfError> {
        let (w, h) = image.dimensions();
        let page = PdfPage {
            width: w as f32,
            height: h as f32,
            scale: 1.0,
            cells: Vec::new(),
            code_cells: Vec::new(),
            checkboxes: Vec::new(),
            bitmaps: Vec::new(),
            word_cells: Vec::new(),
            image_layout: None,
            image: image.clone(),
            links: Vec::new(),
            rotation: 0,
            keep_typography: false,
            #[cfg(feature = "render")]
            embedded_images: Vec::new(),
            #[cfg(feature = "ml")]
            pending_image: None,
        };
        let ocr_scale = page_ocr_scale(self.ocr_scale, page.width, page.height, page.scale);
        let mut cache = None;
        let (view, scale) = ocr_input(&mut cache, &page.image, 1.0, ocr_scale);
        let region = Region {
            label: "text",
            score: 1.0,
            l: 0.0,
            t: 0.0,
            r: w as f32,
            b: h as f32,
        };
        let worker = self.primary()?;
        let detected = worker
            .with_det(|det| det.detect(view))
            .transpose()
            .map_err(PdfError::Ocr)?;
        let Some(model) = worker.ocr_model()? else {
            return Err(PdfError::Ocr("OCR model unavailable".into()));
        };
        let cells = model
            .ocr_page_with(
                view,
                std::slice::from_ref(&region),
                scale,
                detected.as_deref(),
            )
            .map_err(PdfError::Ocr)?;
        Ok(cells.into_iter().map(|(c, _)| c).collect())
    }
}
