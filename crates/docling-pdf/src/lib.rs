//! PDF backend for docling.rs.
//!
//! A port of docling's standard PDF pipeline: pdfium extracts the text layer
//! (cells with bounding boxes) and renders page images; a discriminative ONNX
//! stack (layout detection, table structure, OCR) classifies regions; the cells
//! are assembled in reading order into a [`DoclingDocument`].
//!
//! Current stages: pdfium text-cell extraction + page rendering ([`pdfium_backend`])
//! and the deterministic text/reading-order assembly ([`assemble`]). The layout,
//! table-structure and OCR ONNX stages land behind [`Pipeline`] next.

// Without `ml` only the text-layer path runs; the shared assembly/label
// helpers it doesn't exercise stay compiled for API stability (the full
// build still flags genuinely dead code).
#![cfg_attr(not(feature = "ml"), allow(dead_code))]

mod assemble;
mod dp_lines;
#[cfg(feature = "ml")]
pub mod enrich;
// Public so sibling crates (e.g. docling-rag's ONNX embedder) can route their
// own `ort` sessions through the same `DOCLING_RS_EP` selection.
#[cfg(feature = "ml")]
pub mod ep;
pub mod layout;
#[cfg(feature = "ml")]
mod mets;
#[cfg(feature = "ml")]
mod ocr;
#[cfg(feature = "ocr-prep")]
pub mod ocr_prep;
pub mod pdfium_backend;
mod reading_order;
#[cfg(feature = "ml")]
pub mod resample;
#[cfg(feature = "ocr-prep")]
pub mod scanned;
#[cfg(feature = "ml")]
pub mod tableformer;
pub mod textparse;
#[cfg(feature = "ml")]
mod tf_match;
pub mod timing;

#[cfg(feature = "ml")]
use std::collections::BTreeMap;
use std::fmt;
#[cfg(feature = "ml")]
use std::sync::mpsc::{sync_channel, Receiver};
#[cfg(feature = "ml")]
use std::sync::{Arc, Mutex};

use docling_core::DoclingDocument;
#[cfg(feature = "ml")]
use docling_core::Node;

#[cfg(feature = "ml")]
pub use mets::{convert_mets_gbs, convert_mets_gbs_with_options};
#[cfg(feature = "ml")]
pub use ocr::OcrLang;
#[cfg(feature = "ml")]
pub use pdfium_backend::PdfDocument;
pub use pdfium_backend::{PdfPage, TextCell};

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

#[cfg(feature = "ml")]
impl From<pdfium_render::prelude::PdfiumError> for PdfError {
    fn from(e: pdfium_render::prelude::PdfiumError) -> Self {
        PdfError::Pdfium(e.to_string())
    }
}

/// Convert a PDF's **embedded text layer only** — no pdfium, no ONNX, no
/// threads: the pure-Rust content-stream parser ([`textparse`]) feeds the same
/// orphan-region assembly the `no_ocr` pipeline flag uses, so text-layer PDFs
/// come out identical to `--no-ocr` (flat, line-grouped paragraphs in reading
/// order; no headings/lists/tables/pictures, and no hyperlink recovery).
///
/// This is the only conversion entry compiled without the `ml` feature (it is
/// what a wasm32 build runs). A scanned/image-only PDF (no embedded text
/// layer) yields an empty document rather than an error, same as `no_ocr` —
/// callers can detect that and fall back to an OCR-capable build.
pub fn convert_text_layer(bytes: &[u8], name: &str) -> Result<DoclingDocument, PdfError> {
    convert_text_layer_pages(bytes, name, None)
}

/// [`convert_text_layer`] restricted to a **1-based inclusive** page window
/// (issue #80's `--pages`); `None` converts everything. The window is
/// validated the same way as [`Pipeline::pages`]: `first <= last`, 1-based,
/// and it must select at least one existing page.
pub fn convert_text_layer_pages(
    bytes: &[u8],
    name: &str,
    pages: Option<(usize, usize)>,
) -> Result<DoclingDocument, PdfError> {
    if let Some((first, last)) = pages {
        if first == 0 || last < first {
            return Err(PdfError::Pdfium(format!(
                "invalid page range {first}-{last} (pages are 1-based, first <= last)"
            )));
        }
    }
    let mut doc = DoclingDocument::new(name);
    let mut total = 0usize;
    for (i, page) in textparse::pdf_text_pages(bytes).into_iter().enumerate() {
        total += 1;
        if let Some((first, last)) = pages {
            if i + 1 < first || i + 1 > last {
                continue;
            }
        }
        let mut regions = Vec::new();
        assemble::add_orphan_regions(&mut regions, &page.cells);
        let table_rows = vec![None; regions.len()];
        let enrich_out = vec![None; regions.len()];
        let (nodes, links) = assemble::assemble_page(&page, regions, &table_rows, &enrich_out);
        doc.nodes.extend(nodes);
        doc.links.extend(links);
    }
    if let Some((first, last)) = pages {
        if first > total {
            return Err(PdfError::Pdfium(format!(
                "page range {first}-{last} is outside the document ({total} page(s))"
            )));
        }
    }
    assemble::merge_continuations(&mut doc.nodes);
    Ok(doc)
}

/// Threads ONNX inference may use, capped by `DOCLING_RS_PDF_THREADS` if set.
/// Defaults to the available parallelism (ort otherwise picks a low number).
#[cfg(feature = "ml")]
pub(crate) fn intra_threads() -> usize {
    if let Some(n) = std::env::var("DOCLING_RS_PDF_THREADS")
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

#[cfg(feature = "ml")]
/// True when `DOCLING_RS_FP32` (any value but `0`) forces the full-precision
/// models even where an INT8 variant sits next to the fp32 default.
pub(crate) fn fp32_forced() -> bool {
    std::env::var("DOCLING_RS_FP32")
        .map(|v| v != "0")
        .unwrap_or(false)
}

#[cfg(feature = "ml")]
/// Should the int8 model defaults be skipped in favor of fp32? Either the
/// user said so (`DOCLING_RS_FP32`), or a GPU execution provider is selected
/// (#74) — the int8 exports are QDQ graphs calibrated for CPU kernels and
/// only conformance-validated there. An explicit `DOCLING_*_ONNX` path
/// override still wins over this at every call site.
pub(crate) fn prefer_fp32() -> bool {
    fp32_forced() || ep::prefers_fp32()
}

#[cfg(feature = "ml")]
/// Resolve a default (CWD-relative) asset path. If it doesn't exist relative
/// to the current directory, try next to the executable and one level above
/// it (following symlinks — the layout `scripts/install/install.sh` produces:
/// `/usr/local/bin/docling-rs` → `/usr/local/docling.rs/bin/docling-rs`
/// with `models/` and `.pdfium/` in `/usr/local/docling.rs`). Lets an
/// installed binary run from any working directory with no env vars; explicit
/// env overrides never reach this. Returns `rel` unchanged when nothing
/// exists anywhere, so callers' error messages keep the familiar path.
pub(crate) fn resolve_asset(rel: &str) -> String {
    if std::path::Path::new(rel).exists() {
        return rel.to_string();
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
    {
        for base in [Some(dir.as_path()), dir.parent()].into_iter().flatten() {
            let p = base.join(rel);
            if p.exists() {
                return p.to_string_lossy().into_owned();
            }
        }
    }
    rel.to_string()
}

#[cfg(feature = "ml")]
/// Resolve a model path: an explicit env override always wins; otherwise the
/// INT8 variant of the default path when it exists on disk (the quantized
/// models are conformance-validated — see docs/PDF_CONFORMANCE.md — and load/run
/// markedly faster on CPU), unless `DOCLING_RS_FP32` opts back into full
/// precision; else the fp32 default.
pub(crate) fn model_path(env: &str, fp32_default: &str, int8_default: &str) -> String {
    if let Ok(p) = std::env::var(env) {
        return p;
    }
    if !prefer_fp32() {
        let p = resolve_asset(int8_default);
        if std::path::Path::new(&p).exists() {
            return p;
        }
    }
    resolve_asset(fp32_default)
}

/// Decode a standalone image with hard resource limits. A crafted image can
/// declare enormous dimensions in a few-KB file; `image::load_from_memory`
/// then tries to allocate the full pixel buffer (e.g. 60000×60000 → ~10 GB),
/// and allocation failure aborts the whole process, bypassing the per-request
/// panic catch. The 256 MiB alloc / 30000-px caps below turn that into a
/// recoverable decode error instead. `DOCLING_RS_MAX_IMAGE_PIXELS` overrides
/// the per-side pixel cap for the rare legitimately-huge scan.
///
/// Gated on `ml`: the only callers (`convert_image`, the METS backend) are
/// ML-only, and the `image` crate is an `ml`-feature dependency — the
/// text-layer wasm build has neither.
#[cfg(feature = "ml")]
pub(crate) fn decode_image_limited(bytes: &[u8]) -> Result<image::RgbImage, PdfError> {
    let max_side: u32 = std::env::var("DOCLING_RS_MAX_IMAGE_PIXELS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30_000);
    decode_image_with_max_side(bytes, max_side)
}

#[cfg(feature = "ml")]
fn decode_image_with_max_side(bytes: &[u8], max_side: u32) -> Result<image::RgbImage, PdfError> {
    use image::ImageReader;
    use std::io::Cursor;

    let mut limits = image::Limits::default();
    limits.max_image_width = Some(max_side);
    limits.max_image_height = Some(max_side);
    limits.max_alloc = Some(256 * 1024 * 1024);

    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| PdfError::Pdfium(format!("image: {e}")))?;
    reader.limits(limits);
    Ok(reader
        .decode()
        .map_err(|e| PdfError::Pdfium(format!("image: {e}")))?
        .into_rgb8())
}

#[cfg(feature = "ml")]
/// One page's assembled output: typed nodes plus the page's hyperlinks, kept
/// separate so pages processed out of order can be stitched back in page order.
type PageOut = (Vec<Node>, Vec<(String, String)>);

#[cfg(feature = "ml")]
/// The pool-wide TableFormer slot: one instance shared by every worker, loaded
/// lazily on the first table region any worker sees. Tables appear on a
/// minority of pages, so per-worker copies mostly multiplied ~0.4 GB of
/// weights+arenas by the pool size for nothing; a single shared instance keeps
/// the peak flat regardless of pool width, and a table's structure prediction
/// is independent of which worker runs it, so output is byte-identical. The
/// mutex serialises concurrent tables — the shared instance is loaded with the
/// full intra-op thread budget to compensate (one wide TableFormer instead of
/// several narrow ones).
enum TfSlot {
    /// Not attempted yet (no table seen so far).
    Unloaded,
    /// Load attempted, graphs absent — geometric fallback (warned once).
    Missing,
    Ready(tableformer::TableFormer),
}

#[cfg(feature = "ml")]
type SharedTables = Arc<Mutex<TfSlot>>;

#[cfg(feature = "ml")]
/// The same lazy shared-slot pattern for the (rarer still) enrichment models:
/// one instance per pipeline, loaded on the first region that needs it.
enum EnrichSlot<T> {
    Unloaded,
    /// Load attempted, model files absent — enrichment skipped (warned once).
    Missing,
    Ready(T),
}

#[cfg(feature = "ml")]
type SharedClassifier = Arc<Mutex<EnrichSlot<enrich::PictureClassifier>>>;
#[cfg(feature = "ml")]
type SharedCodeFormula = Arc<Mutex<EnrichSlot<enrich::CodeFormula>>>;

#[cfg(feature = "ml")]
/// The opt-in enrichment passes, mirroring docling's `PdfPipelineOptions`
/// flags (`do_picture_classification`, `do_code_enrichment`,
/// `do_formula_enrichment`). All off by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EnrichmentOptions {
    /// Classify each picture with DocumentFigureClassifier (26 classes).
    pub picture_classification: bool,
    /// Rewrite code blocks (and detect their language) with CodeFormulaV2.
    pub code: bool,
    /// Decode display formulas to LaTeX with CodeFormulaV2.
    pub formula: bool,
}

#[cfg(feature = "ml")]
impl EnrichmentOptions {
    fn any(&self) -> bool {
        self.picture_classification || self.code || self.formula
    }
}

#[cfg(feature = "ml")]
/// A self-contained set of the per-page models (layout, OCR). Each parallel
/// page-worker owns its own `Worker` so inference runs concurrently without
/// sharing an ONNX session (`ort`'s `Session::run` is `&mut self`); only the
/// rarely-hit TableFormer is shared (see [`TfSlot`]).
struct Worker {
    /// `None` when `no_ocr` skips layout entirely — no model load, no inference.
    layout: Option<layout::LayoutModel>,
    ocr: Option<ocr::OcrModel>,
    /// Shared TableFormer slot; `None` when `no_table_former`/`no_ocr` skip it.
    tables: Option<SharedTables>,
    /// Shared enrichment slots; `None` unless the corresponding flag is on.
    classifier: Option<SharedClassifier>,
    code_formula: Option<SharedCodeFormula>,
    enrich: EnrichmentOptions,
    /// Skip layout, OCR, and TableFormer; reconstruct text purely from the PDF's
    /// embedded text layer. See [`Pipeline::no_ocr`].
    no_ocr: bool,
    /// Which recognition model [`Self::ocr`] loads. See [`Pipeline::ocr_lang`].
    ocr_lang: ocr::OcrLang,
}

#[cfg(feature = "ml")]
impl Worker {
    fn load(
        intra: usize,
        tables: Option<SharedTables>,
        enrich_slots: (Option<SharedClassifier>, Option<SharedCodeFormula>),
        enrich: EnrichmentOptions,
        no_ocr: bool,
        ocr_lang: ocr::OcrLang,
    ) -> Result<Self, PdfError> {
        Ok(Self {
            layout: if no_ocr {
                None
            } else {
                Some(layout::LayoutModel::load_with(intra).map_err(PdfError::Layout)?)
            },
            ocr: None,
            tables,
            classifier: enrich_slots.0,
            code_formula: enrich_slots.1,
            enrich,
            no_ocr,
            ocr_lang,
        })
    }

    /// Run layout (+ OCR for cell-less pages) + TableFormer and assemble page `n`
    /// into its nodes and links. Pure given the page (mutates only the worker's
    /// lazily-loaded OCR model), so it is safe to run concurrently across pages.
    fn process(&mut self, n: usize, page: &mut PdfPage) -> Result<PageOut, PdfError> {
        if self.no_ocr {
            // Fastest path: no layout/OCR/TableFormer inference at all. The PDF's
            // embedded text cells (if any) become flat, line-grouped paragraphs in
            // reading order via the same orphan-region machinery that normally
            // rescues text the detector missed — here it rescues *all* of it.
            // Pages with no embedded text layer (scanned/image-only) yield nothing;
            // convert those without `no_ocr`.
            let mut regions = Vec::new();
            assemble::add_orphan_regions(&mut regions, &page.cells);
            let table_rows = vec![None; regions.len()];
            let enrich_out = vec![None; regions.len()];
            return Ok(timing::timed("assemble_page", || {
                assemble::assemble_page(page, regions, &table_rows, &enrich_out)
            }));
        }
        let regions = timing::timed("layout.predict", || {
            self.layout
                .as_mut()
                .expect("layout model loaded unless no_ocr")
                .predict(&page.image, page.width, page.height)
        })
        .map_err(|e| PdfError::Layout(format!("page {}: {e}", n + 1)))?;
        self.finish_page(n, page, regions)
    }

    /// Layout-detect a whole batch of pages with one inference call (issue #73),
    /// then run each page's remaining stages (OCR / TableFormer / enrichment /
    /// assembly) per page. Index-aligned with `items`; a layout failure fails
    /// every page in the batch (they shared the one inference call).
    fn process_batch(&mut self, items: &mut [(usize, PdfPage)]) -> Vec<Result<PageOut, PdfError>> {
        if self.no_ocr {
            // No layout model to batch — the text-layer-only path is per page.
            return items
                .iter_mut()
                .map(|(n, page)| {
                    let n = *n;
                    self.process(n, page)
                })
                .collect();
        }
        let inputs: Vec<(&image::RgbImage, f32, f32)> = items
            .iter()
            .map(|(_, page)| (&page.image, page.width, page.height))
            .collect();
        let batched = timing::timed("layout.predict", || {
            self.layout
                .as_mut()
                .expect("layout model loaded unless no_ocr")
                .predict_batch(&inputs)
        });
        match batched {
            Ok(all) => items
                .iter_mut()
                .zip(all)
                .map(|((n, page), regions)| self.finish_page(*n, page, regions))
                .collect(),
            Err(e) => items
                .iter()
                .map(|(n, _)| Err(PdfError::Layout(format!("page {}: {e}", n + 1))))
                .collect(),
        }
    }

    /// Everything after layout detection: per-label confidence thresholds,
    /// overlap resolution, orphan-text recovery, OCR for cell-less pages,
    /// TableFormer, enrichment, and page assembly.
    fn finish_page(
        &mut self,
        n: usize,
        page: &mut PdfPage,
        regions: Vec<layout::Region>,
    ) -> Result<PageOut, PdfError> {
        // docling's LayoutPostprocessor drops each detection below its label's
        // confidence threshold (stricter than the 0.3 base the predictor keeps),
        // before any overlap resolution. This removes the low-confidence tables /
        // pictures / list-items that otherwise double-emit or mis-classify.
        let mut regions = regions;
        regions.retain(|r| r.score >= layout::label_threshold(r.label));
        // Resolve overlapping detections once, before OCR.
        let mut regions = assemble::resolve(regions);
        // Emit text the detector missed as orphan text regions (docling parity).
        assemble::add_orphan_regions(&mut regions, &page.cells);
        // Drop phantom empty low-confidence picture boxes (docling parity).
        assemble::drop_false_pictures(&mut regions, &page.cells, page.width, page.height);
        // A regular region fully inside a surviving table/index/picture is that
        // special's child (a cell / in-figure label), not a separate block —
        // remove it so it isn't emitted twice (docling parity).
        assemble::drop_contained_regulars(&mut regions);
        // No text layer → recognise text from the page image via OCR.
        if page.cells.is_empty() {
            if self.ocr.is_none() {
                self.ocr = Some(ocr::OcrModel::load(self.ocr_lang).map_err(PdfError::Ocr)?);
            }
            let cells = timing::timed("ocr.page", || {
                self.ocr
                    .as_mut()
                    .unwrap()
                    .ocr_page(&page.image, &regions, page.scale)
            })
            .map_err(|e| PdfError::Ocr(format!("page {}: {e}", n + 1)))?;
            page.cells = cells;
        }
        // TableFormer structure per table region (else geometric fallback). The
        // shared slot is only locked (and lazily loaded) when the page actually
        // has a table, so table-free documents never pay for TableFormer at all.
        let mut table_rows: Vec<Option<Vec<Vec<String>>>> = vec![None; regions.len()];
        if let Some(slot) = self.tables.as_ref() {
            if regions.iter().any(|r| assemble::is_table_like(r.label)) {
                timing::timed("tableformer", || {
                    let mut guard = slot.lock().unwrap();
                    if matches!(*guard, TfSlot::Unloaded) {
                        // Full intra-op width: tables serialise on this mutex, so
                        // the one instance gets the whole thread budget.
                        *guard = match tableformer::TableFormer::load_with(intra_threads()) {
                            Some(tf) => TfSlot::Ready(tf),
                            None => TfSlot::Missing,
                        };
                    }
                    if let TfSlot::Ready(tf) = &mut *guard {
                        for (i, r) in regions.iter().enumerate() {
                            if assemble::is_table_like(r.label) {
                                table_rows[i] = tf.predict_table_rows(
                                    &page.image,
                                    [r.l, r.t, r.r, r.b],
                                    &page.word_cells,
                                );
                            }
                        }
                    }
                });
            }
        }
        // Enrichment passes (opt-in): DocumentPictureClassifier over picture
        // regions, CodeFormulaV2 over code/formula regions. Same shared-slot
        // shape as TableFormer — one lazily-loaded instance per pipeline, only
        // ever locked when a page actually has a matching region.
        let mut enrich_out: Vec<Option<assemble::Enrichment>> = vec![None; regions.len()];
        if let Some(slot) = self.classifier.as_ref() {
            if regions.iter().any(|r| r.label == "picture") {
                timing::timed("picture_classifier", || {
                    let mut guard = slot.lock().unwrap();
                    if matches!(*guard, EnrichSlot::Unloaded) {
                        *guard = match enrich::PictureClassifier::load_with(intra_threads()) {
                            Some(m) => EnrichSlot::Ready(m),
                            None => EnrichSlot::Missing,
                        };
                    }
                    if let EnrichSlot::Ready(model) = &mut *guard {
                        for (i, r) in regions.iter().enumerate() {
                            if r.label != "picture" {
                                continue;
                            }
                            let Some(crop) = assemble::crop_region_scaled(
                                page,
                                [r.l, r.t, r.r, r.b],
                                enrich::CLASSIFIER_SCALE,
                            ) else {
                                continue;
                            };
                            match model.classify(&crop) {
                                Ok(classes) => {
                                    enrich_out[i] =
                                        Some(assemble::Enrichment::PictureClasses(classes));
                                }
                                Err(e) => eprintln!("docling-pdf: page {}: {e}", n + 1),
                            }
                        }
                    }
                });
            }
        }
        if let Some(slot) = self.code_formula.as_ref() {
            let wants = |label: &str| {
                (label == "code" && self.enrich.code) || (label == "formula" && self.enrich.formula)
            };
            if regions.iter().any(|r| wants(r.label)) {
                timing::timed("code_formula", || {
                    let mut guard = slot.lock().unwrap();
                    if matches!(*guard, EnrichSlot::Unloaded) {
                        *guard = match enrich::CodeFormula::load_with(intra_threads()) {
                            Some(m) => EnrichSlot::Ready(m),
                            None => EnrichSlot::Missing,
                        };
                    }
                    if let EnrichSlot::Ready(model) = &mut *guard {
                        for (i, r) in regions.iter().enumerate() {
                            if !wants(r.label) {
                                continue;
                            }
                            // docling crops the postprocessed cluster box — the
                            // union of the region's text cells, not the raw
                            // detector box — expanded by 18% per side, at
                            // ~120 dpi.
                            let [bl, bt, br, bb] = assemble::region_cell_bbox(r, &page.cells)
                                .unwrap_or([r.l, r.t, r.r, r.b]);
                            let (w, h) = (br - bl, bb - bt);
                            let ex = enrich::CODE_FORMULA_EXPANSION;
                            let bbox = [bl - w * ex, bt - h * ex, br + w * ex, bb + h * ex];
                            let Some(crop) = assemble::crop_region_scaled(
                                page,
                                bbox,
                                enrich::CODE_FORMULA_SCALE,
                            ) else {
                                continue;
                            };
                            let kind = if r.label == "code" {
                                enrich::CodeFormulaKind::Code
                            } else {
                                enrich::CodeFormulaKind::Formula
                            };
                            match model.predict(&crop, kind) {
                                Ok(text) => {
                                    enrich_out[i] = Some(match kind {
                                        enrich::CodeFormulaKind::Code => {
                                            let (code, language) =
                                                enrich::extract_code_language(&text);
                                            assemble::Enrichment::Code {
                                                language,
                                                text: code,
                                            }
                                        }
                                        enrich::CodeFormulaKind::Formula => {
                                            assemble::Enrichment::Formula { latex: text }
                                        }
                                    });
                                }
                                Err(e) => eprintln!("docling-pdf: page {}: {e}", n + 1),
                            }
                        }
                    }
                });
            }
        }
        Ok(timing::timed("assemble_page", || {
            assemble::assemble_page(page, regions, &table_rows, &enrich_out)
        }))
    }
}

#[cfg(feature = "ml")]
/// Per-worker ONNX intra-op threads. The layout model is memory-bandwidth bound,
/// so on a typical machine two threads per worker (sharing one in-cache copy of
/// the weights) extracts more throughput than one fat model or many single-thread
/// workers. `DOCLING_RS_PDF_INTRA` overrides for per-machine tuning.
fn pdf_intra() -> usize {
    if let Some(n) = std::env::var("DOCLING_RS_PDF_INTRA")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
    {
        return n;
    }
    if intra_threads() >= 2 {
        2
    } else {
        1
    }
}

#[cfg(feature = "ml")]
/// How many page-workers to spin up for a multi-page PDF. `DOCLING_RS_PDF_WORKERS`
/// overrides; otherwise size the pool so `workers × intra ≈ cores`, capped at 4 so
/// a worst-case pool holds a bounded amount of model memory (~0.4 GB per worker)
/// and does not oversaturate the memory bus with model-weight traffic.
fn pdf_worker_count() -> usize {
    if let Some(n) = std::env::var("DOCLING_RS_PDF_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
    {
        return n;
    }
    (intra_threads() / pdf_intra()).clamp(1, 4)
}

#[cfg(feature = "ml")]
/// Max pages a worker layout-detects with one batched inference call (issue
/// #73). Workers drain the work channel opportunistically up to this size —
/// whatever is already rendered gets batched, so batching never *waits* for
/// pages and adds no latency when rendering is the bottleneck.
///
/// Default: 4 on 8+ cores, 1 (per-page) below. Measured on a 4-core box the
/// batch only adds cache pressure and costs pipeline overlap (2 workers × 2
/// threads: 8.1 s/conv at batch=1 vs 9.3 s at batch=4 on the 9-page
/// 2206.01062 fixture); the single-session amortization it buys needs the
/// wider thread budget of a many-core machine. Output is bit-identical at
/// every batch size, so this is purely a throughput knob.
/// `DOCLING_RS_PDF_LAYOUT_BATCH` overrides; `1` restores per-page inference.
fn pdf_layout_batch() -> usize {
    std::env::var("DOCLING_RS_PDF_LAYOUT_BATCH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| if intra_threads() >= 8 { 4 } else { 1 })
}

#[cfg(feature = "ml")]
/// Minimum page count before a PDF is worth the parallel worker pool. Below this,
/// the serial primary (running its model on every core) is faster than fanning out
/// — the helper pool's one-time model-load cost only pays off once enough pages
/// share it. `DOCLING_RS_PDF_PARALLEL_MIN` overrides.
fn pdf_parallel_min() -> usize {
    std::env::var("DOCLING_RS_PDF_PARALLEL_MIN")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(6)
}

#[cfg(feature = "ml")]
/// A reusable PDF pipeline. The **primary** worker runs its models on every core,
/// so a single-page / small / image / METS input is converted at full intra-op
/// speed with no pool to load. A document with enough pages instead fans out
/// across a **pool** of narrower workers processed concurrently. Both load lazily
/// and are cached for reuse, so a one-shot conversion only pays for what it uses.
pub struct Pipeline {
    /// Full-intra worker for the serial path; loaded on first serial use.
    primary: Option<Worker>,
    /// Narrower workers (≈cores/`target_workers` threads each) for the parallel
    /// path; loaded on first multi-page use and cached.
    pool: Vec<Worker>,
    /// The single TableFormer instance every worker shares (see [`TfSlot`]).
    tables: SharedTables,
    /// The shared enrichment-model slots (same pattern as [`TfSlot`]).
    classifier: SharedClassifier,
    code_formula: SharedCodeFormula,
    /// Desired pool size for multi-page documents.
    target_workers: usize,
    /// Page count at/above which the parallel pool is worth its load cost.
    parallel_min: usize,
    /// Skip loading/running TableFormer; table regions fall back to geometric
    /// reconstruction. See [`Pipeline::no_table_former`].
    no_table_former: bool,
    /// Skip layout, OCR, and TableFormer entirely. See [`Pipeline::no_ocr`].
    no_ocr: bool,
    /// Opt-in enrichment passes. See [`Pipeline::enrichments`].
    enrich: EnrichmentOptions,
    /// 1-based inclusive page window to convert. See [`Pipeline::pages`].
    page_range: Option<(usize, usize)>,
    /// OCR recognition language. See [`Pipeline::ocr_lang`].
    ocr_lang: ocr::OcrLang,
}

#[cfg(feature = "ml")]
impl Pipeline {
    /// Construct the pipeline. Models load lazily on first use (full-intra primary
    /// for serial inputs, the helper pool for multi-page PDFs), so nothing is
    /// loaded that a given document doesn't need.
    pub fn new() -> Result<Self, PdfError> {
        Ok(Self {
            primary: None,
            pool: Vec::new(),
            tables: Arc::new(Mutex::new(TfSlot::Unloaded)),
            classifier: Arc::new(Mutex::new(EnrichSlot::Unloaded)),
            code_formula: Arc::new(Mutex::new(EnrichSlot::Unloaded)),
            target_workers: pdf_worker_count(),
            parallel_min: pdf_parallel_min(),
            no_table_former: false,
            no_ocr: false,
            enrich: EnrichmentOptions::default(),
            page_range: None,
            ocr_lang: ocr::OcrLang::from_env(),
        })
    }

    /// Convert only pages `first..=last` (**1-based**, like the page numbers a
    /// PDF viewer shows — issue #80's `--pages A-B`). Out-of-range pages are
    /// skipped before rasterization, so the cost is proportional to the window,
    /// not the document. `last` past the end of the document clamps; a window
    /// that selects no pages at all (`first` beyond the last page) is an error
    /// at convert time. `None` (the default) converts everything.
    pub fn pages(mut self, range: Option<(usize, usize)>) -> Self {
        self.page_range = range;
        self
    }

    /// In-place variant of [`pages`](Self::pages) for a long-lived pipeline
    /// (e.g. docling-serve's warm instance) that applies a per-request window
    /// without rebuilding — unlike the model switches, the window is pure
    /// configuration. Set it before every conversion; it stays until changed.
    pub fn set_pages(&mut self, range: Option<(usize, usize)>) {
        self.page_range = range;
    }

    /// OCR recognition language (see [`OcrLang`]): English by default, `ch`
    /// for the multilingual docling-conformance model. `None` keeps the
    /// process default (`DOCLING_RS_OCR_LANG`, else English). Set before the
    /// first conversion; for a warm pipeline use
    /// [`set_ocr_lang`](Self::set_ocr_lang).
    pub fn ocr_lang(mut self, lang: Option<ocr::OcrLang>) -> Self {
        self.set_ocr_lang(lang);
        self
    }

    /// In-place variant of [`ocr_lang`](Self::ocr_lang) for a long-lived
    /// pipeline (docling-serve's warm instance). Unlike the page window this
    /// is a *model* switch: any worker whose cached recognition model was
    /// loaded for a different language drops it, to be lazily reloaded on the
    /// next OCR-needing page (cheap — the rec models are ~10 MB).
    pub fn set_ocr_lang(&mut self, lang: Option<ocr::OcrLang>) {
        let lang = lang.unwrap_or_else(ocr::OcrLang::from_env);
        self.ocr_lang = lang;
        for worker in self.primary.iter_mut().chain(self.pool.iter_mut()) {
            if worker.ocr_lang != lang {
                worker.ocr_lang = lang;
                worker.ocr = None;
            }
        }
    }

    /// Resolve the configured 1-based window against a page count into the
    /// 0-based inclusive form the backend walks, validating it selects at
    /// least one existing page.
    fn resolve_range(&self, total: usize) -> Result<Option<(usize, usize)>, PdfError> {
        let Some((first, last)) = self.page_range else {
            return Ok(None);
        };
        if first == 0 || last < first {
            return Err(PdfError::Pdfium(format!(
                "invalid page range {first}-{last} (pages are 1-based, first <= last)"
            )));
        }
        if first > total {
            return Err(PdfError::Pdfium(format!(
                "page range {first}-{last} is outside the document ({total} page(s))"
            )));
        }
        Ok(Some((first - 1, last.min(total) - 1)))
    }

    /// Enable the opt-in enrichment passes (docling's
    /// `do_picture_classification` / `do_code_enrichment` /
    /// `do_formula_enrichment`). Each enabled pass lazily loads its model on
    /// the first matching region; a missing model warns once and is skipped.
    /// Set before the first conversion (no effect on already-loaded workers).
    pub fn enrichments(mut self, opts: EnrichmentOptions) -> Self {
        self.enrich = opts;
        self
    }

    /// Skip loading and running the TableFormer table-structure model. Table
    /// regions still get emitted, but reconstructed geometrically from cell
    /// positions instead of via the ONNX model's predicted structure — faster
    /// (no model load, no per-table inference) at the cost of table fidelity.
    /// No effect if a worker is already loaded; set this before the first
    /// conversion.
    pub fn no_table_former(mut self, disable: bool) -> Self {
        self.no_table_former = disable;
        self
    }

    /// Skip layout detection, OCR, and TableFormer entirely — no model load, no
    /// inference of any kind. The PDF's embedded text cells are grouped by line
    /// and emitted as plain paragraphs in reading order: no headings, lists,
    /// tables, code blocks, or pictures, since that structure comes from the
    /// layout model. The fastest possible PDF path, but pages with no embedded
    /// text layer (scanned/image-only PDFs) yield no text at all — convert those
    /// without this flag. Implies `no_table_former`. No effect if a worker is
    /// already loaded; set this before the first conversion.
    pub fn no_ocr(mut self, disable: bool) -> Self {
        self.no_ocr = disable;
        self
    }

    /// The shared TableFormer slot handed to each worker, or `None` when the
    /// pipeline options skip TableFormer entirely.
    fn tables_slot(&self) -> Option<SharedTables> {
        if self.no_table_former || self.no_ocr {
            None
        } else {
            Some(Arc::clone(&self.tables))
        }
    }

    /// The shared enrichment slots for a worker (`None` per model unless its
    /// flag is on; `no_ocr` skips layout, so there are no regions to enrich).
    fn enrich_slots(&self) -> (Option<SharedClassifier>, Option<SharedCodeFormula>) {
        if self.no_ocr || !self.enrich.any() {
            return (None, None);
        }
        (
            self.enrich
                .picture_classification
                .then(|| Arc::clone(&self.classifier)),
            (self.enrich.code || self.enrich.formula).then(|| Arc::clone(&self.code_formula)),
        )
    }

    /// Eagerly load the models (the full-intra serial worker: layout + OCR, and
    /// the shared TableFormer unless disabled) so the first conversion doesn't pay
    /// the load cost. Idempotent; respects `no_ocr` / `no_table_former` (with
    /// `no_ocr` there is nothing to load). The docling.rs analogue of docling's
    /// `DocumentConverter.initialize_pipeline`.
    pub fn warm_up(&mut self) -> Result<(), PdfError> {
        self.primary()?;
        Ok(())
    }

    /// The full-intra serial worker, loaded on first use.
    fn primary(&mut self) -> Result<&mut Worker, PdfError> {
        if self.primary.is_none() {
            self.primary = Some(Worker::load(
                intra_threads(),
                self.tables_slot(),
                self.enrich_slots(),
                self.enrich,
                self.no_ocr,
                self.ocr_lang,
            )?);
        }
        Ok(self.primary.as_mut().unwrap())
    }

    /// Convert a PDF (bytes) to a [`DoclingDocument`]. A document with fewer than
    /// `parallel_min` pages (or a pool size of 1) streams through the full-intra
    /// primary; a larger one renders on this thread (pdfium is not thread-safe) and
    /// fans the pages out across the worker pool, reassembled in page order so the
    /// output is byte-identical to the serial path.
    pub fn convert(
        &mut self,
        bytes: &[u8],
        password: Option<&str>,
        name: &str,
    ) -> Result<DoclingDocument, PdfError> {
        let pages = pdfium_backend::page_count(bytes, password)?;
        let range = self.resolve_range(pages)?;
        // Serial vs parallel is decided by the pages actually converted: a
        // 3-page window over a 500-page PDF should not pay the pool load.
        let selected = range.map_or(pages, |(a, b)| b - a + 1);
        let doc = if self.target_workers >= 2 && selected >= self.parallel_min {
            self.convert_parallel(bytes, password, name, range)?
        } else {
            self.convert_serial(bytes, password, name, range)?
        };
        timing::report();
        Ok(doc)
    }

    /// Stream pages one at a time through the primary worker — render → process →
    /// drop — so the document holds ~one page bitmap (~5 MB) at a time.
    fn convert_serial(
        &mut self,
        bytes: &[u8],
        password: Option<&str>,
        name: &str,
        range: Option<(usize, usize)>,
    ) -> Result<DoclingDocument, PdfError> {
        let mut doc = DoclingDocument::new(name);
        let render_image = !self.no_ocr;
        let worker = self.primary()?;
        pdfium_backend::for_each_page(
            bytes,
            password,
            render_image,
            range,
            |n, _total, mut page| {
                let (nodes, links) = worker.process(n, &mut page)?;
                doc.nodes.extend(nodes);
                doc.links.extend(links);
                Ok::<(), PdfError>(())
            },
        )?;
        assemble::merge_continuations(&mut doc.nodes);
        Ok(doc)
    }

    /// Render pages serially on this thread (pdfium) and process them in parallel
    /// across the worker pool. A bounded channel applies backpressure so only a
    /// handful of page bitmaps are resident at once; results carry their page
    /// index and are reassembled in order, so the output is byte-identical to the
    /// serial path.
    fn convert_parallel(
        &mut self,
        bytes: &[u8],
        password: Option<&str>,
        name: &str,
        range: Option<(usize, usize)>,
    ) -> Result<DoclingDocument, PdfError> {
        self.ensure_pool()?;
        let n_workers = self.pool.len();
        let render_image = !self.no_ocr;
        let layout_batch = pdf_layout_batch();
        // Bound sized so every worker can accumulate a full layout batch while
        // rendering stays ahead (and never below the pre-#73 render-ahead of
        // two pages per worker); still a hard cap on resident page bitmaps.
        let (work_tx, work_rx) = sync_channel::<(usize, PdfPage)>(n_workers * layout_batch.max(2));
        let work_rx: Arc<Mutex<Receiver<(usize, PdfPage)>>> = Arc::new(Mutex::new(work_rx));
        let results: Arc<Mutex<Vec<(usize, PageOut)>>> = Arc::new(Mutex::new(Vec::new()));
        let first_err: Arc<Mutex<Option<PdfError>>> = Arc::new(Mutex::new(None));

        // Move the pool into the scope so each worker gets an exclusive `&mut`.
        let mut workers = std::mem::take(&mut self.pool);
        std::thread::scope(|s| {
            for worker in workers.iter_mut() {
                let work_rx = Arc::clone(&work_rx);
                let results = Arc::clone(&results);
                let first_err = Arc::clone(&first_err);
                s.spawn(move || loop {
                    // Hold the receiver lock only for the recv (plus a non-blocking
                    // drain up to the layout batch size); release before the (long)
                    // per-page work so other workers can pull concurrently.
                    let mut batch = Vec::new();
                    {
                        let rx = work_rx.lock().unwrap();
                        match rx.recv() {
                            Ok(item) => {
                                batch.push(item);
                                while batch.len() < layout_batch {
                                    match rx.try_recv() {
                                        Ok(item) => batch.push(item),
                                        Err(_) => break,
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let outs = worker.process_batch(&mut batch);
                    for ((idx, _), out) in batch.iter().zip(outs) {
                        match out {
                            Ok(out) => results.lock().unwrap().push((*idx, out)),
                            Err(e) => {
                                let mut slot = first_err.lock().unwrap();
                                if slot.is_none() {
                                    *slot = Some(e);
                                }
                            }
                        }
                    }
                });
            }
            // Render on this thread and feed the workers; backpressure blocks here
            // when the channel is full. Dropping `work_tx` afterwards signals the
            // workers (recv → Err) to finish.
            let render = pdfium_backend::for_each_page(
                bytes,
                password,
                render_image,
                range,
                |i, _total, page| {
                    work_tx
                        .send((i, page))
                        .map_err(|_| PdfError::Pdfium("page-worker channel closed".into()))
                },
            );
            drop(work_tx);
            if let Err(e) = render {
                let mut slot = first_err.lock().unwrap();
                if slot.is_none() {
                    *slot = Some(e);
                }
            }
        });
        // Threads have joined; restore the pool for the next conversion.
        self.pool = workers;

        if let Some(e) = first_err.lock().unwrap().take() {
            return Err(e);
        }
        let mut results = Arc::try_unwrap(results)
            .unwrap_or_else(|arc| Mutex::new(arc.lock().unwrap().clone()))
            .into_inner()
            .unwrap();
        results.sort_by_key(|(idx, _)| *idx);
        let mut doc = DoclingDocument::new(name);
        for (_, (nodes, links)) in results {
            doc.nodes.extend(nodes);
            doc.links.extend(links);
        }
        assemble::merge_continuations(&mut doc.nodes);
        Ok(doc)
    }

    /// Convert a PDF in **streaming** mode: `emit` is called with each finalized,
    /// in-document-order batch of nodes (and that span's recovered links) as pages
    /// complete, so a caller can serialize Markdown page by page instead of waiting
    /// for the whole document. The batches are exactly the buffered [`convert`]'s
    /// nodes, split at safe block boundaries by [`assemble::StreamAssembler`] — the
    /// parallel path reorders pages back into document order before emitting, so
    /// the output is identical regardless of worker scheduling.
    ///
    /// `emit` runs on the calling thread (never a worker), so it needn't be `Send`
    /// and its backpressure throttles the whole pipeline. Returning `Err` from
    /// `emit` aborts the conversion with that error.
    pub fn convert_streaming<F>(
        &mut self,
        bytes: &[u8],
        password: Option<&str>,
        name: &str,
        emit: F,
    ) -> Result<(), PdfError>
    where
        F: FnMut(Vec<Node>, Vec<(String, String)>) -> Result<(), PdfError>,
    {
        let _ = name; // page nodes carry no name; the caller owns the document name.
        let pages = pdfium_backend::page_count(bytes, password)?;
        let range = self.resolve_range(pages)?;
        let selected = range.map_or(pages, |(a, b)| b - a + 1);
        let r = if self.target_workers >= 2 && selected >= self.parallel_min {
            self.convert_streaming_parallel(bytes, password, range, emit)
        } else {
            self.convert_streaming_serial(bytes, password, range, emit)
        };
        timing::report();
        r
    }

    /// Serial streaming: render → process → emit, one page at a time, holding back
    /// only the tail that might still merge into the next page.
    fn convert_streaming_serial<F>(
        &mut self,
        bytes: &[u8],
        password: Option<&str>,
        range: Option<(usize, usize)>,
        mut emit: F,
    ) -> Result<(), PdfError>
    where
        F: FnMut(Vec<Node>, Vec<(String, String)>) -> Result<(), PdfError>,
    {
        let mut asm = assemble::StreamAssembler::new();
        let render_image = !self.no_ocr;
        let worker = self.primary()?;
        pdfium_backend::for_each_page(
            bytes,
            password,
            render_image,
            range,
            |n, _total, mut page| {
                let (nodes, links) = worker.process(n, &mut page)?;
                emit(asm.push(nodes), links)
            },
        )?;
        emit(asm.finish(), Vec::new())
    }

    /// Parallel streaming: pages render serially on a dedicated thread (pdfium is
    /// not thread-safe) and process across the worker pool; results carry their
    /// page index and are reordered on the calling thread into a
    /// [`assemble::StreamAssembler`], which emits each page in document order as
    /// soon as its predecessors have arrived. Bounded channels keep only a handful
    /// of pages resident and let `emit`'s backpressure reach the renderer.
    fn convert_streaming_parallel<F>(
        &mut self,
        bytes: &[u8],
        password: Option<&str>,
        range: Option<(usize, usize)>,
        mut emit: F,
    ) -> Result<(), PdfError>
    where
        F: FnMut(Vec<Node>, Vec<(String, String)>) -> Result<(), PdfError>,
    {
        self.ensure_pool()?;
        let n_workers = self.pool.len();
        let render_image = !self.no_ocr;
        let layout_batch = pdf_layout_batch();
        // Bound sized so every worker can accumulate a full layout batch while
        // rendering stays ahead (and never below the pre-#73 render-ahead of
        // two pages per worker); still a hard cap on resident page bitmaps.
        let (work_tx, work_rx) = sync_channel::<(usize, PdfPage)>(n_workers * layout_batch.max(2));
        let work_rx: Arc<Mutex<Receiver<(usize, PdfPage)>>> = Arc::new(Mutex::new(work_rx));
        // Workers and the renderer report here; the calling thread drains it in
        // page order. Bounded so workers block (bounding resident bitmaps) when the
        // consumer falls behind.
        let (res_tx, res_rx) = sync_channel::<Result<(usize, PageOut), PdfError>>(n_workers * 2);

        let mut workers = std::mem::take(&mut self.pool);
        let mut asm = assemble::StreamAssembler::new();
        let mut first_err: Option<PdfError> = None;

        std::thread::scope(|s| {
            // Workers: pull a batch of pages (whatever is already rendered, up
            // to the layout batch size), process it, report (index-tagged)
            // results.
            for worker in workers.iter_mut() {
                let work_rx = Arc::clone(&work_rx);
                let res_tx = res_tx.clone();
                s.spawn(move || 'outer: loop {
                    let mut batch = Vec::new();
                    {
                        let rx = work_rx.lock().unwrap();
                        match rx.recv() {
                            Ok(item) => {
                                batch.push(item);
                                while batch.len() < layout_batch {
                                    match rx.try_recv() {
                                        Ok(item) => batch.push(item),
                                        Err(_) => break,
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let outs = worker.process_batch(&mut batch);
                    for ((idx, _), out) in batch.iter().zip(outs) {
                        if res_tx.send(out.map(|o| (*idx, o))).is_err() {
                            break 'outer; // consumer gone
                        }
                    }
                });
            }
            // Renderer: feed pages to the pool on its own thread (pdfium stays on a
            // single thread); report a render error through the same channel.
            {
                let res_tx = res_tx.clone();
                s.spawn(move || {
                    let render = pdfium_backend::for_each_page(
                        bytes,
                        password,
                        render_image,
                        range,
                        |i, _total, page| {
                            work_tx
                                .send((i, page))
                                .map_err(|_| PdfError::Pdfium("page-worker channel closed".into()))
                        },
                    );
                    drop(work_tx); // signal workers to finish
                    if let Err(e) = render {
                        let _ = res_tx.send(Err(e));
                    }
                });
            }
            // Drop our own sender so the channel closes once the threads finish.
            drop(res_tx);

            // Collector (this thread): reorder into document order and emit.
            // With a page window, indices start at the window's first page.
            let mut buffer: BTreeMap<usize, PageOut> = BTreeMap::new();
            let mut next = range.map_or(0, |(first, _)| first);
            for msg in res_rx.iter() {
                match msg {
                    Err(e) => {
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                    Ok((idx, out)) => {
                        buffer.insert(idx, out);
                        if first_err.is_some() {
                            continue; // keep draining so the threads can exit
                        }
                        while let Some((nodes, links)) = buffer.remove(&next) {
                            if let Err(e) = emit(asm.push(nodes), links) {
                                first_err = Some(e);
                                break;
                            }
                            next += 1;
                        }
                    }
                }
            }
        });
        // Threads have joined; restore the pool for the next conversion.
        self.pool = workers;

        if let Some(e) = first_err {
            return Err(e);
        }
        emit(asm.finish(), Vec::new())
    }

    /// Lazily grow the pool to `target_workers`, loading the new workers
    /// concurrently (model load is mostly I/O + mmap, so N loads overlap to roughly
    /// one load's wall-time). Cached for reuse across documents.
    fn ensure_pool(&mut self) -> Result<(), PdfError> {
        let need = self.target_workers.saturating_sub(self.pool.len());
        if need == 0 {
            return Ok(());
        }
        let intra = pdf_intra();
        let no_ocr = self.no_ocr;
        let ocr_lang = self.ocr_lang;
        let enrich = self.enrich;
        let tables = self.tables_slot();
        let enrich_slots = self.enrich_slots();
        let loaded: Vec<Result<Worker, PdfError>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..need)
                .map(|_| {
                    let tables = tables.clone();
                    let enrich_slots = enrich_slots.clone();
                    s.spawn(move || {
                        Worker::load(intra, tables, enrich_slots, enrich, no_ocr, ocr_lang)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for w in loaded {
            self.pool.push(w?);
        }
        Ok(())
    }

    /// Convert a standalone image (PNG/JPEG/TIFF/WebP/…) as a single page —
    /// docling routes images through the same layout+OCR pipeline as a PDF page.
    pub fn convert_image(&mut self, bytes: &[u8], name: &str) -> Result<DoclingDocument, PdfError> {
        let image = decode_image_limited(bytes)?;
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

    /// Run layout (+ OCR for cell-less pages) and assemble each already-rendered
    /// page (image / METS inputs, which are small and already materialised).
    fn process_pages(
        &mut self,
        mut pages: Vec<PdfPage>,
        name: &str,
    ) -> Result<DoclingDocument, PdfError> {
        let mut doc = DoclingDocument::new(name);
        let worker = self.primary()?;
        for (n, page) in pages.iter_mut().enumerate() {
            let (nodes, links) = worker.process(n, page)?;
            doc.nodes.extend(nodes);
            doc.links.extend(links);
        }
        assemble::merge_continuations(&mut doc.nodes);
        Ok(doc)
    }
}

#[cfg(feature = "ml")]
/// Convenience one-shot conversion (loads the pipeline per call). Errors are
/// detailed and surfaced (never silently skipped).
pub fn convert(
    bytes: &[u8],
    password: Option<&str>,
    name: &str,
) -> Result<DoclingDocument, PdfError> {
    convert_with_options(
        bytes,
        password,
        name,
        false,
        false,
        EnrichmentOptions::default(),
        None,
        None,
    )
}

#[cfg(feature = "ml")]
/// Like [`convert`], but optionally skips loading/running TableFormer (see
/// [`Pipeline::no_table_former`]) and/or layout+OCR+TableFormer entirely (see
/// [`Pipeline::no_ocr`]), and/or enables the enrichment passes (see
/// [`Pipeline::enrichments`]).
// One positional per pipeline switch mirrors the Pipeline builder; growing
// past clippy's arity cap is the price of keeping this one-shot signature
// stable-ish instead of churning callers into an options struct mid-series.
#[allow(clippy::too_many_arguments)]
pub fn convert_with_options(
    bytes: &[u8],
    password: Option<&str>,
    name: &str,
    no_table_former: bool,
    no_ocr: bool,
    enrich: EnrichmentOptions,
    pages: Option<(usize, usize)>,
    ocr_lang: Option<OcrLang>,
) -> Result<DoclingDocument, PdfError> {
    Pipeline::new()?
        .no_table_former(no_table_former)
        .no_ocr(no_ocr)
        .enrichments(enrich)
        .pages(pages)
        .ocr_lang(ocr_lang)
        .convert(bytes, password, name)
}

#[cfg(feature = "ml")]
/// Convenience one-shot image conversion (loads the pipeline per call).
pub fn convert_image(bytes: &[u8], name: &str) -> Result<DoclingDocument, PdfError> {
    convert_image_with_options(
        bytes,
        name,
        false,
        false,
        EnrichmentOptions::default(),
        None,
    )
}

#[cfg(feature = "ml")]
/// Like [`convert_image`], but optionally skips loading/running TableFormer (see
/// [`Pipeline::no_table_former`]) and/or layout+OCR+TableFormer entirely (see
/// [`Pipeline::no_ocr`]), and/or enables the enrichment passes.
pub fn convert_image_with_options(
    bytes: &[u8],
    name: &str,
    no_table_former: bool,
    no_ocr: bool,
    enrich: EnrichmentOptions,
    ocr_lang: Option<OcrLang>,
) -> Result<DoclingDocument, PdfError> {
    Pipeline::new()?
        .no_table_former(no_table_former)
        .no_ocr(no_ocr)
        .enrichments(enrich)
        .ocr_lang(ocr_lang)
        .convert_image(bytes, name)
}

#[cfg(feature = "ml")]
/// Convert pre-segmented pages (image + already-known text cells, e.g. METS/hOCR
/// scans) through the shared layout + assembly pipeline.
pub fn convert_pages(pages: Vec<PdfPage>, name: &str) -> Result<DoclingDocument, PdfError> {
    convert_pages_with_options(pages, name, false, false, EnrichmentOptions::default())
}

#[cfg(feature = "ml")]
/// Like [`convert_pages`], but optionally skips loading/running TableFormer (see
/// [`Pipeline::no_table_former`]) and/or layout+OCR+TableFormer entirely (see
/// [`Pipeline::no_ocr`]), and/or enables the enrichment passes.
pub fn convert_pages_with_options(
    pages: Vec<PdfPage>,
    name: &str,
    no_table_former: bool,
    no_ocr: bool,
    enrich: EnrichmentOptions,
) -> Result<DoclingDocument, PdfError> {
    Pipeline::new()?
        .no_table_former(no_table_former)
        .no_ocr(no_ocr)
        .enrichments(enrich)
        .process_pages(pages, name)
}

#[cfg(feature = "ml")]
#[cfg(all(test, feature = "ml"))]
mod image_limit_tests {
    use super::decode_image_with_max_side;

    /// A small valid PNG encoded via the `image` crate (robust vs. a hand-rolled
    /// byte literal).
    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        use std::io::Cursor;
        let img = image::RgbImage::new(w, h);
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn normal_image_decodes_under_the_cap() {
        let img = decode_image_with_max_side(&png_bytes(8, 8), 30_000).expect("8x8 decodes");
        assert_eq!(img.dimensions(), (8, 8));
    }

    #[test]
    fn dimensions_over_the_cap_are_rejected_not_aborted() {
        // A per-side cap below the image's declared size must yield a
        // recoverable Err, never an allocation-abort — the mechanism that stops
        // a crafted image declaring 60000×60000 from OOM-killing the process.
        let r = decode_image_with_max_side(&png_bytes(8, 8), 4);
        assert!(
            r.is_err(),
            "decode must fail under the pixel cap, not abort"
        );
    }
}

#[cfg(test)]
mod median_tests {
    #[test]
    fn median_of_empty_is_zero_not_a_panic() {
        // A crafted table can leave a row/column with zero matched cells; the
        // even-count branch would index values[0 - 1] and panic (→ remote crash
        // via docling-serve) without the empty guard.
        assert_eq!(super::tf_match::median_for_test(&mut []), 0.0);
        assert_eq!(super::tf_match::median_for_test(&mut [4.0, 2.0]), 3.0);
        assert_eq!(super::tf_match::median_for_test(&mut [5.0, 1.0, 3.0]), 3.0);
    }
}

#[cfg(test)]
mod send_check {
    /// The Node bindings (`docling-node`) run a shared [`super::Pipeline`] on
    /// libuv worker threads (`Arc<Mutex<Pipeline>>`), which is only sound while
    /// `Pipeline: Send` holds — this fails to compile if a non-`Send` field
    /// (e.g. an `Rc` or a raw pdfium handle) ever lands in the pipeline.
    fn assert_send<T: Send>() {}

    #[test]
    fn pipeline_is_send() {
        assert_send::<super::Pipeline>();
    }
}
