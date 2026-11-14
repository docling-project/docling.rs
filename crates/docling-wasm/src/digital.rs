//! Browser pipeline for **digital** PDFs — the ones that carry a text layer
//! (#157). It is the scanned pipeline with the expensive half removed: the text
//! comes from the PDF itself rather than from OCR, so only the layout model
//! runs, and the result is both faster and exact (no recognition errors, no
//! mangled umlauts).
//!
//! This is what the native pipeline does for a digital PDF — it OCRs a page
//! only when `page.cells` is empty — so the same three steps happen here in the
//! same order: layout detection, region refinement against the real text cells,
//! then TableFormer (or the geometric fallback) per table region.
//!
//! Without it the browser had to choose between structure and fidelity: the
//! pure text-layer path (`convert`) is milliseconds but emits flat paragraphs,
//! because headings, tables and pictures are all things the layout model finds.
//!
//! ```js
//! const conv = new DigitalConverter(pdfBytes, dictText); // throws when there is no text layer
//! for (let i = 0; i < conv.page_count(); i++) {
//!   // rasterize page i with pdf.js at 2 px/point, then:
//!   await conv.add_page(i, rgba, w, h, 2.0, layout, rec); // rec optional: OCRs embedded pictures
//! }
//! const markdown = conv.finish("bill.pdf", "md", "embedded");
//! ```

use docling_pdf::assemble::{
    add_orphan_regions, geometric_rows_share_lines, geometric_table_is_reliable, reconstruct_table,
};
use docling_pdf::layout::{decode_layout, layout_input, Region};
use docling_pdf::ocr_prep::{dict_chars, prep_region_lines};
use docling_pdf::pdfium_backend::{PdfPage, TextCell};
use docling_pdf::scanned::{
    assemble_page_with_tables, drop_duplicate_text_claims, finish_document, refine_regions,
};
use image::RgbImage;
use wasm_bindgen::prelude::*;

use crate::ocr::{tensor_parts, RecSession};
use crate::scanned::{ocr_lines, render, LayoutSession};
use crate::tableformer::TfSession;

/// A digital PDF being converted page by page. Construct it from the file's
/// bytes (the text layer is parsed once, in Rust), then feed the rasterized
/// pages in order.
#[wasm_bindgen]
pub struct DigitalConverter {
    /// One entry per page, carrying that page's text cells in PDF points.
    pages: Vec<PdfPage>,
    /// Recognition dictionary, when the host wants embedded pictures OCR'd.
    chars: Vec<String>,
    out: Vec<docling_pdf::scanned::AssembledPage>,
}

#[wasm_bindgen]
impl DigitalConverter {
    /// Parse the PDF's text layer. Fails when there is none — the caller should
    /// fall back to the scanned pipeline, exactly as the demo page does.
    /// `dict` (optional) is the recognition dictionary text; with it and a
    /// `RecSession` on `add_page`, embedded raster pictures get OCR'd too.
    #[wasm_bindgen(constructor)]
    pub fn new(bytes: &[u8], dict: Option<String>) -> Result<DigitalConverter, JsError> {
        let pages = docling_pdf::textparse::pdf_text_pages(bytes);
        // Vestigial covers the all-empty case too — and the scanned form with a
        // typed-in date, whose three tiny strings must not masquerade as the
        // document's text (the letter on those pages needs OCR).
        if docling_pdf::textparse::text_layer_is_vestigial(&pages) {
            return Err(JsError::new(
                "PDF has no usable embedded text layer (scanned/image-only?) — use the OCR pipeline",
            ));
        }
        Ok(Self {
            pages,
            chars: dict.as_deref().map(dict_chars).unwrap_or_default(),
            out: Vec::new(),
        })
    }

    /// Pages the text parser found.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Install the recognition dictionary after construction — the host probes
    /// the text layer first (the constructor throws on a scan) and only then
    /// fetches the recognition model + dictionary.
    #[wasm_bindgen(js_name = setDict)]
    pub fn set_dict(&mut self, dict: &str) {
        self.chars = dict_chars(dict);
    }

    /// Convert page `index` (0-based) given its rendered bitmap: layout
    /// detection plus geometric tables. `scale` is the raster's pixels per PDF
    /// point (2.0 for pdf.js `{scale: 2}`). With `rec` (and a dictionary at
    /// construction), embedded raster pictures that carry no text cells are
    /// OCR'd — the text a digital page's images hide from its text layer.
    #[allow(clippy::too_many_arguments)]
    pub async fn add_page(
        &mut self,
        index: usize,
        rgba: &[u8],
        px_w: u32,
        px_h: u32,
        scale: f32,
        layout: &LayoutSession,
        rec: Option<RecSession>,
    ) -> Result<(), JsError> {
        self.add_page_impl(index, rgba, px_w, px_h, scale, layout, rec.as_ref(), None)
            .await
    }

    /// [`add_page`](Self::add_page) with TableFormer for the table regions whose
    /// geometric reconstruction looks unreliable.
    #[wasm_bindgen(js_name = addPageTf)]
    #[allow(clippy::too_many_arguments)]
    pub async fn add_page_tf(
        &mut self,
        index: usize,
        rgba: &[u8],
        px_w: u32,
        px_h: u32,
        scale: f32,
        layout: &LayoutSession,
        tf: &TfSession,
        rec: Option<RecSession>,
    ) -> Result<(), JsError> {
        self.add_page_impl(
            index,
            rgba,
            px_w,
            px_h,
            scale,
            layout,
            rec.as_ref(),
            Some(tf),
        )
        .await
    }

    /// Assemble the converted pages into a document and render it as `"md"`
    /// (default), `"json"`, `"doclang"` or `"latex"`, with `images` picking how pictures
    /// render in Markdown. Resets the converter.
    pub fn finish(
        &mut self,
        name: &str,
        to: Option<String>,
        images: Option<String>,
    ) -> Result<String, JsError> {
        let doc = finish_document(name, std::mem::take(&mut self.out));
        render(&doc, to.as_deref(), images.as_deref())
    }
}

impl DigitalConverter {
    #[allow(clippy::too_many_arguments)]
    async fn add_page_impl(
        &mut self,
        index: usize,
        rgba: &[u8],
        px_w: u32,
        px_h: u32,
        scale: f32,
        layout: &LayoutSession,
        rec: Option<&RecSession>,
        tf: Option<&TfSession>,
    ) -> Result<(), JsError> {
        if rgba.len() != (px_w as usize) * (px_h as usize) * 4 {
            return Err(JsError::new("rgba buffer size does not match dimensions"));
        }
        let mut page = self
            .pages
            .get(index)
            .cloned()
            .ok_or_else(|| JsError::new("page index is out of range"))?;

        let mut img = RgbImage::new(px_w, px_h);
        for (i, px) in img.pixels_mut().enumerate() {
            px.0 = [rgba[i * 4], rgba[i * 4 + 1], rgba[i * 4 + 2]];
        }

        // Layout: Rust preprocessing → JS inference → Rust decoding. The page
        // geometry comes from the text parser, not the bitmap, so a raster at
        // any scale lines up with the cells (which are in PDF points).
        let (page_w, page_h) = (page.width, page.height);
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
        // Refine against the *real* text cells: unlike the OCR path, orphan-text
        // recovery and the false-picture drop have something to work with here.
        let regions = refine_regions(regions, &page.cells, page_w, page_h);
        // The pdf.js raster can tease overlapping text detections out of the
        // model (a block plus its own parts), and every overlap reads the same
        // cells twice — drop the re-readers.
        let mut regions = drop_duplicate_text_claims(regions, &page.cells);

        // Python docling OCRs the bitmap-covered areas of *every* page — even a
        // digital one — once they exceed `bitmap_area_threshold` (5 % of the
        // page). A picture region without a single text cell is exactly that: an
        // embedded raster whose text the text layer cannot see (e.g. terms-and-
        // conditions boxes exported as images). Recognize those crops and merge
        // the lines in as text regions, the way docling's orphan-cell recovery
        // turns unclaimed OCR cells into text clusters next to the picture.
        if let Some(rec) = rec.filter(|_| !self.chars.is_empty()) {
            let page_area = (page_w * page_h).max(1.0);
            let has_text = |r: &Region| {
                page.cells.iter().any(|c| {
                    let ca = ((c.r - c.l) * (c.b - c.t)).max(1.0);
                    let ix = (r.r.min(c.r) - r.l.max(c.l)).max(0.0);
                    let iy = (r.b.min(c.b) - r.t.max(c.t)).max(0.0);
                    !c.text.trim().is_empty() && ix * iy / ca > 0.5
                })
            };
            let bare: Vec<Region> = regions
                .iter()
                .filter(|r| {
                    r.label == "picture"
                        && (r.r - r.l) * (r.b - r.t) / page_area >= 0.05
                        && !has_text(r)
                })
                .map(|r| Region {
                    label: "text",
                    ..r.clone()
                })
                .collect();
            if !bare.is_empty() {
                let (bboxes, lines) = prep_region_lines(&img, &bare, scale);
                let texts = ocr_lines(&self.chars, rec, &lines).await?;
                let mut ocr_cells = Vec::new();
                for ((l, t, r, b), text) in bboxes.into_iter().zip(texts) {
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        ocr_cells.push(TextCell { text, l, t, r, b });
                    }
                }
                if !ocr_cells.is_empty() {
                    page.cells.extend(ocr_cells.iter().cloned());
                    // Dense wide OCR text demotes the crop to paragraphs;
                    // sparse text keeps the picture and reads out beside it.
                    docling_pdf::assemble::recover_text_panels(&mut regions, &page.cells);
                    // Pictures no longer count as claimers (#165), so the
                    // plain orphan pass places the recognized lines directly.
                    add_orphan_regions(&mut regions, &ocr_cells);
                }
            }
        }

        // Attach the raster so picture regions crop out of it, and record what
        // it is scaled by (cells stay in points).
        page.image = img;
        page.scale = scale;

        let mut table_rows: Vec<Option<docling_pdf::tf_core::TableGrid>> =
            Vec::with_capacity(regions.len());
        for r in &regions {
            if r.label != "table" {
                table_rows.push(None);
                continue;
            }
            // Same bargain as the scanned path: TableFormer costs seconds per
            // region, so skip it wherever the free geometric reconstruction is
            // already dense and well-formed — and, the text layer's boxes
            // being exact, wherever its rows each sit on one text line. A
            // table of wrapped cells does not (#1129): its grid is a row per
            // line, dense and wrong, and only the model sees the two rows.
            let geometric = reconstruct_table(r, &page.cells);
            let trusted = geometric_table_is_reliable(&geometric)
                && geometric_rows_share_lines(r, &page.cells);
            match tf {
                Some(tf) if !trusted => {
                    table_rows.push(
                        crate::tableformer::predict_table_rows(
                            tf,
                            &page.image,
                            [r.l, r.t, r.r, r.b],
                            &page.word_cells,
                        )
                        .await,
                    );
                }
                _ => table_rows.push(None),
            }
        }

        self.out
            .push(assemble_page_with_tables(&page, regions, table_rows));
        Ok(())
    }
}
