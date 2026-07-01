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
pub mod textparse;
pub mod timing;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::{Arc, Mutex};

use fleischwolf_core::{DoclingDocument, Node};

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

/// One page's assembled output: typed nodes plus the page's hyperlinks, kept
/// separate so pages processed out of order can be stitched back in page order.
type PageOut = (Vec<Node>, Vec<(String, String)>);

/// A self-contained set of the per-page models (layout, OCR, TableFormer). Each
/// parallel page-worker owns its own `Worker` so inference runs concurrently
/// without sharing an ONNX session (`ort`'s `Session::run` is `&mut self`).
struct Worker {
    layout: layout::LayoutModel,
    ocr: Option<ocr::OcrModel>,
    /// TableFormer structure model; `None` when its ONNX graphs aren't present
    /// (the assembler then falls back to geometric table reconstruction).
    tables: Option<tableformer::TableFormer>,
}

impl Worker {
    fn load(intra: usize) -> Result<Self, PdfError> {
        Ok(Self {
            layout: layout::LayoutModel::load_with(intra).map_err(PdfError::Layout)?,
            ocr: None,
            tables: tableformer::TableFormer::load_with(intra),
        })
    }

    /// Run layout (+ OCR for cell-less pages) + TableFormer and assemble page `n`
    /// into its nodes and links. Pure given the page (mutates only the worker's
    /// lazily-loaded OCR model), so it is safe to run concurrently across pages.
    fn process(&mut self, n: usize, page: &mut PdfPage) -> Result<PageOut, PdfError> {
        let regions = timing::timed("layout.predict", || {
            self.layout.predict(&page.image, page.width, page.height)
        })
        .map_err(|e| PdfError::Layout(format!("page {}: {e}", n + 1)))?;
        // Resolve overlapping detections once, before OCR.
        let mut regions = assemble::resolve(regions);
        // Emit text the detector missed as orphan text regions (docling parity).
        assemble::add_orphan_regions(&mut regions, &page.cells);
        // Drop phantom empty low-confidence picture boxes (docling parity).
        assemble::drop_false_pictures(&mut regions, &page.cells, page.width, page.height);
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
            timing::timed("tableformer", || {
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
            });
        }
        Ok(timing::timed("assemble_page", || {
            assemble::assemble_page(page, regions, &table_rows)
        }))
    }
}

/// Per-worker ONNX intra-op threads. The layout model is memory-bandwidth bound,
/// so on a typical machine two threads per worker (sharing one in-cache copy of
/// the weights) extracts more throughput than one fat model or many single-thread
/// workers. `FLEISCHWOLF_PDF_INTRA` overrides for per-machine tuning.
fn pdf_intra() -> usize {
    if let Some(n) = std::env::var("FLEISCHWOLF_PDF_INTRA")
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

/// How many page-workers to spin up for a multi-page PDF. `FLEISCHWOLF_PDF_WORKERS`
/// overrides; otherwise size the pool so `workers × intra ≈ cores`, capped at 4 so
/// a worst-case pool holds a bounded amount of model memory (~0.4 GB per worker)
/// and does not oversaturate the memory bus with model-weight traffic.
fn pdf_worker_count() -> usize {
    if let Some(n) = std::env::var("FLEISCHWOLF_PDF_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
    {
        return n;
    }
    (intra_threads() / pdf_intra()).clamp(1, 4)
}

/// Minimum page count before a PDF is worth the parallel worker pool. Below this,
/// the serial primary (running its model on every core) is faster than fanning out
/// — the helper pool's one-time model-load cost only pays off once enough pages
/// share it. `FLEISCHWOLF_PDF_PARALLEL_MIN` overrides.
fn pdf_parallel_min() -> usize {
    std::env::var("FLEISCHWOLF_PDF_PARALLEL_MIN")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(6)
}

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
    /// Desired pool size for multi-page documents.
    target_workers: usize,
    /// Page count at/above which the parallel pool is worth its load cost.
    parallel_min: usize,
}

impl Pipeline {
    /// Construct the pipeline. Models load lazily on first use (full-intra primary
    /// for serial inputs, the helper pool for multi-page PDFs), so nothing is
    /// loaded that a given document doesn't need.
    pub fn new() -> Result<Self, PdfError> {
        Ok(Self {
            primary: None,
            pool: Vec::new(),
            target_workers: pdf_worker_count(),
            parallel_min: pdf_parallel_min(),
        })
    }

    /// The full-intra serial worker, loaded on first use.
    fn primary(&mut self) -> Result<&mut Worker, PdfError> {
        if self.primary.is_none() {
            self.primary = Some(Worker::load(intra_threads())?);
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
        let doc = if self.target_workers >= 2 && pages >= self.parallel_min {
            self.convert_parallel(bytes, password, name)?
        } else {
            self.convert_serial(bytes, password, name)?
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
    ) -> Result<DoclingDocument, PdfError> {
        let mut doc = DoclingDocument::new(name);
        let worker = self.primary()?;
        pdfium_backend::for_each_page(bytes, password, |n, _total, mut page| {
            let (nodes, links) = worker.process(n, &mut page)?;
            doc.nodes.extend(nodes);
            doc.links.extend(links);
            Ok::<(), PdfError>(())
        })?;
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
    ) -> Result<DoclingDocument, PdfError> {
        self.ensure_pool()?;
        let n_workers = self.pool.len();
        let (work_tx, work_rx) = sync_channel::<(usize, PdfPage)>(n_workers * 2);
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
                    // Hold the receiver lock only for the recv; release before the
                    // (long) per-page work so other workers can pull concurrently.
                    let item = work_rx.lock().unwrap().recv();
                    let Ok((idx, mut page)) = item else { break };
                    match worker.process(idx, &mut page) {
                        Ok(out) => results.lock().unwrap().push((idx, out)),
                        Err(e) => {
                            let mut slot = first_err.lock().unwrap();
                            if slot.is_none() {
                                *slot = Some(e);
                            }
                        }
                    }
                });
            }
            // Render on this thread and feed the workers; backpressure blocks here
            // when the channel is full. Dropping `work_tx` afterwards signals the
            // workers (recv → Err) to finish.
            let render = pdfium_backend::for_each_page(bytes, password, |i, _total, page| {
                work_tx
                    .send((i, page))
                    .map_err(|_| PdfError::Pdfium("page-worker channel closed".into()))
            });
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
        let r = if self.target_workers >= 2 && pages >= self.parallel_min {
            self.convert_streaming_parallel(bytes, password, emit)
        } else {
            self.convert_streaming_serial(bytes, password, emit)
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
        mut emit: F,
    ) -> Result<(), PdfError>
    where
        F: FnMut(Vec<Node>, Vec<(String, String)>) -> Result<(), PdfError>,
    {
        let mut asm = assemble::StreamAssembler::new();
        let worker = self.primary()?;
        pdfium_backend::for_each_page(bytes, password, |n, _total, mut page| {
            let (nodes, links) = worker.process(n, &mut page)?;
            emit(asm.push(nodes), links)
        })?;
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
        mut emit: F,
    ) -> Result<(), PdfError>
    where
        F: FnMut(Vec<Node>, Vec<(String, String)>) -> Result<(), PdfError>,
    {
        self.ensure_pool()?;
        let n_workers = self.pool.len();
        let (work_tx, work_rx) = sync_channel::<(usize, PdfPage)>(n_workers * 2);
        let work_rx: Arc<Mutex<Receiver<(usize, PdfPage)>>> = Arc::new(Mutex::new(work_rx));
        // Workers and the renderer report here; the calling thread drains it in
        // page order. Bounded so workers block (bounding resident bitmaps) when the
        // consumer falls behind.
        let (res_tx, res_rx) = sync_channel::<Result<(usize, PageOut), PdfError>>(n_workers * 2);

        let mut workers = std::mem::take(&mut self.pool);
        let mut asm = assemble::StreamAssembler::new();
        let mut first_err: Option<PdfError> = None;

        std::thread::scope(|s| {
            // Workers: pull a page, process it, report (index-tagged) result.
            for worker in workers.iter_mut() {
                let work_rx = Arc::clone(&work_rx);
                let res_tx = res_tx.clone();
                s.spawn(move || loop {
                    let item = work_rx.lock().unwrap().recv();
                    let Ok((idx, mut page)) = item else { break };
                    let out = worker.process(idx, &mut page).map(|o| (idx, o));
                    if res_tx.send(out).is_err() {
                        break; // consumer gone
                    }
                });
            }
            // Renderer: feed pages to the pool on its own thread (pdfium stays on a
            // single thread); report a render error through the same channel.
            {
                let res_tx = res_tx.clone();
                s.spawn(move || {
                    let render =
                        pdfium_backend::for_each_page(bytes, password, |i, _total, page| {
                            work_tx
                                .send((i, page))
                                .map_err(|_| PdfError::Pdfium("page-worker channel closed".into()))
                        });
                    drop(work_tx); // signal workers to finish
                    if let Err(e) = render {
                        let _ = res_tx.send(Err(e));
                    }
                });
            }
            // Drop our own sender so the channel closes once the threads finish.
            drop(res_tx);

            // Collector (this thread): reorder into document order and emit.
            let mut buffer: BTreeMap<usize, PageOut> = BTreeMap::new();
            let mut next = 0usize;
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
        let loaded: Vec<Result<Worker, PdfError>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..need)
                .map(|_| s.spawn(move || Worker::load(intra)))
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
