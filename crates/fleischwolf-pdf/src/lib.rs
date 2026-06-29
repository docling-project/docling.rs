//! PDF backend for fleischwolf.
//!
//! A port of docling's standard PDF pipeline: pdfium extracts the text layer
//! (cells with bounding boxes) and renders page images; a discriminative ONNX
//! stack (layout detection, table structure, OCR) classifies regions; the cells
//! are assembled in reading order into a [`DoclingDocument`].
//!
//! Current stages: pdfium text-cell extraction + page rendering ([`pdfium_backend`])
//! and the deterministic text/reading-order assembly ([`assemble`]). The layout,
//! table-structure and OCR ONNX stages land behind [`Pipeline`] next.

mod assemble;
mod dp_lines;
pub mod layout;
mod mets;
mod ocr;
pub mod pdfium_backend;
pub mod resample;
pub mod tableformer;

use std::fmt;

use fleischwolf_core::DoclingDocument;

pub use mets::convert_mets_gbs;
pub use pdfium_backend::{PdfDocument, PdfPage, TextCell};

/// Errors from the PDF backend. Detailed and surfaced (never silently skipped).
#[derive(Debug)]
pub enum PdfError {
    /// pdfium failed to bind, open, or read the document.
    Pdfium(String),
    /// The layout ONNX model failed to load or run.
    Layout(String),
    /// The OCR ONNX model failed to load or run.
    Ocr(String),
}

impl fmt::Display for PdfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PdfError::Pdfium(m) => write!(f, "pdf: pdfium error: {m}"),
            PdfError::Layout(m) => write!(f, "pdf: {m}"),
            PdfError::Ocr(m) => write!(f, "pdf: {m}"),
        }
    }
}

impl std::error::Error for PdfError {}

impl From<pdfium_render::prelude::PdfiumError> for PdfError {
    fn from(e: pdfium_render::prelude::PdfiumError) -> Self {
        PdfError::Pdfium(e.to_string())
    }
}

/// Threads ONNX inference may use, capped by `FLEISCHWOLF_PDF_THREADS` if set.
/// Defaults to the available parallelism (ort otherwise picks a low number).
pub(crate) fn intra_threads() -> usize {
    if let Some(n) = std::env::var("FLEISCHWOLF_PDF_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
    {
        return n;
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// A reusable PDF pipeline: the layout model is loaded once and reused across
/// documents; OCR loads lazily the first time a scanned page is seen.
pub struct Pipeline {
    layout: layout::LayoutModel,
    ocr: Option<ocr::OcrModel>,
    /// TableFormer structure model; `None` when its ONNX graphs aren't present
    /// (the assembler then falls back to geometric table reconstruction).
    tables: Option<tableformer::TableFormer>,
}

impl Pipeline {
    /// Load the layout model (the only always-required model). TableFormer loads
    /// if its exported graphs are present, else table regions use the geometric
    /// fallback.
    pub fn new() -> Result<Self, PdfError> {
        Ok(Self {
            layout: layout::LayoutModel::load().map_err(PdfError::Layout)?,
            ocr: None,
            tables: tableformer::TableFormer::load(),
        })
    }

    /// Convert a PDF (bytes) to a [`DoclingDocument`] via the discriminative
    /// pipeline: pdfium text cells (or OCR for scanned pages) + per-page layout
    /// detection, assembled in reading order. Errors are detailed and surfaced.
    pub fn convert(
        &mut self,
        bytes: &[u8],
        password: Option<&str>,
        name: &str,
    ) -> Result<DoclingDocument, PdfError> {
        // Stream pages: render → process → drop one at a time, so a large PDF
        // holds ~one page bitmap (~5 MB) rather than every page at once (which
        // is gigabytes for a multi-thousand-page document and drives the machine
        // into swap).
        let mut doc = DoclingDocument::new(name);
        pdfium_backend::for_each_page(bytes, password, |n, _total, mut page| {
            self.process_one_page(n, &mut page, &mut doc)
        })?;
        assemble::merge_continuations(&mut doc.nodes);
        Ok(doc)
    }

    /// Convert a standalone image (PNG/JPEG/TIFF/WebP/…) as a single page —
    /// docling routes images through the same layout+OCR pipeline as a PDF page.
    pub fn convert_image(&mut self, bytes: &[u8], name: &str) -> Result<DoclingDocument, PdfError> {
        let image = image::load_from_memory(bytes)
            .map_err(|e| PdfError::Pdfium(format!("image: {e}")))?
            .into_rgb8();
        let (w, h) = image.dimensions();
        // The image is its own page rendered at 1 px per "point" (scale 1.0); a
        // standalone image has no text layer, so OCR supplies the cells.
        let page = PdfPage {
            width: w as f32,
            height: h as f32,
            scale: 1.0,
            cells: Vec::new(),
            code_cells: Vec::new(),
            word_cells: Vec::new(),
            image,
            links: Vec::new(),
        };
        self.process_pages(vec![page], name)
    }

    /// Run layout (+ OCR for cell-less pages) and assemble one page into `doc`.
    fn process_one_page(
        &mut self,
        n: usize,
        page: &mut PdfPage,
        doc: &mut DoclingDocument,
    ) -> Result<(), PdfError> {
        let regions = self
            .layout
            .predict(&page.image, page.width, page.height)
            .map_err(|e| PdfError::Layout(format!("page {}: {e}", n + 1)))?;
        // Resolve overlapping detections once, before OCR.
        let regions = assemble::resolve(regions);
        // No text layer → recognise text from the page image via OCR.
        if page.cells.is_empty() {
            if self.ocr.is_none() {
                self.ocr = Some(ocr::OcrModel::load().map_err(PdfError::Ocr)?);
            }
            let cells = self
                .ocr
                .as_mut()
                .unwrap()
                .ocr_page(&page.image, &regions, page.scale)
                .map_err(|e| PdfError::Ocr(format!("page {}: {e}", n + 1)))?;
            page.cells = cells;
        }
        // TableFormer structure per table region (else geometric fallback).
        let mut table_rows: Vec<Option<Vec<Vec<String>>>> = vec![None; regions.len()];
        if let Some(tf) = self.tables.as_mut() {
            for (i, r) in regions.iter().enumerate() {
                if r.label == "table" {
                    table_rows[i] = tf.predict_table_rows(
                        &page.image,
                        page.height,
                        [r.l, r.t, r.r, r.b],
                        &page.word_cells,
                    );
                }
            }
        }
        assemble::assemble_page(page, regions, &table_rows, doc);
        Ok(())
    }

    /// Run layout (+ OCR for cell-less pages) and assemble each already-rendered
    /// page (image / METS inputs, which are small and already materialised).
    fn process_pages(
        &mut self,
        mut pages: Vec<PdfPage>,
        name: &str,
    ) -> Result<DoclingDocument, PdfError> {
        let mut doc = DoclingDocument::new(name);
        for (n, page) in pages.iter_mut().enumerate() {
            self.process_one_page(n, page, &mut doc)?;
        }
        assemble::merge_continuations(&mut doc.nodes);
        Ok(doc)
    }
}

/// Convenience one-shot conversion (loads the pipeline per call). Errors are
/// detailed and surfaced (never silently skipped).
pub fn convert(
    bytes: &[u8],
    password: Option<&str>,
    name: &str,
) -> Result<DoclingDocument, PdfError> {
    Pipeline::new()?.convert(bytes, password, name)
}

/// Convenience one-shot image conversion (loads the pipeline per call).
pub fn convert_image(bytes: &[u8], name: &str) -> Result<DoclingDocument, PdfError> {
    Pipeline::new()?.convert_image(bytes, name)
}

/// Convert pre-segmented pages (image + already-known text cells, e.g. METS/hOCR
/// scans) through the shared layout + assembly pipeline.
pub fn convert_pages(pages: Vec<PdfPage>, name: &str) -> Result<DoclingDocument, PdfError> {
    Pipeline::new()?.process_pages(pages, name)
}
