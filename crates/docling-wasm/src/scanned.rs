//! Browser scanned-document pipeline — stage 2 of #157 (the *lite profile*):
//! RT-DETR layout detection + PP-OCRv3 recognition, both via ONNX Runtime Web
//! on the JS side; region refinement, region-cropped OCR, geometric table
//! reconstruction and reading-order assembly in Rust — the same code as the
//! native pipeline (`docling_pdf::{layout, ocr_prep, scanned}`). TableFormer
//! and the enrichment models are stage 3; table regions fall back to the
//! geometric reconstruction the native `--no-table-former` flag uses.
//!
//! Pages arrive as raw RGBA bitmaps: for a scanned PDF the host page renders
//! them with pdf.js (`page.getViewport({scale: 2})` — 2 px per PDF point,
//! matching the native pipeline's `RENDER_SCALE`); a standalone image is its
//! own page at scale 1, exactly like the native image path.
//!
//! ```js
//! const conv = new ScannedConverter(dictText);
//! for (const bitmap of pages) {
//!   await conv.add_page(bitmap.data, bitmap.width, bitmap.height, 2.0, layout, rec);
//! }
//! const markdown = conv.finish("scan.pdf", "md");
//! ```
//! (`www/scan.html` is the complete wiring.)

use docling_pdf::layout::{decode_layout, layout_input, SIDE};
use docling_pdf::ocr_prep::{
    batch_input_padded, decode_row, dict_chars, normalize_polarity, prep_region_lines,
    width_batches_padded, REC_HEIGHT,
};
use docling_pdf::pdfium_backend::{PdfPage, TextCell};
use docling_pdf::scanned::{assemble_page, finish_document, refine_regions};
use image::RgbImage;
use wasm_bindgen::prelude::*;

use crate::ocr::{tensor_parts, RecSession};

#[wasm_bindgen]
extern "C" {
    /// The JS-side layout session: a wrapper around an `ort.InferenceSession`
    /// over the RT-DETR layout model exposing `run(data)` — feed the
    /// `(1, 3, 640, 640)` CHW float buffer, resolve to
    /// `{ logits: {data, dims: [1, q, c]}, boxes: {data, dims: [1, q, 4]} }`.
    pub type LayoutSession;

    #[wasm_bindgen(method, catch)]
    pub async fn run(this: &LayoutSession, data: js_sys::Float32Array) -> Result<JsValue, JsValue>;
}

/// Multi-page scanned-document converter (lite profile). Feed pages in
/// order, then [`finish`](Self::finish) — cross-page paragraph continuations
/// merge exactly like the native pipeline.
#[wasm_bindgen]
pub struct ScannedConverter {
    chars: Vec<String>,
    pages: Vec<docling_pdf::scanned::AssembledPage>,
}

#[wasm_bindgen]
impl ScannedConverter {
    /// `dict` is the recognition dictionary text (`en_dict.txt` for the
    /// default English model).
    #[wasm_bindgen(constructor)]
    pub fn new(dict: &str) -> Self {
        Self {
            chars: dict_chars(dict),
            pages: Vec::new(),
        }
    }

    /// Convert one page: `rgba` is the rendered bitmap (canvas ImageData),
    /// `scale` its pixels-per-PDF-point (2.0 for pdf.js `{scale: 2}`; 1.0
    /// for a standalone image).
    pub async fn add_page(
        &mut self,
        rgba: &[u8],
        px_w: u32,
        px_h: u32,
        scale: f32,
        layout: &LayoutSession,
        rec: &RecSession,
    ) -> Result<(), JsError> {
        if rgba.len() != (px_w as usize) * (px_h as usize) * 4 {
            return Err(JsError::new("rgba buffer size does not match dimensions"));
        }
        let mut img = RgbImage::new(px_w, px_h);
        for (i, px) in img.pixels_mut().enumerate() {
            px.0 = [rgba[i * 4], rgba[i * 4 + 1], rgba[i * 4 + 2]];
        }
        // Dark-mode screenshots invert scan polarity; normalize before both
        // layout and OCR (each assumes dark ink on light paper).
        let img = normalize_polarity(img);
        let (page_w, page_h) = (px_w as f32 / scale, px_h as f32 / scale);

        // Layout: Rust preprocessing → JS inference → Rust decoding.
        let input = layout_input(&img);
        let out = layout
            .run(js_sys::Float32Array::from(input.as_slice()))
            .await
            .map_err(|e| JsError::new(&format!("layout session.run: {e:?}")))?;
        let get = |k: &str| {
            js_sys::Reflect::get(&out, &JsValue::from_str(k))
                .map_err(|_| JsError::new(&format!("layout result has no `{k}`")))
        };
        let (logits, q, c) = tensor_parts(&get("logits")?)?;
        let (boxes, bq, four) = tensor_parts(&get("boxes")?)?;
        if bq != q || four != 4 {
            return Err(JsError::new("layout boxes dims must be [1, q, 4]"));
        }
        let regions = decode_layout(&logits, &boxes, q, c, page_w, page_h);
        let regions = refine_regions(regions, &[], page_w, page_h);

        // OCR the text regions (same gather/batch/decode as native ocr_page).
        let (bboxes, lines) = prep_region_lines(&img, &regions, scale);
        let mut texts = vec![String::new(); lines.len()];
        // Padded batching across differing widths (browser-only): far fewer ORT
        // calls than exact-width grouping, output-equivalent under zero padding.
        for (w, chunk) in width_batches_padded(&lines) {
            let data = batch_input_padded(w, &chunk, &lines);
            let out = rec
                .run(
                    chunk.len() as u32,
                    REC_HEIGHT,
                    w as u32,
                    js_sys::Float32Array::from(data.as_slice()),
                )
                .await
                .map_err(|e| JsError::new(&format!("rec session.run: {e:?}")))?;
            let (probs, t_len, nc) = tensor_parts(&out)?;
            if probs.len() < chunk.len() * t_len * nc {
                return Err(JsError::new("rec session.run returned a short tensor"));
            }
            for (i, &ix) in chunk.iter().enumerate() {
                texts[ix] = decode_row(
                    &self.chars,
                    &probs[i * t_len * nc..(i + 1) * t_len * nc],
                    nc,
                );
            }
        }
        let mut cells = Vec::new();
        for ((l, t, r, b), text) in bboxes.into_iter().zip(texts) {
            let text = text.trim().to_string();
            if text.is_empty() {
                continue;
            }
            cells.push(TextCell { text, l, t, r, b });
        }

        let page = PdfPage::from_cells(page_w, page_h, scale, cells);
        self.pages.push(assemble_page(&page, regions));
        Ok(())
    }

    /// Number of pages converted so far (progress display).
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Assemble the accumulated pages into the final document and render it
    /// as `"md"` (default) or `"json"`. Resets the converter.
    pub fn finish(&mut self, name: &str, to: Option<String>) -> Result<String, JsError> {
        let doc = finish_document(name, std::mem::take(&mut self.pages));
        match to.as_deref().unwrap_or("md") {
            "md" | "markdown" => Ok(doc.export_to_markdown()),
            "json" => Ok(doc.export_to_json()),
            other => Err(JsError::new(&format!(
                "unknown output format {other:?} (expected \"md\" or \"json\")"
            ))),
        }
    }
}

/// One-shot scanned-image conversion through the full lite profile (layout +
/// OCR + assembly) — the browser counterpart of the native image path
/// (a standalone image is its own page at scale 1).
#[wasm_bindgen]
pub async fn convert_scanned_image(
    bytes: &[u8],
    name: &str,
    dict: &str,
    layout: &LayoutSession,
    rec: &RecSession,
    to: Option<String>,
) -> Result<String, JsError> {
    let img = image::load_from_memory(bytes)
        .map_err(|e| JsError::new(&format!("decode image: {e}")))?
        .to_rgba8();
    let (w, h) = img.dimensions();
    let mut conv = ScannedConverter::new(dict);
    conv.add_page(img.as_raw(), w, h, 1.0, layout, rec).await?;
    conv.finish(name, to)
}

// Silence the unused warning for SIDE re-export path (the JS side sizes its
// tensor from the buffer length, but the constant documents the contract).
const _: u32 = SIDE;
